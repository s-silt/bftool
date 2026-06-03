//! 备份视图:archive::plan(只读预览)→「正式备份」后台跑 archive::run_plan(GuiReporter/进度/取消)。
//! **绝不在 UI 线程跑 run_plan**(Spec D §3);危险组合沿用 core `verify_disabled` fail-closed(Spec D §5)。

use std::sync::{mpsc, Arc};

use eframe::egui;

use bftool_core::engine::archive::{self, Options, PlanAction};
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::reporter::{GuiReporter, ProgressState};
use crate::task::BackgroundTask;
use crate::views::util;

/// 备份页高级设置的持久状态(跨帧)。默认 = 最安全(全 SHA256 + 压缩包测试)。
#[derive(Debug, Clone, Default)]
pub struct ArchiveUiState {
    /// 本次最多处理几个项目(空 = 不限)。文本框,解析失败按不限。
    pub limit_text: String,
    /// 关闭压缩包内部结构测试。
    pub no_test_archives: bool,
    /// 危险:跳过 SHA256 内容校验。
    pub unsafe_no_hash: bool,
    /// --unsafe-no-hash 的二次确认(对应 CLI 的 --i-understand-this-can-miss-bitrot)。
    pub confirm_unsafe: bool,
}

impl ArchiveUiState {
    fn to_options(&self) -> Result<Options, String> {
        let limit = parse_limit(&self.limit_text)?;
        Ok(Options {
            dry_run: false,
            no_hash: self.unsafe_no_hash,
            limit,
            drive_letter_override: None,
            no_test_archives: self.no_test_archives,
        })
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    ui.heading("备份");
    ui.add_space(4.0);

    let busy = app.is_busy();

    // ── 高级设置(默认折叠;改了设置后需重新「演练」生成计划)──
    ui.add_enabled_ui(!busy, |ui| {
        ui.collapsing("高级设置", |ui| {
            ui.horizontal(|ui| {
                ui.label("本次上限(空=不限)：");
                ui.text_edit_singleline(&mut app.archive_ui.limit_text);
            });
            ui.checkbox(
                &mut app.archive_ui.no_test_archives,
                "关闭压缩包内部测试(SHA256 仍在)",
            );
            ui.checkbox(
                &mut app.archive_ui.unsafe_no_hash,
                "⚠ 跳过 SHA256 内容校验(危险:挡不住比特腐烂)",
            );
            if app.archive_ui.unsafe_no_hash {
                ui.checkbox(
                    &mut app.archive_ui.confirm_unsafe,
                    "我明白这会漏掉静默损坏,仅用于可再生素材",
                );
            } else {
                // 取消「跳过 SHA256」时复位二次确认,使每次重新启用危险开关都必须重新确认 ——
                // 否则陈旧的 confirm_unsafe=true 会静默满足守卫、绕过确认摩擦。与 init.rs 复位
                // confirm_force 的做法一致。(review-r3 round3)
                app.archive_ui.confirm_unsafe = false;
            }
        });
    });

    ui.add_space(4.0);

    // ── 按钮行:演练/刷新计划 · 正式备份 · 取消 ──
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!busy, egui::Button::new("🔍 演练 / 刷新计划"))
            .clicked()
        {
            refresh_plan(app);
        }

        let can_run = !busy && app.archive_plan.is_some() && plan_matches_current(app);
        if ui
            .add_enabled(can_run, egui::Button::new("▶ 正式备份"))
            .clicked()
        {
            start_archive(app);
        }

        if app.plan_task.is_some() {
            // 计划在后台算(只读、不可中途取消)。
            ui.spinner();
            ui.label("正在生成计划…");
        } else if app.task.is_some() {
            let requested = app
                .task
                .as_ref()
                .map(|t| t.cancel_requested())
                .unwrap_or(false);
            if ui
                .add_enabled(!requested, egui::Button::new("✕ 取消"))
                .clicked()
            {
                if let Some(t) = &app.task {
                    t.request_cancel();
                }
            }
            if requested {
                ui.label("取消中…(当前项目完成后停止)");
            }
        }
    });

    // 危险组合提示(与 core verify_disabled 同口径;真正拦截在 start_archive / plan)。
    if archive::verify_disabled(
        app.archive_ui.unsafe_no_hash,
        app.cfg.test_archives,
        app.archive_ui.no_test_archives,
    ) {
        ui.colored_label(
            util::level_color(LogLevel::Error),
            "⚠ 当前组合 = 跳过 SHA256 且关闭压缩包测试 → 等价无校验,工具会拒绝执行。",
        );
    }

    ui.separator();

    // ── 进度条 ──
    util::progress_bar(&app.progress, ui);

    // ── 计划预览表 ──
    if let Some(plan) = &app.archive_plan {
        ui.label(format!(
            "计划(盘 {}）：共 {} 项",
            plan.drive.id,
            plan.items.len()
        ));
        egui::ScrollArea::vertical()
            .max_height(200.0)
            .id_salt("plan")
            .show(ui, |ui| {
                egui::Grid::new("plan_grid")
                    .num_columns(2)
                    .striped(true)
                    .show(ui, |ui| {
                        for it in &plan.items {
                            ui.monospace(&it.name);
                            let (color, text) = action_text(&it.action);
                            ui.colored_label(color, text);
                            ui.end_row();
                        }
                    });
            });
    } else if !busy {
        ui.weak("点「演练 / 刷新计划」生成本轮计划预览。");
    }
    if app.archive_plan.is_some() && !plan_matches_current(app) {
        ui.colored_label(
            util::level_color(LogLevel::Warn),
            "配置或高级设置已变化，请重新演练后再正式备份。",
        );
    }

    // ── 摘要 ──
    if let Some(s) = &app.last_summary {
        ui.separator();
        ui.strong(format!("上次结果：{}", s));
    }

    // ── 日志面板 ──
    ui.separator();
    ui.label("日志：");
    util::log_panel(&app.logs, ui);
}

/// 后台跑 archive::plan(只读)生成预览——`plan` 会遍历待备份所有项目(folder_stats),
/// 大目录时耗时,故放后台线程,不冻 UI。plan 消息经 GuiReporter 进日志;结果经 plan_task 回传。
fn refresh_plan(app: &mut App) {
    // R-06:refresh_plan 与 start_archive 共用 app.rx,无编译期"互斥"保证。
    // 运行期保证:两个入口按钮都 add_enabled(!busy)(busy = task 或 plan_task 在跑),
    // 任一任务进行中时按钮禁用 → 不会重入 → 不会 clobber 正在用的 rx。
    // debug_assert 在 debug 构建里把这个隐式契约显式化,违反即 panic 早暴露。
    debug_assert!(
        app.plan_task.is_none(),
        "rx clobber: plan_task still running"
    );
    app.logs.clear();
    app.last_summary = None;
    app.archive_plan = None;
    app.archive_plan_inputs = None;
    if app.archive_ui.unsafe_no_hash && !app.archive_ui.confirm_unsafe {
        app.logs.push((
            LogLevel::Error,
            "已勾选「跳过 SHA256」但未确认 —— 请先勾选确认,或取消该危险选项。".to_string(),
        ));
        return;
    }
    let opts = match app.archive_ui.to_options() {
        Ok(o) => o,
        Err(e) => {
            app.logs.push((LogLevel::Error, e));
            return;
        }
    };
    let (tx, rx) = mpsc::channel();
    let reporter = GuiReporter::new(tx, Arc::clone(&app.progress));
    app.rx = Some(rx);
    let cfg = app.cfg.clone();
    app.plan_task = Some(
        BackgroundTask::spawn(move |_cancel| {
            // plan 只读、不可中途取消(folder_stats 无 cancel 钩子);返回结构化计划。
            archive::plan(&cfg, &opts, &reporter)
        })
        // 只读不写盘:关窗时 detach 而非 join,避免卡在慢速/掉线网络盘的 folder_stats 上挂死 UI。(review-r3 round3)
        .detachable(),
    );
}

/// 把当前 plan 交给后台线程跑 run_plan(GuiReporter 推日志/进度,cancel 项目边界)。
fn start_archive(app: &mut App) {
    // R-06:同 refresh_plan——「正式备份」按钮 add_enabled(can_run = !busy && plan.is_some()),
    // busy 时禁用,运行期不会重入 clobber app.rx;debug_assert 把该契约显式化。
    debug_assert!(app.task.is_none(), "rx clobber: task still running");
    if !plan_matches_current(app) {
        app.logs.push((
            LogLevel::Error,
            "配置或高级设置已变化，请先重新演练，再正式备份。".to_string(),
        ));
        return;
    }
    let Some(plan) = app.archive_plan.take() else {
        return;
    };
    // 危险组合二次确认(plan/run_plan 内还有 core fail-closed 兜底)。
    if app.archive_ui.unsafe_no_hash && !app.archive_ui.confirm_unsafe {
        app.logs.push((
            LogLevel::Error,
            "跳过 SHA256 需先勾选确认 —— 已取消本次备份。".to_string(),
        ));
        app.archive_plan = Some(plan);
        return;
    }
    app.archive_plan_inputs = None;
    app.logs.clear();
    app.last_summary = None;
    if let Ok(mut p) = app.progress.lock() {
        *p = ProgressState::default();
    }
    let (tx, rx) = mpsc::channel();
    let reporter = GuiReporter::new(tx, Arc::clone(&app.progress));
    app.rx = Some(rx);
    app.task_started = Some(std::time::Instant::now());
    let cfg = app.cfg.clone();
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        let s = archive::run_plan(&cfg, &plan, cancel, &reporter)?;
        let mut parts = vec![format!("完成 {} 项", s.handled)];
        if s.failed > 0 {
            parts.push(format!("失败 {}", s.failed));
        }
        if s.cancelled {
            parts.push("已取消".to_string());
        }
        if s.sealed_stopped {
            parts.push("已封盘停本轮".to_string());
        }
        Ok(parts.join("，"))
    }));
}

fn parse_limit(text: &str) -> Result<usize, String> {
    let s = text.trim();
    if s.is_empty() {
        return Ok(0);
    }
    s.parse::<usize>()
        .map_err(|_| format!("「本次上限」必须是非负整数或留空(当前:「{}」)。", s))
}

fn plan_matches_current(app: &App) -> bool {
    let Some(inputs) = &app.archive_plan_inputs else {
        return false;
    };
    let Ok(opts) = app.archive_ui.to_options() else {
        return false;
    };
    inputs.cfg == app.cfg && inputs.opts == opts
}

/// 计划动作 → (颜色, 文案)。纯函数,可测。颜色复用 util::level_color 避免重复硬编码。
fn action_text(a: &PlanAction) -> (egui::Color32, String) {
    match a {
        PlanAction::Archive { dest_name } => (
            util::level_color(LogLevel::Ok),
            format!("归档 → {}", dest_name),
        ),
        PlanAction::RenameAndArchive { dest_name } => (
            util::level_color(LogLevel::Ok),
            format!("改名归档 → {}", dest_name),
        ),
        PlanAction::Skip(r) => (util::level_color(LogLevel::Info), format!("跳过：{}", r)),
        PlanAction::SealAndStop(r) => (
            util::level_color(LogLevel::Warn),
            format!("封盘停本轮：{}", r),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_text_carries_keyword() {
        let (_, a) = action_text(&PlanAction::Archive {
            dest_name: "001x".into(),
        });
        assert!(a.contains("归档") && a.contains("001x"));
        let (_, r) = action_text(&PlanAction::RenameAndArchive {
            dest_name: "001x_20260529".into(),
        });
        assert!(r.contains("改名归档"));
        let (_, s) = action_text(&PlanAction::Skip("未稳定".into()));
        assert!(s.contains("跳过") && s.contains("未稳定"));
        let (_, seal) = action_text(&PlanAction::SealAndStop("余量不足".into()));
        assert!(seal.contains("封盘"));
    }

    #[test]
    fn limit_text_parses_to_options() {
        let mut st = ArchiveUiState::default();
        assert_eq!(st.to_options().unwrap().limit, 0, "空 = 不限");
        st.limit_text = "5".into();
        assert_eq!(st.to_options().unwrap().limit, 5);
        st.limit_text = "  abc ".into();
        assert!(st.to_options().is_err(), "解析失败应阻止执行,不能变成不限");
        st.limit_text.clear();
        st.unsafe_no_hash = true;
        st.no_test_archives = true;
        let o = st.to_options().unwrap();
        assert!(o.no_hash && o.no_test_archives && !o.dry_run);
    }

    #[test]
    fn parse_limit_rejects_invalid_number() {
        assert_eq!(parse_limit("").unwrap(), 0);
        assert_eq!(parse_limit(" 2 ").unwrap(), 2);
        assert!(parse_limit("1O").is_err());
    }
}

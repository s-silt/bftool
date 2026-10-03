//! 备份视图: 清楚分开“演练预览”和“正式备份”；计划列出实际源、目的位置、大小、动作和原因。
//! 执行期间保持进度与日志可见，明确取消在项目边界生效。

use std::sync::{mpsc, Arc};

use eframe::egui;

use bftool_core::engine::archive::{Options, PlanAction};
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::reporter::{GuiReporter, ProgressState};
use crate::task::BackgroundTask;
use crate::views::{theme, util};

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
    pub fn to_options(&self) -> Result<Options, String> {
        let limit = parse_limit(&self.limit_text)?;
        Ok(Options {
            dry_run: false,
            no_hash: self.unsafe_no_hash,
            limit,
            drive_letter_override: None,
            no_test_archives: self.no_test_archives,
            source_override: None,
            incremental: false,
            retain_source: false,
            include_ext: None,
            file_globs: Vec::new(),
            ext_recursive: false,
            include_subfolder_projects: true,
            seed_from_global_catalog: false,
            incremental_verify: Default::default(),
        })
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    theme::page_header(
        ui,
        "安全备份归档",
        "遵循 SSD → 机械盘的三重 SHA256 完整性校验流程；两步操作：先演练预览，确认无误后再正式写入。",
    );

    let busy = app.is_busy();

    // ── 操作卡片：清晰分开两步 ──
    theme::card(ui, |ui| {
        theme::responsive_row(ui, 600.0, |ui| {
            // 步骤一：演练预览
            ui.vertical(|ui| {
                ui.set_max_width(280.0_f32.min(ui.available_width()));
                ui.label(
                    egui::RichText::new("第一步：演练预览")
                        .font(theme::subtitle_font())
                        .color(theme::TEXT_TITLE),
                );
                ui.label(
                    egui::RichText::new(
                        "只读遍历源目录与目标盘，生成拟归档清单，不写盘也不移动源文件。",
                    )
                    .size(11.5)
                    .color(theme::TEXT_MUTED),
                );
                ui.add_space(4.0);
                if ui
                    .add_enabled(!busy, theme::btn_secondary("🔍 演练 / 刷新计划"))
                    .clicked()
                {
                    refresh_plan(app);
                }
            });

            ui.add_space(16.0);
            ui.separator();
            ui.add_space(16.0);

            // 步骤二：正式备份
            ui.vertical(|ui| {
                ui.set_max_width(220.0_f32.min(ui.available_width()));
                ui.label(
                    egui::RichText::new("第二步：正式归档")
                        .font(theme::subtitle_font())
                        .color(theme::TEXT_TITLE),
                );
                let can_run = !busy && app.archive_plan.is_some() && plan_matches_current(app);
                let hint = if app.archive_plan.is_none() {
                    "请先点击左侧「演练 / 刷新计划」"
                } else if !plan_matches_current(app) {
                    "配置已变更，请重新演练计划"
                } else {
                    "计划已就绪，点击开始写入机械盘"
                };
                ui.label(egui::RichText::new(hint).size(11.5).color(if can_run {
                    theme::PRIMARY
                } else {
                    theme::TEXT_MUTED
                }));
                ui.add_space(4.0);
                if ui
                    .add_enabled(
                        can_run,
                        theme::btn_primary(&app.operation_label("正式备份")),
                    )
                    .clicked()
                {
                    start_archive(app);
                }
            });

            // 任务运行中控制区
            if busy {
                ui.add_space(16.0);
                ui.separator();
                ui.add_space(16.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("当前执行状态")
                            .font(theme::subtitle_font())
                            .color(theme::TEXT_TITLE),
                    );
                    if app.plan_task.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("正在生成计划…");
                        });
                    } else if app.task_running(crate::app::TaskKind::Archive) {
                        let requested = app
                            .task
                            .as_ref()
                            .map(|t| t.cancel_requested())
                            .unwrap_or(false);
                        if requested {
                            theme::badge(
                                ui,
                                "取消中(当前项目完成后停止)",
                                theme::WARN,
                                egui::Color32::WHITE,
                            );
                        } else {
                            let cancel_btn = ui.add(theme::btn_danger("停止备份"));
                            if cancel_btn.clicked() {
                                if let Some(t) = &app.task {
                                    t.request_cancel();
                                }
                            }
                            cancel_btn.on_hover_text(
                                "在当前项目/文件完成后安全停止，已写入的项目完整保留",
                            );
                        }
                    } else if app.is_busy() {
                        ui.label(app.busy_label());
                    }
                });
            }
        });

        // 危险组合提示
        if bftool_core::engine::archive::verify_disabled(
            app.archive_ui.unsafe_no_hash,
            app.cfg.test_archives,
            app.archive_ui.no_test_archives,
        ) {
            ui.add_space(8.0);
            theme::callout_with_tag(
                ui,
                theme::DANGER,
                theme::DANGER_SOFT,
                "危险组合",
                "当前设置同时跳过了 SHA256 且关闭了压缩包测试，等价于无完整性校验，工具将拒绝执行。",
            );
        }

        // 高级设置折叠区
        ui.add_space(8.0);
        ui.collapsing("高级归档选项", |ui| {
            ui.horizontal(|ui| {
                ui.label("本次上限项目数(留空表示不限)：");
                ui.text_edit_singleline(&mut app.archive_ui.limit_text);
            });
            ui.checkbox(
                &mut app.archive_ui.no_test_archives,
                "关闭压缩包内部测试(仅跳过解压自检，三遍 SHA256 校验仍生效)",
            );
            ui.checkbox(
                &mut app.archive_ui.unsafe_no_hash,
                "跳过 SHA256 内容校验(危险: 无法发现静默比特腐烂)",
            );
            if app.archive_ui.unsafe_no_hash {
                ui.checkbox(
                    &mut app.archive_ui.confirm_unsafe,
                    "我确认已知晓数据完整性风险，此选项仅用于可再生素材",
                );
            } else {
                app.archive_ui.confirm_unsafe = false;
            }
        });
    });

    ui.add_space(theme::GAP);

    // ── 计划预览表（展开实际源路径、目的位置、大小、动作和原因） ──
    theme::card(ui, |ui| {
        if let Some(plan) = &app.archive_plan {
            ui.horizontal(|ui| {
                theme::section_title(
                    ui,
                    &format!("拟归档计划清单（写入目标盘: {}）", plan.drive.id),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let total_bytes: u64 = plan.items.iter().map(|it| it.est_bytes).sum();
                    ui.label(
                        egui::RichText::new(format!(
                            "共计 {} 项 · 预估数据量约 {}",
                            plan.items.len(),
                            util::fmt_gb(total_bytes)
                        ))
                        .color(theme::TEXT_MUTED)
                        .size(12.0),
                    );
                });
            });
            ui.add_space(4.0);

            if plan.items.is_empty() {
                util::empty_state(
                    ui,
                    "待备份目录为空",
                    "当前待备份目录中没有可处理的文件或文件夹。",
                );
            } else {
                egui::ScrollArea::both()
                    .auto_shrink([false, true])
                    .max_height(240.0)
                    .id_salt("archive_plan_table")
                    .show(ui, |ui| {
                        egui::Grid::new("plan_detail_grid")
                            .num_columns(5)
                            .striped(true)
                            .spacing([12.0, 6.0])
                            .show(ui, |ui| {
                                ui.strong("源项目 / 实际路径");
                                ui.strong("目标备份位置");
                                ui.strong("预估大小");
                                ui.strong("拟执行动作");
                                ui.strong("动作说明 / 原因");
                                ui.end_row();

                                for it in &plan.items {
                                    // 实际源路径
                                    let full_src = it.source_path.display().to_string();
                                    util::copyable_path(ui, &full_src, 30);

                                    // 目的位置
                                    let dest_str = match &it.action {
                                        PlanAction::Archive { dest_name }
                                        | PlanAction::RenameAndArchive { dest_name } => {
                                            format!("{}:\\项目\\{}", plan.drive.letter, dest_name)
                                        }
                                        PlanAction::Skip(_) => "跳过 (无目标)".to_string(),
                                        PlanAction::SealAndStop(_) => {
                                            "封盘中止 (无目标)".to_string()
                                        }
                                    };
                                    util::copyable_path(ui, &dest_str, 26);

                                    // 大小
                                    ui.label(util::fmt_gb(it.est_bytes));

                                    // 动作
                                    let (badge_bg, badge_text, reason) = action_details(&it.action);
                                    theme::badge(ui, badge_text, badge_bg, egui::Color32::WHITE);

                                    // 原因
                                    ui.label(
                                        egui::RichText::new(reason)
                                            .size(12.0)
                                            .color(theme::TEXT_BODY),
                                    );
                                    ui.end_row();
                                }
                            });
                    });
            }
        } else if !busy {
            util::empty_state(
                ui,
                "尚未生成计划预览",
                "点击上方「第一步：演练预览」扫描待归档项目，预览清单无误后即可正式归档备份。",
            );
        } else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在加载计划预览…");
            });
        }
    });

    if app.archive_plan.is_some() && !plan_matches_current(app) {
        ui.add_space(4.0);
        theme::callout_with_tag(
            ui,
            theme::WARN,
            theme::WARN_SOFT,
            "需重新演练",
            "检测到归档配置或高级选项已更改，请重新点击「演练 / 刷新计划」后再执行正式备份。",
        );
    }

    ui.add_space(theme::GAP);

    // ── 执行进度与实时日志（始终保持可见） ──
    theme::card(ui, |ui| {
        theme::section_title(ui, "执行进度与运行日志");
        if let Some(s) = &app.last_summary {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("上次完成结果：{s}"))
                        .size(12.0)
                        .color(theme::TEXT_BODY),
                )
                .wrap(),
            );
        }
        ui.add_space(4.0);

        util::progress_bar(&app.progress, ui);
        util::log_panel(&app.logs, ui);
    });
}

fn refresh_plan(app: &mut App) {
    if !app.ensure_idle() {
        return;
    }
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
            "已勾选「跳过 SHA256」但未确认 —— 请先勾选确认，或取消该危险选项。".to_string(),
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
    let inputs = crate::app::ArchivePlanInputs {
        cfg: cfg.clone(),
        opts: opts.clone(),
    };
    let backend = app.backend;
    app.plan_task = Some(
        BackgroundTask::spawn(move |_cancel| {
            Ok(crate::app::PlannedArchive {
                plan: backend.plan(&cfg, &opts, &reporter)?,
                inputs,
            })
        })
        .detachable(),
    );
}

fn start_archive(app: &mut App) {
    if !app.ensure_idle() {
        return;
    }
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
    let backend = app.backend;
    app.task_kind = Some(crate::app::TaskKind::Archive);
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        let planned = plan.items.len();
        let s = backend.archive(&cfg, &plan, cancel, &reporter)?;
        Ok(crate::app::ExecutionResult::Summary(summarize_archive(
            planned, &s,
        )))
    }));
}

fn summarize_archive(planned: usize, s: &bftool_core::engine::archive::ArchiveSummary) -> String {
    let quiet = s.handled == 0 && s.failed == 0 && !s.cancelled && !s.sealed_stopped;
    if quiet {
        return if planned > 0 {
            "本轮未归档任何项目(执行前被中止,请看日志原因:盘掉线/换盘/检测到多块盘/中途封盘等)"
                .to_string()
        } else {
            "本轮无可处理项目".to_string()
        };
    }
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
    parts.join("，")
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

/// 计划动作 → (徽章背景色, 徽章文字, 动作原因/说明)
fn action_details(a: &PlanAction) -> (egui::Color32, &'static str, String) {
    match a {
        PlanAction::Archive { dest_name } => {
            (theme::OK, "归档", format!("标准安全归档 → {dest_name}"))
        }
        PlanAction::RenameAndArchive { dest_name } => (
            theme::PRIMARY,
            "改名归档",
            format!("避免重名冲突，重命名后写入 → {dest_name}"),
        ),
        PlanAction::Skip(reason) => (theme::WARN, "跳过", reason.clone()),
        PlanAction::SealAndStop(reason) => (theme::DANGER, "封盘中止", reason.clone()),
    }
}

/// 兼容测试函数
#[cfg(test)]
pub fn action_text(a: &PlanAction) -> (egui::Color32, String) {
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
        assert!(a.contains("001x"));
    }

    #[test]
    fn limit_text_parses_to_options() {
        let mut s = ArchiveUiState {
            limit_text: "5".into(),
            ..Default::default()
        };
        assert_eq!(s.to_options().unwrap().limit, 5);
        s.limit_text = "  ".into();
        assert_eq!(s.to_options().unwrap().limit, 0);
    }

    #[test]
    fn parse_limit_rejects_invalid_number() {
        assert!(parse_limit("-1").is_err());
        assert!(parse_limit("abc").is_err());
        assert_eq!(parse_limit("0").unwrap(), 0);
        assert_eq!(parse_limit("42").unwrap(), 42);
    }

    #[test]
    fn summarize_archive_rejected_round_is_not_success() {
        let quiet_with_planned =
            summarize_archive(3, &bftool_core::engine::archive::ArchiveSummary::default());
        assert!(
            quiet_with_planned.contains("被中止") && !quiet_with_planned.contains("完成 0 项"),
            "有计划项却被安全前置拦截时,不能报完成0项冒充成功: {}",
            quiet_with_planned
        );

        let quiet_zero_planned =
            summarize_archive(0, &bftool_core::engine::archive::ArchiveSummary::default());
        assert_eq!(quiet_zero_planned, "本轮无可处理项目");

        let normal = summarize_archive(
            2,
            &bftool_core::engine::archive::ArchiveSummary {
                handled: 2,
                failed: 0,
                cancelled: false,
                sealed_stopped: false,
            },
        );
        assert_eq!(normal, "完成 2 项");
    }
}

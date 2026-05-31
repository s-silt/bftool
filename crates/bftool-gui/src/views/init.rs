//! 初始化视图:init_candidates 列出挂载盘 + 防呆判定(系统盘/资料库/非空盘),
//! 选可初始化的 → drive::init。--force 藏在折叠"高级"内 + 二次确认(危险:绕过防呆)。(Spec D §5)

use std::sync::{mpsc, Arc};

use eframe::egui;

use bftool_core::engine::drive::{self, InitCandidate};
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::reporter::{GuiReporter, UiEvent};
use crate::views::util;

/// 初始化页跨帧状态。
#[derive(Debug, Default)]
pub struct InitUiState {
    pub cache: Option<Vec<InitCandidate>>,
    pub selected: Option<String>, // 盘符
    pub custom_id: String,
    pub force: bool,
    pub confirm_force: bool,
    pub result: Option<(bool, String)>,
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    ui.heading("初始化新盘");
    ui.add_space(4.0);

    if ui.button("🔄 刷新候选").clicked() {
        rescan(app);
    }
    // R-04:仅当缓存为空(首次进入,或 init 成功后置 None)才自动扫一次。
    // rescan 是阻塞的同步 I/O(枚举挂载盘),绝不能每帧/每次进视图都扫——否则 UI 卡顿。
    // 后续刷新只走上面的「刷新候选」按钮(用户主动触发)。
    if app.init_ui.cache.is_none() {
        rescan(app);
    }

    ui.separator();
    let mut chosen = app.init_ui.selected.clone();
    match &app.init_ui.cache {
        Some(cands) if !cands.is_empty() => {
            for c in cands {
                ui.horizontal(|ui| {
                    let head = format!("{}:  {} GB", c.letter, c.total_gb);
                    if candidate_selectable(c, app.init_ui.force, app.init_ui.confirm_force) {
                        let sel = app.init_ui.selected.as_deref() == Some(c.letter.as_str());
                        if ui.selectable_label(sel, head).clicked() {
                            chosen = Some(c.letter.clone());
                        }
                        if c.can_init {
                            ui.weak("可初始化");
                        } else {
                            ui.colored_label(
                                util::level_color(LogLevel::Warn),
                                format!("强制可选：{}", candidate_block(c)),
                            );
                        }
                    } else {
                        ui.add_enabled(false, egui::Button::new(head));
                        ui.colored_label(util::level_color(LogLevel::Warn), candidate_block(c));
                    }
                });
            }
        }
        Some(_) => {
            ui.label("未发现可挂载的盘候选。");
        }
        None => {
            ui.label("（点「刷新候选」）");
        }
    }
    app.init_ui.selected = chosen;

    ui.separator();
    ui.horizontal(|ui| {
        ui.label("自定义编号(留空=自动「备份N」)：");
        ui.text_edit_singleline(&mut app.init_ui.custom_id);
    });
    ui.collapsing("高级(危险)", |ui| {
        ui.checkbox(
            &mut app.init_ui.force,
            "强制初始化(跳过系统盘/资料库/非空盘防呆)",
        );
        if app.init_ui.force {
            ui.checkbox(
                &mut app.init_ui.confirm_force,
                "我确认知道风险(会绕过所有防呆)",
            );
        }
    });

    let can = app.init_ui.selected.is_some()
        && init_allowed(app.init_ui.force, app.init_ui.confirm_force);
    if ui
        .add_enabled(can, egui::Button::new("▶ 初始化为备份盘"))
        .clicked()
    {
        do_init(app);
    }
    if app.init_ui.force && !app.init_ui.confirm_force {
        ui.colored_label(
            util::level_color(LogLevel::Warn),
            "强制模式需勾选确认才能执行。",
        );
    }
    if let Some((ok, msg)) = &app.init_ui.result {
        let color = if *ok {
            util::level_color(LogLevel::Ok)
        } else {
            util::level_color(LogLevel::Error)
        };
        ui.colored_label(color, msg);
    }
}

fn rescan(app: &mut App) {
    match drive::init_candidates(&app.cfg) {
        Ok(c) => {
            app.init_ui.result = Some((true, format!("已刷新候选盘：{} 个。", c.len())));
            app.init_ui.cache = Some(c);
        }
        Err(e) => {
            app.init_ui.cache = Some(Vec::new());
            app.init_ui.result = Some((false, format!("枚举候选盘失败：{:#}", e)));
            app.logs
                .push((LogLevel::Error, format!("枚举候选盘失败：{:#}", e)));
        }
    }
}

fn do_init(app: &mut App) {
    let Some(letter) = app.init_ui.selected.clone() else {
        return;
    };
    let custom = app.init_ui.custom_id.trim().to_string();
    let id_opt = if custom.is_empty() {
        None
    } else {
        Some(custom.as_str())
    };
    let force = app.init_ui.force;

    // 同步跑 init,用临时 GuiReporter 把消息收进日志(init 是快操作,UI 线程内即可)。
    let (tx, rx) = mpsc::channel();
    let reporter = GuiReporter::new(tx, Arc::clone(&app.progress));
    let result = drive::init(&app.cfg, &reporter, &letter, id_opt, force);
    drop(reporter);
    let drained: Vec<UiEvent> = rx.try_iter().collect();
    for ev in drained {
        match ev {
            UiEvent::Log { level, msg } => app.logs.push((level, msg)),
        }
    }
    match result {
        Ok(()) => {
            app.init_ui.result = Some((true, format!("已初始化 {}: 为备份盘。", letter)));
            app.last_summary = Some(format!("已初始化 {}: 为备份盘。", letter));
            app.logs
                .push((LogLevel::Ok, format!("已初始化 {}: 为备份盘。", letter)));
            // 刷新候选与盘列表缓存
            app.init_ui.cache = None;
            app.init_ui.selected = None;
            app.init_ui.confirm_force = false;
            app.drives_cache = None;
        }
        Err(e) => {
            let msg = format!("初始化失败：{:#}", e);
            app.init_ui.result = Some((false, msg.clone()));
            app.last_summary = Some(msg.clone());
            app.logs.push((LogLevel::Error, msg));
        }
    }
}

/// 不可初始化时的阻断原因文案。纯函数,可测。
fn candidate_block(c: &InitCandidate) -> String {
    c.block_reason
        .clone()
        .unwrap_or_else(|| "不可初始化".to_string())
}

/// 强制模式必须二次确认才放行;非强制始终放行。纯函数,可测。
fn init_allowed(force: bool, confirm: bool) -> bool {
    !force || confirm
}

fn candidate_selectable(c: &InitCandidate, force: bool, confirm: bool) -> bool {
    c.can_init || (force && confirm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(letter: &str, can: bool, reason: Option<&str>) -> InitCandidate {
        InitCandidate {
            letter: letter.into(),
            total_gb: 500,
            is_system: false,
            is_library: false,
            already_backup: false,
            non_empty: false,
            can_init: can,
            block_reason: reason.map(|s| s.into()),
        }
    }

    #[test]
    fn candidate_block_uses_reason() {
        assert_eq!(candidate_block(&cand("X", false, Some("系统盘"))), "系统盘");
        assert_eq!(candidate_block(&cand("Y", false, None)), "不可初始化");
    }

    #[test]
    fn init_allowed_force_needs_confirm() {
        assert!(init_allowed(false, false), "非强制始终放行");
        assert!(init_allowed(false, true));
        assert!(!init_allowed(true, false), "强制未确认 → 不放行");
        assert!(init_allowed(true, true), "强制 + 确认 → 放行");
    }

    #[test]
    fn candidate_selectable_allows_blocked_only_after_force_confirm() {
        let blocked = cand("X", false, Some("非空盘"));
        assert!(!candidate_selectable(&blocked, false, false));
        assert!(!candidate_selectable(&blocked, true, false));
        assert!(candidate_selectable(&blocked, true, true));
        assert!(candidate_selectable(&cand("Y", true, None), false, false));
    }
}

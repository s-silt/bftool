//! 初始化视图: 盘符、编号、容量、已有内容及风险提示清晰呈现；
//! 危险操作与普通操作明确区分，不隐藏原有确认和限制。

use std::sync::{mpsc, Arc};

use eframe::egui;

use bftool_core::engine::drive::InitCandidate;
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::reporter::{GuiReporter, UiEvent};
use crate::views::{theme, util};

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
    theme::page_header(
        ui,
        "初始化新备份盘",
        "将外接机械盘/移动硬盘初始化为 bftool 格式，自动创建「项目」目录并写入盘标识文件；非空盘默认保留现有文件。",
    );

    let busy = app.is_busy();

    // ── 顶部操作与候选盘列表 ──
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            theme::section_title(ui, "1. 检测与选择目标磁盘");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!busy, theme::btn_secondary("🔄 刷新候选盘"))
                    .clicked()
                {
                    rescan(app);
                }
            });
        });
        ui.add_space(4.0);

        if app.init_ui.cache.is_none() {
            rescan(app);
        }

        let mut chosen = app.init_ui.selected.clone();

        match &app.init_ui.cache {
            Some(cands) if !cands.is_empty() => {
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        egui::Grid::new("init_cands_grid")
                            .num_columns(5)
                            .striped(true)
                            .spacing([12.0, 8.0])
                            .show(ui, |ui| {
                                ui.strong("选择");
                                ui.strong("盘符");
                                ui.strong("容量");
                                ui.strong("已有状态与安全检查");
                                ui.strong("内容与风险提示");
                                ui.end_row();

                                for c in cands {
                                    let selectable = candidate_selectable(
                                        c,
                                        app.init_ui.force,
                                        app.init_ui.confirm_force,
                                    );
                                    let is_selected =
                                        app.init_ui.selected.as_deref() == Some(c.letter.as_str());

                                    // 选择框
                                    ui.add_enabled_ui(selectable, |ui| {
                                        let mut checked = is_selected;
                                        if ui.checkbox(&mut checked, "").changed() {
                                            if checked {
                                                chosen = Some(c.letter.clone());
                                            } else if is_selected {
                                                chosen = None;
                                            }
                                        }
                                    });

                                    // 盘符
                                    ui.label(
                                        egui::RichText::new(format!("{}:", c.letter))
                                            .font(theme::subtitle_font())
                                            .color(if selectable {
                                                theme::TEXT_TITLE
                                            } else {
                                                theme::TEXT_DISABLED
                                            }),
                                    );

                                    // 容量
                                    ui.label(format!("{} GB", c.total_gb));

                                    // 安全检查标签
                                    if c.can_init {
                                        theme::badge(
                                            ui,
                                            "允许初始化",
                                            theme::OK,
                                            egui::Color32::WHITE,
                                        );
                                    } else {
                                        let block_msg = candidate_block(c);
                                        theme::badge(
                                            ui,
                                            &format!("阻止: {block_msg}"),
                                            theme::DANGER,
                                            egui::Color32::WHITE,
                                        );
                                    }

                                    // 提示信息
                                    ui.vertical(|ui| {
                                        ui.set_max_width(230.0);
                                        if let Some(hint) = c.soft_hint.as_deref() {
                                            ui.colored_label(theme::WARN, hint);
                                        } else if c.can_init {
                                            ui.colored_label(theme::TEXT_MUTED, "空盘或已就绪");
                                        } else {
                                            ui.colored_label(
                                                theme::DANGER,
                                                "受保护分区，默认禁止写入",
                                            );
                                        }
                                    });

                                    ui.end_row();
                                }
                            });
                    });
            }
            Some(_) => {
                util::empty_state(
                    ui,
                    "未发现可用外接盘",
                    "未检测到可挂载的新磁盘。请插入外部硬盘后点击右上角「刷新候选盘」。",
                );
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("正在扫描系统磁盘…");
                });
            }
        }
        app.init_ui.selected = chosen;
    });

    ui.add_space(theme::GAP);

    // ── 初始化配置与高级确认 ──
    theme::card(ui, |ui| {
        theme::section_title(ui, "2. 备份盘配置与安全授权");
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.label("自定义编号(留空则自动按顺序命名为「备份N」)：");
            ui.text_edit_singleline(&mut app.init_ui.custom_id);
        });

        ui.add_space(6.0);
        ui.collapsing("高级与强制模式 (涉及高风险操作)", |ui| {
            ui.checkbox(
                &mut app.init_ui.force,
                "启用强制初始化 (仅用于特殊情况跳过系统盘/资料库硬闸；非空盘默认无需勾选)",
            );
            if app.init_ui.force {
                ui.add_space(4.0);
                theme::callout_with_tag(
                    ui,
                    theme::DANGER,
                    theme::DANGER_SOFT,
                    "极高风险",
                    "强制模式将允许对系统盘或现有资料库写入备份元数据，操作不当可能导致数据混乱！",
                );
                ui.add_space(4.0);
                ui.checkbox(
                    &mut app.init_ui.confirm_force,
                    "我已充分知晓并确认上述风险，仍要强制初始化选中的磁盘",
                );
            } else {
                app.init_ui.confirm_force = false;
            }
        });

        // 选中盘若带 soft_hint，显示非阻断提示
        if let (Some(sel), Some(cands)) = (&app.init_ui.selected, &app.init_ui.cache) {
            if let Some(c) = cands.iter().find(|c| c.letter == *sel) {
                if let Some(hint) = c.soft_hint.as_deref() {
                    ui.add_space(6.0);
                    theme::callout_with_tag(
                        ui,
                        theme::WARN,
                        theme::WARN_SOFT,
                        "盘内内容提醒",
                        hint,
                    );
                }
            }
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);

        // ── 危险执行按钮 ──
        ui.horizontal(|ui| {
            let can = app.init_ui.selected.is_some()
                && init_allowed(app.init_ui.force, app.init_ui.confirm_force);

            let init_btn = ui.add_enabled(
                can && !busy,
                theme::btn_primary(&app.operation_label("初始化选中磁盘为备份盘")),
            );
            if init_btn.clicked() {
                do_init(app);
            }

            if app.init_ui.selected.is_none() {
                ui.colored_label(theme::TEXT_MUTED, "(请先在上方勾选一个目标磁盘)");
            } else if app.init_ui.force && !app.init_ui.confirm_force {
                ui.colored_label(theme::WARN, "(强制模式须勾选确认框方可放行)");
            }
        });

        // 结果反馈
        if let Some((ok, msg)) = &app.init_ui.result {
            ui.add_space(6.0);
            if *ok {
                theme::callout_with_tag(ui, theme::OK, theme::OK_SOFT, "成功", msg);
            } else {
                util::error_banner(
                    ui,
                    "初始化失败",
                    msg,
                    "请检查目标磁盘写入权限或磁盘空间后重试。",
                );
            }
        }
    });
}

fn rescan(app: &mut App) {
    match app.backend.init_candidates(&app.cfg) {
        Ok(c) => {
            app.init_ui.result =
                Some((true, format!("已扫描并识别到 {} 个候选磁盘分区。", c.len())));
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
    if !app.ensure_idle() {
        return;
    }
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

    let (tx, rx) = mpsc::channel();
    let reporter = GuiReporter::new(tx, Arc::clone(&app.progress));
    let cfg = app.cfg.clone();
    let res = app
        .backend
        .initialize(&cfg, &letter, id_opt, force, &reporter);
    while let Ok(ev) = rx.try_recv() {
        match ev {
            UiEvent::Log { level, msg } => app.logs.push((level, msg)),
        }
    }
    match res {
        Ok(()) => {
            let msg = app.operation_label(&format!("磁盘「{letter}:」初始化完成。"));
            app.init_ui.result = Some((true, msg.clone()));
            app.last_summary = Some(msg);
            app.init_ui.selected = None;
            app.init_ui.cache = None;
            app.drives_cache = None;
            app.status_cache = None;
        }
        Err(e) => {
            let msg = format!("初始化磁盘失败：{:#}", e);
            app.init_ui.result = Some((false, msg.clone()));
            app.logs.push((LogLevel::Error, msg));
        }
    }
}

pub fn candidate_block(c: &InitCandidate) -> String {
    c.block_reason
        .clone()
        .unwrap_or_else(|| "不可初始化".to_string())
}

pub fn init_allowed(force: bool, confirm: bool) -> bool {
    !force || confirm
}

pub fn candidate_selectable(c: &InitCandidate, force: bool, confirm: bool) -> bool {
    c.can_init || (force && confirm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(letter: &str, can: bool, reason: Option<&str>, soft: Option<&str>) -> InitCandidate {
        InitCandidate {
            letter: letter.into(),
            total_gb: 500,
            is_system: false,
            is_library: false,
            already_backup: false,
            non_empty: soft.is_some(),
            can_init: can,
            block_reason: reason.map(|s| s.into()),
            soft_hint: soft.map(|s| s.into()),
        }
    }

    #[test]
    fn candidate_block_uses_reason() {
        assert_eq!(
            candidate_block(&cand("X", false, Some("系统盘"), None)),
            "系统盘"
        );
        assert_eq!(candidate_block(&cand("Y", false, None, None)), "不可初始化");
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
        let blocked = cand("X", false, Some("系统盘"), None);
        assert!(!candidate_selectable(&blocked, false, false));
        assert!(!candidate_selectable(&blocked, true, false));
        assert!(candidate_selectable(&blocked, true, true));
        assert!(candidate_selectable(
            &cand("Y", true, None, None),
            false,
            false
        ));
    }

    #[test]
    fn non_empty_candidate_selectable_without_force() {
        let ne = cand("E", true, None, Some("根目录非空，请确认目标盘无误"));
        assert!(
            candidate_selectable(&ne, false, false),
            "非空盘应可选中，无需 --force"
        );
        assert!(ne.soft_hint.is_some());
        assert!(ne.can_init);
    }
}

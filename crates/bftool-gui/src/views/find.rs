//! 查找视图: 搜索框醒目，结果突出项目名、备份盘、盘内路径和时间，方便查看与复制；
//! 表格支持长路径截断与完整查看、一键复制路径、明确的空状态和错误提示。

use eframe::egui;

use bftool_core::engine::find::{FindMatch, FindOutcome};

use crate::app::App;
use crate::task::BackgroundTask;
use crate::views::{theme, util};

/// 查找页跨帧状态。
#[derive(Debug, Default)]
pub struct FindUiState {
    pub keyword: String,
    pub result: Option<FindOutcome>,
    pub error: Option<String>,
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    theme::page_header(
        ui,
        "项目检索与定位",
        "快速检索全部备份盘与多机汇总索引名单中的项目，获取备份盘号、盘内完整路径及校验记录。",
    );

    let busy = app.is_busy();
    let extra = app.cfg.extra_catalogs.len();

    // ── 醒目的搜索栏卡片 ──
    theme::card(ui, |ui| {
        let input_width = (ui.available_width() - 180.0).clamp(80.0, 380.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("关键词：")
                    .font(theme::subtitle_font())
                    .color(theme::TEXT_TITLE),
            );

            let edit_resp = ui.add(
                egui::TextEdit::singleline(&mut app.find_ui.keyword)
                    .hint_text("输入项目名、文件夹名或部分路径…")
                    .desired_width(input_width),
            );

            if edit_resp.changed() {
                app.find_ui.result = None;
                app.find_ui.error = None;
            }

            let mut do_search = false;
            if edit_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                do_search = !busy;
            }

            let search_btn = ui.add_enabled(!busy, theme::btn_primary("🔍 查找"));
            if search_btn.clicked() {
                do_search = true;
            }

            if !app.find_ui.keyword.is_empty() && ui.button("清空").clicked() {
                app.find_ui.keyword.clear();
                app.find_ui.result = None;
                app.find_ui.error = None;
            }

            if app.find_task.is_some() {
                ui.spinner();
                ui.label("正在全盘检索…");
            }

            if do_search {
                run_search(app);
            }
        });

        if extra > 0 {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("已启用多机汇总查询：检索本机总索引 + {extra} 个外部机器索引（在「设置」页管理）。"))
                    .size(11.5)
                    .color(theme::TEXT_MUTED),
            );
        }
    });

    ui.add_space(theme::GAP);

    // 错误提示条
    if let Some(err) = &app.find_ui.error {
        util::error_banner(
            ui,
            "检索错误",
            err,
            "请检查关键词格式或确认索引文件可正常读取。",
        );
        ui.add_space(theme::GAP);
    }

    // ── 结果展示 ──
    theme::card(ui, |ui| {
        match &app.find_ui.result {
            Some(outcome) => {
                // 部分索引失败提示
                if !outcome.sources_failed.is_empty() {
                    theme::callout_with_tag(
                        ui,
                        theme::WARN,
                        theme::WARN_SOFT,
                        "部分索引未读取",
                        &format!(
                            "{} 个外部索引文件读取失败已跳过：{}",
                            outcome.sources_failed.len(),
                            bftool_core::engine::find::render_failed_sources(
                                &outcome.sources_failed
                            )
                        ),
                    );
                    ui.add_space(6.0);
                }
                if outcome.malformed_rows > 0 {
                    theme::callout_with_tag(
                        ui,
                        theme::WARN,
                        theme::WARN_SOFT,
                        "格式跳过",
                        &format!(
                            "{} 行数据因格式不合规被跳过，未计入结果。",
                            outcome.malformed_rows
                        ),
                    );
                    ui.add_space(6.0);
                }

                if outcome.matches.is_empty() {
                    util::empty_state(
                        ui,
                        "未找到匹配项目",
                        &format!(
                            "未在已索引的 {} 个来源中检索到包含「{}」的项目。请核对关键词。",
                            outcome.sources_searched,
                            app.find_ui.keyword.trim()
                        ),
                    );
                } else {
                    ui.horizontal(|ui| {
                        theme::section_title(ui, "检索结果列表");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "共匹配到 {} 项（检索了 {} 个索引来源）",
                                    outcome.matches.len(),
                                    outcome.sources_searched
                                ))
                                .size(12.0)
                                .color(theme::TEXT_MUTED),
                            );
                        });
                    });
                    ui.add_space(6.0);

                    egui::ScrollArea::both()
                        .auto_shrink([false, true])
                        .max_height(420.0)
                        .id_salt("find_results_grid")
                        .show(ui, |ui| {
                            egui::Grid::new("find_table")
                                .num_columns(6)
                                .striped(true)
                                .spacing([12.0, 8.0])
                                .show(ui, |ui| {
                                    ui.strong("项目名 / 文件夹");
                                    ui.strong("备份盘");
                                    ui.strong("盘内完整相对路径");
                                    ui.strong("归档时间");
                                    ui.strong("校验状态");
                                    ui.strong("来源索引");
                                    ui.end_row();

                                    for m in &outcome.matches {
                                        // 项目名
                                        ui.label(
                                            egui::RichText::new(&m.folder)
                                                .font(theme::subtitle_font())
                                                .color(theme::TEXT_TITLE),
                                        );

                                        // 备份盘
                                        theme::badge(
                                            ui,
                                            &m.drive_id,
                                            theme::PRIMARY_SOFT,
                                            theme::PRIMARY,
                                        );

                                        // 盘内路径：带截断与一键复制
                                        util::copyable_path(ui, &m.in_drive_path, 34);

                                        // 归档时间
                                        let time_display = if m.archived_time.is_empty() {
                                            "未知时间".to_string()
                                        } else {
                                            m.archived_time
                                                .split('T')
                                                .next()
                                                .unwrap_or(&m.archived_time)
                                                .to_string()
                                        };
                                        ui.label(time_display);

                                        // 校验状态
                                        let (badge_bg, badge_fg) = if m.verify.contains("OK") {
                                            (theme::OK_SOFT, theme::OK)
                                        } else {
                                            (theme::WARN_SOFT, theme::WARN)
                                        };
                                        theme::badge(ui, &m.verify, badge_bg, badge_fg);

                                        // 来源
                                        ui.label(
                                            egui::RichText::new(&m.source)
                                                .size(11.5)
                                                .color(theme::TEXT_MUTED),
                                        );

                                        ui.end_row();
                                    }
                                });
                        });
                }
            }
            None => {
                if app.find_task.is_none() {
                    util::empty_state(
                        ui,
                        "等待检索输入",
                        "在上方搜索框输入项目名称或关键词，按下回车或点「查找」即可快速定位备份数据。",
                    );
                }
            }
        }
    });
}

fn run_search(app: &mut App) {
    if app.is_busy() {
        return;
    }
    let kw = match normalize_keyword(&app.find_ui.keyword) {
        Ok(kw) => kw,
        Err(e) => {
            app.find_ui.result = None;
            app.find_ui.error = Some(e);
            return;
        }
    };
    app.find_ui.error = None;
    app.find_ui.result = None;
    let cfg = app.cfg.clone();
    let backend = app.backend;
    app.find_task =
        Some(BackgroundTask::spawn(move |_cancel| backend.find(&cfg, &kw)).detachable());
}

pub fn normalize_keyword(raw: &str) -> Result<String, String> {
    let kw = raw.trim();
    if kw.is_empty() {
        Err("请输入关键词。".to_string())
    } else {
        Ok(kw.to_string())
    }
}

/// 一条命中 → 表格 5 列文本。纯函数，可测。
pub fn match_row(m: &FindMatch) -> [String; 5] {
    [
        m.folder.clone(),
        m.source.clone(),
        m.drive_id.clone(),
        m.in_drive_path.clone(),
        m.verify.clone(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_ui_default_empty() {
        let s = FindUiState::default();
        assert!(s.keyword.is_empty());
        assert!(s.result.is_none());
        assert!(s.error.is_none());
    }

    #[test]
    fn match_row_maps_columns() {
        let m = FindMatch {
            folder: "001p".into(),
            drive_id: "备份1".into(),
            archived_time: "2026-05-29".into(),
            project_no: "001".into(),
            in_drive_path: "项目\\001p".into(),
            verify: "SHA256-OK".into(),
            source: "本机".into(),
        };
        let r = match_row(&m);
        assert_eq!(r[0], "001p");
        assert_eq!(r[1], "本机");
        assert_eq!(r[2], "备份1");
        assert_eq!(r[3], "项目\\001p");
        assert_eq!(r[4], "SHA256-OK");
    }

    #[test]
    fn normalize_keyword_rejects_empty() {
        assert!(normalize_keyword("").is_err());
        assert!(normalize_keyword("   ").is_err());
        assert_eq!(normalize_keyword(" 001 ").unwrap(), "001");
    }
}

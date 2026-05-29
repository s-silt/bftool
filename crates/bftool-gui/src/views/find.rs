//! 查找视图:关键词 → find::search → 结果表(来源 / 在哪块盘 / 盘内路径 / 校验)。
//! 支持多机汇总:本机总索引 + 设置页配置的额外索引一并检索。(Spec D §5)

use eframe::egui;

use bftool_core::engine::find::{self, FindMatch, FindOutcome};
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::views::util;

/// 查找页跨帧状态。直接持有 core 返回的 `FindOutcome`,避免把命中/来源数/失败来源
/// 拆成多个字段后还要在各分支手动同步。
#[derive(Debug, Default)]
pub struct FindUiState {
    pub keyword: String,
    pub result: Option<FindOutcome>,
    pub error: Option<String>,
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    ui.heading("查找");
    let extra = app.cfg.extra_catalogs.len();
    if extra > 0 {
        ui.weak(format!(
            "多机汇总：本机索引 + {extra} 个额外来源(在「设置」页管理)。"
        ));
    }
    ui.add_space(4.0);

    let mut do_search = false;
    ui.horizontal(|ui| {
        ui.label("关键词：");
        let resp = ui.text_edit_singleline(&mut app.find_ui.keyword);
        if resp.changed() {
            app.find_ui.result = None; // 关键词改变立即清空旧结果
            app.find_ui.error = None;
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            do_search = true;
        }
        if ui.button("查找").clicked() {
            do_search = true;
        }
    });
    if do_search {
        run_search(app);
    }

    ui.separator();
    if let Some(err) = &app.find_ui.error {
        ui.colored_label(util::level_color(LogLevel::Error), err);
    }
    match &app.find_ui.result {
        Some(outcome) => {
            if !outcome.sources_failed.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(0xB0, 0x6A, 0x00),
                    format!(
                        "{} 个索引来源读取失败已跳过：{}",
                        outcome.sources_failed.len(),
                        outcome.sources_failed.join("、")
                    ),
                );
            }
            if outcome.matches.is_empty() {
                ui.label("无匹配。");
            } else {
                ui.label(format!(
                    "匹配 {} 项(检索了 {} 个来源)：",
                    outcome.matches.len(),
                    outcome.sources_searched
                ));
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        egui::Grid::new("find_grid")
                            .num_columns(5)
                            .striped(true)
                            .show(ui, |ui| {
                                ui.strong("文件夹");
                                ui.strong("来源");
                                ui.strong("备份盘");
                                ui.strong("盘内路径");
                                ui.strong("校验");
                                ui.end_row();
                                for m in &outcome.matches {
                                    let [folder, source, drive_id, path, verify] = match_row(m);
                                    ui.monospace(folder);
                                    ui.label(source);
                                    ui.label(drive_id);
                                    ui.monospace(path);
                                    ui.label(verify);
                                    ui.end_row();
                                }
                            });
                    });
            }
        }
        None => {
            ui.weak("输入关键词后回车 / 点「查找」。");
        }
    }
}

fn run_search(app: &mut App) {
    let kw = app.find_ui.keyword.trim().to_string();
    app.find_ui.error = None;
    if kw.is_empty() {
        app.find_ui.result = None;
        app.find_ui.error = Some("请输入关键词。".to_string());
        return;
    }
    match find::search(&app.cfg, &kw) {
        Ok(outcome) => app.find_ui.result = Some(outcome),
        Err(e) => {
            app.find_ui.result = None;
            app.find_ui.error = Some(format!("查找失败：{:#}", e));
        }
    }
}

/// 一条命中 → 表格 5 列文本。纯函数,可测。
fn match_row(m: &FindMatch) -> [String; 5] {
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
}

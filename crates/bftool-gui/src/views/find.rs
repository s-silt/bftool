//! 查找视图:关键词 → find::search → 结果表(来源 / 在哪块盘 / 盘内路径 / 校验)。
//! 支持多机汇总:本机总索引 + 设置页配置的额外索引一并检索。(Spec D §5)

use eframe::egui;

use bftool_core::engine::find::{self, FindMatch};

use crate::app::App;

/// 查找页跨帧状态。
#[derive(Debug, Default)]
pub struct FindUiState {
    pub keyword: String,
    pub results: Option<Vec<FindMatch>>,
    /// 上次查询检索了几个来源 / 哪些额外来源读不了(供提示)。
    pub sources_searched: usize,
    pub sources_failed: Vec<String>,
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
        ui.colored_label(egui::Color32::from_rgb(0xCC, 0x33, 0x33), err);
    }
    if !app.find_ui.sources_failed.is_empty() {
        ui.colored_label(
            egui::Color32::from_rgb(0xB0, 0x6A, 0x00),
            format!(
                "{} 个额外索引来源读取失败已跳过：{}",
                app.find_ui.sources_failed.len(),
                app.find_ui.sources_failed.join("、")
            ),
        );
    }
    match &app.find_ui.results {
        Some(rows) if !rows.is_empty() => {
            ui.label(format!(
                "匹配 {} 项(检索了 {} 个来源)：",
                rows.len(),
                app.find_ui.sources_searched
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
                            for m in rows {
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
        Some(_) => {
            ui.label("无匹配。");
        }
        None => {
            ui.weak("输入关键词后回车 / 点「查找」。");
        }
    }
}

fn run_search(app: &mut App) {
    let kw = app.find_ui.keyword.trim().to_string();
    app.find_ui.error = None;
    app.find_ui.sources_failed.clear();
    if kw.is_empty() {
        app.find_ui.results = None;
        app.find_ui.sources_searched = 0;
        app.find_ui.error = Some("请输入关键词。".to_string());
        return;
    }
    match find::search(&app.cfg, &kw) {
        Ok(outcome) => {
            app.find_ui.results = Some(outcome.matches);
            app.find_ui.sources_searched = outcome.sources_searched;
            app.find_ui.sources_failed = outcome.sources_failed;
        }
        Err(e) => {
            app.find_ui.results = None;
            app.find_ui.sources_searched = 0;
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
        assert!(s.results.is_none());
        assert!(s.error.is_none());
        assert_eq!(s.sources_searched, 0);
        assert!(s.sources_failed.is_empty());
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

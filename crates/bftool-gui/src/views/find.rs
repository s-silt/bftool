//! 查找视图:关键词 → find::search → 结果表(来源 / 在哪块盘 / 盘内路径 / 校验)。
//! 支持多机汇总:本机总索引 + 设置页配置的额外索引一并检索。(Spec D §5)

use eframe::egui;

use bftool_core::engine::find::{self, FindMatch, FindOutcome};
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::task::BackgroundTask;
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
        // R5-7:用 !is_busy()(含 backup/verify 的 app.task、plan_task、其它 find_task),
        // 否则查找可与备份/复查并发跑。
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            do_search = !app.is_busy();
        }
        if ui
            .add_enabled(!app.is_busy(), egui::Button::new("查找"))
            .clicked()
        {
            do_search = true;
        }
        if app.find_task.is_some() {
            ui.spinner();
            ui.label("查找中…");
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
                        find::render_failed_sources(&outcome.sources_failed)
                    ),
                );
            }
            if outcome.malformed_rows > 0 {
                ui.colored_label(
                    egui::Color32::from_rgb(0xB0, 0x6A, 0x00),
                    format!(
                        "{} 行因格式错误被跳过(未计入结果)。",
                        outcome.malformed_rows
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
            if app.find_task.is_none() {
                ui.weak("输入关键词后回车 / 点「查找」。");
            }
        }
    }
}

fn run_search(app: &mut App) {
    if app.is_busy() {
        return; // R5-7:任何任务进行中(备份/复查/计划/查找)都不重入
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
    app.find_task = Some(
        BackgroundTask::spawn(move |_cancel| find::search(&cfg, &kw))
            // 只读查询:关窗时 detach 而非 join,避免卡在慢速/掉线网络盘的大索引读上挂死 UI。(review-r3 round3)
            .detachable(),
    );
}

fn normalize_keyword(raw: &str) -> Result<String, String> {
    let kw = raw.trim();
    if kw.is_empty() {
        Err("请输入关键词。".to_string())
    } else {
        Ok(kw.to_string())
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

    #[test]
    fn normalize_keyword_rejects_empty() {
        assert_eq!(normalize_keyword("  ").unwrap_err(), "请输入关键词。");
        assert_eq!(normalize_keyword("  proj ").unwrap(), "proj");
    }
}

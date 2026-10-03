//! 监视视图：清晰展示监视目录、过滤条件、运行状态、轮询周期、开始／停止及最近结果，保留源文件的语义明确。
//! 增量轮询归档，保留源目录文件不变动。

use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use eframe::egui;

use bftool_core::pipeline::archive::Options;
use bftool_core::service::FileFilter;
use bftool_core::service::WatchRequest;

use crate::app::App;
use crate::reporter::GuiReporter;
use crate::task::BackgroundTask;
use crate::views::{theme, util};

#[derive(Debug, Clone, Default)]
pub struct WatchUiState {
    pub folder: String,
    pub ext_text: String,
    pub recursive: bool,
    pub once: bool,
    pub poll_secs_text: String,
    pub last_msg: Option<String>,
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    theme::page_header(
        ui,
        "文件夹增量监视归档",
        "后台周期性轮询指定目录，对新增或变更的文件自动执行增量归档校验；源文件严格保留，绝不删除或移动。",
    );

    let busy = app.is_busy();

    // ── 核心安全语义明确声明卡片 ──
    theme::card(ui, |ui| {
        theme::callout_with_tag(
            ui,
            theme::PRIMARY,
            theme::PRIMARY_SOFT,
            "保留源文件规则",
            "【增量保留模式】：监视任务在完成数据复制与 SHA256 三重校验后，将严格保留源目录中的所有原始文件，不会执行剪切或移动操作，适合下载盘、工作区或共享目录的持续保护。",
        );
    });

    ui.add_space(theme::GAP);

    // ── 监视配置卡片 ──
    theme::card(ui, |ui| {
        theme::section_title(ui, "1. 监视目标与过滤规则");
        ui.add_space(4.0);

        // 监视目录
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("监视目录：")
                    .font(theme::subtitle_font())
                    .color(theme::TEXT_TITLE),
            );
            ui.add_enabled_ui(!busy, |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.watch_ui.folder)
                        .hint_text("点击右侧按钮选择或输入需要持续监视的文件夹路径…")
                        .desired_width(420.0_f32.min(ui.available_width())),
                );
                if ui
                    .add_enabled(!app.backend.is_demo(), theme::btn_secondary("浏览目录…"))
                    .clicked()
                {
                    let mut dlg = rfd::FileDialog::new().set_title("选择需要持续监视的目录");
                    let cur = app.watch_ui.folder.trim();
                    if !cur.is_empty() {
                        dlg = dlg.set_directory(cur);
                    }
                    if let Some(p) = dlg.pick_folder() {
                        app.watch_ui.folder = p.display().to_string();
                    }
                }
            });
        });

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        // 过滤条件
        theme::section_title(ui, "2. 过滤条件与轮询周期");
        ui.add_space(4.0);

        egui::Grid::new("watch_filter_grid")
            .num_columns(2)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label("扩展名过滤 (可选)：");
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut app.watch_ui.ext_text)
                                .hint_text("逗号分隔，如: zip, 7z, tar (留空监视所有文件)")
                                .desired_width(260.0),
                        );
                        ui.checkbox(&mut app.watch_ui.recursive, "递归扫描子目录");
                    });
                });
                ui.end_row();

                ui.label("轮询周期与单次模式：");
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.label("轮询间隔 (秒)：");
                        ui.add(
                            egui::TextEdit::singleline(&mut app.watch_ui.poll_secs_text)
                                .hint_text("默认 60")
                                .desired_width(80.0),
                        );
                        ui.add_space(10.0);
                        ui.checkbox(&mut app.watch_ui.once, "仅执行单轮归档 (--once)");
                    });
                });
                ui.end_row();
            });
    });

    ui.add_space(theme::GAP);

    // ── 运行控制与状态展示 ──
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            theme::section_title(ui, "运行控制与实时状态");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if app.task_running(crate::app::TaskKind::Watch) {
                    theme::badge(ui, "监视任务运行中", theme::PRIMARY, egui::Color32::WHITE);
                } else if busy {
                    theme::badge(ui, app.busy_label(), theme::TRACK, theme::TEXT_MUTED);
                } else {
                    theme::badge(ui, "就绪 / 未运行", theme::TRACK, theme::TEXT_MUTED);
                }
            });
        });
        ui.add_space(6.0);

        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    !busy,
                    theme::btn_primary(&app.operation_label("开始监视归档")),
                )
                .clicked()
            {
                start_watch(app);
            }
            if app.task_running(crate::app::TaskKind::Watch) {
                let requested = app
                    .task
                    .as_ref()
                    .map(|t| t.cancel_requested())
                    .unwrap_or(false);
                if requested {
                    theme::badge(ui, "正在停止中…", theme::WARN, egui::Color32::WHITE);
                } else {
                    let stop_btn = ui.add(theme::btn_danger("⏹ 停止监视任务"));
                    if stop_btn.clicked() {
                        if let Some(t) = app.task.as_ref() {
                            t.request_cancel();
                        }
                    }
                    stop_btn.on_hover_text("停止后续轮询，正在传输校验中的文件将安全完成");
                }
                ui.add_space(8.0);
                ui.spinner();
                ui.label(
                    egui::RichText::new("后台轮询引擎正在监听目录变更…").color(theme::PRIMARY),
                );
            } else if busy {
                ui.label(format!(
                    "{}，请在全局状态栏停止该任务或等待结束。",
                    app.busy_label()
                ));
            }
        });

        if let Some(msg) = &app.watch_ui.last_msg {
            ui.add_space(6.0);
            theme::callout_with_tag(ui, theme::PRIMARY, theme::PRIMARY_SOFT, "最近结果", msg);
        }
    });

    ui.add_space(theme::GAP);

    // ── 进度条与实时日志 ──
    theme::card(ui, |ui| {
        theme::section_title(ui, "归档进度与监视日志");
        ui.add_space(4.0);
        util::progress_bar(&app.progress, ui);
        util::log_panel(&app.logs, ui);
    });
}

fn start_watch(app: &mut App) {
    if !app.ensure_idle() {
        return;
    }
    let folder_str = app.watch_ui.folder.trim().to_string();
    if folder_str.is_empty() {
        app.watch_ui.last_msg = Some("错误：请先指定需要监视的目录路径。".into());
        return;
    }
    let folder = PathBuf::from(&folder_str);
    if !app.backend.watch_folder_exists(&folder) {
        app.watch_ui.last_msg = Some(format!("错误：指定的监视目录不存在：{}", folder.display()));
        return;
    }

    let exts: Vec<String> = app
        .watch_ui
        .ext_text
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let poll_secs = app
        .watch_ui
        .poll_secs_text
        .trim()
        .parse::<u64>()
        .unwrap_or(60);

    let files = FileFilter::from_exts(exts.clone(), app.watch_ui.recursive);
    let archive = Options {
        dry_run: false,
        no_hash: false,
        limit: 0,
        drive_letter_override: None,
        no_test_archives: false,
        source_override: Some(folder.clone()),
        incremental: true,
        retain_source: true, // 核心安全规则：严格保留源文件
        include_ext: if exts.is_empty() { None } else { Some(exts) },
        file_globs: Vec::new(),
        ext_recursive: app.watch_ui.recursive,
        include_subfolder_projects: true,
        seed_from_global_catalog: true,
        incremental_verify: Default::default(),
    };
    let req = WatchRequest {
        folder,
        files,
        poll_secs,
        once: app.watch_ui.once,
        archive,
    };
    let cfg = app.cfg.clone();
    let backend = app.backend;
    let progress = Arc::clone(&app.progress);
    let (tx, rx) = mpsc::channel();
    app.rx = Some(rx);
    app.task_kind = Some(crate::app::TaskKind::Watch);
    app.task_started = Some(std::time::Instant::now());
    app.logs.clear();
    app.last_summary = None;
    if let Ok(mut p) = app.progress.lock() {
        *p = Default::default();
    }
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        let reporter = GuiReporter::new(tx, progress);
        let summary = backend.watch(&cfg, &req, cancel, &reporter)?;
        Ok(crate::app::ExecutionResult::Summary(format!(
            "监视{}：共轮询 {} 轮，成功归档 {} 项，未改动跳过 {} 项，失败 {} 项",
            if summary.cancelled {
                "已取消"
            } else {
                "结束"
            },
            summary.cycles,
            summary.archived,
            summary.skipped_unchanged,
            summary.failed
        )))
    }));
    app.watch_ui.last_msg = Some("已启动后台监视，正在监听文件变动…".into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regression_busy_watch_returns_before_folder_probe_or_channel_mutation() {
        let mut app = crate::app::tests::fixture();
        crate::app::tests::occupy_find(&mut app);
        let (tx, rx) = mpsc::channel();
        tx.send(crate::reporter::UiEvent::Log {
            level: bftool_core::reporter::LogLevel::Info,
            msg: "owned by first task".into(),
        })
        .unwrap();
        app.rx = Some(rx);
        start_watch(&mut app);
        assert!(
            app.watch_ui.last_msg.is_none(),
            "busy action must return before folder validation"
        );
        assert!(app.task.is_none());
        assert!(
            app.rx.as_ref().unwrap().try_recv().is_ok(),
            "first task still owns its log channel"
        );
    }
}

//! 任务与记录视图:
//! 实时监控当前执行任务（活动进度、正在处理文件、速率与耗时），
//! 呈现历史执行记录与状态汇总（成功、部分完成、失败、已停止、未校验），
//! 并集成实时运行日志面板与归档查找入口，不堆砌无意义的 KPI 指标。

use eframe::egui::{self, Color32};

use crate::app::App;
use crate::views::{theme, util};

pub struct LoadedHistory {
    target: String,
    records: Vec<bftool_core::pipeline::backup::BackupHistoryRecord>,
}
#[derive(Default)]
pub struct TasksUiState {
    pub history_task: Option<crate::task::BackgroundTask<LoadedHistory>>,
    pub(crate) records: Vec<bftool_core::pipeline::backup::BackupHistoryRecord>,
    target: String,
    pub(crate) error: Option<String>,
    pub(crate) loaded: bool,
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    theme::page_header(
        ui,
        "任务执行与记录",
        "查看当前任务实时进度、历史备份复查执行记录及详细运行日志。任务运行期间支持自由切换页面。",
    );

    // ── 1. 当前活动任务监控卡片 ──
    render_active_task_section(app, ui);

    ui.add_space(theme::GAP);

    // ── 2. 最近执行记录表 ──
    if app.backend.is_demo() {
        render_history_section(app, ui);
    } else {
        render_plain_history(app, ui);
    }

    ui.add_space(theme::GAP);

    // ── 3. 详细运行日志流面板 ──
    render_log_stream_section(app, ui);
}

fn render_active_task_section(app: &mut App, ui: &mut egui::Ui) {
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            theme::section_title(ui, "当前活动任务监控");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if app.is_busy() || app.backup_ui.is_running {
                    theme::badge(ui, "任务执行中", theme::PRIMARY, Color32::WHITE);
                } else {
                    theme::badge(ui, "空闲中 (无活动任务)", theme::TRACK, theme::TEXT_MUTED);
                }
            });
        });
        ui.add_space(4.0);

        if app.backup_ui.is_running || app.backup_ui.stopping_requested {
            // 当前文件
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("正在处理：")
                        .size(12.0)
                        .color(theme::TEXT_MUTED),
                );
                let curr = if app.backup_ui.current_file.is_empty() {
                    "准备文件流...".to_string()
                } else {
                    app.backup_ui.current_file.clone()
                };
                ui.label(
                    egui::RichText::new(curr)
                        .size(12.0)
                        .color(theme::TEXT_TITLE),
                );
            });

            ui.add_space(4.0);
            let frac = if app.backup_ui.total_bytes > 0 {
                (app.backup_ui.transferred_bytes as f32 / app.backup_ui.total_bytes as f32)
                    .clamp(0.0, 1.0)
            } else {
                0.0
            };
            theme::hbar(ui, frac, theme::PRIMARY);

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(app.backup_ui.running_progress_text())
                        .size(11.5)
                        .color(theme::TEXT_MUTED),
                );

                if let (Some(bps), Some(eta)) = (app.backup_ui.speed_bps, app.backup_ui.eta_secs) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let speed_mb = bps as f64 / 1024.0 / 1024.0;
                        let eta_min = eta / 60;
                        let eta_sec = eta % 60;
                        ui.label(
                            egui::RichText::new(format!(
                                "【合成示例】{:.1} MB/s · 示例剩余 {:02}:{:02}",
                                speed_mb, eta_min, eta_sec
                            ))
                            .size(11.5)
                            .color(theme::PRIMARY),
                        );
                    });
                }
            });
        } else if let Some(summary) = &app.last_summary {
            ui.horizontal(|ui| {
                theme::badge(ui, "最近记录", theme::PRIMARY_SOFT, theme::PRIMARY);
                ui.label(
                    egui::RichText::new(summary)
                        .size(12.0)
                        .color(theme::TEXT_BODY),
                );
            });
        } else {
            ui.colored_label(
                theme::TEXT_MUTED,
                "当前暂无正在执行的后台任务。请前往「备份」或「校验」页面启动任务。",
            );
        }
    });
}

fn render_history_section(app: &App, ui: &mut egui::Ui) {
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            theme::section_title(ui, "最近任务记录");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    egui::RichText::new("记录保留在本地安全会话中")
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                );
            });
        });
        ui.add_space(4.0);

        egui::Grid::new("task_history_table")
            .striped(true)
            .num_columns(5)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                ui.strong("时间");
                ui.strong("任务类型");
                ui.strong("最终状态");
                ui.strong("已处理规模");
                ui.strong("校验结论");
                ui.end_row();

                if app.backend.is_demo() {
                    // 演示模式示例记录
                    ui.label("2026-10-03 19:15");
                    ui.label("文件备份");
                    theme::badge(ui, "已停止", theme::TEXT_MUTED, Color32::WHITE);
                    ui.label("48 项 · 68.5 GB");
                    ui.colored_label(theme::TEXT_MUTED, "未校验 (中途安全停止)");
                    ui.end_row();

                    ui.label("2026-10-03 18:30");
                    ui.label("文件备份");
                    theme::badge(ui, "部分完成", theme::WARN, Color32::WHITE);
                    ui.label("2/3 项 · 4.5 GB");
                    ui.colored_label(theme::WARN, "未做内容校验 (1 项不可读)");
                    ui.end_row();

                    ui.label("2026-10-02 16:40");
                    ui.label("整盘复查");
                    theme::badge(ui, "复查通过", theme::OK, Color32::WHITE);
                    ui.label("152 项 · 240.2 GB");
                    ui.colored_label(theme::OK, "全量 SHA256 内容已校验");
                    ui.end_row();
                } else if let Some(summary) = &app.last_summary {
                    ui.label("本次会话");
                    ui.label(
                        app.task_kind
                            .map(|k| match k {
                                crate::app::TaskKind::Backup => "普通目录备份",
                                crate::app::TaskKind::Archive => "文件备份",
                                crate::app::TaskKind::Verify => "完整性复查",
                                crate::app::TaskKind::Watch => "实时监视",
                            })
                            .unwrap_or("备份"),
                    );
                    theme::badge(ui, "已结束", theme::PRIMARY_SOFT, theme::PRIMARY);
                    ui.label(summary);
                    ui.colored_label(theme::TEXT_MUTED, "详见日志");
                    ui.end_row();
                } else {
                    ui.colored_label(theme::TEXT_MUTED, "—");
                    ui.colored_label(theme::TEXT_MUTED, "暂无任务历史");
                    ui.colored_label(theme::TEXT_MUTED, "—");
                    ui.colored_label(theme::TEXT_MUTED, "0 项");
                    ui.colored_label(theme::TEXT_MUTED, "—");
                    ui.end_row();
                }
            });
    });
}

fn render_log_stream_section(app: &mut App, ui: &mut egui::Ui) {
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            theme::section_title(ui, "实时运行日志");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(egui::RichText::new("复制全部日志").size(11.0))
                    .clicked()
                {
                    let mut all_logs = String::new();
                    for (level, msg) in &app.logs {
                        all_logs.push_str(&format!("[{:?}] {}\n", level, msg));
                    }
                    ui.ctx().copy_text(all_logs);
                }
                if ui
                    .button(egui::RichText::new("清空日志").size(11.0))
                    .clicked()
                {
                    app.logs.clear();
                }
            });
        });
        ui.add_space(4.0);

        util::log_panel(&app.logs, ui);
    });
}

pub(crate) fn refresh_history(app: &mut App) {
    if app.backend.is_demo() || app.backup_ui.target_path.is_empty() || !app.ensure_idle() {
        return;
    }
    let target = app.backup_ui.target_path.clone();
    app.tasks_ui.target = target.clone();
    app.tasks_ui.records.clear();
    app.tasks_ui.loaded = false;
    app.tasks_ui.error = None;
    app.tasks_ui.history_task = Some(crate::task::BackgroundTask::spawn(move |cancel| {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("历史读取已取消");
        }
        let records = bftool_core::service::list_backup_history_with_cancel(
            std::path::Path::new(&target),
            cancel,
        )?;
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("历史读取已取消");
        }
        Ok(LoadedHistory { target, records })
    }));
}

pub(crate) fn pump_history(app: &mut App, ctx: &egui::Context) {
    if app.tasks_ui.target != app.backup_ui.target_path && app.tasks_ui.history_task.is_none() {
        app.tasks_ui.records.clear();
        app.tasks_ui.loaded = false;
        app.tasks_ui.error = None;
        app.tasks_ui.target = app.backup_ui.target_path.clone();
    }
    match app
        .tasks_ui
        .history_task
        .as_ref()
        .map(|task| task.is_finished())
    {
        Some(true) => {
            let result = app
                .tasks_ui
                .history_task
                .as_mut()
                .and_then(|task| task.take_outcome());
            app.tasks_ui.history_task = None;
            match result {
                Some(crate::task::TaskOutcome::Done(history))
                    if history.target == app.backup_ui.target_path =>
                {
                    app.tasks_ui.records = history.records;
                    app.tasks_ui.target = history.target;
                    app.tasks_ui.loaded = true;
                    app.tasks_ui.error = None;
                }
                Some(crate::task::TaskOutcome::Failed(error)) => app.tasks_ui.error = Some(error),
                _ => app.tasks_ui.error = Some("选择已改变，历史结果已丢弃".into()),
            }
        }
        Some(false) => ctx.request_repaint(),
        None => {}
    }
}

fn render_plain_history(app: &mut App, ui: &mut egui::Ui) {
    theme::card(ui, |ui| {
        theme::section_title(ui, "目标目录持久任务记录");
        ui.label("从所选目录的任务清单和日志读取；会话摘要不属于持久历史。未完成任务可复验后按文件重试，不支持字节断点。");
        ui.label(if app.backup_ui.target_path.is_empty() {
            "请先在备份页选择目标目录"
        } else {
            &app.backup_ui.target_path
        });
        if ui
            .add_enabled(
                !app.is_busy() && !app.backup_ui.target_path.is_empty(),
                theme::btn_secondary("读取 / 刷新持久记录"),
            )
            .clicked()
        {
            refresh_history(app);
        }
        if app.tasks_ui.history_task.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("后台读取记录…");
                let task = app.tasks_ui.history_task.as_ref().unwrap();
                if ui
                    .add_enabled(!task.cancel_requested(), theme::btn_secondary("取消读取"))
                    .clicked()
                {
                    task.request_cancel();
                }
            });
        }
        if let Some(error) = &app.tasks_ui.error {
            ui.colored_label(theme::DANGER, error);
        }
        if app.tasks_ui.loaded && app.tasks_ui.records.is_empty() {
            ui.label("未找到持久备份任务");
        }
        let mut retry = None;
        egui::ScrollArea::both().max_height(260.0).show(ui, |ui| {
            for record in &app.tasks_ui.records {
                ui.group(|ui| {
                    ui.label(format!(
                        "{} · {} · {}",
                        record.job_id,
                        record.destination_name.display(),
                        record.state
                    ));
                    ui.label(format!(
                        "来源 {} · 计划规模 {}",
                        record.source.path().display(),
                        util::fmt_gb(record.bytes)
                    ));
                    if matches!(
                        record.source,
                        bftool_core::pipeline::backup::SourceSelection::Directory(_)
                    ) {
                        ui.label(folder_rule_label(&record.directory_options));
                    }
                    ui.label(if record.completed {
                        "日志标记已完成；当前内容完整性请在校验页重新检查"
                    } else {
                        "未完成 / 未确认发布；不要视为完成备份"
                    });
                    if !record.completed
                        && ui
                            .add_enabled(!app.is_busy(), theme::btn_secondary("复验并按文件重试"))
                            .clicked()
                    {
                        retry = Some(record.job_id.clone());
                    }
                });
            }
        });
        if let Some(job) = retry {
            crate::views::backup::adapter::resume(app, job);
        }
    });
}

fn folder_rule_label(options: &bftool_core::pipeline::backup::DirectoryOptions) -> String {
    let scope = if options.recursive {
        "包含子文件夹（递归）"
    } else {
        "仅当前层（不包含子文件夹）"
    };
    let suffixes = options
        .extensions
        .as_ref()
        .map(|values| {
            if values.is_empty() {
                "未选择后缀".into()
            } else {
                values
                    .iter()
                    .map(|s| format!(".{s}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        })
        .unwrap_or_else(|| "全部后缀".into());
    format!(
        "范围：{scope}；后缀：{suffixes}；无后缀文件：{}",
        if options.include_extensionless {
            "包含"
        } else {
            "排除"
        }
    )
}

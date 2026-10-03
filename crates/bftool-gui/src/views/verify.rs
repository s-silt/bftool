//! 复查视图: 整盘、项目、文件选择清晰；分别展示已验证、损坏、缺失、不可验证和未检查，不把零检查数显示为全部正常。
//! 对备份盘严格只读，封盘亦可安全复查。

use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use eframe::egui;

use bftool_core::engine::drive;
use bftool_core::engine::verify::{VerifyIssueKind, VerifyReport};
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::reporter::{GuiReporter, ProgressState};
use crate::task::BackgroundTask;
use crate::views::{theme, util};

/// 复查结果明细数据
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerifyStats {
    pub checked: u64,
    pub bad: u64,
    pub extra: u64,
    pub size_only: u64,
    pub cancelled: bool,
}

impl VerifyStats {
    pub fn from_report(report: &VerifyReport) -> Self {
        Self {
            checked: report.checked,
            bad: report.bad,
            extra: report.extra,
            size_only: report.size_only,
            cancelled: report.cancelled,
        }
    }
}

/// 复查页跨帧状态。
#[derive(Debug, Default)]
pub struct VerifyUiState {
    pub drives: Option<Vec<drive::DriveInfo>>,
    pub selected: Option<String>,
    pub last_stats: Option<VerifyStats>,
    pub last_report: Option<VerifyReport>,
    pub summary: Option<String>,
    pub error: Option<String>,
    generation: u64,
    active_plain: Option<(u64, PathBuf)>,
    verified_target: Option<PathBuf>,
}

impl VerifyUiState {
    pub(crate) fn invalidate_plain(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.active_plain = None;
        self.verified_target = None;
        self.last_stats = None;
        self.last_report = None;
        self.summary = None;
        self.error = None;
    }

    pub(crate) fn begin_plain(&mut self, target: PathBuf) -> u64 {
        self.invalidate_plain();
        self.active_plain = Some((self.generation, target));
        self.generation
    }

    pub(crate) fn enforce_target(&mut self, target: &std::path::Path) {
        if self
            .active_plain
            .as_ref()
            .is_some_and(|(_, bound)| bound != target)
            || self
                .verified_target
                .as_ref()
                .is_some_and(|bound| bound != target)
        {
            self.invalidate_plain();
        }
    }

    pub(crate) fn accept_plain(
        &mut self,
        selected: &std::path::Path,
        target: PathBuf,
        token: u64,
        result: Result<VerifyReport, String>,
    ) -> Option<String> {
        if selected != target || self.active_plain.as_ref() != Some(&(token, target.clone())) {
            return None;
        }
        self.active_plain = None;
        self.verified_target = Some(target.clone());
        let text = match result {
            Ok(report) => {
                let text = format!(
                    "{}: {}",
                    target.display(),
                    verify_summary(
                        report.checked,
                        report.bad,
                        report.extra,
                        report.size_only,
                        report.cancelled
                    )
                );
                self.last_stats = Some(VerifyStats::from_report(&report));
                self.last_report = Some(report);
                self.summary = Some(text.clone());
                self.error = None;
                text
            }
            Err(error) => {
                let text = format!("{}: {error}", target.display());
                self.last_stats = None;
                self.last_report = None;
                self.summary = None;
                self.error = Some(text.clone());
                text
            }
        };
        Some(text)
    }

    pub(crate) fn fail_plain(&mut self, selected: &std::path::Path, error: String) {
        if let Some((token, target)) = self.active_plain.clone() {
            let _ = self.accept_plain(selected, target, token, Err(error));
        }
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    if !app.backend.is_demo() {
        plain_ui(app, ui);
        return;
    }
    theme::page_header(
        ui,
        "数据完整性复查",
        "重算备份盘上每个归档项目的真实 SHA256 校验和并与历史清单逐一比对；严格只读，封盘亦可安全复查。",
    );

    let busy = app.is_busy();

    // ── 模式与目标选择 ──
    theme::card(ui, |ui| {
        theme::section_title(ui, "1. 选择复查范围与目标");
        ui.add_space(4.0);

        // 刷新盘
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, theme::btn_secondary("🔄 刷新备份盘"))
                .clicked()
            {
                rescan(app);
            }
            if app.verify_ui.drives.is_none() {
                rescan(app);
            }
        });
        ui.add_space(8.0);

        // 整盘选择
        ui.label(
            egui::RichText::new("方式 A：整盘复查（核验指定盘上的所有备份项目）")
                .font(theme::subtitle_font())
                .color(theme::TEXT_TITLE),
        );
        ui.add_space(2.0);

        let mut sel = app.verify_ui.selected.clone();
        ui.horizontal_wrapped(|ui| {
            let auto_on = sel.is_none();
            let auto_resp = ui.selectable_label(auto_on, "自动识别单盘");
            if auto_resp.clicked() {
                sel = None;
            }
            if let Some(drives) = &app.verify_ui.drives {
                for d in drives {
                    let on = sel.as_deref() == Some(d.letter.as_str());
                    let label = format!(
                        "{} ({}:) - 剩余 {}",
                        d.id,
                        d.letter,
                        util::fmt_gb(d.free_bytes)
                    );
                    if ui.selectable_label(on, label).clicked() {
                        sel = Some(d.letter.clone());
                    }
                }
            }
        });
        app.verify_ui.selected = sel;

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);

        // 局部复查
        ui.label(
            egui::RichText::new("方式 B：针对性定向复查（选指定项目或具体文件）")
                .font(theme::subtitle_font())
                .color(theme::TEXT_TITLE),
        );
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !busy && !app.backend.is_demo(),
                    theme::btn_secondary("📁 选择项目文件夹复查"),
                )
                .clicked()
            {
                if let Some(p) = rfd::FileDialog::new()
                    .set_title("选择要复查的备份项目文件夹（备份盘「项目」目录下）")
                    .pick_folder()
                {
                    start_verify_one(app, p);
                }
            }
            if ui
                .add_enabled(
                    !busy && !app.backend.is_demo(),
                    theme::btn_secondary("📄 选择单个备份文件复查"),
                )
                .clicked()
            {
                if let Some(p) = rfd::FileDialog::new()
                    .set_title("选择要复查的备份文件")
                    .pick_file()
                {
                    start_verify_one(app, p);
                }
            }
        });
    });

    ui.add_space(theme::GAP);

    // ── 操作按钮行 ──
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            let start_btn = ui.add_enabled(
                !busy,
                theme::btn_primary(&app.operation_label("开始整盘复查")),
            );
            if start_btn.clicked() {
                start_verify(app);
            }

            if busy {
                ui.add_space(8.0);
                if app.task_running(crate::app::TaskKind::Verify) {
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
                        let cancel_btn = ui.add(theme::btn_danger("停止复查"));
                        if cancel_btn.clicked() {
                            if let Some(t) = &app.task {
                                t.request_cancel();
                            }
                        }
                        cancel_btn.on_hover_text("在当前项目/文件完成后安全停止");
                    }
                } else {
                    ui.label(app.busy_label());
                }
            }
        });
    });

    ui.add_space(theme::GAP);

    // ── 结果结构化呈现（已验证、损坏、缺失、不可验证、未检查） ──
    theme::card(ui, |ui| {
        theme::section_title(ui, "2. 完整性复查核验结果");
        ui.add_space(4.0);

        if let Some(stats) = app.verify_ui.last_stats {
            theme::card_grid(ui, 4, 4, |card, index| match index {
                0 => {
                    theme::kpi_card(
                        card,
                        "已检查文件 (Checked)",
                        &stats.checked.to_string(),
                        "已检查数（包含失败或仅大小项）",
                        if stats.checked > 0 {
                            theme::OK
                        } else {
                            theme::TEXT_MUTED
                        },
                    );
                }
                1 => {
                    theme::kpi_card(
                        card,
                        "完整性问题 (Bad)",
                        &stats.bad.to_string(),
                        if stats.bad > 0 {
                            "本次记录的完整性问题，详情见下方"
                        } else {
                            "本次已检查范围内记录为 0"
                        },
                        if stats.bad > 0 {
                            theme::DANGER
                        } else {
                            theme::TEXT_MUTED
                        },
                    );
                }
                2 => {
                    theme::kpi_card(
                        card,
                        "多余文件 (Extra)",
                        &stats.extra.to_string(),
                        if stats.extra > 0 {
                            "发现清单外多余文件"
                        } else {
                            "本次已枚举范围内记录为 0"
                        },
                        if stats.extra > 0 {
                            theme::WARN
                        } else {
                            theme::TEXT_MUTED
                        },
                    );
                }
                3 => {
                    theme::kpi_card(
                        card,
                        "仅大小校验 (Size-only)",
                        &stats.size_only.to_string(),
                        if stats.size_only > 0 {
                            "历史清单无哈希，仅核对大小"
                        } else {
                            "仅大小项记录为 0；不推定哈希覆盖"
                        },
                        if stats.size_only > 0 {
                            theme::WARN
                        } else {
                            theme::TEXT_MUTED
                        },
                    );
                }
                _ => unreachable!(),
            });

            ui.add_space(8.0);
            if let Some(report) = &app.verify_ui.last_report {
                let missing = report
                    .issues
                    .iter()
                    .filter(|i| i.kind == VerifyIssueKind::Missing)
                    .count();
                let unreadable = report
                    .issues
                    .iter()
                    .filter(|i| {
                        matches!(
                            i.kind,
                            VerifyIssueKind::ReadError | VerifyIssueKind::EnumError
                        )
                    })
                    .count();
                let unverifiable = report
                    .issues
                    .iter()
                    .filter(|i| i.kind == VerifyIssueKind::Unverifiable)
                    .count();
                ui.label(format!("问题明细：缺失 {missing} · 读取/枚举失败 {unreadable} · 不可验证 {unverifiable}"));
                if report.cancelled {
                    ui.colored_label(
                        theme::WARN,
                        "未检查：任务已取消，剩余数量未知；不推定未检查内容完好。",
                    );
                }
                if !report.issues.is_empty() {
                    egui::ScrollArea::both().max_height(180.0).show(ui, |ui| {
                        for issue in &report.issues {
                            ui.label(format!(
                                "{:?} · {} / {}",
                                issue.kind, issue.project, issue.rel
                            ));
                        }
                    });
                }
            }
            // 总体结论条
            if stats.cancelled {
                theme::callout_with_tag(
                    ui,
                    theme::TEXT_MUTED,
                    theme::TRACK,
                    "已取消",
                    "复查任务已中途安全停止，部分项目尚未完成核对。",
                );
            } else if stats.checked == 0 && stats.bad == 0 && stats.extra == 0 {
                // 重点：检查 0 项不显示为全部正常！
                theme::callout_with_tag(
                    ui,
                    theme::WARN,
                    theme::WARN_SOFT,
                    "未检查 / 无数据",
                    "本次复查未发现任何已归档的校验项（当前盘尚无备份项目或清单为空）。零检查数不代表数据完好。",
                );
            } else if stats.bad > 0 {
                theme::callout_with_tag(
                    ui,
                    theme::DANGER,
                    theme::DANGER_SOFT,
                    "发现完整性问题",
                    &format!("记录 {} 处完整性问题（含缺失、读取失败或无法验证项）。请根据明细核对；不能据此推定这些项全部损坏。", stats.bad),
                );
            } else if stats.extra > 0 {
                theme::callout_with_tag(
                    ui,
                    theme::WARN,
                    theme::WARN_SOFT,
                    "有多余文件",
                    &format!(
                        "本次枚举记录 {} 个清单外文件；未检查内容不作完整性结论。",
                        stats.extra
                    ),
                );
            } else if stats.size_only > 0 {
                theme::callout_with_tag(
                    ui,
                    theme::WARN,
                    theme::WARN_SOFT,
                    "仅大小校验",
                    &format!(
                        "存在 {} 项仅按文件大小核对（无历史 SHA256 哈希），未完整核对内容。",
                        stats.size_only
                    ),
                );
            } else {
                theme::callout_with_tag(
                    ui,
                    theme::OK,
                    theme::OK_SOFT,
                    "本次检查通过",
                    &format!(
                        "本次已检查 {} 项，报告未记录完整性问题；结论限于本次已检查范围。",
                        stats.checked
                    ),
                );
            }
            if stats.size_only > 0 && (stats.extra > 0 || stats.bad > 0 || stats.cancelled) {
                ui.colored_label(
                    theme::WARN,
                    format!("{} 项仅核对大小，未完整核对内容。", stats.size_only),
                );
            }
        } else if let Some(error) = &app.verify_ui.error {
            util::error_banner(
                ui,
                "复查失败",
                error,
                "请查看运行日志；失败不代表数据已验证。",
            );
        } else if let Some(s) = &app.verify_ui.summary {
            ui.label(
                egui::RichText::new(format!("最近一次复查结果：{s}"))
                    .size(12.5)
                    .color(theme::TEXT_BODY),
            );
        } else {
            util::empty_state(
                ui,
                "暂无复查结果",
                "选择上方备份盘或目标后点击「开始整盘复查」执行 SHA256 校验和核对。",
            );
        }
    });

    ui.add_space(theme::GAP);

    // ── 进度条与实时日志 ──
    theme::card(ui, |ui| {
        theme::section_title(ui, "复查进度与运行日志");
        ui.add_space(4.0);
        util::progress_bar(&app.progress, ui);
        util::log_panel(&app.logs, ui);
    });
}

fn rescan(app: &mut App) {
    if !app.backend.is_demo() {
        return;
    }
    match app.backend.drives() {
        Ok(ds) => app.verify_ui.drives = Some(ds),
        Err(e) => {
            app.verify_ui.drives = Some(Vec::new());
            app.logs
                .push((LogLevel::Error, format!("扫描盘失败：{:#}", e)));
        }
    }
}

fn plain_ui(app: &mut App, ui: &mut egui::Ui) {
    app.verify_ui
        .enforce_target(std::path::Path::new(&app.backup_ui.target_path));
    theme::page_header(
        ui,
        "数据完整性校验",
        "普通目标目录：从持久清单重算 SHA-256，不需要初始化磁盘。范围仅为所选目录内的备份任务。",
    );
    theme::card(ui, |ui| {
        theme::section_title(ui, "选择普通备份目标目录");
        ui.label(if app.backup_ui.target_path.is_empty() {
            "尚未选择目标目录"
        } else {
            &app.backup_ui.target_path
        });
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!app.is_busy(), theme::btn_secondary("选择目标目录"))
                .clicked()
            {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    crate::views::backup::set_target_folder(app, path);
                }
            }
            if ui
                .add_enabled(
                    !app.is_busy() && !app.backup_ui.target_path.is_empty(),
                    theme::btn_primary("校验所选目录"),
                )
                .clicked()
            {
                start_plain_verify(app, PathBuf::from(&app.backup_ui.target_path));
            }
            if app.task_running(crate::app::TaskKind::Verify) {
                let task = app.task.as_ref().unwrap();
                if ui
                    .add_enabled(!task.cancel_requested(), theme::btn_danger("停止复查"))
                    .clicked()
                {
                    task.request_cancel();
                }
            } else if app.is_busy() {
                ui.label(app.busy_label());
            }
        });
    });
    ui.add_space(theme::GAP);
    theme::card(ui, |ui| {
        theme::section_title(ui, "实际校验结果");
        if let Some(error) = &app.verify_ui.error {
            ui.colored_label(theme::DANGER, error);
        }
        if let Some(summary) = &app.verify_ui.summary {
            ui.label(summary);
        }
        if let Some(report) = &app.verify_ui.last_report {
            if report.cancelled {
                ui.colored_label(theme::WARN, "已取消：未检查的内容仍未知");
            }
            if report.checked == 0 {
                ui.colored_label(theme::WARN, "未检查到内容；不能推断完整性");
            }
            egui::ScrollArea::both().max_height(250.0).show(ui, |ui| {
                for issue in &report.issues {
                    ui.label(format!(
                        "{:?} · {} / {}",
                        issue.kind, issue.project, issue.rel
                    ));
                }
            });
        }
        util::progress_bar(&app.progress, ui);
        util::log_panel(&app.logs, ui);
    });
}

pub(crate) fn start_plain_verify(app: &mut App, target: PathBuf) {
    if app.backend.is_demo() || target.as_os_str().is_empty() || !app.ensure_idle() {
        return;
    }
    if target.as_path() != std::path::Path::new(&app.backup_ui.target_path) {
        return;
    }
    let token = app.verify_ui.begin_plain(target.clone());
    app.task_kind = Some(crate::app::TaskKind::Verify);
    app.task_started = Some(std::time::Instant::now());
    app.last_summary = None;
    let (tx, rx) = mpsc::channel();
    app.rx = Some(rx);
    let reporter = GuiReporter::new(tx, Arc::clone(&app.progress));
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        let result = bftool_core::service::verify_backup(&target, cancel, &reporter)
            .map_err(|error| format!("{error:#}"));
        Ok(crate::app::ExecutionResult::PlainVerification {
            target,
            token,
            result,
        })
    }));
}

fn start_verify(app: &mut App) {
    if !app.backend.is_demo() {
        start_plain_verify(app, PathBuf::from(&app.backup_ui.target_path));
        return;
    }
    if !app.ensure_idle() {
        return;
    }
    app.logs.clear();
    app.last_summary = None;
    app.verify_ui.last_stats = None;
    app.verify_ui.last_report = None;
    app.verify_ui.summary = None;
    app.verify_ui.error = None;
    app.task_kind = Some(crate::app::TaskKind::Verify);
    if let Ok(mut p) = app.progress.lock() {
        *p = ProgressState::default();
    }
    let (tx, rx) = mpsc::channel();
    let reporter = GuiReporter::new(tx, Arc::clone(&app.progress));
    app.rx = Some(rx);
    app.task_started = Some(std::time::Instant::now());
    let cfg = app.cfg.clone();
    let backend = app.backend;
    let sel = app.verify_ui.selected.clone();
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        Ok(crate::app::ExecutionResult::Verification(backend.verify(
            &cfg,
            sel.as_deref(),
            None,
            cancel,
            &reporter,
        )?))
    }));
}

fn start_verify_one(app: &mut App, target: PathBuf) {
    if !app.backend.is_demo() {
        start_plain_verify(app, target);
        return;
    }
    if !app.ensure_idle() {
        return;
    }
    app.logs.clear();
    app.last_summary = None;
    app.verify_ui.last_stats = None;
    app.verify_ui.last_report = None;
    app.verify_ui.summary = None;
    app.verify_ui.error = None;
    app.task_kind = Some(crate::app::TaskKind::Verify);
    if let Ok(mut p) = app.progress.lock() {
        *p = ProgressState::default();
    }
    let (tx, rx) = mpsc::channel();
    let reporter = GuiReporter::new(tx, Arc::clone(&app.progress));
    app.rx = Some(rx);
    app.task_started = Some(std::time::Instant::now());
    let cfg = app.cfg.clone();
    let backend = app.backend;
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        Ok(crate::app::ExecutionResult::Verification(backend.verify(
            &cfg,
            None,
            Some(&target),
            cancel,
            &reporter,
        )?))
    }));
}

/// 复查摘要文案。纯函数，可测。
pub fn verify_summary(
    checked: u64,
    bad: u64,
    extra: u64,
    size_only: u64,
    cancelled: bool,
) -> String {
    let mut s = format!("检查 {} · 完整性问题 {} · 多余 {}", checked, bad, extra);
    if size_only > 0 {
        s.push_str(&format!(" · 仅大小校验 {}", size_only));
    }
    if cancelled {
        s.push_str(" · 已取消");
    } else if checked == 0 && bad == 0 && extra == 0 {
        s.push_str(" · 无可校验项");
    } else if bad == 0 && size_only > 0 {
        s.push_str(" · 仅大小校验(未验证内容)");
    } else if bad == 0 && extra == 0 {
        s.push_str(" · 已检查范围完好");
    } else if bad == 0 {
        s.push_str(" · 有多余文件");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_summary_text() {
        assert!(verify_summary(10, 0, 0, 0, false).contains("完好"));
        let s = verify_summary(10, 2, 1, 0, false);
        assert!(s.contains("完整性问题 2"));
        assert!(s.contains("多余 1"));
        assert!(!s.contains("完好"), "有损坏不应显示完好");
        assert!(verify_summary(5, 0, 0, 0, true).contains("已取消"));
    }

    #[test]
    fn verify_summary_extra_only_is_not_clean() {
        let s = verify_summary(10, 0, 3, 0, false);
        assert!(!s.contains("完好"), "有多余文件不应显示完好;实际:{s}");
        assert!(s.contains("有多余文件"), "应提示有多余文件;实际:{s}");
    }

    #[test]
    fn verify_summary_zero_checked_is_not_clean() {
        let s = verify_summary(0, 0, 0, 0, false);
        assert!(!s.contains("完好"), "0 项被检查不应显示完好;实际:{s}");
        assert!(s.contains("无可校验项"), "应给中性提示;实际:{s}");
    }

    #[test]
    fn verify_summary_size_only_is_not_clean() {
        let s = verify_summary(10, 0, 0, 4, false);
        assert!(
            !s.contains("· 完好"),
            "含未验证内容的文件不应标完好;实际:{s}"
        );
        assert!(s.contains("仅大小校验"), "应提示仅大小校验;实际:{s}");
    }
}

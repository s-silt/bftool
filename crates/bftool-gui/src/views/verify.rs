//! 复查视图:选盘 → 后台 verify::run(重算 SHA256 比对清单)。唯一长任务,复用 archive 的后台机制。
//! 复查对盘**只读**(Spec D §4.4);逐项 issue 经 GuiReporter 进日志,摘要经 task 回传。(Spec D §3/§5)

use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use eframe::egui;

use bftool_core::engine::{drive, verify};
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::reporter::{GuiReporter, ProgressState};
use crate::task::BackgroundTask;
use crate::views::util;

/// 复查页跨帧状态。`selected=None` 表示"自动(单盘)"。
#[derive(Debug, Default)]
pub struct VerifyUiState {
    pub drives: Option<Vec<drive::DriveInfo>>,
    pub selected: Option<String>,
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    ui.heading("复查");
    ui.add_space(4.0);
    ui.label("重算备份盘上每个项目的 SHA256,与校验清单比对(对盘只读,封盘盘也能查)。");

    let busy = app.is_busy();

    if ui
        .add_enabled(!busy, egui::Button::new("🔄 刷新盘"))
        .clicked()
    {
        rescan(app);
    }
    if app.verify_ui.drives.is_none() {
        rescan(app);
    }

    // 盘选择(扁平渲染;运行中选择只影响下一次,无害)
    let mut sel = app.verify_ui.selected.clone();
    ui.horizontal(|ui| {
        if ui.selectable_label(sel.is_none(), "自动(单盘)").clicked() {
            sel = None;
        }
        if let Some(drives) = &app.verify_ui.drives {
            for d in drives {
                let on = sel.as_deref() == Some(d.letter.as_str());
                if ui
                    .selectable_label(on, format!("{} ({}:)", d.id, d.letter))
                    .clicked()
                {
                    sel = Some(d.letter.clone());
                }
            }
        }
    });
    app.verify_ui.selected = sel;

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!busy, egui::Button::new("✓ 开始复查"))
            .clicked()
        {
            start_verify(app);
        }
        if busy {
            let requested = app
                .task
                .as_ref()
                .map(|t| t.cancel_requested())
                .unwrap_or(false);
            if ui
                .add_enabled(!requested, egui::Button::new("✕ 取消"))
                .clicked()
            {
                if let Some(t) = &app.task {
                    t.request_cancel();
                }
            }
            if requested {
                ui.label("取消中…(当前项目完成后停止)");
            }
        }
    });

    // ── 单目标:只复查一个备份文件夹 / 一个文件的哈希 ──
    ui.add_space(6.0);
    ui.label("或只复查单个目标(到备份盘「项目」下选文件夹或文件):");
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!busy, egui::Button::new("📁 选文件夹复查"))
            .clicked()
        {
            if let Some(p) = rfd::FileDialog::new()
                .set_title("选择要复查的备份项目文件夹")
                .pick_folder()
            {
                start_verify_one(app, p);
            }
        }
        if ui
            .add_enabled(!busy, egui::Button::new("📄 选文件复查"))
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

    ui.separator();
    util::progress_bar(&app.progress, ui);
    if let Some(s) = &app.last_summary {
        ui.strong(format!("上次结果：{}", s));
        ui.separator();
    }
    ui.label("日志：");
    util::log_panel(&app.logs, ui);
}

fn rescan(app: &mut App) {
    match drive::scan_mounted(None) {
        Ok(ds) => app.verify_ui.drives = Some(ds),
        Err(e) => {
            app.verify_ui.drives = Some(Vec::new());
            app.logs
                .push((LogLevel::Error, format!("扫描盘失败：{:#}", e)));
        }
    }
}

fn start_verify(app: &mut App) {
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
    let sel = app.verify_ui.selected.clone();
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        let r = verify::run(&cfg, &reporter, sel.as_deref(), cancel)?;
        Ok(verify_summary(
            r.checked,
            r.bad,
            r.extra,
            r.size_only,
            r.cancelled,
        ))
    }));
}

/// 后台复查单个目标(文件夹/文件)的哈希。core 自动定位所在盘 + 项目。
fn start_verify_one(app: &mut App, target: PathBuf) {
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
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        let r = verify::verify_one(&cfg, &reporter, &target, cancel)?;
        Ok(verify_summary(
            r.checked,
            r.bad,
            r.extra,
            r.size_only,
            r.cancelled,
        ))
    }));
}

/// 复查摘要文案。纯函数,可测。
/// 结论与 core `VerifyOutcome`/`outcome_label` 对齐:取消 > 损坏 > 多余 > 完好。
/// extra-only(无损坏但有清单外多余文件)是**警示态**,不能标"完好"。(BF-VERIFY-SUMMARY-EXTRA)
fn verify_summary(checked: u64, bad: u64, extra: u64, size_only: u64, cancelled: bool) -> String {
    let mut s = format!("检查 {} · 损坏/缺失 {} · 多余 {}", checked, bad, extra);
    if size_only > 0 {
        s.push_str(&format!(" · 仅大小校验 {}", size_only));
    }
    if cancelled {
        s.push_str(" · 已取消");
    } else if checked == 0 && bad == 0 && extra == 0 {
        // 一项都没校验 ≠ 校验通过:空校验(该盘/项目尚无已归档内容、或清单为空)不能标「完好」,
        // 否则把「什么都没查」呈现成「内容完整」。(review-r3 round4)
        s.push_str(" · 无可校验项");
    } else if bad == 0 && size_only > 0 {
        // 有文件只校验了大小、未验证内容(清单无哈希)→ 不能笼统标「完好」。(review-r3 round4)
        s.push_str(" · 仅大小校验(未验证内容)");
    } else if bad == 0 && extra == 0 {
        s.push_str(" · 完好");
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
        assert!(s.contains("损坏/缺失 2"));
        assert!(s.contains("多余 1"));
        assert!(!s.contains("完好"), "有损坏不应显示完好");
        assert!(verify_summary(5, 0, 0, 0, true).contains("已取消"));
    }

    // ── BF-VERIFY-SUMMARY-EXTRA: extra-only(无损坏但有多余文件)不能标"完好" ──
    #[test]
    fn verify_summary_extra_only_is_not_clean() {
        let s = verify_summary(10, 0, 3, 0, false);
        assert!(!s.contains("完好"), "有多余文件不应显示完好;实际:{s}");
        assert!(s.contains("有多余文件"), "应提示有多余文件;实际:{s}");
    }

    // ── review-r3 round4:检查 0 项不能标「完好」(空校验 ≠ 校验通过)──
    #[test]
    fn verify_summary_zero_checked_is_not_clean() {
        let s = verify_summary(0, 0, 0, 0, false);
        assert!(!s.contains("完好"), "0 项被检查不应显示完好;实际:{s}");
        assert!(s.contains("无可校验项"), "应给中性提示;实际:{s}");
    }

    // ── review-r3 round4:有「仅大小校验(无哈希)」文件时不能标「完好」,须提示未验证内容 ──
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

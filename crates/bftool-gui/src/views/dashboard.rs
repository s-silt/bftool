//! 仪表盘视图(默认):消费 `status::gather` —— 当前盘 / 待备份数 / 上次复查 + 三大入口按钮。
//! 首次无配置或无盘 → 顶部横幅引导去设置/初始化。(Spec D §5)

use eframe::egui;

use bftool_core::engine::status;
use bftool_core::engine::verify::VerifyOutcome;

use crate::app::{App, View};
use crate::views::util;

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    ui.heading("仪表盘");
    ui.add_space(4.0);

    let report = match status::gather(&app.cfg) {
        Ok(r) => r,
        Err(e) => {
            ui.colored_label(
                egui::Color32::from_rgb(0xCC, 0x33, 0x33),
                format!("读取状态失败：{:#}", e),
            );
            return;
        }
    };

    // 顶部引导横幅:没有任何在线盘 → 提示去初始化。
    if report.drives.is_empty() {
        banner(
            ui,
            egui::Color32::from_rgb(0x8a, 0x6d, 0x00),
            "还没有在线的备份盘。插入一块空盘 → 到「初始化新盘」把它做成「备份N」。",
        );
    }

    egui::Grid::new("roots")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            row(ui, "待备份(源)", &report.ready_root.display().to_string());
            row(ui, "已备份", &report.archived_root.display().to_string());
            row(ui, "备份系统", &report.system_root.display().to_string());
            row(
                ui,
                "参数",
                &format!(
                    "预留 {} GB · 稳定期 {} 分 · 认盘最小 {} GB",
                    report.reserve_gb, report.stable_minutes, report.min_drive_gb
                ),
            );
            row(ui, "待归档项目数", &report.pending_count.to_string());
        });

    ui.separator();
    ui.label("在线机械盘：");
    if report.drives.is_empty() {
        ui.label("  无");
    } else {
        for ds in &report.drives {
            let d = &ds.drive;
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!("{} ({}:)", d.id, d.letter));
                    ui.label(if d.sealed { "已封盘" } else { "可用" });
                });
                ui.label(format!(
                    "剩余 {} / 共 {}",
                    util::fmt_gb(d.free_bytes),
                    util::fmt_gb(d.total_bytes)
                ));
                let lv = match &ds.last_verify {
                    Some(v) => format!(
                        "上次复查：{}（{}）",
                        short_date(&v.when),
                        outcome_label(&v.status)
                    ),
                    None => "上次复查：未知".to_string(),
                };
                ui.label(lv);
            });
        }
    }

    if report.txn_pending {
        banner(
            ui,
            egui::Color32::from_rgb(0x8a, 0x6d, 0x00),
            "发现上次未完成的事务残留 —— 下次运行备份/复查时会自动自检核对。",
        );
    }

    ui.separator();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("▶  备份").clicked() {
            app.view = View::Archive;
        }
        if ui.button("✓  复查").clicked() {
            app.view = View::Verify;
        }
        if ui.button("＋  初始化新盘").clicked() {
            app.view = View::Init;
        }
    });

    // 顺手把"配置来源"显示出来,方便用户确认当前生效的是哪份配置。
    ui.add_space(6.0);
    ui.weak(util::source_hint(&app.config_source));
}

fn banner(ui: &mut egui::Ui, color: egui::Color32, text: &str) {
    ui.add_space(2.0);
    ui.colored_label(color, format!("⚠  {}", text));
    ui.add_space(2.0);
}

fn row(ui: &mut egui::Ui, k: &str, v: &str) {
    ui.label(k);
    ui.monospace(v);
    ui.end_row();
}

/// RFC3339 取日期部分（够仪表盘用）。
fn short_date(rfc3339: &str) -> &str {
    rfc3339.split('T').next().unwrap_or(rfc3339)
}

fn outcome_label(o: &VerifyOutcome) -> &'static str {
    match o {
        VerifyOutcome::Clean => "完好",
        VerifyOutcome::IssuesFound { .. } => "发现损坏!",
        VerifyOutcome::ExtraOnly { .. } => "有多余文件",
        VerifyOutcome::Cancelled => "上次取消",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_date_takes_date_part() {
        assert_eq!(short_date("2026-05-29T12:00:00+00:00"), "2026-05-29");
        assert_eq!(short_date("no-t-here"), "no-t-here");
    }

    #[test]
    fn outcome_labels_distinct() {
        assert_eq!(outcome_label(&VerifyOutcome::Clean), "完好");
        assert_eq!(
            outcome_label(&VerifyOutcome::IssuesFound { bad: 2 }),
            "发现损坏!"
        );
        assert_eq!(
            outcome_label(&VerifyOutcome::ExtraOnly { extra: 1 }),
            "有多余文件"
        );
        assert_eq!(outcome_label(&VerifyOutcome::Cancelled), "上次取消");
    }
}

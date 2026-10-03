//! 仪表盘视图: 突出当前状态、待备份项目、可用容量、最近复查结果，以及“备份／复查／初始化”入口。
//! 未复查、无数据、失败绝不展示绿色健康。

use eframe::egui::{self, Color32};

use bftool_core::engine::status::StatusReport;
use bftool_core::engine::verify::VerifyOutcome;

use crate::app::{App, View};
use crate::views::{theme, util};

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    top_bar(app, ui);
    ui.add_space(theme::GAP);
    progress_card(app, ui);

    const CACHE_TTL_MS: u128 = 1500;
    let report = {
        let now = std::time::Instant::now();
        let stale = app
            .status_cache
            .as_ref()
            .map(|(_, t)| t.elapsed().as_millis() > CACHE_TTL_MS)
            .unwrap_or(true);
        if stale {
            match app.backend.status(&app.cfg) {
                Ok(r) => {
                    app.status_cache = Some((r.clone(), now));
                    Some(r)
                }
                Err(e) => {
                    theme::callout_with_tag(
                        ui,
                        theme::DANGER,
                        theme::DANGER_SOFT,
                        "状态异常",
                        &format!("读取状态失败：{:#}", e),
                    );
                    None
                }
            }
        } else {
            Some(app.status_cache.as_ref().unwrap().0.clone())
        }
    };

    let Some(report) = report else {
        ui.add_space(theme::GAP);
        actions_row(app, ui);
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(util::source_hint(&app.config_source))
                .size(11.0)
                .color(theme::TEXT_MUTED),
        );
        return;
    };

    egui::ScrollArea::vertical()
        .id_salt("dashboard_scroll")
        .show(ui, |ui| {
            kpi_row(ui, &report);
            ui.add_space(theme::GAP);

            charts_row(ui, &report);
            ui.add_space(theme::GAP);

            if report.drives.is_empty() {
                theme::callout_with_tag(
                    ui,
                    theme::WARN,
                    theme::WARN_SOFT,
                    "无备份盘",
                    "当前未检测到在线的备份盘。请插入已初始化的备份盘，或进入「初始化新盘」准备新磁盘。",
                );
                ui.add_space(8.0);
            }
            if report.txn_pending {
                theme::callout_with_tag(
                    ui,
                    theme::WARN,
                    theme::WARN_SOFT,
                    "待恢复事务",
                    "发现上次未完成的事务日志 —— 下次运行备份或复查时将自动自检核验，保障数据安全。",
                );
                ui.add_space(8.0);
            }

            // 主次分明的快捷操作区
            theme::card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        theme::section_title(ui, "核心操作入口");
                        ui.label(
                            egui::RichText::new("推荐日常流程：先检查待备份项目，插入备份盘后执行备份归档。")
                                .size(11.5)
                                .color(theme::TEXT_MUTED),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        actions_row(app, ui);
                    });
                });
            });

            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(util::source_hint(&app.config_source))
                    .size(11.0)
                    .color(theme::TEXT_MUTED),
            );
        });
}

fn top_bar(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new("仪表盘 · 总览")
                    .font(theme::h1_font())
                    .color(theme::TEXT_TITLE),
            );
            ui.label(
                egui::RichText::new("实时监控待归档数据、在线磁盘容量与完整性复查状态")
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut app.find_ui.keyword)
                    .hint_text("快速搜索已备份项目… (回车)")
                    .desired_width(200.0),
            );
            if resp.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && !app.find_ui.keyword.trim().is_empty()
                && !app.is_busy()
            {
                app.find_ui.result = None;
                app.find_ui.error = None;
                app.view = View::Find;
            }
        });
    });
}

/// 进度卡片：仅当 core 正在上报字节进度时展示真实进度
fn progress_card(app: &mut App, ui: &mut egui::Ui) {
    let snap = app
        .progress
        .lock()
        .ok()
        .filter(|p| p.active && p.total > 0)
        .map(|p| (p.label.clone(), p.current, p.total));
    let Some((phase, cur, total)) = snap else {
        return;
    };
    let frac = cur as f32 / total as f32;
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(14.0).color(theme::PRIMARY));
            ui.add_space(6.0);
            theme::badge(ui, "任务进行中", theme::PRIMARY, Color32::WHITE);
            ui.add_space(4.0);
            ui.colored_label(theme::TEXT_TITLE, format!("{phase} 正在进行…"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.colored_label(theme::PRIMARY, format!("{:.1}%", frac * 100.0));
            });
        });
        ui.add_space(6.0);
        theme::hbar(ui, frac, theme::PRIMARY);
        ui.add_space(4.0);
        ui.colored_label(
            theme::TEXT_MUTED,
            format!(
                "已校验 {} / 总计 {} · 阶段：{phase}",
                util::fmt_gb(cur),
                util::fmt_gb(total)
            ),
        );
    });
    ui.add_space(theme::GAP);
}

fn kpi_row(ui: &mut egui::Ui, report: &StatusReport) {
    let online = report.drives.len();
    let usable = report.drives.iter().filter(|d| !d.drive.sealed).count();
    let sealed = online - usable;
    let (used, total) = report.drives.iter().fold((0u64, 0u64), |(u, t), d| {
        (
            u + d.drive.total_bytes.saturating_sub(d.drive.free_bytes),
            t + d.drive.total_bytes,
        )
    });
    let pct = if total > 0 {
        used as f32 / total as f32 * 100.0
    } else {
        0.0
    };
    let (vval, vsub, vcolor) = latest_verify(report);

    theme::card_grid(ui, 4, 4, |card, index| match index {
        0 => {
            theme::kpi_card(
                card,
                "待备份项目",
                &report.pending_count.to_string(),
                if report.pending_count > 0 {
                    "个目录待安全归档"
                } else {
                    "源目录目前已清空"
                },
                if report.pending_count > 0 {
                    theme::PRIMARY
                } else {
                    theme::TEXT_MUTED
                },
            );
        }
        1 => {
            theme::kpi_card(
                card,
                "在线备份盘",
                &online.to_string(),
                &format!("可用 {usable} · 封盘 {sealed}"),
                if online > 0 {
                    theme::PRIMARY
                } else {
                    theme::WARN
                },
            );
        }
        2 => {
            theme::kpi_card(
                card,
                "可用容量情况",
                &format!("{pct:.0}% 已用"),
                &format!(
                    "剩余 {} / 共 {}",
                    util::fmt_gb(total.saturating_sub(used)),
                    util::fmt_gb(total)
                ),
                if pct > 90.0 { theme::WARN } else { theme::CYAN },
            );
        }
        3 => {
            theme::kpi_card(card, "最近复查结果", &vval, &vsub, vcolor);
        }
        _ => unreachable!(),
    });
}

/// 复查结论 → (标签, 颜色)。纯函数，覆盖 core 权威状态
pub fn outcome_label(o: &VerifyOutcome) -> (&'static str, Color32) {
    let color = match o {
        VerifyOutcome::Clean => theme::OK,
        VerifyOutcome::IssuesFound { .. } => theme::DANGER,
        VerifyOutcome::ExtraOnly { .. } => theme::WARN,
        VerifyOutcome::CleanButSizeOnly { .. } => theme::WARN,
        VerifyOutcome::Cancelled => theme::TEXT_MUTED,
    };
    (o.label(), color)
}

fn latest_verify(report: &StatusReport) -> (String, String, Color32) {
    if report.drives.is_empty() {
        return (
            "无在线盘".into(),
            "暂未连接备份盘".into(),
            theme::TEXT_MUTED,
        );
    }
    let latest = report
        .drives
        .iter()
        .filter_map(|d| d.last_verify.as_ref())
        .max_by(|a, b| a.when.cmp(&b.when));
    match latest {
        Some(v) => {
            let (label, color) = outcome_label(&v.status);
            let date = v.when.split('T').next().unwrap_or(&v.when).to_string();
            (label.to_string(), format!("复查日期：{date}"), color)
        }
        None => ("未复查".into(), "建议执行完整性复查".into(), theme::WARN),
    }
}

fn charts_row(ui: &mut egui::Ui, report: &StatusReport) {
    theme::card_grid(ui, 2, 2, |card, index| match index {
        0 => {
            theme::card(card, |ui| {
                theme::section_title(ui, "磁盘容量使用率");
                ui.add_space(8.0);
                if report.drives.is_empty() {
                    ui.colored_label(theme::TEXT_MUTED, "暂无已挂载的备份盘");
                } else {
                    for ds in &report.drives {
                        let d = &ds.drive;
                        let used = d.total_bytes.saturating_sub(d.free_bytes);
                        let frac = if d.total_bytes > 0 {
                            used as f32 / d.total_bytes as f32
                        } else {
                            0.0
                        };
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("{} ({}:)", d.id, d.letter))
                                    .font(theme::subtitle_font())
                                    .color(theme::TEXT_TITLE),
                            );
                            if d.sealed {
                                theme::badge(ui, "已封盘", theme::TRACK, theme::TEXT_MUTED);
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.colored_label(
                                        theme::TEXT_MUTED,
                                        format!(
                                            "可用 {} / 总共 {}",
                                            util::fmt_gb(d.free_bytes),
                                            util::fmt_gb(d.total_bytes)
                                        ),
                                    );
                                },
                            );
                        });
                        ui.add_space(3.0);
                        theme::hbar(
                            ui,
                            frac,
                            if frac > 0.9 {
                                theme::WARN
                            } else {
                                theme::PRIMARY
                            },
                        );
                        ui.add_space(10.0);
                    }
                }
            });
        }
        1 => {
            theme::card(card, |ui| {
                theme::section_title(ui, "数据完整性复查健康度");
                ui.add_space(8.0);
                let verified: Vec<_> = report
                    .drives
                    .iter()
                    .filter_map(|d| d.last_verify.as_ref())
                    .collect();

                if report.drives.is_empty() {
                    ui.vertical_centered(|ui| {
                        theme::ring(ui, 0.0, theme::TEXT_MUTED, "无数据", "未连接备份盘", 130.0);
                        ui.add_space(6.0);
                        ui.colored_label(theme::TEXT_MUTED, "请先接入或初始化备份盘");
                    });
                } else if verified.is_empty() {
                    // 重点：未复查不能展示绿色健康！显示灰色/橙色与未复查文字
                    ui.vertical_centered(|ui| {
                        theme::ring(ui, 0.0, theme::WARN, "未复查", "尚无记录", 130.0);
                        ui.add_space(6.0);
                        ui.colored_label(theme::WARN, "建议定期执行复查比对 SHA256 校验和");
                    });
                } else {
                    let clean = verified
                        .iter()
                        .filter(|v| matches!(v.status, VerifyOutcome::Clean))
                        .count();
                    let issues = verified
                        .iter()
                        .filter(|v| matches!(v.status, VerifyOutcome::IssuesFound { .. }))
                        .count();
                    let tot = report.drives.len(); // 统计所有在线盘
                    let frac = clean as f32 / tot as f32;

                    let ring_color = if issues > 0 {
                        theme::DANGER
                    } else if clean == tot {
                        theme::OK
                    } else {
                        theme::WARN
                    };

                    ui.vertical_centered(|ui| {
                        theme::ring(
                            ui,
                            frac,
                            ring_color,
                            &format!("{:.0}%", frac * 100.0),
                            if issues > 0 {
                                "有损坏"
                            } else if clean == tot {
                                "完好"
                            } else {
                                "部分未查"
                            },
                            130.0,
                        );
                        ui.add_space(6.0);
                        let info_text = format!("{clean} / {tot} 块在线盘经校验完好");
                        ui.colored_label(theme::TEXT_BODY, info_text);
                    });
                }
            });
        }
        _ => unreachable!(),
    });
}

fn actions_row(app: &mut App, ui: &mut egui::Ui) {
    let busy = app.is_busy();
    ui.horizontal(|ui| {
        let primary_btn = ui.add_enabled(!busy, theme::btn_primary("▶ 备份归档"));
        if primary_btn.clicked() {
            app.view = View::Archive;
        }

        let verify_btn = ui.add_enabled(!busy, theme::btn_secondary("✓ 完整性复查"));
        if verify_btn.clicked() {
            app.view = View::Verify;
        }

        let init_btn = ui.add_enabled(!busy, theme::btn_secondary("＋ 初始化新盘"));
        if init_btn.clicked() {
            app.view = View::Init;
        }

        if busy {
            ui.add_space(4.0);
            ui.colored_label(theme::TEXT_MUTED, "(任务正在运行中)");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_label_maps_all_variants() {
        assert_eq!(outcome_label(&VerifyOutcome::Clean).0, "完好");
        assert_eq!(
            outcome_label(&VerifyOutcome::IssuesFound { bad: 2 }).0,
            "发现完整性问题"
        );
        assert_eq!(
            outcome_label(&VerifyOutcome::ExtraOnly { extra: 1 }).0,
            "有多余文件"
        );
        let (lbl, color) = outcome_label(&VerifyOutcome::CleanButSizeOnly { size_only: 3 });
        assert!(
            lbl.contains("仅大小校验") && !lbl.contains("完好"),
            "size-only 不应标完好"
        );
        assert_eq!(color, theme::WARN, "size-only 应为 WARN 色,非绿色 OK");
        assert_eq!(outcome_label(&VerifyOutcome::Cancelled).0, "上次取消");
    }
}

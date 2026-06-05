//! 仪表盘:顶部栏 + 忠实进度卡 + KPI 行 + 图表网格 + 引导 callout。浅色扁平科技风。
//! 进度卡只反映 core 上报的 SHA256 校验/枚举各遍(复制阶段 core 无进度 → 自然隐藏,不伪造)。

use eframe::egui::{self, Color32};

use bftool_core::engine::status::{self, StatusReport};
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
            // GUI 仪表盘每 1500ms 刷新一次:复查记录损坏的 warn 走 NoopReporter 丢弃,避免每次刷新刷屏。(review-r3 round4)
            match status::gather(&app.cfg, &bftool_core::reporter::NoopReporter) {
                Ok(r) => {
                    app.status_cache = Some((r.clone(), now));
                    Some(r)
                }
                // R-03:gather 失败时不直接 return——否则用户连"去设置/初始化"的入口都看不到,
                // 卡死在仪表盘。改为显示 error callout 后仍渲染操作按钮,让用户能去其他页修配置。
                Err(e) => {
                    theme::callout(
                        ui,
                        theme::DANGER,
                        theme::DANGER_SOFT,
                        &format!("读取状态失败：{:#}", e),
                    );
                    None
                }
            }
        } else {
            Some(app.status_cache.as_ref().unwrap().0.clone())
        }
    };

    // gather 失败:仍渲染操作按钮 + 来源提示,让用户能跳去设置/初始化页修配置。
    let Some(report) = report else {
        ui.add_space(theme::GAP);
        actions_row(app, ui);
        ui.add_space(6.0);
        ui.weak(util::source_hint(&app.config_source));
        return;
    };

    egui::ScrollArea::vertical().show(ui, |ui| {
        kpi_row(ui, &report);
        ui.add_space(theme::GAP);
        charts_row(ui, &report);
        ui.add_space(theme::GAP);
        if report.drives.is_empty() {
            theme::callout(
                ui,
                theme::WARN,
                theme::WARN_SOFT,
                "还没有在线的备份盘。插入一块空盘 → 到「初始化新盘」把它做成「备份N」。",
            );
            ui.add_space(8.0);
        }
        if report.txn_pending {
            theme::callout(
                ui,
                theme::WARN,
                theme::WARN_SOFT,
                "发现上次未完成的事务残留 —— 下次运行备份/复查时会自动自检核对。",
            );
            ui.add_space(8.0);
        }
        actions_row(app, ui);
        ui.add_space(6.0);
        ui.weak(util::source_hint(&app.config_source));
    });
}

fn top_bar(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("归档备份 · 总览")
                .font(egui::FontId::new(
                    20.0,
                    egui::FontFamily::Name("semibold".into()),
                ))
                .color(theme::TEXT_TITLE),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 头像
            let (r, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(r.center(), 14.0, theme::PRIMARY_SOFT);
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                "我",
                egui::FontId::proportional(12.0),
                theme::PRIMARY,
            );
            ui.add_space(8.0);
            // 搜索框:回车 → 跳查找页
            let resp = ui.add(
                egui::TextEdit::singleline(&mut app.find_ui.keyword)
                    .hint_text("搜索项目…")
                    .desired_width(180.0),
            );
            if resp.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                && !app.find_ui.keyword.trim().is_empty()
                && !app.is_busy()
            // R5-6:任务进行中不跳转(与各页 busy 禁用一致,避免绕过)
            {
                // R4-7:跳转前清掉上次查找的旧结果/错误,避免查找页显示与当前关键词无关的过期结果。
                app.find_ui.result = None;
                app.find_ui.error = None;
                app.view = View::Find;
            }
        });
    });
}

/// 忠实进度卡:仅当 core 正在上报字节进度(某一遍校验/枚举)时显示。
/// 复制阶段 core 无进度事件 → `active=false` → 隐藏(不伪造"已传/总量/ETA")。
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
            ui.colored_label(theme::TEXT_TITLE, format!("{phase}中…"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.colored_label(theme::PRIMARY, format!("{:.0}%", frac * 100.0));
            });
        });
        ui.add_space(8.0);
        theme::hbar(ui, frac, theme::PRIMARY);
        ui.add_space(6.0);
        ui.colored_label(
            theme::TEXT_MUTED,
            format!(
                "{} / {} · 当前阶段({phase})",
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
    ui.columns(4, |c| {
        theme::kpi_card(
            &mut c[0],
            "待备份项目",
            &report.pending_count.to_string(),
            "个文件夹待归档",
            theme::PRIMARY,
        );
        theme::kpi_card(
            &mut c[1],
            "在线备份盘",
            &online.to_string(),
            &format!("可用 {usable} · 封盘 {sealed}"),
            theme::VIOLET,
        );
        theme::kpi_card(
            &mut c[2],
            "容量已用",
            &format!("{pct:.0}%"),
            &format!("{} / {}", util::fmt_gb(used), util::fmt_gb(total)),
            theme::CYAN,
        );
        theme::kpi_card(&mut c[3], "最近复查", &vval, &vsub, vcolor);
    });
}

/// 复查结论 → (标签, 颜色)。纯函数,可测。
fn outcome_label(o: &VerifyOutcome) -> (&'static str, Color32) {
    // 强优化:文案统一用 core 的权威 VerifyOutcome::label();此处只决定配色(UI 关注点)。
    let color = match o {
        VerifyOutcome::Clean => theme::OK,
        VerifyOutcome::IssuesFound { .. } => theme::DANGER,
        VerifyOutcome::ExtraOnly { .. } => theme::WARN,
        // 仅大小校验(清单无哈希)未验证内容 → WARN 色,不绿标完好。(review-r3 round5)
        VerifyOutcome::CleanButSizeOnly { .. } => theme::WARN,
        VerifyOutcome::Cancelled => theme::TEXT_BODY,
    };
    (o.label(), color)
}

fn latest_verify(report: &StatusReport) -> (String, String, Color32) {
    let latest = report
        .drives
        .iter()
        .filter_map(|d| d.last_verify.as_ref())
        .max_by(|a, b| a.when.cmp(&b.when));
    match latest {
        Some(v) => {
            let (label, color) = outcome_label(&v.status);
            let date = v.when.split('T').next().unwrap_or(&v.when).to_string();
            (label.to_string(), date, color)
        }
        None => (
            "未复查".into(),
            "建议每 6–12 个月一次".into(),
            theme::TEXT_MUTED,
        ),
    }
}

fn charts_row(ui: &mut egui::Ui, report: &StatusReport) {
    ui.columns(2, |c| {
        theme::card(&mut c[0], |ui| {
            ui.label(
                egui::RichText::new("每盘容量")
                    .font(theme::title_font())
                    .color(theme::TEXT_TITLE),
            );
            ui.add_space(12.0);
            if report.drives.is_empty() {
                ui.colored_label(theme::TEXT_MUTED, "暂无在线盘");
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
                        ui.colored_label(theme::TEXT_TITLE, format!("{} ({}:)", d.id, d.letter));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.colored_label(
                                theme::TEXT_MUTED,
                                format!(
                                    "剩余 {} / {}",
                                    util::fmt_gb(d.free_bytes),
                                    util::fmt_gb(d.total_bytes)
                                ),
                            );
                        });
                    });
                    ui.add_space(4.0);
                    theme::hbar(
                        ui,
                        frac,
                        if frac > 0.9 {
                            theme::WARN
                        } else {
                            theme::PRIMARY
                        },
                    );
                    ui.add_space(14.0);
                }
            }
        });
        theme::card(&mut c[1], |ui| {
            ui.label(
                egui::RichText::new("复查健康")
                    .font(theme::title_font())
                    .color(theme::TEXT_TITLE),
            );
            ui.add_space(8.0);
            let verified: Vec<_> = report
                .drives
                .iter()
                .filter_map(|d| d.last_verify.as_ref())
                .collect();
            if verified.is_empty() {
                ui.colored_label(theme::TEXT_MUTED, "暂无复查记录");
            } else {
                let clean = verified
                    .iter()
                    .filter(|v| matches!(v.status, VerifyOutcome::Clean))
                    .count();
                let tot = verified.len();
                let frac = clean as f32 / tot as f32;
                ui.vertical_centered(|ui| {
                    theme::ring(
                        ui,
                        frac,
                        theme::OK,
                        &format!("{:.0}%", frac * 100.0),
                        "完好",
                        140.0,
                    );
                    ui.add_space(8.0);
                    ui.colored_label(theme::TEXT_BODY, format!("{clean} / {tot} 盘完好"));
                });
            }
        });
    });
}

fn actions_row(app: &mut App, ui: &mut egui::Ui) {
    ui.add_enabled_ui(!app.is_busy(), |ui| {
        ui.horizontal(|ui| {
            let primary =
                egui::Button::new(egui::RichText::new("▶  备份").color(egui::Color32::WHITE))
                    .fill(theme::PRIMARY);
            if ui.add(primary).clicked() {
                app.view = View::Archive;
            }
            if ui.button("✓  复查").clicked() {
                app.view = View::Verify;
            }
            if ui.button("＋  初始化新盘").clicked() {
                app.view = View::Init;
            }
        });
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
            "发现损坏"
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

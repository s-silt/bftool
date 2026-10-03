//! 盘列表视图: 容量、封盘状态、最近复查和离线状态统一呈现。

use eframe::egui;

use bftool_core::engine::verify::VerifyOutcome;
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::views::{theme, util};

pub use crate::backend::OfflineDriveInfo;

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    theme::page_header(
        ui,
        "备份盘全景列表",
        "统一监控全部在线备份盘的容量占用、封盘状态、最近完整性复查结果，以及历史离线盘档案。",
    );

    let busy = app.is_busy();

    // ── 顶部操作栏 ──
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            theme::section_title(ui, "磁盘状态概览");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(!busy, theme::btn_secondary("🔄 刷新磁盘"))
                    .clicked()
                {
                    rescan(app);
                }
            });
        });

        if app.drives_cache.is_none() {
            rescan(app);
        }

        if let Some((ok, msg)) = &app.drives_result {
            ui.add_space(4.0);
            if *ok {
                ui.colored_label(theme::TEXT_MUTED, msg);
            } else {
                ui.colored_label(theme::DANGER, msg);
            }
        }
    });

    ui.add_space(theme::GAP);

    // ── 在线盘与离线盘列表 ──
    theme::card(ui, |ui| {
        let online_drives = app.drives_cache.as_ref();
        let offline_drives = app
            .backend
            .offline_drives(&app.cfg, online_drives.map(Vec::as_slice).unwrap_or(&[]));

        let has_any =
            online_drives.map(|d| !d.is_empty()).unwrap_or(false) || !offline_drives.is_empty();

        if !has_any {
            util::empty_state(
                ui,
                "未发现任何备份盘",
                "当前未识别到已连接的备份盘。插入已初始化的备份硬盘，或前往「初始化新盘」将新盘加入归档体系。",
            );
            return;
        }

        egui::ScrollArea::both()
            .auto_shrink([false, true])
            .max_height(480.0)
            .id_salt("drives_scroll_table")
            .show(ui, |ui| {
                egui::Grid::new("drives_unified_grid")
                    .num_columns(6)
                    .striped(true)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.strong("连接状态");
                        ui.strong("盘编号与挂载点");
                        ui.strong("归档封盘状态");
                        ui.strong("容量使用率");
                        ui.strong("剩余 / 总容量");
                        ui.strong("最近完整性复查");
                        ui.end_row();

                        // 渲染在线盘
                        if let Some(drives) = online_drives {
                            for d in drives {
                                // 1. 连接状态
                                ui.horizontal(|ui| {
                                    let (r, _) = ui.allocate_exact_size(
                                        egui::vec2(8.0, 8.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().circle_filled(r.center(), 3.5, theme::OK);
                                    ui.add_space(2.0);
                                    ui.label(
                                        egui::RichText::new("在线").color(theme::OK).size(12.0),
                                    );
                                });

                                // 2. 盘编号与挂载点
                                ui.label(
                                    egui::RichText::new(format!("{} ({}:)", d.id, d.letter))
                                        .font(theme::subtitle_font())
                                        .color(theme::TEXT_TITLE),
                                );

                                // 3. 封盘状态
                                if d.sealed {
                                    theme::badge(ui, "已封盘", theme::TRACK, theme::TEXT_MUTED);
                                } else {
                                    theme::badge(ui, "可用可写", theme::OK_SOFT, theme::OK);
                                }

                                // 4. 容量条
                                let used = d.total_bytes.saturating_sub(d.free_bytes);
                                let frac = if d.total_bytes > 0 {
                                    used as f32 / d.total_bytes as f32
                                } else {
                                    0.0
                                };
                                ui.horizontal(|ui| {
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(110.0, 6.0),
                                        egui::Sense::hover(),
                                    );
                                    let p = ui.painter();
                                    p.rect_filled(rect, egui::CornerRadius::same(3), theme::TRACK);
                                    if frac > 0.0 {
                                        let mut fill = rect;
                                        fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
                                        p.rect_filled(
                                            fill,
                                            egui::CornerRadius::same(3),
                                            if frac > 0.9 {
                                                theme::WARN
                                            } else {
                                                theme::PRIMARY
                                            },
                                        );
                                    }
                                    ui.add_space(4.0);
                                    ui.label(format!("{:.0}%", frac * 100.0));
                                });

                                // 5. 剩余 / 总容量
                                ui.label(format!(
                                    "剩余 {} / 共 {}",
                                    util::fmt_gb(d.free_bytes),
                                    util::fmt_gb(d.total_bytes)
                                ));

                                // 6. 最近复查
                                let verify_info = app
                                    .status_cache
                                    .as_ref()
                                    .and_then(|(r, _)| {
                                        r.drives.iter().find(|ds| ds.drive.id == d.id)
                                    })
                                    .and_then(|ds| ds.last_verify.as_ref())
                                    .map(|v| (v.status.clone(), v.when.clone()))
                                    .or_else(|| {
                                        app.backend
                                            .last_verify(&app.cfg, &d.id)
                                            .ok()
                                            .flatten()
                                            .map(|v| (v.status, v.when))
                                    });
                                match verify_info {
                                    Some((status, when)) => {
                                        let (tag_bg, tag_fg) = match status {
                                            VerifyOutcome::Clean => (theme::OK_SOFT, theme::OK),
                                            VerifyOutcome::IssuesFound { .. } => {
                                                (theme::DANGER_SOFT, theme::DANGER)
                                            }
                                            VerifyOutcome::ExtraOnly { .. }
                                            | VerifyOutcome::CleanButSizeOnly { .. } => {
                                                (theme::WARN_SOFT, theme::WARN)
                                            }
                                            VerifyOutcome::Cancelled => {
                                                (theme::TRACK, theme::TEXT_MUTED)
                                            }
                                        };
                                        ui.horizontal(|ui| {
                                            theme::badge(ui, status.label(), tag_bg, tag_fg);
                                            let date = when.split('T').next().unwrap_or(&when);
                                            ui.label(
                                                egui::RichText::new(date)
                                                    .size(11.0)
                                                    .color(theme::TEXT_MUTED),
                                            );
                                        });
                                    }
                                    None => {
                                        ui.colored_label(theme::TEXT_MUTED, "未复查");
                                    }
                                }

                                ui.end_row();
                            }
                        }

                        // 渲染离线盘
                        for off in &offline_drives {
                            // 1. 连接状态
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(
                                    egui::vec2(8.0, 8.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter()
                                    .circle_filled(r.center(), 3.5, theme::TEXT_DISABLED);
                                ui.add_space(2.0);
                                ui.label(
                                    egui::RichText::new("离线")
                                        .color(theme::TEXT_MUTED)
                                        .size(12.0),
                                );
                            });

                            // 2. 盘编号
                            ui.label(
                                egui::RichText::new(&off.id)
                                    .font(theme::subtitle_font())
                                    .color(theme::TEXT_MUTED),
                            );

                            // 3. 封盘状态
                            theme::badge(ui, "离线未知", theme::TRACK, theme::TEXT_MUTED);

                            // 4. 容量使用率
                            ui.label(egui::RichText::new("--").color(theme::TEXT_MUTED));

                            // 5. 剩余 / 总容量
                            ui.label(egui::RichText::new("磁盘未接入").color(theme::TEXT_MUTED));

                            // 6. 最近复查
                            let date = off
                                .last_verify_when
                                .split('T')
                                .next()
                                .unwrap_or(&off.last_verify_when);
                            ui.horizontal(|ui| {
                                theme::badge(
                                    ui,
                                    off.last_verify_status.label(),
                                    theme::TRACK,
                                    theme::TEXT_MUTED,
                                );
                                ui.label(
                                    egui::RichText::new(date)
                                        .size(11.0)
                                        .color(theme::TEXT_MUTED),
                                );
                            });

                            ui.end_row();
                        }
                    });
            });
    });
}

fn rescan(app: &mut App) {
    match app.backend.drives() {
        Ok(ds) => {
            app.drives_result = Some((
                true,
                format!("扫描完成：当前识别到 {} 块在线备份盘。", ds.len()),
            ));
            app.drives_cache = Some(ds);
        }
        Err(e) => {
            app.drives_cache = Some(Vec::new());
            app.drives_result = Some((false, format!("扫描盘失败：{:#}", e)));
            app.logs
                .push((LogLevel::Error, format!("扫描盘失败：{:#}", e)));
        }
    }
}

pub fn drive_tag(sealed: bool) -> &'static str {
    if sealed {
        "已封盘"
    } else {
        "可用"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_tag_distinguishes() {
        assert_eq!(drive_tag(true), "已封盘");
        assert_eq!(drive_tag(false), "可用");
    }
}

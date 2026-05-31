//! 盘列表视图:scan_mounted 列出已识别的备份盘(可用/已封盘/容量)。
//! 带缓存 + 「刷新」,不每帧重扫(scan_mounted 走 sysinfo,频繁扫浪费)。(Spec D §5)

use eframe::egui;

use bftool_core::engine::drive;
use bftool_core::reporter::LogLevel;

use crate::app::App;
use crate::views::util;

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    ui.heading("盘列表");
    ui.add_space(4.0);

    if ui.button("🔄 刷新").clicked() {
        rescan(app);
    }
    // 首次进入自动扫一次
    if app.drives_cache.is_none() {
        rescan(app);
    }

    ui.separator();
    if let Some((ok, msg)) = &app.drives_result {
        let color = if *ok {
            util::level_color(LogLevel::Ok)
        } else {
            util::level_color(LogLevel::Error)
        };
        ui.colored_label(color, msg);
    }
    match &app.drives_cache {
        Some(drives) if !drives.is_empty() => {
            egui::Grid::new("drives_grid")
                .num_columns(3)
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("盘");
                    ui.strong("状态");
                    ui.strong("容量");
                    ui.end_row();
                    for d in drives {
                        ui.monospace(format!("{} ({}:)", d.id, d.letter));
                        ui.label(drive_tag(d.sealed));
                        ui.label(format!(
                            "剩余 {} / 共 {}",
                            util::fmt_gb(d.free_bytes),
                            util::fmt_gb(d.total_bytes)
                        ));
                        ui.end_row();
                    }
                });
        }
        Some(_) => {
            ui.label("未发现已识别的备份盘。插入一块空盘 → 到「初始化新盘」做成「备份N」。");
        }
        None => {
            ui.label("（点「刷新」扫描）");
        }
    }
}

fn rescan(app: &mut App) {
    match drive::scan_mounted() {
        Ok(ds) => {
            app.drives_result = Some((true, format!("已刷新：识别到 {} 块备份盘。", ds.len())));
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

fn drive_tag(sealed: bool) -> &'static str {
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

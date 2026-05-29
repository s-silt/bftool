//! 视图间共享的小工具:格式化 / 配置来源文案 / 进度条 + 日志面板。(避免各视图重复)

use std::sync::{Arc, Mutex};

use eframe::egui;

use bftool_core::config::ConfigSource;
use bftool_core::reporter::LogLevel;

use crate::reporter::ProgressState;

/// 字节 → 人类可读 GB（一位小数）。纯函数,可测。
pub fn fmt_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1024.0 / 1024.0 / 1024.0)
}

/// 配置来源 → 一句话文案(仪表盘 / 设置页共用)。
pub fn source_hint(src: &ConfigSource) -> String {
    match src {
        ConfigSource::Explicit(p) => format!("配置来源：命令行指定 {}", p.display()),
        ConfigSource::Candidate(p) => format!("配置来源：自动发现 {}", p.display()),
        ConfigSource::Default => "配置来源：内置默认（到「设置」保存一份固化你的配置）".to_string(),
    }
}

/// 日志级别 → 颜色(archive/verify 日志面板共用)。
pub fn level_color(level: LogLevel) -> egui::Color32 {
    match level {
        LogLevel::Info => egui::Color32::GRAY,
        LogLevel::Ok => egui::Color32::from_rgb(0x2e, 0x7d, 0x32),
        LogLevel::Warn => egui::Color32::from_rgb(0x8a, 0x6d, 0x00),
        LogLevel::Error => egui::Color32::from_rgb(0xCC, 0x33, 0x33),
        LogLevel::Action => egui::Color32::from_rgb(0x15, 0x65, 0xc0),
    }
}

/// 进度条:有进行中的字节量进度(total>0)才渲染。后台任务通过 GuiReporter 写状态。
pub fn progress_bar(progress: &Arc<Mutex<ProgressState>>, ui: &mut egui::Ui) {
    if let Ok(p) = progress.lock() {
        if p.active && p.total > 0 {
            let frac = (p.current as f32 / p.total as f32).clamp(0.0, 1.0);
            ui.add(egui::ProgressBar::new(frac).text(format!("{} {:.0}%", p.label, frac * 100.0)));
        }
    }
}

/// 日志面板:滚动 + 贴底 + 按 level 上色(archive/verify 共用)。
pub fn log_panel(logs: &[(LogLevel, String)], ui: &mut egui::Ui) {
    egui::ScrollArea::vertical()
        .max_height(180.0)
        .id_salt("log_panel")
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for (level, msg) in logs {
                ui.colored_label(level_color(*level), msg);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_gb_one_decimal() {
        assert_eq!(fmt_gb(0), "0.0 GB");
        assert_eq!(fmt_gb(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(fmt_gb(1024u64 * 1024 * 1024 * 3 / 2), "1.5 GB");
    }

    #[test]
    fn source_hint_mentions_origin() {
        use std::path::PathBuf;
        assert!(source_hint(&ConfigSource::Explicit(PathBuf::from("a.toml"))).contains("命令行"));
        assert!(
            source_hint(&ConfigSource::Candidate(PathBuf::from("b.toml"))).contains("自动发现")
        );
        assert!(source_hint(&ConfigSource::Default).contains("内置默认"));
    }
}

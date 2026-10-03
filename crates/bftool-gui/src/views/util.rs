//! 视图间共享的小工具: 格式化 / 路径处理 / 复制支持 / 错误与空状态 / 进度条与日志面板。

use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32};

use bftool_core::config::ConfigSource;
use bftool_core::reporter::LogLevel;

use crate::reporter::ProgressState;
use crate::views::theme;

/// 字节 → 人类可读 GB（一位小数）。纯函数，可测。
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

/// 日志级别 → 语义配色
pub fn level_color(level: LogLevel) -> Color32 {
    match level {
        LogLevel::Info => theme::TEXT_MUTED,
        LogLevel::Ok => theme::OK,
        LogLevel::Warn => theme::WARN,
        LogLevel::Error => theme::DANGER,
        LogLevel::Action => theme::PRIMARY,
    }
}

/// 长路径智能截断 (保留前后关键特征，中间以 … 省略)
pub fn truncate_path(path: &str, max_len: usize) -> String {
    if path.chars().count() <= max_len || max_len < 10 {
        return path.to_string();
    }
    let head_len = (max_len - 3) / 2;
    let tail_len = max_len - 3 - head_len;

    let head: String = path.chars().take(head_len).collect();
    let chars_count = path.chars().count();
    let tail: String = path.chars().skip(chars_count - tail_len).collect();
    format!("{head}…{tail}")
}

/// 表格或列表中展示的长路径组件:
/// - 自动截断避免布局溢出
/// - 鼠标悬停显示完整路径
/// - 提供小巧的“复制”按钮
pub fn copyable_path(ui: &mut egui::Ui, full_path: &str, max_len: usize) {
    let truncated = truncate_path(full_path, max_len);
    ui.horizontal(|ui| {
        let label_resp = ui.monospace(&truncated);
        if truncated != full_path {
            label_resp.on_hover_ui(|ui| {
                ui.label(full_path);
            });
        }
        let copy_btn = ui.add(
            egui::Button::new(egui::RichText::new("复制").size(10.5))
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .corner_radius(3),
        );
        if copy_btn.clicked() {
            ui.ctx().copy_text(full_path.to_string());
        }
        copy_btn.on_hover_text("复制完整路径");
    });
}

/// 明确规范的错误展示组件 (说明出了什么问题、哪些操作未完成、下一步建议)
pub fn error_banner(ui: &mut egui::Ui, title: &str, error_detail: &str, next_step: &str) {
    egui::Frame::default()
        .fill(theme::DANGER_SOFT)
        .stroke(egui::Stroke::new(1.0, theme::DANGER_BORDER))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    theme::badge(ui, "错误", theme::DANGER, Color32::WHITE);
                    ui.label(
                        egui::RichText::new(title)
                            .font(theme::title_font())
                            .color(theme::DANGER),
                    );
                });
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!("问题详情：{error_detail}"))
                        .color(theme::TEXT_BODY),
                );
                if !next_step.is_empty() {
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new(format!("下一步建议：{next_step}"))
                            .color(theme::TEXT_MUTED),
                    );
                }
            });
        });
}

/// 统一样式的空状态展示
pub fn empty_state(ui: &mut egui::Ui, title: &str, desc: &str) {
    egui::Frame::default()
        .fill(theme::CARD)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS))
        .inner_margin(egui::Margin::symmetric(24, 20))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .font(theme::title_font())
                        .color(theme::TEXT_TITLE),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(desc)
                        .size(12.0)
                        .color(theme::TEXT_MUTED),
                );
            });
        });
}

/// 进度条: 有进行中的字节量进度 (total>0) 才渲染
pub fn progress_bar(progress: &Arc<Mutex<ProgressState>>, ui: &mut egui::Ui) {
    if let Ok(p) = progress.lock() {
        if p.active && p.total > 0 {
            let frac = (p.current as f32 / p.total as f32).clamp(0.0, 1.0);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&p.label)
                        .size(12.0)
                        .color(theme::TEXT_BODY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.colored_label(theme::PRIMARY, format!("{:.1}%", frac * 100.0));
                });
            });
            ui.add_space(2.0);
            theme::hbar(ui, frac, theme::PRIMARY);
            ui.add_space(4.0);
        }
    }
}

/// 日志面板: 滚动 + 贴底 + 按 level 上色 + 带清空/复制入口
pub fn log_panel(logs: &[(LogLevel, String)], ui: &mut egui::Ui) {
    egui::Frame::default()
        .fill(theme::TRACK)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(160.0)
                .id_salt("log_panel_scroll")
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if logs.is_empty() {
                        ui.colored_label(theme::TEXT_MUTED, "暂无日志输出");
                    } else {
                        for (level, msg) in logs {
                            ui.horizontal_wrapped(|ui| {
                                let tag = match level {
                                    LogLevel::Info => "信息",
                                    LogLevel::Ok => "成功",
                                    LogLevel::Warn => "警告",
                                    LogLevel::Error => "错误",
                                    LogLevel::Action => "操作",
                                };
                                ui.colored_label(level_color(*level), format!("[{tag}]"));
                                ui.colored_label(theme::TEXT_BODY, msg);
                            });
                        }
                    }
                });
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

    #[test]
    fn truncate_path_works() {
        let p = "C:\\Synthetic\\Work\\projects\\subfolder\\target_file.txt";
        let t = truncate_path(p, 20);
        assert!(t.contains('…'));
        assert!(t.chars().count() <= 21);
    }
}

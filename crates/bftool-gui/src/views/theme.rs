//! 现代 Windows 生产力工具风格主题: 配色令牌 + 字体与样式装载 + 基础布局组件。
//! 纯 UI, 不依赖 core 业务逻辑。

use std::sync::Arc;

use eframe::egui::{self, Color32, FontFamily, FontId};

pub fn responsive_row(ui: &mut egui::Ui, threshold: f32, contents: impl FnOnce(&mut egui::Ui)) {
    if ui.available_width() < threshold {
        ui.vertical(contents);
    } else {
        // horizontal() bounds the row height; with_layout() lets separators fill remaining height.
        ui.horizontal(contents);
    }
}

/// Keep cards readable as the available logical width changes with window/zoom.
pub fn card_grid(
    ui: &mut egui::Ui,
    count: usize,
    wide_columns: usize,
    mut render: impl FnMut(&mut egui::Ui, usize),
) {
    let columns = if ui.available_width() < 700.0 {
        (wide_columns / 2).max(1)
    } else {
        wide_columns
    };
    for first in (0..count).step_by(columns) {
        ui.columns(columns, |cells| {
            for (offset, cell) in cells.iter_mut().enumerate() {
                if first + offset < count {
                    render(cell, first + offset);
                }
            }
        });
        if first + columns < count {
            ui.add_space(GAP);
        }
    }
}

// ── 配色令牌 (安静、克制、专业的现代 Windows 浅色风格) ──
pub const BG: Color32 = Color32::from_rgb(0xF8, 0xFA, 0xFC); // Slate-50
pub const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF); // 纯白卡片
pub const BORDER: Color32 = Color32::from_rgb(0xE2, 0xE8, 0xF0); // 细线边框 Slate-200
pub const BORDER_STRONG: Color32 = Color32::from_rgb(0xCB, 0xD5, 0xE1); // Slate-300
pub const TRACK: Color32 = Color32::from_rgb(0xF1, 0xF5, 0xF9); // 轨道槽底色 Slate-100

// 主色: 克制而清晰的专业深天蓝
pub const PRIMARY: Color32 = Color32::from_rgb(0x02, 0x84, 0xC7); // Sky-600
pub const PRIMARY_HOVER: Color32 = Color32::from_rgb(0x03, 0x69, 0xA1); // Sky-700
pub const PRIMARY_SOFT: Color32 = Color32::from_rgb(0xF0, 0xF9, 0xFF); // Sky-50
pub const PRIMARY_BORDER: Color32 = Color32::from_rgb(0xBA, 0xE6, 0xFD); // Sky-200

pub const CYAN: Color32 = Color32::from_rgb(0x02, 0x84, 0xC7);
pub const VIOLET: Color32 = Color32::from_rgb(0x63, 0x66, 0xF1);

// 文字层级
pub const TEXT_TITLE: Color32 = Color32::from_rgb(0x0F, 0x17, 0x2A); // Slate-900
pub const TEXT_BODY: Color32 = Color32::from_rgb(0x33, 0x41, 0x55); // Slate-700
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x64, 0x74, 0x8B); // Slate-500
pub const TEXT_DISABLED: Color32 = Color32::from_rgb(0x94, 0xA3, 0xB8); // Slate-400

// 状态语义色 (均配有专属软底色)
pub const OK: Color32 = Color32::from_rgb(0x16, 0xA3, 0x4A); // Emerald-600
pub const OK_SOFT: Color32 = Color32::from_rgb(0xDC, 0xFC, 0xE7); // Emerald-100
pub const OK_BORDER: Color32 = Color32::from_rgb(0x86, 0xEF, 0xAC);

pub const WARN: Color32 = Color32::from_rgb(0xD9, 0x77, 0x06); // Amber-600
pub const WARN_SOFT: Color32 = Color32::from_rgb(0xFE, 0xF3, 0xC7); // Amber-100
pub const WARN_BORDER: Color32 = Color32::from_rgb(0xFC, 0xD3, 0x4D);

pub const DANGER: Color32 = Color32::from_rgb(0xDC, 0x26, 0x26); // Red-600
pub const DANGER_SOFT: Color32 = Color32::from_rgb(0xFE, 0xE2, 0xE2); // Red-100
pub const DANGER_BORDER: Color32 = Color32::from_rgb(0xFC, 0xA5, 0xA5);

/// 卡片与模块默认间距
pub const GAP: f32 = 14.0;
/// 克制的微圆角 (6px，避免大圆角带来的松散感)
pub const RADIUS: u8 = 6;

/// 页面大标题
pub fn h1_font() -> FontId {
    FontId::new(19.0, FontFamily::Name("semibold".into()))
}

/// 区块/卡片标题
pub fn title_font() -> FontId {
    FontId::new(15.0, FontFamily::Name("semibold".into()))
}

/// 次级小标题
pub fn subtitle_font() -> FontId {
    FontId::new(13.0, FontFamily::Name("semibold".into()))
}

/// 应用主题配置
pub fn apply(ctx: &egui::Context) -> String {
    let status = install_fonts(ctx);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = CARD;
    style.visuals.extreme_bg_color = TRACK;
    style.visuals.override_text_color = Some(TEXT_BODY);
    style.visuals.widgets.noninteractive.bg_fill = CARD;
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    style.visuals.widgets.inactive.bg_fill = CARD;
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    style.visuals.widgets.hovered.bg_fill = PRIMARY_SOFT;
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, PRIMARY_BORDER);
    style.visuals.widgets.active.bg_fill = PRIMARY_SOFT;
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.5, PRIMARY);

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.indent = 16.0;
    ctx.set_style(style);
    status
}

/// 加载系统 CJK 字体
fn load_first(paths: &[&str]) -> Option<(String, Vec<u8>)> {
    use ab_glyph::{Font, FontRef};
    for p in paths {
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let usable = FontRef::try_from_slice_and_index(&bytes, 0)
            .map(|f| f.glyph_id('备').0 != 0)
            .unwrap_or(false);
        if usable {
            return Some((p.to_string(), bytes));
        }
    }
    None
}

fn install_fonts(ctx: &egui::Context) -> String {
    const REGULAR: [&str; 4] = [
        r"C:\Windows\Fonts\msyh.ttc", // 微软雅黑
        r"C:\Windows\Fonts\msyh.ttf",
        r"C:\Windows\Fonts\simhei.ttf", // 黑体
        r"C:\Windows\Fonts\simsun.ttc", // 宋体
    ];
    const BOLD: [&str; 3] = [
        r"C:\Windows\Fonts\msyhbd.ttc", // 微软雅黑 Bold
        r"C:\Windows\Fonts\msyhbd.ttf",
        r"C:\Windows\Fonts\simhei.ttf", // 黑体兜底
    ];

    let Some((reg_path, reg_bytes)) = load_first(&REGULAR) else {
        let msg = "未找到可用中文字体, 界面中文可能显示为方块".to_string();
        eprintln!("[bftool-gui] 警告: {msg}");
        return msg;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        Arc::new(egui::FontData::from_owned(reg_bytes)),
    );

    let semibold_list = match load_first(&BOLD) {
        Some((_, bold_bytes)) => {
            fonts.font_data.insert(
                "cjk-bold".to_owned(),
                Arc::new(egui::FontData::from_owned(bold_bytes)),
            );
            vec!["cjk-bold".to_owned(), "cjk".to_owned()]
        }
        None => vec!["cjk".to_owned()],
    };

    for fam in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(fam)
            .or_default()
            .insert(0, "cjk".to_owned());
    }
    fonts
        .families
        .insert(FontFamily::Name("semibold".into()), semibold_list);

    ctx.set_fonts(fonts);
    let msg = format!("已加载中文字体: {reg_path}");
    eprintln!("[bftool-gui] {msg}");
    msg
}

// ── 核心 UI 组件 ──

/// 统一样式的页面标题头部
pub fn page_header(ui: &mut egui::Ui, title: &str, desc: &str) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(title).font(h1_font()).color(TEXT_TITLE));
        if !desc.is_empty() {
            ui.add_space(2.0);
            ui.label(egui::RichText::new(desc).size(12.5).color(TEXT_MUTED));
        }
    });
    ui.add_space(8.0);
}

/// 统一样式的区块小标题
pub fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .font(title_font())
            .color(TEXT_TITLE),
    );
    ui.add_space(4.0);
}

/// 标准白底卡片容器
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::default()
        .fill(CARD)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(egui::CornerRadius::same(RADIUS))
        .inner_margin(egui::Margin::same(12))
        .show(ui, add)
        .inner
}

/// 浅色弱化底色卡片容器
pub fn card_subtle<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::default()
        .fill(BG)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(egui::CornerRadius::same(RADIUS))
        .inner_margin(egui::Margin::same(10))
        .show(ui, add)
        .inner
}

/// 状态提示条 (不使用特殊表情符号，以清晰文字标识提示性质)
pub fn callout(ui: &mut egui::Ui, color: Color32, soft: Color32, text: &str) {
    callout_with_tag(ui, color, soft, "提示", text);
}

/// 带标签前缀的状态提示条
pub fn callout_with_tag(ui: &mut egui::Ui, color: Color32, soft: Color32, tag: &str, text: &str) {
    egui::Frame::default()
        .fill(soft)
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(egui::CornerRadius::same(RADIUS))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                badge(ui, tag, color, Color32::WHITE);
                ui.add_space(6.0);
                ui.colored_label(TEXT_BODY, text);
            });
        });
}

/// 状态徽章胶囊
pub fn badge(ui: &mut egui::Ui, text: &str, bg: Color32, fg: Color32) {
    egui::Frame::default()
        .fill(bg)
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).size(11.0).color(fg));
        });
}

/// KPI 指标卡片
pub fn kpi_card(ui: &mut egui::Ui, title: &str, value: &str, sub: &str, accent: Color32) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(egui::vec2(4.0, 48.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(r, egui::CornerRadius::same(2), accent);
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(title).size(11.5).color(TEXT_MUTED));
                ui.add_space(1.0);
                ui.label(
                    egui::RichText::new(value)
                        .font(FontId::new(26.0, FontFamily::Name("semibold".into())))
                        .color(TEXT_TITLE),
                );
                ui.add_space(1.0);
                ui.label(egui::RichText::new(sub).size(11.0).color(TEXT_MUTED));
            });
        });
    });
}

/// 平滑水平进度条
pub fn hbar(ui: &mut egui::Ui, frac: f32, color: Color32) {
    let frac = frac.clamp(0.0, 1.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 7.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, egui::CornerRadius::same(3), TRACK);
    if frac > 0.0 {
        let mut fill = rect;
        fill.set_width(rect.width() * frac);
        p.rect_filled(fill, egui::CornerRadius::same(3), color);
    }
}

/// 环形统计图
pub fn ring(ui: &mut egui::Ui, frac: f32, color: Color32, center: &str, label: &str, size: f32) {
    let frac = frac.clamp(0.0, 1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let c = rect.center();
    let radius = size * 0.38;
    let stroke_w = size * 0.09;
    let p = ui.painter();
    p.circle_stroke(c, radius, egui::Stroke::new(stroke_w, TRACK));
    if frac > 0.0 {
        let start = -std::f32::consts::FRAC_PI_2;
        let n = ((frac * 96.0).round() as usize).max(1);
        let mut pts = Vec::with_capacity(n + 1);
        for i in 0..=n {
            let a = start + (frac * i as f32 / n as f32) * std::f32::consts::TAU;
            pts.push(c + egui::vec2(a.cos(), a.sin()) * radius);
        }
        p.add(egui::Shape::line(pts, egui::Stroke::new(stroke_w, color)));
    }
    p.text(
        c + egui::vec2(0.0, -8.0),
        egui::Align2::CENTER_CENTER,
        center,
        FontId::new(20.0, FontFamily::Name("semibold".into())),
        TEXT_TITLE,
    );
    p.text(
        c + egui::vec2(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.0),
        TEXT_MUTED,
    );
}

/// 主操作按钮样式 (实心蓝底白字)
pub fn btn_primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned())
            .size(13.0)
            .color(Color32::WHITE),
    )
    .fill(PRIMARY)
    .corner_radius(RADIUS)
}

/// 危险操作按钮样式 (实心红底白字)
pub fn btn_danger(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned())
            .size(13.0)
            .color(Color32::WHITE),
    )
    .fill(DANGER)
    .corner_radius(RADIUS)
}

/// 次要操作按钮样式 (带细边框的浅色按钮)
pub fn btn_secondary(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.to_owned())
            .size(13.0)
            .color(TEXT_BODY),
    )
    .stroke(egui::Stroke::new(1.0, BORDER))
    .corner_radius(RADIUS)
}

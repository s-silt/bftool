//! 浅色 · 扁平 · 科技风主题:配色令牌 + `apply`(字体/样式)+ 组件(card / callout / kpi_card / hbar / ring)。
//! **纯 UI,不依赖 core。** 字体走运行时系统加载(含中文 + 拉丁),不内嵌(免下载/subset 工具链与二进制膨胀)。

use std::sync::Arc;

use eframe::egui::{self, Color32, FontFamily, FontId};

// ── 配色令牌(Tailwind 系)──
pub const BG: Color32 = Color32::from_rgb(0xF5, 0xF7, 0xFA);
pub const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
pub const BORDER: Color32 = Color32::from_rgb(0xE2, 0xE8, 0xF0);
pub const TRACK: Color32 = Color32::from_rgb(0xEE, 0xF2, 0xF7);
pub const PRIMARY: Color32 = Color32::from_rgb(0x3B, 0x82, 0xF6);
pub const PRIMARY_SOFT: Color32 = Color32::from_rgb(0xEF, 0xF6, 0xFF);
pub const CYAN: Color32 = Color32::from_rgb(0x06, 0xB6, 0xD4);
pub const VIOLET: Color32 = Color32::from_rgb(0x8B, 0x5C, 0xF6);
pub const TEXT_TITLE: Color32 = Color32::from_rgb(0x1E, 0x29, 0x3B);
pub const TEXT_BODY: Color32 = Color32::from_rgb(0x64, 0x74, 0x8B);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x94, 0xA3, 0xB8);
pub const OK: Color32 = Color32::from_rgb(0x10, 0xB9, 0x81);
pub const WARN: Color32 = Color32::from_rgb(0xF5, 0x9E, 0x0B);
pub const WARN_SOFT: Color32 = Color32::from_rgb(0xFF, 0xFB, 0xEB);
pub const DANGER: Color32 = Color32::from_rgb(0xEF, 0x44, 0x44);
pub const DANGER_SOFT: Color32 = Color32::from_rgb(0xFE, 0xF2, 0xF2);

/// 卡片间距。
pub const GAP: f32 = 16.0;
/// 卡片圆角。
pub const RADIUS: u8 = 10;

/// 小标题字体(卡片标题用)。
pub fn title_font() -> FontId {
    FontId::new(17.0, FontFamily::Name("semibold".into()))
}

/// 应用主题:装字体 + 浅色扁平样式。返回字体加载状态(进日志,便于排查中文显示)。
pub fn apply(ctx: &egui::Context) -> String {
    let status = install_fonts(ctx);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = CARD;
    style.visuals.extreme_bg_color = TRACK;
    style.visuals.override_text_color = Some(TEXT_BODY);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    ctx.set_style(style);
    status
}

/// 读第一个"能解析且含中文字形('备')"的系统字体。纯运行时,避免把坏字体喂给 egui(epaint 解析失败会 panic)。
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

/// 装系统 CJK 字体(常规 + 粗体)。常规插字体链首位(保留默认字体作 emoji 后备);
/// 粗体注册为 `"semibold"` 族(找不到则回退常规,保证 `FontFamily::Name("semibold")` 永远可解析)。
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
        r"C:\Windows\Fonts\simhei.ttf", // 黑体(偏粗,兜底)
    ];

    let Some((reg_path, reg_bytes)) = load_first(&REGULAR) else {
        let msg = "未找到可用中文字体,界面中文可能显示为方块(请确认系统装有中文字体)".to_string();
        eprintln!("[bftool-gui] 警告: {msg}");
        return msg;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".to_owned(),
        Arc::new(egui::FontData::from_owned(reg_bytes)),
    );

    // 粗体族:有粗体用粗体 + 常规兜底;没有就纯常规(标题不会更粗,但不破)。
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

// ── 组件 ──

/// 白底卡片:圆角 + 细边 + 内边距。
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::default()
        .fill(CARD)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(egui::CornerRadius::same(RADIUS))
        .inner_margin(egui::Margin::same(14))
        .show(ui, add)
        .inner
}

/// 提示条:浅底 + 同色边 + ⚠ 图标 + 文案(自动换行)。
pub fn callout(ui: &mut egui::Ui, color: Color32, soft: Color32, text: &str) {
    egui::Frame::default()
        .fill(soft)
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(color, "⚠");
                ui.add_space(4.0);
                ui.colored_label(TEXT_BODY, text);
            });
        });
}

/// KPI 卡:左侧强调竖条 + 标签 + 大数字 + 副标签。
pub fn kpi_card(ui: &mut egui::Ui, title: &str, value: &str, sub: &str, accent: Color32) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(egui::vec2(4.0, 44.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(r, egui::CornerRadius::same(2), accent);
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new(title).size(11.0).color(TEXT_MUTED));
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(value)
                        .font(FontId::new(28.0, FontFamily::Name("semibold".into())))
                        .color(TEXT_TITLE),
                );
                ui.add_space(2.0);
                ui.label(egui::RichText::new(sub).size(11.0).color(TEXT_MUTED));
            });
        });
    });
}

/// 水平进度条(轨道 + 填充)。`frac` 自动 clamp。
pub fn hbar(ui: &mut egui::Ui, frac: f32, color: Color32) {
    let frac = frac.clamp(0.0, 1.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 8.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, egui::CornerRadius::same(4), TRACK);
    let mut fill = rect;
    fill.set_width(rect.width() * frac);
    p.rect_filled(fill, egui::CornerRadius::same(4), color);
}

/// 进度环(圆环 + 沿弧折线进度 + 中心大数字 + 下方小标签)。egui 无原生弧,用采样折线画。
pub fn ring(ui: &mut egui::Ui, frac: f32, color: Color32, center: &str, label: &str, size: f32) {
    let frac = frac.clamp(0.0, 1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let c = rect.center();
    let radius = size * 0.40;
    let stroke_w = size * 0.10;
    let p = ui.painter();
    p.circle_stroke(c, radius, egui::Stroke::new(stroke_w, TRACK));
    if frac > 0.0 {
        let start = -std::f32::consts::FRAC_PI_2; // 12 点方向起
        let n = ((frac * 96.0).round() as usize).max(1);
        let mut pts = Vec::with_capacity(n + 1);
        for i in 0..=n {
            let a = start + (frac * i as f32 / n as f32) * std::f32::consts::TAU;
            pts.push(c + egui::vec2(a.cos(), a.sin()) * radius);
        }
        p.add(egui::Shape::line(pts, egui::Stroke::new(stroke_w, color)));
    }
    p.text(
        c + egui::vec2(0.0, -7.0),
        egui::Align2::CENTER_CENTER,
        center,
        FontId::new(22.0, FontFamily::Name("semibold".into())),
        TEXT_TITLE,
    );
    p.text(
        c + egui::vec2(0.0, 15.0),
        egui::Align2::CENTER_CENTER,
        label,
        FontId::proportional(11.0),
        TEXT_MUTED,
    );
}

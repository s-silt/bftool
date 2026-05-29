//! 设计令牌 + 全局外观(浅色·扁平·科技风)+ 字体安装 + 卡片/KPI/图表小组件。
//!
//! 风格规范(本次重设计):
//! - 浅灰白背景(#F5F7FA),纯白卡片(#FFFFFF)+1px 浅边框(#E2E8F0),无阴影 → 扁平。
//! - 主强调科技蓝(#3B82F6),辅以青(#06B6D4)/紫(#8B5CF6)区分数据维度。
//! - 文字深灰(标题 #1E293B / 正文 #64748B),不用纯黑。
//! - 数字用内嵌 Inter(科技感、等宽数字 tnum),中文走系统字体逐字形回退。
//!
//! 所有视图共享:`apply(ctx)` 注入主题+字体;`card / kpi_card / hbar / ring` 画组件。

use std::sync::Arc;

use eframe::egui;
use egui::{Color32, CornerRadius, FontId, Margin, Stroke};

// ───────────────────────── 配色令牌 ─────────────────────────

pub const BG: Color32 = Color32::from_rgb(0xF5, 0xF7, 0xFA); // 窗口背景
pub const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF); // 卡片底
pub const BORDER: Color32 = Color32::from_rgb(0xE2, 0xE8, 0xF0); // 1px 浅边框
pub const TRACK: Color32 = Color32::from_rgb(0xEE, 0xF2, 0xF7); // 进度/容量轨道底

pub const PRIMARY: Color32 = Color32::from_rgb(0x3B, 0x82, 0xF6); // 科技蓝(主)
pub const PRIMARY_SOFT: Color32 = Color32::from_rgb(0xEF, 0xF6, 0xFF); // 蓝的极浅填充
pub const CYAN: Color32 = Color32::from_rgb(0x06, 0xB6, 0xD4); // 青
pub const VIOLET: Color32 = Color32::from_rgb(0x8B, 0x5C, 0xF6); // 紫

pub const TEXT_TITLE: Color32 = Color32::from_rgb(0x1E, 0x29, 0x3B); // 标题深灰
pub const TEXT_BODY: Color32 = Color32::from_rgb(0x64, 0x74, 0x8B); // 正文灰
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x94, 0xA3, 0xB8); // 更弱的标签灰

pub const OK: Color32 = Color32::from_rgb(0x10, 0xB9, 0x81); // 成功/完好 绿
pub const WARN: Color32 = Color32::from_rgb(0xF5, 0x9E, 0x0B); // 警告 琥珀
pub const WARN_SOFT: Color32 = Color32::from_rgb(0xFF, 0xFB, 0xEB); // 警告极浅底
pub const DANGER: Color32 = Color32::from_rgb(0xEF, 0x44, 0x44); // 危险/损坏 红
pub const DANGER_SOFT: Color32 = Color32::from_rgb(0xFE, 0xF2, 0xF2);

pub const RADIUS: u8 = 10; // 全局圆角
pub const GAP: f32 = 16.0; // 卡片间距

/// 大号醒目数字(KPI)。半粗体 Inter,等宽数字。
pub fn num_font() -> FontId {
    FontId::new(28.0, egui::FontFamily::Name("semibold".into()))
}
/// 卡片小标题(半粗)。
pub fn title_font() -> FontId {
    FontId::new(13.0, egui::FontFamily::Name("semibold".into()))
}

// ───────────────────────── 主题 + 字体 ─────────────────────────

/// 一次性注入字体 + 浅色扁平主题。返回字体加载状态(记进日志,可观测)。
pub fn apply(ctx: &egui::Context) -> String {
    let status = install_fonts(ctx);
    apply_visuals(ctx);
    status
}

/// 浅色·扁平:白卡片、浅边框、零阴影、统一圆角、深灰文字。
fn apply_visuals(ctx: &egui::Context) {
    use egui::style::{Selection, WidgetVisuals, Widgets};
    let mut style = (*ctx.style()).clone();
    let mut v = egui::Visuals::light();

    v.panel_fill = BG;
    v.window_fill = BG;
    v.faint_bg_color = BG;
    v.extreme_bg_color = Color32::from_rgb(0xFB, 0xFC, 0xFD);
    v.window_corner_radius = CornerRadius::same(RADIUS);
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.window_shadow = egui::epaint::Shadow::NONE; // 扁平:去窗口阴影
    v.popup_shadow = egui::epaint::Shadow::NONE;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.hyperlink_color = PRIMARY;
    v.selection = Selection {
        bg_fill: PRIMARY_SOFT,
        stroke: Stroke::new(1.0, PRIMARY),
    };

    // 控件外观:白底、浅边、圆角;文字深灰;hover/active 用蓝色调。
    let radius = CornerRadius::same(8);
    let mk = |bg: Color32, weak: Color32, stroke: Color32, fg: Color32| WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: weak,
        bg_stroke: Stroke::new(1.0, stroke),
        fg_stroke: Stroke::new(1.0, fg),
        corner_radius: radius,
        expansion: 0.0,
    };
    v.widgets = Widgets {
        noninteractive: mk(CARD, BG, BORDER, TEXT_BODY),
        inactive: mk(CARD, CARD, BORDER, TEXT_TITLE),
        hovered: mk(PRIMARY_SOFT, PRIMARY_SOFT, PRIMARY, TEXT_TITLE),
        active: mk(PRIMARY_SOFT, PRIMARY_SOFT, PRIMARY, PRIMARY),
        open: mk(CARD, CARD, BORDER, TEXT_TITLE),
    };

    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.window_margin = Margin::same(8);

    // 字号层级:标题半粗、正文 13、小字 11(弱化标签)。
    use egui::{FontFamily, TextStyle};
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(17.0, FontFamily::Name("semibold".into())),
    );
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(13.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(13.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(11.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(12.0, FontFamily::Monospace),
    );

    ctx.set_style(style);
}

/// 装字体:内嵌 Inter(拉丁/数字,科技感)排首位 + 运行时系统中文字体(逐字形回退)。
/// Inter 无中文字形 → egui 自动回退到 CJK 字体渲染中文;CJK 覆盖动态内容(路径/项目名)。
/// 故意不内嵌完整中文字体(避免 ~16MB 膨胀);仅内嵌 70KB 的 Inter 子集。
fn install_fonts(ctx: &egui::Context) -> String {
    let mut fonts = egui::FontDefinitions::default();

    // 内嵌 Inter(子集:拉丁+数字+常用标点,各 ~70KB)。
    fonts.font_data.insert(
        "inter".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "inter-sb".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-SemiBold.ttf"
        ))),
    );

    // 运行时加载系统中文字体,校验可解析且含中文字形。
    let (cjk_key, cjk_status) = load_cjk(&mut fonts);

    use egui::FontFamily;
    // Proportional:Inter 在前(拉丁/数字),其次 CJK,再保留 egui 默认(emoji 等)。
    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    prop.insert(0, "inter".to_owned());
    if let Some(k) = &cjk_key {
        prop.insert(1, k.clone());
    }
    // Monospace:CJK 排首(等宽里也能显中文),保留默认等宽。
    if let Some(k) = &cjk_key {
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, k.clone());
    }
    // 半粗家族:Inter-SemiBold + CJK 回退(让加粗标题里的中文也有字形)。
    let mut semibold = vec!["inter-sb".to_owned()];
    if let Some(k) = &cjk_key {
        semibold.push(k.clone());
    }
    fonts
        .families
        .insert(FontFamily::Name("semibold".into()), semibold);

    ctx.set_fonts(fonts);
    cjk_status
}

/// 找一份可用的系统中文字体,插入 `fonts` 并返回其 key + 状态文案。
fn load_cjk(fonts: &mut egui::FontDefinitions) -> (Option<String>, String) {
    use ab_glyph::{Font, FontRef};
    const CANDIDATES: [&str; 7] = [
        r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑(Windows 优先)
        r"C:\Windows\Fonts\msyh.ttf",   // 旧版雅黑
        r"C:\Windows\Fonts\simhei.ttf", // 黑体
        r"C:\Windows\Fonts\simsun.ttc", // 宋体
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", // Linux
        "/usr/share/fonts/truetype/noto/NotoSansCJKsc-Regular.otf",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", // 本构建/测试环境
    ];
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let usable = FontRef::try_from_slice_and_index(&bytes, 0)
            .map(|f| f.glyph_id('备').0 != 0)
            .unwrap_or(false);
        if !usable {
            continue;
        }
        fonts.font_data.insert(
            "cjk".to_owned(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        let msg = format!("已加载中文字体: {path}");
        eprintln!("[bftool-gui] {msg}");
        return (Some("cjk".to_owned()), msg);
    }
    let msg = "未找到可用中文字体,界面中文可能显示为方块(请确认系统装有中文字体)".to_string();
    eprintln!("[bftool-gui] 警告: {msg}");
    (None, msg)
}

// ───────────────────────── 组件 ─────────────────────────

/// 白卡片容器:纯白底 + 1px 浅边 + 圆角 + 内边距,无阴影。所有面板统一走它。
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(16))
        .show(ui, add)
        .inner
}

/// 浅色提示条(info/warn/danger):极浅底 + 同色描边 + 深字,扁平。
pub fn callout(ui: &mut egui::Ui, accent: Color32, soft: Color32, text: &str) {
    egui::Frame::new()
        .fill(soft)
        .stroke(Stroke::new(1.0, accent))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(accent, "●");
                ui.add_space(2.0);
                ui.colored_label(TEXT_TITLE, text);
            });
        });
}

/// KPI 卡:标题(弱化)+ 大数字(醒目)+ 副标题(更弱)+ 右上角强调色圆点。
/// `delta` 可选(同比),正绿负红;本静态版通常传 None。
pub fn kpi_card(
    ui: &mut egui::Ui,
    title: &str,
    value: &str,
    subtitle: &str,
    accent: Color32,
    delta: Option<(&str, Color32)>,
) {
    card(ui, |ui| {
        ui.set_min_height(96.0);
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.colored_label(TEXT_BODY, egui::RichText::new(title).size(12.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // 右上角强调色小圆点(线性科技风的弱标记)。
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 4.0, accent);
                });
            });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(value)
                    .font(num_font())
                    .color(TEXT_TITLE),
            );
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.colored_label(TEXT_MUTED, egui::RichText::new(subtitle).size(11.0));
                if let Some((d, c)) = delta {
                    ui.colored_label(c, egui::RichText::new(d).size(11.0));
                }
            });
        });
    });
}

/// 横向容量条:轨道底 + 已用色块(圆角),右侧百分比文字。height≈10。
pub fn hbar(ui: &mut egui::Ui, frac: f32, color: Color32) {
    let frac = frac.clamp(0.0, 1.0);
    let h = 10.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    let p = ui.painter();
    let r = CornerRadius::same(5);
    p.rect_filled(rect, r, TRACK);
    if frac > 0.0 {
        let mut fill = rect;
        fill.set_width((rect.width() * frac).max(h)); // 至少画出圆角端
        p.rect_filled(fill, r, color);
    }
}

/// 环形进度(donut):track 底环 + 进度弧 + 圆心文字。占用 `size×size` 方形。
pub fn ring(ui: &mut egui::Ui, frac: f32, color: Color32, center: &str, sub: &str, size: f32) {
    use std::f32::consts::TAU;
    let frac = frac.clamp(0.0, 1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let c = rect.center();
    let radius = size * 0.42;
    let thickness = size * 0.12;
    let p = ui.painter();

    // 底环:整圈描边。
    p.circle_stroke(c, radius, Stroke::new(thickness, TRACK));

    // 进度弧:从 12 点钟方向顺时针,分段折线近似。
    if frac > 0.0 {
        let start = -TAU / 4.0;
        let segs = (96.0 * frac).ceil().max(2.0) as usize;
        let pts: Vec<egui::Pos2> = (0..=segs)
            .map(|i| {
                let a = start + TAU * frac * (i as f32 / segs as f32);
                egui::pos2(c.x + radius * a.cos(), c.y + radius * a.sin())
            })
            .collect();
        p.add(egui::Shape::line(
            pts,
            egui::epaint::PathStroke::new(thickness, color),
        ));
    }

    // 圆心:大百分比 + 小标签。
    p.text(
        c - egui::vec2(0.0, 6.0),
        egui::Align2::CENTER_CENTER,
        center,
        FontId::new(20.0, egui::FontFamily::Name("semibold".into())),
        TEXT_TITLE,
    );
    p.text(
        c + egui::vec2(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        sub,
        FontId::new(11.0, egui::FontFamily::Proportional),
        TEXT_MUTED,
    );
}

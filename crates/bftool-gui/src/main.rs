//! bftool 桌面版入口(eframe)。薄 bin:设窗口尺寸 + 构造 lib 的 `App`,交给 eframe 主循环。
//!
//! release 构建隐藏控制台窗口(GUI 双击不弹黑框);debug(`cargo run`)保留控制台,
//! 以便看到字体加载等启动诊断。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use bftool_gui::app::App;

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([920.0, 660.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("归档备份工具 bftool"),
        ..Default::default()
    };
    eframe::run_native(
        "归档备份工具 bftool",
        opts,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

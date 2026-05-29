//! bftool 桌面版入口(eframe)。薄 bin:构造 lib 的 `App` 并交给 eframe 主循环。

use bftool_gui::app::App;

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions::default();
    eframe::run_native(
        "归档备份工具 bftool",
        opts,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

//! bftool 桌面版入口(eframe)。
//!
//! Phase 2 T1:仅起一个最小可编译窗口,验证 eframe 在本机/CI 真机能 build。
//! 左侧栏 + 视图路由在 T4 接入(改用 `app::App`)。

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions::default();
    eframe::run_native(
        "归档备份工具 bftool",
        opts,
        Box::new(|_cc| Ok(Box::<MinApp>::default())),
    )
}

#[derive(Default)]
struct MinApp;

impl eframe::App for MinApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("归档备份工具 bftool");
            ui.label("Phase 2 骨架占位 —— 后续 task 接入左侧栏与视图。");
        });
    }
}

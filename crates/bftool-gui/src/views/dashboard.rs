//! 仪表盘视图(默认)。T5 接入 `status::gather`。

use eframe::egui;

use crate::app::App;

pub fn ui(_app: &mut App, ui: &mut egui::Ui) {
    ui.heading("仪表盘");
    ui.label("（T5 接入 status::gather：当前盘 / 待备份数 / 上次复查 + 三大入口）");
}

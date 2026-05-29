//! 备份视图。T6 接入 archive::plan(预览)+ 后台 run_plan(进度/日志/取消)。

use eframe::egui;

use crate::app::App;

pub fn ui(_app: &mut App, ui: &mut egui::Ui) {
    ui.heading("备份");
    ui.label("（T6 接入 archive::plan 预览 + 后台 run_plan + 进度条/日志/取消）");
}

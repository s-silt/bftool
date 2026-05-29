//! 各视图渲染。每个视图导出 `pub fn ui(app: &mut App, ui: &mut egui::Ui)`。
//! Phase 2 实现 dashboard + archive;其余视图在 app.rs 里走 placeholder(Phase 3 补)。

pub mod archive;
pub mod dashboard;
pub mod drives;
pub mod find;
pub mod util;

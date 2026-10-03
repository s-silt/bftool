//! bftool 桌面版(eframe/egui)。
//!
//! 模块放在 lib(而非全塞 bin),便于在被 UI 接线前就对纯逻辑(GuiReporter 投递、
//! task 状态机、视图启用)写单测,且 pub 项不触发 dead_code。bin(`main.rs`)是薄入口。
//!
//! 分工(Spec D §7):
//! - `reporter` ── GuiReporter:core Reporter → channel(日志)/ 共享状态(进度)
//! - `task`     ── 后台线程执行 + 取消(项目边界)+ 三态结果
//! - `app`      ── App 壳 + View 路由 + 左侧栏(T4)
//! - `views`    ── 各视图渲染(T4 起)

pub mod app;
pub mod backend;
pub mod reporter;
pub mod screenshot;
pub mod task;
pub mod views;

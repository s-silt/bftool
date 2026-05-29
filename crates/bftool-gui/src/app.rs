//! App 壳:左侧栏切换视图,中央面板渲染当前视图。长任务一律走 `task::BackgroundTask`,
//! UI 线程每帧 drain 日志 channel + 轮询任务完成 + 进行中时 request_repaint。(Spec D §3/§5)

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use eframe::egui;

use bftool_core::config::{Config, ConfigSource};
use bftool_core::engine::archive::ArchivePlan;
use bftool_core::reporter::LogLevel;

use crate::reporter::{ProgressState, UiEvent};
use crate::task::{BackgroundTask, TaskOutcome};

/// 左侧栏的 7 个视图(Spec D §5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Dashboard,
    Archive,
    Verify,
    Init,
    Find,
    Drives,
    Settings,
}

impl View {
    pub const ALL: [View; 7] = [
        View::Dashboard,
        View::Archive,
        View::Verify,
        View::Init,
        View::Find,
        View::Drives,
        View::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            View::Dashboard => "仪表盘",
            View::Archive => "备份",
            View::Verify => "复查",
            View::Init => "初始化新盘",
            View::Find => "查找",
            View::Drives => "盘列表",
            View::Settings => "设置",
        }
    }
}

/// 应用根状态。字段刻意平铺(非拆子 struct),按用途分两类:
/// - **全局状态**(跨视图共享):`view` / `cfg` / `config_source` / `logs` /
///   `progress` / `rx` / `task` / `plan_task` / `task_started` / `last_summary` /
///   `archive_plan` / `status_cache`。
/// - **各视图状态**(仅本视图借用):`archive_ui` / `drives_cache` / `find_ui` /
///   `init_ui` / `verify_ui` / `settings_ui`。
///
/// 视图函数签名统一为 `ui(app: &mut App, ...)`,各自只读/写自己那块,目前无
/// borrow-checker 冲突(同一时刻只有一个视图在渲染)。已知权衡:平铺字段多,但拆
/// 子 struct 会牵动所有 view 函数签名,高风险低收益——故暂不拆。**若未来出现
/// borrow-checker 冲突**(如某视图需同时可变借用两块状态),再按视图分组拆子 struct。
pub struct App {
    pub view: View,
    pub cfg: Config,
    pub config_source: ConfigSource,
    /// 累积的日志(level + 文本),供日志面板渲染。
    pub logs: Vec<(LogLevel, String)>,
    /// 当前进度(后台任务通过 GuiReporter 写,UI 读)。
    pub progress: Arc<Mutex<ProgressState>>,
    /// 当前任务的日志接收端(任务结束后置 None)。
    pub rx: Option<Receiver<UiEvent>>,
    /// 进行中的执行任务(archive run / verify;None = 空闲)。
    pub task: Option<BackgroundTask<String>>,
    /// 进行中的计划预览任务(archive::plan;只读、后台算,避免 UI 线程遍历大目录卡顿)。
    pub plan_task: Option<BackgroundTask<ArchivePlan>>,
    /// 执行任务的开始时刻(算"已用时间";None=空闲)。在 archive/verify 启动处置位,pump 完成处清空。
    pub task_started: Option<std::time::Instant>,
    /// 上一个任务的摘要(Done/Failed 文案),供结果区显示。
    pub last_summary: Option<String>,
    /// 备份页的计划预览(archive::plan 的结果)。
    pub archive_plan: Option<ArchivePlan>,
    /// 备份页高级设置的持久 UI 状态(跨帧保留)。
    pub archive_ui: crate::views::archive::ArchiveUiState,
    /// 盘列表缓存(None = 未扫;带「刷新」按钮,不每帧重扫)。
    pub drives_cache: Option<Vec<bftool_core::engine::drive::DriveInfo>>,
    /// 查找页跨帧状态(关键词 + 结果)。
    pub find_ui: crate::views::find::FindUiState,
    /// 初始化页跨帧状态(候选缓存 + 选择 + force)。
    pub init_ui: crate::views::init::InitUiState,
    /// 复查页跨帧状态(盘列表 + 选择)。
    pub verify_ui: crate::views::verify::VerifyUiState,
    /// 设置页跨帧状态(表单字段 + 保存位置)。
    pub settings_ui: crate::views::settings::SettingsUiState,
    /// 仪表盘状态缓存(避免每帧调 status::gather;TTL 1500ms)。
    pub status_cache: Option<(
        bftool_core::engine::status::StatusReport,
        std::time::Instant,
    )>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // 应用主题(浅色扁平 + 装中文字体);返回字体状态进日志便于排查中文显示。
        let font_status = crate::views::theme::apply(&cc.egui_ctx);
        let mut logs = vec![(LogLevel::Info, font_status)];
        // 不吞错:配置加载失败时退默认,但把错误进日志面板让用户可见(Spec D §6 / 不掩盖 fail-closed)。
        let (cfg, config_source) = match Config::load_with_source(None) {
            Ok(l) => (l.config, l.source),
            Err(e) => {
                logs.push((
                    LogLevel::Error,
                    format!("加载配置失败,暂用内置默认：{:#}", e),
                ));
                (Config::default(), ConfigSource::Default)
            }
        };
        Self {
            view: View::default(),
            cfg,
            config_source,
            logs,
            progress: Arc::new(Mutex::new(ProgressState::default())),
            rx: None,
            task: None,
            plan_task: None,
            task_started: None,
            last_summary: None,
            archive_plan: None,
            archive_ui: crate::views::archive::ArchiveUiState::default(),
            drives_cache: None,
            find_ui: crate::views::find::FindUiState::default(),
            init_ui: crate::views::init::InitUiState::default(),
            verify_ui: crate::views::verify::VerifyUiState::default(),
            settings_ui: crate::views::settings::SettingsUiState::default(),
            status_cache: None,
        }
    }

    /// 每帧:drain 日志 channel + 轮询后台任务完成。进行中则 request_repaint 持续刷新。
    fn pump(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    UiEvent::Log { level, msg } => self.logs.push((level, msg)),
                }
            }
        }
        // 执行任务(run/verify)→ 摘要
        match self.task.as_ref().map(|t| t.is_finished()) {
            Some(true) => {
                if let Some(outcome) = self.task.as_mut().and_then(|t| t.take_outcome()) {
                    self.last_summary = Some(match outcome {
                        TaskOutcome::Done(s) => s,
                        TaskOutcome::Failed(e) => format!("失败：{}", e),
                    });
                }
                // 最终 drain:线程已结束但日志可能在最后一次 try_recv 之后才入队;
                // 丢弃 rx 前再收一遍,避免后台线程最后几条日志丢失。
                if let Some(rx) = &self.rx {
                    while let Ok(ev) = rx.try_recv() {
                        match ev {
                            UiEvent::Log { level, msg } => self.logs.push((level, msg)),
                        }
                    }
                }
                self.task = None;
                self.task_started = None;
                self.rx = None;
            }
            Some(false) => ctx.request_repaint(),
            None => {}
        }
        // 计划预览任务(plan)→ archive_plan
        match self.plan_task.as_ref().map(|t| t.is_finished()) {
            Some(true) => {
                if let Some(outcome) = self.plan_task.as_mut().and_then(|t| t.take_outcome()) {
                    match outcome {
                        TaskOutcome::Done(plan) => self.archive_plan = Some(plan),
                        TaskOutcome::Failed(e) => {
                            self.archive_plan = None;
                            self.logs.push((LogLevel::Error, e));
                        }
                    }
                }
                self.plan_task = None;
                // plan 不设 task_started(只读、不计"已用时间");此处一并清空仅作未来防陷阱
                // ——若以后 plan 也开始用 task_started,这里已替它收尾,不会残留脏值。
                self.task_started = None;
                // plan 期间用的也是 rx(GuiReporter 日志);算完前做最终 drain 再释放,
                // 避免 plan 线程最后几条日志丢失。
                if self.task.is_none() {
                    if let Some(rx) = &self.rx {
                        while let Ok(ev) = rx.try_recv() {
                            match ev {
                                UiEvent::Log { level, msg } => self.logs.push((level, msg)),
                            }
                        }
                    }
                    self.rx = None;
                }
            }
            Some(false) => ctx.request_repaint(),
            None => {}
        }
    }

    /// 是否有进行中的后台任务(执行或计划预览)。导航/按钮据此禁用。
    pub fn is_busy(&self) -> bool {
        self.task.is_some() || self.plan_task.is_some()
    }

    /// 底部全局状态栏:区分 执行任务(可取消)/ 生成计划(不可取消)/ 空闲。
    /// 进度只显示 core 上报的字节进度(=某遍 SHA256 校验);复制/间隙阶段不伪造百分比。
    fn status_bar(&mut self, ctx: &egui::Context) {
        use crate::views::theme;
        egui::TopBottomPanel::bottom("status")
            .exact_height(34.0)
            .frame(
                egui::Frame::default()
                    .fill(theme::CARD)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .inner_margin(egui::Margin::symmetric(12, 6)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.task.is_some() {
                        // 真·可取消的执行任务(备份 run / 复查)
                        ui.add(egui::Spinner::new().size(14.0).color(theme::PRIMARY));
                        ui.add_space(6.0);
                        // progress 锁竞争说明:后台线程每个 SHA256 chunk 都 inc(锁同一 Mutex),
                        // UI 每帧(~60fps)也读这把锁。但两侧临界区都极短——后台只 +=delta,
                        // UI 只 clone 几个字段后立即释放,无 I/O、无长循环——故竞争可接受;
                        // 而每帧读进度是渲染进度条所必需的,无法避开。
                        let snap = self
                            .progress
                            .lock()
                            .ok()
                            .filter(|p| p.active && p.total > 0)
                            .map(|p| (p.label.clone(), p.current as f32 / p.total as f32));
                        let elapsed = self
                            .task_started
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let line = match snap {
                            Some((phase, frac)) => format!(
                                "运行中 · {phase} {:.0}%　已用 {}",
                                frac * 100.0,
                                fmt_dur(elapsed)
                            ),
                            // 复制阶段/阶段间隙:core 无字节进度,不伪造百分比。
                            None => format!("运行中 · 处理中…　已用 {}", fmt_dur(elapsed)),
                        };
                        ui.colored_label(theme::PRIMARY, line);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let cancelling = self
                                .task
                                .as_ref()
                                .map(|t| t.cancel_requested())
                                .unwrap_or(false);
                            if cancelling {
                                ui.add_enabled(false, egui::Button::new("取消中…"));
                            } else if ui.button("✕ 取消").clicked() {
                                if let Some(t) = &self.task {
                                    t.request_cancel();
                                }
                            }
                        });
                    } else if self.plan_task.is_some() {
                        // 生成计划:只读、不可中途取消 → 不给取消按钮。
                        ui.add(egui::Spinner::new().size(14.0).color(theme::PRIMARY));
                        ui.add_space(6.0);
                        ui.colored_label(theme::PRIMARY, "正在生成计划…");
                    } else {
                        // 空闲
                        let (r, _) =
                            ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, theme::OK);
                        ui.add_space(4.0);
                        ui.colored_label(
                            theme::TEXT_BODY,
                            self.last_summary
                                .clone()
                                .unwrap_or_else(|| "就绪".to_string()),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let go = egui::Button::new(
                                egui::RichText::new("去备份").color(egui::Color32::WHITE),
                            )
                            .fill(theme::PRIMARY);
                            if ui.add(go).clicked() {
                                self.view = View::Archive;
                            }
                        });
                    }
                });
            });
    }
}

/// 秒 → mm:ss 或 hh:mm:ss。
fn fmt_dur(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// 扁平导航项:选中→浅蓝底 + 左侧主色竖条;hover→浅底。整行可点。
fn nav_item(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    use crate::views::theme;
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, egui::CornerRadius::same(8), theme::PRIMARY_SOFT);
        let bar = egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
        p.rect_filled(bar, egui::CornerRadius::same(2), theme::PRIMARY);
    } else if resp.hovered() {
        p.rect_filled(rect, egui::CornerRadius::same(8), theme::BG);
    }
    let color = if selected {
        theme::PRIMARY
    } else {
        theme::TEXT_BODY
    };
    p.text(
        rect.left_center() + egui::vec2(14.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        color,
    );
    resp
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);

        egui::SidePanel::left("nav")
            .resizable(false)
            .exact_width(150.0)
            .frame(
                egui::Frame::default()
                    .fill(crate::views::theme::CARD)
                    .inner_margin(egui::Margin::same(10)),
            )
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("bftool")
                        .font(egui::FontId::new(
                            18.0,
                            egui::FontFamily::Name("semibold".into()),
                        ))
                        .color(crate::views::theme::PRIMARY),
                );
                ui.add_space(10.0);
                let busy = self.is_busy();
                let current = self.view;
                for v in View::ALL {
                    let selected = current == v;
                    // 任务进行中禁用导航(选中项除外),避免切走正在跑的任务。
                    ui.add_enabled_ui(!busy || selected, |ui| {
                        if nav_item(ui, selected, v.label()).clicked() {
                            self.view = v;
                        }
                    });
                    ui.add_space(2.0);
                }
                // 底部版本号
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.add_space(6.0);
                    ui.weak(concat!("v", env!("CARGO_PKG_VERSION")));
                });
            });

        // 全局底部状态栏(在 CentralPanel 之前挂)。
        self.status_bar(ctx);

        egui::CentralPanel::default().show(ctx, |ui| match self.view {
            View::Dashboard => crate::views::dashboard::ui(self, ui),
            View::Archive => crate::views::archive::ui(self, ui),
            View::Drives => crate::views::drives::ui(self, ui),
            View::Find => crate::views::find::ui(self, ui),
            View::Init => crate::views::init::ui(self, ui),
            View::Verify => crate::views::verify::ui(self, ui),
            View::Settings => crate::views::settings::ui(self, ui),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_view_is_dashboard() {
        assert_eq!(View::default(), View::Dashboard);
    }

    #[test]
    fn all_views_have_nonempty_labels() {
        for v in View::ALL {
            assert!(!v.label().is_empty(), "{:?} 应有标签", v);
        }
        // ALL 覆盖 7 个且无重复标签
        assert_eq!(View::ALL.len(), 7);
        let mut labels: Vec<&str> = View::ALL.iter().map(|v| v.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 7, "标签不应重复");
    }
}

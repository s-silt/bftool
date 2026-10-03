//! App 壳:左侧栏切换视图,中央面板渲染当前视图。长任务一律走 `task::BackgroundTask`,
//! UI 线程每帧 drain 日志 channel + 轮询任务完成 + 进行中时 request_repaint。(Spec D §3/§5)

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use eframe::egui;

use bftool_core::config::{Config, ConfigSource};
use bftool_core::engine::archive::{ArchivePlan, Options};
use bftool_core::engine::find::FindOutcome;
use bftool_core::reporter::LogLevel;

use crate::backend::Backend;
use crate::reporter::{ProgressState, UiEvent};
use crate::task::{BackgroundTask, TaskOutcome};
use bftool_core::engine::verify::VerifyReport;

/// 左侧栏的 7 个视图(Spec D §5)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum View {
    #[default]
    Dashboard,
    Archive,
    Verify,
    Init,
    Find,
    Drives,
    Watch,
    Settings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePlanInputs {
    pub cfg: Config,
    pub opts: Options,
}

pub struct PlannedArchive {
    pub plan: ArchivePlan,
    pub inputs: ArchivePlanInputs,
}

pub enum ExecutionResult {
    Summary(String),
    Verification(VerifyReport),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Archive,
    Verify,
    Watch,
}

impl View {
    pub const ALL: [View; 8] = [
        View::Dashboard,
        View::Archive,
        View::Watch,
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
            View::Watch => "监视",
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
    pub backend: Backend,
    pub task_kind: Option<TaskKind>,
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
    pub task: Option<BackgroundTask<ExecutionResult>>,
    /// 进行中的计划预览任务(archive::plan;只读、后台算,避免 UI 线程遍历大目录卡顿)。
    pub plan_task: Option<BackgroundTask<PlannedArchive>>,
    /// 进行中的查找任务(find::search;可能读取大索引/网络盘,不能阻塞 UI 线程)。
    pub find_task: Option<BackgroundTask<FindOutcome>>,
    /// 执行任务的开始时刻(算"已用时间";None=空闲)。在 archive/verify 启动处置位,pump 完成处清空。
    pub task_started: Option<std::time::Instant>,
    /// 上一个任务的摘要(Done/Failed 文案),供结果区显示。
    pub last_summary: Option<String>,
    /// 备份页的计划预览(archive::plan 的结果)。
    pub archive_plan: Option<ArchivePlan>,
    /// 生成 `archive_plan` 时使用的配置/选项快照。执行前必须仍一致,否则要求重新演练。
    pub archive_plan_inputs: Option<ArchivePlanInputs>,
    /// 备份页高级设置的持久 UI 状态(跨帧保留)。
    pub archive_ui: crate::views::archive::ArchiveUiState,
    /// 盘列表缓存(None = 未扫;带「刷新」按钮,不每帧重扫)。
    pub drives_cache: Option<Vec<bftool_core::engine::drive::DriveInfo>>,
    /// 盘列表页本页结果/错误提示。
    pub drives_result: Option<(bool, String)>,
    /// 查找页跨帧状态(关键词 + 结果)。
    pub find_ui: crate::views::find::FindUiState,
    /// 初始化页跨帧状态(候选缓存 + 选择 + force)。
    pub init_ui: crate::views::init::InitUiState,
    /// 复查页跨帧状态(盘列表 + 选择)。
    pub verify_ui: crate::views::verify::VerifyUiState,
    /// 设置页跨帧状态(表单字段 + 保存位置)。
    pub settings_ui: crate::views::settings::SettingsUiState,
    pub watch_ui: crate::views::watch::WatchUiState,
    /// 仪表盘状态缓存(避免每帧调 status::gather;TTL 1500ms)。
    pub status_cache: Option<(
        bftool_core::engine::status::StatusReport,
        std::time::Instant,
    )>,
    pub screenshot_runner: Option<crate::screenshot::ScreenshotRunner>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        Self::build(&cc.egui_ctx, Backend::Live)
    }

    pub fn new_demo(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::build(&cc.egui_ctx, Backend::Demo);
        crate::screenshot::inject_demo_data(&mut app);
        app
    }

    fn build(ctx: &egui::Context, backend: Backend) -> Self {
        // 应用主题(浅色扁平 + 装中文字体);返回字体状态进日志便于排查中文显示。
        let font_status = crate::views::theme::apply(ctx);
        let mut logs = vec![(LogLevel::Info, font_status)];
        // 不吞错:配置加载失败时退默认,但把错误进日志面板让用户可见(Spec D §6 / 不掩盖 fail-closed)。
        let (cfg, config_source) = match backend.load_config() {
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
            backend,
            task_kind: None,
            view: View::default(),
            cfg,
            config_source,
            logs,
            progress: Arc::new(Mutex::new(ProgressState::default())),
            rx: None,
            task: None,
            plan_task: None,
            find_task: None,
            task_started: None,
            last_summary: None,
            archive_plan: None,
            archive_plan_inputs: None,
            archive_ui: crate::views::archive::ArchiveUiState::default(),
            drives_cache: None,
            drives_result: None,
            find_ui: crate::views::find::FindUiState::default(),
            init_ui: crate::views::init::InitUiState::default(),
            verify_ui: crate::views::verify::VerifyUiState::default(),
            settings_ui: crate::views::settings::SettingsUiState::default(),
            watch_ui: crate::views::watch::WatchUiState::default(),
            status_cache: None,
            screenshot_runner: None,
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
                    let summary = match outcome {
                        TaskOutcome::Done(ExecutionResult::Summary(s)) => s,
                        TaskOutcome::Done(ExecutionResult::Verification(report)) => {
                            let text = crate::views::verify::verify_summary(
                                report.checked,
                                report.bad,
                                report.extra,
                                report.size_only,
                                report.cancelled,
                            );
                            self.verify_ui.last_stats =
                                Some(crate::views::verify::VerifyStats::from_report(&report));
                            self.verify_ui.last_report = Some(report);
                            self.verify_ui.summary = Some(text.clone());
                            self.verify_ui.error = None;
                            text
                        }
                        TaskOutcome::Failed(e) => {
                            if self.task_kind == Some(TaskKind::Verify) {
                                self.verify_ui.last_stats = None;
                                self.verify_ui.last_report = None;
                                self.verify_ui.summary = None;
                                self.verify_ui.error = Some(e.clone());
                            }
                            format!("失败：{e}")
                        }
                    };
                    self.last_summary = Some(if self.backend.is_demo() {
                        format!("【演示合成结果】{summary}")
                    } else {
                        summary
                    });
                    if self.task_kind == Some(TaskKind::Watch) {
                        self.watch_ui.last_msg = self.last_summary.clone();
                    }
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
                self.task_kind = None;
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
                        TaskOutcome::Done(planned) => {
                            if planned.inputs.cfg != self.cfg
                                || self.archive_ui.to_options().ok().as_ref()
                                    != Some(&planned.inputs.opts)
                            {
                                self.archive_plan = None;
                                self.archive_plan_inputs = None;
                                self.logs.push((
                                    LogLevel::Warn,
                                    "预览期间配置或选项已改变，结果已丢弃，请重新演练。".into(),
                                ));
                            } else {
                                self.archive_plan_inputs = Some(planned.inputs);
                                self.archive_plan = Some(planned.plan);
                            }
                        }
                        TaskOutcome::Failed(e) => {
                            self.archive_plan = None;
                            self.archive_plan_inputs = None;
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
        match self.find_task.as_ref().map(|t| t.is_finished()) {
            Some(true) => {
                if let Some(outcome) = self.find_task.as_mut().and_then(|t| t.take_outcome()) {
                    match outcome {
                        TaskOutcome::Done(result) => {
                            self.find_ui.result = Some(result);
                            self.find_ui.error = None;
                        }
                        TaskOutcome::Failed(e) => {
                            self.find_ui.result = None;
                            self.find_ui.error = Some(format!("查找失败：{}", e));
                        }
                    }
                }
                self.find_task = None;
            }
            Some(false) => ctx.request_repaint(),
            None => {}
        }
    }

    /// 是否有进行中的后台任务(执行或计划预览)。导航/按钮据此禁用。
    pub fn is_busy(&self) -> bool {
        self.task.is_some() || self.plan_task.is_some() || self.find_task.is_some()
    }

    pub fn task_running(&self, kind: TaskKind) -> bool {
        self.task.is_some() && self.task_kind == Some(kind)
    }

    pub fn busy_label(&self) -> &'static str {
        if self.plan_task.is_some() {
            "归档预览进行中"
        } else if self.find_task.is_some() {
            "索引查询进行中"
        } else {
            match self.task_kind {
                Some(TaskKind::Archive) => "归档任务进行中",
                Some(TaskKind::Verify) => "复查任务进行中",
                Some(TaskKind::Watch) => "监视任务进行中",
                None => "任务进行中",
            }
        }
    }

    pub fn ensure_idle(&mut self) -> bool {
        if self.is_busy() {
            self.logs.push((
                LogLevel::Warn,
                "已有任务拥有当前日志与进度；请等待结束或取消后再发起操作。".into(),
            ));
            false
        } else {
            true
        }
    }

    pub fn operation_label(&self, label: &str) -> String {
        if self.backend.is_demo() {
            format!("【演示模拟】{label}")
        } else {
            label.into()
        }
    }

    /// 底部全局状态栏: 结构化呈现阶段、项目、真实进度、已用时间及取消入口。
    fn status_bar(&mut self, ctx: &egui::Context) {
        use crate::views::theme;
        egui::TopBottomPanel::bottom("status")
            .exact_height(38.0)
            .frame(
                egui::Frame::default()
                    .fill(theme::CARD)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .inner_margin(egui::Margin::symmetric(16, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(task) = &self.task {
                        // Reserve the cancellation control before any variable-length summary.
                        if task.cancel_requested() {
                            theme::badge(ui, "取消中…", theme::WARN, egui::Color32::WHITE);
                        } else if ui.add(theme::btn_danger("停止任务")).clicked() {
                            task.request_cancel();
                        }
                        theme::badge(ui, self.busy_label(), theme::PRIMARY, egui::Color32::WHITE);
                        let elapsed = self
                            .task_started
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let line = self
                            .progress
                            .lock()
                            .ok()
                            .filter(|p| p.active && p.total > 0)
                            .map(|p| {
                                format!(
                                    "{} {:.0}% · 已用 {}",
                                    p.label,
                                    p.current as f32 / p.total as f32 * 100.0,
                                    fmt_dur(elapsed)
                                )
                            })
                            .unwrap_or_else(|| format!("正在处理… · 已用 {}", fmt_dur(elapsed)));
                        ui.add(egui::Label::new(&line).truncate())
                            .on_hover_text(line);
                    } else if self.plan_task.is_some() || self.find_task.is_some() {
                        ui.spinner();
                        ui.label(self.busy_label());
                    } else {
                        if ui.add(theme::btn_primary("快速归档备份")).clicked() {
                            self.view = View::Archive;
                        }
                        ui.colored_label(theme::TEXT_MUTED, "就绪");
                        if let Some(summary) = &self.last_summary {
                            ui.add(egui::Label::new(summary).truncate())
                                .on_hover_text(summary);
                        }
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

/// 扁平现代导航项: 选中→浅蓝背景 + 细竖条指示; hover→弱浅底。整行可点。
fn nav_item(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    use crate::views::theme;
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 36.0), egui::Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(
            rect,
            egui::CornerRadius::same(theme::RADIUS),
            theme::PRIMARY_SOFT,
        );
        let bar = egui::Rect::from_min_size(rect.min, egui::vec2(3.0, rect.height()));
        p.rect_filled(bar, egui::CornerRadius::same(1), theme::PRIMARY);
    } else if resp.hovered() {
        p.rect_filled(rect, egui::CornerRadius::same(theme::RADIUS), theme::BG);
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
        egui::FontId::new(
            13.0,
            if selected {
                egui::FontFamily::Name("semibold".into())
            } else {
                egui::FontFamily::Proportional
            },
        ),
        color,
    );
    resp
}

impl App {
    pub fn render(&mut self, ctx: &egui::Context) {
        self.pump(ctx);

        egui::SidePanel::left("nav")
            .resizable(false)
            .exact_width(175.0)
            .frame(
                egui::Frame::default()
                    .fill(crate::views::theme::CARD)
                    .stroke(egui::Stroke::new(1.0, crate::views::theme::BORDER))
                    .inner_margin(egui::Margin::symmetric(10, 14)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (icon_rect, _) =
                        ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                    ui.painter().rect_filled(
                        icon_rect,
                        egui::CornerRadius::same(4),
                        crate::views::theme::PRIMARY,
                    );
                    ui.painter().text(
                        icon_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "BF",
                        egui::FontId::new(10.5, egui::FontFamily::Name("semibold".into())),
                        egui::Color32::WHITE,
                    );
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("bftool 备份")
                                .font(egui::FontId::new(
                                    15.0,
                                    egui::FontFamily::Name("semibold".into()),
                                ))
                                .color(crate::views::theme::TEXT_TITLE),
                        );
                    });
                });
                ui.add_space(14.0);

                let current = self.view;
                for v in View::ALL {
                    let selected = current == v;
                    // 运行时允许自由切换页面查看状态，绝不锁定导航；仅在各页面内禁用重复/冲突操作
                    if nav_item(ui, selected, v.label()).clicked() {
                        self.view = v;
                    }
                    ui.add_space(2.0);
                }

                // 底部版本与就绪状态
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION")))
                            .size(11.0)
                            .color(crate::views::theme::TEXT_MUTED),
                    );
                });
            });

        // 全局底部状态栏
        self.status_bar(ctx);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(crate::views::theme::BG)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                if self.backend.is_demo() {
                    crate::views::theme::callout_with_tag(ui, crate::views::theme::WARN, crate::views::theme::WARN_SOFT, "演示模式", "所有数据与任务均为内存合成；不读取真实配置或卷，不写入用户文件。文件选择已禁用。截图仅写入指定输出目录。");
                    ui.add_space(8.0);
                }
                egui::ScrollArea::both().id_salt(("workspace", self.view)).auto_shrink([false, false]).show(ui, |ui| match self.view {
                View::Dashboard => crate::views::dashboard::ui(self, ui),
                View::Archive => crate::views::archive::ui(self, ui),
                View::Drives => crate::views::drives::ui(self, ui),
                View::Find => crate::views::find::ui(self, ui),
                View::Init => crate::views::init::ui(self, ui),
                View::Verify => crate::views::verify::ui(self, ui),
                View::Watch => crate::views::watch::ui(self, ui),
                View::Settings => crate::views::settings::ui(self, ui),
                });
            });

        // 帧末兜底重绘
        if self.is_busy() {
            ctx.request_repaint();
        }

        // 自动化截图流程驱动
        if let Some(mut runner) = self.screenshot_runner.take() {
            runner.step(self, ctx);
            self.screenshot_runner = Some(runner);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn shape_text(shape: &egui::epaint::Shape, text: &mut String) {
        match shape {
            egui::epaint::Shape::Text(t) => {
                text.push_str(t.galley.text());
                text.push('\n');
            }
            egui::epaint::Shape::Vec(v) => {
                for s in v {
                    shape_text(s, text);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn view_text(app: &mut App, view: View) -> String {
        let ctx = headless_context();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 1600.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| match view {
                    View::Watch => crate::views::watch::ui(app, ui),
                    View::Verify => crate::views::verify::ui(app, ui),
                    View::Archive => crate::views::archive::ui(app, ui),
                    _ => unreachable!(),
                });
            },
        );
        let mut text = String::new();
        for s in output.shapes {
            shape_text(&s.shape, &mut text);
        }
        text
    }

    #[test]
    fn regression_cross_page_controls_follow_task_owner() {
        for kind in [TaskKind::Archive, TaskKind::Verify, TaskKind::Watch] {
            let mut app = fixture();
            crate::screenshot::inject_demo_data(&mut app);
            app.task_kind = Some(kind);
            app.task = Some(BackgroundTask::spawn(|cancel| {
                while !cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                Ok(ExecutionResult::Summary("synthetic cancelled".into()))
            }));
            for (view, owner, label) in [
                (View::Archive, TaskKind::Archive, "停止备份"),
                (View::Verify, TaskKind::Verify, "停止复查"),
                (View::Watch, TaskKind::Watch, "停止监视任务"),
            ] {
                let text = view_text(&mut app, view);
                assert_eq!(
                    text.contains(label),
                    kind == owner,
                    "wrong task owner {kind:?}/{view:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn regression_cancelled_zero_report_never_claims_hash_coverage() {
        let mut app = fixture();
        crate::screenshot::inject_demo_data(&mut app);
        app.verify_ui.last_stats = Some(crate::views::verify::VerifyStats {
            cancelled: true,
            ..Default::default()
        });
        app.verify_ui.last_report = Some(VerifyReport {
            cancelled: true,
            ..Default::default()
        });
        let text = view_text(&mut app, View::Verify);
        for assurance in [
            "全部具备哈希校验",
            "无异常未受管文件",
            "未发现比特腐烂或损坏",
        ] {
            assert!(
                !text.contains(assurance),
                "unsupported assurance: {assurance}"
            );
        }
    }

    #[test]
    fn regression_mixed_verification_does_not_invent_corruption_or_integrity() {
        let mut app = fixture();
        crate::screenshot::inject_demo_data(&mut app);
        app.verify_ui.last_stats = Some(crate::views::verify::VerifyStats {
            checked: 10,
            extra: 1,
            size_only: 4,
            ..Default::default()
        });
        let text = view_text(&mut app, View::Verify);
        assert!(!text.contains("文件完整无损"));
        assert!(text.contains("未完整核对内容"));
        app.verify_ui.last_stats = Some(crate::views::verify::VerifyStats {
            bad: 1,
            ..Default::default()
        });
        let text = view_text(&mut app, View::Verify);
        assert!(!text.contains("发现数据损坏"));
        assert!(text.contains("完整性问题"));
    }

    #[test]
    fn regression_settings_directory_layout_fits_narrow_width() {
        let mut app = fixture();
        crate::screenshot::inject_demo_data(&mut app);
        let ctx = headless_context();
        let mut body = egui::Rect::NOTHING;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(660.0, 1600.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    body = ui
                        .scope(|ui| crate::views::settings::ui(&mut app, ui))
                        .response
                        .rect;
                });
            },
        );
        assert!(body.right() <= 660.0, "settings overflowed: {body:?}");
    }

    #[test]
    fn regression_scaled_narrow_cards_and_archive_steps_remain_readable() {
        for view in [View::Dashboard, View::Archive] {
            let mut app = fixture();
            crate::screenshot::inject_demo_data(&mut app);
            let ctx = headless_context();
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(380.0, 2000.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| match view {
                        View::Dashboard => crate::views::dashboard::ui(&mut app, ui),
                        View::Archive => crate::views::archive::ui(&mut app, ui),
                        _ => unreachable!(),
                    });
                },
            );
            let needle = if view == View::Dashboard {
                "87% 已用"
            } else {
                "第二步：正式归档"
            };
            let text = output
                .shapes
                .iter()
                .find_map(|s| match &s.shape {
                    egui::epaint::Shape::Text(t) if t.galley.text() == needle => Some(t),
                    _ => None,
                })
                .expect("expected visible card/step text");
            assert!(
                text.galley.rows.len() <= 2,
                "{needle} compressed into {} rows",
                text.galley.rows.len()
            );
        }
    }

    #[test]
    fn regression_watch_completion_updates_its_own_recent_result() {
        let mut app = fixture();
        app.backend = Backend::Demo;
        app.task_kind = Some(TaskKind::Watch);
        app.watch_ui.last_msg = Some("已启动".into());
        app.task = Some(BackgroundTask::spawn(|_| {
            Ok(ExecutionResult::Summary("synthetic watch cancelled".into()))
        }));
        while !app.task.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        app.pump(&egui::Context::default());
        assert_eq!(app.watch_ui.last_msg.as_ref(), app.last_summary.as_ref());
        assert!(app.watch_ui.last_msg.unwrap().contains("cancelled"));
    }

    pub(crate) fn headless_context() -> egui::Context {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let family = fonts.families[&egui::FontFamily::Proportional].clone();
        fonts
            .families
            .insert(egui::FontFamily::Name("semibold".into()), family);
        ctx.set_fonts(fonts);
        ctx
    }

    pub(crate) fn fixture() -> App {
        App {
            backend: Backend::Live,
            task_kind: None,
            view: View::Dashboard,
            cfg: Config::default(),
            config_source: ConfigSource::Default,
            logs: Vec::new(),
            progress: Arc::new(Mutex::new(ProgressState::default())),
            rx: None,
            task: None,
            plan_task: None,
            find_task: None,
            task_started: None,
            last_summary: None,
            archive_plan: None,
            archive_plan_inputs: None,
            archive_ui: Default::default(),
            drives_cache: None,
            drives_result: None,
            find_ui: Default::default(),
            init_ui: Default::default(),
            verify_ui: Default::default(),
            settings_ui: Default::default(),
            watch_ui: Default::default(),
            status_cache: None,
            screenshot_runner: None,
        }
    }

    pub(crate) fn occupy_find(app: &mut App) {
        app.find_task = Some(
            BackgroundTask::spawn(|cancel| {
                while !cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::yield_now();
                }
                Ok(FindOutcome {
                    matches: Vec::new(),
                    sources_searched: 0,
                    sources_failed: Vec::new(),
                    malformed_rows: 0,
                })
            })
            .detachable(),
        );
    }

    #[test]
    fn regression_real_verification_report_reaches_view_statistics() {
        let mut app = fixture();
        let report = VerifyReport {
            checked: 7,
            bad: 2,
            size_only: 1,
            extra: 3,
            cancelled: true,
            issues: vec![
                bftool_core::engine::verify::VerifyIssue {
                    project: "synthetic".into(),
                    rel: "missing.txt".into(),
                    kind: bftool_core::engine::verify::VerifyIssueKind::Missing,
                },
                bftool_core::engine::verify::VerifyIssue {
                    project: "synthetic".into(),
                    rel: "unverifiable.txt".into(),
                    kind: bftool_core::engine::verify::VerifyIssueKind::Unverifiable,
                },
            ],
            ..Default::default()
        };
        app.task_kind = Some(TaskKind::Verify);
        app.task = Some(BackgroundTask::spawn(move |_| {
            Ok(ExecutionResult::Verification(report))
        }));
        while !app.task.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        app.pump(&egui::Context::default());
        let stats = app.verify_ui.last_stats.unwrap();
        assert_eq!(
            (
                stats.checked,
                stats.bad,
                stats.extra,
                stats.size_only,
                stats.cancelled
            ),
            (7, 2, 3, 1, true)
        );
        assert_eq!(app.verify_ui.last_report.as_ref().unwrap().issues.len(), 2);
        assert!(!app.last_summary.as_ref().unwrap().starts_with("【演示"));
        assert!(app.verify_ui.summary.as_ref().unwrap().contains("取消"));
    }

    #[test]
    fn verification_failure_clears_previous_statistics() {
        let mut app = fixture();
        app.verify_ui.last_stats = Some(Default::default());
        app.verify_ui.last_report = Some(Default::default());
        app.task_kind = Some(TaskKind::Verify);
        app.task = Some(BackgroundTask::spawn(|_| {
            anyhow::bail!("synthetic unreadable manifest")
        }));
        while !app.task.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        app.pump(&egui::Context::default());
        assert!(app.verify_ui.last_stats.is_none());
        assert!(app.verify_ui.last_report.is_none());
        assert!(app.verify_ui.error.as_ref().unwrap().contains("unreadable"));
    }

    #[test]
    fn cold_demo_views_render_without_production_reads_at_all_application_scales() {
        for (width, height) in [(1200.0, 820.0), (880.0, 600.0)] {
            for scale in [1.0, 1.25, 1.5] {
                let mut app = fixture();
                app.backend = Backend::Demo;
                app.cfg = crate::backend::synthetic_config();
                let ctx = headless_context();
                for view in View::ALL {
                    app.view = view;
                    app.status_cache = None;
                    app.drives_cache = None;
                    app.verify_ui.drives = None;
                    app.init_ui.cache = None;
                    let _ = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(width / scale, height / scale),
                            )),
                            ..Default::default()
                        },
                        |ctx| app.render(ctx),
                    );
                    assert_eq!(app.backend, Backend::Demo);
                    assert!(!app.is_busy());
                }
            }
        }
    }

    #[test]
    fn regression_demo_replaces_configuration_and_all_drive_caches() {
        let mut app = fixture();
        crate::screenshot::inject_demo_data(&mut app);
        assert!(
            app.verify_ui.drives.is_some(),
            "demo must not leave verify drive scan cold"
        );
        assert!(app.cfg.ready_root.to_string_lossy().contains("演示"));
        assert!(app.cfg.system_root.to_string_lossy().contains("演示"));
        assert_eq!(
            app.settings_ui.system_root,
            app.cfg.system_root.to_string_lossy()
        );
    }

    #[test]
    fn regression_changed_config_discards_completed_preview() {
        let mut app = fixture();
        crate::screenshot::inject_demo_data(&mut app);
        let plan = app.archive_plan.take().unwrap();
        app.archive_plan_inputs = None;
        let inputs = ArchivePlanInputs {
            cfg: app.cfg.clone(),
            opts: plan.opts.clone(),
        };
        app.plan_task =
            Some(BackgroundTask::spawn(move |_| Ok(PlannedArchive { plan, inputs })).detachable());
        while !app.plan_task.as_ref().unwrap().is_finished() {
            std::thread::yield_now();
        }
        app.cfg.reserve_gb += 1;
        app.pump(&egui::Context::default());
        assert!(
            app.archive_plan.is_none(),
            "old preview must not be rebound to a changed config"
        );
        assert!(app.archive_plan_inputs.is_none());
    }

    #[test]
    fn regression_archive_layout_stays_inside_narrow_viewport() {
        let mut app = fixture();
        crate::screenshot::inject_demo_data(&mut app);
        let ctx = headless_context();
        let mut body_rect = egui::Rect::NOTHING;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(880.0, 600.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    body_rect = ui
                        .scope(|ui| crate::views::archive::ui(&mut app, ui))
                        .response
                        .rect;
                });
            },
        );
        assert!(
            body_rect.right() <= 880.0,
            "archive content overflowed viewport: {body_rect:?}"
        );
    }

    #[test]
    fn default_view_is_dashboard() {
        assert_eq!(View::default(), View::Dashboard);
    }

    #[test]
    fn all_views_have_nonempty_labels() {
        for v in View::ALL {
            assert!(!v.label().is_empty(), "{:?} 应有标签", v);
        }
        // ALL 覆盖 8 个且无重复标签
        assert_eq!(View::ALL.len(), 8);
        let mut labels: Vec<&str> = View::ALL.iter().map(|v| v.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 8, "标签不应重复");
    }
}

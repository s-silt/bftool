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
    /// 进行中的后台任务(None = 空闲)。
    pub task: Option<BackgroundTask>,
    /// 上一个任务的摘要(Done/Failed 文案),供结果区显示。
    pub last_summary: Option<String>,
    /// 备份页的计划预览(archive::plan 的结果);T6 填充。
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
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut logs = Vec::new();
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
            last_summary: None,
            archive_plan: None,
            archive_ui: crate::views::archive::ArchiveUiState::default(),
            drives_cache: None,
            find_ui: crate::views::find::FindUiState::default(),
            init_ui: crate::views::init::InitUiState::default(),
            verify_ui: crate::views::verify::VerifyUiState::default(),
            settings_ui: crate::views::settings::SettingsUiState::default(),
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
        let finished = self.task.as_ref().map(|t| t.is_finished());
        match finished {
            Some(true) => {
                if let Some(outcome) = self.task.as_mut().and_then(|t| t.take_outcome()) {
                    self.last_summary = Some(match outcome {
                        TaskOutcome::Done(s) => s,
                        TaskOutcome::Failed(e) => format!("失败：{}", e),
                    });
                }
                self.task = None;
                self.rx = None;
            }
            Some(false) => ctx.request_repaint(), // 任务进行中:持续刷新进度/日志
            None => {}
        }
    }

    /// 是否有进行中的后台任务(导航/按钮据此禁用)。
    pub fn is_busy(&self) -> bool {
        self.task.is_some()
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);

        egui::SidePanel::left("nav")
            .resizable(false)
            .exact_width(140.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("bftool");
                ui.separator();
                let busy = self.is_busy();
                let current = self.view;
                for v in View::ALL {
                    // 任务进行中禁用导航,避免切走正在跑的备份页(简单稳妥;Phase 3 可放开只读视图)。
                    ui.add_enabled_ui(!busy || v == current, |ui| {
                        if ui.selectable_label(current == v, v.label()).clicked() {
                            self.view = v;
                        }
                    });
                }
            });

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

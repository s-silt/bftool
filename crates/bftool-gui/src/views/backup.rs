//! 3步极简文件备份界面：
//! 1. 选择文件或文件夹（支持多项来源、拖放、名称/路径/大小/移除）
//! 2. 选择机械盘上的目标文件夹（显示完整路径、可用空间、机械盘/目标目录判定、来源与目标关系校验）
//! 3. 预览后开始备份（清晰呈现待复制、已存在、冲突、不可读取、空间不足；同名不得静默覆盖）

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use eframe::egui::{self, Color32};

use crate::app::App;
use crate::task::BackgroundTask;
use crate::views::{theme, util};

#[path = "backup_adapter.rs"]
pub(crate) mod adapter;
pub use adapter::PlannedFileBackup;

/// 来源项目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceItem {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: u64,
    pub folder_filter: FolderFilter,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderFilter {
    pub include_subfolders: bool,
    pub enabled: bool,
    pub extensions_input: String,
    pub include_extensionless: bool,
}
impl FolderFilter {
    pub fn directory_options(
        &self,
    ) -> anyhow::Result<bftool_core::pipeline::backup::DirectoryOptions> {
        let extensions = bftool_core::pipeline::backup::parse_extensions(&self.extensions_input)?;
        Ok(bftool_core::pipeline::backup::DirectoryOptions {
            recursive: self.include_subfolders,
            extensions: self.enabled.then_some(extensions),
            include_extensionless: !self.enabled || self.include_extensionless,
        })
    }
}

/// 目标盘类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDriveKind {
    Hdd,
    Ssd,
    Unknown,
}

impl TargetDriveKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hdd => "机械盘目标",
            Self::Ssd => "目标目录 (固态盘)",
            Self::Unknown => "目标目录",
        }
    }
}

/// 目标路径及磁盘信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInfo {
    pub path: PathBuf,
    pub drive_letter: Option<String>,
    pub drive_kind: TargetDriveKind,
    pub total_space: u64,
    pub available_space: u64,
    pub exists: bool,
}

/// 来源与目标关系校验结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationValidation {
    Valid,
    EmptyTarget,
    TargetNotFound,
    SourceEqualsTarget,
    TargetInsideSource,
    SourceInsideTarget,
    TargetNotWritable,
}

impl RelationValidation {
    pub fn is_ok(self) -> bool {
        self == Self::Valid
    }
}

/// 计划文件状态分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePlanItemStatus {
    PendingCopy,       // 待复制
    AlreadyExists,     // 已存在 (安全跳过)
    Conflict,          // 同名冲突 (同名文件不同内容，不得静默覆盖)
    Unreadable,        // 不可读取 (权限/路径错误)
    InsufficientSpace, // 空间不足
}

impl FilePlanItemStatus {
    pub fn badge_info(self) -> (&'static str, Color32, Color32) {
        match self {
            Self::PendingCopy => ("待复制", theme::PRIMARY, theme::PRIMARY_SOFT),
            Self::AlreadyExists => ("已存在 (跳过)", theme::TEXT_MUTED, theme::TRACK),
            Self::Conflict => ("同名冲突", theme::WARN, theme::WARN_SOFT),
            Self::Unreadable => ("不可读取", theme::DANGER, theme::DANGER_SOFT),
            Self::InsufficientSpace => ("空间不足", theme::DANGER, theme::DANGER_SOFT),
        }
    }
}

/// 计划中的单个条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePlanItem {
    pub source_path: PathBuf,
    pub target_path: PathBuf,
    pub rel_path: String,
    pub size_bytes: u64,
    pub status: FilePlanItemStatus,
    pub reason: String,
}

/// 拟归档备份计划汇总
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBackupPlan {
    pub items: Vec<FilePlanItem>,
    pub pending_count: usize,
    pub pending_bytes: u64,
    pub already_exists_count: usize,
    pub already_exists_bytes: u64,
    pub conflict_count: usize,
    pub conflict_bytes: u64,
    pub unreadable_count: usize,
    pub unreadable_bytes: u64,
    pub has_insufficient_space: bool,
    pub space_needed_bytes: u64,
    pub space_available_bytes: u64,
}

/// 8 类验收状态枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoState {
    BlankInitial,      // 01 空白初始
    SingleFile,        // 02 单文件
    MultiFolder,       // 03 多文件夹
    Conflict,          // 04 同名冲突
    InsufficientSpace, // 05 空间不足
    Running,           // 06 运行中
    Stopped,           // 07 停止后
    PartialFailure,    // 08 部分失败
}

impl DemoState {
    pub const ALL: [DemoState; 8] = [
        DemoState::BlankInitial,
        DemoState::SingleFile,
        DemoState::MultiFolder,
        DemoState::Conflict,
        DemoState::InsufficientSpace,
        DemoState::Running,
        DemoState::Stopped,
        DemoState::PartialFailure,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::BlankInitial => "01 空白初始",
            Self::SingleFile => "02 单文件",
            Self::MultiFolder => "03 多文件夹",
            Self::Conflict => "04 同名冲突",
            Self::InsufficientSpace => "05 空间不足",
            Self::Running => "06 运行中",
            Self::Stopped => "07 停止后",
            Self::PartialFailure => "08 部分失败",
        }
    }
}

/// 备份结果汇总
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupOutcomeKind {
    Success,
    PartialComplete,
    Failed,
    Stopped,
    NotVerified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupOutcomeSummary {
    pub kind: BackupOutcomeKind,
    pub title: String,
    pub detail: String,
    pub completed_items: usize,
    pub completed_bytes: u64,
    pub failed_items: usize,
    pub is_content_verified: bool,
}

/// 备份视图持久状态
#[derive(Debug, Clone)]
pub struct BackupUiState {
    // Executable plans remain private, independent of the editable preview model.
    core_plans: Option<std::sync::Arc<Vec<bftool_core::pipeline::backup::BackupPlan>>>,
    core_signature: String,
    pub sources: Vec<SourceItem>,
    pub target_path: String,
    pub target_info: Option<TargetInfo>,
    pub validation: RelationValidation,
    pub plan: Option<FileBackupPlan>,
    pub plan_signature: String,

    // 运行态与交互状态
    pub is_running: bool,
    pub stopping_requested: bool,
    pub current_file: String,
    pub done_files: usize,
    pub total_files: usize,
    // Recovery has no validated whole-job denominator in the GUI contract.
    pub recovery_totals_unknown: bool,
    pub transferred_bytes: u64,
    pub total_bytes: u64,
    pub speed_bps: Option<u64>,
    pub eta_secs: Option<u64>,

    pub last_outcome: Option<BackupOutcomeSummary>,
    pub show_pending_core_modal: bool,
    pub demo_state: Option<DemoState>,
}

impl Default for BackupUiState {
    fn default() -> Self {
        Self {
            core_plans: None,
            core_signature: String::new(),
            sources: Vec::new(),
            target_path: String::new(),
            target_info: None,
            validation: RelationValidation::EmptyTarget,
            plan: None,
            plan_signature: String::new(),
            is_running: false,
            stopping_requested: false,
            current_file: String::new(),
            done_files: 0,
            total_files: 0,
            recovery_totals_unknown: false,
            transferred_bytes: 0,
            total_bytes: 0,
            speed_bps: None,
            eta_secs: None,
            last_outcome: None,
            show_pending_core_modal: false,
            demo_state: None,
        }
    }
}

impl BackupUiState {
    pub(crate) fn running_progress_text(&self) -> String {
        let fraction = if self.total_bytes > 0 {
            (self.transferred_bytes as f32 / self.total_bytes as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if self.recovery_totals_unknown {
            if self.total_bytes == 0 {
                "恢复任务：整项总量未知；等待当前步骤字节进度".into()
            } else {
                format!(
                    "恢复任务：整项总量未知；当前步骤字节 {} / {} ({:.1}%)",
                    util::fmt_gb(self.transferred_bytes),
                    util::fmt_gb(self.total_bytes),
                    fraction * 100.0
                )
            }
        } else {
            format!(
                "整批最终计数（结束后更新）{}/{} · 当前源字节 {} / {} ({:.1}%)",
                self.done_files,
                self.total_files,
                util::fmt_gb(self.transferred_bytes),
                util::fmt_gb(self.total_bytes),
                fraction * 100.0
            )
        }
    }

    /// 计算当前输入快照签名，用于探测输入变更时失效旧计划
    pub fn current_signature(&self) -> String {
        format!("{:?}=>{:?}", self.sources, self.target_path)
    }

    pub fn filter_error(&self) -> Option<String> {
        self.sources.iter().filter(|s| s.is_dir).find_map(|s| {
            s.folder_filter
                .directory_options()
                .err()
                .map(|e| format!("{}: {e}", s.name))
        })
    }

    pub fn has_no_matches(&self) -> bool {
        self.core_plans
            .as_ref()
            .is_some_and(|plans| plans.iter().any(adapter::has_no_matches))
    }

    /// 输入变更时检查并失效预览。
    pub fn check_invalidate_plan(&mut self) {
        let sig = self.current_signature();
        if sig != self.plan_signature {
            self.plan = None;
            for source in &mut self.sources {
                source.size_bytes = 0;
            }
            self.core_plans = None;
            self.core_signature.clear();
            self.plan_signature.clear();
        }
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    // 监听拖放文件事件 (Windows 拖放来源)
    handle_drag_and_drop(app, ui.ctx());

    // 顶部模式标识与说明 (无无意义 KPI，以操作为中心)
    render_header(app, ui);

    ui.add_space(theme::GAP);

    // 步骤 1 与 步骤 2 响应式布局：宽窗口左右分栏，窄窗口垂直排布
    let available_w = ui.available_width();
    if available_w >= 880.0 {
        ui.columns(2, |cols| {
            render_step_1(app, &mut cols[0]);
            render_step_2(app, &mut cols[1]);
        });
    } else {
        ui.vertical(|ui| {
            render_step_1(app, ui);
            ui.add_space(theme::GAP);
            render_step_2(app, ui);
        });
    }

    ui.add_space(theme::GAP);

    // 步骤 3：计划预览、状态明细与开始/停止备份
    render_step_3(app, ui);

    // 正式模式未接入核心弹窗提示
    if app.backup_ui.show_pending_core_modal {
        render_pending_core_modal(app, ui.ctx());
    }
}

/// 顶部信息栏与演示状态预设切换条
fn render_header(app: &mut App, ui: &mut egui::Ui) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("文件备份与归档")
                    .font(theme::h1_font())
                    .color(theme::TEXT_TITLE),
            );
            ui.add_space(8.0);
            if app.backend.is_demo() {
                theme::badge(
                    ui,
                    "【演示模式 · 纯内存示例数据】",
                    theme::PRIMARY,
                    Color32::WHITE,
                );
            } else {
                theme::badge(
                    ui,
                    "安全备份模式 (保留源文件 · 禁止静默覆盖)",
                    theme::OK_SOFT,
                    theme::OK,
                );
            }
        });
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("把固态盘里的文件或文件夹复制备份到机械盘。无需初始化磁盘或建立盘池，直选来源与目标即可安全备份。")
                .size(12.5)
                .color(theme::TEXT_MUTED),
        );

        // 若处于演示模式，展示 8 类状态快捷调试切换条（便于验收检查）
        if app.backend.is_demo() {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("示例数据验收状态：").size(11.5).color(theme::TEXT_MUTED));
                for state in DemoState::ALL {
                    let active = app.backup_ui.demo_state == Some(state);
                    if ui.add_enabled(!app.is_busy(), egui::Button::new(state.label()).selected(active)).clicked() {
                        apply_demo_state(app, state);
                    }
                }
            });
        }
    });
}

/// 步骤 1：选择文件或文件夹 (来源列表)
fn render_step_1(app: &mut App, ui: &mut egui::Ui) {
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("步骤 1：选择文件或文件夹 (来源)")
                    .font(theme::title_font())
                    .color(theme::TEXT_TITLE),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let total_bytes: u64 = app.backup_ui.sources.iter().map(|s| s.size_bytes).sum();
                ui.label(
                    egui::RichText::new(format!(
                        "共 {} 项 · {}",
                        app.backup_ui.sources.len(),
                        util::fmt_gb(total_bytes)
                    ))
                    .size(11.5)
                    .color(theme::TEXT_MUTED),
                );
            });
        });
        ui.add_space(6.0);

        // 操作入口栏：添加文件、添加文件夹、清空
        ui.horizontal(|ui| {
            let busy = !selection_allowed(app);
            if ui
                .add_enabled(!busy, theme::btn_secondary("＋ 添加文件"))
                .on_hover_text("从固态盘或本地路径选择单个或多个文件")
                .clicked()
            {
                if let Some(files) = rfd::FileDialog::new().pick_files() {
                    for f in files {
                        add_source_path(app, f);
                    }
                }
            }

            if ui
                .add_enabled(!busy, theme::btn_secondary("＋ 添加文件夹"))
                .on_hover_text("选择整个素材或项目文件夹")
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    add_source_folder(app, folder);
                }
            }

            if !app.backup_ui.sources.is_empty()
                && ui
                    .add_enabled(!busy, theme::btn_secondary("清空来源"))
                    .clicked()
            {
                app.backup_ui.sources.clear();
                app.backup_ui.check_invalidate_plan();
            }
        });

        ui.add_space(8.0);

        // 来源列表容器
        if app.backup_ui.sources.is_empty() {
            util::empty_state(
                ui,
                "未添加来源项目",
                "点击上方「添加文件」或「添加文件夹」，支持将文件/文件夹直接拖拽至窗口。",
            );
        } else {
            let busy = app.is_busy() || app.backup_ui.is_running;
            let mut remove_idx = None;
            egui::ScrollArea::vertical()
                .id_salt("source_list_scroll")
                .max_height(200.0)
                .show(ui, |ui| {
                    let mut edits = Vec::new();
                    let preview_current = app.backup_ui.plan.is_some();
                    for (i, item) in app.backup_ui.sources.clone().iter().enumerate() {
                        ui.group(|ui| {
                            ui.horizontal(|ui| {
                                let icon = if item.is_dir { "📁" } else { "📄" };
                                ui.label(egui::RichText::new(icon).size(14.0));
                                ui.vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new(&item.name)
                                            .font(theme::subtitle_font())
                                            .color(theme::TEXT_TITLE),
                                    );
                                    let full_p = item.path.display().to_string();
                                    util::copyable_path(ui, &full_p, 36);
                                });

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let btn = ui.add_enabled(
                                            !busy,
                                            egui::Button::new(
                                                egui::RichText::new("移除")
                                                    .size(11.0)
                                                    .color(theme::DANGER),
                                            )
                                            .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                                            .corner_radius(3),
                                        );
                                        if btn.clicked() {
                                            remove_idx = Some(i);
                                        }
                                        ui.label(
                                            egui::RichText::new(if preview_current { util::fmt_gb(item.size_bytes) } else { "待预览".into() })
                                                .size(12.0)
                                                .color(theme::TEXT_MUTED),
                                        );
                                    },
                                );
                            });
                            if item.is_dir {
                                let mut draft = item.folder_filter.clone();
                                ui.push_id(i, |ui| render_folder_filter(ui, &mut draft, !busy));
                                if draft != item.folder_filter { edits.push((i, draft)); }
                            } else if !preview_current {
                                ui.colored_label(theme::TEXT_MUTED, "拖入的文件夹将在首次预览识别；文件夹默认仅当前层，单文件直接备份。");
                            }
                        });
                        ui.add_space(2.0);
                    }
                    for (i, draft) in edits { set_folder_filter(app, i, draft); }
                });

            if let Some(i) = remove_idx {
                app.backup_ui.sources.remove(i);
                app.backup_ui.check_invalidate_plan();
            }
        }
    });
}

/// 步骤 2：选择机械盘上的目标文件夹
fn render_step_2(app: &mut App, ui: &mut egui::Ui) {
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("步骤 2：选择目标文件夹 (存储机械盘)")
                    .font(theme::title_font())
                    .color(theme::TEXT_TITLE),
            );
            if let Some(info) = &app.backup_ui.target_info {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    theme::badge(
                        ui,
                        info.drive_kind.label(),
                        theme::PRIMARY_SOFT,
                        theme::PRIMARY,
                    );
                });
            }
        });
        ui.add_space(6.0);

        // 目标路径选择入口
        let busy = !selection_allowed(app);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!busy, theme::btn_secondary("选择目标文件夹"))
                .on_hover_text("浏览选择机械盘或大容量存储上的备份目录")
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    set_target_folder(app, folder);
                }
            }

            if !app.backup_ui.target_path.is_empty() {
                let full_p = app.backup_ui.target_path.clone();
                util::copyable_path(ui, &full_p, 34);
            } else {
                ui.colored_label(theme::TEXT_MUTED, "尚未选择目标目录");
            }
        });

        ui.add_space(8.0);

        // 目标盘容量与空间展示
        if let Some(info) = &app.backup_ui.target_info {
            ui.group(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        let drive_label = info
                            .drive_letter
                            .as_deref()
                            .map(|l| format!("{l}: "))
                            .unwrap_or_default();
                        ui.label(
                            egui::RichText::new(format!(
                                "目标驱动器：{drive_label}{}",
                                info.drive_kind.label()
                            ))
                            .font(theme::subtitle_font())
                            .color(theme::TEXT_TITLE),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "可用 {} / 总共 {}",
                                    if info.total_space > 0 {
                                        util::fmt_gb(info.available_space)
                                    } else {
                                        "未知".into()
                                    },
                                    if info.total_space > 0 {
                                        util::fmt_gb(info.total_space)
                                    } else {
                                        "未知".into()
                                    }
                                ))
                                .size(11.5)
                                .color(theme::TEXT_MUTED),
                            );
                        });
                    });

                    ui.add_space(2.0);
                    let frac = if info.total_space > 0 {
                        (info.total_space.saturating_sub(info.available_space)) as f32
                            / info.total_space as f32
                    } else {
                        0.0
                    };
                    theme::hbar(ui, frac, theme::PRIMARY);
                });
            });
        }

        ui.add_space(6.0);

        // 来源与目标关系实时校验 Banner
        render_relation_validation(app, ui);
    });
}

/// 关系校验结果提示
fn render_relation_validation(app: &App, ui: &mut egui::Ui) {
    match app.backup_ui.validation {
        RelationValidation::Valid => {
            theme::callout_with_tag(
                ui,
                theme::OK,
                theme::OK_SOFT,
                "校验通过",
                "路径初步关系正常；生成后台核心计划后确认身份、内容及权限。",
            );
        }
        RelationValidation::EmptyTarget => {
            theme::callout_with_tag(
                ui,
                theme::TEXT_MUTED,
                theme::TRACK,
                "等待目标",
                "请选择机械盘上的目标备份文件夹以进行关系校验与可用空间测算。",
            );
        }
        RelationValidation::TargetNotFound => {
            theme::callout_with_tag(
                ui,
                theme::DANGER,
                theme::DANGER_SOFT,
                "路径不存在",
                "所选目标目录在磁盘上不存在或无法访问，请重新选择有效目录。",
            );
        }
        RelationValidation::SourceEqualsTarget => {
            theme::callout_with_tag(
                ui,
                theme::DANGER,
                theme::DANGER_SOFT,
                "关系错误",
                "所选来源目录与目标目录相同！禁止将目录备份至其自身。",
            );
        }
        RelationValidation::TargetInsideSource => {
            theme::callout_with_tag(
                ui,
                theme::DANGER,
                theme::DANGER_SOFT,
                "死循环警告",
                "目标文件夹位于来源文件夹内部！此结构会导致递归自我复制死循环，必须更换目标路径。",
            );
        }
        RelationValidation::SourceInsideTarget => {
            theme::callout_with_tag(
                ui,
                theme::WARN,
                theme::WARN_SOFT,
                "嵌套提示",
                "来源文件夹位于目标文件夹内部。请确认备份层次规划，防止混合归档。",
            );
        }
        RelationValidation::TargetNotWritable => {
            theme::callout_with_tag(
                ui,
                theme::DANGER,
                theme::DANGER_SOFT,
                "只读或无写权限",
                "目标文件夹只读或无写入权限，请检查磁盘写保护状态与 Windows 用户权限。",
            );
        }
    }
}

/// 步骤 3：预览后开始备份 (核心状态交互与表格)
fn render_step_3(app: &mut App, ui: &mut egui::Ui) {
    theme::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("步骤 3：计划预览与执行备份")
                    .font(theme::title_font())
                    .color(theme::TEXT_TITLE),
            );

            // 操作按钮群
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy = app.is_busy() || app.backup_ui.is_running;
                let has_plan = app.backup_ui.plan.is_some();
                let can_plan = !busy
                    && !app.backup_ui.sources.is_empty()
                    && app.backup_ui.validation.is_ok()
                    && app.backup_ui.filter_error().is_none();
                let can_start = !busy
                    && app.backup_ui.filter_error().is_none()
                    && !app.backup_ui.has_no_matches()
                    && has_plan
                    && app
                        .backup_ui
                        .plan
                        .as_ref()
                        .map(|p| {
                            !p.has_insufficient_space
                                && (p.pending_count > 0 || !p.items.is_empty())
                        })
                        .unwrap_or(false);

                // 开始备份按钮
                let start_btn = ui.add_enabled(!busy && can_start, theme::btn_primary("开始备份"));
                if start_btn.clicked() {
                    start_backup_action(app);
                }

                // 刷新/生成计划按钮
                let plan_btn = ui.add_enabled(
                    can_plan,
                    theme::btn_secondary(if has_plan {
                        "刷新计划预览"
                    } else {
                        "生成备份计划"
                    }),
                );
                if plan_btn.clicked() {
                    generate_plan_action(app);
                }

                if app.backup_ui.plan.is_none()
                    && !app.backup_ui.sources.is_empty()
                    && app.backup_ui.validation.is_ok()
                {
                    ui.label(
                        egui::RichText::new("请先点击右侧生成计划")
                            .size(11.5)
                            .color(theme::PRIMARY),
                    );
                }
            });
        });

        ui.add_space(8.0);

        if app.file_backup_plan_task.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("后台生成计划：检查来源内容与目标身份…");
                if ui
                    .add_enabled(
                        !app.backup_ui.stopping_requested,
                        theme::btn_danger("取消计划"),
                    )
                    .clicked()
                {
                    request_stop_backup(app);
                }
            });
        }

        // 若处于执行中或停止状态，优先呈现执行监控卡片
        if app.backup_ui.is_running || app.backup_ui.stopping_requested {
            render_running_card(app, ui);
            ui.add_space(8.0);
        }

        // 上次完成结果提示
        if let Some(outcome) = &app.backup_ui.last_outcome {
            render_outcome_banner(outcome, ui);
            ui.add_space(6.0);
        }

        // 计划汇总胶囊标签 (5 大分类)
        if let Some(error) = app.backup_ui.filter_error() {
            ui.colored_label(theme::DANGER, format!("后缀输入无效：{error}"));
        }
        if app.backup_ui.has_no_matches() {
            ui.colored_label(
                theme::WARN,
                "没有匹配文件：该文件夹为 0 文件 / 0 字节；请修改筛选或移除该源后重新预览。",
            );
        }
        if let Some(plan) = &app.backup_ui.plan {
            if !app.backend.is_demo() {
                ui.label("容量数字仅含文件内容；元数据和文件系统分配开销未知，0 字节文件也需要空间，不能据此保证空间充足。");
            }
            ui.horizontal_wrapped(|ui| {
                theme::badge(
                    ui,
                    &format!(
                        "待复制: {} 项 ({})",
                        plan.pending_count,
                        util::fmt_gb(plan.pending_bytes)
                    ),
                    theme::PRIMARY_SOFT,
                    theme::PRIMARY,
                );
                ui.add_space(4.0);
                theme::badge(
                    ui,
                    &format!(
                        "已存在: {} 项 ({})",
                        plan.already_exists_count,
                        util::fmt_gb(plan.already_exists_bytes)
                    ),
                    theme::TRACK,
                    theme::TEXT_MUTED,
                );
                ui.add_space(4.0);
                theme::badge(
                    ui,
                    &format!(
                        "同名冲突: {} 项 ({})",
                        plan.conflict_count,
                        util::fmt_gb(plan.conflict_bytes)
                    ),
                    theme::WARN_SOFT,
                    theme::WARN,
                );
                ui.add_space(4.0);
                theme::badge(
                    ui,
                    &format!(
                        "不可读取: {} 项 ({})",
                        plan.unreadable_count,
                        util::fmt_gb(plan.unreadable_bytes)
                    ),
                    theme::DANGER_SOFT,
                    theme::DANGER,
                );
                if plan.has_insufficient_space {
                    ui.add_space(4.0);
                    theme::badge(
                        ui,
                        &format!(
                            "空间不足! 缺口 {}",
                            util::fmt_gb(
                                plan.space_needed_bytes
                                    .saturating_sub(plan.space_available_bytes)
                            )
                        ),
                        theme::DANGER,
                        Color32::WHITE,
                    );
                }
            });

            // 同名冲突与禁止静默覆盖说明
            if plan.conflict_count > 0 {
                ui.add_space(6.0);
                theme::callout_with_tag(
                    ui,
                    theme::WARN,
                    theme::WARN_SOFT,
                    "同名冲突防覆盖机制",
                    "已有同名项将保留；本次写入预览显示的新版本名（整文件夹保留双份）。只支持保留双份；拒绝冲突/跳过既有项暂未实现。",
                );
            }

            // 空间不足致命警告
            if plan.has_insufficient_space {
                ui.add_space(6.0);
                theme::callout_with_tag(
                    ui,
                    theme::DANGER,
                    theme::DANGER_SOFT,
                    "目标磁盘空间不足",
                    &format!(
                        "拟写入文件共需约 {}，但目标盘仅余 {} 可用空间（缺少约 {}）。为防写入中断损坏，已禁止开始备份，请清理空间或更换更大目标盘。",
                        util::fmt_gb(plan.space_needed_bytes),
                        util::fmt_gb(plan.space_available_bytes),
                        util::fmt_gb(plan.space_needed_bytes.saturating_sub(plan.space_available_bytes))
                    ),
                );
            }

            ui.add_space(8.0);

            // 详细拟复制文件列表
            egui::ScrollArea::both()
                .id_salt("backup_plan_items_scroll")
                .max_height(240.0)
                .show(ui, |ui| {
                    egui::Grid::new("plan_items_grid")
                        .striped(true)
                        .num_columns(5)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.strong("源文件 / 相对路径");
                            ui.strong("目标完整路径");
                            ui.strong("文件大小");
                            ui.strong("计划动作状态");
                            ui.strong("动作说明");
                            ui.end_row();

                            for item in &plan.items {
                                let src_display = item.rel_path.clone();
                                util::copyable_path(ui, &src_display, 28);

                                let dst_display = item.target_path.display().to_string();
                                util::copyable_path(ui, &dst_display, 28);

                                ui.label(util::fmt_gb(item.size_bytes));

                                let (label, fg, bg) = item.status.badge_info();
                                theme::badge(ui, label, bg, fg);

                                ui.colored_label(theme::TEXT_BODY, &item.reason);
                                ui.end_row();
                            }
                        });
                });
        } else if !app.backup_ui.is_running {
            util::empty_state(
                ui,
                "尚未生成计划预览",
                "选择来源与目标目录后，点击右上角「生成备份计划」获取待复制、冲突与空间校验清单。",
            );
        }
    });
}

/// 运行中状态卡片
fn render_running_card(app: &mut App, ui: &mut egui::Ui) {
    theme::card_subtle(ui, |ui| {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                if app.backup_ui.stopping_requested {
                    theme::badge(
                        ui,
                        "停止请求中（等待安全检查点响应）",
                        theme::WARN,
                        Color32::WHITE,
                    );
                } else {
                    ui.spinner();
                    theme::badge(ui, if app.backup_ui.recovery_totals_unknown { "正在恢复备份…" } else { "正在备份复制中" }, theme::PRIMARY, Color32::WHITE);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !app.backup_ui.stopping_requested {
                        let stop_btn = ui.add_enabled(
                            app.task_running(crate::app::TaskKind::Backup),
                            theme::btn_danger("停止备份"),
                        );
                        if stop_btn.clicked() {
                            request_stop_backup(app);
                        }
                        stop_btn.on_hover_text(
                            "在复制或校验的安全检查点响应取消；源保留。当前未完成文件可能需要重新复制，已完成收据须复验后才可跳过。",
                        );
                    }
                });
            });

            ui.add_space(4.0);

            // 当前文件
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("当前阶段 / 文件：")
                        .size(12.0)
                        .color(theme::TEXT_MUTED),
                );
                let curr = if app.backup_ui.current_file.is_empty() {
                    if app.backup_ui.recovery_totals_unknown { "正在读取已保存的恢复任务…".into() } else { "正在初始化文件读取...".into() }
                } else {
                    app.backup_ui.current_file.clone()
                };
                ui.label(
                    egui::RichText::new(curr)
                        .size(12.0)
                        .color(theme::TEXT_TITLE),
                );
            });

            // 文件数与字节进度条
            let frac = if app.backup_ui.total_bytes > 0 {
                (app.backup_ui.transferred_bytes as f32 / app.backup_ui.total_bytes as f32)
                    .clamp(0.0, 1.0)
            } else {
                0.0
            };
            ui.add_space(2.0);
            theme::hbar(ui, frac, theme::PRIMARY);

            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(app.backup_ui.running_progress_text())
                    .size(11.5)
                    .color(theme::TEXT_MUTED),
                );

                // 速度与剩余时间仅在具有真实测量数据时展示
                if let (Some(bps), Some(eta)) = (app.backup_ui.speed_bps, app.backup_ui.eta_secs) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let speed_mb = bps as f64 / 1024.0 / 1024.0;
                        let eta_min = eta / 60;
                        let eta_sec = eta % 60;
                        ui.label(
                            egui::RichText::new(format!(
                                "【合成示例】{:.1} MB/s · 示例剩余 {:02}:{:02}",
                                speed_mb, eta_min, eta_sec
                            ))
                            .size(11.5)
                            .color(theme::PRIMARY),
                        );
                    });
                } else {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new("当前未提供实测速率 / ETA")
                                .size(11.5)
                                .color(theme::TEXT_MUTED),
                        );
                    });
                }
            });
        });
    });
}

/// 结果状态展示条
fn render_outcome_banner(outcome: &BackupOutcomeSummary, ui: &mut egui::Ui) {
    let (tag, color, soft) = match outcome.kind {
        BackupOutcomeKind::Success => ("备份成功", theme::OK, theme::OK_SOFT),
        BackupOutcomeKind::PartialComplete => ("部分完成", theme::WARN, theme::WARN_SOFT),
        BackupOutcomeKind::Failed => ("备份失败", theme::DANGER, theme::DANGER_SOFT),
        BackupOutcomeKind::Stopped => ("已安全停止", theme::TEXT_MUTED, theme::TRACK),
        BackupOutcomeKind::NotVerified => ("未校验", theme::WARN, theme::WARN_SOFT),
    };

    egui::Frame::default()
        .fill(soft)
        .stroke(egui::Stroke::new(1.0_f32, color))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    theme::badge(ui, tag, color, Color32::WHITE);
                    ui.label(
                        egui::RichText::new(&outcome.title)
                            .font(theme::subtitle_font())
                            .color(theme::TEXT_TITLE),
                    );
                });
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(&outcome.detail)
                        .size(12.0)
                        .color(theme::TEXT_BODY),
                );
            });
        });
}

/// 正式入口未接入弹窗
fn render_pending_core_modal(app: &mut App, ctx: &egui::Context) {
    egui::Window::new("核心备份引擎对接说明")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .frame(
            egui::Frame::default()
                .fill(theme::CARD)
                .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                .corner_radius(egui::CornerRadius::same(theme::RADIUS))
                .inner_margin(egui::Margin::same(16)),
        )
        .show(ctx, |ui| {
            ui.set_max_width(460.0);
            theme::callout_with_tag(
                ui,
                theme::PRIMARY,
                theme::PRIMARY_SOFT,
                "功能待接入",
                "由于核心开发者的底层文件复制与哈希通道正在独立调整，正式入口在此阶段保持安全封锁：不伪造成功、不偷偷初始化磁盘、不绕过安全检查。",
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("您可以：\n1. 点击下方按钮切换到「纯内存演示模式」，验证全部 8 种备份交互与边界状态；\n2. 查阅项目文档中的核心开发接口需求清单。")
                    .size(12.5)
                    .color(theme::TEXT_BODY),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!app.is_busy(), theme::btn_primary("切换为演示模式体验")).clicked() {
                    switch_to_demo_if_idle(app);
                }
                if ui.add(theme::btn_secondary("知道了，保持当前状态")).clicked() {
                    app.backup_ui.show_pending_core_modal = false;
                }
            });
        });
}

fn switch_to_demo_if_idle(app: &mut App) {
    if app.is_busy() {
        return;
    }
    app.task_kind = None;
    app.task_started = None;
    app.rx = None;
    app.logs.clear();
    if let Ok(mut progress) = app.progress.lock() {
        *progress = crate::reporter::ProgressState::default();
    }
    app.archive_ui = Default::default();
    app.find_ui = Default::default();
    app.init_ui = Default::default();
    app.verify_ui = Default::default();
    app.settings_ui = Default::default();
    app.watch_ui = Default::default();
    app.tasks_ui = Default::default();
    app.drives_result = None;
    app.backend = crate::backend::Backend::Demo;
    app.backup_ui = BackupUiState::default();
    crate::screenshot::inject_demo_data(app);
    apply_demo_state(app, DemoState::SingleFile);
}

// ── 内部辅助操作与数据流 ──

fn handle_drag_and_drop(app: &mut App, ctx: &egui::Context) {
    let dropped = ctx.input(|i| i.raw.dropped_files.clone());
    for file in dropped {
        if let Some(path) = file.path {
            add_source_path(app, path);
        }
    }
}

fn render_folder_filter(ui: &mut egui::Ui, draft: &mut FolderFilter, editable: bool) {
    ui.add_enabled_ui(editable, |ui| {
        ui.checkbox(&mut draft.include_subfolders, "包含子文件夹");
        ui.colored_label(
            theme::TEXT_MUTED,
            if draft.include_subfolders {
                "当前文件夹及子文件夹（递归）"
            } else {
                "仅当前层（不包含子文件夹）"
            },
        );
        ui.checkbox(&mut draft.enabled, "按文件后缀筛选");
        ui.add_enabled_ui(draft.enabled, |ui| {
            ui.label("常用后缀");
            let parsed = bftool_core::pipeline::backup::parse_extensions(&draft.extensions_input);
            ui.horizontal_wrapped(|ui| {
                for suffix in ["tar", "zip", "jpg", "png", "mp4", "mov", "pdf", "txt"] {
                    let mut chosen = parsed
                        .as_ref()
                        .is_ok_and(|values| values.iter().any(|s| s == suffix));
                    if ui
                        .add_enabled(
                            parsed.is_ok(),
                            egui::Checkbox::new(&mut chosen, format!(".{suffix}")),
                        )
                        .changed()
                    {
                        let mut values = bftool_core::pipeline::backup::parse_extensions(
                            &draft.extensions_input,
                        )
                        .unwrap_or_default();
                        values.retain(|s| s != suffix);
                        if chosen {
                            values.push(suffix.into());
                        }
                        values.sort();
                        draft.extensions_input = values.join(", ");
                    }
                }
            });
            ui.label("手动后缀（逗号 / 空格分隔；a.tar.gz 的后缀为 gz）");
            ui.add(
                egui::TextEdit::singleline(&mut draft.extensions_input)
                    .hint_text("tar, zip")
                    .char_limit(8192)
                    .desired_width(ui.available_width()),
            );
            ui.checkbox(&mut draft.include_extensionless, "包含无后缀文件");
        });
        if let Err(error) = draft.directory_options() {
            ui.colored_label(theme::DANGER, format!("后缀输入无效：{error}"));
        }
        if ui.small_button("清除筛选（保留子文件夹范围）").clicked() {
            draft.enabled = false;
            draft.extensions_input.clear();
            draft.include_extensionless = false;
        }
    });
}

pub fn selection_allowed(app: &App) -> bool {
    !app.backend.is_demo() && !app.is_busy() && !app.backup_ui.is_running
}

pub fn add_source_folder(app: &mut App, path: PathBuf) {
    add_source_selection(app, path, true);
}

pub fn set_folder_filter(app: &mut App, index: usize, draft: FolderFilter) {
    if !selection_allowed(app) {
        return;
    }
    if let Some(source) = app.backup_ui.sources.get_mut(index) {
        source.folder_filter = draft;
        app.backup_ui.check_invalidate_plan();
        app.backup_ui.last_outcome = None;
    }
}

pub fn add_source_path(app: &mut App, path: PathBuf) {
    add_source_selection(app, path, false);
}

fn add_source_selection(app: &mut App, path: PathBuf, folder_hint: bool) {
    if !selection_allowed(app) {
        return;
    }
    if app.backup_ui.sources.iter().any(|s| s.path == path) {
        return;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    // Classification, directory traversal and hashes belong to the plan worker.
    app.backup_ui.sources.push(SourceItem {
        path,
        name,
        is_dir: folder_hint,
        size_bytes: 0,
        folder_filter: FolderFilter::default(),
    });
    revalidate(app);
    app.backup_ui.check_invalidate_plan();
}

pub fn set_target_folder(app: &mut App, path: PathBuf) {
    if !selection_allowed(app) {
        return;
    }
    app.backup_ui.target_path = path.display().to_string();
    app.backup_ui.target_info = None;
    app.verify_ui.invalidate_plain();
    app.last_summary = None;
    revalidate(app);
    app.backup_ui.check_invalidate_plan();
}

pub fn revalidate(app: &mut App) {
    if app.backend.is_demo() || app.is_busy() {
        return;
    }
    app.backup_ui.validation = validate_relation(
        &app.backup_ui.sources,
        Path::new(&app.backup_ui.target_path),
    );
}

pub fn validate_relation(sources: &[SourceItem], target: &Path) -> RelationValidation {
    if target.as_os_str().is_empty() {
        return RelationValidation::EmptyTarget;
    }
    for source in sources {
        if source.path == target {
            return RelationValidation::SourceEqualsTarget;
        }
        if target.starts_with(&source.path) {
            return RelationValidation::TargetInsideSource;
        }
        if source.path.starts_with(target) {
            return RelationValidation::SourceInsideTarget;
        }
    }
    // Pure lexical hint; core checks existence, canonical overlap and identity in its worker.
    RelationValidation::Valid
}

#[cfg(windows)]
fn selected_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            path: *const u16,
            available: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
    }
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return None;
    }
    wide.push(0);
    let (mut available, mut total, mut free) = (0, 0, 0);
    // Windows reads space for this selected directory only; no volume enumeration.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) };
    (ok != 0).then_some((available, total))
}
#[cfg(not(windows))]
fn selected_space(_path: &Path) -> Option<(u64, u64)> {
    None
}

pub fn query_target_info(path: &Path) -> TargetInfo {
    let (available_space, total_space) = selected_space(path).unwrap_or((0, 0));
    TargetInfo {
        path: path.to_path_buf(),
        drive_letter: None,
        drive_kind: TargetDriveKind::Unknown,
        total_space,
        available_space,
        exists: true,
    }
}

pub fn generate_plan_action(app: &mut App) {
    if !app.ensure_idle() {
        return;
    }
    if app.backend.is_demo() {
        let state = app.backup_ui.demo_state.unwrap_or(DemoState::SingleFile);
        apply_demo_state(app, state);
        return;
    }
    app.backup_ui.check_invalidate_plan();
    if let Some(error) = app.backup_ui.filter_error() {
        adapter::apply_outcome(app, adapter::failure(error));
        return;
    }
    adapter::generate(app);
}

pub fn start_backup_action(app: &mut App) {
    if !app.ensure_idle() || app.backup_ui.is_running {
        return;
    }
    if !app.backend.is_demo() {
        adapter::start(app);
        return;
    }
    let Some(plan) = &app.backup_ui.plan else {
        return;
    };
    app.backup_ui.is_running = true;
    app.backup_ui.stopping_requested = false;
    app.backup_ui.done_files = 0;
    app.backup_ui.recovery_totals_unknown = false;
    app.backup_ui.total_files = plan.pending_count;
    app.backup_ui.transferred_bytes = 0;
    app.backup_ui.total_bytes = plan.pending_bytes;
    app.backup_ui.speed_bps = Some(142 * 1024 * 1024); // Pure synthetic example, never a measurement.
    app.backup_ui.eta_secs = Some(180);
    app.backup_ui.last_outcome = None;
    app.task_kind = Some(crate::app::TaskKind::Backup);
    app.task_started = Some(std::time::Instant::now());
    let total_files = plan.pending_count;
    let items = plan.items.clone();
    let (tx, rx) = mpsc::channel();
    app.rx = Some(rx);
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        let mut completed = 0;
        let mut bytes = 0u64;
        let mut stopped = false;
        for item in items
            .iter()
            .filter(|i| i.status == FilePlanItemStatus::PendingCopy)
        {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                stopped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                stopped = true;
                break;
            }
            completed += 1;
            bytes = bytes.saturating_add(item.size_bytes);
            let _ = tx.send(crate::reporter::UiEvent::Log {
                level: bftool_core::reporter::LogLevel::Info,
                msg: format!("【合成示例】复制 {}", item.rel_path),
            });
        }
        Ok(crate::app::ExecutionResult::Backup(BackupOutcomeSummary {
            kind: if stopped {
                BackupOutcomeKind::Stopped
            } else {
                BackupOutcomeKind::NotVerified
            },
            title: format!("【合成示例】{completed}/{total_files} 项"),
            detail: if stopped {
                "演示任务已停止；没有真实文件写入".into()
            } else {
                "纯内存演示完成；没有真实复制或哈希校验".into()
            },
            completed_items: completed,
            completed_bytes: bytes,
            failed_items: 0,
            is_content_verified: false,
        }))
    }));
}

pub fn request_stop_backup(app: &mut App) {
    if let Some(task) = &app.file_backup_plan_task {
        task.request_cancel();
        app.backup_ui.stopping_requested = true;
    } else if app.task_running(crate::app::TaskKind::Backup) {
        app.task.as_ref().unwrap().request_cancel();
        app.backup_ui.stopping_requested = true;
    }
}
/// 应用 8 类演示验收状态（纯内存，不碰磁盘）
pub fn apply_demo_state(app: &mut App, state: DemoState) {
    if !app.backend.is_demo() || app.is_busy() {
        return;
    }
    app.backup_ui.core_plans = None;
    app.backup_ui.recovery_totals_unknown = false;
    app.backup_ui.demo_state = Some(state);
    app.backup_ui.is_running = false;
    app.backup_ui.stopping_requested = false;
    app.backup_ui.speed_bps = None;
    app.backup_ui.eta_secs = None;

    match state {
        DemoState::BlankInitial => {
            app.backup_ui.sources.clear();
            app.backup_ui.target_path.clear();
            app.backup_ui.target_info = None;
            app.backup_ui.validation = RelationValidation::EmptyTarget;
            app.backup_ui.plan = None;
            app.backup_ui.last_outcome = None;
        }

        DemoState::SingleFile => {
            app.backup_ui.sources = vec![SourceItem {
                path: PathBuf::from(r"D:\素材库\纪录片_终剪版_4K.mov"),
                name: "纪录片_终剪版_4K.mov".into(),
                is_dir: false,
                size_bytes: 28 * 1024 * 1024 * 1024 + 400 * 1024 * 1024, // 28.4 GB
                folder_filter: FolderFilter::default(),
            }];
            app.backup_ui.target_path = r"E:\机械备份主盘\202610_媒体归档".into();
            app.backup_ui.target_info = Some(TargetInfo {
                path: PathBuf::from(&app.backup_ui.target_path),
                drive_letter: Some("E".into()),
                drive_kind: TargetDriveKind::Hdd,
                total_space: 3600 * 1024 * 1024 * 1024,
                available_space: 1800 * 1024 * 1024 * 1024,
                exists: true,
            });
            app.backup_ui.validation = RelationValidation::Valid;
            app.backup_ui.plan = Some(FileBackupPlan {
                items: vec![FilePlanItem {
                    source_path: PathBuf::from(r"D:\素材库\纪录片_终剪版_4K.mov"),
                    target_path: PathBuf::from(
                        r"E:\机械备份主盘\202610_媒体归档\纪录片_终剪版_4K.mov",
                    ),
                    rel_path: "纪录片_终剪版_4K.mov".into(),
                    size_bytes: 28 * 1024 * 1024 * 1024 + 400 * 1024 * 1024,
                    status: FilePlanItemStatus::PendingCopy,
                    reason: "完整复制，写入后比对".into(),
                }],
                pending_count: 1,
                pending_bytes: 28 * 1024 * 1024 * 1024 + 400 * 1024 * 1024,
                already_exists_count: 0,
                already_exists_bytes: 0,
                conflict_count: 0,
                conflict_bytes: 0,
                unreadable_count: 0,
                unreadable_bytes: 0,
                has_insufficient_space: false,
                space_needed_bytes: 28 * 1024 * 1024 * 1024 + 400 * 1024 * 1024,
                space_available_bytes: 1800 * 1024 * 1024 * 1024,
            });
            app.backup_ui.last_outcome = None;
        }

        DemoState::MultiFolder => {
            app.backup_ui.sources = vec![
                SourceItem {
                    path: PathBuf::from(r"D:\工作项目\202610_宣传片拍摄原片"),
                    name: "202610_宣传片拍摄原片".into(),
                    is_dir: true,
                    size_bytes: 125 * 1024 * 1024 * 1024,
                    folder_filter: FolderFilter {
                        include_subfolders: true,
                        ..Default::default()
                    },
                },
                SourceItem {
                    path: PathBuf::from(r"D:\音频工程\环境音效库_WAV"),
                    name: "环境音效库_WAV".into(),
                    is_dir: true,
                    size_bytes: 42 * 1024 * 1024 * 1024,
                    folder_filter: FolderFilter {
                        include_subfolders: true,
                        ..Default::default()
                    },
                },
                SourceItem {
                    path: PathBuf::from(r"D:\设计源文件\宣传画册分层设计.psd"),
                    name: "宣传画册分层设计.psd".into(),
                    is_dir: false,
                    size_bytes: 3800 * 1024 * 1024,
                    folder_filter: FolderFilter::default(),
                },
            ];
            app.backup_ui.target_path = r"F:\机械存储阵列\项目归档区".into();
            app.backup_ui.target_info = Some(TargetInfo {
                path: PathBuf::from(&app.backup_ui.target_path),
                drive_letter: Some("F".into()),
                drive_kind: TargetDriveKind::Hdd,
                total_space: 1863 * 1024 * 1024 * 1024,
                available_space: 850 * 1024 * 1024 * 1024,
                exists: true,
            });
            app.backup_ui.validation = RelationValidation::Valid;
            app.backup_ui.plan = Some(FileBackupPlan {
                items: vec![
                    FilePlanItem {
                        source_path: PathBuf::from(
                            r"D:\工作项目\202610_宣传片拍摄原片\镜头_01_ProRes422.mov",
                        ),
                        target_path: PathBuf::from(
                            r"F:\机械存储阵列\项目归档区\202610_宣传片拍摄原片\镜头_01_ProRes422.mov",
                        ),
                        rel_path: "202610_宣传片拍摄原片/镜头_01_ProRes422.mov".into(),
                        size_bytes: 65 * 1024 * 1024 * 1024,
                        status: FilePlanItemStatus::PendingCopy,
                        reason: "待复制".into(),
                    },
                    FilePlanItem {
                        source_path: PathBuf::from(
                            r"D:\工作项目\202610_宣传片拍摄原片\镜头_02_ProRes422.mov",
                        ),
                        target_path: PathBuf::from(
                            r"F:\机械存储阵列\项目归档区\202610_宣传片拍摄原片\镜头_02_ProRes422.mov",
                        ),
                        rel_path: "202610_宣传片拍摄原片/镜头_02_ProRes422.mov".into(),
                        size_bytes: 60 * 1024 * 1024 * 1024,
                        status: FilePlanItemStatus::PendingCopy,
                        reason: "待复制".into(),
                    },
                    FilePlanItem {
                        source_path: PathBuf::from(r"D:\音频工程\环境音效库_WAV\山林风声_96K.wav"),
                        target_path: PathBuf::from(
                            r"F:\机械存储阵列\项目归档区\环境音效库_WAV\山林风声_96K.wav",
                        ),
                        rel_path: "环境音效库_WAV/山林风声_96K.wav".into(),
                        size_bytes: 42 * 1024 * 1024 * 1024,
                        status: FilePlanItemStatus::PendingCopy,
                        reason: "待复制".into(),
                    },
                    FilePlanItem {
                        source_path: PathBuf::from(r"D:\设计源文件\宣传画册分层设计.psd"),
                        target_path: PathBuf::from(
                            r"F:\机械存储阵列\项目归档区\宣传画册分层设计.psd",
                        ),
                        rel_path: "宣传画册分层设计.psd".into(),
                        size_bytes: 3800 * 1024 * 1024,
                        status: FilePlanItemStatus::PendingCopy,
                        reason: "待复制".into(),
                    },
                ],
                pending_count: 4,
                pending_bytes: (125 + 42) * 1024 * 1024 * 1024 + 3800 * 1024 * 1024,
                already_exists_count: 0,
                already_exists_bytes: 0,
                conflict_count: 0,
                conflict_bytes: 0,
                unreadable_count: 0,
                unreadable_bytes: 0,
                has_insufficient_space: false,
                space_needed_bytes: (125 + 42) * 1024 * 1024 * 1024 + 3800 * 1024 * 1024,
                space_available_bytes: 850 * 1024 * 1024 * 1024,
            });
            app.backup_ui.last_outcome = None;
        }

        DemoState::Conflict => {
            app.backup_ui.sources = vec![
                SourceItem {
                    path: PathBuf::from(r"D:\剪辑工程\纪录片剪辑工程_v2.prproj"),
                    name: "纪录片剪辑工程_v2.prproj".into(),
                    is_dir: false,
                    size_bytes: 2100 * 1024 * 1024,
                    folder_filter: FolderFilter::default(),
                },
                SourceItem {
                    path: PathBuf::from(r"D:\剪辑工程\背景配乐_母带.wav"),
                    name: "背景配乐_母带.wav".into(),
                    is_dir: false,
                    size_bytes: 150 * 1024 * 1024,
                    folder_filter: FolderFilter::default(),
                },
            ];
            app.backup_ui.target_path = r"E:\项目备份目录".into();
            app.backup_ui.target_info = Some(TargetInfo {
                path: PathBuf::from(&app.backup_ui.target_path),
                drive_letter: Some("E".into()),
                drive_kind: TargetDriveKind::Unknown,
                total_space: 931 * 1024 * 1024 * 1024,
                available_space: 450 * 1024 * 1024 * 1024,
                exists: true,
            });
            app.backup_ui.validation = RelationValidation::Valid;
            app.backup_ui.plan = Some(FileBackupPlan {
                items: vec![
                    FilePlanItem {
                        source_path: PathBuf::from(r"D:\剪辑工程\纪录片剪辑工程_v2.prproj"),
                        target_path: PathBuf::from(r"E:\项目备份目录\纪录片剪辑工程_v2.prproj"),
                        rel_path: "纪录片剪辑工程_v2.prproj".into(),
                        size_bytes: 2100 * 1024 * 1024,
                        status: FilePlanItemStatus::Conflict,
                        reason:
                            "【同名冲突】目标已存在同名但修改时间与大小不同之文件，严格禁止静默覆盖"
                                .into(),
                    },
                    FilePlanItem {
                        source_path: PathBuf::from(r"D:\剪辑工程\背景配乐_母带.wav"),
                        target_path: PathBuf::from(r"E:\项目备份目录\背景配乐_母带.wav"),
                        rel_path: "背景配乐_母带.wav".into(),
                        size_bytes: 150 * 1024 * 1024,
                        status: FilePlanItemStatus::PendingCopy,
                        reason: "正常复制".into(),
                    },
                ],
                pending_count: 1,
                pending_bytes: 150 * 1024 * 1024,
                already_exists_count: 0,
                already_exists_bytes: 0,
                conflict_count: 1,
                conflict_bytes: 2100 * 1024 * 1024,
                unreadable_count: 0,
                unreadable_bytes: 0,
                has_insufficient_space: false,
                space_needed_bytes: 150 * 1024 * 1024,
                space_available_bytes: 450 * 1024 * 1024 * 1024,
            });
            app.backup_ui.last_outcome = None;
        }

        DemoState::InsufficientSpace => {
            app.backup_ui.sources = vec![SourceItem {
                path: PathBuf::from(r"D:\原始拍摄母带\8K_RED_RAW素材集"),
                name: "8K_RED_RAW素材集".into(),
                is_dir: true,
                size_bytes: 820 * 1024 * 1024 * 1024,
                folder_filter: FolderFilter {
                    include_subfolders: true,
                    ..Default::default()
                },
            }];
            app.backup_ui.target_path = r"G:\老式机械备份移动盘".into();
            app.backup_ui.target_info = Some(TargetInfo {
                path: PathBuf::from(&app.backup_ui.target_path),
                drive_letter: Some("G".into()),
                drive_kind: TargetDriveKind::Hdd,
                total_space: 931 * 1024 * 1024 * 1024,
                available_space: 120 * 1024 * 1024 * 1024, // 仅剩 120 GB
                exists: true,
            });
            app.backup_ui.validation = RelationValidation::Valid;
            app.backup_ui.plan = Some(FileBackupPlan {
                items: vec![FilePlanItem {
                    source_path: PathBuf::from(r"D:\原始拍摄母带\8K_RED_RAW素材集\01卷.R3D"),
                    target_path: PathBuf::from(r"G:\老式机械备份移动盘\8K_RED_RAW素材集\01卷.R3D"),
                    rel_path: "8K_RED_RAW素材集/01卷.R3D".into(),
                    size_bytes: 820 * 1024 * 1024 * 1024,
                    status: FilePlanItemStatus::InsufficientSpace,
                    reason: "拟写入需 820 GB，但可用仅 120 GB（缺口 700 GB），空间不足".into(),
                }],
                pending_count: 1,
                pending_bytes: 820 * 1024 * 1024 * 1024,
                already_exists_count: 0,
                already_exists_bytes: 0,
                conflict_count: 0,
                conflict_bytes: 0,
                unreadable_count: 0,
                unreadable_bytes: 0,
                has_insufficient_space: true,
                space_needed_bytes: 820 * 1024 * 1024 * 1024,
                space_available_bytes: 120 * 1024 * 1024 * 1024,
            });
            app.backup_ui.last_outcome = None;
        }

        DemoState::Running => {
            apply_demo_state(app, DemoState::MultiFolder);
            app.backup_ui.demo_state = Some(DemoState::Running);
            app.backup_ui.is_running = true;
            app.backup_ui.stopping_requested = false;
            app.backup_ui.current_file = "202610_宣传片拍摄原片/镜头_01_ProRes422.mov".into();
            app.backup_ui.done_files = 48;
            app.backup_ui.total_files = 120;
            app.backup_ui.transferred_bytes = 68 * 1024 * 1024 * 1024 + 500 * 1024 * 1024;
            app.backup_ui.total_bytes = 171 * 1024 * 1024 * 1024;
            app.backup_ui.speed_bps = Some(142 * 1024 * 1024);
            app.backup_ui.eta_secs = Some(750); // 12分30秒
        }

        DemoState::Stopped => {
            apply_demo_state(app, DemoState::MultiFolder);
            app.backup_ui.demo_state = Some(DemoState::Stopped);
            app.backup_ui.is_running = false;
            app.backup_ui.stopping_requested = false;
            app.backup_ui.last_outcome = Some(BackupOutcomeSummary {
                kind: BackupOutcomeKind::Stopped,
                title: "任务已由用户手动停止".into(),
                detail: "任务于 2026-10-03 19:15 安全中断。已安全写入 48 项（68.5 GB），剩余 72 项保持原样未修改，目标盘既有数据完整未受损。".into(),
                completed_items: 48,
                completed_bytes: 68 * 1024 * 1024 * 1024 + 500 * 1024 * 1024,
                failed_items: 0,
                is_content_verified: false,
            });
        }

        DemoState::PartialFailure => {
            apply_demo_state(app, DemoState::SingleFile);
            app.backup_ui.demo_state = Some(DemoState::PartialFailure);
            app.backup_ui.is_running = false;
            app.backup_ui.stopping_requested = false;
            app.backup_ui.last_outcome = Some(BackupOutcomeSummary {
                kind: BackupOutcomeKind::PartialComplete,
                title: "部分完成 (存在 1 项不可读取失败)".into(),
                detail: "本次计划处理 3 项：成功备份 2 项（4.5 GB）；1 项因操作系统文件被其他进程独占锁定导致读取失败（系统缓存文件.dat），未进行盲目覆盖。".into(),
                completed_items: 2,
                completed_bytes: 4500 * 1024 * 1024,
                failed_items: 1,
                is_content_verified: false,
            });
        }
    }

    app.backup_ui.plan_signature = app.backup_ui.current_signature();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered_text(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, egui::Pos2)> {
        fn collect(shape: &egui::Shape, found: &mut Vec<(String, egui::Pos2)>) {
            match shape {
                egui::Shape::Text(text) => found.push((text.galley.text().into(), text.pos)),
                egui::Shape::Vec(values) => {
                    for value in values {
                        collect(value, found);
                    }
                }
                _ => {}
            }
        }
        let mut found = Vec::new();
        for shape in shapes {
            collect(&shape.shape, &mut found);
        }
        found
    }

    #[test]
    fn folder_filter_gui_headless_real_controls_clicks_and_busy_inert() {
        let mut app = crate::app::tests::fixture();
        app.backend = crate::backend::Backend::Live;
        // No filesystem classification is necessary to expose native AddFolder controls.
        add_source_folder(&mut app, PathBuf::from("synthetic-no-such-folder"));
        assert!(app.backup_ui.sources[0].is_dir);
        let mut draft = app.backup_ui.sources[0].folder_filter.clone();
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(360.0, 600.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input.clone(), |ctx| {
            egui::CentralPanel::default()
                .show(ctx, |ui| render_folder_filter(ui, &mut draft, true));
        });
        let text = rendered_text(&output.shapes);
        for label in [
            "包含子文件夹",
            "仅当前层（不包含子文件夹）",
            "按文件后缀筛选",
            "常用后缀",
            ".tar",
            ".zip",
            "tar, zip",
            "包含无后缀文件",
            "清除筛选（保留子文件夹范围）",
        ] {
            assert!(
                text.iter().any(|(value, _)| value.contains(label)),
                "missing rendered control: {label}"
            );
        }
        let click_pos = text
            .iter()
            .find(|(value, _)| value == "包含子文件夹")
            .unwrap()
            .1
            + egui::vec2(6.0, 6.0);
        for pressed in [true, false] {
            let mut event = input.clone();
            event.events = vec![
                egui::Event::PointerMoved(click_pos),
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ];
            let _ = ctx.run(event, |ctx| {
                egui::CentralPanel::default()
                    .show(ctx, |ui| render_folder_filter(ui, &mut draft, true));
            });
        }
        assert!(
            draft.include_subfolders,
            "actual checkbox pointer event must update draft"
        );
        let saved = draft.clone();
        for pressed in [true, false] {
            let mut event = input.clone();
            event.events = vec![
                egui::Event::PointerMoved(click_pos),
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ];
            let _ = ctx.run(event, |ctx| {
                egui::CentralPanel::default()
                    .show(ctx, |ui| render_folder_filter(ui, &mut draft, false));
            });
        }
        assert_eq!(draft, saved, "busy rendered controls must ignore input");
    }

    #[test]
    fn folder_filter_gui_clear_filter_restores_all_files_with_scope_preserved() {
        let mut draft = FolderFilter {
            include_subfolders: true,
            enabled: true,
            extensions_input: "zip".into(),
            include_extensionless: false,
        };
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(360.0, 600.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input.clone(), |ctx| {
            egui::CentralPanel::default()
                .show(ctx, |ui| render_folder_filter(ui, &mut draft, true));
        });
        let text = rendered_text(&output.shapes);
        let click_pos = text
            .iter()
            .find(|(value, _)| value == "清除筛选（保留子文件夹范围）")
            .unwrap()
            .1
            + egui::vec2(6.0, 6.0);
        for pressed in [true, false] {
            let mut event = input.clone();
            event.events = vec![
                egui::Event::PointerMoved(click_pos),
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ];
            let _ = ctx.run(event, |ctx| {
                egui::CentralPanel::default()
                    .show(ctx, |ui| render_folder_filter(ui, &mut draft, true));
            });
        }
        let options = draft.directory_options().unwrap();
        assert!(options.recursive);
        assert!(options.extensions.is_none());
        assert!(options.include_extensionless);
        assert!(draft.extensions_input.is_empty());
        assert!(!draft.enabled);
    }

    #[test]
    fn direct_regression_demo_selection_is_inert_even_for_existing_directory() {
        let mut app = crate::app::tests::fixture();
        app.backend = crate::backend::Backend::Demo;
        let existing = std::env::temp_dir();
        add_source_path(&mut app, existing.clone());
        set_target_folder(&mut app, existing);
        assert!(app.backup_ui.sources.is_empty());
        assert!(app.backup_ui.target_path.is_empty());
        assert!(app.backup_ui.target_info.is_none());
    }

    fn busy_verify() -> App {
        let mut app = crate::app::tests::fixture();
        app.backend = crate::backend::Backend::Demo;
        app.task_kind = Some(crate::app::TaskKind::Verify);
        app.task_started = Some(std::time::Instant::now());
        app.task = Some(BackgroundTask::spawn(|cancel| {
            while !cancel.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::yield_now();
            }
            Ok(crate::app::ExecutionResult::Summary("synthetic".into()))
        }));
        app
    }

    #[test]
    fn direct_regression_backup_stop_does_not_cancel_verify_owner() {
        let mut app = busy_verify();
        request_stop_backup(&mut app);
        assert!(!app.task.as_ref().unwrap().cancel_requested());
        assert!(!app.backup_ui.stopping_requested);
    }

    #[test]
    fn direct_regression_busy_demo_preset_preserves_owner_and_selection() {
        let mut app = busy_verify();
        let signature = app.backup_ui.current_signature();
        apply_demo_state(&mut app, DemoState::Running);
        assert_eq!(signature, app.backup_ui.current_signature());
        assert!(!app.backup_ui.is_running);
        assert_eq!(app.task_kind, Some(crate::app::TaskKind::Verify));
    }

    #[test]
    fn direct_regression_duplicate_start_preserves_actual_owner_token() {
        let mut app = crate::app::tests::fixture();
        app.backend = crate::backend::Backend::Demo;
        apply_demo_state(&mut app, DemoState::MultiFolder);
        start_backup_action(&mut app);
        assert!(app.task.is_some());
        let started = app
            .task_started
            .expect("actual task must record start time");
        start_backup_action(&mut app);
        assert_eq!(app.task_started, Some(started));
    }

    #[test]
    fn direct_regression_busy_selection_methods_are_inert() {
        let mut app = busy_verify();
        app.backend = crate::backend::Backend::Live;
        let existing = std::env::temp_dir();
        add_source_path(&mut app, existing.clone());
        set_target_folder(&mut app, existing);
        assert!(app.backup_ui.sources.is_empty());
        assert!(app.backup_ui.target_path.is_empty());
    }

    #[test]
    fn direct_regression_busy_live_to_demo_switch_preserves_real_task_mode() {
        let mut app = busy_verify();
        app.backend = crate::backend::Backend::Live;
        switch_to_demo_if_idle(&mut app);
        assert!(!app.backend.is_demo());
        assert_eq!(app.task_kind, Some(crate::app::TaskKind::Verify));
        assert!(!app.task.as_ref().unwrap().cancel_requested());
    }

    #[test]
    fn direct_regression_idle_live_demo_switch_replaces_real_caches() {
        let mut app = crate::app::tests::fixture();
        app.backend = crate::backend::Backend::Live;
        app.logs.push((
            bftool_core::reporter::LogLevel::Info,
            "real-path-marker".into(),
        ));
        app.verify_ui.error = Some("real-path-marker".into());
        app.settings_ui.ready_root = "real-path-marker".into();
        switch_to_demo_if_idle(&mut app);
        assert!(app.backend.is_demo());
        assert!(app.logs.is_empty());
        assert!(app.verify_ui.error.is_none());
        assert!(!app.settings_ui.ready_root.contains("real-path-marker"));
        assert!(app.cfg.ready_root.to_string_lossy().contains("演示"));
    }

    #[test]
    fn direct_regression_demo_plain_verify_history_resume_are_inert() {
        let mut app = crate::app::tests::fixture();
        app.backend = crate::backend::Backend::Demo;
        app.backup_ui.target_path = "pure-synthetic-target".into();
        crate::views::verify::start_plain_verify(&mut app, PathBuf::from("pure-synthetic-target"));
        crate::views::tasks::refresh_history(&mut app);
        adapter::resume(&mut app, "pure-synthetic-job".into());
        assert!(app.task.is_none());
        assert!(app.tasks_ui.history_task.is_none());
    }

    #[test]
    fn test_relation_validation() {
        let sources = vec![SourceItem {
            path: PathBuf::from(r"C:\work\project"),
            name: "project".into(),
            is_dir: true,
            size_bytes: 100,
            folder_filter: FolderFilter::default(),
        }];

        // 目标为空
        assert_eq!(
            validate_relation(&sources, Path::new("")),
            RelationValidation::EmptyTarget
        );

        // 目标不存在 (如果是伪造路径)
        assert_eq!(
            validate_relation(&sources, Path::new("ordinary-target")),
            RelationValidation::Valid
        );

        // 来源与目标相同
        assert_eq!(
            validate_relation(&sources, Path::new(r"C:\work\project")),
            RelationValidation::SourceEqualsTarget
        );

        // 目标位于来源内部
        assert_eq!(
            validate_relation(&sources, Path::new(r"C:\work\project\backup_sub")),
            RelationValidation::TargetInsideSource
        );
    }

    #[test]
    fn test_signature_invalidates_plan() {
        let mut state = BackupUiState::default();
        state.sources.push(SourceItem {
            path: PathBuf::from(r"C:\source1.txt"),
            name: "source1.txt".into(),
            is_dir: false,
            size_bytes: 1024,
            folder_filter: FolderFilter::default(),
        });
        state.target_path = r"D:\target".into();
        state.plan = Some(FileBackupPlan {
            items: Vec::new(),
            pending_count: 1,
            pending_bytes: 1024,
            already_exists_count: 0,
            already_exists_bytes: 0,
            conflict_count: 0,
            conflict_bytes: 0,
            unreadable_count: 0,
            unreadable_bytes: 0,
            has_insufficient_space: false,
            space_needed_bytes: 1024,
            space_available_bytes: 10000,
        });
        state.plan_signature = state.current_signature();

        // 来源未变时保持
        state.check_invalidate_plan();
        assert!(state.plan.is_some());

        // 目标改变时，计划必须立即失效！
        state.target_path = r"E:\target_changed".into();
        state.check_invalidate_plan();
        assert!(state.plan.is_none(), "目标改变时旧计划必须立即失效");
    }

    #[test]
    fn test_eight_demo_states_all_valid() {
        let mut app = crate::app::tests::fixture();
        app.backend = crate::backend::Backend::Demo;

        for state in DemoState::ALL {
            apply_demo_state(&mut app, state);
            assert_eq!(app.backup_ui.demo_state, Some(state));
        }
    }
}

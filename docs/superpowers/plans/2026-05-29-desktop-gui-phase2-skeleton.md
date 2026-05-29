# 桌面版 Phase 2：GUI 骨架 + 后台执行 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: `superpowers:executing-plans`(inline)或 `subagent-driven-development`。Steps 用 `- [ ]`。每个 task 独立(可测的红-绿;纯 GUI 渲染 task 以"编译通过 + 逻辑单测"为绿)+ 单 commit;每步后跑 §门禁。**cargo 用 PowerShell 跑**(git-bash 会误抓 coreutils link)。

**Goal:** 用 eframe/egui 起一个能跑的 `bftool-gui`:左侧栏导航 + 仪表盘 + 备份页,**点「备份」→ 后台线程跑 `archive::plan()`→预览→`run_plan(cancel,GuiReporter)`**,实时进度/日志,可项目边界取消。复用 Phase 1 的结构化 core API,**绝不解析 reporter 文本、绝不在 UI 线程跑长任务**。

**Architecture:** 新 crate `crates/bftool-gui`(eframe 入口)。`GuiReporter` 实现 core `Reporter` trait,把 `log` 推 `mpsc::Sender<UiEvent>`、把 `progress_bytes` 写 `Arc<Mutex<ProgressState>>`;后台线程跑 core,UI 每帧 `try_recv` drain + 读进度 + `request_repaint`。取消用 `Arc<AtomicBool>`(项目边界,Spec D §3/§4.5)。

**Tech Stack:** Rust workspace、eframe/egui、bftool-core、anyhow;std::sync::mpsc / Arc / Mutex / AtomicBool。

**门禁(每步)**:`cargo build -p bftool-gui`(GUI 至少编译)/ `cargo test --workspace --all-targets` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo fmt --all -- --check`,全绿。**GUI 渲染无法在无头环境强测**(Spec D §7):测试覆盖 `GuiReporter` 投递、`task` 状态机、视图启用等**纯逻辑**;窗口/渲染靠编译 + 用户实跑验证。

**隔离**:在 `desktop-gui` 分支(已与 main 同步)。Phase 2 提交其上,完成开 PR → main(squash 合并,我自管 git)。

**不在 Phase 2**(留 Phase 3):verify / init / find / drives / settings 五个视图(各自独立 plan)。本阶段只交付 **骨架 + 仪表盘 + 备份页 + 后台执行基建**(其余视图先占位 placeholder)。

---

## 文件结构(Phase 2 触碰)

- `Cargo.toml`(workspace)— members 加 `crates/bftool-gui`;workspace.dependencies 加 `eframe`
- `crates/bftool-gui/Cargo.toml` — eframe + bftool-core + anyhow
- `crates/bftool-gui/src/main.rs` — eframe 入口(`eframe::run_native`)
- `crates/bftool-gui/src/app.rs` — `App` + `View` enum + `update()` 循环 + 左侧栏
- `crates/bftool-gui/src/reporter.rs` — `GuiReporter`/`UiEvent`/`ProgressState`/`GuiProgress`
- `crates/bftool-gui/src/task.rs` — `BackgroundTask` + 取消 + 任务状态机(`TaskState`)
- `crates/bftool-gui/src/views/mod.rs` + `dashboard.rs` + `archive.rs`(+ 其余视图 placeholder)

**任务顺序**:T1 crate 脚手架(编译出空窗) → T2 GuiReporter(纯逻辑测) → T3 task 后台+取消(纯逻辑测) → T4 App 壳 + 左侧栏 + 视图路由 → T5 仪表盘 → T6 备份页(plan→预览→run_plan + 进度/日志/取消)。

---

### Task 1：`bftool-gui` crate 脚手架(编译出一个空窗)

**Files:** Modify `Cargo.toml`(root);Create `crates/bftool-gui/Cargo.toml`、`crates/bftool-gui/src/main.rs`

eframe 拉入 winit/wgpu(glow)等大量依赖;**本 task 唯一目标是验证它在本机 + CI 真机能编译**,先把最大未知量(eframe 能否 build)消掉。

- [ ] **Step 1** root `Cargo.toml`:`members` 加 `"crates/bftool-gui"`;`[workspace.dependencies]` 加 `eframe = "0.31"`(版本以 `cargo build` 实际解析为准;若 0.31 不可用就退到能 build 的最近稳定,并在 commit message 注明)。`default-members` 保持只有 cli(GUI 不进默认 build,避免 CLI 用户被迫编 eframe)。
- [ ] **Step 2** `crates/bftool-gui/Cargo.toml`:
```toml
[package]
name = "bftool-gui"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[[bin]]
name = "bftool-gui"
path = "src/main.rs"

[dependencies]
bftool-core = { path = "../bftool-core" }
eframe.workspace = true
anyhow.workspace = true
```
- [ ] **Step 3** `src/main.rs` 最小可运行窗口(暂不引 app.rs):
```rust
//! bftool 桌面版入口(eframe)。
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
```
> eframe 0.31 的 creator 闭包返回 `Result<Box<dyn App>, _>`(故 `Ok(...)`)。若解析到的版本签名不同(老版本返回 `Box<dyn App>` 不带 `Ok`),按编译器提示调整这一行。
- [ ] **Step 4** 跑 `cargo build -p bftool-gui`(PowerShell)→ 期望编译通过(可能首次拉依赖较久)。**不**尝试运行窗口(无头环境跑不起来,也不需要)。
- [ ] **Step 5** `cargo fmt --all` + `cargo clippy -p bftool-gui --all-targets -- -D warnings` 绿。
- [ ] **Step 6** commit：`feat(gui): bftool-gui crate 脚手架 + eframe 空窗(Phase2 T1)`

> 风险闸:若 eframe 在本机(ARM64 Windows)build 失败但只是平台问题——以 CI 真机(x86_64-windows-msvc)为准,在 PR 阶段验证;若本机/CI 都 build 不过,停下报告(可能要换 GUI 框架或调 features),不硬闯。

---

### Task 2：`GuiReporter` 实现 core `Reporter`(纯逻辑可测)

**Files:** Create `crates/bftool-gui/src/reporter.rs`

core `Reporter` 要求 `Send + Sync`、`fn log(&self, LogLevel, &str)`、`fn progress_bytes(&self, &str, u64) -> Box<dyn ProgressHandle>`(`ProgressHandle: Send`,`inc`/`finish`)。GUI 把 log 推 channel、progress 写共享状态。**Sender 用 `Mutex` 包裹保证 `Sync`**(不依赖 std `Sender: Sync` 的版本差异)。

- [ ] **Step 1（红）** 写测试(纯逻辑,不开窗):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bftool_core::reporter::{LogLevel, Reporter};

    #[test]
    fn log_events_flow_to_receiver() {
        let (tx, rx) = std::sync::mpsc::channel();
        let prog = std::sync::Arc::new(std::sync::Mutex::new(ProgressState::default()));
        let rep = GuiReporter::new(tx, prog);
        rep.info("hello");
        rep.error("boom");
        let got: Vec<UiEvent> = rx.try_iter().collect();
        assert_eq!(got.len(), 2);
        assert!(matches!(&got[0], UiEvent::Log { level: LogLevel::Info, msg } if msg == "hello"));
        assert!(matches!(&got[1], UiEvent::Log { level: LogLevel::Error, msg } if msg == "boom"));
    }

    #[test]
    fn progress_handle_updates_shared_state() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let prog = std::sync::Arc::new(std::sync::Mutex::new(ProgressState::default()));
        let rep = GuiReporter::new(tx, std::sync::Arc::clone(&prog));
        let mut h = rep.progress_bytes("复制 X", 100);
        h.inc(30);
        h.inc(20);
        {
            let p = prog.lock().unwrap();
            assert_eq!(p.label, "复制 X");
            assert_eq!(p.total, 100);
            assert_eq!(p.current, 50);
            assert!(p.active);
        }
        h.finish();
        assert!(!prog.lock().unwrap().active);
    }

    #[test]
    fn is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GuiReporter>();
    }
}
```
- [ ] **Step 2** 跑 → FAIL(类型不存在)。
- [ ] **Step 3（绿）** 实现:
```rust
//! GuiReporter:把 core 的 Reporter 调用桥到 GUI —— 日志推 channel,进度写共享状态。
//! UI 线程每帧 try_recv drain channel + 读 ProgressState 渲染。core 跑在后台线程。
use std::sync::{mpsc::Sender, Arc, Mutex};

use bftool_core::reporter::{LogLevel, ProgressHandle, Reporter};

/// 投递给 UI 的离散事件(目前只有日志;后续可加完成/里程碑)。
#[derive(Debug, Clone)]
pub enum UiEvent {
    Log { level: LogLevel, msg: String },
}

/// 当前进度(字节量)。UI 每帧读它渲染进度条。`active=false` = 无进行中的进度。
#[derive(Debug, Default, Clone)]
pub struct ProgressState {
    pub label: String,
    pub total: u64,
    pub current: u64,
    pub active: bool,
}

/// 实现 core `Reporter`:Send+Sync(Sender 用 Mutex 包裹保证 Sync,不依赖 std 版本差异)。
pub struct GuiReporter {
    tx: Mutex<Sender<UiEvent>>,
    progress: Arc<Mutex<ProgressState>>,
}

impl GuiReporter {
    pub fn new(tx: Sender<UiEvent>, progress: Arc<Mutex<ProgressState>>) -> Self {
        Self { tx: Mutex::new(tx), progress }
    }
}

impl Reporter for GuiReporter {
    fn log(&self, level: LogLevel, msg: &str) {
        // 发送失败(UI 已关 / 接收端 drop)只静默丢弃这条日志 —— 不是错误,不 panic、不阻塞后台任务。
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(UiEvent::Log { level, msg: msg.to_string() });
        }
    }
    fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle> {
        if let Ok(mut p) = self.progress.lock() {
            *p = ProgressState { label: label.to_string(), total, current: 0, active: true };
        }
        Box::new(GuiProgress { progress: Arc::clone(&self.progress) })
    }
}

struct GuiProgress {
    progress: Arc<Mutex<ProgressState>>,
}
impl ProgressHandle for GuiProgress {
    fn inc(&mut self, delta: u64) {
        if let Ok(mut p) = self.progress.lock() {
            p.current = p.current.saturating_add(delta);
        }
    }
    fn finish(&mut self) {
        if let Ok(mut p) = self.progress.lock() {
            p.active = false;
            p.current = p.total;
        }
    }
}
```
- [ ] **Step 4** test + clippy + fmt 绿。
- [ ] **Step 5** commit：`feat(gui): GuiReporter 桥接 core Reporter→channel/进度状态 (Phase2 T2; Spec D §3)`

---

### Task 3：`task` 后台执行 + 取消 + 状态机(纯逻辑可测)

**Files:** Create `crates/bftool-gui/src/task.rs`

后台线程跑一个返回 `anyhow::Result<String>`(摘要文案)的闭包;UI 轮询完成态。取消是 `Arc<AtomicBool>`(闭包内传给 core 的 `&AtomicBool`,项目边界生效)。

- [ ] **Step 1（红）** 纯逻辑测试(用闭包模拟 core,不开窗):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn task_runs_to_done() {
        let t = BackgroundTask::spawn(|_cancel| Ok("完成 3 项".to_string()));
        let outcome = t.join_for_test();
        assert!(matches!(outcome, TaskOutcome::Done(s) if s == "完成 3 项"));
    }

    #[test]
    fn task_reports_failure() {
        let t = BackgroundTask::spawn(|_cancel| anyhow::bail!("炸了"));
        assert!(matches!(t.join_for_test(), TaskOutcome::Failed(e) if e.contains("炸了")));
    }

    #[test]
    fn cancel_flag_visible_to_closure() {
        let t = BackgroundTask::spawn(|cancel| {
            // 自旋到被取消(测试里立即 request_cancel)
            while !cancel.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            Ok("已响应取消".to_string())
        });
        t.request_cancel();
        assert!(matches!(t.join_for_test(), TaskOutcome::Done(_)));
    }
}
```
- [ ] **Step 2** 跑 → FAIL。
- [ ] **Step 3（绿）** 实现:
```rust
//! 后台任务:在独立线程跑长任务(archive/verify),UI 轮询结果。取消用 Arc<AtomicBool>(项目边界)。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// 任务最终结果(给 UI 呈现 Done/Failed/Cancelled 三态)。
#[derive(Debug)]
pub enum TaskOutcome {
    Done(String),
    Failed(String),
}

/// UI 侧看到的任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Finished,
}

pub struct BackgroundTask {
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    result: Arc<Mutex<Option<TaskOutcome>>>,
}

impl BackgroundTask {
    /// 起一个后台线程跑 `f`(`f` 收到 cancel 标志,可在项目边界检查)。
    pub fn spawn<F>(f: F) -> Self
    where
        F: FnOnce(&AtomicBool) -> anyhow::Result<String> + Send + 'static,
    {
        let cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let cancel_t = Arc::clone(&cancel);
        let result_t = Arc::clone(&result);
        let handle = std::thread::spawn(move || {
            let outcome = match f(&cancel_t) {
                Ok(s) => TaskOutcome::Done(s),
                Err(e) => TaskOutcome::Failed(format!("{:#}", e)),
            };
            *result_t.lock().unwrap() = Some(outcome);
        });
        Self { cancel, handle: Some(handle), result }
    }

    /// 请求取消(置位标志;core 在项目边界读到后安全收尾)。
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 线程是否已结束(UI 每帧轮询;true 后可 take_outcome)。
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().map(|h| h.is_finished()).unwrap_or(true)
    }

    /// 取走最终结果(join 线程)。仅在 is_finished 后调用。
    pub fn take_outcome(&mut self) -> Option<TaskOutcome> {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.result.lock().unwrap().take()
    }

    #[cfg(test)]
    fn join_for_test(mut self) -> TaskOutcome {
        if let Some(h) = self.handle.take() {
            h.join().unwrap();
        }
        self.result.lock().unwrap().take().unwrap()
    }
}
```
- [ ] **Step 4** test + clippy + fmt 绿。
- [ ] **Step 5** commit：`feat(gui): BackgroundTask 后台执行 + 取消 + 三态结果 (Phase2 T3; Spec D §3/§4.5)`

---

### Task 4：`App` 壳 + 左侧栏 + 视图路由

**Files:** Create `crates/bftool-gui/src/app.rs`、`crates/bftool-gui/src/views/mod.rs`;Modify `src/main.rs`(改用 `app::App`)

- [ ] **Step 1（红）** 纯逻辑测试(视图枚举与默认):
```rust
#[test]
fn default_view_is_dashboard() {
    assert_eq!(View::default(), View::Dashboard);
}
#[test]
fn all_views_have_labels() {
    for v in View::ALL {
        assert!(!v.label().is_empty());
    }
}
```
- [ ] **Step 2** 跑 → FAIL。
- [ ] **Step 3（绿）** `app.rs`:
```rust
//! App 壳:左侧栏切换视图,中央面板渲染当前视图。长任务一律走 task::BackgroundTask。
use eframe::egui;

use crate::reporter::{ProgressState, UiEvent};
use crate::task::BackgroundTask;

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
        View::Dashboard, View::Archive, View::Verify,
        View::Init, View::Find, View::Drives, View::Settings,
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
    pub cfg: bftool_core::config::Config,
    pub config_source: bftool_core::config::ConfigSource,
    pub logs: Vec<(bftool_core::reporter::LogLevel, String)>,
    pub progress: std::sync::Arc<std::sync::Mutex<ProgressState>>,
    pub rx: Option<std::sync::mpsc::Receiver<UiEvent>>,
    pub task: Option<BackgroundTask>,
    pub last_summary: Option<String>,
    // 备份页状态(plan 预览)在 T6 填充
    pub archive_plan: Option<bftool_core::engine::archive::ArchivePlan>,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let loaded = bftool_core::config::Config::load_with_source(None)
            .unwrap_or_else(|_| bftool_core::config::Config::default_loaded());
        Self {
            view: View::default(),
            cfg: loaded.config,
            config_source: loaded.source,
            logs: Vec::new(),
            progress: std::sync::Arc::new(std::sync::Mutex::new(ProgressState::default())),
            rx: None,
            task: None,
            last_summary: None,
            archive_plan: None,
        }
    }

    /// 每帧:drain 日志 channel + 轮询后台任务完成。
    fn pump(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.rx {
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    UiEvent::Log { level, msg } => self.logs.push((level, msg)),
                }
            }
        }
        if let Some(t) = &self.task {
            if t.is_finished() {
                if let Some(outcome) = self.task.as_mut().unwrap().take_outcome() {
                    self.last_summary = Some(match outcome {
                        crate::task::TaskOutcome::Done(s) => s,
                        crate::task::TaskOutcome::Failed(e) => format!("失败：{}", e),
                    });
                }
                self.task = None;
                self.rx = None;
            } else {
                ctx.request_repaint(); // 任务进行中:持续刷新进度/日志
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump(ctx);
        egui::SidePanel::left("nav").resizable(false).show(ctx, |ui| {
            ui.heading("bftool");
            ui.separator();
            let running = self.task.is_some();
            for v in View::ALL {
                // 任务进行中禁用导航,避免切走正在跑的备份页(简单稳妥;后续可放开只读视图)
                ui.add_enabled_ui(!running || v == self.view, |ui| {
                    if ui.selectable_label(self.view == v, v.label()).clicked() {
                        self.view = v;
                    }
                });
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| match self.view {
            View::Dashboard => crate::views::dashboard::ui(self, ui),
            View::Archive => crate::views::archive::ui(self, ui),
            other => {
                ui.heading(other.label());
                ui.label("（此视图将在 Phase 3 接入）");
            }
        });
    }
}
```
> 依赖:`Config::default_loaded()`(返回 `LoadedConfig{ default, ConfigSource::Default }`)。若 config.rs 没有此 helper,本 task **加一个**(core 小改 + 单测:返回 source==Default);或就地用 `LoadedConfig{ config: Config::default(), source: ConfigSource::Default }` 构造(需 ConfigSource/LoadedConfig 字段 pub —— 已是)。优先就地构造,不动 core。
- [ ] **Step 4** `views/mod.rs`:`pub mod dashboard; pub mod archive;`。`src/main.rs` 改用 `app::App::new`,声明 `mod app; mod reporter; mod task; mod views;`。
- [ ] **Step 5** `cargo build -p bftool-gui` + test + clippy + fmt 绿(dashboard/archive 先放最小 `pub fn ui(app,ui){}` 占位让它编译,具体在 T5/T6)。
- [ ] **Step 6** commit：`feat(gui): App 壳 + 左侧栏 7 视图路由 + 日志/任务 pump (Phase2 T4; Spec D §5)`

---

### Task 5：仪表盘视图(消费 `status::gather`)

**Files:** Create/Modify `crates/bftool-gui/src/views/dashboard.rs`

- [ ] **Step 1** 实现 `pub fn ui(app: &mut App, ui: &mut egui::Ui)`:调 `status::gather(&app.cfg)` 拿 `StatusReport`,渲染:当前盘(在线 BackupDrive 列表 + 容量)、待备份数(`pending_count`)、各盘上次复查(`last_verify`,无则"未知")、事务残留提示;底部三大按钮「备份 / 复查 / 初始化新盘」→ 切到对应视图(`app.view = View::Archive` 等)。首次无配置或无盘 → 顶部 `ui.colored_label` 横幅引导去设置/init。`gather` 出错 → 显示 error 文案(带"怎么修",不崩)。
- [ ] **Step 2** 纯逻辑(若抽了格式化 helper,如 `fmt_gb(bytes)->String`)配单测;视图渲染靠编译。
- [ ] **Step 3** `cargo build -p bftool-gui` + test + clippy + fmt 绿。
- [ ] **Step 4** commit：`feat(gui): 仪表盘视图消费 status::gather + 三大入口按钮 (Phase2 T5; Spec D §5)`

---

### Task 6：备份页(plan → 预览 → 后台 run_plan + 进度/日志/取消)

**Files:** Create/Modify `crates/bftool-gui/src/views/archive.rs`

这是 Phase 2 的核心交互。**绝不在 UI 线程跑 run_plan**。

- [ ] **Step 1** 实现 `pub fn ui(app: &mut App, ui: &mut egui::Ui)`:
  - **「演练 / 刷新计划」按钮** → `archive::plan(&app.cfg, &opts, &noop_or_capture_reporter)`(plan 只读,可用 `NoopReporter` 或一个就地收集 warning 的 reporter)→ 存 `app.archive_plan`;渲染 items 表(name / est_bytes / action 文案:Archive/RenameAndArchive/Skip(reason)/SealAndStop(reason))。plan 出错(无可写盘等)→ 显示提示。
  - **「正式备份」按钮**(仅在有 plan 且无进行中任务时可点):建 `mpsc::channel` + `Arc<Mutex<ProgressState>>`(复用 `app.progress`,先 reset),建 `GuiReporter`;`app.rx = Some(rx)`;`app.task = Some(BackgroundTask::spawn(move |cancel| { let s = archive::run_plan(&cfg, &plan, cancel, &reporter)?; Ok(format!("完成 {} 项,失败 {},{}", s.handled, s.failed, if s.cancelled {"已取消"} else {"未取消"})) }))`。`cfg`/`plan`/`reporter` move 进闭包(plan 需 Clone —— `ArchivePlan: Clone` 已具备)。
  - **进度条**:读 `app.progress.lock()`,`active` 时 `ui.add(egui::ProgressBar::new(current/total))` + label。
  - **日志面板**:`egui::ScrollArea` 滚动显示 `app.logs`(按 level 上色)。
  - **「取消」按钮**(任务进行中可点)→ `app.task.as_ref().unwrap().request_cancel()`;提示"已请求取消,将在当前项目完成后停止"。
  - **汇总**:`app.last_summary` 有值时显示。
  - **高级设置(默认折叠 `egui::CollapsingHeader`)**:limit / 指定盘 / stable_minutes / reserve_gb / no_test_archives / unsafe_no_hash(+ 勾选确认)。**危险组合**(no_hash + 关测试)沿用 core `verify_disabled`——点正式备份前先 `if verify_disabled(..) { 弹确认/拒绝 }`,不绕过 core fail-closed。
- [ ] **Step 2** 纯逻辑测试:`action 文案` 格式化函数 `fn action_text(&PlanAction)->String`(给定四种 action 返回含关键字的文案)配单测。
- [ ] **Step 3** `cargo build -p bftool-gui` + `cargo test --workspace --all-targets` + clippy + fmt 全绿。
- [ ] **Step 4** commit：`feat(gui): 备份页 plan 预览 + 后台 run_plan(GuiReporter/进度/取消) (Phase2 T6; Spec D §3/§5)`
- [ ] **Step 5** 开 PR `desktop-gui` → main(Phase 2),CI 真机验证 eframe 能 build + 测试绿;绿后 squash 合并(我自管)。

---

## Self-Review(plan vs Spec D §3/§5/§7)

- §3 后台线程跑 `run_plan`、`GuiReporter`→channel/进度、UI try_recv+request_repaint、取消项目边界 → T2(reporter)+T3(task)+T4(pump)+T6(接线)✓
- §5 左侧栏 7 视图 → T4(路由,7 个都在;dashboard/archive 实现,其余 placeholder 留 Phase 3)✓;仪表盘 → T5 ✓;备份页(plan 预览 + 演练 + 正式 + 进度 + 取消 + 高级设置 + 危险组合 fail-closed)→ T6 ✓
- §7 文件结构(main/app/views/reporter/task)→ 全覆盖 ✓;测试边界(纯逻辑单测 + 渲染靠编译)→ 各 task 已遵守 ✓
- §4.5 取消(archive 项目边界)→ T3 cancel + T6 接线 ✓
- **不在 Phase 2**:verify/init/find/drives/settings 五视图 → Phase 3 独立 plan(T4 已留 placeholder 入口)。
- 占位符扫描:reporter/task 给了完整可编译代码 + 测试;app 给了完整 update 循环;views 给了数据源 + 交互契约(渲染细节执行时按 egui API 落地)。
- 类型一致性:`UiEvent`/`ProgressState`/`GuiReporter`/`BackgroundTask`/`TaskOutcome`/`View`/`ArchivePlan`(复用 core)跨 task 命名一致。
- 已知风险:eframe 在本机(ARM64)build——若失败以 CI 真机为准(T1 风险闸);GUI 渲染/交互无法无头自动验证,靠编译 + 用户实跑(Spec D §7 已认)。

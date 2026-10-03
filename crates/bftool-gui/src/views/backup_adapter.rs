//! Ordinary-directory backup adapter. Display rows never serve as executable plans.
use super::*;
use anyhow::{bail, Context};
use bftool_core::pipeline::backup::{
    BackupEntryKind, BackupOutcome, BackupPlan, BackupRequest, ConflictPolicy, SourceSelection,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct PlannedFileBackup {
    signature: String,
    plans: Arc<Vec<BackupPlan>>,
    display: FileBackupPlan,
    sources: Vec<SourceItem>,
    target_info: TargetInfo,
}

fn path_key(path: &Path) -> Vec<String> {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

fn plan_batch(
    sources: Vec<SourceItem>,
    target: PathBuf,
    cancel: &AtomicBool,
    reporter: &dyn bftool_core::reporter::Reporter,
) -> anyhow::Result<PlannedFileBackup> {
    if sources.is_empty() {
        bail!("请选择来源");
    }
    let mut canonical = Vec::new();
    for source in &sources {
        if cancel.load(Ordering::Relaxed) {
            bail!("计划已取消");
        }
        let path = source.path.canonicalize().context("来源路径无法解析")?;
        let key = path_key(&path);
        if canonical
            .iter()
            .any(|old: &Vec<String>| key.starts_with(old) || old.starts_with(&key))
        {
            bail!("多来源存在重叠、重复或大小写别名，请仅选择一次");
        }
        canonical.push(key);
    }
    let mut plans = Vec::new();
    let mut selected = Vec::new();
    let mut destinations = Vec::new();
    let mut items = Vec::new();
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut conflicts = 0usize;
    for mut source in sources {
        if cancel.load(Ordering::Relaxed) {
            bail!("计划已取消");
        }
        let metadata = std::fs::symlink_metadata(&source.path).context("来源无法读取")?;
        source.is_dir = metadata.is_dir();
        let request = BackupRequest {
            source: if source.is_dir {
                SourceSelection::Directory(source.path.clone())
            } else {
                SourceSelection::File(source.path.clone())
            },
            target_dir: target.clone(),
            conflict: ConflictPolicy::KeepBoth,
        };
        let plan = bftool_core::service::plan_backup(&request, cancel, reporter)?;
        let view = plan.view();
        let key = path_key(&view.destination_name);
        if destinations.contains(&key) {
            bail!("多来源将写入相同版本名，请分批预览与备份");
        }
        destinations.push(key);
        source.size_bytes = view.bytes;
        files = files
            .checked_add(usize::try_from(view.counts.files)?)
            .context("文件计数溢出")?;
        bytes = bytes.checked_add(view.bytes).context("字节计数溢出")?;
        conflicts += view.conflicts.len();
        for entry in &view.entries {
            let root = view.selected_target.join(&view.destination_name);
            // A selected file has an empty relative path. Joining it would append a
            // directory separator and misrepresent the actual published file on Windows.
            let destination = if entry.relative_path.as_os_str().is_empty() {
                root
            } else {
                root.join(&entry.relative_path)
            };
            let source_path = if source.is_dir {
                source.path.join(&entry.relative_path)
            } else {
                source.path.clone()
            };
            items.push(FilePlanItem {
                source_path,
                target_path: destination.clone(),
                rel_path: destination.display().to_string(),
                size_bytes: entry.bytes,
                status: FilePlanItemStatus::PendingCopy,
                reason: if entry.kind == BackupEntryKind::Directory {
                    "保留目录（含空目录）".into()
                } else if !view.conflicts.is_empty() {
                    "保留双份：写入显示的新版本名，SHA-256 校验".into()
                } else {
                    "复制并 SHA-256 校验，不覆盖已有内容".into()
                },
            });
        }
        selected.push(source);
        plans.push(plan);
    }
    let target_info = query_target_info(&target);
    let display = FileBackupPlan {
        items,
        pending_count: files,
        pending_bytes: bytes,
        already_exists_count: 0,
        already_exists_bytes: 0,
        conflict_count: conflicts,
        conflict_bytes: 0,
        unreadable_count: 0,
        unreadable_bytes: 0,
        has_insufficient_space: target_info.total_space > 0 && bytes > target_info.available_space,
        space_needed_bytes: bytes,
        space_available_bytes: target_info.available_space,
    };
    Ok(PlannedFileBackup {
        signature: String::new(),
        plans: Arc::new(plans),
        display,
        sources: selected,
        target_info,
    })
}

pub(super) fn generate(app: &mut App) {
    app.backup_ui.plan = None;
    app.backup_ui.core_plans = None;
    app.backup_ui.core_signature.clear();
    app.backup_ui.last_outcome = None;
    let signature = app.backup_ui.current_signature();
    let sources = app.backup_ui.sources.clone();
    let target = PathBuf::from(&app.backup_ui.target_path);
    let (tx, rx) = mpsc::channel();
    app.rx = Some(rx);
    let reporter = crate::reporter::GuiReporter::new(tx, Arc::clone(&app.progress));
    app.file_backup_plan_task = Some(BackgroundTask::spawn(move |cancel| {
        let mut planned = plan_batch(sources, target, cancel, &reporter)?;
        planned.signature = signature;
        Ok(planned)
    }));
}

pub(crate) fn pump(app: &mut App, ctx: &egui::Context) {
    use crate::task::TaskOutcome;
    match app.file_backup_plan_task.as_ref().map(|t| t.is_finished()) {
        Some(true) => {
            let cancelled = app
                .file_backup_plan_task
                .as_ref()
                .is_some_and(|t| t.cancel_requested());
            let result = app
                .file_backup_plan_task
                .as_mut()
                .and_then(|t| t.take_outcome());
            app.file_backup_plan_task = None;
            match result {
                Some(TaskOutcome::Done(p))
                    if !cancelled && p.signature == app.backup_ui.current_signature() =>
                {
                    app.backup_ui.sources = p.sources;
                    app.backup_ui.target_info = Some(p.target_info);
                    app.backup_ui.plan = Some(p.display);
                    app.backup_ui.core_plans = Some(p.plans);
                    app.backup_ui.plan_signature = app.backup_ui.current_signature();
                    app.backup_ui.core_signature = app.backup_ui.current_signature();
                    app.backup_ui.validation = RelationValidation::Valid;
                }
                Some(TaskOutcome::Failed(error)) => {
                    app.logs
                        .push((bftool_core::reporter::LogLevel::Error, error.clone()));
                    apply_outcome(app, failure(error));
                }
                _ => apply_outcome(app, failure("计划已取消或选择已改变，请重新生成".into())),
            }
            if let Some(rx) = app.rx.take() {
                for event in rx.try_iter() {
                    let crate::reporter::UiEvent::Log { level, msg } = event;
                    app.logs.push((level, msg));
                }
            }
            app.backup_ui.stopping_requested = false;
        }
        Some(false) => ctx.request_repaint(),
        None => {}
    }
    if app.task_kind == Some(crate::app::TaskKind::Backup) && !app.backend.is_demo() {
        if let Ok(progress) = app.progress.lock() {
            app.backup_ui.current_file = progress.label.clone();
            app.backup_ui.transferred_bytes = progress.current;
            app.backup_ui.total_bytes = progress.total;
        }
    }
}

pub(super) fn start(app: &mut App) {
    app.backup_ui.check_invalidate_plan();
    if app.backup_ui.current_signature() != app.backup_ui.core_signature {
        app.backup_ui.core_plans = None;
        return;
    }
    let Some(plans) = app.backup_ui.core_plans.clone() else {
        app.logs.push((
            bftool_core::reporter::LogLevel::Warn,
            "请先生成真实核心计划".into(),
        ));
        return;
    };
    if app
        .backup_ui
        .plan
        .as_ref()
        .is_some_and(|p| p.has_insufficient_space)
    {
        return;
    }
    app.backup_ui.is_running = true;
    app.backup_ui.stopping_requested = false;
    app.backup_ui.last_outcome = None;
    app.backup_ui.done_files = 0;
    app.backup_ui.transferred_bytes = 0;
    app.backup_ui.total_files = plans.iter().map(|p| p.view().counts.files as usize).sum();
    app.backup_ui.total_bytes = plans.iter().map(|p| p.view().bytes).sum();
    app.backup_ui.speed_bps = None;
    app.backup_ui.eta_secs = None;
    app.task_kind = Some(crate::app::TaskKind::Backup);
    app.task_started = Some(std::time::Instant::now());
    let (tx, rx) = mpsc::channel();
    app.rx = Some(rx);
    let reporter = crate::reporter::GuiReporter::new(tx, Arc::clone(&app.progress));
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        Ok(crate::app::ExecutionResult::Backup(run_batch(
            &plans, cancel, &reporter,
        )))
    }));
}

#[derive(Default)]
struct BatchResult {
    copied: u64,
    verified: u64,
    bytes: u64,
    failed: u64,
    published_files: u64,
    published_bytes: u64,
    published_jobs: u64,
    issues: Vec<String>,
    stopped: bool,
    unpublished: bool,
}
impl BatchResult {
    fn add(&mut self, summary: &bftool_core::pipeline::backup::BackupSummary) {
        self.copied += summary.copied;
        self.verified += summary.verified;
        self.bytes += summary.bytes;
        self.failed += summary.failed;
        self.issues.extend(summary.issues.iter().cloned());
        self.stopped |= summary.outcome == BackupOutcome::Cancelled;
        self.unpublished |= !summary.published;
        if summary.published {
            self.published_files += summary.copied + summary.skipped_verified;
            self.published_bytes += summary.bytes;
            self.published_jobs += 1;
        }
    }
    fn add_error(&mut self, error: anyhow::Error) {
        if let Some(partial) =
            error.downcast_ref::<bftool_core::pipeline::backup::BackupExecutionError>()
        {
            self.add(&partial.summary);
        } else {
            self.failed += 1;
            self.issues.push(format!("{error:#}"));
            self.unpublished = true;
        }
    }
    fn outcome(self) -> BackupOutcomeSummary {
        let kind = if self.stopped {
            BackupOutcomeKind::Stopped
        } else if self.failed > 0 || !self.issues.is_empty() {
            if self.published_jobs > 0 {
                BackupOutcomeKind::PartialComplete
            } else {
                BackupOutcomeKind::Failed
            }
        } else if self.unpublished {
            BackupOutcomeKind::NotVerified
        } else {
            BackupOutcomeKind::Success
        };
        let detail = format!("来源保留。复制/复验进度 {} 项、校验 {} 项、{}；只有确认发布的 {} 个任务计入完成备份。{}{}",
            self.copied, self.verified, util::fmt_gb(self.bytes), self.published_jobs,
            if self.unpublished { "含未发布或发布状态未确认的数据；不得视为完成备份。" } else { "" },
            if self.issues.is_empty() { if self.stopped { "已响应取消，后续来源未执行。".into() } else { String::new() } } else { self.issues.join("\n") });
        BackupOutcomeSummary {
            kind,
            title: format!(
                "确认发布 {} 项（{}），失败 {} 项",
                self.published_files,
                util::fmt_gb(self.published_bytes),
                self.failed
            ),
            detail,
            completed_items: self.published_files as usize,
            completed_bytes: self.published_bytes,
            failed_items: self.failed as usize,
            is_content_verified: !self.stopped
                && !self.unpublished
                && self.failed == 0
                && self.issues.is_empty(),
        }
    }
}
fn run_batch(
    plans: &[BackupPlan],
    cancel: &AtomicBool,
    reporter: &dyn bftool_core::reporter::Reporter,
) -> BackupOutcomeSummary {
    let mut result = BatchResult::default();
    for plan in plans {
        if cancel.load(Ordering::Relaxed) {
            result.stopped = true;
            break;
        }
        match bftool_core::service::run_backup_plan(plan, cancel, reporter) {
            Ok(summary) => {
                result.add(&summary);
                if summary.outcome != BackupOutcome::Completed
                    || summary.failed > 0
                    || !summary.issues.is_empty()
                    || !summary.published
                {
                    break;
                }
            }
            Err(error) => {
                result.add_error(error);
                break;
            }
        }
    }
    result.outcome()
}

pub(crate) fn resume(app: &mut App, job_id: String) {
    if app.backend.is_demo() || app.backup_ui.target_path.is_empty() || !app.ensure_idle() {
        return;
    }
    let target = PathBuf::from(&app.backup_ui.target_path);
    app.backup_ui.is_running = true;
    app.backup_ui.stopping_requested = false;
    app.backup_ui.last_outcome = None;
    app.backup_ui.speed_bps = None;
    app.backup_ui.eta_secs = None;
    app.backup_ui.done_files = 0;
    app.backup_ui.transferred_bytes = 0;
    app.task_kind = Some(crate::app::TaskKind::Backup);
    app.task_started = Some(std::time::Instant::now());
    let (tx, rx) = mpsc::channel();
    app.rx = Some(rx);
    let reporter = crate::reporter::GuiReporter::new(tx, Arc::clone(&app.progress));
    app.task = Some(BackgroundTask::spawn(move |cancel| {
        let mut result = BatchResult::default();
        match bftool_core::service::resume_backup(&target, &job_id, cancel, &reporter) {
            Ok(summary) => result.add(&summary),
            Err(error) => result.add_error(error),
        }
        Ok(crate::app::ExecutionResult::Backup(result.outcome()))
    }));
}
pub(crate) fn failure(error: String) -> BackupOutcomeSummary {
    BackupOutcomeSummary {
        kind: BackupOutcomeKind::Failed,
        title: "备份未完成".into(),
        detail: error,
        completed_items: 0,
        completed_bytes: 0,
        failed_items: 1,
        is_content_verified: false,
    }
}
pub(crate) fn apply_outcome(app: &mut App, outcome: BackupOutcomeSummary) {
    app.backup_ui.done_files = outcome.completed_items;
    app.backup_ui.transferred_bytes = outcome.completed_bytes;
    app.backup_ui.last_outcome = Some(outcome);
}

#[cfg(test)]
mod tests {
    use super::*;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct SyntheticDir(PathBuf);
    impl SyntheticDir {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "bftool-gui-synthetic-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for SyntheticDir {
        fn drop(&mut self) {
            let root = self.0.canonicalize().unwrap();
            let temporary_root = std::env::temp_dir().canonicalize().unwrap();
            assert_eq!(root.parent(), Some(temporary_root.as_path()));
            assert!(root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("bftool-gui-synthetic-"));
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    fn app_for(root: &SyntheticDir) -> App {
        let mut app = crate::app::tests::fixture();
        app.backend = crate::backend::Backend::Live;
        std::fs::create_dir(root.0.join("target")).unwrap();
        set_target_folder(&mut app, root.0.join("target"));
        app
    }
    fn await_plan(app: &mut App) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let ctx = egui::Context::default();
        while app.file_backup_plan_task.is_some() {
            assert!(std::time::Instant::now() < deadline, "plan timeout");
            pump(app, &ctx);
            std::thread::yield_now();
        }
    }
    fn finish_run(app: &mut App) -> BackupOutcomeSummary {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !app.task.as_ref().unwrap().is_finished() {
            assert!(std::time::Instant::now() < deadline, "run timeout");
            std::thread::yield_now();
        }
        let result = app.task.as_mut().unwrap().take_outcome().unwrap();
        app.task = None;
        app.task_kind = None;
        app.backup_ui.is_running = false;
        app.backup_ui.stopping_requested = false;
        match result {
            crate::task::TaskOutcome::Done(crate::app::ExecutionResult::Backup(summary)) => summary,
            other => panic!(
                "unexpected result: {}",
                match other {
                    crate::task::TaskOutcome::Failed(e) => e,
                    _ => "wrong result kind".into(),
                }
            ),
        }
    }

    #[test]
    fn direct_integration_same_size_conflict_keeps_both_source_and_unknown_target() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let source = root.0.join("movie.dat");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(root.0.join("target/movie.dat"), b"oldold").unwrap();
        add_source_path(&mut app, source.clone());
        generate_plan_action(&mut app);
        assert!(app.file_backup_plan_task.is_some());
        assert!(app.is_busy());
        await_plan(&mut app);
        let display = app.backup_ui.plan.as_ref().unwrap();
        assert_eq!(display.already_exists_count, 0);
        assert_eq!(display.pending_count, 1);
        let destination = display.items[0].target_path.clone();
        assert_ne!(destination, root.0.join("target/movie.dat"));
        // The public preview is editable; execution must retain its private core token.
        app.backup_ui.plan.as_mut().unwrap().items[0].source_path = root.0.join("missing");
        app.backup_ui.plan.as_mut().unwrap().items[0].target_path = source.clone();
        start_backup_action(&mut app);
        let summary = finish_run(&mut app);
        assert_eq!(summary.kind, BackupOutcomeKind::Success);
        assert!(summary.is_content_verified);
        assert_eq!(std::fs::read(&source).unwrap(), b"source");
        assert_eq!(
            std::fs::read(root.0.join("target/movie.dat")).unwrap(),
            b"oldold"
        );
        assert_eq!(std::fs::read(destination).unwrap(), b"source");
    }

    #[test]
    fn direct_integration_changed_selection_cannot_execute_old_token() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let source = root.0.join("item.dat");
        std::fs::write(&source, b"data").unwrap();
        add_source_path(&mut app, source);
        generate_plan_action(&mut app);
        await_plan(&mut app);
        assert!(app.backup_ui.plan.is_some());
        app.backup_ui.sources.clear();
        // Even forging the public display signature cannot authorize the old core token.
        app.backup_ui.plan_signature = app.backup_ui.current_signature();
        start_backup_action(&mut app);
        assert!(app.task.is_none());
        assert!(app.backup_ui.core_plans.is_none());
        assert_eq!(std::fs::read_dir(root.0.join("target")).unwrap().count(), 0);
    }

    #[test]
    fn direct_integration_overlap_rejected_and_empty_folder_preserved() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let folder = root.0.join("folder");
        std::fs::create_dir_all(folder.join("empty")).unwrap();
        std::fs::write(folder.join("file"), b"data").unwrap();
        add_source_path(&mut app, folder.clone());
        add_source_path(&mut app, folder.join("file"));
        generate_plan_action(&mut app);
        await_plan(&mut app);
        assert!(app.backup_ui.plan.is_none());
        app.backup_ui.sources.pop();
        generate_plan_action(&mut app);
        await_plan(&mut app);
        start_backup_action(&mut app);
        let summary = finish_run(&mut app);
        assert_eq!(summary.kind, BackupOutcomeKind::Success);
        assert!(root.0.join("target/folder/empty").is_dir());
        assert!(folder.join("file").is_file());
    }

    #[test]
    fn direct_integration_completed_first_batch_failure_stops_later_sources() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        for name in ["first", "second", "third"] {
            let path = root.0.join(name);
            std::fs::write(&path, b"data").unwrap();
            add_source_path(&mut app, path);
        }
        generate_plan_action(&mut app);
        await_plan(&mut app);
        std::fs::write(root.0.join("second"), b"edit").unwrap();
        start_backup_action(&mut app);
        let summary = finish_run(&mut app);
        assert_eq!(summary.kind, BackupOutcomeKind::PartialComplete);
        assert_eq!(summary.completed_items, 1);
        assert!(!summary.is_content_verified);
        assert_eq!(std::fs::read(root.0.join("target/first")).unwrap(), b"data");
        assert!(!root.0.join("target/second").exists());
        assert!(!root.0.join("target/third").exists());
        assert_eq!(std::fs::read(root.0.join("second")).unwrap(), b"edit");
    }

    #[test]
    fn direct_integration_changed_inflight_inputs_and_cancelled_plan_are_discarded() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let path = root.0.join("item");
        std::fs::write(&path, b"data").unwrap();
        add_source_path(&mut app, path);
        generate_plan_action(&mut app);
        app.backup_ui.sources.clear();
        await_plan(&mut app);
        assert!(app.backup_ui.core_plans.is_none());
        add_source_path(&mut app, root.0.join("item"));
        generate_plan_action(&mut app);
        request_stop_backup(&mut app);
        assert!(app.backup_ui.stopping_requested);
        await_plan(&mut app);
        assert!(app.backup_ui.core_plans.is_none());
        assert!(!app.backup_ui.stopping_requested);
        assert_eq!(std::fs::read_dir(root.0.join("target")).unwrap().count(), 0);
    }

    fn await_history(app: &mut App) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let ctx = egui::Context::default();
        while app.tasks_ui.history_task.is_some() {
            assert!(std::time::Instant::now() < deadline, "history timeout");
            crate::views::tasks::pump_history(app, &ctx);
            std::thread::yield_now();
        }
    }

    #[test]
    fn direct_integration_live_verify_and_persistent_history_without_disk_scan() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let path = root.0.join("item");
        std::fs::write(&path, b"data").unwrap();
        add_source_path(&mut app, path);
        generate_plan_action(&mut app);
        await_plan(&mut app);
        start_backup_action(&mut app);
        assert_eq!(finish_run(&mut app).kind, BackupOutcomeKind::Success);
        let target = root.0.join("target");
        assert_eq!(std::path::Path::new(&app.backup_ui.target_path), target);
        crate::views::verify::start_plain_verify(&mut app, target.clone());
        assert_eq!(app.task_kind, Some(crate::app::TaskKind::Verify));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !app.task.as_ref().unwrap().is_finished() {
            assert!(std::time::Instant::now() < deadline, "verify timeout");
            std::thread::yield_now();
        }
        // The real producer must reach App's target/token-bound consumer.
        // Rendering pumps the completed task without taking its result here.
        let ctx = crate::app::tests::headless_context();
        let _ = ctx.run(Default::default(), |ctx| app.render(ctx));
        let report = app
            .verify_ui
            .last_report
            .as_ref()
            .expect("expected accepted real verification report");
        assert_eq!(report.checked, 1);
        assert_eq!(report.bad, 0);
        assert!(!report.cancelled);
        assert_eq!(std::path::Path::new(&app.backup_ui.target_path), target);
        assert!(app
            .verify_ui
            .summary
            .as_ref()
            .unwrap()
            .contains(target.to_str().unwrap()));
        assert!(app.verify_ui.error.is_none());
        assert!(app.task.is_none());
        assert!(app.task_kind.is_none());
        assert!(app.task_started.is_none());
        assert!(app.rx.is_none());
        assert!(!app.is_busy());
        assert!(app.verify_ui.drives.is_none());
        drop(app);
        // A new GUI state reads persisted core metadata, without session summary.
        let mut reopened = crate::app::tests::fixture();
        reopened.backend = crate::backend::Backend::Live;
        set_target_folder(&mut reopened, target.clone());
        assert_eq!(
            std::path::Path::new(&reopened.backup_ui.target_path),
            target
        );
        assert!(reopened.last_summary.is_none());
        crate::views::tasks::refresh_history(&mut reopened);
        assert!(reopened.is_busy());
        await_history(&mut reopened);
        assert!(reopened.tasks_ui.loaded);
        assert_eq!(reopened.tasks_ui.records.len(), 1);
        assert!(reopened.tasks_ui.records[0].completed);
        assert!(reopened.tasks_ui.error.is_none());
        assert!(reopened.tasks_ui.history_task.is_none());
        assert!(!reopened.is_busy());
    }

    #[test]
    fn direct_integration_cancelled_staging_not_completed_then_gui_file_resume() {
        use bftool_core::reporter::{LogLevel, NoopReporter, ProgressHandle, Reporter};
        struct CancelBeforePublish<'a>(&'a AtomicBool);
        impl Reporter for CancelBeforePublish<'_> {
            fn log(&self, _: LogLevel, message: &str) {
                if message == "Verifying direct backup" {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
            fn progress_bytes(&self, label: &str, bytes: u64) -> Box<dyn ProgressHandle> {
                NoopReporter.progress_bytes(label, bytes)
            }
        }
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let source = root.0.join("item");
        std::fs::write(&source, b"data").unwrap();
        add_source_path(&mut app, source.clone());
        generate_plan_action(&mut app);
        await_plan(&mut app);
        let cancel = AtomicBool::new(false);
        let plans = app.backup_ui.core_plans.as_ref().unwrap();
        let staged = bftool_core::service::run_backup_plan(
            &plans[0],
            &cancel,
            &CancelBeforePublish(&cancel),
        )
        .unwrap();
        assert_eq!(staged.outcome, BackupOutcome::Cancelled);
        assert!(!staged.published);
        assert_eq!(staged.copied, 1);
        let mut result = BatchResult::default();
        result.add(&staged);
        let outcome = result.outcome();
        assert_eq!(outcome.kind, BackupOutcomeKind::Stopped);
        assert_eq!(outcome.completed_items, 0);
        assert!(outcome.detail.contains("未发布"));
        crate::views::tasks::refresh_history(&mut app);
        await_history(&mut app);
        assert_eq!(app.tasks_ui.records.len(), 1);
        assert!(!app.tasks_ui.records[0].completed);
        let job = app.tasks_ui.records[0].job_id.clone();
        resume(&mut app, job);
        assert_eq!(app.task_kind, Some(crate::app::TaskKind::Backup));
        let summary = finish_run(&mut app);
        assert_eq!(summary.kind, BackupOutcomeKind::Success);
        assert_eq!(summary.completed_items, 1);
        assert!(summary.is_content_verified);
        assert_eq!(std::fs::read(&source).unwrap(), b"data");
        assert_eq!(std::fs::read(root.0.join("target/item")).unwrap(), b"data");
    }
}

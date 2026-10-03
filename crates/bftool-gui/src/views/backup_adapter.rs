//! Ordinary-directory backup adapter. Display rows never serve as executable plans.
use super::*;
use anyhow::{bail, Context};
use bftool_core::pipeline::backup::{
    BackupEntryKind, BackupOutcome, BackupPlan, BackupRequest, ConflictPolicy, DirectoryOptions,
    SourceSelection,
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
            directory_options: Some(if source.is_dir {
                source.folder_filter.directory_options()?
            } else {
                DirectoryOptions::default()
            }),
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
        for entry in view.entries.iter().filter(|_| !has_no_matches(&plan)) {
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

pub(super) fn has_no_matches(plan: &BackupPlan) -> bool {
    let options = plan.request().effective_directory_options();
    matches!(plan.request().source, SourceSelection::Directory(_))
        && (options.extensions.is_some() || !options.include_extensionless)
        && plan.view().counts.files == 0
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
    if plans.iter().any(has_no_matches) || app.backup_ui.filter_error().is_some() {
        apply_outcome(
            app,
            failure("没有匹配文件，整个批次未开始；请修改筛选后重新预览。".into()),
        );
        return;
    }
    app.backup_ui.is_running = true;
    app.backup_ui.stopping_requested = false;
    app.backup_ui.last_outcome = None;
    app.backup_ui.done_files = 0;
    app.backup_ui.transferred_bytes = 0;
    app.backup_ui.recovery_totals_unknown = false;
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
    if plans.iter().any(has_no_matches) {
        return failure("没有匹配文件，整个批次未开始。".into());
    }
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
    app.backup_ui.total_files = 0;
    app.backup_ui.total_bytes = 0;
    app.backup_ui.current_file.clear();
    app.backup_ui.recovery_totals_unknown = true;
    app.last_summary = None;
    if let Ok(mut progress) = app.progress.lock() {
        *progress = crate::reporter::ProgressState::default();
    }
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
    fn folder_filter_gui_new_folder_defaults_to_shallow_worker_snapshot() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let folder = root.0.join("archives");
        std::fs::create_dir_all(folder.join("nested")).unwrap();
        std::fs::write(folder.join("top.zip"), b"root").unwrap();
        std::fs::write(folder.join("nested/deep.zip"), b"nested").unwrap();
        add_source_path(&mut app, folder.clone());
        generate_plan_action(&mut app);
        await_plan(&mut app);
        let display = app.backup_ui.plan.as_ref().unwrap();
        assert_eq!(
            display.pending_count, 1,
            "new GUI selection must be shallow"
        );
        assert_eq!(display.pending_bytes, 4);
        start_backup_action(&mut app);
        let summary = finish_run(&mut app);
        assert_eq!(summary.kind, BackupOutcomeKind::Success);
        assert!(root.0.join("target/archives/top.zip").is_file());
        assert!(!root.0.join("target/archives/nested").exists());
        assert!(folder.join("nested/deep.zip").is_file());
    }

    fn filtered_folder(
        app: &mut App,
        path: PathBuf,
        suffixes: &str,
        recursive: bool,
        extensionless: bool,
    ) {
        add_source_folder(app, path);
        let index = app.backup_ui.sources.len() - 1;
        set_folder_filter(
            app,
            index,
            FolderFilter {
                include_subfolders: recursive,
                enabled: true,
                extensions_input: suffixes.into(),
                include_extensionless: extensionless,
            },
        );
    }

    #[test]
    fn folder_filter_gui_independent_rules_selected_file_copy_verify_history() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let archives = root.0.join("archives");
        let photos = root.0.join("photos");
        std::fs::create_dir_all(archives.join("nested")).unwrap();
        std::fs::create_dir(&photos).unwrap();
        for (name, bytes) in [
            ("A.TAR", &b"tar"[..]),
            ("b.zip", &b"zip!"[..]),
            ("c.txt", &b"txt"[..]),
            ("a.tar.gz", &b"gz"[..]),
            ("nested/deep.zip", &b"deep"[..]),
        ] {
            std::fs::write(archives.join(name), bytes).unwrap();
        }
        std::fs::write(photos.join("one.JPG"), b"jpg").unwrap();
        std::fs::write(photos.join("README"), b"plain").unwrap();
        std::fs::write(photos.join("two.png"), b"png").unwrap();
        let single = root.0.join("single.txt");
        std::fs::write(&single, b"single").unwrap();
        filtered_folder(&mut app, archives.clone(), ".tar, ZIP", false, false);
        filtered_folder(&mut app, photos.clone(), "jpg", false, true);
        add_source_path(&mut app, single.clone());
        // A selected ordinary file bypasses a forged invalid folder draft.
        app.backup_ui.sources[2].folder_filter = FolderFilter {
            enabled: true,
            extensions_input: "../bad".into(),
            ..Default::default()
        };
        generate_plan_action(&mut app);
        await_plan(&mut app);
        assert_eq!(app.backup_ui.plan.as_ref().unwrap().pending_count, 5);
        assert_eq!(app.backup_ui.plan.as_ref().unwrap().pending_bytes, 21);
        assert_eq!(app.backup_ui.sources[0].size_bytes, 7);
        let plans = app.backup_ui.core_plans.as_ref().unwrap();
        assert_eq!(
            plans[0].request().effective_directory_options().extensions,
            Some(vec!["tar".into(), "zip".into()])
        );
        assert_eq!(
            plans[2].request().directory_options,
            Some(DirectoryOptions::default())
        );
        start_backup_action(&mut app);
        let summary = finish_run(&mut app);
        assert_eq!(summary.completed_items, 5);
        assert_eq!(summary.completed_bytes, 21);
        assert_eq!(summary.kind, BackupOutcomeKind::Success);
        let target = root.0.join("target");
        assert!(!target.join("archives/nested").exists());
        assert!(!target.join("archives/c.txt").exists());
        assert!(!target.join("archives/a.tar.gz").exists());
        assert!(!target.join("photos/two.png").exists());
        assert_eq!(std::fs::read(target.join("single.txt")).unwrap(), b"single");
        let report = bftool_core::service::verify_backup(
            &target,
            &AtomicBool::new(false),
            &bftool_core::reporter::NoopReporter,
        )
        .unwrap();
        assert_eq!(report.checked, 5);
        assert_eq!(report.bad, 0);
        crate::views::tasks::refresh_history(&mut app);
        await_history(&mut app);
        assert_eq!(app.tasks_ui.records.len(), 3);
        let record = app
            .tasks_ui
            .records
            .iter()
            .find(|r| r.source.path().file_name() == archives.file_name())
            .unwrap();
        assert!(!record.directory_options.recursive);
        assert_eq!(
            record.directory_options.extensions,
            Some(vec!["tar".into(), "zip".into()])
        );
        assert!(!record.directory_options.include_extensionless);
        // Render an actual persisted producer/consumer record, not a fabricated model.
        app.tasks_ui.records = vec![record.clone()];
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        fonts.families.insert(
            egui::FontFamily::Name("semibold".into()),
            fonts.families[&egui::FontFamily::Proportional].clone(),
        );
        ctx.set_fonts(fonts);
        let painted = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(700.0, 1000.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| crate::views::tasks::ui(&mut app, ui));
            },
        );
        let text: Vec<_> = painted
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(value) => Some(value.galley.text()),
                _ => None,
            })
            .collect();
        assert!(
            text.iter().any(|label| label.contains("仅当前层")
                && label.contains(".tar, .zip")
                && label.contains("无后缀文件：排除")),
            "actual history must display saved folder rules"
        );
        assert_eq!(
            std::fs::read(archives.join("nested/deep.zip")).unwrap(),
            b"deep"
        );
    }

    #[test]
    fn folder_filter_gui_recursion_opt_in_preserves_only_selected_structure() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let source = root.0.join("recursive");
        std::fs::create_dir_all(source.join("nested/deeper")).unwrap();
        std::fs::create_dir(source.join("empty")).unwrap();
        std::fs::write(source.join("nested/deeper/a.zip"), b"zip").unwrap();
        std::fs::write(source.join("nested/no.txt"), b"txt").unwrap();
        filtered_folder(&mut app, source, "zip", true, false);
        generate_plan_action(&mut app);
        await_plan(&mut app);
        assert_eq!(app.backup_ui.plan.as_ref().unwrap().pending_count, 1);
        start_backup_action(&mut app);
        assert_eq!(finish_run(&mut app).kind, BackupOutcomeKind::Success);
        assert!(root
            .0
            .join("target/recursive/nested/deeper/a.zip")
            .is_file());
        assert!(!root.0.join("target/recursive/empty").exists());
        assert!(!root.0.join("target/recursive/nested/no.txt").exists());
    }

    #[test]
    fn folder_filter_gui_zero_match_mixed_batch_rejects_forged_display_without_writes() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let single = root.0.join("first.txt");
        std::fs::write(&single, b"file").unwrap();
        add_source_path(&mut app, single);
        let source = root.0.join("zero");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.txt"), b"txt").unwrap();
        filtered_folder(&mut app, source.clone(), "zip", false, false);
        generate_plan_action(&mut app);
        await_plan(&mut app);
        let display = app.backup_ui.plan.as_ref().unwrap();
        assert_eq!(display.pending_count, 1);
        assert_eq!(display.pending_bytes, 4);
        assert!(display
            .items
            .iter()
            .all(|i| !i.source_path.starts_with(&source)));
        assert!(app.backup_ui.has_no_matches());
        app.backup_ui.plan.as_mut().unwrap().pending_count = 99;
        start_backup_action(&mut app);
        assert!(app.task.is_none());
        assert!(!app.backup_ui.is_running);
        assert_eq!(std::fs::read_dir(root.0.join("target")).unwrap().count(), 0);
        // Private batch runner must also reject before publishing its matching first source.
        let outcome = run_batch(
            app.backup_ui.core_plans.as_ref().unwrap(),
            &AtomicBool::new(false),
            &bftool_core::reporter::NoopReporter,
        );
        assert_eq!(outcome.kind, BackupOutcomeKind::Failed);
        assert_eq!(outcome.completed_items, 0);
        assert_eq!(std::fs::read_dir(root.0.join("target")).unwrap().count(), 0);
    }

    #[test]
    fn folder_filter_gui_rule_edits_invalidate_counts_tokens_and_block_invalid_drafts() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let source = root.0.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.zip"), b"zip").unwrap();
        filtered_folder(&mut app, source, "zip", false, false);
        for field in 0..4 {
            set_folder_filter(
                &mut app,
                0,
                FolderFilter {
                    enabled: true,
                    extensions_input: "zip".into(),
                    ..Default::default()
                },
            );
            generate_plan_action(&mut app);
            await_plan(&mut app);
            assert_eq!(app.backup_ui.sources[0].size_bytes, 3);
            let mut draft = app.backup_ui.sources[0].folder_filter.clone();
            match field {
                0 => draft.include_subfolders = true,
                1 => draft.enabled = false,
                2 => draft.extensions_input = "ZIP".into(),
                _ => draft.include_extensionless = true,
            }
            set_folder_filter(&mut app, 0, draft);
            assert!(app.backup_ui.plan.is_none());
            assert!(app.backup_ui.core_plans.is_none());
            assert_eq!(app.backup_ui.sources[0].size_bytes, 0);
            start_backup_action(&mut app);
            assert!(app.task.is_none());
        }
        let mut draft = app.backup_ui.sources[0].folder_filter.clone();
        draft.extensions_input = "../bad".into();
        set_folder_filter(&mut app, 0, draft);
        generate_plan_action(&mut app);
        assert!(app.file_backup_plan_task.is_none());
        assert!(app
            .backup_ui
            .last_outcome
            .as_ref()
            .unwrap()
            .detail
            .contains("suffix"));
        // Direct worker invocation has the same validation for programmatic callers.
        generate(&mut app);
        await_plan(&mut app);
        assert!(app.backup_ui.plan.is_none());
        assert_eq!(std::fs::read_dir(root.0.join("target")).unwrap().count(), 0);
    }

    #[test]
    fn folder_filter_gui_busy_edits_inert_and_forged_inflight_edit_discarded() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let source = root.0.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.zip"), b"zip").unwrap();
        filtered_folder(&mut app, source, "zip", false, false);
        generate_plan_action(&mut app);
        let old = app.backup_ui.current_signature();
        set_folder_filter(&mut app, 0, FolderFilter::default());
        assert_eq!(old, app.backup_ui.current_signature());
        // External state mutation during the actual producer/consumer task loses its token.
        app.backup_ui.sources[0].folder_filter.include_subfolders = true;
        await_plan(&mut app);
        assert!(app.backup_ui.plan.is_none());
        assert!(app.backup_ui.core_plans.is_none());
        start_backup_action(&mut app);
        assert!(app.task.is_none());
        assert_eq!(std::fs::read_dir(root.0.join("target")).unwrap().count(), 0);
    }

    #[test]
    fn folder_filter_gui_cancel_resume_history_reuses_saved_subset_after_ui_change() {
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
        let source = root.0.join("saved");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("A.ZIP"), b"zip").unwrap();
        std::fs::write(source.join("b.txt"), b"txt").unwrap();
        std::fs::write(source.join("nested/deep.zip"), b"deep").unwrap();
        filtered_folder(&mut app, source.clone(), "zip", false, false);
        generate_plan_action(&mut app);
        await_plan(&mut app);
        let cancel = AtomicBool::new(false);
        let staged = bftool_core::service::run_backup_plan(
            &app.backup_ui.core_plans.as_ref().unwrap()[0],
            &cancel,
            &CancelBeforePublish(&cancel),
        )
        .unwrap();
        assert_eq!(staged.outcome, BackupOutcome::Cancelled);
        assert!(!staged.published);
        assert_eq!(staged.copied, 1);
        set_folder_filter(
            &mut app,
            0,
            FolderFilter {
                include_subfolders: true,
                enabled: true,
                extensions_input: "txt".into(),
                ..Default::default()
            },
        );
        std::fs::write(source.join("b.txt"), b"changed excluded").unwrap();
        std::fs::write(source.join("nested/deep.zip"), b"changed excluded nested").unwrap();
        crate::views::tasks::refresh_history(&mut app);
        await_history(&mut app);
        let record = &app.tasks_ui.records[0];
        assert!(!record.completed);
        assert!(!record.directory_options.recursive);
        assert_eq!(
            record.directory_options.extensions,
            Some(vec!["zip".into()])
        );
        let job = record.job_id.clone();
        resume(&mut app, job);
        let outcome = finish_run(&mut app);
        assert_eq!(outcome.kind, BackupOutcomeKind::Success);
        assert_eq!(outcome.completed_items, 1);
        assert_eq!(
            std::fs::read(root.0.join("target/saved/A.ZIP")).unwrap(),
            b"zip"
        );
        assert!(!root.0.join("target/saved/b.txt").exists());
        assert!(!root.0.join("target/saved/nested").exists());
        let verify = bftool_core::service::verify_backup(
            &root.0.join("target"),
            &AtomicBool::new(false),
            &NoopReporter,
        )
        .unwrap();
        assert_eq!(verify.checked, 1);
        assert_eq!(verify.bad, 0);
        crate::views::tasks::refresh_history(&mut app);
        await_history(&mut app);
        assert!(app.tasks_ui.records[0].completed);
        assert_eq!(
            app.tasks_ui.records[0].directory_options.extensions,
            Some(vec!["zip".into()])
        );
    }

    #[test]
    fn folder_filter_gui_root_only_zero_matches_has_zero_preview_no_success_row() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let source = root.0.join("zero");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.txt"), b"txt").unwrap();
        filtered_folder(&mut app, source, "zip", false, false);
        generate_plan_action(&mut app);
        await_plan(&mut app);
        let preview = app.backup_ui.plan.as_ref().unwrap();
        assert_eq!(preview.pending_count, 0);
        assert_eq!(preview.pending_bytes, 0);
        assert!(preview.items.is_empty());
        assert!(app.backup_ui.has_no_matches());
        start_backup_action(&mut app);
        assert!(app.task.is_none());
        assert_eq!(std::fs::read_dir(root.0.join("target")).unwrap().count(), 0);
    }

    #[test]
    fn folder_filter_gui_extensionless_only_selection_and_unfiltered_empty_root() {
        let root = SyntheticDir::new();
        let mut app = app_for(&root);
        let source = root.0.join("plain");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("README"), b"plain").unwrap();
        std::fs::write(source.join("file.zip"), b"zip").unwrap();
        std::fs::write(source.join("nested/NESTED"), b"nested").unwrap();
        filtered_folder(&mut app, source, "", false, true);
        let empty = root.0.join("empty");
        std::fs::create_dir(&empty).unwrap();
        add_source_folder(&mut app, empty);
        generate_plan_action(&mut app);
        await_plan(&mut app);
        assert_eq!(app.backup_ui.plan.as_ref().unwrap().pending_count, 1);
        assert!(!app.backup_ui.has_no_matches());
        start_backup_action(&mut app);
        assert_eq!(finish_run(&mut app).kind, BackupOutcomeKind::Success);
        assert_eq!(
            std::fs::read(root.0.join("target/plain/README")).unwrap(),
            b"plain"
        );
        assert!(!root.0.join("target/plain/file.zip").exists());
        assert!(!root.0.join("target/plain/nested").exists());
        assert!(root.0.join("target/empty").is_dir());
    }

    fn rendered_recovery_labels(app: &mut App, backup_page: bool) -> Vec<String> {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        fonts.families.insert(
            egui::FontFamily::Name("semibold".into()),
            fonts.families[&egui::FontFamily::Proportional].clone(),
        );
        ctx.set_fonts(fonts);
        let painted = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 700.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if backup_page {
                        super::super::render_running_card(app, ui);
                    } else {
                        crate::views::tasks::ui(app, ui);
                    }
                });
            },
        );
        fn collect(shape: &egui::Shape, labels: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(value) => labels.push(value.galley.text().into()),
                egui::Shape::Vec(values) => {
                    for value in values {
                        collect(value, labels);
                    }
                }
                _ => {}
            }
        }
        let mut labels = Vec::new();
        for shape in &painted.shapes {
            collect(&shape.shape, &mut labels);
        }
        labels
    }

    fn resume_display_case(prior_batch: bool) {
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
        let mut original = app_for(&root);
        let saved = root.0.join("saved");
        std::fs::create_dir(&saved).unwrap();
        std::fs::write(saved.join("one.zip"), b"saved zip").unwrap();
        std::fs::write(saved.join("excluded.txt"), b"excluded").unwrap();
        filtered_folder(&mut original, saved.clone(), "zip", false, false);
        generate_plan_action(&mut original);
        await_plan(&mut original);
        let plan = &original.backup_ui.core_plans.as_ref().unwrap()[0];
        let job = plan.view().job_id.clone();
        let cancel = AtomicBool::new(false);
        let staged =
            bftool_core::service::run_backup_plan(plan, &cancel, &CancelBeforePublish(&cancel))
                .unwrap();
        assert!(!staged.published);
        assert_eq!(staged.outcome, BackupOutcome::Cancelled);
        drop(original);
        let mut app = crate::app::tests::fixture();
        app.backend = crate::backend::Backend::Live;
        set_target_folder(&mut app, root.0.join("target"));
        if prior_batch {
            let prior = root.0.join("prior");
            std::fs::create_dir(&prior).unwrap();
            for name in ["a", "b", "c"] {
                std::fs::write(prior.join(name), b"unrelated").unwrap();
            }
            add_source_folder(&mut app, prior);
            generate_plan_action(&mut app);
            await_plan(&mut app);
            start_backup_action(&mut app);
            assert_eq!(app.backup_ui.total_files, 3);
            assert_eq!(finish_run(&mut app).kind, BackupOutcomeKind::Success);
            app.backup_ui.current_file = "unrelated stale phase".into();
            app.backup_ui.done_files = 3;
            app.backup_ui.transferred_bytes = 27;
            app.backup_ui.speed_bps = Some(42);
            app.backup_ui.eta_secs = Some(9);
        }
        resume(&mut app, job);
        // GUI state is read before pumping reporter updates, regardless of worker speed.
        for backup_page in [true, false] {
            let labels = rendered_recovery_labels(&mut app, backup_page);
            assert!(
                labels.iter().any(|s| s.contains("恢复任务：整项总量未知")),
                "recovery card must not invent whole-job denominator: {labels:?}"
            );
            assert!(!labels.iter().any(|s| s.contains("0/0")
                || s.contains("0/3")
                || s.contains("unrelated stale phase")));
        }
        assert_eq!(app.backup_ui.total_files, 0);
        assert_eq!(app.backup_ui.total_bytes, 0);
        assert_eq!(app.backup_ui.done_files, 0);
        assert_eq!(app.backup_ui.transferred_bytes, 0);
        assert!(app.backup_ui.current_file.is_empty());
        assert!(app.backup_ui.speed_bps.is_none());
        assert!(app.backup_ui.eta_secs.is_none());
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !app.task.as_ref().unwrap().is_finished() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let (progress_tx, _progress_rx) = mpsc::channel();
        let actual_reporter =
            crate::reporter::GuiReporter::new(progress_tx, Arc::clone(&app.progress));
        let mut bytes =
            actual_reporter.progress_bytes("trusted current-file reporter", 100 * 1024 * 1024);
        bytes.inc(20 * 1024 * 1024);
        pump(&mut app, &egui::Context::default());
        assert_eq!(app.backup_ui.transferred_bytes, 20 * 1024 * 1024);
        assert_eq!(app.backup_ui.total_bytes, 100 * 1024 * 1024);
        for backup_page in [true, false] {
            let labels = rendered_recovery_labels(&mut app, backup_page);
            assert!(labels.iter().any(|s| s.contains("恢复任务：整项总量未知")
                && s.contains("当前步骤字节")
                && s.contains("20.0%")));
            assert!(labels
                .iter()
                .any(|s| s.contains("trusted current-file reporter")));
            assert!(!labels.iter().any(|s| s.contains("整批最终计数")));
        }
        bytes.finish();
        let outcome = finish_run(&mut app);
        assert_eq!(outcome.kind, BackupOutcomeKind::Success);
        assert_eq!(outcome.completed_items, 1);
        assert_eq!(std::fs::read(saved.join("one.zip")).unwrap(), b"saved zip");
        assert_eq!(
            std::fs::read(root.0.join("target/saved/one.zip")).unwrap(),
            b"saved zip"
        );
        assert!(!root.0.join("target/saved/excluded.txt").exists());
        // A subsequent normal start and demo keep their trusted known totals.
        if !app.backup_ui.sources.is_empty() {
            generate_plan_action(&mut app);
            await_plan(&mut app);
            start_backup_action(&mut app);
            for backup_page in [true, false] {
                let labels = rendered_recovery_labels(&mut app, backup_page);
                assert!(labels.iter().any(|s| s.contains("0/3")));
                assert!(!labels.iter().any(|s| s.contains("整项总量未知")));
            }
            assert_eq!(finish_run(&mut app).kind, BackupOutcomeKind::Success);
        }
        app.backend = crate::backend::Backend::Demo;
        apply_demo_state(&mut app, DemoState::Running);
        for backup_page in [true, false] {
            let labels = rendered_recovery_labels(&mut app, backup_page);
            assert!(labels.iter().any(|s| s.contains("48/120")));
            assert!(!labels.iter().any(|s| s.contains("整项总量未知")));
        }
    }

    #[test]
    fn folder_filter_resume_fresh_session_marks_totals_unknown_in_both_cards() {
        resume_display_case(false);
    }

    #[test]
    fn folder_filter_resume_prior_batch_clears_stale_totals_in_both_cards() {
        resume_display_case(true);
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
        app.backup_ui.sources[0].folder_filter.include_subfolders = true;
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

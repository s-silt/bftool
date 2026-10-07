use bftool_core::{
    pipeline::backup::{
        plan_backup, run_backup_plan, BackupOutcome, BackupRequest, SourceSelection,
    },
    reporter::{LogLevel, ProgressHandle, Reporter},
};
use bftool_desktop_bridge::*;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

struct PauseReporter(Arc<AtomicBool>);
struct PauseProgress {
    cancel: Arc<AtomicBool>,
    done: u64,
}
impl Reporter for PauseReporter {
    fn log(&self, _: LogLevel, _: &str) {}
    fn progress_bytes(&self, _: &str, _: u64) -> Box<dyn ProgressHandle> {
        Box::new(PauseProgress {
            cancel: self.0.clone(),
            done: 0,
        })
    }
}
impl ProgressHandle for PauseProgress {
    fn inc(&mut self, delta: u64) {
        self.done += delta;
        if self.done > 1024 * 1024 {
            self.cancel.store(true, Ordering::Release);
        }
    }
    fn finish(&mut self) {}
}
fn paused_backup(long: bool) -> (tempfile::TempDir, PathBuf, PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("paused-source");
    let target = if long {
        let mut target = dir.path().to_path_buf();
        while target
            .join("paused-source")
            .to_string_lossy()
            .encode_utf16()
            .count()
            < 320
        {
            target.push("long-target-segment".repeat(4));
        }
        target
    } else {
        dir.path().join("target")
    };
    fs::create_dir(&source).unwrap();
    fs::create_dir_all(&target).unwrap();
    for i in 0..4 {
        fs::write(source.join(format!("{i:03}.bin")), vec![b'x'; 1024 * 1024]).unwrap();
    }
    let mut io_target = target.clone();
    #[cfg(windows)]
    if long {
        io_target = PathBuf::from(format!(r"\\?\{}", target.display()));
    }
    let request = BackupRequest::new(SourceSelection::Directory(source.clone()), io_target);
    let cancel = Arc::new(AtomicBool::new(false));
    let reporter = PauseReporter(cancel.clone());
    let plan = plan_backup(&request, &cancel, &reporter).unwrap();
    let job = plan.view().job_id.clone();
    let summary = run_backup_plan(&plan, &cancel, &reporter).unwrap();
    assert_eq!(summary.outcome, BackupOutcome::Cancelled);
    assert!(!summary.published);
    assert!(summary.copied >= 1);
    (dir, source, target, job)
}
fn idle(engine: &Engine) -> Snapshot {
    for _ in 0..1000 {
        if !engine.is_busy() {
            return engine.snapshot().unwrap().unwrap();
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("synthetic recovery did not terminate");
}
fn token(engine: &Engine, target: &Path) -> String {
    let rows = engine.history(&target.to_string_lossy()).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].completed);
    rows[0]
        .recovery_id
        .clone()
        .expect("validated unfinished record token")
}
#[test]
fn restart_loads_real_journal_and_resumes_verified_receipts() {
    let (_dir, source, target, core_id) = paused_backup(false);
    let before = Engine::default();
    let old = token(&before, &target);
    drop(before);
    let engine = Engine::default();
    assert_eq!(engine.resume("0", &old).unwrap_err().code, "STALE_RECOVERY");
    assert_eq!(
        engine.resume("0", &core_id).unwrap_err().code,
        "STALE_RECOVERY"
    );
    let fresh = token(&engine, &target);
    let job = engine.resume("0", &fresh).unwrap();
    let result = idle(&engine);
    assert_eq!(result.job_id, job);
    assert_eq!(result.phase, "completed");
    assert!(result.results[0].published);
    assert!(result.results[0].skipped_verified.parse::<u64>().unwrap() >= 1);
    assert_eq!(result.results[0].verified, "4");
    for i in 0..4 {
        assert_eq!(
            fs::read(source.join(format!("{i:03}.bin"))).unwrap(),
            vec![b'x'; 1024 * 1024]
        );
    }
    let verified = engine.verify(&target.to_string_lossy()).unwrap();
    assert_eq!(verified.checked, "4");
    assert_eq!(verified.bad, "0");
    assert!(engine.history(&target.to_string_lossy()).unwrap()[0]
        .recovery_id
        .is_none());
    assert_eq!(
        engine.resume("0", &fresh).unwrap_err().code,
        "STALE_RECOVERY"
    );
}
#[test]
fn changed_inputs_and_changed_source_reject_recovery_without_publication() {
    let (_dir, source, target, _job) = paused_backup(false);
    let engine = Engine::default();
    let old = token(&engine, &target);
    engine.invalidate("1").unwrap();
    assert_eq!(engine.resume("1", &old).unwrap_err().code, "STALE_RECOVERY");
    let fresh = token(&engine, &target);
    fs::write(source.join("000.bin"), b"synthetic changed source").unwrap();
    engine.resume("1", &fresh).unwrap();
    let result = idle(&engine);
    assert_eq!(result.phase, "failed");
    assert_eq!(result.error.unwrap().code, "RECOVERY_FAILED");
    assert!(!target.join("paused-source").exists());
    assert_eq!(
        fs::read(source.join("000.bin")).unwrap(),
        b"synthetic changed source"
    );
}
#[test]
fn corrupt_metadata_and_unknown_stage_are_preserved_and_refused() {
    for metadata in [true, false] {
        let (_dir, source, target, job) = paused_backup(false);
        let engine = Engine::default();
        let fresh = token(&engine, &target);
        let corrupt = if metadata {
            target
                .join(".bftool-backup/jobs")
                .join(&job)
                .join("manifest.json")
        } else {
            target
                .join(".bftool-backup/jobs")
                .join(&job)
                .join("stage/unknown.bin")
        };
        fs::write(&corrupt, b"synthetic unknown evidence").unwrap();
        engine.resume("0", &fresh).unwrap();
        let result = idle(&engine);
        assert_eq!(result.phase, "failed");
        assert_eq!(result.error.unwrap().code, "RECOVERY_FAILED");
        assert_eq!(fs::read(&corrupt).unwrap(), b"synthetic unknown evidence");
        assert!(source.join("000.bin").exists());
        assert!(!target.join("paused-source").exists());
    }
}
#[test]
fn occupied_recovery_destination_is_never_overwritten() {
    let (_dir, source, target, _job) = paused_backup(false);
    let engine = Engine::default();
    let fresh = token(&engine, &target);
    fs::create_dir(target.join("paused-source")).unwrap();
    fs::write(
        target.join("paused-source/unknown.bin"),
        b"keep occupied target",
    )
    .unwrap();
    engine.resume("0", &fresh).unwrap();
    let result = idle(&engine);
    assert_eq!(result.phase, "failed");
    assert_eq!(
        fs::read(target.join("paused-source/unknown.bin")).unwrap(),
        b"keep occupied target"
    );
    assert!(source.join("000.bin").exists());
}
#[cfg(windows)]
#[test]
fn long_target_recovery_uses_core_validation_and_verbatim_publication() {
    let (_dir, _source, target, _job) = paused_backup(true);
    assert!(
        target
            .join("paused-source")
            .to_string_lossy()
            .encode_utf16()
            .count()
            >= 300
    );
    let engine = Engine::default();
    let fresh = token(&engine, &target);
    engine.resume("0", &fresh).unwrap();
    let result = idle(&engine);
    assert_eq!(result.phase, "completed");
    assert!(result.results[0].published);
    let verified = engine.verify(&target.to_string_lossy()).unwrap();
    assert_eq!(verified.checked, "4");
    assert_eq!(verified.bad, "0");
}

#[cfg(windows)]
#[test]
fn legacy_raw_260_publication_failure_resumes_without_rewriting_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    fs::create_dir(&target).unwrap();
    let target_len = target.to_string_lossy().encode_utf16().count();
    let leaf_len = 260 - target_len - 1;
    assert!((8..=255).contains(&leaf_len));
    let source = dir
        .path()
        .join(format!("legacy_{}", "s".repeat(leaf_len - 7)));
    fs::create_dir(&source).unwrap();
    fs::write(source.join("payload.bin"), b"legacy synthetic payload").unwrap();
    let cancel = AtomicBool::new(false);
    let reporter = bftool_core::reporter::NoopReporter;
    let request = BackupRequest::new(SourceSelection::Directory(source.clone()), target.clone());
    let plan = plan_backup(&request, &cancel, &reporter).unwrap();
    assert_eq!(
        target
            .join(&plan.view().destination_name)
            .to_string_lossy()
            .encode_utf16()
            .count(),
        260
    );
    let failed = run_backup_plan(&plan, &cancel, &reporter).unwrap_err();
    let partial = failed
        .downcast_ref::<bftool_core::pipeline::backup::BackupExecutionError>()
        .unwrap();
    assert!(!partial.summary.published);
    assert_eq!(partial.summary.copied, 1);
    assert_eq!(partial.summary.verified, 1);
    assert!(format!("{failed:#}").contains("206"));
    let manifest = target
        .join(".bftool-backup/jobs")
        .join(&plan.view().job_id)
        .join("manifest.json");
    let original_manifest = fs::read(&manifest).unwrap();
    drop(plan);
    let engine = Engine::default();
    let fresh = token(&engine, &target);
    engine.resume("0", &fresh).unwrap();
    let done = idle(&engine);
    assert_eq!(done.phase, "completed");
    assert!(done.results[0].published);
    assert_eq!(done.results[0].skipped_verified, "1");
    assert_eq!(fs::read(&manifest).unwrap(), original_manifest);
    assert_eq!(
        fs::read(source.join("payload.bin")).unwrap(),
        b"legacy synthetic payload"
    );
    assert_eq!(engine.verify(&target.to_string_lossy()).unwrap().bad, "0");
}

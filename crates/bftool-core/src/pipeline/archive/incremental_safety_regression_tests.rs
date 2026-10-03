//! Synthetic regressions for reviewed incremental safety defects. No real volumes or external programs.
use super::super::incremental::{
    classify, fingerprint_from_stats, resolve_incremental, source_id_for, IncrementalVerifyMode,
    JsonlIncrementalIndex,
};
use super::super::lock::{check_pending_txn, CommitJournal, CommitStage};
use super::super::plan::plan_on_drive;
use super::super::runner::run_plan;
use super::super::types::{Options, PlanAction};
use super::{handle_one, HandleOutcome};
use crate::config::Config;
use crate::engine::{drive::DriveInfo, paths};
use crate::reporter::{LogLevel, NoopReporter, ProgressHandle, Reporter};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

fn world() -> (tempfile::TempDir, Config, DriveInfo, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let cfg = Config {
        ready_root: d.path().join("ready"),
        archived_root: d.path().join("archived"),
        system_root: d.path().join("system"),
        stable_minutes: 0,
        reserve_gb: 0,
        test_archives: false,
        min_drive_gb: 0,
        ..Config::default()
    };
    let root = d.path().join("drive");
    for p in [&cfg.ready_root, &cfg.archived_root, &cfg.system_root, &root] {
        fs::create_dir_all(p).unwrap();
    }
    fs::create_dir_all(paths::drive_info_dir(&root)).unwrap();
    fs::write(paths::drive_id_path(&root), "备份1").unwrap();
    let drive = DriveInfo {
        root,
        letter: "T".into(),
        id: "备份1".into(),
        free_bytes: 1 << 40,
        total_bytes: 1 << 40,
        sealed: false,
    };
    let source = cfg.ready_root.join("001project");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.txt"), b"AAA").unwrap();
    // Match the canonical source identity passed from public plans into handle_one.
    let source = source.canonicalize().unwrap();
    (d, cfg, drive, source)
}

fn opts() -> Options {
    Options {
        incremental: true,
        retain_source: true,
        ..Options::default()
    }
}

fn archive(cfg: &Config, drive: &DriveInfo, source: &Path) {
    assert!(matches!(
        handle_one(cfg, &NoopReporter, drive, source, &opts(), None, None).unwrap(),
        HandleOutcome::Done(_)
    ));
}

fn planned_copy(
    cfg: &Config,
    drive: &DriveInfo,
    options: &Options,
) -> super::super::types::ArchivePlan {
    let plan = plan_on_drive(cfg, drive.clone(), options, &NoopReporter).unwrap();
    assert!(plan.items.iter().any(|item| matches!(
        item.action,
        PlanAction::Archive { .. } | PlanAction::RenameAndArchive { .. }
    )));
    plan
}

#[test]
fn s1_missing_old_hash_never_authenticates_same_size_update() {
    let (_d, _cfg, _drive, source) = world();
    let old = fingerprint_from_stats(&super::super::copy::folder_stats(&source));
    fs::write(source.join("a.txt"), b"BBB").unwrap();
    let current = fingerprint_from_stats(&super::super::copy::folder_stats(&source));
    for mode in [
        IncrementalVerifyMode::OnSuspect,
        IncrementalVerifyMode::Always,
        IncrementalVerifyMode::Never,
    ] {
        let result = resolve_incremental(
            classify(Some(&old), &current),
            Some(&old),
            &current,
            &source,
            mode,
        )
        .unwrap();
        assert!(!result.skip);
    }
}

#[test]
fn s1_same_size_content_update_is_archived_again() {
    let (_d, cfg, drive, source) = world();
    archive(&cfg, &drive, &source);
    fs::write(source.join("a.txt"), b"BBB").unwrap();
    let plan = planned_copy(&cfg, &drive, &opts());
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (1, 0));
    let newer = plan
        .items
        .iter()
        .find_map(|i| match &i.action {
            PlanAction::Archive { dest_name } | PlanAction::RenameAndArchive { dest_name } => {
                Some(dest_name)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        fs::read(
            paths::drive_projects_dir(&drive.root)
                .join(newer)
                .join("a.txt")
        )
        .unwrap(),
        b"BBB"
    );
    assert_eq!(
        fs::read(paths::drive_projects_dir(&drive.root).join("001project/a.txt")).unwrap(),
        b"AAA"
    );
}

#[test]
fn s2_catalog_seed_and_plan_are_read_only_and_require_a_backup() {
    let (_d, cfg, drive, _source) = world();
    fs::write(
        paths::system_global_catalog(&cfg.system_root),
        "文件夹名,备份盘名,备份时间,校验方式\n001project,备份1,old,SHA256-OK\n",
    )
    .unwrap();
    let mut index =
        JsonlIncrementalIndex::open_for_source(&cfg.system_root, &cfg.ready_root).unwrap();
    assert_eq!(
        index
            .seed_from_global_catalog(
                &cfg.system_root,
                &cfg.ready_root,
                &source_id_for(&cfg.ready_root)
            )
            .unwrap(),
        0
    );
    let options = Options {
        seed_from_global_catalog: true,
        ..opts()
    };
    planned_copy(&cfg, &drive, &options);
    assert!(!paths::system_incremental_dir(&cfg.system_root).exists());
}

#[test]
fn s2_missing_or_corrupted_backup_cannot_be_unchanged() {
    let (_d, cfg, drive, source) = world();
    archive(&cfg, &drive, &source);
    fs::write(
        paths::drive_projects_dir(&drive.root).join("001project/a.txt"),
        b"BAD",
    )
    .unwrap();
    planned_copy(&cfg, &drive, &opts());
    fs::remove_dir_all(paths::drive_projects_dir(&drive.root).join("001project")).unwrap();
    planned_copy(&cfg, &drive, &opts());
}

#[test]
fn s3_retain_recovery_preserves_partial_evidence_and_commits_only_old_snapshot() {
    let (_d, cfg, drive, source) = world();
    let catalog = paths::drive_catalog_path(&drive.root);
    fs::create_dir(&catalog).unwrap();
    assert!(matches!(
        handle_one(&cfg, &NoopReporter, &drive, &source, &opts(), None, None).unwrap(),
        HandleOutcome::CommitInterrupted
    ));
    let marker = paths::system_pending_txn(&cfg.system_root);
    let text = fs::read_to_string(&marker).unwrap();
    let journal: CommitJournal = toml::from_str(&text).unwrap();
    assert!(journal.retain_source);
    assert!(journal.pending.move_to.is_empty());
    assert_eq!(journal.stage, CommitStage::GlobalWritten);
    fs::write(source.join("a.txt"), b"NEW CONTENT").unwrap();
    assert!(check_pending_txn(&cfg, &NoopReporter, &drive.id).is_err());
    assert!(marker.is_file());
    assert!(catalog.is_dir());
    fs::remove_dir(&catalog).unwrap();
    check_pending_txn(&cfg, &NoopReporter, &drive.id).unwrap();
    assert!(!marker.exists());
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"NEW CONTENT");
    assert_eq!(
        fs::read(paths::drive_projects_dir(&drive.root).join("001project/a.txt")).unwrap(),
        b"AAA"
    );
    planned_copy(&cfg, &drive, &opts());
}

#[test]
fn s3_incremental_index_failure_keeps_transaction_until_recovery_succeeds() {
    let (_d, cfg, drive, source) = world();
    let index_path = paths::system_incremental_dir(&cfg.system_root)
        .join(format!("{}.jsonl", source_id_for(&cfg.ready_root)));
    fs::create_dir_all(&index_path).unwrap();
    assert!(matches!(
        handle_one(&cfg, &NoopReporter, &drive, &source, &opts(), None, None).unwrap(),
        HandleOutcome::CommitInterrupted
    ));
    let marker = paths::system_pending_txn(&cfg.system_root);
    assert!(marker.exists());
    assert!(check_pending_txn(&cfg, &NoopReporter, &drive.id).is_err());
    assert!(marker.exists());
    fs::remove_dir(&index_path).unwrap();
    check_pending_txn(&cfg, &NoopReporter, &drive.id).unwrap();
    assert!(!marker.exists());
    let plan = plan_on_drive(&cfg, drive.clone(), &opts(), &NoopReporter).unwrap();
    assert!(matches!(plan.items[0].action, PlanAction::Skip(_)));
}

struct ChangeAtFinalRead {
    source: PathBuf,
    armed: Arc<AtomicBool>,
}
struct ChangeProgress {
    source: PathBuf,
    armed: Arc<AtomicBool>,
}
impl ProgressHandle for ChangeProgress {
    fn inc(&mut self, _: u64) {}
    fn finish(&mut self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            fs::write(
                self.source.join("a.txt"),
                b"CHANGED AFTER FINAL SOURCE READ",
            )
            .unwrap();
        }
    }
}
impl Reporter for ChangeAtFinalRead {
    fn log(&self, _: LogLevel, msg: &str) {
        if msg == "复核源文件在复制期间未变化…" {
            self.armed.store(true, Ordering::SeqCst);
        }
    }
    fn progress_bytes(&self, _: &str, _: u64) -> Box<dyn ProgressHandle> {
        Box::new(ChangeProgress {
            source: self.source.clone(),
            armed: self.armed.clone(),
        })
    }
}

#[test]
fn s4_index_is_bound_to_verified_manifest_when_source_changes_after_final_read() {
    let (_d, cfg, drive, source) = world();
    let reporter = ChangeAtFinalRead {
        source: source.clone(),
        armed: Arc::new(AtomicBool::new(false)),
    };
    assert!(matches!(
        handle_one(&cfg, &reporter, &drive, &source, &opts(), None, None).unwrap(),
        HandleOutcome::Done(_)
    ));
    let index = JsonlIncrementalIndex::open_for_source(&cfg.system_root, &cfg.ready_root).unwrap();
    let baseline = index
        .verified_fingerprint(&source_id_for(&cfg.ready_root), "001project", &drive.root)
        .unwrap()
        .unwrap();
    assert_eq!(baseline.size, 3);
    assert_eq!(
        fs::read(paths::drive_projects_dir(&drive.root).join("001project/a.txt")).unwrap(),
        b"AAA"
    );
    assert_ne!(fs::read(source.join("a.txt")).unwrap(), b"AAA");
    planned_copy(&cfg, &drive, &opts());
}

#[test]
fn s7_rename_is_detected_with_identical_count_size_and_mtime() {
    let (_d, cfg, drive, source) = world();
    archive(&cfg, &drive, &source);
    let before = super::super::copy::folder_stats(&source);
    fs::rename(source.join("a.txt"), source.join("renamed.txt")).unwrap();
    let after = super::super::copy::folder_stats(&source);
    assert_eq!(
        (before.bytes, before.files, before.latest_mtime_secs),
        (after.bytes, after.files, after.latest_mtime_secs)
    );
    for mode in [
        IncrementalVerifyMode::Always,
        IncrementalVerifyMode::Never,
        IncrementalVerifyMode::OnSuspect,
    ] {
        planned_copy(
            &cfg,
            &drive,
            &Options {
                incremental_verify: mode,
                ..opts()
            },
        );
    }
}

struct CorruptAfterCopy {
    target: PathBuf,
}
impl Reporter for CorruptAfterCopy {
    fn log(&self, _: LogLevel, msg: &str) {
        if msg == "复制完成，开始校验…" {
            fs::write(&self.target, b"BAD").unwrap();
        }
    }
    fn progress_bytes(&self, _: &str, _: u64) -> Box<dyn ProgressHandle> {
        struct Noop;
        impl ProgressHandle for Noop {
            fn inc(&mut self, _: u64) {}
            fn finish(&mut self) {}
        }
        Box::new(Noop)
    }
}

#[test]
fn a04_real_hash_diff_failure_is_counted_not_skipped() {
    let (_d, cfg, drive, source) = world();
    let options = opts();
    let plan = plan_on_drive(&cfg, drive.clone(), &options, &NoopReporter).unwrap();
    let reporter = CorruptAfterCopy {
        target: paths::drive_projects_dir(&drive.root).join("001project/a.txt"),
    };
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &reporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (0, 1));
    assert!(source.exists());
    assert!(!paths::system_global_catalog(&cfg.system_root).exists());
    assert!(!paths::system_pending_txn(&cfg.system_root).exists());
    assert_eq!(
        fs::read(paths::drive_projects_dir(&drive.root).join("001project/a.txt")).unwrap(),
        b"BAD"
    );
    assert!(!paths::drive_quarantine_dir(&drive.root)
        .join("001project/a.txt")
        .exists());
}

#[test]
fn h7_item_boundary_rejects_mutated_options_before_writes() {
    let (_d, mut cfg, drive, source) = world();
    cfg.test_archives = true;
    let options = Options {
        no_hash: true,
        ..opts()
    };
    assert!(handle_one(&cfg, &NoopReporter, &drive, &source, &options, None, None).is_err());
    assert!(!paths::drive_projects_dir(&drive.root).exists());
}

#[test]
fn nonretain_source_changed_after_original_verification_is_kept() {
    let (_d, cfg, drive, source) = world();
    let reporter = ChangeAtFinalRead {
        source: source.clone(),
        armed: Arc::new(AtomicBool::new(false)),
    };
    let options = Options {
        retain_source: false,
        ..opts()
    };
    assert!(matches!(
        handle_one(&cfg, &reporter, &drive, &source, &options, None, None).unwrap(),
        HandleOutcome::CommitInterrupted
    ));
    assert!(source.exists());
    assert!(!cfg.archived_root.join("001project").exists());
    assert!(paths::system_pending_txn(&cfg.system_root).exists());
    assert_eq!(
        fs::read(paths::drive_projects_dir(&drive.root).join("001project/a.txt")).unwrap(),
        b"AAA"
    );
}

struct OccupyLate {
    archive_target: PathBuf,
    checks: Arc<std::sync::atomic::AtomicUsize>,
}
struct OccupyProgress {
    archive_target: PathBuf,
    checks: Arc<std::sync::atomic::AtomicUsize>,
}
impl ProgressHandle for OccupyProgress {
    fn inc(&mut self, _: u64) {}
    fn finish(&mut self) {
        // Source manifest, destination manifest, final source manifest, then pre-move source manifest.
        if self.checks.fetch_add(1, Ordering::SeqCst) == 3 {
            fs::create_dir(&self.archive_target).unwrap();
            fs::write(
                self.archive_target.join("unrelated.txt"),
                b"DO NOT OVERWRITE",
            )
            .unwrap();
        }
    }
}
impl Reporter for OccupyLate {
    fn log(&self, _: LogLevel, _: &str) {}
    fn progress_bytes(&self, label: &str, _: u64) -> Box<dyn ProgressHandle> {
        if label == "校验" {
            Box::new(OccupyProgress {
                archive_target: self.archive_target.clone(),
                checks: self.checks.clone(),
            })
        } else {
            struct Noop;
            impl ProgressHandle for Noop {
                fn inc(&mut self, _: u64) {}
                fn finish(&mut self) {}
            }
            Box::new(Noop)
        }
    }
}

#[test]
fn late_archive_destination_occupant_is_not_replaced() {
    let (_d, cfg, drive, source) = world();
    let target = cfg.archived_root.join("001project");
    let reporter = OccupyLate {
        archive_target: target.clone(),
        checks: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let options = Options {
        incremental: false,
        retain_source: false,
        ..Options::default()
    };
    assert!(matches!(
        handle_one(&cfg, &reporter, &drive, &source, &options, None, None).unwrap(),
        HandleOutcome::CommitInterrupted
    ));
    assert_eq!(
        fs::read(target.join("unrelated.txt")).unwrap(),
        b"DO NOT OVERWRITE"
    );
    assert!(source.join("a.txt").is_file());
    assert!(paths::system_pending_txn(&cfg.system_root).exists());
}

#[test]
fn s2_stale_catalog_candidate_is_copied_and_verified_to_a_new_name() {
    let (_d, cfg, drive, source) = world();
    super::super::catalog::append_global_catalog(
        &paths::system_global_catalog(&cfg.system_root),
        &super::super::catalog::GlobalCatalogRow {
            folder_name: "001project".into(),
            drive_name: drive.id.clone(),
            archived_time: "historical".into(),
            project_no: "001".into(),
            in_drive_path: "项目\\001project".into(),
            file_count: 99,
            size_gb: 10.0,
            verify: "SHA256-OK".into(),
            manifest_path: "old unavailable manifest".into(),
        },
    )
    .unwrap();
    let options = Options {
        seed_from_global_catalog: true,
        ..opts()
    };
    let plan = planned_copy(&cfg, &drive, &options);
    let name = match &plan.items[0].action {
        PlanAction::Archive { dest_name } | PlanAction::RenameAndArchive { dest_name } => dest_name,
        _ => unreachable!(),
    };
    assert_ne!(
        name, "001project",
        "historical metadata name must not be appropriated"
    );
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (1, 0));
    assert_eq!(
        fs::read(
            paths::drive_projects_dir(&drive.root)
                .join(name)
                .join("a.txt")
        )
        .unwrap(),
        b"AAA"
    );
    assert!(source.exists());
    let catalog = fs::read_to_string(paths::system_global_catalog(&cfg.system_root)).unwrap();
    assert!(catalog.contains("historical"));
    assert!(catalog.contains(name));
}

struct ReplaceReservedDirectory {
    destination: PathBuf,
    saved: PathBuf,
    unrelated: PathBuf,
    armed: AtomicBool,
}
impl Reporter for ReplaceReservedDirectory {
    fn log(&self, _: LogLevel, msg: &str) {
        if msg.starts_with("复制并生成源校验和（") && self.armed.swap(false, Ordering::SeqCst)
        {
            fs::rename(&self.destination, &self.saved).unwrap();
            fs::rename(&self.unrelated, &self.destination).unwrap();
        }
    }
    fn progress_bytes(&self, _: &str, _: u64) -> Box<dyn ProgressHandle> {
        struct Noop;
        impl ProgressHandle for Noop {
            fn inc(&mut self, _: u64) {}
            fn finish(&mut self) {}
        }
        Box::new(Noop)
    }
}

#[test]
fn replaced_destination_directory_preserves_unknown_files_in_place() {
    let (d, cfg, drive, source) = world();
    let plan = plan_on_drive(&cfg, drive.clone(), &opts(), &NoopReporter).unwrap();
    let unrelated = d.path().join("unrelated-user-directory");
    fs::create_dir(&unrelated).unwrap();
    fs::write(
        unrelated.join("user-document.txt"),
        b"KEEP AT SAME LOCATION",
    )
    .unwrap();
    let destination = paths::drive_projects_dir(&drive.root).join("001project");
    let reporter = ReplaceReservedDirectory {
        destination: destination.clone(),
        saved: d.path().join("original-reserved"),
        unrelated,
        armed: AtomicBool::new(true),
    };
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &reporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (0, 1));
    assert_eq!(
        fs::read(destination.join("user-document.txt")).unwrap(),
        b"KEEP AT SAME LOCATION"
    );
    assert!(!destination.join("a.txt").exists());
    assert!(fs::read_dir(&reporter.saved).unwrap().next().is_none());
    assert!(!paths::drive_quarantine_dir(&drive.root)
        .join("001project/user-document.txt")
        .exists());
    assert!(source.join("a.txt").is_file());
    assert!(!paths::system_global_catalog(&cfg.system_root).exists());
}

#[cfg(target_os = "linux")]
#[test]
fn metadata_symlink_creates_no_outside_directory_and_keeps_transaction() {
    let (d, cfg, drive, source) = world();
    let outside = d.path().join("outside-drive");
    fs::rename(paths::drive_info_dir(&drive.root), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, paths::drive_info_dir(&drive.root)).unwrap();
    let plan = plan_on_drive(&cfg, drive.clone(), &opts(), &NoopReporter).unwrap();
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (0, 1));
    assert!(!outside.join(paths::DRIVE_MANIFEST_DIR).exists());
    assert!(paths::system_pending_txn(&cfg.system_root).exists());
    assert!(source.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn linked_manual_log_is_not_appended_or_replaced() {
    let (d, cfg, _drive, _source) = world();
    let outside = d.path().join("outside-user-document.txt");
    fs::write(&outside, b"UNCHANGED USER DATA").unwrap();
    std::os::unix::fs::symlink(&outside, paths::system_need_manual(&cfg.system_root)).unwrap();
    assert!(super::super::lock::append_manual(&cfg, "001project", "failure").is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"UNCHANGED USER DATA");
    assert!(
        fs::symlink_metadata(paths::system_need_manual(&cfg.system_root))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linked_incremental_parent_creates_no_outside_index() {
    let (d, cfg, drive, source) = world();
    let outside = d.path().join("outside-system");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, paths::system_incremental_dir(&cfg.system_root)).unwrap();
    assert!(matches!(
        handle_one(&cfg, &NoopReporter, &drive, &source, &opts(), None, None).unwrap(),
        HandleOutcome::CommitInterrupted
    ));
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    assert!(paths::system_pending_txn(&cfg.system_root).exists());
}

struct LateManifestEvidence {
    path: PathBuf,
    armed: AtomicBool,
}
impl Reporter for LateManifestEvidence {
    fn log(&self, _: LogLevel, msg: &str) {
        if msg == "复核源文件在复制期间未变化…" && self.armed.swap(false, Ordering::SeqCst)
        {
            fs::create_dir_all(self.path.parent().unwrap()).unwrap();
            fs::write(&self.path, b"UNRELATED RECOVERY EVIDENCE").unwrap();
        }
    }
    fn progress_bytes(&self, _: &str, _: u64) -> Box<dyn ProgressHandle> {
        struct Noop;
        impl ProgressHandle for Noop {
            fn inc(&mut self, _: u64) {}
            fn finish(&mut self) {}
        }
        Box::new(Noop)
    }
}

#[test]
fn late_unrelated_manifest_evidence_is_not_overwritten() {
    let (_d, cfg, drive, source) = world();
    let path = paths::drive_manifest_dir(&drive.root).join("001project.sha256.csv");
    let reporter = LateManifestEvidence {
        path: path.clone(),
        armed: AtomicBool::new(true),
    };
    assert!(matches!(
        handle_one(&cfg, &reporter, &drive, &source, &opts(), None, None).unwrap(),
        HandleOutcome::CommitInterrupted
    ));
    assert_eq!(fs::read(&path).unwrap(), b"UNRELATED RECOVERY EVIDENCE");
    assert!(source.exists());
    assert!(paths::system_pending_txn(&cfg.system_root).exists());
    assert!(!paths::system_global_catalog(&cfg.system_root).exists());
}

#[test]
fn existing_orphan_manifest_reserves_name_without_overwriting_evidence() {
    let (_d, cfg, drive, source) = world();
    let manifest = paths::drive_manifest_dir(&drive.root).join("001project.sha256.csv");
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(&manifest, b"EXISTING RECOVERY EVIDENCE").unwrap();
    let plan = plan_on_drive(&cfg, drive.clone(), &opts(), &NoopReporter).unwrap();
    let dest = match &plan.items[0].action {
        PlanAction::RenameAndArchive { dest_name } => dest_name.clone(),
        action => panic!("must allocate a new name: {action:?}"),
    };
    assert_ne!(dest, "001project");
    let result = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!(result.handled, 1);
    assert_eq!(result.failed, 0);
    assert_eq!(fs::read(&manifest).unwrap(), b"EXISTING RECOVERY EVIDENCE");
    assert_eq!(
        fs::read(
            paths::drive_projects_dir(&drive.root)
                .join(dest)
                .join("a.txt")
        )
        .unwrap(),
        b"AAA"
    );
    assert!(source.exists());
}

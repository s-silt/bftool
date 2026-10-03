use bftool_core::pipeline::backup::{
    BackupOutcome, BackupRequest, ConflictPolicy, SourceSelection,
};
use bftool_core::reporter::NoopReporter;
use bftool_core::service;
use std::fs;
use std::sync::atomic::AtomicBool;

fn request(source: SourceSelection, target: &std::path::Path) -> BackupRequest {
    BackupRequest {
        source,
        target_dir: target.to_path_buf(),
        conflict: ConflictPolicy::KeepBoth,
    }
}

struct ReplaceJournal {
    journal: std::path::PathBuf,
    evidence: std::path::PathBuf,
    same_object: bool,
}

struct ReplaceOldCheckpoint(std::path::PathBuf);

#[test]
fn delta_fix_zero_byte_directory_metadata_stays_within_linear_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let target = tmp.path().join("target");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    for index in 0..32 {
        fs::write(source.join(format!("zero-{index:03}")), []).unwrap();
    }
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::Directory(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    fn tree_bytes(path: &std::path::Path) -> u64 {
        fs::read_dir(path)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    tree_bytes(&entry.path())
                } else {
                    entry.metadata().unwrap().len()
                }
            })
            .sum()
    }
    let bytes = tree_bytes(&target.join(".bftool-backup"));
    println!("delta32 zero_byte_files=32 owned_metadata_bytes={bytes}");
    assert!(bytes < 32 * 1536 + 4096, "32 empty files consumed {bytes} metadata bytes; expected linear storage including manifest and all records");
    let report = service::verify_backup(&target, &cancel, &NoopReporter).unwrap();
    assert_eq!((report.checked, report.bad), (32, 0));
    assert_eq!(fs::read_dir(source).unwrap().count(), 32);
}

struct CancelBeforeMetadata {
    cancel: std::sync::Arc<AtomicBool>,
    journal: std::path::PathBuf,
}
impl bftool_core::reporter::Reporter for CancelBeforeMetadata {
    fn log(&self, _: bftool_core::reporter::LogLevel, message: &str) {
        if message == "Verifying direct-backup history" {
            fs::write(&self.journal, b"UNKNOWN-CANCEL-KEEP").unwrap();
            self.cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn progress_bytes(
        &self,
        label: &str,
        total: u64,
    ) -> Box<dyn bftool_core::reporter::ProgressHandle> {
        bftool_core::reporter::Reporter::progress_bytes(&NoopReporter, label, total)
    }
}

#[test]
fn delta_fix_verify_cancels_before_reading_metadata_after_reporter_callback() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let target = tmp.path().join("target");
    fs::write(&source, []).unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let plan = service::plan_backup(
        &request(SourceSelection::File(source), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    let journal = target
        .join(".bftool-backup/jobs")
        .join(&plan.view().job_id)
        .join("journal.json");
    let reporter = CancelBeforeMetadata {
        cancel: cancel.clone(),
        journal: journal.clone(),
    };
    let result = service::verify_backup(&target, &cancel, &reporter);
    assert!(
        result
            .as_ref()
            .is_ok_and(|report| report.cancelled && report.checked == 0),
        "metadata cancellation must be a cancelled report, not parsing failure: {result:?}"
    );
    assert_eq!(fs::read(journal).unwrap(), b"UNKNOWN-CANCEL-KEEP");
}
impl bftool_core::reporter::Reporter for ReplaceOldCheckpoint {
    fn log(&self, _: bftool_core::reporter::LogLevel, message: &str) {
        if message == "Publishing direct backup" {
            fs::write(&self.0, b"OLD-CHECKPOINT-UNKNOWN").unwrap();
        }
    }
    fn progress_bytes(
        &self,
        label: &str,
        total: u64,
    ) -> Box<dyn bftool_core::reporter::ProgressHandle> {
        bftool_core::reporter::Reporter::progress_bytes(&NoopReporter, label, total)
    }
}

#[test]
fn delta_fix_old_checkpoint_change_at_publication_is_preserved_and_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let target = tmp.path().join("target");
    fs::write(&source, b"keep source").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    let first = target
        .join(".bftool-backup/jobs")
        .join(&plan.view().job_id)
        .join("checkpoint-00000001.json");
    let result = service::run_backup_plan(&plan, &cancel, &ReplaceOldCheckpoint(first.clone()));
    assert!(result.is_err());
    assert_eq!(fs::read(first).unwrap(), b"OLD-CHECKPOINT-UNKNOWN");
    assert!(!target.join("source").exists());
    assert_eq!(fs::read(source).unwrap(), b"keep source");
    assert!(service::list_backup_history(&target).is_err());
}

#[test]
fn final_fix_verification_wrong_kind_directory_is_not_missing() {
    use bftool_core::engine::verify::VerifyIssueKind;
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("folder");
    let target = tmp.path().join("target");
    fs::create_dir_all(source.join("empty")).unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::Directory(source), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    fs::remove_dir(target.join("folder/empty")).unwrap();
    fs::write(target.join("folder/empty"), b"UNKNOWN").unwrap();
    let report = service::verify_backup(&target, &cancel, &NoopReporter).unwrap();
    assert!(report
        .issues
        .iter()
        .any(|issue| issue.rel.ends_with("empty") && issue.kind == VerifyIssueKind::Unverifiable));
    assert!(!report
        .issues
        .iter()
        .any(|issue| issue.rel.ends_with("empty") && issue.kind == VerifyIssueKind::Missing));
}
impl bftool_core::reporter::Reporter for ReplaceJournal {
    fn log(&self, _: bftool_core::reporter::LogLevel, message: &str) {
        if message == "Copying direct backup" {
            if !self.same_object {
                fs::rename(&self.journal, &self.evidence).unwrap();
            }
            fs::write(&self.journal, b"UNKNOWN-KEEP").unwrap();
        }
    }
    fn progress_bytes(
        &self,
        label: &str,
        total: u64,
    ) -> Box<dyn bftool_core::reporter::ProgressHandle> {
        bftool_core::reporter::Reporter::progress_bytes(&NoopReporter, label, total)
    }
}

#[test]
fn final_fix_journal_callback_preserves_replacement_and_same_object_unknown() {
    for same_object in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let target = tmp.path().join("target");
        let evidence = tmp.path().join("evidence");
        fs::create_dir(&target).unwrap();
        fs::create_dir(&evidence).unwrap();
        fs::write(&source, b"source retained").unwrap();
        let cancel = AtomicBool::new(false);
        let plan = service::plan_backup(
            &request(SourceSelection::File(source.clone()), &target),
            &cancel,
            &NoopReporter,
        )
        .unwrap();
        let journal = target
            .join(".bftool-backup/jobs")
            .join(&plan.view().job_id)
            .join("journal.json");
        let reporter = ReplaceJournal {
            journal: journal.clone(),
            evidence: evidence.join("original.json"),
            same_object,
        };
        let result = service::run_backup_plan(&plan, &cancel, &reporter);
        assert_eq!(
            fs::read(&journal).unwrap(),
            b"UNKNOWN-KEEP",
            "unknown metadata must never be overwritten"
        );
        let error = result.expect_err("lost journal ownership must fail");
        let partial = error
            .downcast_ref::<bftool_core::pipeline::backup::BackupExecutionError>()
            .unwrap();
        assert!(!partial.summary.published);
        assert!(!target.join("source").exists());
        assert_eq!(fs::read(&source).unwrap(), b"source retained");
        assert!(service::list_backup_history(&target).is_err());
    }
}

#[test]
fn copies_one_file_to_uninitialized_directory_and_keeps_source() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("photo.txt");
    let target = tmp.path().join("ordinary-hdd");
    fs::write(&source, b"abc").unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(target.join("unrelated.txt"), b"keep me").unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    assert!(
        !target.join(".bftool-backup").exists(),
        "preview must be read only"
    );
    assert_eq!(plan.view().bytes, 3);
    let summary = service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    assert_eq!(summary.outcome, BackupOutcome::Completed);
    assert!(summary.published);
    assert_eq!((summary.copied, summary.verified), (1, 1));
    assert_eq!(fs::read(&source).unwrap(), b"abc");
    assert_eq!(fs::read(target.join("photo.txt")).unwrap(), b"abc");
    assert_eq!(fs::read(target.join("unrelated.txt")).unwrap(), b"keep me");
    let report = service::verify_backup(&target, &cancel, &NoopReporter).unwrap();
    assert_eq!((report.checked, report.bad, report.extra), (1, 0, 0));
    assert_eq!(service::list_backup_history(&target).unwrap().len(), 1);
}

#[test]
fn folder_preserves_empty_dirs_and_explicit_hidden_and_part_files() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("project");
    let target = tmp.path().join("hdd");
    fs::create_dir_all(source.join("empty/nested")).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join(".secret"), b"hidden").unwrap();
    fs::write(source.join("important.part"), b"explicit").unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::Directory(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    assert!(target.join("project/empty/nested").is_dir());
    assert_eq!(fs::read(target.join("project/.secret")).unwrap(), b"hidden");
    assert_eq!(
        fs::read(target.join("project/important.part")).unwrap(),
        b"explicit"
    );
    assert!(source.join("empty/nested").is_dir());
}

#[test]
fn same_name_keeps_both_and_late_conflict_never_overwrites() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("a.txt");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"new").unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(target.join("a.txt"), b"old").unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    let destination = target.join(&plan.view().destination_name);
    assert_ne!(destination, target.join("a.txt"));
    fs::write(&destination, b"late unknown").unwrap();
    assert!(service::run_backup_plan(&plan, &cancel, &NoopReporter).is_err());
    assert_eq!(fs::read(destination).unwrap(), b"late unknown");
    assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"old");
    assert_eq!(fs::read(source).unwrap(), b"new");
}

#[test]
fn source_mutation_after_preview_fails_before_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("a.txt");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"old").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    fs::write(&source, b"new").unwrap();
    assert!(service::run_backup_plan(&plan, &cancel, &NoopReporter).is_err());
    assert!(!target.join("a.txt").exists());
    assert_eq!(fs::read(source).unwrap(), b"new");
}

#[test]
fn poisoned_metadata_namespace_is_preserved_and_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"source").unwrap();
    fs::create_dir_all(target.join(".bftool-backup")).unwrap();
    fs::write(target.join(".bftool-backup/user-data"), b"UNKNOWN").unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    assert!(service::run_backup_plan(&plan, &cancel, &NoopReporter).is_err());
    assert_eq!(
        fs::read(target.join(".bftool-backup/user-data")).unwrap(),
        b"UNKNOWN"
    );
    assert_eq!(fs::read(source).unwrap(), b"source");
    assert!(!target.join("source").exists());
}

#[test]
fn newly_added_source_entry_invalidates_directory_preview() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("folder");
    let target = tmp.path().join("hdd");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"a").unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::Directory(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    fs::write(source.join("late"), b"new").unwrap();
    assert!(service::run_backup_plan(&plan, &cancel, &NoopReporter).is_err());
    assert!(!target.join("folder").exists());
    assert!(!target.join(".bftool-backup").exists());
}

#[test]
fn verification_reports_corruption_missing_empty_directory_and_extra_payload() {
    use bftool_core::engine::verify::VerifyIssueKind;
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("folder");
    let target = tmp.path().join("hdd");
    fs::create_dir_all(source.join("empty")).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"abc").unwrap();
    fs::write(source.join("b"), b"def").unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::Directory(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    fs::write(target.join("folder/a"), b"XYZ").unwrap();
    fs::remove_file(target.join("folder/b")).unwrap();
    fs::remove_dir(target.join("folder/empty")).unwrap();
    fs::write(target.join("folder/extra"), b"extra").unwrap();
    fs::write(target.join("unrelated"), b"not a payload extra").unwrap();
    let report = service::verify_backup(&target, &cancel, &NoopReporter).unwrap();
    assert_eq!((report.checked, report.bad, report.extra), (2, 3, 1));
    assert!(report
        .issues
        .iter()
        .any(|i| i.kind == VerifyIssueKind::Corrupt));
    assert_eq!(
        report
            .issues
            .iter()
            .filter(|i| i.kind == VerifyIssueKind::Missing)
            .count(),
        2
    );
    assert_eq!(fs::read(source.join("a")).unwrap(), b"abc");
}

struct CancelOnSecondChunk(std::sync::Arc<AtomicBool>);
struct ChunkProgress {
    cancel: std::sync::Arc<AtomicBool>,
    calls: usize,
}
impl bftool_core::reporter::ProgressHandle for ChunkProgress {
    fn inc(&mut self, _delta: u64) {
        self.calls += 1;
        if self.calls == 2 {
            self.cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn finish(&mut self) {}
}
impl bftool_core::reporter::Reporter for CancelOnSecondChunk {
    fn log(&self, _level: bftool_core::reporter::LogLevel, _msg: &str) {}
    fn progress_bytes(
        &self,
        _label: &str,
        _total: u64,
    ) -> Box<dyn bftool_core::reporter::ProgressHandle> {
        Box::new(ChunkProgress {
            cancel: self.0.clone(),
            calls: 0,
        })
    }
}

#[test]
fn cancelled_job_resumes_only_hash_proven_completed_files() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("folder");
    let target = tmp.path().join("hdd");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"abc").unwrap();
    let large = vec![b'x'; 2 * 1024 * 1024 + 3];
    fs::write(source.join("b"), &large).unwrap();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let plan = service::plan_backup(
        &request(SourceSelection::Directory(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    let summary =
        service::run_backup_plan(&plan, &cancel, &CancelOnSecondChunk(cancel.clone())).unwrap();
    assert_eq!(summary.outcome, BackupOutcome::Cancelled);
    assert!(!summary.published);
    assert_eq!(summary.copied, 1);
    assert!(!target.join("folder").exists());
    assert_eq!(fs::read(source.join("b")).unwrap(), large);
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let summary =
        service::resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter).unwrap();
    assert_eq!(summary.outcome, BackupOutcome::Completed);
    assert!(summary.published);
    assert_eq!(
        (summary.copied, summary.skipped_verified, summary.verified),
        (1, 1, 2)
    );
    assert_eq!(fs::read(target.join("folder/a")).unwrap(), b"abc");
    assert_eq!(fs::read(target.join("folder/b")).unwrap(), large);
}

#[test]
fn malformed_journal_path_cannot_escape_or_overwrite_unknown_files() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("a");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"abc").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    let journal_path = target
        .join(".bftool-backup/jobs")
        .join(&plan.view().job_id)
        .join("checkpoint-00000001.json");
    let mut journal: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal_path).unwrap()).unwrap();
    assert_eq!(journal["change"]["kind"], "File");
    journal["change"]["path"] = serde_json::json!("../unknown");
    fs::write(&journal_path, serde_json::to_vec(&journal).unwrap()).unwrap();
    fs::write(target.join("unknown"), b"KEEP").unwrap();
    assert!(service::resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter).is_err());
    assert!(service::verify_backup(&target, &cancel, &NoopReporter).is_err());
    assert_eq!(fs::read(target.join("unknown")).unwrap(), b"KEEP");
    assert_eq!(fs::read(source).unwrap(), b"abc");
}

struct EditBeforePublishing(std::path::PathBuf);
impl bftool_core::reporter::Reporter for EditBeforePublishing {
    fn log(&self, _level: bftool_core::reporter::LogLevel, msg: &str) {
        if msg == "Publishing direct backup" {
            fs::write(&self.0, b"changed at publishing callback").unwrap();
        }
    }
    fn progress_bytes(
        &self,
        label: &str,
        total: u64,
    ) -> Box<dyn bftool_core::reporter::ProgressHandle> {
        bftool_core::reporter::Reporter::progress_bytes(&NoopReporter, label, total)
    }
}

#[test]
fn source_change_at_publish_callback_prevents_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("a");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"abc").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    assert!(
        service::run_backup_plan(&plan, &cancel, &EditBeforePublishing(source.clone())).is_err()
    );
    assert!(
        !target.join("a").exists(),
        "Source mutation must stop before publication"
    );
    assert_eq!(fs::read(source).unwrap(), b"changed at publishing callback");
}

#[test]
fn duplicate_completed_journal_keys_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("a");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"abc").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    let path = target
        .join(".bftool-backup/jobs")
        .join(&plan.view().job_id)
        .join("checkpoint-00000001.json");
    let journal: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(journal["change"]["kind"], "File");
    let encoded = serde_json::to_string(&journal).unwrap();
    // The completed receipt is now a single delta: duplicated path fields must
    // be rejected before serde can choose a last value.
    let needle = "\"path\":\"\"";
    assert!(encoded.contains(needle));
    let replacement = "\"path\":\"\",\"path\":\"\"";
    fs::write(&path, encoded.replacen(needle, replacement, 1)).unwrap();
    assert!(service::resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter).is_err());
    assert!(service::verify_backup(&target, &cancel, &NoopReporter).is_err());
    assert_eq!(fs::read(target.join("a")).unwrap(), b"abc");
}

struct ChangeSecondSource(std::path::PathBuf);

#[test]
fn delta_fix_duplicate_file_receipt_is_rejected_with_valid_previous_hash() {
    use sha2::{Digest, Sha256};
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let target = tmp.path().join("target");
    fs::write(&source, b"source").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    let job = target.join(".bftool-backup/jobs").join(&plan.view().job_id);
    let first = fs::read(job.join("checkpoint-00000001.json")).unwrap();
    let mut duplicate: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(duplicate["change"]["kind"], "File");
    duplicate["previous_sha256"] = serde_json::json!(format!("{:X}", Sha256::digest(&first)));
    fs::write(
        job.join("checkpoint-00000002.json"),
        serde_json::to_vec(&duplicate).unwrap(),
    )
    .unwrap();
    let error = service::list_backup_history(&target).unwrap_err();
    assert!(
        format!("{error:#}").contains("duplicate completed-file"),
        "must reject semantic duplicate before later chain mismatch: {error:#}"
    );
    assert!(service::resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter).is_err());
    assert_eq!(fs::read(source).unwrap(), b"source");
    assert_eq!(fs::read(target.join("source")).unwrap(), b"source");
}
struct ChangeSourceProgress {
    path: std::path::PathBuf,
    changed: bool,
}
impl bftool_core::reporter::ProgressHandle for ChangeSourceProgress {
    fn inc(&mut self, _delta: u64) {
        if !self.changed {
            fs::write(&self.path, b"XYZ").unwrap();
            self.changed = true;
        }
    }
    fn finish(&mut self) {}
}
impl bftool_core::reporter::Reporter for ChangeSecondSource {
    fn log(&self, _level: bftool_core::reporter::LogLevel, _msg: &str) {}
    fn progress_bytes(
        &self,
        _label: &str,
        _total: u64,
    ) -> Box<dyn bftool_core::reporter::ProgressHandle> {
        Box::new(ChangeSourceProgress {
            path: self.0.clone(),
            changed: false,
        })
    }
}

#[test]
fn execution_error_preserves_verified_partial_counts_without_claiming_publication() {
    use bftool_core::pipeline::backup::BackupExecutionError;
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("folder");
    let target = tmp.path().join("hdd");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("a"), b"abc").unwrap();
    fs::write(source.join("b"), b"def").unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::Directory(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    let error = service::run_backup_plan(&plan, &cancel, &ChangeSecondSource(source.join("b")))
        .unwrap_err();
    let partial = error
        .downcast_ref::<BackupExecutionError>()
        .expect("Execution failures carry their verified partial results");
    assert_eq!(partial.summary.outcome, BackupOutcome::Failed);
    assert_eq!(
        (
            partial.summary.copied,
            partial.summary.verified,
            partial.summary.skipped_verified,
            partial.summary.failed
        ),
        (1, 1, 0, 1)
    );
    assert_eq!(partial.summary.bytes, 3);
    assert!(!partial.summary.published);
    assert!(partial
        .summary
        .issues
        .iter()
        .any(|i| i.contains("Source changed before copy")));
    assert!(std::error::Error::source(partial).is_some());
    assert!(!target.join("folder").exists());
    assert_eq!(fs::read(source.join("a")).unwrap(), b"abc");
    assert_eq!(fs::read(source.join("b")).unwrap(), b"XYZ");
    assert_eq!(
        fs::read(
            target
                .join(".bftool-backup/jobs")
                .join(&plan.view().job_id)
                .join("stage/payload/a")
        )
        .unwrap(),
        b"abc"
    );
}

#[test]
fn unknown_job_directory_blocks_new_execution_and_resume_without_adoption() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("a");
    let second = tmp.path().join("b");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"abc").unwrap();
    fs::write(&second, b"def").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    let unknown = target.join(".bftool-backup/jobs/job-user-data");
    fs::create_dir(&unknown).unwrap();
    fs::write(unknown.join("KEEP"), b"UNKNOWN").unwrap();
    let second_plan = service::plan_backup(
        &request(SourceSelection::File(second.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    let new_result = service::run_backup_plan(&second_plan, &cancel, &NoopReporter);
    let resumed = service::resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter);
    assert!(new_result.is_err(), "Unknown jobs must block new execution");
    assert!(resumed.is_err(), "Unknown jobs must block recovery");
    assert!(!target.join("b").exists());
    assert_eq!(fs::read(unknown.join("KEEP")).unwrap(), b"UNKNOWN");
    assert_eq!(fs::read(source).unwrap(), b"abc");
    assert_eq!(fs::read(second).unwrap(), b"def");
}

#[test]
fn completed_job_with_unknown_staging_is_neither_resumed_nor_verified_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("a");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"abc").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    let unknown = target
        .join(".bftool-backup/jobs")
        .join(&plan.view().job_id)
        .join("stage/UNKNOWN");
    fs::write(&unknown, b"KEEP").unwrap();
    let resumed = service::resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter);
    let verified = service::verify_backup(&target, &cancel, &NoopReporter);
    assert!(
        resumed.is_err(),
        "Unknown staging must block completed recovery"
    );
    if let Ok(report) = verified {
        assert!(
            report.bad > 0 || report.extra > 0,
            "Unknown staging cannot produce Clean"
        );
    }
    assert_eq!(fs::read(unknown).unwrap(), b"KEEP");
    assert_eq!(fs::read(source).unwrap(), b"abc");
}

struct OverwriteStagingBeforePublish(std::path::PathBuf);
impl bftool_core::reporter::Reporter for OverwriteStagingBeforePublish {
    fn log(&self, _level: bftool_core::reporter::LogLevel, msg: &str) {
        if msg == "Publishing direct backup" {
            fs::write(&self.0, b"XYZ").unwrap();
        }
    }
    fn progress_bytes(
        &self,
        label: &str,
        total: u64,
    ) -> Box<dyn bftool_core::reporter::ProgressHandle> {
        bftool_core::reporter::Reporter::progress_bytes(&NoopReporter, label, total)
    }
}

#[test]
fn staged_same_size_mutation_at_publish_callback_prevents_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("a");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"abc").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    let payload = target
        .join(".bftool-backup/jobs")
        .join(&plan.view().job_id)
        .join("stage/payload");
    let result = service::run_backup_plan(
        &plan,
        &cancel,
        &OverwriteStagingBeforePublish(payload.clone()),
    );
    assert!(result.is_err());
    assert!(
        !target.join("a").exists(),
        "Tampered staging must never be published"
    );
    assert_eq!(fs::read(source).unwrap(), b"abc");
    assert_eq!(fs::read(payload).unwrap(), b"XYZ");
}

struct CancelRecoveryPlanning(std::sync::Arc<AtomicBool>);
impl bftool_core::reporter::Reporter for CancelRecoveryPlanning {
    fn log(&self, _level: bftool_core::reporter::LogLevel, msg: &str) {
        if msg == "Planning direct backup" {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn progress_bytes(
        &self,
        label: &str,
        total: u64,
    ) -> Box<dyn bftool_core::reporter::ProgressHandle> {
        bftool_core::reporter::Reporter::progress_bytes(&NoopReporter, label, total)
    }
}

#[test]
fn cancellation_during_resume_preflight_returns_structured_cancelled() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("a");
    let target = tmp.path().join("hdd");
    fs::write(&source, b"abc").unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let plan = service::plan_backup(
        &request(SourceSelection::File(source.clone()), &target),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    service::run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    let result = service::resume_backup(
        &target,
        &plan.view().job_id,
        &cancel,
        &CancelRecoveryPlanning(cancel.clone()),
    )
    .expect("Preflight cancellation is a structured outcome");
    assert_eq!(result.outcome, BackupOutcome::Cancelled);
    assert!(!result.published);
    assert_eq!((result.copied, result.verified, result.failed), (0, 0, 0));
    assert_eq!(fs::read(source).unwrap(), b"abc");
}

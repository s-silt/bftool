use bftool_core::pipeline::backup::*;
use bftool_core::reporter::NoopReporter;
use std::{fs, sync::atomic::AtomicBool};

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let world = tempfile::tempdir().unwrap();
    let source = world.path().join("source");
    let target = world.path().join("target");
    fs::create_dir_all(source.join("child/deeper")).unwrap();
    fs::create_dir_all(source.join("unrelated/empty")).unwrap();
    fs::create_dir(&target).unwrap();
    for (name, bytes) in [
        ("a.tar", "tar"),
        ("b.ZIP", "zip"),
        ("skip.txt", "skip"),
        ("archive.tar.gz", "gzip"),
        ("plain", "none"),
        (".hidden", "hidden"),
        ("child/c.zip", "nested"),
        ("child/deeper/d.tar", "deep"),
    ] {
        fs::write(source.join(name), bytes).unwrap();
    }
    (world, source, target)
}
fn filtered(source: &std::path::Path, target: &std::path::Path, recursive: bool) -> BackupRequest {
    let mut request = BackupRequest::new(
        SourceSelection::Directory(source.to_path_buf()),
        target.to_path_buf(),
    );
    request.directory_options = Some(DirectoryOptions {
        recursive,
        extensions: Some(vec![".ZIP".into(), "tar".into(), "zip".into()]),
        include_extensionless: false,
    });
    request
}
fn manifest_path(target: &std::path::Path, id: &str) -> std::path::PathBuf {
    target
        .join(".bftool-backup/jobs")
        .join(id)
        .join("manifest.json")
}

#[test]
fn folder_filter_parser_and_final_suffix_contract() {
    assert_eq!(
        parse_extensions(".ZIP, tar; ZIP gz").unwrap(),
        ["gz", "tar", "zip"]
    );
    assert!(parse_extensions("  ").unwrap().is_empty());
    for bad in [
        ".", "tar.gz", "*.zip", "a/b", "a\\b", "C:zip", "zip,,tar", "zip;", "a@b", "a\u{0}b", "İ",
    ] {
        assert!(
            parse_extensions(bad).is_err(),
            "accepted malformed suffix {bad:?}"
        );
    }
    assert!(parse_extensions(&"a".repeat(65)).is_err());
    assert_eq!(parse_extensions(&"Я".repeat(64)).unwrap(), ["я".repeat(64)]);
    assert!(parse_extensions(&"界".repeat(43)).is_err());
    assert!(parse_extensions(
        &(0..129)
            .map(|i| format!("x{i}"))
            .collect::<Vec<_>>()
            .join(",")
    )
    .is_err());
    let options = DirectoryOptions {
        recursive: false,
        extensions: Some(vec!["tar".into(), "zip".into()]),
        include_extensionless: false,
    };
    for name in ["a.tar", "b.ZIP", "a.part.tar"] {
        assert!(options.matches_file(std::path::Path::new(name)));
    }
    for name in ["a.tar.gz", "plain", ".hidden"] {
        assert!(!options.matches_file(std::path::Path::new(name)));
    }
    assert!(DirectoryOptions {
        include_extensionless: true,
        ..options
    }
    .matches_file(std::path::Path::new(".hidden")));
    assert!(serde_json::from_str::<DirectoryOptions>(
        r#"{"recursive":false,"extensions":null,"include_extensionless":true,"unknown":0}"#
    )
    .is_err());
}

#[test]
fn folder_filter_parser_bounds_total_input_before_tokenizing() {
    assert!(parse_extensions(&"zip ".repeat(10_000)).is_err());
}

#[test]
fn folder_filter_shallow_copy_verifies_history_and_ignores_excluded_changes() {
    let (_world, source, target) = fixture();
    let cancel = AtomicBool::new(false);
    let plan = plan_backup(&filtered(&source, &target, false), &cancel, &NoopReporter).unwrap();
    assert_eq!(
        (
            plan.view().counts.files,
            plan.view().counts.directories,
            plan.view().bytes
        ),
        (2, 1, 6)
    );
    assert_eq!(
        plan.request()
            .directory_options
            .as_ref()
            .unwrap()
            .extensions
            .as_ref()
            .unwrap(),
        &["tar", "zip"]
    );
    fs::write(source.join("child/c.zip"), "modified child content").unwrap();
    fs::write(source.join("child/new.tar"), "added child").unwrap();
    fs::write(source.join("skip.txt"), "modified excluded").unwrap();
    let summary = run_backup_plan(&plan, &cancel, &NoopReporter).unwrap();
    assert!(summary.published);
    assert_eq!((summary.copied, summary.verified), (2, 2));
    let destination = target.join(&plan.view().destination_name);
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 2);
    assert_eq!(fs::read(destination.join("a.tar")).unwrap(), b"tar");
    assert_eq!(fs::read(destination.join("b.ZIP")).unwrap(), b"zip");
    let report = verify_backup(&target, &cancel, &NoopReporter).unwrap();
    assert_eq!((report.checked, report.bad, report.extra), (2, 0, 0));
    let history = list_backup_history(&target).unwrap();
    assert!(history[0].completed);
    assert_eq!(
        history[0].directory_options,
        plan.request().effective_directory_options()
    );
    assert_eq!(
        fs::read(source.join("child/c.zip")).unwrap(),
        b"modified child content"
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(manifest_path(&target, &plan.view().job_id)).unwrap())
            .unwrap();
    assert_eq!(manifest["version"], 2);
    assert!(!manifest["request"]["directory_options"].is_null());
}

#[test]
fn folder_filter_recursive_opt_in_preserves_needed_structure() {
    let (_world, source, target) = fixture();
    let cancel = AtomicBool::new(false);
    let plan = plan_backup(&filtered(&source, &target, true), &cancel, &NoopReporter).unwrap();
    assert_eq!(
        (plan.view().counts.files, plan.view().counts.directories),
        (4, 3)
    );
    assert!(!plan
        .view()
        .entries
        .iter()
        .any(|e| e.relative_path.starts_with("unrelated")));
    assert!(
        run_backup_plan(&plan, &cancel, &NoopReporter)
            .unwrap()
            .published
    );
    let destination = target.join(&plan.view().destination_name);
    assert_eq!(
        fs::read(destination.join("child/deeper/d.tar")).unwrap(),
        b"deep"
    );
    assert!(!destination.join("unrelated").exists());
    let report = verify_backup(&target, &cancel, &NoopReporter).unwrap();
    assert_eq!((report.checked, report.bad, report.extra), (4, 0, 0));
}

#[test]
fn folder_filter_zero_matches_rejects_before_any_target_writes() {
    let (_world, source, target) = fixture();
    let cancel = AtomicBool::new(false);
    let mut request = filtered(&source, &target, false);
    request.directory_options.as_mut().unwrap().extensions = Some(vec!["absent".into()]);
    let plan = plan_backup(&request, &cancel, &NoopReporter).unwrap();
    assert_eq!((plan.view().counts.files, plan.view().bytes), (0, 0));
    assert!(!plan.view().issues.is_empty());
    assert!(run_backup_plan(&plan, &cancel, &NoopReporter)
        .unwrap_err()
        .to_string()
        .contains("No files match"));
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    assert_eq!(fs::read(source.join("a.tar")).unwrap(), b"tar");
}

#[test]
fn folder_filter_explicit_file_ignores_even_invalid_folder_filters() {
    let (_world, source, target) = fixture();
    let mut request = filtered(&source, &target, true);
    request.source = SourceSelection::File(source.join("skip.txt"));
    request.directory_options.as_mut().unwrap().extensions = Some(vec!["*.bad".into()]);
    let cancel = AtomicBool::new(false);
    let plan = plan_backup(&request, &cancel, &NoopReporter).unwrap();
    assert_eq!(
        plan.request().directory_options,
        Some(DirectoryOptions::default())
    );
    assert!(
        run_backup_plan(&plan, &cancel, &NoopReporter)
            .unwrap()
            .published
    );
    assert_eq!(fs::read(target.join("skip.txt")).unwrap(), b"skip");
    assert_eq!(
        verify_backup(&target, &cancel, &NoopReporter)
            .unwrap()
            .checked,
        1
    );
}

#[test]
fn folder_filter_selected_change_and_added_matching_file_invalidate_plan() {
    for added in [false, true] {
        let (_world, source, target) = fixture();
        let cancel = AtomicBool::new(false);
        let plan = plan_backup(&filtered(&source, &target, false), &cancel, &NoopReporter).unwrap();
        fs::write(
            source.join(if added { "new.zip" } else { "a.tar" }),
            "changed",
        )
        .unwrap();
        assert!(run_backup_plan(&plan, &cancel, &NoopReporter).is_err());
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    }
}

#[test]
fn folder_filter_unknown_destination_is_preserved() {
    let (_world, source, target) = fixture();
    let cancel = AtomicBool::new(false);
    let plan = plan_backup(&filtered(&source, &target, false), &cancel, &NoopReporter).unwrap();
    fs::write(target.join(&plan.view().destination_name), b"UNKNOWN-KEEP").unwrap();
    assert!(run_backup_plan(&plan, &cancel, &NoopReporter).is_err());
    assert_eq!(
        fs::read(target.join(&plan.view().destination_name)).unwrap(),
        b"UNKNOWN-KEEP"
    );
    assert!(!target.join(".bftool-backup").exists());
}

#[test]
fn folder_filter_extensionless_and_last_suffix_copy_exact_files() {
    let (_world, source, target) = fixture();
    let cancel = AtomicBool::new(false);
    let mut request = filtered(&source, &target, false);
    request.directory_options.as_mut().unwrap().extensions = Some(vec!["gz".into()]);
    request
        .directory_options
        .as_mut()
        .unwrap()
        .include_extensionless = true;
    let plan = plan_backup(&request, &cancel, &NoopReporter).unwrap();
    assert_eq!(plan.view().counts.files, 3);
    assert!(
        run_backup_plan(&plan, &cancel, &NoopReporter)
            .unwrap()
            .published
    );
    let destination = target.join(&plan.view().destination_name);
    for name in ["archive.tar.gz", "plain", ".hidden"] {
        assert_eq!(
            fs::read(destination.join(name)).unwrap(),
            fs::read(source.join(name)).unwrap()
        );
    }
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 3);
    let report = verify_backup(&target, &cancel, &NoopReporter).unwrap();
    assert_eq!((report.checked, report.bad, report.extra), (3, 0, 0));
}

#[test]
fn folder_filter_unfiltered_empty_shallow_folder_can_publish_root() {
    let world = tempfile::tempdir().unwrap();
    let source = world.path().join("source");
    let target = world.path().join("target");
    fs::create_dir_all(source.join("child")).unwrap();
    fs::create_dir(&target).unwrap();
    let cancel = AtomicBool::new(false);
    let plan = plan_backup(
        &BackupRequest::new(SourceSelection::Directory(source.clone()), target.clone()),
        &cancel,
        &NoopReporter,
    )
    .unwrap();
    assert_eq!(
        (plan.view().counts.files, plan.view().counts.directories),
        (0, 1)
    );
    assert!(
        run_backup_plan(&plan, &cancel, &NoopReporter)
            .unwrap()
            .published
    );
    assert_eq!(
        fs::read_dir(target.join(&plan.view().destination_name))
            .unwrap()
            .count(),
        0
    );
    assert!(source.join("child").is_dir());
    let report = verify_backup(&target, &cancel, &NoopReporter).unwrap();
    assert_eq!((report.checked, report.bad, report.extra), (0, 0, 0));
}

struct CancelCopy(std::sync::Arc<AtomicBool>);
struct CancelProgress(std::sync::Arc<AtomicBool>, usize);
impl bftool_core::reporter::ProgressHandle for CancelProgress {
    fn inc(&mut self, _: u64) {
        self.1 += 1;
        if self.1 == 2 {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    fn finish(&mut self) {}
}
impl bftool_core::reporter::Reporter for CancelCopy {
    fn log(&self, _: bftool_core::reporter::LogLevel, _: &str) {}
    fn progress_bytes(&self, _: &str, _: u64) -> Box<dyn bftool_core::reporter::ProgressHandle> {
        Box::new(CancelProgress(self.0.clone(), 0))
    }
}

#[test]
fn folder_filter_cancel_resume_keeps_exact_subset() {
    let (_world, source, target) = fixture();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let plan = plan_backup(&filtered(&source, &target, false), &cancel, &NoopReporter).unwrap();
    let first = run_backup_plan(&plan, &cancel, &CancelCopy(cancel.clone())).unwrap();
    assert_eq!(first.outcome, BackupOutcome::Cancelled);
    assert!(!first.published);
    assert!(first.verified >= 1);
    fs::write(source.join("child/new.zip"), "excluded new").unwrap();
    fs::write(source.join("skip.txt"), "excluded changed").unwrap();
    cancel.store(false, std::sync::atomic::Ordering::Relaxed);
    let resumed = resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter).unwrap();
    assert!(resumed.published);
    assert_eq!(resumed.verified, 2);
    assert!(resumed.skipped_verified >= 1);
    assert_eq!(
        fs::read_dir(target.join(&plan.view().destination_name))
            .unwrap()
            .count(),
        2
    );
    let report = verify_backup(&target, &cancel, &NoopReporter).unwrap();
    assert_eq!((report.checked, report.bad, report.extra), (2, 0, 0));
}

#[test]
fn folder_filter_version_one_missing_or_null_options_resumes_recursively() {
    for null in [false, true] {
        let (_world, source, target) = fixture();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let mut value = serde_json::to_value(BackupRequest::new(
            SourceSelection::Directory(source.clone()),
            target.clone(),
        ))
        .unwrap();
        value.as_object_mut().unwrap().remove("directory_options");
        let request: BackupRequest = serde_json::from_value(value).unwrap();
        assert_eq!(
            request.effective_directory_options(),
            DirectoryOptions::legacy()
        );
        let plan = plan_backup(&request, &cancel, &NoopReporter).unwrap();
        assert_eq!(plan.view().counts.files, 8);
        assert!(
            run_backup_plan(&plan, &cancel, &CancelCopy(cancel.clone()))
                .unwrap()
                .outcome
                == BackupOutcome::Cancelled
        );
        let path = manifest_path(&target, &plan.view().job_id);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest["version"] = 1.into();
        if null {
            manifest["request"]["directory_options"] = serde_json::Value::Null;
        } else {
            manifest["request"]
                .as_object_mut()
                .unwrap()
                .remove("directory_options");
        }
        let original = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(&path, &original).unwrap();
        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(
            resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter)
                .unwrap()
                .published
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "legacy manifest must not be rewritten"
        );
        assert_eq!(
            list_backup_history(&target).unwrap()[0].directory_options,
            DirectoryOptions::legacy()
        );
        let report = verify_backup(&target, &cancel, &NoopReporter).unwrap();
        assert_eq!((report.checked, report.bad, report.extra), (8, 0, 0));
        assert!(target
            .join(&plan.view().destination_name)
            .join("unrelated/empty")
            .is_dir());
    }
}

#[test]
fn folder_filter_manifest_rule_tampering_is_rejected_and_preserved() {
    for change in ["missing", "null", "suffix", "depth", "noncanonical"] {
        let (_world, source, target) = fixture();
        let cancel = AtomicBool::new(false);
        let plan = plan_backup(
            &filtered(&source, &target, change == "depth"),
            &cancel,
            &NoopReporter,
        )
        .unwrap();
        assert!(
            run_backup_plan(&plan, &cancel, &NoopReporter)
                .unwrap()
                .published
        );
        let path = manifest_path(&target, &plan.view().job_id);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match change {
            "missing" => {
                manifest["request"]
                    .as_object_mut()
                    .unwrap()
                    .remove("directory_options");
            }
            "null" => manifest["request"]["directory_options"] = serde_json::Value::Null,
            "suffix" => {
                manifest["request"]["directory_options"]["extensions"] = serde_json::json!(["txt"])
            }
            "depth" => manifest["request"]["directory_options"]["recursive"] = false.into(),
            "noncanonical" => {
                manifest["request"]["directory_options"]["extensions"] =
                    serde_json::json!(["ZIP", "tar"])
            }
            _ => unreachable!(),
        }
        let altered = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(&path, &altered).unwrap();
        assert!(list_backup_history(&target).is_err(), "accepted {change}");
        assert!(
            verify_backup(&target, &cancel, &NoopReporter).is_err(),
            "accepted {change}"
        );
        assert!(
            resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter).is_err(),
            "accepted {change}"
        );
        assert_eq!(fs::read(&path).unwrap(), altered);
        assert_eq!(fs::read(source.join("a.tar")).unwrap(), b"tar");
    }
}

#[test]
fn folder_filter_new_request_is_shallow() {
    let world = tempfile::tempdir().unwrap();
    let source = world.path().join("source");
    let target = world.path().join("target");
    fs::create_dir_all(source.join("child")).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("top.tar"), b"top").unwrap();
    fs::write(source.join("child/nested.zip"), b"nested").unwrap();
    let request = BackupRequest::new(SourceSelection::Directory(source), target);
    let plan = plan_backup(&request, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!(
        plan.view().counts.files,
        1,
        "new folder requests must only select first-level files"
    );
}

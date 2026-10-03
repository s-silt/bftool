//! A05 public-entry regressions. No mounted Windows disk or production safety bypass is used.
use bftool_core::config::Config;
use bftool_core::engine::verify::{VerifyIssueKind, VerifyOutcome, VerifyReport};
use bftool_core::engine::{paths, verify};
use bftool_core::reporter::NoopReporter;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

struct Fixture {
    temp: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(paths::drive_manifest_dir(temp.path())).unwrap();
        fs::create_dir_all(paths::drive_projects_dir(temp.path())).unwrap();
        fs::write(paths::drive_id_path(temp.path()), "备份1").unwrap();
        Self { temp }
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn project(&self, name: &str) -> PathBuf {
        paths::drive_projects_dir(self.root()).join(name)
    }
    fn manifest(&self, name: &str) -> PathBuf {
        paths::drive_manifest_dir(self.root()).join(format!("{name}.sha256.csv"))
    }
    fn catalog(&self, rows: &str) {
        fs::write(
            paths::drive_catalog_path(self.root()),
            format!("ProjectName,FileCount,TotalBytes\n{rows}"),
        )
        .unwrap();
    }
    fn payload(&self, name: &str, bytes: &[u8]) {
        fs::create_dir_all(self.project(name)).unwrap();
        fs::write(self.project(name).join("a.txt"), bytes).unwrap();
    }
    fn clean_project(&self, name: &str, bytes: &[u8]) {
        self.payload(name, bytes);
        fs::write(
            self.manifest(name),
            format!("Rel,Size,Hash\na.txt,{},{}\n", bytes.len(), hash(bytes)),
        )
        .unwrap();
    }
    fn verify(&self) -> VerifyReport {
        let before = snapshot(self.root());
        let report =
            verify::verify_drive(self.root(), &NoopReporter, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            snapshot(self.root()),
            before,
            "verification must not mutate backup data or metadata"
        );
        report
    }
    fn verify_one(&self, target: &Path) -> VerifyReport {
        let before = snapshot(self.root());
        let report = verify::verify_one(
            &Config::default(),
            &NoopReporter,
            target,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            snapshot(self.root()),
            before,
            "local spot check must also remain read-only"
        );
        report
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:X}", Sha256::digest(bytes))
}
fn snapshot(root: &Path) -> Vec<(PathBuf, String, Vec<u8>, SystemTime)> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.unwrap();
        let path = entry.path();
        let meta = fs::symlink_metadata(path).unwrap();
        let (kind, bytes) = if meta.file_type().is_symlink() {
            (
                "link",
                fs::read_link(path)
                    .unwrap()
                    .to_string_lossy()
                    .as_bytes()
                    .to_vec(),
            )
        } else if meta.is_file() {
            ("file", fs::read(path).unwrap())
        } else if meta.is_dir() {
            ("dir", Vec::new())
        } else {
            ("special", Vec::new())
        };
        out.push((
            path.strip_prefix(root).unwrap().to_path_buf(),
            kind.to_string(),
            bytes,
            meta.modified().unwrap(),
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}
fn has(report: &VerifyReport, project: &str, kind: VerifyIssueKind) -> bool {
    report
        .issues
        .iter()
        .any(|issue| issue.project == project && issue.kind == kind)
}
fn failed(report: &VerifyReport) {
    assert!(report.bad > 0);
    assert!(
        report.has_corruption(),
        "CLI's established nonzero integrity-failure gate must fire"
    );
    assert!(matches!(
        report.outcome(),
        VerifyOutcome::IssuesFound { .. }
    ));
    assert_eq!(report.outcome().label(), "发现完整性问题");
}

#[test]
fn public_whole_drive_checks_registered_payload() {
    let f = Fixture::new();
    f.clean_project("proj", b"hello");
    f.catalog("proj,1,5\n");
    let r = f.verify();
    assert_eq!(r.checked, 1);
    assert_eq!(r.bad, 0);
    assert_eq!(r.outcome(), VerifyOutcome::Clean);
}
#[test]
fn fresh_empty_drive_is_distinct_from_registered_missing_manifest() {
    let empty = Fixture::new();
    let r = empty.verify();
    assert_eq!(r.checked, 0);
    assert_eq!(r.bad, 0);
    let registered = Fixture::new();
    registered.payload("proj", b"hello");
    registered.catalog("proj,1,5\n");
    let r = registered.verify();
    assert_eq!(r.checked, 0);
    failed(&r);
    assert!(has(&r, "proj", VerifyIssueKind::Unverifiable));
}
#[test]
fn deleted_only_manifest_is_failure_in_both_public_entries() {
    let f = Fixture::new();
    f.clean_project("proj", b"hello");
    f.catalog("proj,1,5\n");
    fs::remove_file(f.manifest("proj")).unwrap();
    let whole = f.verify();
    let one = f.verify_one(&f.project("proj"));
    for r in [&whole, &one] {
        failed(r);
        assert_eq!(r.checked, 0);
        assert!(has(r, "proj", VerifyIssueKind::Unverifiable));
    }
    assert_eq!(whole.bad, one.bad);
}
#[test]
fn registered_project_with_no_payload_or_manifest_is_not_skipped() {
    let f = Fixture::new();
    f.catalog("lost,1,5\n");
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "lost", VerifyIssueKind::Missing));
    assert!(has(&r, "lost", VerifyIssueKind::Unverifiable));
}
#[test]
fn orphan_payload_without_catalog_or_manifest_is_unverifiable() {
    let f = Fixture::new();
    f.payload("orphan", b"hello");
    let r = f.verify();
    failed(&r);
    assert_eq!(r.checked, 0);
    assert!(has(&r, "orphan", VerifyIssueKind::Unverifiable));
}
#[test]
fn orphan_manifest_without_catalog_or_payload_is_failure() {
    let f = Fixture::new();
    fs::write(f.manifest("orphan"), "Rel,Size,Hash\n").unwrap();
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "orphan", VerifyIssueKind::Missing));
}
#[test]
fn complete_payload_and_manifest_without_catalog_are_still_unverifiable() {
    let f = Fixture::new();
    f.clean_project("orphan", b"hello");
    let r = f.verify();
    assert_eq!(r.checked, 1);
    failed(&r);
    assert!(has(&r, "orphan", VerifyIssueKind::Unverifiable));
}
#[test]
fn corrupted_catalog_header_does_not_hide_good_content_or_mark_clean() {
    let f = Fixture::new();
    f.clean_project("proj", b"hello");
    fs::write(paths::drive_catalog_path(f.root()), "broken\nmetadata\n").unwrap();
    let r = f.verify();
    failed(&r);
    assert_eq!(r.checked, 1);
    assert!(has(&r, "<本盘>", VerifyIssueKind::ReadError));
    failed(&f.verify_one(&f.project("proj")));
}
#[test]
fn malformed_catalog_row_does_not_abort_valid_project_checks() {
    let f = Fixture::new();
    f.clean_project("good", b"hello");
    f.catalog("good,1,5\nbad,1\n");
    let r = f.verify();
    failed(&r);
    assert_eq!(r.checked, 1);
    assert!(has(&r, "<本盘>", VerifyIssueKind::ReadError));
}
#[test]
fn invalid_catalog_counts_and_duplicate_registration_are_fail_closed() {
    let f = Fixture::new();
    f.clean_project("proj", b"hello");
    f.catalog("proj,invalid,5\nproj,1,5\n");
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "proj", VerifyIssueKind::ReadError));
}
#[test]
fn unsafe_catalog_name_is_not_followed_outside_project_root() {
    let f = Fixture::new();
    f.catalog("../outside,1,5\n");
    fs::create_dir_all(f.root().join("outside")).unwrap();
    fs::write(f.root().join("outside/a.txt"), b"hello").unwrap();
    let r = f.verify();
    failed(&r);
    assert_eq!(r.checked, 0);
    assert!(has(&r, "<本盘>", VerifyIssueKind::ReadError));
}
#[test]
fn manifest_truncation_is_detected_by_catalog_counts() {
    let f = Fixture::new();
    f.clean_project("proj", b"hello");
    f.catalog("proj,2,10\n");
    let whole = f.verify();
    let one = f.verify_one(&f.project("proj"));
    for r in [&whole, &one] {
        failed(r);
        assert_eq!(r.checked, 1);
        assert!(has(r, "proj", VerifyIssueKind::Unverifiable));
    }
}
#[test]
fn empty_manifest_for_nonempty_catalog_is_not_clean() {
    let f = Fixture::new();
    f.payload("proj", b"hello");
    f.catalog("proj,1,5\n");
    fs::write(f.manifest("proj"), "Rel,Size,Hash\n").unwrap();
    let r = f.verify();
    failed(&r);
    assert_eq!(r.checked, 0);
    assert_eq!(r.extra, 1);
}
#[test]
fn legitimately_empty_registered_project_has_no_false_integrity_failure() {
    let f = Fixture::new();
    fs::create_dir_all(f.project("proj")).unwrap();
    f.catalog("proj,0,0\n");
    fs::write(f.manifest("proj"), "Rel,Size,Hash\n").unwrap();
    let r = f.verify();
    assert_eq!(r.checked, 0);
    assert_eq!(r.bad, 0);
    assert_eq!(r.extra, 0);
}
#[test]
fn missing_manifest_directory_is_structured_failure() {
    let f = Fixture::new();
    f.payload("proj", b"hello");
    f.catalog("proj,1,5\n");
    fs::remove_dir(paths::drive_manifest_dir(f.root())).unwrap();
    let r = f.verify();
    failed(&r);
    assert_eq!(r.checked, 0);
    assert!(has(&r, "<本盘>", VerifyIssueKind::Missing));
}
#[test]
fn manifest_directory_replaced_by_file_is_failure() {
    let f = Fixture::new();
    fs::remove_dir(paths::drive_manifest_dir(f.root())).unwrap();
    fs::write(paths::drive_manifest_dir(f.root()), b"file").unwrap();
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "<本盘>", VerifyIssueKind::ReadError));
}
#[test]
fn projects_directory_replaced_by_file_is_failure() {
    let f = Fixture::new();
    fs::remove_dir(paths::drive_projects_dir(f.root())).unwrap();
    fs::write(paths::drive_projects_dir(f.root()), b"file").unwrap();
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "<本盘>", VerifyIssueKind::ReadError));
}
#[test]
fn manifest_path_replaced_by_directory_is_failure() {
    let f = Fixture::new();
    f.payload("proj", b"hello");
    f.catalog("proj,1,5\n");
    fs::create_dir(f.manifest("proj")).unwrap();
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "proj", VerifyIssueKind::ReadError));
}
#[test]
fn project_path_replaced_by_file_is_failure() {
    let f = Fixture::new();
    f.catalog("proj,0,0\n");
    fs::write(f.project("proj"), b"file").unwrap();
    fs::write(f.manifest("proj"), "Rel,Size,Hash\n").unwrap();
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "proj", VerifyIssueKind::ReadError));
}
#[test]
fn partial_success_keeps_checked_count_and_missing_project_failure() {
    let f = Fixture::new();
    f.clean_project("good", b"hello");
    f.payload("lost", b"world");
    f.catalog("good,1,5\nlost,1,5\n");
    let r = f.verify();
    failed(&r);
    assert_eq!(r.checked, 1);
    assert!(has(&r, "lost", VerifyIssueKind::Unverifiable));
}
#[test]
fn cancelled_before_work_does_not_claim_success_or_failure() {
    let f = Fixture::new();
    f.payload("lost", b"hello");
    f.catalog("lost,1,5\n");
    let r = verify::verify_drive(f.root(), &NoopReporter, &AtomicBool::new(true)).unwrap();
    assert_eq!(r.outcome(), VerifyOutcome::Cancelled);
    assert_eq!(r.checked, 0);
    assert_eq!(r.bad, 0);
}
#[test]
fn unexpected_manifest_directory_entry_is_reported_as_extra() {
    let f = Fixture::new();
    fs::write(
        paths::drive_manifest_dir(f.root()).join("unexpected.txt"),
        b"x",
    )
    .unwrap();
    let r = f.verify();
    assert_eq!(r.checked, 0);
    assert_eq!(r.extra, 1);
    assert!(matches!(r.outcome(), VerifyOutcome::ExtraOnly { extra: 1 }));
}
#[test]
fn public_root_entry_requires_initialized_regular_drive() {
    let temp = tempfile::tempdir().unwrap();
    assert!(verify::verify_drive(temp.path(), &NoopReporter, &AtomicBool::new(false)).is_err());
}
#[cfg(unix)]
#[test]
fn project_symlink_is_not_followed_even_when_content_matches() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("a.txt"), b"hello").unwrap();
    std::os::unix::fs::symlink(outside.path(), f.project("proj")).unwrap();
    f.catalog("proj,1,5\n");
    fs::write(
        f.manifest("proj"),
        format!("Rel,Size,Hash\na.txt,5,{}\n", hash(b"hello")),
    )
    .unwrap();
    let r = f.verify();
    failed(&r);
    assert_eq!(r.checked, 0);
    assert!(has(&r, "proj", VerifyIssueKind::ReadError));
}
#[cfg(unix)]
#[test]
fn listed_file_parent_symlink_is_not_followed() {
    let f = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("a.txt"), b"hello").unwrap();
    fs::create_dir_all(f.project("proj")).unwrap();
    std::os::unix::fs::symlink(outside.path(), f.project("proj").join("sub")).unwrap();
    f.catalog("proj,1,5\n");
    fs::write(
        f.manifest("proj"),
        format!("Rel,Size,Hash\nsub\\a.txt,5,{}\n", hash(b"hello")),
    )
    .unwrap();
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "proj", VerifyIssueKind::Corrupt));
}
#[cfg(unix)]
#[test]
fn manifest_and_catalog_symlinks_are_not_followed() {
    for metadata in ["manifest", "catalog"] {
        let f = Fixture::new();
        f.clean_project("proj", b"hello");
        f.catalog("proj,1,5\n");
        let source = if metadata == "manifest" {
            f.manifest("proj")
        } else {
            paths::drive_catalog_path(f.root())
        };
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("metadata.csv");
        fs::rename(&source, &target).unwrap();
        std::os::unix::fs::symlink(&target, &source).unwrap();
        let r = f.verify();
        failed(&r);
        assert!(r
            .issues
            .iter()
            .any(|issue| issue.kind == VerifyIssueKind::ReadError));
    }
}
#[cfg(unix)]
#[test]
fn extra_symlink_in_empty_project_is_unverifiable() {
    let f = Fixture::new();
    fs::create_dir_all(f.project("proj")).unwrap();
    f.catalog("proj,0,0\n");
    fs::write(f.manifest("proj"), "Rel,Size,Hash\n").unwrap();
    std::os::unix::fs::symlink(
        "/nonexistent-bftool-regression-target",
        f.project("proj").join("link"),
    )
    .unwrap();
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "proj", VerifyIssueKind::Unverifiable));
}
#[test]
fn duplicate_catalog_identity_columns_are_untrusted() {
    let f = Fixture::new();
    f.clean_project("proj", b"hello");
    fs::write(
        paths::drive_catalog_path(f.root()),
        "ProjectName,ProjectName,FileCount,TotalBytes\nproj,hidden,1,5\n",
    )
    .unwrap();
    let r = f.verify();
    failed(&r);
    assert_eq!(r.checked, 1);
    assert!(has(&r, "<本盘>", VerifyIssueKind::ReadError));
}
#[test]
fn duplicate_manifest_rows_or_identity_columns_are_untrusted() {
    for rows in [
        "Rel,Rel,Size,Hash\na.txt,hidden.txt,5,\n",
        "Rel,Size,Hash\na.txt,5,\na.txt,5,\n",
    ] {
        let f = Fixture::new();
        f.payload("proj", b"hello");
        f.catalog("proj,1,5\n");
        fs::write(f.manifest("proj"), rows).unwrap();
        let r = f.verify();
        failed(&r);
        assert!(has(&r, "proj", VerifyIssueKind::ReadError));
    }
}
#[test]
fn manifest_row_with_empty_identity_and_nonempty_metadata_is_untrusted() {
    let f = Fixture::new();
    fs::create_dir_all(f.project("proj")).unwrap();
    f.catalog("proj,0,0\n");
    fs::write(f.manifest("proj"), "Rel,Size,Hash\n,5,\n").unwrap();
    let r = f.verify();
    failed(&r);
    assert!(has(&r, "proj", VerifyIssueKind::ReadError));
}
#[cfg(unix)]
#[test]
fn single_unlisted_symlink_uses_same_unverifiable_semantics_as_whole_project() {
    let f = Fixture::new();
    fs::create_dir_all(f.project("proj")).unwrap();
    f.catalog("proj,0,0\n");
    fs::write(f.manifest("proj"), "Rel,Size,Hash\n").unwrap();
    let link = f.project("proj").join("link");
    std::os::unix::fs::symlink("/nonexistent-bftool-regression-target", &link).unwrap();
    for r in [f.verify(), f.verify_one(&link)] {
        failed(&r);
        assert!(has(&r, "proj", VerifyIssueKind::Unverifiable));
    }
}

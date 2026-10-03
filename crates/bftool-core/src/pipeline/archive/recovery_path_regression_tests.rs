//! Real commit interruptions in temporary directories; no physical volumes or external programs.
use super::{archive_leaf_matches_source, check_pending_txn, CommitJournal, CommitStage};
use crate::config::Config;
use crate::engine::{drive::DriveInfo, paths};
use crate::pipeline::archive::{plan::plan_on_drive, runner::run_plan, types::Options};
use crate::reporter::{LogLevel, NoopReporter, ProgressHandle, Reporter};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

struct BlockDriveCatalog(PathBuf);
impl Reporter for BlockDriveCatalog {
    fn log(&self, _: LogLevel, message: &str) {
        if message == "复制完成，开始校验…" {
            fs::create_dir(&self.0).unwrap();
        }
    }
    fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle> {
        NoopReporter.progress_bytes(label, total)
    }
}

fn interrupted(collision: bool) -> (tempfile::TempDir, Config, DriveInfo, PathBuf) {
    let world = tempfile::tempdir().unwrap();
    let cfg = Config {
        // Leave the public configuration in its ordinary absolute, non-canonical form.
        ready_root: world.path().join("ready"),
        archived_root: world.path().join("archived"),
        system_root: world.path().join("system"),
        stable_minutes: 0,
        reserve_gb: 0,
        min_drive_gb: 0,
        test_archives: false,
        ..Config::default()
    };
    let root = world.path().join("drive");
    for path in [&cfg.ready_root, &cfg.archived_root, &cfg.system_root, &root] {
        fs::create_dir(path).unwrap();
    }
    fs::create_dir_all(paths::drive_info_dir(&root)).unwrap();
    fs::write(paths::drive_id_path(&root), "备份1").unwrap();
    let drive = DriveInfo {
        root,
        letter: "SYNTHETIC".into(),
        id: "备份1".into(),
        sealed: false,
        free_bytes: 1 << 40,
        total_bytes: 1 << 40,
    };
    let source = cfg.ready_root.join("001project");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.txt"), b"VERIFIED SOURCE").unwrap();
    if collision {
        fs::create_dir(cfg.archived_root.join("001project")).unwrap();
        fs::write(
            cfg.archived_root.join("001project/keep.txt"),
            b"UNKNOWN OCCUPANT",
        )
        .unwrap();
    }
    let options = Options {
        incremental: true,
        // The explicit invalid-volume label prevents both scanning and real drive-letter lookup.
        drive_letter_override: Some("SYNTHETIC".into()),
        ..Options::default()
    };
    let plan = plan_on_drive(&cfg, drive.clone(), &options, &NoopReporter).unwrap();
    assert_eq!(plan.items.len(), 1);
    let catalog = paths::drive_catalog_path(&drive.root);
    let result = run_plan(
        &cfg,
        &plan,
        &AtomicBool::new(false),
        &BlockDriveCatalog(catalog.clone()),
    )
    .unwrap();
    assert_eq!((result.handled, result.failed), (0, 1));
    let marker = paths::system_pending_txn(&cfg.system_root);
    let journal: CommitJournal = toml::from_str(&fs::read_to_string(&marker).unwrap()).unwrap();
    assert_eq!(journal.stage, CommitStage::GlobalWritten);
    assert!(!journal.retain_source);
    assert_eq!(
        journal.pending.src_path,
        plan.items[0].source_path.to_str().unwrap()
    );
    assert_eq!(
        Path::new(&journal.pending.move_to).parent().unwrap(),
        cfg.archived_root.canonicalize().unwrap()
    );
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"VERIFIED SOURCE");
    fs::remove_dir(&catalog).unwrap();
    (world, cfg, drive, source)
}

fn edit_move(cfg: &Config, change: impl FnOnce(&mut CommitJournal)) -> Vec<u8> {
    let marker = paths::system_pending_txn(&cfg.system_root);
    let mut journal: CommitJournal = toml::from_str(&fs::read_to_string(&marker).unwrap()).unwrap();
    change(&mut journal);
    fs::write(&marker, toml::to_string_pretty(&journal).unwrap()).unwrap();
    fs::read(marker).unwrap()
}

fn assert_rejected_unchanged(cfg: &Config, drive: &DriveInfo, source: &Path, marker: &[u8]) {
    let global = paths::system_global_catalog(&cfg.system_root);
    let before_global = fs::read(&global).unwrap();
    let error = check_pending_txn(cfg, &NoopReporter, &drive.id).unwrap_err();
    assert!(!error.to_string().is_empty());
    assert_eq!(
        fs::read(paths::system_pending_txn(&cfg.system_root)).unwrap(),
        marker
    );
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"VERIFIED SOURCE");
    assert_eq!(
        fs::read(paths::drive_projects_dir(&drive.root).join("001project/a.txt")).unwrap(),
        b"VERIFIED SOURCE"
    );
    assert_eq!(fs::read(global).unwrap(), before_global);
    assert!(
        !paths::drive_catalog_path(&drive.root).exists(),
        "reject before metadata replay"
    );
}

fn assert_recovered(cfg: &Config, drive: &DriveInfo, source: &Path, moved: &Path) {
    check_pending_txn(cfg, &NoopReporter, &drive.id).unwrap();
    assert!(!source.exists());
    assert!(!paths::system_pending_txn(&cfg.system_root).exists());
    assert_eq!(fs::read(moved.join("a.txt")).unwrap(), b"VERIFIED SOURCE");
    assert_eq!(
        fs::read(paths::drive_projects_dir(&drive.root).join("001project/a.txt")).unwrap(),
        b"VERIFIED SOURCE"
    );
    assert_eq!(
        fs::read_to_string(paths::system_global_catalog(&cfg.system_root))
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("001project,"))
            .count(),
        1
    );
    assert_eq!(
        fs::read_to_string(paths::drive_catalog_path(&drive.root))
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("001,"))
            .count(),
        1
    );
}

#[test]
fn ordinary_absolute_config_commits_canonical_move_path_and_recovers() {
    let (_world, cfg, drive, source) = interrupted(false);
    assert!(cfg.archived_root.is_absolute());
    #[cfg(windows)]
    assert_ne!(cfg.archived_root, cfg.archived_root.canonicalize().unwrap());
    assert_recovered(&cfg, &drive, &source, &cfg.archived_root.join("001project"));
}

#[test]
fn existing_modern_ordinary_absolute_move_path_recovers_without_rewrite() {
    let (_world, cfg, drive, source) = interrupted(false);
    let moved = cfg.archived_root.join("001project");
    edit_move(&cfg, |journal| {
        journal.pending.move_to = moved.to_str().unwrap().into()
    });
    assert_recovered(&cfg, &drive, &source, &moved);
}

#[test]
fn generated_collision_leaf_recovers_and_preserves_original_occupant() {
    let (_world, cfg, drive, source) = interrupted(true);
    let journal: CommitJournal =
        toml::from_str(&fs::read_to_string(paths::system_pending_txn(&cfg.system_root)).unwrap())
            .unwrap();
    let moved = PathBuf::from(journal.pending.move_to);
    assert_ne!(moved.file_name().unwrap(), "001project");
    assert_recovered(&cfg, &drive, &source, &moved);
    assert_eq!(
        fs::read(cfg.archived_root.join("001project/keep.txt")).unwrap(),
        b"UNKNOWN OCCUPANT"
    );
}

#[test]
fn exact_collision_name_shape_is_required() {
    for name in [
        "001project",
        "001project_20261003063000",
        "001project_20261003063000_2",
        "001project_20261003063000_17",
    ] {
        assert!(archive_leaf_matches_source("001project", name), "{name}");
    }
    for name in [
        "other",
        "001project_20261303063000",
        "001project_202610030630",
        "001project_20261003063000_1",
        "001project_20261003063000_02",
        "001project_20261003063000_+2",
        "001project_20261003063000_2_more",
        "001project:stream",
        "001project\0",
    ] {
        assert!(!archive_leaf_matches_source("001project", name), "{name:?}");
    }
}

#[test]
fn outside_root_traversal_relative_and_mismatched_leaves_keep_evidence() {
    for case in 0..10 {
        let (world, cfg, drive, source) = interrupted(false);
        let outside = world.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), b"OUTSIDE USER FILE").unwrap();
        let path = match case {
            0 => outside.join("001project"),
            1 => world.path().join("missing/001project"),
            2 => cfg.archived_root.join("../archived/001project"),
            3 => PathBuf::from("archived/001project"),
            4 => cfg.archived_root.join("different_leaf"),
            5 => cfg.archived_root.join("001project_20261303063000"),
            6 => cfg.archived_root.join("001project_20261003063000_02"),
            7 => cfg.archived_root.join("001project:stream"),
            8 => cfg.archived_root.join("001project/"),
            _ => cfg.archived_root.join("001project/."),
        };
        let marker = edit_move(&cfg, |journal| {
            journal.pending.move_to = path.to_str().unwrap().into()
        });
        assert_rejected_unchanged(&cfg, &drive, &source, &marker);
        assert_eq!(
            fs::read(outside.join("keep.txt")).unwrap(),
            b"OUTSIDE USER FILE"
        );
        assert!(!outside.join("001project").exists());
    }
}

#[test]
fn source_leaf_identity_mismatch_keeps_evidence() {
    let (_world, cfg, drive, source) = interrupted(false);
    let marker = edit_move(&cfg, |journal| {
        journal.pending.project_src_name = "different_leaf".into()
    });
    assert_rejected_unchanged(&cfg, &drive, &source, &marker);
}

#[cfg(unix)]
#[test]
fn linked_configured_or_recorded_parent_is_rejected_even_for_same_root() {
    for configured in [false, true] {
        let (world, mut cfg, drive, source) = interrupted(false);
        let link = world.path().join("linked-archive");
        std::os::unix::fs::symlink(&cfg.archived_root, &link).unwrap();
        if configured {
            cfg.archived_root = link;
        } else {
            edit_move(&cfg, |journal| {
                journal.pending.move_to = link.join("001project").to_str().unwrap().into()
            });
        }
        let marker = fs::read(paths::system_pending_txn(&cfg.system_root)).unwrap();
        assert_rejected_unchanged(&cfg, &drive, &source, &marker);
    }
}

#[cfg(unix)]
#[test]
fn same_content_moved_leaf_symlink_cannot_complete_recovery() {
    let (_world, cfg, drive, source) = interrupted(false);
    std::os::unix::fs::symlink(&source, cfg.archived_root.join("001project")).unwrap();
    let marker = fs::read(paths::system_pending_txn(&cfg.system_root)).unwrap();
    assert_rejected_unchanged(&cfg, &drive, &source, &marker);
    assert!(fs::symlink_metadata(cfg.archived_root.join("001project"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn already_moved_matching_snapshot_recovers_and_leaves_new_source_alone() {
    let (_world, cfg, drive, source) = interrupted(false);
    let moved = cfg.archived_root.join("001project");
    fs::rename(&source, &moved).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.txt"), b"NEW SOURCE").unwrap();
    check_pending_txn(&cfg, &NoopReporter, &drive.id).unwrap();
    assert!(!paths::system_pending_txn(&cfg.system_root).exists());
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"NEW SOURCE");
    assert_eq!(fs::read(moved.join("a.txt")).unwrap(), b"VERIFIED SOURCE");
}

#[cfg(windows)]
#[test]
fn windows_equivalent_parent_spellings_use_filesystem_identity() {
    for verbatim in [false, true] {
        let (_world, cfg, drive, source) = interrupted(false);
        let root = if verbatim {
            cfg.archived_root.canonicalize().unwrap()
        } else {
            cfg.archived_root.clone()
        };
        // Case changes only the parent spelling; the committed leaf remains exact.
        let parent = PathBuf::from(root.to_str().unwrap().to_ascii_uppercase());
        edit_move(&cfg, |journal| {
            journal.pending.move_to = parent.join("001project").to_str().unwrap().into()
        });
        assert_recovered(&cfg, &drive, &source, &cfg.archived_root.join("001project"));
    }
}

#[cfg(windows)]
#[test]
fn windows_ambiguous_leaf_aliases_are_rejected() {
    for leaf in [
        "001project.",
        "001project ",
        "CON",
        "CON.txt",
        "COM1",
        "LPT9.txt",
        "CONIN$",
        "COM¹",
        "001project:stream",
        "001project?",
        "001project|",
        "001project\u{0001}",
    ] {
        assert!(!super::safe_archive_leaf(leaf), "{leaf:?}");
    }
    for path in ["C:archived\\001project", "\\archived\\001project"] {
        let (_world, cfg, drive, source) = interrupted(false);
        let marker = edit_move(&cfg, |journal| journal.pending.move_to = path.into());
        assert_rejected_unchanged(&cfg, &drive, &source, &marker);
    }
}

#[test]
fn existing_modern_ordinal_collision_leaf_recovers_without_taking_occupied_names() {
    let (_world, cfg, drive, source) = interrupted(true);
    let base = "001project_20261003063000";
    fs::create_dir(cfg.archived_root.join(base)).unwrap();
    fs::write(
        cfg.archived_root.join(base).join("keep.txt"),
        b"OTHER OCCUPANT",
    )
    .unwrap();
    let moved = cfg.archived_root.join(format!("{base}_2"));
    edit_move(&cfg, |journal| {
        journal.pending.move_to = moved.to_str().unwrap().into()
    });
    assert_recovered(&cfg, &drive, &source, &moved);
    assert_eq!(
        fs::read(cfg.archived_root.join(base).join("keep.txt")).unwrap(),
        b"OTHER OCCUPANT"
    );
    assert_eq!(
        fs::read(cfg.archived_root.join("001project/keep.txt")).unwrap(),
        b"UNKNOWN OCCUPANT"
    );
}

struct OccupyAtMoveRead {
    target: PathBuf,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
struct OccupyProgress {
    target: PathBuf,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl ProgressHandle for OccupyProgress {
    fn inc(&mut self, _: u64) {}
    fn finish(&mut self) {
        // Recovery reads the verified backup, then the still-pending source.
        if self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            fs::create_dir(&self.target).unwrap();
            fs::write(self.target.join("keep.txt"), b"LATE OCCUPANT").unwrap();
        }
    }
}
impl Reporter for OccupyAtMoveRead {
    fn log(&self, _: LogLevel, _: &str) {}
    fn progress_bytes(&self, label: &str, _: u64) -> Box<dyn ProgressHandle> {
        if label == "校验" {
            Box::new(OccupyProgress {
                target: self.target.clone(),
                reads: self.reads.clone(),
            })
        } else {
            NoopReporter.progress_bytes(label, 0)
        }
    }
}

#[test]
fn recovery_late_occupant_is_not_replaced() {
    let (_world, cfg, drive, source) = interrupted(false);
    let moved = cfg.archived_root.join("001project");
    let reporter = OccupyAtMoveRead {
        target: moved.clone(),
        reads: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    assert!(check_pending_txn(&cfg, &reporter, &drive.id).is_err());
    assert!(paths::system_pending_txn(&cfg.system_root).is_file());
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"VERIFIED SOURCE");
    assert_eq!(fs::read(moved.join("keep.txt")).unwrap(), b"LATE OCCUPANT");
    assert!(!moved.join("a.txt").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn recovery_refuses_replaced_ordinary_archive_parent() {
    struct ReplaceParent {
        root: PathBuf,
        old: PathBuf,
    }
    impl Reporter for ReplaceParent {
        fn log(&self, _: LogLevel, message: &str) {
            if message.starts_with("恢复已校验的事务副本") {
                fs::rename(&self.root, &self.old).unwrap();
                fs::create_dir(&self.root).unwrap();
                fs::write(self.root.join("keep.txt"), b"REPLACEMENT USER FILE").unwrap();
            }
        }
        fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle> {
            NoopReporter.progress_bytes(label, total)
        }
    }
    let (world, cfg, drive, source) = interrupted(false);
    let reporter = ReplaceParent {
        root: cfg.archived_root.clone(),
        old: world.path().join("original-archive"),
    };
    assert!(check_pending_txn(&cfg, &reporter, &drive.id).is_err());
    assert!(paths::system_pending_txn(&cfg.system_root).is_file());
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"VERIFIED SOURCE");
    assert_eq!(
        fs::read(cfg.archived_root.join("keep.txt")).unwrap(),
        b"REPLACEMENT USER FILE"
    );
    assert!(!cfg.archived_root.join("001project").exists());
    assert!(!reporter.old.join("001project").exists());
}

#[test]
fn ordinary_single_file_commit_interruption_and_recovery_preserve_content() {
    let (_world, cfg, drive, source) = interrupted(false);
    assert_recovered(&cfg, &drive, &source, &cfg.archived_root.join("001project"));
    fs::remove_file(paths::drive_catalog_path(&drive.root)).unwrap();
    let file = cfg.ready_root.join("002file.txt");
    fs::write(&file, b"SINGLE FILE CONTENT").unwrap();
    let options = Options {
        include_ext: Some(vec!["txt".into()]),
        include_subfolder_projects: false,
        drive_letter_override: Some("SYNTHETIC".into()),
        ..Options::default()
    };
    let plan = plan_on_drive(&cfg, drive.clone(), &options, &NoopReporter).unwrap();
    assert_eq!(plan.items.len(), 1);
    let catalog = paths::drive_catalog_path(&drive.root);
    let result = run_plan(
        &cfg,
        &plan,
        &AtomicBool::new(false),
        &BlockDriveCatalog(catalog.clone()),
    )
    .unwrap();
    assert_eq!((result.handled, result.failed), (0, 1));
    assert!(paths::system_pending_txn(&cfg.system_root).is_file());
    fs::remove_dir(&catalog).unwrap();
    check_pending_txn(&cfg, &NoopReporter, &drive.id).unwrap();
    assert!(!file.exists());
    assert_eq!(
        fs::read(cfg.archived_root.join("002file.txt")).unwrap(),
        b"SINGLE FILE CONTENT"
    );
    assert_eq!(
        fs::read(paths::drive_projects_dir(&drive.root).join("002file.txt/002file.txt")).unwrap(),
        b"SINGLE FILE CONTENT"
    );
    assert!(!paths::system_pending_txn(&cfg.system_root).exists());
}

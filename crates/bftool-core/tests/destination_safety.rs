//! Public-API regressions use only synthetic directories. Windows junction behavior
//! requires a separate real-platform run and is not claimed by these Linux tests.
#![cfg(target_os = "linux")]

use bftool_core::config::Config;
use bftool_core::engine::drive::DriveInfo;
use bftool_core::engine::paths;
use bftool_core::pipeline::archive::{plan_on_drive, run_plan, Options, PlanAction};
use bftool_core::reporter::{LogLevel, NoopReporter, ProgressHandle, Reporter};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

fn world() -> (tempfile::TempDir, Config, DriveInfo) {
    let world = tempfile::tempdir().unwrap();
    let config = Config {
        ready_root: world.path().join("ready"),
        archived_root: world.path().join("archived"),
        system_root: world.path().join("system"),
        reserve_gb: 0,
        min_drive_gb: 0,
        stable_minutes: 0,
        test_archives: false,
        ..Config::default()
    };
    for root in [
        &config.ready_root,
        &config.archived_root,
        &config.system_root,
    ] {
        fs::create_dir_all(root).unwrap();
    }
    let root = world.path().join("synthetic-drive");
    fs::create_dir_all(paths::drive_info_dir(&root)).unwrap();
    fs::write(paths::drive_id_path(&root), "备份1").unwrap();
    let drive = DriveInfo {
        letter: "T".into(),
        root,
        id: "备份1".into(),
        sealed: false,
        free_bytes: 1 << 40,
        total_bytes: 1 << 40,
    };
    (world, config, drive)
}

fn options() -> Options {
    Options {
        retain_source: true,
        ..Options::default()
    }
}

#[test]
fn unowned_existing_payload_gets_a_fresh_complete_copy_and_is_preserved() {
    for old in [b"OLD".as_slice(), b"OLD-UNRELATED".as_slice()] {
        let (_world, config, drive) = world();
        let source = config.ready_root.join("project");
        let old_destination = paths::drive_projects_dir(&drive.root).join("project");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&old_destination).unwrap();
        fs::write(source.join("a.txt"), b"NEW").unwrap();
        fs::write(old_destination.join("a.txt"), old).unwrap();
        let plan = plan_on_drive(&config, drive.clone(), &options(), &NoopReporter).unwrap();
        let destination_name = match &plan.items[0].action {
            PlanAction::RenameAndArchive { dest_name } => dest_name,
            action => panic!("unowned destination must use a new name: {action:?}"),
        };
        assert_ne!(destination_name, "project");
        let summary = run_plan(&config, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
        assert_eq!((summary.handled, summary.failed), (1, 0));
        assert_eq!(fs::read(old_destination.join("a.txt")).unwrap(), old);
        assert_eq!(
            fs::read(
                paths::drive_projects_dir(&drive.root)
                    .join(destination_name)
                    .join("a.txt")
            )
            .unwrap(),
            b"NEW"
        );
        assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"NEW");
    }
}

struct ReplaceReservedDirectory {
    destination: PathBuf,
    retained: PathBuf,
    outside: Option<PathBuf>,
    replaced: AtomicBool,
}
impl Reporter for ReplaceReservedDirectory {
    fn log(&self, level: LogLevel, message: &str) {
        if level != LogLevel::Info
            || !message.starts_with("复制并生成源校验和")
            || self.replaced.swap(true, Ordering::SeqCst)
        {
            return;
        }
        fs::rename(&self.destination, &self.retained).unwrap();
        if let Some(outside) = &self.outside {
            std::os::unix::fs::symlink(outside, &self.destination).unwrap();
        } else {
            fs::create_dir(&self.destination).unwrap();
            fs::write(
                self.destination.join("user-extra.txt"),
                b"UNKNOWN-USER-DATA",
            )
            .unwrap();
        }
    }
    fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle> {
        NoopReporter.progress_bytes(label, total)
    }
}

#[test]
fn reserved_directory_object_replacement_keeps_source_and_unknown_user_extras() {
    let (world, config, drive) = world();
    let source = config.ready_root.join("project");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.txt"), b"SOURCE").unwrap();
    let plan = plan_on_drive(&config, drive.clone(), &options(), &NoopReporter).unwrap();
    let destination = paths::drive_projects_dir(&drive.root).join("project");
    let reporter = ReplaceReservedDirectory {
        destination: destination.clone(),
        retained: world.path().join("retained-owned"),
        outside: None,
        replaced: AtomicBool::new(false),
    };
    let summary = run_plan(&config, &plan, &AtomicBool::new(false), &reporter).unwrap();
    assert!(reporter.replaced.load(Ordering::SeqCst));
    assert_eq!((summary.handled, summary.failed), (0, 1));
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"SOURCE");
    assert_eq!(
        fs::read(destination.join("user-extra.txt")).unwrap(),
        b"UNKNOWN-USER-DATA"
    );
    assert!(!destination.join("a.txt").exists());
    assert!(!paths::system_global_catalog(&config.system_root).exists());
    assert!(!paths::drive_catalog_path(&drive.root).exists());
}

#[test]
fn late_destination_symlink_replacement_cannot_write_outside_the_drive() {
    let (world, config, drive) = world();
    let source = config.ready_root.join("project");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.txt"), b"SOURCE").unwrap();
    let outside = world.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let plan = plan_on_drive(&config, drive.clone(), &options(), &NoopReporter).unwrap();
    let reporter = ReplaceReservedDirectory {
        destination: paths::drive_projects_dir(&drive.root).join("project"),
        retained: world.path().join("retained-owned"),
        outside: Some(outside.clone()),
        replaced: AtomicBool::new(false),
    };
    let summary = run_plan(&config, &plan, &AtomicBool::new(false), &reporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (0, 1));
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"SOURCE");
    assert!(!paths::system_global_catalog(&config.system_root).exists());
}

#[test]
fn preexisting_projects_symlink_is_rejected_before_any_outside_directory_is_created() {
    let (world, config, drive) = world();
    let source = config.ready_root.join("project");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("a.txt"), b"SOURCE").unwrap();
    let outside = world.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, paths::drive_projects_dir(&drive.root)).unwrap();
    let plan = plan_on_drive(&config, drive.clone(), &options(), &NoopReporter).unwrap();
    let summary = run_plan(&config, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (0, 1));
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"SOURCE");
}

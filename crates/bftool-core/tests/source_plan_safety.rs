//! Source identity, read-only planning, file manifests and public execution safety.
//! Every case uses synthetic temporary directories, never a mounted backup volume.
use bftool_core::config::Config;
use bftool_core::engine::drive::DriveInfo;
use bftool_core::engine::{manifest, paths};
use bftool_core::pipeline::archive::{plan_on_drive, run_plan, ArchivePlan, Options, PlanAction};
use bftool_core::reporter::{LogLevel, NoopReporter, ProgressHandle, Reporter};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

fn world() -> (tempfile::TempDir, Config, DriveInfo) {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Config {
        ready_root: temp.path().join("ready"),
        archived_root: temp.path().join("archived"),
        system_root: temp.path().join("system"),
        reserve_gb: 0,
        stable_minutes: 0,
        min_drive_gb: 0,
        test_archives: false,
        ..Config::default()
    };
    for root in [&cfg.ready_root, &cfg.archived_root, &cfg.system_root] {
        fs::create_dir_all(root).unwrap();
    }
    let root = temp.path().join("synthetic-drive");
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
    (temp, cfg, drive)
}

fn options() -> Options {
    Options {
        drive_letter_override: Some("T".into()),
        ..Options::default()
    }
}

fn dest_name(plan: &ArchivePlan, index: usize) -> &str {
    match &plan.items[index].action {
        PlanAction::Archive { dest_name } | PlanAction::RenameAndArchive { dest_name } => dest_name,
        action => panic!("expected archive, found {action:?}"),
    }
}

fn tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut entries = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            entries.insert(
                entry.path().strip_prefix(root).unwrap().to_owned(),
                fs::read(entry.path()).unwrap(),
            );
        } else if entry.file_type().is_dir() {
            entries.insert(
                entry.path().strip_prefix(root).unwrap().to_owned(),
                Vec::new(),
            );
        }
    }
    entries
}

#[test]
fn specified_source_is_executed_and_ready_namesake_is_untouched() {
    let (temp, cfg, drive) = world();
    let requested_root = temp.path().join("requested");
    let requested = requested_root.join("001proj");
    let namesake = cfg.ready_root.join("001proj");
    fs::create_dir_all(&requested).unwrap();
    fs::create_dir_all(&namesake).unwrap();
    fs::write(requested.join("a.txt"), b"REQUESTED").unwrap();
    fs::write(namesake.join("a.txt"), b"WRONG").unwrap();
    let opts = Options {
        source_override: Some(requested_root.clone()),
        ..options()
    };
    let plan = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    assert_eq!(
        plan.items[0].source_root,
        requested_root.canonicalize().unwrap()
    );
    assert_eq!(plan.items[0].source_path, requested.canonicalize().unwrap());
    assert_eq!(plan.items[0].source_relative, Path::new("001proj"));
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (1, 0));
    assert_eq!(
        fs::read(
            paths::drive_projects_dir(&drive.root)
                .join(dest_name(&plan, 0))
                .join("a.txt")
        )
        .unwrap(),
        b"REQUESTED"
    );
    assert_eq!(fs::read(namesake.join("a.txt")).unwrap(), b"WRONG");
    assert!(!requested.exists());
    assert_eq!(
        fs::read(cfg.archived_root.join("001proj/a.txt")).unwrap(),
        b"REQUESTED"
    );
}

#[test]
fn recursive_namesakes_keep_distinct_source_and_incremental_identities() {
    let (_temp, cfg, drive) = world();
    for (dir, bytes) in [("left", b"AAA"), ("right", b"BBB")] {
        fs::create_dir_all(cfg.ready_root.join(dir)).unwrap();
        fs::write(cfg.ready_root.join(dir).join("a.zip"), bytes).unwrap();
    }
    let opts = Options {
        incremental: true,
        retain_source: true,
        include_ext: Some(vec!["zip".into()]),
        include_subfolder_projects: false,
        ext_recursive: true,
        ..options()
    };
    let plan = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    assert_eq!(plan.items.len(), 2);
    assert_ne!(plan.items[0].source_relative, plan.items[1].source_relative);
    assert_ne!(dest_name(&plan, 0), dest_name(&plan, 1));
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (2, 0));
    for (index, item) in plan.items.iter().enumerate() {
        assert_eq!(
            fs::read(
                paths::drive_projects_dir(&drive.root)
                    .join(dest_name(&plan, index))
                    .join("a.zip")
            )
            .unwrap(),
            fs::read(&item.source_path).unwrap()
        );
    }
    let index_files = tree(&paths::system_incremental_dir(&cfg.system_root));
    let records: Vec<serde_json::Value> = index_files
        .iter()
        .filter(|(path, _)| path.extension().is_some_and(|ext| ext == "jsonl"))
        .flat_map(|(_, bytes)| {
            std::str::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
        })
        .collect();
    let identities: std::collections::BTreeSet<_> = records
        .iter()
        .map(|record| record["rel_path"].as_str().unwrap())
        .collect();
    assert_eq!(
        identities,
        ["left/a.zip", "right/a.zip"].into_iter().collect()
    );
    let second = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    assert!(second
        .items
        .iter()
        .all(|item| matches!(item.action, PlanAction::Skip(_))));
    fs::write(cfg.ready_root.join("left/a.zip"), b"CCC").unwrap();
    let third = plan_on_drive(&cfg, drive, &opts, &NoopReporter).unwrap();
    assert_eq!(
        third
            .items
            .iter()
            .filter(|item| matches!(
                item.action,
                PlanAction::Archive { .. } | PlanAction::RenameAndArchive { .. }
            ))
            .count(),
        1
    );
}

#[test]
fn recursive_namesakes_move_each_verified_file_to_unique_archive_paths() {
    let (_temp, cfg, drive) = world();
    for (folder, content) in [("one", b"AAA"), ("two", b"BBB"), ("three", b"CCC")] {
        fs::create_dir_all(cfg.ready_root.join(folder)).unwrap();
        fs::write(cfg.ready_root.join(folder).join("same.zip"), content).unwrap();
    }
    let opts = Options {
        include_ext: Some(vec!["zip".into()]),
        ext_recursive: true,
        include_subfolder_projects: false,
        ..options()
    };
    let plan = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (3, 0));
    let archived: std::collections::BTreeSet<Vec<u8>> = fs::read_dir(&cfg.archived_root)
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
        .collect();
    assert_eq!(
        archived,
        [b"AAA".to_vec(), b"BBB".to_vec(), b"CCC".to_vec()]
            .into_iter()
            .collect()
    );
    for item in plan.items {
        assert!(!item.source_path.exists());
    }
}

#[test]
fn single_file_manifest_matches_destination_and_archive_commits() {
    let (_temp, cfg, drive) = world();
    let source = cfg.ready_root.join("a.zip");
    fs::write(&source, b"single-file-payload").unwrap();
    let source_manifest = manifest::build(
        &source,
        manifest::ManifestOpts { no_hash: false },
        &NoopReporter,
    )
    .unwrap();
    assert_eq!(source_manifest.entries[0].rel, "a.zip");
    let opts = Options {
        include_ext: Some(vec!["zip".into()]),
        include_subfolder_projects: false,
        ..options()
    };
    let plan = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (1, 0));
    let destination = paths::drive_projects_dir(&drive.root).join(dest_name(&plan, 0));
    let destination_manifest = manifest::build(
        &destination,
        manifest::ManifestOpts { no_hash: false },
        &NoopReporter,
    )
    .unwrap();
    assert!(manifest::diff(&source_manifest, &destination_manifest, true).ok());
    assert!(!source.exists());
    assert_eq!(
        fs::read(cfg.archived_root.join("a.zip")).unwrap(),
        b"single-file-payload"
    );
    assert!(paths::drive_manifest_dir(&drive.root)
        .join(format!("{}.sha256.csv", dest_name(&plan, 0)))
        .is_file());
}

#[cfg(target_os = "linux")]
#[test]
fn manifest_write_refuses_linked_parent_before_creating_any_outside_directory() {
    use std::os::unix::fs::symlink;
    let (temp, cfg, _drive) = world();
    let source = cfg.ready_root.join("a.zip");
    fs::write(&source, b"DATA").unwrap();
    let snapshot = manifest::build(
        &source,
        manifest::ManifestOpts { no_hash: false },
        &NoopReporter,
    )
    .unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep.txt"), b"KEEP").unwrap();
    symlink(&outside, temp.path().join("linked")).unwrap();
    let before = tree(&outside);
    assert!(snapshot
        .write_csv(&temp.path().join("linked/new-directory/a.sha256.csv"))
        .is_err());
    assert_eq!(tree(&outside), before);
    assert!(!outside.join("new-directory").exists());
}

#[test]
fn modified_public_plan_is_denied_by_core_and_service_h7() {
    let (_temp, mut cfg, drive) = world();
    let source = cfg.ready_root.join("001proj");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), b"SAFE").unwrap();
    let mut plan = plan_on_drive(
        &cfg,
        drive.clone(),
        &Options {
            incremental: true,
            retain_source: true,
            ..options()
        },
        &NoopReporter,
    )
    .unwrap();
    cfg.test_archives = true; // H6 passes, so only H7 can deny this mutated plan.
    plan.opts.no_hash = true;
    let before = tree(&cfg.system_root);
    for run in [run_plan, bftool_core::service::run_archive_plan] {
        let error = run(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap_err();
        assert!(format!("{error:#}").contains("SHA256"));
    }
    assert_eq!(tree(&cfg.system_root), before);
    assert!(!paths::drive_projects_dir(&drive.root).exists());
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"SAFE");
}

#[test]
fn seed_planning_without_payload_is_read_only_and_cannot_skip() {
    let (_temp, cfg, drive) = world();
    fs::create_dir_all(cfg.ready_root.join("001proj")).unwrap();
    fs::write(cfg.ready_root.join("001proj/a.txt"), b"NEW").unwrap();
    fs::write(paths::system_global_catalog(&cfg.system_root), "文件夹名,备份盘名,备份时间,编号,盘内路径,文件数,大小GB,校验方式,校验清单\n001proj,备份1,t,001,项目\\001proj,1,0,SHA256-OK,missing.csv\n").unwrap();
    let before = tree(&cfg.system_root);
    let opts = Options {
        incremental: true,
        retain_source: true,
        seed_from_global_catalog: true,
        dry_run: true,
        ..options()
    };
    let plan = plan_on_drive(&cfg, drive, &opts, &NoopReporter).unwrap();
    assert!(matches!(
        plan.items[0].action,
        PlanAction::Archive { .. } | PlanAction::RenameAndArchive { .. }
    ));
    assert_eq!(
        run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter)
            .unwrap()
            .handled,
        1
    );
    assert_eq!(tree(&cfg.system_root), before);
    assert!(!paths::system_incremental_dir(&cfg.system_root).exists());
    assert!(!paths::drive_projects_dir(&plan.drive.root).exists());
    assert_eq!(
        fs::read(cfg.ready_root.join("001proj/a.txt")).unwrap(),
        b"NEW"
    );
}

#[test]
fn metadata_only_planning_does_not_refresh_committed_index() {
    let (_temp, cfg, drive) = world();
    let file = cfg.ready_root.join("001proj/a.txt");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, b"SAME").unwrap();
    let opts = Options {
        incremental: true,
        retain_source: true,
        ..options()
    };
    let first = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    assert_eq!(
        run_plan(&cfg, &first, &AtomicBool::new(false), &NoopReporter)
            .unwrap()
            .handled,
        1
    );
    fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100)),
        )
        .unwrap();
    let before = tree(&cfg.system_root);
    let plan = plan_on_drive(&cfg, drive, &opts, &NoopReporter).unwrap();
    assert!(matches!(plan.items[0].action, PlanAction::Skip(_)));
    assert_eq!(tree(&cfg.system_root), before);
}

#[test]
fn unchanged_preview_is_revalidated_when_source_changes_before_execute() {
    let (_temp, cfg, drive) = world();
    let file = cfg.ready_root.join("001proj/a.txt");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, b"AAA").unwrap();
    let opts = Options {
        incremental: true,
        retain_source: true,
        ..options()
    };
    let first = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    assert_eq!(
        run_plan(&cfg, &first, &AtomicBool::new(false), &NoopReporter)
            .unwrap()
            .handled,
        1
    );
    let unchanged = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    assert!(matches!(unchanged.items[0].action, PlanAction::Skip(_)));
    fs::write(&file, b"BBB").unwrap();
    let summary = run_plan(&cfg, &unchanged, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!((summary.handled, summary.failed), (1, 0));
    let payloads: Vec<Vec<u8>> = fs::read_dir(paths::drive_projects_dir(&drive.root))
        .unwrap()
        .map(|entry| fs::read(entry.unwrap().path().join("a.txt")).unwrap())
        .collect();
    assert!(payloads.contains(&b"AAA".to_vec()));
    assert!(payloads.contains(&b"BBB".to_vec()));
}

#[test]
fn mismatched_source_path_and_relative_identity_cannot_execute() {
    let (temp, cfg, drive) = world();
    let requested_root = temp.path().join("requested");
    let requested = requested_root.join("001proj");
    let namesake = cfg.ready_root.join("001proj");
    for path in [&requested, &namesake] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(requested.join("a.txt"), b"REQUESTED").unwrap();
    fs::write(namesake.join("a.txt"), b"WRONG").unwrap();
    let mut plan = plan_on_drive(
        &cfg,
        drive.clone(),
        &Options {
            source_override: Some(requested_root),
            ..options()
        },
        &NoopReporter,
    )
    .unwrap();
    plan.items[0].source_path = namesake.canonicalize().unwrap();
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!(summary.handled, 0);
    assert!(!paths::drive_projects_dir(&drive.root).exists());
    assert!(requested.is_dir());
    assert!(namesake.is_dir());
}

#[cfg(unix)]
#[test]
fn source_link_replacement_after_preview_invalidates_canonical_binding() {
    use std::os::unix::fs::symlink;
    let (temp, cfg, drive) = world();
    let source = cfg.ready_root.join("001proj");
    let saved = cfg.ready_root.join("001proj-original");
    let outside = temp.path().join("outside-source");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(source.join("a.txt"), b"REQUESTED").unwrap();
    fs::write(outside.join("a.txt"), b"OTHER").unwrap();
    let plan = plan_on_drive(&cfg, drive.clone(), &options(), &NoopReporter).unwrap();
    fs::rename(&source, &saved).unwrap();
    symlink(&outside, &source).unwrap();
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
    assert_eq!(summary.handled, 0);
    assert!(!paths::drive_projects_dir(&drive.root).exists());
    assert_eq!(fs::read(saved.join("a.txt")).unwrap(), b"REQUESTED");
    assert_eq!(fs::read(outside.join("a.txt")).unwrap(), b"OTHER");
    assert!(fs::symlink_metadata(source)
        .unwrap()
        .file_type()
        .is_symlink());
}

struct CorruptAfterCopy {
    destination: PathBuf,
    fired: AtomicBool,
}
impl Reporter for CorruptAfterCopy {
    fn log(&self, _level: LogLevel, message: &str) {
        if message == "复制完成，开始校验…" && !self.fired.swap(true, Ordering::SeqCst) {
            fs::write(&self.destination, b"BAD!").unwrap();
        }
    }
    fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle> {
        NoopReporter.progress_bytes(label, total)
    }
}

#[test]
fn sha256_verification_failure_counts_failed_without_commit_or_move() {
    let (_temp, cfg, drive) = world();
    let source = cfg.ready_root.join("001proj");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("a.txt"), b"GOOD").unwrap();
    let plan = plan_on_drive(&cfg, drive.clone(), &options(), &NoopReporter).unwrap();
    let reporter = CorruptAfterCopy {
        destination: paths::drive_projects_dir(&drive.root)
            .join(dest_name(&plan, 0))
            .join("a.txt"),
        fired: AtomicBool::new(false),
    };
    let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &reporter).unwrap();
    assert!(
        reporter.fired.load(Ordering::SeqCst),
        "fault must actually fire after copy"
    );
    assert_eq!((summary.handled, summary.failed), (0, 1));
    assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"GOOD");
    assert_eq!(fs::read(&reporter.destination).unwrap(), b"BAD!");
    assert!(!paths::drive_quarantine_dir(&drive.root)
        .join("001proj/a.txt")
        .exists());
    assert!(!paths::system_global_catalog(&cfg.system_root).is_file());
    assert!(!cfg.archived_root.join("001proj").exists());
}

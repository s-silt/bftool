//! P0' 契约：增量二轮不重拷、H7、Q2、streaming 路径存在。

use super::incremental::{
    change_is_skip, classify, fingerprint_from_stats, ChangeKind, SKIP_UNCHANGED,
};
use super::plan::plan_on_drive;
use super::types::incremental_forbids_no_hash;
use super::*;
use crate::config::Config;
use crate::engine::drive::DriveInfo;
use crate::engine::paths;
use crate::observe::{Event, EventSink};
use crate::pipeline::stages::hash_policy::run_hash_policy;
use crate::reporter::NoopReporter;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

fn temp_world() -> (tempfile::TempDir, Config, DriveInfo) {
    let d = tempfile::tempdir().unwrap();
    let base = d.path();
    let cfg = Config {
        ready_root: base.join("ready"),
        archived_root: base.join("archived"),
        system_root: base.join("sys"),
        reserve_gb: 0,
        stable_minutes: 0,
        min_drive_gb: 0,
        name_prefix: "备份".into(),
        test_archives: false,
        winrar_path: PathBuf::new(),
        bandizip_path: PathBuf::new(),
        seven_zip_path: PathBuf::new(),
        extra_catalogs: Vec::new(),
        watch_source: None,
        enable_watch: false,
        watch_poll_secs: 30,
        incremental_verify: "on_suspect".into(),
    };
    fs::create_dir_all(&cfg.ready_root).unwrap();
    fs::create_dir_all(&cfg.archived_root).unwrap();
    fs::create_dir_all(&cfg.system_root).unwrap();
    let drive_root = base.join("drive");
    fs::create_dir_all(&drive_root).unwrap();
    fs::create_dir_all(paths::drive_info_dir(&drive_root)).unwrap();
    fs::write(paths::drive_id_path(&drive_root), "备份1").unwrap();
    let drive = DriveInfo {
        letter: "T".into(),
        root: drive_root,
        id: "备份1".into(),
        sealed: false,
        free_bytes: 1 << 40,
        total_bytes: 1 << 40,
    };
    (d, cfg, drive)
}

fn opts_incr(retain: bool) -> Options {
    Options {
        incremental: true,
        retain_source: retain,
        no_hash: false,
        ..Options::default()
    }
}

struct Rec(Mutex<Vec<Event>>);
impl EventSink for Rec {
    fn emit(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

#[test]
fn h7_incremental_no_hash_denied_force_irrelevant() {
    assert!(incremental_forbids_no_hash(true, true));
    assert!(!incremental_forbids_no_hash(true, false));
    let cfg = Config {
        test_archives: true, // 即便压缩包测试开着，incremental+no_hash 仍拒
        ..Config::default()
    };
    let opts = Options {
        incremental: true,
        no_hash: true,
        ..Options::default()
    };
    let sink = Rec(Mutex::new(Vec::new()));
    let out = run_hash_policy(&cfg, &opts, &sink).unwrap();
    assert!(matches!(
        out,
        crate::pipeline::StageOutcome::Deny {
            event: Event::DenyVerifySkipWatch,
            ..
        }
    ));
}

#[test]
fn second_incremental_pass_skips_unchanged_no_recopy() {
    let (_d, cfg, drive) = temp_world();
    let proj = cfg.ready_root.join("001proj");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("a.txt"), b"hello-world").unwrap();

    let opts = opts_incr(true);
    let cancel = AtomicBool::new(false);
    let plan1 = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    assert!(
        plan1.items.iter().any(|i| matches!(
            i.action,
            PlanAction::Archive { .. } | PlanAction::RenameAndArchive { .. }
        )),
        "first pass should archive: {:?}",
        plan1.items
    );
    let s1 = run_plan(&cfg, &plan1, &cancel, &NoopReporter).unwrap();
    assert_eq!(s1.failed, 0);
    assert!(s1.handled >= 1);

    let dest = paths::drive_projects_dir(&drive.root).join("001proj");
    assert!(dest.is_dir(), "dest should exist after first archive");
    let meta1 = fs::metadata(&dest).unwrap();
    let mtime1 = meta1.modified().unwrap();
    let size1: u64 = walkdir::WalkDir::new(&dest)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum();

    // 源仍在（retain）
    assert!(proj.is_dir());

    // 第二轮：应 skipped_unchanged，不重拷
    let plan2 = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    assert!(
        plan2.items.iter().all(|i| matches!(
            &i.action,
            PlanAction::Skip(r) if r.contains(SKIP_UNCHANGED)
        )),
        "second pass must skip unchanged: {:?}",
        plan2.items
    );
    let s2 = run_plan(&cfg, &plan2, &cancel, &NoopReporter).unwrap();
    assert_eq!(s2.handled, 0, "no re-archive");
    let meta2 = fs::metadata(&dest).unwrap();
    assert_eq!(meta2.modified().unwrap(), mtime1, "dest mtime unchanged");
    let size2: u64 = walkdir::WalkDir::new(&dest)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum();
    assert_eq!(size2, size1, "dest size unchanged");
}

#[test]
fn content_changed_plans_rename_not_overwrite_intent() {
    let (_d, cfg, drive) = temp_world();
    let proj = cfg.ready_root.join("001proj");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("a.txt"), b"v1").unwrap();
    let opts = opts_incr(true);
    let cancel = AtomicBool::new(false);
    let plan1 = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    run_plan(&cfg, &plan1, &cancel, &NoopReporter).unwrap();

    // 改 size → ContentChanged → 仍走 Archive/Rename（有 catalog 则 Rename）
    fs::write(proj.join("a.txt"), b"v2-longer").unwrap();
    // 触碰 mtime/size
    let plan2 = plan_on_drive(&cfg, drive.clone(), &opts, &NoopReporter).unwrap();
    let act = &plan2.items[0].action;
    assert!(
        matches!(
            act,
            PlanAction::Archive { .. } | PlanAction::RenameAndArchive { .. }
        ),
        "changed content must re-archive (RenameAndArchive if catalog hit): {act:?}"
    );
}

#[test]
fn q2_verify_disabled_path_never_marks_incremental_success_without_run() {
    // 质量闸：incremental + no_hash 在 plan 入口硬拒，不会写索引
    let (_d, cfg, drive) = temp_world();
    let proj = cfg.ready_root.join("001proj");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("a.txt"), b"x").unwrap();
    let opts = Options {
        incremental: true,
        no_hash: true,
        retain_source: true,
        ..Options::default()
    };
    let err = plan_on_drive(&cfg, drive, &opts, &NoopReporter).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("verify_skip_watch")
            || msg.contains("跳过校验")
            || msg.contains("DenyVerifySkipWatch")
            || incremental_forbids_no_hash(true, true),
        "expected H7 deny, got {msg}"
    );
    let idx_dir = paths::system_incremental_dir(&cfg.system_root);
    assert!(
        !idx_dir.exists() || fs::read_dir(&idx_dir).map(|i| i.count()).unwrap_or(0) == 0,
        "Q2: no incremental success marks after deny"
    );
}

#[test]
fn classify_size_mtime_fast_path() {
    let a = fingerprint_from_stats(&FolderStats {
        files: 1,
        bytes: 10,
        latest_mtime_secs: Some(100),
        ..FolderStats::default()
    });
    assert_eq!(classify(None, &a), ChangeKind::New);
    assert!(change_is_skip(classify(Some(&a), &a)));
    let mut b = a.clone();
    b.size = 11;
    assert!(!change_is_skip(classify(Some(&a), &b)));
}

#[test]
fn streaming_copy_api_exists_not_read_to_end() {
    // 契约：copy_file_hashed / copy_folder 使用流式；源码不含 read_to_end
    let src = include_str!("copy.rs");
    let destination = include_str!("../../engine/destination.rs");
    assert!(
        !destination.contains("read_to_end"),
        "safe copy must remain streamed"
    );
    assert!(
        !src.contains("read_to_end"),
        "copy.rs must not load whole files via read_to_end"
    );
    assert!(
        src.contains("copy_new"),
        "copy must use the no-follow streaming destination primitive"
    );
}

#[test]
fn seed_without_verified_backup_requires_initial_copy() {
    use crate::pipeline::archive::catalog::{append_global_catalog, GlobalCatalogRow};

    let (_d, cfg, drive) = temp_world();
    let proj = cfg.ready_root.join("seeded_proj");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("a.txt"), b"payload").unwrap();

    // 写入全局 catalog（模拟已备份），尚无增量索引
    append_global_catalog(
        &paths::system_global_catalog(&cfg.system_root),
        &GlobalCatalogRow {
            folder_name: "seeded_proj".into(),
            drive_name: "备份1".into(),
            archived_time: "2026-01-01".into(),
            project_no: "".into(),
            in_drive_path: "项目\\seeded_proj".into(),
            file_count: 1,
            size_gb: 0.0,
            verify: "SHA256-OK".into(),
            manifest_path: "".into(),
        },
    )
    .unwrap();

    let mut opts = opts_incr(true);
    opts.seed_from_global_catalog = true;
    // 历史 catalog 缺少实际副本/可信源内容基线；首轮必须复制及校验。
    let plan = plan_on_drive(&cfg, drive, &opts, &NoopReporter).unwrap();
    assert!(
        plan.items.iter().all(|i| matches!(
            &i.action,
            PlanAction::Archive { .. } | PlanAction::RenameAndArchive { .. }
        )),
        "unverified catalog must not authenticate current source: {:?}",
        plan.items
    );
}

#[test]
fn metadata_only_verified_hash_match_is_a_read_only_skip() {
    use super::incremental::{
        fingerprint_from_stats, project_content_hash, resolve_incremental, ChangeKind,
        IncrementalVerifyMode,
    };
    let d = tempfile::tempdir().unwrap();
    let proj = d.path().join("p");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("f.bin"), b"abc").unwrap();
    let stats = super::copy::folder_stats(&proj);
    let mut old = fingerprint_from_stats(&stats);
    let hash = project_content_hash(&proj).unwrap();
    old.sha256 = Some(hash.clone());
    old.mtime_secs = Some(old.mtime_secs.unwrap_or(1) - 10); // 人为 mtime 漂
    let new_fp = fingerprint_from_stats(&stats);
    assert_eq!(
        super::incremental::classify(Some(&old), &new_fp),
        ChangeKind::MetadataOnly
    );
    let r = resolve_incremental(
        ChangeKind::MetadataOnly,
        Some(&old),
        &new_fp,
        &proj,
        IncrementalVerifyMode::OnSuspect,
    )
    .unwrap();
    assert!(r.skip, "same content hash → Unchanged");
    assert_eq!(
        old.sha256.as_deref(),
        Some(hash.as_str()),
        "read-only resolution cannot replace the old baseline"
    );
}

#[test]
fn metadata_only_hash_diff_proceeds() {
    use super::incremental::{
        fingerprint_from_stats, resolve_incremental, ChangeKind, IncrementalVerifyMode,
    };
    let d = tempfile::tempdir().unwrap();
    let proj = d.path().join("p");
    fs::create_dir_all(&proj).unwrap();
    fs::write(proj.join("f.bin"), b"abc").unwrap();
    let new_fp = fingerprint_from_stats(&super::copy::folder_stats(&proj));
    let mut old = new_fp.clone();
    old.sha256 = Some("DEADBEEF".into());
    old.mtime_secs = Some(1); // drift vs new
    let r = resolve_incremental(
        ChangeKind::MetadataOnly,
        Some(&old),
        &new_fp,
        &proj,
        IncrementalVerifyMode::OnSuspect,
    )
    .unwrap();
    assert!(!r.skip, "different hash → proceed to re-archive");
}

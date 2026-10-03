//! 主归档流程（P1：pipeline/archive 切片）。
//!
//! - [`types`] 选项/计划类型
//! - [`plan`] 只读计划
//! - [`runner`] run / run_plan / ArchiveLock
//! - [`item`] 单项目处理
//! - [`copy`] 复制与统计
//! - [`catalog`] 索引 CSV
//! - [`lock`] 事务残留检查

mod types;
pub use types::{
    incremental_forbids_no_hash, verify_disabled, ArchivePlan, ArchiveSummary, FolderStats,
    Options, PlanAction, PlanItem,
};

mod catalog;
mod copy;
mod incremental;
pub(crate) mod item;
mod lock;
mod plan;
mod runner;

pub use incremental::{ChangeKind, Fingerprint, IncrementalVerifyMode, SKIP_UNCHANGED};
pub use plan::{plan, plan_on_drive};
pub(crate) use runner::ArchiveLock;
pub use runner::{run, run_plan};

#[cfg(test)]
#[path = "p0_contract_tests.rs"]
mod p0_contract_tests;

#[cfg(test)]
mod tests {
    use super::catalog::{
        append_drive_catalog, append_global_catalog, catalog_has_project, DriveCatalogRow,
        GlobalCatalogRow,
    };
    use super::copy::{copy_folder, discover_projects};
    use super::item::{handle_one, HandleOutcome};
    use super::lock::{check_pending_txn, clear_marker, global_has_folder, note_manual};
    use super::plan::{decide, plan_items};
    use super::runner::{reconcile_item_drive, ItemDriveCheck};
    use super::types::VerifyStatus;
    use super::*;
    use crate::config::Config;
    use crate::engine::drive::{self, DriveInfo};
    use crate::engine::{manifest, paths, txn};
    use crate::reporter::NoopReporter;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    // AR-12 测试分工说明:本模块的 handle_one / run_plan 集成测试都跑在 test_archives=false 下,
    // 是**有意**的——它们聚焦核心归档流程(复制/SHA256 校验/源复核/事务提交/移源/断点续传),
    // 不想被外部压缩包测试器(WinRAR/Bandizip/7-Zip 是否安装)的环境依赖污染、影响可重复性。
    // 「archive test 通路」(detect / test_folder / 覆盖率 / 坏包隔离)有 archive_test.rs 自己的
    // 单元测试覆盖(见 engine::archive_test 的 tests 模块);两边职责不重叠。

    /// 搭一个临时"世界":ready/archived/sys + 一块假备份盘,供 handle_one 集成测试复用。
    fn temp_world() -> (tempfile::TempDir, Config, DriveInfo) {
        let d = tempfile::tempdir().unwrap();
        let base = d.path();
        let mut cfg = Config {
            ready_root: base.join("ready"),
            archived_root: base.join("archived"),
            system_root: base.join("sys"),
            reserve_gb: 0,
            stable_minutes: 0, // 不卡稳定性
            min_drive_gb: 0,
            name_prefix: "备份".into(),
            test_archives: false,
            winrar_path: std::path::PathBuf::new(),
            bandizip_path: std::path::PathBuf::new(),
            seven_zip_path: std::path::PathBuf::new(),
            extra_catalogs: Vec::new(),
            watch_source: None,
            enable_watch: false,
            watch_poll_secs: 30,
            incremental_verify: "on_suspect".into(),
        };
        fs::create_dir_all(&cfg.ready_root).unwrap();
        fs::create_dir_all(&cfg.archived_root).unwrap();
        fs::create_dir_all(&cfg.system_root).unwrap();
        // Private item entry points receive the canonical identity frozen by public planning.
        // On Windows this includes the verbatim prefix returned by canonicalize().
        cfg.ready_root = cfg.ready_root.canonicalize().unwrap();
        let drive_root = base.join("drive");
        fs::create_dir_all(&drive_root).unwrap();
        // 让它看起来是一块已初始化的备份盘:run_plan 重验会查盘上 id 文件是否在线。
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

    fn test_opts() -> Options {
        Options {
            dry_run: false,
            no_hash: false,
            limit: 0,
            drive_letter_override: None,
            no_test_archives: false,
            source_override: None,
            incremental: false,
            retain_source: false,
            include_ext: None,
            file_globs: Vec::new(),
            ext_recursive: false,
            include_subfolder_projects: true,
            seed_from_global_catalog: false,
            incremental_verify: Default::default(),
        }
    }

    fn decide0(cfg: &Config, drive: &DriveInfo, p: &Path) -> PlanItem {
        decide(cfg, drive, p, &test_opts(), None, &cfg.ready_root)
    }

    struct RecordingReporter(std::sync::Mutex<Vec<String>>);
    impl crate::reporter::Reporter for RecordingReporter {
        fn log(&self, level: crate::reporter::LogLevel, msg: &str) {
            self.0.lock().unwrap().push(format!("{:?} {}", level, msg));
        }
        fn progress_bytes(&self, _l: &str, _t: u64) -> Box<dyn crate::reporter::ProgressHandle> {
            struct P;
            impl crate::reporter::ProgressHandle for P {
                fn inc(&mut self, _: u64) {}
                fn finish(&mut self) {}
            }
            Box::new(P)
        }
    }

    // ── L-003: 空源(0 真实文件)不被记成功、不移源 ──
    #[test]
    fn handle_one_empty_source_does_not_move() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001empty");
        fs::create_dir_all(&proj).unwrap();
        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        assert!(matches!(outcome, HandleOutcome::Skipped), "空源应 Skipped");
        assert!(proj.is_dir(), "空源不应被移走(应仍在 待备份)");
    }

    #[test]
    fn handle_one_rejects_source_file_named_bftool_part() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("cover.jpg"), b"cover").unwrap();
        fs::write(proj.join("session.bftool-part"), b"real user file").unwrap();

        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();

        assert!(matches!(outcome, HandleOutcome::Failed));
        assert!(proj.exists(), "source must remain for user action");
        assert!(
            !paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .exists(),
            ".bftool-part source files must not be silently omitted"
        );
        assert_eq!(fs::read(proj.join("cover.jpg")).unwrap(), b"cover");
        assert_eq!(
            fs::read(proj.join("session.bftool-part")).unwrap(),
            b"real user file"
        );
        assert!(!paths::system_global_catalog(&cfg.system_root).is_file());
    }

    // ── happy-path:正常项目完整走通 复制→校验→提交→移源(覆盖 L-001 fsync 提交链路) ──
    #[test]
    fn handle_one_happy_path_archives_and_moves() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        fs::write(proj.join("b.bin"), b"world!!").unwrap();
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        let outcome = handle_one(&cfg, &rep, &drive, &proj, &test_opts(), None, None).unwrap();
        let log = rep.0.lock().unwrap().join("\n");
        assert!(
            matches!(outcome, HandleOutcome::Done(_)),
            "正常项目应 Done;日志:\n{}",
            log
        );
        assert!(!proj.exists(), "源应已移到 已备份");
        assert!(cfg.archived_root.join("001proj").is_dir(), "源应在 已备份");
        assert!(
            paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .is_dir(),
            "目标盘应有项目副本"
        );
        assert!(
            paths::drive_manifest_dir(&drive.root)
                .join("001proj.sha256.csv")
                .is_file(),
            "应写出校验清单"
        );
    }

    #[cfg(windows)]
    #[test]
    fn handle_one_rejects_raw_windows_source_identity_before_commit() {
        let (d, cfg, drive) = temp_world();
        let raw_source = d.path().join("ready/001raw");
        fs::create_dir(&raw_source).unwrap();
        fs::write(raw_source.join("a.txt"), b"RAW SOURCE").unwrap();
        assert_ne!(raw_source.canonicalize().unwrap(), raw_source);
        let result = handle_one(
            &cfg,
            &NoopReporter,
            &drive,
            &raw_source,
            &test_opts(),
            None,
            None,
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a raw Windows path must not pass the frozen identity guard"),
        };
        assert!(format!("{error:#}").contains("源路径在执行期间改变或含未授权链接"));
        assert_eq!(fs::read(raw_source.join("a.txt")).unwrap(), b"RAW SOURCE");
        assert_eq!(
            fs::read(paths::drive_projects_dir(&drive.root).join("001raw/a.txt")).unwrap(),
            b"RAW SOURCE"
        );
        assert!(!cfg.archived_root.join("001raw").exists());
        assert!(!paths::system_pending_txn(&cfg.system_root).exists());
        assert!(!paths::system_global_catalog(&cfg.system_root).exists());
        assert!(!paths::drive_catalog_path(&drive.root).exists());
        assert!(!paths::drive_manifest_dir(&drive.root)
            .join("001raw.sha256.csv")
            .exists());
    }

    #[cfg(windows)]
    #[test]
    fn public_plan_canonicalizes_raw_windows_source_before_execution() {
        let (d, mut cfg, drive) = temp_world();
        cfg.ready_root = d.path().join("ready");
        let raw_source = cfg.ready_root.join("001raw");
        fs::create_dir(&raw_source).unwrap();
        fs::write(raw_source.join("a.txt"), b"PLANNED SOURCE").unwrap();
        let plan = plan_on_drive(&cfg, drive.clone(), &test_opts(), &NoopReporter).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(
            plan.items[0].source_path,
            raw_source.canonicalize().unwrap()
        );
        assert_eq!(
            plan.items[0].source_root,
            cfg.ready_root.canonicalize().unwrap()
        );
        assert_ne!(plan.items[0].source_path, raw_source);
        let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
        assert_eq!((summary.handled, summary.failed), (1, 0));
        assert!(!raw_source.exists());
        assert_eq!(
            fs::read(cfg.archived_root.join("001raw/a.txt")).unwrap(),
            b"PLANNED SOURCE"
        );
        assert_eq!(
            fs::read(paths::drive_projects_dir(&drive.root).join("001raw/a.txt")).unwrap(),
            b"PLANNED SOURCE"
        );
        assert!(paths::system_global_catalog(&cfg.system_root).is_file());
        assert!(paths::drive_catalog_path(&drive.root).is_file());
        assert!(!paths::system_pending_txn(&cfg.system_root).exists());
    }

    // ── L-014: copy_folder 原子复制,内容正确且不遗留 .bftool-part ──
    #[test]
    fn copy_folder_atomic_no_part_left() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("s");
        let dst = d.path().join("t");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("sub").join("b.bin"), b"xyz").unwrap();
        let hashes = copy_folder(&src, &dst, false, &NoopReporter).unwrap();
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("sub").join("b.bin")).unwrap(), b"xyz");
        assert!(!dst.join("a.txt.bftool-part").exists(), "不应遗留 .part");
        // 强优化:复制时边读边算的源哈希应与独立 sha256_hex 逐字一致(折叠正确性)。
        assert_eq!(
            hashes.get("a.txt").map(String::as_str),
            Some(manifest::sha256_hex(&src.join("a.txt")).unwrap().as_str())
        );
        assert_eq!(
            hashes.get("sub\\b.bin").map(String::as_str),
            Some(
                manifest::sha256_hex(&src.join("sub").join("b.bin"))
                    .unwrap()
                    .as_str()
            )
        );
    }

    // 既有同名文件缺少当前复制所有权：拒绝覆盖，即使大小相同。
    #[test]
    fn copy_folder_rejects_existing_file_without_ownership() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("s");
        let dst = d.path().join("t");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(dst.join("a.txt"), b"world").unwrap();
        assert!(copy_folder(&src, &dst, false, &NoopReporter).is_err());
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"world");
        assert_eq!(fs::read(src.join("a.txt")).unwrap(), b"hello");
    }

    // ── review-r3 #3:空目录建立失败不再 .ok() 静默吞,本项目 fail-closed(返回 Err、不移源)──
    #[test]
    fn copy_folder_dir_create_failure_is_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("s");
        let dst = d.path().join("t");
        // 源含一个空目录 sub_empty(只有它,无文件,故只走目录创建分支)。
        fs::create_dir_all(src.join("sub_empty")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        // 在 dst 下预置一个与该空目录同名的**文件**,使 create_dir_all(dst/sub_empty) 失败。
        fs::write(dst.join("sub_empty"), b"x").unwrap();
        let r = copy_folder(&src, &dst, false, &NoopReporter);
        assert!(
            r.is_err(),
            "空目录建立失败应 fail-closed(不静默吞),返回 Err"
        );
    }

    // ── L-01(VulnGym 审计):符号链接/junction 不静默丢弃,复制阶段 warn 告知;不跟随(防逃逸) ──
    #[test]
    fn copy_folder_warns_on_link_not_silent() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("s");
        let dst = d.path().join("t");
        let target = d.path().join("target");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(src.join("real.txt"), b"hi").unwrap();
        fs::write(target.join("inner.txt"), b"x").unwrap();
        // 在 src 内建一个指向 target 的链接:Windows 用 junction(免管理员),Unix 用 symlink。
        let link = src.join("jlink");
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&target, &link).is_ok();
        if !made {
            eprintln!("跳过 copy_folder_warns_on_link_not_silent：本环境无法创建链接");
            return;
        }
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        copy_folder(&src, &dst, false, &rep).unwrap();
        assert!(dst.join("real.txt").is_file(), "普通文件应被复制");
        assert!(
            !dst.join("jlink").join("inner.txt").exists(),
            "不应跟随链接把链接外内容复制进来(防逃逸)"
        );
        let logs = rep.0.lock().unwrap();
        assert!(
            logs.iter()
                .any(|l| l.starts_with("Warn") && l.contains("jlink")),
            "链接应被 warn 告知而非静默跳过,实际日志:{:?}",
            logs
        );
    }

    // ── L-05(VulnGym 审计):待备份下顶层若是链接,不归档但要 warn 告知(否则整项目静默忽略) ──
    #[test]
    fn discover_projects_warns_on_top_level_link_and_excludes_it() {
        let d = tempfile::tempdir().unwrap();
        let ready = d.path().join("ready");
        let ext = d.path().join("external");
        fs::create_dir_all(ready.join("001proj")).unwrap();
        fs::create_dir_all(&ext).unwrap();
        let link = ready.join("002link");
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&ext)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&ext, &link).is_ok();
        if !made {
            eprintln!("跳过 discover_projects_warns_on_top_level_link：本环境无法创建链接");
            return;
        }
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        let projects = discover_projects(&ready, &rep).unwrap();
        assert!(
            projects.iter().any(|p| p.file_name().unwrap() == "001proj"),
            "正常项目应在列表"
        );
        assert!(
            projects.iter().all(|p| p.file_name().unwrap() != "002link"),
            "链接不应作为项目归档,实际:{:?}",
            projects
        );
        let logs = rep.0.lock().unwrap();
        assert!(
            logs.iter()
                .any(|l| l.starts_with("Warn") && l.contains("002link")),
            "顶层链接应 warn 告知,实际:{:?}",
            logs
        );
    }

    // ── L-017 修订(Phase 4 F-4):本盘索引损坏 → fail-closed Failed,不覆盖也不重复写盘 ──
    #[test]
    fn handle_one_corrupt_drive_catalog_fails_closed() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        let cat = paths::drive_catalog_path(&drive.root);
        if let Some(p) = cat.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&cat, [0xff, 0xfe, 0x00]).unwrap(); // 非 UTF-8 → 读索引必失败
        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        assert!(
            matches!(outcome, HandleOutcome::Failed),
            "索引损坏应计失败，保留源与损坏索引"
        );
        assert!(proj.is_dir(), "不应移源");
        assert!(
            !paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .exists(),
            "不应写盘"
        );
        assert_eq!(fs::read(proj.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(&cat).unwrap(), [0xff, 0xfe, 0x00]);
        assert!(!paths::system_global_catalog(&cfg.system_root).is_file());
    }

    #[test]
    fn handle_one_malformed_drive_catalog_row_fails_closed() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        let cat = paths::drive_catalog_path(&drive.root);
        if let Some(p) = cat.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(
            &cat,
            "ProjectNo,ProjectName,FileCount,TotalBytes,ArchivedUTC,VerifyStatus,Status,Notes\n\
             001,001proj\n",
        )
        .unwrap();

        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();

        assert!(matches!(outcome, HandleOutcome::Failed));
        assert!(proj.is_dir(), "malformed catalog row should fail closed");
        assert!(!paths::drive_projects_dir(&drive.root)
            .join("001proj")
            .exists());
        assert_eq!(fs::read(proj.join("a.txt")).unwrap(), b"hello");
        assert!(fs::read_to_string(&cat).unwrap().ends_with("001,001proj\n"));
        assert!(!paths::system_global_catalog(&cfg.system_root).is_file());
    }

    // ── L-018: 台账写失败不致命(note_manual 返回 () 不向上抛) ──
    #[test]
    fn note_manual_infallible_when_system_root_unwritable() {
        let (_d, mut cfg, _drive) = temp_world();
        let bogus = cfg.system_root.join("not_a_dir");
        fs::write(&bogus, b"x").unwrap();
        cfg.system_root = bogus; // system_root 指向文件 → 台账写入必失败
                                 // 关键:返回 () 且不 panic —— 写失败只 warn,不会中止整轮归档
        note_manual(&cfg, &NoopReporter, "proj", "校验失败：xxx");
    }

    // ── L-020: VerifyStatus token 稳定且单一来源 ──
    #[test]
    fn verify_status_tokens() {
        assert_eq!(VerifyStatus::from_opts(false).token(), "SHA256-OK");
        assert_eq!(VerifyStatus::from_opts(true).token(), "SIZE+COUNT");
    }

    // ── L-008: 无校验判定单一来源,core/cli 共用 verify_disabled ──
    #[test]
    fn verify_disabled_truth_table() {
        // no_hash=false → 始终有 SHA256 兜底,绝不算无校验
        assert!(!verify_disabled(false, true, false));
        assert!(!verify_disabled(false, false, true));
        // no_hash=true 且 archive test 开启 → 有压缩包测试兜底,允许
        assert!(!verify_disabled(true, true, false));
        // no_hash=true 且 test_archives=false → 无校验,禁止
        assert!(verify_disabled(true, false, false));
        // no_hash=true 且 no_test_archives=true → 无校验,禁止
        assert!(verify_disabled(true, true, true));
    }

    #[test]
    fn handle_one_revalidates_h7_at_write_boundary() {
        let (_temp, mut cfg, drive) = temp_world();
        cfg.test_archives = true;
        let source = cfg.ready_root.join("001proj");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"SAFE").unwrap();
        let opts = Options {
            incremental: true,
            no_hash: true,
            ..test_opts()
        };
        let result = handle_one(&cfg, &NoopReporter, &drive, &source, &opts, None, None);
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("H7 must reject at item write boundary"),
        };
        assert!(format!("{error:#}").contains("SHA256"));
        assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"SAFE");
        assert!(!paths::drive_projects_dir(&drive.root).exists());
        assert!(!paths::system_global_catalog(&cfg.system_root).exists());
    }

    // ── Spec D §4.1: decide() 只读裁决 Archive / Skip(无可备份) / SealAndStop(余量不足) ──
    #[test]
    fn decide_archive_skip_and_seal_and_stop() {
        let (_d, cfg, drive) = temp_world();
        // 正常项目 → Archive{dest_name}
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        let it1 = decide0(&cfg, &drive, &p1);
        assert!(
            matches!(&it1.action, PlanAction::Archive { dest_name } if dest_name == "001proj"),
            "正常项目应 Archive,得到 {:?}",
            it1.action
        );
        assert_eq!(it1.est_bytes, 5);
        // 空源 → Skip("无可备份…")
        let p2 = cfg.ready_root.join("002empty");
        fs::create_dir_all(&p2).unwrap();
        let it2 = decide0(&cfg, &drive, &p2);
        assert!(
            matches!(&it2.action, PlanAction::Skip(r) if r.contains("可备份的真实文件")),
            "空源应 Skip(无可备份的真实文件),得到 {:?}",
            it2.action
        );
        // 余量不足 → SealAndStop(reserve_gb=0,free=1 字节,项目 4KB)
        let mut tiny = drive.clone();
        tiny.free_bytes = 1;
        let p3 = cfg.ready_root.join("003big");
        fs::create_dir_all(&p3).unwrap();
        fs::write(p3.join("x.bin"), vec![0u8; 4096]).unwrap();
        let it3 = decide0(&cfg, &tiny, &p3);
        assert!(
            matches!(it3.action, PlanAction::SealAndStop(_)),
            "余量不足应 SealAndStop,得到 {:?}",
            it3.action
        );
    }

    // ── review-r2 #2:plan_items 跨项目递减剩余 —— 两个项目各自放得下但累计放不下时,
    // 须在第二个处 SealAndStop,而非因每个单独都够就全标 Archive(冻结 free_bytes 高估 bug) ──
    #[test]
    fn plan_items_seals_on_cumulative_overcommit() {
        let (_d, cfg, mut drive) = temp_world();
        let p1 = cfg.ready_root.join("001a");
        let p2 = cfg.ready_root.join("002b");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();
        fs::write(p1.join("f"), vec![0u8; 4096]).unwrap();
        fs::write(p2.join("f"), vec![0u8; 4096]).unwrap();
        // reserve_gb=0(temp_world);free 够一个 4096、不够两个 8192。
        drive.free_bytes = 4096 + 2048;
        let items = plan_items(
            &cfg,
            &drive,
            &[p1.clone(), p2.clone()],
            &test_opts(),
            &cfg.ready_root,
        );
        assert!(
            matches!(items[0].action, PlanAction::Archive { .. }),
            "首个应归档,实际:{:?}",
            items[0].action
        );
        assert!(
            matches!(items[1].action, PlanAction::SealAndStop(_)),
            "次个累计超容量应封盘,实际:{:?}",
            items[1].action
        );
    }

    // ── review-r2 R2-3:清事务标记失败不静默 —— 否则下轮重复触发恢复、提示与实际不符 ──
    #[test]
    fn clear_marker_warns_when_remove_fails() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("marker_is_dir");
        fs::create_dir(&p).unwrap(); // 目录:remove_file 必失败
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        clear_marker(&p, &rep);
        let logs = rep.0.lock().unwrap();
        assert!(
            logs.iter()
                .any(|l| l.starts_with("Warn") && l.contains("清除事务标记失败")),
            "删除失败应 warn 而非静默,实际:{:?}",
            logs
        );
    }

    #[test]
    fn clear_marker_silent_on_success() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("marker");
        fs::write(&p, "x").unwrap();
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        clear_marker(&p, &rep);
        assert!(!p.exists(), "成功应已删除标记");
        assert!(rep.0.lock().unwrap().is_empty(), "成功路径不应有日志");
    }

    // ── review-r2 R2-2:Done 携带实际归档字节(供 run_plan 用真实占用累计 consumed)──
    #[test]
    fn handle_one_done_carries_real_bytes() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap(); // 5
        fs::write(proj.join("b.bin"), b"world!!").unwrap(); // 7
        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        let n = match outcome {
            HandleOutcome::Done(n) => n,
            _ => panic!("正常项目应 Done"),
        };
        assert_eq!(n, 12, "Done 应携带实际归档字节(供 consumed 真实累计)");
    }

    // ── review-r2 R6-4:已有归档锁(另一实例在跑)时 run_plan 应拒绝、不删别人的锁 ──
    #[test]
    fn run_plan_refuses_when_archive_lock_present() {
        let (_d, cfg, drive) = temp_world();
        fs::create_dir_all(&cfg.system_root).unwrap();
        let lock = cfg.system_root.join(".bftool-archive.lock");
        fs::write(&lock, "pid=999").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let r = run_plan(&cfg, &plan, &cancel, &NoopReporter);
        assert!(r.is_err(), "已有归档锁时应拒绝运行");
        assert!(lock.is_file(), "拒绝时不应删除别人的锁");
    }

    // ── review-r2 R6-2:封盘标记写失败也应终止本轮(DriveSealed),不退化成逐项目 Err 重试 ──
    #[test]
    fn handle_one_seal_failure_still_terminates_round() {
        let (_d, cfg, mut drive) = temp_world();
        drive.free_bytes = 1; // 余量不足 → 触发封盘
                              // 把封盘标记路径占成目录 → drive::seal 的 write_synced 失败
        let sealed = paths::drive_sealed_path(&drive.root);
        fs::create_dir_all(&sealed).unwrap();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("f"), vec![0u8; 4096]).unwrap();
        let outcome = handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None);
        assert!(
            matches!(outcome, Ok(HandleOutcome::DriveSealed)),
            "封盘标记写失败也应 DriveSealed(终止本轮),而非 Err 逐项目重试"
        );
    }

    // ── review-r2 R5-2:提交段索引写失败(标记已写)→ CommitInterrupted,保留标记、中止本轮 ──
    #[test]
    fn handle_one_index_write_failure_is_commit_interrupted() {
        struct BlockGlobalCommit(PathBuf);
        impl crate::reporter::Reporter for BlockGlobalCommit {
            fn log(&self, _level: crate::reporter::LogLevel, message: &str) {
                // The naming reads must succeed first; inject at the actual copy→commit window.
                if message == "复制完成，开始校验…" {
                    fs::create_dir(&self.0).unwrap();
                }
            }
            fn progress_bytes(
                &self,
                label: &str,
                total: u64,
            ) -> Box<dyn crate::reporter::ProgressHandle> {
                NoopReporter.progress_bytes(label, total)
            }
        }
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hi").unwrap();
        let global = paths::system_global_catalog(&cfg.system_root);
        let reporter = BlockGlobalCommit(global.clone());
        let outcome = handle_one(&cfg, &reporter, &drive, &proj, &test_opts(), None, None);
        assert!(
            matches!(outcome, Ok(HandleOutcome::CommitInterrupted)),
            "actual post-copy metadata commit failure must preserve the journal"
        );
        assert_eq!(fs::read(proj.join("a.txt")).unwrap(), b"hi");
        assert!(global.is_dir(), "injected obstruction must not be removed");
        assert_eq!(
            fs::read(paths::drive_projects_dir(&drive.root).join("001proj/a.txt")).unwrap(),
            b"hi"
        );
        assert!(paths::drive_manifest_dir(&drive.root)
            .join("001proj.sha256.csv")
            .is_file());
        assert!(
            paths::system_pending_txn(&cfg.system_root).is_file(),
            "verified snapshot journal must remain available for recovery"
        );
        assert!(!cfg.archived_root.join("001proj").exists());
    }

    // ── review-r2 R2-1:预览→执行间换盘(同盘符)→ 盘内编号变了 → 按"计划已过期"中止 ──
    #[test]
    fn run_plan_aborts_when_drive_id_differs_from_plan() {
        let (_d, cfg, mut drive) = temp_world();
        // 盘上 id 文件是「备份1」(temp_world 写的),让 plan 冻结的 id 是「备份3」(模拟换过盘)。
        drive.id = "备份3".into();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hi").unwrap();
        let plan = ArchivePlan {
            drive,
            items: vec![PlanItem {
                name: "001proj".into(),
                source_root: cfg.ready_root.canonicalize().unwrap(),
                source_path: cfg
                    .ready_root
                    .join("001proj")
                    .canonicalize()
                    .unwrap_or_else(|_| cfg.ready_root.canonicalize().unwrap().join("001proj")),
                source_relative: PathBuf::from("001proj"),
                est_bytes: 2,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let summary = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap();
        assert_eq!(summary.handled, 0, "盘内编号不符应中止,不归档任何项目");
        assert!(!cfg.archived_root.join("001proj").exists(), "源不应被移动");
    }

    // ── review-r2 R2-4:catalog append 原子落盘 —— 两次追加都在、表头一次、不遗留 tmp ──
    #[test]
    fn append_drive_catalog_atomic_appends_and_no_tmp() {
        let d = tempfile::tempdir().unwrap();
        let cat = d.path().join("本盘信息").join("本盘索引记录.csv");
        let mk = |no: &str, name: &str| DriveCatalogRow {
            project_no: no.into(),
            project_name: name.into(),
            file_count: 1,
            total_bytes: 10,
            archived_utc: "2026-05-30T00:00:00Z".into(),
            verify_status: "SHA256-OK".into(),
            status: "OK".into(),
            notes: String::new(),
        };
        append_drive_catalog(&cat, &mk("001", "projA")).unwrap();
        append_drive_catalog(&cat, &mk("002", "projB")).unwrap();
        assert!(
            catalog_has_project(&cat, "projA").unwrap(),
            "首行仍在(追加非覆盖)"
        );
        assert!(catalog_has_project(&cat, "projB").unwrap(), "次行也在");
        let content = std::fs::read_to_string(&cat).unwrap();
        assert_eq!(
            content.matches("ProjectName").count(),
            1,
            "表头只一行;实际:\n{content}"
        );
        let tmp = cat.with_file_name("本盘索引记录.csv.bftool-tmp");
        assert!(!tmp.exists(), "原子写不应遗留 .bftool-tmp");
    }

    // ── review-r2 R3-1:良性 Skip(超单盘容量)即使台账写失败也应 Skipped,不升级为 Err/failed ──
    #[test]
    fn handle_one_oversize_skipped_even_if_manual_write_fails() {
        let (_d, mut cfg, mut drive) = temp_world();
        let bogus = cfg.system_root.join("not_a_dir");
        fs::write(&bogus, b"x").unwrap();
        cfg.system_root = bogus; // system_root 指向文件 → 台账写入必失败
        drive.total_bytes = 1; // 任何项目都"超单盘容量"
        let proj = cfg.ready_root.join("001big");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("f"), vec![0u8; 100]).unwrap();
        let outcome = handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None);
        assert!(
            matches!(outcome, Ok(HandleOutcome::Skipped)),
            "超容量良性 Skip 即使台账写失败也应 Skipped、不升级为 Err"
        );
    }

    // ── review-r2 R3-3:移源失败(索引已写)→ 中止本轮,后续项目不得覆盖前项的事务标记 ──
    // Windows-only:靠"持有源内文件句柄使目录无法被 rename"触发移源失败(本工具仅 Windows)。
    #[cfg(windows)]
    #[test]
    fn run_plan_rename_failure_aborts_round_preserving_marker() {
        let (_d, cfg, drive) = temp_world();
        let a = cfg.ready_root.join("001A");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("f"), b"hi").unwrap();
        // B:正常项目(若被处理会写新标记/清标记,从而覆盖 A 的标记)
        let b = cfg.ready_root.join("002B");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("f"), b"yo").unwrap();
        // 持有 A 内文件的打开句柄 → Windows 上 A 目录无法被 rename(移源)→ 移源失败。
        // 共享读句柄不挡 copy_folder/folder_stable 的读,只挡父目录的移动。
        let hold = std::fs::File::open(a.join("f")).unwrap();
        let plan =
            ArchivePlan {
                drive: drive.clone(),
                items: vec![
                    PlanItem {
                        name: "001A".into(),
                        source_root: cfg.ready_root.canonicalize().unwrap(),
                        source_path: cfg.ready_root.join("001A").canonicalize().unwrap_or_else(
                            |_| cfg.ready_root.canonicalize().unwrap().join("001A"),
                        ),
                        source_relative: PathBuf::from("001A"),
                        est_bytes: 2,
                        dest_existed_at_plan: false,
                        action: PlanAction::Archive {
                            dest_name: "001A".into(),
                        },
                    },
                    PlanItem {
                        name: "002B".into(),
                        source_root: cfg.ready_root.canonicalize().unwrap(),
                        source_path: cfg.ready_root.join("002B").canonicalize().unwrap_or_else(
                            |_| cfg.ready_root.canonicalize().unwrap().join("002B"),
                        ),
                        source_relative: PathBuf::from("002B"),
                        est_bytes: 2,
                        dest_existed_at_plan: false,
                        action: PlanAction::Archive {
                            dest_name: "002B".into(),
                        },
                    },
                ],
                opts: test_opts(),
            };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let _ = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap();
        drop(hold);
        assert!(
            paths::system_pending_txn(&cfg.system_root).is_file(),
            "A 的事务标记应保留(本轮中止,B 未处理、未覆盖标记)"
        );
        assert!(
            cfg.ready_root.join("002B").exists(),
            "B 不应被处理(本轮已中止)"
        );
        assert!(
            !paths::drive_projects_dir(&drive.root).join("002B").exists(),
            "B 不应被归档"
        );
    }

    // ── Spec D §4.1: run_plan 执行冻结目标名 → 归档 + 移源 ──
    #[test]
    fn run_plan_executes_frozen_name_and_moves_source() {
        let (_d, cfg, drive) = temp_world();
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "001proj".into(),
                source_root: cfg.ready_root.canonicalize().unwrap(),
                source_path: cfg
                    .ready_root
                    .join("001proj")
                    .canonicalize()
                    .unwrap_or_else(|_| cfg.ready_root.canonicalize().unwrap().join("001proj")),
                source_relative: PathBuf::from("001proj"),
                est_bytes: 5,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let s = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap();
        assert_eq!(s.handled, 1);
        assert_eq!(s.failed, 0);
        assert!(!p1.exists(), "源应已移到 已备份");
        assert!(
            paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .is_dir(),
            "目标盘应有项目副本"
        );
    }

    #[test]
    fn run_plan_preserves_preexisting_unindexed_dest_and_uses_unique_name() {
        let (_d, cfg, drive) = temp_world();
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        fs::write(p1.join("b.txt"), b"world").unwrap();
        let original = paths::drive_projects_dir(&drive.root).join("001proj");
        fs::create_dir_all(&original).unwrap();
        fs::write(original.join("a.txt"), b"unrelated-user-data").unwrap();
        let item = decide0(&cfg, &drive, &p1);
        let destination = match &item.action {
            PlanAction::RenameAndArchive { dest_name } => dest_name.clone(),
            action => panic!("existing unowned destination requires unique name: {action:?}"),
        };
        assert_ne!(destination, "001proj");
        assert!(!item.dest_existed_at_plan);
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![item],
            opts: test_opts(),
        };
        let summary = run_plan(&cfg, &plan, &AtomicBool::new(false), &NoopReporter).unwrap();
        assert_eq!((summary.handled, summary.failed), (1, 0));
        assert!(!p1.exists());
        assert_eq!(
            fs::read(original.join("a.txt")).unwrap(),
            b"unrelated-user-data"
        );
        assert!(!original.join("b.txt").exists());
        let copied = paths::drive_projects_dir(&drive.root).join(destination);
        assert_eq!(fs::read(copied.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(copied.join("b.txt")).unwrap(), b"world");
    }

    // ── Spec D §4.1: 预览后盘被封 → run_plan 重验判定"计划已过期",不写盘不移源 ──
    #[test]
    fn run_plan_stale_when_drive_sealed_after_plan() {
        let (_d, cfg, drive) = temp_world();
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "001proj".into(),
                source_root: cfg.ready_root.canonicalize().unwrap(),
                source_path: cfg
                    .ready_root
                    .join("001proj")
                    .canonicalize()
                    .unwrap_or_else(|_| cfg.ready_root.canonicalize().unwrap().join("001proj")),
                source_relative: PathBuf::from("001proj"),
                est_bytes: 5,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        // 预览之后、执行之前:盘被封
        drive::seal(&drive).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        let s = run_plan(&cfg, &plan, &cancel, &rep).unwrap();
        assert_eq!(s.handled, 0, "封盘后不执行");
        assert!(s.sealed_stopped, "应标记封盘停本轮");
        assert!(p1.is_dir(), "源不应被移动");
        assert!(
            !paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .exists(),
            "不应写盘"
        );
        let log = rep.0.lock().unwrap().join("\n");
        assert!(
            log.contains("计划已过期"),
            "应提示计划已过期;日志:\n{}",
            log
        );
    }

    // ── Spec D §4.1: 冻结目标名在执行前被占用 → StalePlan,不写盘不移源 ──
    #[test]
    fn run_plan_stale_when_frozen_name_taken() {
        let (_d, cfg, drive) = temp_world();
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        // 预览给的是普通名 001proj;但执行前本盘索引里已出现 001proj(被他人占用)
        let cat = paths::drive_catalog_path(&drive.root);
        if let Some(p) = cat.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(
            &cat,
            "ProjectNo,ProjectName,FileCount,TotalBytes,ArchivedUTC,VerifyStatus,Status,Notes\n\
             001,001proj,1,5,2026-05-29T00:00:00+00:00,SHA256-OK,Complete,\n",
        )
        .unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "001proj".into(),
                source_root: cfg.ready_root.canonicalize().unwrap(),
                source_path: cfg
                    .ready_root
                    .join("001proj")
                    .canonicalize()
                    .unwrap_or_else(|_| cfg.ready_root.canonicalize().unwrap().join("001proj")),
                source_relative: PathBuf::from("001proj"),
                est_bytes: 5,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        let s = run_plan(&cfg, &plan, &cancel, &rep).unwrap();
        assert_eq!(s.handled, 0, "冻名被占不执行");
        assert!(p1.is_dir(), "源不应被移动");
        let log = rep.0.lock().unwrap().join("\n");
        assert!(
            log.contains("计划已过期"),
            "应提示计划已过期;日志:\n{}",
            log
        );
    }

    #[test]
    fn run_plan_checks_paths_before_creating_archived_root() {
        let (_d, mut cfg, drive) = temp_world();
        let unsafe_archived = cfg.ready_root.join("已备份");
        cfg.archived_root = unsafe_archived.clone();
        assert!(!unsafe_archived.exists());
        let plan = ArchivePlan {
            drive,
            items: Vec::new(),
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let err = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap_err();

        assert!(
            format!("{:#}", err).contains("位于"),
            "should fail on unsafe paths, got {err:#}"
        );
        assert!(
            !unsafe_archived.exists(),
            "safety check must run before creating archived_root inside ready_root"
        );
    }

    // ── AR-01 崩溃恢复回归(TEST-AR01):check_pending_txn 三分支 + 重做幂等 ──

    /// 在 cfg.system_root 造一个事务标记。
    fn write_marker(cfg: &Config, src_name: &str, dest_name: &str, src_path: &str, move_to: &str) {
        txn::PendingTxn {
            project_dest_name: dest_name.into(),
            project_src_name: src_name.into(),
            drive_id: "备份1".into(),
            drive_letter: "T".into(),
            in_drive_path: format!("项目\\{dest_name}"),
            src_path: src_path.into(),
            move_to: move_to.into(),
            started_at: "2026-05-29 12:00:00".into(),
        }
        .write(&paths::system_pending_txn(&cfg.system_root))
        .unwrap();
    }

    // ── review-r2 R3-2(撤回后回归守卫):in_ready 重做分支**不得**删除任何索引行
    // (曾尝试在恢复路径清孤儿,引入两个 P1 数据丢失风险,已撤回 → 恢复路径绝不删除)──
    #[test]
    fn check_pending_txn_in_ready_does_not_delete_index() {
        let (_d, cfg, _drive) = temp_world();
        let src = cfg.ready_root.join("001A");
        fs::create_dir_all(&src).unwrap(); // 源仍在待备份 → in_ready=true
        let global = paths::system_global_catalog(&cfg.system_root);
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(
            &global,
            "文件夹名,备份盘名,备份时间,编号,盘内路径,文件数,大小GB,校验方式,校验清单\n\
             001A,备份1,t,001,项目\\001A,1,0.0,SHA256-OK,x\n",
        )
        .unwrap();
        // move_to 指向不存在路径 → moved=false → 走 in_ready 重做分支
        write_marker(
            &cfg,
            "001A",
            "001A",
            &src.display().to_string(),
            "Z:/nope/001A",
        );
        assert!(
            check_pending_txn(&cfg, &NoopReporter, "备份1").is_err(),
            "legacy marker lacks a verified snapshot"
        );
        let content = fs::read_to_string(&global).unwrap();
        assert!(
            content.contains("001A"),
            "in_ready 重做不得删除任何索引行(恢复路径绝不删除);实际:\n{content}"
        );
        assert!(
            paths::system_pending_txn(&cfg.system_root).is_file(),
            "不可信旧标记必须保留"
        );
    }

    // ── review-r2 R4-1(回归):已完成事务(moved&&indexed)+ 用户重建同名源(in_ready)
    // 必须走"仅标记残留→清标记",绝不删已完成备份的索引行(否则误删/污染旧备份)──
    #[test]
    fn check_pending_txn_completed_tx_with_recreated_source_preserves_index() {
        let (_d, cfg, _drive) = temp_world();
        let moved_dest = cfg.archived_root.join("001A");
        fs::create_dir_all(&moved_dest).unwrap(); // moved=true(源已移到已备份)
        let global = paths::system_global_catalog(&cfg.system_root);
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(
            &global,
            "文件夹名,备份盘名,备份时间,编号,盘内路径,文件数,大小GB,校验方式,校验清单\n\
             001A,备份1,t,001,项目\\001A,1,0.0,SHA256-OK,x\n",
        )
        .unwrap(); // indexed=true(已完成备份的记录)
        fs::create_dir_all(cfg.ready_root.join("001A")).unwrap(); // in_ready=true(用户重建同名源)
        write_marker(
            &cfg,
            "001A",
            "001A",
            &cfg.ready_root.join("001A").display().to_string(),
            &moved_dest.display().to_string(),
        );
        assert!(
            check_pending_txn(&cfg, &NoopReporter, "备份1").is_err(),
            "legacy marker lacks a verified snapshot"
        );
        let content = fs::read_to_string(&global).unwrap();
        assert!(
            content.contains("001A"),
            "已完成备份的索引行不应被删;实际:\n{content}"
        );
        assert!(
            paths::system_pending_txn(&cfg.system_root).is_file(),
            "不可信旧标记必须保留"
        );
    }

    #[test]
    fn check_pending_txn_legacy_source_presence_keeps_marker() {
        // 新顺序"写索引→移源"崩在"索引已写、移源未发生"→ 源还在待备份 → 清标记自动重做。
        let (_d, cfg, _drive) = temp_world();
        let src = cfg.ready_root.join("001proj");
        fs::create_dir_all(&src).unwrap();
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &src.display().to_string(),
            &cfg.archived_root.join("001proj").display().to_string(),
        );
        let marker = paths::system_pending_txn(&cfg.system_root);
        assert!(marker.is_file());
        assert!(
            check_pending_txn(&cfg, &NoopReporter, "备份1").is_err(),
            "legacy marker lacks a verified snapshot"
        );
        assert!(marker.exists(), "源存在不能认证事务完成，旧标记保留");
    }

    // ── review-r3 #2:in_ready 重做但本轮目标盘 ≠ 标记里的盘(用户换了盘/原盘离线)
    // → fail-closed,保留标记、不重做、不删除,保护原盘上中断副本的恢复证据 ──
    #[test]
    fn check_pending_txn_in_ready_different_drive_keeps_marker() {
        let (_d, cfg, _drive) = temp_world();
        let src = cfg.ready_root.join("001proj");
        fs::create_dir_all(&src).unwrap(); // 源仍在待备份 → in_ready=true
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &src.display().to_string(),
            &cfg.archived_root.join("001proj").display().to_string(),
        ); // 标记里 drive_id = "备份1"
        let marker = paths::system_pending_txn(&cfg.system_root);
        // 本轮目标盘是「备份2」—— 与标记的「备份1」不符 → 应 fail-closed。
        let r = check_pending_txn(&cfg, &NoopReporter, "备份2");
        assert!(
            r.is_err(),
            "重做目标盘与标记盘不符应 fail-closed,保护原盘孤儿副本的恢复证据"
        );
        assert!(marker.is_file(), "fail-closed 必须保留标记(不清、不删)");
        assert!(src.is_dir(), "源不得被动到");
    }

    #[test]
    fn check_pending_txn_legacy_move_and_global_index_keep_marker() {
        // 已移源 + 索引已写 → 完整完成,仅标记残留 → 自动清除。
        let (_d, cfg, _drive) = temp_world();
        let arch = cfg.archived_root.join("001proj");
        fs::create_dir_all(&arch).unwrap();
        fs::write(
            paths::system_global_catalog(&cfg.system_root),
            "文件夹名,备份盘名\n001proj,备份1\n",
        )
        .unwrap();
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &cfg.ready_root.join("001proj").display().to_string(),
            &arch.display().to_string(),
        );
        let marker = paths::system_pending_txn(&cfg.system_root);
        assert!(
            check_pending_txn(&cfg, &NoopReporter, "备份1").is_err(),
            "legacy marker lacks a verified snapshot"
        );
        assert!(marker.exists(), "缺已校验副本与提交阶段，旧标记保留");
    }

    #[test]
    fn check_pending_txn_moved_not_indexed_keeps_marker() {
        // 罕见:已移源但索引漏写 → 保留标记,交人工核对(不自动清)。
        let (_d, cfg, _drive) = temp_world();
        let arch = cfg.archived_root.join("001proj");
        fs::create_dir_all(&arch).unwrap();
        // 不写 global catalog → indexed=false
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &cfg.ready_root.join("001proj").display().to_string(),
            &arch.display().to_string(),
        );
        let marker = paths::system_pending_txn(&cfg.system_root);
        assert!(
            check_pending_txn(&cfg, &NoopReporter, "备份1").is_err(),
            "已移但索引漏写 → 应阻塞新归档,保留标记待人工"
        );
        assert!(marker.exists(), "已移但索引漏写 → 保留标记待人工");
    }

    #[test]
    fn run_plan_stops_when_pending_txn_unresolved() {
        let (_d, cfg, drive) = temp_world();
        let arch = cfg.archived_root.join("001proj");
        fs::create_dir_all(&arch).unwrap();
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &cfg.ready_root.join("001proj").display().to_string(),
            &arch.display().to_string(),
        );
        let marker = paths::system_pending_txn(&cfg.system_root);
        let marker_before = fs::read_to_string(&marker).unwrap();

        let p2 = cfg.ready_root.join("002proj");
        fs::create_dir_all(&p2).unwrap();
        fs::write(p2.join("b.txt"), b"world").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "002proj".into(),
                source_root: cfg.ready_root.canonicalize().unwrap(),
                source_path: cfg
                    .ready_root
                    .join("002proj")
                    .canonicalize()
                    .unwrap_or_else(|_| cfg.ready_root.canonicalize().unwrap().join("002proj")),
                source_relative: PathBuf::from("002proj"),
                est_bytes: 5,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "002proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let err = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap_err();

        assert!(
            format!("{err:#}").contains("可验证的提交阶段及内容快照"),
            "legacy evidence must block new work rather than infer success: {err:#}"
        );
        assert_eq!(
            fs::read_to_string(&marker).unwrap(),
            marker_before,
            "new archive must not overwrite unresolved marker"
        );
        assert!(
            p2.is_dir(),
            "new source must not move while old txn unresolved"
        );
        assert!(
            !paths::drive_projects_dir(&drive.root)
                .join("002proj")
                .exists(),
            "new project must not be copied while old txn unresolved"
        );
    }

    #[test]
    fn run_plan_stops_when_pending_txn_parse_fails() {
        let (_d, cfg, drive) = temp_world();
        let marker = paths::system_pending_txn(&cfg.system_root);
        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&marker, "not = valid = toml").unwrap();
        let marker_before = fs::read_to_string(&marker).unwrap();

        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "001proj".into(),
                source_root: cfg.ready_root.canonicalize().unwrap(),
                source_path: cfg
                    .ready_root
                    .join("001proj")
                    .canonicalize()
                    .unwrap_or_else(|_| cfg.ready_root.canonicalize().unwrap().join("001proj")),
                source_relative: PathBuf::from("001proj"),
                est_bytes: 5,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let err = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap_err();

        assert!(
            format!("{:#}", err).contains("事务标记"),
            "should stop on unreadable pending txn, got {err:#}"
        );
        assert_eq!(fs::read_to_string(&marker).unwrap(), marker_before);
        assert!(p1.is_dir(), "source must not move after parse failure");
        assert!(!paths::drive_projects_dir(&drive.root)
            .join("001proj")
            .exists());
    }

    #[test]
    fn crash_after_index_before_move_redo_is_idempotent() {
        // AR-01 核心保证:新顺序"写索引→移源",若崩在"索引已写、源未移",源还在待备份;
        // 重做时本盘索引已有该项 → 改时间戳唯一名归档,绝不覆盖已写副本,源不丢失。
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        // 首次归档成功(源移走,本盘索引写入 001proj)。
        handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        assert!(!proj.exists(), "首次归档后源已移走");
        // 复现崩溃残留:源又出现在待备份(= 索引已写但移源被中断,源还在)。
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        // 重做:本盘索引已有 001proj → 走重名 → 改时间戳唯一名。
        let out = handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        assert!(
            matches!(out, HandleOutcome::Done(_)),
            "重做应成功(改名归档)"
        );
        assert!(!proj.exists(), "重做后源被移走,不丢失");
        let proj_dir = paths::drive_projects_dir(&drive.root);
        assert!(proj_dir.join("001proj").is_dir(), "原副本未被覆盖");
        let renamed = fs::read_dir(&proj_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with("001proj_") && n != "001proj"
            });
        assert!(renamed, "重做应以时间戳唯一名归档,不覆盖原 001proj");
    }

    // 计划的 existed=true 不构成所有权证明：未登记用户数据不能隔离或覆盖。
    #[test]
    fn handle_one_unowned_existing_payload_is_kept_with_source() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        let dest = paths::drive_projects_dir(&drive.root).join("001proj");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("a.txt"), b"world").unwrap();
        let outcome = handle_one(
            &cfg,
            &NoopReporter,
            &drive,
            &proj,
            &test_opts(),
            None,
            Some(("001proj", true)),
        )
        .unwrap();
        assert!(matches!(outcome, HandleOutcome::StalePlan));
        assert_eq!(fs::read(proj.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"world");
        assert!(!paths::drive_quarantine_dir(&drive.root)
            .join("001proj/a.txt")
            .exists());
        assert!(!paths::system_global_catalog(&cfg.system_root).is_file());
    }

    // ── 强优化:run_plan 执行期 consumed 累计触发封盘(前项占用后,后项余量不足停本轮) ──
    // 现有测试只覆盖 plan 期 plan_items 纯函数;本测试走 run_plan 集成路径,验证 consumed 扣减 →
    // handle_one 返回 DriveSealed → summary.sealed_stopped、后项源保留。(强优化 review)
    #[test]
    fn run_plan_seals_mid_round_on_cumulative_consumed() {
        let (_d, cfg, mut drive) = temp_world();
        drive.free_bytes = 6000; // 够一个 4096,不够两个
        let p1 = cfg.ready_root.join("001a");
        let p2 = cfg.ready_root.join("002b");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();
        fs::write(p1.join("f"), vec![0u8; 4096]).unwrap();
        fs::write(p2.join("f"), vec![0u8; 4096]).unwrap();
        let plan =
            ArchivePlan {
                drive: drive.clone(),
                items: vec![
                    PlanItem {
                        name: "001a".into(),
                        source_root: cfg.ready_root.canonicalize().unwrap(),
                        source_path: cfg.ready_root.join("001a").canonicalize().unwrap_or_else(
                            |_| cfg.ready_root.canonicalize().unwrap().join("001a"),
                        ),
                        source_relative: PathBuf::from("001a"),
                        est_bytes: 4096,
                        dest_existed_at_plan: false,
                        action: PlanAction::Archive {
                            dest_name: "001a".into(),
                        },
                    },
                    PlanItem {
                        name: "002b".into(),
                        source_root: cfg.ready_root.canonicalize().unwrap(),
                        source_path: cfg.ready_root.join("002b").canonicalize().unwrap_or_else(
                            |_| cfg.ready_root.canonicalize().unwrap().join("002b"),
                        ),
                        source_relative: PathBuf::from("002b"),
                        est_bytes: 4096,
                        dest_existed_at_plan: false,
                        action: PlanAction::Archive {
                            dest_name: "002b".into(),
                        },
                    },
                ],
                opts: test_opts(),
            };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let s = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap();
        assert_eq!(s.handled, 1, "第一个项目应归档");
        assert!(s.sealed_stopped, "第二个项目累计余量不足应封盘停本轮");
        assert!(p2.is_dir(), "封盘停本轮后第二个项目源应仍在待备份");
        assert!(
            !cfg.archived_root.join("002b").exists(),
            "第二个项目不应被归档"
        );
        assert!(
            cfg.archived_root.join("001a").is_dir(),
            "第一个项目应已归档移源"
        );
    }

    // ── 强优化:ArchiveLock 的 Drop(RAII)在 run_plan 正常退出后释放锁,次轮可再取锁 ──
    // 现有测试只覆盖『锁预先存在 → 拒绝』。本测试锁死反向:正常跑完锁文件消失、第二轮能成功取锁,
    // 防止 `_archive_lock` 误成 `_` 立即 drop / 锁未释放导致归档永久被自己上轮残留锁挡死。(强优化 review)
    #[test]
    fn run_plan_releases_lock_so_next_round_can_acquire() {
        let (_d, cfg, drive) = temp_world();
        let lock = cfg.system_root.join(".bftool-archive.lock");
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let p1 = cfg.ready_root.join("001a");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        let plan1 = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "001a".into(),
                source_root: cfg.ready_root.canonicalize().unwrap(),
                source_path: cfg
                    .ready_root
                    .join("001a")
                    .canonicalize()
                    .unwrap_or_else(|_| cfg.ready_root.canonicalize().unwrap().join("001a")),
                source_relative: PathBuf::from("001a"),
                est_bytes: 5,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "001a".into(),
                },
            }],
            opts: test_opts(),
        };
        let s1 = run_plan(&cfg, &plan1, &cancel, &NoopReporter).unwrap();
        assert_eq!(s1.handled, 1);
        assert!(!lock.exists(), "run_plan 正常退出后应已释放归档锁(Drop)");

        let p2 = cfg.ready_root.join("002b");
        fs::create_dir_all(&p2).unwrap();
        fs::write(p2.join("a.txt"), b"world").unwrap();
        let plan2 = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "002b".into(),
                source_root: cfg.ready_root.canonicalize().unwrap(),
                source_path: cfg
                    .ready_root
                    .join("002b")
                    .canonicalize()
                    .unwrap_or_else(|_| cfg.ready_root.canonicalize().unwrap().join("002b")),
                source_relative: PathBuf::from("002b"),
                est_bytes: 5,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "002b".into(),
                },
            }],
            opts: test_opts(),
        };
        let s2 = run_plan(&cfg, &plan2, &cancel, &NoopReporter).unwrap();
        assert_eq!(s2.handled, 1, "上轮锁已释放,第二轮应能取锁并归档");
    }

    // ── 强优化:崩溃重做(索引已写源未移)产出的新副本内容必须正确,且不触碰旧副本 ──
    // 现有 idempotent 测试只验『改名/不覆盖/源移走』,两次源内容相同、从不读新副本内容。本测试把盘上
    // 旧副本篡改成坏内容,重做后断言:新时间戳副本内容 == 源内容、旧坏副本未被覆盖、源被移走。(强优化 review)
    #[test]
    fn crash_redo_new_copy_content_correct_old_untouched() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        let proj_dir = paths::drive_projects_dir(&drive.root);
        // 篡改盘上已索引的旧副本(模拟它其实是坏的)。
        fs::write(proj_dir.join("001proj").join("a.txt"), b"BAD!!").unwrap();
        // 崩溃残留:源又出现在待备份(内容正确)。
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        let out = handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        assert!(
            matches!(out, HandleOutcome::Done(_)),
            "重做应成功(改名归档)"
        );
        assert!(!proj.exists(), "重做后源被移走");
        assert_eq!(
            fs::read(proj_dir.join("001proj").join("a.txt")).unwrap(),
            b"BAD!!",
            "原(坏)副本不应被覆盖"
        );
        let renamed = fs::read_dir(&proj_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.starts_with("001proj_") && n != "001proj")
            .expect("重做应产出时间戳唯一名新副本");
        assert_eq!(
            fs::read(proj_dir.join(&renamed).join("a.txt")).unwrap(),
            b"hello",
            "重做新副本内容应等于源内容"
        );
    }

    // ── 强优化:NTFS 大小写不敏感 —— catalog/global 索引判重应折叠大小写(仅大小写不同算已存在) ──
    #[cfg(windows)]
    #[test]
    fn catalog_and_global_has_project_case_insensitive_on_windows() {
        let d = tempfile::tempdir().unwrap();
        let cat = d.path().join("cat.csv");
        fs::write(&cat, "ProjectName,TotalBytes\nProj,10\n").unwrap();
        assert!(
            catalog_has_project(&cat, "proj").unwrap(),
            "NTFS:仅大小写不同应判已存在"
        );
        assert!(catalog_has_project(&cat, "PROJ").unwrap());
        assert!(!catalog_has_project(&cat, "other").unwrap());

        let glob = d.path().join("global.csv");
        fs::write(&glob, "文件夹名,盘\nProj,备份1\n").unwrap();
        assert!(global_has_folder(&glob, "proj").unwrap());
        assert!(!global_has_folder(&glob, "nope").unwrap());
    }

    // ── 强优化:每项执行前的盘身份复验(换盘/封盘中止;离线退回投影;同盘取 min(投影,实时)) ──
    #[test]
    fn reconcile_item_drive_guards_swap_seal_offline() {
        let planned = DriveInfo {
            letter: "T".into(),
            root: std::path::PathBuf::new(),
            id: "备份1".into(),
            sealed: false,
            free_bytes: 10_000,
            total_bytes: 20_000,
        };
        // 盘离线/读不到 → 退回投影值(free - consumed)。
        assert!(matches!(
            reconcile_item_drive(&planned, 3_000, None),
            ItemDriveCheck::Use(7_000)
        ));
        // 同盘、实时更低 → 取 min(投影 9000, 实时 4000)。
        let same = DriveInfo {
            id: "备份1".into(),
            free_bytes: 4_000,
            ..planned.clone()
        };
        assert!(matches!(
            reconcile_item_drive(&planned, 1_000, Some(&same)),
            ItemDriveCheck::Use(4_000)
        ));
        // 同盘、实时更高 → 仍取投影 9000。
        let roomy = DriveInfo {
            free_bytes: 999_999,
            ..planned.clone()
        };
        assert!(matches!(
            reconcile_item_drive(&planned, 1_000, Some(&roomy)),
            ItemDriveCheck::Use(9_000)
        ));
        // 盘内编号变了(同盘符换盘)→ 中止本轮。
        let other = DriveInfo {
            id: "备份7".into(),
            ..planned.clone()
        };
        assert!(matches!(
            reconcile_item_drive(&planned, 0, Some(&other)),
            ItemDriveCheck::AbortSwapped
        ));
        // 中途被封盘 → 中止本轮。
        let sealed = DriveInfo {
            id: "备份1".into(),
            sealed: true,
            ..planned.clone()
        };
        assert!(matches!(
            reconcile_item_drive(&planned, 0, Some(&sealed)),
            ItemDriveCheck::AbortSealed
        ));
    }

    // ── 强优化(孤儿索引行):append_global_catalog 对同(文件夹名,备份盘名)幂等;不同盘照常追加 ──
    #[test]
    fn append_global_catalog_idempotent_per_folder_and_drive() {
        let d = tempfile::tempdir().unwrap();
        let g = d.path().join("global.csv");
        let mk = |drive: &str| GlobalCatalogRow {
            folder_name: "001proj".into(),
            drive_name: drive.into(),
            archived_time: "2026-06-06 00:00:00".into(),
            project_no: "001".into(),
            in_drive_path: "项目\\001proj".into(),
            file_count: 1,
            size_gb: 0.0,
            verify: "SHA256-OK".into(),
            manifest_path: "本盘信息\\校验清单\\001proj.sha256.csv".into(),
        };
        append_global_catalog(&g, &mk("备份1")).unwrap();
        append_global_catalog(&g, &mk("备份1")).unwrap(); // 同(文件夹,盘)→ 幂等跳过
        append_global_catalog(&g, &mk("备份2")).unwrap(); // 不同盘 → 仍追加
        let gc = fs::read_to_string(&g).unwrap();
        assert_eq!(
            gc.lines()
                .filter(|l| l.starts_with("001proj,备份1,"))
                .count(),
            1,
            "同盘同名应幂等(只一行),实际:\n{gc}"
        );
        assert_eq!(
            gc.lines()
                .filter(|l| l.starts_with("001proj,备份2,"))
                .count(),
            1,
            "不同盘应照常追加"
        );
    }

    // NTFS name folding detects occupied identities; only the exact committed row is replayable.
    #[cfg(windows)]
    #[test]
    fn append_global_catalog_idempotent_case_insensitive_on_windows() {
        let d = tempfile::tempdir().unwrap();
        let g = d.path().join("global.csv");
        let mk = |folder: &str| GlobalCatalogRow {
            folder_name: folder.into(),
            drive_name: "备份1".into(),
            archived_time: "2026-06-06 00:00:00".into(),
            project_no: "001".into(),
            in_drive_path: format!("项目\\{}", folder),
            file_count: 1,
            size_gb: 0.0,
            verify: "SHA256-OK".into(),
            manifest_path: "m".into(),
        };
        let committed = mk("MyProj");
        append_global_catalog(&g, &committed).unwrap();
        let original = fs::read(&g).unwrap();
        assert!(
            super::catalog::global_catalog_has_project_on_drive(&g, "myproj", "备份1").unwrap()
        );
        assert!(
            super::catalog::global_catalog_has_project_on_drive(&g, "MYPROJ", "备份1").unwrap()
        );
        assert!(append_global_catalog(&g, &mk("myproj")).is_err());
        assert_eq!(
            fs::read(&g).unwrap(),
            original,
            "case-only conflicts keep the committed evidence"
        );
        let mut conflicting = committed.clone();
        conflicting.file_count += 1;
        assert!(append_global_catalog(&g, &conflicting).is_err());
        assert_eq!(
            fs::read(&g).unwrap(),
            original,
            "metadata conflicts keep the committed evidence"
        );
        append_global_catalog(&g, &committed).unwrap();
        assert_eq!(
            fs::read(&g).unwrap(),
            original,
            "exact replay is byte-idempotent"
        );
        let gc = fs::read_to_string(&g).unwrap();
        assert_eq!(
            gc.lines()
                .filter(|l| l.to_lowercase().starts_with("myproj,备份1,"))
                .count(),
            1,
            "casefold conflicts must not add a second row; exact replay stays idempotent:\n{gc}"
        );
    }

    // ── 强优化(孤儿索引行):「全局已写、本盘未写」崩溃→重做应复用原名、无时间戳重复副本、全局无重复行 ──
    // 同时守住两个修复点:写序「全局先于本盘」(否则全局不会被写)+ 全局 append 幂等(否则重做产生重复行)。
    #[test]
    fn redo_after_partial_global_write_reuses_name_no_orphan() {
        let (_d, cfg, drive) = temp_world();
        let p = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("a.txt"), b"hello").unwrap();
        // 模拟「全局已写、本盘未写」崩溃:把本盘索引路径占成目录 → append_drive 失败;
        // 因新写序「全局先于本盘」,此前 append_global 已成功写入。
        let drive_cat = paths::drive_catalog_path(&drive.root);
        fs::create_dir_all(&drive_cat).unwrap();
        let out = handle_one(&cfg, &NoopReporter, &drive, &p, &test_opts(), None, None).unwrap();
        assert!(
            matches!(out, HandleOutcome::CommitInterrupted),
            "本盘写失败应 CommitInterrupted"
        );
        let global = paths::system_global_catalog(&cfg.system_root);
        assert!(
            global_has_folder(&global, "001proj").unwrap(),
            "新写序下全局索引应已写(全局先于本盘)"
        );
        assert!(p.is_dir(), "提交中断,源仍在待备份");

        // 解除占用,重做(源仍在待备份)。
        fs::remove_dir_all(&drive_cat).unwrap();
        check_pending_txn(&cfg, &NoopReporter, &drive.id).unwrap();
        assert!(!p.exists(), "可信快照恢复提交后源被移走");
        assert!(!paths::system_pending_txn(&cfg.system_root).exists());

        // 复用原名,不产生时间戳重复副本(孤儿)。
        let proj_dir = paths::drive_projects_dir(&drive.root);
        let stamped = fs::read_dir(&proj_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with("001proj_"));
        assert!(!stamped, "应复用原名 001proj,不产生时间戳重复副本+孤儿");
        // 全局索引只有一行 001proj(幂等吸收了崩溃前写的那行)。
        let gc = fs::read_to_string(&global).unwrap();
        assert_eq!(
            gc.lines().filter(|l| l.starts_with("001proj,")).count(),
            1,
            "全局索引应只有一行 001proj(幂等),实际:\n{gc}"
        );
        // 本盘索引补回该行。
        assert!(
            catalog_has_project(&drive_cat, "001proj").unwrap(),
            "本盘索引应补回 001proj"
        );
    }
}

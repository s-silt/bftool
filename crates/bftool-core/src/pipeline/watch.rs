//! P1'：文件夹监视 daemon（poll + 单实例锁）。
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::observe::{Event, EventSink, ReporterSink};
use crate::pipeline::archive::{self, PlanAction, SKIP_UNCHANGED};
use crate::reporter::Reporter;
use crate::service::request::WatchRequest;

#[derive(Debug, Default, Clone)]
pub struct WatchSummary {
    pub cycles: u32,
    pub archived: usize,
    pub skipped_unchanged: usize,
    pub failed: usize,
    pub cancelled: bool,
}

/// `system_root/watch.lock` 单实例锁。
#[derive(Debug)]
pub struct WatchLock {
    path: PathBuf,
}

impl WatchLock {
    pub fn try_acquire(system_root: &Path) -> Result<Self> {
        fs::create_dir_all(system_root).ok();
        let path = system_root.join("watch.lock");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "pid={} ts={}", std::process::id(), chrono::Local::now());
                let _ = f.sync_all();
                Ok(Self { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!(
                    "已有监视在跑（锁文件存在）：{}。若确认无其它 bftool watch，可手动删除该锁后重试。",
                    path.display()
                );
            }
            Err(e) => Err(e).with_context(|| format!("创建 watch 锁失败：{}", path.display())),
        }
    }
}

impl Drop for WatchLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn run(
    cfg: &Config,
    req: &WatchRequest,
    reporter: &dyn Reporter,
    cancel: &AtomicBool,
) -> Result<WatchSummary> {
    // A pre-cancelled request must not create a system directory or a lock.
    if cancel.load(Ordering::Relaxed) {
        return Ok(WatchSummary {
            cancelled: true,
            ..WatchSummary::default()
        });
    }
    // Dry-run only observes/plans: even creating and removing a lock changes
    // directory metadata. Keep the exclusive writer lock for normal watch.
    let _lock = if req.archive.dry_run {
        None
    } else {
        Some(WatchLock::try_acquire(&cfg.system_root)?)
    };
    let sink = ReporterSink { reporter };
    let poll = if req.poll_secs == 0 {
        cfg.watch_poll_secs.max(1)
    } else {
        req.poll_secs
    };
    let mut summary = WatchSummary::default();

    loop {
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        sink.emit(Event::WatchTick {
            source: req.folder.clone(),
            drive: None,
        });
        reporter.info(&format!(
            "watch.tick #{} source={}",
            summary.cycles + 1,
            req.folder.display()
        ));

        let arch = req.to_archive_request();
        let opts = arch.merged_options();
        // H7 由 plan 预检内 run_hash_policy 强制
        match archive::plan(cfg, &opts, reporter) {
            Ok(plan) => {
                let skipped = plan
                    .items
                    .iter()
                    .filter(|it| {
                        matches!(&it.action, PlanAction::Skip(r) if r.contains(SKIP_UNCHANGED))
                    })
                    .count();
                for it in &plan.items {
                    if matches!(&it.action, PlanAction::Skip(r) if r.contains(SKIP_UNCHANGED)) {
                        sink.emit(Event::ArchiveSkippedUnchanged {
                            name: it.name.clone(),
                        });
                    }
                }
                summary.skipped_unchanged += skipped;
                if opts.dry_run {
                    let _ = archive::run(cfg, reporter, opts, cancel)?;
                } else {
                    let s = archive::run_plan(cfg, &plan, cancel, reporter)?;
                    summary.archived += s.handled;
                    summary.failed += s.failed;
                    if s.cancelled {
                        summary.cancelled = true;
                        summary.cycles += 1;
                        break;
                    }
                }
            }
            Err(e) => {
                // 无可写盘等：软空转；硬闸上抛
                let msg = format!("{e:#}");
                if msg.contains("未发现可写入的备份盘") || msg.contains("NoWritable") {
                    reporter.warn(&format!("本轮无可用备份盘，稍后重试：{msg}"));
                } else {
                    return Err(e);
                }
            }
        }
        summary.cycles += 1;
        if req.once {
            break;
        }
        sleep_interruptible(Duration::from_secs(poll), cancel);
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
    }
    Ok(summary)
}

fn sleep_interruptible(total: Duration, cancel: &AtomicBool) {
    let slice = Duration::from_millis(200);
    let mut left = total;
    while left > Duration::ZERO {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let step = if left < slice { left } else { slice };
        thread::sleep(step);
        left = left.saturating_sub(step);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporter::NoopReporter;
    use crate::service::FileFilter;
    use tempfile::tempdir;

    fn watch_world(base: &Path, dry_run: bool) -> (Config, WatchRequest) {
        let source = base.join("ready");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("example.txt"), b"synthetic source text").unwrap();
        let cfg = Config {
            ready_root: source.clone(),
            archived_root: base.join("archived"),
            system_root: base.join("system"),
            // No physical volume can be selected by this read-only unit test.
            min_drive_gb: u64::MAX,
            ..Config::default()
        };
        let req = WatchRequest {
            folder: source,
            files: FileFilter::default(),
            poll_secs: 1,
            once: true,
            archive: archive::Options {
                dry_run,
                ..archive::Options::default()
            },
        };
        (cfg, req)
    }

    // Include directory mtimes as well as every file's bytes: create/remove of
    // a temporary lock must not be mistaken for an unchanged dry-run tree.
    type Snapshot = Vec<(PathBuf, std::time::SystemTime, Option<Vec<u8>>)>;

    fn snapshot(root: &Path) -> Snapshot {
        fn visit(root: &Path, path: &Path, out: &mut Snapshot) {
            let meta = fs::metadata(path).unwrap();
            out.push((
                path.strip_prefix(root).unwrap().to_path_buf(),
                meta.modified().unwrap(),
                if meta.is_file() {
                    Some(fs::read(path).unwrap())
                } else {
                    None
                },
            ));
            if meta.is_dir() {
                let mut children: Vec<_> = fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .collect();
                children.sort();
                for child in children {
                    visit(root, &child, out);
                }
            }
        }
        let mut out = Vec::new();
        visit(root, root, &mut out);
        out
    }

    #[test]
    fn watch_dry_run_does_not_create_system_directory() {
        let d = tempdir().unwrap();
        let (cfg, req) = watch_world(d.path(), true);
        let before = snapshot(d.path());
        let result = run(&cfg, &req, &NoopReporter, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.cycles, 1);
        assert!(!result.cancelled);
        assert!(!cfg.system_root.exists());
        assert_eq!(before, snapshot(d.path()));
    }

    #[test]
    fn watch_dry_run_preserves_existing_directory_metadata() {
        let d = tempdir().unwrap();
        let (cfg, req) = watch_world(d.path(), true);
        fs::create_dir_all(&cfg.system_root).unwrap();
        fs::write(cfg.system_root.join("note.txt"), b"existing synthetic text").unwrap();
        let before = snapshot(d.path());
        let result = run(&cfg, &req, &NoopReporter, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.cycles, 1);
        assert!(!cfg.system_root.join("watch.lock").exists());
        assert_eq!(before, snapshot(d.path()));
    }

    #[test]
    fn watch_dry_run_does_not_acquire_or_change_writer_lock() {
        let d = tempdir().unwrap();
        let (cfg, req) = watch_world(d.path(), true);
        fs::create_dir_all(&cfg.system_root).unwrap();
        fs::write(cfg.system_root.join("watch.lock"), b"existing writer lock").unwrap();
        let before = snapshot(d.path());
        let result = run(&cfg, &req, &NoopReporter, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.cycles, 1);
        assert_eq!(before, snapshot(d.path()));
    }

    #[test]
    fn watch_normal_run_still_rejects_existing_writer_lock() {
        let d = tempdir().unwrap();
        let (cfg, req) = watch_world(d.path(), false);
        fs::create_dir_all(&cfg.system_root).unwrap();
        fs::write(cfg.system_root.join("watch.lock"), b"existing writer lock").unwrap();
        let before = snapshot(d.path());
        let err = run(&cfg, &req, &NoopReporter, &AtomicBool::new(false)).unwrap_err();
        assert!(err.to_string().contains("已有监视"));
        assert_eq!(before, snapshot(d.path()));
    }

    #[test]
    fn watch_pre_cancelled_requests_do_not_create_directories() {
        for dry_run in [false, true] {
            let d = tempdir().unwrap();
            let (cfg, req) = watch_world(d.path(), dry_run);
            let before = snapshot(d.path());
            let result = run(&cfg, &req, &NoopReporter, &AtomicBool::new(true)).unwrap();
            assert!(result.cancelled);
            assert_eq!(result.cycles, 0);
            assert!(!cfg.system_root.exists());
            assert_eq!(before, snapshot(d.path()));
        }
    }

    #[test]
    fn watch_pre_cancelled_request_preserves_existing_writer_lock() {
        let d = tempdir().unwrap();
        let (cfg, req) = watch_world(d.path(), false);
        fs::create_dir_all(&cfg.system_root).unwrap();
        fs::write(cfg.system_root.join("watch.lock"), b"existing writer lock").unwrap();
        let before = snapshot(d.path());
        let result = run(&cfg, &req, &NoopReporter, &AtomicBool::new(true)).unwrap();
        assert!(result.cancelled);
        assert_eq!(result.cycles, 0);
        assert_eq!(before, snapshot(d.path()));
    }

    struct CancelAtTick<'a> {
        system_root: &'a Path,
        cancel: &'a AtomicBool,
        observed_writer_lock: AtomicBool,
    }

    impl Reporter for CancelAtTick<'_> {
        fn log(&self, _level: crate::reporter::LogLevel, msg: &str) {
            if msg.starts_with("watch.tick #") {
                self.observed_writer_lock.store(
                    self.system_root.join("watch.lock").is_file(),
                    Ordering::Relaxed,
                );
                self.cancel.store(true, Ordering::Relaxed);
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

    #[test]
    fn watch_normal_run_holds_lock_and_cancellation_releases_it() {
        let d = tempdir().unwrap();
        let (cfg, mut req) = watch_world(d.path(), false);
        req.once = false;
        let cancel = AtomicBool::new(false);
        let reporter = CancelAtTick {
            system_root: &cfg.system_root,
            cancel: &cancel,
            observed_writer_lock: AtomicBool::new(false),
        };
        let result = run(&cfg, &req, &reporter, &cancel).unwrap();
        assert!(reporter.observed_writer_lock.load(Ordering::Relaxed));
        assert!(result.cancelled);
        assert_eq!(result.cycles, 1);
        assert!(!cfg.system_root.join("watch.lock").exists());
    }

    #[test]
    fn watch_lock_second_instance_fails() {
        let d = tempdir().unwrap();
        let sys = d.path().join("sys");
        fs::create_dir_all(&sys).unwrap();
        let _a = WatchLock::try_acquire(&sys).unwrap();
        let err = WatchLock::try_acquire(&sys).unwrap_err();
        assert!(
            err.to_string().contains("已有监视"),
            "second lock should fail: {err}"
        );
    }
}

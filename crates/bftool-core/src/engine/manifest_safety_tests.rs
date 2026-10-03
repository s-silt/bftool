//! Deterministic no-follow and owned-directory substitution regressions.
use super::*;
use crate::engine::destination::SafeDir;
use crate::reporter::{LogLevel, NoopReporter, ProgressHandle};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "linux")]
struct ReplaceLeaf {
    leaf: PathBuf,
    outside: PathBuf,
    fired: AtomicBool,
}
#[cfg(target_os = "linux")]
impl Reporter for ReplaceLeaf {
    fn log(&self, _level: LogLevel, _message: &str) {}
    fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle> {
        if !self.fired.swap(true, Ordering::SeqCst) {
            fs::remove_file(&self.leaf).unwrap();
            std::os::unix::fs::symlink(&self.outside, &self.leaf).unwrap();
        }
        NoopReporter.progress_bytes(label, total)
    }
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_manifest_rejects_leaf_replaced_with_link_after_enumeration() {
    let world = tempfile::tempdir().unwrap();
    let root = world.path().join("owned");
    let guard = SafeDir::open(&root, true).unwrap();
    let leaf = root.join("a.txt");
    fs::write(&leaf, b"OWNED").unwrap();
    let outside = world.path().join("outside.txt");
    fs::write(&outside, b"DO-NOT-READ-AS-BACKUP").unwrap();
    let reporter = ReplaceLeaf {
        leaf: leaf.clone(),
        outside: outside.clone(),
        fired: AtomicBool::new(false),
    };
    let result = build_guarded(&guard, ManifestOpts { no_hash: false }, &reporter);
    assert!(reporter.fired.load(Ordering::SeqCst));
    assert!(
        result.is_err(),
        "guard must reject the substituted leaf before hashing it"
    );
    assert_eq!(fs::read(outside).unwrap(), b"DO-NOT-READ-AS-BACKUP");
    assert!(fs::symlink_metadata(leaf).unwrap().file_type().is_symlink());
}

#[cfg(target_os = "linux")]
struct ReplaceRootAtFinish {
    root: PathBuf,
    moved: PathBuf,
}
#[cfg(target_os = "linux")]
struct ReplaceRootProgress {
    root: PathBuf,
    moved: PathBuf,
}
#[cfg(target_os = "linux")]
impl ProgressHandle for ReplaceRootProgress {
    fn inc(&mut self, _bytes: u64) {}
    fn finish(&mut self) {
        fs::rename(&self.root, &self.moved).unwrap();
        fs::create_dir(&self.root).unwrap();
        fs::write(self.root.join("user.txt"), b"UNRELATED").unwrap();
    }
}
#[cfg(target_os = "linux")]
impl Reporter for ReplaceRootAtFinish {
    fn log(&self, _level: LogLevel, _message: &str) {}
    fn progress_bytes(&self, _label: &str, _total: u64) -> Box<dyn ProgressHandle> {
        Box::new(ReplaceRootProgress {
            root: self.root.clone(),
            moved: self.moved.clone(),
        })
    }
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_manifest_refuses_success_after_owned_root_is_replaced() {
    let world = tempfile::tempdir().unwrap();
    let root = world.path().join("owned");
    let moved = world.path().join("original-held-directory");
    let guard = SafeDir::open(&root, true).unwrap();
    fs::write(root.join("a.txt"), b"OWNED").unwrap();
    let reporter = ReplaceRootAtFinish {
        root: root.clone(),
        moved: moved.clone(),
    };
    let result = build_guarded(&guard, ManifestOpts { no_hash: false }, &reporter);
    assert!(
        result.is_err(),
        "read success cannot authenticate a new object at the old pathname"
    );
    assert_eq!(fs::read(moved.join("a.txt")).unwrap(), b"OWNED");
    assert_eq!(fs::read(root.join("user.txt")).unwrap(), b"UNRELATED");
    assert!(!root.join("a.txt").exists());
}

#[test]
fn new_manifest_publication_preserves_existing_evidence() {
    let world = tempfile::tempdir().unwrap();
    let source = world.path().join("source.txt");
    fs::write(&source, b"PAYLOAD").unwrap();
    let snapshot = build(&source, ManifestOpts { no_hash: false }, &NoopReporter).unwrap();
    let evidence = world.path().join("same.sha256.csv");
    fs::write(&evidence, b"FORENSIC-EVIDENCE").unwrap();
    assert!(snapshot.write_csv_new(&evidence).is_err());
    assert_eq!(fs::read(&evidence).unwrap(), b"FORENSIC-EVIDENCE");
    assert_eq!(fs::read(&source).unwrap(), b"PAYLOAD");
    let fresh = world.path().join("fresh.sha256.csv");
    snapshot.write_csv_new(&fresh).unwrap();
    assert!(fs::read_to_string(fresh).unwrap().contains("source.txt"));
}

//! Atomic durable metadata writes. Temporary files have unique create-new names;
//! unrelated temporary files are never truncated or removed. Every destination
//! ancestor is opened without following links by the shared write boundary.
//! File contents are synced on all supported platforms. Linux also syncs the
//! containing directory after publication; Windows still has the file-level
//! FlushFileBuffers durability boundary and requires crash-recovery verification.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::Path;

/// Write and sync a unique same-directory temporary file, then atomically publish it.
/// Existing ordinary metadata files may be replaced; links and special files fail closed.
pub fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().context("原子写入目标缺少文件名")?;
    crate::engine::destination::SafeDir::open(parent, false)?.write_atomic(Path::new(name), bytes)
}

/// 对已写好的 `File` 句柄 fsync,供 csv writer 等"先 flush 再拿回底层 File"的场景。
pub fn sync_file(f: &File) -> Result<()> {
    f.sync_all().context("刷盘(fsync)失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_synced_writes_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.bin");
        write_synced(&p, b"hello\x00\xffworld").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hello\x00\xffworld");
    }

    #[test]
    fn write_synced_overwrites_fully_without_leftover_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.bin");
        write_synced(&p, b"a very long previous content").unwrap();
        write_synced(&p, b"short").unwrap();
        // 截断重写:不能残留旧内容尾巴
        assert_eq!(std::fs::read(&p).unwrap(), b"short");
    }
    #[test]
    fn concurrent_atomic_writers_never_share_or_truncate_temporary_files() {
        let world = tempfile::tempdir().unwrap();
        let target = world.path().join("metadata");
        let legacy_temporary = world.path().join("metadata.bftool-tmp");
        std::fs::write(&legacy_temporary, b"UNRELATED").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        let workers: Vec<_> = (0..12)
            .map(|index| {
                let target = target.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let bytes = vec![index as u8; 64 * 1024];
                    barrier.wait();
                    write_synced(&target, &bytes).unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let bytes = std::fs::read(&target).unwrap();
        assert_eq!(bytes.len(), 64 * 1024);
        assert!(bytes[0] < 12);
        assert!(bytes.iter().all(|byte| *byte == bytes[0]));
        assert_eq!(std::fs::read(&legacy_temporary).unwrap(), b"UNRELATED");
        assert_eq!(std::fs::read_dir(world.path()).unwrap().count(), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn atomic_write_refuses_linked_parent_and_preserves_its_referent() {
        use std::os::unix::fs::symlink;
        let world = tempfile::tempdir().unwrap();
        let outside = world.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("metadata"), b"KEEP").unwrap();
        symlink(&outside, world.path().join("linked")).unwrap();
        assert!(write_synced(&world.path().join("linked/metadata"), b"NO").is_err());
        assert_eq!(std::fs::read(outside.join("metadata")).unwrap(), b"KEEP");
    }
}

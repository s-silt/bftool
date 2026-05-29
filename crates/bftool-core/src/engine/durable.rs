//! 持久化写入助手：写文件后 fsync(`sync_all`/FlushFileBuffers),保证崩溃/断电后
//! 数据真正落盘,而不是停在 OS 页缓存(`flush` 只把缓冲交给 OS,并不保证落盘)。
//!
//! Windows-first 说明：这里以**文件级** `sync_all` 为主。父目录 fsync 在 Windows 上
//! 语义不明确(需以 `FILE_FLAG_BACKUP_SEMANTICS` 打开目录句柄,且 NTFS 元数据本身有日志),
//! 故不做。关键的崩溃一致性靠提交顺序保证:**事务标记先于移源落盘、索引先于删标记落盘**
//! (见 [`crate::engine::archive`] 提交段与 [`crate::engine::txn`])。

use anyhow::{Context, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;

/// 写入全部内容后 `sync_all` 落盘。用于事务标记、校验清单等关键写入。
///
/// 这是"截断重写 + fsync",不是原子替换;调用方若需要原子性应写临时文件再 rename。
/// 对 bftool 当前用法(标记/清单/索引,写失败即整步失败重做)足够。
pub fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = File::create(path).with_context(|| format!("创建文件失败：{}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("写入失败：{}", path.display()))?;
    f.sync_all()
        .with_context(|| format!("刷盘(fsync)失败：{}", path.display()))?;
    Ok(())
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
}

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

/// 原子写入：写临时文件 + fsync + rename 替换目标。用于事务标记、校验清单、索引等关键写入。
///
/// 旧实现是"`File::create`(截断)+ write + fsync"。崩溃/断电若发生在截断之后、写完之前,
/// 目标文件会留下零字节或半截内容 —— 对"存在即生效"的事务标记尤其危险(SEC-006)。
/// 改为先把内容写进同目录的临时文件并 fsync,再 `rename` 原子替换目标:
/// rename 在同卷上是原子的,读者只会看到"旧内容"或"完整新内容",绝不会看到半截。
/// 临时文件放在**目标同目录**(而非系统临时目录),保证与目标同卷 → rename 不退化成跨卷拷贝。
///
/// 注:这里不对父目录 fsync(Windows 上目录 fsync 语义不明确,见本模块头注);
/// 内容本身已 fsync,rename 后即便目录项尚未落盘,也只是"看到旧名/新名"的差别,不会半截。
pub fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // 临时名带目标文件名 + .bftool-tmp 后缀,放在同目录(同卷)。不同步并发写同一目标时,
    // 调用方本就该串行(事务/单盘不变式保证),故不额外加唯一后缀。
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "bftool".to_string());
    let tmp = parent.join(format!("{file_name}.bftool-tmp"));

    // 写临时文件 + fsync(块作用域确保 File 在 rename 前已 drop/关闭,Windows 上句柄未关无法 rename)。
    {
        let mut f =
            File::create(&tmp).with_context(|| format!("创建临时文件失败：{}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("写入临时文件失败：{}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("刷盘(fsync)失败：{}", tmp.display()))?;
    }

    // 原子替换。`std::fs::rename` 在 Windows 走 MoveFileExW + MOVEFILE_REPLACE_EXISTING,
    // 目标已存在也会原子覆盖(POSIX 同样允许覆盖)。失败时清理临时文件再上抛,
    // 避免在目标目录里留下 .bftool-tmp 残渣。
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e)
            .with_context(|| format!("原子替换失败：{} → {}", tmp.display(), path.display()));
    }
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

//! 目录复制、统计、待备份发现。
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::engine::{cruft, destination::SafeDir};
use crate::reporter::Reporter;

use super::types::FolderStats;

pub(crate) fn folder_stats(p: &Path) -> FolderStats {
    let mut s = FolderStats::default();
    for entry in cruft::walk(p) {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                s.files += 1;
                match e.metadata() {
                    Ok(m) => {
                        s.bytes += m.len();
                        if let Ok(modified) = m.modified() {
                            if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                                let secs = dur.as_secs() as i64;
                                s.latest_mtime_secs = Some(match s.latest_mtime_secs {
                                    Some(prev) => prev.max(secs),
                                    None => secs,
                                });
                            }
                        }
                    }
                    Err(err) => s
                        .metadata_errors
                        .push(format!("{}: {}", e.path().display(), err)),
                }
            }
            Ok(_) => continue,
            Err(err) => s.enum_errors.push(format!("{}", err)),
        }
    }
    s
}

pub(super) fn source_bftool_part_files(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 || !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !cruft::is_cruft_dir(&name)
        })
    {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                let name = e.file_name().to_string_lossy();
                if name.ends_with(cruft::PART_SUFFIX) {
                    let rel = e
                        .path()
                        .strip_prefix(root)
                        .unwrap_or(e.path())
                        .to_string_lossy()
                        .replace('/', "\\");
                    files.push(rel);
                }
            }
            Ok(_) => {}
            Err(e) => errors.push(e.to_string()),
        }
    }
    if !errors.is_empty() {
        anyhow::bail!("{} 个枚举错误:{}", errors.len(), errors.join("; "));
    }
    Ok(files)
}

/// Copy into a destination opened at the no-follow write boundary.
/// An existing file is never treated as an owned interrupted copy merely because
/// its size matches. The item layer chooses a fresh unique destination; publication
/// refuses every occupied payload name atomically, including dangling links.
#[cfg(test)]
pub(super) fn copy_folder(
    src: &Path,
    dst: &Path,
    no_hash: bool,
    reporter: &dyn Reporter,
) -> Result<std::collections::HashMap<String, String>> {
    let destination = SafeDir::open(dst, true)?;
    copy_folder_into(src, &destination, no_hash, reporter)
}

pub(super) fn copy_folder_into(
    src: &Path,
    destination: &SafeDir,
    no_hash: bool,
    reporter: &dyn Reporter,
) -> Result<std::collections::HashMap<String, String>> {
    destination.require_current_binding()?;
    let mut hashes = std::collections::HashMap::new();
    let source_meta = fs::symlink_metadata(src)
        .with_context(|| format!("读取复制源类型失败：{}", src.display()))?;
    if source_meta.file_type().is_symlink() {
        anyhow::bail!("复制源不能是符号链接：{}", src.display());
    }
    if source_meta.is_file() {
        let name = src.file_name().context("复制源缺少文件名")?;
        if let Some(hash) = destination.copy_new(src, Path::new(name), no_hash)? {
            hashes.insert(name.to_string_lossy().replace('/', "\\"), hash);
        }
        destination.require_current_binding()?;
        return Ok(hashes);
    }
    if !source_meta.is_dir() {
        anyhow::bail!("复制源不是普通文件或目录：{}", src.display());
    }
    cruft::warn_excluded_real_content(src, reporter);
    let mut errors = Vec::new();
    // Keep the current depth-first ancestor handles. Reopening a child by name
    // could otherwise adopt an unrelated ordinary directory swapped in later.
    let mut owned_dirs = vec![destination.ensure_dir(Path::new(""))?];
    for entry in cruft::walk(src) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(error.to_string());
                continue;
            }
        };
        let relative = entry.path().strip_prefix(src)?;
        let depth = relative.components().count();
        if depth == 0 {
            continue;
        }
        owned_dirs.truncate(depth);
        let parent = owned_dirs
            .get(depth - 1)
            .context("目标父目录未取得所有权，停止复制")?;
        let name = Path::new(entry.file_name());
        if entry.file_type().is_dir() {
            parent.require_current_binding()?;
            match parent.create_new_dir(name) {
                Ok(child) => owned_dirs.push(child),
                Err(error) => {
                    errors.push(format!(
                        "安全创建全新目录失败 {}：{}",
                        relative.display(),
                        error
                    ));
                    anyhow::bail!("目标子目录被占用或无法安全创建：{}", relative.display());
                }
            }
        } else if entry.file_type().is_file() {
            if let Some(hash) = parent.copy_new(entry.path(), name, no_hash)? {
                hashes.insert(relative.to_string_lossy().replace('/', "\\"), hash);
            }
        } else {
            reporter.warn(&format!(
                "跳过链接(未复制):{} —— 不跟随符号链接/junction。",
                relative.display()
            ));
        }
    }
    if !errors.is_empty() {
        for error in &errors {
            reporter.error(&format!("复制阶段枚举或创建目录失败：{}", error));
        }
        anyhow::bail!("复制阶段失败 {} 项 → 本项目失败。", errors.len());
    }
    destination.require_current_binding()?;
    Ok(hashes)
}

/// 列出 待备份 下的待归档项目目录(按文件夹名前导数字升序,无数字前缀的排最后)。
///
/// 顶层的符号链接 / junction **不**作为项目归档(本工具不跟随链接,防链接逃逸),
/// 但逐个 `reporter.warn` 告知 —— 否则用户用 junction 把外部目录挂进待备份区时,
/// 整个"项目"会被静默忽略、连"发现 N 个项目"的计数里都看不到。(L-05 VulnGym 审计)
pub(super) fn discover_projects(
    ready_root: &Path,
    reporter: &dyn Reporter,
) -> Result<Vec<PathBuf>> {
    let mut projects = Vec::new();
    let mut linked = Vec::new();
    for e in fs::read_dir(ready_root)
        .with_context(|| format!("读取待备份目录失败：{}", ready_root.display()))?
    {
        let e = e.with_context(|| format!("枚举待备份目录失败：{}", ready_root.display()))?;
        let ft = e
            .file_type()
            .with_context(|| format!("读取待备份项目类型失败：{}", e.path().display()))?;
        // is_symlink() 在 Windows 上对 junction(mount point)同样为 true,且此时 is_dir()==false。
        if ft.is_symlink() {
            linked.push(e.file_name().to_string_lossy().into_owned());
        } else if ft.is_dir() {
            projects.push(e.path());
        }
    }
    for name in &linked {
        reporter.warn(&format!(
            "待备份下的链接「{}」不会被归档(本工具不跟随符号链接/junction);如需备份请改放实际目录。",
            name
        ));
    }
    // 按文件夹名前导数字升序，无数字前缀的排最后
    projects.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        leading_number(name).unwrap_or(u64::MAX)
    });
    Ok(projects)
}

pub(super) fn leading_number(name: &str) -> Option<u64> {
    let s = name.trim_start();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<u64>().ok()
    }
}

pub(super) fn leading_digits(name: &str) -> String {
    let s = name.trim_start();
    s.chars().take_while(|c| c.is_ascii_digit()).collect()
}

#[cfg(test)]
mod destination_tests {
    use super::*;
    use crate::reporter::NoopReporter;

    #[test]
    fn unrelated_existing_payload_is_never_overwritten_or_size_skipped() {
        for old in [b"OLD-UNRELATED".as_slice(), b"OLD".as_slice()] {
            let world = tempfile::tempdir().unwrap();
            let source = world.path().join("source");
            let destination = world.path().join("destination");
            fs::create_dir(&source).unwrap();
            fs::create_dir(&destination).unwrap();
            fs::write(source.join("a.txt"), b"NEW").unwrap();
            fs::write(destination.join("a.txt"), old).unwrap();
            assert!(copy_folder(&source, &destination, false, &NoopReporter).is_err());
            assert_eq!(fs::read(destination.join("a.txt")).unwrap(), old);
            assert_eq!(fs::read(source.join("a.txt")).unwrap(), b"NEW");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn destination_symlink_subtree_is_rejected_before_payload_creation() {
        use std::os::unix::fs::symlink;
        let world = tempfile::tempdir().unwrap();
        let source = world.path().join("source");
        let drive = world.path().join("drive");
        let outside = world.path().join("outside");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::create_dir_all(drive.join("project")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(source.join("sub/a.txt"), b"SOURCE").unwrap();
        symlink(&outside, drive.join("project/sub")).unwrap();
        assert!(copy_folder(&source, &drive.join("project"), false, &NoopReporter).is_err());
        assert!(!outside.join("a.txt").exists());
        fs::remove_file(drive.join("project/sub")).unwrap();
        fs::remove_dir(drive.join("project")).unwrap();
        symlink(&outside, drive.join("project")).unwrap();
        assert!(copy_folder(&source, &drive.join("project"), false, &NoopReporter).is_err());
        assert!(!outside.join("sub").exists());
    }
}

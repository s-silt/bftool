//! 路径安全检查 + 文件夹稳定性检测。

use anyhow::{bail, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::engine::cruft;

/// 校验三个根目录之间的相互关系，以及它们不在备份盘上。
pub fn check_paths(
    ready: &Path,
    archived: &Path,
    system: &Path,
    drive_root: Option<&Path>,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let r = canon(ready);
    let a = canon(archived);
    let s = canon(system);

    if eq_ci(&r, &a) {
        bail!("待备份 与 已备份 不能是同一目录（{}）。", r.display());
    }
    if is_inside(&a, &r) {
        bail!(
            "已备份({}) 不能位于 待备份({}) 之内，否则会被当作待归档项目处理。",
            a.display(),
            r.display()
        );
    }
    if is_inside(&s, &r) {
        bail!(
            "备份系统({}) 不能位于 待备份({}) 之内。",
            s.display(),
            r.display()
        );
    }
    if is_inside(&r, &a) {
        bail!(
            "待备份({}) 不能位于 已备份({}) 之内。",
            r.display(),
            a.display()
        );
    }

    if let Some(drive) = drive_root {
        let drive_q = qualifier(drive);
        for (name, p) in [("待备份", &r), ("已备份", &a), ("备份系统", &s)] {
            if qualifier(p).eq_ignore_ascii_case(&drive_q) {
                bail!(
                    "{}({}) 位于当前备份盘 {} 上 —— 会把源/索引写到备份盘自身。请改到固态盘。",
                    name,
                    p.display(),
                    drive_q
                );
            }
        }
    }

    if !qualifier(&r).eq_ignore_ascii_case(&qualifier(&a)) {
        warnings.push(format!(
            "提醒：待备份({}) 与 已备份({}) 不在同一分区 —— 跨卷无法原子移动源,归档到「移动源」这步会失败(项目留在 待备份、可重做)。建议把两者放在同一块盘。",
            r.display(),
            a.display()
        ));
    }
    Ok(warnings)
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn eq_ci(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(b.to_string_lossy().as_ref())
}

fn is_inside(child: &Path, parent: &Path) -> bool {
    // 注意:canonicalize 在路径不存在时回退原始路径,junction/symlink 重定向场景下保护有限。
    // Windows 备份场景下此风险低(正常用户不会故意构建 junction 攻击自己),已知局限。
    let c = format!("{}\\", child.to_string_lossy().to_lowercase());
    let p = format!("{}\\", parent.to_string_lossy().to_lowercase());
    c.starts_with(&p) && c != p
}

fn qualifier(p: &Path) -> String {
    // 取盘符部分 "D:" / ""；字符安全:多字节首字符 / UNC 不 panic(旧 `&s[1..2]` 会)。(ledger L-023)
    // 注意:盘符比较是已知局限——canonicalize 在路径不存在时回退原始路径,
    // junction/symlink 可绕过此检查。Windows 备份场景下风险低,已知局限。
    let s = p.to_string_lossy();
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), Some(':')) if c.is_ascii_alphabetic() => format!("{}:", c.to_ascii_uppercase()),
        _ => String::new(),
    }
}

#[derive(Debug)]
pub struct StableCheck {
    pub stable: bool,
    pub reason: String,
}

/// 文件夹是否稳定：所有文件最近修改时间须早于 N 分钟前，且不被占用。
pub fn folder_stable(root: &Path, minutes: u64) -> StableCheck {
    let cutoff = SystemTime::now() - Duration::from_secs(minutes * 60);
    for entry in cruft::walk(root) {
        // 稳定性检测对 walkdir 错误**保持原 swallow 语义**：
        // 真实枚举错误会在后续 manifest::real_files 阶段被收集并 bail。
        // 让稳定性检测也 bail 会让单个"权限拒绝"在第一关就把项目挡掉，
        // 用户看不到 manifest 阶段更详细的多文件错误汇总。
        let Ok(e) = entry else { continue };
        if !e.file_type().is_file() {
            continue;
        }
        if let Ok(meta) = e.metadata() {
            if let Ok(mt) = meta.modified() {
                if mt > cutoff {
                    return StableCheck {
                        stable: false,
                        reason: format!("最近被修改：{}", e.file_name().to_string_lossy()),
                    };
                }
            }
        }
        // 试着独占只读打开一下，发现被进程独占就跳过
        if let Err(err) = File::open(e.path()) {
            return StableCheck {
                stable: false,
                reason: format!(
                    "被占用/无法读取：{}({})",
                    e.file_name().to_string_lossy(),
                    err
                ),
            };
        }
    }
    StableCheck {
        stable: true,
        reason: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── L-023: qualifier 字符安全 ──
    #[test]
    fn qualifier_is_char_safe() {
        assert_eq!(qualifier(Path::new("D:\\x")), "D:");
        assert_eq!(qualifier(Path::new("e:\\")), "E:");
        assert_eq!(qualifier(Path::new(r"\\srv\share")), "");
        assert_eq!(qualifier(Path::new("中:\\x")), ""); // 多字节首字符:不 panic
        assert_eq!(qualifier(Path::new("")), "");
    }

    // ── L-026: is_inside 边界(前缀不能误判为嵌套) ──
    #[test]
    fn is_inside_basic() {
        assert!(is_inside(Path::new("D:\\a\\b"), Path::new("D:\\a")));
        assert!(!is_inside(Path::new("D:\\ab"), Path::new("D:\\a"))); // 前缀但非父目录
        assert!(!is_inside(Path::new("D:\\a"), Path::new("D:\\a"))); // 自身不算 inside
    }

    // ── L-026: check_paths 拒绝同目录/嵌套/同备份盘 ──
    #[test]
    fn check_paths_rejects_same_ready_archived() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\sys"),
            None,
        );
        assert!(r.is_err(), "待备份==已备份 应拒绝");
    }

    #[test]
    fn check_paths_rejects_archived_inside_ready() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\ready\\done"),
            Path::new("D:\\lib\\sys"),
            None,
        );
        assert!(r.is_err(), "已备份 在 待备份 之内 应拒绝");
    }

    #[test]
    fn check_paths_rejects_root_on_backup_drive() {
        let r = check_paths(
            Path::new("E:\\ready"),
            Path::new("D:\\archived"),
            Path::new("D:\\sys"),
            Some(Path::new("E:\\")),
        );
        assert!(r.is_err(), "根目录在备份盘 E: 上 应拒绝");
    }

    // ── L-010: 跨分区给出准确提醒(归档移源会失败,而非"复制+删除") ──
    #[test]
    fn check_paths_warns_on_different_partition() {
        let w = check_paths(
            Path::new("C:\\ready"),
            Path::new("D:\\archived"),
            Path::new("D:\\sys"),
            None,
        )
        .unwrap();
        assert!(
            w.iter().any(|s| s.contains("不在同一分区")),
            "跨分区应有提醒,实际:{:?}",
            w
        );
    }

    #[test]
    fn check_paths_ok_when_distinct_same_partition() {
        let r = check_paths(
            Path::new("D:\\lib\\ready"),
            Path::new("D:\\lib\\archived"),
            Path::new("D:\\lib\\sys"),
            Some(Path::new("E:\\")),
        )
        .unwrap();
        // 同分区、互不嵌套 → Ok,且无"不同分区"警告
        assert!(r.is_empty(), "同分区互不嵌套应无警告,实际:{:?}", r);
    }
}

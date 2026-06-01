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
        // 备份盘根在生产中总是卷根(如 `E:\`):此时「根目录在备份盘上」= 同卷(比盘符)。
        // 但若传入的是某个子目录(测试夹具或特殊配置),按盘符比会把"同卷不同目录"误判 ——
        // 改为看「根目录是否就在该目录之内」。两种情形都能正确拦住"把源/索引写到备份盘自身"。(review-r2 #1)
        let drive_c = canon(drive);
        let drive_q = qualifier(&drive_c);
        let drive_is_volume = is_volume_root(&drive_c);
        for (name, p) in [("待备份", &r), ("已备份", &a), ("备份系统", &s)] {
            let on_backup = if drive_is_volume {
                !drive_q.is_empty() && qualifier(p).eq_ignore_ascii_case(&drive_q)
            } else {
                eq_ci(p, &drive_c) || is_inside(p, &drive_c)
            };
            if on_backup {
                bail!(
                    "{}({}) 位于当前备份盘 {} 上 —— 会把源/索引写到备份盘自身。请改到固态盘。",
                    name,
                    p.display(),
                    drive.display()
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
    let c = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    strip_verbatim(&c)
}

/// 去掉 Windows `canonicalize()` 对**存在**路径加的 `\\?\`(及 `\\?\UNC\`)verbatim 前缀。
/// canon 对存在/不存在的路径分别返回 verbatim / 原样,前缀不一致会让 `is_inside` / `eq_ci` /
/// `qualifier` 在两者之间比较时错位(把嵌套判成不嵌套、同盘判成不同盘)。统一剥掉前缀即可。(review-r2 #1/#4)
fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_path_buf()
    }
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

/// `p` 是否是卷根(如 `E:\` / `\\?\E:\`)—— 即只有「盘符前缀 + 根」、无其它路径段。
/// 生产中备份盘根恒为卷根;据此区分"按盘符比同卷" vs"按目录包含比"。(review-r2 #1)
fn is_volume_root(p: &Path) -> bool {
    use std::path::Component;
    let mut comps = p.components();
    matches!(comps.next(), Some(Component::Prefix(_)))
        && matches!(comps.next(), Some(Component::RootDir))
        && comps.next().is_none()
}

fn qualifier(p: &Path) -> String {
    // 取盘符部分 "D:" / ""；字符安全:多字节首字符 / UNC 不 panic(旧 `&s[1..2]` 会)。(ledger L-023)
    // 注意:junction/symlink 仍可绕过此盘符比较(已知局限,Windows 备份场景风险低)。
    let full = p.to_string_lossy();
    // 先剥掉 Windows `canonicalize()` 对**真实存在**路径加的 verbatim 前缀(`\\?\` / `\\?\UNC\`)。
    // 否则裸取前两字符会得到 `\\` → 盘符判空 → check_paths 的「根目录在备份盘上」安全闸与
    // 跨分区警告都静默失效(review-r2 #1/#4:canon 后的路径走这里,而 drive.root 未 canon,两边永不相等)。
    let s: &str = match full.strip_prefix(r"\\?\") {
        Some(rest) => match rest.strip_prefix(r"UNC\") {
            Some(_) => "", // verbatim UNC:无盘符
            None => rest,  // \\?\C:\... → C:\...
        },
        None => full.as_ref(),
    };
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

    // ── SEC verbatim(review-r2 #1/#4):canonicalize 给真实路径加 `\\?\` 前缀后,
    // qualifier 必须仍能取出盘符 —— 否则「根目录写到备份盘」安全闸与跨分区警告静默失效 ──
    #[test]
    fn qualifier_strips_verbatim_prefix() {
        // Windows canonicalize() 对**存在**的路径返回 `\\?\` verbatim 前缀(实测 C:\Windows → \\?\C:\Windows)。
        assert_eq!(qualifier(Path::new(r"\\?\C:\Windows")), "C:");
        assert_eq!(qualifier(Path::new(r"\\?\E:\ready")), "E:");
        // verbatim UNC 没有盘符
        assert_eq!(qualifier(Path::new(r"\\?\UNC\srv\share")), "");
        // 普通盘符 / 普通 UNC 行为不变
        assert_eq!(qualifier(Path::new(r"D:\x")), "D:");
        assert_eq!(qualifier(Path::new(r"\\srv\share")), "");
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

    // ── review-r2 #1 回归:真实**存在**的根目录会被 canonicalize 加 `\\?\` 前缀;
    // 安全闸必须仍拒绝。旧测试只用不存在路径(canon 回退原样)"假绿",掩盖了 verbatim bug。──
    #[cfg(windows)]
    #[test]
    fn check_paths_rejects_existing_root_on_backup_drive() {
        let d = tempfile::tempdir().unwrap();
        let ready = d.path().join("ready");
        let archived = d.path().join("archived");
        let system = d.path().join("system");
        std::fs::create_dir_all(&ready).unwrap();
        std::fs::create_dir_all(&archived).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        // 备份盘根 = tempdir 所在盘 → 三根目录都"在备份盘上",应触发安全闸。
        let drive_letter = d.path().to_string_lossy().chars().next().unwrap();
        let drive_root = PathBuf::from(format!("{drive_letter}:\\"));
        let r = check_paths(&ready, &archived, &system, Some(&drive_root));
        assert!(
            r.is_err(),
            "真实存在的根目录在备份盘上 应被拒绝(verbatim 前缀不得绕过安全闸)"
        );
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

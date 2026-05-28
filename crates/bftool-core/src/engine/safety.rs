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
            "提醒：待备份({}) 与 已备份({}) 不在同一分区，归档成功后的「移动」会变成复制+删除（较慢、不够原子）。建议放同一块固态盘。",
            r.display(), a.display()
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
    let c = format!("{}\\", child.to_string_lossy().to_lowercase());
    let p = format!("{}\\", parent.to_string_lossy().to_lowercase());
    c.starts_with(&p) && c != p
}

fn qualifier(p: &Path) -> String {
    // 取盘符部分 "D:" / "" ；不依赖 win32 API，直接看路径前缀
    let s = p.to_string_lossy();
    if s.len() >= 2 && &s[1..2] == ":" {
        s[..2].to_string()
    } else {
        String::new()
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
                    "被占用/无法读取：{}（{}）",
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

//! 挂载扫描与可写盘挑选（P1.x：自 `engine/drive` 迁入）。
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

use crate::engine::drive::{drive_is_sealed, DriveInfo};
use crate::engine::paths;
use crate::reporter::Reporter;

/// 扫描所有已挂载、被识别为备份盘的卷。
/// `reporter`:Some 时,对「看起来是备份盘(有 本盘编号.txt)但编号读失败或为空」的卷发出 warn,
/// 而非静默跳过(io 错误不得被静默吞掉);只读/内部场景传 None 避免刷屏。(review-r3 #8/#9)
pub fn scan_mounted(reporter: Option<&dyn Reporter>) -> Result<Vec<DriveInfo>> {
    let mut out = Vec::new();
    let disks = sysinfo::Disks::new_with_refreshed_list();
    for d in disks.list() {
        let mount = d.mount_point();
        let Some(letter) = drive_letter_of(mount) else {
            continue;
        };
        let root = PathBuf::from(format!("{}:\\", letter));
        let id_file = paths::drive_id_path(&root);
        if !id_file.is_file() {
            continue;
        }
        // 已确认 本盘编号.txt 存在却读不出 → 不静默跳过:这块盘看起来是备份盘,读失败
        // (权限/瞬时 IO/被占用)应可见,否则编号分配与多盘安全闸都在"看不全所有盘"的前提下工作。
        // 仍 skip(不 fail-closed,以免一块无关盘的瞬时锁拖垮整次扫描/列盘/选盘)。(review-r3 #8)
        let id = match fs::read_to_string(&id_file) {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                if let Some(r) = reporter {
                    r.warn(&format!(
                        "跳过盘 {}:(看起来是备份盘但读取 {} 失败:{})—— 编号/容量信息可能不全,请检查该盘。",
                        letter,
                        id_file.display(),
                        e
                    ));
                }
                continue;
            }
        };
        // 空/全空白编号 = 损坏或未初始化:与 resolve_drive_id 的 .filter(|s| !s.is_empty()) 一致,
        // 不放行为合法备份盘,否则它会被 pick_active 选为可写盘、把空编号写进索引污染盘身份。(review-r3 #9)
        if id.is_empty() {
            if let Some(r) = reporter {
                r.warn(&format!(
                    "跳过盘 {}:(本盘编号文件为空,疑似损坏)—— 请重新 init 该盘。",
                    letter
                ));
            }
            continue;
        }
        out.push(DriveInfo {
            letter,
            root: root.clone(),
            id,
            sealed: drive_is_sealed(&root),
            free_bytes: d.available_space(),
            total_bytes: d.total_space(),
        });
    }
    Ok(out)
}

/// 从一组盘里挑出"可写入"的(未封盘且容量 ≥ min_drive_gb),并把"过小被忽略"的单独返回。
/// 纯函数便于测试;容量过滤是「防误抓 U 盘」安全闸 —— 此前 min_drive_gb 形同虚设。(ledger L-006)
pub(crate) fn usable_drives(
    all: Vec<DriveInfo>,
    min_drive_gb: u64,
) -> (Vec<DriveInfo>, Vec<DriveInfo>) {
    let min_bytes = min_drive_gb.saturating_mul(1024 * 1024 * 1024);
    let mut usable = Vec::new();
    let mut too_small = Vec::new();
    for d in all.into_iter().filter(|d| !d.sealed) {
        if d.total_bytes >= min_bytes {
            usable.push(d);
        } else {
            too_small.push(d);
        }
    }
    (usable, too_small)
}

/// 当前在线、未封盘、容量达标的备份盘列表（不打日志、不挑唯一）。
/// `run_plan` 在执行前用它复验「单盘不变式」——`pick_active` 的多盘检查只在 `plan()` 跑过一次，
/// 预览→执行之间若插入第二块可写盘，需在这里重新拦下，否则单盘安全闸被绕过。(SEC-007)
pub(crate) fn usable_drives_now(min_drive_gb: u64) -> Result<Vec<DriveInfo>> {
    let (usable, _too_small) = usable_drives(scan_mounted(None)?, min_drive_gb);
    Ok(usable)
}

/// 返回唯一一块未封盘且容量达标的备份盘；多块返回错误；零块返回 None。
/// 容量过滤(min_drive_gb)是防误抓 U 盘/SD 卡的安全闸。(ledger L-006)
pub(crate) fn pick_active(min_drive_gb: u64, reporter: &dyn Reporter) -> Result<Option<DriveInfo>> {
    // min_drive_gb=0 会使容量闸 total_bytes >= 0 恒真 → 禁用『防误抓 U 盘/SD 卡』安全闸。
    // 在选盘这步(消费该闸、且有 reporter)显式提醒,不让安全闸被静默关闭。(review-r3 round2)
    if min_drive_gb == 0 {
        reporter.warn(
            "min_drive_gb=0 已禁用『防误抓 U 盘/SD 卡』的最小容量闸 —— 任何未封盘的已初始化盘\
             (含小容量介质)都可能被选为可写备份盘。如非有意,请把配置 min_drive_gb 调回正值(默认 200)。",
        );
    }
    let (usable, too_small) = usable_drives(scan_mounted(Some(reporter))?, min_drive_gb);
    for d in &too_small {
        reporter.warn(&format!(
            "忽略疑似过小的盘 {} ({}:) {:.0}GB(低于最小 {}GB)—— 防误抓 U 盘/SD 卡。\
             若确需用它,把配置 min_drive_gb 调低后重试。",
            d.id,
            d.letter,
            d.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
            min_drive_gb
        ));
    }
    crate::pipeline::stages::SingleWritableGuard::check(&usable)?;
    Ok(usable.into_iter().next())
}

pub(crate) fn drive_letter_of(p: &Path) -> Option<String> {
    // 字符安全:对 UNC(\\server)、卷 GUID、多字节首字符的挂载点返回 None 而非 panic。
    // 旧实现 `&s[1..2]`/`s[..1]` 按字节切片,首字符是多字节字符时会 panic。(ledger L-023)
    let s = p.to_string_lossy();
    let mut it = s.chars();
    let first = it.next()?;
    if first.is_ascii_alphabetic() && it.next() == Some(':') {
        Some(first.to_ascii_uppercase().to_string())
    } else {
        None
    }
}

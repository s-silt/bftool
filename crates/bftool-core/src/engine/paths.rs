//! 盘内固定路径常量与系统路径构造。
//!
//! 用中文目录名是和 PowerShell 旧版兼容的需要；同时也是用户已经形成的认知约定。

use std::path::{Path, PathBuf};

// 备份盘内布局
pub const DRIVE_INFO_DIR: &str = "本盘信息";
pub const DRIVE_ID_FILE: &str = "本盘编号.txt";
pub const DRIVE_README_FILE: &str = "本盘说明.txt";
pub const DRIVE_CATALOG_FILE: &str = "本盘索引记录.csv";
pub const DRIVE_SEALED_FILE: &str = "已封盘.txt";
pub const DRIVE_LOGS_DIR: &str = "日志";
pub const DRIVE_MANIFEST_DIR: &str = "校验清单";
pub const DRIVE_QUARANTINE_DIR: &str = "异常文件";
pub const DRIVE_PROJECTS_DIR: &str = "项目";

// 系统目录（SSD 上的 system_root）
pub const SYSTEM_LOGS_DIR: &str = "日志";
pub const GLOBAL_CATALOG_FILE: &str = "备份索引名单.csv";
pub const PENDING_TXN_FILE: &str = "进行中事务.txt";
pub const NEED_MANUAL_FILE: &str = "需人工处理.txt";
pub const DRIVE_SEQ_FILE: &str = "盘号计数.txt";
pub const VERIFY_LOG_FILE: &str = "复查记录.csv";

pub fn drive_info_dir(drive_root: &Path) -> PathBuf {
    drive_root.join(DRIVE_INFO_DIR)
}

pub fn drive_id_path(drive_root: &Path) -> PathBuf {
    drive_info_dir(drive_root).join(DRIVE_ID_FILE)
}

pub fn drive_sealed_path(drive_root: &Path) -> PathBuf {
    drive_info_dir(drive_root).join(DRIVE_SEALED_FILE)
}

pub fn drive_catalog_path(drive_root: &Path) -> PathBuf {
    drive_info_dir(drive_root).join(DRIVE_CATALOG_FILE)
}

pub fn drive_manifest_dir(drive_root: &Path) -> PathBuf {
    drive_info_dir(drive_root).join(DRIVE_MANIFEST_DIR)
}

pub fn drive_projects_dir(drive_root: &Path) -> PathBuf {
    drive_root.join(DRIVE_PROJECTS_DIR)
}

pub fn drive_logs_dir(drive_root: &Path) -> PathBuf {
    drive_info_dir(drive_root).join(DRIVE_LOGS_DIR)
}

pub fn drive_quarantine_dir(drive_root: &Path) -> PathBuf {
    drive_info_dir(drive_root).join(DRIVE_QUARANTINE_DIR)
}

pub fn system_global_catalog(system_root: &Path) -> PathBuf {
    system_root.join(GLOBAL_CATALOG_FILE)
}

pub fn system_pending_txn(system_root: &Path) -> PathBuf {
    system_root.join(PENDING_TXN_FILE)
}

pub fn system_need_manual(system_root: &Path) -> PathBuf {
    system_root.join(NEED_MANUAL_FILE)
}

pub fn system_drive_seq(system_root: &Path) -> PathBuf {
    system_root.join(DRIVE_SEQ_FILE)
}

pub fn system_logs_dir(system_root: &Path) -> PathBuf {
    system_root.join(SYSTEM_LOGS_DIR)
}

pub fn system_verify_log(system_root: &Path) -> PathBuf {
    system_root.join(VERIFY_LOG_FILE)
}

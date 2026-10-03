//! 盘池：扫描 / 选可写盘 / 单可写不变式。

pub(crate) mod scan;

use crate::channel::LocalVolume;
use crate::observe::EventSink;
use crate::reporter::Reporter;

/// Typed H5 rejection; presentation text is never used as a security predicate.
#[derive(Debug, Clone)]
pub struct MultipleWritableDrives {
    pub drives: Vec<String>,
}
impl std::fmt::Display for MultipleWritableDrives {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "检测到多块未封盘的备份盘：{} —— 为防止写错盘已停止。请只保留一块在线。",
            self.drives.join(", ")
        )
    }
}
impl std::error::Error for MultipleWritableDrives {}

#[derive(Debug, Clone)]
pub struct PoolPolicy {
    pub min_drive_gb: u64,
}

impl PoolPolicy {
    pub fn from_min_gb(min_drive_gb: u64) -> Self {
        Self { min_drive_gb }
    }
}

pub trait DrivePool: Send + Sync {
    fn scan(&self, sink: Option<&dyn EventSink>) -> anyhow::Result<Vec<LocalVolume>>;
    fn by_letter(&self, letter: &str) -> anyhow::Result<LocalVolume>;
    fn pick_writable(
        &self,
        policy: &PoolPolicy,
        reporter: &dyn Reporter,
    ) -> anyhow::Result<Option<LocalVolume>>;
    fn usable_now(&self, policy: &PoolPolicy) -> anyhow::Result<Vec<LocalVolume>>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalDrivePool;

impl DrivePool for LocalDrivePool {
    fn scan(&self, _sink: Option<&dyn EventSink>) -> anyhow::Result<Vec<LocalVolume>> {
        let drives = scan::scan_mounted(None)?;
        Ok(drives.iter().map(LocalVolume::from_drive_info).collect())
    }

    fn by_letter(&self, letter: &str) -> anyhow::Result<LocalVolume> {
        let facts = crate::engine::drive::info_by_letter(letter)?;
        Ok(LocalVolume::from_drive_info(&facts))
    }

    fn pick_writable(
        &self,
        policy: &PoolPolicy,
        reporter: &dyn Reporter,
    ) -> anyhow::Result<Option<LocalVolume>> {
        Ok(scan::pick_active(policy.min_drive_gb, reporter)?
            .map(|d| LocalVolume::from_drive_info(&d)))
    }

    fn usable_now(&self, policy: &PoolPolicy) -> anyhow::Result<Vec<LocalVolume>> {
        Ok(scan::usable_drives_now(policy.min_drive_gb)?
            .iter()
            .map(LocalVolume::from_drive_info)
            .collect())
    }
}

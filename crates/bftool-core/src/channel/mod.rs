//! 目标介质抽象：只报告事实，不发硬闸。

mod local_volume;

pub use local_volume::LocalVolume;

use std::path::{Path, PathBuf};

use crate::domain::{Capacity, DiskOccupancy, DriveId, SealState};

/// 目标介质能力。P1 仅 [`LocalVolume`]。
pub trait Channel: Send + Sync {
    fn root(&self) -> &Path;
    fn letter(&self) -> &str;
    fn drive_id(&self) -> Option<&DriveId>;
    fn seal_state(&self) -> SealState;
    fn capacity(&self) -> Capacity;
    /// 忽略 cruft 后的根占用；供 EmptyDiskHint；**不得**据此 Deny。
    fn occupancy(&self) -> anyhow::Result<DiskOccupancy>;
    fn is_initialized(&self) -> bool;
    fn projects_dir(&self) -> PathBuf;
    fn info_dir(&self) -> PathBuf;

    // Initialization is a service/pipeline operation with configuration, locking
    // and safety checks. A read-only channel must not advertise a bypass write API.
}

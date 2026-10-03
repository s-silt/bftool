//! 本地卷：包装盘符根 + 可选缓存的 DriveInfo 字段。

use std::path::{Path, PathBuf};

use crate::channel::Channel;
use crate::domain::{Capacity, DiskOccupancy, DriveId, SealState};
use crate::engine::drive;
use crate::engine::paths;

/// 本地备份/候选卷。
#[derive(Debug, Clone)]
pub struct LocalVolume {
    pub letter: String,
    pub root: PathBuf,
    id: Option<DriveId>,
    free_bytes: u64,
    total_bytes: u64,
    sealed: SealState,
}

impl LocalVolume {
    pub fn new(letter: impl Into<String>, root: PathBuf) -> Self {
        let letter = letter.into();
        let id = read_id(&root);
        let sealed = if drive::drive_is_sealed(&root) {
            SealState::Sealed
        } else {
            SealState::Open
        };
        Self {
            letter,
            root,
            id,
            free_bytes: 0,
            total_bytes: 0,
            sealed,
        }
    }

    /// 从已扫描的 [`drive::DriveInfo`] 构造。
    pub fn from_drive_info(d: &drive::DriveInfo) -> Self {
        Self {
            letter: d.letter.clone(),
            root: d.root.clone(),
            id: Some(DriveId(d.id.clone())),
            free_bytes: d.free_bytes,
            total_bytes: d.total_bytes,
            sealed: if d.sealed {
                SealState::Sealed
            } else {
                SealState::Open
            },
        }
    }

    /// Preserve the scanned identity when bridging the execution contract.
    pub fn into_drive_info(self) -> anyhow::Result<drive::DriveInfo> {
        let id = self
            .id
            .ok_or_else(|| anyhow::anyhow!("目标介质尚未初始化"))?;
        Ok(drive::DriveInfo {
            letter: self.letter,
            root: self.root,
            id: id.0,
            free_bytes: self.free_bytes,
            total_bytes: self.total_bytes,
            sealed: self.sealed == SealState::Sealed,
        })
    }

    pub fn with_capacity(mut self, free: u64, total: u64) -> Self {
        self.free_bytes = free;
        self.total_bytes = total;
        self
    }
}

fn read_id(root: &Path) -> Option<DriveId> {
    std::fs::read_to_string(paths::drive_id_path(root))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(DriveId)
}

impl Channel for LocalVolume {
    fn root(&self) -> &Path {
        &self.root
    }

    fn letter(&self) -> &str {
        &self.letter
    }

    fn drive_id(&self) -> Option<&DriveId> {
        self.id.as_ref()
    }

    fn seal_state(&self) -> SealState {
        self.sealed
    }

    fn capacity(&self) -> Capacity {
        Capacity {
            free_bytes: self.free_bytes,
            total_bytes: self.total_bytes,
        }
    }

    fn occupancy(&self) -> anyhow::Result<DiskOccupancy> {
        if self.is_initialized() {
            return Ok(DiskOccupancy::BackupOccupied);
        }
        if drive::root_is_empty(&self.root)? {
            Ok(DiskOccupancy::Empty)
        } else {
            Ok(DiskOccupancy::NonEmpty)
        }
    }

    fn is_initialized(&self) -> bool {
        self.id.is_some() || paths::drive_id_path(&self.root).is_file()
    }

    fn projects_dir(&self) -> PathBuf {
        paths::drive_projects_dir(&self.root)
    }

    fn info_dir(&self) -> PathBuf {
        paths::drive_info_dir(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Channel;

    #[test]
    fn occupancy_non_empty_is_not_deny_key() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("user.txt"), b"x").unwrap();
        let vol = LocalVolume::new("T", d.path().to_path_buf());
        assert_eq!(vol.occupancy().unwrap(), DiskOccupancy::NonEmpty);
        // Channel 只报告事实；Deny 不在此处。
    }

    #[test]
    fn occupancy_empty_when_only_cruft() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("desktop.ini"), b"x").unwrap();
        let vol = LocalVolume::new("T", d.path().to_path_buf());
        assert_eq!(vol.occupancy().unwrap(), DiskOccupancy::Empty);
    }
}

//! 纯领域类型（盘身份、封盘态、容量、占用）。不含 IO。

use std::path::PathBuf;

/// 本盘编号，例如「备份3」。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DriveId(pub String);

impl DriveId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DriveId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for DriveId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for DriveId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealState {
    Open,
    Sealed,
    /// stat 失败等：保守当已封盘。
    UnknownTreatedAsSealed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capacity {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

impl Capacity {
    pub fn total_gb(self) -> u64 {
        self.total_bytes / (1024 * 1024 * 1024)
    }
}

/// 盘根占用。**任何变体都不得映射为 StageOutcome::Deny / bail。**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskOccupancy {
    Empty,
    NonEmpty,
    /// 已是备份盘（`\项目\` 可能有仓位）—— 允许操作。
    BackupOccupied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveRef {
    pub letter: String,
    pub root: PathBuf,
    pub id: Option<DriveId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupancy_variants_exist_for_soft_hint_only() {
        // 契约：三态都存在；策略层不得把它们当硬拒键。
        assert_ne!(DiskOccupancy::Empty, DiskOccupancy::NonEmpty);
        assert_ne!(DiskOccupancy::NonEmpty, DiskOccupancy::BackupOccupied);
    }
}

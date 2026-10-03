//! 归档 API facade（P1：实现迁至 [`crate::pipeline::archive`]）。
//!
//! CLI/GUI 继续 `use bftool_core::engine::archive::*`；内部符号不变。

pub use crate::pipeline::archive::{
    incremental_forbids_no_hash, plan, plan_on_drive, run, run_plan, verify_disabled, ArchivePlan,
    ArchiveSummary, FolderStats, Options, PlanAction, PlanItem,
};

pub(crate) use crate::pipeline::archive::ArchiveLock;

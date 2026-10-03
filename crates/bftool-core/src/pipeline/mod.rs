//! 操作管道：阶段化硬闸 / 软提示 + archive 切片。

pub mod archive;
pub mod backup;
pub mod init;
pub mod stages;
pub mod watch;

use crate::config::Config;
use crate::observe::Event;
use crate::reporter::Reporter;
use crate::service::request::OperationRequest;

/// 阶段裁决。`SoftHint` **永不**升级为 `Deny`；occupancy 不得出现在 Deny 理由里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    Continue,
    SoftHint { event: Event },
    Deny { event: Event, message: String },
    Cancelled,
}

pub struct StageCtx<'a> {
    pub cfg: Option<&'a Config>,
    pub request: &'a OperationRequest,
    pub reporter: &'a dyn Reporter,
    pub sink: &'a dyn crate::observe::EventSink,
    pub init: Option<&'a mut InitStageData>,
}

#[derive(Debug, Clone)]
pub struct InitStageData {
    pub letter: String,
    pub is_system: bool,
    pub is_library: bool,
    pub already_backup: bool,
    pub non_empty: bool,
    pub force_system: bool,
    pub force_library: bool,
}

pub trait PipelineStage: Send + Sync {
    fn name(&self) -> &'static str;
    fn run(&self, ctx: &mut StageCtx<'_>) -> anyhow::Result<StageOutcome>;
}

pub fn run_stages(stages: &[&dyn PipelineStage], ctx: &mut StageCtx<'_>) -> anyhow::Result<()> {
    for stage in stages {
        match stage.run(ctx)? {
            StageOutcome::Continue => {}
            StageOutcome::SoftHint { .. } => {}
            StageOutcome::Deny { message, .. } => anyhow::bail!("{message}"),
            StageOutcome::Cancelled => anyhow::bail!("操作已取消"),
        }
    }
    Ok(())
}

/// Archive 入口预检：PathSafety + HashPolicy（单可写在选盘时由 pool/pick_active 强制）。
pub fn run_archive_preflight(
    cfg: &Config,
    opts: &archive::Options,
    reporter: &dyn Reporter,
    sink: &dyn crate::observe::EventSink,
) -> anyhow::Result<()> {
    use stages::{hash_policy, path_safety};
    match path_safety::run_path_safety(cfg, reporter, sink, None)? {
        StageOutcome::Deny { message, .. } => anyhow::bail!("{message}"),
        StageOutcome::Cancelled => anyhow::bail!("操作已取消"),
        _ => {}
    }
    match hash_policy::run_hash_policy(cfg, opts, sink)? {
        StageOutcome::Deny { message, .. } => anyhow::bail!("{message}"),
        StageOutcome::Cancelled => anyhow::bail!("操作已取消"),
        _ => {}
    }
    Ok(())
}

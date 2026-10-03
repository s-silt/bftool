//! Init 管道：SystemDrive → LibraryDrive → EmptyDiskHint → 业务写入。

use crate::config::Config;
use crate::observe::EventSink;
use crate::pipeline::stages::{EmptyDiskHint, LibraryDriveGuard, SystemDriveGuard};
use crate::pipeline::{run_stages, InitStageData, StageCtx};
use crate::reporter::Reporter;
use crate::service::request::{InitRequest, OperationRequest};

/// 跑 Init 硬/软阶段（不含元数据写入）。供 `drive::init` / `service::run` 共用。
pub fn run_init_gates(
    mut data: InitStageData,
    req: &OperationRequest,
    reporter: &dyn Reporter,
    sink: &dyn EventSink,
) -> anyhow::Result<()> {
    let mut ctx = StageCtx {
        cfg: None,
        request: req,
        reporter,
        sink,
        init: Some(&mut data),
    };
    let stages: [&dyn crate::pipeline::PipelineStage; 3] =
        [&SystemDriveGuard, &LibraryDriveGuard, &EmptyDiskHint];
    run_stages(&stages, &mut ctx)?;

    if data.already_backup {
        reporter.warn(&format!(
            "{}: 已经是一块初始化过的备份盘；将覆盖元数据，但不会动 \\项目\\ 下的数据。",
            data.letter
        ));
    }
    Ok(())
}

/// 完整 Init：分类与写入共用 typed 请求，分别保留两种硬闸的授权。
pub fn run_init(
    cfg: &Config,
    req: &InitRequest,
    reporter: &dyn Reporter,
    sink: &dyn EventSink,
) -> anyhow::Result<()> {
    crate::engine::drive::init_with_request(cfg, reporter, req, sink)
}

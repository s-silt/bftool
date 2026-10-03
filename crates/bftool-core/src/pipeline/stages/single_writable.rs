//! H5：单可写盘不变式（委托 pool::pick_writable / 与 drive::pick_active 同判据）。

use crate::observe::Event;
use crate::pipeline::{PipelineStage, StageCtx, StageOutcome};
use crate::pool::{DrivePool, LocalDrivePool, PoolPolicy};
use crate::service::request::OperationRequest;

pub struct SingleWritableGuard;

impl SingleWritableGuard {
    /// Pure gate over a single scan snapshot, shared by real pool selection.
    pub(crate) fn check(drives: &[crate::engine::drive::DriveInfo]) -> anyhow::Result<()> {
        if drives.len() > 1 {
            return Err(crate::pool::MultipleWritableDrives {
                drives: drives
                    .iter()
                    .map(|d| format!("{}({}:)", d.id, d.letter))
                    .collect(),
            }
            .into());
        }
        Ok(())
    }
}

impl PipelineStage for SingleWritableGuard {
    fn name(&self) -> &'static str {
        "SingleWritableGuard"
    }

    fn run(&self, ctx: &mut StageCtx<'_>) -> anyhow::Result<StageOutcome> {
        if !matches!(ctx.request, OperationRequest::Archive(_)) {
            return Ok(StageOutcome::Continue);
        }
        let Some(cfg) = ctx.cfg else {
            return Ok(StageOutcome::Continue);
        };
        run_single_writable(cfg.min_drive_gb, ctx.reporter, ctx.sink)
    }
}

pub fn run_single_writable(
    min_drive_gb: u64,
    reporter: &dyn crate::reporter::Reporter,
    sink: &dyn crate::observe::EventSink,
) -> anyhow::Result<StageOutcome> {
    let pool = LocalDrivePool;
    let policy = PoolPolicy::from_min_gb(min_drive_gb);
    match pool.pick_writable(&policy, reporter) {
        Ok(_) => Ok(StageOutcome::Continue),
        Err(e) => {
            if let Some(denial) = e.downcast_ref::<crate::pool::MultipleWritableDrives>() {
                let msg = denial.to_string();
                let event = Event::DenyMultiWritable {
                    drives: denial.drives.clone(),
                };
                sink.emit(event.clone());
                Ok(StageOutcome::Deny {
                    event,
                    message: msg,
                })
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::drive::DriveInfo;
    fn drive(id: &str) -> DriveInfo {
        DriveInfo {
            letter: id.to_string(),
            root: std::path::PathBuf::from(id),
            id: id.to_string(),
            free_bytes: 1024,
            total_bytes: 2048,
            sealed: false,
        }
    }
    #[test]
    fn h5_uses_typed_rejection_and_single_snapshot() {
        assert!(SingleWritableGuard::check(&[]).is_ok());
        assert!(SingleWritableGuard::check(&[drive("A")]).is_ok());
        let error = SingleWritableGuard::check(&[drive("A"), drive("B")]).unwrap_err();
        let denied = error
            .downcast_ref::<crate::pool::MultipleWritableDrives>()
            .unwrap();
        assert_eq!(denied.drives.len(), 2);
    }
}

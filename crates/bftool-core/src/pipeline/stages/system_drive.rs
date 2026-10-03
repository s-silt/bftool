//! H1：系统盘硬闸。

use crate::observe::Event;
use crate::pipeline::{PipelineStage, StageCtx, StageOutcome};

pub struct SystemDriveGuard;

impl PipelineStage for SystemDriveGuard {
    fn name(&self) -> &'static str {
        "SystemDriveGuard"
    }

    fn run(&self, ctx: &mut StageCtx<'_>) -> anyhow::Result<StageOutcome> {
        let Some(init) = ctx.init.as_ref() else {
            return Ok(StageOutcome::Continue);
        };
        if !init.is_system {
            return Ok(StageOutcome::Continue);
        }
        if init.force_system {
            ctx.reporter.warn(&format!(
                "{}: --force 绕过系统盘硬闸；风险自负。",
                init.letter
            ));
            return Ok(StageOutcome::Continue);
        }
        let event = Event::DenySystemDrive {
            letter: init.letter.clone(),
        };
        ctx.sink.emit(event.clone());
        Ok(StageOutcome::Deny {
            event,
            message: format!("拒绝初始化系统盘 {}:（如确需，请加 --force）", init.letter),
        })
    }
}

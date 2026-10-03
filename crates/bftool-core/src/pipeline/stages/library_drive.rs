//! H2：资料库盘硬闸。

use crate::observe::Event;
use crate::pipeline::{PipelineStage, StageCtx, StageOutcome};

pub struct LibraryDriveGuard;

impl PipelineStage for LibraryDriveGuard {
    fn name(&self) -> &'static str {
        "LibraryDriveGuard"
    }

    fn run(&self, ctx: &mut StageCtx<'_>) -> anyhow::Result<StageOutcome> {
        let Some(init) = ctx.init.as_ref() else {
            return Ok(StageOutcome::Continue);
        };
        if !init.is_library {
            return Ok(StageOutcome::Continue);
        }
        if init.force_library {
            ctx.reporter.warn(&format!(
                "{}: --force 绕过资料库盘硬闸；风险自负。",
                init.letter
            ));
            return Ok(StageOutcome::Continue);
        }
        let event = Event::DenyLibraryDrive {
            letter: init.letter.clone(),
        };
        ctx.sink.emit(event.clone());
        Ok(StageOutcome::Deny {
            event,
            message: format!(
                "拒绝初始化资料库所在盘 {}:（待备份/已备份/备份系统 在此盘；如确需，请加 --force）",
                init.letter
            ),
        })
    }
}

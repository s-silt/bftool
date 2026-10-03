//! H3/H4：路径互斥/嵌套 + 源在备份盘（委托 `engine::safety::check_paths`）。

use crate::config::Config;
use crate::observe::Event;
use crate::pipeline::{PipelineStage, StageCtx, StageOutcome};
use crate::service::request::OperationRequest;

pub struct PathSafety;

impl PipelineStage for PathSafety {
    fn name(&self) -> &'static str {
        "PathSafety"
    }

    fn run(&self, ctx: &mut StageCtx<'_>) -> anyhow::Result<StageOutcome> {
        // Init 不跑三根路径检查。
        if matches!(ctx.request, OperationRequest::Init(_)) {
            return Ok(StageOutcome::Continue);
        }
        let Some(cfg) = ctx.cfg else {
            return Ok(StageOutcome::Continue);
        };
        run_path_safety(cfg, ctx.reporter, ctx.sink, None)
    }
}

/// 供 archive plan/run 直接调用（可带当前目标盘根）。
pub fn run_path_safety(
    cfg: &Config,
    reporter: &dyn crate::reporter::Reporter,
    sink: &dyn crate::observe::EventSink,
    drive_root: Option<&std::path::Path>,
) -> anyhow::Result<StageOutcome> {
    match crate::engine::safety::check_paths(
        &cfg.ready_root,
        &cfg.archived_root,
        &cfg.system_root,
        drive_root,
    ) {
        Ok(warnings) => {
            for w in warnings {
                reporter.warn(&w);
            }
            Ok(StageOutcome::Continue)
        }
        Err(e) => {
            let message = format!("{e:#}");
            let event = Event::DenyPathSafety {
                message: message.clone(),
            };
            sink.emit(event.clone());
            Ok(StageOutcome::Deny { event, message })
        }
    }
}

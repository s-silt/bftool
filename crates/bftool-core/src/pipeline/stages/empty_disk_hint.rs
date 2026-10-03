//! 空盘/非空 **软提示** —— 无否决权。永不返回 Deny。

use crate::observe::Event;
use crate::pipeline::{PipelineStage, StageCtx, StageOutcome};
use crate::service::request::OperationRequest;

pub struct EmptyDiskHint;

impl PipelineStage for EmptyDiskHint {
    fn name(&self) -> &'static str {
        "EmptyDiskHint"
    }

    fn run(&self, ctx: &mut StageCtx<'_>) -> anyhow::Result<StageOutcome> {
        let Some(init) = ctx.init.as_mut() else {
            return Ok(StageOutcome::Continue);
        };
        // 仅 Init 关心；已是备份盘用已有 warn，不重复 occupancy SoftHint。
        if init.already_backup || !init.non_empty {
            return Ok(StageOutcome::Continue);
        }

        let event = Event::HintEmptyDisk {
            letter: init.letter.clone(),
        };
        ctx.sink.emit(event.clone());
        ctx.reporter.warn(&format!(
            "{}: 根目录非空，请确认目标盘无误（不是系统盘/资料库盘）；已有数据不会被清洗，init 只写本盘约定目录。",
            init.letter
        ));

        // 旧 --force 对非空：no-op + warn（DESIGN §6）
        if matches!(
            ctx.request,
            OperationRequest::Init(r) if r.force_system || r.force_library
        ) {
            // 仅当调用方把旧 force 打开时提示语义收窄；不阻断。
            ctx.reporter.info(
                "--force 已不再用于「允许非空」（非空默认允许）；此处仅保留对系统盘/资料库硬闸的绕过语义。",
            );
        }

        Ok(StageOutcome::SoftHint { event })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::{EventSink, ReporterSink};
    use crate::pipeline::InitStageData;
    use crate::reporter::NoopReporter;
    use crate::service::request::{InitRequest, OperationRequest};
    use std::sync::Mutex;

    struct Rec(Mutex<Vec<Event>>);
    impl EventSink for Rec {
        fn emit(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[test]
    fn non_empty_returns_soft_hint_never_deny() {
        let reporter = NoopReporter;
        let sink = Rec(Mutex::new(Vec::new()));
        let req = OperationRequest::Init(InitRequest {
            drive_letter: "E".into(),
            id: None,
            force_system: false,
            force_library: false,
        });
        let mut data = InitStageData {
            letter: "E".into(),
            is_system: false,
            is_library: false,
            already_backup: false,
            non_empty: true,
            force_system: false,
            force_library: false,
        };
        let mut ctx = StageCtx {
            cfg: None,
            request: &req,
            reporter: &reporter,
            sink: &sink,
            init: Some(&mut data),
        };
        let out = EmptyDiskHint.run(&mut ctx).unwrap();
        match out {
            StageOutcome::SoftHint {
                event: Event::HintEmptyDisk { .. },
            } => {}
            other => panic!("expected SoftHint, got {:?}", other),
        }
        assert!(sink.0.lock().unwrap().iter().any(|e| matches!(
            e,
            Event::HintEmptyDisk { letter } if letter == "E"
        )));
        let _ = ReporterSink {
            reporter: &reporter,
        };
    }
}

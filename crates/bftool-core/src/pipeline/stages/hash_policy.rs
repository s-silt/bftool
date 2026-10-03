//! H6：校验全关硬闸；H7/Q1：incremental/watch 禁止对拷贝项跳过 SHA256。

use crate::config::Config;
use crate::observe::Event;
use crate::pipeline::archive::{self, incremental_forbids_no_hash, Options};
use crate::pipeline::{PipelineStage, StageCtx, StageOutcome};
use crate::service::request::OperationRequest;

pub struct HashPolicyGuard;

impl PipelineStage for HashPolicyGuard {
    fn name(&self) -> &'static str {
        "HashPolicyGuard"
    }

    fn run(&self, ctx: &mut StageCtx<'_>) -> anyhow::Result<StageOutcome> {
        let OperationRequest::Archive(a) = ctx.request else {
            return Ok(StageOutcome::Continue);
        };
        let Some(cfg) = ctx.cfg else {
            return Ok(StageOutcome::Continue);
        };
        // ArchiveRequest.incremental 优先；Options.incremental 为合并后真值
        let mut opts = a.options.clone();
        if a.incremental {
            opts.incremental = true;
        }
        run_hash_policy(cfg, &opts, ctx.sink)
    }
}

pub fn run_hash_policy(
    cfg: &Config,
    opts: &Options,
    sink: &dyn crate::observe::EventSink,
) -> anyhow::Result<StageOutcome> {
    // H6
    if let Err(e) = archive::item::guard_verify_enabled(cfg, opts) {
        let event = Event::DenyHashAllOff;
        sink.emit(event.clone());
        return Ok(StageOutcome::Deny {
            event,
            message: format!("{e:#}"),
        });
    }
    // H7/Q1：incremental + no_hash → 硬拒（即使压缩包测试开着也不算「内容校验」替代）
    // --force 不得绕过（本函数无 force 参数）。
    if incremental_forbids_no_hash(opts.incremental, opts.no_hash) {
        let event = Event::DenyVerifySkipWatch;
        let message = event.message();
        sink.emit(event.clone());
        return Ok(StageOutcome::Deny { event, message });
    }
    Ok(StageOutcome::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::EventSink;
    use std::sync::Mutex;

    struct Rec(Mutex<Vec<Event>>);
    impl EventSink for Rec {
        fn emit(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[test]
    fn h7_incremental_no_hash_denied() {
        let cfg = Config::default();
        let opts = Options {
            incremental: true,
            no_hash: true,
            ..Options::default()
        };
        let sink = Rec(Mutex::new(Vec::new()));
        let out = run_hash_policy(&cfg, &opts, &sink).unwrap();
        assert!(matches!(
            out,
            StageOutcome::Deny {
                event: Event::DenyVerifySkipWatch,
                ..
            }
        ));
        assert_eq!(sink.0.lock().unwrap()[0].code(), "deny.verify_skip_watch");
    }

    #[test]
    fn h7_skipped_unchanged_path_allows_hash_on() {
        // incremental + SHA256 on → Continue（skipped_unchanged 是计划层 Skip，不经本闸否决）
        let cfg = Config::default();
        let opts = Options {
            incremental: true,
            no_hash: false,
            ..Options::default()
        };
        let sink = Rec(Mutex::new(Vec::new()));
        let out = run_hash_policy(&cfg, &opts, &sink).unwrap();
        assert_eq!(out, StageOutcome::Continue);
    }

    #[test]
    fn h6_still_denies_all_off_without_incremental() {
        let cfg = Config {
            test_archives: false,
            ..Config::default()
        };
        let opts = Options {
            no_hash: true,
            incremental: false,
            ..Options::default()
        };
        let sink = Rec(Mutex::new(Vec::new()));
        let out = run_hash_policy(&cfg, &opts, &sink).unwrap();
        assert!(matches!(
            out,
            StageOutcome::Deny {
                event: Event::DenyHashAllOff,
                ..
            }
        ));
    }
}

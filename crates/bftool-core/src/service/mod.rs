//! 控制面入口：`run(OperationRequest)` 统一分发。

pub mod request;

pub use request::{
    ArchiveRequest, FileFilter, FindRequest, InitRequest, OperationRequest, OperationResult,
    SourceSpec, VerifyRequest, WatchRequest,
};

use crate::config::Config;
use crate::engine::drive::{self, DriveInfo, InitCandidate};
use crate::engine::find::{self, FindOutcome};
use crate::engine::verify::{self, VerifyReport};
use crate::observe::{EventSink, ReporterSink};
use crate::pipeline::archive::{self, ArchivePlan, ArchiveSummary, Options};
use crate::reporter::Reporter;
use std::sync::atomic::AtomicBool;

/// 统一数据面入口（无 cancel；CLI 默认路径）。
pub fn run(
    cfg: &Config,
    req: OperationRequest,
    reporter: &dyn Reporter,
    sink: &dyn EventSink,
) -> anyhow::Result<OperationResult> {
    let cancel = AtomicBool::new(false);
    match req {
        OperationRequest::Init(init) => {
            crate::pipeline::init::run_init(cfg, &init, reporter, sink)?;
            Ok(OperationResult::Ok)
        }
        OperationRequest::Archive(a) => {
            let opts = a.merged_options();
            let summary = archive::run(cfg, reporter, opts, &cancel)?;
            Ok(OperationResult::Archive { summary })
        }
        OperationRequest::Watch(w) => {
            let summary = crate::pipeline::watch::run(cfg, &w, reporter, &cancel)?;
            Ok(OperationResult::Watch {
                cycles: summary.cycles,
                archived: summary.archived,
                skipped_unchanged: summary.skipped_unchanged,
                failed: summary.failed,
            })
        }
        OperationRequest::Verify(v) => {
            let report = verify::run(cfg, reporter, v.drive.as_deref(), &cancel)?;
            if report.has_corruption() {
                return Ok(OperationResult::HardFail {
                    message: format!(
                        "复查发现 {} 处完整性问题（损坏/缺失/大小不符/读取失败等）",
                        report.bad
                    ),
                });
            }
            Ok(OperationResult::Verify { report })
        }
        OperationRequest::Find(f) => {
            find::run(cfg, &f.keyword)?;
            Ok(OperationResult::Ok)
        }
        OperationRequest::Drives => {
            drive::list_mounted(cfg, reporter)?;
            Ok(OperationResult::Ok)
        }
        OperationRequest::Status => {
            crate::engine::status::run(cfg, reporter)?;
            Ok(OperationResult::Ok)
        }
    }
}

pub fn run_with_reporter(
    cfg: &Config,
    req: OperationRequest,
    reporter: &dyn Reporter,
) -> anyhow::Result<OperationResult> {
    let sink = ReporterSink { reporter };
    run(cfg, req, reporter, &sink)
}

/// 归档（带取消）—— GUI / 精细 CLI。
pub fn run_archive(
    cfg: &Config,
    options: Options,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> anyhow::Result<ArchiveSummary> {
    archive::run(cfg, reporter, options, cancel)
}

pub fn plan_archive(
    cfg: &Config,
    options: &Options,
    reporter: &dyn Reporter,
) -> anyhow::Result<ArchivePlan> {
    archive::plan(cfg, options, reporter)
}

pub fn run_archive_plan(
    cfg: &Config,
    plan: &ArchivePlan,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> anyhow::Result<ArchiveSummary> {
    archive::run_plan(cfg, plan, cancel, reporter)
}

pub fn run_verify(
    cfg: &Config,
    drive: Option<&str>,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> anyhow::Result<VerifyReport> {
    verify::run(cfg, reporter, drive, cancel)
}

pub fn verify_one(
    cfg: &Config,
    target: &std::path::Path,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> anyhow::Result<VerifyReport> {
    verify::verify_one(cfg, reporter, target, cancel)
}

pub fn find_search(cfg: &Config, keyword: &str) -> anyhow::Result<FindOutcome> {
    find::search(cfg, keyword)
}

pub fn scan_drives(reporter: Option<&dyn Reporter>) -> anyhow::Result<Vec<DriveInfo>> {
    drive::scan_mounted(reporter)
}

pub fn list_init_candidates(cfg: &Config) -> anyhow::Result<Vec<InitCandidate>> {
    drive::init_candidates(cfg)
}

pub fn run_init_legacy(
    cfg: &Config,
    letter: &str,
    id: Option<&str>,
    force: bool,
    reporter: &dyn Reporter,
) -> anyhow::Result<()> {
    let req = OperationRequest::Init(InitRequest::from_legacy_force(
        letter.to_string(),
        id.map(|s| s.to_string()),
        force,
    ));
    run_with_reporter(cfg, req, reporter).map(|_| ())
}

//! 归档执行：`run` / `run_plan`。
use anyhow::{Context, Result};
use chrono::Local;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Config;
use crate::engine::drive::{self, DriveInfo};
use crate::engine::{paths, safety};
use crate::reporter::Reporter;

use super::incremental::SKIP_UNCHANGED;
use super::item::{detect_tester, handle_one, HandleOutcome};
use super::lock::{check_pending_txn_for_drive, note_manual};
use super::plan::plan;
use super::plan::{render_dry_run, verified_unchanged};
use super::types::{
    source_relative_key, ArchivePlan, ArchiveSummary, NoWritableDrive, Options, PlanAction,
    PlanItem,
};
use crate::observe::ReporterSink;
use crate::pipeline::stages::hash_policy::run_hash_policy;
use crate::pipeline::StageOutcome;

pub fn run(
    cfg: &Config,
    reporter: &dyn Reporter,
    opts: Options,
    cancel: &AtomicBool,
) -> Result<ArchiveSummary> {
    let plan = match plan(cfg, &opts, reporter) {
        Ok(p) => p,
        // 自动选盘选不出可写盘 = 无事可做(退出码 0,行为不变)。但**显式 --drive 指定了盘却不可写**
        // (封盘/过小/不存在)是用户明确意图落空 → 作为错误上抛(非零退出码),不静默成 0。(review-r2 R5-5)
        Err(e) if e.downcast_ref::<NoWritableDrive>().is_some() => {
            if opts.drive_letter_override.is_some() {
                return Err(e);
            }
            return Ok(ArchiveSummary::default());
        }
        Err(e) => return Err(e),
    };
    if opts.dry_run {
        return Ok(render_dry_run(&plan, reporter));
    }
    run_plan(cfg, &plan, cancel, reporter)
}

/// 进程级归档锁:一个独占创建的锁文件,防多个 bftool 实例并发归档(并发会破坏事务标记/索引)。
/// Drop 时删锁(正常返回与 panic 展开都触发;release 用 panic=unwind 见 R5-8)。(review-r2 R6-4)
/// 系统级跨进程互斥锁(锁文件在 system_root)。归档(run_plan)与初始化(drive::init)共用,
/// 把所有会改动 system_root 状态(事务标记 / 盘号 seq / 全局索引)的操作串行化。(review-r3 round4)
pub(crate) struct ArchiveLock {
    path: PathBuf,
}
impl ArchiveLock {
    pub(crate) fn acquire(system_root: &Path) -> Result<Self> {
        let path = system_root.join(".bftool-archive.lock");
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "pid={} started_at={}",
                    std::process::id(),
                    Local::now().format("%Y-%m-%d %H:%M:%S")
                );
                Ok(ArchiveLock { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => anyhow::bail!(
                "另一个 bftool 操作(归档/初始化)可能正在运行(锁文件存在:{})。请等它完成;\
                 若确认没有其它实例在跑(上次异常退出残留),手动删除该文件后重试。",
                path.display()
            ),
            Err(e) => Err(e).context(format!("创建系统锁失败:{}", path.display())),
        }
    }
}
impl Drop for ArchiveLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `run_plan` 每个项目执行前对盘的实时裁决。
pub(super) enum ItemDriveCheck {
    /// 同一块盘:用该值作为本项目可见剩余(已取 min(投影, 实时))。
    Use(u64),
    /// 盘内编号变了(同盘符被热插拔换了另一块盘)→ 中止本轮。
    AbortSwapped,
    /// 盘在本轮中途被封盘 → 中止本轮。
    AbortSealed,
}

/// 每个项目执行前,按实时盘状态校正可见容量并**复验盘身份**。plan→run 边界(`run_plan` 开头)已验过
/// 一次盘内编号/封盘态,但多项目一轮里,项目与项目之间仍可能被热插拔换盘(同盘符)或中途封盘。本工具
/// 「认盘靠盘内编号、不靠盘符」—— 复用每项本就要做的 `info_by_letter` 实时查询顺带校验身份,编号不符/
/// 被封盘即中止本轮,绝不把后续项目写到一块身份已不可信的盘上(否则数据落到错盘、索引却记成计划盘 id)。
/// 读不到(盘离线)则保守退回投影值,沿用 plan→run 边界其余守卫兜底。纯函数,便于单测覆盖各分支。(强优化 review)
pub(super) fn reconcile_item_drive(
    planned: &DriveInfo,
    consumed: u64,
    live: Option<&DriveInfo>,
) -> ItemDriveCheck {
    let projected = planned.free_bytes.saturating_sub(consumed);
    match live {
        Some(l) if l.id.trim() != planned.id.trim() => ItemDriveCheck::AbortSwapped,
        Some(l) if l.sealed => ItemDriveCheck::AbortSealed,
        Some(l) => ItemDriveCheck::Use(projected.min(l.free_bytes)),
        None => ItemDriveCheck::Use(projected),
    }
}

/// 源根、实际源路径及完整相对身份必须仍绑定同一对象路径。
fn validate_source_identity(cfg: &Config, opts: &Options, item: &PlanItem) -> Result<()> {
    source_relative_key(&item.source_relative)?;
    let configured_root = opts.source_override.as_ref().unwrap_or(&cfg.ready_root);
    let current_root = configured_root
        .canonicalize()
        .context("实际源根已不存在或不可读")?;
    if current_root != item.source_root || !item.source_path.is_absolute() {
        anyhow::bail!("源根与计划不一致");
    }
    let relative = item
        .source_path
        .strip_prefix(&item.source_root)
        .context("实际源不在计划根内")?;
    if relative != item.source_relative {
        anyhow::bail!("源路径与相对身份不一致");
    }
    let current_source = current_root
        .join(&item.source_relative)
        .canonicalize()
        .context("实际源已不存在或不可读")?;
    if current_source != item.source_path {
        anyhow::bail!("源链接或路径在预览后改变");
    }
    Ok(())
}

/// 执行计划:**冻"意图"(目标名/选盘)、不冻"安全判断"**。每项执行前重验受外部状态影响的安全前置——
/// 盘仍在且未封盘(本函数开头按盘上 marker 实时重读)、源仍稳定、容量仍够、目标名仍不冲突——
/// 任一已变 → 该项"计划已过期"跳过,绝不照旧 plan 误归档/误封盘。(Spec D §4.1)
pub fn run_plan(
    cfg: &Config,
    plan: &ArchivePlan,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<ArchiveSummary> {
    let opts = &plan.opts;
    // 公共计划可在预览后被修改；每个执行入口都使用与计划相同的 H6/H7 守卫。
    let sink = ReporterSink { reporter };
    match run_hash_policy(cfg, opts, &sink)? {
        StageOutcome::Deny { message, .. } => anyhow::bail!("{message}"),
        StageOutcome::Cancelled => anyhow::bail!("操作已取消"),
        _ => {}
    }

    // 公开执行入口也遵守演练只读；不能先恢复事务或建立目录再由 item 返回演练。
    if opts.dry_run {
        return Ok(render_dry_run(plan, reporter));
    }

    // 重验盘:预览→执行之间盘可能被拔/被封。按盘上实时状态(id 文件 + 封盘 marker)重读,
    // 不信 plan 里冻结的 sealed/在线态。任一变 → 整份计划过期(不写任何盘)。
    let drive = &plan.drive;
    let id_path = paths::drive_id_path(&drive.root);
    if !id_path.is_file() {
        reporter.error(&format!(
            "计划已过期：备份盘 {} ({}:) 已不在线或未初始化 → 本轮不执行,请重新规划(bftool archive)。",
            drive.id, drive.letter
        ));
        return Ok(ArchiveSummary::default());
    }
    // R2-1:不仅查 id 文件**存在**,还要比对**内容**(盘内编号)。预览→执行之间若换上另一块
    // 备份盘且 Windows 复用了同一盘符,只查存在会通过 → 数据写到实际盘却记成计划盘的 id。
    // 「认盘靠盘内编号不靠盘符」(drive.rs)—— 编号不符即计划过期,中止本轮、不写任何盘。
    match fs::read_to_string(&id_path) {
        Ok(on_disk) if on_disk.trim() == drive.id.trim() => {}
        Ok(on_disk) => {
            reporter.error(&format!(
                "计划已过期：盘符 {}: 上现在是「{}」,而本轮计划针对的是「{}」(预览后换过盘?)\
                 → 本轮不执行,请重新规划(bftool archive)。",
                drive.letter,
                on_disk.trim(),
                drive.id
            ));
            return Ok(ArchiveSummary::default());
        }
        Err(e) => {
            reporter.error(&format!(
                "计划已过期:无法读取盘 {}: 的盘内编号确认身份({})→ 本轮不执行,请重新规划。",
                drive.letter, e
            ));
            return Ok(ArchiveSummary::default());
        }
    }
    if drive::drive_is_sealed(&drive.root) {
        reporter.error(&format!(
            "计划已过期：盘 {} ({}:) 在预览后被封盘(或封盘态读取失败,保守停止)→ 本轮不执行,请换未封盘的盘重新规划。",
            drive.id, drive.letter
        ));
        return Ok(ArchiveSummary {
            sealed_stopped: true,
            ..Default::default()
        });
    }

    // SEC-007: 单盘不变式复验。`pick_active` 的「多块可写盘 → 停」检查只在 plan() 跑过一次；
    // 预览→执行之间若插入第二块未封盘备份盘，原来的检查就被绕过、可能写错盘。
    // 仅在**自动选盘**(drive_letter_override 为 None)时复验——显式 --drive 指定盘符时是用户
    // 主动选定，不在此拦。重新 scan：出现 >1 块可写盘即中止本轮，让用户确认只留一块。
    if opts.drive_letter_override.is_none() {
        match drive::usable_drives_now(cfg.min_drive_gb) {
            Ok(usable) if usable.len() > 1 => {
                let names = usable
                    .iter()
                    .map(|d| format!("{}({}:)", d.id, d.letter))
                    .collect::<Vec<_>>()
                    .join(", ");
                reporter.error(&format!(
                    "计划已过期：预览后检测到多块未封盘的备份盘：{} —— 为防止写错盘已停止。\
                     请只保留一块在线(其余盘可封盘或拔下)后重新运行 `bftool archive`。",
                    names
                ));
                return Ok(ArchiveSummary::default());
            }
            // 0 块/1 块都不阻断:1 块是正常情形;0 块(此盘恰好被拔)由后面 drive_id 重验已覆盖。
            Ok(_) => {}
            // scan 失败不静默吞:复验本身出错时 fail-closed 中止,不冒「漏检多盘」的险。
            Err(e) => {
                reporter.error(&format!(
                    "无法复验在线备份盘数量({}) → 为防止写错盘,本轮不执行,请检查盘连接后重试。",
                    e
                ));
                return Ok(ArchiveSummary::default());
            }
        }
    }

    // 路径安全检查
    let warnings = safety::check_paths(
        &cfg.ready_root,
        &cfg.archived_root,
        &cfg.system_root,
        Some(&drive.root),
    )?;
    for w in warnings {
        reporter.warn(&w);
    }

    if let Some(source) = &opts.source_override {
        for warning in safety::check_paths(
            source,
            &cfg.archived_root,
            &cfg.system_root,
            Some(&drive.root),
        )? {
            reporter.warn(&warning);
        }
    }

    // 先建 system_root(锁文件与事务标记都在此),再取进程级归档锁 —— 必须在路径安全检查之后建,
    // 避免坏配置先污染。R6-4:锁防两个 bftool 实例(CLI+CLI / CLI+GUI)并发归档同一套配置/盘。
    let system_guard = crate::engine::destination::SafeDir::open(&cfg.system_root, true)
        .context("安全创建备份系统目录失败")?;
    // RAII:_archive_lock 在 run_plan 返回(含 panic 展开,release 已用 panic=unwind,见 R5-8)时 Drop 删锁。
    // review-r3 round4:锁必须**先于** check_pending_txn 获取 —— 后者会在恢复分支主动 clear_marker
    // 删除事务标记,这属于受锁保护的恢复状态变更。若放在锁外,并发实例可在持锁实例提交途中删掉其活动
    // 标记。把锁上移使「读取/清除/重做事务标记」整体处于跨进程互斥内。
    let _archive_lock = ArchiveLock::acquire(&cfg.system_root)?;

    // 启动自检(已在锁内):上次的事务标记是不是残留？未解决时必须先停,避免新归档覆盖旧恢复证据。
    // 传入本轮目标盘 id(上面 L497-519 已实时复验过盘内编号):若残留事务写在另一块盘上,
    // 重做前会 fail-closed,避免抹掉原盘那份中断副本的恢复证据。(review-r3 #2)
    check_pending_txn_for_drive(cfg, reporter, drive)?;

    // 其余系统/归档目录:在事务自检之后建,避免未解决事务时就对归档目标区产生新写入。
    system_guard.ensure_dir(Path::new(paths::SYSTEM_LOGS_DIR))?;
    let _archived_guard = crate::engine::destination::SafeDir::open(&cfg.archived_root, true)
        .context("安全创建已备份目录失败")?;

    // tester detection:本轮只 detect 一次、只 warn 一次
    let mut tester_opt = detect_tester(cfg, opts, reporter)?;

    let mut summary = ArchiveSummary::default();
    // review-r2 #2:已归档项目累计占用。每完成一个就累加其占用,让后续项目的容量/封盘判定
    // (handle_one 内的余量检查)看到「前面项目占用后」的剩余 —— 否则每个项目都对冻结的
    // plan.drive.free_bytes 独立判断,累计超额时不会优雅封盘,而是一路写到磁盘满才复制失败。
    let mut consumed: u64 = 0;
    for item in &plan.items {
        if opts.limit > 0 && summary.handled >= opts.limit {
            reporter.action(&format!(
                "已达本次处理上限（{} 个项目），停止；剩余项目下次运行继续。",
                opts.limit
            ));
            break;
        }
        // 取消只在**项目边界**生效(不在文件复制中途);已开始的项目跑完或安全跳过。(Spec D §4.5)
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            reporter.action(&format!(
                "已取消：{} 个已完成,其余未处理(已开始的项目已安全收尾)。",
                summary.handled
            ));
            break;
        }
        // 执行实际源路径，绝不从展示 basename 在 ready_root 重新猜测。
        if let Err(error) = validate_source_identity(cfg, opts, item) {
            reporter.warn(&format!(
                "{}：计划已过期，源身份无法复验（{error:#}）；请重新规划。",
                item.name
            ));
            continue;
        }
        if let PlanAction::Skip(reason) = &item.action {
            if opts.incremental && reason.contains(SKIP_UNCHANGED) {
                match verified_unchanged(cfg, drive, item, opts) {
                    Ok(true) => {
                        reporter.info(&format!(
                            "{}：{}",
                            item.source_relative.display(),
                            SKIP_UNCHANGED
                        ));
                        continue;
                    }
                    Ok(false) => reporter.info(&format!(
                        "{}：源或已备副本在预览后改变，本轮重新归档。",
                        item.source_relative.display()
                    )),
                    Err(error) => {
                        reporter.error(&format!(
                            "{}：增量复验失败（{error:#}），保留源。",
                            item.source_relative.display()
                        ));
                        summary.failed += 1;
                        continue;
                    }
                }
            }
        }
        let proj_path = &item.source_path;
        let forced = match &item.action {
            PlanAction::Archive { dest_name } | PlanAction::RenameAndArchive { dest_name } => {
                Some((dest_name.as_str(), item.dest_existed_at_plan))
            }
            PlanAction::Skip(_) | PlanAction::SealAndStop(_) => None,
        };
        // 把可见剩余传给 handle_one。R6-1:取 min(投影剩余, 实时剩余):consumed 处理本工具自身
        // 串行占用;但 plan→run 之间若有外部进程往备份盘写入,实时剩余会更低 —— 实时重查校正,
        // 查询失败(盘离线等)则退回投影值(保守:不放大可用量)。强优化:这次实时查询顺带复验盘身份
        // (编号/封盘),拦住「轮内同盘符热插拔换盘」窗口(见 reconcile_item_drive)。
        let mut drive_now = drive.clone();
        let live = drive::info_by_letter(&drive.letter).ok();
        match reconcile_item_drive(drive, consumed, live.as_ref()) {
            ItemDriveCheck::Use(free) => drive_now.free_bytes = free,
            ItemDriveCheck::AbortSwapped => {
                let now_id = live
                    .as_ref()
                    .map(|l| l.id.trim().to_string())
                    .unwrap_or_default();
                reporter.error(&format!(
                    "计划已过期:盘符 {}: 上现在是「{}」而本轮计划针对「{}」(运行中途换过盘?)\
                     → 停止本轮,已完成项目保留,请重新规划(bftool archive)。",
                    drive.letter,
                    now_id,
                    drive.id.trim()
                ));
                break;
            }
            ItemDriveCheck::AbortSealed => {
                reporter.error(&format!(
                    "计划已过期:盘 {} ({}:) 在本轮中途被封盘 → 停止本轮,请换未封盘的盘重新规划。",
                    drive.id.trim(),
                    drive.letter
                ));
                break;
            }
        }
        match handle_one(
            cfg,
            reporter,
            &drive_now,
            proj_path,
            opts,
            tester_opt.as_ref(),
            forced,
        ) {
            Ok(HandleOutcome::Done(bytes)) => {
                summary.handled += 1;
                // R2-2:用 handle_one 返回的**实际**归档字节累计,而非 plan 期 est_bytes ——
                // Skip(est=0)/SealAndStop 项在 plan→run 间翻转为 Done 时,est 与真实写入不一致。
                consumed = consumed.saturating_add(bytes);
            }
            Ok(HandleOutcome::Skipped) => {}
            Ok(HandleOutcome::Failed) => summary.failed += 1,
            Ok(HandleOutcome::StalePlan) => {} // 已在 handle_one 内打"计划已过期",不计 failed
            Ok(HandleOutcome::CommitInterrupted) => {
                // R3-3:提交中途(索引已写、移源失败),事务标记保留 → 立即中止本轮,
                // 不让后续项目的 pending.write 覆盖该标记。计 failed 提醒用户需处理。
                summary.failed += 1;
                break;
            }
            Ok(HandleOutcome::DriveSealed) => {
                summary.sealed_stopped = true;
                reporter.action(
                    "本盘已封盘，本轮结束。换上下一块目标盘后用 `bftool init <盘符>` 初始化再继续。",
                );
                break;
            }
            Ok(HandleOutcome::TesterFatallyDisabled) => {
                summary.failed += 1;
                if opts.no_hash {
                    reporter.error(
                        "测试器运行时失效(spawn/被信号杀/exe 被 AV 拦),同时 --unsafe-no-hash 已开 \
                         —— 后续项目将落到「无 SHA256 + 无 archive test」无校验状态。本轮停止。\n\
                         如何修：去掉 --unsafe-no-hash 让 SHA256 兜底,或修复测试器环境后重跑。",
                    );
                    break;
                }
                reporter.warn(
                    "测试器自身异常 —— 本轮剩下的项目跳过压缩包测试；SHA256 整文件校验仍在。\
                     请检查配置的测试器路径是否正确、exe 是否被 AV 拦截。",
                );
                tester_opt = None;
                // 触发该分支的本项目在 handle_one 的压缩包测试阶段已提前返回、**本轮未归档**(只是关掉了
                // 后续项目的压缩包测试)。计 failed,使 CLI 退出非零、自动化/计划任务能感知「有项目未成功」,
                // 与 L-007『批量归档里有失败要 exit 非零』一致。(review-r3 round5)
                reporter.warn(&format!(
                    "项目 {} 因测试器失效本轮未归档、已计为失败;修复测试器或加 --no-test-archives 后重跑。",
                    item.name
                ));
            }
            Err(e) => {
                reporter.error(&format!(
                    "项目 {} 处理时发生意外错误：{} → 跳过该项目，继续下一个。",
                    item.name, e
                ));
                // 用 note_manual(非致命):台账写失败不再把整轮归档拖垮。(ledger L-018)
                note_manual(cfg, reporter, &item.name, &format!("未捕获异常：{}", e));
                summary.failed += 1;
            }
        }
    }
    // AR-13: 取消与失败可同时发生(取消前已有项目失败)。汇总里同时体现取消原因,
    // 否则只打失败消息会让「为什么没处理完」对用户不可见。
    let cancel_note = if summary.cancelled {
        "（本轮被取消，剩余项目未处理）"
    } else {
        ""
    };
    if summary.failed > 0 {
        reporter.error(&format!(
            "本轮结束：{} 个成功,{} 个失败(详见上方与「需人工处理.txt」){}。",
            summary.handled, summary.failed, cancel_note
        ));
    } else if !summary.cancelled {
        reporter.ok("本轮结束。");
    }
    Ok(summary)
}

//! 主归档流程：扫描 → 稳定性 → 容量 → 复制 → 校验 → 复核源 → 事务式提交。
//!
//! 顺序与不变量（与 PowerShell 旧版一致，且经实测）：
//! 1. 多块未封盘备份盘同时在线 → 立即停止（防写错盘）
//! 2. 单项目 try/catch 隔离，意外只跳过该项目不中断整轮
//! 3. 校验失败时把目标侧坏文件移到 异常文件/，下次自动重传
//! 4. 移动源前再次比对源（防"备份的是旧版本"）
//! 5. 写事务标记 → 移动源 → 写清单/索引 → 删标记（宁可漏写索引也不要"假装归档好了"）

use anyhow::{Context, Result};
use chrono::{Local, Utc};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Config;
use crate::engine::archive_test::{self, Tester, TesterPaths};
use crate::engine::drive::{self, BackupDrive, DriveInfo};
use crate::engine::manifest::{self, ManifestOpts};
use crate::engine::{cruft, durable, paths, safety, txn};
use crate::reporter::Reporter;

#[derive(Debug, Default, Clone)]
pub struct Options {
    pub dry_run: bool,
    pub no_hash: bool,
    pub limit: usize,
    pub drive_letter_override: Option<String>,
    pub no_test_archives: bool,
}

/// 一轮归档的结果汇总。`failed>0` = 有项目处理失败 —— CLI 据此设非零退出码,
/// 自动化/计划任务才能识别"批量归档里有失败"(此前总是 exit 0)。(ledger L-007)
/// `cancelled`/`sealed_stopped` 是独立态,**不计入 failed**(取消≠失败,封盘停≠失败)。(Spec D §4.5/§4.1)
#[derive(Debug, Default, Clone)]
pub struct ArchiveSummary {
    pub handled: usize,
    pub failed: usize,
    pub cancelled: bool,
    pub sealed_stopped: bool,
}

/// 本轮计划里的一项(GUI 预览 / CLI dry-run 渲染;`run_plan` 据此执行)。(Spec D §4.1)
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub name: String,
    pub est_bytes: u64,
    pub action: PlanAction,
}

/// 对单个项目的计划动作。`dest_name` 在 `plan()` 阶段**冻结**(含重名时基于 `Local::now()`
/// 的时间戳名),保证预览的目标名 = 正式执行的目标名(不各自重算时间戳漂移)。(Spec D §4.1)
#[derive(Debug, Clone)]
pub enum PlanAction {
    /// 正常归档到 `dest_name`。
    Archive { dest_name: String },
    /// 本盘已有同名历史备份 → 改用唯一名 `dest_name` 归档。
    RenameAndArchive { dest_name: String },
    /// 不归档本项目(未稳定 / 0 文件 / 超单盘容量 / 索引损坏 …),`reason` 给原因。
    Skip(String),
    /// 余量不足放下本项目 → 封盘停本轮(非 skip:其后项目本轮不再尝试)。(Finding #2)
    SealAndStop(String),
}

/// 本轮归档计划:选定的盘 + 各项目动作 + 冻结的执行选项。
/// `plan()` 不动数据算出它;`run_plan()` 消费同一份执行(冻"意图"、不冻"安全判断")。(Spec D §4.1)
#[derive(Debug, Clone)]
pub struct ArchivePlan {
    pub drive: BackupDrive,
    pub items: Vec<PlanItem>,
    /// 冻结的执行选项(no_hash/no_test_archives/limit);`dry_run`/`drive_letter_override` 已在
    /// `plan()` 阶段消费。让 `run_plan(cfg, plan, cancel, reporter)` 维持 4 参签名。
    pub opts: Options,
}

/// `plan()` 选不出可写盘时的友好信号(不是错误,CLI/GUI 据此提示插盘 init,退出码 0)。
#[derive(Debug)]
struct NoWritableDrive;
impl std::fmt::Display for NoWritableDrive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "未发现可写入的备份盘")
    }
}
impl std::error::Error for NoWritableDrive {}

/// 是否处于"没有任何内容完整性校验"的状态:跳过 SHA256(no_hash) 且 archive test 实际关闭。
/// 等价于只剩 size+count+mtime,是禁止的组合。判定集中在此,core::run 守卫与 CLI 守卫共用
/// 同一真值,避免两处条件漂移把"无校验后门"悄悄打开。(ledger L-008)
pub fn verify_disabled(no_hash: bool, test_archives: bool, no_test_archives: bool) -> bool {
    no_hash && (!test_archives || no_test_archives)
}

/// 校验强度的类型化表示:由"是否跑了 SHA256"派生,避免 "SHA256-OK"/"SIZE+COUNT" 字面量
/// 散落各处、与实际校验脱钩。token() 是写入索引 CSV 的稳定列值(保持旧版兼容)。(ledger L-020)
#[derive(Debug, Clone, Copy)]
enum VerifyStatus {
    Sha256Ok,
    SizeCount,
}

impl VerifyStatus {
    fn from_opts(no_hash: bool) -> Self {
        if no_hash {
            Self::SizeCount
        } else {
            Self::Sha256Ok
        }
    }
    fn token(self) -> &'static str {
        match self {
            Self::Sha256Ok => "SHA256-OK",
            Self::SizeCount => "SIZE+COUNT",
        }
    }
}

/// 一轮归档 = 算计划 + 执行计划。`dry_run` 时只算计划并渲染、不执行。(Spec D §4.1)
/// CLI 传永不取消的 `cancel`(行为不变);GUI 传可置位的。(Spec D §4.5)
pub fn run(
    cfg: &Config,
    reporter: &dyn Reporter,
    opts: Options,
    cancel: &AtomicBool,
) -> Result<ArchiveSummary> {
    let plan = match plan(cfg, &opts, reporter) {
        Ok(p) => p,
        // 选不出可写盘:友好提示已由 plan() 打过,这里按"无事可做"返回(退出码 0,行为不变)。
        Err(e) if e.downcast_ref::<NoWritableDrive>().is_some() => {
            return Ok(ArchiveSummary::default())
        }
        Err(e) => return Err(e),
    };
    if opts.dry_run {
        return Ok(render_dry_run(&plan, reporter));
    }
    run_plan(cfg, &plan, cancel, reporter)
}

/// 算出本轮计划——**不动任何数据**(只 folder_stats / 读本盘索引)。选盘失败 → `NoWritableDrive`
/// 信号(plan 内已打友好提示)。`ready_root` 缺失/无项目 → 空 items(由 run/run_plan 收尾)。(Spec D §4.1)
pub fn plan(cfg: &Config, opts: &Options, reporter: &dyn Reporter) -> Result<ArchivePlan> {
    // Spec B D14/D15 fail-closed:无校验组合连预览都不给(判定收口到 verify_disabled)。(ledger L-008)
    guard_verify_enabled(cfg, opts)?;

    // 选盘:override 走 info_by_letter,否则 pick_active(唯一未封盘且达标)。
    let drive = match opts.drive_letter_override.as_deref() {
        Some(letter) => drive::info_by_letter(letter)?,
        None => match drive::pick_active(cfg.min_drive_gb, reporter)? {
            Some(d) => d,
            None => {
                reporter.error("未发现已初始化且未封盘的备份盘。");
                reporter.info("插入空盘后运行：bftool init <盘符>（例：bftool init E）");
                return Err(NoWritableDrive.into());
            }
        },
    };
    // 类型化盘:封盘/过小在此挡掉(编译期挡"写错盘"的运行期对等)。(Spec D §4.2 / L-021)
    let drive = match drive.clone().try_into_writable(cfg.min_drive_gb) {
        Ok(w) => w.into_inner(),
        Err(e) => {
            reporter.error(&format!(
                "盘 {} ({}:) 不可写入：{}",
                drive.id, drive.letter, e
            ));
            return Err(NoWritableDrive.into());
        }
    };
    reporter.ok(&format!("当前备份盘: {} ({}:)", drive.id, drive.letter));

    // 列出待归档项目(读不到/为空 → 空计划,友好提示)。
    let mut items = Vec::new();
    if !cfg.ready_root.is_dir() {
        reporter.error(&format!("待备份 不存在：{}", cfg.ready_root.display()));
        return Ok(ArchivePlan {
            drive,
            items,
            opts: opts.clone(),
        });
    }
    let mut projects: Vec<_> = fs::read_dir(&cfg.ready_root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    // 按文件夹名前导数字升序，无数字前缀的排最后
    projects.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        leading_number(name).unwrap_or(u64::MAX)
    });
    if projects.is_empty() {
        reporter.info("待备份 中没有待归档项目，结束。");
    } else {
        reporter.info(&format!(
            "发现 {} 个待归档项目（按编号升序处理）。",
            projects.len()
        ));
    }
    for proj in &projects {
        items.push(decide(cfg, &drive, proj));
    }
    Ok(ArchivePlan {
        drive,
        items,
        opts: opts.clone(),
    })
}

/// 对单个项目算计划动作——**只读**(folder_stats + 读本盘索引,不 build manifest、不写盘)。
/// 重名时在此**冻结**时间戳目标名(预览=执行同名)。容量判定与 handle_one 同序:先"超单盘容量"再"余量不足"。
fn decide(cfg: &Config, drive: &BackupDrive, proj_path: &Path) -> PlanItem {
    let name = match proj_path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => {
            return PlanItem {
                name: proj_path.display().to_string(),
                est_bytes: 0,
                action: PlanAction::Skip("无效的项目目录名".into()),
            }
        }
    };
    let skip = |est: u64, reason: String| PlanItem {
        name: name.clone(),
        est_bytes: est,
        action: PlanAction::Skip(reason),
    };

    // 稳定性(未稳定是暂态:跳过不记台账,下轮再来)
    let st = safety::folder_stable(proj_path, cfg.stable_minutes);
    if !st.stable {
        return skip(0, format!("未稳定：{}", st.reason));
    }
    // 统计 + 枚举/元数据错误(fail-closed)
    let stats = folder_stats(proj_path);
    if !stats.enum_errors.is_empty() || !stats.metadata_errors.is_empty() {
        return skip(
            stats.bytes,
            format!(
                "统计不完整({} 个枚举错误 + {} 个 metadata 错误)→ 不归档,请先解决环境问题(权限/超长路径/AV 锁)再重试。",
                stats.enum_errors.len(),
                stats.metadata_errors.len()
            ),
        );
    }
    // 0 真实文件(空目录或全 cruft):不归档不移源
    if stats.files == 0 {
        return skip(
            0,
            "项目内没有可备份的真实文件(空目录或全是 cruft)——不归档、不移源,请人工确认。".into(),
        );
    }
    let size = stats.bytes;
    let size_gb = size as f64 / 1024.0 / 1024.0 / 1024.0;
    // 重名(读本盘索引;损坏 → fail-closed skip)
    let catalog = paths::drive_catalog_path(&drive.root);
    let dup = match catalog_has_project(&catalog, &name) {
        Ok(b) => b,
        Err(e) => {
            return skip(
                size,
                format!(
                "读本盘索引失败({}):无法判断是否重名 → 跳过,避免覆盖旧备份或重复写盘。请检查 {}",
                e,
                catalog.display()
            ),
            )
        }
    };
    // 超单盘容量(空盘也放不下)
    let reserve = cfg.reserve_gb * 1024 * 1024 * 1024;
    if size > drive.total_bytes.saturating_sub(reserve) {
        return skip(
            size,
            format!(
                "项目 {:.2}GB 超过单盘容量，空盘也放不下 → 需人工拆分",
                size_gb
            ),
        );
    }
    // 冻结目标名(重名 → 唯一时间戳名)
    let dest_name = if dup {
        format!("{}_{}", name, Local::now().format("%Y%m%d%H%M%S"))
    } else {
        name.clone()
    };
    // 余量(断点续传时仅按"还需写入"判断)→ 不足则封盘停本轮
    let dest = paths::drive_projects_dir(&drive.root).join(&dest_name);
    let dest_have = if dest.is_dir() {
        folder_stats(&dest).bytes
    } else {
        0
    };
    let need = size.saturating_sub(dest_have);
    if need.saturating_add(reserve) > drive.free_bytes {
        let need_gb = need as f64 / 1024.0 / 1024.0 / 1024.0;
        let free_gb = drive.free_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        let reason = format!(
            "盘 {} 余量不足以放下 {}（还需约 {:.2}GB + 余量；当前剩余 {:.2}GB）。封盘并请换下一块盘后重跑。",
            drive.id, name, need_gb, free_gb
        );
        return PlanItem {
            name,
            est_bytes: size,
            action: PlanAction::SealAndStop(reason),
        };
    }
    PlanItem {
        name,
        est_bytes: size,
        action: if dup {
            PlanAction::RenameAndArchive { dest_name }
        } else {
            PlanAction::Archive { dest_name }
        },
    }
}

/// dry-run:只渲染计划、不执行。`handled` = 将归档的项目数(Archive/RenameAndArchive)。
fn render_dry_run(plan: &ArchivePlan, reporter: &dyn Reporter) -> ArchiveSummary {
    let mut handled = 0usize;
    for it in &plan.items {
        let gb = it.est_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        match &it.action {
            PlanAction::Archive { dest_name } => {
                handled += 1;
                reporter.info(&format!(
                    "[演练] 将归档 {} (~{:.2}GB) → {}\\项目\\{}",
                    it.name, gb, plan.drive.id, dest_name
                ));
            }
            PlanAction::RenameAndArchive { dest_name } => {
                handled += 1;
                reporter.info(&format!(
                    "[演练] 将归档 {} (~{:.2}GB) → {}\\项目\\{}(本盘已有同名 → 改用唯一名)",
                    it.name, gb, plan.drive.id, dest_name
                ));
            }
            PlanAction::Skip(reason) => {
                reporter.info(&format!("[演练] 跳过 {}：{}", it.name, reason));
            }
            PlanAction::SealAndStop(reason) => {
                reporter.warn(&format!(
                    "[演练] {} → 此处将封盘停本轮：{}",
                    it.name, reason
                ));
                break; // 封盘点之后的项目本轮不会处理,预览也到此为止
            }
        }
    }
    reporter.ok("[演练] 结束(未真正复制/移动任何文件)。");
    ArchiveSummary {
        handled,
        ..Default::default()
    }
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
    // fail-closed(纵深:GUI 可能直接调 run_plan)
    guard_verify_enabled(cfg, opts)?;

    // 准备系统目录
    fs::create_dir_all(&cfg.system_root).context("创建 备份系统 目录失败")?;
    fs::create_dir_all(paths::system_logs_dir(&cfg.system_root)).ok();
    fs::create_dir_all(&cfg.archived_root).context("创建 已备份 目录失败")?;

    // 启动自检：上次的事务标记是不是残留？
    check_pending_txn(cfg, reporter)?;

    // 重验盘:预览→执行之间盘可能被拔/被封。按盘上实时状态(id 文件 + 封盘 marker)重读,
    // 不信 plan 里冻结的 sealed/在线态。任一变 → 整份计划过期(不写任何盘)。
    let drive = &plan.drive;
    if !paths::drive_id_path(&drive.root).is_file() {
        reporter.error(&format!(
            "计划已过期：备份盘 {} ({}:) 已不在线或未初始化 → 本轮不执行,请重新规划(bftool archive)。",
            drive.id, drive.letter
        ));
        return Ok(ArchiveSummary::default());
    }
    if paths::drive_sealed_path(&drive.root).is_file() {
        reporter.error(&format!(
            "计划已过期：盘 {} ({}:) 在预览后被封盘 → 本轮不执行,请换未封盘的盘重新规划。",
            drive.id, drive.letter
        ));
        return Ok(ArchiveSummary {
            sealed_stopped: true,
            ..Default::default()
        });
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

    // tester detection:本轮只 detect 一次、只 warn 一次
    let mut tester_opt = detect_tester(cfg, opts, reporter)?;

    let mut summary = ArchiveSummary::default();
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
        // Skip/SealAndStop 的预览项也走 handle_one(forced=None)重新裁决:这样安全前置实时重验、
        // 台账(需人工处理.txt)按真实结果记;Archive/RenameAndArchive 则带上冻结目标名。
        let proj_path = cfg.ready_root.join(&item.name);
        let forced = match &item.action {
            PlanAction::Archive { dest_name } | PlanAction::RenameAndArchive { dest_name } => {
                Some(dest_name.as_str())
            }
            PlanAction::Skip(_) | PlanAction::SealAndStop(_) => None,
        };
        match handle_one(
            cfg,
            reporter,
            drive,
            &proj_path,
            opts,
            tester_opt.as_ref(),
            forced,
        ) {
            Ok(HandleOutcome::Done) => summary.handled += 1,
            Ok(HandleOutcome::Skipped) => {}
            Ok(HandleOutcome::StalePlan) => {} // 已在 handle_one 内打"计划已过期",不计 failed
            Ok(HandleOutcome::DriveSealed) => {
                summary.sealed_stopped = true;
                reporter.action(
                    "本盘已封盘，本轮结束。换上下一块空盘后用 `bftool init <盘符>` 初始化再继续。",
                );
                break;
            }
            Ok(HandleOutcome::TesterFatallyDisabled) => {
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
    if summary.failed > 0 {
        reporter.error(&format!(
            "本轮结束：{} 个成功,{} 个失败(详见上方与「需人工处理.txt」)。",
            summary.handled, summary.failed
        ));
    } else if !summary.cancelled {
        reporter.ok("本轮结束。");
    }
    Ok(summary)
}

/// fail-closed 守卫:无校验组合(no_hash + archive test 关闭)直接拒。(ledger L-008)
fn guard_verify_enabled(cfg: &Config, opts: &Options) -> Result<()> {
    if verify_disabled(opts.no_hash, cfg.test_archives, opts.no_test_archives) {
        anyhow::bail!(
            "拒绝运行：no_hash=true 与 archive test 关闭(配置 test_archives=false 或 \
             opts.no_test_archives=true)不能同时存在 —— 等价于「没在做完整性校验」。\n\
             这是 core 级 fail-closed 拦截,调用方应该在传 Options 之前就做合并检查\
             (CLI dispatch 已经做了一次更友好的)。\n\
             如何修：\n\
               - 去掉 no_hash 让 SHA256 兜底；或\n\
               - 不要把 test_archives 设成 false 也不要把 no_test_archives 设成 true"
        );
    }
    Ok(())
}

/// 顶层 tester detection:本轮只 detect 一次。no_hash 且一个测试器都没装 → fail-closed bail。
fn detect_tester(
    cfg: &Config,
    opts: &Options,
    reporter: &dyn Reporter,
) -> Result<Option<(Tester, PathBuf)>> {
    if !cfg.test_archives || opts.no_test_archives {
        return Ok(None);
    }
    let p = TesterPaths {
        winrar: cfg.winrar_path.clone(),
        bandizip: cfg.bandizip_path.clone(),
        seven_zip: cfg.seven_zip_path.clone(),
    };
    match archive_test::detect(&p) {
        Some(t) => Ok(Some(t)),
        None => {
            if opts.no_hash {
                anyhow::bail!(
                    "拒绝运行：开启了 --unsafe-no-hash 但**机器上一个压缩包测试器都没装**。\n\
                     此时 SHA256 没在跑,archive test 也跑不起来 —— 只剩\n\
                     文件数+大小+修改时间,等价于「没在做完整性校验」。\n\
                     如何修：\n\
                       - 装一个测试器：推荐 7-Zip(免费开源 https://7-zip.org);或\n\
                       - 去掉 --unsafe-no-hash,让 SHA256 兜底"
                );
            }
            reporter.warn(
                "未检测到 WinRAR / Bandizip / 7-Zip —— 压缩包内部结构无法测试。\
                 重要资料建议装一个(推荐 7-Zip 免费开源：https://7-zip.org)。\
                 SHA256 整文件校验仍在正常进行。",
            );
            Ok(None)
        }
    }
}

enum HandleOutcome {
    Done,
    Skipped,
    /// 冻结的目标名在执行时已被占用(预览→执行间状态变了)——不写盘,不计 failed。(Spec D §4.1)
    StalePlan,
    DriveSealed,
    TesterFatallyDisabled,
}

/// 执行单个项目的真实归档(复制/校验/源复核/事务提交)。`forced_dest_name`:
/// - `Some(name)` = run_plan 传入的**冻结目标名**(复验未被占用后使用;被占 → StalePlan);
/// - `None` = 直调(自行裁决目标名,重名时现算时间戳)——兼容旧测试与非计划路径。
///
/// 无论哪条路径,**安全前置(稳定/统计/容量/源复核)都在此实时重验**——冻的是命名,不是安全判断。
#[allow(clippy::too_many_arguments)]
fn handle_one(
    cfg: &Config,
    reporter: &dyn Reporter,
    drive: &DriveInfo,
    proj_path: &Path,
    opts: &Options,
    tester_opt: Option<&(Tester, PathBuf)>,
    forced_dest_name: Option<&str>,
) -> Result<HandleOutcome> {
    let name = proj_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("无效的项目目录名"))?
        .to_string();
    let proj_no = leading_digits(&name);
    reporter.action(&format!("=== 处理: {} ===", name));

    // 稳定性
    let st = safety::folder_stable(proj_path, cfg.stable_minutes);
    if !st.stable {
        reporter.warn(&format!("跳过（未稳定）：{}", st.reason));
        return Ok(HandleOutcome::Skipped);
    }

    // 体积 + 目标已存在量（断点续传时仅按"还需写入"判断）
    let stats = folder_stats(proj_path);
    let size = stats.bytes;
    let size_gb = size as f64 / 1024.0 / 1024.0 / 1024.0;

    // 枚举/元数据错误:dry-run 与正式路径都要 fail-closed —— 此前只有 dry-run 报告,
    // 正式路径靠后面 manifest::build 兜底但太晚,且容量按可能偏小的 size 误判。(ledger L-005)
    if !stats.enum_errors.is_empty() || !stats.metadata_errors.is_empty() {
        for e in &stats.enum_errors {
            reporter.error(&format!("枚举失败：{}", e));
        }
        for e in &stats.metadata_errors {
            reporter.error(&format!("读元数据失败：{}", e));
        }
        let msg = format!(
            "统计不完整({} 个枚举错误 + {} 个 metadata 错误)→ 不归档。请先解决环境问题\
             (权限拒绝？路径过长？被 AV 锁住？)再重试。",
            stats.enum_errors.len(),
            stats.metadata_errors.len()
        );
        reporter.error(&msg);
        note_manual(cfg, reporter, &name, &msg);
        return Ok(HandleOutcome::Skipped);
    }

    // 重名保护 / 冻结名复验
    let catalog = paths::drive_catalog_path(&drive.root);
    let dup_in_drive = match catalog_has_project(&catalog, &name) {
        Ok(b) => b,
        Err(e) => {
            // 本盘索引存在但读不出(损坏/非 UTF-8):fail-closed —— 不确定是否重名时,既不能覆盖
            // 旧备份,也不该每轮用新时间戳名重复全量写盘(会塞满)。停下让用户修索引。(Phase 4 F-4;修订 L-017)
            reporter.error(&format!(
                "读本盘索引失败({}):无法判断是否重名 → 跳过本项目,避免覆盖旧备份或重复写盘。请检查 {}",
                e,
                catalog.display()
            ));
            note_manual(
                cfg,
                reporter,
                &name,
                &format!(
                    "本盘索引读取失败:{} —— 请人工检查/修复 {}",
                    e,
                    catalog.display()
                ),
            );
            return Ok(HandleOutcome::Skipped);
        }
    };
    let dest_name = match forced_dest_name {
        // run_plan 路径:用冻结名,但**复验**它未被占用(预览→执行间可能有人占了它)。
        // 时间戳唯一名(重名场景)不会撞;普通名若现在已存在 → 计划已过期(StalePlan),不照旧误写。
        Some(frozen) => {
            let taken = if frozen == name {
                dup_in_drive
            } else {
                catalog_has_project(&catalog, frozen).unwrap_or(false)
            };
            if taken {
                reporter.warn(&format!(
                    "计划已过期：目标名『{}』在本盘索引中已存在(预览后被占用)→ 跳过本项目,请重新规划。",
                    frozen
                ));
                return Ok(HandleOutcome::StalePlan);
            }
            frozen.to_string()
        }
        // 直调路径:自行裁决,重名时现算唯一时间戳名(旧行为)。
        None => {
            if dup_in_drive {
                let stamp = Local::now().format("%Y%m%d%H%M%S");
                let dn = format!("{}_{}", name, stamp);
                reporter.action(&format!(
                    "本盘已存在同名历史备份『{}』→ 为避免污染旧备份，本次改用唯一名『{}』。",
                    name, dn
                ));
                dn
            } else {
                name.clone()
            }
        }
    };
    let dest = paths::drive_projects_dir(&drive.root).join(&dest_name);
    let dest_have = if dest.is_dir() {
        folder_stats(&dest).bytes
    } else {
        0
    };
    let need = size.saturating_sub(dest_have);

    // 容量
    let reserve = cfg.reserve_gb * 1024 * 1024 * 1024;
    if size > drive.total_bytes.saturating_sub(reserve) {
        reporter.error(&format!(
            "项目 {:.2}GB 超过单盘容量，空盘也放不下 → 需人工拆分",
            size_gb
        ));
        append_manual(
            cfg,
            &name,
            &format!("{:.2}GB 超过单盘容量，需拆分或显式跨盘", size_gb),
        )?;
        return Ok(HandleOutcome::Skipped);
    }
    if need.saturating_add(reserve) > drive.free_bytes {
        let need_gb = need as f64 / 1024.0 / 1024.0 / 1024.0;
        let free_gb = drive.free_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        reporter.warn(&format!(
            "盘 {} 余量不足以放下 {}（还需约 {:.2}GB + 余量；当前剩余 {:.2}GB）。封盘并请换下一块盘后重跑。",
            drive.id, name, need_gb, free_gb
        ));
        drive::seal(drive).context("封盘失败")?;
        reporter.action(&format!(
            "已封盘 {}。取下本盘、插上下一块空盘(NTFS)后，运行 `bftool init <盘符>` 初始化再继续。",
            drive.id
        ));
        return Ok(HandleOutcome::DriveSealed);
    }

    if opts.dry_run {
        reporter.info(&format!(
            "[演练] 将归档 {} ({} 个文件, {:.2}GB) → {}",
            name, stats.files, size_gb, drive.id
        ));
        return Ok(HandleOutcome::Done);
    }

    // Spec B 源压缩包测试 —— 在 dry_run check 之后,在生成 manifest 之前
    if let Some((kind, path)) = tester_opt {
        let r =
            archive_test::test_folder(proj_path, (*kind, path.as_path()), opts.no_hash, reporter);
        if !r.ok {
            for detail in r.details() {
                reporter.error(&detail);
            }
            append_manual(cfg, &name, &format!("源压缩包测试失败：{}", r.summary()))?;
            if !r.tester_errors.is_empty() {
                return Ok(HandleOutcome::TesterFatallyDisabled);
            }
            return Ok(HandleOutcome::Skipped);
        }

        if opts.no_hash && r.uncovered_files > 0 {
            reporter.error(&format!(
                "本项目有 {} 个未被 archive test 覆盖的文件,开启了 --unsafe-no-hash —— \
                 这些文件没有 SHA256,只剩 size+count+mtime,违反完整性校验底线。",
                r.uncovered_files
            ));
            append_manual(
                cfg,
                &name,
                &format!(
                    "unsafe_no_hash 模式下项目有 {} 个未覆盖文件 → 这些文件无完整性校验。\
                     请去掉 --unsafe-no-hash 让 SHA256 兜底,或把非压缩包内容打包成一个\
                     压缩包后再归档。",
                    r.uncovered_files
                ),
            )?;
            return Ok(HandleOutcome::Skipped);
        }
    }

    // 生成源清单
    reporter.info(&format!("生成源清单/校验和（{:.2}GB，可能较慢）…", size_gb));
    let src = manifest::build(
        proj_path,
        ManifestOpts {
            no_hash: opts.no_hash,
        },
        reporter,
    )?;

    // 0 真实文件:不归档、不移源 —— 否则"什么都没备份"会被记成 SHA256-OK 成功并把源移走。(ledger L-003)
    if src.count() == 0 {
        reporter.warn(&format!("跳过(无可备份的真实文件):{}", name));
        append_manual(
            cfg,
            &name,
            "项目内没有可备份的真实文件(空目录或全是 cruft)——不归档、不移源,请人工确认。",
        )?;
        return Ok(HandleOutcome::Skipped);
    }

    // 复制
    fs::create_dir_all(&dest).context("创建目标目录失败")?;
    reporter.info("开始复制（断点续传：已存在且大小一致的文件会被跳过）…");
    copy_folder(proj_path, &dest, reporter)?;
    reporter.info("复制完成，开始校验…");

    // 目标清单 + 比对
    let dst = manifest::build(
        &dest,
        ManifestOpts {
            no_hash: opts.no_hash,
        },
        reporter,
    )?;
    let d = manifest::diff(&src, &dst, !opts.no_hash);
    if !d.ok() {
        reporter.error(&format!(
            "校验失败：{} → 不写索引、不移动源。",
            d.reasons.join("; ")
        ));
        // 隔离坏文件，让下次 robocopy 风格的"补传"再来一遍
        if !d.bad_dst_rels.is_empty() {
            let quar = paths::drive_quarantine_dir(&drive.root).join(&name);
            let mut moved = 0usize;
            for rel in &d.bad_dst_rels {
                let src_p = dest.join(rel);
                if src_p.exists() {
                    let dst_p = quar.join(rel);
                    if let Some(p) = dst_p.parent() {
                        if let Err(e) = fs::create_dir_all(p) {
                            // 不 swallow:建隔离目录失败时明确归因、跳过该文件隔离。(ledger L-015)
                            reporter.warn(&format!(
                                "建隔离目录失败 {}:{} —— 坏文件 {} 未隔离,请手动检查。",
                                p.display(),
                                e,
                                src_p.display()
                            ));
                            continue;
                        }
                    }
                    if let Err(e) = fs::rename(&src_p, &dst_p) {
                        reporter.warn(&format!(
                            "隔离失败 {}：{}（请手动检查 {}）",
                            rel,
                            e,
                            src_p.display()
                        ));
                    } else {
                        moved += 1;
                    }
                }
            }
            reporter.action(&format!(
                "已将 {} 个损坏/多余的目标文件移到：{} → 下次运行会自动补传并重新校验。",
                moved,
                quar.display()
            ));
        }
        append_manual(cfg, &name, &format!("校验失败：{}", d.reasons.join("; ")))?;
        return Ok(HandleOutcome::Skipped);
    }

    // Spec B 目标压缩包测试 —— 在 SHA256 校验通过后、源复核之前
    if let Some((kind, path)) = tester_opt {
        let r = archive_test::test_folder(&dest, (*kind, path.as_path()), opts.no_hash, reporter);
        if !r.ok {
            for detail in r.details() {
                reporter.error(&detail);
            }
            if !r.archive_failed.is_empty() {
                let quar = paths::drive_quarantine_dir(&drive.root).join(&name);
                let mut moved = 0usize;
                for (bad, _reason) in &r.archive_failed {
                    if let Ok(rel) = bad.strip_prefix(&dest) {
                        let to = quar.join(rel);
                        if let Some(p) = to.parent() {
                            if let Err(e) = fs::create_dir_all(p) {
                                reporter.warn(&format!(
                                    "建隔离目录失败 {}:{} —— 坏压缩包 {} 未隔离,请手动检查。",
                                    p.display(),
                                    e,
                                    bad.display()
                                ));
                                continue;
                            }
                        }
                        if let Err(e) = fs::rename(bad, &to) {
                            reporter.warn(&format!("隔离失败 {}：{}", bad.display(), e));
                        } else {
                            moved += 1;
                        }
                    }
                }
                reporter.action(&format!(
                    "已隔离 {} 个损坏压缩包到 {}；下次运行会自动补传并重测。",
                    moved,
                    quar.display()
                ));
            }
            append_manual(cfg, &name, &format!("目标压缩包测试失败：{}", r.summary()))?;
            if !r.tester_errors.is_empty() {
                return Ok(HandleOutcome::TesterFatallyDisabled);
            }
            return Ok(HandleOutcome::Skipped);
        }
    }

    // 复核源在复制期间未变化
    reporter.info("复核源文件在复制期间未变化…");
    let src2 = manifest::build(
        proj_path,
        ManifestOpts {
            no_hash: opts.no_hash,
        },
        reporter,
    )?;
    let (changed, why) = manifest::source_changed(&src, &src2, opts.no_hash);
    if changed {
        reporter.error(&format!(
            "源在复制期间发生变化（{}）→ 不移动源、不写索引；该项目保留在 待备份，下次重做。",
            why.join("; ")
        ));
        append_manual(cfg, &name, &format!("源在复制期间变化：{}", why.join("; ")))?;
        return Ok(HandleOutcome::Skipped);
    }

    // 事务式提交
    let local_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let utc = Utc::now().to_rfc3339();
    let src_bytes = src.total_bytes();
    let size_gbval = src_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    let verify_status = VerifyStatus::from_opts(opts.no_hash);
    let manifest_path =
        paths::drive_manifest_dir(&drive.root).join(format!("{}.sha256.csv", dest_name));
    let rel_manifest = format!("本盘信息\\校验清单\\{}.sha256.csv", dest_name);
    let mut arch_dest = cfg.archived_root.join(&name);
    if arch_dest.exists() {
        let stamp = Local::now().format("%Y%m%d%H%M%S");
        arch_dest = cfg.archived_root.join(format!("{}_{}", name, stamp));
    }

    let txn_path = paths::system_pending_txn(&cfg.system_root);
    let pending = txn::PendingTxn {
        project_dest_name: dest_name.clone(),
        project_src_name: name.clone(),
        drive_id: drive.id.clone(),
        drive_letter: drive.letter.clone(),
        in_drive_path: format!("项目\\{}", dest_name),
        src_path: proj_path.display().to_string(),
        move_to: arch_dest.display().to_string(),
        started_at: local_time.clone(),
    };
    pending.write(&txn_path)?;

    // 移动源（先动它；失败抛错 → 不写索引；源留在 待备份 下次重做）
    fs::rename(proj_path, &arch_dest).map_err(|e| {
        // 跨卷 rename 在 Windows 返回 ERROR_NOT_SAME_DEVICE(17):给可操作的 fail-closed 提示。(ledger L-010)
        if e.raw_os_error() == Some(17) {
            anyhow::anyhow!(
                "移动源失败:待备份({})与已备份({})不在同一磁盘卷,无法原子移动。\n\
                 如何修:把 ready_root 与 archived_root 配到同一块盘(通常都在你的 SSD 上)。\n\
                 项目仍留在 待备份,改好配置后会自动重做(目标盘上的副本+校验清单已写好)。",
                proj_path.display(),
                arch_dest.display()
            )
        } else {
            anyhow::anyhow!(
                "移动源失败:{} → {}: {}",
                proj_path.display(),
                arch_dest.display(),
                e
            )
        }
    })?;

    // 写清单 + 本盘索引 + 全局索引
    src.write_csv(&manifest_path)?;
    append_drive_catalog(
        &catalog,
        &DriveCatalogRow {
            project_no: proj_no,
            project_name: dest_name.clone(),
            file_count: src.count() as u64,
            total_bytes: src_bytes,
            archived_utc: utc.clone(),
            verify_status: verify_status.token().to_string(),
            status: "Complete".to_string(),
            notes: if dup_in_drive {
                format!("原名 {}", name)
            } else {
                String::new()
            },
        },
    )?;
    append_global_catalog(
        &paths::system_global_catalog(&cfg.system_root),
        &GlobalCatalogRow {
            folder_name: dest_name.clone(),
            drive_name: drive.id.clone(),
            archived_time: local_time.clone(),
            project_no: leading_digits(&name),
            in_drive_path: format!("项目\\{}", dest_name),
            file_count: src.count() as u64,
            size_gb: size_gbval,
            verify: verify_status.token().to_string(),
            manifest_path: rel_manifest,
        },
    )?;

    txn::PendingTxn::clear(&txn_path)?;
    reporter.ok(&format!(
        "✓ {} 归档+校验成功 → {}\\项目\\{}；SSD 源已移到 已备份（未删除）。",
        name, drive.id, dest_name
    ));
    Ok(HandleOutcome::Done)
}

#[derive(Debug, Serialize)]
struct DriveCatalogRow {
    #[serde(rename = "ProjectNo")]
    project_no: String,
    #[serde(rename = "ProjectName")]
    project_name: String,
    #[serde(rename = "FileCount")]
    file_count: u64,
    #[serde(rename = "TotalBytes")]
    total_bytes: u64,
    #[serde(rename = "ArchivedUTC")]
    archived_utc: String,
    #[serde(rename = "VerifyStatus")]
    verify_status: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Notes")]
    notes: String,
}

#[derive(Debug, Serialize)]
struct GlobalCatalogRow {
    #[serde(rename = "文件夹名")]
    folder_name: String,
    #[serde(rename = "备份盘名")]
    drive_name: String,
    #[serde(rename = "备份时间")]
    archived_time: String,
    #[serde(rename = "编号")]
    project_no: String,
    #[serde(rename = "盘内路径")]
    in_drive_path: String,
    #[serde(rename = "文件数")]
    file_count: u64,
    #[serde(rename = "大小GB")]
    size_gb: f64,
    #[serde(rename = "校验方式")]
    verify: String,
    #[serde(rename = "校验清单")]
    manifest_path: String,
}

fn append_drive_catalog(path: &Path, row: &DriveCatalogRow) -> Result<()> {
    let file_existed = path.is_file();
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(!file_existed)
        .from_writer(file);
    wtr.serialize(row)?;
    wtr.flush()?;
    // fsync:索引必须先于"删事务标记"真正落盘,否则断电后会出现"标记已删、索引未落"。(ledger L-001)
    let f = wtr
        .into_inner()
        .map_err(|e| anyhow::anyhow!("刷新索引缓冲失败：{}", e))?;
    durable::sync_file(&f).with_context(|| format!("索引刷盘失败：{}", path.display()))?;
    Ok(())
}

fn append_global_catalog(path: &Path, row: &GlobalCatalogRow) -> Result<()> {
    let file_existed = path.is_file();
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(!file_existed)
        .from_writer(file);
    wtr.serialize(row)?;
    wtr.flush()?;
    // fsync:索引必须先于"删事务标记"真正落盘,否则断电后会出现"标记已删、索引未落"。(ledger L-001)
    let f = wtr
        .into_inner()
        .map_err(|e| anyhow::anyhow!("刷新索引缓冲失败：{}", e))?;
    durable::sync_file(&f).with_context(|| format!("索引刷盘失败：{}", path.display()))?;
    Ok(())
}

fn catalog_has_project(catalog: &Path, project_name: &str) -> Result<bool> {
    if !catalog.is_file() {
        return Ok(false);
    }
    let mut rdr = csv::Reader::from_path(catalog)?;
    let headers = rdr.headers()?.clone();
    let Some(col) = headers.iter().position(|h| h == "ProjectName") else {
        return Ok(false);
    };
    for rec in rdr.records().flatten() {
        if rec.get(col).map(|v| v == project_name).unwrap_or(false) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// best-effort 记一行"需人工处理"台账:写失败只 warn,不向上抛 —— 台账写入故障
/// 不该把"跳过该项目继续下一个"升级成整轮中止(此前 run 的 catch-all 用 `?` 会)。(ledger L-018)
fn note_manual(cfg: &Config, reporter: &dyn Reporter, name: &str, why: &str) {
    if let Err(e) = append_manual(cfg, name, why) {
        reporter.warn(&format!("写「需人工处理」台账失败({}):{}", name, e));
    }
}

fn append_manual(cfg: &Config, name: &str, why: &str) -> Result<()> {
    let path = paths::system_need_manual(&cfg.system_root);
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).ok();
    }
    let line = format!(
        "{}\t{}\t{}\n",
        Local::now().format("%Y-%m-%d %H:%M"),
        name,
        why
    );
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

fn check_pending_txn(cfg: &Config, reporter: &dyn Reporter) -> Result<()> {
    let path = paths::system_pending_txn(&cfg.system_root);
    if !path.is_file() {
        return Ok(());
    }
    // 结构化读回(替代脆弱的 grab_field 标签抓取);解析失败 → 提示人工核对,不擅自删标记。(ledger L-022)
    let pending = match txn::PendingTxn::read(&path) {
        Ok(p) => p,
        Err(e) => {
            reporter.error(&format!(
                "→ 发现事务标记但解析失败({})。请人工核对该项目是否已归档,确认后删除：{}",
                e,
                path.display()
            ));
            return Ok(());
        }
    };
    let pname = pending.project_dest_name.clone();
    let psrc = if pending.project_src_name.is_empty() {
        pname.clone()
    } else {
        pending.project_src_name.clone()
    };
    let parch = pending.move_to.clone();
    let in_ready = !psrc.is_empty() && cfg.ready_root.join(&psrc).exists();
    let moved = !parch.is_empty() && Path::new(&parch).exists();
    let global = paths::system_global_catalog(&cfg.system_root);
    let indexed = global_has_folder(&global, &pname).unwrap_or(false);

    reporter.action(&format!("发现上次未完成的事务（项目：{}）。", pname));
    if in_ready {
        reporter
            .action("→ 源仍在『待备份』，说明移动尚未发生；本次会自动重做该项目。已清除旧标记。");
        let _ = fs::remove_file(&path);
    } else if moved && indexed {
        reporter.action("→ 已移动且索引中已有记录，判定为已完成（仅标记残留）。已自动清除标记。");
        let _ = fs::remove_file(&path);
    } else if moved && !indexed {
        reporter.error(&format!(
            "→ 源已移到『已备份』但全局索引可能漏写：数据应在备份盘 {}。请人工核对并在索引中补登；标记保留。",
            parch
        ));
    } else {
        reporter.error(&format!(
            "→ 无法自动判定（源不在待备份、目标也未确认）。请人工核对事务标记后处理,标记保留：{}",
            path.display()
        ));
    }
    Ok(())
}

fn global_has_folder(global: &Path, folder_name: &str) -> Result<bool> {
    if !global.is_file() {
        return Ok(false);
    }
    let mut rdr = csv::Reader::from_path(global)?;
    let headers = rdr.headers()?.clone();
    let Some(col) = headers.iter().position(|h| h == "文件夹名") else {
        return Ok(false);
    };
    for rec in rdr.records().flatten() {
        if rec.get(col).map(|v| v == folder_name).unwrap_or(false) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Default)]
pub struct FolderStats {
    pub files: u64,
    pub bytes: u64,
    pub enum_errors: Vec<String>,
    pub metadata_errors: Vec<String>,
}

fn folder_stats(p: &Path) -> FolderStats {
    let mut s = FolderStats::default();
    for entry in cruft::walk(p) {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                s.files += 1;
                match e.metadata() {
                    Ok(m) => s.bytes += m.len(),
                    Err(err) => s
                        .metadata_errors
                        .push(format!("{}: {}", e.path().display(), err)),
                }
            }
            Ok(_) => continue,
            Err(err) => s.enum_errors.push(format!("{}", err)),
        }
    }
    s
}

/// 简单的递归复制；与 robocopy 比缺少 /Z 断点续传中段恢复，但小文件/中等大小够用。
/// 已存在且大小相同的文件直接跳过（保留断点续传的核心语义）。
///
/// TODO(Batch 4)：改为 .bftool-part 临时文件 + 写完 sync + 重读目标算 hash + 原子 rename。
fn copy_folder(src: &Path, dst: &Path, reporter: &dyn Reporter) -> Result<()> {
    fs::create_dir_all(dst).ok();
    let mut errors: Vec<String> = Vec::new();
    for entry in cruft::walk(src) {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                errors.push(format!("{}", err));
                continue;
            }
        };
        let path = entry.path();
        let rel = path.strip_prefix(src)?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).ok();
        } else if entry.file_type().is_file() {
            // 原子复制的临时名;先清理可能残留的孤儿 .part(上次中断留下),避免在备份盘累积。(Phase 4 F-3)
            let part = {
                let mut s = target.clone().into_os_string();
                s.push(".bftool-part");
                PathBuf::from(s)
            };
            let _ = fs::remove_file(&part); // best-effort:失败也会被下面 fs::copy 覆盖
                                            // 已存在且大小一致 → 视为已传，跳过（断点续传）
            if let (Ok(meta_src), Ok(meta_dst)) = (path.metadata(), target.metadata()) {
                if meta_src.len() == meta_dst.len() {
                    continue;
                }
            }
            if let Some(p) = target.parent() {
                fs::create_dir_all(p).ok();
            }
            // 原子复制:写 <target>.bftool-part → fsync → rename。崩溃只会留下 .part
            // (被当 cruft 忽略、下轮重写),不会留下"大小对得上的半成品"被续传误跳过。(ledger L-014)
            fs::copy(path, &part)
                .with_context(|| format!("复制失败：{} → {}", path.display(), part.display()))?;
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&part)
                .with_context(|| format!("打开临时文件刷盘失败：{}", part.display()))?;
            f.sync_all()
                .with_context(|| format!("临时文件刷盘失败：{}", part.display()))?;
            drop(f);
            fs::rename(&part, &target).with_context(|| {
                format!("提交复制失败：{} → {}", part.display(), target.display())
            })?;
        }
    }
    if !errors.is_empty() {
        for e in &errors {
            reporter.error(&format!("复制阶段枚举源失败：{}", e));
        }
        anyhow::bail!("复制阶段枚举源失败 {} 项 → 本项目跳过。", errors.len());
    }
    Ok(())
}

fn leading_number(name: &str) -> Option<u64> {
    let s = name.trim_start();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<u64>().ok()
    }
}

fn leading_digits(name: &str) -> String {
    let s = name.trim_start();
    s.chars().take_while(|c| c.is_ascii_digit()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporter::NoopReporter;

    /// 搭一个临时"世界":ready/archived/sys + 一块假备份盘,供 handle_one 集成测试复用。
    fn temp_world() -> (tempfile::TempDir, Config, DriveInfo) {
        let d = tempfile::tempdir().unwrap();
        let base = d.path();
        let cfg = Config {
            ready_root: base.join("ready"),
            archived_root: base.join("archived"),
            system_root: base.join("sys"),
            reserve_gb: 0,
            stable_minutes: 0, // 不卡稳定性
            min_drive_gb: 0,
            name_prefix: "备份".into(),
            test_archives: false,
            winrar_path: std::path::PathBuf::new(),
            bandizip_path: std::path::PathBuf::new(),
            seven_zip_path: std::path::PathBuf::new(),
            extra_catalogs: Vec::new(),
        };
        fs::create_dir_all(&cfg.ready_root).unwrap();
        fs::create_dir_all(&cfg.archived_root).unwrap();
        fs::create_dir_all(&cfg.system_root).unwrap();
        let drive_root = base.join("drive");
        fs::create_dir_all(&drive_root).unwrap();
        // 让它看起来是一块已初始化的备份盘:run_plan 重验会查盘上 id 文件是否在线。
        fs::create_dir_all(paths::drive_info_dir(&drive_root)).unwrap();
        fs::write(paths::drive_id_path(&drive_root), "备份1").unwrap();
        let drive = DriveInfo {
            letter: "T".into(),
            root: drive_root,
            id: "备份1".into(),
            sealed: false,
            free_bytes: 1 << 40,
            total_bytes: 1 << 40,
        };
        (d, cfg, drive)
    }

    fn test_opts() -> Options {
        Options {
            dry_run: false,
            no_hash: false,
            limit: 0,
            drive_letter_override: None,
            no_test_archives: false,
        }
    }

    struct RecordingReporter(std::sync::Mutex<Vec<String>>);
    impl crate::reporter::Reporter for RecordingReporter {
        fn log(&self, level: crate::reporter::LogLevel, msg: &str) {
            self.0.lock().unwrap().push(format!("{:?} {}", level, msg));
        }
        fn progress_bytes(&self, _l: &str, _t: u64) -> Box<dyn crate::reporter::ProgressHandle> {
            struct P;
            impl crate::reporter::ProgressHandle for P {
                fn inc(&mut self, _: u64) {}
                fn finish(&mut self) {}
            }
            Box::new(P)
        }
    }

    // ── L-003: 空源(0 真实文件)不被记成功、不移源 ──
    #[test]
    fn handle_one_empty_source_does_not_move() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001empty");
        fs::create_dir_all(&proj).unwrap();
        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        assert!(matches!(outcome, HandleOutcome::Skipped), "空源应 Skipped");
        assert!(proj.is_dir(), "空源不应被移走(应仍在 待备份)");
    }

    // ── happy-path:正常项目完整走通 复制→校验→提交→移源(覆盖 L-001 fsync 提交链路) ──
    #[test]
    fn handle_one_happy_path_archives_and_moves() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        fs::write(proj.join("b.bin"), b"world!!").unwrap();
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        let outcome = handle_one(&cfg, &rep, &drive, &proj, &test_opts(), None, None).unwrap();
        let log = rep.0.lock().unwrap().join("\n");
        assert!(
            matches!(outcome, HandleOutcome::Done),
            "正常项目应 Done;日志:\n{}",
            log
        );
        assert!(!proj.exists(), "源应已移到 已备份");
        assert!(cfg.archived_root.join("001proj").is_dir(), "源应在 已备份");
        assert!(
            paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .is_dir(),
            "目标盘应有项目副本"
        );
        assert!(
            paths::drive_manifest_dir(&drive.root)
                .join("001proj.sha256.csv")
                .is_file(),
            "应写出校验清单"
        );
    }

    // ── L-014: copy_folder 原子复制,内容正确且不遗留 .bftool-part ──
    #[test]
    fn copy_folder_atomic_no_part_left() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("s");
        let dst = d.path().join("t");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("sub").join("b.bin"), b"xyz").unwrap();
        copy_folder(&src, &dst, &NoopReporter).unwrap();
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("sub").join("b.bin")).unwrap(), b"xyz");
        assert!(!dst.join("a.txt.bftool-part").exists(), "不应遗留 .part");
    }

    // ── L-017 修订(Phase 4 F-4):本盘索引损坏 → fail-closed Skipped,不覆盖也不重复写盘 ──
    #[test]
    fn handle_one_corrupt_drive_catalog_fails_closed() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        let cat = paths::drive_catalog_path(&drive.root);
        if let Some(p) = cat.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&cat, [0xff, 0xfe, 0x00]).unwrap(); // 非 UTF-8 → 读索引必失败
        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        assert!(
            matches!(outcome, HandleOutcome::Skipped),
            "索引损坏应 fail-closed Skipped"
        );
        assert!(proj.is_dir(), "不应移源");
        assert!(
            !paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .exists(),
            "不应写盘"
        );
    }

    // ── L-018: 台账写失败不致命(note_manual 返回 () 不向上抛) ──
    #[test]
    fn note_manual_infallible_when_system_root_unwritable() {
        let (_d, mut cfg, _drive) = temp_world();
        let bogus = cfg.system_root.join("not_a_dir");
        fs::write(&bogus, b"x").unwrap();
        cfg.system_root = bogus; // system_root 指向文件 → 台账写入必失败
                                 // 关键:返回 () 且不 panic —— 写失败只 warn,不会中止整轮归档
        note_manual(&cfg, &NoopReporter, "proj", "校验失败：xxx");
    }

    // ── L-020: VerifyStatus token 稳定且单一来源 ──
    #[test]
    fn verify_status_tokens() {
        assert_eq!(VerifyStatus::from_opts(false).token(), "SHA256-OK");
        assert_eq!(VerifyStatus::from_opts(true).token(), "SIZE+COUNT");
    }

    // ── L-008: 无校验判定单一来源,core/cli 共用 verify_disabled ──
    #[test]
    fn verify_disabled_truth_table() {
        // no_hash=false → 始终有 SHA256 兜底,绝不算无校验
        assert!(!verify_disabled(false, true, false));
        assert!(!verify_disabled(false, false, true));
        // no_hash=true 且 archive test 开启 → 有压缩包测试兜底,允许
        assert!(!verify_disabled(true, true, false));
        // no_hash=true 且 test_archives=false → 无校验,禁止
        assert!(verify_disabled(true, false, false));
        // no_hash=true 且 no_test_archives=true → 无校验,禁止
        assert!(verify_disabled(true, true, true));
    }

    // ── Spec D §4.1: decide() 只读裁决 Archive / Skip(无可备份) / SealAndStop(余量不足) ──
    #[test]
    fn decide_archive_skip_and_seal_and_stop() {
        let (_d, cfg, drive) = temp_world();
        // 正常项目 → Archive{dest_name}
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        let it1 = decide(&cfg, &drive, &p1);
        assert!(
            matches!(&it1.action, PlanAction::Archive { dest_name } if dest_name == "001proj"),
            "正常项目应 Archive,得到 {:?}",
            it1.action
        );
        assert_eq!(it1.est_bytes, 5);
        // 空源 → Skip("无可备份…")
        let p2 = cfg.ready_root.join("002empty");
        fs::create_dir_all(&p2).unwrap();
        let it2 = decide(&cfg, &drive, &p2);
        assert!(
            matches!(&it2.action, PlanAction::Skip(r) if r.contains("可备份的真实文件")),
            "空源应 Skip(无可备份的真实文件),得到 {:?}",
            it2.action
        );
        // 余量不足 → SealAndStop(reserve_gb=0,free=1 字节,项目 4KB)
        let mut tiny = drive.clone();
        tiny.free_bytes = 1;
        let p3 = cfg.ready_root.join("003big");
        fs::create_dir_all(&p3).unwrap();
        fs::write(p3.join("x.bin"), vec![0u8; 4096]).unwrap();
        let it3 = decide(&cfg, &tiny, &p3);
        assert!(
            matches!(it3.action, PlanAction::SealAndStop(_)),
            "余量不足应 SealAndStop,得到 {:?}",
            it3.action
        );
    }

    // ── Spec D §4.1: run_plan 执行冻结目标名 → 归档 + 移源 ──
    #[test]
    fn run_plan_executes_frozen_name_and_moves_source() {
        let (_d, cfg, drive) = temp_world();
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "001proj".into(),
                est_bytes: 5,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let s = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap();
        assert_eq!(s.handled, 1);
        assert_eq!(s.failed, 0);
        assert!(!p1.exists(), "源应已移到 已备份");
        assert!(
            paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .is_dir(),
            "目标盘应有项目副本"
        );
    }

    // ── Spec D §4.1: 预览后盘被封 → run_plan 重验判定"计划已过期",不写盘不移源 ──
    #[test]
    fn run_plan_stale_when_drive_sealed_after_plan() {
        let (_d, cfg, drive) = temp_world();
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "001proj".into(),
                est_bytes: 5,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        // 预览之后、执行之前:盘被封
        drive::seal(&drive).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        let s = run_plan(&cfg, &plan, &cancel, &rep).unwrap();
        assert_eq!(s.handled, 0, "封盘后不执行");
        assert!(s.sealed_stopped, "应标记封盘停本轮");
        assert!(p1.is_dir(), "源不应被移动");
        assert!(
            !paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .exists(),
            "不应写盘"
        );
        let log = rep.0.lock().unwrap().join("\n");
        assert!(
            log.contains("计划已过期"),
            "应提示计划已过期;日志:\n{}",
            log
        );
    }

    // ── Spec D §4.1: 冻结目标名在执行前被占用 → StalePlan,不写盘不移源 ──
    #[test]
    fn run_plan_stale_when_frozen_name_taken() {
        let (_d, cfg, drive) = temp_world();
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        // 预览给的是普通名 001proj;但执行前本盘索引里已出现 001proj(被他人占用)
        let cat = paths::drive_catalog_path(&drive.root);
        if let Some(p) = cat.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(
            &cat,
            "ProjectNo,ProjectName,FileCount,TotalBytes,ArchivedUTC,VerifyStatus,Status,Notes\n\
             001,001proj,1,5,2026-05-29T00:00:00+00:00,SHA256-OK,Complete,\n",
        )
        .unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "001proj".into(),
                est_bytes: 5,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        let s = run_plan(&cfg, &plan, &cancel, &rep).unwrap();
        assert_eq!(s.handled, 0, "冻名被占不执行");
        assert!(p1.is_dir(), "源不应被移动");
        let log = rep.0.lock().unwrap().join("\n");
        assert!(
            log.contains("计划已过期"),
            "应提示计划已过期;日志:\n{}",
            log
        );
    }
}

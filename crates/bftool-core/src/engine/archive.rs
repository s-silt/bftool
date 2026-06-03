//! 主归档流程：扫描 → 稳定性 → 容量 → 复制 → 校验 → 复核源 → 事务式提交。
//!
//! 顺序与不变量（与 PowerShell 旧版一致，且经实测）：
//! 1. 多块未封盘备份盘同时在线 → 立即停止（防写错盘）
//! 2. 单项目 try/catch 隔离，意外只跳过该项目不中断整轮
//! 3. 校验失败时把目标侧坏文件移到 异常文件/，下次自动重传
//! 4. 移动源前再次比对源（防"备份的是旧版本"）
//! 5. 写事务标记 → 写清单/索引 → 移动源 → 删标记（索引先于移源落盘;write_csv/catalog 失败源仍可重做）

use anyhow::{Context, Result};
use chrono::{Local, Utc};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use walkdir::WalkDir;

use crate::config::Config;
use crate::engine::archive_test::{self, Tester, TesterPaths};
use crate::engine::drive::{self, BackupDrive, DriveInfo};
use crate::engine::manifest::{self, ManifestOpts};
use crate::engine::{cruft, durable, paths, safety, txn};
use crate::reporter::Reporter;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Options {
    pub dry_run: bool,
    pub no_hash: bool,
    /// 本轮最多处理多少个项目。`0 = 不限`（处理本轮计划里的全部项目）。
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
    /// `plan()` 时目标目录是否已经存在。存在且未入索引通常表示上轮复制阶段中断留下的
    /// 可续传半成品；`run_plan()` 允许继续它，但仍拒绝预览之后才出现的目标占用。
    pub dest_existed_at_plan: bool,
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
///
/// SEC-008: 三个 bool 参数语义重叠，签名暂不改（改了牵动 core+cli 两处调用点，
/// 风险中、收益低）。三者各自含义与组合真值如下，调用方务必按命名传值：
/// - `no_hash`        —— 用户是否传了 `--unsafe-no-hash`（**关掉** SHA256 整文件校验）。
/// - `test_archives`  —— 配置项 `cfg.test_archives`（是否**启用**压缩包内部测试）。
/// - `no_test_archives` —— 本轮 `opts.no_test_archives`（是否**临时关掉**压缩包测试，覆盖配置）。
///
/// 「archive test 实际开启」≡ `test_archives && !no_test_archives`（配置开 且 本轮没关）。
/// 「无校验」≡ `no_hash && !(test_archives && !no_test_archives)`，化简即下式。真值表：
///
/// | no_hash | test_archives | no_test_archives | 结果(=无校验) |
/// |---------|---------------|------------------|----------------|
/// | false   | *             | *                | false（SHA256 兜底） |
/// | true    | true          | false            | false（压缩包测试兜底） |
/// | true    | false         | *                | **true**（配置没开测试） |
/// | true    | true          | true             | **true**（本轮关了测试） |
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

    let warnings = safety::check_paths(
        &cfg.ready_root,
        &cfg.archived_root,
        &cfg.system_root,
        Some(&drive.root),
    )?;
    for w in warnings {
        reporter.warn(&w);
    }

    // 列出待归档项目(读不到/为空 → 空计划,友好提示)。
    let mut items = Vec::new();
    if !cfg.ready_root.is_dir() {
        // AR-16: ready_root 非目录时**有意**返回空 items 而非 Err —— 让 run/run_plan 走
        // 友好的「本轮无项目可做」收尾路径(打印提示、退出码 0)，而不是抛硬错误把
        // CLI/GUI 弹成异常。配置写错盘符等情况下，用户看到的是可读提示而非堆栈。
        reporter.error(&format!("待备份 不存在：{}", cfg.ready_root.display()));
        return Ok(ArchivePlan {
            drive,
            items,
            opts: opts.clone(),
        });
    }
    let projects = discover_projects(&cfg.ready_root, reporter)?;
    if projects.is_empty() {
        reporter.info("待备份 中没有待归档项目，结束。");
    } else {
        reporter.info(&format!(
            "发现 {} 个待归档项目（按编号升序处理）。",
            projects.len()
        ));
    }
    items = plan_items(cfg, &drive, &projects);
    Ok(ArchivePlan {
        drive,
        items,
        opts: opts.clone(),
    })
}

/// 对一组项目算计划:跨项目**递减剩余容量**。每裁决一个 Archive/RenameAndArchive 就从
/// running_free 扣掉它的占用(est_bytes),使后续项目看到「前面项目占用后」的剩余。
/// 否则每个项目都对同一份冻结 free_bytes 独立判断 → 多个项目各自放得下、累计放不下时
/// 不会触发封盘,预览高估、执行才在磁盘满时失败。(review-r2 #2)
fn plan_items(cfg: &Config, drive: &BackupDrive, projects: &[PathBuf]) -> Vec<PlanItem> {
    let mut items = Vec::with_capacity(projects.len());
    let mut running_free = drive.free_bytes;
    for proj in projects {
        let mut drive_now = drive.clone();
        drive_now.free_bytes = running_free;
        let item = decide(cfg, &drive_now, proj);
        if matches!(
            item.action,
            PlanAction::Archive { .. } | PlanAction::RenameAndArchive { .. }
        ) {
            running_free = running_free.saturating_sub(item.est_bytes);
        }
        items.push(item);
    }
    items
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
                dest_existed_at_plan: false,
                action: PlanAction::Skip("无效的项目目录名".into()),
            }
        }
    };
    let skip = |est: u64, reason: String| PlanItem {
        name: name.clone(),
        est_bytes: est,
        dest_existed_at_plan: false,
        action: PlanAction::Skip(reason),
    };

    // 稳定性(未稳定是暂态:跳过不记台账,下轮再来)
    let st = safety::folder_stable(proj_path, cfg.stable_minutes);
    if !st.stable {
        return skip(0, format!("未稳定：{}", st.reason));
    }
    match source_bftool_part_files(proj_path) {
        Ok(files) if !files.is_empty() => {
            return skip(
                0,
                format!(
                    "项目内含 bftool 内部临时后缀 .bftool-part 的文件({})，为避免静默漏备已跳过；请改名或人工核对。",
                    files.join("、")
                ),
            )
        }
        Ok(_) => {}
        Err(e) => {
            return skip(
                0,
                format!("扫描 .bftool-part 文件失败({})→ 不归档,请先解决环境问题。", e),
            )
        }
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
    let reserve = cfg.reserve_gb.saturating_mul(1024 * 1024 * 1024);
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
    // AR-11: 时间戳精度为秒(%Y%m%d%H%M%S)。同一秒内对两个同名项目算计划，会得到相同的时间戳目标名。
    // 这种「同秒 + 同名」极罕见(秒级竞态 + 重名两条件叠加)，且即便发生也不会写坏数据：
    // run_plan 执行前 handle_one 会用 forced_dest_name 复验该名是否已被占用(索引或磁盘)，
    // 命中即判 StalePlan 跳过、不照旧误写。这里接受秒级精度，不引入更细粒度(纳秒会让目标名难读)。
    let dest_name = if dup {
        format!("{}_{}", name, Local::now().format("%Y%m%d%H%M%S"))
    } else {
        name.clone()
    };
    // 余量(断点续传时仅按"还需写入"判断)→ 不足则封盘停本轮
    let dest = paths::drive_projects_dir(&drive.root).join(&dest_name);
    let dest_existed_at_plan = dest.exists();
    // dest_have 可能含孤儿 .bftool-part 文件(copy_folder 开头会先删它,最终不影响完成大小)。
    // 此处轻微高估已有量导致 need 偏小,极端情况下可能多余地触发封盘;属已知可接受的精度损失。
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
            dest_existed_at_plan,
            action: PlanAction::SealAndStop(reason),
        };
    }
    PlanItem {
        name,
        est_bytes: size,
        dest_existed_at_plan,
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

    // 先建 system_root(锁文件与事务标记都在此),再取进程级归档锁 —— 必须在路径安全检查之后建,
    // 避免坏配置先污染。R6-4:锁防两个 bftool 实例(CLI+CLI / CLI+GUI)并发归档同一套配置/盘。
    fs::create_dir_all(&cfg.system_root).context("创建 备份系统 目录失败")?;
    // RAII:_archive_lock 在 run_plan 返回(含 panic 展开,release 已用 panic=unwind,见 R5-8)时 Drop 删锁。
    // review-r3 round4:锁必须**先于** check_pending_txn 获取 —— 后者会在恢复分支主动 clear_marker
    // 删除事务标记,这属于受锁保护的恢复状态变更。若放在锁外,并发实例可在持锁实例提交途中删掉其活动
    // 标记。把锁上移使「读取/清除/重做事务标记」整体处于跨进程互斥内。
    let _archive_lock = ArchiveLock::acquire(&cfg.system_root)?;

    // 启动自检(已在锁内):上次的事务标记是不是残留？未解决时必须先停,避免新归档覆盖旧恢复证据。
    // 传入本轮目标盘 id(上面 L497-519 已实时复验过盘内编号):若残留事务写在另一块盘上,
    // 重做前会 fail-closed,避免抹掉原盘那份中断副本的恢复证据。(review-r3 #2)
    check_pending_txn(cfg, reporter, &drive.id)?;

    // 其余系统/归档目录:在事务自检之后建,避免未解决事务时就对归档目标区产生新写入。
    fs::create_dir_all(paths::system_logs_dir(&cfg.system_root)).ok();
    fs::create_dir_all(&cfg.archived_root).context("创建 已备份 目录失败")?;

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
        // Skip/SealAndStop 的预览项也走 handle_one(forced=None)重新裁决:这样安全前置实时重验、
        // 台账(需人工处理.txt)按真实结果记;Archive/RenameAndArchive 则带上冻结目标名。
        let proj_path = cfg.ready_root.join(&item.name);
        let forced = match &item.action {
            PlanAction::Archive { dest_name } | PlanAction::RenameAndArchive { dest_name } => {
                Some((dest_name.as_str(), item.dest_existed_at_plan))
            }
            PlanAction::Skip(_) | PlanAction::SealAndStop(_) => None,
        };
        // 把可见剩余传给 handle_one。R6-1:取 min(投影剩余, 实时剩余):consumed 处理本工具自身
        // 串行占用;但 plan→run 之间若有外部进程往备份盘写入,实时剩余会更低 —— 实时重查校正,
        // 查询失败(盘离线等)则退回投影值(保守:不放大可用量)。
        let mut drive_now = drive.clone();
        let projected = drive.free_bytes.saturating_sub(consumed);
        let live_free = drive::info_by_letter(&drive.letter)
            .ok()
            .map(|d| d.free_bytes);
        drive_now.free_bytes = match live_free {
            Some(live) => projected.min(live),
            None => projected,
        };
        match handle_one(
            cfg,
            reporter,
            &drive_now,
            &proj_path,
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
                // 触发该分支的本项目在 handle_one 的压缩包测试阶段已提前返回、**本轮未归档**(只是关掉了
                // 后续项目的压缩包测试)。计 failed,使 CLI 退出非零、自动化/计划任务能感知「有项目未成功」,
                // 与 L-007『批量归档里有失败要 exit 非零』一致。(review-r3 round5)
                summary.failed += 1;
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

/// fail-closed 守卫:无校验组合(no_hash + archive test 关闭)直接拒。(ledger L-008)
fn guard_verify_enabled(cfg: &Config, opts: &Options) -> Result<()> {
    if verify_disabled(opts.no_hash, cfg.test_archives, opts.no_test_archives) {
        // EH-006: 纯用户向文案（不含 core/调用方等内部开发说明），CLI 与 GUI 都直接展示。
        anyhow::bail!(
            "已停止：当前设置会让 SHA256 校验和压缩包测试同时关闭 —— 等于「没有做任何完整性校验」，\
             不能这样备份。\n\
             如何修（任选其一）：\n\
               - 不要使用 --unsafe-no-hash（让 SHA256 整文件校验兜底）；或\n\
               - 开启压缩包测试（不要把 test_archives 设为 false，也不要使用 --no-test-archives）"
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
                // AR-08: 与 guard_verify_enabled 保持一致的措辞风格——都是"拒绝运行：…等价于没在做完整性校验"。
                anyhow::bail!(
                    "拒绝运行：开启了 --unsafe-no-hash 但没有可用的压缩包测试器(WinRAR/Bandizip/7-Zip 均未检测到)。\n\
                     此时 SHA256 与 archive test 均不运行,等价于「没在做完整性校验」。\n\
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
    /// 归档成功;携带**实际归档的源字节数**,供 run_plan 用真实占用累计 consumed。(review-r2 R2-2)
    Done(u64),
    Skipped,
    /// 冻结的目标名在执行时已被占用(预览→执行间状态变了)——不写盘,不计 failed。(Spec D §4.1)
    StalePlan,
    DriveSealed,
    TesterFatallyDisabled,
    /// 提交中途移源失败:索引已落盘、源未移动,事务标记**保留**待恢复。run_plan 收到后必须
    /// 立即中止本轮,否则下一个项目的 pending.write 会覆盖该标记、抹掉恢复证据。(review-r2 R3-3)
    CommitInterrupted,
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
    forced_dest_name: Option<(&str, bool)>,
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

    match source_bftool_part_files(proj_path) {
        Ok(files) if !files.is_empty() => {
            let msg = format!(
                "项目内含 bftool 内部临时后缀 .bftool-part 的文件({})，为避免静默漏备已跳过。\
                 请改名或人工核对后重试。",
                files.join("、")
            );
            reporter.error(&msg);
            note_manual(cfg, reporter, &name, &msg);
            return Ok(HandleOutcome::Skipped);
        }
        Ok(_) => {}
        Err(e) => {
            let msg = format!(
                "扫描 .bftool-part 文件失败({})→ 不归档,请先解决环境问题。",
                e
            );
            reporter.error(&msg);
            note_manual(cfg, reporter, &name, &msg);
            return Ok(HandleOutcome::Skipped);
        }
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
        Some((frozen, dest_existed_at_plan)) => {
            let drive_projects = paths::drive_projects_dir(&drive.root);
            let dest_phys = drive_projects.join(frozen);
            // 索引检查:frozen==name 时用 dup_in_drive;重命名场景重新查 catalog。
            // 额外 OR 物理路径检查:索引与磁盘不一致时(写索引前宕机等),磁盘上已有目录也算被占用。
            let indexed = if frozen == name {
                dup_in_drive
            } else {
                match catalog_has_project(&catalog, frozen) {
                    Ok(b) => b,
                    Err(e) => {
                        reporter.error(&format!(
                            "读本盘索引失败({}):无法复验冻结目标名 → 跳过本项目,避免覆盖旧备份或重复写盘。请检查 {}",
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
                }
            };
            let occupied_after_plan = dest_phys.exists() && !dest_existed_at_plan;
            let taken = indexed || occupied_after_plan;
            if indexed {
                reporter.warn(&format!(
                    "计划已过期：目标名『{}』在本盘索引中已存在(预览后被占用)→ 跳过本项目,请重新规划。",
                    frozen
                ));
                return Ok(HandleOutcome::StalePlan);
            }
            if occupied_after_plan {
                reporter.warn(&format!(
                    "计划已过期：目标目录『{}』在预览后出现在磁盘上 → 跳过本项目,请重新规划。",
                    frozen
                ));
                return Ok(HandleOutcome::StalePlan);
            }
            if taken {
                // Defensive fallback if future conditions are added above.
                reporter.warn(&format!(
                    "计划已过期：目标名『{}』已被占用 → 跳过本项目,请重新规划。",
                    frozen
                ));
                return Ok(HandleOutcome::StalePlan);
            };
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
    let reserve = cfg.reserve_gb.saturating_mul(1024 * 1024 * 1024);
    if size > drive.total_bytes.saturating_sub(reserve) {
        reporter.error(&format!(
            "项目 {:.2}GB 超过单盘容量，空盘也放不下 → 需人工拆分",
            size_gb
        ));
        note_manual(
            cfg,
            reporter,
            &name,
            &format!("{:.2}GB 超过单盘容量，需拆分或显式跨盘", size_gb),
        );
        return Ok(HandleOutcome::Skipped);
    }
    if need.saturating_add(reserve) > drive.free_bytes {
        let need_gb = need as f64 / 1024.0 / 1024.0 / 1024.0;
        let free_gb = drive.free_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        reporter.warn(&format!(
            "盘 {} 余量不足以放下 {}（还需约 {:.2}GB + 余量；当前剩余 {:.2}GB）。封盘并请换下一块盘后重跑。",
            drive.id, name, need_gb, free_gb
        ));
        // R6-2:封盘标记写失败(盘只读/被占用/掉线)不应退化成逐项目 Err 重试(余量仍不足,
        // 每个后续项目都会再来一次)。无论标记是否写成功,余量不足都按 DriveSealed 终止本轮。
        if let Err(e) = drive::seal(drive) {
            reporter.error(&format!(
                "封盘标记写入失败({}):盘 {} 余量已不足、本轮就此停止。请取下本盘换下一块空盘;\
                 如该盘还要继续用,请手工在盘根 本盘信息\\ 下创建 已封盘.txt 或排查占用后重跑。",
                e, drive.id
            ));
        } else {
            reporter.action(&format!(
                "已封盘 {}。取下本盘、插上下一块空盘(NTFS)后，运行 `bftool init <盘符>` 初始化再继续。",
                drive.id
            ));
        }
        return Ok(HandleOutcome::DriveSealed);
    }

    // dry_run 路径由 run() 在 plan() 后直接 render_dry_run(),不会进入 run_plan/handle_one。
    // 此处 dry_run 分支仅供直接调用 handle_one(如旧测试)触发时兜底,正常流程不会到达。
    if opts.dry_run {
        reporter.info(&format!(
            "[演练] 将归档 {} ({} 个文件, {:.2}GB) → {}",
            name, stats.files, size_gb, drive.id
        ));
        return Ok(HandleOutcome::Done(0)); // 演练不写盘 → 0 字节
    }

    // Spec B 源压缩包测试 —— 在 dry_run check 之后,在生成 manifest 之前
    if let Some((kind, path)) = tester_opt {
        let r =
            archive_test::test_folder(proj_path, (*kind, path.as_path()), opts.no_hash, reporter);
        if !r.ok {
            for detail in r.details() {
                reporter.error(&detail);
            }
            note_manual(
                cfg,
                reporter,
                &name,
                &format!("源压缩包测试失败：{}", r.summary()),
            );
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
            note_manual(
                cfg,
                reporter,
                &name,
                &format!(
                    "unsafe_no_hash 模式下项目有 {} 个未覆盖文件 → 这些文件无完整性校验。\
                     请去掉 --unsafe-no-hash 让 SHA256 兜底,或把非压缩包内容打包成一个\
                     压缩包后再归档。",
                    r.uncovered_files
                ),
            );
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
        note_manual(
            cfg,
            reporter,
            &name,
            "项目内没有可备份的真实文件(空目录或全是 cruft)——不归档、不移源,请人工确认。",
        );
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
        note_manual(
            cfg,
            reporter,
            &name,
            &format!("校验失败：{}", d.reasons.join("; ")),
        );
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
            note_manual(
                cfg,
                reporter,
                &name,
                &format!("目标压缩包测试失败：{}", r.summary()),
            );
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
        note_manual(
            cfg,
            reporter,
            &name,
            &format!("源在复制期间变化：{}", why.join("; ")),
        );
        return Ok(HandleOutcome::Skipped);
    }

    // 事务式提交
    // AR-01 提交顺序:写标记 → 写清单/索引 → rename 移源 → 清标记。
    // 这样如果 write_csv/catalog 失败,源还在 待备份,下次可以重做。
    // rename 失败时不清标记(抛错 → 标记留着,下次 check_pending_txn 感知)。
    let local_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let utc = Utc::now().to_rfc3339();
    let src_bytes = src.total_bytes();
    let size_gbval = src_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    let verify_status = VerifyStatus::from_opts(opts.no_hash);
    let manifest_path =
        paths::drive_manifest_dir(&drive.root).join(format!("{}.sha256.csv", dest_name));
    let rel_manifest = format!("本盘信息\\校验清单\\{}.sha256.csv", dest_name);
    // AR-04: arch_dest 的最终值(含时间戳冲突处理)在 pending.write 之前确定,
    // 保证 PendingTxn.move_to 记录的是真正的目标路径。
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

    // 写清单 + 本盘索引 + 全局索引(在 rename 之前写)。R5-2:这三处写在**写标记之后**,任一失败必须
    // 中止本轮(CommitInterrupted),否则 handle_one 返回普通 Err、run_plan 继续,下个项目的
    // pending.write 会覆盖本项目残留的事务标记、抹掉崩溃恢复证据(与移源失败的 R3-3 守卫对称)。
    let commit_writes = (|| -> Result<()> {
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
        Ok(())
    })();
    if let Err(e) = commit_writes {
        reporter.error(&format!("写索引失败(提交中断):{:#}", e));
        reporter.action(
            "事务标记已保留,下次启动会据此恢复;本轮就此中止,避免后续项目覆盖该标记。\
             请解决写索引失败的原因(备份盘断开/写满/索引损坏等)后重跑 `bftool archive`。",
        );
        return Ok(HandleOutcome::CommitInterrupted);
    }

    // 移动源（索引已落盘后再移）。失败 → 不清标记、**中止本轮**(CommitInterrupted),源留待备份下次重做。
    if let Err(e) = fs::rename(proj_path, &arch_dest) {
        // 跨卷 rename 在 Windows 返回 ERROR_NOT_SAME_DEVICE(17):给可操作的 fail-closed 提示。(ledger L-010)
        let msg = if e.raw_os_error() == Some(17) {
            format!(
                "移动源失败:待备份({})与已备份({})不在同一磁盘卷,无法原子移动。\n\
                 如何修:把 ready_root 与 archived_root 配到同一块盘(通常都在你的 SSD 上)。\n\
                 项目仍留在 待备份,改好配置后会自动重做(目标盘上的副本+校验清单已写好)。",
                proj_path.display(),
                arch_dest.display()
            )
        } else {
            format!(
                "移动源失败:{} → {}: {}",
                proj_path.display(),
                arch_dest.display(),
                e
            )
        };
        reporter.error(&msg);
        // R3-3:索引已落盘、移源失败 → 保留事务标记并**中止本轮**。否则后续项目的
        // pending.write 会覆盖本项目的标记、抹掉 check_pending_txn 的恢复证据。
        reporter.action(
            "事务标记已保留,下次启动会据此恢复;本轮就此中止,避免后续项目覆盖该标记。\
             请解决移源失败的原因(盘符冲突/文件被占用等)后重跑 `bftool archive`。",
        );
        return Ok(HandleOutcome::CommitInterrupted);
    }

    // R6-3:此刻事务在数据层面已完整完成(源已移、三处索引已落盘),只剩删一个无意义的残留标记。
    // 删标记失败(AV 占用/只读/瞬时 IO)不应把成功归档误报成 failed/非零退出 —— 用 warn-only 的
    // clear_marker(与恢复路径一致),标记残留无害、下次 check_pending_txn 的 moved&&indexed 分支自愈。
    clear_marker(&txn_path, reporter);
    reporter.ok(&format!(
        "✓ {} 归档+校验成功 → {}\\项目\\{}；SSD 源已移到 已备份（未删除）。",
        name, drive.id, dest_name
    ));
    Ok(HandleOutcome::Done(src_bytes))
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
    append_catalog_row(path, row)
}

fn append_global_catalog(path: &Path, row: &GlobalCatalogRow) -> Result<()> {
    append_catalog_row(path, row)
}

/// 把一行追加进 CSV 索引并**原子落盘**:读现有内容到内存 → 追加序列化的新行(文件不存在时
/// 连表头一起)→ durable::write_synced(同目录 tmp + fsync + rename)整体替换。
///
/// 旧实现用 `OpenOptions::append` + fsync,断电恰发生在"扇区已分配、字节未写完"时会在 CSV 末尾
/// 留下半行,让后续 catalog_has_project / global_has_folder / next_drive_number / seal 的解析
/// fail-closed(跳过项目/恢复 bail/选号失败),需用户手动修 CSV。原子 rename 保证读者只看到
/// 旧索引或完整新索引,绝不半行 —— 与 manifest/事务标记的原子写一致。(review-r2 R2-4 / L-001)
///
/// AR-10:`file_existed` 探测与写入之间的 TOCTOU 在单进程串行归档下可接受(无并发写者)。
fn append_catalog_row<R: serde::Serialize>(path: &Path, row: &R) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).ok();
    }
    let existed = path.is_file();
    let mut content = if existed {
        fs::read(path).with_context(|| format!("读索引失败：{}", path.display()))?
    } else {
        Vec::new()
    };
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(!existed) // 文件不存在时连表头一起写
        .from_writer(Vec::new());
    wtr.serialize(row)?;
    wtr.flush()?;
    let row_bytes = wtr
        .into_inner()
        .map_err(|e| anyhow::anyhow!("序列化索引行失败：{}", e))?;
    content.extend_from_slice(&row_bytes);
    durable::write_synced(path, &content)
        .with_context(|| format!("写索引失败：{}", path.display()))?;
    Ok(())
}

fn catalog_has_project(catalog: &Path, project_name: &str) -> Result<bool> {
    if !catalog.is_file() {
        return Ok(false);
    }
    let mut rdr = csv::Reader::from_path(catalog)?;
    let headers = rdr.headers()?.clone();
    // 文件存在且能作为 CSV 打开,但缺 ProjectName 列 → 索引不可信(损坏/被改格式),不能当作「空索引/未登记」
    // 静默返回 Ok(false)(那会让恢复判定把『已索引』误判为『未索引』→ 重做产生重复副本/漏判重名)。
    // 与 CSV 解析失败的 fail-closed 一致:bail,让调用方走既有 skip+note_manual / 上抛路径。(review-r3 round5)
    let Some(col) = headers.iter().position(|h| h == "ProjectName") else {
        anyhow::bail!(
            "本盘索引缺少 ProjectName 列,索引不可信(可能损坏或被改格式):{}",
            catalog.display()
        );
    };
    for rec in rdr.records() {
        let rec = rec?;
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
    // EH-009: 追加后 fsync，与 catalog 写入(append_drive_catalog/append_global_catalog)的落盘
    // 语义一致。「需人工处理.txt」本身不是关键事务(它是给人看的提醒台账，丢一行不影响数据安全)，
    // 但这里 sync 成本极低且能保证断电后提醒不丢；用 durable::sync_file 对追加场景同样适用。
    durable::sync_file(&f)?;
    Ok(())
}

fn check_pending_txn(cfg: &Config, reporter: &dyn Reporter, redo_drive_id: &str) -> Result<()> {
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
            anyhow::bail!(
                "发现未完成的事务标记但解析失败。为避免覆盖恢复证据,本轮归档已停止；请人工核对后删除：{}",
                path.display()
            );
        }
    };
    let pname = pending.project_dest_name.clone();
    let parch = pending.move_to.clone();
    // AR-09: 判断「源是否仍在待备份」时，绝不能回退到 project_dest_name —— 重名场景下它带秒级
    // 时间戳后缀(如 001proj_20260529...)，而待备份里的源名永远是**原始名**(001proj)，
    // 拿带时间戳的名去 ready_root 拼路径会永远查不到、把「源还在」误判成「源已移走」。
    // 可靠来源优先级：
    //   1. project_src_name —— 现版本一定会写的原始项目名；
    //   2. src_path —— 写标记时记录的源**完整路径**，直接 exists() 最可靠(兼容旧标记)；
    //   3. 两者都没有(极旧标记) → 无法判定源名，不猜，in_ready=false 交给「无法自动判定」分支
    //      保留标记请人工核对，而不是用时间戳名误判。
    let in_ready = if !pending.project_src_name.is_empty() {
        cfg.ready_root.join(&pending.project_src_name).exists()
    } else if !pending.src_path.is_empty() {
        Path::new(&pending.src_path).exists()
    } else {
        false
    };
    let moved = !parch.is_empty() && Path::new(&parch).exists();
    let global = paths::system_global_catalog(&cfg.system_root);
    let indexed = match global_has_folder(&global, &pname) {
        Ok(v) => v,
        Err(e) => {
            reporter.error(&format!(
                "→ 无法读取全局索引确认上次事务是否完成({})。标记保留：{}",
                e,
                path.display()
            ));
            anyhow::bail!(
                "发现未完成的事务,且无法确认全局索引状态。为避免覆盖恢复证据,本轮归档已停止。"
            );
        }
    };

    reporter.action(&format!("发现上次未完成的事务（项目：{}）。", pname));
    // in_ready 加 !(moved && indexed) 守卫:「事务其实已完成(源已移、索引已写)、仅 clear 标记前崩溃」
    // 叠加「用户在待备份重建了同名源」时,应判定为已完成(走下面 moved&&indexed 分支只清标记),
    // 而非误判为需重做。(review-r2 R4-1)
    if in_ready && !(moved && indexed) {
        // review-r3 #2:重做并清标记前,确认本次重做的目标盘就是当时写入的那块盘。进入本分支意味着
        // 「副本已写到原备份盘且通过 SHA256、源未移走」—— 原盘上必有一份已校验的孤儿副本+清单。
        // 若本次重做要落到另一块盘(用户换了盘 / 原盘离线),清掉标记会抹掉指向原盘那份孤儿的唯一
        // 恢复证据。改为 fail-closed:保留标记、不重做,提示用户插回原盘核对/清理。绝不删除任何东西
        // (与「恢复路径不删除」一致)。redo_drive_id 来自 run_plan 已实时复验过盘内编号的目标盘。
        // 标记无 drive_id(极旧标记缺该字段)时跳过此校验,维持原有重做行为。
        if !pending.drive_id.trim().is_empty() && pending.drive_id.trim() != redo_drive_id.trim() {
            reporter.error(&format!(
                "→ 上次中断的副本写在备份盘「{}」上(盘上可能残留一份已校验的项目副本+校验清单),\
                 而本轮将写入「{}」。为不丢失对原盘那份副本的恢复证据,本轮归档已停止、标记保留。\
                 请插回备份盘「{}」核对/清理后再归档,或确认无误后手动删除标记：{}",
                pending.drive_id.trim(),
                redo_drive_id.trim(),
                pending.drive_id.trim(),
                path.display()
            ));
            anyhow::bail!(
                "未完成的事务写在备份盘「{}」上,而本轮目标盘是「{}」—— 为避免丢失原盘上中断副本的恢复证据,本轮归档已停止(标记保留)。",
                pending.drive_id.trim(),
                redo_drive_id.trim()
            );
        }
        reporter
            .action("→ 源仍在『待备份』，说明移动尚未发生；本次会自动重做该项目。正在清除旧标记…");
        // 已知局限(review-r2 R3-2):若崩溃恰发生在「索引已写、移源未完成」的窄窗口,自动重做会在
        // 备份盘上产生一份带时间戳的重复副本(源始终保留、数据不丢,与既有"保守重做"设计、
        // crash_after_index_before_move_redo_is_idempotent 测试一致)。曾尝试在恢复路径清理孤儿
        // (删索引行/盘上副本),但删除依赖易变盘符且无法充分测试,两轮 re-review 各暴露一个 P1
        // 数据丢失风险(删错盘 / 删已完成备份),故撤回 —— **恢复路径绝不做任何删除**。
        clear_marker(&path, reporter);
    } else if moved && indexed {
        reporter.action("→ 已移动且索引中已有记录，判定为已完成（仅标记残留）。正在清除标记…");
        clear_marker(&path, reporter);
    } else if moved && !indexed {
        reporter.error(&format!(
            "→ 源已移到『已备份』但全局索引可能漏写：数据应在备份盘 {}。请人工核对并在索引中补登；标记保留。",
            parch
        ));
        anyhow::bail!(
            "发现未完成的事务：源已移动但全局索引未确认。为避免覆盖恢复证据,本轮归档已停止。"
        );
    } else {
        reporter.error(&format!(
            "→ 无法自动判定（源不在待备份、目标也未确认）。请人工核对事务标记后处理,标记保留：{}",
            path.display()
        ));
        anyhow::bail!("发现未完成的事务且无法自动判定状态。为避免覆盖恢复证据,本轮归档已停止。");
    }
    Ok(())
}

/// 清除事务标记。删除失败(AV 占用/只读/权限)时 warn 而非静默 —— 否则下轮启动会对一个
/// 已正确处理的事务重复触发恢复,且"已清除标记"的提示与磁盘实际状态(标记仍在)不符。(review-r2 R2-3)
fn clear_marker(path: &Path, reporter: &dyn Reporter) {
    if let Err(e) = fs::remove_file(path) {
        reporter.warn(&format!(
            "清除事务标记失败({}):{} —— 标记仍在,下次启动可能再次提示此事务;如反复出现请手动删除该文件。",
            e,
            path.display()
        ));
    }
}

fn global_has_folder(global: &Path, folder_name: &str) -> Result<bool> {
    if !global.is_file() {
        return Ok(false);
    }
    let mut rdr = csv::Reader::from_path(global)?;
    let headers = rdr.headers()?.clone();
    // 同 catalog_has_project:缺『文件夹名』列 = 索引不可信,bail 而非静默 Ok(false),避免把损坏全局索引
    // 当成空索引导致恢复判定把『已索引』误判为『未索引』。(review-r3 round5)
    let Some(col) = headers.iter().position(|h| h == "文件夹名") else {
        anyhow::bail!(
            "全局索引缺少『文件夹名』列,索引不可信(可能损坏或被改格式):{}",
            global.display()
        );
    };
    for rec in rdr.records() {
        let rec = rec?;
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

fn source_bftool_part_files(root: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 || !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !cruft::is_cruft_dir(&name)
        })
    {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                let name = e.file_name().to_string_lossy();
                if name.ends_with(".bftool-part") {
                    let rel = e
                        .path()
                        .strip_prefix(root)
                        .unwrap_or(e.path())
                        .to_string_lossy()
                        .replace('/', "\\");
                    files.push(rel);
                }
            }
            Ok(_) => {}
            Err(e) => errors.push(e.to_string()),
        }
    }
    if !errors.is_empty() {
        anyhow::bail!("{} 个枚举错误:{}", errors.len(), errors.join("; "));
    }
    Ok(files)
}

/// 简单的递归复制；与 robocopy 比缺少 /Z 断点续传中段恢复，但小文件/中等大小够用。
/// 已存在且大小相同的文件直接跳过（保留断点续传的核心语义）。
///
/// 原子写已实现：每个文件先写 `<target>.bftool-part` → fsync → rename 到正式名(见下方循环)。
/// 崩溃只会留下 .part(被当 cruft 忽略、下轮重写)，不会留下「大小对得上的半成品」被续传误跳过。
///
/// EH-010 续传策略 + 兜底：续传时以「目标已存在且**大小相同**」判定该文件已传、直接跳过。
/// 大小相同但内容损坏(如坏扇区、之前被截断后又恰好补到同样字节数)的文件会被这一步跳过，
/// 但**不会漏检**：copy_folder 返回后，handle_one 会对整个目标目录重算 manifest 并与源做
/// SHA256 diff(manifest::diff，非 no_hash 时逐文件比哈希)，内容不一致的文件在那一步被发现、
/// 移入隔离目录，下次运行补传重校。即「大小跳过」是性能优化，「SHA256 diff」是正确性兜底。
fn copy_folder(src: &Path, dst: &Path, reporter: &dyn Reporter) -> Result<()> {
    // 把「按系统杂文件名单静默排除、但其实含真实内容」的源条目变为可见 warn —— 防止用户真实数据
    // 恰好命名为 cruft(如恢复产物目录 found.000、或被命名为 Thumbs.db 的业务文件)在归档『成功』、
    // 源被 MOVE 走之后才发现备份里少了东西。只告警不改过滤(过滤须对称,否则 verify 误报)。(review-r3 round2)
    cruft::warn_excluded_real_content(src, reporter);
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
            // 空目录也要 fail-closed:若 create_dir_all 失败(权限/超长路径/同名文件占用),
            // 旧的 .ok() 会静默吞掉 → 该空目录漏备,而 manifest/diff/verify 都只枚举 is_file()、
            // 永远发现不了缺失的空目录 → 形成「校验通过却漏了目录结构」。收进 errors 让本项目跳过、
            // 下次重做(与下面文件复制失败同一兜底)。非空目录的失败原本也会在子文件复制时上抛,
            // 这里提前归因更清晰。(review-r3 #3)
            if let Err(e) = fs::create_dir_all(&target) {
                errors.push(format!("建目录失败 {}：{}", target.display(), e));
            }
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
        } else {
            // L-01(VulnGym 审计):既非普通目录也非普通文件 = 符号链接/junction/特殊条目。
            // follow_links(false) 有意不跟随(防链接逃逸把链接外内容复制进备份),但**不静默丢弃**——
            // warn 告知用户它没进备份,否则 manifest 同样漏登、verify 还会误报"完好"。
            let rel = path.strip_prefix(src).unwrap_or(path);
            reporter.warn(&format!(
                "跳过链接(未复制):{} —— 不跟随符号链接/junction;其指向的内容若在项目内会作为普通文件单独备份。",
                rel.display()
            ));
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

/// 列出 待备份 下的待归档项目目录(按文件夹名前导数字升序,无数字前缀的排最后)。
///
/// 顶层的符号链接 / junction **不**作为项目归档(本工具不跟随链接,防链接逃逸),
/// 但逐个 `reporter.warn` 告知 —— 否则用户用 junction 把外部目录挂进待备份区时,
/// 整个"项目"会被静默忽略、连"发现 N 个项目"的计数里都看不到。(L-05 VulnGym 审计)
fn discover_projects(ready_root: &Path, reporter: &dyn Reporter) -> Result<Vec<PathBuf>> {
    let mut projects = Vec::new();
    let mut linked = Vec::new();
    for e in fs::read_dir(ready_root)
        .with_context(|| format!("读取待备份目录失败：{}", ready_root.display()))?
    {
        let e = e.with_context(|| format!("枚举待备份目录失败：{}", ready_root.display()))?;
        let ft = e
            .file_type()
            .with_context(|| format!("读取待备份项目类型失败：{}", e.path().display()))?;
        // is_symlink() 在 Windows 上对 junction(mount point)同样为 true,且此时 is_dir()==false。
        if ft.is_symlink() {
            linked.push(e.file_name().to_string_lossy().into_owned());
        } else if ft.is_dir() {
            projects.push(e.path());
        }
    }
    for name in &linked {
        reporter.warn(&format!(
            "待备份下的链接「{}」不会被归档(本工具不跟随符号链接/junction);如需备份请改放实际目录。",
            name
        ));
    }
    // 按文件夹名前导数字升序，无数字前缀的排最后
    projects.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        leading_number(name).unwrap_or(u64::MAX)
    });
    Ok(projects)
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

    // AR-12 测试分工说明:本模块的 handle_one / run_plan 集成测试都跑在 test_archives=false 下,
    // 是**有意**的——它们聚焦核心归档流程(复制/SHA256 校验/源复核/事务提交/移源/断点续传),
    // 不想被外部压缩包测试器(WinRAR/Bandizip/7-Zip 是否安装)的环境依赖污染、影响可重复性。
    // 「archive test 通路」(detect / test_folder / 覆盖率 / 坏包隔离)有 archive_test.rs 自己的
    // 单元测试覆盖(见 engine::archive_test 的 tests 模块);两边职责不重叠。

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

    #[test]
    fn handle_one_rejects_source_file_named_bftool_part() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("cover.jpg"), b"cover").unwrap();
        fs::write(proj.join("session.bftool-part"), b"real user file").unwrap();

        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();

        assert!(matches!(outcome, HandleOutcome::Skipped));
        assert!(proj.exists(), "source must remain for user action");
        assert!(
            !paths::drive_projects_dir(&drive.root)
                .join("001proj")
                .exists(),
            ".bftool-part source files must not be silently omitted"
        );
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
            matches!(outcome, HandleOutcome::Done(_)),
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

    // ── review-r3 #3:空目录建立失败不再 .ok() 静默吞,本项目 fail-closed(返回 Err、不移源)──
    #[test]
    fn copy_folder_dir_create_failure_is_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("s");
        let dst = d.path().join("t");
        // 源含一个空目录 sub_empty(只有它,无文件,故只走目录创建分支)。
        fs::create_dir_all(src.join("sub_empty")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        // 在 dst 下预置一个与该空目录同名的**文件**,使 create_dir_all(dst/sub_empty) 失败。
        fs::write(dst.join("sub_empty"), b"x").unwrap();
        let r = copy_folder(&src, &dst, &NoopReporter);
        assert!(
            r.is_err(),
            "空目录建立失败应 fail-closed(不静默吞),返回 Err"
        );
    }

    // ── L-01(VulnGym 审计):符号链接/junction 不静默丢弃,复制阶段 warn 告知;不跟随(防逃逸) ──
    #[test]
    fn copy_folder_warns_on_link_not_silent() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("s");
        let dst = d.path().join("t");
        let target = d.path().join("target");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(src.join("real.txt"), b"hi").unwrap();
        fs::write(target.join("inner.txt"), b"x").unwrap();
        // 在 src 内建一个指向 target 的链接:Windows 用 junction(免管理员),Unix 用 symlink。
        let link = src.join("jlink");
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&target, &link).is_ok();
        if !made {
            eprintln!("跳过 copy_folder_warns_on_link_not_silent：本环境无法创建链接");
            return;
        }
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        copy_folder(&src, &dst, &rep).unwrap();
        assert!(dst.join("real.txt").is_file(), "普通文件应被复制");
        assert!(
            !dst.join("jlink").join("inner.txt").exists(),
            "不应跟随链接把链接外内容复制进来(防逃逸)"
        );
        let logs = rep.0.lock().unwrap();
        assert!(
            logs.iter()
                .any(|l| l.starts_with("Warn") && l.contains("jlink")),
            "链接应被 warn 告知而非静默跳过,实际日志:{:?}",
            logs
        );
    }

    // ── L-05(VulnGym 审计):待备份下顶层若是链接,不归档但要 warn 告知(否则整项目静默忽略) ──
    #[test]
    fn discover_projects_warns_on_top_level_link_and_excludes_it() {
        let d = tempfile::tempdir().unwrap();
        let ready = d.path().join("ready");
        let ext = d.path().join("external");
        fs::create_dir_all(ready.join("001proj")).unwrap();
        fs::create_dir_all(&ext).unwrap();
        let link = ready.join("002link");
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&ext)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&ext, &link).is_ok();
        if !made {
            eprintln!("跳过 discover_projects_warns_on_top_level_link：本环境无法创建链接");
            return;
        }
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        let projects = discover_projects(&ready, &rep).unwrap();
        assert!(
            projects.iter().any(|p| p.file_name().unwrap() == "001proj"),
            "正常项目应在列表"
        );
        assert!(
            projects.iter().all(|p| p.file_name().unwrap() != "002link"),
            "链接不应作为项目归档,实际:{:?}",
            projects
        );
        let logs = rep.0.lock().unwrap();
        assert!(
            logs.iter()
                .any(|l| l.starts_with("Warn") && l.contains("002link")),
            "顶层链接应 warn 告知,实际:{:?}",
            logs
        );
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

    #[test]
    fn handle_one_malformed_drive_catalog_row_fails_closed() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        let cat = paths::drive_catalog_path(&drive.root);
        if let Some(p) = cat.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(
            &cat,
            "ProjectNo,ProjectName,FileCount,TotalBytes,ArchivedUTC,VerifyStatus,Status,Notes\n\
             001,001proj\n",
        )
        .unwrap();

        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();

        assert!(matches!(outcome, HandleOutcome::Skipped));
        assert!(proj.is_dir(), "malformed catalog row should fail closed");
        assert!(!paths::drive_projects_dir(&drive.root)
            .join("001proj")
            .exists());
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

    // ── review-r2 #2:plan_items 跨项目递减剩余 —— 两个项目各自放得下但累计放不下时,
    // 须在第二个处 SealAndStop,而非因每个单独都够就全标 Archive(冻结 free_bytes 高估 bug) ──
    #[test]
    fn plan_items_seals_on_cumulative_overcommit() {
        let (_d, cfg, mut drive) = temp_world();
        let p1 = cfg.ready_root.join("001a");
        let p2 = cfg.ready_root.join("002b");
        fs::create_dir_all(&p1).unwrap();
        fs::create_dir_all(&p2).unwrap();
        fs::write(p1.join("f"), vec![0u8; 4096]).unwrap();
        fs::write(p2.join("f"), vec![0u8; 4096]).unwrap();
        // reserve_gb=0(temp_world);free 够一个 4096、不够两个 8192。
        drive.free_bytes = 4096 + 2048;
        let items = plan_items(&cfg, &drive, &[p1.clone(), p2.clone()]);
        assert!(
            matches!(items[0].action, PlanAction::Archive { .. }),
            "首个应归档,实际:{:?}",
            items[0].action
        );
        assert!(
            matches!(items[1].action, PlanAction::SealAndStop(_)),
            "次个累计超容量应封盘,实际:{:?}",
            items[1].action
        );
    }

    // ── review-r2 R2-3:清事务标记失败不静默 —— 否则下轮重复触发恢复、提示与实际不符 ──
    #[test]
    fn clear_marker_warns_when_remove_fails() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("marker_is_dir");
        fs::create_dir(&p).unwrap(); // 目录:remove_file 必失败
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        clear_marker(&p, &rep);
        let logs = rep.0.lock().unwrap();
        assert!(
            logs.iter()
                .any(|l| l.starts_with("Warn") && l.contains("清除事务标记失败")),
            "删除失败应 warn 而非静默,实际:{:?}",
            logs
        );
    }

    #[test]
    fn clear_marker_silent_on_success() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("marker");
        fs::write(&p, "x").unwrap();
        let rep = RecordingReporter(std::sync::Mutex::new(Vec::new()));
        clear_marker(&p, &rep);
        assert!(!p.exists(), "成功应已删除标记");
        assert!(rep.0.lock().unwrap().is_empty(), "成功路径不应有日志");
    }

    // ── review-r2 R2-2:Done 携带实际归档字节(供 run_plan 用真实占用累计 consumed)──
    #[test]
    fn handle_one_done_carries_real_bytes() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap(); // 5
        fs::write(proj.join("b.bin"), b"world!!").unwrap(); // 7
        let outcome =
            handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        let n = match outcome {
            HandleOutcome::Done(n) => n,
            _ => panic!("正常项目应 Done"),
        };
        assert_eq!(n, 12, "Done 应携带实际归档字节(供 consumed 真实累计)");
    }

    // ── review-r2 R6-4:已有归档锁(另一实例在跑)时 run_plan 应拒绝、不删别人的锁 ──
    #[test]
    fn run_plan_refuses_when_archive_lock_present() {
        let (_d, cfg, drive) = temp_world();
        fs::create_dir_all(&cfg.system_root).unwrap();
        let lock = cfg.system_root.join(".bftool-archive.lock");
        fs::write(&lock, "pid=999").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let r = run_plan(&cfg, &plan, &cancel, &NoopReporter);
        assert!(r.is_err(), "已有归档锁时应拒绝运行");
        assert!(lock.is_file(), "拒绝时不应删除别人的锁");
    }

    // ── review-r2 R6-2:封盘标记写失败也应终止本轮(DriveSealed),不退化成逐项目 Err 重试 ──
    #[test]
    fn handle_one_seal_failure_still_terminates_round() {
        let (_d, cfg, mut drive) = temp_world();
        drive.free_bytes = 1; // 余量不足 → 触发封盘
                              // 把封盘标记路径占成目录 → drive::seal 的 write_synced 失败
        let sealed = paths::drive_sealed_path(&drive.root);
        fs::create_dir_all(&sealed).unwrap();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("f"), vec![0u8; 4096]).unwrap();
        let outcome = handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None);
        assert!(
            matches!(outcome, Ok(HandleOutcome::DriveSealed)),
            "封盘标记写失败也应 DriveSealed(终止本轮),而非 Err 逐项目重试"
        );
    }

    // ── review-r2 R5-2:提交段索引写失败(标记已写)→ CommitInterrupted,保留标记、中止本轮 ──
    #[test]
    fn handle_one_index_write_failure_is_commit_interrupted() {
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hi").unwrap();
        // 把全局索引路径占成目录 → append_global_catalog 的 write_synced 必失败
        let global = paths::system_global_catalog(&cfg.system_root);
        fs::create_dir_all(&global).unwrap();
        let outcome = handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None);
        assert!(
            matches!(outcome, Ok(HandleOutcome::CommitInterrupted)),
            "提交段索引写失败应 CommitInterrupted(而非普通 Err)"
        );
        assert!(proj.exists(), "源应仍在待备份(提交中断)");
        assert!(
            paths::system_pending_txn(&cfg.system_root).is_file(),
            "事务标记应保留以便恢复"
        );
    }

    // ── review-r2 R2-1:预览→执行间换盘(同盘符)→ 盘内编号变了 → 按"计划已过期"中止 ──
    #[test]
    fn run_plan_aborts_when_drive_id_differs_from_plan() {
        let (_d, cfg, mut drive) = temp_world();
        // 盘上 id 文件是「备份1」(temp_world 写的),让 plan 冻结的 id 是「备份3」(模拟换过盘)。
        drive.id = "备份3".into();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hi").unwrap();
        let plan = ArchivePlan {
            drive,
            items: vec![PlanItem {
                name: "001proj".into(),
                est_bytes: 2,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let summary = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap();
        assert_eq!(summary.handled, 0, "盘内编号不符应中止,不归档任何项目");
        assert!(!cfg.archived_root.join("001proj").exists(), "源不应被移动");
    }

    // ── review-r2 R2-4:catalog append 原子落盘 —— 两次追加都在、表头一次、不遗留 tmp ──
    #[test]
    fn append_drive_catalog_atomic_appends_and_no_tmp() {
        let d = tempfile::tempdir().unwrap();
        let cat = d.path().join("本盘信息").join("本盘索引记录.csv");
        let mk = |no: &str, name: &str| DriveCatalogRow {
            project_no: no.into(),
            project_name: name.into(),
            file_count: 1,
            total_bytes: 10,
            archived_utc: "2026-05-30T00:00:00Z".into(),
            verify_status: "SHA256-OK".into(),
            status: "OK".into(),
            notes: String::new(),
        };
        append_drive_catalog(&cat, &mk("001", "projA")).unwrap();
        append_drive_catalog(&cat, &mk("002", "projB")).unwrap();
        assert!(
            catalog_has_project(&cat, "projA").unwrap(),
            "首行仍在(追加非覆盖)"
        );
        assert!(catalog_has_project(&cat, "projB").unwrap(), "次行也在");
        let content = std::fs::read_to_string(&cat).unwrap();
        assert_eq!(
            content.matches("ProjectName").count(),
            1,
            "表头只一行;实际:\n{content}"
        );
        let tmp = cat.with_file_name("本盘索引记录.csv.bftool-tmp");
        assert!(!tmp.exists(), "原子写不应遗留 .bftool-tmp");
    }

    // ── review-r2 R3-1:良性 Skip(超单盘容量)即使台账写失败也应 Skipped,不升级为 Err/failed ──
    #[test]
    fn handle_one_oversize_skipped_even_if_manual_write_fails() {
        let (_d, mut cfg, mut drive) = temp_world();
        let bogus = cfg.system_root.join("not_a_dir");
        fs::write(&bogus, b"x").unwrap();
        cfg.system_root = bogus; // system_root 指向文件 → 台账写入必失败
        drive.total_bytes = 1; // 任何项目都"超单盘容量"
        let proj = cfg.ready_root.join("001big");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("f"), vec![0u8; 100]).unwrap();
        let outcome = handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None);
        assert!(
            matches!(outcome, Ok(HandleOutcome::Skipped)),
            "超容量良性 Skip 即使台账写失败也应 Skipped、不升级为 Err"
        );
    }

    // ── review-r2 R3-3:移源失败(索引已写)→ 中止本轮,后续项目不得覆盖前项的事务标记 ──
    // Windows-only:靠"持有源内文件句柄使目录无法被 rename"触发移源失败(本工具仅 Windows)。
    #[cfg(windows)]
    #[test]
    fn run_plan_rename_failure_aborts_round_preserving_marker() {
        let (_d, cfg, drive) = temp_world();
        let a = cfg.ready_root.join("001A");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("f"), b"hi").unwrap();
        // B:正常项目(若被处理会写新标记/清标记,从而覆盖 A 的标记)
        let b = cfg.ready_root.join("002B");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("f"), b"yo").unwrap();
        // 持有 A 内文件的打开句柄 → Windows 上 A 目录无法被 rename(移源)→ 移源失败。
        // 共享读句柄不挡 copy_folder/folder_stable 的读,只挡父目录的移动。
        let hold = std::fs::File::open(a.join("f")).unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![
                PlanItem {
                    name: "001A".into(),
                    est_bytes: 2,
                    dest_existed_at_plan: false,
                    action: PlanAction::Archive {
                        dest_name: "001A".into(),
                    },
                },
                PlanItem {
                    name: "002B".into(),
                    est_bytes: 2,
                    dest_existed_at_plan: false,
                    action: PlanAction::Archive {
                        dest_name: "002B".into(),
                    },
                },
            ],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let _ = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap();
        drop(hold);
        assert!(
            paths::system_pending_txn(&cfg.system_root).is_file(),
            "A 的事务标记应保留(本轮中止,B 未处理、未覆盖标记)"
        );
        assert!(
            cfg.ready_root.join("002B").exists(),
            "B 不应被处理(本轮已中止)"
        );
        assert!(
            !paths::drive_projects_dir(&drive.root).join("002B").exists(),
            "B 不应被归档"
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
                dest_existed_at_plan: false,
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

    #[test]
    fn run_plan_resumes_preexisting_unindexed_dest_from_plan() {
        let (_d, cfg, drive) = temp_world();
        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        fs::write(p1.join("b.txt"), b"world").unwrap();

        let dest = paths::drive_projects_dir(&drive.root).join("001proj");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("a.txt"), b"hello").unwrap();

        let item = decide(&cfg, &drive, &p1);
        assert!(
            matches!(&item.action, PlanAction::Archive { dest_name } if dest_name == "001proj"),
            "preexisting unindexed dest should still plan as resumable Archive, got {:?}",
            item.action
        );
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![item],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let s = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap();

        assert_eq!(s.handled, 1, "preexisting dest should be resumed");
        assert_eq!(s.failed, 0);
        assert!(
            !p1.exists(),
            "source should move after resumed archive completes"
        );
        assert_eq!(fs::read(dest.join("b.txt")).unwrap(), b"world");
        assert!(
            paths::system_global_catalog(&cfg.system_root).is_file(),
            "resumed archive should still write the global catalog"
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
                dest_existed_at_plan: false,
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
                dest_existed_at_plan: false,
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

    #[test]
    fn run_plan_checks_paths_before_creating_archived_root() {
        let (_d, mut cfg, drive) = temp_world();
        let unsafe_archived = cfg.ready_root.join("已备份");
        cfg.archived_root = unsafe_archived.clone();
        assert!(!unsafe_archived.exists());
        let plan = ArchivePlan {
            drive,
            items: Vec::new(),
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let err = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap_err();

        assert!(
            format!("{:#}", err).contains("位于"),
            "should fail on unsafe paths, got {err:#}"
        );
        assert!(
            !unsafe_archived.exists(),
            "safety check must run before creating archived_root inside ready_root"
        );
    }

    // ── AR-01 崩溃恢复回归(TEST-AR01):check_pending_txn 三分支 + 重做幂等 ──

    /// 在 cfg.system_root 造一个事务标记。
    fn write_marker(cfg: &Config, src_name: &str, dest_name: &str, src_path: &str, move_to: &str) {
        txn::PendingTxn {
            project_dest_name: dest_name.into(),
            project_src_name: src_name.into(),
            drive_id: "备份1".into(),
            drive_letter: "T".into(),
            in_drive_path: format!("项目\\{dest_name}"),
            src_path: src_path.into(),
            move_to: move_to.into(),
            started_at: "2026-05-29 12:00:00".into(),
        }
        .write(&paths::system_pending_txn(&cfg.system_root))
        .unwrap();
    }

    // ── review-r2 R3-2(撤回后回归守卫):in_ready 重做分支**不得**删除任何索引行
    // (曾尝试在恢复路径清孤儿,引入两个 P1 数据丢失风险,已撤回 → 恢复路径绝不删除)──
    #[test]
    fn check_pending_txn_in_ready_does_not_delete_index() {
        let (_d, cfg, _drive) = temp_world();
        let src = cfg.ready_root.join("001A");
        fs::create_dir_all(&src).unwrap(); // 源仍在待备份 → in_ready=true
        let global = paths::system_global_catalog(&cfg.system_root);
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(
            &global,
            "文件夹名,备份盘名,备份时间,编号,盘内路径,文件数,大小GB,校验方式,校验清单\n\
             001A,备份1,t,001,项目\\001A,1,0.0,SHA256-OK,x\n",
        )
        .unwrap();
        // move_to 指向不存在路径 → moved=false → 走 in_ready 重做分支
        write_marker(
            &cfg,
            "001A",
            "001A",
            &src.display().to_string(),
            "Z:/nope/001A",
        );
        check_pending_txn(&cfg, &NoopReporter, "备份1").unwrap();
        let content = fs::read_to_string(&global).unwrap();
        assert!(
            content.contains("001A"),
            "in_ready 重做不得删除任何索引行(恢复路径绝不删除);实际:\n{content}"
        );
        assert!(
            !paths::system_pending_txn(&cfg.system_root).is_file(),
            "标记应已清除"
        );
    }

    // ── review-r2 R4-1(回归):已完成事务(moved&&indexed)+ 用户重建同名源(in_ready)
    // 必须走"仅标记残留→清标记",绝不删已完成备份的索引行(否则误删/污染旧备份)──
    #[test]
    fn check_pending_txn_completed_tx_with_recreated_source_preserves_index() {
        let (_d, cfg, _drive) = temp_world();
        let moved_dest = cfg.archived_root.join("001A");
        fs::create_dir_all(&moved_dest).unwrap(); // moved=true(源已移到已备份)
        let global = paths::system_global_catalog(&cfg.system_root);
        fs::create_dir_all(global.parent().unwrap()).unwrap();
        fs::write(
            &global,
            "文件夹名,备份盘名,备份时间,编号,盘内路径,文件数,大小GB,校验方式,校验清单\n\
             001A,备份1,t,001,项目\\001A,1,0.0,SHA256-OK,x\n",
        )
        .unwrap(); // indexed=true(已完成备份的记录)
        fs::create_dir_all(cfg.ready_root.join("001A")).unwrap(); // in_ready=true(用户重建同名源)
        write_marker(
            &cfg,
            "001A",
            "001A",
            &cfg.ready_root.join("001A").display().to_string(),
            &moved_dest.display().to_string(),
        );
        check_pending_txn(&cfg, &NoopReporter, "备份1").unwrap();
        let content = fs::read_to_string(&global).unwrap();
        assert!(
            content.contains("001A"),
            "已完成备份的索引行不应被删;实际:\n{content}"
        );
        assert!(
            !paths::system_pending_txn(&cfg.system_root).is_file(),
            "标记应清除"
        );
    }

    #[test]
    fn check_pending_txn_source_in_ready_clears_marker() {
        // 新顺序"写索引→移源"崩在"索引已写、移源未发生"→ 源还在待备份 → 清标记自动重做。
        let (_d, cfg, _drive) = temp_world();
        let src = cfg.ready_root.join("001proj");
        fs::create_dir_all(&src).unwrap();
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &src.display().to_string(),
            &cfg.archived_root.join("001proj").display().to_string(),
        );
        let marker = paths::system_pending_txn(&cfg.system_root);
        assert!(marker.is_file());
        check_pending_txn(&cfg, &NoopReporter, "备份1").unwrap();
        assert!(!marker.exists(), "源仍在待备份 → 清标记自动重做");
    }

    // ── review-r3 #2:in_ready 重做但本轮目标盘 ≠ 标记里的盘(用户换了盘/原盘离线)
    // → fail-closed,保留标记、不重做、不删除,保护原盘上中断副本的恢复证据 ──
    #[test]
    fn check_pending_txn_in_ready_different_drive_keeps_marker() {
        let (_d, cfg, _drive) = temp_world();
        let src = cfg.ready_root.join("001proj");
        fs::create_dir_all(&src).unwrap(); // 源仍在待备份 → in_ready=true
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &src.display().to_string(),
            &cfg.archived_root.join("001proj").display().to_string(),
        ); // 标记里 drive_id = "备份1"
        let marker = paths::system_pending_txn(&cfg.system_root);
        // 本轮目标盘是「备份2」—— 与标记的「备份1」不符 → 应 fail-closed。
        let r = check_pending_txn(&cfg, &NoopReporter, "备份2");
        assert!(
            r.is_err(),
            "重做目标盘与标记盘不符应 fail-closed,保护原盘孤儿副本的恢复证据"
        );
        assert!(marker.is_file(), "fail-closed 必须保留标记(不清、不删)");
        assert!(src.is_dir(), "源不得被动到");
    }

    #[test]
    fn check_pending_txn_moved_and_indexed_clears_marker() {
        // 已移源 + 索引已写 → 完整完成,仅标记残留 → 自动清除。
        let (_d, cfg, _drive) = temp_world();
        let arch = cfg.archived_root.join("001proj");
        fs::create_dir_all(&arch).unwrap();
        fs::write(
            paths::system_global_catalog(&cfg.system_root),
            "文件夹名,备份盘名\n001proj,备份1\n",
        )
        .unwrap();
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &cfg.ready_root.join("001proj").display().to_string(),
            &arch.display().to_string(),
        );
        let marker = paths::system_pending_txn(&cfg.system_root);
        check_pending_txn(&cfg, &NoopReporter, "备份1").unwrap();
        assert!(!marker.exists(), "已移+已索引 → 判定完成,清标记");
    }

    #[test]
    fn check_pending_txn_moved_not_indexed_keeps_marker() {
        // 罕见:已移源但索引漏写 → 保留标记,交人工核对(不自动清)。
        let (_d, cfg, _drive) = temp_world();
        let arch = cfg.archived_root.join("001proj");
        fs::create_dir_all(&arch).unwrap();
        // 不写 global catalog → indexed=false
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &cfg.ready_root.join("001proj").display().to_string(),
            &arch.display().to_string(),
        );
        let marker = paths::system_pending_txn(&cfg.system_root);
        assert!(
            check_pending_txn(&cfg, &NoopReporter, "备份1").is_err(),
            "已移但索引漏写 → 应阻塞新归档,保留标记待人工"
        );
        assert!(marker.exists(), "已移但索引漏写 → 保留标记待人工");
    }

    #[test]
    fn run_plan_stops_when_pending_txn_unresolved() {
        let (_d, cfg, drive) = temp_world();
        let arch = cfg.archived_root.join("001proj");
        fs::create_dir_all(&arch).unwrap();
        write_marker(
            &cfg,
            "001proj",
            "001proj",
            &cfg.ready_root.join("001proj").display().to_string(),
            &arch.display().to_string(),
        );
        let marker = paths::system_pending_txn(&cfg.system_root);
        let marker_before = fs::read_to_string(&marker).unwrap();

        let p2 = cfg.ready_root.join("002proj");
        fs::create_dir_all(&p2).unwrap();
        fs::write(p2.join("b.txt"), b"world").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "002proj".into(),
                est_bytes: 5,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "002proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let err = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap_err();

        assert!(
            format!("{:#}", err).contains("未完成的事务"),
            "should stop on unresolved pending txn, got {err:#}"
        );
        assert_eq!(
            fs::read_to_string(&marker).unwrap(),
            marker_before,
            "new archive must not overwrite unresolved marker"
        );
        assert!(
            p2.is_dir(),
            "new source must not move while old txn unresolved"
        );
        assert!(
            !paths::drive_projects_dir(&drive.root)
                .join("002proj")
                .exists(),
            "new project must not be copied while old txn unresolved"
        );
    }

    #[test]
    fn run_plan_stops_when_pending_txn_parse_fails() {
        let (_d, cfg, drive) = temp_world();
        let marker = paths::system_pending_txn(&cfg.system_root);
        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&marker, "not = valid = toml").unwrap();
        let marker_before = fs::read_to_string(&marker).unwrap();

        let p1 = cfg.ready_root.join("001proj");
        fs::create_dir_all(&p1).unwrap();
        fs::write(p1.join("a.txt"), b"hello").unwrap();
        let plan = ArchivePlan {
            drive: drive.clone(),
            items: vec![PlanItem {
                name: "001proj".into(),
                est_bytes: 5,
                dest_existed_at_plan: false,
                action: PlanAction::Archive {
                    dest_name: "001proj".into(),
                },
            }],
            opts: test_opts(),
        };
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let err = run_plan(&cfg, &plan, &cancel, &NoopReporter).unwrap_err();

        assert!(
            format!("{:#}", err).contains("事务标记"),
            "should stop on unreadable pending txn, got {err:#}"
        );
        assert_eq!(fs::read_to_string(&marker).unwrap(), marker_before);
        assert!(p1.is_dir(), "source must not move after parse failure");
        assert!(!paths::drive_projects_dir(&drive.root)
            .join("001proj")
            .exists());
    }

    #[test]
    fn crash_after_index_before_move_redo_is_idempotent() {
        // AR-01 核心保证:新顺序"写索引→移源",若崩在"索引已写、源未移",源还在待备份;
        // 重做时本盘索引已有该项 → 改时间戳唯一名归档,绝不覆盖已写副本,源不丢失。
        let (_d, cfg, drive) = temp_world();
        let proj = cfg.ready_root.join("001proj");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        // 首次归档成功(源移走,本盘索引写入 001proj)。
        handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        assert!(!proj.exists(), "首次归档后源已移走");
        // 复现崩溃残留:源又出现在待备份(= 索引已写但移源被中断,源还在)。
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        // 重做:本盘索引已有 001proj → 走重名 → 改时间戳唯一名。
        let out = handle_one(&cfg, &NoopReporter, &drive, &proj, &test_opts(), None, None).unwrap();
        assert!(
            matches!(out, HandleOutcome::Done(_)),
            "重做应成功(改名归档)"
        );
        assert!(!proj.exists(), "重做后源被移走,不丢失");
        let proj_dir = paths::drive_projects_dir(&drive.root);
        assert!(proj_dir.join("001proj").is_dir(), "原副本未被覆盖");
        let renamed = fs::read_dir(&proj_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.starts_with("001proj_") && n != "001proj"
            });
        assert!(renamed, "重做应以时间戳唯一名归档,不覆盖原 001proj");
    }
}

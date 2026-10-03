//! 备份盘检测、初始化、序号管理。
//!
//! 「认盘」靠盘内 `本盘信息\本盘编号.txt`，不依赖盘符也不依赖卷标。
//! 一旦盘里有这个文件 + 未封盘，就是一块可写入的备份盘。

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::engine::{cruft, destination::SafeDir, durable, paths};
use crate::observe::{EventSink, ReporterSink};
use crate::reporter::Reporter;
use crate::service::request::{InitRequest, OperationRequest};

#[derive(Debug, Clone, Serialize)]
pub struct DriveInfo {
    pub letter: String, // "E"
    pub root: PathBuf,  // "E:\"
    pub id: String,     // "备份3"
    pub sealed: bool,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// 已初始化的备份盘(通用类型,含封盘态)。scan / 列盘 / 复查 / 状态都用它。(Spec D §4.2 / L-021)
pub type BackupDrive = DriveInfo;

/// 可写入的备份盘:不变量 = 未封盘且容量达标。archive 写路径只接受它,编译期防"写错盘/封盘盘"。
#[derive(Debug, Clone)]
pub(crate) struct WritableDrive(BackupDrive);

/// `try_into_writable` 失败原因(带"怎么修")。
#[derive(Debug)]
pub enum DriveError {
    Sealed,
    TooSmall { total_gb: u64, min_gb: u64 },
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveError::Sealed => {
                write!(f, "该盘已封盘,禁止写入。请换一块未封盘的备份盘,或 init 新盘。")
            }
            DriveError::TooSmall { total_gb, min_gb } => write!(
                f,
                "该盘仅 {total_gb}GB,低于最小 {min_gb}GB(防误抓 U 盘)。如确需用它,调低配置 min_drive_gb。"
            ),
        }
    }
}
impl std::error::Error for DriveError {}

impl DriveInfo {
    /// 升级为可写盘:仅**未封盘且容量 ≥ min_drive_gb**时成功;阈值显式传入(来自 `cfg.min_drive_gb`)。
    pub(crate) fn try_into_writable(self, min_drive_gb: u64) -> Result<WritableDrive, DriveError> {
        if self.sealed {
            return Err(DriveError::Sealed);
        }
        let min = min_drive_gb.saturating_mul(1024 * 1024 * 1024);
        if self.total_bytes < min {
            return Err(DriveError::TooSmall {
                total_gb: self.total_bytes / (1024 * 1024 * 1024),
                min_gb: min_drive_gb,
            });
        }
        Ok(WritableDrive(self))
    }
}

impl WritableDrive {
    // 强优化:删除仅测试引用的死方法 inner();生产路径只用 into_inner()。收敛 API 表面。
    pub(crate) fn into_inner(self) -> BackupDrive {
        self.0
    }
}

pub fn list_mounted(cfg: &Config, reporter: &dyn Reporter) -> Result<()> {
    let drives = scan_mounted(Some(reporter))?;
    if drives.is_empty() {
        reporter.warn("未发现已初始化的备份盘。");
        reporter
            .info("插入备份盘或目标盘后，运行：bftool init <盘符> 把它初始化为下一个「备份N」。");
        return Ok(());
    }
    for d in &drives {
        let tag = if d.sealed {
            "[已封盘]"
        } else {
            "[可用]  "
        };
        // 每行是结构化"盘项"，借 Info 级别打印；GUI 会换成自己的 list view。
        reporter.info(&format!(
            "  {}  {} ({}:)  剩余 {:.1} GB / 共 {:.0} GB",
            tag,
            d.id,
            d.letter,
            d.free_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
            d.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
        ));
    }
    let _ = cfg; // 当前未用 cfg；保留参数便于将来加 system_root 联动展示
    Ok(())
}

/// 探测某盘是否已封盘。`Path::is_file()` 在 stat 失败(权限拒绝/瞬时锁/FS 错误)时返回 false,
/// 会把「封盘标记其实存在但此刻 stat 不到」误判为未封盘 → 该盘可能被 pick_active 重新选为可写盘、
/// 误写已封盘的冷备盘。改为区分 NotFound(确认无标记=未封盘)与其它错误(封盘态未知 → fail-closed
/// 当作已封盘),与本文件 read_to_string 失败的处理一致,绝不误写已封盘盘。(review-r3 round4)
pub(crate) fn drive_is_sealed(root: &Path) -> bool {
    match fs::symlink_metadata(paths::drive_sealed_path(root)) {
        Ok(_) => true,                                               // 标记存在 → 已封盘
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false, // 确认无标记 → 未封盘
        Err(_) => true, // 读不出(权限/锁/FS 错误)→ 封盘态未知,保守当已封盘,绝不误写
    }
}

/// 扫描已挂载备份盘（实现见 [`crate::pool::scan`]）。
pub fn scan_mounted(reporter: Option<&dyn Reporter>) -> Result<Vec<DriveInfo>> {
    crate::pool::scan::scan_mounted(reporter)
}

pub(crate) fn usable_drives_now(min_drive_gb: u64) -> Result<Vec<DriveInfo>> {
    crate::pool::scan::usable_drives_now(min_drive_gb)
}

pub(crate) use crate::pool::scan::drive_letter_of;

/// 初始化一块盘为下一个「备份N」（或自定义 ID）
pub fn init(
    cfg: &Config,
    reporter: &dyn Reporter,
    drive_letter: &str,
    id: Option<&str>,
    force: bool,
) -> Result<()> {
    let req =
        InitRequest::from_legacy_force(drive_letter.to_string(), id.map(str::to_string), force);
    let sink = ReporterSink { reporter };
    init_with_request(cfg, reporter, &req, &sink)
}

pub(crate) fn init_with_request(
    cfg: &Config,
    reporter: &dyn Reporter,
    req: &InitRequest,
    sink: &dyn EventSink,
) -> Result<()> {
    let letter = req.drive_letter.trim_end_matches(':').to_uppercase();
    if letter.len() != 1 {
        bail!("盘符无效：{}（应为单字母，例如 E）", req.drive_letter);
    }
    let root = PathBuf::from(format!("{}:\\", letter));
    if !root.exists() {
        bail!("驱动器 {}: 不存在或未挂载", letter);
    }

    let req = InitRequest {
        drive_letter: letter,
        ..req.clone()
    };
    init_at_root(cfg, reporter, &root, &req, sink)
}

fn init_at_root(
    cfg: &Config,
    reporter: &dyn Reporter,
    root: &Path,
    req: &InitRequest,
    sink: &dyn EventSink,
) -> Result<()> {
    let letter = &req.drive_letter;
    // 防呆：系统盘 / 资料库盘硬闸；非空仅为软提示（DESIGN §4）。判定收口到 classify_for_init。
    let c = classify_for_init(cfg, letter, root)?;
    apply_init_gates(&c, req, reporter, sink)?;
    // An abnormal marker is user data until proved otherwise. Stop before metadata
    // writes; --force never authorizes recursively deleting a conflicting directory.
    validate_sealed_marker(root)?;
    let root_guard = SafeDir::open(root, false)?;

    // 并发 init 两块新盘会因 next_drive_number(读 seq/全局索引/挂载盘)+ bump_drive_seq(写回)非原子
    // 而分到相同「备份N」→ 盘身份碰撞、find/恢复歧义。用与 archive 同一把系统级跨进程锁,把
    // 「定号 → 写本盘编号 → bump seq」串成临界区,消除分号碰撞;与正在跑的归档也互斥。(review-r3 round4)
    let _system_guard = SafeDir::open(&cfg.system_root, true)?;
    let _sys_lock = crate::engine::archive::ArchiveLock::acquire(&cfg.system_root)?;

    // Retain the existing explicit-ID renumber policy. This aggregate is only
    // for that metadata policy; hard-gate bypasses above use their own scopes.
    let force = req.force_system || req.force_library;
    let id = resolve_drive_id(root, req.id.as_deref(), force, cfg, reporter)?;

    // 写盘内目录
    let info = paths::drive_info_dir(root);
    let info_guard = root_guard.ensure_dir(Path::new(paths::DRIVE_INFO_DIR))?;
    info_guard.ensure_dir(Path::new(paths::DRIVE_LOGS_DIR))?;
    info_guard.ensure_dir(Path::new(paths::DRIVE_MANIFEST_DIR))?;
    root_guard.ensure_dir(Path::new(paths::DRIVE_PROJECTS_DIR))?;
    // 原子写(tmp+fsync+rename):本盘编号是认盘依据,断电不能留 0 字节/半截坏文件。(review-r2 R4-4)
    durable::write_synced(&paths::drive_id_path(root), id.as_bytes()).context("写本盘编号失败")?;
    durable::write_synced(&info.join(paths::DRIVE_README_FILE), readme(&id).as_bytes())?;

    // 序号文件追踪：保证下次取下一块的时候编号单调递增
    if let Some(n) = parse_drive_number(&cfg.name_prefix, &id) {
        bump_drive_seq(cfg, n, reporter)?;
    }

    // Re-init removes only a recognized ordinary marker. A directory, link,
    // special entry or inspection error is preserved and reported explicitly.
    clear_sealed_marker(root, &info_guard, letter, reporter)?;

    reporter.ok(&format!("已初始化备份盘 {} ({}:)", id, letter));
    if !force && paths::drive_id_path(root).is_file() {
        // 提示用户可以接着 archive
        reporter.info(
            "现在可以运行 `bftool archive` 开始归档；先 `bftool archive --dry-run` 演练一下更稳。",
        );
    }
    Ok(())
}

fn marker_is_ordinary_file(meta: &fs::Metadata) -> bool {
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Every reparse point is abnormal, not only symlink-tagged reparse points.
        if meta.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    true
}

fn validate_sealed_marker(root: &Path) -> Result<bool> {
    let marker = paths::drive_sealed_path(root);
    match fs::symlink_metadata(&marker) {
        Ok(meta) if marker_is_ordinary_file(&meta) => Ok(true),
        Ok(_) => bail!(
            "封盘标记路径不是普通文件，已保留原内容并停止初始化；请人工检查：{}",
            marker.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("无法安全检查封盘标记，已停止初始化：{}", marker.display())),
    }
}

fn clear_sealed_marker(
    root: &Path,
    info_guard: &SafeDir,
    letter: &str,
    reporter: &dyn Reporter,
) -> Result<()> {
    if validate_sealed_marker(root)? {
        info_guard
            .remove_regular(Path::new(paths::DRIVE_SEALED_FILE))
            .with_context(|| {
                format!(
                    "安全清除封盘标记失败：{}",
                    paths::drive_sealed_path(root).display()
                )
            })?;
        reporter.info(&format!(
            "已清除 {}: 的普通封盘标记(重新初始化 = 当作可写新盘)。",
            letter
        ));
    }
    Ok(())
}

/// init 防呆分类(系统盘/资料库盘/已是备份盘/非空)。init() 与 init_candidates 共用。(Spec D §4.2 Finding #4)
struct InitClass {
    is_system: bool,
    is_library: bool,
    already_backup: bool,
    non_empty: bool,
}

fn classify_for_init(cfg: &Config, letter: &str, root: &Path) -> Result<InitClass> {
    let is_system = system_drive_letter().eq_ignore_ascii_case(letter);
    let is_library = [&cfg.ready_root, &cfg.archived_root, &cfg.system_root]
        .iter()
        .filter_map(|p| qualifier_letter(p))
        .any(|l| l.eq_ignore_ascii_case(letter));
    let already_backup = paths::drive_id_path(root).is_file();
    let non_empty = if already_backup {
        false
    } else {
        !root_is_empty(root)?
    };
    Ok(InitClass {
        is_system,
        is_library,
        already_backup,
        non_empty,
    })
}

/// 决定本盘编号。R5-3:已是备份盘(盘上有 本盘编号.txt)时 **不指定 --id 必须复用现有编号、
/// 绝不分配新号** —— 否则重初始化会静默改掉盘身份,让盘上已索引数据与新号脱节、find/恢复指向错误。
/// 显式 --id 改成与现有不同的号(非 --force)直接拒绝;--force 才允许强改。
fn resolve_drive_id(
    root: &Path,
    id: Option<&str>,
    force: bool,
    cfg: &Config,
    reporter: &dyn Reporter,
) -> Result<String> {
    let existing_id = fs::read_to_string(paths::drive_id_path(root))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match id {
        Some(s) if !s.trim().is_empty() => {
            let s = s.trim();
            if let Some(ex) = &existing_id {
                if ex != s && !force {
                    bail!(
                        "盘已是备份盘「{}」,拒绝改号为「{}」—— 会让盘上已归档数据与新编号脱节、\
                         find/恢复指向错误。如确需改号,请先清空该盘再初始化,或加 --force 强改。",
                        ex,
                        s
                    );
                }
            }
            Ok(s.to_string())
        }
        // 未指定 --id:已是备份盘 → 复用现有编号;全新盘 → 取下一个「备份N」。
        _ => match existing_id {
            Some(ex) => Ok(ex),
            None => Ok(format!(
                "{}{}",
                cfg.name_prefix,
                next_drive_number(cfg, reporter)?
            )),
        },
    }
}

/// 根目录是否为空(忽略系统目录与 OS 自动注入的杂文件)。
/// 杂文件名单统一交给 cruft 判定(desktop.ini/Thumbs.db/$RECYCLE.BIN/System Volume Information…),
/// 否则一块全新盘只要有个 desktop.ini/Thumbs.db 就被误判非空（软提示噪声）。(review-r2 R4-3)
pub(crate) fn root_is_empty(root: &Path) -> Result<bool> {
    // RECYCLER 是旧版 Windows 回收站目录名,cruft 名单未含,这里额外忽略。
    let extra_ignore: &[&str] = &["RECYCLER"];
    let mut n = 0usize;
    for e in fs::read_dir(root).with_context(|| format!("读取 {} 根目录失败", root.display()))?
    {
        let e = e.with_context(|| format!("枚举 {} 根目录失败", root.display()))?;
        let name = e.file_name();
        let s = name.to_string_lossy();
        let ignorable = extra_ignore.iter().any(|x| x.eq_ignore_ascii_case(&s))
            || cruft::is_cruft_dir(&s)
            || cruft::is_cruft_file(&s);
        if !ignorable {
            n += 1;
        }
    }
    Ok(n == 0)
}

/// 由分类得出硬闸 / 软提示决策。非空永不阻断（DESIGN §4）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct InitDecision {
    can_init: bool,
    block_reason: Option<String>,
    soft_hint: Option<String>,
}

fn init_decision(c: &InitClass) -> InitDecision {
    if c.is_system {
        return InitDecision {
            can_init: false,
            block_reason: Some("系统盘".into()),
            soft_hint: None,
        };
    }
    if c.is_library {
        return InitDecision {
            can_init: false,
            block_reason: Some("资料库所在盘(待备份/已备份/备份系统)".into()),
            soft_hint: None,
        };
    }
    let soft_hint = if !c.already_backup && c.non_empty {
        Some("根目录非空，请确认目标盘无误（不是系统盘/资料库盘）；已有数据不会被清洗，init 只写本盘约定目录".into())
    } else {
        None
    };
    InitDecision {
        can_init: true, // 含已是备份盘(re-init)与非空(软提示)
        block_reason: None,
        soft_hint,
    }
}

/// 应用 init 硬闸与软提示（P1：委托 pipeline stages）。
/// 软提示只 warn，绝不 bail；`--force` 仅绕过系统盘/资料库硬闸。
fn apply_init_gates(
    c: &InitClass,
    req: &InitRequest,
    reporter: &dyn Reporter,
    sink: &dyn EventSink,
) -> Result<()> {
    let op = OperationRequest::Init(req.clone());
    crate::pipeline::init::run_init_gates(
        crate::pipeline::InitStageData {
            letter: req.drive_letter.clone(),
            is_system: c.is_system,
            is_library: c.is_library,
            already_backup: c.already_backup,
            non_empty: c.non_empty,
            force_system: req.force_system,
            force_library: req.force_library,
        },
        &op,
        reporter,
        sink,
    )
}

/// GUI 初始化页用:列出所有挂载盘 + 结构化防呆判定。(Spec D §4.2 / DESIGN §4)
#[derive(Debug, Clone)]
pub struct InitCandidate {
    pub letter: String,
    pub total_gb: u64,
    pub is_system: bool,
    pub is_library: bool,
    pub already_backup: bool,
    pub non_empty: bool,
    pub can_init: bool,
    pub block_reason: Option<String>,
    /// 非阻断提示（如根目录非空）；GUI 显示黄条，仍可选中。
    pub soft_hint: Option<String>,
}

pub fn init_candidates(cfg: &Config) -> Result<Vec<InitCandidate>> {
    let mut out = Vec::new();
    let disks = sysinfo::Disks::new_with_refreshed_list();
    for d in disks.list() {
        let Some(letter) = drive_letter_of(d.mount_point()) else {
            continue;
        };
        let root = PathBuf::from(format!("{}:\\", letter));
        let Ok(cls) = classify_for_init(cfg, &letter, &root) else {
            continue; // 读不了根目录的盘跳过(不进候选)
        };
        let dec = init_decision(&cls);
        out.push(InitCandidate {
            letter,
            total_gb: d.total_space() / (1024 * 1024 * 1024),
            is_system: cls.is_system,
            is_library: cls.is_library,
            already_backup: cls.already_backup,
            non_empty: cls.non_empty,
            can_init: dec.can_init,
            block_reason: dec.block_reason,
            soft_hint: dec.soft_hint,
        });
    }
    Ok(out)
}

fn readme(id: &str) -> String {
    format!(
        "本盘编号 : {id}\n用途     : 项目文件夹备份（以文件夹为最小单位；增量复制，非镜像，绝不删除）\n目录结构 :\n  \\项目\\<项目名>\\          原样存放的项目，可直接复制恢复\n  \\本盘信息\\本盘编号.txt         本盘编号（程序识别用，请勿改动）\n  \\本盘信息\\本盘说明.txt          本说明文件\n  \\本盘信息\\本盘索引记录.csv    本盘项目清单\n  \\本盘信息\\校验清单\\*.sha256.csv  每个项目的逐文件 SHA256 校验清单\n  \\本盘信息\\日志\\                备份日志\n  \\本盘信息\\已封盘.txt            存在即表示本盘已写满/停用，请勿再写入\n校验方式 : 文件数 + 总字节数 + 每个文件的 SHA256\n恢复方法 : 直接把 \\项目\\<项目名> 复制回去即可；如需核对，用 校验清单 重算 SHA256 比对\n复查建议 : 每 6～12 个月通电一次，重算哈希与 校验清单 比对，检查有无坏道\n初始化时间: {}\n",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    )
}

fn parse_drive_number(prefix: &str, id: &str) -> Option<u32> {
    id.strip_prefix(prefix).and_then(|s| s.parse::<u32>().ok())
}

/// 下一个编号 = 所有已知「备份N」里的最大值 + 1(空 → 1)。纯函数,保证单调不碰撞。(ledger L-027)
fn next_number(found: &[u32]) -> u32 {
    found.iter().copied().max().unwrap_or(0) + 1
}

/// 计算下一个可用「备份N」编号 = max(序号文件, 全局索引里出现过的「备份N」, 当前挂载盘里的「备份N」) + 1
fn next_drive_number(cfg: &Config, reporter: &dyn Reporter) -> Result<u32> {
    let mut found: Vec<u32> = Vec::new();
    let seq_file = paths::system_drive_seq(&cfg.system_root);
    // 盘号计数文件(seq)是「编号单调不碰撞」的唯一持久兜底(一块已 init 但从未 archive、且此刻
    // 离线的盘,只有它还记得编号)。区分两种读失败:文件不存在 = 全新系统首盘,正常静默;但
    // 「存在却内容损坏/读不出」必须可见,否则在唯一兜底失效时仍乐观分配可能重号 ——
    // 与下方全局索引读损坏即 bail 的处理对齐。(review-r3 round2)
    match fs::read_to_string(&seq_file) {
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(n) => found.push(n),
            Err(_) => reporter.warn(&format!(
                "盘号计数文件内容损坏(非数字「{}」),无法据此保证「备份N」编号单调不碰撞,\
                 请人工核对当前最大编号后修复:{}",
                text.trim(),
                seq_file.display()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // 全新系统首盘:正常
        Err(e) => reporter.warn(&format!(
            "读取盘号计数文件失败({}),无法据此保证「备份N」编号单调不碰撞:{}",
            e,
            seq_file.display()
        )),
    }
    let gc = paths::system_global_catalog(&cfg.system_root);
    if gc.is_file() {
        let mut rdr = csv::Reader::from_path(&gc)
            .with_context(|| format!("读取全局索引失败：{}", gc.display()))?;
        // 用动态 row：先读 headers 找「备份盘名」列
        let headers = rdr
            .headers()
            .with_context(|| format!("读取全局索引表头失败：{}", gc.display()))?
            .clone();
        if let Some(col) = headers.iter().position(|h| h == "备份盘名") {
            for (idx, rec) in rdr.records().enumerate() {
                let rec = rec.with_context(|| {
                    format!("全局索引第 {} 行格式错误：{}", idx + 2, gc.display())
                })?;
                if let Some(v) = rec.get(col) {
                    if let Some(n) = parse_drive_number(&cfg.name_prefix, v.trim()) {
                        found.push(n);
                    }
                }
            }
        }
    }
    // 编号分配路径不得在「看不全所有盘」的前提下静默工作:传 Some(reporter),让有 本盘编号.txt
    // 却读不出的在线盘至少发 warn(配合 seq 兜底,降低重号风险)。(review-r3 round2)
    for d in scan_mounted(Some(reporter))? {
        if let Some(n) = parse_drive_number(&cfg.name_prefix, &d.id) {
            found.push(n);
        }
    }
    Ok(next_number(&found))
}

fn bump_drive_seq(cfg: &Config, n: u32, reporter: &dyn Reporter) -> Result<()> {
    let seq_file = paths::system_drive_seq(&cfg.system_root);
    if let Some(parent) = seq_file.parent() {
        fs::create_dir_all(parent).ok();
    }
    // 盘号计数器单调不回退。区分读失败种类:NotFound = 全新系统首盘,正常写入 n;但「文件存在却读不出/
    // 内容损坏」绝不能当 cur=0 —— 那会让 n>0 成立、把磁盘上更大的旧值覆写成较小的 n,造成计数器回退、
    // 后续给新盘自动选号时与离线旧盘重号。读不出时保守跳过本次 bump(保留磁盘上更大的旧值)并 warn,
    // 与 next_drive_number 的 seq 读取处理对齐。(review-r3 round3)
    match fs::read_to_string(&seq_file) {
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(cur) => {
                if n > cur {
                    durable::write_synced(&seq_file, n.to_string().as_bytes())?;
                    // 原子写(review-r2 R4-4)
                }
            }
            Err(_) => reporter.warn(&format!(
                "盘号计数文件内容损坏(非数字「{}」),跳过本次更新以免把计数器写小导致后续重号;\
                 请人工核对当前最大编号后修复:{}",
                text.trim(),
                seq_file.display()
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            durable::write_synced(&seq_file, n.to_string().as_bytes())?; // 全新系统首盘:正常写入
        }
        Err(e) => reporter.warn(&format!(
            "读取盘号计数文件失败({}),跳过本次更新以免把计数器写小导致后续重号:{}",
            e,
            seq_file.display()
        )),
    }
    Ok(())
}

/// 写封盘标记。当前盘剩余不足下一个项目时调用。
pub(crate) fn seal(drive: &DriveInfo) -> Result<()> {
    let cat = paths::drive_catalog_path(&drive.root);
    let (cnt, bytes) = if cat.is_file() {
        let mut total_bytes = 0u64;
        let mut total = 0u32;
        let mut rdr = csv::Reader::from_path(&cat)
            .with_context(|| format!("读取本盘索引失败：{}", cat.display()))?;
        let headers = rdr
            .headers()
            .with_context(|| format!("读取本盘索引表头失败：{}", cat.display()))?
            .clone();
        let bcol = headers.iter().position(|h| h == "TotalBytes");
        for (idx, r) in rdr.records().enumerate() {
            let r =
                r.with_context(|| format!("本盘索引第 {} 行格式错误：{}", idx + 2, cat.display()))?;
            total += 1;
            if let Some(c) = bcol {
                if let Some(s) = r.get(c) {
                    if let Ok(n) = s.parse::<u64>() {
                        total_bytes += n
                    }
                }
            }
        }
        (total, total_bytes)
    } else {
        (0, 0)
    };
    let text = format!(
        "封盘编号   : {}\n封盘时间   : {}\n项目数量   : {}\n归档总字节 : {}\n说明       : 本盘已写满/停用，请勿继续写入；可作为冷备离线保存。\n",
        drive.id,
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        cnt, bytes
    );
    // 原子写 + fsync:封盘标记若断电丢失,本盘会被再次选为可写盘 → 误写已封盘的盘。(review-r2 R4-5)
    durable::write_synced(&paths::drive_sealed_path(&drive.root), text.as_bytes())
        .with_context(|| format!("写封盘标记失败：{}", drive.root.display()))?;
    Ok(())
}

fn system_drive_letter() -> String {
    std::env::var("SystemDrive")
        .unwrap_or_else(|_| "C:".into())
        .trim_end_matches(':')
        .to_uppercase()
}

fn qualifier_letter(p: &Path) -> Option<String> {
    // 与 drive_letter_of 同义,直接复用避免重复的盘符解析。(ledger L-023 / TD-06)
    drive_letter_of(p)
}

/// 把盘符字符串规范化为根路径 PathBuf。
fn root_from_letter(letter: &str) -> Result<PathBuf> {
    let l = letter.trim_end_matches(':').to_uppercase();
    if l.len() != 1 {
        bail!("盘符无效：{}（应为单字母）", letter);
    }
    Ok(PathBuf::from(format!("{}:\\", l)))
}

/// 根据盘符直接读 DriveInfo（不要求事先 scan_mounted）。
pub(crate) fn info_by_letter(letter: &str) -> Result<DriveInfo> {
    let letter = letter.trim_end_matches(':').to_uppercase();
    let root = root_from_letter(&letter)?;
    let id_file = paths::drive_id_path(&root);
    let id = fs::read_to_string(&id_file)
        .with_context(|| {
            format!(
                "{}: 不是一块已初始化的备份盘（找不到 {}）",
                letter,
                id_file.display()
            )
        })?
        .trim()
        .to_string();
    // 空/全空白编号 = 损坏或未初始化:与 scan_mounted/resolve_drive_id 的判定一致,拒绝当作
    // 合法备份盘,否则空 id 会被 archive 写进本盘/全局索引污染盘身份、find/恢复无法定位。(review-r3 #9)
    if id.is_empty() {
        bail!(
            "{}: 本盘编号文件为空,疑似损坏,请重新 init 该盘（{}）",
            letter,
            id_file.display()
        );
    }
    // 容量信息来自 sysinfo。若该盘符未被枚举到(例如卷以 GUID 路径挂载、或枚举时机差异),
    // 旧实现 free/total 停留在 0 → 下游把任何非空项目误判「超过单盘容量」或整盘 TooSmall,
    // 静默拒绝一块其实可写的盘。改为 fail-closed:容量读不到就明确报错,而非伪装成 0 容量盘。(review-r3 #10)
    let mut found = false;
    let mut free = 0;
    let mut total = 0;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    for d in disks.list() {
        if let Some(l) = drive_letter_of(d.mount_point()) {
            if l == letter {
                free = d.available_space();
                total = d.total_space();
                found = true;
                break;
            }
        }
    }
    if !found {
        bail!(
            "{}: 无法读取该盘容量信息(未被系统磁盘列表枚举到)—— 无法安全判定是否够放。\
             请确认盘符正确、盘在线;若该盘以卷 GUID 方式挂载,请改用普通盘符 X:\\。",
            letter
        );
    }
    Ok(DriveInfo {
        letter,
        root: root.clone(),
        id,
        sealed: drive_is_sealed(&root),
        free_bytes: free,
        total_bytes: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporter::NoopReporter;

    fn apply_legacy_init_gates(
        c: &InitClass,
        letter: &str,
        force: bool,
        reporter: &dyn Reporter,
    ) -> Result<()> {
        let req = InitRequest::from_legacy_force(letter.into(), None, force);
        let sink = ReporterSink { reporter };
        apply_init_gates(c, &req, reporter, &sink)
    }

    fn init_legacy_at_root(
        cfg: &Config,
        reporter: &dyn Reporter,
        letter: &str,
        root: &Path,
        id: Option<&str>,
        force: bool,
    ) -> Result<()> {
        let req = InitRequest::from_legacy_force(letter.into(), id.map(str::to_string), force);
        let sink = ReporterSink { reporter };
        init_at_root(cfg, reporter, root, &req, &sink)
    }

    #[test]
    fn typed_init_force_scopes_truth_table() {
        let reporter = NoopReporter;
        let sink = ReporterSink {
            reporter: &reporter,
        };
        for is_system in [false, true] {
            for is_library in [false, true] {
                for force_system in [false, true] {
                    for force_library in [false, true] {
                        let c = InitClass {
                            is_system,
                            is_library,
                            already_backup: false,
                            non_empty: false,
                        };
                        let req = InitRequest {
                            drive_letter: "T".into(),
                            id: None,
                            force_system,
                            force_library,
                        };
                        let allowed =
                            (!is_system || force_system) && (!is_library || force_library);
                        assert_eq!(
                            apply_init_gates(&c, &req, &reporter, &sink).is_ok(),
                            allowed,
                            "system={is_system} library={is_library} force_system={force_system} force_library={force_library}",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn typed_init_only_bypasses_the_authorized_gate_before_writes() {
        let reporter = NoopReporter;
        let sink = ReporterSink {
            reporter: &reporter,
        };
        let system_letter = system_drive_letter();
        let library_letter = if system_letter.eq_ignore_ascii_case("T") {
            "U"
        } else {
            "T"
        };
        for is_system in [false, true] {
            for is_library in [false, true] {
                for force_system in [false, true] {
                    for force_library in [false, true] {
                        let world = tempfile::tempdir().unwrap();
                        let root = world.path().join("synthetic-drive");
                        fs::create_dir(&root).unwrap();
                        fs::write(root.join("user-document.txt"), b"KEEP-ME").unwrap();
                        let letter = if is_system {
                            &system_letter
                        } else {
                            library_letter
                        };
                        let cfg = Config {
                            ready_root: if is_library {
                                PathBuf::from(format!("{letter}:\\synthetic-ready"))
                            } else {
                                world.path().join("ready")
                            },
                            archived_root: world.path().join("archived"),
                            system_root: world.path().join("system"),
                            ..Config::default()
                        };
                        // On Windows, temporary library roots can share the system drive.
                        // Both scopes must be authorized when the target matches both.
                        let temporary_library =
                            qualifier_letter(world.path()).is_some_and(|temporary_letter| {
                                temporary_letter.eq_ignore_ascii_case(letter)
                            });
                        let classified_as_library = is_library || temporary_library;
                        let classification = classify_for_init(&cfg, letter, &root).unwrap();
                        assert_eq!(classification.is_system, is_system);
                        assert_eq!(classification.is_library, classified_as_library);
                        let req = InitRequest {
                            drive_letter: letter.into(),
                            id: Some("备份99".into()),
                            force_system,
                            force_library,
                        };
                        let authorized = (!is_system || force_system)
                            && (!classified_as_library || force_library);
                        let result = init_at_root(&cfg, &reporter, &root, &req, &sink);
                        assert_eq!(
                            result.is_ok(),
                            authorized,
                            "system={is_system} library={classified_as_library} force_system={force_system} force_library={force_library}: {result:?}",
                        );
                        if authorized {
                            result.unwrap();
                            assert_eq!(
                                fs::read_to_string(paths::drive_id_path(&root)).unwrap(),
                                "备份99"
                            );
                        } else {
                            let error = result.unwrap_err().to_string();
                            assert!(
                                error.contains(if is_system && !force_system {
                                    "系统盘"
                                } else {
                                    "资料库"
                                }),
                                "{error}"
                            );
                            assert!(
                                !cfg.system_root.exists(),
                                "denial must precede system metadata writes"
                            );
                            assert!(
                                !paths::drive_info_dir(&root).exists(),
                                "denial must precede drive metadata writes"
                            );
                        }
                        assert_eq!(
                            fs::read(root.join("user-document.txt")).unwrap(),
                            b"KEEP-ME"
                        );
                    }
                }
            }
        }
    }

    fn di(letter: &str, total_gb: u64, sealed: bool) -> DriveInfo {
        let bytes = total_gb * 1024 * 1024 * 1024;
        DriveInfo {
            letter: letter.into(),
            root: PathBuf::from(format!("{}:\\", letter)),
            id: format!("备份{}", letter),
            sealed,
            free_bytes: bytes,
            total_bytes: bytes,
        }
    }

    // ── review-r2 R5-3:re-init 已有数据备份盘:不指定 --id 复用现有编号、显式改号被拒(非 force)──
    #[test]
    fn resolve_drive_id_reuses_existing_and_rejects_renumber() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        let cfg = Config {
            system_root: d.path().join("sys"),
            name_prefix: "备份".into(),
            ..Config::default()
        };
        std::fs::create_dir_all(paths::drive_info_dir(&root)).unwrap();
        std::fs::write(paths::drive_id_path(&root), "备份3").unwrap();
        // 不指定 --id → 复用现有「备份3」,不分配新号
        assert_eq!(
            resolve_drive_id(&root, None, false, &cfg, &NoopReporter).unwrap(),
            "备份3"
        );
        // 指定相同 --id → OK
        assert_eq!(
            resolve_drive_id(&root, Some("备份3"), false, &cfg, &NoopReporter).unwrap(),
            "备份3"
        );
        // 指定不同 --id 且非 force → 拒绝改号
        assert!(resolve_drive_id(&root, Some("备份9"), false, &cfg, &NoopReporter).is_err());
        // --force 可改号
        assert_eq!(
            resolve_drive_id(&root, Some("备份9"), true, &cfg, &NoopReporter).unwrap(),
            "备份9"
        );
    }

    // ── review-r2 R4-3:root_is_empty 应忽略 OS 杂文件(desktop.ini/Thumbs.db),否则空盘被误判非空 ──
    #[test]
    fn root_is_empty_ignores_cruft_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("desktop.ini"), b"x").unwrap();
        std::fs::write(d.path().join("Thumbs.db"), b"x").unwrap();
        assert!(
            root_is_empty(d.path()).unwrap(),
            "只有 OS 自动生成的杂文件应视为空盘"
        );
        std::fs::write(d.path().join("real.txt"), b"x").unwrap();
        assert!(!root_is_empty(d.path()).unwrap(), "有真实文件应判非空");
    }

    // ── Spec D §4.2 / DESIGN §4: init_decision 真值表（非空仅软提示，不阻断）──
    #[test]
    fn init_decision_truth_table() {
        let mk = |sys, lib, bak, ne| InitClass {
            is_system: sys,
            is_library: lib,
            already_backup: bak,
            non_empty: ne,
        };
        let empty = init_decision(&mk(false, false, false, false));
        assert!(empty.can_init, "空盘可初始化");
        assert!(empty.soft_hint.is_none());

        assert!(
            init_decision(&mk(false, false, true, false)).can_init,
            "已是备份盘可 re-init"
        );
        assert!(
            init_decision(&mk(false, false, true, true)).can_init,
            "已是备份盘优先于非空"
        );
        assert!(
            !init_decision(&mk(true, false, false, false)).can_init,
            "系统盘拒绝"
        );
        assert!(
            !init_decision(&mk(false, true, false, false)).can_init,
            "资料库盘拒绝"
        );

        let ne = init_decision(&mk(false, false, false, true));
        assert!(ne.can_init, "非空盘不阻断（软提示）");
        assert!(ne.block_reason.is_none());
        assert!(
            ne.soft_hint.as_deref().is_some_and(|s| s.contains("非空")),
            "非空应带 soft_hint: {:?}",
            ne.soft_hint
        );
    }

    // ── DESIGN §4 P0: apply_init_gates — 非空只 warn 不 Err；系统盘无 force 仍拒 ──
    #[test]
    fn apply_init_gates_non_empty_warns_not_blocks() {
        let c = InitClass {
            is_system: false,
            is_library: false,
            already_backup: false,
            non_empty: true,
        };
        let rep = RecReporter(std::sync::Mutex::new(Vec::new()));
        assert!(
            apply_legacy_init_gates(&c, "E", false, &rep).is_ok(),
            "非空盘 init 不得 bail"
        );
        let logs = rep.0.lock().unwrap();
        assert!(
            logs.iter().any(|m| m.contains("非空")),
            "非空必须 warn，实际: {:?}",
            logs
        );
    }

    #[test]
    fn apply_init_gates_system_refused_without_force() {
        let c = InitClass {
            is_system: true,
            is_library: false,
            already_backup: false,
            non_empty: false,
        };
        let rep = RecReporter(std::sync::Mutex::new(Vec::new()));
        let err = apply_legacy_init_gates(&c, "C", false, &rep).unwrap_err();
        assert!(
            err.to_string().contains("系统盘"),
            "系统盘必须硬拒: {}",
            err
        );
        // --force 可绕过硬闸
        let rep2 = RecReporter(std::sync::Mutex::new(Vec::new()));
        assert!(apply_legacy_init_gates(&c, "C", true, &rep2).is_ok());
    }

    #[test]
    fn apply_init_gates_library_refused_without_force() {
        let c = InitClass {
            is_system: false,
            is_library: true,
            already_backup: false,
            non_empty: true, // 非空 + 资料库：仍硬拒资料库，非空不另开闸
        };
        let rep = RecReporter(std::sync::Mutex::new(Vec::new()));
        let err = apply_legacy_init_gates(&c, "D", false, &rep).unwrap_err();
        assert!(
            err.to_string().contains("资料库"),
            "资料库盘必须硬拒: {}",
            err
        );
    }

    // ── Spec D §4.2 / L-021: try_into_writable 类型化盘 ──
    #[test]
    fn try_into_writable_rejects_sealed_and_small() {
        assert!(di("E", 500, true).try_into_writable(200).is_err()); // 封盘
        assert!(di("F", 8, false).try_into_writable(200).is_err()); // 过小
        let w = di("E", 500, false).try_into_writable(200);
        assert!(w.is_ok());
        assert_eq!(w.unwrap().into_inner().letter, "E");
    }

    // ── L-006: min_drive_gb 真正生效,排除过小盘(防误抓 U 盘) ──
    #[test]
    fn usable_drives_excludes_below_min_size() {
        let all = vec![di("F", 8, false), di("E", 500, false)];
        let (usable, too_small) = crate::pool::scan::usable_drives(all, 200);
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].letter, "E");
        assert_eq!(too_small.len(), 1);
        assert_eq!(too_small[0].letter, "F");
    }

    #[test]
    fn usable_drives_excludes_sealed_without_marking_too_small() {
        let all = vec![di("E", 500, true)];
        let (usable, too_small) = crate::pool::scan::usable_drives(all, 200);
        assert!(usable.is_empty());
        assert!(too_small.is_empty(), "已封盘不应被算作'过小'");
    }

    #[test]
    fn usable_drives_keeps_large_unsealed() {
        let all = vec![di("E", 500, false)];
        let (usable, too_small) = crate::pool::scan::usable_drives(all, 200);
        assert_eq!(usable.len(), 1);
        assert!(too_small.is_empty());
    }

    #[test]
    fn usable_drives_at_exact_threshold_is_usable() {
        let all = vec![di("E", 200, false)];
        let (usable, _) = crate::pool::scan::usable_drives(all, 200);
        assert_eq!(usable.len(), 1, "恰好等于阈值应可用");
    }

    // ── L-023: 盘符解析字符安全,不对多字节首字符/UNC panic ──
    #[test]
    fn drive_letter_of_is_char_safe() {
        assert_eq!(drive_letter_of(Path::new("E:\\")), Some("E".to_string()));
        assert_eq!(drive_letter_of(Path::new("c:\\x")), Some("C".to_string()));
        assert_eq!(drive_letter_of(Path::new(r"\\server\share")), None);
        assert_eq!(drive_letter_of(Path::new("中:\\x")), None); // 多字节首字符:不 panic
        assert_eq!(drive_letter_of(Path::new("")), None);
        // qualifier_letter 复用 drive_letter_of
        assert_eq!(qualifier_letter(Path::new("E:\\")), Some("E".to_string()));
    }

    // ── L-027: 盘编号单调不碰撞 ──
    #[test]
    fn next_number_is_max_plus_one() {
        assert_eq!(next_number(&[]), 1);
        assert_eq!(next_number(&[1, 2, 3]), 4);
        assert_eq!(next_number(&[5, 2, 5]), 6); // 乱序 + 重复
        assert_eq!(next_number(&[10]), 11);
    }

    #[test]
    fn parse_drive_number_basic() {
        assert_eq!(parse_drive_number("备份", "备份12"), Some(12));
        assert_eq!(parse_drive_number("备份", "备份"), None);
        assert_eq!(parse_drive_number("备份", "X3"), None);
        assert_eq!(parse_drive_number("备份", "备份0"), Some(0));
    }

    #[test]
    fn next_drive_number_rejects_malformed_global_catalog() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config {
            system_root: d.path().to_path_buf(),
            ..Config::default()
        };
        std::fs::write(
            paths::system_global_catalog(&cfg.system_root),
            "备份盘名,Other\n备份1,ok,extra\n",
        )
        .unwrap();

        assert!(
            next_drive_number(&cfg, &NoopReporter).is_err(),
            "bad global catalog must not be ignored when assigning the next drive id"
        );
    }

    /// 记录型 Reporter:只收集 warn 文本,用于断言「不静默」。
    struct RecReporter(std::sync::Mutex<Vec<String>>);
    impl Reporter for RecReporter {
        fn log(&self, _level: crate::reporter::LogLevel, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
        fn warn(&self, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
        fn progress_bytes(
            &self,
            _label: &str,
            _total: u64,
        ) -> Box<dyn crate::reporter::ProgressHandle> {
            Box::new(NoopProg)
        }
    }
    struct NoopProg;
    impl crate::reporter::ProgressHandle for NoopProg {
        fn inc(&mut self, _delta: u64) {}
        fn finish(&mut self) {}
    }

    // ── review-r3 round2:盘号计数文件「存在但内容损坏」→ 发 warn(不再静默)且不 bail,仍能算出编号
    // (区别于全局索引损坏的硬失败,也区别于文件不存在的正常静默)──
    #[test]
    fn next_drive_number_warns_on_corrupt_seq_not_silent() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config {
            system_root: d.path().to_path_buf(),
            ..Config::default()
        };
        let seq = paths::system_drive_seq(&cfg.system_root);
        if let Some(p) = seq.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&seq, "garbage").unwrap(); // 损坏:非数字
        let rep = RecReporter(std::sync::Mutex::new(Vec::new()));
        assert!(
            next_drive_number(&cfg, &rep).is_ok(),
            "损坏 seq 应 warn 跳过而非 bail(区别于全局索引损坏的硬失败)"
        );
        let logs = rep.0.lock().unwrap();
        assert!(
            logs.iter().any(|m| m.contains("盘号计数文件")),
            "损坏 seq 必须发 warn(不静默),实际 warn:{:?}",
            logs
        );
    }

    // ── review-r3 round3:bump_drive_seq 遇损坏 seq 不得当 cur=0 把计数器写小(回退→重号)──
    #[test]
    fn bump_drive_seq_corrupt_does_not_shrink_counter() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config {
            system_root: d.path().to_path_buf(),
            ..Config::default()
        };
        let seq = paths::system_drive_seq(&cfg.system_root);
        if let Some(p) = seq.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&seq, "garbage").unwrap(); // 损坏:不可解析(模拟读不出真实大值)
        let rep = RecReporter(std::sync::Mutex::new(Vec::new()));
        bump_drive_seq(&cfg, 3, &rep).unwrap();
        assert_eq!(
            std::fs::read_to_string(&seq).unwrap(),
            "garbage",
            "损坏 seq 不应被覆写成较小的 3(避免计数器回退)"
        );
        assert!(
            rep.0
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.contains("盘号计数文件")),
            "损坏 seq 的 bump 必须发 warn"
        );
    }

    #[test]
    fn seal_rejects_malformed_drive_catalog() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("drive");
        std::fs::create_dir_all(paths::drive_info_dir(&root)).unwrap();
        std::fs::write(
            paths::drive_catalog_path(&root),
            "ProjectName,TotalBytes\n\"unterminated",
        )
        .unwrap();
        let drive = DriveInfo {
            letter: "E".into(),
            root: root.clone(),
            id: "备份1".into(),
            sealed: false,
            free_bytes: 0,
            total_bytes: 0,
        };

        assert!(
            seal(&drive).is_err(),
            "bad drive catalog must not produce an authoritative sealed summary"
        );
        assert!(!paths::drive_sealed_path(&root).exists());
    }

    // ── 强优化:seal 正向路径 —— 合法本盘索引应写出封盘标记,统计项目数/总字节正确,且可被识别 ──
    // 现有 seal 测试只覆盖『坏索引 → 不产出标记』;本测试锁死正向写出与 drive_is_sealed 识别链路。
    #[test]
    fn seal_writes_marker_with_counts_and_is_recognized() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("drive");
        std::fs::create_dir_all(paths::drive_info_dir(&root)).unwrap();
        std::fs::write(
            paths::drive_catalog_path(&root),
            "ProjectName,TotalBytes\nA,10\nB,20\n",
        )
        .unwrap();
        let drive = DriveInfo {
            letter: "E".into(),
            root: root.clone(),
            id: "备份1".into(),
            sealed: false,
            free_bytes: 0,
            total_bytes: 0,
        };
        seal(&drive).unwrap();
        let marker = paths::drive_sealed_path(&root);
        assert!(marker.is_file(), "合法索引应写出封盘标记");
        assert!(drive_is_sealed(&root), "封盘后 drive_is_sealed 应为 true");
        let text = std::fs::read_to_string(&marker).unwrap();
        assert!(
            text.contains("归档总字节"),
            "标记应含归档总字节,实际:\n{text}"
        );
        assert!(
            text.contains("30"),
            "归档总字节应为 30(10+20),实际:\n{text}"
        );
        assert!(
            text.lines()
                .any(|l| l.contains("项目数量") && l.contains('2')),
            "项目数量应为 2,实际:\n{text}"
        );
    }
    #[test]
    fn init_preserves_abnormal_sealed_marker_directory_even_with_force() {
        for force in [false, true] {
            let world = tempfile::tempdir().unwrap();
            let root = world.path().join("synthetic-drive");
            let marker = paths::drive_sealed_path(&root);
            fs::create_dir_all(&marker).unwrap();
            fs::write(marker.join("user-document.txt"), b"KEEP-ME").unwrap();
            let cfg = Config {
                system_root: world.path().join("system"),
                ..Config::default()
            };
            let result =
                init_legacy_at_root(&cfg, &NoopReporter, "T", &root, Some("备份99"), force);
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("已保留原内容"));
            assert_eq!(
                fs::read(marker.join("user-document.txt")).unwrap(),
                b"KEEP-ME"
            );
            assert!(!paths::drive_id_path(&root).exists());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn init_preserves_abnormal_sealed_marker_symlink() {
        use std::os::unix::fs::symlink;
        let world = tempfile::tempdir().unwrap();
        let root = world.path().join("synthetic-drive");
        fs::create_dir_all(paths::drive_info_dir(&root)).unwrap();
        let outside = world.path().join("outside.txt");
        fs::write(&outside, b"KEEP-ME").unwrap();
        symlink(&outside, paths::drive_sealed_path(&root)).unwrap();
        let cfg = Config {
            system_root: world.path().join("system"),
            ..Config::default()
        };
        assert!(
            init_legacy_at_root(&cfg, &NoopReporter, "T", &root, Some("备份99"), true).is_err()
        );
        assert!(fs::symlink_metadata(paths::drive_sealed_path(&root))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(outside).unwrap(), b"KEEP-ME");
    }
    #[test]
    fn init_clears_only_an_ordinary_marker_and_retains_other_user_files() {
        let world = tempfile::tempdir().unwrap();
        let root = world.path().join("synthetic-drive");
        fs::create_dir_all(paths::drive_info_dir(&root)).unwrap();
        fs::write(paths::drive_sealed_path(&root), b"OLD-MARKER").unwrap();
        fs::write(root.join("user-document.txt"), b"KEEP-ME").unwrap();
        let cfg = Config {
            ready_root: world.path().join("ready"),
            archived_root: world.path().join("archived"),
            system_root: world.path().join("system"),
            ..Config::default()
        };
        init_legacy_at_root(&cfg, &NoopReporter, "T", &root, Some("备份99"), true).unwrap();
        assert!(!paths::drive_sealed_path(&root).exists());
        assert_eq!(
            fs::read(root.join("user-document.txt")).unwrap(),
            b"KEEP-ME"
        );
        assert_eq!(
            fs::read_to_string(paths::drive_id_path(&root)).unwrap(),
            "备份99"
        );
    }
}

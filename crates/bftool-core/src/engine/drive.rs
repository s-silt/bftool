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
use crate::engine::{cruft, durable, paths};
use crate::reporter::Reporter;

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
pub struct WritableDrive(BackupDrive);

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
    pub fn try_into_writable(self, min_drive_gb: u64) -> Result<WritableDrive, DriveError> {
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
    pub fn inner(&self) -> &BackupDrive {
        &self.0
    }
    pub fn into_inner(self) -> BackupDrive {
        self.0
    }
}

pub fn list_mounted(cfg: &Config, reporter: &dyn Reporter) -> Result<()> {
    let drives = scan_mounted()?;
    if drives.is_empty() {
        reporter.warn("未发现已初始化的备份盘。");
        reporter.info("插入一块空盘后，运行：bftool init <盘符> 把它初始化为下一个「备份N」。");
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

/// 扫描所有已挂载、被识别为备份盘的卷。
pub fn scan_mounted() -> Result<Vec<DriveInfo>> {
    let mut out = Vec::new();
    let disks = sysinfo::Disks::new_with_refreshed_list();
    for d in disks.list() {
        let mount = d.mount_point();
        let Some(letter) = drive_letter_of(mount) else {
            continue;
        };
        let root = PathBuf::from(format!("{}:\\", letter));
        let id_file = paths::drive_id_path(&root);
        if !id_file.is_file() {
            continue;
        }
        let id = match fs::read_to_string(&id_file) {
            Ok(s) => s.trim().to_string(),
            Err(_) => continue,
        };
        out.push(DriveInfo {
            letter,
            root: root.clone(),
            id,
            sealed: paths::drive_sealed_path(&root).is_file(),
            free_bytes: d.available_space(),
            total_bytes: d.total_space(),
        });
    }
    Ok(out)
}

/// 从一组盘里挑出"可写入"的(未封盘且容量 ≥ min_drive_gb),并把"过小被忽略"的单独返回。
/// 纯函数便于测试;容量过滤是「防误抓 U 盘」安全闸 —— 此前 min_drive_gb 形同虚设。(ledger L-006)
fn usable_drives(all: Vec<DriveInfo>, min_drive_gb: u64) -> (Vec<DriveInfo>, Vec<DriveInfo>) {
    let min_bytes = min_drive_gb.saturating_mul(1024 * 1024 * 1024);
    let mut usable = Vec::new();
    let mut too_small = Vec::new();
    for d in all.into_iter().filter(|d| !d.sealed) {
        if d.total_bytes >= min_bytes {
            usable.push(d);
        } else {
            too_small.push(d);
        }
    }
    (usable, too_small)
}

/// 当前在线、未封盘、容量达标的备份盘列表（不打日志、不挑唯一）。
/// `run_plan` 在执行前用它复验「单盘不变式」——`pick_active` 的多盘检查只在 `plan()` 跑过一次，
/// 预览→执行之间若插入第二块可写盘，需在这里重新拦下，否则单盘安全闸被绕过。(SEC-007)
pub fn usable_drives_now(min_drive_gb: u64) -> Result<Vec<DriveInfo>> {
    let (usable, _too_small) = usable_drives(scan_mounted()?, min_drive_gb);
    Ok(usable)
}

/// 返回唯一一块未封盘且容量达标的备份盘；多块返回错误；零块返回 None。
/// 容量过滤(min_drive_gb)是防误抓 U 盘/SD 卡的安全闸。(ledger L-006)
pub fn pick_active(min_drive_gb: u64, reporter: &dyn Reporter) -> Result<Option<DriveInfo>> {
    let (usable, too_small) = usable_drives(scan_mounted()?, min_drive_gb);
    for d in &too_small {
        reporter.warn(&format!(
            "忽略疑似过小的盘 {} ({}:) {:.0}GB(低于最小 {}GB)—— 防误抓 U 盘/SD 卡。\
             若确需用它,把配置 min_drive_gb 调低后重试。",
            d.id,
            d.letter,
            d.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
            min_drive_gb
        ));
    }
    if usable.len() > 1 {
        let names = usable
            .iter()
            .map(|d| format!("{}({}:)", d.id, d.letter))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "检测到多块未封盘的备份盘：{} —— 为防止写错盘已停止。请只保留一块在线（其余盘可封盘或拔下）。",
            names
        );
    }
    Ok(usable.into_iter().next())
}

fn drive_letter_of(p: &Path) -> Option<String> {
    // 字符安全:对 UNC(\\server)、卷 GUID、多字节首字符的挂载点返回 None 而非 panic。
    // 旧实现 `&s[1..2]`/`s[..1]` 按字节切片,首字符是多字节字符时会 panic。(ledger L-023)
    let s = p.to_string_lossy();
    let mut it = s.chars();
    let first = it.next()?;
    if first.is_ascii_alphabetic() && it.next() == Some(':') {
        Some(first.to_ascii_uppercase().to_string())
    } else {
        None
    }
}

/// 初始化一块盘为下一个「备份N」（或自定义 ID）
pub fn init(
    cfg: &Config,
    reporter: &dyn Reporter,
    drive_letter: &str,
    id: Option<&str>,
    force: bool,
) -> Result<()> {
    let letter = drive_letter.trim_end_matches(':').to_uppercase();
    if letter.len() != 1 {
        bail!("盘符无效：{}（应为单字母，例如 E）", drive_letter);
    }
    let root = PathBuf::from(format!("{}:\\", letter));
    if !root.exists() {
        bail!("驱动器 {}: 不存在或未挂载", letter);
    }

    // 防呆：系统盘 / 资料库盘 / 已是备份盘 / 非空盘 —— 判定收口到 classify_for_init(与 init_candidates 共用)
    if !force {
        let c = classify_for_init(cfg, &letter, &root)?;
        if c.is_system {
            bail!("拒绝初始化系统盘 {}:（如确需，请加 --force）", letter);
        }
        if c.is_library {
            bail!(
                "拒绝初始化资料库所在盘 {}:（待备份/已备份/备份系统 在此盘；如确需，请加 --force）",
                letter
            );
        }
        if c.already_backup {
            // 已是备份盘 → 不阻止（等同重写元数据），但提示
            reporter.warn(&format!(
                "{}: 已经是一块初始化过的备份盘；将覆盖元数据，但不会动 \\项目\\ 下的数据。",
                letter
            ));
        } else if c.non_empty {
            bail!(
                "拒绝初始化非空盘 {}:（根目录有数据，怕认错盘；如确需，请加 --force）",
                letter
            );
        }
    }

    let id = match id {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => format!("{}{}", cfg.name_prefix, next_drive_number(cfg)?),
    };

    // 写盘内目录
    let info = paths::drive_info_dir(&root);
    fs::create_dir_all(&info).context("创建本盘信息目录失败")?;
    fs::create_dir_all(paths::drive_logs_dir(&root)).ok();
    fs::create_dir_all(paths::drive_manifest_dir(&root)).ok();
    fs::create_dir_all(paths::drive_projects_dir(&root)).ok();
    // 原子写(tmp+fsync+rename):本盘编号是认盘依据,断电不能留 0 字节/半截坏文件。(review-r2 R4-4)
    durable::write_synced(&paths::drive_id_path(&root), id.as_bytes()).context("写本盘编号失败")?;
    durable::write_synced(&info.join(paths::DRIVE_README_FILE), readme(&id).as_bytes())?;

    // 序号文件追踪：保证下次取下一块的时候编号单调递增
    if let Some(n) = parse_drive_number(&cfg.name_prefix, &id) {
        bump_drive_seq(cfg, n)?;
    }

    // re-init = 当作新盘用:若残留封盘标记,清除它。否则盘虽被重新初始化、提示"可以 archive",
    // 但封盘标记仍在 → pick_active/try_into_writable 仍判其为已封盘而拒写,提示与实际矛盾。(EH-005)
    let sealed_marker = paths::drive_sealed_path(&root);
    if sealed_marker.is_file() {
        fs::remove_file(&sealed_marker)
            .with_context(|| format!("清除封盘标记失败：{}", sealed_marker.display()))?;
        reporter.info(&format!(
            "已清除 {}: 的封盘标记(重新初始化 = 当作可写新盘)。",
            letter
        ));
    }

    reporter.ok(&format!("已初始化备份盘 {} ({}:)", id, letter));
    if !force && paths::drive_id_path(&root).is_file() {
        // 提示用户可以接着 archive
        reporter.info(
            "现在可以运行 `bftool archive` 开始归档；先 `bftool archive --dry-run` 演练一下更稳。",
        );
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

/// 根目录是否为空(忽略系统目录与 OS 自动注入的杂文件)。
/// 杂文件名单统一交给 cruft 判定(desktop.ini/Thumbs.db/$RECYCLE.BIN/System Volume Information…),
/// 否则一块全新空盘只要有个 desktop.ini/Thumbs.db 就被误判非空、拒绝初始化。(review-r2 R4-3)
fn root_is_empty(root: &Path) -> Result<bool> {
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

/// 由分类得出能否初始化 + 阻断原因。
fn init_decision(c: &InitClass) -> (bool, Option<String>) {
    if c.is_system {
        return (false, Some("系统盘".into()));
    }
    if c.is_library {
        return (false, Some("资料库所在盘(待备份/已备份/备份系统)".into()));
    }
    if !c.already_backup && c.non_empty {
        return (false, Some("根目录非空(怕认错盘)".into()));
    }
    (true, None) // 含"已是备份盘"(re-init 覆盖元数据,允许)
}

/// GUI 初始化页用:列出所有挂载盘 + 结构化防呆判定。(Spec D §4.2)
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
        let (can_init, block_reason) = init_decision(&cls);
        out.push(InitCandidate {
            letter,
            total_gb: d.total_space() / (1024 * 1024 * 1024),
            is_system: cls.is_system,
            is_library: cls.is_library,
            already_backup: cls.already_backup,
            non_empty: cls.non_empty,
            can_init,
            block_reason,
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

pub fn parse_drive_number(prefix: &str, id: &str) -> Option<u32> {
    id.strip_prefix(prefix).and_then(|s| s.parse::<u32>().ok())
}

/// 下一个编号 = 所有已知「备份N」里的最大值 + 1(空 → 1)。纯函数,保证单调不碰撞。(ledger L-027)
fn next_number(found: &[u32]) -> u32 {
    found.iter().copied().max().unwrap_or(0) + 1
}

/// 计算下一个可用「备份N」编号 = max(序号文件, 全局索引里出现过的「备份N」, 当前挂载盘里的「备份N」) + 1
fn next_drive_number(cfg: &Config) -> Result<u32> {
    let mut found: Vec<u32> = Vec::new();
    let seq_file = paths::system_drive_seq(&cfg.system_root);
    if let Ok(text) = fs::read_to_string(&seq_file) {
        if let Ok(n) = text.trim().parse::<u32>() {
            found.push(n);
        }
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
    for d in scan_mounted()? {
        if let Some(n) = parse_drive_number(&cfg.name_prefix, &d.id) {
            found.push(n);
        }
    }
    Ok(next_number(&found))
}

fn bump_drive_seq(cfg: &Config, n: u32) -> Result<()> {
    let seq_file = paths::system_drive_seq(&cfg.system_root);
    if let Some(parent) = seq_file.parent() {
        fs::create_dir_all(parent).ok();
    }
    let cur = fs::read_to_string(&seq_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    if n > cur {
        durable::write_synced(&seq_file, n.to_string().as_bytes())?; // 原子写,断电不留半截序号(review-r2 R4-4)
    }
    Ok(())
}

/// 写封盘标记。当前盘剩余不足下一个项目时调用。
pub fn seal(drive: &DriveInfo) -> Result<()> {
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
pub fn root_from_letter(letter: &str) -> Result<PathBuf> {
    let l = letter.trim_end_matches(':').to_uppercase();
    if l.len() != 1 {
        bail!("盘符无效：{}（应为单字母）", letter);
    }
    Ok(PathBuf::from(format!("{}:\\", l)))
}

/// 根据盘符直接读 DriveInfo（不要求事先 scan_mounted）。
pub fn info_by_letter(letter: &str) -> Result<DriveInfo> {
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
    // 容量信息：从 sysinfo 兜底；失败则填 0
    let mut free = 0;
    let mut total = 0;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    for d in disks.list() {
        if let Some(l) = drive_letter_of(d.mount_point()) {
            if l == letter {
                free = d.available_space();
                total = d.total_space();
                break;
            }
        }
    }
    Ok(DriveInfo {
        letter,
        root: root.clone(),
        id,
        sealed: paths::drive_sealed_path(&root).is_file(),
        free_bytes: free,
        total_bytes: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── Spec D §4.2 Finding #4: init_decision 真值表 ──
    #[test]
    fn init_decision_truth_table() {
        let mk = |sys, lib, bak, ne| InitClass {
            is_system: sys,
            is_library: lib,
            already_backup: bak,
            non_empty: ne,
        };
        assert!(
            init_decision(&mk(false, false, false, false)).0,
            "空盘可初始化"
        );
        assert!(
            init_decision(&mk(false, false, true, false)).0,
            "已是备份盘可 re-init"
        );
        assert!(
            init_decision(&mk(false, false, true, true)).0,
            "已是备份盘优先于非空"
        );
        assert!(
            !init_decision(&mk(true, false, false, false)).0,
            "系统盘拒绝"
        );
        assert!(
            !init_decision(&mk(false, true, false, false)).0,
            "资料库盘拒绝"
        );
        assert!(
            !init_decision(&mk(false, false, false, true)).0,
            "非空盘拒绝"
        );
    }

    // ── Spec D §4.2 / L-021: try_into_writable 类型化盘 ──
    #[test]
    fn try_into_writable_rejects_sealed_and_small() {
        assert!(di("E", 500, true).try_into_writable(200).is_err()); // 封盘
        assert!(di("F", 8, false).try_into_writable(200).is_err()); // 过小
        let w = di("E", 500, false).try_into_writable(200);
        assert!(w.is_ok());
        assert_eq!(w.unwrap().inner().letter, "E");
    }

    // ── L-006: min_drive_gb 真正生效,排除过小盘(防误抓 U 盘) ──
    #[test]
    fn usable_drives_excludes_below_min_size() {
        let all = vec![di("F", 8, false), di("E", 500, false)];
        let (usable, too_small) = usable_drives(all, 200);
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].letter, "E");
        assert_eq!(too_small.len(), 1);
        assert_eq!(too_small[0].letter, "F");
    }

    #[test]
    fn usable_drives_excludes_sealed_without_marking_too_small() {
        let all = vec![di("E", 500, true)];
        let (usable, too_small) = usable_drives(all, 200);
        assert!(usable.is_empty());
        assert!(too_small.is_empty(), "已封盘不应被算作'过小'");
    }

    #[test]
    fn usable_drives_keeps_large_unsealed() {
        let all = vec![di("E", 500, false)];
        let (usable, too_small) = usable_drives(all, 200);
        assert_eq!(usable.len(), 1);
        assert!(too_small.is_empty());
    }

    #[test]
    fn usable_drives_at_exact_threshold_is_usable() {
        let all = vec![di("E", 200, false)];
        let (usable, _) = usable_drives(all, 200);
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
            next_drive_number(&cfg).is_err(),
            "bad global catalog must not be ignored when assigning the next drive id"
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
}

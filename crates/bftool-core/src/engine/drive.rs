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
use crate::engine::paths;
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

    // 防呆：不允许系统盘 / 资料库三目录所在盘 / 已是其它备份盘 / 根目录非空
    if !force {
        let sys = system_drive_letter();
        if sys.eq_ignore_ascii_case(&letter) {
            bail!("拒绝初始化系统盘 {}:（如确需，请加 --force）", letter);
        }
        let lib_letters = [&cfg.ready_root, &cfg.archived_root, &cfg.system_root]
            .iter()
            .filter_map(|p| qualifier_letter(p))
            .collect::<Vec<_>>();
        if lib_letters.iter().any(|l| l.eq_ignore_ascii_case(&letter)) {
            bail!(
                "拒绝初始化资料库所在盘 {}:（待备份/已备份/备份系统 在此盘；如确需，请加 --force）",
                letter
            );
        }
        // 已是备份盘 → 不阻止（init 等同重新写元数据），但提示
        if paths::drive_id_path(&root).is_file() {
            reporter.warn(&format!(
                "{}: 已经是一块初始化过的备份盘；将覆盖元数据，但不会动 \\项目\\ 下的数据。",
                letter
            ));
        } else {
            // 不是备份盘 → 必须根目录为空（忽略 Windows 系统目录）
            let ignore: &[&str] = &[
                "System Volume Information",
                "$RECYCLE.BIN",
                "RECYCLER",
                "found.000",
                "lost+found",
                ".Trashes",
            ];
            let entries = fs::read_dir(&root)
                .with_context(|| format!("读取 {}: 根目录失败", letter))?
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name();
                    let n = name.to_string_lossy();
                    !ignore.iter().any(|x| x.eq_ignore_ascii_case(&n))
                })
                .count();
            if entries > 0 {
                bail!(
                    "拒绝初始化非空盘 {}:（根目录有数据，怕认错盘；如确需，请加 --force）",
                    letter
                );
            }
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
    fs::write(paths::drive_id_path(&root), &id).context("写本盘编号失败")?;
    fs::write(info.join(paths::DRIVE_README_FILE), readme(&id))?;

    // 序号文件追踪：保证下次取下一块的时候编号单调递增
    if let Some(n) = parse_drive_number(&cfg.name_prefix, &id) {
        bump_drive_seq(cfg, n)?;
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

fn readme(id: &str) -> String {
    format!(
        "本盘编号 : {id}\n用途     : 项目文件夹备份（以文件夹为最小单位；增量复制，非镜像，绝不删除）\n目录结构 :\n  \\项目\\<项目名>\\          原样存放的项目，可直接复制恢复\n  \\本盘信息\\本盘编号.txt         本盘编号（程序识别用，请勿改动）\n  \\本盘信息\\本盘说明.txt          本说明文件\n  \\本盘信息\\本盘索引记录.csv    本盘项目清单\n  \\本盘信息\\校验清单\\*.sha256.csv  每个项目的逐文件 SHA256 校验清单\n  \\本盘信息\\日志\\                备份日志\n  \\本盘信息\\已封盘.txt            存在即表示本盘已写满/停用，请勿再写入\n校验方式 : 文件数 + 总字节数 + 每个文件的 SHA256\n恢复方法 : 直接把 \\项目\\<项目名> 复制回去即可；如需核对，用 校验清单 重算 SHA256 比对\n复查建议 : 每 6～12 个月通电一次，重算哈希与 校验清单 比对，检查有无坏道\n初始化时间: {}\n",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    )
}

pub fn parse_drive_number(prefix: &str, id: &str) -> Option<u32> {
    id.strip_prefix(prefix).and_then(|s| s.parse::<u32>().ok())
}

/// 计算下一个可用「备份N」编号 = max(序号文件, 全局索引里出现过的「备份N」, 当前挂载盘里的「备份N」) + 1
fn next_drive_number(cfg: &Config) -> Result<u32> {
    let mut max = 0u32;
    let seq_file = paths::system_drive_seq(&cfg.system_root);
    if let Ok(text) = fs::read_to_string(&seq_file) {
        if let Ok(n) = text.trim().parse::<u32>() {
            max = max.max(n);
        }
    }
    let gc = paths::system_global_catalog(&cfg.system_root);
    if gc.is_file() {
        if let Ok(mut rdr) = csv::Reader::from_path(&gc) {
            // 用动态 row：先读 headers 找「备份盘名」列
            let headers = rdr.headers().cloned().unwrap_or_default();
            if let Some(col) = headers.iter().position(|h| h == "备份盘名") {
                for rec in rdr.records().flatten() {
                    if let Some(v) = rec.get(col) {
                        if let Some(n) = parse_drive_number(&cfg.name_prefix, v.trim()) {
                            max = max.max(n);
                        }
                    }
                }
            }
        }
    }
    for d in scan_mounted()? {
        if let Some(n) = parse_drive_number(&cfg.name_prefix, &d.id) {
            max = max.max(n);
        }
    }
    Ok(max + 1)
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
        fs::write(&seq_file, n.to_string())?;
    }
    Ok(())
}

/// 写封盘标记。当前盘剩余不足下一个项目时调用。
pub fn seal(drive: &DriveInfo) -> Result<()> {
    let cat = paths::drive_catalog_path(&drive.root);
    let (cnt, bytes) = if cat.is_file() {
        let mut total_bytes = 0u64;
        let mut total = 0u32;
        if let Ok(mut rdr) = csv::Reader::from_path(&cat) {
            let headers = rdr.headers().cloned().unwrap_or_default();
            let bcol = headers.iter().position(|h| h == "TotalBytes");
            for r in rdr.records().flatten() {
                total += 1;
                if let Some(c) = bcol {
                    if let Some(s) = r.get(c) {
                        if let Ok(n) = s.parse::<u64>() {
                            total_bytes += n
                        }
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
    fs::write(paths::drive_sealed_path(&drive.root), text)
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
}

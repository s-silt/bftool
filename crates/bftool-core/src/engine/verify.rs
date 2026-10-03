//! 复查：对一块盘上每个项目重算 SHA256，比对清单；同时报告"清单外多余文件"。
//! 借鉴 restic check / conserve validate 的理念：完整性检查要覆盖 数据 + 元数据。
//! **对备份盘严格只读**(复查时间/结果记到本地 system_root,见 verify_state)。

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Config;
use crate::engine::{cruft, drive, paths, verify_state};
use crate::reporter::Reporter;

/// 复查发现的一类问题(带项目上下文,GUI 能定位"哪个项目的哪个文件")。(Spec D §4.1 Finding #3)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyIssueKind {
    Missing,      // 清单有、盘上无
    SizeMismatch, // 大小不符
    Corrupt,      // SHA256 不一致
    Unverifiable, // 清单缺失/元数据不一致/清单项没有可验证属性
    ReadError,    // 元数据/内容读取失败
    EnumError,    // 枚举失败
}

#[derive(Debug, Clone)]
pub struct VerifyIssue {
    pub project: String,
    pub rel: String,
    pub kind: VerifyIssueKind,
}

#[derive(Debug, Clone)]
pub struct ExtraFile {
    pub project: String,
    pub rel: String,
}

/// 复查结论(供 last_verify 记录;Spec D §4.4)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    Clean,
    IssuesFound {
        bad: u64,
    },
    ExtraOnly {
        extra: u64,
    },
    /// 无损坏/无多余,但有文件仅按大小校验、未验证内容(清单无哈希)。不是「完好」——
    /// 持久化此态,避免事后仪表盘/CLI 把『内容从未被 SHA256 校验过』的盘绿标为完好。(review-r3 round5)
    CleanButSizeOnly {
        size_only: u64,
    },
    Cancelled,
}

impl VerifyOutcome {
    /// 复查结论的权威中文标签。CLI(status)与 GUI(dashboard)统一复用,避免同一结论两套说法
    /// (历史上 CLI「发现损坏!」与 GUI「发现损坏」曾漂移,新增/改分支须人肉同步两处易再漂)。
    /// GUI 的配色是 UI 关注点,另在其渲染层决定。(强优化 review)
    pub fn label(&self) -> &'static str {
        match self {
            VerifyOutcome::Clean => "完好",
            VerifyOutcome::IssuesFound { .. } => "发现完整性问题",
            VerifyOutcome::ExtraOnly { .. } => "有多余文件",
            VerifyOutcome::CleanButSizeOnly { .. } => "仅大小校验·未验证内容",
            VerifyOutcome::Cancelled => "上次取消",
        }
    }
}

/// 复查结果。`has_corruption()` = 发现完整性问题;结构化明细在 `issues`/`extras`。
/// CLI 据此设非零退出码(L-007);GUI 直接列明细(Spec D §4.1)。
#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    pub checked: u64,
    pub bad: u64,
    pub extra: u64,
    /// 仅按**大小**校验、未验证内容(SHA256)的文件数 —— 清单该行 Hash 为空(no_hash 归档 /
    /// 缺 Hash 列 / 单元格被裁空)。>0 说明本次复查无法检测等长内容篡改,不能笼统呈现为「完好」。(review-r3 round4)
    pub size_only: u64,
    pub issues: Vec<VerifyIssue>,
    pub extras: Vec<ExtraFile>,
    pub cancelled: bool,
}

impl VerifyReport {
    /// 是否发现完整性问题(含不可验证/读取失败)。沿用 CLI 的非零退出判据;extra 仅警告。
    pub fn has_corruption(&self) -> bool {
        self.bad > 0
    }

    /// 记一条问题:计数与明细一处更新,不会漂移。
    fn push_issue(&mut self, project: &str, rel: &str, kind: VerifyIssueKind) {
        self.bad += 1;
        self.issues.push(VerifyIssue {
            project: project.to_string(),
            rel: rel.to_string(),
            kind,
        });
    }

    fn push_extra(&mut self, project: &str, rel: &str) {
        self.extra += 1;
        self.extras.push(ExtraFile {
            project: project.to_string(),
            rel: rel.to_string(),
        });
    }

    /// 复查结论(取消优先;否则 损坏 > 多余 > clean)。供 last_verify 记录。
    pub fn outcome(&self) -> VerifyOutcome {
        if self.cancelled {
            VerifyOutcome::Cancelled
        } else if self.bad > 0 {
            VerifyOutcome::IssuesFound { bad: self.bad }
        } else if self.extra > 0 {
            VerifyOutcome::ExtraOnly { extra: self.extra }
        } else if self.size_only > 0 {
            // 无损坏/多余,但有「仅大小校验」文件 → 不是 Clean,持久化为可区分态。(review-r3 round5)
            VerifyOutcome::CleanButSizeOnly {
                size_only: self.size_only,
            }
        } else {
            VerifyOutcome::Clean
        }
    }
}

pub fn run(
    cfg: &Config,
    reporter: &dyn Reporter,
    drive_letter: Option<&str>,
    cancel: &AtomicBool,
) -> Result<VerifyReport> {
    let drives = drive::scan_mounted(Some(reporter))?;
    if drives.is_empty() {
        bail!("未发现已初始化的备份盘。");
    }
    let target = match drive_letter {
        Some(l) => {
            let letter = l.trim_end_matches(':').to_uppercase();
            drives
                .into_iter()
                .find(|d| d.letter == letter)
                .ok_or_else(|| {
                    anyhow::anyhow!("找不到盘 {}:（用 `bftool drives` 看一下）", letter)
                })?
        }
        None => {
            if drives.len() == 1 {
                drives.into_iter().next().unwrap()
            } else {
                reporter.info("检测到多块备份盘；请显式指定盘符。已识别：");
                for (i, d) in drives.iter().enumerate() {
                    reporter.info(&format!("  [{}] {} ({}:)", i + 1, d.id, d.letter));
                }
                bail!("用法：bftool verify <盘符>（例：bftool verify E）");
            }
        }
    };

    reporter.info(&format!(
        "开始复查 {} ({}:)，重算 SHA256 / 核对大小，可能较慢…",
        target.id, target.letter
    ));

    let report = verify_drive(&target.root, reporter, cancel)?;

    if report.cancelled {
        reporter.warn("复查已取消(部分项目未检查)。");
        return Ok(report);
    }

    // 跑完(非取消)→ 把复查结果记到本地 system_root(verify 对盘只读);best-effort,失败只 warn。(Spec D §4.4)
    // 全新空盘没有实际内容被验证,不把零检查结果写成仪表盘绿色的「完好」。
    if report.checked > 0 || report.bad > 0 || report.extra > 0 {
        if let Err(e) = verify_state::record_verify(&cfg.system_root, &target.id, &report.outcome())
        {
            reporter.warn(&format!("复查结果记录失败(不影响本次复查):{}", e));
        }
    }

    let summary = format!(
        "复查完成：检查 {} 个文件，完整性问题(缺失/损坏/不可验证/读取或枚举失败) {} 个,清单外多余 {} 个,仅大小校验(未验证内容) {} 个。",
        report.checked, report.bad, report.extra, report.size_only
    );
    if report.bad > 0 {
        reporter.error(&summary);
    } else if report.extra > 0 || report.size_only > 0 {
        // size-only(无哈希)不是「完好」:降级为 warn,避免绿色「完好」掩盖「未验证内容」。(review-r3 round4)
        reporter.warn(&summary);
    } else if report.checked == 0 {
        reporter.info("没有可校验文件,未验证任何备份内容。");
        reporter.info(&summary);
    } else {
        reporter.ok(&summary);
    }
    if report.size_only > 0 {
        reporter.warn(&format!(
            "注意:{} 个文件只校验了大小、未验证内容(清单无哈希,如 --unsafe-no-hash 归档)——\
             等长内容篡改/比特腐烂无法被本次复查发现。",
            report.size_only
        ));
    }
    if report.extra > 0 {
        reporter.info("（多余文件不会自动删除；如确认无用可人工清理。）");
    }
    Ok(report)
}

/// 对已初始化备份盘的实际目录做整盘复查,不依赖 Windows 挂载枚举。
/// 与 `run` 共用的生产入口,便于 Linux/离线介质及合成目录验证;始终只读,
/// 不修复元数据、不更新 last_verify,也不绕过本盘标识与普通目录检查。
/// 本盘索引、项目目录、清单的并集都必须核对,不能只检查仍然存在的清单。
pub fn verify_drive(
    root: &Path,
    reporter: &dyn Reporter,
    cancel: &AtomicBool,
) -> Result<VerifyReport> {
    validate_drive_root(root)?;
    let mut report = VerifyReport::default();
    if cancelled(cancel, &mut report) {
        return Ok(report);
    }
    let catalog = read_drive_catalog(root, reporter, &mut report);
    let mut inventory = BTreeMap::<String, ProjectInventory>::new();
    for (key, registered) in catalog {
        inventory.insert(
            key,
            ProjectInventory {
                name: registered.name.clone(),
                registered: Some(registered),
            },
        );
    }
    enumerate_projects(root, &mut inventory, reporter, &mut report);
    enumerate_manifests(root, &mut inventory, reporter, &mut report);
    for project in inventory.values() {
        if cancelled(cancel, &mut report) {
            break;
        }
        reporter.action(&format!("· {}", project.name));
        reconcile_project(project, reporter, &mut report);
        verify_project_content(
            &paths::drive_projects_dir(root).join(&project.name),
            &paths::drive_manifest_dir(root).join(format!("{}.sha256.csv", project.name)),
            &project.name,
            project.registered.as_ref(),
            None,
            cancel,
            reporter,
            &mut report,
        );
    }
    Ok(report)
}

#[derive(Debug, Clone)]
struct CatalogProject {
    name: String,
    file_count: Option<u64>,
    total_bytes: Option<u64>,
}

#[derive(Default)]
struct ProjectInventory {
    name: String,
    registered: Option<CatalogProject>,
}

fn cancelled(cancel: &AtomicBool, report: &mut VerifyReport) -> bool {
    if cancel.load(Ordering::Relaxed) {
        report.cancelled = true;
        true
    } else {
        false
    }
}

fn validate_drive_root(root: &Path) -> Result<()> {
    for dir in [root.to_path_buf(), paths::drive_info_dir(root)] {
        let meta = fs::symlink_metadata(&dir)
            .with_context(|| format!("读取备份盘目录失败:{}", dir.display()))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            bail!("备份盘目录不是普通目录(不跟随链接):{}", dir.display());
        }
    }
    let id = paths::drive_id_path(root);
    let meta = fs::symlink_metadata(&id)
        .with_context(|| format!("找不到已初始化备份盘标识:{}", id.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        bail!("备份盘标识不是普通文件(不跟随链接):{}", id.display());
    }
    let id_text =
        fs::read_to_string(&id).with_context(|| format!("读取备份盘标识失败:{}", id.display()))?;
    if id_text.trim().is_empty() {
        bail!("备份盘标识为空:{}", id.display());
    }
    Ok(())
}

fn metadata_issue(
    project: &str,
    rel: &str,
    kind: VerifyIssueKind,
    message: &str,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) {
    reporter.error(message);
    report.push_issue(project, rel, kind);
}

/// 只允许一个真实项目名称,CSV 不能借目录分隔符/父段/盘符跳出「项目」。
fn valid_project_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['\\', '/', ':'])
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}

/// 缺失 catalog 是刚初始化的空盘的合法状态;调用方通过其它两棵树判断是否确实为空。
/// 存在但损坏的 catalog 必须计为问题,仍保留可读行以继续核查其它项目。
fn read_drive_catalog(
    root: &Path,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) -> BTreeMap<String, CatalogProject> {
    let path = paths::drive_catalog_path(root);
    let mut projects = BTreeMap::new();
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return projects,
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
        _ => {
            metadata_issue(
                "<本盘>",
                paths::DRIVE_CATALOG_FILE,
                VerifyIssueKind::ReadError,
                "本盘索引无法读取或不是普通文件(不跟随链接)。",
                reporter,
                report,
            );
            return projects;
        }
    }
    let mut rdr = match csv::Reader::from_path(&path) {
        Ok(rdr) => rdr,
        Err(e) => {
            metadata_issue(
                "<本盘>",
                paths::DRIVE_CATALOG_FILE,
                VerifyIssueKind::ReadError,
                &format!("读取本盘索引失败:{}", e),
                reporter,
                report,
            );
            return projects;
        }
    };
    let headers = match rdr.headers() {
        Ok(headers) => headers.clone(),
        Err(e) => {
            metadata_issue(
                "<本盘>",
                paths::DRIVE_CATALOG_FILE,
                VerifyIssueKind::ReadError,
                &format!("本盘索引表头损坏:{}", e),
                reporter,
                report,
            );
            return projects;
        }
    };
    if ["ProjectName", "FileCount", "TotalBytes"]
        .iter()
        .any(|column| headers.iter().filter(|h| h == column).count() > 1)
    {
        metadata_issue(
            "<本盘>",
            paths::DRIVE_CATALOG_FILE,
            VerifyIssueKind::ReadError,
            "本盘索引重复定义关键列,元数据不可信。",
            reporter,
            report,
        );
    }
    let name_col = headers.iter().position(|h| h == "ProjectName");
    let count_col = headers.iter().position(|h| h == "FileCount");
    let bytes_col = headers.iter().position(|h| h == "TotalBytes");
    if name_col.is_none() || count_col.is_none() || bytes_col.is_none() {
        metadata_issue(
            "<本盘>",
            paths::DRIVE_CATALOG_FILE,
            VerifyIssueKind::ReadError,
            "本盘索引缺少 ProjectName/FileCount/TotalBytes 列,元数据不可信。",
            reporter,
            report,
        );
    }
    let Some(name_col) = name_col else {
        return projects;
    };
    for (index, record) in rdr.records().enumerate() {
        let record = match record {
            Ok(record) => record,
            Err(e) => {
                metadata_issue(
                    "<本盘>",
                    paths::DRIVE_CATALOG_FILE,
                    VerifyIssueKind::ReadError,
                    &format!("本盘索引第{}行损坏:{}", index + 2, e),
                    reporter,
                    report,
                );
                continue;
            }
        };
        let name = record.get(name_col).unwrap_or("");
        if !valid_project_name(name) {
            metadata_issue(
                "<本盘>",
                paths::DRIVE_CATALOG_FILE,
                VerifyIssueKind::ReadError,
                &format!("本盘索引第{}行项目名无效。", index + 2),
                reporter,
                report,
            );
            continue;
        }
        let count = count_col
            .and_then(|c| record.get(c))
            .and_then(|v| v.trim().parse::<u64>().ok());
        let bytes = bytes_col
            .and_then(|c| record.get(c))
            .and_then(|v| v.trim().parse::<u64>().ok());
        if count.is_none() || bytes.is_none() {
            metadata_issue(
                name,
                paths::DRIVE_CATALOG_FILE,
                VerifyIssueKind::ReadError,
                &format!("本盘索引项目「{}」的文件数/字节数缺失或无效。", name),
                reporter,
                report,
            );
        }
        let key = extras_key(name);
        if projects.contains_key(&key) {
            metadata_issue(
                name,
                paths::DRIVE_CATALOG_FILE,
                VerifyIssueKind::ReadError,
                &format!("本盘索引重复登记项目「{}」。", name),
                reporter,
                report,
            );
            continue;
        }
        projects.insert(
            key,
            CatalogProject {
                name: name.to_string(),
                file_count: count,
                total_bytes: bytes,
            },
        );
    }
    projects
}

fn read_inventory_directory(
    dir: &Path,
    rel: &str,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) -> Option<fs::ReadDir> {
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            metadata_issue(
                "<本盘>",
                rel,
                VerifyIssueKind::Missing,
                &format!("复查目录缺失:{}", dir.display()),
                reporter,
                report,
            );
            return None;
        }
        _ => {
            metadata_issue(
                "<本盘>",
                rel,
                VerifyIssueKind::ReadError,
                &format!("复查目录不可读或不是普通目录(不跟随链接):{}", dir.display()),
                reporter,
                report,
            );
            return None;
        }
    }
    match fs::read_dir(dir) {
        Ok(entries) => Some(entries),
        Err(e) => {
            metadata_issue(
                "<本盘>",
                rel,
                VerifyIssueKind::EnumError,
                &format!("枚举复查目录失败:{}: {}", dir.display(), e),
                reporter,
                report,
            );
            None
        }
    }
}

fn enumerate_projects(
    root: &Path,
    inventory: &mut BTreeMap<String, ProjectInventory>,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) {
    let dir = paths::drive_projects_dir(root);
    let Some(entries) = read_inventory_directory(&dir, paths::DRIVE_PROJECTS_DIR, reporter, report)
    else {
        return;
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                metadata_issue(
                    "<本盘>",
                    paths::DRIVE_PROJECTS_DIR,
                    VerifyIssueKind::EnumError,
                    &format!("枚举项目失败:{}", e),
                    reporter,
                    report,
                );
                continue;
            }
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            metadata_issue(
                "<本盘>",
                paths::DRIVE_PROJECTS_DIR,
                VerifyIssueKind::ReadError,
                "项目名称不是有效 UTF-8,无法与索引和清单核对。",
                reporter,
                report,
            );
            continue;
        };
        // 与归档的 cruft 过滤一致;真实项目目录只按目录名单排除。
        if let Ok(ft) = entry.file_type() {
            if (ft.is_dir() && cruft::is_cruft_dir(&name))
                || (ft.is_file() && cruft::is_cruft_file(&name))
            {
                continue;
            }
        }
        if !valid_project_name(&name) {
            metadata_issue(
                "<本盘>",
                &name,
                VerifyIssueKind::ReadError,
                "项目目录条目名称无法安全核对。",
                reporter,
                report,
            );
            continue;
        }
        let project = inventory.entry(extras_key(&name)).or_default();
        if project.name.is_empty() {
            project.name = name;
        }
    }
}

fn enumerate_manifests(
    root: &Path,
    inventory: &mut BTreeMap<String, ProjectInventory>,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) {
    let dir = paths::drive_manifest_dir(root);
    let Some(entries) = read_inventory_directory(&dir, paths::DRIVE_MANIFEST_DIR, reporter, report)
    else {
        return;
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                metadata_issue(
                    "<本盘>",
                    paths::DRIVE_MANIFEST_DIR,
                    VerifyIssueKind::EnumError,
                    &format!("枚举清单失败:{}", e),
                    reporter,
                    report,
                );
                continue;
            }
        };
        let Some(filename) = entry.file_name().to_str().map(str::to_string) else {
            metadata_issue(
                "<本盘>",
                paths::DRIVE_MANIFEST_DIR,
                VerifyIssueKind::ReadError,
                "清单名称不是有效 UTF-8,无法核对。",
                reporter,
                report,
            );
            continue;
        };
        let folded = if cfg!(windows) {
            filename.to_lowercase()
        } else {
            filename.clone()
        };
        if !folded.ends_with(".sha256.csv") {
            if !cruft::is_cruft_file(&filename) {
                reporter.warn(&format!("清单目录存在未登记条目:{}", filename));
                report.push_extra(
                    "<本盘>",
                    &format!("{}\\{}", paths::DRIVE_MANIFEST_DIR, filename),
                );
            }
            continue;
        }
        let name = &filename[..filename.len() - ".sha256.csv".len()];
        if !valid_project_name(name) {
            metadata_issue(
                "<本盘>",
                &filename,
                VerifyIssueKind::ReadError,
                "清单文件名无法安全映射到项目。",
                reporter,
                report,
            );
            continue;
        }
        let project = inventory.entry(extras_key(name)).or_default();
        if project.name.is_empty() {
            project.name = name.to_string();
        }
    }
}

fn reconcile_project(
    project: &ProjectInventory,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) {
    if project.registered.is_none() {
        metadata_issue(
            &project.name,
            paths::DRIVE_CATALOG_FILE,
            VerifyIssueKind::Unverifiable,
            &format!(
                "项目「{}」未在本盘索引登记,项目目录/清单与索引不一致。",
                project.name
            ),
            reporter,
            report,
        );
    }
}

/// 项目与单文件抽查共用的只读内容核查。先校验父目录及清单类型,避免跟随目录/清单链接。
#[allow(clippy::too_many_arguments)]
fn verify_project_content(
    proj_dir: &Path,
    manifest: &Path,
    project: &str,
    registered: Option<&CatalogProject>,
    selected_rel: Option<&str>,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) {
    if cancelled(cancel, report) {
        return;
    }
    // 检查两棵树的根目录,即使项目缺失/清单目录被改成链接也不会读取树外数据。
    let parents_ok = [proj_dir.parent(), manifest.parent()]
        .into_iter()
        .flatten()
        .all(|parent| {
            fs::symlink_metadata(parent)
                .map(|m| m.is_dir() && !m.file_type().is_symlink())
                .unwrap_or(false)
        });
    if !parents_ok {
        metadata_issue(
            project,
            "",
            VerifyIssueKind::Unverifiable,
            &format!(
                "项目「{}」的项目/清单根目录缺失或不是普通目录,无法校验。",
                project
            ),
            reporter,
            report,
        );
        return;
    }
    let project_ok = match fs::symlink_metadata(proj_dir) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            metadata_issue(
                project,
                "",
                VerifyIssueKind::Missing,
                &format!("项目目录缺失:{}", proj_dir.display()),
                reporter,
                report,
            );
            false
        }
        _ => {
            metadata_issue(
                project,
                "",
                VerifyIssueKind::ReadError,
                &format!(
                    "项目目录不是可读普通目录(不跟随链接):{}",
                    proj_dir.display()
                ),
                reporter,
                report,
            );
            false
        }
    };
    match fs::symlink_metadata(manifest) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            metadata_issue(
                project,
                &format!("{}.sha256.csv", project),
                VerifyIssueKind::Unverifiable,
                &format!("项目「{}」缺少校验清单,备份内容不可验证。", project),
                reporter,
                report,
            );
            return;
        }
        _ => {
            metadata_issue(
                project,
                &format!("{}.sha256.csv", project),
                VerifyIssueKind::ReadError,
                &format!(
                    "校验清单无法读取或不是普通文件(不跟随链接):{}",
                    manifest.display()
                ),
                reporter,
                report,
            );
            return;
        }
    }
    let rows = match read_manifest_rows(manifest, reporter) {
        Ok(rows) => rows,
        Err(e) => {
            metadata_issue(
                project,
                &format!("{}.sha256.csv", project),
                VerifyIssueKind::ReadError,
                &format!("清单有问题:{} —— {}", project, e),
                reporter,
                report,
            );
            return;
        }
    };
    if let Some(registered) = registered {
        let bytes = rows
            .iter()
            .try_fold(0u64, |sum, (_, size, _)| sum.checked_add((*size)?));
        if registered
            .file_count
            .is_some_and(|count| count != rows.len() as u64)
            || (registered.total_bytes.is_some()
                && bytes.is_some()
                && registered.total_bytes != bytes)
        {
            metadata_issue(
                project,
                &format!("{}.sha256.csv", project),
                VerifyIssueKind::Unverifiable,
                &format!(
                    "项目「{}」的清单文件数/字节数与本盘索引不一致,元数据不可验证。",
                    project
                ),
                reporter,
                report,
            );
        }
    }
    if !project_ok {
        return;
    }
    if let Some(rel) = selected_rel {
        let key = extras_key(rel);
        match rows.iter().find(|(row, _, _)| extras_key(row) == key) {
            Some((row, size, hash)) => {
                check_one_file(proj_dir, project, row, *size, hash, reporter, report)
            }
            None => {
                let mut path = proj_dir.to_path_buf();
                let segments = rel.split('\\').collect::<Vec<_>>();
                for (index, segment) in segments.iter().enumerate() {
                    path.push(segment);
                    let is_leaf = index + 1 == segments.len();
                    match fs::symlink_metadata(&path) {
                        Ok(meta)
                            if !meta.file_type().is_symlink()
                                && ((is_leaf && meta.is_file()) || (!is_leaf && meta.is_dir())) => {
                        }
                        Ok(_) => {
                            metadata_issue(
                                project,
                                rel,
                                VerifyIssueKind::Unverifiable,
                                "抽查路径不是普通文件/目录(不跟随链接),内容不可验证。",
                                reporter,
                                report,
                            );
                            return;
                        }
                        Err(e) => {
                            let kind = if e.kind() == std::io::ErrorKind::NotFound {
                                VerifyIssueKind::Missing
                            } else {
                                VerifyIssueKind::ReadError
                            };
                            metadata_issue(
                                project,
                                rel,
                                kind,
                                &format!("抽查路径读取失败:{}", e),
                                reporter,
                                report,
                            );
                            return;
                        }
                    }
                }
                reporter.warn(&format!("文件不在项目清单中,无法比对哈希:{}", rel));
                report.push_extra(project, rel);
            }
        }
    } else {
        let mut expected = HashSet::new();
        for (rel, size, hash) in &rows {
            if cancelled(cancel, report) {
                return;
            }
            expected.insert(extras_key(rel));
            check_one_file(proj_dir, project, rel, *size, hash, reporter, report);
        }
        report_extras(proj_dir, project, &expected, reporter, report);
    }
}

/// 旧内容级单测的私有适配器;真实全盘入口始终核对本盘索引。
#[cfg(test)]
fn verify_tree(
    mdir: &Path,
    projects_dir: &Path,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();
    let mut manifests = fs::read_dir(mdir)?.collect::<std::io::Result<Vec<_>>>()?;
    manifests.sort_by_key(|entry| entry.file_name());
    for manifest in manifests {
        if cancelled(cancel, &mut report) {
            break;
        }
        let filename = manifest.file_name();
        let filename = filename.to_string_lossy();
        if let Some(project) = filename.strip_suffix(".sha256.csv") {
            verify_project_content(
                &projects_dir.join(project),
                &manifest.path(),
                project,
                None,
                None,
                cancel,
                reporter,
                &mut report,
            );
        }
    }
    Ok(report)
}

/// 复查单个目标:一个项目文件夹(选的是 `项目\<proj>`)或项目内单个文件(更深的路径)。
/// 自动定位所在备份盘 + 项目,读该项目清单比对。**对盘只读**;不更新 last_verify
/// (这是局部抽查,非整盘复查,记录全盘 outcome 会误导)。(Spec D §4.4 精神)
pub fn verify_one(
    _cfg: &Config,
    reporter: &dyn Reporter,
    target: &Path,
    cancel: &AtomicBool,
) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();
    let (root, project, rel) = locate_backup_target(target)?;
    if !valid_project_name(&project) {
        bail!("项目名称不能包含路径跳转或分隔符。");
    }
    validate_drive_root(&root)?;
    if cancelled(cancel, &mut report) {
        return Ok(report);
    }
    let catalog = read_drive_catalog(&root, reporter, &mut report);
    let registered = catalog.get(&extras_key(&project));
    let inventory = ProjectInventory {
        name: project.clone(),
        registered: registered.cloned(),
    };
    reconcile_project(&inventory, reporter, &mut report);
    if let Some(rel) = &rel {
        validate_manifest_rel(target, rel)?;
    }
    let manifest = paths::drive_manifest_dir(&root).join(format!("{}.sha256.csv", project));
    let proj_dir = paths::drive_projects_dir(&root).join(&project);
    reporter.info(&format!("复查项目「{}」(重算 SHA256 比对清单)…", project));
    verify_project_content(
        &proj_dir,
        &manifest,
        &project,
        registered,
        rel.as_deref(),
        cancel,
        reporter,
        &mut report,
    );
    if report.cancelled {
        reporter.warn("复查已取消。");
        return Ok(report);
    }

    let summary = format!(
        "单目标复查完成:检查 {} 个文件,完整性问题 {} 个,清单外 {} 个,仅大小校验 {} 个。",
        report.checked, report.bad, report.extra, report.size_only
    );
    if report.bad > 0 {
        reporter.error(&summary);
    } else if report.extra > 0 || report.size_only > 0 {
        reporter.warn(&summary);
    } else if report.checked == 0 {
        reporter.info("没有可校验文件,未验证任何备份内容。");
        reporter.info(&summary);
    } else {
        reporter.ok(&summary);
    }
    if report.size_only > 0 {
        reporter.warn("注意:含未验证内容(仅大小校验)的文件——清单无哈希,等长篡改无法被发现。");
    }
    Ok(report)
}

/// 校验单个文件 vs 清单期望(缺失/读失败/不可校验/大小/SHA),结果记进 report。
/// verify_drive(整盘)与 verify_one(单目标)共用,保证两条路径判定一致。
fn check_one_file(
    proj_dir: &Path,
    project_name: &str,
    rel: &str,
    size: Option<u64>,
    hash: &str,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) {
    // rel 已在 read_manifest_rows 归一化为 `\` 分隔。proj_dir.join(rel) 在 Windows 上正确(\ 是分隔符),
    // 但在非 Windows(Linux/CI)上 `\` 是合法文件名字符 → 整个 "sub\a.txt" 被当单一文件名,嵌套子目录
    // 文件被误报 Missing,使 CI 对嵌套损坏结构性盲区。显式按 `\` 拆段 fold-join,跨平台一致(Windows 上
    // "sub\a.txt".split('\\')=["sub","a.txt"],fold-join 与原 join 等价)。各段已由 validate_manifest_rel
    // 保证不含空段/`.`/`..`/`:`。(强优化 review)
    report.checked += 1;
    let mut f = proj_dir.to_path_buf();
    let segments = rel.split('\\').collect::<Vec<_>>();
    // 不只检查文件叶子:登记路径的任一父目录变成链接也不能顺着它读项目外数据。
    for segment in &segments[..segments.len().saturating_sub(1)] {
        f.push(segment);
        match fs::symlink_metadata(&f) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            Ok(_) => {
                reporter.error(&format!("  文件父路径不是普通目录(不跟随链接): {}", rel));
                report.push_issue(project_name, rel, VerifyIssueKind::Corrupt);
                return;
            }
            Err(e) => {
                let kind = if e.kind() == std::io::ErrorKind::NotFound {
                    VerifyIssueKind::Missing
                } else {
                    VerifyIssueKind::ReadError
                };
                reporter.error(&format!("  文件父目录读取失败 {}: {}", rel, e));
                report.push_issue(project_name, rel, kind);
                return;
            }
        }
    }
    f.push(segments.last().copied().unwrap_or(""));
    let meta = match fs::symlink_metadata(&f) {
        Ok(meta) => meta,
        Err(e) => {
            let kind = if e.kind() == std::io::ErrorKind::NotFound {
                VerifyIssueKind::Missing
            } else {
                VerifyIssueKind::ReadError
            };
            reporter.error(&format!("  文件读取失败 {}: {}", rel, e));
            report.push_issue(project_name, rel, kind);
            return;
        }
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        reporter.error(&format!(
            "  已被替换为链接、非当初备份的真实文件(不跟随): {}",
            rel
        ));
        report.push_issue(project_name, rel, VerifyIssueKind::Corrupt);
        return;
    }
    if !ft.is_file() {
        reporter.error(&format!("  缺失(该路径已不是普通文件): {}", rel));
        report.push_issue(project_name, rel, VerifyIssueKind::Missing);
        return;
    }
    // 既无 Size 也无 Hash → 无任何可校验属性 → fail-closed。(Phase 4 F-01/F-6)
    // 注意条件是「两者皆缺」:no_hash 归档模式(有 Size、Hash="")**不**触发 Unverifiable,
    // 因为还能靠 Size 精确比对(下面 if let Some(sz) 分支),不算不可校验。(V-04)
    if size.is_none() && hash.is_empty() {
        reporter.error(&format!("  清单项缺 Size 且缺 Hash,无法校验: {}", rel));
        report.push_issue(project_name, rel, VerifyIssueKind::Unverifiable);
        return;
    }
    if let Some(sz) = size {
        if meta.len() != sz {
            reporter.error(&format!("  大小不一致: {}", rel));
            report.push_issue(project_name, rel, VerifyIssueKind::SizeMismatch);
            // 大小已经不符,内容必然不一致,再算 SHA256 只是浪费 IO/CPU。
            // 一条问题一种 kind:这里报 SizeMismatch 即够,early-return 跳过哈希。(V-07)
            return;
        }
    }
    if !hash.is_empty() {
        match sha256_hex(&f) {
            // 大小写不敏感:本工具写大写十六进制(见 sha256_hex),但旧版本/其他工具
            // 可能写小写哈希。eq_ignore_ascii_case 兼容历史小写清单,避免误报 Corrupt。(V-08)
            Ok(h) if h.eq_ignore_ascii_case(hash) => {}
            Ok(_) => {
                reporter.error(&format!("  损坏/不一致: {}", rel));
                report.push_issue(project_name, rel, VerifyIssueKind::Corrupt);
            }
            Err(e) => {
                reporter.error(&format!("  读取失败 {}: {}", rel, e));
                report.push_issue(project_name, rel, VerifyIssueKind::ReadError);
            }
        }
    } else {
        // 走到这里:Size 校验已通过(或无 Size 但有……不可能,因上面 fail-closed 已拦两者皆缺),
        // 但 Hash 为空 → 本文件**只做了大小校验、未验证内容**。计数以便整盘结论不笼统标「完好」。(review-r3 round4)
        report.size_only += 1;
    }
}

/// extras 比对用的归一化 key。Windows 文件系统大小写不敏感:折叠大小写,避免
/// 「清单登记 A.txt、磁盘实为 a.txt」时同一文件既被 check_one_file 校验通过、
/// 又被 report_extras 误报为多余的自相矛盾。非 Windows(大小写敏感)保持原样,
/// 以免把本应区分的 A.txt / a.txt 误并。(review-r3)
fn extras_key(rel: &str) -> String {
    if cfg!(windows) {
        rel.to_lowercase()
    } else {
        rel.to_string()
    }
}

/// 报告项目目录里清单外的多余文件(不删除)。
fn report_extras(
    proj_dir: &Path,
    project_name: &str,
    expected: &HashSet<String>,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) {
    if !proj_dir.is_dir() {
        return;
    }
    // 同 L-044:不 canonicalize,否则 `\\?\` 前缀与 walk 路径失配 → extra 全部误报。
    // cruft::walk 已过滤 .bftool-part 等临时残留:归档中断留下的 .part **不**报为 extra
    // 是有意的 —— 那是续传/下次归档要清理的中间产物,不是用户该关心的「清单外多余文件」。(V-05)
    for entry in cruft::walk(proj_dir) {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                reporter.error(&format!("  枚举失败: {}", err));
                report.push_issue(project_name, "", VerifyIssueKind::EnumError);
                continue;
            }
        };
        let ft = entry.file_type();
        if ft.is_symlink() {
            // 与归档侧一致:不跟随链接;但不静默 —— 备份盘里出现链接值得提示。登记文件被替换成链接的情形
            // 已由 check_one_file 报损坏,此处只对**清单外**的链接告警,避免与之重复。(review-r3 round3)
            let rel = entry
                .path()
                .strip_prefix(proj_dir)
                .map(|p| p.to_string_lossy().replace('/', "\\"))
                .unwrap_or_default();
            if !expected.contains(&extras_key(&rel)) {
                reporter.error(&format!(
                    "  发现符号链接/junction(不跟随、内容不可验证): {}",
                    rel
                ));
                report.push_issue(project_name, &rel, VerifyIssueKind::Unverifiable);
            }
            continue;
        }
        if !ft.is_file() {
            if !ft.is_dir() {
                let rel = entry
                    .path()
                    .strip_prefix(proj_dir)
                    .map(|p| p.to_string_lossy().replace('/', "\\"))
                    .unwrap_or_default();
                reporter.error(&format!("  项目中存在非普通文件,不可验证: {}", rel));
                report.push_issue(project_name, &rel, VerifyIssueKind::Unverifiable);
            }
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(proj_dir)
            .map(|p| p.to_string_lossy().replace('/', "\\"))
            .unwrap_or_default();
        if !expected.contains(&extras_key(&rel)) {
            reporter.warn(&format!("  多余(清单外): {}", rel));
            report.push_extra(project_name, &rel);
        }
    }
}

/// 读清单为 (Rel, Size?, Hash) 行;跳过 cruft 段;缺 Rel 列/读失败 → Err(由调用方决定跳过还是报错)。
/// size 为 Option:列缺失/不可解析 = 未知(不校验大小);存在(含 0)就精确比对。(ledger L-013)
fn read_manifest_rows(
    path: &Path,
    reporter: &dyn Reporter,
) -> Result<Vec<(String, Option<u64>, String)>> {
    let mut rdr = csv::Reader::from_path(path)
        .with_context(|| format!("读校验清单失败：{}", path.display()))?;
    let headers = rdr
        .headers()
        .with_context(|| format!("读校验清单表头失败：{}", path.display()))?
        .clone();
    if ["Rel", "Size", "Hash"]
        .iter()
        .any(|column| headers.iter().filter(|h| h == column).count() > 1)
    {
        bail!("清单重复定义关键列:{}", path.display());
    }
    let i_rel = headers.iter().position(|h| h == "Rel");
    let i_size = headers.iter().position(|h| h == "Size");
    let i_hash = headers.iter().position(|h| h == "Hash");
    let Some(i_rel) = i_rel else {
        bail!("清单缺少 Rel 列：{}", path.display());
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (idx, rec) in rdr.records().enumerate() {
        let rec =
            rec.with_context(|| format!("校验清单第 {} 行格式错误：{}", idx + 2, path.display()))?;
        let rel = rec.get(i_rel).unwrap_or("").to_string();
        // 跳过空 Rel 行:proj_dir.join("") == proj_dir 本身,会把项目目录当文件校验
        // 而误报 Missing(目录不是 is_file())。空 Rel 是脏数据,直接丢弃。(V-09)
        if rel.is_empty() {
            if rec.iter().any(|cell| !cell.trim().is_empty()) {
                bail!("清单第{}行缺 Rel 但含其它数据:{}", idx + 2, path.display());
            }
            continue;
        }
        validate_manifest_rel(path, &rel)?;
        if cruft::rel_has_cruft_component(&rel) {
            continue;
        }
        // trim 单元格:外部/Excel 另存的清单常带尾随空白。不 trim 会让 (a) 含尾随空格的 Hash
        // 与重算值 eq_ignore_ascii_case 永不相等 → 完好文件误报 Corrupt;(b) 空白 Hash " " 因
        // is_empty()==false 绕过 size+hash 皆缺的 Unverifiable fail-closed;(c) Size "5 " parse 失败
        // 静默 None → 跳过大小校验。与 verify_state::parse_count 已 trim 的口径一致。(强优化 review)
        let size = i_size
            .and_then(|c| rec.get(c))
            .and_then(|s| s.trim().parse::<u64>().ok());
        let hash = i_hash
            .and_then(|c| rec.get(c))
            .map(|s| s.trim())
            .unwrap_or("")
            .to_string();
        // 归一化分隔符为 `\`,与 report_extras 产出的 rel 及 check_one_file 的 join 口径一致;
        // 否则正斜杠清单(旧版/外部工具写的 "sub/a.txt")会让已登记且已校验的文件被
        // report_extras 误报为「清单外多余」。本工具自写清单已是 `\`,此处只兜底外来清单。(review-r3)
        let rel = rel.replace('/', "\\");
        if !seen.insert(extras_key(&rel)) {
            bail!("清单重复登记同一文件:{} ({})", rel, path.display());
        }
        out.push((rel, size, hash));
    }
    // 出现「无哈希」条目就告警(覆盖三种来源:缺 Hash 列 / no_hash 归档写出的空单元格 / 外部裁空)。
    // 旧实现只在「整列缺失(i_hash.is_none())」时 warn,漏掉了本工具 --unsafe-no-hash 自产清单
    // (Hash 列在、单元格全空)这一最常见情形 → 那种情况下整盘 size-only 校验却零提示。(review-r3 round4)
    if out.iter().any(|(_, _, h)| h.is_empty()) {
        reporter.warn(&format!(
            "清单有未含哈希的条目,这些文件仅按大小校验、无法检测等长内容篡改:{}",
            path.display()
        ));
    }
    Ok(out)
}

fn validate_manifest_rel(manifest_path: &Path, rel: &str) -> Result<()> {
    let p = Path::new(rel);
    if p.is_absolute() {
        bail!(
            "清单 Rel 不能是绝对路径：{} ({})",
            rel,
            manifest_path.display()
        );
    }
    for comp in p.components() {
        match comp {
            Component::Normal(_) => {}
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                bail!(
                    "清单 Rel 含非法路径段,可能逃逸项目目录：{} ({})",
                    rel,
                    manifest_path.display()
                );
            }
        }
    }
    for segment in rel.split(['\\', '/']) {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains(':') {
            bail!(
                "清单 Rel 含非法路径段,可能逃逸项目目录：{} ({})",
                rel,
                manifest_path.display()
            );
        }
    }
    Ok(())
}

/// 从用户选的路径定位:所在备份盘根 + 项目名 + (若选的是文件)项目内相对路径(None=整个项目文件夹)。
/// 备份盘根 = 含 `本盘信息\本盘编号.txt` 的祖先目录;目标须在该盘 `项目\` 下。
fn locate_backup_target(target: &Path) -> Result<(PathBuf, String, Option<String>)> {
    // ancestors() 从近到远(self → parent → … → 盘根)。取**第一个**含 本盘信息\本盘编号.txt
    // 的祖先即「目标所在盘的盘根」:盘根标识只在盘根一层存在,从近到远第一个命中就是它,
    // 不会被更外层(理论上不该有)的同名标识抢走。语义正确。(V-06)
    let root = target
        .ancestors()
        .find(|a| paths::drive_id_path(a).is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "选中的路径不在某块已初始化备份盘内(找不到 {}\\{}):{}",
                paths::DRIVE_INFO_DIR,
                paths::DRIVE_ID_FILE,
                target.display()
            )
        })?
        .to_path_buf();
    let projects_dir = paths::drive_projects_dir(&root);
    let rest = target.strip_prefix(&projects_dir).map_err(|_| {
        anyhow::anyhow!(
            "请在备份盘的「{}」目录下选择项目文件夹或其中的文件:{}",
            paths::DRIVE_PROJECTS_DIR,
            target.display()
        )
    })?;
    let mut comps = rest.components();
    let project = comps
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "请选择一个具体的项目文件夹(或其中的文件),而不是「{}」根目录",
                paths::DRIVE_PROJECTS_DIR
            )
        })?;
    let remainder = comps.as_path();
    let rel = if remainder.as_os_str().is_empty() {
        None
    } else {
        Some(remainder.to_string_lossy().replace('/', "\\"))
    };
    Ok((root, project, rel))
}

fn sha256_hex(path: &Path) -> Result<String> {
    let f = std::fs::File::open(path)?;
    let mut reader = BufReader::with_capacity(1024 * 1024, f);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        use std::fmt::Write as _;
        write!(&mut s, "{:02X}", b).ok();
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporter::NoopReporter;
    use std::io::Write as _;
    use std::path::PathBuf;

    fn abool() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn sha_of(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        let out = h.finalize();
        let mut s = String::new();
        for b in out {
            use std::fmt::Write as _;
            write!(&mut s, "{:02X}", b).ok();
        }
        s
    }

    /// 在 dir 下铺一个最小"盘":校验清单 m/proj.sha256.csv + 项目 p/proj/<files>。
    /// projects_dir 用**非 canonical** 临时路径(L-044 修复前会让 extra 误报的场景)。
    fn setup(
        dir: &Path,
        rows: &[(&str, u64, &str)],
        files: &[(&str, &[u8])],
    ) -> (PathBuf, PathBuf) {
        let mdir = dir.join("m");
        let pdir = dir.join("p");
        let proj = pdir.join("proj");
        fs::create_dir_all(&mdir).unwrap();
        fs::create_dir_all(&proj).unwrap();
        let mut csv = String::from("Rel,Size,Hash\n");
        for (rel, size, hash) in rows {
            csv.push_str(&format!("{},{},{}\n", rel, size, hash));
        }
        fs::write(mdir.join("proj.sha256.csv"), csv).unwrap();
        for (rel, bytes) in files {
            let p = proj.join(rel);
            if let Some(par) = p.parent() {
                fs::create_dir_all(par).unwrap();
            }
            let mut f = fs::File::create(&p).unwrap();
            f.write_all(bytes).unwrap();
        }
        (mdir, pdir)
    }

    #[test]
    fn verify_tree_clean_reports_no_bad() {
        let d = tempfile::tempdir().unwrap();
        let content = b"hello";
        let (mdir, pdir) = setup(
            d.path(),
            &[("a.txt", 5, &sha_of(content))],
            &[("a.txt", content)],
        );
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0);
        assert_eq!(r.checked, 1);
        assert_eq!(r.extra, 0);
    }

    #[test]
    fn verify_tree_detects_missing_file() {
        let d = tempfile::tempdir().unwrap();
        let (mdir, pdir) = setup(d.path(), &[("a.txt", 5, &sha_of(b"hello"))], &[]);
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 1);
        assert!(r.has_corruption());
    }

    #[test]
    fn verify_tree_detects_size_mismatch() {
        let d = tempfile::tempdir().unwrap();
        let (mdir, pdir) = setup(d.path(), &[("a.txt", 5, "")], &[("a.txt", b"abc")]);
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 1);
    }

    #[test]
    fn verify_tree_detects_content_change_same_size() {
        let d = tempfile::tempdir().unwrap();
        let (mdir, pdir) = setup(
            d.path(),
            &[("a.txt", 5, &sha_of(b"hello"))],
            &[("a.txt", b"world")],
        );
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 1, "等长内容篡改必须靠 SHA 抓出");
    }

    // ── L-013: 清单记 size=0 不能跳过大小校验 ──
    #[test]
    fn verify_tree_detects_tampered_zero_byte_file() {
        let d = tempfile::tempdir().unwrap();
        let (mdir, pdir) = setup(d.path(), &[("z.txt", 0, "")], &[("z.txt", b"surprise")]);
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 1, "清单记 0 字节、实际非 0 必须报大小不一致");
    }

    #[test]
    fn verify_tree_real_zero_byte_file_is_ok() {
        let d = tempfile::tempdir().unwrap();
        let (mdir, pdir) = setup(d.path(), &[("z.txt", 0, "")], &[("z.txt", b"")]);
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "真实 0 字节文件应通过");
    }

    // ── review-r3 round4:无哈希(no_hash 归档 / 缺 Hash 列 / 单元格被裁空)的行只做大小校验,
    // 计入 size_only,不报损坏,但整盘结论不应笼统当「完好」(由 size_only>0 体现)──
    #[test]
    fn verify_tree_counts_size_only_when_hash_empty() {
        let d = tempfile::tempdir().unwrap();
        let content = b"hello";
        // Hash 字段为空(模拟 --unsafe-no-hash 自产清单 / 被裁空单元格),Size 仍在。
        let (mdir, pdir) = setup(d.path(), &[("a.txt", 5, "")], &[("a.txt", content)]);
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "大小一致、无哈希 → 不报损坏");
        assert_eq!(r.checked, 1);
        assert_eq!(
            r.size_only, 1,
            "无哈希行应计入 size_only(仅大小校验、未验证内容)"
        );
    }

    // ── 强优化:清单整列缺 Hash(只有 Rel,Size,连 Hash 列都没有)→ size-only,不报损坏 ──
    // setup() 硬编码三列表头,造不出『整列缺失』;手写两列清单覆盖 i_hash.is_none() 分支,
    // 防止一块全 no-hash 内容的盘因解析回归被误绿标。(强优化 review)
    #[test]
    fn verify_tree_whole_hash_column_missing_is_size_only() {
        let d = tempfile::tempdir().unwrap();
        let mdir = d.path().join("m");
        let proj = d.path().join("p").join("proj");
        fs::create_dir_all(&mdir).unwrap();
        fs::create_dir_all(&proj).unwrap();
        // 只有两列、没有 Hash 列(模拟外部/旧版清单)。
        fs::write(mdir.join("proj.sha256.csv"), "Rel,Size\na.txt,5\n").unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        let r = verify_tree(&mdir, &d.path().join("p"), &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "大小一致、整列无哈希 → 不报损坏");
        assert_eq!(r.checked, 1);
        assert_eq!(r.size_only, 1, "整列缺 Hash 的行应计入 size_only");
        assert_eq!(
            r.outcome(),
            VerifyOutcome::CleanButSizeOnly { size_only: 1 },
            "结论应为『仅大小校验』而非 Clean"
        );
    }

    // ── 强优化:嵌套子目录文件须跨平台按 `\` 拆段定位,不被误报 Missing(CI/Linux 盲区) ──
    // 不走 setup()(它用 proj.join(rel) 建文件,在非 Windows 上 "sub\a.txt" 会建成单一文件名)。
    #[test]
    fn verify_tree_nested_subdir_file_checked_cross_platform() {
        let d = tempfile::tempdir().unwrap();
        let mdir = d.path().join("m");
        let proj = d.path().join("p").join("proj");
        fs::create_dir_all(&mdir).unwrap();
        fs::create_dir_all(proj.join("sub")).unwrap();
        let content = b"hello";
        fs::write(proj.join("sub").join("a.txt"), content).unwrap();
        // 清单用本工具自写的 `\` 分隔。check_one_file 必须跨平台拆段,否则非 Windows 误报 Missing。
        fs::write(
            mdir.join("proj.sha256.csv"),
            format!("Rel,Size,Hash\nsub\\a.txt,5,{}\n", sha_of(content)),
        )
        .unwrap();
        let r = verify_tree(&mdir, &d.path().join("p"), &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "嵌套子目录文件应被定位校验,不应误报 Missing");
        assert_eq!(r.checked, 1);
    }

    // ── 强优化:Hash/Size 单元格的尾随空白须 trim,完好文件不被误报 Corrupt ──
    #[test]
    fn verify_tree_trims_whitespace_in_hash_and_size_cells() {
        let d = tempfile::tempdir().unwrap();
        let mdir = d.path().join("m");
        let proj = d.path().join("p").join("proj");
        fs::create_dir_all(&mdir).unwrap();
        fs::create_dir_all(&proj).unwrap();
        let content = b"hello";
        fs::write(proj.join("a.txt"), content).unwrap();
        // Hash 带尾随空格、Size 带尾随空格(模拟 Excel 另存)。
        fs::write(
            mdir.join("proj.sha256.csv"),
            format!("Rel,Size,Hash\na.txt,5 ,{} \n", sha_of(content)),
        )
        .unwrap();
        let r = verify_tree(&mdir, &d.path().join("p"), &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "尾随空白应被 trim,完好文件不应误报 Corrupt");
        assert_eq!(r.checked, 1);
        assert_eq!(
            r.size_only, 0,
            "Size trim 后应解析成功并精确比对(非 size_only)"
        );
    }

    // ── Phase 4 F-01/F-6: 清单缺 Size 且缺 Hash → fail-closed ──
    #[test]
    fn verify_tree_unverifiable_row_is_bad() {
        let d = tempfile::tempdir().unwrap();
        let mdir = d.path().join("m");
        let proj = d.path().join("p").join("proj");
        fs::create_dir_all(&mdir).unwrap();
        fs::create_dir_all(&proj).unwrap();
        fs::write(mdir.join("proj.sha256.csv"), "Rel,Size,Hash\nz.txt,abc,\n").unwrap();
        fs::write(proj.join("z.txt"), b"whatever").unwrap();
        let r = verify_tree(&mdir, &d.path().join("p"), &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 1, "Size 不可解析 + 无 Hash 应判 bad(fail-closed)");
    }

    #[test]
    fn verify_tree_malformed_manifest_row_is_bad() {
        let d = tempfile::tempdir().unwrap();
        let mdir = d.path().join("m");
        let proj = d.path().join("p").join("proj");
        fs::create_dir_all(&mdir).unwrap();
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        fs::write(
            mdir.join("proj.sha256.csv"),
            format!(
                "Rel,Size,Hash\n\
                 a.txt,5,{}\n\
                 missing.bin,1\n",
                sha_of(b"hello")
            ),
        )
        .unwrap();

        let r = verify_tree(&mdir, &d.path().join("p"), &abool(), &NoopReporter).unwrap();

        assert_eq!(r.bad, 1, "malformed manifest rows must fail closed");
        assert!(matches!(r.issues[0].kind, VerifyIssueKind::ReadError));
        assert_eq!(r.issues[0].project, "proj");
    }

    #[test]
    fn verify_tree_rejects_manifest_parent_traversal_rel() {
        let d = tempfile::tempdir().unwrap();
        let mdir = d.path().join("m");
        let projects = d.path().join("p");
        let proj = projects.join("proj");
        fs::create_dir_all(&mdir).unwrap();
        fs::create_dir_all(&proj).unwrap();
        fs::write(projects.join("outside.txt"), b"safe").unwrap();
        fs::write(
            mdir.join("proj.sha256.csv"),
            format!("Rel,Size,Hash\n..\\outside.txt,4,{}\n", sha_of(b"safe")),
        )
        .unwrap();

        let r = verify_tree(&mdir, &projects, &abool(), &NoopReporter).unwrap();

        assert_eq!(r.bad, 1, "manifest Rel must not escape project dir");
        assert!(matches!(r.issues[0].kind, VerifyIssueKind::ReadError));
    }

    #[test]
    fn verify_tree_rejects_manifest_absolute_rel() {
        let d = tempfile::tempdir().unwrap();
        let mdir = d.path().join("m");
        let projects = d.path().join("p");
        let proj = projects.join("proj");
        fs::create_dir_all(&mdir).unwrap();
        fs::create_dir_all(&proj).unwrap();
        let outside = d.path().join("outside.txt");
        fs::write(&outside, b"safe").unwrap();
        fs::write(
            mdir.join("proj.sha256.csv"),
            format!(
                "Rel,Size,Hash\n{},4,{}\n",
                outside.display(),
                sha_of(b"safe")
            ),
        )
        .unwrap();

        let r = verify_tree(&mdir, &projects, &abool(), &NoopReporter).unwrap();

        assert_eq!(r.bad, 1, "manifest Rel must not be absolute");
        assert!(matches!(r.issues[0].kind, VerifyIssueKind::ReadError));
    }

    // ── V-08: 历史小写哈希应大小写不敏感比对,不误报 Corrupt ──
    #[test]
    fn verify_tree_lowercase_hash_matches() {
        let d = tempfile::tempdir().unwrap();
        let content = b"hello";
        let lower = sha_of(content).to_lowercase();
        let (mdir, pdir) = setup(d.path(), &[("a.txt", 5, &lower)], &[("a.txt", content)]);
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "小写哈希(旧版本/他工具)应兼容,不报损坏");
        assert_eq!(r.checked, 1);
    }

    // ── V-09: 空 Rel 行被跳过,不会把项目目录当文件误报 Missing ──
    #[test]
    fn verify_tree_skips_empty_rel_row() {
        let d = tempfile::tempdir().unwrap();
        let mdir = d.path().join("m");
        let proj = d.path().join("p").join("proj");
        fs::create_dir_all(&mdir).unwrap();
        fs::create_dir_all(&proj).unwrap();
        // 第二行 Rel 为空:旧逻辑会 join("")==proj_dir,目录非 is_file() → 误报 Missing。
        fs::write(
            mdir.join("proj.sha256.csv"),
            "Rel,Size,Hash\na.txt,5,\n,,\n",
        )
        .unwrap();
        fs::write(proj.join("a.txt"), b"hello").unwrap();
        let r = verify_tree(&mdir, &d.path().join("p"), &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "空 Rel 行应被跳过,不产生误导性 Missing");
        assert_eq!(r.checked, 1, "只校验有效的 a.txt 一行");
    }

    #[test]
    fn verify_tree_counts_extra_files() {
        let d = tempfile::tempdir().unwrap();
        let content = b"hello";
        let (mdir, pdir) = setup(
            d.path(),
            &[("a.txt", 5, &sha_of(content))],
            &[("a.txt", content), ("extra.txt", b"x")],
        );
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0);
        assert_eq!(r.extra, 1);
    }

    // ── review-r3 #5:正斜杠清单(旧版/外部工具)不得把已登记文件误报为「清单外多余」──
    #[cfg(windows)]
    #[test]
    fn verify_tree_forward_slash_manifest_no_false_extra() {
        let d = tempfile::tempdir().unwrap();
        let content = b"hello";
        // 清单 Rel 用正斜杠 "sub/a.txt";文件铺在 sub\a.txt(同一文件)。
        let (mdir, pdir) = setup(
            d.path(),
            &[("sub/a.txt", 5, &sha_of(content))],
            &[("sub\\a.txt", content)],
        );
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "正斜杠清单项应被正确校验(分隔符归一化)");
        assert_eq!(r.checked, 1);
        assert_eq!(r.extra, 0, "已登记的正斜杠文件不得被误报为多余");
    }

    // ── review-r3 #6:Windows 大小写不敏感 —— 清单与磁盘大小写不同的同一文件不得误报多余 ──
    #[cfg(windows)]
    #[test]
    fn verify_tree_case_insensitive_no_false_extra() {
        let d = tempfile::tempdir().unwrap();
        let content = b"hello";
        // 清单登记 "A.txt",磁盘实际 "a.txt"(同一文件,Windows 大小写折叠)。
        let (mdir, pdir) = setup(
            d.path(),
            &[("A.txt", 5, &sha_of(content))],
            &[("a.txt", content)],
        );
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "大小写不同的同一文件应被校验通过");
        assert_eq!(r.extra, 0, "同一文件不得既校验通过又被报多余(大小写折叠)");
    }

    // ── review-r3 round3:登记文件事后被替换成符号链接(指向外部同内容文件)→ 不跟随、判 bad,
    // 而非顺链接读目标判「完好」(与归档侧 follow_links=false 对齐)──
    #[test]
    fn verify_tree_flags_listed_file_replaced_by_symlink() {
        let d = tempfile::tempdir().unwrap();
        let content = b"hello";
        // 清单登记 a.txt,但**不**铺真实文件,改为放一个指向外部同内容文件的链接。
        let (mdir, pdir) = setup(d.path(), &[("a.txt", 5, &sha_of(content))], &[]);
        let target = d.path().join("outside.txt");
        std::fs::write(&target, content).unwrap();
        let link = pdir.join("proj").join("a.txt");
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&target, &link).is_ok();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&target, &link).is_ok();
        if !made {
            eprintln!(
                "跳过 verify_tree_flags_listed_file_replaced_by_symlink:本环境无法创建符号链接"
            );
            return;
        }
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert!(
            r.bad >= 1,
            "登记文件被替换成链接应判 bad(不跟随链接读目标判完好)"
        );
        assert!(r.has_corruption(), "链接替换应计为完整性问题");
    }

    // ── Spec D §4.1 Finding #3: issue 带项目上下文 + kind ──
    #[test]
    fn verify_tree_issue_carries_project_and_kind() {
        let d = tempfile::tempdir().unwrap();
        let (mdir, pdir) = setup(
            d.path(),
            &[("a.txt", 5, &sha_of(b"hello"))],
            &[("a.txt", b"world")], // 等长篡改 → Corrupt
        );
        let r = verify_tree(&mdir, &pdir, &abool(), &NoopReporter).unwrap();
        assert_eq!(r.issues.len(), 1);
        assert_eq!(r.issues[0].project, "proj");
        assert_eq!(r.issues[0].rel, "a.txt");
        assert_eq!(r.issues[0].kind, VerifyIssueKind::Corrupt);
    }

    // ── Spec D §4.5: 项目边界取消 → Cancelled,不计 corruption ──
    #[test]
    fn verify_tree_cancel_returns_cancelled() {
        let d = tempfile::tempdir().unwrap();
        let (mdir, pdir) = setup(
            d.path(),
            &[("a.txt", 5, &sha_of(b"hello"))],
            &[("a.txt", b"hello")],
        );
        let cancel = AtomicBool::new(true);
        let r = verify_tree(&mdir, &pdir, &cancel, &NoopReporter).unwrap();
        assert!(r.cancelled);
        assert_eq!(r.checked, 0, "项目边界即取消,未检查任何文件");
        assert!(!r.has_corruption());
        assert_eq!(r.outcome(), VerifyOutcome::Cancelled);
    }

    // ── 强优化:中途取消(已检查若干文件、已发现损坏)时结论应是 Cancelled,优先于 IssuesFound ──
    // 现有取消测试是首项即取消(checked==0)。这里锁死 outcome() 的优先级:cancelled 压过 bad,
    // 防止一次被用户中途取消的复查被误报成『整盘发现损坏』。(强优化 review)
    #[test]
    fn outcome_cancelled_takes_precedence_over_corruption() {
        // 已检查若干文件并发现一处损坏,随后被取消。
        let r = VerifyReport {
            checked: 3,
            bad: 1,
            cancelled: true,
            ..Default::default()
        };
        assert!(r.checked > 0);
        assert!(r.has_corruption());
        assert_eq!(
            r.outcome(),
            VerifyOutcome::Cancelled,
            "取消应优先于损坏,避免把未跑完的复查误报为发现损坏"
        );
    }

    /// 铺一块完整的临时"备份盘":本盘信息\本盘编号.txt + 校验清单\proj.sha256.csv + 项目\proj\<files>。
    fn setup_drive(root: &Path, rows: &[(&str, u64, &str)], files: &[(&str, &[u8])]) {
        let info = root.join(paths::DRIVE_INFO_DIR);
        let mdir = info.join(paths::DRIVE_MANIFEST_DIR);
        fs::create_dir_all(&mdir).unwrap();
        fs::write(info.join(paths::DRIVE_ID_FILE), "备份1").unwrap();
        let mut csv = String::from("Rel,Size,Hash\n");
        for (rel, size, hash) in rows {
            csv.push_str(&format!("{},{},{}\n", rel, size, hash));
        }
        fs::write(mdir.join("proj.sha256.csv"), csv).unwrap();
        fs::write(
            paths::drive_catalog_path(root),
            format!(
                "ProjectName,FileCount,TotalBytes\nproj,{},{}\n",
                rows.len(),
                rows.iter().map(|row| row.1).sum::<u64>()
            ),
        )
        .unwrap();
        let proj = root.join(paths::DRIVE_PROJECTS_DIR).join("proj");
        fs::create_dir_all(&proj).unwrap();
        for (rel, bytes) in files {
            let p = proj.join(rel);
            if let Some(par) = p.parent() {
                fs::create_dir_all(par).unwrap();
            }
            fs::write(&p, bytes).unwrap();
        }
    }

    // ── 单目标复查:整个项目文件夹(clean)──
    #[test]
    fn verify_one_clean_project_folder() {
        let d = tempfile::tempdir().unwrap();
        let content = b"hello";
        setup_drive(
            d.path(),
            &[("a.txt", 5, &sha_of(content))],
            &[("a.txt", content)],
        );
        let target = d.path().join(paths::DRIVE_PROJECTS_DIR).join("proj");
        let r = verify_one(&Config::default(), &NoopReporter, &target, &abool()).unwrap();
        assert_eq!(r.bad, 0);
        assert_eq!(r.checked, 1);
    }

    // ── 单目标复查:单个文件等长篡改 → Corrupt ──
    #[test]
    fn verify_one_corrupt_single_file() {
        let d = tempfile::tempdir().unwrap();
        setup_drive(
            d.path(),
            &[("a.txt", 5, &sha_of(b"hello"))],
            &[("a.txt", b"world")],
        );
        let target = d
            .path()
            .join(paths::DRIVE_PROJECTS_DIR)
            .join("proj")
            .join("a.txt");
        let r = verify_one(&Config::default(), &NoopReporter, &target, &abool()).unwrap();
        assert_eq!(r.checked, 1);
        assert_eq!(r.bad, 1);
        assert_eq!(r.issues[0].kind, VerifyIssueKind::Corrupt);
        assert_eq!(r.issues[0].rel, "a.txt");
    }

    // ── review-r3 round5:单文件抽查对大小写漂移(清单 A.txt / 磁盘 a.txt)应命中清单并重算 SHA256,
    // 而非误报「清单外多余」(与 verify_tree 的 extras_key 同口径)──
    #[cfg(windows)]
    #[test]
    fn verify_one_single_file_case_insensitive() {
        let d = tempfile::tempdir().unwrap();
        let content = b"hello";
        // 清单登记 A.txt,磁盘实为 a.txt(同一文件)。
        setup_drive(
            d.path(),
            &[("A.txt", 5, &sha_of(content))],
            &[("a.txt", content)],
        );
        let target = d
            .path()
            .join(paths::DRIVE_PROJECTS_DIR)
            .join("proj")
            .join("a.txt");
        let r = verify_one(&Config::default(), &NoopReporter, &target, &abool()).unwrap();
        assert_eq!(r.checked, 1, "应命中清单并重算 SHA256(而非误报多余)");
        assert_eq!(r.bad, 0, "内容一致应通过");
        assert_eq!(r.extra, 0, "大小写漂移的同一文件不得被报『清单外多余』");
    }

    // ── 单目标复查:清单外的文件 → extra,不算损坏 ──
    #[test]
    fn verify_one_file_not_in_manifest_is_extra() {
        let d = tempfile::tempdir().unwrap();
        setup_drive(
            d.path(),
            &[("a.txt", 5, &sha_of(b"hello"))],
            &[("a.txt", b"hello"), ("ghost.txt", b"x")],
        );
        let target = d
            .path()
            .join(paths::DRIVE_PROJECTS_DIR)
            .join("proj")
            .join("ghost.txt");
        let r = verify_one(&Config::default(), &NoopReporter, &target, &abool()).unwrap();
        assert_eq!(r.bad, 0);
        assert_eq!(r.extra, 1);
    }

    // ── 单目标复查:取消整项目复查 → Cancelled,不计 corruption。(V-10)
    // verify_one 复用 verify_tree 同样的 cancel.load 机制,这里覆盖整项目分支的取消路径。
    #[test]
    fn verify_one_project_folder_cancel_returns_cancelled() {
        let d = tempfile::tempdir().unwrap();
        // 故意铺一个等长篡改的坏文件:若取消没生效,会被算成 bad,断言能抓住。
        setup_drive(
            d.path(),
            &[("a.txt", 5, &sha_of(b"hello"))],
            &[("a.txt", b"world")],
        );
        let target = d.path().join(paths::DRIVE_PROJECTS_DIR).join("proj");
        let cancel = AtomicBool::new(true);
        let r = verify_one(&Config::default(), &NoopReporter, &target, &cancel).unwrap();
        assert!(r.cancelled);
        assert_eq!(r.checked, 0, "首项即取消,未检查任何文件");
        assert!(!r.has_corruption());
        assert_eq!(r.outcome(), VerifyOutcome::Cancelled);
    }

    #[test]
    fn locate_rejects_path_outside_drive() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("whatever");
        fs::create_dir_all(&p).unwrap();
        assert!(locate_backup_target(&p).is_err(), "不在备份盘内应报错");
    }

    #[test]
    fn locate_parses_project_and_rel() {
        let d = tempfile::tempdir().unwrap();
        setup_drive(d.path(), &[], &[]);
        let folder = d.path().join(paths::DRIVE_PROJECTS_DIR).join("proj");
        let (_r, project, rel) = locate_backup_target(&folder).unwrap();
        assert_eq!(project, "proj");
        assert!(rel.is_none(), "选项目文件夹 → rel=None");
        let file = folder.join("sub").join("a.txt");
        let (_r2, p2, rel2) = locate_backup_target(&file).unwrap();
        assert_eq!(p2, "proj");
        assert_eq!(rel2.as_deref(), Some("sub\\a.txt"));
    }
}

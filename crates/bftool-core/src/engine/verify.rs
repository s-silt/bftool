//! 复查：对一块盘上每个项目重算 SHA256，比对清单；同时报告"清单外多余文件"。
//! 借鉴 restic check / conserve validate 的理念：完整性检查要覆盖 数据 + 元数据。
//! **对备份盘严格只读**(复查时间/结果记到本地 system_root,见 verify_state)。

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
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
    Unverifiable, // 清单缺 Size 且缺 Hash
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
    IssuesFound { bad: u64 },
    ExtraOnly { extra: u64 },
    Cancelled,
}

/// 复查结果。`has_corruption()` = 发现损坏/缺失;结构化明细在 `issues`/`extras`。
/// CLI 据此设非零退出码(L-007);GUI 直接列明细(Spec D §4.1)。
#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    pub checked: u64,
    pub bad: u64,
    pub extra: u64,
    pub issues: Vec<VerifyIssue>,
    pub extras: Vec<ExtraFile>,
    pub cancelled: bool,
}

impl VerifyReport {
    /// 是否发现损坏/缺失。多余文件(extra)只是警告,不算数据损坏。
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
    let drives = drive::scan_mounted()?;
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

    let mdir = paths::drive_manifest_dir(&target.root);
    if !mdir.is_dir() {
        bail!("盘 {} 上没有校验清单目录：{}", target.id, mdir.display());
    }
    reporter.info(&format!(
        "开始复查 {} ({}:)，重算 SHA256 / 核对大小，可能较慢…",
        target.id, target.letter
    ));

    let projects_dir = paths::drive_projects_dir(&target.root);

    let report = verify_tree(&mdir, &projects_dir, cancel, reporter)?;

    if report.cancelled {
        reporter.warn("复查已取消(部分项目未检查)。");
        return Ok(report);
    }

    // 跑完(非取消)→ 把复查结果记到本地 system_root(verify 对盘只读);best-effort,失败只 warn。(Spec D §4.4)
    if let Err(e) = verify_state::record_verify(&cfg.system_root, &target.id, &report.outcome()) {
        reporter.warn(&format!("复查结果记录失败(不影响本次复查):{}", e));
    }

    let summary = format!(
        "复查完成：检查 {} 个文件，损坏/缺失/大小/枚举问题 {} 个,清单外多余 {} 个。",
        report.checked, report.bad, report.extra
    );
    if report.bad > 0 {
        reporter.error(&summary);
    } else if report.extra > 0 {
        reporter.warn(&summary);
    } else {
        reporter.ok(&summary);
    }
    if report.extra > 0 {
        reporter.info("（多余文件不会自动删除；如确认无用可人工清理。）");
    }
    Ok(report)
}

/// 按"校验清单目录 + 项目目录"做复查,不依赖挂载盘检测 —— 便于测试。
/// 覆盖损坏类别:缺失 / 大小不符 / 内容(SHA)不符 / 不可校验 / 读失败 / 枚举失败;另统计多余文件。
/// 取消在**项目边界**检查(verify 只读,可安全在任意项目前停);结果 `cancelled=true`。(Spec D §4.5)
fn verify_tree(
    mdir: &Path,
    projects_dir: &Path,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();

    let mut manifest_files = Vec::new();
    for e in
        fs::read_dir(mdir).with_context(|| format!("读取校验清单目录失败：{}", mdir.display()))?
    {
        let e = e.with_context(|| format!("枚举校验清单目录失败：{}", mdir.display()))?;
        if e.file_name().to_string_lossy().ends_with(".sha256.csv") {
            manifest_files.push(e);
        }
    }
    manifest_files.sort_by_key(|e| e.file_name());

    for mf in &manifest_files {
        // 项目边界取消:verify 只读,可安全在此停下。
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            return Ok(report);
        }
        let stem = mf.file_name();
        let stem = stem.to_string_lossy();
        let project_name = stem
            .strip_suffix(".sha256.csv")
            .unwrap_or(&stem)
            .to_string();
        reporter.action(&format!("· {}", project_name));
        let proj_dir = projects_dir.join(&project_name);

        // 一个坏清单(读不出/缺 Rel 列)只跳过该项目(warn),不中断整盘复查。
        let rows = match read_manifest_rows(&mf.path()) {
            Ok(r) => r,
            Err(e) => {
                reporter.error(&format!("跳过(清单有问题):{} —— {}", project_name, e));
                report.push_issue(
                    &project_name,
                    &mf.file_name().to_string_lossy(),
                    VerifyIssueKind::ReadError,
                );
                continue;
            }
        };
        let mut expected: HashSet<String> = HashSet::new();
        for (rel, size, hash) in &rows {
            expected.insert(rel.clone());
            check_one_file(
                &proj_dir,
                &project_name,
                rel,
                *size,
                hash,
                reporter,
                &mut report,
            );
        }
        report_extras(&proj_dir, &project_name, &expected, reporter, &mut report);
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
    let manifest = paths::drive_manifest_dir(&root).join(format!("{}.sha256.csv", project));
    if !manifest.is_file() {
        bail!(
            "找不到项目「{}」的校验清单:{}(它可能不是本工具归档的项目)",
            project,
            manifest.display()
        );
    }
    let proj_dir = paths::drive_projects_dir(&root).join(&project);
    let rows = read_manifest_rows(&manifest)?;

    match rel {
        // 整个项目文件夹:逐清单项校验 + 报告多余文件。
        None => {
            reporter.info(&format!("复查项目「{}」(重算 SHA256 比对清单)…", project));
            let mut expected: HashSet<String> = HashSet::new();
            for (r, size, hash) in &rows {
                if cancel.load(Ordering::Relaxed) {
                    report.cancelled = true;
                    reporter.warn("复查已取消。");
                    return Ok(report);
                }
                expected.insert(r.clone());
                check_one_file(&proj_dir, &project, r, *size, hash, reporter, &mut report);
            }
            report_extras(&proj_dir, &project, &expected, reporter, &mut report);
        }
        // 单个文件:在清单里找它的期望值再比对;清单里没有 → 多余(无法比对哈希)。
        Some(rel) => {
            reporter.info(&format!("复查文件「{}\\{}」…", project, rel));
            match rows.iter().find(|(r, _, _)| r == &rel) {
                Some((r, size, hash)) => {
                    check_one_file(&proj_dir, &project, r, *size, hash, reporter, &mut report);
                }
                None => {
                    reporter.warn(&format!(
                        "该文件不在项目清单中,无法比对哈希(可能是清单外的多余文件):{}",
                        rel
                    ));
                    report.push_extra(&project, &rel);
                }
            }
        }
    }

    let summary = format!(
        "单目标复查完成:检查 {} 个文件,损坏/缺失/大小问题 {} 个,清单外 {} 个。",
        report.checked, report.bad, report.extra
    );
    if report.bad > 0 {
        reporter.error(&summary);
    } else if report.extra > 0 {
        reporter.warn(&summary);
    } else {
        reporter.ok(&summary);
    }
    Ok(report)
}

/// 校验单个文件 vs 清单期望(缺失/读失败/不可校验/大小/SHA),结果记进 report。
/// verify_tree(整盘)与 verify_one(单目标)共用,保证两条路径判定一致。
fn check_one_file(
    proj_dir: &Path,
    project_name: &str,
    rel: &str,
    size: Option<u64>,
    hash: &str,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) {
    let f = proj_dir.join(rel);
    report.checked += 1;
    if !f.is_file() {
        reporter.error(&format!("  缺失: {}", rel));
        report.push_issue(project_name, rel, VerifyIssueKind::Missing);
        return;
    }
    let meta = match fs::metadata(&f) {
        Ok(m) => m,
        Err(e) => {
            reporter.error(&format!("  无法读元数据 {}: {}", rel, e));
            report.push_issue(project_name, rel, VerifyIssueKind::ReadError);
            return;
        }
    };
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
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(proj_dir)
            .map(|p| p.to_string_lossy().replace('/', "\\"))
            .unwrap_or_default();
        if !expected.contains(&rel) {
            reporter.warn(&format!("  多余(清单外): {}", rel));
            report.push_extra(project_name, &rel);
        }
    }
}

/// 读清单为 (Rel, Size?, Hash) 行;跳过 cruft 段;缺 Rel 列/读失败 → Err(由调用方决定跳过还是报错)。
/// size 为 Option:列缺失/不可解析 = 未知(不校验大小);存在(含 0)就精确比对。(ledger L-013)
fn read_manifest_rows(path: &Path) -> Result<Vec<(String, Option<u64>, String)>> {
    let mut rdr = csv::Reader::from_path(path)
        .with_context(|| format!("读校验清单失败：{}", path.display()))?;
    let headers = rdr
        .headers()
        .with_context(|| format!("读校验清单表头失败：{}", path.display()))?
        .clone();
    let i_rel = headers.iter().position(|h| h == "Rel");
    let i_size = headers.iter().position(|h| h == "Size");
    let i_hash = headers.iter().position(|h| h == "Hash");
    let Some(i_rel) = i_rel else {
        bail!("清单缺少 Rel 列：{}", path.display());
    };
    let mut out = Vec::new();
    for (idx, rec) in rdr.records().enumerate() {
        let rec =
            rec.with_context(|| format!("校验清单第 {} 行格式错误：{}", idx + 2, path.display()))?;
        let rel = rec.get(i_rel).unwrap_or("").to_string();
        // 跳过空 Rel 行:proj_dir.join("") == proj_dir 本身,会把项目目录当文件校验
        // 而误报 Missing(目录不是 is_file())。空 Rel 是脏数据,直接丢弃。(V-09)
        if rel.is_empty() {
            continue;
        }
        validate_manifest_rel(path, &rel)?;
        if cruft::rel_has_cruft_component(&rel) {
            continue;
        }
        let size = i_size
            .and_then(|c| rec.get(c))
            .and_then(|s| s.parse::<u64>().ok());
        let hash = i_hash.and_then(|c| rec.get(c)).unwrap_or("").to_string();
        out.push((rel, size, hash));
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

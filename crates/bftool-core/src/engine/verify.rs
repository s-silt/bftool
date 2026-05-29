//! 复查：对一块盘上每个项目重算 SHA256，比对清单；同时报告"清单外多余文件"。
//! 借鉴 restic check / conserve validate 的理念：完整性检查要覆盖 数据 + 元数据。
//! **对备份盘严格只读**(复查时间/结果记到本地 system_root,见 verify_state)。

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;
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

    let mut manifest_files: Vec<_> = fs::read_dir(mdir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sha256.csv"))
        .collect();
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
        let mut expected: HashMap<String, (u64, String)> = HashMap::new();

        let mut rdr = csv::Reader::from_path(mf.path())
            .with_context(|| format!("读校验清单失败：{}", mf.path().display()))?;
        let headers = rdr.headers()?.clone();
        let i_rel = headers.iter().position(|h| h == "Rel");
        let i_size = headers.iter().position(|h| h == "Size");
        let i_hash = headers.iter().position(|h| h == "Hash");
        let Some(i_rel) = i_rel else {
            reporter.warn(&format!("清单缺少 Rel 列，跳过：{}", mf.path().display()));
            continue;
        };

        for rec in rdr.records().flatten() {
            let rel = rec.get(i_rel).unwrap_or("").to_string();
            // 用 rel_has_cruft_component 检查全部路径段,覆盖 cruft 目录下的旧条目
            if cruft::rel_has_cruft_component(&rel) {
                continue;
            }
            // size 为 Option:列缺失/不可解析 = 未知(不校验大小);存在(含 0)就精确比对。(ledger L-013)
            let size = i_size
                .and_then(|c| rec.get(c))
                .and_then(|s| s.parse::<u64>().ok());
            let hash = i_hash.and_then(|c| rec.get(c)).unwrap_or("").to_string();
            expected.insert(rel.clone(), (size.unwrap_or(0), hash.clone()));

            let f = proj_dir.join(&rel);
            report.checked += 1;
            if !f.is_file() {
                reporter.error(&format!("  缺失: {}", rel));
                report.push_issue(&project_name, &rel, VerifyIssueKind::Missing);
                continue;
            }
            let meta = match fs::metadata(&f) {
                Ok(m) => m,
                Err(e) => {
                    reporter.error(&format!("  无法读元数据 {}: {}", rel, e));
                    report.push_issue(&project_name, &rel, VerifyIssueKind::ReadError);
                    continue;
                }
            };
            // 既无 Size 也无 Hash → 无任何可校验属性 → fail-closed。(Phase 4 F-01/F-6)
            if size.is_none() && hash.is_empty() {
                reporter.error(&format!("  清单项缺 Size 且缺 Hash,无法校验: {}", rel));
                report.push_issue(&project_name, &rel, VerifyIssueKind::Unverifiable);
                continue;
            }
            if let Some(sz) = size {
                if meta.len() != sz {
                    reporter.error(&format!("  大小不一致: {}", rel));
                    report.push_issue(&project_name, &rel, VerifyIssueKind::SizeMismatch);
                    continue;
                }
            }
            if !hash.is_empty() {
                match sha256_hex(&f) {
                    Ok(h) if h == hash => {}
                    Ok(_) => {
                        reporter.error(&format!("  损坏/不一致: {}", rel));
                        report.push_issue(&project_name, &rel, VerifyIssueKind::Corrupt);
                    }
                    Err(e) => {
                        reporter.error(&format!("  读取失败 {}: {}", rel, e));
                        report.push_issue(&project_name, &rel, VerifyIssueKind::ReadError);
                    }
                }
            }
        }

        // 报告清单外的多余文件（不删除）
        if proj_dir.is_dir() {
            // 同 L-044:不 canonicalize,否则 `\\?\` 前缀与 walk 路径失配 → extra 全部误报。
            let base = proj_dir.clone();
            for entry in cruft::walk(&proj_dir) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        reporter.error(&format!("  枚举失败: {}", err));
                        report.push_issue(&project_name, "", VerifyIssueKind::EnumError);
                        continue;
                    }
                };
                if !entry.file_type().is_file() {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(&base)
                    .map(|p| p.to_string_lossy().replace('/', "\\"))
                    .unwrap_or_default();
                if !expected.contains_key(&rel) {
                    reporter.warn(&format!("  多余(清单外): {}", rel));
                    report.push_extra(&project_name, &rel);
                }
            }
        }
    }

    Ok(report)
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
}

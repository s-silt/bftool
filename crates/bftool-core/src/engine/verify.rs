//! 复查：对一块盘上每个项目重算 SHA256，比对清单；同时报告"清单外多余文件"。
//! 借鉴 restic check / conserve validate 的理念：完整性检查要覆盖 数据 + 元数据。

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::config::Config;
use crate::engine::{cruft, drive, paths};
use crate::reporter::Reporter;

/// 复查结果。`bad>0` = 本盘完整性有问题(损坏/缺失/大小不符/枚举失败)。
/// 返回给调用方:CLI 据此设非零退出码(cron/计划任务能识别坏盘),GUI 可读字段。
/// 此前 `run` 永远返回 `Ok(())`,损坏只走 reporter 文本 → 自动化层看不到。(ledger L-007)
#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    pub checked: u64,
    pub bad: u64,
    pub extra: u64,
}

impl VerifyReport {
    /// 是否发现损坏/缺失。多余文件(extra)只是警告,不算数据损坏。
    pub fn has_corruption(&self) -> bool {
        self.bad > 0
    }
}

pub fn run(
    cfg: &Config,
    reporter: &dyn Reporter,
    drive_letter: Option<&str>,
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
    let _ = cfg; // 当前 verify 不需要 cfg；保留参数便于将来加 --full 整库交叉核对

    let report = verify_tree(&mdir, &projects_dir, reporter)?;

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
/// 覆盖四类损坏:缺失 / 大小不符 / 内容(SHA)不符 / 枚举失败;另统计清单外多余文件。
fn verify_tree(mdir: &Path, projects_dir: &Path, reporter: &dyn Reporter) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();

    let mut manifest_files: Vec<_> = fs::read_dir(mdir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sha256.csv"))
        .collect();
    manifest_files.sort_by_key(|e| e.file_name());

    for mf in &manifest_files {
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
            // 第七轮 P2:用 rel_has_cruft_component 检查全部路径段,覆盖 cruft 目录下的旧条目
            if cruft::rel_has_cruft_component(&rel) {
                continue;
            }
            // size 为 Option:列缺失/不可解析 = 未知(不校验大小);存在(含 0)就精确比对。
            // 旧实现的 `size>0 &&` 守卫会让"清单记 0、实际非 0"的真实 0 字节文件被篡改时漏判。(ledger L-013)
            let size = i_size
                .and_then(|c| rec.get(c))
                .and_then(|s| s.parse::<u64>().ok());
            let hash = i_hash.and_then(|c| rec.get(c)).unwrap_or("").to_string();
            expected.insert(rel.clone(), (size.unwrap_or(0), hash.clone()));

            let f = proj_dir.join(&rel);
            report.checked += 1;
            if !f.is_file() {
                reporter.error(&format!("  缺失: {}", rel));
                report.bad += 1;
                continue;
            }
            let meta = match fs::metadata(&f) {
                Ok(m) => m,
                Err(e) => {
                    reporter.error(&format!("  无法读元数据 {}: {}", rel, e));
                    report.bad += 1;
                    continue;
                }
            };
            if let Some(sz) = size {
                if meta.len() != sz {
                    reporter.error(&format!("  大小不一致: {}", rel));
                    report.bad += 1;
                    continue;
                }
            }
            if !hash.is_empty() {
                match sha256_hex(&f) {
                    Ok(h) if h == hash => {}
                    Ok(_) => {
                        reporter.error(&format!("  损坏/不一致: {}", rel));
                        report.bad += 1;
                    }
                    Err(e) => {
                        reporter.error(&format!("  读取失败 {}: {}", rel, e));
                        report.bad += 1;
                    }
                }
            }
        }

        // 报告清单外的多余文件（不删除）
        if proj_dir.is_dir() {
            let base = proj_dir.canonicalize().unwrap_or_else(|_| proj_dir.clone());
            for entry in cruft::walk(&proj_dir) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        reporter.error(&format!("  枚举失败: {}", err));
                        report.bad += 1;
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
                    report.extra += 1;
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
    /// 返回 (mdir, projects_dir);projects_dir 已 canonicalize 以便 extra 检测的
    /// strip_prefix 与 walk 前缀一致。
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
        (mdir, pdir.canonicalize().unwrap())
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
        let r = verify_tree(&mdir, &pdir, &NoopReporter).unwrap();
        assert_eq!(r.bad, 0);
        assert_eq!(r.checked, 1);
        assert_eq!(r.extra, 0);
    }

    #[test]
    fn verify_tree_detects_missing_file() {
        let d = tempfile::tempdir().unwrap();
        let (mdir, pdir) = setup(d.path(), &[("a.txt", 5, &sha_of(b"hello"))], &[]);
        let r = verify_tree(&mdir, &pdir, &NoopReporter).unwrap();
        assert_eq!(r.bad, 1);
        assert!(r.has_corruption());
    }

    #[test]
    fn verify_tree_detects_size_mismatch() {
        let d = tempfile::tempdir().unwrap();
        // 清单 size=5,实际 3 字节
        let (mdir, pdir) = setup(d.path(), &[("a.txt", 5, "")], &[("a.txt", b"abc")]);
        let r = verify_tree(&mdir, &pdir, &NoopReporter).unwrap();
        assert_eq!(r.bad, 1);
    }

    #[test]
    fn verify_tree_detects_content_change_same_size() {
        let d = tempfile::tempdir().unwrap();
        // 清单记 "hello" 的 hash,实际是等长 "world":size 不变,只能靠 SHA 抓
        let (mdir, pdir) = setup(
            d.path(),
            &[("a.txt", 5, &sha_of(b"hello"))],
            &[("a.txt", b"world")],
        );
        let r = verify_tree(&mdir, &pdir, &NoopReporter).unwrap();
        assert_eq!(r.bad, 1, "等长内容篡改必须靠 SHA 抓出");
    }

    // ── L-013: 清单记 size=0 不能跳过大小校验 ──
    #[test]
    fn verify_tree_detects_tampered_zero_byte_file() {
        let d = tempfile::tempdir().unwrap();
        // 清单记 size=0、无 hash(no_hash 模式),但实际文件被塞了内容
        let (mdir, pdir) = setup(d.path(), &[("z.txt", 0, "")], &[("z.txt", b"surprise")]);
        let r = verify_tree(&mdir, &pdir, &NoopReporter).unwrap();
        assert_eq!(r.bad, 1, "清单记 0 字节、实际非 0 必须报大小不一致");
    }

    #[test]
    fn verify_tree_real_zero_byte_file_is_ok() {
        let d = tempfile::tempdir().unwrap();
        let (mdir, pdir) = setup(d.path(), &[("z.txt", 0, "")], &[("z.txt", b"")]);
        let r = verify_tree(&mdir, &pdir, &NoopReporter).unwrap();
        assert_eq!(r.bad, 0, "真实 0 字节文件应通过");
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
        let r = verify_tree(&mdir, &pdir, &NoopReporter).unwrap();
        assert_eq!(r.bad, 0);
        assert_eq!(r.extra, 1);
    }
}

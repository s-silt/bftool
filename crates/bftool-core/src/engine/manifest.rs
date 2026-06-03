//! 项目清单：枚举文件夹下所有「真实文件」（follow_links=false,不跟随目录 symlink/junction），
//! 算大小 / SHA256 / 修改时间。复制前生成源清单，复制后生成目标清单做三重比对。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::engine::cruft;
use crate::reporter::Reporter;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// 相对源目录的路径，使用 `\` 分隔（Windows 习惯）
    #[serde(rename = "Rel")]
    pub rel: String,
    #[serde(rename = "Size")]
    pub size: u64,
    /// SHA256 大写十六进制；`no_hash` 模式下为 `None`(不再用空串当"没算哈希"的哨兵)。(ledger L-019)
    #[serde(rename = "Hash")]
    pub hash: Option<String>,
    /// 修改时间，RFC3339 UTC；用于 `no_hash` 模式的源稳定性二次复核
    #[serde(rename = "Mtime")]
    pub mtime: String,
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub entries: Vec<Entry>,
}

impl Manifest {
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// 把清单写成 CSV 文件（与 PowerShell 旧版列名一致：Rel,Size,Hash 三列）
    pub fn write_csv(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // 先序列化到内存,再用 durable::write_synced 原子落盘(同目录 tmp + fsync + rename)。
        // 校验清单是恢复/复查的唯一依据,提交序「写清单 → 移源」中它先落盘;旧实现 from_path
        // 直接在原位截断后写,崩溃/断电恰发生在「已截断、未写完」之间会留下半截清单(与 SEC-006 同源)。
        // 原子 rename 保证读者只看到旧清单或完整新清单,绝不半截。fsync 仍在(write_synced 内)。(review-r2 #3 / L-001)
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(true)
            .from_writer(Vec::new());
        // 只写 Rel/Size/Hash 三列以保持简洁；Mtime 内部用，不入清单
        #[derive(Serialize)]
        struct Row<'a> {
            #[serde(rename = "Rel")]
            rel: &'a str,
            #[serde(rename = "Size")]
            size: u64,
            #[serde(rename = "Hash")]
            hash: &'a str,
        }
        for e in &self.entries {
            wtr.serialize(Row {
                rel: &e.rel,
                size: e.size,
                hash: e.hash.as_deref().unwrap_or(""),
            })?;
        }
        wtr.flush()?;
        let bytes = wtr
            .into_inner()
            .map_err(|e| anyhow::anyhow!("序列化校验清单失败：{}", e))?;
        crate::engine::durable::write_synced(path, &bytes)
            .with_context(|| format!("写校验清单失败：{}", path.display()))?;
        Ok(())
    }
}

/// 列举一个文件夹下所有真实文件。
/// - 不跟随目录链接（symlink/junction；follow_links=false）
/// - 跳过 cruft（Thumbs.db、$RECYCLE.BIN 等）
/// - 枚举失败 → 收集所有错误,本项目 bail（D4 一次输出）；本项目跳过,不影响后续项目；下次运行重做。
fn real_files(root: &Path, reporter: &dyn Reporter) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for entry in cruft::walk(root) {
        match entry {
            Ok(e) if e.file_type().is_file() => out.push(e.into_path()),
            Ok(_) => continue,
            Err(err) => errors.push(format!("{}", err)),
        }
    }

    if !errors.is_empty() {
        for e in &errors {
            reporter.error(&format!("枚举失败：{}", e));
        }
        anyhow::bail!(
            "枚举本项目时遇到 {} 个错误（详见上方）→ 本项目跳过,不影响后续项目；下次运行会重做。",
            errors.len()
        );
    }
    Ok(out)
}

pub struct ManifestOpts {
    pub no_hash: bool,
}

/// 生成文件夹清单。`no_hash=true` 时 `Entry.hash` 为 `None`(CSV 落盘仍写空串保持兼容)。
pub fn build(root: &Path, opts: ManifestOpts, reporter: &dyn Reporter) -> Result<Manifest> {
    let files = real_files(root, reporter)?;
    let total_bytes: u64 = files
        .iter()
        .filter_map(|p| p.metadata().ok().map(|m| m.len()))
        .sum();

    let mut bar = reporter.progress_bytes(if opts.no_hash { "枚举" } else { "校验" }, total_bytes);

    // 不要 canonicalize:Windows 上它会加 `\\?\` 前缀,而 cruft::walk(WalkDir)产出的路径
    // 不带前缀 → strip_prefix 失配 → rel 退化成绝对路径 → diff/verify 永远失败。
    // WalkDir 产出的路径必以传入的 root 为前缀,直接用 root 做 base 即可。(ledger L-044)
    let base = root.to_path_buf();
    let mut entries = Vec::with_capacity(files.len());
    let mut metadata_errors: Vec<String> = Vec::new();
    let mut hash_errors: Vec<String> = Vec::new();

    for f in &files {
        let meta = match f.metadata() {
            Ok(m) => m,
            Err(e) => {
                metadata_errors.push(format!("{}: {}", f.display(), e));
                continue;
            }
        };
        let size = meta.len();
        // mtime 读不到时存空串(不要用 UNIX_EPOCH 兜底):空串在 no_hash 复核里被当作
        // "无法证明未变" → 保守判定为已变,而 UNIX_EPOCH 兜底会让两次都相等从而漏判。(ledger L-004)
        let mtime = meta
            .modified()
            .ok()
            .map(system_time_to_rfc3339)
            .unwrap_or_default();
        let rel = path_relative(&base, f);

        let hash = if opts.no_hash {
            None
        } else {
            match sha256_hex(f) {
                Ok(h) => Some(h),
                Err(e) => {
                    hash_errors.push(format!("{}: {}", f.display(), e));
                    continue;
                }
            }
        };

        bar.inc(size);
        entries.push(Entry {
            rel,
            size,
            hash,
            mtime,
        });
    }
    bar.finish();

    if !metadata_errors.is_empty() || !hash_errors.is_empty() {
        for e in &metadata_errors {
            reporter.error(&format!("读元数据失败：{}", e));
        }
        for e in &hash_errors {
            reporter.error(&format!("读文件内容失败：{}", e));
        }
        anyhow::bail!(
            "本项目有 {} 个元数据失败、{} 个内容读取失败 → 本项目跳过,下次重做。",
            metadata_errors.len(),
            hash_errors.len()
        );
    }

    // 重复 rel 检测(fail-closed)。path_relative 用 to_string_lossy 把非法 UTF-16 文件名折叠成
    // U+FFFD,两个仅在非法字节序列上不同的文件可能映射到同一 rel。而 diff/source_changed 都用
    // HashMap<rel,&Entry> 索引,collect 遇重复 key 只保留最后一条、静默丢弃前一条 → 另一文件的
    // 损坏/缺失逃过逐项比对,形成「校验通过」假象。这里在源头拒绝:发现重复即判清单不可信,
    // 本项目跳过、下次重做,杜绝 diff/source_changed 的静默折叠。(review-r3)
    {
        let mut seen: HashSet<&str> = HashSet::with_capacity(entries.len());
        let dups: Vec<&str> = entries
            .iter()
            .filter(|e| !seen.insert(e.rel.as_str()))
            .map(|e| e.rel.as_str())
            .collect();
        if !dups.is_empty() {
            for r in &dups {
                reporter.error(&format!("清单出现重复相对路径：{}", r));
            }
            anyhow::bail!(
                "本项目清单出现 {} 个重复相对路径(可能含非法文件名)→ 清单不可信,本项目跳过,下次重做。",
                dups.len()
            );
        }
    }

    Ok(Manifest { entries })
}

fn sha256_hex(path: &Path) -> Result<String> {
    let f = File::open(path).with_context(|| format!("打开文件失败：{}", path.display()))?;
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

fn path_relative(base: &Path, full: &Path) -> String {
    full.strip_prefix(base)
        .map(|p| p.to_string_lossy().replace('/', "\\"))
        .unwrap_or_else(|_| full.to_string_lossy().to_string())
}

fn system_time_to_rfc3339(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.to_rfc3339()
}

/// 三重比对结果
#[derive(Debug, Default)]
pub struct Diff {
    pub reasons: Vec<String>,
    /// 目标侧有问题的相对路径（多余/大小不符/哈希不符），用来隔离重传
    pub bad_dst_rels: Vec<String>,
}

impl Diff {
    /// 是否通过:由 reasons 派生(有任何 reason = 不通过),消除 ok 字段与 reasons 漂移的可能。(ledger L-024)
    pub fn ok(&self) -> bool {
        self.reasons.is_empty()
    }
}

/// 用源清单核对目标清单：数量、字节、逐文件相对路径 + 大小 (+ 哈希)。
pub fn diff(src: &Manifest, dst: &Manifest, check_hash: bool) -> Diff {
    let mut d = Diff::default();

    if src.count() != dst.count() {
        d.reasons
            .push(format!("文件数 src={} dst={}", src.count(), dst.count()));
    }
    if src.total_bytes() != dst.total_bytes() {
        d.reasons.push(format!(
            "总字节 src={} dst={}",
            src.total_bytes(),
            dst.total_bytes()
        ));
    }
    let smap: HashMap<&str, &Entry> = src.entries.iter().map(|e| (e.rel.as_str(), e)).collect();
    for ent in &dst.entries {
        let Some(s) = smap.get(ent.rel.as_str()) else {
            d.reasons.push(format!("目标多出 {}", ent.rel));
            d.bad_dst_rels.push(ent.rel.clone());
            continue;
        };
        if s.size != ent.size {
            d.reasons.push(format!("大小不一致 {}", ent.rel));
            d.bad_dst_rels.push(ent.rel.clone());
            continue;
        }
        if check_hash && s.hash.is_some() && s.hash != ent.hash {
            d.reasons.push(format!("哈希不一致 {}", ent.rel));
            d.bad_dst_rels.push(ent.rel.clone());
        }
    }
    // 对称检查:源有但目标缺的文件,显式报告,不只靠 count/total_bytes 聚合量兜底
    // (聚合量在 cruft 过滤不对称时可能被凑平 → 漏检;reason 也更准)。缺失文件不进
    // bad_dst_rels(目标侧无此文件可隔离),copy_folder 下轮会自动补传。(ledger L-002)
    let dmap: HashMap<&str, &Entry> = dst.entries.iter().map(|e| (e.rel.as_str(), e)).collect();
    for ent in &src.entries {
        if !dmap.contains_key(ent.rel.as_str()) {
            d.reasons.push(format!("源有目标缺 {}", ent.rel));
        }
    }
    d
}

/// 复制期间源是否变化（移动源之前最后一道防线；新顺序下索引已写、就差移源）。
pub fn source_changed(
    initial: &Manifest,
    current: &Manifest,
    no_hash: bool,
) -> (bool, Vec<String>) {
    let mut reasons = Vec::new();
    let mut changed = false;
    if initial.count() != current.count() {
        changed = true;
        reasons.push(format!("文件数 {}→{}", initial.count(), current.count()));
    }
    if initial.total_bytes() != current.total_bytes() {
        changed = true;
        reasons.push(format!(
            "总字节 {}→{}",
            initial.total_bytes(),
            current.total_bytes()
        ));
    }
    if !changed {
        let cmap: HashMap<&str, &Entry> = current
            .entries
            .iter()
            .map(|e| (e.rel.as_str(), e))
            .collect();
        for i in &initial.entries {
            let Some(j) = cmap.get(i.rel.as_str()) else {
                changed = true;
                reasons.push(format!("缺少 {}", i.rel));
                break;
            };
            if j.size != i.size {
                changed = true;
                reasons.push(format!("大小变化 {}", i.rel));
                break;
            }
            if no_hash {
                // 空 mtime = 该文件 mtime 不可读 → 无法证明复制期间未变 → 保守判为已变(fail-closed)
                if i.mtime.is_empty() || j.mtime.is_empty() {
                    changed = true;
                    reasons.push(format!("修改时间不可读,无法确认未变 {}", i.rel));
                    break;
                }
                if j.mtime != i.mtime {
                    changed = true;
                    reasons.push(format!("修改时间变化 {}", i.rel));
                    break;
                }
            } else if j.hash != i.hash {
                changed = true;
                reasons.push(format!("内容变化 {}", i.rel));
                break;
            }
        }
    }
    (changed, reasons)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(rel: &str, size: u64, hash: &str, mtime: &str) -> Entry {
        Entry {
            rel: rel.into(),
            size,
            hash: if hash.is_empty() {
                None
            } else {
                Some(hash.to_string())
            },
            mtime: mtime.into(),
        }
    }

    // ── review-r2 #3:write_csv 内容正确 + 原子落盘(不遗留 .bftool-tmp);崩溃原子性靠 durable::write_synced ──
    #[test]
    fn write_csv_writes_rel_size_hash_atomically() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub").join("001proj.sha256.csv"); // 父目录不存在 → 应自动建
        let m = Manifest {
            entries: vec![
                e("a.txt", 5, "ABC", "2026-05-29T00:00:00Z"),
                e("sub\\b.bin", 7, "DEF", "2026-05-29T00:00:00Z"),
            ],
        };
        m.write_csv(&p).unwrap();
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(
            content.starts_with("Rel,Size,Hash"),
            "表头三列;实际:{content}"
        );
        assert!(content.contains("a.txt,5,ABC"));
        assert!(content.contains("sub\\b.bin,7,DEF"));
        let tmp = p.with_file_name("001proj.sha256.csv.bftool-tmp");
        assert!(!tmp.exists(), "原子写不应遗留临时文件");
    }

    // ── L-004: no_hash 复核不能被"不可读 mtime"绕过 ──
    #[test]
    fn source_changed_no_hash_treats_unreadable_mtime_as_changed() {
        // 两次 build 都读不到 mtime(空串):旧实现用 UNIX_EPOCH 兜底会让两者相等 → 漏判;
        // 现在应保守判为"已变",防止"复制期间内容变了但 mtime 读不到"时把旧/坏版本移走。
        let a = Manifest {
            entries: vec![e("x", 10, "", "")],
        };
        let b = Manifest {
            entries: vec![e("x", 10, "", "")],
        };
        assert!(
            source_changed(&a, &b, true).0,
            "空 mtime 无法证明未变,应保守判为已变"
        );
    }

    #[test]
    fn source_changed_no_hash_same_mtime_unchanged() {
        let a = Manifest {
            entries: vec![e("x", 10, "", "2026-01-01T00:00:00+00:00")],
        };
        let b = Manifest {
            entries: vec![e("x", 10, "", "2026-01-01T00:00:00+00:00")],
        };
        assert!(!source_changed(&a, &b, true).0);
    }

    #[test]
    fn source_changed_no_hash_diff_mtime_changed() {
        let a = Manifest {
            entries: vec![e("x", 10, "", "2026-01-01T00:00:00+00:00")],
        };
        let b = Manifest {
            entries: vec![e("x", 10, "", "2026-02-02T00:00:00+00:00")],
        };
        assert!(source_changed(&a, &b, true).0);
    }

    // ── L-002: diff 显式报告"源有目标缺",不只靠 count/bytes 兜底 ──
    #[test]
    fn diff_reports_missing_source_file_explicitly() {
        // 同 count、同字节,但 b 缺失、c 多出(改名/替换):旧实现只报"目标多出 c",
        // 漏报缺 b;现在应同时显式报"源有目标缺 b"。
        let src = Manifest {
            entries: vec![e("a", 10, "h1", ""), e("b", 10, "h2", "")],
        };
        let dst = Manifest {
            entries: vec![e("a", 10, "h1", ""), e("c", 10, "h3", "")],
        };
        let d = diff(&src, &dst, true);
        assert!(!d.ok());
        assert!(
            d.reasons
                .iter()
                .any(|r| r.contains("源有目标缺") && r.contains('b')),
            "应显式报告缺失的源文件 b,实际 reasons={:?}",
            d.reasons
        );
    }

    #[test]
    fn diff_missing_in_dst_sets_not_ok() {
        let src = Manifest {
            entries: vec![e("a", 10, "h1", ""), e("b", 10, "h2", "")],
        };
        let dst = Manifest {
            entries: vec![e("a", 10, "h1", "")],
        };
        let d = diff(&src, &dst, true);
        assert!(!d.ok());
        assert!(d
            .reasons
            .iter()
            .any(|r| r.contains("源有目标缺") && r.contains('b')));
    }

    #[test]
    fn diff_identical_is_ok() {
        let src = Manifest {
            entries: vec![e("a", 10, "h1", "")],
        };
        let dst = Manifest {
            entries: vec![e("a", 10, "h1", "")],
        };
        assert!(diff(&src, &dst, true).ok());
    }
}

//! 项目清单：枚举文件夹下所有「真实文件」（不进 junction、跳过 reparse point），
//! 算大小 / SHA256 / 修改时间。复制前生成源清单，复制后生成目标清单做三重比对。

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

use crate::ui;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// 相对源目录的路径，使用 `\` 分隔（Windows 习惯）
    #[serde(rename = "Rel")]
    pub rel: String,
    #[serde(rename = "Size")]
    pub size: u64,
    /// SHA256 大写十六进制；`no_hash` 模式下为空串
    #[serde(rename = "Hash")]
    pub hash: String,
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
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(true)
            .from_path(path)
            .with_context(|| format!("写校验清单失败：{}", path.display()))?;
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
                hash: &e.hash,
            })?;
        }
        wtr.flush()?;
        Ok(())
    }
}

/// 列举一个文件夹下所有真实文件。
/// - 不进入目录链接（symlink/junction）
/// - 跳过文件 reparse point
/// - 出错的子项记日志后跳过，不让整轮失败
fn real_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false) // 不递归进 symlink/junction
        .into_iter();
    for entry in walker {
        match entry {
            Ok(e) => {
                let ft = e.file_type();
                if ft.is_file() {
                    // walkdir 已经在 follow_links=false 时跳过链接，无需再判
                    out.push(e.into_path());
                }
            }
            Err(err) => {
                ui::warn(format!("枚举文件时跳过一项：{}", err));
            }
        }
    }
    out
}

pub struct ManifestOpts {
    pub no_hash: bool,
}

/// 生成文件夹清单。`no_hash=true` 时 Hash 字段为空串。
pub fn build(root: &Path, opts: ManifestOpts) -> Result<Manifest> {
    let files = real_files(root);
    let total_bytes: u64 = files
        .iter()
        .filter_map(|p| p.metadata().ok().map(|m| m.len()))
        .sum();

    let bar = ui::bytes_bar(total_bytes, if opts.no_hash { "枚举" } else { "校验" });

    let base = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut entries = Vec::with_capacity(files.len());

    for f in &files {
        let meta = f
            .metadata()
            .with_context(|| format!("读元数据失败：{}", f.display()))?;
        let size = meta.len();
        let mtime = system_time_to_rfc3339(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));
        let rel = path_relative(&base, f);

        let hash = if opts.no_hash {
            String::new()
        } else {
            sha256_hex(f)?
        };

        bar.inc(size);
        entries.push(Entry {
            rel,
            size,
            hash,
            mtime,
        });
    }
    bar.finish_and_clear();
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
    pub ok: bool,
    pub reasons: Vec<String>,
    /// 目标侧有问题的相对路径（多余/大小不符/哈希不符），用来隔离重传
    pub bad_dst_rels: Vec<String>,
}

/// 用源清单核对目标清单：数量、字节、逐文件相对路径 + 大小 (+ 哈希)。
pub fn diff(src: &Manifest, dst: &Manifest, check_hash: bool) -> Diff {
    let mut d = Diff {
        ok: true,
        reasons: Vec::new(),
        bad_dst_rels: Vec::new(),
    };

    if src.count() != dst.count() {
        d.ok = false;
        d.reasons
            .push(format!("文件数 src={} dst={}", src.count(), dst.count()));
    }
    if src.total_bytes() != dst.total_bytes() {
        d.ok = false;
        d.reasons.push(format!(
            "总字节 src={} dst={}",
            src.total_bytes(),
            dst.total_bytes()
        ));
    }
    let smap: HashMap<&str, &Entry> = src.entries.iter().map(|e| (e.rel.as_str(), e)).collect();
    for ent in &dst.entries {
        let Some(s) = smap.get(ent.rel.as_str()) else {
            d.ok = false;
            d.reasons.push(format!("目标多出 {}", ent.rel));
            d.bad_dst_rels.push(ent.rel.clone());
            continue;
        };
        if s.size != ent.size {
            d.ok = false;
            d.reasons.push(format!("大小不一致 {}", ent.rel));
            d.bad_dst_rels.push(ent.rel.clone());
            continue;
        }
        if check_hash && !s.hash.is_empty() && s.hash != ent.hash {
            d.ok = false;
            d.reasons.push(format!("哈希不一致 {}", ent.rel));
            d.bad_dst_rels.push(ent.rel.clone());
        }
    }
    d
}

/// 复制期间源是否变化（在「移动源 → 写索引」之前最后一道防线）。
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

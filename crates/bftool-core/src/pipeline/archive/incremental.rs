//! 增量索引：只有已校验的路径/内容快照和仍可验证的备份才能作为跳过基线。
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::engine::paths;

use super::types::FolderStats;

/// 增量跳过（指纹未变）原因前缀。
pub const SKIP_UNCHANGED: &str = "已备份且未变，增量跳过";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub size: u64,
    pub mtime_secs: Option<i64>,
    pub sha256: Option<String>,
    pub file_count: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    New,
    Unchanged,
    ContentChanged,
    MetadataOnly,
    DeletedAtSource,
}

/// 兼容原配置名称。为保证路径/内容快照可信，所有模式均完整校验候选内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncrementalVerifyMode {
    /// 默认模式：完整比对可信旧快照与当前源
    #[default]
    OnSuspect,
    /// 每轮完整内容确认（目前与默认模式同样严格）
    Always,
    /// 保留配置兼容性；不能关闭安全跳过所必需的内容确认
    Never,
}

impl IncrementalVerifyMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" => Self::Always,
            "never" => Self::Never,
            _ => Self::OnSuspect,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexRecord {
    source_id: String,
    rel_path: String,
    size: u64,
    mtime_secs: Option<i64>,
    sha256: Option<String>,
    file_count: Option<u64>,
    last_archived_at: String,
    dest_drive_id: String,
    dest_name: String,
    verify_status: String,
    #[serde(default)]
    snapshot_version: u32,
    #[serde(default)]
    backup_root: String,
}

/// JSONL 增量索引（内存 map；写时 rewrite 全文件以保证简洁可靠）。
#[derive(Debug, Default)]
pub struct JsonlIncrementalIndex {
    path: PathBuf,
    /// key = "source_id\0rel"
    entries: HashMap<String, IndexRecord>,
}

fn key(source_id: &str, rel: &str) -> String {
    format!("{source_id}\0{rel}")
}

impl JsonlIncrementalIndex {
    pub fn open_for_source(system_root: &Path, source_root: &Path) -> Result<Self> {
        let dir = paths::system_incremental_dir(system_root);
        let sid = source_id_for(source_root);
        let path = dir.join(format!("{sid}.jsonl"));
        let mut idx = Self {
            path,
            entries: HashMap::new(),
        };
        idx.load()?;
        Ok(idx)
    }

    fn load(&mut self) -> Result<()> {
        self.entries.clear();
        if !self.path.is_file() {
            return Ok(());
        }
        let f = crate::engine::destination::SafeDir::open(
            self.path.parent().context("增量索引缺少父目录")?,
            false,
        )?
        .read_regular(Path::new(
            self.path.file_name().context("增量索引缺少文件名")?,
        ))
        .with_context(|| format!("安全读增量索引失败：{}", self.path.display()))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let rec: IndexRecord = serde_json::from_str(line)
                .with_context(|| format!("解析增量索引行失败：{}", self.path.display()))?;
            self.entries.insert(key(&rec.source_id, &rec.rel_path), rec);
        }
        Ok(())
    }

    pub fn get(&self, source_id: &str, rel: &str) -> Option<Fingerprint> {
        self.entries.get(&key(source_id, rel)).map(|r| Fingerprint {
            size: r.size,
            mtime_secs: r.mtime_secs,
            sha256: r.sha256.clone(),
            file_count: r.file_count,
        })
    }

    pub fn upsert(
        &mut self,
        source_id: &str,
        rel: &str,
        fp: Fingerprint,
        destination: (&str, &str),
        verification: (&str, &str),
    ) {
        let (dest_drive, dest_name) = destination;
        let (verify_status, archived_at) = verification;
        self.entries.insert(
            key(source_id, rel),
            IndexRecord {
                source_id: source_id.to_string(),
                rel_path: rel.to_string(),
                size: fp.size,
                mtime_secs: fp.mtime_secs,
                sha256: fp.sha256,
                file_count: fp.file_count,
                last_archived_at: archived_at.to_string(),
                dest_drive_id: dest_drive.to_string(),
                dest_name: dest_name.to_string(),
                verify_status: verify_status.to_string(),
                snapshot_version: 0,
                backup_root: String::new(),
            },
        );
    }

    pub fn flush(&self) -> Result<()> {
        let parent = self.path.parent().context("增量索引缺少父目录")?;
        let directory = crate::engine::destination::SafeDir::open(parent, true)?;
        let mut records: Vec<_> = self.entries.values().collect();
        records.sort_by(|a, b| (&a.source_id, &a.rel_path).cmp(&(&b.source_id, &b.rel_path)));
        let mut buf = Vec::new();
        for rec in records {
            serde_json::to_writer(&mut buf, rec)?;
            buf.push(b'\n');
        }
        directory
            .write_atomic(
                Path::new(self.path.file_name().context("增量索引缺少文件名")?),
                &buf,
            )
            .with_context(|| format!("写增量索引失败：{}", self.path.display()))?;
        Ok(())
    }
    /// 历史 catalog 没有源身份或内容快照，不能认证当前源。保留兼容入口但不写索引。
    #[cfg(test)]
    pub fn seed_from_global_catalog(
        &mut self,
        _system_root: &Path,
        _source_root: &Path,
        _source_id: &str,
    ) -> Result<usize> {
        Ok(0)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn upsert_verified(
        &mut self,
        source_id: &str,
        rel: &str,
        fp: Fingerprint,
        drive_root: &Path,
        dest_drive: &str,
        dest_name: &str,
        verify_status: &str,
        archived_at: &str,
    ) -> Result<()> {
        anyhow::ensure!(fp.sha256.is_some(), "增量成功快照缺少内容哈希");
        self.upsert(
            source_id,
            rel,
            fp,
            (dest_drive, dest_name),
            (verify_status, archived_at),
        );
        let rec = self
            .entries
            .get_mut(&key(source_id, rel))
            .expect("inserted record");
        rec.snapshot_version = 1;
        rec.backup_root = drive_root.canonicalize()?.to_string_lossy().into_owned();
        Ok(())
    }

    /// 读并重新校验盘身份、清单及备份内容。旧格式和不可用副本都只是不可信候选。
    pub fn verified_fingerprint(
        &self,
        source_id: &str,
        rel: &str,
        drive_root: &Path,
    ) -> Result<Option<Fingerprint>> {
        let Some(rec) = self.entries.get(&key(source_id, rel)) else {
            return Ok(None);
        };
        if rec.snapshot_version != 1 || rec.sha256.is_none() || rec.verify_status != "SHA256-OK" {
            return Ok(None);
        }
        let Ok(root) = drive_root.canonicalize() else {
            return Ok(None);
        };
        if root.to_string_lossy() != rec.backup_root {
            return Ok(None);
        }
        let id_path = paths::drive_id_path(&root);
        if fs::read_to_string(&id_path)
            .map(|s| s.trim() != rec.dest_drive_id)
            .unwrap_or(true)
        {
            return Ok(None);
        }
        if !safe_component(&rec.dest_name) {
            return Ok(None);
        }
        let payload = paths::drive_projects_dir(&root).join(&rec.dest_name);
        let manifest_path =
            paths::drive_manifest_dir(&root).join(format!("{}.sha256.csv", rec.dest_name));
        if !trusted_tree_under(&root, &payload)? || !trusted_tree_under(&root, &manifest_path)? {
            return Ok(None);
        }
        if !super::catalog::catalog_has_project(&paths::drive_catalog_path(&root), &rec.dest_name)?
        {
            return Ok(None);
        }
        let stored = read_verified_manifest(&manifest_path)?;
        let stored_fp = fingerprint_from_manifest(&stored)?;
        let backup = crate::engine::destination::SafeDir::open(&payload, false)?;
        let actual = crate::engine::manifest::build_guarded(
            &backup,
            crate::engine::manifest::ManifestOpts { no_hash: false },
            &crate::reporter::NoopReporter,
        )?;
        let actual_fp = fingerprint_from_manifest(&actual)?;
        if stored_fp.sha256 != rec.sha256
            || actual_fp.sha256 != rec.sha256
            || stored_fp.size != rec.size
            || stored_fp.file_count != rec.file_count
        {
            return Ok(None);
        }
        Ok(self.get(source_id, rel))
    }
}

pub fn source_id_for(source_root: &Path) -> String {
    let s = source_root
        .canonicalize()
        .unwrap_or_else(|_| source_root.to_path_buf())
        .to_string_lossy()
        .to_string();
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex_lower(&h.finalize()[..8]) // 短 hash 作文件名
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn fingerprint_from_stats(stats: &FolderStats) -> Fingerprint {
    Fingerprint {
        size: stats.bytes,
        mtime_secs: stats.latest_mtime_secs,
        sha256: None,
        file_count: Some(stats.files),
    }
}

/// 聚合统计提供候选分类；Unchanged 不是成功证明，必须经过完整内容裁决。
pub fn classify(old: Option<&Fingerprint>, new: &Fingerprint) -> ChangeKind {
    let Some(old) = old else {
        return ChangeKind::New;
    };
    if new.mtime_secs.is_none() {
        return ChangeKind::ContentChanged;
    }
    let size_ok = old.size == new.size;
    let count_ok = old.file_count == new.file_count;
    let mtime_ok = old.mtime_secs == new.mtime_secs;
    if size_ok && count_ok && mtime_ok {
        return ChangeKind::Unchanged;
    }
    if size_ok && count_ok && !mtime_ok {
        return ChangeKind::MetadataOnly;
    }
    ChangeKind::ContentChanged
}

#[cfg(test)]
pub fn change_is_skip(kind: ChangeKind) -> bool {
    matches!(kind, ChangeKind::Unchanged)
}

/// 只读的增量裁决。计划永不刷新成功基线。
pub struct IncrementalResolve {
    pub skip: bool,
}

pub fn resolve_incremental(
    kind: ChangeKind,
    old: Option<&Fingerprint>,
    _new_fp: &Fingerprint,
    proj: &Path,
    _mode: IncrementalVerifyMode,
) -> Result<IncrementalResolve> {
    // 聚合统计只能提示可疑项，不能证明未变；所有模式都必须比对可信旧内容快照。
    let Some(old_hash) = old.and_then(|o| o.sha256.as_ref()) else {
        return Ok(IncrementalResolve { skip: false });
    };
    if matches!(kind, ChangeKind::New | ChangeKind::DeletedAtSource) {
        return Ok(IncrementalResolve { skip: false });
    }
    let current = project_content_hash(proj)?;
    Ok(IncrementalResolve {
        skip: old_hash.eq_ignore_ascii_case(&current),
    })
}

pub fn project_content_hash(root: &Path) -> Result<String> {
    let snapshot = crate::engine::manifest::build(
        root,
        crate::engine::manifest::ManifestOpts { no_hash: false },
        &crate::reporter::NoopReporter,
    )?;
    fingerprint_from_manifest(&snapshot)?
        .sha256
        .context("清单没有内容哈希")
}

/// 由已经通过源/目的内容比对的清单派生基线，绝不重新读取源。
/// 长度前缀绑定路径、文件大小与内容，避免路径变动及拼接歧义。
pub(super) fn fingerprint_from_manifest(
    snapshot: &crate::engine::manifest::Manifest,
) -> Result<Fingerprint> {
    let mut entries: Vec<_> = snapshot.entries.iter().collect();
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));
    let mut hasher = Sha256::new();
    hasher.update(b"bftool-verified-manifest-v1\0");
    let mut seen = std::collections::HashSet::new();
    let mut latest = None;
    for entry in entries {
        let rel = entry.rel.replace('\\', "/");
        anyhow::ensure!(
            !rel.is_empty()
                && !rel.starts_with('/')
                && !rel
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == "..")
                && !rel.contains(':')
                && seen.insert(rel.clone()),
            "清单含不安全或重复相对路径"
        );
        let hash = entry.hash.as_deref().context("清单缺少内容哈希")?;
        anyhow::ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "清单内容哈希格式无效"
        );
        hasher.update((rel.len() as u64).to_le_bytes());
        hasher.update(rel.as_bytes());
        hasher.update(entry.size.to_le_bytes());
        hasher.update(hash.to_ascii_uppercase().as_bytes());
        if let Ok(mtime) = chrono::DateTime::parse_from_rfc3339(&entry.mtime) {
            let secs = mtime.timestamp();
            latest = Some(latest.map_or(secs, |prev: i64| prev.max(secs)));
        }
    }
    Ok(Fingerprint {
        size: snapshot.total_bytes(),
        mtime_secs: latest,
        sha256: Some(hex_lower(&hasher.finalize()).to_ascii_uppercase()),
        file_count: Some(snapshot.count() as u64),
    })
}

pub(super) fn safe_component(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', ':'])
}

/// Read-only trust check. Every existing component must stay under this physical root and be non-link.
pub(super) fn trusted_tree_under(root: &Path, target: &Path) -> Result<bool> {
    let Ok(rel) = target.strip_prefix(root) else {
        return Ok(false);
    };
    let mut path = root.to_path_buf();
    for component in rel.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Ok(false);
        }
        path.push(component.as_os_str());
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return Ok(false);
        };
        if meta.file_type().is_symlink() {
            return Ok(false);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Ok(false);
            }
        }
    }
    if target.is_dir() {
        for entry in walkdir::WalkDir::new(target).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                return Ok(false);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if entry.metadata()?.file_attributes() & 0x400 != 0 {
                    return Ok(false);
                }
            }
        }
    }
    Ok(target.canonicalize()?.starts_with(root))
}

pub(super) fn read_verified_manifest(path: &Path) -> Result<crate::engine::manifest::Manifest> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(rename = "Rel")]
        rel: String,
        #[serde(rename = "Size")]
        size: u64,
        #[serde(rename = "Hash")]
        hash: String,
    }
    let mut entries = Vec::new();
    let directory =
        crate::engine::destination::SafeDir::open(path.parent().context("清单缺少父目录")?, false)?;
    let file = directory.read_regular(Path::new(path.file_name().context("清单缺少文件名")?))?;
    let mut rdr = csv::Reader::from_reader(file);
    for row in rdr.deserialize::<Row>() {
        let row = row?;
        entries.push(crate::engine::manifest::Entry {
            rel: row.rel,
            size: row.size,
            hash: Some(row.hash),
            mtime: String::new(),
        });
    }
    anyhow::ensure!(!entries.is_empty(), "备份清单为空");
    let snapshot = crate::engine::manifest::Manifest { entries };
    fingerprint_from_manifest(&snapshot)?;
    Ok(snapshot)
}

/// 规范化扩展名列表：去点、小写。
pub fn normalize_exts(exts: &[String]) -> Vec<String> {
    exts.iter()
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

/// 项目内是否至少有一个匹配扩展名的真实文件。
pub fn project_has_ext(proj: &Path, exts: &[String]) -> bool {
    if exts.is_empty() {
        return true;
    }
    for entry in crate::engine::cruft::walk(proj).flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if let Some(dot) = name.rfind('.') {
            let ext = name[dot + 1..].to_ascii_lowercase();
            if exts.iter().any(|e| e == &ext) {
                return true;
            }
        }
    }
    false
}

/// 在 source_root 下按扩展名/简易 glob 发现文件。
pub fn discover_matching_files(
    source_root: &Path,
    exts: &[String],
    globs: &[String],
    recursive: bool,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !source_root.is_dir() {
        return Ok(out);
    }
    let walker = if recursive {
        walkdir::WalkDir::new(source_root).follow_links(false)
    } else {
        walkdir::WalkDir::new(source_root)
            .follow_links(false)
            .max_depth(1)
    };
    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy();
        let mut ok = false;
        if !exts.is_empty() {
            if let Some(dot) = name.rfind('.') {
                let ext = name[dot + 1..].to_ascii_lowercase();
                if exts.iter().any(|e| e == &ext) {
                    ok = true;
                }
            }
        }
        if !ok && !globs.is_empty() {
            let rel = path
                .strip_prefix(source_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            for g in globs {
                let g = g.replace('\\', "/");
                // 简易：后缀匹配 **/*.ext 或 *.ext
                if let Some(suf) = g.strip_prefix("**/") {
                    if rel.ends_with(
                        suf.trim_start_matches('*')
                            .trim_start_matches('.')
                            .trim_start_matches('*'),
                    ) || name.ends_with(suf.trim_start_matches('*'))
                    {
                        ok = true;
                        break;
                    }
                } else if let Some(suf) = g.strip_prefix("*.") {
                    if name.to_ascii_lowercase().ends_with(&format!(".{suf}")) {
                        ok = true;
                        break;
                    }
                } else if rel.contains(g.trim_matches('*')) {
                    ok = true;
                    break;
                }
            }
        }
        if ok {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_new_unchanged_changed() {
        let fp = Fingerprint {
            size: 10,
            mtime_secs: Some(100),
            sha256: None,
            file_count: Some(2),
        };
        assert_eq!(classify(None, &fp), ChangeKind::New);
        assert_eq!(classify(Some(&fp), &fp), ChangeKind::Unchanged);
        let mut other = fp.clone();
        other.size = 11;
        assert_eq!(classify(Some(&fp), &other), ChangeKind::ContentChanged);
        let mut meta = fp.clone();
        meta.mtime_secs = Some(200);
        assert_eq!(classify(Some(&fp), &meta), ChangeKind::MetadataOnly);
        assert!(!change_is_skip(ChangeKind::MetadataOnly));
        assert!(change_is_skip(ChangeKind::Unchanged));
        // 即使 never，缺少可信旧哈希也必须重新归档
        let r = resolve_incremental(
            ChangeKind::MetadataOnly,
            Some(&fp),
            &meta,
            Path::new("."),
            IncrementalVerifyMode::Never,
        )
        .unwrap();
        assert!(!r.skip);
    }

    #[test]
    fn jsonl_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let sys = d.path().join("sys");
        let src = d.path().join("src");
        fs::create_dir_all(&sys).unwrap();
        fs::create_dir_all(&src).unwrap();
        let mut idx = JsonlIncrementalIndex::open_for_source(&sys, &src).unwrap();
        let sid = source_id_for(&src);
        idx.upsert(
            &sid,
            "projA",
            Fingerprint {
                size: 3,
                mtime_secs: Some(1),
                sha256: None,
                file_count: Some(1),
            },
            ("备份1", "projA"),
            ("SHA256-OK", "2026-10-02"),
        );
        idx.flush().unwrap();
        let idx2 = JsonlIncrementalIndex::open_for_source(&sys, &src).unwrap();
        let got = idx2.get(&sid, "projA").unwrap();
        assert_eq!(got.size, 3);
        assert_eq!(got.file_count, Some(1));
    }
}

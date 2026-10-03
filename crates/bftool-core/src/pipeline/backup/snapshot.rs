use super::types::*;
use crate::engine::destination::{file_identity, SafeDir};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub(super) struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Backup cancelled")
    }
}
impl std::error::Error for Cancelled {}
pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    Ok(())
}
pub(super) fn safe_relative(path: &Path, empty: bool) -> Result<()> {
    if path.as_os_str().is_empty() {
        if empty {
            return Ok(());
        }
        bail!("Empty payload path");
    }
    for c in path.components() {
        let Component::Normal(name) = c else {
            bail!("Unsafe relative path: {}", path.display());
        };
        let s = name
            .to_str()
            .context("Non-Unicode payload name is not portable")?;
        let stem = s
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end()
            .to_ascii_uppercase();
        let device = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
        ) || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && matches!(stem.as_bytes()[3], b'1'..=b'9' | 0xB9 | 0xB2 | 0xB3));
        if s.chars()
            .any(|ch| ch.is_control() || "\\/:*?\"<>|".contains(ch))
            || s.ends_with(['.', ' '])
            || device
        {
            bail!("Unsafe Windows payload name: {s}");
        }
    }
    Ok(())
}
pub(super) fn key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}
pub(super) fn modified(file: &File) -> Result<String> {
    Ok(file
        .metadata()?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos()
        .to_string())
}
pub(super) fn hash(file: &mut File, cancel: &AtomicBool) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        check_cancel(cancel)?;
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    check_cancel(cancel)?;
    Ok(format!("{:X}", h.finalize()))
}
pub(super) fn file_entry(
    root: &SafeDir,
    read_path: &Path,
    relative_path: PathBuf,
    cancel: &AtomicBool,
) -> Result<BackupEntry> {
    let mut f = root.read_regular(read_path)?;
    let identity = file_identity(&f)?;
    let bytes = f.metadata()?.len();
    let mt = modified(&f)?;
    let sha256 = hash(&mut f, cancel)?;
    if file_identity(&f)? != identity || f.metadata()?.len() != bytes || modified(&f)? != mt {
        bail!("Source changed while hashing");
    }
    // The visible entry must still be this exact object, even on platforms where rename is allowed.
    let visible = root.read_regular(read_path)?;
    if file_identity(&visible)? != identity {
        bail!("Source identity changed while hashing");
    }
    Ok(BackupEntry {
        relative_path,
        kind: BackupEntryKind::File,
        bytes,
        sha256: Some(sha256),
        identity,
        modified: Some(mt),
    })
}
fn visit(dir: &SafeDir, rel: &Path, cancel: &AtomicBool, out: &mut Vec<BackupEntry>) -> Result<()> {
    check_cancel(cancel)?;
    out.push(BackupEntry {
        relative_path: rel.to_path_buf(),
        kind: BackupEntryKind::Directory,
        bytes: 0,
        sha256: None,
        identity: dir.identity()?,
        modified: None,
    });
    let mut names = dir.list_entries()?;
    names.sort();
    let mut seen = BTreeSet::new();
    for name in names {
        check_cancel(cancel)?;
        let leaf = PathBuf::from(name);
        safe_relative(&leaf, false)?;
        if !seen.insert(key(&leaf)) {
            bail!("Case-colliding source entries");
        }
        let child = rel.join(&leaf);
        match dir.open_existing_dir(&leaf) {
            Ok(sub) => visit(&sub, &child, cancel, out)?,
            Err(_) => out.push(file_entry(dir, &leaf, child, cancel)?),
        }
    }
    dir.require_current_binding()?;
    Ok(())
}
pub(super) fn snapshot(
    root: &SafeDir,
    leaf: Option<&Path>,
    cancel: &AtomicBool,
) -> Result<Vec<BackupEntry>> {
    root.require_current_binding()?;
    let mut entries = Vec::new();
    if let Some(leaf) = leaf {
        entries.push(file_entry(root, leaf, PathBuf::new(), cancel)?);
    } else {
        visit(root, Path::new(""), cancel, &mut entries)?;
    }
    entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(entries)
}
pub(super) fn source_path(plan: &BackupPlan, entry: &BackupEntry) -> PathBuf {
    plan.source_leaf
        .clone()
        .unwrap_or_else(|| entry.relative_path.clone())
}
pub(super) fn revalidate(plan: &BackupPlan, cancel: &AtomicBool) -> Result<()> {
    plan.target.require_current_binding()?;
    plan.source.require_current_binding()?;
    if plan.target.identity()? != plan.target_id || plan.source.identity()? != plan.source_id {
        bail!("Source or target identity changed");
    }
    if let Some(held) = &plan.selected_file_handle {
        if plan.view.entries.first().map(|e| e.identity.as_str())
            != Some(file_identity(held)?.as_str())
        {
            bail!("Selected source file identity pin mismatch");
        }
    }
    if snapshot(&plan.source, plan.source_leaf.as_deref(), cancel)? != plan.view.entries {
        bail!("Source tree changed since preview");
    }
    Ok(())
}

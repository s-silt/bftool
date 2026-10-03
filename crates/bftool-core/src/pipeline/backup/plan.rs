use super::{snapshot::*, types::*};
use crate::engine::destination::{file_identity, SafeDir};
use crate::reporter::Reporter;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
static NEXT_JOB: AtomicU64 = AtomicU64::new(0);

fn ancestor_contains(path: &Path, identity: &str) -> Result<bool> {
    for ancestor in path.ancestors() {
        if SafeDir::open(ancestor, false)?.identity()? == identity {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn plan_backup(
    request: &BackupRequest,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<BackupPlan> {
    reporter.info("Planning direct backup");
    check_cancel(cancel)?;
    let options = match request.source {
        SourceSelection::File(_) => DirectoryOptions::default(),
        SourceSelection::Directory(_) => request.effective_directory_options().canonicalized()?,
    };
    // Open the original paths first so canonicalization cannot hide a symlink ancestor.
    let target = SafeDir::open(&request.target_dir, false)?;
    let path = request.source.path();
    let name = path
        .file_name()
        .context("Source must have a selected item name")?;
    let name = PathBuf::from(name);
    safe_relative(&name, false)?;
    if key(&name) == ".bftool-backup" {
        bail!("Selected name is reserved for backup metadata");
    }
    let (source, source_leaf) = match &request.source {
        SourceSelection::File(p) => (
            SafeDir::open(p.parent().context("Source has no parent")?, false)?,
            Some(name.clone()),
        ),
        SourceSelection::Directory(p) => (SafeDir::open(p, false)?, None),
    };
    let selected_file_handle = source_leaf
        .as_ref()
        .map(|leaf| source.hold_file_identity(leaf))
        .transpose()?;
    let target_path = std::fs::canonicalize(&request.target_dir)?;
    let source_path = std::fs::canonicalize(path)?;
    let source_root_path = if source_leaf.is_some() {
        source_path.parent().context("Missing source parent")?
    } else {
        source_path.as_path()
    };
    let source_id = source.identity()?;
    let target_id = target.identity()?;
    if (source_leaf.is_none() && ancestor_contains(&target_path, &source_id)?)
        || ancestor_contains(source_root_path, &target_id)?
    {
        bail!("Source and target overlap or share a selected file parent");
    }
    let entries = snapshot(&source, source_leaf.as_deref(), &options, cancel)?;
    if let Some(held) = &selected_file_handle {
        if entries.first().map(|e| e.identity.as_str()) != Some(file_identity(held)?.as_str()) {
            bail!("Selected source file identity changed while planning");
        }
    }
    let names = target.list_entries()?;
    let occupied = |candidate: &Path| names.iter().any(|n| key(Path::new(n)) == key(candidate));
    let mut destination_name = name.clone();
    let mut conflicts = Vec::new();
    if occupied(&destination_name) {
        conflicts.push(name.clone());
        let s = name.to_str().context("Invalid source name")?;
        for version in 1..=1_000_000 {
            let candidate = PathBuf::from(format!("{s} (backup {version})"));
            if !occupied(&candidate) {
                destination_name = candidate;
                break;
            }
            if version == 1_000_000 {
                bail!("No available keep-both name");
            }
        }
    }
    let mut counts = BackupCounts::default();
    let mut bytes = 0u64;
    for e in &entries {
        match e.kind {
            BackupEntryKind::File => {
                counts.files += 1;
                bytes = bytes.checked_add(e.bytes).context("Backup size overflow")?;
            }
            BackupEntryKind::Directory => counts.directories += 1,
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let job_id = format!(
        "job-{}-{stamp}-{}",
        std::process::id(),
        NEXT_JOB.fetch_add(1, Ordering::Relaxed)
    );
    let mut request = request.clone();
    request.directory_options = Some(options.clone());
    request.target_dir = target_path.clone();
    request.source = if source_leaf.is_some() {
        SourceSelection::File(source_path)
    } else {
        SourceSelection::Directory(source_path)
    };
    Ok(BackupPlan {
        request,
        source,
        target,
        source_id,
        target_id,
        source_leaf,
        selected_file_handle,
        view: BackupPlanView {
            job_id,
            destination_name,
            selected_target: target_path,
            entries,
            counts: counts.clone(),
            bytes,
            conflicts,
            issues: if options.filtered() && counts.files == 0 {
                vec!["No files match the selected folder suffix rules".into()]
            } else {
                Vec::new()
            },
        },
    })
}

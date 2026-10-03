use super::{journal::*, plan::plan_backup, snapshot::*, types::*};
use crate::engine::destination::{file_identity, SafeDir};
use crate::reporter::Reporter;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

fn destination_free(target: &SafeDir, name: &Path) -> Result<()> {
    if target
        .list_entries()?
        .iter()
        .any(|n| key(Path::new(n)) == key(name))
    {
        bail!("Planned destination is now occupied; replan required");
    }
    Ok(())
}
pub(super) fn inventory(
    dir: &SafeDir,
    rel: &Path,
    out: &mut BTreeMap<PathBuf, (BackupEntryKind, String)>,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for name in dir.list_entries_checked(|| check_cancel(cancel))? {
        check_cancel(cancel)?;
        let leaf = PathBuf::from(name);
        safe_relative(&leaf, false)?;
        if !seen.insert(key(&leaf)) {
            bail!("Case-colliding payload entries");
        }
        let path = rel.join(&leaf);
        if let Ok(child) = dir.open_existing_dir(&leaf) {
            out.insert(
                path.clone(),
                (BackupEntryKind::Directory, child.identity()?),
            );
            inventory(&child, &path, out, cancel)?;
        } else {
            let f = dir.read_regular(&leaf)?;
            out.insert(path, (BackupEntryKind::File, file_identity(&f)?));
        }
    }
    dir.require_current_binding()?;
    Ok(())
}
fn stage_owned(stage: &SafeDir, journal: &Journal, cancel: &AtomicBool) -> Result<()> {
    let mut actual = BTreeMap::new();
    inventory(stage, Path::new(""), &mut actual, cancel)?;
    let mut expected = BTreeMap::new();
    for (p, id) in &journal.directories {
        expected.insert(
            Path::new("payload").join(p),
            (BackupEntryKind::Directory, id.clone()),
        );
    }
    for (p, r) in &journal.completed {
        expected.insert(
            Path::new("payload").join(p),
            (BackupEntryKind::File, r.identity.clone()),
        );
    }
    if actual != expected {
        bail!("Unknown, missing or substituted staging entries; preserved for manual inspection");
    }
    Ok(())
}
fn verify_staged_hashes(
    stage: &SafeDir,
    m: &Manifest,
    j: &Journal,
    cancel: &AtomicBool,
) -> Result<()> {
    for e in m.entries.iter().filter(|e| e.kind == BackupEntryKind::File) {
        let mut f = stage.read_regular(&payload_path(e))?;
        let receipt = j
            .completed
            .get(&e.relative_path)
            .context("Missing staged ownership receipt")?;
        if file_identity(&f)? != receipt.identity
            || f.metadata()?.len() != e.bytes
            || Some(hash(&mut f, cancel)?) != e.sha256
        {
            bail!("Staging content changed before publication");
        }
    }
    Ok(())
}
fn map_preflight_cancellation(result: Result<BackupSummary>) -> Result<BackupSummary> {
    match result {
        Err(error) if error.downcast_ref::<Cancelled>().is_some() => {
            let mut summary = BackupSummary::new();
            summary.outcome = BackupOutcome::Cancelled;
            Ok(summary)
        }
        other => other,
    }
}
pub(super) fn assert_published(
    target: &SafeDir,
    m: &Manifest,
    j: &Journal,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut actual = BTreeMap::new();
    match m.request.source {
        SourceSelection::File(_) => {
            let f = target.read_regular(&m.destination_name)?;
            actual.insert(PathBuf::new(), (BackupEntryKind::File, file_identity(&f)?));
        }
        SourceSelection::Directory(_) => {
            let root = target.open_existing_dir(&m.destination_name)?;
            actual.insert(
                PathBuf::new(),
                (BackupEntryKind::Directory, root.identity()?),
            );
            inventory(&root, Path::new(""), &mut actual, cancel)?;
        }
    }
    let mut expected = BTreeMap::new();
    for (p, id) in &j.directories {
        expected.insert(p.clone(), (BackupEntryKind::Directory, id.clone()));
    }
    for (p, r) in &j.completed {
        expected.insert(p.clone(), (BackupEntryKind::File, r.identity.clone()));
    }
    if actual != expected || actual.get(Path::new("")).map(|x| &x.1) != j.payload_id.as_ref() {
        bail!("Published payload identity or exact contents do not match owned job");
    }
    for e in m.entries.iter().filter(|e| e.kind == BackupEntryKind::File) {
        let mut f = target.read_regular(&m.destination_name.join(&e.relative_path))?;
        if f.metadata()?.len() != e.bytes || Some(hash(&mut f, cancel)?) != e.sha256 {
            bail!("Published payload verification failed");
        }
    }
    Ok(())
}
pub fn run_backup_plan(
    plan: &BackupPlan,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<BackupSummary> {
    map_preflight_cancellation((|| {
        if cancel.load(Ordering::Relaxed) {
            let mut summary = BackupSummary::new();
            summary.outcome = BackupOutcome::Cancelled;
            return Ok(summary);
        }
        revalidate(plan, cancel)?;
        destination_free(&plan.target, &plan.view.destination_name)?;
        check_cancel(cancel)?;
        let (job, stage, m, j) = create_job(plan, cancel)?;
        execute(plan, &job, &stage, &m, j, cancel, reporter)
    })())
}
pub fn resume_backup(
    target: &Path,
    job_id: &str,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<BackupSummary> {
    map_preflight_cancellation((|| {
        if cancel.load(Ordering::Relaxed) {
            let mut summary = BackupSummary::new();
            summary.outcome = BackupOutcome::Cancelled;
            return Ok(summary);
        }
        let target_handle = SafeDir::open(target, false)?;
        let (job, stage, m, j) = load_job(&target_handle, job_id, cancel)?;
        let mut plan = plan_backup(&m.request, cancel, reporter)?;
        if plan.source_id != m.source_id
            || plan.target_id != m.target_id
            || plan.view.entries != m.entries
        {
            bail!("Recovery source/target identity or contents changed");
        }
        plan.view.job_id = m.job_id.clone();
        plan.view.destination_name = m.destination_name.clone();
        execute(&plan, &job, &stage, &m, j, cancel, reporter)
    })())
}
fn execute(
    plan: &BackupPlan,
    job: &SafeDir,
    stage: &SafeDir,
    m: &Manifest,
    _journal: Journal,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<BackupSummary> {
    let mut summary = BackupSummary::new();
    let result = (|| -> Result<()> {
        let _lock = job.lock_exclusive(Path::new("job.lock"))?;
        // Reload the immutable chain under the cooperative execution lock.
        let (mut journal, mut journal_log) = read_journal(job, m, cancel)?;
        validate(m, &journal, cancel)?;
        revalidate(plan, cancel)?;
        stage.require_current_binding()?;
        job.require_current_binding()?;
        if journal.state == JobState::Completed {
            require_empty_stage(stage, cancel)?;
            assert_published(&plan.target, m, &journal, cancel)?;
            summary.skipped_verified = journal.completed.len() as u64;
            summary.verified = summary.skipped_verified;
            summary.bytes = m.bytes;
            journal_log.require_current(job, cancel)?;
            summary.published = true;
            return Ok(());
        }
        if journal.state == JobState::Publishing && stage.list_entries()?.is_empty() {
            assert_published(&plan.target, m, &journal, cancel)?;
            summary.skipped_verified = journal.completed.len() as u64;
            summary.verified = summary.skipped_verified;
            summary.bytes = m.bytes;
            // Completion is recorded as a compact immutable transition.
            journal_log.append(job, &mut journal, Delta::Completed, cancel)?;
            journal_log.require_current(job, cancel)?;
            summary.published = true;
            return Ok(());
        }
        destination_free(&plan.target, &m.destination_name)?;
        stage_owned(stage, &journal, cancel)?;
        reporter.info("Copying direct backup");
        journal_log.require_current(job, cancel)?;
        let mut progress = reporter.progress_bytes("Copying direct backup", m.bytes);
        for e in &m.entries {
            check_cancel(cancel)?;
            let dest = payload_path(e);
            if e.kind == BackupEntryKind::Directory {
                if !journal.directories.contains_key(&e.relative_path) {
                    let dir = stage.create_new_dir(&dest)?;
                    journal_log.append(
                        job,
                        &mut journal,
                        Delta::Directory {
                            path: e.relative_path.clone(),
                            identity: dir.identity()?,
                        },
                        cancel,
                    )?;
                }
                continue;
            }
            if let Some(receipt) = journal.completed.get(&e.relative_path) {
                let mut f = stage.read_regular(&dest)?;
                if file_identity(&f)? != receipt.identity
                    || f.metadata()?.len() != e.bytes
                    || hash(&mut f, cancel)? != receipt.sha256
                {
                    bail!("Completed staged file no longer matches receipt");
                }
                summary.skipped_verified += 1;
                summary.verified += 1;
                summary.bytes += e.bytes;
                progress.inc(e.bytes);
                continue;
            }
            let path = source_path(plan, e);
            let mut source = plan.source.read_regular(&path)?;
            if file_identity(&source)? != e.identity
                || source.metadata()?.len() != e.bytes
                || modified(&source)? != e.modified.as_deref().context("Missing source mtime")?
                || Some(hash(&mut source, cancel)?) != e.sha256
            {
                bail!("Source changed before copy");
            }
            let copied_hash =
                stage.copy_from_handle_new(&mut source, &dest, cancel, progress.as_mut())?;
            if Some(&copied_hash) != e.sha256.as_ref()
                || Some(hash(&mut source, cancel)?) != e.sha256
                || file_identity(&source)? != e.identity
                || source.metadata()?.len() != e.bytes
                || modified(&source)? != e.modified.as_deref().context("Missing source mtime")?
            {
                bail!("Source changed during copy");
            }
            let mut copied = stage.read_regular(&dest)?;
            if copied.metadata()?.len() != e.bytes || Some(hash(&mut copied, cancel)?) != e.sha256 {
                bail!("Independent destination verification failed");
            }
            let receipt = Receipt {
                identity: file_identity(&copied)?,
                sha256: copied_hash,
            };
            summary.copied += 1;
            summary.verified += 1;
            summary.bytes += e.bytes;
            journal_log.append(
                job,
                &mut journal,
                Delta::File {
                    path: e.relative_path.clone(),
                    receipt,
                },
                cancel,
            )?;
        }
        progress.finish();
        reporter.info("Verifying direct backup");
        stage_owned(stage, &journal, cancel)?;
        // A second independent read of every completed file precedes publication.
        verify_staged_hashes(stage, m, &journal, cancel)?;
        revalidate(plan, cancel)?;
        let payload_id = match m.request.source {
            SourceSelection::File(_) => file_identity(&stage.read_regular(Path::new("payload"))?)?,
            SourceSelection::Directory(_) => {
                stage.open_existing_dir(Path::new("payload"))?.identity()?
            }
        };
        journal_log.append(job, &mut journal, Delta::Publishing { payload_id }, cancel)?;
        validate(m, &journal, cancel)?;
        check_cancel(cancel)?;
        destination_free(&plan.target, &m.destination_name)?;
        reporter.info("Publishing direct backup");
        // Reporter callbacks can run application code; validate after the last callback.
        revalidate(plan, cancel)?;
        stage_owned(stage, &journal, cancel)?;
        verify_staged_hashes(stage, m, &journal, cancel)?;
        check_cancel(cancel)?;
        journal_log.require_current(job, cancel)?;
        // All payload/descendant read handles have left scope. The native publish
        // primitive opens and renames the exact expected filesystem object.
        stage.rename_owned_entry_to(
            Path::new("payload"),
            &plan.target,
            &m.destination_name,
            journal
                .payload_id
                .as_deref()
                .context("Missing owned payload identity")?,
        )?;
        assert_published(&plan.target, m, &journal, cancel)?;
        // Retain a failure journal if a concurrent source edit invalidates completion.
        revalidate(plan, cancel)?;
        // Completion is recorded as a compact immutable transition.
        journal_log.append(job, &mut journal, Delta::Completed, cancel)?;
        journal_log.require_current(job, cancel)?;
        summary.published = true;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(summary),
        Err(error)
            if error.downcast_ref::<Cancelled>().is_some()
                || error
                    .chain()
                    .any(|cause| cause.to_string() == "Copy cancelled; source retained") =>
        {
            summary.outcome = BackupOutcome::Cancelled;
            Ok(summary)
        }
        Err(error) => Err(BackupExecutionError::new(summary, error).into()),
    }
}

use super::{journal::*, runner::inventory, snapshot::*, types::*};
use crate::engine::destination::{file_identity, SafeDir};
use crate::engine::verify::{ExtraFile, VerifyIssue, VerifyIssueKind, VerifyReport};
use crate::reporter::Reporter;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

fn issue(report: &mut VerifyReport, project: &str, path: &Path, kind: VerifyIssueKind) {
    report.bad += 1;
    report.issues.push(VerifyIssue {
        project: project.into(),
        rel: path.to_string_lossy().into_owned(),
        kind,
    });
}
fn read_kind(target: &SafeDir, path: &Path) -> VerifyIssueKind {
    match std::fs::symlink_metadata(target.anchored_path().join(path)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => VerifyIssueKind::Missing,
        _ => VerifyIssueKind::ReadError,
    }
}
pub fn verify_backup(
    target: &Path,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
) -> Result<VerifyReport> {
    let mut report = VerifyReport::default();
    match verify_into(target, cancel, reporter, &mut report) {
        Err(error) if error.downcast_ref::<Cancelled>().is_some() => {
            report.cancelled = true;
            Ok(report)
        }
        Err(error) => Err(error),
        Ok(()) => Ok(report),
    }
}
fn verify_into(
    target: &Path,
    cancel: &AtomicBool,
    reporter: &dyn Reporter,
    report: &mut VerifyReport,
) -> Result<()> {
    check_cancel(cancel)?;
    reporter.info("Verifying direct-backup history");
    check_cancel(cancel)?;
    let root = SafeDir::open(target, false)?;
    let jobs = load_jobs(&root, cancel)?;
    if jobs.is_empty() {
        issue(
            report,
            "direct-backup",
            Path::new(""),
            VerifyIssueKind::Unverifiable,
        );
    }
    for (_job, _stage, m, j) in jobs {
        let id = m.job_id.clone();
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        check_cancel(cancel)?;
        if j.state != JobState::Completed {
            issue(
                report,
                &id,
                &m.destination_name,
                VerifyIssueKind::Unverifiable,
            );
            continue;
        }
        let mut actual = BTreeMap::new();
        match &m.request.source {
            SourceSelection::Directory(_) => match root.open_existing_dir(&m.destination_name) {
                Ok(dir) => {
                    actual.insert(
                        PathBuf::new(),
                        (BackupEntryKind::Directory, dir.identity()?),
                    );
                    if let Err(error) = inventory(&dir, Path::new(""), &mut actual, cancel) {
                        if error.downcast_ref::<Cancelled>().is_some() {
                            return Err(error);
                        }
                        issue(report, &id, &m.destination_name, VerifyIssueKind::EnumError);
                    }
                }
                Err(_) => {
                    issue(
                        report,
                        &id,
                        &m.destination_name,
                        read_kind(&root, &m.destination_name),
                    );
                    continue;
                }
            },
            SourceSelection::File(_) => match root.read_regular(&m.destination_name) {
                Ok(f) => {
                    actual.insert(PathBuf::new(), (BackupEntryKind::File, file_identity(&f)?));
                }
                Err(_) => {
                    report.checked += 1;
                    issue(
                        report,
                        &id,
                        &m.destination_name,
                        read_kind(&root, &m.destination_name),
                    );
                    continue;
                }
            },
        }
        if actual.get(Path::new("")).map(|e| &e.1) != j.payload_id.as_ref() {
            issue(
                report,
                &id,
                &m.destination_name,
                VerifyIssueKind::Unverifiable,
            );
        }
        for e in &m.entries {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            let path = m.destination_name.join(&e.relative_path);
            if e.kind == BackupEntryKind::Directory {
                if actual.get(&e.relative_path).map(|x| &x.0) != Some(&BackupEntryKind::Directory) {
                    let kind = if actual.contains_key(&e.relative_path) {
                        VerifyIssueKind::Unverifiable
                    } else {
                        // Partial inventory cannot establish absence.
                        read_kind(&root, &path)
                    };
                    issue(report, &id, &path, kind);
                } else if actual.get(&e.relative_path).map(|x| &x.1)
                    != j.directories.get(&e.relative_path)
                {
                    issue(report, &id, &path, VerifyIssueKind::Unverifiable);
                }
                continue;
            }
            report.checked += 1;
            let mut f = match root.read_regular(&path) {
                Ok(f) => f,
                Err(_) => {
                    issue(report, &id, &path, read_kind(&root, &path));
                    continue;
                }
            };
            let result = (|| -> Result<Option<VerifyIssueKind>> {
                let receipt = j
                    .completed
                    .get(&e.relative_path)
                    .context("Missing completed receipt")?;
                if file_identity(&f)? != receipt.identity {
                    return Ok(Some(VerifyIssueKind::Unverifiable));
                }
                if f.metadata()?.len() != e.bytes {
                    return Ok(Some(VerifyIssueKind::SizeMismatch));
                }
                if Some(hash(&mut f, cancel)?) != e.sha256 {
                    return Ok(Some(VerifyIssueKind::Corrupt));
                }
                // Reopen the visible entry to catch a pathname substitution during hashing.
                if file_identity(&root.read_regular(&path)?)? != receipt.identity {
                    return Ok(Some(VerifyIssueKind::Unverifiable));
                }
                Ok(None)
            })();
            match result {
                Ok(Some(kind)) => issue(report, &id, &path, kind),
                Ok(None) => {}
                Err(error) if error.downcast_ref::<Cancelled>().is_some() => {
                    report.cancelled = true;
                    break;
                }
                Err(_) => issue(report, &id, &path, VerifyIssueKind::ReadError),
            }
        }
        let expected_paths: std::collections::BTreeSet<_> =
            m.entries.iter().map(|e| &e.relative_path).collect();
        for p in actual.keys() {
            check_cancel(cancel)?;
            if !expected_paths.contains(p) {
                report.extra += 1;
                report.extras.push(ExtraFile {
                    project: id.clone(),
                    rel: m.destination_name.join(p).to_string_lossy().into_owned(),
                });
            }
        }
    }
    root.require_current_binding()?;
    Ok(())
}
pub fn list_backup_history(target: &Path) -> Result<Vec<BackupHistoryRecord>> {
    list_backup_history_with_cancel(target, &AtomicBool::new(false))
}
pub fn list_backup_history_with_cancel(
    target: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<BackupHistoryRecord>> {
    check_cancel(cancel)?;
    let root = SafeDir::open(target, false)?;
    let mut records = Vec::new();
    for (_job, _stage, m, j) in load_jobs(&root, cancel)? {
        check_cancel(cancel)?;
        records.push(BackupHistoryRecord {
            job_id: m.job_id,
            source: m.request.source,
            destination_name: m.destination_name,
            bytes: m.bytes,
            completed: j.state == JobState::Completed,
            state: format!("{:?}", j.state),
        });
    }
    check_cancel(cancel)?;
    root.require_current_binding()?;
    Ok(records)
}
#[cfg(test)]
mod cancellation_tests {
    use super::*;
    #[test]
    fn delta_fix_cancelled_history_does_not_read_poisoned_metadata() {
        let world = tempfile::tempdir().unwrap();
        let ns = world.path().join(".bftool-backup");
        std::fs::create_dir(&ns).unwrap();
        std::fs::write(ns.join("owner.json"), b"UNKNOWN-KEEP").unwrap();
        let result = list_backup_history_with_cancel(world.path(), &AtomicBool::new(true));
        assert!(result.unwrap_err().downcast_ref::<Cancelled>().is_some());
        assert_eq!(
            std::fs::read(ns.join("owner.json")).unwrap(),
            b"UNKNOWN-KEEP"
        );
    }
}

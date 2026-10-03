use crate::engine::destination::SafeDir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceSelection {
    File(PathBuf),
    Directory(PathBuf),
}
impl SourceSelection {
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::File(p) | Self::Directory(p) => p,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictPolicy {
    KeepBoth,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupRequest {
    pub source: SourceSelection,
    pub target_dir: PathBuf,
    pub conflict: ConflictPolicy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackupOutcome {
    Completed,
    Cancelled,
    Failed,
}
#[derive(Debug, Clone, Default)]
pub struct BackupCounts {
    pub files: u64,
    pub directories: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackupEntryKind {
    File,
    Directory,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupEntry {
    pub relative_path: PathBuf,
    pub kind: BackupEntryKind,
    pub bytes: u64,
    pub sha256: Option<String>,
    pub identity: String,
    pub modified: Option<String>,
}
#[derive(Debug, Clone)]
pub struct BackupPlanView {
    pub job_id: String,
    pub destination_name: PathBuf,
    pub selected_target: PathBuf,
    pub entries: Vec<BackupEntry>,
    pub counts: BackupCounts,
    pub bytes: u64,
    pub conflicts: Vec<PathBuf>,
    pub issues: Vec<String>,
}
pub struct BackupPlan {
    pub(super) request: BackupRequest,
    pub(super) view: BackupPlanView,
    pub(super) source: SafeDir,
    pub(super) target: SafeDir,
    pub(super) source_id: String,
    pub(super) target_id: String,
    pub(super) source_leaf: Option<PathBuf>,
    // Attributes-only identity pin on Windows permits preview-time edits while
    // keeping the selected object's identity alive until the plan is dropped.
    pub(super) selected_file_handle: Option<std::fs::File>,
}
impl BackupPlan {
    pub fn view(&self) -> &BackupPlanView {
        &self.view
    }
    pub fn request(&self) -> &BackupRequest {
        &self.request
    }
}
impl std::fmt::Debug for BackupPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackupPlan")
            .field("request", &self.request)
            .field("view", &self.view)
            .finish_non_exhaustive()
    }
}
#[derive(Debug, Clone)]
pub struct BackupSummary {
    pub outcome: BackupOutcome,
    /// True only after verified publication and a durable Completed journal.
    /// False does not prove the destination path is absent after an interrupted commit.
    pub published: bool,
    pub copied: u64,
    pub verified: u64,
    pub skipped_verified: u64,
    pub failed: u64,
    pub bytes: u64,
    pub issues: Vec<String>,
}
impl BackupSummary {
    pub(super) fn new() -> Self {
        Self {
            outcome: BackupOutcome::Completed,
            published: false,
            copied: 0,
            verified: 0,
            skipped_verified: 0,
            failed: 0,
            bytes: 0,
            issues: Vec::new(),
        }
    }
}
/// A real execution failure carrying independently verified partial staging results.
/// The source is retained; callers can downcast an anyhow error to inspect this summary.
#[derive(Debug)]
pub struct BackupExecutionError {
    pub summary: BackupSummary,
    cause: anyhow::Error,
}
impl BackupExecutionError {
    pub(super) fn new(mut summary: BackupSummary, cause: anyhow::Error) -> Self {
        summary.outcome = BackupOutcome::Failed;
        summary.failed += 1;
        summary.issues.push(format!("{cause:#}"));
        Self { summary, cause }
    }
}
impl std::fmt::Display for BackupExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Direct backup failed; source retained: {:#}", self.cause)
    }
}
impl std::error::Error for BackupExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}
#[derive(Debug, Clone)]
pub struct BackupHistoryRecord {
    pub job_id: String,
    pub source: SourceSelection,
    pub destination_name: PathBuf,
    pub bytes: u64,
    pub completed: bool,
    pub state: String,
}

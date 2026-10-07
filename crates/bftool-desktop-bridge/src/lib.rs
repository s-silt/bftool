//! UI boundary for the existing copy-only engine. This crate never copies files itself.
use bftool_core::{
    pipeline::backup::*,
    reporter::{LogLevel, ProgressHandle, Reporter},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize)]
pub struct ErrorDto {
    pub code: String,
    pub message: String,
    pub operation: String,
    pub retry: String,
}
impl ErrorDto {
    fn new(code: &str, message: impl ToString, operation: &str) -> Self {
        Self {
            code: code.into(),
            message: message.to_string(),
            operation: operation.into(),
            retry: "检查输入及目标状态后重新预览；保留未完成任务的恢复记录".into(),
        }
    }
}
type Result<T> = std::result::Result<T, ErrorDto>;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    File,
    Directory,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInput {
    pub path: String,
    pub kind: SourceKind,
    pub recursive: bool,
    pub suffixes: String,
    pub include_extensionless: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub revision: String,
    pub target: String,
    pub sources: Vec<SourceInput>,
}
#[derive(Clone, Debug, Serialize)]
pub struct PlanDto {
    pub plan_id: String,
    pub revision: String,
    pub target: String,
    pub files: String,
    pub directories: String,
    pub bytes: String,
    pub entry_count: String,
    pub destinations: Vec<String>,
    pub issues: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct EntryDto {
    pub source: String,
    pub relative_path: String,
    pub kind: String,
    pub bytes: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct ResultDto {
    pub core_job_id: String,
    pub outcome: String,
    pub published: bool,
    pub copied: String,
    pub verified: String,
    pub skipped_verified: String,
    pub failed: String,
    pub bytes: String,
    pub issues: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub job_id: String,
    pub sequence: String,
    pub revision: String,
    pub phase: String,
    pub label: String,
    pub current_bytes: String,
    pub total_bytes: String,
    pub terminal: bool,
    pub cancel_requested: bool,
    pub results: Vec<ResultDto>,
    pub error: Option<ErrorDto>,
}
#[derive(Clone, Debug, Serialize)]
pub struct HistoryDto {
    pub core_job_id: String,
    pub source: String,
    pub destination: String,
    pub bytes: String,
    pub completed: bool,
    pub state: String,
    pub recursive: bool,
    pub recovery_id: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct VerificationDto {
    pub checked: String,
    pub bad: String,
    pub size_only: String,
    pub extras: String,
    pub cancelled: bool,
    pub issues: Vec<String>,
}

struct SavedPlan {
    id: String,
    revision: u64,
    plans: Vec<BackupPlan>,
}
struct SavedRecovery {
    id: String,
    revision: u64,
    target: PathBuf,
    core_job_id: String,
}
struct Job {
    cancel: AtomicBool,
    snapshot: Mutex<Snapshot>,
}
#[derive(Default)]
struct Inner {
    revision: u64,
    closing: bool,
    plan: Option<SavedPlan>,
    recoveries: Vec<SavedRecovery>,
    active: Option<Arc<Job>>,
    last: Option<Arc<Job>>,
}
#[derive(Default, Clone)]
pub struct Engine {
    inner: Arc<Mutex<Inner>>,
}
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str) -> String {
    format!(
        "{prefix}-{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}
fn revision(input: &str) -> Result<u64> {
    input
        .parse::<u64>()
        .ok()
        .filter(|v| v.to_string() == input)
        .ok_or_else(|| ErrorDto::new("INVALID_INPUT", "版本号须为规范十进制整数", "revision"))
}
fn path(input: &str) -> Result<PathBuf> {
    if input.is_empty()
        || input.len() > 32768
        || input.contains('\0')
        || !Path::new(input).is_absolute()
    {
        return Err(ErrorDto::new("INVALID_INPUT", "请选择绝对路径", "validate"));
    }
    Ok(PathBuf::from(input))
}

// Do not canonicalize before SafeDir checks each ancestor. Only change the
// Windows API spelling, so held-object/no-replace publication supports long paths.
fn target_path(input: &str) -> Result<PathBuf> {
    let path = path(input)?;
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let components: Vec<_> = path.components().collect();
        let invalid = || {
            ErrorDto::new(
                "INVALID_INPUT",
                "Windows目标路径包含不支持的设备路径、上级跳转或歧义组件",
                "target",
            )
        };
        // PathBuf::push can normalize ParentDir under a verbatim prefix: reject
        // all original components first, before constructing a new path.
        if components.iter().any(|c| match c {
            Component::ParentDir => true,
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
                name.contains(['/', '\\', ':'])
                    || name.ends_with(['.', ' '])
                    || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    || ((stem.starts_with("COM") || stem.starts_with("LPT"))
                        && stem.len() == 4
                        && matches!(stem.as_bytes()[3], b'1'..=b'9'))
            }
            _ => false,
        }) {
            return Err(invalid());
        }
        let Some(Component::Prefix(prefix)) = components.first() else {
            return Err(invalid());
        };
        if !matches!(components.get(1), Some(Component::RootDir)) {
            return Err(invalid());
        }
        let root = match prefix.kind() {
            Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => {
                format!("\\\\?\\{}:\\", disk as char)
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                let server = server.to_str().ok_or_else(invalid)?;
                let share = share.to_str().ok_or_else(invalid)?;
                if server.contains(['/', '\\', ':'])
                    || share.contains(['/', '\\', ':'])
                    || server.ends_with(['.', ' '])
                    || share.ends_with(['.', ' '])
                {
                    return Err(invalid());
                }
                format!("\\\\?\\UNC\\{server}\\{share}\\")
            }
            _ => return Err(invalid()),
        };
        let mut normalized = PathBuf::from(root);
        for component in components.iter().skip(2) {
            match component {
                Component::Normal(name) => normalized.push(name),
                Component::CurDir => {}
                _ => return Err(invalid()),
            }
        }
        Ok(normalized)
    }
    #[cfg(not(windows))]
    Ok(path)
}
impl Job {
    fn change(&self, f: impl FnOnce(&mut Snapshot)) {
        let mut s = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut s);
        s.sequence = (s.sequence.parse::<u64>().unwrap_or(0).saturating_add(1)).to_string();
    }
    fn terminal(&self, phase: &str, error: Option<ErrorDto>) {
        self.change(|s| {
            s.phase = phase.into();
            s.terminal = true;
            s.error = error;
        });
    }
    fn end_error(&self, error: ErrorDto) {
        // The core cancellation error is private. Preserve its full error rather
        // than infer a successful cancellation from message text or a flag.
        let requested = self.cancel.load(Ordering::Acquire);
        self.terminal(
            if requested {
                "cancel_requested_end"
            } else {
                "failed"
            },
            Some(error),
        );
    }
}
struct Operation {
    inner: Arc<Mutex<Inner>>,
    job: Arc<Job>,
}
impl Drop for Operation {
    fn drop(&mut self) {
        if !self
            .job
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminal
        {
            self.job.terminal(
                "failed",
                Some(ErrorDto::new("WORKER_FAILED", "后台操作异常结束", "worker")),
            );
        }
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .active
            .as_ref()
            .is_some_and(|j| Arc::ptr_eq(j, &self.job))
        {
            state.active = None;
        }
    }
}
fn begin(state: &mut Inner, inner: Arc<Mutex<Inner>>, phase: &str) -> Result<Operation> {
    if state.closing {
        return Err(ErrorDto::new(
            "SHUTTING_DOWN",
            "窗口正在关闭，无法开始新任务",
            phase,
        ));
    }
    if state.active.is_some() {
        return Err(ErrorDto::new("BUSY", "已有后台操作，请等待或取消", phase));
    }
    let job = Arc::new(Job {
        cancel: AtomicBool::new(false),
        snapshot: Mutex::new(Snapshot {
            job_id: id("job"),
            sequence: "0".into(),
            revision: state.revision.to_string(),
            phase: phase.into(),
            label: String::new(),
            current_bytes: "0".into(),
            total_bytes: "0".into(),
            terminal: false,
            cancel_requested: false,
            results: Vec::new(),
            error: None,
        }),
    });
    state.active = Some(job.clone());
    state.last = Some(job.clone());
    Ok(Operation { inner, job })
}
impl Engine {
    /// Atomically stops admitting operations before cancelling the active worker.
    /// Once active is cleared, no queued command can restart work before exit.
    pub fn request_shutdown(&self) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.closing = true;
        state.plan = None;
        state.recoveries.clear();
        if let Some(job) = &state.active {
            job.cancel.store(true, Ordering::Release);
            job.change(|s| s.cancel_requested = true);
            true
        } else {
            false
        }
    }
    pub fn input_revision(&self) -> String {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revision
            .to_string()
    }
    pub fn invalidate(&self, input: &str) -> Result<()> {
        let rev = revision(input)?;
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if rev < state.revision {
            return Err(ErrorDto::new(
                "STALE_REQUEST",
                "输入版本已过期",
                "invalidate",
            ));
        }
        if let Some(job) = &state.active {
            if job.snapshot.lock().unwrap_or_else(|e| e.into_inner()).phase != "planning" {
                return Err(ErrorDto::new("BUSY", "执行期间不能更改输入", "invalidate"));
            }
            job.cancel.store(true, Ordering::Release);
            job.change(|s| s.cancel_requested = true);
        }
        state.revision = rev;
        state.plan = None;
        state.recoveries.clear();
        Ok(())
    }
    pub fn preview(&self, request: PreviewRequest) -> Result<PlanDto> {
        let rev = revision(&request.revision)?;
        let op = {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if rev != state.revision {
                return Err(ErrorDto::new("STALE_REQUEST", "输入版本已过期", "preview"));
            }
            let op = begin(&mut state, self.inner.clone(), "planning")?;
            state.plan = None;
            op
        };
        let planned = self.make_plans(&request, &op.job);
        let plans = match planned {
            Ok(p) => p,
            Err(e) => {
                op.job.end_error(e.clone());
                return Err(e);
            }
        };
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state.revision != rev || op.job.cancel.load(Ordering::Acquire) {
            op.job.terminal("cancelled", None);
            return Err(ErrorDto::new(
                "STALE_REQUEST",
                "预览取消或输入已更改",
                "preview",
            ));
        }
        let dto = PlanDto {
            plan_id: id("plan"),
            revision: rev.to_string(),
            target: request.target,
            files: plans
                .iter()
                .map(|p| p.view().counts.files as u128)
                .sum::<u128>()
                .to_string(),
            directories: plans
                .iter()
                .map(|p| p.view().counts.directories as u128)
                .sum::<u128>()
                .to_string(),
            bytes: plans
                .iter()
                .map(|p| p.view().bytes as u128)
                .sum::<u128>()
                .to_string(),
            entry_count: plans
                .iter()
                .map(|p| p.view().entries.len() as u128)
                .sum::<u128>()
                .to_string(),
            destinations: plans
                .iter()
                .map(|p| p.view().destination_name.to_string_lossy().into())
                .collect(),
            issues: plans.iter().flat_map(|p| p.view().issues.clone()).collect(),
        };
        state.plan = Some(SavedPlan {
            id: dto.plan_id.clone(),
            revision: rev,
            plans,
        });
        op.job.terminal("preview_ready", None);
        drop(state);
        Ok(dto)
    }
    fn make_plans(&self, request: &PreviewRequest, job: &Arc<Job>) -> Result<Vec<BackupPlan>> {
        if request.sources.is_empty() || request.sources.len() > 256 {
            return Err(ErrorDto::new(
                "INVALID_INPUT",
                "请选择 1 至 256 个源",
                "preview",
            ));
        }
        let target = target_path(&request.target)?;
        let mut canonical = Vec::<PathBuf>::new();
        let mut plans = Vec::<BackupPlan>::new();
        for input in &request.sources {
            let src = path(&input.path)?;
            let c = src
                .canonicalize()
                .map_err(|e| ErrorDto::new("INVALID_INPUT", e, "preview"))?;
            if canonical
                .iter()
                .any(|p| p.starts_with(&c) || c.starts_with(p))
            {
                return Err(ErrorDto::new(
                    "INVALID_INPUT",
                    "源路径重叠或重复，请减少选择",
                    "preview",
                ));
            }
            canonical.push(c);
            let source = match input.kind {
                SourceKind::File => SourceSelection::File(src),
                SourceKind::Directory => SourceSelection::Directory(src),
            };
            let mut req = BackupRequest::new(source, target.clone());
            if matches!(input.kind, SourceKind::Directory) {
                let suffixes = parse_extensions(&input.suffixes)
                    .map_err(|e| ErrorDto::new("INVALID_INPUT", e, "suffixes"))?;
                req.directory_options = Some(DirectoryOptions {
                    recursive: input.recursive,
                    extensions: if suffixes.is_empty() {
                        None
                    } else {
                        Some(suffixes)
                    },
                    include_extensionless: input.include_extensionless,
                });
            }
            let plan = plan_backup(&req, &job.cancel, &UiReporter(job.clone()))
                .map_err(|e| ErrorDto::new("PLAN_FAILED", format!("{e:#}"), "preview"))?;
            let name = plan
                .view()
                .destination_name
                .to_string_lossy()
                .to_lowercase();
            if plans
                .iter()
                .any(|p| p.view().destination_name.to_string_lossy().to_lowercase() == name)
            {
                return Err(ErrorDto::new(
                    "INVALID_INPUT",
                    "多个源的目标名称冲突，请分别备份",
                    "preview",
                ));
            }
            plans.push(plan);
        }
        Ok(plans)
    }
    pub fn entries(&self, token: &str, offset: usize, limit: usize) -> Result<Vec<EntryDto>> {
        if limit == 0 || limit > 200 {
            return Err(ErrorDto::new(
                "INVALID_INPUT",
                "预览分页限 1 至 200 行",
                "entries",
            ));
        }
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let saved = state
            .plan
            .as_ref()
            .filter(|p| p.id == token)
            .ok_or_else(|| ErrorDto::new("STALE_PLAN", "预览已失效", "entries"))?;
        Ok(saved
            .plans
            .iter()
            .flat_map(|p| p.view().entries.iter().map(move |e| (p, e)))
            .skip(offset)
            .take(limit)
            .map(|(p, e)| EntryDto {
                source: p.request().source.path().to_string_lossy().into(),
                relative_path: e.relative_path.to_string_lossy().into(),
                kind: format!("{:?}", e.kind),
                bytes: e.bytes.to_string(),
            })
            .collect())
    }
    pub fn start(&self, input: &str, token: &str) -> Result<String> {
        let rev = revision(input)?;
        let (saved, op) = {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if state.revision != rev
                || !state
                    .plan
                    .as_ref()
                    .is_some_and(|p| p.revision == rev && p.id == token)
            {
                return Err(ErrorDto::new("STALE_PLAN", "请重新预览", "start"));
            }
            let op = begin(&mut state, self.inner.clone(), "copying")?;
            (state.plan.take().unwrap(), op)
        };
        let job_id = op
            .job
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .job_id
            .clone();
        let worker = move || {
            for plan in saved.plans {
                if op.job.cancel.load(Ordering::Acquire) {
                    op.job.terminal("cancelled", None);
                    return;
                }
                let core_id = plan.view().job_id.clone();
                match run_backup_plan(&plan, &op.job.cancel, &UiReporter(op.job.clone())) {
                    Ok(summary) => {
                        let success =
                            summary.outcome == BackupOutcome::Completed && summary.published;
                        let cancelled = summary.outcome == BackupOutcome::Cancelled;
                        op.job
                            .change(|s| s.results.push(result_dto(&core_id, &summary)));
                        if !success {
                            op.job
                                .terminal(if cancelled { "cancelled" } else { "failed" }, None);
                            return;
                        }
                    }
                    Err(e) => {
                        if let Some(partial) = e.downcast_ref::<BackupExecutionError>() {
                            op.job
                                .change(|s| s.results.push(result_dto(&core_id, &partial.summary)));
                        }
                        op.job.terminal(
                            "failed",
                            Some(ErrorDto::new("COPY_FAILED", format!("{e:#}"), "copy")),
                        );
                        return;
                    }
                }
            }
            op.job.terminal("completed", None);
        };
        std::thread::Builder::new()
            .name("bftool-copy".into())
            .spawn(worker)
            .map_err(|e| ErrorDto::new("WORKER_FAILED", e, "start"))?;
        Ok(job_id)
    }
    pub fn is_busy(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .is_some()
    }
    /// Frontend supplies only a one-use token from validated history, never a
    /// core job ID, manifest, staging path, or executable entry list.
    pub fn resume(&self, input: &str, token: &str) -> Result<String> {
        let rev = revision(input)?;
        let (saved, op) = {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let index = state
                .recoveries
                .iter()
                .position(|r| r.id == token && r.revision == rev)
                .filter(|_| state.revision == rev)
                .ok_or_else(|| {
                    ErrorDto::new(
                        "STALE_RECOVERY",
                        "恢复记录已失效，请重新加载原目标目录的任务记录",
                        "resume",
                    )
                })?;
            let op = begin(&mut state, self.inner.clone(), "recovering")?;
            let saved = state.recoveries.remove(index);
            state.plan = None;
            (saved, op)
        };
        let job_id = op
            .job
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .job_id
            .clone();
        std::thread::Builder::new()
            .name("bftool-resume".into())
            .spawn(move || {
                match resume_backup(
                    &saved.target,
                    &saved.core_job_id,
                    &op.job.cancel,
                    &UiReporter(op.job.clone()),
                ) {
                    Ok(summary) => {
                        let success =
                            summary.outcome == BackupOutcome::Completed && summary.published;
                        let cancelled = summary.outcome == BackupOutcome::Cancelled;
                        op.job
                            .change(|s| s.results.push(result_dto(&saved.core_job_id, &summary)));
                        op.job.terminal(
                            if success {
                                "completed"
                            } else if cancelled {
                                "cancelled"
                            } else {
                                "failed"
                            },
                            None,
                        );
                    }
                    Err(e) => {
                        if let Some(partial) = e.downcast_ref::<BackupExecutionError>() {
                            op.job.change(|s| {
                                s.results
                                    .push(result_dto(&saved.core_job_id, &partial.summary))
                            });
                        }
                        op.job.end_error(ErrorDto::new(
                            "RECOVERY_FAILED",
                            format!("{e:#}"),
                            "resume",
                        ));
                    }
                }
            })
            .map_err(|e| ErrorDto::new("WORKER_FAILED", e, "resume"))?;
        Ok(job_id)
    }
    pub fn snapshot(&self) -> Result<Option<Snapshot>> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(state
            .last
            .as_ref()
            .map(|j| j.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone()))
    }
    pub fn cancel(&self, job_id: &str) -> Result<()> {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let job = state
            .active
            .as_ref()
            .ok_or_else(|| ErrorDto::new("STALE_JOB", "任务已结束", "cancel"))?;
        if job
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .job_id
            != job_id
        {
            return Err(ErrorDto::new("STALE_JOB", "任务编号已过期", "cancel"));
        }
        job.cancel.store(true, Ordering::Release);
        job.change(|s| s.cancel_requested = true);
        Ok(())
    }
    pub fn history(&self, target: &str) -> Result<Vec<HistoryDto>> {
        let target = target_path(target)?;
        let op = {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let op = begin(&mut state, self.inner.clone(), "history")?;
            state.recoveries.clear();
            op
        };
        let result = list_backup_history_with_cancel(&target, &op.job.cancel).map(|records| {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            records
                .into_iter()
                .map(|r| {
                    let recovery_id = if !state.closing
                        && !op.job.cancel.load(Ordering::Acquire)
                        && !r.completed
                        && matches!(r.state.as_str(), "Copying" | "Publishing")
                    {
                        let token = id("recovery");
                        let rev = state.revision;
                        state.recoveries.push(SavedRecovery {
                            id: token.clone(),
                            revision: rev,
                            target: target.clone(),
                            core_job_id: r.job_id.clone(),
                        });
                        Some(token)
                    } else {
                        None
                    };
                    HistoryDto {
                        core_job_id: r.job_id,
                        source: r.source.path().to_string_lossy().into(),
                        destination: r.destination_name.to_string_lossy().into(),
                        bytes: r.bytes.to_string(),
                        completed: r.completed,
                        state: r.state,
                        recursive: r.directory_options.recursive,
                        recovery_id,
                    }
                })
                .collect()
        });
        match result {
            Ok(r) => {
                op.job.terminal("history_loaded", None);
                Ok(r)
            }
            Err(e) => {
                let e = ErrorDto::new("HISTORY_FAILED", format!("{e:#}"), "history");
                op.job.end_error(e.clone());
                Err(e)
            }
        }
    }
    pub fn verify(&self, target: &str) -> Result<VerificationDto> {
        let target = target_path(target)?;
        let op = {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            begin(&mut state, self.inner.clone(), "verifying")?
        };
        match verify_backup(&target, &op.job.cancel, &UiReporter(op.job.clone())) {
            Ok(r) => {
                let dto = VerificationDto {
                    checked: r.checked.to_string(),
                    bad: r.bad.to_string(),
                    size_only: r.size_only.to_string(),
                    extras: r.extras.len().to_string(),
                    cancelled: r.cancelled,
                    issues: r
                        .issues
                        .iter()
                        .map(|i| format!("{} / {}: {:?}", i.project, i.rel, i.kind))
                        .collect(),
                };
                op.job.terminal(
                    if r.cancelled {
                        "cancelled"
                    } else {
                        "verification_finished"
                    },
                    None,
                );
                Ok(dto)
            }
            Err(e) => {
                let e = ErrorDto::new("VERIFY_FAILED", format!("{e:#}"), "verify");
                op.job.terminal("failed", Some(e.clone()));
                Err(e)
            }
        }
    }
}
fn result_dto(id: &str, s: &BackupSummary) -> ResultDto {
    ResultDto {
        core_job_id: id.into(),
        outcome: format!("{:?}", s.outcome),
        published: s.published,
        copied: s.copied.to_string(),
        verified: s.verified.to_string(),
        skipped_verified: s.skipped_verified.to_string(),
        failed: s.failed.to_string(),
        bytes: s.bytes.to_string(),
        issues: s.issues.clone(),
    }
}
struct UiReporter(Arc<Job>);
struct UiProgress {
    job: Arc<Job>,
    done: u64,
}
impl Reporter for UiReporter {
    fn log(&self, _: LogLevel, message: &str) {
        self.0.change(|s| s.label = message.into());
    }
    fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle> {
        self.0.change(|s| {
            s.label = label.into();
            s.current_bytes = "0".into();
            s.total_bytes = total.to_string();
        });
        Box::new(UiProgress {
            job: self.0.clone(),
            done: 0,
        })
    }
}
impl ProgressHandle for UiProgress {
    fn inc(&mut self, delta: u64) {
        self.done = self.done.saturating_add(delta);
        self.job.change(|s| s.current_bytes = self.done.to_string());
    }
    fn finish(&mut self) {} // Never invent completion; the core outcome owns terminal status.
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn shutdown_closes_admission_before_waiting_for_active_operation() {
        let engine = Engine::default();
        let operation = {
            let mut state = engine.inner.lock().unwrap();
            begin(&mut state, engine.inner.clone(), "history").unwrap()
        };
        assert!(engine.request_shutdown());
        assert!(operation.job.cancel.load(Ordering::Acquire));
        assert!(engine.snapshot().unwrap().unwrap().cancel_requested);
        drop(operation);
        assert!(!engine.is_busy());
        let mut state = engine.inner.lock().unwrap();
        assert_eq!(
            begin(&mut state, engine.inner.clone(), "planning")
                .err()
                .unwrap()
                .code,
            "SHUTTING_DOWN"
        );
    }

    #[test]
    fn idle_shutdown_also_rejects_queued_operations() {
        let engine = Engine::default();
        assert!(!engine.request_shutdown());
        let error = engine.history("C:/synthetic-unused-target").unwrap_err();
        assert_eq!(error.code, "SHUTTING_DOWN");
    }

    #[test]
    fn busy_preview_does_not_destroy_existing_plan() {
        let engine = Engine::default();
        let operation = {
            let mut state = engine.inner.lock().unwrap();
            state.plan = Some(SavedPlan {
                id: "retained-plan".into(),
                revision: 0,
                plans: vec![],
            });
            begin(&mut state, engine.inner.clone(), "history").unwrap()
        };
        let error = engine
            .preview(PreviewRequest {
                revision: "0".into(),
                target: "C:/synthetic-unused-target".into(),
                sources: vec![],
            })
            .unwrap_err();
        assert_eq!(error.code, "BUSY");
        assert_eq!(
            engine.inner.lock().unwrap().plan.as_ref().unwrap().id,
            "retained-plan"
        );
        drop(operation);
    }

    #[test]
    fn busy_resume_keeps_token_and_shutdown_invalidates_it() {
        let engine = Engine::default();
        let operation = {
            let mut state = engine.inner.lock().unwrap();
            state.recoveries.push(SavedRecovery {
                id: "recovery-test".into(),
                revision: 0,
                target: PathBuf::from("C:/synthetic-unused"),
                core_job_id: "job-synthetic".into(),
            });
            begin(&mut state, engine.inner.clone(), "copying").unwrap()
        };
        assert_eq!(
            engine.resume("0", "recovery-test").unwrap_err().code,
            "BUSY"
        );
        assert_eq!(engine.inner.lock().unwrap().recoveries.len(), 1);
        drop(operation);
        assert!(!engine.request_shutdown());
        assert_eq!(
            engine.resume("0", "recovery-test").unwrap_err().code,
            "STALE_RECOVERY"
        );
        assert!(!engine.is_busy());
    }

    #[cfg(windows)]
    #[test]
    fn target_spelling_normalization_does_not_resolve_paths_or_traversal() {
        assert_eq!(
            target_path("C:/ordinary/./child").unwrap(),
            PathBuf::from(r"\\?\C:\ordinary\child")
        );
        assert_eq!(
            target_path(r"\\?\C:\ordinary\child").unwrap(),
            PathBuf::from(r"\\?\C:\ordinary\child")
        );
        assert_eq!(
            target_path(r"\\server\share\ordinary").unwrap(),
            PathBuf::from(r"\\?\UNC\server\share\ordinary")
        );
        for input in [
            r"C:\ordinary\..\child",
            r"\\?\C:\ordinary\..\child",
            r"\\.\C:\ordinary",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1",
            r"C:\ordinary.\child",
            r"C:\ordinary \child",
            r"C:\NUL\child",
            r"\\?\C:\a/b",
            r"C:\a:stream",
            r"C:relative",
            r"\root-relative",
        ] {
            assert_eq!(
                target_path(input).unwrap_err().code,
                "INVALID_INPUT",
                "{input}"
            );
        }
    }
}

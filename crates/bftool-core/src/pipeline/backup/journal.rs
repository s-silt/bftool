use super::{snapshot::*, types::*};
use crate::engine::destination::{file_identity, SafeDir};
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub(super) const NAMESPACE: &str = ".bftool-backup";
const MAGIC: &str = "bftool-direct-copy-v1";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    magic: String,
    version: u32,
    target_id: String,
    namespace_id: String,
    jobs_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub magic: String,
    pub version: u32,
    pub job_id: String,
    pub job_dir_id: String,
    pub stage_id: String,
    pub source_id: String,
    pub target_id: String,
    pub request: BackupRequest,
    pub destination_name: PathBuf,
    pub entries: Vec<BackupEntry>,
    pub bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum JobState {
    Copying,
    Publishing,
    Completed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub identity: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    pub magic: String,
    pub job_id: String,
    pub state: JobState,
    pub completed: BTreeMap<PathBuf, Receipt>,
    pub directories: BTreeMap<PathBuf, String>,
    pub payload_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(super) enum Delta {
    Directory { path: PathBuf, identity: String },
    File { path: PathBuf, receipt: Receipt },
    Publishing { payload_id: String },
    Completed,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    previous_sha256: String,
    change: Delta,
}

// Immutable deltas retain one receipt each, never a growing journal snapshot.
// The only write operation is held-object no-replace publication.
pub(super) struct JournalLog {
    records: Vec<(String, String, String)>, // name, identity, SHA-256
}

fn read_metadata_stream(reader: &mut impl Read, cancel: &AtomicBool) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = vec![0; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        let count = reader.read(&mut chunk)?;
        #[cfg(test)]
        scale_tests::record_metadata_io(0, count as u64);
        check_cancel(cancel)?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > 64 * 1024 * 1024 {
            bail!("Backup metadata exceeds size limit");
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
}

fn metadata_bytes(dir: &SafeDir, name: &str, cancel: &AtomicBool) -> Result<(Vec<u8>, String)> {
    check_cancel(cancel)?;
    let mut file = dir.read_regular(Path::new(name))?;
    #[cfg(test)]
    scale_tests::record_metadata_io(1, 0);
    if file.metadata()?.len() > 64 * 1024 * 1024 {
        bail!("Backup metadata exceeds size limit");
    }
    let identity = file_identity(&file)?;
    let bytes = read_metadata_stream(&mut file, cancel)?;
    Ok((bytes, identity))
}
fn digest(bytes: &[u8], cancel: &AtomicBool) -> Result<String> {
    let mut hash = Sha256::new();
    for chunk in bytes.chunks(64 * 1024) {
        check_cancel(cancel)?;
        hash.update(chunk);
    }
    check_cancel(cancel)?;
    Ok(format!("{:X}", hash.finalize()))
}
fn decode<T: DeserializeOwned>(bytes: &[u8], cancel: &AtomicBool) -> Result<T> {
    check_cancel(cancel)?;
    let unique: UniqueJson = serde_json::from_slice(bytes)
        .context("Malformed or duplicate-key direct-backup metadata")?;
    check_cancel(cancel)?;
    let value = serde_json::from_value(unique.0).context("Malformed direct-backup metadata")?;
    check_cancel(cancel)?;
    Ok(value)
}
fn metadata_names(dir: &SafeDir, cancel: &AtomicBool) -> Result<Vec<std::ffi::OsString>> {
    dir.list_entries_checked(|| check_cancel(cancel))
}
fn checkpoint_names(job: &SafeDir, cancel: &AtomicBool) -> Result<Vec<String>> {
    let mut checkpoints = Vec::new();
    let names = metadata_names(job, cancel)?;
    for name in &names {
        check_cancel(cancel)?;
        let name = name.to_str().context("Non-Unicode job metadata name")?;
        if ["stage", "manifest.json", "journal.json", "job.lock"].contains(&name) {
            continue;
        }
        let number = name
            .strip_prefix("checkpoint-")
            .and_then(|s| s.strip_suffix(".json"));
        if !number.is_some_and(|s| s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit())) {
            bail!("Unknown job metadata entry; preserved");
        }
        checkpoints.push(name.to_string());
    }
    if !["stage", "manifest.json", "journal.json"]
        .iter()
        .all(|e| names.iter().any(|n| n == *e))
    {
        bail!("Incomplete job metadata; preserved");
    }
    checkpoints.sort();
    for (index, name) in checkpoints.iter().enumerate() {
        check_cancel(cancel)?;
        if *name != format!("checkpoint-{:08}.json", index + 1) {
            bail!("Missing or out-of-sequence checkpoint; preserved");
        }
    }
    let mut records = vec!["journal.json".to_string()];
    records.extend(checkpoints);
    Ok(records)
}
fn apply_delta(journal: &mut Journal, change: &Delta, cancel: &AtomicBool) -> Result<()> {
    check_cancel(cancel)?;
    match change {
        Delta::Directory { path, identity } => {
            safe_relative(path, true)?;
            if journal.state != JobState::Copying
                || identity.is_empty()
                || journal.directories.contains_key(path)
                || journal.completed.contains_key(path)
            {
                bail!("Invalid or duplicate directory receipt transition");
            }
            journal.directories.insert(path.clone(), identity.clone());
        }
        Delta::File { path, receipt } => {
            safe_relative(path, true)?;
            validate_hash(&receipt.sha256)?;
            if journal.state != JobState::Copying
                || receipt.identity.is_empty()
                || journal.completed.contains_key(path)
                || journal.directories.contains_key(path)
            {
                bail!("Invalid or duplicate completed-file receipt transition");
            }
            journal.completed.insert(path.clone(), receipt.clone());
        }
        Delta::Publishing { payload_id } => {
            if payload_id.is_empty()
                || journal.state == JobState::Completed
                || journal
                    .payload_id
                    .as_ref()
                    .is_some_and(|id| id != payload_id)
            {
                bail!("Invalid publishing transition");
            }
            journal.payload_id = Some(payload_id.clone());
            journal.state = JobState::Publishing;
        }
        Delta::Completed => {
            if journal.state != JobState::Publishing {
                bail!("Invalid completion transition");
            }
            journal.state = JobState::Completed;
        }
    }
    Ok(())
}
pub(super) fn read_journal(
    job: &SafeDir,
    manifest: &Manifest,
    cancel: &AtomicBool,
) -> Result<(Journal, JournalLog)> {
    let names = checkpoint_names(job, cancel)?;
    let (bytes, identity) = metadata_bytes(job, &names[0], cancel)?;
    let mut journal: Journal = decode(&bytes, cancel)?;
    if journal.state != JobState::Copying
        || !journal.completed.is_empty()
        || !journal.directories.is_empty()
        || journal.payload_id.is_some()
    {
        bail!("Initial immutable journal is not empty; legacy mutable format is not adopted");
    }
    let mut log = JournalLog {
        records: vec![(names[0].clone(), identity, digest(&bytes, cancel)?)],
    };
    for name in names.into_iter().skip(1) {
        check_cancel(cancel)?;
        let (bytes, identity) = metadata_bytes(job, &name, cancel)?;
        let checkpoint: Checkpoint = decode(&bytes, cancel)?;
        if checkpoint.previous_sha256 != log.records.last().context("Missing prior record")?.2 {
            bail!("Immutable journal hash chain changed; preserved");
        }
        apply_delta(&mut journal, &checkpoint.change, cancel)?;
        log.records.push((name, identity, digest(&bytes, cancel)?));
    }
    // Validate the reconstructed receipts against an indexed manifest once.
    validate(manifest, &journal, cancel)?;
    log.require_current(job, cancel)?;
    Ok((journal, log))
}
impl JournalLog {
    fn require_record(
        &self,
        job: &SafeDir,
        record: &(String, String, String),
        cancel: &AtomicBool,
    ) -> Result<()> {
        let (bytes, id) = metadata_bytes(job, &record.0, cancel)?;
        if id != record.1 || digest(&bytes, cancel)? != record.2 {
            bail!("Journal ownership/content changed; unknown metadata preserved");
        }
        Ok(())
    }
    // A complete independent audit is mandatory at publication/completion, but
    // is not repeated for each appended receipt.
    pub(super) fn require_current(&self, job: &SafeDir, cancel: &AtomicBool) -> Result<()> {
        let names = checkpoint_names(job, cancel)?;
        if names.len() != self.records.len() {
            bail!("Journal namespace changed; unknown entries preserved");
        }
        for (name, record) in names.iter().zip(&self.records) {
            check_cancel(cancel)?;
            if name != &record.0 {
                bail!("Journal namespace changed; unknown entries preserved");
            }
            self.require_record(job, record, cancel)?;
        }
        job.require_current_binding()
    }
    pub(super) fn append(
        &mut self,
        job: &SafeDir,
        journal: &mut Journal,
        change: Delta,
        cancel: &AtomicBool,
    ) -> Result<()> {
        check_cancel(cancel)?;
        job.require_current_binding()?;
        let previous = self.records.last().context("Missing journal origin")?;
        self.require_record(job, previous, cancel)?;
        let name = format!("checkpoint-{:08}.json", self.records.len());
        apply_delta(journal, &change, cancel)?;
        let bytes = serde_json::to_vec_pretty(&Checkpoint {
            previous_sha256: previous.2.clone(),
            change,
        })?;
        check_cancel(cancel)?;
        // Occupation of this next name is rejected by the actual native operation.
        // Other unknown historical entries are preserved and caught by the audit.
        let published_identity = job.write_metadata_new(Path::new(&name), &bytes)?;
        let (current, identity) = metadata_bytes(job, &name, cancel)?;
        if current != bytes || identity != published_identity {
            bail!("New checkpoint changed after publication; preserved");
        }
        self.records.push((name, identity, digest(&bytes, cancel)?));
        job.require_current_binding()
    }
}
// serde's map deserializer ordinarily accepts duplicate keys with last-value wins.
// Reject them recursively before decoding any ownership evidence.
struct UniqueJson(serde_json::Value);
impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if out.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "Duplicate metadata key: {key}"
                        )));
                    }
                    out.insert(key, map.next_value::<UniqueJson>()?.0);
                }
                Ok(UniqueJson(serde_json::Value::Object(out)))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(value) = seq.next_element::<UniqueJson>()? {
                    out.push(value.0);
                }
                Ok(UniqueJson(serde_json::Value::Array(out)))
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|v| UniqueJson(serde_json::Value::Number(v)))
                    .ok_or_else(|| E::custom("Invalid JSON number"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(UniqueJson(serde_json::Value::Null))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}
pub(super) fn read_json<T: DeserializeOwned>(
    dir: &SafeDir,
    name: &Path,
    cancel: &AtomicBool,
) -> Result<T> {
    let (bytes, _) = metadata_bytes(
        dir,
        name.to_str().context("Non-Unicode metadata name")?,
        cancel,
    )?;
    decode(&bytes, cancel)
}
pub(super) fn write_json<T: Serialize>(dir: &SafeDir, name: &str, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    dir.write_metadata_new(Path::new(name), &bytes).map(|_| ())
}
fn exact_names(dir: &SafeDir, expected: &[&str], cancel: &AtomicBool) -> Result<()> {
    let names = metadata_names(dir, cancel)?;
    if names.len() != expected.len() || names.iter().any(|n| !expected.iter().any(|e| n == *e)) {
        bail!("Unknown entries in backup metadata namespace; preserved");
    }
    Ok(())
}
fn namespace(target: &SafeDir, create: bool, cancel: &AtomicBool) -> Result<SafeDir> {
    let names = metadata_names(target, cancel)?;
    if let Some(actual) = names.iter().find(|n| key(Path::new(n)) == NAMESPACE) {
        if actual != NAMESPACE {
            bail!("Reserved metadata name has a case alias");
        }
        let ns = target.open_existing_dir(Path::new(NAMESPACE))?;
        exact_names(&ns, &["owner.json", "jobs"], cancel)?;
        let owner: Owner = read_json(&ns, Path::new("owner.json"), cancel)?;
        let jobs = ns.open_existing_dir(Path::new("jobs"))?;
        if owner.magic != MAGIC
            || owner.version != 1
            || owner.target_id != target.identity()?
            || owner.namespace_id != ns.identity()?
            || owner.jobs_id != jobs.identity()?
        {
            bail!("Unowned or substituted backup metadata namespace");
        }
        return Ok(ns);
    }
    if !create {
        bail!("No owned direct-backup metadata in target");
    }
    let ns = target.create_new_dir(Path::new(NAMESPACE))?;
    let jobs = ns.create_new_dir(Path::new("jobs"))?;
    let owner = Owner {
        magic: MAGIC.into(),
        version: 1,
        target_id: target.identity()?,
        namespace_id: ns.identity()?,
        jobs_id: jobs.identity()?,
    };
    write_json(&ns, "owner.json", &owner)?;
    Ok(ns)
}
pub(super) fn create_job(
    plan: &BackupPlan,
    cancel: &AtomicBool,
) -> Result<(SafeDir, SafeDir, Manifest, Journal)> {
    let ns = namespace(&plan.target, true, cancel)?;
    let jobs = ns.open_existing_dir(Path::new("jobs"))?;
    // Adopt the namespace only after every existing job proves ownership, once.
    for id in job_names(&jobs, cancel)? {
        check_cancel(cancel)?;
        load_job_from_jobs(&plan.target, &jobs, &id, cancel)?;
    }
    check_cancel(cancel)?;
    let job = jobs.create_new_dir(Path::new(&plan.view.job_id))?;
    let stage = job.create_new_dir(Path::new("stage"))?;
    let manifest = Manifest {
        magic: MAGIC.into(),
        version: 2,
        job_id: plan.view.job_id.clone(),
        job_dir_id: job.identity()?,
        stage_id: stage.identity()?,
        source_id: plan.source_id.clone(),
        target_id: plan.target_id.clone(),
        request: plan.request.clone(),
        destination_name: plan.view.destination_name.clone(),
        entries: plan.view.entries.clone(),
        bytes: plan.view.bytes,
    };
    let journal = Journal {
        magic: MAGIC.into(),
        job_id: manifest.job_id.clone(),
        state: JobState::Copying,
        completed: BTreeMap::new(),
        directories: BTreeMap::new(),
        payload_id: None,
    };
    write_json(&job, "manifest.json", &manifest)?;
    write_json(&job, "journal.json", &journal)?;
    Ok((job, stage, manifest, journal))
}
pub(super) fn valid_job_id(id: &str) -> Result<()> {
    safe_relative(Path::new(id), false)?;
    if !id.starts_with("job-")
        || id.len() > 100
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        bail!("Invalid backup job id");
    }
    Ok(())
}
type LoadedJob = (SafeDir, SafeDir, Manifest, Journal);
fn job_names(jobs: &SafeDir, cancel: &AtomicBool) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    for name in metadata_names(jobs, cancel)? {
        check_cancel(cancel)?;
        let id = name
            .into_string()
            .map_err(|_| anyhow::anyhow!("Non-Unicode job directory name"))?;
        valid_job_id(&id)?;
        if !seen.insert(id.to_lowercase()) {
            bail!("Case-colliding backup job directory names");
        }
        ids.push(id);
    }
    ids.sort();
    check_cancel(cancel)?;
    Ok(ids)
}
pub(super) fn load_jobs(target: &SafeDir, cancel: &AtomicBool) -> Result<Vec<LoadedJob>> {
    check_cancel(cancel)?;
    let ns = namespace(target, false, cancel)?;
    let jobs = ns.open_existing_dir(Path::new("jobs"))?;
    let mut loaded = Vec::new();
    for id in job_names(&jobs, cancel)? {
        check_cancel(cancel)?;
        loaded.push(load_job_from_jobs(target, &jobs, &id, cancel)?);
    }
    check_cancel(cancel)?;
    Ok(loaded)
}
pub(super) fn load_job(target: &SafeDir, id: &str, cancel: &AtomicBool) -> Result<LoadedJob> {
    valid_job_id(id)?;
    load_jobs(target, cancel)?
        .into_iter()
        .find(|(_, _, m, _)| m.job_id == id)
        .context("Backup job not found")
}
fn load_job_from_jobs(
    target: &SafeDir,
    jobs: &SafeDir,
    id: &str,
    cancel: &AtomicBool,
) -> Result<(SafeDir, SafeDir, Manifest, Journal)> {
    valid_job_id(id)?;
    let job = jobs.open_existing_dir(Path::new(id))?;
    let manifest: Manifest = read_json(&job, Path::new("manifest.json"), cancel)?;
    let (journal, _) = read_journal(&job, &manifest, cancel)?;
    let stage = job.open_existing_dir(Path::new("stage"))?;
    check_cancel(cancel)?;
    if manifest.job_id != id
        || manifest.target_id != target.identity()?
        || manifest.job_dir_id != job.identity()?
        || manifest.stage_id != stage.identity()?
    {
        bail!("Backup job filesystem identity mismatch");
    }
    let requested_target = SafeDir::open(&manifest.request.target_dir, false)?;
    if requested_target.identity()? != manifest.target_id {
        bail!("Manifest target path identity mismatch");
    }
    if journal.state == JobState::Completed {
        require_empty_stage(&stage, cancel)?;
    }
    Ok((job, stage, manifest, journal))
}
pub(super) fn require_empty_stage(stage: &SafeDir, cancel: &AtomicBool) -> Result<()> {
    if !metadata_names(stage, cancel)?.is_empty() {
        bail!("Completed job contains unknown staging entries; preserved");
    }
    Ok(())
}
pub(super) fn validate(m: &Manifest, j: &Journal, cancel: &AtomicBool) -> Result<()> {
    check_cancel(cancel)?;
    if m.magic != MAGIC || j.magic != MAGIC || !matches!(m.version, 1 | 2) || m.job_id != j.job_id {
        bail!("Invalid backup manifest/journal header");
    }
    let options = m.request.effective_directory_options();
    if m.version == 2 {
        if m.request.directory_options.is_none() || options.canonicalized()? != options {
            bail!("Version 2 manifest requires canonical folder options");
        }
        if matches!(m.request.source, SourceSelection::File(_))
            && options != DirectoryOptions::default()
        {
            bail!("Selected-file manifest contains active folder options");
        }
    } else if m.request.directory_options.is_some() && options != DirectoryOptions::legacy() {
        bail!("Version 1 manifest cannot introduce folder filtering");
    }
    valid_job_id(&m.job_id)?;
    safe_relative(&m.destination_name, false)?;
    if m.destination_name.components().count() != 1 || key(&m.destination_name) == NAMESPACE {
        bail!("Unsafe published payload name");
    }
    for id in [&m.source_id, &m.target_id, &m.job_dir_id, &m.stage_id] {
        if id.is_empty() {
            bail!("Missing bound object identity");
        }
    }
    let mut seen = BTreeSet::new();
    let mut paths = BTreeMap::new();
    let mut entries_by_path = BTreeMap::new();
    let mut bytes = 0u64;
    for entry in &m.entries {
        check_cancel(cancel)?;
        entries_by_path.insert(entry.relative_path.clone(), entry);
        safe_relative(&entry.relative_path, true)?;
        if m.version == 2 && matches!(m.request.source, SourceSelection::Directory(_)) {
            if !options.recursive
                && ((entry.kind == BackupEntryKind::Directory
                    && !entry.relative_path.as_os_str().is_empty())
                    || entry.relative_path.components().count() > 1)
            {
                bail!("Manifest entry violates shallow folder rule");
            }
            if entry.kind == BackupEntryKind::File && !options.matches_file(&entry.relative_path) {
                bail!("Manifest file violates recorded suffix rule");
            }
        }
        if entry.identity.is_empty() || !seen.insert(key(&entry.relative_path)) {
            bail!("Duplicate or missing manifest entry identity");
        }
        match entry.kind {
            BackupEntryKind::File => {
                let hash = entry.sha256.as_deref().context("Missing SHA256")?;
                validate_hash(hash)?;
                if entry.modified.is_none() {
                    bail!("Missing file metadata snapshot");
                }
                bytes = bytes
                    .checked_add(entry.bytes)
                    .context("Manifest byte overflow")?;
            }
            BackupEntryKind::Directory => {
                if entry.sha256.is_some() || entry.bytes != 0 || entry.modified.is_some() {
                    bail!("Invalid directory snapshot");
                }
            }
        }
        paths.insert(entry.relative_path.clone(), entry.kind.clone());
    }
    if bytes != m.bytes || m.entries.is_empty() {
        bail!("Invalid manifest byte count or empty manifest");
    }
    if m.version == 2
        && matches!(m.request.source, SourceSelection::Directory(_))
        && options.filtered()
    {
        let mut needed = BTreeSet::new();
        for entry in m.entries.iter().filter(|e| e.kind == BackupEntryKind::File) {
            for parent in entry.relative_path.ancestors().skip(1) {
                needed.insert(parent.to_path_buf());
            }
        }
        if needed.is_empty()
            || m.entries
                .iter()
                .any(|e| e.kind == BackupEntryKind::Directory && !needed.contains(&e.relative_path))
        {
            bail!("Filtered manifest has no selected files or unrelated directories");
        }
    }
    let root_kind = paths
        .get(Path::new(""))
        .context("Manifest lacks payload root")?;
    match &m.request.source {
        SourceSelection::File(_) if *root_kind != BackupEntryKind::File || m.entries.len() != 1 => {
            bail!("Invalid selected-file manifest")
        }
        SourceSelection::Directory(_) if *root_kind != BackupEntryKind::Directory => {
            bail!("Invalid selected-directory manifest")
        }
        _ => {}
    }
    for path in paths.keys().filter(|p| !p.as_os_str().is_empty()) {
        check_cancel(cancel)?;
        if paths.get(path.parent().context("Missing parent")?) != Some(&BackupEntryKind::Directory)
        {
            bail!("Manifest entry lacks recorded directory parent");
        }
    }
    for (p, receipt) in &j.completed {
        check_cancel(cancel)?;
        safe_relative(p, true)?;
        validate_hash(&receipt.sha256)?;
        let e = entries_by_path
            .get(p)
            .context("Journal contains an unknown file")?;
        if e.kind != BackupEntryKind::File
            || e.sha256.as_ref() != Some(&receipt.sha256)
            || receipt.identity.is_empty()
        {
            bail!("Invalid completed-file receipt");
        }
    }
    for (p, id) in &j.directories {
        check_cancel(cancel)?;
        safe_relative(p, true)?;
        if paths.get(p) != Some(&BackupEntryKind::Directory) || id.is_empty() {
            bail!("Invalid directory ownership receipt");
        }
    }
    if j.state != JobState::Copying
        && (j.payload_id.as_ref().is_none_or(|id| id.is_empty())
            || j.completed.len()
                != paths
                    .values()
                    .filter(|k| **k == BackupEntryKind::File)
                    .count()
            || j.directories.len()
                != paths
                    .values()
                    .filter(|k| **k == BackupEntryKind::Directory)
                    .count())
    {
        bail!("Publication journal lacks verified ownership evidence");
    }
    Ok(())
}
fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
    {
        bail!("Invalid SHA256 in metadata");
    }
    Ok(())
}
pub(super) fn payload_path(entry: &BackupEntry) -> PathBuf {
    Path::new("payload").join(&entry.relative_path)
}

#[cfg(test)]
mod scale_tests {
    use super::*;

    thread_local! {
        static METADATA_IO: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
    }
    pub(super) fn record_metadata_io(opens: u64, bytes: u64) {
        METADATA_IO.with(|counter| {
            let (old_opens, old_bytes) = counter.get();
            counter.set((old_opens + opens, old_bytes + bytes));
        });
    }

    #[cfg(windows)]
    #[test]
    fn delta_fix_256_file_cancel_resume_has_linear_metadata_io_and_keeps_sources() {
        use crate::reporter::{NoopReporter, ProgressHandle, Reporter};
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};
        use std::time::Instant;
        struct CancelProgress {
            chunks: usize,
            cancel: Arc<AtomicBool>,
            triggered: Arc<Mutex<Option<Instant>>>,
        }
        impl ProgressHandle for CancelProgress {
            fn inc(&mut self, _: u64) {
                self.chunks += 1;
                if self.chunks == 32 {
                    *self.triggered.lock().unwrap() = Some(Instant::now());
                    self.cancel.store(true, Ordering::Relaxed);
                }
            }
            fn finish(&mut self) {}
        }
        struct CancelReporter(Arc<AtomicBool>, Arc<Mutex<Option<Instant>>>);
        impl Reporter for CancelReporter {
            fn log(&self, _: crate::reporter::LogLevel, _: &str) {}
            fn progress_bytes(&self, _: &str, _: u64) -> Box<dyn ProgressHandle> {
                Box::new(CancelProgress {
                    chunks: 0,
                    cancel: self.0.clone(),
                    triggered: self.1.clone(),
                })
            }
        }
        fn tree_bytes(path: &Path) -> u64 {
            std::fs::read_dir(path)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_dir() {
                        tree_bytes(&entry.path())
                    } else {
                        entry.metadata().unwrap().len()
                    }
                })
                .sum()
        }
        const FILES: u64 = 256;
        let world = tempfile::tempdir().unwrap();
        let source = world.path().join("source");
        let target = world.path().join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        for index in 0..FILES {
            std::fs::write(
                source.join(format!("file-{index:04}")),
                format!("original-{index:04}"),
            )
            .unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let request = BackupRequest {
            source: SourceSelection::Directory(source.clone()),
            target_dir: target.clone(),
            conflict: ConflictPolicy::KeepBoth,
            directory_options: Some(DirectoryOptions::legacy()),
        };
        let plan = crate::service::plan_backup(&request, &cancel, &NoopReporter).unwrap();
        METADATA_IO.with(|counter| counter.set((0, 0)));
        let triggered = Arc::new(Mutex::new(None));
        let stopped = crate::service::run_backup_plan(
            &plan,
            &cancel,
            &CancelReporter(cancel.clone(), triggered.clone()),
        )
        .unwrap();
        let cancel_return_ms = triggered
            .lock()
            .unwrap()
            .expect("cancellation callback must execute")
            .elapsed()
            .as_millis();
        assert_eq!(stopped.outcome, BackupOutcome::Cancelled);
        assert!(!stopped.published);
        assert_eq!(stopped.copied, 31);
        assert!(!target.join("source").exists());
        cancel.store(false, Ordering::Relaxed);
        let resumed =
            crate::service::resume_backup(&target, &plan.view().job_id, &cancel, &NoopReporter)
                .unwrap();
        assert!(resumed.published);
        assert_eq!(resumed.skipped_verified, stopped.copied);
        assert_eq!(resumed.copied + resumed.skipped_verified, FILES);
        let report = crate::service::verify_backup(&target, &cancel, &NoopReporter).unwrap();
        assert_eq!(
            (report.checked, report.bad, report.extra, report.cancelled),
            (FILES, 0, 0, false)
        );
        for index in 0..FILES {
            let name = format!("file-{index:04}");
            let expected = format!("original-{index:04}").into_bytes();
            assert_eq!(std::fs::read(source.join(&name)).unwrap(), expected);
            assert_eq!(
                std::fs::read(target.join("source").join(&name)).unwrap(),
                expected
            );
        }
        let (opens, read_bytes) = METADATA_IO.with(|counter| counter.get());
        let stored_bytes = tree_bytes(&target.join(NAMESPACE));
        println!("delta256 files={FILES} cancelled_copied={} resumed_skipped={} metadata_file_opens={opens} logical_metadata_read_bytes={read_bytes} owned_metadata_bytes={stored_bytes} cancel_return_ms={cancel_return_ms}", stopped.copied, resumed.skipped_verified);
        assert!(
            opens <= FILES * 24 + 128,
            "metadata open count must be linear: {opens}"
        );
        assert!(
            read_bytes <= FILES * 32768,
            "metadata read amplification must be bounded: {read_bytes}"
        );
        assert!(
            stored_bytes <= FILES * 1536 + 4096,
            "metadata storage must be linear: {stored_bytes}"
        );
    }

    #[test]
    fn delta_fix_metadata_stream_cancellation_is_checked_before_and_between_chunks() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Reader<'a> {
            read: usize,
            cancel: &'a AtomicBool,
        }
        impl Read for Reader<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                if self.read >= 2 * 1024 * 1024 {
                    return Ok(0);
                }
                let count = output.len().min(64 * 1024);
                output[..count].fill(b'x');
                self.read += count;
                self.cancel.store(true, Ordering::Relaxed);
                Ok(count)
            }
        }
        for early in [true, false] {
            let cancel = AtomicBool::new(early);
            let mut reader = Reader {
                read: 0,
                cancel: &cancel,
            };
            let result = read_metadata_stream(&mut reader, &cancel);
            assert!(result.unwrap_err().downcast_ref::<Cancelled>().is_some());
            assert_eq!(reader.read, if early { 0 } else { 64 * 1024 });
        }
    }

    #[test]
    fn delta_fix_hundreds_of_zero_byte_receipts_have_linear_wire_size() {
        // Exercise the actual serialized checkpoint representation, without
        // timing thresholds, volume enumeration or hundreds of owned writes.
        let mut bytes = 0;
        for index in 0..400 {
            bytes += serde_json::to_vec_pretty(&Checkpoint {
                previous_sha256: "A".repeat(64),
                change: Delta::File {
                    path: PathBuf::from(format!("zero-{index:04}")),
                    receipt: Receipt {
                        identity: format!("identity-{index:04}"),
                        sha256: format!("{:X}", Sha256::digest([])),
                    },
                },
            })
            .unwrap()
            .len();
        }
        println!("delta400 zero_byte_receipts=400 serialized_checkpoint_bytes={bytes}");
        assert!(bytes < 400 * 1024, "400 zero-byte file receipts used {bytes} metadata bytes; receipts must not repeat earlier files");
    }
}

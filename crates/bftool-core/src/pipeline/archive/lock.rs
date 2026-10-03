//! 事务残留检查 / 人工处理名单 / 标记清理。
use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::engine::{durable, paths, txn};
use crate::reporter::Reporter;

pub(super) fn note_manual(cfg: &Config, reporter: &dyn Reporter, name: &str, why: &str) {
    if let Err(e) = append_manual(cfg, name, why) {
        reporter.warn(&format!("写「需人工处理」台账失败({}):{}", name, e));
    }
}

pub(super) fn append_manual(cfg: &Config, name: &str, why: &str) -> Result<()> {
    let path = paths::system_need_manual(&cfg.system_root);
    let line = format!(
        "{}\t{}\t{}\n",
        Local::now().format("%Y-%m-%d %H:%M"),
        name,
        why
    );
    let directory = crate::engine::destination::SafeDir::open(
        path.parent().context("人工处理台账缺少父目录")?,
        true,
    )?;
    directory.append_synced(
        Path::new(path.file_name().context("人工处理台账缺少文件名")?),
        line.as_bytes(),
    )?;
    Ok(())
}

/// Every phase is synced before the following phase starts. Legacy markers remain readable
/// by PendingTxn, but cannot prove a retained source is a completed backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CommitStage {
    Verified,
    ManifestWritten,
    GlobalWritten,
    DriveWritten,
    IncrementalWritten,
    SourceCompleted,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct CommitJournal {
    pub version: u32,
    #[serde(flatten)]
    pub pending: txn::PendingTxn,
    pub stage: CommitStage,
    pub retain_source: bool,
    pub incremental: bool,
    pub drive_root: String,
    pub source_root: String,
    pub source_id: String,
    pub source_relative: String,
    pub snapshot: Option<super::incremental::Fingerprint>,
    pub entries: Vec<crate::engine::manifest::Entry>,
    pub global_row: super::catalog::GlobalCatalogRow,
    pub drive_row: super::catalog::DriveCatalogRow,
}

impl CommitJournal {
    pub(super) fn write(&self, path: &Path) -> Result<()> {
        let body = toml::to_string_pretty(self).context("序列化事务阶段失败")?;
        durable::write_synced(path, body.as_bytes()).context("写事务阶段失败")
    }

    pub(super) fn advance(&mut self, stage: CommitStage, path: &Path) -> Result<()> {
        self.stage = stage;
        self.write(path)
    }

    pub(super) fn commit_metadata(&mut self, cfg: &Config, path: &Path) -> Result<()> {
        let drive_root = PathBuf::from(&self.drive_root);
        let manifest_path = paths::drive_manifest_dir(&drive_root)
            .join(format!("{}.sha256.csv", self.pending.project_dest_name));
        let snapshot = crate::engine::manifest::Manifest {
            entries: self.entries.clone(),
        };
        // Existing manifest evidence is never overwritten. Replay can only reuse an identical
        // verified snapshot; a new manifest is atomically published with no-replace.
        if fs::symlink_metadata(&manifest_path).is_ok() {
            let existing = super::incremental::read_verified_manifest(&manifest_path)?;
            anyhow::ensure!(
                super::incremental::fingerprint_from_manifest(&existing)?.sha256
                    == self.snapshot.as_ref().and_then(|fp| fp.sha256.clone()),
                "已有校验清单与事务快照不同；原清单与事务证据保留"
            );
        } else {
            snapshot.write_csv_new(&manifest_path)?;
        }
        self.advance(CommitStage::ManifestWritten, path)?;
        super::catalog::append_global_catalog(
            &paths::system_global_catalog(&cfg.system_root),
            &self.global_row,
        )?;
        self.advance(CommitStage::GlobalWritten, path)?;
        super::catalog::append_drive_catalog(
            &paths::drive_catalog_path(&drive_root),
            &self.drive_row,
        )?;
        self.advance(CommitStage::DriveWritten, path)?;
        if self.incremental {
            let fp = self
                .snapshot
                .clone()
                .context("增量事务缺少已校验内容快照")?;
            let mut index = super::incremental::JsonlIncrementalIndex::open_for_source(
                &cfg.system_root,
                Path::new(&self.source_root),
            )?;
            index.upsert_verified(
                &self.source_id,
                &self.source_relative,
                fp,
                &drive_root,
                &self.pending.drive_id,
                &self.pending.project_dest_name,
                &self.drive_row.verify_status,
                &self.pending.started_at,
            )?;
            index.flush()?;
        }
        self.advance(CommitStage::IncrementalWritten, path)
    }
}

pub(super) fn move_source_no_replace(
    source: &Path,
    destination: &Path,
    destination_parent: &crate::engine::destination::SafeDir,
) -> Result<()> {
    let from = crate::engine::destination::SafeDir::open(
        source.parent().context("源路径缺少父目录")?,
        false,
    )?;
    from.rename_entry_to(
        Path::new(source.file_name().context("源路径缺少文件名")?),
        destination_parent,
        Path::new(destination.file_name().context("已备份源路径缺少文件名")?),
    )
}

fn safe_archive_leaf(name: &str) -> bool {
    if !super::incremental::safe_component(name)
        || name.contains('\0')
        || !matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
    {
        return false;
    }
    #[cfg(windows)]
    {
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        let device_suffix = stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"));
        if name.ends_with(['.', ' '])
            || name
                .chars()
                .any(|c| (c as u32) <= 31 || "<>\"|?*".contains(c))
            || matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            )
            || device_suffix.is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        {
            return false;
        }
    }
    true
}

/// A v1 journal may contain the original source leaf or the exact collision-name shape
/// emitted by commit_archive. Other names cannot acquire ownership through recovery.
pub(super) fn archive_leaf_matches_source(source: &str, archived: &str) -> bool {
    if !safe_archive_leaf(source) || !safe_archive_leaf(archived) {
        return false;
    }
    if source == archived {
        return true;
    }
    let Some(suffix) = archived
        .strip_prefix(source)
        .and_then(|s| s.strip_prefix('_'))
    else {
        return false;
    };
    let (stamp, ordinal) = suffix
        .split_once('_')
        .map_or((suffix, None), |(s, n)| (s, Some(n)));
    stamp.len() == 14
        && stamp.bytes().all(|c| c.is_ascii_digit())
        && chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d%H%M%S").is_ok()
        && ordinal.is_none_or(|n| {
            n.parse::<usize>()
                .is_ok_and(|value| value >= 2 && value.to_string() == n)
        })
}

/// Normalize only an already no-link-opened parent. Keep both bindings alive and retain
/// the original journal bytes; normal and verbatim Windows paths name the same parent.
fn recovery_move_destination(
    cfg: &Config,
    journal: &CommitJournal,
) -> Result<(
    crate::engine::destination::SafeDir,
    crate::engine::destination::SafeDir,
    PathBuf,
)> {
    let moved_to = Path::new(&journal.pending.move_to);
    anyhow::ensure!(
        moved_to.is_absolute()
            && !moved_to
                .components()
                .any(|c| matches!(c, Component::ParentDir)),
        "事务移动目标不是安全的绝对路径；证据保留"
    );
    let leaf = moved_to
        .file_name()
        .and_then(|n| n.to_str())
        .context("事务移动目标缺少正常文件名；证据保留")?;
    anyhow::ensure!(
        journal.pending.move_to.ends_with(leaf)
            && Path::new(&journal.pending.src_path)
                .file_name()
                .and_then(|n| n.to_str())
                == Some(journal.pending.project_src_name.as_str())
            && archive_leaf_matches_source(&journal.pending.project_src_name, leaf),
        "事务移动目标名不安全或与源身份不一致；证据保留"
    );
    let configured = crate::engine::destination::SafeDir::open(&cfg.archived_root, false)?;
    let parent = moved_to
        .parent()
        .context("事务移动目标缺少父路径；证据保留")?;
    let recorded = crate::engine::destination::SafeDir::open(parent, false)?;
    configured.require_current_binding()?;
    recorded.require_current_binding()?;
    let authorized_root = cfg
        .archived_root
        .canonicalize()
        .context("已备份源根不可用；证据保留")?;
    anyhow::ensure!(
        parent.canonicalize()? == authorized_root,
        "事务移动目标不属于当前授权的已备份源根；证据保留"
    );
    configured.require_current_binding()?;
    recorded.require_current_binding()?;
    let normalized = authorized_root.join(leaf);
    if fs::symlink_metadata(&normalized).is_ok() {
        anyhow::ensure!(
            super::incremental::trusted_tree_under(&authorized_root, &normalized)?,
            "已备份源路径包含链接或重定向；标记保留"
        );
    }
    Ok((configured, recorded, normalized))
}

/// Production recovery also binds the journal to the exact currently selected physical root.
pub(super) fn check_pending_txn_for_drive(
    cfg: &Config,
    reporter: &dyn Reporter,
    drive: &crate::engine::drive::DriveInfo,
) -> Result<()> {
    let path = paths::system_pending_txn(&cfg.system_root);
    if path.exists() {
        let text = fs::read_to_string(&path).context("读事务标记失败；证据保留")?;
        let value: toml::Value = toml::from_str(&text).context("解析事务标记失败；证据保留")?;
        if value.get("version").and_then(toml::Value::as_integer) == Some(1) {
            let journal: CommitJournal = toml::from_str(&text)?;
            anyhow::ensure!(
                Path::new(&journal.drive_root).canonicalize()? == drive.root.canonicalize()?,
                "事务记录的实际备份盘路径与本轮目标不同；证据保留"
            );
        }
    }
    check_pending_txn(cfg, reporter, &drive.id)
}

pub(super) fn check_pending_txn(
    cfg: &Config,
    reporter: &dyn Reporter,
    redo_drive_id: &str,
) -> Result<()> {
    let path = paths::system_pending_txn(&cfg.system_root);
    if !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(&path).context("读事务标记失败；恢复证据保留")?;
    let value: toml::Value = toml::from_str(&text).context("解析事务标记失败；恢复证据保留")?;
    anyhow::ensure!(
        value.get("version").and_then(toml::Value::as_integer) == Some(1),
        "事务标记缺少可验证的提交阶段及内容快照；请人工核对，恢复证据保留"
    );
    let mut journal: CommitJournal =
        toml::from_str(&text).context("解析事务阶段失败；恢复证据保留")?;
    anyhow::ensure!(
        journal.pending.drive_id.trim() == redo_drive_id.trim(),
        "未完成事务在另一块备份盘；请插回原盘，事务标记保留"
    );
    let drive_root = PathBuf::from(&journal.drive_root);
    crate::engine::destination::SafeDir::open(&drive_root, false)?;
    let root = drive_root
        .canonicalize()
        .context("原事务备份盘不可用；事务标记保留")?;
    let id = fs::read_to_string(paths::drive_id_path(&root)).context("无法复验原事务备份盘身份")?;
    anyhow::ensure!(
        id.trim() == journal.pending.drive_id.trim(),
        "原事务备份盘身份不符；标记保留"
    );
    anyhow::ensure!(
        super::incremental::safe_component(&journal.pending.project_dest_name),
        "事务目的名不安全"
    );
    anyhow::ensure!(
        journal.global_row.folder_name == journal.pending.project_dest_name
            && journal.global_row.drive_name == journal.pending.drive_id
            && journal.drive_row.project_name == journal.pending.project_dest_name
            && journal.global_row.verify == "SHA256-OK"
            && journal.drive_row.verify_status == "SHA256-OK",
        "事务清单与索引身份不一致；证据保留"
    );
    let source_root = Path::new(&journal.source_root);
    anyhow::ensure!(
        super::incremental::source_id_for(source_root) == journal.source_id,
        "事务源根身份不一致；证据保留"
    );
    let relative = Path::new(&journal.pending.src_path)
        .strip_prefix(source_root)
        .context("事务源路径不在记录的源根内；证据保留")?;
    anyhow::ensure!(
        super::types::source_relative_key(relative)? == journal.source_relative,
        "事务源相对身份不一致；证据保留"
    );
    let payload = paths::drive_projects_dir(&root).join(&journal.pending.project_dest_name);
    anyhow::ensure!(
        super::incremental::trusted_tree_under(&root, &payload)?,
        "事务副本包含链接或缺失；标记保留"
    );
    let saved = crate::engine::manifest::Manifest {
        entries: journal.entries.clone(),
    };
    let expected = super::incremental::fingerprint_from_manifest(&saved)
        .context("事务未保存可信的内容清单；请人工核对，标记保留")?;
    anyhow::ensure!(
        journal
            .snapshot
            .as_ref()
            .is_some_and(|fp| fp.sha256 == expected.sha256
                && fp.size == expected.size
                && fp.file_count == expected.file_count),
        "事务内容快照不匹配；标记保留"
    );
    let backup = crate::engine::destination::SafeDir::open(&payload, false)?;
    let actual = crate::engine::manifest::build_guarded(
        &backup,
        crate::engine::manifest::ManifestOpts { no_hash: false },
        reporter,
    )?;
    anyhow::ensure!(
        crate::engine::manifest::diff(&saved, &actual, true).ok(),
        "原事务副本校验失败；事务标记保留"
    );
    // Reject untrusted move paths before any metadata replay can rewrite the evidence.
    let move_destination =
        if !journal.retain_source && journal.stage != CommitStage::SourceCompleted {
            Some(recovery_move_destination(cfg, &journal)?)
        } else {
            None
        };
    reporter.action(&format!(
        "恢复已校验的事务副本：{}（阶段 {:?}）。",
        journal.pending.project_dest_name, journal.stage
    ));
    backup.require_current_binding()?;
    journal.commit_metadata(cfg, &path)?;
    backup.require_current_binding()?;
    if let Some((archived_parent, recorded_parent, moved_to)) = move_destination.as_ref() {
        let source = Path::new(&journal.pending.src_path);
        archived_parent.require_current_binding()?;
        recorded_parent.require_current_binding()?;
        if fs::symlink_metadata(moved_to).is_ok() {
            anyhow::ensure!(
                super::incremental::trusted_tree_under(
                    moved_to.parent().context("已备份源缺少父路径")?,
                    moved_to
                )?,
                "已备份源路径包含链接或重定向；标记保留"
            );
            let moved = crate::engine::manifest::build(
                moved_to,
                crate::engine::manifest::ManifestOpts { no_hash: false },
                reporter,
            )?;
            anyhow::ensure!(
                crate::engine::manifest::diff(&saved, &moved, true).ok(),
                "已备份源路径含不同内容；不能完成恢复，标记保留"
            );
        } else {
            anyhow::ensure!(source.exists(), "原事务源和已备份源均不可用；标记保留");
            anyhow::ensure!(
                source.canonicalize()? == source,
                "事务源路径已被链接或重定向；标记保留"
            );
            crate::engine::destination::SafeDir::open(
                source.parent().context("事务源缺少父路径")?,
                false,
            )?;
            let current = crate::engine::manifest::build(
                source,
                crate::engine::manifest::ManifestOpts { no_hash: false },
                reporter,
            )?;
            anyhow::ensure!(
                crate::engine::manifest::diff(&saved, &current, true).ok(),
                "原事务源已变更；不会移动新内容，事务标记保留"
            );
            backup.require_current_binding()?;
            archived_parent.require_current_binding()?;
            recorded_parent.require_current_binding()?;
            move_source_no_replace(source, moved_to, archived_parent)
                .context("恢复移源失败；事务标记保留")?;
        }
        archived_parent.require_current_binding()?;
        recorded_parent.require_current_binding()?;
    }
    journal.advance(CommitStage::SourceCompleted, &path)?;
    txn::PendingTxn::clear(&path)?;
    reporter.action("已补齐该事务的清单、本盘/全局索引和增量快照，恢复完成。");
    Ok(())
}

#[cfg(test)]
#[path = "recovery_path_regression_tests.rs"]
mod recovery_path_regression_tests;

/// 清除事务标记。删除失败(AV 占用/只读/权限)时 warn 而非静默 —— 否则下轮启动会对一个
/// 已正确处理的事务重复触发恢复,且"已清除标记"的提示与磁盘实际状态(标记仍在)不符。(review-r2 R2-3)
#[cfg(test)]
pub(super) fn clear_marker(path: &Path, reporter: &dyn Reporter) {
    if let Err(e) = fs::remove_file(path) {
        reporter.warn(&format!(
            "清除事务标记失败({}):{} —— 标记仍在,下次启动可能再次提示此事务;如反复出现请手动删除该文件。",
            e,
            path.display()
        ));
    }
}

#[cfg(test)]
pub(super) fn global_has_folder(global: &Path, folder_name: &str) -> Result<bool> {
    if !global.is_file() {
        return Ok(false);
    }
    let mut rdr = csv::Reader::from_path(global)?;
    let headers = rdr.headers()?.clone();
    // 同 catalog_has_project:缺『文件夹名』列 = 索引不可信,bail 而非静默 Ok(false),避免把损坏全局索引
    // 当成空索引导致恢复判定把『已索引』误判为『未索引』。(review-r3 round5)
    let Some(col) = headers.iter().position(|h| h == "文件夹名") else {
        anyhow::bail!(
            "全局索引缺少『文件夹名』列,索引不可信(可能损坏或被改格式):{}",
            global.display()
        );
    };
    // 同 catalog_has_project:NTFS 大小写折叠,仅大小写不同的同名也算已登记;非 Windows 精确比较。(强优化 review)
    let want = if cfg!(windows) {
        folder_name.to_lowercase()
    } else {
        folder_name.to_string()
    };
    for rec in rdr.records() {
        let rec = rec?;
        let hit = rec
            .get(col)
            .map(|v| {
                if cfg!(windows) {
                    v.to_lowercase() == want
                } else {
                    v == want
                }
            })
            .unwrap_or(false);
        if hit {
            return Ok(true);
        }
    }
    Ok(false)
}

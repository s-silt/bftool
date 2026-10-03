//! 归档计划：选盘 + 项目动作（只读）。
use anyhow::{Context, Result};
use chrono::Local;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::engine::drive::BackupDrive;
use crate::engine::{paths, safety};
use crate::pool::{DrivePool, LocalDrivePool, PoolPolicy};
use crate::reporter::Reporter;

use super::catalog::{catalog_has_project, global_catalog_has_project_on_drive};
use super::copy::{discover_projects, folder_stats, source_bftool_part_files};
use super::incremental::{
    classify, fingerprint_from_stats, normalize_exts, project_has_ext, resolve_incremental,
    source_id_for, IncrementalVerifyMode, JsonlIncrementalIndex, SKIP_UNCHANGED,
};
use super::types::{
    source_relative_key, ArchivePlan, ArchiveSummary, NoWritableDrive, Options, PlanAction,
    PlanItem,
};
use crate::observe::ReporterSink;
use crate::pipeline::run_archive_preflight;

fn destination_indexed(cfg: &Config, drive: &BackupDrive, name: &str) -> Result<bool> {
    let local = catalog_has_project(&paths::drive_catalog_path(&drive.root), name)?;
    let global = global_catalog_has_project_on_drive(
        &paths::system_global_catalog(&cfg.system_root),
        name,
        &drive.id,
    )?;
    let manifest = paths::drive_manifest_dir(&drive.root).join(format!("{name}.sha256.csv"));
    let evidence_exists = match std::fs::symlink_metadata(&manifest) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("无法判断既有校验清单占用"),
    };
    Ok(local || global || evidence_exists)
}

fn opts_with_cfg_verify(cfg: &Config, opts: &Options) -> Options {
    let mut o = opts.clone();
    // CLI/GUI 未显式改时用配置文件
    if o.incremental {
        o.incremental_verify = IncrementalVerifyMode::parse(&cfg.incremental_verify);
    }
    o
}

pub fn plan(cfg: &Config, opts: &Options, reporter: &dyn Reporter) -> Result<ArchivePlan> {
    let opts = &opts_with_cfg_verify(cfg, opts);
    // Spec B D14/D15 fail-closed:无校验组合连预览都不给(判定收口到 verify_disabled)。(ledger L-008)
    let sink = ReporterSink { reporter };
    run_archive_preflight(cfg, opts, reporter, &sink)?;

    // 选盘:override 走 info_by_letter,否则 pick_active(唯一未封盘且达标)。
    let pool = LocalDrivePool;
    let policy = PoolPolicy::from_min_gb(cfg.min_drive_gb);
    let drive = match opts.drive_letter_override.as_deref() {
        Some(letter) => pool.by_letter(letter)?.into_drive_info()?,
        None => match pool.pick_writable(&policy, reporter)? {
            Some(d) => d.into_drive_info()?,
            None => {
                reporter.error("未发现已初始化且未封盘的备份盘。");
                reporter.info("插入备份盘或目标盘后运行：bftool init <盘符>（例：bftool init E）");
                return Err(NoWritableDrive.into());
            }
        },
    };
    // 类型化盘:封盘/过小在此挡掉(编译期挡"写错盘"的运行期对等)。(Spec D §4.2 / L-021)
    let drive = match drive.clone().try_into_writable(cfg.min_drive_gb) {
        Ok(w) => w.into_inner(),
        Err(e) => {
            reporter.error(&format!(
                "盘 {} ({}:) 不可写入：{}",
                drive.id, drive.letter, e
            ));
            return Err(NoWritableDrive.into());
        }
    };
    reporter.ok(&format!("当前备份盘: {} ({}:)", drive.id, drive.letter));

    let warnings = safety::check_paths(
        &cfg.ready_root,
        &cfg.archived_root,
        &cfg.system_root,
        Some(&drive.root),
    )?;
    for w in warnings {
        reporter.warn(&w);
    }

    let source_root = effective_source_root(cfg, opts);
    // Folder 源额外做一次路径关系检查（与三根/备份盘互斥同口径）
    if opts.source_override.is_some() {
        let extra = safety::check_paths(
            &source_root,
            &cfg.archived_root,
            &cfg.system_root,
            Some(&drive.root),
        )?;
        for w in extra {
            reporter.warn(&w);
        }
    }

    // 列出待归档项目(读不到/为空 → 空计划,友好提示)。
    let mut items = Vec::new();
    if !source_root.is_dir() {
        // AR-16: 源根非目录时**有意**返回空 items 而非 Err
        reporter.error(&format!("待备份/源目录 不存在：{}", source_root.display()));
        return Ok(ArchivePlan {
            drive,
            items,
            opts: opts.clone(),
        });
    }
    let source_root = source_root.canonicalize().context("解析实际源根失败")?;
    let extra = safety::check_paths(
        &source_root,
        &cfg.archived_root,
        &cfg.system_root,
        Some(&drive.root),
    )?;
    for warning in extra {
        reporter.warn(&warning);
    }
    let projects = discover_and_filter(&source_root, opts, reporter)?;
    if projects.is_empty() {
        reporter.info("源目录中没有待归档项目，结束。");
    } else {
        reporter.info(&format!(
            "发现 {} 个待归档项目（按编号升序处理）。",
            projects.len()
        ));
    }
    items = plan_items(cfg, &drive, &projects, opts, &source_root);
    Ok(ArchivePlan {
        drive,
        items,
        opts: opts.clone(),
    })
}

/// 在已选定的盘上做只读计划（跳过 pick_active）。供 watch 测试注入假盘，以及显式盘编排。
pub fn plan_on_drive(
    cfg: &Config,
    drive: BackupDrive,
    opts: &Options,
    reporter: &dyn Reporter,
) -> Result<ArchivePlan> {
    let opts = &opts_with_cfg_verify(cfg, opts);
    let sink = ReporterSink { reporter };
    run_archive_preflight(cfg, opts, reporter, &sink)?;
    let drive = match drive.clone().try_into_writable(cfg.min_drive_gb) {
        Ok(w) => w.into_inner(),
        Err(e) => {
            reporter.error(&format!(
                "盘 {} ({}:) 不可写入：{}",
                drive.id, drive.letter, e
            ));
            return Err(NoWritableDrive.into());
        }
    };
    reporter.ok(&format!("当前备份盘: {} ({}:)", drive.id, drive.letter));
    let warnings = safety::check_paths(
        &cfg.ready_root,
        &cfg.archived_root,
        &cfg.system_root,
        Some(&drive.root),
    )?;
    for w in warnings {
        reporter.warn(&w);
    }
    let source_root = effective_source_root(cfg, opts);
    let mut items = Vec::new();
    if !source_root.is_dir() {
        reporter.error(&format!("待备份/源目录 不存在：{}", source_root.display()));
        return Ok(ArchivePlan {
            drive,
            items,
            opts: opts.clone(),
        });
    }
    let source_root = source_root.canonicalize().context("解析实际源根失败")?;
    let extra = safety::check_paths(
        &source_root,
        &cfg.archived_root,
        &cfg.system_root,
        Some(&drive.root),
    )?;
    for warning in extra {
        reporter.warn(&warning);
    }
    let projects = discover_and_filter(&source_root, opts, reporter)?;
    if projects.is_empty() {
        reporter.info("源目录中没有待归档项目，结束。");
    } else {
        reporter.info(&format!(
            "发现 {} 个待归档项目（按编号升序处理）。",
            projects.len()
        ));
    }
    items = plan_items(cfg, &drive, &projects, opts, &source_root);
    Ok(ArchivePlan {
        drive,
        items,
        opts: opts.clone(),
    })
}

/// 对一组项目算计划:跨项目**递减剩余容量**。每裁决一个 Archive/RenameAndArchive 就从
/// running_free 扣掉它的占用(est_bytes),使后续项目看到「前面项目占用后」的剩余。
/// 否则每个项目都对同一份冻结 free_bytes 独立判断 → 多个项目各自放得下、累计放不下时
/// 不会触发封盘,预览高估、执行才在磁盘满时失败。(review-r2 #2)
fn effective_source_root(cfg: &Config, opts: &Options) -> PathBuf {
    opts.source_override
        .clone()
        .unwrap_or_else(|| cfg.ready_root.clone())
}

fn discover_and_filter(
    source_root: &Path,
    opts: &Options,
    reporter: &dyn Reporter,
) -> Result<Vec<PathBuf>> {
    use std::collections::BTreeSet;
    let exts = opts
        .include_ext
        .as_ref()
        .map(|v| normalize_exts(v))
        .unwrap_or_default();
    let has_file_filter = !exts.is_empty() || !opts.file_globs.is_empty();

    let mut units: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let norm_key = |p: &Path| -> Result<String> {
        source_relative_key(
            p.strip_prefix(source_root)
                .context("发现的源不在指定根内")?,
        )
    };

    // FolderProjects（默认 / 并存）
    if opts.include_subfolder_projects {
        let projects = discover_projects(source_root, reporter)?;
        for p in projects {
            // 若仅开了 ext 过滤且项目内无匹配文件，则跳过该项目（仍可被文件级命中）
            if has_file_filter && !exts.is_empty() && !project_has_ext(&p, &exts) {
                continue;
            }
            let k = norm_key(&p)?;
            if seen.insert(k) {
                units.push(p);
            }
        }
    }

    // FileExtensions：匹配文件作为计划单元（与项目并存、按 rel 去重）
    if has_file_filter {
        let files = super::incremental::discover_matching_files(
            source_root,
            &exts,
            &opts.file_globs,
            opts.ext_recursive,
        )?;
        for f in files {
            let k = norm_key(&f)?;
            // 若某项目目录已入选，跳过其内部已被项目覆盖的文件？并存规则：文件级命中不重复归入同一项目。
            // 已作为项目根的路径跳过；位于已选项目目录下的文件也跳过（项目级会整夹备）。
            let under_selected_proj = units.iter().any(|u| u.is_dir() && f.starts_with(u));
            if under_selected_proj {
                continue;
            }
            if seen.insert(k) {
                units.push(f);
            }
        }
        reporter.info(&format!(
            "文件级筛选：exts={:?} recursive={} → 本轮计划单元 {} 个",
            exts,
            opts.ext_recursive,
            units.len()
        ));
    }

    Ok(units)
}

pub(super) fn plan_items(
    cfg: &Config,
    drive: &BackupDrive,
    projects: &[PathBuf],
    opts: &Options,
    source_root: &Path,
) -> Vec<PlanItem> {
    // 计划只读取已提交索引。seed、refresh、flush 只能在持锁的执行/提交阶段进行。
    let index = if opts.incremental {
        JsonlIncrementalIndex::open_for_source(&cfg.system_root, source_root).ok()
    } else {
        None
    };
    let mut items = Vec::with_capacity(projects.len());
    let mut running_free = drive.free_bytes;
    let mut reserved = BTreeSet::new();
    for proj in projects {
        let mut drive_now = drive.clone();
        drive_now.free_bytes = running_free;
        let mut item = decide(cfg, &drive_now, proj, opts, index.as_ref(), source_root);
        // 同一轮递归文件筛选可出现多个同 basename 的源，目标身份也必须互不冲突。
        if let PlanAction::Archive { dest_name } | PlanAction::RenameAndArchive { dest_name } =
            &item.action
        {
            let base = dest_name.clone();
            let mut candidate = base.clone();
            let mut suffix = 2u64;
            let key_for = |name: &str| {
                if cfg!(windows) {
                    name.to_lowercase()
                } else {
                    name.to_owned()
                }
            };
            loop {
                match destination_indexed(cfg, drive, &candidate) {
                    Ok(indexed) => {
                        if !indexed
                            && !reserved.contains(&key_for(&candidate))
                            && std::fs::symlink_metadata(
                                paths::drive_projects_dir(&drive.root).join(&candidate),
                            )
                            .is_err()
                        {
                            reserved.insert(key_for(&candidate));
                            item.dest_existed_at_plan = false;
                            item.action = if candidate == item.name {
                                PlanAction::Archive {
                                    dest_name: candidate,
                                }
                            } else {
                                PlanAction::RenameAndArchive {
                                    dest_name: candidate,
                                }
                            };
                            break;
                        }
                        candidate = format!("{base}__{suffix}");
                        suffix += 1;
                    }
                    Err(e) => {
                        item.action = PlanAction::Skip(format!("读取本盘索引失败：{e}"));
                        break;
                    }
                }
            }
        }
        if matches!(
            item.action,
            PlanAction::Archive { .. } | PlanAction::RenameAndArchive { .. }
        ) {
            running_free = running_free.saturating_sub(item.est_bytes);
        }
        items.push(item);
    }
    items
}

/// 对单个项目算计划动作——**只读**(统计与可信历史内容复验，不写源、目标或索引)。
/// 重名时在此**冻结**时间戳目标名(预览=执行同名)。容量判定与 handle_one 同序:先"超单盘容量"再"余量不足"。
pub(super) fn decide(
    cfg: &Config,
    drive: &BackupDrive,
    proj_path: &Path,
    opts: &Options,
    index: Option<&JsonlIncrementalIndex>,
    source_root: &Path,
) -> PlanItem {
    let source_root = source_root
        .canonicalize()
        .unwrap_or_else(|_| source_root.to_path_buf());
    let source_id = source_id_for(&source_root);
    let source_path = proj_path
        .canonicalize()
        .unwrap_or_else(|_| proj_path.to_path_buf());
    let source_relative = source_path
        .strip_prefix(&source_root)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let name = match proj_path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n.to_string(),
        None => {
            return PlanItem {
                name: proj_path.display().to_string(),
                source_root,
                source_path,
                source_relative,
                est_bytes: 0,
                dest_existed_at_plan: false,
                action: PlanAction::Skip("无效的项目目录名".into()),
            }
        }
    };
    let skip = |est: u64, reason: String| PlanItem {
        name: name.clone(),
        source_root: source_root.clone(),
        source_path: source_path.clone(),
        source_relative: source_relative.clone(),
        est_bytes: est,
        dest_existed_at_plan: false,
        action: PlanAction::Skip(reason),
    };

    let relative_key = match source_relative_key(&source_relative) {
        Ok(key) => key,
        Err(e) => return skip(0, format!("源身份不可验证：{e}")),
    };

    // 稳定性(未稳定是暂态:跳过不记台账,下轮再来)
    let st = safety::folder_stable(proj_path, cfg.stable_minutes);
    if !st.stable {
        return skip(0, format!("未稳定：{}", st.reason));
    }
    match source_bftool_part_files(proj_path) {
        Ok(files) if !files.is_empty() => {
            return skip(
                0,
                format!(
                    "项目内含 bftool 内部临时后缀 .bftool-part 的文件({})，为避免静默漏备已跳过；请改名或人工核对。",
                    files.join("、")
                ),
            )
        }
        Ok(_) => {}
        Err(e) => {
            return skip(
                0,
                format!("扫描 .bftool-part 文件失败({})→ 不归档,请先解决环境问题。", e),
            )
        }
    }
    // 统计 + 枚举/元数据错误(fail-closed)
    let stats = folder_stats(proj_path);
    if !stats.enum_errors.is_empty() || !stats.metadata_errors.is_empty() {
        return skip(
            stats.bytes,
            format!(
                "统计不完整({} 个枚举错误 + {} 个 metadata 错误)→ 不归档,请先解决环境问题(权限/超长路径/AV 锁)再重试。",
                stats.enum_errors.len(),
                stats.metadata_errors.len()
            ),
        );
    }
    // 0 真实文件(空目录或全 cruft):不归档不移源
    if stats.files == 0 {
        return skip(
            0,
            "项目内没有可备份的真实文件(空目录或全是 cruft)——不归档、不移源,请人工确认。".into(),
        );
    }
    let size = stats.bytes;
    let size_gb = size as f64 / 1024.0 / 1024.0 / 1024.0;
    // 重名(读本盘索引;损坏 → fail-closed skip)
    let catalog = paths::drive_catalog_path(&drive.root);
    let dup = match destination_indexed(cfg, drive, &name) {
        Ok(b) => b,
        Err(e) => {
            return skip(
                size,
                format!(
                "读本盘索引失败({}):无法判断是否重名 → 跳过,避免覆盖旧备份或重复写盘。请检查 {}",
                e,
                catalog.display()
            ),
            )
        }
    };
    // 增量跳过必须以已验证目的副本与当前完整源内容共同认证。
    if opts.incremental {
        let fp = fingerprint_from_stats(&stats);
        match index {
            None => {
                return skip(
                    size,
                    format!(
                        "增量索引不可用，无法判定是否未变 → 跳过（fail-closed）；请检查 {}",
                        paths::system_incremental_dir(&cfg.system_root).display()
                    ),
                );
            }
            Some(idx) => {
                let old = match idx.verified_fingerprint(&source_id, &relative_key, &drive.root) {
                    Ok(old) => old,
                    Err(e) => return skip(size, format!("增量历史副本不可验证：{e}")),
                };
                let kind = classify(old.as_ref(), &fp);
                let mode = opts.incremental_verify;
                match resolve_incremental(kind, old.as_ref(), &fp, proj_path, mode) {
                    Ok(res) if res.skip => {
                        return skip(size, SKIP_UNCHANGED.to_string());
                    }
                    Ok(_) => {
                        // Proceed to Archive / RenameAndArchive
                    }
                    Err(e) => {
                        return skip(size, format!("增量哈希确认失败({})→ 跳过本项", e));
                    }
                }
            }
        }
    }
    // 超单盘容量(空盘也放不下)
    let reserve = cfg.reserve_gb.saturating_mul(1024 * 1024 * 1024);
    if size > drive.total_bytes.saturating_sub(reserve) {
        return skip(
            size,
            format!(
                "项目 {:.2}GB 超过单盘容量，空盘也放不下 → 需人工拆分",
                size_gb
            ),
        );
    }
    // 既有目录（登记与否）都不是本次写入的所有权证明；冻结一个未占用的新名字。
    let projects_dir = paths::drive_projects_dir(&drive.root);
    let occupied = dup || std::fs::symlink_metadata(projects_dir.join(&name)).is_ok();
    let base = if occupied {
        format!("{}_{}", name, Local::now().format("%Y%m%d%H%M%S"))
    } else {
        name.clone()
    };
    let mut dest_name = base.clone();
    let mut suffix = 2u64;
    loop {
        let indexed = match destination_indexed(cfg, drive, &dest_name) {
            Ok(value) => value,
            Err(e) => return skip(size, format!("读取本盘索引失败：{e}")),
        };
        if !indexed && std::fs::symlink_metadata(projects_dir.join(&dest_name)).is_err() {
            break;
        }
        dest_name = format!("{base}__{suffix}");
        suffix += 1;
    }
    let dest_existed_at_plan = false;
    let need = size;
    if need.saturating_add(reserve) > drive.free_bytes {
        let need_gb = need as f64 / 1024.0 / 1024.0 / 1024.0;
        let free_gb = drive.free_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        let reason = format!(
            "盘 {} 余量不足以放下 {}（还需约 {:.2}GB + 余量；当前剩余 {:.2}GB）。封盘并请换下一块盘后重跑。",
            drive.id, name, need_gb, free_gb
        );
        return PlanItem {
            name,
            source_root,
            source_path,
            source_relative,
            est_bytes: size,
            dest_existed_at_plan,
            action: PlanAction::SealAndStop(reason),
        };
    }
    PlanItem {
        name: name.clone(),
        source_root,
        source_path,
        source_relative,
        est_bytes: size,
        dest_existed_at_plan,
        action: if dest_name != name {
            PlanAction::RenameAndArchive { dest_name }
        } else {
            PlanAction::Archive { dest_name }
        },
    }
}

/// 预览到执行期间，源或历史副本可能改变。增量 Skip 也必须再次读取完整可信内容。
pub(super) fn verified_unchanged(
    cfg: &Config,
    drive: &BackupDrive,
    item: &PlanItem,
    opts: &Options,
) -> Result<bool> {
    let index = JsonlIncrementalIndex::open_for_source(&cfg.system_root, &item.source_root)?;
    let source_id = source_id_for(&item.source_root);
    let relative = source_relative_key(&item.source_relative)?;
    let old = index.verified_fingerprint(&source_id, &relative, &drive.root)?;
    let stats = folder_stats(&item.source_path);
    if !stats.enum_errors.is_empty() || !stats.metadata_errors.is_empty() || stats.files == 0 {
        return Ok(false);
    }
    let fingerprint = fingerprint_from_stats(&stats);
    let kind = classify(old.as_ref(), &fingerprint);
    Ok(resolve_incremental(
        kind,
        old.as_ref(),
        &fingerprint,
        &item.source_path,
        opts.incremental_verify,
    )?
    .skip)
}

/// dry-run:只渲染计划、不执行。`handled` = 将归档的项目数(Archive/RenameAndArchive)。
pub(super) fn render_dry_run(plan: &ArchivePlan, reporter: &dyn Reporter) -> ArchiveSummary {
    let mut handled = 0usize;
    for it in &plan.items {
        let gb = it.est_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        match &it.action {
            PlanAction::Archive { dest_name } => {
                handled += 1;
                reporter.info(&format!(
                    "[演练] 将归档 {} (~{:.2}GB) → {}\\项目\\{}",
                    it.name, gb, plan.drive.id, dest_name
                ));
            }
            PlanAction::RenameAndArchive { dest_name } => {
                handled += 1;
                reporter.info(&format!(
                    "[演练] 将归档 {} (~{:.2}GB) → {}\\项目\\{}(本盘已有同名 → 改用唯一名)",
                    it.name, gb, plan.drive.id, dest_name
                ));
            }
            PlanAction::Skip(reason) => {
                reporter.info(&format!("[演练] 跳过 {}：{}", it.name, reason));
            }
            PlanAction::SealAndStop(reason) => {
                reporter.warn(&format!(
                    "[演练] {} → 此处将封盘停本轮：{}",
                    it.name, reason
                ));
                break; // 封盘点之后的项目本轮不会处理,预览也到此为止
            }
        }
    }
    reporter.ok("[演练] 结束(未真正复制/移动任何文件)。");
    ArchiveSummary {
        handled,
        ..Default::default()
    }
}

//! 单项目处理：handle_one / commit / 校验器探测。
use anyhow::{Context, Result};
use chrono::{Local, Utc};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::engine::archive_test::{self, Tester, TesterPaths};
use crate::engine::drive::{self, DriveInfo};
use crate::engine::manifest::{self, ManifestOpts};
use crate::engine::{paths, safety, txn};
use crate::reporter::Reporter;

use super::catalog::{
    catalog_has_project, global_catalog_has_project_on_drive, DriveCatalogRow, GlobalCatalogRow,
};
use super::copy::{copy_folder_into, folder_stats, leading_digits, source_bftool_part_files};
use super::incremental::{fingerprint_from_manifest, source_id_for};
use super::lock::{
    archive_leaf_matches_source, move_source_no_replace, note_manual, CommitJournal, CommitStage,
};
use super::types::{source_relative_key, verify_disabled, Options, VerifyStatus};

fn manifest_evidence_exists(drive: &DriveInfo, name: &str) -> bool {
    fs::symlink_metadata(paths::drive_manifest_dir(&drive.root).join(format!("{name}.sha256.csv")))
        .is_ok()
}

pub(crate) fn guard_verify_enabled(cfg: &Config, opts: &Options) -> Result<()> {
    if verify_disabled(opts.no_hash, cfg.test_archives, opts.no_test_archives) {
        // EH-006: 纯用户向文案（不含 core/调用方等内部开发说明），CLI 与 GUI 都直接展示。
        anyhow::bail!(
            "已停止：当前设置会让 SHA256 校验和压缩包测试同时关闭 —— 等于「没有做任何完整性校验」，\
             不能这样备份。\n\
             如何修（任选其一）：\n\
               - 不要使用 --unsafe-no-hash（让 SHA256 整文件校验兜底）；或\n\
               - 开启压缩包测试（不要把 test_archives 设为 false，也不要使用 --no-test-archives）"
        );
    }
    Ok(())
}

/// 顶层 tester detection:本轮只 detect 一次。no_hash 且一个测试器都没装 → fail-closed bail。
pub(super) fn detect_tester(
    cfg: &Config,
    opts: &Options,
    reporter: &dyn Reporter,
) -> Result<Option<(Tester, PathBuf)>> {
    if !cfg.test_archives || opts.no_test_archives {
        return Ok(None);
    }
    let p = TesterPaths {
        winrar: cfg.winrar_path.clone(),
        bandizip: cfg.bandizip_path.clone(),
        seven_zip: cfg.seven_zip_path.clone(),
    };
    match archive_test::detect(&p) {
        Some(t) => Ok(Some(t)),
        None => {
            if opts.no_hash {
                // AR-08: 与 guard_verify_enabled 保持一致的措辞风格——都是"拒绝运行：…等价于没在做完整性校验"。
                anyhow::bail!(
                    "拒绝运行：开启了 --unsafe-no-hash 但没有可用的压缩包测试器(WinRAR/Bandizip/7-Zip 均未检测到)。\n\
                     此时 SHA256 与 archive test 均不运行,等价于「没在做完整性校验」。\n\
                     如何修：\n\
                       - 装一个测试器：推荐 7-Zip(免费开源 https://7-zip.org);或\n\
                       - 去掉 --unsafe-no-hash,让 SHA256 兜底"
                );
            }
            reporter.warn(
                "未检测到 WinRAR / Bandizip / 7-Zip —— 压缩包内部结构无法测试。\
                 重要资料建议装一个(推荐 7-Zip 免费开源：https://7-zip.org)。\
                 SHA256 整文件校验仍在正常进行。",
            );
            Ok(None)
        }
    }
}

pub(super) enum HandleOutcome {
    /// 归档成功;携带**实际归档的源字节数**,供 run_plan 用真实占用累计 consumed。(review-r2 R2-2)
    Done(u64),
    Skipped,
    /// 实际复制/内容校验失败，必须进入顶层失败汇总。
    Failed,
    /// 冻结的目标名在执行时已被占用(预览→执行间状态变了)——不写盘,不计 failed。(Spec D §4.1)
    StalePlan,
    DriveSealed,
    TesterFatallyDisabled,
    /// 提交中途移源失败:索引已落盘、源未移动,事务标记**保留**待恢复。run_plan 收到后必须
    /// 立即中止本轮,否则下一个项目的 pending.write 会覆盖该标记、抹掉恢复证据。(review-r2 R3-3)
    CommitInterrupted,
}

/// Copy into a genuinely new destination, verify the reserved directory object, then
/// commit its verified snapshot. Temporary instability remains distinct from integrity failure.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_one(
    cfg: &Config,
    reporter: &dyn Reporter,
    drive: &DriveInfo,
    proj_path: &Path,
    opts: &Options,
    tester_opt: Option<&(Tester, PathBuf)>,
    forced_dest_name: Option<(&str, bool)>,
) -> Result<HandleOutcome> {
    let sink = crate::observe::ReporterSink { reporter };
    if let crate::pipeline::StageOutcome::Deny { message, .. } =
        crate::pipeline::stages::hash_policy::run_hash_policy(cfg, opts, &sink)?
    {
        anyhow::bail!(message);
    }
    let name = proj_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("无效的项目目录名"))?
        .to_string();
    let proj_no = leading_digits(&name);
    reporter.action(&format!("=== 处理: {} ===", name));

    // 稳定性
    let st = safety::folder_stable(proj_path, cfg.stable_minutes);
    if !st.stable {
        reporter.warn(&format!("跳过（未稳定）：{}", st.reason));
        return Ok(HandleOutcome::Skipped);
    }

    match source_bftool_part_files(proj_path) {
        Ok(files) if !files.is_empty() => {
            let msg = format!(
                "项目内含 bftool 内部临时后缀 .bftool-part 的文件({})，为避免静默漏备已跳过。\
                 请改名或人工核对后重试。",
                files.join("、")
            );
            reporter.error(&msg);
            note_manual(cfg, reporter, &name, &msg);
            return Ok(HandleOutcome::Failed);
        }
        Ok(_) => {}
        Err(e) => {
            let msg = format!(
                "扫描 .bftool-part 文件失败({})→ 不归档,请先解决环境问题。",
                e
            );
            reporter.error(&msg);
            note_manual(cfg, reporter, &name, &msg);
            return Ok(HandleOutcome::Failed);
        }
    }

    // 体积 + 目标已存在量（断点续传时仅按"还需写入"判断）
    let stats = folder_stats(proj_path);
    let size = stats.bytes;
    let size_gb = size as f64 / 1024.0 / 1024.0 / 1024.0;

    // 枚举/元数据错误:dry-run 与正式路径都要 fail-closed —— 此前只有 dry-run 报告,
    // 正式路径靠后面 manifest::build 兜底但太晚,且容量按可能偏小的 size 误判。(ledger L-005)
    if !stats.enum_errors.is_empty() || !stats.metadata_errors.is_empty() {
        for e in &stats.enum_errors {
            reporter.error(&format!("枚举失败：{}", e));
        }
        for e in &stats.metadata_errors {
            reporter.error(&format!("读元数据失败：{}", e));
        }
        let msg = format!(
            "统计不完整({} 个枚举错误 + {} 个 metadata 错误)→ 不归档。请先解决环境问题\
             (权限拒绝？路径过长？被 AV 锁住？)再重试。",
            stats.enum_errors.len(),
            stats.metadata_errors.len()
        );
        reporter.error(&msg);
        note_manual(cfg, reporter, &name, &msg);
        return Ok(HandleOutcome::Failed);
    }

    // 重名保护 / 冻结名复验
    let catalog = paths::drive_catalog_path(&drive.root);
    let dup_in_drive = match catalog_has_project(&catalog, &name) {
        Ok(b) => b,
        Err(e) => {
            // 本盘索引存在但读不出(损坏/非 UTF-8):fail-closed —— 不确定是否重名时,既不能覆盖
            // 旧备份,也不该每轮用新时间戳名重复全量写盘(会塞满)。停下让用户修索引。(Phase 4 F-4;修订 L-017)
            reporter.error(&format!(
                "读本盘索引失败({}):无法判断是否重名 → 跳过本项目,避免覆盖旧备份或重复写盘。请检查 {}",
                e,
                catalog.display()
            ));
            note_manual(
                cfg,
                reporter,
                &name,
                &format!(
                    "本盘索引读取失败:{} —— 请人工检查/修复 {}",
                    e,
                    catalog.display()
                ),
            );
            return Ok(HandleOutcome::Failed);
        }
    };
    let dest_name = match forced_dest_name {
        // run_plan 路径:用冻结名,但**复验**它未被占用(预览→执行间可能有人占了它)。
        // 时间戳唯一名(重名场景)不会撞;普通名若现在已存在 → 计划已过期(StalePlan),不照旧误写。
        Some((frozen, _dest_existed_at_plan)) => {
            let drive_projects = paths::drive_projects_dir(&drive.root);
            let dest_phys = drive_projects.join(frozen);
            // 索引检查:frozen==name 时用 dup_in_drive;重命名场景重新查 catalog。
            // 额外 OR 物理路径检查:索引与磁盘不一致时(写索引前宕机等),磁盘上已有目录也算被占用。
            let indexed = if frozen == name {
                dup_in_drive
            } else {
                match catalog_has_project(&catalog, frozen) {
                    Ok(b) => b,
                    Err(e) => {
                        reporter.error(&format!(
                            "读本盘索引失败({}):无法复验冻结目标名 → 跳过本项目,避免覆盖旧备份或重复写盘。请检查 {}",
                            e,
                            catalog.display()
                        ));
                        note_manual(
                            cfg,
                            reporter,
                            &name,
                            &format!(
                                "本盘索引读取失败:{} —— 请人工检查/修复 {}",
                                e,
                                catalog.display()
                            ),
                        );
                        return Ok(HandleOutcome::Failed);
                    }
                }
            };
            let occupied_after_plan = fs::symlink_metadata(&dest_phys).is_ok();
            // Any catalog, payload, or manifest occupancy requires a new plan; existence
            // never grants continuation ownership.
            if indexed
                || manifest_evidence_exists(drive, frozen)
                || global_catalog_has_project_on_drive(
                    &paths::system_global_catalog(&cfg.system_root),
                    frozen,
                    &drive.id,
                )?
            {
                reporter.warn(&format!(
                    "计划已过期：目标名『{}』已有索引或清单证据 → 跳过本项目,请重新规划新目标。",
                    frozen
                ));
                return Ok(HandleOutcome::StalePlan);
            }
            if occupied_after_plan {
                reporter.warn(&format!(
                    "计划已过期：目标目录『{}』已被占用且没有可信续传所有权 → 跳过本项目,请重新规划新目标。",
                    frozen
                ));
                return Ok(HandleOutcome::StalePlan);
            }
            frozen.to_string()
        }
        // 直调路径:自行裁决,重名时现算唯一时间戳名(旧行为)。
        None => {
            let projects = paths::drive_projects_dir(&drive.root);
            if dup_in_drive
                || manifest_evidence_exists(drive, &name)
                || fs::symlink_metadata(projects.join(&name)).is_ok()
                || global_catalog_has_project_on_drive(
                    &paths::system_global_catalog(&cfg.system_root),
                    &name,
                    &drive.id,
                )?
            {
                let stamp = Local::now().format("%Y%m%d%H%M%S");
                let base = format!("{}_{}", name, stamp);
                let mut candidate = base.clone();
                let mut suffix = 2usize;
                while fs::symlink_metadata(projects.join(&candidate)).is_ok()
                    || manifest_evidence_exists(drive, &candidate)
                    || catalog_has_project(&catalog, &candidate)?
                    || global_catalog_has_project_on_drive(
                        &paths::system_global_catalog(&cfg.system_root),
                        &candidate,
                        &drive.id,
                    )?
                {
                    candidate = format!("{base}_{suffix}");
                    suffix += 1;
                }
                reporter.action(&format!(
                    "目标已占用；源将完整重新复制到新目标『{}』。",
                    candidate
                ));
                candidate
            } else {
                name.clone()
            }
        }
    };
    let dest = paths::drive_projects_dir(&drive.root).join(&dest_name);
    let dest_have = if dest.is_dir() {
        folder_stats(&dest).bytes
    } else {
        0
    };
    let need = size.saturating_sub(dest_have);

    // 容量
    let reserve = cfg.reserve_gb.saturating_mul(1024 * 1024 * 1024);
    if size > drive.total_bytes.saturating_sub(reserve) {
        reporter.error(&format!(
            "项目 {:.2}GB 超过单盘容量，空盘也放不下 → 需人工拆分",
            size_gb
        ));
        note_manual(
            cfg,
            reporter,
            &name,
            &format!("{:.2}GB 超过单盘容量，需拆分或显式跨盘", size_gb),
        );
        return Ok(HandleOutcome::Skipped);
    }
    if need.saturating_add(reserve) > drive.free_bytes {
        let need_gb = need as f64 / 1024.0 / 1024.0 / 1024.0;
        let free_gb = drive.free_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        reporter.warn(&format!(
            "盘 {} 余量不足以放下 {}（还需约 {:.2}GB + 余量；当前剩余 {:.2}GB）。封盘并请换下一块盘后重跑。",
            drive.id, name, need_gb, free_gb
        ));
        // R6-2:封盘标记写失败(盘只读/被占用/掉线)不应退化成逐项目 Err 重试(余量仍不足,
        // 每个后续项目都会再来一次)。无论标记是否写成功,余量不足都按 DriveSealed 终止本轮。
        if let Err(e) = drive::seal(drive) {
            reporter.error(&format!(
                "封盘标记写入失败({}):盘 {} 余量已不足、本轮就此停止。请取下本盘换下一块目标盘;\
                 如该盘还要继续用,请手工在盘根 本盘信息\\ 下创建 已封盘.txt 或排查占用后重跑。",
                e, drive.id
            ));
        } else {
            reporter.action(&format!(
                "已封盘 {}。取下本盘、插上下一块目标盘(NTFS)后，运行 `bftool init <盘符>` 初始化再继续。",
                drive.id
            ));
        }
        return Ok(HandleOutcome::DriveSealed);
    }

    // dry_run 路径由 run() 在 plan() 后直接 render_dry_run(),不会进入 run_plan/handle_one。
    // 此处 dry_run 分支仅供直接调用 handle_one(如旧测试)触发时兜底,正常流程不会到达。
    if opts.dry_run {
        reporter.info(&format!(
            "[演练] 将归档 {} ({} 个文件, {:.2}GB) → {}",
            name, stats.files, size_gb, drive.id
        ));
        return Ok(HandleOutcome::Done(0)); // 演练不写盘 → 0 字节
    }

    // Spec B 源压缩包测试 —— 在 dry_run check 之后,在生成 manifest 之前
    if let Some((kind, path)) = tester_opt {
        let r =
            archive_test::test_folder(proj_path, (*kind, path.as_path()), opts.no_hash, reporter);
        if !r.ok {
            for detail in r.details() {
                reporter.error(&detail);
            }
            note_manual(
                cfg,
                reporter,
                &name,
                &format!("源压缩包测试失败：{}", r.summary()),
            );
            if !r.tester_errors.is_empty() {
                return Ok(HandleOutcome::TesterFatallyDisabled);
            }
            return Ok(HandleOutcome::Failed);
        }

        if opts.no_hash && r.uncovered_files > 0 {
            reporter.error(&format!(
                "本项目有 {} 个未被 archive test 覆盖的文件,开启了 --unsafe-no-hash —— \
                 这些文件没有 SHA256,只剩 size+count+mtime,违反完整性校验底线。",
                r.uncovered_files
            ));
            note_manual(
                cfg,
                reporter,
                &name,
                &format!(
                    "unsafe_no_hash 模式下项目有 {} 个未覆盖文件 → 这些文件无完整性校验。\
                     请去掉 --unsafe-no-hash 让 SHA256 兜底,或把非压缩包内容打包成一个\
                     压缩包后再归档。",
                    r.uncovered_files
                ),
            );
            return Ok(HandleOutcome::Failed);
        }
    }

    // 复制(强优化:复制时边读边算源哈希,把『复制 + 生成源清单』折叠为一遍读源,省掉独立的源清单读盘)。
    // Reserve a genuinely new destination atomically. Existing directories do not grant ownership.
    let destination_handle = crate::engine::destination::SafeDir::open(&drive.root, false)?
        .create_new_dir(Path::new("项目").join(&dest_name).as_path())?;
    reporter.info(&format!(
        "复制并生成源校验和（{:.2}GB，可能较慢；新目标完整复制；中断后的旧副本保留）…",
        size_gb
    ));
    destination_handle.require_current_binding()?;
    let src_hashes = copy_folder_into(proj_path, &destination_handle, opts.no_hash, reporter)?;
    reporter.info("复制完成，开始校验…");
    destination_handle.require_current_binding()?;
    let anchored_dest = destination_handle.anchored_path();

    // 源清单:复用复制时算出的哈希(build_with_hashes),免再读一遍源内容;rel/size/mtime/去重等结构仍由
    // manifest 权威产出。size-skip 跳过复制的文件其哈希已在 copy_folder 内单独补算注入。
    let src = manifest::build_with_hashes(
        proj_path,
        ManifestOpts {
            no_hash: opts.no_hash,
        },
        reporter,
        &src_hashes,
    )?;

    // 0 真实文件:不归档、不移源 —— 否则"什么都没备份"会被记成 SHA256-OK 成功并把源移走。(ledger L-003)
    if src.count() == 0 {
        reporter.warn(&format!("跳过(无可备份的真实文件):{}", name));
        note_manual(
            cfg,
            reporter,
            &name,
            "项目内没有可备份的真实文件(空目录或全是 cruft)——不归档、不移源,请人工确认。",
        );
        return Ok(HandleOutcome::Skipped);
    }

    // 目标清单 + 比对
    let dst = manifest::build_guarded(
        &destination_handle,
        ManifestOpts {
            no_hash: opts.no_hash,
        },
        reporter,
    )?;
    destination_handle.require_current_binding()?;
    let d = manifest::diff(&src, &dst, !opts.no_hash);
    if !d.ok() {
        reporter.error(&format!(
            "校验失败：{} → 不写索引、不移动源。",
            d.reasons.join("; ")
        ));
        // A diff does not prove ownership of extras or replaced files. Keep every destination
        // entry in place for review; a future attempt uses a fresh unique destination.
        reporter.warn("目标内容校验失败；原样保留全部目标文件及源，请核对后重新规划新目标。");
        note_manual(
            cfg,
            reporter,
            &name,
            &format!("校验失败：{}", d.reasons.join("; ")),
        );
        return Ok(HandleOutcome::Failed);
    }

    // Spec B 目标压缩包测试 —— 在 SHA256 校验通过后、源复核之前
    if let Some((kind, path)) = tester_opt {
        let r = archive_test::test_folder(
            &anchored_dest,
            (*kind, path.as_path()),
            opts.no_hash,
            reporter,
        );
        if !r.ok {
            for detail in r.details() {
                reporter.error(&detail);
            }
            reporter.warn("目标压缩包测试失败；原样保留目标及源，不移动未知文件。");
            note_manual(
                cfg,
                reporter,
                &name,
                &format!("目标压缩包测试失败：{}", r.summary()),
            );
            if !r.tester_errors.is_empty() {
                return Ok(HandleOutcome::TesterFatallyDisabled);
            }
            return Ok(HandleOutcome::Failed);
        }
    }

    // 复核源在复制期间未变化
    reporter.info("复核源文件在复制期间未变化…");
    let src2 = manifest::build(
        proj_path,
        ManifestOpts {
            no_hash: opts.no_hash,
        },
        reporter,
    )?;
    let (changed, why) = manifest::source_changed(&src, &src2, opts.no_hash);
    if changed {
        reporter.error(&format!(
            "源在复制期间发生变化（{}）→ 不移动源、不写索引；该项目保留在 待备份，下次重做。",
            why.join("; ")
        ));
        note_manual(
            cfg,
            reporter,
            &name,
            &format!("源在复制期间变化：{}", why.join("; ")),
        );
        return Ok(HandleOutcome::Skipped);
    }

    destination_handle.require_current_binding()?;
    // 事务式提交(抽出为 commit_archive,使最安全攸关的提交段成为命名函数)。(强优化 review)
    commit_archive(
        cfg,
        reporter,
        drive,
        proj_path,
        &name,
        &proj_no,
        &dest_name,
        dup_in_drive,
        &catalog,
        &src,
        opts,
        &destination_handle,
    )
}

/// 事务式提交单个项目:写标记 → 写清单/本盘索引/全局索引 → rename 移源 → 清标记。AR-01 提交顺序保证
/// 任一步失败时源仍在『待备份』、可由 check_pending_txn 恢复。强优化:从 handle_one 543 行长流的尾部
/// 抽出,行为与抽取前逐字一致(仅把原局部变量改为参数/借用)。
#[allow(clippy::too_many_arguments)]
pub(super) fn commit_archive(
    cfg: &Config,
    reporter: &dyn Reporter,
    drive: &DriveInfo,
    proj_path: &Path,
    name: &str,
    proj_no: &str,
    dest_name: &str,
    dup_in_drive: bool,
    catalog: &Path,
    src: &manifest::Manifest,
    opts: &Options,
    destination_handle: &crate::engine::destination::SafeDir,
) -> Result<HandleOutcome> {
    destination_handle.require_current_binding()?;
    let local_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let utc = Utc::now().to_rfc3339();
    let src_bytes = src.total_bytes();
    let verify_status = VerifyStatus::from_opts(opts.no_hash);
    let source_root = opts
        .source_override
        .as_ref()
        .unwrap_or(&cfg.ready_root)
        .canonicalize()?;
    let source_path = proj_path.canonicalize()?;
    anyhow::ensure!(
        source_path == proj_path,
        "源路径在执行期间改变或含未授权链接；不提交归档"
    );
    let source_relative = source_relative_key(
        source_path
            .strip_prefix(&source_root)
            .context("归档源不在冻结发现根内")?,
    )?;
    // Hold the no-link parent while deriving and using the canonical journal target.
    // Windows canonical paths may use a verbatim prefix absent from the public config.
    let archive_parent = if opts.retain_source {
        None
    } else {
        Some(crate::engine::destination::SafeDir::open(
            &cfg.archived_root,
            true,
        )?)
    };
    let arch_dest = if opts.retain_source {
        // Retention is explicit. An existing source is never evidence that a move completed.
        PathBuf::new()
    } else {
        let parent = archive_parent.as_ref().context("已备份源根缺少安全绑定")?;
        parent.require_current_binding()?;
        let root = cfg.archived_root.canonicalize()?;
        parent.require_current_binding()?;
        let mut candidate = root.join(name);
        if fs::symlink_metadata(&candidate).is_ok() {
            let base = format!("{}_{}", name, Local::now().format("%Y%m%d%H%M%S"));
            candidate = root.join(&base);
            let mut suffix = 2usize;
            while fs::symlink_metadata(&candidate).is_ok() {
                candidate = root.join(format!("{base}_{suffix}"));
                suffix += 1;
            }
        }
        anyhow::ensure!(
            source_path.file_name().and_then(|leaf| leaf.to_str()) == Some(name)
                && candidate
                    .file_name()
                    .and_then(|leaf| leaf.to_str())
                    .is_some_and(|leaf| archive_leaf_matches_source(name, leaf)),
            "已备份源名不安全或与源身份不一致；不提交归档"
        );
        candidate
    };
    let txn_path = paths::system_pending_txn(&cfg.system_root);
    anyhow::ensure!(!txn_path.exists(), "尚有未完成事务；不能覆盖恢复证据");
    let snapshot = if opts.no_hash {
        None
    } else {
        Some(fingerprint_from_manifest(src)?)
    };
    let mut journal = CommitJournal {
        version: 1,
        pending: txn::PendingTxn {
            project_dest_name: dest_name.to_string(),
            project_src_name: name.to_string(),
            drive_id: drive.id.clone(),
            drive_letter: drive.letter.clone(),
            in_drive_path: format!("项目\\{}", dest_name),
            src_path: source_path.display().to_string(),
            move_to: arch_dest.display().to_string(),
            started_at: local_time.clone(),
        },
        stage: CommitStage::Verified,
        retain_source: opts.retain_source,
        incremental: opts.incremental,
        drive_root: drive.root.canonicalize()?.display().to_string(),
        source_root: source_root.display().to_string(),
        source_id: source_id_for(&source_root),
        source_relative,
        snapshot,
        entries: src.entries.clone(),
        global_row: GlobalCatalogRow {
            folder_name: dest_name.to_string(),
            drive_name: drive.id.clone(),
            archived_time: local_time,
            project_no: leading_digits(name),
            in_drive_path: format!("项目\\{}", dest_name),
            file_count: src.count() as u64,
            size_gb: src_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
            verify: verify_status.token().to_string(),
            manifest_path: format!("本盘信息\\校验清单\\{}.sha256.csv", dest_name),
        },
        drive_row: DriveCatalogRow {
            project_no: proj_no.to_string(),
            project_name: dest_name.to_string(),
            file_count: src.count() as u64,
            total_bytes: src_bytes,
            archived_utc: utc,
            verify_status: verify_status.token().to_string(),
            status: "Complete".to_string(),
            notes: if dup_in_drive {
                format!("原名 {}", name)
            } else {
                String::new()
            },
        },
    };
    let _ = catalog; // journal derives the exact catalog path from its verified drive root.
    journal.write(&txn_path)?;
    if let Err(e) = journal.commit_metadata(cfg, &txn_path) {
        reporter.error(&format!(
            "提交中断，清单/索引阶段尚未完成：{e:#}；事务证据保留。"
        ));
        return Ok(HandleOutcome::CommitInterrupted);
    }
    if let Err(e) = destination_handle.require_current_binding() {
        reporter.error(&format!(
            "目标目录对象在提交期间被替换：{e:#}；事务证据保留。"
        ));
        return Ok(HandleOutcome::CommitInterrupted);
    }
    if !opts.retain_source {
        // A concurrent source writer must not turn a verified old snapshot into a claim about new data.
        let current = match manifest::build(
            proj_path,
            ManifestOpts {
                no_hash: opts.no_hash,
            },
            reporter,
        ) {
            Ok(snapshot) => snapshot,
            Err(e) => {
                reporter.error(&format!("提交前源复验失败：{e:#}；事务证据保留。"));
                return Ok(HandleOutcome::CommitInterrupted);
            }
        };
        if manifest::source_changed(src, &current, opts.no_hash).0 {
            reporter.error("源在提交期间变更；不会移动新内容，事务证据保留。");
            return Ok(HandleOutcome::CommitInterrupted);
        }
        if let Err(e) = destination_handle
            .require_current_binding()
            .and_then(|_| {
                archive_parent
                    .as_ref()
                    .context("已备份源根缺少安全绑定")?
                    .require_current_binding()
            })
            .and_then(|_| {
                move_source_no_replace(
                    proj_path,
                    &arch_dest,
                    archive_parent.as_ref().context("已备份源根缺少安全绑定")?,
                )
            })
        {
            reporter.error(&format!("移动源失败：{e:#}；事务证据保留，源未删除。"));
            return Ok(HandleOutcome::CommitInterrupted);
        }
    }
    if let Err(e) = journal
        .advance(CommitStage::SourceCompleted, &txn_path)
        .and_then(|_| txn::PendingTxn::clear(&txn_path))
    {
        reporter.error(&format!("提交收尾中断：{e:#}；事务证据保留。"));
        return Ok(HandleOutcome::CommitInterrupted);
    }
    reporter.ok(&format!(
        "✓ {} 归档+校验成功 → {}\\项目\\{}；{}。",
        name,
        drive.id,
        dest_name,
        if opts.retain_source {
            "源保留未移动"
        } else {
            "源已移到已备份，未删除"
        }
    ));
    Ok(HandleOutcome::Done(src_bytes))
}

#[cfg(test)]
#[path = "incremental_safety_regression_tests.rs"]
mod safety_regression_tests;

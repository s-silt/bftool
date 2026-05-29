//! 主归档流程：扫描 → 稳定性 → 容量 → 复制 → 校验 → 复核源 → 事务式提交。
//!
//! 顺序与不变量（与 PowerShell 旧版一致，且经实测）：
//! 1. 多块未封盘备份盘同时在线 → 立即停止（防写错盘）
//! 2. 单项目 try/catch 隔离，意外只跳过该项目不中断整轮
//! 3. 校验失败时把目标侧坏文件移到 异常文件/，下次自动重传
//! 4. 移动源前再次比对源（防"备份的是旧版本"）
//! 5. 写事务标记 → 移动源 → 写清单/索引 → 删标记（宁可漏写索引也不要"假装归档好了"）

use anyhow::{Context, Result};
use chrono::{Local, Utc};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::engine::archive_test::{self, Tester, TesterPaths};
use crate::engine::drive::{self, DriveInfo};
use crate::engine::manifest::{self, ManifestOpts};
use crate::engine::{cruft, durable, paths, safety, txn};
use crate::reporter::Reporter;

#[derive(Debug, Default)]
pub struct Options {
    pub dry_run: bool,
    pub no_hash: bool,
    pub limit: usize,
    pub drive_letter_override: Option<String>,
    pub no_test_archives: bool,
}

pub fn run(cfg: &Config, reporter: &dyn Reporter, opts: Options) -> Result<()> {
    // Spec B D14/D15 core-level fail-closed guard
    if opts.no_hash && (!cfg.test_archives || opts.no_test_archives) {
        anyhow::bail!(
            "拒绝运行：no_hash=true 与 archive test 关闭(配置 test_archives=false 或 \
             opts.no_test_archives=true)不能同时存在 —— 等价于「没在做完整性校验」。\n\
             这是 core 级 fail-closed 拦截,调用方应该在传 Options 之前就做合并检查\
             (CLI dispatch 已经做了一次更友好的)。\n\
             如何修：\n\
               - 去掉 no_hash 让 SHA256 兜底；或\n\
               - 不要把 test_archives 设成 false 也不要把 no_test_archives 设成 true"
        );
    }

    // 准备系统目录
    fs::create_dir_all(&cfg.system_root).context("创建 备份系统 目录失败")?;
    fs::create_dir_all(paths::system_logs_dir(&cfg.system_root)).ok();
    fs::create_dir_all(&cfg.archived_root).context("创建 已备份 目录失败")?;

    // 启动自检：上次的事务标记是不是残留？
    check_pending_txn(cfg, reporter)?;

    // 找当前可用备份盘
    let drive = match opts.drive_letter_override.as_deref() {
        Some(letter) => drive::info_by_letter(letter)?,
        None => match drive::pick_active(cfg.min_drive_gb, reporter)? {
            Some(d) => d,
            None => {
                reporter.error("未发现已初始化且未封盘的备份盘。");
                reporter.info("插入空盘后运行：bftool init <盘符>（例：bftool init E）");
                return Ok(());
            }
        },
    };
    if drive.sealed {
        reporter.error(&format!(
            "盘 {} ({}:) 已封盘，禁止写入。请换上一块未封盘的备份盘或初始化新盘。",
            drive.id, drive.letter
        ));
        return Ok(());
    }

    reporter.ok(&format!("当前备份盘: {} ({}:)", drive.id, drive.letter));

    // 路径安全检查
    let warnings = safety::check_paths(
        &cfg.ready_root,
        &cfg.archived_root,
        &cfg.system_root,
        Some(&drive.root),
    )?;
    for w in warnings {
        reporter.warn(&w);
    }

    // Spec B 顶层 tester detection:本轮只 detect 一次、只 warn 一次
    let mut tester_opt: Option<(Tester, PathBuf)> = if cfg.test_archives && !opts.no_test_archives {
        let p = TesterPaths {
            winrar: cfg.winrar_path.clone(),
            bandizip: cfg.bandizip_path.clone(),
            seven_zip: cfg.seven_zip_path.clone(),
        };
        match archive_test::detect(&p) {
            Some(t) => Some(t),
            None => {
                if opts.no_hash {
                    anyhow::bail!(
                        "拒绝运行：开启了 --unsafe-no-hash 但**机器上一个压缩包测试器都没装**。\n\
                         此时 SHA256 没在跑,archive test 也跑不起来 —— 只剩\n\
                         文件数+大小+修改时间,等价于「没在做完整性校验」。\n\
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
                None
            }
        }
    } else {
        None
    };

    // 列出待归档项目
    if !cfg.ready_root.is_dir() {
        reporter.error(&format!("待备份 不存在：{}", cfg.ready_root.display()));
        return Ok(());
    }
    let mut projects: Vec<_> = fs::read_dir(&cfg.ready_root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    // 按文件夹名前导数字升序，无数字前缀的排最后
    projects.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        leading_number(name).unwrap_or(u64::MAX)
    });
    if projects.is_empty() {
        reporter.info("待备份 中没有待归档项目，结束。");
        return Ok(());
    }
    reporter.info(&format!(
        "发现 {} 个待归档项目（按编号升序处理）。",
        projects.len()
    ));

    let mut handled = 0usize;
    for proj in &projects {
        if opts.limit > 0 && handled >= opts.limit {
            reporter.action(&format!(
                "已达本次处理上限（{} 个项目），停止；剩余项目下次运行继续。",
                opts.limit
            ));
            break;
        }
        match handle_one(cfg, reporter, &drive, proj, &opts, tester_opt.as_ref()) {
            Ok(HandleOutcome::Done) => handled += 1,
            Ok(HandleOutcome::Skipped) => {}
            Ok(HandleOutcome::DriveSealed) => {
                reporter.action(
                    "本盘已封盘，本轮结束。换上下一块空盘后用 `bftool init <盘符>` 初始化再继续。",
                );
                break;
            }
            Ok(HandleOutcome::TesterFatallyDisabled) => {
                if opts.no_hash {
                    reporter.error(
                        "测试器运行时失效(spawn/被信号杀/exe 被 AV 拦),同时 --unsafe-no-hash 已开 \
                         —— 后续项目将落到「无 SHA256 + 无 archive test」无校验状态。本轮停止。\n\
                         如何修：去掉 --unsafe-no-hash 让 SHA256 兜底,或修复测试器环境后重跑。",
                    );
                    break;
                }
                reporter.warn(
                    "测试器自身异常 —— 本轮剩下的项目跳过压缩包测试；SHA256 整文件校验仍在。\
                     请检查配置的测试器路径是否正确、exe 是否被 AV 拦截。",
                );
                tester_opt = None;
            }
            Err(e) => {
                let name = proj
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                reporter.error(&format!(
                    "项目 {} 处理时发生意外错误：{} → 跳过该项目，继续下一个。",
                    name, e
                ));
                append_manual(cfg, &name, &format!("未捕获异常：{}", e))?;
            }
        }
    }
    reporter.ok("本轮结束。");
    Ok(())
}

enum HandleOutcome {
    Done,
    Skipped,
    DriveSealed,
    TesterFatallyDisabled,
}

fn handle_one(
    cfg: &Config,
    reporter: &dyn Reporter,
    drive: &DriveInfo,
    proj_path: &Path,
    opts: &Options,
    tester_opt: Option<&(Tester, PathBuf)>,
) -> Result<HandleOutcome> {
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

    // 体积 + 目标已存在量（断点续传时仅按"还需写入"判断）
    let stats = folder_stats(proj_path);
    let size = stats.bytes;
    let size_gb = size as f64 / 1024.0 / 1024.0 / 1024.0;

    // 重名保护
    let mut dest_name = name.clone();
    let catalog = paths::drive_catalog_path(&drive.root);
    let dup_in_drive = catalog_has_project(&catalog, &name).unwrap_or(false);
    if dup_in_drive {
        let stamp = Local::now().format("%Y%m%d%H%M%S");
        dest_name = format!("{}_{}", name, stamp);
        reporter.action(&format!(
            "本盘已存在同名历史备份『{}』→ 为避免污染旧备份，本次改用唯一名『{}』。",
            name, dest_name
        ));
    }
    let dest = paths::drive_projects_dir(&drive.root).join(&dest_name);
    let dest_have = if dest.is_dir() {
        folder_stats(&dest).bytes
    } else {
        0
    };
    let need = size.saturating_sub(dest_have);

    // 容量
    let reserve = cfg.reserve_gb * 1024 * 1024 * 1024;
    if size > drive.total_bytes.saturating_sub(reserve) {
        reporter.error(&format!(
            "项目 {:.2}GB 超过单盘容量，空盘也放不下 → 需人工拆分",
            size_gb
        ));
        append_manual(
            cfg,
            &name,
            &format!("{:.2}GB 超过单盘容量，需拆分或显式跨盘", size_gb),
        )?;
        return Ok(HandleOutcome::Skipped);
    }
    if need.saturating_add(reserve) > drive.free_bytes {
        let need_gb = need as f64 / 1024.0 / 1024.0 / 1024.0;
        let free_gb = drive.free_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
        reporter.warn(&format!(
            "盘 {} 余量不足以放下 {}（还需约 {:.2}GB + 余量；当前剩余 {:.2}GB）。封盘并请换下一块盘后重跑。",
            drive.id, name, need_gb, free_gb
        ));
        drive::seal(drive).context("封盘失败")?;
        reporter.action(&format!(
            "已封盘 {}。取下本盘、插上下一块空盘(NTFS)后，运行 `bftool init <盘符>` 初始化再继续。",
            drive.id
        ));
        return Ok(HandleOutcome::DriveSealed);
    }

    if opts.dry_run {
        reporter.info(&format!(
            "[演练] 将归档 {} ({} 个文件, {:.2}GB) → {}",
            name, stats.files, size_gb, drive.id
        ));
        if !stats.enum_errors.is_empty() || !stats.metadata_errors.is_empty() {
            for e in &stats.enum_errors {
                reporter.warn(&format!("[演练] 枚举失败：{}", e));
            }
            for e in &stats.metadata_errors {
                reporter.warn(&format!("[演练] 读元数据失败：{}", e));
            }
            reporter.warn(&format!(
                "[演练] {} 项目的统计可能不完整 ({} 个枚举错误 + {} 个 metadata 错误)。\
                 正式归档会因为同样的错误失败 —— 请先解决环境问题(权限拒绝？路径过长？\
                 被 AV 锁住？)再去掉 --dry-run 跑。",
                name,
                stats.enum_errors.len(),
                stats.metadata_errors.len()
            ));
        }
        return Ok(HandleOutcome::Done);
    }

    // Spec B 源压缩包测试 —— 在 dry_run check 之后,在生成 manifest 之前
    if let Some((kind, path)) = tester_opt {
        let r = archive_test::test_folder(proj_path, (*kind, path.as_path()), reporter);
        if !r.ok {
            for detail in r.details() {
                reporter.error(&detail);
            }
            append_manual(cfg, &name, &format!("源压缩包测试失败：{}", r.summary()))?;
            if !r.tester_errors.is_empty() {
                return Ok(HandleOutcome::TesterFatallyDisabled);
            }
            return Ok(HandleOutcome::Skipped);
        }

        if opts.no_hash && r.uncovered_files > 0 {
            reporter.error(&format!(
                "本项目有 {} 个未被 archive test 覆盖的文件,开启了 --unsafe-no-hash —— \
                 这些文件没有 SHA256,只剩 size+count+mtime,违反完整性校验底线。",
                r.uncovered_files
            ));
            append_manual(
                cfg,
                &name,
                &format!(
                    "unsafe_no_hash 模式下项目有 {} 个未覆盖文件 → 这些文件无完整性校验。\
                     请去掉 --unsafe-no-hash 让 SHA256 兜底,或把非压缩包内容打包成一个\
                     压缩包后再归档。",
                    r.uncovered_files
                ),
            )?;
            return Ok(HandleOutcome::Skipped);
        }
    }

    // 生成源清单
    reporter.info(&format!("生成源清单/校验和（{:.2}GB，可能较慢）…", size_gb));
    let src = manifest::build(
        proj_path,
        ManifestOpts {
            no_hash: opts.no_hash,
        },
        reporter,
    )?;

    // 复制
    fs::create_dir_all(&dest).context("创建目标目录失败")?;
    reporter.info("开始复制（断点续传：已存在且大小一致的文件会被跳过）…");
    copy_folder(proj_path, &dest, reporter)?;
    reporter.info("复制完成，开始校验…");

    // 目标清单 + 比对
    let dst = manifest::build(
        &dest,
        ManifestOpts {
            no_hash: opts.no_hash,
        },
        reporter,
    )?;
    let d = manifest::diff(&src, &dst, !opts.no_hash);
    if !d.ok {
        reporter.error(&format!(
            "校验失败：{} → 不写索引、不移动源。",
            d.reasons.join("; ")
        ));
        // 隔离坏文件，让下次 robocopy 风格的"补传"再来一遍
        if !d.bad_dst_rels.is_empty() {
            let quar = paths::drive_quarantine_dir(&drive.root).join(&name);
            let mut moved = 0usize;
            for rel in &d.bad_dst_rels {
                let src_p = dest.join(rel);
                if src_p.exists() {
                    let dst_p = quar.join(rel);
                    if let Some(p) = dst_p.parent() {
                        fs::create_dir_all(p).ok();
                    }
                    if let Err(e) = fs::rename(&src_p, &dst_p) {
                        reporter.warn(&format!(
                            "隔离失败 {}：{}（请手动检查 {}）",
                            rel,
                            e,
                            src_p.display()
                        ));
                    } else {
                        moved += 1;
                    }
                }
            }
            reporter.action(&format!(
                "已将 {} 个损坏/多余的目标文件移到：{} → 下次运行会自动补传并重新校验。",
                moved,
                quar.display()
            ));
        }
        append_manual(cfg, &name, &format!("校验失败：{}", d.reasons.join("; ")))?;
        return Ok(HandleOutcome::Skipped);
    }

    // Spec B 目标压缩包测试 —— 在 SHA256 校验通过后、源复核之前
    if let Some((kind, path)) = tester_opt {
        let r = archive_test::test_folder(&dest, (*kind, path.as_path()), reporter);
        if !r.ok {
            for detail in r.details() {
                reporter.error(&detail);
            }
            if !r.archive_failed.is_empty() {
                let quar = paths::drive_quarantine_dir(&drive.root).join(&name);
                let mut moved = 0usize;
                for (bad, _reason) in &r.archive_failed {
                    if let Ok(rel) = bad.strip_prefix(&dest) {
                        let to = quar.join(rel);
                        if let Some(p) = to.parent() {
                            fs::create_dir_all(p).ok();
                        }
                        if let Err(e) = fs::rename(bad, &to) {
                            reporter.warn(&format!("隔离失败 {}：{}", bad.display(), e));
                        } else {
                            moved += 1;
                        }
                    }
                }
                reporter.action(&format!(
                    "已隔离 {} 个损坏压缩包到 {}；下次运行会自动补传并重测。",
                    moved,
                    quar.display()
                ));
            }
            append_manual(cfg, &name, &format!("目标压缩包测试失败：{}", r.summary()))?;
            if !r.tester_errors.is_empty() {
                return Ok(HandleOutcome::TesterFatallyDisabled);
            }
            return Ok(HandleOutcome::Skipped);
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
        append_manual(cfg, &name, &format!("源在复制期间变化：{}", why.join("; ")))?;
        return Ok(HandleOutcome::Skipped);
    }

    // 事务式提交
    let local_time = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let utc = Utc::now().to_rfc3339();
    let src_bytes = src.total_bytes();
    let size_gbval = src_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    let verify_status = if opts.no_hash {
        "SIZE+COUNT"
    } else {
        "SHA256-OK"
    };
    let manifest_path =
        paths::drive_manifest_dir(&drive.root).join(format!("{}.sha256.csv", dest_name));
    let rel_manifest = format!("本盘信息\\校验清单\\{}.sha256.csv", dest_name);
    let mut arch_dest = cfg.archived_root.join(&name);
    if arch_dest.exists() {
        let stamp = Local::now().format("%Y%m%d%H%M%S");
        arch_dest = cfg.archived_root.join(format!("{}_{}", name, stamp));
    }

    let txn_path = paths::system_pending_txn(&cfg.system_root);
    let pending = txn::PendingTxn {
        project_dest_name: dest_name.clone(),
        project_src_name: name.clone(),
        drive_id: drive.id.clone(),
        drive_letter: drive.letter.clone(),
        in_drive_path: format!("项目\\{}", dest_name),
        src_path: proj_path.display().to_string(),
        move_to: arch_dest.display().to_string(),
        started_at: local_time.clone(),
    };
    pending.write(&txn_path)?;

    // 移动源（先动它；失败抛错 → 不写索引；源留在 待备份 下次重做）
    fs::rename(proj_path, &arch_dest).with_context(|| {
        format!(
            "移动源失败：{} → {}",
            proj_path.display(),
            arch_dest.display()
        )
    })?;

    // 写清单 + 本盘索引 + 全局索引
    src.write_csv(&manifest_path)?;
    append_drive_catalog(
        &catalog,
        &DriveCatalogRow {
            project_no: proj_no,
            project_name: dest_name.clone(),
            file_count: src.count() as u64,
            total_bytes: src_bytes,
            archived_utc: utc.clone(),
            verify_status: verify_status.to_string(),
            status: "Complete".to_string(),
            notes: if dup_in_drive {
                format!("原名 {}", name)
            } else {
                String::new()
            },
        },
    )?;
    append_global_catalog(
        &paths::system_global_catalog(&cfg.system_root),
        &GlobalCatalogRow {
            folder_name: dest_name.clone(),
            drive_name: drive.id.clone(),
            archived_time: local_time.clone(),
            project_no: leading_digits(&name),
            in_drive_path: format!("项目\\{}", dest_name),
            file_count: src.count() as u64,
            size_gb: size_gbval,
            verify: verify_status.to_string(),
            manifest_path: rel_manifest,
        },
    )?;

    txn::PendingTxn::clear(&txn_path)?;
    reporter.ok(&format!(
        "✓ {} 归档+校验成功 → {}\\项目\\{}；SSD 源已移到 已备份（未删除）。",
        name, drive.id, dest_name
    ));
    Ok(HandleOutcome::Done)
}

#[derive(Debug, Serialize)]
struct DriveCatalogRow {
    #[serde(rename = "ProjectNo")]
    project_no: String,
    #[serde(rename = "ProjectName")]
    project_name: String,
    #[serde(rename = "FileCount")]
    file_count: u64,
    #[serde(rename = "TotalBytes")]
    total_bytes: u64,
    #[serde(rename = "ArchivedUTC")]
    archived_utc: String,
    #[serde(rename = "VerifyStatus")]
    verify_status: String,
    #[serde(rename = "Status")]
    status: String,
    #[serde(rename = "Notes")]
    notes: String,
}

#[derive(Debug, Serialize)]
struct GlobalCatalogRow {
    #[serde(rename = "文件夹名")]
    folder_name: String,
    #[serde(rename = "备份盘名")]
    drive_name: String,
    #[serde(rename = "备份时间")]
    archived_time: String,
    #[serde(rename = "编号")]
    project_no: String,
    #[serde(rename = "盘内路径")]
    in_drive_path: String,
    #[serde(rename = "文件数")]
    file_count: u64,
    #[serde(rename = "大小GB")]
    size_gb: f64,
    #[serde(rename = "校验方式")]
    verify: String,
    #[serde(rename = "校验清单")]
    manifest_path: String,
}

fn append_drive_catalog(path: &Path, row: &DriveCatalogRow) -> Result<()> {
    let file_existed = path.is_file();
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(!file_existed)
        .from_writer(file);
    wtr.serialize(row)?;
    wtr.flush()?;
    // fsync:索引必须先于"删事务标记"真正落盘,否则断电后会出现"标记已删、索引未落"。(ledger L-001)
    let f = wtr
        .into_inner()
        .map_err(|e| anyhow::anyhow!("刷新索引缓冲失败：{}", e))?;
    durable::sync_file(&f).with_context(|| format!("索引刷盘失败：{}", path.display()))?;
    Ok(())
}

fn append_global_catalog(path: &Path, row: &GlobalCatalogRow) -> Result<()> {
    let file_existed = path.is_file();
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut wtr = csv::WriterBuilder::new()
        .has_headers(!file_existed)
        .from_writer(file);
    wtr.serialize(row)?;
    wtr.flush()?;
    // fsync:索引必须先于"删事务标记"真正落盘,否则断电后会出现"标记已删、索引未落"。(ledger L-001)
    let f = wtr
        .into_inner()
        .map_err(|e| anyhow::anyhow!("刷新索引缓冲失败：{}", e))?;
    durable::sync_file(&f).with_context(|| format!("索引刷盘失败：{}", path.display()))?;
    Ok(())
}

fn catalog_has_project(catalog: &Path, project_name: &str) -> Result<bool> {
    if !catalog.is_file() {
        return Ok(false);
    }
    let mut rdr = csv::Reader::from_path(catalog)?;
    let headers = rdr.headers()?.clone();
    let Some(col) = headers.iter().position(|h| h == "ProjectName") else {
        return Ok(false);
    };
    for rec in rdr.records().flatten() {
        if rec.get(col).map(|v| v == project_name).unwrap_or(false) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn append_manual(cfg: &Config, name: &str, why: &str) -> Result<()> {
    let path = paths::system_need_manual(&cfg.system_root);
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).ok();
    }
    let line = format!(
        "{}\t{}\t{}\n",
        Local::now().format("%Y-%m-%d %H:%M"),
        name,
        why
    );
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

fn check_pending_txn(cfg: &Config, reporter: &dyn Reporter) -> Result<()> {
    let path = paths::system_pending_txn(&cfg.system_root);
    if !path.is_file() {
        return Ok(());
    }
    let text = txn::PendingTxn::read_text(&path)?;
    let pname = grab_field(&text, "项目").unwrap_or_default();
    let psrc = grab_field(&text, "源名").unwrap_or_else(|| pname.clone());
    let parch = grab_field(&text, "将移至").unwrap_or_default();
    let in_ready = !psrc.is_empty() && cfg.ready_root.join(&psrc).exists();
    let moved = !parch.is_empty() && Path::new(&parch).exists();
    let global = paths::system_global_catalog(&cfg.system_root);
    let indexed = global_has_folder(&global, &pname).unwrap_or(false);

    reporter.action(&format!("发现上次未完成的事务（项目：{}）。", pname));
    if in_ready {
        reporter
            .action("→ 源仍在『待备份』，说明移动尚未发生；本次会自动重做该项目。已清除旧标记。");
        let _ = fs::remove_file(&path);
    } else if moved && indexed {
        reporter.action("→ 已移动且索引中已有记录，判定为已完成（仅标记残留）。已自动清除标记。");
        let _ = fs::remove_file(&path);
    } else if moved && !indexed {
        reporter.error(&format!(
            "→ 源已移到『已备份』但全局索引可能漏写：数据应在备份盘 {}。请人工核对并在索引中补登；标记保留。",
            parch
        ));
    } else {
        reporter.error(&format!(
            "→ 无法自动判定（源不在待备份、目标也未确认）。请按以下信息人工核对，标记保留：\n{}",
            text
        ));
    }
    Ok(())
}

fn grab_field(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        // 兼容 "key:" 和 "key  :" 写法（PowerShell 旧版用全角/空格混排）
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(key) {
            let r = rest.trim_start();
            let r = r.trim_start_matches([':', '：']).trim();
            return Some(r.to_string());
        }
    }
    None
}

fn global_has_folder(global: &Path, folder_name: &str) -> Result<bool> {
    if !global.is_file() {
        return Ok(false);
    }
    let mut rdr = csv::Reader::from_path(global)?;
    let headers = rdr.headers()?.clone();
    let Some(col) = headers.iter().position(|h| h == "文件夹名") else {
        return Ok(false);
    };
    for rec in rdr.records().flatten() {
        if rec.get(col).map(|v| v == folder_name).unwrap_or(false) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Default)]
pub struct FolderStats {
    pub files: u64,
    pub bytes: u64,
    pub enum_errors: Vec<String>,
    pub metadata_errors: Vec<String>,
}

fn folder_stats(p: &Path) -> FolderStats {
    let mut s = FolderStats::default();
    for entry in cruft::walk(p) {
        match entry {
            Ok(e) if e.file_type().is_file() => {
                s.files += 1;
                match e.metadata() {
                    Ok(m) => s.bytes += m.len(),
                    Err(err) => s
                        .metadata_errors
                        .push(format!("{}: {}", e.path().display(), err)),
                }
            }
            Ok(_) => continue,
            Err(err) => s.enum_errors.push(format!("{}", err)),
        }
    }
    s
}

/// 简单的递归复制；与 robocopy 比缺少 /Z 断点续传中段恢复，但小文件/中等大小够用。
/// 已存在且大小相同的文件直接跳过（保留断点续传的核心语义）。
///
/// TODO(Batch 4)：改为 .bftool-part 临时文件 + 写完 sync + 重读目标算 hash + 原子 rename。
fn copy_folder(src: &Path, dst: &Path, reporter: &dyn Reporter) -> Result<()> {
    fs::create_dir_all(dst).ok();
    let mut errors: Vec<String> = Vec::new();
    for entry in cruft::walk(src) {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                errors.push(format!("{}", err));
                continue;
            }
        };
        let path = entry.path();
        let rel = path.strip_prefix(src)?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).ok();
        } else if entry.file_type().is_file() {
            // 已存在且大小一致 → 视为已传，跳过（与 robocopy 的默认行为一致）
            if let (Ok(meta_src), Ok(meta_dst)) = (path.metadata(), target.metadata()) {
                if meta_src.len() == meta_dst.len() {
                    continue;
                }
            }
            if let Some(p) = target.parent() {
                fs::create_dir_all(p).ok();
            }
            fs::copy(path, &target)
                .with_context(|| format!("复制失败：{} → {}", path.display(), target.display()))?;
        }
    }
    if !errors.is_empty() {
        for e in &errors {
            reporter.error(&format!("复制阶段枚举源失败：{}", e));
        }
        anyhow::bail!("复制阶段枚举源失败 {} 项 → 本项目跳过。", errors.len());
    }
    Ok(())
}

fn leading_number(name: &str) -> Option<u64> {
    let s = name.trim_start();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<u64>().ok()
    }
}

fn leading_digits(name: &str) -> String {
    let s = name.trim_start();
    s.chars().take_while(|c| c.is_ascii_digit()).collect()
}

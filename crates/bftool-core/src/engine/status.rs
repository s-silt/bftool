//! `bftool` 不带子命令时的状态总览。
//!
//! core 提供结构化 `gather() -> StatusReport`(GUI 仪表盘直接消费),
//! CLI `run` 负责把它渲染成一屏面板。GUI 不解析 reporter 文本。(Spec D §4.1 / L-031)

use anyhow::Result;
use std::fs;
use std::path::PathBuf;

use crate::config::Config;
use crate::engine::{drive, paths, verify_state};
use crate::reporter::Reporter;

/// 一块在线盘 + 它的最近复查记录。
#[derive(Debug, Clone)]
pub struct DriveStatus {
    pub drive: drive::BackupDrive,
    pub last_verify: Option<verify_state::LastVerify>,
}

/// 仪表盘所需的全部结构化数据。
#[derive(Debug, Clone)]
pub struct StatusReport {
    pub ready_root: PathBuf,
    pub archived_root: PathBuf,
    pub system_root: PathBuf,
    pub reserve_gb: u64,
    pub stable_minutes: u64,
    pub min_drive_gb: u64,
    pub drives: Vec<DriveStatus>,
    pub pending_count: usize,
    pub txn_pending: bool,
}

/// 收集状态(扫描在线盘 + 各盘最近复查 + 待归档数 + 事务残留)。
pub fn gather(cfg: &Config) -> Result<StatusReport> {
    let mut drives = Vec::new();
    for d in drive::scan_mounted()? {
        let last_verify = verify_state::read_last_verify(&cfg.system_root, &d.id)
            .ok()
            .flatten();
        drives.push(DriveStatus {
            drive: d,
            last_verify,
        });
    }
    Ok(StatusReport {
        ready_root: cfg.ready_root.clone(),
        archived_root: cfg.archived_root.clone(),
        system_root: cfg.system_root.clone(),
        reserve_gb: cfg.reserve_gb,
        stable_minutes: cfg.stable_minutes,
        min_drive_gb: cfg.min_drive_gb,
        drives,
        pending_count: count_subdirs(&cfg.ready_root),
        txn_pending: paths::system_pending_txn(&cfg.system_root).is_file(),
    })
}

/// `_reporter` 暂未使用:状态面板是多行整屏渲染,CLI 直接 println(GUI 走 gather)。
pub fn run(cfg: &Config, _reporter: &dyn Reporter) -> Result<()> {
    let s = gather(cfg)?;
    println!("==================== 归档备份工具 (bftool) ====================");
    println!("  待备份(源)  : {}", display_root(&s.ready_root));
    println!("  已备份      : {}", display_root(&s.archived_root));
    println!("  备份系统    : {}", display_root(&s.system_root));
    println!(
        "  预留余量    : {} GB    稳定期 : {} 分钟    认盘最小容量 : {} GB",
        s.reserve_gb, s.stable_minutes, s.min_drive_gb
    );
    println!("-----------------------------------------------------------");

    if s.drives.is_empty() {
        println!("  机械盘 · 当前在线 : 无（插入空盘后执行 `bftool init <盘符>` 初始化）");
    } else {
        for ds in &s.drives {
            let d = &ds.drive;
            let tag = if d.sealed {
                "[已封盘]"
            } else {
                "[可用]  "
            };
            let lv = match &ds.last_verify {
                Some(v) => format!(
                    "  上次复查: {} ({})",
                    short_time(&v.when),
                    outcome_label(&v.status)
                ),
                None => "  上次复查: 未知".to_string(),
            };
            println!(
                "  机械盘 · 当前在线 : {}  {} ({}:)  剩余 {:.1}/{:.0} GB{}",
                tag,
                d.id,
                d.letter,
                d.free_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
                d.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
                lv,
            );
        }
    }

    println!("  待归档项目数: {}", s.pending_count);
    if s.txn_pending {
        println!("  事务残留    : 是 —— 上次未完成的归档需要核对（详情运行任意子命令时会自检）");
    }

    println!("===========================================================");
    println!("常用命令：");
    println!(
        "  bftool init <盘符>            初始化一块空盘为下一个「备份N」（例：bftool init E）"
    );
    println!("  bftool archive --dry-run     演练（不真正复制）");
    println!("  bftool archive               正式归档");
    println!("  bftool verify [盘符]         复查：重算 SHA256 比对清单");
    println!("  bftool find <关键词>         查询某项目在哪块盘");
    println!("  bftool drives                列出所有已识别的备份盘");
    println!("  bftool config-show           显示当前生效的配置");
    println!("提示：在仓库目录或当前目录放一个 `bftool.toml` 即可固化你的三个根目录与参数。");
    Ok(())
}

fn outcome_label(o: &crate::engine::verify::VerifyOutcome) -> &'static str {
    use crate::engine::verify::VerifyOutcome;
    match o {
        VerifyOutcome::Clean => "完好",
        VerifyOutcome::IssuesFound { .. } => "发现损坏!",
        VerifyOutcome::ExtraOnly { .. } => "有多余文件",
        VerifyOutcome::Cancelled => "上次取消",
    }
}

fn short_time(rfc3339: &str) -> &str {
    // "2026-05-29T12:00:00+00:00" → "2026-05-29"(取日期部分,够仪表盘用)
    rfc3339.split('T').next().unwrap_or(rfc3339)
}

fn display_root(p: &std::path::Path) -> String {
    if p.is_dir() {
        p.display().to_string()
    } else {
        format!("{} (不存在)", p.display())
    }
}

fn count_subdirs(p: &std::path::Path) -> usize {
    fs::read_dir(p)
        .ok()
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gather_reports_pending_and_txn() {
        let d = tempfile::tempdir().unwrap();
        let ready = d.path().join("ready");
        let sys = d.path().join("sys");
        std::fs::create_dir_all(ready.join("001proj")).unwrap();
        std::fs::create_dir_all(ready.join("002proj")).unwrap();
        std::fs::create_dir_all(&sys).unwrap();
        let cfg = Config {
            ready_root: ready,
            system_root: sys.clone(),
            ..Config::default()
        };
        let s = gather(&cfg).unwrap();
        assert_eq!(s.pending_count, 2);
        assert!(!s.txn_pending);
        // 造一个事务残留
        std::fs::write(paths::system_pending_txn(&sys), "x").unwrap();
        assert!(gather(&cfg).unwrap().txn_pending);
    }
}

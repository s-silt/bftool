//! `bftool` 不带子命令时的状态总览。
//!
//! core 提供结构化 `gather() -> StatusReport`(GUI 仪表盘直接消费),
//! CLI `run` 负责把它渲染成一屏面板。GUI 不解析 reporter 文本。(Spec D §4.1 / L-031)

use anyhow::{Context, Result};
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

/// 读某盘最近复查记录,**读失败退化为 None**(=未知,UI 显示「上次复查: 未知」)。
/// 单块盘的损坏复查记录不应让 `?` 拖垮整个状态收集 —— 否则无参 `bftool` 直接报错、
/// GUI 仪表盘整屏失效,而待归档数 / 事务残留 / 其余盘本与这条损坏记录无关。
/// 恢复路径:对该盘重跑 `bftool verify` 会重写记录。(review-r2 #5)
/// 但读失败不再静默吞:经 reporter 区分「损坏(Err→warn)」与「从未复查(Ok(None),静默)」,
/// 让 CLI 用户看到「记录损坏」而非误以为「从未复查」。GUI 传 NoopReporter 避免每次刷新刷屏。(review-r3 round4)
fn tolerant_last_verify(
    system_root: &std::path::Path,
    drive_id: &str,
    reporter: &dyn Reporter,
) -> Option<verify_state::LastVerify> {
    match verify_state::read_last_verify(system_root, drive_id) {
        Ok(v) => v,
        Err(e) => {
            reporter.warn(&format!(
                "盘「{}」的复查记录读取失败、已按『未知』显示(可能损坏);请对该盘重跑 verify:{:#}",
                drive_id, e
            ));
            None
        }
    }
}

/// 收集状态(扫描在线盘 + 各盘最近复查 + 待归档数 + 事务残留)。
/// `reporter`:用于在某盘复查记录损坏时发 warn(CLI 传真 reporter;GUI 传 NoopReporter 防刷屏)。
pub fn gather(cfg: &Config, reporter: &dyn Reporter) -> Result<StatusReport> {
    let mut drives = Vec::new();
    for d in drive::scan_mounted(None)? {
        let last_verify = tolerant_last_verify(&cfg.system_root, &d.id, reporter);
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
        pending_count: count_subdirs(&cfg.ready_root)?,
        txn_pending: paths::system_pending_txn(&cfg.system_root).is_file(),
    })
}

/// 状态面板是多行整屏渲染,CLI 直接 println(GUI 走 gather)。reporter 传给 gather 用于
/// 复查记录损坏时的 warn。
pub fn run(cfg: &Config, reporter: &dyn Reporter) -> Result<()> {
    let s = gather(cfg, reporter)?;
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
    // 强优化:下沉到 core 的权威 VerifyOutcome::label(),与 GUI dashboard 统一,消除文案漂移。
    o.label()
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

fn count_subdirs(p: &std::path::Path) -> Result<usize> {
    // 非目录(不存在 / 是文件 / 是链接)→ 0,不让 read_dir 的 Err 经 ? 拖垮整个 gather
    // (与 tolerant_last_verify 同精神:某项坏掉不应使整个状态面板失效)。(review-r2 R3-5)
    if !p.is_dir() {
        return Ok(0);
    }
    let mut count = 0usize;
    for e in fs::read_dir(p).with_context(|| format!("读取待备份目录失败：{}", p.display()))?
    {
        let e = e.with_context(|| format!("枚举待备份目录失败：{}", p.display()))?;
        if e.file_type()
            .with_context(|| format!("读取待备份项目类型失败：{}", e.path().display()))?
            .is_dir()
        {
            count += 1;
        }
    }
    Ok(count)
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
        let s = gather(&cfg, &crate::reporter::NoopReporter).unwrap();
        assert_eq!(s.pending_count, 2);
        assert!(!s.txn_pending);
        // 造一个事务残留
        std::fs::write(paths::system_pending_txn(&sys), "x").unwrap();
        assert!(
            gather(&cfg, &crate::reporter::NoopReporter)
                .unwrap()
                .txn_pending
        );
    }

    // ── review-r2 R3-5:ready_root 是文件(非目录)时 count_subdirs 返回 0,不 Err 拖垮 gather ──
    #[test]
    fn count_subdirs_tolerates_non_dir() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("not_a_dir");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(count_subdirs(&f).unwrap(), 0, "非目录应返回 0 而非 Err");
    }

    // ── review-r2 #5:单块盘复查记录损坏不应使整个状态面板崩 ──
    #[test]
    fn tolerant_last_verify_degrades_on_corrupt_log() {
        let d = tempfile::tempdir().unwrap();
        let sys = d.path().join("sys");
        std::fs::create_dir_all(&sys).unwrap();
        // 损坏的复查日志:数字列非法 → read_rows 解析失败 → read_last_verify 返回 Err。
        std::fs::write(
            paths::system_verify_log(&sys),
            "DriveId,When,Status,Bad,Extra\n备份1,2026-05-29T00:00:00Z,IssuesFound,notanum,0\n",
        )
        .unwrap();
        assert!(
            verify_state::read_last_verify(&sys, "备份1").is_err(),
            "前提:损坏日志确实让 read_last_verify 失败"
        );
        // 容错包装退化为 None(=未知),不 panic、不 Err、不拖垮整个 gather;
        // 但**不再静默**:经 reporter 发 warn,让损坏记录可被区分于『从未复查』。(review-r3 round4)
        let rep = RecReporter(std::sync::Mutex::new(Vec::new()));
        assert!(tolerant_last_verify(&sys, "备份1", &rep).is_none());
        assert!(
            rep.0
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.contains("复查记录读取失败")),
            "损坏复查日志应发 warn(不静默吞),实际:{:?}",
            rep.0.lock().unwrap()
        );
    }

    /// 记录型 Reporter:只收集 warn,用于断言「不静默」。
    struct RecReporter(std::sync::Mutex<Vec<String>>);
    impl Reporter for RecReporter {
        fn log(&self, _level: crate::reporter::LogLevel, _msg: &str) {}
        fn warn(&self, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
        fn progress_bytes(
            &self,
            _label: &str,
            _total: u64,
        ) -> Box<dyn crate::reporter::ProgressHandle> {
            Box::new(NoopProg)
        }
    }
    struct NoopProg;
    impl crate::reporter::ProgressHandle for NoopProg {
        fn inc(&mut self, _delta: u64) {}
        fn finish(&mut self) {}
    }
}

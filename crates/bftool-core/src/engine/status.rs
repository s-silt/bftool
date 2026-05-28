//! `bftool` 不带子命令时的输出：把"现在是什么样、可以做什么"一屏摆给用户看。

use anyhow::Result;
use std::fs;

use crate::config::Config;
use crate::engine::drive;
use crate::engine::paths;

pub fn run(cfg: &Config) -> Result<()> {
    println!("==================== 归档备份工具 (bftool) ====================");
    println!("  待备份(源)  : {}", display_root(&cfg.ready_root));
    println!("  已备份      : {}", display_root(&cfg.archived_root));
    println!("  备份系统    : {}", display_root(&cfg.system_root));
    println!(
        "  预留余量    : {} GB    稳定期 : {} 分钟    认盘最小容量 : {} GB",
        cfg.reserve_gb, cfg.stable_minutes, cfg.min_drive_gb
    );
    println!("-----------------------------------------------------------");

    // 检测当前在线的备份盘
    let drives = drive::scan_mounted()?;
    if drives.is_empty() {
        println!("  机械盘 · 当前在线 : 无（插入空盘后执行 `bftool init <盘符>` 初始化）");
    } else {
        for d in &drives {
            let tag = if d.sealed {
                "[已封盘]"
            } else {
                "[可用]  "
            };
            println!(
                "  机械盘 · 当前在线 : {}  {} ({}:)  剩余 {:.1}/{:.0} GB",
                tag,
                d.id,
                d.letter,
                d.free_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
                d.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0,
            );
        }
    }

    // 待归档项目数
    let pending = count_subdirs(&cfg.ready_root);
    println!("  待归档项目数: {}", pending);

    // pending 事务残留？
    let txn = paths::system_pending_txn(&cfg.system_root);
    if txn.is_file() {
        println!("  事务残留    : 是 —— 上次未完成的归档需要核对（详情运行任意子命令时会自检）");
        println!("                文件：{}", txn.display());
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

//! CLI 解析与子命令分发。
//!
//! 新手友好原则：
//! - 不传任何参数运行 `bftool` 会打印当前状态 + 可用子命令
//! - 所有默认值都偏保守（不自动认盘、完整 SHA256、不删源）
//! - 错误信息要带「具体怎么修」的提示

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::config::Config;
use crate::engine;

#[derive(Parser, Debug)]
#[command(
    name = "bftool",
    version,
    about = "归档备份工具：SSD → 机械盘，按文件夹分盘归档，SHA256 三重校验，绝不误删",
    long_about = "\
归档备份工具（bftool）

把 SSD 上「待备份」目录下的项目（每个子文件夹 = 一个项目），整体复制到\n\
机械硬盘并做 SHA256 三重校验；校验通过才会把 SSD 上的源移到「已备份」\n\
（仍保留、不删除）。盘满会封盘并提示换下一块。所有索引/清单/日志都\n\
留在盘内，离线自洽。\n\
\n\
快速上手：\n\
  bftool                             # 显示状态 + 可用子命令\n\
  bftool init E                      # 把 E: 盘初始化为下一个「备份N」\n\
  bftool archive --dry-run           # 演练（只检查不复制）\n\
  bftool archive                     # 开始归档\n\
  bftool verify E                    # 复查 E: 盘\n\
  bftool find 项目关键词             # 查询项目在哪块盘\n\
"
)]
pub struct Cli {
    /// 配置文件路径；不传则按当前目录 → 可执行文件目录 → %APPDATA%\bftool\config.toml 顺序查找
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 归档：把 待备份 下的项目逐个备份到当前机械盘
    Archive {
        /// 演练模式：只检查/列出会做什么，不真正复制、不写索引、不移动源
        #[arg(short = 'n', long, alias = "what-if")]
        dry_run: bool,

        /// 跳过 SHA256（只比对相对路径 + 大小 + 修改时间）；快但挡不住静默损坏
        #[arg(long)]
        no_hash: bool,

        /// 本次最多处理几个项目（0 = 不限）
        #[arg(long, default_value_t = 0)]
        limit: usize,

        /// 文件最近修改须早于现在 N 分钟才算「稳定」可归档
        #[arg(long)]
        stable_minutes: Option<u64>,

        /// 每块盘要留多少空闲余量（GB）
        #[arg(long)]
        reserve_gb: Option<u64>,

        /// 覆盖配置中的备份盘符（例如 E）。一般不用：默认会自动找
        #[arg(long)]
        drive: Option<String>,
    },

    /// 初始化一块空盘为下一个「备份N」（写本盘信息、设卷标）
    Init {
        /// 要初始化的盘符（例如 E、F）
        drive: String,

        /// 自定义编号；留空则自动取「备份N」
        #[arg(long)]
        id: Option<String>,

        /// 跳过防呆（系统盘 / 资料库盘 / 非空盘检查）。风险自负
        #[arg(long)]
        force: bool,
    },

    /// 复查：重算指定盘上每个项目的 SHA256，比对校验清单
    Verify {
        /// 盘符（例如 E）；不传则交互式选择已识别到的备份盘
        drive: Option<String>,
    },

    /// 查询：根据关键词在全局索引里查项目在哪块盘
    Find {
        /// 关键词（项目名或编号片段，区分大小写匹配）
        keyword: String,
    },

    /// 列出当前已挂载、可识别的备份盘
    Drives,

    /// 显示当前生效的配置
    ConfigShow,
}

pub fn dispatch(args: Cli) -> Result<()> {
    let cfg = Config::load(args.config.as_deref()).context("加载配置失败")?;

    match args.cmd {
        None => engine::status::run(&cfg),
        Some(Command::Archive {
            dry_run,
            no_hash,
            limit,
            stable_minutes,
            reserve_gb,
            drive,
        }) => {
            let mut cfg = cfg;
            if let Some(m) = stable_minutes {
                cfg.stable_minutes = m;
            }
            if let Some(r) = reserve_gb {
                cfg.reserve_gb = r;
            }
            engine::archive::run(
                &cfg,
                engine::archive::Options {
                    dry_run,
                    no_hash,
                    limit,
                    drive_letter_override: drive,
                },
            )
        }
        Some(Command::Init { drive, id, force }) => {
            engine::drive::init(&cfg, &drive, id.as_deref(), force)
        }
        Some(Command::Verify { drive }) => engine::verify::run(&cfg, drive.as_deref()),
        Some(Command::Find { keyword }) => engine::find::run(&cfg, &keyword),
        Some(Command::Drives) => engine::drive::list_mounted(&cfg),
        Some(Command::ConfigShow) => {
            println!(
                "{}",
                toml::to_string_pretty(&cfg).context("序列化配置失败")?
            );
            Ok(())
        }
    }
}

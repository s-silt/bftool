//! CLI 解析与子命令分发。
//!
//! 新手友好原则：
//! - 不传任何参数运行 `bftool` 会打印当前状态 + 可用子命令
//! - 所有默认值都偏保守（不自动认盘、完整 SHA256、不删源）
//! - 错误信息要带「具体怎么修」的提示

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use bftool_core::config::{Config, ConfigSource};
use bftool_core::engine;
use bftool_core::reporter::Reporter;

/// CLI 的"永不取消"标志:GUI 传可置位的 AtomicBool,CLI 不支持图形化取消,
/// 用这个工厂函数构造永远为 false 的标志,避免两处相同硬编码漂移。(EH-001)
fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}

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
    /// 配置文件路径；不传则按 当前目录\bftool.toml → 可执行文件目录\bftool.toml → %APPDATA%\bftool\config.toml 顺序查找
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

        /// 危险模式：跳过 SHA256 内容校验，只比对相对路径 + 大小 + 修改时间。
        /// 速度快但**挡不住静默损坏（比特腐烂）**，不适合不可再生的资料。
        /// 必须同时传 --i-understand-this-can-miss-bitrot 才会生效。
        #[arg(long)]
        unsafe_no_hash: bool,

        /// 与 --unsafe-no-hash 必须同时出现的"我懂这能漏掉比特腐烂"确认开关。
        /// 单独传它没用；存在的目的是让"不校验内容"难以误开。
        #[arg(long, requires = "unsafe_no_hash")]
        i_understand_this_can_miss_bitrot: bool,

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

        /// 关闭压缩包内部结构测试（默认开启）。
        /// 单独关掉它没问题：SHA256 字节级校验仍在，只是压缩包内部结构损坏可能被漏判。
        /// 但不能与 --unsafe-no-hash 同时用：两者一起关 → 只剩 size+count+mtime ≈ 无校验，会被拒绝。
        #[arg(long)]
        no_test_archives: bool,
    },

    /// 初始化一块空盘为下一个「备份N」（写本盘信息）
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
        /// 盘符（例如 E）；仅一块盘时可省略（自动选择），多块盘时会列出并要求显式指定
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

pub fn dispatch(args: Cli, reporter: &dyn Reporter) -> Result<()> {
    // 用 LoadedConfig 读:既拿到生效配置,也记住「从哪读的」供 config-show 显示(Spec D §4.3)。
    let loaded = Config::load_with_source(args.config.as_deref()).context("加载配置失败")?;
    // move 出 config(非 clone);loaded.source 仍可用(部分移动),config-show 分支再读它。(EH-002)
    let cfg = loaded.config;

    match args.cmd {
        None => engine::status::run(&cfg, reporter),
        Some(Command::Archive {
            dry_run,
            unsafe_no_hash,
            i_understand_this_can_miss_bitrot,
            limit,
            stable_minutes,
            reserve_gb,
            drive,
            no_test_archives,
        }) => {
            // 强制二次确认：让"不校验内容"难以误开。重要资料应当走完整 SHA256。
            if unsafe_no_hash && !i_understand_this_can_miss_bitrot {
                bail!(
                    "拒绝运行：--unsafe-no-hash 是危险模式，跳过 SHA256 内容校验会让\n\
                     比特腐烂（静默损坏）无法被发现。\n\
                     如果你**确实**理解风险（仅用于大量素材、且接受静默损坏不可见），\n\
                     请再次显式加上：\n\
                       --i-understand-this-can-miss-bitrot\n\
                     重要资料/不可再生资料：请去掉 --unsafe-no-hash，走完整 SHA256。"
                );
            }

            // Batch 3.5：--unsafe-no-hash + 任何方式关 archive test = 几乎无校验。
            // 判定收口到 core 的 verify_disabled,与 core::run 守卫同一真值(防漂移)。(ledger L-008)
            if engine::archive::verify_disabled(unsafe_no_hash, cfg.test_archives, no_test_archives)
            {
                bail!(
                    "拒绝运行：--unsafe-no-hash 与「压缩包测试关闭」不能同时存在。\n\
                     同时关掉 SHA256 内容校验和压缩包内部测试 → 只剩文件数 + 大小 + 修改时间，\n\
                     这等价于「没在做完整性校验」，任何静默损坏都查不出。\n\
                     如何修：\n\
                       - 想保留压缩包测试 → 去掉 --no-test-archives 或 bftool.toml 改 test_archives = true\n\
                       - 想用完整 SHA256 → 去掉 --unsafe-no-hash\n\
                       - 两个都要关 → 抱歉，工具拒绝这种组合"
                );
            }

            let mut cfg = cfg;
            if let Some(m) = stable_minutes {
                cfg.stable_minutes = m;
            }
            if let Some(r) = reserve_gb {
                cfg.reserve_gb = r;
            }
            // CLI 不支持图形化取消:传一个永不取消的标志(行为不变)。
            let no_cancel = no_cancel();
            let summary = engine::archive::run(
                &cfg,
                reporter,
                engine::archive::Options {
                    dry_run,
                    // core API 仍叫 no_hash：CLI 层负责让"开启它"变得困难。
                    no_hash: unsafe_no_hash,
                    limit,
                    drive_letter_override: drive,
                    no_test_archives,
                },
                &no_cancel,
            )?;
            if summary.failed > 0 {
                bail!(
                    "{} 个项目未成功归档(详见上方与「需人工处理.txt」);其余 {} 个已完成。\n\
                     (本命令以非零退出码结束,便于脚本/计划任务识别失败。)",
                    summary.failed,
                    summary.handled
                );
            }
            Ok(())
        }
        Some(Command::Init { drive, id, force }) => {
            engine::drive::init(&cfg, reporter, &drive, id.as_deref(), force)
        }
        Some(Command::Verify { drive }) => {
            // CLI 不支持图形化取消:传一个永不取消的标志(行为不变)。
            let no_cancel = no_cancel();
            let report = engine::verify::run(&cfg, reporter, drive.as_deref(), &no_cancel)?;
            if report.has_corruption() {
                bail!(
                    "复查发现 {} 处完整性问题（损坏/缺失/大小不符/读取失败等） —— 本盘完整性有问题。\n\
                     请用其它副本恢复受损项目,或重做本盘。\n\
                     (本命令以非零退出码结束,便于定期复查脚本/计划任务识别坏盘。)",
                    report.bad
                );
            }
            Ok(())
        }
        Some(Command::Find { keyword }) => engine::find::run(&cfg, &keyword),
        Some(Command::Drives) => engine::drive::list_mounted(&cfg, reporter),
        Some(Command::ConfigShow) => {
            // 查询性质，直接打到 stdout（GUI 端走 LoadedConfig getter，不解析这里的文本）。
            // 头一行用 TOML 注释标出「配置来源」——让用户/脚本明确当前生效配置从哪读的,
            // 且输出整体仍是合法 TOML 可直接管道/重定向。(Spec D §4.3)
            let origin = match &loaded.source {
                ConfigSource::Explicit(p) => format!("命令行 --config 指定：{}", p.display()),
                ConfigSource::Candidate(p) => format!("自动发现：{}", p.display()),
                ConfigSource::Default => {
                    "内置默认（未找到任何 bftool.toml / config.toml）".to_string()
                }
            };
            println!("# 配置来源：{}", origin);
            println!(
                "{}",
                toml::to_string_pretty(&cfg).context("序列化配置失败")?
            );
            Ok(())
        }
    }
}

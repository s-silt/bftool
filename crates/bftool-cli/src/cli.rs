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
use bftool_core::service::{
    self, ArchiveRequest, FileFilter, FindRequest, InitRequest, OperationRequest, OperationResult,
    SourceSpec, VerifyRequest, WatchRequest,
};

/// CLI 的"永不取消"标志:GUI 传可置位的 AtomicBool,CLI 不支持图形化取消,
/// 用这个工厂函数构造永远为 false 的标志,避免两处相同硬编码漂移。(EH-001)
#[allow(dead_code)]
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
  bftool archive --from <文件夹> --incremental --retain-source\n\
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

        /// 指定源文件夹（其直接子目录 = 项目）；省略则用配置 ready_root
        #[arg(long = "from")]
        from: Option<PathBuf>,

        /// 增量：完整内容快照与可信历史副本均匹配才跳过复制；变更项完整拷贝+SHA256
        #[arg(long)]
        incremental: bool,

        /// 校验通过后不把源移到 archived_root（源常驻，适合增量/监视）
        #[arg(long = "retain-source")]
        retain_source: bool,

        /// 仅处理含这些扩展名文件的项目（逗号分隔，如 zip,7z）；省略 = 全部
        #[arg(long = "ext", value_delimiter = ',')]
        ext: Option<Vec<String>>,
    },

    /// 初始化目标盘为下一个「备份N」（写本盘信息；不要求空盘）
    Init {
        /// 要初始化的盘符（例如 E、F）
        drive: String,

        /// 自定义编号；留空则自动取「备份N」
        #[arg(long)]
        id: Option<String>,

        /// 跳过硬闸防呆（系统盘 / 资料库盘）。非空盘默认允许，无需本开关。风险自负
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

    /// 监视文件夹：增量归档到机械盘池（poll；默认 retain_source）
    Watch {
        /// 监视的 SSD 文件夹
        folder: PathBuf,
        /// 轮询间隔秒（默认取配置 watch_poll_secs / 30）
        #[arg(long = "poll-secs")]
        poll_secs: Option<u64>,
        /// 只跑一轮（测试/CI）
        #[arg(long)]
        once: bool,
        /// 文件级扩展名过滤（逗号分隔）
        #[arg(long = "ext", value_delimiter = ',')]
        ext: Option<Vec<String>>,
        /// 递归匹配 --ext
        #[arg(long)]
        recursive: bool,
        /// 本次最多处理几个项目（0 = 不限）
        #[arg(long, default_value_t = 0)]
        limit: usize,
        #[arg(long)]
        dry_run: bool,
    },

    /// 显示当前生效的配置
    ConfigShow,
}

pub fn dispatch(args: Cli, reporter: &dyn Reporter) -> Result<()> {
    // 用 LoadedConfig 读:既拿到生效配置,也记住「从哪读的」供 config-show 显示(Spec D §4.3)。
    let loaded = Config::load_with_source(args.config.as_deref()).context("加载配置失败")?;
    // move 出 config(非 clone);loaded.source 仍可用(部分移动),config-show 分支再读它。(EH-002)
    let cfg = loaded.config;

    match args.cmd {
        None => service::run_with_reporter(&cfg, OperationRequest::Status, reporter).map(|_| ()),
        Some(Command::Archive {
            dry_run,
            unsafe_no_hash,
            i_understand_this_can_miss_bitrot,
            limit,
            stable_minutes,
            reserve_gb,
            drive,
            no_test_archives,
            from,
            incremental,
            retain_source,
            ext,
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
            // H7/Q1：增量路径禁止 --unsafe-no-hash（对拷贝项必须走 SHA256）；--force 无关本闸
            if engine::archive::incremental_forbids_no_hash(incremental, unsafe_no_hash) {
                bail!(
                    "拒绝运行：[deny.verify_skip_watch] --incremental 与 --unsafe-no-hash 不能同时使用。\n\
                     增量未变项会 skipped_unchanged（先核对内容与可信副本，再跳过复制）；\n\
                     实际要拷的 New/Changed 项必须走 SHA256 三重校验。"
                );
            }

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
            let include_ext = ext.filter(|v| !v.is_empty());
            let options = engine::archive::Options {
                dry_run,
                // core API 仍叫 no_hash：CLI 层负责让"开启它"变得困难。
                no_hash: unsafe_no_hash,
                limit,
                drive_letter_override: drive,
                no_test_archives,
                source_override: from.clone(),
                incremental,
                retain_source,
                include_ext: include_ext.clone(),
                file_globs: Vec::new(),
                ext_recursive: false,
                include_subfolder_projects: true,
                seed_from_global_catalog: incremental,
                incremental_verify: bftool_core::pipeline::archive::IncrementalVerifyMode::parse(
                    &cfg.incremental_verify,
                ),
            };
            let source = match from {
                Some(p) => {
                    let files = bftool_core::service::FileFilter::from_exts(
                        include_ext.clone().unwrap_or_default(),
                        false,
                    );
                    SourceSpec::Folder {
                        root: p,
                        include_subfolder_projects: true,
                        files,
                    }
                }
                None => SourceSpec::PendingRoot,
            };
            let result = service::run_with_reporter(
                &cfg,
                OperationRequest::Archive(ArchiveRequest {
                    options,
                    source,
                    incremental,
                    retain_source,
                    seed_from_global_catalog: incremental,
                }),
                reporter,
            )?;
            let OperationResult::Archive { summary } = result else {
                bail!("内部错误：archive 未返回 Archive 结果");
            };
            if summary.failed > 0 {
                bail!(
                    "{} 个项目未成功归档(详见上方与「需人工处理.txt」);其余 {} 个已完成。
                     (本命令以非零退出码结束,便于脚本/计划任务识别失败。)",
                    summary.failed,
                    summary.handled
                );
            }
            Ok(())
        }
        Some(Command::Init { drive, id, force }) => {
            // P1：CLI init 走统一 service::run（内部仍调 drive::init + pipeline stages）
            let req = OperationRequest::Init(InitRequest::from_legacy_force(drive, id, force));
            service::run_with_reporter(&cfg, req, reporter).map(|_| ())
        }
        Some(Command::Verify { drive }) => {
            let result = service::run_with_reporter(
                &cfg,
                OperationRequest::Verify(VerifyRequest { drive }),
                reporter,
            )?;
            match result {
                OperationResult::Verify { .. } | OperationResult::Ok => Ok(()),
                OperationResult::HardFail { message } => bail!(
                    "{} —— 本盘完整性有问题。
                     请用其它副本恢复受损项目,或重做本盘。
                     (本命令以非零退出码结束,便于定期复查脚本/计划任务识别坏盘。)",
                    message
                ),
                other => bail!("内部错误：verify 返回意外结果: {:?}", other),
            }
        }
        Some(Command::Find { keyword }) => service::run_with_reporter(
            &cfg,
            OperationRequest::Find(FindRequest { keyword }),
            reporter,
        )
        .map(|_| ()),
        Some(Command::Drives) => {
            service::run_with_reporter(&cfg, OperationRequest::Drives, reporter).map(|_| ())
        }
        Some(Command::Watch {
            folder,
            poll_secs,
            once,
            ext,
            recursive,
            limit,
            dry_run,
        }) => {
            let files = FileFilter::from_exts(ext.unwrap_or_default(), recursive);
            let archive = engine::archive::Options {
                dry_run,
                no_hash: false, // watch/增量禁止 no_hash（H7）
                limit,
                drive_letter_override: None,
                no_test_archives: false,
                source_override: Some(folder.clone()),
                incremental: true,
                retain_source: true,
                include_ext: if files.extensions.is_empty() {
                    None
                } else {
                    Some(files.extensions.clone())
                },
                file_globs: files.globs.clone(),
                ext_recursive: files.recursive,
                include_subfolder_projects: true,
                seed_from_global_catalog: true,
                incremental_verify: bftool_core::pipeline::archive::IncrementalVerifyMode::parse(
                    &cfg.incremental_verify,
                ),
            };
            let req = WatchRequest {
                folder,
                files,
                poll_secs: poll_secs.unwrap_or(0),
                once,
                archive,
            };
            let result = service::run_with_reporter(&cfg, OperationRequest::Watch(req), reporter)?;
            match result {
                OperationResult::Watch { failed, .. } if failed > 0 => {
                    bail!("{} 个项目未成功归档（详见上方）。", failed)
                }
                OperationResult::Watch { .. } | OperationResult::Ok => Ok(()),
                OperationResult::HardFail { message } => bail!("{message}"),
                other => bail!("内部错误：watch 返回意外结果: {:?}", other),
            }
        }
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

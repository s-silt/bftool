//! 归档公共类型与校验守卫真值。
use crate::engine::drive::BackupDrive;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub dry_run: bool,
    pub no_hash: bool,
    /// 本轮最多处理多少个项目。`0 = 不限`（处理本轮计划里的全部项目）。
    pub limit: usize,
    pub drive_letter_override: Option<String>,
    pub no_test_archives: bool,
    /// 覆盖发现根（`SourceSpec::Folder`）；None = 用 Config.ready_root。
    pub source_override: Option<std::path::PathBuf>,
    /// 计划阶段用 IncrementalIndex 过滤未变项。
    pub incremental: bool,
    /// 校验通过后不把源移到 archived_root（源常驻）。
    pub retain_source: bool,
    /// 仅处理含这些扩展名的文件/项目；None/空 + 无 globs = FolderProjects 默认。
    pub include_ext: Option<Vec<String>>,
    /// 额外 glob（P0' 简易前缀/后缀匹配；完整 glob 可后补）。
    pub file_globs: Vec<String>,
    /// `--ext` 是否递归扫描。
    pub ext_recursive: bool,
    /// 是否发现直接子文件夹项目（与 FileExtensions 可并存）。
    pub include_subfolder_projects: bool,
    /// 兼容旧请求字段；catalog 只能用于候选/占名，不可建立成功增量基线。
    pub seed_from_global_catalog: bool,
    /// 增量复验策略名称（兼容旧请求）；任何模式均要求完整可信内容比较。
    pub incremental_verify: super::incremental::IncrementalVerifyMode,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            dry_run: false,
            no_hash: false,
            limit: 0,
            drive_letter_override: None,
            no_test_archives: false,
            source_override: None,
            incremental: false,
            retain_source: false,
            include_ext: None,
            file_globs: Vec::new(),
            ext_recursive: false,
            include_subfolder_projects: true,
            seed_from_global_catalog: false,
            incremental_verify: Default::default(),
        }
    }
}

/// 一轮归档的结果汇总。`failed>0` = 有项目处理失败 —— CLI 据此设非零退出码,
/// 自动化/计划任务才能识别"批量归档里有失败"(此前总是 exit 0)。(ledger L-007)
/// `cancelled`/`sealed_stopped` 是独立态,**不计入 failed**(取消≠失败,封盘停≠失败)。(Spec D §4.5/§4.1)
#[derive(Debug, Default, Clone)]
pub struct ArchiveSummary {
    pub handled: usize,
    pub failed: usize,
    pub cancelled: bool,
    pub sealed_stopped: bool,
}

/// 本轮计划里的一项(GUI 预览 / CLI dry-run 渲染;`run_plan` 据此执行)。(Spec D §4.1)
#[derive(Debug, Clone)]
pub struct PlanItem {
    /// 展示名称；不能用于定位源或作为增量索引键。
    pub name: String,
    /// 计划时已解析的实际源根。
    pub source_root: PathBuf,
    /// 计划时已解析的实际源路径，执行时必须复验与相对身份的绑定。
    pub source_path: PathBuf,
    /// 源根内完整的相对路径（文件级递归筛选也不能丢失父目录）。
    pub source_relative: PathBuf,
    pub est_bytes: u64,
    /// `plan()` 时目标是否存在。这只是观察结果，不能证明目标属于本次归档或允许覆盖。
    pub dest_existed_at_plan: bool,
    pub action: PlanAction,
}

/// 对单个项目的计划动作。`dest_name` 在 `plan()` 阶段**冻结**(含重名时基于 `Local::now()`
/// 的时间戳名),保证预览的目标名 = 正式执行的目标名(不各自重算时间戳漂移)。(Spec D §4.1)
#[derive(Debug, Clone)]
pub enum PlanAction {
    /// 正常归档到 `dest_name`。
    Archive { dest_name: String },
    /// 本盘已有同名历史备份 → 改用唯一名 `dest_name` 归档。
    RenameAndArchive { dest_name: String },
    /// 不归档本项目(未稳定 / 0 文件 / 超单盘容量 / 索引损坏 …),`reason` 给原因。
    Skip(String),
    /// 余量不足放下本项目 → 封盘停本轮(非 skip:其后项目本轮不再尝试)。(Finding #2)
    SealAndStop(String),
}

/// 本轮归档计划:选定的盘 + 各项目动作 + 冻结的执行选项。
/// `plan()` 不动数据算出它;`run_plan()` 消费同一份执行(冻"意图"、不冻"安全判断")。(Spec D §4.1)
#[derive(Debug, Clone)]
pub struct ArchivePlan {
    pub drive: BackupDrive,
    pub items: Vec<PlanItem>,
    /// 计划的执行选项。公开执行入口仍重验 HashPolicy，并保持 dry_run 只读。
    /// `run_plan(cfg, plan, cancel, reporter)` 维持 4 参签名。
    pub opts: Options,
}

/// `plan()` 选不出可写盘时的友好信号(不是错误,CLI/GUI 据此提示插盘 init,退出码 0)。
#[derive(Debug)]
pub(super) struct NoWritableDrive;
impl std::fmt::Display for NoWritableDrive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "未发现可写入的备份盘")
    }
}
impl std::error::Error for NoWritableDrive {}

/// 是否处于"没有任何内容完整性校验"的状态:跳过 SHA256(no_hash) 且 archive test 实际关闭。
/// 等价于只剩 size+count+mtime,是禁止的组合。判定集中在此,core::run 守卫与 CLI 守卫共用
/// 同一真值,避免两处条件漂移把"无校验后门"悄悄打开。(ledger L-008)
///
/// SEC-008: 三个 bool 参数语义重叠，签名暂不改（改了牵动 core+cli 两处调用点，
/// 风险中、收益低）。三者各自含义与组合真值如下，调用方务必按命名传值：
/// - `no_hash`        —— 用户是否传了 `--unsafe-no-hash`（**关掉** SHA256 整文件校验）。
/// - `test_archives`  —— 配置项 `cfg.test_archives`（是否**启用**压缩包内部测试）。
/// - `no_test_archives` —— 本轮 `opts.no_test_archives`（是否**临时关掉**压缩包测试，覆盖配置）。
///
/// 「archive test 实际开启」≡ `test_archives && !no_test_archives`（配置开 且 本轮没关）。
/// 「无校验」≡ `no_hash && !(test_archives && !no_test_archives)`，化简即下式。真值表：
///
/// | no_hash | test_archives | no_test_archives | 结果(=无校验) |
/// |---------|---------------|------------------|----------------|
/// | false   | *             | *                | false（SHA256 兜底） |
/// | true    | true          | false            | false（压缩包测试兜底） |
/// | true    | false         | *                | **true**（配置没开测试） |
/// | true    | true          | true             | **true**（本轮关了测试） |
pub fn verify_disabled(no_hash: bool, test_archives: bool, no_test_archives: bool) -> bool {
    no_hash && (!test_archives || no_test_archives)
}

/// H7/Q1：incremental/watch 关闭 SHA256（`--unsafe-no-hash`）即硬拒。
/// 公开计划的执行入口也必须重验；可信的未变项仍可在 SHA256 开启时只读 Skip。
/// `--force` 不得绕过（调用方不得在 force 下跳过本检查）。
pub fn incremental_forbids_no_hash(incremental: bool, no_hash: bool) -> bool {
    incremental && no_hash
}

/// 校验强度的类型化表示:由"是否跑了 SHA256"派生,避免 "SHA256-OK"/"SIZE+COUNT" 字面量
/// 散落各处、与实际校验脱钩。token() 是写入索引 CSV 的稳定列值(保持旧版兼容)。(ledger L-020)
#[derive(Debug, Clone, Copy)]
pub(super) enum VerifyStatus {
    Sha256Ok,
    SizeCount,
}

impl VerifyStatus {
    pub(super) fn from_opts(no_hash: bool) -> Self {
        if no_hash {
            Self::SizeCount
        } else {
            Self::Sha256Ok
        }
    }
    pub(super) fn token(self) -> &'static str {
        match self {
            Self::Sha256Ok => "SHA256-OK",
            Self::SizeCount => "SIZE+COUNT",
        }
    }
}

/// 完整源相对身份的稳定键。拒绝空路径、父目录及非 UTF-8，避免身份折叠。
/// 只有 Windows 折叠大小写与分隔符；Unix 的反斜杠是合法文件名字符。
pub(super) fn source_relative_key(relative: &Path) -> anyhow::Result<String> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        anyhow::bail!("无效的源相对身份：{}", relative.display());
    }
    let key = relative
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("源路径不是有效 UTF-8：{}", relative.display()))?;
    Ok(if cfg!(windows) {
        key.replace('\\', "/").to_lowercase()
    } else {
        key.to_string()
    })
}

#[derive(Debug, Default, Clone)]
pub struct FolderStats {
    pub files: u64,
    pub bytes: u64,
    /// 目录内真实文件最新 mtime（UNIX 秒）；不可读则为 None。
    pub latest_mtime_secs: Option<i64>,
    pub enum_errors: Vec<String>,
    pub metadata_errors: Vec<String>,
}

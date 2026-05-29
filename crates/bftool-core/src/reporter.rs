//! 与界面解耦的报告接口。
//!
//! 设计目标：
//! - `bftool-core` 永远不直接 `println!`。所有面向用户的输出都通过 [`Reporter`]。
//! - CLI 端实现一个把调用渲染成短前缀标记（`[i]`/`[✓]`/`[!]`/`[x]`/`[>]`）+ indicatif 进度条的 reporter；
//!   未来 GUI 端实现自己的 reporter，把事件推到 channel 或更新 UI 状态。
//! - 进度条用 [`ProgressHandle`] 抽象，避免 core 依赖 indicatif。
//!
//! 这是 Batch 1b 的接口；下一批（5）会把追加索引等离散动作也变成结构化事件。

/// 日志级别。CLI 渲染时用不同短前缀标记（`[i]`/`[✓]`/`[!]`/`[x]`/`[>]`）；GUI 可按级别分色或选择是否声音提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    /// 普通信息（"开始处理…"、"复制完成"）
    Info,
    /// 成功（"✓ 归档成功"）
    Ok,
    /// 警告（"未稳定，跳过"、"目标多余文件"）
    Warn,
    /// 错误（"校验失败"、"未发现备份盘"）
    Error,
    /// 用户需要看见、可能需要行动的高亮提示（"封盘"、"事务残留"）
    Action,
}

/// 进度条句柄。引擎在长任务开始时拿一个，过程中 `inc`，结束时 `finish`（或 drop）。
///
/// CLI 端可以包装 indicatif::ProgressBar；GUI 端可以把进度推到状态管理。
/// 测试场景下用 [`NoopReporter`] 返回的句柄什么也不做。
pub trait ProgressHandle: Send {
    fn inc(&mut self, delta: u64);
    fn finish(&mut self);
}

/// 报告接口。引擎所有面向用户的输出都从这里走。
///
/// 设计成 object-safe（用 `&dyn Reporter` 传递），代码侵入最小。
pub trait Reporter: Send + Sync {
    /// 写一条日志。CLI 端按级别用 emoji 前缀打到 stdout/stderr，GUI 端推到事件流。
    fn log(&self, level: LogLevel, msg: &str);

    /// 开一个字节量进度条。`total = 0` 表示总量未知（CLI 端可以转 spinner）。
    ///
    /// 不需要进度条的实现（如测试用的 noop）可以返回一个什么都不做的句柄。
    fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle>;

    // 便利方法 —— 让调用点保持 `reporter.info("...")` 这种顺手的写法。
    fn info(&self, msg: &str) {
        self.log(LogLevel::Info, msg);
    }
    fn ok(&self, msg: &str) {
        self.log(LogLevel::Ok, msg);
    }
    fn warn(&self, msg: &str) {
        self.log(LogLevel::Warn, msg);
    }
    fn error(&self, msg: &str) {
        self.log(LogLevel::Error, msg);
    }
    fn action(&self, msg: &str) {
        self.log(LogLevel::Action, msg);
    }
}

/// 什么都不做的 Reporter。给单元测试、或库的非 CLI 调用者用。
pub struct NoopReporter;

impl Reporter for NoopReporter {
    fn log(&self, _level: LogLevel, _msg: &str) {}
    fn progress_bytes(&self, _label: &str, _total: u64) -> Box<dyn ProgressHandle> {
        Box::new(NoopProgress)
    }
}

struct NoopProgress;
impl ProgressHandle for NoopProgress {
    fn inc(&mut self, _delta: u64) {}
    fn finish(&mut self) {}
}

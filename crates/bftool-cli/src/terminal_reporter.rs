//! 把 `bftool_core::reporter::Reporter` 渲染成终端输出。
//!
//! - 日志按级别用 emoji 前缀打到 stdout（warn/error 走 stderr）
//! - 进度条用 indicatif，统一样式
//!
//! 未来桌面版会写一个 EventChannelReporter，把同样的接口推到 channel 给 UI 用。

use bftool_core::reporter::{LogLevel, ProgressHandle, Reporter};
use indicatif::{ProgressBar, ProgressStyle};

pub struct TerminalReporter;

impl Reporter for TerminalReporter {
    fn log(&self, level: LogLevel, msg: &str) {
        match level {
            LogLevel::Info => println!("[i] {}", msg),
            LogLevel::Ok => println!("[✓] {}", msg),
            LogLevel::Warn => eprintln!("[!] {}", msg),
            LogLevel::Error => eprintln!("[x] {}", msg),
            LogLevel::Action => println!("[>] {}", msg),
        }
    }

    fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle> {
        let pb = ProgressBar::new(total);
        pb.set_style(
            ProgressStyle::with_template(
                "{prefix:>10} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
            )
            .unwrap()
            .progress_chars("=> "),
        );
        pb.set_prefix(label.to_string());
        Box::new(IndicatifHandle(pb))
    }
}

struct IndicatifHandle(ProgressBar);

impl ProgressHandle for IndicatifHandle {
    fn inc(&mut self, delta: u64) {
        self.0.inc(delta);
    }
    fn finish(&mut self) {
        self.0.finish_and_clear();
    }
}

impl Drop for IndicatifHandle {
    fn drop(&mut self) {
        // 兜底：调用方忘记 finish 时也清干净进度条。
        self.0.finish_and_clear();
    }
}

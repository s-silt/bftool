//! 结构化操作事件。`hint.*` 永不失败；`deny.*` 对应硬闸。
//! **禁止** occupancy → Deny。

use crate::reporter::{LogLevel, Reporter};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    HintEmptyDisk {
        letter: String,
    },
    DenySystemDrive {
        letter: String,
    },
    DenyLibraryDrive {
        letter: String,
    },
    DenyPathSafety {
        message: String,
    },
    DenyHashAllOff,
    /// H7/Q1：watch/incremental 对拷贝项跳过内容校验
    DenyVerifySkipWatch,
    DenyMultiWritable {
        drives: Vec<String>,
    },
    /// 增量指纹未变、本项不拷（合法软跳过）
    ArchiveSkippedUnchanged {
        name: String,
    },
    WatchTick {
        source: std::path::PathBuf,
        drive: Option<String>,
    },
    WatchSourceDeleted {
        rel: String,
    },
}

impl Event {
    pub fn code(&self) -> &'static str {
        match self {
            Event::HintEmptyDisk { .. } => "hint.empty_disk",
            Event::DenySystemDrive { .. } => "deny.system_drive",
            Event::DenyLibraryDrive { .. } => "deny.library_drive",
            Event::DenyPathSafety { .. } => "deny.path_safety",
            Event::DenyHashAllOff => "deny.hash_all_off",
            Event::DenyVerifySkipWatch => "deny.verify_skip_watch",
            Event::DenyMultiWritable { .. } => "deny.multi_writable",
            Event::ArchiveSkippedUnchanged { .. } => "archive.skipped_unchanged",
            Event::WatchTick { .. } => "watch.tick",
            Event::WatchSourceDeleted { .. } => "watch.source_deleted",
        }
    }

    pub fn level(&self) -> LogLevel {
        match self {
            Event::HintEmptyDisk { .. }
            | Event::ArchiveSkippedUnchanged { .. }
            | Event::WatchTick { .. }
            | Event::WatchSourceDeleted { .. } => LogLevel::Warn,
            _ => LogLevel::Error,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Event::HintEmptyDisk { letter } => format!(
                "[{}] {}: 根目录非空（软提示，不阻断）",
                self.code(),
                letter
            ),
            Event::DenySystemDrive { letter } => {
                format!("[{}] 拒绝系统盘 {}", self.code(), letter)
            }
            Event::DenyLibraryDrive { letter } => {
                format!("[{}] 拒绝资料库盘 {}", self.code(), letter)
            }
            Event::DenyPathSafety { message } => format!("[{}] {}", self.code(), message),
            Event::DenyHashAllOff => format!(
                "[{}] 校验策略全关（SHA256 与压缩包测试同时关闭）",
                self.code()
            ),
            Event::DenyVerifySkipWatch => format!(
                "[{}] watch/增量路径禁止对拷贝项跳过校验（须走 SHA256；fingerprint 未变的 skipped_unchanged 除外）",
                self.code()
            ),
            Event::DenyMultiWritable { drives } => format!(
                "[{}] 多块可写盘同时在线：{}",
                self.code(),
                drives.join(", ")
            ),
            Event::ArchiveSkippedUnchanged { name } => format!(
                "[{}] {} 已备份且未变，增量跳过",
                self.code(),
                name
            ),
            Event::WatchTick { source, drive } => format!(
                "[{}] tick source={} drive={:?}",
                self.code(),
                source.display(),
                drive
            ),
            Event::WatchSourceDeleted { rel } => format!(
                "[{}] 源侧已删除（备份保留）：{}",
                self.code(),
                rel
            ),
        }
    }
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
}

pub struct ReporterSink<'a> {
    pub reporter: &'a dyn Reporter,
}

impl EventSink for ReporterSink<'_> {
    fn emit(&self, event: Event) {
        self.reporter.log(event.level(), &event.message());
    }
}

pub fn emit(reporter: &dyn Reporter, event: Event) {
    ReporterSink { reporter }.emit(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_codes_match_design() {
        assert_eq!(
            Event::HintEmptyDisk { letter: "E".into() }.code(),
            "hint.empty_disk"
        );
        assert_eq!(Event::DenyHashAllOff.code(), "deny.hash_all_off");
        assert_eq!(Event::DenyVerifySkipWatch.code(), "deny.verify_skip_watch");
        assert_eq!(
            Event::ArchiveSkippedUnchanged { name: "p".into() }.code(),
            "archive.skipped_unchanged"
        );
        assert_eq!(
            Event::DenyMultiWritable {
                drives: vec!["A".into()]
            }
            .code(),
            "deny.multi_writable"
        );
    }

    #[test]
    fn hint_empty_disk_never_error_level() {
        assert_eq!(
            Event::HintEmptyDisk { letter: "E".into() }.level(),
            LogLevel::Warn
        );
    }
}

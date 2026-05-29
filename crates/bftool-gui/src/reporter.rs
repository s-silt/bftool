//! GuiReporter:把 core 的 Reporter 调用桥到 GUI —— 日志推 channel,进度写共享状态。
//! UI 线程每帧 try_recv drain channel + 读 ProgressState 渲染;core 跑在后台线程。(Spec D §3)

use std::sync::{mpsc::Sender, Arc, Mutex};

use bftool_core::reporter::{LogLevel, ProgressHandle, Reporter};

/// 投递给 UI 的离散事件(目前只有日志;后续可加完成/里程碑)。
#[derive(Debug, Clone)]
pub enum UiEvent {
    Log { level: LogLevel, msg: String },
}

/// 当前进度(字节量)。UI 每帧读它渲染进度条。`active=false` = 无进行中的进度。
#[derive(Debug, Default, Clone)]
pub struct ProgressState {
    pub label: String,
    pub total: u64,
    pub current: u64,
    pub active: bool,
}

/// 实现 core `Reporter`。core 的 `Reporter` trait 要求 `Send + Sync`(后台线程持有,
/// 可能被多处并发调用),而 `mpsc::Sender` **是 `Send` 但不保证 `Sync`**(取决于 std
/// 版本)。故用 `Mutex<Sender>` 包裹换取稳定的 `Sync`。日志调用不频繁(每条 log 一次),
/// 锁只圈住一次 `send`,持有极短——锁开销可忽略,不会成为瓶颈。
/// 进度走 `Arc<Mutex<ProgressState>>`(同理临界区极短,见 app.rs status_bar 注释)。
pub struct GuiReporter {
    tx: Mutex<Sender<UiEvent>>,
    progress: Arc<Mutex<ProgressState>>,
}

impl GuiReporter {
    pub fn new(tx: Sender<UiEvent>, progress: Arc<Mutex<ProgressState>>) -> Self {
        Self {
            tx: Mutex::new(tx),
            progress,
        }
    }
}

impl Reporter for GuiReporter {
    fn log(&self, level: LogLevel, msg: &str) {
        // 发送失败(UI 已关 / 接收端 drop)只静默丢弃这条日志 —— 不是错误,不 panic、不阻塞后台任务。
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(UiEvent::Log {
                level,
                msg: msg.to_string(),
            });
        }
    }

    fn progress_bytes(&self, label: &str, total: u64) -> Box<dyn ProgressHandle> {
        // EH-004:total 可能为 0(空目录/0 字节项)。此处不特判——所有进度渲染处
        // (app.rs status_bar、dashboard::progress_card、util::progress_bar)都以
        // `p.active && p.total > 0` 过滤:total=0 时不画"满/空"进度条,改回退到 spinner
        // /"处理中…"文案,避免 0/0 算出 NaN 百分比或误显 100%。
        if let Ok(mut p) = self.progress.lock() {
            *p = ProgressState {
                label: label.to_string(),
                total,
                current: 0,
                active: true,
            };
        }
        Box::new(GuiProgress {
            progress: Arc::clone(&self.progress),
        })
    }
}

struct GuiProgress {
    progress: Arc<Mutex<ProgressState>>,
}

impl ProgressHandle for GuiProgress {
    fn inc(&mut self, delta: u64) {
        if let Ok(mut p) = self.progress.lock() {
            p.current = p.current.saturating_add(delta);
        }
    }
    fn finish(&mut self) {
        if let Ok(mut p) = self.progress.lock() {
            p.active = false;
            p.current = p.total;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bftool_core::reporter::{LogLevel, Reporter};

    #[test]
    fn log_events_flow_to_receiver() {
        let (tx, rx) = std::sync::mpsc::channel();
        let prog = Arc::new(Mutex::new(ProgressState::default()));
        let rep = GuiReporter::new(tx, prog);
        rep.info("hello");
        rep.error("boom");
        let got: Vec<UiEvent> = rx.try_iter().collect();
        assert_eq!(got.len(), 2);
        assert!(matches!(&got[0], UiEvent::Log { level: LogLevel::Info, msg } if msg == "hello"));
        assert!(matches!(&got[1], UiEvent::Log { level: LogLevel::Error, msg } if msg == "boom"));
    }

    #[test]
    fn progress_handle_updates_shared_state() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let prog = Arc::new(Mutex::new(ProgressState::default()));
        let rep = GuiReporter::new(tx, Arc::clone(&prog));
        let mut h = rep.progress_bytes("复制 X", 100);
        h.inc(30);
        h.inc(20);
        {
            let p = prog.lock().unwrap();
            assert_eq!(p.label, "复制 X");
            assert_eq!(p.total, 100);
            assert_eq!(p.current, 50);
            assert!(p.active);
        }
        h.finish();
        let p = prog.lock().unwrap();
        assert!(!p.active);
        assert_eq!(p.current, 100, "finish 把进度推满");
    }

    #[test]
    fn is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GuiReporter>();
    }
}

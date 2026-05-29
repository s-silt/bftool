//! 后台任务:在独立线程跑长任务(archive/verify),UI 轮询结果——**绝不在 UI 线程跑**。
//! 取消用 `Arc<AtomicBool>`,闭包内把 `&AtomicBool` 传给 core,在**项目边界**生效。(Spec D §3/§4.5)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// 任务最终结果(给 UI 呈现 Done/Failed 两类;取消由 core 收进摘要文案)。
#[derive(Debug)]
pub enum TaskOutcome {
    Done(String),
    Failed(String),
}

pub struct BackgroundTask {
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    result: Arc<Mutex<Option<TaskOutcome>>>,
}

impl BackgroundTask {
    /// 起一个后台线程跑 `f`(`f` 收到 cancel 标志,可在项目边界检查)。
    pub fn spawn<F>(f: F) -> Self
    where
        F: FnOnce(&AtomicBool) -> anyhow::Result<String> + Send + 'static,
    {
        let cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let cancel_t = Arc::clone(&cancel);
        let result_t = Arc::clone(&result);
        let handle = std::thread::spawn(move || {
            let outcome = match f(&cancel_t) {
                Ok(s) => TaskOutcome::Done(s),
                // {:#} 展开 anyhow 的错误链(含 context),保留"怎么修"链路。
                Err(e) => TaskOutcome::Failed(format!("{:#}", e)),
            };
            if let Ok(mut slot) = result_t.lock() {
                *slot = Some(outcome);
            }
        });
        Self {
            cancel,
            handle: Some(handle),
            result,
        }
    }

    /// 请求取消(置位标志;core 在项目边界读到后安全收尾)。
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 是否已请求取消(UI 据此把「取消」按钮置灰为"取消中…")。
    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 线程是否已结束(UI 每帧轮询;true 后可 take_outcome)。
    pub fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .map(|h| h.is_finished())
            .unwrap_or(true)
    }

    /// 取走最终结果(join 线程)。仅在 is_finished 后调用;重复调用返回 None。
    pub fn take_outcome(&mut self) -> Option<TaskOutcome> {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.result.lock().ok().and_then(|mut s| s.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_runs_to_done() {
        let mut t = BackgroundTask::spawn(|_cancel| Ok("完成 3 项".to_string()));
        while !t.is_finished() {
            std::thread::yield_now();
        }
        assert!(matches!(t.take_outcome(), Some(TaskOutcome::Done(s)) if s == "完成 3 项"));
    }

    #[test]
    fn task_reports_failure() {
        let mut t = BackgroundTask::spawn(|_cancel| anyhow::bail!("炸了"));
        while !t.is_finished() {
            std::thread::yield_now();
        }
        assert!(matches!(t.take_outcome(), Some(TaskOutcome::Failed(e)) if e.contains("炸了")));
    }

    #[test]
    fn cancel_flag_visible_to_closure() {
        let mut t = BackgroundTask::spawn(|cancel| {
            // 自旋到被取消(测试里立即 request_cancel)
            while !cancel.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            Ok("已响应取消".to_string())
        });
        assert!(!t.cancel_requested());
        t.request_cancel();
        assert!(t.cancel_requested());
        while !t.is_finished() {
            std::thread::yield_now();
        }
        assert!(matches!(t.take_outcome(), Some(TaskOutcome::Done(_))));
    }
}

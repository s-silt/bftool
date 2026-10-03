//! 后台任务:在独立线程跑长任务(archive/verify/plan),UI 轮询结果——**绝不在 UI 线程跑**。
//! 取消用 `Arc<AtomicBool>`,闭包内把 `&AtomicBool` 传给 core,在**项目边界**生效。(Spec D §3/§4.5)
//!
//! 泛型 `T`:run/verify 回传 `String` 摘要,plan 回传结构化 `ArchivePlan`——同一原语两用。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// 任务最终结果(Done 携带 `T`;Failed 携带展开后的错误链文案)。
#[derive(Debug)]
pub enum TaskOutcome<T> {
    Done(T),
    Failed(String),
}

pub struct BackgroundTask<T> {
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    result: Arc<Mutex<Option<TaskOutcome<T>>>>,
    /// Drop 时是否允许 detach(不 join)。默认 false = 写任务(archive run)必须 join 保数据不变量;
    /// 只读任务(plan 预览 / find 查询,不写盘)置 true,使关窗时不被慢速/掉线网络盘的阻塞读拖住 UI。
    /// (review-r3 round3)
    detach_on_drop: bool,
}

impl<T: Send + 'static> BackgroundTask<T> {
    /// 起一个后台线程跑 `f`(`f` 收到 cancel 标志,可在项目边界检查)。
    pub fn spawn<F>(f: F) -> Self
    where
        F: FnOnce(&AtomicBool) -> anyhow::Result<T> + Send + 'static,
    {
        let cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let cancel_t = Arc::clone(&cancel);
        let result_t = Arc::clone(&result);
        let handle = std::thread::spawn(move || {
            let outcome = match f(&cancel_t) {
                Ok(v) => TaskOutcome::Done(v),
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
            detach_on_drop: false,
        }
    }

    /// 标记为「只读任务」:Drop 时允许 detach(不 join)。仅用于 plan 预览 / find 查询这类
    /// **绝不写盘**的任务 —— 它们卡在慢速/掉线网络盘的阻塞读时,关窗不应让 UI 线程陪着 join 挂死;
    /// 线程随进程退出被回收,且只持有 cfg 克隆与 mpsc 发送端(通道关闭时 send 静默失败),detach 安全。
    /// **绝不可**用于 archive run 等写任务(那必须 join,见 Drop 注释的 AR-01 不变量)。(review-r3 round3)
    pub fn detachable(mut self) -> Self {
        self.detach_on_drop = true;
        self
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
    pub fn take_outcome(&mut self) -> Option<TaskOutcome<T>> {
        let join_result = self.handle.take().map(|h| h.join());
        let outcome = self.result.lock().ok().and_then(|mut s| s.take());
        // 若线程 join 返回 Err 且 result 为 None，说明任务 panic 了
        if outcome.is_none() {
            if let Some(Err(_)) = join_result {
                return Some(TaskOutcome::Failed("后台任务崩溃（内部错误）".to_string()));
            }
        }
        outcome
    }
}

/// Drop 时**先置取消**;写任务再 **join**(不 detach),只读任务可 detach。
/// 为什么写任务 join 而非 detach:与 core 的 AR-01 不变式一致——绝不让后台线程在进程/视图撤销后
/// 还继续改动半移动状态的归档(否则可能在退出后写坏盘/索引)。数据完整性 > 即时关闭。
/// 已知代价:若正在跑一个大项目,取消在**项目边界**才生效,join 会阻塞到下一个边界,
/// 关窗时窗口可能短暂"无响应"(几秒)。这是有意权衡,可接受。
/// 例外(detach_on_drop=true):plan 预览 / find 查询是**只读、不写盘**任务,卡在慢速/掉线网络盘的
/// 阻塞读时若也 join,会让关窗挂死数十秒~分钟级。这类任务 Drop 时 detach(丢弃 JoinHandle、不 join):
/// 线程随进程退出回收,只持有 cfg 克隆与 mpsc 发送端(通道已关时 send 静默失败),不写坏任何状态。(review-r3 round3)
impl<T> Drop for BackgroundTask<T> {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            if !self.detach_on_drop {
                let _ = h.join();
            }
            // detach_on_drop=true:不 join,h 在此 drop → 线程 detach,不阻塞关窗。
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain<T: Send + 'static>(mut t: BackgroundTask<T>) -> TaskOutcome<T> {
        while !t.is_finished() {
            std::thread::yield_now();
        }
        t.take_outcome().unwrap()
    }

    #[test]
    fn task_runs_to_done() {
        let t = BackgroundTask::spawn(|_cancel| Ok("完成 3 项".to_string()));
        assert!(matches!(drain(t), TaskOutcome::Done(s) if s == "完成 3 项"));
    }

    #[test]
    fn task_reports_failure() {
        let t = BackgroundTask::spawn(|_cancel| anyhow::bail!("炸了"));
        assert!(matches!(drain::<String>(t), TaskOutcome::Failed(e) if e.contains("炸了")));
    }

    #[test]
    fn task_carries_non_string_result() {
        // 泛型:Done 可携带任意 Send 类型(这里 Vec<u32> 模拟结构化结果如 ArchivePlan)。
        let t = BackgroundTask::spawn(|_cancel| Ok(vec![1u32, 2, 3]));
        assert!(matches!(drain(t), TaskOutcome::Done(v) if v == vec![1, 2, 3]));
    }

    #[test]
    fn task_panic_maps_to_failed() {
        // 契约(G-03):闭包 panic → 线程 result 槽为 None 且 join 返回 Err →
        // take_outcome 兜底为 TaskOutcome::Failed,UI 不会卡在"运行中"。
        // 临时静默 panic hook,避免测试输出里出现吓人的 backtrace。
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let t = BackgroundTask::<String>::spawn(|_cancel| panic!("boom"));
        let outcome = drain::<String>(t);
        std::panic::set_hook(prev);
        assert!(matches!(outcome, TaskOutcome::Failed(_)));
    }

    // ── review-r3 round3:只读任务 detachable() 后 Drop 不得 join 阻塞(否则关窗挂死)──
    #[test]
    fn detachable_task_drop_does_not_join() {
        // 任务阻塞等待一个永不到来的信号(模拟卡在慢速网络盘的阻塞读),且忽略 cancel。
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let t = BackgroundTask::spawn(move |_cancel| {
            let _ = rx.recv(); // 阻塞,直到 tx 被 drop 才返回 Err
            Ok("done".to_string())
        })
        .detachable();
        // 若 Drop 仍 join,这里会永久阻塞(测试挂死);detachable → 立即返回。
        drop(t);
        // 能走到这里即证明 Drop 未 join 阻塞。释放 tx 让被 detach 的线程收尾。
        drop(tx);
    }

    #[test]
    fn cancel_flag_visible_to_closure() {
        let t = BackgroundTask::spawn(|cancel| {
            while !cancel.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            Ok("已响应取消".to_string())
        });
        assert!(!t.cancel_requested());
        t.request_cancel();
        assert!(t.cancel_requested());
        assert!(matches!(drain(t), TaskOutcome::Done(_)));
    }

    #[test]
    fn write_task_drop_waits_for_cancelled_worker_cleanup() {
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let task = BackgroundTask::spawn(move |cancel| {
            while !cancel.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
            worker_finished.store(true, Ordering::Release);
            Ok(())
        });
        drop(task);
        assert!(
            finished.load(Ordering::Acquire),
            "write worker must finish before Drop returns"
        );
    }
}

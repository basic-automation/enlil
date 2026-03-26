//! Async Runtime — Minimal executor, waker, and reactor
//!
//! Provides a lightweight async runtime for the Enlil hypervisor.
//!
//! # Backends
//!
//! - **Linux:** Uses std threading for the reactor, epoll-based I/O wakeup.
//! - **Bare-metal:** Interrupt-driven reactor with per-CPU executors.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

/// A boxed future that can be sent across threads.
type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

/// A spawned async task.
struct Task {
    /// The future to poll.
    future: Mutex<BoxFuture>,
    /// Queue to re-enqueue ourselves on wake.
    queue: Arc<TaskQueue>,
}

impl Wake for Task {
    fn wake(self: Arc<Self>) {
        self.queue.push(self.clone());
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.queue.push(self.clone());
    }
}

// ---------------------------------------------------------------------------
// TaskQueue
// ---------------------------------------------------------------------------

/// Thread-safe queue of tasks ready to be polled.
struct TaskQueue {
    queue: Mutex<VecDeque<Arc<Task>>>,
}

impl TaskQueue {
    const fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
        }
    }

    fn push(&self, task: Arc<Task>) {
        self.queue.lock().unwrap().push_back(task);
    }

    fn pop(&self) -> Option<Arc<Task>> {
        self.queue.lock().unwrap().pop_front()
    }

    fn is_empty(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// A minimal single-threaded async executor.
///
/// Polls spawned futures to completion. This is intentionally simple —
/// no I/O reactor integration yet (that comes with real device backends).
pub struct Executor {
    queue: Arc<TaskQueue>,
}

impl Executor {
    /// Create a new executor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: Arc::new(TaskQueue::new()),
        }
    }

    /// Spawn a future onto this executor.
    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let task = Arc::new(Task {
            future: Mutex::new(Box::pin(future)),
            queue: self.queue.clone(),
        });
        self.queue.push(task);
    }

    /// Run the executor until all spawned tasks complete.
    ///
    /// This blocks the current thread.
    ///
    /// # Panics
    ///
    /// Panics if a task's [`Mutex`] is poisoned.
    pub fn run(&self) {
        while let Some(task) = self.queue.pop() {
            let waker = Waker::from(task.clone());
            let mut cx = Context::from_waker(&waker);
            let mut future = task.future.lock().unwrap();
            if future.as_mut().poll(&mut cx).is_pending() {
                // Task will re-enqueue itself via the waker when ready.
            }
            // If Ready, task is done — it just drops.
        }
    }

    /// Run the executor, polling once. Returns true if there are still pending tasks.
    ///
    /// # Panics
    ///
    /// Panics if a task's [`Mutex`] is poisoned.
    #[must_use]
    pub fn poll_once(&self) -> bool {
        if let Some(task) = self.queue.pop() {
            let waker = Waker::from(task.clone());
            let mut cx = Context::from_waker(&waker);
            let mut future = task.future.lock().unwrap();
            let _ = future.as_mut().poll(&mut cx);
        }
        !self.queue.is_empty()
    }

    /// Returns true if there are no pending tasks.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.queue.is_empty()
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// TaskPriority
// ---------------------------------------------------------------------------

/// Priority level for tasks spawned on a [`PriorityExecutor`].
///
/// Ordering: `High > Normal > Low`. The executor always drains higher-priority
/// queues before moving on to lower ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskPriority {
    /// Lowest priority — runs only when no Normal or High tasks are pending.
    Low = 0,
    /// Default priority.
    Normal = 1,
    /// Highest priority — always serviced first.
    High = 2,
}

// ---------------------------------------------------------------------------
// PriorityExecutor
// ---------------------------------------------------------------------------

/// A priority-aware single-threaded async executor.
///
/// Maintains three internal task queues (one per [`TaskPriority`] level).
/// When polling, it fully drains the High queue before touching Normal,
/// and Normal before Low, ensuring strict priority ordering.
pub struct PriorityExecutor {
    high: Arc<TaskQueue>,
    normal: Arc<TaskQueue>,
    low: Arc<TaskQueue>,
}

impl PriorityExecutor {
    /// Create a new priority executor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            high: Arc::new(TaskQueue::new()),
            normal: Arc::new(TaskQueue::new()),
            low: Arc::new(TaskQueue::new()),
        }
    }

    /// Spawn a future with the given priority.
    pub fn spawn(
        &self,
        future: impl Future<Output = ()> + Send + 'static,
        priority: TaskPriority,
    ) {
        let queue = self.queue_for(priority);
        let task = Arc::new(Task {
            future: Mutex::new(Box::pin(future)),
            queue: queue.clone(),
        });
        queue.push(task);
    }

    /// Spawn a future at [`TaskPriority::Normal`].
    pub fn spawn_default(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.spawn(future, TaskPriority::Normal);
    }

    /// Run the executor until all queues are empty.
    ///
    /// Tasks are polled in strict priority order: all High tasks are drained
    /// before any Normal task is polled, and all Normal tasks before any Low.
    ///
    /// # Panics
    ///
    /// Panics if a task's [`Mutex`] is poisoned.
    pub fn run(&self) {
        loop {
            // Always restart from the highest priority queue.
            if let Some(task) = self.high.pop() {
                Self::poll_task(&task);
                continue;
            }
            if let Some(task) = self.normal.pop() {
                Self::poll_task(&task);
                continue;
            }
            if let Some(task) = self.low.pop() {
                Self::poll_task(&task);
                continue;
            }
            // All queues empty.
            break;
        }
    }

    /// Poll one task from the highest non-empty queue.
    ///
    /// Returns `true` if there are still pending tasks in any queue.
    ///
    /// # Panics
    ///
    /// Panics if a task's [`Mutex`] is poisoned.
    #[must_use]
    pub fn poll_once(&self) -> bool {
        if let Some(task) = self.high.pop() {
            Self::poll_task(&task);
        } else if let Some(task) = self.normal.pop() {
            Self::poll_task(&task);
        } else if let Some(task) = self.low.pop() {
            Self::poll_task(&task);
        }
        !self.is_idle()
    }

    /// Returns `true` if all three queues are empty.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.high.is_empty() && self.normal.is_empty() && self.low.is_empty()
    }

    // -- private helpers ----------------------------------------------------

    const fn queue_for(&self, priority: TaskPriority) -> &Arc<TaskQueue> {
        match priority {
            TaskPriority::High => &self.high,
            TaskPriority::Normal => &self.normal,
            TaskPriority::Low => &self.low,
        }
    }

    fn poll_task(task: &Arc<Task>) {
        let waker = Waker::from(task.clone());
        let mut cx = Context::from_waker(&waker);
        let mut future = task.future.lock().unwrap();
        let _ = future.as_mut().poll(&mut cx);
    }
}

impl Default for PriorityExecutor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Reactor (stub)
// ---------------------------------------------------------------------------

/// I/O reactor — wakes tasks when I/O events are ready.
///
/// On Linux: will wrap epoll.
/// On bare-metal: will be interrupt-driven.
///
/// Currently a stub — real implementation comes when we have device I/O.
pub struct Reactor {
    _private: (),
}

impl Reactor {
    /// Create a new reactor.
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }

    /// Register interest in an I/O source. Returns a token for later use.
    #[must_use]
    pub const fn register(&self, _fd: usize) -> usize {
        // Stub — returns a dummy token.
        0
    }

    /// Wait for I/O events, waking associated tasks.
    pub fn wait(&self, timeout_ms: Option<u64>) {
        // Stub — does nothing yet.
        #[cfg(feature = "platform-linux")]
        {
            if let Some(ms) = timeout_ms {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }
    }
}

impl Default for Reactor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Convenience: block_on
// ---------------------------------------------------------------------------

/// Block the current thread on a single future until it completes.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => {
                // In a real implementation, we'd park the thread and wait
                // for a wakeup from the reactor. For now, spin.
                std::hint::spin_loop();
            }
        }
    }
}

/// Create a no-op waker (for `block_on`).
fn noop_waker() -> Waker {
    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }
    Waker::from(Arc::new(NoopWake))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn executor_runs_simple_task() {
        let exec = Executor::new();
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();

        exec.spawn(async move {
            done2.store(true, Ordering::SeqCst);
        });

        exec.run();
        assert!(done.load(Ordering::SeqCst));
    }

    #[test]
    fn executor_runs_multiple_tasks() {
        let exec = Executor::new();
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..10 {
            let c = counter.clone();
            exec.spawn(async move {
                c.fetch_add(1, Ordering::SeqCst);
            });
        }

        exec.run();
        assert_eq!(counter.load(Ordering::SeqCst), 10);
    }

    #[test]
    fn block_on_resolves_immediately() {
        let val = block_on(async { 42 });
        assert_eq!(val, 42);
    }

    #[test]
    fn executor_is_idle_after_completion() {
        let exec = Executor::new();
        assert!(exec.is_idle());

        exec.spawn(async {});
        assert!(!exec.is_idle());

        exec.run();
        assert!(exec.is_idle());
    }

    #[test]
    fn reactor_creation() {
        let reactor = Reactor::new();
        let token = reactor.register(0);
        assert_eq!(token, 0); // Stub returns 0.
    }

    // -- PriorityExecutor tests ---------------------------------------------

    #[test]
    fn priority_executor_respects_order() {
        let exec = PriorityExecutor::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        // Spawn in reverse priority order: Low, Normal, High.
        let o = order.clone();
        exec.spawn(async move { o.lock().unwrap().push("low"); }, TaskPriority::Low);

        let o = order.clone();
        exec.spawn(async move { o.lock().unwrap().push("normal"); }, TaskPriority::Normal);

        let o = order.clone();
        exec.spawn(async move { o.lock().unwrap().push("high"); }, TaskPriority::High);

        exec.run();

        let result = order.lock().unwrap();
        assert_eq!(*result, vec!["high", "normal", "low"]);
        drop(result);
    }

    #[test]
    fn priority_executor_mixed() {
        let exec = PriorityExecutor::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        // Interleaved spawn order.
        let o = order.clone();
        exec.spawn(async move { o.lock().unwrap().push("normal-1"); }, TaskPriority::Normal);

        let o = order.clone();
        exec.spawn(async move { o.lock().unwrap().push("low-1"); }, TaskPriority::Low);

        let o = order.clone();
        exec.spawn(async move { o.lock().unwrap().push("high-1"); }, TaskPriority::High);

        let o = order.clone();
        exec.spawn(async move { o.lock().unwrap().push("high-2"); }, TaskPriority::High);

        let o = order.clone();
        exec.spawn(async move { o.lock().unwrap().push("low-2"); }, TaskPriority::Low);

        let o = order.clone();
        exec.spawn(async move { o.lock().unwrap().push("normal-2"); }, TaskPriority::Normal);

        exec.run();

        let result = order.lock().unwrap();
        assert_eq!(
            *result,
            vec!["high-1", "high-2", "normal-1", "normal-2", "low-1", "low-2"]
        );
        drop(result);
    }
}

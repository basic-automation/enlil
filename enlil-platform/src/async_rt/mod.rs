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
    fn new() -> Self {
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
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Register interest in an I/O source. Returns a token for later use.
    pub fn register(&self, _fd: usize) -> usize {
        // Stub — returns a dummy token.
        0
    }

    /// Wait for I/O events, waking associated tasks.
    pub fn wait(&self, _timeout_ms: Option<u64>) {
        // Stub — does nothing yet.
        #[cfg(feature = "platform-linux")]
        {
            if let Some(ms) = _timeout_ms {
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

/// Create a no-op waker (for block_on).
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
}

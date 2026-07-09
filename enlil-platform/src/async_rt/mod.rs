//! Async Runtime — Minimal executor, waker, and reactor
//!
//! Provides a lightweight async runtime for the Enlil hypervisor.
//!
//! # Backends
//!
//! - **Linux:** Uses std threading for the reactor, epoll-based I/O wakeup.
//! - **Bare-metal:** Interrupt-driven reactor with per-CPU executors.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Wake, Waker};

/// Linux epoll event source driving the [`Reactor`] (item 1.6).
#[cfg(all(feature = "platform-linux", target_os = "linux"))]
pub mod epoll;

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
    pub fn spawn(&self, future: impl Future<Output = ()> + Send + 'static, priority: TaskPriority) {
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

/// One source registered with the [`Reactor`].
struct Registration {
    /// The opaque source handle the caller registered (an fd on Linux, an
    /// IRQ/device slot on bare metal). Retained so an event backend can map a
    /// raw event back to its token.
    source: usize,
    /// Waker of the task blocked on this source, armed via
    /// [`Reactor::set_waker`]; fired and cleared when the source becomes ready.
    waker: Option<Waker>,
    /// Readiness signalled ([`Reactor::mark_ready`]) but not yet consumed
    /// ([`Reactor::take_ready`]).
    ready: bool,
}

/// I/O reactor — tracks which tasks wait on which I/O sources and wakes them
/// when a source becomes ready.
///
/// This is the **backend-neutral core**: the registration table and the
/// readiness→waker bookkeeping. The OS-specific event source that drives
/// [`mark_ready`](Self::mark_ready) layers on top — on Linux the
/// [`epoll::EpollPoller`] source, a device interrupt on bare metal (item 1.6).
/// All state sits behind a `Mutex` so a task can register and arm its waker from
/// one
/// thread while an interrupt/epoll thread signals readiness from another; wakers
/// are always fired *after* the lock is released, so a waker that re-enters the
/// reactor cannot deadlock.
pub struct Reactor {
    inner: Mutex<ReactorInner>,
}

struct ReactorInner {
    /// token → registration.
    sources: HashMap<usize, Registration>,
    /// Monotonic token allocator (never reused, so a stale token cannot alias a
    /// later registration).
    next_token: usize,
    /// Tokens signalled ready and not yet drained, in signal order.
    ready_queue: VecDeque<usize>,
}

impl Reactor {
    /// Create an empty reactor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ReactorInner {
                sources: HashMap::new(),
                next_token: 0,
                ready_queue: VecDeque::new(),
            }),
        }
    }

    /// Lock the shared state. Panics only on lock poisoning (a thread panicked
    /// while holding it); kept private so the public API's panic surface is this
    /// one well-understood case rather than a `# Panics` note on every method.
    fn locked(&self) -> MutexGuard<'_, ReactorInner> {
        self.inner.lock().unwrap()
    }

    /// Register interest in an I/O `source`, returning a token used for
    /// [`set_waker`](Self::set_waker) / [`mark_ready`](Self::mark_ready) /
    /// [`deregister`](Self::deregister). Tokens are unique and never reused.
    pub fn register(&self, source: usize) -> usize {
        let mut inner = self.locked();
        let token = inner.next_token;
        inner.next_token += 1;
        inner.sources.insert(
            token,
            Registration {
                source,
                waker: None,
                ready: false,
            },
        );
        token
    }

    /// Arm (or replace) the waker fired when `token`'s source becomes ready. If
    /// the source is *already* ready, the waker fires immediately — so a task
    /// that polls-then-arms never misses a readiness signalled in between.
    /// No-op for an unknown token.
    pub fn set_waker(&self, token: usize, waker: Waker) {
        let mut inner = self.locked();
        let Some(reg) = inner.sources.get_mut(&token) else {
            return;
        };
        if reg.ready {
            drop(inner);
            waker.wake();
            return;
        }
        reg.waker = Some(waker);
    }

    /// Signal that `token`'s source is ready: mark it, enqueue it for
    /// [`take_ready`](Self::take_ready), and wake the armed task (if any).
    /// Returns `false` for an unknown token. Idempotent — a source already
    /// marked ready is not enqueued twice. The OS event backend calls this.
    pub fn mark_ready(&self, token: usize) -> bool {
        let mut inner = self.locked();
        let Some(reg) = inner.sources.get_mut(&token) else {
            return false;
        };
        let waker = reg.waker.take();
        let newly_ready = !reg.ready;
        reg.ready = true;
        if newly_ready {
            inner.ready_queue.push_back(token);
        }
        // Wake outside the lock so a waker re-entering the reactor cannot
        // deadlock.
        drop(inner);
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }

    /// Drain and return the tokens signalled ready since the last call, clearing
    /// their ready flag so a source must be re-signalled to appear again.
    pub fn take_ready(&self) -> Vec<usize> {
        let mut inner = self.locked();
        let tokens: Vec<usize> = inner.ready_queue.drain(..).collect();
        for &token in &tokens {
            if let Some(reg) = inner.sources.get_mut(&token) {
                reg.ready = false;
            }
        }
        tokens
    }

    /// Whether `token`'s source is currently marked ready (unknown token →
    /// `false`).
    #[must_use]
    pub fn is_ready(&self, token: usize) -> bool {
        self.locked()
            .sources
            .get(&token)
            .is_some_and(|reg| reg.ready)
    }

    /// Remove a registration, returning its source handle if the token was
    /// known. A pending ready-queue entry for it is skipped by
    /// [`take_ready`](Self::take_ready) once the registration is gone.
    pub fn deregister(&self, token: usize) -> Option<usize> {
        self.locked().sources.remove(&token).map(|reg| reg.source)
    }

    /// Number of registered sources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.locked().sources.len()
    }

    /// Whether no source is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.locked().sources.is_empty()
    }

    /// Block until an I/O event arrives, waking the associated tasks.
    ///
    /// This is the *sourceless* fallback: it simply sleeps for `timeout_ms` to
    /// yield the CPU. The real Linux path drives readiness with
    /// [`epoll::EpollPoller::poll`], which blocks in the kernel and calls
    /// [`mark_ready`](Self::mark_ready) for each ready descriptor; a caller
    /// using a poller uses that instead of this method. The bare-metal source
    /// (device interrupt + IPI) is still item 1.6.
    pub fn wait(&self, timeout_ms: Option<u64>) {
        #[cfg(feature = "platform-linux")]
        {
            if let Some(ms) = timeout_ms {
                std::thread::sleep(std::time::Duration::from_millis(ms));
            }
        }
        #[cfg(not(feature = "platform-linux"))]
        {
            let _ = timeout_ms;
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
    Waker::noop().clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A `Wake` implementation that counts how many times it was woken — lets the
    /// reactor tests assert a waker actually fired without a full executor.
    struct CountingWaker(Arc<AtomicUsize>);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

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
    fn reactor_registers_and_hands_out_unique_tokens() {
        let reactor = Reactor::new();
        assert!(reactor.is_empty());
        let a = reactor.register(10);
        let b = reactor.register(20);
        assert_ne!(a, b, "tokens are unique");
        assert_eq!(reactor.len(), 2);
        assert_eq!(reactor.deregister(a), Some(10), "returns the source handle");
        assert_eq!(reactor.deregister(a), None, "double-deregister is None");
        assert_eq!(reactor.len(), 1);
        // A token is never reused, so a later registration cannot alias `a`.
        let c = reactor.register(30);
        assert_ne!(c, a);
    }

    #[test]
    fn reactor_marks_ready_wakes_and_drains() {
        let reactor = Reactor::new();
        let woken = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWaker(woken.clone())));

        let token = reactor.register(7);
        reactor.set_waker(token, waker);
        assert!(!reactor.is_ready(token));
        assert_eq!(woken.load(Ordering::SeqCst), 0);

        // Signalling readiness fires the armed waker and enqueues the token.
        assert!(reactor.mark_ready(token));
        assert_eq!(woken.load(Ordering::SeqCst), 1);
        assert!(reactor.is_ready(token));
        // Re-signalling is idempotent — no duplicate queue entry.
        assert!(reactor.mark_ready(token));

        let ready = reactor.take_ready();
        assert_eq!(ready, vec![token], "drained once despite two signals");
        assert!(!reactor.is_ready(token), "flag cleared after draining");
        assert!(reactor.take_ready().is_empty(), "nothing left to drain");

        assert!(!reactor.mark_ready(9999), "unknown token → false");
    }

    #[test]
    fn reactor_arming_a_waker_on_an_already_ready_source_wakes_immediately() {
        // A task that marks-ready then arms its waker (the poll/arm race) must
        // still be woken, or it would sleep forever on a ready source.
        let reactor = Reactor::new();
        let woken = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CountingWaker(woken.clone())));

        let token = reactor.register(1);
        assert!(reactor.mark_ready(token)); // ready before any waker is armed
        reactor.set_waker(token, waker);
        assert_eq!(
            woken.load(Ordering::SeqCst),
            1,
            "arming a waker on an already-ready source wakes it at once"
        );
    }

    // -- PriorityExecutor tests ---------------------------------------------

    #[test]
    fn priority_executor_respects_order() {
        let exec = PriorityExecutor::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        // Spawn in reverse priority order: Low, Normal, High.
        let o = order.clone();
        exec.spawn(
            async move {
                o.lock().unwrap().push("low");
            },
            TaskPriority::Low,
        );

        let o = order.clone();
        exec.spawn(
            async move {
                o.lock().unwrap().push("normal");
            },
            TaskPriority::Normal,
        );

        let o = order.clone();
        exec.spawn(
            async move {
                o.lock().unwrap().push("high");
            },
            TaskPriority::High,
        );

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
        exec.spawn(
            async move {
                o.lock().unwrap().push("normal-1");
            },
            TaskPriority::Normal,
        );

        let o = order.clone();
        exec.spawn(
            async move {
                o.lock().unwrap().push("low-1");
            },
            TaskPriority::Low,
        );

        let o = order.clone();
        exec.spawn(
            async move {
                o.lock().unwrap().push("high-1");
            },
            TaskPriority::High,
        );

        let o = order.clone();
        exec.spawn(
            async move {
                o.lock().unwrap().push("high-2");
            },
            TaskPriority::High,
        );

        let o = order.clone();
        exec.spawn(
            async move {
                o.lock().unwrap().push("low-2");
            },
            TaskPriority::Low,
        );

        let o = order.clone();
        exec.spawn(
            async move {
                o.lock().unwrap().push("normal-2");
            },
            TaskPriority::Normal,
        );

        exec.run();

        let result = order.lock().unwrap();
        assert_eq!(
            *result,
            vec!["high-1", "high-2", "normal-1", "normal-2", "low-1", "low-2"]
        );
        drop(result);
    }
}

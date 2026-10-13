//! Redox kernel `WaitQueue` port.
//!
//! Redox's `kernel/src/sync/wait_queue.rs` parks *contexts* in FIFO order
//! until a notifier wakes them, letting the scheduler run something else
//! meanwhile. This port keeps the two properties that make a wait queue a
//! wait queue rather than a bare condvar:
//!
//! - **FIFO wake order** — [`WaitQueue::notify_one`] wakes the longest-waiting
//!   waiter, not an arbitrary one.
//! - **No missed wakeups** — each waiter is a parker node: a notify that
//!   lands between enqueue and park still wakes the waiter, because the
//!   unpark token survives until the park consumes it.
//!
//! Blocking backend: on Linux the waiter parks its OS thread
//! (`std::thread::park`); on bare metal it yields via
//! [`yield_now`](crate::threading::yield_now) until the flag flips — an
//! interrupt or another core's notify provides the wakeup, exactly the way
//! Redox's scheduler-driven wakeups do.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

#[cfg(feature = "platform-linux")]
use std::thread::{self, Thread};

use crate::sync::SpinLock;

/// One parked waiter. Shared between the queue (which pops and wakes it)
/// and the waiting thread (which watches `notified`).
struct Waiter {
    notified: AtomicBool,
    #[cfg(feature = "platform-linux")]
    thread: Thread,
}

impl Waiter {
    // Not `const`: on Linux this calls `thread::current()`, which is not a
    // `const fn` (clippy only suggests `const` under the bare-metal cfg).
    #[allow(clippy::missing_const_for_fn)]
    fn new() -> Self {
        Self {
            notified: AtomicBool::new(false),
            #[cfg(feature = "platform-linux")]
            thread: thread::current(),
        }
    }
}

/// Block until this waiter's `notified` flag is set.
#[cfg(feature = "platform-linux")]
fn park_until_notified(mine: &Waiter) {
    while !mine.notified.load(Ordering::Acquire) {
        thread::park();
    }
}

/// Block until this waiter's `notified` flag is set (bare metal).
#[cfg(all(not(feature = "platform-linux"), feature = "platform-baremetal"))]
fn park_until_notified(mine: &Waiter) {
    while !mine.notified.load(Ordering::Acquire) {
        crate::threading::yield_now();
    }
}

/// Block until this waiter's `notified` flag is set (no backend: spin).
#[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
fn park_until_notified(mine: &Waiter) {
    while !mine.notified.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

/// A FIFO queue of parked waiters with [`notify_one`](WaitQueue::notify_one)
/// / [`notify_all`](WaitQueue::notify_all) wakeups.
///
/// Like a condvar, a notify that arrives *before* the corresponding `wait`
/// is lost — pair waits with an explicit predicate when that race matters.
pub struct WaitQueue {
    inner: SpinLock<VecDeque<Arc<Waiter>>>,
}

impl WaitQueue {
    /// An empty wait queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(VecDeque::new()),
        }
    }

    /// Park the current waiter until [`notify_one`](Self::notify_one) or
    /// [`notify_all`](Self::notify_all) wakes it. Waiters wake in the order
    /// they arrived.
    pub fn wait(&self) {
        let mine = Arc::new(Waiter::new());
        {
            let mut inner = self.inner.lock();
            inner.push_back(Arc::clone(&mine));
        }
        // A notify that landed between the enqueue and now already set the
        // flag (or left an unpark token on Linux), so no wakeup is missed.
        park_until_notified(&mine);
        // The notifier popped our node; nothing to clean up.
    }

    /// Park until woken or `timeout` elapses. Returns `true` when a notify
    /// woke this waiter, `false` on timeout.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let mine = Arc::new(Waiter::new());
        {
            let mut inner = self.inner.lock();
            inner.push_back(Arc::clone(&mine));
        }
        #[cfg(feature = "platform-linux")]
        {
            let deadline = std::time::Instant::now() + timeout;
            while !mine.notified.load(Ordering::Acquire) {
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                thread::park_timeout(deadline - now);
            }
        }
        #[cfg(not(feature = "platform-linux"))]
        {
            let start = crate::time::Instant::now();
            while !mine.notified.load(Ordering::Acquire) {
                if start.elapsed() >= timeout {
                    break;
                }
                crate::threading::yield_now();
            }
        }
        // If we are still queued, the timeout won: dequeue ourselves and
        // report it. If a notifier already popped us, its wakeup counts.
        // `is_some_and` (rather than `map_or`) keeps the position lookup and
        // the removal under the single lock hold above — re-locking between
        // the two could remove a shifted, unrelated node.
        let mut inner = self.inner.lock();
        let timed_out = inner
            .iter()
            .position(|w| Arc::ptr_eq(w, &mine))
            .is_some_and(|p| {
                inner.remove(p);
                true
            });
        drop(inner);
        !timed_out
    }

    /// Wake the longest-waiting waiter. Returns whether anyone was woken.
    pub fn notify_one(&self) -> bool {
        let waiter = self.inner.lock().pop_front();
        let Some(waiter) = waiter else {
            return false;
        };
        waiter.notified.store(true, Ordering::Release);
        #[cfg(feature = "platform-linux")]
        waiter.thread.unpark();
        true
    }

    /// Wake every waiter, in arrival order. Returns how many were woken.
    pub fn notify_all(&self) -> usize {
        let mut woken = 0;
        loop {
            let waiter = self.inner.lock().pop_front();
            let Some(waiter) = waiter else {
                break;
            };
            waiter.notified.store(true, Ordering::Release);
            #[cfg(feature = "platform-linux")]
            waiter.thread.unpark();
            woken += 1;
        }
        woken
    }

    /// Number of waiters currently parked.
    #[must_use]
    pub fn waiter_count(&self) -> usize {
        self.inner.lock().len()
    }

    /// Whether no waiter is parked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.waiter_count() == 0
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the Linux-gated tests below use `Vec` / `vec!`.
    #[cfg(feature = "platform-linux")]
    use alloc::{vec, vec::Vec};

    #[cfg(feature = "platform-linux")]
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    #[test]
    fn notify_on_empty_queue_wakes_nobody() {
        let q = WaitQueue::new();
        assert!(q.is_empty());
        assert!(!q.notify_one());
        assert_eq!(q.notify_all(), 0);
    }

    #[cfg(feature = "platform-linux")]
    #[test]
    fn notify_one_wakes_in_fifo_order() {
        let q = StdArc::new(WaitQueue::new());
        let woken: StdArc<StdMutex<Vec<u32>>> = StdArc::new(StdMutex::new(Vec::new()));

        for i in 0..4u32 {
            let (q2, w2) = (StdArc::clone(&q), StdArc::clone(&woken));
            std::thread::spawn(move || {
                q2.wait();
                w2.lock().unwrap().push(i);
            });
            // Let this thread enqueue before the next spawns, so the arrival
            // order is deterministic.
            while q.waiter_count() != (i + 1) as usize {
                std::thread::yield_now();
            }
        }

        for expected in 0..4u32 {
            assert!(q.notify_one());
            // Wait for the woken thread to record before the next notify, so
            // the recorded order equals the wake order.
            while woken.lock().unwrap().len() != (expected + 1) as usize {
                std::thread::yield_now();
            }
        }
        assert_eq!(*woken.lock().unwrap(), vec![0, 1, 2, 3]);
        assert!(q.is_empty());
    }

    #[cfg(feature = "platform-linux")]
    #[test]
    fn notify_all_wakes_everyone() {
        let q = StdArc::new(WaitQueue::new());
        let woken: StdArc<StdMutex<Vec<u32>>> = StdArc::new(StdMutex::new(Vec::new()));
        let mut handles = Vec::new();

        for i in 0..8u32 {
            let (q2, w2) = (StdArc::clone(&q), StdArc::clone(&woken));
            handles.push(std::thread::spawn(move || {
                q2.wait();
                w2.lock().unwrap().push(i);
            }));
        }
        while q.waiter_count() != 8 {
            std::thread::yield_now();
        }
        assert_eq!(q.notify_all(), 8);
        for h in handles {
            h.join().unwrap();
        }
        let mut got = woken.lock().unwrap().clone();
        got.sort_unstable();
        assert_eq!(got, (0..8u32).collect::<Vec<_>>());
        assert!(q.is_empty());
    }

    #[cfg(feature = "platform-linux")]
    #[test]
    fn wait_timeout_expires_and_succeeds() {
        let q = WaitQueue::new();
        // Nobody notifies: the wait must time out, not hang.
        assert!(!q.wait_timeout(Duration::from_millis(50)));
        assert!(q.is_empty());

        // A notify racing the wait still wakes it.
        let q = StdArc::new(WaitQueue::new());
        let q2 = StdArc::clone(&q);
        let handle = std::thread::spawn(move || q2.wait_timeout(Duration::from_secs(10)));
        while q.waiter_count() != 1 {
            std::thread::yield_now();
        }
        assert!(q.notify_one());
        assert!(handle.join().unwrap());
    }
}

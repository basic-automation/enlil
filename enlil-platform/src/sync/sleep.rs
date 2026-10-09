//! A sleep-lock: a blocking mutex whose contended waiters *sleep* instead of
//! spinning (Phase 6.2, T-6.9).
//!
//! [`SpinLock`](super::spinlock::SpinLock) busy-waits, which is right for
//! short, bounded critical sections but burns the waiter's whole quantum on
//! longer ones. [`SleepLock`] blocks instead:
//!
//! - **Linux:** delegates to `std::sync::Mutex` — the OS parks the waiter in
//!   a futex until the holder releases. A true sleep.
//! - **Bare metal:** the waiter yields its quantum to the scheduler via
//!   [`crate::threading::yield_now`] instead of `pause`-spinning — with
//!   interrupts enabled that halts the CPU in `hlt` until the next interrupt
//!   (the scheduler's LAPIC timer tick guarantees progress once preemption
//!   is live), so the lock holder is scheduled promptly instead of fighting
//!   a spinning waiter for the core.
//!
//! The raw exclusion cell on bare metal is the audited
//! [`SpinLock`](super::spinlock::SpinLock); `SleepLock` differs from it only
//! in its wait policy. On Linux the whole lock is `std`'s.
//!
//! Do not take a sleep-lock with interrupts masked for unbounded time on
//! bare metal: the waiter can only sleep when an interrupt can wake it.

use core::ops::{Deref, DerefMut};

#[cfg(feature = "platform-baremetal")]
use super::spinlock::SpinLock;

/// A mutual-exclusion lock whose waiters block (sleep) instead of spinning.
///
/// See the [module docs](self) for the backend strategy. Like
/// [`SpinLock`](super::spinlock::SpinLock), the guard releases on drop with
/// release ordering so the next holder sees the critical section's writes.
pub struct SleepLock<T> {
    #[cfg(feature = "platform-linux")]
    inner: std::sync::Mutex<T>,
    #[cfg(feature = "platform-baremetal")]
    inner: SpinLock<T>,
}

impl<T> SleepLock<T> {
    /// Create a new unlocked sleep-lock around `value`.
    pub const fn new(value: T) -> Self {
        Self {
            #[cfg(feature = "platform-linux")]
            inner: std::sync::Mutex::new(value),
            #[cfg(feature = "platform-baremetal")]
            inner: SpinLock::new(value),
        }
    }

    /// Acquire the lock, blocking until it is free, and return a guard.
    ///
    /// On Linux the thread sleeps in the OS while contended; on bare metal
    /// it yields its quantum to the scheduler (see
    /// [`crate::threading::yield_now`]) rather than spinning.
    ///
    /// # Panics
    ///
    /// Panics if the underlying mutex is poisoned (Linux).
    pub fn lock(&self) -> SleepGuard<'_, T> {
        #[cfg(feature = "platform-linux")]
        {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(|e| panic!("sleep-lock poisoned: {e}"));
            SleepGuard { inner }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            // Fast path: uncontended acquire, no scheduler interaction.
            if let Some(inner) = self.inner.try_lock() {
                return SleepGuard { inner };
            }
            // Slow path: block instead of spinning. Each iteration yields so
            // the holder gets the CPU promptly.
            loop {
                crate::threading::yield_now();
                if let Some(inner) = self.inner.try_lock() {
                    return SleepGuard { inner };
                }
            }
        }
    }

    /// Try to acquire the lock without blocking, returning `None` if it is
    /// held (or poisoned, on Linux).
    #[must_use]
    pub fn try_lock(&self) -> Option<SleepGuard<'_, T>> {
        #[cfg(feature = "platform-linux")]
        {
            self.inner.try_lock().ok().map(|inner| SleepGuard { inner })
        }
        #[cfg(feature = "platform-baremetal")]
        {
            self.inner.try_lock().map(|inner| SleepGuard { inner })
        }
    }

    /// Whether the lock is currently held (a racy observation — for
    /// diagnostics and self-tests, not for synchronization decisions).
    #[must_use]
    pub fn is_locked(&self) -> bool {
        #[cfg(feature = "platform-linux")]
        {
            // A probe acquire: succeeds (then immediately releases) when free.
            self.inner.try_lock().is_err()
        }
        #[cfg(feature = "platform-baremetal")]
        {
            self.inner.is_locked()
        }
    }
}

/// An RAII guard that holds a [`SleepLock`] and releases it on drop.
pub struct SleepGuard<'a, T> {
    #[cfg(feature = "platform-linux")]
    inner: std::sync::MutexGuard<'a, T>,
    #[cfg(feature = "platform-baremetal")]
    inner: super::spinlock::SpinGuard<'a, T>,
}

impl<T> Deref for SleepGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T> DerefMut for SleepGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncontended_lock_guards_and_mutates() {
        let lock = SleepLock::new(10u32);
        {
            let mut g = lock.lock();
            *g += 5;
        }
        assert_eq!(*lock.lock(), 15);
    }

    #[test]
    fn try_lock_fails_while_held_and_succeeds_after_release() {
        let lock = SleepLock::new(());
        let g = lock.lock();
        assert!(lock.is_locked());
        assert!(lock.try_lock().is_none()); // held → rejected
        drop(g);
        assert!(!lock.is_locked());
        assert!(lock.try_lock().is_some()); // free → acquired
    }

    #[test]
    fn waiter_blocks_until_holder_releases() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let lock = Arc::new(SleepLock::new(0u64));
        let holder = Arc::clone(&lock);
        let guard = holder.lock();
        let start = Instant::now();
        // The waiter must block (not spin forever, not return early) until
        // the holder drops the guard.
        let waiter = std::thread::spawn(move || {
            let mut g = lock.lock();
            *g = 42;
        });
        // Give the waiter time to block on the held lock.
        std::thread::sleep(Duration::from_millis(100));
        assert!(holder.is_locked());
        drop(guard);
        waiter.join().unwrap();
        assert!(start.elapsed() >= Duration::from_millis(100));
        assert_eq!(*holder.lock(), 42);
    }

    #[test]
    fn contended_sleep_lock_serializes() {
        use std::sync::Arc;
        let lock = Arc::new(SleepLock::new(0u64));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        *lock.lock() += 1;
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*lock.lock(), 8 * 500);
    }
}

//! A spinlock for the bare-metal enlil kernel (Phase 6.2).
//!
//! With no OS futex underneath it, the kernel guards shared state (device
//! models, the per-CPU tables once SMP brings up more cores) with a spinning
//! test-and-set lock. This one is a thin, audited primitive: an `AtomicBool`
//! flag, acquire/release ordering so a critical section's writes are visible to
//! the next holder, and a RAII guard that releases on drop. It is fully
//! target-agnostic (only `core::sync::atomic`), so it compiles and is tested on
//! the dev host as well as the firmware target.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A mutual-exclusion lock that busy-waits (spins) until it can acquire.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the lock serializes all access to `value`, so it is safe to share a
// `&SpinLock<T>` across threads/cores as long as `T` can be sent between them.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: moving the lock moves the `T`; that only needs `T: Send`.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Create a new unlocked spinlock around `value`.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it is free, and return a guard.
    ///
    /// Uses an acquire load on success so the critical section sees every write
    /// the previous holder made before it released (release on drop).
    pub fn lock(&self) -> SpinGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            // Spin without hammering the cache line / bus while it stays taken.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    /// Try to acquire the lock without spinning, returning `None` if it is held.
    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        // Acquire on success pairs with the release in `SpinGuard::drop`.
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SpinGuard { lock: self })
        } else {
            None
        }
    }

    /// Whether the lock is currently held (a racy observation — for diagnostics
    /// and self-tests, not for synchronization decisions).
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

/// An RAII guard that holds a [`SpinLock`] and releases it on drop.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: holding the guard means we hold the lock, so the reference is
        // exclusive for its lifetime.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: holding the guard means we hold the lock exclusively.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        // Release so the next holder's acquire sees our writes.
        self.lock.locked.store(false, Ordering::Release);
    }
}

#[cfg(target_os = "uefi")]
pub use hw::selftest;

#[cfg(target_os = "uefi")]
mod hw {
    use super::SpinLock;

    /// Exercise the spinlock's acquire/mutate/contended/release cycle, returning
    /// whether every step behaved (the kernel's boot self-test).
    ///
    /// Single boot CPU, so this proves the atomic mechanics rather than true
    /// contention: a held lock rejects a second `try_lock`, the guarded value is
    /// mutable through the guard, and releasing frees it again.
    #[must_use]
    pub fn selftest() -> bool {
        let lock = SpinLock::new(0u32);
        {
            let mut guard = lock.lock();
            *guard = 0x1234;
            // While the guard is held, a second acquire must fail.
            if lock.try_lock().is_some() || !lock.is_locked() {
                return false;
            }
        }
        // Released: the value persisted and the lock is free again.
        lock.try_lock().is_some_and(|g| *g == 0x1234)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncontended_lock_guards_and_mutates() {
        let lock = SpinLock::new(10u32);
        {
            let mut g = lock.lock();
            *g += 5;
        }
        assert_eq!(*lock.lock(), 15);
    }

    #[test]
    fn try_lock_fails_while_held_and_succeeds_after_release() {
        let lock = SpinLock::new(());
        let g = lock.lock();
        assert!(lock.is_locked());
        assert!(lock.try_lock().is_none()); // held → rejected
        drop(g);
        assert!(!lock.is_locked());
        assert!(lock.try_lock().is_some()); // free → acquired
    }

    #[test]
    fn release_publishes_writes_to_the_next_holder() {
        let lock = SpinLock::new(0u64);
        {
            *lock.lock() = 0xDEAD_BEEF;
        }
        assert_eq!(*lock.lock(), 0xDEAD_BEEF);
    }
}

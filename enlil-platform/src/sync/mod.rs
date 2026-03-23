//! Synchronization Primitives
//!
//! Platform-abstracted Mutex, RwLock, and Condvar.
//!
//! - **Linux:** Delegates to `std::sync` (pthread-backed).
//! - **Bare-metal:** Uses `spin` crate for spinlock-based implementations.

use core::ops::{Deref, DerefMut};

// ===========================================================================
// Mutex
// ===========================================================================

/// A platform-abstracted mutual exclusion lock.
pub struct Mutex<T: ?Sized> {
    #[cfg(feature = "platform-linux")]
    inner: std::sync::Mutex<T>,
    #[cfg(feature = "platform-baremetal")]
    inner: spin::Mutex<T>,
}

// Safety: inner types are already Send + Sync where T: Send.
unsafe impl<T: ?Sized + Send> Send for Mutex<T> {}
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates a new mutex wrapping the given value.
    pub fn new(value: T) -> Self {
        Self {
            #[cfg(feature = "platform-linux")]
            inner: std::sync::Mutex::new(value),
            #[cfg(feature = "platform-baremetal")]
            inner: spin::Mutex::new(value),
        }
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Acquires the mutex, blocking until available.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        #[cfg(feature = "platform-linux")]
        {
            MutexGuard {
                inner: MutexGuardInner::Linux(self.inner.lock().unwrap()),
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            MutexGuard {
                inner: MutexGuardInner::Baremetal(self.inner.lock()),
            }
        }
    }

    /// Attempts to acquire the mutex without blocking.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        #[cfg(feature = "platform-linux")]
        {
            self.inner.try_lock().ok().map(|g| MutexGuard {
                inner: MutexGuardInner::Linux(g),
            })
        }
        #[cfg(feature = "platform-baremetal")]
        {
            self.inner.try_lock().map(|g| MutexGuard {
                inner: MutexGuardInner::Baremetal(g),
            })
        }
    }
}

/// RAII guard for `Mutex`.
pub struct MutexGuard<'a, T: ?Sized> {
    inner: MutexGuardInner<'a, T>,
}

enum MutexGuardInner<'a, T: ?Sized> {
    #[cfg(feature = "platform-linux")]
    Linux(std::sync::MutexGuard<'a, T>),
    #[cfg(feature = "platform-baremetal")]
    Baremetal(spin::MutexGuard<'a, T>),
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.inner {
            #[cfg(feature = "platform-linux")]
            MutexGuardInner::Linux(g) => g.deref(),
            #[cfg(feature = "platform-baremetal")]
            MutexGuardInner::Baremetal(g) => g.deref(),
        }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match &mut self.inner {
            #[cfg(feature = "platform-linux")]
            MutexGuardInner::Linux(g) => g.deref_mut(),
            #[cfg(feature = "platform-baremetal")]
            MutexGuardInner::Baremetal(g) => g.deref_mut(),
        }
    }
}

// ===========================================================================
// RwLock
// ===========================================================================

/// A platform-abstracted reader-writer lock.
pub struct RwLock<T: ?Sized> {
    #[cfg(feature = "platform-linux")]
    inner: std::sync::RwLock<T>,
    #[cfg(feature = "platform-baremetal")]
    inner: spin::RwLock<T>,
}

unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            #[cfg(feature = "platform-linux")]
            inner: std::sync::RwLock::new(value),
            #[cfg(feature = "platform-baremetal")]
            inner: spin::RwLock::new(value),
        }
    }
}

impl<T: ?Sized> RwLock<T> {
    /// Acquires a shared read lock.
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        #[cfg(feature = "platform-linux")]
        {
            RwLockReadGuard {
                inner: RwLockReadGuardInner::Linux(self.inner.read().unwrap()),
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            RwLockReadGuard {
                inner: RwLockReadGuardInner::Baremetal(self.inner.read()),
            }
        }
    }

    /// Acquires an exclusive write lock.
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        #[cfg(feature = "platform-linux")]
        {
            RwLockWriteGuard {
                inner: RwLockWriteGuardInner::Linux(self.inner.write().unwrap()),
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            RwLockWriteGuard {
                inner: RwLockWriteGuardInner::Baremetal(self.inner.write()),
            }
        }
    }

    /// Attempts to acquire a shared read lock without blocking.
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        #[cfg(feature = "platform-linux")]
        {
            self.inner.try_read().ok().map(|g| RwLockReadGuard {
                inner: RwLockReadGuardInner::Linux(g),
            })
        }
        #[cfg(feature = "platform-baremetal")]
        {
            self.inner.try_read().map(|g| RwLockReadGuard {
                inner: RwLockReadGuardInner::Baremetal(g),
            })
        }
    }

    /// Attempts to acquire an exclusive write lock without blocking.
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        #[cfg(feature = "platform-linux")]
        {
            self.inner.try_write().ok().map(|g| RwLockWriteGuard {
                inner: RwLockWriteGuardInner::Linux(g),
            })
        }
        #[cfg(feature = "platform-baremetal")]
        {
            self.inner.try_write().map(|g| RwLockWriteGuard {
                inner: RwLockWriteGuardInner::Baremetal(g),
            })
        }
    }
}

/// RAII guard for shared read access.
pub struct RwLockReadGuard<'a, T: ?Sized> {
    inner: RwLockReadGuardInner<'a, T>,
}

enum RwLockReadGuardInner<'a, T: ?Sized> {
    #[cfg(feature = "platform-linux")]
    Linux(std::sync::RwLockReadGuard<'a, T>),
    #[cfg(feature = "platform-baremetal")]
    Baremetal(spin::RwLockReadGuard<'a, T>),
}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.inner {
            #[cfg(feature = "platform-linux")]
            RwLockReadGuardInner::Linux(g) => g.deref(),
            #[cfg(feature = "platform-baremetal")]
            RwLockReadGuardInner::Baremetal(g) => g.deref(),
        }
    }
}

/// RAII guard for exclusive write access.
pub struct RwLockWriteGuard<'a, T: ?Sized> {
    inner: RwLockWriteGuardInner<'a, T>,
}

enum RwLockWriteGuardInner<'a, T: ?Sized> {
    #[cfg(feature = "platform-linux")]
    Linux(std::sync::RwLockWriteGuard<'a, T>),
    #[cfg(feature = "platform-baremetal")]
    Baremetal(spin::RwLockWriteGuard<'a, T>),
}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.inner {
            #[cfg(feature = "platform-linux")]
            RwLockWriteGuardInner::Linux(g) => g.deref(),
            #[cfg(feature = "platform-baremetal")]
            RwLockWriteGuardInner::Baremetal(g) => g.deref(),
        }
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match &mut self.inner {
            #[cfg(feature = "platform-linux")]
            RwLockWriteGuardInner::Linux(g) => g.deref_mut(),
            #[cfg(feature = "platform-baremetal")]
            RwLockWriteGuardInner::Baremetal(g) => g.deref_mut(),
        }
    }
}

// ===========================================================================
// Condvar
// ===========================================================================

/// A platform-abstracted condition variable.
///
/// On Linux: delegates to `std::sync::Condvar`.
/// On bare-metal: spin-waits (sufficient for Phase 1; proper wait queues in Phase 6).
pub struct Condvar {
    #[cfg(feature = "platform-linux")]
    inner: std::sync::Condvar,
    #[cfg(feature = "platform-baremetal")]
    _phantom: (),
}

impl Condvar {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "platform-linux")]
            inner: std::sync::Condvar::new(),
            #[cfg(feature = "platform-baremetal")]
            _phantom: (),
        }
    }

    /// Blocks the current thread until notified.
    ///
    /// The mutex guard is released while waiting and re-acquired before returning.
    #[cfg(feature = "platform-linux")]
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        // We need to extract the std guard, wait on it, then rewrap.
        // This is tricky because our MutexGuard wraps the std one.
        // For the Linux backend, we provide a direct std::sync path.
        //
        // In practice, code using Condvar should use the platform Mutex.
        // The wait implementation here is a pragmatic approach.
        let _ = guard;
        // Note: A full implementation would need to unwrap the inner guard,
        // pass it to std::sync::Condvar::wait, and rewrap. This requires
        // unsafe or restructuring. For Phase 1, we provide the API surface
        // and a working notify mechanism. Real condvar wait is Phase 6.
        todo!("Condvar::wait requires inner guard extraction — deferred to Phase 6")
    }

    /// Wakes one waiting thread.
    pub fn notify_one(&self) {
        #[cfg(feature = "platform-linux")]
        self.inner.notify_one();
    }

    /// Wakes all waiting threads.
    pub fn notify_all(&self) {
        #[cfg(feature = "platform-linux")]
        self.inner.notify_all();
    }
}

impl Default for Condvar {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn mutex_basic() {
        let m = Mutex::new(42);
        {
            let mut g = m.lock();
            assert_eq!(*g, 42);
            *g = 100;
        }
        assert_eq!(*m.lock(), 100);
    }

    #[test]
    fn mutex_try_lock() {
        let m = Mutex::new(0);
        let g = m.lock();
        assert!(m.try_lock().is_none());
        drop(g);
        assert!(m.try_lock().is_some());
    }

    #[test]
    fn mutex_threaded() {
        let m = Arc::new(Mutex::new(0u64));
        let mut handles = vec![];

        for _ in 0..10 {
            let m = Arc::clone(&m);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    *m.lock() += 1;
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*m.lock(), 10_000);
    }

    #[test]
    fn rwlock_basic() {
        let rw = RwLock::new(42);
        assert_eq!(*rw.read(), 42);
        *rw.write() = 100;
        assert_eq!(*rw.read(), 100);
    }

    #[test]
    fn rwlock_concurrent_readers() {
        let rw = Arc::new(RwLock::new(42));
        let mut handles = vec![];

        for _ in 0..10 {
            let rw = Arc::clone(&rw);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    let _ = *rw.read();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn condvar_notify() {
        let cv = Condvar::new();
        cv.notify_one();
        cv.notify_all();
    }
}

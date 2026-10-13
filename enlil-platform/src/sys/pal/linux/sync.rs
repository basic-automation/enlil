//! Synchronization bindings — mirrors `std::sys::sync`'s role.
//!
//! The platform layer's [`crate::sync`] primitives are already
//! backend-agnostic (spin-backed, `no_std`-clean), so both PALs bind the
//! same types here; `sys::sync` is the uniform path portable code uses.

pub use crate::sync::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
/// One-time initialization primitive.
pub use spin::Once;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutex_roundtrip() {
        let m = Mutex::new(0u32);
        *m.lock() += 1;
        assert_eq!(*m.lock(), 1);
        assert!(m.try_lock().is_some());
    }

    #[test]
    fn rwlock_roundtrip() {
        let r = RwLock::new(7u32);
        assert_eq!(*r.read(), 7);
        *r.write() = 8;
        assert_eq!(*r.read(), 8);
    }

    #[test]
    fn once_runs_once() {
        let o = Once::new();
        let mut n = 0;
        o.call_once(|| n += 1);
        o.call_once(|| n += 1);
        assert_eq!(n, 1);
    }
}

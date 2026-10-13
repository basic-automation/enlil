//! Synchronization bindings for the bare-metal kernel.
//!
//! Same types as the Linux PAL: the platform layer's [`crate::sync`]
//! primitives are backend-agnostic (spin-backed, `no_std`-clean), so
//! `sys::sync` is the uniform path portable code uses on both backends.

pub use crate::sync::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
/// One-time initialization primitive.
pub use spin::Once;

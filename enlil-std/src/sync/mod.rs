//! Synchronization primitives — `std::sync` compatible API backed by enlil-platform.
//!
//! Routes through `enlil_platform::sys::sync` (the `std::sys`-equivalent
//! bindings) so the hosted and bare-metal backends share one path.
//! Also provides Arc and atomic types: from `std` on the hosted backend,
//! from `alloc`/`core` (these are compiler intrinsics, not OS-dependent)
//! on the bare-metal backend.

// Platform sync primitives, via the sys bindings.
pub use enlil_platform::sys::sync::{
    Condvar, Mutex, MutexGuard, Once, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

// Channels
pub use enlil_platform::sync::{
    Receiver, RecvError, SendError, Sender, TryRecvError, TrySendError, channel,
};

#[cfg(feature = "platform-linux")]
pub use std::sync::atomic;
#[cfg(feature = "platform-linux")]
pub use std::sync::{Arc, Weak};

#[cfg(feature = "platform-baremetal")]
pub use alloc::sync::{Arc, Weak};
#[cfg(feature = "platform-baremetal")]
pub use core::sync::atomic;

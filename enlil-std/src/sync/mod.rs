//! Synchronization primitives — `std::sync` compatible API backed by enlil-platform.
//!
//! Re-exports platform Mutex, `RwLock`, Condvar, and mpsc channels.
//! Also provides Arc and atomic types: from `std` on the hosted backend,
//! from `alloc`/`core` (these are compiler intrinsics, not OS-dependent)
//! on the bare-metal backend.

// Platform sync primitives
pub use enlil_platform::sync::{
    Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
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

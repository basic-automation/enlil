//! Synchronization primitives — `std::sync` compatible API backed by enlil-platform.
//!
//! Re-exports platform Mutex, `RwLock`, Condvar, and mpsc channels.
//! Also provides Arc and atomic types from std (these are compiler intrinsics,
//! not OS-dependent).

// Platform sync primitives
pub use enlil_platform::sync::{
    Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

// Channels
pub use enlil_platform::sync::{
    Receiver, RecvError, SendError, Sender, TryRecvError, TrySendError, channel,
};

// These are architecture intrinsics, not OS-dependent — safe to re-export from std
pub use std::sync::atomic;
pub use std::sync::{Arc, Weak};

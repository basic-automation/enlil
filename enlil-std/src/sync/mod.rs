//! Synchronization primitives — `std::sync` compatible API backed by enlil-platform.
//!
//! Re-exports platform Mutex, `RwLock`, Condvar, and mpsc channels.
//! Also provides Arc and atomic types from std (these are compiler intrinsics,
//! not OS-dependent).

// Platform sync primitives
pub use enlil_platform::sync::{
    Mutex, MutexGuard,
    RwLock, RwLockReadGuard, RwLockWriteGuard,
    Condvar,
};

// Channels
pub use enlil_platform::sync::{
    channel, Sender, Receiver,
    SendError, TrySendError, RecvError, TryRecvError,
};

// These are architecture intrinsics, not OS-dependent — safe to re-export from std
pub use std::sync::{Arc, Weak};
pub use std::sync::atomic;

//! Platform abstraction layer backends — mirrors `std::sys::pal`.
//!
//! * [`linux`] — hosted development backend: real OS threads, files,
//!   sockets, backed by `std`/`libc`.
//! * [`enlil`] — bare-metal Enlil kernel backend: scheduler tasks,
//!   spin locks, TSC clock, RDRAND; OS services the kernel does not
//!   provide yet report `Unsupported`.
//!
//! The `platform-baremetal` feature wins when both backend features are
//! enabled (they are meant to be exclusive; the crate already treats them
//! that way for `no_std` selection).

/// Bare-metal Enlil kernel backend.
#[cfg(feature = "platform-baremetal")]
pub mod enlil;
/// Hosted Linux backend.
#[cfg(all(feature = "platform-linux", not(feature = "platform-baremetal")))]
pub mod linux;

// Re-export the active backend's area modules at `pal::{thread, …}` so
// portable code can write `sys::thread::…` without naming the backend.
#[cfg(feature = "platform-baremetal")]
pub use enlil::{env, fs, io, net, os, process, random, sync, thread, time};
#[cfg(all(feature = "platform-linux", not(feature = "platform-baremetal")))]
pub use linux::{env, fs, io, net, os, process, random, sync, thread, time};

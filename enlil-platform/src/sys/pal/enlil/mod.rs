//! Enlil PAL: `std::sys`-style bindings for the bare-metal kernel.
//!
//! Same area-module signatures as [`super::linux`], backed by the
//! hypervisor's own primitives (scheduler tasks, spin locks, TSC clock,
//! RDRAND, serial console). OS services the kernel does not provide yet —
//! files, sockets, stdin, wall clock — exist with real signatures but
//! report [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported),
//! the same contract as `std`'s `unsupported` PAL.

pub mod env;
pub mod fs;
pub mod io;
pub mod net;
pub mod os;
pub mod process;
pub mod random;
pub mod sync;
pub mod thread;
pub mod time;

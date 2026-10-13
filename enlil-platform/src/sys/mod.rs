//! `std::sys`-equivalent platform bindings for Enlil.
//!
//! In Rust's standard library, `std::sys` is the seam between the portable
//! API surface and the per-OS implementations (`std::sys::pal::unix`,
//! `std::sys::pal::windows`, …). This module is Enlil's version of that
//! seam: the low-level platform primitives that Enlil's `std`-compatible
//! layer (`enlil-std`) and the rest of the hypervisor build on.
//!
//! # Layout
//!
//! * [`pal`] — the platform abstraction layer, mirroring `std::sys::pal`:
//!   one backend per OS ([`pal::linux`] for the hosted development backend,
//!   [`pal::enlil`] for the bare-metal kernel). Every backend exposes the
//!   same area modules (`thread`, `sync`, `time`, `io`, `fs`, `net`, `env`,
//!   `process`, `random`, `os`).
//! * The area modules are re-exported here, so portable code writes
//!   `enlil_platform::sys::thread::spawn` and gets the active backend's
//!   implementation — the same way `std` code goes through `std::sys`.
//!
//! # Backends
//!
//! The backend is selected by the crate features `platform-linux` /
//! `platform-baremetal` (`platform-baremetal` wins if both are somehow
//! enabled). Everything here is `core` + `alloc` only, so the whole module
//! cross-compiles for `x86_64-unknown-enlil`.
//!
//! Where the bare-metal kernel has no equivalent of an OS service yet
//! (files, sockets, stdin), the binding still exists with the real
//! signature but reports
//! [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported) — the
//! same contract as `std`'s `unsupported` PAL, which is what `rustc`'s own
//! `std` falls back to for `os = "none"` targets.

pub mod pal;

pub use pal::{env, fs, io, net, os, process, random, sync, thread, time};

#[cfg(feature = "platform-baremetal")]
use crate::io::{IoError, IoErrorKind, IoResult};

/// Position for [`fs::File::seek`](fs::File::seek). Mirrors `std::io::SeekFrom`.
///
/// Lives here (rather than duplicated per backend) so the `fs` API is
/// identical no matter which PAL is active — mirroring how
/// `std::sys_common` backs several PALs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekFrom {
    /// Absolute offset from the start of the stream.
    Start(u64),
    /// Offset relative to the end of the stream.
    End(i64),
    /// Offset relative to the current position.
    Current(i64),
}

/// Error for [`env::var`](env::var). Mirrors `std::env::VarError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarError {
    /// The variable was not present in the environment.
    NotPresent,
    /// The variable was present but not valid Unicode.
    NotUnicode,
}

impl core::fmt::Display for VarError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotPresent => write!(f, "environment variable not found"),
            Self::NotUnicode => write!(f, "environment variable was not valid Unicode"),
        }
    }
}

impl core::error::Error for VarError {}

/// Builds the standard "this backend has no such service" error.
///
/// Used by the bare-metal PAL for OS services the kernel does not provide
/// yet (filesystem, sockets, stdin, wall clock).
#[cfg(feature = "platform-baremetal")]
pub(crate) const fn unsupported<T>(what: &'static str) -> IoResult<T> {
    Err(IoError::with_message(IoErrorKind::Unsupported, what))
}

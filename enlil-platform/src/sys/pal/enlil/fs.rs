//! Filesystem bindings for the bare-metal kernel: signatures only.
//!
//! Enlil has no filesystem driver yet, so every operation reports
//! [`Unsupported`](crate::io::IoErrorKind::Unsupported). The types exist
//! with real signatures so portable code compiles and can probe for
//! support at runtime.

use crate::io::IoResult;
/// Re-exported so `sys::fs::SeekFrom` names the same type on every backend.
pub use crate::sys::SeekFrom;
use crate::sys::unsupported;

/// File open options, stored as a bitmask. Builds fine;
/// [`open`](OpenOptions::open) always fails.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenOptions {
    flags: u8,
}

impl OpenOptions {
    const READ: u8 = 1 << 0;
    const WRITE: u8 = 1 << 1;
    const APPEND: u8 = 1 << 2;
    const TRUNCATE: u8 = 1 << 3;
    const CREATE: u8 = 1 << 4;
    const CREATE_NEW: u8 = 1 << 5;

    /// Empty option set.
    #[must_use]
    pub const fn new() -> Self {
        Self { flags: 0 }
    }

    const fn set_flag(mut self, flag: u8, on: bool) -> Self {
        if on {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
        self
    }

    /// Enable/disable read access.
    #[must_use]
    pub const fn read(self, read: bool) -> Self {
        self.set_flag(Self::READ, read)
    }

    /// Enable/disable write access.
    #[must_use]
    pub const fn write(self, write: bool) -> Self {
        self.set_flag(Self::WRITE, write)
    }

    /// Enable/disable append mode.
    #[must_use]
    pub const fn append(self, append: bool) -> Self {
        self.set_flag(Self::APPEND, append)
    }

    /// Truncate a pre-existing file on open.
    #[must_use]
    pub const fn truncate(self, truncate: bool) -> Self {
        self.set_flag(Self::TRUNCATE, truncate)
    }

    /// Create the file if it does not exist.
    #[must_use]
    pub const fn create(self, create: bool) -> Self {
        self.set_flag(Self::CREATE, create)
    }

    /// Fail if the file already exists.
    #[must_use]
    pub const fn create_new(self, create_new: bool) -> Self {
        self.set_flag(Self::CREATE_NEW, create_new)
    }

    /// Always fails: no filesystem on bare-metal Enlil.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn open(&self, _path: &str) -> IoResult<File> {
        unsupported("no filesystem on bare-metal enlil")
    }
}

/// An open file. Uninhabited in practice: [`File::open`] never succeeds.
#[derive(Debug)]
pub struct File {
    _private: (),
}

impl File {
    /// Always fails — see the module docs.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn open(_path: &str) -> IoResult<Self> {
        unsupported("no filesystem on bare-metal enlil")
    }

    /// Always fails — see the module docs.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn create(_path: &str) -> IoResult<Self> {
        unsupported("no filesystem on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `open` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn read(&mut self, _buf: &mut [u8]) -> IoResult<usize> {
        unsupported("no filesystem on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `open` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn write(&mut self, _buf: &[u8]) -> IoResult<usize> {
        unsupported("no filesystem on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `open` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn flush(&mut self) -> IoResult<()> {
        unsupported("no filesystem on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `open` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn seek(&mut self, _pos: SeekFrom) -> IoResult<u64> {
        unsupported("no filesystem on bare-metal enlil")
    }
}

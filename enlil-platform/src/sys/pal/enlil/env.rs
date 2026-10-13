//! Environment bindings for the bare-metal kernel.
//!
//! The kernel is not started with an argv/environment block, so [`args`]
//! is empty and [`var`] always reports [`VarError::NotPresent`]. (A future
//! boot-config handoff can seed `args`.)

use alloc::string::String;
use core::iter;

/// Re-exported so `sys::env::VarError` names the same type on every backend.
pub use crate::sys::VarError;

/// Fetches the environment variable `key`.
///
/// # Errors
///
/// Always returns [`VarError::NotPresent`]: the kernel has no environment
/// block.
pub const fn var(_key: &str) -> Result<String, VarError> {
    Err(VarError::NotPresent)
}

/// Kernel argument iterator. Empty today; mirrors `std::env::Args`.
#[derive(Debug, Clone)]
pub struct Args {
    inner: iter::Empty<String>,
}

/// Returns the (empty) kernel argument iterator.
#[must_use]
pub const fn args() -> Args {
    Args {
        inner: iter::empty(),
    }
}

impl Iterator for Args {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(0))
    }
}

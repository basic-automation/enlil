//! Time bindings for the bare-metal kernel.

use core::time::Duration;

use crate::io::IoResult;
use crate::sys::unsupported;

/// Monotonic clock reading: the calibrated TSC.
pub use crate::time::{Instant, sleep};

/// Wall-clock time.
///
/// The kernel has no wall-clock source wired up yet (no RTC/HPET driver in
/// the platform layer), so only [`UNIX_EPOCH`](SystemTime::UNIX_EPOCH)
/// exists as a marker; [`now`](SystemTime::now) reports
/// [`Unsupported`](IoErrorKind::Unsupported).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemTime {
    _private: (),
}

impl SystemTime {
    /// 1970-01-01 00:00:00 UTC marker.
    pub const UNIX_EPOCH: Self = Self { _private: () };

    /// Current wall-clock time — always
    /// [`Unsupported`](IoErrorKind::Unsupported) on bare metal.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`].
    pub const fn now() -> IoResult<Self> {
        unsupported("no wall clock on bare-metal enlil")
    }

    /// Duration from `earlier` to `self` — always
    /// [`Unsupported`](IoErrorKind::Unsupported) (see [`now`](Self::now)).
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`].
    pub const fn duration_since(&self, _earlier: Self) -> IoResult<Duration> {
        unsupported("no wall clock on bare-metal enlil")
    }
}

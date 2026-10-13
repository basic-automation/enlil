//! Time bindings — mirrors `std::sys::time`'s role.

use core::time::Duration;

use crate::io::{IoError, IoErrorKind, IoResult};

/// Monotonic clock reading. Already backend-agnostic: TSC on bare metal,
/// `std::time::Instant` on Linux.
pub use crate::time::{Instant, sleep};

/// Wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SystemTime {
    inner: std::time::SystemTime,
}

impl SystemTime {
    /// 1970-01-01 00:00:00 UTC.
    pub const UNIX_EPOCH: Self = Self {
        inner: std::time::UNIX_EPOCH,
    };

    /// Current wall-clock time.
    ///
    /// # Errors
    ///
    /// This backend never fails; the `IoResult` is for API parity with the
    /// bare-metal PAL, where the kernel has no wall clock.
    pub fn now() -> IoResult<Self> {
        Ok(Self {
            inner: std::time::SystemTime::now(),
        })
    }

    /// Duration from `earlier` to `self`.
    ///
    /// # Errors
    ///
    /// Returns an error if `earlier` is later than `self`.
    pub fn duration_since(&self, earlier: Self) -> IoResult<Duration> {
        self.inner
            .duration_since(earlier.inner)
            .map_err(|_| IoError::new(IoErrorKind::Other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instant_is_monotonic() {
        let a = Instant::now();
        sleep(Duration::from_millis(2));
        assert!(a.elapsed() >= Duration::from_millis(1));
    }

    #[test]
    fn system_time_after_epoch() {
        let t = SystemTime::now().unwrap();
        assert!(t.duration_since(SystemTime::UNIX_EPOCH).unwrap() > Duration::ZERO);
    }

    #[test]
    fn duration_since_rejects_reversed() {
        let t = SystemTime::now().unwrap();
        assert!(SystemTime::UNIX_EPOCH.duration_since(t).is_err());
    }
}

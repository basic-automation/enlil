//! Redox `TimeSpec`-derived time types.
//!
//! Ports the timestamp type from Redox OS's kernel `time` module (and
//! relibc's `timespec`): a seconds-plus-nanoseconds [`TimeSpec`] with a
//! normalization invariant (`0 <= tv_nsec < NANOS_PER_SEC`), checked
//! conversions to and from [`Duration`](core::time::Duration), and the two
//! clocks Redox exposes — [`monotonic`] (ticks since a boot-time epoch) and
//! [`realtime`] (wall clock where the backend has one).

use core::fmt;
use core::ops::{Add, Sub};
use core::time::Duration;

#[cfg(feature = "platform-baremetal")]
use spin::Once;

#[cfg(feature = "platform-linux")]
use std::sync::OnceLock;

use crate::time::Instant;

/// Nanoseconds per second; the modulus of [`TimeSpec`]'s sub-second field.
pub const NANOS_PER_SEC: i64 = 1_000_000_000;

/// A POSIX-style seconds/nanoseconds timestamp.
///
/// Invariant: `0 <= tv_nsec < NANOS_PER_SEC`. [`TimeSpec::new`] normalizes any
/// input into this form, so arithmetic never has to re-normalize.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimeSpec {
    /// Whole seconds; may be negative for times before the epoch.
    pub tv_sec: i64,
    /// Sub-second nanoseconds, always in `0 .. NANOS_PER_SEC`.
    pub tv_nsec: i64,
}

impl TimeSpec {
    /// The zero timestamp.
    pub const ZERO: Self = Self {
        tv_sec: 0,
        tv_nsec: 0,
    };

    /// Build a timestamp, normalizing `tv_nsec` into `0 .. NANOS_PER_SEC`
    /// (carrying overflow into `tv_sec`, borrowing for negatives).
    #[must_use]
    pub const fn new(secs: i64, nanos: i64) -> Self {
        let carry = nanos / NANOS_PER_SEC;
        let mut sec = secs + carry;
        let mut nsec = nanos % NANOS_PER_SEC;
        if nsec < 0 {
            sec -= 1;
            nsec += NANOS_PER_SEC;
        }
        Self {
            tv_sec: sec,
            tv_nsec: nsec,
        }
    }

    /// A timestamp `nanos` nanoseconds after the epoch (may be negative).
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Self {
        Self::new(0, nanos)
    }

    /// Total nanoseconds as a wide integer.
    #[must_use]
    pub fn as_nanos(self) -> i128 {
        i128::from(self.tv_sec) * i128::from(NANOS_PER_SEC) + i128::from(self.tv_nsec)
    }

    /// Whether this is the zero timestamp.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.tv_sec == 0 && self.tv_nsec == 0
    }

    /// Convert a [`Duration`]; saturates at `i64::MAX` seconds.
    #[must_use]
    pub fn from_duration(duration: Duration) -> Self {
        let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
        Self::new(secs, i64::from(duration.subsec_nanos()))
    }

    /// Convert to a [`Duration`]; negative timestamps saturate to zero.
    #[must_use]
    pub fn to_duration(self) -> Duration {
        if self.tv_sec < 0 {
            return Duration::ZERO;
        }
        // `tv_sec >= 0` and `tv_nsec` is normalized: both conversions hold.
        let secs = u64::try_from(self.tv_sec).unwrap_or(0);
        let nanos = u32::try_from(self.tv_nsec).unwrap_or(0);
        Duration::new(secs, nanos)
    }

    /// Absolute distance between two timestamps.
    #[must_use]
    pub fn abs_diff(self, other: Self) -> Duration {
        let (earlier, later) = if self <= other {
            (self, other)
        } else {
            (other, self)
        };
        (later - earlier).to_duration()
    }
}

impl Add for TimeSpec {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self::new(self.tv_sec + rhs.tv_sec, self.tv_nsec + rhs.tv_nsec)
    }
}

impl Sub for TimeSpec {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self::new(self.tv_sec - rhs.tv_sec, self.tv_nsec - rhs.tv_nsec)
    }
}

impl Add<Duration> for TimeSpec {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self {
        self + Self::from_duration(rhs)
    }
}

impl fmt::Display for TimeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:09}", self.tv_sec, self.tv_nsec)
    }
}

#[cfg(feature = "platform-linux")]
static MONO_EPOCH: OnceLock<Instant> = OnceLock::new();
#[cfg(feature = "platform-baremetal")]
static MONO_EPOCH: Once<Instant> = Once::new();

/// Monotonic time since an unspecified boot-time epoch.
///
/// Never goes backwards; the epoch is fixed at first call. Backed by the
/// platform [`Instant`](crate::time::Instant) (TSC on bare metal).
#[must_use]
pub fn monotonic() -> TimeSpec {
    #[cfg(feature = "platform-linux")]
    let epoch = MONO_EPOCH.get_or_init(Instant::now);
    #[cfg(feature = "platform-baremetal")]
    let epoch = MONO_EPOCH.call_once(Instant::now);
    #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
    let epoch = &Instant::now();
    TimeSpec::from_duration(Instant::now().duration_since(epoch))
}

/// Wall-clock time, when the backend has a clock source.
///
/// - Linux: `clock_gettime(CLOCK_REALTIME)`; `None` if the call fails.
/// - Bare metal: `None` — no RTC driver is wired up yet (roadmap Phase 6).
// Not `const`: the Linux path calls `clock_gettime` (clippy only suggests
// `const` under the bare-metal cfg, where the body is just `None`).
#[allow(clippy::missing_const_for_fn)]
#[must_use]
pub fn realtime() -> Option<TimeSpec> {
    #[cfg(all(feature = "platform-linux", target_os = "linux"))]
    {
        let mut raw = core::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: `raw` is a valid out-pointer for `clock_gettime`.
        let ret = unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, raw.as_mut_ptr()) };
        if ret != 0 {
            return None;
        }
        // SAFETY: the call succeeded, so `raw` is initialized.
        let raw = unsafe { raw.assume_init() };
        // `tv_sec`/`tv_nsec` are `i64` on every Linux target this workspace
        // builds for (all x86-64).
        Some(TimeSpec::new(raw.tv_sec, raw.tv_nsec))
    }
    #[cfg(not(all(feature = "platform-linux", target_os = "linux")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn normalization() {
        assert_eq!(TimeSpec::new(1, 2_000_000_000), TimeSpec::new(3, 0));
        assert_eq!(TimeSpec::new(1, -1), TimeSpec::new(0, 999_999_999));
        assert_eq!(
            TimeSpec::new(0, -1_500_000_000),
            TimeSpec::new(-2, 500_000_000)
        );
        assert_eq!(TimeSpec::from_nanos(1_000_000_001), TimeSpec::new(1, 1));
        assert!(TimeSpec::ZERO.is_zero());
    }

    #[test]
    fn arithmetic() {
        let a = TimeSpec::new(10, 700_000_000);
        let b = TimeSpec::new(5, 500_000_000);
        assert_eq!(a + b, TimeSpec::new(16, 200_000_000));
        assert_eq!(a - b, TimeSpec::new(5, 200_000_000));
        assert_eq!(b - a, TimeSpec::new(-6, 800_000_000));
        assert_eq!(
            a + Duration::from_millis(500),
            TimeSpec::new(11, 200_000_000)
        );
        assert_eq!(a.abs_diff(b), Duration::new(5, 200_000_000));
        assert_eq!(b.abs_diff(a), Duration::new(5, 200_000_000));
        assert_eq!(TimeSpec::new(3, 0).as_nanos(), 3_000_000_000);
        assert_eq!(format!("{}", TimeSpec::new(3, 42)), "3.000000042");
    }

    #[test]
    fn duration_conversions() {
        let d = Duration::new(123, 456_789_000);
        let ts = TimeSpec::from_duration(d);
        assert_eq!((ts.tv_sec, ts.tv_nsec), (123, 456_789_000));
        assert_eq!(ts.to_duration(), d);
        // Negative timestamps saturate to zero.
        assert_eq!(TimeSpec::new(-5, 0).to_duration(), Duration::ZERO);
        // Huge durations saturate at i64::MAX seconds.
        let huge = TimeSpec::from_duration(Duration::from_secs(u64::MAX));
        assert_eq!(huge.tv_sec, i64::MAX);
    }

    #[test]
    fn ordering() {
        assert!(TimeSpec::new(1, 0) < TimeSpec::new(2, 0));
        assert!(TimeSpec::new(1, 999_999_999) < TimeSpec::new(2, 0));
        assert_eq!(TimeSpec::new(2, 0), TimeSpec::new(1, 1_000_000_000));
    }

    #[cfg(feature = "platform-linux")]
    #[test]
    fn monotonic_never_goes_backwards() {
        let first = monotonic();
        std::thread::sleep(Duration::from_millis(5));
        let second = monotonic();
        assert!(second >= first);
        assert!(second.abs_diff(first) >= Duration::from_millis(1));
    }

    #[cfg(all(feature = "platform-linux", target_os = "linux"))]
    #[test]
    fn realtime_is_sane() {
        let now = realtime().expect("clock_gettime failed");
        // After 2026-01-01 (1_767_225_600); before 2100 (4_102_444_800).
        assert!(now.tv_sec > 1_767_225_600);
        assert!(now.tv_sec < 4_102_444_800);
        assert!(now.tv_nsec >= 0 && now.tv_nsec < NANOS_PER_SEC);
    }
}

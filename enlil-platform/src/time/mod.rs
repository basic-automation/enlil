//! Time Subsystem — Instant, Duration, and clock sources
//!
//! Provides monotonic time primitives for the Enlil hypervisor.
//!
//! # Backends
//!
//! - **Linux:** Delegates to `std::time` (backed by `clock_gettime`).
//! - **Bare-metal:** TSC-based monotonic clock with HPET/APIC calibration.

use std::time::Duration;

/// A monotonic instant in time.
///
/// On Linux, wraps `std::time::Instant`.
/// On bare-metal, reads from calibrated TSC.
#[derive(Clone, Copy, Debug)]
pub struct Instant {
    #[cfg(feature = "platform-linux")]
    inner: std::time::Instant,
    #[cfg(feature = "platform-baremetal")]
    tsc_value: u64,
}

impl Instant {
    /// Returns the current instant.
    pub fn now() -> Self {
        #[cfg(feature = "platform-linux")]
        {
            Self {
                inner: std::time::Instant::now(),
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            Self {
                tsc_value: read_tsc(),
            }
        }
        #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
        {
            panic!("no platform backend enabled")
        }
    }

    /// Returns the duration elapsed since this instant.
    pub fn elapsed(&self) -> Duration {
        Self::now().duration_since(self)
    }

    /// Returns the duration from `earlier` to `self`.
    pub fn duration_since(&self, earlier: &Instant) -> Duration {
        #[cfg(feature = "platform-linux")]
        {
            self.inner.duration_since(earlier.inner)
        }
        #[cfg(feature = "platform-baremetal")]
        {
            let ticks = self.tsc_value.saturating_sub(earlier.tsc_value);
            tsc_ticks_to_duration(ticks)
        }
        #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
        {
            let _ = earlier;
            Duration::ZERO
        }
    }
}

impl std::ops::Add<Duration> for Instant {
    type Output = Instant;

    fn add(self, dur: Duration) -> Self::Output {
        #[cfg(feature = "platform-linux")]
        {
            Self {
                inner: self.inner + dur,
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            Self {
                tsc_value: self.tsc_value + duration_to_tsc_ticks(dur),
            }
        }
        #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
        {
            let _ = dur;
            self
        }
    }
}

impl std::ops::Sub for Instant {
    type Output = Duration;

    fn sub(self, other: Instant) -> Duration {
        self.duration_since(&other)
    }
}

impl PartialEq for Instant {
    fn eq(&self, other: &Self) -> bool {
        #[cfg(feature = "platform-linux")]
        { self.inner == other.inner }
        #[cfg(feature = "platform-baremetal")]
        { self.tsc_value == other.tsc_value }
        #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
        { let _ = other; true }
    }
}

impl Eq for Instant {}

impl PartialOrd for Instant {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Instant {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        #[cfg(feature = "platform-linux")]
        { self.inner.cmp(&other.inner) }
        #[cfg(feature = "platform-baremetal")]
        { self.tsc_value.cmp(&other.tsc_value) }
        #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
        { let _ = other; std::cmp::Ordering::Equal }
    }
}

// ---------------------------------------------------------------------------
// TSC calibration (bare-metal backend)
// ---------------------------------------------------------------------------

/// TSC frequency in Hz. Calibrated at boot time on bare-metal.
/// On Linux this is unused (we delegate to clock_gettime).
static TSC_FREQ_HZ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Set the TSC frequency (called during bare-metal boot calibration).
pub fn set_tsc_frequency(hz: u64) {
    TSC_FREQ_HZ.store(hz, std::sync::atomic::Ordering::Relaxed);
}

/// Get the calibrated TSC frequency.
pub fn tsc_frequency() -> u64 {
    TSC_FREQ_HZ.load(std::sync::atomic::Ordering::Relaxed)
}

/// Read the TSC register.
#[cfg(feature = "platform-baremetal")]
fn read_tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    { 0 }
}

/// Convert TSC ticks to Duration.
#[cfg(feature = "platform-baremetal")]
fn tsc_ticks_to_duration(ticks: u64) -> Duration {
    let freq = tsc_frequency();
    if freq == 0 {
        return Duration::ZERO;
    }
    let secs = ticks / freq;
    let remaining = ticks % freq;
    let nanos = (remaining as u128 * 1_000_000_000u128 / freq as u128) as u64;
    Duration::new(secs, nanos as u32)
}

/// Convert Duration to TSC ticks.
#[cfg(feature = "platform-baremetal")]
fn duration_to_tsc_ticks(dur: Duration) -> u64 {
    let freq = tsc_frequency();
    if freq == 0 {
        return 0;
    }
    let total_nanos = dur.as_nanos();
    (total_nanos as u64 * freq) / 1_000_000_000
}

/// Sleep for the given duration.
///
/// On Linux: delegates to `std::thread::sleep`.
/// On bare-metal: busy-waits on TSC (will be replaced with timer interrupt sleep).
pub fn sleep(duration: Duration) {
    #[cfg(feature = "platform-linux")]
    {
        std::thread::sleep(duration);
    }
    #[cfg(feature = "platform-baremetal")]
    {
        let start = Instant::now();
        while start.elapsed() < duration {
            core::hint::spin_loop();
        }
    }
}

/// A simple stopwatch for benchmarking.
pub struct Stopwatch {
    start: Instant,
    label: &'static str,
}

impl Stopwatch {
    /// Start a new stopwatch with a label.
    pub fn start(label: &'static str) -> Self {
        Self {
            start: Instant::now(),
            label,
        }
    }

    /// Returns elapsed duration without stopping.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Stop and log the elapsed time.
    pub fn stop(self) -> Duration {
        let elapsed = self.start.elapsed();
        log::debug!("[{}] elapsed: {:?}", self.label, elapsed);
        elapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instant_now_is_monotonic() {
        let a = Instant::now();
        let b = Instant::now();
        assert!(b >= a);
    }

    #[test]
    fn instant_elapsed() {
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(5)); // generous tolerance
    }

    #[test]
    fn instant_duration_since() {
        let a = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        let b = Instant::now();
        let dur = b.duration_since(&a);
        assert!(dur >= Duration::from_millis(5));
    }

    #[test]
    fn instant_add_duration() {
        let a = Instant::now();
        let b = a + Duration::from_secs(1);
        assert!(b > a);
    }

    #[test]
    fn instant_sub() {
        let a = Instant::now();
        std::thread::sleep(Duration::from_millis(10));
        let b = Instant::now();
        let dur = b - a;
        assert!(dur >= Duration::from_millis(5));
    }

    #[test]
    fn sleep_works() {
        let start = Instant::now();
        sleep(Duration::from_millis(50));
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn stopwatch_basic() {
        let sw = Stopwatch::start("test");
        std::thread::sleep(Duration::from_millis(10));
        let elapsed = sw.stop();
        assert!(elapsed >= Duration::from_millis(5));
    }

    #[test]
    fn tsc_frequency_default_zero() {
        // On linux backend, TSC freq is unused but should be settable.
        let old = tsc_frequency();
        set_tsc_frequency(3_000_000_000);
        assert_eq!(tsc_frequency(), 3_000_000_000);
        set_tsc_frequency(old); // restore
    }
}

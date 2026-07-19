//! Time Subsystem — Instant, Duration, and clock sources
//!
//! Provides monotonic time primitives for the Enlil hypervisor.
//!
//! # Backends
//!
//! - **Linux:** Delegates to `std::time` (backed by `clock_gettime`).
//! - **Bare-metal:** TSC-based monotonic clock with HPET/APIC calibration.

use core::time::Duration;

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
    #[must_use]
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
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        Self::now().duration_since(self)
    }

    /// Returns the duration from `earlier` to `self`.
    #[must_use]
    pub fn duration_since(&self, earlier: &Self) -> Duration {
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

impl core::ops::Add<Duration> for Instant {
    type Output = Self;

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

impl core::ops::Sub for Instant {
    type Output = Duration;

    fn sub(self, other: Self) -> Duration {
        self.duration_since(&other)
    }
}

impl PartialEq for Instant {
    fn eq(&self, other: &Self) -> bool {
        #[cfg(feature = "platform-linux")]
        {
            self.inner == other.inner
        }
        #[cfg(feature = "platform-baremetal")]
        {
            self.tsc_value == other.tsc_value
        }
        #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
        {
            let _ = other;
            true
        }
    }
}

impl Eq for Instant {}

impl PartialOrd for Instant {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Instant {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        #[cfg(feature = "platform-linux")]
        {
            self.inner.cmp(&other.inner)
        }
        #[cfg(feature = "platform-baremetal")]
        {
            self.tsc_value.cmp(&other.tsc_value)
        }
        #[cfg(not(any(feature = "platform-linux", feature = "platform-baremetal")))]
        {
            let _ = other;
            core::cmp::Ordering::Equal
        }
    }
}

// ---------------------------------------------------------------------------
// TSC calibration (bare-metal backend)
// ---------------------------------------------------------------------------

/// TSC frequency in Hz. Calibrated at boot time on bare-metal.
/// On Linux this is unused (we delegate to `clock_gettime`).
static TSC_FREQ_HZ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Set the TSC frequency (called during bare-metal boot calibration).
pub fn set_tsc_frequency(hz: u64) {
    TSC_FREQ_HZ.store(hz, core::sync::atomic::Ordering::Relaxed);
}

/// Get the calibrated TSC frequency.
pub fn tsc_frequency() -> u64 {
    TSC_FREQ_HZ.load(core::sync::atomic::Ordering::Relaxed)
}

/// Derive the TSC frequency in Hz from raw CPUID leaf `0x15` / `0x16` values.
///
/// Pure (no `__cpuid`), so it is unit-testable with synthetic leaf values;
/// returns `None` when neither leaf pins the frequency down.
///
/// - **Leaf `0x15`** (TSC / core-crystal info): `ratio_den` = `EAX`,
///   `ratio_num` = `EBX`, `crystal_hz` = `ECX` (crystal frequency in Hz, often
///   0). With the ratio and crystal all present, `TSC = ECX × EBX / EAX`
///   exactly (Intel SDM Vol.3 §18.7.3).
/// - **Leaf `0x16`** (processor frequency): `leaf16_eax[15:0]` = base frequency
///   in MHz. On invariant-TSC parts the TSC runs at the base frequency, so when
///   leaf `0x15` reports the ratio but no crystal (`ECX == 0`) we fall back to
///   `TSC ≈ base_MHz × 1e6` — the same fallback the kernel's
///   `native_calibrate_tsc` uses.
///
/// `max_leaf` is leaf-0 `EAX` (the maximum basic leaf); a leaf is consulted
/// only when `max_leaf` advertises it.
#[must_use]
pub fn tsc_hz_from_cpuid_leaves(
    max_leaf: u32,
    ratio_den: u32,
    ratio_num: u32,
    crystal_hz: u32,
    leaf16_eax: u32,
) -> Option<u64> {
    if max_leaf < 0x15 {
        return None;
    }
    let denominator = u64::from(ratio_den);
    let numerator = u64::from(ratio_num);

    // Exact path: the ratio plus an enumerated crystal frequency.
    if denominator != 0 && numerator != 0 {
        let crystal = u64::from(crystal_hz);
        if crystal != 0 {
            return Some(crystal * numerator / denominator);
        }
    }

    // Crystal absent (or the ratio unusable): fall back to leaf 0x16's base
    // frequency (MHz, low 16 bits), which the TSC tracks on invariant-TSC parts.
    if max_leaf >= 0x16 {
        let base_mhz = u64::from(leaf16_eax & 0xFFFF);
        if base_mhz != 0 {
            return Some(base_mhz * 1_000_000);
        }
    }

    None
}

/// Calibrate the TSC frequency from CPUID.
///
/// Uses leaf `0x15` (TSC / core-crystal info), falling back to leaf `0x16`
/// (processor base frequency) when the crystal is not enumerated — see
/// [`tsc_hz_from_cpuid_leaves`] for the arithmetic. Returns `Some(frequency_hz)`
/// on success, `None` if neither leaf pins the frequency down. Reads the actual
/// CPU on both the Linux and bare-metal backends; architecture-specific
/// (`x86_64` only).
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn calibrate_tsc_from_cpuid() -> Option<u64> {
    let max_leaf = core::arch::x86_64::__cpuid(0x0).eax;
    if max_leaf < 0x15 {
        return None;
    }
    let l15 = core::arch::x86_64::__cpuid(0x15);
    // Only read leaf 0x16 when the CPU advertises it, so we never sample an
    // out-of-range leaf (whose value is vendor-defined and not a frequency).
    let l16_eax = if max_leaf >= 0x16 {
        core::arch::x86_64::__cpuid(0x16).eax
    } else {
        0
    };
    tsc_hz_from_cpuid_leaves(max_leaf, l15.eax, l15.ebx, l15.ecx, l16_eax)
}

/// Attempt to calibrate TSC and store the result.
/// Tries CPUID 0x15 first, falls back to a default estimate.
#[cfg(target_arch = "x86_64")]
pub fn calibrate_tsc() {
    if let Some(freq) = calibrate_tsc_from_cpuid() {
        set_tsc_frequency(freq);
        let ghz_int = freq / 1_000_000_000;
        let ghz_frac = (freq % 1_000_000_000) / 10_000_000;
        log::info!(
            "TSC frequency calibrated via CPUID 0x15: {freq} Hz ({ghz_int}.{ghz_frac:02} GHz)"
        );
    } else {
        log::warn!("CPUID 0x15 TSC calibration not available; TSC frequency must be set manually");
    }
}

/// Read the TSC register.
#[cfg(feature = "platform-baremetal")]
fn read_tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_rdtsc()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
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
    // `remaining < freq`, so the quotient is `< 1_000_000_000` and always fits a
    // `u32`; the `u128` intermediate avoids overflow in `remaining * 1e9`.
    let nanos = u128::from(remaining) * 1_000_000_000u128 / u128::from(freq);
    Duration::new(secs, u32::try_from(nanos).unwrap_or(0))
}

/// Convert Duration to TSC ticks.
#[cfg(feature = "platform-baremetal")]
fn duration_to_tsc_ticks(dur: Duration) -> u64 {
    let freq = tsc_frequency();
    if freq == 0 {
        return 0;
    }
    let total_nanos = dur.as_nanos();
    let ticks = total_nanos * u128::from(freq) / 1_000_000_000u128;
    u64::try_from(ticks).unwrap_or(u64::MAX)
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
    #[must_use]
    pub fn start(label: &'static str) -> Self {
        Self {
            start: Instant::now(),
            label,
        }
    }

    /// Returns elapsed duration without stopping.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Stop and log the elapsed time.
    #[must_use]
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

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn tsc_calibration_from_cpuid() {
        // This may return None on older CPUs or VMs without CPUID 0x15 support.
        // We just verify it doesn't panic.
        let result = calibrate_tsc_from_cpuid();
        if let Some(freq) = result {
            assert!(freq > 0, "TSC frequency should be positive");
            // Sanity: should be between 100 MHz and 10 GHz
            assert!(freq > 100_000_000, "TSC frequency suspiciously low: {freq}");
            assert!(
                freq < 10_000_000_000,
                "TSC frequency suspiciously high: {freq}"
            );
        }
    }

    #[test]
    fn tsc_hz_exact_from_leaf_15_crystal() {
        // Skylake-style: crystal 24 MHz, ratio 250/2 → 3.0 GHz TSC. ECX present
        // takes the exact path regardless of leaf 0x16.
        assert_eq!(
            tsc_hz_from_cpuid_leaves(0x16, 2, 250, 24_000_000, 2600),
            Some(3_000_000_000)
        );
    }

    #[test]
    fn tsc_hz_falls_back_to_leaf_16_base_frequency() {
        // Ratio present but no crystal (ECX == 0): fall back to leaf 0x16's
        // base frequency (2600 MHz → 2.6 GHz). Only the low 16 bits are the MHz.
        assert_eq!(
            tsc_hz_from_cpuid_leaves(0x16, 2, 250, 0, 2600),
            Some(2_600_000_000)
        );
        assert_eq!(
            // 0x0C80 == 3200 MHz in EAX[15:0]; the high bits must be ignored.
            tsc_hz_from_cpuid_leaves(0x16, 2, 250, 0, 0xFFFF_0C80),
            Some(3_200_000_000),
            "only EAX[15:0] is the base-frequency MHz field"
        );
    }

    #[test]
    fn tsc_hz_none_when_no_leaf_pins_it() {
        // Leaf 0x15 unsupported at all.
        assert_eq!(tsc_hz_from_cpuid_leaves(0x14, 2, 250, 0, 2600), None);
        // Leaf 0x15 with no crystal, and leaf 0x16 not advertised.
        assert_eq!(tsc_hz_from_cpuid_leaves(0x15, 2, 250, 0, 2600), None);
        // Leaf 0x15 with no crystal, and leaf 0x16 advertised but zero MHz.
        assert_eq!(tsc_hz_from_cpuid_leaves(0x16, 2, 250, 0, 0), None);
    }

    #[test]
    fn tsc_hz_bad_ratio_still_uses_leaf_16() {
        // A zero numerator/denominator makes the 0x15 ratio unusable, but the
        // leaf 0x16 base frequency still yields an answer.
        assert_eq!(
            tsc_hz_from_cpuid_leaves(0x16, 0, 0, 0, 2600),
            Some(2_600_000_000)
        );
    }
}

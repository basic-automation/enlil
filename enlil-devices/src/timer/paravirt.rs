//! Paravirtualized clock sources.
//!
//! Provides KVM clock (for Linux guests) and Hyper-V reference TSC (for Windows guests).
//! These allow guests to read time without VM exits.

use std::sync::atomic::{AtomicU32, AtomicU64};

// ---------------------------------------------------------------------------
// KVM Clock (kvmclock)
// ---------------------------------------------------------------------------

/// KVM clock page layout (as defined by the KVM paravirt spec).
///
/// The hypervisor maps this page into guest memory. The guest reads
/// `tsc_timestamp`, `system_time`, and `tsc_to_system_mul` to compute
/// wall-clock time without a VM exit.
#[repr(C)]
#[derive(Debug)]
pub struct KvmClockPage {
    /// Version counter — odd means update in progress.
    pub version: AtomicU32,
    _pad0: u32,
    /// TSC value at the time `system_time` was captured.
    pub tsc_timestamp: AtomicU64,
    /// System time in nanoseconds at `tsc_timestamp`.
    pub system_time: AtomicU64,
    /// Multiplier: ns = (`tsc_delta` * `tsc_to_system_mul`) >> `tsc_shift`.
    pub tsc_to_system_mul: AtomicU32,
    /// Shift for the TSC-to-ns conversion.
    pub tsc_shift: i8,
    /// Flags.
    pub flags: u8,
    _pad1: [u8; 2],
}

/// Manager for the KVM clock paravirt interface.
pub struct KvmClock {
    /// Guest physical address of the clock page.
    page_gpa: u64,
    /// Current version counter.
    version: u32,
    /// TSC frequency in Hz.
    tsc_freq_hz: u64,
}

impl KvmClock {
    /// Create a new KVM clock manager.
    ///
    /// # Arguments
    ///
    /// * `page_gpa` - guest physical address where the clock page is mapped.
    /// * `tsc_freq_hz` - the TSC frequency for this vCPU.
    #[must_use]
    pub const fn new(page_gpa: u64, tsc_freq_hz: u64) -> Self {
        Self {
            page_gpa,
            version: 0,
            tsc_freq_hz,
        }
    }

    /// Get the guest physical address of the clock page.
    #[must_use]
    pub const fn page_gpa(&self) -> u64 {
        self.page_gpa
    }

    /// Compute the TSC-to-nanoseconds multiplier and shift.
    ///
    /// Returns `(multiplier, shift)` such that:
    ///   ns = (`tsc_delta` * multiplier) >> shift
    #[must_use]
    pub fn compute_mul_shift(&self) -> (u32, i8) {
        if self.tsc_freq_hz == 0 {
            return (0, 0);
        }
        // We want: mul / 2^shift = 10^9 / tsc_freq
        // Choose shift = 32 for good precision
        let shift: i8 = 32;
        let mul = ((1_000_000_000u128) << u32::from(shift.unsigned_abs())) / u128::from(self.tsc_freq_hz);
        #[allow(clippy::cast_possible_truncation)]
        {
            (mul as u32, shift)
        }
    }

    /// Update the clock page data. Call this on each VM entry or periodically.
    ///
    /// # Arguments
    ///
    /// * `current_tsc` - the current TSC value.
    /// * `system_time_ns` - the current system time in nanoseconds.
    ///
    /// # Returns
    ///
    /// The serialized clock page bytes (64 bytes).
    #[must_use]
    pub fn update(&mut self, current_tsc: u64, system_time_ns: u64) -> Vec<u8> {
        self.version += 2; // Always even after update
        let (mul, shift) = self.compute_mul_shift();

        let mut page = vec![0u8; 64];
        // version (offset 0, 4 bytes)
        page[0..4].copy_from_slice(&self.version.to_le_bytes());
        // tsc_timestamp (offset 8, 8 bytes)
        page[8..16].copy_from_slice(&current_tsc.to_le_bytes());
        // system_time (offset 16, 8 bytes)
        page[16..24].copy_from_slice(&system_time_ns.to_le_bytes());
        // tsc_to_system_mul (offset 24, 4 bytes)
        page[24..28].copy_from_slice(&mul.to_le_bytes());
        // tsc_shift (offset 28, 1 byte)
        #[allow(clippy::cast_possible_truncation)]
        {
            page[28] = shift.unsigned_abs();
        }
        // flags (offset 29, 1 byte) — TSC is stable
        page[29] = 0x01;
        page
    }

    /// Get the current version counter.
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }
}

// ---------------------------------------------------------------------------
// Hyper-V Reference TSC
// ---------------------------------------------------------------------------

/// Hyper-V Reference TSC page layout.
///
/// Windows guests use this for high-resolution timekeeping.
/// The guest reads the page and computes: time = (`rdtsc()` * `scale`) >> 64 + `offset`.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct HyperVReferenceTscPage {
    /// Sequence counter — 0 means invalid/use fallback.
    pub sequence: u32,
    _reserved0: u32,
    /// Scale factor: `reference_time` = (`tsc` * `scale`) >> 64.
    pub scale: u64,
    /// Offset added after scaling.
    pub offset: i64,
}

/// Manager for the Hyper-V reference TSC interface.
pub struct HyperVReferenceTsc {
    /// Guest physical address of the TSC page.
    page_gpa: u64,
    /// Current sequence number.
    sequence: u32,
    /// TSC frequency in Hz.
    tsc_freq_hz: u64,
    /// Reference frequency (Hyper-V uses 10 MHz = 100ns units).
    ref_freq_hz: u64,
}

impl HyperVReferenceTsc {
    /// Create a new Hyper-V reference TSC manager.
    ///
    /// # Arguments
    ///
    /// * `page_gpa` - guest physical address of the TSC page.
    /// * `tsc_freq_hz` - the TSC frequency in Hz.
    #[must_use]
    pub const fn new(page_gpa: u64, tsc_freq_hz: u64) -> Self {
        Self {
            page_gpa,
            sequence: 0,
            tsc_freq_hz,
            ref_freq_hz: 10_000_000, // 10 MHz (100ns units)
        }
    }

    /// Get the guest physical address.
    #[must_use]
    pub const fn page_gpa(&self) -> u64 {
        self.page_gpa
    }

    /// Compute the scale factor.
    ///
    /// scale = (`ref_freq` * 2^64) / `tsc_freq`
    #[must_use]
    #[allow(clippy::cast_lossless)]
    pub const fn compute_scale(&self) -> u64 {
        if self.tsc_freq_hz == 0 {
            return 0;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            (((self.ref_freq_hz as u128) << 64) / (self.tsc_freq_hz as u128)) as u64
        }
    }

    /// Update the reference TSC page.
    ///
    /// # Arguments
    ///
    /// * `tsc_offset` - the TSC offset for this vCPU.
    ///
    /// # Returns
    ///
    /// The serialized page bytes (32 bytes).
    #[must_use]
    pub fn update(&mut self, tsc_offset: i64) -> Vec<u8> {
        self.sequence += 1;
        let scale = self.compute_scale();

        let mut page = vec![0u8; 32];
        // sequence (offset 0, 4 bytes)
        page[0..4].copy_from_slice(&self.sequence.to_le_bytes());
        // reserved (offset 4, 4 bytes) — zero
        // scale (offset 8, 8 bytes)
        page[8..16].copy_from_slice(&scale.to_le_bytes());
        // offset (offset 16, 8 bytes)
        page[16..24].copy_from_slice(&tsc_offset.to_le_bytes());
        page
    }

    /// Invalidate the page (guest falls back to MSR-based time).
    #[must_use]
    pub fn invalidate(&mut self) -> Vec<u8> {
        let mut page = vec![0u8; 32];
        // sequence = 0 means invalid
        page[0..4].copy_from_slice(&0u32.to_le_bytes());
        page
    }

    /// Get the current sequence number.
    #[must_use]
    pub const fn sequence(&self) -> u32 {
        self.sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kvm_clock_mul_shift() {
        // 3 GHz TSC
        let clock = KvmClock::new(0x1000, 3_000_000_000);
        let (mul, shift) = clock.compute_mul_shift();
        assert!(mul > 0);
        assert_eq!(shift, 32);

        // Verify: 3 billion ticks * mul >> 32 should ≈ 1 second (10^9 ns)
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        {
            let ns = ((3_000_000_000u128 * u128::from(mul)) >> 32) as u64;
            // Allow 1% error
            assert!((ns as i64 - 1_000_000_000i64).unsigned_abs() < 10_000_000);
        }
    }

    #[test]
    fn kvm_clock_update() {
        let mut clock = KvmClock::new(0x1000, 3_000_000_000);
        let page = clock.update(1000, 500_000);

        // Version should be 2 after first update
        let version = u32::from_le_bytes([page[0], page[1], page[2], page[3]]);
        assert_eq!(version, 2);

        // TSC timestamp
        let tsc = u64::from_le_bytes(page[8..16].try_into().unwrap());
        assert_eq!(tsc, 1000);

        // System time
        let sys = u64::from_le_bytes(page[16..24].try_into().unwrap());
        assert_eq!(sys, 500_000);
    }

    #[test]
    fn kvm_clock_version_increments() {
        let mut clock = KvmClock::new(0x1000, 3_000_000_000);
        let _ = clock.update(0, 0);
        assert_eq!(clock.version(), 2);
        let _ = clock.update(0, 0);
        assert_eq!(clock.version(), 4);
    }

    #[test]
    fn hyperv_tsc_scale() {
        let tsc = HyperVReferenceTsc::new(0x2000, 3_000_000_000);
        let scale = tsc.compute_scale();
        assert!(scale > 0);
    }

    #[test]
    fn hyperv_tsc_update() {
        let mut tsc = HyperVReferenceTsc::new(0x2000, 3_000_000_000);
        let page = tsc.update(0);
        let seq = u32::from_le_bytes([page[0], page[1], page[2], page[3]]);
        assert_eq!(seq, 1);
    }

    #[test]
    fn hyperv_tsc_invalidate() {
        let mut tsc = HyperVReferenceTsc::new(0x2000, 3_000_000_000);
        let _ = tsc.update(0); // seq = 1
        let page = tsc.invalidate();
        let seq = u32::from_le_bytes([page[0], page[1], page[2], page[3]]);
        assert_eq!(seq, 0); // invalid
    }
}

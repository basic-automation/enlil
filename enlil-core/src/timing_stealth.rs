//! Timing Stealth — Hide VM exit overhead
//!
//! Implements APERF/MPERF shadow counters and TSC offsetting to defeat IET
//! divergence detection and timing-based VM detectors. (LBR sanitization lives
//! in the canonical `enlil_devices::stealth::lbr` — see the note below.)

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

/// Per-vCPU timing state for stealth
pub struct VcpuTimingState {
    /// Shadow APERF counter (actual performance counter)
    pub aperf: AtomicU64,
    /// Shadow MPERF counter (reference max frequency counter)
    pub mperf: AtomicU64,
    /// Cumulative VMEXIT time on this vCPU (nanoseconds)
    pub total_exit_time_ns: AtomicU64,
    /// Guest-visible TSC offset (applied during VMRUN)
    pub tsc_offset: AtomicI64,
    /// TSC value when last VMEXIT occurred
    pub last_exit_tsc: AtomicU64,
    /// Last guest instruction pointer before VMEXIT (for LBR sanitization)
    pub last_guest_rip: AtomicU64,
}

impl VcpuTimingState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            aperf: AtomicU64::new(0),
            mperf: AtomicU64::new(0),
            total_exit_time_ns: AtomicU64::new(0),
            tsc_offset: AtomicI64::new(0),
            last_exit_tsc: AtomicU64::new(0),
            last_guest_rip: AtomicU64::new(0),
        })
    }

    /// Called on VMEXIT: measure how long we spent outside the guest
    pub fn on_vmexit(&self, exit_tsc: u64) {
        self.last_exit_tsc.store(exit_tsc, Ordering::Release);
    }

    /// Called before VMRESUME: account for exit time and adjust shadow counters.
    ///
    /// The VMEXIT must be hidden from *both* APERF and MPERF so neither shadow counter
    /// advances across the exit, and the decrement must be proportional so the
    /// APERF/MPERF ratio — the frequency/utilization signal an IET detector inspects —
    /// is unchanged. `exit_overhead_cycles` is a TSC delta, which is in MPERF units
    /// (MPERF runs at the nominal/TSC rate); APERF runs at the core frequency, i.e.
    /// `ratio * MPERF`, so its decrement is scaled by the established ratio.
    pub fn on_vmresume(&self, entry_tsc: u64, guest_rip: u64) {
        let last_tsc = self.last_exit_tsc.load(Ordering::Acquire);
        let exit_overhead_cycles = entry_tsc.saturating_sub(last_tsc);

        let current_aperf = self.aperf.load(Ordering::Relaxed);
        let current_mperf = self.mperf.load(Ordering::Relaxed);

        // Preserve APERF/MPERF (default 1.0 before MPERF has advanced).
        let ratio = if current_mperf > 0 {
            (current_aperf as f64) / (current_mperf as f64)
        } else {
            1.0
        };
        let aperf_overhead = (exit_overhead_cycles as f64 * ratio) as u64;

        self.aperf.store(
            current_aperf.saturating_sub(aperf_overhead),
            Ordering::Release,
        );
        self.mperf.store(
            current_mperf.saturating_sub(exit_overhead_cycles),
            Ordering::Release,
        );

        self.last_guest_rip.store(guest_rip, Ordering::Release);
    }

    /// Handle RDMSR 0xE8 (IA32_APERF) — actual performance counter
    pub fn read_aperf(&self) -> u64 {
        self.aperf.load(Ordering::Acquire)
    }

    /// Handle RDMSR 0xE7 (IA32_MPERF) — max frequency counter
    pub fn read_mperf(&self) -> u64 {
        self.mperf.load(Ordering::Acquire)
    }

    /// Handle WRMSR — guest tries to reset counters
    pub fn write_aperf(&self, value: u64) {
        self.aperf.store(value, Ordering::Release);
    }

    pub fn write_mperf(&self, value: u64) {
        self.mperf.store(value, Ordering::Release);
    }
}

/// TSC offsetting helper
pub struct TscOffsetHelper {
    /// Host TSC at VM creation
    pub vm_creation_tsc: u64,
    /// Guest-side offset to apply (VMCS IA32_TSC_OFFSET field)
    pub tsc_offset: i64,
}

impl TscOffsetHelper {
    pub fn new(vm_creation_tsc: u64) -> Self {
        Self {
            vm_creation_tsc,
            tsc_offset: 0,
        }
    }

    /// Calculate offset to make guest TSC appear consistent
    pub fn calculate_offset(&mut self, current_host_tsc: u64, guest_visible_tsc: u64) {
        // offset = guest_tsc - host_tsc
        // When guest reads TSC, the CPU computes: TSC + offset
        // Goal: make the delta appear normal despite exit overhead
        self.tsc_offset = (guest_visible_tsc as i64) - (current_host_tsc as i64);
    }
}

// LBR sanitization lives in the canonical `enlil_devices::stealth::lbr::LbrState`
// (`sanitize_after_exit`), which erases both branch endpoints *and* the LBR_INFO
// cycle count, gates on the guest's DEBUGCTL.LBR enable, and tracks the full
// 32-entry MSR-indexed stack. A skeletal `LbrSanitizer` used to sit here with a
// never-wired `expected_guest_branch_target` field and no info-field clearing —
// a partial duplicate, so it was removed; the KVM backend should use `LbrState`.

// The CPUID pre-computation cache lives in the canonical, reference-corrected
// `enlil_devices::stealth::cpuid::CpuidStealthTable` (with proper vendor-specific
// out-of-range semantics). A skeletal `CpuidCachingHelper` stub used to sit here and
// duplicated it — incompletely and with the all-zeros `0x40000000` tell — so it was
// removed; the KVM backend (which can reach `enlil-devices`) should use that table.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vcpu_timing_state() {
        let state = VcpuTimingState::new();
        state.write_aperf(1000);
        assert_eq!(state.read_aperf(), 1000);
    }

    #[test]
    fn vmexit_overhead_hidden_from_both_counters_preserving_ratio() {
        // Ratio 1.0: both counters must drop by the exit overhead, ratio unchanged.
        let state = VcpuTimingState::new();
        state.write_aperf(1000);
        state.write_mperf(1000);
        state.on_vmexit(100);
        state.on_vmresume(150, 0xDEAD); // 50 cycles of exit overhead
        assert_eq!(state.read_mperf(), 950, "MPERF must hide the exit overhead");
        assert_eq!(state.read_aperf(), 950, "APERF must hide the exit overhead");
        // The old code left MPERF at 1000 (ratio 0.95) — a detectable anomaly.
    }

    #[test]
    fn vmexit_overhead_scales_aperf_by_ratio() {
        // Ratio 0.8 (core running below nominal): APERF decrement is ratio-scaled so
        // the APERF/MPERF ratio is preserved across the hidden exit.
        let state = VcpuTimingState::new();
        state.write_aperf(800);
        state.write_mperf(1000);
        state.on_vmexit(0);
        state.on_vmresume(50, 0); // overhead 50
        assert_eq!(state.read_mperf(), 950); // 1000 - 50
        assert_eq!(state.read_aperf(), 760); // 800 - (50 * 0.8)
                                             // Ratio preserved: 760/950 == 0.8 == 800/1000.
        assert!((state.read_aperf() as f64 / state.read_mperf() as f64 - 0.8).abs() < 1e-9);
    }

    #[test]
    fn vmexit_counters_saturate_at_zero() {
        // A large exit overhead must not underflow the shadow counters.
        let state = VcpuTimingState::new();
        state.write_aperf(10);
        state.write_mperf(10);
        state.on_vmexit(0);
        state.on_vmresume(1000, 0);
        assert_eq!(state.read_aperf(), 0);
        assert_eq!(state.read_mperf(), 0);
    }

    #[test]
    fn test_tsc_offset_calculation() {
        let mut helper = TscOffsetHelper::new(0);
        helper.calculate_offset(1000, 2000);
        assert_eq!(helper.tsc_offset, 1000);
    }
}

//! Timing Stealth — Hide VM exit overhead
//!
//! Implements APERF/MPERF shadow counters and TSC offsetting to defeat IET
//! divergence detection and timing-based VM detectors. (LBR sanitization lives
//! in the canonical `enlil_devices::stealth::lbr` — see the note below.)

use enlil_devices::stealth::pmc::PmcRateModel;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
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
    /// Whether a VMEXIT has ever been recorded ([`on_vmexit`](Self::on_vmexit)
    /// has run). Distinct from `last_exit_tsc == 0`, which is a *valid* exit
    /// timestamp — without this flag the very first `on_vmresume` would compute
    /// `entry_tsc - 0`, an enormous bogus "exit overhead" that zeroes both
    /// shadows on the first guest entry.
    pub exit_seen: AtomicBool,
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
            exit_seen: AtomicBool::new(false),
            last_guest_rip: AtomicU64::new(0),
        })
    }

    /// Called on VMEXIT: measure how long we spent outside the guest
    pub fn on_vmexit(&self, exit_tsc: u64) {
        self.last_exit_tsc.store(exit_tsc, Ordering::Release);
        self.exit_seen.store(true, Ordering::Release);
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
        // No exit has happened yet: there is no overhead to hide, and
        // `entry_tsc - 0` would be a huge spurious decrement that zeroes the
        // shadows (e.g. wiping a seeded APERF before the guest's first read).
        // Record the RIP and return.
        if !self.exit_seen.load(Ordering::Acquire) {
            self.last_guest_rip.store(guest_rip, Ordering::Release);
            return;
        }
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

    /// Advance the shadow counters for guest execution time, at the rates of
    /// the PMC rate model: MPERF at the reference (TSC) rate, APERF at the
    /// model's core rate.
    ///
    /// This is the cross-surface consistency requirement documented on
    /// [`PmcRateModel`]: a detector can compare the APERF/MPERF ratio against
    /// RDPMC's `CPU_CLK_UNHALTED.THREAD / REF_TSC` — both surfaces must encode
    /// the same core/ref ratio, so the run loop must drive this and
    /// `PmcState::advance_counters` with the *same* model and the same
    /// reference-cycle delta. It also establishes a non-1.0 ratio from the
    /// first guest read (the shadows previously only ever decremented on
    /// exits, leaving the default ratio at exactly 1.0 — its own tell).
    pub fn advance(&self, guest_ref_cycles: u64, model: &PmcRateModel) {
        self.mperf.fetch_add(guest_ref_cycles, Ordering::AcqRel);
        self.aperf
            .fetch_add(model.core_cycles(guest_ref_cycles), Ordering::AcqRel);
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
    fn first_resume_without_a_prior_exit_does_not_zero_the_shadows() {
        // A run loop seeds the shadows, then the *first* run_vcpu_timed calls
        // on_vmresume before any on_vmexit has set a baseline. Without the
        // exit_seen guard, entry_tsc - 0 is a huge spurious overhead that wipes
        // the seed; with it, the seeded values survive to the guest's first read.
        let state = VcpuTimingState::new();
        state.write_aperf(0xBE);
        state.write_mperf(0xAB);
        // A realistically large entry TSC, the value rdtsc would return.
        state.on_vmresume(0x1234_5678_9ABC, 0x1000);
        assert_eq!(
            state.read_aperf(),
            0xBE,
            "seeded APERF must survive the first resume"
        );
        assert_eq!(
            state.read_mperf(),
            0xAB,
            "seeded MPERF must survive the first resume"
        );
        assert_eq!(state.last_guest_rip.load(Ordering::Acquire), 0x1000);

        // Once a real exit baseline exists, overhead hiding resumes normally.
        state.on_vmexit(0x1234_5678_9ABC + 1000);
        state.on_vmresume(0x1234_5678_9ABC + 1100, 0x2000); // 100 cycles overhead
        assert_eq!(
            state.read_mperf(),
            0xAB - 100,
            "overhead hiding resumes after an exit"
        );
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
    fn advance_tracks_the_pmc_rate_model_ratio() {
        // Drive the MSR-surface shadows and the RDPMC-surface fixed counters
        // with the same model and delta: the two surfaces must agree exactly,
        // and the ratio must not be the 1.0 identity.
        use enlil_devices::stealth::pmc::PmcState;

        let model = PmcRateModel::DEFAULT;
        let state = VcpuTimingState::new();
        let mut pmc = PmcState::new();
        pmc.fixed_ctr_ctrl = 0x330; // enable core + ref fixed counters

        state.advance(1_000_000, &model);
        pmc.advance_counters(1_000_000);

        assert_eq!(state.read_mperf(), pmc.fixed_counters[2], "ref surfaces");
        assert_eq!(state.read_aperf(), pmc.fixed_counters[1], "core surfaces");
        assert_ne!(state.read_aperf(), state.read_mperf(), "ratio ≠ 1.0");

        // An exit hidden by on_vmresume must preserve that same ratio.
        state.on_vmexit(0);
        state.on_vmresume(50_000, 0);
        let ratio = state.read_aperf() as f64 / state.read_mperf() as f64;
        let model_ratio = model.core_per_kilo_ref as f64 / 1000.0;
        assert!(
            (ratio - model_ratio).abs() < 1e-3,
            "exit hiding must preserve the model ratio (got {ratio}, want {model_ratio})"
        );
    }

    #[test]
    fn iet_ratio_does_not_diverge_across_many_interleaved_exits() {
        // Model a pafish/al-khaser-style IET (Instruction-Emulation-Time)
        // detector: a guest interleaves real execution with many hypervisor
        // exits and, at each sample, reads IA32_APERF/IA32_MPERF and computes the
        // core-frequency ratio. On bare metal that ratio is rock-steady; a naive
        // hypervisor that lets exit overhead leak into one counter but not the
        // other makes it wobble in a detectable way. Drive the stealth shadows
        // through that interleaving and assert the sampled ratio never strays from
        // the model ratio beyond a tight tolerance — the anti-detection guarantee
        // (Phase 5.8), exercising the 5.4 timing-stealth path end to end.
        let model = PmcRateModel::DEFAULT;
        let state = VcpuTimingState::new();
        let model_ratio = model.core_per_kilo_ref as f64 / 1000.0;

        // A realistic starting TSC so exit timestamps look like real rdtsc reads.
        let mut tsc: u64 = 0x1_0000_0000;
        // Establish the non-1.0 ratio from an initial slice of execution.
        state.advance(500_000, &model);

        let mut max_dev = 0.0_f64;
        for i in 0..1000u64 {
            // A slice of guest execution advances both shadows at the model rates.
            let ref_cycles = 10_000 + (i % 7) * 1_234;
            state.advance(ref_cycles, &model);
            tsc += ref_cycles;

            // A hypervisor exit of varying — sometimes large — overhead the guest
            // must not be able to see in either counter.
            state.on_vmexit(tsc);
            let overhead = 200 + (i % 13) * 90;
            tsc += overhead;
            state.on_vmresume(tsc, 0x1000 + i);

            // The guest samples the two MSRs and computes the frequency ratio.
            let aperf = state.read_aperf() as f64;
            let mperf = state.read_mperf() as f64;
            assert!(mperf > 0.0, "MPERF must keep advancing");
            max_dev = max_dev.max((aperf / mperf - model_ratio).abs());
        }

        // Over 1000 interleaved exits the sampled ratio stays glued to the model
        // ratio; only sub-cycle integer truncation may drift it.
        assert!(
            max_dev < 1e-3,
            "IET ratio diverged by {max_dev} across 1000 exits (model ratio {model_ratio})"
        );
    }

    #[test]
    fn test_tsc_offset_calculation() {
        let mut helper = TscOffsetHelper::new(0);
        helper.calculate_offset(1000, 2000);
        assert_eq!(helper.tsc_offset, 1000);
    }
}

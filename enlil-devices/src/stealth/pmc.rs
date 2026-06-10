//! PMC (Performance Monitoring Counter) virtualization
//!
//! Some detectors use RDPMC to read hardware performance counters.
//! We virtualize PMC access to return consistent values that don't
//! reveal VMEXIT overhead.

use crate::truncate::{Widen, u32_of};
/// Maximum number of general-purpose PMCs to virtualize
pub const MAX_GP_PMCS: usize = 8;
/// Maximum number of fixed-function PMCs
pub const MAX_FIXED_PMCS: usize = 4;

/// Rates at which the fixed-function counters advance per unit of guest time.
///
/// `advance_counters` receives a **TSC delta** (reference cycles). On bare
/// metal the three architectural fixed counters run at *different* rates:
///
/// - Fixed 2 (`CPU_CLK_UNHALTED.REF_TSC`) ticks at the TSC/reference rate.
/// - Fixed 1 (`CPU_CLK_UNHALTED.THREAD`) ticks at the *core* frequency, which
///   under turbo/throttling differs from the reference rate — its ratio to
///   fixed 2 is exactly the `APERF/MPERF` ratio (Intel SDM vol. 3,
///   "Time-Stamp Counter": APERF/MPERF and `THREAD/REF_TSC` measure the same
///   actual-vs-nominal frequency signal).
/// - Fixed 0 (`INST_RETIRED.ANY`) ticks at `IPC × core cycles`; real IPC is
///   workload-dependent but is essentially never *exactly* 1.0 over a window.
///
/// The previous model advanced all three by the same delta — IPC ≡ 1.0 and
/// core ≡ ref forever — which is the same class of tell as the APERF/MPERF
/// no-op this repo already fixed: a detector cross-checking RDPMC against
/// RDTSC (or APERF/MPERF) sees three perfectly identical counters.
///
/// **Consistency requirement:** the KVM run loop must seed the
/// `VcpuTimingState` APERF/MPERF shadows with the same core/ref ratio used
/// here (`core_per_kilo_ref`), or the two surfaces contradict each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PmcRateModel {
    /// Core cycles per 1000 reference cycles (the APERF/MPERF ratio × 1000).
    pub core_per_kilo_ref: u64,
    /// Instructions retired per 1000 core cycles (IPC × 1000).
    pub instr_per_kilo_core: u64,
    /// TOPDOWN.SLOTS (fixed 3, Ice Lake+) per core cycle: the pipeline
    /// allocation width (4 on Ice Lake; only used if the guest enables it).
    pub slots_per_core_cycle: u64,
}

impl PmcRateModel {
    /// A busy core under light turbo (core = 1.15 × ref) retiring a modest
    /// 1.31 IPC — deliberately not round numbers, and neither identical to
    /// nor an integer multiple of the reference rate.
    pub const DEFAULT: Self = Self {
        core_per_kilo_ref: 1150,
        instr_per_kilo_core: 1310,
        slots_per_core_cycle: 4,
    };

    /// Core-cycle delta for a given reference-cycle (TSC) delta.
    #[must_use]
    pub fn core_cycles(&self, ref_cycles: u64) -> u64 {
        mul_per_kilo(ref_cycles, self.core_per_kilo_ref)
    }

    /// Instructions-retired delta for a given reference-cycle (TSC) delta.
    #[must_use]
    pub fn instructions(&self, ref_cycles: u64) -> u64 {
        mul_per_kilo(self.core_cycles(ref_cycles), self.instr_per_kilo_core)
    }
}

impl Default for PmcRateModel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// `value × per_kilo / 1000` without intermediate overflow (wraps to 64 bits,
/// matching the counters' wrapping semantics).
fn mul_per_kilo(value: u64, per_kilo: u64) -> u64 {
    Widen::to_u64((u128::from(value) * u128::from(per_kilo)) / 1000)
}

/// Per-vCPU PMC state
#[derive(Debug, Clone)]
pub struct PmcState {
    /// General-purpose PMC shadow values
    pub gp_counters: [u64; MAX_GP_PMCS],
    /// Fixed-function PMC shadow values
    pub fixed_counters: [u64; MAX_FIXED_PMCS],
    /// Event select MSRs (what each GP PMC is counting)
    pub event_select: [u64; MAX_GP_PMCS],
    /// Fixed counter control
    pub fixed_ctr_ctrl: u64,
    /// Global PMC control
    pub global_ctrl: u64,
    /// Global PMC status
    pub global_status: u64,
    /// Whether RDPMC should be trapped
    pub trap_rdpmc: bool,
    /// Rates at which the fixed counters advance (see [`PmcRateModel`])
    pub rate_model: PmcRateModel,
}

/// PMC-related MSR addresses
pub mod msr {
    pub const IA32_PMC0: u32 = 0xC1;
    pub const IA32_PERFEVTSEL0: u32 = 0x186;
    pub const IA32_FIXED_CTR0: u32 = 0x309;
    pub const IA32_FIXED_CTR_CTRL: u32 = 0x38D;
    pub const IA32_PERF_GLOBAL_CTRL: u32 = 0x38F;
    pub const IA32_PERF_GLOBAL_STATUS: u32 = 0x38E;
    pub const IA32_PERF_GLOBAL_STATUS_RESET: u32 = 0x390;
}

impl PmcState {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            gp_counters: [0; MAX_GP_PMCS],
            fixed_counters: [0; MAX_FIXED_PMCS],
            event_select: [0; MAX_GP_PMCS],
            fixed_ctr_ctrl: 0,
            global_ctrl: 0,
            global_status: 0,
            trap_rdpmc: true,
            rate_model: PmcRateModel::DEFAULT,
        }
    }

    /// Handle RDPMC instruction. Returns the shadow counter value.
    /// `ecx` is the PMC index: 0-N for GP, 0x40000000+ for fixed.
    #[must_use]
    pub const fn read_pmc(&self, ecx: u32) -> u64 {
        if ecx >= 0x4000_0000 {
            // Fixed-function PMC
            let idx = (ecx - 0x4000_0000) as usize;
            if idx < MAX_FIXED_PMCS {
                self.fixed_counters[idx]
            } else {
                0
            }
        } else {
            // General-purpose PMC
            let idx = ecx as usize;
            if idx < MAX_GP_PMCS {
                self.gp_counters[idx]
            } else {
                0
            }
        }
    }

    /// Handle WRMSR for a PMC MSR
    pub fn write_msr(&mut self, msr: u32, value: u64) {
        match msr {
            m if m >= msr::IA32_PMC0 && m < msr::IA32_PMC0 + u32_of(MAX_GP_PMCS) => {
                let idx = (m - msr::IA32_PMC0) as usize;
                self.gp_counters[idx] = value;
            }
            m if m >= msr::IA32_PERFEVTSEL0 && m < msr::IA32_PERFEVTSEL0 + u32_of(MAX_GP_PMCS) => {
                let idx = (m - msr::IA32_PERFEVTSEL0) as usize;
                self.event_select[idx] = value;
            }
            m if m >= msr::IA32_FIXED_CTR0 && m < msr::IA32_FIXED_CTR0 + u32_of(MAX_FIXED_PMCS) => {
                let idx = (m - msr::IA32_FIXED_CTR0) as usize;
                self.fixed_counters[idx] = value;
            }
            msr::IA32_FIXED_CTR_CTRL => self.fixed_ctr_ctrl = value,
            msr::IA32_PERF_GLOBAL_CTRL => self.global_ctrl = value,
            msr::IA32_PERF_GLOBAL_STATUS_RESET => {
                self.global_status &= !value;
            }
            _ => {}
        }
    }

    /// Handle RDMSR for a PMC MSR
    #[must_use]
    pub fn read_msr(&self, msr: u32) -> Option<u64> {
        match msr {
            m if m >= msr::IA32_PMC0 && m < msr::IA32_PMC0 + u32_of(MAX_GP_PMCS) => {
                let idx = (m - msr::IA32_PMC0) as usize;
                Some(self.gp_counters[idx])
            }
            m if m >= msr::IA32_PERFEVTSEL0 && m < msr::IA32_PERFEVTSEL0 + u32_of(MAX_GP_PMCS) => {
                let idx = (m - msr::IA32_PERFEVTSEL0) as usize;
                Some(self.event_select[idx])
            }
            m if m >= msr::IA32_FIXED_CTR0 && m < msr::IA32_FIXED_CTR0 + u32_of(MAX_FIXED_PMCS) => {
                let idx = (m - msr::IA32_FIXED_CTR0) as usize;
                Some(self.fixed_counters[idx])
            }
            msr::IA32_FIXED_CTR_CTRL => Some(self.fixed_ctr_ctrl),
            msr::IA32_PERF_GLOBAL_CTRL => Some(self.global_ctrl),
            msr::IA32_PERF_GLOBAL_STATUS => Some(self.global_status),
            _ => None,
        }
    }

    /// Advance shadow PMC values proportionally to guest execution time.
    /// Called during VMENTRY to simulate counter advancement.
    ///
    /// `guest_ref_cycles` is a TSC delta (reference cycles). The fixed
    /// counters advance at the *distinct* per-counter rates of the
    /// [`PmcRateModel`] — advancing them all by the raw delta pins IPC to
    /// exactly 1.0 and core cycles exactly equal to reference cycles, an
    /// identity no real CPU sustains and a cross-check detection tell.
    pub fn advance_counters(&mut self, guest_ref_cycles: u64) {
        let core_cycles = self.rate_model.core_cycles(guest_ref_cycles);

        for (i, &event) in self.event_select.iter().enumerate() {
            if event != 0 && (self.global_ctrl & (1u64 << i)) != 0 {
                // Simple model: advance proportionally to core cycles
                self.gp_counters[i] = self.gp_counters[i].wrapping_add(core_cycles);
            }
        }

        // Fixed counters (enable = OS|USR bits of the counter's 4-bit
        // IA32_FIXED_CTR_CTRL field). Each runs at its own rate:
        // 0: INST_RETIRED.ANY — IPC × core cycles
        if (self.fixed_ctr_ctrl & 0x3) != 0 {
            self.fixed_counters[0] =
                self.fixed_counters[0].wrapping_add(self.rate_model.instructions(guest_ref_cycles));
        }
        // 1: CPU_CLK_UNHALTED.THREAD — core-frequency cycles
        if (self.fixed_ctr_ctrl & 0x30) != 0 {
            self.fixed_counters[1] = self.fixed_counters[1].wrapping_add(core_cycles);
        }
        // 2: CPU_CLK_UNHALTED.REF_TSC — reference (TSC-rate) cycles
        if (self.fixed_ctr_ctrl & 0x300) != 0 {
            self.fixed_counters[2] = self.fixed_counters[2].wrapping_add(guest_ref_cycles);
        }
        // 3: TOPDOWN.SLOTS (Ice Lake+) — allocation width × core cycles
        if (self.fixed_ctr_ctrl & 0x3000) != 0 {
            self.fixed_counters[3] = self.fixed_counters[3]
                .wrapping_add(core_cycles.wrapping_mul(self.rate_model.slots_per_core_cycle));
        }
    }
}

impl Default for PmcState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_counters_zero() {
        let pmc = PmcState::new();
        for i in 0..MAX_GP_PMCS {
            assert_eq!(pmc.read_pmc(u32_of(i)), 0);
        }
    }

    #[test]
    fn rdpmc_gp_counter() {
        let mut pmc = PmcState::new();
        pmc.gp_counters[2] = 12345;
        assert_eq!(pmc.read_pmc(2), 12345);
    }

    #[test]
    fn rdpmc_fixed_counter() {
        let mut pmc = PmcState::new();
        pmc.fixed_counters[1] = 99999;
        assert_eq!(pmc.read_pmc(0x4000_0001), 99999);
    }

    #[test]
    fn write_read_msr() {
        let mut pmc = PmcState::new();
        pmc.write_msr(msr::IA32_PMC0, 42);
        assert_eq!(pmc.read_msr(msr::IA32_PMC0), Some(42));
    }

    #[test]
    fn advance_counters_with_event_select() {
        let mut pmc = PmcState::new();
        // Enable GP counter 0 with some event
        pmc.event_select[0] = 0x41; // Some event selector
        pmc.global_ctrl = 1; // Enable counter 0

        // GP counters tick at the core rate: 1000 ref → 1150 core (default model).
        pmc.advance_counters(1000);
        assert_eq!(pmc.gp_counters[0], 1150);

        pmc.advance_counters(500);
        assert_eq!(pmc.gp_counters[0], 1150 + 575);
    }

    #[test]
    fn advance_fixed_counters() {
        let mut pmc = PmcState::new();
        // Enable fixed counter 1 (core cycles)
        pmc.fixed_ctr_ctrl = 0x30;

        pmc.advance_counters(2000);
        assert_eq!(pmc.fixed_counters[1], 2300); // 2000 ref × 1.15
        assert_eq!(pmc.fixed_counters[0], 0); // Not enabled
    }

    #[test]
    fn fixed_counters_advance_at_distinct_plausible_rates() {
        let mut pmc = PmcState::new();
        pmc.fixed_ctr_ctrl = 0x3333; // enable all four fixed counters (OS|USR)

        pmc.advance_counters(10_000);
        let [instr, core, ref_tsc, slots] = pmc.fixed_counters;

        assert_eq!(ref_tsc, 10_000, "REF_TSC ticks at the TSC rate");
        assert_eq!(core, 11_500, "core cycles = ref × 1.15 (default model)");
        assert_eq!(instr, 15_065, "instructions = core × 1.31 IPC");
        assert_eq!(slots, 46_000, "TOPDOWN.SLOTS = 4 × core cycles");

        // The detection tells this model removes: all-equal counters / IPC ≡ 1.0.
        assert_ne!(core, ref_tsc, "core ≡ ref is a VM tell");
        assert_ne!(instr, core, "IPC ≡ 1.0 is a VM tell");
    }

    #[test]
    fn core_ref_ratio_matches_the_model_for_aperf_mperf_consistency() {
        // A detector can cross-check RDPMC's core/ref against APERF/MPERF —
        // the two must encode the same ratio. The model exposes that ratio as
        // core_per_kilo_ref; verify the advanced counters reproduce it.
        let mut pmc = PmcState::new();
        pmc.fixed_ctr_ctrl = 0x330; // core + ref

        pmc.advance_counters(1_000_000);
        let ratio_kilo = pmc.fixed_counters[1] * 1000 / pmc.fixed_counters[2];
        assert_eq!(ratio_kilo, pmc.rate_model.core_per_kilo_ref);
    }

    #[test]
    fn custom_rate_model_is_honored() {
        let mut pmc = PmcState::new();
        pmc.rate_model = PmcRateModel {
            core_per_kilo_ref: 2000,  // 2.0× turbo
            instr_per_kilo_core: 500, // 0.5 IPC (memory-bound)
            slots_per_core_cycle: 4,
        };
        pmc.fixed_ctr_ctrl = 0x333;

        pmc.advance_counters(1000);
        assert_eq!(pmc.fixed_counters[2], 1000);
        assert_eq!(pmc.fixed_counters[1], 2000);
        assert_eq!(pmc.fixed_counters[0], 1000);
    }

    #[test]
    fn global_status_reset() {
        let mut pmc = PmcState::new();
        pmc.global_status = 0xFF;
        pmc.write_msr(msr::IA32_PERF_GLOBAL_STATUS_RESET, 0x0F);
        assert_eq!(pmc.global_status, 0xF0);
    }
}

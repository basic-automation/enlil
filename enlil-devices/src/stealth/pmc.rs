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

/// Writable bits of `IA32_PERF_GLOBAL_CTRL` (MSR 0x38F): `EN_PMCn` for the
/// implemented GP counters (bits `[MAX_GP_PMCS-1 : 0]`) and `EN_FIXEDn` for the
/// fixed counters (bits `[32 + MAX_FIXED_PMCS - 1 : 32]`). These exactly match
/// the counter counts CPUID leaf 0xA advertises (`CpuidStealthTable::build_leaf_a`).
/// Every other bit is reserved — real hardware `#GP`s a write that sets one, so
/// they must read back 0 rather than store the guest's value (a guest writing
/// all-ones and reading it back unchanged would catch a hypervisor that
/// stored the reserved bits verbatim).
const GLOBAL_CTRL_MASK: u64 = ((1u64 << MAX_GP_PMCS) - 1) | (((1u64 << MAX_FIXED_PMCS) - 1) << 32);

/// Writable bits of `IA32_FIXED_CTR_CTRL` (MSR 0x38D): a 4-bit control field
/// (`EN`/`AnyThread`/`PMI`) per fixed counter, bits `[4*MAX_FIXED_PMCS - 1 : 0]`;
/// the rest are reserved (same reserved-bit-writeback tell as `GLOBAL_CTRL`).
const FIXED_CTR_CTRL_MASK: u64 = (1u64 << (4 * MAX_FIXED_PMCS)) - 1;

/// Writable bits of the AMD `PerfMonV2` `PerfCntrGlobalCtl` (MSR `0xC000_0301`):
/// one `PerfCtrEn` bit per implemented core PMC (bits `[AMD_CORE_PMCS-1 : 0]`).
/// Higher bits are reserved — the AMD-side counterpart of `GLOBAL_CTRL_MASK`,
/// on the path an AMD-presented guest (the same-vendor case on this AMD host)
/// actually exercises.
const AMD_GLOBAL_CTRL_MASK: u64 = (1u64 << msr::AMD_CORE_PMCS) - 1;

/// Writable bits of an **Intel** `IA32_PERFEVTSELn` (MSR `0x186 + n`). Every
/// architectural field — Event Select `[7:0]`, Unit Mask `[15:8]`, USR/OS/Edge/
/// PinControl/INT `[16:20]`, `AnyThread` `[21]`, EN/INV `[22:23]`, CMASK `[31:24]`
/// — lives in the low 32 bits; **all of `[63:32]` is reserved** (SDM Vol. 3
/// §20.2.1.1, Fig. 20-1). A guest that writes all-ones and reads back the high
/// bits unchanged would catch a hypervisor that stored the value verbatim, so
/// the reserved upper half must read 0.
const INTEL_PERFEVTSEL_MASK: u64 = 0xFFFF_FFFF;

/// Writable bits of an **AMD** `PerfEvtSel` (legacy `0xC001_000n` / core
/// `0xC001_0200 + 2n`). AMD defines more than Intel: besides the low-32 fields
/// it has the Event Select extension `[35:32]` and HostOnly/GuestOnly `[41:40]`
/// (AMD APM Vol. 2 §13.2.1). The definitely-reserved regions `[39:36]` and
/// `[63:42]` must read back 0. The low 32 bits are kept verbatim (AMD's two
/// in-range reserved holes at `[19]`/`[21]` vary by family, so policing them
/// risks corrupting a legitimate counter config for a negligible extra tell).
/// This is the crux of why the mask must be vendor-aware: the shared
/// `event_select` shadow is reached by **either** the Intel `IA32_*` path
/// **or** this AMD path (never both, since a guest is presented one vendor), and
/// Intel reserves `[63:32]` wholesale where AMD keeps `[41:32]` partly live — a
/// single mask would corrupt one vendor.
const AMD_PERFEVTSEL_MASK: u64 = 0x0000_030F_FFFF_FFFF;

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
/// **Consistency requirement:** the KVM run loop must drive the
/// `VcpuTimingState` APERF/MPERF shadows (`enlil-core`, via its
/// model-driven `advance`) with the same model and reference-cycle delta
/// used here, or the two surfaces contradict each other.
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

    // --- AMD PMC MSRs (AMD APM vol. 2 §13.2 / PPR Family 19h) ---
    //
    // An AMD-presented guest reads its performance counters through these, not
    // the Intel `IA32_*` registers above. The legacy (K7) block aliases the
    // first four core counters on real silicon, so both map to the same shadow.

    /// AMD legacy `PerfEvtSel0..3` (`0xC001_0000..0xC001_0003`).
    pub const AMD_LEGACY_PERFEVTSEL0: u32 = 0xC001_0000;
    /// AMD legacy `PerfCtr0..3` (`0xC001_0004..0xC001_0007`).
    pub const AMD_LEGACY_PERFCTR0: u32 = 0xC001_0004;
    /// Number of legacy AMD PMCs.
    pub const AMD_LEGACY_PMCS: u32 = 4;

    /// AMD core / `PerfMonV2` `PerfEvtSel[n] = 0xC001_0200 + 2n` (even MSRs).
    pub const AMD_CORE_PERFEVTSEL0: u32 = 0xC001_0200;
    /// AMD core / `PerfMonV2` `PerfCtr[n] = 0xC001_0201 + 2n` (odd MSRs).
    pub const AMD_CORE_PERFCTR0: u32 = 0xC001_0201;
    /// Number of core AMD PMCs (Zen exposes 6).
    pub const AMD_CORE_PMCS: u32 = 6;

    /// AMD `PerfMonV2` `PerfCntrGlobalStatus` (`0xC000_0300`).
    pub const AMD_PERF_CNTR_GLOBAL_STATUS: u32 = 0xC000_0300;
    /// AMD `PerfMonV2` `PerfCntrGlobalCtl` (`0xC000_0301`).
    pub const AMD_PERF_CNTR_GLOBAL_CTL: u32 = 0xC000_0301;
    /// AMD `PerfMonV2` `PerfCntrGlobalStatusClr` (`0xC000_0302`).
    pub const AMD_PERF_CNTR_GLOBAL_STATUS_CLR: u32 = 0xC000_0302;
}

/// What an AMD PMC MSR addresses within the shared shadow arrays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AmdPmcTarget {
    /// A counter value (`gp_counters[idx]`).
    Counter(usize),
    /// An event-select register (`event_select[idx]`).
    EventSelect(usize),
    /// The `PerfMonV2` global-control MSR (`global_ctrl`).
    GlobalCtrl,
    /// The `PerfMonV2` global-status MSR (`global_status`, read-only here).
    GlobalStatus,
    /// The `PerfMonV2` global-status-clear MSR (write clears `global_status`).
    GlobalStatusClr,
}

/// Map an AMD PMC MSR number onto the shadow arrays, or `None` if `msr` is not
/// an AMD PMC register. The legacy (K7) block aliases the first four core
/// counters, exactly as on hardware, so both decode to the same index.
const fn amd_pmc_target(msr: u32) -> Option<AmdPmcTarget> {
    use msr::{
        AMD_CORE_PERFEVTSEL0, AMD_CORE_PMCS, AMD_LEGACY_PERFCTR0, AMD_LEGACY_PERFEVTSEL0,
        AMD_LEGACY_PMCS, AMD_PERF_CNTR_GLOBAL_CTL, AMD_PERF_CNTR_GLOBAL_STATUS,
        AMD_PERF_CNTR_GLOBAL_STATUS_CLR,
    };
    if msr >= AMD_LEGACY_PERFEVTSEL0 && msr < AMD_LEGACY_PERFEVTSEL0 + AMD_LEGACY_PMCS {
        return Some(AmdPmcTarget::EventSelect(
            (msr - AMD_LEGACY_PERFEVTSEL0) as usize,
        ));
    }
    if msr >= AMD_LEGACY_PERFCTR0 && msr < AMD_LEGACY_PERFCTR0 + AMD_LEGACY_PMCS {
        return Some(AmdPmcTarget::Counter((msr - AMD_LEGACY_PERFCTR0) as usize));
    }
    // Core / PerfMonV2 counters interleave EvtSel (even) and Ctr (odd).
    if msr >= AMD_CORE_PERFEVTSEL0 && msr < AMD_CORE_PERFEVTSEL0 + 2 * AMD_CORE_PMCS {
        let off = msr - AMD_CORE_PERFEVTSEL0;
        let idx = (off / 2) as usize;
        return Some(if off.is_multiple_of(2) {
            AmdPmcTarget::EventSelect(idx)
        } else {
            AmdPmcTarget::Counter(idx)
        });
    }
    match msr {
        AMD_PERF_CNTR_GLOBAL_CTL => Some(AmdPmcTarget::GlobalCtrl),
        AMD_PERF_CNTR_GLOBAL_STATUS => Some(AmdPmcTarget::GlobalStatus),
        AMD_PERF_CNTR_GLOBAL_STATUS_CLR => Some(AmdPmcTarget::GlobalStatusClr),
        _ => None,
    }
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

    /// Whether `msr` is one of the AMD PMC MSRs this state models (legacy or
    /// core counters/event-selects, or the `PerfMonV2` global registers).
    #[must_use]
    pub const fn is_amd_pmc_msr(msr: u32) -> bool {
        amd_pmc_target(msr).is_some()
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
                self.event_select[idx] = value & INTEL_PERFEVTSEL_MASK;
            }
            m if m >= msr::IA32_FIXED_CTR0 && m < msr::IA32_FIXED_CTR0 + u32_of(MAX_FIXED_PMCS) => {
                let idx = (m - msr::IA32_FIXED_CTR0) as usize;
                self.fixed_counters[idx] = value;
            }
            msr::IA32_FIXED_CTR_CTRL => self.fixed_ctr_ctrl = value & FIXED_CTR_CTRL_MASK,
            msr::IA32_PERF_GLOBAL_CTRL => self.global_ctrl = value & GLOBAL_CTRL_MASK,
            msr::IA32_PERF_GLOBAL_STATUS_RESET => {
                self.global_status &= !value;
            }
            m => self.write_amd_msr(m, value),
        }
    }

    /// Apply a WRMSR to an AMD PMC MSR. A no-op for any MSR this state does not
    /// model. The legacy and core counter blocks alias to the same shadow index.
    const fn write_amd_msr(&mut self, msr: u32, value: u64) {
        match amd_pmc_target(msr) {
            Some(AmdPmcTarget::Counter(i)) if i < MAX_GP_PMCS => self.gp_counters[i] = value,
            Some(AmdPmcTarget::EventSelect(i)) if i < MAX_GP_PMCS => {
                self.event_select[i] = value & AMD_PERFEVTSEL_MASK;
            }
            Some(AmdPmcTarget::GlobalCtrl) => self.global_ctrl = value & AMD_GLOBAL_CTRL_MASK,
            // Writing the clear MSR clears the set status bits (write-1-to-clear),
            // matching the Intel `GLOBAL_STATUS_RESET` semantics above.
            Some(AmdPmcTarget::GlobalStatusClr) => self.global_status &= !value,
            // `GlobalStatus` is read-only; an out-of-range index is ignored.
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
            m => self.read_amd_msr(m),
        }
    }

    /// Serve an RDMSR for an AMD PMC MSR, or `None` if `msr` is not one. The
    /// legacy block reads the same shadow as the aliased core counters.
    #[must_use]
    const fn read_amd_msr(&self, msr: u32) -> Option<u64> {
        match amd_pmc_target(msr) {
            Some(AmdPmcTarget::Counter(i)) if i < MAX_GP_PMCS => Some(self.gp_counters[i]),
            Some(AmdPmcTarget::EventSelect(i)) if i < MAX_GP_PMCS => Some(self.event_select[i]),
            Some(AmdPmcTarget::GlobalCtrl) => Some(self.global_ctrl),
            Some(AmdPmcTarget::GlobalStatus) => Some(self.global_status),
            // `GlobalStatusClr` is write-only; out-of-range index unmodelled.
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
    fn pmu_control_msrs_mask_reserved_bits() {
        let mut pmc = PmcState::new();

        // A guest writes all-ones. The implemented enable bits take; every
        // reserved bit must read back 0 (real hardware #GPs the reserved write).
        pmc.write_msr(msr::IA32_PERF_GLOBAL_CTRL, u64::MAX);
        assert_eq!(
            pmc.read_msr(msr::IA32_PERF_GLOBAL_CTRL),
            Some(0x0000_000F_0000_00FF), // 8 GP enables [7:0] + 4 fixed enables [35:32]
            "PERF_GLOBAL_CTRL reserved bits must read back 0"
        );

        pmc.write_msr(msr::IA32_FIXED_CTR_CTRL, u64::MAX);
        assert_eq!(
            pmc.read_msr(msr::IA32_FIXED_CTR_CTRL),
            Some(0x0000_FFFF), // 4 fixed counters × 4-bit control field
            "FIXED_CTR_CTRL reserved bits must read back 0"
        );

        // Legitimate enable values are unaffected.
        pmc.write_msr(msr::IA32_PERF_GLOBAL_CTRL, 0x07);
        assert_eq!(pmc.read_msr(msr::IA32_PERF_GLOBAL_CTRL), Some(0x07));
    }

    #[test]
    fn amd_global_ctrl_masks_reserved_bits() {
        let mut pmc = PmcState::new();
        // All-ones to the AMD PerfCntrGlobalCtl: only the 6 core-PMC enables
        // (bits [5:0]) take; the reserved bits read back 0.
        pmc.write_msr(msr::AMD_PERF_CNTR_GLOBAL_CTL, u64::MAX);
        assert_eq!(
            pmc.read_msr(msr::AMD_PERF_CNTR_GLOBAL_CTL),
            Some(0x3F),
            "AMD PerfCntrGlobalCtl reserved bits must read back 0"
        );
    }

    #[test]
    fn perfevtsel_masking_is_vendor_aware() {
        // The Intel and AMD event-select MSRs share one shadow, but a guest is
        // presented exactly one vendor and writes only that namespace, so each
        // write path masks to its own vendor's valid bits.

        // Intel IA32_PERFEVTSEL: only the low 32 bits are defined; all of
        // [63:32] is reserved and must read back 0.
        let mut intel = PmcState::new();
        intel.write_msr(msr::IA32_PERFEVTSEL0, u64::MAX);
        assert_eq!(
            intel.read_msr(msr::IA32_PERFEVTSEL0),
            Some(0xFFFF_FFFF),
            "Intel PERFEVTSEL reserved high bits [63:32] must read back 0"
        );
        // A legitimate low-32 event config is untouched (event 0xC0, umask 0x00,
        // USR|OS|EN = 0x53_0000 → 0x0053_00C0).
        intel.write_msr(msr::IA32_PERFEVTSEL0 + 1, 0x0053_00C0);
        assert_eq!(intel.read_msr(msr::IA32_PERFEVTSEL0 + 1), Some(0x0053_00C0));

        // AMD PerfEvtSel additionally defines the Event Select extension [35:32]
        // and HostOnly/GuestOnly [41:40]; only [39:36] and [63:42] are reserved.
        // Masking AMD with the Intel mask would wrongly drop the high event bits.
        let mut amd = PmcState::new();
        amd.write_msr(msr::AMD_CORE_PERFEVTSEL0, u64::MAX);
        assert_eq!(
            amd.read_msr(msr::AMD_CORE_PERFEVTSEL0),
            Some(0x0000_030F_FFFF_FFFF),
            "AMD PerfEvtSel keeps [41:40] and [35:32]; clears [39:36] and [63:42]"
        );
        // The legacy block aliases the same shadow and is masked identically.
        amd.write_msr(msr::AMD_LEGACY_PERFEVTSEL0 + 1, u64::MAX);
        assert_eq!(
            amd.read_msr(msr::AMD_CORE_PERFEVTSEL0 + 2),
            Some(0x0000_030F_FFFF_FFFF)
        );
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

    #[test]
    fn amd_core_pmc_msrs_round_trip_through_the_shadow() {
        // An AMD-presented guest reads/writes its counters via the interleaved
        // core block (EvtSel even, Ctr odd). Counter n = 0xC0010201 + 2n.
        let mut pmc = PmcState::new();
        pmc.write_msr(msr::AMD_CORE_PERFCTR0 + 2 * 3, 0xDEAD); // PerfCtr3
        pmc.write_msr(msr::AMD_CORE_PERFEVTSEL0 + 2 * 3, 0x76); // PerfEvtSel3
        assert_eq!(pmc.read_msr(msr::AMD_CORE_PERFCTR0 + 2 * 3), Some(0xDEAD));
        assert_eq!(pmc.read_msr(msr::AMD_CORE_PERFEVTSEL0 + 2 * 3), Some(0x76));
        // It is the same physical counter RDPMC index 3 reads.
        assert_eq!(pmc.read_pmc(3), 0xDEAD);
    }

    #[test]
    fn amd_legacy_block_aliases_the_first_core_counters() {
        // On real AMD parts the legacy 0xC001_000x block aliases core 0..3.
        let mut pmc = PmcState::new();
        pmc.write_msr(msr::AMD_CORE_PERFCTR0, 0x1234); // core counter 0
        // Legacy PerfCtr0 (0xC0010004) reads the same shadow.
        assert_eq!(pmc.read_msr(msr::AMD_LEGACY_PERFCTR0), Some(0x1234));
        // Writing the legacy MSR is visible through the core MSR too.
        pmc.write_msr(msr::AMD_LEGACY_PERFEVTSEL0 + 1, 0x99); // legacy EvtSel1
        assert_eq!(pmc.read_msr(msr::AMD_CORE_PERFEVTSEL0 + 2), Some(0x99));
    }

    #[test]
    fn amd_perfmon_v2_global_registers() {
        let mut pmc = PmcState::new();
        pmc.write_msr(msr::AMD_PERF_CNTR_GLOBAL_CTL, 0b101);
        assert_eq!(pmc.read_msr(msr::AMD_PERF_CNTR_GLOBAL_CTL), Some(0b101));
        // Global ctl bit n enables core counter n for advance(), mirroring Intel.
        pmc.event_select[0] = 0x42;
        pmc.event_select[2] = 0x42;
        pmc.advance_counters(1000);
        assert_eq!(pmc.gp_counters[0], 1150, "counter 0 enabled by global ctl");
        assert_eq!(pmc.gp_counters[1], 0, "counter 1 not enabled");
        assert_eq!(pmc.gp_counters[2], 1150, "counter 2 enabled by global ctl");
        // Global status is write-1-to-clear via the dedicated Clr MSR.
        pmc.global_status = 0b111;
        pmc.write_msr(msr::AMD_PERF_CNTR_GLOBAL_STATUS_CLR, 0b010);
        assert_eq!(pmc.read_msr(msr::AMD_PERF_CNTR_GLOBAL_STATUS), Some(0b101));
    }

    #[test]
    fn amd_pmc_predicate_excludes_intel_and_unrelated_msrs() {
        assert!(PmcState::is_amd_pmc_msr(msr::AMD_CORE_PERFCTR0));
        assert!(PmcState::is_amd_pmc_msr(msr::AMD_LEGACY_PERFEVTSEL0));
        assert!(PmcState::is_amd_pmc_msr(msr::AMD_PERF_CNTR_GLOBAL_CTL));
        // Intel PMC and unrelated MSRs are not AMD PMC MSRs.
        assert!(!PmcState::is_amd_pmc_msr(msr::IA32_PMC0));
        assert!(!PmcState::is_amd_pmc_msr(0x10)); // IA32_TSC
        // One past each AMD block must not be claimed.
        assert!(!PmcState::is_amd_pmc_msr(
            msr::AMD_CORE_PERFEVTSEL0 + 2 * msr::AMD_CORE_PMCS
        ));
        assert!(!PmcState::is_amd_pmc_msr(
            msr::AMD_LEGACY_PERFCTR0 + msr::AMD_LEGACY_PMCS
        ));
    }
}

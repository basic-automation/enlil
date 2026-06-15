//! Stealth MSR routing — serve spoofed APERF/MPERF, PMC, and LBR MSR values.
//!
//! The KVM run loop forwards guest MSR accesses it does not itself emulate to
//! userspace (see [`KvmBackend::enable_userspace_msr_exits`]). This router is
//! the destination: it answers `RDMSR`/`WRMSR` for the model-specific registers
//! a timing / PMC / branch-record VM detector inspects, serving the shadow
//! values the Phase 5 stealth modules maintain so the guest cannot observe
//! VMEXIT overhead (APERF/MPERF, RDPMC fixed/GP counters) or a hypervisor
//! branch in the LBR stack (`IA32_DEBUGCTL`, the LBR FROM/TO/INFO arrays).
//!
//! Each surface is consistency-coupled by design (see the
//! [`PmcRateModel`](enlil_devices::stealth::pmc::PmcRateModel) note): the
//! APERF/MPERF ratio this router reads from [`VcpuTimingState`] must equal the
//! `CPU_CLK_UNHALTED.THREAD / REF_TSC` ratio RDPMC reads from [`PmcState`], so
//! the run loop drives both with the same model and reference-cycle delta.
//!
//! This is the routing *policy* only — read/write dispatch over the MSR number
//! space. Wiring it into the live [`VmExitHandler`] is a separate step; keeping
//! it standalone lets the whole MSR map be unit-tested without KVM.
//!
//! [`KvmBackend::enable_userspace_msr_exits`]: crate::kvm_backend::KvmBackend
//! [`VmExitHandler`]: crate::kvm_backend::VmExitHandler

use crate::timing_stealth::VcpuTimingState;
use enlil_devices::stealth::lbr::{amd_msr, intel_msr, LbrPlatform, LbrState, LBR_STACK_SIZE};
use enlil_devices::stealth::pmc::{msr as pmc_msr, PmcState, MAX_FIXED_PMCS, MAX_GP_PMCS};
use enlil_devices::stealth::timing::msr as timing_msr;
use std::sync::Arc;

/// Number of LBR stack entries as a `u32` (the FROM/TO/INFO MSR-block width).
const LBR_COUNT: u32 = LBR_STACK_SIZE as u32;

/// Routes guest MSR exits to the per-vCPU stealth shadow state.
///
/// Holds the timing shadows ([`VcpuTimingState`], shared with the run loop so
/// the VMEXIT-overhead accounting and the guest-visible APERF/MPERF agree), the
/// per-vCPU [`PmcState`], and the [`LbrState`].
pub struct StealthMsrRouter {
    /// APERF/MPERF shadows + TSC offset, shared with the run loop's exit
    /// accounting.
    pub timing: Arc<VcpuTimingState>,
    /// Performance-monitoring-counter shadows (RDPMC and the PMC MSRs).
    pub pmc: PmcState,
    /// Last-branch-record shadows (`IA32_DEBUGCTL` + the LBR arrays).
    pub lbr: LbrState,
}

impl StealthMsrRouter {
    /// Build a router over a shared timing state and an LBR state, with a fresh
    /// [`PmcState`].
    #[must_use]
    pub fn new(timing: Arc<VcpuTimingState>, lbr: LbrState) -> Self {
        Self {
            timing,
            pmc: PmcState::new(),
            lbr,
        }
    }

    /// Index into the LBR FROM array if `msr` is in that block.
    const fn lbr_from_index(msr: u32) -> Option<usize> {
        let base = intel_msr::LBR_FROM_BASE;
        if msr >= base && msr < base + LBR_COUNT {
            Some((msr - base) as usize)
        } else {
            None
        }
    }

    /// Index into the LBR TO array if `msr` is in that block.
    const fn lbr_to_index(msr: u32) -> Option<usize> {
        let base = intel_msr::LBR_TO_BASE;
        if msr >= base && msr < base + LBR_COUNT {
            Some((msr - base) as usize)
        } else {
            None
        }
    }

    /// Index into the LBR INFO array if `msr` is in that block.
    const fn lbr_info_index(msr: u32) -> Option<usize> {
        let base = intel_msr::LBR_INFO_BASE;
        if msr >= base && msr < base + LBR_COUNT {
            Some((msr - base) as usize)
        } else {
            None
        }
    }

    /// Whether `msr` is one of the performance-monitoring-counter MSRs the
    /// [`PmcState`] models (including the write-only `GLOBAL_STATUS_RESET`,
    /// which [`PmcState::read_msr`] cannot report).
    #[must_use]
    pub const fn is_pmc_msr(msr: u32) -> bool {
        let gp = pmc_msr::IA32_PMC0;
        let evt = pmc_msr::IA32_PERFEVTSEL0;
        let fixed = pmc_msr::IA32_FIXED_CTR0;
        (msr >= gp && msr < gp + MAX_GP_PMCS as u32)
            || (msr >= evt && msr < evt + MAX_GP_PMCS as u32)
            || (msr >= fixed && msr < fixed + MAX_FIXED_PMCS as u32)
            || matches!(
                msr,
                pmc_msr::IA32_FIXED_CTR_CTRL
                    | pmc_msr::IA32_PERF_GLOBAL_CTRL
                    | pmc_msr::IA32_PERF_GLOBAL_STATUS
                    | pmc_msr::IA32_PERF_GLOBAL_STATUS_RESET
            )
    }

    /// The MSR ranges this router answers, as `(base, count)` pairs, for the
    /// router's platform — the single source of truth to hand to
    /// [`KvmBackend::forward_msrs_to_userspace`] so the KVM filter and this
    /// router cover exactly the same MSRs.
    ///
    /// Timing (APERF/MPERF), `IA32_DEBUGCTL`, and the PMC registers are
    /// platform-independent; the last-branch MSRs are not: only the **AMD**
    /// single-register pair (0x1DB–0x1DE) is included on `AmdSvm`, and only the
    /// **Intel** TOS + FROM/TO/INFO stack blocks on `IntelVmx`. Forwarding the
    /// other platform's LBR MSRs would make non-existent registers readable —
    /// itself a tell — so each platform forwards only what it really exposes.
    /// At most 11 ranges, within KVM's 16-range limit.
    ///
    /// [`KvmBackend::forward_msrs_to_userspace`]: crate::kvm_backend::KvmBackend::forward_msrs_to_userspace
    #[must_use]
    pub fn filter_ranges(&self) -> Vec<(u32, u32)> {
        let gp = MAX_GP_PMCS as u32;
        let fixed = MAX_FIXED_PMCS as u32;
        let mut ranges = vec![
            (timing_msr::IA32_MPERF, 2), // 0xE7 MPERF, 0xE8 APERF
            (intel_msr::IA32_DEBUGCTL, 1),
            (pmc_msr::IA32_PMC0, gp),
            (pmc_msr::IA32_PERFEVTSEL0, gp),
            (pmc_msr::IA32_FIXED_CTR0, fixed),
            // FIXED_CTR_CTRL (0x38D), GLOBAL_STATUS (0x38E), GLOBAL_CTRL (0x38F),
            // GLOBAL_STATUS_RESET (0x390) — four consecutive.
            (pmc_msr::IA32_FIXED_CTR_CTRL, 4),
        ];
        match self.lbr.platform {
            LbrPlatform::AmdSvm => {
                ranges.push((amd_msr::LAST_BRANCH_FROM_IP, 4)); // 0x1DB..=0x1DE
            }
            LbrPlatform::IntelVmx => {
                ranges.push((intel_msr::LBR_TOS, 1));
                ranges.push((intel_msr::LBR_FROM_BASE, LBR_COUNT));
                ranges.push((intel_msr::LBR_TO_BASE, LBR_COUNT));
                ranges.push((intel_msr::LBR_INFO_BASE, LBR_COUNT));
            }
        }
        ranges
    }

    /// Serve a guest `RDMSR`. Returns `Some(value)` for an MSR this router
    /// models, or `None` to let the caller fall through (e.g. inject `#GP`).
    #[must_use]
    pub fn read_msr(&self, msr: u32) -> Option<u64> {
        match msr {
            timing_msr::IA32_APERF => Some(self.timing.read_aperf()),
            timing_msr::IA32_MPERF => Some(self.timing.read_mperf()),
            intel_msr::IA32_DEBUGCTL => Some(self.lbr.read_debug_ctl()),
            intel_msr::LBR_TOS => Some(self.lbr.read_tos()),
            _ => {
                if let Some(v) = self.lbr.read_amd_lbr(msr) {
                    return Some(v);
                }
                if let Some(i) = Self::lbr_from_index(msr) {
                    Some(self.lbr.read_from(i))
                } else if let Some(i) = Self::lbr_to_index(msr) {
                    Some(self.lbr.read_to(i))
                } else if let Some(i) = Self::lbr_info_index(msr) {
                    Some(self.lbr.read_info(i))
                } else {
                    self.pmc.read_msr(msr)
                }
            }
        }
    }

    /// Serve a guest `WRMSR`. Returns `true` if this router models `msr` (and
    /// applied the write), or `false` to let the caller fall through.
    pub fn write_msr(&mut self, msr: u32, value: u64) -> bool {
        match msr {
            timing_msr::IA32_APERF => {
                self.timing.write_aperf(value);
                true
            }
            timing_msr::IA32_MPERF => {
                self.timing.write_mperf(value);
                true
            }
            intel_msr::IA32_DEBUGCTL => {
                self.lbr.write_debug_ctl(value);
                true
            }
            m if self.lbr.write_amd_lbr(m, value) => true,
            m if Self::is_pmc_msr(m) => {
                self.pmc.write_msr(m, value);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enlil_devices::stealth::lbr::LbrPlatform;
    use enlil_devices::stealth::pmc::PmcRateModel;

    fn router() -> StealthMsrRouter {
        StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(LbrPlatform::IntelVmx))
    }

    fn router_for(platform: LbrPlatform) -> StealthMsrRouter {
        StealthMsrRouter::new(VcpuTimingState::new(), LbrState::new(platform))
    }

    #[test]
    fn filter_ranges_cover_exactly_what_the_router_serves() {
        for platform in [LbrPlatform::IntelVmx, LbrPlatform::AmdSvm] {
            let mut r = router_for(platform);
            let ranges = r.filter_ranges();
            assert!(ranges.len() <= 16, "within KVM's 16-range limit");
            // Every MSR the filter forwards must be one the router answers
            // (via read or write) — otherwise we'd forward an MSR only to #GP
            // it, or model an MSR we never forward.
            for (base, count) in ranges {
                for msr in base..base + count {
                    let served = r.read_msr(msr).is_some() || r.write_msr(msr, 0);
                    assert!(served, "router must serve filtered MSR {msr:#x} on {platform:?}");
                }
            }
        }
    }

    #[test]
    fn filter_ranges_are_platform_correct() {
        let intel = router_for(LbrPlatform::IntelVmx).filter_ranges();
        let amd = router_for(LbrPlatform::AmdSvm).filter_ranges();
        let has = |rs: &[(u32, u32)], msr: u32| {
            rs.iter().any(|&(b, c)| msr >= b && msr < b + c)
        };
        // Intel forwards the 32-entry stack + TOS, not the AMD pair.
        assert!(has(&intel, intel_msr::LBR_FROM_BASE));
        assert!(has(&intel, intel_msr::LBR_TOS));
        assert!(!has(&intel, amd_msr::LAST_BRANCH_FROM_IP));
        // AMD forwards the single pair, not the Intel stack.
        assert!(has(&amd, amd_msr::LAST_BRANCH_FROM_IP));
        assert!(has(&amd, amd_msr::LAST_INT_TO_IP));
        assert!(!has(&amd, intel_msr::LBR_FROM_BASE));
        // Both forward the platform-independent surfaces.
        for rs in [&intel, &amd] {
            assert!(has(rs, timing_msr::IA32_APERF));
            assert!(has(rs, intel_msr::IA32_DEBUGCTL));
            assert!(has(rs, pmc_msr::IA32_PERF_GLOBAL_STATUS_RESET));
        }
    }

    #[test]
    fn aperf_mperf_read_the_timing_shadows() {
        let r = router();
        r.timing.write_aperf(0x1111);
        r.timing.write_mperf(0x2222);
        assert_eq!(r.read_msr(timing_msr::IA32_APERF), Some(0x1111));
        assert_eq!(r.read_msr(timing_msr::IA32_MPERF), Some(0x2222));
    }

    #[test]
    fn aperf_mperf_writes_reach_the_shadows() {
        let mut r = router();
        assert!(r.write_msr(timing_msr::IA32_APERF, 7));
        assert!(r.write_msr(timing_msr::IA32_MPERF, 9));
        assert_eq!(r.timing.read_aperf(), 7);
        assert_eq!(r.timing.read_mperf(), 9);
    }

    #[test]
    fn debugctl_round_trips_and_toggles_lbr() {
        let mut r = router();
        assert!(r.write_msr(intel_msr::IA32_DEBUGCTL, 0x1));
        assert_eq!(r.read_msr(intel_msr::IA32_DEBUGCTL), Some(0x1));
        assert!(r.lbr.lbr_enabled);
        assert!(r.write_msr(intel_msr::IA32_DEBUGCTL, 0x0));
        assert!(!r.lbr.lbr_enabled);
    }

    #[test]
    fn lbr_from_to_info_arrays_are_addressable() {
        let mut r = router();
        // Plant values at index 3 of each LBR block, then read via MSR number.
        r.lbr.from_addresses[3] = 0xF00D;
        r.lbr.to_addresses[3] = 0xBEEF;
        r.lbr.info[3] = 0x42;
        assert_eq!(r.read_msr(intel_msr::LBR_FROM_BASE + 3), Some(0xF00D));
        assert_eq!(r.read_msr(intel_msr::LBR_TO_BASE + 3), Some(0xBEEF));
        assert_eq!(r.read_msr(intel_msr::LBR_INFO_BASE + 3), Some(0x42));
        // TOS is its own MSR.
        r.lbr.tos = 5;
        assert_eq!(r.read_msr(intel_msr::LBR_TOS), Some(5));
    }

    #[test]
    fn lbr_block_bounds_do_not_bleed_into_neighbours() {
        let r = router();
        // One past the 32-entry FROM block must not be served as an LBR MSR.
        assert_eq!(r.read_msr(intel_msr::LBR_FROM_BASE + LBR_COUNT), None);
    }

    #[test]
    fn pmc_reads_and_writes_route_to_the_pmc_state() {
        let mut r = router();
        assert!(r.write_msr(pmc_msr::IA32_PMC0 + 2, 12345));
        assert_eq!(r.read_msr(pmc_msr::IA32_PMC0 + 2), Some(12345));
        // The write-only reset MSR is recognised for writes even though it has
        // no readable value.
        assert!(StealthMsrRouter::is_pmc_msr(pmc_msr::IA32_PERF_GLOBAL_STATUS_RESET));
        assert!(r.write_msr(pmc_msr::IA32_PERF_GLOBAL_STATUS_RESET, 0));
    }

    #[test]
    fn rdpmc_fixed_counter_advances_through_the_router_state() {
        // The router owns the PMC state the run loop advances; a detector that
        // reads a fixed counter via its MSR sees the model-driven value.
        let mut r = router();
        r.pmc.fixed_ctr_ctrl = 0x30; // enable CPU_CLK_UNHALTED.THREAD (core)
        r.pmc.advance_counters(1000);
        // 1000 ref cycles -> 1150 core cycles under the default model.
        assert_eq!(r.read_msr(pmc_msr::IA32_FIXED_CTR0 + 1), Some(1150));
    }

    #[test]
    fn amd_last_branch_registers_route_through_the_lbr_state() {
        use enlil_devices::stealth::lbr::amd_msr;
        let mut r = router();
        // AMD's single last-branch pair (0x1DB/0x1DC) and last-interrupt pair
        // (0x1DD/0x1DE) round-trip through the router, distinct from the Intel
        // FROM/TO stack.
        assert!(r.write_msr(amd_msr::LAST_BRANCH_FROM_IP, 0xAAAA));
        assert!(r.write_msr(amd_msr::LAST_BRANCH_TO_IP, 0xBBBB));
        assert!(r.write_msr(amd_msr::LAST_INT_FROM_IP, 0xCCCC));
        assert!(r.write_msr(amd_msr::LAST_INT_TO_IP, 0xDDDD));
        assert_eq!(r.read_msr(amd_msr::LAST_BRANCH_FROM_IP), Some(0xAAAA));
        assert_eq!(r.read_msr(amd_msr::LAST_BRANCH_TO_IP), Some(0xBBBB));
        assert_eq!(r.read_msr(amd_msr::LAST_INT_FROM_IP), Some(0xCCCC));
        assert_eq!(r.read_msr(amd_msr::LAST_INT_TO_IP), Some(0xDDDD));
    }

    #[test]
    fn unmodelled_msr_falls_through() {
        let mut r = router();
        assert_eq!(r.read_msr(0xDEAD_BEEF), None);
        assert!(!r.write_msr(0xDEAD_BEEF, 1));
        // A standard but unrelated MSR (IA32_TSC) is not this router's concern.
        assert_eq!(r.read_msr(0x10), None);
    }

    #[test]
    fn aperf_mperf_ratio_matches_the_pmc_core_ref_ratio() {
        // Cross-surface consistency: drive both the timing shadows and the PMC
        // fixed counters with the same model + delta, then read each surface
        // through the router's MSR map. The APERF/MPERF ratio must equal the
        // RDPMC core/ref ratio (the detection cross-check the model defeats).
        let model = PmcRateModel::DEFAULT;
        let mut r = router();
        r.pmc.fixed_ctr_ctrl = 0x330; // core + ref fixed counters
        r.timing.advance(1_000_000, &model);
        r.pmc.advance_counters(1_000_000);

        let aperf = r.read_msr(timing_msr::IA32_APERF).unwrap();
        let mperf = r.read_msr(timing_msr::IA32_MPERF).unwrap();
        let core = r.read_msr(pmc_msr::IA32_FIXED_CTR0 + 1).unwrap();
        let reff = r.read_msr(pmc_msr::IA32_FIXED_CTR0 + 2).unwrap();

        assert_eq!(aperf, core, "APERF surface must equal the core fixed counter");
        assert_eq!(mperf, reff, "MPERF surface must equal the ref fixed counter");
        assert_ne!(aperf, mperf, "ratio must not be the 1.0 VM tell");
    }
}

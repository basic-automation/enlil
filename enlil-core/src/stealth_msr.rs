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
use enlil_devices::stealth::cpuid::CpuVendor;
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
    /// The CPU vendor the *guest* believes it runs on — the vendor of the
    /// CPUID identity install ([`CpuidStealthTable`](enlil_devices::stealth::cpuid::CpuidStealthTable)).
    /// Usually the physical platform's vendor; differs for a cross-vendor
    /// identity (e.g. Intel presented on an AMD host). The MSR filter and the
    /// read/write dispatch follow this vendor, so a guest cross-checking
    /// CPUID against `RDMSR` sees one coherent vendor instead of the physical
    /// host's registers — presenting Intel while serving AMD's MSR block (or
    /// vice versa) is itself a detection tell.
    pub emulated: CpuVendor,
}

impl StealthMsrRouter {
    /// Build a router over a shared timing state and an LBR state, with a fresh
    /// [`PmcState`]. The emulated vendor defaults to the LBR platform's vendor
    /// (same-vendor identity); use [`with_emulated_vendor`](Self::with_emulated_vendor)
    /// for a cross-vendor identity.
    #[must_use]
    pub fn new(timing: Arc<VcpuTimingState>, lbr: LbrState) -> Self {
        let emulated = match lbr.platform {
            LbrPlatform::IntelVmx => CpuVendor::Intel,
            LbrPlatform::AmdSvm => CpuVendor::Amd,
        };
        Self::with_emulated_vendor(timing, lbr, emulated)
    }

    /// Build a router that emulates `emulated`'s MSR surface — the second half
    /// of a cross-vendor identity switch (see
    /// [`CpuidStealthConfig::cross_vendor`](enlil_devices::stealth::cpuid::CpuidStealthConfig::cross_vendor)
    /// for the CPUID half). The caller passes the [`LbrState`] for the
    /// *presented* platform (its `platform` selects which LBR shadow shape the
    /// router sanitizes); `emulated` then selects which vendor's PMC and LBR
    /// MSR blocks the filter forwards and the dispatch serves.
    #[must_use]
    pub fn with_emulated_vendor(
        timing: Arc<VcpuTimingState>,
        lbr: LbrState,
        emulated: CpuVendor,
    ) -> Self {
        Self {
            timing,
            pmc: PmcState::new(),
            lbr,
            emulated,
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

    /// Whether `msr` is one of the Intel `IA32_*` performance-monitoring-counter
    /// MSRs the [`PmcState`] models (GP counters, event selects, fixed
    /// counters, and the global control/status block including the write-only
    /// `GLOBAL_STATUS_RESET`).
    #[must_use]
    pub const fn is_intel_pmc_msr(msr: u32) -> bool {
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

    /// Whether `msr` is one of the performance-monitoring-counter MSRs the
    /// [`PmcState`] models — the Intel `IA32_*` block (including the write-only
    /// `GLOBAL_STATUS_RESET`, which [`PmcState::read_msr`] cannot report) **or**
    /// the AMD legacy / `PerfMonV2` block. The union across vendors: the router
    /// only *serves* the emulated vendor's block (see [`read_msr`](Self::read_msr)).
    #[must_use]
    pub const fn is_pmc_msr(msr: u32) -> bool {
        Self::is_intel_pmc_msr(msr) || PmcState::is_amd_pmc_msr(msr)
    }

    /// Advance both stealth time surfaces by `ref_cycles` of guest execution.
    ///
    /// The run loop calls this once per guest entry (the reference-cycle delta
    /// being the in-guest TSC delta) so the guest sees its performance counters
    /// move forward. Crucially it drives *both* surfaces from the **same**
    /// rate model and the **same** delta — the APERF/MPERF shadows
    /// ([`VcpuTimingState::advance`](crate::timing_stealth::VcpuTimingState::advance)
    /// at the PMC state's `rate_model`) and the RDPMC fixed/GP counters
    /// ([`PmcState::advance_counters`]) — so a detector cross-checking
    /// APERF/MPERF against `CPU_CLK_UNHALTED.THREAD / REF_TSC` finds them
    /// consistent (the invariant [`PmcRateModel`] documents).
    ///
    /// [`PmcState::advance_counters`]: enlil_devices::stealth::pmc::PmcState::advance_counters
    /// [`PmcRateModel`]: enlil_devices::stealth::pmc::PmcRateModel
    pub fn advance(&mut self, ref_cycles: u64) {
        let model = self.pmc.rate_model;
        self.timing.advance(ref_cycles, &model);
        self.pmc.advance_counters(ref_cycles);
    }

    /// The MSR ranges this router answers, as `(base, count)` pairs, for the
    /// router's **emulated** vendor — the single source of truth to hand to
    /// [`KvmBackend::forward_msrs_to_userspace`] so the KVM filter and this
    /// router cover exactly the same MSRs.
    ///
    /// Timing (APERF/MPERF) and `IA32_DEBUGCTL` exist on both vendors and are
    /// always forwarded; the PMC **and** last-branch MSRs are vendor-specific.
    /// On an Intel-emulated identity we forward the Intel `IA32_*` PMC block
    /// and the TOS + FROM/TO/INFO LBR stack; on an AMD-emulated identity the
    /// AMD legacy / `PerfMonV2` PMC block and the single AMD last-branch pair
    /// (0x1DB–0x1DE). Forwarding the other vendor's PMC or LBR MSRs would make
    /// non-existent registers readable — itself a tell — so each identity
    /// forwards only what it really exposes. At most 11 ranges, within KVM's
    /// 16-range limit.
    ///
    /// [`KvmBackend::forward_msrs_to_userspace`]: crate::kvm_backend::KvmBackend::forward_msrs_to_userspace
    #[must_use]
    pub fn filter_ranges(&self) -> Vec<(u32, u32)> {
        let gp = MAX_GP_PMCS as u32;
        let fixed = MAX_FIXED_PMCS as u32;
        let mut ranges = vec![
            (timing_msr::IA32_MPERF, 2), // 0xE7 MPERF, 0xE8 APERF
            (intel_msr::IA32_DEBUGCTL, 1),
        ];
        match self.emulated {
            CpuVendor::Amd => {
                // AMD legacy (4) + interleaved core EvtSel/Ctr (12) + PerfMonV2
                // global block (status/ctl/clr — 3 consecutive).
                ranges.push((pmc_msr::AMD_LEGACY_PERFEVTSEL0, pmc_msr::AMD_LEGACY_PMCS));
                ranges.push((pmc_msr::AMD_LEGACY_PERFCTR0, pmc_msr::AMD_LEGACY_PMCS));
                ranges.push((pmc_msr::AMD_CORE_PERFEVTSEL0, 2 * pmc_msr::AMD_CORE_PMCS));
                ranges.push((pmc_msr::AMD_PERF_CNTR_GLOBAL_STATUS, 3));
                ranges.push((amd_msr::LAST_BRANCH_FROM_IP, 4)); // 0x1DB..=0x1DE
            }
            CpuVendor::Intel => {
                ranges.push((pmc_msr::IA32_PMC0, gp));
                ranges.push((pmc_msr::IA32_PERFEVTSEL0, gp));
                ranges.push((pmc_msr::IA32_FIXED_CTR0, fixed));
                // FIXED_CTR_CTRL (0x38D), GLOBAL_STATUS (0x38E), GLOBAL_CTRL
                // (0x38F), GLOBAL_STATUS_RESET (0x390) — four consecutive.
                ranges.push((pmc_msr::IA32_FIXED_CTR_CTRL, 4));
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
    ///
    /// The vendor-specific surfaces follow the **emulated** vendor: an
    /// Intel-emulated identity serves the `IA32_*` PMC block and the LBR
    /// stack; an AMD-emulated identity the AMD PMC block and the last-branch
    /// pair. The other vendor's MSRs read as unmodelled (`None`) — exactly
    /// like the `#GP` real hardware raises for registers it does not have.
    #[must_use]
    pub fn read_msr(&self, msr: u32) -> Option<u64> {
        match msr {
            timing_msr::IA32_APERF => Some(self.timing.read_aperf()),
            timing_msr::IA32_MPERF => Some(self.timing.read_mperf()),
            // IA32_DEBUGCTL (0x1D9) exists on both vendors.
            intel_msr::IA32_DEBUGCTL => Some(self.lbr.read_debug_ctl()),
            _ => match self.emulated {
                CpuVendor::Intel => {
                    if msr == intel_msr::LBR_TOS {
                        Some(self.lbr.read_tos())
                    } else if let Some(i) = Self::lbr_from_index(msr) {
                        Some(self.lbr.read_from(i))
                    } else if let Some(i) = Self::lbr_to_index(msr) {
                        Some(self.lbr.read_to(i))
                    } else if let Some(i) = Self::lbr_info_index(msr) {
                        Some(self.lbr.read_info(i))
                    } else if Self::is_intel_pmc_msr(msr) {
                        self.pmc.read_msr(msr)
                    } else {
                        None
                    }
                }
                CpuVendor::Amd => {
                    if let Some(v) = self.lbr.read_amd_lbr(msr) {
                        Some(v)
                    } else if PmcState::is_amd_pmc_msr(msr) {
                        self.pmc.read_msr(msr)
                    } else {
                        None
                    }
                }
            },
        }
    }

    /// Serve a guest `WRMSR`. Returns `true` if this router models `msr` (and
    /// applied the write), or `false` to let the caller fall through.
    ///
    /// Like [`read_msr`](Self::read_msr), the vendor-specific surfaces follow
    /// the **emulated** vendor; writes to the other vendor's MSRs fall through.
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
            // IA32_DEBUGCTL (0x1D9) exists on both vendors.
            intel_msr::IA32_DEBUGCTL => {
                self.lbr.write_debug_ctl(value);
                true
            }
            m => match self.emulated {
                CpuVendor::Intel => {
                    if Self::is_intel_pmc_msr(m) {
                        self.pmc.write_msr(m, value);
                        true
                    } else {
                        false
                    }
                }
                CpuVendor::Amd => {
                    if self.lbr.write_amd_lbr(m, value) {
                        true
                    } else if PmcState::is_amd_pmc_msr(m) {
                        self.pmc.write_msr(m, value);
                        true
                    } else {
                        false
                    }
                }
            },
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

    /// A router on `platform` hardware emulating `emulated`'s MSR surface —
    /// the cross-vendor identity case (e.g. Intel presented on AMD hardware).
    fn cross_vendor_router(platform: LbrPlatform, emulated: CpuVendor) -> StealthMsrRouter {
        StealthMsrRouter::with_emulated_vendor(
            VcpuTimingState::new(),
            LbrState::new(platform),
            emulated,
        )
    }

    #[test]
    fn emulated_vendor_defaults_to_the_platform_vendor() {
        assert_eq!(router_for(LbrPlatform::IntelVmx).emulated, CpuVendor::Intel);
        assert_eq!(router_for(LbrPlatform::AmdSvm).emulated, CpuVendor::Amd);
        assert_eq!(
            cross_vendor_router(LbrPlatform::AmdSvm, CpuVendor::Intel).emulated,
            CpuVendor::Intel
        );
    }

    #[test]
    fn filter_ranges_cover_exactly_what_the_router_serves() {
        // Same-vendor and both cross-vendor combinations: whatever the filter
        // forwards, the router must answer (via read or write).
        let combos = [
            (LbrPlatform::IntelVmx, CpuVendor::Intel),
            (LbrPlatform::AmdSvm, CpuVendor::Amd),
            (LbrPlatform::AmdSvm, CpuVendor::Intel),
            (LbrPlatform::IntelVmx, CpuVendor::Amd),
        ];
        for (platform, emulated) in combos {
            let mut r = cross_vendor_router(platform, emulated);
            let ranges = r.filter_ranges();
            assert!(ranges.len() <= 16, "within KVM's 16-range limit");
            // Every MSR the filter forwards must be one the router answers
            // (via read or write) — otherwise we'd forward an MSR only to #GP
            // it, or model an MSR we never forward.
            for (base, count) in ranges {
                for msr in base..base + count {
                    let served = r.read_msr(msr).is_some() || r.write_msr(msr, 0);
                    assert!(
                        served,
                        "router must serve filtered MSR {msr:#x} on {platform:?}/{emulated:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn filter_ranges_are_platform_correct() {
        let intel = router_for(LbrPlatform::IntelVmx).filter_ranges();
        let amd = router_for(LbrPlatform::AmdSvm).filter_ranges();
        let has = |rs: &[(u32, u32)], msr: u32| rs.iter().any(|&(b, c)| msr >= b && msr < b + c);
        // Intel forwards the 32-entry stack + TOS, not the AMD pair.
        assert!(has(&intel, intel_msr::LBR_FROM_BASE));
        assert!(has(&intel, intel_msr::LBR_TOS));
        assert!(!has(&intel, amd_msr::LAST_BRANCH_FROM_IP));
        // AMD forwards the single pair, not the Intel stack.
        assert!(has(&amd, amd_msr::LAST_BRANCH_FROM_IP));
        assert!(has(&amd, amd_msr::LAST_INT_TO_IP));
        assert!(!has(&amd, intel_msr::LBR_FROM_BASE));
        // PMC MSRs are vendor-specific: Intel forwards the IA32 block, AMD the
        // legacy/PerfMonV2 block — never the other vendor's (a non-existent
        // register a guest could read is itself a tell).
        assert!(has(&intel, pmc_msr::IA32_PERF_GLOBAL_STATUS_RESET));
        assert!(has(&intel, pmc_msr::IA32_PMC0));
        assert!(!has(&intel, pmc_msr::AMD_CORE_PERFCTR0));
        assert!(has(&amd, pmc_msr::AMD_CORE_PERFCTR0));
        assert!(has(&amd, pmc_msr::AMD_LEGACY_PERFEVTSEL0));
        assert!(has(&amd, pmc_msr::AMD_PERF_CNTR_GLOBAL_CTL));
        assert!(!has(&amd, pmc_msr::IA32_PMC0));
        // Both forward the platform-independent surfaces.
        for rs in [&intel, &amd] {
            assert!(has(rs, timing_msr::IA32_APERF));
            assert!(has(rs, intel_msr::IA32_DEBUGCTL));
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
        assert!(StealthMsrRouter::is_pmc_msr(
            pmc_msr::IA32_PERF_GLOBAL_STATUS_RESET
        ));
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
    fn amd_pmc_msrs_route_to_the_pmc_state() {
        // An AMD-presented guest's PerfMonV2 counter writes/reads round-trip
        // through the router's PmcState, just like the Intel block.
        let mut r = router_for(LbrPlatform::AmdSvm);
        assert!(r.write_msr(pmc_msr::AMD_CORE_PERFCTR0 + 2, 0xCAFE)); // PerfCtr1
        assert_eq!(r.read_msr(pmc_msr::AMD_CORE_PERFCTR0 + 2), Some(0xCAFE));
        assert!(r.write_msr(pmc_msr::AMD_PERF_CNTR_GLOBAL_CTL, 0b11));
        assert_eq!(r.read_msr(pmc_msr::AMD_PERF_CNTR_GLOBAL_CTL), Some(0b11));
        // The write-only clear MSR is recognised for writes.
        assert!(r.write_msr(pmc_msr::AMD_PERF_CNTR_GLOBAL_STATUS_CLR, 0));
    }

    #[test]
    fn amd_last_branch_registers_route_through_the_lbr_state() {
        use enlil_devices::stealth::lbr::amd_msr;
        // An AMD-emulated router serves AMD's single last-branch pair
        // (0x1DB/0x1DC) and last-interrupt pair (0x1DD/0x1DE), distinct from
        // the Intel FROM/TO stack.
        let mut r = router_for(LbrPlatform::AmdSvm);
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
    fn cross_vendor_intel_on_amd_serves_only_the_intel_surface() {
        use enlil_devices::stealth::lbr::{amd_msr, intel_msr};
        // Intel presented on AMD hardware: the router emulates Intel's MSR
        // surface even though the LBR platform is AMD.
        let mut r = cross_vendor_router(LbrPlatform::AmdSvm, CpuVendor::Intel);

        // The Intel PMC block round-trips.
        assert!(r.write_msr(pmc_msr::IA32_PMC0, 0xCAFE));
        assert_eq!(r.read_msr(pmc_msr::IA32_PMC0), Some(0xCAFE));
        assert!(r.write_msr(pmc_msr::IA32_PERFEVTSEL0, 0x4300C1));
        // The LBR stack reads through the shadow arrays (writes to the stack
        // fall through, as on the same-vendor path — the router never claimed
        // them).
        r.lbr.from_addresses[3] = 0xF00D;
        assert_eq!(r.read_msr(intel_msr::LBR_FROM_BASE + 3), Some(0xF00D));
        assert!(!r.write_msr(intel_msr::LBR_FROM_BASE + 3, 1));
        assert_eq!(r.read_msr(intel_msr::LBR_TOS), Some(0));

        // The AMD surface is gone: reads fall through (the guest's #GP, like
        // real Intel hardware), writes are refused.
        for msr in [
            amd_msr::LAST_BRANCH_FROM_IP,
            amd_msr::LAST_INT_TO_IP,
            pmc_msr::AMD_CORE_PERFCTR0,
            pmc_msr::AMD_LEGACY_PERFEVTSEL0,
            pmc_msr::AMD_PERF_CNTR_GLOBAL_CTL,
        ] {
            assert_eq!(r.read_msr(msr), None, "AMD MSR {msr:#x} must #GP");
            assert!(!r.write_msr(msr, 1), "AMD MSR {msr:#x} must #GP");
        }

        // The filter follows the emulated vendor too.
        let ranges = r.filter_ranges();
        let has = |msr: u32| ranges.iter().any(|&(b, c)| msr >= b && msr < b + c);
        assert!(has(pmc_msr::IA32_PMC0));
        assert!(has(intel_msr::LBR_FROM_BASE));
        assert!(!has(pmc_msr::AMD_CORE_PERFCTR0));
        assert!(!has(amd_msr::LAST_BRANCH_FROM_IP));

        // The vendor-neutral surfaces stay up on both identities.
        assert!(r.write_msr(timing_msr::IA32_APERF, 11));
        assert_eq!(r.read_msr(timing_msr::IA32_APERF), Some(11));
        assert!(r.write_msr(intel_msr::IA32_DEBUGCTL, 1));
        assert_eq!(r.read_msr(intel_msr::IA32_DEBUGCTL), Some(1));
    }

    #[test]
    fn cross_vendor_amd_on_intel_serves_only_the_amd_surface() {
        use enlil_devices::stealth::lbr::{amd_msr, intel_msr};
        // AMD presented on Intel hardware: the mirror image.
        let mut r = cross_vendor_router(LbrPlatform::IntelVmx, CpuVendor::Amd);

        assert!(r.write_msr(pmc_msr::AMD_CORE_PERFCTR0 + 2, 0xCAFE));
        assert_eq!(r.read_msr(pmc_msr::AMD_CORE_PERFCTR0 + 2), Some(0xCAFE));
        assert!(r.write_msr(amd_msr::LAST_BRANCH_FROM_IP, 0xAAAA));
        assert_eq!(r.read_msr(amd_msr::LAST_BRANCH_FROM_IP), Some(0xAAAA));

        for msr in [
            intel_msr::LBR_FROM_BASE,
            intel_msr::LBR_TOS,
            pmc_msr::IA32_PMC0,
            pmc_msr::IA32_PERFEVTSEL0,
            pmc_msr::IA32_FIXED_CTR0,
        ] {
            assert_eq!(r.read_msr(msr), None, "Intel MSR {msr:#x} must #GP");
            assert!(!r.write_msr(msr, 1), "Intel MSR {msr:#x} must #GP");
        }

        let ranges = r.filter_ranges();
        let has = |msr: u32| ranges.iter().any(|&(b, c)| msr >= b && msr < b + c);
        assert!(has(pmc_msr::AMD_CORE_PERFCTR0));
        assert!(has(amd_msr::LAST_BRANCH_FROM_IP));
        assert!(!has(pmc_msr::IA32_PMC0));
        assert!(!has(intel_msr::LBR_FROM_BASE));
    }

    #[test]
    fn advance_drives_both_surfaces_consistently() {
        // One advance() call moves the APERF/MPERF shadows and the RDPMC fixed
        // counters together; reading each back through the MSR map, the two
        // surfaces agree exactly (core↔APERF, ref↔MPERF) and the ratio is not
        // the 1.0 tell.
        let mut r = router();
        r.pmc.fixed_ctr_ctrl = 0x330; // enable core + ref fixed counters

        r.advance(1_000_000);

        let aperf = r.read_msr(timing_msr::IA32_APERF).unwrap();
        let mperf = r.read_msr(timing_msr::IA32_MPERF).unwrap();
        let core = r.read_msr(pmc_msr::IA32_FIXED_CTR0 + 1).unwrap();
        let reff = r.read_msr(pmc_msr::IA32_FIXED_CTR0 + 2).unwrap();
        assert_eq!(aperf, core, "APERF surface == RDPMC core counter");
        assert_eq!(mperf, reff, "MPERF surface == RDPMC ref counter");
        assert_ne!(aperf, mperf, "ratio is not the 1.0 tell");

        // A second tick keeps them in lockstep.
        r.advance(500_000);
        assert_eq!(
            r.read_msr(timing_msr::IA32_APERF).unwrap(),
            r.read_msr(pmc_msr::IA32_FIXED_CTR0 + 1).unwrap()
        );
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

        assert_eq!(
            aperf, core,
            "APERF surface must equal the core fixed counter"
        );
        assert_eq!(
            mperf, reff,
            "MPERF surface must equal the ref fixed counter"
        );
        assert_ne!(aperf, mperf, "ratio must not be the 1.0 VM tell");
    }
}

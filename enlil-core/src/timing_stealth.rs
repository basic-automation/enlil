//! Timing Stealth — Hide VM exit overhead
//!
//! Implements APERF/MPERF shadow counters, TSC offsetting, and LBR sanitization
//! to defeat IET divergence detection and timing-based VM detectors.

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

    /// Called before VMRESUME: account for exit time and adjust shadow counters
    pub fn on_vmresume(&self, entry_tsc: u64, guest_rip: u64) {
        let last_tsc = self.last_exit_tsc.load(Ordering::Acquire);
        let exit_overhead_cycles = entry_tsc.saturating_sub(last_tsc);

        // Subtract exit overhead from shadow APERF to hide hypervisor time
        let current_aperf = self.aperf.load(Ordering::Relaxed);
        self.aperf.store(
            current_aperf.saturating_sub(exit_overhead_cycles),
            Ordering::Release,
        );

        // Keep MPERF proportional (APERF/MPERF ratio ~1.0 for high performance)
        let current_mperf = self.mperf.load(Ordering::Relaxed);
        if current_mperf > 0 {
            let ratio = (current_aperf as f64) / (current_mperf as f64);
            self.mperf
                .store((current_aperf as f64 / ratio) as u64, Ordering::Release);
        }

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

/// LBR (Last Branch Record) sanitization for detection evasion
pub struct LbrSanitizer {
    /// The branch target that points back into the guest (should be hidden)
    pub expected_guest_branch_target: u64,
}

impl Default for LbrSanitizer {
    fn default() -> Self {
        Self::new()
    }
}

impl LbrSanitizer {
    pub fn new() -> Self {
        Self {
            expected_guest_branch_target: 0,
        }
    }

    /// Sanitize LBR stack after a detected VMEXIT (e.g., via CPUID trap)
    /// Removes or falsifies the branch record that shows branch-to-hypervisor
    pub fn sanitize_lbr(&self, lbr_stack: &mut [(u64, u64)], guest_rip: u64) {
        if let Some((from, _to)) = lbr_stack.last_mut() {
            // The most recent LBR entry shows: from=guest_instruction, to=hypervisor_entry
            // Replace the "to" with the next expected guest instruction (to hide the VMEXIT)
            *from = guest_rip;
        }
    }
}

/// CPUID response pre-computation for constant-time handling
pub struct CpuidCachingHelper {
    /// Pre-computed CPUID responses
    cache: Vec<CpuidResponse>,
}

#[derive(Clone, Debug)]
pub struct CpuidResponse {
    pub leaf: u32,
    pub subleaf: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

impl Default for CpuidCachingHelper {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuidCachingHelper {
    pub fn new() -> Self {
        Self { cache: Vec::new() }
    }

    /// Build CPUID cache for all relevant leaves
    /// Should be called at guest boot with stealth values pre-computed
    pub fn cache_cpuid(&mut self, response: CpuidResponse) {
        self.cache.push(response);
    }

    /// Lookup CPUID response in cache (< 100 cycles)
    pub fn lookup(&self, leaf: u32, subleaf: u32) -> Option<CpuidResponse> {
        self.cache
            .iter()
            .find(|r| r.leaf == leaf && r.subleaf == subleaf)
            .cloned()
    }

    /// Pre-populate with Intel stealth values
    pub fn populate_intel_stealth(&mut self) {
        // Leaf 0x00: Vendor string
        self.cache_cpuid(CpuidResponse {
            leaf: 0x00,
            subleaf: 0,
            eax: 0x16,       // Max leaf
            ebx: 0x756e6547, // "Genu"
            ecx: 0x6c65746e, // "ntel"
            edx: 0x49656e69, // "ineI"
        });

        // Leaf 0x01: Feature flags (clear hypervisor bit)
        self.cache_cpuid(CpuidResponse {
            leaf: 0x01,
            subleaf: 0,
            eax: 0x000006c2, // Family 6, Model C, Stepping 2
            ebx: 0x01100800,
            ecx: 0x7fffbfff, // ECX bit 31 (hypervisor bit) = 0
            edx: 0xbfebfbff,
        });

        // Leaf 0x40000000+: Return zeros (no hypervisor signature)
        for i in 0..16 {
            self.cache_cpuid(CpuidResponse {
                leaf: 0x40000000 + i,
                subleaf: 0,
                eax: 0,
                ebx: 0,
                ecx: 0,
                edx: 0,
            });
        }
    }
}

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
    fn test_tsc_offset_calculation() {
        let mut helper = TscOffsetHelper::new(0);
        helper.calculate_offset(1000, 2000);
        assert_eq!(helper.tsc_offset, 1000);
    }

    #[test]
    fn test_cpuid_caching() {
        let mut cache = CpuidCachingHelper::new();
        cache.populate_intel_stealth();
        let resp = cache.lookup(0x01, 0).unwrap();
        // Hypervisor bit should be 0
        assert_eq!(resp.ecx & (1 << 31), 0);
    }
}

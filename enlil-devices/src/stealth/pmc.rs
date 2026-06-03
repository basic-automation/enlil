//! PMC (Performance Monitoring Counter) virtualization
//!
//! Some detectors use RDPMC to read hardware performance counters.
//! We virtualize PMC access to return consistent values that don't
//! reveal VMEXIT overhead.

use crate::truncate::u32_of;
/// Maximum number of general-purpose PMCs to virtualize
pub const MAX_GP_PMCS: usize = 8;
/// Maximum number of fixed-function PMCs
pub const MAX_FIXED_PMCS: usize = 4;

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
    pub fn advance_counters(&mut self, guest_cycles: u64) {
        for (i, &event) in self.event_select.iter().enumerate() {
            if event != 0 && (self.global_ctrl & (1u64 << i)) != 0 {
                // Simple model: advance proportionally to cycles
                self.gp_counters[i] = self.gp_counters[i].wrapping_add(guest_cycles);
            }
        }

        // Fixed counters
        // Counter 0: Instructions retired (approximate as cycles * IPC estimate)
        if (self.fixed_ctr_ctrl & 0x3) != 0 {
            self.fixed_counters[0] = self.fixed_counters[0].wrapping_add(guest_cycles);
        }
        // Counter 1: Core cycles
        if (self.fixed_ctr_ctrl & 0x30) != 0 {
            self.fixed_counters[1] = self.fixed_counters[1].wrapping_add(guest_cycles);
        }
        // Counter 2: Reference cycles
        if (self.fixed_ctr_ctrl & 0x300) != 0 {
            self.fixed_counters[2] = self.fixed_counters[2].wrapping_add(guest_cycles);
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

        pmc.advance_counters(1000);
        assert_eq!(pmc.gp_counters[0], 1000);

        pmc.advance_counters(500);
        assert_eq!(pmc.gp_counters[0], 1500);
    }

    #[test]
    fn advance_fixed_counters() {
        let mut pmc = PmcState::new();
        // Enable fixed counter 1 (core cycles)
        pmc.fixed_ctr_ctrl = 0x30;

        pmc.advance_counters(2000);
        assert_eq!(pmc.fixed_counters[1], 2000);
        assert_eq!(pmc.fixed_counters[0], 0); // Not enabled
    }

    #[test]
    fn global_status_reset() {
        let mut pmc = PmcState::new();
        pmc.global_status = 0xFF;
        pmc.write_msr(msr::IA32_PERF_GLOBAL_STATUS_RESET, 0x0F);
        assert_eq!(pmc.global_status, 0xF0);
    }
}

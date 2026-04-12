//! LBR (Last Branch Record) save/restore for stealth
//!
//! Anti-cheats check the LBR stack after forcing a VMEXIT (via CPUID) to detect
//! that a branch to the hypervisor occurred. If the last branch target doesn't
//! match the expected next instruction after CPUID, a hypervisor is detected.
//!
//! Intel: VMCS VM-exit/entry controls bit 22 for automatic LBR save/restore
//! AMD: SVM LBRV (LBR Virtualization) in VMCB control area

/// Number of LBR FROM/TO pairs (Intel Skylake+)
pub const LBR_STACK_SIZE: usize = 32;

/// Intel LBR MSR addresses
pub mod intel_msr {
    /// LBR FROM MSRs (0x680-0x69F for 32 entries)
    pub const LBR_FROM_BASE: u32 = 0x680;
    /// LBR TO MSRs (0x6C0-0x6DF for 32 entries)
    pub const LBR_TO_BASE: u32 = 0x6C0;
    /// LBR info MSRs (0xDC0-0xDDF for 32 entries)
    pub const LBR_INFO_BASE: u32 = 0xDC0;
    /// LBR TOS (Top of Stack pointer)
    pub const LBR_TOS: u32 = 0x1C9;
    /// DebugCtl MSR (enables LBR recording)
    pub const IA32_DEBUGCTL: u32 = 0x1D9;
}

/// AMD LBR MSR addresses (for LBRV)
pub mod amd_msr {
    pub const DEBUG_CTL: u32 = 0x1D9;
    pub const LAST_BRANCH_FROM_IP: u32 = 0x1DB;
    pub const LAST_BRANCH_TO_IP: u32 = 0x1DC;
    pub const LAST_INT_FROM_IP: u32 = 0x1DD;
    pub const LAST_INT_TO_IP: u32 = 0x1DE;
}

/// Per-vCPU LBR state
#[derive(Debug, Clone)]
pub struct LbrState {
    /// LBR FROM addresses (branch source)
    pub from_addresses: [u64; LBR_STACK_SIZE],
    /// LBR TO addresses (branch target)
    pub to_addresses: [u64; LBR_STACK_SIZE],
    /// LBR info (cycle count, misprediction, etc.)
    pub info: [u64; LBR_STACK_SIZE],
    /// Top of stack pointer (index into the circular buffer)
    pub tos: u32,
    /// IA32_DEBUGCTL shadow value
    pub debug_ctl: u64,
    /// Whether LBR recording is enabled by guest
    pub lbr_enabled: bool,
    /// Platform type for save/restore strategy
    pub platform: LbrPlatform,
}

/// LBR platform type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LbrPlatform {
    /// Intel VMX with VMCS LBR save/restore controls
    IntelVmx,
    /// AMD SVM with LBRV support
    AmdSvm,
}

impl LbrState {
    #[must_use]
    pub fn new(platform: LbrPlatform) -> Self {
        Self {
            from_addresses: [0; LBR_STACK_SIZE],
            to_addresses: [0; LBR_STACK_SIZE],
            info: [0; LBR_STACK_SIZE],
            tos: 0,
            debug_ctl: 0,
            lbr_enabled: false,
            platform,
        }
    }

    /// Sanitize the LBR stack after a VMEXIT.
    ///
    /// The most recent LBR entry will contain the branch from guest code
    /// into the hypervisor's VMEXIT handler. This must be removed or the
    /// guest can detect the hypervisor by inspecting its own LBR stack.
    pub fn sanitize_after_exit(&mut self, guest_rip: u64) {
        if !self.lbr_enabled {
            return;
        }

        let tos_idx = self.tos as usize % LBR_STACK_SIZE;

        // The top-of-stack entry was just written by the VMEXIT.
        // It contains: FROM = guest code, TO = hypervisor entry point.
        // We need to either:
        // 1. Remove it (decrement TOS), or
        // 2. Overwrite it with a plausible guest-to-guest branch
        //
        // Strategy: overwrite the TO address with the instruction after
        // the one that caused the VMEXIT, making it look like a normal
        // sequential execution (no branch actually happened).
        self.to_addresses[tos_idx] = guest_rip;
        self.from_addresses[tos_idx] = guest_rip;
        // Clear the info field (cycle count would reveal the anomaly)
        self.info[tos_idx] = 0;
    }

    /// Handle RDMSR for IA32_DEBUGCTL
    #[must_use]
    pub const fn read_debug_ctl(&self) -> u64 {
        self.debug_ctl
    }

    /// Handle WRMSR for IA32_DEBUGCTL
    pub fn write_debug_ctl(&mut self, value: u64) {
        self.debug_ctl = value;
        // LBR is enabled when bit 0 is set
        self.lbr_enabled = (value & 1) != 0;
    }

    /// Handle RDMSR for LBR TOS
    #[must_use]
    pub const fn read_tos(&self) -> u64 {
        self.tos as u64
    }

    /// Handle RDMSR for LBR FROM[index]
    #[must_use]
    pub fn read_from(&self, index: usize) -> u64 {
        if index < LBR_STACK_SIZE {
            self.from_addresses[index]
        } else {
            0
        }
    }

    /// Handle RDMSR for LBR TO[index]
    #[must_use]
    pub fn read_to(&self, index: usize) -> u64 {
        if index < LBR_STACK_SIZE {
            self.to_addresses[index]
        } else {
            0
        }
    }

    /// Handle RDMSR for LBR INFO[index]
    #[must_use]
    pub fn read_info(&self, index: usize) -> u64 {
        if index < LBR_STACK_SIZE {
            self.info[index]
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_clean() {
        let lbr = LbrState::new(LbrPlatform::IntelVmx);
        assert!(!lbr.lbr_enabled);
        assert_eq!(lbr.tos, 0);
        for i in 0..LBR_STACK_SIZE {
            assert_eq!(lbr.from_addresses[i], 0);
            assert_eq!(lbr.to_addresses[i], 0);
        }
    }

    #[test]
    fn debug_ctl_enables_lbr() {
        let mut lbr = LbrState::new(LbrPlatform::IntelVmx);
        lbr.write_debug_ctl(0x01);
        assert!(lbr.lbr_enabled);
        lbr.write_debug_ctl(0x00);
        assert!(!lbr.lbr_enabled);
    }

    #[test]
    fn sanitize_removes_hypervisor_branch() {
        let mut lbr = LbrState::new(LbrPlatform::IntelVmx);
        lbr.write_debug_ctl(1); // Enable LBR

        // Simulate a VMEXIT: hardware wrote a branch record
        lbr.tos = 5;
        lbr.from_addresses[5] = 0x7FFF_1234; // guest instruction
        lbr.to_addresses[5] = 0xFFFF_8800_0001_0000; // hypervisor entry
        lbr.info[5] = 42; // cycle count

        // Sanitize
        let guest_next_rip = 0x7FFF_1236;
        lbr.sanitize_after_exit(guest_next_rip);

        // The TO address should now be the guest RIP, not hypervisor
        assert_eq!(lbr.to_addresses[5], guest_next_rip);
        assert_eq!(lbr.info[5], 0); // cycle count cleared
    }

    #[test]
    fn sanitize_noop_when_disabled() {
        let mut lbr = LbrState::new(LbrPlatform::IntelVmx);
        // LBR not enabled — sanitize should not modify anything
        lbr.tos = 0;
        lbr.from_addresses[0] = 0xDEAD;
        lbr.to_addresses[0] = 0xBEEF;

        lbr.sanitize_after_exit(0x1234);

        // Should be unchanged
        assert_eq!(lbr.from_addresses[0], 0xDEAD);
        assert_eq!(lbr.to_addresses[0], 0xBEEF);
    }

    #[test]
    fn amd_platform() {
        let lbr = LbrState::new(LbrPlatform::AmdSvm);
        assert_eq!(lbr.platform, LbrPlatform::AmdSvm);
    }
}

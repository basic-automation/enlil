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
    /// `DebugCtl` MSR (enables LBR recording)
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
    /// `IA32_DEBUGCTL` shadow value
    pub debug_ctl: u64,
    /// Whether LBR recording is enabled by guest
    pub lbr_enabled: bool,
    /// Platform type for save/restore strategy
    pub platform: LbrPlatform,
    /// AMD `LastBranchFromIP` (0x1DB) — branch source. AMD's basic LBRV
    /// exposes a single last-branch register pair (plus a last-interrupt pair)
    /// rather than Intel's 32-entry stack, so these four registers are the AMD
    /// equivalent of the FROM/TO arrays above.
    pub last_branch_from_ip: u64,
    /// AMD `LastBranchToIP` (0x1DC) — branch target.
    pub last_branch_to_ip: u64,
    /// AMD `LastIntFromIP` (0x1DD) — source of the last interrupt/exception.
    pub last_int_from_ip: u64,
    /// AMD `LastIntToIP` (0x1DE) — target of the last interrupt/exception.
    pub last_int_to_ip: u64,
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
    pub const fn new(platform: LbrPlatform) -> Self {
        Self {
            from_addresses: [0; LBR_STACK_SIZE],
            to_addresses: [0; LBR_STACK_SIZE],
            info: [0; LBR_STACK_SIZE],
            tos: 0,
            debug_ctl: 0,
            lbr_enabled: false,
            platform,
            last_branch_from_ip: 0,
            last_branch_to_ip: 0,
            last_int_from_ip: 0,
            last_int_to_ip: 0,
        }
    }

    /// Sanitize the last-branch record after a VMEXIT.
    ///
    /// The most recent branch record will contain the branch from guest code
    /// into the hypervisor's VMEXIT handler. This must be erased or the guest
    /// can detect the hypervisor by inspecting its own LBR state. The save
    /// mechanism differs per platform (Intel's 32-entry MSR stack vs AMD's
    /// single LastBranchFrom/ToIP pair), but the sanitization is the same idea:
    /// overwrite the branch endpoints with `guest_rip` so it reads as ordinary
    /// sequential execution, with no branch into the hypervisor.
    pub const fn sanitize_after_exit(&mut self, guest_rip: u64) {
        if !self.lbr_enabled {
            return;
        }

        match self.platform {
            LbrPlatform::IntelVmx => {
                let tos_idx = self.tos as usize % LBR_STACK_SIZE;
                // The top-of-stack entry was just written by the VMEXIT:
                // FROM = guest code, TO = hypervisor entry point. Overwrite it
                // with a self-branch (no branch actually happened) and clear
                // the info field (its cycle count would reveal the anomaly).
                self.to_addresses[tos_idx] = guest_rip;
                self.from_addresses[tos_idx] = guest_rip;
                self.info[tos_idx] = 0;
            }
            LbrPlatform::AmdSvm => {
                // AMD saves a single last-branch pair; #VMEXIT leaves
                // LastBranchToIP pointing into the hypervisor. Erase the pair
                // (the last-interrupt pair is left alone — the exit is not an
                // architecturally-visible guest interrupt).
                self.last_branch_from_ip = guest_rip;
                self.last_branch_to_ip = guest_rip;
            }
        }
    }

    /// Handle RDMSR for an AMD last-branch/last-interrupt register
    /// (`LastBranchFromIP`/`ToIP`, `LastIntFromIP`/`ToIP`). Returns `None` for
    /// any other MSR.
    #[must_use]
    pub const fn read_amd_lbr(&self, msr: u32) -> Option<u64> {
        match msr {
            amd_msr::LAST_BRANCH_FROM_IP => Some(self.last_branch_from_ip),
            amd_msr::LAST_BRANCH_TO_IP => Some(self.last_branch_to_ip),
            amd_msr::LAST_INT_FROM_IP => Some(self.last_int_from_ip),
            amd_msr::LAST_INT_TO_IP => Some(self.last_int_to_ip),
            _ => None,
        }
    }

    /// Handle WRMSR for an AMD last-branch/last-interrupt register. Returns
    /// `true` if `msr` was one of them (and the write was applied).
    pub const fn write_amd_lbr(&mut self, msr: u32, value: u64) -> bool {
        match msr {
            amd_msr::LAST_BRANCH_FROM_IP => self.last_branch_from_ip = value,
            amd_msr::LAST_BRANCH_TO_IP => self.last_branch_to_ip = value,
            amd_msr::LAST_INT_FROM_IP => self.last_int_from_ip = value,
            amd_msr::LAST_INT_TO_IP => self.last_int_to_ip = value,
            _ => return false,
        }
        true
    }

    /// Handle RDMSR for `IA32_DEBUGCTL`
    #[must_use]
    pub const fn read_debug_ctl(&self) -> u64 {
        self.debug_ctl
    }

    /// Handle WRMSR for `IA32_DEBUGCTL`
    pub const fn write_debug_ctl(&mut self, value: u64) {
        // The high 48 bits [63:16] of IA32_DEBUGCTL are reserved on both Intel
        // (SDM Vol.3 §18.4.1) and AMD (APM Vol.2) — every defined control bit
        // lives in [15:0] (and which of those are valid differs by vendor, so we
        // keep them all). Storing a guest's reserved bits verbatim and reading
        // them back is a reserved-bit-writeback tell on a register anti-cheat
        // reads to probe for LBR/BTF tampering; real hardware reads them as 0.
        self.debug_ctl = value & 0x0000_0000_0000_FFFF;
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
    pub const fn read_from(&self, index: usize) -> u64 {
        if index < LBR_STACK_SIZE {
            self.from_addresses[index]
        } else {
            0
        }
    }

    /// Handle RDMSR for LBR TO[index]
    #[must_use]
    pub const fn read_to(&self, index: usize) -> u64 {
        if index < LBR_STACK_SIZE {
            self.to_addresses[index]
        } else {
            0
        }
    }

    /// Handle RDMSR for LBR INFO[index]
    #[must_use]
    pub const fn read_info(&self, index: usize) -> u64 {
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
    fn debug_ctl_masks_reserved_high_bits() {
        let mut lbr = LbrState::new(LbrPlatform::IntelVmx);
        // A guest writes all-ones: the defined low-16 bits take (LBR enabled),
        // but the reserved bits [63:16] read back 0 as on real hardware.
        lbr.write_debug_ctl(u64::MAX);
        assert_eq!(lbr.read_debug_ctl(), 0x0000_0000_0000_FFFF);
        assert!(lbr.lbr_enabled);
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

    #[test]
    fn amd_lbr_msrs_read_and_write() {
        let mut lbr = LbrState::new(LbrPlatform::AmdSvm);
        assert!(lbr.write_amd_lbr(amd_msr::LAST_BRANCH_FROM_IP, 0x1111));
        assert!(lbr.write_amd_lbr(amd_msr::LAST_BRANCH_TO_IP, 0x2222));
        assert!(lbr.write_amd_lbr(amd_msr::LAST_INT_FROM_IP, 0x3333));
        assert!(lbr.write_amd_lbr(amd_msr::LAST_INT_TO_IP, 0x4444));
        assert_eq!(lbr.read_amd_lbr(amd_msr::LAST_BRANCH_FROM_IP), Some(0x1111));
        assert_eq!(lbr.read_amd_lbr(amd_msr::LAST_BRANCH_TO_IP), Some(0x2222));
        assert_eq!(lbr.read_amd_lbr(amd_msr::LAST_INT_FROM_IP), Some(0x3333));
        assert_eq!(lbr.read_amd_lbr(amd_msr::LAST_INT_TO_IP), Some(0x4444));
        // A non-AMD-LBR MSR is not claimed.
        assert_eq!(lbr.read_amd_lbr(0x1D9), None);
        assert!(!lbr.write_amd_lbr(0x1D9, 1));
    }

    #[test]
    fn amd_sanitize_erases_the_branch_into_the_hypervisor() {
        let mut lbr = LbrState::new(LbrPlatform::AmdSvm);
        lbr.write_debug_ctl(1); // enable LBR

        // #VMEXIT left the branch pointing into the hypervisor; the
        // last-interrupt pair holds an earlier, legitimate guest record.
        lbr.last_branch_from_ip = 0x7FFF_2000;
        lbr.last_branch_to_ip = 0xFFFF_8800_0001_0000; // hypervisor entry
        lbr.last_int_from_ip = 0x7FFF_0100;
        lbr.last_int_to_ip = 0x7FFF_0200;

        let guest_next_rip = 0x7FFF_2002;
        lbr.sanitize_after_exit(guest_next_rip);

        // The branch pair now reads as sequential guest execution.
        assert_eq!(lbr.last_branch_from_ip, guest_next_rip);
        assert_eq!(lbr.last_branch_to_ip, guest_next_rip);
        // The last-interrupt pair is untouched (the exit is not a guest IRQ).
        assert_eq!(lbr.last_int_from_ip, 0x7FFF_0100);
        assert_eq!(lbr.last_int_to_ip, 0x7FFF_0200);
    }

    #[test]
    fn amd_sanitize_is_a_noop_when_lbr_disabled() {
        let mut lbr = LbrState::new(LbrPlatform::AmdSvm);
        lbr.last_branch_to_ip = 0xBEEF;
        lbr.sanitize_after_exit(0x1234);
        assert_eq!(lbr.last_branch_to_ip, 0xBEEF);
    }

    #[test]
    fn intel_sanitize_leaves_amd_registers_untouched() {
        let mut lbr = LbrState::new(LbrPlatform::IntelVmx);
        lbr.write_debug_ctl(1);
        lbr.last_branch_to_ip = 0xCAFE;
        lbr.tos = 0;
        lbr.to_addresses[0] = 0xFFFF_8800_0000_0000;
        lbr.sanitize_after_exit(0x4000);
        // Intel path sanitized the stack, not the (irrelevant) AMD registers.
        assert_eq!(lbr.to_addresses[0], 0x4000);
        assert_eq!(lbr.last_branch_to_ip, 0xCAFE);
    }
}

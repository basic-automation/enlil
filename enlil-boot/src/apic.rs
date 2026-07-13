//! Local APIC bring-up for the enlil kernel (Phase 6.2).
//!
//! With its own IDT installed, the kernel enables the local APIC in x2APIC
//! mode — the interrupt-delivery hardware every later step needs (the LAPIC
//! timer for preemption, IPIs for waking other cores, MSI/MSI-X routing). The
//! MSR bit arithmetic (which bits of `IA32_APIC_BASE` to set, whether the CPU
//! advertises x2APIC) is pure and host-tested; only the `rdmsr`/`wrmsr`/`cpuid`
//! that read and program the hardware are gated to the firmware target.

/// `IA32_APIC_BASE` MSR — the APIC's enable bits and physical base.
pub const IA32_APIC_BASE: u32 = 0x1B;

/// `IA32_APIC_BASE.EN` (bit 11): global APIC enable.
pub const APIC_BASE_ENABLE: u64 = 1 << 11;

/// `IA32_APIC_BASE.EXTD` (bit 10): x2APIC mode enable. Setting it requires
/// `EN` also set (Intel SDM Vol. 3 §10.12.1: `EN=0, EXTD=1` is invalid).
pub const APIC_BASE_X2APIC: u64 = 1 << 10;

/// `IA32_X2APIC_APICID` MSR (x2APIC mode): the local APIC ID, read-only.
pub const IA32_X2APIC_APICID: u32 = 0x802;

/// `IA32_X2APIC_VERSION` MSR (x2APIC mode): the LAPIC version register.
pub const IA32_X2APIC_VERSION: u32 = 0x803;

/// `CPUID.1:ECX[21]` — the processor advertises x2APIC support.
pub const CPUID_1_ECX_X2APIC: u32 = 1 << 21;

/// Whether `CPUID.1:ECX` advertises x2APIC support.
#[must_use]
pub const fn x2apic_supported(cpuid_1_ecx: u32) -> bool {
    cpuid_1_ecx & CPUID_1_ECX_X2APIC != 0
}

/// The value to write back to `IA32_APIC_BASE` to enable x2APIC mode,
/// preserving the firmware-programmed physical base and reserved bits.
///
/// Sets both `EN` and `EXTD`: the global enable must stay set when entering
/// x2APIC mode, and once in x2APIC mode the APIC cannot be disabled without a
/// reset (SDM §10.12.1), so this transition is one-way.
#[must_use]
pub const fn apic_base_enable_x2apic(current: u64) -> u64 {
    current | APIC_BASE_ENABLE | APIC_BASE_X2APIC
}

/// Whether `IA32_APIC_BASE` already has x2APIC mode active (`EN` + `EXTD`).
#[must_use]
pub const fn is_x2apic_enabled(apic_base: u64) -> bool {
    apic_base & (APIC_BASE_ENABLE | APIC_BASE_X2APIC) == (APIC_BASE_ENABLE | APIC_BASE_X2APIC)
}

/// The 32-bit local APIC ID from the x2APIC ID MSR value (the full MSR is the
/// ID in x2APIC mode — no shift, unlike the xAPIC MMIO register).
#[must_use]
pub const fn x2apic_id_from_msr(msr_value: u64) -> u32 {
    (msr_value & 0xFFFF_FFFF) as u32
}

#[cfg(target_os = "uefi")]
pub use hw::enable_x2apic;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        IA32_APIC_BASE, IA32_X2APIC_APICID, apic_base_enable_x2apic, is_x2apic_enabled,
        x2apic_id_from_msr, x2apic_supported,
    };

    /// Read a 64-bit MSR.
    ///
    /// # Safety
    ///
    /// `msr` must be a readable MSR at the current privilege level (ring 0).
    unsafe fn rdmsr(msr: u32) -> u64 {
        let (low, high): (u32, u32);
        unsafe {
            core::arch::asm!(
                "rdmsr",
                in("ecx") msr,
                out("eax") low,
                out("edx") high,
                options(nomem, nostack, preserves_flags),
            );
        }
        (u64::from(high) << 32) | u64::from(low)
    }

    /// Write a 64-bit MSR.
    ///
    /// # Safety
    ///
    /// `msr` must be a writable MSR and `value` a legal value for it; a bad
    /// write faults (#GP).
    unsafe fn wrmsr(msr: u32, value: u64) {
        let low = (value & 0xFFFF_FFFF) as u32;
        let high = (value >> 32) as u32;
        unsafe {
            core::arch::asm!(
                "wrmsr",
                in("ecx") msr,
                in("eax") low,
                in("edx") high,
                options(nomem, nostack, preserves_flags),
            );
        }
    }

    /// Read `CPUID.1:ECX`.
    fn cpuid_1_ecx() -> u32 {
        let ecx: u32;
        // SAFETY: CPUID leaf 1 is universally available; preserves no state
        // we depend on beyond the clobbered GPRs.
        unsafe {
            core::arch::asm!(
                "push rbx",           // CPUID clobbers RBX (LLVM-reserved)
                "mov eax, 1",
                "cpuid",
                "pop rbx",
                out("ecx") ecx,
                out("eax") _,
                out("edx") _,
                options(nostack, preserves_flags),
            );
        }
        ecx
    }

    /// Enable the local APIC in x2APIC mode and return the local APIC ID, or
    /// `None` if the CPU does not support x2APIC.
    #[must_use]
    pub fn enable_x2apic() -> Option<u32> {
        if !x2apic_supported(cpuid_1_ecx()) {
            return None;
        }
        // SAFETY: ring 0 after ExitBootServices; IA32_APIC_BASE is a standard
        // architectural MSR and the value keeps the firmware base, only
        // setting the enable bits.
        unsafe {
            let base = rdmsr(IA32_APIC_BASE);
            if !is_x2apic_enabled(base) {
                wrmsr(IA32_APIC_BASE, apic_base_enable_x2apic(base));
            }
            Some(x2apic_id_from_msr(rdmsr(IA32_X2APIC_APICID)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x2apic_support_bit() {
        assert!(!x2apic_supported(0));
        assert!(x2apic_supported(CPUID_1_ECX_X2APIC));
        // Other ECX bits set but not 21 → unsupported.
        assert!(!x2apic_supported(0xFFDF_FFFF));
    }

    #[test]
    fn enable_sets_both_bits_and_preserves_base() {
        // A typical firmware APIC base: 0xFEE00000 with EN already set.
        let base = 0xFEE0_0000 | APIC_BASE_ENABLE;
        let enabled = apic_base_enable_x2apic(base);
        assert!(is_x2apic_enabled(enabled));
        // Physical base preserved.
        assert_eq!(enabled & 0xFFFF_F000, 0xFEE0_0000);
        // EXTD now set.
        assert_ne!(enabled & APIC_BASE_X2APIC, 0);
    }

    #[test]
    fn enable_from_disabled_sets_enable_too() {
        // BSP power-on may have EN clear; the transition must set both.
        let enabled = apic_base_enable_x2apic(0xFEE0_0000);
        assert_eq!(
            enabled & (APIC_BASE_ENABLE | APIC_BASE_X2APIC),
            APIC_BASE_ENABLE | APIC_BASE_X2APIC
        );
    }

    #[test]
    fn is_x2apic_enabled_requires_both_bits() {
        assert!(!is_x2apic_enabled(0));
        assert!(!is_x2apic_enabled(APIC_BASE_ENABLE)); // EN only
        assert!(!is_x2apic_enabled(APIC_BASE_X2APIC)); // EXTD only (invalid)
        assert!(is_x2apic_enabled(APIC_BASE_ENABLE | APIC_BASE_X2APIC));
    }

    #[test]
    fn apic_id_is_the_full_msr_low_dword() {
        assert_eq!(x2apic_id_from_msr(0), 0);
        assert_eq!(x2apic_id_from_msr(7), 7);
        // High dword ignored (reserved in the ID MSR).
        assert_eq!(x2apic_id_from_msr(0xDEAD_0000_0000_0005), 5);
    }
}

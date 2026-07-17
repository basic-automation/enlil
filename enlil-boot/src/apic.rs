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

/// `IA32_X2APIC_EOI` MSR: write 0 to signal end-of-interrupt.
pub const IA32_X2APIC_EOI: u32 = 0x80B;

/// `IA32_X2APIC_SIVR` MSR: spurious-interrupt vector register.
pub const IA32_X2APIC_SIVR: u32 = 0x80F;

/// `IA32_X2APIC_LVT_TIMER` MSR: the local-vector-table timer entry.
pub const IA32_X2APIC_LVT_TIMER: u32 = 0x832;

/// `IA32_X2APIC_INIT_COUNT` MSR: writing it (re)starts the timer countdown.
pub const IA32_X2APIC_INIT_COUNT: u32 = 0x838;

/// `IA32_X2APIC_CUR_COUNT` MSR: the timer's current count (read-only).
pub const IA32_X2APIC_CUR_COUNT: u32 = 0x839;

/// `IA32_X2APIC_DIV_CONF` MSR: the timer divide-configuration.
pub const IA32_X2APIC_DIV_CONF: u32 = 0x83E;

/// `SIVR` bit 8 — APIC software enable. The LAPIC delivers no interrupts until
/// this is set (SDM Vol. 3 §10.9).
pub const SIVR_APIC_ENABLE: u64 = 1 << 8;

/// `LVT` bit 16 — mask. A masked LVT entry delivers no interrupt.
pub const LVT_MASKED: u64 = 1 << 16;

/// `LVT_TIMER` bits 18:17 = 00 — one-shot mode (fire once when the count
/// reaches 0). Periodic is 01, TSC-deadline 10.
pub const TIMER_MODE_ONESHOT: u64 = 0b00 << 17;

/// `DIV_CONF` value for divide-by-16 (bits {3,1,0} = 0b0011; SDM Vol. 3
/// §10.5.4).
pub const TIMER_DIV_16: u64 = 0b0011;

/// The `SIVR` value to software-enable the APIC with `spurious_vector`.
#[must_use]
pub const fn sivr_value(spurious_vector: u8) -> u64 {
    spurious_vector as u64 | SIVR_APIC_ENABLE
}

/// The `LVT_TIMER` value for an unmasked one-shot timer delivering `vector`.
#[must_use]
pub const fn lvt_timer_oneshot(vector: u8) -> u64 {
    // Mode one-shot, unmasked (mask bit clear), interrupt vector in bits 7:0.
    (vector as u64) | TIMER_MODE_ONESHOT
}

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
pub use hw::{arm_oneshot_timer, enable_x2apic, signal_eoi, timer_current_count};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        IA32_APIC_BASE, IA32_X2APIC_APICID, IA32_X2APIC_CUR_COUNT, IA32_X2APIC_DIV_CONF,
        IA32_X2APIC_EOI, IA32_X2APIC_INIT_COUNT, IA32_X2APIC_LVT_TIMER, IA32_X2APIC_SIVR,
        TIMER_DIV_16, apic_base_enable_x2apic, is_x2apic_enabled, lvt_timer_oneshot, sivr_value,
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

    /// Software-enable the APIC and arm a one-shot LAPIC timer that delivers
    /// `vector` after `init_count` ticks (divide-by-16).
    ///
    /// Programs `SIVR` (software enable + spurious vector 0xFF), the divide
    /// configuration, the one-shot unmasked `LVT_TIMER` for `vector`, then
    /// writes `INIT_COUNT` — which starts the countdown. When it reaches 0 the
    /// LAPIC raises `vector`; with interrupts enabled the IDT handler runs.
    pub fn arm_oneshot_timer(vector: u8, init_count: u32) {
        // SAFETY: ring 0 after ExitBootServices; these are architectural x2APIC
        // MSRs and the values are legal (enable + a one-shot timer).
        unsafe {
            wrmsr(IA32_X2APIC_SIVR, sivr_value(0xFF));
            wrmsr(IA32_X2APIC_DIV_CONF, TIMER_DIV_16);
            wrmsr(IA32_X2APIC_LVT_TIMER, lvt_timer_oneshot(vector));
            wrmsr(IA32_X2APIC_INIT_COUNT, u64::from(init_count));
        }
    }

    /// Signal end-of-interrupt to the local APIC (write 0 to `EOI`). Called
    /// from an interrupt handler before it returns.
    pub fn signal_eoi() {
        // SAFETY: ring 0; EOI is a standard x2APIC MSR, 0 is the only legal value.
        unsafe { wrmsr(IA32_X2APIC_EOI, 0) };
    }

    /// The LAPIC timer's current count (low 32 bits of `CUR_COUNT`).
    #[must_use]
    pub fn timer_current_count() -> u32 {
        // SAFETY: ring 0; CUR_COUNT is a read-only architectural x2APIC MSR.
        (unsafe { rdmsr(IA32_X2APIC_CUR_COUNT) } & 0xFFFF_FFFF) as u32
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

    #[test]
    fn sivr_value_software_enables_with_the_spurious_vector() {
        let v = sivr_value(0xFF);
        assert_ne!(v & SIVR_APIC_ENABLE, 0); // software-enable bit set
        assert_eq!(v & 0xFF, 0xFF); // spurious vector in bits 7:0
    }

    #[test]
    fn lvt_timer_oneshot_carries_vector_unmasked() {
        let lvt = lvt_timer_oneshot(0x40);
        assert_eq!(lvt & 0xFF, 0x40); // interrupt vector
        assert_eq!(lvt & LVT_MASKED, 0); // unmasked
        // One-shot mode: the timer-mode bits (18:17) are 0.
        assert_eq!((lvt >> 17) & 0b11, 0);
    }
}

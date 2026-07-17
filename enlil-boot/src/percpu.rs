//! Per-CPU data with a `GS`-base TLS pointer for the enlil kernel (Phase 6.2).
//!
//! Every later per-core structure (this CPU's run queue, its current vCPU, its
//! LAPIC id) is reached through the `GS` segment base: x86-64 kernels park a
//! per-CPU block's address in `IA32_GS_BASE` and read it as `gs:[0]`, so any
//! core finds its own data with no lookup. This module installs the boot CPU's
//! block and proves the `GS`-relative read returns it. The block layout is pure
//! and host-tested; only the `wrmsr`/`gs:[…]` access is gated to firmware.
//!
//! The base survives the SVM run loop: its `VMSAVE`/`VMLOAD` bracket saves and
//! restores the host `GS` base around every `VMRUN` (see [`crate::svm`]), so a
//! guest cannot clobber the kernel's TLS pointer.

/// `IA32_GS_BASE` MSR — the current `GS` segment base in 64-bit mode.
pub const IA32_GS_BASE: u32 = 0xC000_0101;

/// The boot CPU's per-core data block.
///
/// `self_ptr` at offset 0 holds the block's own address — the self-pointer
/// idiom that lets `gs:[0]` yield the per-CPU base for further `gs`-relative
/// field access. `#[repr(C)]` pins the field offsets the `GS` reads assume.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PerCpu {
    /// The block's own linear address (offset 0). `gs:[0]` reads it back.
    pub self_ptr: u64,
    /// This CPU's local APIC id (offset 8).
    pub apic_id: u32,
}

#[cfg(target_os = "uefi")]
pub use hw::install_and_selftest;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{IA32_GS_BASE, PerCpu};
    use core::cell::UnsafeCell;

    /// The boot CPU's per-CPU block. Single boot CPU, written once before the
    /// `GS` base points at it, so the plain `UnsafeCell` is sound.
    struct PerCpuStore(UnsafeCell<PerCpu>);
    // SAFETY: written only during single-CPU init, before any `gs`-relative
    // read; never shared across threads (there is one boot CPU).
    unsafe impl Sync for PerCpuStore {}

    static BOOT_CPU: PerCpuStore = PerCpuStore(UnsafeCell::new(PerCpu {
        self_ptr: 0,
        apic_id: 0,
    }));

    /// Write `value` to `msr`.
    ///
    /// # Safety
    ///
    /// `msr` must be a writable MSR and `value` legal for it.
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

    /// Read the `u64` at `gs:[0]` (the per-CPU self-pointer).
    fn read_gs_self_ptr() -> u64 {
        let value: u64;
        // SAFETY: the GS base points at a live PerCpu whose offset 0 is a u64.
        unsafe {
            core::arch::asm!(
                "mov {}, gs:[0]",
                out(reg) value,
                options(nomem, nostack, preserves_flags),
            );
        }
        value
    }

    /// Install the boot CPU's per-CPU block as the `GS`-base TLS pointer and
    /// prove it, returning whether the round-trip matched.
    ///
    /// Sets `self_ptr` to the block's address, loads it into `IA32_GS_BASE`, and
    /// checks `gs:[0]` reads that address back.
    pub fn install_and_selftest(apic_id: u32) -> bool {
        let base = BOOT_CPU.0.get() as u64;
        // SAFETY: single boot CPU; no `gs`-relative read happens until the base
        // is set below, and the block is written once here.
        unsafe {
            let block = &mut *BOOT_CPU.0.get();
            block.self_ptr = base;
            block.apic_id = apic_id;
            wrmsr(IA32_GS_BASE, base);
        }
        read_gs_self_ptr() == base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    #[test]
    fn percpu_field_offsets_are_stable_for_gs_reads() {
        // The GS-relative reads assume self_ptr at 0, apic_id at 8.
        assert_eq!(offset_of!(PerCpu, self_ptr), 0);
        assert_eq!(offset_of!(PerCpu, apic_id), 8);
    }

    #[test]
    fn gs_base_msr_number_is_ia32_gs_base() {
        assert_eq!(IA32_GS_BASE, 0xC000_0101);
    }
}

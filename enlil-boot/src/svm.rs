//! SVM enablement for the enlil kernel (Phase 6.2, second live-boot slice).
//!
//! Before the kernel can run a guest with `VMRUN` it must turn SVM on: the CPU
//! has to advertise it (`CPUID Fn8000_0001` ECX bit 2), firmware must not have
//! locked it off (`VM_CR.SVMDIS` + `LOCK`), and `EFER.SVME` has to be set (AMD
//! APM Vol. 2 §15.4). This module is that gate — the enable *decision* is pure
//! and host-tested; only the `cpuid`/`rdmsr`/`wrmsr` are firmware-gated. The
//! full VMCB programming and the `VMRUN` op build on top (the `enlil-hal::svm`
//! decode layer + region types are the authoritative backend seam).

/// `CPUID Fn8000_0001` ECX bit 2: SVM is supported.
pub const CPUID_FN8000_0001_ECX_SVM: u32 = 1 << 2;

/// The `VM_CR` MSR (AMD APM §15.30.1).
pub const MSR_VM_CR: u32 = 0xC001_0114;

/// `VM_CR.SVMDIS` (bit 4): SVM disabled — `EFER.SVME` writes #GP when set.
pub const VM_CR_SVMDIS: u64 = 1 << 4;

/// `VM_CR.LOCK` (bit 3): the SVMDIS setting is locked until reset.
pub const VM_CR_LOCK: u64 = 1 << 3;

/// The extended-feature-enable register MSR.
pub const MSR_EFER: u32 = 0xC000_0080;

/// `EFER.SVME` (bit 12): the SVM-enable bit `VMRUN` requires.
pub const EFER_SVME: u64 = 1 << 12;

/// The `VM_HSAVE_PA` MSR: physical base of the 4 KiB host state-save area
/// `VMRUN` uses (must be programmed before the first `VMRUN`, APM §15.30.4).
pub const MSR_VM_HSAVE_PA: u32 = 0xC001_0117;

/// The size/alignment of the host state-save area: one 4 KiB page.
pub const HSAVE_PAGE_SIZE: usize = 4096;

/// Whether `addr` is a valid `VM_HSAVE_PA`: nonzero and 4 KiB-aligned (the
/// hardware requires a page-aligned host-save area; bits 11:0 are reserved).
#[must_use]
pub const fn is_valid_hsave_pa(addr: u64) -> bool {
    addr != 0 && addr.is_multiple_of(HSAVE_PAGE_SIZE as u64)
}

/// Why SVM cannot be enabled, or that it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvmStatus {
    /// SVM is usable: `EFER.SVME` may be set.
    Available,
    /// `CPUID Fn8000_0001` ECX does not advertise SVM.
    Unsupported,
    /// `VM_CR.SVMDIS` is set and locked — firmware disabled SVM until reset.
    DisabledByFirmware,
}

/// Decide whether SVM can be enabled from the CPUID and `VM_CR` values.
///
/// `SVMDIS` set but not `LOCK`ed is still [`Available`](SvmStatus::Available):
/// software may clear `SVMDIS` before setting `EFER.SVME` (APM §15.30.1). Only
/// a locked `SVMDIS` is a hard block.
#[must_use]
pub const fn svm_status(cpuid_fn8000_0001_ecx: u32, vm_cr: u64) -> SvmStatus {
    if cpuid_fn8000_0001_ecx & CPUID_FN8000_0001_ECX_SVM == 0 {
        return SvmStatus::Unsupported;
    }
    if vm_cr & VM_CR_SVMDIS != 0 && vm_cr & VM_CR_LOCK != 0 {
        return SvmStatus::DisabledByFirmware;
    }
    SvmStatus::Available
}

/// Whether `EFER` already has `SVME` set.
#[must_use]
pub const fn is_svm_enabled(efer: u64) -> bool {
    efer & EFER_SVME != 0
}

/// The `EFER` value that turns SVM on, preserving the other bits.
#[must_use]
pub const fn efer_with_svme(efer: u64) -> u64 {
    efer | EFER_SVME
}

/// The `VM_CR` value with `SVMDIS` cleared, for the unlocked-disabled case.
#[must_use]
pub const fn vm_cr_clear_svmdis(vm_cr: u64) -> u64 {
    vm_cr & !VM_CR_SVMDIS
}

#[cfg(target_os = "uefi")]
pub use hw::{enable_svm, program_host_save_area};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        HSAVE_PAGE_SIZE, MSR_EFER, MSR_VM_CR, MSR_VM_HSAVE_PA, SvmStatus, efer_with_svme,
        is_svm_enabled, is_valid_hsave_pa, svm_status, vm_cr_clear_svmdis,
    };
    use alloc::alloc::{Layout, alloc_zeroed};

    /// Read a 64-bit MSR.
    ///
    /// # Safety
    ///
    /// `msr` must be readable at ring 0.
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
    /// `msr` must be writable and `value` legal for it.
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

    /// Read `CPUID Fn8000_0001` ECX.
    fn cpuid_ext_features_ecx() -> u32 {
        let ecx: u32;
        // SAFETY: extended leaf 0x8000_0001 exists on every x86-64 CPU.
        unsafe {
            core::arch::asm!(
                "push rbx",
                "mov eax, 0x80000001",
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

    /// Turn SVM on if the CPU allows it, returning the resulting status.
    ///
    /// On [`Available`](SvmStatus::Available) this clears an unlocked
    /// `VM_CR.SVMDIS` (if set) and sets `EFER.SVME`, leaving the CPU ready for
    /// `VM_HSAVE_PA` programming and `VMRUN`.
    #[must_use]
    pub fn enable_svm() -> SvmStatus {
        // SAFETY: ring 0 after ExitBootServices; VM_CR/EFER are architectural
        // AMD MSRs and we only touch the SVMDIS/SVME bits.
        unsafe {
            let vm_cr = rdmsr(MSR_VM_CR);
            let status = svm_status(cpuid_ext_features_ecx(), vm_cr);
            if !matches!(status, SvmStatus::Available) {
                return status;
            }
            // Clear an unlocked SVMDIS so the EFER.SVME write does not #GP.
            let cleared = vm_cr_clear_svmdis(vm_cr);
            if cleared != vm_cr {
                wrmsr(MSR_VM_CR, cleared);
            }
            let efer = rdmsr(MSR_EFER);
            if !is_svm_enabled(efer) {
                wrmsr(MSR_EFER, efer_with_svme(efer));
            }
            SvmStatus::Available
        }
    }

    /// A page-sized, page-aligned block, for a `const` 4 KiB-aligned layout.
    #[repr(C, align(4096))]
    struct HsavePage([u8; HSAVE_PAGE_SIZE]);

    /// Allocate the host state-save area and program `VM_HSAVE_PA` with its
    /// address, returning the programmed base if it read back correctly.
    ///
    /// The page is intentionally leaked: the host-save area must live for the
    /// machine's lifetime (every `VMRUN` uses it). The kernel runs
    /// identity-mapped this early, so the allocation's virtual address is its
    /// physical address — the value the MSR takes. Returns `None` if the page
    /// cannot be allocated, is not page-aligned, or the MSR does not hold the
    /// value after the write.
    #[must_use]
    pub fn program_host_save_area() -> Option<u64> {
        // SAFETY: HSAVE_PAGE_SIZE is a nonzero power of two.
        let page = unsafe { alloc_zeroed(Layout::new::<HsavePage>()) };
        if page.is_null() {
            return None;
        }
        let pa = page as u64;
        if !is_valid_hsave_pa(pa) {
            return None;
        }
        // SAFETY: ring 0; VM_HSAVE_PA is an architectural AMD MSR taking a
        // page-aligned physical address, which `pa` is.
        unsafe { wrmsr(MSR_VM_HSAVE_PA, pa) };
        // Read back: confirm the MSR accepted the address.
        let readback = unsafe { rdmsr(MSR_VM_HSAVE_PA) };
        if readback == pa { Some(pa) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_when_cpuid_bit_clear() {
        assert_eq!(svm_status(0, 0), SvmStatus::Unsupported);
        // Other ECX bits set but not SVM.
        assert_eq!(svm_status(0xFFFF_FFFB, 0), SvmStatus::Unsupported);
    }

    #[test]
    fn available_when_supported_and_not_disabled() {
        assert_eq!(
            svm_status(CPUID_FN8000_0001_ECX_SVM, 0),
            SvmStatus::Available
        );
        // SVMDIS set but NOT locked → software can clear it → still available.
        assert_eq!(
            svm_status(CPUID_FN8000_0001_ECX_SVM, VM_CR_SVMDIS),
            SvmStatus::Available
        );
    }

    #[test]
    fn firmware_disabled_when_svmdis_locked() {
        assert_eq!(
            svm_status(CPUID_FN8000_0001_ECX_SVM, VM_CR_SVMDIS | VM_CR_LOCK),
            SvmStatus::DisabledByFirmware
        );
        // LOCK without SVMDIS is fine (SVM enabled + locked on).
        assert_eq!(
            svm_status(CPUID_FN8000_0001_ECX_SVM, VM_CR_LOCK),
            SvmStatus::Available
        );
    }

    #[test]
    fn efer_svme_set_and_detected() {
        assert!(!is_svm_enabled(0));
        let efer = efer_with_svme(0x0000_0501); // LME|LMA|... already set
        assert!(is_svm_enabled(efer));
        // Other bits preserved.
        assert_eq!(efer & 0x0000_0501, 0x0000_0501);
    }

    #[test]
    fn hsave_pa_validity() {
        assert!(!is_valid_hsave_pa(0)); // null
        assert!(!is_valid_hsave_pa(0x1000_0001)); // not page-aligned
        assert!(!is_valid_hsave_pa(0xFFF)); // sub-page
        assert!(is_valid_hsave_pa(0x1000)); // one aligned page
        assert!(is_valid_hsave_pa(0x1_780_000)); // a real heap-region base
    }

    #[test]
    fn clearing_svmdis_preserves_other_bits() {
        let vm_cr = VM_CR_SVMDIS | VM_CR_LOCK | (1 << 0);
        let cleared = vm_cr_clear_svmdis(vm_cr);
        assert_eq!(cleared & VM_CR_SVMDIS, 0);
        assert_ne!(cleared & VM_CR_LOCK, 0);
        assert_ne!(cleared & 1, 0);
    }
}

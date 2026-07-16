//! SVM enablement for the enlil kernel (Phase 6.2, second live-boot slice).
//!
//! Before the kernel can run a guest with `VMRUN` it must turn SVM on: the CPU
//! has to advertise it (`CPUID Fn8000_0001` ECX bit 2), firmware must not have
//! locked it off (`VM_CR.SVMDIS` + `LOCK`), and `EFER.SVME` has to be set (AMD
//! APM Vol. 2 §15.4). This module is that gate — the enable *decision* is pure
//! and host-tested; only the `cpuid`/`rdmsr`/`wrmsr` are firmware-gated. The
//! full VMCB programming and the `VMRUN` op build on top.
//!
//! The MSR numbers and `VM_CR`/`EFER` bit predicates come from
//! [`enlil_hal::svm`] — the authoritative ISA seam (LOCKED PRINCIPLE 2) — so
//! they are defined once for the whole hypervisor; this module keeps only the
//! boot-specific enable *policy* ([`SvmStatus`], [`svm_status`]) and the
//! privileged firmware ops.

pub use enlil_hal::svm::{
    CPUID_FN8000_0001_ECX_SVM, EFER_SVME, MSR_EFER, MSR_VM_CR, MSR_VM_HSAVE_PA, VM_CR_LOCK,
    VM_CR_SVMDIS,
};
use enlil_hal::svm::{vm_cr_svm_disabled, vm_cr_svm_locked};

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
    if vm_cr_svm_disabled(vm_cr) && vm_cr_svm_locked(vm_cr) {
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

/// How the boot guest's #VMEXIT dispatch loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStop {
    /// Guest executed `HLT` — the expected clean stop.
    Halted,
    /// Guest triggered `SHUTDOWN` (triple fault), contained by the intercept.
    ShutDown,
    /// `VMRUN` reported invalid guest state — the loop aborted.
    Invalid,
    /// An exit the loop does not route yet — stopped, `final_exit` holds it.
    Unhandled,
    /// The loop hit its `VMRUN` cap without a terminal exit (a runaway guest).
    IterationCap,
}

/// What driving the boot guest through the dispatch loop observed.
///
/// Bookkeeping the kernel logs to prove the guest ran more than one
/// instruction and that the emulate-and-skip path worked end to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestRunOutcome {
    /// How the run ended.
    pub stop: RunStop,
    /// Number of `VMRUN` entries executed.
    pub vmruns: u32,
    /// Intercepted `CPUID` exits skipped.
    pub cpuid_exits: u32,
    /// Intercepted port-I/O exits handled.
    pub io_exits: u32,
    /// The last `OUT` the guest performed, as `(port, byte)` — the value enlil
    /// captured through the arch-neutral `VmExit::IoOut`.
    pub last_io_out: Option<(u16, u32)>,
    /// Raw #VMEXIT code of the final (stopping) exit.
    pub final_exit: u64,
}

impl GuestRunOutcome {
    /// A fresh outcome before the first `VMRUN`.
    #[must_use]
    const fn new() -> Self {
        Self {
            stop: RunStop::IterationCap,
            vmruns: 0,
            cpuid_exits: 0,
            io_exits: 0,
            last_io_out: None,
            final_exit: 0,
        }
    }
}

#[cfg(target_os = "uefi")]
pub use hw::{enable_svm, program_boot_vmcb, program_host_save_area, run_boot_guest_loop};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        HSAVE_PAGE_SIZE, MSR_EFER, MSR_VM_CR, MSR_VM_HSAVE_PA, SvmStatus, efer_with_svme,
        is_svm_enabled, is_valid_hsave_pa, svm_status, vm_cr_clear_svmdis,
    };
    use alloc::alloc::{Layout, alloc_zeroed};
    use enlil_hal::npt::build_identity_npt_2mib;
    use enlil_hal::region::{PageRegion, Vmcb};
    use enlil_hal::svm::{MinimalGuestSetup, control, program_minimal_hlt_guest};

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

    /// A 3-page buffer (PML4 + PDPT + one PD) for a ≤ 1 GiB nested identity map.
    #[repr(C, align(4096))]
    struct NptBuf([u8; 3 * 4096]);

    /// Identity-map the low gibibyte in the guest NPT — this covers the whole
    /// boot heap (capped at 64 MiB), where the guest code page lives.
    const NPT_MAP_BYTES: u64 = 1024 * 1024 * 1024;

    /// Build a complete, `VMRUN`-ready VMCB for a minimal real-mode guest that
    /// executes a single `HLT`, returning `(vmcb_pa, ncr3, guest_code_pa)`.
    ///
    /// Every piece the second live-boot sub-milestone needs is assembled here
    /// through the `enlil-hal` layer, with the firmware gone: a guest code page
    /// holding a `HLT` (`0xF4`), a nested page table identity-mapping the low
    /// gibibyte (so the guest's `GPA == SPA`), and a [`Vmcb`] programmed via
    /// [`program_minimal_hlt_guest`] to enter that code under that NPT. The
    /// programmed nested-CR3 is read straight back out of the VMCB to prove the
    /// write landed. All three allocations are leaked — they must outlive this
    /// call for the eventual `VMRUN`. Returns `None` if any allocation fails or
    /// the NPT/VMCB is malformed (none can happen here). The `VMRUN` op that
    /// actually runs this guest to `#VMEXIT(HLT)` is the next slice.
    #[must_use]
    pub fn program_boot_vmcb() -> Option<(u64, u64, u64)> {
        // Guest code page: `CPUID` (0F A2) then `HLT` (F4). CPUID is
        // intercepted (LOCKED PRINCIPLE 1), so the guest takes a #VMEXIT the
        // dispatch loop skips past, then runs its *second* instruction (HLT) —
        // proving the loop re-VMRUNs a guest through more than one instruction.
        let mut code = PageRegion::new()?;
        {
            let bytes = code.as_bytes_mut();
            bytes[0] = 0x0F; // CPUID
            bytes[1] = 0xA2;
            bytes[2] = 0xF4; // HLT
        }
        let guest_code_pa = code.base_addr();
        core::mem::forget(code); // the guest's RAM must persist

        // Nested page tables identity-mapping the low GiB (GPA == SPA).
        // SAFETY: NptBuf has a nonzero size; alloc_zeroed yields a zeroed,
        // 4 KiB-aligned NptBuf-sized block or null.
        let npt_raw = unsafe { alloc_zeroed(Layout::new::<NptBuf>()) };
        if npt_raw.is_null() {
            return None;
        }
        let npt_pa = npt_raw as u64;
        // SAFETY: npt_raw points at a live, zeroed, exclusively-owned NptBuf;
        // it is leaked below so the slice never outlives the allocation.
        let npt_buf = unsafe { core::slice::from_raw_parts_mut(npt_raw, 3 * 4096) };
        let ncr3 = build_identity_npt_2mib(npt_buf, npt_pa, NPT_MAP_BYTES)
            .ok()?
            .ncr3;

        // Program a VMCB to enter the guest code under that NPT.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            entry_ip: 0,
            code_base: guest_code_pa,
            stack_pointer: 0,
        };
        program_minimal_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;
        // Read the nested-CR3 back to confirm the programming landed.
        let mut ncr3_le = [0u8; 8];
        ncr3_le.copy_from_slice(&vmcb.as_bytes()[control::NESTED_CR3..control::NESTED_CR3 + 8]);
        if u64::from_le_bytes(ncr3_le) != ncr3 {
            return None;
        }
        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // the VMCB must outlive this call for VMRUN
        Some((vmcb_pa, ncr3, guest_code_pa))
    }

    /// Execute one `VMRUN` on the VMCB at `vmcb_pa`, entering the guest and
    /// returning on its `#VMEXIT`.
    ///
    /// `clgi` clears the global interrupt flag so no host interrupt disturbs
    /// the transition, `vmrun rax` enters the guest (RAX holds the VMCB
    /// physical address) and returns here on `#VMEXIT`, and `stgi` restores the
    /// flag. VMRUN does not save the volatile GPRs, so they are marked
    /// clobbered; the guest runs on its own VMCB RSP, so the host stack — and
    /// the hand-saved rbx — survive. The CPU writes the exit reason into the
    /// VMCB control area, which the caller reads.
    ///
    /// # Safety
    ///
    /// `vmcb_pa` must be a `VMRUN`-ready VMCB with SVM enabled and
    /// `VM_HSAVE_PA` programmed (see [`run_boot_guest_loop`]).
    unsafe fn vmrun(vmcb_pa: u64) {
        unsafe {
            core::arch::asm!(
                // rbx/rbp are reserved by LLVM and cannot be clobber operands,
                // so preserve rbx across the guest by hand (rbp is untouched by
                // these guests). The guest runs on its own VMCB RSP, so the
                // host stack — and this saved rbx — survive the transition.
                "push rbx",
                "clgi",
                "vmrun rax",
                "stgi",
                "pop rbx",
                inout("rax") vmcb_pa => _,
                out("rcx") _,
                out("rdx") _,
                out("rsi") _,
                out("rdi") _,
                out("r8") _,
                out("r9") _,
                out("r10") _,
                out("r11") _,
                out("r12") _,
                out("r13") _,
                out("r14") _,
                out("r15") _,
            );
        }
    }

    /// Drive the guest at `vmcb_pa` through a real `#VMEXIT` dispatch loop,
    /// returning what the run observed ([`GuestRunOutcome`]).
    ///
    /// Each `#VMEXIT` is classified through the `enlil-hal` seam
    /// ([`classify_run_loop_exit`]) onto the arch-neutral model (LOCKED
    /// PRINCIPLE 2). `HLT`/`SHUTDOWN`/invalid state stop the loop; the two
    /// emulate-and-skip exits resume it:
    ///
    /// - **CPUID** — intercepted for stealth (LOCKED PRINCIPLE 1). The minimal
    ///   guest does not consume its result, so the loop only advances guest RIP
    ///   past the instruction ([`resume_rip_after`] — hardware `NEXT_RIP`, or
    ///   `RIP + CPUID_INSN_LEN` when NRIP-save is absent) and re-`VMRUN`s. The
    ///   stealth CPUID table plugs in here later.
    /// - **IOIO** — decode `EXITINFO1` ([`IoioExitInfo`]) with the guest `RAX`
    ///   onto [`VmExit::IoOut`]/`IoIn`, capture an `OUT` byte, then advance to
    ///   the `EXITINFO2` RIP the hardware saved past the `IN`/`OUT`.
    ///
    /// A `VMRUN` cap bounds a runaway guest so the loop always terminates. The
    /// VMCB clean bits stay 0, so each re-`VMRUN` reloads the RIP the loop
    /// wrote; TLB control stays flush-all, correct across re-entry.
    ///
    /// # Safety
    ///
    /// `vmcb_pa` must be a `VMRUN`-ready VMCB (from [`program_boot_vmcb`]) with
    /// SVM enabled ([`enable_svm`]) and `VM_HSAVE_PA` programmed
    /// ([`program_host_save_area`]); it is identity-mapped, so the byte view is
    /// the CPU's own VMCB memory.
    #[must_use]
    pub unsafe fn run_boot_guest_loop(vmcb_pa: u64) -> super::GuestRunOutcome {
        use enlil_hal::VmExit;
        use enlil_hal::svm::{
            CPUID_INSN_LEN, IoioExitInfo, RunLoopExit, VMCB_SIZE, classify_run_loop_exit,
            exit_code, exit_info_1, exit_info_2, guest_rax, guest_rip, ioio_to_vmexit, next_rip,
            resume_rip_after, set_guest_rip,
        };

        /// Bound on total `VMRUN`s so a misbehaving guest cannot spin forever.
        const MAX_VMRUNS: u32 = 32;

        let mut outcome = super::GuestRunOutcome::new();
        loop {
            // SAFETY: the caller guarantees a VMRUN-ready VMCB with SVM on.
            unsafe { vmrun(vmcb_pa) };
            outcome.vmruns += 1;

            // The VMCB is identity-mapped and exclusively owned (leaked by
            // program_boot_vmcb); the CPU just wrote its exit fields, and the
            // `vmrun` asm may touch memory so these reads are not reordered
            // before it.
            // SAFETY: vmcb_pa points at our live, page-sized VMCB.
            let vmcb = unsafe { core::slice::from_raw_parts_mut(vmcb_pa as *mut u8, VMCB_SIZE) };
            let code = exit_code(vmcb);
            outcome.final_exit = code.raw();

            match classify_run_loop_exit(code) {
                RunLoopExit::Halted => {
                    outcome.stop = super::RunStop::Halted;
                    break;
                }
                RunLoopExit::ShutDown => {
                    outcome.stop = super::RunStop::ShutDown;
                    break;
                }
                RunLoopExit::Invalid => {
                    outcome.stop = super::RunStop::Invalid;
                    break;
                }
                RunLoopExit::Unhandled => {
                    outcome.stop = super::RunStop::Unhandled;
                    break;
                }
                RunLoopExit::Cpuid => {
                    outcome.cpuid_exits += 1;
                    let rip = resume_rip_after(next_rip(vmcb), guest_rip(vmcb), CPUID_INSN_LEN);
                    set_guest_rip(vmcb, rip);
                }
                RunLoopExit::Io => {
                    outcome.io_exits += 1;
                    let info = IoioExitInfo::from_raw(exit_info_1(vmcb));
                    // Low 32 bits of guest RAX supply the OUT data; masking
                    // first makes the narrowing total (never truncating).
                    let rax = u32::try_from(guest_rax(vmcb) & 0xFFFF_FFFF).unwrap_or(0);
                    if let Some(VmExit::IoOut { port, data, .. }) = ioio_to_vmexit(info, rax) {
                        outcome.last_io_out = Some((port, data));
                    }
                    // EXITINFO2 carries the RIP just past the IN/OUT.
                    set_guest_rip(vmcb, exit_info_2(vmcb));
                }
            }

            if outcome.vmruns >= MAX_VMRUNS {
                outcome.stop = super::RunStop::IterationCap;
                break;
            }
        }
        outcome
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

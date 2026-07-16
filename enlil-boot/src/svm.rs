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

/// The port the boot guest writes to prove the I/O exit path — an unused
/// legacy "POST" port so nothing else contends for it. A `u8` because
/// `OUT imm8, AL` takes an 8-bit port immediate.
pub const GUEST_IO_PORT: u8 = 0x80;

/// The value the boot guest stashes in `BX` *before* the CPUID exit.
///
/// It then copies `BX` into `AX` and `OUT`s `BL`. If the GPR shell preserves
/// `BX` across the exit, the emulated `OUT` carries this value's low byte
/// ([`GUEST_EXPECTED_OUT`]) — the shell's end-to-end proof.
pub const GUEST_BX_STASH: u16 = 0x1234;

/// The byte the guest is expected to `OUT` — `BL`, the low byte of
/// [`GUEST_BX_STASH`]. `report_guest_run` checks the emulated `OUT` against
/// this to confirm the GPR shell preserved `BX`.
pub const GUEST_EXPECTED_OUT: u8 = GUEST_BX_STASH.to_le_bytes()[0];

/// The guest general-purpose registers `VMRUN` does **not** carry.
///
/// `VMRUN` swaps only `RAX`, `RSP`, `RIP`, and `RFLAGS` through the VMCB save
/// area; the other 14 GPRs are shared with the host. The [`run_boot_guest_loop`]
/// shell loads these into the CPU before `VMRUN` and stores them back on
/// `#VMEXIT`, so the guest keeps register state across an intercepted-and-
/// resumed instruction — the enabling step for real guest code and for a
/// faithful CPUID emulation (which must write guest EBX/ECX/EDX).
///
/// Field order and `#[repr(C)]` are load-bearing: the run shell's inline asm
/// indexes this struct by fixed byte offset (`rbx` at 0, each field +8). The
/// `guest_gprs_layout_is_stable_for_the_asm_shell` test pins that contract.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GuestGprs {
    /// RBX — offset 0x00.
    pub rbx: u64,
    /// RCX — offset 0x08.
    pub rcx: u64,
    /// RDX — offset 0x10.
    pub rdx: u64,
    /// RSI — offset 0x18.
    pub rsi: u64,
    /// RDI — offset 0x20.
    pub rdi: u64,
    /// RBP — offset 0x28.
    pub rbp: u64,
    /// R8 — offset 0x30.
    pub r8: u64,
    /// R9 — offset 0x38.
    pub r9: u64,
    /// R10 — offset 0x40.
    pub r10: u64,
    /// R11 — offset 0x48.
    pub r11: u64,
    /// R12 — offset 0x50.
    pub r12: u64,
    /// R13 — offset 0x58.
    pub r13: u64,
    /// R14 — offset 0x60.
    pub r14: u64,
    /// R15 — offset 0x68.
    pub r15: u64,
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
    use super::{GUEST_BX_STASH, GUEST_IO_PORT};
    use super::{
        HSAVE_PAGE_SIZE, MSR_EFER, MSR_VM_CR, MSR_VM_HSAVE_PA, SvmStatus, efer_with_svme,
        is_svm_enabled, is_valid_hsave_pa, svm_status, vm_cr_clear_svmdis,
    };
    use alloc::alloc::{Layout, alloc_zeroed};
    use enlil_hal::npt::build_identity_npt_2mib;
    use enlil_hal::region::{IoPermissionsMap, PageRegion, Vmcb};
    use enlil_hal::svm::{
        MinimalGuestSetup, control, enable_io_intercept, program_minimal_hlt_guest,
    };

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
    /// exercises the exit-handling path, returning `(vmcb_pa, ncr3,
    /// guest_code_pa)`.
    ///
    /// The guest stashes a value in `BX`, then runs `CPUID; MOV AX,BX; OUT; HLT`
    /// — so the dispatch loop routes all three exit classes (`CPUID` skipped,
    /// `IOIO` decoded + emulated, `HLT` stop) *and* the GPR shell must preserve
    /// `BX` across the CPUID exit for the emulated `OUT` to carry
    /// [`GUEST_EXPECTED_OUT`]. Assembled through the `enlil-hal` layer with the
    /// firmware gone: the code page, a nested page table identity-mapping the
    /// low gibibyte (so `GPA == SPA`), a [`Vmcb`] programmed via
    /// [`program_minimal_hlt_guest`] then armed for port I/O
    /// ([`enable_io_intercept`] + an intercept-all [`IoPermissionsMap`]). The
    /// nested-CR3 is read back to prove the write landed. All allocations are
    /// leaked — they must outlive the `VMRUN`. Returns `None` if any allocation
    /// fails or the NPT/VMCB is malformed (none can happen here).
    #[must_use]
    pub fn program_boot_vmcb() -> Option<(u64, u64, u64)> {
        // Guest code page (real-mode, 16-bit): stash a value in BX, take the
        // CPUID exit, then copy BX→AX and OUT it. If the GPR shell preserves BX
        // across the intercepted CPUID, the emulated OUT carries BL.
        //   mov bx, 0x1234   BB lo hi
        //   cpuid            0F A2     (intercepted → skipped by the loop)
        //   mov ax, bx       89 D8
        //   out 0x80, al     E6 80     (IOIO #VMEXIT → decoded + emulated)
        //   hlt              F4        (clean stop)
        let mut code = PageRegion::new()?;
        {
            let stash = GUEST_BX_STASH.to_le_bytes();
            let bytes = code.as_bytes_mut();
            bytes[0] = 0xBB; // mov bx, imm16
            bytes[1] = stash[0];
            bytes[2] = stash[1];
            bytes[3] = 0x0F; // CPUID
            bytes[4] = 0xA2;
            bytes[5] = 0x89; // mov ax, bx
            bytes[6] = 0xD8;
            bytes[7] = 0xE6; // OUT imm8, AL
            bytes[8] = GUEST_IO_PORT;
            bytes[9] = 0xF4; // HLT
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

        // Arm port-I/O interception so the guest's OUT takes an IOIO #VMEXIT.
        // The guest loads AL itself (from BX), so no RAX preload is needed. The
        // IOPM intercepts every port; it is leaked so it outlives the VMRUN the
        // CPU checks it against.
        let iopm = IoPermissionsMap::intercept_all().ok()?;
        enable_io_intercept(vmcb.as_bytes_mut(), iopm.base_addr());
        core::mem::forget(iopm);

        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // the VMCB must outlive this call for VMRUN
        Some((vmcb_pa, ncr3, guest_code_pa))
    }

    /// Execute one `VMRUN` on the VMCB at `vmcb_pa`, swapping the guest's
    /// general-purpose registers (`*gprs`) in around the guest and back out on
    /// `#VMEXIT`.
    ///
    /// `VMRUN` carries only `RAX`/`RSP`/`RIP`/`RFLAGS` through the VMCB; the
    /// other 14 GPRs are shared with the host. This shell loads them from
    /// `*gprs` before `vmrun` and stores the guest's values back after, so the
    /// guest keeps register state across an intercepted-and-resumed
    /// instruction. `clgi`/`stgi` bracket the transition so no host interrupt
    /// disturbs it; the CPU writes the exit reason into the VMCB control area,
    /// which the caller reads.
    ///
    /// The `RDI`-held `gprs` pointer is pushed before the guest overwrites RDI,
    /// then recovered from the stack after `#VMEXIT` (host `RSP` is restored by
    /// `VMRUN`). `RBX`/`RBP` are LLVM-reserved and hand-preserved; the other
    /// GPRs are marked clobbered. The stack is balanced (three pushes, three
    /// pops).
    ///
    /// # Safety
    ///
    /// `vmcb_pa` must be a `VMRUN`-ready VMCB with SVM enabled and
    /// `VM_HSAVE_PA` programmed (see [`run_boot_guest_loop`]); `gprs` must point
    /// at a live [`GuestGprs`](super::GuestGprs).
    unsafe fn vmrun(vmcb_pa: u64, gprs: *mut super::GuestGprs) {
        unsafe {
            core::arch::asm!(
                "push rbx",             // preserve host rbx (callee-saved)
                "push rbp",             // preserve host rbp (callee-saved)
                "push rdi",             // keep the gprs pointer across VMRUN
                // Load guest GPRs from *gprs (rdi); load rbp and rdi last since
                // rdi still holds the struct pointer for the earlier loads.
                "mov rbx, [rdi + 0x00]",
                "mov rcx, [rdi + 0x08]",
                "mov rdx, [rdi + 0x10]",
                "mov rsi, [rdi + 0x18]",
                "mov rbp, [rdi + 0x28]",
                "mov r8,  [rdi + 0x30]",
                "mov r9,  [rdi + 0x38]",
                "mov r10, [rdi + 0x40]",
                "mov r11, [rdi + 0x48]",
                "mov r12, [rdi + 0x50]",
                "mov r13, [rdi + 0x58]",
                "mov r14, [rdi + 0x60]",
                "mov r15, [rdi + 0x68]",
                "mov rdi, [rdi + 0x20]", // guest rdi last (pointer now on stack)
                "clgi",
                "vmrun rax",
                "stgi",
                // Guest GPRs are live in the registers. Recover the struct
                // pointer from the stack into rax (host rax is dead here) and
                // store the guest values back.
                "pop rax",
                "mov [rax + 0x00], rbx",
                "mov [rax + 0x08], rcx",
                "mov [rax + 0x10], rdx",
                "mov [rax + 0x18], rsi",
                "mov [rax + 0x20], rdi",
                "mov [rax + 0x28], rbp",
                "mov [rax + 0x30], r8",
                "mov [rax + 0x38], r9",
                "mov [rax + 0x40], r10",
                "mov [rax + 0x48], r11",
                "mov [rax + 0x50], r12",
                "mov [rax + 0x58], r13",
                "mov [rax + 0x60], r14",
                "mov [rax + 0x68], r15",
                "pop rbp",              // restore host rbp
                "pop rbx",              // restore host rbx
                inout("rax") vmcb_pa => _,
                inout("rdi") gprs => _,
                out("rcx") _,
                out("rdx") _,
                out("rsi") _,
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

        // The guest's non-VMCB GPRs, carried across every VMRUN by the shell so
        // register state survives an intercepted-and-resumed instruction.
        // Starts zeroed; the guest sets what it uses.
        let mut gprs = super::GuestGprs::default();
        let mut outcome = super::GuestRunOutcome::new();
        loop {
            // SAFETY: the caller guarantees a VMRUN-ready VMCB with SVM on, and
            // `gprs` is a live local.
            unsafe { vmrun(vmcb_pa, &raw mut gprs) };
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
    fn guest_gprs_layout_is_stable_for_the_asm_shell() {
        use core::mem::{offset_of, size_of};
        // The run shell's inline asm indexes GuestGprs by these fixed offsets;
        // this test is the contract that pins them.
        assert_eq!(offset_of!(GuestGprs, rbx), 0x00);
        assert_eq!(offset_of!(GuestGprs, rcx), 0x08);
        assert_eq!(offset_of!(GuestGprs, rdx), 0x10);
        assert_eq!(offset_of!(GuestGprs, rsi), 0x18);
        assert_eq!(offset_of!(GuestGprs, rdi), 0x20);
        assert_eq!(offset_of!(GuestGprs, rbp), 0x28);
        assert_eq!(offset_of!(GuestGprs, r8), 0x30);
        assert_eq!(offset_of!(GuestGprs, r9), 0x38);
        assert_eq!(offset_of!(GuestGprs, r10), 0x40);
        assert_eq!(offset_of!(GuestGprs, r11), 0x48);
        assert_eq!(offset_of!(GuestGprs, r12), 0x50);
        assert_eq!(offset_of!(GuestGprs, r13), 0x58);
        assert_eq!(offset_of!(GuestGprs, r14), 0x60);
        assert_eq!(offset_of!(GuestGprs, r15), 0x68);
        assert_eq!(size_of::<GuestGprs>(), 0x70);
    }

    #[test]
    fn guest_expected_out_is_the_low_byte_of_the_stash() {
        assert_eq!(GUEST_EXPECTED_OUT, 0x34);
        assert_eq!(u16::from(GUEST_EXPECTED_OUT), GUEST_BX_STASH & 0xFF);
    }

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

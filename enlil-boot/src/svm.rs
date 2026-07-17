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

/// The port the boot guest writes the byte it read from an emulated MSR to —
/// distinct from [`GUEST_IO_PORT`] so both the CPUID and MSR proofs survive in
/// [`GuestRunOutcome`].
pub const GUEST_MSR_PORT: u8 = 0x81;

/// The port the boot guest writes `CPUID.1:ECX[31]` (the hypervisor-present
/// bit) to. With enlil's stealth it must read `0` — the in-guest proof that
/// enlil hides itself (LOCKED PRINCIPLE 1).
pub const GUEST_HV_BIT_PORT: u8 = 0x82;

/// The port the boot guest writes the byte it read from the demand-paged page
/// to — proving the NPF was caught and the mapped page reached the guest.
pub const GUEST_NPF_PORT: u8 = 0x83;

/// The guest-physical address the boot guest reads to trigger a nested page
/// fault: 2 MiB, just past its initially-mapped `[0, 2 MiB)` RAM window, so the
/// access faults and enlil demand-maps it.
pub const GUEST_NPF_GPA: u64 = 0x0020_0000;

/// The byte enlil writes at the demand-paged page's base; the guest reads and
/// `OUT`s it, so a match proves the demand-mapped page is the one the guest
/// sees.
pub const GUEST_NPF_SENTINEL: u8 = 0x3C;

/// The MSR the boot guest reads to prove MSR interception. enlil intercepts it
/// and injects [`GUEST_MSR_SENTINEL`] rather than the real value.
pub const GUEST_MSR_NUMBER: u32 = 0x10;

/// The `EAX` value enlil injects for a guest read of [`GUEST_MSR_NUMBER`] — a
/// sentinel whose low byte the guest `OUT`s, proving the RDMSR was intercepted
/// and the spoofed value reached the guest.
pub const GUEST_MSR_SENTINEL: u32 = 0x5A;

/// The MSR the boot guest writes then reads back to prove WRMSR/RDMSR state
/// virtualization. enlil intercepts both and shadows the written value.
pub const GUEST_MSR_W_NUMBER: u32 = 0x11;

/// The port the boot guest writes the value it read back from
/// [`GUEST_MSR_W_NUMBER`] to.
pub const GUEST_MSR_W_PORT: u8 = 0x84;

/// The `EAX` the boot guest writes to [`GUEST_MSR_W_NUMBER`]; reading it back
/// through enlil's shadow must return the same value.
pub const GUEST_MSR_W_VALUE: u8 = 0x99;

/// The port the boot guest writes the result of a native compute loop to —
/// proving it runs real code (arithmetic + a taken branch) at native speed with
/// no #VMEXIT until the `OUT`.
pub const GUEST_COMPUTE_PORT: u8 = 0x85;

/// The loop count the compute loop runs; it sums `1..=N`, so the result is
/// `N*(N+1)/2` ([`GUEST_COMPUTE_SUM`]).
pub const GUEST_COMPUTE_N: u8 = 5;

/// The expected compute-loop result — `sum(1..=GUEST_COMPUTE_N)`.
pub const GUEST_COMPUTE_SUM: u8 = GUEST_COMPUTE_N * (GUEST_COMPUTE_N + 1) / 2;

/// The port the boot guest writes the byte it read through its `GS` segment to.
///
/// `VMRUN` does not load `FS`/`GS`/`TR`/`LDTR` — only `VMLOAD` does — so a
/// correct read here proves the run shell's `VMSAVE`/`VMLOAD` swap loaded the
/// guest's extended segment state before entry (ROADMAP 6.2).
pub const GUEST_GS_PORT: u8 = 0x86;

/// Byte offset within guest RAM where the `GS` sentinel is planted — the `usize`
/// form of [`GUEST_GS_BASE`] used to index the RAM slice.
const GUEST_GS_BASE_OFF: usize = 0x1000;

/// The guest-physical address the boot guest's `GS` base is programmed to.
///
/// Also where [`GUEST_GS_SENTINEL`] is planted: inside the guest's `[0, 2 MiB)`
/// RAM window and past the program, so the `GS`-relative read hits a mapped,
/// known byte.
pub const GUEST_GS_BASE: u64 = GUEST_GS_BASE_OFF as u64;

/// The byte planted at [`GUEST_GS_BASE`]; the guest reads it via `GS:[0]` and
/// `OUT`s it, so a match proves `VMLOAD` loaded the guest `GS` base from the
/// VMCB (a value `VMRUN` alone never installs).
pub const GUEST_GS_SENTINEL: u8 = 0x5E;

/// The interrupt vector enlil injects into the event-injection guest.
///
/// Delivered via the VMCB `EVENTINJ` field. Chosen above the guest program so
/// its real-mode IVT slot (`4 * vector`) does not overlap the code at GPA 0.
pub const GUEST_EVENT_VECTOR: u8 = 0x20;

/// Guest RAM offset (a 16-bit real-mode offset) of the handler.
///
/// The IVT slot for [`GUEST_EVENT_VECTOR`] points at segment 0, this offset.
pub const GUEST_EVENT_HANDLER_OFF: u16 = 0x0200;

/// The port the event-injection guest's handler writes its sentinel to. A
/// capture here proves the injected interrupt was delivered and vectored
/// through the guest's IVT to the handler.
pub const GUEST_EVENT_PORT: u8 = 0x87;

/// The byte the event-injection guest's handler `OUT`s. Present in the run's
/// I/O record iff [`GUEST_EVENT_VECTOR`] was injected and the handler ran.
pub const GUEST_EVENT_SENTINEL: u8 = 0x7E;

/// A small per-guest shadow of MSRs the guest has written with `WRMSR`.
///
/// A later `RDMSR` reads back what the guest wrote — MSR-state virtualization
/// (the guest owns its MSR view; the write never reaches host hardware).
#[derive(Debug, Clone, Copy)]
pub struct MsrShadow {
    entries: [(u32, u64); Self::CAP],
    count: usize,
}

impl MsrShadow {
    /// How many distinct MSRs the shadow holds (a bring-up guest writes few).
    pub const CAP: usize = 4;

    /// An empty shadow.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [(0, 0); Self::CAP],
            count: 0,
        }
    }

    /// Store `value` for `msr` (updating an existing entry, else appending;
    /// dropped silently past [`CAP`](Self::CAP)).
    pub const fn set(&mut self, msr: u32, value: u64) {
        let mut i = 0;
        while i < self.count {
            if self.entries[i].0 == msr {
                self.entries[i].1 = value;
                return;
            }
            i += 1;
        }
        if self.count < Self::CAP {
            self.entries[self.count] = (msr, value);
            self.count += 1;
        }
    }

    /// The shadowed value of `msr`, if the guest has written it.
    #[must_use]
    pub const fn get(&self, msr: u32) -> Option<u64> {
        let mut i = 0;
        while i < self.count {
            if self.entries[i].0 == msr {
                return Some(self.entries[i].1);
            }
            i += 1;
        }
        None
    }
}

impl Default for MsrShadow {
    fn default() -> Self {
        Self::new()
    }
}

/// The stealth value enlil returns for a guest `RDMSR` of `msr`, or `None` to
/// pass the real MSR through.
///
/// Minimal bare-metal MSR stealth (LOCKED PRINCIPLE 1): only the demo MSR
/// [`GUEST_MSR_NUMBER`] is spoofed today (to `EDX:EAX = 0:sentinel`); the real
/// per-MSR policy (offset TSC, hidden hypervisor MSRs, shadowed PMCs) converges
/// with `enlil-core`'s stealth later (ROADMAP 6.2). Returns `(eax, edx)`.
#[must_use]
pub const fn stealth_msr_read(msr: u32) -> Option<(u32, u32)> {
    if msr == GUEST_MSR_NUMBER {
        Some((GUEST_MSR_SENTINEL, 0))
    } else {
        None
    }
}

/// The four registers `CPUID` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuidRegs {
    /// EAX result.
    pub eax: u32,
    /// EBX result.
    pub ebx: u32,
    /// ECX result.
    pub ecx: u32,
    /// EDX result.
    pub edx: u32,
}

/// CPUID leaf 1 ECX bit 31 — "hypervisor present". Cleared for stealth.
const CPUID_1_ECX_HYPERVISOR: u32 = 1 << 31;
/// Base of the hypervisor CPUID vendor range (`0x4000_0000..=0x4000_00FF`).
const CPUID_HV_RANGE_BASE: u32 = 0x4000_0000;
/// End of the hypervisor CPUID vendor range.
const CPUID_HV_RANGE_END: u32 = 0x4000_00FF;

/// Apply enlil's bare-metal CPUID stealth to a raw host `CPUID` result.
///
/// LOCKED PRINCIPLE 1 — the guest must not see that it runs under enlil. Clears
/// `CPUID.1:ECX[31]` (hypervisor-present) and zeros the whole hypervisor vendor
/// leaf range `0x4000_0000..=0x4000_00FF`. Other leaves pass through unchanged;
/// the topology/PMU/frequency stealth is the `enlil-core` `CpuidStealthTable`'s
/// job, with which this minimal bare-metal surface should later converge
/// (ROADMAP 6.2).
#[must_use]
pub const fn sanitize_cpuid(leaf: u32, regs: CpuidRegs) -> CpuidRegs {
    if leaf >= CPUID_HV_RANGE_BASE && leaf <= CPUID_HV_RANGE_END {
        return CpuidRegs {
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        };
    }
    let mut regs = regs;
    if leaf == 1 {
        regs.ecx &= !CPUID_1_ECX_HYPERVISOR;
    }
    regs
}

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
    /// Intercepted `RDMSR`/`WRMSR` exits handled.
    pub msr_exits: u32,
    /// Nested page faults demand-mapped.
    pub npf_exits: u32,
    /// The `(port, byte)` writes the guest performed, oldest first, up to
    /// [`MAX_IO_OUTS`](Self::MAX_IO_OUTS) — each captured through the
    /// arch-neutral `VmExit::IoOut`. Query with [`io_out_to`](Self::io_out_to).
    pub io_outs: [(u16, u32); Self::MAX_IO_OUTS],
    /// How many entries of [`io_outs`](Self::io_outs) are valid.
    pub io_out_count: usize,
    /// The `EBX` enlil emulated for a guest `CPUID` leaf 0, if the guest ran
    /// one — the host vendor string's first word, delivered to the guest via
    /// the GPR shell. Lets the caller confirm the guest received exactly what
    /// enlil computed.
    pub cpuid_leaf0_ebx: Option<u32>,
    /// Raw #VMEXIT code of the final (stopping) exit.
    pub final_exit: u64,
}

impl GuestRunOutcome {
    /// Cap on recorded port writes (a bring-up guest does only a handful).
    pub const MAX_IO_OUTS: usize = 8;

    /// A fresh outcome before the first `VMRUN`.
    ///
    /// Only the firmware run loop (and host tests) construct one, so it is gated
    /// to those configs to stay dead-code-clean on the plain host build.
    #[cfg(any(target_os = "uefi", test))]
    #[must_use]
    const fn new() -> Self {
        Self {
            stop: RunStop::IterationCap,
            vmruns: 0,
            cpuid_exits: 0,
            io_exits: 0,
            msr_exits: 0,
            npf_exits: 0,
            io_outs: [(0, 0); Self::MAX_IO_OUTS],
            io_out_count: 0,
            cpuid_leaf0_ebx: None,
            final_exit: 0,
        }
    }

    /// Record a guest `OUT` (dropped silently past the cap).
    #[cfg(any(target_os = "uefi", test))]
    const fn record_io_out(&mut self, port: u16, data: u32) {
        if self.io_out_count < Self::MAX_IO_OUTS {
            self.io_outs[self.io_out_count] = (port, data);
            self.io_out_count += 1;
        }
    }

    /// The data of the last recorded `OUT` to `port`, or `None`.
    #[must_use]
    pub const fn io_out_to(&self, port: u16) -> Option<u32> {
        let mut found = None;
        let mut i = 0;
        while i < self.io_out_count {
            if self.io_outs[i].0 == port {
                found = Some(self.io_outs[i].1);
            }
            i += 1;
        }
        found
    }

    /// The last recorded `OUT`, as `(port, data)`, or `None`.
    #[must_use]
    pub const fn last_io_out(&self) -> Option<(u16, u32)> {
        if self.io_out_count == 0 {
            None
        } else {
            Some(self.io_outs[self.io_out_count - 1])
        }
    }
}

#[cfg(target_os = "uefi")]
pub use hw::{
    enable_svm, program_boot_vmcb, program_event_inj_vmcb, program_host_save_area,
    run_boot_guest_loop,
};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{GUEST_IO_PORT, GUEST_MSR_NUMBER, GUEST_NPF_GPA};
    use super::{
        HSAVE_PAGE_SIZE, MSR_EFER, MSR_VM_CR, MSR_VM_HSAVE_PA, SvmStatus, efer_with_svme,
        is_svm_enabled, is_valid_hsave_pa, svm_status, vm_cr_clear_svmdis,
    };
    use alloc::alloc::{Layout, alloc_zeroed};
    use enlil_hal::npt::build_npt_2mib;
    use enlil_hal::region::{IoPermissionsMap, MsrPermissionsMap, Vmcb};
    use enlil_hal::svm::{
        MinimalGuestSetup, VmcbSegment, control, enable_io_intercept, enable_msr_intercept,
        encode_event_inj, event_type, program_minimal_hlt_guest, save, set_event_inj,
        write_segment,
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

    /// Execute the host `CPUID` for `leaf`/`subleaf`, returning all four result
    /// registers.
    ///
    /// The enlil kernel runs this on the guest's behalf when it intercepts a
    /// guest `CPUID` (LOCKED PRINCIPLE 1), then stealths the result with
    /// [`sanitize_cpuid`](super::sanitize_cpuid) before delivering it. `rbx` is
    /// hand-preserved (LLVM-reserved) via a scratch register.
    fn host_cpuid(leaf: u32, subleaf: u32) -> super::CpuidRegs {
        let eax: u32;
        let ebx: u32;
        let ecx: u32;
        let edx: u32;
        // SAFETY: CPUID is unprivileged and has no memory or side effects; the
        // caller passes a leaf the guest requested.
        unsafe {
            core::arch::asm!(
                "push rbx",
                "cpuid",
                "mov {ebx_out:e}, ebx",
                "pop rbx",
                inout("eax") leaf => eax,
                inout("ecx") subleaf => ecx,
                out("edx") edx,
                ebx_out = out(reg) ebx,
                options(nostack, preserves_flags),
            );
        }
        super::CpuidRegs { eax, ebx, ecx, edx }
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

    /// A 3-page buffer (PML4 + PDPT + one PD) for a ≤ 1 GiB nested map.
    #[repr(C, align(4096))]
    struct NptBuf([u8; 3 * 4096]);

    /// The guest's RAM: one 2 MiB huge page, 2 MiB-aligned so a single NPT
    /// huge-page leaf maps it (`build_npt_2mib`). The guest sees it at GPA 0
    /// though it lives at this region's system-physical base — the isolation
    /// model (LOCKED PRINCIPLE 5).
    #[repr(C, align(0x20_0000))]
    struct GuestRam([u8; GUEST_RAM_BYTES]);

    /// Size of the guest's isolated RAM window (2 MiB).
    const GUEST_RAM_BYTES: usize = 2 * 1024 * 1024;

    /// Write the real-mode guest program into `bytes` at offset 0 (see the
    /// [`program_boot_vmcb`] instruction listing).
    const fn write_guest_program(bytes: &mut [u8]) {
        let msr = GUEST_MSR_NUMBER.to_le_bytes();
        let msr_w = super::GUEST_MSR_W_NUMBER.to_le_bytes();
        bytes[0] = 0xB8; // mov ax, 0
        bytes[1] = 0x00;
        bytes[2] = 0x00;
        bytes[3] = 0x0F; // CPUID (leaf 0)
        bytes[4] = 0xA2;
        bytes[5] = 0x89; // mov ax, bx
        bytes[6] = 0xD8;
        bytes[7] = 0xE6; // OUT imm8, AL
        bytes[8] = GUEST_IO_PORT;
        bytes[9] = 0xB8; // mov ax, 1
        bytes[10] = 0x01;
        bytes[11] = 0x00;
        bytes[12] = 0x0F; // CPUID (leaf 1)
        bytes[13] = 0xA2;
        bytes[14] = 0x66; // shr ecx, 31
        bytes[15] = 0xC1;
        bytes[16] = 0xE9;
        bytes[17] = 0x1F;
        bytes[18] = 0x89; // mov ax, cx
        bytes[19] = 0xC8;
        bytes[20] = 0xE6; // OUT imm8, AL
        bytes[21] = super::GUEST_HV_BIT_PORT;
        bytes[22] = 0x66; // mov ecx, imm32 (read-target MSR)
        bytes[23] = 0xB9;
        bytes[24] = msr[0];
        bytes[25] = msr[1];
        bytes[26] = msr[2];
        bytes[27] = msr[3];
        bytes[28] = 0x0F; // RDMSR
        bytes[29] = 0x32;
        bytes[30] = 0xE6; // OUT imm8, AL
        bytes[31] = super::GUEST_MSR_PORT;
        bytes[32] = 0x66; // mov ecx, imm32 (write-target MSR)
        bytes[33] = 0xB9;
        bytes[34] = msr_w[0];
        bytes[35] = msr_w[1];
        bytes[36] = msr_w[2];
        bytes[37] = msr_w[3];
        bytes[38] = 0xB8; // mov ax, VALUE (EAX low; EDX still 0 from rdmsr)
        bytes[39] = super::GUEST_MSR_W_VALUE;
        bytes[40] = 0x00;
        bytes[41] = 0x0F; // WRMSR (intercepted → shadowed, never hits HW)
        bytes[42] = 0x30;
        bytes[43] = 0x0F; // RDMSR (ECX still the write-target → shadow value)
        bytes[44] = 0x32;
        bytes[45] = 0xE6; // OUT imm8, AL
        bytes[46] = super::GUEST_MSR_W_PORT;
        bytes[47] = 0xA1; // mov ax, [0]  (DS:0 → GPA 2 MiB, NPF)
        bytes[48] = 0x00;
        bytes[49] = 0x00;
        bytes[50] = 0xE6; // OUT imm8, AL
        bytes[51] = super::GUEST_NPF_PORT;
        // Native compute loop: sum 1..=N with a taken branch and NO #VMEXIT
        // until the OUT — proving near-native guest execution.
        bytes[52] = 0x31; // xor ax, ax     (accumulator = 0)
        bytes[53] = 0xC0;
        bytes[54] = 0xB9; // mov cx, N       (counter)
        bytes[55] = super::GUEST_COMPUTE_N;
        bytes[56] = 0x00;
        bytes[57] = 0x01; // add ax, cx      ← loop target
        bytes[58] = 0xC8;
        bytes[59] = 0xE2; // loop -4         (dec cx; jump to add while cx != 0)
        bytes[60] = 0xFC;
        bytes[61] = 0xE6; // OUT imm8, AL     (= sum(1..=N))
        bytes[62] = super::GUEST_COMPUTE_PORT;
        // Read a byte through GS (base loaded from the VMCB by VMLOAD only) and
        // OUT it — proving the run shell's VMSAVE/VMLOAD extended-state swap.
        bytes[63] = 0x65; // GS segment override
        bytes[64] = 0xA0; // MOV AL, moffs16
        bytes[65] = 0x00; // offset 0x0000 (16-bit) → GS:[0]
        bytes[66] = 0x00;
        bytes[67] = 0xE6; // OUT imm8, AL
        bytes[68] = super::GUEST_GS_PORT;
        bytes[69] = 0xF4; // HLT
        // Plant the sentinel the GS-relative read expects at GUEST_GS_BASE.
        bytes[super::GUEST_GS_BASE_OFF] = super::GUEST_GS_SENTINEL;
    }

    /// Build a complete, `VMRUN`-ready VMCB for a minimal real-mode guest that
    /// exercises the exit-handling path, returning `(vmcb_pa, ncr3,
    /// guest_code_pa)`.
    ///
    /// The guest exercises every routed exit class: it runs `CPUID` leaf 0 and
    /// `OUT`s the emulated vendor byte, then `RDMSR` and `OUT`s the emulated MSR
    /// byte, then `HLT`s — so the dispatch loop routes `CPUID` (emulated),
    /// `IOIO` (twice), `MSR` (emulated), and `HLT`. enlil answers the CPUID by
    /// running the host CPUID + stealthing it, and answers the RDMSR by
    /// injecting a sentinel; both results reach the guest through the GPR shell,
    /// and the two `OUT`s (to distinct ports) let the kernel confirm each. All
    /// assembled through `enlil-hal`: the code page, a nested page table
    /// identity-mapping the low gibibyte (so `GPA == SPA`), a [`Vmcb`] programmed
    /// via [`program_minimal_hlt_guest`] then armed for port I/O
    /// ([`enable_io_intercept`] + an intercept-all [`IoPermissionsMap`]) and MSR
    /// access ([`enable_msr_intercept`] + an [`MsrPermissionsMap`]). The
    /// nested-CR3 is read back to prove the write landed. All allocations are
    /// leaked — they must outlive the `VMRUN`. Returns `None` if any allocation
    /// fails or the NPT/VMCB is malformed (none can happen here).
    ///
    /// The guest runs in its own isolated [`GuestRam`] window (LOCKED PRINCIPLE
    /// 5): it sees its code at GPA 0, but that RAM lives at a disjoint
    /// system-physical base the NPT ([`build_npt_2mib`]) maps GPA 0 onto — not
    /// the hypervisor's own heap addresses.
    #[must_use]
    pub fn program_boot_vmcb() -> Option<(u64, u64, u64)> {
        // Allocate the guest's isolated RAM (2 MiB, 2 MiB-aligned) and write the
        // real-mode guest program into it at GPA 0:
        //   mov ax, 0        B8 00 00        (EAX = 0 → CPUID leaf 0)
        //   cpuid            0F A2           (intercepted → emulated)
        //   mov ax, bx       89 D8           (AX = emulated EBX low word)
        //   out 0x80, al     E6 80           (IOIO → captured at port 0x80)
        //   mov ax, 1        B8 01 00        (EAX = 1 → CPUID leaf 1)
        //   cpuid            0F A2           (intercepted → stealthed)
        //   shr ecx, 31      66 C1 E9 1F     (ECX bit 0 = hypervisor-present)
        //   mov ax, cx       89 C8
        //   out 0x82, al     E6 82           (IOIO → 0 if enlil is hidden)
        //   mov ecx, 0x10    66 B9 10 00 00 00  (ECX = MSR number, full 32-bit)
        //   rdmsr            0F 32           (intercepted → sentinel injected)
        //   out 0x81, al     E6 81           (IOIO → captured at port 0x81)
        //   mov ecx, 0x11    66 B9 11 00 00 00  (ECX = write-target MSR)
        //   mov ax, 0x99     B8 99 00        (EAX = value; EDX still 0)
        //   wrmsr            0F 30           (intercepted → shadowed, not to HW)
        //   rdmsr            0F 32           (intercepted → reads the shadow back)
        //   out 0x84, al     E6 84           (IOIO → the shadowed value)
        //   mov ax, [0]      A1 00 00        (DS:0 = GPA 2 MiB → NPF, demand-mapped)
        //   out 0x83, al     E6 83           (IOIO → the demand-paged sentinel)
        //   xor ax, ax / mov cx, N / add ax, cx / loop -4  (native sum 1..=N, no exit)
        //   out 0x85, al     E6 85           (IOIO → the computed sum)
        //   mov al, gs:[0]   65 A0 00 00     (GS.base loaded by VMLOAD only)
        //   out 0x86, al     E6 86           (IOIO → the GS sentinel)
        //   hlt              F4              (clean stop)
        // SAFETY: GuestRam has a nonzero size; alloc_zeroed yields a zeroed,
        // 2 MiB-aligned GuestRam-sized block or null.
        let ram_raw = unsafe { alloc_zeroed(Layout::new::<GuestRam>()) };
        if ram_raw.is_null() {
            return None;
        }
        let guest_spa = ram_raw as u64;
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes (leaked below).
        write_guest_program(unsafe { core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES) });
        // The guest's own view of its code: GPA 0.
        let guest_code_gpa = 0u64;

        // Nested page tables mapping guest GPA [0, 2 MiB) onto the isolated RAM
        // window at `guest_spa` (GPA 0 → guest_spa) — not an identity map.
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
        let ncr3 = build_npt_2mib(npt_buf, npt_pa, guest_spa, GUEST_RAM_BYTES as u64)
            .ok()?
            .ncr3;

        // Program a VMCB to enter the guest code at GPA 0 under that NPT.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            entry_ip: 0,
            code_base: guest_code_gpa,
            stack_pointer: 0,
        };
        program_minimal_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;
        // Point the guest's DS at the unmapped GPA so its `mov ax, [0]` reads
        // GPA GUEST_NPF_GPA and takes a nested page fault enlil demand-maps.
        // (SVM allows an arbitrary real-mode segment base — unreal mode.)
        write_segment(
            vmcb.as_bytes_mut(),
            save::DS,
            VmcbSegment::real_mode_data(GUEST_NPF_GPA),
        );
        // Point the guest's GS at GUEST_GS_BASE. VMRUN never loads FS/GS/TR/LDTR;
        // only the run shell's VMLOAD does, so the guest reading the sentinel at
        // GS:[0] proves the extended-state swap ran.
        write_segment(
            vmcb.as_bytes_mut(),
            save::GS,
            VmcbSegment::real_mode_data(super::GUEST_GS_BASE),
        );
        // Read the nested-CR3 back to confirm the programming landed.
        let mut ncr3_le = [0u8; 8];
        ncr3_le.copy_from_slice(&vmcb.as_bytes()[control::NESTED_CR3..control::NESTED_CR3 + 8]);
        if u64::from_le_bytes(ncr3_le) != ncr3 {
            return None;
        }

        // Arm port-I/O interception so the guest's OUT takes an IOIO #VMEXIT.
        // The guest loads AL itself, so no RAX preload is needed. The IOPM
        // intercepts every port; it is leaked so it outlives the VMRUN the CPU
        // checks it against.
        let iopm = IoPermissionsMap::intercept_all().ok()?;
        enable_io_intercept(vmcb.as_bytes_mut(), iopm.base_addr());
        core::mem::forget(iopm);

        // Arm MSR interception for the one MSR the guest reads, so its RDMSR
        // takes an MSR #VMEXIT enlil answers with a sentinel. Only that MSR is
        // intercepted, so nothing else the guest might touch traps. Leaked to
        // outlive VMRUN.
        let mut msrpm = MsrPermissionsMap::new().ok()?;
        msrpm.set_intercept(GUEST_MSR_NUMBER, true, false);
        // Intercept both read and write of the write-target MSR so its WRMSR is
        // shadowed (never reaches hardware) and its RDMSR reads the shadow.
        msrpm.set_intercept(super::GUEST_MSR_W_NUMBER, true, true);
        enable_msr_intercept(vmcb.as_bytes_mut(), msrpm.base_addr());
        core::mem::forget(msrpm);

        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // the VMCB must outlive this call for VMRUN
        Some((vmcb_pa, ncr3, guest_code_gpa))
    }

    /// Build a `VMRUN`-ready VMCB that proves **event injection**, returning
    /// `(vmcb_pa, handler_gpa)`.
    ///
    /// The VMCB `EVENTINJ` field is armed so `VMRUN` delivers
    /// [`GUEST_EVENT_VECTOR`](super::GUEST_EVENT_VECTOR) as an external
    /// interrupt before the guest's first instruction (APM §15.20). The guest's
    /// real-mode IVT slot for that vector (`4 * vector`) points at a handler
    /// that `OUT`s [`GUEST_EVENT_SENTINEL`](super::GUEST_EVENT_SENTINEL) and
    /// `HLT`s. The dispatch loop clears `EVENTINJ` after the first entry so the
    /// event fires exactly once.
    ///
    /// The entry code at GPA 0 is a bare `HLT` — the "injection did not fire"
    /// path. If injection works the CPU never runs it: it reads the IVT slot,
    /// pushes FLAGS/CS/IP, and jumps to the handler, whose `OUT` the loop
    /// captures. A sentinel in the run's I/O record therefore means the
    /// injected interrupt was delivered and handled; its absence means it was
    /// not. Assembled entirely through `enlil-hal` (RAM + IVT + handler bytes,
    /// an NPT mapping the low GiB, a [`Vmcb`] with the I/O intercept and
    /// `EVENTINJ` armed via [`encode_event_inj`]); allocations are leaked to
    /// outlive `VMRUN`. Returns `None` on any allocation/programming failure.
    #[must_use]
    pub fn program_event_inj_vmcb() -> Option<(u64, u64)> {
        use super::{
            GUEST_EVENT_HANDLER_OFF, GUEST_EVENT_PORT, GUEST_EVENT_SENTINEL, GUEST_EVENT_VECTOR,
        };

        // Allocate + populate the guest's isolated RAM.
        // SAFETY: GuestRam is a nonzero, 2 MiB-aligned block; alloc_zeroed
        // yields it zeroed or null.
        let ram_raw = unsafe { alloc_zeroed(Layout::new::<GuestRam>()) };
        if ram_raw.is_null() {
            return None;
        }
        let guest_spa = ram_raw as u64;
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes (leaked below).
        let ram = unsafe { core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES) };
        // Entry code at GPA 0: a bare HLT (the injection-did-not-fire path).
        ram[0] = 0xF4;
        // Real-mode IVT slot for the vector: offset (u16 LE) then segment (u16
        // LE), pointing at segment 0, offset GUEST_EVENT_HANDLER_OFF.
        let ivt = (GUEST_EVENT_VECTOR as usize) * 4;
        let off = GUEST_EVENT_HANDLER_OFF.to_le_bytes();
        ram[ivt] = off[0];
        ram[ivt + 1] = off[1];
        ram[ivt + 2] = 0x00; // segment low
        ram[ivt + 3] = 0x00; // segment high
        // Handler: mov al, SENTINEL; out PORT, al; hlt.
        let h = GUEST_EVENT_HANDLER_OFF as usize;
        ram[h] = 0xB0; // MOV AL, imm8
        ram[h + 1] = GUEST_EVENT_SENTINEL;
        ram[h + 2] = 0xE6; // OUT imm8, AL
        ram[h + 3] = GUEST_EVENT_PORT;
        ram[h + 4] = 0xF4; // HLT

        // NPT mapping guest GPA [0, 2 MiB) onto the isolated RAM window.
        // SAFETY: NptBuf is nonzero, 4 KiB-aligned; alloc_zeroed yields it or null.
        let npt_raw = unsafe { alloc_zeroed(Layout::new::<NptBuf>()) };
        if npt_raw.is_null() {
            return None;
        }
        let npt_pa = npt_raw as u64;
        // SAFETY: npt_raw owns a live, zeroed NptBuf, leaked below.
        let npt_buf = unsafe { core::slice::from_raw_parts_mut(npt_raw, 3 * 4096) };
        let ncr3 = build_npt_2mib(npt_buf, npt_pa, guest_spa, GUEST_RAM_BYTES as u64)
            .ok()?
            .ncr3;

        // Program a VMCB entering the bare-HLT at GPA 0 under that NPT.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            entry_ip: 0,
            code_base: 0,
            stack_pointer: 0,
        };
        program_minimal_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;
        // program_minimal_hlt_guest zeroes the VMCB, leaving IDTR limit 0 — the
        // real-mode IVT would then not cover GUEST_EVENT_VECTOR's slot and the
        // injected interrupt would fault instead of vectoring. Install the
        // standard real-mode IVT (base 0, limit 0x3FF covers all 256 vectors).
        write_segment(
            vmcb.as_bytes_mut(),
            save::IDTR,
            VmcbSegment {
                selector: 0,
                attrib: 0,
                limit: 0x03FF,
                base: 0,
            },
        );

        // Arm port-I/O interception so the handler's OUT takes an IOIO #VMEXIT.
        let iopm = IoPermissionsMap::intercept_all().ok()?;
        enable_io_intercept(vmcb.as_bytes_mut(), iopm.base_addr());
        core::mem::forget(iopm);

        // Arm EVENTINJ: VMRUN injects GUEST_EVENT_VECTOR as an external interrupt
        // before the first guest instruction; the run loop clears it after entry.
        let inj = encode_event_inj(GUEST_EVENT_VECTOR, event_type::EXTERNAL_INTERRUPT, None);
        set_event_inj(vmcb.as_bytes_mut(), inj);

        let handler_gpa = u64::from(GUEST_EVENT_HANDLER_OFF);
        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // must outlive VMRUN
        Some((vmcb_pa, handler_gpa))
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
    /// at a live [`GuestGprs`](super::GuestGprs); `host_save_pa` must be a
    /// distinct, zeroed 4 KiB-aligned VMCB-format page owned for `VMSAVE`.
    ///
    /// `VMRUN` swaps only `RAX`/`RSP`/`RIP`/`RFLAGS` plus `CS`/`DS`/`ES`/`SS`
    /// through the VMCB; it does **not** touch `FS`/`GS`/`TR`/`LDTR`,
    /// `KernelGSBase`, `STAR`/`LSTAR`/`CSTAR`/`SFMASK`, or the `SYSENTER` MSRs —
    /// those are `VMLOAD`/`VMSAVE`'s domain (APM §15.5.2). So the shell brackets
    /// `VMRUN` with the canonical swap: `VMSAVE` the host's extended state to
    /// `host_save_pa`, `VMLOAD` the guest's from the VMCB, run, then `VMSAVE`
    /// the guest's back and `VMLOAD` the host's — a guest using segmentation or
    /// syscalls is now safe, and the host's `FS`/`GS`/`TR`/`LDTR` survive.
    ///
    /// `vmcb_pa`, `host_save_pa`, and the `gprs` pointer are pushed to the stack
    /// first because the guest clobbers every GPR and `VMRUN` only restores host
    /// `RAX`/`RSP`; they are reloaded from `[rsp+N]` after `#VMEXIT`.
    unsafe fn vmrun(vmcb_pa: u64, host_save_pa: u64, gprs: *mut super::GuestGprs) {
        unsafe {
            core::arch::asm!(
                "push rbx",             // preserve host rbx (callee-saved)
                "push rbp",             // preserve host rbp (callee-saved)
                "push rax",             // [rsp+16] vmcb_pa
                "push rsi",             // [rsp+8]  host_save_pa
                "push rdi",             // [rsp+0]  gprs pointer
                // Swap extended state in: save the host's, load the guest's.
                "mov rax, [rsp + 8]",   // host_save_pa
                "vmsave rax",
                "mov rax, [rsp + 16]",  // vmcb_pa
                "vmload rax",
                // Load guest GPRs from *gprs; guest rdi last (it holds the ptr).
                "mov rdi, [rsp]",
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
                "mov rdi, [rdi + 0x20]", // guest rdi last
                "mov rax, [rsp + 16]",   // vmcb_pa for VMRUN
                "clgi",
                "vmrun rax",
                "stgi",
                // Guest GPRs are live in the registers; VMRUN restored host RSP,
                // so the stack slots are intact. Store the guest values back.
                "mov rax, [rsp]",        // gprs pointer
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
                // Swap extended state back: save the guest's, restore the host's.
                "mov rax, [rsp + 16]",   // vmcb_pa
                "vmsave rax",
                "mov rax, [rsp + 8]",    // host_save_pa
                "vmload rax",
                "add rsp, 24",           // drop gprs ptr, host_save_pa, vmcb_pa
                "pop rbp",               // restore host rbp
                "pop rbx",               // restore host rbx
                inout("rax") vmcb_pa => _,
                inout("rsi") host_save_pa => _,
                inout("rdi") gprs => _,
                out("rcx") _,
                out("rdx") _,
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

    /// Emulate an intercepted guest `CPUID` (LOCKED PRINCIPLE 1).
    ///
    /// Runs the host `CPUID` for the guest's leaf (guest `EAX`) and subleaf
    /// (shell-carried `gprs.rcx`), stealths it ([`sanitize_cpuid`]), and delivers
    /// the result — `EAX` via the VMCB, `EBX`/`ECX`/`EDX` via the GPR shell.
    /// Returns the emulated `EBX` when the leaf was 0 (for the caller's vendor
    /// proof), else `None`.
    fn emulate_cpuid(vmcb: &mut [u8], gprs: &mut super::GuestGprs) -> Option<u32> {
        use enlil_hal::svm::{guest_rax, set_guest_rax};

        let leaf = u32::try_from(guest_rax(vmcb) & 0xFFFF_FFFF).unwrap_or(0);
        let subleaf = u32::try_from(gprs.rcx & 0xFFFF_FFFF).unwrap_or(0);
        let regs = super::sanitize_cpuid(leaf, host_cpuid(leaf, subleaf));
        set_guest_rax(vmcb, u64::from(regs.eax));
        gprs.rbx = u64::from(regs.ebx);
        gprs.rcx = u64::from(regs.ecx);
        gprs.rdx = u64::from(regs.edx);
        if leaf == 0 { Some(regs.ebx) } else { None }
    }

    /// Emulate an intercepted guest `RDMSR`/`WRMSR` (LOCKED PRINCIPLE 1).
    ///
    /// The MSR number is in the guest `ECX` (shell-carried `gprs.rcx`) and the
    /// direction in `EXITINFO1`. A `WRMSR` is shadowed per-guest and never
    /// reaches host hardware; a `RDMSR` returns the shadowed value if the guest
    /// has written one, else enlil's stealth value ([`stealth_msr_read`]), else
    /// nothing — delivered `EAX` via the VMCB, `EDX` via the shell.
    fn emulate_msr(vmcb: &mut [u8], gprs: &mut super::GuestGprs, shadow: &mut super::MsrShadow) {
        use enlil_hal::svm::{exit_info_1, guest_rax, msr_exit_is_write, set_guest_rax};

        let msr = u32::try_from(gprs.rcx & 0xFFFF_FFFF).unwrap_or(0);
        if msr_exit_is_write(exit_info_1(vmcb)) {
            let eax = guest_rax(vmcb) & 0xFFFF_FFFF;
            let edx = gprs.rdx & 0xFFFF_FFFF;
            shadow.set(msr, (edx << 32) | eax);
            return;
        }
        // RDMSR: shadow first (what the guest wrote), else the stealth value.
        let value = shadow.get(msr).or_else(|| {
            super::stealth_msr_read(msr).map(|(eax, edx)| (u64::from(edx) << 32) | u64::from(eax))
        });
        if let Some(v) = value {
            set_guest_rax(vmcb, v & 0xFFFF_FFFF);
            gprs.rdx = v >> 32;
        }
    }

    /// Demand-map the 2 MiB guest page that faulted, reading the faulting GPA
    /// and NPT root from `vmcb`.
    ///
    /// Allocates a fresh 2 MiB frame, stamps [`GUEST_NPF_SENTINEL`] at its base,
    /// and writes the leaf into the guest's NPT (whose `nCR3` — identity-mapped
    /// — is both the root's physical address and its VA). The frame is leaked so
    /// it outlives the guest. Returns `false` if allocation or the leaf write
    /// fails.
    ///
    /// # Safety
    ///
    /// `vmcb` must be a live VMCB whose `NESTED_CR3` points at an
    /// identity-mapped NPT built by [`build_npt_2mib`].
    unsafe fn demand_map_npf(vmcb: &[u8]) -> bool {
        use enlil_hal::npt::{HUGE_2MIB, map_npt_2mib_leaf};
        use enlil_hal::svm::{control, exit_info_2};

        let fault_gpa = exit_info_2(vmcb) & !(HUGE_2MIB - 1);
        // SAFETY: GuestRam has a nonzero size; alloc_zeroed yields a zeroed
        // 2 MiB-aligned block or null.
        let frame = unsafe { alloc_zeroed(Layout::new::<GuestRam>()) };
        if frame.is_null() {
            return false;
        }
        // SAFETY: frame owns the page; we stamp one byte, then leak it below.
        unsafe { frame.write(super::GUEST_NPF_SENTINEL) };
        let frame_spa = frame as u64;

        let mut b = [0u8; 8];
        b.copy_from_slice(&vmcb[control::NESTED_CR3..control::NESTED_CR3 + 8]);
        let ncr3 = u64::from_le_bytes(b);
        // SAFETY: ncr3 is the identity-mapped NPT root (3 pages).
        let npt = unsafe { core::slice::from_raw_parts_mut(ncr3 as *mut u8, 3 * 4096) };
        map_npt_2mib_leaf(npt, ncr3, fault_gpa, frame_spa).is_ok()
    }

    /// Drive the guest at `vmcb_pa` through a real `#VMEXIT` dispatch loop,
    /// returning what the run observed ([`GuestRunOutcome`]).
    ///
    /// Each `#VMEXIT` is classified through the `enlil-hal` seam
    /// ([`classify_run_loop_exit`]) onto the arch-neutral model (LOCKED
    /// PRINCIPLE 2). `HLT`/`SHUTDOWN`/invalid state stop the loop; the two
    /// emulate-and-skip exits resume it:
    ///
    /// - **CPUID** — intercepted for stealth (LOCKED PRINCIPLE 1). The loop
    ///   runs the host `CPUID` for the guest's leaf/subleaf, stealths it
    ///   ([`sanitize_cpuid`](super::sanitize_cpuid) — clears the
    ///   hypervisor-present bit, hides the hypervisor vendor range), delivers
    ///   EAX via the VMCB and EBX/ECX/EDX via the GPR shell, then advances guest
    ///   RIP past the instruction ([`resume_rip_after`] — hardware `NEXT_RIP`,
    ///   or `RIP + CPUID_INSN_LEN` when NRIP-save is absent) and re-`VMRUN`s.
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
        use enlil_hal::svm::VMCB_SIZE;

        /// Bound on total `VMRUN`s so a misbehaving guest cannot spin forever.
        const MAX_VMRUNS: u32 = 32;

        // The guest's non-VMCB GPRs, carried across every VMRUN by the shell so
        // register state survives an intercepted-and-resumed instruction, plus
        // the per-guest MSR shadow (WRMSR values read back on RDMSR). Both start
        // zeroed/empty; the guest sets what it uses.
        let mut gprs = super::GuestGprs::default();
        let mut msr_shadow = super::MsrShadow::new();
        let mut outcome = super::GuestRunOutcome::new();

        // A distinct, zeroed VMCB-format page for the run shell's VMSAVE of the
        // host's extended state (FS/GS/TR/LDTR + SYSENTER/STAR MSRs) — kept
        // separate from VM_HSAVE_PA, which VMRUN uses for its own host save. If
        // it cannot be allocated, fall back to a run without the swap by
        // reporting a failed outcome rather than running with a null page.
        // SAFETY: HsavePage is a nonzero 4 KiB page; alloc_zeroed yields a
        // zeroed, page-aligned block or null.
        let host_save = unsafe { alloc_zeroed(Layout::new::<HsavePage>()) };
        if host_save.is_null() {
            outcome.stop = super::RunStop::Invalid;
            return outcome;
        }
        let host_save_pa = host_save as u64;
        loop {
            // SAFETY: the caller guarantees a VMRUN-ready VMCB with SVM on,
            // `gprs` is a live local, and `host_save_pa` is our owned zeroed page.
            unsafe { vmrun(vmcb_pa, host_save_pa, &raw mut gprs) };
            outcome.vmruns += 1;

            // The VMCB is identity-mapped and exclusively owned (leaked by
            // program_boot_vmcb); the CPU just wrote its exit fields, and the
            // `vmrun` asm may touch memory so these reads are not reordered
            // before it.
            // SAFETY: vmcb_pa points at our live, page-sized VMCB.
            let vmcb = unsafe { core::slice::from_raw_parts_mut(vmcb_pa as *mut u8, VMCB_SIZE) };
            // Consume any armed EVENTINJ so an injected event fires exactly once:
            // VMRUN just delivered it, and leaving the valid bit set would
            // re-inject on every re-entry. A no-op when nothing was armed.
            set_event_inj(vmcb, 0);
            // SAFETY: the VMCB's NPT root is identity-mapped (for demand paging).
            if !unsafe { step_guest(vmcb, &mut gprs, &mut msr_shadow, &mut outcome) } {
                break;
            }
            if outcome.vmruns >= MAX_VMRUNS {
                outcome.stop = super::RunStop::IterationCap;
                break;
            }
        }
        outcome
    }

    /// Handle one `#VMEXIT` on `vmcb`, returning `true` to keep running the
    /// guest or `false` to stop (setting `outcome.stop`).
    ///
    /// Classifies the exit through the `enlil-hal` seam
    /// ([`classify_run_loop_exit`](enlil_hal::svm::classify_run_loop_exit)) onto
    /// the arch-neutral model (LOCKED PRINCIPLE 2): `HLT`/`SHUTDOWN`/invalid
    /// stop; `CPUID`/`RDMSR`/`WRMSR` are emulated-and-skipped
    /// ([`emulate_cpuid`]/[`emulate_msr`]); `IOIO` is decoded + captured and
    /// resumed at the `EXITINFO2` RIP; `NPF` is demand-mapped ([`demand_map_npf`])
    /// and resumed *without* advancing RIP so the access re-executes.
    ///
    /// # Safety
    ///
    /// `vmcb`'s `NESTED_CR3` must point at an identity-mapped NPT (for the NPF
    /// demand-map path).
    unsafe fn step_guest(
        vmcb: &mut [u8],
        gprs: &mut super::GuestGprs,
        msr_shadow: &mut super::MsrShadow,
        outcome: &mut super::GuestRunOutcome,
    ) -> bool {
        use enlil_hal::VmExit;
        use enlil_hal::svm::{
            CPUID_INSN_LEN, IoioExitInfo, MSR_INSN_LEN, RunLoopExit, classify_run_loop_exit,
            exit_code, exit_info_1, exit_info_2, guest_rax, guest_rip, ioio_to_vmexit, next_rip,
            resume_rip_after, set_guest_rip,
        };

        let code = exit_code(vmcb);
        outcome.final_exit = code.raw();
        match classify_run_loop_exit(code) {
            RunLoopExit::Halted => outcome.stop = super::RunStop::Halted,
            RunLoopExit::ShutDown => outcome.stop = super::RunStop::ShutDown,
            RunLoopExit::Invalid => outcome.stop = super::RunStop::Invalid,
            RunLoopExit::Unhandled => outcome.stop = super::RunStop::Unhandled,
            RunLoopExit::Cpuid => {
                outcome.cpuid_exits += 1;
                if let Some(ebx0) = emulate_cpuid(vmcb, gprs) {
                    outcome.cpuid_leaf0_ebx = Some(ebx0);
                }
                let rip = resume_rip_after(next_rip(vmcb), guest_rip(vmcb), CPUID_INSN_LEN);
                set_guest_rip(vmcb, rip);
                return true;
            }
            RunLoopExit::Io => {
                outcome.io_exits += 1;
                let info = IoioExitInfo::from_raw(exit_info_1(vmcb));
                // Low 32 bits of guest RAX supply the OUT data; masking first
                // makes the narrowing total (never truncating).
                let rax = u32::try_from(guest_rax(vmcb) & 0xFFFF_FFFF).unwrap_or(0);
                if let Some(VmExit::IoOut { port, data, .. }) = ioio_to_vmexit(info, rax) {
                    outcome.record_io_out(port, data);
                }
                set_guest_rip(vmcb, exit_info_2(vmcb)); // RIP past the IN/OUT
                return true;
            }
            RunLoopExit::Msr => {
                outcome.msr_exits += 1;
                emulate_msr(vmcb, gprs, msr_shadow);
                let rip = resume_rip_after(next_rip(vmcb), guest_rip(vmcb), MSR_INSN_LEN);
                set_guest_rip(vmcb, rip);
                return true;
            }
            RunLoopExit::Npf => {
                // SAFETY: the caller guarantees an identity-mapped NPT root.
                if unsafe { demand_map_npf(vmcb) } {
                    outcome.npf_exits += 1;
                    return true; // resume WITHOUT advancing RIP
                }
                outcome.stop = super::RunStop::Unhandled;
            }
        }
        false
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
    fn sanitize_cpuid_clears_the_hypervisor_present_bit_on_leaf_1() {
        let raw = CpuidRegs {
            eax: 0x0010_0F10,
            ebx: 0x0080_0800,
            ecx: 0x8000_0201, // bit 31 (hypervisor) set, plus real feature bits
            edx: 0x1783_FBFF,
        };
        let out = sanitize_cpuid(1, raw);
        // Hypervisor-present bit cleared; every other bit preserved.
        assert_eq!(out.ecx, 0x0000_0201);
        assert_eq!(out.eax, raw.eax);
        assert_eq!(out.ebx, raw.ebx);
        assert_eq!(out.edx, raw.edx);
    }

    #[test]
    fn sanitize_cpuid_zeros_the_hypervisor_vendor_range() {
        let raw = CpuidRegs {
            eax: 0x4000_0001,
            ebx: 0x7263_694D, // "Micr..." — a hypervisor signature
            ecx: 0x666F_736F,
            edx: 0x76482074,
        };
        assert_eq!(sanitize_cpuid(0x4000_0000, raw), CpuidRegs::default());
        assert_eq!(sanitize_cpuid(0x4000_00FF, raw), CpuidRegs::default());
    }

    #[test]
    fn stealth_msr_read_spoofs_only_the_demo_msr() {
        assert_eq!(
            stealth_msr_read(GUEST_MSR_NUMBER),
            Some((GUEST_MSR_SENTINEL, 0))
        );
        // Any other MSR passes through (None → the loop does not inject).
        assert_eq!(stealth_msr_read(0x1B), None);
        assert_eq!(stealth_msr_read(0xC000_0080), None);
    }

    #[test]
    fn msr_shadow_stores_updates_and_reads_back() {
        let mut s = MsrShadow::new();
        assert_eq!(s.get(0x11), None);
        s.set(0x11, 0x99);
        assert_eq!(s.get(0x11), Some(0x99));
        // Update in place, not append.
        s.set(0x11, 0xDEAD_BEEF);
        assert_eq!(s.get(0x11), Some(0xDEAD_BEEF));
        // A distinct MSR is a separate slot (slots 0,1 now used).
        s.set(0x174, 0x1234);
        assert_eq!(s.get(0x174), Some(0x1234));
        assert_eq!(s.get(0x11), Some(0xDEAD_BEEF));
        // Fill the remaining 2 slots, then overflow: the extra writes are
        // dropped (no panic), earlier entries survive.
        s.set(0x200, 2);
        s.set(0x201, 3);
        s.set(0x202, 4); // past CAP=4 → dropped
        assert_eq!(s.get(0x200), Some(2));
        assert_eq!(s.get(0x201), Some(3));
        assert_eq!(s.get(0x202), None);
        assert_eq!(s.get(0x11), Some(0xDEAD_BEEF));
    }

    #[test]
    fn io_out_log_records_and_queries_by_port() {
        let mut o = GuestRunOutcome::new();
        assert_eq!(o.last_io_out(), None);
        o.record_io_out(0x80, 0x41);
        o.record_io_out(0x81, 0x5A);
        assert_eq!(o.io_out_count, 2);
        assert_eq!(o.io_out_to(0x80), Some(0x41));
        assert_eq!(o.io_out_to(0x81), Some(0x5A));
        assert_eq!(o.io_out_to(0x99), None);
        assert_eq!(o.last_io_out(), Some((0x81, 0x5A)));
    }

    #[test]
    fn sanitize_cpuid_passes_other_leaves_through() {
        let raw = CpuidRegs {
            eax: 0x10,
            ebx: 0x6874_7541, // "Auth" (AMD vendor word)
            ecx: 0x444D_4163,
            edx: 0x6974_6E65,
        };
        // Leaf 0 (vendor) and a normal feature leaf are untouched.
        assert_eq!(sanitize_cpuid(0, raw), raw);
        assert_eq!(sanitize_cpuid(7, raw), raw);
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

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

/// The port the boot guest writes its `VMMCALL` hypercall result to. A capture
/// here proves the guest's paravirt hypercall reached enlil and its result
/// reached the guest.
pub const GUEST_VMMCALL_PORT: u8 = 0x89;

/// The hypercall number the boot guest passes in `AX` to `VMMCALL` (0 — a
/// "get version" style call enlil answers with [`GUEST_VMMCALL_RESULT`]).
pub const GUEST_VMMCALL_NUMBER: u16 = 0;

/// The byte enlil returns (in guest `RAX`) for the guest's
/// [`GUEST_VMMCALL_NUMBER`] hypercall; the guest `OUT`s its low byte.
pub const GUEST_VMMCALL_RESULT: u8 = 0x2A;

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

/// The port the *resume* event-injection guest `OUT`s from its interrupted
/// code stream after the injected interrupt's handler `IRET`s back to it.
///
/// A capture here proves the full interrupt round-trip: `VMRUN` delivered the
/// event, the handler ran and returned via `IRET`, and the CPU resumed the
/// instruction stream that was interrupted (which then `OUT`s). This is what a
/// real guest OS interrupt does — deliver, handle, return, continue.
pub const GUEST_EVENT_RESUME_PORT: u8 = 0x8A;

/// The byte the resume guest's *interrupted* code `OUT`s after the handler
/// `IRET`s.
///
/// The handler leaves it in `AL` (real-mode `IRET` restores `IP`/`CS`/`FLAGS`
/// but not `AX`), so a match proves both the `IRET` resume and that register
/// state set in the handler survived the return.
pub const GUEST_EVENT_RESUME_SENTINEL: u8 = 0x6B;

/// Guest RAM offset (a 16-bit real-mode offset) the resume guest's handler
/// stores [`GUEST_EVENT_WORK_SENTINEL`] to.
///
/// Placed past the IVT and handler, clear of the injected-interrupt stack frame
/// near `SP=0`.
pub const GUEST_EVENT_WORK_OFF: u16 = 0x0300;

/// The byte the resume guest's handler stores into guest RAM at
/// [`GUEST_EVENT_WORK_OFF`].
///
/// After the run enlil reads it back through the NPT (out of the guest's
/// isolated system-physical window), so a match proves the injected interrupt's
/// handler performed real work in guest memory that the hypervisor can observe —
/// the basis for interrupt-driven device backends.
pub const GUEST_EVENT_WORK_SENTINEL: u8 = 0x4D;

/// The exception vector the exception-intercept guest deliberately raises: `#UD`
/// (invalid opcode, vector 6). enlil traps it and re-delivers it to the guest's
/// own IVT handler (exception virtualization).
pub const GUEST_UD_VECTOR: u8 = 6;

/// Guest RAM offset (a 16-bit real-mode offset) of the `#UD` handler — above the
/// real-mode IVT (`[0, 0x400)`) and below the [`GUEST_GS_BASE`] sentinel.
pub const GUEST_UD_HANDLER_OFF: u16 = 0x0500;

/// The port the exception-intercept guest's `#UD` handler `OUT`s its sentinel
/// to.
pub const GUEST_UD_PORT: u8 = 0x8B;

/// The byte the `#UD` handler `OUT`s. Present in the run's I/O record iff enlil
/// trapped the guest's `#UD` and re-injected it to the guest's own handler.
pub const GUEST_UD_SENTINEL: u8 = 0x4D;

/// The interrupt vector the interrupt-round-trip guest is injected with.
///
/// Its handler runs then `IRET`s so the guest resumes past the injection point —
/// the full inject → handle → `IRET` → resume cycle a virtual timer tick needs.
pub const GUEST_IRQ_VECTOR: u8 = 0x21;

/// Guest RAM offset of the interrupt-round-trip guest's handler (above the code
/// at GPA 0, over an unused real-mode IVT slot like the event-injection guest).
pub const GUEST_IRQ_HANDLER_OFF: u16 = 0x0200;

/// The port the interrupt-round-trip guest's handler `OUT`s to (proves the
/// injected interrupt vectored to the handler).
pub const GUEST_IRQ_HANDLER_PORT: u8 = 0x8C;

/// The byte the interrupt-round-trip handler `OUT`s before it `IRET`s.
pub const GUEST_IRQ_HANDLER_SENTINEL: u8 = 0x3B;

/// The port the interrupt-round-trip guest `OUT`s *after* the handler `IRET`s
/// (proves the guest resumed execution past the injection point).
pub const GUEST_IRQ_RESUME_PORT: u8 = 0x8D;

/// The byte the interrupt-round-trip guest `OUT`s after the handler returns —
/// present only if the handler `IRET`'d and the guest continued running.
pub const GUEST_IRQ_RESUME_SENTINEL: u8 = 0x63;

/// The guest-physical address the write-protection guest stores to.
///
/// A 16-bit real-mode offset within the guest's `[0, 2 MiB)` RAM. enlil marks
/// the covering NPT leaf read-only, so the store takes a present+write nested
/// page fault the run loop observes and grants.
pub const GUEST_WP_GPA: u16 = 0x1800;

/// The byte the write-protection guest stores then reads back — a matching `OUT`
/// proves the store completed after enlil cleared the write-protection.
pub const GUEST_WP_VALUE: u8 = 0x71;

/// The port the write-protection guest `OUT`s its read-back byte to.
pub const GUEST_WP_PORT: u8 = 0x8E;

/// The port the 64-bit long-mode guest `OUT`s its sentinel to. A capture here
/// proves a guest ran in long mode (paging on, `CR3` walked through the NPT)
/// under enlil.
pub const GUEST_LM_PORT: u8 = 0x88;

/// The byte the long-mode guest `OUT`s — present in the run's I/O record iff the
/// guest entered long mode and ran to its `OUT`.
pub const GUEST_LM_SENTINEL: u8 = 0x6D;

/// Guest-physical address of the long-mode guest's own page-table root (loaded
/// into guest `CR3`), as a RAM offset. Past the code + stack, inside the
/// `[0, 2 MiB)` window so the NPT resolves the page-table walk.
#[cfg(target_os = "uefi")]
const GUEST_LM_PT_GPA: usize = 0x0001_0000;

/// The long-mode guest's initial `RSP` — below the page tables, above the code.
#[cfg(target_os = "uefi")]
const GUEST_LM_STACK: u64 = 0x0000_8000;

/// The port the long-mode *event-injection* guest `OUT`s from its interrupted
/// code after the injected interrupt's 64-bit handler `IRETQ`s back to it.
///
/// A capture here proves an injected interrupt was delivered through a real
/// 64-bit `IDT`, vectored to a handler that ran and returned via `IRETQ`, and
/// the interrupted long-mode stream resumed — the full interrupt round-trip in
/// the mode a real x86-64 OS handles interrupts in.
pub const GUEST_LM_EVENT_PORT: u8 = 0x8B;

/// The byte the long-mode event-injection guest's interrupted code `OUT`s after
/// the handler `IRETQ`s (the handler leaves it in `AL`).
pub const GUEST_LM_EVENT_SENTINEL: u8 = 0x3E;

/// The port the long-mode *virtual-interrupt* guest `OUT`s **before** `STI`.
///
/// A virtual interrupt is already pending but interrupts are masked (`IF=0`), so
/// a capture of this *ahead of* the handler port proves the pending interrupt
/// was correctly held off — interrupt masking works.
pub const GUEST_LM_VINTR_BEFORE_PORT: u8 = 0x8C;

/// The byte the virtual-interrupt guest `OUT`s before `STI` (masked window).
pub const GUEST_LM_VINTR_BEFORE_SENTINEL: u8 = 0x11;

/// The port the virtual-interrupt guest's handler `OUT`s once the pending
/// interrupt is delivered (after `STI` unmasks it).
pub const GUEST_LM_VINTR_HANDLER_PORT: u8 = 0x8D;

/// The byte the virtual-interrupt guest's handler `OUT`s.
pub const GUEST_LM_VINTR_HANDLER_SENTINEL: u8 = 0x22;

/// The port the virtual-interrupt guest `OUT`s after the handler `IRETQ`s and
/// the interrupted code resumes.
pub const GUEST_LM_VINTR_RESUME_PORT: u8 = 0x8E;

/// The byte the virtual-interrupt guest `OUT`s after resuming.
pub const GUEST_LM_VINTR_RESUME_SENTINEL: u8 = 0x33;

/// The port the long-mode *preemption* guest's handler `OUT`s.
///
/// The guest spins in an unconditional `jmp $` that never exits on its own, so
/// a capture here can only come from the pending virtual interrupt breaking the
/// loop — proof enlil forcibly preempted a non-cooperative running guest.
pub const GUEST_LM_PREEMPT_PORT: u8 = 0x8F;

/// The byte the preemption guest's handler `OUT`s before halting.
pub const GUEST_LM_PREEMPT_SENTINEL: u8 = 0x44;

/// Guest-virtual/-physical offset of the long-mode event guest's handler (past
/// the entry code, within the identity-mapped `[0, 2 MiB)` window).
#[cfg(any(target_os = "uefi", test))]
const GUEST_LM_HANDLER_OFF: usize = 0x0000_0100;

/// Guest-physical base of the long-mode event guest's `GDT` (the `GDTR` base);
/// holds a null, a 64-bit code (selector `0x08`), and a data (`0x10`) descriptor.
#[cfg(any(target_os = "uefi", test))]
const GUEST_LM_GDT_GPA: usize = 0x0000_1000;

/// Guest-physical base of the long-mode event guest's `IDT` (the `IDTR` base);
/// one 64-bit interrupt gate for [`GUEST_EVENT_VECTOR`] points at the handler.
#[cfg(any(target_os = "uefi", test))]
const GUEST_LM_IDT_GPA: usize = 0x0000_2000;

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
    /// Nested page faults demand-mapped (not-present faults).
    pub npf_exits: u32,
    /// Nested write-protection faults observed then granted (present + write —
    /// the dirty-tracking / copy-on-write signal).
    pub npf_write_faults: u32,
    /// Intercepted `VMMCALL` hypercalls serviced.
    pub vmmcall_exits: u32,
    /// Intercepted guest exceptions re-delivered to the guest's own IDT/IVT.
    pub exception_exits: u32,
    /// Vector of the last intercepted guest exception, if any.
    pub last_exception_vector: Option<u8>,
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
            vmmcall_exits: 0,
            exception_exits: 0,
            last_exception_vector: None,
            npf_write_faults: 0,
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

/// Stamp the *resume* event-injection guest's program into `ram` (its isolated
/// `[0, 2 MiB)` window): the interrupted entry code, the real-mode IVT slot for
/// [`GUEST_EVENT_VECTOR`], and a handler that does work then `IRET`s.
///
/// Layout (all real-mode, segment bases 0):
/// - **GPA 0** — the interrupted stream the handler `IRET`s back to:
///   `out GUEST_EVENT_RESUME_PORT, al ; hlt`. It does not run first: `VMRUN`
///   injects the interrupt before the entry executes, so this runs only *after*
///   the handler returns, with `AL` carrying [`GUEST_EVENT_RESUME_SENTINEL`].
/// - **IVT[[`GUEST_EVENT_VECTOR`]]** (offset `4 * vector`) — segment 0, offset
///   [`GUEST_EVENT_HANDLER_OFF`].
/// - **[`GUEST_EVENT_HANDLER_OFF`]** — the handler:
///   `mov al, GUEST_EVENT_WORK_SENTINEL ; mov [GUEST_EVENT_WORK_OFF], al ;
///   mov al, GUEST_EVENT_RESUME_SENTINEL ; iret`. It stores the work sentinel
///   into guest RAM (visible to enlil through the NPT) then returns to GPA 0.
///
/// Pure and host-testable; the firmware builder allocates the RAM and calls it.
#[cfg(any(target_os = "uefi", test))]
const fn write_event_resume_program(ram: &mut [u8]) {
    // Interrupted entry at GPA 0 (runs only after the handler IRETs).
    ram[0] = 0xE6; // OUT imm8, AL
    ram[1] = GUEST_EVENT_RESUME_PORT;
    ram[2] = 0xF4; // HLT
    // Real-mode IVT slot: offset (u16 LE) then segment (u16 LE).
    let ivt = (GUEST_EVENT_VECTOR as usize) * 4;
    let off = GUEST_EVENT_HANDLER_OFF.to_le_bytes();
    ram[ivt] = off[0];
    ram[ivt + 1] = off[1];
    ram[ivt + 2] = 0x00; // segment low
    ram[ivt + 3] = 0x00; // segment high
    // Handler: store the work sentinel into guest RAM, load the resume sentinel
    // into AL, then IRET back to the interrupted GPA 0.
    let h = GUEST_EVENT_HANDLER_OFF as usize;
    let work = GUEST_EVENT_WORK_OFF.to_le_bytes();
    ram[h] = 0xB0; // MOV AL, imm8
    ram[h + 1] = GUEST_EVENT_WORK_SENTINEL;
    ram[h + 2] = 0xA2; // MOV moffs16, AL  (store AL → DS:GUEST_EVENT_WORK_OFF)
    ram[h + 3] = work[0];
    ram[h + 4] = work[1];
    ram[h + 5] = 0xB0; // MOV AL, imm8
    ram[h + 6] = GUEST_EVENT_RESUME_SENTINEL;
    ram[h + 7] = 0xCF; // IRET
}

/// Stamp the **64-bit long-mode** event-injection guest's program into `ram`:
/// the interrupted entry code, a real 64-bit `GDT` and `IDT`, and a handler
/// that `IRETQ`s.
///
/// This is the long-mode analogue of [`write_event_resume_program`] — the mode
/// a real x86-64 OS handles interrupts in. Layout (all within the identity-
/// mapped `[0, 2 MiB)` window; segment bases 0):
/// - **GVA 0** — the interrupted stream the handler `IRETQ`s back to:
///   `out GUEST_LM_EVENT_PORT, al ; hlt`. It runs only *after* the handler
///   returns (VMRUN injects before the entry executes), with `AL` carrying
///   [`GUEST_LM_EVENT_SENTINEL`].
/// - **[`GUEST_LM_GDT_GPA`]** — a `GDT` of `[null, 64-bit code (selector
///   `0x08`, the VMCB `CS`), data (`0x10`)]`; the injected interrupt reloads
///   `CS` from the code descriptor.
/// - **[`GUEST_LM_IDT_GPA`]** — a 64-bit interrupt gate at
///   `IDT[GUEST_EVENT_VECTOR]` → (selector `0x08`, offset
///   [`GUEST_LM_HANDLER_OFF`]).
/// - **[`GUEST_LM_HANDLER_OFF`]** — the handler: `mov al, GUEST_LM_EVENT_SENTINEL
///   ; iretq` (`REX.W` `CF`, so it pops the 64-bit `SS:RSP:RFLAGS:CS:RIP` frame).
///
/// Pure and host-testable; the firmware builder allocates the RAM, the guest
/// page tables, and the NPT, and programs the VMCB `GDTR`/`IDTR`.
#[cfg(any(target_os = "uefi", test))]
fn write_long_mode_event_program(ram: &mut [u8]) {
    // Interrupted entry at GVA 0 (runs only after the handler IRETQs).
    ram[0] = 0xE6; // OUT imm8, AL
    ram[1] = GUEST_LM_EVENT_PORT;
    ram[2] = 0xF4; // HLT

    // 64-bit handler: mov al, SENTINEL; iretq.
    let h = GUEST_LM_HANDLER_OFF;
    ram[h] = 0xB0; // MOV AL, imm8
    ram[h + 1] = GUEST_LM_EVENT_SENTINEL;
    ram[h + 2] = 0x48; // REX.W
    ram[h + 3] = 0xCF; // IRETQ (pops the 64-bit interrupt frame)

    write_long_mode_gdt_and_gate(ram);
}

/// Stamp the shared long-mode `GDT` and `IDT` interrupt gate into `ram` — the
/// descriptor tables both long-mode interrupt guests use.
///
/// - `GDT` at [`GUEST_LM_GDT_GPA`]: `[0]` null, `[1]` a 64-bit code descriptor
///   (`G=1,L=1`, P/DPL0/S exec-read-accessed) at selector `0x08`, `[2]` a data
///   descriptor (`G=1,B=1`, P/DPL0/S read-write-accessed) at `0x10` — matching
///   the VMCB's long-mode `CS`/`DS` selectors.
/// - `IDT` at [`GUEST_LM_IDT_GPA`]: a 64-bit interrupt gate at
///   `IDT[GUEST_EVENT_VECTOR]` → (selector `0x08`, offset
///   [`GUEST_LM_HANDLER_OFF`]); present, DPL 0, type `0xE`.
#[cfg(any(target_os = "uefi", test))]
fn write_long_mode_gdt_and_gate(ram: &mut [u8]) {
    let g = GUEST_LM_GDT_GPA;
    ram[g + 8..g + 16].copy_from_slice(&0x00AF_9B00_0000_FFFFu64.to_le_bytes());
    ram[g + 16..g + 24].copy_from_slice(&0x00CF_9300_0000_FFFFu64.to_le_bytes());

    let gate = GUEST_LM_IDT_GPA + (GUEST_EVENT_VECTOR as usize) * 16;
    let ob = (GUEST_LM_HANDLER_OFF as u64).to_le_bytes();
    ram[gate] = ob[0]; // offset 7:0
    ram[gate + 1] = ob[1]; // offset 15:8
    ram[gate + 2] = 0x08; // segment selector 7:0 (code)
    ram[gate + 3] = 0x00; // segment selector 15:8
    ram[gate + 4] = 0x00; // IST = 0 (use the current RSP)
    ram[gate + 5] = 0x8E; // P=1, DPL=0, type=0xE (64-bit interrupt gate)
    ram[gate + 6] = ob[2]; // offset 23:16
    ram[gate + 7] = ob[3]; // offset 31:24
    ram[gate + 8] = ob[4]; // offset 39:32
    ram[gate + 9] = ob[5]; // offset 47:40
    ram[gate + 10] = ob[6]; // offset 55:48
    ram[gate + 11] = ob[7]; // offset 63:56
}

/// Stamp the long-mode **virtual-interrupt (masking)** guest's program into
/// `ram`: an interrupted stream that runs with interrupts masked, `STI`s, and
/// takes a *pending* virtual interrupt through the shared long-mode `IDT`.
///
/// The guest starts with `IF=0` (real-mode-style reset `RFLAGS`), so the
/// virtual interrupt posted in the VMCB (`encode_vintr`) is held off. Flow at
/// GVA 0: `mov al, BEFORE ; out BEFORE_PORT, al` (masked window — this `OUT`
/// lands *before* the handler runs), then `sti ; nop` (the `STI` shadow lets the
/// `nop` retire, then the pending interrupt is recognized → the handler `OUT`s
/// its sentinel and `IRETQ`s), then `mov al, RESUME ; out RESUME_PORT, al ; hlt`.
/// The resulting `OUT` order — BEFORE, then HANDLER, then RESUME — proves the
/// interrupt stayed masked until `STI` (interrupt masking), was then delivered,
/// and the interrupted code resumed. Reuses [`write_long_mode_gdt_and_gate`]
/// (its handler `OUT`s [`GUEST_LM_VINTR_HANDLER_SENTINEL`] rather than the
/// event guest's sentinel).
///
/// Pure and host-testable; the firmware builder allocates the RAM, guest page
/// tables, and NPT, and posts the virtual interrupt via `INT_CONTROL`.
#[cfg(any(target_os = "uefi", test))]
fn write_long_mode_vintr_program(ram: &mut [u8]) {
    // Interrupted entry at GVA 0, starting with interrupts masked (IF=0).
    ram[0] = 0xB0; // MOV AL, imm8
    ram[1] = GUEST_LM_VINTR_BEFORE_SENTINEL;
    ram[2] = 0xE6; // OUT imm8, AL  (masked window — lands before the handler)
    ram[3] = GUEST_LM_VINTR_BEFORE_PORT;
    ram[4] = 0xFB; // STI  (enable interrupts; a 1-instruction shadow follows)
    ram[5] = 0x90; // NOP  (the shadow instruction; the interrupt fires after it)
    ram[6] = 0xB0; // MOV AL, imm8  (reload AL — the handler clobbered it)
    ram[7] = GUEST_LM_VINTR_RESUME_SENTINEL;
    ram[8] = 0xE6; // OUT imm8, AL
    ram[9] = GUEST_LM_VINTR_RESUME_PORT;
    ram[10] = 0xF4; // HLT

    // 64-bit handler: mov al, HANDLER; out HANDLER_PORT, al; iretq.
    let h = GUEST_LM_HANDLER_OFF;
    ram[h] = 0xB0; // MOV AL, imm8
    ram[h + 1] = GUEST_LM_VINTR_HANDLER_SENTINEL;
    ram[h + 2] = 0xE6; // OUT imm8, AL
    ram[h + 3] = GUEST_LM_VINTR_HANDLER_PORT;
    ram[h + 4] = 0x48; // REX.W
    ram[h + 5] = 0xCF; // IRETQ

    write_long_mode_gdt_and_gate(ram);
}

/// Stamp the long-mode **preemption** guest's program into `ram`: a guest that
/// `STI`s and then spins forever, only stoppable by a preempting interrupt.
///
/// Entry at GVA 0: `sti ; jmp $` — after the `STI` shadow the guest loops on an
/// unconditional short jump-to-self that never exits (no `OUT`, no `HLT`). The
/// pending virtual interrupt (posted in the VMCB) is the only thing that can
/// break the loop: it vectors through the shared long-mode `IDT` to a handler
/// that `OUT`s [`GUEST_LM_PREEMPT_SENTINEL`] and `HLT`s. A captured sentinel
/// therefore proves enlil forcibly preempted a non-cooperative running guest —
/// the essence of time-slicing. Reuses [`write_long_mode_gdt_and_gate`].
///
/// Pure and host-testable; the firmware builder posts the virtual interrupt.
#[cfg(any(target_os = "uefi", test))]
fn write_long_mode_preempt_program(ram: &mut [u8]) {
    // Entry at GVA 0: sti; jmp $ (spin forever with interrupts enabled).
    ram[0] = 0xFB; // STI
    ram[1] = 0xEB; // JMP rel8
    ram[2] = 0xFE; // -2 → jump to the JMP itself (spin)

    // 64-bit handler: mov al, SENTINEL; out PORT, al; hlt (stops the guest).
    let h = GUEST_LM_HANDLER_OFF;
    ram[h] = 0xB0; // MOV AL, imm8
    ram[h + 1] = GUEST_LM_PREEMPT_SENTINEL;
    ram[h + 2] = 0xE6; // OUT imm8, AL
    ram[h + 3] = GUEST_LM_PREEMPT_PORT;
    ram[h + 4] = 0xF4; // HLT

    write_long_mode_gdt_and_gate(ram);
}

#[cfg(target_os = "uefi")]
pub use hw::{
    enable_svm, program_boot_vmcb, program_event_inj_resume_vmcb, program_event_inj_vmcb,
    program_host_save_area, program_irq_resume_vmcb, program_long_mode_event_inj_vmcb,
    program_long_mode_preempt_vmcb, program_long_mode_vintr_vmcb, program_long_mode_vmcb,
    program_ud_exception_vmcb, program_wp_npf_vmcb, run_boot_guest_loop,
};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{GUEST_IO_PORT, GUEST_MSR_NUMBER, GUEST_NPF_GPA};
    use super::{
        HSAVE_PAGE_SIZE, MSR_EFER, MSR_VM_CR, MSR_VM_HSAVE_PA, SvmStatus, efer_with_svme,
        is_svm_enabled, is_valid_hsave_pa, svm_status, vm_cr_clear_svmdis,
    };
    use alloc::alloc::{Layout, alloc_zeroed};
    use enlil_hal::npt::{build_identity_npt_2mib, build_npt_2mib, set_npt_2mib_leaf_writable};
    use enlil_hal::region::{IoPermissionsMap, MsrPermissionsMap, Vmcb};
    use enlil_hal::svm::{
        LongModeGuestSetup, MinimalGuestSetup, VmcbSegment, control, enable_io_intercept,
        enable_msr_intercept, encode_event_inj, encode_vintr, event_type, intercept_vmmcall,
        program_long_mode_hlt_guest, program_minimal_hlt_guest, save, set_event_inj,
        set_exception_intercept, set_int_control, write_segment,
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

    /// Obtain a zeroed, 2 MiB-aligned [`GUEST_RAM_BYTES`] window for one guest,
    /// returning a pointer to it (its system-physical base, since the kernel runs
    /// identity-mapped).
    ///
    /// Prefers a disjoint slice of the **planned** guest-RAM region carved by
    /// `plan_hypervisor_regions` — memory reserved for guests and provably clear
    /// of the kernel heap, so a guest's nested mapping cannot reach the
    /// hypervisor's own allocator (LOCKED PRINCIPLE 5). Falls back to the kernel
    /// heap when no plan was published (no memory map in the handoff, or the plan
    /// did not fit), so bring-up still works on a machine the planner cannot
    /// satisfy.
    ///
    /// The window is leaked either way: it must outlive `VMRUN`.
    fn alloc_guest_ram() -> Option<*mut u8> {
        if let Some(spa) = crate::kernel::guest_ram::take(GUEST_RAM_BYTES as u64, 0x20_0000) {
            let ptr: *mut u8 = core::ptr::with_exposed_provenance_mut(usize::try_from(spa).ok()?);
            // Carved RAM is raw physical memory the firmware called usable; the
            // heap's zeroing does not apply, so zero it here.
            // SAFETY: the span is a disjoint slice of the planned guest region,
            // identity-mapped by the kernel's own page tables, handed to exactly
            // one caller and never reused.
            unsafe { core::ptr::write_bytes(ptr, 0, GUEST_RAM_BYTES) };
            return Some(ptr);
        }
        // SAFETY: GuestRam has a nonzero size; alloc_zeroed yields a zeroed,
        // 2 MiB-aligned GuestRam-sized block or null.
        let raw = unsafe { alloc_zeroed(Layout::new::<GuestRam>()) };
        (!raw.is_null()).then_some(raw)
    }

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
        // Paravirt hypercall: VMMCALL with a hypercall number in AX; enlil
        // answers by writing the result into guest RAX, which the guest OUTs.
        let hc = super::GUEST_VMMCALL_NUMBER.to_le_bytes();
        bytes[63] = 0xB8; // mov ax, GUEST_VMMCALL_NUMBER
        bytes[64] = hc[0];
        bytes[65] = hc[1];
        bytes[66] = 0x0F; // VMMCALL (0F 01 D9)
        bytes[67] = 0x01;
        bytes[68] = 0xD9;
        bytes[69] = 0xE6; // OUT imm8, AL     (= enlil's hypercall result)
        bytes[70] = super::GUEST_VMMCALL_PORT;
        // Read a byte through GS (base loaded from the VMCB by VMLOAD only) and
        // OUT it — proving the run shell's VMSAVE/VMLOAD extended-state swap.
        bytes[71] = 0x65; // GS segment override
        bytes[72] = 0xA0; // MOV AL, moffs16
        bytes[73] = 0x00; // offset 0x0000 (16-bit) → GS:[0]
        bytes[74] = 0x00;
        bytes[75] = 0xE6; // OUT imm8, AL
        bytes[76] = super::GUEST_GS_PORT;
        bytes[77] = 0xF4; // HLT
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
        //   mov ax, 0        B8 00 00        (hypercall number)
        //   vmmcall          0F 01 D9        (intercepted → enlil sets RAX)
        //   out 0x89, al     E6 89           (IOIO → the hypercall result)
        //   mov al, gs:[0]   65 A0 00 00     (GS.base loaded by VMLOAD only)
        //   out 0x86, al     E6 86           (IOIO → the GS sentinel)
        //   hlt              F4              (clean stop)
        // Planned guest region when available, kernel heap otherwise.
        let ram_raw = alloc_guest_ram()?;
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

        // Arm the VMMCALL intercept so the guest's hypercall traps to enlil.
        intercept_vmmcall(vmcb.as_bytes_mut());

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
        // Planned guest region when available, kernel heap otherwise.
        let ram_raw = alloc_guest_ram()?;
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

    /// Build a `VMRUN`-ready VMCB that proves an injected interrupt's handler
    /// **does work and returns via `IRET`** to resume the interrupted guest,
    /// returning `(vmcb_pa, guest_spa)`.
    ///
    /// Where [`program_event_inj_vmcb`] proves an injected interrupt reaches a
    /// handler that then `HLT`s (the handler never returns), this proves the
    /// full round-trip a real guest OS interrupt performs: `VMRUN` injects
    /// [`GUEST_EVENT_VECTOR`](super::GUEST_EVENT_VECTOR) before the first
    /// instruction, the real-mode IVT vectors it to a handler that stores
    /// [`GUEST_EVENT_WORK_SENTINEL`](super::GUEST_EVENT_WORK_SENTINEL) into guest
    /// RAM and `IRET`s, and the CPU resumes the interrupted stream at GPA 0
    /// (`out GUEST_EVENT_RESUME_PORT, al`) with `AL` still holding the
    /// [`GUEST_EVENT_RESUME_SENTINEL`](super::GUEST_EVENT_RESUME_SENTINEL) the
    /// handler set. Two independent post-run proofs follow: the resume-port
    /// `OUT` in the run record (interrupt delivered → handled → `IRET` resumed →
    /// interrupted code ran), and the work sentinel the caller reads back at
    /// `guest_spa + GUEST_EVENT_WORK_OFF` through the NPT window (the handler
    /// mutated guest memory the hypervisor can observe).
    ///
    /// Assembled entirely through `enlil-hal` (RAM + IVT + handler via
    /// [`write_event_resume_program`](super::write_event_resume_program), an NPT
    /// mapping the guest window, a [`Vmcb`] with the I/O intercept, a real-mode
    /// IDTR, and `EVENTINJ` armed via [`encode_event_inj`]); allocations are
    /// leaked to outlive `VMRUN`. Returns `None` on any allocation/programming
    /// failure.
    #[must_use]
    pub fn program_event_inj_resume_vmcb() -> Option<(u64, u64)> {
        use super::GUEST_EVENT_VECTOR;

        // Allocate + populate the guest's isolated RAM.
        // SAFETY: GuestRam is a nonzero, 2 MiB-aligned block; alloc_zeroed
        // yields it zeroed or null.
        let ram_raw = unsafe { alloc_zeroed(Layout::new::<GuestRam>()) };
        if ram_raw.is_null() {
            return None;
        }
        let guest_spa = ram_raw as u64;
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes (leaked below).
        super::write_event_resume_program(unsafe {
            core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES)
        });

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

        // Program a VMCB entering the interrupted code at GPA 0 under that NPT.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            entry_ip: 0,
            code_base: 0,
            stack_pointer: 0,
        };
        program_minimal_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;
        // Install the standard real-mode IVT (base 0, limit 0x3FF) so the
        // injected vector's slot is covered (program_minimal_hlt_guest zeroes
        // the VMCB, leaving IDTR limit 0). The stack frame the injection pushes
        // (FLAGS/CS/IP near SP=0) and the IRET that pops it both need it.
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

        // Arm port-I/O interception so the resumed code's OUT takes an IOIO exit.
        let iopm = IoPermissionsMap::intercept_all().ok()?;
        enable_io_intercept(vmcb.as_bytes_mut(), iopm.base_addr());
        core::mem::forget(iopm);

        // Arm EVENTINJ: VMRUN injects GUEST_EVENT_VECTOR before the first guest
        // instruction; the run loop clears it after entry so it fires once.
        let inj = encode_event_inj(GUEST_EVENT_VECTOR, event_type::EXTERNAL_INTERRUPT, None);
        set_event_inj(vmcb.as_bytes_mut(), inj);

        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // must outlive VMRUN
        Some((vmcb_pa, guest_spa))
    }

    /// Build a `VMRUN`-ready VMCB that proves **guest exception interception**,
    /// returning `(vmcb_pa, handler_gpa)`.
    ///
    /// The guest's first instruction is `UD2` (`0F 0B`), which raises `#UD`.
    /// enlil arms the `#UD` exception intercept ([`set_exception_intercept`]) so
    /// the fault takes a `#VMEXIT` ([`RunLoopExit::Exception`]) instead of
    /// vectoring directly; the run loop then **re-delivers** it to the guest's
    /// own real-mode IVT (`EVENTINJ`, resumed without advancing RIP). The IVT
    /// slot for `#UD` (`4 * 6`) points at a handler that `OUT`s
    /// [`GUEST_UD_SENTINEL`](super::GUEST_UD_SENTINEL) and `HLT`s. A matching
    /// capture proves enlil trapped the guest's own fault and handed it back to
    /// the guest — the mechanism for observing/emulating guest exceptions while
    /// the guest still handles them (ROADMAP 6.2).
    ///
    /// Assembled through `enlil-hal` like the other proof guests (isolated RAM +
    /// IVT + handler, an NPT mapping the low GiB, a [`Vmcb`] with the real-mode
    /// IVT limit, the I/O intercept, and the `#UD` intercept armed); allocations
    /// are leaked to outlive `VMRUN`. Returns `None` on any allocation/
    /// programming failure.
    #[must_use]
    pub fn program_ud_exception_vmcb() -> Option<(u64, u64)> {
        use super::{GUEST_UD_HANDLER_OFF, GUEST_UD_PORT, GUEST_UD_SENTINEL, GUEST_UD_VECTOR};

        let ram_raw = alloc_guest_ram()?;
        let guest_spa = ram_raw as u64;
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes (leaked below).
        let ram = unsafe { core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES) };
        // Entry code at GPA 0: UD2 (raises #UD) then a fallback HLT (unreached if
        // the intercept + re-injection work, since control goes to the handler).
        ram[0] = 0x0F; // UD2
        ram[1] = 0x0B;
        ram[2] = 0xF4; // HLT (fallback)
        // Real-mode IVT slot for #UD (vector 6): offset (u16 LE) then segment
        // (u16 LE), pointing at segment 0, offset GUEST_UD_HANDLER_OFF.
        let ivt = (GUEST_UD_VECTOR as usize) * 4;
        let off = GUEST_UD_HANDLER_OFF.to_le_bytes();
        ram[ivt] = off[0];
        ram[ivt + 1] = off[1];
        ram[ivt + 2] = 0x00; // segment low
        ram[ivt + 3] = 0x00; // segment high
        // Handler: mov al, SENTINEL; out PORT, al; hlt.
        let h = GUEST_UD_HANDLER_OFF as usize;
        ram[h] = 0xB0; // MOV AL, imm8
        ram[h + 1] = GUEST_UD_SENTINEL;
        ram[h + 2] = 0xE6; // OUT imm8, AL
        ram[h + 3] = GUEST_UD_PORT;
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

        // Program a VMCB entering the UD2 at GPA 0 under that NPT.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            entry_ip: 0,
            code_base: 0,
            stack_pointer: 0,
        };
        program_minimal_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;
        // Install the standard real-mode IVT (base 0, limit 0x3FF) so the
        // re-injected #UD's IVT slot is covered (program_minimal_hlt_guest leaves
        // IDTR limit 0).
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

        // Arm the #UD exception intercept so the guest's UD2 takes a #VMEXIT the
        // run loop routes as RunLoopExit::Exception (then re-injects to the guest).
        set_exception_intercept(vmcb.as_bytes_mut(), GUEST_UD_VECTOR);

        let handler_gpa = u64::from(GUEST_UD_HANDLER_OFF);
        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // must outlive VMRUN
        Some((vmcb_pa, handler_gpa))
    }

    /// Build a `VMRUN`-ready VMCB that proves a full **interrupt round-trip**,
    /// returning `(vmcb_pa, handler_gpa)`.
    ///
    /// enlil arms `EVENTINJ` so `VMRUN` injects
    /// [`GUEST_IRQ_VECTOR`](super::GUEST_IRQ_VECTOR) before the guest's first
    /// instruction. The guest's real-mode IVT vectors it to a handler that
    /// `OUT`s [`GUEST_IRQ_HANDLER_SENTINEL`](super::GUEST_IRQ_HANDLER_SENTINEL)
    /// and then `IRET`s. `IRET` returns to the injection point (GPA 0), where the
    /// guest's continuation `OUT`s
    /// [`GUEST_IRQ_RESUME_SENTINEL`](super::GUEST_IRQ_RESUME_SENTINEL) and
    /// `HLT`s. Capturing **both** sentinels proves the whole inject → handle →
    /// `IRET` → resume cycle — the mechanism a virtual timer tick or device
    /// interrupt uses to preempt a guest and let it keep running (ROADMAP 6.2),
    /// a step beyond the handler-only event-injection proof.
    ///
    /// The injected interrupt pushes `FLAGS:CS:IP` (IP = 0) to the guest stack,
    /// so `IRET` resumes at GPA 0 — which is why the continuation lives there and
    /// the handler sits above the IVT. Assembled through `enlil-hal` like the
    /// other proof guests; allocations are leaked to outlive `VMRUN`. Returns
    /// `None` on any allocation/programming failure.
    #[must_use]
    pub fn program_irq_resume_vmcb() -> Option<(u64, u64)> {
        use super::{
            GUEST_IRQ_HANDLER_OFF, GUEST_IRQ_HANDLER_PORT, GUEST_IRQ_HANDLER_SENTINEL,
            GUEST_IRQ_RESUME_PORT, GUEST_IRQ_RESUME_SENTINEL, GUEST_IRQ_VECTOR,
        };

        let ram_raw = alloc_guest_ram()?;
        let guest_spa = ram_raw as u64;
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes (leaked below).
        let ram = unsafe { core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES) };
        // Continuation at GPA 0 (where IRET returns): mov al, RESUME; out; hlt.
        ram[0] = 0xB0; // MOV AL, imm8
        ram[1] = GUEST_IRQ_RESUME_SENTINEL;
        ram[2] = 0xE6; // OUT imm8, AL
        ram[3] = GUEST_IRQ_RESUME_PORT;
        ram[4] = 0xF4; // HLT
        // Real-mode IVT slot for the vector → segment 0, offset GUEST_IRQ_HANDLER_OFF.
        let ivt = (GUEST_IRQ_VECTOR as usize) * 4;
        let off = GUEST_IRQ_HANDLER_OFF.to_le_bytes();
        ram[ivt] = off[0];
        ram[ivt + 1] = off[1];
        ram[ivt + 2] = 0x00; // segment low
        ram[ivt + 3] = 0x00; // segment high
        // Handler: mov al, HANDLER_SENTINEL; out PORT, al; iret.
        let h = GUEST_IRQ_HANDLER_OFF as usize;
        ram[h] = 0xB0; // MOV AL, imm8
        ram[h + 1] = GUEST_IRQ_HANDLER_SENTINEL;
        ram[h + 2] = 0xE6; // OUT imm8, AL
        ram[h + 3] = GUEST_IRQ_HANDLER_PORT;
        ram[h + 4] = 0xCF; // IRET (real-mode: pop IP, CS, FLAGS)

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

        // Program a VMCB entering the continuation at GPA 0 under that NPT.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            entry_ip: 0,
            code_base: 0,
            stack_pointer: 0,
        };
        program_minimal_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;
        // Install the real-mode IVT (base 0, limit 0x3FF) so the injected
        // vector's slot is covered (program_minimal_hlt_guest leaves limit 0).
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

        // Arm port-I/O interception so both OUTs take IOIO #VMEXITs.
        let iopm = IoPermissionsMap::intercept_all().ok()?;
        enable_io_intercept(vmcb.as_bytes_mut(), iopm.base_addr());
        core::mem::forget(iopm);

        // Arm EVENTINJ: VMRUN injects GUEST_IRQ_VECTOR before the first guest
        // instruction; the run loop clears it after entry so it fires once.
        let inj = encode_event_inj(GUEST_IRQ_VECTOR, event_type::EXTERNAL_INTERRUPT, None);
        set_event_inj(vmcb.as_bytes_mut(), inj);

        let handler_gpa = u64::from(GUEST_IRQ_HANDLER_OFF);
        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // must outlive VMRUN
        Some((vmcb_pa, handler_gpa))
    }

    /// Build a `VMRUN`-ready VMCB that proves **NPT write-protection** (the
    /// dirty-tracking / copy-on-write primitive), returning `(vmcb_pa, wp_gpa)`.
    ///
    /// After building the guest's NPT, enlil clears the `WRITABLE` bit on the
    /// leaf covering the guest's RAM ([`set_npt_2mib_leaf_writable`]), so the
    /// guest can still fetch and read but any store faults. The guest stores
    /// [`GUEST_WP_VALUE`](super::GUEST_WP_VALUE) to
    /// [`GUEST_WP_GPA`](super::GUEST_WP_GPA); that store takes a present+write
    /// nested page fault the run loop routes to `grant_npf_write` (records the
    /// write, clears the protection, resumes without advancing RIP so the store
    /// re-executes). The guest then reads the byte back and `OUT`s it — a match
    /// proves the store completed only after enlil granted write access, i.e.
    /// enlil observed the write before it landed (the signal live-migration
    /// dirty tracking and copy-on-write build on; ROADMAP 6.2 / Phase 8).
    ///
    /// Assembled through `enlil-hal` like the other proof guests; allocations
    /// are leaked to outlive `VMRUN`. Returns `None` on any allocation/
    /// programming failure.
    #[must_use]
    pub fn program_wp_npf_vmcb() -> Option<(u64, u64)> {
        use super::{GUEST_WP_GPA, GUEST_WP_PORT, GUEST_WP_VALUE};

        let ram_raw = alloc_guest_ram()?;
        let guest_spa = ram_raw as u64;
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes (leaked below).
        let ram = unsafe { core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES) };
        // Real-mode program at GPA 0 (reads DS:off with default DS base 0):
        //   mov al, VALUE       B0 71
        //   mov [WP_GPA], al    A2 lo hi   (store → present+write NPF, then granted)
        //   mov al, [WP_GPA]    A0 lo hi   (read the stored byte back)
        //   out WP_PORT, al     E6 8E      (IOIO → VALUE iff the store completed)
        //   hlt                 F4
        let off = GUEST_WP_GPA.to_le_bytes();
        ram[0] = 0xB0; // mov al, VALUE
        ram[1] = GUEST_WP_VALUE;
        ram[2] = 0xA2; // mov moffs16, al (store)
        ram[3] = off[0];
        ram[4] = off[1];
        ram[5] = 0xA0; // mov al, moffs16 (load)
        ram[6] = off[0];
        ram[7] = off[1];
        ram[8] = 0xE6; // out imm8, al
        ram[9] = GUEST_WP_PORT;
        ram[10] = 0xF4; // hlt

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
        // Write-protect the leaf covering the guest's RAM so its store faults.
        set_npt_2mib_leaf_writable(npt_buf, npt_pa, 0, false).ok()?;

        // Program a VMCB entering the code at GPA 0 under that NPT.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = MinimalGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            entry_ip: 0,
            code_base: 0,
            stack_pointer: 0,
        };
        program_minimal_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;

        // Arm port-I/O interception so the guest's OUT takes an IOIO #VMEXIT.
        let iopm = IoPermissionsMap::intercept_all().ok()?;
        enable_io_intercept(vmcb.as_bytes_mut(), iopm.base_addr());
        core::mem::forget(iopm);

        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // must outlive VMRUN
        Some((vmcb_pa, u64::from(GUEST_WP_GPA)))
    }

    /// Build a `VMRUN`-ready VMCB for a **64-bit long-mode** guest, returning
    /// `(vmcb_pa, guest_cr3)`.
    ///
    /// This is the mode a real x86-64 OS boots in. The guest runs with paging on
    /// (`CR0.PG`, `CR4.PAE`, `EFER.LMA|LME`) walking its own page tables via
    /// `CR3` — which the NPT in turn resolves to system-physical — under an
    /// `L`-bit code segment. Its code (`mov al, SENTINEL; out PORT, al; hlt`)
    /// runs at GVA 0 and the dispatch loop captures the `OUT`, so a sentinel in
    /// the run record proves a guest executed in long mode under enlil.
    ///
    /// Layout in the guest's isolated `[0, 2 MiB)` RAM: code at GPA 0, stack at
    /// [`GUEST_LM_STACK`](super::GUEST_LM_STACK), and the guest's own identity
    /// page tables at [`GUEST_LM_PT_GPA`](super::GUEST_LM_PT_GPA) (built with
    /// [`build_identity_npt_2mib`] — the x86-64 table format the guest walk and
    /// the NPT share). The NPT ([`build_npt_2mib`]) maps that GPA window onto a
    /// disjoint system-physical window (LOCKED PRINCIPLE 5). All allocations are
    /// leaked to outlive `VMRUN`. Returns `None` on any allocation/programming
    /// failure.
    #[must_use]
    pub fn program_long_mode_vmcb() -> Option<(u64, u64)> {
        use super::{GUEST_LM_PORT, GUEST_LM_PT_GPA, GUEST_LM_SENTINEL, GUEST_LM_STACK};

        // Allocate + populate the guest's isolated RAM.
        // Planned guest region when available, kernel heap otherwise.
        let ram_raw = alloc_guest_ram()?;
        let guest_spa = ram_raw as u64;
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes (leaked below).
        let ram = unsafe { core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES) };
        // 64-bit guest code at GVA/GPA 0: mov al, SENTINEL; out PORT, al; hlt
        // (these opcodes encode identically in long mode).
        ram[0] = 0xB0; // MOV AL, imm8
        ram[1] = GUEST_LM_SENTINEL;
        ram[2] = 0xE6; // OUT imm8, AL
        ram[3] = GUEST_LM_PORT;
        ram[4] = 0xF4; // HLT

        // The guest's own page tables (identity GVA→GPA over [0, 2 MiB)) at
        // GUEST_LM_PT_GPA — CR3 points here; the NPT resolves the GPAs the walk
        // reads. build_identity_npt_2mib emits the shared x86-64 table format.
        let pt = GUEST_LM_PT_GPA;
        let guest_cr3 = build_identity_npt_2mib(
            &mut ram[pt..pt + 3 * 4096],
            GUEST_LM_PT_GPA as u64,
            GUEST_RAM_BYTES as u64,
        )
        .ok()?
        .ncr3;

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

        // Program the VMCB for long mode entering the code at GVA 0.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = LongModeGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            guest_cr3,
            entry_ip: 0,
            stack_pointer: GUEST_LM_STACK,
        };
        program_long_mode_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;

        // Arm port-I/O interception so the guest's OUT takes an IOIO #VMEXIT.
        let iopm = IoPermissionsMap::intercept_all().ok()?;
        enable_io_intercept(vmcb.as_bytes_mut(), iopm.base_addr());
        core::mem::forget(iopm);

        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // must outlive VMRUN
        Some((vmcb_pa, guest_cr3))
    }

    /// Build a `VMRUN`-ready VMCB that proves an injected interrupt is delivered
    /// through a real **64-bit long-mode `IDT`** and the handler `IRETQ`s back
    /// to resume the interrupted guest, returning `(vmcb_pa, guest_spa)`.
    ///
    /// This is the long-mode counterpart of [`program_event_inj_resume_vmcb`] —
    /// interrupt handling in the mode a real x86-64 OS runs in. The guest boots
    /// in long mode (paging on, `CS.L`, its own `CR3` walked through the NPT)
    /// with a real `GDT` + `IDT`
    /// ([`write_long_mode_event_program`](super::write_long_mode_event_program)):
    /// `VMRUN` arms `EVENTINJ` so [`GUEST_EVENT_VECTOR`](super::GUEST_EVENT_VECTOR)
    /// is delivered before the first instruction, the CPU reads the 64-bit
    /// interrupt gate, reloads `CS` from the `GDT` code descriptor, pushes the
    /// `SS:RSP:RFLAGS:CS:RIP` frame, and vectors to the handler, which sets `AL`
    /// and `IRETQ`s. The interrupted code at GVA 0 then `OUT`s
    /// [`GUEST_LM_EVENT_SENTINEL`](super::GUEST_LM_EVENT_SENTINEL) — a capture of
    /// it proves the whole long-mode deliver → handle → `IRETQ` → resume path.
    ///
    /// Layout in the guest's isolated `[0, 2 MiB)` RAM: entry at GVA 0, handler
    /// at [`GUEST_LM_HANDLER_OFF`](super::GUEST_LM_HANDLER_OFF), `GDT` at
    /// [`GUEST_LM_GDT_GPA`](super::GUEST_LM_GDT_GPA), `IDT` at
    /// [`GUEST_LM_IDT_GPA`](super::GUEST_LM_IDT_GPA), stack at
    /// [`GUEST_LM_STACK`](super::GUEST_LM_STACK), and the guest's own identity
    /// page tables at [`GUEST_LM_PT_GPA`](super::GUEST_LM_PT_GPA) — all inside
    /// the NPT-mapped window. The VMCB's `GDTR`/`IDTR` point at the guest tables.
    /// All allocations are leaked to outlive `VMRUN`. Returns `None` on any
    /// allocation/programming failure.
    #[must_use]
    pub fn program_long_mode_event_inj_vmcb() -> Option<(u64, u64)> {
        use super::{
            GUEST_EVENT_VECTOR, GUEST_LM_GDT_GPA, GUEST_LM_IDT_GPA, GUEST_LM_PT_GPA, GUEST_LM_STACK,
        };

        // Allocate + populate the guest's isolated RAM.
        // SAFETY: GuestRam is nonzero, 2 MiB-aligned; alloc_zeroed yields it or null.
        let ram_raw = unsafe { alloc_zeroed(Layout::new::<GuestRam>()) };
        if ram_raw.is_null() {
            return None;
        }
        let guest_spa = ram_raw as u64;
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes (leaked below).
        super::write_long_mode_event_program(unsafe {
            core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES)
        });

        // The guest's own identity page tables (GVA→GPA over [0, 2 MiB)) at
        // GUEST_LM_PT_GPA — CR3 points here; the NPT resolves the GPAs.
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes.
        let ram = unsafe { core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES) };
        let pt = GUEST_LM_PT_GPA;
        let guest_cr3 = build_identity_npt_2mib(
            &mut ram[pt..pt + 3 * 4096],
            GUEST_LM_PT_GPA as u64,
            GUEST_RAM_BYTES as u64,
        )
        .ok()?
        .ncr3;

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

        // Program the VMCB for long mode entering the interrupted code at GVA 0.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = LongModeGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            guest_cr3,
            entry_ip: 0,
            stack_pointer: GUEST_LM_STACK,
        };
        program_long_mode_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;

        // Point GDTR/IDTR at the guest's tables so interrupt delivery can read
        // the gate and reload CS from the code descriptor (program_long_mode_
        // hlt_guest leaves them zeroed). GDT: 3 descriptors (limit 0x17); IDT:
        // 256 gates (limit 0xFFF, covering GUEST_EVENT_VECTOR's slot).
        write_segment(
            vmcb.as_bytes_mut(),
            save::GDTR,
            VmcbSegment {
                selector: 0,
                attrib: 0,
                limit: 0x0017,
                base: GUEST_LM_GDT_GPA as u64,
            },
        );
        write_segment(
            vmcb.as_bytes_mut(),
            save::IDTR,
            VmcbSegment {
                selector: 0,
                attrib: 0,
                limit: 0x0FFF,
                base: GUEST_LM_IDT_GPA as u64,
            },
        );

        // Arm port-I/O interception so the resumed code's OUT takes an IOIO exit.
        let iopm = IoPermissionsMap::intercept_all().ok()?;
        enable_io_intercept(vmcb.as_bytes_mut(), iopm.base_addr());
        core::mem::forget(iopm);

        // Arm EVENTINJ: VMRUN injects GUEST_EVENT_VECTOR before the first guest
        // instruction; the run loop clears it after entry so it fires once.
        let inj = encode_event_inj(GUEST_EVENT_VECTOR, event_type::EXTERNAL_INTERRUPT, None);
        set_event_inj(vmcb.as_bytes_mut(), inj);

        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // must outlive VMRUN
        Some((vmcb_pa, guest_spa))
    }

    /// Build a `VMRUN`-ready VMCB that proves **virtual-interrupt masking**: a
    /// 64-bit long-mode guest runs with interrupts masked, `STI`s, and only then
    /// takes a *pending* virtual interrupt through its `IDT`. Returns
    /// `(vmcb_pa, guest_spa)`.
    ///
    /// Where [`program_long_mode_event_inj_vmcb`] injects unconditionally via
    /// `EVENTINJ`, this posts a virtual interrupt via `INT_CONTROL`
    /// ([`encode_vintr`]) that the guest's own `EFLAGS.IF` gates — the mechanism
    /// that preempts a *running* guest with an asynchronous tick while honoring
    /// its interrupt masking. The guest
    /// ([`write_long_mode_vintr_program`](super::write_long_mode_vintr_program))
    /// `OUT`s a BEFORE sentinel with interrupts masked, `STI`s, takes the pending
    /// interrupt (handler `OUT`s + `IRETQ`s), then `OUT`s a RESUME sentinel — the
    /// BEFORE → HANDLER → RESUME order proving the interrupt was held off until
    /// `STI`. Same long-mode `GDT`/`IDT`/paging setup as the event guest; the
    /// virtual interrupt is posted in the VMCB instead of `EVENTINJ`. All
    /// allocations are leaked to outlive `VMRUN`. Returns `None` on failure.
    #[must_use]
    pub fn program_long_mode_vintr_vmcb() -> Option<(u64, u64)> {
        build_long_mode_vintr_guest(super::write_long_mode_vintr_program)
    }

    /// Build a `VMRUN`-ready VMCB that **preempts a spinning long-mode guest**
    /// with a pending virtual interrupt, returning `(vmcb_pa, guest_spa)`.
    ///
    /// The strongest form of the V_INTR proof: the guest
    /// ([`write_long_mode_preempt_program`](super::write_long_mode_preempt_program))
    /// `STI`s and then spins in an unconditional `jmp $` that never exits on its
    /// own. The only way it ever stops is the pending virtual interrupt breaking
    /// the loop — its handler `OUT`s [`GUEST_LM_PREEMPT_SENTINEL`] and `HLT`s. A
    /// captured sentinel plus a clean `HLT` stop proves enlil forcibly preempted
    /// a non-cooperative running guest — time-slicing (ROADMAP 6.2 toward 6.7).
    /// Same long-mode `GDT`/`IDT`/paging + posted virtual interrupt as
    /// [`program_long_mode_vintr_vmcb`].
    #[must_use]
    pub fn program_long_mode_preempt_vmcb() -> Option<(u64, u64)> {
        build_long_mode_vintr_guest(super::write_long_mode_preempt_program)
    }

    /// Assemble a long-mode guest with a **pending virtual interrupt** posted in
    /// the VMCB, running the program `write_program` stamps into its RAM.
    ///
    /// The shared body of [`program_long_mode_vintr_vmcb`] (masking proof) and
    /// [`program_long_mode_preempt_vmcb`] (spinning-guest preemption): it lays
    /// out the guest's isolated RAM (running `write_program`, which also stamps
    /// the shared long-mode `GDT`/`IDT` gate), its own identity page tables, and
    /// the NPT; programs a long-mode VMCB with `GDTR`/`IDTR` pointing at those
    /// tables, port-I/O interception, and a high-priority virtual interrupt via
    /// `INT_CONTROL` ([`encode_vintr`]) that the guest's `EFLAGS.IF` gates. All
    /// allocations are leaked to outlive `VMRUN`. Returns `None` on failure.
    fn build_long_mode_vintr_guest(write_program: fn(&mut [u8])) -> Option<(u64, u64)> {
        use super::{
            GUEST_EVENT_VECTOR, GUEST_LM_GDT_GPA, GUEST_LM_IDT_GPA, GUEST_LM_PT_GPA, GUEST_LM_STACK,
        };

        // Allocate + populate the guest's isolated RAM.
        // SAFETY: GuestRam is nonzero, 2 MiB-aligned; alloc_zeroed yields it or null.
        let ram_raw = unsafe { alloc_zeroed(Layout::new::<GuestRam>()) };
        if ram_raw.is_null() {
            return None;
        }
        let guest_spa = ram_raw as u64;
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes (leaked below).
        write_program(unsafe { core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES) });

        // The guest's own identity page tables at GUEST_LM_PT_GPA.
        // SAFETY: ram_raw owns GUEST_RAM_BYTES writable bytes.
        let ram = unsafe { core::slice::from_raw_parts_mut(ram_raw, GUEST_RAM_BYTES) };
        let pt = GUEST_LM_PT_GPA;
        let guest_cr3 = build_identity_npt_2mib(
            &mut ram[pt..pt + 3 * 4096],
            GUEST_LM_PT_GPA as u64,
            GUEST_RAM_BYTES as u64,
        )
        .ok()?
        .ncr3;

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

        // Program the VMCB for long mode entering the code at GVA 0.
        let mut vmcb = Vmcb::new().ok()?;
        let setup = LongModeGuestSetup {
            asid: 1,
            nested_cr3: ncr3,
            guest_cr3,
            entry_ip: 0,
            stack_pointer: GUEST_LM_STACK,
        };
        program_long_mode_hlt_guest(vmcb.as_bytes_mut(), &setup).ok()?;

        // Point GDTR/IDTR at the guest's tables (same as the event guest).
        write_segment(
            vmcb.as_bytes_mut(),
            save::GDTR,
            VmcbSegment {
                selector: 0,
                attrib: 0,
                limit: 0x0017,
                base: GUEST_LM_GDT_GPA as u64,
            },
        );
        write_segment(
            vmcb.as_bytes_mut(),
            save::IDTR,
            VmcbSegment {
                selector: 0,
                attrib: 0,
                limit: 0x0FFF,
                base: GUEST_LM_IDT_GPA as u64,
            },
        );

        // Arm port-I/O interception so each OUT takes an IOIO exit.
        let iopm = IoPermissionsMap::intercept_all().ok()?;
        enable_io_intercept(vmcb.as_bytes_mut(), iopm.base_addr());
        core::mem::forget(iopm);

        // Post a pending virtual interrupt (V_IRQ) at high priority. Unlike
        // EVENTINJ it is gated by the guest's EFLAGS.IF — held off until the
        // guest STIs. The hardware clears V_IRQ once it delivers the interrupt,
        // so it fires exactly once; the run loop does not touch INT_CONTROL.
        set_int_control(vmcb.as_bytes_mut(), encode_vintr(GUEST_EVENT_VECTOR, 0xF));

        let vmcb_pa = vmcb.base_addr();
        core::mem::forget(vmcb); // must outlive VMRUN
        Some((vmcb_pa, guest_spa))
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

    /// Service an intercepted guest `VMMCALL` (a paravirt hypercall).
    ///
    /// The hypercall number is in the guest's `AX` (low 16 bits of the
    /// VMCB-carried `RAX`). For [`GUEST_VMMCALL_NUMBER`](super::GUEST_VMMCALL_NUMBER)
    /// enlil writes [`GUEST_VMMCALL_RESULT`](super::GUEST_VMMCALL_RESULT) back
    /// into guest `RAX` (the ABI: result in `RAX`); an unknown number leaves
    /// `RAX` unchanged. This is the enlil↔guest channel a real paravirt guest
    /// uses (event signaling, fast MMIO, etc.).
    fn emulate_vmmcall(vmcb: &mut [u8]) {
        use enlil_hal::svm::{guest_rax, set_guest_rax};

        let number = u16::try_from(guest_rax(vmcb) & 0xFFFF).unwrap_or(u16::MAX);
        if number == super::GUEST_VMMCALL_NUMBER {
            set_guest_rax(vmcb, u64::from(super::GUEST_VMMCALL_RESULT));
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

    /// Grant write access to the write-protected page a present+write NPF hit,
    /// so a re-`VMRUN` completes the guest's store. Returns whether the leaf was
    /// made writable.
    ///
    /// The fault (present + write, [`NptFaultInfo`](enlil_hal::svm::NptFaultInfo))
    /// is the signal dirty-page tracking / copy-on-write want; here enlil simply
    /// clears the write-protection ([`set_npt_2mib_leaf_writable`]) after noting
    /// the write, then resumes without advancing RIP so the store re-executes.
    ///
    /// # Safety
    ///
    /// `vmcb`'s `NESTED_CR3` must point at the identity-mapped NPT (3 pages).
    unsafe fn grant_npf_write(vmcb: &[u8]) -> bool {
        use enlil_hal::npt::{HUGE_2MIB, set_npt_2mib_leaf_writable};
        use enlil_hal::svm::{control, exit_info_2};

        let fault_gpa = exit_info_2(vmcb) & !(HUGE_2MIB - 1);
        let mut b = [0u8; 8];
        b.copy_from_slice(&vmcb[control::NESTED_CR3..control::NESTED_CR3 + 8]);
        let ncr3 = u64::from_le_bytes(b);
        // SAFETY: ncr3 is the identity-mapped NPT root (3 pages) the guest runs on.
        let npt = unsafe { core::slice::from_raw_parts_mut(ncr3 as *mut u8, 3 * 4096) };
        set_npt_2mib_leaf_writable(npt, ncr3, fault_gpa, true).is_ok()
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
            CPUID_INSN_LEN, IoioExitInfo, MSR_INSN_LEN, NptFaultInfo, RunLoopExit,
            VMMCALL_INSN_LEN, classify_run_loop_exit, encode_event_inj, event_type,
            exception_has_error_code, exit_code, exit_info_1, exit_info_2, guest_rax, guest_rip,
            ioio_to_vmexit, next_rip, resume_rip_after, set_event_inj, set_guest_rip,
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
                let info = NptFaultInfo::from_raw(exit_info_1(vmcb));
                if info.was_present() && info.was_write() {
                    // A write to a present-but-write-protected page — the dirty-
                    // page / copy-on-write signal. Record it, grant write, and
                    // re-run so the write completes (no RIP advance).
                    // SAFETY: the caller guarantees an identity-mapped NPT root.
                    if unsafe { grant_npf_write(vmcb) } {
                        outcome.npf_write_faults += 1;
                        return true;
                    }
                } else if unsafe { demand_map_npf(vmcb) } {
                    // A not-present fault — demand-map the missing page.
                    // SAFETY: the caller guarantees an identity-mapped NPT root.
                    outcome.npf_exits += 1;
                    return true; // resume WITHOUT advancing RIP
                }
                outcome.stop = super::RunStop::Unhandled;
            }
            RunLoopExit::Vmmcall => {
                outcome.vmmcall_exits += 1;
                emulate_vmmcall(vmcb);
                let rip = resume_rip_after(next_rip(vmcb), guest_rip(vmcb), VMMCALL_INSN_LEN);
                set_guest_rip(vmcb, rip);
                return true;
            }
            RunLoopExit::Exception { vector } => {
                outcome.exception_exits += 1;
                outcome.last_exception_vector = Some(vector);
                // Re-deliver the trapped fault to the guest's own IDT/IVT so its
                // handler runs — exception virtualization. Carry the EXITINFO1
                // error code for the vectors that push one (#DF/#TS/#NP/#SS/#GP/
                // #PF/#AC/#CP). Resume WITHOUT advancing RIP: the fault is on the
                // current instruction, and VMRUN's injected event pushes the
                // faulting CS:IP and vectors through the guest's IDT itself.
                let error_code = exception_has_error_code(vector)
                    .then(|| u32::try_from(exit_info_1(vmcb) & 0xFFFF_FFFF).unwrap_or(0));
                let inj = encode_event_inj(vector, event_type::EXCEPTION, error_code);
                set_event_inj(vmcb, inj);
                return true;
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
            edx: 0x7648_2074,
        };
        assert_eq!(sanitize_cpuid(0x4000_0000, raw), CpuidRegs::default());
        assert_eq!(sanitize_cpuid(0x4000_00FF, raw), CpuidRegs::default());
    }

    #[test]
    fn event_resume_program_lays_out_entry_ivt_and_iret_handler() {
        // A 2 MiB buffer stands in for the guest's isolated RAM window.
        let mut ram = vec![0u8; 0x0020_0000];
        write_event_resume_program(&mut ram);

        // Interrupted entry at GPA 0: OUT GUEST_EVENT_RESUME_PORT, AL; HLT.
        assert_eq!(ram[0], 0xE6, "entry OUT opcode");
        assert_eq!(ram[1], GUEST_EVENT_RESUME_PORT, "entry OUT port");
        assert_eq!(ram[2], 0xF4, "entry HLT");

        // IVT slot for the injected vector points at segment 0, handler offset.
        let ivt = (GUEST_EVENT_VECTOR as usize) * 4;
        let off = GUEST_EVENT_HANDLER_OFF.to_le_bytes();
        assert_eq!(ram[ivt], off[0]);
        assert_eq!(ram[ivt + 1], off[1]);
        assert_eq!(ram[ivt + 2], 0x00, "IVT segment low");
        assert_eq!(ram[ivt + 3], 0x00, "IVT segment high");

        // Handler: MOV AL, WORK; MOV [WORK_OFF], AL; MOV AL, RESUME; IRET.
        let h = GUEST_EVENT_HANDLER_OFF as usize;
        let work = GUEST_EVENT_WORK_OFF.to_le_bytes();
        assert_eq!(ram[h], 0xB0);
        assert_eq!(ram[h + 1], GUEST_EVENT_WORK_SENTINEL);
        assert_eq!(ram[h + 2], 0xA2, "MOV moffs16, AL (store)");
        assert_eq!(ram[h + 3], work[0]);
        assert_eq!(ram[h + 4], work[1]);
        assert_eq!(ram[h + 5], 0xB0);
        assert_eq!(ram[h + 6], GUEST_EVENT_RESUME_SENTINEL);
        assert_eq!(ram[h + 7], 0xCF, "IRET");

        // The handler's store target and the injection stack frame near SP=0
        // must not clobber the entry, IVT, or handler bytes.
        assert!(
            GUEST_EVENT_WORK_OFF as usize > h + 7,
            "work marker past handler"
        );
        assert!(
            usize::from(GUEST_EVENT_WORK_OFF) < 0xFFF0,
            "work marker below the stack frame"
        );
    }

    #[test]
    fn long_mode_event_program_lays_out_gdt_idt_and_iretq_handler() {
        let mut ram = vec![0u8; 0x0020_0000];
        write_long_mode_event_program(&mut ram);

        // Interrupted entry at GVA 0: OUT GUEST_LM_EVENT_PORT, AL; HLT.
        assert_eq!(ram[0], 0xE6);
        assert_eq!(ram[1], GUEST_LM_EVENT_PORT);
        assert_eq!(ram[2], 0xF4);

        // Handler: MOV AL, SENTINEL; IRETQ (REX.W CF — pops the 64-bit frame).
        let h = GUEST_LM_HANDLER_OFF;
        assert_eq!(ram[h], 0xB0);
        assert_eq!(ram[h + 1], GUEST_LM_EVENT_SENTINEL);
        assert_eq!(ram[h + 2], 0x48, "REX.W");
        assert_eq!(ram[h + 3], 0xCF, "IRETQ");

        // GDT: [0] null, [1] 64-bit code (selector 0x08), [2] data (0x10).
        let g = GUEST_LM_GDT_GPA;
        assert!(ram[g..g + 8].iter().all(|&b| b == 0), "null descriptor");
        let code = u64::from_le_bytes(ram[g + 8..g + 16].try_into().unwrap());
        assert_eq!(code, 0x00AF_9B00_0000_FFFF, "64-bit code descriptor");
        let data = u64::from_le_bytes(ram[g + 16..g + 24].try_into().unwrap());
        assert_eq!(data, 0x00CF_9300_0000_FFFF, "data descriptor");

        // IDT 64-bit interrupt gate for the vector → selector 0x08, offset
        // GUEST_LM_HANDLER_OFF, present/DPL0/type-0xE.
        let gate = GUEST_LM_IDT_GPA + (GUEST_EVENT_VECTOR as usize) * 16;
        let ob = (GUEST_LM_HANDLER_OFF as u64).to_le_bytes();
        assert_eq!(ram[gate], ob[0], "offset 7:0");
        assert_eq!(ram[gate + 1], ob[1], "offset 15:8");
        assert_eq!(ram[gate + 2], 0x08, "gate selector low (code)");
        assert_eq!(ram[gate + 3], 0x00, "gate selector high");
        assert_eq!(ram[gate + 4], 0x00, "IST");
        assert_eq!(ram[gate + 5], 0x8E, "P|DPL0|64-bit interrupt gate");
        assert_eq!(ram[gate + 6], ob[2], "offset 23:16");
        assert_eq!(ram[gate + 7], ob[3], "offset 31:24");
    }

    #[test]
    fn vintr_program_masks_then_stis_before_the_handler() {
        let mut ram = vec![0u8; 0x0020_0000];
        write_long_mode_vintr_program(&mut ram);

        // Entry: mov al, BEFORE; out BEFORE_PORT; sti; nop; mov al, RESUME;
        // out RESUME_PORT; hlt — the OUT-before-STI is the masked window.
        assert_eq!(ram[0], 0xB0);
        assert_eq!(ram[1], GUEST_LM_VINTR_BEFORE_SENTINEL);
        assert_eq!(ram[2], 0xE6);
        assert_eq!(ram[3], GUEST_LM_VINTR_BEFORE_PORT);
        assert_eq!(ram[4], 0xFB, "STI");
        assert_eq!(ram[5], 0x90, "NOP (STI shadow)");
        assert_eq!(ram[6], 0xB0);
        assert_eq!(ram[7], GUEST_LM_VINTR_RESUME_SENTINEL);
        assert_eq!(ram[8], 0xE6);
        assert_eq!(ram[9], GUEST_LM_VINTR_RESUME_PORT);
        assert_eq!(ram[10], 0xF4, "HLT");

        // Handler: mov al, HANDLER; out HANDLER_PORT; iretq.
        let h = GUEST_LM_HANDLER_OFF;
        assert_eq!(ram[h], 0xB0);
        assert_eq!(ram[h + 1], GUEST_LM_VINTR_HANDLER_SENTINEL);
        assert_eq!(ram[h + 2], 0xE6);
        assert_eq!(ram[h + 3], GUEST_LM_VINTR_HANDLER_PORT);
        assert_eq!(ram[h + 4], 0x48, "REX.W");
        assert_eq!(ram[h + 5], 0xCF, "IRETQ");

        // Shares the long-mode GDT + IDT gate with the event guest.
        let g = GUEST_LM_GDT_GPA;
        assert_eq!(
            u64::from_le_bytes(ram[g + 8..g + 16].try_into().unwrap()),
            0x00AF_9B00_0000_FFFF
        );
        let gate = GUEST_LM_IDT_GPA + (GUEST_EVENT_VECTOR as usize) * 16;
        assert_eq!(ram[gate + 2], 0x08, "gate selector (code)");
        assert_eq!(ram[gate + 5], 0x8E, "64-bit interrupt gate");
    }

    #[test]
    fn preempt_program_spins_forever_until_interrupted() {
        let mut ram = vec![0u8; 0x0020_0000];
        write_long_mode_preempt_program(&mut ram);

        // Entry: sti; jmp $ (EB FE = jump to self — an infinite loop, no exit).
        assert_eq!(ram[0], 0xFB, "STI");
        assert_eq!(ram[1], 0xEB, "JMP rel8");
        assert_eq!(ram[2], 0xFE, "-2 → spin on the JMP itself");

        // Handler: mov al, SENTINEL; out PREEMPT_PORT; hlt (stops the guest).
        let h = GUEST_LM_HANDLER_OFF;
        assert_eq!(ram[h], 0xB0);
        assert_eq!(ram[h + 1], GUEST_LM_PREEMPT_SENTINEL);
        assert_eq!(ram[h + 2], 0xE6);
        assert_eq!(ram[h + 3], GUEST_LM_PREEMPT_PORT);
        assert_eq!(ram[h + 4], 0xF4, "HLT");

        // Reuses the shared long-mode GDT + IDT gate.
        let gate = GUEST_LM_IDT_GPA + (GUEST_EVENT_VECTOR as usize) * 16;
        assert_eq!(ram[gate + 5], 0x8E, "64-bit interrupt gate");
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

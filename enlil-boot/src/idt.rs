//! Interrupt descriptor table for the enlil kernel (Phase 6.2).
//!
//! After `ExitBootServices()` the kernel still runs on the firmware's IDT,
//! whose handlers live in boot-services memory the kernel is about to
//! reclaim. This module gives the kernel its own: a full 256-entry IDT with
//! exception handlers that report on the serial console instead of silently
//! triple-faulting, plus a breakpoint (`int3`) self-test proving the table is
//! live — the first interrupt taken under enlil's own control.
//!
//! The 16-byte gate-descriptor encoding (Intel SDM Vol. 3 §6.14.1 / AMD APM
//! Vol. 2 §4.6.5 — identical layout) is pure and host-tested; only the
//! `lidt`/handler code is gated to the firmware target.

/// Number of IDT vectors on x86-64.
pub const IDT_ENTRIES: usize = 256;

/// 64-bit interrupt-gate type (IF cleared on entry).
pub const GATE_TYPE_INTERRUPT: u8 = 0xE;

/// 64-bit trap-gate type (IF left as-is — used for `int3` so the self-test
/// does not disturb interruptibility).
pub const GATE_TYPE_TRAP: u8 = 0xF;

/// One 16-byte IDT gate descriptor in the exact hardware layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdtEntry {
    offset_low: u16,
    selector: u16,
    options: u16,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    /// A not-present entry (all zeros): vectoring through it faults with #NP
    /// rather than jumping to address 0.
    #[must_use]
    pub const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            options: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// Encode a present ring-0 gate for `handler` in code segment `selector`.
    ///
    /// `ist` (0 = use the current stack, 1–7 = an IST stack) and `gate_type`
    /// ([`GATE_TYPE_INTERRUPT`] or [`GATE_TYPE_TRAP`]) fill the options word;
    /// DPL is fixed at 0 — no enlil interrupt is software-invocable from a
    /// less privileged ring.
    #[must_use]
    pub const fn new(handler: u64, selector: u16, ist: u8, gate_type: u8) -> Self {
        let options =
            (ist as u16 & 0x7) | ((gate_type as u16 & 0xF) << 8) | (1 << 15) /* present */;
        Self {
            offset_low: (handler & 0xFFFF) as u16,
            selector,
            options,
            offset_mid: ((handler >> 16) & 0xFFFF) as u16,
            offset_high: ((handler >> 32) & 0xFFFF_FFFF) as u32,
            reserved: 0,
        }
    }

    /// The full 64-bit handler address this gate vectors to.
    #[must_use]
    pub const fn handler_addr(&self) -> u64 {
        self.offset_low as u64
            | ((self.offset_mid as u64) << 16)
            | ((self.offset_high as u64) << 32)
    }

    /// Whether the present bit is set.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        self.options & (1 << 15) != 0
    }

    /// The gate type field (0xE interrupt, 0xF trap).
    #[must_use]
    pub const fn gate_type(&self) -> u8 {
        ((self.options >> 8) & 0xF) as u8
    }

    /// The IST index (0 = none).
    #[must_use]
    pub const fn ist(&self) -> u8 {
        (self.options & 0x7) as u8
    }

    /// The code-segment selector the gate loads.
    #[must_use]
    pub const fn selector(&self) -> u16 {
        self.selector
    }
}

/// The `lidt` operand: table limit and linear base (10 bytes, packed).
#[repr(C, packed)]
pub struct IdtPointer {
    /// Table size in bytes minus one.
    pub limit: u16,
    /// Linear base address of the table.
    pub base: u64,
}

#[cfg(target_os = "uefi")]
pub use hw::{
    InterruptStackFrame, breakpoint_hits, init_and_selftest, install_interrupt_gate,
    install_timer_gate, repoint_df_to_ist, repoint_gp_to_ist, repoint_pf_to_ist, timer_ticks,
};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{GATE_TYPE_INTERRUPT, GATE_TYPE_TRAP, IDT_ENTRIES, IdtEntry, IdtPointer};
    use crate::serial::SerialPort;
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicU32, Ordering};

    /// The stack frame the CPU pushes for an `x86-interrupt` handler.
    #[repr(C)]
    pub struct InterruptStackFrame {
        rip: u64,
        cs: u64,
        rflags: u64,
        rsp: u64,
        ss: u64,
    }

    /// The kernel IDT. Single boot CPU, written once before any interrupt
    /// can vector through it, so the plain `UnsafeCell` is sound.
    struct IdtStore(UnsafeCell<[IdtEntry; IDT_ENTRIES]>);
    // SAFETY: written only during single-CPU init (before `lidt`); the CPU
    // reads it afterward.
    unsafe impl Sync for IdtStore {}

    static IDT: IdtStore = IdtStore(UnsafeCell::new([IdtEntry::missing(); IDT_ENTRIES]));

    /// Breakpoint-handler hit counter — the self-test's observable.
    static BREAKPOINT_HITS: AtomicU32 = AtomicU32::new(0);

    /// LAPIC-timer-handler tick counter — the timer self-test's observable.
    static TIMER_TICKS: AtomicU32 = AtomicU32::new(0);

    /// How many times the breakpoint handler has run.
    pub fn breakpoint_hits() -> u32 {
        BREAKPOINT_HITS.load(Ordering::Acquire)
    }

    /// How many times the LAPIC timer handler has run.
    pub fn timer_ticks() -> u32 {
        TIMER_TICKS.load(Ordering::Acquire)
    }

    extern "x86-interrupt" fn breakpoint_handler(_frame: InterruptStackFrame) {
        // A trap gate: return continues at the instruction after `int3`.
        BREAKPOINT_HITS.fetch_add(1, Ordering::AcqRel);
    }

    /// LAPIC timer interrupt handler: count the tick and acknowledge the APIC.
    ///
    /// An interrupt gate (IF cleared on entry), so it is not re-entered; it
    /// signals EOI so the LAPIC can deliver further interrupts, then `IRET`s
    /// back to the interrupted code (the kernel's wait loop).
    extern "x86-interrupt" fn timer_handler(_frame: InterruptStackFrame) {
        TIMER_TICKS.fetch_add(1, Ordering::AcqRel);
        crate::apic::signal_eoi();
    }

    /// Install the LAPIC timer handler at `vector` (an interrupt gate).
    ///
    /// Written into the live IDT after `lidt`; the CPU re-reads the table per
    /// interrupt, so a post-load gate write takes effect. Call with interrupts
    /// disabled on the single boot CPU (as `kernel_entry` does) so no interrupt
    /// races the write.
    pub fn install_timer_gate(vector: u8) {
        type Handler = extern "x86-interrupt" fn(InterruptStackFrame);
        let addr = |h: Handler| h as usize as u64;
        let cs = current_cs();
        // SAFETY: single boot CPU with interrupts disabled; the IDT static is
        // live but no interrupt vectors through `vector` until the timer arms.
        let idt = unsafe { &mut *IDT.0.get() };
        idt[vector as usize] = IdtEntry::new(addr(timer_handler), cs, 0, GATE_TYPE_INTERRUPT);
    }

    /// Install an interrupt gate at `vector` for `handler_addr`, using IST index
    /// `ist`.
    ///
    /// `ist` is 0 for the current stack or 1–7 for the TSS's `IST` stack — the
    /// seam the `gdt` module uses to route a vector onto its IST stack for the
    /// self-test. Written into the live IDT (post-`lidt`); call with interrupts
    /// disabled.
    pub fn install_interrupt_gate(vector: u8, handler_addr: u64, ist: u8) {
        let cs = current_cs();
        // SAFETY: single boot CPU, interrupts disabled; the IDT is live but
        // nothing vectors through `vector` until the caller triggers it.
        let idt = unsafe { &mut *IDT.0.get() };
        idt[vector as usize] = IdtEntry::new(handler_addr, cs, ist, GATE_TYPE_INTERRUPT);
    }

    /// Re-point the `#DF` (double-fault, vector 8) gate at IST index `ist`, so a
    /// double fault runs on a known-good stack instead of the faulting one.
    ///
    /// Written into the live IDT; call with interrupts disabled after the TSS
    /// (whose `IST` stack this names) is loaded.
    pub fn repoint_df_to_ist(ist: u8) {
        type HandlerErr = extern "x86-interrupt" fn(InterruptStackFrame, u64);
        let addr = |h: HandlerErr| h as usize as u64;
        let cs = current_cs();
        // SAFETY: single boot CPU, interrupts disabled; rewriting one live gate.
        let idt = unsafe { &mut *IDT.0.get() };
        idt[8] = IdtEntry::new(addr(double_fault_handler), cs, ist, GATE_TYPE_INTERRUPT);
    }

    /// Re-point the `#PF` (page-fault, vector 14) gate at IST index `ist`, so a
    /// page fault runs on a known-good stack.
    ///
    /// This protects the handler from a fault taken on an already-corrupt or
    /// overflowed kernel stack. Written into the live IDT; call with interrupts
    /// disabled after the TSS (whose `IST` stack this names) is loaded.
    pub fn repoint_pf_to_ist(ist: u8) {
        type HandlerErr = extern "x86-interrupt" fn(InterruptStackFrame, u64);
        let addr = |h: HandlerErr| h as usize as u64;
        let cs = current_cs();
        // SAFETY: single boot CPU, interrupts disabled; rewriting one live gate.
        let idt = unsafe { &mut *IDT.0.get() };
        idt[14] = IdtEntry::new(addr(page_fault_handler), cs, ist, GATE_TYPE_INTERRUPT);
    }

    /// Re-point the `#GP` (general-protection, vector 13) gate at IST index
    /// `ist`, so a general-protection fault runs on a known-good stack.
    ///
    /// Written into the live IDT; call with interrupts disabled after the TSS
    /// (whose `IST` stack this names) is loaded.
    pub fn repoint_gp_to_ist(ist: u8) {
        type HandlerErr = extern "x86-interrupt" fn(InterruptStackFrame, u64);
        let addr = |h: HandlerErr| h as usize as u64;
        let cs = current_cs();
        // SAFETY: single boot CPU, interrupts disabled; rewriting one live gate.
        let idt = unsafe { &mut *IDT.0.get() };
        idt[13] = IdtEntry::new(
            addr(general_protection_handler),
            cs,
            ist,
            GATE_TYPE_INTERRUPT,
        );
    }

    /// Report-and-park handler for faults without an error code.
    macro_rules! fault_handler {
        ($name:ident, $label:expr) => {
            extern "x86-interrupt" fn $name(_frame: InterruptStackFrame) {
                fault_park($label);
            }
        };
    }

    /// Report-and-park handler for faults that push an error code.
    macro_rules! fault_handler_err {
        ($name:ident, $label:expr) => {
            extern "x86-interrupt" fn $name(_frame: InterruptStackFrame, _error_code: u64) {
                fault_park($label);
            }
        };
    }

    fault_handler!(divide_error_handler, "#DE divide error");
    fault_handler!(invalid_opcode_handler, "#UD invalid opcode");
    fault_handler_err!(double_fault_handler, "#DF double fault");
    fault_handler_err!(general_protection_handler, "#GP general protection");
    fault_handler_err!(page_fault_handler, "#PF page fault");

    /// Emit the fault on serial and halt: the kernel has no recovery path
    /// yet, but a named serial line beats a silent triple-fault reboot.
    fn fault_park(label: &str) -> ! {
        let serial = SerialPort::com1();
        serial.write_str("enlil kernel: FAULT: ");
        serial.write_str(label);
        serial.write_str("\n");
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }

    /// The current code-segment selector (the gates must target the segment
    /// the kernel executes in — the firmware's flat 64-bit code segment).
    fn current_cs() -> u16 {
        let cs: u16;
        unsafe {
            core::arch::asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags));
        }
        cs
    }

    /// Build the kernel IDT, load it, and prove it vectors: fire `int3` and
    /// check the breakpoint handler ran. Returns whether the self-test
    /// passed.
    pub fn init_and_selftest() -> bool {
        // Function-pointer types for the two handler shapes, so the address
        // is taken via a typed fn pointer (not a direct function-item cast).
        type Handler = extern "x86-interrupt" fn(InterruptStackFrame);
        type HandlerErr = extern "x86-interrupt" fn(InterruptStackFrame, u64);
        let addr = |h: Handler| h as usize as u64;
        let addr_err = |h: HandlerErr| h as usize as u64;

        let cs = current_cs();
        // SAFETY: single boot CPU, no interrupts vector through the table
        // until `lidt` below; see `IdtStore`.
        let idt = unsafe { &mut *IDT.0.get() };
        idt[0] = IdtEntry::new(addr(divide_error_handler), cs, 0, GATE_TYPE_INTERRUPT);
        idt[3] = IdtEntry::new(addr(breakpoint_handler), cs, 0, GATE_TYPE_TRAP);
        idt[6] = IdtEntry::new(addr(invalid_opcode_handler), cs, 0, GATE_TYPE_INTERRUPT);
        idt[8] = IdtEntry::new(addr_err(double_fault_handler), cs, 0, GATE_TYPE_INTERRUPT);
        idt[13] = IdtEntry::new(
            addr_err(general_protection_handler),
            cs,
            0,
            GATE_TYPE_INTERRUPT,
        );
        idt[14] = IdtEntry::new(addr_err(page_fault_handler), cs, 0, GATE_TYPE_INTERRUPT);

        let pointer = IdtPointer {
            limit: u16::try_from(core::mem::size_of::<[IdtEntry; IDT_ENTRIES]>() - 1)
                .unwrap_or(u16::MAX),
            base: IDT.0.get() as u64,
        };
        // SAFETY: the table is fully initialized and lives in a static; the
        // pointer operand describes exactly that table.
        unsafe {
            core::arch::asm!("lidt [{}]", in(reg) core::ptr::addr_of!(pointer), options(readonly, nostack, preserves_flags));
        }

        let before = breakpoint_hits();
        // SAFETY: vector 3 is installed as a trap gate; execution resumes
        // after the instruction.
        unsafe { core::arch::asm!("int3", options(nomem, nostack)) };
        breakpoint_hits() == before + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_encoding_round_trips_the_handler_address() {
        let entry = IdtEntry::new(0xFFFF_8000_1234_5678, 0x38, 0, GATE_TYPE_INTERRUPT);
        assert_eq!(entry.handler_addr(), 0xFFFF_8000_1234_5678);
        assert_eq!(entry.selector(), 0x38);
        assert!(entry.is_present());
        assert_eq!(entry.gate_type(), GATE_TYPE_INTERRUPT);
        assert_eq!(entry.ist(), 0);
    }

    #[test]
    fn trap_gate_and_ist_are_encoded() {
        let entry = IdtEntry::new(0x1000, 0x08, 2, GATE_TYPE_TRAP);
        assert_eq!(entry.gate_type(), GATE_TYPE_TRAP);
        assert_eq!(entry.ist(), 2);
        assert!(entry.is_present());
    }

    #[test]
    fn missing_entry_is_not_present() {
        let entry = IdtEntry::missing();
        assert!(!entry.is_present());
        assert_eq!(entry.handler_addr(), 0);
    }

    #[test]
    fn descriptor_sizes_match_the_hardware_layout() {
        // 16-byte gates, 10-byte lidt operand (SDM Vol. 3 §6.14.1).
        assert_eq!(core::mem::size_of::<IdtEntry>(), 16);
        assert_eq!(core::mem::size_of::<IdtPointer>(), 10);
    }

    #[test]
    fn low_mid_high_offset_split() {
        let entry = IdtEntry::new(0xAAAA_BBBB_CCCC_DDDD, 0x08, 0, GATE_TYPE_INTERRUPT);
        // Reassembly proves each piece landed in its hardware field.
        assert_eq!(entry.handler_addr(), 0xAAAA_BBBB_CCCC_DDDD);
        let bytes: [u8; 16] = unsafe { core::mem::transmute(entry) };
        assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 0xDDDD); // offset 15:0
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 0x0008); // selector
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 0xCCCC); // offset 31:16
        assert_eq!(
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            0xAAAA_BBBB // offset 63:32
        );
    }
}

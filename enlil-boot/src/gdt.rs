//! GDT + TSS descriptor primitives for the enlil kernel (Phase 6.2).
//!
//! A Task State Segment with an Interrupt Stack Table lets a fault handler run
//! on a known-good stack instead of the interrupted one — the difference
//! between a reported `#DF` and a silent triple-fault reboot when the kernel
//! stack overflows. That needs the kernel's own GDT (with a TSS descriptor) and
//! a TSS whose `IST1` points at a dedicated stack.
//!
//! This module is the pure, host-tested encoding layer: the 8-byte segment and
//! 16-byte system (TSS) descriptors and the long-mode TSS byte layout. The
//! privileged `lgdt`/`ltr` that install them (and point `#DF` at `IST1`) are a
//! follow-up — the ordering there needs care (the interrupt-gate code selector
//! must stay valid across the GDT swap), so the primitives land first.

/// A flat 64-bit code segment descriptor (present, ring 0, executable,
/// long-mode `L`-bit set): access `0x9A`, flags `0xA` (G + L).
///
/// Base and limit are ignored in 64-bit mode, so only the type/flag bits carry
/// meaning; this matches the firmware's own code descriptor so an interrupt
/// gate targeting this selector keeps working across a GDT swap.
#[must_use]
pub const fn code_segment_64() -> u64 {
    segment_descriptor(0x9A, 0xA)
}

/// A flat data segment descriptor (present, ring 0, writable): access `0x92`.
#[must_use]
pub const fn data_segment() -> u64 {
    segment_descriptor(0x92, 0xC)
}

/// Build an 8-byte segment descriptor with `access` (bits 47:40) and `flags`
/// (bits 55:52), a zero base and a full `0xFFFFF` limit (granular).
const fn segment_descriptor(access: u8, flags: u8) -> u64 {
    let limit_low = 0xFFFF_u64; // limit 15:0
    let limit_high = 0xF_u64; // limit 19:16
    limit_low | ((access as u64) << 40) | (limit_high << 48) | ((flags as u64 & 0xF) << 52)
}

/// The type nibble of an available 64-bit TSS descriptor (System Descriptor,
/// type `0x9`), present, ring 0 → access byte `0x89`.
pub const TSS_TYPE_AVAILABLE_64: u8 = 0x89;

/// A 16-byte long-mode system descriptor (as a GDT TSS entry) for a TSS at
/// `base` with byte-limit `limit`.
///
/// Two 8-byte halves: the low half is the standard segment-descriptor layout
/// with the available-64-bit-TSS type ([`TSS_TYPE_AVAILABLE_64`]); the high half
/// carries base bits 63:32. Returns `(low, high)`.
#[must_use]
pub const fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let limit_low = (limit as u64) & 0xFFFF;
    let limit_high = ((limit as u64) >> 16) & 0xF;
    let base_low = base & 0xFF_FFFF; // base 23:0
    let base_mid = (base >> 24) & 0xFF; // base 31:24
    let low = limit_low
        | (base_low << 16)
        | ((TSS_TYPE_AVAILABLE_64 as u64) << 40)
        | (limit_high << 48)
        | (base_mid << 56);
    let high = (base >> 32) & 0xFFFF_FFFF; // base 63:32
    (low, high)
}

/// The long-mode Task State Segment (AMD APM Vol. 2 §12.2.5 / SDM Vol. 3 §7.7).
///
/// `#[repr(C, packed)]` and the field order are load-bearing: the CPU indexes
/// this by fixed offset (`RSP0` at 4, `IST1` at 36). Only the `RSPn`/`ISTn`
/// stack pointers and the I/O-map base are meaningful; the reserved fields must
/// be zero.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Tss {
    reserved0: u32,
    /// Ring-0/1/2 stack pointers (unused by enlil — one ring).
    pub rsp: [u64; 3],
    reserved1: u64,
    /// The seven Interrupt Stack Table pointers; `ist[0]` is `IST1`.
    pub ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    /// I/O permission-bitmap base offset (set past the TSS to disable it).
    pub iomap_base: u16,
}

/// The I/O-map base that disables the bitmap: the TSS's own size in bytes (an
/// offset at/past the segment limit — moot for a ring-0-only kernel anyway).
const TSS_SIZE: u16 = 104;
const _: () = assert!(core::mem::size_of::<Tss>() == TSS_SIZE as usize);

impl Tss {
    /// A zeroed TSS with the I/O map disabled (base past the segment).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            reserved0: 0,
            rsp: [0; 3],
            reserved1: 0,
            ist: [0; 7],
            reserved2: 0,
            reserved3: 0,
            // Base ≥ limit disables the I/O bitmap.
            iomap_base: TSS_SIZE,
        }
    }

    /// Set `IST1` (the stack a fault gate with IST index 1 switches to) to
    /// `stack_top`.
    pub const fn set_ist1(&mut self, stack_top: u64) {
        self.ist[0] = stack_top;
    }
}

impl Default for Tss {
    fn default() -> Self {
        Self::new()
    }
}

/// The `lgdt` operand: table limit and linear base (10 bytes, packed).
#[repr(C, packed)]
pub struct GdtPointer {
    /// Table size in bytes minus one.
    pub limit: u16,
    /// Linear base address of the table.
    pub base: u64,
}

#[cfg(target_os = "uefi")]
pub use hw::install_and_selftest;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{GdtPointer, Tss, code_segment_64, data_segment, tss_descriptor};
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, Ordering};

    /// GDT slots. The firmware code selector's slot is overwritten with our own
    /// (matching) 64-bit code descriptor, and the TSS descriptor (16 bytes = 2
    /// slots) sits at [`TSS_INDEX`]; the rest stay null (unused).
    const GDT_ENTRIES: usize = 32;
    /// The `lgdt` limit (table bytes − 1). `GDT_ENTRIES * 8 − 1`.
    const GDT_LIMIT: u16 = 255;
    /// GDT slot of the TSS descriptor. Chosen high so it never collides with the
    /// firmware code-selector index.
    const TSS_INDEX: usize = 30;
    /// The TSS selector (`TSS_INDEX << 3`).
    const TSS_SELECTOR: u16 = 0xF0;
    const _: () = assert!(GDT_ENTRIES * 8 - 1 == GDT_LIMIT as usize);
    const _: () = assert!(TSS_INDEX << 3 == TSS_SELECTOR as usize);
    /// The self-test interrupt vector routed onto `IST1`.
    const IST_TEST_VECTOR: u8 = 0x41;
    /// Size of the dedicated IST stack (16 KiB).
    const IST_STACK_SIZE: usize = 16 * 1024;

    /// The `x86-interrupt` handler type — a typed fn pointer avoids a direct
    /// function-item-to-integer cast when taking the handler address.
    type IstHandler = extern "x86-interrupt" fn(crate::idt::InterruptStackFrame);

    #[repr(C, align(16))]
    struct GdtStore(UnsafeCell<[u64; GDT_ENTRIES]>);
    // SAFETY: written once during single-CPU init before `lgdt`/`ltr`.
    unsafe impl Sync for GdtStore {}

    #[repr(C, align(16))]
    struct TssStore(UnsafeCell<Tss>);
    // SAFETY: written once during single-CPU init before `ltr`.
    unsafe impl Sync for TssStore {}

    #[repr(C, align(16))]
    struct IstStack(UnsafeCell<[u8; IST_STACK_SIZE]>);
    // SAFETY: used only as the CPU's IST1 stack (the CPU writes it on an
    // IST-routed interrupt); never shared as data.
    unsafe impl Sync for IstStack {}

    static GDT: GdtStore = GdtStore(UnsafeCell::new([0; GDT_ENTRIES]));
    static TSS: TssStore = TssStore(UnsafeCell::new(Tss::new()));
    static IST_STACK: IstStack = IstStack(UnsafeCell::new([0; IST_STACK_SIZE]));

    /// Set by the self-test handler when it confirms it ran on the IST stack.
    static IST_PROBE_OK: AtomicBool = AtomicBool::new(false);

    /// The current code-segment selector.
    fn current_cs() -> u16 {
        let cs: u16;
        // SAFETY: reading CS is unprivileged and side-effect-free.
        unsafe {
            core::arch::asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags));
        }
        cs
    }

    /// The current stack-segment selector.
    fn current_ss() -> u16 {
        let ss: u16;
        // SAFETY: reading SS is unprivileged and side-effect-free.
        unsafe {
            core::arch::asm!("mov {0:x}, ss", out(reg) ss, options(nomem, nostack, preserves_flags));
        }
        ss
    }

    /// The current task-register selector (`str`).
    fn task_register() -> u16 {
        let tr: u16;
        // SAFETY: `str` is unprivileged and side-effect-free.
        unsafe {
            core::arch::asm!("str {0:x}", out(reg) tr, options(nomem, nostack, preserves_flags));
        }
        tr
    }

    /// The `IST1` self-test handler: confirm the CPU switched to the IST stack.
    ///
    /// Reads `RSP` and checks it lies inside [`IST_STACK`]; if so the IST switch
    /// worked. Returns via the `x86-interrupt` `IRETQ`, so triggering it does
    /// not hang (unlike the real park-on-fault handlers).
    extern "x86-interrupt" fn ist_probe_handler(_frame: crate::idt::InterruptStackFrame) {
        let rsp: u64;
        // SAFETY: reading RSP is side-effect-free.
        unsafe {
            core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
        }
        let base = IST_STACK.0.get() as u64;
        let top = base + IST_STACK_SIZE as u64;
        if rsp > base && rsp <= top {
            IST_PROBE_OK.store(true, Ordering::Release);
        }
    }

    /// Install the kernel's own GDT + TSS with an IST stack and prove the IST
    /// switch works.
    ///
    /// Returns whether every step behaved. Builds a GDT that keeps the firmware
    /// code selector valid (so the loaded
    /// IDT gates keep working — no CS reload) and adds a TSS descriptor, loads
    /// it (`lgdt`), loads the task register (`ltr`), points the `#DF` gate and a
    /// self-test vector at `IST1`, then fires the self-test vector and checks it
    /// ran on the IST stack. Single boot CPU, interrupts masked by the caller.
    #[must_use]
    pub fn install_and_selftest() -> bool {
        let cs_index = (current_cs() >> 3) as usize;
        let ss_index = (current_ss() >> 3) as usize;
        if cs_index == 0 || cs_index >= TSS_INDEX || ss_index >= TSS_INDEX {
            return false; // firmware CS/SS selector out of our GDT's usable range
        }

        // Program the TSS: IST1 = top of the dedicated stack (grows down).
        let ist_top = IST_STACK.0.get() as u64 + IST_STACK_SIZE as u64;
        // SAFETY: single-CPU init; the TSS is not yet loaded.
        let tss = unsafe { &mut *TSS.0.get() };
        tss.set_ist1(ist_top);
        let tss_addr = TSS.0.get() as u64;

        // Build the GDT: our code descriptor at the firmware CS index, the TSS
        // descriptor at TSS_INDEX (16 bytes → two slots).
        let tss_limit = u32::try_from(core::mem::size_of::<Tss>() - 1).unwrap_or(0);
        let (tss_lo, tss_hi) = tss_descriptor(tss_addr, tss_limit);
        // SAFETY: single-CPU init; the GDT is not yet loaded.
        let gdt = unsafe { &mut *GDT.0.get() };
        gdt[cs_index] = code_segment_64();
        // Replicate a data descriptor at the firmware SS index: in long mode
        // IRET reloads SS from the GDT, so a null slot there faults the first
        // interrupt return after the swap (SS != 0 at CPL 0). (DS/ES/FS/GS are
        // not reloaded by IRET, so only SS must be covered.)
        if ss_index != 0 {
            gdt[ss_index] = data_segment();
        }
        gdt[TSS_INDEX] = tss_lo;
        gdt[TSS_INDEX + 1] = tss_hi;

        let pointer = GdtPointer {
            limit: GDT_LIMIT,
            base: GDT.0.get() as u64,
        };
        // SAFETY: the GDT is fully built; the cached CS/DS descriptors persist
        // across lgdt, and GDT[cs_index] is a valid code segment for future
        // interrupt CS loads.
        unsafe {
            core::arch::asm!("lgdt [{}]", in(reg) core::ptr::addr_of!(pointer), options(readonly, nostack, preserves_flags));
        }
        // SAFETY: GDT[TSS_INDEX] is a valid available-64-bit-TSS descriptor.
        unsafe {
            core::arch::asm!("ltr {0:x}", in(reg) TSS_SELECTOR, options(nomem, nostack, preserves_flags));
        }

        // Route the self-test vector and the real #DF gate onto IST1. Take the
        // handler address via a typed fn pointer (not a direct item cast).
        let probe: IstHandler = ist_probe_handler;
        crate::idt::install_interrupt_gate(IST_TEST_VECTOR, probe as usize as u64, 1);
        crate::idt::repoint_df_to_ist(1);

        // Fire the self-test: a software interrupt switches to IST1, the handler
        // checks RSP is on that stack and IRETs. `int` is unaffected by IF.
        // SAFETY: the gate is installed with a handler that returns cleanly.
        unsafe {
            core::arch::asm!("int 0x41", options(nomem, nostack));
        }

        IST_PROBE_OK.load(Ordering::Acquire) && task_register() == TSS_SELECTOR
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn code_segment_sets_long_mode_and_present() {
        let d = code_segment_64();
        assert_eq!((d >> 40) & 0xFF, 0x9A); // access: P|DPL0|S|code|readable
        assert_ne!(d & (1 << 47), 0); // present
        assert_ne!(d & (1 << 53), 0); // L-bit (long mode)
    }

    #[test]
    fn data_segment_is_present_and_writable() {
        let d = data_segment();
        assert_eq!((d >> 40) & 0xFF, 0x92);
        assert_ne!(d & (1 << 47), 0); // present
    }

    #[test]
    fn tss_descriptor_encodes_base_and_type() {
        let base = 0x1234_5678_9ABC_u64;
        let (low, high) = tss_descriptor(base, 0x67);
        // Type is available-64-bit-TSS, present.
        assert_eq!((low >> 40) & 0xFF, u64::from(TSS_TYPE_AVAILABLE_64));
        // Base reassembles from its scattered fields.
        let base_low = (low >> 16) & 0xFF_FFFF;
        let base_mid = (low >> 56) & 0xFF;
        let base_high = high & 0xFFFF_FFFF;
        assert_eq!(base_low | (base_mid << 24) | (base_high << 32), base);
        // Limit low 16 bits.
        assert_eq!(low & 0xFFFF, 0x67);
    }

    #[test]
    fn tss_layout_matches_the_hardware() {
        // The CPU reads RSP0 at 4 and IST1 at 36 (SDM Vol. 3 §7.7).
        assert_eq!(size_of::<Tss>(), 104);
        assert_eq!(offset_of!(Tss, rsp), 4); // RSP0
        assert_eq!(offset_of!(Tss, ist), 36); // IST1
    }

    #[test]
    fn set_ist1_writes_the_first_ist_slot() {
        let mut tss = Tss::new();
        tss.set_ist1(0xFFFF_8000_0010_0000);
        // Copy the packed fields to locals before comparing (no unaligned refs).
        let ist0 = tss.ist[0];
        let iomap = tss.iomap_base;
        assert_eq!(ist0, 0xFFFF_8000_0010_0000);
        // The I/O map is disabled (base at/after the segment end).
        assert!(usize::from(iomap) >= size_of::<Tss>());
    }
}

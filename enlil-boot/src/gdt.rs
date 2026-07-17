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

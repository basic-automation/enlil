//! An I/O APIC: the interrupt router x86-64 operating systems expect.
//!
//! Legacy ISA interrupts and PCI `INTx` lines do not reach the CPU directly on a
//! machine with an APIC: they arrive on I/O APIC *pins*, and a 64-bit
//! *redirection entry* per pin says which vector to raise, whether the line is
//! edge- or level-triggered, its polarity, and whether it is masked. Windows on
//! x86-64 will not run without this (it has no PIC-only mode); Linux uses it
//! when the ACPI tables say it exists.
//!
//! The model follows QEMU's IOAPIC (version `0x11`, 24 pins): the OS reaches the
//! registers through an index (`IOREGSEL`, offset `0x00`) and a window (`IOWIN`,
//! offset `0x10`). A level-triggered interrupt sets *Remote IRR* when it is
//! delivered, and stays blocked until the local APIC's end-of-interrupt for that
//! vector clears it; if the line is still asserted it is delivered again. There
//! is one CPU, so every interrupt goes to local APIC 0.
//!
//! Pure and host-tested.

use crate::platform::Lapic;

/// The I/O APIC's MMIO base (the architectural default).
pub const IOAPIC_BASE: u64 = 0xFEC0_0000;
/// The I/O APIC's MMIO window.
pub const IOAPIC_BYTES: u64 = 0x1000;
/// Interrupt input pins.
pub const PINS: usize = 24;
/// The same count as a register index bound.
const REDIR_REGISTERS: u8 = 48;

const OFF_IOREGSEL: u64 = 0x00;
const OFF_IOWIN: u64 = 0x10;
const REG_ID: u8 = 0x00;
const REG_VER: u8 = 0x01;
const REG_ARB: u8 = 0x02;
const REG_REDIR: u8 = 0x10;
/// `IOAPICVER`: version `0x11`, highest redirection entry `PINS - 1`.
const VERSION: u32 = 0x11 | (23 << 16);

// Redirection entry bits (low dword).
const R_VECTOR: u64 = 0xFF;
const R_DELIVERY_MODE: u64 = 0b111 << 8;
const R_DEST_MODE: u64 = 1 << 11;
const R_POLARITY_LOW: u64 = 1 << 13;
const R_REMOTE_IRR: u64 = 1 << 14;
const R_LEVEL: u64 = 1 << 15;
const R_MASKED: u64 = 1 << 16;
/// The bits an OS can write: everything but delivery status (12) and Remote IRR (14).
const R_WRITABLE_LOW: u64 =
    R_VECTOR | R_DELIVERY_MODE | R_DEST_MODE | R_POLARITY_LOW | R_LEVEL | R_MASKED;
/// The destination field (high dword, bits 63:56).
const R_DEST: u64 = 0xFF << 56;
/// Delivery mode 0: fixed.
const MODE_FIXED: u64 = 0;
/// Delivery mode 1: lowest priority (with one CPU, the same as fixed).
const MODE_LOWEST: u64 = 1;

/// The I/O APIC.
#[derive(Debug, Clone)]
pub struct IoApic {
    id: u32,
    index: u8,
    redir: [u64; PINS],
    /// The level each input is currently driven to.
    line: u32,
    /// Edge-triggered requests latched while masked, delivered on unmask.
    irr: u32,
}

impl Default for IoApic {
    fn default() -> Self {
        Self::new()
    }
}

impl IoApic {
    /// A reset I/O APIC: every pin masked, ID 0.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            id: 0,
            index: 0,
            redir: [R_MASKED; PINS],
            line: 0,
            irr: 0,
        }
    }

    /// Whether `gpa` falls in the I/O APIC's window.
    #[must_use]
    pub const fn contains(gpa: u64) -> bool {
        gpa >= IOAPIC_BASE && gpa < IOAPIC_BASE + IOAPIC_BYTES
    }

    /// A register read at `offset` in the window (only the index and the data
    /// window exist; anything else reads zero).
    #[must_use]
    pub fn read(&self, offset: u64, size: u8) -> u64 {
        let v = match offset & !0xF {
            OFF_IOREGSEL => u32::from(self.index),
            OFF_IOWIN => self.read_register(),
            _ => 0,
        };
        crate::insn::mask_to(u64::from(v), size)
    }

    fn read_register(&self) -> u32 {
        match self.index {
            REG_ID | REG_ARB => self.id << 24,
            REG_VER => VERSION,
            i if (REG_REDIR..REG_REDIR + REDIR_REGISTERS).contains(&i) => {
                let pin = usize::from((i - REG_REDIR) / 2);
                let e = self.redir[pin];
                #[allow(clippy::cast_possible_truncation)] // a register half
                if (i - REG_REDIR).is_multiple_of(2) {
                    e as u32
                } else {
                    (e >> 32) as u32
                }
            }
            _ => 0,
        }
    }

    /// A register write of `value` at `offset`; unmasking or retriggering a pin
    /// may deliver an interrupt through `lapic`.
    pub fn write(&mut self, offset: u64, size: u8, value: u64, lapic: &mut Lapic) {
        if size != 4 {
            return;
        }
        #[allow(clippy::cast_possible_truncation)] // a 32-bit register write
        let v = value as u32;
        match offset & !0xF {
            OFF_IOREGSEL => self.index = (v & 0xFF) as u8,
            OFF_IOWIN => self.write_register(v, lapic),
            _ => {}
        }
    }

    fn write_register(&mut self, v: u32, lapic: &mut Lapic) {
        match self.index {
            REG_ID => self.id = (v >> 24) & 0xF,
            i if (REG_REDIR..REG_REDIR + REDIR_REGISTERS).contains(&i) => {
                let pin = usize::from((i - REG_REDIR) / 2);
                let old = self.redir[pin];
                self.redir[pin] = if (i - REG_REDIR).is_multiple_of(2) {
                    // Delivery status and Remote IRR are not the OS's to write;
                    // switching a pin to edge-triggered clears Remote IRR.
                    let mut e = (old & !R_WRITABLE_LOW) | (u64::from(v) & R_WRITABLE_LOW);
                    if e & R_LEVEL == 0 {
                        e &= !R_REMOTE_IRR;
                    } else {
                        e |= old & R_REMOTE_IRR;
                    }
                    (old & 0xFFFF_FFFF_0000_0000) | (e & 0xFFFF_FFFF)
                } else {
                    (old & 0xFFFF_FFFF) | ((u64::from(v) << 32) & R_DEST)
                };
                // A change can unmask a request that arrived while masked.
                self.service(pin, lapic);
            }
            _ => {}
        }
    }

    /// Whether the pin's input is asserted.
    ///
    /// Devices here are software: they drive a pin *logically* ("I am requesting
    /// an interrupt"), so there is no electrical level for the entry's polarity
    /// bit to invert. It is stored and read back, and otherwise ignored, as in
    /// QEMU's model; honouring it would make every idle active-low PCI pin look
    /// asserted.
    const fn asserted(&self, pin: usize) -> bool {
        self.line & (1 << pin) != 0
    }

    /// Drive input `pin` to the logical `level` (`true` = requesting an
    /// interrupt). A rising edge on an edge-triggered pin, or an asserted level on
    /// a level-triggered one, raises its vector.
    pub fn set_irq(&mut self, pin: u8, level: bool, lapic: &mut Lapic) {
        let pin = usize::from(pin);
        if pin >= PINS {
            return;
        }
        let was = self.asserted(pin);
        if level {
            self.line |= 1 << pin;
        } else {
            self.line &= !(1 << pin);
        }
        let is = self.asserted(pin);
        if self.redir[pin] & R_LEVEL == 0 {
            if is && !was {
                self.irr |= 1 << pin;
            }
        } else if !is {
            // A level that drops before it was serviced is forgotten.
            self.irr &= !(1 << pin);
        }
        self.service(pin, lapic);
    }

    /// Deliver `pin`'s pending interrupt if it may go out now.
    fn service(&mut self, pin: usize, lapic: &mut Lapic) {
        let e = self.redir[pin];
        if e & R_MASKED != 0 {
            return;
        }
        let mode = (e & R_DELIVERY_MODE) >> 8;
        if mode != MODE_FIXED && mode != MODE_LOWEST {
            return; // NMI, SMI, INIT and ExtINT are not routed to the CPU model
        }
        #[allow(clippy::cast_possible_truncation)] // the vector field is eight bits
        let vector = (e & R_VECTOR) as u8;
        if e & R_LEVEL != 0 {
            if self.asserted(pin) && e & R_REMOTE_IRR == 0 {
                self.redir[pin] |= R_REMOTE_IRR;
                lapic.raise(vector);
            }
        } else if self.irr & (1 << pin) != 0 {
            self.irr &= !(1 << pin);
            lapic.raise(vector);
        }
    }

    /// The local APIC retired `vector` (an end of interrupt): level-triggered
    /// pins routed to it become deliverable again, and fire at once if still
    /// asserted.
    pub fn eoi(&mut self, vector: u8, lapic: &mut Lapic) {
        for pin in 0..PINS {
            let e = self.redir[pin];
            if e & R_LEVEL != 0 && e & R_REMOTE_IRR != 0 && (e & R_VECTOR) as u8 == vector {
                self.redir[pin] &= !R_REMOTE_IRR;
                self.service(pin, lapic);
            }
        }
    }
}

#[cfg(test)]
mod tests;

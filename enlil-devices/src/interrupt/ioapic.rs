//! Virtual I/O APIC emulation.
//!
//! Implements the Intel 82093AA I/O APIC with 24 redirection table entries.
//! Accessible via MMIO at `0xFEC00000`.

use super::DeliveryMode;

/// I/O APIC base address.
pub const IOAPIC_BASE: u64 = 0xFEC0_0000;

/// I/O APIC register select (`IOREGSEL`) offset.
const IOREGSEL: u64 = 0x00;
/// I/O APIC data window (`IOWIN`) offset.
const IOWIN: u64 = 0x10;

/// I/O APIC register indices.
const IOAPIC_REG_ID: u8 = 0x00;
const IOAPIC_REG_VER: u8 = 0x01;
const IOAPIC_REG_ARB: u8 = 0x02;
const IOAPIC_REG_REDTBL_BASE: u8 = 0x10;

/// Number of redirection table entries.
pub const NUM_IOAPIC_PINS: usize = 24;

/// I/O APIC version (simulating 82093AA).
const IOAPIC_VERSION: u32 = 0x11;

/// Destination-mode, delivery-status, and polarity bits of a redirection entry.
#[derive(Debug, Clone, Copy, Default)]
pub struct RteFlags {
    /// Destination mode: `false` = physical, `true` = logical.
    pub dest_logical: bool,
    /// Delivery status (read-only): `false` = idle, `true` = send pending.
    pub delivery_pending: bool,
    /// Polarity: `false` = active high, `true` = active low.
    pub active_low: bool,
}

/// Redirection Table Entry.
#[derive(Debug, Clone, Copy)]
pub struct RedirectionEntry {
    /// Interrupt vector (0-255).
    pub vector: u8,
    /// Delivery mode.
    pub delivery_mode: DeliveryMode,
    /// Destination-mode / status / polarity flags.
    pub flags: RteFlags,
    /// Remote IRR (for level-triggered, read-only).
    pub remote_irr: bool,
    /// Trigger mode: `false` = edge, `true` = level.
    pub level_triggered: bool,
    /// Mask: `true` = masked (disabled).
    pub masked: bool,
    /// Destination field (APIC ID or logical destination).
    pub destination: u8,
}

impl Default for RedirectionEntry {
    fn default() -> Self {
        Self {
            vector: 0,
            delivery_mode: DeliveryMode::Fixed,
            flags: RteFlags {
                dest_logical: false,
                delivery_pending: false,
                active_low: false,
            },
            remote_irr: false,
            level_triggered: false,
            masked: true, // All entries start masked
            destination: 0,
        }
    }
}

impl RedirectionEntry {
    /// Encode the low 32 bits of the RTE.
    #[must_use]
    pub const fn low(&self) -> u32 {
        let mut val = self.vector as u32;
        val |= (self.delivery_mode as u32 & 0x7) << 8;
        if self.flags.dest_logical {
            val |= 1 << 11;
        }
        if self.flags.delivery_pending {
            val |= 1 << 12;
        }
        if self.flags.active_low {
            val |= 1 << 13;
        }
        if self.remote_irr {
            val |= 1 << 14;
        }
        if self.level_triggered {
            val |= 1 << 15;
        }
        if self.masked {
            val |= 1 << 16;
        }
        val
    }

    /// Encode the high 32 bits of the RTE.
    #[must_use]
    pub const fn high(&self) -> u32 {
        (self.destination as u32) << 24
    }

    /// Decode low 32 bits into the RTE fields.
    pub const fn set_low(&mut self, val: u32) {
        self.vector = (val & 0xFF) as u8;
        self.delivery_mode = DeliveryMode::from_bits(((val >> 8) & 0x7) as u8);
        self.flags.dest_logical = (val >> 11) & 1 != 0;
        // delivery_pending is read-only
        self.flags.active_low = (val >> 13) & 1 != 0;
        // remote_irr is read-only
        self.level_triggered = (val >> 15) & 1 != 0;
        self.masked = (val >> 16) & 1 != 0;
    }

    /// Decode high 32 bits.
    pub const fn set_high(&mut self, val: u32) {
        self.destination = ((val >> 24) & 0xFF) as u8;
    }

    // --- Accessor methods used by controller.rs ---

    /// Whether this entry is masked.
    #[must_use]
    pub const fn masked(&self) -> bool {
        self.masked
    }

    /// Get the interrupt vector.
    #[must_use]
    pub const fn vector(&self) -> u8 {
        self.vector
    }

    /// Get the delivery mode as a `u8` value.
    #[must_use]
    pub const fn delivery_mode(&self) -> u8 {
        self.delivery_mode as u8
    }

    /// Get the destination field.
    #[must_use]
    pub const fn destination(&self) -> u8 {
        self.destination
    }

    /// Whether destination mode is logical.
    #[must_use]
    pub const fn destination_mode_logical(&self) -> bool {
        self.flags.dest_logical
    }

    /// Set the interrupt vector.
    pub const fn set_vector(&mut self, vector: u8) {
        self.vector = vector;
    }

    /// Set the delivery mode from a `u8` value.
    pub const fn set_delivery_mode(&mut self, mode: u8) {
        self.delivery_mode = DeliveryMode::from_bits(mode);
    }

    /// Set the destination field.
    pub const fn set_destination(&mut self, dest: u8) {
        self.destination = dest;
    }

    /// Set the destination mode to logical.
    pub const fn set_destination_mode_logical(&mut self, logical: bool) {
        self.flags.dest_logical = logical;
    }

    /// Set the mask bit.
    pub const fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }
}

/// Virtual I/O APIC.
pub struct IoApic {
    /// APIC ID.
    id: u8,
    /// Currently selected register (via `IOREGSEL`).
    reg_select: u8,
    /// Redirection table entries.
    entries: [RedirectionEntry; NUM_IOAPIC_PINS],
    /// IRQ lines state (for level-triggered).
    irq_level: [bool; NUM_IOAPIC_PINS],
}

impl IoApic {
    /// Create a new I/O APIC with the given ID.
    #[must_use]
    pub const fn new(id: u8) -> Self {
        Self {
            id,
            reg_select: 0,
            entries: [RedirectionEntry {
                vector: 0,
                delivery_mode: DeliveryMode::Fixed,
                flags: RteFlags {
                    dest_logical: false,
                    delivery_pending: false,
                    active_low: false,
                },
                remote_irr: false,
                level_triggered: false,
                masked: true,
                destination: 0,
            }; NUM_IOAPIC_PINS],
            irq_level: [false; NUM_IOAPIC_PINS],
        }
    }

    /// Handle MMIO read at the given offset from `IOAPIC_BASE`.
    #[must_use]
    pub fn mmio_read(&self, offset: u64) -> u32 {
        match offset {
            IOREGSEL => u32::from(self.reg_select),
            IOWIN => self.read_register(self.reg_select),
            _ => 0,
        }
    }

    /// Handle MMIO write at the given offset from `IOAPIC_BASE`.
    pub fn mmio_write(&mut self, offset: u64, value: u32) {
        match offset {
            IOREGSEL => self.reg_select = (value & 0xFF) as u8,
            IOWIN => self.write_register(self.reg_select, value),
            _ => {}
        }
    }

    #[must_use]
    fn read_register(&self, reg: u8) -> u32 {
        match reg {
            IOAPIC_REG_ID | IOAPIC_REG_ARB => u32::from(self.id) << 24,
            IOAPIC_REG_VER => {
                IOAPIC_VERSION | (u32::from(u8::try_from(NUM_IOAPIC_PINS - 1).unwrap_or(0)) << 16)
            }
            r if r >= IOAPIC_REG_REDTBL_BASE => {
                let index = usize::from(r - IOAPIC_REG_REDTBL_BASE);
                let entry_idx = index / 2;
                if entry_idx >= NUM_IOAPIC_PINS {
                    return 0;
                }
                if index.is_multiple_of(2) {
                    self.entries[entry_idx].low()
                } else {
                    self.entries[entry_idx].high()
                }
            }
            _ => 0,
        }
    }

    fn write_register(&mut self, reg: u8, value: u32) {
        match reg {
            IOAPIC_REG_ID => self.id = ((value >> 24) & 0xF) as u8,
            r if r >= IOAPIC_REG_REDTBL_BASE => {
                let index = usize::from(r - IOAPIC_REG_REDTBL_BASE);
                let entry_idx = index / 2;
                if entry_idx >= NUM_IOAPIC_PINS {
                    return;
                }
                if index.is_multiple_of(2) {
                    self.entries[entry_idx].set_low(value);
                } else {
                    self.entries[entry_idx].set_high(value);
                }
            }
            _ => {}
        }
    }

    /// Assert an IRQ line. Returns the routing info if the interrupt should be delivered.
    pub const fn set_irq(&mut self, irq: usize) -> Option<InterruptRoute> {
        if irq >= NUM_IOAPIC_PINS {
            return None;
        }

        let entry = &mut self.entries[irq];

        if entry.masked {
            return None;
        }

        if entry.level_triggered {
            if self.irq_level[irq] {
                return None; // Already asserted
            }
            self.irq_level[irq] = true;
            entry.remote_irr = true;
        }

        Some(InterruptRoute {
            vector: entry.vector,
            delivery_mode: entry.delivery_mode,
            dest_logical: entry.flags.dest_logical,
            destination: entry.destination,
            level_triggered: entry.level_triggered,
        })
    }

    /// Deassert an IRQ line (for level-triggered interrupts).
    pub const fn clear_irq(&mut self, irq: usize) {
        if irq < NUM_IOAPIC_PINS {
            self.irq_level[irq] = false;
        }
    }

    /// Handle EOI broadcast for a given vector.
    ///
    /// Clears `remote_irr` on matching level-triggered entries. For any whose
    /// input line is still asserted (and not masked), a real I/O APIC
    /// immediately re-sends the interrupt — `remote_irr` is set again and the
    /// route is returned so the caller can re-deliver it. Edge-triggered
    /// entries never set `remote_irr`, so they are untouched.
    pub fn eoi(&mut self, vector: u8) -> Vec<InterruptRoute> {
        let mut resend = Vec::new();
        for (i, entry) in self.entries.iter_mut().enumerate() {
            if entry.level_triggered && entry.remote_irr && entry.vector == vector {
                entry.remote_irr = false;
                entry.flags.delivery_pending = false;
                // Line still held high and unmasked → re-assert and re-deliver,
                // exactly as the hardware retriggers a level-sensitive line.
                if self.irq_level[i] && !entry.masked {
                    entry.remote_irr = true;
                    resend.push(InterruptRoute {
                        vector: entry.vector,
                        delivery_mode: entry.delivery_mode,
                        dest_logical: entry.flags.dest_logical,
                        destination: entry.destination,
                        level_triggered: true,
                    });
                }
            }
        }
        resend
    }

    /// Handle EOI broadcast (alias used by controller).
    pub fn eoi_broadcast(&mut self, vector: u8) -> Vec<InterruptRoute> {
        self.eoi(vector)
    }

    /// Get a copy of a redirection table entry by IRQ number.
    ///
    /// # Panics
    /// Panics if `irq >= 24`.
    #[must_use]
    pub const fn get_rte(&self, irq: u8) -> RedirectionEntry {
        self.entries[irq as usize]
    }

    /// Get a mutable reference to a redirection table entry by IRQ number.
    ///
    /// # Panics
    /// Panics if `irq >= 24`.
    pub const fn get_rte_mut(&mut self, irq: u8) -> &mut RedirectionEntry {
        &mut self.entries[irq as usize]
    }

    /// Get a reference to a redirection entry.
    #[must_use]
    pub const fn entry(&self, irq: usize) -> Option<&RedirectionEntry> {
        if irq < self.entries.len() {
            Some(&self.entries[irq])
        } else {
            None
        }
    }
}

/// Routing information for an interrupt from the I/O APIC.
#[derive(Debug, Clone)]
pub struct InterruptRoute {
    pub vector: u8,
    pub delivery_mode: DeliveryMode,
    pub dest_logical: bool,
    pub destination: u8,
    pub level_triggered: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::truncate::u32_of;

    #[test]
    fn ioapic_default_entries_masked() {
        let ioapic = IoApic::new(0);
        for i in 0..NUM_IOAPIC_PINS {
            assert!(ioapic.entries[i].masked);
        }
    }

    #[test]
    fn ioapic_read_version() {
        let mut ioapic = IoApic::new(0);
        ioapic.mmio_write(IOREGSEL, u32::from(IOAPIC_REG_VER));
        let ver = ioapic.mmio_read(IOWIN);
        assert_eq!(ver & 0xFF, IOAPIC_VERSION);
        {
            assert_eq!((ver >> 16) & 0xFF, u32_of(NUM_IOAPIC_PINS - 1));
        }
    }

    #[test]
    fn ioapic_write_read_rte() {
        let mut ioapic = IoApic::new(0);

        // Write low word of RTE 0: vector=0x30, delivery=Fixed, unmasked
        let rte_reg = IOAPIC_REG_REDTBL_BASE;
        ioapic.mmio_write(IOREGSEL, u32::from(rte_reg));
        ioapic.mmio_write(IOWIN, 0x0000_0030); // vector 0x30, unmasked

        // Write high word: destination APIC ID = 1
        ioapic.mmio_write(IOREGSEL, u32::from(rte_reg + 1));
        ioapic.mmio_write(IOWIN, 0x0100_0000); // dest = 1

        // Read back
        ioapic.mmio_write(IOREGSEL, u32::from(rte_reg));
        let low = ioapic.mmio_read(IOWIN);
        assert_eq!(low & 0xFF, 0x30);
        assert_eq!((low >> 16) & 1, 0); // unmasked

        ioapic.mmio_write(IOREGSEL, u32::from(rte_reg + 1));
        let high = ioapic.mmio_read(IOWIN);
        assert_eq!((high >> 24) & 0xFF, 1);
    }

    #[test]
    fn ioapic_masked_irq_not_delivered() {
        let mut ioapic = IoApic::new(0);
        // IRQ 0 is masked by default
        assert!(ioapic.set_irq(0).is_none());
    }

    #[test]
    fn ioapic_unmasked_irq_delivered() {
        let mut ioapic = IoApic::new(0);
        ioapic.entries[5].masked = false;
        ioapic.entries[5].vector = 0x40;
        ioapic.entries[5].destination = 0;

        let route = ioapic.set_irq(5);
        assert!(route.is_some());
        let route = route.unwrap();
        assert_eq!(route.vector, 0x40);
    }

    #[test]
    fn ioapic_level_triggered_eoi() {
        let mut ioapic = IoApic::new(0);
        ioapic.entries[3].masked = false;
        ioapic.entries[3].vector = 0x33;
        ioapic.entries[3].level_triggered = true;

        // First assertion delivers
        assert!(ioapic.set_irq(3).is_some());
        assert!(ioapic.entries[3].remote_irr);

        // Second assertion while remote_irr set does NOT deliver
        assert!(ioapic.set_irq(3).is_none());

        // EOI with the line STILL asserted re-sends: remote_irr is set again
        // and the caller gets a route to re-deliver.
        let resend = ioapic.eoi(0x33);
        assert_eq!(resend.len(), 1);
        assert_eq!(resend[0].vector, 0x33);
        assert!(resend[0].level_triggered);
        assert!(ioapic.entries[3].remote_irr);

        // Once the device deasserts the line, EOI clears remote_irr and does
        // NOT re-send.
        ioapic.clear_irq(3);
        let resend = ioapic.eoi(0x33);
        assert!(resend.is_empty());
        assert!(!ioapic.entries[3].remote_irr);
    }

    #[test]
    fn ioapic_eoi_does_not_resend_a_masked_line() {
        // A guest may mask the RTE before EOI; a masked line must not re-send
        // even while the input is asserted.
        let mut ioapic = IoApic::new(0);
        ioapic.entries[7].masked = false;
        ioapic.entries[7].vector = 0x44;
        ioapic.entries[7].level_triggered = true;
        assert!(ioapic.set_irq(7).is_some());

        ioapic.entries[7].masked = true;
        let resend = ioapic.eoi(0x44);
        assert!(resend.is_empty());
        assert!(!ioapic.entries[7].remote_irr);
    }

    #[test]
    fn ioapic_base_address() {
        assert_eq!(IOAPIC_BASE, 0xFEC0_0000);
    }
}

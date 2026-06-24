//! Unified interrupt controller — coordinates LAPIC, IOAPIC, and MSI delivery.

use super::ioapic::IoApic;
use super::lapic::LocalApic;
use super::msi::MsiMessage;
use super::{DeliveryMode, InterruptEntry, TriggerMode};

/// Unified interrupt controller managing all interrupt sources and destinations.
pub struct InterruptController {
    /// Per-vCPU local APICs.
    pub lapics: Vec<LocalApic>,
    /// I/O APIC.
    pub ioapic: IoApic,
}

impl InterruptController {
    /// Create a new interrupt controller with the given number of vCPUs.
    #[must_use]
    pub fn new(num_vcpus: u8) -> Self {
        let lapics = (0..num_vcpus).map(LocalApic::new).collect();
        Self {
            lapics,
            ioapic: IoApic::new(0),
        }
    }

    /// Deliver an IOAPIC interrupt (from a device IRQ line).
    pub fn deliver_irq(&mut self, irq: u8) {
        if let Some(route) = self.ioapic.set_irq(irq as usize) {
            let entry = InterruptEntry {
                vector: route.vector,
                delivery_mode: route.delivery_mode,
                trigger_mode: if route.level_triggered {
                    TriggerMode::Level
                } else {
                    TriggerMode::Edge
                },
                level: true,
            };

            if route.dest_logical {
                self.deliver_logical(route.destination, entry);
            } else {
                self.deliver_physical(route.destination, entry);
            }
        }
    }

    /// Deassert a device IRQ line (for level-triggered routing).
    ///
    /// Edge-triggered ISA lines (the legacy default) latch on assertion and do
    /// not need a deassert, so this is a no-op for them; for a level-triggered
    /// RTE it clears the I/O APIC's tracked line state so a later re-assertion
    /// delivers again.
    pub const fn clear_irq(&mut self, irq: u8) {
        self.ioapic.clear_irq(irq as usize);
    }

    /// Deliver an MSI/MSI-X interrupt.
    pub fn deliver_msi(&mut self, msg: &MsiMessage) {
        let vector = msg.vector();
        let dest_id = msg.destination_id();
        let logical = msg.destination_mode_logical();
        let dm = msg.delivery_mode;

        let entry = InterruptEntry {
            vector,
            delivery_mode: dm,
            trigger_mode: TriggerMode::Edge,
            level: true,
        };

        if logical {
            self.deliver_logical(dest_id, entry);
        } else {
            self.deliver_physical(dest_id, entry);
        }
    }

    /// Deliver to a specific LAPIC by physical APIC ID.
    fn deliver_physical(&mut self, dest: u8, entry: InterruptEntry) {
        match entry.delivery_mode {
            DeliveryMode::LowestPriority => {
                // Find the LAPIC with the lowest TPR among matching destinations
                let target = self
                    .lapics
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.id() == dest && l.is_enabled())
                    .min_by_key(|(_, l)| l.get_tpr());
                if let Some((idx, _)) = target {
                    let _ = self.lapics[idx].accept_interrupt(&entry);
                }
            }
            _ => {
                if let Some(lapic) = self.lapics.iter_mut().find(|l| l.id() == dest) {
                    let _ = lapic.accept_interrupt(&entry);
                }
            }
        }
    }

    /// Deliver to the LAPICs that are members of the logical destination `dest`
    /// (matched via [`LocalApic::matches_logical`] against each LAPIC's
    /// `DFR`/`LDR`), not every enabled LAPIC. For Fixed/NMI/etc. every matching
    /// member accepts; for Lowest-Priority only the matching member with the
    /// lowest TPR does.
    fn deliver_logical(&mut self, dest: u8, entry: InterruptEntry) {
        match entry.delivery_mode {
            DeliveryMode::LowestPriority => {
                let target = self
                    .lapics
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.is_enabled() && l.matches_logical(dest))
                    .min_by_key(|(_, l)| l.get_tpr());
                if let Some((idx, _)) = target {
                    let _ = self.lapics[idx].accept_interrupt(&entry);
                }
            }
            _ => {
                for lapic in &mut self.lapics {
                    if lapic.is_enabled() && lapic.matches_logical(dest) {
                        let _ = lapic.accept_interrupt(&entry);
                    }
                }
            }
        }
    }

    /// Signal EOI from a vCPU.
    pub fn eoi(&mut self, vcpu_id: u8, vector: u8) {
        if let Some(lapic) = self.lapics.iter_mut().find(|l| l.id() == vcpu_id) {
            lapic.signal_eoi();
        }
        self.ioapic.eoi_broadcast(vector);
    }

    /// Check if a vCPU has a pending interrupt.
    #[must_use]
    pub fn has_pending(&self, vcpu_id: u8) -> bool {
        self.lapics
            .iter()
            .find(|l| l.id() == vcpu_id)
            .is_some_and(LocalApic::has_pending_interrupt)
    }

    /// Get the pending interrupt vector for a vCPU.
    #[must_use]
    pub fn pending_vector(&self, vcpu_id: u8) -> Option<u8> {
        self.lapics
            .iter()
            .find(|l| l.id() == vcpu_id)
            .and_then(LocalApic::pending_vector)
    }

    /// Get a mutable reference to a LAPIC by vCPU ID.
    pub fn lapic_mut(&mut self, vcpu_id: u8) -> Option<&mut LocalApic> {
        self.lapics.iter_mut().find(|l| l.id() == vcpu_id)
    }

    /// Get a reference to a LAPIC by vCPU ID.
    #[must_use]
    pub fn lapic(&self, vcpu_id: u8) -> Option<&LocalApic> {
        self.lapics.iter().find(|l| l.id() == vcpu_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::lapic::{LAPIC_DFR, LAPIC_LDR, LAPIC_SVR};
    use super::*;

    fn make_controller(n: u8) -> InterruptController {
        let mut ctrl = InterruptController::new(n);
        for lapic in &mut ctrl.lapics {
            lapic.write_register(LAPIC_SVR, 0x1FF);
        }
        ctrl
    }

    #[test]
    fn test_create_controller() {
        let ctrl = InterruptController::new(4);
        assert_eq!(ctrl.lapics.len(), 4);
    }

    #[test]
    fn test_deliver_msi() {
        let mut ctrl = make_controller(2);
        let msg = MsiMessage {
            address: 0xFEE0_0000, // dest 0, physical
            data: 0x30,           // vector 0x30, fixed delivery
            delivery_mode: DeliveryMode::Fixed,
        };
        ctrl.deliver_msi(&msg);
        assert!(ctrl.has_pending(0));
        assert_eq!(ctrl.pending_vector(0), Some(0x30));
    }

    #[test]
    fn logical_msi_targets_only_matching_lapics() {
        let mut ctrl = make_controller(4);
        // Flat model; LAPIC 1 owns logical bit 0x02, LAPIC 2 owns 0x04.
        ctrl.lapics[1].write_register(LAPIC_DFR, 0xFFFF_FFFF);
        ctrl.lapics[1].write_register(LAPIC_LDR, 0x02 << 24);
        ctrl.lapics[2].write_register(LAPIC_DFR, 0xFFFF_FFFF);
        ctrl.lapics[2].write_register(LAPIC_LDR, 0x04 << 24);

        // Logical MSI to destination bitmask 0x02 (address bit 2 set), vector 0x33.
        ctrl.deliver_msi(&MsiMessage::new(0xFEE0_2004, 0x33));
        assert!(ctrl.has_pending(1), "LAPIC 1 (logical 0x02) is targeted");
        assert!(!ctrl.has_pending(2), "LAPIC 2 (logical 0x04) is not");
        assert!(!ctrl.has_pending(0), "unprogrammed LAPIC 0 is not");
        assert_eq!(ctrl.pending_vector(1), Some(0x33));
    }

    #[test]
    fn logical_broadcast_reaches_every_member_but_unprogrammed_lapics_are_dropped() {
        let mut ctrl = make_controller(3);
        ctrl.lapics[0].write_register(LAPIC_DFR, 0xFFFF_FFFF);
        ctrl.lapics[0].write_register(LAPIC_LDR, 0x01 << 24);
        ctrl.lapics[1].write_register(LAPIC_DFR, 0xFFFF_FFFF);
        ctrl.lapics[1].write_register(LAPIC_LDR, 0x02 << 24);
        // LAPIC 2 left at LDR=0 (reset): a logical interrupt must not reach it.

        // Destination 0xFF (all bits) reaches every LAPIC with a programmed LDR.
        ctrl.deliver_msi(&MsiMessage::new(0xFEE0_FF04, 0x44));
        assert!(ctrl.has_pending(0));
        assert!(ctrl.has_pending(1));
        assert!(!ctrl.has_pending(2), "LAPIC with LDR=0 matches nothing");
    }

    #[test]
    fn test_eoi() {
        let mut ctrl = make_controller(1);
        let msg = MsiMessage {
            address: 0xFEE0_0000,
            data: 0x40,
            delivery_mode: DeliveryMode::Fixed,
        };
        ctrl.deliver_msi(&msg);
        assert!(ctrl.has_pending(0));
        // Service the interrupt
        ctrl.lapics[0].start_servicing(0x40);
        // EOI
        ctrl.eoi(0, 0x40);
        assert!(!ctrl.has_pending(0));
    }
}

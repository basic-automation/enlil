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

    /// Deliver to LAPICs matching logical destination.
    fn deliver_logical(&mut self, _dest: u8, entry: InterruptEntry) {
        match entry.delivery_mode {
            DeliveryMode::LowestPriority => {
                let target = self
                    .lapics
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.is_enabled())
                    .min_by_key(|(_, l)| l.get_tpr());
                if let Some((idx, _)) = target {
                    let _ = self.lapics[idx].accept_interrupt(&entry);
                }
            }
            _ => {
                for lapic in &mut self.lapics {
                    if lapic.is_enabled() {
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
    use super::super::lapic::LAPIC_SVR;
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

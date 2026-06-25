//! Unified interrupt controller — coordinates LAPIC, IOAPIC, and MSI delivery.

use super::ioapic::{InterruptRoute, IoApic};
use super::lapic::{LAPIC_EOI, LAPIC_ICR_LOW, LocalApic};
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
            self.deliver_route(route);
        }
    }

    /// Resolve an I/O APIC [`InterruptRoute`] to the addressed LAPIC(s).
    fn deliver_route(&mut self, route: InterruptRoute) {
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

    /// Apply a guest LAPIC register write for `vcpu_id`, dispatching the side
    /// effects a bare register store cannot express.
    ///
    /// This is the entry point a LAPIC MMIO / x2APIC front-end uses for guest
    /// writes: it stores the register, and when the guest writes `ICR_LOW`
    /// (the trigger on real hardware, after `ICR_HIGH` is set up) it dispatches
    /// the programmed IPI via [`send_ipi`](Self::send_ipi). Unknown vCPU IDs
    /// are ignored.
    pub fn write_lapic(&mut self, vcpu_id: u8, offset: u32, value: u32) {
        // An EOI register write retires the highest in-service vector and must
        // also broadcast to the I/O APIC (clearing remote_IRR / retriggering a
        // still-asserted level line), so route it through the full EOI path
        // rather than the bare register store.
        if offset == LAPIC_EOI {
            if let Some(vector) = self.lapic(vcpu_id).and_then(LocalApic::in_service_vector) {
                self.eoi(vcpu_id, vector);
            }
            return;
        }

        let trigger = {
            let Some(lapic) = self.lapic_mut(vcpu_id) else {
                return;
            };
            lapic.write_register(offset, value);
            (offset == LAPIC_ICR_LOW).then(|| (lapic.id(), lapic.icr()))
        };
        if let Some((source, icr)) = trigger {
            self.send_ipi(source, icr);
        }
    }

    /// Deliver an inter-processor interrupt programmed into a source LAPIC's
    /// Interrupt Command Register.
    ///
    /// Decodes the 64-bit ICR (Intel SDM Vol.3 §10.6.1): vector, delivery
    /// mode, destination mode, and the destination shorthand — `00` use the
    /// destination field, `01` self, `10` all-including-self, `11`
    /// all-excluding-self. Only the *vectored* delivery modes (Fixed, Lowest
    /// Priority) are injected into the target LAPIC(s)' IRR; SMI/NMI/INIT/SIPI
    /// drive vCPU-state transitions (SMM entry, AP bring-up) that this software
    /// LAPIC model does not represent as IRR vectors, so they are decoded but
    /// not injected here. `source_id` is the APIC ID of the LAPIC that wrote
    /// the ICR (needed for the self / all-excluding-self shorthands).
    pub fn send_ipi(&mut self, source_id: u8, icr: u64) {
        let delivery_mode = DeliveryMode::from_bits(((icr >> 8) & 0x7) as u8);
        if !matches!(
            delivery_mode,
            DeliveryMode::Fixed | DeliveryMode::LowestPriority
        ) {
            return;
        }

        let entry = InterruptEntry {
            vector: (icr & 0xFF) as u8,
            delivery_mode,
            trigger_mode: TriggerMode::Edge,
            level: true,
        };
        let dest_logical = (icr >> 11) & 1 != 0;
        let shorthand = (icr >> 18) & 0x3;
        let dest = ((icr >> 56) & 0xFF) as u8;

        match shorthand {
            0b01 => {
                // Self IPI.
                if let Some(lapic) = self.lapics.iter_mut().find(|l| l.id() == source_id) {
                    let _ = lapic.accept_interrupt(&entry);
                }
            }
            // All including self — identical to a physical broadcast (0xFF),
            // which also honours lowest-priority arbitration.
            0b10 => self.deliver_physical(0xFF, entry),
            0b11 => {
                // All excluding self.
                if delivery_mode == DeliveryMode::LowestPriority {
                    let target = self
                        .lapics
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| l.id() != source_id && l.is_enabled())
                        .min_by_key(|(_, l)| l.get_tpr());
                    if let Some((idx, _)) = target {
                        let _ = self.lapics[idx].accept_interrupt(&entry);
                    }
                } else {
                    for lapic in &mut self.lapics {
                        if lapic.id() != source_id {
                            let _ = lapic.accept_interrupt(&entry);
                        }
                    }
                }
            }
            // No shorthand: route by the destination field + mode.
            _ if dest_logical => self.deliver_logical(dest, entry),
            _ => self.deliver_physical(dest, entry),
        }
    }

    /// Deliver to a specific LAPIC by physical APIC ID.
    fn deliver_physical(&mut self, dest: u8, entry: InterruptEntry) {
        // In physical destination mode, APIC ID 0xFF is the broadcast shorthand
        // (Intel SDM Vol.3 §10.6.2.1) — it addresses every LAPIC.
        let broadcast = dest == 0xFF;
        match entry.delivery_mode {
            DeliveryMode::LowestPriority => {
                // Find the LAPIC with the lowest TPR among matching destinations
                let target = self
                    .lapics
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| (broadcast || l.id() == dest) && l.is_enabled())
                    .min_by_key(|(_, l)| l.get_tpr());
                if let Some((idx, _)) = target {
                    let _ = self.lapics[idx].accept_interrupt(&entry);
                }
            }
            _ => {
                for lapic in &mut self.lapics {
                    if broadcast || lapic.id() == dest {
                        let _ = lapic.accept_interrupt(&entry);
                    }
                }
            }
        }
    }

    /// Deliver to LAPICs matching the logical destination (MDA).
    ///
    /// Each LAPIC decides membership from its own `LDR`/`DFR`
    /// ([`LocalApic::matches_logical`]); only enabled, addressed LAPICs are
    /// considered. For lowest-priority delivery the one with the lowest TPR
    /// among the matching set wins; otherwise every matching LAPIC accepts.
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
    ///
    /// Clears the LAPIC's in-service bit, then broadcasts the EOI to the I/O
    /// APIC. Any level-triggered RTE whose input line is still asserted is
    /// re-sent immediately (the standard level-triggered retrigger-after-EOI
    /// path), so a held PCI INTx line keeps interrupting until the device
    /// deasserts it.
    pub fn eoi(&mut self, vcpu_id: u8, vector: u8) {
        if let Some(lapic) = self.lapics.iter_mut().find(|l| l.id() == vcpu_id) {
            lapic.signal_eoi();
        }
        for route in self.ioapic.eoi_broadcast(vector) {
            self.deliver_route(route);
        }
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
    use super::super::lapic::{LAPIC_DFR, LAPIC_ICR_HIGH, LAPIC_ICR_LOW, LAPIC_LDR, LAPIC_SVR};
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
    fn test_deliver_msi_physical_broadcast() {
        // Physical-mode destination 0xFF is the broadcast shorthand: every
        // enabled LAPIC receives the interrupt.
        let mut ctrl = make_controller(3);
        ctrl.lapics[0].write_register(super::super::lapic::LAPIC_ID, 0u32);
        ctrl.lapics[1].write_register(super::super::lapic::LAPIC_ID, 1 << 24);
        ctrl.lapics[2].write_register(super::super::lapic::LAPIC_ID, 2 << 24);
        let msg = MsiMessage {
            address: 0xFEEF_F000, // physical (bit 2 clear), dest id 0xFF
            data: 0x80,
            delivery_mode: DeliveryMode::Fixed,
        };
        assert!(!msg.destination_mode_logical());
        assert_eq!(msg.destination_id(), 0xFF);
        ctrl.deliver_msi(&msg);
        assert_eq!(ctrl.pending_vector(0), Some(0x80));
        assert_eq!(ctrl.pending_vector(1), Some(0x80));
        assert_eq!(ctrl.pending_vector(2), Some(0x80));
    }

    #[test]
    fn test_deliver_msi_logical_flat_targets_only_addressed_lapics() {
        // Flat model (default DFR): give LAPIC 0 logical id bit 0, LAPIC 1 bit 1.
        let mut ctrl = make_controller(2);
        ctrl.lapics[0].write_register(LAPIC_LDR, 0x0100_0000);
        ctrl.lapics[1].write_register(LAPIC_LDR, 0x0200_0000);

        // Logical MSI (address bit 2 set) to logical id 0x02 — only LAPIC 1.
        let msg = MsiMessage {
            address: 0xFEE0_2004,
            data: 0x50,
            delivery_mode: DeliveryMode::Fixed,
        };
        ctrl.deliver_msi(&msg);
        assert!(!ctrl.has_pending(0), "LAPIC 0 not in the logical destination");
        assert_eq!(ctrl.pending_vector(1), Some(0x50));
    }

    #[test]
    fn test_deliver_msi_logical_flat_multicast() {
        // A logical MDA with two bits set fans out to both members (Fixed).
        let mut ctrl = make_controller(2);
        ctrl.lapics[0].write_register(LAPIC_LDR, 0x0100_0000);
        ctrl.lapics[1].write_register(LAPIC_LDR, 0x0200_0000);
        let msg = MsiMessage {
            address: 0xFEE0_3004, // logical, MDA = 0x03
            data: 0x61,
            delivery_mode: DeliveryMode::Fixed,
        };
        ctrl.deliver_msi(&msg);
        assert_eq!(ctrl.pending_vector(0), Some(0x61));
        assert_eq!(ctrl.pending_vector(1), Some(0x61));
    }

    #[test]
    fn test_deliver_msi_logical_lowest_priority_picks_lowest_tpr() {
        // Both LAPICs are addressed; lowest-priority delivery picks the one
        // with the lower TPR (LAPIC 1 here).
        let mut ctrl = make_controller(2);
        ctrl.lapics[0].write_register(LAPIC_LDR, 0x0100_0000);
        ctrl.lapics[1].write_register(LAPIC_LDR, 0x0200_0000);
        ctrl.lapics[0].set_tpr(0x40);
        ctrl.lapics[1].set_tpr(0x10);
        let msg = MsiMessage {
            address: 0xFEE0_3004, // logical, MDA = 0x03 (both members)
            data: 0x71,
            delivery_mode: DeliveryMode::LowestPriority,
        };
        ctrl.deliver_msi(&msg);
        assert!(!ctrl.has_pending(0));
        assert_eq!(ctrl.pending_vector(1), Some(0x71));
    }

    #[test]
    fn test_send_ipi_directed_physical() {
        // vCPU 0 sends a Fixed IPI directed at APIC ID 1 (no shorthand).
        let mut ctrl = make_controller(2);
        ctrl.write_lapic(0, LAPIC_ICR_HIGH, 1 << 24); // destination = 1
        ctrl.write_lapic(0, LAPIC_ICR_LOW, 0x90); // vector 0x90, Fixed, physical
        assert!(!ctrl.has_pending(0));
        assert_eq!(ctrl.pending_vector(1), Some(0x90));
    }

    #[test]
    fn test_send_ipi_self_shorthand() {
        let mut ctrl = make_controller(2);
        // Shorthand 0b01 = self.
        ctrl.write_lapic(0, LAPIC_ICR_LOW, 0x91 | (0b01 << 18));
        assert_eq!(ctrl.pending_vector(0), Some(0x91));
        assert!(!ctrl.has_pending(1));
    }

    #[test]
    fn test_send_ipi_all_excluding_self() {
        let mut ctrl = make_controller(3);
        // Shorthand 0b11 = all-excluding-self, from source 0.
        ctrl.write_lapic(0, LAPIC_ICR_LOW, 0x92 | (0b11 << 18));
        assert!(!ctrl.has_pending(0));
        assert_eq!(ctrl.pending_vector(1), Some(0x92));
        assert_eq!(ctrl.pending_vector(2), Some(0x92));
    }

    #[test]
    fn test_send_ipi_all_including_self() {
        let mut ctrl = make_controller(2);
        // Shorthand 0b10 = all-including-self.
        ctrl.write_lapic(0, LAPIC_ICR_LOW, 0x93 | (0b10 << 18));
        assert_eq!(ctrl.pending_vector(0), Some(0x93));
        assert_eq!(ctrl.pending_vector(1), Some(0x93));
    }

    #[test]
    fn test_send_ipi_init_is_decoded_but_not_injected() {
        // INIT (delivery mode 5) drives AP state, not the IRR — even with a
        // valid vector field it must not be injected here.
        let mut ctrl = make_controller(2);
        ctrl.write_lapic(0, LAPIC_ICR_HIGH, 1 << 24);
        ctrl.write_lapic(0, LAPIC_ICR_LOW, 0x90 | (5 << 8)); // INIT
        assert!(!ctrl.has_pending(1));
        // The ICR value is still stored (read side intact).
        assert_eq!(ctrl.lapic(0).unwrap().icr() & 0xFF, 0x90);
    }

    #[test]
    fn test_eoi_retriggers_held_level_line() {
        let mut ctrl = make_controller(1);
        // Program I/O APIC pin 9 as a level-triggered RTE: vector 0x55, dest 0.
        {
            let rte = ctrl.ioapic.get_rte_mut(9);
            rte.set_masked(false);
            rte.set_vector(0x55);
            rte.level_triggered = true;
            rte.set_destination(0);
        }
        // Device asserts the level line.
        ctrl.deliver_irq(9);
        assert_eq!(ctrl.pending_vector(0), Some(0x55));
        // vCPU services it.
        ctrl.lapics[0].start_servicing(0x55);
        assert!(!ctrl.has_pending(0));
        // EOI with the line STILL asserted re-delivers the interrupt.
        ctrl.eoi(0, 0x55);
        assert_eq!(ctrl.pending_vector(0), Some(0x55));
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
    fn level_triggered_line_redelivers_on_eoi_until_deasserted() {
        let mut ctrl = make_controller(1);
        // Program IRQ 7 as a level-triggered RTE to LAPIC 0, vector 0x50.
        {
            let rte = ctrl.ioapic.get_rte_mut(7);
            rte.set_vector(0x50);
            rte.set_destination(0);
            rte.set_masked(false);
            rte.level_triggered = true;
        }

        // Device asserts the line → delivered to LAPIC 0.
        ctrl.deliver_irq(7);
        assert_eq!(ctrl.pending_vector(0), Some(0x50));

        // Guest services and EOIs while the line is STILL asserted: the
        // level-triggered interrupt re-arms and is delivered again.
        ctrl.lapics[0].start_servicing(0x50);
        ctrl.eoi(0, 0x50);
        assert_eq!(
            ctrl.pending_vector(0),
            Some(0x50),
            "still-asserted level line re-fires on EOI"
        );

        // The ISR cleared the device condition (line deasserts); now service +
        // EOI leaves nothing pending.
        ctrl.clear_irq(7);
        ctrl.lapics[0].start_servicing(0x50);
        ctrl.eoi(0, 0x50);
        assert!(
            !ctrl.has_pending(0),
            "a deasserted line does not re-fire on EOI"
        );
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

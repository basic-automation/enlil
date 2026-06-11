//! The PIIX3/ICH9 PCI interrupt router (the "PIRQ" router).
//!
//! The four `PIRQ[A-D]_ROUT` routing registers and their bit layout are identical
//! on the legacy PIIX3 (`8086:7000`, at `00:01.0`) and the PCIe-era ICH9 LPC
//! bridge (`8086:2918`, at `00:1F.0`); Enlil mounts the ICH9 LPC bridge, so the
//! register addresses below are config `0x60`..`0x63` on that bridge.
//!
//! PCI devices signal interrupts on one of four level-triggered pins —
//! `INTA#`..`INTD#` (encoded 1..4 in config-space register `0x3D`,
//! [`INTERRUPT_PIN`](crate::pcie::cfg::INTERRUPT_PIN)). The south-bridge routes
//! those pins to legacy 8259 IRQs in **two** ways, depending on the interrupt
//! mode the OS has chosen:
//!
//! - **PIC mode:** each device's pin is *swizzled* by slot number onto one of
//!   four router lines `PIRQ[A..D]`, and four chipset registers (PIIX3 config
//!   offsets `0x60`..`0x63`) program which ISA IRQ each `PIRQ` line drives. PCI
//!   interrupts are **level-triggered**, so the target IRQ must be set to level
//!   in the [ELCR](crate::interrupt::pic) — which is exactly what
//!   [`Pic8259::set_line`](crate::interrupt::pic::Pic8259::set_line)'s
//!   level-mode IRR models.
//! - **APIC mode:** the same four `PIRQ` lines wire straight to I/O APIC global
//!   system interrupts **16..19** (`PIRQA`→16 .. `PIRQD`→19), bypassing the
//!   routing registers entirely.
//!
//! This module models the router: the four routing registers, the slot/pin
//! swizzle, and the resolution of a device's `(slot, pin)` to its ISA IRQ (PIC
//! mode) or GSI (APIC mode). Driving the resolved line into a live controller is
//! left to the caller, which owns the [`SharedPic`](crate::interrupt::SharedPic)
//! / [`SharedInterruptController`](crate::interrupt::SharedInterruptController).

/// Number of PCI interrupt-router lines: `PIRQA`, `PIRQB`, `PIRQC`, `PIRQD`.
pub const PIRQ_LINES: usize = 4;

/// First PIIX3 config-space offset of the routing registers (`PIRQRCA`); the
/// four registers are contiguous at `0x60`..=`0x63`.
pub const PIRQ_ROUTE_BASE: u16 = 0x60;

/// First I/O APIC GSI the four PIRQ lines wire to in APIC mode (`PIRQA`→16).
pub const PIRQ_GSI_BASE: u8 = 16;

/// The default ISA IRQ each PIRQ line (`A`,`B`,`C`,`D`) drives in **PIC mode**.
///
/// These are the values the firmware programs into the routing registers at
/// power-on and that the static PIC-mode `_PRT` advertises, so the table a guest
/// reads and the routing the firmware programs into `PIRQRC[A-D]` agree by
/// construction (the same discipline the APIC-mode `_PRT` already follows for
/// GSIs).
///
/// All four are PCI-routable (3-7, 9-12, 14, 15) and avoid the IRQs Enlil's
/// modeled legacy devices already own — IRQ1 (keyboard), IRQ4 (COM1), IRQ8
/// (RTC), IRQ12 (mouse) — and the ACPI SCI (IRQ9). They are distinct so the four
/// lines do not needlessly share a vector.
pub const PIRQ_DEFAULT_IRQS: [u8; PIRQ_LINES] = [11, 10, 5, 6];

/// A set routing register's bit 7 means the line is **not routed** to any IRQ
/// (the PIIX3 reset state — firmware must program a valid IRQ to enable it).
const ROUTE_DISABLED: u8 = 0x80;

/// Bits 3:0 of a routing register select the ISA IRQ.
const ROUTE_IRQ_MASK: u8 = 0x0F;

/// The PIIX3 PCI interrupt router.
#[derive(Debug, Clone)]
pub struct PirqRouter {
    /// `PIRQRC[A..D]` (config `0x60`..`0x63`): bit 7 = disabled, bits 3:0 = the
    /// ISA IRQ this line drives.
    routes: [u8; PIRQ_LINES],
}

impl Default for PirqRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl PirqRouter {
    /// A router in its PIIX3 reset state: every PIRQ line disabled (`0x80`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            routes: [ROUTE_DISABLED; PIRQ_LINES],
        }
    }

    /// The PIRQ line (`0`=A..`3`=D) a device's interrupt pin lands on, by the
    /// standard PCI swizzle: a device in slot `slot` asserting pin `pin`
    /// (`1`=INTA..`4`=INTD) routes to `PIRQ[(slot + pin - 1) mod 4]`. This is the
    /// chained "barber-pole" mapping that spreads the four pins of adjacent slots
    /// across the four router lines so they don't all collide on PIRQA.
    ///
    /// `pin` outside `1..=4` (i.e. a device that declares no interrupt) returns
    /// `None`.
    #[must_use]
    pub const fn pirq_line(slot: u8, pin: u8) -> Option<usize> {
        if pin == 0 || pin > 4 {
            return None;
        }
        Some(((slot as usize) + (pin as usize) - 1) % PIRQ_LINES)
    }

    /// Program one routing register (`pirq` `0`=A..`3`=D) — the effect of a guest
    /// writing PIIX3 config offset `0x60 + pirq`.
    pub const fn set_route(&mut self, pirq: usize, value: u8) {
        self.routes[pirq % PIRQ_LINES] = value;
    }

    /// Read back one routing register.
    #[must_use]
    pub const fn route_register(&self, pirq: usize) -> u8 {
        self.routes[pirq % PIRQ_LINES]
    }

    /// Load all four routing registers at once from the `PIRQRC[A-D]` bytes a
    /// guest has programmed in the PIIX3 bridge's config space (offsets
    /// `0x60`..`0x63`, [`PIRQ_ROUTE_CONFIG_BASE`](crate::pcie::PIRQ_ROUTE_CONFIG_BASE)).
    /// This is how the router picks up routing the guest configured through PCI
    /// config writes, keeping a single source of truth in config space.
    pub const fn sync_from_config(&mut self, pirq_registers: [u8; PIRQ_LINES]) {
        self.routes = pirq_registers;
    }

    /// The ISA IRQ a PIRQ line currently drives in **PIC mode**, or `None` if the
    /// register is disabled (bit 7) or programmed to an IRQ that cannot carry a
    /// PCI interrupt. The PIIX3 hardwires IRQ 0/1/2/8/13 (and the always-edge
    /// timer/keyboard/cascade/RTC/FPU lines) so they are never valid PCI targets;
    /// the routable set is 3-7, 9-12, 14, 15.
    #[must_use]
    pub const fn isa_irq(&self, pirq: usize) -> Option<u8> {
        let reg = self.routes[pirq % PIRQ_LINES];
        if reg & ROUTE_DISABLED != 0 {
            return None;
        }
        let irq = reg & ROUTE_IRQ_MASK;
        if is_pci_routable_irq(irq) {
            Some(irq)
        } else {
            None
        }
    }

    /// The ISA IRQ a device's `(slot, pin)` resolves to in PIC mode: the swizzle
    /// followed by the routing register. `None` if the device declares no pin or
    /// its PIRQ line is unrouted.
    #[must_use]
    pub const fn device_isa_irq(&self, slot: u8, pin: u8) -> Option<u8> {
        match Self::pirq_line(slot, pin) {
            Some(line) => self.isa_irq(line),
            None => None,
        }
    }

    /// A router programmed with the firmware [`PIRQ_DEFAULT_IRQS`] routing — the
    /// state a guest finds after power-on, and the routing the static PIC-mode
    /// `_PRT` advertises.
    #[must_use]
    pub const fn firmware_default() -> Self {
        Self {
            routes: PIRQ_DEFAULT_IRQS,
        }
    }

    /// The ISA IRQ a device's `(slot, pin)` resolves to under the **default**
    /// PIC-mode routing ([`PIRQ_DEFAULT_IRQS`]): the swizzle onto `PIRQ[A..D]`,
    /// then the firmware-default IRQ for that line. `None` if the device declares
    /// no pin. Used to build the static PIC-mode `_PRT` so it matches the routing
    /// the firmware programs.
    #[must_use]
    pub const fn default_device_isa_irq(slot: u8, pin: u8) -> Option<u8> {
        match Self::pirq_line(slot, pin) {
            Some(line) => Some(PIRQ_DEFAULT_IRQS[line]),
            None => None,
        }
    }

    /// The I/O APIC GSI a device's `(slot, pin)` resolves to in **APIC mode**:
    /// the swizzle onto `PIRQ[A..D]`, then the fixed `PIRQA`→16 .. `PIRQD`→19
    /// wiring (independent of the routing registers). `None` if the device
    /// declares no pin.
    #[must_use]
    pub const fn device_gsi(slot: u8, pin: u8) -> Option<u8> {
        // `pirq_line` is 0..=3; map it onto GSI 16..=19 without a narrowing cast.
        match Self::pirq_line(slot, pin) {
            Some(0) => Some(PIRQ_GSI_BASE),
            Some(1) => Some(PIRQ_GSI_BASE + 1),
            Some(2) => Some(PIRQ_GSI_BASE + 2),
            Some(_) => Some(PIRQ_GSI_BASE + 3),
            None => None,
        }
    }
}

/// Whether `irq` is an ISA IRQ a PIIX3 PIRQ line may be routed to. The hardwired
/// legacy functions — IRQ0 timer, IRQ1 keyboard, IRQ2 cascade, IRQ8 RTC, IRQ13
/// FPU — are never valid PCI targets; everything else in 3-15 is.
const fn is_pci_routable_irq(irq: u8) -> bool {
    matches!(irq, 3..=7 | 9..=12 | 14 | 15)
}

#[cfg(test)]
mod tests {
    use super::{PIRQ_GSI_BASE, PIRQ_ROUTE_BASE, PirqRouter, ROUTE_DISABLED};
    use crate::interrupt::pic::{
        DualPic, ELCR_SLAVE, MASTER_CMD, MASTER_DATA, SLAVE_CMD, SLAVE_DATA,
    };

    #[test]
    fn reset_state_routes_nothing() {
        let r = PirqRouter::new();
        for line in 0..4 {
            assert_eq!(r.route_register(line), ROUTE_DISABLED);
            assert_eq!(r.isa_irq(line), None);
        }
        assert_eq!(PIRQ_ROUTE_BASE, 0x60);
    }

    #[test]
    fn the_pci_swizzle_spreads_slot_pins_across_the_four_lines() {
        // INTA (pin 1) of consecutive slots walks the four PIRQ lines.
        assert_eq!(PirqRouter::pirq_line(0, 1), Some(0)); // slot 0 INTA -> PIRQA
        assert_eq!(PirqRouter::pirq_line(1, 1), Some(1)); // slot 1 INTA -> PIRQB
        assert_eq!(PirqRouter::pirq_line(2, 1), Some(2));
        assert_eq!(PirqRouter::pirq_line(3, 1), Some(3));
        assert_eq!(PirqRouter::pirq_line(4, 1), Some(0)); // wraps
        // Within one slot, the four pins also spread across the four lines.
        assert_eq!(PirqRouter::pirq_line(0, 2), Some(1)); // INTB
        assert_eq!(PirqRouter::pirq_line(0, 3), Some(2)); // INTC
        assert_eq!(PirqRouter::pirq_line(0, 4), Some(3)); // INTD
        // No pin declared (0) or out of range -> no line.
        assert_eq!(PirqRouter::pirq_line(0, 0), None);
        assert_eq!(PirqRouter::pirq_line(0, 5), None);
    }

    #[test]
    fn disabled_and_reserved_irqs_do_not_resolve() {
        let mut r = PirqRouter::new();
        // Bit 7 set => disabled, even with a valid IRQ in the low nibble.
        r.set_route(0, 0x80 | 0x0A);
        assert_eq!(r.isa_irq(0), None);
        // A hardwired legacy IRQ (0/1/2/8/13) is not PCI-routable.
        for irq in [0u8, 1, 2, 8, 13] {
            r.set_route(0, irq);
            assert_eq!(r.isa_irq(0), None, "IRQ {irq} must not be routable");
        }
        // A normal PCI IRQ resolves.
        r.set_route(0, 11);
        assert_eq!(r.isa_irq(0), Some(11));
    }

    #[test]
    fn device_resolves_to_an_isa_irq_through_swizzle_then_register() {
        let mut r = PirqRouter::new();
        // Route PIRQB -> IRQ10. A slot-1 INTA device swizzles to PIRQB.
        r.set_route(1, 10);
        assert_eq!(r.device_isa_irq(1, 1), Some(10));
        // A slot-0 INTB device also lands on PIRQB -> the same shared IRQ10
        // (PCI interrupt sharing).
        assert_eq!(r.device_isa_irq(0, 2), Some(10));
        // A device on an unrouted line resolves to nothing.
        assert_eq!(r.device_isa_irq(0, 1), None); // PIRQA still disabled
    }

    #[test]
    fn firmware_default_routing_resolves_pic_mode_irqs() {
        use super::PIRQ_DEFAULT_IRQS;
        // The default routes are all PCI-routable and avoid the modeled legacy
        // IRQs (1,4,8,12) and the SCI (9).
        for irq in PIRQ_DEFAULT_IRQS {
            assert!(
                matches!(irq, 3..=7 | 9..=12 | 14 | 15),
                "default IRQ {irq} must be PCI-routable"
            );
            assert!(
                !matches!(irq, 1 | 4 | 8 | 12 | 9),
                "default IRQ {irq} must not collide with a modeled legacy line"
            );
        }
        // A firmware-default router resolves a device through the swizzle + default.
        let r = PirqRouter::firmware_default();
        // slot 0 INTA -> PIRQA -> PIRQ_DEFAULT_IRQS[0].
        assert_eq!(r.device_isa_irq(0, 1), Some(PIRQ_DEFAULT_IRQS[0]));
        // slot 1 INTA -> PIRQB -> PIRQ_DEFAULT_IRQS[1].
        assert_eq!(r.device_isa_irq(1, 1), Some(PIRQ_DEFAULT_IRQS[1]));
        // The const resolver agrees with the live router.
        assert_eq!(
            PirqRouter::default_device_isa_irq(2, 1),
            Some(PIRQ_DEFAULT_IRQS[2])
        );
        assert_eq!(PirqRouter::default_device_isa_irq(0, 0), None);
    }

    #[test]
    fn apic_mode_maps_pirq_lines_to_gsi_16_through_19() {
        assert_eq!(PirqRouter::device_gsi(0, 1), Some(PIRQ_GSI_BASE)); // PIRQA -> 16
        assert_eq!(PirqRouter::device_gsi(1, 1), Some(17)); // PIRQB -> 17
        assert_eq!(PirqRouter::device_gsi(2, 1), Some(18));
        assert_eq!(PirqRouter::device_gsi(3, 1), Some(19));
        assert_eq!(PirqRouter::device_gsi(0, 0), None);
    }

    /// End-to-end PIC-mode path: program the router, set the target IRQ to level
    /// in the ELCR (PCI interrupts are level), then drive a device's `INTx` through
    /// the resolved IRQ and observe the PIC deliver it — and re-deliver while the
    /// line stays asserted, exactly as a shared level-triggered PCI line does.
    #[test]
    fn pci_intx_drives_a_level_irq_on_the_8259() {
        let mut router = PirqRouter::new();
        // Firmware routes PIRQA -> IRQ11 (config write to 0x60).
        router.set_route(0, 11);

        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        // IRQ11 is on the slave; mark it level in the slave ELCR (bit 11-8 = 3).
        pic.write_elcr(ELCR_SLAVE, 1 << 3);

        // A slot-0 INTA device asserts: resolve and drive the line.
        let irq = router.device_isa_irq(0, 1).expect("PIRQA routed to IRQ11");
        assert_eq!(irq, 11);
        pic.set_irq_level(irq, true);

        // The PIC presents IRQ11's vector (slave base 0x28 + 3 = 0x2B).
        assert_eq!(pic.pending_vector(), Some(0x2B));
        assert_eq!(pic.acknowledge(), Some(0x2B));
        // In service until EOI...
        assert_eq!(pic.pending_vector(), None);
        // ...then the still-asserted level line re-fires (shared PCI IRQ).
        pic.write_port(SLAVE_CMD, 0x20); // slave EOI
        pic.write_port(MASTER_CMD, 0x20); // master EOI (cascade)
        assert_eq!(pic.pending_vector(), Some(0x2B));
        // Device deasserts -> request withdrawn.
        pic.set_irq_level(irq, false);
        assert_eq!(pic.pending_vector(), None);
    }

    #[test]
    fn router_syncs_routing_a_guest_programmed_in_the_lpc_bridge_config() {
        use crate::pcie::{
            ICH9_LPC_DEVICE_ID, LPC_BRIDGE_BDF, PIRQ_ROUTE_CONFIG_BASE, PcieRootComplex, vendors,
        };

        // A guest enumerates the ICH9 LPC bridge (00:1F.0) and programs PIRQB ->
        // IRQ10 by writing config offset 0x61, leaving the others at reset (0x80).
        let mut bridge =
            PcieRootComplex::create_isa_bridge(LPC_BRIDGE_BDF, vendors::INTEL, ICH9_LPC_DEVICE_ID);
        assert_eq!(
            bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE),
            0x80,
            "PIRQA resets to disabled"
        );
        bridge.write_u8(PIRQ_ROUTE_CONFIG_BASE + 1, 10); // PIRQB -> IRQ10

        // The router reads the four PIRQRC bytes straight out of config space.
        let regs = [
            bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE),
            bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE + 1),
            bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE + 2),
            bridge.read_u8(PIRQ_ROUTE_CONFIG_BASE + 3),
        ];
        let mut router = PirqRouter::new();
        router.sync_from_config(regs);

        // A slot-1 INTA device swizzles to PIRQB, which now routes to IRQ10; the
        // still-disabled PIRQA resolves to nothing.
        assert_eq!(router.device_isa_irq(1, 1), Some(10));
        assert_eq!(router.device_isa_irq(0, 1), None);
    }

    fn init_pc_at(pic: &mut DualPic) {
        pic.write_port(MASTER_CMD, 0x11);
        pic.write_port(MASTER_DATA, 0x20);
        pic.write_port(MASTER_DATA, 0x04);
        pic.write_port(MASTER_DATA, 0x01);
        pic.write_port(SLAVE_CMD, 0x11);
        pic.write_port(SLAVE_DATA, 0x28);
        pic.write_port(SLAVE_DATA, 0x02);
        pic.write_port(SLAVE_DATA, 0x01);
        pic.write_port(MASTER_DATA, 0x00);
        pic.write_port(SLAVE_DATA, 0x00);
    }
}

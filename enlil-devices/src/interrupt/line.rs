//! Wiring device IRQ lines to the [`InterruptController`].
//!
//! Device models in this crate (and the 16550 UART in `enlil-core`) drive a
//! host-side level sink — an `IrqLine` — whenever an enabled interrupt
//! condition changes. On real hardware that pin runs to an input of the I/O
//! APIC; here it runs to [`InterruptController::deliver_irq`]. This module
//! provides the shared, thread-safe handle and the small adapter that connects
//! the two, so the native (VMX) and KVM backends wire a device the same way:
//!
//! ```ignore
//! let pic = SharedInterruptController::new(1);
//! // program IRQ0/IRQ4 redirection entries, enable LAPIC 0, ...
//! pit.attach_irq0(Box::new(pic.line(0)));        // 8254 PIT -> IRQ0
//! com1.attach_irq_line(Box::new(pic.line(4)));   // 16550 UART -> IRQ4
//! ```
//!
//! Both `IrqLine` traits (the PIT's in this crate, the UART's in `enlil-core`)
//! have a blanket `impl … for F: Fn(bool) + Send`, so the closure returned by
//! [`SharedInterruptController::line`] satisfies either one through that impl —
//! a single wiring path that does not need this crate to depend on `enlil-core`.

use std::sync::{Arc, Mutex};

use super::controller::InterruptController;
use super::ioapic::IOAPIC_BASE;
use super::msi::MsiMessage;
use crate::bus::MmioDevice;
use crate::truncate::u32_of;

/// Size of the I/O APIC MMIO aperture (one 4 KiB page at [`IOAPIC_BASE`]).
const IOAPIC_MMIO_SIZE: u64 = 0x1000;

/// Standard-PC ACPI interrupt-source overrides that change the *Global System
/// Interrupt* (I/O APIC pin) number an ISA IRQ is delivered on.
///
/// On a PC/AT the only override that renumbers a GSI is the system timer:
/// ISA IRQ0 (the 8254 PIT) is wired to I/O APIC pin 2, not pin 0 — the classic
/// `(bus 0, source 0) -> GSI 2` MADT interrupt-source-override. Every other ISA
/// IRQ identity-maps to its own GSI. This table is the wiring-side companion to
/// the override `MadtBuilder::standard` advertises, so a guest that programs the
/// I/O APIC from the MADT and the device line that feeds it agree on the pin
/// (see [`SharedInterruptController::isa_line`]).
///
/// The polarity/trigger overrides (e.g. the active-low, level-triggered SCI)
/// keep the same GSI number, so they do not appear here — they are encoded in
/// the RTE the guest programs, not in this pin remapping.
const STANDARD_PC_GSI_OVERRIDES: &[(u8, u32)] = &[(0, 2)];

/// Resolve an ISA IRQ to the Global System Interrupt (I/O APIC pin) it is
/// delivered on under the standard-PC interrupt-source overrides.
///
/// Returns GSI 2 for the PC/AT timer (ISA IRQ0) and an identity mapping for
/// every other line. This is the single source of truth shared by the device
/// line wiring ([`SharedInterruptController::isa_line`]) and cross-checked
/// against the MADT the guest reads.
#[must_use]
pub fn isa_to_gsi(isa_irq: u8) -> u32 {
    STANDARD_PC_GSI_OVERRIDES
        .iter()
        .find_map(|&(src, gsi)| (src == isa_irq).then_some(gsi))
        .unwrap_or_else(|| u32::from(isa_irq))
}

/// A thread-safe, shareable handle to one [`InterruptController`].
///
/// vCPUs run on separate threads and a device fires from whichever thread owns
/// it, so the controller is guarded by a `Mutex` — matching the `Send` bound
/// the device `IrqLine` traits require. Cloning shares the same controller.
#[derive(Clone)]
pub struct SharedInterruptController(Arc<Mutex<InterruptController>>);

impl SharedInterruptController {
    /// Wrap a fresh controller for `num_vcpus` vCPUs.
    #[must_use]
    pub fn new(num_vcpus: u8) -> Self {
        Self(Arc::new(Mutex::new(InterruptController::new(num_vcpus))))
    }

    /// Wrap an already-built controller (e.g. one whose RTEs are pre-programmed).
    #[must_use]
    pub fn from_controller(controller: InterruptController) -> Self {
        Self(Arc::new(Mutex::new(controller)))
    }

    /// Run `f` with exclusive access to the controller — to program the I/O APIC
    /// redirection table, read pending vectors, signal EOI, etc.
    ///
    /// # Panics
    /// Panics if the controller mutex has been poisoned by a prior panic while
    /// the lock was held.
    pub fn with<R>(&self, f: impl FnOnce(&mut InterruptController) -> R) -> R {
        f(&mut self.0.lock().expect("interrupt controller mutex poisoned"))
    }

    /// Build a level sink for `irq`, suitable for `attach_irq0` /
    /// `attach_irq_line`.
    ///
    /// A rising edge (`set_level(true)`) asserts the line via
    /// [`InterruptController::deliver_irq`]; a falling edge (`set_level(false)`)
    /// deasserts it via [`InterruptController::clear_irq`] (a no-op for an
    /// edge-triggered ISA line, but correct for a level-triggered RTE). The
    /// returned closure is `Send` and owns a clone of this handle, so it
    /// outlives the borrow.
    pub fn line(&self, irq: u8) -> impl Fn(bool) + Send + use<> {
        let controller = self.clone();
        move |level: bool| {
            controller.with(|c| {
                if level {
                    c.deliver_irq(irq);
                } else {
                    c.clear_irq(irq);
                }
            });
        }
    }

    /// Build an MSI/MSI-X message sink: a `Send` closure that injects each
    /// message via [`InterruptController::deliver_msi`].
    ///
    /// This is the message-signalled counterpart to [`line`](Self::line) — a PCI
    /// function in MSI/MSI-X mode hands a fully-formed `(address, data)` message
    /// straight to the LAPICs (decoded for destination/vector) rather than
    /// asserting a wired `INTx` pin into the I/O APIC. The xHCI MMIO adapter
    /// (`set_msi_sink`) drives it on the emulated backend; the KVM backend uses
    /// `KVM_SIGNAL_MSI` instead. The closure owns a clone of this handle.
    pub fn msi_sink(&self) -> impl Fn(MsiMessage) + Send + use<> {
        let controller = self.clone();
        move |msg: MsiMessage| {
            controller.with(|c| c.deliver_msi(&msg));
        }
    }

    /// Build a level sink for an ISA device IRQ, resolving the standard-PC
    /// interrupt-source overrides ([`isa_to_gsi`]) to the I/O APIC pin the line
    /// actually drives.
    ///
    /// This is the I/O APIC counterpart to [`line`](Self::line): a device named
    /// by its legacy ISA IRQ (e.g. the 8254 PIT on IRQ0) is wired to the GSI the
    /// MADT advertises (IRQ0 → GSI 2), so a guest that programs the redirection
    /// table from the MADT unmasks the same pin the line asserts. For lines with
    /// no override it is identical to [`line`](Self::line). Use this — not
    /// [`line`](Self::line) — whenever an ISA device feeds the I/O APIC; the raw
    /// [`line`](Self::line) (or the 8259 front-end) still takes the bare ISA IRQ.
    pub fn isa_line(&self, isa_irq: u8) -> impl Fn(bool) + Send + use<> {
        // GSIs for ISA lines fit in the 24-pin I/O APIC; fall back to the raw
        // IRQ if a future override ever exceeds a u8.
        let pin = u8::try_from(isa_to_gsi(isa_irq)).unwrap_or(isa_irq);
        self.line(pin)
    }
}

/// The I/O APIC's MMIO aperture as a bus [`MmioDevice`].
///
/// Mounted at [`IOAPIC_BASE`] (`0xFEC0_0000`), it forwards the two 32-bit
/// registers a guest uses to program the redirection table — `IOREGSEL`
/// (offset `0x00`) and `IOWIN` (offset `0x10`) — into the shared
/// [`InterruptController`]'s I/O APIC. This is the front-end that lets a guest
/// OS route device IRQ lines (see [`SharedInterruptController::line`]) to a
/// vCPU's vector by writing redirection entries; without it the RTEs stay at
/// their masked reset state and no device interrupt is ever delivered.
///
/// Register accesses are 32-bit dwords, so the access size is ignored.
pub struct IoApicMmio {
    controller: SharedInterruptController,
}

impl IoApicMmio {
    /// Mount the I/O APIC aperture over the shared controller `controller`.
    #[must_use]
    pub const fn new(controller: SharedInterruptController) -> Self {
        Self { controller }
    }
}

impl MmioDevice for IoApicMmio {
    fn mmio_read(&mut self, offset: u64, _size: u8) -> u64 {
        self.controller
            .with(|c| u64::from(c.ioapic.mmio_read(offset)))
    }

    fn mmio_write(&mut self, offset: u64, _size: u8, data: u64) {
        // IOREGSEL/IOWIN are 32-bit; the I/O APIC takes the low dword.
        self.controller
            .with(|c| c.ioapic.mmio_write(offset, u32_of(data)));
    }

    fn mmio_range(&self) -> (u64, u64) {
        (IOAPIC_BASE, IOAPIC_BASE + IOAPIC_MMIO_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupt::lapic::LAPIC_SVR;

    /// Unmask IRQ `irq`'s redirection entry, route it to LAPIC 0 with `vector`,
    /// and enable LAPIC 0 — the minimal programming a guest OS would do.
    fn program(pic: &SharedInterruptController, irq: u8, vector: u8) {
        pic.with(|c| {
            let rte = c.ioapic.get_rte_mut(irq);
            rte.set_vector(vector);
            rte.set_destination(0);
            rte.set_masked(false);
            c.lapics[0].write_register(LAPIC_SVR, 0x1FF);
        });
    }

    #[test]
    fn rising_edge_delivers_to_lapic_irr() {
        let pic = SharedInterruptController::new(1);
        program(&pic, 0, 0x20);

        let line = pic.line(0);
        assert!(!pic.with(|c| c.has_pending(0)));
        line(true);

        assert!(pic.with(|c| c.has_pending(0)));
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x20));
    }

    #[test]
    fn pulse_true_then_false_still_delivers_one_edge() {
        // The PIT pulses true then immediately false per terminal-count edge;
        // an edge-triggered line must still latch the vector in the IRR.
        let pic = SharedInterruptController::new(1);
        program(&pic, 0, 0x20);

        let line = pic.line(0);
        line(true);
        line(false);

        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x20));
    }

    #[test]
    fn msi_sink_delivers_message_to_lapic() {
        // The message-signalled path: a fully-formed MSI message reaches the
        // destination LAPIC's IRR without any I/O APIC redirection entry.
        let pic = SharedInterruptController::new(1);
        pic.with(|c| c.lapics[0].write_register(LAPIC_SVR, 0x1FF));

        let sink = pic.msi_sink();
        // Address: dest LAPIC 0, physical; Data: vector 0x55, fixed delivery.
        sink(MsiMessage::new(0xFEE0_0000, 0x55));

        assert!(pic.with(|c| c.has_pending(0)));
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x55));
    }

    #[test]
    fn masked_line_delivers_nothing() {
        // RTEs default to masked; a device asserting before the OS programs the
        // I/O APIC must not reach any LAPIC.
        let pic = SharedInterruptController::new(1);
        pic.with(|c| c.lapics[0].write_register(LAPIC_SVR, 0x1FF));

        let line = pic.line(4);
        line(true);

        assert!(!pic.with(|c| c.has_pending(0)));
    }

    #[test]
    fn separate_lines_target_their_own_vectors() {
        let pic = SharedInterruptController::new(1);
        program(&pic, 0, 0x20); // PIT  -> IRQ0
        program(&pic, 4, 0x24); // UART -> IRQ4

        let irq0 = pic.line(0);
        let irq4 = pic.line(4);
        irq4(true);

        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x24));

        // Service it, then IRQ0 lands its own vector.
        pic.with(|c| {
            c.lapics[0].start_servicing(0x24);
            c.eoi(0, 0x24);
        });
        irq0(true);
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x20));
    }

    #[test]
    fn handle_clones_share_one_controller() {
        let pic = SharedInterruptController::new(1);
        program(&pic, 0, 0x20);
        let other = pic.clone();

        pic.line(0)(true);
        // Observed through an independent clone -> same underlying controller.
        assert!(other.with(|c| c.has_pending(0)));
    }

    #[test]
    fn mmio_front_end_programs_an_rte_and_routes_delivery() {
        let pic = SharedInterruptController::new(1);
        pic.with(|c| c.lapics[0].write_register(LAPIC_SVR, 0x1FF));
        let mut mmio = IoApicMmio::new(pic.clone());

        // Program IRQ4's RTE through MMIO the way a guest OS would: select the
        // low dword (REDTBL base 0x10 + 2*4 = 0x18), write vector 0x24 unmasked;
        // then the high dword (0x19), destination APIC 0.
        mmio.mmio_write(0x00, 4, 0x18); // IOREGSEL = RTE4 low
        mmio.mmio_write(0x10, 4, 0x24); // IOWIN: vector 0x24, unmasked
        mmio.mmio_write(0x00, 4, 0x19); // IOREGSEL = RTE4 high
        mmio.mmio_write(0x10, 4, 0); // IOWIN: destination 0

        // Read the low dword back through MMIO.
        mmio.mmio_write(0x00, 4, 0x18);
        assert_eq!(mmio.mmio_read(0x10, 4) & 0xFF, 0x24);

        // A device asserting IRQ4 now routes through the programmed RTE to
        // LAPIC 0 — the full guest-programmed delivery path.
        pic.line(4)(true);
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x24));
    }

    #[test]
    fn mmio_aperture_covers_the_ioapic_page() {
        let mmio = IoApicMmio::new(SharedInterruptController::new(1));
        assert_eq!(mmio.mmio_range(), (0xFEC0_0000, 0xFEC0_1000));
    }

    #[test]
    fn isa_to_gsi_remaps_only_the_timer() {
        // PC/AT timer (ISA IRQ0) is on GSI 2; every other line identity-maps.
        assert_eq!(isa_to_gsi(0), 2);
        for irq in 1..16u8 {
            assert_eq!(
                isa_to_gsi(irq),
                u32::from(irq),
                "IRQ{irq} must identity-map"
            );
        }
    }

    /// The wiring-side override table ([`isa_to_gsi`]) must agree with the
    /// timer interrupt-source-override the MADT advertises to the guest — if
    /// they ever drift, a guest that programs the I/O APIC from the MADT would
    /// unmask a pin the PIT line never drives, and the timer would silently die.
    #[test]
    fn isa_to_gsi_agrees_with_the_madt_timer_override() {
        use crate::acpi::madt::MadtBuilder;

        let madt = MadtBuilder::standard(1).build();

        // Walk the variable-length entry list past the 36-byte SDT header and the
        // 8 bytes of fixed MADT fields, looking for the type-2 override whose
        // source bus/IRQ is ISA IRQ0.
        let mut offset = 44;
        let mut timer_gsi = None;
        while offset + 1 < madt.len() {
            let entry_type = madt[offset];
            let entry_len = madt[offset + 1] as usize;
            if entry_type == 2 && madt[offset + 2] == 0 && madt[offset + 3] == 0 {
                timer_gsi = Some(u32::from_le_bytes(
                    madt[offset + 4..offset + 8].try_into().unwrap(),
                ));
                break;
            }
            offset += entry_len.max(1);
        }

        assert_eq!(
            timer_gsi,
            Some(isa_to_gsi(0)),
            "MADT timer override GSI must match the line wiring's isa_to_gsi(0)"
        );
    }

    #[test]
    fn isa_line_delivers_the_timer_on_gsi_2_not_pin_0() {
        // A guest that reads the MADT programs the timer's RTE on GSI 2. The PIT
        // line, wired via isa_line(IRQ0), must assert that pin — pin 0 stays dark.
        let pic = SharedInterruptController::new(1);
        program(&pic, 2, 0x20); // GSI 2 RTE -> vector 0x20 (the timer)
        pic.with(|c| c.lapics[0].write_register(LAPIC_SVR, 0x1FF));

        let timer = pic.isa_line(0);
        timer(true);
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x20));
    }

    #[test]
    fn isa_line_leaving_pin_0_unprogrammed_delivers_nothing() {
        // The flip side: if the guest had (incorrectly) programmed pin 0 instead
        // of GSI 2, the override-aware line would deliver nothing — proving the
        // line drives GSI 2 and not the bare IRQ number.
        let pic = SharedInterruptController::new(1);
        program(&pic, 0, 0x20); // pin 0, the pre-override (wrong) pin
        pic.with(|c| c.lapics[0].write_register(LAPIC_SVR, 0x1FF));

        pic.isa_line(0)(true);
        assert!(!pic.with(|c| c.has_pending(0)));
    }

    #[test]
    fn isa_line_is_identity_for_non_overridden_lines() {
        // COM1 (IRQ4) has no override, so isa_line and line target the same pin.
        let pic = SharedInterruptController::new(1);
        program(&pic, 4, 0x24);
        pic.with(|c| c.lapics[0].write_register(LAPIC_SVR, 0x1FF));

        pic.isa_line(4)(true);
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x24));
    }

    #[test]
    fn level_triggered_line_clears_on_falling_edge() {
        let pic = SharedInterruptController::new(1);
        pic.with(|c| {
            let rte = c.ioapic.get_rte_mut(5);
            rte.set_vector(0x25);
            rte.set_destination(0);
            rte.set_masked(false);
            rte.set_low(rte.low() | (1 << 15)); // level-triggered
            c.lapics[0].write_register(LAPIC_SVR, 0x1FF);
        });

        let line = pic.line(5);
        line(true);
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x25));

        // A second assertion while still asserted does not re-deliver...
        pic.with(|c| c.lapics[0].start_servicing(0x25));
        line(true);
        // ...but after deasserting (falling edge) the line can fire again.
        line(false);
        pic.with(|c| c.eoi(0, 0x25));
        line(true);
        assert_eq!(pic.with(|c| c.pending_vector(0)), Some(0x25));
    }
}

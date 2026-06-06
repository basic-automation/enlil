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
use crate::bus::MmioDevice;
use crate::truncate::u32_of;

/// Size of the I/O APIC MMIO aperture (one 4 KiB page at [`IOAPIC_BASE`]).
const IOAPIC_MMIO_SIZE: u64 = 0x1000;

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

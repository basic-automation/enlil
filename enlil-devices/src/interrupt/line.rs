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

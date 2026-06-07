//! Chipset system-control ports that aren't owned by a specific peripheral.
//!
//! These are the small fixed-function I/O ports a PC chipset (the PIIX/ICH
//! south-bridge, historically the PS/2 controller's "port A") exposes for
//! platform control — distinct from device register files like the PIT or RTC.
//! Today this is **System Control Port A** (`0x92`): the fast-A20 / fast-reset
//! register every x86 boot path touches.

use crate::bus::PioDevice;
use crate::truncate::u8_of;

/// The port [`SystemControlPortA`] claims.
pub const PORT_A: u16 = 0x92;

/// Bit 0: fast INIT (CPU reset). Writing 1 requests a reset; it reads back 0.
const FAST_RESET: u8 = 1 << 0;
/// Bit 1: fast A20 gate. When set, the A20 address line is enabled (unmasked).
const A20_GATE: u8 = 1 << 1;

/// **System Control Port A** (`0x92`) as a bus [`PioDevice`].
///
/// This is the fast path for the two things early boot used to drive through the
/// keyboard controller: opening the **A20 gate** (bit 1) and pulsing a **CPU
/// reset** (bit 0). The slow i8042 route still works, but every modern boot path
/// (and most BIOSes) prefers `0x92` because it's a single `out` rather than the
/// keyboard-controller command dance.
///
/// - **A20 (bit 1):** read/write, and defaults to **enabled**. Enlil runs no
///   legacy BIOS that performs the real-mode A20 handshake, and KVM keeps A20
///   open, so the gate is modelled as already unmasked — a guest that reads
///   `0x92` to confirm A20 finds it set, and one that writes the bit sees it
///   stick, both matching a post-firmware machine.
/// - **Fast reset (bit 0):** write-1 latches a [reset request](Self::take_reset)
///   for the run loop to act on (reinitialise the vCPU to its reset vector); the
///   bit is edge-triggered, so it always reads back 0.
/// - Other bits are stored and read back unchanged (reserved / lock bits).
pub struct SystemControlPortA {
    /// Stored bits 1-7 (bit 0, the reset edge, is never stored).
    value: u8,
    /// Set when bit 0 was written 1; consumed by [`take_reset`](Self::take_reset).
    reset_requested: bool,
}

impl Default for SystemControlPortA {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemControlPortA {
    /// A new port with the A20 gate already enabled (post-firmware default) and
    /// no pending reset.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            value: A20_GATE,
            reset_requested: false,
        }
    }

    /// Whether the A20 address line is currently enabled (bit 1).
    #[must_use]
    pub const fn a20_enabled(&self) -> bool {
        self.value & A20_GATE != 0
    }

    /// Consume a pending fast-reset request: returns `true` exactly once after a
    /// guest writes bit 0 = 1, so the backend's run loop can reset the vCPU and
    /// then clear the latch.
    pub const fn take_reset(&mut self) -> bool {
        let pending = self.reset_requested;
        self.reset_requested = false;
        pending
    }
}

impl PioDevice for SystemControlPortA {
    fn pio_read(&mut self, _port: u16, _size: u8) -> u32 {
        // Bit 0 is the reset edge — it always reads back 0.
        u32::from(self.value & !FAST_RESET)
    }

    fn pio_write(&mut self, _port: u16, _size: u8, data: u32) {
        let byte = u8_of(data);
        if byte & FAST_RESET != 0 {
            self.reset_requested = true;
        }
        // Persist everything except the reset edge.
        self.value = byte & !FAST_RESET;
    }

    fn port_range(&self) -> (u16, u16) {
        (PORT_A, PORT_A + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{A20_GATE, PORT_A, SystemControlPortA};
    use crate::bus::PioDevice;

    #[test]
    fn claims_only_port_0x92() {
        assert_eq!(SystemControlPortA::new().port_range(), (0x92, 0x93));
    }

    #[test]
    fn a20_is_enabled_by_default_and_reads_back_set() {
        let mut port = SystemControlPortA::new();
        assert!(port.a20_enabled());
        assert_ne!(port.pio_read(PORT_A, 1) & u32::from(A20_GATE), 0);
    }

    #[test]
    fn writing_bit1_toggles_the_a20_gate() {
        let mut port = SystemControlPortA::new();
        // Clear A20.
        port.pio_write(PORT_A, 1, 0x00);
        assert!(!port.a20_enabled());
        assert_eq!(port.pio_read(PORT_A, 1) & u32::from(A20_GATE), 0);
        // Re-enable it.
        port.pio_write(PORT_A, 1, u32::from(A20_GATE));
        assert!(port.a20_enabled());
    }

    #[test]
    fn writing_bit0_latches_a_one_shot_reset_request() {
        let mut port = SystemControlPortA::new();
        assert!(!port.take_reset(), "no reset pending at start");

        // Pulse fast reset together with A20 enabled.
        port.pio_write(PORT_A, 1, 0x03);
        assert!(port.take_reset(), "reset latched");
        assert!(!port.take_reset(), "and consumed exactly once");
        // A20 still set; the reset edge did not persist.
        assert!(port.a20_enabled());
    }

    #[test]
    fn the_reset_bit_always_reads_back_zero() {
        let mut port = SystemControlPortA::new();
        port.pio_write(PORT_A, 1, 0xFF);
        assert_eq!(port.pio_read(PORT_A, 1) & 0x01, 0, "bit 0 reads 0");
    }
}

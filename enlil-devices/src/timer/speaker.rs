//! System Control Port B (`0x61`) — the legacy NMI status/control register that
//! also gates the PIT's channel-2 tone (the PC speaker) and reads back its OUT
//! pin and the DRAM-refresh clock.
//!
//! On a real PC/AT this single port lives in the chipset, not the i8042, which
//! is why [`Ps2DataPort`](crate::ps2::Ps2DataPort)/`Ps2CmdPort` deliberately
//! leave `0x61` unclaimed. It carries:
//!
//! | bit | dir | meaning                                                |
//! |-----|-----|--------------------------------------------------------|
//! | 0   | R/W | Timer-2 GATE — drives PIT channel-2's gate (tone on)   |
//! | 1   | R/W | Speaker data enable (the tone reaches the speaker)     |
//! | 2   | R/W | Parity-check / PCI I/O-check enable (stored, inert)    |
//! | 3   | R/W | Channel-check enable (stored, inert)                   |
//! | 4   | R   | Refresh request — toggles each read (the refresh clock)|
//! | 5   | R   | Timer-2 OUT pin (the channel-2 output level)           |
//! | 6   | R   | I/O-channel-check latch (no error → 0)                 |
//! | 7   | R   | System parity-error latch (no error → 0)               |
//!
//! Two guest behaviours make this port matter beyond the speaker: software that
//! plays a tone toggles bits 0-1 and polls bit 5 to time the waveform, and old
//! timing loops poll bit 4 (the refresh clock) as a coarse delay reference. Both
//! need real, changing reads rather than open-bus `0xFF`.

use super::pit::SharedPit;
use crate::bus::PioDevice;
use crate::truncate::u8_of;

/// The port this device claims: System Control Port B.
pub const PORT_B: u16 = 0x61;

/// Bit 0: PIT channel-2 GATE — when set, channel 2 counts (the tone plays).
const TIMER2_GATE: u8 = 1 << 0;
/// Bit 1: speaker data enable — the channel-2 OUT pin reaches the speaker.
const SPEAKER_DATA: u8 = 1 << 1;
/// Bits 0-3 are software-writable and read back as last written; bits 4-7 are
/// status bits computed on each read.
const WRITABLE_MASK: u8 = 0x0F;
/// Bit 4: DRAM-refresh request, toggled on every read.
const REFRESH_TOGGLE: u8 = 1 << 4;
/// Bit 5: the PIT channel-2 OUT-pin level.
const TIMER2_OUT: u8 = 1 << 5;

/// System Control Port B (`0x61`) as a bus [`PioDevice`], coupled to the PIT's
/// channel 2 through a [`SharedPit`] handle.
pub struct SystemControlPortB {
    /// The PIT whose channel 2 this port gates and whose OUT pin it reports.
    pit: SharedPit,
    /// Last value written to the software-writable bits (0-3).
    control: u8,
    /// The refresh-clock bit (bit 4), toggled on each read.
    refresh: bool,
}

impl SystemControlPortB {
    /// A new port B coupled to `pit`, with the speaker gated off and the refresh
    /// bit low (matching a just-reset chipset).
    #[must_use]
    pub const fn new(pit: SharedPit) -> Self {
        Self {
            pit,
            control: 0,
            refresh: false,
        }
    }

    /// Whether the channel-2 GATE bit (bit 0) is set — i.e. the tone counter is
    /// enabled. The host's audio backend can poll this to know when to emit a
    /// beep.
    #[must_use]
    pub const fn timer2_gated(&self) -> bool {
        self.control & TIMER2_GATE != 0
    }

    /// Whether the speaker-data bit (bit 1) is set — i.e. the channel-2 tone is
    /// routed to the speaker. A tone is audible only when this **and**
    /// [`timer2_gated`](Self::timer2_gated) are set.
    #[must_use]
    pub const fn speaker_enabled(&self) -> bool {
        self.control & SPEAKER_DATA != 0
    }
}

impl PioDevice for SystemControlPortB {
    fn pio_read(&mut self, _port: u16, _size: u8) -> u32 {
        // Bit 4 flips on every read so a guest polling it sees the refresh clock
        // advance; bit 5 mirrors the live PIT channel-2 OUT pin. The error
        // latches (bits 6-7) read 0 — we never inject a parity / I/O-check NMI.
        self.refresh = !self.refresh;
        let out2 = self.pit.with(|p| p.channels[2].status.output);
        let mut val = self.control & WRITABLE_MASK;
        if self.refresh {
            val |= REFRESH_TOGGLE;
        }
        if out2 {
            val |= TIMER2_OUT;
        }
        u32::from(val)
    }

    fn pio_write(&mut self, _port: u16, _size: u8, data: u32) {
        let byte = u8_of(data);
        self.control = byte & WRITABLE_MASK;
        // Bit 0 drives PIT channel-2's gate: the tone counter only advances
        // while gated, so clearing it freezes (silences) the channel.
        let gate = byte & TIMER2_GATE != 0;
        self.pit.with(|p| p.channels[2].gate = gate);
    }

    fn port_range(&self) -> (u16, u16) {
        (PORT_B, PORT_B + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::{PORT_B, SystemControlPortB, TIMER2_OUT};
    use crate::bus::PioDevice;
    use crate::timer::pit::SharedPit;

    #[test]
    fn claims_only_port_0x61() {
        let port = SystemControlPortB::new(SharedPit::new());
        assert_eq!(port.port_range(), (0x61, 0x62));
    }

    #[test]
    fn writing_bit0_gates_pit_channel_2() {
        let pit = SharedPit::new();
        let mut port = SystemControlPortB::new(pit.clone());

        // Channel 2 starts gated-on at reset; clear bit 0 and the gate drops.
        port.pio_write(PORT_B, 1, 0x00);
        assert!(!pit.with(|p| p.channels[2].gate));
        assert!(!port.timer2_gated());

        // Set bit 0 (gate) + bit 1 (speaker): the tone is now enabled+audible.
        port.pio_write(PORT_B, 1, 0x03);
        assert!(pit.with(|p| p.channels[2].gate));
        assert!(port.timer2_gated());
        assert!(port.speaker_enabled());
    }

    #[test]
    fn read_back_reflects_writable_bits_and_masks_the_rest() {
        let mut port = SystemControlPortB::new(SharedPit::new());
        // Write all eight bits; only bits 0-3 are writable and read back.
        port.pio_write(PORT_B, 1, 0xFF);
        let v = port.pio_read(PORT_B, 1);
        assert_eq!(v & 0x0F, 0x0F, "writable bits 0-3 read back");
        assert_eq!(v & 0xC0, 0x00, "error latches (bits 6-7) read 0");
    }

    #[test]
    fn bit4_refresh_toggles_on_every_read() {
        let mut port = SystemControlPortB::new(SharedPit::new());
        let a = port.pio_read(PORT_B, 1) & 0x10;
        let b = port.pio_read(PORT_B, 1) & 0x10;
        let c = port.pio_read(PORT_B, 1) & 0x10;
        assert_ne!(a, b, "refresh bit must change between reads");
        assert_eq!(a, c, "and toggle back");
    }

    #[test]
    fn bit5_mirrors_pit_channel_2_out_pin() {
        let pit = SharedPit::new();
        let mut port = SystemControlPortB::new(pit.clone());
        let out_mask = u32::from(TIMER2_OUT);

        // Force channel-2 OUT high and observe bit 5 follow it.
        pit.with(|p| p.channels[2].status.output = true);
        assert_ne!(port.pio_read(PORT_B, 1) & out_mask, 0);

        pit.with(|p| p.channels[2].status.output = false);
        assert_eq!(port.pio_read(PORT_B, 1) & out_mask, 0);
    }
}

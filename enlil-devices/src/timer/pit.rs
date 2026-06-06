//! Intel 8254 Programmable Interval Timer (PIT) emulation.
//!
//! The PIT has 3 channels:
//! - Channel 0: System timer (IRQ 0)
//! - Channel 1: DRAM refresh (legacy, unused)
//! - Channel 2: PC speaker
//!
//! I/O ports: 0x40-0x43 (channels 0-2 data, 0x43 command)

use crate::bus::PioDevice;
use crate::truncate::u16_of;
/// PIT oscillator frequency in Hz.
pub const PIT_FREQUENCY: u32 = 1_193_182;

/// Nanoseconds per PIT tick.
const NS_PER_TICK: u64 = 838;

/// First port the PIT claims on the I/O bus (channel-0 data).
pub const PIT_PORT_BASE: u16 = 0x40;
/// Number of contiguous ports the PIT claims: `0x40..=0x43`.
pub const PIT_PORT_COUNT: u16 = 4;

/// PIT channel operating modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    InterruptOnTerminalCount = 0,
    HardwareRetriggerable = 1,
    RateGenerator = 2,
    SquareWave = 3,
    SoftwareStrobe = 4,
    HardwareStrobe = 5,
}

impl ChannelMode {
    /// Create a `ChannelMode` from bit representation.
    #[must_use]
    const fn from_bits(bits: u8) -> Self {
        match bits & 0x7 {
            1 => Self::HardwareRetriggerable,
            2 => Self::RateGenerator,
            3 => Self::SquareWave,
            4 => Self::SoftwareStrobe,
            5 => Self::HardwareStrobe,
            _ => Self::InterruptOnTerminalCount,
        }
    }

    /// The 3-bit operating-mode field as it appears in a control/status word.
    #[must_use]
    const fn bits(self) -> u8 {
        match self {
            Self::InterruptOnTerminalCount => 0,
            Self::HardwareRetriggerable => 1,
            Self::RateGenerator => 2,
            Self::SquareWave => 3,
            Self::SoftwareStrobe => 4,
            Self::HardwareStrobe => 5,
        }
    }
}

/// Access mode for reading/writing channel count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    Latch = 0,
    LoByte = 1,
    HiByte = 2,
    LoHiByte = 3,
}

impl AccessMode {
    /// Create an `AccessMode` from bit representation.
    #[must_use]
    const fn from_bits(bits: u8) -> Self {
        match bits & 0x3 {
            1 => Self::LoByte,
            2 => Self::HiByte,
            3 => Self::LoHiByte,
            _ => Self::Latch,
        }
    }

    /// The 2-bit read/write-access field as it appears in a control/status word.
    #[must_use]
    const fn bits(self) -> u8 {
        match self {
            Self::Latch => 0,
            Self::LoByte => 1,
            Self::HiByte => 2,
            Self::LoHiByte => 3,
        }
    }
}

/// A single PIT channel.
/// Lo/hi byte access-latch position for a PIT channel.
#[derive(Debug, Clone, Default)]
pub struct ByteLatch {
    /// Next read targets the high byte (lobyte/hibyte access mode).
    pub read_hi: bool,
    /// Next write targets the high byte.
    pub write_hi: bool,
}

/// The two counting-element status bits surfaced by the read-back status byte
/// (kept together so [`PitChannel`] stays under the bool-field threshold).
#[derive(Debug, Clone, Default)]
pub struct CounterStatus {
    /// Current OUT-pin state (bit 7 of the status byte).
    pub output: bool,
    /// Set when a control word has been written but the initial count has not
    /// yet been loaded into the counting element (bit 6, "null count").
    pub null_count: bool,
}

#[derive(Debug, Clone)]
pub struct PitChannel {
    pub count: u16,
    pub reload: u16,
    pub mode: ChannelMode,
    pub access: AccessMode,
    pub latched_count: Option<u16>,
    /// A read-back-latched status byte, returned ahead of any latched count on
    /// the next data-port read (see [`Pit::read_back`]).
    pub latched_status: Option<u8>,
    pub byte_latch: ByteLatch,
    pub status: CounterStatus,
    pub gate: bool,
    pub enabled: bool,
}

impl PitChannel {
    /// Create a new `PitChannel` with default state.
    #[must_use]
    const fn new() -> Self {
        Self {
            count: 0,
            reload: 0,
            mode: ChannelMode::InterruptOnTerminalCount,
            access: AccessMode::LoHiByte,
            latched_count: None,
            latched_status: None,
            byte_latch: ByteLatch {
                read_hi: false,
                write_hi: false,
            },
            status: CounterStatus {
                output: false,
                null_count: false,
            },
            gate: true,
            enabled: false,
        }
    }

    /// Build the read-back status byte for this channel (Intel 8254 §"Read-Back
    /// Command"): bit 7 = output-pin state, bit 6 = null-count flag, bits 5-4 =
    /// read/write-access field, bits 3-1 = operating mode, bit 0 = BCD (always
    /// 0 — we count in binary).
    #[must_use]
    fn status_byte(&self) -> u8 {
        (u8::from(self.status.output) << 7)
            | (u8::from(self.status.null_count) << 6)
            | (self.access.bits() << 4)
            | (self.mode.bits() << 1)
    }

    fn read_data(&mut self) -> u8 {
        // A read-back-latched status byte is delivered before any latched count.
        if let Some(status) = self.latched_status.take() {
            return status;
        }
        let value = self.latched_count.unwrap_or(self.count);
        match self.access {
            AccessMode::LoByte | AccessMode::Latch => (value & 0xFF) as u8,
            AccessMode::HiByte => ((value >> 8) & 0xFF) as u8,
            AccessMode::LoHiByte => {
                if self.byte_latch.read_hi {
                    self.byte_latch.read_hi = false;
                    self.latched_count = None;
                    ((value >> 8) & 0xFF) as u8
                } else {
                    self.byte_latch.read_hi = true;
                    (value & 0xFF) as u8
                }
            }
        }
    }

    fn write_data(&mut self, val: u8) {
        match self.access {
            AccessMode::LoByte => {
                self.reload = (self.reload & 0xFF00) | u16::from(val);
                self.load_count();
            }
            AccessMode::HiByte => {
                self.reload = (self.reload & 0x00FF) | (u16::from(val) << 8);
                self.load_count();
            }
            AccessMode::LoHiByte => {
                if self.byte_latch.write_hi {
                    self.reload = (self.reload & 0x00FF) | (u16::from(val) << 8);
                    self.byte_latch.write_hi = false;
                    self.load_count();
                } else {
                    self.reload = (self.reload & 0xFF00) | u16::from(val);
                    self.byte_latch.write_hi = true;
                }
            }
            AccessMode::Latch => {}
        }
    }

    fn load_count(&mut self) {
        let effective = if self.reload == 0 {
            0x0001_0000_u32
        } else {
            u32::from(self.reload)
        };
        self.count = u16_of(effective);
        self.enabled = true;
        self.status.output = false;
        // The initial count is now in the counting element.
        self.status.null_count = false;
    }

    const fn tick(&mut self) -> bool {
        if !self.enabled || !self.gate {
            return false;
        }

        match self.mode {
            ChannelMode::InterruptOnTerminalCount => {
                if self.count == 0 {
                    self.status.output = true;
                    return false;
                }
                self.count = self.count.wrapping_sub(1);
                if self.count == 0 {
                    self.status.output = true;
                    return true;
                }
                false
            }
            ChannelMode::RateGenerator => {
                if self.count == 0 {
                    self.count = self.reload;
                    return false;
                }
                self.count = self.count.wrapping_sub(1);
                if self.count == 1 {
                    self.status.output = false;
                    self.count = self.reload;
                    return true;
                }
                self.status.output = true;
                false
            }
            ChannelMode::SquareWave => {
                if self.count == 0 {
                    self.count = self.reload;
                    return false;
                }
                self.count = self.count.wrapping_sub(2);
                if self.count <= 1 {
                    self.status.output = !self.status.output;
                    self.count = self.reload;
                    return self.status.output;
                }
                false
            }
            _ => {
                if self.count > 0 {
                    self.count = self.count.wrapping_sub(1);
                }
                false
            }
        }
    }
}

/// The full 8254 PIT with 3 channels.
pub struct Pit {
    pub channels: [PitChannel; 3],
    accumulator_ns: u64,
}

impl Pit {
    /// Create a new `Pit` with all channels in default state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            channels: [PitChannel::new(), PitChannel::new(), PitChannel::new()],
            accumulator_ns: 0,
        }
    }

    /// Read from a PIT I/O port (0x40-0x42).
    #[must_use]
    pub fn read_port(&mut self, port: u16) -> u8 {
        let channel = (port & 0x3) as usize;
        if channel < 3 {
            self.channels[channel].read_data()
        } else {
            0
        }
    }

    /// Write to a PIT I/O port (0x40-0x43).
    pub fn write_port(&mut self, port: u16, val: u8) {
        if port == 0x43 {
            self.write_command(val);
        } else {
            let channel = (port & 0x3) as usize;
            if channel < 3 {
                self.channels[channel].write_data(val);
            }
        }
    }

    fn write_command(&mut self, val: u8) {
        let channel_idx = ((val >> 6) & 0x3) as usize;
        if channel_idx >= 3 {
            self.read_back(val);
            return;
        }

        let access = AccessMode::from_bits((val >> 4) & 0x3);
        if access == AccessMode::Latch {
            self.channels[channel_idx].latched_count = Some(self.channels[channel_idx].count);
            return;
        }

        let mode = ChannelMode::from_bits((val >> 1) & 0x7);
        let ch = &mut self.channels[channel_idx];
        ch.access = access;
        ch.mode = mode;
        ch.byte_latch.read_hi = false;
        ch.byte_latch.write_hi = false;
        ch.status.output = false;
        ch.enabled = false;
        // Control word written, initial count not yet loaded.
        ch.status.null_count = true;
    }

    /// Handle the 8254 read-back command (control word with bits 7-6 = `11`).
    ///
    /// Bit 5 low (`/COUNT`) latches the current count of every selected channel;
    /// bit 4 low (`/STATUS`) latches a status byte. Bits 3-1 select channels
    /// 2/1/0. When both are requested the status byte is delivered first on the
    /// next data-port read, then the latched count — matching real hardware.
    fn read_back(&mut self, val: u8) {
        let latch_count = val & 0x20 == 0;
        let latch_status = val & 0x10 == 0;
        for (idx, ch) in self.channels.iter_mut().enumerate() {
            if val & (1u8 << (idx + 1)) == 0 {
                continue;
            }
            // Per the datasheet, a status latch already pending is not overwritten
            // by a second read-back until it has been read.
            if latch_status && ch.latched_status.is_none() {
                ch.latched_status = Some(ch.status_byte());
            }
            if latch_count && ch.latched_count.is_none() {
                ch.latched_count = Some(ch.count);
            }
        }
    }

    /// Advance the PIT by `ns` nanoseconds.
    ///
    /// Returns `true` if channel 0 generated an interrupt (IRQ 0).
    #[must_use]
    pub fn tick(&mut self, ns: u64) -> bool {
        self.accumulator_ns += ns;
        let ticks = self.accumulator_ns / NS_PER_TICK;
        self.accumulator_ns %= NS_PER_TICK;

        let mut irq = false;
        for _ in 0..ticks {
            if self.channels[0].tick() {
                irq = true;
            }
            self.channels[1].tick();
            self.channels[2].tick();
        }
        irq
    }

    /// Get the current frequency of channel 0 in Hz.
    #[must_use]
    pub fn channel0_frequency(&self) -> u32 {
        let reload = self.channels[0].reload;
        if reload == 0 {
            PIT_FREQUENCY / 65_536
        } else {
            PIT_FREQUENCY / u32::from(reload)
        }
    }
}

impl Default for Pit {
    fn default() -> Self {
        Self::new()
    }
}

/// Mounts the 8254 on the port-I/O bus over `0x40..=0x43`.
///
/// The PIT registers are byte-wide, so a guest accesses them one byte at a time;
/// reads return the value in the low byte of the `u32` and writes consume the
/// low byte. The bus only routes ports within the declared range here, so every
/// `port` is one of the four PIT ports.
impl PioDevice for Pit {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        u32::from(self.read_port(port))
    }

    fn pio_write(&mut self, port: u16, _size: u8, data: u32) {
        self.write_port(port, data.to_le_bytes()[0]);
    }

    fn port_range(&self) -> (u16, u16) {
        (PIT_PORT_BASE, PIT_PORT_BASE + PIT_PORT_COUNT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pit_creation() {
        let pit = Pit::new();
        assert_eq!(pit.channels[0].count, 0);
        assert!(!pit.channels[0].enabled);
    }

    #[test]
    fn test_pit_command_write() {
        let mut pit = Pit::new();
        // Channel 0, lo/hi access, mode 2 (rate generator)
        pit.write_port(0x43, 0x34); // 0b00_11_010_0
        assert_eq!(pit.channels[0].access, AccessMode::LoHiByte);
        assert_eq!(pit.channels[0].mode, ChannelMode::RateGenerator);
    }

    #[test]
    fn test_pit_count_write() {
        let mut pit = Pit::new();
        pit.write_port(0x43, 0x34); // Channel 0, lo/hi, mode 2
        pit.write_port(0x40, 0x00); // Low byte
        pit.write_port(0x40, 0x04); // High byte -> reload = 0x0400 = 1024
        assert_eq!(pit.channels[0].reload, 0x0400);
        assert!(pit.channels[0].enabled);
    }

    #[test]
    fn test_pit_latch() {
        let mut pit = Pit::new();
        pit.write_port(0x43, 0x34);
        pit.write_port(0x40, 0x00);
        pit.write_port(0x40, 0x04);
        // Latch channel 0
        pit.write_port(0x43, 0x00);
        assert!(pit.channels[0].latched_count.is_some());
    }

    #[test]
    fn test_pit_tick_generates_irq() {
        let mut pit = Pit::new();
        // Channel 0, lo/hi, mode 3 (square wave)
        pit.write_port(0x43, 0x36);
        pit.write_port(0x40, 0x02); // Low byte
        pit.write_port(0x40, 0x00); // High byte -> reload = 2
        // Tick enough nanoseconds for multiple PIT cycles
        let irq = pit.tick(NS_PER_TICK * 4);
        assert!(irq);
    }

    #[test]
    fn test_pit_channel0_frequency() {
        let mut pit = Pit::new();
        pit.write_port(0x43, 0x34);
        // Set reload to 1193 (approximately 1000 Hz)
        pit.write_port(0x40, (0x4A9 & 0xFF) as u8);
        pit.write_port(0x40, ((0x4A9 >> 8) & 0xFF) as u8);
        let freq = pit.channel0_frequency();
        assert!((999..=1001).contains(&freq));
    }

    #[test]
    fn test_pio_device_claims_four_command_ports() {
        let pit = Pit::new();
        assert_eq!(PioDevice::port_range(&pit), (0x40, 0x44));
    }

    #[test]
    fn test_pio_write_programs_channel_low_byte_only() {
        let mut pit = Pit::new();
        // Command: channel 0, lo/hi access, mode 2. Only the low byte counts.
        pit.pio_write(0x43, 1, 0xFFFF_FF34);
        assert_eq!(pit.channels[0].mode, ChannelMode::RateGenerator);
        // Reload low byte then high byte via the data port.
        pit.pio_write(0x40, 1, 0x9A);
        pit.pio_write(0x40, 1, 0x02);
        assert_eq!(pit.channels[0].reload, 0x029A);
    }

    #[test]
    fn test_pio_read_returns_count_in_low_byte() {
        let mut pit = Pit::new();
        pit.pio_write(0x43, 1, 0x34); // ch0, lo/hi, mode 2
        pit.pio_write(0x40, 1, 0x34);
        pit.pio_write(0x40, 1, 0x12); // reload = 0x1234
        // Latch then read back lo, hi through the PIO path.
        pit.pio_write(0x43, 1, 0x00); // latch channel 0
        assert_eq!(pit.pio_read(0x40, 1) & 0xFF, 0x34);
        assert_eq!(pit.pio_read(0x40, 1) & 0xFF, 0x12);
    }

    #[test]
    fn test_read_back_latches_status_before_count() {
        let mut pit = Pit::new();
        // Program channel 0: lo/hi access, mode 2 (rate generator), then load.
        pit.write_port(0x43, 0x34);
        pit.write_port(0x40, 0x10);
        pit.write_port(0x40, 0x00); // reload = 0x0010, count loaded
        // Read-back: latch both status and count of channel 0.
        // 0b11_0_0_001_0 = 0xC2 (/STATUS=0, /COUNT=0, select ch0).
        pit.write_port(0x43, 0xC2);
        // Status byte comes first: access=LoHiByte (0b11<<4), mode=2 (010<<1).
        let status = pit.read_port(0x40);
        assert_eq!(status & 0x30, 0x30, "RW field should report lo/hi access");
        assert_eq!((status >> 1) & 0x7, 2, "mode field should report rate-gen");
        assert_eq!(status & 0x40, 0, "null-count clear after count loaded");
        // Then the latched count's low byte.
        assert_eq!(pit.read_port(0x40), 0x10);
    }

    #[test]
    fn test_read_back_null_count_set_before_load() {
        let mut pit = Pit::new();
        // Control word written but no count loaded yet -> null count is set.
        pit.write_port(0x43, 0x34);
        pit.write_port(0x43, 0xE2); // read-back status only (/STATUS=0), ch0
        let status = pit.read_port(0x40);
        assert_ne!(status & 0x40, 0, "null-count bit set before initial load");
    }

    #[test]
    fn test_read_back_only_selected_channels() {
        let mut pit = Pit::new();
        pit.write_port(0x43, 0x34); // ch0 program
        pit.write_port(0x43, 0xB6); // ch2 program (lo/hi, mode 3)
        // Read-back status of channel 2 only (bit3 set, bit1 clear): 0xE8.
        pit.write_port(0x43, 0xE8);
        assert!(pit.channels[0].latched_status.is_none());
        assert!(pit.channels[2].latched_status.is_some());
    }
}

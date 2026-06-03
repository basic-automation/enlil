//! Intel 8254 Programmable Interval Timer (PIT) emulation.
//!
//! The PIT has 3 channels:
//! - Channel 0: System timer (IRQ 0)
//! - Channel 1: DRAM refresh (legacy, unused)
//! - Channel 2: PC speaker
//!
//! I/O ports: 0x40-0x43 (channels 0-2 data, 0x43 command)

use crate::truncate::u16_of;
/// PIT oscillator frequency in Hz.
pub const PIT_FREQUENCY: u32 = 1_193_182;

/// Nanoseconds per PIT tick.
const NS_PER_TICK: u64 = 838;

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

#[derive(Debug, Clone)]
pub struct PitChannel {
    pub count: u16,
    pub reload: u16,
    pub mode: ChannelMode,
    pub access: AccessMode,
    pub latched_count: Option<u16>,
    pub byte_latch: ByteLatch,
    pub gate: bool,
    pub output: bool,
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
            byte_latch: ByteLatch {
                read_hi: false,
                write_hi: false,
            },
            gate: true,
            output: false,
            enabled: false,
        }
    }

    fn read_data(&mut self) -> u8 {
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
        self.output = false;
    }

    const fn tick(&mut self) -> bool {
        if !self.enabled || !self.gate {
            return false;
        }

        match self.mode {
            ChannelMode::InterruptOnTerminalCount => {
                if self.count == 0 {
                    self.output = true;
                    return false;
                }
                self.count = self.count.wrapping_sub(1);
                if self.count == 0 {
                    self.output = true;
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
                    self.output = false;
                    self.count = self.reload;
                    return true;
                }
                self.output = true;
                false
            }
            ChannelMode::SquareWave => {
                if self.count == 0 {
                    self.count = self.reload;
                    return false;
                }
                self.count = self.count.wrapping_sub(2);
                if self.count <= 1 {
                    self.output = !self.output;
                    self.count = self.reload;
                    return self.output;
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
            return; // Read-back command, ignore for now
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
        ch.output = false;
        ch.enabled = false;
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
}

//! Intel 8237A DMA controllers + the PC/AT DMA page registers.
//!
//! A PC/AT cascades two 8237As behind the chipset:
//! - **DMA-1** (8-bit, channels 0-3) at I/O ports `0x00-0x0F`.
//! - **DMA-2** (16-bit, channels 4-7) at `0xC0-0xDF`, with its registers on
//!   2-byte spacing (`offset = (port - 0xC0) >> 1`). Channel 4 (its channel 0) is
//!   the cascade input for DMA-1 and is never used for a real transfer.
//!
//! The high address bits (A16-A23) live in the separate **DMA page registers**
//! at `0x80-0x8F` ([`DmaPageRegisters`]).
//!
//! Enlil drives no real ISA DMA today — nothing in-tree (no floppy, no SB16) owns
//! a channel — so this is a faithful *passive register model*, not a transfer
//! engine. Its job is transparency: a guest that `request_region`s and probes ISA
//! DMA at boot (Linux always does, via `reserve_dma_pages`/`dma_init`) must read
//! back coherent register state instead of open-bus `0xFF`, which would otherwise
//! be a cheap VM tell. Each channel's base/current address and count registers,
//! the shared byte-pointer flip-flop, and the command/status/request/mask/mode
//! registers and master clear are all modelled per the 8237A datasheet.

use crate::bus::PioDevice;
use crate::truncate::u8_of;

/// First port DMA-1 (8-bit, channels 0-3) claims.
pub const DMA1_PORT_BASE: u16 = 0x00;
/// First port DMA-2 (16-bit, channels 4-7) claims.
pub const DMA2_PORT_BASE: u16 = 0xC0;
/// First port the DMA page registers claim.
pub const DMA_PAGE_PORT_BASE: u16 = 0x80;
/// Number of contiguous ports the page-register file claims (`0x80..=0x8F`).
pub const DMA_PAGE_PORT_COUNT: u16 = 0x10;

/// A single 8237A channel.
///
/// Holds a base/current address and base/current count plus the channel's mode
/// byte. Both halves of an address or count are reached one byte at a time
/// through the controller's shared byte-pointer flip-flop.
#[derive(Debug, Clone, Default)]
pub struct DmaChannel {
    /// Base (programmed) transfer address; reloaded into `cur_addr` on a write.
    pub base_addr: u16,
    /// Current transfer address (what a read returns).
    pub cur_addr: u16,
    /// Base (programmed) transfer count.
    pub base_count: u16,
    /// Current transfer count (what a read returns).
    pub cur_count: u16,
    /// The channel's mode-register byte (transfer type / direction / autoinit).
    pub mode: u8,
}

/// One 8237A DMA controller (four channels) as a bus [`PioDevice`].
///
/// `port_base`/`stride` place it on the bus: DMA-1 at `0x00` with stride 1, DMA-2
/// at `0xC0` with stride 2 (its registers sit on even ports). The model is
/// passive — it never moves data — so reads return the current register file and
/// writes update it, exactly as a guest's probe expects.
pub struct Dma8237 {
    /// The four channels (0-3 for DMA-1, 4-7 for DMA-2).
    pub channels: [DmaChannel; 4],
    /// Command register (offset 8 write).
    command: u8,
    /// Status register's transfer-complete (lower) nibble; read-and-clear.
    tc_reached: u8,
    /// Request register's pending (upper) nibble.
    request: u8,
    /// One mask bit per channel (set = masked). All set after master clear.
    mask: u8,
    /// Byte-pointer flip-flop: `false` selects the low byte of the next 16-bit
    /// address/count access, `true` the high byte.
    flip_flop: bool,
    /// First port this controller occupies (`0x00` or `0xC0`).
    port_base: u16,
    /// Per-register port spacing (1 for DMA-1, 2 for the 16-bit DMA-2).
    stride: u16,
    /// `true` for the 16-bit secondary controller (kept for callers/inspection).
    is_16bit: bool,
}

impl Dma8237 {
    /// Build a controller at `port_base` with the given register `stride`.
    const fn with_layout(port_base: u16, stride: u16, is_16bit: bool) -> Self {
        Self {
            channels: [
                DmaChannel {
                    base_addr: 0,
                    cur_addr: 0,
                    base_count: 0,
                    cur_count: 0,
                    mode: 0,
                },
                DmaChannel {
                    base_addr: 0,
                    cur_addr: 0,
                    base_count: 0,
                    cur_count: 0,
                    mode: 0,
                },
                DmaChannel {
                    base_addr: 0,
                    cur_addr: 0,
                    base_count: 0,
                    cur_count: 0,
                    mode: 0,
                },
                DmaChannel {
                    base_addr: 0,
                    cur_addr: 0,
                    base_count: 0,
                    cur_count: 0,
                    mode: 0,
                },
            ],
            command: 0,
            tc_reached: 0,
            request: 0,
            // Power-on/master-clear state: every channel masked.
            mask: 0x0F,
            flip_flop: false,
            port_base,
            stride,
            is_16bit,
        }
    }

    /// The 8-bit primary controller (DMA-1): channels 0-3 at ports `0x00-0x0F`.
    #[must_use]
    pub const fn primary() -> Self {
        Self::with_layout(DMA1_PORT_BASE, 1, false)
    }

    /// The 16-bit secondary controller (DMA-2): channels 4-7 at `0xC0-0xDF`,
    /// registers on 2-byte spacing. Its channel 0 cascades DMA-1.
    #[must_use]
    pub const fn secondary() -> Self {
        Self::with_layout(DMA2_PORT_BASE, 2, true)
    }

    /// Whether this is the 16-bit secondary controller.
    #[must_use]
    pub const fn is_16bit(&self) -> bool {
        self.is_16bit
    }

    /// The current mask register (bit *n* set = channel *n* masked).
    #[must_use]
    pub const fn mask(&self) -> u8 {
        self.mask
    }

    /// Reset to the master-clear state: command/status/request cleared, the
    /// flip-flop cleared, and every channel masked (the 8237A's reset behaviour).
    const fn master_clear(&mut self) {
        self.command = 0;
        self.tc_reached = 0;
        self.request = 0;
        self.mask = 0x0F;
        self.flip_flop = false;
    }

    /// Read one byte from controller register `reg` (0-15, already de-strided).
    fn read_reg(&mut self, reg: u8) -> u8 {
        match reg {
            // Channel address (even reg) / count (odd reg), low byte then high.
            0x0..=0x7 => {
                let ch = &self.channels[(reg >> 1) as usize];
                let value = if reg & 1 == 0 {
                    ch.cur_addr
                } else {
                    ch.cur_count
                };
                let byte = if self.flip_flop {
                    u8_of(value >> 8)
                } else {
                    u8_of(value)
                };
                self.flip_flop = !self.flip_flop;
                byte
            }
            // Status register: upper nibble = pending requests, lower = TC flags.
            // Reading clears the (sticky) TC flags, per the datasheet.
            0x8 => {
                let status = (self.request << 4) | (self.tc_reached & 0x0F);
                self.tc_reached = 0;
                status
            }
            // Mask register read-back (some BIOSes probe it via 0x0F).
            0xF => self.mask,
            // The temporary register (0xD) reads 0 — nothing has been
            // transferred — as do the write-only registers.
            _ => 0,
        }
    }

    /// Write one byte to controller register `reg` (0-15, already de-strided).
    fn write_reg(&mut self, reg: u8, val: u8) {
        match reg {
            // Channel address (even) / count (odd): low byte then high byte,
            // loading both the base and the current register (real hardware
            // copies base→current on the programming write).
            0x0..=0x7 => {
                let ch = &mut self.channels[(reg >> 1) as usize];
                let is_count = reg & 1 == 1;
                let cur = if is_count { ch.cur_count } else { ch.cur_addr };
                let next = if self.flip_flop {
                    (u16::from(val) << 8) | (cur & 0x00FF)
                } else {
                    (cur & 0xFF00) | u16::from(val)
                };
                if is_count {
                    ch.cur_count = next;
                    ch.base_count = next;
                } else {
                    ch.cur_addr = next;
                    ch.base_addr = next;
                }
                self.flip_flop = !self.flip_flop;
            }
            // Command register.
            0x8 => self.command = val,
            // Request register: bit 2 sets/clears the request for the channel in
            // bits 0-1.
            0x9 => {
                let ch = val & 0x03;
                if val & 0x04 == 0 {
                    self.request &= !(1 << ch);
                } else {
                    self.request |= 1 << ch;
                }
            }
            // Single-channel mask: bit 2 = mask/unmask the channel in bits 0-1.
            0xA => {
                let ch = val & 0x03;
                if val & 0x04 == 0 {
                    self.mask &= !(1 << ch);
                } else {
                    self.mask |= 1 << ch;
                }
            }
            // Mode register: channel in bits 0-1.
            0xB => {
                let ch = (val & 0x03) as usize;
                self.channels[ch].mode = val;
            }
            // Clear byte-pointer flip-flop.
            0xC => self.flip_flop = false,
            // Master clear.
            0xD => self.master_clear(),
            // Clear mask register: enable (unmask) all four channels.
            0xE => self.mask = 0,
            // Write all four mask bits at once.
            0xF => self.mask = val & 0x0F,
            _ => {}
        }
    }

    /// Translate an absolute guest `port` into the de-strided register index.
    fn reg_of(&self, port: u16) -> u8 {
        u8_of((port - self.port_base) / self.stride)
    }
}

impl PioDevice for Dma8237 {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        u32::from(self.read_reg(self.reg_of(port)))
    }

    fn pio_write(&mut self, port: u16, _size: u8, data: u32) {
        self.write_reg(self.reg_of(port), u8_of(data));
    }

    fn port_range(&self) -> (u16, u16) {
        (self.port_base, self.port_base + 16 * self.stride)
    }
}

/// The PC/AT DMA **page registers** (`0x80-0x8F`) as a bus [`PioDevice`].
///
/// These latch the high address bits (A16-A23) for each DMA channel. The
/// channel→port map is the historical non-linear one
/// (ch0=`0x87`, ch1=`0x83`, ch2=`0x81`, ch3=`0x82`, ch5=`0x8B`, ch6=`0x89`,
/// ch7=`0x8A`, refresh=`0x8F`); the remaining ports
/// (`0x80`/`0x84`/`0x85`/`0x86`/`0x88`/`0x8C`-`0x8E`) are scratch registers, with
/// `0x80` the classic POST diagnostic port. Every port simply reads back what was
/// last written, which is all a probing guest checks.
pub struct DmaPageRegisters {
    regs: [u8; DMA_PAGE_PORT_COUNT as usize],
}

impl DmaPageRegisters {
    /// A fresh page-register file (all zero).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            regs: [0; DMA_PAGE_PORT_COUNT as usize],
        }
    }

    /// The page-register port that latches the high address byte of DMA channel
    /// `ch` (0-7), or `None` for the cascade channel 4 (which has no page).
    #[must_use]
    pub const fn page_port_for_channel(ch: u8) -> Option<u16> {
        match ch {
            0 => Some(0x87),
            1 => Some(0x83),
            2 => Some(0x81),
            3 => Some(0x82),
            5 => Some(0x8B),
            6 => Some(0x89),
            7 => Some(0x8A),
            _ => None,
        }
    }

    /// The latched high address byte (A16-A23) for DMA channel `ch`, or `None`
    /// for a channel with no page register.
    #[must_use]
    pub fn channel_page(&self, ch: u8) -> Option<u8> {
        Self::page_port_for_channel(ch).map(|port| self.regs[(port - DMA_PAGE_PORT_BASE) as usize])
    }
}

impl Default for DmaPageRegisters {
    fn default() -> Self {
        Self::new()
    }
}

impl PioDevice for DmaPageRegisters {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        u32::from(self.regs[(port - DMA_PAGE_PORT_BASE) as usize])
    }

    fn pio_write(&mut self, port: u16, _size: u8, data: u32) {
        self.regs[(port - DMA_PAGE_PORT_BASE) as usize] = u8_of(data);
    }

    fn port_range(&self) -> (u16, u16) {
        (DMA_PAGE_PORT_BASE, DMA_PAGE_PORT_BASE + DMA_PAGE_PORT_COUNT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_claims_low_sixteen_ports() {
        let dma = Dma8237::primary();
        assert_eq!(PioDevice::port_range(&dma), (0x00, 0x10));
        assert!(!dma.is_16bit());
    }

    #[test]
    fn secondary_claims_strided_window() {
        let dma = Dma8237::secondary();
        // 16 registers at 2-byte spacing => 0xC0..0xE0.
        assert_eq!(PioDevice::port_range(&dma), (0xC0, 0xE0));
        assert!(dma.is_16bit());
    }

    #[test]
    fn reset_state_masks_all_channels() {
        let dma = Dma8237::primary();
        assert_eq!(dma.mask(), 0x0F);
    }

    #[test]
    fn address_register_round_trips_through_flip_flop() {
        let mut dma = Dma8237::primary();
        // Clear flip-flop, then program channel-1 base address low+high = 0x1234.
        dma.pio_write(0x0C, 1, 0); // clear byte-pointer flip-flop
        dma.pio_write(0x02, 1, 0x34); // ch1 addr, low byte
        dma.pio_write(0x02, 1, 0x12); // ch1 addr, high byte
        assert_eq!(dma.channels[1].cur_addr, 0x1234);
        assert_eq!(dma.channels[1].base_addr, 0x1234);
        // Read it back, low byte then high byte.
        dma.pio_write(0x0C, 1, 0);
        assert_eq!(dma.pio_read(0x02, 1) & 0xFF, 0x34);
        assert_eq!(dma.pio_read(0x02, 1) & 0xFF, 0x12);
    }

    #[test]
    fn count_register_round_trips() {
        let mut dma = Dma8237::primary();
        dma.pio_write(0x0C, 1, 0);
        dma.pio_write(0x03, 1, 0xFF); // ch1 count low
        dma.pio_write(0x03, 1, 0x01); // ch1 count high => 0x01FF
        assert_eq!(dma.channels[1].cur_count, 0x01FF);
        assert_eq!(dma.channels[1].base_count, 0x01FF);
    }

    #[test]
    fn secondary_decodes_strided_ports() {
        let mut dma = Dma8237::secondary();
        // Channel-5 (controller channel 1) address lives at register 2 => port
        // 0xC0 + 2*2 = 0xC4.
        dma.pio_write(0xC0 + 0x0C * 2, 1, 0); // clear flip-flop (reg 0x0C)
        dma.pio_write(0xC4, 1, 0xAA);
        dma.pio_write(0xC4, 1, 0xBB);
        assert_eq!(dma.channels[1].cur_addr, 0xBBAA);
    }

    #[test]
    fn single_mask_bit_sets_and_clears() {
        let mut dma = Dma8237::primary();
        dma.pio_write(0x0E, 1, 0); // clear-mask: unmask all
        assert_eq!(dma.mask(), 0x00);
        dma.pio_write(0x0A, 1, 0x06); // mask channel 2 (bits: set=0x04 | ch=0x02)
        assert_eq!(dma.mask(), 0x04);
        dma.pio_write(0x0A, 1, 0x02); // unmask channel 2
        assert_eq!(dma.mask(), 0x00);
    }

    #[test]
    fn write_all_mask_bits() {
        let mut dma = Dma8237::primary();
        dma.pio_write(0x0F, 1, 0x0A);
        assert_eq!(dma.mask(), 0x0A);
        // Read-back of register 0x0F returns the mask.
        assert_eq!(dma.pio_read(0x0F, 1) & 0xFF, 0x0A);
    }

    #[test]
    fn mode_register_targets_selected_channel() {
        let mut dma = Dma8237::primary();
        // Mode byte 0x58: channel 0, single mode, read transfer, autoinit, etc.
        dma.pio_write(0x0B, 1, 0x58);
        assert_eq!(dma.channels[0].mode, 0x58);
        // Channel 3 selected by bits 0-1 = 0b11.
        dma.pio_write(0x0B, 1, 0x47);
        assert_eq!(dma.channels[3].mode, 0x47);
    }

    #[test]
    fn request_register_shows_in_status_upper_nibble() {
        let mut dma = Dma8237::primary();
        dma.pio_write(0x09, 1, 0x05); // set request for channel 1 (0x04 | 0x01)
        let status = dma.pio_read(0x08, 1) & 0xFF;
        assert_eq!(status & 0xF0, 0x20, "channel-1 request bit in upper nibble");
        dma.pio_write(0x09, 1, 0x01); // clear request for channel 1
        let status = dma.pio_read(0x08, 1) & 0xFF;
        assert_eq!(status & 0xF0, 0x00);
    }

    #[test]
    fn master_clear_resets_and_masks() {
        let mut dma = Dma8237::primary();
        dma.pio_write(0x0E, 1, 0); // unmask all
        dma.pio_write(0x08, 1, 0x04); // set a command bit
        dma.pio_write(0x02, 1, 0x34); // advance the flip-flop
        dma.pio_write(0x0D, 1, 0); // master clear
        assert_eq!(dma.mask(), 0x0F, "all channels masked after master clear");
        // Flip-flop is cleared, so the next address byte is the low byte.
        dma.pio_write(0x00, 1, 0x77);
        assert_eq!(dma.channels[0].cur_addr & 0xFF, 0x77);
    }

    #[test]
    fn temporary_register_reads_zero() {
        let mut dma = Dma8237::primary();
        assert_eq!(dma.pio_read(0x0D, 1) & 0xFF, 0x00);
    }

    #[test]
    fn page_registers_claim_window_and_round_trip() {
        let mut pages = DmaPageRegisters::new();
        assert_eq!(PioDevice::port_range(&pages), (0x80, 0x90));
        pages.pio_write(0x81, 1, 0xC3); // channel-2 page
        assert_eq!(pages.pio_read(0x81, 1) & 0xFF, 0xC3);
        assert_eq!(pages.channel_page(2), Some(0xC3));
    }

    #[test]
    fn page_port_map_matches_pc_at_layout() {
        assert_eq!(DmaPageRegisters::page_port_for_channel(0), Some(0x87));
        assert_eq!(DmaPageRegisters::page_port_for_channel(1), Some(0x83));
        assert_eq!(DmaPageRegisters::page_port_for_channel(2), Some(0x81));
        assert_eq!(DmaPageRegisters::page_port_for_channel(3), Some(0x82));
        assert_eq!(DmaPageRegisters::page_port_for_channel(5), Some(0x8B));
        assert_eq!(DmaPageRegisters::page_port_for_channel(6), Some(0x89));
        assert_eq!(DmaPageRegisters::page_port_for_channel(7), Some(0x8A));
        // Channel 4 is the cascade — no page register.
        assert_eq!(DmaPageRegisters::page_port_for_channel(4), None);
    }

    #[test]
    fn scratch_page_ports_are_independent() {
        let mut pages = DmaPageRegisters::new();
        pages.pio_write(0x80, 1, 0x55); // POST diagnostic / scratch
        pages.pio_write(0x88, 1, 0xAA);
        assert_eq!(pages.pio_read(0x80, 1) & 0xFF, 0x55);
        assert_eq!(pages.pio_read(0x88, 1) & 0xFF, 0xAA);
        // A scratch port is not any channel's page.
        assert_eq!(pages.channel_page(0), Some(0x00));
    }
}

//! HDA controller (Intel HD Audio) MMIO register emulation
//!
//! Implements the Intel HDA controller register set exposed via PCI BAR0.
//! Windows loads hdaudio.sys when it detects this device.

use super::codec::HdaCodec;

/// HDA controller MMIO register offsets (Intel HD Audio spec section 3)
pub mod regs {
    pub const GCAP: u32 = 0x00; // Global Capabilities
    pub const VMIN: u32 = 0x02; // Minor Version
    pub const VMAJ: u32 = 0x03; // Major Version
    pub const OUTPAY: u32 = 0x04; // Output Payload Capability
    pub const INPAY: u32 = 0x06; // Input Payload Capability
    pub const GCTL: u32 = 0x08; // Global Control
    pub const WAKEEN: u32 = 0x0C; // Wake Enable
    pub const STATESTS: u32 = 0x0E; // State Change Status
    pub const GSTS: u32 = 0x10; // Global Status
    pub const INTCTL: u32 = 0x20; // Interrupt Control
    pub const INTSTS: u32 = 0x24; // Interrupt Status
    pub const WALCLK: u32 = 0x30; // Wall Clock Counter
    pub const SSYNC: u32 = 0x38; // Stream Synchronization

    // CORB (Command Output Ring Buffer)
    pub const CORBLBASE: u32 = 0x40;
    pub const CORBUBASE: u32 = 0x44;
    pub const CORBWP: u32 = 0x48; // Write Pointer
    pub const CORBRP: u32 = 0x4A; // Read Pointer
    pub const CORBCTL: u32 = 0x4C; // Control
    pub const CORBSTS: u32 = 0x4D; // Status
    pub const CORBSIZE: u32 = 0x4E; // Size

    // RIRB (Response Input Ring Buffer)
    pub const RIRBLBASE: u32 = 0x50;
    pub const RIRBUBASE: u32 = 0x54;
    pub const RIRBWP: u32 = 0x58; // Write Pointer
    pub const RINTCNT: u32 = 0x5A; // Response Interrupt Count
    pub const RIRBCTL: u32 = 0x5C; // Control
    pub const RIRBSTS: u32 = 0x5D; // Status
    pub const RIRBSIZE: u32 = 0x5E; // Size

    // Immediate Command
    pub const IC: u32 = 0x60; // Immediate Command
    pub const IR: u32 = 0x64; // Immediate Response
    pub const ICS: u32 = 0x68; // Immediate Command Status

    // Stream Descriptor base (first output stream)
    pub const SD0_BASE: u32 = 0x80;
    pub const SD_SIZE: u32 = 0x20; // Each stream descriptor is 0x20 bytes
}

/// Stream descriptor register offsets (relative to stream base)
pub mod sd_regs {
    pub const CTL: u32 = 0x00; // Stream Descriptor Control (3 bytes)
    pub const STS: u32 = 0x03; // Status
    pub const LPIB: u32 = 0x04; // Link Position in Buffer
    pub const CBL: u32 = 0x08; // Cyclic Buffer Length
    pub const LVI: u32 = 0x0C; // Last Valid Index
    pub const FMT: u32 = 0x12; // Format
    pub const BDPL: u32 = 0x18; // BDL Pointer Lower
    pub const BDPU: u32 = 0x1C; // BDL Pointer Upper
}

/// Number of output streams
const NUM_OUTPUT_STREAMS: usize = 4;
/// Number of input streams
const NUM_INPUT_STREAMS: usize = 4;

/// Per-stream descriptor state
#[derive(Debug, Clone, Default)]
pub struct StreamDescriptor {
    pub ctl: u32,
    pub sts: u8,
    pub lpib: u32,
    pub cbl: u32,
    pub lvi: u16,
    pub fmt: u16,
    pub bdl_lower: u32,
    pub bdl_upper: u32,
}

/// Intel HDA controller state
#[derive(Debug, Clone)]
pub struct HdaController {
    // Global registers
    pub gcap: u16,
    pub vmin: u8,
    pub vmaj: u8,
    pub outpay: u16,
    pub inpay: u16,
    pub gctl: u32,
    pub wakeen: u16,
    pub statests: u16,
    pub gsts: u16,
    pub intctl: u32,
    pub intsts: u32,
    pub walclk: u32,
    pub ssync: u32,

    // CORB
    pub corb_base: u64,
    pub corb_wp: u16,
    pub corb_rp: u16,
    pub corb_ctl: u8,
    pub corb_sts: u8,
    pub corb_size: u8,

    // RIRB
    pub rirb_base: u64,
    pub rirb_wp: u16,
    pub rintcnt: u16,
    pub rirb_ctl: u8,
    pub rirb_sts: u8,
    pub rirb_size: u8,

    // Immediate command interface
    pub ic: u32,
    pub ir: u32,
    pub ics: u16,

    // Stream descriptors
    pub output_streams: [StreamDescriptor; NUM_OUTPUT_STREAMS],
    pub input_streams: [StreamDescriptor; NUM_INPUT_STREAMS],

    // Codec
    pub codec: HdaCodec,

    /// IRQ pending
    pub irq_pending: bool,
}

impl HdaController {
    #[must_use]
    pub fn new() -> Self {
        // GCAP: 4 output streams, 4 input streams, 64-bit addressing, serial bus number 0
        let gcap: u16 =
            ((NUM_OUTPUT_STREAMS as u16) << 12) | ((NUM_INPUT_STREAMS as u16) << 8) | 0x01; // 64-bit

        Self {
            gcap,
            vmin: 0,
            vmaj: 1, // HDA 1.0
            outpay: 0x003C,
            inpay: 0x001D,
            gctl: 0,
            wakeen: 0,
            statests: 0,
            gsts: 0,
            intctl: 0,
            intsts: 0,
            walclk: 0,
            ssync: 0,

            corb_base: 0,
            corb_wp: 0,
            corb_rp: 0,
            corb_ctl: 0,
            corb_sts: 0,
            corb_size: 0x02, // 256 entries supported

            rirb_base: 0,
            rirb_wp: 0,
            rintcnt: 1,
            rirb_ctl: 0,
            rirb_sts: 0,
            rirb_size: 0x02,

            ic: 0,
            ir: 0,
            ics: 0,

            output_streams: Default::default(),
            input_streams: Default::default(),

            codec: HdaCodec::new_realtek(),
            irq_pending: false,
        }
    }

    /// Handle MMIO read at offset from BAR0
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn read(&self, offset: u32, _size: u8) -> u64 {
        match offset {
            regs::GCAP => u64::from(self.gcap),
            regs::VMIN => u64::from(self.vmin),
            regs::VMAJ => u64::from(self.vmaj),
            regs::OUTPAY => u64::from(self.outpay),
            regs::INPAY => u64::from(self.inpay),
            regs::GCTL => u64::from(self.gctl),
            regs::WAKEEN => u64::from(self.wakeen),
            regs::STATESTS => u64::from(self.statests),
            regs::GSTS => u64::from(self.gsts),
            regs::INTCTL => u64::from(self.intctl),
            regs::INTSTS => u64::from(self.intsts),
            regs::WALCLK => u64::from(self.walclk),
            regs::SSYNC => u64::from(self.ssync),

            regs::CORBLBASE => self.corb_base & 0xFFFF_FFFF,
            regs::CORBUBASE => self.corb_base >> 32,
            regs::CORBWP => u64::from(self.corb_wp),
            regs::CORBRP => u64::from(self.corb_rp),
            regs::CORBCTL => u64::from(self.corb_ctl),
            regs::CORBSTS => u64::from(self.corb_sts),
            regs::CORBSIZE => u64::from(self.corb_size),

            regs::RIRBLBASE => self.rirb_base & 0xFFFF_FFFF,
            regs::RIRBUBASE => self.rirb_base >> 32,
            regs::RIRBWP => u64::from(self.rirb_wp),
            regs::RINTCNT => u64::from(self.rintcnt),
            regs::RIRBCTL => u64::from(self.rirb_ctl),
            regs::RIRBSTS => u64::from(self.rirb_sts),
            regs::RIRBSIZE => u64::from(self.rirb_size),

            regs::IC => u64::from(self.ic),
            regs::IR => u64::from(self.ir),
            regs::ICS => u64::from(self.ics),

            // Stream descriptors
            o if o >= regs::SD0_BASE => self.read_stream_descriptor(o),

            _ => 0,
        }
    }

    /// Handle MMIO write at offset from BAR0
    #[allow(clippy::cast_possible_truncation)]
    pub fn write(&mut self, offset: u32, value: u64, _size: u8) {
        match offset {
            regs::GCTL => {
                let val = value as u32;
                // Bit 0: Controller Reset (CRST)
                if val & 1 != 0 && self.gctl & 1 == 0 {
                    // Coming out of reset
                    self.statests = 0x01; // Codec 0 present
                }
                self.gctl = val;
            }
            regs::WAKEEN => self.wakeen = value as u16,
            regs::STATESTS => {
                // Write-1-to-clear
                self.statests &= !(value as u16);
            }
            regs::GSTS => {
                self.gsts &= !(value as u16);
            }
            regs::INTCTL => self.intctl = value as u32,
            regs::INTSTS => {
                // Write-1-to-clear
                self.intsts &= !(value as u32);
            }
            regs::SSYNC => self.ssync = value as u32,

            regs::CORBLBASE => {
                self.corb_base = (self.corb_base & 0xFFFF_FFFF_0000_0000) | (value & 0xFFFF_FFFF);
            }
            regs::CORBUBASE => {
                self.corb_base =
                    (self.corb_base & 0x0000_0000_FFFF_FFFF) | ((value & 0xFFFF_FFFF) << 32);
            }
            regs::CORBWP => self.corb_wp = value as u16,
            regs::CORBRP => {
                // Bit 15: reset read pointer
                if value & 0x8000 != 0 {
                    self.corb_rp = 0;
                }
            }
            regs::CORBCTL => self.corb_ctl = value as u8,
            regs::CORBSTS => {
                self.corb_sts &= !(value as u8);
            }
            regs::CORBSIZE => self.corb_size = value as u8 & 0x03,

            regs::RIRBLBASE => {
                self.rirb_base = (self.rirb_base & 0xFFFF_FFFF_0000_0000) | (value & 0xFFFF_FFFF);
            }
            regs::RIRBUBASE => {
                self.rirb_base =
                    (self.rirb_base & 0x0000_0000_FFFF_FFFF) | ((value & 0xFFFF_FFFF) << 32);
            }
            regs::RIRBWP => {
                // Bit 15: reset write pointer
                if value & 0x8000 != 0 {
                    self.rirb_wp = 0;
                }
            }
            regs::RINTCNT => self.rintcnt = value as u16,
            regs::RIRBCTL => self.rirb_ctl = value as u8,
            regs::RIRBSTS => {
                self.rirb_sts &= !(value as u8);
            }
            regs::RIRBSIZE => self.rirb_size = value as u8 & 0x03,

            // Immediate command interface
            regs::IC => {
                self.ic = value as u32;
            }
            regs::ICS => {
                let val = value as u16;
                // Bit 0: Immediate Command Busy (ICB) — set to 1 to execute
                if val & 0x01 != 0 {
                    self.execute_immediate_command();
                }
            }

            // Stream descriptors
            o if o >= regs::SD0_BASE => {
                self.write_stream_descriptor(o, value);
            }

            _ => {}
        }
    }

    /// Execute an immediate verb command via IC/IR registers
    fn execute_immediate_command(&mut self) {
        let response = self.codec.process_verb(self.ic);
        self.ir = response;
        // Clear ICB (bit 0), set IRV (bit 1) — immediate response valid
        self.ics = 0x02;
    }

    /// Read a stream descriptor register
    fn read_stream_descriptor(&self, offset: u32) -> u64 {
        let rel = offset - regs::SD0_BASE;
        let stream_idx = (rel / regs::SD_SIZE) as usize;
        let reg_offset = rel % regs::SD_SIZE;

        let sd = if stream_idx < NUM_INPUT_STREAMS {
            &self.input_streams[stream_idx]
        } else if stream_idx < NUM_INPUT_STREAMS + NUM_OUTPUT_STREAMS {
            &self.output_streams[stream_idx - NUM_INPUT_STREAMS]
        } else {
            return 0;
        };

        match reg_offset {
            sd_regs::CTL => u64::from(sd.ctl & 0x00FF_FFFF),
            sd_regs::STS => u64::from(sd.sts),
            sd_regs::LPIB => u64::from(sd.lpib),
            sd_regs::CBL => u64::from(sd.cbl),
            sd_regs::LVI => u64::from(sd.lvi),
            sd_regs::FMT => u64::from(sd.fmt),
            sd_regs::BDPL => u64::from(sd.bdl_lower),
            sd_regs::BDPU => u64::from(sd.bdl_upper),
            _ => 0,
        }
    }

    /// Write a stream descriptor register
    #[allow(clippy::cast_possible_truncation)]
    const fn write_stream_descriptor(&mut self, offset: u32, value: u64) {
        let rel = offset - regs::SD0_BASE;
        let stream_idx = (rel / regs::SD_SIZE) as usize;
        let reg_offset = rel % regs::SD_SIZE;

        let sd = if stream_idx < NUM_INPUT_STREAMS {
            &mut self.input_streams[stream_idx]
        } else if stream_idx < NUM_INPUT_STREAMS + NUM_OUTPUT_STREAMS {
            &mut self.output_streams[stream_idx - NUM_INPUT_STREAMS]
        } else {
            return;
        };

        match reg_offset {
            sd_regs::CTL => {
                let val = value as u32 & 0x00FF_FFFF;
                // Bit 1: Stream Reset
                if val & 0x02 != 0 {
                    sd.lpib = 0;
                    sd.sts = 0;
                }
                sd.ctl = val;
            }
            sd_regs::STS => {
                // Write-1-to-clear
                sd.sts &= !(value as u8);
            }
            sd_regs::CBL => sd.cbl = value as u32,
            sd_regs::LVI => sd.lvi = value as u16,
            sd_regs::FMT => sd.fmt = value as u16,
            sd_regs::BDPL => sd.bdl_lower = value as u32,
            sd_regs::BDPU => sd.bdl_upper = value as u32,
            _ => {}
        }
    }

    /// Get PCI vendor/device ID for this controller
    #[must_use]
    pub const fn pci_ids() -> (u16, u16) {
        // Intel Cannon Lake HD Audio Controller
        (0x8086, 0xA348)
    }
}

impl Default for HdaController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let ctrl = HdaController::new();
        assert_eq!(ctrl.vmaj, 1);
        assert_eq!(ctrl.gctl, 0);
        assert_eq!(ctrl.read(regs::GCAP, 2), u64::from(ctrl.gcap));
    }

    #[test]
    fn controller_reset() {
        let mut ctrl = HdaController::new();
        assert_eq!(ctrl.gctl & 1, 0); // In reset
        ctrl.write(regs::GCTL, 1, 4); // Come out of reset
        assert_eq!(ctrl.gctl & 1, 1);
        assert_eq!(ctrl.statests, 0x01); // Codec 0 detected
    }

    #[test]
    fn immediate_command() {
        let mut ctrl = HdaController::new();
        ctrl.write(regs::GCTL, 1, 4); // Reset
        // Send verb: Get Parameter (root node, vendor ID)
        ctrl.write(regs::IC, 0x000F_0000, 4);
        ctrl.write(regs::ICS, 0x01, 2); // Execute
        assert_eq!(ctrl.ics & 0x02, 0x02); // IRV set
        assert_ne!(ctrl.ir, 0); // Should have a response
    }

    #[test]
    fn corb_rirb_setup() {
        let mut ctrl = HdaController::new();
        ctrl.write(regs::CORBLBASE, 0x1000_0000, 4);
        ctrl.write(regs::CORBUBASE, 0x0000_0001, 4);
        assert_eq!(ctrl.corb_base, 0x0000_0001_1000_0000);

        // Reset read pointer
        ctrl.write(regs::CORBRP, 0x8000, 2);
        assert_eq!(ctrl.corb_rp, 0);
    }

    #[test]
    fn stream_descriptor_rw() {
        let mut ctrl = HdaController::new();
        let sd_base = regs::SD0_BASE;

        ctrl.write(sd_base + sd_regs::CBL, 0x1000, 4);
        assert_eq!(ctrl.read(sd_base + sd_regs::CBL, 4), 0x1000);

        ctrl.write(sd_base + sd_regs::LVI, 15, 2);
        assert_eq!(ctrl.read(sd_base + sd_regs::LVI, 2), 15);
    }

    #[test]
    fn stream_reset() {
        let mut ctrl = HdaController::new();
        let sd_base = regs::SD0_BASE;

        ctrl.write(sd_base + sd_regs::CTL, 0x02, 4); // Set reset bit
        assert_eq!(ctrl.read(sd_base + sd_regs::LPIB, 4), 0); // LPIB cleared
    }

    #[test]
    fn statests_write_clear() {
        let mut ctrl = HdaController::new();
        ctrl.write(regs::GCTL, 1, 4); // Reset — sets statests
        assert_eq!(ctrl.statests, 0x01);
        ctrl.write(regs::STATESTS, 0x01, 2); // Write-1-to-clear
        assert_eq!(ctrl.statests, 0x00);
    }

    #[test]
    fn pci_ids() {
        let (vendor, _device) = HdaController::pci_ids();
        assert_eq!(vendor, 0x8086);
    }
}

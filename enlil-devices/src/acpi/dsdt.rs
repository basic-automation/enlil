//! DSDT (Differentiated System Description Table) builder
//!
//! Generates a realistic DSDT with AML bytecode that defines the virtual
//! machine's device topology. Windows parses this to discover:
//! - PCI Express root complex
//! - ISA/LPC bridge
//! - RTC, keyboard controller, COM ports
//! - Processor objects
//! - Power management (_S5 sleep state for shutdown)

use crate::truncate::u32_of;
use super::aml::AmlBuilder;
use super::tables::{AcpiSdtHeader, OemInfo};

/// DSDT builder configuration
pub struct DsdtConfig {
    pub vcpu_count: u8,
    pub pci_hole_start: u32,
    pub pci_hole_end: u32,
    pub pci_hole_64_start: u64,
    pub pci_hole_64_size: u64,
    pub com1_port: u16,
    pub com1_irq: u8,
    pub has_hpet: bool,
    pub has_rtc: bool,
    pub has_ps2: bool,
}

impl Default for DsdtConfig {
    fn default() -> Self {
        Self {
            vcpu_count: 4,
            pci_hole_start: 0xC000_0000,
            pci_hole_end: 0xFEBF_FFFF,
            pci_hole_64_start: 0x8_0000_0000,
            pci_hole_64_size: 0x80_0000_0000, // 512 GB
            com1_port: 0x3F8,
            com1_irq: 4,
            has_hpet: true,
            has_rtc: true,
            has_ps2: true,
        }
    }
}

/// DSDT builder
pub struct DsdtBuilder {
    oem: OemInfo,
    config: DsdtConfig,
}

impl DsdtBuilder {
    #[must_use]
    pub fn new(config: DsdtConfig) -> Self {
        Self {
            oem: OemInfo::default(),
            config,
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    /// Generate the AML bytecode for the DSDT
    fn generate_aml(&self) -> Vec<u8> {
        let mut aml = AmlBuilder::new();
        self.build_system_bus(&mut aml);
        self.build_processors(&mut aml);
        self.build_sleep_states(&mut aml);
        aml.into_bytes()
    }

    /// Build \_SB scope with PCI root and ISA devices
    fn build_system_bus(&self, aml: &mut AmlBuilder) {
        let sb = aml.scope_start(b"_SB_");

        // PCI0 — PCI Express Root Complex
        self.build_pci_root(aml);

        aml.scope_end(&sb);
    }

    /// Build PCI Express Root Complex (PCI0)
    fn build_pci_root(&self, aml: &mut AmlBuilder) {
        let pci0 = aml.device_start(b"PCI0");

        // _HID: PCI Express root
        aml.name_string(b"_HID", "PNP0A08");
        // _CID: PCI compatible
        aml.name_string(b"_CID", "PNP0A03");
        // _ADR: 0
        aml.name_integer(b"_ADR", 0);
        // _UID: 0
        aml.name_integer(b"_UID", 0);
        // _BBN: bus base number 0
        aml.name_integer(b"_BBN", 0);

        // _STA: present and functional
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);

        // ISA/LPC bridge
        self.build_isa_bridge(aml);

        aml.device_end(&pci0);
    }

    /// Build ISA/LPC bridge under PCI0
    fn build_isa_bridge(&self, aml: &mut AmlBuilder) {
        let isa = aml.device_start(b"ISA_");

        // ISA bridge at PCI 00:1F.0 (standard Intel ICH location)
        aml.name_integer(b"_ADR", 0x001F_0000);

        // RTC
        if self.config.has_rtc {
            self.build_rtc(aml);
        }

        // PS/2 Keyboard Controller
        if self.config.has_ps2 {
            self.build_ps2(aml);
        }

        // COM1 serial port
        self.build_com1(aml);

        aml.device_end(&isa);
    }

    /// Build RTC device
    fn build_rtc(&self, aml: &mut AmlBuilder) {
        let rtc = aml.device_start(b"RTC_");
        aml.name_string(b"_HID", "PNP0B00");
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&rtc);
    }

    /// Build PS/2 keyboard and mouse
    fn build_ps2(&self, aml: &mut AmlBuilder) {
        // Keyboard
        let kbd = aml.device_start(b"KBD_");
        aml.name_string(b"_HID", "PNP0303");
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&kbd);

        // Mouse
        let mou = aml.device_start(b"MOU_");
        aml.name_string(b"_HID", "PNP0F13");
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&mou);
    }

    /// Build COM1 serial port
    fn build_com1(&self, aml: &mut AmlBuilder) {
        let com1 = aml.device_start(b"COM1");
        aml.name_string(b"_HID", "PNP0501");
        aml.name_integer(b"_UID", 1);
        let sta = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F);
        aml.method_end(&sta);
        aml.device_end(&com1);
    }

    /// Build processor objects (_PR scope)
    fn build_processors(&self, aml: &mut AmlBuilder) {
        let pr = aml.scope_start(b"_PR_");
        for i in 0..self.config.vcpu_count {
            let name = processor_name(i);
            let proc_dev = aml.device_start(&name);
            aml.name_string(b"_HID", "ACPI0007");
            aml.name_integer(b"_UID", u64::from(i));
            let sta = aml.method_start(b"_STA", 0, false);
            aml.return_integer(0x0F);
            aml.method_end(&sta);
            aml.device_end(&proc_dev);
        }
        aml.scope_end(&pr);
    }

    /// Build sleep state objects (\S5 for shutdown)
    fn build_sleep_states(&self, aml: &mut AmlBuilder) {
        // \_S5 (soft off) — required for ACPI shutdown
        aml.name_integer(b"_S5_", 0);
    }

    /// Build the DSDT as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let aml_bytes = self.generate_aml();
        let total_length = 36 + aml_bytes.len();

        let mut buf = Vec::with_capacity(total_length);

        let header = AcpiSdtHeader::new(*b"DSDT", u32_of(total_length), 2, &self.oem);
        buf.extend_from_slice(&header.to_bytes());
        buf.extend_from_slice(&aml_bytes);

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

/// Generate processor name like C000, C001, ..., C00F, C010, etc.
const fn processor_name(index: u8) -> [u8; 4] {
    let hex_chars = b"0123456789ABCDEF";
    [
        b'C',
        hex_chars[((index >> 4) & 0xF) as usize],
        hex_chars[(index & 0xF) as usize],
        b'_',
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsdt_builds() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        assert_eq!(&dsdt[0..4], b"DSDT");
        assert!(dsdt.len() > 36, "DSDT should contain AML data");
    }

    #[test]
    fn dsdt_checksum() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn dsdt_revision() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        assert_eq!(dsdt[8], 2);
    }

    #[test]
    fn dsdt_contains_pci_hid() {
        let dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
        // Should contain PNP0A08 string somewhere in the AML
        let aml = &dsdt[36..];
        let found = aml.windows(7).any(|w| w == b"PNP0A08");
        assert!(found, "DSDT must contain PCI Express root HID");
    }

    #[test]
    fn dsdt_contains_processor_objects() {
        let config = DsdtConfig {
            vcpu_count: 2,
            ..DsdtConfig::default()
        };
        let dsdt = DsdtBuilder::new(config).build();
        let aml = &dsdt[36..];
        // Should contain ACPI0007 (processor device HID)
        let found = aml.windows(8).any(|w| w == b"ACPI0007");
        assert!(found, "DSDT must contain processor device HID");
    }

    #[test]
    fn processor_name_format() {
        assert_eq!(&processor_name(0), b"C00_");
        assert_eq!(&processor_name(1), b"C01_");
        assert_eq!(&processor_name(15), b"C0F_");
        assert_eq!(&processor_name(16), b"C10_");
    }

    #[test]
    fn dsdt_custom_vcpu_count() {
        let config = DsdtConfig {
            vcpu_count: 8,
            ..DsdtConfig::default()
        };
        let dsdt = DsdtBuilder::new(config).build();
        assert!(dsdt.len() > 36);
        let sum: u8 = dsdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }
}

//! FADT (Fixed ACPI Description Table) builder
//!
//! ACPI 6.4 revision 6. Windows requires FADT to find PM timer, SCI interrupt,
//! and the DSDT pointer. We generate a realistic FADT that matches common
//! motherboard firmware output.

use crate::truncate::u32_of;
use super::tables::{AcpiSdtHeader, OemInfo};

/// FADT revision 6 (ACPI 6.4) — 276 bytes total
const FADT_REVISION: u8 = 6;
const FADT_LENGTH: u32 = 276;

/// FADT boot architecture flags
pub mod boot_flags {
    pub const LEGACY_DEVICES: u16 = 1 << 0;
    pub const PS2_8042: u16 = 1 << 1;
    pub const VGA_NOT_PRESENT: u16 = 1 << 2;
    pub const MSI_NOT_SUPPORTED: u16 = 1 << 3;
    pub const PCIE_ASPM_CONTROLS: u16 = 1 << 4;
    pub const CMOS_RTC_NOT_PRESENT: u16 = 1 << 5;
}

/// FADT feature flags (FADT.Flags)
pub mod fadt_flags {
    pub const WBINVD: u32 = 1 << 0;
    pub const WBINVD_FLUSH: u32 = 1 << 1;
    pub const PROC_C1: u32 = 1 << 2;
    pub const P_LVL2_UP: u32 = 1 << 3;
    pub const PWR_BUTTON: u32 = 1 << 4;
    pub const SLP_BUTTON: u32 = 1 << 5;
    pub const FIX_RTC: u32 = 1 << 6;
    pub const RTC_S4: u32 = 1 << 7;
    pub const TMR_VAL_EXT: u32 = 1 << 8;
    pub const DCK_CAP: u32 = 1 << 9;
    pub const RESET_REG_SUP: u32 = 1 << 10;
    pub const SEALED_CASE: u32 = 1 << 11;
    pub const HEADLESS: u32 = 1 << 12;
    pub const CPU_SW_SLP: u32 = 1 << 13;
    pub const PCI_EXP_WAK: u32 = 1 << 14;
    pub const USE_PLATFORM_CLOCK: u32 = 1 << 15;
    pub const S4_RTC_STS_VALID: u32 = 1 << 16;
    pub const REMOTE_POWER_ON: u32 = 1 << 17;
    pub const HW_REDUCED_ACPI: u32 = 1 << 20;
    pub const LOW_POWER_S0: u32 = 1 << 21;
}

/// Generic Address Structure (GAS) — 12 bytes
#[derive(Debug, Clone, Copy)]
pub struct GenericAddress {
    pub address_space: u8,
    pub bit_width: u8,
    pub bit_offset: u8,
    pub access_size: u8,
    pub address: u64,
}

impl GenericAddress {
    #[must_use]
    pub const fn io(address: u64, bit_width: u8) -> Self {
        Self {
            address_space: 1, // System I/O
            bit_width,
            bit_offset: 0,
            access_size: if bit_width <= 8 {
                1
            } else if bit_width <= 16 {
                2
            } else {
                3
            },
            address,
        }
    }

    #[must_use]
    pub const fn mmio(address: u64, bit_width: u8) -> Self {
        Self {
            address_space: 0, // System Memory
            bit_width,
            bit_offset: 0,
            access_size: if bit_width <= 8 {
                1
            } else if bit_width <= 16 {
                2
            } else {
                3
            },
            address,
        }
    }

    #[must_use]
    pub const fn zero() -> Self {
        Self {
            address_space: 0,
            bit_width: 0,
            bit_offset: 0,
            access_size: 0,
            address: 0,
        }
    }

    fn write_to(&self, buf: &mut Vec<u8>) {
        buf.push(self.address_space);
        buf.push(self.bit_width);
        buf.push(self.bit_offset);
        buf.push(self.access_size);
        buf.extend_from_slice(&self.address.to_le_bytes());
    }
}

/// Builder for FADT tables
pub struct FadtBuilder {
    oem: OemInfo,
    dsdt_address: u64,
    sci_interrupt: u16,
    smi_command: u32,
    acpi_enable: u8,
    acpi_disable: u8,
    pm1a_event_block: u32,
    pm1a_control_block: u32,
    pm_timer_block: u32,
    pm_timer_length: u8,
    gpe0_block: u32,
    gpe0_length: u8,
    flags: u32,
    boot_arch_flags: u16,
    reset_register: GenericAddress,
    reset_value: u8,
    hypervisor_vendor_id: u16,
}

impl FadtBuilder {
    #[must_use]
    pub fn new(dsdt_address: u64) -> Self {
        Self {
            oem: OemInfo::default(),
            dsdt_address,
            sci_interrupt: 9,
            smi_command: 0xB2,
            acpi_enable: 0xA0,
            acpi_disable: 0xA1,
            pm1a_event_block: 0x600,
            pm1a_control_block: 0x604,
            pm_timer_block: 0x608,
            pm_timer_length: 4,
            gpe0_block: 0x620,
            gpe0_length: 16,
            flags: fadt_flags::WBINVD
                | fadt_flags::PROC_C1
                | fadt_flags::SLP_BUTTON
                | fadt_flags::TMR_VAL_EXT
                | fadt_flags::RESET_REG_SUP
                | fadt_flags::HW_REDUCED_ACPI,
            boot_arch_flags: boot_flags::LEGACY_DEVICES | boot_flags::PS2_8042,
            reset_register: GenericAddress::io(0x0CF9, 8),
            reset_value: 0x06,
            hypervisor_vendor_id: 0,
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub const fn sci_interrupt(mut self, irq: u16) -> Self {
        self.sci_interrupt = irq;
        self
    }

    #[must_use]
    pub const fn flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    #[must_use]
    pub const fn boot_arch_flags(mut self, flags: u16) -> Self {
        self.boot_arch_flags = flags;
        self
    }

    #[must_use]
    pub const fn pm1a_event_block(mut self, port: u32) -> Self {
        self.pm1a_event_block = port;
        self
    }

    #[must_use]
    pub const fn pm1a_control_block(mut self, port: u32) -> Self {
        self.pm1a_control_block = port;
        self
    }

    #[must_use]
    pub const fn pm_timer_block(mut self, port: u32) -> Self {
        self.pm_timer_block = port;
        self
    }

    /// Build the FADT as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FADT_LENGTH as usize);

        // SDT Header (36 bytes) — placeholder, fixed up at end
        let header = AcpiSdtHeader::new(*b"FACP", FADT_LENGTH, FADT_REVISION, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Offset 36: FIRMWARE_CTRL (4 bytes) — 32-bit physical address of FACS
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Offset 40: DSDT (4 bytes) — 32-bit physical address of DSDT
        buf.extend_from_slice(&(u32_of(self.dsdt_address)).to_le_bytes());
        // Offset 44: Reserved (was INT_MODEL in ACPI 1.0)
        buf.push(0);
        // Offset 45: Preferred PM Profile (2 = Mobile, 1 = Desktop)
        buf.push(1); // Desktop
        // Offset 46: SCI_INT
        buf.extend_from_slice(&self.sci_interrupt.to_le_bytes());
        // Offset 48: SMI_CMD
        buf.extend_from_slice(&self.smi_command.to_le_bytes());
        // Offset 52: ACPI_ENABLE
        buf.push(self.acpi_enable);
        // Offset 53: ACPI_DISABLE
        buf.push(self.acpi_disable);
        // Offset 54: S4BIOS_REQ
        buf.push(0);
        // Offset 55: PSTATE_CNT
        buf.push(0);
        // Offset 56: PM1a_EVT_BLK
        buf.extend_from_slice(&self.pm1a_event_block.to_le_bytes());
        // Offset 60: PM1b_EVT_BLK
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Offset 64: PM1a_CNT_BLK
        buf.extend_from_slice(&self.pm1a_control_block.to_le_bytes());
        // Offset 68: PM1b_CNT_BLK
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Offset 72: PM2_CNT_BLK
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Offset 76: PM_TMR_BLK
        buf.extend_from_slice(&self.pm_timer_block.to_le_bytes());
        // Offset 80: GPE0_BLK
        buf.extend_from_slice(&self.gpe0_block.to_le_bytes());
        // Offset 84: GPE1_BLK
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Offset 88: PM1_EVT_LEN
        buf.push(4);
        // Offset 89: PM1_CNT_LEN
        buf.push(2);
        // Offset 90: PM2_CNT_LEN
        buf.push(0);
        // Offset 91: PM_TMR_LEN
        buf.push(self.pm_timer_length);
        // Offset 92: GPE0_BLK_LEN
        buf.push(self.gpe0_length);
        // Offset 93: GPE1_BLK_LEN
        buf.push(0);
        // Offset 94: GPE1_BASE
        buf.push(0);
        // Offset 95: CST_CNT
        buf.push(0);
        // Offset 96: P_LVL2_LAT
        buf.extend_from_slice(&0x65u16.to_le_bytes());
        // Offset 98: P_LVL3_LAT
        buf.extend_from_slice(&0x03E9u16.to_le_bytes());
        // Offset 100: FLUSH_SIZE
        buf.extend_from_slice(&0u16.to_le_bytes());
        // Offset 102: FLUSH_STRIDE
        buf.extend_from_slice(&0u16.to_le_bytes());
        // Offset 104: DUTY_OFFSET
        buf.push(0);
        // Offset 105: DUTY_WIDTH
        buf.push(0);
        // Offset 106: DAY_ALRM
        buf.push(0x0D);
        // Offset 107: MON_ALRM
        buf.push(0);
        // Offset 108: CENTURY
        buf.push(0x32);
        // Offset 109: IAPC_BOOT_ARCH
        buf.extend_from_slice(&self.boot_arch_flags.to_le_bytes());
        // Offset 111: Reserved
        buf.push(0);
        // Offset 112: Flags
        buf.extend_from_slice(&self.flags.to_le_bytes());
        // Offset 116: RESET_REG (12 bytes GAS)
        self.reset_register.write_to(&mut buf);
        // Offset 128: RESET_VALUE
        buf.push(self.reset_value);
        // Offset 129: ARM_BOOT_ARCH
        buf.extend_from_slice(&0u16.to_le_bytes());
        // Offset 131: FADT Minor Version
        buf.push(1); // 6.1
        // Offset 132: X_FIRMWARE_CTRL (8 bytes)
        buf.extend_from_slice(&0u64.to_le_bytes());
        // Offset 140: X_DSDT (8 bytes)
        buf.extend_from_slice(&self.dsdt_address.to_le_bytes());
        // Offset 148: X_PM1a_EVT_BLK (12 bytes GAS)
        GenericAddress::io(u64::from(self.pm1a_event_block), 32).write_to(&mut buf);
        // Offset 160: X_PM1b_EVT_BLK
        GenericAddress::zero().write_to(&mut buf);
        // Offset 172: X_PM1a_CNT_BLK
        GenericAddress::io(u64::from(self.pm1a_control_block), 16).write_to(&mut buf);
        // Offset 184: X_PM1b_CNT_BLK
        GenericAddress::zero().write_to(&mut buf);
        // Offset 196: X_PM2_CNT_BLK
        GenericAddress::zero().write_to(&mut buf);
        // Offset 208: X_PM_TMR_BLK
        GenericAddress::io(u64::from(self.pm_timer_block), 32).write_to(&mut buf);
        // Offset 220: X_GPE0_BLK
        GenericAddress::io(u64::from(self.gpe0_block), self.gpe0_length * 8).write_to(&mut buf);
        // Offset 232: X_GPE1_BLK
        GenericAddress::zero().write_to(&mut buf);
        // Offset 244: SLEEP_CONTROL_REG
        GenericAddress::zero().write_to(&mut buf);
        // Offset 256: SLEEP_STATUS_REG
        GenericAddress::zero().write_to(&mut buf);
        // Offset 268: Hypervisor Vendor Identity (8 bytes)
        buf.extend_from_slice(&self.hypervisor_vendor_id.to_le_bytes());
        // Pad to 276 bytes
        while buf.len() < FADT_LENGTH as usize {
            buf.push(0);
        }

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fadt_length_is_correct() {
        let fadt = FadtBuilder::new(0xDEAD_0000).build();
        assert_eq!(fadt.len(), 276);
    }

    #[test]
    fn fadt_signature() {
        let fadt = FadtBuilder::new(0xDEAD_0000).build();
        assert_eq!(&fadt[0..4], b"FACP");
    }

    #[test]
    fn fadt_checksum_valid() {
        let fadt = FadtBuilder::new(0xDEAD_0000).build();
        let sum: u8 = fadt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0, "FADT checksum must sum to 0");
    }

    #[test]
    fn fadt_dsdt_pointer() {
        let fadt = FadtBuilder::new(0x1234_5678_9ABC_DEF0).build();
        // X_DSDT at offset 140
        let x_dsdt = u64::from_le_bytes(fadt[140..148].try_into().unwrap());
        assert_eq!(x_dsdt, 0x1234_5678_9ABC_DEF0);
    }

    #[test]
    fn fadt_revision() {
        let fadt = FadtBuilder::new(0).build();
        assert_eq!(fadt[8], 6); // FADT revision 6
    }

    #[test]
    fn fadt_sci_interrupt() {
        let fadt = FadtBuilder::new(0).sci_interrupt(11).build();
        let sci = u16::from_le_bytes(fadt[46..48].try_into().unwrap());
        assert_eq!(sci, 11);
    }
}

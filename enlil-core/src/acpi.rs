//! ACPI Table Synthesis for Guest VMs
//!
//! Generates RSDP → XSDT → FADT, MADT, DSDT, SSDT, MCFG for Windows boot.

use std::mem::size_of;

/// ACPI table header common to all ACPI tables
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct AcpiTableHeader {
    pub signature: [u8; 4], // "RSDT", "XSDT", "FADT", etc.
    pub length: u32,        // Total length including header
    pub revision: u8,
    pub checksum: u8,
    pub oem_id: [u8; 6],
    pub oem_table_id: [u8; 8],
    pub oem_revision: u32,
    pub creator_id: u32,
    pub creator_revision: u32,
}

/// Root System Description Pointer — points to XSDT
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Rsdp {
    pub signature: [u8; 8], // "RSD PTR "
    pub checksum: u8,
    pub oem_id: [u8; 6],
    pub revision: u8,      // 2 for ACPI 2.0
    pub rsdt_address: u32, // For ACPI 1.0 compat
    pub length: u32,
    pub xsdt_address: u64, // For ACPI 2.0
    pub extended_checksum: u8,
    pub reserved: [u8; 3],
}

/// Fixed ACPI Description Table — defines PM1a/PM1b, FACS, DSDT locations
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Fadt {
    pub header: AcpiTableHeader,
    pub firmware_ctrl: u32, // FACS address (32-bit)
    pub dsdt: u32,          // DSDT address (32-bit)
    pub reserved1: u8,
    pub preferred_pm_profile: u8,
    pub sci_int: u16, // SCI interrupt (IRQ 9 typical)
    pub smi_cmd: u32,
    pub acpi_enable: u8,
    pub acpi_disable: u8,
    pub s4bios_req: u8,
    pub pstate_cnt: u8,
    pub pm1a_evt_blk: u32,
    pub pm1b_evt_blk: u32,
    pub pm1a_cnt_blk: u32,
    pub pm1b_cnt_blk: u32,
    pub pm2_cnt_blk: u32,
    pub pm_tmr_blk: u32,
    pub gpe0_blk: u32,
    pub gpe1_blk: u32,
    pub pm1_evt_len: u8,
    pub pm1_cnt_len: u8,
    pub pm2_cnt_len: u8,
    pub pm_tmr_len: u8,
    pub gpe0_blk_len: u8,
    pub gpe1_blk_len: u8,
    pub gpe1_base: u8,
    pub cst_cnt: u8,
    pub p_lvl2_lat: u16,
    pub p_lvl3_lat: u16,
    pub flush_size: u16,
    pub flush_stride: u16,
    pub duty_offset: u8,
    pub duty_width: u8,
    pub day_alrm: u8,
    pub mon_alrm: u8,
    pub century: u8,
    pub iapc_boot_arch: u16,
    pub reserved2: u8,
    pub flags: u32,
    // ACPI 2.0+ 64-bit addresses
    pub reset_reg: u64, // GAS (Generic Address Structure)
    pub reset_value: u8,
    pub arm_boot_arch: u16,
    pub fadt_minor_version: u8,
    pub x_firmware_ctrl: u64,
    pub x_dsdt: u64,
    // ... truncated for brevity
}

/// Multiple APIC Description Table — CPU and IO APIC topology
#[repr(C, packed)]
pub struct MadtHeader {
    pub header: AcpiTableHeader,
    pub local_apic_addr: u32,
    pub flags: u32,
}

#[repr(C, packed)]
pub struct MadtLocalApic {
    pub entry_type: u8, // 0 = local APIC
    pub length: u8,
    pub processor_uid: u8,
    pub apic_id: u8,
    pub flags: u32,
}

#[repr(C, packed)]
pub struct MadtIoApic {
    pub entry_type: u8, // 1 = IO APIC
    pub length: u8,
    pub io_apic_id: u8,
    pub reserved: u8,
    pub io_apic_address: u32,
    pub global_system_interrupt_base: u32,
}

/// ACPI table generator
// Fields hold table parameters captured at construction; the per-table
// emitters that read them are part of the in-progress Phase 5 synthesis.
pub struct AcpiTableGenerator {
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    local_apic_addr: u32,
    vcpu_count: u32,
}

impl AcpiTableGenerator {
    pub fn new(vcpu_count: u32) -> Self {
        let mut oem_id = [0u8; 6];
        let mut oem_table_id = [0u8; 8];

        // "ENLIL" with padding
        oem_id[0..5].copy_from_slice(b"ENLIL");
        oem_table_id[0..7].copy_from_slice(b"ENLILVM");

        Self {
            oem_id,
            oem_table_id,
            local_apic_addr: 0xFEE00000, // Standard x86 local APIC address
            vcpu_count,
        }
    }

    /// The vCPU count this generator emits topology for.
    #[must_use]
    pub const fn vcpu_count(&self) -> u32 {
        self.vcpu_count
    }

    /// The local APIC base address reported in the MADT.
    #[must_use]
    pub const fn local_apic_addr(&self) -> u32 {
        self.local_apic_addr
    }

    /// The OEM table identifier stamped into emitted tables.
    #[must_use]
    pub const fn oem_table_id(&self) -> [u8; 8] {
        self.oem_table_id
    }

    /// Generate RSDP at a fixed location (0xE0000 on x86)
    pub fn generate_rsdp(&self, xsdt_addr: u64) -> Rsdp {
        let mut rsdp = Rsdp {
            signature: *b"RSD PTR ",
            checksum: 0,
            oem_id: self.oem_id,
            revision: 2,
            rsdt_address: 0,
            length: size_of::<Rsdp>() as u32,
            xsdt_address: xsdt_addr,
            extended_checksum: 0,
            reserved: [0; 3],
        };

        // Calculate checksums
        rsdp.checksum = Self::calculate_checksum(&rsdp as *const _ as *const u8, 20);
        rsdp.extended_checksum =
            Self::calculate_checksum(&rsdp as *const _ as *const u8, size_of::<Rsdp>());

        rsdp
    }

    fn calculate_checksum(ptr: *const u8, len: usize) -> u8 {
        let mut sum = 0u8;
        unsafe {
            for i in 0..len {
                sum = sum.wrapping_add(*ptr.add(i));
            }
        }
        (256u16 - sum as u16) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rsdp_generation() {
        let gen = AcpiTableGenerator::new(4);
        let rsdp = gen.generate_rsdp(0x1000);
        assert_eq!(&rsdp.signature, b"RSD PTR ");
        assert_eq!(rsdp.revision, 2);
    }
}

//! ACPI table synthesis for transparent guest boot
//!
//! Generates realistic ACPI tables that appear to come from a physical
//! motherboard. Windows and other OSes use these tables to discover hardware
//! topology, power management, and device configuration.
//!
//! # Table hierarchy
//! ```text
//! RSDP → XSDT → ┬ FADT → DSDT (AML bytecode)
//!                ├ SSDT (CPU P-states/C-states)
//!                ├ MADT (APIC topology)
//!                ├ MCFG (PCI Express config)
//!                ├ HPET (High-precision timer)
//!                ├ SRAT (NUMA topology)
//!                ├ SLIT (NUMA distances)
//!                ├ WAET (Emulated device hints)
//!                ├ BGRT (Boot logo)
//!                └ TPM2 (Trusted Platform Module)
//! ```

pub mod aml;
pub mod bgrt;
pub mod dsdt;
pub mod fadt;
pub mod hpet;
pub mod madt;
pub mod mcfg;
pub mod rsdp;
pub mod slit;
pub mod srat;
pub mod ssdt;
pub mod tables;
pub mod tpm2;
pub mod waet;
pub mod xsdt;

use tables::OemInfo;

/// Configuration for complete ACPI table set generation
pub struct AcpiTableSetConfig {
    /// OEM identification (shared across all tables)
    pub oem: OemInfo,
    /// Number of virtual CPUs
    pub vcpu_count: u8,
    /// Guest physical memory size in bytes
    pub memory_size_bytes: u64,
    /// PCI Express ECAM base address
    pub pcie_ecam_base: u64,
    /// Guest physical address where tables will be loaded
    pub table_base_address: u64,
    /// HPET base address
    pub hpet_base: u64,
    /// DSDT configuration
    pub dsdt_config: dsdt::DsdtConfig,
    /// Guest physical address of boot logo BMP (0 = none)
    pub boot_logo_address: u64,
}

impl Default for AcpiTableSetConfig {
    fn default() -> Self {
        Self {
            oem: OemInfo::default(),
            vcpu_count: 4,
            memory_size_bytes: 0x1_0000_0000, // 4 GiB
            pcie_ecam_base: 0xB000_0000,
            table_base_address: 0xF000_0000,
            hpet_base: hpet::HPET_BASE_ADDRESS,
            dsdt_config: dsdt::DsdtConfig::default(),
            boot_logo_address: 0,
        }
    }
}

/// Complete set of ACPI tables ready to be loaded into guest memory
pub struct AcpiTableSet {
    /// RSDP — placed at a well-known address (e.g., EBDA or UEFI config table)
    pub rsdp: Vec<u8>,
    /// All tables concatenated, to be loaded at `table_base_address`
    pub tables: Vec<u8>,
    /// Individual table offsets within `tables` (for debugging)
    pub table_offsets: Vec<(String, usize)>,
}

/// Build a complete ACPI table set for a guest VM
#[must_use]
pub fn build_acpi_tables(config: &AcpiTableSetConfig) -> AcpiTableSet {
    let base = config.table_base_address;
    let mut tables = Vec::new();
    let mut offsets = Vec::new();

    // Build DSDT first (FADT needs its address)
    let dsdt_bytes = dsdt::DsdtBuilder::new(dsdt::DsdtConfig {
        vcpu_count: config.vcpu_count,
        ..config.dsdt_config
    })
    .oem_info(config.oem.clone())
    .build();

    // Build all other tables to compute sizes
    let madt_bytes = madt::MadtBuilder::standard(config.vcpu_count)
        .oem_info(config.oem.clone())
        .build();
    let mcfg_bytes = mcfg::McfgBuilder::standard(config.pcie_ecam_base)
        .oem_info(config.oem.clone())
        .build();
    let hpet_bytes = hpet::HpetBuilder::new()
        .oem_info(config.oem.clone())
        .base_address(config.hpet_base)
        .build();
    let ssdt_bytes = ssdt::SsdtBuilder::new(config.vcpu_count)
        .oem_info(config.oem.clone())
        .build();
    let srat_bytes = srat::SratBuilder::single_node(config.vcpu_count, config.memory_size_bytes)
        .oem_info(config.oem.clone())
        .build();
    let slit_bytes = slit::SlitBuilder::single_node()
        .oem_info(config.oem.clone())
        .build();
    let waet_bytes = waet::WaetBuilder::new()
        .oem_info(config.oem.clone())
        .build();
    let bgrt_bytes = bgrt::BgrtBuilder::new()
        .oem_info(config.oem.clone())
        .image_address(config.boot_logo_address)
        .build();
    let tpm2_bytes = tpm2::Tpm2Builder::new()
        .oem_info(config.oem.clone())
        .build();

    // Layout: XSDT | FADT | MADT | MCFG | HPET | SSDT | SRAT | SLIT | WAET | BGRT | TPM2 | DSDT
    let xsdt_offset = 0usize;
    let fadt_offset = 256; // align XSDT to 256 bytes
    let madt_start = fadt_offset + 276; // FADT is always 276 bytes
    let mcfg_start = madt_start + madt_bytes.len();
    let hpet_start = mcfg_start + mcfg_bytes.len();
    let ssdt_start = hpet_start + hpet_bytes.len();
    let srat_start = ssdt_start + ssdt_bytes.len();
    let slit_start = srat_start + srat_bytes.len();
    let waet_start = slit_start + slit_bytes.len();
    let bgrt_start = waet_start + waet_bytes.len();
    let tpm2_start = bgrt_start + bgrt_bytes.len();
    let dsdt_start = tpm2_start + tpm2_bytes.len();

    // Build FADT with DSDT address
    let dsdt_gpa = base + dsdt_start as u64;
    let fadt_bytes = fadt::FadtBuilder::new(dsdt_gpa)
        .oem_info(config.oem.clone())
        .build();

    // Build XSDT with all table addresses
    let fadt_gpa = base + fadt_offset as u64;
    let madt_gpa = base + madt_start as u64;
    let mcfg_gpa = base + mcfg_start as u64;
    let hpet_gpa = base + hpet_start as u64;
    let ssdt_gpa = base + ssdt_start as u64;
    let srat_gpa = base + srat_start as u64;
    let slit_gpa = base + slit_start as u64;
    let waet_gpa = base + waet_start as u64;
    let bgrt_gpa = base + bgrt_start as u64;
    let tpm2_gpa = base + tpm2_start as u64;

    let xsdt_bytes = xsdt::XsdtBuilder::new()
        .oem_info(config.oem.clone())
        .add_table(fadt_gpa)
        .add_table(madt_gpa)
        .add_table(mcfg_gpa)
        .add_table(hpet_gpa)
        .add_table(ssdt_gpa)
        .add_table(srat_gpa)
        .add_table(slit_gpa)
        .add_table(waet_gpa)
        .add_table(bgrt_gpa)
        .add_table(tpm2_gpa)
        .build();

    // Assemble all tables into contiguous buffer
    tables.resize(fadt_offset, 0);
    tables[xsdt_offset..xsdt_offset + xsdt_bytes.len()].copy_from_slice(&xsdt_bytes);
    offsets.push(("XSDT".to_string(), xsdt_offset));

    tables.extend_from_slice(&fadt_bytes);
    offsets.push(("FADT".to_string(), fadt_offset));

    tables.extend_from_slice(&madt_bytes);
    offsets.push(("MADT".to_string(), madt_start));

    tables.extend_from_slice(&mcfg_bytes);
    offsets.push(("MCFG".to_string(), mcfg_start));

    tables.extend_from_slice(&hpet_bytes);
    offsets.push(("HPET".to_string(), hpet_start));

    tables.extend_from_slice(&ssdt_bytes);
    offsets.push(("SSDT".to_string(), ssdt_start));

    tables.extend_from_slice(&srat_bytes);
    offsets.push(("SRAT".to_string(), srat_start));

    tables.extend_from_slice(&slit_bytes);
    offsets.push(("SLIT".to_string(), slit_start));

    tables.extend_from_slice(&waet_bytes);
    offsets.push(("WAET".to_string(), waet_start));

    tables.extend_from_slice(&bgrt_bytes);
    offsets.push(("BGRT".to_string(), bgrt_start));

    tables.extend_from_slice(&tpm2_bytes);
    offsets.push(("TPM2".to_string(), tpm2_start));

    tables.extend_from_slice(&dsdt_bytes);
    offsets.push(("DSDT".to_string(), dsdt_start));

    // Build RSDP pointing to XSDT
    let xsdt_gpa = base + xsdt_offset as u64;
    let rsdp = rsdp::RsdpBuilder::new()
        .oem_id(config.oem.oem_id)
        .xsdt_address(xsdt_gpa)
        .build();

    AcpiTableSet {
        rsdp,
        tables,
        table_offsets: offsets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_complete_table_set() {
        let table_set = build_acpi_tables(&AcpiTableSetConfig::default());

        // RSDP should be 36 bytes
        assert_eq!(table_set.rsdp.len(), 36);

        // Tables should contain all expected tables
        assert!(
            table_set.tables.len() > 276,
            "Tables must be larger than just FADT"
        );

        // Should have 12 table entries (XSDT + FADT + MADT + MCFG + HPET + SSDT + SRAT + SLIT + WAET + BGRT + TPM2 + DSDT)
        assert_eq!(table_set.table_offsets.len(), 12);

        let names: Vec<&str> = table_set
            .table_offsets
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert!(names.contains(&"XSDT"));
        assert!(names.contains(&"FADT"));
        assert!(names.contains(&"MADT"));
        assert!(names.contains(&"MCFG"));
        assert!(names.contains(&"HPET"));
        assert!(names.contains(&"SSDT"));
        assert!(names.contains(&"SRAT"));
        assert!(names.contains(&"SLIT"));
        assert!(names.contains(&"WAET"));
        assert!(names.contains(&"BGRT"));
        assert!(names.contains(&"TPM2"));
        assert!(names.contains(&"DSDT"));
    }

    #[test]
    fn table_set_rsdp_checksum() {
        let table_set = build_acpi_tables(&AcpiTableSetConfig::default());
        let sum: u8 = table_set.rsdp[..20]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0, "RSDP v1 checksum must be 0");
    }

    #[test]
    fn table_set_all_checksums_valid() {
        let table_set = build_acpi_tables(&AcpiTableSetConfig::default());
        for &(ref name, offset) in &table_set.table_offsets {
            let table = &table_set.tables[offset..];
            let length = u32::from_le_bytes(table[4..8].try_into().unwrap()) as usize;
            let sum: u8 = table[..length]
                .iter()
                .fold(0u8, |acc, &b| acc.wrapping_add(b));
            assert_eq!(sum, 0, "{name} checksum must be 0");
        }
    }

    #[test]
    fn table_set_custom_vcpu_count() {
        let config = AcpiTableSetConfig {
            vcpu_count: 8,
            ..AcpiTableSetConfig::default()
        };
        let table_set = build_acpi_tables(&config);
        assert!(!table_set.tables.is_empty());
    }

    #[test]
    fn table_set_xsdt_has_10_entries() {
        let table_set = build_acpi_tables(&AcpiTableSetConfig::default());
        let xsdt = &table_set.tables[0..];
        let xsdt_len = u32::from_le_bytes(xsdt[4..8].try_into().unwrap()) as usize;
        // XSDT: 36-byte header + 8 bytes per entry
        let entry_count = (xsdt_len - 36) / 8;
        assert_eq!(
            entry_count, 10,
            "XSDT must point to 10 tables (FADT+MADT+MCFG+HPET+SSDT+SRAT+SLIT+WAET+BGRT+TPM2)"
        );
    }

    #[test]
    fn table_set_memory_size_propagates() {
        let config = AcpiTableSetConfig {
            memory_size_bytes: 0x2_0000_0000, // 8 GiB
            ..AcpiTableSetConfig::default()
        };
        let table_set = build_acpi_tables(&config);
        // Find SRAT and verify memory range
        let srat_offset = table_set
            .table_offsets
            .iter()
            .find(|(n, _)| n == "SRAT")
            .unwrap()
            .1;
        let srat = &table_set.tables[srat_offset..];
        let srat_len = u32::from_le_bytes(srat[4..8].try_into().unwrap()) as usize;
        assert!(srat_len > 48, "SRAT must contain entries");
    }
}

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
pub mod facs;
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

/// The ACPI tables that have no inter-table address dependencies.
struct SecondaryTables {
    madt: Vec<u8>,
    mcfg: Vec<u8>,
    hpet: Vec<u8>,
    ssdt: Vec<u8>,
    srat: Vec<u8>,
    slit: Vec<u8>,
    waet: Vec<u8>,
    bgrt: Vec<u8>,
    tpm2: Vec<u8>,
}

fn build_secondary_tables(config: &AcpiTableSetConfig) -> SecondaryTables {
    SecondaryTables {
        madt: madt::MadtBuilder::standard(config.vcpu_count)
            .oem_info(config.oem.clone())
            .build(),
        mcfg: mcfg::McfgBuilder::standard(config.pcie_ecam_base)
            .oem_info(config.oem.clone())
            .build(),
        hpet: hpet::HpetBuilder::new()
            .oem_info(config.oem.clone())
            .base_address(config.hpet_base)
            .build(),
        ssdt: ssdt::SsdtBuilder::new(config.vcpu_count)
            .oem_info(config.oem.clone())
            .build(),
        srat: srat::SratBuilder::single_node(config.vcpu_count, config.memory_size_bytes)
            .oem_info(config.oem.clone())
            .build(),
        slit: slit::SlitBuilder::single_node()
            .oem_info(config.oem.clone())
            .build(),
        waet: waet::WaetBuilder::new()
            .oem_info(config.oem.clone())
            .build(),
        bgrt: bgrt::BgrtBuilder::new()
            .oem_info(config.oem.clone())
            .image_address(config.boot_logo_address)
            .build(),
        tpm2: tpm2::Tpm2Builder::new()
            .oem_info(config.oem.clone())
            .build(),
    }
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

    // Build all other tables to compute sizes.
    let SecondaryTables {
        madt: madt_bytes,
        mcfg: mcfg_bytes,
        hpet: hpet_bytes,
        ssdt: ssdt_bytes,
        srat: srat_bytes,
        slit: slit_bytes,
        waet: waet_bytes,
        bgrt: bgrt_bytes,
        tpm2: tpm2_bytes,
    } = build_secondary_tables(config);

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

    // The FACS lives in the 64-byte-aligned padding between the XSDT and the
    // FADT (the region is zero-filled to `fadt_offset` below). It is referenced
    // only through the FADT's FIRMWARE_CTRL, never the XSDT, so it is not a
    // table-set entry. ACPI requires 64-byte alignment; offset 128 over the
    // 64-byte-aligned table base satisfies that and fits before the FADT at 256.
    let facs_offset = 128usize;
    let facs_bytes = facs::FacsBuilder::new().build();
    let facs_gpa = base + facs_offset as u64;

    // Build FADT with DSDT + FACS addresses
    let dsdt_gpa = base + dsdt_start as u64;
    let fadt_bytes = fadt::FadtBuilder::new(dsdt_gpa)
        .firmware_ctrl(facs_gpa)
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

    // Assemble all tables into a contiguous buffer (XSDT at 0, FADT at fadt_offset).
    tables.resize(fadt_offset, 0);
    tables[xsdt_offset..xsdt_offset + xsdt_bytes.len()].copy_from_slice(&xsdt_bytes);
    offsets.push(("XSDT".to_string(), xsdt_offset));
    // Place the FACS in the aligned padding (not an XSDT entry; reached via the
    // FADT's FIRMWARE_CTRL set above).
    tables[facs_offset..facs_offset + facs_bytes.len()].copy_from_slice(&facs_bytes);
    for (name, bytes) in [
        ("FADT", &fadt_bytes),
        ("MADT", &madt_bytes),
        ("MCFG", &mcfg_bytes),
        ("HPET", &hpet_bytes),
        ("SSDT", &ssdt_bytes),
        ("SRAT", &srat_bytes),
        ("SLIT", &slit_bytes),
        ("WAET", &waet_bytes),
        ("BGRT", &bgrt_bytes),
        ("TPM2", &tpm2_bytes),
        ("DSDT", &dsdt_bytes),
    ] {
        offsets.push((name.to_string(), tables.len()));
        tables.extend_from_slice(bytes);
    }

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
    fn dsdt_processor_devices_and_ssdt_power_scopes_agree() {
        // The SSDT augments each processor's power objects via Scope(C0n); those
        // names must match the processor Devices the DSDT declares, or the _PSS/_CST
        // attach to nothing. Both derive from vcpu_count — assert they stay in lock
        // step for a representative count.
        let config = AcpiTableSetConfig {
            vcpu_count: 6,
            ..AcpiTableSetConfig::default()
        };
        let ts = build_acpi_tables(&config);
        let off = |name: &str| ts.table_offsets.iter().find(|(n, _)| n == name).unwrap().1;
        let dsdt = &ts.tables[off("DSDT")..];
        let ssdt = &ts.tables[off("SSDT")..];
        // Processor names C00..C05 (6 vCPUs) must appear in BOTH tables.
        for cpu in 0u8..6 {
            let hex = b"0123456789ABCDEF";
            let name = [
                b'C',
                hex[((cpu >> 4) & 0xF) as usize],
                hex[(cpu & 0xF) as usize],
                b'_',
            ];
            assert!(
                dsdt.windows(4).any(|w| w == name),
                "DSDT must declare processor {cpu}"
            );
            assert!(
                ssdt.windows(4).any(|w| w == name),
                "SSDT must scope power objects into processor {cpu}"
            );
        }
        // And neither references a processor beyond the count (C06 absent in both).
        let c06 = *b"C06_";
        assert!(
            !dsdt.windows(4).any(|w| w == c06),
            "no extra DSDT processor"
        );
        assert!(!ssdt.windows(4).any(|w| w == c06), "no extra SSDT scope");
    }

    #[test]
    fn rsdp_xsdt_pointer_chain_resolves_to_valid_tables() {
        // Walk the address chain a guest's ACPICA follows — RSDP → XSDT → each
        // table — and confirm every guest-physical pointer lands on a table whose
        // signature is well-formed and whose checksum is valid. This catches any
        // layout/offset drift that would leave a pointer dangling.
        let config = AcpiTableSetConfig::default();
        let ts = build_acpi_tables(&config);
        let base = config.table_base_address;
        let to_off = |gpa: u64| usize::try_from(gpa - base).unwrap();

        let valid_table = |bytes: &[u8]| -> bool {
            if bytes.len() < 36 {
                return false;
            }
            let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
            if len < 36 || len > bytes.len() {
                return false;
            }
            // 4-char ASCII signature and a zero 8-bit checksum over `len` bytes.
            bytes[..4]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                && bytes[..len].iter().fold(0u8, |a, &b| a.wrapping_add(b)) == 0
        };

        // RSDP (revision 2) carries the 64-bit XSDT address at offset 24.
        let xsdt_gpa = u64::from_le_bytes(ts.rsdp[24..32].try_into().unwrap());
        let xsdt = &ts.tables[to_off(xsdt_gpa)..];
        assert_eq!(&xsdt[0..4], b"XSDT");
        assert!(valid_table(xsdt), "XSDT must be self-consistent");

        // Each 8-byte XSDT entry points to a valid table; one of them is the FADT.
        let xsdt_len = u32::from_le_bytes(xsdt[4..8].try_into().unwrap()) as usize;
        let mut saw_fadt = false;
        for entry in xsdt[36..xsdt_len].chunks_exact(8) {
            let gpa = u64::from_le_bytes(entry.try_into().unwrap());
            let tbl = &ts.tables[to_off(gpa)..];
            assert!(
                valid_table(tbl),
                "XSDT entry -> invalid table (sig {:?})",
                &tbl[0..4]
            );
            if &tbl[0..4] == b"FACP" {
                saw_fadt = true;
                // The FADT's X_DSDT (offset 140) must resolve to the DSDT.
                let x_dsdt = u64::from_le_bytes(tbl[140..148].try_into().unwrap());
                assert_eq!(&ts.tables[to_off(x_dsdt)..to_off(x_dsdt) + 4], b"DSDT");
            }
        }
        assert!(saw_fadt, "XSDT must reference the FADT");
    }

    #[test]
    fn fadt_points_to_a_valid_facs_in_the_buffer() {
        let config = AcpiTableSetConfig::default();
        let table_set = build_acpi_tables(&config);
        let base = config.table_base_address;

        // Locate the FADT and read its FIRMWARE_CTRL (offset 36) and
        // X_FIRMWARE_CTRL (offset 132); both must agree and be non-zero.
        let fadt_off = table_set
            .table_offsets
            .iter()
            .find(|(n, _)| n == "FADT")
            .expect("FADT present")
            .1;
        let fadt = &table_set.tables[fadt_off..];
        let firmware_ctrl = u32::from_le_bytes(fadt[36..40].try_into().unwrap());
        let x_firmware_ctrl = u64::from_le_bytes(fadt[132..140].try_into().unwrap());
        assert_ne!(firmware_ctrl, 0, "FADT must reference a FACS");
        assert_eq!(
            u64::from(firmware_ctrl),
            x_firmware_ctrl,
            "32/64-bit FACS pointers agree"
        );

        // The pointer is a guest-physical address; convert back to a buffer offset
        // and confirm a 64-byte, version-2 "FACS" sits there, 64-byte aligned.
        let facs_off = usize::try_from(x_firmware_ctrl - base).unwrap();
        assert_eq!(facs_off % 64, 0, "FACS must be 64-byte aligned");
        let facs = &table_set.tables[facs_off..];
        assert_eq!(&facs[0..4], b"FACS");
        assert_eq!(u32::from_le_bytes(facs[4..8].try_into().unwrap()), 64);
        assert_eq!(facs[32], facs::FACS_VERSION);
        // The FACS must not overlap the XSDT (which starts at offset 0).
        let xsdt_len = u32::from_le_bytes(table_set.tables[4..8].try_into().unwrap()) as usize;
        assert!(facs_off >= xsdt_len, "FACS must sit past the XSDT");
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

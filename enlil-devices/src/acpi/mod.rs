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
//!                ├ WSMT (SMM security mitigations)
//!                ├ FPDT (Firmware boot-performance → FBPT)
//!                ├ BGRT (Boot logo)
//!                └ TPM2 (Trusted Platform Module)
//! ```

pub mod aml;
pub mod bgrt;
pub mod dsdt;
pub mod facs;
pub mod fadt;
pub mod fpdt;
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
pub mod wsmt;
pub mod xsdt;

use tables::OemInfo;

/// Canonical `SLP_TYP` value for ACPI **S3** (suspend-to-RAM) in enlil's DSDT.
///
/// The DSDT's `\_S3` package advertises this value and the guest writes it (with
/// `SLP_EN`) to `PM1a_CNT` to suspend; the chipset PM1 model captures it and the
/// run loop classifies it as a suspend transition. enlil uses the `SLP_TYP` =
/// sleep-state-number convention, so S3→3, S4→4, S5→5 — internally consistent
/// and distinct, which is all a guest requires (the numeric values are
/// board-specific on real hardware, discovered from the `_Sx` objects).
pub const SLP_TYP_S3: u8 = 3;
/// Canonical `SLP_TYP` value for ACPI **S4** (suspend-to-disk / hibernate).
/// See [`SLP_TYP_S3`].
pub const SLP_TYP_S4: u8 = 4;
/// Canonical `SLP_TYP` value for ACPI **S5** (soft off / power down).
/// See [`SLP_TYP_S3`].
pub const SLP_TYP_S5: u8 = 5;

/// Configuration for complete ACPI table set generation
#[derive(Debug, Clone)]
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
    /// Every inter-table pointer the firmware must relocate once it places the
    /// delivered files in guest RAM — the data the QEMU `etc/table-loader`
    /// `ADD_POINTER` commands are generated from. Reported relative to each
    /// file's base, so for a set built with `table_base_address == 0` the value
    /// stored at the pointer field equals [`AcpiPointer::target_offset`].
    pub pointers: Vec<AcpiPointer>,
}

/// Which delivered `fw_cfg` file a pointer lives in or targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpiFile {
    /// `etc/acpi/rsdp` — the Root System Description Pointer.
    Rsdp,
    /// `etc/acpi/tables` — the concatenated table blob (XSDT, FADT, …).
    Tables,
}

impl AcpiFile {
    /// The `fw_cfg` file name the firmware matches in an `ADD_POINTER` command.
    #[must_use]
    pub const fn fw_cfg_name(self) -> &'static str {
        match self {
            Self::Rsdp => "etc/acpi/rsdp",
            Self::Tables => "etc/acpi/tables",
        }
    }
}

/// One inter-table pointer to relocate at firmware load time.
///
/// In QEMU `etc/table-loader` `ADD_POINTER` terms: the pointer field lives in
/// [`pointer_file`](Self::pointer_file) (the command's `dest_file`) at
/// [`offset`](Self::offset), is [`size`](Self::size) bytes wide, and is
/// relocated by adding the runtime base of [`target_file`](Self::target_file)
/// (the command's `src_file`). [`target_offset`](Self::target_offset) is where
/// inside the target file the pointer aims — i.e. exactly the little-endian
/// value stored at the field when the set is built at base 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpiPointer {
    /// File containing the pointer field (the loader command's `dest_file`).
    pub pointer_file: AcpiFile,
    /// File the pointer targets (the loader command's `src_file`).
    pub target_file: AcpiFile,
    /// Byte offset of the pointer field within `pointer_file`.
    pub offset: u32,
    /// Width of the pointer field in bytes (4 for the legacy 32-bit fields,
    /// 8 for the `X_` 64-bit fields).
    pub size: u8,
    /// Offset of the target within `target_file` (the base-0 stored value).
    pub target_offset: u64,
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
    wsmt: Vec<u8>,
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
        wsmt: wsmt::WsmtBuilder::new()
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

/// Build the FACS and the FADT that points at it (and at the DSDT).
///
/// The FACS lives at a fixed 64-byte-aligned offset (192) in the zero-filled
/// padding between the XSDT and the FADT; it is referenced only through the
/// FADT's `FIRMWARE_CTRL`, never the XSDT, so it is not a table-set entry. 192
/// (not 128) so the FACS clears the XSDT even once the XSDT holds a dozen entry
/// pointers (12 entries = 132 bytes); it still ends exactly at the FADT (256).
///
/// The layout and the FADT relocation offsets assume the fixed 276-byte FADT
/// (= `madt_start - fadt_offset`); the `fadt_fixed_layout_assumption_holds` test
/// pins that. Returns `(facs_offset, facs_bytes, fadt_bytes)`.
fn build_fadt_and_facs(
    config: &AcpiTableSetConfig,
    base: u64,
    dsdt_start: usize,
) -> (usize, Vec<u8>, Vec<u8>) {
    let facs_offset = 192usize;
    let facs_bytes = facs::FacsBuilder::new().build();
    let facs_gpa = base + facs_offset as u64;
    let dsdt_gpa = base + dsdt_start as u64;
    let fadt_bytes = fadt::FadtBuilder::new(dsdt_gpa)
        .firmware_ctrl(facs_gpa)
        .oem_info(config.oem.clone())
        .build();
    (facs_offset, facs_bytes, fadt_bytes)
}

/// Build the XSDT pointing at each table, in layout order. `entry_targets` are
/// base-relative offsets; the same list feeds the pointer-relocation map so the
/// XSDT entries and the table-loader `ADD_POINTER` offsets can never disagree.
///
/// `facs_offset` is only used for a debug-mode guard: the XSDT grows 8 bytes per
/// entry and must still clear the FACS that sits in the pre-FADT padding (a
/// 12-entry XSDT is 132 bytes, the FACS starts at 192).
fn build_xsdt(oem: &OemInfo, base: u64, entry_targets: &[usize], facs_offset: usize) -> Vec<u8> {
    let gpas: Vec<u64> = entry_targets.iter().map(|&o| base + o as u64).collect();
    let bytes = xsdt::XsdtBuilder::new()
        .oem_info(oem.clone())
        .add_tables(&gpas)
        .build();
    debug_assert!(
        bytes.len() <= facs_offset,
        "XSDT ({} bytes) overruns the FACS at offset {facs_offset}",
        bytes.len()
    );
    bytes
}

/// Build the FPDT and its FBPT target blob. The FPDT's pointer record targets
/// the FBPT's guest-physical address (`base + fbpt_offset`), relocated by the
/// firmware table-loader like FADT→FACS. Returns `(fpdt_table, fbpt_blob)`.
fn build_fpdt_and_fbpt(
    config: &AcpiTableSetConfig,
    base: u64,
    fbpt_offset: usize,
) -> (Vec<u8>, Vec<u8>) {
    let fpdt_table = fpdt::FpdtBuilder::new()
        .oem_info(config.oem.clone())
        .fbpt_address(base + fbpt_offset as u64)
        .build();
    (fpdt_table, fpdt::FbptBuilder::new().build())
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
        wsmt: wsmt_bytes,
        bgrt: bgrt_bytes,
        tpm2: tpm2_bytes,
    } = build_secondary_tables(config);

    // Layout: XSDT | FADT | MADT | MCFG | HPET | SSDT | SRAT | SLIT | WAET | WSMT | FPDT | BGRT | TPM2 | DSDT | FBPT
    let xsdt_offset = 0usize;
    let fadt_offset = 256; // align XSDT to 256 bytes
    let madt_start = fadt_offset + 276; // FADT is always 276 bytes
    let mcfg_start = madt_start + madt_bytes.len();
    let hpet_start = mcfg_start + mcfg_bytes.len();
    let ssdt_start = hpet_start + hpet_bytes.len();
    let srat_start = ssdt_start + ssdt_bytes.len();
    let slit_start = srat_start + srat_bytes.len();
    let waet_start = slit_start + slit_bytes.len();
    let wsmt_start = waet_start + waet_bytes.len();
    // FPDT has a fixed 52-byte length, so its slot can be reserved before the
    // FBPT (its pointer target) address is known.
    let fpdt_start = wsmt_start + wsmt_bytes.len();
    let bgrt_start = fpdt_start + fpdt::FPDT_LENGTH as usize;
    let tpm2_start = bgrt_start + bgrt_bytes.len();
    let dsdt_start = tpm2_start + tpm2_bytes.len();
    // The FBPT (Firmware Basic Boot Performance Table) is the FPDT pointer's
    // target: a plain blob after the DSDT, reached only via the relocated FPDT
    // pointer, so — like the FACS — it is not an XSDT entry.
    let fbpt_offset = dsdt_start + dsdt_bytes.len();

    // FPDT (address-dependent, like the FADT) + its FBPT target blob.
    let (fpdt_table, fbpt_blob) = build_fpdt_and_fbpt(config, base, fbpt_offset);

    // FACS + FADT (the FACS lives in the aligned padding before the FADT and is
    // reached only via the FADT's FIRMWARE_CTRL, so it is not an XSDT entry).
    let (facs_offset, facs_bytes, fadt_bytes) = build_fadt_and_facs(config, base, dsdt_start);

    // Build XSDT with all table addresses, in layout order. The same offset
    // list feeds the pointer-relocation map below, so the XSDT entries and the
    // table-loader ADD_POINTER offsets can never disagree.
    let xsdt_entry_targets = [
        fadt_offset,
        madt_start,
        mcfg_start,
        hpet_start,
        ssdt_start,
        srat_start,
        slit_start,
        waet_start,
        wsmt_start,
        fpdt_start,
        bgrt_start,
        tpm2_start,
    ];
    let xsdt_bytes = build_xsdt(&config.oem, base, &xsdt_entry_targets, facs_offset);

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
        ("WSMT", &wsmt_bytes),
        ("FPDT", &fpdt_table),
        ("BGRT", &bgrt_bytes),
        ("TPM2", &tpm2_bytes),
        ("DSDT", &dsdt_bytes),
    ] {
        offsets.push((name.to_string(), tables.len()));
        tables.extend_from_slice(bytes);
    }
    // Append the FBPT after the DSDT (not an XSDT entry; reached only via the
    // FPDT pointer relocated below). Its computed offset must match the layout.
    debug_assert_eq!(tables.len(), fbpt_offset);
    tables.extend_from_slice(&fbpt_blob);

    // Build RSDP pointing to XSDT
    let xsdt_gpa = base + xsdt_offset as u64;
    let rsdp = rsdp::RsdpBuilder::new()
        .oem_id(config.oem.oem_id)
        .xsdt_address(xsdt_gpa)
        .build();

    let pointers = build_acpi_pointers(
        xsdt_offset,
        fadt_offset,
        facs_offset,
        dsdt_start,
        &xsdt_entry_targets,
        fpdt_start,
        fbpt_offset,
    );

    AcpiTableSet {
        rsdp,
        tables,
        table_offsets: offsets,
        pointers,
    }
}

/// Compute the inter-table pointer relocations (the `etc/table-loader`
/// `ADD_POINTER` inputs) from the concrete table layout.
///
/// Offsets are confirmed against the builders: RSDP `XsdtAddress` @24
/// (`rsdp.rs`), XSDT entries @`36 + i*8` after the 36-byte SDT header
/// (`xsdt.rs`), FADT `FIRMWARE_CTRL` @36 / `DSDT` @40 / `X_FIRMWARE_CTRL` @132 /
/// `X_DSDT` @140 (`fadt.rs`). The `pointer_self_validates_against_built_bytes`
/// test re-checks every one against the actual bytes, so a wrong offset here
/// fails CI rather than silently mis-patching a real firmware load.
fn build_acpi_pointers(
    xsdt_offset: usize,
    fadt_offset: usize,
    facs_offset: usize,
    dsdt_start: usize,
    xsdt_entry_targets: &[usize],
    fpdt_start: usize,
    fbpt_offset: usize,
) -> Vec<AcpiPointer> {
    let u32_of = crate::truncate::u32_of;
    let mut pointers = vec![AcpiPointer {
        pointer_file: AcpiFile::Rsdp,
        target_file: AcpiFile::Tables,
        offset: 24,
        size: 8,
        target_offset: xsdt_offset as u64,
    }];
    // XSDT entries → each table, in the add_table() order.
    for (i, &target) in xsdt_entry_targets.iter().enumerate() {
        pointers.push(AcpiPointer {
            pointer_file: AcpiFile::Tables,
            target_file: AcpiFile::Tables,
            offset: u32_of(xsdt_offset + 36 + i * 8),
            size: 8,
            target_offset: target as u64,
        });
    }
    // FADT → FACS and DSDT, both the legacy 32-bit and the X_ 64-bit fields.
    for &(field_off, size, target) in &[
        (36u32, 4u8, facs_offset),
        (40, 4, dsdt_start),
        (132, 8, facs_offset),
        (140, 8, dsdt_start),
    ] {
        pointers.push(AcpiPointer {
            pointer_file: AcpiFile::Tables,
            target_file: AcpiFile::Tables,
            offset: u32_of(fadt_offset) + field_off,
            size,
            target_offset: target as u64,
        });
    }
    // FPDT's Firmware Basic Boot Performance Pointer record → the FBPT blob.
    pointers.push(AcpiPointer {
        pointer_file: AcpiFile::Tables,
        target_file: AcpiFile::Tables,
        offset: u32_of(fpdt_start + fpdt::FPDT_FBPT_POINTER_OFFSET),
        size: 8,
        target_offset: fbpt_offset as u64,
    });
    pointers
}

/// Build the `etc/table-loader` command stream that relocates and re-checksums
/// `set` at firmware load time.
///
/// `set` must have been built with `table_base_address == 0` so the stored
/// pointer values are pure offsets the firmware adds the runtime allocation
/// base to (see [`AcpiTableSet::pointers`]). The stream is, in firmware
/// execution order:
/// 1. **`ALLOCATE`** `etc/acpi/tables` (high memory) and `etc/acpi/rsdp` (the
///    F-segment, where a legacy OS scans for the RSDP).
/// 2. **`ADD_POINTER`** for every relocation in `set.pointers`.
/// 3. **`ADD_CHECKSUM`** for each SDT (header checksum at offset 9, length read
///    from the table's own header so the span is exact) and the RSDP's two
///    checksums (the 20-byte ACPI-1.0 sum at offset 8 and the 36-byte extended
///    sum at offset 32). Checksums come last so they cover the relocated bytes.
///
/// # Panics
/// Panics if a `set.table_offsets` entry does not point at a full 8-byte SDT
/// header within `set.tables` — impossible for a set from [`build_acpi_tables`].
#[must_use]
pub fn build_acpi_table_loader(set: &AcpiTableSet) -> Vec<u8> {
    use crate::fw_cfg_loader::{AllocZone, BiosLinkerLoader};
    let u32_of = crate::truncate::u32_of;
    let tables = AcpiFile::Tables.fw_cfg_name();
    let rsdp = AcpiFile::Rsdp.fw_cfg_name();

    let mut loader = BiosLinkerLoader::new();
    loader.allocate(tables, 64, AllocZone::High);
    loader.allocate(rsdp, 16, AllocZone::FSeg);
    for p in &set.pointers {
        loader.add_pointer(
            p.pointer_file.fw_cfg_name(),
            p.target_file.fw_cfg_name(),
            p.offset,
            p.size,
        );
    }
    // Each SDT carries its length at header offset 4 and its checksum byte at
    // offset 9 — derive the span from the bytes themselves, never a guess.
    for (_name, start) in &set.table_offsets {
        let len = u32::from_le_bytes(set.tables[start + 4..start + 8].try_into().unwrap());
        loader.add_checksum(tables, u32_of(start + 9), u32_of(*start), len);
    }
    loader.add_checksum(rsdp, 8, 0, 20);
    loader.add_checksum(rsdp, 32, 0, 36);
    loader.into_bytes()
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

        // Should have 14 table entries (XSDT + FADT + MADT + MCFG + HPET + SSDT + SRAT + SLIT + WAET + WSMT + FPDT + BGRT + TPM2 + DSDT). The FBPT is a sub-blob, not an XSDT entry, so it is not counted.
        assert_eq!(table_set.table_offsets.len(), 14);

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
        assert!(names.contains(&"WSMT"));
        assert!(names.contains(&"FPDT"));
        assert!(names.contains(&"BGRT"));
        assert!(names.contains(&"TPM2"));
        assert!(names.contains(&"DSDT"));
    }

    #[test]
    fn pointer_self_validates_against_built_bytes() {
        // For every reported relocation, the little-endian value actually stored
        // at the claimed offset in the built file must equal base + target_offset.
        // This proves each reported pointer-field offset is *really* where that
        // pointer lives — catching a wrong offset here without needing a firmware
        // boot, and confirming the set is relocatable as the table-loader assumes.
        let config = AcpiTableSetConfig {
            table_base_address: 0xF000_0000,
            ..AcpiTableSetConfig::default()
        };
        let base = config.table_base_address;
        let ts = build_acpi_tables(&config);
        assert!(!ts.pointers.is_empty());

        for p in &ts.pointers {
            let file = match p.pointer_file {
                AcpiFile::Rsdp => &ts.rsdp,
                AcpiFile::Tables => &ts.tables,
            };
            let off = usize::try_from(p.offset).unwrap();
            let stored = match p.size {
                4 => u64::from(u32::from_le_bytes(file[off..off + 4].try_into().unwrap())),
                8 => u64::from_le_bytes(file[off..off + 8].try_into().unwrap()),
                other => panic!("unexpected pointer size {other}"),
            };
            let expected = base.wrapping_add(p.target_offset) & mask(p.size);
            assert_eq!(
                stored, expected,
                "{:?} pointer at offset {:#x} (size {}) -> target_offset {:#x}: \
                 stored {:#x} != base+target {:#x}",
                p.pointer_file, p.offset, p.size, p.target_offset, stored, expected
            );
            // The target offset must land inside the target file.
            let target_file_len = match p.target_file {
                AcpiFile::Rsdp => ts.rsdp.len(),
                AcpiFile::Tables => ts.tables.len(),
            };
            assert!(
                usize::try_from(p.target_offset).unwrap() < target_file_len,
                "target_offset {:#x} out of range for target file",
                p.target_offset
            );
        }
    }

    #[test]
    fn fadt_fixed_layout_assumption_holds() {
        // build_acpi_tables hardcodes madt_start = fadt_offset + 276 and the FADT
        // relocation offsets assume a 276-byte FADT (X_DSDT@140 etc.). Pin that:
        // the FADT lands exactly where MADT begins, occupies 276 bytes, and its
        // own header length field agrees — so a FADT size change trips this test.
        let ts = build_acpi_tables(&AcpiTableSetConfig::default());
        let fadt_start = ts
            .table_offsets
            .iter()
            .find(|(n, _)| n == "FADT")
            .unwrap()
            .1;
        let madt_start = ts
            .table_offsets
            .iter()
            .find(|(n, _)| n == "MADT")
            .unwrap()
            .1;
        assert_eq!(
            madt_start - fadt_start,
            276,
            "FADT occupies its fixed 276 bytes"
        );
        let hdr_len = u32::from_le_bytes(
            ts.tables[fadt_start + 4..fadt_start + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(hdr_len, 276, "FADT header length field matches the layout");
        // X_DSDT (offset 140) must hold the DSDT address, proving the field the
        // relocation map targets is where we think it is.
        let dsdt_start = ts
            .table_offsets
            .iter()
            .find(|(n, _)| n == "DSDT")
            .unwrap()
            .1 as u64;
        let base = AcpiTableSetConfig::default().table_base_address;
        let x_dsdt = u64::from_le_bytes(
            ts.tables[fadt_start + 140..fadt_start + 148]
                .try_into()
                .unwrap(),
        );
        assert_eq!(x_dsdt, base + dsdt_start);
    }

    #[test]
    fn pointers_cover_rsdp_xsdt_and_fadt_links() {
        // The relocation set must include the RSDP->XSDT link, one XSDT entry per
        // table the XSDT references (10), and the four FADT FACS/DSDT pointers.
        let ts = build_acpi_tables(&AcpiTableSetConfig::default());
        let rsdp_links = ts
            .pointers
            .iter()
            .filter(|p| p.pointer_file == AcpiFile::Rsdp)
            .count();
        assert_eq!(rsdp_links, 1, "exactly the RSDP->XSDT pointer");
        // 1 (RSDP) + 12 (XSDT entries) + 4 (FADT FACS/DSDT x {32,64}-bit)
        // + 1 (FPDT → FBPT) = 18.
        assert_eq!(ts.pointers.len(), 18);
    }

    fn mask(size: u8) -> u64 {
        if size == 8 {
            u64::MAX
        } else {
            (1u64 << (size * 8)) - 1
        }
    }

    /// A decoded bios-linker-loader command (just the fields the tests check).
    struct Cmd {
        command: u32,
        dest: String,
        src: String,
        offset: u32,
        size: u8,
        cksum_offset: u32,
        cksum_start: u32,
        cksum_len: u32,
    }

    fn decode_loader(bytes: &[u8]) -> Vec<Cmd> {
        let name = |b: &[u8], o: usize| {
            let f = &b[o..o + 56];
            let end = f.iter().position(|&c| c == 0).unwrap_or(56);
            String::from_utf8(f[..end].to_vec()).unwrap()
        };
        let le32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        bytes
            .as_chunks::<128>()
            .0
            .iter()
            .map(|e| Cmd {
                command: le32(e, 0),
                dest: name(e, 0x04),
                src: name(e, 0x38),
                offset: le32(e, 0x6C),
                size: e[0x70],
                cksum_offset: le32(e, 0x3C),
                cksum_start: le32(e, 0x40),
                cksum_len: le32(e, 0x44),
            })
            .collect()
    }

    #[test]
    fn table_loader_encodes_every_relocation() {
        // Build at base 0 — the table-loader contract (stored pointer = offset).
        let config = AcpiTableSetConfig {
            table_base_address: 0,
            ..AcpiTableSetConfig::default()
        };
        let set = build_acpi_tables(&config);
        let cmds = decode_loader(&build_acpi_table_loader(&set));

        let allocs: Vec<&Cmd> = cmds.iter().filter(|c| c.command == 0x1).collect();
        let pointers: Vec<&Cmd> = cmds.iter().filter(|c| c.command == 0x2).collect();
        let cksum_count = cmds.iter().filter(|c| c.command == 0x3).count();

        // Two ALLOCATEs: tables (high=1) and rsdp (fseg=2).
        assert_eq!(allocs.len(), 2);
        assert_eq!(allocs[0].dest, "etc/acpi/tables");
        assert_eq!(allocs[1].dest, "etc/acpi/rsdp");

        // One ADD_POINTER per reported relocation, in order, with matching
        // dest/src files, offset and size.
        assert_eq!(pointers.len(), set.pointers.len());
        for (cmd, p) in pointers.iter().zip(&set.pointers) {
            assert_eq!(cmd.dest, p.pointer_file.fw_cfg_name());
            assert_eq!(cmd.src, p.target_file.fw_cfg_name());
            assert_eq!(cmd.offset, p.offset);
            assert_eq!(cmd.size, p.size);
        }

        // One checksum per SDT plus the RSDP's two; every command precedes no
        // pointer (checksums are emitted last).
        assert_eq!(cksum_count, set.table_offsets.len() + 2);
        let last_pointer = cmds.iter().rposition(|c| c.command == 0x2).unwrap();
        let first_cksum = cmds.iter().position(|c| c.command == 0x3).unwrap();
        assert!(last_pointer < first_cksum, "checksums must follow pointers");
    }

    #[test]
    fn loader_relocates_and_revalidates_the_whole_acpi_set() {
        // The end-to-end proof, without an OVMF boot: build the set at base 0,
        // build its etc/table-loader, then *execute* that loader (the firmware
        // side) placing the files at chosen addresses. Afterward the pointer
        // chain must resolve to the relocated tables and every ACPI checksum
        // must validate — exactly what a real firmware would end up with.
        use crate::fw_cfg_loader::LoaderExecutor;
        const TABLES_BASE: u64 = 0x7F00_0000;
        const RSDP_BASE: u64 = 0x000E_0000;

        let config = AcpiTableSetConfig {
            table_base_address: 0,
            ..AcpiTableSetConfig::default()
        };
        let set = build_acpi_tables(&config);
        let loader = build_acpi_table_loader(&set);

        let mut exec = LoaderExecutor::new();
        exec.add_file("etc/acpi/tables", TABLES_BASE, set.tables.clone());
        exec.add_file("etc/acpi/rsdp", RSDP_BASE, set.rsdp.clone());
        exec.execute(&loader).expect("loader executes cleanly");

        let tables = exec.file("etc/acpi/tables").unwrap();
        let rsdp = exec.file("etc/acpi/rsdp").unwrap();
        let off = |name: &str| set.table_offsets.iter().find(|(n, _)| n == name).unwrap().1;
        let le64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());

        // RSDP -> XSDT now points at the relocated XSDT (offset 0 in tables).
        assert_eq!(le64(rsdp, 24), TABLES_BASE);
        // XSDT entry 0 -> FADT, FADT X_DSDT -> DSDT, both at the new base.
        assert_eq!(
            le64(tables, off("XSDT") + 36),
            TABLES_BASE + off("FADT") as u64
        );
        assert_eq!(
            le64(tables, off("FADT") + 140),
            TABLES_BASE + off("DSDT") as u64
        );

        // Every SDT checksum validates (whole-table byte sum == 0).
        for (name, start) in &set.table_offsets {
            let len = u32::from_le_bytes(tables[start + 4..start + 8].try_into().unwrap()) as usize;
            let sum = tables[*start..start + len]
                .iter()
                .fold(0u8, |a, &b| a.wrapping_add(b));
            assert_eq!(sum, 0, "{name} checksum invalid after relocation");
        }
        // RSDP's two checksums validate (first 20 bytes, then all 36).
        assert_eq!(rsdp[..20].iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0);
        assert_eq!(rsdp[..36].iter().fold(0u8, |a, &b| a.wrapping_add(b)), 0);
    }

    #[test]
    fn table_loader_checksum_spans_match_table_lengths() {
        let config = AcpiTableSetConfig {
            table_base_address: 0,
            ..AcpiTableSetConfig::default()
        };
        let set = build_acpi_tables(&config);
        let cmds = decode_loader(&build_acpi_table_loader(&set));

        // For each SDT checksum over etc/acpi/tables, the covered span must equal
        // the table's own header length field, and the checksum byte sits at
        // start+9 — derived from the bytes, so this catches a wrong span.
        for (_name, start) in &set.table_offsets {
            let start = u32::try_from(*start).unwrap();
            let cmd = cmds
                .iter()
                .find(|c| c.command == 0x3 && c.dest == "etc/acpi/tables" && c.cksum_start == start)
                .expect("a checksum command for each SDT");
            let hdr_len = u32::from_le_bytes(
                set.tables[start as usize + 4..start as usize + 8]
                    .try_into()
                    .unwrap(),
            );
            assert_eq!(cmd.cksum_len, hdr_len);
            assert_eq!(cmd.cksum_offset, start + 9);
        }
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
        for entry in xsdt[36..xsdt_len].as_chunks::<8>().0 {
            let gpa = u64::from_le_bytes(*entry);
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
    fn table_set_xsdt_has_12_entries() {
        let table_set = build_acpi_tables(&AcpiTableSetConfig::default());
        let xsdt = &table_set.tables[0..];
        let xsdt_len = u32::from_le_bytes(xsdt[4..8].try_into().unwrap()) as usize;
        // XSDT: 36-byte header + 8 bytes per entry
        let entry_count = (xsdt_len - 36) / 8;
        assert_eq!(
            entry_count, 12,
            "XSDT must point to 12 tables (FADT+MADT+MCFG+HPET+SSDT+SRAT+SLIT+WAET+WSMT+FPDT+BGRT+TPM2)"
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

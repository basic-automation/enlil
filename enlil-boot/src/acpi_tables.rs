//! The ACPI tables a guest OS reads, and the loader script that places them.
//!
//! OVMF does not ship ACPI tables for this machine; it asks `fw_cfg` for three
//! files and follows a small linker script (QEMU's "table loader") that says how
//! to place them in memory: `etc/acpi/tables` (every table back to back),
//! `etc/acpi/rsdp` (the root pointer) and `etc/table-loader` (the commands that
//! allocate those two blobs, add each blob's final address into the pointers
//! inside, and fill in the checksums). This module builds all three.
//!
//! The tables describe a uniprocessor i440FX-style PC: a local APIC and an I/O
//! APIC (Windows on x86-64 needs APIC interrupt delivery), the ACPI power
//! management block the platform model implements (the PM timer, `PM1_CNT`
//! soft-off, reset through `0xCF9`), and the AML machine description in
//! `acpi/dsdt.asl`.
//!
//! Pure and host-tested: the tests play the loader against the blobs exactly as
//! OVMF does and check the resulting tables.

extern crate alloc;

use alloc::vec::Vec;

/// The compiled machine description (`acpi/dsdt.asl`, via `scripts/build-dsdt.sh`).
pub const DSDT_AML: &[u8] = include_bytes!("acpi/dsdt.aml");

/// Name of the blob holding every table.
pub const FILE_TABLES: &str = "etc/acpi/tables";
/// Name of the blob holding the root pointer.
pub const FILE_RSDP: &str = "etc/acpi/rsdp";
/// Name of the loader script.
pub const FILE_LOADER: &str = "etc/table-loader";

/// Bytes in an ACPI table header.
const HEADER: usize = 36;
/// Bytes of one table-loader command.
const COMMAND: usize = 128;
/// Bytes of a file name field in a loader command.
const NAME: usize = 56;

/// Machine facts the tables encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpiParams {
    /// OEM id stamped in every header (6 bytes).
    pub oem_id: [u8; 6],
    /// OEM table id stamped in every header (8 bytes).
    pub oem_table_id: [u8; 8],
    /// I/O base of the PIIX4 power-management block (`PMBA`).
    pub pm_base: u16,
    /// Local APIC MMIO base.
    pub lapic_base: u32,
    /// I/O APIC MMIO base.
    pub ioapic_base: u32,
}

impl Default for AcpiParams {
    fn default() -> Self {
        Self {
            oem_id: *b"ENLIL ",
            oem_table_id: *b"ENLILVM ",
            pm_base: 0xB000,
            lapic_base: 0xFEE0_0000,
            ioapic_base: 0xFEC0_0000,
        }
    }
}

/// The three blobs `fw_cfg` publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpiBlobs {
    /// `etc/acpi/tables`.
    pub tables: Vec<u8>,
    /// `etc/acpi/rsdp`.
    pub rsdp: Vec<u8>,
    /// `etc/table-loader`.
    pub loader: Vec<u8>,
}

/// A table under construction inside the tables blob.
struct Layout {
    blob: Vec<u8>,
    /// `(offset, length)` of every table, for the checksum commands.
    tables: Vec<(usize, usize)>,
    /// `(offset, size)` of every pointer that must have the blob's address added.
    pointers: Vec<(usize, u8)>,
}

impl Layout {
    /// Append a table, 8-byte aligned (`align` of 64 for the FACS), returning its offset.
    fn push(&mut self, bytes: &[u8], align: usize, checksummed: bool) -> usize {
        while !self.blob.len().is_multiple_of(align) {
            self.blob.push(0);
        }
        let at = self.blob.len();
        self.blob.extend_from_slice(bytes);
        if checksummed {
            self.tables.push((at, bytes.len()));
        }
        at
    }

    fn put32(&mut self, at: usize, v: u32) {
        self.blob[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn put64(&mut self, at: usize, v: u64) {
        self.blob[at..at + 8].copy_from_slice(&v.to_le_bytes());
    }
}

/// A standard table header; the length is patched by the caller via `finish`.
fn header(sig: [u8; 4], revision: u8, p: &AcpiParams) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER);
    h.extend_from_slice(&sig);
    h.extend_from_slice(&0u32.to_le_bytes()); // length, set by `finish`
    h.push(revision);
    h.push(0); // checksum, set by the loader
    h.extend_from_slice(&p.oem_id);
    h.extend_from_slice(&p.oem_table_id);
    h.extend_from_slice(&1u32.to_le_bytes()); // OEM revision
    h.extend_from_slice(b"ENLL"); // creator id
    h.extend_from_slice(&1u32.to_le_bytes()); // creator revision
    h
}

/// Set a table's length field to its final size.
fn finish(mut t: Vec<u8>) -> Vec<u8> {
    let len = u32::try_from(t.len()).unwrap_or(0);
    t[4..8].copy_from_slice(&len.to_le_bytes());
    t
}

/// A Generic Address Structure for an I/O port register.
fn gas_io(width: u8, access: u8, port: u16) -> [u8; 12] {
    let mut g = [0u8; 12];
    g[0] = 1; // system I/O
    g[1] = width;
    g[3] = access;
    g[4..12].copy_from_slice(&u64::from(port).to_le_bytes());
    g
}

/// The FACS (firmware ACPI control structure): 64 bytes, not checksummed.
fn facs() -> Vec<u8> {
    let mut t = alloc::vec![0u8; 64];
    t[0..4].copy_from_slice(b"FACS");
    t[4..8].copy_from_slice(&64u32.to_le_bytes());
    t[32] = 2; // version
    t
}

/// The FADT (revision 5): where the power-management registers live and which
/// legacy hardware exists. Its table pointers are filled in by the loader.
fn fadt(p: &AcpiParams) -> Vec<u8> {
    let mut t = header(*b"FACP", 5, p);
    t.resize(268, 0);
    let put16 =
        |t: &mut Vec<u8>, at: usize, v: u16| t[at..at + 2].copy_from_slice(&v.to_le_bytes());
    let put32 =
        |t: &mut Vec<u8>, at: usize, v: u32| t[at..at + 4].copy_from_slice(&v.to_le_bytes());
    t[45] = 1; // preferred PM profile: desktop
    put16(&mut t, 46, 9); // SCI interrupt
    // No SMI command port: ACPI is always enabled (there is no SMM).
    put32(&mut t, 56, u32::from(p.pm_base)); // PM1a_EVT_BLK
    put32(&mut t, 64, u32::from(p.pm_base) + 4); // PM1a_CNT_BLK
    put32(&mut t, 76, u32::from(p.pm_base) + 8); // PM_TMR_BLK
    t[88] = 4; // PM1_EVT_LEN
    t[89] = 2; // PM1_CNT_LEN
    t[91] = 4; // PM_TMR_LEN
    put16(&mut t, 96, 0x0FFF); // P_LVL2_LAT: C2 not supported
    put16(&mut t, 98, 0x0FFF); // P_LVL3_LAT: C3 not supported
    t[108] = 0x32; // RTC century register
    // Legacy devices exist; no 8042; MSI and PCIe ASPM control not offered.
    put16(&mut t, 109, 0x0001 | 0x0008 | 0x0010);
    // WBINVD | PROC_C1 | no fixed power button | no fixed sleep button |
    // RTC not in fixed space | reset register supported.
    put32(
        &mut t,
        112,
        (1 << 0) | (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 10),
    );
    t[116..128].copy_from_slice(&gas_io(8, 1, 0x0CF9)); // RESET_REG
    t[128] = 0x0E; // RESET_VALUE
    t[148..160].copy_from_slice(&gas_io(32, 3, p.pm_base)); // X_PM1a_EVT_BLK
    t[172..184].copy_from_slice(&gas_io(16, 2, p.pm_base + 4)); // X_PM1a_CNT_BLK
    t[208..220].copy_from_slice(&gas_io(32, 3, p.pm_base + 8)); // X_PM_TMR_BLK
    finish(t)
}

/// The MADT: one local APIC, one I/O APIC, the timer and SCI interrupt overrides.
fn madt(p: &AcpiParams) -> Vec<u8> {
    let mut t = header(*b"APIC", 3, p);
    t.extend_from_slice(&p.lapic_base.to_le_bytes());
    t.extend_from_slice(&1u32.to_le_bytes()); // PCAT_COMPAT: dual 8259s exist
    // Processor local APIC: ACPI processor 0, APIC id 0, enabled.
    t.extend_from_slice(&[0, 8, 0, 0, 1, 0, 0, 0]);
    // I/O APIC: id 0, at its base, global system interrupts from 0.
    t.extend_from_slice(&[1, 12, 0, 0]);
    t.extend_from_slice(&p.ioapic_base.to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes());
    // Interrupt source override: the PIT's IRQ 0 arrives on I/O APIC pin 2.
    t.extend_from_slice(&[2, 10, 0, 0]);
    t.extend_from_slice(&2u32.to_le_bytes());
    t.extend_from_slice(&0u16.to_le_bytes());
    // The SCI (IRQ 9) is level-triggered, active high.
    t.extend_from_slice(&[2, 10, 0, 9]);
    t.extend_from_slice(&9u32.to_le_bytes());
    t.extend_from_slice(&0x000Du16.to_le_bytes());
    // Local APIC NMI: all processors, LINT1.
    t.extend_from_slice(&[4, 6, 0xFF]);
    t.extend_from_slice(&0u16.to_le_bytes());
    t.push(1);
    finish(t)
}

/// A root or extended system description table of `entries` (4- or 8-byte
/// addresses, as offsets into the blob until the loader adds the base).
fn sdt(sig: [u8; 4], revision: u8, wide: bool, entries: &[usize], p: &AcpiParams) -> Vec<u8> {
    let mut t = header(sig, revision, p);
    for &e in entries {
        if wide {
            t.extend_from_slice(&(e as u64).to_le_bytes());
        } else {
            t.extend_from_slice(&u32::try_from(e).unwrap_or(0).to_le_bytes());
        }
    }
    finish(t)
}

/// A NUL-padded file name field.
fn name_field(name: &str) -> [u8; NAME] {
    let mut f = [0u8; NAME];
    f[..name.len()].copy_from_slice(name.as_bytes());
    f
}

fn cmd_allocate(out: &mut Vec<u8>, file: &str, align: u32, zone: u8) {
    let mut c = [0u8; COMMAND];
    c[0..4].copy_from_slice(&1u32.to_le_bytes());
    c[4..4 + NAME].copy_from_slice(&name_field(file));
    c[4 + NAME..8 + NAME].copy_from_slice(&align.to_le_bytes());
    c[8 + NAME] = zone;
    out.extend_from_slice(&c);
}

fn cmd_add_pointer(out: &mut Vec<u8>, dest: &str, src: &str, offset: u32, size: u8) {
    let mut c = [0u8; COMMAND];
    c[0..4].copy_from_slice(&2u32.to_le_bytes());
    c[4..4 + NAME].copy_from_slice(&name_field(dest));
    c[4 + NAME..4 + 2 * NAME].copy_from_slice(&name_field(src));
    c[4 + 2 * NAME..8 + 2 * NAME].copy_from_slice(&offset.to_le_bytes());
    c[8 + 2 * NAME] = size;
    out.extend_from_slice(&c);
}

fn cmd_add_checksum(out: &mut Vec<u8>, file: &str, result: u32, start: u32, len: u32) {
    let mut c = [0u8; COMMAND];
    c[0..4].copy_from_slice(&3u32.to_le_bytes());
    c[4..4 + NAME].copy_from_slice(&name_field(file));
    c[4 + NAME..8 + NAME].copy_from_slice(&result.to_le_bytes());
    c[8 + NAME..12 + NAME].copy_from_slice(&start.to_le_bytes());
    c[12 + NAME..16 + NAME].copy_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&c);
}

/// Build the ACPI blobs for `params`, with `dsdt` as the machine description.
#[must_use]
pub fn build(params: &AcpiParams, dsdt: &[u8]) -> AcpiBlobs {
    let mut l = Layout {
        blob: Vec::new(),
        tables: Vec::new(),
        pointers: Vec::new(),
    };
    let dsdt_at = l.push(dsdt, 8, true);
    // The loader's checksum command adds to whatever the checksum cell holds
    // (it does not clear it first, as QEMU's tables never carry one), so the
    // valid checksum `iasl` wrote into the DSDT must be zeroed or the loader
    // would turn it into garbage.
    l.blob[dsdt_at + 9] = 0;
    let facs_at = l.push(&facs(), 64, false);
    let fadt_at = l.push(&fadt(params), 8, true);
    let madt_at = l.push(&madt(params), 8, true);
    let tables = [fadt_at, madt_at];
    let rsdt_at = l.push(&sdt(*b"RSDT", 1, false, &tables, params), 8, true);
    let xsdt_at = l.push(&sdt(*b"XSDT", 1, true, &tables, params), 8, true);

    // The FADT holds the FACS and DSDT as offsets for now.
    l.put32(fadt_at + 36, u32::try_from(facs_at).unwrap_or(0));
    l.put32(fadt_at + 40, u32::try_from(dsdt_at).unwrap_or(0));
    l.put64(fadt_at + 132, facs_at as u64);
    l.put64(fadt_at + 140, dsdt_at as u64);
    l.pointers.extend([
        (fadt_at + 36, 4),
        (fadt_at + 40, 4),
        (fadt_at + 132, 8),
        (fadt_at + 140, 8),
    ]);
    for i in 0..tables.len() {
        l.pointers.push((rsdt_at + HEADER + 4 * i, 4));
        l.pointers.push((xsdt_at + HEADER + 8 * i, 8));
    }

    // The root pointer: both system description tables, as offsets until loaded.
    let mut rsdp = alloc::vec![0u8; 36];
    rsdp[0..8].copy_from_slice(b"RSD PTR ");
    rsdp[9..15].copy_from_slice(&params.oem_id);
    rsdp[15] = 2;
    rsdp[16..20].copy_from_slice(&u32::try_from(rsdt_at).unwrap_or(0).to_le_bytes());
    rsdp[20..24].copy_from_slice(&36u32.to_le_bytes());
    rsdp[24..32].copy_from_slice(&(xsdt_at as u64).to_le_bytes());

    let mut loader = Vec::new();
    cmd_allocate(&mut loader, FILE_TABLES, 64, 1); // high memory
    cmd_allocate(&mut loader, FILE_RSDP, 16, 2); // F segment
    for &(offset, size) in &l.pointers {
        cmd_add_pointer(
            &mut loader,
            FILE_TABLES,
            FILE_TABLES,
            u32::try_from(offset).unwrap_or(0),
            size,
        );
    }
    for &(start, len) in &l.tables {
        cmd_add_checksum(
            &mut loader,
            FILE_TABLES,
            u32::try_from(start + 9).unwrap_or(0),
            u32::try_from(start).unwrap_or(0),
            u32::try_from(len).unwrap_or(0),
        );
    }
    cmd_add_pointer(&mut loader, FILE_RSDP, FILE_TABLES, 16, 4);
    cmd_add_pointer(&mut loader, FILE_RSDP, FILE_TABLES, 24, 8);
    cmd_add_checksum(&mut loader, FILE_RSDP, 8, 0, 20);
    cmd_add_checksum(&mut loader, FILE_RSDP, 32, 0, 36);

    AcpiBlobs {
        tables: l.blob,
        rsdp,
        loader,
    }
}

#[cfg(test)]
mod tests;

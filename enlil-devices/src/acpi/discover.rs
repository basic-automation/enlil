//! Runtime ACPI table discovery.
//!
//! The hypervisor delivers the ACPI tables to a guest via `fw_cfg`; the guest OS
//! reads them and places them in RAM wherever it likes, so the S3 resume path
//! cannot know the FACS address a priori — it must walk the guest's *live*
//! tables. This module does that walk: RSDP → XSDT/RSDT → FADT → FACS address,
//! over a view of guest RAM based at guest-physical 0.

use super::fadt::read_facs_address;

/// Read the ACPI table at guest-physical `gpa` from `mem` (RAM based at gpa 0),
/// sliced to the length in its SDT header (offset 4). `None` if the address is
/// out of range or the table overruns RAM.
fn table_at(mem: &[u8], gpa: u64) -> Option<&[u8]> {
    let start = usize::try_from(gpa).ok()?;
    let header = mem.get(start..start.checked_add(8)?)?;
    let length = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
    mem.get(start..start.checked_add(length)?)
}

/// Walk a guest's live ACPI tables to find its FACS physical address.
///
/// Validates the RSDP at `rsdp_gpa`, follows it to the XSDT (or the RSDT on an
/// ACPI-1.0 RSDP), finds the FADT (signature `FACP`) among the entries, and
/// reads its FACS pointer via [`read_facs_address`]. `mem` is guest RAM based at
/// guest-physical 0. Returns `None` if any step is missing or out of range.
/// Checksums are not verified — this reads a table set enlil (or the guest)
/// built, not an adversarial one.
#[must_use]
pub fn find_facs_address(mem: &[u8], rsdp_gpa: u64) -> Option<u64> {
    let start = usize::try_from(rsdp_gpa).ok()?;
    // ACPI 1.0 RSDP is 20 bytes (signature..RsdtAddress); 2.0+ extends to 36.
    let rsdp = mem.get(start..start.checked_add(20)?)?;
    if &rsdp[0..8] != b"RSD PTR " {
        return None;
    }
    let revision = rsdp[15];
    let legacy_rsdt = u64::from(u32::from_le_bytes(rsdp[16..20].try_into().ok()?));
    // Prefer the 64-bit XSDT (offset 24) on a 2.0+ RSDP when present.
    let desc_gpa = if revision >= 2
        && let Some(full) = mem.get(start..start.checked_add(32)?)
    {
        let ext_xsdt = u64::from_le_bytes(full[24..32].try_into().ok()?);
        if ext_xsdt != 0 { ext_xsdt } else { legacy_rsdt }
    } else {
        legacy_rsdt
    };

    let desc = table_at(mem, desc_gpa)?;
    if desc.len() < 36 {
        return None; // truncated system description table header
    }
    let entry_size = if &desc[0..4] == b"XSDT" { 8 } else { 4 };
    let mut off = 36; // past the 36-byte SDT header
    while off + entry_size <= desc.len() {
        let table_gpa = if entry_size == 8 {
            u64::from_le_bytes(desc[off..off + 8].try_into().ok()?)
        } else {
            u64::from(u32::from_le_bytes(desc[off..off + 4].try_into().ok()?))
        };
        off += entry_size;
        if let Some(table) = table_at(mem, table_gpa)
            && table.len() >= 4
            && &table[0..4] == b"FACP"
        {
            return read_facs_address(table);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::fadt::FadtBuilder;
    use crate::acpi::rsdp::RsdpBuilder;
    use crate::acpi::xsdt::XsdtBuilder;

    fn place(ram: &mut [u8], gpa: u64, bytes: &[u8]) {
        let o = usize::try_from(gpa).unwrap();
        ram[o..o + bytes.len()].copy_from_slice(bytes);
    }

    #[test]
    fn walks_rsdp_xsdt_fadt_to_the_facs() {
        const FACS_GPA: u64 = 0x1000;
        const FADT_GPA: u64 = 0x2000;
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let mut ram = vec![0u8; 0x6000];

        place(
            &mut ram,
            FADT_GPA,
            &FadtBuilder::new(0x3000).firmware_ctrl(FACS_GPA).build(),
        );
        place(
            &mut ram,
            XSDT_GPA,
            &XsdtBuilder::new().add_table(FADT_GPA).build(),
        );
        place(
            &mut ram,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );

        assert_eq!(
            find_facs_address(&ram, RSDP_GPA),
            Some(FACS_GPA),
            "discovery walks RSDP → XSDT → FADT → FACS"
        );
    }

    #[test]
    fn none_when_the_fadt_names_no_facs() {
        const FADT_GPA: u64 = 0x2000;
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let mut ram = vec![0u8; 0x6000];
        // FADT with FIRMWARE_CTRL left at 0 (no FACS).
        place(&mut ram, FADT_GPA, &FadtBuilder::new(0x3000).build());
        place(
            &mut ram,
            XSDT_GPA,
            &XsdtBuilder::new().add_table(FADT_GPA).build(),
        );
        place(
            &mut ram,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );
        assert_eq!(find_facs_address(&ram, RSDP_GPA), None);
    }

    #[test]
    fn rejects_a_bad_or_out_of_range_rsdp() {
        let ram = vec![0u8; 0x100];
        assert_eq!(find_facs_address(&ram, 0), None, "no RSD PTR signature");
        assert_eq!(
            find_facs_address(&ram, 0x1000),
            None,
            "rsdp gpa past the end of RAM"
        );
    }

    #[test]
    fn none_when_no_fadt_in_the_xsdt() {
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let mut ram = vec![0u8; 0x6000];
        // An XSDT that lists no tables at all.
        place(&mut ram, XSDT_GPA, &XsdtBuilder::new().build());
        place(
            &mut ram,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );
        assert_eq!(find_facs_address(&ram, RSDP_GPA), None);
    }
}

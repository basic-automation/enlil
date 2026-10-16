#![allow(clippy::cast_possible_truncation)] // fixture arithmetic on small bounded values

use super::*;
use alloc::collections::BTreeMap;

const TABLES_BASE: u64 = 0x7000_0000;
const RSDP_BASE: u64 = 0xE_0000;

/// The blobs after OVMF's loader has run over them at fixed base addresses.
struct Loaded {
    tables: Vec<u8>,
    rsdp: Vec<u8>,
}

fn rd(b: &[u8], at: usize, n: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..n {
        v |= u64::from(b[at + i]) << (8 * i);
    }
    v
}

fn wr(b: &mut [u8], at: usize, n: usize, v: u64) {
    for i in 0..n {
        b[at + i] = (v >> (8 * i)) as u8;
    }
}

fn name_of(f: &[u8]) -> String {
    let end = f.iter().position(|&b| b == 0).unwrap_or(f.len());
    String::from(core::str::from_utf8(&f[..end]).unwrap())
}

/// Run the loader script the way OVMF's `QemuFwCfgAcpi.c` does.
fn run_loader(blobs: &AcpiBlobs) -> Loaded {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    files.insert(FILE_TABLES.into(), blobs.tables.clone());
    files.insert(FILE_RSDP.into(), blobs.rsdp.clone());
    let mut base: BTreeMap<String, u64> = BTreeMap::new();
    assert_eq!(blobs.loader.len() % COMMAND, 0, "whole commands only");
    for c in blobs.loader.chunks(COMMAND) {
        match rd(c, 0, 4) {
            1 => {
                let file = name_of(&c[4..4 + NAME]);
                let align = rd(c, 4 + NAME, 4);
                assert!(
                    align.is_power_of_two() && align <= 4096,
                    "alignment {align}"
                );
                assert!(files.contains_key(&file), "ALLOCATE of unknown file {file}");
                let at = if file == FILE_TABLES {
                    TABLES_BASE
                } else {
                    RSDP_BASE
                };
                assert_eq!(at % align, 0);
                assert!(base.insert(file, at).is_none(), "allocated twice");
            }
            2 => {
                let dest = name_of(&c[4..4 + NAME]);
                let src = name_of(&c[4 + NAME..4 + 2 * NAME]);
                let off = rd(c, 4 + 2 * NAME, 4) as usize;
                let size = c[8 + 2 * NAME] as usize;
                assert!([1, 2, 4, 8].contains(&size));
                let add = *base.get(&src).expect("pointee allocated first");
                assert!(base.contains_key(&dest), "pointer file allocated first");
                let f = files.get_mut(&dest).unwrap();
                let v = rd(f, off, size).wrapping_add(add);
                wr(f, off, size, v);
            }
            3 => {
                let file = name_of(&c[4..4 + NAME]);
                let result = rd(c, 4 + NAME, 4) as usize;
                let start = rd(c, 8 + NAME, 4) as usize;
                let len = rd(c, 12 + NAME, 4) as usize;
                let f = files.get_mut(&file).unwrap();
                // As OVMF does: the result is minus the sum of the range *as it
                // stands*, so a cell that is not already zero corrupts the table.
                let sum: u8 = f[start..start + len]
                    .iter()
                    .fold(0u8, |a, &b| a.wrapping_add(b));
                f[result] = 0u8.wrapping_sub(sum);
            }
            other => panic!("unknown loader command {other}"),
        }
    }
    Loaded {
        tables: files.remove(FILE_TABLES).unwrap(),
        rsdp: files.remove(FILE_RSDP).unwrap(),
    }
}

fn sum(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
}

/// The table at guest-physical `addr` inside the loaded tables blob.
fn table_at(l: &Loaded, addr: u64) -> &[u8] {
    let off = (addr - TABLES_BASE) as usize;
    let len = rd(&l.tables, off + 4, 4) as usize;
    &l.tables[off..off + len]
}

fn loaded() -> Loaded {
    run_loader(&build(&AcpiParams::default(), DSDT_AML))
}

#[test]
fn the_root_pointer_checksums_and_points_at_both_system_tables() {
    let l = loaded();
    let r = &l.rsdp;
    assert_eq!(&r[0..8], b"RSD PTR ");
    assert_eq!(sum(&r[..20]), 0, "ACPI 1.0 checksum");
    assert_eq!(sum(&r[..36]), 0, "extended checksum");
    assert_eq!(r[15], 2, "ACPI 2.0+");
    assert_eq!(rd(r, 20, 4), 36);
    let rsdt = table_at(&l, rd(r, 16, 4));
    let xsdt = table_at(&l, rd(r, 24, 8));
    assert_eq!(&rsdt[0..4], b"RSDT");
    assert_eq!(&xsdt[0..4], b"XSDT");
    assert_eq!(&r[9..15], b"ENLIL ");
}

#[test]
fn every_table_has_the_right_length_and_a_valid_checksum() {
    let l = loaded();
    let r = &l.rsdp;
    let mut seen = Vec::new();
    for sdt_addr in [rd(r, 16, 4), rd(r, 24, 8)] {
        let sdt = table_at(&l, sdt_addr);
        assert_eq!(sum(sdt), 0, "{:?}", &sdt[0..4]);
        let wide = &sdt[0..4] == b"XSDT";
        let n = (sdt.len() - HEADER) / if wide { 8 } else { 4 };
        let mut sigs = Vec::new();
        for i in 0..n {
            let addr = if wide {
                rd(sdt, HEADER + 8 * i, 8)
            } else {
                rd(sdt, HEADER + 4 * i, 4)
            };
            let t = table_at(&l, addr);
            assert_eq!(sum(t), 0, "{:?}", &t[0..4]);
            sigs.push([t[0], t[1], t[2], t[3]]);
        }
        seen.push(sigs);
    }
    // The 32-bit and 64-bit tables list the same two tables.
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], [*b"FACP", *b"APIC"]);
}

#[test]
fn the_fadt_links_the_dsdt_and_facs_and_describes_power_management() {
    let l = loaded();
    let xsdt = table_at(&l, rd(&l.rsdp, 24, 8));
    let fadt = table_at(&l, rd(xsdt, HEADER, 8));
    assert_eq!(&fadt[0..4], b"FACP");
    assert_eq!(
        (fadt[8], fadt.len()),
        (5, 268),
        "revision 5, ACPI 5.0 length"
    );
    // Both the 32- and 64-bit pointers reach the same, valid tables.
    let dsdt32 = table_at(&l, rd(fadt, 40, 4));
    let dsdt64 = table_at(&l, rd(fadt, 140, 8));
    assert_eq!(dsdt32.as_ptr(), dsdt64.as_ptr());
    assert_eq!(&dsdt32[0..4], b"DSDT");
    assert_eq!(dsdt32.len(), DSDT_AML.len());
    assert_eq!(sum(dsdt32), 0, "the loader re-checksums the compiled DSDT");
    let facs_off = (rd(fadt, 36, 4) - TABLES_BASE) as usize;
    assert_eq!(&l.tables[facs_off..facs_off + 4], b"FACS");
    assert_eq!(facs_off % 64, 0, "the FACS must be 64-byte aligned");
    assert_eq!(rd(fadt, 132, 8), rd(fadt, 36, 4), "X_FIRMWARE_CTRL");
    // Power management: the blocks the platform model implements.
    assert_eq!(rd(fadt, 56, 4), 0xB000);
    assert_eq!(rd(fadt, 64, 4), 0xB004);
    assert_eq!(rd(fadt, 76, 4), 0xB008);
    assert_eq!((fadt[88], fadt[89], fadt[91]), (4, 2, 4));
    assert_eq!(rd(fadt, 48, 4), 0, "no SMI command: ACPI is always on");
    assert_eq!(rd(fadt, 80, 4), 0, "no GPE block");
    // Reset through the 0xCF9 register with a value that sets its reset bit.
    assert_eq!(fadt[116], 1, "I/O space");
    assert_eq!(rd(fadt, 120, 8), 0xCF9);
    assert_ne!(fadt[128] & 0x04, 0);
    assert_ne!(rd(fadt, 112, 4) & (1 << 10), 0, "RESET_REG_SUP");
    // The extended registers agree with the legacy ones.
    assert_eq!(rd(fadt, 152, 8), 0xB000);
    assert_eq!(rd(fadt, 176, 8), 0xB004);
    assert_eq!(rd(fadt, 212, 8), 0xB008);
}

#[test]
fn the_madt_describes_one_cpu_and_the_apics() {
    let l = loaded();
    let xsdt = table_at(&l, rd(&l.rsdp, 24, 8));
    let madt = table_at(&l, rd(xsdt, HEADER + 8, 8));
    assert_eq!(&madt[0..4], b"APIC");
    assert_eq!(rd(madt, 36, 4), 0xFEE0_0000, "local APIC address");
    assert_eq!(rd(madt, 40, 4) & 1, 1, "PCAT_COMPAT");
    // Walk the entries.
    let mut at = 44;
    let mut kinds = Vec::new();
    while at < madt.len() {
        let (kind, len) = (madt[at], madt[at + 1] as usize);
        assert!(
            len >= 2 && at + len <= madt.len(),
            "entry stays inside the table"
        );
        match kind {
            0 => assert_eq!((madt[at + 2], madt[at + 3], rd(madt, at + 4, 4)), (0, 0, 1)),
            1 => {
                assert_eq!(rd(madt, at + 4, 4), 0xFEC0_0000, "I/O APIC address");
                assert_eq!(rd(madt, at + 8, 4), 0, "GSI base");
            }
            2 => {
                let (irq, gsi) = (madt[at + 3], rd(madt, at + 4, 4));
                assert!(
                    matches!((irq, gsi), (0, 2) | (9, 9)),
                    "override {irq} -> {gsi}"
                );
            }
            4 => assert_eq!(
                (madt[at + 2], madt[at + 5]),
                (0xFF, 1),
                "NMI on LINT1 of all CPUs"
            ),
            other => panic!("unexpected MADT entry {other}"),
        }
        kinds.push(kind);
        at += len;
    }
    assert_eq!(at, madt.len());
    assert_eq!(kinds, [0, 1, 2, 2, 4]);
}

#[test]
fn every_table_enters_the_loader_with_a_zero_checksum_cell() {
    // The compiled DSDT carries a valid checksum; the blob must not.
    assert_ne!(
        DSDT_AML[9], 0,
        "iasl wrote a real checksum (the case this guards)"
    );
    let blobs = build(&AcpiParams::default(), DSDT_AML);
    for c in blobs.loader.chunks(COMMAND).filter(|c| rd(c, 0, 4) == 3) {
        let (file, result) = (name_of(&c[4..4 + NAME]), rd(c, 4 + NAME, 4) as usize);
        let blob = if file == FILE_TABLES {
            &blobs.tables
        } else {
            &blobs.rsdp
        };
        assert_eq!(blob[result], 0, "{file}+{result:#x}");
    }
}

#[test]
fn the_dsdt_declares_the_root_bridge_and_the_soft_off_state() {
    let contains = |needle: &[u8]| DSDT_AML.windows(needle.len()).any(|w| w == needle);
    assert_eq!(&DSDT_AML[0..4], b"DSDT");
    assert_eq!(rd(DSDT_AML, 4, 4) as usize, DSDT_AML.len());
    assert!(contains(b"PCI0") && contains(b"_PRT") && contains(b"_CRS"));
    assert!(contains(b"_S5_"), "soft-off sleep state");
    assert!(contains(b"CPU0") && contains(b"COM1") && contains(b"RTC_"));
}

#[test]
fn the_loader_script_is_whole_commands_in_a_valid_order() {
    let blobs = build(&AcpiParams::default(), DSDT_AML);
    assert_eq!(blobs.loader.len() % COMMAND, 0);
    let kinds: Vec<u64> = blobs.loader.chunks(COMMAND).map(|c| rd(c, 0, 4)).collect();
    // Two allocations first, then pointer fix-ups, then checksums (a checksum
    // must come after the pointers inside its table are final).
    assert_eq!(&kinds[..2], &[1, 1]);
    let first_checksum = kinds.iter().position(|&k| k == 3).unwrap();
    let last_table_pointer = kinds[..first_checksum]
        .iter()
        .rposition(|&k| k == 2)
        .unwrap();
    assert!(last_table_pointer < first_checksum);
    assert_eq!(kinds.iter().filter(|&&k| k == 1).count(), 2);
    assert_eq!(
        kinds.iter().filter(|&&k| k == 2).count(),
        4 + 4 + 2,
        "FADT, RSDT+XSDT, RSDP"
    );
    assert_eq!(
        kinds.iter().filter(|&&k| k == 3).count(),
        5 + 2,
        "five tables, two RSDP sums"
    );
}

#[test]
fn oem_identity_and_platform_addresses_follow_the_parameters() {
    let p = AcpiParams {
        oem_id: *b"ALASKA",
        oem_table_id: *b"A M I   ",
        pm_base: 0x1800,
        lapic_base: 0xFEE0_0000,
        ioapic_base: 0xFEC0_1000,
    };
    let l = run_loader(&build(&p, DSDT_AML));
    assert_eq!(&l.rsdp[9..15], b"ALASKA");
    let xsdt = table_at(&l, rd(&l.rsdp, 24, 8));
    assert_eq!(&xsdt[10..16], b"ALASKA");
    assert_eq!(&xsdt[16..24], b"A M I   ");
    let fadt = table_at(&l, rd(xsdt, HEADER, 8));
    assert_eq!(rd(fadt, 56, 4), 0x1800);
    assert_eq!(rd(fadt, 76, 4), 0x1808);
    let madt = table_at(&l, rd(xsdt, HEADER + 8, 8));
    assert_eq!(
        rd(madt, 44 + 8 + 4, 4),
        0xFEC0_1000,
        "the I/O APIC entry's address"
    );
}

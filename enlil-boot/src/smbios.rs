//! SMBIOS: the machine's own description of itself (vendor, board, serial
//! numbers, UUID), which operating systems read to identify the hardware.
//!
//! Windows ties its activation to this, and anything that wants to tell a virtual
//! machine from a real one looks at it. So a guest must see the *real* board's
//! tables, not invented ones: the UEFI stage copies the firmware's SMBIOS
//! structure table into memory the kernel keeps, and this module reads the
//! entry point that locates it, rebuilds a valid entry point for the copy, and
//! pulls the machine UUID out of it for `fw_cfg`.
//!
//! Pure and host-tested.

extern crate alloc;

use alloc::vec::Vec;

/// An SMBIOS 3.x entry point (`_SM3_`) is 24 bytes.
const V3_LEN: usize = 24;
/// An SMBIOS 2.x entry point (`_SM_`) is 31 bytes.
const V2_LEN: usize = 31;

/// Where the structure table is and how big, from an entry point's bytes.
///
/// Understands both the 32-bit (`_SM_`) and 64-bit (`_SM3_`) entry points.
#[must_use]
pub fn table_extent(entry: &[u8]) -> Option<(u64, usize)> {
    if entry.starts_with(b"_SM3_") && entry.len() >= V3_LEN {
        let len = u32::from_le_bytes(entry[12..16].try_into().ok()?);
        let addr = u64::from_le_bytes(entry[16..24].try_into().ok()?);
        return Some((addr, usize::try_from(len).ok()?));
    }
    if entry.starts_with(b"_SM_") && entry.len() >= V2_LEN && &entry[16..21] == b"_DMI_" {
        let len = u16::from_le_bytes(entry[22..24].try_into().ok()?);
        let addr = u32::from_le_bytes(entry[24..28].try_into().ok()?);
        return Some((u64::from(addr), usize::from(len)));
    }
    None
}

/// How many bytes of the entry point itself to read, from its first bytes.
#[must_use]
pub fn entry_len(first: &[u8]) -> Option<usize> {
    if first.starts_with(b"_SM3_") {
        Some(V3_LEN)
    } else if first.starts_with(b"_SM_") {
        Some(V2_LEN)
    } else {
        None
    }
}

/// Make `sum` of `bytes` zero by adjusting the byte at `at`.
fn fix_checksum(bytes: &mut [u8], at: usize, start: usize, len: usize) {
    bytes[at] = 0;
    let sum: u8 = bytes[start..start + len]
        .iter()
        .fold(0u8, |a, &b| a.wrapping_add(b));
    bytes[at] = 0u8.wrapping_sub(sum);
}

/// A copy of the firmware's entry point describing a table of `tables_len` bytes.
///
/// The table address is cleared (the guest firmware places the table itself)
/// and every checksum redone. `None` if `entry` is not an entry point.
#[must_use]
pub fn rebuild_anchor(entry: &[u8], tables_len: usize) -> Option<Vec<u8>> {
    let len = entry_len(entry)?;
    let mut a = entry.get(..len)?.to_vec();
    let tl = u32::try_from(tables_len).ok()?;
    if len == V3_LEN {
        a[12..16].copy_from_slice(&tl.to_le_bytes());
        a[16..24].fill(0);
        fix_checksum(&mut a, 5, 0, V3_LEN);
    } else {
        a[22..24].copy_from_slice(&u16::try_from(tables_len).ok()?.to_le_bytes());
        a[24..28].fill(0);
        // The intermediate checksum covers `_DMI_` onward; the main one all 31.
        fix_checksum(&mut a, 21, 16, V2_LEN - 16);
        fix_checksum(&mut a, 4, 0, V2_LEN);
    }
    Some(a)
}

/// One structure in a table: its type, the formatted area, and where it ends
/// (after its string set's double NUL).
struct Structure<'a> {
    kind: u8,
    formatted: &'a [u8],
}

/// Walk the structures of `tables`, stopping at the end-of-table structure or a
/// malformed one.
fn structures(tables: &[u8]) -> impl Iterator<Item = Structure<'_>> {
    let mut at = 0usize;
    core::iter::from_fn(move || {
        let head = tables.get(at..at + 4)?;
        let (kind, len) = (head[0], usize::from(head[1]));
        if len < 4 {
            return None;
        }
        let formatted = tables.get(at..at + len)?;
        // The strings follow, ended by two NULs (or one more NUL if there are none).
        let mut end = at + len;
        loop {
            let pair = tables.get(end..end + 2)?;
            if pair == [0, 0] {
                end += 2;
                break;
            }
            end += 1;
        }
        let s = Structure { kind, formatted };
        at = if kind == 127 { tables.len() } else { end };
        Some(s)
    })
}

/// The machine UUID from the System Information structure (type 1), if the
/// table has one that is not all zeros or all ones (which firmware uses for
/// "not set").
#[must_use]
pub fn system_uuid(tables: &[u8]) -> Option<[u8; 16]> {
    let s = structures(tables).find(|s| s.kind == 1)?;
    let uuid: [u8; 16] = s.formatted.get(8..24)?.try_into().ok()?;
    (uuid != [0; 16] && uuid != [0xFF; 16]).then_some(uuid)
}

/// How many structures `tables` holds (up to and including the end marker).
#[must_use]
pub fn structure_count(tables: &[u8]) -> usize {
    structures(tables).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn structure(kind: u8, formatted: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut s = Vec::new();
        s.push(kind);
        s.push(u8::try_from(4 + formatted.len()).unwrap());
        s.extend_from_slice(&[0, 0]); // handle
        s.extend_from_slice(formatted);
        if strings.is_empty() {
            s.extend_from_slice(&[0, 0]);
        } else {
            for t in strings {
                s.extend_from_slice(t.as_bytes());
                s.push(0);
            }
            s.push(0);
        }
        s
    }

    fn sample_tables() -> Vec<u8> {
        let mut t = Vec::new();
        t.extend(structure(
            0,
            &[1, 2, 0xE8, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &["AMI", "3886", "06/24/2026"],
        ));
        let mut f = alloc::vec![1u8, 2, 0, 0]; // manufacturer, product, version, serial strings
        f.extend_from_slice(b"0123456789abcdef"); // UUID at offset 8 of the structure
        f.extend_from_slice(&[6, 0, 0, 0]);
        t.extend(structure(1, &f, &["ASUS", "PRIME"]));
        t.extend(structure(127, &[], &[]));
        t
    }

    fn v3_entry(tables_len: u32, addr: u64) -> Vec<u8> {
        let mut e = alloc::vec![0u8; 24];
        e[0..5].copy_from_slice(b"_SM3_");
        e[6] = 24;
        e[7] = 3;
        e[8] = 4;
        e[12..16].copy_from_slice(&tables_len.to_le_bytes());
        e[16..24].copy_from_slice(&addr.to_le_bytes());
        fix_checksum(&mut e, 5, 0, 24);
        e
    }

    fn v2_entry(tables_len: u16, addr: u32, count: u16) -> Vec<u8> {
        let mut e = alloc::vec![0u8; 31];
        e[0..4].copy_from_slice(b"_SM_");
        e[5] = 31;
        e[6] = 2;
        e[7] = 8;
        e[16..21].copy_from_slice(b"_DMI_");
        e[22..24].copy_from_slice(&tables_len.to_le_bytes());
        e[24..28].copy_from_slice(&addr.to_le_bytes());
        e[28..30].copy_from_slice(&count.to_le_bytes());
        fix_checksum(&mut e, 21, 16, 15);
        fix_checksum(&mut e, 4, 0, 31);
        e
    }

    fn sum(b: &[u8]) -> u8 {
        b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
    }

    #[test]
    fn both_entry_point_kinds_locate_their_table() {
        assert_eq!(
            table_extent(&v3_entry(3652, 0xBD00_0000)),
            Some((0xBD00_0000, 3652))
        );
        assert_eq!(
            table_extent(&v2_entry(1500, 0xF_0000, 20)),
            Some((0xF_0000, 1500))
        );
        assert_eq!(table_extent(b"not an entry point at all......"), None);
        assert_eq!(table_extent(&v3_entry(1, 2)[..10]), None, "truncated");
        // A 2.x anchor without the intermediate marker is rejected.
        let mut bad = v2_entry(1, 2, 3);
        bad[16] = b'X';
        assert_eq!(table_extent(&bad), None);
    }

    #[test]
    fn the_entry_length_follows_the_signature() {
        assert_eq!(entry_len(b"_SM3_xxxx"), Some(24));
        assert_eq!(entry_len(b"_SM_xxxxx"), Some(31));
        assert_eq!(entry_len(b"RSD PTR "), None);
    }

    #[test]
    fn a_rebuilt_anchor_describes_the_copy_and_checksums() {
        let a3 = rebuild_anchor(&v3_entry(3652, 0xBD00_0000), 3000).unwrap();
        assert_eq!(a3.len(), 24);
        assert_eq!(
            table_extent(&a3),
            Some((0, 3000)),
            "address cleared, length updated"
        );
        assert_eq!(sum(&a3), 0);
        assert_eq!((a3[7], a3[8]), (3, 4), "the version survives");

        let a2 = rebuild_anchor(&v2_entry(1500, 0xF_0000, 20), 1200).unwrap();
        assert_eq!(a2.len(), 31);
        assert_eq!(table_extent(&a2), Some((0, 1200)));
        assert_eq!(sum(&a2), 0, "main checksum");
        assert_eq!(sum(&a2[16..31]), 0, "intermediate checksum");
        assert_eq!(
            u16::from_le_bytes([a2[28], a2[29]]),
            20,
            "structure count kept"
        );
        assert_eq!(rebuild_anchor(b"junk", 10), None);
    }

    #[test]
    fn the_machine_uuid_comes_from_the_system_information_structure() {
        assert_eq!(system_uuid(&sample_tables()), Some(*b"0123456789abcdef"));
    }

    #[test]
    fn an_unset_uuid_is_not_reported() {
        for fill in [0x00u8, 0xFF] {
            let mut f = alloc::vec![1u8, 2, 0, 0];
            f.extend_from_slice(&[fill; 16]);
            f.extend_from_slice(&[6, 0, 0, 0]);
            let mut t = structure(1, &f, &["a", "b"]);
            t.extend(structure(127, &[], &[]));
            assert_eq!(system_uuid(&t), None, "fill {fill:#x}");
        }
        assert_eq!(system_uuid(&[]), None, "no tables");
        assert_eq!(system_uuid(&structure(127, &[], &[])), None, "no type 1");
    }

    #[test]
    fn walking_counts_structures_and_stops_on_garbage() {
        assert_eq!(structure_count(&sample_tables()), 3);
        // A structure whose length runs off the end ends the walk without a panic.
        let mut t = sample_tables();
        t.truncate(t.len() - 10);
        let _ = structure_count(&t);
        assert_eq!(
            structure_count(&[0, 2, 0, 0]),
            0,
            "length below the header size"
        );
        assert_eq!(
            structure_count(&[1, 4, 0, 0, 7, 7]),
            0,
            "strings never terminate"
        );
    }
}

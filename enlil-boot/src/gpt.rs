//! A read-only GUID Partition Table (GPT) reader.
//!
//! A firmware guest finds its operating system through a *boot entry* whose
//! device path names a partition by its GPT identity: the partition number,
//! where it starts and how long it is, and its unique GUID. The hypervisor
//! builds that entry itself (see [`crate::efivars`]), so it reads the guest
//! disk's partition table: the header in LBA 1 and the entry array it points to.
//!
//! The header's CRC is checked (a disk without a valid primary GPT is refused
//! rather than guessed at); the entry array's CRC is not, so a table with one
//! damaged entry still yields the others. Nothing here writes.
//!
//! Pure and host-tested.

extern crate alloc;

use alloc::vec::Vec;

use crate::blockdev::{BlockBackend, SECTOR};

/// The header signature, `"EFI PART"`.
const SIGNATURE: &[u8; 8] = b"EFI PART";
/// The header's LBA.
const HEADER_LBA: u64 = 1;
/// Bytes of the header the CRC covers at least (`HeaderSize` of a 1.0 header).
const MIN_HEADER_BYTES: usize = 92;
/// Smallest partition-entry size the specification allows.
const MIN_ENTRY_BYTES: usize = 128;
/// A bound on the entry size (a sector): larger is certainly corrupt.
const MAX_ENTRY_BYTES: usize = SECTOR;

/// Where the partition entries are, from a validated header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// LBA of the first partition entry.
    pub entries_lba: u64,
    /// Entries in the array (used or not).
    pub entry_count: u32,
    /// Bytes per entry.
    pub entry_bytes: u32,
}

/// One used partition entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    /// The partition's 1-based position in the entry array.
    pub number: u32,
    /// The partition-type GUID, in on-disk (mixed-endian) byte order.
    pub type_guid: [u8; 16],
    /// The partition's unique GUID, in on-disk (mixed-endian) byte order.
    pub unique_guid: [u8; 16],
    /// First sector.
    pub first_lba: u64,
    /// Last sector (inclusive).
    pub last_lba: u64,
}

impl Partition {
    /// Sectors in the partition.
    #[must_use]
    pub const fn sectors(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

/// Why a partition could not be found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GptError {
    /// A read failed.
    Io,
    /// The disk has no valid GPT header.
    NoGpt,
    /// The partition number is past the array, or that entry is unused.
    NoSuchPartition,
}

/// The CRC-32 (IEEE, as used by GPT and zlib) of `data`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Validate the header in `sector` (the contents of LBA 1).
#[must_use]
pub fn parse_header(sector: &[u8]) -> Option<Header> {
    if sector.get(..8)? != SIGNATURE {
        return None;
    }
    let size = usize::try_from(le32(sector, 12)?).ok()?;
    if !(MIN_HEADER_BYTES..=sector.len()).contains(&size) {
        return None;
    }
    // The CRC covers `size` bytes with its own field (offset 16) read as zero.
    let mut covered = sector.get(..size)?.to_vec();
    let stored = le32(&covered, 16)?;
    covered[16..20].fill(0);
    if crc32(&covered) != stored {
        return None;
    }
    let entry_bytes = le32(sector, 84)?;
    let eb = usize::try_from(entry_bytes).ok()?;
    if !(MIN_ENTRY_BYTES..=MAX_ENTRY_BYTES).contains(&eb) || !eb.is_multiple_of(8) {
        return None;
    }
    Some(Header {
        entries_lba: le64(sector, 72)?,
        entry_count: le32(sector, 80)?,
        entry_bytes,
    })
}

/// Parse a partition entry, or `None` for an unused one (all-zero type GUID) or
/// one whose range is backwards.
#[must_use]
pub fn parse_entry(number: u32, entry: &[u8]) -> Option<Partition> {
    let type_guid: [u8; 16] = entry.get(0..16)?.try_into().ok()?;
    let unique_guid: [u8; 16] = entry.get(16..32)?.try_into().ok()?;
    let (first_lba, last_lba) = (le64(entry, 32)?, le64(entry, 40)?);
    if type_guid == [0; 16] || last_lba < first_lba {
        return None;
    }
    Some(Partition {
        number,
        type_guid,
        unique_guid,
        first_lba,
        last_lba,
    })
}

/// Read partition `number` (1-based, as in `nvme0n1p<number>` and a UEFI
/// `HD()` device-path node) from `disk`.
///
/// # Errors
///
/// [`GptError::NoGpt`] if LBA 1 is not a valid GPT header,
/// [`GptError::NoSuchPartition`] if `number` is zero, past the table or unused,
/// [`GptError::Io`] if the disk cannot be read.
pub fn find_partition<B: BlockBackend>(disk: &mut B, number: u32) -> Result<Partition, GptError> {
    let mut sector = [0u8; SECTOR];
    disk.read(HEADER_LBA, &mut sector)
        .map_err(|_| GptError::Io)?;
    let header = parse_header(&sector).ok_or(GptError::NoGpt)?;
    if number == 0 || number > header.entry_count {
        return Err(GptError::NoSuchPartition);
    }
    // Byte offset of the entry within the array, and the sector(s) holding it.
    let at = u64::from(number - 1) * u64::from(header.entry_bytes);
    let first = header.entries_lba + at / SECTOR as u64;
    let within = usize::try_from(at % SECTOR as u64).map_err(|_| GptError::Io)?;
    let want = within + header.entry_bytes as usize;
    let mut buf: Vec<u8> = alloc::vec![0; want.div_ceil(SECTOR) * SECTOR];
    disk.read(first, &mut buf).map_err(|_| GptError::Io)?;
    parse_entry(number, &buf[within..want]).ok_or(GptError::NoSuchPartition)
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // small, bounded test values
mod tests {
    use super::*;
    use crate::blockdev::RamDisk;

    const ESP_TYPE: [u8; 16] = [
        0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9,
        0x3B,
    ];

    /// A disk with a GPT: `entries` as (type GUID, unique GUID, first, last),
    /// each `entry_bytes` long, stored from LBA 2.
    fn gpt_disk(entries: &[([u8; 16], [u8; 16], u64, u64)], entry_bytes: usize) -> RamDisk {
        let mut d = RamDisk::new(128);
        let count = 16u32;
        {
            let b = d.bytes_mut();
            for (i, (t, u, first, last)) in entries.iter().enumerate() {
                let at = 2 * SECTOR + i * entry_bytes;
                b[at..at + 16].copy_from_slice(t);
                b[at + 16..at + 32].copy_from_slice(u);
                b[at + 32..at + 40].copy_from_slice(&first.to_le_bytes());
                b[at + 40..at + 48].copy_from_slice(&last.to_le_bytes());
            }
            let h = &mut b[SECTOR..SECTOR + 92];
            h[0..8].copy_from_slice(SIGNATURE);
            h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            h[12..16].copy_from_slice(&92u32.to_le_bytes());
            h[24..32].copy_from_slice(&1u64.to_le_bytes());
            h[72..80].copy_from_slice(&2u64.to_le_bytes());
            h[80..84].copy_from_slice(&count.to_le_bytes());
            h[84..88].copy_from_slice(&u32::try_from(entry_bytes).unwrap().to_le_bytes());
            let crc = crc32(h);
            h[16..20].copy_from_slice(&crc.to_le_bytes());
        }
        d
    }

    fn guid(seed: u8) -> [u8; 16] {
        core::array::from_fn(|i| seed.wrapping_add(i as u8))
    }

    #[test]
    fn the_crc_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn a_partition_is_found_by_its_one_based_number() {
        let mut d = gpt_disk(
            &[
                (ESP_TYPE, guid(1), 2048, 4095),
                ([0; 16], [0; 16], 0, 0),
                (guid(9), guid(3), 4096, 8191),
            ],
            128,
        );
        let p1 = find_partition(&mut d, 1).unwrap();
        assert_eq!(p1.number, 1);
        assert_eq!(p1.type_guid, ESP_TYPE);
        assert_eq!(p1.unique_guid, guid(1));
        assert_eq!(
            (p1.first_lba, p1.last_lba, p1.sectors()),
            (2048, 4095, 2048)
        );
        let p3 = find_partition(&mut d, 3).unwrap();
        assert_eq!((p3.number, p3.first_lba, p3.sectors()), (3, 4096, 4096));
    }

    #[test]
    fn unused_and_out_of_range_numbers_are_refused() {
        let mut d = gpt_disk(&[(ESP_TYPE, guid(1), 34, 99)], 128);
        assert_eq!(find_partition(&mut d, 0), Err(GptError::NoSuchPartition));
        assert_eq!(find_partition(&mut d, 2), Err(GptError::NoSuchPartition));
        assert_eq!(find_partition(&mut d, 17), Err(GptError::NoSuchPartition));
        assert!(find_partition(&mut d, 1).is_ok());
    }

    #[test]
    fn a_larger_entry_size_is_honoured() {
        // 256-byte entries: the second entry starts in the first sector's tail.
        let mut d = gpt_disk(
            &[(ESP_TYPE, guid(1), 34, 99), (guid(5), guid(2), 100, 199)],
            256,
        );
        let p = find_partition(&mut d, 2).unwrap();
        assert_eq!((p.first_lba, p.last_lba), (100, 199));
        assert_eq!(p.unique_guid, guid(2));
    }

    #[test]
    fn an_entry_straddling_a_sector_boundary_is_read_whole() {
        // 384-byte entries are not a power of two, but are multiples of 8: the
        // second one begins at byte 384 and ends at 768, across the boundary.
        let mut d = gpt_disk(
            &[(ESP_TYPE, guid(1), 34, 99), (guid(5), guid(2), 100, 199)],
            384,
        );
        let p = find_partition(&mut d, 2).unwrap();
        assert_eq!((p.first_lba, p.last_lba), (100, 199));
    }

    #[test]
    fn a_disk_without_a_valid_header_is_not_gpt() {
        let mut blank = RamDisk::new(8);
        assert_eq!(find_partition(&mut blank, 1), Err(GptError::NoGpt));

        // Flip a covered byte: the CRC no longer matches.
        let mut d = gpt_disk(&[(ESP_TYPE, guid(1), 34, 99)], 128);
        d.bytes_mut()[SECTOR + 72] ^= 1;
        assert_eq!(find_partition(&mut d, 1), Err(GptError::NoGpt));

        // A bad entry size is refused even with a matching CRC.
        let mut e = gpt_disk(&[(ESP_TYPE, guid(1), 34, 99)], 128);
        {
            let h = &mut e.bytes_mut()[SECTOR..SECTOR + 92];
            h[84..88].copy_from_slice(&64u32.to_le_bytes());
            h[16..20].fill(0);
            let crc = crc32(h);
            h[16..20].copy_from_slice(&crc.to_le_bytes());
        }
        assert_eq!(find_partition(&mut e, 1), Err(GptError::NoGpt));
    }

    #[test]
    fn a_backwards_range_is_not_a_partition() {
        let mut d = gpt_disk(&[(ESP_TYPE, guid(1), 500, 100)], 128);
        assert_eq!(find_partition(&mut d, 1), Err(GptError::NoSuchPartition));
    }

    #[test]
    fn a_read_failure_is_reported() {
        let mut tiny = RamDisk::new(1);
        assert_eq!(find_partition(&mut tiny, 1), Err(GptError::Io));
    }
}

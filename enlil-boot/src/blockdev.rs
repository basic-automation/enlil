//! Block storage for guests: the backing-store trait and the access policy that
//! decides which sectors a guest may touch.
//!
//! A guest's virtual disk is *carved out of* a host disk, never the whole thing
//! by default. [`DiskMap`] wraps any [`BlockBackend`] with an allow-list of
//! sector ranges, each read-only or read-write; everything outside the list is
//! **denied**: it reads as zeros and writes fail without touching the disk.
//! That is what lets two guests share one physical `NVMe` — the guest sees the
//! host disk's own LBA space (so a real partition table and bootloaders keep
//! working) but can reach only the partitions it was given.
//!
//! [`DiskConfig`] is the small text format that names the disk and the ranges
//! (`nvme-serial = ...`, `allow = 32-63 rw`). It refuses a configuration with no
//! allowed range, so an assignment is always explicit.
//!
//! Everything here is pure; the hardware backend (the host `NVMe` driver) lives in
//! `linux_guest`.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The sector size every backend speaks, in bytes.
pub const SECTOR: usize = 512;

/// Why a block operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockError {
    /// The request runs past the end of the disk.
    OutOfRange,
    /// A write to a sector the guest has no write access to.
    Denied,
    /// The buffer is not a whole number of sectors.
    Unaligned,
    /// The backing store failed.
    Io,
}

/// A disk the hypervisor can read and write in 512-byte sectors.
pub trait BlockBackend {
    /// Total sectors on the disk.
    fn sectors(&self) -> u64;

    /// Fill `buf` (a whole number of sectors) from `lba` onward.
    ///
    /// # Errors
    ///
    /// See [`BlockError`].
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;

    /// Write `data` (a whole number of sectors) at `lba` onward.
    ///
    /// # Errors
    ///
    /// See [`BlockError`].
    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError>;

    /// Make completed writes durable.
    ///
    /// # Errors
    ///
    /// See [`BlockError`].
    fn flush(&mut self) -> Result<(), BlockError>;
}

/// Check that `len` bytes at `lba` are whole sectors inside a disk of
/// `sectors`, returning the sector count.
const fn check_span(sectors: u64, lba: u64, len: usize) -> Result<u64, BlockError> {
    if !len.is_multiple_of(SECTOR) {
        return Err(BlockError::Unaligned);
    }
    let n = (len / SECTOR) as u64;
    match lba.checked_add(n) {
        Some(end) if end <= sectors => Ok(n),
        _ => Err(BlockError::OutOfRange),
    }
}

/// A disk held in memory — the test double, and a scratch disk for a guest
/// that needs one with no host storage behind it.
#[derive(Debug, Clone)]
pub struct RamDisk {
    data: Vec<u8>,
}

impl RamDisk {
    /// A zeroed disk of `sectors` sectors.
    #[must_use]
    pub fn new(sectors: usize) -> Self {
        Self {
            data: alloc::vec![0; sectors * SECTOR],
        }
    }

    /// The raw bytes of the disk (for assertions).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Mutable access to the raw bytes (to seed contents).
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl BlockBackend for RamDisk {
    fn sectors(&self) -> u64 {
        (self.data.len() / SECTOR) as u64
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        check_span(self.sectors(), lba, buf.len())?;
        let start = usize::try_from(lba).map_err(|_| BlockError::OutOfRange)? * SECTOR;
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        check_span(self.sectors(), lba, data.len())?;
        let start = usize::try_from(lba).map_err(|_| BlockError::OutOfRange)? * SECTOR;
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
}

/// One allowed sector range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allow {
    /// First sector (inclusive).
    pub start: u64,
    /// One past the last sector.
    pub end: u64,
    /// Whether writes are permitted.
    pub writable: bool,
}

/// A backend restricted to an allow-list of sector ranges.
///
/// The disk keeps the host's own LBA numbering. Reads outside every range
/// return zeros; a write that touches any sector without write access fails
/// with [`BlockError::Denied`] *before* any sector is written, so a rejected
/// request has no effect.
pub struct DiskMap<B> {
    inner: B,
    ranges: Vec<Allow>,
}

impl<B: BlockBackend> DiskMap<B> {
    /// Restrict `inner` to `ranges`. Overlaps are allowed: a sector is writable
    /// if *any* range covering it is.
    #[must_use]
    pub fn new(inner: B, mut ranges: Vec<Allow>) -> Self {
        ranges.retain(|r| r.start < r.end);
        ranges.sort_by_key(|r| r.start);
        Self { inner, ranges }
    }

    /// The allowed ranges.
    #[must_use]
    pub fn ranges(&self) -> &[Allow] {
        &self.ranges
    }

    /// Whether the guest may read sector `lba`.
    fn readable(&self, lba: u64) -> bool {
        self.ranges.iter().any(|r| lba >= r.start && lba < r.end)
    }

    /// Whether the guest may write sector `lba`.
    fn writable(&self, lba: u64) -> bool {
        self.ranges
            .iter()
            .any(|r| r.writable && lba >= r.start && lba < r.end)
    }

    /// The end of the run of sectors starting at `lba` that share its
    /// readability, capped at `limit`.
    fn run_end(&self, lba: u64, limit: u64) -> u64 {
        let readable = self.readable(lba);
        let mut end = lba + 1;
        while end < limit && self.readable(end) == readable {
            end += 1;
        }
        end
    }
}

impl<B: BlockBackend> BlockBackend for DiskMap<B> {
    fn sectors(&self) -> u64 {
        self.inner.sectors()
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let n = check_span(self.inner.sectors(), lba, buf.len())?;
        let end = lba + n;
        let mut cur = lba;
        while cur < end {
            let run_end = self.run_end(cur, end);
            let lo = usize::try_from(cur - lba).map_err(|_| BlockError::OutOfRange)? * SECTOR;
            let hi = usize::try_from(run_end - lba).map_err(|_| BlockError::OutOfRange)? * SECTOR;
            if self.readable(cur) {
                self.inner.read(cur, &mut buf[lo..hi])?;
            } else {
                buf[lo..hi].fill(0);
            }
            cur = run_end;
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        let n = check_span(self.inner.sectors(), lba, data.len())?;
        // All-or-nothing: refuse the whole request if any sector is not writable.
        if (lba..lba + n).any(|s| !self.writable(s)) {
            return Err(BlockError::Denied);
        }
        self.inner.write(lba, data)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        self.inner.flush()
    }
}

/// Why a [`DiskConfig`] could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A line that is not `key = value`, or an unknown key (carries the line).
    BadLine(String),
    /// A sector range that does not parse or is empty (carries the value).
    BadRange(String),
    /// No `nvme-serial` was given: the backing disk must be named explicitly.
    NoSerial,
    /// No `allow` range was given: nothing would be accessible.
    NoRanges,
}

/// Which host disk a guest's virtual disk is carved from, and how much of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskConfig {
    /// The serial number of the host `NVMe` disk (as Identify reports it).
    pub serial: String,
    /// The sector ranges the guest may access.
    pub ranges: Vec<Allow>,
    /// The controller the guest sees the disk through.
    pub model: DiskModel,
}

/// The kind of controller a guest's virtual disk appears on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiskModel {
    /// A virtio-blk device: small and fast, for guests with a virtio driver
    /// (Linux).
    #[default]
    Virtio,
    /// An `NVMe` controller: what an unmodified Windows install already boots
    /// from, with its inbox driver.
    Nvme,
}

/// Parse `0x..` or decimal.
fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .map_or_else(|| s.parse().ok(), |hex| u64::from_str_radix(hex, 16).ok())
}

impl DiskConfig {
    /// Parse the assignment text:
    ///
    /// ```text
    /// # comments start with '#'
    /// nvme-serial = S6XNNF0R123456
    /// allow = 0-2047 ro        # sectors 0..=2047, read-only (GPT + ESP)
    /// allow = 4096-0xFFFFF rw  # a partition, read-write
    /// allow = all ro           # the whole disk, read-only
    /// ```
    ///
    /// Range ends are inclusive. The disk size is not known here, so `all` is
    /// recorded as `0..u64::MAX` and clamped by the backend.
    ///
    /// # Errors
    ///
    /// See [`ConfigError`]. A config without a serial or without any `allow`
    /// range is rejected: an assignment must be explicit.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut serial = None;
        let mut model = DiskModel::default();
        let mut ranges = Vec::new();
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ConfigError::BadLine(String::from(line)));
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "nvme-serial" => serial = Some(String::from(value)),
                "allow" => ranges.push(Self::parse_allow(value)?),
                "model" => {
                    model = match value {
                        "virtio" => DiskModel::Virtio,
                        "nvme" => DiskModel::Nvme,
                        _ => return Err(ConfigError::BadLine(String::from(line))),
                    };
                }
                _ => return Err(ConfigError::BadLine(String::from(line))),
            }
        }
        let serial = serial
            .filter(|s| !s.is_empty())
            .ok_or(ConfigError::NoSerial)?;
        if ranges.is_empty() {
            return Err(ConfigError::NoRanges);
        }
        Ok(Self {
            serial,
            ranges,
            model,
        })
    }

    /// Parse one `allow` value: `<start>-<end> <ro|rw>` or `all <ro|rw>`.
    fn parse_allow(value: &str) -> Result<Allow, ConfigError> {
        let bad = || ConfigError::BadRange(String::from(value));
        let mut parts = value.split_whitespace();
        let span = parts.next().ok_or_else(bad)?;
        let writable = match parts.next() {
            Some("rw") => true,
            Some("ro") => false,
            _ => return Err(bad()),
        };
        if parts.next().is_some() {
            return Err(bad());
        }
        if span == "all" {
            return Ok(Allow {
                start: 0,
                end: u64::MAX,
                writable,
            });
        }
        let (a, b) = span.split_once('-').ok_or_else(bad)?;
        let (start, last) = (parse_u64(a).ok_or_else(bad)?, parse_u64(b).ok_or_else(bad)?);
        if last < start || last == u64::MAX {
            return Err(bad());
        }
        Ok(Allow {
            start,
            end: last + 1,
            writable,
        })
    }

    /// The ranges clamped to a disk of `sectors` sectors (so an `all` range
    /// becomes the actual disk).
    #[must_use]
    pub fn clamped(&self, sectors: u64) -> Vec<Allow> {
        self.ranges
            .iter()
            .map(|r| Allow {
                end: r.end.min(sectors),
                ..*r
            })
            .filter(|r| r.start < r.end)
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // test fixtures build small, bounded values
mod tests {
    use super::*;

    /// A 16-sector RAM disk where sector `n` is filled with the byte `n + 1`.
    fn seeded() -> RamDisk {
        let mut d = RamDisk::new(16);
        for s in 0..16 {
            d.bytes_mut()[s * SECTOR..(s + 1) * SECTOR].fill(s as u8 + 1);
        }
        d
    }

    fn allow(start: u64, end: u64, writable: bool) -> Allow {
        Allow {
            start,
            end,
            writable,
        }
    }

    #[test]
    fn ram_disk_reads_and_writes_whole_sectors() {
        let mut d = RamDisk::new(4);
        d.write(1, &[7u8; 2 * SECTOR]).unwrap();
        let mut b = [0u8; SECTOR];
        d.read(2, &mut b).unwrap();
        assert!(b.iter().all(|&x| x == 7));
        d.read(0, &mut b).unwrap();
        assert!(b.iter().all(|&x| x == 0));
        assert_eq!(d.sectors(), 4);
    }

    #[test]
    fn ram_disk_rejects_bad_spans() {
        let mut d = RamDisk::new(4);
        let mut b = [0u8; SECTOR];
        assert_eq!(d.read(4, &mut b), Err(BlockError::OutOfRange));
        assert_eq!(d.read(u64::MAX, &mut b), Err(BlockError::OutOfRange));
        assert_eq!(d.write(3, &[0u8; 2 * SECTOR]), Err(BlockError::OutOfRange));
        assert_eq!(d.read(0, &mut [0u8; 100]), Err(BlockError::Unaligned));
    }

    #[test]
    fn disk_map_reads_allowed_sectors_and_zeroes_the_rest() {
        let mut m = DiskMap::new(seeded(), alloc::vec![allow(2, 4, false)]);
        let mut b = [0xFFu8; 6 * SECTOR];
        m.read(0, &mut b).unwrap();
        // sectors 0,1 denied -> zero; 2,3 allowed -> 3,4; 4,5 denied -> zero.
        assert!(b[..2 * SECTOR].iter().all(|&x| x == 0));
        assert!(b[2 * SECTOR..3 * SECTOR].iter().all(|&x| x == 3));
        assert!(b[3 * SECTOR..4 * SECTOR].iter().all(|&x| x == 4));
        assert!(b[4 * SECTOR..].iter().all(|&x| x == 0));
    }

    #[test]
    fn disk_map_read_spanning_several_ranges() {
        let mut m = DiskMap::new(
            seeded(),
            alloc::vec![allow(0, 2, false), allow(3, 5, true), allow(8, 9, false)],
        );
        let mut b = [0xFFu8; 10 * SECTOR];
        m.read(0, &mut b).unwrap();
        let sector = |i: usize| b[i * SECTOR];
        assert_eq!(
            (0..10).map(sector).collect::<Vec<_>>(),
            [1, 2, 0, 4, 5, 0, 0, 0, 9, 0]
        );
    }

    #[test]
    fn disk_map_write_needs_write_access_for_every_sector() {
        let mut m = DiskMap::new(seeded(), alloc::vec![allow(0, 2, false), allow(2, 4, true)]);
        // Fully inside the rw range: ok.
        m.write(2, &[0xAAu8; 2 * SECTOR]).unwrap();
        // Read-only range: denied.
        assert_eq!(m.write(0, &[0xBBu8; SECTOR]), Err(BlockError::Denied));
        // Straddling rw and ro: denied, and NOTHING is written (all-or-nothing).
        assert_eq!(m.write(1, &[0xCCu8; 2 * SECTOR]), Err(BlockError::Denied));
        // Outside every range: denied.
        assert_eq!(m.write(8, &[0xDDu8; SECTOR]), Err(BlockError::Denied));
        let mut raw = [0u8; SECTOR];
        let inner = &mut m.inner;
        inner.read(1, &mut raw).unwrap();
        assert!(
            raw.iter().all(|&x| x == 2),
            "sector 1 untouched by the denied write"
        );
        inner.read(2, &mut raw).unwrap();
        assert!(raw.iter().all(|&x| x == 0xAA), "the allowed write landed");
    }

    #[test]
    fn disk_map_bounds_errors_come_before_policy() {
        let mut m = DiskMap::new(seeded(), alloc::vec![allow(0, 16, true)]);
        let mut b = [0u8; SECTOR];
        assert_eq!(m.read(16, &mut b), Err(BlockError::OutOfRange));
        assert_eq!(m.write(16, &b), Err(BlockError::OutOfRange));
    }

    #[test]
    fn disk_map_with_no_ranges_denies_everything() {
        let mut m = DiskMap::new(seeded(), Vec::new());
        let mut b = [0xFFu8; SECTOR];
        m.read(0, &mut b).unwrap();
        assert!(b.iter().all(|&x| x == 0));
        assert_eq!(m.write(0, &b), Err(BlockError::Denied));
    }

    #[test]
    fn disk_map_ignores_empty_ranges_and_overlap_widens_access() {
        let m = DiskMap::new(
            seeded(),
            alloc::vec![allow(4, 4, true), allow(0, 4, false), allow(2, 6, true)],
        );
        assert_eq!(m.ranges().len(), 2, "the empty range is dropped");
        assert!(m.writable(3), "covered by the rw range too");
        assert!(!m.writable(1));
        assert!(m.readable(5) && !m.readable(6));
    }

    #[test]
    fn config_parses_serial_and_ranges() {
        let c = DiskConfig::parse(
            "# a guest disk\nnvme-serial = S6XN123\nallow = 0-31 ro  # GPT + ESP\nallow = 0x40-0x7F rw\n",
        )
        .unwrap();
        assert_eq!(c.serial, "S6XN123");
        assert_eq!(c.ranges, [allow(0, 32, false), allow(0x40, 0x80, true)]);
    }

    #[test]
    fn config_chooses_the_controller_model() {
        let base = "nvme-serial = X\nallow = 0-1 ro\n";
        assert_eq!(DiskConfig::parse(base).unwrap().model, DiskModel::Virtio);
        let nvme = DiskConfig::parse(&format!("{base}model = nvme\n")).unwrap();
        assert_eq!(nvme.model, DiskModel::Nvme);
        let virtio = DiskConfig::parse(&format!("model = virtio # explicit\n{base}")).unwrap();
        assert_eq!(virtio.model, DiskModel::Virtio);
        assert!(matches!(
            DiskConfig::parse(&format!("{base}model = ide\n")),
            Err(ConfigError::BadLine(_))
        ));
    }

    #[test]
    fn config_all_is_clamped_to_the_disk() {
        let c = DiskConfig::parse("nvme-serial=X\nallow = all ro\n").unwrap();
        assert_eq!(c.ranges, [allow(0, u64::MAX, false)]);
        assert_eq!(c.clamped(100), [allow(0, 100, false)]);
        let c2 = DiskConfig::parse("nvme-serial=X\nallow = 50-200 rw\n").unwrap();
        assert_eq!(c2.clamped(100), [allow(50, 100, true)]);
        assert!(c2.clamped(10).is_empty(), "range entirely past the disk");
    }

    #[test]
    fn config_requires_an_explicit_assignment() {
        assert_eq!(
            DiskConfig::parse("allow = 0-1 ro"),
            Err(ConfigError::NoSerial)
        );
        assert_eq!(
            DiskConfig::parse("nvme-serial = "),
            Err(ConfigError::NoSerial)
        );
        assert_eq!(
            DiskConfig::parse("nvme-serial = X"),
            Err(ConfigError::NoRanges)
        );
        assert_eq!(DiskConfig::parse(""), Err(ConfigError::NoSerial));
    }

    #[test]
    fn config_rejects_malformed_input() {
        for bad in [
            "nvme-serial = X\nallow = 5-2 rw",
            "nvme-serial = X\nallow = 1-2",
            "nvme-serial = X\nallow = 1-2 wx",
            "nvme-serial = X\nallow = 1-2 rw extra",
            "nvme-serial = X\nallow = a-b rw",
            "nvme-serial = X\nallow = 1 rw",
            "nvme-serial = X\nallow = 0-18446744073709551615 rw",
        ] {
            assert!(
                matches!(DiskConfig::parse(bad), Err(ConfigError::BadRange(_))),
                "{bad}"
            );
        }
        assert!(matches!(
            DiskConfig::parse("nvme-serial X"),
            Err(ConfigError::BadLine(_))
        ));
        assert!(matches!(
            DiskConfig::parse("nvme-serial = X\nallow = 0-1 ro\nbogus = 1"),
            Err(ConfigError::BadLine(_))
        ));
    }
}

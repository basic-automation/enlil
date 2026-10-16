//! The firmware-configuration (`fw_cfg`) interface OVMF reads machine data from.
//!
//! OVMF has no ACPI tables, SMBIOS tables or CPU count of its own: it asks the
//! machine, over two I/O ports. Write an item number to the selector port
//! (`0x510`, 16 bits), then read that item's bytes one at a time from the data
//! port (`0x511`). Items are either fixed numbers (the signature, the machine
//! UUID, the CPU count) or *files* named in a directory (`etc/acpi/tables`,
//! `etc/smbios/smbios-tables`, ...). Without this OVMF installs no ACPI tables,
//! and an OS that needs ACPI - Windows - cannot start.
//!
//! This is the traditional port interface only; the DMA interface is not
//! offered (the revision register says so), and OVMF falls back to reading
//! bytes, which it does with string I/O the run loop already emulates.
//!
//! Pure and host-tested.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The selector port (16-bit writes).
pub const PORT_SELECTOR: u16 = 0x510;
/// The data port (byte reads).
pub const PORT_DATA: u16 = 0x511;

/// Fixed items.
const SEL_SIGNATURE: u16 = 0x00;
const SEL_ID: u16 = 0x01;
const SEL_UUID: u16 = 0x02;
const SEL_RAM_SIZE: u16 = 0x03;
const SEL_NB_CPUS: u16 = 0x05;
const SEL_MAX_CPUS: u16 = 0x0F;
/// The file directory.
const SEL_FILE_DIR: u16 = 0x19;
/// The first file's item number.
const SEL_FILE_FIRST: u16 = 0x20;
/// Bytes of a file name in the directory (NUL-padded).
const NAME_BYTES: usize = 56;
/// `FW_CFG_ID` bit 0: the traditional interface exists (bit 1, DMA, is not offered).
const ID_TRADITIONAL: u32 = 1;

/// The machine facts the fixed items report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineInfo {
    /// The machine UUID (also in SMBIOS).
    pub uuid: [u8; 16],
    /// Bytes of RAM.
    pub ram_bytes: u64,
    /// Number of CPUs.
    pub cpus: u16,
}

/// A named blob the firmware can read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct File {
    name: String,
    data: Vec<u8>,
}

/// The `fw_cfg` device.
#[derive(Debug, Clone)]
pub struct FwCfg {
    fixed: Vec<(u16, Vec<u8>)>,
    files: Vec<File>,
    selector: u16,
    offset: usize,
}

impl FwCfg {
    /// A device reporting `machine`, with no files yet.
    #[must_use]
    pub fn new(machine: MachineInfo) -> Self {
        let fixed = alloc::vec![
            (SEL_SIGNATURE, b"QEMU".to_vec()),
            (SEL_ID, ID_TRADITIONAL.to_le_bytes().to_vec()),
            (SEL_UUID, machine.uuid.to_vec()),
            (SEL_RAM_SIZE, machine.ram_bytes.to_le_bytes().to_vec()),
            (SEL_NB_CPUS, machine.cpus.to_le_bytes().to_vec()),
            (SEL_MAX_CPUS, machine.cpus.to_le_bytes().to_vec()),
        ];
        Self {
            fixed,
            files: Vec::new(),
            selector: 0,
            offset: 0,
        }
    }

    /// Publish `data` under `name`; returns the file's item number.
    ///
    /// # Panics
    ///
    /// If `name` does not fit the directory entry (55 bytes and a NUL).
    pub fn add_file(&mut self, name: &str, data: Vec<u8>) -> u16 {
        assert!(name.len() < NAME_BYTES, "fw_cfg file name too long: {name}");
        self.files.push(File {
            name: String::from(name),
            data,
        });
        SEL_FILE_FIRST + u16::try_from(self.files.len() - 1).unwrap_or(0)
    }

    /// The bytes of the file called `name`, if present.
    #[must_use]
    pub fn file(&self, name: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.data.as_slice())
    }

    /// The directory: a big-endian file count, then per file its size and item
    /// number (big-endian) and its name.
    fn directory(&self) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&u32::try_from(self.files.len()).unwrap_or(0).to_be_bytes());
        for (i, f) in self.files.iter().enumerate() {
            d.extend_from_slice(&u32::try_from(f.data.len()).unwrap_or(0).to_be_bytes());
            d.extend_from_slice(&(SEL_FILE_FIRST + u16::try_from(i).unwrap_or(0)).to_be_bytes());
            d.extend_from_slice(&[0, 0]);
            let mut name = [0u8; NAME_BYTES];
            name[..f.name.len()].copy_from_slice(f.name.as_bytes());
            d.extend_from_slice(&name);
        }
        d
    }

    /// The bytes of the currently selected item (empty for an unknown one).
    fn selected(&self) -> Vec<u8> {
        match self.selector {
            SEL_FILE_DIR => self.directory(),
            s if s >= SEL_FILE_FIRST => self
                .files
                .get(usize::from(s - SEL_FILE_FIRST))
                .map_or_else(Vec::new, |f| f.data.clone()),
            s => self
                .fixed
                .iter()
                .find(|(id, _)| *id == s)
                .map_or_else(Vec::new, |(_, d)| d.clone()),
        }
    }

    /// Whether the device owns I/O port `port`.
    #[must_use]
    pub const fn claims(port: u16) -> bool {
        port == PORT_SELECTOR || port == PORT_SELECTOR + 1 || port == PORT_DATA
    }

    /// An `IN` of `size` bytes: the data port returns the next bytes of the
    /// selected item (zeros past its end, as on QEMU); the selector reads back
    /// as the selected item number.
    pub fn io_read(&mut self, port: u16, size: u8) -> u32 {
        if port != PORT_DATA {
            return u32::from(self.selector);
        }
        let item = self.selected();
        let mut v = 0u32;
        for i in 0..usize::from(size.min(4)) {
            let byte = item.get(self.offset).copied().unwrap_or(0);
            self.offset += 1;
            v |= u32::from(byte) << (8 * i);
        }
        v
    }

    /// An `OUT` to the selector selects an item and restarts reading it.
    pub const fn io_write(&mut self, port: u16, size: u8, val: u32) {
        if port == PORT_SELECTOR && size >= 2 {
            self.selector = (val & 0xFFFF) as u16;
            self.offset = 0;
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // byte-wide reads of test fixtures
mod tests {
    use super::*;

    fn machine() -> MachineInfo {
        MachineInfo {
            uuid: *b"0123456789abcdef",
            ram_bytes: 256 << 20,
            cpus: 1,
        }
    }

    /// Select an item and read `n` bytes of it, one at a time.
    fn read_item(d: &mut FwCfg, item: u16, n: usize) -> Vec<u8> {
        d.io_write(PORT_SELECTOR, 2, u32::from(item));
        (0..n).map(|_| d.io_read(PORT_DATA, 1) as u8).collect()
    }

    #[test]
    fn the_signature_and_revision_identify_the_interface() {
        let mut d = FwCfg::new(machine());
        assert_eq!(read_item(&mut d, 0, 4), b"QEMU");
        // Revision: the traditional interface only, no DMA bit.
        assert_eq!(read_item(&mut d, 1, 4), [1, 0, 0, 0]);
    }

    #[test]
    fn the_fixed_items_report_the_machine() {
        let mut d = FwCfg::new(machine());
        assert_eq!(read_item(&mut d, 2, 16), b"0123456789abcdef");
        assert_eq!(read_item(&mut d, 3, 8), (256u64 << 20).to_le_bytes());
        assert_eq!(read_item(&mut d, 5, 2), [1, 0], "CPU count");
        assert_eq!(read_item(&mut d, 0x0F, 2), [1, 0], "maximum CPUs");
    }

    #[test]
    fn files_are_listed_in_a_big_endian_directory_and_readable_by_item() {
        let mut d = FwCfg::new(machine());
        let a = d.add_file("etc/acpi/rsdp", alloc::vec![1, 2, 3]);
        let b = d.add_file("etc/table-loader", alloc::vec![9; 300]);
        assert_eq!((a, b), (0x20, 0x21));

        let dir = read_item(&mut d, SEL_FILE_DIR, 4 + 2 * 64);
        assert_eq!(&dir[0..4], &2u32.to_be_bytes(), "file count");
        // First entry: size, item number, reserved, name.
        assert_eq!(&dir[4..8], &3u32.to_be_bytes());
        assert_eq!(&dir[8..10], &0x20u16.to_be_bytes());
        assert_eq!(&dir[12..12 + 13], b"etc/acpi/rsdp");
        assert_eq!(dir[12 + 13], 0, "names are NUL-padded");
        // Second entry follows at 4 + 64.
        assert_eq!(&dir[68..72], &300u32.to_be_bytes());
        assert_eq!(&dir[72..74], &0x21u16.to_be_bytes());

        assert_eq!(read_item(&mut d, a, 3), [1, 2, 3]);
        assert_eq!(read_item(&mut d, b, 300), [9; 300]);
        assert_eq!(d.file("etc/acpi/rsdp"), Some(&[1u8, 2, 3][..]));
        assert_eq!(d.file("nope"), None);
    }

    #[test]
    fn reading_past_the_end_and_unknown_items_give_zeros() {
        let mut d = FwCfg::new(machine());
        let f = d.add_file("x", alloc::vec![0xAA, 0xBB]);
        assert_eq!(read_item(&mut d, f, 5), [0xAA, 0xBB, 0, 0, 0]);
        assert_eq!(read_item(&mut d, 0x7777, 3), [0, 0, 0]);
        // An empty directory is a zero count.
        let mut e = FwCfg::new(machine());
        assert_eq!(read_item(&mut e, SEL_FILE_DIR, 4), [0, 0, 0, 0]);
    }

    #[test]
    fn selecting_an_item_restarts_it_and_wide_reads_take_consecutive_bytes() {
        let mut d = FwCfg::new(machine());
        let f = d.add_file("x", alloc::vec![1, 2, 3, 4, 5, 6, 7, 8]);
        d.io_write(PORT_SELECTOR, 2, u32::from(f));
        assert_eq!(d.io_read(PORT_DATA, 2), 0x0201);
        assert_eq!(d.io_read(PORT_DATA, 4), 0x0605_0403);
        // Selecting again starts over.
        d.io_write(PORT_SELECTOR, 2, u32::from(f));
        assert_eq!(d.io_read(PORT_DATA, 1), 1);
        // The selector reads back, and a byte-wide write to it is ignored.
        assert_eq!(d.io_read(PORT_SELECTOR, 2), u32::from(f));
        d.io_write(PORT_SELECTOR, 1, 0x99);
        assert_eq!(d.io_read(PORT_SELECTOR, 2), u32::from(f));
    }

    #[test]
    fn only_the_two_ports_are_claimed() {
        assert!(FwCfg::claims(0x510) && FwCfg::claims(0x511));
        assert!(!FwCfg::claims(0x50F) && !FwCfg::claims(0x514));
    }

    #[test]
    #[should_panic(expected = "too long")]
    fn an_over_long_file_name_is_a_programming_error() {
        let mut d = FwCfg::new(machine());
        d.add_file(&"n".repeat(56), Vec::new());
    }
}

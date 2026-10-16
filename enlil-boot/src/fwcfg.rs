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
//! Both interfaces are offered. The traditional one moves a byte at a time over
//! the data port. The DMA one (revision bit 1) has the guest write the address
//! of a small control block to port `0x514`/`0x518`; the device then reads,
//! skips or *writes* the selected item straight to or from guest memory. OVMF
//! uses DMA when it can: it is much faster for the ACPI tables, and it is the
//! only way to write an item, which the `ramfb` display needs.
//!
//! Pure and host-tested.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::virtio_blk::GuestMemory;

/// The selector port (16-bit writes).
pub const PORT_SELECTOR: u16 = 0x510;
/// The data port (byte reads).
pub const PORT_DATA: u16 = 0x511;
/// The DMA address register: two big-endian 32-bit halves, the low one
/// (at `+4`) starting the transfer.
pub const PORT_DMA: u16 = 0x514;
/// What the DMA register reads back: `"QEMU CFG"`.
const DMA_SIGNATURE: u64 = 0x5145_4D55_2043_4647;
/// Largest DMA transfer accepted (a sanity bound, not a hardware limit).
const DMA_MAX_BYTES: usize = 16 << 20;

// Bits of the DMA control word.
const DMA_ERROR: u32 = 1;
const DMA_READ: u32 = 2;
const DMA_SKIP: u32 = 4;
const DMA_SELECT: u32 = 8;
const DMA_WRITE: u32 = 16;

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
/// `FW_CFG_ID` bit 0: the traditional interface exists.
const ID_TRADITIONAL: u32 = 1;
/// `FW_CFG_ID` bit 1: the DMA interface exists.
const ID_DMA: u32 = 2;

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

/// A named blob the firmware can read (and, if `writable`, overwrite in place).
#[derive(Debug, Clone, PartialEq, Eq)]
struct File {
    name: String,
    data: Vec<u8>,
    writable: bool,
}

/// The `fw_cfg` device.
#[derive(Debug, Clone)]
pub struct FwCfg {
    fixed: Vec<(u16, Vec<u8>)>,
    files: Vec<File>,
    selector: u16,
    offset: usize,
    /// The high half of a DMA address, latched until the low half arrives.
    dma_high: u32,
    /// A DMA request the guest has made and the run loop has not served yet.
    dma_pending: Option<u64>,
}

impl FwCfg {
    /// A device reporting `machine`, with no files yet.
    #[must_use]
    pub fn new(machine: MachineInfo) -> Self {
        let fixed = alloc::vec![
            (SEL_SIGNATURE, b"QEMU".to_vec()),
            (SEL_ID, (ID_TRADITIONAL | ID_DMA).to_le_bytes().to_vec()),
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
            dma_high: 0,
            dma_pending: None,
        }
    }

    /// Publish `data` under `name`; returns the file's item number.
    ///
    /// # Panics
    ///
    /// If `name` does not fit the directory entry (55 bytes and a NUL).
    pub fn add_file(&mut self, name: &str, data: Vec<u8>) -> u16 {
        self.push_file(name, data, false)
    }

    /// Publish `data` under `name` as a file the guest may overwrite (by DMA
    /// write, in place and not beyond its size); returns the item number.
    ///
    /// # Panics
    ///
    /// If `name` does not fit the directory entry (55 bytes and a NUL).
    pub fn add_writable_file(&mut self, name: &str, data: Vec<u8>) -> u16 {
        self.push_file(name, data, true)
    }

    fn push_file(&mut self, name: &str, data: Vec<u8>, writable: bool) -> u16 {
        assert!(name.len() < NAME_BYTES, "fw_cfg file name too long: {name}");
        self.files.push(File {
            name: String::from(name),
            data,
            writable,
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
        port == PORT_SELECTOR
            || port == PORT_SELECTOR + 1
            || port == PORT_DATA
            || (port >= PORT_DMA && port < PORT_DMA + 8)
    }

    /// An `IN` of `size` bytes: the data port returns the next bytes of the
    /// selected item (zeros past its end, as on QEMU); the selector reads back
    /// as the selected item number; the DMA register reads as `"QEMU CFG"`.
    pub fn io_read(&mut self, port: u16, size: u8) -> u32 {
        if (PORT_DMA..PORT_DMA + 8).contains(&port) {
            // The signature, as a big-endian 64-bit register: a read returns the
            // `size` bytes at `port`, as one integer.
            let size = u32::from(size.clamp(1, 4));
            let shift = 8u32.saturating_sub(u32::from(port - PORT_DMA) + size) * 8;
            #[allow(clippy::cast_possible_truncation)] // masked to the access size
            return ((DMA_SIGNATURE >> shift) & ((1u64 << (size * 8)) - 1)) as u32;
        }
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

    /// An `OUT` to the selector selects an item and restarts reading it; the two
    /// halves of the DMA register latch a request (see [`Self::take_dma`]).
    pub fn io_write(&mut self, port: u16, size: u8, val: u32) {
        if (PORT_DMA..PORT_DMA + 8).contains(&port) {
            if size == 4 {
                match port - PORT_DMA {
                    0 => self.dma_high = val.swap_bytes(),
                    4 => {
                        self.dma_pending =
                            Some(u64::from(self.dma_high) << 32 | u64::from(val.swap_bytes()));
                    }
                    _ => {}
                }
            }
            return;
        }
        if port == PORT_SELECTOR && size >= 2 {
            self.selector = (val & 0xFFFF) as u16;
            self.offset = 0;
        }
    }

    /// The DMA request the guest just made, if any: the guest-physical address
    /// of its control block, to pass to [`Self::dma`].
    pub const fn take_dma(&mut self) -> Option<u64> {
        self.dma_pending.take()
    }

    /// Serve the DMA request whose control block is at `addr`.
    ///
    /// The block is a big-endian control word, a length and a guest address. The
    /// control word may select an item first, then reads (zeros past the item's
    /// end), skips or writes `length` bytes at the current offset. The word is
    /// replaced by `0` on success or the error bit on failure - a write to a
    /// read-only item, past a writable one's end, or to memory that is not RAM.
    pub fn dma(&mut self, mem: &dyn GuestMemory, addr: u64) {
        let mut block = [0u8; 16];
        if !mem.read(addr, &mut block) {
            return;
        }
        let word = |at: usize| {
            u32::from_be_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]])
        };
        let (control, length) = (word(0), usize::try_from(word(4)).unwrap_or(usize::MAX));
        let target = u64::from_be_bytes(block[8..16].try_into().unwrap_or([0; 8]));
        if control & DMA_SELECT != 0 {
            self.selector = (control >> 16) as u16;
            self.offset = 0;
        }
        let ok = length <= DMA_MAX_BYTES
            && if control & DMA_READ != 0 {
                let item = self.selected();
                let chunk: Vec<u8> = (0..length)
                    .map(|i| item.get(self.offset + i).copied().unwrap_or(0))
                    .collect();
                self.offset += length;
                mem.write(target, &chunk)
            } else if control & DMA_WRITE != 0 {
                let done = self.write_selected(mem, target, length);
                self.offset += length;
                done
            } else {
                // A skip, or a select with nothing to move.
                if control & DMA_SKIP != 0 {
                    self.offset += length;
                }
                true
            };
        let status = if ok { 0 } else { DMA_ERROR };
        let _ = mem.write(addr, &status.to_be_bytes());
    }

    /// Overwrite the selected item with `length` bytes from guest memory at
    /// `from`, if it is a writable file and the span lies inside it.
    fn write_selected(&mut self, mem: &dyn GuestMemory, from: u64, length: usize) -> bool {
        let Some(index) = usize::from(self.selector).checked_sub(usize::from(SEL_FILE_FIRST))
        else {
            return false;
        };
        let offset = self.offset;
        let Some(file) = self.files.get_mut(index).filter(|f| f.writable) else {
            return false;
        };
        let Some(span) = offset
            .checked_add(length)
            .and_then(|end| file.data.get_mut(offset..end))
        else {
            return false;
        };
        mem.read(from, span)
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
        // Revision: the traditional interface and the DMA one.
        assert_eq!(read_item(&mut d, 1, 4), [3, 0, 0, 0]);
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
    fn only_the_selector_data_and_dma_ports_are_claimed() {
        assert!(FwCfg::claims(0x510) && FwCfg::claims(0x511));
        assert!(FwCfg::claims(0x514) && FwCfg::claims(0x51B));
        assert!(!FwCfg::claims(0x50F) && !FwCfg::claims(0x512) && !FwCfg::claims(0x51C));
    }

    /// Guest RAM for DMA tests: `bytes` from guest address 0.
    struct Ram(core::cell::RefCell<Vec<u8>>);

    impl Ram {
        fn new(bytes: Vec<u8>) -> Self {
            Self(core::cell::RefCell::new(bytes))
        }

        fn bytes(&self, at: usize, len: usize) -> Vec<u8> {
            self.0.borrow()[at..at + len].to_vec()
        }
    }

    impl GuestMemory for Ram {
        fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
            let Ok(at) = usize::try_from(gpa) else {
                return false;
            };
            self.0
                .borrow()
                .get(at..at + buf.len())
                .map(|s| buf.copy_from_slice(s))
                .is_some()
        }

        fn write(&self, gpa: u64, data: &[u8]) -> bool {
            let Ok(at) = usize::try_from(gpa) else {
                return false;
            };
            self.0
                .borrow_mut()
                .get_mut(at..at + data.len())
                .map(|s| s.copy_from_slice(data))
                .is_some()
        }
    }

    /// Write a DMA control block at `at` and ring the register for it.
    fn request(d: &mut FwCfg, ram: &Ram, at: u64, control: u32, length: u32, target: u64) {
        let mut block = Vec::new();
        block.extend_from_slice(&control.to_be_bytes());
        block.extend_from_slice(&length.to_be_bytes());
        block.extend_from_slice(&target.to_be_bytes());
        assert!(ram.write(at, &block));
        d.io_write(PORT_DMA, 4, ((at >> 32) as u32).swap_bytes());
        assert_eq!(d.take_dma(), None, "the low half starts the transfer");
        d.io_write(PORT_DMA + 4, 4, (at as u32).swap_bytes());
        let addr = d.take_dma().expect("a request is pending");
        assert_eq!(addr, at);
        assert_eq!(d.take_dma(), None, "taken once");
        d.dma(ram, addr);
    }

    fn status(ram: &Ram, at: u64) -> u32 {
        let mut s = [0u8; 4];
        assert!(ram.read(at, &mut s));
        u32::from_be_bytes(s)
    }

    #[test]
    fn the_dma_register_reads_back_its_signature() {
        let mut d = FwCfg::new(machine());
        assert_eq!(
            d.io_read(PORT_DMA, 4),
            0x5145_4D55,
            "high half, as an integer"
        );
        assert_eq!(d.io_read(PORT_DMA + 4, 4), 0x2043_4647);
        assert_eq!(d.io_read(PORT_DMA, 1), 0x51);
        assert_eq!(d.io_read(PORT_DMA + 7, 1), 0x47);
    }

    #[test]
    fn a_dma_read_selects_and_fills_guest_memory() {
        let mut d = FwCfg::new(machine());
        let f = d.add_file("x", alloc::vec![10, 20, 30]);
        let ram = Ram::new(alloc::vec![0xEE; 256]);
        // SELECT | READ of five bytes: the file, then zeros past its end.
        request(
            &mut d,
            &ram,
            0,
            (u32::from(f) << 16) | DMA_SELECT | DMA_READ,
            5,
            64,
        );
        assert_eq!(ram.bytes(64, 5), [10, 20, 30, 0, 0]);
        assert_eq!(status(&ram, 0), 0, "success clears the control word");
        assert_eq!(
            ram.bytes(69, 1),
            [0xEE],
            "nothing past the length is touched"
        );
    }

    #[test]
    fn dma_continues_from_the_current_offset_and_can_skip() {
        let mut d = FwCfg::new(machine());
        let f = d.add_file("x", alloc::vec![1, 2, 3, 4, 5, 6]);
        let ram = Ram::new(alloc::vec![0; 256]);
        d.io_write(PORT_SELECTOR, 2, u32::from(f));
        request(&mut d, &ram, 0, DMA_SKIP, 2, 0);
        request(&mut d, &ram, 0, DMA_READ, 3, 100);
        assert_eq!(ram.bytes(100, 3), [3, 4, 5]);
        // The byte interface continues where DMA left off.
        assert_eq!(d.io_read(PORT_DATA, 1), 6);
    }

    #[test]
    fn a_dma_write_changes_a_writable_file_in_place() {
        let mut d = FwCfg::new(machine());
        let f = d.add_writable_file("etc/ramfb", alloc::vec![0; 8]);
        let ram = Ram::new(alloc::vec![0; 256]);
        assert!(ram.write(200, &[9, 8, 7, 6]));
        d.io_write(PORT_SELECTOR, 2, u32::from(f));
        request(&mut d, &ram, 0, DMA_WRITE, 4, 200);
        assert_eq!(status(&ram, 0), 0);
        assert_eq!(d.file("etc/ramfb"), Some(&[9u8, 8, 7, 6, 0, 0, 0, 0][..]));
        // A write that would run past the end is refused and changes nothing.
        d.io_write(PORT_SELECTOR, 2, u32::from(f));
        request(&mut d, &ram, 0, DMA_WRITE, 9, 200);
        assert_eq!(status(&ram, 0), DMA_ERROR);
        assert_eq!(d.file("etc/ramfb"), Some(&[9u8, 8, 7, 6, 0, 0, 0, 0][..]));
    }

    #[test]
    fn a_dma_write_to_a_read_only_or_fixed_item_is_an_error() {
        let mut d = FwCfg::new(machine());
        let f = d.add_file("ro", alloc::vec![1, 2]);
        let ram = Ram::new(alloc::vec![0; 64]);
        d.io_write(PORT_SELECTOR, 2, u32::from(f));
        request(&mut d, &ram, 0, DMA_WRITE, 2, 32);
        assert_eq!(status(&ram, 0), DMA_ERROR);
        assert_eq!(d.file("ro"), Some(&[1u8, 2][..]));
        d.io_write(PORT_SELECTOR, 2, u32::from(SEL_UUID));
        request(&mut d, &ram, 0, DMA_WRITE, 2, 32);
        assert_eq!(status(&ram, 0), DMA_ERROR);
    }

    #[test]
    fn a_dma_to_memory_that_is_not_ram_is_an_error_and_oversize_is_refused() {
        let mut d = FwCfg::new(machine());
        let ram = Ram::new(alloc::vec![0; 64]);
        request(&mut d, &ram, 0, DMA_SELECT | DMA_READ, 4, 1 << 40);
        assert_eq!(status(&ram, 0), DMA_ERROR);
        request(&mut d, &ram, 0, DMA_SELECT | DMA_READ, u32::MAX, 32);
        assert_eq!(status(&ram, 0), DMA_ERROR);
        // A control block outside RAM is ignored without a panic.
        d.dma(&ram, 1 << 40);
    }

    #[test]
    fn dma_reads_the_file_directory_like_the_byte_interface() {
        let mut d = FwCfg::new(machine());
        d.add_file("etc/acpi/rsdp", alloc::vec![1, 2, 3]);
        let ram = Ram::new(alloc::vec![0; 256]);
        let by_port = read_item(&mut d, SEL_FILE_DIR, 4 + 64);
        request(
            &mut d,
            &ram,
            0,
            (u32::from(SEL_FILE_DIR) << 16) | DMA_SELECT | DMA_READ,
            68,
            64,
        );
        assert_eq!(ram.bytes(64, 68), by_port);
    }

    #[test]
    #[should_panic(expected = "too long")]
    fn an_over_long_file_name_is_a_programming_error() {
        let mut d = FwCfg::new(machine());
        d.add_file(&"n".repeat(56), Vec::new());
    }
}

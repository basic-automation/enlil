//! A virtual PCI function's configuration space, and the legacy
//! configuration-access mechanism guests use to reach it.
//!
//! [`PciFunction`] models one type-0 function: a 256-byte config space where each
//! byte has a write mask, so read-only fields stay put, the command register
//! only accepts the bits a device implements, and a 64-bit memory BAR behaves
//! the way sizing software expects (write all-ones, read back the size mask).
//! A capability chain can be appended. [`pci_target`] decodes a `0xCF8`
//! address into bus / device / function / register.
//!
//! Pure and host-tested: an OS enumerates this exactly as it would hardware, so
//! the semantics matter more than the size of the code.

/// Size of a function's configuration space in bytes.
pub const CONFIG_BYTES: usize = 256;

const OFF_VENDOR: usize = 0x00;
const OFF_DEVICE: usize = 0x02;
const OFF_COMMAND: usize = 0x04;
const OFF_STATUS: usize = 0x06;
const OFF_REVISION: usize = 0x08;
const OFF_CLASS: usize = 0x09;
const OFF_CACHE_LINE: usize = 0x0C;
const OFF_LATENCY: usize = 0x0D;
const OFF_BAR0: usize = 0x10;
const OFF_BAR1: usize = 0x14;
const OFF_SUBSYS_VENDOR: usize = 0x2C;
const OFF_SUBSYS_ID: usize = 0x2E;
const OFF_CAP_PTR: usize = 0x34;
const OFF_INT_LINE: usize = 0x3C;
const OFF_INT_PIN: usize = 0x3D;

/// `COMMAND`: I/O space enable.
pub const CMD_IO: u16 = 1 << 0;
/// `COMMAND`: memory space enable.
pub const CMD_MEM: u16 = 1 << 1;
/// `COMMAND`: bus master enable.
pub const CMD_BUS_MASTER: u16 = 1 << 2;
/// `COMMAND`: `INTx` disable.
pub const CMD_INTX_DISABLE: u16 = 1 << 10;
/// `STATUS`: a capability list is present.
const STATUS_CAP_LIST: u16 = 1 << 4;
/// First configuration offset capabilities are placed at.
const CAP_START: usize = 0x40;

/// One type-0 PCI function's configuration space.
#[derive(Debug, Clone)]
pub struct PciFunction {
    cfg: [u8; CONFIG_BYTES],
    wmask: [u8; CONFIG_BYTES],
    bar0_size: u64,
    cap_tail: Option<usize>,
    cap_next_free: usize,
}

impl PciFunction {
    /// A function with the given identity. `class` is `[class, subclass,
    /// prog_if]`.
    #[must_use]
    pub fn new(
        vendor: u16,
        device: u16,
        class: [u8; 3],
        revision: u8,
        subsystem: (u16, u16),
    ) -> Self {
        let mut f = Self {
            cfg: [0; CONFIG_BYTES],
            wmask: [0; CONFIG_BYTES],
            bar0_size: 0,
            cap_tail: None,
            cap_next_free: CAP_START,
        };
        f.put16(OFF_VENDOR, vendor);
        f.put16(OFF_DEVICE, device);
        f.cfg[OFF_REVISION] = revision;
        f.cfg[OFF_CLASS] = class[2];
        f.cfg[OFF_CLASS + 1] = class[1];
        f.cfg[OFF_CLASS + 2] = class[0];
        f.put16(OFF_SUBSYS_VENDOR, subsystem.0);
        f.put16(OFF_SUBSYS_ID, subsystem.1);
        // The command register implements I/O, memory, bus-master, INTx-disable.
        let cmd_mask = CMD_IO | CMD_MEM | CMD_BUS_MASTER | CMD_INTX_DISABLE;
        f.wmask[OFF_COMMAND] = cmd_mask.to_le_bytes()[0];
        f.wmask[OFF_COMMAND + 1] = cmd_mask.to_le_bytes()[1];
        f.wmask[OFF_CACHE_LINE] = 0xFF;
        f.wmask[OFF_LATENCY] = 0xFF;
        f
    }

    fn put16(&mut self, off: usize, v: u16) {
        self.cfg[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    const fn get16(&self, off: usize) -> u16 {
        u16::from_le_bytes([self.cfg[off], self.cfg[off + 1]])
    }

    /// Give the function a 64-bit, non-prefetchable memory BAR0 of `size` bytes
    /// (a power of two, at least 16).
    ///
    /// # Panics
    ///
    /// If `size` is not a power of two of at least 16 bytes.
    #[allow(clippy::cast_possible_truncation)] // the low BAR dword is the low 32 bits of the mask
    pub fn set_bar0_mem64(&mut self, size: u64) {
        assert!(
            size.is_power_of_two() && size >= 16,
            "BAR size must be a power of two >= 16"
        );
        self.bar0_size = size;
        // Memory BAR, 64-bit (type 10b), non-prefetchable.
        self.cfg[OFF_BAR0] = 0x04;
        // Address bits above the size are writable; the low bits are not.
        let lo_mask = !(size - 1) as u32;
        self.wmask[OFF_BAR0..OFF_BAR0 + 4].copy_from_slice(&lo_mask.to_le_bytes());
        self.wmask[OFF_BAR1..OFF_BAR1 + 4].fill(0xFF);
    }

    /// Make `len` bytes of configuration space from `off` writable (a chipset
    /// register the guest programs, such as a power-management base address).
    pub fn set_writable(&mut self, off: usize, len: usize) {
        if let Some(mask) = self.wmask.get_mut(off..off.saturating_add(len)) {
            mask.fill(0xFF);
        }
    }

    /// Set a register's read-only reset value (for example the header type or a
    /// fixed indicator bit) without making it writable.
    pub fn preset(&mut self, off: usize, bytes: &[u8]) {
        if let Some(cfg) = self.cfg.get_mut(off..off.saturating_add(bytes.len())) {
            cfg.copy_from_slice(bytes);
        }
    }

    /// Set the interrupt line (the legacy PIC input the OS should use) and pin
    /// (`1` = INTA#). The line is writable, as on real hardware.
    pub const fn set_interrupt(&mut self, line: u8, pin: u8) {
        self.cfg[OFF_INT_LINE] = line;
        self.cfg[OFF_INT_PIN] = pin;
        self.wmask[OFF_INT_LINE] = 0xFF;
    }

    /// Append a capability. `body` is the whole structure *including* the
    /// capability ID byte (offset 0); its `next` byte (offset 1) is filled in.
    /// Returns the configuration offset it was placed at.
    ///
    /// # Panics
    ///
    /// If the capability would not fit in the standard 256-byte space.
    pub fn add_capability(&mut self, body: &[u8]) -> usize {
        let at = self.cap_next_free;
        assert!(
            at + body.len() <= CONFIG_BYTES && body.len() >= 2,
            "capability does not fit"
        );
        self.cfg[at..at + body.len()].copy_from_slice(body);
        self.cfg[at + 1] = 0;
        if let Some(prev) = self.cap_tail {
            self.cfg[prev + 1] = u8::try_from(at).unwrap_or(0);
        } else {
            self.cfg[OFF_CAP_PTR] = u8::try_from(at).unwrap_or(0);
            let st = self.get16(OFF_STATUS) | STATUS_CAP_LIST;
            self.put16(OFF_STATUS, st);
        }
        self.cap_tail = Some(at);
        // Keep capabilities dword-aligned.
        self.cap_next_free = (at + body.len() + 3) & !3;
        at
    }

    /// Read `size` (1, 2 or 4) bytes at config offset `off`; reads past the end
    /// of config space return ones.
    #[must_use]
    pub fn read(&self, off: usize, size: u8) -> u32 {
        let mut v = 0u32;
        for i in 0..usize::from(size) {
            let byte = self.cfg.get(off + i).copied().unwrap_or(0xFF);
            v |= u32::from(byte) << (8 * i);
        }
        v
    }

    /// Write `size` (1, 2 or 4) bytes at config offset `off`; only bits the
    /// function implements change.
    pub fn write(&mut self, off: usize, size: u8, val: u32) {
        for i in 0..usize::from(size) {
            let Some(&mask) = self.wmask.get(off + i) else {
                continue;
            };
            let byte = val.to_le_bytes()[i];
            let cur = self.cfg[off + i];
            self.cfg[off + i] = (cur & !mask) | (byte & mask);
        }
    }

    /// The `COMMAND` register.
    #[must_use]
    pub const fn command(&self) -> u16 {
        self.get16(OFF_COMMAND)
    }

    /// Whether the guest has enabled memory decoding.
    #[must_use]
    pub const fn mem_enabled(&self) -> bool {
        self.command() & CMD_MEM != 0
    }

    /// Whether the guest has enabled bus mastering (DMA).
    #[must_use]
    pub const fn bus_master(&self) -> bool {
        self.command() & CMD_BUS_MASTER != 0
    }

    /// Whether `INTx` interrupts are disabled.
    #[must_use]
    pub const fn intx_disabled(&self) -> bool {
        self.command() & CMD_INTX_DISABLE != 0
    }

    /// The BAR0 window `(base, size)` the guest has programmed, if memory
    /// decoding is enabled and an address has been assigned.
    #[must_use]
    pub fn bar0(&self) -> Option<(u64, u64)> {
        if self.bar0_size == 0 || !self.mem_enabled() {
            return None;
        }
        let lo = u32::from_le_bytes([
            self.cfg[OFF_BAR0],
            self.cfg[OFF_BAR0 + 1],
            self.cfg[OFF_BAR0 + 2],
            self.cfg[OFF_BAR0 + 3],
        ]);
        let hi = u32::from_le_bytes([
            self.cfg[OFF_BAR1],
            self.cfg[OFF_BAR1 + 1],
            self.cfg[OFF_BAR1 + 2],
            self.cfg[OFF_BAR1 + 3],
        ]);
        let base = (u64::from(hi) << 32) | u64::from(lo & !0xF);
        (base != 0).then_some((base, self.bar0_size))
    }
}

/// The bus, device, function and register a `0xCF8` configuration address
/// selects, or `None` if its enable bit (31) is clear.
#[must_use]
pub const fn pci_target(addr: u32) -> Option<(u8, u8, u8, usize)> {
    if addr & 0x8000_0000 == 0 {
        return None;
    }
    Some((
        ((addr >> 16) & 0xFF) as u8,
        ((addr >> 11) & 0x1F) as u8,
        ((addr >> 8) & 0x07) as u8,
        (addr & 0xFC) as usize,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nic() -> PciFunction {
        let mut f = PciFunction::new(0x1AF4, 0x1042, [0x01, 0x80, 0x00], 1, (0x1AF4, 0x0002));
        f.set_bar0_mem64(0x4000);
        f.set_interrupt(10, 1);
        f
    }

    #[test]
    fn chipset_registers_can_be_made_writable_and_preset() {
        let mut f = PciFunction::new(0x8086, 0x7113, [0x06, 0x80, 0x00], 3, (0, 0));
        f.preset(0x0E, &[0x80]);
        f.preset(0x40, &[0x01]);
        f.set_writable(0x41, 1);
        assert_eq!(f.read(0x0E, 1), 0x80);
        f.write(0x0E, 1, 0x00);
        assert_eq!(f.read(0x0E, 1), 0x80, "a preset register stays read-only");
        f.write(0x40, 4, 0xB0C1);
        assert_eq!(f.read(0x40, 4), 0xB001, "only the opened byte changed");
        // Out-of-range requests are ignored rather than panicking.
        f.set_writable(0xFF, 8);
        f.preset(0xFF, &[1, 2, 3]);
    }

    #[test]
    fn identity_registers_read_back() {
        let f = nic();
        assert_eq!(f.read(0x00, 4), 0x1042_1AF4, "device<<16 | vendor");
        assert_eq!(f.read(0x08, 4), 0x0180_0001, "class 01:80:00, revision 1");
        assert_eq!(f.read(0x2C, 4), 0x0002_1AF4, "subsystem");
        assert_eq!(f.read(0x0E, 1), 0, "header type 0, single function");
        assert_eq!(f.read(0x3C, 2), 0x010A, "pin INTA, line 10");
    }

    #[test]
    fn config_reads_work_at_any_size_and_offset() {
        let f = nic();
        assert_eq!(f.read(0x00, 2), 0x1AF4);
        assert_eq!(f.read(0x02, 2), 0x1042);
        assert_eq!(f.read(0x01, 1), 0x1A);
        assert_eq!(f.read(0x03, 1), 0x10);
        assert_eq!(f.read(0xFE, 4) & 0xFFFF, 0, "bytes inside the space");
        assert_eq!(
            f.read(0xFE, 4) >> 16,
            0xFFFF,
            "bytes past the end read as ones"
        );
    }

    #[test]
    fn identity_registers_are_read_only() {
        let mut f = nic();
        f.write(0x00, 4, 0xDEAD_BEEF);
        f.write(0x08, 4, 0xFFFF_FFFF);
        f.write(0x3D, 1, 0xFF);
        assert_eq!(f.read(0x00, 4), 0x1042_1AF4);
        assert_eq!(f.read(0x08, 4), 0x0180_0001);
        assert_eq!(f.read(0x3D, 1), 1, "interrupt pin is read-only");
    }

    #[test]
    fn command_register_accepts_only_implemented_bits() {
        let mut f = nic();
        assert!(!f.mem_enabled() && !f.bus_master());
        f.write(0x04, 2, 0xFFFF);
        assert_eq!(
            f.command(),
            CMD_IO | CMD_MEM | CMD_BUS_MASTER | CMD_INTX_DISABLE
        );
        f.write(0x04, 2, u32::from(CMD_MEM | CMD_BUS_MASTER));
        assert!(f.mem_enabled() && f.bus_master() && !f.intx_disabled());
        // A 4-byte write covering the (read-only) status half does not change it.
        let status = f.read(0x06, 2);
        f.write(0x04, 4, 0xFFFF_0006);
        assert_eq!(f.read(0x06, 2), status);
    }

    #[test]
    fn bar_sizing_returns_the_size_mask() {
        let mut f = nic();
        // The standard probe: write all ones, read back, restore.
        f.write(0x10, 4, 0xFFFF_FFFF);
        f.write(0x14, 4, 0xFFFF_FFFF);
        let lo = f.read(0x10, 4);
        let hi = f.read(0x14, 4);
        assert_eq!(lo & 0xF, 0x4, "64-bit memory BAR type bits survive");
        let size_mask = (u64::from(hi) << 32) | u64::from(lo & !0xF);
        assert_eq!((!size_mask).wrapping_add(1), 0x4000);
        // 16 KiB: bits 13:0 of the low dword are not writable.
        assert_eq!(lo & 0x3FF0, 0);
    }

    #[test]
    fn programmed_bar0_is_reported_once_memory_decode_is_on() {
        let mut f = nic();
        assert_eq!(f.bar0(), None, "memory decoding is off");
        f.write(0x10, 4, 0x1000_0000);
        f.write(0x14, 4, 0);
        assert_eq!(f.bar0(), None, "still off");
        f.write(0x04, 2, u32::from(CMD_MEM));
        assert_eq!(f.bar0(), Some((0x1000_0000, 0x4000)));
        // An unaligned address bit below the size is dropped by the write mask.
        f.write(0x10, 4, 0x1000_1230);
        assert_eq!(f.bar0(), Some((0x1000_0000, 0x4000)));
        // A 64-bit address above 4 GiB.
        f.write(0x10, 4, 0x0000_0000);
        f.write(0x14, 4, 0x0000_0002);
        assert_eq!(f.bar0(), Some((0x2_0000_0000, 0x4000)));
        f.write(0x04, 2, 0);
        assert_eq!(f.bar0(), None);
    }

    #[test]
    fn an_unassigned_bar_is_not_a_window() {
        let mut f = nic();
        f.write(0x04, 2, u32::from(CMD_MEM));
        assert_eq!(f.bar0(), None, "address 0 means unassigned");
    }

    #[test]
    fn interrupt_line_is_writable_pin_is_not() {
        let mut f = nic();
        f.write(0x3C, 1, 11);
        assert_eq!(f.read(0x3C, 1), 11);
    }

    #[test]
    fn capabilities_form_a_linked_list() {
        let mut f = nic();
        assert_eq!(f.read(0x34, 1), 0, "no capabilities yet");
        assert_eq!(f.read(0x06, 2) & 0x10, 0);
        let a = f.add_capability(&[0x09, 0xFF, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0]);
        let b = f.add_capability(&[0x09, 0xFF, 16, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0x10, 0, 0, 0]);
        assert_eq!(a, 0x40);
        assert_eq!(b, 0x50);
        assert_eq!(f.read(0x34, 1), 0x40, "capability pointer");
        assert_ne!(f.read(0x06, 2) & 0x10, 0, "status advertises a list");
        assert_eq!(f.read(0x41, 1), 0x50, "first links to second");
        assert_eq!(f.read(0x51, 1), 0, "last terminates the list");
        assert_eq!(f.read(0x40, 1), 0x09, "vendor-specific capability id");
        assert_eq!(f.read(0x43, 1), 1);
        assert_eq!(f.read(0x53, 1), 2);
    }

    #[test]
    fn capabilities_stay_dword_aligned() {
        let mut f = nic();
        let a = f.add_capability(&[0x09, 0, 5, 1, 0]);
        let b = f.add_capability(&[0x05, 0, 0, 0]);
        assert_eq!(a, 0x40);
        assert_eq!(b, 0x48, "5 bytes round up to the next dword");
    }

    #[test]
    fn cf8_address_decodes_to_bdf_and_register() {
        // enable | bus 0 | device 3 | function 0 | register 0x10
        assert_eq!(pci_target(0x8000_1810), Some((0, 3, 0, 0x10)));
        assert_eq!(pci_target(0x8001_0A04), Some((1, 1, 2, 0x04)));
        assert_eq!(pci_target(0x0000_1810), None, "enable bit clear");
        assert_eq!(
            pci_target(0x8000_1813),
            Some((0, 3, 0, 0x10)),
            "low 2 bits dropped"
        );
    }
}

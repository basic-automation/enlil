use super::*;
use crate::blockdev::{Allow, BlockError, DiskMap, RamDisk};
use crate::pci_dev::{CMD_BUS_MASTER, CMD_INTX_DISABLE, CMD_MEM};
use core::cell::RefCell;

/// Guest RAM for the tests: a bounds-checked byte vector.
struct Mem(RefCell<Vec<u8>>);

impl Mem {
    fn new(len: usize) -> Self {
        Self(RefCell::new(alloc::vec![0; len]))
    }

    fn put(&self, gpa: u64, data: &[u8]) {
        assert!(self.write(gpa, data));
    }

    fn get(&self, gpa: u64, len: usize) -> Vec<u8> {
        let mut b = alloc::vec![0u8; len];
        assert!(self.read(gpa, &mut b));
        b
    }
}

impl GuestMemory for Mem {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool {
        let m = self.0.borrow();
        let Some(end) = gpa.checked_add(buf.len() as u64) else {
            return false;
        };
        if end > m.len() as u64 {
            return false;
        }
        buf.copy_from_slice(&m[gpa as usize..end as usize]);
        true
    }

    fn write(&self, gpa: u64, data: &[u8]) -> bool {
        let mut m = self.0.borrow_mut();
        let Some(end) = gpa.checked_add(data.len() as u64) else {
            return false;
        };
        if end > m.len() as u64 {
            return false;
        }
        m[gpa as usize..end as usize].copy_from_slice(data);
        true
    }
}

const DESC: u64 = 0x1000;
const AVAIL: u64 = 0x2000;
const USED: u64 = 0x3000;
const HDR: u64 = 0x4000;
const STATUS: u64 = 0x4800;
const DATA: u64 = 0x10000;

/// A simulated guest driver around a device.
struct Driver {
    dev: VirtioBlk,
    mem: Mem,
    avail_idx: u16,
    qsize: u16,
}

impl Driver {
    fn with_backend(backend: Box<dyn BlockBackend>, read_only: bool) -> Self {
        Self {
            dev: VirtioBlk::new(backend, 10, read_only),
            mem: Mem::new(0x40_0000),
            avail_idx: 0,
            qsize: 8,
        }
    }

    fn new(sectors: usize) -> Self {
        Self::with_backend(Box::new(RamDisk::new(sectors)), false)
    }

    fn cw(&mut self, off: usize, size: u8, val: u64) {
        self.dev
            .mmio_write((COMMON_OFF + off) as u64, size, val, &self.mem);
    }

    fn cr(&mut self, off: usize, size: u8) -> u64 {
        self.dev.mmio_read((COMMON_OFF + off) as u64, size)
    }

    fn status(&mut self) -> u8 {
        self.cr(0x14, 1) as u8
    }

    /// The handshake a Linux virtio-pci driver performs.
    fn init(&mut self) {
        self.dev
            .pci
            .write(0x04, 2, u32::from(CMD_MEM | CMD_BUS_MASTER));
        self.cw(0x14, 1, 0); // reset
        self.cw(0x14, 1, 1); // ACKNOWLEDGE
        self.cw(0x14, 1, 3); // + DRIVER
        // Read both feature words and accept what we understand.
        self.cw(0x00, 4, 0);
        let lo = self.cr(0x04, 4);
        self.cw(0x00, 4, 1);
        let hi = self.cr(0x04, 4);
        assert_ne!(hi & 1, 0, "VERSION_1 must be offered");
        self.cw(0x08, 4, 0);
        self.cw(0x0C, 4, lo);
        self.cw(0x08, 4, 1);
        self.cw(0x0C, 4, hi);
        self.cw(0x14, 1, 0x0B); // + FEATURES_OK
        assert_ne!(
            self.status() & S_FEATURES_OK,
            0,
            "device must accept the features"
        );
        // Queue 0.
        self.cw(0x16, 2, 0);
        assert_eq!(
            self.cr(0x18, 2),
            u64::from(QUEUE_MAX),
            "default size is the maximum"
        );
        self.cw(0x18, 2, u64::from(self.qsize));
        self.cw(0x20, 4, DESC);
        self.cw(0x24, 4, 0);
        self.cw(0x28, 4, AVAIL);
        self.cw(0x2C, 4, 0);
        self.cw(0x30, 4, USED);
        self.cw(0x34, 4, 0);
        self.cw(0x1C, 2, 1); // enable
        self.cw(0x14, 1, 0x0F); // + DRIVER_OK
    }

    fn desc(&self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut d = [0u8; 16];
        d[0..8].copy_from_slice(&addr.to_le_bytes());
        d[8..12].copy_from_slice(&len.to_le_bytes());
        d[12..14].copy_from_slice(&flags.to_le_bytes());
        d[14..16].copy_from_slice(&next.to_le_bytes());
        self.mem.put(DESC + 16 * u64::from(idx), &d);
    }

    /// Post a request chain `header -> data segments -> status` and return its
    /// head descriptor index. The chain is not yet notified.
    fn post(&mut self, kind: u32, sector: u64, data: &[(u64, u32, bool)]) -> u16 {
        let mut h = [0u8; 16];
        h[0..4].copy_from_slice(&kind.to_le_bytes());
        h[8..16].copy_from_slice(&sector.to_le_bytes());
        self.mem.put(HDR, &h);
        self.mem.put(STATUS, &[0xEE]);
        let n = data.len() as u16;
        self.desc(0, HDR, 16, D_NEXT, 1);
        for (i, &(gpa, len, write)) in data.iter().enumerate() {
            let i = i as u16;
            self.desc(
                1 + i,
                gpa,
                len,
                D_NEXT | if write { D_WRITE } else { 0 },
                2 + i,
            );
        }
        self.desc(1 + n, STATUS, 1, D_WRITE, 0);
        let slot = u64::from(self.avail_idx % self.qsize);
        self.mem.put(AVAIL + 4 + 2 * slot, &0u16.to_le_bytes());
        self.avail_idx = self.avail_idx.wrapping_add(1);
        self.mem.put(AVAIL + 2, &self.avail_idx.to_le_bytes());
        0
    }

    fn notify(&mut self) -> u32 {
        // Doorbell write: the processed count is observable via the used ring.
        let before = self.used_idx();
        self.dev.mmio_write(NOTIFY_OFF as u64, 2, 0, &self.mem);
        u32::from(self.used_idx().wrapping_sub(before))
    }

    fn used_idx(&self) -> u16 {
        u16::from_le_bytes(self.mem.get(USED + 2, 2).try_into().unwrap())
    }

    fn used_elem(&self, i: u16) -> (u32, u32) {
        let b = self.mem.get(USED + 4 + 8 * u64::from(i % self.qsize), 8);
        (
            u32::from_le_bytes(b[0..4].try_into().unwrap()),
            u32::from_le_bytes(b[4..8].try_into().unwrap()),
        )
    }

    fn result(&self) -> u8 {
        self.mem.get(STATUS, 1)[0]
    }

    /// Post, notify and return `(status, used_len)`.
    fn request(&mut self, kind: u32, sector: u64, data: &[(u64, u32, bool)]) -> (u8, u32) {
        let before = self.used_idx();
        self.post(kind, sector, data);
        assert_eq!(self.notify(), 1, "exactly one request completes");
        let (id, len) = self.used_elem(before);
        assert_eq!(id, 0, "used id is the head descriptor");
        (self.result(), len)
    }
}

#[test]
fn identity_and_capabilities_match_the_virtio_spec() {
    let d = Driver::new(8);
    let pci = &d.dev.pci;
    assert_eq!(pci.read(0, 4), 0x1042_1AF4);
    assert_eq!(pci.read(0x08, 1), 1, "revision >= 1 for modern virtio");
    assert_eq!(pci.read(0x0A, 2), 0x0180, "mass storage class");
    // Walk the capability chain: types 1..=4, all in BAR 0, with sane offsets.
    let mut ptr = pci.read(0x34, 1) as usize;
    let mut types = Vec::new();
    while ptr != 0 {
        assert_eq!(pci.read(ptr, 1), 0x09, "vendor-specific capability");
        let cap_len = pci.read(ptr + 2, 1);
        let cfg_type = pci.read(ptr + 3, 1);
        assert_eq!(pci.read(ptr + 4, 1), 0, "BAR 0");
        let off = pci.read(ptr + 8, 4);
        let len = pci.read(ptr + 12, 4);
        assert!(u64::from(off + len) <= BAR0_SIZE);
        if cfg_type == 2 {
            assert_eq!(cap_len, 20);
            assert_eq!(pci.read(ptr + 16, 4), NOTIFY_MULTIPLIER);
        } else {
            assert_eq!(cap_len, 16);
        }
        types.push((cfg_type, off));
        ptr = pci.read(ptr + 1, 1) as usize;
    }
    assert_eq!(types, [(1, 0x0000), (2, 0x1000), (3, 0x2000), (4, 0x3000)]);
    assert_eq!(pci.read(0x3C, 2), 0x010A, "INTA on PIC line 10");
}

#[test]
fn feature_negotiation_requires_version_1_and_a_subset() {
    let mut d = Driver::new(8);
    d.cw(0x14, 1, 0);
    d.cw(0x14, 1, 3);
    // A legacy driver (no VERSION_1) is refused.
    d.cw(0x08, 4, 0);
    d.cw(0x0C, 4, u64::from(F_FLUSH as u32));
    d.cw(0x14, 1, 0x0B);
    assert_eq!(d.status() & S_FEATURES_OK, 0, "FEATURES_OK is withheld");
    // Asking for a feature that was not offered is refused too.
    d.cw(0x14, 1, 0);
    d.cw(0x14, 1, 3);
    d.cw(0x08, 4, 1);
    d.cw(0x0C, 4, 1); // VERSION_1
    d.cw(0x08, 4, 0);
    d.cw(0x0C, 4, 1 << 1); // SIZE_MAX: not offered
    d.cw(0x14, 1, 0x0B);
    assert_eq!(d.status() & S_FEATURES_OK, 0);
    // A good set is accepted.
    d.cw(0x14, 1, 0);
    d.cw(0x14, 1, 3);
    d.cw(0x08, 4, 1);
    d.cw(0x0C, 4, 1);
    d.cw(0x08, 4, 0);
    d.cw(0x0C, 4, u64::from(F_FLUSH as u32));
    d.cw(0x14, 1, 0x0B);
    assert_ne!(d.status() & S_FEATURES_OK, 0);
}

#[test]
fn device_offers_the_expected_features() {
    let mut d = Driver::new(8);
    d.cw(0x00, 4, 0);
    let lo = d.cr(0x04, 4);
    d.cw(0x00, 4, 1);
    let hi = d.cr(0x04, 4);
    assert_eq!(hi, 1, "only VERSION_1 in the high word");
    assert_ne!(lo & (1 << 9), 0, "FLUSH");
    assert_ne!(lo & (1 << 6), 0, "BLK_SIZE");
    assert_eq!(lo & (1 << 5), 0, "RO only when read-only");
    d.cw(0x00, 4, 2);
    assert_eq!(d.cr(0x04, 4), 0, "unknown select reads zero");
    let ro = Driver::with_backend(Box::new(RamDisk::new(8)), true);
    assert_ne!(ro.dev.device_features() & F_RO, 0);
}

#[test]
fn a_status_write_of_zero_resets_the_device() {
    let mut d = Driver::new(8);
    d.init();
    assert_eq!(d.status(), 0x0F);
    d.cw(0x14, 1, 0);
    assert_eq!(d.status(), 0);
    assert_eq!(d.cr(0x1C, 2), 0, "queue disabled by reset");
    d.cw(0x08, 4, 0);
    assert_eq!(d.cr(0x0C, 4), 0, "driver features cleared");
}

#[test]
fn queue_size_is_validated_and_other_queues_do_not_exist() {
    let mut d = Driver::new(8);
    d.cw(0x16, 2, 0);
    for bad in [0u64, 3, 100, 256, 1000] {
        d.cw(0x18, 2, bad);
        assert_eq!(d.cr(0x18, 2), u64::from(QUEUE_MAX), "size {bad} rejected");
    }
    d.cw(0x18, 2, 64);
    assert_eq!(d.cr(0x18, 2), 64);
    d.cw(0x16, 2, 1);
    assert_eq!(d.cr(0x18, 2), 0, "queue 1 does not exist");
    assert_eq!(d.cr(0x12, 2), 1, "one queue advertised");
    d.cw(0x1C, 2, 1);
    d.cw(0x16, 2, 0);
    assert_eq!(d.cr(0x1C, 2), 0, "writes to a missing queue are ignored");
}

#[test]
fn device_config_reports_capacity_and_block_size() {
    let mut d = Driver::new(2048);
    let cap = d.dev.mmio_read(DEVICE_OFF as u64, 8);
    assert_eq!(cap, 2048);
    assert_eq!(d.dev.mmio_read(DEVICE_OFF as u64, 4), 2048, "low half");
    assert_eq!(d.dev.mmio_read(DEVICE_OFF as u64 + 4, 4), 0, "high half");
    assert_eq!(d.dev.mmio_read(DEVICE_OFF as u64 + 0x14, 4), 512);
    assert_eq!(d.dev.mmio_read(DEVICE_OFF as u64 + 0x0C, 4), 126);
}

#[test]
fn a_read_fills_scattered_buffers_and_completes() {
    let mut disk = RamDisk::new(16);
    for s in 0..16 {
        disk.bytes_mut()[s * SECTOR..(s + 1) * SECTOR].fill(s as u8 + 1);
    }
    let mut d = Driver::with_backend(Box::new(disk), false);
    d.init();
    // Read sectors 2..6 (4 sectors) into two separate buffers.
    let (status, len) = d.request(
        T_IN,
        2,
        &[
            (DATA, 3 * SECTOR as u32, true),
            (DATA + 0x8000, SECTOR as u32, true),
        ],
    );
    assert_eq!(status, R_OK);
    assert_eq!(len, 4 * SECTOR as u32 + 1, "data bytes + the status byte");
    assert!(d.mem.get(DATA, SECTOR).iter().all(|&b| b == 3));
    assert!(
        d.mem
            .get(DATA + 2 * SECTOR as u64, SECTOR)
            .iter()
            .all(|&b| b == 5)
    );
    assert!(d.mem.get(DATA + 0x8000, SECTOR).iter().all(|&b| b == 6));
    assert_eq!(d.used_idx(), 1);
}

#[test]
fn a_write_lands_in_the_backing_disk() {
    let mut d = Driver::new(16);
    d.init();
    d.mem.put(DATA, &[0xA5u8; 2 * SECTOR]);
    let (status, len) = d.request(T_OUT, 4, &[(DATA, 2 * SECTOR as u32, false)]);
    assert_eq!((status, len), (R_OK, 1), "no data returned to the guest");
    // Read it back through the device.
    let (status, _) = d.request(T_IN, 4, &[(DATA + 0x4000, 2 * SECTOR as u32, true)]);
    assert_eq!(status, R_OK);
    assert!(
        d.mem
            .get(DATA + 0x4000, 2 * SECTOR)
            .iter()
            .all(|&b| b == 0xA5)
    );
    // Neighbouring sectors untouched.
    let (_, _) = d.request(T_IN, 3, &[(DATA + 0x6000, SECTOR as u32, true)]);
    assert!(d.mem.get(DATA + 0x6000, SECTOR).iter().all(|&b| b == 0));
}

#[test]
fn flush_get_id_and_unknown_types() {
    let mut d = Driver::new(16);
    d.init();
    assert_eq!(d.request(T_FLUSH, 0, &[]), (R_OK, 1));
    let (status, len) = d.request(T_GET_ID, 0, &[(DATA, 20, true)]);
    assert_eq!(status, R_OK);
    assert_eq!(len, 21);
    assert_eq!(&d.mem.get(DATA, 18), b"enlil-virtual-disk");
    assert_eq!(d.request(0x77, 0, &[]).0, R_UNSUPP);
}

#[test]
fn a_read_only_device_refuses_writes() {
    let mut d = Driver::with_backend(Box::new(RamDisk::new(16)), true);
    d.init();
    d.mem.put(DATA, &[0x11u8; SECTOR]);
    assert_eq!(
        d.request(T_OUT, 0, &[(DATA, SECTOR as u32, false)]).0,
        R_IOERR
    );
    // Reads still work.
    assert_eq!(
        d.request(T_IN, 0, &[(DATA + 0x1000, SECTOR as u32, true)])
            .0,
        R_OK
    );
}

#[test]
fn the_allow_list_is_enforced_through_the_device() {
    let mut disk = RamDisk::new(16);
    for s in 0..16 {
        disk.bytes_mut()[s * SECTOR..(s + 1) * SECTOR].fill(0x40 + s as u8);
    }
    let map = DiskMap::new(
        disk,
        alloc::vec![
            Allow {
                start: 0,
                end: 4,
                writable: false
            },
            Allow {
                start: 4,
                end: 8,
                writable: true
            },
        ],
    );
    let mut d = Driver::with_backend(Box::new(map), false);
    d.init();
    // Reading a denied sector returns zeros, not the host data.
    assert_eq!(d.request(T_IN, 12, &[(DATA, SECTOR as u32, true)]).0, R_OK);
    assert!(
        d.mem.get(DATA, SECTOR).iter().all(|&b| b == 0),
        "denied read is zeros"
    );
    // Reading an allowed one returns real data.
    d.request(T_IN, 1, &[(DATA, SECTOR as u32, true)]);
    assert!(d.mem.get(DATA, SECTOR).iter().all(|&b| b == 0x41));
    // Writing the read-only range and a denied range fails...
    d.mem.put(DATA, &[0xFFu8; SECTOR]);
    assert_eq!(
        d.request(T_OUT, 1, &[(DATA, SECTOR as u32, false)]).0,
        R_IOERR
    );
    assert_eq!(
        d.request(T_OUT, 12, &[(DATA, SECTOR as u32, false)]).0,
        R_IOERR
    );
    // ...writing the writable range succeeds.
    assert_eq!(d.request(T_OUT, 5, &[(DATA, SECTOR as u32, false)]).0, R_OK);
    d.request(T_IN, 5, &[(DATA + 0x2000, SECTOR as u32, true)]);
    assert!(d.mem.get(DATA + 0x2000, SECTOR).iter().all(|&b| b == 0xFF));
    // The denied sectors were never modified.
    d.request(T_IN, 1, &[(DATA + 0x3000, SECTOR as u32, true)]);
    assert!(d.mem.get(DATA + 0x3000, SECTOR).iter().all(|&b| b == 0x41));
}

#[test]
fn bad_io_parameters_complete_with_an_error_and_never_panic() {
    let mut d = Driver::new(16);
    d.init();
    // Out-of-range sector.
    assert_eq!(
        d.request(T_IN, 1000, &[(DATA, SECTOR as u32, true)]).0,
        R_IOERR
    );
    assert_eq!(
        d.request(T_IN, u64::MAX, &[(DATA, SECTOR as u32, true)]).0,
        R_IOERR
    );
    // Length not a multiple of the sector size.
    assert_eq!(d.request(T_IN, 0, &[(DATA, 100, true)]).0, R_IOERR);
    // Wrong direction: a read into a device-READABLE buffer.
    assert_eq!(
        d.request(T_IN, 0, &[(DATA, SECTOR as u32, false)]).0,
        R_IOERR
    );
    // Wrong direction: a write from a device-WRITABLE buffer.
    assert_eq!(
        d.request(T_OUT, 0, &[(DATA, SECTOR as u32, true)]).0,
        R_IOERR
    );
    // A data buffer that points outside guest memory.
    assert_eq!(
        d.request(T_IN, 0, &[(0xFFFF_F000, SECTOR as u32, true)]).0,
        R_IOERR
    );
    assert_eq!(
        d.request(T_OUT, 0, &[(u64::MAX - 100, SECTOR as u32, false)])
            .0,
        R_IOERR
    );
    // An absurdly large request.
    assert_eq!(d.request(T_IN, 0, &[(DATA, 64 << 20, true)]).0, R_IOERR);
}

#[test]
fn malformed_chains_are_dropped_safely() {
    let mut d = Driver::new(16);
    d.init();
    // A descriptor chain that loops forever: header -> itself.
    d.desc(0, HDR, 16, D_NEXT, 0);
    d.mem.put(AVAIL + 4, &0u16.to_le_bytes());
    d.avail_idx = 1;
    d.mem.put(AVAIL + 2, &1u16.to_le_bytes());
    assert_eq!(d.notify(), 1, "completed (with length 0), did not hang");
    assert_eq!(d.used_elem(0), (0, 0));
    // An indirect descriptor is not supported.
    d.desc(0, HDR, 16, D_NEXT, 1);
    d.desc(1, DATA, 16, D_INDIRECT, 0);
    d.mem.put(AVAIL + 4 + 2, &0u16.to_le_bytes());
    d.avail_idx = 2;
    d.mem.put(AVAIL + 2, &2u16.to_le_bytes());
    assert_eq!(d.notify(), 1);
    assert_eq!(d.used_elem(1), (0, 0));
    // A `next` index past the end of the queue.
    d.desc(0, HDR, 16, D_NEXT, 200);
    d.mem.put(AVAIL + 4 + 4, &0u16.to_le_bytes());
    d.avail_idx = 3;
    d.mem.put(AVAIL + 2, &3u16.to_le_bytes());
    assert_eq!(d.notify(), 1);
    // A head index past the end of the queue.
    d.mem.put(AVAIL + 4 + 6, &200u16.to_le_bytes());
    d.avail_idx = 4;
    d.mem.put(AVAIL + 2, &4u16.to_le_bytes());
    assert_eq!(d.notify(), 1);
    // A header that is too short, or a status that is not writable.
    d.desc(0, HDR, 8, D_NEXT, 1);
    d.desc(1, STATUS, 1, D_WRITE, 0);
    d.mem.put(AVAIL + 4 + 8, &0u16.to_le_bytes());
    d.avail_idx = 5;
    d.mem.put(AVAIL + 2, &5u16.to_le_bytes());
    assert_eq!(d.notify(), 1);
    assert_eq!(d.used_elem(4), (0, 0));
}

#[test]
fn many_requests_wrap_the_ring_correctly() {
    let mut d = Driver::new(64);
    d.init(); // queue size 8
    for round in 0..40u32 {
        d.mem.put(DATA, &[round as u8; SECTOR]);
        assert_eq!(
            d.request(T_OUT, u64::from(round), &[(DATA, SECTOR as u32, false)])
                .0,
            R_OK
        );
    }
    assert_eq!(d.used_idx(), 40, "the used index counts every completion");
    for round in 0..40u32 {
        d.request(
            T_IN,
            u64::from(round),
            &[(DATA + 0x1000, SECTOR as u32, true)],
        );
        assert!(
            d.mem
                .get(DATA + 0x1000, SECTOR)
                .iter()
                .all(|&b| b == round as u8)
        );
    }
}

#[test]
fn a_bogus_available_index_cannot_make_the_device_spin() {
    let mut d = Driver::new(16);
    d.init();
    // Claim 60000 outstanding requests with no descriptors posted.
    d.mem.put(AVAIL + 2, &60000u16.to_le_bytes());
    let n = d.dev.process(&d.mem);
    assert!(
        n <= u32::from(d.qsize),
        "bounded to one queue per notification, got {n}"
    );
}

#[test]
fn nothing_is_processed_before_driver_ok_or_without_bus_mastering() {
    let mut d = Driver::new(16);
    d.init();
    d.post(T_FLUSH, 0, &[]);
    // Bus mastering off: the device must not touch guest memory.
    d.dev.pci.write(0x04, 2, u32::from(CMD_MEM));
    assert_eq!(d.dev.process(&d.mem), 0);
    d.dev
        .pci
        .write(0x04, 2, u32::from(CMD_MEM | CMD_BUS_MASTER));
    // DRIVER_OK cleared: not running.
    d.cw(0x14, 1, 0x0B);
    assert_eq!(d.dev.process(&d.mem), 0);
    d.cw(0x14, 1, 0x0F);
    assert_eq!(d.dev.process(&d.mem), 1);
}

#[test]
fn interrupt_status_is_read_to_clear_and_respects_intx_disable() {
    let mut d = Driver::new(16);
    d.init();
    assert!(!d.dev.irq_line());
    d.request(T_FLUSH, 0, &[]);
    assert!(d.dev.irq_line(), "a completion asserts the interrupt");
    // INTx disable masks the line without losing the status.
    d.dev.pci.write(
        0x04,
        2,
        u32::from(CMD_MEM | CMD_BUS_MASTER | CMD_INTX_DISABLE),
    );
    assert!(!d.dev.irq_line());
    d.dev
        .pci
        .write(0x04, 2, u32::from(CMD_MEM | CMD_BUS_MASTER));
    assert!(d.dev.irq_line());
    // The ISR register returns the status and clears it.
    assert_eq!(d.dev.mmio_read(ISR_OFF as u64, 1), 1);
    assert!(!d.dev.irq_line());
    assert_eq!(
        d.dev.mmio_read(ISR_OFF as u64, 1),
        0,
        "second read sees nothing"
    );
}

#[test]
fn only_the_queue_zero_doorbell_runs_the_queue() {
    let mut d = Driver::new(16);
    d.init();
    d.post(T_FLUSH, 0, &[]);
    d.dev.mmio_write(NOTIFY_OFF as u64 + 4, 2, 1, &d.mem); // a doorbell for queue 1
    assert_eq!(d.used_idx(), 0, "no queue 1: nothing runs");
    d.dev.mmio_write(NOTIFY_OFF as u64, 2, 0, &d.mem);
    assert_eq!(d.used_idx(), 1);
}

#[test]
fn read_only_regions_ignore_writes() {
    let mut d = Driver::new(16);
    d.dev.mmio_write(DEVICE_OFF as u64, 8, 0xDEAD, &d.mem);
    assert_eq!(d.dev.mmio_read(DEVICE_OFF as u64, 8), 16);
    d.dev.mmio_write(ISR_OFF as u64, 1, 0xFF, &d.mem);
    assert_eq!(d.dev.mmio_read(ISR_OFF as u64, 1), 0);
    assert_eq!(
        d.dev.mmio_read(NOTIFY_OFF as u64, 2),
        0,
        "the doorbell reads as zero"
    );
    assert_eq!(d.dev.mmio_read(0x3FFF, 1), 0, "beyond the regions");
}

#[test]
fn backend_errors_surface_as_io_errors() {
    struct Failing;
    impl BlockBackend for Failing {
        fn sectors(&self) -> u64 {
            8
        }
        fn read(&mut self, _: u64, _: &mut [u8]) -> Result<(), BlockError> {
            Err(BlockError::Io)
        }
        fn write(&mut self, _: u64, _: &[u8]) -> Result<(), BlockError> {
            Err(BlockError::Io)
        }
        fn flush(&mut self) -> Result<(), BlockError> {
            Err(BlockError::Io)
        }
    }
    let mut d = Driver::with_backend(Box::new(Failing), false);
    d.init();
    d.mem.put(DATA, &[1u8; SECTOR]);
    assert_eq!(
        d.request(T_IN, 0, &[(DATA, SECTOR as u32, true)]).0,
        R_IOERR
    );
    assert_eq!(
        d.request(T_OUT, 0, &[(DATA, SECTOR as u32, false)]).0,
        R_IOERR
    );
    assert_eq!(d.request(T_FLUSH, 0, &[]).0, R_IOERR);
}

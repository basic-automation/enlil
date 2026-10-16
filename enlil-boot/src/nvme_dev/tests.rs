use super::*;
use crate::blockdev::{Allow, DiskMap, RamDisk};
use core::cell::RefCell;

/// Guest RAM for the tests: a bounds-checked byte vector.
struct Mem(RefCell<Vec<u8>>);

impl Mem {
    fn new(len: usize) -> Self {
        Self(RefCell::new(vec![0; len]))
    }

    fn put(&self, gpa: u64, data: &[u8]) {
        assert!(self.write(gpa, data));
    }

    fn get(&self, gpa: u64, len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
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

const ASQ: u64 = 0x1000;
const ACQ: u64 = 0x2000;
const IOSQ: u64 = 0x3000;
const IOCQ: u64 = 0x4000;
const DATA: u64 = 0x10_0000;
const LIST: u64 = 0x8000;
const QD: u16 = 16;

/// A completion as the guest reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cqe {
    result: u32,
    sq_head: u16,
    sqid: u16,
    cid: u16,
    /// Status code, `SCT << 8 | SC`.
    status: u16,
}

/// A simulated guest `NVMe` driver around a device.
struct Driver {
    dev: NvmeDevice,
    mem: Mem,
    next_cid: u16,
    /// Next free submission slot / completion slot per queue (0 = admin).
    sq_tail: [u16; 3],
    cq_head: [u16; 3],
    cq_phase: [bool; 3],
}

impl Driver {
    fn new(backend: Box<dyn BlockBackend>, read_only: bool) -> Self {
        Self {
            dev: NvmeDevice::new(backend, NvmeIdentity::default(), 11, read_only),
            mem: Mem::new(8 << 20),
            next_cid: 1,
            sq_tail: [0; 3],
            cq_head: [0; 3],
            cq_phase: [true; 3],
        }
    }

    fn write_reg(&mut self, off: u64, size: u8, val: u64) {
        self.dev.mmio_write(off, size, val, &self.mem);
    }

    /// Bring the controller up the way a driver does.
    fn enable(&mut self) {
        let aqa = u64::from(QD - 1) | (u64::from(QD - 1) << 16);
        self.write_reg(0x24, 4, aqa);
        self.write_reg(0x28, 8, ASQ);
        self.write_reg(0x30, 8, ACQ);
        // EN, IOSQES = 6 (64 B), IOCQES = 4 (16 B).
        self.write_reg(0x14, 4, 1 | (6 << 16) | (4 << 20));
        assert!(self.dev.ready(), "CSTS.RDY after enabling");
    }

    /// Build a 64-byte submission entry.
    fn sqe(
        opcode: u8,
        cid: u16,
        nsid: u32,
        prp1: u64,
        prp2: u64,
        cdw: [u32; 6],
    ) -> [u8; SQE_BYTES] {
        let mut e = [0u8; SQE_BYTES];
        e[0] = opcode;
        e[2..4].copy_from_slice(&cid.to_le_bytes());
        e[4..8].copy_from_slice(&nsid.to_le_bytes());
        e[24..32].copy_from_slice(&prp1.to_le_bytes());
        e[32..40].copy_from_slice(&prp2.to_le_bytes());
        for (i, d) in cdw.iter().enumerate() {
            e[40 + i * 4..44 + i * 4].copy_from_slice(&d.to_le_bytes());
        }
        e
    }

    /// Submit `entry` on queue `q`, ring the doorbell, and reap its completion.
    fn run(&mut self, q: usize, entry: &[u8; SQE_BYTES]) -> Cqe {
        let cid = u16::from_le_bytes([entry[2], entry[3]]);
        self.submit(q, entry);
        self.reap(q, cid).expect("a completion")
    }

    fn submit(&mut self, q: usize, entry: &[u8; SQE_BYTES]) {
        let base = if q == 0 { ASQ } else { IOSQ };
        let tail = self.sq_tail[q];
        self.mem
            .put(base + u64::from(tail) * SQE_BYTES as u64, entry);
        self.sq_tail[q] = (tail + 1) % QD;
        self.write_reg(0x1000 + (2 * q as u64) * 4, 4, u64::from(self.sq_tail[q]));
    }

    /// Read the next completion on queue `q`'s completion queue, if present.
    fn reap(&mut self, q: usize, cid: u16) -> Option<Cqe> {
        let base = if q == 0 { ACQ } else { IOCQ };
        let head = self.cq_head[q];
        let raw = self
            .mem
            .get(base + u64::from(head) * CQE_BYTES as u64, CQE_BYTES);
        let status = u16::from_le_bytes([raw[14], raw[15]]);
        if (status & 1 != 0) != self.cq_phase[q] {
            return None;
        }
        let cqe = Cqe {
            result: u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
            sq_head: u16::from_le_bytes([raw[8], raw[9]]),
            sqid: u16::from_le_bytes([raw[10], raw[11]]),
            cid: u16::from_le_bytes([raw[12], raw[13]]),
            // Status: SC in bits 8:1, SCT in 11:9.
            status: ((status >> 1) & 0xFF) | (((status >> 9) & 0x7) << 8),
        };
        assert_eq!(cqe.cid, cid, "completions arrive in order");
        self.cq_head[q] = (head + 1) % QD;
        if self.cq_head[q] == 0 {
            self.cq_phase[q] = !self.cq_phase[q];
        }
        self.write_reg(
            0x1000 + (2 * q as u64 + 1) * 4,
            4,
            u64::from(self.cq_head[q]),
        );
        Some(cqe)
    }

    fn cid(&mut self) -> u16 {
        let c = self.next_cid;
        self.next_cid += 1;
        c
    }

    fn admin(&mut self, opcode: u8, nsid: u32, prp1: u64, prp2: u64, cdw: [u32; 6]) -> Cqe {
        let cid = self.cid();
        self.run(0, &Self::sqe(opcode, cid, nsid, prp1, prp2, cdw))
    }

    /// Create I/O queue pair 1 (CQ then SQ), `QD` entries each.
    fn create_io_queues(&mut self) {
        let size = u32::from(QD - 1) << 16;
        let cq = self.admin(ADMIN_CREATE_CQ, 0, IOCQ, 0, [1 | size, 0b11, 0, 0, 0, 0]);
        assert_eq!(cq.status, SC_OK);
        let sq = self.admin(
            ADMIN_CREATE_SQ,
            0,
            IOSQ,
            0,
            [1 | size, 0b1 | (1 << 16), 0, 0, 0, 0],
        );
        assert_eq!(sq.status, SC_OK);
    }

    fn io(&mut self, opcode: u8, lba: u64, blocks: u32, prp1: u64, prp2: u64) -> Cqe {
        let cid = self.cid();
        let e = Self::sqe(
            opcode,
            cid,
            1,
            prp1,
            prp2,
            [lba as u32, (lba >> 32) as u32, blocks - 1, 0, 0, 0],
        );
        self.run(1, &e)
    }
}

fn ramdisk(sectors: usize) -> Box<dyn BlockBackend> {
    let mut d = RamDisk::new(sectors);
    for (i, b) in d.bytes_mut().iter_mut().enumerate() {
        *b = (i * 7 + i / SECTOR) as u8;
    }
    Box::new(d)
}

fn pattern(lba: u64, blocks: usize) -> Vec<u8> {
    (0..blocks * SECTOR)
        .map(|i| {
            let abs = lba as usize * SECTOR + i;
            (abs * 7 + abs / SECTOR) as u8
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Registers and enabling
// ---------------------------------------------------------------------------

#[test]
fn the_capability_register_describes_a_simple_nvm_controller() {
    let d = Driver::new(ramdisk(64), false);
    let cap = d.dev.mmio_read(0, 4) | (d.dev.mmio_read(4, 4) << 32);
    assert_eq!(
        cap & 0xFFFF,
        u64::from(QUEUE_ENTRIES_MAX) - 1,
        "MQES is 0-based"
    );
    assert_ne!(cap & (1 << 16), 0, "queues must be physically contiguous");
    assert_eq!((cap >> 32) & 0xF, 0, "4-byte doorbell stride");
    assert_ne!(cap & (1 << 37), 0, "the NVM command set");
    assert_eq!((cap >> 48) & 0xF, 0, "MPSMIN: 4 KiB");
    assert_eq!((cap >> 52) & 0xF, 0, "MPSMAX: 4 KiB");
    assert_eq!(d.dev.mmio_read(8, 4), 0x0001_0300, "NVMe 1.3");
    // A 32-bit read of the high half of CAP and a byte read of the version work.
    assert_eq!(d.dev.mmio_read(0xA, 1), (0x0001_0300 >> 16) & 0xFF);
    assert!(!d.dev.ready());
}

#[test]
fn enabling_with_admin_queues_sets_ready_and_disabling_clears_it() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    assert_eq!(d.dev.mmio_read(0x1C, 4) & 1, 1);
    d.write_reg(0x14, 4, 0);
    assert!(!d.dev.ready());
    assert_eq!(d.dev.mmio_read(0x1C, 4), 0);
}

#[test]
fn enabling_without_admin_queues_or_with_unsupported_settings_is_fatal() {
    // No admin queue addresses programmed.
    let mut d = Driver::new(ramdisk(64), false);
    d.write_reg(0x14, 4, 1);
    assert!(!d.dev.ready());
    assert_eq!(
        d.dev.mmio_read(0x1C, 4) & 0b10,
        0b10,
        "controller fatal status"
    );

    // 8 KiB pages (CC.MPS = 1) are not offered.
    let mut d = Driver::new(ramdisk(64), false);
    d.write_reg(0x24, 4, u64::from(QD - 1) | (u64::from(QD - 1) << 16));
    d.write_reg(0x28, 8, ASQ);
    d.write_reg(0x30, 8, ACQ);
    d.write_reg(0x14, 4, 1 | (1 << 7));
    assert!(!d.dev.ready());
}

#[test]
fn admin_queue_registers_are_locked_while_the_controller_is_enabled() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.write_reg(0x28, 8, 0xDEAD_0000);
    assert_eq!(
        d.dev.mmio_read(0x28, 4),
        ASQ,
        "ASQ ignores writes once enabled"
    );
}

#[test]
fn shutdown_notification_completes_immediately() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.write_reg(0x14, 4, 1 | (1 << 14) | (6 << 16) | (4 << 20)); // SHN = normal
    assert_eq!(
        (d.dev.mmio_read(0x1C, 4) >> 2) & 0b11,
        0b10,
        "SHST: complete"
    );
}

#[test]
fn doorbells_do_nothing_before_the_controller_is_ready() {
    let mut d = Driver::new(ramdisk(64), false);
    d.write_reg(0x1000, 4, 1); // an SQ0 tail doorbell with no controller
    assert!(!d.dev.ready());
}

// ---------------------------------------------------------------------------
// Identify
// ---------------------------------------------------------------------------

#[test]
fn identify_controller_reports_the_configured_identity() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    let c = d.admin(ADMIN_IDENTIFY, 0, DATA, 0, [1, 0, 0, 0, 0, 0]);
    assert_eq!(c.status, SC_OK);
    let id = d.mem.get(DATA, 4096);
    assert_eq!(u16::from_le_bytes([id[0], id[1]]), 0x144D, "vendor");
    assert_eq!(&id[4..4 + 15], b"ENLIL0000000001");
    assert_eq!(id[4 + 15], b' ', "serial is space padded");
    assert_eq!(&id[24..24 + 18], b"Enlil Virtual NVMe");
    assert_eq!(&id[64..67], b"1.0");
    assert_eq!(id[77], 5, "MDTS");
    assert_eq!(id[512], 0x66, "64-byte submission entries");
    assert_eq!(id[513], 0x44, "16-byte completion entries");
    assert_eq!(
        u32::from_le_bytes([id[516], id[517], id[518], id[519]]),
        1,
        "one namespace"
    );
    assert_eq!(c.sqid, 0);
    assert_eq!(c.sq_head, 1);
}

#[test]
fn identify_namespace_reports_the_disk_size_and_512_byte_sectors() {
    let mut d = Driver::new(ramdisk(2048), false);
    d.enable();
    let c = d.admin(ADMIN_IDENTIFY, 1, DATA, 0, [0, 0, 0, 0, 0, 0]);
    assert_eq!(c.status, SC_OK);
    let ns = d.mem.get(DATA, 4096);
    assert_eq!(
        u64::from_le_bytes(ns[0..8].try_into().unwrap()),
        2048,
        "NSZE"
    );
    assert_eq!(
        u64::from_le_bytes(ns[8..16].try_into().unwrap()),
        2048,
        "NCAP"
    );
    assert_eq!(
        (u32::from_le_bytes(ns[128..132].try_into().unwrap()) >> 16) & 0xFF,
        9,
        "LBADS: 2^9"
    );
    assert_ne!(&ns[104..120], &[0u8; 16], "an NGUID is reported");
    // Any other namespace does not exist.
    let c = d.admin(ADMIN_IDENTIFY, 2, DATA, 0, [0, 0, 0, 0, 0, 0]);
    assert_eq!(c.status, SC_INVALID_NAMESPACE);
}

#[test]
fn the_active_namespace_list_holds_namespace_one() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.admin(ADMIN_IDENTIFY, 0, DATA, 0, [2, 0, 0, 0, 0, 0]);
    let list = d.mem.get(DATA, 8);
    assert_eq!(u32::from_le_bytes(list[0..4].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_le_bytes(list[4..8].try_into().unwrap()),
        0,
        "list ends"
    );
    // Asking for namespaces after #1 yields an empty list.
    d.admin(ADMIN_IDENTIFY, 1, DATA, 0, [2, 0, 0, 0, 0, 0]);
    assert_eq!(d.mem.get(DATA, 4), [0; 4]);
}

#[test]
fn the_namespace_descriptor_list_has_a_stable_nguid() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.admin(ADMIN_IDENTIFY, 1, DATA, 0, [3, 0, 0, 0, 0, 0]);
    let a = d.mem.get(DATA, 20);
    assert_eq!((a[0], a[1]), (2, 16), "NIDT = NGUID, NIDL = 16");
    d.admin(ADMIN_IDENTIFY, 1, DATA + 0x2000, 0, [0, 0, 0, 0, 0, 0]);
    let ns = d.mem.get(DATA + 0x2000, 4096);
    assert_eq!(&a[4..20], &ns[104..120], "same id in both structures");
}

#[test]
fn unknown_identify_structures_are_rejected() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    let c = d.admin(ADMIN_IDENTIFY, 0, DATA, 0, [0x77, 0, 0, 0, 0, 0]);
    assert_eq!(c.status, SC_INVALID_FIELD);
}

// ---------------------------------------------------------------------------
// Queues and features
// ---------------------------------------------------------------------------

#[test]
fn queue_count_negotiation_grants_up_to_what_exists() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    // Ask for 64 of each (0-based 63): granted min(63, 7).
    let c = d.admin(
        ADMIN_SET_FEATURES,
        0,
        0,
        0,
        [FEAT_NUM_QUEUES, 63 | (63 << 16), 0, 0, 0, 0],
    );
    assert_eq!(c.status, SC_OK);
    assert_eq!(c.result, 7 | (7 << 16));
    let g = d.admin(
        ADMIN_GET_FEATURES,
        0,
        0,
        0,
        [FEAT_NUM_QUEUES, 0, 0, 0, 0, 0],
    );
    assert_eq!(g.result, 7 | (7 << 16));
    // Asking for fewer grants exactly that.
    let c = d.admin(
        ADMIN_SET_FEATURES,
        0,
        0,
        0,
        [FEAT_NUM_QUEUES, 1 | (3 << 16), 0, 0, 0, 0],
    );
    assert_eq!(c.result, 1 | (1 << 16));
}

#[test]
fn features_round_trip_and_unknown_ones_are_refused() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    let c = d.admin(
        ADMIN_SET_FEATURES,
        0,
        0,
        0,
        [FEAT_ASYNC_EVENT, 0x1F, 0, 0, 0, 0],
    );
    assert_eq!(c.status, SC_OK);
    let g = d.admin(
        ADMIN_GET_FEATURES,
        0,
        0,
        0,
        [FEAT_ASYNC_EVENT, 0, 0, 0, 0, 0],
    );
    assert_eq!(g.result, 0x1F);
    let c = d.admin(ADMIN_SET_FEATURES, 0, 0, 0, [0x7F, 1, 0, 0, 0, 0]);
    assert_eq!(c.status, SC_INVALID_FIELD);
}

#[test]
fn io_queues_must_be_created_validly() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    let size = u32::from(QD - 1) << 16;
    // A submission queue needs its completion queue to exist first.
    let c = d.admin(
        ADMIN_CREATE_SQ,
        0,
        IOSQ,
        0,
        [1 | size, 1 | (1 << 16), 0, 0, 0, 0],
    );
    assert_eq!(c.status, SC_INVALID_CQ);
    // Queue id 0 is the admin queue; an out-of-range id is refused.
    assert_eq!(
        d.admin(ADMIN_CREATE_CQ, 0, IOCQ, 0, [size, 0b11, 0, 0, 0, 0])
            .status,
        SC_INVALID_QID
    );
    assert_eq!(
        d.admin(ADMIN_CREATE_CQ, 0, IOCQ, 0, [0x63 | size, 0b11, 0, 0, 0, 0])
            .status,
        SC_INVALID_QID
    );
    // A one-entry queue is too small.
    assert_eq!(
        d.admin(ADMIN_CREATE_CQ, 0, IOCQ, 0, [1, 0b11, 0, 0, 0, 0])
            .status,
        SC_INVALID_QSIZE
    );
    // Discontiguous queues and unaligned bases are refused.
    assert_eq!(
        d.admin(ADMIN_CREATE_CQ, 0, IOCQ, 0, [1 | size, 0b10, 0, 0, 0, 0])
            .status,
        SC_INVALID_FIELD
    );
    assert_eq!(
        d.admin(
            ADMIN_CREATE_CQ,
            0,
            IOCQ + 8,
            0,
            [1 | size, 0b11, 0, 0, 0, 0]
        )
        .status,
        SC_INVALID_FIELD
    );
    // Pin-based interrupts have exactly one vector.
    assert_eq!(
        d.admin(
            ADMIN_CREATE_CQ,
            0,
            IOCQ,
            0,
            [1 | size, 0b11 | (1 << 16), 0, 0, 0, 0]
        )
        .status,
        SC_INVALID_VECTOR
    );
    // A valid pair, then a duplicate id is refused.
    d.create_io_queues();
    assert_eq!(
        d.admin(
            ADMIN_CREATE_CQ,
            0,
            IOCQ + 0x1000,
            0,
            [1 | size, 0b11, 0, 0, 0, 0]
        )
        .status,
        SC_INVALID_QID
    );
}

#[test]
fn queues_can_be_deleted_but_not_while_in_use() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.create_io_queues();
    // The completion queue still serves the submission queue.
    assert_eq!(
        d.admin(ADMIN_DELETE_CQ, 0, 0, 0, [1, 0, 0, 0, 0, 0]).status,
        SC_INVALID_DELETE
    );
    assert_eq!(
        d.admin(ADMIN_DELETE_SQ, 0, 0, 0, [1, 0, 0, 0, 0, 0]).status,
        SC_OK
    );
    assert_eq!(
        d.admin(ADMIN_DELETE_CQ, 0, 0, 0, [1, 0, 0, 0, 0, 0]).status,
        SC_OK
    );
    assert_eq!(
        d.admin(ADMIN_DELETE_SQ, 0, 0, 0, [1, 0, 0, 0, 0, 0]).status,
        SC_INVALID_QID
    );
}

#[test]
fn unknown_admin_opcodes_fail_cleanly_and_abort_reports_not_aborted() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    assert_eq!(d.admin(0x7E, 0, 0, 0, [0; 6]).status, SC_INVALID_OPCODE);
    let c = d.admin(ADMIN_ABORT, 0, 0, 0, [0; 6]);
    assert_eq!((c.status, c.result), (SC_OK, 1));
}

#[test]
fn asynchronous_event_requests_stay_outstanding_up_to_the_limit() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    for _ in 0..AER_LIMIT {
        let cid = d.cid();
        d.submit(0, &Driver::sqe(ADMIN_ASYNC_EVENT, cid, 0, 0, 0, [0; 6]));
        assert!(d.reap(0, cid).is_none(), "an event request never completes");
    }
    // One more than the controller allows is refused.
    let c = d.admin(ADMIN_ASYNC_EVENT, 0, 0, 0, [0; 6]);
    assert_eq!(c.status, SC_ABORT_LIMIT);
    // And the admin queue still works behind the parked requests.
    assert_eq!(
        d.admin(ADMIN_IDENTIFY, 0, DATA, 0, [1, 0, 0, 0, 0, 0])
            .status,
        SC_OK
    );
}

#[test]
fn the_smart_log_page_reports_a_healthy_device() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    // LID 2, 512 bytes (NUMD = 127).
    let c = d.admin(
        ADMIN_GET_LOG_PAGE,
        0xFFFF_FFFF,
        DATA,
        0,
        [0x02 | (127 << 16), 0, 0, 0, 0, 0],
    );
    assert_eq!(c.status, SC_OK);
    let log = d.mem.get(DATA, 512);
    assert_eq!(log[3], 100, "available spare");
    // Oversized log requests are refused.
    let c = d.admin(
        ADMIN_GET_LOG_PAGE,
        0,
        DATA,
        0,
        [0x02 | (0xFFF << 16), 0xFFFF, 0, 0, 0, 0],
    );
    assert_eq!(c.status, SC_INVALID_FIELD);
}

// ---------------------------------------------------------------------------
// I/O
// ---------------------------------------------------------------------------

#[test]
fn a_single_page_read_returns_the_disk_contents() {
    let mut d = Driver::new(ramdisk(256), false);
    d.enable();
    d.create_io_queues();
    let c = d.io(IO_READ, 3, 1, DATA, 0);
    assert_eq!((c.status, c.sqid), (SC_OK, 1));
    assert_eq!(d.mem.get(DATA, SECTOR), pattern(3, 1));
}

#[test]
fn a_two_page_read_uses_prp2_as_the_second_page() {
    let mut d = Driver::new(ramdisk(256), false);
    d.enable();
    d.create_io_queues();
    // 16 sectors = 8 KiB: PRP1 and PRP2 are each a page.
    let c = d.io(IO_READ, 10, 16, DATA, DATA + 0x5000);
    assert_eq!(c.status, SC_OK);
    let want = pattern(10, 16);
    assert_eq!(d.mem.get(DATA, 4096), &want[..4096]);
    assert_eq!(d.mem.get(DATA + 0x5000, 4096), &want[4096..]);
}

#[test]
fn a_large_read_walks_a_prp_list_and_follows_a_chained_list() {
    let mut d = Driver::new(ramdisk(2048), false);
    d.enable();
    d.create_io_queues();
    // 128 KiB = 32 pages: 1 in PRP1, 31 in the list. Scatter them non-contiguously.
    let pages: Vec<u64> = (0..32).map(|i| DATA + (31 - i) * 0x2000).collect();
    for (i, p) in pages[1..].iter().enumerate() {
        d.mem.put(LIST + i as u64 * 8, &p.to_le_bytes());
    }
    let c = d.io(IO_READ, 100, 256, pages[0], LIST);
    assert_eq!(c.status, SC_OK);
    let want = pattern(100, 256);
    for (i, p) in pages.iter().enumerate() {
        assert_eq!(
            d.mem.get(*p, 4096),
            &want[i * 4096..(i + 1) * 4096],
            "page {i}"
        );
    }

    // A list that starts near the end of its page chains to the next list page.
    let list = LIST + 0x1000 - 16; // two entries fit, the last slot is the chain
    let next = LIST + 0x3000;
    d.mem.put(list, &pages[1].to_le_bytes());
    d.mem.put(list + 8, &next.to_le_bytes());
    for (i, p) in pages[2..].iter().enumerate() {
        d.mem.put(next + i as u64 * 8, &p.to_le_bytes());
    }
    let c = d.io(IO_READ, 100, 256, pages[0], list);
    assert_eq!(c.status, SC_OK);
    // The first page, the one before the chain slot, then the chained pages.
    assert_eq!(d.mem.get(pages[1], 4096), &want[4096..8192]);
    assert_eq!(d.mem.get(pages[2], 4096), &want[8192..12288]);
    assert_eq!(d.mem.get(pages[31], 4096), &want[31 * 4096..32 * 4096]);
}

#[test]
fn an_unaligned_prp1_splits_the_first_segment_at_the_page_boundary() {
    let mut d = Driver::new(ramdisk(256), false);
    d.enable();
    d.create_io_queues();
    // 1 sector starting 256 bytes before a page end: 256 bytes, then PRP2.
    let c = d.io(IO_READ, 0, 1, DATA + 0xF00, DATA + 0x4000);
    assert_eq!(c.status, SC_OK);
    let want = pattern(0, 1);
    assert_eq!(d.mem.get(DATA + 0xF00, 256), &want[..256]);
    assert_eq!(d.mem.get(DATA + 0x4000, 256), &want[256..]);
}

#[test]
fn a_write_reaches_the_disk_and_reads_back() {
    let mut d = Driver::new(ramdisk(256), false);
    d.enable();
    d.create_io_queues();
    // 16 sectors = 8 KiB: two pages, PRP1 and PRP2.
    let data: Vec<u8> = (0..16 * SECTOR).map(|i| (i % 251) as u8).collect();
    d.mem.put(DATA, &data[..4096]);
    d.mem.put(DATA + 0x3000, &data[4096..]);
    let c = d.io(IO_WRITE, 40, 16, DATA, DATA + 0x3000);
    assert_eq!(c.status, SC_OK);
    // Read it back into different buffers.
    let c = d.io(IO_READ, 40, 16, DATA + 0x10000, DATA + 0x11000);
    assert_eq!(c.status, SC_OK);
    assert_eq!(d.mem.get(DATA + 0x10000, 4096), &data[..4096]);
    assert_eq!(d.mem.get(DATA + 0x11000, 4096), &data[4096..]);
}

#[test]
fn transfers_past_the_end_of_the_disk_fail_without_touching_it() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.create_io_queues();
    assert_eq!(
        d.io(IO_READ, 63, 2, DATA, DATA + 0x1000).status,
        SC_LBA_OUT_OF_RANGE
    );
    assert_eq!(d.io(IO_READ, 64, 1, DATA, 0).status, SC_LBA_OUT_OF_RANGE);
    assert_eq!(
        d.io(IO_WRITE, u64::MAX - 1, 4, DATA, DATA + 0x1000).status,
        SC_LBA_OUT_OF_RANGE
    );
    assert_eq!(
        d.io(IO_READ, 63, 1, DATA, 0).status,
        SC_OK,
        "the last sector is fine"
    );
}

#[test]
fn malformed_prps_and_oversized_transfers_are_rejected() {
    let mut d = Driver::new(ramdisk(2048), false);
    d.enable();
    d.create_io_queues();
    // Two pages but no PRP2.
    assert_eq!(d.io(IO_READ, 0, 16, DATA, 0).status, SC_INVALID_FIELD);
    // PRP2 as a data page must be page-aligned.
    assert_eq!(
        d.io(IO_READ, 0, 16, DATA, DATA + 0x2010).status,
        SC_INVALID_FIELD
    );
    // A null PRP1.
    assert_eq!(d.io(IO_READ, 0, 1, 0, 0).status, SC_INVALID_FIELD);
    // Larger than MDTS (128 KiB): 257 sectors.
    assert_eq!(d.io(IO_READ, 0, 257, DATA, LIST).status, SC_INVALID_FIELD);
    // A PRP list with a null entry.
    d.mem.put(LIST, &(DATA + 0x2000).to_le_bytes());
    assert_eq!(d.io(IO_READ, 0, 24, DATA, LIST).status, SC_INVALID_FIELD);
    // A data page that lies outside guest memory fails the transfer, not the host.
    let status = d.io(IO_READ, 0, 1, 64 << 20, 0).status;
    assert_eq!(status, SC_DATA_TRANSFER_ERROR);
}

#[test]
fn flush_dataset_management_and_unknown_opcodes() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.create_io_queues();
    let cid = d.cid();
    let c = d.run(1, &Driver::sqe(IO_FLUSH, cid, 1, 0, 0, [0; 6]));
    assert_eq!(c.status, SC_OK);
    let cid = d.cid();
    let c = d.run(
        1,
        &Driver::sqe(IO_DATASET_MGMT, cid, 1, DATA, 0, [0, 1 << 2, 0, 0, 0, 0]),
    );
    assert_eq!(c.status, SC_OK);
    let cid = d.cid();
    let c = d.run(1, &Driver::sqe(0x55, cid, 1, 0, 0, [0; 6]));
    assert_eq!(c.status, SC_INVALID_OPCODE);
    // A command for a namespace that does not exist.
    let cid = d.cid();
    let c = d.run(1, &Driver::sqe(IO_FLUSH, cid, 7, 0, 0, [0; 6]));
    assert_eq!(c.status, SC_INVALID_NAMESPACE);
}

#[test]
fn write_zeroes_clears_the_requested_range_only() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.create_io_queues();
    let cid = d.cid();
    let c = d.run(
        1,
        &Driver::sqe(IO_WRITE_ZEROES, cid, 1, 0, 0, [4, 0, 3, 0, 0, 0]),
    );
    assert_eq!(c.status, SC_OK);
    d.io(IO_READ, 3, 6, DATA, DATA + 0x1000);
    let got = d.mem.get(DATA, 6 * SECTOR.min(4096 / 6 * 6));
    // Sector 3 is original, sectors 4..=7 are zero, sector 8 is original.
    assert_eq!(&got[..SECTOR], &pattern(3, 1)[..]);
    assert!(got[SECTOR..5 * SECTOR].iter().all(|&b| b == 0));
    let tail = d.mem.get(DATA + 5 * SECTOR as u64, SECTOR);
    assert_eq!(tail, pattern(8, 1));
}

// ---------------------------------------------------------------------------
// The allow-list is enforced through the controller
// ---------------------------------------------------------------------------

fn confined(sectors: usize) -> Box<dyn BlockBackend> {
    let mut raw = RamDisk::new(sectors);
    for (i, b) in raw.bytes_mut().iter_mut().enumerate() {
        *b = (i / SECTOR) as u8 + 1;
    }
    Box::new(DiskMap::new(
        raw,
        vec![
            Allow {
                start: 0,
                end: 7,
                writable: false,
            },
            Allow {
                start: 8,
                end: 15,
                writable: true,
            },
        ],
    ))
}

#[test]
fn a_denied_read_returns_zeros_and_a_denied_write_is_refused() {
    let mut d = Driver::new(confined(64), false);
    d.enable();
    d.create_io_queues();
    // Read-only range: readable.
    assert_eq!(d.io(IO_READ, 2, 1, DATA, 0).status, SC_OK);
    assert_eq!(d.mem.get(DATA, 4), [3; 4]);
    // Outside the allow-list: reads as zeros.
    d.mem.put(DATA, &[0xFF; 512]);
    assert_eq!(d.io(IO_READ, 40, 1, DATA, 0).status, SC_OK);
    assert!(d.mem.get(DATA, 512).iter().all(|&b| b == 0));
    // Writing the read-only range or a denied sector fails.
    d.mem.put(DATA, &[0xAB; 512]);
    assert_eq!(d.io(IO_WRITE, 2, 1, DATA, 0).status, SC_WRITE_DENIED);
    assert_eq!(d.io(IO_WRITE, 40, 1, DATA, 0).status, SC_WRITE_DENIED);
    // The writable range works.
    assert_eq!(d.io(IO_WRITE, 9, 1, DATA, 0).status, SC_OK);
    // Write Zeroes obeys the same policy, and a multi-sector write straddling
    // the boundary is refused whole.
    let cid = d.cid();
    let c = d.run(
        1,
        &Driver::sqe(IO_WRITE_ZEROES, cid, 1, 0, 0, [2, 0, 0, 0, 0, 0]),
    );
    assert_eq!(c.status, SC_WRITE_DENIED);
    assert_eq!(
        d.io(IO_WRITE, 14, 4, DATA, DATA + 0x1000).status,
        SC_WRITE_DENIED
    );
    d.io(IO_READ, 14, 2, DATA + 0x4000, 0);
    assert_eq!(
        d.mem.get(DATA + 0x4000, 4),
        [15; 4],
        "sector 14 untouched by the refused write"
    );
}

#[test]
fn a_read_only_controller_refuses_every_write() {
    let mut d = Driver::new(ramdisk(64), true);
    d.enable();
    d.create_io_queues();
    assert_eq!(d.io(IO_WRITE, 1, 1, DATA, 0).status, SC_WRITE_DENIED);
    let cid = d.cid();
    let c = d.run(
        1,
        &Driver::sqe(IO_WRITE_ZEROES, cid, 1, 0, 0, [1, 0, 0, 0, 0, 0]),
    );
    assert_eq!(c.status, SC_WRITE_DENIED);
    assert_eq!(d.io(IO_READ, 1, 1, DATA, 0).status, SC_OK);
}

// ---------------------------------------------------------------------------
// Completions, phase, interrupts, backpressure
// ---------------------------------------------------------------------------

#[test]
fn completions_carry_the_submission_head_and_the_phase_flips_on_wrap() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.create_io_queues();
    // Run more commands than the queue holds so both the SQ and CQ wrap twice.
    for i in 0..(3 * u64::from(QD)) {
        let c = d.io(IO_READ, i % 32, 1, DATA, 0);
        assert_eq!(c.status, SC_OK, "command {i}");
        assert_eq!(u64::from(c.sq_head), (i + 1) % u64::from(QD));
    }
}

#[test]
fn the_interrupt_line_is_level_triggered_on_unconsumed_completions() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    assert!(!d.dev.irq_line(), "nothing completed yet");
    d.create_io_queues();
    // Submit without reaping: the completion is posted and the line rises.
    let cid = d.cid();
    d.submit(
        1,
        &Driver::sqe(IO_READ, cid, 1, DATA, 0, [0, 0, 0, 0, 0, 0]),
    );
    assert!(d.dev.irq_line());
    // Masking it through INTMS drops the line; INTMC restores it.
    d.write_reg(0x0C, 4, 1);
    assert!(!d.dev.irq_line());
    d.write_reg(0x10, 4, 1);
    assert!(d.dev.irq_line());
    // Consuming the completion (moving the head doorbell) drops it.
    d.reap(1, cid).unwrap();
    assert!(!d.dev.irq_line());
}

#[test]
fn a_queue_without_interrupts_enabled_never_raises_the_line() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    let size = u32::from(QD - 1) << 16;
    // IEN clear.
    d.admin(ADMIN_CREATE_CQ, 0, IOCQ, 0, [1 | size, 0b01, 0, 0, 0, 0]);
    d.admin(
        ADMIN_CREATE_SQ,
        0,
        IOSQ,
        0,
        [1 | size, 0b1 | (1 << 16), 0, 0, 0, 0],
    );
    let cid = d.cid();
    d.submit(1, &Driver::sqe(IO_READ, cid, 1, DATA, 0, [0; 6]));
    assert!(!d.dev.irq_line());
    assert!(d.reap(1, cid).is_some(), "the completion is still posted");
}

#[test]
fn a_full_completion_queue_stalls_the_submission_queue_until_it_is_drained() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    d.create_io_queues();
    // Fill the CQ: a queue of QD entries holds QD - 1 completions.
    let mut cids = Vec::new();
    for _ in 0..(QD - 1) {
        let cid = d.cid();
        d.submit(1, &Driver::sqe(IO_READ, cid, 1, DATA, 0, [0; 6]));
        cids.push(cid);
    }
    // One more command cannot complete: the CQ is full.
    let stalled = d.cid();
    d.submit(1, &Driver::sqe(IO_READ, stalled, 1, DATA, 0, [0; 6]));
    // The guest drains the queue; the stalled command then completes.
    let first = d.reap(1, cids[0]).unwrap();
    assert_eq!(first.status, SC_OK);
    for cid in &cids[1..] {
        assert!(d.reap(1, *cid).is_some());
    }
    let last = d
        .reap(1, stalled)
        .expect("the stalled command completed after the drain");
    assert_eq!(last.status, SC_OK);
}

#[test]
fn a_command_in_unreadable_guest_memory_completes_with_an_error() {
    let mut d = Driver::new(ramdisk(64), false);
    d.enable();
    // Point the admin queue's tail doorbell at a slot, but put the queue where
    // the guest memory ends: the entry cannot be read, so it completes in error
    // rather than the device touching anything outside guest RAM.
    d.write_reg(0x14, 4, 0);
    let aqa = u64::from(QD - 1) | (u64::from(QD - 1) << 16);
    d.write_reg(0x24, 4, aqa);
    d.write_reg(0x28, 8, (8u64 << 20) + 0x1000); // beyond the 8 MiB of RAM
    d.write_reg(0x30, 8, ACQ);
    d.write_reg(0x14, 4, 1 | (6 << 16) | (4 << 20));
    d.write_reg(0x1000, 4, 1);
    let raw = d.mem.get(ACQ, CQE_BYTES);
    let status = u16::from_le_bytes([raw[14], raw[15]]);
    assert_eq!(status & 1, 1, "a completion was posted");
    assert_ne!(status >> 1, 0, "with an error status");
}

#[test]
fn disabling_resets_queues_and_the_guest_can_start_over() {
    let mut d = Driver::new(ramdisk(256), false);
    d.enable();
    d.create_io_queues();
    assert_eq!(d.io(IO_READ, 1, 1, DATA, 0).status, SC_OK);
    // Reset: queue state is gone; a fresh driver instance can reconfigure.
    d.write_reg(0x14, 4, 0);
    d.sq_tail = [0; 3];
    d.cq_head = [0; 3];
    d.cq_phase = [true; 3];
    d.mem = Mem::new(8 << 20);
    d.enable();
    d.create_io_queues();
    assert_eq!(d.io(IO_READ, 2, 1, DATA, 0).status, SC_OK);
    assert_eq!(d.mem.get(DATA, SECTOR), pattern(2, 1));
}

#[test]
fn the_pci_function_is_an_nvme_controller_with_a_16k_bar() {
    let d = Driver::new(ramdisk(64), false);
    assert_eq!(
        d.dev.pci.read(0x00, 4),
        0xA80C_144D,
        "device << 16 | vendor"
    );
    // Class 01:08:02, revision 1.
    assert_eq!(d.dev.pci.read(0x08, 4), 0x0108_0201);
    assert_eq!(d.dev.pci.read(0x3C, 1), 11, "interrupt line");
    assert_eq!(d.dev.pci.read(0x3D, 1), 1, "INTA#");
    // BAR0 sizes to 16 KiB (a 64-bit memory BAR).
    let mut pci = d.dev.pci;
    pci.write(0x10, 4, 0xFFFF_FFFF);
    pci.write(0x14, 4, 0xFFFF_FFFF);
    assert_eq!(pci.read(0x10, 4) & 0xFFFF_FFF0, 0xFFFF_C000);
    assert_eq!(pci.read(0x10, 4) & 0xF, 0x4);
}

#[test]
fn a_custom_identity_is_what_the_guest_sees() {
    let id = NvmeIdentity {
        vendor: 0x144D,
        device: 0xA80C,
        serial: b"S7DSNJ0X123456A".to_vec(),
        model: b"Samsung SSD 990 PRO 4TB".to_vec(),
        firmware: b"4B2QJXD7".to_vec(),
    };
    let mut d = Driver::new(ramdisk(64), false);
    d.dev = NvmeDevice::new(ramdisk(64), id, 11, false);
    d.enable();
    d.admin(ADMIN_IDENTIFY, 0, DATA, 0, [1, 0, 0, 0, 0, 0]);
    let idc = d.mem.get(DATA, 4096);
    assert_eq!(&idc[4..19], b"S7DSNJ0X123456A");
    assert_eq!(&idc[24..47], b"Samsung SSD 990 PRO 4TB");
    assert_eq!(&idc[64..72], b"4B2QJXD7");
}

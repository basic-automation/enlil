//! An `NVMe` controller as a guest sees it, backed by a [`BlockBackend`].
//!
//! Windows has no inbox virtio driver, but it ships `stornvme`, and an `NVMe`
//! drive is what its boot disk already is. So this is the disk model for a
//! guest that must run an *unmodified* existing install: a controller with the
//! register set, queue pairs, admin and I/O command sets and PRP data transfer
//! that `stornvme` (and UEFI's `NvmExpressDxe`, which boots Windows' boot
//! manager from the ESP) expect, over the same allow-listed [`BlockBackend`] the
//! virtio disk uses. The guest sees the host disk's own LBA space, so its real
//! partition table and bootloaders work; sectors outside its allow-list read as
//! zeros and refuse writes.
//!
//! The controller is deliberately small: one namespace, 512-byte sectors,
//! 4 KiB memory pages, physically contiguous queues, pin-based interrupts, PRP
//! (no SGL) transfers, no optional admin commands. Everything a guest chooses
//! is bounds-checked: queue sizes, PRP lists, transfer lengths, and every access
//! to guest memory goes through [`GuestMemory`].
//!
//! Commands run synchronously when the guest rings a doorbell: the device walks
//! the submission queue, executes each entry against the backing disk, posts the
//! completions and updates its interrupt line before the doorbell write returns.
//!
//! Pure: the host tests below play the part of a guest driver.

// The NVMe wire format is fixed-width (u16 queue ids and entry counts, u32
// dwords) while the device works in `usize`/`u64`. Every guest-chosen value is
// bounded by an explicit check before it is narrowed.
#![allow(clippy::cast_possible_truncation)]

extern crate alloc;

use crate::blockdev::{BlockBackend, BlockError, SECTOR};
use crate::pci_dev::PciFunction;
use crate::virtio_blk::GuestMemory;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Size of `BAR0`: the controller registers plus the doorbells.
pub const BAR0_SIZE: u64 = 0x4000;
/// Largest queue (in entries) the controller supports.
pub const QUEUE_ENTRIES_MAX: u32 = 256;
/// I/O queue pairs offered (admin queue excluded).
pub const IO_QUEUES_MAX: usize = 8;
/// Largest data transfer per command, in bytes (`MDTS` = 5: 2^5 pages).
pub const MAX_TRANSFER_BYTES: usize = 32 * PAGE;
/// Memory page size the controller uses (`CC.MPS` must be 0).
const PAGE: usize = 4096;
/// Bytes in a submission queue entry.
const SQE_BYTES: usize = 64;
/// Bytes in a completion queue entry.
const CQE_BYTES: usize = 16;
/// The one namespace id.
const NSID: u32 = 1;
/// Asynchronous event requests held pending (`AERL` + 1).
const AER_LIMIT: usize = 4;

// Controller registers.
const REG_CAP: u64 = 0x00;
const REG_VS: u64 = 0x08;
const REG_INTMS: u64 = 0x0C;
const REG_INTMC: u64 = 0x10;
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1C;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;
/// First doorbell: submission queue 0's tail.
const DOORBELL_BASE: u64 = 0x1000;

/// `CAP`: `MQES` (0-based max entries) | `CQR` | `TO` (500 ms units) | `CSS`
/// bit 0 (the NVM command set). `DSTRD` = 0 (4-byte doorbell stride), `MPSMIN`
/// = `MPSMAX` = 0 (4 KiB pages only).
const CAP_VALUE: u64 = (QUEUE_ENTRIES_MAX as u64 - 1) | (1 << 16) | (0x0F << 24) | (1 << 37);
/// `VS`: `NVMe` 1.3.
const VS_VALUE: u32 = 0x0001_0300;

// `CC` fields.
const CC_EN: u32 = 1;
const CC_SHN_SHIFT: u32 = 14;
const CC_MPS_MASK: u32 = 0xF << 7;
const CC_CSS_MASK: u32 = 0x7 << 4;

// `CSTS` bits.
const CSTS_RDY: u32 = 1;
/// `CSTS.SHST` = shutdown processing complete.
const CSTS_SHST_DONE: u32 = 0b10 << 2;

// Admin opcodes.
const ADMIN_DELETE_SQ: u8 = 0x00;
const ADMIN_CREATE_SQ: u8 = 0x01;
const ADMIN_GET_LOG_PAGE: u8 = 0x02;
const ADMIN_DELETE_CQ: u8 = 0x04;
const ADMIN_CREATE_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const ADMIN_ABORT: u8 = 0x08;
const ADMIN_SET_FEATURES: u8 = 0x09;
const ADMIN_GET_FEATURES: u8 = 0x0A;
const ADMIN_ASYNC_EVENT: u8 = 0x0C;

// NVM opcodes.
const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;
const IO_WRITE_ZEROES: u8 = 0x08;
const IO_DATASET_MGMT: u8 = 0x09;

// Feature ids.
const FEAT_ARBITRATION: u32 = 0x01;
const FEAT_POWER: u32 = 0x02;
const FEAT_TEMP_THRESHOLD: u32 = 0x04;
const FEAT_ERROR_RECOVERY: u32 = 0x05;
const FEAT_VOLATILE_CACHE: u32 = 0x06;
const FEAT_NUM_QUEUES: u32 = 0x07;
const FEAT_INT_COALESCING: u32 = 0x08;
const FEAT_INT_VECTOR: u32 = 0x09;
const FEAT_WRITE_ATOMICITY: u32 = 0x0A;
const FEAT_ASYNC_EVENT: u32 = 0x0B;

// Status codes (SCT << 8 | SC).
const SC_OK: u16 = 0x0000;
const SC_INVALID_OPCODE: u16 = 0x0001;
const SC_INVALID_FIELD: u16 = 0x0002;
const SC_DATA_TRANSFER_ERROR: u16 = 0x0004;
const SC_INTERNAL_ERROR: u16 = 0x0006;
const SC_INVALID_NAMESPACE: u16 = 0x000B;
const SC_LBA_OUT_OF_RANGE: u16 = 0x0080;
/// Command-specific: invalid completion queue id.
const SC_INVALID_CQ: u16 = 0x0100;
/// Command-specific: invalid queue id.
const SC_INVALID_QID: u16 = 0x0101;
/// Command-specific: invalid queue size.
const SC_INVALID_QSIZE: u16 = 0x0102;
/// Command-specific: abort command limit exceeded / command not aborted.
const SC_ABORT_LIMIT: u16 = 0x0103;
/// Command-specific: invalid interrupt vector.
const SC_INVALID_VECTOR: u16 = 0x0108;
/// Command-specific: invalid queue deletion.
const SC_INVALID_DELETE: u16 = 0x010C;
/// Command-specific: write attempted to a read-only range.
const SC_WRITE_DENIED: u16 = 0x0182;

/// The identity the controller presents.
///
/// What `stornvme` and disk-information tools show. For a guest that runs an
/// existing install this is copied from the real drive, so the guest believes
/// it owns that drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmeIdentity {
    /// PCI vendor id.
    pub vendor: u16,
    /// PCI device id.
    pub device: u16,
    /// Serial number, ASCII (at most 20 bytes; space padded on the wire).
    pub serial: Vec<u8>,
    /// Model number, ASCII (at most 40 bytes).
    pub model: Vec<u8>,
    /// Firmware revision, ASCII (at most 8 bytes).
    pub firmware: Vec<u8>,
}

impl Default for NvmeIdentity {
    fn default() -> Self {
        Self {
            vendor: 0x144D,
            device: 0xA80C,
            serial: b"ENLIL0000000001".to_vec(),
            model: b"Enlil Virtual NVMe".to_vec(),
            firmware: b"1.0".to_vec(),
        }
    }
}

/// A submission queue the guest created.
#[derive(Debug, Clone, Copy, Default)]
struct Sq {
    base: u64,
    /// Entries.
    size: u16,
    /// Completion queue it posts to.
    cqid: u16,
    head: u16,
    tail: u16,
}

/// A completion queue the guest created.
#[derive(Debug, Clone, Copy, Default)]
struct Cq {
    base: u64,
    size: u16,
    head: u16,
    tail: u16,
    /// The phase tag the next entry is posted with.
    phase: bool,
    /// Interrupts enabled for this queue.
    ien: bool,
}

/// The controller's enable-time and runtime state.
#[derive(Debug, Clone, Copy, Default)]
struct Regs {
    cc: u32,
    csts: u32,
    aqa: u32,
    asq: u64,
    acq: u64,
    /// Pin-based interrupt mask (`INTMS`/`INTMC`).
    intm: u32,
}

/// An `NVMe` controller over a [`BlockBackend`].
pub struct NvmeDevice {
    backend: Box<dyn BlockBackend>,
    identity: NvmeIdentity,
    /// The function's PCI configuration space.
    pub pci: PciFunction,
    regs: Regs,
    /// Queue 0 is the admin queue.
    sqs: [Option<Sq>; IO_QUEUES_MAX + 1],
    cqs: [Option<Cq>; IO_QUEUES_MAX + 1],
    /// Feature values the guest set (and defaults).
    features: [(u32, u32); 10],
    /// Number of I/O queues the guest was granted (0-based).
    granted_queues: u16,
    /// Command ids of the pending asynchronous event requests, never completed:
    /// this controller raises no events.
    aer_pending: Vec<u16>,
    read_only: bool,
}

/// Pack a status code (`SCT << 8 | SC`) and the do-not-retry bit into the
/// completion's status field (without the phase bit).
const fn status_field(code: u16, dnr: bool) -> u16 {
    let sc = code & 0xFF;
    let sct = (code >> 8) & 0x7;
    (sc << 1) | (sct << 9) | if dnr { 1 << 15 } else { 0 }
}

/// The 4-byte-aligned little-endian 64-bit value from two registers' halves.
const fn join(lo: u32, hi: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn rd64(m: &dyn GuestMemory, gpa: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    m.read(gpa, &mut b).then(|| u64::from_le_bytes(b))
}

/// An `NVMe` command (a 64-byte submission queue entry), decoded.
#[derive(Debug, Clone, Copy)]
struct Command {
    opcode: u8,
    cid: u16,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
}

impl Command {
    fn parse(raw: &[u8; SQE_BYTES]) -> Self {
        let dw = |i: usize| {
            u32::from_le_bytes([raw[i * 4], raw[i * 4 + 1], raw[i * 4 + 2], raw[i * 4 + 3]])
        };
        Self {
            opcode: raw[0],
            cid: u16::from_le_bytes([raw[2], raw[3]]),
            nsid: dw(1),
            prp1: join(dw(6), dw(7)),
            prp2: join(dw(8), dw(9)),
            cdw10: dw(10),
            cdw11: dw(11),
            cdw12: dw(12),
        }
    }
}

/// What executing a command produced.
#[derive(Debug, Clone, Copy)]
struct Outcome {
    status: u16,
    /// Completion dword 0.
    result: u32,
}

impl Outcome {
    const OK: Self = Self {
        status: SC_OK,
        result: 0,
    };

    const fn err(status: u16) -> Self {
        Self { status, result: 0 }
    }
}

impl NvmeDevice {
    /// A controller presenting `identity` over `backend`. `irq` is the legacy
    /// interrupt line firmware and the OS see in config space; `read_only`
    /// marks a disk whose every range is read-only (writes then fail early).
    #[must_use]
    pub fn new(
        backend: Box<dyn BlockBackend>,
        identity: NvmeIdentity,
        irq: u8,
        read_only: bool,
    ) -> Self {
        // Mass storage / NVM / NVMe programming interface (01:08:02).
        let mut pci = PciFunction::new(
            identity.vendor,
            identity.device,
            [0x01, 0x08, 0x02],
            1,
            (identity.vendor, 0x0001),
        );
        pci.set_bar0_mem64(BAR0_SIZE);
        pci.set_interrupt(irq, 1);
        Self {
            backend,
            identity,
            pci,
            regs: Regs::default(),
            sqs: [None; IO_QUEUES_MAX + 1],
            cqs: [None; IO_QUEUES_MAX + 1],
            features: [
                (FEAT_ARBITRATION, 0),
                (FEAT_POWER, 0),
                (FEAT_TEMP_THRESHOLD, 0x0157),
                (FEAT_ERROR_RECOVERY, 0),
                (FEAT_VOLATILE_CACHE, 0),
                (FEAT_NUM_QUEUES, 0),
                (FEAT_INT_COALESCING, 0),
                (FEAT_INT_VECTOR, 0),
                (FEAT_WRITE_ATOMICITY, 0),
                (FEAT_ASYNC_EVENT, 0),
            ],
            granted_queues: 0,
            aer_pending: Vec::new(),
            read_only,
        }
    }

    /// Whether the controller is enabled and ready.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.regs.csts & CSTS_RDY != 0
    }

    /// The level of the legacy interrupt line: asserted while some completion
    /// queue with interrupts enabled holds entries the guest has not consumed,
    /// unless the guest masked it through `INTMS`.
    #[must_use]
    pub fn irq_line(&self) -> bool {
        self.regs.intm & 1 == 0
            && self
                .cqs
                .iter()
                .flatten()
                .any(|cq| cq.ien && cq.head != cq.tail)
    }

    /// Whether the guest may `BAR0`-decode right now (memory space enabled).
    #[must_use]
    pub const fn mem_enabled(&self) -> bool {
        self.pci.mem_enabled()
    }

    // ---- registers --------------------------------------------------------

    /// A `size`-byte register read at `offset` in `BAR0`.
    #[must_use]
    pub fn mmio_read(&self, offset: u64, size: u8) -> u64 {
        let aligned = offset & !3;
        let dword = |reg: u64| -> u64 {
            match reg {
                REG_CAP => CAP_VALUE & 0xFFFF_FFFF,
                r if r == REG_CAP + 4 => CAP_VALUE >> 32,
                REG_VS => u64::from(VS_VALUE),
                REG_INTMS | REG_INTMC => u64::from(self.regs.intm),
                REG_CC => u64::from(self.regs.cc),
                REG_CSTS => u64::from(self.regs.csts),
                REG_AQA => u64::from(self.regs.aqa),
                REG_ASQ => self.regs.asq & 0xFFFF_FFFF,
                r if r == REG_ASQ + 4 => self.regs.asq >> 32,
                REG_ACQ => self.regs.acq & 0xFFFF_FFFF,
                r if r == REG_ACQ + 4 => self.regs.acq >> 32,
                _ => 0,
            }
        };
        let v = dword(aligned) >> (8 * (offset & 3));
        crate::insn::mask_to(v, size)
    }

    /// A `size`-byte register write of `val` at `offset` in `BAR0`; doorbell
    /// writes run the queues against guest memory `mem`.
    pub fn mmio_write(&mut self, offset: u64, size: u8, val: u64, mem: &dyn GuestMemory) {
        if size != 4 && size != 8 {
            return;
        }
        if size == 8 {
            // A 64-bit write is two dword writes, low then high.
            self.mmio_write(offset, 4, val & 0xFFFF_FFFF, mem);
            self.mmio_write(offset + 4, 4, val >> 32, mem);
            return;
        }
        let v = val as u32;
        match offset {
            REG_INTMS => self.regs.intm |= v,
            REG_INTMC => self.regs.intm &= !v,
            REG_CC => self.write_cc(v),
            REG_AQA if !self.ready() => self.regs.aqa = v,
            REG_ASQ if !self.ready() => {
                self.regs.asq = (self.regs.asq & !0xFFFF_FFFF) | u64::from(v & !0xFFF);
            }
            o if o == REG_ASQ + 4 && !self.ready() => {
                self.regs.asq = (self.regs.asq & 0xFFFF_FFFF) | (u64::from(v) << 32);
            }
            REG_ACQ if !self.ready() => {
                self.regs.acq = (self.regs.acq & !0xFFFF_FFFF) | u64::from(v & !0xFFF);
            }
            o if o == REG_ACQ + 4 && !self.ready() => {
                self.regs.acq = (self.regs.acq & 0xFFFF_FFFF) | (u64::from(v) << 32);
            }
            o if o >= DOORBELL_BASE => self.doorbell(o, v, mem),
            _ => {}
        }
    }

    fn write_cc(&mut self, v: u32) {
        let was_enabled = self.regs.cc & CC_EN != 0;
        self.regs.cc = v;
        let enable = v & CC_EN != 0;
        if enable && !was_enabled {
            // Refuse a configuration the controller cannot honour: a page size
            // other than 4 KiB, or a command set other than NVM.
            let aqs = (self.regs.aqa & 0xFFF) + 1;
            let acs = ((self.regs.aqa >> 16) & 0xFFF) + 1;
            let ok = v & CC_MPS_MASK == 0
                && v & CC_CSS_MASK == 0
                && self.regs.asq != 0
                && self.regs.acq != 0
                && aqs >= 2
                && acs >= 2
                && aqs <= QUEUE_ENTRIES_MAX
                && acs <= QUEUE_ENTRIES_MAX;
            if ok {
                self.sqs = [None; IO_QUEUES_MAX + 1];
                self.cqs = [None; IO_QUEUES_MAX + 1];
                self.sqs[0] = Some(Sq {
                    base: self.regs.asq,
                    size: aqs as u16,
                    cqid: 0,
                    head: 0,
                    tail: 0,
                });
                self.cqs[0] = Some(Cq {
                    base: self.regs.acq,
                    size: acs as u16,
                    head: 0,
                    tail: 0,
                    phase: true,
                    ien: true,
                });
                self.regs.csts = CSTS_RDY;
            } else {
                // Controller fatal status: the guest sees RDY never set.
                self.regs.csts = 1 << 1;
            }
        } else if !enable && was_enabled {
            self.reset_queues();
            self.regs.csts = 0;
        }
        // Shutdown notification: processing completes immediately.
        if (v >> CC_SHN_SHIFT) & 0b11 != 0 {
            self.regs.csts = (self.regs.csts & !(0b11 << 2)) | CSTS_SHST_DONE;
        } else {
            self.regs.csts &= !(0b11 << 2);
        }
    }

    fn reset_queues(&mut self) {
        self.sqs = [None; IO_QUEUES_MAX + 1];
        self.cqs = [None; IO_QUEUES_MAX + 1];
        self.granted_queues = 0;
        self.aer_pending.clear();
    }

    // ---- doorbells and queue execution -------------------------------------

    fn doorbell(&mut self, offset: u64, value: u32, mem: &dyn GuestMemory) {
        if !self.ready() {
            return;
        }
        // Stride 4: doorbell `n` is at 0x1000 + 4n; even n = SQ tail, odd = CQ head.
        let n = ((offset - DOORBELL_BASE) / 4) as usize;
        let qid = n / 2;
        if qid > IO_QUEUES_MAX {
            return;
        }
        let index = (value & 0xFFFF) as u16;
        if n.is_multiple_of(2) {
            if let Some(sq) = self.sqs[qid].as_mut()
                && index < sq.size
            {
                sq.tail = index;
            }
        } else if let Some(cq) = self.cqs[qid].as_mut()
            && index < cq.size
        {
            cq.head = index;
        }
        self.run_queues(mem);
    }

    /// Execute every submitted command that can complete: stop a queue when
    /// its completion queue is full (it resumes when the guest moves the head).
    fn run_queues(&mut self, mem: &dyn GuestMemory) {
        loop {
            let mut progressed = false;
            for qid in 0..=IO_QUEUES_MAX {
                while self.step_queue(qid, mem) {
                    progressed = true;
                }
            }
            if !progressed {
                return;
            }
        }
    }

    /// Run one command from submission queue `qid`, if one is waiting and its
    /// completion queue has room. Returns whether a command ran.
    fn step_queue(&mut self, qid: usize, mem: &dyn GuestMemory) -> bool {
        let Some(sq) = self.sqs[qid] else {
            return false;
        };
        if sq.head == sq.tail {
            return false;
        }
        let cqid = usize::from(sq.cqid);
        let Some(cq) = self.cqs[cqid] else {
            return false;
        };
        if (cq.tail + 1) % cq.size == cq.head {
            return false; // completion queue full
        }
        let mut raw = [0u8; SQE_BYTES];
        let at = sq.base + u64::from(sq.head) * SQE_BYTES as u64;
        let cmd = if mem.read(at, &mut raw) {
            Some(Command::parse(&raw))
        } else {
            None
        };
        // Advance the head first: even a command that cannot be read completes.
        if let Some(s) = self.sqs[qid].as_mut() {
            s.head = (s.head + 1) % s.size;
        }
        let (command_id, outcome, stays_open) = cmd.map_or_else(
            || (0, Outcome::err(SC_DATA_TRANSFER_ERROR), false),
            |c| {
                let (result, open) = if qid == 0 {
                    self.admin(&c, mem)
                } else {
                    (self.io(&c, mem), false)
                };
                (c.cid, result, open)
            },
        );
        if !stays_open {
            self.complete(qid, cqid, command_id, outcome, mem);
        }
        true
    }

    /// Post a completion on queue `cqid` for a command from queue `qid`.
    fn complete(
        &mut self,
        qid: usize,
        cqid: usize,
        command_id: u16,
        out: Outcome,
        mem: &dyn GuestMemory,
    ) {
        let Some(cq) = self.cqs[cqid] else {
            return;
        };
        let sq_head = self.sqs[qid].map_or(0, |s| s.head);
        let dnr = out.status != SC_OK;
        let mut status = status_field(out.status, dnr);
        if cq.phase {
            status |= 1;
        }
        let mut cqe = [0u8; CQE_BYTES];
        cqe[0..4].copy_from_slice(&out.result.to_le_bytes());
        cqe[8..10].copy_from_slice(&sq_head.to_le_bytes());
        cqe[10..12].copy_from_slice(&(qid as u16).to_le_bytes());
        cqe[12..14].copy_from_slice(&command_id.to_le_bytes());
        cqe[14..16].copy_from_slice(&status.to_le_bytes());
        let at = cq.base + u64::from(cq.tail) * CQE_BYTES as u64;
        // The phase bit lives in the last byte pair: write the body first so a
        // guest polling the phase never sees a half-written entry.
        let _ = mem.write(at, &cqe[..14]);
        let _ = mem.write(at + 14, &cqe[14..]);
        if let Some(c) = self.cqs[cqid].as_mut() {
            c.tail += 1;
            if c.tail == c.size {
                c.tail = 0;
                c.phase = !c.phase;
            }
        }
    }

    // ---- admin commands ------------------------------------------------------

    /// Execute an admin command. The second value is `true` when the command
    /// stays outstanding (an asynchronous event request).
    fn admin(&mut self, c: &Command, mem: &dyn GuestMemory) -> (Outcome, bool) {
        let out = match c.opcode {
            ADMIN_IDENTIFY => self.identify(c, mem),
            ADMIN_CREATE_CQ => self.create_cq(c),
            ADMIN_CREATE_SQ => self.create_sq(c),
            ADMIN_DELETE_SQ => self.delete_sq(c),
            ADMIN_DELETE_CQ => self.delete_cq(c),
            ADMIN_SET_FEATURES => self.set_features(c),
            ADMIN_GET_FEATURES => self.get_features(c),
            ADMIN_GET_LOG_PAGE => Self::get_log_page(c, mem),
            ADMIN_ABORT => Outcome {
                status: SC_OK,
                // Bit 0 set: the command was not aborted (it already completed).
                result: 1,
            },
            ADMIN_ASYNC_EVENT => {
                if self.aer_pending.len() >= AER_LIMIT {
                    Outcome::err(SC_ABORT_LIMIT)
                } else {
                    self.aer_pending.push(c.cid);
                    return (Outcome::OK, true);
                }
            }
            _ => Outcome::err(SC_INVALID_OPCODE),
        };
        (out, false)
    }

    fn identify(&self, c: &Command, mem: &dyn GuestMemory) -> Outcome {
        let cns = c.cdw10 & 0xFF;
        let data = match cns {
            0x00 => {
                if c.nsid != NSID && c.nsid != 0xFFFF_FFFF {
                    return Outcome::err(SC_INVALID_NAMESPACE);
                }
                self.identify_namespace()
            }
            0x01 => self.identify_controller(),
            0x02 => {
                // Active namespace ids greater than `nsid`.
                let mut page = vec![0u8; PAGE];
                if c.nsid < NSID {
                    page[0..4].copy_from_slice(&NSID.to_le_bytes());
                }
                page
            }
            0x03 => {
                // Namespace identification descriptors: one NGUID.
                if c.nsid != NSID {
                    return Outcome::err(SC_INVALID_NAMESPACE);
                }
                let mut page = vec![0u8; PAGE];
                page[0] = 0x02; // NIDT: NGUID
                page[1] = 16; // NIDL
                page[4..20].copy_from_slice(&self.nguid());
                page
            }
            // Controller list, namespace-management and all other structures:
            // an empty list rather than an error, which every driver tolerates.
            0x10 | 0x12 | 0x13 => vec![0u8; PAGE],
            _ => return Outcome::err(SC_INVALID_FIELD),
        };
        if write_prp(mem, c.prp1, c.prp2, &data) {
            Outcome::OK
        } else {
            Outcome::err(SC_DATA_TRANSFER_ERROR)
        }
    }

    /// A 16-byte namespace GUID derived from the serial number: stable across
    /// boots, which Windows uses to recognise the same disk.
    fn nguid(&self) -> [u8; 16] {
        let mut g = [0u8; 16];
        // The vendor OUI-style prefix, then the serial packed in.
        for (i, b) in self.identity.serial.iter().enumerate() {
            g[i % 16] ^= b.rotate_left((i / 16) as u32);
        }
        g[0] = 0x00;
        g[1] = 0x25;
        g[2] = 0x38; // Samsung's OUI, matching the default identity
        g
    }

    fn identify_controller(&self) -> Vec<u8> {
        let mut d = vec![0u8; PAGE];
        d[0..2].copy_from_slice(&self.identity.vendor.to_le_bytes());
        d[2..4].copy_from_slice(&self.identity.vendor.to_le_bytes()); // SSVID
        pad_ascii(&mut d[4..24], &self.identity.serial);
        pad_ascii(&mut d[24..64], &self.identity.model);
        pad_ascii(&mut d[64..72], &self.identity.firmware);
        d[73..76].copy_from_slice(&[0x00, 0x25, 0x38]); // IEEE OUI
        d[77] = 5; // MDTS: 2^5 pages = 128 KiB
        d[80..84].copy_from_slice(&VS_VALUE.to_le_bytes());
        d[259] = (AER_LIMIT - 1) as u8; // AERL
        d[260] = 0x02; // FRMW: one firmware slot, not updatable
        d[262] = 63; // ELPE: 64 error log entries
        d[266..268].copy_from_slice(&0x0157u16.to_le_bytes()); // WCTEMP
        d[268..270].copy_from_slice(&0x015Fu16.to_le_bytes()); // CCTEMP
        d[512] = 0x66; // SQES: 64-byte entries
        d[513] = 0x44; // CQES: 16-byte entries
        d[516..520].copy_from_slice(&1u32.to_le_bytes()); // NN: one namespace
        // ONCS: Write Zeroes and Dataset Management (deallocate is a no-op).
        d[520..522].copy_from_slice(&((1u16 << 3) | (1 << 2)).to_le_bytes());
        // SUBNQN.
        let mut nqn = b"nqn.2026-10.org.enlil:".to_vec();
        nqn.extend_from_slice(&self.identity.serial);
        d[768..768 + nqn.len().min(223)].copy_from_slice(&nqn[..nqn.len().min(223)]);
        d
    }

    fn identify_namespace(&self) -> Vec<u8> {
        let mut d = vec![0u8; PAGE];
        let sectors = self.backend.sectors();
        d[0..8].copy_from_slice(&sectors.to_le_bytes()); // NSZE
        d[8..16].copy_from_slice(&sectors.to_le_bytes()); // NCAP
        d[16..24].copy_from_slice(&sectors.to_le_bytes()); // NUSE
        d[25] = 0; // NLBAF: one LBA format
        d[26] = 0; // FLBAS: format 0
        d[104..120].copy_from_slice(&self.nguid());
        // LBAF0: no metadata, 2^9 = 512-byte sectors, best performance.
        d[128..132].copy_from_slice(&(9u32 << 16).to_le_bytes());
        d
    }

    fn create_cq(&mut self, c: &Command) -> Outcome {
        let qid = (c.cdw10 & 0xFFFF) as usize;
        let size = ((c.cdw10 >> 16) & 0xFFFF) + 1;
        let contiguous = c.cdw11 & 1 != 0;
        let ien = c.cdw11 & 2 != 0;
        let iv = (c.cdw11 >> 16) & 0xFFFF;
        if qid == 0 || qid > IO_QUEUES_MAX || self.cqs[qid].is_some() {
            return Outcome::err(SC_INVALID_QID);
        }
        if !(2..=QUEUE_ENTRIES_MAX).contains(&size) {
            return Outcome::err(SC_INVALID_QSIZE);
        }
        if !contiguous || c.prp1 == 0 || c.prp1 & 0xFFF != 0 {
            return Outcome::err(SC_INVALID_FIELD);
        }
        if iv != 0 {
            // Pin-based interrupts have a single vector.
            return Outcome::err(SC_INVALID_VECTOR);
        }
        self.cqs[qid] = Some(Cq {
            base: c.prp1,
            size: size as u16,
            head: 0,
            tail: 0,
            phase: true,
            ien,
        });
        Outcome::OK
    }

    fn create_sq(&mut self, c: &Command) -> Outcome {
        let qid = (c.cdw10 & 0xFFFF) as usize;
        let size = ((c.cdw10 >> 16) & 0xFFFF) + 1;
        let contiguous = c.cdw11 & 1 != 0;
        let cqid = ((c.cdw11 >> 16) & 0xFFFF) as usize;
        if qid == 0 || qid > IO_QUEUES_MAX || self.sqs[qid].is_some() {
            return Outcome::err(SC_INVALID_QID);
        }
        if !(2..=QUEUE_ENTRIES_MAX).contains(&size) {
            return Outcome::err(SC_INVALID_QSIZE);
        }
        if cqid == 0 || cqid > IO_QUEUES_MAX || self.cqs[cqid].is_none() {
            return Outcome::err(SC_INVALID_CQ);
        }
        if !contiguous || c.prp1 == 0 || c.prp1 & 0xFFF != 0 {
            return Outcome::err(SC_INVALID_FIELD);
        }
        self.sqs[qid] = Some(Sq {
            base: c.prp1,
            size: size as u16,
            cqid: cqid as u16,
            head: 0,
            tail: 0,
        });
        Outcome::OK
    }

    const fn delete_sq(&mut self, c: &Command) -> Outcome {
        let qid = (c.cdw10 & 0xFFFF) as usize;
        if qid == 0 || qid > IO_QUEUES_MAX || self.sqs[qid].is_none() {
            return Outcome::err(SC_INVALID_QID);
        }
        self.sqs[qid] = None;
        Outcome::OK
    }

    fn delete_cq(&mut self, c: &Command) -> Outcome {
        let qid = (c.cdw10 & 0xFFFF) as usize;
        if qid == 0 || qid > IO_QUEUES_MAX || self.cqs[qid].is_none() {
            return Outcome::err(SC_INVALID_QID);
        }
        // A completion queue still used by a submission queue cannot go.
        if self
            .sqs
            .iter()
            .flatten()
            .any(|s| usize::from(s.cqid) == qid)
        {
            return Outcome::err(SC_INVALID_DELETE);
        }
        self.cqs[qid] = None;
        Outcome::OK
    }

    fn feature_slot(&mut self, fid: u32) -> Option<&mut (u32, u32)> {
        self.features.iter_mut().find(|(id, _)| *id == fid)
    }

    fn set_features(&mut self, c: &Command) -> Outcome {
        let fid = c.cdw10 & 0xFF;
        if fid == FEAT_NUM_QUEUES {
            // Grant what was asked for, up to what exists (both values 0-based).
            let submission_wanted = (c.cdw11 & 0xFFFF) as usize;
            let completion_wanted = ((c.cdw11 >> 16) & 0xFFFF) as usize;
            let granted = submission_wanted
                .min(completion_wanted)
                .min(IO_QUEUES_MAX - 1) as u16;
            self.granted_queues = granted;
            return Outcome {
                status: SC_OK,
                result: u32::from(granted) | (u32::from(granted) << 16),
            };
        }
        match self.feature_slot(fid) {
            Some(slot) => {
                slot.1 = c.cdw11;
                Outcome::OK
            }
            None => Outcome::err(SC_INVALID_FIELD),
        }
    }

    fn get_features(&mut self, c: &Command) -> Outcome {
        let fid = c.cdw10 & 0xFF;
        if fid == FEAT_NUM_QUEUES {
            let granted = u32::from(self.granted_queues);
            return Outcome {
                status: SC_OK,
                result: granted | (granted << 16),
            };
        }
        match self.feature_slot(fid) {
            Some(slot) => Outcome {
                status: SC_OK,
                result: slot.1,
            },
            None => Outcome::err(SC_INVALID_FIELD),
        }
    }

    fn get_log_page(c: &Command, mem: &dyn GuestMemory) -> Outcome {
        // Dword count is 0-based, split across CDW10[27:16] and CDW11[15:0].
        let numd = ((c.cdw10 >> 16) & 0xFFF) | ((c.cdw11 & 0xFFFF) << 12);
        let len = ((numd as usize) + 1) * 4;
        if len > MAX_TRANSFER_BYTES {
            return Outcome::err(SC_INVALID_FIELD);
        }
        let lid = c.cdw10 & 0xFF;
        let mut page = vec![0u8; len];
        if lid == 0x02 && len >= 512 {
            // SMART / health: spare capacity healthy, temperature 40 C.
            page[1..3].copy_from_slice(&313u16.to_le_bytes());
            page[3] = 100; // available spare
            page[4] = 10; // spare threshold
        }
        // Log pages need no PRP-list chain here: they are small.
        let ok = write_prp(mem, c.prp1, c.prp2, &page);
        if ok {
            Outcome::OK
        } else {
            Outcome::err(SC_DATA_TRANSFER_ERROR)
        }
    }

    // ---- I/O commands ---------------------------------------------------------

    fn io(&mut self, c: &Command, mem: &dyn GuestMemory) -> Outcome {
        if c.nsid != NSID && c.nsid != 0xFFFF_FFFF {
            return Outcome::err(SC_INVALID_NAMESPACE);
        }
        match c.opcode {
            IO_FLUSH => match self.backend.flush() {
                Ok(()) => Outcome::OK,
                Err(e) => Outcome::err(block_status(e)),
            },
            IO_READ | IO_WRITE => self.transfer(c, mem),
            IO_WRITE_ZEROES => self.write_zeroes(c),
            // Deallocate (TRIM) is only a hint: accepting it without touching
            // the disk can never reach a sector outside the allow-list.
            IO_DATASET_MGMT => Outcome::OK,
            _ => Outcome::err(SC_INVALID_OPCODE),
        }
    }

    /// The starting LBA and block count of a read/write.
    const fn lba_range(c: &Command) -> (u64, usize) {
        let slba = join(c.cdw10, c.cdw11);
        let blocks = ((c.cdw12 & 0xFFFF) as usize) + 1;
        (slba, blocks)
    }

    fn transfer(&mut self, c: &Command, mem: &dyn GuestMemory) -> Outcome {
        let (slba, blocks) = Self::lba_range(c);
        let bytes = blocks * SECTOR;
        if bytes > MAX_TRANSFER_BYTES {
            return Outcome::err(SC_INVALID_FIELD);
        }
        let end = slba.checked_add(blocks as u64);
        if end.is_none_or(|e| e > self.backend.sectors()) {
            return Outcome::err(SC_LBA_OUT_OF_RANGE);
        }
        let Some(segments) = prp_segments(mem, c.prp1, c.prp2, bytes) else {
            return Outcome::err(SC_INVALID_FIELD);
        };
        let mut buf = vec![0u8; bytes];
        if c.opcode == IO_READ {
            if let Err(e) = self.backend.read(slba, &mut buf) {
                return Outcome::err(block_status(e));
            }
            let mut at = 0;
            for (gpa, len) in segments {
                if !mem.write(gpa, &buf[at..at + len]) {
                    return Outcome::err(SC_DATA_TRANSFER_ERROR);
                }
                at += len;
            }
        } else {
            if self.read_only {
                return Outcome::err(SC_WRITE_DENIED);
            }
            let mut at = 0;
            for (gpa, len) in segments {
                if !mem.read(gpa, &mut buf[at..at + len]) {
                    return Outcome::err(SC_DATA_TRANSFER_ERROR);
                }
                at += len;
            }
            if let Err(e) = self.backend.write(slba, &buf) {
                return Outcome::err(block_status(e));
            }
        }
        Outcome::OK
    }

    fn write_zeroes(&mut self, c: &Command) -> Outcome {
        let (slba, blocks) = Self::lba_range(c);
        let end = slba.checked_add(blocks as u64);
        if end.is_none_or(|e| e > self.backend.sectors()) {
            return Outcome::err(SC_LBA_OUT_OF_RANGE);
        }
        if self.read_only {
            return Outcome::err(SC_WRITE_DENIED);
        }
        // Zero in bounded chunks so a huge request cannot allocate unboundedly.
        let zeros = vec![0u8; MAX_TRANSFER_BYTES.min(blocks * SECTOR)];
        let mut lba = slba;
        let mut left = blocks;
        while left > 0 {
            let n = left.min(zeros.len() / SECTOR);
            if let Err(e) = self.backend.write(lba, &zeros[..n * SECTOR]) {
                return Outcome::err(block_status(e));
            }
            lba += n as u64;
            left -= n;
        }
        Outcome::OK
    }
}

/// Space-pad `src` into `dst` (an ASCII identify-structure field).
fn pad_ascii(dst: &mut [u8], src: &[u8]) {
    dst.fill(b' ');
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
}

/// The completion status for a failed block operation.
const fn block_status(e: BlockError) -> u16 {
    match e {
        BlockError::OutOfRange => SC_LBA_OUT_OF_RANGE,
        BlockError::Denied => SC_WRITE_DENIED,
        BlockError::Unaligned => SC_INVALID_FIELD,
        BlockError::Io => SC_INTERNAL_ERROR,
    }
}

/// Resolve a command's PRP entries into `(guest address, length)` segments
/// covering `len` bytes, or `None` for a malformed or out-of-range description.
///
/// PRP1 points at the first (possibly unaligned) page; if the transfer fits in
/// that page and one more, PRP2 is the second page; beyond that PRP2 points at
/// a list of page addresses, whose last entry chains to the next list page when
/// the transfer needs more entries than one list page holds.
fn prp_segments(
    mem: &dyn GuestMemory,
    prp1: u64,
    prp2: u64,
    len: usize,
) -> Option<Vec<(u64, usize)>> {
    if len == 0 || len > MAX_TRANSFER_BYTES || prp1 == 0 {
        return None;
    }
    let mut segments = Vec::new();
    let first = (PAGE - (prp1 as usize & (PAGE - 1))).min(len);
    segments.push((prp1, first));
    let mut left = len - first;
    if left == 0 {
        return Some(segments);
    }
    if prp2 == 0 {
        return None;
    }
    if left <= PAGE {
        // PRP2 is the second data page and must be page-aligned.
        if prp2 & (PAGE as u64 - 1) != 0 {
            return None;
        }
        segments.push((prp2, left));
        return Some(segments);
    }
    // PRP2 is a list pointer (dword-aligned at least).
    if prp2 & 7 != 0 {
        return None;
    }
    let page_mask = PAGE as u64 - 1;
    let per_page = PAGE / 8;
    // The list page and the entry index within it, kept separate so an
    // unaligned list pointer is counted once.
    let mut page = prp2 & !page_mask;
    let mut index = (prp2 & page_mask) as usize / 8;
    while left > 0 {
        let mut entry = rd64(mem, page + (index as u64) * 8)?;
        // The last slot of a list page is a chain pointer when more follows.
        if index == per_page - 1 && left.div_ceil(PAGE) > 1 {
            if entry == 0 || entry & 7 != 0 {
                return None;
            }
            page = entry & !page_mask;
            index = (entry & page_mask) as usize / 8;
            entry = rd64(mem, page + (index as u64) * 8)?;
        }
        if entry == 0 || entry & page_mask != 0 {
            return None;
        }
        let n = left.min(PAGE);
        segments.push((entry, n));
        left -= n;
        index += 1;
    }
    Some(segments)
}

/// Write `data` to the guest through PRP1/PRP2.
fn write_prp(mem: &dyn GuestMemory, prp1: u64, prp2: u64, data: &[u8]) -> bool {
    let Some(segments) = prp_segments(mem, prp1, prp2, data.len()) else {
        return false;
    };
    let mut at = 0;
    for (gpa, len) in segments {
        if !mem.write(gpa, &data[at..at + len]) {
            return false;
        }
        at += len;
    }
    true
}

#[cfg(test)]
mod tests;

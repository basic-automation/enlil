//! Bare-metal `NVMe` storage driver for the enlil kernel (Phase 6.5, T-6.19).
//!
//! Drives a `PCI` `NVMe` controller directly from the boot kernel: the admin
//! queue pair is programmed, the controller enabled, the first namespace
//! identified, and one I/O queue pair created -- all with polled completions
//! (no interrupts yet; that is the reactor's later slice). Guests get disks
//! on bare metal through this; an `AHCI` controller is the fallback
//! ([`crate::ahci`]).
//!
//! The driver is generic over [`Mmio`](crate::mmio::Mmio): real hardware uses
//! [`PhysMmio`](crate::mmio::PhysMmio) at the controller's `BAR0`, unit tests
//! plug in a fake controller. DMA memory is [`DmaBuf`](crate::mmio::DmaBuf) --
//! page-aligned heap whose virtual address is the physical address under the
//! kernel's early identity map. There is no IOMMU programming here (that is
//! ROADMAP 6.4); the device DMAs straight into the kernel heap, which is the
//! documented posture until per-guest DMA domains exist.

extern crate alloc;

use crate::mmio::{DmaBuf, DmaError, Mmio};
use core::fmt;

// ---------------------------------------------------------------------------
// Controller registers (NVMe base spec §3.1)
// ---------------------------------------------------------------------------

/// Controller capabilities.
const REG_CAP: u64 = 0x0;
/// Controller configuration.
const REG_CC: u64 = 0x14;
/// Controller status.
const REG_CSTS: u64 = 0x1c;
/// Admin queue attributes.
const REG_AQA: u64 = 0x24;
/// Admin submission queue base address.
const REG_ASQ: u64 = 0x28;
/// Admin completion queue base address.
const REG_ACQ: u64 = 0x30;
/// First queue doorbell (SQ tail / CQ head pairs, `4 << DSTRD` apart).
const REG_DOORBELL_BASE: u64 = 0x1000;

/// `CC.EN`: controller enable.
const CC_EN: u32 = 1 << 0;
/// `CC.MPS` = 0 selects 4 KiB memory pages (bits 10:7).
const CC_MPS_4KIB: u32 = 0 << 7;
/// `CC.IOSQES` = 6: 64-byte submission entries (bits 19:16).
const CC_IOSQES_64: u32 = 6 << 16;
/// `CC.IOCQES` = 4: 16-byte completion entries (bits 23:20).
const CC_IOCQES_16: u32 = 4 << 20;
/// `CSTS.RDY`: controller ready.
const CSTS_RDY: u32 = 1 << 0;
/// `CAP.DSTRD` field: bits 35:32 (doorbell stride as `4 << DSTRD`).
const CAP_DSTRD_SHIFT: u32 = 32;
const CAP_DSTRD_MASK: u64 = 0x0f;

// ---------------------------------------------------------------------------
// Admin opcodes (NVMe base spec, Figure for Admin Command Set)
// ---------------------------------------------------------------------------

/// Create I/O submission queue.
const ADMIN_CREATE_IO_SQ: u8 = 0x01;
/// Create I/O completion queue.
const ADMIN_CREATE_IO_CQ: u8 = 0x05;
/// Identify.
const ADMIN_IDENTIFY: u8 = 0x06;

/// Identify: controller data structure.
const IDENTIFY_CNS_CONTROLLER: u32 = 0x01;
/// Identify: namespace data structure.
const IDENTIFY_CNS_NAMESPACE: u32 = 0x00;

// ---------------------------------------------------------------------------
// NVM command-set opcodes
// ---------------------------------------------------------------------------

/// Flush the volatile write cache.
const NVM_FLUSH: u8 = 0x00;
/// Write.
const NVM_WRITE: u8 = 0x01;
/// Read.
const NVM_READ: u8 = 0x02;

// ---------------------------------------------------------------------------
// Queue geometry
// ---------------------------------------------------------------------------

/// Admin queue depth (0's-based in `AQA`).
const ADMIN_QUEUE_SIZE: u16 = 64;
/// I/O queue depth.
const IO_QUEUE_SIZE: u16 = 256;
/// Bytes per submission entry.
const SQ_ENTRY_SIZE: usize = 64;
/// Bytes per completion entry.
const CQ_ENTRY_SIZE: usize = 16;
/// `NVMe` memory page size for PRP addressing.
const NVME_PAGE_SIZE: u64 = 4096;
/// Staging buffer: the largest single transfer (1 MiB, 512-byte multiple).
const STAGING_SIZE: usize = 1024 * 1024;

/// Bounded spin budget for controller init steps (enable/shutdown).
const INIT_SPINS: u64 = 20_000_000;
/// Bounded spin budget for one I/O completion.
const IO_SPINS: u64 = 200_000_000;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What can go wrong driving the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvmeError {
    /// A polled condition never became true within its spin budget.
    Timeout(&'static str),
    /// A command completed with a non-zero status.
    CommandFailed {
        /// The opcode that failed.
        opcode: u8,
        /// The controller's status field.
        status: u16,
    },
    /// The completion's command id did not match the submission.
    CompletionMismatch {
        /// The submitted command id.
        submitted: u16,
        /// The completed command id.
        got: u16,
    },
    /// Namespace id 0 is the broadcast namespace, not a real namespace.
    InvalidNamespaceId,
    /// The controller reports fewer namespaces than requested.
    NamespaceNotFound {
        /// The requested namespace id.
        nsid: u32,
        /// How many namespaces the controller reports.
        count: u32,
    },
    /// The namespace is not formatted with 512-byte sectors.
    UnsupportedSectorSize {
        /// The namespace's sector size in bytes.
        bytes: u64,
    },
    /// The namespace reports zero blocks.
    ZeroSizedNamespace,
    /// A transfer length that is not a non-zero 512-multiple <= 1 MiB.
    BadTransferLength(usize),
    /// The transfer would run past the namespace end.
    LbaOutOfRange {
        /// The requested start LBA.
        lba: u64,
        /// Blocks in the namespace.
        blocks: u64,
    },
    /// DMA allocation failed.
    Alloc(DmaError),
}

impl fmt::Display for NvmeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(what) => write!(f, "NVMe timeout waiting for {what}"),
            Self::CommandFailed { opcode, status } => {
                write!(f, "NVMe command {opcode:#04x} failed, status {status:#06x}")
            }
            Self::CompletionMismatch { submitted, got } => write!(
                f,
                "NVMe completion id mismatch: submitted {submitted}, got {got}"
            ),
            Self::InvalidNamespaceId => {
                write!(f, "NVMe namespace id 0 is the broadcast namespace")
            }
            Self::NamespaceNotFound { nsid, count } => write!(
                f,
                "NVMe controller reports {count} namespace(s); namespace {nsid} does not exist"
            ),
            Self::UnsupportedSectorSize { bytes } => write!(
                f,
                "NVMe namespace has {bytes}-byte sectors; only 512-byte sectors are supported"
            ),
            Self::ZeroSizedNamespace => write!(f, "NVMe namespace has zero size"),
            Self::BadTransferLength(len) => write!(
                f,
                "NVMe transfer must be a non-zero multiple of 512 bytes up to 1 MiB, got {len}"
            ),
            Self::LbaOutOfRange { lba, blocks } => {
                write!(
                    f,
                    "NVMe LBA {lba} is past the namespace end ({blocks} blocks)"
                )
            }
            Self::Alloc(e) => write!(f, "NVMe DMA allocation failed: {e}"),
        }
    }
}

impl core::error::Error for NvmeError {}

impl From<DmaError> for NvmeError {
    fn from(e: DmaError) -> Self {
        Self::Alloc(e)
    }
}

// ---------------------------------------------------------------------------
// Submission-queue entries
// ---------------------------------------------------------------------------

/// A 64-byte `NVMe` submission queue entry, little-endian.
#[derive(Clone, Copy)]
struct NvmeCmd([u8; 64]);

impl NvmeCmd {
    const fn new(opcode: u8) -> Self {
        let mut bytes = [0u8; 64];
        bytes[0] = opcode;
        Self(bytes)
    }

    #[must_use]
    const fn opcode(&self) -> u8 {
        self.0[0]
    }

    #[must_use]
    fn nsid(mut self, nsid: u32) -> Self {
        self.0[4..8].copy_from_slice(&nsid.to_le_bytes());
        self
    }

    #[must_use]
    fn cdw10(mut self, v: u32) -> Self {
        self.0[40..44].copy_from_slice(&v.to_le_bytes());
        self
    }

    #[must_use]
    fn cdw11(mut self, v: u32) -> Self {
        self.0[44..48].copy_from_slice(&v.to_le_bytes());
        self
    }

    #[must_use]
    fn cdw12(mut self, v: u32) -> Self {
        self.0[48..52].copy_from_slice(&v.to_le_bytes());
        self
    }

    #[must_use]
    fn prp1(mut self, v: u64) -> Self {
        self.0[24..32].copy_from_slice(&v.to_le_bytes());
        self
    }

    #[must_use]
    fn prp2(mut self, v: u64) -> Self {
        self.0[32..40].copy_from_slice(&v.to_le_bytes());
        self
    }

    #[must_use]
    const fn bytes(&self) -> &[u8; 64] {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// Queue pairs
// ---------------------------------------------------------------------------

/// One `NVMe` submission/completion queue pair with its DMA memory.
///
/// A single command is outstanding per queue at a time (submit, then poll),
/// so no SQ-full tracking is needed.
struct QueuePair {
    sq: DmaBuf,
    cq: DmaBuf,
    qid: u16,
    size: u16,
    db_stride: u64,
    sq_tail: u16,
    cq_head: u16,
    cq_phase: bool,
    next_cid: u16,
}

impl QueuePair {
    fn new(qid: u16, size: u16, db_stride: u64) -> Result<Self, NvmeError> {
        Ok(Self {
            sq: DmaBuf::new(usize::from(size) * SQ_ENTRY_SIZE)?,
            cq: DmaBuf::new(usize::from(size) * CQ_ENTRY_SIZE)?,
            qid,
            size,
            db_stride,
            sq_tail: 0,
            cq_head: 0,
            // Completion queues start with phase tag 1 (NVMe base spec §4.6).
            cq_phase: true,
            next_cid: 0,
        })
    }

    const fn sq_doorbell(&self) -> u64 {
        REG_DOORBELL_BASE + 2 * self.qid as u64 * self.db_stride
    }

    const fn cq_doorbell(&self) -> u64 {
        REG_DOORBELL_BASE + (2 * self.qid as u64 + 1) * self.db_stride
    }

    /// Copy `cmd` into the next submission slot, ring the doorbell, and return
    /// the command identifier echoed back in the completion.
    fn submit(&mut self, mmio: &impl Mmio, cmd: &NvmeCmd) -> u16 {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        let mut entry = *cmd.bytes();
        entry[2..4].copy_from_slice(&cid.to_le_bytes());
        let off = usize::from(self.sq_tail) * SQ_ENTRY_SIZE;
        self.sq.as_mut_slice()[off..off + SQ_ENTRY_SIZE].copy_from_slice(&entry);
        self.sq_tail = (self.sq_tail + 1) % self.size;
        mmio.write32(self.sq_doorbell(), u32::from(self.sq_tail));
        cid
    }

    /// Poll the completion queue until an entry with the expected phase tag
    /// appears. Returns `(command_id, status)`.
    ///
    /// # Errors
    ///
    /// Returns [`NvmeError::Timeout`] if no completion arrives within
    /// `max_spins` volatile reads.
    fn poll(&mut self, mmio: &impl Mmio, max_spins: u64) -> Result<(u16, u16), NvmeError> {
        let mut spins = 0u64;
        loop {
            let off = usize::from(self.cq_head) * CQ_ENTRY_SIZE;
            // Read DW3 volatile: the device writes the completion queue, so
            // the read must not be hoisted out of the poll loop. It must also
            // be ONE aligned 32-bit load: the device DMA-writes the entry
            // concurrently, and reading the dword a byte at a time can tear —
            // CID bytes from before the write, phase byte from after — which
            // surfaces as a "completed" entry with command id 0.
            let base = self.cq.as_slice().as_ptr();
            // SAFETY: `cq_head < size`, so the dword is in-bounds of the
            // exclusively-owned CQ buffer; the buffer is 4 KiB-aligned and each
            // entry is 16 bytes, so `off + 12` is 4-byte aligned.
            #[allow(clippy::cast_ptr_alignment)]
            let dw3 = u32::from_le(unsafe { base.add(off + 12).cast::<u32>().read_volatile() });
            // DW3: bits 15:0 CID, bit 16 phase tag, bits 31:17 status.
            if (dw3 >> 16) & 1 == u32::from(self.cq_phase) {
                let cid = (dw3 & 0xffff) as u16;
                let status = ((dw3 >> 17) & 0x7fff) as u16;
                self.cq_head = (self.cq_head + 1) % self.size;
                if self.cq_head == 0 {
                    self.cq_phase = !self.cq_phase;
                }
                mmio.write32(self.cq_doorbell(), u32::from(self.cq_head));
                return Ok((cid, status));
            }
            spins += 1;
            if spins >= max_spins {
                return Err(NvmeError::Timeout("I/O completion"));
            }
            core::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// Controller driver
// ---------------------------------------------------------------------------

/// Byte offset of the model number (`MN`) in the Identify Controller data.
const IDENTIFY_MN_OFFSET: usize = 24;
/// Length of the model number field: 40 ASCII bytes, space padded.
pub const MODEL_LEN: usize = 40;
/// Byte offset of the serial number (`SN`) in the Identify Controller data.
const IDENTIFY_SN_OFFSET: usize = 4;
/// Length of the serial number field: 20 ASCII bytes, space padded.
pub const SERIAL_LEN: usize = 20;
/// Length of the firmware revision (`FR`) field in the controller identify data.
pub const FIRMWARE_LEN: usize = 8;
/// Offset of the firmware revision in the controller identify data.
const IDENTIFY_FR_OFFSET: usize = 64;

/// Whether a space-padded Identify serial number equals `wanted`.
///
/// Both sides are trimmed of leading/trailing spaces and NULs; an empty
/// `wanted` never matches (an unnamed disk must not be selected by accident).
#[must_use]
pub fn serial_matches(serial: &[u8], wanted: &str) -> bool {
    let trim = |b: &[u8]| -> usize {
        let start = b
            .iter()
            .position(|&c| c != b' ' && c != 0)
            .unwrap_or(b.len());
        let end = b
            .iter()
            .rposition(|&c| c != b' ' && c != 0)
            .map_or(start, |i| i + 1);
        end - start
    };
    let a = serial;
    let w = wanted.as_bytes();
    let (sa, sw) = (
        a.iter()
            .position(|&c| c != b' ' && c != 0)
            .unwrap_or(a.len()),
        w.iter()
            .position(|&c| c != b' ' && c != 0)
            .unwrap_or(w.len()),
    );
    let (la, lw) = (trim(a), trim(w));
    lw != 0 && la == lw && a[sa..sa + la] == w[sw..sw + lw]
}

/// Whether an Identify Controller model number names an **emulated** device.
///
/// The kernel's destructive block write/read self-test is allowed only on such
/// controllers: it overwrites one sector, which on a real disk is real data
/// (the last sector of a GPT disk holds the backup partition table). QEMU's
/// `NVMe` model is `QEMU NVMe Ctrl`; anything else is treated as hardware.
#[must_use]
pub fn model_is_emulated(model: &[u8]) -> bool {
    let start = model.iter().position(|&b| b != b' ').unwrap_or(model.len());
    model[start..].starts_with(b"QEMU")
}

/// A directly-driven `NVMe` controller: admin queue + one I/O queue pair with
/// polled completions.
///
/// `M` is the `BAR0` access method -- [`PhysMmio`](crate::mmio::PhysMmio) on
/// real hardware, a fake in tests.
pub struct NvmeController<M: Mmio> {
    mmio: M,
    admin: QueuePair,
    io: QueuePair,
    staging: DmaBuf,
    prp_list: DmaBuf,
    nsid: u32,
    namespace_blocks: u64,
    model: [u8; MODEL_LEN],
    serial: [u8; SERIAL_LEN],
    firmware: [u8; FIRMWARE_LEN],
}

/// Spin until `condition` holds or the budget is exhausted.
///
/// # Errors
///
/// Returns [`NvmeError::Timeout`] on exhaustion.
fn poll_until(
    max_spins: u64,
    what: &'static str,
    mut condition: impl FnMut() -> bool,
) -> Result<(), NvmeError> {
    let mut spins = 0u64;
    while !condition() {
        spins += 1;
        if spins >= max_spins {
            return Err(NvmeError::Timeout(what));
        }
        core::hint::spin_loop();
    }
    Ok(())
}

impl<M: Mmio> fmt::Debug for NvmeController<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NvmeController")
            .field("nsid", &self.nsid)
            .field("namespace_blocks", &self.namespace_blocks)
            .field("admin_qid", &self.admin.qid)
            .field("io_qid", &self.io.qid)
            .finish_non_exhaustive()
    }
}

impl<M: Mmio> NvmeController<M> {
    /// Take ownership of an `NVMe` controller: program the admin queues, enable
    /// it, identify the controller and `nsid`, and create the I/O queue pair.
    ///
    /// `nsid` is the namespace to expose (usually 1); only 512-byte
    /// namespaces are supported.
    ///
    /// # Errors
    ///
    /// Returns [`NvmeError`] if the controller does not become ready, the
    /// namespace does not exist or is not 512-byte formatted, or any admin
    /// command fails.
    pub fn init(mmio: M, nsid: u32) -> Result<Self, NvmeError> {
        if nsid == 0 {
            return Err(NvmeError::InvalidNamespaceId);
        }
        let cap = mmio.read64(REG_CAP);
        let dstrd = (cap >> CAP_DSTRD_SHIFT) & CAP_DSTRD_MASK;
        let db_stride = 4u64 << dstrd;

        if mmio.read32(REG_CSTS) & CSTS_RDY != 0 {
            // A previous driver left the controller enabled; shut it down
            // before reprogramming the admin queues.
            mmio.write32(REG_CC, 0);
            poll_until(INIT_SPINS, "controller shutdown", || {
                mmio.read32(REG_CSTS) & CSTS_RDY == 0
            })?;
        }

        let admin = QueuePair::new(0, ADMIN_QUEUE_SIZE, db_stride)?;
        let io = QueuePair::new(1, IO_QUEUE_SIZE, db_stride)?;
        let staging = DmaBuf::new(STAGING_SIZE)?;
        let prp_list = DmaBuf::new(4096)?;

        let mut ctrl = Self {
            mmio,
            admin,
            io,
            staging,
            prp_list,
            nsid,
            namespace_blocks: 0,
            model: [0; MODEL_LEN],
            serial: [0; SERIAL_LEN],
            firmware: [0; FIRMWARE_LEN],
        };

        // Program the admin queue pair: queue sizes are 0's-based in AQA, and
        // the queue addresses are physical (= virtual under the identity map).
        let qsize = u32::from(ADMIN_QUEUE_SIZE - 1);
        ctrl.mmio.write32(REG_AQA, qsize | (qsize << 16));
        ctrl.mmio.write64(REG_ASQ, ctrl.admin.sq.phys());
        ctrl.mmio.write64(REG_ACQ, ctrl.admin.cq.phys());

        // Enable: NVM command set, 4 KiB memory pages, 64-byte SQ entries,
        // 16-byte CQ entries.
        ctrl.mmio
            .write32(REG_CC, CC_EN | CC_MPS_4KIB | CC_IOSQES_64 | CC_IOCQES_16);
        poll_until(INIT_SPINS, "controller ready", || {
            ctrl.mmio.read32(REG_CSTS) & CSTS_RDY != 0
        })?;

        // Identify the controller: how many namespaces exist.
        ctrl.identify(IDENTIFY_CNS_CONTROLLER, 0)?;
        let data = ctrl.staging.as_slice();
        ctrl.model
            .copy_from_slice(&data[IDENTIFY_MN_OFFSET..IDENTIFY_MN_OFFSET + MODEL_LEN]);
        ctrl.serial
            .copy_from_slice(&data[IDENTIFY_SN_OFFSET..IDENTIFY_SN_OFFSET + SERIAL_LEN]);
        ctrl.firmware
            .copy_from_slice(&data[IDENTIFY_FR_OFFSET..IDENTIFY_FR_OFFSET + FIRMWARE_LEN]);
        let nn = u32::from_le_bytes(
            data[516..520]
                .try_into()
                .map_err(|_| NvmeError::Timeout("identify data"))?,
        );
        if nsid > nn {
            return Err(NvmeError::NamespaceNotFound { nsid, count: nn });
        }

        // Identify the namespace: size and LBA format. The block layer above
        // speaks 512-byte sectors.
        ctrl.identify(IDENTIFY_CNS_NAMESPACE, nsid)?;
        let data = ctrl.staging.as_slice();
        let nsze = u64::from_le_bytes(
            data[0..8]
                .try_into()
                .map_err(|_| NvmeError::Timeout("identify data"))?,
        );
        let flbas = data[26];
        let lba_fmt_off = 128 + usize::from(flbas & 0x0f) * 16;
        let lbads = u16::from_le_bytes(
            data[lba_fmt_off + 2..lba_fmt_off + 4]
                .try_into()
                .map_err(|_| NvmeError::Timeout("identify data"))?,
        );
        if lbads != 9 {
            return Err(NvmeError::UnsupportedSectorSize {
                bytes: 1u64 << lbads,
            });
        }
        if nsze == 0 {
            return Err(NvmeError::ZeroSizedNamespace);
        }
        ctrl.namespace_blocks = nsze;

        // Create I/O completion queue 1: physically contiguous, no interrupts
        // (completions are polled). CDW10: QID in bits 15:0, 0's-based QSIZE
        // in bits 31:16.
        let qsize = u32::from(IO_QUEUE_SIZE - 1);
        let cmd = NvmeCmd::new(ADMIN_CREATE_IO_CQ)
            .cdw10((qsize << 16) | 1)
            .cdw11(1) // PC = 1
            .prp1(ctrl.io.cq.phys());
        ctrl.admin_cmd(cmd)?;

        // Create I/O submission queue 1 on completion queue 1. CDW11: CQID in
        // bits 15:0, PC in bit 16.
        let cmd = NvmeCmd::new(ADMIN_CREATE_IO_SQ)
            .cdw10((qsize << 16) | 1)
            .cdw11(1 | (1 << 16))
            .prp1(ctrl.io.sq.phys());
        ctrl.admin_cmd(cmd)?;

        Ok(ctrl)
    }

    /// The controller's model number (`MN`), 40 ASCII bytes, space padded.
    #[must_use]
    pub const fn model(&self) -> &[u8; MODEL_LEN] {
        &self.model
    }

    /// The controller's serial number (`SN`), 20 ASCII bytes, space padded.
    #[must_use]
    pub const fn serial(&self) -> &[u8; SERIAL_LEN] {
        &self.serial
    }

    /// The controller's firmware revision (`FR`), 8 ASCII bytes, space padded.
    #[must_use]
    pub const fn firmware(&self) -> &[u8; FIRMWARE_LEN] {
        &self.firmware
    }

    /// Whether this controller's serial number equals `wanted` (ignoring the
    /// space padding the controller adds).
    #[must_use]
    pub fn serial_is(&self, wanted: &str) -> bool {
        serial_matches(&self.serial, wanted)
    }

    /// Stop the controller (`CC.EN = 0`) and wait for it to report not-ready.
    ///
    /// Used on a controller that was opened only to read its identity and is
    /// not going to be used: it must not be left running with queues in memory
    /// that is about to be freed.
    pub fn disable(&mut self) {
        self.mmio.write32(REG_CC, 0);
        let _ = poll_until(INIT_SPINS, "controller shutdown", || {
            self.mmio.read32(REG_CSTS) & CSTS_RDY == 0
        });
    }

    /// Whether this controller is an emulated device (see [`model_is_emulated`]).
    #[must_use]
    pub fn is_emulated(&self) -> bool {
        model_is_emulated(&self.model)
    }

    /// Number of 512-byte blocks in the namespace.
    #[must_use]
    pub const fn namespace_blocks(&self) -> u64 {
        self.namespace_blocks
    }

    /// Submit an admin command and wait for its completion.
    fn admin_cmd(&mut self, cmd: NvmeCmd) -> Result<(), NvmeError> {
        let cid = self.admin.submit(&self.mmio, &cmd);
        let (got, status) = self.admin.poll(&self.mmio, IO_SPINS)?;
        if got != cid {
            return Err(NvmeError::CompletionMismatch {
                submitted: cid,
                got,
            });
        }
        if status != 0 {
            return Err(NvmeError::CommandFailed {
                opcode: cmd.opcode(),
                status,
            });
        }
        Ok(())
    }

    /// Submit an I/O command and wait for its completion.
    fn io_cmd(&mut self, cmd: NvmeCmd) -> Result<(), NvmeError> {
        let cid = self.io.submit(&self.mmio, &cmd);
        let (got, status) = self.io.poll(&self.mmio, IO_SPINS)?;
        if got != cid {
            return Err(NvmeError::CompletionMismatch {
                submitted: cid,
                got,
            });
        }
        if status != 0 {
            return Err(NvmeError::CommandFailed {
                opcode: cmd.opcode(),
                status,
            });
        }
        Ok(())
    }

    /// Run Identify (`cns`) into the staging buffer.
    fn identify(&mut self, cns: u32, nsid: u32) -> Result<(), NvmeError> {
        let cmd = NvmeCmd::new(ADMIN_IDENTIFY)
            .nsid(nsid)
            .cdw10(cns)
            .prp1(self.staging.phys());
        self.admin_cmd(cmd)
    }

    /// Transfer `len` bytes (multiple of 512, at most [`STAGING_SIZE`])
    /// between the staging buffer and the namespace at `lba`.
    fn transfer(&mut self, opcode: u8, lba: u64, len: usize) -> Result<(), NvmeError> {
        let staging_phys = self.staging.phys();
        let npages = len.div_ceil(4096);
        let (prp1, prp2) = if npages <= 2 {
            (
                staging_phys,
                if npages == 2 {
                    staging_phys + NVME_PAGE_SIZE
                } else {
                    0
                },
            )
        } else {
            // More than two pages: PRP2 points at a list of the remaining
            // page addresses. The transfer always starts at staging offset 0,
            // so PRP1 is page-aligned and the list covers pages 1..npages.
            let mut addr = staging_phys + NVME_PAGE_SIZE;
            for i in 0..npages - 1 {
                let dst = &mut self.prp_list.as_mut_slice()[i * 8..(i + 1) * 8];
                dst.copy_from_slice(&addr.to_le_bytes());
                addr += NVME_PAGE_SIZE;
            }
            (staging_phys, self.prp_list.phys())
        };
        let nblocks = u32::try_from(len / 512).map_err(|_| NvmeError::BadTransferLength(len))?;
        let cmd = NvmeCmd::new(opcode)
            .nsid(self.nsid)
            .cdw10((lba & 0xFFFF_FFFF) as u32)
            .cdw11((lba >> 32) as u32)
            .cdw12(nblocks - 1)
            .prp1(prp1)
            .prp2(prp2);
        self.io_cmd(cmd)
    }

    const fn check_transfer_len(len: usize) -> Result<(), NvmeError> {
        if len == 0 || len > STAGING_SIZE || !len.is_multiple_of(512) {
            return Err(NvmeError::BadTransferLength(len));
        }
        Ok(())
    }

    fn check_lba(&self, lba: u64, len: usize) -> Result<(), NvmeError> {
        let blocks = u64::try_from(len / 512).map_err(|_| NvmeError::BadTransferLength(len))?;
        if lba >= self.namespace_blocks || blocks > self.namespace_blocks - lba {
            return Err(NvmeError::LbaOutOfRange {
                lba,
                blocks: self.namespace_blocks,
            });
        }
        Ok(())
    }

    /// Read `out.len()` bytes (512-multiple, <= 1 MiB) at `lba`.
    ///
    /// # Errors
    ///
    /// Returns [`NvmeError`] if the length/LBA is invalid or the read fails.
    pub fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> Result<(), NvmeError> {
        Self::check_transfer_len(out.len())?;
        self.check_lba(lba, out.len())?;
        self.transfer(NVM_READ, lba, out.len())?;
        out.copy_from_slice(&self.staging.as_slice()[..out.len()]);
        Ok(())
    }

    /// Write `data.len()` bytes (512-multiple, <= 1 MiB) at `lba`.
    ///
    /// # Errors
    ///
    /// Returns [`NvmeError`] if the length/LBA is invalid or the write fails.
    pub fn write_blocks(&mut self, lba: u64, data: &[u8]) -> Result<(), NvmeError> {
        Self::check_transfer_len(data.len())?;
        self.check_lba(lba, data.len())?;
        self.staging.as_mut_slice()[..data.len()].copy_from_slice(data);
        self.transfer(NVM_WRITE, lba, data.len())?;
        Ok(())
    }

    /// Flush the namespace's volatile write cache.
    ///
    /// # Errors
    ///
    /// Returns [`NvmeError`] if the flush command fails.
    pub fn flush(&mut self) -> Result<(), NvmeError> {
        let cmd = NvmeCmd::new(NVM_FLUSH).nsid(self.nsid);
        self.io_cmd(cmd)
    }
}

// ---------------------------------------------------------------------------
// Tests: a fake NVMe controller behind the Mmio trait
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// Namespace size of the fake controller: 2048 blocks = 1 MiB.
    const FAKE_NS_BLOCKS: u64 = 2048;

    struct FakeQueueHalf {
        host: *mut u8,
        size: u16,
    }

    struct FakeQueue {
        sq: Option<FakeQueueHalf>,
        cq: Option<FakeQueueHalf>,
        sq_tail_seen: u16,
        cq_head: u16,
        cq_phase: bool,
    }

    struct FakeState {
        /// Register file: registers below 0x1000, doorbells above.
        regs: Vec<u8>,
        admin_sq: Option<FakeQueueHalf>,
        admin_cq: Option<FakeQueueHalf>,
        admin_sq_tail_seen: u16,
        admin_cq_head: u16,
        admin_cq_phase: bool,
        queues: HashMap<u16, FakeQueue>,
        namespace: Vec<u8>,
    }

    impl FakeState {
        fn new() -> Self {
            Self {
                regs: vec![0u8; 0x2000],
                admin_sq: None,
                admin_cq: None,
                admin_sq_tail_seen: 0,
                admin_cq_head: 0,
                admin_cq_phase: true,
                queues: HashMap::new(),
                namespace: vec![0u8; usize::try_from(FAKE_NS_BLOCKS * 512).unwrap()],
            }
        }

        fn set_reg32(&mut self, off: u64, v: u32) {
            let o = usize::try_from(off).unwrap();
            self.regs[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }

        fn reg32(&self, off: u64) -> u32 {
            let o = usize::try_from(off).unwrap();
            u32::from_le_bytes(self.regs[o..o + 4].try_into().unwrap())
        }

        /// Post a completion entry to a queue's CQ.
        fn complete(
            cq: Option<&FakeQueueHalf>,
            cq_head: &mut u16,
            cq_phase: &mut bool,
            cid: u16,
            qid: u16,
            status: u16,
        ) {
            let half = cq.as_ref().expect("fake: CQ not programmed");
            let off = usize::from(*cq_head) * CQ_ENTRY_SIZE;
            // DW2: SQ head (0) | SQ ID. DW3: CID | phase | status.
            let dw2 = u32::from(qid) << 16;
            let dw3 = u32::from(cid) | (u32::from(*cq_phase) << 16) | (u32::from(status) << 17);
            // SAFETY: the CQ buffer belongs to the driver under test; the
            // offset is inside it and 4-byte writes are alignment-safe via
            // write_unaligned on a byte pointer.
            unsafe {
                let base = half.host.add(off);
                base.add(8).cast::<u32>().write_unaligned(dw2.to_le());
                base.add(12).cast::<u32>().write_unaligned(dw3.to_le());
            }
            *cq_head += 1;
            if *cq_head == half.size {
                *cq_head = 0;
                *cq_phase = !*cq_phase;
            }
        }

        /// Gather the host pages backing a PRP-described transfer as
        /// (`address`, `len`) pairs. On the host the driver's "physical"
        /// address is the live pointer, so pages are dereferenced directly.
        fn prp_pages(prp1: u64, prp2: u64, len: usize) -> Vec<(usize, usize)> {
            let mut pages = Vec::new();
            let mut remaining = len;
            // The first page may start at a page offset (never the case for
            // our page-aligned staging buffer, but handled anyway).
            let first_page = usize::try_from(prp1).unwrap();
            let first = (4096 - (first_page % 4096)).min(remaining);
            pages.push((first_page, first));
            remaining -= first;
            if remaining == 0 {
                return pages;
            }
            if remaining <= 4096 && prp2 != 0 {
                // Two pages: PRP2 is the second page address.
                pages.push((usize::try_from(prp2).unwrap(), remaining));
                return pages;
            }
            // PRP list: little-endian u64 page addresses for the rest.
            let list = prp2 as *const u64;
            let mut i = 0usize;
            while remaining > 0 {
                // SAFETY: the list is the driver's live PRP-list buffer.
                let addr = unsafe { list.add(i).read_unaligned() };
                let take = remaining.min(4096);
                pages.push((usize::try_from(addr).unwrap(), take));
                remaining -= take;
                i += 1;
            }
            pages
        }

        fn xfer_pages(pages: &[(usize, usize)], ns: &mut [u8], ns_off: usize, to_ns: bool) {
            let mut off = ns_off;
            for &(addr, len) in pages {
                // SAFETY: the pages are the driver's live staging/PRP buffers.
                let page = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len) };
                if to_ns {
                    ns[off..off + len].copy_from_slice(&page[..len]);
                } else {
                    page[..len].copy_from_slice(&ns[off..off + len]);
                }
                off += len;
            }
        }

        fn exec_admin(&mut self, entry: &[u8; 64]) {
            let opcode = entry[0];
            let cid = u16::from_le_bytes([entry[2], entry[3]]);
            let nsid = u32::from_le_bytes(entry[4..8].try_into().unwrap());
            let prp1 = u64::from_le_bytes(entry[24..32].try_into().unwrap());
            let cdw10 = u32::from_le_bytes(entry[40..44].try_into().unwrap());
            let cdw11 = u32::from_le_bytes(entry[44..48].try_into().unwrap());
            let mut status = 0u16;
            match opcode {
                ADMIN_IDENTIFY => {
                    // SAFETY: PRP1 is the driver's live 4 KiB staging buffer.
                    let buf = unsafe { core::slice::from_raw_parts_mut(prp1 as *mut u8, 4096) };
                    buf.fill(0);
                    if cdw10 == IDENTIFY_CNS_CONTROLLER {
                        buf[516..520].copy_from_slice(&1u32.to_le_bytes()); // NN = 1
                        buf[24..64].copy_from_slice(b"QEMU NVMe Ctrl                          "); // MN
                        buf[4..24]
                            .copy_from_slice(b"FAKESERIAL123         "[..20].try_into().unwrap()); // SN
                    } else {
                        buf[0..8].copy_from_slice(&FAKE_NS_BLOCKS.to_le_bytes()); // NSZE
                        buf[26] = 0; // FLBAS: format 0
                        buf[128 + 2..128 + 4].copy_from_slice(&9u16.to_le_bytes()); // LBADS = 9
                    }
                    let _ = nsid;
                }
                ADMIN_CREATE_IO_CQ => {
                    let qid = (cdw10 & 0xffff) as u16;
                    let size = u16::try_from((cdw10 >> 16) + 1).unwrap();
                    let q = self.queues.entry(qid).or_insert(FakeQueue {
                        sq: None,
                        cq: None,
                        sq_tail_seen: 0,
                        cq_head: 0,
                        cq_phase: true,
                    });
                    q.cq = Some(FakeQueueHalf {
                        host: prp1 as *mut u8,
                        size,
                    });
                }
                ADMIN_CREATE_IO_SQ => {
                    let qid = (cdw10 & 0xffff) as u16;
                    let size = u16::try_from((cdw10 >> 16) + 1).unwrap();
                    let q = self.queues.entry(qid).or_insert(FakeQueue {
                        sq: None,
                        cq: None,
                        sq_tail_seen: 0,
                        cq_head: 0,
                        cq_phase: true,
                    });
                    q.sq = Some(FakeQueueHalf {
                        host: prp1 as *mut u8,
                        size,
                    });
                    let _ = cdw11;
                }
                _ => status = 1, // invalid opcode
            }
            Self::complete(
                self.admin_cq.as_ref(),
                &mut self.admin_cq_head,
                &mut self.admin_cq_phase,
                cid,
                0,
                status,
            );
        }

        fn exec_io(&mut self, qid: u16, entry: &[u8; 64]) {
            let opcode = entry[0];
            let cid = u16::from_le_bytes([entry[2], entry[3]]);
            let prp1 = u64::from_le_bytes(entry[24..32].try_into().unwrap());
            let prp2 = u64::from_le_bytes(entry[32..40].try_into().unwrap());
            let cdw10 = u32::from_le_bytes(entry[40..44].try_into().unwrap());
            let cdw11 = u32::from_le_bytes(entry[44..48].try_into().unwrap());
            let cdw12 = u32::from_le_bytes(entry[48..52].try_into().unwrap());
            let mut status = 0u16;
            match opcode {
                NVM_READ | NVM_WRITE => {
                    let lba = u64::from(cdw10) | (u64::from(cdw11) << 32);
                    let nblocks = u64::from(cdw12 & 0xffff) + 1;
                    let len = usize::try_from(nblocks * 512).unwrap();
                    let ns_off = usize::try_from(lba * 512).unwrap();
                    if ns_off + len <= self.namespace.len() {
                        let pages = Self::prp_pages(prp1, prp2, len);
                        Self::xfer_pages(&pages, &mut self.namespace, ns_off, opcode == NVM_WRITE);
                    } else {
                        status = 0x80; // LBA out of range
                    }
                }
                NVM_FLUSH => {}
                _ => status = 1,
            }
            let q = self.queues.get_mut(&qid).expect("fake: unknown IO queue");
            let (cq, mut head, mut phase) = (q.cq.take(), q.cq_head, q.cq_phase);
            Self::complete(cq.as_ref(), &mut head, &mut phase, cid, qid, status);
            let q = self.queues.get_mut(&qid).expect("fake: unknown IO queue");
            q.cq = cq;
            q.cq_head = head;
            q.cq_phase = phase;
        }

        /// Run newly-submitted entries on an SQ doorbell ring.
        fn ring_sq(&mut self, qid: u16, new_tail: u16) {
            // Collect the entries first: `exec_admin`/`exec_io` need
            // `&mut self` while the queue halves are borrowed from it.
            let (host, size, mut idx) = if qid == 0 {
                let half = self
                    .admin_sq
                    .as_ref()
                    .expect("fake: admin SQ not programmed");
                (half.host, half.size, self.admin_sq_tail_seen)
            } else {
                let q = self.queues.get(&qid).expect("fake: unknown IO queue");
                let half = q.sq.as_ref().expect("fake: IO SQ not programmed");
                (half.host, half.size, q.sq_tail_seen)
            };
            let mut entries = Vec::new();
            while idx != new_tail {
                let mut entry = [0u8; 64];
                // SAFETY: the SQ buffer belongs to the driver under test.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        host.add(usize::from(idx) * SQ_ENTRY_SIZE),
                        entry.as_mut_ptr(),
                        64,
                    );
                }
                entries.push(entry);
                idx = (idx + 1) % size;
            }
            for entry in &entries {
                if qid == 0 {
                    self.exec_admin(entry);
                } else {
                    self.exec_io(qid, entry);
                }
            }
            if qid == 0 {
                self.admin_sq_tail_seen = new_tail;
            } else {
                self.queues.get_mut(&qid).unwrap().sq_tail_seen = new_tail;
            }
        }
    }

    /// The fake controller: register file + synchronous command execution.
    struct FakeNvme {
        state: RefCell<FakeState>,
    }

    impl FakeNvme {
        fn new() -> Self {
            Self {
                state: RefCell::new(FakeState::new()),
            }
        }
    }

    impl Mmio for FakeNvme {
        fn read32(&self, off: u64) -> u32 {
            self.state.borrow().reg32(off)
        }

        fn read64(&self, off: u64) -> u64 {
            let s = self.state.borrow();
            let o = usize::try_from(off).unwrap();
            u64::from_le_bytes(s.regs[o..o + 8].try_into().unwrap())
        }

        fn write32(&self, off: u64, val: u32) {
            let mut s = self.state.borrow_mut();
            if off == REG_CC {
                let cur = s.reg32(REG_CSTS);
                s.set_reg32(
                    REG_CSTS,
                    if val & CC_EN != 0 {
                        cur | CSTS_RDY
                    } else {
                        cur & !CSTS_RDY
                    },
                );
                return;
            }
            if off >= REG_DOORBELL_BASE {
                // Doorbell stride 4 (CAP.DSTRD = 0).
                let dbl = (off - REG_DOORBELL_BASE) / 4;
                let qid = u16::try_from(dbl / 2).unwrap();
                let is_cq = dbl % 2 == 1;
                if !is_cq {
                    s.ring_sq(qid, u16::try_from(val & 0xFFFF).unwrap());
                }
                return;
            }
            s.set_reg32(off, val);
        }

        fn write64(&self, off: u64, val: u64) {
            let mut s = self.state.borrow_mut();
            if off == REG_ASQ {
                let aqa = s.reg32(REG_AQA);
                let size = u16::try_from((aqa & 0xffff) + 1).unwrap();
                s.admin_sq = Some(FakeQueueHalf {
                    host: val as *mut u8,
                    size,
                });
                return;
            }
            if off == REG_ACQ {
                let aqa = s.reg32(REG_AQA);
                let size = u16::try_from(((aqa >> 16) & 0xffff) + 1).unwrap();
                s.admin_cq = Some(FakeQueueHalf {
                    host: val as *mut u8,
                    size,
                });
                return;
            }
            let o = usize::try_from(off).unwrap();
            s.regs[o..o + 8].copy_from_slice(&val.to_le_bytes());
        }
    }

    fn init_fake() -> NvmeController<FakeNvme> {
        NvmeController::init(FakeNvme::new(), 1).expect("fake NVMe init")
    }

    #[test]
    fn init_captures_the_model_and_flags_qemu_as_emulated() {
        let ctl = init_fake();
        assert!(ctl.model().starts_with(b"QEMU NVMe Ctrl"));
        assert!(ctl.is_emulated());
    }

    #[test]
    fn init_captures_the_serial_number() {
        let ctl = init_fake();
        assert_eq!(ctl.serial(), b"FAKESERIAL123       ");
        assert!(ctl.serial_is("FAKESERIAL123"));
        assert!(
            ctl.serial_is("  FAKESERIAL123 "),
            "padding is ignored on both sides"
        );
        assert!(!ctl.serial_is("FAKESERIAL12"), "a prefix is not a match");
        assert!(!ctl.serial_is("fakeserial123"), "case matters");
        assert!(!ctl.serial_is(""), "an empty name never matches");
        assert!(!ctl.serial_is("   "), "nor does blank");
    }

    #[test]
    fn serial_matching_handles_nuls_and_lengths() {
        assert!(serial_matches(b"ABC\0\0\0", "ABC"));
        assert!(serial_matches(b"  ABC  ", "ABC"));
        assert!(!serial_matches(b"ABCD", "ABC"));
        assert!(!serial_matches(b"ABC", "ABCD"));
        assert!(!serial_matches(b"        ", "ABC"));
        assert!(!serial_matches(b"", "ABC"));
        assert!(!serial_matches(b"ABC", ""));
    }

    #[test]
    fn real_hardware_models_are_not_emulated() {
        for m in [
            &b"Samsung SSD 990 PRO 4TB                "[..],
            b"WDC WDS100T3X0C-00SJG0                  ",
            b"INTEL SSDPEKNW010T8                     ",
            b"                                        ",
            b"",
            b"QEM",
        ] {
            assert!(!model_is_emulated(m), "{:?}", core::str::from_utf8(m));
        }
    }

    #[test]
    fn emulated_model_tolerates_leading_padding() {
        assert!(model_is_emulated(b"  QEMU NVMe Ctrl"));
        assert!(model_is_emulated(b"QEMU HARDDISK"));
    }

    #[test]
    fn init_reports_namespace_size() {
        let ctl = init_fake();
        assert_eq!(ctl.namespace_blocks(), FAKE_NS_BLOCKS);
    }

    #[test]
    fn init_rejects_namespace_zero() {
        let err = NvmeController::init(FakeNvme::new(), 0).unwrap_err();
        assert_eq!(err, NvmeError::InvalidNamespaceId);
    }

    #[test]
    fn init_rejects_missing_namespace() {
        let err = NvmeController::init(FakeNvme::new(), 2).unwrap_err();
        assert_eq!(err, NvmeError::NamespaceNotFound { nsid: 2, count: 1 });
    }

    #[test]
    fn write_then_read_single_block() {
        let mut ctl = init_fake();
        let mut wbuf = [0u8; 512];
        for (i, b) in wbuf.iter_mut().enumerate() {
            *b = u8::try_from(i & 0xff).unwrap();
        }
        ctl.write_blocks(7, &wbuf).unwrap();
        let mut rbuf = [0u8; 512];
        ctl.read_blocks(7, &mut rbuf).unwrap();
        assert_eq!(wbuf, rbuf);
    }

    #[test]
    fn multi_page_transfer_uses_prp_list() {
        // 8 pages > 2: exercises the PRP-list path.
        let mut ctl = init_fake();
        let mut wbuf = vec![0u8; 8 * 4096];
        for (i, b) in wbuf.iter_mut().enumerate() {
            *b = u8::try_from((i * 31) & 0xff).unwrap();
        }
        ctl.write_blocks(100, &wbuf).unwrap();
        let mut rbuf = vec![0u8; 8 * 4096];
        ctl.read_blocks(100, &mut rbuf).unwrap();
        assert_eq!(wbuf, rbuf);
    }

    #[test]
    fn blocks_are_independent() {
        let mut ctl = init_fake();
        let wbuf = [0xabu8; 512];
        ctl.write_blocks(3, &wbuf).unwrap();
        let mut rbuf = [0u8; 512];
        ctl.read_blocks(4, &mut rbuf).unwrap();
        assert!(rbuf.iter().all(|&b| b == 0));
        ctl.read_blocks(3, &mut rbuf).unwrap();
        assert_eq!(rbuf, wbuf);
    }

    #[test]
    fn out_of_range_lba_is_rejected() {
        let mut ctl = init_fake();
        let buf = [0u8; 512];
        let err = ctl.write_blocks(FAKE_NS_BLOCKS, &buf).unwrap_err();
        assert_eq!(
            err,
            NvmeError::LbaOutOfRange {
                lba: FAKE_NS_BLOCKS,
                blocks: FAKE_NS_BLOCKS
            }
        );
        let mut out = [0u8; 512];
        assert!(ctl.read_blocks(FAKE_NS_BLOCKS, &mut out).is_err());
    }

    #[test]
    fn bad_lengths_rejected() {
        let mut ctl = init_fake();
        let mut out = [0u8; 100];
        assert_eq!(
            ctl.read_blocks(0, &mut out).unwrap_err(),
            NvmeError::BadTransferLength(100)
        );
        assert_eq!(
            ctl.read_blocks(0, &mut []).unwrap_err(),
            NvmeError::BadTransferLength(0)
        );
    }

    #[test]
    fn flush_succeeds() {
        let mut ctl = init_fake();
        ctl.flush().unwrap();
    }

    #[test]
    fn last_block_roundtrip() {
        // The kernel's bare-metal self-test target: the last LBA. Two
        // distinct patterns prove the fake really stores per-LBA data rather
        // than looping the staging buffer back.
        let mut ctl = init_fake();
        let last = FAKE_NS_BLOCKS - 1;
        let mut wbuf = [0u8; 512];
        for (i, b) in wbuf.iter_mut().enumerate() {
            *b = u8::try_from((i ^ 0x5a) & 0xff).unwrap();
        }
        ctl.write_blocks(last, &wbuf).unwrap();
        ctl.flush().unwrap();
        let other = [0x33u8; 512];
        ctl.write_blocks(0, &other).unwrap();
        let mut rbuf = [0u8; 512];
        ctl.read_blocks(last, &mut rbuf).unwrap();
        assert_eq!(wbuf, rbuf);
        ctl.read_blocks(0, &mut rbuf).unwrap();
        assert_eq!(other, rbuf);
    }
}

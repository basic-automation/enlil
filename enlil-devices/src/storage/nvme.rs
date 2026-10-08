//! `NVMe` namespace passthrough via VFIO.
//!
//! [`NvmePassthroughBackend`] binds a physical `NVMe` controller to VFIO, drives
//! it directly (admin queue + one I/O queue pair, polled completions), and
//! exposes one of its namespaces as a [`StorageBackend`] — so a guest gets
//! near-native storage performance with no emulation in the data path.
//!
//! The backend is IOMMU-gated: [`NvmePassthroughBackend::open`] refuses to
//! bind the device when the host IOMMU is unavailable, when the controller is
//! not in an IOMMU group, or when it is not bound to `vfio-pci`.

use std::path::Path;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::debug;

use crate::storage::StorageBackend;
use crate::storage::vfio::{
    self, DmaMapping, MmapRegion, Mmio, VfioContainer, VfioGroup, VfioPciDevice,
};
use crate::truncate::{Widen, u32_of, usize_of};

// ---------------------------------------------------------------------------
// NVMe register / command constants (NVM Express 1.4, section 3.1)
// ---------------------------------------------------------------------------

/// Controller registers (BAR0 offsets).
const REG_CAP: u64 = 0x00;
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1c;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;
/// Doorbell registers start here; stride comes from `CAP.DSTRD`.
const REG_DOORBELL_BASE: u64 = 0x1000;

/// `CC.EN`: controller enable.
const CC_EN: u32 = 1 << 0;
/// `CSTS.RDY`: controller ready.
const CSTS_RDY: u32 = 1 << 0;

/// Admin opcodes.
const ADMIN_CREATE_IO_SQ: u8 = 0x01;
const ADMIN_CREATE_IO_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
/// NVM command-set opcodes.
const NVM_FLUSH: u8 = 0x00;
const NVM_WRITE: u8 = 0x01;
const NVM_READ: u8 = 0x02;
const NVM_DATASET_MANAGEMENT: u8 = 0x09;

/// Identify "controller" data structure.
const IDENTIFY_CNS_CONTROLLER: u32 = 1;
/// Identify "namespace" data structure.
const IDENTIFY_CNS_NAMESPACE: u32 = 0;
/// `ONCS` bit: Dataset Management supported.
const ONCS_DSM: u16 = 1 << 2;
/// `DSM` attribute bit: deallocate.
const DSM_ATTR_DEALLOCATE: u32 = 1 << 2;

const SQ_ENTRY_SIZE: usize = 64;
const CQ_ENTRY_SIZE: usize = 16;
const NVME_PAGE_SIZE: u64 = 4096;
/// Data staging buffer: one IOMMU-mapped megabyte; transfers are chunked to it.
const STAGING_SIZE: usize = 1024 * 1024;
const ADMIN_QUEUE_SIZE: u16 = 64;
const IO_QUEUE_SIZE: u16 = 256;
/// Base of our IOVA window. Well above the low 4 GiB so it cannot collide
/// with 32-bit device DMA windows.
const IOVA_BASE: u64 = 0x1_0000_0000;
/// How long to wait for controller enable/disable.
const INIT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for one command completion.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// DMA buffers
// ---------------------------------------------------------------------------

/// A page-aligned DMA buffer, optionally mapped into a VFIO container's IOMMU.
///
/// The IOMMU mapping (if any) is declared before the allocation so it is
/// dropped — i.e. unmapped — before the pages are freed.
pub struct DmaBuf {
    mapping: Option<DmaMapping>,
    ptr: NonNull<u8>,
    len: usize,
    iova: u64,
}

// SAFETY: exclusive ownership of the allocation; the IOMMU mapping cannot
// outlive it because `mapping` drops first.
unsafe impl Send for DmaBuf {}
unsafe impl Sync for DmaBuf {}

impl DmaBuf {
    fn alloc_pages(size: usize) -> Result<NonNull<u8>> {
        let len = size.next_multiple_of(4096).max(4096);
        let mut ptr: *mut libc::c_void = std::ptr::null_mut();
        // SAFETY: alignment is a non-zero power of two; on success `ptr` is
        // freed with `libc::free` in `Drop`.
        let r = unsafe { libc::posix_memalign(std::ptr::addr_of_mut!(ptr), 4096, len) };
        if r != 0 {
            bail!("posix_memalign({len}) failed: {r}");
        }
        let ptr = NonNull::new(ptr.cast::<u8>()).context("posix_memalign returned null")?;
        // SAFETY: the fresh pages are ours for `len` bytes.
        unsafe {
            std::ptr::write_bytes(ptr.as_ptr(), 0, len);
        }
        Ok(ptr)
    }

    /// Allocate `size` bytes (rounded up to a page multiple) and map them at
    /// `iova` in the container's IOMMU.
    ///
    /// # Errors
    ///
    /// Returns an error if the allocation or the IOMMU mapping fails.
    pub fn new(container: &VfioContainer, size: usize, iova: u64, writable: bool) -> Result<Self> {
        let len = size.next_multiple_of(4096).max(4096);
        let ptr = Self::alloc_pages(len)?;
        // SAFETY: `ptr` is page-aligned, `len` is a page multiple, and the
        // mapping is dropped before the pages are freed (field order).
        let mapping = unsafe { container.map_dma(iova, ptr.as_ptr(), len, writable)? };
        Ok(Self {
            mapping: Some(mapping),
            ptr,
            len,
            iova,
        })
    }

    /// Allocate page-aligned DMA memory without an IOMMU mapping. Used by
    /// tests and by drivers that manage IOMMU mapping themselves.
    ///
    /// # Errors
    ///
    /// Returns an error if the allocation fails.
    pub fn new_unmapped(size: usize, iova: u64) -> Result<Self> {
        let len = size.next_multiple_of(4096).max(4096);
        Ok(Self {
            mapping: None,
            ptr: Self::alloc_pages(len)?,
            len,
            iova,
        })
    }

    /// I/O virtual address of this buffer (what the device sees).
    #[must_use]
    pub const fn iova(&self) -> u64 {
        self.iova
    }

    /// Length in bytes (page multiple).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer is empty (never true: length is at least one page).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Raw pointer to the buffer (what the CPU sees).
    #[must_use]
    pub const fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Mutable raw pointer to the buffer.
    #[must_use]
    pub const fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// The buffer as a byte slice.
    #[must_use]
    pub const fn as_slice(&self) -> &[u8] {
        // SAFETY: the allocation is valid for `len` bytes and exclusively owned.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The buffer as a mutable byte slice.
    #[must_use]
    pub const fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as for `as_slice`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        // Unmap from the IOMMU before freeing the pages.
        self.mapping.take();
        // SAFETY: allocated by `posix_memalign` in `alloc_pages`.
        unsafe {
            libc::free(self.ptr.as_ptr().cast::<libc::c_void>());
        }
    }
}

// ---------------------------------------------------------------------------
// NVMe submission-queue entries
// ---------------------------------------------------------------------------

/// A 64-byte `NVMe` submission queue entry, little-endian.
#[derive(Clone, Copy)]
struct NvmeCmd([u8; 64]);

impl NvmeCmd {
    const fn new(opcode: u8, cid: u16) -> Self {
        let mut bytes = [0u8; 64];
        bytes[0] = opcode;
        let cid = cid.to_le_bytes();
        bytes[2] = cid[0];
        bytes[3] = cid[1];
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
/// The driver keeps a single command outstanding per queue (submit, then poll
/// for its completion), so no SQ-full tracking is needed.
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
    const fn new(qid: u16, size: u16, db_stride: u64, sq: DmaBuf, cq: DmaBuf) -> Self {
        Self {
            sq,
            cq,
            qid,
            size,
            db_stride,
            sq_tail: 0,
            cq_head: 0,
            // Completion queues start with phase tag 1 (NVMe 1.4 §4.6).
            cq_phase: true,
            next_cid: 0,
        }
    }

    const fn sq_doorbell(&self) -> u64 {
        REG_DOORBELL_BASE + 2 * self.qid as u64 * self.db_stride
    }

    const fn cq_doorbell(&self) -> u64 {
        REG_DOORBELL_BASE + (2 * self.qid as u64 + 1) * self.db_stride
    }

    /// Copy `cmd` into the next submission slot, ring the doorbell, and return
    /// the command identifier. The CID is stamped into the entry's CID field:
    /// the device echoes the entry's CID back in the completion.
    fn submit(&mut self, mmio: &dyn Mmio, cmd: &NvmeCmd) -> u16 {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        let mut entry = *cmd.bytes();
        entry[2..4].copy_from_slice(&cid.to_le_bytes());
        let off = usize::from(self.sq_tail) * SQ_ENTRY_SIZE;
        // SAFETY: `sq_tail < size`, so the 64-byte slot is inside the buffer.
        unsafe {
            std::ptr::copy_nonoverlapping(entry.as_ptr(), self.sq.as_mut_ptr().add(off), 64);
        }
        self.sq_tail = (self.sq_tail + 1) % self.size;
        mmio.write32(self.sq_doorbell(), u32::from(self.sq_tail));
        cid
    }

    /// Poll the completion queue until an entry with the expected phase tag
    /// appears. Returns `(command_id, status)`.
    ///
    /// # Errors
    ///
    /// Returns an error if no completion arrives within `timeout`.
    fn poll(&mut self, mmio: &dyn Mmio, timeout: Duration) -> Result<(u16, u16)> {
        use crate::truncate::u16_of;
        let deadline = Instant::now() + timeout;
        loop {
            let off = usize::from(self.cq_head) * CQ_ENTRY_SIZE;
            // Read DW3 as four volatile bytes: the completion queue lives in
            // DMA memory written by the device, so the read must not be
            // hoisted out of the poll loop, and a `u32` cast would assert an
            // alignment the compiler cannot prove.
            // SAFETY: `cq_head < size`, so the four bytes are in-bounds; byte
            // reads are always aligned.
            let dw3 = unsafe {
                let base = self.cq.as_ptr().add(off + 12);
                u32::from_le_bytes([
                    base.read_volatile(),
                    base.add(1).read_volatile(),
                    base.add(2).read_volatile(),
                    base.add(3).read_volatile(),
                ])
            };
            // DW3: bits 0-15 CID, bit 16 phase tag, bits 17+ status.
            if (dw3 >> 16) & 1 == u32::from(self.cq_phase) {
                let cid = u16_of(dw3 & 0xffff);
                let status = u16_of((dw3 >> 17) & 0x7fff);
                self.cq_head = (self.cq_head + 1) % self.size;
                if self.cq_head == 0 {
                    self.cq_phase = !self.cq_phase;
                }
                mmio.write32(self.cq_doorbell(), u32::from(self.cq_head));
                return Ok((cid, status));
            }
            if Instant::now() >= deadline {
                bail!(
                    "NVMe completion timeout on queue {} after {}s",
                    self.qid,
                    timeout.as_secs()
                );
            }
            std::thread::sleep(Duration::from_micros(50));
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Spin until `condition` holds or `timeout` elapses.
///
/// # Errors
///
/// Returns an error on timeout.
fn poll_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !condition() {
        if Instant::now() >= deadline {
            bail!("timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    Ok(())
}

fn le16(data: &[u8], off: usize) -> Result<u16> {
    let b = data
        .get(off..off + 2)
        .context("NVMe identify data truncated")?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn le32(data: &[u8], off: usize) -> Result<u32> {
    let b = data
        .get(off..off + 4)
        .context("NVMe identify data truncated")?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le64(data: &[u8], off: usize) -> Result<u64> {
    let b = data
        .get(off..off + 8)
        .context("NVMe identify data truncated")?;
    Ok(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

// ---------------------------------------------------------------------------
// NVMe controller driver
// ---------------------------------------------------------------------------

/// A directly-driven `NVMe` controller: admin queue + one I/O queue pair with
/// polled completions.
///
/// `M` is the BAR0 access method — [`MmapRegion`] on real hardware, a fake in
/// tests. DMA buffers are allocated through `alloc`, which maps them into the
/// IOMMU on real hardware and just allocates in tests.
pub struct NvmeController<M: Mmio> {
    mmio: M,
    admin: QueuePair,
    io: QueuePair,
    staging: DmaBuf,
    prp_list: DmaBuf,
    nsid: u32,
    namespace_blocks: u64,
    oncs_dsm: bool,
}

impl<M: Mmio> NvmeController<M> {
    /// Take ownership of a freshly VFIO-bound `NVMe` controller: program the
    /// admin queues, enable it, identify the controller and `nsid`, and create
    /// the I/O queue pair.
    ///
    /// `alloc(size, iova, writable)` provides page-aligned DMA buffers; the
    /// driver lays them out starting at [`IOVA_BASE`].
    ///
    /// # Errors
    ///
    /// Returns an error if the controller does not become ready, the
    /// namespace does not exist or is not 512-byte formatted, or any admin
    /// command fails.
    pub fn init(
        mmio: M,
        nsid: u32,
        alloc: &mut dyn FnMut(usize, u64, bool) -> Result<DmaBuf>,
    ) -> Result<Self> {
        if nsid == 0 {
            bail!("NVMe namespace id 0 is the broadcast namespace and cannot be passed through");
        }
        let cap = mmio.read64(REG_CAP);
        let dstrd = (cap >> 32) & 0x0f;
        let db_stride = 4u64 << dstrd;

        if mmio.read32(REG_CSTS) & CSTS_RDY != 0 {
            // A previous driver left the controller enabled; shut it down
            // before reprogramming the admin queues.
            mmio.write32(REG_CC, 0);
            poll_until(INIT_TIMEOUT, || mmio.read32(REG_CSTS) & CSTS_RDY == 0)
                .context("NVMe controller did not shut down (CSTS.RDY stayed set)")?;
        }

        let mut next_iova = IOVA_BASE;
        let mut take = |size: usize, writable: bool| -> Result<DmaBuf> {
            let iova = next_iova;
            let buf = alloc(size, iova, writable)?;
            next_iova += buf.len().to_u64();
            Ok(buf)
        };

        // Queue memory is allocated back-to-back in one IOVA window, in this
        // order: admin SQ, admin CQ, I/O SQ, I/O CQ, staging, PRP list.
        let admin = QueuePair::new(
            0,
            ADMIN_QUEUE_SIZE,
            db_stride,
            take(usize::from(ADMIN_QUEUE_SIZE) * SQ_ENTRY_SIZE, true)?,
            take(usize::from(ADMIN_QUEUE_SIZE) * CQ_ENTRY_SIZE, true)?,
        );
        let io = QueuePair::new(
            1,
            IO_QUEUE_SIZE,
            db_stride,
            take(usize::from(IO_QUEUE_SIZE) * SQ_ENTRY_SIZE, true)?,
            take(usize::from(IO_QUEUE_SIZE) * CQ_ENTRY_SIZE, true)?,
        );
        let staging = take(STAGING_SIZE, true)?;
        let prp_list = take(4096, true)?;

        let mut ctrl = Self {
            mmio,
            admin,
            io,
            staging,
            prp_list,
            nsid,
            namespace_blocks: 0,
            oncs_dsm: false,
        };

        // Program the admin queue pair: queue size is 0's-based in AQA.
        let qsize = u32::from(ADMIN_QUEUE_SIZE - 1);
        ctrl.mmio.write32(REG_AQA, qsize | (qsize << 16));
        ctrl.mmio.write64(REG_ASQ, ctrl.admin.sq.iova());
        ctrl.mmio.write64(REG_ACQ, ctrl.admin.cq.iova());

        // Enable: NVM command set, 4 KiB memory pages, 64-byte SQ entries,
        // 16-byte CQ entries.
        ctrl.mmio.write32(REG_CC, CC_EN | (6 << 16) | (4 << 20));
        poll_until(INIT_TIMEOUT, || ctrl.mmio.read32(REG_CSTS) & CSTS_RDY != 0)
            .context("NVMe controller did not become ready (CSTS.RDY never set)")?;

        // Identify the controller: namespace count + optional command support.
        ctrl.identify(IDENTIFY_CNS_CONTROLLER, 0)?;
        let data = ctrl.staging.as_slice();
        let nn = le32(data, 516)?;
        let oncs = le16(data, 520)?;
        if u64::from(nsid) > u64::from(nn) {
            bail!("NVMe controller reports {nn} namespace(s); namespace {nsid} does not exist");
        }
        ctrl.oncs_dsm = oncs & ONCS_DSM != 0;

        // Identify the namespace: size and LBA format. The block layer above
        // speaks 512-byte sectors, so only 512-byte namespaces are supported.
        ctrl.identify(IDENTIFY_CNS_NAMESPACE, nsid)?;
        let data = ctrl.staging.as_slice();
        let nsze = le64(data, 0)?;
        let flbas = *data.get(26).context("identify namespace data truncated")?;
        let lbads = le16(data, 128 + usize::from(flbas & 0x0f) * 16 + 2)?;
        if lbads != 9 {
            let bytes = 1u64 << lbads;
            bail!(
                "NVMe namespace {nsid} is formatted with {bytes}-byte sectors; \
                 only 512-byte sectors are supported for passthrough"
            );
        }
        if nsze == 0 {
            bail!("NVMe namespace {nsid} has zero size");
        }
        ctrl.namespace_blocks = nsze;

        // Create I/O completion queue 1: physically contiguous, no interrupts
        // (completions are polled).
        let qsize = u32::from(IO_QUEUE_SIZE - 1);
        let cmd = NvmeCmd::new(ADMIN_CREATE_IO_CQ, 0)
            .cdw10(qsize | (1 << 16))
            .cdw11(1) // PC = 1
            .prp1(ctrl.io.cq.iova());
        ctrl.admin_cmd(cmd)?;

        // Create I/O submission queue 1 on completion queue 1.
        let cmd = NvmeCmd::new(ADMIN_CREATE_IO_SQ, 0)
            .cdw10(qsize | (1 << 16))
            .cdw11(1 | (1 << 16)) // PC = 1, CQID = 1
            .prp1(ctrl.io.sq.iova());
        ctrl.admin_cmd(cmd)?;

        Ok(ctrl)
    }

    /// Number of 512-byte blocks in the namespace.
    #[must_use]
    pub const fn namespace_blocks(&self) -> u64 {
        self.namespace_blocks
    }

    /// Submit an admin command and wait for its completion.
    fn admin_cmd(&mut self, cmd: NvmeCmd) -> Result<()> {
        let cid = self.admin.submit(&self.mmio, &cmd);
        let (got, status) = self.admin.poll(&self.mmio, IO_TIMEOUT)?;
        if got != cid {
            bail!("NVMe admin completion CID mismatch: submitted {cid}, got {got}");
        }
        if status != 0 {
            bail!(
                "NVMe admin command {:#04x} failed with status {:#06x}",
                cmd.opcode(),
                status
            );
        }
        Ok(())
    }

    /// Submit an I/O command and wait for its completion.
    fn io_cmd(&mut self, cmd: NvmeCmd) -> Result<()> {
        let cid = self.io.submit(&self.mmio, &cmd);
        let (got, status) = self.io.poll(&self.mmio, IO_TIMEOUT)?;
        if got != cid {
            bail!("NVMe I/O completion CID mismatch: submitted {cid}, got {got}");
        }
        if status != 0 {
            bail!(
                "NVMe I/O command {:#04x} failed with status {:#06x}",
                cmd.opcode(),
                status
            );
        }
        Ok(())
    }

    /// Run Identify (`cns`) into the staging buffer.
    fn identify(&mut self, cns: u32, nsid: u32) -> Result<()> {
        let cmd = NvmeCmd::new(ADMIN_IDENTIFY, 0)
            .nsid(nsid)
            .cdw10(cns)
            .prp1(self.staging.iova());
        self.admin_cmd(cmd)
    }

    /// Transfer `len` bytes (multiple of 512, at most [`STAGING_SIZE`])
    /// between the staging buffer and the namespace at `lba`.
    fn transfer(&mut self, opcode: u8, lba: u64, len: usize) -> Result<()> {
        let staging_iova = self.staging.iova();
        let npages = len.div_ceil(4096);
        let (prp1, prp2) = if npages <= 2 {
            (
                staging_iova,
                if npages == 2 {
                    staging_iova + NVME_PAGE_SIZE
                } else {
                    0
                },
            )
        } else {
            // More than two pages: PRP2 points at a list of the remaining
            // page addresses. The transfer always starts at staging offset 0,
            // so PRP1 is page-aligned and the list covers pages 1..npages.
            for (i, page) in (1..npages).enumerate() {
                let addr = staging_iova + page.to_u64() * NVME_PAGE_SIZE;
                let dst = &mut self.prp_list.as_mut_slice()[i * 8..(i + 1) * 8];
                dst.copy_from_slice(&addr.to_le_bytes());
            }
            (staging_iova, self.prp_list.iova())
        };
        let nblocks = u32::try_from(len / 512).context("transfer exceeds u32 blocks")?;
        let cmd = NvmeCmd::new(opcode, 0)
            .nsid(self.nsid)
            .cdw10(u32_of(lba))
            .cdw11(u32_of(lba >> 32))
            .cdw12(nblocks - 1)
            .prp1(prp1)
            .prp2(prp2);
        self.io_cmd(cmd)
    }

    fn check_transfer_len(len: usize) -> Result<()> {
        if len == 0 || len > STAGING_SIZE || !len.is_multiple_of(512) {
            bail!("NVMe transfer must be a non-zero multiple of 512 bytes up to 1 MiB, got {len}");
        }
        Ok(())
    }

    /// Read `out.len()` bytes (512-multiple, ≤ 1 MiB) at `lba`.
    ///
    /// # Errors
    ///
    /// Returns an error if the length is invalid or the read fails.
    pub fn read_blocks(&mut self, lba: u64, out: &mut [u8]) -> Result<()> {
        Self::check_transfer_len(out.len())?;
        self.transfer(NVM_READ, lba, out.len())?;
        out.copy_from_slice(&self.staging.as_slice()[..out.len()]);
        Ok(())
    }

    /// Write `data.len()` bytes (512-multiple, ≤ 1 MiB) at `lba`.
    ///
    /// # Errors
    ///
    /// Returns an error if the length is invalid or the write fails.
    pub fn write_blocks(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        Self::check_transfer_len(data.len())?;
        self.staging.as_mut_slice()[..data.len()].copy_from_slice(data);
        self.transfer(NVM_WRITE, lba, data.len())?;
        Ok(())
    }

    /// Flush the namespace's volatile write cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the flush command fails.
    pub fn flush(&mut self) -> Result<()> {
        let cmd = NvmeCmd::new(NVM_FLUSH, 0).nsid(self.nsid);
        self.io_cmd(cmd)
    }

    /// Advise the device that `[lba, lba + nblocks)` is no longer needed
    /// (Dataset Management, deallocate). Best-effort: failures are ignored —
    /// the data is still readable, just not deallocated.
    pub fn deallocate(&mut self, lba: u64, nblocks: u64) {
        if !self.oncs_dsm {
            return;
        }
        let Ok(nlb) = u32::try_from(nblocks) else {
            return;
        };
        // One 16-byte range descriptor in the staging buffer.
        let s = self.staging.as_mut_slice();
        s[0..8].copy_from_slice(&lba.to_le_bytes());
        s[8..12].copy_from_slice(&nlb.to_le_bytes());
        s[12..16].fill(0);
        let cmd = NvmeCmd::new(NVM_DATASET_MANAGEMENT, 0)
            .nsid(self.nsid)
            .cdw10(0) // NR = 0: one range
            .cdw11(DSM_ATTR_DEALLOCATE)
            .prp1(self.staging.iova());
        if let Err(e) = self.io_cmd(cmd) {
            debug!("NVMe deallocate failed (best-effort): {e:#}");
        }
    }
}

// ---------------------------------------------------------------------------
// Passthrough storage backend
// ---------------------------------------------------------------------------

struct BackendState {
    // The Arcs inside the DMA mappings keep the container alive until every
    // mapping is unmapped, so field drop order needs no special care.
    _container: VfioContainer,
    _group: VfioGroup,
    _device: VfioPciDevice,
    controller: NvmeController<MmapRegion>,
}

/// A [`StorageBackend`] that passes I/O straight through to a physical `NVMe`
/// namespace via VFIO.
///
/// The guest (or `VirtioBlockDevice` above this backend) sees a plain
/// 512-byte-sector disk; every read/write is submitted as an `NVMe` command to
/// the real controller, with data bounced through one IOMMU-mapped staging
/// buffer.
pub struct NvmePassthroughBackend {
    state: Mutex<BackendState>,
    capacity_bytes: u64,
    readonly: bool,
    label: String,
}

impl NvmePassthroughBackend {
    /// Bind the `NVMe` controller at `bdf` (`dddd:bb:dd.f`) via VFIO and expose
    /// `namespace_id` (usually 1) as a block device.
    ///
    /// This is IOMMU-gated: it fails when the host IOMMU is unavailable
    /// (`/sys/kernel/iommu_groups` missing or empty), when the controller is
    /// not in an IOMMU group, when it is not an `NVMe` controller, or when it is
    /// not bound to `vfio-pci`.
    ///
    /// # Errors
    ///
    /// Returns an error describing exactly which gate failed, or if VFIO
    /// setup / `NVMe` controller initialization fails.
    pub fn open(bdf: &str, namespace_id: u32, readonly: bool) -> Result<Self> {
        vfio::validate_bdf(bdf)?;
        if !vfio::iommu_available() {
            bail!(
                "NVMe passthrough requires an IOMMU, but none is available: \
                 /sys/kernel/iommu_groups is missing or empty. Boot with \
                 intel_iommu=on (or amd_iommu=on) and make sure the NVMe \
                 controller sits in its own IOMMU group."
            );
        }
        let group_id = vfio::device_iommu_group(bdf)
            .with_context(|| format!("NVMe controller {bdf} is not in an IOMMU group"))?;
        let class =
            vfio::device_class(bdf).with_context(|| format!("reading PCI class of {bdf}"))?;
        if class != vfio::PCI_CLASS_NVME {
            bail!("PCI device {bdf} has class {class:#08x}, not an NVMe controller (0x010802)");
        }
        match vfio::device_driver(bdf).with_context(|| format!("reading driver of {bdf}"))? {
            Some(driver) if driver == "vfio-pci" => {}
            Some(driver) => bail!(
                "PCI device {bdf} is bound to driver {driver:?}, not vfio-pci. Unbind it \
                 (echo {bdf} > /sys/bus/pci/devices/{bdf}/driver/unbind) and bind vfio-pci; \
                 every device in IOMMU group {group_id} must be bound to vfio-pci or unbound."
            ),
            None => bail!(
                "PCI device {bdf} has no driver bound; bind it to vfio-pci before passthrough \
                 (echo {bdf} > /sys/bus/pci/drivers/vfio-pci/bind, after unbinding any driver \
                 and adding the device ID to vfio-pci if needed)."
            ),
        }
        for path in ["/dev/vfio/vfio", &format!("/dev/vfio/{group_id}")] {
            if !Path::new(path).exists() {
                bail!("{path} is missing: load the vfio, vfio_iommu_type1 and vfio-pci modules");
            }
        }

        let container = VfioContainer::open().context("opening VFIO container")?;
        let group = VfioGroup::open(group_id).context("opening VFIO IOMMU group")?;
        group.set_container(&container)?;
        let device = group
            .device(bdf)
            .with_context(|| format!("getting VFIO device fd for {bdf}"))?;
        device.reset().context("resetting the NVMe controller")?;

        // A reset clears PCI config: re-enable memory decoding + bus mastering
        // (the device must DMA), and mask legacy INTx while we poll
        // completions instead of using interrupts.
        let pci_cmd = device.config_read16(0x04)?;
        device.config_write16(0x04, pci_cmd | (1 << 1) | (1 << 2) | (1 << 10))?;

        // BAR0 holds the NVMe controller registers.
        let bar0 = device.region(vfio::VFIO_PCI_BAR0_REGION_INDEX)?;
        if !bar0.mmapable() {
            bail!("NVMe BAR0 of {bdf} is not mmapable");
        }
        if bar0.size < 0x4000 {
            bail!(
                "NVMe BAR0 of {bdf} is {:#x} bytes, smaller than the register + doorbell window",
                bar0.size
            );
        }
        let bar = device.mmap_region(&bar0)?;

        let mut alloc =
            |size: usize, iova: u64, writable: bool| DmaBuf::new(&container, size, iova, writable);
        let controller = NvmeController::init(bar, namespace_id, &mut alloc)
            .with_context(|| format!("initializing NVMe controller {bdf}"))?;
        let capacity_bytes = controller.namespace_blocks() * 512;

        Ok(Self {
            state: Mutex::new(BackendState {
                _container: container,
                _group: group,
                _device: device,
                controller,
            }),
            capacity_bytes,
            readonly,
            label: format!("nvme-{bdf}-ns{namespace_id}"),
        })
    }

    /// Human-readable device label, e.g. `nvme-0000:01:00.0-ns1`.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

impl StorageBackend for NvmePassthroughBackend {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if offset >= self.capacity_bytes {
            return Ok(0);
        }
        let len = buf.len().min(usize_of(self.capacity_bytes - offset));
        if len == 0 {
            return Ok(0);
        }
        if !offset.is_multiple_of(512) || !len.is_multiple_of(512) {
            bail!("NVMe passthrough requires 512-byte aligned I/O (offset={offset}, len={len})");
        }
        let mut state = self
            .state
            .lock()
            .map_err(|e| anyhow::anyhow!("backend lock poisoned: {e}"))?;
        let mut done = 0;
        while done < len {
            let chunk = (len - done).min(STAGING_SIZE);
            let lba = (offset + done.to_u64()) / 512;
            state
                .controller
                .read_blocks(lba, &mut buf[done..done + chunk])?;
            done += chunk;
        }
        drop(state);
        Ok(len)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        if self.readonly {
            bail!("backend is read-only");
        }
        if offset >= self.capacity_bytes {
            return Ok(0);
        }
        let len = buf.len().min(usize_of(self.capacity_bytes - offset));
        if len == 0 {
            return Ok(0);
        }
        if !offset.is_multiple_of(512) || !len.is_multiple_of(512) {
            bail!("NVMe passthrough requires 512-byte aligned I/O (offset={offset}, len={len})");
        }
        let mut state = self
            .state
            .lock()
            .map_err(|e| anyhow::anyhow!("backend lock poisoned: {e}"))?;
        let mut done = 0;
        while done < len {
            let chunk = (len - done).min(STAGING_SIZE);
            let lba = (offset + done.to_u64()) / 512;
            state
                .controller
                .write_blocks(lba, &buf[done..done + chunk])?;
            done += chunk;
        }
        drop(state);
        Ok(len)
    }

    fn flush(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|e| anyhow::anyhow!("backend lock poisoned: {e}"))?;
        state.controller.flush()
    }

    fn trim(&self, offset: u64, len: u64) -> Result<()> {
        if self.readonly || len == 0 || !offset.is_multiple_of(512) || !len.is_multiple_of(512) {
            return Ok(());
        }
        let end = offset.saturating_add(len).min(self.capacity_bytes);
        if offset >= end {
            return Ok(());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|e| anyhow::anyhow!("backend lock poisoned: {e}"))?;
        // Dataset Management takes a u32 block count per range.
        let mut lba = offset / 512;
        let mut remaining = (end - offset) / 512;
        while remaining > 0 {
            let n = remaining.min(u64::from(u32::MAX));
            state.controller.deallocate(lba, n);
            lba += n;
            remaining -= n;
        }
        drop(state);
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.capacity_bytes
    }

    fn is_readonly(&self) -> bool {
        self.readonly
    }
}

// ---------------------------------------------------------------------------
// Tests: a fake NVMe controller behind the Mmio trait
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::truncate::u16_of;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// Namespace size of the fake controller: 2048 blocks = 1 MiB.
    const FAKE_NS_BLOCKS: u64 = 2048;

    struct FakeQueue {
        sq_addr: usize,
        cq_addr: usize,
        size: u16,
        cq_head: u16,
        cq_phase: bool,
    }

    struct FakeState {
        /// Register file for offsets < 0x1000, byte-addressed.
        regs: Vec<u8>,
        controller_enabled: bool,
        csts_reads: u32,
        admin_sq_iova: Option<u64>,
        admin_cq_iova: Option<u64>,
        queues: HashMap<u16, FakeQueue>,
        /// (`iova_base`, `host_addr`, `len`) for IOVA translation.
        iova_map: Vec<(u64, usize, usize)>,
        namespace: Vec<u8>,
        /// Formatted LBA data size (9 = 512 bytes).
        lbads: u16,
        doorbells: Vec<(u64, u32)>,
    }

    impl FakeState {
        fn new() -> Self {
            Self {
                regs: vec![0u8; 0x1000],
                controller_enabled: false,
                csts_reads: 0,
                admin_sq_iova: None,
                admin_cq_iova: None,
                queues: HashMap::new(),
                iova_map: Vec::new(),
                namespace: vec![0u8; usize_of(FAKE_NS_BLOCKS * 512)],
                lbads: 9,
                doorbells: Vec::new(),
            }
        }

        fn reg32(&self, off: u64) -> u32 {
            let o = usize_of(off);
            u32::from_le_bytes(self.regs[o..o + 4].try_into().unwrap())
        }

        fn set_reg32(&mut self, off: u64, v: u32) {
            let o = usize_of(off);
            self.regs[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }

        fn set_reg64(&mut self, off: u64, v: u64) {
            let o = usize_of(off);
            self.regs[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }

        fn translate(&self, iova: u64, len: usize) -> usize {
            self.iova_map
                .iter()
                .find(|(base, _, l)| iova >= *base && iova + len.to_u64() <= base + l.to_u64())
                .map(|(base, addr, _)| addr + usize_of(iova - base))
                .expect("fake: IOVA not mapped")
        }

        /// Post a completion entry to queue `qid`.
        fn complete(&mut self, qid: u16, cid: u16, status: u16) {
            let q = self.queues.get_mut(&qid).expect("fake: unknown queue");
            let off = usize::from(q.cq_head) * CQ_ENTRY_SIZE;
            // DW2: SQ head pointer (0) | SQ ID. DW3: CID | phase | status.
            let dw2 = u32::from(qid) << 16;
            let dw3 = u32::from(cid)
                | (u32::from(u8::from(q.cq_phase)) << 16)
                | (u32::from(status) << 17);
            let base = q.cq_addr as *mut u8;
            // SAFETY: the CQ buffer is ours; entries are 16-byte aligned.
            unsafe {
                base.add(off + 8).cast::<u32>().write_unaligned(dw2.to_le());
                base.add(off + 12)
                    .cast::<u32>()
                    .write_unaligned(dw3.to_le());
            }
            q.cq_head += 1;
            if q.cq_head == q.size {
                q.cq_head = 0;
                q.cq_phase = !q.cq_phase;
            }
        }

        /// Gather the (`host_addr`, `len`) pages backing a transfer described by
        /// PRP1/PRP2. The driver always starts page-aligned at staging offset
        /// 0, like real usage here.
        fn data_pages(&self, prp1: u64, prp2: u64, total: usize) -> Vec<(usize, usize)> {
            let mut pages = Vec::new();
            let mut remaining = total;
            let first = remaining.min(4096);
            pages.push((self.translate(prp1, 4096), first));
            remaining -= first;
            if remaining == 0 {
                return pages;
            }
            if total.div_ceil(4096) == 2 {
                pages.push((self.translate(prp2, 4096), remaining));
            } else {
                let list = self.translate(prp2, 4096) as *const u8;
                for i in 0..total.div_ceil(4096) - 1 {
                    // SAFETY: the PRP list page is ours.
                    let entry =
                        u64::from_le(unsafe { list.add(i * 8).cast::<u64>().read_unaligned() });
                    let take = remaining.min(4096);
                    pages.push((self.translate(entry, 4096), take));
                    remaining -= take;
                }
            }
            pages
        }

        fn write_identify(&self, cns: u32, prp1: u64) {
            let dst = self.translate(prp1, 4096) as *mut u8;
            // SAFETY: the identify buffer is one ours page.
            let buf = unsafe { std::slice::from_raw_parts_mut(dst, 4096) };
            buf.fill(0);
            match cns {
                IDENTIFY_CNS_CONTROLLER => {
                    buf[516..520].copy_from_slice(&1u32.to_le_bytes()); // NN = 1
                    buf[520..522].copy_from_slice(&ONCS_DSM.to_le_bytes()); // DSM supported
                }
                IDENTIFY_CNS_NAMESPACE => {
                    buf[0..8].copy_from_slice(&FAKE_NS_BLOCKS.to_le_bytes()); // NSZE
                    buf[26] = 0; // FLBAS: format 0
                    buf[130..132].copy_from_slice(&self.lbads.to_le_bytes()); // LBADS
                }
                _ => {}
            }
        }

        fn handle_doorbell(&mut self, offset: u64, value: u32) {
            // Fake CAP has DSTRD = 0, so the stride is 4.
            let idx = (offset - REG_DOORBELL_BASE) / 4;
            let qid = u16_of(idx / 2);
            if idx % 2 == 1 {
                return; // completion-queue doorbell: nothing to synthesize
            }
            let tail = u32_of(value);
            // Queues the fake does not know (e.g. hand-built in unit tests)
            // get their doorbell recorded but no synthesized completion.
            let Some(q) = self.queues.get(&qid) else {
                return;
            };
            let (sq_addr, size) = (q.sq_addr, u32::from(q.size));
            // The new entry is the one before the tail the driver just rang.
            let entry_off = usize_of((tail + size - 1) % size) * SQ_ENTRY_SIZE;
            // SAFETY: the SQ buffer is ours.
            let cmd: [u8; 64] = unsafe {
                std::slice::from_raw_parts((sq_addr as *const u8).add(entry_off), 64)
                    .try_into()
                    .unwrap()
            };
            let opcode = cmd[0];
            let cid = u16::from_le_bytes([cmd[2], cmd[3]]);
            let nsid = u32::from_le_bytes([cmd[4], cmd[5], cmd[6], cmd[7]]);
            let cdw10 = u32::from_le_bytes([cmd[40], cmd[41], cmd[42], cmd[43]]);
            let cdw11 = u32::from_le_bytes([cmd[44], cmd[45], cmd[46], cmd[47]]);
            let cdw12 = u32::from_le_bytes([cmd[48], cmd[49], cmd[50], cmd[51]]);
            let prp1 = u64::from_le_bytes(cmd[24..32].try_into().unwrap());
            let prp2 = u64::from_le_bytes(cmd[32..40].try_into().unwrap());

            // Admin opcodes and NVM opcodes share the number space (e.g.
            // CREATE_IO_SQ = 0x01 = NVM_WRITE); they are distinguished by
            // the queue the command was submitted on.
            if qid == 0 {
                self.handle_admin(qid, cid, opcode, cdw10, prp1);
                return;
            }
            match opcode {
                NVM_READ | NVM_WRITE => {
                    assert_eq!(nsid, 1, "fake only has namespace 1");
                    let lba = u64::from(cdw10) | (u64::from(cdw11) << 32);
                    let nlb = u64::from(cdw12 & 0xffff) + 1;
                    let total = usize_of(nlb * 512);
                    let mut ns_off = usize_of(lba * 512);
                    assert!(ns_off + total <= self.namespace.len());
                    for (addr, len) in self.data_pages(prp1, prp2, total) {
                        // SAFETY: both sides are ours.
                        unsafe {
                            if opcode == NVM_READ {
                                std::ptr::copy_nonoverlapping(
                                    self.namespace.as_ptr().add(ns_off),
                                    addr as *mut u8,
                                    len,
                                );
                            } else {
                                std::ptr::copy_nonoverlapping(
                                    addr as *const u8,
                                    self.namespace.as_mut_ptr().add(ns_off),
                                    len,
                                );
                            }
                        }
                        ns_off += len;
                    }
                    self.complete(qid, cid, 0);
                }
                NVM_FLUSH | NVM_DATASET_MANAGEMENT => {
                    self.complete(qid, cid, 0);
                }
                other => panic!("fake: unexpected NVM opcode {other:#04x}"),
            }
        }

        /// Handle a command submitted on the admin queue (`qid` is 0).
        fn handle_admin(&mut self, qid: u16, cid: u16, opcode: u8, cdw10: u32, prp1: u64) {
            match opcode {
                ADMIN_IDENTIFY => {
                    self.write_identify(cdw10, prp1);
                    self.complete(qid, cid, 0);
                }
                ADMIN_CREATE_IO_CQ => {
                    // The created queue's id is in CDW10 bits 16-31.
                    let new_qid = u16_of(cdw10 >> 16);
                    let qsize = u16_of((cdw10 & 0xffff) + 1);
                    let cq_addr = self.translate(prp1, usize::from(qsize) * CQ_ENTRY_SIZE);
                    self.queues.insert(
                        new_qid,
                        FakeQueue {
                            sq_addr: 0,
                            cq_addr,
                            size: qsize,
                            cq_head: 0,
                            cq_phase: true,
                        },
                    );
                    self.complete(qid, cid, 0);
                }
                ADMIN_CREATE_IO_SQ => {
                    let new_qid = u16_of(cdw10 >> 16);
                    let qsize = u16_of((cdw10 & 0xffff) + 1);
                    let sq_addr = self.translate(prp1, usize::from(qsize) * SQ_ENTRY_SIZE);
                    let q = self
                        .queues
                        .get_mut(&new_qid)
                        .expect("fake: CQ missing for SQ");
                    q.sq_addr = sq_addr;
                    q.size = qsize;
                    self.complete(qid, cid, 0);
                }
                other => panic!("fake: unexpected admin opcode {other:#04x}"),
            }
        }
    }

    /// Fake BAR0: register file + doorbell interception backed by `FakeState`.
    struct FakeMmio {
        state: Rc<RefCell<FakeState>>,
    }

    impl Mmio for FakeMmio {
        fn read32(&self, offset: u64) -> u32 {
            let mut st = self.state.borrow_mut();
            if offset == REG_CSTS {
                if st.controller_enabled {
                    st.csts_reads += 1;
                    // Become ready after a couple of polls, like real hardware.
                    if st.csts_reads >= 2 {
                        return CSTS_RDY;
                    }
                }
                return 0;
            }
            st.reg32(offset)
        }

        fn write32(&self, offset: u64, value: u32) {
            let mut st = self.state.borrow_mut();
            if offset == REG_CC {
                st.controller_enabled = value & CC_EN != 0;
                st.csts_reads = 0;
                st.set_reg32(offset, value);
                return;
            }
            if offset >= REG_DOORBELL_BASE {
                st.doorbells.push((offset, value));
                st.handle_doorbell(offset, value);
                return;
            }
            st.set_reg32(offset, value);
        }

        fn read64(&self, offset: u64) -> u64 {
            let st = self.state.borrow();
            let o = usize_of(offset);
            u64::from_le_bytes(st.regs[o..o + 8].try_into().unwrap())
        }

        fn write64(&self, offset: u64, value: u64) {
            let mut st = self.state.borrow_mut();
            // The admin queue pair becomes visible to the fake once both base
            // registers are programmed.
            if offset == REG_ASQ {
                st.admin_sq_iova = Some(value);
            } else if offset == REG_ACQ {
                st.admin_cq_iova = Some(value);
            } else {
                st.set_reg64(offset, value);
            }
            if let (Some(sq), Some(cq)) = (st.admin_sq_iova, st.admin_cq_iova)
                && !st.queues.contains_key(&0)
            {
                let sq_addr = st.translate(sq, usize::from(ADMIN_QUEUE_SIZE) * SQ_ENTRY_SIZE);
                let cq_addr = st.translate(cq, usize::from(ADMIN_QUEUE_SIZE) * CQ_ENTRY_SIZE);
                st.queues.insert(
                    0,
                    FakeQueue {
                        sq_addr,
                        cq_addr,
                        size: ADMIN_QUEUE_SIZE,
                        cq_head: 0,
                        cq_phase: true,
                    },
                );
            }
        }
    }

    /// Build a controller against the fake, registering every DMA buffer's
    /// IOVA mapping with the fake for translation.
    fn make_controller(
        state: &Rc<RefCell<FakeState>>,
        nsid: u32,
    ) -> Result<NvmeController<FakeMmio>> {
        let mmio = FakeMmio {
            state: Rc::clone(state),
        };
        let st = Rc::clone(state);
        let mut alloc = move |size: usize, iova: u64, _writable: bool| -> Result<DmaBuf> {
            let buf = DmaBuf::new_unmapped(size, iova)?;
            st.borrow_mut()
                .iova_map
                .push((iova, buf.as_ptr().addr(), buf.len()));
            Ok(buf)
        };
        NvmeController::init(mmio, nsid, &mut alloc)
    }

    #[test]
    fn nvme_cmd_lays_out_fields_little_endian() {
        let cmd = NvmeCmd::new(0x02, 0x1234)
            .nsid(7)
            .cdw10(0xAABB_CCDD)
            .cdw11(0x1122_3344)
            .cdw12(0x5566_7788)
            .prp1(0x1000_2000_3000_4000)
            .prp2(0x5000_6000_7000_8000);
        let b = cmd.bytes();
        assert_eq!(b[0], 0x02); // opcode
        assert_eq!(&b[2..4], &[0x34, 0x12]); // CID
        assert_eq!(&b[4..8], &[7, 0, 0, 0]); // NSID
        assert_eq!(
            &b[24..32],
            &[0x00, 0x40, 0x00, 0x30, 0x00, 0x20, 0x00, 0x10]
        ); // PRP1
        assert_eq!(
            &b[32..40],
            &[0x00, 0x80, 0x00, 0x70, 0x00, 0x60, 0x00, 0x50]
        ); // PRP2
        assert_eq!(&b[40..44], &[0xDD, 0xCC, 0xBB, 0xAA]); // CDW10
        assert_eq!(&b[44..48], &[0x44, 0x33, 0x22, 0x11]); // CDW11
        assert_eq!(&b[48..52], &[0x88, 0x77, 0x66, 0x55]); // CDW12
        assert_eq!(cmd.opcode(), 0x02);
    }

    #[test]
    fn queue_doorbell_offsets_follow_dstrd() {
        let stride = 4u64 << 2; // DSTRD = 2 -> 16-byte stride
        let sq = DmaBuf::new_unmapped(64, 0x1000).unwrap();
        let cq = DmaBuf::new_unmapped(16, 0x2000).unwrap();
        let qp = QueuePair::new(1, 8, stride, sq, cq);
        // SQ y tail at 0x1000 + 2*y*stride; CQ y head one stride later.
        assert_eq!(qp.sq_doorbell(), 0x1000 + 2 * 16);
        assert_eq!(qp.cq_doorbell(), 0x1000 + 3 * 16);
    }

    #[test]
    fn submit_rings_doorbell_and_poll_consumes_completion() {
        let state = Rc::new(RefCell::new(FakeState::new()));
        let mmio = FakeMmio {
            state: Rc::clone(&state),
        };
        let sq = DmaBuf::new_unmapped(4 * SQ_ENTRY_SIZE, 0x1000).unwrap();
        let cq = DmaBuf::new_unmapped(4 * CQ_ENTRY_SIZE, 0x2000).unwrap();
        let mut qp = QueuePair::new(3, 4, 16, sq, cq);

        // Submit: command lands in slot 0, tail becomes 1, doorbell rung.
        let cid = qp.submit(&mmio, &NvmeCmd::new(0x02, 0));
        assert_eq!(cid, 0);
        assert_eq!(qp.sq.as_slice()[0], 0x02);
        assert_eq!(state.borrow().doorbells.last(), Some(&(0x1060, 1)));

        // Synthesize a completion with phase 1 at head 0.
        unsafe {
            qp.cq
                .as_mut_ptr()
                .add(12)
                .cast::<u32>()
                .write_unaligned((1u32 << 16).to_le());
        }
        let (got, status) = qp.poll(&mmio, Duration::from_secs(1)).unwrap();
        assert_eq!((got, status), (0, 0));
        // Head advanced and its doorbell was rung.
        assert_eq!(state.borrow().doorbells.last(), Some(&(0x1070, 1)));

        // A nonzero status is reported, not hidden.
        let cid = qp.submit(&mmio, &NvmeCmd::new(0x02, 0));
        unsafe {
            qp.cq
                .as_mut_ptr()
                .add(16 + 12)
                .cast::<u32>()
                .write_unaligned((u32::from(cid) | (1 << 16) | (0x2802 << 17)).to_le());
        }
        let (got, status) = qp.poll(&mmio, Duration::from_secs(1)).unwrap();
        assert_eq!(got, cid);
        assert_eq!(status, 0x2802);
    }

    #[test]
    fn poll_times_out_when_no_completion_arrives() {
        let state = Rc::new(RefCell::new(FakeState::new()));
        let mmio = FakeMmio {
            state: Rc::clone(&state),
        };
        let sq = DmaBuf::new_unmapped(SQ_ENTRY_SIZE, 0x1000).unwrap();
        let cq = DmaBuf::new_unmapped(CQ_ENTRY_SIZE, 0x2000).unwrap();
        let mut qp = QueuePair::new(0, 1, 4, sq, cq);
        // CQ entry is zeroed: phase 0 != expected phase 1, so poll must time out.
        assert!(qp.poll(&mmio, Duration::from_millis(5)).is_err());
    }

    #[test]
    fn poll_toggles_phase_when_head_wraps() {
        let state = Rc::new(RefCell::new(FakeState::new()));
        let mmio = FakeMmio {
            state: Rc::clone(&state),
        };
        let sq = DmaBuf::new_unmapped(2 * SQ_ENTRY_SIZE, 0x1000).unwrap();
        let cq = DmaBuf::new_unmapped(2 * CQ_ENTRY_SIZE, 0x2000).unwrap();
        let mut qp = QueuePair::new(0, 2, 4, sq, cq);

        // Complete at head 1 (last slot) with phase 1.
        qp.cq_head = 1;
        unsafe {
            qp.cq
                .as_mut_ptr()
                .add(16 + 12)
                .cast::<u32>()
                .write_unaligned((7u32 | (1 << 16)).to_le());
        }
        let (got, _) = qp.poll(&mmio, Duration::from_secs(1)).unwrap();
        assert_eq!(got, 7);
        assert_eq!(qp.cq_head, 0);
        assert!(!qp.cq_phase, "phase toggles on wrap");

        // The next completion needs phase 0 now.
        unsafe {
            qp.cq
                .as_mut_ptr()
                .add(12)
                .cast::<u32>()
                .write_unaligned(9u32.to_le());
        }
        let (got, _) = qp.poll(&mmio, Duration::from_secs(1)).unwrap();
        assert_eq!(got, 9);
    }

    #[test]
    fn init_brings_up_controller_and_identifies_namespace() {
        let state = Rc::new(RefCell::new(FakeState::new()));
        let ctrl = make_controller(&state, 1).unwrap();
        assert_eq!(ctrl.namespace_blocks(), FAKE_NS_BLOCKS);
        // Controller was enabled via CC.EN.
        assert!(state.borrow().controller_enabled);
    }

    #[test]
    fn init_rejects_missing_namespace_and_bad_lba_format() {
        let state = Rc::new(RefCell::new(FakeState::new()));
        assert!(make_controller(&state, 0).is_err());
        assert!(make_controller(&state, 2).is_err());

        let state = Rc::new(RefCell::new(FakeState::new()));
        state.borrow_mut().lbads = 12; // 4096-byte sectors
        assert!(make_controller(&state, 1).is_err());
    }

    #[test]
    fn read_write_flush_deallocate_roundtrip() {
        let state = Rc::new(RefCell::new(FakeState::new()));
        let mut ctrl = make_controller(&state, 1).unwrap();

        // Small transfer: PRP1 only.
        let data: Vec<u8> = (0..4096u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        ctrl.write_blocks(10, &data).unwrap();
        let mut back = vec![0u8; 4096];
        ctrl.read_blocks(10, &mut back).unwrap();
        assert_eq!(back, data);

        // Two-page transfer: PRP1 + PRP2.
        let data: Vec<u8> = (0..8192u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        ctrl.write_blocks(100, &data).unwrap();
        let mut back = vec![0u8; 8192];
        ctrl.read_blocks(100, &mut back).unwrap();
        assert_eq!(back, data);

        // Full staging buffer: PRP list path.
        let big: Vec<u8> = (0..u32::try_from(STAGING_SIZE).unwrap())
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        ctrl.write_blocks(0, &big).unwrap();
        let mut big_back = vec![0u8; STAGING_SIZE];
        ctrl.read_blocks(0, &mut big_back).unwrap();
        assert_eq!(big_back, big);

        ctrl.flush().unwrap();
        // Deallocate is best-effort and must not fail.
        ctrl.deallocate(0, 8);
    }

    #[test]
    fn transfer_rejects_bad_lengths() {
        let state = Rc::new(RefCell::new(FakeState::new()));
        let mut ctrl = make_controller(&state, 1).unwrap();
        assert!(ctrl.read_blocks(0, &mut []).is_err());
        assert!(ctrl.read_blocks(0, &mut [0u8; 100]).is_err());
        assert!(
            ctrl.write_blocks(0, &vec![0u8; STAGING_SIZE + 512])
                .is_err()
        );
    }
}

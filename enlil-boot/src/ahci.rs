//! Bare-metal `AHCI` storage driver for the enlil kernel (Phase 6.5, T-6.19).
//!
//! The fallback when no `NVMe` controller is present: drives an `AHCI` host bus
//! adapter directly from the boot kernel -- one command issued at a time on
//! slot 0 of each started port, with polled completion. Like
//! [`crate::nvme`] it is generic over [`Mmio`](crate::mmio::Mmio) so unit
//! tests plug in a fake HBA, and DMA memory is
//! [`DmaBuf`](crate::mmio::DmaBuf) (physical = virtual under the kernel's
//! early identity map).
//!
//! Only the parts needed to find disks and move blocks are implemented:
//! HBA enable, per-port start (command list + FIS receive area), IDENTIFY
//! DEVICE, and 48-bit LBA READ/WRITE DMA EXT. Port multipliers, NCQ, ATAPI,
//! and staggered spin-up are out of scope.

extern crate alloc;

use crate::mmio::{DmaBuf, DmaError, Mmio};
use core::fmt;

// ---------------------------------------------------------------------------
// HBA registers (AHCI spec §3, memory-mapped at ABAR)
// ---------------------------------------------------------------------------

/// Global HBA control.
const REG_GHC: u64 = 0x04;
/// Ports implemented bitmask.
const REG_PI: u64 = 0x0c;

/// `GHC.AE`: `AHCI` enable.
const GHC_AE: u32 = 1 << 31;

/// First port register block; each port strides 0x80.
const PORT_BASE: u64 = 0x100;
/// Port register stride.
const PORT_STRIDE: u64 = 0x80;
/// Maximum ports (the PI bitmask is 32 bits).
const MAX_PORTS: u8 = 32;

// Port register offsets within a port block.
const P_CLB: u64 = 0x00;
const P_CLBU: u64 = 0x04;
const P_FB: u64 = 0x08;
const P_FBU: u64 = 0x0c;
const P_IS: u64 = 0x10;
const P_CMD: u64 = 0x18;
const P_TFD: u64 = 0x20;
const P_SIG: u64 = 0x24;
const P_SSTS: u64 = 0x28;
const P_SERR: u64 = 0x30;
const P_CI: u64 = 0x38;

/// `PxCMD.ST`: start the port's DMA engine.
const CMD_ST: u32 = 1 << 0;
/// `PxCMD.FRE`: FIS receive enable.
const CMD_FRE: u32 = 1 << 4;
/// `PxCMD.FR`: FIS receive running (read-only status).
const CMD_FR: u32 = 1 << 14;
/// `PxCMD.CR`: command list running (read-only status).
const CMD_CR: u32 = 1 << 15;
/// `PxTFD` status byte: busy.
const TFD_BSY: u32 = 1 << 7;
/// `PxTFD` status byte: data request.
const TFD_DRQ: u32 = 1 << 3;
/// `PxTFD` status byte: error.
const TFD_ERR: u32 = 1 << 0;
/// `PxSSTS.DET`: device present and PHY online.
const SSTS_DET_PRESENT: u32 = 0x3;
/// `PxSIG` for a SATA (non-ATAPI) disk.
const SIG_SATA: u32 = 0x0000_0101;
/// `PxIS.TFES`: task file error status (sticky, write-1-clear).
const IS_TFES: u32 = 1 << 30;

// ---------------------------------------------------------------------------
// Command structures (AHCI spec §4.2)
// ---------------------------------------------------------------------------

/// Command-list entries per port (32 slots x 32 bytes); we only use slot 0.
const CMD_LIST_BYTES: usize = 1024;
/// Received-FIS area per port.
const FIS_BYTES: usize = 256;
/// Data buffer per started port: 32 pages = 128 KiB = 256 sectors max.
const DATA_BYTES: usize = 128 * 1024;
/// Largest single transfer (must stay a 512-multiple within the data buffer).
const MAX_TRANSFER_BYTES: usize = DATA_BYTES;
/// Bytes per PRD entry.
const PRD_PAGE: usize = 4096;
/// Bytes per PRD entry, as `u64` for DMA address arithmetic.
const PRD_PAGE_U64: u64 = 4096;

/// Low 32 bits of a DMA address for a 32-bit register half.
///
/// The mask makes the narrowing cast explicit: DMA buffers come from the
/// kernel heap below 4 GiB, so the high half carried separately is zero.
#[must_use]
const fn phys_lo(phys: u64) -> u32 {
    (phys & 0xFFFF_FFFF) as u32
}

/// High 32 bits of a DMA address for a 32-bit register half.
#[must_use]
const fn phys_hi(phys: u64) -> u32 {
    (phys >> 32) as u32
}

/// Command-header DW0: command FIS length, in DWORDs (5 = 20-byte H2D FIS).
const HDR_CFL_5: u32 = 5;
/// Command-header DW0: write bit (1 = write, 0 = read).
const HDR_W: u32 = 1 << 6;
/// Command-header DW0: PRDTL field shift (entry count, bits 31:16).
const HDR_PRDTL_SHIFT: u32 = 16;

/// Register Host-to-Device FIS type.
const FIS_TYPE_H2D: u8 = 0x27;
/// H2D FIS byte 1: `C` bit (this FIS carries a command).
const FIS_H2D_C: u8 = 1 << 7;
/// H2D FIS device byte: LBA mode.
const FIS_DEVICE_LBA: u8 = 0x40;

/// ATA IDENTIFY DEVICE.
const ATA_IDENTIFY: u8 = 0xec;
/// ATA READ DMA EXT (48-bit LBA).
const ATA_READ_DMA_EXT: u8 = 0x25;
/// ATA WRITE DMA EXT (48-bit LBA).
const ATA_WRITE_DMA_EXT: u8 = 0x35;

/// Bounded spin budget for port/DMA-engine state changes.
const PORT_SPINS: u64 = 10_000_000;
/// Bounded spin budget for one command completion.
const CMD_SPINS: u64 = 100_000_000;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What can go wrong driving the HBA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AhciError {
    /// A polled condition never became true within its spin budget.
    Timeout(&'static str),
    /// The port is not in the HBA's implemented-ports mask.
    PortNotImplemented(u8),
    /// No device on the port (`PxSSTS.DET` != present+online).
    NoDevice(u8),
    /// The port's signature is not a SATA disk (ATAPI etc. unsupported).
    BadSignature {
        /// The port number.
        port: u8,
        /// The signature read from `PxSIG`.
        sig: u32,
    },
    /// The device reported a task-file error for the command.
    TaskFileError(u8),
    /// A transfer length that is not a non-zero 512-multiple <= 128 KiB.
    BadTransferLength(usize),
    /// DMA allocation failed.
    Alloc(DmaError),
}

impl fmt::Display for AhciError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(what) => write!(f, "AHCI timeout waiting for {what}"),
            Self::PortNotImplemented(port) => {
                write!(f, "AHCI port {port} is not implemented by the HBA")
            }
            Self::NoDevice(port) => write!(f, "AHCI port {port} has no device"),
            Self::BadSignature { port, sig } => {
                write!(
                    f,
                    "AHCI port {port} signature {sig:#010x} is not a SATA disk"
                )
            }
            Self::TaskFileError(status) => {
                write!(f, "AHCI task file error, status {status:#04x}")
            }
            Self::BadTransferLength(len) => write!(
                f,
                "AHCI transfer must be a non-zero multiple of 512 bytes up to 128 KiB, got {len}"
            ),
            Self::Alloc(e) => write!(f, "AHCI DMA allocation failed: {e}"),
        }
    }
}

impl core::error::Error for AhciError {}

impl From<DmaError> for AhciError {
    fn from(e: DmaError) -> Self {
        Self::Alloc(e)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Byte offset of `reg` in port `port`'s register block.
#[must_use]
pub const fn port_reg(port: u8, reg: u64) -> u64 {
    PORT_BASE + (port as u64) * PORT_STRIDE + reg
}

/// The 40-byte model string from an IDENTIFY DEVICE buffer (words 27..46,
/// byte-swapped per ATA string convention), trimmed of trailing spaces.
///
/// Pure and host-tested; the kernel prints it when it finds a disk.
#[must_use]
pub fn identify_model(identify: &[u8; 512]) -> [u8; 40] {
    // Word i contributes bytes [2i] = high, [2i+1] = low: ATA strings store
    // each word byte-swapped, so the first character is the high byte.
    let mut model = [b' '; 40];
    for i in 0..20 {
        let word = u16::from_le_bytes([identify[54 + i * 2], identify[54 + i * 2 + 1]]);
        model[i * 2] = (word >> 8) as u8;
        model[i * 2 + 1] = (word & 0xff) as u8;
    }
    // Trim trailing spaces.
    let mut len = 40;
    while len > 0 && model[len - 1] == b' ' {
        len -= 1;
    }
    let mut trimmed = [0u8; 40];
    trimmed[..len].copy_from_slice(&model[..len]);
    trimmed
}

// ---------------------------------------------------------------------------
// Controller driver
// ---------------------------------------------------------------------------

/// DMA resources for one started port.
struct PortDma {
    /// Command list (32 headers).
    clb: DmaBuf,
    /// Received-FIS area.
    fb: DmaBuf,
    /// Command table: CFIS + PRDT (one page, slot 0 only).
    table: DmaBuf,
    /// Data buffer (128 KiB).
    data: DmaBuf,
}

impl PortDma {
    fn new() -> Result<Self, AhciError> {
        Ok(Self {
            clb: DmaBuf::new(CMD_LIST_BYTES)?,
            fb: DmaBuf::new(FIS_BYTES)?,
            table: DmaBuf::new(4096)?,
            data: DmaBuf::new(DATA_BYTES)?,
        })
    }
}

/// A directly-driven `AHCI` host bus adapter: HBA enable, per-port start, and
/// polled IDENTIFY + 48-bit LBA read/write on slot 0.
///
/// `M` is the ABAR access method -- [`PhysMmio`](crate::mmio::PhysMmio) on
/// real hardware, a fake in tests.
pub struct AhciController<M: Mmio> {
    mmio: M,
    ports_mask: u32,
    ports: [Option<PortDma>; MAX_PORTS as usize],
}

/// Spin until `condition` holds or the budget is exhausted.
///
/// # Errors
///
/// Returns [`AhciError::Timeout`] on exhaustion.
fn poll_until(
    max_spins: u64,
    what: &'static str,
    mut condition: impl FnMut() -> bool,
) -> Result<(), AhciError> {
    let mut spins = 0u64;
    while !condition() {
        spins += 1;
        if spins >= max_spins {
            return Err(AhciError::Timeout(what));
        }
        core::hint::spin_loop();
    }
    Ok(())
}

impl<M: Mmio> fmt::Debug for AhciController<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AhciController")
            .field("ports_mask", &self.ports_mask)
            .finish_non_exhaustive()
    }
}

impl<M: Mmio> AhciController<M> {
    /// Take ownership of an `AHCI` HBA: set `GHC.AE` (`AHCI` enable) and read the
    /// implemented-ports mask.
    ///
    /// # Errors
    ///
    /// Returns [`AhciError`] if DMA allocation fails (no port DMA is
    /// allocated yet -- that happens per port in [`Self::start_port`]).
    pub fn init(mmio: M) -> Result<Self, AhciError> {
        let ghc = mmio.read32(REG_GHC);
        mmio.write32(REG_GHC, ghc | GHC_AE);
        let ports_mask = mmio.read32(REG_PI);
        Ok(Self {
            mmio,
            ports_mask,
            ports: core::array::from_fn(|_| None),
        })
    }

    /// Bitmask of ports the HBA implements (from `PI`).
    #[must_use]
    pub const fn implemented_ports(&self) -> u32 {
        self.ports_mask
    }

    /// Whether `port` is implemented by the HBA.
    #[must_use]
    pub const fn port_implemented(&self, port: u8) -> bool {
        port < MAX_PORTS && (self.ports_mask >> port) & 1 != 0
    }

    /// The `PxSIG` signature of `port`.
    #[must_use]
    pub fn port_signature(&self, port: u8) -> u32 {
        self.mmio.read32(port_reg(port, P_SIG))
    }

    /// Whether a device is present and its PHY online on `port`.
    #[must_use]
    pub fn device_present(&self, port: u8) -> bool {
        self.mmio.read32(port_reg(port, P_SSTS)) & 0x0f == SSTS_DET_PRESENT
    }

    /// Start `port`'s DMA engine: stop anything running, program the command
    /// list + FIS receive area, clear error state, and set `FRE` + `ST`.
    ///
    /// # Errors
    ///
    /// Returns [`AhciError`] if the port is not implemented, no device is
    /// present, the signature is not a SATA disk, or a state change times out.
    pub fn start_port(&mut self, port: u8) -> Result<(), AhciError> {
        if !self.port_implemented(port) {
            return Err(AhciError::PortNotImplemented(port));
        }
        if !self.device_present(port) {
            return Err(AhciError::NoDevice(port));
        }
        let sig = self.port_signature(port);
        if sig != SIG_SATA {
            return Err(AhciError::BadSignature { port, sig });
        }

        // Stop the port: clear ST, wait for CR to clear.
        let cmd = self.mmio.read32(port_reg(port, P_CMD));
        self.mmio.write32(port_reg(port, P_CMD), cmd & !CMD_ST);
        poll_until(PORT_SPINS, "port command-list stop", || {
            self.mmio.read32(port_reg(port, P_CMD)) & CMD_CR == 0
        })?;

        let dma = PortDma::new()?;
        self.mmio
            .write32(port_reg(port, P_CLB), phys_lo(dma.clb.phys()));
        self.mmio
            .write32(port_reg(port, P_CLBU), phys_hi(dma.clb.phys()));
        self.mmio
            .write32(port_reg(port, P_FB), phys_lo(dma.fb.phys()));
        self.mmio
            .write32(port_reg(port, P_FBU), phys_hi(dma.fb.phys()));
        // Clear sticky error state before starting.
        self.mmio.write32(port_reg(port, P_SERR), 0xffff_ffff);
        self.mmio.write32(port_reg(port, P_IS), 0xffff_ffff);

        // Enable FIS receive, then start.
        let cmd = self.mmio.read32(port_reg(port, P_CMD));
        self.mmio.write32(port_reg(port, P_CMD), cmd | CMD_FRE);
        poll_until(PORT_SPINS, "port FIS receive running", || {
            self.mmio.read32(port_reg(port, P_CMD)) & CMD_FR != 0
        })?;
        let cmd = self.mmio.read32(port_reg(port, P_CMD));
        self.mmio.write32(port_reg(port, P_CMD), cmd | CMD_ST);

        self.ports[usize::from(port)] = Some(dma);
        Ok(())
    }

    /// Build a Register Host-to-Device FIS for an ATA command.
    const fn build_fis(command: u8, lba: u64, count: u16, device: u8) -> [u8; 20] {
        let mut fis = [0u8; 20];
        fis[0] = FIS_TYPE_H2D;
        fis[1] = FIS_H2D_C; // PM port 0, C bit
        fis[2] = command;
        fis[4] = (lba & 0xFF) as u8;
        fis[5] = ((lba >> 8) & 0xFF) as u8;
        fis[6] = ((lba >> 16) & 0xFF) as u8;
        fis[7] = device;
        fis[8] = ((lba >> 24) & 0xFF) as u8;
        fis[9] = ((lba >> 32) & 0xFF) as u8;
        fis[10] = ((lba >> 40) & 0xFF) as u8;
        fis[12] = (count & 0xFF) as u8;
        fis[13] = ((count >> 8) & 0xFF) as u8;
        fis
    }

    /// Issue one command on slot 0 of `port` and poll for its completion.
    ///
    /// `data_len` bytes move between the device and the port's data buffer
    /// (`to_device` selects the direction); the caller copies in/out.
    fn issue(
        &mut self,
        port: u8,
        fis: &[u8; 20],
        data_len: usize,
        to_device: bool,
    ) -> Result<(), AhciError> {
        let dma = self.ports[usize::from(port)]
            .as_mut()
            .ok_or(AhciError::PortNotImplemented(port))?;

        // The device must be idle before its slot is reused.
        poll_until(PORT_SPINS, "port idle", || {
            let tfd = self.mmio.read32(port_reg(port, P_TFD));
            tfd & (TFD_BSY | TFD_DRQ) == 0 && self.mmio.read32(port_reg(port, P_CI)) & 1 == 0
        })?;

        // Command header 0: 5-DWORD CFIS, direction, PRD count.
        let npages = data_len.div_ceil(PRD_PAGE);
        let prdtl = u32::try_from(npages).map_err(|_| AhciError::BadTransferLength(data_len))?;
        let w = if to_device { HDR_W } else { 0 };
        let dw0 = HDR_CFL_5 | w | (prdtl << HDR_PRDTL_SHIFT);
        let clb = dma.clb.as_mut_slice();
        clb[0..4].copy_from_slice(&dw0.to_le_bytes());
        clb[4..8].copy_from_slice(&0u32.to_le_bytes()); // PRDBC
        let table_phys = dma.table.phys();
        clb[8..12].copy_from_slice(&phys_lo(table_phys).to_le_bytes());
        clb[12..16].copy_from_slice(&phys_hi(table_phys).to_le_bytes());
        for b in &mut clb[16..32] {
            *b = 0;
        }

        // Command table: CFIS then one PRD per data page.
        let table = dma.table.as_mut_slice();
        table[..64].fill(0);
        table[..20].copy_from_slice(fis);
        let mut addr = dma.data.phys();
        for i in 0..npages {
            let prd = &mut table[128 + i * 16..128 + (i + 1) * 16];
            prd[0..4].copy_from_slice(&phys_lo(addr).to_le_bytes());
            prd[4..8].copy_from_slice(&phys_hi(addr).to_le_bytes());
            for b in &mut prd[8..12] {
                *b = 0;
            }
            // Byte count is 0-based; the last entry may be short.
            let count = if i + 1 == npages {
                data_len - i * PRD_PAGE
            } else {
                PRD_PAGE
            };
            let count32 =
                u32::try_from(count).map_err(|_| AhciError::BadTransferLength(data_len))?;
            prd[12..16].copy_from_slice(&(count32 - 1).to_le_bytes());
            addr += PRD_PAGE_U64;
        }

        // Ring the slot-0 doorbell and poll for completion.
        self.mmio.write32(port_reg(port, P_CI), 1);
        poll_until(CMD_SPINS, "command completion", || {
            self.mmio.read32(port_reg(port, P_CI)) & 1 == 0
        })?;

        // Check for a task-file error.
        if self.mmio.read32(port_reg(port, P_IS)) & IS_TFES != 0 {
            let tfd = self.mmio.read32(port_reg(port, P_TFD));
            self.mmio.write32(port_reg(port, P_IS), IS_TFES);
            return Err(AhciError::TaskFileError((tfd & 0xff) as u8));
        }
        let tfd = self.mmio.read32(port_reg(port, P_TFD));
        if tfd & TFD_ERR != 0 {
            return Err(AhciError::TaskFileError((tfd & 0xff) as u8));
        }
        Ok(())
    }

    /// Run IDENTIFY DEVICE on `port`, returning the 512-byte data buffer.
    ///
    /// # Errors
    ///
    /// Returns [`AhciError`] if the port was not started or the command fails.
    ///
    /// # Panics
    ///
    /// Panics if the port was not started with [`Self::start_port`] -- a
    /// driver bug; the kernel always starts a port before commanding it.
    pub fn identify(&mut self, port: u8) -> Result<[u8; 512], AhciError> {
        let fis = Self::build_fis(ATA_IDENTIFY, 0, 0, 0);
        self.issue(port, &fis, 512, false)?;
        let mut out = [0u8; 512];
        out.copy_from_slice(
            &self.ports[usize::from(port)]
                .as_ref()
                .expect("port started")
                .data
                .as_slice()[..512],
        );
        Ok(out)
    }

    const fn check_transfer_len(len: usize) -> Result<(), AhciError> {
        if len == 0 || len > MAX_TRANSFER_BYTES || !len.is_multiple_of(512) {
            return Err(AhciError::BadTransferLength(len));
        }
        Ok(())
    }

    /// Read `out.len()` bytes (512-multiple, <= 128 KiB) at `lba`.
    ///
    /// # Errors
    ///
    /// Returns [`AhciError`] if the port was not started, the length is
    /// invalid, or the command fails.
    ///
    /// # Panics
    ///
    /// Panics if the port was not started with [`Self::start_port`] -- a
    /// driver bug; the kernel always starts a port before commanding it.
    pub fn read_sectors(&mut self, port: u8, lba: u64, out: &mut [u8]) -> Result<(), AhciError> {
        Self::check_transfer_len(out.len())?;
        let count =
            u16::try_from(out.len() / 512).map_err(|_| AhciError::BadTransferLength(out.len()))?;
        let fis = Self::build_fis(ATA_READ_DMA_EXT, lba, count, FIS_DEVICE_LBA);
        self.issue(port, &fis, out.len(), false)?;
        out.copy_from_slice(
            &self.ports[usize::from(port)]
                .as_ref()
                .expect("port started")
                .data
                .as_slice()[..out.len()],
        );
        Ok(())
    }

    /// Write `data.len()` bytes (512-multiple, <= 128 KiB) at `lba`.
    ///
    /// # Errors
    ///
    /// Returns [`AhciError`] if the port was not started, the length is
    /// invalid, or the command fails.
    ///
    /// # Panics
    ///
    /// Panics if the port was not started with [`Self::start_port`] -- a
    /// driver bug; the kernel always starts a port before commanding it.
    pub fn write_sectors(&mut self, port: u8, lba: u64, data: &[u8]) -> Result<(), AhciError> {
        Self::check_transfer_len(data.len())?;
        let count = u16::try_from(data.len() / 512)
            .map_err(|_| AhciError::BadTransferLength(data.len()))?;
        let fis = Self::build_fis(ATA_WRITE_DMA_EXT, lba, count, FIS_DEVICE_LBA);
        self.ports[usize::from(port)]
            .as_mut()
            .expect("port started")
            .data
            .as_mut_slice()[..data.len()]
            .copy_from_slice(data);
        self.issue(port, &fis, data.len(), true)
    }
}

// ---------------------------------------------------------------------------
// Tests: a fake AHCI HBA behind the Mmio trait
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Fake disk: 16384 sectors = 8 MiB.
    const FAKE_SECTORS: u64 = 16384;

    struct FakePort {
        clb: *mut u8,
        table: *mut u8,
    }

    struct FakeState {
        /// Register file: HBA regs + 32 port blocks.
        regs: Vec<u8>,
        ports: [Option<FakePort>; MAX_PORTS as usize],
        disk: Vec<u8>,
    }

    impl FakeState {
        fn new() -> Self {
            let mut regs = vec![0u8; 0x100 + 32 * 0x80];
            // PI: port 0 implemented.
            regs[0x0c..0x10].copy_from_slice(&1u32.to_le_bytes());
            // Port 0: device present + PHY online, SATA signature.
            let ssts = reg_off(0, P_SSTS);
            regs[ssts..ssts + 4].copy_from_slice(&0x123u32.to_le_bytes()); // DET=3
            let sig = reg_off(0, P_SIG);
            regs[sig..sig + 4].copy_from_slice(&SIG_SATA.to_le_bytes());
            Self {
                regs,
                ports: core::array::from_fn(|_| None),
                disk: vec![0u8; usize::try_from(FAKE_SECTORS * 512).unwrap()],
            }
        }

        fn set32(&mut self, off: u64, v: u32) {
            let o = usize::try_from(off).unwrap();
            self.regs[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }

        fn get32(&self, off: u64) -> u32 {
            let o = usize::try_from(off).unwrap();
            u32::from_le_bytes(self.regs[o..o + 4].try_into().unwrap())
        }

        /// Execute the slot-0 command whose CTBA the driver programmed.
        fn exec_command(&mut self, port: u8) {
            let p = usize::from(port);
            let fake = self.ports[p].as_ref().expect("fake: port DMA unknown");
            // SAFETY: the buffers belong to the driver under test.
            let clb = unsafe { core::slice::from_raw_parts(fake.clb, CMD_LIST_BYTES) };
            let dw0 = u32::from_le_bytes(clb[0..4].try_into().unwrap());
            let prdtl = (dw0 >> HDR_PRDTL_SHIFT) as usize;
            let ctba = u64::from_le_bytes(clb[8..16].try_into().unwrap());
            debug_assert_eq!(ctba, fake.table as u64);
            let table = unsafe { core::slice::from_raw_parts_mut(fake.table, 4096) };
            let fis = &table[..20];
            let command = fis[2];
            let lba = u64::from(fis[4])
                | (u64::from(fis[5]) << 8)
                | (u64::from(fis[6]) << 16)
                | (u64::from(fis[8]) << 24)
                | (u64::from(fis[9]) << 32)
                | (u64::from(fis[10]) << 40);
            let count = u64::from(fis[12]) | (u64::from(fis[13]) << 8);
            // Gather PRD data pages.
            let mut pages: Vec<(usize, usize)> = Vec::new();
            for i in 0..prdtl {
                let prd = &table[128 + i * 16..128 + (i + 1) * 16];
                let addr =
                    usize::try_from(u64::from_le_bytes(prd[0..8].try_into().unwrap())).unwrap();
                let dbc = usize::try_from(u32::from_le_bytes(prd[12..16].try_into().unwrap()))
                    .unwrap()
                    + 1;
                pages.push((addr, dbc));
            }
            match command {
                ATA_IDENTIFY => {
                    let mut id = [0u8; 512];
                    // Model "FAKE AHCI DISK", ATA-swapped per word.
                    let model = b"FAKE AHCI DISK";
                    for (i, chunk) in model.chunks(2).enumerate() {
                        let (a, b) = (chunk[0], *chunk.get(1).unwrap_or(&b' '));
                        id[54 + i * 2] = b;
                        id[54 + i * 2 + 1] = a;
                    }
                    // Words 60-61: total sectors (LBA48).
                    id[120..128].copy_from_slice(&FAKE_SECTORS.to_le_bytes());
                    let (addr, len) = pages[0];
                    let page = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len) };
                    page[..512.min(len)].copy_from_slice(&id[..512.min(len)]);
                }
                ATA_READ_DMA_EXT | ATA_WRITE_DMA_EXT => {
                    let len = usize::try_from(count * 512).unwrap();
                    let base = usize::try_from(lba * 512).unwrap();
                    let mut off = base;
                    assert!(off + len <= self.disk.len(), "fake: LBA out of range");
                    for (addr, dbc) in &pages {
                        let take = (*dbc).min(len - (off - base));
                        let page =
                            unsafe { core::slice::from_raw_parts_mut(*addr as *mut u8, *dbc) };
                        if command == ATA_WRITE_DMA_EXT {
                            self.disk[off..off + take].copy_from_slice(&page[..take]);
                        } else {
                            page[..take].copy_from_slice(&self.disk[off..off + take]);
                        }
                        off += take;
                    }
                }
                _ => {
                    // Unknown command: report a task-file error.
                    let is = reg_off(port, P_IS);
                    self.regs[is..is + 4].copy_from_slice(&IS_TFES.to_le_bytes());
                }
            }
            // Complete: clear CI, device idle.
            let ci = reg_off(port, P_CI);
            self.regs[ci..ci + 4].copy_from_slice(&0u32.to_le_bytes());
            let tfd = reg_off(port, P_TFD);
            self.regs[tfd..tfd + 4].copy_from_slice(&0x50u32.to_le_bytes()); // DRDY|DSC
        }
    }

    /// Register-file index of `reg` in port `port`'s block (test helper).
    fn reg_off(port: u8, reg: u64) -> usize {
        usize::try_from(PORT_BASE + u64::from(port) * PORT_STRIDE + reg).unwrap()
    }

    struct FakeAhci {
        state: RefCell<FakeState>,
    }

    impl FakeAhci {
        fn new() -> Self {
            Self {
                state: RefCell::new(FakeState::new()),
            }
        }
    }

    impl Mmio for FakeAhci {
        fn read32(&self, off: u64) -> u32 {
            self.state.borrow().get32(off)
        }

        fn read64(&self, off: u64) -> u64 {
            let s = self.state.borrow();
            let o = usize::try_from(off).unwrap();
            u64::from_le_bytes(s.regs[o..o + 8].try_into().unwrap())
        }

        fn write32(&self, off: u64, val: u32) {
            let mut s = self.state.borrow_mut();
            // Port CLB/FB program the fake's view of the DMA buffers.
            for port in 0..MAX_PORTS {
                let base = PORT_BASE + u64::from(port) * PORT_STRIDE;
                if off == base + P_CLB {
                    let p = &mut s.ports[usize::from(port)];
                    let lo = u64::from(val);
                    let entry = p.get_or_insert(FakePort {
                        clb: lo as *mut u8,
                        table: core::ptr::null_mut(),
                    });
                    entry.clb = (lo | (entry.clb as u64 & 0xffff_ffff_0000_0000)) as *mut u8;
                    return;
                }
                if off == base + P_CLBU {
                    let p = s.ports[usize::from(port)]
                        .as_mut()
                        .expect("fake: CLB first");
                    p.clb = (((u64::from(val)) << 32) | (p.clb as u64 & 0xffff_ffff)) as *mut u8;
                    return;
                }
                if off == base + P_CMD {
                    // Emulate the read-only status bits: FRE brings up FR,
                    // ST brings up CR; clearing them drops the status.
                    let mut v = val;
                    if v & CMD_FRE != 0 {
                        v |= CMD_FR;
                    } else {
                        v &= !CMD_FR;
                    }
                    if v & CMD_ST != 0 {
                        v |= CMD_CR;
                    } else {
                        v &= !CMD_CR;
                    }
                    s.set32(off, v);
                    return;
                }
                if off == base + P_IS || off == base + P_SERR {
                    // Write-1-to-clear in hardware: only the set bits clear.
                    let cur = s.get32(off);
                    s.set32(off, cur & !val);
                    return;
                }
                if off == base + P_CI && val & 1 != 0 {
                    // Slot-0 doorbell: capture the command table address from
                    // the header, then execute synchronously.
                    let clb = s.ports[usize::from(port)]
                        .as_ref()
                        .expect("fake: CLB first")
                        .clb;
                    let hdr = unsafe { core::slice::from_raw_parts(clb, 32) };
                    let ctba = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
                    s.ports[usize::from(port)].as_mut().unwrap().table = ctba as *mut u8;
                    s.exec_command(port);
                    return;
                }
            }
            s.set32(off, val);
        }

        fn write64(&self, _off: u64, _val: u64) {
            // The driver only uses 32-bit accesses on the AHCI path.
        }
    }

    fn init_fake() -> AhciController<FakeAhci> {
        AhciController::init(FakeAhci::new()).expect("fake AHCI init")
    }

    #[test]
    fn init_reports_implemented_ports() {
        let ctl = init_fake();
        assert_eq!(ctl.implemented_ports(), 1);
        assert!(ctl.port_implemented(0));
        assert!(!ctl.port_implemented(1));
    }

    #[test]
    fn start_port_finds_sata_disk() {
        let mut ctl = init_fake();
        ctl.start_port(0).unwrap();
        assert_eq!(ctl.port_signature(0), SIG_SATA);
        assert!(ctl.device_present(0));
    }

    #[test]
    fn start_port_rejects_unimplemented() {
        let mut ctl = init_fake();
        assert_eq!(
            ctl.start_port(1).unwrap_err(),
            AhciError::PortNotImplemented(1)
        );
    }

    #[test]
    fn identify_returns_model() {
        let mut ctl = init_fake();
        ctl.start_port(0).unwrap();
        let id = ctl.identify(0).unwrap();
        let model = identify_model(&id);
        let text = core::str::from_utf8(&model).unwrap();
        assert!(text.starts_with("FAKE AHCI DISK"), "model was {text:?}");
    }

    #[test]
    fn write_then_read_sectors() {
        let mut ctl = init_fake();
        ctl.start_port(0).unwrap();
        let mut wbuf = [0u8; 4096];
        for (i, b) in wbuf.iter_mut().enumerate() {
            *b = u8::try_from((i * 7) & 0xff).unwrap();
        }
        ctl.write_sectors(0, 42, &wbuf).unwrap();
        let mut rbuf = [0u8; 4096];
        ctl.read_sectors(0, 42, &mut rbuf).unwrap();
        assert_eq!(wbuf, rbuf);
    }

    #[test]
    fn multi_page_transfer() {
        // 16 sectors = 8 KiB = 2 PRD entries.
        let mut ctl = init_fake();
        ctl.start_port(0).unwrap();
        let wbuf = [0x5au8; 8192];
        ctl.write_sectors(0, 100, &wbuf).unwrap();
        let mut rbuf = [0u8; 8192];
        ctl.read_sectors(0, 100, &mut rbuf).unwrap();
        assert_eq!(wbuf, rbuf);
        // Untouched sectors still read as zero.
        let mut zero = [0u8; 512];
        ctl.read_sectors(0, 0, &mut zero).unwrap();
        assert!(zero.iter().all(|&b| b == 0));
    }

    #[test]
    fn bad_lengths_rejected() {
        let mut ctl = init_fake();
        ctl.start_port(0).unwrap();
        let mut out = [0u8; 100];
        assert_eq!(
            ctl.read_sectors(0, 0, &mut out).unwrap_err(),
            AhciError::BadTransferLength(100)
        );
    }

    #[test]
    fn port_reg_addresses() {
        assert_eq!(port_reg(0, P_CMD), 0x118);
        assert_eq!(port_reg(1, P_CMD), 0x198);
        assert_eq!(port_reg(2, P_SIG), 0x100 + 2 * 0x80 + 0x24);
    }

    #[test]
    fn identify_model_trims_spaces() {
        let mut id = [0u8; 512];
        // "AB" as one ATA-swapped word at word 27.
        id[54] = b'B';
        id[55] = b'A';
        let model = identify_model(&id);
        assert_eq!(&model[..2], b"AB");
        assert!(model[2..].iter().all(|&b| b == 0));
    }
}

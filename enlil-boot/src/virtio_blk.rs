//! A virtio-blk device (virtio 1.x "modern" PCI transport) backed by a
//! [`BlockBackend`].
//!
//! This is the disk a guest sees. It owns its PCI configuration space
//! ([`PciFunction`]) with the virtio capabilities, a 16 KiB `BAR0` holding the
//! common / notify / ISR / device configuration regions, and a single split
//! virtqueue. A guest write to the notify region makes the device walk the
//! available ring, execute each request against the backing disk, post the
//! completions on the used ring and raise its interrupt.
//!
//! Every access to guest memory goes through [`GuestMemory`] and is
//! bounds-checked there, so a malicious or buggy driver can make a request
//! fail but never reach hypervisor memory. The device offers only
//! `VIRTIO_F_VERSION_1` plus a few block features — no indirect descriptors, no
//! event index, no packed ring — which keeps the queue logic small and the
//! attack surface narrow.
//!
//! Pure: the host tests below play the part of a guest driver.

// The virtio wire format is fixed-width (u16 indices, u32 lengths) while the
// device works in `usize`/`u64`. Every guest-chosen value is bounded by an
// explicit check before it is narrowed: queue size <= QUEUE_MAX, request size
// <= MAX_IO_BYTES, descriptor lengths are u32 on the wire already.
#![allow(clippy::cast_possible_truncation)]

extern crate alloc;

use crate::blockdev::{BlockBackend, SECTOR};
use crate::pci_dev::PciFunction;
use alloc::boxed::Box;
use alloc::vec::Vec;

/// Guest-physical memory as the device sees it.
///
/// Accesses are whole: they either succeed or report failure (out of range).
/// Interior mutability so a device can hold `&dyn GuestMemory` while the guest
/// owns the memory.
pub trait GuestMemory {
    /// Fill `buf` from guest-physical `gpa`; `false` if any byte is out of range.
    fn read(&self, gpa: u64, buf: &mut [u8]) -> bool;
    /// Write `data` to guest-physical `gpa`; `false` if any byte is out of range.
    fn write(&self, gpa: u64, data: &[u8]) -> bool;
}

fn rd16(m: &dyn GuestMemory, gpa: u64) -> Option<u16> {
    let mut b = [0u8; 2];
    m.read(gpa, &mut b).then(|| u16::from_le_bytes(b))
}

fn rd32(m: &dyn GuestMemory, gpa: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    m.read(gpa, &mut b).then(|| u32::from_le_bytes(b))
}

fn rd64(m: &dyn GuestMemory, gpa: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    m.read(gpa, &mut b).then(|| u64::from_le_bytes(b))
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Red Hat / virtio PCI vendor id.
pub const VENDOR_ID: u16 = 0x1AF4;
/// Modern virtio-blk PCI device id (`0x1040 + 2`).
pub const DEVICE_ID: u16 = 0x1042;
/// Size of `BAR0`.
pub const BAR0_SIZE: u64 = 0x4000;

const COMMON_OFF: usize = 0x0000;
const NOTIFY_OFF: usize = 0x1000;
const ISR_OFF: usize = 0x2000;
const DEVICE_OFF: usize = 0x3000;
const REGION_LEN: usize = 0x1000;
/// Bytes of the common configuration structure.
const COMMON_LEN: usize = 0x38;
/// `notify_off_multiplier`: queue `n`'s doorbell is at `n * 4`.
const NOTIFY_MULTIPLIER: u32 = 4;

/// Largest queue the device supports.
pub const QUEUE_MAX: u16 = 128;
/// Largest single request (all data segments together) it will execute.
const MAX_IO_BYTES: usize = 4 * 1024 * 1024;

// Feature bits.
const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;
const F_VERSION_1: u64 = 1 << 32;

// Device status bits.
const S_FEATURES_OK: u8 = 0x08;
const S_DRIVER_OK: u8 = 0x04;

// Descriptor flags.
const D_NEXT: u16 = 1;
const D_WRITE: u16 = 2;
const D_INDIRECT: u16 = 4;

// Request types and results.
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const R_OK: u8 = 0;
const R_IOERR: u8 = 1;
const R_UNSUPP: u8 = 2;

/// The identifier returned for `GET_ID`.
const DISK_ID: &[u8] = b"enlil-virtual-disk\0\0";

/// The split queue the device exposes.
#[derive(Debug, Clone, Copy, Default)]
struct Queue {
    size: u16,
    enabled: bool,
    desc: u64,
    avail: u64,
    used: u64,
    last_avail: u16,
    used_idx: u16,
}

/// A virtio-blk PCI device.
pub struct VirtioBlk {
    /// The device's PCI configuration space.
    pub pci: PciFunction,
    backend: Box<dyn BlockBackend>,
    read_only: bool,
    status: u8,
    isr: u8,
    dev_feature_sel: u32,
    drv_feature_sel: u32,
    driver_features: u64,
    queue_sel: u16,
    queue: Queue,
}

/// One descriptor in a request chain.
#[derive(Debug, Clone, Copy)]
struct Seg {
    gpa: u64,
    len: u32,
    write: bool,
}

/// Build a `virtio_pci_cap` capability body.
fn cap(cfg_type: u8, offset: usize, length: usize, multiplier: Option<u32>) -> Vec<u8> {
    let len: u8 = if multiplier.is_some() { 20 } else { 16 };
    let mut c = alloc::vec![0u8; usize::from(len)];
    c[0] = 0x09; // vendor-specific capability
    c[2] = len;
    c[3] = cfg_type;
    c[4] = 0; // BAR 0
    c[8..12].copy_from_slice(&(offset as u32).to_le_bytes());
    c[12..16].copy_from_slice(&(length as u32).to_le_bytes());
    if let Some(m) = multiplier {
        c[16..20].copy_from_slice(&m.to_le_bytes());
    }
    c
}

impl VirtioBlk {
    /// A virtio-blk device over `backend`. `irq` is the legacy PIC line the guest
    /// should use for its `INTx` interrupt; `read_only` offers `VIRTIO_BLK_F_RO`
    /// and fails every write.
    #[must_use]
    pub fn new(backend: Box<dyn BlockBackend>, irq: u8, read_only: bool) -> Self {
        let mut pci = PciFunction::new(
            VENDOR_ID,
            DEVICE_ID,
            [0x01, 0x80, 0x00],  // mass storage, other
            1,                   // virtio 1.x requires revision >= 1
            (VENDOR_ID, 0x0002), // subsystem id: virtio device type 2 (block)
        );
        pci.set_bar0_mem64(BAR0_SIZE);
        pci.set_interrupt(irq, 1);
        pci.add_capability(&cap(1, COMMON_OFF, COMMON_LEN, None));
        pci.add_capability(&cap(2, NOTIFY_OFF, REGION_LEN, Some(NOTIFY_MULTIPLIER)));
        pci.add_capability(&cap(3, ISR_OFF, 4, None));
        pci.add_capability(&cap(4, DEVICE_OFF, 0x40, None));
        Self {
            pci,
            backend,
            read_only,
            status: 0,
            isr: 0,
            dev_feature_sel: 0,
            drv_feature_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            queue: Queue::default(),
        }
    }

    /// The device feature bits offered to the driver.
    #[must_use]
    pub const fn device_features(&self) -> u64 {
        let mut f = F_VERSION_1 | F_SEG_MAX | F_BLK_SIZE | F_FLUSH;
        if self.read_only {
            f |= F_RO;
        }
        f
    }

    /// Whether the interrupt line is asserted.
    #[must_use]
    pub const fn irq_line(&self) -> bool {
        self.isr != 0 && !self.pci.intx_disabled()
    }

    /// Reset the device to its power-on state (a driver writes status `0`).
    fn reset(&mut self) {
        self.status = 0;
        self.isr = 0;
        self.dev_feature_sel = 0;
        self.drv_feature_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.queue = Queue::default();
    }

    /// The common configuration structure as a byte image.
    fn common_image(&self) -> [u8; COMMON_LEN] {
        let mut b = [0u8; COMMON_LEN];
        let feats = self.device_features();
        let df = if self.dev_feature_sel == 0 {
            feats as u32
        } else if self.dev_feature_sel == 1 {
            (feats >> 32) as u32
        } else {
            0
        };
        let drv = if self.drv_feature_sel == 0 {
            self.driver_features as u32
        } else if self.drv_feature_sel == 1 {
            (self.driver_features >> 32) as u32
        } else {
            0
        };
        b[0x00..0x04].copy_from_slice(&self.dev_feature_sel.to_le_bytes());
        b[0x04..0x08].copy_from_slice(&df.to_le_bytes());
        b[0x08..0x0C].copy_from_slice(&self.drv_feature_sel.to_le_bytes());
        b[0x0C..0x10].copy_from_slice(&drv.to_le_bytes());
        b[0x10..0x12].copy_from_slice(&0xFFFFu16.to_le_bytes()); // no MSI-X vector
        b[0x12..0x14].copy_from_slice(&1u16.to_le_bytes()); // one queue
        b[0x14] = self.status;
        b[0x15] = 0; // config generation
        b[0x16..0x18].copy_from_slice(&self.queue_sel.to_le_bytes());
        let present = self.queue_sel == 0;
        let (size, enabled) = if present {
            (self.queue.size, u16::from(self.queue.enabled))
        } else {
            (0, 0)
        };
        b[0x18..0x1A].copy_from_slice(&size.to_le_bytes());
        b[0x1A..0x1C].copy_from_slice(&0xFFFFu16.to_le_bytes());
        b[0x1C..0x1E].copy_from_slice(&enabled.to_le_bytes());
        b[0x1E..0x20].copy_from_slice(&0u16.to_le_bytes()); // notify offset 0
        if present {
            b[0x20..0x28].copy_from_slice(&self.queue.desc.to_le_bytes());
            b[0x28..0x30].copy_from_slice(&self.queue.avail.to_le_bytes());
            b[0x30..0x38].copy_from_slice(&self.queue.used.to_le_bytes());
        }
        b
    }

    /// The device-specific configuration (`struct virtio_blk_config`).
    fn device_image(&self) -> [u8; 0x40] {
        let mut b = [0u8; 0x40];
        b[0x00..0x08].copy_from_slice(&self.backend.sectors().to_le_bytes());
        b[0x08..0x0C].copy_from_slice(&0u32.to_le_bytes()); // size_max (unused)
        b[0x0C..0x10].copy_from_slice(&126u32.to_le_bytes()); // seg_max
        b[0x14..0x18].copy_from_slice(&(SECTOR as u32).to_le_bytes()); // blk_size
        b
    }

    /// Read `size` bytes (1/2/4/8) at byte `off` of `image`.
    fn extract(image: &[u8], off: usize, size: u8) -> u64 {
        let mut v = 0u64;
        for i in 0..usize::from(size) {
            let byte = image.get(off + i).copied().unwrap_or(0);
            v |= u64::from(byte) << (8 * i);
        }
        v
    }

    /// A guest read of `size` bytes at `offset` within `BAR0`.
    pub fn mmio_read(&mut self, offset: u64, size: u8) -> u64 {
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        match off {
            o if o < COMMON_OFF + REGION_LEN => {
                Self::extract(&self.common_image(), o - COMMON_OFF, size)
            }
            o if (ISR_OFF..ISR_OFF + REGION_LEN).contains(&o) => {
                // Reading the ISR status clears it and drops the interrupt.
                if o == ISR_OFF {
                    let v = self.isr;
                    self.isr = 0;
                    u64::from(v)
                } else {
                    0
                }
            }
            o if (DEVICE_OFF..DEVICE_OFF + REGION_LEN).contains(&o) => {
                Self::extract(&self.device_image(), o - DEVICE_OFF, size)
            }
            // The notify region is write-only: reads see zero.
            _ => 0,
        }
    }

    /// A guest write of `size` bytes (`val`) at `offset` within `BAR0`. A write
    /// to the notify region runs the queue against `mem`.
    pub fn mmio_write(&mut self, offset: u64, size: u8, val: u64, mem: &dyn GuestMemory) {
        let off = usize::try_from(offset).unwrap_or(usize::MAX);
        if off < COMMON_OFF + REGION_LEN {
            self.write_common(off - COMMON_OFF, size, val);
        } else if (NOTIFY_OFF..NOTIFY_OFF + REGION_LEN).contains(&off) {
            // Only queue 0 exists; its doorbell is at offset 0.
            if off == NOTIFY_OFF {
                self.process(mem);
            }
        }
        // ISR and device configuration are read-only.
    }

    /// Apply a write to the common configuration structure.
    #[allow(clippy::cast_possible_truncation)] // fields are written at their own width
    fn write_common(&mut self, off: usize, size: u8, val: u64) {
        let q = self.queue_sel == 0;
        match (off, size) {
            (0x00, 4) => self.dev_feature_sel = val as u32,
            (0x08, 4) => self.drv_feature_sel = val as u32,
            (0x0C, 4) => {
                let shift = match self.drv_feature_sel {
                    0 => 0,
                    1 => 32,
                    _ => return,
                };
                let mask = 0xFFFF_FFFFu64 << shift;
                self.driver_features =
                    (self.driver_features & !mask) | ((val & 0xFFFF_FFFF) << shift);
            }
            (0x14, 1) => self.write_status(val as u8),
            (0x16, 2) => self.queue_sel = val as u16,
            (0x18, 2) if q && !self.queue.enabled => {
                let n = val as u16;
                // A queue size must be a power of two within the device limit.
                if n != 0 && n <= QUEUE_MAX && n.is_power_of_two() {
                    self.queue.size = n;
                }
            }
            (0x1C, 2) if q => {
                self.queue.enabled = val & 1 != 0 && self.queue.size != 0;
                if self.queue.enabled {
                    self.queue.last_avail = 0;
                    self.queue.used_idx = 0;
                }
            }
            (0x20, 4) if q => {
                self.queue.desc = (self.queue.desc & !0xFFFF_FFFF) | (val & 0xFFFF_FFFF);
            }
            (0x24, 4) if q => {
                self.queue.desc = (self.queue.desc & 0xFFFF_FFFF) | ((val & 0xFFFF_FFFF) << 32);
            }
            (0x28, 4) if q => {
                self.queue.avail = (self.queue.avail & !0xFFFF_FFFF) | (val & 0xFFFF_FFFF);
            }
            (0x2C, 4) if q => {
                self.queue.avail = (self.queue.avail & 0xFFFF_FFFF) | ((val & 0xFFFF_FFFF) << 32);
            }
            (0x30, 4) if q => {
                self.queue.used = (self.queue.used & !0xFFFF_FFFF) | (val & 0xFFFF_FFFF);
            }
            (0x34, 4) if q => {
                self.queue.used = (self.queue.used & 0xFFFF_FFFF) | ((val & 0xFFFF_FFFF) << 32);
            }
            // 64-bit accesses to the address registers.
            (0x20, 8) if q => self.queue.desc = val,
            (0x28, 8) if q => self.queue.avail = val,
            (0x30, 8) if q => self.queue.used = val,
            _ => {}
        }
        // The queue size defaults to the device maximum until the driver sets it.
        if self.queue.size == 0 && !self.queue.enabled {
            self.queue.size = QUEUE_MAX;
        }
    }

    /// Handle a write to the device-status register.
    fn write_status(&mut self, val: u8) {
        if val == 0 {
            self.reset();
            return;
        }
        let mut status = val;
        if val & S_FEATURES_OK != 0 {
            // Accept the driver's features only if they are a subset of ours and
            // include VERSION_1; otherwise FEATURES_OK stays clear and the driver
            // sees it fail.
            let ok = self.driver_features & !self.device_features() == 0
                && self.driver_features & F_VERSION_1 != 0;
            if !ok {
                status &= !S_FEATURES_OK;
            }
        }
        self.status = status;
    }

    /// Walk the available ring and execute every pending request. Returns how
    /// many were completed.
    pub fn process(&mut self, mem: &dyn GuestMemory) -> u32 {
        let q = self.queue;
        if !q.enabled || self.status & S_DRIVER_OK == 0 || !self.pci.bus_master() || q.size == 0 {
            return 0;
        }
        let Some(avail_idx) = rd16(mem, q.avail + 2) else {
            return 0;
        };
        let mut done = 0u32;
        let mut last_avail = q.last_avail;
        let mut used_idx = q.used_idx;
        // A bogus avail index must not make the device spin: at most one full
        // queue of requests per notification.
        while last_avail != avail_idx && done < u32::from(q.size) {
            let slot = u64::from(last_avail % q.size);
            let Some(head) = rd16(mem, q.avail + 4 + 2 * slot) else {
                break;
            };
            let written = self.handle_request(mem, head);
            let elem = q.used + 4 + 8 * u64::from(used_idx % q.size);
            let mut e = [0u8; 8];
            e[0..4].copy_from_slice(&u32::from(head).to_le_bytes());
            e[4..8].copy_from_slice(&written.to_le_bytes());
            if !mem.write(elem, &e) {
                break;
            }
            used_idx = used_idx.wrapping_add(1);
            last_avail = last_avail.wrapping_add(1);
            done += 1;
        }
        if done > 0 {
            // Publish the completions, then the new used index.
            let _ = mem.write(q.used + 2, &used_idx.to_le_bytes());
            self.queue.last_avail = last_avail;
            self.queue.used_idx = used_idx;
            self.isr |= 1;
        }
        done
    }

    /// Read the descriptor chain starting at `head`, or `None` if it is
    /// malformed (bad index, a loop, an indirect descriptor, or out of memory).
    fn read_chain(&self, mem: &dyn GuestMemory, head: u16) -> Option<Vec<Seg>> {
        let q = self.queue;
        let mut segs = Vec::new();
        let mut idx = head;
        for _ in 0..q.size {
            if idx >= q.size {
                return None;
            }
            let d = q.desc + 16 * u64::from(idx);
            let addr = rd64(mem, d)?;
            let len = rd32(mem, d + 8)?;
            let flags = rd16(mem, d + 12)?;
            let next = rd16(mem, d + 14)?;
            if flags & D_INDIRECT != 0 {
                return None;
            }
            segs.push(Seg {
                gpa: addr,
                len,
                write: flags & D_WRITE != 0,
            });
            if flags & D_NEXT == 0 {
                return Some(segs);
            }
            idx = next;
        }
        None // chain longer than the queue: a loop
    }

    /// Execute the request whose chain starts at `head`; returns the number of
    /// bytes written into device-writable buffers (the `used` length).
    fn handle_request(&mut self, mem: &dyn GuestMemory, head: u16) -> u32 {
        let Some(segs) = self.read_chain(mem, head) else {
            return 0;
        };
        // header + [data...] + status
        let (Some(hdr), Some(status)) = (segs.first(), segs.last()) else {
            return 0;
        };
        if segs.len() < 2 || hdr.write || hdr.len < 16 || !status.write || status.len < 1 {
            return 0;
        }
        let status_gpa = status.gpa + u64::from(status.len) - 1;
        let mut h = [0u8; 16];
        if !mem.read(hdr.gpa, &mut h) {
            return 0;
        }
        let kind = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        let sector = u64::from_le_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
        let data = &segs[1..segs.len() - 1];

        let (result, written) = self.execute(mem, kind, sector, data);
        // The status byte is the last byte of the last (writable) descriptor.
        let _ = mem.write(status_gpa, &[result]);
        written.saturating_add(1)
    }

    /// Run one decoded request; returns its status and the data bytes written to
    /// the guest.
    fn execute(
        &mut self,
        mem: &dyn GuestMemory,
        kind: u32,
        sector: u64,
        data: &[Seg],
    ) -> (u8, u32) {
        let total: u64 = data.iter().map(|s| u64::from(s.len)).sum();
        let io_ok = total as usize <= MAX_IO_BYTES && total.is_multiple_of(SECTOR as u64);
        match kind {
            T_IN => {
                if !io_ok || data.iter().any(|s| !s.write) {
                    return (R_IOERR, 0);
                }
                let mut buf = alloc::vec![0u8; total as usize];
                if self.backend.read(sector, &mut buf).is_err() {
                    return (R_IOERR, 0);
                }
                let mut at = 0usize;
                for s in data {
                    let n = s.len as usize;
                    if !mem.write(s.gpa, &buf[at..at + n]) {
                        return (R_IOERR, at as u32);
                    }
                    at += n;
                }
                (R_OK, total as u32)
            }
            T_OUT => {
                if !io_ok || self.read_only || data.iter().any(|s| s.write) {
                    return (R_IOERR, 0);
                }
                let mut buf = alloc::vec![0u8; total as usize];
                let mut at = 0usize;
                for s in data {
                    let n = s.len as usize;
                    if !mem.read(s.gpa, &mut buf[at..at + n]) {
                        return (R_IOERR, 0);
                    }
                    at += n;
                }
                match self.backend.write(sector, &buf) {
                    Ok(()) => (R_OK, 0),
                    Err(_) => (R_IOERR, 0),
                }
            }
            T_FLUSH => match self.backend.flush() {
                Ok(()) => (R_OK, 0),
                Err(_) => (R_IOERR, 0),
            },
            T_GET_ID => {
                let mut written = 0u32;
                let mut left = DISK_ID;
                for s in data.iter().filter(|s| s.write) {
                    let n = left.len().min(s.len as usize);
                    if !mem.write(s.gpa, &left[..n]) {
                        return (R_IOERR, written);
                    }
                    written += n as u32;
                    left = &left[n..];
                }
                (R_OK, written)
            }
            _ => (R_UNSUPP, 0),
        }
    }
}

#[cfg(test)]
mod tests;

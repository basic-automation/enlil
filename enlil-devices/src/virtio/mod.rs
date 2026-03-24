//! VirtIO device definitions and virtqueue types.
//!
//! Provides the core types shared across VirtIO device implementations
//! and the MMIO transport layer.

pub mod transport;

use bitflags::bitflags;

/// Magic value identifying a VirtIO MMIO device ("virt" in little-endian).
pub const VIRTIO_MMIO_MAGIC: u32 = 0x74726976;

/// VirtIO device type identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum VirtioDeviceType {
    Net = 1,
    Block = 2,
    Console = 3,
    Rng = 4,
    GPU = 16,
    Input = 18,
    Socket = 19,
}

bitflags! {
    /// VirtIO device status flags.
    ///
    /// The driver follows a specific sequence of setting these bits
    /// during device initialization.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VirtioStatus: u8 {
        /// Guest OS has found the device and recognised it as a valid virtio device.
        const ACKNOWLEDGE       = 1;
        /// Guest OS knows how to drive the device.
        const DRIVER            = 2;
        /// Driver is set up and ready to drive the device.
        const DRIVER_OK         = 4;
        /// Driver has acknowledged all the features it understands.
        const FEATURES_OK       = 8;
        /// Device has experienced an error and needs reset.
        const DEVICE_NEEDS_RESET = 64;
        /// Something went wrong; guest gave up on the device.
        const FAILED            = 128;
    }
}

// ---------------------------------------------------------------------------
// Virtqueue types
// ---------------------------------------------------------------------------

/// Virtqueue descriptor flags.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// A single descriptor in the virtqueue descriptor table.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct VirtqDesc {
    /// Physical (guest) address of the buffer.
    pub addr: u64,
    /// Length of the buffer in bytes.
    pub len: u32,
    /// Descriptor flags (see `VIRTQ_DESC_F_*`).
    pub flags: u16,
    /// Index of the next descriptor if `VIRTQ_DESC_F_NEXT` is set.
    pub next: u16,
}

/// The available ring — written by the driver, read by the device.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct VirtqAvail {
    pub flags: u16,
    pub idx: u16,
    pub ring: Vec<u16>,
}

impl VirtqAvail {
    pub fn new(queue_size: u16) -> Self {
        Self {
            flags: 0,
            idx: 0,
            ring: vec![0; queue_size as usize],
        }
    }
}

/// An element of the used ring.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct VirtqUsedElem {
    /// Index of the head of the descriptor chain.
    pub id: u32,
    /// Total bytes written into the descriptor chain buffers.
    pub len: u32,
}

/// The used ring — written by the device, read by the driver.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx: u16,
    pub ring: Vec<VirtqUsedElem>,
}

impl VirtqUsed {
    pub fn new(queue_size: u16) -> Self {
        Self {
            flags: 0,
            idx: 0,
            ring: vec![VirtqUsedElem::default(); queue_size as usize],
        }
    }
}

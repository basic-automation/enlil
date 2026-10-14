//! Bare-metal MMIO + DMA helpers shared by the kernel's device drivers.
//!
//! [`Mmio`] abstracts a controller's register window so drivers stay
//! host-testable: real hardware goes through [`PhysMmio`] (volatile accesses
//! at the BAR's physical address, valid because the kernel runs on an
//! identity map), while unit tests plug in a fake. [`DmaBuf`] is a
//! page-aligned, page-sized heap allocation whose virtual address *is* its
//! DMA address under that same identity map.

extern crate alloc;

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use core::fmt;

/// A controller register window, addressed by byte offset from the BAR base.
pub trait Mmio {
    /// Volatile 32-bit read at `offset`.
    fn read32(&self, offset: u64) -> u32;
    /// Volatile 64-bit read at `offset`.
    fn read64(&self, offset: u64) -> u64;
    /// Volatile 32-bit write at `offset`.
    fn write32(&self, offset: u64, val: u32);
    /// Volatile 64-bit write at `offset`.
    fn write64(&self, offset: u64, val: u64);
}

/// [`Mmio`] over a physical BAR base via volatile pointer accesses.
///
/// # Safety contract
///
/// The caller must guarantee `base` maps the device's registers (the kernel's
/// identity map covers the low 4 GiB; a `BAR` placed above that must first be
/// mapped with [`map_mmio_range`](crate::paging::map_mmio_range)) and that no other
/// live reference aliases the window while this handle exists.
#[derive(Debug, Clone, Copy)]
pub struct PhysMmio {
    base: u64,
}

impl PhysMmio {
    /// A register window based at physical address `base`.
    #[must_use]
    pub const fn new(base: u64) -> Self {
        Self { base }
    }

    /// The BAR base this window reads.
    #[must_use]
    pub const fn base(&self) -> u64 {
        self.base
    }
}

impl Mmio for PhysMmio {
    fn read32(&self, offset: u64) -> u32 {
        // SAFETY: construction guarantees the window is mapped and unaliased;
        // volatile so the compiler never caches or reorders device reads.
        unsafe { (self.base.wrapping_add(offset) as *const u32).read_volatile() }
    }

    fn read64(&self, offset: u64) -> u64 {
        // SAFETY: as for `read32`.
        unsafe { (self.base.wrapping_add(offset) as *const u64).read_volatile() }
    }

    fn write32(&self, offset: u64, val: u32) {
        // SAFETY: as for `read32`.
        unsafe { (self.base.wrapping_add(offset) as *mut u32).write_volatile(val) }
    }

    fn write64(&self, offset: u64, val: u64) {
        // SAFETY: as for `read32`.
        unsafe { (self.base.wrapping_add(offset) as *mut u64).write_volatile(val) }
    }
}

/// Page-aligned DMA memory owned by a driver.
///
/// The allocation is a multiple of 4 KiB, zeroed, and freed on drop. The
/// kernel runs identity-mapped, so [`DmaBuf::phys`] (the virtual address as a
/// `u64`) is the physical address the device must be programmed with.
pub struct DmaBuf {
    ptr: *mut u8,
    len: usize,
    layout: Layout,
}

/// Errors allocating DMA memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaError {
    /// The size/alignment combination is not a valid [`Layout`].
    BadLayout,
    /// The allocator returned null (out of memory).
    OutOfMemory,
}

impl fmt::Display for DmaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadLayout => write!(f, "invalid DMA layout"),
            Self::OutOfMemory => write!(f, "out of memory allocating DMA buffer"),
        }
    }
}

impl core::error::Error for DmaError {}

impl DmaBuf {
    /// Allocate `size` bytes of zeroed, 4 KiB-aligned DMA memory (rounded up
    /// to a whole number of pages, at least one page).
    ///
    /// # Errors
    ///
    /// Returns [`DmaError`] if the layout is invalid or allocation fails.
    pub fn new(size: usize) -> Result<Self, DmaError> {
        let len = size.next_multiple_of(4096).max(4096);
        let layout = Layout::from_size_align(len, 4096).map_err(|_| DmaError::BadLayout)?;
        // SAFETY: `layout` has non-zero size; the pointer is checked for null
        // and freed with the same layout in `Drop`.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(DmaError::OutOfMemory);
        }
        Ok(Self { ptr, len, layout })
    }

    /// The DMA (physical, under the identity map) address of the buffer.
    #[must_use]
    pub fn phys(&self) -> u64 {
        self.ptr as u64
    }

    /// Length in bytes (a multiple of 4096, never 0).
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer is empty (never true: at least one page).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The buffer as a byte slice.
    #[must_use]
    pub const fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr`/`len` describe the live, exclusively-owned allocation.
        unsafe { core::slice::from_raw_parts(self.ptr.cast_const(), self.len) }
    }

    /// The buffer as a mutable byte slice.
    #[must_use]
    pub const fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as for `as_slice`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        // SAFETY: the layout and pointer came from `alloc_zeroed` in `new`.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

// `DmaBuf` is moved between the driver and its queues; the pointer is only
// ever dereferenced by its owner (or by the device via the physical address).
unsafe impl Send for DmaBuf {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dma_buf_is_page_aligned_and_zeroed() {
        let buf = DmaBuf::new(100).unwrap();
        assert_eq!(buf.len(), 4096);
        assert_eq!(buf.phys() % 4096, 0);
        assert!(buf.as_slice().iter().all(|&b| b == 0));
    }

    #[test]
    fn dma_buf_rounds_up_to_pages() {
        let buf = DmaBuf::new(4097).unwrap();
        assert_eq!(buf.len(), 8192);
    }

    #[test]
    fn phys_mmio_base_roundtrips() {
        assert_eq!(PhysMmio::new(0xfe_b80000).base(), 0xfe_b80000);
    }
}

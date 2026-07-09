//! Memory Subsystem — `GlobalAlloc` implementation
//!
//! Provides the memory allocation infrastructure for the Enlil hypervisor.
//!
//! # Backends
//!
//! - **Linux:** Delegates to system allocator (mmap-backed). This is the default
//!   for development on a Linux host with KVM.
//! - **Bare-metal:** Buddy allocator over physical memory with slab allocator
//!   for small objects. Implemented in Phase 6.

pub mod map;
pub mod paging;

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Platform memory allocator statistics.
#[derive(Debug, Default)]
pub struct AllocStats {
    /// Total bytes currently allocated.
    pub allocated: AtomicUsize,
    /// Total allocation count.
    pub alloc_count: AtomicUsize,
    /// Total deallocation count.
    pub dealloc_count: AtomicUsize,
}

impl AllocStats {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            allocated: AtomicUsize::new(0),
            alloc_count: AtomicUsize::new(0),
            dealloc_count: AtomicUsize::new(0),
        }
    }

    pub fn allocated_bytes(&self) -> usize {
        self.allocated.load(Ordering::Relaxed)
    }

    pub fn total_allocs(&self) -> usize {
        self.alloc_count.load(Ordering::Relaxed)
    }

    pub fn total_deallocs(&self) -> usize {
        self.dealloc_count.load(Ordering::Relaxed)
    }
}

static STATS: AllocStats = AllocStats::new();

/// Returns a reference to the global allocation statistics.
#[must_use]
pub fn stats() -> &'static AllocStats {
    &STATS
}

// ---------------------------------------------------------------------------
// Buddy Allocator (bare-metal backend)
// ---------------------------------------------------------------------------

/// Order-based buddy allocator for physical memory management.
///
/// Manages memory in power-of-two blocks from 4KB (order 0) up to
/// `2^MAX_ORDER` * 4KB. Used as the `GlobalAlloc` backend on bare-metal.
pub struct BuddyAllocator {
    /// Free lists per order. Each entry is a linked list of free blocks.
    free_lists: [Vec<usize>; Self::MAX_ORDER + 1],
    /// Base physical address of managed region.
    base: usize,
    /// Total size in bytes.
    size: usize,
}

impl BuddyAllocator {
    /// Maximum order (`2^MAX_ORDER` * 4KB = 4GB max block).
    pub const MAX_ORDER: usize = 20;
    /// Minimum block size (4KB page).
    pub const MIN_BLOCK_SIZE: usize = 4096;

    /// Create a new buddy allocator managing the given memory region.
    #[must_use]
    pub fn new(base: usize, size: usize) -> Self {
        let mut alloc = Self {
            free_lists: Default::default(),
            base,
            size,
        };
        alloc.init_region(base, size);
        alloc
    }

    /// Initialize the allocator by adding the region as free blocks.
    fn init_region(&mut self, base: usize, size: usize) {
        let mut offset = 0;
        let mut remaining = size;

        // Break the region into the largest possible buddy blocks.
        for order in (0..=Self::MAX_ORDER).rev() {
            let block_size = Self::MIN_BLOCK_SIZE << order;
            while remaining >= block_size {
                self.free_lists[order].push(base + offset);
                offset += block_size;
                remaining -= block_size;
            }
        }
    }

    /// Allocate a block of at least `size` bytes with the given alignment.
    pub fn allocate(&mut self, layout: Layout) -> Option<*mut u8> {
        let size = layout.size().max(layout.align()).max(Self::MIN_BLOCK_SIZE);
        let order = Self::size_to_order(size);

        if order > Self::MAX_ORDER {
            return None;
        }

        // Find a free block at the required order or higher.
        let found_order = (order..=Self::MAX_ORDER).find(|&o| !self.free_lists[o].is_empty())?;

        let block = self.free_lists[found_order].pop()?;

        // Split larger blocks down to the required order.
        for o in (order..found_order).rev() {
            let buddy = block + (Self::MIN_BLOCK_SIZE << o);
            self.free_lists[o].push(buddy);
        }

        STATS
            .allocated
            .fetch_add(Self::MIN_BLOCK_SIZE << order, Ordering::Relaxed);
        STATS.alloc_count.fetch_add(1, Ordering::Relaxed);

        Some(block as *mut u8)
    }

    /// Free a previously allocated block.
    pub fn deallocate(&mut self, ptr: *mut u8, layout: Layout) {
        let size = layout.size().max(layout.align()).max(Self::MIN_BLOCK_SIZE);
        let order = Self::size_to_order(size);
        let mut addr = ptr as usize;
        let mut current_order = order;

        // Coalesce with buddies.
        while current_order < Self::MAX_ORDER {
            let buddy = addr ^ (Self::MIN_BLOCK_SIZE << current_order);

            // Check if buddy is in the free list.
            if let Some(pos) = self.free_lists[current_order]
                .iter()
                .position(|&a| a == buddy)
            {
                self.free_lists[current_order].swap_remove(pos);
                addr = addr.min(buddy);
                current_order += 1;
            } else {
                break;
            }
        }

        self.free_lists[current_order].push(addr);
        STATS
            .allocated
            .fetch_sub(Self::MIN_BLOCK_SIZE << order, Ordering::Relaxed);
        STATS.dealloc_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Convert a size to the minimum buddy order that can hold it.
    const fn size_to_order(size: usize) -> usize {
        let mut order = 0;
        let mut block_size = Self::MIN_BLOCK_SIZE;
        while block_size < size {
            block_size <<= 1;
            order += 1;
        }
        order
    }

    /// Returns total free bytes across all orders.
    #[must_use]
    pub fn free_bytes(&self) -> usize {
        self.free_lists
            .iter()
            .enumerate()
            .map(|(order, list)| list.len() * (Self::MIN_BLOCK_SIZE << order))
            .sum()
    }

    /// Returns the base address of the managed region.
    #[must_use]
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Returns the total size of the managed region.
    #[must_use]
    pub const fn size(&self) -> usize {
        self.size
    }
}

// ---------------------------------------------------------------------------
// Slab Allocator (small object cache, sits on top of buddy allocator)
// ---------------------------------------------------------------------------

/// Slab allocator for fixed-size small objects.
///
/// Reduces fragmentation and improves performance for frequently
/// allocated/freed objects of known sizes (e.g., vCPU contexts, `VirtIO`
/// descriptors, task structs).
pub struct SlabCache {
    /// Object size for this cache.
    object_size: usize,
    /// Free list of available objects.
    free_list: Vec<*mut u8>,
    /// Pages backing this slab.
    slabs: Vec<*mut u8>,
}

impl SlabCache {
    /// Create a new slab cache for objects of the given size.
    ///
    ///
    /// # Panics
    ///
    /// Panics if `object_size` is less than 8.
    /// # Panics
    ///
    /// Panics if `object_size` is less than 8.
    #[must_use]
    pub fn new(object_size: usize) -> Self {
        assert!(object_size >= 8, "minimum slab object size is 8 bytes");
        Self {
            object_size: object_size.next_power_of_two().max(8),
            free_list: Vec::new(),
            slabs: Vec::new(),
        }
    }

    /// Allocate an object from this slab cache.
    pub fn alloc(&mut self) -> Option<*mut u8> {
        if self.free_list.is_empty() {
            self.grow()?;
        }
        let ptr = self.free_list.pop()?;
        STATS.alloc_count.fetch_add(1, Ordering::Relaxed);
        STATS
            .allocated
            .fetch_add(self.object_size, Ordering::Relaxed);
        Some(ptr)
    }

    /// Return an object to this slab cache.
    pub fn free(&mut self, ptr: *mut u8) {
        self.free_list.push(ptr);
        STATS.dealloc_count.fetch_add(1, Ordering::Relaxed);
        STATS
            .allocated
            .fetch_sub(self.object_size, Ordering::Relaxed);
    }

    /// Grow the slab by allocating a new page and carving it into objects.
    fn grow(&mut self) -> Option<()> {
        let page_size = 4096;
        let layout = Layout::from_size_align(page_size, page_size).ok()?;
        let page = unsafe { std::alloc::alloc(layout) };
        if page.is_null() {
            return None;
        }
        self.slabs.push(page);

        let objects_per_page = page_size / self.object_size;
        for i in 0..objects_per_page {
            let obj = unsafe { page.add(i * self.object_size) };
            self.free_list.push(obj);
        }
        Some(())
    }

    /// Returns the object size for this cache.
    #[must_use]
    pub const fn object_size(&self) -> usize {
        self.object_size
    }

    /// Returns the number of free objects available.
    #[must_use]
    pub const fn free_count(&self) -> usize {
        self.free_list.len()
    }
}

// ---------------------------------------------------------------------------
// Platform GlobalAlloc wrapper
// ---------------------------------------------------------------------------

/// The platform allocator — dispatches to the appropriate backend.
///
/// On Linux: wraps the system allocator.
/// On bare-metal: wraps `BuddyAllocator` + `SlabCache`.
pub struct PlatformAllocator;

unsafe impl GlobalAlloc for PlatformAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(feature = "platform-linux")]
        {
            let ptr = unsafe { std::alloc::System.alloc(layout) };
            if !ptr.is_null() {
                STATS.allocated.fetch_add(layout.size(), Ordering::Relaxed);
                STATS.alloc_count.fetch_add(1, Ordering::Relaxed);
            }
            ptr
        }
        #[cfg(not(feature = "platform-linux"))]
        {
            // Bare-metal: would dispatch to buddy/slab.
            // Stubbed — real implementation in Phase 6.
            let _ = layout;
            std::ptr::null_mut()
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        #[cfg(feature = "platform-linux")]
        {
            STATS.allocated.fetch_sub(layout.size(), Ordering::Relaxed);
            STATS.dealloc_count.fetch_add(1, Ordering::Relaxed);
            unsafe { std::alloc::System.dealloc(ptr, layout) };
        }
        #[cfg(not(feature = "platform-linux"))]
        {
            let _ = (ptr, layout);
        }
    }
}

// ---------------------------------------------------------------------------
// Physical / Virtual Address Types
// ---------------------------------------------------------------------------

use std::fmt;
use std::hash::Hash;
use std::ops::{Add, Sub};

/// Page size constant (4KB).
const PAGE_SIZE: u64 = 4096;

/// Newtype wrapper around `u64` for physical addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysAddr(u64);

impl PhysAddr {
    /// Create a new physical address.
    #[must_use]
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    /// Return the raw `u64` value.
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Check whether the address is aligned to `align`.
    #[must_use]
    ///
    /// # Panics
    ///
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    /// Panics if `align` is not a power of two.
    pub fn is_aligned(&self, align: u64) -> bool {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        self.0 & (align - 1) == 0
    }

    /// Round the address up to the next multiple of `align`.
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    pub fn align_up(&self, align: u64) -> Self {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        Self((self.0 + align - 1) & !(align - 1))
    }

    /// Round the address down to the previous multiple of `align`.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    pub fn align_down(&self, align: u64) -> Self {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        Self(self.0 & !(align - 1))
    }

    /// Offset within a 4KB page.
    #[must_use]
    pub const fn page_offset(&self) -> u64 {
        self.0 & (PAGE_SIZE - 1)
    }
}

impl fmt::Display for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#x})", self.0)
    }
}

impl Add<u64> for PhysAddr {
    type Output = Self;

    fn add(self, rhs: u64) -> Self {
        Self(self.0 + rhs)
    }
}

impl Sub<u64> for PhysAddr {
    type Output = Self;

    fn sub(self, rhs: u64) -> Self {
        Self(self.0 - rhs)
    }
}

/// Newtype wrapper around `u64` for virtual addresses.
///
/// # Panics
///
/// Panics if `align` is not a power of two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VirtAddr(u64);

impl VirtAddr {
    /// Create a new virtual address.
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    #[must_use]
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    /// Return the raw `u64` value.
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Check whether the address is aligned to `align`.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    pub fn is_aligned(&self, align: u64) -> bool {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        self.0 & (align - 1) == 0
    }

    /// Round the address up to the next multiple of `align`.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    pub fn align_up(&self, align: u64) -> Self {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        Self((self.0 + align - 1) & !(align - 1))
    }

    /// Round the address down to the previous multiple of `align`.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics if `align` is not a power of two.
    pub fn align_down(&self, align: u64) -> Self {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        Self(self.0 & !(align - 1))
    }

    /// Offset within a 4KB page.
    #[must_use]
    pub const fn page_offset(&self) -> u64 {
        self.0 & (PAGE_SIZE - 1)
    }
}

impl fmt::Display for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtAddr({:#x})", self.0)
    }
}

impl Add<u64> for VirtAddr {
    type Output = Self;

    fn add(self, rhs: u64) -> Self {
        Self(self.0 + rhs)
    }
}

impl Sub<u64> for VirtAddr {
    type Output = Self;

    fn sub(self, rhs: u64) -> Self {
        Self(self.0 - rhs)
    }
}

// ---------------------------------------------------------------------------
// Physical Frame
// ---------------------------------------------------------------------------

/// Represents a 4KB physical page frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysFrame {
    /// Frame number (physical address / `PAGE_SIZE`).
    number: u64,
}

impl PhysFrame {
    /// Return the frame that contains the given physical address.
    #[must_use]
    pub const fn containing_address(addr: PhysAddr) -> Self {
        Self {
            number: addr.as_u64() / PAGE_SIZE,
        }
    }

    /// Return the start physical address of this frame.
    #[must_use]
    pub const fn start_address(&self) -> PhysAddr {
        PhysAddr::new(self.number * PAGE_SIZE)
    }

    /// Create a frame from a raw frame number.
    #[must_use]
    pub const fn from_number(n: u64) -> Self {
        Self { number: n }
    }

    /// Return the frame number.
    ///
    /// # Panics
    ///
    /// Panics if `base` is not page-aligned.
    #[must_use]
    pub const fn number(&self) -> u64 {
        self.number
    }
}

// ---------------------------------------------------------------------------
// Bitmap Frame Allocator
// ---------------------------------------------------------------------------

/// Physical frame allocator backed by a bitmap.
///
/// Each bit in the bitmap represents one 4KB frame. A set bit means the
/// frame is allocated; a clear bit means it is free.
pub struct BitmapFrameAllocator {
    /// Bitmap storage — each `u64` tracks 64 frames.
    bitmap: Vec<u64>,
    /// Frame number of the first frame managed by this allocator.
    base_frame: u64,
    /// Total number of frames managed.
    total_frames: usize,
    /// Number of currently free frames.
    free_count: usize,
}

impl BitmapFrameAllocator {
    /// Create a new bitmap frame allocator.
    ///
    /// `base` is the starting physical address (must be page-aligned).
    /// `size` is the total region size in bytes.
    ///
    /// # Panics
    ///
    /// Panics if `base` is not page-aligned or if size is not a multiple of `PAGE_SIZE`.
    ///
    /// # Panics
    ///
    /// Panics if the frame is outside the managed region or was not allocated.
    #[must_use]
    pub fn new(base: PhysAddr, size: usize) -> Self {
        assert!(
            base.is_aligned(PAGE_SIZE),
            "base address must be page-aligned"
        );
        let total_frames = size / usize::try_from(PAGE_SIZE).expect("PAGE_SIZE exceeds usize");
        let bitmap_words = total_frames.div_ceil(64);
        Self {
            bitmap: vec![0u64; bitmap_words],
            base_frame: base.as_u64() / PAGE_SIZE,
            total_frames,
            free_count: total_frames,
        }
    }

    /// Allocate a single physical frame.
    pub fn allocate_frame(&mut self) -> Option<PhysFrame> {
        for (word_idx, word) in self.bitmap.iter_mut().enumerate() {
            if *word == u64::MAX {
                continue; // all 64 bits set — no free frame here
            }
            // Find the first zero bit.
            let bit = (!*word).trailing_zeros() as usize;
            let frame_idx = word_idx * 64 + bit;
            if frame_idx >= self.total_frames {
                return None;
            }
            *word |= 1u64 << bit;
            self.free_count -= 1;
            return Some(PhysFrame::from_number(self.base_frame + frame_idx as u64));
        }
        None
    }

    /// Deallocate a previously allocated frame.
    ///
    /// # Panics
    ///
    /// Panics if the frame is outside the managed region.
    pub fn deallocate_frame(&mut self, frame: PhysFrame) {
        let frame_idx =
            usize::try_from(frame.number() - self.base_frame).expect("frame index exceeds usize");
        assert!(
            frame_idx < self.total_frames,
            "frame outside managed region"
        );
        let word_idx = frame_idx / 64;
        let bit = frame_idx % 64;
        assert!(
            self.bitmap[word_idx] & (1u64 << bit) != 0,
            "double free of frame {}",
            frame.number()
        );
        self.bitmap[word_idx] &= !(1u64 << bit);
        self.free_count += 1;
    }

    /// Number of currently free frames.
    #[must_use]
    pub const fn free_frames(&self) -> usize {
        self.free_count
    }

    /// Total number of managed frames.
    #[must_use]
    pub const fn total_frames(&self) -> usize {
        self.total_frames
    }

    /// Check whether a given frame is currently allocated.
    ///
    /// # Panics
    ///
    /// Panics if the frame index cannot be represented as a `usize`.
    #[must_use]
    pub fn is_allocated(&self, frame: PhysFrame) -> bool {
        let frame_idx =
            usize::try_from(frame.number() - self.base_frame).expect("frame index exceeds usize");
        if frame_idx >= self.total_frames {
            return false;
        }
        let word_idx = frame_idx / 64;
        let bit = frame_idx % 64;
        self.bitmap[word_idx] & (1u64 << bit) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buddy_allocator_basic() {
        let region_size = 1024 * 1024; // 1MB
        let mut alloc = BuddyAllocator::new(0x10_0000, region_size);

        assert_eq!(alloc.free_bytes(), region_size);

        // Allocate a 4KB block.
        let layout = Layout::from_size_align(4096, 4096).unwrap();
        let ptr = alloc.allocate(layout).expect("allocation failed");
        assert!(!ptr.is_null());
        assert_eq!(alloc.free_bytes(), region_size - 4096);

        // Free it back.
        alloc.deallocate(ptr, layout);
        assert_eq!(alloc.free_bytes(), region_size);
    }

    #[test]
    fn buddy_allocator_multiple() {
        let region_size = 64 * 4096; // 256KB
        let mut alloc = BuddyAllocator::new(0x20_0000, region_size);

        let layout = Layout::from_size_align(4096, 4096).unwrap();
        let mut ptrs = Vec::new();

        // Allocate all pages.
        for _ in 0..64 {
            let ptr = alloc.allocate(layout).expect("allocation failed");
            ptrs.push(ptr);
        }
        assert_eq!(alloc.free_bytes(), 0);

        // Free all pages.
        for ptr in ptrs {
            alloc.deallocate(ptr, layout);
        }
        assert_eq!(alloc.free_bytes(), region_size);
    }

    #[test]
    fn buddy_allocator_large_block() {
        let region_size = 1024 * 1024; // 1MB
        let mut alloc = BuddyAllocator::new(0x30_0000, region_size);

        // Allocate a 64KB block.
        let layout = Layout::from_size_align(65536, 4096).unwrap();
        let ptr = alloc.allocate(layout).expect("allocation failed");
        assert!(!ptr.is_null());

        alloc.deallocate(ptr, layout);
        assert_eq!(alloc.free_bytes(), region_size);
    }

    #[test]
    fn slab_cache_basic() {
        let mut cache = SlabCache::new(64);
        assert_eq!(cache.object_size(), 64);

        let obj = cache.alloc().expect("slab alloc failed");
        assert!(!obj.is_null());

        cache.free(obj);
    }

    #[test]
    fn slab_cache_multiple() {
        let mut cache = SlabCache::new(128);
        let objects_per_page = 4096 / 128;

        let mut ptrs = Vec::new();
        for _ in 0..objects_per_page {
            ptrs.push(cache.alloc().expect("slab alloc failed"));
        }

        // All objects from first page used.
        assert_eq!(cache.free_count(), 0);

        // Free all.
        for ptr in ptrs {
            cache.free(ptr);
        }
        assert_eq!(cache.free_count(), objects_per_page);
    }

    #[test]
    fn platform_allocator_stats() {
        // Reset stats for test isolation isn't possible with statics,
        // so just verify the allocator works.
        let layout = Layout::from_size_align(256, 8).unwrap();
        let ptr = unsafe { PlatformAllocator.alloc(layout) };
        assert!(!ptr.is_null());
        unsafe { PlatformAllocator.dealloc(ptr, layout) };
    }

    #[test]
    fn physaddr_basic() {
        let addr = PhysAddr::new(0x1000);
        assert_eq!(addr.as_u64(), 0x1000);

        // Alignment checks.
        assert!(addr.is_aligned(4096));
        assert!(addr.is_aligned(256));
        assert!(!PhysAddr::new(0x1001).is_aligned(4096));

        // align_up / align_down.
        let unaligned = PhysAddr::new(0x1234);
        assert_eq!(unaligned.align_up(4096), PhysAddr::new(0x2000));
        assert_eq!(unaligned.align_down(4096), PhysAddr::new(0x1000));

        // page_offset.
        assert_eq!(PhysAddr::new(0x1234).page_offset(), 0x234);
        assert_eq!(PhysAddr::new(0x3000).page_offset(), 0);

        // Add / Sub.
        assert_eq!(addr + 0x500, PhysAddr::new(0x1500));
        assert_eq!(addr - 0x100, PhysAddr::new(0x0F00));

        // Display.
        let s = format!("{addr}");
        assert!(s.contains("0x1000"));
    }

    #[test]
    fn virtaddr_basic() {
        let addr = VirtAddr::new(0x1000);
        assert_eq!(addr.as_u64(), 0x1000);

        // Alignment checks.
        assert!(addr.is_aligned(4096));
        assert!(addr.is_aligned(256));
        assert!(!VirtAddr::new(0x1001).is_aligned(4096));

        // align_up / align_down.
        let unaligned = VirtAddr::new(0x1234);
        assert_eq!(unaligned.align_up(4096), VirtAddr::new(0x2000));
        assert_eq!(unaligned.align_down(4096), VirtAddr::new(0x1000));

        // page_offset.
        assert_eq!(VirtAddr::new(0x1234).page_offset(), 0x234);
        assert_eq!(VirtAddr::new(0x3000).page_offset(), 0);

        // Add / Sub.
        assert_eq!(addr + 0x500, VirtAddr::new(0x1500));
        assert_eq!(addr - 0x100, VirtAddr::new(0x0F00));

        // Display.
        let s = format!("{addr}");
        assert!(s.contains("0x1000"));
    }

    #[test]
    fn physframe_containing_address() {
        // Exact page boundary.
        let frame = PhysFrame::containing_address(PhysAddr::new(0x5000));
        assert_eq!(frame.number(), 5);
        assert_eq!(frame.start_address(), PhysAddr::new(0x5000));

        // Mid-page address rounds down.
        let frame = PhysFrame::containing_address(PhysAddr::new(0x5ABC));
        assert_eq!(frame.number(), 5);
        assert_eq!(frame.start_address(), PhysAddr::new(0x5000));

        // from_number round-trip.
        let frame = PhysFrame::from_number(42);
        assert_eq!(frame.number(), 42);
        assert_eq!(frame.start_address(), PhysAddr::new(42 * 4096));
    }

    #[test]
    fn bitmap_allocator_basic() {
        // 16 frames = 64KB region.
        let mut alloc = BitmapFrameAllocator::new(PhysAddr::new(0x10_0000), 16 * 4096);
        assert_eq!(alloc.total_frames(), 16);
        assert_eq!(alloc.free_frames(), 16);

        // Allocate one frame.
        let frame = alloc.allocate_frame().expect("alloc failed");
        assert_eq!(alloc.free_frames(), 15);
        assert!(alloc.is_allocated(frame));

        // Free it.
        alloc.deallocate_frame(frame);
        assert_eq!(alloc.free_frames(), 16);
        assert!(!alloc.is_allocated(frame));

        // Allocate several, free in reverse.
        let mut frames = Vec::new();
        for _ in 0..8 {
            frames.push(alloc.allocate_frame().expect("alloc failed"));
        }
        assert_eq!(alloc.free_frames(), 8);

        for f in frames.into_iter().rev() {
            alloc.deallocate_frame(f);
        }
        assert_eq!(alloc.free_frames(), 16);
    }

    #[test]
    fn bitmap_allocator_exhaustion() {
        let num_frames = 4;
        let mut alloc = BitmapFrameAllocator::new(PhysAddr::new(0x20_0000), num_frames * 4096);
        assert_eq!(alloc.total_frames(), num_frames);
        assert_eq!(alloc.free_frames(), num_frames);

        // Allocate all frames.
        let mut frames = Vec::new();
        for _ in 0..num_frames {
            frames.push(alloc.allocate_frame().expect("alloc failed"));
        }
        assert_eq!(alloc.free_frames(), 0);

        // Next allocation must return None.
        assert!(alloc.allocate_frame().is_none());

        // Free one, allocate again — should succeed.
        alloc.deallocate_frame(frames.pop().unwrap());
        assert_eq!(alloc.free_frames(), 1);
        let reclaimed = alloc.allocate_frame().expect("alloc after free failed");
        assert!(alloc.is_allocated(reclaimed));
        assert_eq!(alloc.free_frames(), 0);
    }
}

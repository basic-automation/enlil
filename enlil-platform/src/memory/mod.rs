//! Memory Subsystem — GlobalAlloc implementation
//!
//! Provides the memory allocation infrastructure for the Enlil hypervisor.
//!
//! # Backends
//!
//! - **Linux:** Delegates to system allocator (mmap-backed). This is the default
//!   for development on a Linux host with KVM.
//! - **Bare-metal:** Buddy allocator over physical memory with slab allocator
//!   for small objects. Implemented in Phase 6.

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
pub fn stats() -> &'static AllocStats {
    &STATS
}

// ---------------------------------------------------------------------------
// Buddy Allocator (bare-metal backend)
// ---------------------------------------------------------------------------

/// Order-based buddy allocator for physical memory management.
///
/// Manages memory in power-of-two blocks from 4KB (order 0) up to
/// 2^MAX_ORDER * 4KB. Used as the GlobalAlloc backend on bare-metal.
pub struct BuddyAllocator {
    /// Free lists per order. Each entry is a linked list of free blocks.
    free_lists: [Vec<usize>; Self::MAX_ORDER + 1],
    /// Base physical address of managed region.
    base: usize,
    /// Total size in bytes.
    size: usize,
}

impl BuddyAllocator {
    /// Maximum order (2^MAX_ORDER * 4KB = 4GB max block).
    pub const MAX_ORDER: usize = 20;
    /// Minimum block size (4KB page).
    pub const MIN_BLOCK_SIZE: usize = 4096;

    /// Create a new buddy allocator managing the given memory region.
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
        let found_order = (order..=Self::MAX_ORDER)
            .find(|&o| !self.free_lists[o].is_empty())?;

        let block = self.free_lists[found_order].pop()?;

        // Split larger blocks down to the required order.
        for o in (order..found_order).rev() {
            let buddy = block + (Self::MIN_BLOCK_SIZE << o);
            self.free_lists[o].push(buddy);
        }

        STATS.allocated.fetch_add(Self::MIN_BLOCK_SIZE << order, Ordering::Relaxed);
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
        STATS.allocated.fetch_sub(Self::MIN_BLOCK_SIZE << order, Ordering::Relaxed);
        STATS.dealloc_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Convert a size to the minimum buddy order that can hold it.
    fn size_to_order(size: usize) -> usize {
        let mut order = 0;
        let mut block_size = Self::MIN_BLOCK_SIZE;
        while block_size < size {
            block_size <<= 1;
            order += 1;
        }
        order
    }

    /// Returns total free bytes across all orders.
    pub fn free_bytes(&self) -> usize {
        self.free_lists
            .iter()
            .enumerate()
            .map(|(order, list)| list.len() * (Self::MIN_BLOCK_SIZE << order))
            .sum()
    }

    /// Returns the base address of the managed region.
    pub fn base(&self) -> usize {
        self.base
    }

    /// Returns the total size of the managed region.
    pub fn size(&self) -> usize {
        self.size
    }
}

// ---------------------------------------------------------------------------
// Slab Allocator (small object cache, sits on top of buddy allocator)
// ---------------------------------------------------------------------------

/// Slab allocator for fixed-size small objects.
///
/// Reduces fragmentation and improves performance for frequently
/// allocated/freed objects of known sizes (e.g., vCPU contexts, VirtIO
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
        STATS.allocated.fetch_add(self.object_size, Ordering::Relaxed);
        Some(ptr)
    }

    /// Return an object to this slab cache.
    pub fn free(&mut self, ptr: *mut u8) {
        self.free_list.push(ptr);
        STATS.dealloc_count.fetch_add(1, Ordering::Relaxed);
        STATS.allocated.fetch_sub(self.object_size, Ordering::Relaxed);
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
    pub fn object_size(&self) -> usize {
        self.object_size
    }

    /// Returns the number of free objects available.
    pub fn free_count(&self) -> usize {
        self.free_list.len()
    }
}

// ---------------------------------------------------------------------------
// Platform GlobalAlloc wrapper
// ---------------------------------------------------------------------------

/// The platform allocator — dispatches to the appropriate backend.
///
/// On Linux: wraps the system allocator.
/// On bare-metal: wraps BuddyAllocator + SlabCache.
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
}

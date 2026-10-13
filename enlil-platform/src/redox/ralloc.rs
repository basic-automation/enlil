//! Redox `ralloc`-derived slab allocator.
//!
//! Ports the design of Redox OS's `ralloc` crate — the allocator that backs
//! the Redox kernel heap — into the platform layer:
//!
//! - Power-of-two **size classes** (`8 … 2048` bytes). Every allocation is
//!   rounded up to the smallest class covering both its size and its
//!   alignment, exactly like `ralloc`'s class lookup.
//! - Each class carves blocks out of 4 KiB **slabs** with an intrusive free
//!   list; a slab that becomes completely free is returned to the page
//!   source, and a full slab is unlinked until a free makes room again.
//! - Requests larger than the biggest class (or aligned more strictly) take
//!   the **direct-page path**: whole pages from the source with a header
//!   tucked just before the aligned payload.
//! - The backing store is a [`PageSource`]: [`MmapPageSource`] on Linux,
//!   [`BumpPageSource`] for `no_std` bring-up before a real page allocator
//!   is online (the same role Redox's early bump allocator plays).
//!
//! [`Ralloc`] implements [`GlobalAlloc`](core::alloc::GlobalAlloc), so a
//! hypervisor binary can install it with `#[global_allocator]`.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::SpinLock;

/// Page granularity for slabs and direct allocations.
pub const PAGE_SIZE: usize = 4096;

/// Smallest slab block, in bytes.
const CLASS_MIN: usize = 8;
/// Largest slab block; bigger requests go down the direct-page path.
const CLASS_MAX: usize = 2048;
/// Number of size classes: 8, 16, 32, …, 2048.
const N_CLASSES: usize = 9;

/// Round `value` up to a multiple of `align` (`align` must be a power of two).
const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// A source of page-aligned memory for [`Ralloc`].
///
/// The allocator never touches the pages' contents except through the slab
/// and large-allocation headers it writes itself.
pub trait PageSource: Send + Sync {
    /// Allocate `count` contiguous pages. The returned pointer is
    /// [`PAGE_SIZE`]-aligned and the pages are readable and writable.
    /// Returns null on failure.
    fn alloc_pages(&self, count: usize) -> *mut u8;

    /// Return pages previously obtained from [`alloc_pages`](Self::alloc_pages).
    ///
    /// # Safety
    ///
    /// `ptr` and `count` must be exactly what `alloc_pages` returned, and the
    /// pages must not be in use by any live allocation.
    unsafe fn free_pages(&self, ptr: *mut u8, count: usize);
}

/// [`PageSource`] backed by anonymous `mmap` (Linux backend).
///
/// Pages are returned to the OS with `munmap` as soon as their slab drains,
/// so the allocator's footprint tracks live usage.
#[cfg(all(feature = "platform-linux", target_os = "linux"))]
#[derive(Debug, Default)]
pub struct MmapPageSource;

#[cfg(all(feature = "platform-linux", target_os = "linux"))]
impl PageSource for MmapPageSource {
    fn alloc_pages(&self, count: usize) -> *mut u8 {
        let len = match count.checked_mul(PAGE_SIZE) {
            Some(len) if len > 0 => len,
            _ => return core::ptr::null_mut(),
        };
        // SAFETY: `len` is a nonzero multiple of the page size; anonymous
        // private mapping, no file descriptor involved.
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            core::ptr::null_mut()
        } else {
            ptr.cast::<u8>()
        }
    }

    unsafe fn free_pages(&self, ptr: *mut u8, count: usize) {
        let len = count.saturating_mul(PAGE_SIZE);
        // SAFETY: upheld by the caller — these exact pages came from
        // `alloc_pages` and hold no live allocation.
        unsafe {
            libc::munmap(ptr.cast::<libc::c_void>(), len);
        }
    }
}

/// [`PageSource`] that bumps through a caller-provided region (`no_std`
/// bring-up).
///
/// There is no page reclamation: `free_pages` is a no-op and freed slabs stay
/// mapped. That is the documented trade for an allocator that works before
/// any real page manager exists — the same compromise Redox's early-boot
/// bump allocator makes. Switch to a reclaiming source once one is online.
///
/// The region base does not need to be page-aligned; the bump pointer is
/// aligned up on every allocation.
#[derive(Debug)]
pub struct BumpPageSource {
    base: usize,
    next: AtomicUsize,
    end: usize,
}

impl BumpPageSource {
    /// Cover the byte range `base .. base + len`.
    #[must_use]
    pub const fn new(base: usize, len: usize) -> Self {
        Self {
            base,
            next: AtomicUsize::new(base),
            end: base + len,
        }
    }

    /// Bytes handed out so far (monotonic — freed pages are not reclaimed).
    #[must_use]
    pub fn used_bytes(&self) -> usize {
        self.next.load(Ordering::Relaxed).saturating_sub(self.base)
    }
}

impl PageSource for BumpPageSource {
    fn alloc_pages(&self, count: usize) -> *mut u8 {
        let len = count.saturating_mul(PAGE_SIZE);
        let mut current = self.next.load(Ordering::Relaxed);
        loop {
            let aligned = align_up(current, PAGE_SIZE);
            let Some(next) = aligned.checked_add(len) else {
                return core::ptr::null_mut();
            };
            if next > self.end {
                return core::ptr::null_mut();
            }
            match self.next.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return aligned as *mut u8,
                Err(observed) => current = observed,
            }
        }
    }

    unsafe fn free_pages(&self, _ptr: *mut u8, _count: usize) {
        // No reclamation by design; see the type documentation.
    }
}

/// Header of one 4 KiB slab page. Blocks follow the header, each
/// `block_size(class)` bytes and aligned to that size; free blocks are linked
/// through their first word.
#[repr(C)]
struct SlabHead {
    /// Size-class index (`block_size = CLASS_MIN << class`).
    class: u8,
    _reserved: [u8; 3],
    /// Blocks currently on the free list.
    avail: u16,
    /// Blocks the slab holds in total.
    capacity: u16,
    /// Intrusive free list: each free block's first word is the next pointer.
    free_list: *mut u8,
    /// Next slab with free blocks in this class's partial list.
    next: *mut Self,
}

impl SlabHead {
    /// Block size for a class index (`0 .. N_CLASSES`).
    const fn block_size(class: usize) -> usize {
        CLASS_MIN << class
    }

    /// Byte offset where blocks start: the first class-aligned offset past
    /// the header, so every block is class-aligned.
    const fn data_offset(class: usize) -> usize {
        align_up(core::mem::size_of::<Self>(), Self::block_size(class))
    }

    /// How many blocks of this class fit in one page.
    const fn capacity(class: usize) -> usize {
        (PAGE_SIZE - Self::data_offset(class)) / Self::block_size(class)
    }
}

/// Header tucked just before an over-aligned/oversized direct allocation.
#[repr(C)]
struct LargeHead {
    /// Page-source base (what to hand back to `free_pages`).
    base: *mut u8,
    /// Page count of the whole reservation.
    pages: usize,
    /// Payload size, for statistics.
    size: usize,
}

/// Size-class index for a request, or `None` when it must go direct-page.
///
/// The class covers both size and alignment: blocks are class-aligned, so a
/// layout with `align <= class` is always satisfiable from the slab.
fn class_index(size: usize, align: usize) -> Option<usize> {
    if size > CLASS_MAX || align > CLASS_MAX {
        return None;
    }
    let need = size.max(align).max(CLASS_MIN).next_power_of_two();
    debug_assert!(need <= CLASS_MAX);
    // `need` is a power of two in `8 ..= 2048`; its log2 minus 3 is the class.
    Some(need.trailing_zeros() as usize - CLASS_MIN.trailing_zeros() as usize)
}

/// Live allocator counters. All atomics use relaxed ordering: the numbers
/// are diagnostics, not synchronization.
#[derive(Debug)]
pub struct RallocStats {
    slab_bytes: AtomicUsize,
    large_bytes: AtomicUsize,
    slab_allocs: AtomicUsize,
    large_allocs: AtomicUsize,
}

impl RallocStats {
    /// Zeroed counters.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slab_bytes: AtomicUsize::new(0),
            large_bytes: AtomicUsize::new(0),
            slab_allocs: AtomicUsize::new(0),
            large_allocs: AtomicUsize::new(0),
        }
    }

    /// Bytes currently live in slabs.
    #[must_use]
    pub fn slab_bytes(&self) -> usize {
        self.slab_bytes.load(Ordering::Relaxed)
    }

    /// Bytes currently live in direct-page allocations.
    #[must_use]
    pub fn large_bytes(&self) -> usize {
        self.large_bytes.load(Ordering::Relaxed)
    }

    /// Bytes currently live, both paths.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.slab_bytes().saturating_add(self.large_bytes())
    }

    /// Successful slab allocations so far.
    #[must_use]
    pub fn slab_allocs(&self) -> usize {
        self.slab_allocs.load(Ordering::Relaxed)
    }

    /// Successful direct-page allocations so far.
    #[must_use]
    pub fn large_allocs(&self) -> usize {
        self.large_allocs.load(Ordering::Relaxed)
    }
}

impl Default for RallocStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Heap state behind the single global lock.
struct HeapInner {
    /// Per class, the head of the slab list with free blocks. Full slabs are
    /// unlinked (they rejoin when a block frees); drained slabs are returned
    /// to the page source instead of lingering here.
    partial: [*mut SlabHead; N_CLASSES],
}

/// Redox `ralloc`-style slab allocator over a [`PageSource`].
///
/// A single [`SpinLock`] guards the class lists — the same coarse locking
/// `ralloc` itself uses — so this stays obviously correct on SMP bring-up;
/// the lock is only ever held for pointer surgery, never across page-source
/// calls that could block.
///
/// Install as the global allocator with
/// `#[global_allocator] static A: Ralloc<MmapPageSource> = Ralloc::new(MmapPageSource);`
pub struct Ralloc<S> {
    source: S,
    inner: SpinLock<HeapInner>,
    stats: RallocStats,
}

impl<S: PageSource> Ralloc<S> {
    /// Build an allocator over `source`. `const` so it can back a
    /// `#[global_allocator]` static.
    #[must_use]
    pub const fn new(source: S) -> Self {
        Self {
            source,
            inner: SpinLock::new(HeapInner {
                partial: [core::ptr::null_mut(); N_CLASSES],
            }),
            stats: RallocStats::new(),
        }
    }

    /// Live allocation counters.
    #[must_use]
    pub const fn stats(&self) -> &RallocStats {
        &self.stats
    }

    /// Carve a fresh slab page for `class` and link its blocks.
    ///
    /// # Safety
    ///
    /// Called with the heap lock held (or before the allocator is shared);
    /// the returned slab is owned by the heap.
    unsafe fn new_slab(&self, class: usize) -> *mut SlabHead {
        let page = self.source.alloc_pages(1);
        if page.is_null() {
            return core::ptr::null_mut();
        }
        let block_size = SlabHead::block_size(class);
        let capacity = SlabHead::capacity(class);
        let data_off = SlabHead::data_offset(class);
        // `class < N_CLASSES` and `capacity <= 509`: neither cast truncates.
        #[allow(clippy::cast_possible_truncation)]
        let class_u8 = class as u8;
        #[allow(clippy::cast_possible_truncation)]
        let capacity_u16 = capacity as u16;
        // SAFETY: `page` is a fresh page-aligned 4 KiB page, so it is aligned
        // for `SlabHead` and exclusively owned by us.
        #[allow(clippy::cast_ptr_alignment)]
        let head = page.cast::<SlabHead>();
        unsafe {
            head.write(SlabHead {
                class: class_u8,
                _reserved: [0; 3],
                avail: capacity_u16,
                capacity: capacity_u16,
                free_list: core::ptr::null_mut(),
                next: core::ptr::null_mut(),
            });
            // Link blocks lowest-address-first so allocation order is stable.
            let mut free: *mut u8 = core::ptr::null_mut();
            for i in (0..capacity).rev() {
                // `data_off + capacity * block_size <= PAGE_SIZE` by
                // construction, and every block is block-aligned, so the
                // pointer below is aligned for `*mut u8`.
                let block = page.add(data_off + i * block_size);
                #[allow(clippy::cast_ptr_alignment)]
                let slot = block.cast::<*mut u8>();
                slot.write(free);
                free = block;
            }
            (*head).free_list = free;
        }
        head
    }

    /// Allocate from a slab class. `size`/`align` are known to fit a class.
    ///
    /// # Safety
    ///
    /// `class_index(size, align)` must be `Some`.
    unsafe fn alloc_slab(&self, size: usize, align: usize) -> *mut u8 {
        let class = class_index(size, align).expect("slab path requires a size class");
        let mut inner = self.inner.lock();
        let mut head = inner.partial[class];
        if head.is_null() {
            // SAFETY: fresh slab, owned by the heap from here on.
            head = unsafe { self.new_slab(class) };
            if head.is_null() {
                return core::ptr::null_mut();
            }
            inner.partial[class] = head;
        }
        // SAFETY: `head` is a live slab of this class with `avail > 0`
        // (slabs with no free blocks are never on the partial list), and its
        // free-list links are block-aligned words.
        unsafe {
            let block = (*head).free_list;
            debug_assert!(!block.is_null());
            #[allow(clippy::cast_ptr_alignment)]
            let slot = block as *const *mut u8;
            (*head).free_list = slot.read();
            (*head).avail -= 1;
            if (*head).avail == 0 {
                inner.partial[class] = (*head).next;
                (*head).next = core::ptr::null_mut();
            }
            let block_size = SlabHead::block_size(class);
            self.stats
                .slab_bytes
                .fetch_add(block_size, Ordering::Relaxed);
            self.stats.slab_allocs.fetch_add(1, Ordering::Relaxed);
            block
        }
    }

    /// Return a slab block. The slab is found by masking `ptr` to the page.
    ///
    /// # Safety
    ///
    /// `ptr` must have come from [`alloc_slab`](Self::alloc_slab) and not
    /// have been freed already.
    unsafe fn dealloc_slab(&self, ptr: *mut u8) {
        // SAFETY: slab pages are page-aligned, so masking recovers the live
        // `SlabHead`; the block being freed belongs to it.
        unsafe {
            let head = (ptr as usize & !(PAGE_SIZE - 1)) as *mut SlabHead;
            let class = (*head).class as usize;
            let block_size = SlabHead::block_size(class);
            let mut inner = self.inner.lock();
            let was_full = (*head).avail == 0;
            // The freed block is block-aligned, hence aligned for `*mut u8`.
            #[allow(clippy::cast_ptr_alignment)]
            let slot = ptr.cast::<*mut u8>();
            slot.write((*head).free_list);
            (*head).free_list = ptr;
            (*head).avail += 1;
            self.stats
                .slab_bytes
                .fetch_sub(block_size, Ordering::Relaxed);
            if (*head).avail == (*head).capacity {
                // Slab drained: unlink it and hand the page back.
                let mut link = core::ptr::addr_of_mut!(inner.partial[class]);
                while !(*link).is_null() {
                    if core::ptr::eq(*link, head) {
                        *link = (*head).next;
                        break;
                    }
                    link = core::ptr::addr_of_mut!((**link).next);
                }
                drop(inner);
                self.source.free_pages(head.cast::<u8>(), 1);
            } else if was_full {
                // Has room again: rejoin the partial list.
                (*head).next = inner.partial[class];
                inner.partial[class] = head;
            }
        }
    }

    /// Direct-page allocation for oversized or over-aligned requests.
    ///
    /// # Safety
    ///
    /// None beyond the [`GlobalAlloc`] contract: reserves `size + align +
    /// header` bytes worth of pages and returns an `align`-aligned payload
    /// pointer with the [`LargeHead`] tucked just before it.
    unsafe fn alloc_large(&self, size: usize, align: usize) -> *mut u8 {
        let head_size = core::mem::size_of::<LargeHead>();
        let Some(total) = size
            .checked_add(align)
            .and_then(|v| v.checked_add(head_size))
        else {
            return core::ptr::null_mut();
        };
        let pages = total.div_ceil(PAGE_SIZE);
        let base = self.source.alloc_pages(pages);
        if base.is_null() {
            return core::ptr::null_mut();
        }
        let base_addr = base as usize;
        let aligned = align_up(base_addr + head_size, align);
        debug_assert!(
            aligned.saturating_add(size)
                <= base_addr.saturating_add(pages.saturating_mul(PAGE_SIZE))
        );
        // SAFETY: `aligned - head_size .. aligned + size` lies inside the
        // freshly reserved pages, aligned for `LargeHead`.
        unsafe {
            let head_ptr = (aligned - head_size) as *mut LargeHead;
            head_ptr.write(LargeHead { base, pages, size });
        }
        self.stats.large_bytes.fetch_add(size, Ordering::Relaxed);
        self.stats.large_allocs.fetch_add(1, Ordering::Relaxed);
        aligned as *mut u8
    }

    /// # Safety
    ///
    /// `ptr` must have come from [`alloc_large`](Self::alloc_large) with a
    /// matching layout size class (i.e. the large path), not yet freed.
    unsafe fn dealloc_large(&self, ptr: *mut u8) {
        let head_size = core::mem::size_of::<LargeHead>();
        // SAFETY: the header sits exactly `head_size` bytes before a live
        // large allocation's payload.
        unsafe {
            let head = ((ptr as usize - head_size) as *const LargeHead).read();
            self.stats
                .large_bytes
                .fetch_sub(head.size, Ordering::Relaxed);
            self.source.free_pages(head.base, head.pages);
        }
    }
}

// SAFETY: `Ralloc` only shares `&self` access; the spinlock serializes heap
// mutation and both `PageSource` impls are `Send + Sync`. The `GlobalAlloc`
// contract (no aliasing of live allocations) is upheld by the slab/large
// paths above.
unsafe impl<S: PageSource> GlobalAlloc for Ralloc<S> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = (layout.size(), layout.align());
        if size == 0 {
            return NonNull::<u8>::dangling().as_ptr();
        }
        // SAFETY: `GlobalAlloc::alloc` contract — the caller supplies the
        // layout we dispatch on.
        unsafe {
            if class_index(size, align).is_some() {
                self.alloc_slab(size, align)
            } else {
                self.alloc_large(size, align)
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let (size, align) = (layout.size(), layout.align());
        if size == 0 {
            return;
        }
        // SAFETY: `ptr` came from `alloc` with this layout, so the dispatch
        // matches the allocation path.
        unsafe {
            if class_index(size, align).is_some() {
                self.dealloc_slab(ptr);
            } else {
                self.dealloc_large(ptr);
            }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let Ok(new_layout) = Layout::from_size_align(new_size, layout.align()) else {
            return core::ptr::null_mut();
        };
        if layout.size() == 0 {
            // SAFETY: fresh allocation; the old dangling pointer is not read.
            return unsafe { self.alloc(new_layout) };
        }
        // Fast path: the same slab class serves both sizes — nothing moves.
        if let (Some(old), Some(new)) = (
            class_index(layout.size(), layout.align()),
            class_index(new_size, layout.align()),
        ) && old == new
        {
            return ptr;
        }
        // SAFETY: `GlobalAlloc::realloc` contract — `ptr`/`layout` describe a
        // live allocation.
        unsafe {
            let new_ptr = self.alloc(new_layout);
            if new_ptr.is_null() {
                return new_ptr;
            }
            let copy_len = layout.size().min(new_size);
            core::ptr::copy_nonoverlapping(ptr, new_ptr, copy_len);
            self.dealloc(ptr, layout);
            new_ptr
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec, vec::Vec};

    #[cfg(all(feature = "platform-linux", target_os = "linux"))]
    fn test_alloc() -> Ralloc<MmapPageSource> {
        Ralloc::new(MmapPageSource)
    }

    #[cfg(all(feature = "platform-linux", target_os = "linux"))]
    #[test]
    fn slab_classes_roundtrip() {
        let alloc = test_alloc();
        // Sizes spanning every class boundary, plus the extremes.
        for size in [
            1usize, 8, 9, 15, 16, 24, 33, 64, 100, 129, 256, 513, 1024, 1500, 2048,
        ] {
            let layout = Layout::from_size_align(size, 8).unwrap();
            // SAFETY: layout is nonzero; the pointer is freed below.
            let ptr = unsafe { alloc.alloc(layout) };
            assert!(!ptr.is_null(), "alloc failed for size {size}");
            assert_eq!(ptr as usize % 8, 0);
            unsafe {
                core::ptr::write_bytes(ptr, 0xA5, size);
                assert!(
                    core::slice::from_raw_parts(ptr, size)
                        .iter()
                        .all(|&b| b == 0xA5)
                );
                alloc.dealloc(ptr, layout);
            }
        }
        assert_eq!(alloc.stats().total_bytes(), 0);
        assert!(alloc.stats().slab_allocs() > 0);
        assert_eq!(alloc.stats().large_allocs(), 0);
    }

    #[cfg(all(feature = "platform-linux", target_os = "linux"))]
    #[test]
    fn slab_honors_overalignment() {
        let alloc = test_alloc();
        for align in [16usize, 32, 64, 128, 256, 512, 1024, 2048] {
            let layout = Layout::from_size_align(8, align).unwrap();
            // SAFETY: freed below.
            let ptr = unsafe { alloc.alloc(layout) };
            assert!(!ptr.is_null());
            assert_eq!(ptr as usize % align, 0, "misaligned for align {align}");
            unsafe { alloc.dealloc(ptr, layout) };
        }
        assert_eq!(alloc.stats().total_bytes(), 0);
    }

    #[cfg(all(feature = "platform-linux", target_os = "linux"))]
    #[test]
    fn drained_slabs_are_recycled() {
        let alloc = test_alloc();
        // Class 0 holds 509 blocks per slab; two slabs' worth forces a second
        // page, and freeing everything must drain both without leaking.
        let layout = Layout::from_size_align(8, 8).unwrap();
        let mut ptrs = Vec::new();
        for _ in 0..1200 {
            // SAFETY: freed below.
            let ptr = unsafe { alloc.alloc(layout) };
            assert!(!ptr.is_null());
            ptrs.push(ptr);
        }
        assert!(alloc.stats().slab_allocs() >= 1200);
        for ptr in ptrs {
            // SAFETY: every pointer came from `alloc` above.
            unsafe { alloc.dealloc(ptr, layout) };
        }
        assert_eq!(alloc.stats().total_bytes(), 0);
        // The allocator still works after full drain (pages came back from
        // the OS just fine).
        let ptr = unsafe { alloc.alloc(layout) };
        assert!(!ptr.is_null());
        unsafe { alloc.dealloc(ptr, layout) };
    }

    #[cfg(all(feature = "platform-linux", target_os = "linux"))]
    #[test]
    fn large_allocations_roundtrip() {
        let alloc = test_alloc();
        let layout = Layout::from_size_align(100_000, 16).unwrap();
        // SAFETY: freed below.
        let ptr = unsafe { alloc.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 16, 0);
        unsafe {
            core::ptr::write_bytes(ptr, 0x5A, 100_000);
            alloc.dealloc(ptr, layout);
        }
        assert_eq!(alloc.stats().large_bytes(), 0);
        assert_eq!(alloc.stats().large_allocs(), 1);

        // Over-aligned large request: 64 KiB alignment on a small payload.
        let layout = Layout::from_size_align(100, 65536).unwrap();
        let ptr = unsafe { alloc.alloc(layout) };
        assert!(!ptr.is_null());
        assert_eq!(ptr as usize % 65536, 0);
        unsafe { alloc.dealloc(ptr, layout) };
        assert_eq!(alloc.stats().total_bytes(), 0);
    }

    #[cfg(all(feature = "platform-linux", target_os = "linux"))]
    #[test]
    fn realloc_moves_and_preserves() {
        let alloc = test_alloc();
        let layout = Layout::from_size_align(16, 8).unwrap();
        // SAFETY: all pointers below are managed per the GlobalAlloc contract.
        unsafe {
            let ptr = alloc.alloc(layout);
            assert!(!ptr.is_null());
            core::ptr::write_bytes(ptr, 0xCC, 16);
            // Same class (16 -> 16): must not move.
            let same = alloc.realloc(ptr, layout, 16);
            assert_eq!(same, ptr);
            // Grow across classes (16 -> 2000): moves, bytes preserved.
            let grown = alloc.realloc(ptr, layout, 2000);
            assert!(!grown.is_null());
            assert_ne!(grown, ptr);
            assert!(
                core::slice::from_raw_parts(grown, 16)
                    .iter()
                    .all(|&b| b == 0xCC)
            );
            // Shrink back into a smaller class: bytes preserved.
            let grown_layout = Layout::from_size_align(2000, 8).unwrap();
            let shrunk = alloc.realloc(grown, grown_layout, 32);
            assert!(
                core::slice::from_raw_parts(shrunk, 16)
                    .iter()
                    .all(|&b| b == 0xCC)
            );
            alloc.dealloc(shrunk, Layout::from_size_align(32, 8).unwrap());
        }
        assert_eq!(alloc.stats().total_bytes(), 0);
    }

    #[cfg(all(feature = "platform-linux", target_os = "linux"))]
    #[test]
    fn zero_sized_alloc_is_dangling_nonnull() {
        let alloc = test_alloc();
        let layout = Layout::from_size_align(0, 8).unwrap();
        // SAFETY: zero-sized; dealloc is a no-op.
        unsafe {
            let ptr = alloc.alloc(layout);
            assert!(!ptr.is_null());
            alloc.dealloc(ptr, layout);
        }
        assert_eq!(alloc.stats().total_bytes(), 0);
    }

    #[test]
    fn bump_page_source_serves_ralloc() {
        // Heap-backed so the 1 MiB region never sits on the test stack.
        let region: &'static mut [u8] = Box::leak(vec![0u8; 1 << 20].into_boxed_slice());
        let (base, len) = (region.as_mut_ptr() as usize, region.len());
        let alloc = Ralloc::new(BumpPageSource::new(base, len));
        let layout = Layout::from_size_align(64, 8).unwrap();
        let mut ptrs = Vec::new();
        for _ in 0..100 {
            // SAFETY: freed below.
            let ptr = unsafe { alloc.alloc(layout) };
            assert!(!ptr.is_null());
            assert_eq!(ptr as usize % 8, 0);
            ptrs.push(ptr);
        }
        for ptr in ptrs {
            // SAFETY: allocated above.
            unsafe { alloc.dealloc(ptr, layout) };
        }
        assert_eq!(alloc.stats().total_bytes(), 0);
        assert!(alloc.stats().slab_allocs() >= 100);
    }

    #[test]
    fn class_index_covers_alignment() {
        assert_eq!(class_index(8, 8), Some(0));
        assert_eq!(class_index(9, 8), Some(1));
        assert_eq!(class_index(2048, 8), Some(8));
        assert_eq!(class_index(2048, 2048), Some(8));
        assert_eq!(class_index(2049, 8), None);
        assert_eq!(class_index(8, 4096), None);
        assert_eq!(class_index(1, 1), Some(0));
    }
}

//! Switching global allocator: firmware pool → kernel heap (Phase 6.2).
//!
//! The boot binary needs `alloc` in two different worlds. While UEFI boot
//! services are up, allocations come from the firmware pool (uefi-rs's
//! [`uefi::allocator::Allocator`]). After `ExitBootServices()` that pool is
//! gone, so [`kernel_entry`](crate::kernel) installs the kernel's own heap —
//! the same audited `linked_list_allocator` intrusive free-list backing
//! `enlil-platform`'s bare-metal `GlobalAlloc` — in a conventional-memory
//! region picked from the boot handoff, and every later allocation is served
//! from it with no firmware involved.
//!
//! A firmware-pool allocation that outlives the switch (e.g. the uefi-rs
//! logger's buffers) is deliberately leaked on dealloc: its backing pages are
//! boot-services memory the kernel reclaims wholesale when it takes over the
//! memory map, and handing a foreign pointer to the kernel heap would corrupt
//! the free list.
//!
//! Everything here is gated to the firmware target; the dev-host build has a
//! real OS allocator and never routes through this module.

#[cfg(target_os = "uefi")]
pub use hw::{install_kernel_heap, kernel_heap_active};

#[cfg(target_os = "uefi")]
mod hw {
    use core::alloc::{GlobalAlloc, Layout};
    use core::sync::atomic::{AtomicUsize, Ordering};
    use linked_list_allocator::LockedHeap;

    /// Routes allocations to the firmware pool until the kernel heap is
    /// installed, then to the kernel heap.
    struct BootAllocator {
        heap: LockedHeap,
        base: AtomicUsize,
        /// Nonzero once the kernel heap is live — the routing switch.
        size: AtomicUsize,
    }

    #[global_allocator]
    static ALLOCATOR: BootAllocator = BootAllocator {
        heap: LockedHeap::empty(),
        base: AtomicUsize::new(0),
        size: AtomicUsize::new(0),
    };

    impl BootAllocator {
        fn kernel_active(&self) -> bool {
            self.size.load(Ordering::Acquire) != 0
        }

        /// Whether `ptr` lies inside the kernel heap span (a pointer that
        /// does not was handed out by the firmware pool before the switch).
        fn owns(&self, ptr: *mut u8) -> bool {
            let base = self.base.load(Ordering::Acquire);
            let size = self.size.load(Ordering::Acquire);
            (ptr as usize) >= base && (ptr as usize) < base.saturating_add(size)
        }
    }

    unsafe impl GlobalAlloc for BootAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if self.kernel_active() {
                unsafe { self.heap.alloc(layout) }
            } else {
                // Returns null once boot services are gone, so an allocation
                // in the exit→install window fails cleanly instead of
                // touching a dead firmware table.
                unsafe { uefi::allocator::Allocator.alloc(layout) }
            }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if self.kernel_active() {
                if self.owns(ptr) {
                    unsafe { self.heap.dealloc(ptr, layout) };
                }
                // else: a firmware-pool allocation freed after the switch —
                // leaked by design (see module docs).
            } else {
                unsafe { uefi::allocator::Allocator.dealloc(ptr, layout) };
            }
        }
    }

    /// Whether the kernel heap has been installed and is serving allocations.
    pub fn kernel_heap_active() -> bool {
        ALLOCATOR.kernel_active()
    }

    /// Install the kernel heap over `[base, base + size)` and switch every
    /// subsequent allocation to it.
    ///
    /// # Safety
    ///
    /// The span must be conventional memory owned by the caller, unused by
    /// anything else (firmware image, boot stack, handed-off buffers), and
    /// this must be called at most once, before any concurrent allocation.
    pub unsafe fn install_kernel_heap(base: *mut u8, size: usize) {
        unsafe { ALLOCATOR.heap.lock().init(base, size) };
        ALLOCATOR.base.store(base as usize, Ordering::Release);
        // Publishing a nonzero size flips the routing switch, so it goes last.
        ALLOCATOR.size.store(size, Ordering::Release);
    }
}

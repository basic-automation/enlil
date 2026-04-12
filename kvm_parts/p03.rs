// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━ MSR constants ━━━━━━━━━━━━━━━━━━━━━━━━━━

/// Well-known MSR indices used for filtering.
#[cfg(target_os = "linux")]
pub mod msr_index {
    pub const MSR_IA32_APERF: u32 = 0x000000E8;
    pub const MSR_IA32_MPERF: u32 = 0x000000E7;
    pub const MSR_IA32_TSC: u32 = 0x00000010;
    pub const MSR_IA32_TSC_ADJUST: u32 = 0x0000003B;
    pub const MSR_IA32_MISC_ENABLE: u32 = 0x000001A0;
    pub const MSR_IA32_FEATURE_CONTROL: u32 = 0x0000003A;
}

// ━━━━━━━━━━━━━━━━━━━━━━ MemorySlot / SlotAllocator ━━━━━━━━━━━━━━━━━

/// Bookkeeping for a single KVM memory region.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct MemorySlot {
    pub slot: u32,
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
    pub flags: u32,
}

/// Simple monotonic slot id allocator.
#[cfg(target_os = "linux")]
#[derive(Debug)]
struct SlotAllocator {
    next: AtomicU32,
}

#[cfg(target_os = "linux")]
impl SlotAllocator {
    fn new() -> Self {
        Self { next: AtomicU32::new(0) }
    }
    fn alloc(&self) -> u32 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

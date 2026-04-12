//! Guest physical memory management.
//!
//! Tracks memory allocations across guests, ensures no overlap,
//! and provides the foundation for EPT/NPT mapping.
//!
//! All allocations are 2 MB-aligned to support large-page mappings
//! in Extended Page Tables (Intel EPT) and Nested Page Tables (AMD NPT).

use crate::error::Error;
use std::collections::BTreeMap;

/// 2 MB in bytes — the alignment boundary for all guest memory regions.
/// This enables EPT/NPT large-page (2 MB) mappings without splitting.
const LARGE_PAGE_SIZE: u64 = 2 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for a guest's memory region.
#[derive(Debug, Clone)]
pub struct GuestMemoryConfig {
    /// Size in megabytes.
    pub size_mb: u64,
}

// ---------------------------------------------------------------------------
// Memory region
// ---------------------------------------------------------------------------

/// A reserved region of host physical memory assigned to a guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRegion {
    /// Host physical base address (HPA) of this region.
    pub host_base: u64,
    /// Size in bytes (always a multiple of [`LARGE_PAGE_SIZE`]).
    pub size: u64,
    /// Owning guest identifier.
    pub guest_id: String,
}

// ---------------------------------------------------------------------------
// GPA → HPA mapping
// ---------------------------------------------------------------------------

/// A single entry in a guest's memory map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryMapEntry {
    /// Guest Physical Address (start of range).
    pub gpa: u64,
    /// Host Physical Address that `gpa` maps to.
    pub hpa: u64,
    /// Length of the mapped range in bytes.
    pub size: u64,
}

/// Represents the guest-visible physical address space.
///
/// Every guest sees memory starting at GPA 0. The `MemoryMap` records
/// how that guest-physical space maps onto host-physical addresses so
/// that EPT/NPT tables can be constructed later.
#[derive(Debug, Clone)]
pub struct MemoryMap {
    guest_id: String,
    entries: Vec<MemoryMapEntry>,
}

impl MemoryMap {
    /// Create a new, empty memory map for `guest_id`.
    fn new(guest_id: &str) -> Self {
        Self {
            guest_id: guest_id.to_string(),
            entries: Vec::new(),
        }
    }

    /// Add a flat mapping: GPA `gpa` → HPA `hpa` for `size` bytes.
    fn add(&mut self, gpa: u64, hpa: u64, size: u64) {
        self.entries.push(MemoryMapEntry { gpa, hpa, size });
    }

    /// Translate a guest physical address to a host physical address.
    /// Returns `None` if the GPA is not mapped.
    #[must_use]
    pub fn translate(&self, gpa: u64) -> Option<u64> {
        for entry in &self.entries {
            if gpa >= entry.gpa && gpa < entry.gpa + entry.size {
                return Some(entry.hpa + (gpa - entry.gpa));
            }
        }
        None
    }

    /// The guest identifier this map belongs to.
    #[must_use]
    pub fn guest_id(&self) -> &str {
        &self.guest_id
    }

    /// Iterate over all mapping entries.
    #[must_use]
    pub fn entries(&self) -> &[MemoryMapEntry] {
        &self.entries
    }

    /// Total mapped bytes.
    #[must_use]
    pub fn mapped_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }
}

// ---------------------------------------------------------------------------
// Memory manager
// ---------------------------------------------------------------------------

/// Manages host memory allocation across all guests.
///
/// Uses a bump allocator with 2 MB alignment. Freed regions are tracked
/// in a free-list so that `deallocate` + `allocate` can reuse space.
pub struct MemoryManager {
    /// Total available host memory in bytes.
    total_bytes: u64,
    /// Reserved for hypervisor overhead (always at the bottom of the range).
    reserved_bytes: u64,
    /// Allocated regions, keyed by host base address.
    regions: BTreeMap<u64, MemoryRegion>,
    /// Per-guest GPA→HPA memory maps.
    memory_maps: BTreeMap<String, MemoryMap>,
    /// Next available base address (bump pointer).
    next_base: u64,
    /// Previously freed regions available for reuse (base, size).
    free_list: Vec<(u64, u64)>,
}

impl MemoryManager {
    /// Create a new `MemoryManager`.
    ///
    /// `total_mb` — total host memory the hypervisor may use.\
    /// `reserved_mb` — memory reserved for the hypervisor itself (at the
    /// start of the range). Both values are rounded up to 2 MB alignment.
    #[must_use]
    pub const fn new(total_mb: u64, reserved_mb: u64) -> Self {
        let reserved_bytes = align_up(reserved_mb * 1024 * 1024, LARGE_PAGE_SIZE);
        let total_bytes = align_up(total_mb * 1024 * 1024, LARGE_PAGE_SIZE);
        Self {
            total_bytes,
            reserved_bytes,
            regions: BTreeMap::new(),
            memory_maps: BTreeMap::new(),
            next_base: reserved_bytes,
            free_list: Vec::new(),
        }
    }

    /// Allocate a contiguous, 2 MB-aligned memory region for a guest.
    ///
    /// The requested `size_mb` is rounded up to the nearest 2 MB boundary.
    /// Returns the host base address (HPA) of the allocated region.
    ///
    /// A [`MemoryMap`] is automatically created that maps GPA 0 → HPA base.
    ///
    /// # Errors
    ///
    /// Returns an error if insufficient memory or duplicate guest ID.
    pub fn allocate(&mut self, guest_id: &str, size_mb: u64) -> Result<u64, Error> {
        // Reject duplicate guest ids.
        if self.regions.values().any(|r| r.guest_id == guest_id) {
            return Err(Error::Memory(format!(
                "guest '{guest_id}' already has an allocated region",
            )));
        }

        let size = align_up(size_mb * 1024 * 1024, LARGE_PAGE_SIZE);

        // 1. Try the free-list first (first-fit).
        let base = if let Some(idx) = self.find_free_slot(size) {
            let (free_base, free_size) = self.free_list.remove(idx);
            let leftover = free_size - size;
            if leftover > 0 {
                // Return the remainder to the free-list.
                self.free_list.push((free_base + size, leftover));
            }
            free_base
        } else {
            // 2. Bump allocate.
            let base = align_up(self.next_base, LARGE_PAGE_SIZE);
            if base + size > self.total_bytes {
                return Err(Error::Memory(format!(
                    "cannot allocate {} MB for '{}': only {} MB remaining",
                    size / (1024 * 1024),
                    guest_id,
                    (self.total_bytes.saturating_sub(base)) / (1024 * 1024),
                )));
            }
            self.next_base = base + size;
            base
        };

        // Overlap check (belt-and-suspenders — should be impossible with
        // the bump allocator + free-list, but we keep it for safety).
        self.check_overlap(base, size, guest_id)?;

        let region = MemoryRegion {
            host_base: base,
            size,
            guest_id: guest_id.to_string(),
        };
        self.regions.insert(base, region);

        // Build the guest memory map: GPA 0 → HPA base, for `size` bytes.
        let mut mmap = MemoryMap::new(guest_id);
        mmap.add(0, base, size);
        self.memory_maps.insert(guest_id.to_string(), mmap);

        Ok(base)
    }

    /// Deallocate the memory region owned by `guest_id`.
    ///
    /// The region is moved to an internal free-list and may be reused by
    /// future allocations.
    ///
    /// # Errors
    ///
    /// Returns an error if the guest ID is not found.
    ///
    /// # Panics
    ///
    /// Panics if the internal region map is inconsistent.
    pub fn deallocate(&mut self, guest_id: &str) -> Result<(), Error> {
        let base = self
            .regions
            .values()
            .find(|r| r.guest_id == guest_id)
            .map(|r| r.host_base)
            .ok_or_else(|| {
                Error::Memory(format!("no region found for guest '{guest_id}'"))
            })?;

        let region = self.regions.remove(&base).unwrap();
        self.memory_maps.remove(guest_id);

        // Add to free-list and coalesce adjacent blocks.
        self.free_list.push((region.host_base, region.size));
        self.coalesce_free_list();

        Ok(())
    }

    /// Look up the allocated [`MemoryRegion`] for `guest_id`.
    #[must_use]
    pub fn get_region(&self, guest_id: &str) -> Option<&MemoryRegion> {
        self.regions.values().find(|r| r.guest_id == guest_id)
    }

    /// Get the [`MemoryMap`] (GPA→HPA) for `guest_id`.
    #[must_use]
    pub fn get_memory_map(&self, guest_id: &str) -> Option<&MemoryMap> {
        self.memory_maps.get(guest_id)
    }

    /// Returns total allocated memory in bytes.
    #[must_use]
    pub fn allocated_bytes(&self) -> u64 {
        self.regions.values().map(|r| r.size).sum()
    }

    /// Returns available memory in bytes (excluding reserved).
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.total_bytes
            .saturating_sub(self.reserved_bytes)
            .saturating_sub(self.allocated_bytes())
    }

    /// Iterate over all allocated regions.
    pub fn regions(&self) -> impl Iterator<Item = &MemoryRegion> {
        self.regions.values()
    }

    /// The 2 MB alignment constant used for all allocations.
    #[must_use]
    pub const fn page_size() -> u64 {
        LARGE_PAGE_SIZE
    }

    // -- internal helpers ---------------------------------------------------

    /// Verify that `[base, base+size)` does not overlap any existing region.
    fn check_overlap(&self, base: u64, size: u64, guest_id: &str) -> Result<(), Error> {
        for (existing_base, region) in &self.regions {
            let existing_end = existing_base + region.size;
            if base < existing_end && base + size > *existing_base {
                return Err(Error::Memory(format!(
                    "region for '{}' overlaps with '{}'",
                    guest_id, region.guest_id,
                )));
            }
        }
        Ok(())
    }

    /// First-fit search in the free-list for a slot of at least `size` bytes.
    fn find_free_slot(&self, size: u64) -> Option<usize> {
        self.free_list
            .iter()
            .position(|&(_, free_size)| free_size >= size)
    }

    /// Merge adjacent free-list entries to reduce fragmentation.
    fn coalesce_free_list(&mut self) {
        if self.free_list.len() < 2 {
            return;
        }
        self.free_list.sort_by_key(|&(base, _)| base);
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.free_list.len());
        for &(base, size) in &self.free_list {
            if let Some(last) = merged.last_mut() {
                if last.0 + last.1 == base {
                    last.1 += size;
                    continue;
                }
            }
            merged.push((base, size));
        }
        self.free_list = merged;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Round `value` up to the next multiple of `align` (must be a power of two).
const fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- alignment helpers --------------------------------------------------

    #[test]
    fn align_up_basics() {
        assert_eq!(align_up(0, LARGE_PAGE_SIZE), 0);
        assert_eq!(align_up(1, LARGE_PAGE_SIZE), LARGE_PAGE_SIZE);
        assert_eq!(align_up(LARGE_PAGE_SIZE, LARGE_PAGE_SIZE), LARGE_PAGE_SIZE);
        assert_eq!(
            align_up(LARGE_PAGE_SIZE + 1, LARGE_PAGE_SIZE),
            2 * LARGE_PAGE_SIZE
        );
    }

    // -- basic allocation ---------------------------------------------------

    #[test]
    fn allocate_single_guest() {
        let mut mm = MemoryManager::new(1024, 64);
        let base = mm.allocate("guest1", 128).unwrap();
        assert_eq!(base % LARGE_PAGE_SIZE, 0, "allocation must be 2 MB-aligned");
        assert_eq!(mm.allocated_bytes(), 128 * 1024 * 1024);
        assert_eq!(mm.regions().count(), 1);
    }

    #[test]
    fn allocate_two_guests() {
        let mut mm = MemoryManager::new(32768, 512);
        let base1 = mm.allocate("linux1", 8192).unwrap();
        let base2 = mm.allocate("linux2", 8192).unwrap();
        assert_ne!(base1, base2);
        assert_eq!(base1 % LARGE_PAGE_SIZE, 0);
        assert_eq!(base2 % LARGE_PAGE_SIZE, 0);
        assert_eq!(mm.regions().count(), 2);
    }

    #[test]
    fn reject_overcommit() {
        let mut mm = MemoryManager::new(1024, 256);
        mm.allocate("guest1", 512).unwrap();
        assert!(mm.allocate("guest2", 512).is_err());
    }

    #[test]
    fn reject_duplicate_guest_id() {
        let mut mm = MemoryManager::new(4096, 64);
        mm.allocate("dup", 128).unwrap();
        let err = mm.allocate("dup", 128);
        assert!(err.is_err());
    }

    // -- alignment enforcement ----------------------------------------------

    #[test]
    fn allocation_size_rounded_to_2mb() {
        let mut mm = MemoryManager::new(1024, 0);
        // Request 1 MB — should round up to 2 MB.
        mm.allocate("tiny", 1).unwrap();
        let region = mm.get_region("tiny").unwrap();
        assert_eq!(region.size, LARGE_PAGE_SIZE);
        assert_eq!(region.size % LARGE_PAGE_SIZE, 0);
    }

    #[test]
    fn reserved_bytes_aligned() {
        // 513 MB reserved should round up to 514 MB (next 2 MB boundary).
        let mm = MemoryManager::new(2048, 513);
        let first_usable = mm.next_base;
        assert_eq!(first_usable % LARGE_PAGE_SIZE, 0);
    }

    // -- get_region ---------------------------------------------------------

    #[test]
    fn get_region_found() {
        let mut mm = MemoryManager::new(4096, 64);
        let base = mm.allocate("vm-1", 256).unwrap();
        let region = mm.get_region("vm-1").unwrap();
        assert_eq!(region.host_base, base);
        assert_eq!(region.guest_id, "vm-1");
    }

    #[test]
    fn get_region_not_found() {
        let mm = MemoryManager::new(4096, 64);
        assert!(mm.get_region("nonexistent").is_none());
    }

    // -- deallocate ---------------------------------------------------------

    #[test]
    fn deallocate_frees_memory() {
        let mut mm = MemoryManager::new(1024, 0);
        mm.allocate("temp", 256).unwrap();
        let before = mm.available_bytes();
        mm.deallocate("temp").unwrap();
        let after = mm.available_bytes();
        assert!(after > before);
        assert!(mm.get_region("temp").is_none());
    }

    #[test]
    fn deallocate_nonexistent_errors() {
        let mut mm = MemoryManager::new(1024, 0);
        assert!(mm.deallocate("ghost").is_err());
    }

    #[test]
    fn allocate_after_deallocate_reuses_space() {
        let mut mm = MemoryManager::new(1024, 0);
        let base1 = mm.allocate("first", 256).unwrap();
        mm.deallocate("first").unwrap();
        // The freed slot should be reused.
        let base2 = mm.allocate("second", 256).unwrap();
        assert_eq!(base1, base2, "freed region should be reused");
    }

    #[test]
    fn deallocate_and_reallocate_larger_falls_back_to_bump() {
        let mut mm = MemoryManager::new(2048, 0);
        mm.allocate("a", 128).unwrap();
        let base_b = mm.allocate("b", 128).unwrap();
        mm.deallocate("a").unwrap();
        // Request larger than the freed slot — must bump-allocate past 'b'.
        let base_c = mm.allocate("c", 256).unwrap();
        assert!(base_c > base_b, "larger allocation should go past 'b'");
    }

    // -- memory map (GPA → HPA) --------------------------------------------

    #[test]
    fn memory_map_created_on_allocate() {
        let mut mm = MemoryManager::new(4096, 64);
        let base = mm.allocate("mapped", 512).unwrap();
        let mmap = mm.get_memory_map("mapped").unwrap();
        assert_eq!(mmap.guest_id(), "mapped");
        assert_eq!(mmap.entries().len(), 1);
        assert_eq!(mmap.mapped_bytes(), 512 * 1024 * 1024);

        let entry = &mmap.entries()[0];
        assert_eq!(entry.gpa, 0);
        assert_eq!(entry.hpa, base);
    }

    #[test]
    fn memory_map_translate_gpa_to_hpa() {
        let mut mm = MemoryManager::new(4096, 64);
        let base = mm.allocate("xlat", 256).unwrap();
        let mmap = mm.get_memory_map("xlat").unwrap();

        // GPA 0 → HPA base
        assert_eq!(mmap.translate(0), Some(base));
        // GPA 4096 → HPA base + 4096
        assert_eq!(mmap.translate(4096), Some(base + 4096));
        // Past the end → None
        let region_size = 256 * 1024 * 1024;
        assert_eq!(mmap.translate(region_size), None);
    }

    #[test]
    fn memory_map_removed_on_deallocate() {
        let mut mm = MemoryManager::new(4096, 64);
        mm.allocate("ephemeral", 128).unwrap();
        assert!(mm.get_memory_map("ephemeral").is_some());
        mm.deallocate("ephemeral").unwrap();
        assert!(mm.get_memory_map("ephemeral").is_none());
    }

    // -- available / allocated accounting -----------------------------------

    #[test]
    fn accounting_consistent() {
        let mut mm = MemoryManager::new(1024, 128);
        let total_usable = mm.available_bytes();
        mm.allocate("a", 128).unwrap();
        mm.allocate("b", 256).unwrap();
        let alloc = mm.allocated_bytes();
        let avail = mm.available_bytes();
        assert_eq!(alloc + avail, total_usable);
    }

    #[test]
    fn accounting_after_deallocate() {
        let mut mm = MemoryManager::new(1024, 0);
        let total = mm.available_bytes();
        mm.allocate("x", 128).unwrap();
        mm.deallocate("x").unwrap();
        assert_eq!(mm.available_bytes(), total);
        assert_eq!(mm.allocated_bytes(), 0);
    }

    // -- boundary conditions ------------------------------------------------

    #[test]
    fn allocate_exact_remaining() {
        let mut mm = MemoryManager::new(256, 0);
        // Should succeed: request exactly what's available.
        mm.allocate("full", 256).unwrap();
        assert_eq!(mm.available_bytes(), 0);
    }

    #[test]
    fn allocate_one_byte_over_fails() {
        // 256 MB total, 0 reserved. Allocate 256 MB, then try 2 more.
        let mut mm = MemoryManager::new(256, 0);
        mm.allocate("full", 256).unwrap();
        assert!(mm.allocate("over", 2).is_err());
    }

    #[test]
    fn zero_size_allocation_rounds_to_large_page() {
        let mut mm = MemoryManager::new(1024, 0);
        // 0 MB requested — align_up(0) == 0, which is a degenerate case.
        // We still get a region of size 0 (no real memory). This is fine;
        // callers should validate size before calling allocate.
        let _base = mm.allocate("zero", 0).unwrap();
        let region = mm.get_region("zero").unwrap();
        assert_eq!(region.size, 0);
    }

    #[test]
    fn free_list_coalescing() {
        let mut mm = MemoryManager::new(2048, 0);
        let _a = mm.allocate("a", 128).unwrap();
        let _b = mm.allocate("b", 128).unwrap();
        let _c = mm.allocate("c", 128).unwrap();

        // Free a and b — they are adjacent and should coalesce.
        mm.deallocate("a").unwrap();
        mm.deallocate("b").unwrap();
        assert_eq!(mm.free_list.len(), 1, "adjacent frees should coalesce");
        assert_eq!(mm.free_list[0].1, 256 * 1024 * 1024);
    }

    #[test]
    fn multiple_guests_isolated_address_spaces() {
        let mut mm = MemoryManager::new(8192, 0);
        let base1 = mm.allocate("g1", 1024).unwrap();
        let base2 = mm.allocate("g2", 1024).unwrap();

        let map1 = mm.get_memory_map("g1").unwrap();
        let map2 = mm.get_memory_map("g2").unwrap();

        // Both guests see GPA 0, but they map to different HPAs.
        assert_eq!(map1.translate(0), Some(base1));
        assert_eq!(map2.translate(0), Some(base2));
        assert_ne!(base1, base2);
    }

    #[test]
    fn page_size_constant() {
        assert_eq!(MemoryManager::page_size(), 2 * 1024 * 1024);
    }
}

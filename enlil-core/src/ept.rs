//! EPT/NPT page table management for guest memory isolation.
//!
//! Provides per-guest Extended Page Tables with:
//! - Write-protection toggling per page (for snapshots, live migration, fault tolerance)
//! - Dirty page bitmap tracking (for iterative dirty page transfer)
//! - Large page (2MB) support for performance
//!
//! These are first-class operations as required by Phase 2.4, designed for:
//! - Phase 8.12: Snapshots use copy-on-write via EPT write-protect faults
//! - Phase 11.5.1: Live migration uses iterative dirty page transfer
//! - Phase 11.10: Fault tolerance uses continuous dirty page streaming

use std::collections::{BTreeMap, HashMap};
use std::fmt;

/// Page size constants.
pub const PAGE_SIZE_4K: u64 = 4096;
pub const PAGE_SIZE_2M: u64 = 2 * 1024 * 1024;
pub const PAGE_SIZE_1G: u64 = 1024 * 1024 * 1024;

/// EPT permission flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EptPermissions {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl EptPermissions {
    pub const RWX: Self = Self { read: true, write: true, execute: true };
    pub const RX: Self = Self { read: true, write: false, execute: true };
    pub const RW: Self = Self { read: true, write: true, execute: false };
    pub const RO: Self = Self { read: true, write: false, execute: false };
    pub const NONE: Self = Self { read: false, write: false, execute: false };
}

impl Default for EptPermissions {
    fn default() -> Self { Self::RWX }
}

/// Memory type for EPT entries (PAT-like).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum EptMemoryType {
    Uncacheable = 0,
    WriteCombining = 1,
    WriteThrough = 4,
    WriteProtected = 5,
    #[default]
    WriteBack = 6,
}


/// Page size used for a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    Page4K,
    Page2M,
    Page1G,
}

impl PageSize {
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        match self {
            Self::Page4K => PAGE_SIZE_4K,
            Self::Page2M => PAGE_SIZE_2M,
            Self::Page1G => PAGE_SIZE_1G,
        }
    }
}

impl fmt::Display for PageSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Page4K => write!(f, "4K"),
            Self::Page2M => write!(f, "2M"),
            Self::Page1G => write!(f, "1G"),
        }
    }
}

/// A single EPT mapping entry.
#[derive(Debug, Clone)]
pub struct EptMapping {
    /// Guest physical address (page-aligned).
    pub gpa: u64,
    /// Host physical address (page-aligned).
    pub hpa: u64,
    /// Page size for this mapping.
    pub page_size: PageSize,
    /// Current permissions.
    pub permissions: EptPermissions,
    /// Memory type.
    pub memory_type: EptMemoryType,
    /// Whether this page has been written to since last dirty bitmap clear.
    pub dirty: bool,
    /// Whether write-protection is currently active (overrides permissions.write).
    pub write_protected: bool,
}

impl EptMapping {
    /// Effective permissions considering write-protection override.
    #[must_use]
    pub const fn effective_permissions(&self) -> EptPermissions {
        if self.write_protected {
            EptPermissions {
                write: false,
                ..self.permissions
            }
        } else {
            self.permissions
        }
    }
}

/// Per-guest EPT page table manager.
///
/// Manages the GPA→HPA mappings with write-protection and dirty tracking.
/// On Linux/KVM, these translate to KVM memory slot operations.
/// On bare-metal, these will directly manipulate hardware EPT/NPT structures.
pub struct EptManager {
    /// Guest identifier.
    guest_id: String,
    /// All mappings, keyed by GPA (page-aligned).
    mappings: BTreeMap<u64, EptMapping>,
    /// Write-protected GPAs (quick lookup set).
    write_protected_pages: HashMap<u64, ()>,
    /// Dirty page tracking: pages written since last bitmap clear.
    dirty_pages: HashMap<u64, ()>,
    /// Whether dirty tracking is enabled.
    dirty_tracking_enabled: bool,
    /// Statistics.
    stats: EptStats,
}

/// EPT operation statistics.
#[derive(Debug, Default, Clone)]
pub struct EptStats {
    pub total_mappings: u64,
    pub write_protected_count: u64,
    pub dirty_page_count: u64,
    pub total_mapped_bytes: u64,
    pub map_operations: u64,
    pub unmap_operations: u64,
    pub protect_operations: u64,
    pub dirty_clears: u64,
}

impl EptManager {
    /// Create a new EPT manager for a guest.
    #[must_use]
    pub fn new(guest_id: &str) -> Self {
        Self {
            guest_id: guest_id.to_string(),
            mappings: BTreeMap::new(),
            write_protected_pages: HashMap::new(),
            dirty_pages: HashMap::new(),
            dirty_tracking_enabled: false,
            stats: EptStats::default(),
        }
    }

    #[must_use]
    pub fn guest_id(&self) -> &str {
        &self.guest_id
    }

    #[must_use]
    pub const fn stats(&self) -> &EptStats {
        &self.stats
    }

    /// Map a guest physical address range to a host physical address.
    ///
    /// Both `gpa` and `hpa` must be aligned to `page_size`.
    pub fn map(
        &mut self,
        gpa: u64,
        hpa: u64,
        page_size: PageSize,
        permissions: EptPermissions,
        memory_type: EptMemoryType,
    ) -> Result<(), EptError> {
        let size = page_size.bytes();

        // Alignment checks
        if !gpa.is_multiple_of(size) {
            return Err(EptError::MisalignedGpa { gpa, required_alignment: size });
        }
        if !hpa.is_multiple_of(size) {
            return Err(EptError::MisalignedHpa { hpa, required_alignment: size });
        }

        // Check for overlapping mappings
        if self.mappings.contains_key(&gpa) {
            return Err(EptError::AlreadyMapped { gpa });
        }

        let mapping = EptMapping {
            gpa,
            hpa,
            page_size,
            permissions,
            memory_type,
            dirty: false,
            write_protected: false,
        };

        self.mappings.insert(gpa, mapping);
        self.stats.total_mappings += 1;
        self.stats.total_mapped_bytes += size;
        self.stats.map_operations += 1;

        Ok(())
    }

    /// Map a contiguous range of memory using the largest page size possible.
    ///
    /// Maps `size` bytes from `gpa_base` to `hpa_base`, preferring 2MB pages
    /// when alignment allows, falling back to 4KB pages.
    pub fn map_range(
        &mut self,
        gpa_base: u64,
        hpa_base: u64,
        size: u64,
        permissions: EptPermissions,
        memory_type: EptMemoryType,
    ) -> Result<u64, EptError> {
        let mut offset = 0u64;
        let mut pages_mapped = 0u64;

        while offset < size {
            let remaining = size - offset;
            let gpa = gpa_base + offset;
            let hpa = hpa_base + offset;

            // Try 2MB page if aligned and enough space
            let page_size = if gpa.is_multiple_of(PAGE_SIZE_2M)
                && hpa.is_multiple_of(PAGE_SIZE_2M)
                && remaining >= PAGE_SIZE_2M
            {
                PageSize::Page2M
            } else {
                PageSize::Page4K
            };

            self.map(gpa, hpa, page_size, permissions, memory_type)?;
            offset += page_size.bytes();
            pages_mapped += 1;
        }

        Ok(pages_mapped)
    }

    /// Unmap a guest physical address.
    pub fn unmap(&mut self, gpa: u64) -> Result<EptMapping, EptError> {
        let mapping = self.mappings.remove(&gpa)
            .ok_or(EptError::NotMapped { gpa })?;

        self.write_protected_pages.remove(&gpa);
        self.dirty_pages.remove(&gpa);
        self.stats.total_mappings -= 1;
        self.stats.total_mapped_bytes -= mapping.page_size.bytes();
        self.stats.unmap_operations += 1;

        if mapping.write_protected {
            self.stats.write_protected_count -= 1;
        }

        Ok(mapping)
    }

    /// Look up a mapping by GPA.
    #[must_use]
    pub fn lookup(&self, gpa: u64) -> Option<&EptMapping> {
        self.mappings.get(&gpa)
    }

    // -----------------------------------------------------------------------
    // Write-protection (Phase 8.12 snapshots, Phase 11.10 fault tolerance)
    // -----------------------------------------------------------------------

    /// Set write-protection on a guest page.
    ///
    /// The page retains its read/execute permissions but writes will cause
    /// an EPT violation (VM exit), allowing the hypervisor to implement
    /// copy-on-write semantics.
    pub fn ept_set_write_protect(&mut self, gpa: u64) -> Result<(), EptError> {
        let mapping = self.mappings.get_mut(&gpa)
            .ok_or(EptError::NotMapped { gpa })?;

        if !mapping.write_protected {
            mapping.write_protected = true;
            self.write_protected_pages.insert(gpa, ());
            self.stats.write_protected_count += 1;
            self.stats.protect_operations += 1;
        }

        Ok(())
    }

    /// Clear write-protection on a guest page.
    pub fn ept_clear_write_protect(&mut self, gpa: u64) -> Result<(), EptError> {
        let mapping = self.mappings.get_mut(&gpa)
            .ok_or(EptError::NotMapped { gpa })?;

        if mapping.write_protected {
            mapping.write_protected = false;
            self.write_protected_pages.remove(&gpa);
            self.stats.write_protected_count -= 1;
            self.stats.protect_operations += 1;
        }

        Ok(())
    }

    /// Write-protect all mapped pages in this guest's address space.
    ///
    /// Used when taking a snapshot — every page becomes copy-on-write.
    pub fn write_protect_all(&mut self) -> u64 {
        let mut count = 0u64;
        let gpas: Vec<u64> = self.mappings.keys().copied().collect();
        for gpa in gpas {
            if let Some(mapping) = self.mappings.get_mut(&gpa) {
                if !mapping.write_protected {
                    mapping.write_protected = true;
                    self.write_protected_pages.insert(gpa, ());
                    self.stats.write_protected_count += 1;
                    count += 1;
                }
            }
        }
        self.stats.protect_operations += 1;
        count
    }

    /// Clear write-protection on all pages.
    pub fn clear_all_write_protect(&mut self) -> u64 {
        let count = self.write_protected_pages.len() as u64;
        let gpas: Vec<u64> = self.write_protected_pages.keys().copied().collect();
        for gpa in gpas {
            if let Some(mapping) = self.mappings.get_mut(&gpa) {
                mapping.write_protected = false;
            }
        }
        self.write_protected_pages.clear();
        self.stats.write_protected_count = 0;
        self.stats.protect_operations += 1;
        count
    }

    /// Check if a page is write-protected.
    #[must_use]
    pub fn is_write_protected(&self, gpa: u64) -> bool {
        self.write_protected_pages.contains_key(&gpa)
    }

    // -----------------------------------------------------------------------
    // Dirty page tracking (Phase 11.5.1 live migration, Phase 11.10 FT)
    // -----------------------------------------------------------------------

    /// Enable dirty page tracking.
    pub const fn enable_dirty_tracking(&mut self) {
        self.dirty_tracking_enabled = true;
    }

    /// Disable dirty page tracking and clear dirty state.
    pub fn disable_dirty_tracking(&mut self) {
        self.dirty_tracking_enabled = false;
        self.dirty_pages.clear();
        for mapping in self.mappings.values_mut() {
            mapping.dirty = false;
        }
        self.stats.dirty_page_count = 0;
    }

    /// Mark a page as dirty (called on EPT write-protect violation).
    ///
    /// In the real implementation, this is called from the VM exit handler
    /// when a write to a write-protected page occurs.
    pub fn mark_dirty(&mut self, gpa: u64) -> Result<(), EptError> {
        let mapping = self.mappings.get_mut(&gpa)
            .ok_or(EptError::NotMapped { gpa })?;

        if self.dirty_tracking_enabled && !mapping.dirty {
            mapping.dirty = true;
            self.dirty_pages.insert(gpa, ());
            self.stats.dirty_page_count += 1;
        }

        Ok(())
    }

    /// Get the dirty page bitmap and clear all dirty flags.
    ///
    /// Returns a vector of GPAs that have been written to since the last call.
    /// This is the core primitive for iterative dirty page transfer in live migration.
    pub fn ept_get_and_clear_dirty_bitmap(&mut self) -> Vec<u64> {
        let dirty: Vec<u64> = self.dirty_pages.keys().copied().collect();

        // Clear dirty state
        for &gpa in &dirty {
            if let Some(mapping) = self.mappings.get_mut(&gpa) {
                mapping.dirty = false;
            }
        }
        self.dirty_pages.clear();
        self.stats.dirty_page_count = 0;
        self.stats.dirty_clears += 1;

        dirty
    }

    /// Number of currently dirty pages.
    #[must_use]
    pub fn dirty_page_count(&self) -> usize {
        self.dirty_pages.len()
    }

    // -----------------------------------------------------------------------
    // Iteration and queries
    // -----------------------------------------------------------------------

    /// Total number of mapped pages.
    #[must_use]
    pub fn mapping_count(&self) -> usize {
        self.mappings.len()
    }

    /// Total mapped bytes.
    #[must_use]
    pub const fn total_mapped_bytes(&self) -> u64 {
        self.stats.total_mapped_bytes
    }

    /// Iterate over all mappings.
    pub fn mappings(&self) -> impl Iterator<Item = &EptMapping> {
        self.mappings.values()
    }

    /// Iterate over all write-protected GPAs.
    pub fn write_protected_pages(&self) -> impl Iterator<Item = u64> + '_ {
        self.write_protected_pages.keys().copied()
    }

    /// Handle an EPT violation (write to write-protected page).
    ///
    /// Returns the mapping info so the caller can implement copy-on-write.
    /// Marks the page dirty and optionally clears write-protection.
    pub fn handle_ept_violation(
        &mut self,
        gpa: u64,
        clear_protection: bool,
    ) -> Result<EptMapping, EptError> {
        // Mark dirty
        self.mark_dirty(gpa)?;

        // Optionally clear write-protection (for one-shot COW)
        if clear_protection {
            self.ept_clear_write_protect(gpa)?;
        }

        self.mappings.get(&gpa)
            .cloned()
            .ok_or(EptError::NotMapped { gpa })
    }
}

/// EPT operation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EptError {
    MisalignedGpa { gpa: u64, required_alignment: u64 },
    MisalignedHpa { hpa: u64, required_alignment: u64 },
    AlreadyMapped { gpa: u64 },
    NotMapped { gpa: u64 },
    OverlappingMapping { gpa: u64, existing_gpa: u64 },
}

impl fmt::Display for EptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MisalignedGpa { gpa, required_alignment } =>
                write!(f, "GPA {gpa:#x} not aligned to {required_alignment:#x}"),
            Self::MisalignedHpa { hpa, required_alignment } =>
                write!(f, "HPA {hpa:#x} not aligned to {required_alignment:#x}"),
            Self::AlreadyMapped { gpa } =>
                write!(f, "GPA {gpa:#x} already mapped"),
            Self::NotMapped { gpa } =>
                write!(f, "GPA {gpa:#x} not mapped"),
            Self::OverlappingMapping { gpa, existing_gpa } =>
                write!(f, "GPA {gpa:#x} overlaps with existing mapping at {existing_gpa:#x}"),
        }
    }
}

impl std::error::Error for EptError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_map_and_lookup() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x1000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        let m = ept.lookup(0x0).unwrap();
        assert_eq!(m.hpa, 0x1000_0000);
        assert_eq!(m.page_size, PageSize::Page4K);
        assert_eq!(ept.mapping_count(), 1);
    }

    #[test]
    fn map_2mb_page() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x0, PageSize::Page2M, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        assert_eq!(ept.total_mapped_bytes(), PAGE_SIZE_2M);
    }

    #[test]
    fn reject_misaligned_gpa() {
        let mut ept = EptManager::new("guest1");
        let result = ept.map(0x1000, 0x0, PageSize::Page2M, EptPermissions::RWX, EptMemoryType::WriteBack);
        assert!(matches!(result, Err(EptError::MisalignedGpa { .. })));
    }

    #[test]
    fn reject_duplicate_mapping() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x1000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        let result = ept.map(0x0, 0x2000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack);
        assert!(matches!(result, Err(EptError::AlreadyMapped { .. })));
    }

    #[test]
    fn unmap() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x1000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        let removed = ept.unmap(0x0).unwrap();
        assert_eq!(removed.hpa, 0x1000_0000);
        assert_eq!(ept.mapping_count(), 0);
        assert!(ept.lookup(0x0).is_none());
    }

    #[test]
    fn write_protect_and_clear() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x1000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();

        // Write-protect
        ept.ept_set_write_protect(0x0).unwrap();
        assert!(ept.is_write_protected(0x0));
        let m = ept.lookup(0x0).unwrap();
        assert!(!m.effective_permissions().write);
        assert!(m.effective_permissions().read);

        // Clear
        ept.ept_clear_write_protect(0x0).unwrap();
        assert!(!ept.is_write_protected(0x0));
        let m = ept.lookup(0x0).unwrap();
        assert!(m.effective_permissions().write);
    }

    #[test]
    fn write_protect_all_and_clear_all() {
        let mut ept = EptManager::new("guest1");
        for i in 0..10 {
            ept.map(i * PAGE_SIZE_4K, i * PAGE_SIZE_4K + 0x1_0000_0000,
                    PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        }

        let count = ept.write_protect_all();
        assert_eq!(count, 10);
        assert_eq!(ept.stats().write_protected_count, 10);

        // All pages should be write-protected
        for i in 0..10 {
            assert!(ept.is_write_protected(i * PAGE_SIZE_4K));
        }

        let cleared = ept.clear_all_write_protect();
        assert_eq!(cleared, 10);
        assert_eq!(ept.stats().write_protected_count, 0);
    }

    #[test]
    fn dirty_tracking() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x1000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        ept.map(PAGE_SIZE_4K, 0x1000_1000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();

        ept.enable_dirty_tracking();

        // Mark pages dirty
        ept.mark_dirty(0x0).unwrap();
        ept.mark_dirty(PAGE_SIZE_4K).unwrap();
        assert_eq!(ept.dirty_page_count(), 2);

        // Get and clear bitmap
        let mut dirty = ept.ept_get_and_clear_dirty_bitmap();
        dirty.sort_unstable();
        assert_eq!(dirty, vec![0x0, PAGE_SIZE_4K]);
        assert_eq!(ept.dirty_page_count(), 0);

        // Second call should return empty
        let dirty2 = ept.ept_get_and_clear_dirty_bitmap();
        assert!(dirty2.is_empty());
    }

    #[test]
    fn dirty_tracking_disabled_by_default() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x1000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();

        // Marking dirty when tracking is disabled should be a no-op
        ept.mark_dirty(0x0).unwrap();
        assert_eq!(ept.dirty_page_count(), 0);
    }

    #[test]
    fn handle_ept_violation_cow() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x1000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        ept.enable_dirty_tracking();
        ept.ept_set_write_protect(0x0).unwrap();

        // Simulate EPT violation: write to write-protected page
        let mapping = ept.handle_ept_violation(0x0, true).unwrap();
        assert_eq!(mapping.hpa, 0x1000_0000);

        // Page should now be dirty and no longer write-protected
        assert!(!ept.is_write_protected(0x0));
        assert_eq!(ept.dirty_page_count(), 1);
    }

    #[test]
    fn map_range_uses_large_pages() {
        let mut ept = EptManager::new("guest1");
        // Map 4MB starting at 0 — should use two 2MB pages
        let pages = ept.map_range(0, 0x1_0000_0000, 4 * 1024 * 1024,
                                   EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        assert_eq!(pages, 2);
        assert_eq!(ept.total_mapped_bytes(), 4 * 1024 * 1024);

        // Both should be 2MB pages
        let m0 = ept.lookup(0x0).unwrap();
        assert_eq!(m0.page_size, PageSize::Page2M);
        let m1 = ept.lookup(PAGE_SIZE_2M).unwrap();
        assert_eq!(m1.page_size, PageSize::Page2M);
    }

    #[test]
    fn map_range_falls_back_to_4k() {
        let mut ept = EptManager::new("guest1");
        // Map 8KB at a 4K-aligned but not 2M-aligned address
        let pages = ept.map_range(PAGE_SIZE_4K, 0x1000_1000, 2 * PAGE_SIZE_4K,
                                   EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        assert_eq!(pages, 2);
        let m = ept.lookup(PAGE_SIZE_4K).unwrap();
        assert_eq!(m.page_size, PageSize::Page4K);
    }

    #[test]
    fn stats_tracking() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x1000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();
        assert_eq!(ept.stats().map_operations, 1);

        ept.ept_set_write_protect(0x0).unwrap();
        assert_eq!(ept.stats().protect_operations, 1);

        ept.unmap(0x0).unwrap();
        assert_eq!(ept.stats().unmap_operations, 1);
    }

    #[test]
    fn idempotent_write_protect() {
        let mut ept = EptManager::new("guest1");
        ept.map(0x0, 0x1000_0000, PageSize::Page4K, EptPermissions::RWX, EptMemoryType::WriteBack).unwrap();

        ept.ept_set_write_protect(0x0).unwrap();
        ept.ept_set_write_protect(0x0).unwrap(); // second call is no-op
        assert_eq!(ept.stats().write_protected_count, 1);

        ept.ept_clear_write_protect(0x0).unwrap();
        ept.ept_clear_write_protect(0x0).unwrap(); // second call is no-op
        assert_eq!(ept.stats().write_protected_count, 0);
    }
}

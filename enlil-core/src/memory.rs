//! Guest physical memory management.
//!
//! Tracks memory allocations across guests, ensures no overlap,
//! and provides the foundation for EPT/NPT mapping.

use crate::error::EnlilError;
use std::collections::BTreeMap;

/// Configuration for a guest's memory region.
#[derive(Debug, Clone)]
pub struct GuestMemoryConfig {
    /// Size in megabytes.
    pub size_mb: u64,
}

/// A reserved region of host physical memory assigned to a guest.
#[derive(Debug, Clone)]
pub struct MemoryRegion {
    pub host_base: u64,
    pub size: u64,
    pub guest_id: String,
}

/// Manages host memory allocation across all guests.
pub struct MemoryManager {
    /// Total available host memory in bytes.
    total_bytes: u64,
    /// Reserved for hypervisor overhead.
    reserved_bytes: u64,
    /// Allocated regions, keyed by host base address.
    regions: BTreeMap<u64, MemoryRegion>,
    /// Next available base address.
    next_base: u64,
}

impl MemoryManager {
    pub fn new(total_mb: u64, reserved_mb: u64) -> Self {
        let reserved_bytes = reserved_mb * 1024 * 1024;
        Self {
            total_bytes: total_mb * 1024 * 1024,
            reserved_bytes,
            regions: BTreeMap::new(),
            next_base: reserved_bytes, // hypervisor gets the low region
        }
    }

    /// Allocate a contiguous memory region for a guest.
    /// Returns the host base address of the allocated region.
    pub fn allocate(&mut self, guest_id: &str, size_mb: u64) -> Result<u64, EnlilError> {
        let size = size_mb * 1024 * 1024;
        let base = self.next_base;

        if base + size > self.total_bytes {
            return Err(EnlilError::Memory(format!(
                "cannot allocate {}MB for '{}': only {}MB remaining",
                size_mb,
                guest_id,
                (self.total_bytes - base) / (1024 * 1024)
            )));
        }

        // Check for overlap with existing regions
        for (existing_base, region) in &self.regions {
            let existing_end = existing_base + region.size;
            if base < existing_end && base + size > *existing_base {
                return Err(EnlilError::Memory(format!(
                    "region for '{}' overlaps with '{}'",
                    guest_id, region.guest_id
                )));
            }
        }

        let region = MemoryRegion {
            host_base: base,
            size,
            guest_id: guest_id.to_string(),
        };
        self.regions.insert(base, region);
        self.next_base = base + size;

        Ok(base)
    }

    /// Returns total allocated memory in bytes.
    pub fn allocated_bytes(&self) -> u64 {
        self.regions.values().map(|r| r.size).sum()
    }

    /// Returns available memory in bytes (excluding reserved).
    pub fn available_bytes(&self) -> u64 {
        self.total_bytes - self.reserved_bytes - self.allocated_bytes()
    }

    /// Get all allocated regions.
    pub fn regions(&self) -> impl Iterator<Item = &MemoryRegion> {
        self.regions.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_two_guests() {
        let mut mm = MemoryManager::new(32768, 512); // 32GB total, 512MB reserved
        let base1 = mm.allocate("linux1", 8192).unwrap();
        let base2 = mm.allocate("linux2", 8192).unwrap();
        assert_ne!(base1, base2);
        assert_eq!(mm.regions().count(), 2);
    }

    #[test]
    fn reject_overcommit() {
        let mut mm = MemoryManager::new(1024, 256); // 1GB total, 256MB reserved
        mm.allocate("guest1", 512).unwrap();
        assert!(mm.allocate("guest2", 512).is_err()); // only 256MB left
    }
}

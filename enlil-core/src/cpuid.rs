//! `CPUID` filtering for guest transparency.
//!
//! The hypervisor must intercept `CPUID` instructions and return
//! crafted responses that hide its presence and report correct
//! topology for the guest's allocated cores.

/// A single `CPUID` leaf entry.
#[derive(Debug, Clone, Copy)]
pub struct CpuidEntry {
    pub function: u32,
    pub index: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// Guest CPU topology description for `CPUID` crafting.
#[derive(Debug, Clone, Copy)]
pub struct GuestTopology {
    /// Number of logical CPUs the guest sees.
    pub logical_cpus: u32,
    /// Cores per package (physical cores visible to guest).
    pub cores_per_package: u32,
    /// Threads per core (typically 1 unless exposing `HT`).
    pub threads_per_core: u32,
    /// Package (socket) count visible to guest.
    pub packages: u32,
    /// `L1` cache sharing (cores sharing `L1`, typically 1).
    pub l1_sharing: u32,
    /// `L2` cache sharing (cores sharing `L2`, typically 1-2).
    pub l2_sharing: u32,
    /// `L3` cache sharing (cores sharing `L3`, typically all).
    pub l3_sharing: u32,
}

impl GuestTopology {
    /// Create a simple topology: N cores, 1 thread each, 1 package.
    #[must_use]
    pub const fn simple(core_count: u32) -> Self {
        Self {
            logical_cpus: core_count,
            cores_per_package: core_count,
            threads_per_core: 1,
            packages: 1,
            l1_sharing: 1,
            l2_sharing: 1,
            l3_sharing: core_count,
        }
    }
}

/// `ceil(log2(n))` — the number of x2APIC-ID bits needed to enumerate `n`
/// distinct items (the smallest `b` with `2^b >= n`). Returns 0 for `n <= 1`.
///
/// Differs from `32 - n.leading_zeros()` (which is `floor(log2(n)) + 1`) on exact
/// powers of two: `ceil_log2(4) == 2`, not 3. Subtracting one before counting
/// leading zeros removes that off-by-one, which otherwise widens the topology
/// shift fields by a bit and corrupts the package boundary a guest derives.
const fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        u32::BITS - (n - 1).leading_zeros()
    }
}

/// `CPUID` filter that modifies host `CPUID` data for guest consumption.
pub struct CpuidFilter {
    /// Guest topology description.
    topology: GuestTopology,
    /// Whether to hide the hypervisor present bit.
    hide_hypervisor: bool,
    /// Custom vendor string (12 bytes). `None` = pass through host vendor.
    custom_vendor: Option<[u8; 12]>,
}

impl CpuidFilter {
    /// Create a filter for a guest with the given CPU count.
    #[must_use]
    pub const fn new(guest_cpu_count: u32) -> Self {
        Self {
            topology: GuestTopology::simple(guest_cpu_count),
            hide_hypervisor: true,
            custom_vendor: None,
        }
    }

    /// Create a filter with full topology control.
    #[must_use]
    pub const fn with_topology(topology: GuestTopology) -> Self {
        Self {
            topology,
            hide_hypervisor: true,
            custom_vendor: None,
        }
    }

    /// Set whether to hide the hypervisor present bit.
    pub const fn set_hide_hypervisor(&mut self, hide: bool) {
        self.hide_hypervisor = hide;
    }

    /// Set a custom vendor string (must be exactly 12 ASCII bytes).
    pub const fn set_vendor(&mut self, vendor: [u8; 12]) {
        self.custom_vendor = Some(vendor);
    }

    /// Get the guest topology configuration.
    #[must_use]
    pub const fn topology(&self) -> &GuestTopology {
        &self.topology
    }

    /// Filter a `CPUID` entry for guest consumption.
    ///
    /// # Returns
    ///
    /// Returns `Some(entry)` with modified values, or `None` if the leaf should be hidden entirely.
    ///
    /// # Must Use
    ///
    /// The returned filtered entry must be used; ignoring it defeats the purpose of filtering.
    #[must_use]
    pub fn filter(&self, entry: &CpuidEntry) -> Option<CpuidEntry> {
        let mut out = *entry;

        match entry.function {
            // Leaf 0x0: Vendor ID and max standard leaf
            0x0 => {
                if let Some(ref vendor) = self.custom_vendor {
                    out.ebx = u32::from_le_bytes([vendor[0], vendor[1], vendor[2], vendor[3]]);
                    out.edx = u32::from_le_bytes([vendor[4], vendor[5], vendor[6], vendor[7]]);
                    out.ecx = u32::from_le_bytes([vendor[8], vendor[9], vendor[10], vendor[11]]);
                }
            }
            // Leaf 0x1: Feature information
            0x1 => {
                if self.hide_hypervisor {
                    // Clear hypervisor present bit (`ECX` bit 31)
                    out.ecx &= !(1 << 31);
                }
                // Report correct logical processor count in `EBX`[23:16]
                out.ebx = (out.ebx & 0xFF00_FFFF) | ((self.topology.logical_cpus & 0xFF) << 16);
                // Set initial `APIC` ID in `EBX`[31:24] (will be per-vCPU)
                // Leave as-is for now — set per-vCPU at runtime
            }
            // Leaf 0x4: Deterministic cache parameters
            0x4 => {
                let cache_type = out.eax & 0x1F;
                if cache_type == 0 {
                    return Some(out); // null entry, pass through
                }

                let sharing = match entry.index {
                    0 | 1 => self.topology.l1_sharing, // L1 data and instruction
                    2 => self.topology.l2_sharing,     // L2 unified
                    3 => self.topology.l3_sharing,     // L3 unified
                    _ => 1,
                };

                // `EAX`[25:14] = max threads sharing this cache - 1
                let max_sharing = sharing.saturating_sub(1) & 0xFFF;
                out.eax = (out.eax & 0xFC00_3FFF) | (max_sharing << 14);

                // `EAX`[31:26] = max cores per package - 1
                let max_cores = self.topology.cores_per_package.saturating_sub(1) & 0x3F;
                out.eax = (out.eax & 0x03FF_FFFF) | (max_cores << 26);
            }
            // Leaf 0xB: Extended topology enumeration (x2`APIC`)
            0xB => {
                match entry.index {
                    0 => {
                        // `SMT` level: threads per core. EAX = bits the SMT field
                        // occupies = ceil(log2(threads_per_core)).
                        let shift = ceil_log2(self.topology.threads_per_core);
                        out.eax = (out.eax & 0xFFFF_FFE0) | (shift & 0x1F);
                        out.ebx =
                            (out.ebx & 0xFFFF_0000) | (self.topology.threads_per_core & 0xFFFF);
                        // `ECX`[15:8] = level type (1 = `SMT`)
                        out.ecx = (out.ecx & 0xFFFF_00FF) | (1 << 8);
                    }
                    1 => {
                        // Core level: cumulative shift past SMT + core fields =
                        // ceil(log2(logical_cpus)); right-shifting an x2APIC ID by it
                        // yields the package ID. `32 - leading_zeros` overshoots
                        // power-of-two CPU counts by a bit, so use `ceil_log2`.
                        let shift = ceil_log2(self.topology.logical_cpus);
                        out.eax = (out.eax & 0xFFFF_FFE0) | (shift & 0x1F);
                        out.ebx = (out.ebx & 0xFFFF_0000) | (self.topology.logical_cpus & 0xFFFF);
                        // `ECX`[15:8] = level type (2 = Core)
                        out.ecx = (out.ecx & 0xFFFF_00FF) | (2 << 8);
                    }
                    _ => {
                        // Invalid level
                        out.eax = 0;
                        out.ebx = 0;
                        out.ecx = entry.index;
                    }
                }
            }
            // Leaf 0x4000_0000-0x4000_00FF: Hypervisor leaves
            0x4000_0000..=0x4000_00FF if self.hide_hypervisor => {
                return None; // hide all hypervisor-specific leaves
            }
            _ => {}
        }

        Some(out)
    }

    /// Generate a complete set of topology-related `CPUID` entries for a vCPU.
    ///
    /// # Arguments
    ///
    /// * `apic_id` - The initial `APIC` ID for this specific vCPU.
    ///
    /// # Returns
    ///
    /// A vector of `CPUID` entries with per-vCPU topology information.
    #[must_use]
    pub fn generate_topology_entries(&self, apic_id: u32) -> Vec<CpuidEntry> {
        let mut entries = Vec::new();

        // Leaf 0x1 with per-vCPU `APIC` ID
        entries.push(CpuidEntry {
            function: 0x1,
            index: 0,
            eax: 0, // will be filled from host
            ebx: ((apic_id & 0xFF) << 24)
                | ((self.topology.logical_cpus & 0xFF) << 16)
                | (0x08 << 8), // `CLFLUSH` line size = 8 * 8 = 64 bytes
            ecx: 0,
            edx: 0,
        });

        // Leaf 0xB sub-leaves
        // `SMT` level
        let smt_shift = ceil_log2(self.topology.threads_per_core);
        entries.push(CpuidEntry {
            function: 0xB,
            index: 0,
            eax: smt_shift,
            ebx: self.topology.threads_per_core,
            ecx: (1 << 8), // level type = `SMT`, level number = 0
            edx: apic_id,
        });

        // Core level: cumulative shift = ceil(log2(logical_cpus)), so the package
        // ID falls out of an x2APIC ID right-shifted by it. Using `ceil_log2`
        // avoids the power-of-two off-by-one of `32 - leading_zeros`.
        let core_shift = ceil_log2(self.topology.logical_cpus);
        entries.push(CpuidEntry {
            function: 0xB,
            index: 1,
            eax: core_shift,
            ebx: self.topology.logical_cpus,
            ecx: (2 << 8) | 1, // level type = Core, level number = 1
            edx: apic_id,
        });

        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_hypervisor_bit() {
        let filter = CpuidFilter::new(4);
        let entry = CpuidEntry {
            function: 0x1,
            index: 0,
            eax: 0,
            ebx: 0,
            ecx: 1 << 31, // hypervisor bit set
            edx: 0,
        };
        let filtered = filter.filter(&entry).unwrap();
        assert_eq!(filtered.ecx & (1 << 31), 0);
    }

    #[test]
    fn hides_hypervisor_leaves() {
        let filter = CpuidFilter::new(4);
        let entry = CpuidEntry {
            function: 0x4000_0000,
            index: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        };
        assert!(filter.filter(&entry).is_none());
    }

    #[test]
    fn reports_correct_cpu_count() {
        let filter = CpuidFilter::new(4);
        let entry = CpuidEntry {
            function: 0x1,
            index: 0,
            eax: 0,
            ebx: 0x00FF_0000, // host says 255 logical CPUs
            ecx: 0,
            edx: 0,
        };
        let filtered = filter.filter(&entry).unwrap();
        assert_eq!((filtered.ebx >> 16) & 0xFF, 4);
    }

    #[test]
    fn topology_simple_creation() {
        let topo = GuestTopology::simple(8);
        assert_eq!(topo.logical_cpus, 8);
        assert_eq!(topo.cores_per_package, 8);
        assert_eq!(topo.threads_per_core, 1);
        assert_eq!(topo.packages, 1);
        assert_eq!(topo.l3_sharing, 8);
    }

    #[test]
    fn filter_with_topology() {
        let topo = GuestTopology {
            logical_cpus: 4,
            cores_per_package: 4,
            threads_per_core: 1,
            packages: 1,
            l1_sharing: 1,
            l2_sharing: 2,
            l3_sharing: 4,
        };
        let filter = CpuidFilter::with_topology(topo);

        // Check leaf 0x4 cache topology for L3 (index 3)
        let entry = CpuidEntry {
            function: 0x4,
            index: 3,
            eax: 0x0000_0063, // some cache type bits
            ebx: 0,
            ecx: 0,
            edx: 0,
        };
        let filtered = filter.filter(&entry).unwrap();
        // `EAX`[31:26] = max cores per package - 1 = 3
        assert_eq!((filtered.eax >> 26) & 0x3F, 3);
        // `EAX`[25:14] = max threads sharing `L3` - 1 = 3
        assert_eq!((filtered.eax >> 14) & 0xFFF, 3);
    }

    #[test]
    fn leaf_0xb_topology() {
        let filter = CpuidFilter::new(4);

        // `SMT` level (index 0)
        let entry = CpuidEntry {
            function: 0xB,
            index: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        };
        let filtered = filter.filter(&entry).unwrap();
        // threads_per_core = 1, so shift = 0
        assert_eq!(filtered.eax & 0x1F, 0);
        assert_eq!(filtered.ebx & 0xFFFF, 1); // 1 thread per core
        assert_eq!((filtered.ecx >> 8) & 0xFF, 1); // level type = `SMT`

        // Core level (index 1)
        let entry = CpuidEntry {
            function: 0xB,
            index: 1,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        };
        let filtered = filter.filter(&entry).unwrap();
        assert_eq!(filtered.ebx & 0xFFFF, 4); // 4 logical CPUs
        assert_eq!((filtered.ecx >> 8) & 0xFF, 2); // level type = Core
                                                   // 4 logical CPUs fit in exactly 2 x2APIC-ID bits, so the package shift is
                                                   // 2 — not 3, which the old `32 - leading_zeros` produced for powers of two.
        assert_eq!(filtered.eax & 0x1F, 2);
    }

    #[test]
    fn ceil_log2_does_not_overshoot_powers_of_two() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2); // the off-by-one case
        assert_eq!(ceil_log2(8), 3);
    }

    #[test]
    fn generate_topology_entries_per_vcpu() {
        let filter = CpuidFilter::new(4);
        let entries = filter.generate_topology_entries(2);

        assert_eq!(entries.len(), 3); // leaf 1 + leaf 0xB sub0 + leaf 0xB sub1

        // Check `APIC` ID in leaf 0x1
        let leaf1 = &entries[0];
        assert_eq!((leaf1.ebx >> 24) & 0xFF, 2);

        // Check `APIC` ID in leaf 0xB
        let leaf_b0 = &entries[1];
        assert_eq!(leaf_b0.edx, 2);
        let leaf_b1 = &entries[2];
        assert_eq!(leaf_b1.edx, 2);
    }

    #[test]
    fn custom_vendor_string() {
        let mut filter = CpuidFilter::new(1);
        filter.set_vendor(*b"EnlilHypervi"); // 12 bytes

        let entry = CpuidEntry {
            function: 0x0,
            index: 0,
            eax: 0x16,
            ebx: 0,
            ecx: 0,
            edx: 0,
        };
        let filtered = filter.filter(&entry).unwrap();
        assert_eq!(filtered.eax, 0x16); // max leaf preserved

        // Reconstruct vendor string
        let mut vendor = [0u8; 12];
        vendor[0..4].copy_from_slice(&filtered.ebx.to_le_bytes());
        vendor[4..8].copy_from_slice(&filtered.edx.to_le_bytes());
        vendor[8..12].copy_from_slice(&filtered.ecx.to_le_bytes());
        assert_eq!(&vendor, b"EnlilHypervi");
    }

    #[test]
    fn show_hypervisor_when_not_hidden() {
        let mut filter = CpuidFilter::new(1);
        filter.set_hide_hypervisor(false);

        let entry = CpuidEntry {
            function: 0x1,
            index: 0,
            eax: 0,
            ebx: 0,
            ecx: 1 << 31,
            edx: 0,
        };
        let filtered = filter.filter(&entry).unwrap();
        assert_ne!(filtered.ecx & (1 << 31), 0); // bit preserved

        // Hypervisor leaves should also be visible
        let hv_entry = CpuidEntry {
            function: 0x4000_0000,
            index: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        };
        assert!(filter.filter(&hv_entry).is_some());
    }
}

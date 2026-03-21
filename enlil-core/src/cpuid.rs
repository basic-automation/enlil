//! CPUID filtering for guest transparency.
//!
//! The hypervisor must intercept CPUID instructions and return
//! crafted responses that hide its presence and report correct
//! topology for the guest's allocated cores.

/// A single CPUID leaf entry.
#[derive(Debug, Clone)]
pub struct CpuidEntry {
    pub function: u32,
    pub index: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// CPUID filter that modifies host CPUID data for guest consumption.
pub struct CpuidFilter {
    /// Number of logical CPUs the guest sees.
    guest_cpu_count: u32,
    /// Whether to hide the hypervisor present bit.
    hide_hypervisor: bool,
}

impl CpuidFilter {
    pub fn new(guest_cpu_count: u32) -> Self {
        Self {
            guest_cpu_count,
            hide_hypervisor: true,
        }
    }

    /// Filter a CPUID entry for guest consumption.
    /// Returns None if the leaf should be hidden entirely.
    pub fn filter(&self, entry: &CpuidEntry) -> Option<CpuidEntry> {
        let mut out = entry.clone();

        match entry.function {
            // Leaf 0x1: Feature information
            0x1 => {
                if self.hide_hypervisor {
                    // Clear hypervisor present bit (ECX bit 31)
                    out.ecx &= !(1 << 31);
                }
                // Report correct logical processor count in EBX[23:16]
                out.ebx = (out.ebx & 0xFF00FFFF) | ((self.guest_cpu_count & 0xFF) << 16);
            }
            // Leaf 0x4: Deterministic cache parameters
            0x4 => {
                // Adjust max cores sharing cache in EAX[31:26]
                let max_sharing = (self.guest_cpu_count.saturating_sub(1)) & 0x3F;
                out.eax = (out.eax & 0x03FFFFFF) | (max_sharing << 26);
            }
            // Leaf 0xB: Extended topology enumeration
            0xB => {
                // Adjust processor count at each level
                if entry.index == 1 {
                    // Core level: report guest CPU count
                    out.ebx = (out.ebx & 0xFFFF0000) | (self.guest_cpu_count & 0xFFFF);
                }
            }
            // Leaf 0x40000000-0x400000FF: Hypervisor leaves
            0x40000000..=0x400000FF => {
                if self.hide_hypervisor {
                    return None; // hide all hypervisor-specific leaves
                }
            }
            _ => {}
        }

        Some(out)
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
            function: 0x40000000,
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
            ebx: 0x00FF0000, // host says 255 logical CPUs
            ecx: 0,
            edx: 0,
        };
        let filtered = filter.filter(&entry).unwrap();
        assert_eq!((filtered.ebx >> 16) & 0xFF, 4);
    }
}

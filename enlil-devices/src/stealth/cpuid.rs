//! CPUID stealth interception
//!
//! Intercepts all CPUID exits and crafts responses that hide hypervisor
//! presence. Critical for anti-detection:
//! - Leaf 0x1 ECX bit 31: clear hypervisor present bit
//! - Leaf 0x40000000-0x400000FF: return zeros
//! - Leaf 0x0: correct vendor string
//! - Leaf 0x80000002-4: pass through real CPU brand string
//! - All undefined/reserved leaves: return 0

/// CPUID register set for a single leaf result
#[derive(Debug, Clone, Copy, Default)]
pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// Pre-computed CPUID lookup table for fast VMEXIT handling.
/// All results are cached at guest initialization to minimize exit latency.
/// Goal: CPUID exit round-trip < 500 cycles.
pub struct CpuidStealthTable {
    /// Cached results indexed by (leaf, subleaf)
    entries: Vec<CpuidCacheEntry>,
    /// Maximum standard leaf
    max_standard_leaf: u32,
    /// Maximum extended leaf
    max_extended_leaf: u32,
}

#[derive(Debug, Clone)]
struct CpuidCacheEntry {
    leaf: u32,
    subleaf: u32,
    result: CpuidResult,
}

/// Configuration for CPUID stealth
#[derive(Debug, Clone)]
pub struct CpuidStealthConfig {
    /// Physical CPU vendor string (from leaf 0x0)
    pub vendor: CpuVendor,
    /// Physical CPU family/model/stepping (from leaf 0x1 EAX)
    pub family_model_stepping: u32,
    /// Physical CPU feature flags (leaf 0x1 ECX, EDX) — selectively passed through
    pub features_ecx: u32,
    pub features_edx: u32,
    /// Physical CPU brand string (leaves 0x80000002-4, 48 bytes)
    pub brand_string: [u8; 48],
    /// Virtual topology
    pub vcpu_count: u32,
    pub threads_per_core: u32,
    /// Whether to hide the hypervisor completely
    pub hide_hypervisor: bool,
    /// Physical cache info to pass through
    pub cache_info: Vec<CpuidResult>,
}

/// CPU vendor
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuVendor {
    Intel,
    Amd,
}

impl CpuVendor {
    /// Vendor string registers for leaf 0x0 (EBX, EDX, ECX)
    #[must_use]
    pub const fn vendor_regs(self) -> (u32, u32, u32) {
        match self {
            Self::Intel => (
                u32::from_le_bytes(*b"Genu"),
                u32::from_le_bytes(*b"ineI"),
                u32::from_le_bytes(*b"ntel"),
            ),
            Self::Amd => (
                u32::from_le_bytes(*b"Auth"),
                u32::from_le_bytes(*b"enti"),
                u32::from_le_bytes(*b"cAMD"),
            ),
        }
    }
}

impl CpuidStealthTable {
    /// Build the stealth CPUID table from physical CPU info
    #[must_use]
    pub fn build(config: &CpuidStealthConfig) -> Self {
        let mut entries = Vec::with_capacity(128);
        let max_standard_leaf = 0x16; // Processor Frequency
        let max_extended_leaf = 0x8000_0008; // Virtual/Physical address sizes

        // Leaf 0x0: Vendor ID
        let (ebx, edx, ecx) = config.vendor.vendor_regs();
        entries.push(CpuidCacheEntry {
            leaf: 0,
            subleaf: 0,
            result: CpuidResult {
                eax: max_standard_leaf,
                ebx,
                ecx,
                edx,
            },
        });

        // Leaf 0x1: Feature Information
        entries.push(CpuidCacheEntry {
            leaf: 1,
            subleaf: 0,
            result: Self::build_leaf_1(config),
        });

        // Leaf 0x2: Cache/TLB (Intel) or reserved (AMD)
        entries.push(CpuidCacheEntry {
            leaf: 2,
            subleaf: 0,
            result: CpuidResult::default(),
        });

        // Leaf 0x4: Deterministic Cache Parameters (Intel)
        // Leaf 0x5: MONITOR/MWAIT
        // Leaf 0x6: Thermal and Power Management
        // Leaf 0x7: Structured Extended Feature Flags
        entries.push(CpuidCacheEntry {
            leaf: 7,
            subleaf: 0,
            result: Self::build_leaf_7(config),
        });

        // Leaf 0xB: Extended Topology
        Self::build_topology_leaves(config, &mut entries);

        // Leaf 0xD: XSAVE features
        entries.push(CpuidCacheEntry {
            leaf: 0xD,
            subleaf: 0,
            result: CpuidResult {
                eax: 0x7, // x87 + SSE + AVX
                ebx: 0x340,
                ecx: 0x340,
                edx: 0,
            },
        });

        // Leaves 0x40000000-0x400000FF: MUST return zeros when hiding
        if config.hide_hypervisor {
            for leaf in 0x4000_0000..=0x4000_00FF {
                entries.push(CpuidCacheEntry {
                    leaf,
                    subleaf: 0,
                    result: CpuidResult::default(),
                });
            }
        }

        // Extended leaves
        // 0x80000000: Max extended leaf
        entries.push(CpuidCacheEntry {
            leaf: 0x8000_0000,
            subleaf: 0,
            result: CpuidResult {
                eax: max_extended_leaf,
                ..CpuidResult::default()
            },
        });

        // 0x80000001: Extended feature flags
        entries.push(CpuidCacheEntry {
            leaf: 0x8000_0001,
            subleaf: 0,
            result: CpuidResult {
                eax: config.family_model_stepping,
                ecx: 0x0000_0121, // LAHF, CmpLegacy, ABM
                edx: 0x2C10_0800, // NX, Page1GB, RDTSCP, LM
                ..CpuidResult::default()
            },
        });

        // 0x80000002-4: Processor Brand String (pass through real CPU)
        for i in 0..3u32 {
            let offset = (i * 16) as usize;
            entries.push(CpuidCacheEntry {
                leaf: 0x8000_0002 + i,
                subleaf: 0,
                result: CpuidResult {
                    eax: u32::from_le_bytes(config.brand_string[offset..offset + 4].try_into().unwrap_or([0; 4])),
                    ebx: u32::from_le_bytes(config.brand_string[offset + 4..offset + 8].try_into().unwrap_or([0; 4])),
                    ecx: u32::from_le_bytes(config.brand_string[offset + 8..offset + 12].try_into().unwrap_or([0; 4])),
                    edx: u32::from_le_bytes(config.brand_string[offset + 12..offset + 16].try_into().unwrap_or([0; 4])),
                },
            });
        }

        // 0x80000008: Virtual/Physical address sizes
        entries.push(CpuidCacheEntry {
            leaf: 0x8000_0008,
            subleaf: 0,
            result: CpuidResult {
                eax: 0x0000_3930, // 48-bit virtual, 57-bit physical (common)
                ..CpuidResult::default()
            },
        });

        Self {
            entries,
            max_standard_leaf,
            max_extended_leaf,
        }
    }

    /// Look up a CPUID leaf/subleaf. Returns zeros for unknown leaves.
    #[must_use]
    pub fn lookup(&self, leaf: u32, subleaf: u32) -> CpuidResult {
        // Fast path: hypervisor leaves always return zero
        if (0x4000_0000..=0x4000_00FF).contains(&leaf) {
            return CpuidResult::default();
        }

        // Bounds check
        if leaf <= self.max_standard_leaf || (0x8000_0000..=self.max_extended_leaf).contains(&leaf) {
            for entry in &self.entries {
                if entry.leaf == leaf && entry.subleaf == subleaf {
                    return entry.result;
                }
            }
            // Known range but no specific entry — return zeros
            // This is safer than returning garbage
        }

        CpuidResult::default()
    }

    fn build_leaf_1(config: &CpuidStealthConfig) -> CpuidResult {
        let mut ecx = config.features_ecx;
        if config.hide_hypervisor {
            // Clear bit 31: hypervisor present
            ecx &= !(1 << 31);
        }

        CpuidResult {
            eax: config.family_model_stepping,
            ebx: 0x0010_0800, // CLFLUSH size=8, max CPUs=1
            ecx,
            edx: config.features_edx,
        }
    }

    fn build_leaf_7(config: &CpuidStealthConfig) -> CpuidResult {
        // Pass through common structured features, masking dangerous ones
        CpuidResult {
            eax: 0, // max subleaf
            ebx: 0x0000_0281, // FSGSBASE, BMI1, AVX2 (conservative)
            ecx: 0,
            edx: 0,
        }
    }

    #[allow(clippy::cast_possible_truncation)]
    fn build_topology_leaves(config: &CpuidStealthConfig, entries: &mut Vec<CpuidCacheEntry>) {
        // Subleaf 0: SMT level
        let threads_per_core = config.threads_per_core;
        let smt_shift = if threads_per_core > 1 { 1 } else { 0 };
        entries.push(CpuidCacheEntry {
            leaf: 0xB,
            subleaf: 0,
            result: CpuidResult {
                eax: smt_shift,
                ebx: threads_per_core,
                ecx: (1 << 8) | 0, // SMT level type = 1, level number = 0
                edx: 0, // x2APIC ID (set per-vCPU at runtime)
            },
        });

        // Subleaf 1: Core level
        let cores = config.vcpu_count / threads_per_core;
        let core_shift = 32 - cores.leading_zeros(); // ceil(log2(cores))
        entries.push(CpuidCacheEntry {
            leaf: 0xB,
            subleaf: 1,
            result: CpuidResult {
                eax: core_shift + smt_shift,
                ebx: config.vcpu_count,
                ecx: (2 << 8) | 1, // Core level type = 2, level number = 1
                edx: 0,
            },
        });

        // Subleaf 2: Invalid (terminates enumeration)
        entries.push(CpuidCacheEntry {
            leaf: 0xB,
            subleaf: 2,
            result: CpuidResult {
                eax: 0,
                ebx: 0,
                ecx: 2, // level number = 2, level type = 0 (invalid)
                edx: 0,
            },
        });
    }
}

/// Create a brand string from a Rust &str, padded to 48 bytes
#[must_use]
pub fn brand_string_from_str(s: &str) -> [u8; 48] {
    let mut brand = [0u8; 48];
    let bytes = s.as_bytes();
    let len = bytes.len().min(48);
    brand[..len].copy_from_slice(&bytes[..len]);
    brand
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CpuidStealthConfig {
        CpuidStealthConfig {
            vendor: CpuVendor::Amd,
            family_model_stepping: 0x00A6_0F12,
            features_ecx: 0x7ED8_320B,
            features_edx: 0x178B_FBFF,
            brand_string: brand_string_from_str("AMD Ryzen 9 7950X 16-Core Processor"),
            vcpu_count: 8,
            threads_per_core: 2,
            hide_hypervisor: true,
            cache_info: Vec::new(),
        }
    }

    #[test]
    fn hypervisor_bit_cleared() {
        let table = CpuidStealthTable::build(&test_config());
        let result = table.lookup(1, 0);
        assert_eq!(result.ecx & (1 << 31), 0, "Hypervisor bit must be cleared");
    }

    #[test]
    fn hypervisor_leaves_return_zero() {
        let table = CpuidStealthTable::build(&test_config());
        for leaf in 0x4000_0000..=0x4000_0010 {
            let result = table.lookup(leaf, 0);
            assert_eq!(result.eax, 0);
            assert_eq!(result.ebx, 0);
            assert_eq!(result.ecx, 0);
            assert_eq!(result.edx, 0);
        }
    }

    #[test]
    fn vendor_string_correct() {
        let table = CpuidStealthTable::build(&test_config());
        let result = table.lookup(0, 0);
        let ebx_bytes = result.ebx.to_le_bytes();
        let edx_bytes = result.edx.to_le_bytes();
        let ecx_bytes = result.ecx.to_le_bytes();
        let vendor: Vec<u8> = ebx_bytes.iter().chain(edx_bytes.iter()).chain(ecx_bytes.iter()).copied().collect();
        assert_eq!(&vendor, b"AuthenticAMD");
    }

    #[test]
    fn brand_string_passed_through() {
        let table = CpuidStealthTable::build(&test_config());
        let mut brand = Vec::with_capacity(48);
        for leaf in 0x8000_0002..=0x8000_0004 {
            let r = table.lookup(leaf, 0);
            brand.extend_from_slice(&r.eax.to_le_bytes());
            brand.extend_from_slice(&r.ebx.to_le_bytes());
            brand.extend_from_slice(&r.ecx.to_le_bytes());
            brand.extend_from_slice(&r.edx.to_le_bytes());
        }
        let brand_str = std::str::from_utf8(&brand).unwrap().trim_end_matches('\0');
        assert!(brand_str.starts_with("AMD Ryzen 9 7950X"));
    }

    #[test]
    fn unknown_leaves_return_zero() {
        let table = CpuidStealthTable::build(&test_config());
        let result = table.lookup(0xDEAD, 0);
        assert_eq!(result.eax, 0);
    }

    #[test]
    fn topology_leaf_0xb() {
        let table = CpuidStealthTable::build(&test_config());
        // Subleaf 0: SMT
        let smt = table.lookup(0xB, 0);
        assert_eq!(smt.ebx, 2); // 2 threads per core

        // Subleaf 1: Core
        let core = table.lookup(0xB, 1);
        assert_eq!(core.ebx, 8); // 8 total logical processors
    }

    #[test]
    fn intel_vendor() {
        let config = CpuidStealthConfig {
            vendor: CpuVendor::Intel,
            ..test_config()
        };
        let table = CpuidStealthTable::build(&config);
        let result = table.lookup(0, 0);
        let ebx_bytes = result.ebx.to_le_bytes();
        assert_eq!(&ebx_bytes, b"Genu");
    }
}

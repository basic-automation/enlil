//! CPUID stealth interception
//!
//! Intercepts all CPUID exits and crafts responses that hide hypervisor
//! presence. Critical for anti-detection:
//! - Leaf 0x1 ECX bit 31: clear hypervisor present bit
//! - Leaf 0x40000000-0x400000FF: no hypervisor signature — treated as out-of-range, so on
//!   Intel it mirrors the highest basic leaf and on AMD it returns zeros, exactly like bare
//!   metal. (Returning zeros unconditionally is itself a tell: no real Intel CPU does.)
//! - Leaf 0x0: correct vendor string
//! - Leaf 0x80000002-4: pass through real CPU brand string
//! - Out-of-range leaves: highest basic leaf data (Intel) / zeros (AMD), per the SDM/APM;
//!   in-range but reserved leaves return 0.

use crate::truncate::u32_of;

/// CPUID register set for a single leaf result
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    /// Result returned for any *out-of-range* leaf (above the basic max — including the
    /// `0x4000_0000` hypervisor region — or above the extended max). Real Intel CPUs return
    /// the highest basic leaf's data here; AMD returns zeros. Returning zeros on Intel (as the
    /// table used to) is itself a VM tell, so this is precomputed per-vendor at build time.
    out_of_range: CpuidResult,
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
    /// Leaf 7 subleaf 0 EBX/ECX structured-extended-feature candidates.
    /// [`CpuidStealthTable::build`] masks these to the **virtualizable,
    /// leaf-0xD-consistent allowlist** ([`LEAF7_EBX_PASSTHROUGH`] /
    /// [`LEAF7_ECX_PASSTHROUGH`]) regardless of what's set here, so a config
    /// captured from a host with AVX-512/TSX/SGX cannot advertise state the
    /// virtual platform doesn't carry.
    pub features_7_ebx: u32,
    pub features_7_ecx: u32,
    /// Processor base frequency in MHz (leaf 0x16 EAX; also fixes the TSC
    /// rate enumerated by leaf 0x15 — modern Intel clocks the TSC at the
    /// base frequency)
    pub base_frequency_mhz: u32,
    /// Maximum turbo frequency in MHz (leaf 0x16 EBX)
    pub max_frequency_mhz: u32,
    /// The host's AMD `PerfMonV2` capability leaf (`0x8000_0022`), captured
    /// verbatim when the host actually advertises it (max extended leaf
    /// ≥ `0x8000_0022` and a non-zero `EAX`); `None` otherwise. Only AMD parts
    /// from Zen 4 on expose it. When present, the synthesized table advertises
    /// `PerfMonV2` (EAX bit 0 + the host's core-PMC count in `EBX[3:0]`) so an
    /// AMD-presented guest enumerates a PMU consistent with the AMD `PerfCtr`
    /// MSRs the stealth router shadows; when absent it is **not** advertised
    /// (claiming a counter surface the apparent host lacks is itself a tell).
    pub perfmon_v2: Option<CpuidResult>,
}

/// Leaf 7 EBX bits safe to pass through from a host capture.
///
/// Pure instruction-set extensions (and the SMEP/SMAP CR4 bits every
/// hypervisor backend supports) whose state lives in the x87/SSE/AVX XSAVE
/// area leaf 0xD already enumerates. Deliberately **excluded**: AVX-512
/// (bits 16/17/21/26/27/28/30/31 — leaf 0xD advertises no AVX-512 state
/// components, and the pair must agree), TSX HLE/RTM (4/11 — abort behaviour
/// we don't model), SGX (2), MPX (14), PQM/PQE (12/15 — need leaves
/// 0xF/0x10), and Processor Trace (25 — needs its MSR surface).
pub const LEAF7_EBX_PASSTHROUGH: u32 = (1 << 0)  // FSGSBASE
    | (1 << 3)  // BMI1
    | (1 << 5)  // AVX2
    | (1 << 7)  // SMEP
    | (1 << 8)  // BMI2
    | (1 << 9)  // ERMS
    | (1 << 18) // RDSEED
    | (1 << 19) // ADX
    | (1 << 20) // SMAP
    | (1 << 23) // CLFLUSHOPT
    | (1 << 24) // CLWB
    | (1 << 29); // SHA

/// Leaf 7 ECX bits safe to pass through.
///
/// Instruction-set extensions over existing XSAVE state plus UMIP (a CR4
/// bit) and RDPID (`TSC_AUX` already exists — RDTSCP is advertised).
/// Deliberately **excluded**: the AVX-512 extensions (1/6/11/12/14),
/// PKU/OSPKE (3/4 — XSAVE PKRU component not enumerated in leaf 0xD),
/// WAITPKG (5 — UMWAIT MSR), CET (7), **LA57 (16 — leaf 0x80000008
/// advertises 48-bit linear; the pair is test-pinned)**, and KL (23).
pub const LEAF7_ECX_PASSTHROUGH: u32 = (1 << 2)  // UMIP
    | (1 << 8)  // GFNI
    | (1 << 9)  // VAES
    | (1 << 10) // VPCLMULQDQ
    | (1 << 22); // RDPID

#[cfg(target_arch = "x86_64")]
impl CpuidStealthConfig {
    /// Build a stealth config **from the host CPU's own CPUID**, so the
    /// synthesized table presents the physical machine's identity (vendor,
    /// family/model/stepping, features, brand string, cache geometry,
    /// frequencies) with only the virtualization-relevant facts rewritten.
    ///
    /// This replaces the old filter-host-entries approach (the deleted
    /// `enlil-core::cpuid::CpuidFilter`): instead of patching host CPUID
    /// entries leaf by leaf at lookup time, the host facts are captured once
    /// into the config and [`CpuidStealthTable::build`] synthesizes every
    /// leaf with the cross-surface consistency rules applied. The one piece
    /// of per-leaf rewriting the filter did that survives is the **cache
    /// sharing topology**: host leaf-4 geometry is captured verbatim, but
    /// each subleaf's "threads sharing this cache" / "cores per package"
    /// fields are rewritten to the *guest* topology (L1/L2 shared per core,
    /// L3 package-wide), since the host's sharing IDs describe a machine the
    /// guest doesn't have.
    ///
    /// `hide_hypervisor` defaults on — when this host is itself a VM (e.g. a
    /// CI runner) the captured leaf-1 ECX has the hypervisor bit set, and the
    /// build clears it.
    #[must_use]
    pub fn from_host(vcpu_count: u32, threads_per_core: u32) -> Self {
        use core::arch::x86_64::{__cpuid, __cpuid_count};

        let leaf0 = __cpuid(0);
        let vendor = match (leaf0.ebx, leaf0.edx, leaf0.ecx) {
            v if v == CpuVendor::Amd.vendor_regs() => CpuVendor::Amd,
            // Anything else (including unknown vendors) takes the Intel
            // profile — the common case, and a coherent identity either way.
            _ => CpuVendor::Intel,
        };
        let leaf1 = __cpuid(1);
        let leaf7_candidates = if leaf0.eax >= 7 {
            let r = __cpuid_count(7, 0);
            (r.ebx, r.ecx)
        } else {
            (0, 0)
        };

        // Brand string from 0x80000002-4 (present on every x86_64 part).
        let mut brand_string = [0u8; 48];
        if __cpuid(0x8000_0000).eax >= 0x8000_0004 {
            for i in 0..3u32 {
                let r = __cpuid(0x8000_0002 + i);
                let off = (i as usize) * 16;
                brand_string[off..off + 4].copy_from_slice(&r.eax.to_le_bytes());
                brand_string[off + 4..off + 8].copy_from_slice(&r.ebx.to_le_bytes());
                brand_string[off + 8..off + 12].copy_from_slice(&r.ecx.to_le_bytes());
                brand_string[off + 12..off + 16].copy_from_slice(&r.edx.to_le_bytes());
            }
        }

        // Host leaf-4 cache geometry (Intel only), with the sharing fields
        // rewritten to the guest topology.
        let mut cache_info = Vec::new();
        if vendor == CpuVendor::Intel && leaf0.eax >= 4 {
            for subleaf in 0..16u32 {
                let r = __cpuid_count(4, subleaf);
                if r.eax.trailing_zeros() >= 5 {
                    break; // null cache type terminates the enumeration
                }
                let level = (r.eax >> 5) & 0x7;
                let shared_by = if level >= 3 {
                    vcpu_count
                } else {
                    threads_per_core
                };
                let cores = (vcpu_count / threads_per_core).max(1);
                cache_info.push(CpuidResult {
                    eax: (r.eax & 0x0000_3FFF) | ((shared_by - 1) << 14) | ((cores - 1) << 26),
                    ebx: r.ebx,
                    ecx: r.ecx,
                    edx: r.edx,
                });
            }
        }

        // AMD PerfMonV2 capability leaf (0x8000_0022), captured verbatim only
        // when the host really advertises it — so we never claim a PMU surface
        // the apparent host lacks (this nested host, e.g., reports max extended
        // leaf 0x8000_0021 and a zero 0x8000_0022).
        let max_ext = __cpuid(0x8000_0000).eax;
        let perfmon_v2 = if vendor == CpuVendor::Amd && max_ext >= 0x8000_0022 {
            let r = __cpuid(0x8000_0022);
            if r.eax != 0 {
                Some(CpuidResult {
                    eax: r.eax,
                    ebx: r.ebx,
                    ecx: r.ecx,
                    edx: r.edx,
                })
            } else {
                None
            }
        } else {
            None
        };

        // Host frequencies from Intel leaf 0x16, when enumerated; otherwise
        // the plausible defaults the synthetic profile uses.
        let (base_frequency_mhz, max_frequency_mhz) = if leaf0.eax >= 0x16 {
            let f = __cpuid(0x16);
            if f.eax & 0xFFFF != 0 && f.ebx & 0xFFFF != 0 {
                (f.eax & 0xFFFF, f.ebx & 0xFFFF)
            } else {
                (2800, 3300)
            }
        } else {
            (2800, 3300)
        };

        Self {
            vendor,
            family_model_stepping: leaf1.eax,
            features_ecx: leaf1.ecx,
            features_edx: leaf1.edx,
            features_7_ebx: leaf7_candidates.0,
            features_7_ecx: leaf7_candidates.1,
            brand_string,
            vcpu_count,
            threads_per_core,
            hide_hypervisor: true,
            cache_info,
            base_frequency_mhz,
            max_frequency_mhz,
            perfmon_v2,
        }
    }
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
    /// Push the standard (0x0–0xD) CPUID leaves.
    fn push_standard_leaves(
        config: &CpuidStealthConfig,
        max_standard_leaf: u32,
        entries: &mut Vec<CpuidCacheEntry>,
    ) {
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

        // Leaf 0x2: Cache/TLB descriptors (Intel; reserved-zero on AMD).
        // The SDM requires AL = 01H always — an all-zero EAX is a tell. The
        // 0xFF descriptor says "no cache info here, use leaf 4", which is what
        // real modern Intel CPUs report for the cache side.
        if config.vendor == CpuVendor::Intel {
            entries.push(CpuidCacheEntry {
                leaf: 2,
                subleaf: 0,
                result: CpuidResult {
                    eax: 0x0000_FF01,
                    ..CpuidResult::default()
                },
            });
        }

        // Leaf 0x4: Deterministic Cache Parameters (Intel only; AMD enumerates
        // caches via leaf 0x8000001D instead).
        if config.vendor == CpuVendor::Intel {
            Self::push_cache_leaves(config, entries);
        }

        // Leaf 0x5: MONITOR/MWAIT
        // Leaf 0x6: Thermal and Power Management
        entries.push(CpuidCacheEntry {
            leaf: 6,
            subleaf: 0,
            result: Self::build_leaf_6(config),
        });

        // Leaf 0x7: Structured Extended Feature Flags
        entries.push(CpuidCacheEntry {
            leaf: 7,
            subleaf: 0,
            result: Self::build_leaf_7(config),
        });

        // Leaf 0xA: Architectural Performance Monitoring (Intel only — reserved
        // on AMD, where the in-range-reserved zero fallback is correct).
        if config.vendor == CpuVendor::Intel {
            entries.push(CpuidCacheEntry {
                leaf: 0xA,
                subleaf: 0,
                result: Self::build_leaf_a(),
            });
        }

        // Leaf 0xB: Extended Topology
        Self::build_topology_leaves(config, entries);

        // Leaf 0xD: XSAVE features. Subleaf 0 advertises x87+SSE+AVX with an
        // 832-byte (0x340) area: 512 legacy + 64 header + 256 AVX. Subleaves
        // 1 and 2 are required for that to be usable — without subleaf 2 the
        // guest cannot locate the AVX region it was just promised.
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
        // Subleaf 1: XSAVEOPT available; XSAVEC/XSAVES not advertised (XSAVES
        // would require IA32_XSS virtualization).
        entries.push(CpuidCacheEntry {
            leaf: 0xD,
            subleaf: 1,
            result: CpuidResult {
                eax: 0x1, // XSAVEOPT
                ..CpuidResult::default()
            },
        });
        // Subleaf 2: the AVX (YMM-high) state component — 256 bytes at the
        // standard offset 576 (right after legacy area + XSAVE header).
        entries.push(CpuidCacheEntry {
            leaf: 0xD,
            subleaf: 2,
            result: CpuidResult {
                eax: 256, // size
                ebx: 576, // offset
                ..CpuidResult::default()
            },
        });

        // Leaves 0x15/0x16: TSC/crystal ratio + processor frequencies (Intel
        // only). The table advertises max basic leaf 0x16, so leaving these
        // unpopulated returned in-range zeros — "frequency not enumerated" on
        // a CPU model that always enumerates it, and no TSC rate for the guest
        // OS to calibrate against (Linux falls back to noisy PIT/HPET
        // calibration, whose result then has to agree with our virtual timers).
        if config.vendor == CpuVendor::Intel {
            // Leaf 0x15: TSC = crystal × EBX/EAX (Intel SDM Vol 2A). With a
            // 24 MHz crystal and EAX = 24, EBX = base MHz makes the TSC rate
            // exactly base_frequency_mhz × 1e6 — integer-exact, no rounding.
            entries.push(CpuidCacheEntry {
                leaf: 0x15,
                subleaf: 0,
                result: CpuidResult {
                    eax: 24,
                    ebx: config.base_frequency_mhz,
                    ecx: 24_000_000, // crystal: 24 MHz
                    edx: 0,
                },
            });
            // Leaf 0x16: base / max-turbo / bus frequencies in MHz.
            entries.push(CpuidCacheEntry {
                leaf: 0x16,
                subleaf: 0,
                result: CpuidResult {
                    eax: config.base_frequency_mhz,
                    ebx: config.max_frequency_mhz,
                    ecx: 100, // bus/reference: 100 MHz
                    edx: 0,
                },
            });
        }

        // Leaves 0x40000000-0x400000FF (the hypervisor region) are deliberately NOT cached: they
        // are out-of-range of the advertised basic max, so `lookup` resolves them through the
        // vendor-correct `out_of_range` value (highest basic leaf on Intel, zeros on AMD) — exactly
        // like bare metal. Caching zeros here would both waste 256 entries and be an Intel tell.
    }

    /// Push the extended (0x80000000+) CPUID leaves.
    fn push_extended_leaves(
        config: &CpuidStealthConfig,
        max_extended_leaf: u32,
        entries: &mut Vec<CpuidCacheEntry>,
    ) {
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

        // 0x80000001: Extended feature flags. ECX bit 22 (TOPOEXT) is
        // AMD-only and gates the 0x8000001D/0x8000001E topology leaves the
        // kernel's cacheinfo/topology code prefers on Zen — advertising the
        // leaves without the bit (or vice versa) is an inconsistency.
        let ext_ecx = match config.vendor {
            CpuVendor::Intel => 0x0000_0121,           // LAHF, LZCNT, PREFETCHW
            CpuVendor::Amd => 0x0000_0121 | (1 << 22), // + TOPOEXT
        };
        entries.push(CpuidCacheEntry {
            leaf: 0x8000_0001,
            subleaf: 0,
            result: CpuidResult {
                eax: config.family_model_stepping,
                ecx: ext_ecx,
                edx: 0x2C10_0800, // SYSCALL, NX, Page1GB, RDTSCP, LM
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
                    eax: u32::from_le_bytes(
                        config.brand_string[offset..offset + 4]
                            .try_into()
                            .unwrap_or([0; 4]),
                    ),
                    ebx: u32::from_le_bytes(
                        config.brand_string[offset + 4..offset + 8]
                            .try_into()
                            .unwrap_or([0; 4]),
                    ),
                    ecx: u32::from_le_bytes(
                        config.brand_string[offset + 8..offset + 12]
                            .try_into()
                            .unwrap_or([0; 4]),
                    ),
                    edx: u32::from_le_bytes(
                        config.brand_string[offset + 12..offset + 16]
                            .try_into()
                            .unwrap_or([0; 4]),
                    ),
                },
            });
        }

        Self::push_legacy_cache_leaves(config, entries);

        // 0x80000007 EDX[8]: invariant TSC (same bit on Intel and AMD). Every
        // CPU of the advertised generation sets it; leaving it clear tells the
        // guest the TSC stops in deep C-states / varies with P-states, so
        // Linux marks the TSC unstable and falls back to HPET — more traffic
        // through our slower timer paths AND a tell. Our virtual TSC is
        // offset-based and never stops, so claiming invariance is truthful.
        // AMD signals boost via EDX[9] (CPB — the counterpart of Intel's
        // leaf-6 IDA bit; the PMC rate model's core > ref ratio implies it)
        // and the read-only APERF/MPERF pair via EDX[10] (EffFreq), which our
        // timing-stealth MSR shadows actually serve.
        let pm_edx = match config.vendor {
            CpuVendor::Intel => 1 << 8,
            CpuVendor::Amd => (1 << 8) | (1 << 9) | (1 << 10),
        };
        entries.push(CpuidCacheEntry {
            leaf: 0x8000_0007,
            subleaf: 0,
            result: CpuidResult {
                edx: pm_edx,
                ..CpuidResult::default()
            },
        });

        // 0x80000008 EAX: address sizes. EAX[7:0] = physical address bits, EAX[15:8] = linear
        // (virtual) address bits (Intel SDM / AMD APM). The old value 0x3930 decoded as 57-bit
        // linear (LA57) + 48-bit physical — both the comment (fields swapped) and the LA57 claim
        // were wrong: leaf 7 ECX does not advertise LA57, so 57-bit linear is an inconsistency a
        // guest can catch. Use the common, internally-consistent 48/48 (no LA57): 0x3030.
        // On AMD, ECX[7:0] (NC) is threads-in-package minus one and
        // ECX[15:12] (ApicIdSize) the APIC-ID bits per package — the legacy
        // topology source the kernel cross-checks against leaf 1 EBX and
        // leaf 0xB. Intel leaves ECX reserved-zero.
        let sizes_ecx = match config.vendor {
            CpuVendor::Intel => 0,
            CpuVendor::Amd => {
                let apic_id_size = 32 - (config.vcpu_count.max(1) - 1).leading_zeros();
                (config.vcpu_count - 1) | (apic_id_size << 12)
            }
        };
        entries.push(CpuidCacheEntry {
            leaf: 0x8000_0008,
            subleaf: 0,
            result: CpuidResult {
                eax: 0x0000_3030, // 48-bit linear, 48-bit physical
                ecx: sizes_ecx,
                ..CpuidResult::default()
            },
        });

        // AMD topology-extension leaves (gated by the TOPOEXT bit above).
        if config.vendor == CpuVendor::Amd {
            Self::push_amd_topology_leaves(config, entries);
        }
    }

    /// Push the `0x80000005`/`0x80000006` legacy cache + TLB leaves (field
    /// layouts per AMD APM `Fn8000_0005`/6, cross-checked against Linux's
    /// cacheinfo.c `union l1_cache`/`l2_cache`/`l3_cache`). Both were
    /// in-range zeros: on Intel that contradicts the leaf-4 hierarchy
    /// ("no L2" vs a 256 KiB L2 — a one-instruction cross-check for a
    /// detector); on AMD these legacy leaves are the *primary* cache
    /// enumeration a guest parses.
    fn push_legacy_cache_leaves(config: &CpuidStealthConfig, entries: &mut Vec<CpuidCacheEntry>) {
        match config.vendor {
            CpuVendor::Intel => {
                // Intel implements only ECX (L2): 256 KiB, 4-way (encoding 4),
                // 64-byte lines — matching leaf-4 subleaf 2. 0x80000005 stays
                // reserved-zero on Intel.
                entries.push(CpuidCacheEntry {
                    leaf: 0x8000_0006,
                    subleaf: 0,
                    result: CpuidResult {
                        ecx: 64 | (4 << 12) | (256 << 16),
                        ..CpuidResult::default()
                    },
                });
            }
            CpuVendor::Amd => {
                // L1d/L1i: 32 KiB, 8-way (direct encoding), 1 line/tag,
                // 64-byte lines; 64-entry fully-associative (0xFF) I/D TLBs
                // for both 2M/4M (EAX) and 4K (EBX) pages — Zen-typical.
                let l1 = 64 | (1 << 8) | (8 << 16) | (32 << 24);
                let tlb = 0xFF40_FF40;
                entries.push(CpuidCacheEntry {
                    leaf: 0x8000_0005,
                    subleaf: 0,
                    result: CpuidResult {
                        eax: tlb,
                        ebx: tlb,
                        ecx: l1, // L1 data
                        edx: l1, // L1 instruction
                    },
                });
                // L2 (ECX): 1 MiB, 8-way (encoding 6). L3 (EDX): 32 MiB
                // (size_encoded × 512 KiB → 64), 16-way (encoding 8).
                entries.push(CpuidCacheEntry {
                    leaf: 0x8000_0006,
                    subleaf: 0,
                    result: CpuidResult {
                        ecx: 64 | (6 << 12) | (1024 << 16),
                        edx: 64 | (8 << 12) | (64 << 18),
                        ..CpuidResult::default()
                    },
                });
            }
        }
    }

    /// One `0x8000001D` cache-topology subleaf (AMD APM `Fn8000_001D`; the
    /// field layout mirrors Intel leaf 4, but EAX[31:26] is reserved — AMD
    /// has no cores-per-package field there).
    const fn amd_cache_subleaf(
        cache_type: u32,
        level: u32,
        ways: u32,
        line_size: u32,
        sets: u32,
        shared_by: u32,
    ) -> CpuidResult {
        CpuidResult {
            eax: cache_type | (level << 5) | (1 << 8) | ((shared_by - 1) << 14),
            ebx: (line_size - 1) | ((ways - 1) << 22), // partitions = 1
            ecx: sets - 1,
            edx: 0, // L3 is non-inclusive on Zen; WBINVD scope not asserted
        }
    }

    /// Push the AMD `TOPOEXT` leaves: the `0x8000001D` cache hierarchy and
    /// the `0x8000001E` extended APIC/core/node IDs.
    ///
    /// The `0x8000001D` geometry must encode the **same caches** as the
    /// legacy `0x80000005`/`0x80000006` leaves (32 KiB 8-way L1d/L1i,
    /// 1 MiB 8-way L2, 32 MiB 16-way L3, 64-byte lines) — the kernel parses
    /// both and a detector can diff them in four instructions; a test pins
    /// the pair. Sharing follows the configured topology: L1/L2 per core
    /// (shared by its SMT threads), L3 package-wide.
    fn push_amd_topology_leaves(config: &CpuidStealthConfig, entries: &mut Vec<CpuidCacheEntry>) {
        let smt = config.threads_per_core;
        let subleaves = [
            Self::amd_cache_subleaf(1, 1, 8, 64, 64, smt), // 32 KiB L1d
            Self::amd_cache_subleaf(2, 1, 8, 64, 64, smt), // 32 KiB L1i
            Self::amd_cache_subleaf(3, 2, 8, 64, 2048, smt), // 1 MiB L2
            Self::amd_cache_subleaf(3, 3, 16, 64, 32768, config.vcpu_count), // 32 MiB L3
        ];
        for (i, result) in subleaves.into_iter().enumerate() {
            entries.push(CpuidCacheEntry {
                leaf: 0x8000_001D,
                subleaf: u32_of(i),
                result,
            });
        }
        // A null subleaf (type 0) terminates the enumeration; the in-range
        // zero default already encodes it, as with Intel leaf 4.

        // 0x8000001E: extended APIC ID (EAX), core ID + threads-per-core
        // (EBX[7:0] / EBX[15:8] = threads minus one), node IDs (ECX). The
        // per-vCPU fields (APIC ID, core ID) are runtime concerns patched by
        // the vCPU layer, exactly like leaf 0xB EDX; the table carries the
        // package-invariant encoding.
        entries.push(CpuidCacheEntry {
            leaf: 0x8000_001E,
            subleaf: 0,
            result: CpuidResult {
                eax: 0,              // extended APIC ID (per-vCPU)
                ebx: (smt - 1) << 8, // core ID 0 (per-vCPU); SMT count
                ecx: 0,              // node 0 of 1
                edx: 0,
            },
        });

        // 0x8000_0022: AMD PerfMonV2 capability (Zen 4+). Advertised only when
        // the host really exposes it (captured in `from_host`), so the guest's
        // CPUID-enumerated PMU matches the AMD PerfCtr MSRs the stealth router
        // shadows. `build` has already raised the max extended leaf to cover it.
        // A no-op on a host without PerfMonV2 (and on Intel).
        //
        // The leaf is **constrained to what the MSR router actually backs**, not
        // passed through verbatim: we advertise PerfMonV2 (EAX bit 0) with
        // `NumCorePmc` (EBX[3:0]) clamped to the shadowed core-PMC count, and
        // clear the LBR-stack capabilities — `LbrStack`/`LbrAndPmcFreeze`
        // (EAX bits 1/2) and `LbrStackSize` (EBX[9:4]). The router models only
        // the legacy single LBR pair (0x1DB–0x1DE), not the PerfMonV2 extended
        // LBR stack, so advertising a stack whose MSRs we cannot serve would be a
        // cross-surface tell (CPUID promises a register set RDMSR then #GPs).
        if let Some(perfmon_v2) = config.perfmon_v2 {
            let shadowed_pmcs = super::pmc::msr::AMD_CORE_PMCS;
            let num_core_pmc = (perfmon_v2.ebx & 0xF).min(shadowed_pmcs);
            entries.push(CpuidCacheEntry {
                leaf: 0x8000_0022,
                subleaf: 0,
                result: CpuidResult {
                    eax: perfmon_v2.eax & 0x1, // PerfMonV2 only; no LBR-stack caps
                    ebx: num_core_pmc,         // NumCorePmc only; no LbrStackSize
                    ecx: 0,
                    edx: 0,
                },
            });
        }
    }

    /// Build the stealth CPUID table from physical CPU info.
    #[must_use]
    pub fn build(config: &CpuidStealthConfig) -> Self {
        let mut entries = Vec::with_capacity(128);
        // The max leaves are a vendor fingerprint of their own: no AMD part
        // reports basic max 0x16 (Zen reports 0x10; 0x15/0x16 are the Intel
        // TSC/frequency leaves), and AMD's extended range runs past the
        // topology leaves to 0x8000001F (the SEV leaf — in-range reserved
        // zeros here, consistent with not advertising SME/SEV anywhere else).
        let (max_standard_leaf, max_extended_leaf) = match config.vendor {
            CpuVendor::Intel => (0x16, 0x8000_0008), // frequency / address sizes
            // AMD's extended range normally ends at 0x8000_001F (the SEV leaf);
            // a PerfMonV2 host (Zen 4+) extends it to 0x8000_0022 so the
            // capability leaf below is in range. Leaves 0x8000_0020/0021 stay
            // in-range reserved-zero (not advertised), like the SEV leaf.
            CpuVendor::Amd if config.perfmon_v2.is_some() => (0x10, 0x8000_0022),
            CpuVendor::Amd => (0x10, 0x8000_001F), // PQOS / SEV
        };

        Self::push_standard_leaves(config, max_standard_leaf, &mut entries);
        Self::push_extended_leaves(config, max_extended_leaf, &mut entries);

        let out_of_range = Self::out_of_range_result(config.vendor, &entries);

        Self {
            entries,
            max_standard_leaf,
            max_extended_leaf,
            out_of_range,
        }
    }

    /// Compute the value an out-of-range leaf must return for this vendor.
    ///
    /// Intel returns the data of the **highest basic leaf** for any out-of-range input
    /// (Intel SDM Vol 2A, CPUID); we mirror the highest populated basic-range entry, which is
    /// the canonical bare-metal behaviour a detector checks for. AMD returns zeros for
    /// undefined leaves (AMD APM Vol 3), so the default all-zero result is correct there.
    fn out_of_range_result(vendor: CpuVendor, entries: &[CpuidCacheEntry]) -> CpuidResult {
        match vendor {
            CpuVendor::Amd => CpuidResult::default(),
            CpuVendor::Intel => entries
                .iter()
                .filter(|e| e.leaf < 0x4000_0000)
                .max_by_key(|e| e.leaf)
                .map_or_else(CpuidResult::default, |e| e.result),
        }
    }

    /// Look up a CPUID leaf/subleaf.
    ///
    /// In-range leaves return their cached entry, or zeros if the leaf is in range but
    /// reserved/unpopulated (real CPUs do that for reserved leaves). **Out-of-range** leaves —
    /// above the basic max (including the whole `0x4000_0000` hypervisor region, so it is
    /// indistinguishable from bare metal) or above the extended max — return the vendor-correct
    /// `out_of_range` value: the highest basic leaf's data on Intel, zeros on AMD.
    #[must_use]
    pub fn lookup(&self, leaf: u32, subleaf: u32) -> CpuidResult {
        let in_range = leaf <= self.max_standard_leaf
            || (0x8000_0000..=self.max_extended_leaf).contains(&leaf);

        if in_range {
            for entry in &self.entries {
                if entry.leaf == leaf && entry.subleaf == subleaf {
                    return entry.result;
                }
            }
            // In range but no specific entry — reserved leaf, return zeros (matches real HW).
            return CpuidResult::default();
        }

        // Out of range (incl. the 0x4000_0000 hypervisor region): mirror real-CPU semantics.
        self.out_of_range
    }

    const fn build_leaf_1(config: &CpuidStealthConfig) -> CpuidResult {
        let mut ecx = config.features_ecx;
        if config.hide_hypervisor {
            // Clear bit 31: hypervisor present
            ecx &= !(1 << 31);
        }
        // Clear bit 3: MONITOR/MWAIT. We don't virtualize it (MWAIT exits and
        // leaf 0x5 is unpopulated), and passing the physical bit through while
        // leaf 0x5 reports zero line sizes is an inconsistency. A CPU without
        // MONITOR legitimately reports leaf 0x5 as reserved-zero, so hiding
        // the feature keeps both leaves coherent (KVM does the same by
        // default). Guests then idle via HLT, which we already handle.
        ecx &= !(1 << 3);

        // EBX[23:16] = max number of addressable logical-processor IDs in the package. Real CPUs
        // advertise this (rounded up to a power of two) only when HTT (EDX bit 28) is set, and it
        // tracks the actual topology — a fixed constant that disagrees with the leaf-0xB topology
        // and the vCPU count is a tell. EBX[15:8] = CLFLUSH line size / 8 (64-byte lines);
        // EBX[31:24] (initial APIC ID) is filled in per-vCPU at runtime, so it stays 0 here.
        let htt = (config.features_edx & (1 << 28)) != 0;
        let mut max_ids = if htt {
            config.vcpu_count.next_power_of_two()
        } else {
            1
        };
        if max_ids > 0xFF {
            max_ids = 0xFF;
        }
        let ebx = 0x0000_0800 | (max_ids << 16);

        CpuidResult {
            eax: config.family_model_stepping,
            ebx,
            ecx,
            edx: config.features_edx,
        }
    }

    /// Encode one leaf-0x4 subleaf (Intel SDM Vol. 2A, "Deterministic Cache
    /// Parameters"). `cache_type`: 1 = data, 2 = instruction, 3 = unified.
    /// EAX[25:14] = max logical processors sharing the cache − 1;
    /// EAX[31:26] = max core IDs in the package − 1. EBX packs
    /// (ways − 1, partitions − 1, line size − 1); ECX = sets − 1; so
    /// size = ways × partitions × line × sets.
    fn cache_subleaf(
        config: &CpuidStealthConfig,
        cache_type: u32,
        level: u32,
        ways: u32,
        line_size: u32,
        sets: u32,
        shared_by: u32,
    ) -> CpuidResult {
        let cores = (config.vcpu_count / config.threads_per_core).max(1);
        CpuidResult {
            eax: cache_type
                | (level << 5)
                | (1 << 8) // self-initializing
                | ((shared_by - 1) << 14)
                | ((cores - 1) << 26),
            ebx: (line_size - 1) | ((ways - 1) << 22), // partitions = 1
            ecx: sets - 1,
            edx: 0,
        }
    }

    /// Push the leaf-0x4 cache hierarchy. An all-zero leaf 4 ("no caches")
    /// is something no real CPU reports. If the config carries real
    /// pass-through cache info, use it; otherwise synthesize a standard
    /// client hierarchy: 32 KiB L1d + 32 KiB L1i (8-way, per-core),
    /// 256 KiB unified L2 (4-way, per-core), 16 MiB unified L3 (16-way,
    /// package-wide). Sharing IDs track the configured topology. A null
    /// subleaf (type 0) terminates the enumeration, as on real hardware.
    fn push_cache_leaves(config: &CpuidStealthConfig, entries: &mut Vec<CpuidCacheEntry>) {
        let subleaves: Vec<CpuidResult> = if config.cache_info.is_empty() {
            let smt = config.threads_per_core;
            vec![
                Self::cache_subleaf(config, 1, 1, 8, 64, 64, smt), // 32 KiB L1d
                Self::cache_subleaf(config, 2, 1, 8, 64, 64, smt), // 32 KiB L1i
                Self::cache_subleaf(config, 3, 2, 4, 64, 1024, smt), // 256 KiB L2
                Self::cache_subleaf(config, 3, 3, 16, 64, 16384, config.vcpu_count), // 16 MiB L3
            ]
        } else {
            config.cache_info.clone()
        };

        for (i, result) in subleaves.into_iter().enumerate() {
            entries.push(CpuidCacheEntry {
                leaf: 4,
                subleaf: u32_of(i),
                result,
            });
        }
        // Terminator: in-range zero is the real null-subleaf encoding, so no
        // explicit entry is needed — lookup() already returns zeros there.
    }

    /// Leaf 0x6 — Thermal and Power Management.
    ///
    /// ECX bit 0 advertises the `IA32_APERF`/`IA32_MPERF` MSRs (Intel: "hardware
    /// coordination feedback"; AMD: "effective frequency interface" — same
    /// bit, confirmed against Linux `scattered.c`, which sets
    /// `X86_FEATURE_APERFMPERF` from leaf 6 ECX[0] on both vendors). Our
    /// timing-stealth shadow *serves* those MSRs, so not advertising them is
    /// an inconsistency — and an IET detector that politely checks before
    /// reading would conclude the MSRs shouldn't exist.
    ///
    /// EAX bit 1 (Intel Dynamic Acceleration = turbo) must be set on Intel
    /// because leaf 0x16 advertises a max frequency above base and the PMC
    /// rate model claims core > ref — both imply turbo. (AMD signals boost
    /// via leaf 0x80000007 EDX[9] instead.) EAX bit 2 (ARAT, always-running
    /// APIC timer) is set on both: our virtual APIC timer never stops in
    /// deep C-states, and every CPU of the advertised generation has it.
    const fn build_leaf_6(config: &CpuidStealthConfig) -> CpuidResult {
        let eax = match config.vendor {
            CpuVendor::Intel => (1 << 1) | (1 << 2), // IDA (turbo) + ARAT
            CpuVendor::Amd => 1 << 2,                // ARAT
        };
        CpuidResult {
            eax,
            ebx: 0,
            ecx: 1, // APERF/MPERF present
            edx: 0,
        }
    }

    /// Leaf 7 subleaf 0: the config's captured candidates masked to the
    /// virtualizable allowlist. EDX (the speculation-control enumeration —
    /// IBRS/STIBP/SSBD/`ARCH_CAPABILITIES`) stays zero: each bit advertises an
    /// MSR surface we don't emulate yet, and an all-clear EDX just reads as
    /// pre-mitigation microcode (the guest applies retpolines — slower but
    /// coherent), not as a virtual machine.
    const fn build_leaf_7(config: &CpuidStealthConfig) -> CpuidResult {
        CpuidResult {
            eax: 0, // max subleaf
            ebx: config.features_7_ebx & LEAF7_EBX_PASSTHROUGH,
            ecx: config.features_7_ecx & LEAF7_ECX_PASSTHROUGH,
            edx: 0,
        }
    }

    /// Leaf 0xA — Architectural Performance Monitoring (Intel SDM Vol. 2A;
    /// field layout cross-checked against Linux's `union cpuid10_{eax,ebx,edx}`).
    ///
    /// The table used to leave this leaf unpopulated, so the in-range-reserved
    /// fallback returned all zeros — PMU version 0, "no architectural PMU".
    /// Every Intel CPU since Core reports version ≥ 1 (cloud vPMU-less VMs are
    /// the ones that report 0, making it a tell by itself), and it contradicts
    /// the PMC shadow (`stealth::pmc`) servicing RDPMC. Advertise PMU version 5
    /// with exactly the counters the shadow implements: [`super::pmc::MAX_GP_PMCS`]
    /// general-purpose and [`super::pmc::MAX_FIXED_PMCS`] fixed counters
    /// (including TOPDOWN.SLOTS), 48 bits wide, all seven architectural events
    /// available (EBX = 0), ECX = supported-fixed-counter bitmask (version-5
    /// semantics), and EDX.AnyThread-deprecated set (version 5 deprecates it).
    fn build_leaf_a() -> CpuidResult {
        use super::pmc::{MAX_FIXED_PMCS, MAX_GP_PMCS};
        let gp_counters = u32_of(MAX_GP_PMCS);
        let fixed_counters = u32_of(MAX_FIXED_PMCS);
        let counter_width = 48;
        let event_vector_len = 7;
        CpuidResult {
            eax: 5 | (gp_counters << 8) | (counter_width << 16) | (event_vector_len << 24),
            ebx: 0, // all architectural events available
            ecx: (1 << fixed_counters) - 1,
            edx: fixed_counters | (counter_width << 5) | (1 << 15),
        }
    }

    fn build_topology_leaves(config: &CpuidStealthConfig, entries: &mut Vec<CpuidCacheEntry>) {
        // Subleaf 0: SMT level. EAX is the number of x2APIC-ID bits the SMT field
        // occupies — ceil(log2(threads_per_core)), so a core with N threads shifts
        // the ID right by exactly enough bits to reach the core ID.
        let threads_per_core = config.threads_per_core;
        let smt_shift = ceil_log2(threads_per_core);
        entries.push(CpuidCacheEntry {
            leaf: 0xB,
            subleaf: 0,
            result: CpuidResult {
                eax: smt_shift,
                ebx: threads_per_core,
                ecx: (1 << 8), // SMT level type = 1, level number = 0
                edx: 0,        // x2APIC ID (set per-vCPU at runtime)
            },
        });

        // Subleaf 1: Core level. EAX is the cumulative shift past the SMT *and*
        // core fields = ceil(log2(threads)) + ceil(log2(cores)); right-shifting an
        // x2APIC ID by it yields the package ID. The naive `32 - cores.leading_zeros()`
        // is `floor(log2)+1`, which overshoots power-of-two core counts by a bit
        // (4 cores → 3 instead of 2), corrupting the package boundary a guest derives.
        let cores = config.vcpu_count / threads_per_core;
        let core_shift = ceil_log2(cores);
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

/// `ceil(log2(n))` — the number of bits needed to enumerate `n` distinct items,
/// i.e. the smallest `b` with `2^b >= n`. Returns 0 for `n <= 1`.
///
/// This is the correct width for an x2APIC-ID topology field. Note it differs
/// from `32 - n.leading_zeros()` (which is `floor(log2(n)) + 1`) on exact powers
/// of two: `ceil_log2(4) == 2`, whereas the latter gives 3. Using `(n - 1)`
/// before counting leading zeros collapses that off-by-one.
const fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        u32::BITS - (n - 1).leading_zeros()
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
            features_7_ebx: 0x0000_0281, // FSGSBASE (b0), SMEP (b7), ERMS (b9)
            features_7_ecx: 0,
            brand_string: brand_string_from_str("AMD Ryzen 9 7950X 16-Core Processor"),
            vcpu_count: 8,
            threads_per_core: 2,
            hide_hypervisor: true,
            cache_info: Vec::new(),
            base_frequency_mhz: 2800,
            max_frequency_mhz: 3300,
            perfmon_v2: None,
        }
    }

    #[test]
    fn hypervisor_bit_cleared() {
        let table = CpuidStealthTable::build(&test_config());
        let result = table.lookup(1, 0);
        assert_eq!(result.ecx & (1 << 31), 0, "Hypervisor bit must be cleared");
    }

    #[test]
    fn amd_hypervisor_leaves_return_zero() {
        // AMD returns zeros for out-of-range/undefined leaves (AMD APM Vol 3); test_config is AMD.
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
    fn amd_perfmon_v2_leaf_absent_when_host_lacks_it() {
        // The default AMD test_config has perfmon_v2 = None (this nested host's
        // case): leaf 0x8000_0022 must be out of range and the max extended leaf
        // stays at the SEV leaf 0x8000_001F.
        let table = CpuidStealthTable::build(&test_config());
        assert_eq!(table.lookup(0x8000_0000, 0).eax, 0x8000_001F);
        assert_eq!(table.lookup(0x8000_0022, 0), CpuidResult::default());
    }

    #[test]
    fn amd_perfmon_v2_leaf_advertised_when_host_has_it() {
        // A Zen 4-style host: PerfMonV2 (EAX bit 0) with 6 core PMCs (EBX[3:0]).
        let host_leaf = CpuidResult {
            eax: 0x1,
            ebx: 0x6,
            ecx: 0,
            edx: 0,
        };
        let cfg = CpuidStealthConfig {
            perfmon_v2: Some(host_leaf),
            ..test_config()
        };
        let table = CpuidStealthTable::build(&cfg);
        // Max extended leaf is raised to cover 0x8000_0022.
        assert_eq!(table.lookup(0x8000_0000, 0).eax, 0x8000_0022);
        // PerfMonV2 with 6 core PMCs and no LBR-stack bits is already exactly
        // what the router backs, so it survives the constraining unchanged.
        assert_eq!(table.lookup(0x8000_0022, 0), host_leaf);
        // 0x8000_0020/0021 stay in-range reserved-zero (not advertised).
        assert_eq!(table.lookup(0x8000_0020, 0), CpuidResult::default());
        assert_eq!(table.lookup(0x8000_0021, 0), CpuidResult::default());
        // 0x8000_0023 is now out of range → AMD zeros.
        assert_eq!(table.lookup(0x8000_0023, 0), CpuidResult::default());
    }

    #[test]
    fn amd_perfmon_v2_leaf_advertises_only_what_the_router_backs() {
        // Host advertises PerfMonV2 + LbrStack + LbrAndPmcFreeze (EAX bits 0-2)
        // and a large NumCorePmc (EBX[3:0]=15) + LbrStackSize (EBX[9:4]=16). The
        // router backs only core-PMC counting and the legacy LBR pair, so the
        // emitted leaf keeps PerfMonV2 with NumCorePmc clamped to the shadowed
        // count and drops every LBR-stack capability.
        let host_leaf = CpuidResult {
            eax: 0x7,
            ebx: 0xF | (0x10 << 4),
            ecx: 0xDEAD,
            edx: 0xBEEF,
        };
        let cfg = CpuidStealthConfig {
            perfmon_v2: Some(host_leaf),
            ..test_config()
        };
        let leaf = CpuidStealthTable::build(&cfg).lookup(0x8000_0022, 0);
        assert_eq!(leaf.eax, 0x1, "PerfMonV2 only; LbrStack/Freeze cleared");
        assert_eq!(
            leaf.ebx,
            crate::stealth::pmc::msr::AMD_CORE_PMCS,
            "NumCorePmc clamped to the shadowed count; LbrStackSize cleared"
        );
        assert_eq!(leaf.ecx, 0);
        assert_eq!(leaf.edx, 0);
    }

    #[test]
    fn intel_config_never_advertises_amd_perfmon_v2() {
        // Even if a (nonsensical) Intel config carried perfmon_v2, the AMD-only
        // emission path must not fire for an Intel vendor.
        let cfg = CpuidStealthConfig {
            perfmon_v2: Some(CpuidResult {
                eax: 0x1,
                ..CpuidResult::default()
            }),
            ..intel_config()
        };
        let table = CpuidStealthTable::build(&cfg);
        assert_eq!(table.lookup(0x8000_0000, 0).eax, 0x8000_0008);
        // Out of range on Intel → mirrors the highest basic leaf, not the leaf.
        assert_ne!(table.lookup(0x8000_0022, 0).eax, 0x1);
    }

    fn intel_config() -> CpuidStealthConfig {
        CpuidStealthConfig {
            vendor: CpuVendor::Intel,
            ..test_config()
        }
    }

    #[test]
    fn intel_out_of_range_mirrors_highest_basic_leaf() {
        // Intel SDM: an out-of-range leaf returns the highest basic leaf's data, NOT zeros.
        // The highest populated basic-range leaf in the table is 0x16 (frequencies).
        let table = CpuidStealthTable::build(&intel_config());
        let highest_basic = table.lookup(0x16, 0);
        assert_ne!(
            highest_basic,
            CpuidResult::default(),
            "leaf 0xD must be populated for this test to be meaningful"
        );

        // Above the basic max but below 0x4000_0000.
        assert_eq!(table.lookup(0x20, 0), highest_basic);
        assert_eq!(table.lookup(0x1337, 0), highest_basic);
        // Above the extended max.
        assert_eq!(table.lookup(0x8000_0009, 0), highest_basic);
        assert_eq!(table.lookup(0xFFFF_FFFF, 0), highest_basic);
    }

    #[test]
    fn intel_hypervisor_region_indistinguishable_from_bare_metal() {
        // The 0x4000_0000 region must look exactly like an out-of-range leaf on Intel, so a
        // detector comparing CPUID(0x40000000) to a bogus leaf sees no mismatch — and crucially
        // sees no all-zeros (which no real Intel CPU returns out of range).
        let table = CpuidStealthTable::build(&intel_config());
        let bogus = table.lookup(0x1337_1337, 0);
        for leaf in [0x4000_0000, 0x4000_0001, 0x4000_00FF] {
            let hv = table.lookup(leaf, 0);
            assert_eq!(
                hv, bogus,
                "hypervisor leaf {leaf:#x} must match a bogus leaf"
            );
            assert_ne!(hv, CpuidResult::default(), "must not be all-zeros on Intel");
        }
    }

    #[test]
    fn leaf_1_ebx_tracks_vcpu_count() {
        // test_config: 8 vCPUs, HTT set in features_edx → EBX[23:16] = 8 (power of two).
        let table = CpuidStealthTable::build(&test_config());
        let ebx = table.lookup(1, 0).ebx;
        assert_eq!(
            (ebx >> 16) & 0xFF,
            8,
            "max addressable IDs must equal vCPU count"
        );
        assert_eq!((ebx >> 8) & 0xFF, 0x08, "CLFLUSH line size byte preserved");
        assert_eq!(ebx & 0xFF, 0, "brand index byte stays 0");

        // 6 vCPUs rounds up to the next power of two (8).
        let cfg = CpuidStealthConfig {
            vcpu_count: 6,
            ..test_config()
        };
        let ebx = CpuidStealthTable::build(&cfg).lookup(1, 0).ebx;
        assert_eq!((ebx >> 16) & 0xFF, 8);
    }

    #[test]
    fn leaf_1_ebx_without_htt_is_one() {
        // HTT (EDX bit 28) clear → max addressable IDs = 1 regardless of vCPU count.
        let cfg = CpuidStealthConfig {
            features_edx: test_config().features_edx & !(1 << 28),
            ..test_config()
        };
        let ebx = CpuidStealthTable::build(&cfg).lookup(1, 0).ebx;
        assert_eq!((ebx >> 16) & 0xFF, 1);
    }

    /// Decode one leaf-4-style cache subleaf into (type, level, size-bytes,
    /// ways, line-size, sharing count).
    fn decode_cache_subleaf(r: CpuidResult) -> (u32, u32, u32, u32, u32, u32) {
        let ctype = r.eax & 0x1F;
        let level = (r.eax >> 5) & 0x7;
        let sharing = ((r.eax >> 14) & 0xFFF) + 1;
        let line = (r.ebx & 0xFFF) + 1;
        let ways = ((r.ebx >> 22) & 0x3FF) + 1;
        let sets = r.ecx + 1;
        (ctype, level, ways * line * sets, ways, line, sharing)
    }

    #[test]
    fn amd_topoext_cache_leaves_match_the_legacy_cache_leaves() {
        let cfg = test_config(); // AMD
        let table = CpuidStealthTable::build(&cfg);

        // The TOPOEXT bit gates the leaves; both must be present together.
        assert_ne!(
            table.lookup(0x8000_0001, 0).ecx & (1 << 22),
            0,
            "TOPOEXT must be advertised when 0x8000001D/1E are populated"
        );

        // 0x8000001D: L1d, L1i, L2, L3 — then a null terminator.
        let l1d = decode_cache_subleaf(table.lookup(0x8000_001D, 0));
        let l1i = decode_cache_subleaf(table.lookup(0x8000_001D, 1));
        let l2 = decode_cache_subleaf(table.lookup(0x8000_001D, 2));
        let l3 = decode_cache_subleaf(table.lookup(0x8000_001D, 3));
        assert_eq!(table.lookup(0x8000_001D, 4).eax & 0x1F, 0, "terminator");

        // Same caches the legacy leaves enumerate (the kernel parses both):
        // 0x80000005 — L1d/L1i 32 KiB, 8-way, 64-byte lines.
        let legacy_l1 = table.lookup(0x8000_0005, 0);
        for (name, (ctype, level, size, ways, line, _)) in [("L1d", l1d), ("L1i", l1i)] {
            assert_eq!(level, 1, "{name} level");
            assert!(ctype == 1 || ctype == 2, "{name} type");
            assert_eq!(size, (legacy_l1.ecx >> 24) * 1024, "{name} size");
            assert_eq!(ways, (legacy_l1.ecx >> 16) & 0xFF, "{name} ways");
            assert_eq!(line, legacy_l1.ecx & 0xFF, "{name} line");
        }
        // 0x80000006 — L2 1 MiB (8-way: encoding 6), L3 32 MiB (16-way:
        // encoding 8; size field counts 512 KiB units).
        let legacy_l23 = table.lookup(0x8000_0006, 0);
        assert_eq!(l2.1, 2);
        assert_eq!(l2.2, ((legacy_l23.ecx >> 16) & 0xFFFF) * 1024, "L2 size");
        assert_eq!(l2.3, 8, "L2 ways (legacy encoding 6)");
        assert_eq!(l3.1, 3);
        assert_eq!(l3.2, (legacy_l23.edx >> 18) * 512 * 1024, "L3 size");
        assert_eq!(l3.3, 16, "L3 ways (legacy encoding 8)");

        // Sharing tracks the topology: L1/L2 per core (SMT threads), L3
        // package-wide.
        assert_eq!(l1d.5, cfg.threads_per_core);
        assert_eq!(l2.5, cfg.threads_per_core);
        assert_eq!(l3.5, cfg.vcpu_count);

        // 0x8000001E: EBX[15:8] + 1 = threads per core.
        let ext = table.lookup(0x8000_001E, 0);
        assert_eq!(((ext.ebx >> 8) & 0xFF) + 1, cfg.threads_per_core);
    }

    /// `from_host` must capture this machine's identity and clear what a
    /// guest must not see. (The CI runner is itself a VM with the hypervisor
    /// bit set — a perfect negative reference for the hide path.)
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn from_host_captures_the_machine_and_hides_the_hypervisor() {
        use core::arch::x86_64::__cpuid;

        let cfg = CpuidStealthConfig::from_host(8, 2);
        let table = CpuidStealthTable::build(&cfg);

        // Vendor: the table's leaf 0 matches the host's (when the host is a
        // recognized vendor; an exotic vendor falls back to the Intel profile).
        let host0 = __cpuid(0);
        let host_vendor = (host0.ebx, host0.edx, host0.ecx);
        if host_vendor == CpuVendor::Amd.vendor_regs()
            || host_vendor == CpuVendor::Intel.vendor_regs()
        {
            let r = table.lookup(0, 0);
            assert_eq!((r.ebx, r.edx, r.ecx), host_vendor);
        }

        // Family/model/stepping pass through; the hypervisor bit does not —
        // even when (especially when) the host sets it.
        let host1 = __cpuid(1);
        assert_eq!(table.lookup(1, 0).eax, host1.eax);
        assert_eq!(table.lookup(1, 0).ecx & (1 << 31), 0);

        // The brand string was captured (never all-zero on a real x86_64).
        assert!(cfg.brand_string.iter().any(|&b| b != 0));

        // On an Intel host the leaf-4 geometry passes through with the
        // sharing fields rewritten to the GUEST topology, not the host's.
        if cfg.vendor == CpuVendor::Intel && !cfg.cache_info.is_empty() {
            for r in &cfg.cache_info {
                let level = (r.eax >> 5) & 0x7;
                let sharing = ((r.eax >> 14) & 0xFFF) + 1;
                let expected = if level >= 3 { 8 } else { 2 };
                assert_eq!(sharing, expected, "L{level} sharing tracks the guest");
                assert_eq!((r.eax >> 26) + 1, 4, "cores per package");
            }
            // The built table serves the rewritten host geometry at leaf 4.
            assert_eq!(table.lookup(4, 0), cfg.cache_info[0]);
        }
    }

    /// Whatever a host capture claims, leaf 7 must only advertise the
    /// allowlist — every excluded bit pairs with state or an MSR surface the
    /// virtual platform doesn't carry, and the cross-surface partner pins it.
    #[test]
    fn leaf_7_masks_a_hostile_capture_to_the_virtualizable_set() {
        let cfg = CpuidStealthConfig {
            features_7_ebx: u32::MAX, // a host with everything (AVX-512, TSX, SGX…)
            features_7_ecx: u32::MAX,
            ..test_config()
        };
        let table = CpuidStealthTable::build(&cfg);
        let r = table.lookup(7, 0);
        assert_eq!(r.ebx, LEAF7_EBX_PASSTHROUGH);
        assert_eq!(r.ecx, LEAF7_ECX_PASSTHROUGH);
        assert_eq!(r.edx, 0, "speculation-control MSR surfaces unadvertised");
        // The pinned cross-surface pairs: no AVX-512 without leaf-0xD state
        // components; no LA57 against 0x80000008's 48-bit linear.
        for bit in [16u32, 17, 21, 26, 27, 28, 30, 31] {
            assert_eq!(r.ebx & (1 << bit), 0, "AVX-512 EBX bit {bit}");
        }
        assert_eq!(r.ecx & (1 << 16), 0, "LA57");
        // TSX / SGX / MPX / PT stay hidden.
        for bit in [2u32, 4, 11, 14, 25] {
            assert_eq!(r.ebx & (1 << bit), 0, "EBX bit {bit}");
        }
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn from_host_captures_leaf_7_candidates() {
        use core::arch::x86_64::__cpuid_count;
        let cfg = CpuidStealthConfig::from_host(8, 2);
        let host7 = __cpuid_count(7, 0);
        assert_eq!(cfg.features_7_ebx, host7.ebx);
        assert_eq!(cfg.features_7_ecx, host7.ecx);
        // And the built table serves the masked intersection.
        let r = CpuidStealthTable::build(&cfg).lookup(7, 0);
        assert_eq!(r.ebx, host7.ebx & LEAF7_EBX_PASSTHROUGH);
        assert_eq!(r.ecx, host7.ecx & LEAF7_ECX_PASSTHROUGH);
    }

    #[test]
    fn max_leaves_are_vendor_correct() {
        // AMD: basic max 0x10 (no Intel frequency leaves), extended max
        // 0x8000001F with the SEV leaf as in-range reserved zeros.
        let amd = CpuidStealthTable::build(&test_config());
        assert_eq!(amd.lookup(0, 0).eax, 0x10);
        assert_eq!(amd.lookup(0x8000_0000, 0).eax, 0x8000_001F);
        assert_eq!(amd.lookup(0x8000_001F, 0), CpuidResult::default());
        // Intel: basic max 0x16, extended max 0x80000008, and no TOPOEXT.
        let intel = CpuidStealthTable::build(&intel_config());
        assert_eq!(intel.lookup(0, 0).eax, 0x16);
        assert_eq!(intel.lookup(0x8000_0000, 0).eax, 0x8000_0008);
        assert_eq!(intel.lookup(0x8000_0001, 0).ecx & (1 << 22), 0);
    }

    #[test]
    fn amd_boost_and_legacy_core_count_fields() {
        let cfg = test_config();
        let amd = CpuidStealthTable::build(&cfg);
        // 0x80000007 EDX: invariant TSC + CPB (boost) + EffFreq (APERF/MPERF).
        let edx = amd.lookup(0x8000_0007, 0).edx;
        assert_eq!(edx & (1 << 8), 1 << 8, "invariant TSC");
        assert_eq!(edx & (1 << 9), 1 << 9, "CPB: boost backs core > ref");
        assert_eq!(edx & (1 << 10), 1 << 10, "EffFreq: the MSR shadows exist");
        // 0x80000008 ECX: NC = threads-in-package - 1; ApicIdSize covers it.
        let ecx = amd.lookup(0x8000_0008, 0).ecx;
        assert_eq!(ecx & 0xFF, cfg.vcpu_count - 1, "NC");
        assert_eq!((ecx >> 12) & 0xF, 3, "ApicIdSize for 8 threads");
        // Intel: boost is leaf-6 IDA instead; ECX stays reserved.
        let intel = CpuidStealthTable::build(&intel_config());
        assert_eq!(intel.lookup(0x8000_0007, 0).edx, 1 << 8);
        assert_eq!(intel.lookup(0x8000_0008, 0).ecx, 0);
    }

    #[test]
    fn leaf_7_advertises_exactly_the_captured_candidates_within_the_allowlist() {
        // The config captured EBX 0x281 = FSGSBASE (b0), SMEP (b7), ERMS (b9) — all
        // inside LEAF7_EBX_PASSTHROUGH — so the leaf advertises exactly those bits:
        // candidates below the allowlist flow through unmodified, and nothing the
        // host didn't report is invented.
        let table = CpuidStealthTable::build(&intel_config());
        let r = table.lookup(7, 0);
        assert_eq!(r.eax, 0, "subleaf 0 is the only structured-feature subleaf");
        assert_eq!(
            r.ebx,
            (1 << 0) | (1 << 7) | (1 << 9),
            "captured FSGSBASE/SMEP/ERMS pass through; no extra bits appear"
        );
        // The XSAVE area (leaf 0xD subleaf 0) carries x87+SSE+AVX (XCR0 bits 0-2) —
        // the state every allowlisted leaf-7 instruction extension operates on.
        assert_eq!(
            table.lookup(0xD, 0).eax & 0b111,
            0b111,
            "x87+SSE+AVX in XCR0"
        );
    }

    #[test]
    fn leaf_80000008_address_sizes_consistent() {
        // Physical (EAX[7:0]) and linear (EAX[15:8]) address bits must be plausible and the linear
        // width must not imply LA57 (57) while leaf 7 ECX advertises no LA57.
        let table = CpuidStealthTable::build(&intel_config());
        let eax = table.lookup(0x8000_0008, 0).eax;
        let phys = eax & 0xFF;
        let linear = (eax >> 8) & 0xFF;
        assert_eq!(phys, 48, "physical address bits");
        assert_eq!(linear, 48, "linear address bits (no LA57)");
        // Cross-check: leaf 7 ECX bit 16 (LA57) is clear, so 57-bit linear would be inconsistent.
        assert_eq!(
            table.lookup(7, 0).ecx & (1 << 16),
            0,
            "LA57 must be unadvertised"
        );
    }

    #[test]
    fn intel_leaf_a_advertises_a_pmu_consistent_with_the_pmc_shadow() {
        use crate::stealth::pmc::{MAX_FIXED_PMCS, MAX_GP_PMCS};
        let table = CpuidStealthTable::build(&intel_config());
        let r = table.lookup(0xA, 0);

        // Version 0 ("no PMU") is itself a tell — only vPMU-less VMs report it —
        // and contradicts the PMC shadow servicing RDPMC.
        assert_eq!(r.eax & 0xFF, 5, "PMU version");
        assert_eq!(
            (r.eax >> 8) & 0xFF,
            u32_of(MAX_GP_PMCS),
            "GP counter count must match the PMC shadow"
        );
        assert_eq!((r.eax >> 16) & 0xFF, 48, "GP counter width");
        assert_eq!((r.eax >> 24) & 0xFF, 7, "event vector length");
        assert_eq!(r.ebx, 0, "all architectural events available");
        assert_eq!(
            r.ecx,
            (1 << MAX_FIXED_PMCS) - 1,
            "v5 supported-fixed-counter bitmask"
        );
        assert_eq!(
            r.edx & 0x1F,
            u32_of(MAX_FIXED_PMCS),
            "fixed counter count must match the PMC shadow"
        );
        assert_eq!((r.edx >> 5) & 0xFF, 48, "fixed counter width");
        assert_eq!((r.edx >> 15) & 1, 1, "AnyThread deprecated (v5)");
    }

    #[test]
    fn amd_leaf_a_stays_reserved_zero() {
        // Leaves 0xA/0x15/0x16 are Intel-only; on AMD they stay reserved-zero.
        let table = CpuidStealthTable::build(&test_config());
        assert_eq!(table.lookup(0xA, 0), CpuidResult::default());
        assert_eq!(table.lookup(0x15, 0), CpuidResult::default());
        assert_eq!(table.lookup(0x16, 0), CpuidResult::default());
    }

    /// Decode a leaf-0x4 subleaf into (type, level, size-in-bytes, sharing IDs).
    fn decode_cache(r: CpuidResult) -> (u32, u32, u64, u32) {
        let ways = u64::from((r.ebx >> 22) & 0x3FF) + 1;
        let partitions = u64::from((r.ebx >> 12) & 0x3FF) + 1;
        let line = u64::from(r.ebx & 0xFFF) + 1;
        let sets = u64::from(r.ecx) + 1;
        (
            r.eax & 0x1F,
            (r.eax >> 5) & 0x7,
            ways * partitions * line * sets,
            ((r.eax >> 14) & 0xFFF) + 1,
        )
    }

    #[test]
    fn intel_leaf_2_reports_al_01_and_defers_to_leaf_4() {
        // SDM: leaf 2 AL always returns 01H (all-zero EAX is a tell); the 0xFF
        // descriptor defers cache enumeration to leaf 4.
        let r = CpuidStealthTable::build(&intel_config()).lookup(2, 0);
        assert_eq!(r.eax & 0xFF, 0x01);
        assert_eq!((r.eax >> 8) & 0xFF, 0xFF);

        // AMD: leaf 2 is reserved-zero.
        let amd = CpuidStealthTable::build(&test_config()).lookup(2, 0);
        assert_eq!(amd, CpuidResult::default());
    }

    #[test]
    fn intel_leaf_4_enumerates_a_plausible_cache_hierarchy() {
        let table = CpuidStealthTable::build(&intel_config());

        // (type, level, size, sharing): L1d, L1i, L2, L3 — and termination.
        let expect = [
            (1, 1, 32 * 1024, 2u32),     // 32 KiB L1d, shared by 2 SMT threads
            (2, 1, 32 * 1024, 2),        // 32 KiB L1i
            (3, 2, 256 * 1024, 2),       // 256 KiB unified L2
            (3, 3, 16 * 1024 * 1024, 8), // 16 MiB unified L3, package-wide (8 vCPUs)
        ];
        for (i, &(ty, lvl, size, shared)) in expect.iter().enumerate() {
            let r = table.lookup(4, u32_of(i));
            let (got_ty, got_lvl, got_size, got_shared) = decode_cache(r);
            assert_eq!(got_ty, ty, "subleaf {i} cache type");
            assert_eq!(got_lvl, lvl, "subleaf {i} level");
            assert_eq!(got_size, size, "subleaf {i} size");
            assert_eq!(got_shared, shared, "subleaf {i} sharing IDs");
            assert_eq!(r.eax & (1 << 8), 1 << 8, "subleaf {i} self-initializing");
        }

        // Subleaf 4 terminates with a null type, like real hardware.
        assert_eq!(table.lookup(4, 4).eax & 0x1F, 0, "null terminator");

        // AMD enumerates caches via 0x8000001D; leaf 4 stays zero.
        let amd = CpuidStealthTable::build(&test_config()).lookup(4, 0);
        assert_eq!(amd, CpuidResult::default());
    }

    #[test]
    fn leaf_4_passes_through_configured_cache_info() {
        let custom = CpuidResult {
            eax: 1 | (1 << 5) | (1 << 8),
            ebx: 63 | (11 << 22), // 12-way, 64-byte lines
            ecx: 63,              // 64 sets → 48 KiB
            edx: 0,
        };
        let cfg = CpuidStealthConfig {
            cache_info: vec![custom],
            ..intel_config()
        };
        let table = CpuidStealthTable::build(&cfg);
        assert_eq!(table.lookup(4, 0), custom, "pass-through wins");
        assert_eq!(table.lookup(4, 1).eax & 0x1F, 0, "then terminates");
    }

    #[test]
    fn leaf_6_advertises_the_msrs_the_timing_shadow_serves() {
        // Intel: APERF/MPERF present (ECX[0]), turbo (EAX[1]) — required by
        // leaf 0x16's max > base and the PMC core/ref ratio > 1 — and ARAT.
        let intel = CpuidStealthTable::build(&intel_config()).lookup(6, 0);
        assert_eq!(intel.ecx & 1, 1, "APERF/MPERF must be advertised");
        assert_eq!(intel.eax & (1 << 1), 1 << 1, "turbo (IDA)");
        assert_eq!(intel.eax & (1 << 2), 1 << 2, "ARAT");

        // AMD: effective-frequency interface (same ECX bit) + ARAT, but no
        // Intel IDA bit (AMD boost lives in leaf 0x80000007 EDX[9]).
        let amd = CpuidStealthTable::build(&test_config()).lookup(6, 0);
        assert_eq!(amd.ecx & 1, 1, "effective frequency interface");
        assert_eq!(amd.eax & (1 << 1), 0, "no Intel IDA bit on AMD");
        assert_eq!(amd.eax & (1 << 2), 1 << 2, "ARAT");
    }

    #[test]
    fn intel_frequency_leaves_are_enumerated_and_consistent() {
        let table = CpuidStealthTable::build(&intel_config());

        // Leaf 0x15: TSC rate = crystal × EBX/EAX must equal the base
        // frequency exactly (the guest OS calibrates its clocks from this).
        let r15 = table.lookup(0x15, 0);
        assert_ne!(r15.ebx, 0, "TSC/crystal ratio must be enumerated");
        let tsc_hz = u64::from(r15.ecx) * u64::from(r15.ebx) / u64::from(r15.eax);
        assert_eq!(tsc_hz, 2800 * 1_000_000, "TSC rate = base frequency");

        // Leaf 0x16: base/max/bus in MHz.
        let r16 = table.lookup(0x16, 0);
        assert_eq!(r16.eax, 2800, "base MHz");
        assert_eq!(r16.ebx, 3300, "max turbo MHz");
        assert_eq!(r16.ecx, 100, "bus MHz");

        // Cross-surface: the PMC rate model claims core = 1.15 × ref; the
        // advertised turbo headroom (3300/2800 ≈ 1.18) must cover it, or the
        // counters imply a frequency above the CPU's own stated maximum.
        let model = crate::stealth::pmc::PmcRateModel::DEFAULT;
        assert!(
            u64::from(r16.ebx) * 1000 >= u64::from(r16.eax) * model.core_per_kilo_ref,
            "turbo headroom must cover the PMC core/ref ratio"
        );
    }

    #[test]
    fn in_range_reserved_leaf_returns_zero() {
        // Leaf 0x3 is in the advertised basic range but unpopulated → zeros (matches real HW),
        // distinct from the out-of-range mirror.
        let table = CpuidStealthTable::build(&intel_config());
        assert_eq!(table.lookup(0x3, 0), CpuidResult::default());
    }

    #[test]
    fn monitor_mwait_is_hidden_consistently() {
        // test_config's features_ecx has bit 3 (MONITOR) set; the table must
        // clear it because leaf 0x5 is unpopulated — advertising MONITOR with
        // zero monitor-line sizes is an inconsistency.
        let cfg = test_config();
        assert_eq!(cfg.features_ecx & (1 << 3), 1 << 3, "fixture has MONITOR");
        let table = CpuidStealthTable::build(&cfg);
        assert_eq!(table.lookup(1, 0).ecx & (1 << 3), 0, "MONITOR hidden");
        assert_eq!(table.lookup(5, 0), CpuidResult::default(), "leaf 5 empty");
    }

    #[test]
    fn intel_extended_l2_matches_the_leaf_4_hierarchy() {
        // 0x80000006 ECX must describe the same L2 as leaf 4 subleaf 2: a
        // detector can compare the two with one instruction each.
        let table = CpuidStealthTable::build(&intel_config());
        let ecx = table.lookup(0x8000_0006, 0).ecx;
        let (size_kb, assoc_code, line) = (ecx >> 16, (ecx >> 12) & 0xF, ecx & 0xFF);
        assert_eq!(assoc_code, 4, "4-way (legacy encoding)");
        assert_eq!(line, 64);

        let (_, lvl, l4_size, _) = decode_cache(table.lookup(4, 2));
        assert_eq!(lvl, 2);
        assert_eq!(u64::from(size_kb) * 1024, l4_size, "L2 sizes must agree");
    }

    #[test]
    fn amd_legacy_cache_leaves_are_populated() {
        let table = CpuidStealthTable::build(&test_config());

        // Fn8000_0005: L1d (ECX) and L1i (EDX): 32 KiB, 8-way, 64-byte lines.
        let r5 = table.lookup(0x8000_0005, 0);
        for l1 in [r5.ecx, r5.edx] {
            assert_eq!(l1 >> 24, 32, "size KiB");
            assert_eq!((l1 >> 16) & 0xFF, 8, "associativity (direct)");
            assert_eq!(l1 & 0xFF, 64, "line size");
        }
        assert_ne!(r5.eax, 0, "TLB info present");

        // Fn8000_0006: L2 = 1 MiB 8-way (encoding 6); L3 = 32 MiB (64 × 512 KiB).
        let r6 = table.lookup(0x8000_0006, 0);
        assert_eq!(r6.ecx >> 16, 1024, "L2 KiB");
        assert_eq!((r6.ecx >> 12) & 0xF, 6, "L2 8-way encoding");
        assert_eq!(r6.edx >> 18, 64, "L3 size_encoded × 512 KiB = 32 MiB");
    }

    #[test]
    fn invariant_tsc_is_advertised() {
        // 0x80000007 EDX[8] on both vendors: without it the guest treats the
        // TSC as unstable and routes timekeeping through HPET.
        for cfg in [test_config(), intel_config()] {
            let r = CpuidStealthTable::build(&cfg).lookup(0x8000_0007, 0);
            assert_eq!(r.edx & (1 << 8), 1 << 8, "invariant TSC");
        }
    }

    #[test]
    fn xsave_subleaves_locate_the_advertised_avx_state() {
        let table = CpuidStealthTable::build(&intel_config());
        let main = table.lookup(0xD, 0);
        assert_eq!(main.eax & 0x4, 0x4, "AVX state advertised in subleaf 0");

        let avx = table.lookup(0xD, 2);
        assert_eq!(avx.eax, 256, "AVX component size");
        assert_eq!(avx.ebx, 576, "AVX component offset (512 legacy + 64 hdr)");
        // The advertised total area must cover offset + size exactly.
        assert_eq!(main.ebx, avx.ebx + avx.eax, "XSAVE area size consistent");

        let sub1 = table.lookup(0xD, 1);
        assert_eq!(sub1.eax, 0x1, "XSAVEOPT only (no XSAVEC/XSAVES)");
    }

    #[test]
    fn vendor_string_correct() {
        let table = CpuidStealthTable::build(&test_config());
        let result = table.lookup(0, 0);
        let vendor: Vec<u8> = [result.ebx, result.edx, result.ecx]
            .iter()
            .flat_map(|r| r.to_le_bytes())
            .collect();
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
        // test_config(): 8 logical processors, 2 threads/core => 4 cores.
        let table = CpuidStealthTable::build(&test_config());
        // Subleaf 0: SMT. 2 threads/core needs 1 ID bit.
        let smt = table.lookup(0xB, 0);
        assert_eq!(smt.ebx, 2); // 2 threads per core
        assert_eq!(smt.eax, 1, "SMT shift = ceil(log2(2)) = 1");

        // Subleaf 1: Core. The package-level shift covers SMT(1) + core(2) bits.
        // 8 logical processors fit in exactly 3 x2APIC-ID bits, so EAX must be 3 —
        // not 4, which the old `floor(log2(4))+1` core_shift produced.
        let core = table.lookup(0xB, 1);
        assert_eq!(core.ebx, 8); // 8 total logical processors
        assert_eq!(core.eax, 3, "package shift = ceil(log2(8 logical)) = 3");
    }

    #[test]
    fn ceil_log2_does_not_overshoot_powers_of_two() {
        assert_eq!(ceil_log2(0), 0);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2); // the off-by-one case
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(8), 3);
        assert_eq!(ceil_log2(16), 4);
    }

    #[test]
    fn leaf_1_max_addressable_ids_agrees_with_leaf_0xb_package_shift() {
        // Two surfaces encode the package's APIC-ID width: leaf 1 EBX[23:16] is the
        // number of addressable logical-processor IDs (a power of two), and leaf 0xB
        // subleaf 1 EAX is the bit count to shift past them. They must satisfy
        // max_ids == 2^shift, or a guest sees two different package sizes.
        for &(vcpus, threads) in &[(8u32, 2u32), (4, 1), (4, 2), (2, 1), (1, 1), (6, 2)] {
            let cfg = CpuidStealthConfig {
                vcpu_count: vcpus,
                threads_per_core: threads,
                // HTT must be set for leaf 1 to advertise a >1 max-ID count.
                features_edx: test_config().features_edx | (1 << 28),
                ..test_config()
            };
            let table = CpuidStealthTable::build(&cfg);
            let max_ids = (table.lookup(1, 0).ebx >> 16) & 0xFF;
            let shift = table.lookup(0xB, 1).eax;
            assert_eq!(
                max_ids,
                1 << shift,
                "vcpus={vcpus} threads={threads}: leaf1 max_ids {max_ids} != 2^{shift}"
            );
        }
    }

    #[test]
    fn topology_leaf_0xb_single_thread_power_of_two_cores() {
        // 4 logical processors, no SMT => 4 cores, 0 SMT bits, 2 core bits.
        let cfg = CpuidStealthConfig {
            vcpu_count: 4,
            threads_per_core: 1,
            ..test_config()
        };
        let table = CpuidStealthTable::build(&cfg);
        let smt = table.lookup(0xB, 0);
        assert_eq!(smt.eax, 0, "no SMT => 0 shift");
        let core = table.lookup(0xB, 1);
        assert_eq!(core.eax, 2, "4 logical processors => 2 ID bits, not 3");
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

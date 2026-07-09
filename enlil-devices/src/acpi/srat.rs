//! SRAT (System Resource Affinity Table) builder
//!
//! Describes NUMA topology to the OS. Windows 11 expects SRAT even on
//! single-node systems. Contains processor affinity entries mapping each
//! vCPU to a proximity domain, and memory affinity entries describing
//! which memory ranges belong to which domain.

use super::tables::{AcpiSdtHeader, OemInfo};
use crate::truncate::u32_of;

/// Processor Local APIC Affinity structure (type 0, 16 bytes)
#[derive(Debug, Clone)]
pub struct ProcessorAffinityEntry {
    pub proximity_domain_lo: u8,
    pub apic_id: u8,
    pub flags: u32,
    pub local_sapic_eid: u8,
    pub proximity_domain_hi: [u8; 3],
    pub clock_domain: u32,
}

impl ProcessorAffinityEntry {
    /// Create entry for a vCPU in the given proximity domain
    #[must_use]
    pub fn new(apic_id: u8, proximity_domain: u32, enabled: bool) -> Self {
        Self {
            proximity_domain_lo: (proximity_domain & 0xFF) as u8,
            apic_id,
            flags: u32::from(enabled),
            local_sapic_eid: 0,
            proximity_domain_hi: [
                ((proximity_domain >> 8) & 0xFF) as u8,
                ((proximity_domain >> 16) & 0xFF) as u8,
                ((proximity_domain >> 24) & 0xFF) as u8,
            ],
            clock_domain: 0,
        }
    }

    fn to_bytes(&self) -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf[0] = 0; // type: Processor Local APIC Affinity
        buf[1] = 16; // length
        buf[2] = self.proximity_domain_lo;
        buf[3] = self.apic_id;
        buf[4..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8] = self.local_sapic_eid;
        buf[9] = self.proximity_domain_hi[0];
        buf[10] = self.proximity_domain_hi[1];
        buf[11] = self.proximity_domain_hi[2];
        buf[12..16].copy_from_slice(&self.clock_domain.to_le_bytes());
        buf
    }
}

/// Memory Affinity structure (type 1, 40 bytes)
#[derive(Debug, Clone)]
pub struct MemoryAffinityEntry {
    pub proximity_domain: u32,
    pub base_address: u64,
    pub length: u64,
    pub flags: u32,
}

impl MemoryAffinityEntry {
    /// Create entry for a memory range in the given proximity domain
    #[must_use]
    pub const fn new(
        proximity_domain: u32,
        base_address: u64,
        length: u64,
        enabled: bool,
        hot_pluggable: bool,
    ) -> Self {
        let mut flags = 0u32;
        if enabled {
            flags |= 1;
        }
        if hot_pluggable {
            flags |= 2;
        }
        Self {
            proximity_domain,
            base_address,
            length,
            flags,
        }
    }

    fn to_bytes(&self) -> [u8; 40] {
        let mut buf = [0u8; 40];
        buf[0] = 1; // type: Memory Affinity
        buf[1] = 40; // length
        buf[2..6].copy_from_slice(&self.proximity_domain.to_le_bytes());
        // reserved 2 bytes at [6..8]
        // base address low (bits 31:0)
        buf[8..12].copy_from_slice(&(u32_of(self.base_address)).to_le_bytes());
        // base address high (bits 63:32)
        buf[12..16].copy_from_slice(&((self.base_address >> 32) as u32).to_le_bytes());
        // length low (bits 31:0)
        buf[16..20].copy_from_slice(&(u32_of(self.length)).to_le_bytes());
        // length high (bits 63:32)
        buf[20..24].copy_from_slice(&((self.length >> 32) as u32).to_le_bytes());
        // reserved 4 bytes at [24..28]
        buf[28..32].copy_from_slice(&self.flags.to_le_bytes());
        // reserved 8 bytes at [32..40]
        buf
    }
}

/// SRAT table builder
pub struct SratBuilder {
    oem: OemInfo,
    processor_entries: Vec<ProcessorAffinityEntry>,
    memory_entries: Vec<MemoryAffinityEntry>,
}

impl SratBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            processor_entries: Vec::new(),
            memory_entries: Vec::new(),
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub fn add_processor(mut self, entry: ProcessorAffinityEntry) -> Self {
        self.processor_entries.push(entry);
        self
    }

    #[must_use]
    pub fn add_memory(mut self, entry: MemoryAffinityEntry) -> Self {
        self.memory_entries.push(entry);
        self
    }

    /// Build a standard single-node SRAT for N vCPUs and a given memory size
    #[must_use]
    pub fn single_node(vcpu_count: u8, memory_size: u64) -> Self {
        let mut builder = Self::new();
        for i in 0..vcpu_count {
            builder
                .processor_entries
                .push(ProcessorAffinityEntry::new(i, 0, true));
        }
        builder
            .memory_entries
            .push(MemoryAffinityEntry::new(0, 0, memory_size, true, false));
        builder
    }

    /// Build the SRAT as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let proc_size = self.processor_entries.len() * 16;
        let mem_size = self.memory_entries.len() * 40;
        // Header (36) + table_revision (4) + reserved (8) + entries
        let total_length = 36 + 4 + 8 + proc_size + mem_size;
        let mut buf = Vec::with_capacity(total_length);

        let header = AcpiSdtHeader::new(*b"SRAT", u32_of(total_length), 3, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Table Revision (offset 36, 4 bytes) — must be 1
        buf.extend_from_slice(&1u32.to_le_bytes());
        // Reserved (8 bytes)
        buf.extend_from_slice(&[0u8; 8]);

        // Processor affinity entries
        for entry in &self.processor_entries {
            buf.extend_from_slice(&entry.to_bytes());
        }

        // Memory affinity entries
        for entry in &self.memory_entries {
            buf.extend_from_slice(&entry.to_bytes());
        }

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for SratBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Walk the SRAT's affinity structures, invoking `visit(entry_type, entry)` for
/// each well-formed one (`entry` is the whole structure, `entry[0]` its type,
/// `entry[1]` its length). The SRAT header is 48 bytes (36-byte SDT header +
/// 4-byte revision + 8 reserved); each structure is a `(type, length)` pair. A
/// truncated or self-referential entry (`length < 2`, or one that runs past the
/// table) stops the walk.
fn for_each_structure(srat: &[u8], mut visit: impl FnMut(u8, &[u8])) {
    /// Offset of the first affinity structure.
    const STRUCTURES: usize = 48;
    let mut off = STRUCTURES;
    while off + 2 <= srat.len() {
        let entry_type = srat[off];
        let len = srat[off + 1] as usize;
        if len < 2 || off + len > srat.len() {
            break;
        }
        visit(entry_type, &srat[off..off + len]);
        off += len;
    }
}

/// The proximity domain of a Processor Local APIC Affinity (type 0) structure,
/// reassembled from its low byte (+2) and high three bytes (+9..+12).
fn processor_domain(entry: &[u8]) -> u32 {
    u32::from(entry[2])
        | (u32::from(entry[9]) << 8)
        | (u32::from(entry[10]) << 16)
        | (u32::from(entry[11]) << 24)
}

/// Collect the distinct NUMA proximity domains a SRAT advertises (Phase 6.3 NUMA
/// topology).
///
/// Walks the enabled Processor Local APIC Affinity (type 0) and Memory Affinity
/// (type 1) structures and returns their proximity domains, sorted and
/// deduplicated — so the length is the NUMA-node count.
#[must_use]
pub fn numa_domains(srat: &[u8]) -> Vec<u32> {
    let mut domains = Vec::new();
    for_each_structure(srat, |entry_type, entry| match entry_type {
        // Processor Local APIC Affinity: flags at +4 (bit 0 enabled).
        0 if entry.len() >= 16 => {
            let flags = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
            if flags & 1 != 0 {
                domains.push(processor_domain(entry));
            }
        }
        // Memory Affinity: proximity domain at +2 (u32), flags at +28.
        1 if entry.len() >= 40 => {
            let flags = u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]);
            if flags & 1 != 0 {
                domains.push(u32::from_le_bytes([entry[2], entry[3], entry[4], entry[5]]));
            }
        }
        _ => {}
    });
    domains.sort_unstable();
    domains.dedup();
    domains
}

/// A NUMA memory range from a SRAT Memory Affinity (type 1) structure: which
/// proximity domain owns which physical RAM span. This is the placement
/// primitive behind the north-star rule that a kernel's hot CPU+RAM working set
/// stays on one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAffinity {
    /// The proximity (NUMA) domain the range belongs to.
    pub proximity_domain: u32,
    /// Physical base address of the range.
    pub base_address: u64,
    /// Length of the range in bytes.
    pub length: u64,
    /// Whether the range is hot-pluggable (flags bit 1).
    pub hot_pluggable: bool,
    /// Whether the range is non-volatile memory (flags bit 2).
    pub non_volatile: bool,
}

/// Parse the enabled Memory Affinity (type 1) structures into their
/// `(proximity_domain, base, length)` ranges (Phase 6.3 NUMA topology).
///
/// Disabled ranges (flags bit 0 clear) are skipped. Base is at +8 (u64), length
/// at +16 (u64), flags at +28 (bit 1 hot-pluggable, bit 2 non-volatile).
#[must_use]
pub fn memory_affinities(srat: &[u8]) -> Vec<MemoryAffinity> {
    let mut out = Vec::new();
    for_each_structure(srat, |entry_type, entry| {
        if entry_type == 1 && entry.len() >= 40 {
            let flags = u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]);
            if flags & 1 == 0 {
                return; // disabled
            }
            out.push(MemoryAffinity {
                proximity_domain: u32::from_le_bytes([entry[2], entry[3], entry[4], entry[5]]),
                base_address: u64::from_le_bytes([
                    entry[8], entry[9], entry[10], entry[11], entry[12], entry[13], entry[14],
                    entry[15],
                ]),
                length: u64::from_le_bytes([
                    entry[16], entry[17], entry[18], entry[19], entry[20], entry[21], entry[22],
                    entry[23],
                ]),
                hot_pluggable: flags & 0x2 != 0,
                non_volatile: flags & 0x4 != 0,
            });
        }
    });
    out
}

/// A CPU→node binding from a Processor Local APIC Affinity (type 0) structure:
/// which APIC ID belongs to which proximity domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuAffinity {
    /// The processor's local APIC ID (+3).
    pub apic_id: u8,
    /// The proximity (NUMA) domain the processor belongs to.
    pub proximity_domain: u32,
}

/// Parse the enabled Processor Local APIC Affinity (type 0) structures into
/// their `(apic_id, proximity_domain)` bindings (Phase 6.3 NUMA topology).
///
/// Disabled processors (flags bit 0 clear) are skipped. This is the CPU side of
/// [`memory_affinities`]: together they say which cores and which RAM share a
/// node, so a guest's vCPUs and memory can be co-located.
#[must_use]
pub fn cpu_affinities(srat: &[u8]) -> Vec<CpuAffinity> {
    let mut out = Vec::new();
    for_each_structure(srat, |entry_type, entry| {
        if entry_type == 0 && entry.len() >= 16 {
            let flags = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
            if flags & 1 != 0 {
                out.push(CpuAffinity {
                    apic_id: entry[3],
                    proximity_domain: processor_domain(entry),
                });
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numa_domains_collects_distinct_enabled_proximity_domains() {
        let srat = SratBuilder::new()
            .add_processor(ProcessorAffinityEntry::new(0, 0, true)) // domain 0
            .add_processor(ProcessorAffinityEntry::new(1, 1, true)) // domain 1
            .add_processor(ProcessorAffinityEntry::new(2, 2, false)) // disabled → skipped
            .add_memory(MemoryAffinityEntry::new(
                1,
                0x1_0000_0000,
                0x1_0000_0000,
                true,
                false,
            )) // dup of 1
            .add_memory(MemoryAffinityEntry::new(
                3,
                0x2_0000_0000,
                0x1_0000_0000,
                true,
                false,
            )) // domain 3
            .build();
        // Distinct enabled domains: {0, 1, 3}.
        assert_eq!(numa_domains(&srat), vec![0, 1, 3]);
        // An empty/header-only SRAT has no domains.
        assert!(numa_domains(&SratBuilder::new().build()).is_empty());
    }

    #[test]
    fn memory_affinities_parse_enabled_ranges_with_flags() {
        let srat = SratBuilder::new()
            .add_memory(MemoryAffinityEntry::new(0, 0, 0x8000_0000, true, false))
            .add_memory(MemoryAffinityEntry::new(
                1,
                0x1_0000_0000,
                0x1_0000_0000,
                true,
                true, // hot-pluggable
            ))
            .add_memory(MemoryAffinityEntry::new(
                2,
                0x2_0000_0000,
                0x1000,
                false, // disabled → skipped
                false,
            ))
            .build();
        let ranges = memory_affinities(&srat);
        assert_eq!(ranges.len(), 2);
        assert_eq!(
            ranges[0],
            MemoryAffinity {
                proximity_domain: 0,
                base_address: 0,
                length: 0x8000_0000,
                hot_pluggable: false,
                non_volatile: false,
            }
        );
        assert_eq!(
            ranges[1],
            MemoryAffinity {
                proximity_domain: 1,
                base_address: 0x1_0000_0000,
                length: 0x1_0000_0000,
                hot_pluggable: true,
                non_volatile: false,
            }
        );
        // Header-only SRAT: no ranges.
        assert!(memory_affinities(&SratBuilder::new().build()).is_empty());
    }

    #[test]
    fn cpu_affinities_map_apic_ids_to_domains() {
        let srat = SratBuilder::new()
            .add_processor(ProcessorAffinityEntry::new(0, 0, true))
            .add_processor(ProcessorAffinityEntry::new(7, 1, true))
            .add_processor(ProcessorAffinityEntry::new(9, 2, false)) // disabled → skipped
            .build();
        let cpus = cpu_affinities(&srat);
        assert_eq!(
            cpus,
            vec![
                CpuAffinity {
                    apic_id: 0,
                    proximity_domain: 0,
                },
                CpuAffinity {
                    apic_id: 7,
                    proximity_domain: 1,
                },
            ]
        );
    }

    #[test]
    fn high_proximity_domain_bytes_are_reassembled() {
        // A domain whose value needs the high three bytes (+9..+12) exercises
        // the split-field reassembly on both the CPU and memory sides.
        let domain = 0x0201_0300;
        let srat = SratBuilder::new()
            .add_processor(ProcessorAffinityEntry::new(3, domain, true))
            .add_memory(MemoryAffinityEntry::new(
                domain, 0x4000, 0x1000, true, false,
            ))
            .build();
        assert_eq!(cpu_affinities(&srat)[0].proximity_domain, domain);
        assert_eq!(memory_affinities(&srat)[0].proximity_domain, domain);
        assert_eq!(numa_domains(&srat), vec![domain]);
    }

    #[test]
    fn srat_single_node_builds() {
        let srat = SratBuilder::single_node(4, 0x1_0000_0000).build();
        assert_eq!(&srat[0..4], b"SRAT");
        let length = u32::from_le_bytes(srat[4..8].try_into().unwrap()) as usize;
        assert_eq!(srat.len(), length);
    }

    #[test]
    fn srat_checksum() {
        let srat = SratBuilder::single_node(4, 0x1_0000_0000).build();
        let sum: u8 = srat.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn srat_contains_processor_entries() {
        let srat = SratBuilder::single_node(8, 0x1_0000_0000).build();
        // Entries start at offset 48 (36 header + 4 revision + 8 reserved)
        let mut offset = 48;
        let mut proc_count = 0u32;
        while offset < srat.len() {
            let entry_type = srat[offset];
            let entry_len = srat[offset + 1] as usize;
            if entry_type == 0 {
                proc_count += 1;
            }
            offset += entry_len;
        }
        assert_eq!(proc_count, 8);
    }

    #[test]
    fn srat_contains_memory_entry() {
        let srat = SratBuilder::single_node(2, 0x2_0000_0000).build();
        let mut offset = 48;
        let mut mem_found = false;
        while offset < srat.len() {
            let entry_type = srat[offset];
            let entry_len = srat[offset + 1] as usize;
            if entry_type == 1 {
                mem_found = true;
                // Check length field (low 32 bits at offset+16)
                let len_lo = u32::from_le_bytes(srat[offset + 16..offset + 20].try_into().unwrap());
                let len_hi = u32::from_le_bytes(srat[offset + 20..offset + 24].try_into().unwrap());
                let total = u64::from(len_lo) | (u64::from(len_hi) << 32);
                assert_eq!(total, 0x2_0000_0000);
            }
            offset += entry_len;
        }
        assert!(mem_found, "SRAT must contain memory affinity entry");
    }

    #[test]
    fn srat_revision() {
        let srat = SratBuilder::single_node(1, 0x1_0000_0000).build();
        assert_eq!(srat[8], 3); // SDT revision
        // Table revision at offset 36
        let table_rev = u32::from_le_bytes(srat[36..40].try_into().unwrap());
        assert_eq!(table_rev, 1);
    }
}

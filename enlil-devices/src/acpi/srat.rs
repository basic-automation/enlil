//! SRAT (System Resource Affinity Table) builder
//!
//! Describes NUMA topology to the OS. Windows 11 expects SRAT even on
//! single-node systems. Contains processor affinity entries mapping each
//! vCPU to a proximity domain, and memory affinity entries describing
//! which memory ranges belong to which domain.

use crate::truncate::u32_of;
use super::tables::{AcpiSdtHeader, OemInfo};

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

#[cfg(test)]
mod tests {
    use super::*;

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

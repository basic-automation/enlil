//! MADT (Multiple APIC Description Table) builder
//!
//! Defines the virtual APIC topology for the guest. Windows uses this to
//! discover processors and interrupt controllers. Must match the vCPU count
//! and APIC ID assignment.

use crate::truncate::u32_of;
use super::tables::{AcpiSdtHeader, OemInfo};

/// MADT entry types
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
pub enum MadtEntryType {
    LocalApic = 0,
    IoApic = 1,
    InterruptSourceOverride = 2,
    NmiSource = 3,
    LocalApicNmi = 4,
    LocalApicAddressOverride = 5,
    X2Apic = 9,
}

/// Local APIC entry (type 0)
#[derive(Debug, Clone)]
pub struct LocalApicEntry {
    pub acpi_processor_uid: u8,
    pub apic_id: u8,
    pub flags: u32,
}

impl LocalApicEntry {
    #[must_use]
    pub fn new(uid: u8, apic_id: u8, enabled: bool) -> Self {
        Self {
            acpi_processor_uid: uid,
            apic_id,
            flags: u32::from(enabled),
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8);
        buf.push(MadtEntryType::LocalApic as u8);
        buf.push(8); // length
        buf.push(self.acpi_processor_uid);
        buf.push(self.apic_id);
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf
    }
}

/// I/O APIC entry (type 1)
#[derive(Debug, Clone)]
pub struct IoApicEntry {
    pub io_apic_id: u8,
    pub io_apic_address: u32,
    pub global_system_interrupt_base: u32,
}

impl IoApicEntry {
    #[must_use]
    pub const fn new(id: u8, address: u32, gsi_base: u32) -> Self {
        Self {
            io_apic_id: id,
            io_apic_address: address,
            global_system_interrupt_base: gsi_base,
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.push(MadtEntryType::IoApic as u8);
        buf.push(12); // length
        buf.push(self.io_apic_id);
        buf.push(0); // reserved
        buf.extend_from_slice(&self.io_apic_address.to_le_bytes());
        buf.extend_from_slice(&self.global_system_interrupt_base.to_le_bytes());
        buf
    }
}

/// Interrupt Source Override (type 2)
#[derive(Debug, Clone)]
pub struct InterruptOverrideEntry {
    pub bus: u8,
    pub source: u8,
    pub global_system_interrupt: u32,
    pub flags: u16,
}

impl InterruptOverrideEntry {
    #[must_use]
    pub const fn new(bus: u8, source: u8, gsi: u32, flags: u16) -> Self {
        Self {
            bus,
            source,
            global_system_interrupt: gsi,
            flags,
        }
    }

    /// ISA IRQ 0 → GSI 2 override (standard for ACPI timer)
    #[must_use]
    pub const fn timer_override() -> Self {
        Self::new(0, 0, 2, 0)
    }

    /// SCI interrupt override (active low, level triggered)
    #[must_use]
    pub const fn sci_override(sci_irq: u8) -> Self {
        // Flags: active low (bit 1) | level triggered (bit 3) = 0x000D
        Self::new(0, sci_irq, sci_irq as u32, 0x000D)
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(10);
        buf.push(MadtEntryType::InterruptSourceOverride as u8);
        buf.push(10); // length
        buf.push(self.bus);
        buf.push(self.source);
        buf.extend_from_slice(&self.global_system_interrupt.to_le_bytes());
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf
    }
}

/// Local APIC NMI entry (type 4)
#[derive(Debug, Clone)]
pub struct LocalApicNmiEntry {
    pub acpi_processor_uid: u8,
    pub flags: u16,
    pub lint: u8,
}

impl LocalApicNmiEntry {
    /// NMI on LINT1 for all processors
    #[must_use]
    pub const fn all_processors_lint1() -> Self {
        Self {
            acpi_processor_uid: 0xFF,
            flags: 0,
            lint: 1,
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(6);
        buf.push(MadtEntryType::LocalApicNmi as u8);
        buf.push(6); // length
        buf.push(self.acpi_processor_uid);
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf.push(self.lint);
        buf
    }
}

/// MADT builder
pub struct MadtBuilder {
    oem: OemInfo,
    local_apic_address: u32,
    flags: u32,
    local_apics: Vec<LocalApicEntry>,
    io_apics: Vec<IoApicEntry>,
    overrides: Vec<InterruptOverrideEntry>,
    local_nmi: Vec<LocalApicNmiEntry>,
}

impl MadtBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            local_apic_address: 0xFEE0_0000,
            flags: 1, // PCAT_COMPAT
            local_apics: Vec::new(),
            io_apics: Vec::new(),
            overrides: Vec::new(),
            local_nmi: Vec::new(),
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub const fn local_apic_address(mut self, addr: u32) -> Self {
        self.local_apic_address = addr;
        self
    }

    /// Add vCPUs with sequential APIC IDs
    #[must_use]
    pub fn add_vcpus(mut self, count: u8) -> Self {
        for i in 0..count {
            self.local_apics.push(LocalApicEntry::new(i, i, true));
        }
        self
    }

    #[must_use]
    pub fn add_local_apic(mut self, entry: LocalApicEntry) -> Self {
        self.local_apics.push(entry);
        self
    }

    #[must_use]
    pub fn add_io_apic(mut self, entry: IoApicEntry) -> Self {
        self.io_apics.push(entry);
        self
    }

    #[must_use]
    pub fn add_override(mut self, entry: InterruptOverrideEntry) -> Self {
        self.overrides.push(entry);
        self
    }

    #[must_use]
    pub fn add_local_nmi(mut self, entry: LocalApicNmiEntry) -> Self {
        self.local_nmi.push(entry);
        self
    }

    /// Build a standard MADT for a guest with N vCPUs
    #[must_use]
    pub fn standard(vcpu_count: u8) -> Self {
        Self::new()
            .add_vcpus(vcpu_count)
            .add_io_apic(IoApicEntry::new(0, 0xFEC0_0000, 0))
            .add_override(InterruptOverrideEntry::timer_override())
            .add_override(InterruptOverrideEntry::sci_override(9))
            .add_local_nmi(LocalApicNmiEntry::all_processors_lint1())
    }

    /// Build the MADT as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        // Calculate total size
        let entries_size: usize = self
            .local_apics
            .iter()
            .map(|e| e.to_bytes().len())
            .sum::<usize>()
            + self
                .io_apics
                .iter()
                .map(|e| e.to_bytes().len())
                .sum::<usize>()
            + self
                .overrides
                .iter()
                .map(|e| e.to_bytes().len())
                .sum::<usize>()
            + self
                .local_nmi
                .iter()
                .map(|e| e.to_bytes().len())
                .sum::<usize>();

        let total_length = 36 + 8 + entries_size; // header + fixed fields + entries

        let mut buf = Vec::with_capacity(total_length);

        // SDT Header
        let header = AcpiSdtHeader::new(*b"APIC", u32_of(total_length), 3, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Fixed fields
        buf.extend_from_slice(&self.local_apic_address.to_le_bytes());
        buf.extend_from_slice(&self.flags.to_le_bytes());

        // Entries
        for entry in &self.local_apics {
            buf.extend_from_slice(&entry.to_bytes());
        }
        for entry in &self.io_apics {
            buf.extend_from_slice(&entry.to_bytes());
        }
        for entry in &self.overrides {
            buf.extend_from_slice(&entry.to_bytes());
        }
        for entry in &self.local_nmi {
            buf.extend_from_slice(&entry.to_bytes());
        }

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for MadtBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn madt_standard_4_vcpus() {
        let madt = MadtBuilder::standard(4).build();
        assert_eq!(&madt[0..4], b"APIC");
        // Checksum
        let sum: u8 = madt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn madt_contains_all_cpus() {
        let madt = MadtBuilder::standard(8).build();
        // Count Local APIC entries (type 0, length 8)
        let mut offset = 44; // past header + fixed fields
        let mut cpu_count = 0u32;
        while offset < madt.len() {
            let entry_type = madt[offset];
            let entry_len = madt[offset + 1] as usize;
            if entry_type == 0 {
                cpu_count += 1;
            }
            offset += entry_len;
        }
        assert_eq!(cpu_count, 8);
    }

    #[test]
    fn madt_local_apic_address() {
        let madt = MadtBuilder::new()
            .local_apic_address(0xFEE0_0000)
            .add_vcpus(1)
            .build();
        let addr = u32::from_le_bytes(madt[36..40].try_into().unwrap());
        assert_eq!(addr, 0xFEE0_0000);
    }

    #[test]
    fn madt_has_io_apic() {
        let madt = MadtBuilder::standard(2).build();
        // Search for IO APIC entry (type 1)
        let mut offset = 44;
        let mut found = false;
        while offset < madt.len() {
            if madt[offset] == 1 {
                found = true;
                let addr = u32::from_le_bytes(madt[offset + 4..offset + 8].try_into().unwrap());
                assert_eq!(addr, 0xFEC0_0000);
                break;
            }
            offset += madt[offset + 1] as usize;
        }
        assert!(found, "MADT must contain IO APIC entry");
    }

    #[test]
    fn madt_signature_and_revision() {
        let madt = MadtBuilder::standard(1).build();
        assert_eq!(&madt[0..4], b"APIC");
        assert_eq!(madt[8], 3); // revision
    }
}

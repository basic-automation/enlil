//! SMBIOS/DMI Table Generation for Guest VMs
//!
//! Generates system identification tables that Windows reads during setup.

/// SMBIOS Entry Point Structure
#[repr(C, packed)]
pub struct SmbiosEntryPoint {
    pub anchor_string: [u8; 4], // "_SM_"
    pub checksum: u8,
    pub length: u8, // Usually 31 bytes
    pub major_version: u8,
    pub minor_version: u8,
    pub max_structure_size: u16,
    pub entry_point_revision: u8,
    pub formatted_area: [u8; 5],
    pub intermediate_anchor: [u8; 5], // "_DMI_"
    pub intermediate_checksum: u8,
    pub structure_table_length: u16,
    pub structure_table_address: u32,
    pub number_of_structures: u16,
    pub bcd_revision: u8,
}

/// SMBIOS Type 0 — BIOS Information
#[repr(C, packed)]
pub struct SmbiosBiosInfo {
    pub header_type: u8, // 0
    pub length: u8,      // Typically 20 bytes
    pub handle: u16,
    pub vendor: u8, // String index
    pub version: u8,
    pub starting_address: u16,
    pub release_date: u8,
    pub rom_size: u8,
    pub characteristics: u64,
    pub ext_characteristics: [u8; 2],
    pub bios_major: u8,
    pub bios_minor: u8,
    pub ec_major: u8,
    pub ec_minor: u8,
}

/// SMBIOS Type 1 — System Information
#[repr(C, packed)]
pub struct SmbiosSystemInfo {
    pub header_type: u8, // 1
    pub length: u8,
    pub handle: u16,
    pub manufacturer: u8, // String index
    pub product_name: u8,
    pub version: u8,
    pub serial_number: u8,
    pub uuid: [u8; 16],
    pub wakeup_type: u8,
}

/// SMBIOS Type 2 — Baseboard Information
#[repr(C, packed)]
pub struct SmbiosBaseboard {
    pub header_type: u8, // 2
    pub length: u8,
    pub handle: u16,
    pub manufacturer: u8,
    pub product: u8,
    pub version: u8,
    pub serial: u8,
    pub asset_tag: u8,
    pub feature_flags: u8,
    pub location: u8,
    pub chassis_handle: u16,
}

/// SMBIOS Type 4 — Processor Information
#[repr(C, packed)]
pub struct SmbiosProcessor {
    pub header_type: u8, // 4
    pub length: u8,
    pub handle: u16,
    pub socket_designation: u8,
    pub proc_type: u8,
    pub proc_family: u8,
    pub manufacturer: u8,
    pub cpu_id: u64,
    pub version: u8,
    pub voltage: u8,
    pub external_clock_freq: u16,
    pub max_speed: u16,
    pub current_speed: u16,
    pub status: u8,
    pub upgrade: u8,
    pub l1_cache_handle: u16,
    pub l2_cache_handle: u16,
    pub l3_cache_handle: u16,
    pub serial_number: u8,
    pub asset_tag: u8,
    pub part_number: u8,
    pub core_count: u8,
    pub core_enabled: u8,
    pub thread_count: u8,
    pub characteristics: u16,
}

/// SMBIOS Type 17 — Memory Device
#[repr(C, packed)]
pub struct SmbiosMemoryDevice {
    pub header_type: u8, // 17
    pub length: u8,
    pub handle: u16,
    pub array_handle: u16,
    pub error_info_handle: u16,
    pub total_width: u16,
    pub data_width: u16,
    pub size: u16, // In MB (0x7FFF means use extended size)
    pub form_factor: u8,
    pub device_set: u8,
    pub device_locator: u8,
    pub bank_locator: u8,
    pub memory_type: u8,
    pub type_detail: u16,
    pub speed: u16, // MHz
    pub manufacturer: u8,
    pub serial_number: u8,
    pub asset_tag: u8,
    pub part_number: u8,
    pub attributes: u8,      // Rank
    pub extended_size: u32,  // In MB, if size == 0x7FFF
    pub speed_extended: u16, // MHz, for ACPI 3.2+
}

/// SMBIOS table generator
// Fields hold SMBIOS string/identity values captured at construction; the
// table emitters that read them are part of the in-progress Phase 5 synthesis.
#[allow(dead_code)]
pub struct SmbiosGenerator {
    manufacturer: String,
    product_name: String,
    serial_number: String,
    bios_vendor: String,
    cpu_model_name: String,
    total_memory_mb: u64,
}

impl SmbiosGenerator {
    pub fn new(
        manufacturer: String,
        product_name: String,
        serial_number: String,
        bios_vendor: String,
        cpu_model_name: String,
        total_memory_mb: u64,
    ) -> Self {
        Self {
            manufacturer,
            product_name,
            serial_number,
            bios_vendor,
            cpu_model_name,
            total_memory_mb,
        }
    }

    /// Create default SMBIOS matching realistic hardware
    pub fn default_intel_system() -> Self {
        Self::new(
            "Intel Corporation".to_string(),
            "Enlil Virtual Platform".to_string(),
            "ENLIL0123456789".to_string(),
            "AMI".to_string(),
            "Intel(R) Core(TM) i7-12700K CPU @ 3.60GHz".to_string(),
            16384, // 16 GB
        )
    }

    pub fn default_amd_system() -> Self {
        Self::new(
            "AMD".to_string(),
            "Enlil Virtual Platform".to_string(),
            "ENLIL0123456789".to_string(),
            "AMI".to_string(),
            "AMD Ryzen 7 5800X3D".to_string(),
            16384,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_smbios_generator() {
        let gen = SmbiosGenerator::default_intel_system();
        assert_eq!(gen.total_memory_mb, 16384);
    }
}

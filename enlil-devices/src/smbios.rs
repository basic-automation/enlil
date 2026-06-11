//! SMBIOS/DMI table synthesis
//!
//! Generates realistic SMBIOS tables that report plausible hardware identity.
//! Windows reads these extensively during setup and activation. The tables
//! must look like they came from a real motherboard vendor.

use crate::truncate::{u16_of, u32_of};
/// SMBIOS entry point versions
const SMBIOS_MAJOR: u8 = 3;
const SMBIOS_MINOR: u8 = 4;

/// SMBIOS table types
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmbiosType {
    BiosInformation = 0,
    SystemInformation = 1,
    BaseboardInformation = 2,
    SystemEnclosure = 3,
    ProcessorInformation = 4,
    CacheInformation = 7,
    SystemSlots = 9,
    PhysicalMemoryArray = 16,
    MemoryDevice = 17,
    MemoryArrayMappedAddress = 19,
    SystemBoot = 32,
    EndOfTable = 127,
}

/// Configuration for SMBIOS table generation
#[derive(Debug, Clone)]
pub struct SmbiosConfig {
    /// BIOS vendor (e.g., "American Megatrends International, LLC.")
    pub bios_vendor: String,
    /// BIOS version (e.g., "1601")
    pub bios_version: String,
    /// BIOS release date (e.g., "11/12/2023")
    pub bios_date: String,
    /// System manufacturer (e.g., "ASUS")
    pub system_manufacturer: String,
    /// System product name (e.g., "ROG STRIX B650E-E GAMING WIFI")
    pub system_product: String,
    /// System version
    pub system_version: String,
    /// System serial number
    pub system_serial: String,
    /// System UUID (16 bytes)
    pub system_uuid: [u8; 16],
    /// System SKU
    pub system_sku: String,
    /// System family
    pub system_family: String,
    /// Baseboard manufacturer
    pub baseboard_manufacturer: String,
    /// Baseboard product
    pub baseboard_product: String,
    /// Baseboard serial
    pub baseboard_serial: String,
    /// Processor brand string (from CPUID leaf 0x80000002-4)
    pub cpu_brand: String,
    /// Number of CPU cores
    pub cpu_cores: u8,
    /// Number of CPU threads
    pub cpu_threads: u8,
    /// CPU max speed MHz
    pub cpu_max_speed: u16,
    /// Total RAM in MB
    pub total_ram_mb: u32,
    /// RAM modules (`size_mb`, `speed_mhz`, manufacturer, `part_number`)
    pub ram_modules: Vec<RamModule>,
}

/// A physical RAM module
#[derive(Debug, Clone)]
pub struct RamModule {
    pub size_mb: u32,
    pub speed_mhz: u16,
    pub manufacturer: String,
    pub part_number: String,
    pub serial: String,
}

impl Default for SmbiosConfig {
    fn default() -> Self {
        Self {
            bios_vendor: "American Megatrends International, LLC.".to_string(),
            bios_version: "1601".to_string(),
            bios_date: "11/12/2023".to_string(),
            system_manufacturer: "ASUS".to_string(),
            system_product: "System Product Name".to_string(),
            system_version: "System Version".to_string(),
            system_serial: "System Serial Number".to_string(),
            system_uuid: [
                0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
                0x77, 0x88,
            ],
            system_sku: "SKU".to_string(),
            system_family: "To be filled by O.E.M.".to_string(),
            baseboard_manufacturer: "ASUSTeK COMPUTER INC.".to_string(),
            baseboard_product: "ROG STRIX B650E-E GAMING WIFI".to_string(),
            baseboard_serial: "000000000000".to_string(),
            cpu_brand: "AMD Ryzen 9 7950X 16-Core Processor".to_string(),
            cpu_cores: 16,
            cpu_threads: 32,
            cpu_max_speed: 5700,
            total_ram_mb: 32768,
            ram_modules: vec![
                RamModule {
                    size_mb: 16384,
                    speed_mhz: 6000,
                    manufacturer: "G Skill Intl".to_string(),
                    part_number: "F5-6000J3636F16G".to_string(),
                    serial: "00000001".to_string(),
                },
                RamModule {
                    size_mb: 16384,
                    speed_mhz: 6000,
                    manufacturer: "G Skill Intl".to_string(),
                    part_number: "F5-6000J3636F16G".to_string(),
                    serial: "00000002".to_string(),
                },
            ],
        }
    }
}

impl SmbiosConfig {
    /// Build an SMBIOS config **from this host machine's own identity**: the
    /// DMI strings the firmware wrote (read from `/sys/class/dmi/id`) and the
    /// CPU brand string from the host's CPUID — so the guest sees the
    /// physical machine's vendor/board/BIOS identity, which is what Windows
    /// activation checks and what transparency requires (ROADMAP §5.2).
    ///
    /// `cpu_cores`/`cpu_threads` describe the **guest** topology (they must
    /// agree with what CPUID leaf 1/0xB advertises to the guest, not with the
    /// host's full core count). Every field the host doesn't expose — DMI
    /// sysfs absent (containers), root-only serial/UUID files — falls back to
    /// the default profile's value, field by field.
    #[must_use]
    pub fn from_host(cpu_cores: u8, cpu_threads: u8) -> Self {
        let mut config = Self::from_dmi_dir(std::path::Path::new("/sys/class/dmi/id"));
        config.cpu_cores = cpu_cores;
        config.cpu_threads = cpu_threads;
        #[cfg(target_arch = "x86_64")]
        if let Some(brand) = host_cpu_brand() {
            config.cpu_brand = brand;
        }
        config
    }

    /// Read the DMI identity strings from `dir` (the layout of Linux's
    /// `/sys/class/dmi/id`), falling back to the default profile per missing
    /// or unreadable file.
    fn from_dmi_dir(dir: &std::path::Path) -> Self {
        let mut config = Self::default();
        let read = |name: &str| -> Option<String> {
            let s = std::fs::read_to_string(dir.join(name)).ok()?;
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        };
        for (field, file) in [
            (&mut config.bios_vendor, "bios_vendor"),
            (&mut config.bios_version, "bios_version"),
            (&mut config.bios_date, "bios_date"),
            (&mut config.system_manufacturer, "sys_vendor"),
            (&mut config.system_product, "product_name"),
            (&mut config.system_version, "product_version"),
            (&mut config.system_serial, "product_serial"),
            (&mut config.system_sku, "product_sku"),
            (&mut config.system_family, "product_family"),
            (&mut config.baseboard_manufacturer, "board_vendor"),
            (&mut config.baseboard_product, "board_name"),
            (&mut config.baseboard_serial, "board_serial"),
        ] {
            if let Some(value) = read(file) {
                *field = value;
            }
        }
        // product_uuid ("8-4-4-4-12" hex): root-only on most systems, so this
        // usually falls back to the default.
        if let Some(uuid) = read("product_uuid").and_then(|u| parse_dmi_uuid(&u)) {
            config.system_uuid = uuid;
        }
        config
    }
}

/// Parse a DMI UUID string (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`) into its
/// 16 raw bytes (in string order, as the sysfs file presents it).
fn parse_dmi_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(char::is_ascii_alphanumeric).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// The host CPU's brand string from CPUID `0x80000002-4`, trimmed of the
/// leading-space padding real parts carry. `None` if unenumerated (no
/// `x86_64` part ships without it) or empty.
#[cfg(target_arch = "x86_64")]
fn host_cpu_brand() -> Option<String> {
    use core::arch::x86_64::__cpuid;
    if __cpuid(0x8000_0000).eax < 0x8000_0004 {
        return None;
    }
    let mut bytes = Vec::with_capacity(48);
    for i in 0..3u32 {
        let r = __cpuid(0x8000_0002 + i);
        for reg in [r.eax, r.ebx, r.ecx, r.edx] {
            bytes.extend_from_slice(&reg.to_le_bytes());
        }
    }
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let brand = String::from_utf8_lossy(&bytes[..end]).trim().to_string();
    if brand.is_empty() { None } else { Some(brand) }
}

/// SMBIOS table builder
pub struct SmbiosBuilder {
    config: SmbiosConfig,
}

impl SmbiosBuilder {
    #[must_use]
    pub const fn new(config: SmbiosConfig) -> Self {
        Self { config }
    }

    /// Build all SMBIOS structures as a flat byte vector.
    /// This is the structure table data that goes after the entry point.
    #[must_use]
    pub fn build_structures(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(4096);

        // Type 0: BIOS Information
        data.extend_from_slice(&self.build_type0());
        // Type 1: System Information
        data.extend_from_slice(&self.build_type1());
        // Type 2: Baseboard Information
        data.extend_from_slice(&self.build_type2());
        // Type 3: System Enclosure
        data.extend_from_slice(&Self::build_type3());
        // Type 4: Processor Information
        data.extend_from_slice(&self.build_type4());
        // Type 16: Physical Memory Array
        data.extend_from_slice(&self.build_type16());
        // Type 17: Memory Device (one per module)
        for (i, module) in self.config.ram_modules.iter().enumerate() {
            data.extend_from_slice(&Self::build_type17(i, module));
        }
        // Type 32: System Boot Information
        data.extend_from_slice(&Self::build_type32());
        // Type 127: End-of-Table
        data.extend_from_slice(&Self::build_type127());

        data
    }

    /// Build the SMBIOS 3.0 64-bit Entry Point structure
    #[must_use]
    pub fn build_entry_point(&self, structure_table_address: u64) -> Vec<u8> {
        let structures = self.build_structures();
        let structure_table_length = u32_of(structures.len());

        let mut buf = Vec::with_capacity(24);
        // Anchor string
        buf.extend_from_slice(b"_SM3_");
        // Entry point checksum (fixed up later)
        buf.push(0);
        // Entry point length
        buf.push(24);
        // Major version
        buf.push(SMBIOS_MAJOR);
        // Minor version
        buf.push(SMBIOS_MINOR);
        // Docrev
        buf.push(0);
        // Entry point revision
        buf.push(1); // 3.0
        // Reserved
        buf.push(0);
        // Structure table maximum size
        buf.extend_from_slice(&structure_table_length.to_le_bytes());
        // Structure table address
        buf.extend_from_slice(&structure_table_address.to_le_bytes());

        // Fix checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[5] = buf[5].wrapping_sub(sum);

        buf
    }

    // ---- Individual type builders ----

    fn build_type0(&self) -> Vec<u8> {
        let mut header = vec![
            0,  // Type 0
            26, // Length (formatted area)
            0, 0, // Handle
        ];
        // Vendor string ref
        header.push(1);
        // BIOS Version string ref
        header.push(2);
        // BIOS Starting Address Segment
        header.extend_from_slice(&0xE800u16.to_le_bytes());
        // BIOS Release Date string ref
        header.push(3);
        // BIOS ROM Size (64K blocks - 1)
        header.push(0xFF); // 16MB
        // BIOS Characteristics (8 bytes)
        header.extend_from_slice(&0x0000_0003_0000_0000u64.to_le_bytes());
        // BIOS Characteristics Extension Bytes (SMBIOS §7.1.2.2).
        header.push(0x01); // byte 1: ACPI supported (bit 0)
        // byte 2: targeted content distribution (bit 2) + UEFI (bit 3). Bit 4 —
        // "SMBIOS table describes a virtual machine" — MUST stay clear: it is a
        // direct VM tell (dmidecode prints "System is a virtual machine"), exactly
        // what a transparent hypervisor must not advertise. Was 0x1C (bit 4 set).
        header.push(0x0C);
        // System BIOS Major Release
        header.push(5);
        // System BIOS Minor Release
        header.push(29);
        // EC Firmware Major Release
        header.push(0xFF);
        // EC Firmware Minor Release
        header.push(0xFF);
        // Extended BIOS ROM Size
        header.extend_from_slice(&32u16.to_le_bytes()); // 32 MB

        // Strings section
        append_strings(
            &mut header,
            &[
                &self.config.bios_vendor,
                &self.config.bios_version,
                &self.config.bios_date,
            ],
        );

        header
    }

    fn build_type1(&self) -> Vec<u8> {
        let mut header = vec![
            1,  // Type 1
            27, // Length
            1, 0, // Handle
        ];
        // Manufacturer string ref
        header.push(1);
        // Product Name string ref
        header.push(2);
        // Version string ref
        header.push(3);
        // Serial Number string ref
        header.push(4);
        // UUID (16 bytes)
        header.extend_from_slice(&self.config.system_uuid);
        // Wake-up Type: Power Switch
        header.push(6);
        // SKU Number string ref
        header.push(5);
        // Family string ref
        header.push(6);

        append_strings(
            &mut header,
            &[
                &self.config.system_manufacturer,
                &self.config.system_product,
                &self.config.system_version,
                &self.config.system_serial,
                &self.config.system_sku,
                &self.config.system_family,
            ],
        );

        header
    }

    fn build_type2(&self) -> Vec<u8> {
        let mut header = vec![
            2,  // Type 2
            15, // Length
            2, 0, // Handle
        ];
        // Manufacturer
        header.push(1);
        // Product
        header.push(2);
        // Version
        header.push(3);
        // Serial Number
        header.push(4);
        // Asset Tag
        header.push(5);
        // Feature Flags
        header.push(0x09); // Hosting board, replaceable
        // Location in Chassis
        header.push(6);
        // Chassis Handle
        header.extend_from_slice(&3u16.to_le_bytes()); // Type 3 handle
        // Board Type: Motherboard
        header.push(0x0A);

        append_strings(
            &mut header,
            &[
                &self.config.baseboard_manufacturer,
                &self.config.baseboard_product,
                "Rev X.0x",
                &self.config.baseboard_serial,
                "Default string",
                "Default string",
            ],
        );

        header
    }

    fn build_type3() -> Vec<u8> {
        let mut header = vec![
            3,  // Type 3
            22, // Length
            3, 0, // Handle
        ];
        // Manufacturer
        header.push(1);
        // Type: Desktop
        header.push(3);
        // Version
        header.push(2);
        // Serial Number
        header.push(3);
        // Asset Tag
        header.push(4);
        // Boot-up State: Safe
        header.push(3);
        // Power Supply State: Safe
        header.push(3);
        // Thermal State: Safe
        header.push(3);
        // Security Status: None
        header.push(2);
        // OEM-defined (4 bytes)
        header.extend_from_slice(&0u32.to_le_bytes());
        // Height: 0 (unspecified)
        header.push(0);
        // Number of Power Cords: 1
        header.push(1);
        // Contained Element Count: 0
        header.push(0);
        // Contained Element Record Length: 0
        header.push(0);
        // SKU Number (string ref, SMBIOS 2.7+). The Length field (22) accounts for
        // this byte; omitting it left the formatted area one byte short, so a real
        // parser (dmidecode) consumed the first string's leading byte as the SKU
        // index — yielding "efault string" for the manufacturer and a <BAD INDEX>
        // SKU. Reference the 5th string below.
        header.push(5);

        append_strings(
            &mut header,
            &[
                "Default string",
                "Default string",
                "Default string",
                "Default string",
                "Default string",
            ],
        );

        header
    }

    fn build_type4(&self) -> Vec<u8> {
        let mut header = vec![
            4,  // Type 4
            48, // Length (SMBIOS 3.0)
            4, 0, // Handle
        ];
        // Socket Designation
        header.push(1);
        // Processor Type: Central Processor
        header.push(3);
        // Processor Family: use 0xFE for family2
        header.push(0xFE);
        // Processor Manufacturer
        header.push(2);
        // Processor ID (8 bytes - CPUID signature)
        header.extend_from_slice(&0u64.to_le_bytes());
        // Processor Version
        header.push(3);
        // Voltage: 1.1V
        header.push(0x8B);
        // External Clock (MHz)
        header.extend_from_slice(&100u16.to_le_bytes());
        // Max Speed
        header.extend_from_slice(&self.config.cpu_max_speed.to_le_bytes());
        // Current Speed
        header.extend_from_slice(&self.config.cpu_max_speed.to_le_bytes());
        // Status: Populated, Enabled
        header.push(0x41);
        // Processor Upgrade: AM5
        header.push(0x3E);
        // L1 Cache Handle
        header.extend_from_slice(&0xFFFFu16.to_le_bytes());
        // L2 Cache Handle
        header.extend_from_slice(&0xFFFFu16.to_le_bytes());
        // L3 Cache Handle
        header.extend_from_slice(&0xFFFFu16.to_le_bytes());
        // Serial Number
        header.push(4);
        // Asset Tag
        header.push(5);
        // Part Number
        header.push(6);
        // Core Count
        header.push(self.config.cpu_cores);
        // Core Enabled
        header.push(self.config.cpu_cores);
        // Thread Count
        header.push(self.config.cpu_threads);
        // Processor Characteristics
        header.extend_from_slice(&0x00FCu16.to_le_bytes());
        // Processor Family 2
        header.extend_from_slice(&0x0108u16.to_le_bytes()); // Zen 4
        // SMBIOS 3.0 adds the 16-bit core/thread counts (offsets 0x2A-0x2F). The
        // Length field (48) accounts for these 6 bytes; omitting them left the
        // formatted area at 42 bytes, so a real parser read 6 bytes of the string
        // section as fields and then mis-aligned the whole string table — the
        // processor Manufacturer came out as the CPU brand and the Part Number as
        // <BAD INDEX>. Mirror the 8-bit counts above.
        header.extend_from_slice(&u16::from(self.config.cpu_cores).to_le_bytes()); // Core Count 2
        header.extend_from_slice(&u16::from(self.config.cpu_cores).to_le_bytes()); // Core Enabled 2
        header.extend_from_slice(&u16::from(self.config.cpu_threads).to_le_bytes()); // Thread Count 2

        append_strings(
            &mut header,
            &[
                "AM5",
                "Advanced Micro Devices, Inc.",
                &self.config.cpu_brand,
                "Unknown",
                "Unknown",
                "Unknown",
            ],
        );

        header
    }

    fn build_type16(&self) -> Vec<u8> {
        let mut header = vec![
            16, // Type 16
            23, // Length
            16, 0, // Handle
        ];
        // Location: System Board
        header.push(3);
        // Use: System Memory
        header.push(3);
        // Error Correction: None
        header.push(3);
        // Maximum Capacity (KB) — use 0x8000_0000 for extended
        header.extend_from_slice(&0x8000_0000u32.to_le_bytes());
        // Error Info Handle: Not Provided
        header.extend_from_slice(&0xFFFEu16.to_le_bytes());
        // Number of Memory Devices
        let num_devices = u16_of(self.config.ram_modules.len());
        header.extend_from_slice(&num_devices.to_le_bytes());
        // Extended Maximum Capacity (bytes)
        header
            .extend_from_slice(&(u64::from(self.config.total_ram_mb) * 1024 * 1024).to_le_bytes());

        // No strings
        header.push(0);
        header.push(0);

        header
    }

    fn build_type17(index: usize, module: &RamModule) -> Vec<u8> {
        let handle = u16_of(17 + index);
        let mut header = vec![
            17, // Type 17
            92, // Length (SMBIOS 3.3)
        ];
        header.extend_from_slice(&handle.to_le_bytes());
        // Physical Memory Array Handle
        header.extend_from_slice(&16u16.to_le_bytes());
        // Error Info Handle: Not Provided
        header.extend_from_slice(&0xFFFEu16.to_le_bytes());
        // Total Width (bits)
        header.extend_from_slice(&64u16.to_le_bytes());
        // Data Width (bits)
        header.extend_from_slice(&64u16.to_le_bytes());
        // Size (MB) — 0x7FFF means use Extended Size
        if module.size_mb > 0x7FFF {
            header.extend_from_slice(&0x7FFFu16.to_le_bytes());
        } else {
            header.extend_from_slice(&(u16_of(module.size_mb)).to_le_bytes());
        }
        // Form Factor: DIMM
        header.push(9);
        // Device Set: None
        header.push(0);
        // Device Locator
        header.push(1);
        // Bank Locator
        header.push(2);
        // Memory Type: DDR5
        header.push(0x22);
        // Type Detail
        header.extend_from_slice(&0x0080u16.to_le_bytes()); // Synchronous
        // Speed (MT/s)
        header.extend_from_slice(&module.speed_mhz.to_le_bytes());
        // Manufacturer
        header.push(3);
        // Serial Number
        header.push(4);
        // Asset Tag
        header.push(5);
        // Part Number
        header.push(6);
        // Attributes (rank)
        header.push(1);
        // Extended Size (MB)
        header.extend_from_slice(&module.size_mb.to_le_bytes());
        // Configured Speed
        header.extend_from_slice(&module.speed_mhz.to_le_bytes());
        // Min Voltage (mV)
        header.extend_from_slice(&1100u16.to_le_bytes());
        // Max Voltage (mV)
        header.extend_from_slice(&1100u16.to_le_bytes());
        // Configured Voltage (mV)
        header.extend_from_slice(&1100u16.to_le_bytes());
        // Memory Technology: DRAM
        header.push(2);
        // Operating Mode Capability
        header.extend_from_slice(&0x0004u16.to_le_bytes()); // Volatile
        // Firmware Version
        header.push(0);
        // Module Manufacturer ID
        header.extend_from_slice(&0u16.to_le_bytes());
        // Module Product ID
        header.extend_from_slice(&0u16.to_le_bytes());
        // Memory Subsystem Controller Manufacturer ID
        header.extend_from_slice(&0u16.to_le_bytes());
        // Memory Subsystem Controller Product ID
        header.extend_from_slice(&0u16.to_le_bytes());
        // Non-volatile Size
        header.extend_from_slice(&0u64.to_le_bytes());
        // Volatile Size
        header.extend_from_slice(&(u64::from(module.size_mb) * 1024 * 1024).to_le_bytes());
        // Cache Size
        header.extend_from_slice(&0u64.to_le_bytes());
        // Logical Size
        header.extend_from_slice(&0u64.to_le_bytes());

        // Pad to declared length
        while header.len() < 92 {
            header.push(0);
        }

        let dimm_label = format!("DIMM_{index}");
        let bank_label = format!("BANK {index}");
        append_strings(
            &mut header,
            &[
                &dimm_label,
                &bank_label,
                &module.manufacturer,
                &module.serial,
                "Not Specified",
                &module.part_number,
            ],
        );

        header
    }

    fn build_type32() -> Vec<u8> {
        let mut header = vec![
            32, // Type 32
            20, // Length
            32, 0, // Handle
        ];
        // Reserved (6 bytes)
        header.extend_from_slice(&[0u8; 6]);
        // Boot Status: No error
        header.extend_from_slice(&[0u8; 10]);

        // No strings
        header.push(0);
        header.push(0);

        header
    }

    fn build_type127() -> Vec<u8> {
        vec![
            127, // Type 127
            4,   // Length
            127, 0, // Handle
            0, 0, // End of strings
        ]
    }
}

/// Append null-terminated strings and double-null terminator
fn append_strings(buf: &mut Vec<u8>, strings: &[&str]) {
    for s in strings {
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
    }
    buf.push(0); // double-null terminator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smbios_builds() {
        let builder = SmbiosBuilder::new(SmbiosConfig::default());
        let structures = builder.build_structures();
        assert!(!structures.is_empty());
    }

    #[test]
    fn smbios_entry_point() {
        let builder = SmbiosBuilder::new(SmbiosConfig::default());
        let ep = builder.build_entry_point(0xF0000);
        assert_eq!(&ep[0..5], b"_SM3_");
        assert_eq!(ep.len(), 24);
        // Checksum
        let sum: u8 = ep.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn smbios_contains_system_info() {
        let builder = SmbiosBuilder::new(SmbiosConfig::default());
        let data = builder.build_structures();
        // Should contain "ASUS" somewhere
        let found = data.windows(4).any(|w| w == b"ASUS");
        assert!(found, "SMBIOS should contain system manufacturer");
    }

    #[test]
    fn smbios_contains_cpu_brand() {
        let config = SmbiosConfig {
            cpu_brand: "Intel Core i9-14900K".to_string(),
            ..SmbiosConfig::default()
        };
        let builder = SmbiosBuilder::new(config);
        let data = builder.build_structures();
        let found = data.windows(20).any(|w| w == b"Intel Core i9-14900K");
        assert!(found, "SMBIOS should contain CPU brand string");
    }

    #[test]
    fn smbios_ends_with_type127() {
        let builder = SmbiosBuilder::new(SmbiosConfig::default());
        let data = builder.build_structures();
        // Last structure should be type 127
        // Find the last type 127 header
        let mut found = false;
        let mut i = 0;
        while i < data.len() {
            if data[i] == 127 {
                found = true;
            }
            // Skip structure
            let len = data[i + 1] as usize;
            i += len;
            // Skip strings
            while i < data.len() - 1 {
                if data[i] == 0 && data[i + 1] == 0 {
                    i += 2;
                    break;
                }
                i += 1;
            }
        }
        assert!(found, "SMBIOS must end with Type 127");
    }

    /// Walk the structure table, returning for each structure its
    /// `(type, declared_len, strings)` where `strings` are decoded using the
    /// declared formatted-area `Length`. If `Length` overshoots or undershoots the
    /// real formatted area (the Type 3 / Type 4 bug class), the strings parsed here
    /// come out shifted/garbled — exactly how a real DMI parser (dmidecode) mis-reads
    /// the table — so asserting the expected strings catches the defect.
    fn walk(data: &[u8]) -> Vec<(u8, usize, Vec<String>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 1 < data.len() {
            let ty = data[i];
            let len = data[i + 1] as usize;
            let mut strings = Vec::new();
            let mut j = i + len; // strings start right after the formatted area
            // String set: NUL-terminated strings until a double NUL (or a single
            // NUL immediately, meaning "no strings").
            if j < data.len() && data[j] == 0 {
                j += 2; // empty string set: just the terminating double NUL
            } else {
                while j < data.len() {
                    let start = j;
                    while j < data.len() && data[j] != 0 {
                        j += 1;
                    }
                    strings.push(String::from_utf8_lossy(&data[start..j]).into_owned());
                    j += 1; // skip the NUL
                    if j < data.len() && data[j] == 0 {
                        j += 1; // skip the second NUL ending the set
                        break;
                    }
                }
            }
            out.push((ty, len, strings));
            if ty == 127 {
                break;
            }
            i = j;
        }
        out
    }

    #[test]
    fn smbios_type0_does_not_advertise_a_virtual_machine() {
        // BIOS Characteristics Extension Byte 2 (offset 19 in the Type 0 struct)
        // bit 4 = "SMBIOS table describes a virtual machine" — a VM tell that must
        // stay clear for a transparent hypervisor.
        let data = SmbiosBuilder::new(SmbiosConfig::default()).build_structures();
        // Type 0 is first.
        assert_eq!(data[0], 0, "Type 0 leads the table");
        assert_eq!(
            data[19] & 0x10,
            0,
            "the BIOS must not set the 'virtual machine' characteristic bit"
        );
    }

    #[test]
    fn smbios_structure_lengths_match_their_formatted_area() {
        // For each structure, decode its strings using the declared Length. A wrong
        // Length shifts the string table; checking known strings parse correctly is
        // the same end-to-end check dmidecode performs.
        let data = SmbiosBuilder::new(SmbiosConfig::default()).build_structures();
        let structs = walk(&data);

        // Type 3 (System Enclosure): first string is the manufacturer "Default
        // string" — a one-byte-short formatted area used to yield "efault string".
        let t3 = structs
            .iter()
            .find(|(t, ..)| *t == 3)
            .expect("Type 3 present");
        assert_eq!(t3.1, 22, "Type 3 Length is 22 (includes the SKU byte)");
        assert_eq!(
            t3.2.first().map(String::as_str),
            Some("Default string"),
            "Type 3 strings must not be shifted (the missing SKU byte ate the 'D')"
        );

        // Type 4 (Processor): the formatted area is the full 48-byte SMBIOS 3.0
        // layout, so string #2 is the manufacturer "Advanced Micro Devices, Inc.",
        // not the CPU brand (the symptom of the 6-byte-short formatted area).
        let t4 = structs
            .iter()
            .find(|(t, ..)| *t == 4)
            .expect("Type 4 present");
        assert_eq!(t4.1, 48, "Type 4 Length is the full 3.0 size");
        assert_eq!(
            t4.2.get(1).map(String::as_str),
            Some("Advanced Micro Devices, Inc."),
            "Type 4 string table must not be shifted by a short formatted area"
        );
    }

    #[test]
    fn from_dmi_dir_captures_present_fields_and_defaults_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("board_vendor"),
            "Micro-Star INT'L CO.,LTD\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("bios_version"), "  1.80  \n").unwrap();
        std::fs::write(
            dir.path().join("product_uuid"),
            "00112233-4455-6677-8899-aabbccddeeff\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("product_serial"), "\n").unwrap(); // empty → default

        let cfg = SmbiosConfig::from_dmi_dir(dir.path());
        let default = SmbiosConfig::default();
        // Present files are captured, trimmed of sysfs whitespace.
        assert_eq!(cfg.baseboard_manufacturer, "Micro-Star INT'L CO.,LTD");
        assert_eq!(cfg.bios_version, "1.80");
        assert_eq!(
            cfg.system_uuid,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF
            ]
        );
        // Absent or empty files keep the default profile's values.
        assert_eq!(cfg.system_manufacturer, default.system_manufacturer);
        assert_eq!(cfg.system_serial, default.system_serial);
    }

    #[test]
    fn from_dmi_dir_without_dmi_is_the_default_profile() {
        let cfg = SmbiosConfig::from_dmi_dir(std::path::Path::new("/nonexistent/dmi"));
        let default = SmbiosConfig::default();
        assert_eq!(cfg.baseboard_manufacturer, default.baseboard_manufacturer);
        assert_eq!(cfg.bios_vendor, default.bios_vendor);
        assert_eq!(cfg.system_uuid, default.system_uuid);
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn from_host_takes_the_guest_topology_and_the_host_cpu_brand() {
        let cfg = SmbiosConfig::from_host(8, 16);
        assert_eq!(cfg.cpu_cores, 8);
        assert_eq!(cfg.cpu_threads, 16);
        // Every x86_64 part enumerates a brand string; the tables it feeds
        // must never carry an empty CPU name.
        assert!(!cfg.cpu_brand.is_empty());
    }

    #[test]
    fn dmi_uuid_parser_rejects_malformed_input() {
        assert!(parse_dmi_uuid("not-a-uuid").is_none());
        assert!(parse_dmi_uuid("").is_none());
        assert!(parse_dmi_uuid("00112233-4455-6677-8899-aabbccddee").is_none()); // short
        assert_eq!(
            parse_dmi_uuid("00000000-0000-0000-0000-000000000001"),
            Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1])
        );
    }
}

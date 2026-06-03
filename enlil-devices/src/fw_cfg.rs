//! QEMU `fw_cfg` device emulation
//!
//! Provides firmware configuration data to OVMF. Used to pass ACPI tables,
//! SMBIOS tables, kernel images, and other boot-time data to the guest
//! firmware without requiring a full block device.
//!
//! Protocol: port I/O at 0x510 (selector) and 0x511 (data), or MMIO via DMA.

use crate::truncate::u32_of;
/// `fw_cfg` I/O port addresses
pub const FW_CFG_PORT_SEL: u16 = 0x0510;
pub const FW_CFG_PORT_DATA: u16 = 0x0511;
pub const FW_CFG_PORT_DMA: u16 = 0x0514;

/// `fw_cfg` item selectors
pub mod selector {
    pub const SIGNATURE: u16 = 0x0000;
    pub const ID: u16 = 0x0001;
    pub const FILE_DIR: u16 = 0x0019;
    pub const ACPI_TABLES: u16 = 0x8000; // Custom, file-based
    pub const SMBIOS_ANCHOR: u16 = 0x8001;
    pub const SMBIOS_TABLES: u16 = 0x8002;
}

/// A named `fw_cfg` file entry
#[derive(Debug, Clone)]
pub struct FwCfgFile {
    /// File name (up to 55 bytes, null-terminated)
    pub name: String,
    /// File data
    pub data: Vec<u8>,
    /// Selector index
    pub selector: u16,
}

/// `fw_cfg` device state
#[derive(Debug, Clone)]
pub struct FwCfgDevice {
    /// Registered files
    files: Vec<FwCfgFile>,
    /// Currently selected item
    current_selector: u16,
    /// Read offset within current item
    read_offset: usize,
    /// Next available file selector
    next_selector: u16,
}

impl FwCfgDevice {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            files: Vec::new(),
            current_selector: 0,
            read_offset: 0,
            next_selector: 0x0020, // First file selector
        }
    }

    /// Add a named file to the `fw_cfg` device
    pub fn add_file(&mut self, name: &str, data: Vec<u8>) -> u16 {
        let sel = self.next_selector;
        self.files.push(FwCfgFile {
            name: name.to_string(),
            data,
            selector: sel,
        });
        self.next_selector += 1;
        sel
    }

    /// Add ACPI tables
    pub fn add_acpi_tables(&mut self, rsdp: Vec<u8>, tables: Vec<u8>) {
        self.add_file("etc/acpi/rsdp", rsdp);
        self.add_file("etc/acpi/tables", tables);
    }

    /// Add SMBIOS tables
    pub fn add_smbios(&mut self, anchor: Vec<u8>, tables: Vec<u8>) {
        self.add_file("etc/smbios/smbios-anchor", anchor);
        self.add_file("etc/smbios/smbios-tables", tables);
    }

    /// Handle port I/O write to selector port (0x510)
    pub const fn write_selector(&mut self, value: u16) {
        self.current_selector = value;
        self.read_offset = 0;
    }

    /// Handle port I/O read from data port (0x511), one byte at a time
    #[must_use]
    pub fn read_data(&mut self) -> u8 {
        let data = self.get_current_data();
        if self.read_offset < data.len() {
            let byte = data[self.read_offset];
            self.read_offset += 1;
            byte
        } else {
            0
        }
    }

    /// Get data for the currently selected item
    fn get_current_data(&self) -> Vec<u8> {
        match self.current_selector {
            selector::SIGNATURE => b"QEMU".to_vec(),
            selector::ID => {
                // Features: traditional I/O + DMA
                vec![0x03, 0x00, 0x00, 0x00]
            }
            selector::FILE_DIR => self.build_file_directory(),
            sel => {
                // Look up in registered files
                for file in &self.files {
                    if file.selector == sel {
                        return file.data.clone();
                    }
                }
                Vec::new()
            }
        }
    }

    /// Build the file directory structure
    fn build_file_directory(&self) -> Vec<u8> {
        let count = u32_of(self.files.len());
        let mut dir = count.to_be_bytes().to_vec();

        for file in &self.files {
            // Size (4 bytes, big-endian)
            dir.extend_from_slice(&(u32_of(file.data.len())).to_be_bytes());
            // Selector (2 bytes, big-endian)
            dir.extend_from_slice(&file.selector.to_be_bytes());
            // Reserved (2 bytes)
            dir.extend_from_slice(&[0u8; 2]);
            // Name (56 bytes, null-padded)
            let mut name_buf = [0u8; 56];
            let name_bytes = file.name.as_bytes();
            let len = name_bytes.len().min(55);
            name_buf[..len].copy_from_slice(&name_bytes[..len]);
            dir.extend_from_slice(&name_buf);
        }

        dir
    }

    /// Handle PIO read
    #[must_use]
    pub fn pio_read(&mut self, port: u16) -> u8 {
        match port {
            FW_CFG_PORT_DATA => self.read_data(),
            _ => 0,
        }
    }

    /// Handle PIO write
    pub const fn pio_write(&mut self, port: u16, value: u16) {
        if port == FW_CFG_PORT_SEL {
            self.write_selector(value);
        }
    }

    /// Get the number of registered files
    #[must_use]
    pub const fn file_count(&self) -> usize {
        self.files.len()
    }
}

impl Default for FwCfgDevice {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature() {
        let mut dev = FwCfgDevice::new();
        dev.write_selector(selector::SIGNATURE);
        let mut sig = Vec::new();
        for _ in 0..4 {
            sig.push(dev.read_data());
        }
        assert_eq!(&sig, b"QEMU");
    }

    #[test]
    fn add_and_read_file() {
        let mut dev = FwCfgDevice::new();
        let sel = dev.add_file("test/data", vec![0xDE, 0xAD, 0xBE, 0xEF]);

        dev.write_selector(sel);
        assert_eq!(dev.read_data(), 0xDE);
        assert_eq!(dev.read_data(), 0xAD);
        assert_eq!(dev.read_data(), 0xBE);
        assert_eq!(dev.read_data(), 0xEF);
        assert_eq!(dev.read_data(), 0); // past end
    }

    #[test]
    fn file_directory() {
        let mut dev = FwCfgDevice::new();
        dev.add_file("etc/acpi/rsdp", vec![1, 2, 3]);
        dev.add_file("etc/smbios/anchor", vec![4, 5]);

        dev.write_selector(selector::FILE_DIR);
        // First 4 bytes: file count (big-endian)
        let b0 = dev.read_data();
        let b1 = dev.read_data();
        let b2 = dev.read_data();
        let b3 = dev.read_data();
        let count = u32::from_be_bytes([b0, b1, b2, b3]);
        assert_eq!(count, 2);
    }

    #[test]
    fn selector_resets_offset() {
        let mut dev = FwCfgDevice::new();
        let sel = dev.add_file("test", vec![0x11, 0x22, 0x33]);

        dev.write_selector(sel);
        assert_eq!(dev.read_data(), 0x11);
        assert_eq!(dev.read_data(), 0x22);

        // Re-select resets
        dev.write_selector(sel);
        assert_eq!(dev.read_data(), 0x11);
    }

    #[test]
    fn add_acpi_tables() {
        let mut dev = FwCfgDevice::new();
        dev.add_acpi_tables(vec![1, 2, 3], vec![4, 5, 6, 7]);
        assert_eq!(dev.file_count(), 2);
    }

    #[test]
    fn pio_interface() {
        let mut dev = FwCfgDevice::new();
        dev.pio_write(FW_CFG_PORT_SEL, selector::SIGNATURE);
        assert_eq!(dev.pio_read(FW_CFG_PORT_DATA), b'Q');
    }
}

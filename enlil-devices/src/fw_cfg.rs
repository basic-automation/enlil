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

/// Bus-level port I/O: the firmware writes the 16-bit item selector to `0x510`
/// and then reads the selected item one byte at a time from the data register
/// `0x511`. The data register is read-only here (DMA at `0x514` is not modelled;
/// byte-stream reads are sufficient to deliver the table/file set). The selector
/// register is write-only — reads from it return 0, matching real `fw_cfg`.
impl crate::bus::PioDevice for FwCfgDevice {
    fn pio_read(&mut self, port: u16, size: u8) -> u32 {
        if port != FW_CFG_PORT_DATA {
            return 0;
        }
        // Assemble up to `size` little-endian bytes from the item stream; each
        // read advances the offset, exactly as a `rep insb` over the data port.
        let mut value = 0u32;
        for i in 0..size.min(4) {
            value |= u32::from(self.read_data()) << (8 * u32::from(i));
        }
        value
    }

    fn pio_write(&mut self, port: u16, _size: u8, data: u32) {
        if port == FW_CFG_PORT_SEL {
            self.write_selector((data & 0xFFFF) as u16);
        }
    }

    fn port_range(&self) -> (u16, u16) {
        // [0x510, 0x512): the selector and data registers.
        (FW_CFG_PORT_SEL, FW_CFG_PORT_DATA + 1)
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
        use crate::bus::PioDevice;
        let mut dev = FwCfgDevice::new();
        // Select the signature item via a 16-bit write to the selector port,
        // then read its first byte from the data port — the bus-trait path.
        dev.pio_write(FW_CFG_PORT_SEL, 2, u32::from(selector::SIGNATURE));
        assert_eq!(dev.pio_read(FW_CFG_PORT_DATA, 1), u32::from(b'Q'));
        assert_eq!(dev.pio_read(FW_CFG_PORT_DATA, 1), u32::from(b'E'));
        // The selector port is write-only; reads return 0.
        assert_eq!(dev.pio_read(FW_CFG_PORT_SEL, 1), 0);
        // The claimed range is the two registers.
        assert_eq!(dev.port_range(), (FW_CFG_PORT_SEL, FW_CFG_PORT_DATA + 1));
    }

    #[test]
    fn pio_multi_byte_read_assembles_little_endian() {
        use crate::bus::PioDevice;
        let mut dev = FwCfgDevice::new();
        let sel = dev.add_file("test", vec![0x11, 0x22, 0x33, 0x44]);
        dev.pio_write(FW_CFG_PORT_SEL, 2, u32::from(sel));
        // A 4-byte read pulls four stream bytes, low byte first.
        assert_eq!(dev.pio_read(FW_CFG_PORT_DATA, 4), 0x4433_2211);
    }
}

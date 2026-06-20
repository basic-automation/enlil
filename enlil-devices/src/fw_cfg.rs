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

    /// Synthesize and register the SMBIOS file set from a [`SmbiosConfig`].
    ///
    /// Builds the structure table (`etc/smbios/smbios-tables`) and a 3.0 entry
    /// point (`etc/smbios/smbios-anchor`) with a **zero** structure-table
    /// address — the placeholder the `etc/table-loader` `ADD_POINTER` command
    /// patches to the guest-chosen load address at boot, exactly as QEMU does.
    /// This is the synthesis→delivery link: it ties the canonical
    /// [`SmbiosBuilder`] to the `fw_cfg` channel a firmware reads the tables off.
    ///
    /// [`SmbiosConfig`]: crate::smbios::SmbiosConfig
    /// [`SmbiosBuilder`]: crate::smbios::SmbiosBuilder
    pub fn add_smbios_from_config(&mut self, config: &crate::smbios::SmbiosConfig) {
        let builder = crate::smbios::SmbiosBuilder::new(config.clone());
        let tables = builder.build_structures();
        let anchor = builder.build_entry_point(0);
        self.add_smbios(anchor, tables);
    }

    /// Synthesize and register the ACPI file set from an [`AcpiTableSetConfig`].
    ///
    /// Builds the table set ([`build_acpi_tables`]) and registers its RSDP and
    /// concatenated table blob as `etc/acpi/rsdp` / `etc/acpi/tables` — the
    /// firmware reads both off `fw_cfg`. As with QEMU, the inter-table pointers
    /// (RSDP→XSDT→FADT→…) are placed assuming a base of 0 and patched at boot by
    /// the `etc/table-loader` `ADD_POINTER` commands once the firmware chooses
    /// the load address. The synthesis→delivery link for ACPI, mirroring
    /// [`add_smbios_from_config`](Self::add_smbios_from_config).
    ///
    /// [`AcpiTableSetConfig`]: crate::acpi::AcpiTableSetConfig
    /// [`build_acpi_tables`]: crate::acpi::build_acpi_tables
    pub fn add_acpi_from_config(&mut self, config: &crate::acpi::AcpiTableSetConfig) {
        let set = crate::acpi::build_acpi_tables(config);
        self.add_acpi_tables(set.rsdp, set.tables);
    }

    /// Synthesize the ACPI set **and** its `etc/table-loader` relocation stream,
    /// then register all three firmware files (`etc/acpi/rsdp`,
    /// `etc/acpi/tables`, `etc/table-loader`).
    ///
    /// Unlike [`add_acpi_from_config`](Self::add_acpi_from_config) — which
    /// leaves the inter-table pointers placed at the config's
    /// `table_base_address` — this builds the set at **base 0** and ships the
    /// QEMU bios-linker-loader command stream that tells OVMF/SeaBIOS where to
    /// place the files and how to relocate every pointer and recompute every
    /// checksum at load time: the complete delivery path a real firmware boot
    /// drives. The `table_base_address` of `config` is ignored (the firmware
    /// chooses the load address).
    pub fn add_acpi_with_loader(&mut self, config: &crate::acpi::AcpiTableSetConfig) {
        let zero_based = crate::acpi::AcpiTableSetConfig {
            table_base_address: 0,
            ..config.clone()
        };
        let set = crate::acpi::build_acpi_tables(&zero_based);
        let loader = crate::acpi::build_acpi_table_loader(&set);
        self.add_acpi_tables(set.rsdp, set.tables);
        self.add_file("etc/table-loader", loader);
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
    fn acpi_from_config_registers_the_synthesized_tables() {
        use crate::acpi::{AcpiTableSetConfig, build_acpi_tables};
        let config = AcpiTableSetConfig::default();
        let mut dev = FwCfgDevice::new();
        dev.add_acpi_from_config(&config);
        // etc/acpi/rsdp (0x20) then etc/acpi/tables (0x21).
        assert_eq!(dev.file_count(), 2);

        let expected = build_acpi_tables(&config);
        // The delivered tables blob matches the canonical assembler byte for byte.
        dev.write_selector(0x21);
        let got: Vec<u8> = (0..expected.tables.len())
            .map(|_| dev.read_data())
            .collect();
        assert_eq!(got, expected.tables);
        assert!(!expected.tables.is_empty(), "the assembler produced tables");
        // And the RSDP file (0x20) carries the assembled RSDP.
        dev.write_selector(0x20);
        let rsdp: Vec<u8> = (0..expected.rsdp.len()).map(|_| dev.read_data()).collect();
        assert_eq!(rsdp, expected.rsdp);
    }

    #[test]
    fn acpi_with_loader_registers_all_three_files_and_a_valid_loader() {
        use crate::acpi::AcpiTableSetConfig;
        let config = AcpiTableSetConfig::default();
        let mut dev = FwCfgDevice::new();
        dev.add_acpi_with_loader(&config);
        // etc/acpi/rsdp (0x20), etc/acpi/tables (0x21), etc/table-loader (0x22).
        assert_eq!(dev.file_count(), 3);

        // The delivered tables/rsdp are the base-0 build (so the loader's
        // ADD_POINTERs are valid): the RSDP XsdtAddress (offset 24) is the pure
        // XSDT offset (0), not a config base address.
        dev.write_selector(0x20);
        let rsdp: Vec<u8> = (0..36).map(|_| dev.read_data()).collect();
        let xsdt_addr = u64::from_le_bytes(rsdp[24..32].try_into().unwrap());
        assert_eq!(
            xsdt_addr, 0,
            "tables must be delivered at base 0 for the loader"
        );

        // The etc/table-loader file is a whole number of 128-byte commands and
        // its first command is ALLOCATE (0x1) of etc/acpi/tables.
        dev.write_selector(0x22);
        // The loader length is unknown to the test; read until the stream is
        // exhausted (read_data returns 0 past the end, but commands are 128 B so
        // read a generous bound and trim by the directory-reported size instead).
        let loader_len = dev
            .files
            .iter()
            .find(|f| f.name == "etc/table-loader")
            .map(|f| f.data.len())
            .unwrap();
        assert!(loader_len % 128 == 0 && loader_len > 0);
        let loader: Vec<u8> = (0..loader_len).map(|_| dev.read_data()).collect();
        let cmd0 = u32::from_le_bytes(loader[0..4].try_into().unwrap());
        assert_eq!(cmd0, 0x1, "first loader command is ALLOCATE");
        let name0_end = loader[4..60].iter().position(|&c| c == 0).unwrap();
        assert_eq!(&loader[4..4 + name0_end], b"etc/acpi/tables");
    }

    #[test]
    fn smbios_from_config_registers_the_synthesized_tables() {
        use crate::smbios::{SmbiosBuilder, SmbiosConfig};
        let config = SmbiosConfig::default();
        let mut dev = FwCfgDevice::new();
        let tables_sel = dev.add_file("placeholder", Vec::new()); // selector 0x20
        dev.add_smbios_from_config(&config);
        // add_smbios_from_config registered anchor (0x21) then tables (0x22).
        assert_eq!(dev.file_count(), 3);

        // The smbios-tables file matches the canonical builder's structures byte
        // for byte (proving the delivery channel carries the real synthesis).
        let expected = SmbiosBuilder::new(config).build_structures();
        let tables_file_sel = tables_sel + 2; // 0x20 -> placeholder, +1 anchor, +2 tables
        dev.write_selector(tables_file_sel);
        let got: Vec<u8> = (0..expected.len()).map(|_| dev.read_data()).collect();
        assert_eq!(got, expected);
        assert!(
            !expected.is_empty(),
            "the builder produced a non-empty table"
        );
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

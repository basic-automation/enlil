//! TPM2 (Trusted Platform Module 2.0) ACPI table builder
//!
//! Windows 11 requires TPM 2.0. This table tells the OS where to find the
//! TPM's control area and what start method to use. We configure a
//! memory-mapped TPM (start method 6) with the standard CRB interface.
//!
//! The table is emitted at **revision 4**, the layout modern (Windows-11-class)
//! firmware uses. A revision-4 TPM2 table is not the bare 52-byte revision-3
//! structure: after the Start Method it carries a 12-byte Start Method Specific
//! Parameters block and the two TCG log-area fields (Log Area Minimum Length +
//! Log Area Start Address), for a total of 76 bytes (`0x4C`). Emitting revision 4
//! with only the 52-byte body leaves the table truncated mid-structure — a real
//! ACPI parser (iasl) rejects it ("table terminates in the middle of a data
//! structure"), and a too-short rev-4 table is itself a firmware-description tell.

use super::tables::{AcpiSdtHeader, OemInfo};

/// TPM2 start methods
pub const TPM2_START_METHOD_MEMORY_MAPPED: u32 = 6;

/// Default TPM CRB control area address
pub const TPM2_CONTROL_AREA_ADDRESS: u64 = 0xFED4_0040;

/// Length of a complete revision-4 TPM2 table: the 52-byte revision-3 body plus
/// the 12-byte Start Method Specific Parameters and the 4+8-byte log-area fields.
pub const TPM2_REVISION_4_LENGTH: u32 = 76;

/// TPM2 table builder
pub struct Tpm2Builder {
    oem: OemInfo,
    platform_class: u16,
    control_area_address: u64,
    start_method: u32,
    log_area_min_length: u32,
    log_area_start_address: u64,
}

impl Tpm2Builder {
    /// Create a new TPM2 builder with standard CRB configuration
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            platform_class: 0, // client platform
            control_area_address: TPM2_CONTROL_AREA_ADDRESS,
            start_method: TPM2_START_METHOD_MEMORY_MAPPED,
            // No event log exposed through ACPI (Laml/Lasa = 0). The fields are
            // still present so the revision-4 structure is complete; a guest that
            // wants the TCG log falls back to the EFI configuration table.
            log_area_min_length: 0,
            log_area_start_address: 0,
        }
    }

    /// Set the TCG event-log area (Log Area Minimum Length + Log Area Start
    /// Address). Leave at the default `(0, 0)` when no ACPI-described log exists.
    #[must_use]
    pub const fn log_area(mut self, min_length: u32, start_address: u64) -> Self {
        self.log_area_min_length = min_length;
        self.log_area_start_address = start_address;
        self
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub const fn platform_class(mut self, class: u16) -> Self {
        self.platform_class = class;
        self
    }

    #[must_use]
    pub const fn control_area_address(mut self, addr: u64) -> Self {
        self.control_area_address = addr;
        self
    }

    #[must_use]
    pub const fn start_method(mut self, method: u32) -> Self {
        self.start_method = method;
        self
    }

    /// Build the TPM2 table as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        // Revision-4 TPM2: 36-byte header + the 16-byte common body + the 12-byte
        // Start Method Specific Parameters + 4-byte Laml + 8-byte Lasa = 76 bytes.
        let total_length = TPM2_REVISION_4_LENGTH;
        let mut buf = Vec::with_capacity(total_length as usize);

        let header = AcpiSdtHeader::new(*b"TPM2", total_length, 4, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Offset 36: Platform Class (2 bytes)
        buf.extend_from_slice(&self.platform_class.to_le_bytes());

        // Offset 38: Reserved (2 bytes)
        buf.extend_from_slice(&[0u8; 2]);

        // Offset 40: Address of Control Area (8 bytes)
        buf.extend_from_slice(&self.control_area_address.to_le_bytes());

        // Offset 48: Start Method (4 bytes)
        buf.extend_from_slice(&self.start_method.to_le_bytes());

        // Offset 52: Start Method Specific Parameters (12 bytes). The memory-mapped
        // start method takes no platform-specific parameters, so the block is
        // present (the revision-4 layout requires it) but zeroed.
        buf.extend_from_slice(&[0u8; 12]);

        // Offset 64: Log Area Minimum Length (4 bytes)
        buf.extend_from_slice(&self.log_area_min_length.to_le_bytes());

        // Offset 68: Log Area Start Address (8 bytes)
        buf.extend_from_slice(&self.log_area_start_address.to_le_bytes());

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for Tpm2Builder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tpm2_builds() {
        let tpm2 = Tpm2Builder::new().build();
        // A complete revision-4 table is 76 bytes (see TPM2_REVISION_4_LENGTH).
        assert_eq!(tpm2.len(), 76);
        assert_eq!(&tpm2[0..4], b"TPM2");
    }

    #[test]
    fn tpm2_checksum() {
        let tpm2 = Tpm2Builder::new().build();
        let sum: u8 = tpm2.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn tpm2_revision() {
        let tpm2 = Tpm2Builder::new().build();
        assert_eq!(tpm2[8], 4); // TPM2 table revision 4
    }

    #[test]
    fn tpm2_platform_class() {
        let tpm2 = Tpm2Builder::new().build();
        let class = u16::from_le_bytes(tpm2[36..38].try_into().unwrap());
        assert_eq!(class, 0); // client platform
    }

    #[test]
    fn tpm2_control_area() {
        let tpm2 = Tpm2Builder::new().build();
        let addr = u64::from_le_bytes(tpm2[40..48].try_into().unwrap());
        assert_eq!(addr, TPM2_CONTROL_AREA_ADDRESS);
    }

    #[test]
    fn tpm2_start_method() {
        let tpm2 = Tpm2Builder::new().build();
        let method = u32::from_le_bytes(tpm2[48..52].try_into().unwrap());
        assert_eq!(method, TPM2_START_METHOD_MEMORY_MAPPED);
    }

    #[test]
    fn tpm2_control_area_falls_in_the_crb_aperture_at_the_control_block() {
        use crate::tpm::{TPM_MMIO_BASE, TPM_MMIO_SIZE, crb_regs};

        // The control-area address the ACPI TPM2 table advertises must point at
        // the control register block of the CRB MMIO aperture the platform
        // actually mounts (DeviceBus::add_tpm), or a guest that reads the table
        // would drive the TPM at the wrong address. It is the CRB control block
        // (CTRL_REQ) at TPM_MMIO_BASE + 0x40, inside the mounted page.
        assert!(
            (TPM_MMIO_BASE..TPM_MMIO_BASE + TPM_MMIO_SIZE).contains(&TPM2_CONTROL_AREA_ADDRESS),
            "the advertised control area must lie within the mounted CRB aperture"
        );
        assert_eq!(
            TPM2_CONTROL_AREA_ADDRESS,
            TPM_MMIO_BASE + crb_regs::CTRL_REQ,
            "the control area must be the CRB control block (CTRL_REQ)"
        );
    }

    #[test]
    fn tpm2_custom_control_area() {
        let tpm2 = Tpm2Builder::new().control_area_address(0xFED4_0000).build();
        let addr = u64::from_le_bytes(tpm2[40..48].try_into().unwrap());
        assert_eq!(addr, 0xFED4_0000);
        let sum: u8 = tpm2.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn tpm2_length_field() {
        let tpm2 = Tpm2Builder::new().build();
        let length = u32::from_le_bytes(tpm2[4..8].try_into().unwrap());
        assert_eq!(length, 76);
        // The header length field must match the actual buffer length.
        assert_eq!(length as usize, tpm2.len());
    }

    #[test]
    fn tpm2_revision_4_carries_params_and_log_fields() {
        // The revision-4 structure is complete: the Start Method Specific
        // Parameters (12 bytes at offset 52) and the log-area fields (Laml at 64,
        // Lasa at 68) are all present so a real ACPI parser does not see the table
        // terminate mid-structure.
        let tpm2 = Tpm2Builder::new()
            .log_area(0x0001_0000, 0xDEAD_BEEF_0000)
            .build();
        assert_eq!(tpm2.len(), 76);
        // Method parameters block (zeroed).
        assert_eq!(&tpm2[52..64], &[0u8; 12]);
        // Laml / Lasa carry the configured log area.
        assert_eq!(
            u32::from_le_bytes(tpm2[64..68].try_into().unwrap()),
            0x0001_0000
        );
        assert_eq!(
            u64::from_le_bytes(tpm2[68..76].try_into().unwrap()),
            0xDEAD_BEEF_0000
        );
        let sum: u8 = tpm2.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }
}

//! TPM2 (Trusted Platform Module 2.0) ACPI table builder
//!
//! Windows 11 requires TPM 2.0. This table tells the OS where to find the
//! TPM's control area and what start method to use. We configure a
//! memory-mapped TPM (start method 6) with the standard CRB interface.

use super::tables::{AcpiSdtHeader, OemInfo};

/// TPM2 start methods
pub const TPM2_START_METHOD_MEMORY_MAPPED: u32 = 6;

/// Default TPM CRB control area address
pub const TPM2_CONTROL_AREA_ADDRESS: u64 = 0xFED4_0040;

/// TPM2 table builder
pub struct Tpm2Builder {
    oem: OemInfo,
    platform_class: u16,
    control_area_address: u64,
    start_method: u32,
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
        }
    }

    #[must_use]
    pub fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub fn platform_class(mut self, class: u16) -> Self {
        self.platform_class = class;
        self
    }

    #[must_use]
    pub fn control_area_address(mut self, addr: u64) -> Self {
        self.control_area_address = addr;
        self
    }

    #[must_use]
    pub fn start_method(mut self, method: u32) -> Self {
        self.start_method = method;
        self
    }

    /// Build the TPM2 table as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        // TPM2 table: 36-byte header + 16 bytes of fields = 52 bytes minimum
        // (start method parameters omitted for method 6 — no extra params needed)
        let total_length: u32 = 52;
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
        assert_eq!(tpm2.len(), 52);
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
    fn tpm2_custom_control_area() {
        let tpm2 = Tpm2Builder::new()
            .control_area_address(0xFED4_0000)
            .build();
        let addr = u64::from_le_bytes(tpm2[40..48].try_into().unwrap());
        assert_eq!(addr, 0xFED4_0000);
        let sum: u8 = tpm2.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn tpm2_length_field() {
        let tpm2 = Tpm2Builder::new().build();
        let length = u32::from_le_bytes(tpm2[4..8].try_into().unwrap());
        assert_eq!(length, 52);
    }
}

//! Common ACPI table header and OEM info structures.

/// OEM information for ACPI tables — configurable to match real hardware.
#[derive(Debug, Clone)]
pub struct OemInfo {
    pub oem_id: [u8; 6],
    pub oem_table_id: [u8; 8],
    pub oem_revision: u32,
    pub creator_id: [u8; 4],
    pub creator_revision: u32,
}

impl OemInfo {
    /// AMI BIOS-style OEM info (most common on consumer boards)
    #[must_use]
    pub const fn ami() -> Self {
        Self {
            oem_id: *b"ALASKA",
            oem_table_id: *b"A M I   ",
            oem_revision: 0x0100_0013,
            creator_id: *b"AMI ",
            creator_revision: 0x0001_0013,
        }
    }

    /// Create from custom strings
    #[must_use]
    pub fn custom(oem_id: &str, table_id: &str) -> Self {
        Self {
            oem_id: pad6(oem_id),
            oem_table_id: pad8(table_id),
            oem_revision: 1,
            creator_id: *b"INTL",
            creator_revision: 0x2019_1213,
        }
    }
}

impl Default for OemInfo {
    fn default() -> Self {
        Self::ami()
    }
}

/// Standard ACPI SDT header (36 bytes).
#[derive(Debug, Clone)]
pub struct AcpiSdtHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub revision: u8,
    pub oem: OemInfo,
}

impl AcpiSdtHeader {
    pub const SIZE: usize = 36;

    #[must_use]
    pub fn new(signature: [u8; 4], length: u32, revision: u8, oem: &OemInfo) -> Self {
        Self {
            signature,
            length,
            revision,
            oem: oem.clone(),
        }
    }

    /// Serialize to 36 bytes. Checksum byte is 0 — caller must fix up.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(Self::SIZE);
        buf.extend_from_slice(&self.signature);
        buf.extend_from_slice(&self.length.to_le_bytes());
        buf.push(self.revision);
        buf.push(0); // checksum — caller fixes this
        buf.extend_from_slice(&self.oem.oem_id);
        buf.extend_from_slice(&self.oem.oem_table_id);
        buf.extend_from_slice(&self.oem.oem_revision.to_le_bytes());
        buf.extend_from_slice(&self.oem.creator_id);
        buf.extend_from_slice(&self.oem.creator_revision.to_le_bytes());
        buf
    }
}

/// Pad a string to 6 bytes with spaces
#[must_use]
pub fn pad6(s: &str) -> [u8; 6] {
    let mut out = [b' '; 6];
    let bytes = s.as_bytes();
    let len = bytes.len().min(6);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

/// Pad a string to 8 bytes with spaces
#[must_use]
pub fn pad8(s: &str) -> [u8; 8] {
    let mut out = [b' '; 8];
    let bytes = s.as_bytes();
    let len = bytes.len().min(8);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

/// Compute ACPI checksum: all bytes must sum to 0 mod 256.
#[must_use]
pub fn acpi_checksum(data: &[u8]) -> u8 {
    let sum: u8 = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    (!sum).wrapping_add(1)
}

/// Fix up the checksum byte at offset 9 in an ACPI table.
pub fn fixup_checksum(data: &mut [u8]) {
    data[9] = 0;
    let sum: u8 = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    data[9] = (!sum).wrapping_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oem_info_ami() {
        let oem = OemInfo::ami();
        assert_eq!(&oem.oem_id, b"ALASKA");
    }

    #[test]
    fn header_serializes_to_36_bytes() {
        let oem = OemInfo::default();
        let hdr = AcpiSdtHeader::new(*b"FACP", 276, 6, &oem);
        let bytes = hdr.to_bytes();
        assert_eq!(bytes.len(), 36);
        assert_eq!(&bytes[0..4], b"FACP");
    }

    #[test]
    fn pad_strings() {
        assert_eq!(pad6("AMI"), [b'A', b'M', b'I', b' ', b' ', b' ']);
        assert_eq!(&pad8("FACP"), b"FACP    ");
    }

    #[test]
    fn checksum_works() {
        let mut data = vec![0u8; 36];
        data[0..4].copy_from_slice(b"TEST");
        fixup_checksum(&mut data);
        let sum: u8 = data.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }
}

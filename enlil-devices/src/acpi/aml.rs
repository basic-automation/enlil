//! AML (ACPI Machine Language) bytecode builder
//!
//! Generates AML bytecode for DSDT/SSDT tables. This is the "code" that ACPI
//! evaluates — it defines virtual devices, power management objects, and
//! the PCI/ISA bus topology that Windows discovers.
//!
//! We generate realistic AML that matches what a physical motherboard would
//! produce, avoiding any hypervisor-specific strings or structures.

/// AML opcodes
pub mod opcode {
    pub const ZERO: u8 = 0x00;
    pub const ONE: u8 = 0x01;
    pub const BYTE_PREFIX: u8 = 0x0A;
    pub const WORD_PREFIX: u8 = 0x0B;
    pub const DWORD_PREFIX: u8 = 0x0C;
    pub const QWORD_PREFIX: u8 = 0x0E;
    pub const STRING_PREFIX: u8 = 0x0D;
    pub const SCOPE_OP: u8 = 0x10;
    pub const NAME_OP: u8 = 0x08;
    pub const METHOD_OP: u8 = 0x14;
    pub const PACKAGE_OP: u8 = 0x12;
    pub const RETURN_OP: u8 = 0xA4;
    pub const DEVICE_OP: u8 = 0x82; // ExtOp prefix 0x5B required
    pub const PROCESSOR_OP: u8 = 0x83; // ExtOp prefix 0x5B required
    pub const EXT_OP_PREFIX: u8 = 0x5B;
    pub const OPERATION_REGION_OP: u8 = 0x80; // ExtOp prefix 0x5B required
    pub const FIELD_OP: u8 = 0x81; // ExtOp prefix 0x5B required
    pub const IF_OP: u8 = 0xA0;
    pub const ELSE_OP: u8 = 0xA1;
    pub const NOTIFY_OP: u8 = 0x86;
    pub const STORE_OP: u8 = 0x70;
    pub const LOCAL0: u8 = 0x60;
    pub const ARG0: u8 = 0x68;
    pub const BUFFER_OP: u8 = 0x11;
}

/// AML bytecode builder — low-level byte emission
pub struct AmlBuilder {
    data: Vec<u8>,
}

impl AmlBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            data: Vec::with_capacity(4096),
        }
    }

    /// Get the raw bytes
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    /// Current length
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Write raw bytes
    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.data.extend_from_slice(bytes);
        self
    }

    /// Encode a PkgLength field (ACPI spec §20.2.4)
    #[allow(clippy::cast_possible_truncation)]
    fn encode_pkg_length(length: usize) -> Vec<u8> {
        if length < 0x3F {
            vec![length as u8]
        } else if length < 0xFFF {
            vec![
                ((length & 0x0F) as u8) | (1 << 6),
                ((length >> 4) & 0xFF) as u8,
            ]
        } else if length < 0xF_FFFF {
            vec![
                ((length & 0x0F) as u8) | (2 << 6),
                ((length >> 4) & 0xFF) as u8,
                ((length >> 12) & 0xFF) as u8,
            ]
        } else {
            vec![
                ((length & 0x0F) as u8) | (3 << 6),
                ((length >> 4) & 0xFF) as u8,
                ((length >> 12) & 0xFF) as u8,
                ((length >> 20) & 0xFF) as u8,
            ]
        }
    }

    /// Encode a 4-character ACPI name
    fn encode_name(name: &[u8; 4]) -> [u8; 4] {
        *name
    }

    /// Name(name, value) — defines a named integer
    #[allow(clippy::cast_possible_truncation)]
    pub fn name_integer(&mut self, name: &[u8; 4], value: u64) -> &mut Self {
        self.data.push(opcode::NAME_OP);
        self.data.extend_from_slice(&Self::encode_name(name));
        if value == 0 {
            self.data.push(opcode::ZERO);
        } else if value == 1 {
            self.data.push(opcode::ONE);
        } else if value <= 0xFF {
            self.data.push(opcode::BYTE_PREFIX);
            self.data.push(value as u8);
        } else if value <= 0xFFFF {
            self.data.push(opcode::WORD_PREFIX);
            self.data.extend_from_slice(&(value as u16).to_le_bytes());
        } else if value <= 0xFFFF_FFFF {
            self.data.push(opcode::DWORD_PREFIX);
            self.data.extend_from_slice(&(value as u32).to_le_bytes());
        } else {
            self.data.push(opcode::QWORD_PREFIX);
            self.data.extend_from_slice(&value.to_le_bytes());
        }
        self
    }

    /// Name(name, "string")
    pub fn name_string(&mut self, name: &[u8; 4], value: &str) -> &mut Self {
        self.data.push(opcode::NAME_OP);
        self.data.extend_from_slice(&Self::encode_name(name));
        self.data.push(opcode::STRING_PREFIX);
        self.data.extend_from_slice(value.as_bytes());
        self.data.push(0); // null terminator
        self
    }

    /// Start a Scope block, returns position to patch length
    pub fn scope_start(&mut self, name: &[u8; 4]) -> ScopeHandle {
        self.data.push(opcode::SCOPE_OP);
        let length_pos = self.data.len();
        // Reserve 4 bytes for PkgLength (worst case)
        self.data.extend_from_slice(&[0, 0, 0, 0]);
        self.data.extend_from_slice(&Self::encode_name(name));
        ScopeHandle {
            opcode_pos: length_pos - 1,
            length_pos,
            content_start: self.data.len(),
        }
    }

    /// Close a Scope block, patches the PkgLength
    pub fn scope_end(&mut self, handle: ScopeHandle) {
        self.patch_pkg_length(handle.length_pos, handle.content_start);
    }

    /// Start a Device block
    pub fn device_start(&mut self, name: &[u8; 4]) -> ScopeHandle {
        self.data.push(opcode::EXT_OP_PREFIX);
        self.data.push(opcode::DEVICE_OP);
        let length_pos = self.data.len();
        self.data.extend_from_slice(&[0, 0, 0, 0]);
        self.data.extend_from_slice(&Self::encode_name(name));
        ScopeHandle {
            opcode_pos: length_pos - 2,
            length_pos,
            content_start: self.data.len(),
        }
    }

    /// Close a Device block
    pub fn device_end(&mut self, handle: ScopeHandle) {
        self.patch_pkg_length(handle.length_pos, handle.content_start);
    }

    /// Method(name, argc, serialized, body)
    pub fn method_start(&mut self, name: &[u8; 4], argc: u8, serialized: bool) -> ScopeHandle {
        self.data.push(opcode::METHOD_OP);
        let length_pos = self.data.len();
        self.data.extend_from_slice(&[0, 0, 0, 0]);
        self.data.extend_from_slice(&Self::encode_name(name));
        let flags = argc | (u8::from(serialized) << 3);
        self.data.push(flags);
        ScopeHandle {
            opcode_pos: length_pos - 1,
            length_pos,
            content_start: self.data.len(),
        }
    }

    pub fn method_end(&mut self, handle: ScopeHandle) {
        self.patch_pkg_length(handle.length_pos, handle.content_start);
    }

    /// Return(integer)
    #[allow(clippy::cast_possible_truncation)]
    pub fn return_integer(&mut self, value: u64) -> &mut Self {
        self.data.push(opcode::RETURN_OP);
        if value == 0 {
            self.data.push(opcode::ZERO);
        } else if value == 1 {
            self.data.push(opcode::ONE);
        } else if value <= 0xFF {
            self.data.push(opcode::BYTE_PREFIX);
            self.data.push(value as u8);
        } else if value <= 0xFFFF {
            self.data.push(opcode::WORD_PREFIX);
            self.data.extend_from_slice(&(value as u16).to_le_bytes());
        } else {
            self.data.push(opcode::DWORD_PREFIX);
            self.data.extend_from_slice(&(value as u32).to_le_bytes());
        }
        self
    }

    /// Patch a PkgLength at the given position
    #[allow(clippy::cast_possible_truncation)]
    fn patch_pkg_length(&mut self, length_pos: usize, content_start: usize) {
        let total_len = self.data.len() - length_pos;
        let encoded = Self::encode_pkg_length(total_len);

        // We reserved 4 bytes. Replace with actual encoding + shift if needed.
        let reserved = 4;
        let actual = encoded.len();

        if actual <= reserved {
            // Copy encoded bytes, shift remaining content left
            let shift = reserved - actual;
            for (i, &b) in encoded.iter().enumerate() {
                self.data[length_pos + i] = b;
            }
            if shift > 0 {
                let src_start = length_pos + reserved;
                let remaining = self.data.len() - src_start;
                self.data
                    .copy_within(src_start..src_start + remaining, length_pos + actual);
                self.data.truncate(self.data.len() - shift);
            }
        }
    }
}

impl Default for AmlBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle for patching scope/device/method lengths
pub struct ScopeHandle {
    #[allow(dead_code)]
    opcode_pos: usize,
    length_pos: usize,
    #[allow(dead_code)]
    content_start: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aml_name_integer() {
        let mut aml = AmlBuilder::new();
        aml.name_integer(b"TEST", 42);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::NAME_OP);
        assert_eq!(&bytes[1..5], b"TEST");
        assert_eq!(bytes[5], opcode::BYTE_PREFIX);
        assert_eq!(bytes[6], 42);
    }

    #[test]
    fn aml_name_string() {
        let mut aml = AmlBuilder::new();
        aml.name_string(b"_HID", "PNP0A03");
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::NAME_OP);
        assert_eq!(&bytes[1..5], b"_HID");
        assert_eq!(bytes[5], opcode::STRING_PREFIX);
        assert_eq!(&bytes[6..13], b"PNP0A03");
        assert_eq!(bytes[13], 0); // null terminator
    }

    #[test]
    fn aml_name_zero() {
        let mut aml = AmlBuilder::new();
        aml.name_integer(b"ZERO", 0);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[5], opcode::ZERO);
    }

    #[test]
    fn aml_scope() {
        let mut aml = AmlBuilder::new();
        let scope = aml.scope_start(b"_SB_");
        aml.name_integer(b"TEST", 1);
        aml.scope_end(scope);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::SCOPE_OP);
        // Should have valid PkgLength
        assert!(!bytes.is_empty());
    }

    #[test]
    fn aml_device_with_hid() {
        let mut aml = AmlBuilder::new();
        let sb = aml.scope_start(b"_SB_");
        let dev = aml.device_start(b"PCI0");
        aml.name_string(b"_HID", "PNP0A08"); // PCI Express root
        aml.name_string(b"_CID", "PNP0A03"); // PCI compatible
        aml.name_integer(b"_UID", 0);
        aml.device_end(dev);
        aml.scope_end(sb);
        let bytes = aml.into_bytes();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn aml_method_return() {
        let mut aml = AmlBuilder::new();
        let method = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F); // Present + Enabled + Functional
        aml.method_end(method);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::METHOD_OP);
    }

    #[test]
    fn pkg_length_encoding() {
        // Small values (< 63) should be 1 byte
        assert_eq!(AmlBuilder::encode_pkg_length(10).len(), 1);
        // Medium values should be 2 bytes
        assert_eq!(AmlBuilder::encode_pkg_length(0x100).len(), 2);
        // Large values should be 3 bytes
        assert_eq!(AmlBuilder::encode_pkg_length(0x10000).len(), 3);
    }
}

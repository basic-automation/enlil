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
    pub const fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Write raw bytes
    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.data.extend_from_slice(bytes);
        self
    }

    /// Encode a `PkgLength` field (ACPI spec §20.2.4)
    fn encode_pkg_length(length: usize) -> Vec<u8> {
        if length < 0x3F {
            vec![length.to_le_bytes()[0]]
        } else if length < 0xFFF {
            vec![
                (length.to_le_bytes()[0] & 0x0F) | (1 << 6),
                (length >> 4).to_le_bytes()[0],
            ]
        } else if length < 0xF_FFFF {
            vec![
                (length.to_le_bytes()[0] & 0x0F) | (2 << 6),
                (length >> 4).to_le_bytes()[0],
                (length >> 12).to_le_bytes()[0],
            ]
        } else {
            vec![
                (length.to_le_bytes()[0] & 0x0F) | (3 << 6),
                (length >> 4).to_le_bytes()[0],
                (length >> 12).to_le_bytes()[0],
                (length >> 20).to_le_bytes()[0],
            ]
        }
    }

    /// Encode a 4-character ACPI name
    const fn encode_name(name: [u8; 4]) -> [u8; 4] {
        name
    }

    /// Name(name, value) — defines a named integer
    pub fn name_integer(&mut self, name: &[u8; 4], value: u64) -> &mut Self {
        self.data.push(opcode::NAME_OP);
        self.data.extend_from_slice(&Self::encode_name(*name));
        if value == 0 {
            self.data.push(opcode::ZERO);
        } else if value == 1 {
            self.data.push(opcode::ONE);
        } else if value <= 0xFF {
            self.data.push(opcode::BYTE_PREFIX);
            self.data.push(value.to_le_bytes()[0]);
        } else if value <= 0xFFFF {
            self.data.push(opcode::WORD_PREFIX);
            self.data.extend_from_slice(&value.to_le_bytes()[..2]);
        } else if value <= 0xFFFF_FFFF {
            self.data.push(opcode::DWORD_PREFIX);
            self.data.extend_from_slice(&value.to_le_bytes()[..4]);
        } else {
            self.data.push(opcode::QWORD_PREFIX);
            self.data.extend_from_slice(&value.to_le_bytes());
        }
        self
    }

    /// Name(name, "string")
    pub fn name_string(&mut self, name: &[u8; 4], value: &str) -> &mut Self {
        self.data.push(opcode::NAME_OP);
        self.data.extend_from_slice(&Self::encode_name(*name));
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
        self.data.extend_from_slice(&Self::encode_name(*name));
        ScopeHandle {
            length_pos,
            content_start: self.data.len(),
        }
    }

    /// Close a Scope block, patches the `PkgLength`
    pub fn scope_end(&mut self, handle: &ScopeHandle) {
        self.patch_pkg_length(handle.length_pos, handle.content_start);
    }

    /// Start a Device block
    pub fn device_start(&mut self, name: &[u8; 4]) -> ScopeHandle {
        self.data.push(opcode::EXT_OP_PREFIX);
        self.data.push(opcode::DEVICE_OP);
        let length_pos = self.data.len();
        self.data.extend_from_slice(&[0, 0, 0, 0]);
        self.data.extend_from_slice(&Self::encode_name(*name));
        ScopeHandle {
            length_pos,
            content_start: self.data.len(),
        }
    }

    /// Close a Device block
    pub fn device_end(&mut self, handle: &ScopeHandle) {
        self.patch_pkg_length(handle.length_pos, handle.content_start);
    }

    /// Method(name, argc, serialized, body)
    pub fn method_start(&mut self, name: &[u8; 4], argc: u8, serialized: bool) -> ScopeHandle {
        self.data.push(opcode::METHOD_OP);
        let length_pos = self.data.len();
        self.data.extend_from_slice(&[0, 0, 0, 0]);
        self.data.extend_from_slice(&Self::encode_name(*name));
        let flags = argc | (u8::from(serialized) << 3);
        self.data.push(flags);
        ScopeHandle {
            length_pos,
            content_start: self.data.len(),
        }
    }

    pub fn method_end(&mut self, handle: &ScopeHandle) {
        self.patch_pkg_length(handle.length_pos, handle.content_start);
    }

    /// Return(integer)
    pub fn return_integer(&mut self, value: u64) -> &mut Self {
        self.data.push(opcode::RETURN_OP);
        if value == 0 {
            self.data.push(opcode::ZERO);
        } else if value == 1 {
            self.data.push(opcode::ONE);
        } else if value <= 0xFF {
            self.data.push(opcode::BYTE_PREFIX);
            self.data.push(value.to_le_bytes()[0]);
        } else if value <= 0xFFFF {
            self.data.push(opcode::WORD_PREFIX);
            self.data.extend_from_slice(&value.to_le_bytes()[..2]);
        } else {
            self.data.push(opcode::DWORD_PREFIX);
            self.data.extend_from_slice(&value.to_le_bytes()[..4]);
        }
        self
    }

    /// Encode a self-inclusive `PkgLength` for a package whose body (everything
    /// after the `PkgLength` field) is `content_len` bytes.
    ///
    /// A `PkgLength` counts from its own first byte to the end of the package
    /// (ACPI spec §20.2.4), so the encoded value must include the size of the
    /// field itself. The field is 1–4 bytes; a forward pass from the 1-byte form
    /// upward finds the smallest field that can hold `content_len + field_len`
    /// (growing the field only ever grows the total, so the first fit is minimal).
    fn encode_self_pkg_length(content_len: usize) -> Vec<u8> {
        for field_len in 1..=4 {
            let encoded = Self::encode_pkg_length(content_len + field_len);
            if encoded.len() == field_len {
                return encoded;
            }
        }
        // content_len + 4 always fits the 4-byte form for any realistic table.
        Self::encode_pkg_length(content_len + 4)
    }

    /// Patch a `PkgLength` at the given position
    fn patch_pkg_length(&mut self, length_pos: usize, _content_start: usize) {
        // We reserved 4 bytes for the field; the body is everything after them.
        let reserved = 4;
        let content_len = self.data.len() - length_pos - reserved;
        let encoded = Self::encode_self_pkg_length(content_len);
        let actual = encoded.len();

        // Overwrite the reserved field with the real encoding, then close the
        // gap by shifting the body left (the value already accounts for `actual`,
        // not `reserved`, so the package stays self-consistent after the shift).
        for (i, &b) in encoded.iter().enumerate() {
            self.data[length_pos + i] = b;
        }
        let shift = reserved - actual;
        if shift > 0 {
            let src_start = length_pos + reserved;
            let remaining = self.data.len() - src_start;
            self.data
                .copy_within(src_start..src_start + remaining, length_pos + actual);
            self.data.truncate(self.data.len() - shift);
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
    length_pos: usize,
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
        aml.scope_end(&scope);
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
        aml.device_end(&dev);
        aml.scope_end(&sb);
        let bytes = aml.into_bytes();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn aml_method_return() {
        let mut aml = AmlBuilder::new();
        let method = aml.method_start(b"_STA", 0, false);
        aml.return_integer(0x0F); // Present + Enabled + Functional
        aml.method_end(&method);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::METHOD_OP);
    }

    /// Decode an ACPI `PkgLength` field (ACPI spec §20.2.4): returns the encoded
    /// value and the number of bytes the field occupies.
    fn decode_pkg_length(b: &[u8]) -> (usize, usize) {
        let lead = b[0];
        let nbytes = usize::from(lead >> 6);
        if nbytes == 0 {
            (usize::from(lead & 0x3F), 1)
        } else {
            let mut val = usize::from(lead & 0x0F);
            for (i, &byte) in b[1..=nbytes].iter().enumerate() {
                val |= usize::from(byte) << (4 + 8 * i);
            }
            (val, 1 + nbytes)
        }
    }

    #[test]
    fn scope_pkg_length_matches_actual_package_size() {
        // A PkgLength counts from its own first byte to the end of the package.
        // The buggy encoder overshot by the shift amount, which would make a real
        // ACPI interpreter read past the scope and corrupt all following AML.
        let mut aml = AmlBuilder::new();
        let scope = aml.scope_start(b"_SB_");
        aml.name_integer(b"TEST", 1);
        aml.scope_end(&scope);
        let bytes = aml.into_bytes();

        assert_eq!(bytes[0], opcode::SCOPE_OP);
        let (val, field_len) = decode_pkg_length(&bytes[1..]);
        assert_eq!(
            val,
            bytes.len() - 1,
            "PkgLength must equal the bytes from the field to the package end"
        );
        assert_eq!(field_len, 1, "a tiny scope uses a one-byte PkgLength");
    }

    #[test]
    fn large_package_pkg_length_is_self_consistent() {
        // Force a body large enough to need a two-byte PkgLength, exercising the
        // field-shrink/shift path with a multi-byte field.
        let mut aml = AmlBuilder::new();
        let scope = aml.scope_start(b"_SB_");
        for _ in 0..40 {
            // each name_integer(BYTE) is NAME_OP + 4 name + BYTE_PREFIX + value = 7 bytes
            aml.name_integer(b"PADX", 0x42);
        }
        aml.scope_end(&scope);
        let bytes = aml.into_bytes();

        let (val, field_len) = decode_pkg_length(&bytes[1..]);
        assert_eq!(
            field_len, 2,
            "a body over 0x3F bytes needs a two-byte field"
        );
        assert_eq!(val, bytes.len() - 1);
    }

    #[test]
    fn nested_package_lengths_are_each_correct() {
        // Outer scope contains a device; both PkgLengths must point exactly to
        // their own package end.
        let mut aml = AmlBuilder::new();
        let sb = aml.scope_start(b"_SB_");
        let dev = aml.device_start(b"PCI0");
        aml.name_string(b"_HID", "PNP0A08");
        aml.device_end(&dev);
        aml.scope_end(&sb);
        let bytes = aml.into_bytes();

        // Outer Scope(_SB_): SCOPE_OP at 0, PkgLength at 1.
        assert_eq!(bytes[0], opcode::SCOPE_OP);
        let (outer_val, outer_field) = decode_pkg_length(&bytes[1..]);
        assert_eq!(outer_val, bytes.len() - 1);

        // Inner Device: ExtOpPrefix + DEVICE_OP, then its PkgLength. It sits
        // right after the outer name (SCOPE_OP + field + "_SB_").
        let dev_op = 1 + outer_field + 4;
        assert_eq!(bytes[dev_op], opcode::EXT_OP_PREFIX);
        assert_eq!(bytes[dev_op + 1], opcode::DEVICE_OP);
        let (inner_val, _) = decode_pkg_length(&bytes[dev_op + 2..]);
        // Inner package runs from its PkgLength field to the end of the device,
        // which is the end of the whole buffer here.
        assert_eq!(inner_val, bytes.len() - (dev_op + 2));
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

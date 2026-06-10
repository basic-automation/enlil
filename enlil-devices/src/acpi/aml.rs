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
    pub const SUBTRACT_OP: u8 = 0x74;
    pub const SHIFT_LEFT_OP: u8 = 0x79;
    pub const AND_OP: u8 = 0x7B;
    pub const OR_OP: u8 = 0x7D;
    pub const FIND_SET_RIGHT_BIT_OP: u8 = 0x82;
    pub const CREATE_WORD_FIELD_OP: u8 = 0x8B;
    pub const CREATE_BYTE_FIELD_OP: u8 = 0x8C;

    /// `RegionSpace` byte for an `OperationRegion` over PCI configuration space
    /// (ACPI 6.x §19.6.102: `PCI_Config` = 2).
    pub const REGION_SPACE_PCI_CONFIG: u8 = 0x02;
}

/// A term-argument operand for the small expression helpers
/// ([`AmlBuilder::store`], [`AmlBuilder::and_op`], …).
///
/// AML term args are self-describing byte sequences: an integer constant, a
/// `NameSeg` reference, or an `ArgN`/`LocalN` opcode. The same encoding serves
/// both as a source operand and (for `Name`/`Arg`/`Local`) as a store target.
#[derive(Debug, Clone, Copy)]
pub enum Operand<'a> {
    /// Integer constant (encoded as the smallest `Byte`/`Word`/`DWord`/`QWord`).
    Int(u64),
    /// Reference to a named object by its 4-char `NameSeg`.
    Name(&'a [u8; 4]),
    /// Method argument `Arg0..Arg6`.
    Arg(u8),
    /// Method local `Local0..Local7`.
    Local(u8),
}

impl Operand<'_> {
    /// Encode this operand as an AML `TermArg` / `SuperName` byte sequence.
    fn encode(self) -> Vec<u8> {
        match self {
            Self::Int(v) => AmlBuilder::encode_integer_const(v),
            Self::Name(n) => AmlBuilder::encode_name(*n).to_vec(),
            Self::Arg(i) => vec![opcode::ARG0 + i],
            Self::Local(i) => vec![opcode::LOCAL0 + i],
        }
    }
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

    /// Encode an integer as an inline `TermArg` constant (used for a `Buffer`'s
    /// `BufferSize`). Resource templates are small, so a byte/word const suffices.
    fn encode_integer_arg(value: usize) -> Vec<u8> {
        if value == 0 {
            vec![opcode::ZERO]
        } else if value == 1 {
            vec![opcode::ONE]
        } else if value <= 0xFF {
            vec![opcode::BYTE_PREFIX, value.to_le_bytes()[0]]
        } else {
            vec![
                opcode::WORD_PREFIX,
                value.to_le_bytes()[0],
                value.to_le_bytes()[1],
            ]
        }
    }

    /// Encode an integer as an AML data-object constant (the same encoding as
    /// [`Self::name_integer`]'s value, for use as a package element).
    fn encode_integer_const(value: u64) -> Vec<u8> {
        if value == 0 {
            vec![opcode::ZERO]
        } else if value == 1 {
            vec![opcode::ONE]
        } else if value <= 0xFF {
            vec![opcode::BYTE_PREFIX, value.to_le_bytes()[0]]
        } else if value <= 0xFFFF {
            vec![
                opcode::WORD_PREFIX,
                value.to_le_bytes()[0],
                value.to_le_bytes()[1],
            ]
        } else if value <= 0xFFFF_FFFF {
            let mut v = vec![opcode::DWORD_PREFIX];
            v.extend_from_slice(&value.to_le_bytes()[..4]);
            v
        } else {
            let mut v = vec![opcode::QWORD_PREFIX];
            v.extend_from_slice(&value.to_le_bytes());
            v
        }
    }

    /// Encode `Package(){ ...integers... }` as a standalone byte sequence (for use
    /// as an element of an outer package, e.g. one `_PRT` entry).
    fn encode_integer_package(values: &[u64]) -> Vec<u8> {
        let mut body = vec![values.len().to_le_bytes()[0]];
        for &v in values {
            body.extend_from_slice(&Self::encode_integer_const(v));
        }
        let mut out = vec![opcode::PACKAGE_OP];
        out.extend_from_slice(&Self::encode_self_pkg_length(body.len()));
        out.extend_from_slice(&body);
        out
    }

    /// `Name(name, Package(){ ...integers... })` — a fixed package of integer
    /// constants, e.g. the `_Sx` sleep-state packages whose elements are the
    /// `PM1a`/`PM1b` `SLP_TYP` values the OS writes to enter that state.
    pub fn name_package(&mut self, name: &[u8; 4], values: &[u64]) -> &mut Self {
        let pkg = Self::encode_integer_package(values);
        self.data.push(opcode::NAME_OP);
        self.data.extend_from_slice(&Self::encode_name(*name));
        self.data.extend_from_slice(&pkg);
        self
    }

    /// Encode `Package(){ <sub-package> ... }` (a `_PRT`-shaped package: every
    /// element is itself a `{ Address, Pin, Source, SourceIndex }` integer
    /// sub-package) as a standalone data object, for use as a `Name` value or a
    /// `Return` operand.
    fn encode_routing_package(entries: &[[u64; 4]]) -> Vec<u8> {
        let mut body = vec![entries.len().to_le_bytes()[0]];
        for entry in entries {
            body.extend_from_slice(&Self::encode_integer_package(entry));
        }
        let mut out = vec![opcode::PACKAGE_OP];
        out.extend_from_slice(&Self::encode_self_pkg_length(body.len()));
        out.extend_from_slice(&body);
        out
    }

    /// `Name(name, Package(){ <sub-package> ... })` — a package whose every
    /// element is itself a fixed integer package. Used for a `_PRT` (each entry is
    /// `{ Address, Pin, Source, SourceIndex }`); `entries` is one 4-tuple per row.
    pub fn name_routing_table(&mut self, name: &[u8; 4], entries: &[[u64; 4]]) -> &mut Self {
        let pkg = Self::encode_routing_package(entries);
        self.data.push(opcode::NAME_OP);
        self.data.extend_from_slice(&Self::encode_name(*name));
        self.data.extend_from_slice(&pkg);
        self
    }

    /// `Return(Package(){ <sub-package> ... })` — return a `_PRT`-shaped routing
    /// package from inside a method body (e.g. a mode-selecting `_PRT` method).
    pub fn return_routing_table(&mut self, entries: &[[u64; 4]]) -> &mut Self {
        let pkg = Self::encode_routing_package(entries);
        self.data.push(opcode::RETURN_OP);
        self.data.extend_from_slice(&pkg);
        self
    }

    /// Encode one `_PRT` sub-package whose **Source** is a reference to a PCI
    /// interrupt link device (a `NameString`), not the integer 0: `{ Address, Pin,
    /// \\link, SourceIndex }`. `link` is a 4-char `NameSeg` (e.g. `b"LNKA"`) that
    /// resolves relative to the `_PRT`'s scope. In a package, a bare `NameSeg` is a
    /// reference the OS follows to the link device's `_CRS`/`_PRS`/`_SRS`.
    fn encode_named_prt_entry(
        address: u64,
        pin: u64,
        link: &[[u8; 4]],
        source_index: u64,
    ) -> Vec<u8> {
        let mut body = vec![4u8]; // NumElements
        body.extend_from_slice(&Self::encode_integer_const(address));
        body.extend_from_slice(&Self::encode_integer_const(pin));
        // Absolute (root-anchored) path to the link device. A `_PRT` is a Method, so
        // its body adds a scope level; a *relative* multi-seg path would resolve under
        // `…._PRT` (multi-seg paths get no upward search) and not exist. A rooted path
        // resolves unambiguously regardless of the enclosing method scope.
        body.extend_from_slice(&Self::encode_rooted_name_path(link));
        body.extend_from_slice(&Self::encode_integer_const(source_index));
        let mut out = vec![opcode::PACKAGE_OP];
        out.extend_from_slice(&Self::encode_self_pkg_length(body.len()));
        out.extend_from_slice(&body);
        out
    }

    /// Encode a `NameString` from its path segments: a bare `NameSeg` (1), a
    /// `DualNamePrefix` pair (2), or a `MultiNamePrefix` run (≥3), per ACPI 6.x
    /// §20.2.2. A multi-segment name resolves *relative to the current scope*
    /// (no upward search), which is how a `_PRT` under `PCI0` points at a link
    /// device nested under the ISA bridge (`ISA_.LNKA`).
    fn encode_name_path(segs: &[[u8; 4]]) -> Vec<u8> {
        match segs {
            [one] => one.to_vec(),
            [a, b] => {
                let mut v = vec![0x2E]; // DualNamePrefix
                v.extend_from_slice(a);
                v.extend_from_slice(b);
                v
            }
            many => {
                let mut v = vec![0x2F, many.len().to_le_bytes()[0]]; // MultiNamePrefix + SegCount
                for s in many {
                    v.extend_from_slice(s);
                }
                v
            }
        }
    }

    /// A root-anchored `NameString`: `RootChar` (`\`) followed by the name path
    /// (ACPI 6.x §20.2.2). Resolves from the namespace root regardless of the
    /// current scope — used for a `_PRT` `Source` that must point at a link device
    /// by absolute path from inside the `_PRT` method.
    fn encode_rooted_name_path(segs: &[[u8; 4]]) -> Vec<u8> {
        let mut v = vec![0x5C]; // RootChar
        v.extend_from_slice(&Self::encode_name_path(segs));
        v
    }

    /// `Return(Package(){ ... })` for a PIC-mode `_PRT` that routes each entry
    /// through a named PCI interrupt **link device** (the PIIX/ICH firmware
    /// pattern). Each entry is `(address, pin, link_path)` where `link_path` is the
    /// link device's name segments (e.g. `&[*b"ISA_", *b"LNKA"]`); `SourceIndex` is
    /// always 0 (the link's first/only resource).
    pub fn return_routing_table_via_links(
        &mut self,
        entries: &[(u64, u64, &[[u8; 4]])],
    ) -> &mut Self {
        let mut body = vec![entries.len().to_le_bytes()[0]];
        for (address, pin, link) in entries {
            body.extend_from_slice(&Self::encode_named_prt_entry(*address, *pin, link, 0));
        }
        self.data.push(opcode::RETURN_OP);
        self.data.push(opcode::PACKAGE_OP);
        self.data
            .extend_from_slice(&Self::encode_self_pkg_length(body.len()));
        self.data.extend_from_slice(&body);
        self
    }

    /// Start an `If (<name>)` block whose predicate is the value of a named object
    /// (a `TermArg` that evaluates the integer the name holds, e.g. the `PICF`
    /// interrupt-model flag). Returns a handle to close with [`Self::if_end`].
    ///
    /// `If` is `IfOp PkgLength Predicate TermList`; the `PkgLength` spans the
    /// predicate and the body, exactly like a scope, so the shared
    /// [`Self::patch_pkg_length`] closes it.
    pub fn if_name_start(&mut self, name: &[u8; 4]) -> ScopeHandle {
        self.data.push(opcode::IF_OP);
        let length_pos = self.data.len();
        self.data.extend_from_slice(&[0, 0, 0, 0]);
        // Predicate: a bare NameString references the object and yields its value.
        self.data.extend_from_slice(&Self::encode_name(*name));
        ScopeHandle {
            length_pos,
            content_start: self.data.len(),
        }
    }

    /// Close an `If` block opened with [`Self::if_name_start`].
    pub fn if_end(&mut self, handle: &ScopeHandle) {
        self.patch_pkg_length(handle.length_pos, handle.content_start);
    }

    /// `OperationRegion(name, space, offset, length)` — declare a region the
    /// `Field` below names into. Used to expose the PIIX3 PCI-config PIRQ
    /// route-control bytes so the link devices can read/write live routing.
    /// `DefOpRegion := ExtOpPrefix OpRegionOp NameString RegionSpace
    /// RegionOffset RegionLen` — no `PkgLength` (ACPI 6.x §20.2.5.2).
    pub fn operation_region(
        &mut self,
        name: &[u8; 4],
        space: u8,
        offset: u64,
        length: u64,
    ) -> &mut Self {
        self.data.push(opcode::EXT_OP_PREFIX);
        self.data.push(opcode::OPERATION_REGION_OP);
        self.data.extend_from_slice(&Self::encode_name(*name));
        self.data.push(space);
        self.data
            .extend_from_slice(&Self::encode_integer_const(offset));
        self.data
            .extend_from_slice(&Self::encode_integer_const(length));
        self
    }

    /// `Field(region, ByteAcc, NoLock, Preserve){ ... }` — name sub-fields into a
    /// region declared with [`Self::operation_region`]. Each entry is
    /// `(name, bit_width)`; `name` is `None` for a reserved gap that advances the
    /// bit offset without naming it. `DefField := ExtOpPrefix FieldOp PkgLength
    /// NameString FieldFlags FieldList`; the `PkgLength` spans the name, flags and
    /// field list (ACPI 6.x §20.2.5.2). Field flags `0x01` = `ByteAcc`/`NoLock`/
    /// `Preserve`, the PIIX/ICH convention for the byte-wide PIRQ registers.
    pub fn field(&mut self, region: &[u8; 4], entries: &[(Option<[u8; 4]>, u32)]) -> &mut Self {
        let mut body = Vec::new();
        body.extend_from_slice(&Self::encode_name(*region));
        body.push(0x01); // FieldFlags: ByteAcc, NoLock, Preserve
        for (name, bits) in entries {
            match name {
                Some(seg) => body.extend_from_slice(seg),
                None => body.push(0x00), // ReservedField marker
            }
            // The field-element length uses the PkgLength encoding for its bit count.
            body.extend_from_slice(&Self::encode_pkg_length(*bits as usize));
        }
        self.data.push(opcode::EXT_OP_PREFIX);
        self.data.push(opcode::FIELD_OP);
        self.data
            .extend_from_slice(&Self::encode_self_pkg_length(body.len()));
        self.data.extend_from_slice(&body);
        self
    }

    /// `Store(source, target)` — `StoreOp TermArg SuperName` (ACPI 6.x §20.2.5.4).
    pub fn store(&mut self, source: Operand, target: Operand) -> &mut Self {
        self.data.push(opcode::STORE_OP);
        self.data.extend_from_slice(&source.encode());
        self.data.extend_from_slice(&target.encode());
        self
    }

    /// Emit a dyadic operator `op a, b, target` (`And`/`Or`/`ShiftLeft`/
    /// `Subtract` — `Op Operand Operand Target`).
    fn dyadic(&mut self, op: u8, a: Operand, b: Operand, target: Operand) -> &mut Self {
        self.data.push(op);
        self.data.extend_from_slice(&a.encode());
        self.data.extend_from_slice(&b.encode());
        self.data.extend_from_slice(&target.encode());
        self
    }

    /// `And(a, b, target)`.
    pub fn and_op(&mut self, a: Operand, b: Operand, target: Operand) -> &mut Self {
        self.dyadic(opcode::AND_OP, a, b, target)
    }

    /// `Or(a, b, target)`.
    pub fn or_op(&mut self, a: Operand, b: Operand, target: Operand) -> &mut Self {
        self.dyadic(opcode::OR_OP, a, b, target)
    }

    /// `ShiftLeft(value, count, target)`.
    pub fn shift_left(&mut self, value: Operand, count: Operand, target: Operand) -> &mut Self {
        self.dyadic(opcode::SHIFT_LEFT_OP, value, count, target)
    }

    /// `Subtract(a, b, target)`.
    pub fn subtract(&mut self, a: Operand, b: Operand, target: Operand) -> &mut Self {
        self.dyadic(opcode::SUBTRACT_OP, a, b, target)
    }

    /// `FindSetRightBit(source, target)` — `FindSetRightBitOp Operand Target`.
    /// Returns the 1-based index of the least-significant set bit (0 if none),
    /// the standard way a `_SRS` turns an IRQ mask into an IRQ number.
    pub fn find_set_right_bit(&mut self, source: Operand, target: Operand) -> &mut Self {
        self.data.push(opcode::FIND_SET_RIGHT_BIT_OP);
        self.data.extend_from_slice(&source.encode());
        self.data.extend_from_slice(&target.encode());
        self
    }

    /// `CreateWordField(buffer, byte_index, name)` — overlay a 16-bit field on a
    /// buffer (`CreateWordFieldOp SourceBuff ByteIndex NameString`). Used to reach
    /// the 16-bit IRQ mask inside an `_SRS`/`_CRS` resource buffer.
    pub fn create_word_field(
        &mut self,
        buffer: Operand,
        byte_index: u64,
        name: &[u8; 4],
    ) -> &mut Self {
        self.data.push(opcode::CREATE_WORD_FIELD_OP);
        self.data.extend_from_slice(&buffer.encode());
        self.data
            .extend_from_slice(&Self::encode_integer_const(byte_index));
        self.data.extend_from_slice(&Self::encode_name(*name));
        self
    }

    /// `Return(<name>)` — return the value of a named object (e.g. a `_CRS`
    /// resource buffer built in the method body).
    pub fn return_name(&mut self, name: &[u8; 4]) -> &mut Self {
        self.data.push(opcode::RETURN_OP);
        self.data.extend_from_slice(&Self::encode_name(*name));
        self
    }

    /// `Name(name, ResourceTemplate{ ... })` — emit a `_CRS`/`_PRS`-style buffer.
    ///
    /// Appends the End Tag (`0x79`) with a zero checksum byte (ACPI treats `0` as
    /// "valid, checksum not computed") and wraps the descriptors in a `Buffer`
    /// whose `BufferSize` is the total descriptor byte count, per ACPI 6.x §6.4 /
    /// §19.6.114. `ResourceTemplate` in ASL is just sugar for such a buffer.
    pub fn name_resource_template(&mut self, name: &[u8; 4], rt: &ResourceTemplate) -> &mut Self {
        self.data.push(opcode::NAME_OP);
        self.data.extend_from_slice(&Self::encode_name(*name));

        // Body = descriptors + End Tag + checksum byte.
        let mut body = rt.as_bytes().to_vec();
        body.push(0x79);
        body.push(0x00);

        // BufferOp PkgLength BufferSize ByteList. The PkgLength counts the
        // BufferSize term + the byte list + itself.
        let size_arg = Self::encode_integer_arg(body.len());
        self.data.push(opcode::BUFFER_OP);
        let pkg = Self::encode_self_pkg_length(size_arg.len() + body.len());
        self.data.extend_from_slice(&pkg);
        self.data.extend_from_slice(&size_arg);
        self.data.extend_from_slice(&body);
        self
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

/// Resource Type field of an Address Space descriptor: a memory range.
const ADDR_MEMORY: u8 = 0;
/// Resource Type field of an Address Space descriptor: an I/O range.
const ADDR_IO: u8 = 1;
/// Resource Type field of an Address Space descriptor: a bus-number range.
const ADDR_BUS: u8 = 2;

/// Accumulates ACPI resource descriptors for a `ResourceTemplate` buffer.
///
/// Descriptors follow ACPI spec §6.4 and feed a device's `_CRS`/`_PRS`. The End
/// Tag is appended by [`AmlBuilder::name_resource_template`], so callers only
/// push the resources a device actually consumes.
#[derive(Default)]
pub struct ResourceTemplate {
    data: Vec<u8>,
}

impl ResourceTemplate {
    #[must_use]
    pub const fn new() -> Self {
        Self { data: Vec::new() }
    }

    /// Fixed I/O Port Descriptor (small type `0x47`) for a `length`-byte port
    /// window at `base`, decoding the full 16-bit ISA address space. This is the
    /// common case for legacy devices (COM, RTC, keyboard).
    pub fn io_port(&mut self, base: u16, length: u8) -> &mut Self {
        self.io_port_range(base, base, 1, length, true)
    }

    /// General I/O Port Descriptor (small type `0x47`, ACPI §6.4.2.5). `decode16`
    /// selects 16-bit (vs. 10-bit) ISA address decoding.
    pub fn io_port_range(
        &mut self,
        min: u16,
        max: u16,
        align: u8,
        length: u8,
        decode16: bool,
    ) -> &mut Self {
        self.data.push(0x47);
        self.data.push(u8::from(decode16)); // Information: bit0 = _DEC (16-bit decode)
        self.data.extend_from_slice(&min.to_le_bytes());
        self.data.extend_from_slice(&max.to_le_bytes());
        self.data.push(align);
        self.data.push(length);
        self
    }

    /// IRQ Descriptor in its 3-byte form (small type `0x23`, ACPI §6.4.2.1) for a
    /// single edge-triggered, active-high, exclusive ISA interrupt.
    pub fn irq(&mut self, irq: u8) -> &mut Self {
        let mask: u16 = if irq < 16 { 1u16 << irq } else { 0 };
        self.data.push(0x23);
        self.data.extend_from_slice(&mask.to_le_bytes());
        self.data.push(0x01); // bit0=edge, bit3=exclusive, bit4=active-high
        self
    }

    /// IRQ Descriptor (small type `0x23`, ACPI §6.4.2.1) carrying a *set* of
    /// candidate IRQs and explicit mode/polarity/sharing flags. Used for a PCI
    /// interrupt link device's `_PRS` (every IRQ the link may be routed to) and
    /// `_CRS` (the single IRQ it currently drives). PCI interrupts are
    /// level-triggered, active-low and shared.
    pub fn irq_flags(
        &mut self,
        irqs: &[u8],
        edge: bool,
        active_low: bool,
        shared: bool,
    ) -> &mut Self {
        let mut mask: u16 = 0;
        for &irq in irqs {
            if irq < 16 {
                mask |= 1u16 << irq;
            }
        }
        self.data.push(0x23);
        self.data.extend_from_slice(&mask.to_le_bytes());
        // Information byte: bit0 = mode (1=edge, 0=level), bit3 = sharing
        // (1=shared), bit4 = polarity (1=active low).
        let mut flags = 0u8;
        if edge {
            flags |= 1 << 0;
        }
        if shared {
            flags |= 1 << 3;
        }
        if active_low {
            flags |= 1 << 4;
        }
        self.data.push(flags);
        self
    }

    /// 32-bit Fixed Memory Range Descriptor (large type `0x86`, ACPI §6.4.3.4).
    pub fn memory32_fixed(&mut self, base: u32, length: u32, writable: bool) -> &mut Self {
        self.data.push(0x86);
        self.data.extend_from_slice(&9u16.to_le_bytes()); // length of the descriptor body
        self.data.push(u8::from(writable)); // Information: bit0 = write status
        self.data.extend_from_slice(&base.to_le_bytes());
        self.data.extend_from_slice(&length.to_le_bytes());
        self
    }

    /// Word Address Space Descriptor (large type `0x88`, ACPI §6.4.3.5.3) for a
    /// bus-number window a host bridge *produces* for its child bus.
    pub fn word_bus_number(&mut self, min: u16, max: u16) -> &mut Self {
        let len = max - min + 1;
        // General flags 0x0C: producer (bit0=0), min/max fixed (_MIF/_MAF), positive
        // decode. Bus-number resources have no type-specific flags.
        self.data.push(0x88);
        self.data.extend_from_slice(&13u16.to_le_bytes()); // body length: 3 + 5×2
        self.data.push(ADDR_BUS);
        self.data.push(0x0C);
        self.data.push(0x00);
        for f in [0u16, min, max, 0, len] {
            self.data.extend_from_slice(&f.to_le_bytes());
        }
        self
    }

    /// Word Address Space Descriptor (large type `0x88`) for an I/O port window a
    /// host bridge produces for its child bus (entire range, static translation).
    pub fn word_io(&mut self, min: u16, max: u16) -> &mut Self {
        let len = max - min + 1;
        self.data.push(0x88);
        self.data.extend_from_slice(&13u16.to_le_bytes());
        self.data.push(ADDR_IO);
        self.data.push(0x0C); // producer, min/max fixed, positive decode
        self.data.push(0x03); // type flags: entire range (ISA + non-ISA)
        for f in [0u16, min, max, 0, len] {
            self.data.extend_from_slice(&f.to_le_bytes());
        }
        self
    }

    /// `DWord` Address Space Descriptor (large type `0x87`, ACPI §6.4.3.5.2) for a
    /// 32-bit memory window a host bridge produces (e.g. the PCI MMIO hole).
    pub fn dword_memory(&mut self, base: u32, length: u32, writable: bool) -> &mut Self {
        let max = base + length - 1;
        self.data.push(0x87);
        self.data.extend_from_slice(&23u16.to_le_bytes()); // body length: 3 + 5×4
        self.data.push(ADDR_MEMORY);
        self.data.push(0x0C); // producer, min/max fixed, positive decode
        self.data.push(u8::from(writable)); // type flags: bit0 = write status
        for f in [0u32, base, max, 0, length] {
            self.data.extend_from_slice(&f.to_le_bytes());
        }
        self
    }

    /// `QWord` Address Space Descriptor (large type `0x8A`, ACPI §6.4.3.5.1) for a
    /// 64-bit memory window a host bridge produces (the high PCI MMIO hole).
    pub fn qword_memory(&mut self, base: u64, length: u64, writable: bool) -> &mut Self {
        let max = base + length - 1;
        self.data.push(0x8A);
        self.data.extend_from_slice(&43u16.to_le_bytes()); // body length: 3 + 5×8
        self.data.push(ADDR_MEMORY);
        self.data.push(0x0C); // producer, min/max fixed, positive decode
        self.data.push(u8::from(writable)); // type flags: bit0 = write status
        for f in [0u64, base, max, 0, length] {
            self.data.extend_from_slice(&f.to_le_bytes());
        }
        self
    }

    /// The accumulated descriptor bytes (without the End Tag).
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        self.data.as_slice()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
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
    fn name_package_encodes_elements_and_count() {
        let mut aml = AmlBuilder::new();
        aml.name_package(b"_S5_", &[5, 5, 0, 0]);
        let bytes = aml.into_bytes();

        assert_eq!(bytes[0], opcode::NAME_OP);
        assert_eq!(&bytes[1..5], b"_S5_");
        assert_eq!(bytes[5], opcode::PACKAGE_OP);

        let (pkg_val, pkg_field) = decode_pkg_length(&bytes[6..]);
        assert_eq!(
            pkg_val,
            bytes.len() - 6,
            "package PkgLength is self-consistent"
        );

        // NumElements, then the four element encodings: BYTE_PREFIX 5, BYTE_PREFIX 5,
        // ZERO, ZERO.
        let num_pos = 6 + pkg_field;
        assert_eq!(bytes[num_pos], 4, "four elements");
        assert_eq!(&bytes[num_pos + 1..], &[0x0A, 0x05, 0x0A, 0x05, 0x00, 0x00]);
    }

    #[test]
    fn name_routing_table_nests_packages() {
        let mut aml = AmlBuilder::new();
        // Two _PRT rows: slot 0 INTA -> GSI 16, slot 0 INTB -> GSI 17.
        let entries = [[0x0000_FFFF, 0, 0, 16], [0x0000_FFFF, 1, 0, 17]];
        aml.name_routing_table(b"_PRT", &entries);
        let bytes = aml.into_bytes();

        assert_eq!(bytes[0], opcode::NAME_OP);
        assert_eq!(&bytes[1..5], b"_PRT");
        assert_eq!(bytes[5], opcode::PACKAGE_OP);
        let (val, field) = decode_pkg_length(&bytes[6..]);
        assert_eq!(
            val,
            bytes.len() - 6,
            "outer _PRT package is self-consistent"
        );

        // NumElements = 2, then the first sub-package begins with PACKAGE_OP.
        let num_pos = 6 + field;
        assert_eq!(bytes[num_pos], 2);
        assert_eq!(bytes[num_pos + 1], opcode::PACKAGE_OP);

        // The first sub-package's own PkgLength is self-consistent.
        let (sub_val, sub_field) = decode_pkg_length(&bytes[num_pos + 2..]);
        let sub_start = num_pos + 2; // position of the sub PkgLength field
        let sub_end = sub_start + sub_val;
        assert!(sub_end <= bytes.len());
        // Sub NumElements = 4.
        assert_eq!(bytes[sub_start + sub_field], 4);
    }

    #[test]
    fn if_name_block_pkg_length_spans_predicate_and_body() {
        // Method (_PRT) { If (PICF) { Return (Package(){...}) } Return (Package(){...}) }
        let mut aml = AmlBuilder::new();
        let m = aml.method_start(b"_PRT", 0, false);
        let if_h = aml.if_name_start(b"PICF");
        aml.return_routing_table(&[[0x0000_FFFF, 0, 0, 16]]);
        aml.if_end(&if_h);
        aml.return_routing_table(&[[0x0000_FFFF, 0, 0, 11]]);
        aml.method_end(&m);
        let bytes = aml.into_bytes();

        // Method(_PRT, 0): METHOD_OP, PkgLength, name, flags.
        assert_eq!(bytes[0], opcode::METHOD_OP);
        let (mval, mfield) = decode_pkg_length(&bytes[1..]);
        assert_eq!(mval, bytes.len() - 1, "method PkgLength self-consistent");
        // After PkgLength field: name (4) + flags (1) = method body start.
        let body = 1 + mfield + 4 + 1;
        // The body opens with If (PICF): IF_OP, PkgLength, then "PICF".
        assert_eq!(bytes[body], opcode::IF_OP);
        let (ifval, iffield) = decode_pkg_length(&bytes[body + 1..]);
        // Predicate immediately follows the If PkgLength field.
        let pred = body + 1 + iffield;
        assert_eq!(&bytes[pred..pred + 4], b"PICF", "If predicate is PICF");
        // The If package spans the predicate + the inner Return; its end must land
        // before the trailing (else-branch) Return that follows in the method body.
        let if_end = body + 1 + ifval;
        assert_eq!(
            bytes[if_end],
            opcode::RETURN_OP,
            "fall-through Return follows If"
        );
        // The inner Return (APIC, GSI16) sits inside the If.
        assert_eq!(bytes[pred + 4], opcode::RETURN_OP);
        assert_eq!(bytes[pred + 5], opcode::PACKAGE_OP);
    }

    #[test]
    fn return_routing_table_via_links_encodes_a_named_source() {
        let mut aml = AmlBuilder::new();
        // slot 0 INTA -> \_SB.PCI0.ISA_.LNKA: { 0x0000FFFF, 0, \_SB.PCI0.ISA_.LNKA, 0 }.
        aml.return_routing_table_via_links(&[(
            0x0000_FFFF,
            0,
            &[*b"_SB_", *b"PCI0", *b"ISA_", *b"LNKA"],
        )]);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::RETURN_OP);
        assert_eq!(bytes[1], opcode::PACKAGE_OP);
        let (val, field) = decode_pkg_length(&bytes[2..]);
        assert_eq!(val, bytes.len() - 2, "outer package self-consistent");
        // NumElements 1, then a sub-package.
        let num = 2 + field;
        assert_eq!(bytes[num], 1);
        assert_eq!(bytes[num + 1], opcode::PACKAGE_OP);
        // The Source element is a rooted MultiName path \_SB.PCI0.ISA_.LNKA:
        // RootChar 0x5C, MultiNamePrefix 0x2F, SegCount 4, then the four segs.
        let seg = bytes
            .windows(20)
            .find(|w| {
                w[0] == 0x5C
                    && w[1] == 0x2F
                    && w[2] == 0x04
                    && w[3..7] == *b"_SB_"
                    && w[7..11] == *b"PCI0"
                    && w[11..15] == *b"ISA_"
                    && w[15..19] == *b"LNKA"
            })
            .expect("rooted multiname link path present");
        assert_eq!(seg[19], opcode::ZERO, "SourceIndex follows the link path");
    }

    #[test]
    fn encode_name_path_handles_single_dual_and_multi() {
        assert_eq!(AmlBuilder::encode_name_path(&[*b"LNKA"]), b"LNKA".to_vec());
        assert_eq!(
            AmlBuilder::encode_name_path(&[*b"ISA_", *b"LNKA"]),
            [&[0x2E][..], b"ISA_", b"LNKA"].concat()
        );
        assert_eq!(
            AmlBuilder::encode_name_path(&[*b"PCI0", *b"ISA_", *b"LNKA"]),
            [&[0x2F, 0x03][..], b"PCI0", b"ISA_", b"LNKA"].concat()
        );
    }

    #[test]
    fn irq_flags_descriptor_encodes_mask_and_mode() {
        // A PCI-link _PRS: level, active-low, shared over IRQs {10, 11}.
        let mut rt = ResourceTemplate::new();
        rt.irq_flags(&[10, 11], false, true, true);
        let b = rt.as_bytes();
        assert_eq!(b[0], 0x23);
        let mask = u16::from_le_bytes([b[1], b[2]]);
        assert_eq!(mask, (1 << 10) | (1 << 11));
        // flags: level (bit0=0), shared (bit3=1), active-low (bit4=1) => 0x18.
        assert_eq!(b[3], 0x18);
    }

    #[test]
    fn return_routing_table_emits_return_then_package() {
        let mut aml = AmlBuilder::new();
        aml.return_routing_table(&[[0x0001_FFFF, 1, 0, 10]]);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::RETURN_OP);
        assert_eq!(bytes[1], opcode::PACKAGE_OP);
        let (val, field) = decode_pkg_length(&bytes[2..]);
        assert_eq!(val, bytes.len() - 2, "returned package is self-consistent");
        // NumElements = 1, then a sub-package.
        assert_eq!(bytes[2 + field], 1);
        assert_eq!(bytes[2 + field + 1], opcode::PACKAGE_OP);
    }

    #[test]
    fn resource_template_descriptor_bytes() {
        // A COM1-style _CRS: I/O 0x3F8 len 8 + IRQ4.
        let mut rt = ResourceTemplate::new();
        rt.io_port(0x3F8, 8).irq(4);
        let bytes = rt.as_bytes();
        // I/O Port Descriptor: 0x47, info=1 (16-bit decode), min, max, align, len.
        assert_eq!(bytes[0], 0x47);
        assert_eq!(bytes[1], 0x01);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 0x3F8);
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 0x3F8);
        assert_eq!(bytes[6], 1); // alignment
        assert_eq!(bytes[7], 8); // length
        // IRQ Descriptor: 0x23, mask, flags.
        assert_eq!(bytes[8], 0x23);
        assert_eq!(u16::from_le_bytes([bytes[9], bytes[10]]), 1 << 4); // IRQ4
        assert_eq!(bytes[11], 0x01);
    }

    #[test]
    fn memory32_fixed_descriptor_bytes() {
        let mut rt = ResourceTemplate::new();
        rt.memory32_fixed(0xFED0_0000, 0x400, false);
        let b = rt.as_bytes();
        assert_eq!(b[0], 0x86); // large type 0x86
        assert_eq!(u16::from_le_bytes([b[1], b[2]]), 9); // body length
        assert_eq!(b[3], 0); // read-only
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 0xFED0_0000);
        assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 0x400);
        assert_eq!(b.len(), 12); // 3 header + 9 body
    }

    #[test]
    fn address_space_descriptor_bytes() {
        let mut rt = ResourceTemplate::new();
        rt.word_bus_number(0x00, 0xFF)
            .word_io(0x0000, 0x0CF7)
            .dword_memory(0xC000_0000, 0x3EC0_0000, true)
            .qword_memory(0x8_0000_0000, 0x80_0000_0000, true);
        let b = rt.as_bytes();

        // WordBusNumber: 0x88, len 13, restype 2 (bus), genflags 0x0C, typeflags 0,
        // gran 0, min 0, max 0xFF, xlat 0, len 0x100.
        assert_eq!(b[0], 0x88);
        assert_eq!(u16::from_le_bytes([b[1], b[2]]), 13);
        assert_eq!(b[3], 2);
        assert_eq!(b[4], 0x0C);
        assert_eq!(u16::from_le_bytes([b[8], b[9]]), 0x00); // min
        assert_eq!(u16::from_le_bytes([b[10], b[11]]), 0xFF); // max
        assert_eq!(u16::from_le_bytes([b[14], b[15]]), 0x100); // len

        // Next descriptor: WordIO (3 header + 13 body = 16 bytes in).
        let io = &b[16..];
        assert_eq!(io[0], 0x88);
        assert_eq!(io[3], 1); // I/O
        assert_eq!(io[4], 0x0C);
        assert_eq!(io[5], 0x03); // entire range
        assert_eq!(u16::from_le_bytes([io[10], io[11]]), 0x0CF7); // max

        // DWordMemory: 16 (bus) + 16 (io) = 32 in. 4-byte fields: gran[6..10],
        // min[10..14], max[14..18], xlat[18..22], len[22..26].
        let mem = &b[32..];
        assert_eq!(mem[0], 0x87);
        assert_eq!(u16::from_le_bytes([mem[1], mem[2]]), 23);
        assert_eq!(mem[3], 0); // memory
        assert_eq!(mem[5], 0x01); // writable
        assert_eq!(
            u32::from_le_bytes([mem[10], mem[11], mem[12], mem[13]]),
            0xC000_0000
        ); // min
        assert_eq!(
            u32::from_le_bytes([mem[14], mem[15], mem[16], mem[17]]),
            0xFEBF_FFFF
        ); // max
        assert_eq!(
            u32::from_le_bytes([mem[22], mem[23], mem[24], mem[25]]),
            0x3EC0_0000
        ); // len

        // QWordMemory: 32 + (3 + 23) = 58 in. 8-byte fields: gran[6..14],
        // min[14..22].
        let q = &b[58..];
        assert_eq!(q[0], 0x8A);
        assert_eq!(u16::from_le_bytes([q[1], q[2]]), 43);
        assert_eq!(q[3], 0); // memory
        let qmin = u64::from_le_bytes([q[14], q[15], q[16], q[17], q[18], q[19], q[20], q[21]]);
        assert_eq!(qmin, 0x8_0000_0000);
    }

    #[test]
    fn name_resource_template_wraps_buffer_with_end_tag() {
        let mut aml = AmlBuilder::new();
        let mut rt = ResourceTemplate::new();
        rt.io_port(0x60, 1).io_port(0x64, 1).irq(1);
        aml.name_resource_template(b"_CRS", &rt);
        let bytes = aml.into_bytes();

        // Name(_CRS, Buffer(...))
        assert_eq!(bytes[0], opcode::NAME_OP);
        assert_eq!(&bytes[1..5], b"_CRS");
        assert_eq!(bytes[5], opcode::BUFFER_OP);

        // PkgLength is self-consistent (field value == bytes from field to end).
        let (pkg_val, pkg_field) = decode_pkg_length(&bytes[6..]);
        assert_eq!(pkg_val, bytes.len() - 6);

        // BufferSize term immediately follows the PkgLength field.
        let size_pos = 6 + pkg_field;
        // descriptors (8+8+4=20) + End Tag (2) = 22 bytes → a byte const.
        assert_eq!(bytes[size_pos], opcode::BYTE_PREFIX);
        let buf_size = usize::from(bytes[size_pos + 1]);
        let byte_list = &bytes[size_pos + 2..];
        assert_eq!(
            buf_size,
            byte_list.len(),
            "BufferSize must match the byte list"
        );

        // The byte list ends with the End Tag (0x79) + checksum (0x00).
        assert_eq!(byte_list[byte_list.len() - 2], 0x79);
        assert_eq!(byte_list[byte_list.len() - 1], 0x00);
    }

    #[test]
    fn device_with_crs_has_consistent_lengths() {
        // A device carrying a _CRS: the device PkgLength must still cover the whole
        // (variable-length) resource buffer exactly.
        let mut aml = AmlBuilder::new();
        let dev = aml.device_start(b"COM1");
        aml.name_string(b"_HID", "PNP0501");
        let mut rt = ResourceTemplate::new();
        rt.io_port(0x3F8, 8).irq(4);
        aml.name_resource_template(b"_CRS", &rt);
        aml.device_end(&dev);
        let bytes = aml.into_bytes();

        assert_eq!(bytes[0], opcode::EXT_OP_PREFIX);
        assert_eq!(bytes[1], opcode::DEVICE_OP);
        let (dev_val, _) = decode_pkg_length(&bytes[2..]);
        assert_eq!(dev_val, bytes.len() - 2);
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

    #[test]
    fn operation_region_encodes_space_offset_length() {
        let mut aml = AmlBuilder::new();
        aml.operation_region(b"PIRQ", opcode::REGION_SPACE_PCI_CONFIG, 0x60, 0x04);
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::EXT_OP_PREFIX);
        assert_eq!(bytes[1], opcode::OPERATION_REGION_OP);
        assert_eq!(&bytes[2..6], b"PIRQ");
        assert_eq!(bytes[6], 0x02); // PCI_Config
        // Offset 0x60 and length 0x04 as byte consts.
        assert_eq!(&bytes[7..9], &[opcode::BYTE_PREFIX, 0x60]);
        assert_eq!(bytes[9], opcode::BYTE_PREFIX);
        assert_eq!(bytes[10], 0x04);
    }

    #[test]
    fn field_encodes_named_bytes_with_self_consistent_pkg_length() {
        let mut aml = AmlBuilder::new();
        aml.field(
            b"PIRQ",
            &[
                (Some(*b"PIRA"), 8),
                (Some(*b"PIRB"), 8),
                (Some(*b"PIRC"), 8),
                (Some(*b"PIRD"), 8),
            ],
        );
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::EXT_OP_PREFIX);
        assert_eq!(bytes[1], opcode::FIELD_OP);
        let (pkg_val, pkg_field) = decode_pkg_length(&bytes[2..]);
        assert_eq!(pkg_val, bytes.len() - 2, "field PkgLength spans to end");
        let body = &bytes[2 + pkg_field..];
        assert_eq!(&body[0..4], b"PIRQ", "region name");
        assert_eq!(body[4], 0x01, "ByteAcc/NoLock/Preserve flags");
        // First named field: NameSeg "PIRA" then a 1-byte PkgLength of 8.
        assert_eq!(&body[5..9], b"PIRA");
        assert_eq!(body[9], 8);
    }

    #[test]
    fn store_and_dyadic_ops_encode_operands_and_target() {
        let mut aml = AmlBuilder::new();
        aml.store(Operand::Int(0), Operand::Name(b"PIRA"));
        aml.and_op(
            Operand::Name(b"PIRA"),
            Operand::Int(0x0F),
            Operand::Local(0),
        );
        aml.or_op(
            Operand::Name(b"PIRA"),
            Operand::Int(0x80),
            Operand::Name(b"PIRA"),
        );
        aml.shift_left(Operand::Int(1), Operand::Local(0), Operand::Name(b"IRQM"));
        aml.subtract(Operand::Local(0), Operand::Int(1), Operand::Local(0));
        aml.find_set_right_bit(Operand::Name(b"IRQM"), Operand::Local(0));
        let bytes = aml.into_bytes();

        // Store(Zero, PIRA): 0x70 0x00 'P' 'I' 'R' 'A'
        assert_eq!(bytes[0], opcode::STORE_OP);
        assert_eq!(bytes[1], opcode::ZERO);
        assert_eq!(&bytes[2..6], b"PIRA");
        // And(PIRA, 0x0F, Local0): 0x7B 'PIRA' 0x0A 0x0F 0x60
        let and_pos = 6;
        assert_eq!(bytes[and_pos], opcode::AND_OP);
        assert_eq!(&bytes[and_pos + 1..and_pos + 5], b"PIRA");
        assert_eq!(
            &bytes[and_pos + 5..and_pos + 7],
            &[opcode::BYTE_PREFIX, 0x0F]
        );
        assert_eq!(bytes[and_pos + 7], opcode::LOCAL0);
        // Spot-check the remaining opcodes are present in order.
        assert!(bytes.contains(&opcode::OR_OP));
        assert!(bytes.contains(&opcode::SHIFT_LEFT_OP));
        assert!(bytes.contains(&opcode::SUBTRACT_OP));
        assert!(bytes.contains(&opcode::FIND_SET_RIGHT_BIT_OP));
    }

    #[test]
    fn create_word_field_and_return_name_encode() {
        let mut aml = AmlBuilder::new();
        aml.create_word_field(Operand::Name(b"BUF0"), 1, b"IRQM");
        aml.return_name(b"BUF0");
        let bytes = aml.into_bytes();
        assert_eq!(bytes[0], opcode::CREATE_WORD_FIELD_OP);
        assert_eq!(&bytes[1..5], b"BUF0");
        assert_eq!(bytes[5], opcode::ONE); // byte index 1
        assert_eq!(&bytes[6..10], b"IRQM");
        assert_eq!(bytes[10], opcode::RETURN_OP);
        assert_eq!(&bytes[11..15], b"BUF0");
    }
}

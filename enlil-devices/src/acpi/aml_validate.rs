//! Strict structural AML validator — the oracle for BadAML-style fuzzing.
//!
//! Guests parse the AML bytecode this crate synthesizes (DSDT/SSDT). A malformed
//! `PkgLength`, a truncated name, or a buffer whose declared size disagrees with
//! its bytes is both a guest crash and a detection vector (a guest that can
//! fingerprint *how* our tables are broken can fingerprint *us*). This module is
//! a defensive, total re-parse of AML: it walks the byte stream the way a guest
//! interpreter walks it — opcode by opcode, `PkgLength` by `PkgLength` — and
//! rejects anything that is not well-formed.
//!
//! Design notes:
//!
//! * **Total**: every read is bounds-checked, every length uses checked
//!   arithmetic, and nesting is capped ([`MAX_NESTING_DEPTH`]). Arbitrary bytes
//!   — including adversarial mutations — can only produce `Err`, never a panic,
//!   an out-of-bounds read, or unbounded work. That totality is what the
//!   libFuzzer target (`fuzz/fuzz_targets/aml.rs`) and the deterministic
//!   mutation campaign (`tests/aml_badaml_fuzz.rs`) rely on.
//! * **Strict**: this validates the AML *dialect Enlil emits*, not all of ACPI.
//!   An opcode we never generate is rejected with [`AmlError::UnknownOpcode`];
//!   accepting unknown opcodes would require guessing their length, which is
//!   exactly how real interpreters get confused. Structural fields our builder
//!   always gets right (buffer sizes, package element counts, table checksums)
//!   are cross-checked, so a builder regression shows up here before a guest
//!   ever sees it.
//!
//! Grammar references are to the ACPI specification §20 (AML encoding).

use super::aml::opcode;

/// Maximum nesting depth accepted while walking `PkgLength`-framed blocks.
///
/// Real tables nest a handful deep (scope → device → method → if). The cap keeps
/// adversarial inputs (e.g. 10 000 nested `ScopeOp`s) from turning the recursive
/// walk into a stack overflow or a hang; anything deeper is rejected.
pub const MAX_NESTING_DEPTH: u8 = 64;

/// Size of an ACPI system-description-table header; [`validate_table_payload`]
//  strips this many bytes before validating the AML.
pub const ACPI_TABLE_HEADER_LEN: usize = 36;

/// Structural defects found while walking AML bytecode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmlError {
    /// The byte stream ended in the middle of an opcode, name, string, or
    /// `PkgLength`-framed block.
    Truncated,
    /// A `PkgLength` value runs past the end of the buffer, wraps around, or
    /// otherwise fails to describe a span inside the input.
    PkgLengthOverrun,
    /// `PkgLength`-framed blocks nested deeper than [`MAX_NESTING_DEPTH`].
    NestingTooDeep,
    /// A `NameSeg`/`NameString` contains a character outside the ACPI name
    /// alphabet, or a multi-name prefix declares zero segments.
    InvalidName,
    /// An opcode this validator does not know (Enlil never emits it).
    UnknownOpcode(u8),
    /// An `0x5B` extended opcode this validator does not know.
    UnknownExtOpcode(u8),
    /// An operand appeared where the grammar needs a constant, name, string,
    /// `ArgN`/`LocalN`, or null name.
    BadTermArg(u8),
    /// A `Name()` value is not an integer constant, string, buffer, or package.
    BadNameValue(u8),
    /// A field-list element is neither a named/reserved/access field.
    MalformedFieldList,
    /// A `BufferOp`'s declared `BufferSize` disagrees with its actual byte list.
    BufferSizeMismatch,
    /// A `PackageOp`'s declared element count disagrees with the elements found.
    PackageElementCountMismatch,
    /// A whole-table payload's header `Length` field disagrees with its size.
    TableLengthMismatch,
    /// A whole-table payload's checksum does not sum to zero.
    BadChecksum,
}

/// Validate a raw AML byte stream (e.g. a DSDT/SSDT payload with the 36-byte
/// table header stripped).
///
/// Returns `Ok(())` when the stream is a structurally well-formed `TermList` in
/// the dialect Enlil emits, or the first [`AmlError`] encountered. Never panics.
///
/// # Errors
///
/// Returns the [`AmlError`] describing the first structural defect found.
#[must_use = "the validation result must be checked"]
pub fn validate_aml(bytes: &[u8]) -> Result<(), AmlError> {
    let mut walker = Walker::new(bytes);
    walker.parse_term_list(bytes.len(), 0)
}

/// Validate a complete ACPI table (36-byte header + AML payload), as produced by
/// e.g. [`super::dsdt::DsdtBuilder::build`].
///
/// In addition to the AML structural checks this verifies the header `Length`
/// field matches the buffer size and that the table checksum sums to zero —
//  both are guest-visible integrity properties.
///
/// # Errors
///
/// Returns the [`AmlError`] describing the first structural defect found.
#[must_use = "the validation result must be checked"]
pub fn validate_table_payload(table: &[u8]) -> Result<(), AmlError> {
    if table.len() < ACPI_TABLE_HEADER_LEN {
        return Err(AmlError::Truncated);
    }
    let declared = u32::from_le_bytes([table[4], table[5], table[6], table[7]]) as usize;
    if declared != table.len() {
        return Err(AmlError::TableLengthMismatch);
    }
    let sum: u8 = table.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    if sum != 0 {
        return Err(AmlError::BadChecksum);
    }
    validate_aml(&table[ACPI_TABLE_HEADER_LEN..])
}

/// Returns `true` for an ACPI `LeadNameChar` (`A-Z`, `_`).
const fn is_lead_name_char(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'_')
}

/// Returns `true` for an ACPI `NameChar` (digit or `LeadNameChar`).
const fn is_name_char(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'A'..=b'Z' | b'_')
}

struct Walker<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// End offset of the innermost enclosing `PkgLength` block (or the whole
    /// buffer at the top level). Every read is bounded by this, so a truncated
    /// or overrunning nested block can never desync the walk: the cursor may
    /// not advance past the block its bytes belong to.
    limit: usize,
}

impl<'a> Walker<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            limit: bytes.len(),
        }
    }

    /// Peek at the next byte without consuming it.
    fn peek(&self) -> Result<u8, AmlError> {
        if self.pos >= self.limit {
            return Err(AmlError::Truncated);
        }
        self.bytes.get(self.pos).copied().ok_or(AmlError::Truncated)
    }

    /// Consume and return the next byte.
    fn next(&mut self) -> Result<u8, AmlError> {
        let b = self.peek()?;
        self.pos += 1;
        Ok(b)
    }

    /// Skip `n` bytes, rejecting overruns with checked arithmetic.
    fn skip(&mut self, n: usize) -> Result<(), AmlError> {
        let end = self.pos.checked_add(n).ok_or(AmlError::Truncated)?;
        if end > self.limit {
            return Err(AmlError::Truncated);
        }
        self.pos = end;
        Ok(())
    }

    /// Parse a `PkgLength` (ACPI §20.2.4) and return its value: the number of
    /// bytes from the field's own first byte to the end of the package.
    ///
    /// Bits 6-7 of the lead byte select the form: `00` is the single-byte form
    /// carrying 6 length bits; otherwise the lead byte carries the low 4 bits
    /// and 1-3 following bytes extend the value.
    fn parse_pkg_length(&mut self) -> Result<usize, AmlError> {
        let lead = self.next()?;
        let follow = usize::from(lead >> 6);
        let mut length = usize::from(lead & 0x3F);
        if follow > 0 {
            length = usize::from(lead & 0x0F);
            for i in 0..follow {
                let b = self.next()?;
                length |= usize::from(b) << (4 + 8 * i);
            }
        }
        Ok(length)
    }

    /// Parse a `PkgLength` and resolve it to the absolute end offset of the
    /// package body, verifying the span lies inside the enclosing block: a
    /// child block that overruns its parent contradicts the parent's
    /// `PkgLength` and is malformed.
    fn pkg_body_end(&mut self) -> Result<usize, AmlError> {
        let field_start = self.pos;
        let length = self.parse_pkg_length()?;
        let end = field_start
            .checked_add(length)
            .ok_or(AmlError::PkgLengthOverrun)?;
        // The body must lie inside the enclosing block and at/after the field
        // itself (a zero length would otherwise rewind the cursor).
        if end > self.limit || end < self.pos {
            return Err(AmlError::PkgLengthOverrun);
        }
        Ok(end)
    }

    /// Parse a `PkgLength`, resolve and validate the block end against the
    /// enclosing block, and narrow the read limit to the new block. Returns
    /// `(end, outer_limit)`; the caller parses the block's header fields and
    /// body under the narrowed limit, then restores `outer_limit`.
    ///
    /// Narrowing *here* (not after the header) matters: the name, flags, and
    /// predicate bytes belong to the block, so a block too small to hold them
    /// must read as truncated rather than spilling into the sibling.
    fn enter_block(&mut self) -> Result<(usize, usize), AmlError> {
        let end = self.pkg_body_end()?;
        let outer = std::mem::replace(&mut self.limit, end);
        Ok((end, outer))
    }

    /// Run `f` inside the `PkgLength`-framed block at the cursor: parse the
    /// length, narrow the read limit to the block, run `f` with the block's
    /// end offset, then restore the outer limit. Restoring unconditionally is
    /// safe: on error the walker is discarded by the caller.
    fn in_block<R>(
        &mut self,
        f: impl FnOnce(&mut Self, usize) -> Result<R, AmlError>,
    ) -> Result<R, AmlError> {
        let (end, outer) = self.enter_block()?;
        let result = f(self, end);
        self.limit = outer;
        result
    }

    /// Skip a NUL-terminated string, bounded by the enclosing block.
    fn skip_cstr(&mut self) -> Result<(), AmlError> {
        let rest = self
            .bytes
            .get(self.pos..self.limit)
            .ok_or(AmlError::Truncated)?;
        match rest.iter().position(|&b| b == 0) {
            Some(i) => {
                self.pos += i + 1;
                Ok(())
            }
            None => Err(AmlError::Truncated),
        }
    }

    /// Parse a `NameString`: optional `\`/`^` prefixes, then a single, dual
    /// (`0x2E`), or multi (`0x2F` + count) name.
    fn parse_name_string(&mut self) -> Result<(), AmlError> {
        while let 0x5C | 0x5E = self.peek()? {
            self.next()?;
        }
        match self.peek()? {
            0x2E => {
                self.next()?;
                self.parse_name_seg()?;
                self.parse_name_seg()?;
            }
            0x2F => {
                self.next()?;
                let count = self.next()?;
                if count == 0 {
                    return Err(AmlError::InvalidName);
                }
                for _ in 0..count {
                    self.parse_name_seg()?;
                }
            }
            b if is_lead_name_char(b) => self.parse_name_seg()?,
            _ => return Err(AmlError::InvalidName),
        }
        Ok(())
    }

    /// Parse one 4-character `NameSeg`, checking the ACPI name alphabet.
    fn parse_name_seg(&mut self) -> Result<(), AmlError> {
        let lead = self.next()?;
        if !is_lead_name_char(lead) {
            return Err(AmlError::InvalidName);
        }
        for _ in 0..3 {
            if !is_name_char(self.next()?) {
                return Err(AmlError::InvalidName);
            }
        }
        Ok(())
    }

    /// Parse one `TermArg`: an integer constant, string, name reference,
    /// `ArgN`/`LocalN`, or null name.
    fn parse_term_arg(&mut self) -> Result<(), AmlError> {
        let op = self.peek()?;
        match op {
            0x00 | 0x01 | 0x60..=0x6E => {
                self.next()?;
                Ok(())
            }
            0x0A => {
                self.next()?;
                self.skip(1)
            }
            0x0B => {
                self.next()?;
                self.skip(2)
            }
            0x0C => {
                self.next()?;
                self.skip(4)
            }
            0x0D => {
                self.next()?;
                self.skip_cstr()
            }
            0x0E => {
                self.next()?;
                self.skip(8)
            }
            b if b == 0x5C || b == 0x5E || b == 0x2E || b == 0x2F || is_lead_name_char(b) => {
                self.parse_name_string()
            }
            other => Err(AmlError::BadTermArg(other)),
        }
    }

    /// Parse an integer `TermArg` constant, returning its value when it is a
    /// plain constant (used to cross-check `BufferOp` sizes). A non-constant
    /// argument yields `None` without consuming any bytes.
    fn parse_const_arg(&mut self) -> Result<Option<u64>, AmlError> {
        let is_const = matches!(self.peek()?, 0x00 | 0x01 | 0x0A | 0x0B | 0x0C | 0x0E);
        if !is_const {
            return Ok(None);
        }
        let op = self.next()?;
        let value = match op {
            0x00 => 0,
            0x01 => 1,
            0x0A => u64::from(self.next()?),
            0x0B => {
                let lo = u64::from(self.next()?);
                let hi = u64::from(self.next()?);
                lo | (hi << 8)
            }
            0x0C => {
                let mut v = 0u64;
                for i in 0..4 {
                    v |= u64::from(self.next()?) << (8 * i);
                }
                v
            }
            0x0E => {
                let mut v = 0u64;
                for i in 0..8 {
                    v |= u64::from(self.next()?) << (8 * i);
                }
                v
            }
            _ => unreachable!("peeked const opcode changed under us"),
        };
        Ok(Some(value))
    }

    /// Parse an `If` predicate expression: either a plain `TermArg` (a bare
    /// name, constant, …) or a fixed-arity computational operator such as the
    /// `And(PIRx, 0x80, target)` our link-device `_STA` methods evaluate there.
    fn parse_predicate(&mut self) -> Result<(), AmlError> {
        let op = self.peek()?;
        match op {
            opcode::SUBTRACT_OP | opcode::SHIFT_LEFT_OP | opcode::AND_OP | opcode::OR_OP => {
                self.next()?;
                self.parse_term_arg()?;
                self.parse_term_arg()?;
                self.parse_term_arg()
            }
            opcode::FIND_SET_RIGHT_BIT_OP => {
                self.next()?;
                self.parse_term_arg()?;
                self.parse_term_arg()
            }
            _ => self.parse_term_arg(),
        }
    }

    /// Parse the value half of a `Name()` definition or a `Return()` object:
    /// an integer constant, string, name reference, `ArgN`/`LocalN`, buffer,
    /// or package.
    ///
    /// The value opcode has only been peeked, not consumed; buffer/package
    /// values consume it here because [`Self::parse_buffer_op`] and
    /// [`Self::parse_package_op`] require the opcode to already be consumed.
    fn parse_data_object(&mut self, depth: u8) -> Result<(), AmlError> {
        let op = self.peek()?;
        match op {
            0x11 => {
                self.next()?;
                self.parse_buffer_op()
            }
            0x12 => {
                self.next()?;
                self.parse_package_op(depth + 1)
            }
            // Constants, strings, name references, ArgN/LocalN, null name.
            _ => self.parse_term_arg(),
        }
    }

    /// Parse a `TermList` up to (not including) `end`. The caller narrows the
    /// read limit to `end` via [`Self::enter_block`] first (the top-level call
    /// starts with `limit == end`), so every nested read is already bounded.
    /// (The walker is discarded on error, so the limit only needs restoring
    /// on success.)
    fn parse_term_list(&mut self, end: usize, depth: u8) -> Result<(), AmlError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(AmlError::NestingTooDeep);
        }
        debug_assert_eq!(self.limit, end);
        while self.pos < end {
            self.parse_term_obj(depth)?;
        }
        debug_assert_eq!(self.pos, end);
        Ok(())
    }

    /// Parse one `TermObj`. The cursor may not advance past the enclosing
    /// block's end (`self.limit`): every primitive read enforces it.
    #[allow(clippy::too_many_lines)]
    fn parse_term_obj(&mut self, depth: u8) -> Result<(), AmlError> {
        let op = self.next()?;
        match op {
            // Bare constants and locals/args as expression statements.
            0x00 | 0x01 | 0x60..=0x6E => Ok(()),
            0x0A => self.skip(1),
            0x0B => self.skip(2),
            0x0C => self.skip(4),
            0x0D => self.skip_cstr(),
            0x0E => self.skip(8),

            // NameOp: NameString + data object.
            opcode::NAME_OP => {
                self.parse_name_string()?;
                self.parse_data_object(depth)
            }
            // ScopeOp: PkgLength NameString TermList.
            opcode::SCOPE_OP => self.in_block(|w, end| {
                w.parse_name_string()?;
                w.parse_term_list(end, depth + 1)
            }),
            // BufferOp: PkgLength BufferSize ByteList (opcode already consumed).
            opcode::BUFFER_OP => self.parse_buffer_op(),
            // PackageOp: PkgLength NumElements PackageElementList (opcode
            // already consumed); contents nest one level deeper.
            opcode::PACKAGE_OP => self.parse_package_op(depth + 1),
            // MethodOp: PkgLength NameString MethodFlags TermList.
            opcode::METHOD_OP => self.in_block(|w, end| {
                w.parse_name_string()?;
                w.next()?; // MethodFlags
                w.parse_term_list(end, depth + 1)
            }),
            // Extended opcodes behind 0x5B.
            opcode::EXT_OP_PREFIX => {
                let ext = self.next()?;
                match ext {
                    // OperationRegion: NameString RegionSpace RegionOffset RegionLen.
                    opcode::OPERATION_REGION_OP => {
                        self.parse_name_string()?;
                        self.next()?; // RegionSpace
                        self.parse_term_arg()?; // RegionOffset
                        self.parse_term_arg()?; // RegionLen
                        Ok(())
                    }
                    // FieldOp: PkgLength NameString FieldFlags FieldList.
                    opcode::FIELD_OP => self.in_block(|w, end| {
                        w.parse_name_string()?;
                        w.next()?; // FieldFlags
                        w.parse_field_list(end)
                    }),
                    // DeviceOp: PkgLength NameString ObjectList.
                    opcode::DEVICE_OP => self.in_block(|w, end| {
                        w.parse_name_string()?;
                        w.parse_term_list(end, depth + 1)
                    }),
                    // ProcessorOp: PkgLength NameString ProcID PblkAddr PblkLen ObjectList.
                    opcode::PROCESSOR_OP => self.in_block(|w, end| {
                        w.parse_name_string()?;
                        w.next()?; // ProcID (ByteData)
                        w.skip(4)?; // PblkAddr (DWordData)
                        w.skip(4)?; // PblkLen (DWordData)
                        w.parse_term_list(end, depth + 1)
                    }),
                    other => Err(AmlError::UnknownExtOpcode(other)),
                }
            }
            // Two-operand statements: StoreOp (TermArg SuperName),
            // FindSetRightBitOp (Operand Target), and NotifyOp (NotifyObject
            // NotifyValue) all parse as two term args.
            opcode::STORE_OP | opcode::FIND_SET_RIGHT_BIT_OP | opcode::NOTIFY_OP => {
                self.parse_term_arg()?;
                self.parse_term_arg()
            }
            // Dyadic math/logic ops: Operand Operand Target.
            opcode::SUBTRACT_OP | opcode::SHIFT_LEFT_OP | opcode::AND_OP | opcode::OR_OP => {
                self.parse_term_arg()?;
                self.parse_term_arg()?;
                self.parse_term_arg()
            }
            // Create{Word,Byte}FieldOp: SourceBuff ByteIndex NameString.
            opcode::CREATE_WORD_FIELD_OP | opcode::CREATE_BYTE_FIELD_OP => {
                self.parse_term_arg()?;
                self.parse_term_arg()?;
                self.parse_name_string()
            }
            // IfOp: PkgLength Predicate TermList. The predicate is usually a
            // TermArg (e.g. a bare name), but our link-device `_STA` methods
            // use a dyadic `And(PIRx, 0x80, ...)` expression there.
            opcode::IF_OP => self.in_block(|w, end| {
                w.parse_predicate()?;
                w.parse_term_list(end, depth + 1)
            }),
            // ElseOp: PkgLength TermList.
            opcode::ELSE_OP => self.in_block(|w, end| w.parse_term_list(end, depth + 1)),
            // ReturnOp: optional object follows.
            opcode::RETURN_OP => {
                if self.pos < self.limit && Self::starts_data_object(self.peek()?) {
                    self.parse_data_object(depth)?;
                }
                Ok(())
            }
            // A bare name reference used as an expression statement (never
            // emitted, but harmless and total to accept).
            b if b == 0x5C || b == 0x5E || b == 0x2E || b == 0x2F || is_lead_name_char(b) => {
                self.pos -= 1;
                self.parse_name_string()
            }
            other => Err(AmlError::UnknownOpcode(other)),
        }
    }

    /// Returns `true` for opcodes that can start a `Name()` value or a
    /// `Return()` object: constants, strings, buffers, packages, name
    /// references, and `ArgN`/`LocalN`.
    const fn starts_data_object(op: u8) -> bool {
        matches!(
            op,
            0x00 | 0x01
                | 0x0A
                | 0x0B
                | 0x0C
                | 0x0D
                | 0x0E
                | 0x11
                | 0x12
                | 0x5C
                | 0x5E
                | 0x2E
                | 0x2F
                | 0x60..=0x67
                | 0x68..=0x6E
        ) || is_lead_name_char(op)
    }

    /// Parse a `BufferOp` where the cursor is just past the opcode:
    /// `PkgLength BufferSize ByteList`, cross-checking the declared size.
    ///
    /// Requires the `BufferOp` opcode to already be consumed.
    fn parse_buffer_op(&mut self) -> Result<(), AmlError> {
        self.in_block(|w, end| {
            let declared = w.parse_const_arg()?;
            if w.pos > end {
                return Err(AmlError::PkgLengthOverrun);
            }
            if let Some(size) = declared {
                let actual = end - w.pos;
                // A declared size that does not fit in `usize` can never match a
                // buffer that fits in memory, so it is a mismatch either way.
                let declared_fits = usize::try_from(size).is_ok_and(|s| s == actual);
                if !declared_fits {
                    return Err(AmlError::BufferSizeMismatch);
                }
            }
            // The byte list is opaque resource bytes; the PkgLength bounds it.
            w.pos = end;
            Ok(())
        })
    }

    /// Parse a `PackageOp` where the cursor is just past the opcode:
    /// `PkgLength NumElements PackageElementList`, cross-checking the count.
    ///
    /// Requires the `PackageOp` opcode to already be consumed. `depth` is the
    /// nesting level of this package's contents (one deeper than the enclosing
    /// term); deeper nesting than [`MAX_NESTING_DEPTH`] is rejected so
    /// adversarial package-in-package streams cannot overflow the stack —
    /// package elements recurse here directly, never passing through
    /// [`Self::parse_term_list`]'s depth check.
    fn parse_package_op(&mut self, depth: u8) -> Result<(), AmlError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(AmlError::NestingTooDeep);
        }
        self.in_block(|w, end| {
            let declared = usize::from(w.next()?);
            let mut seen = 0usize;
            while w.pos < end {
                w.parse_package_element(depth)?;
                seen += 1;
                if seen > declared {
                    return Err(AmlError::PackageElementCountMismatch);
                }
            }
            if seen != declared {
                return Err(AmlError::PackageElementCountMismatch);
            }
            Ok(())
        })
    }

    /// Parse one package element: a data object, a nested package/buffer, or a
    /// name reference (used for `_PRT` link-device sources).
    ///
    /// `depth` is the nesting level of the enclosing package; nested
    /// packages/buffers consume their opcode here (only peeked so far).
    fn parse_package_element(&mut self, depth: u8) -> Result<(), AmlError> {
        let op = self.peek()?;
        match op {
            0x00 | 0x01 | 0x0A | 0x0B | 0x0C | 0x0D | 0x0E => self.parse_term_arg(),
            0x11 => {
                self.next()?;
                self.parse_buffer_op()
            }
            0x12 => {
                self.next()?;
                self.parse_package_op(depth + 1)
            }
            b if b == 0x5C || b == 0x5E || b == 0x2E || b == 0x2F || is_lead_name_char(b) => {
                self.parse_name_string()
            }
            other => Err(AmlError::BadNameValue(other)),
        }
    }

    /// Parse a `FieldOp` field list up to `end`: named fields
    /// (`NameSeg PkgLength`), reserved gaps (`0x00 PkgLength`), and access-type
    /// modifiers. The caller ([`Self::in_block`]) narrows the read limit to the
    /// field first, so a truncated element cannot read into the sibling.
    fn parse_field_list(&mut self, end: usize) -> Result<(), AmlError> {
        while self.pos < end {
            let b = self.peek()?;
            match b {
                // ReservedField: 0x00 PkgLength.
                0x00 => {
                    self.next()?;
                    self.parse_pkg_length()?;
                }
                // AccessField: 0x01 AccessType AccessAttributes.
                0x01 => {
                    self.next()?;
                    self.next()?;
                    self.next()?;
                }
                // ConnectField: 0x02 NameString (rare; accepted for totality).
                0x02 => {
                    self.next()?;
                    self.parse_name_string()?;
                }
                // ExtendedAccessField: 0x03 AccessType ExtendedAccessAttributes AccessLength.
                0x03 => {
                    self.next()?;
                    self.next()?;
                    self.next()?;
                    self.next()?;
                }
                // NamedField: NameSeg PkgLength.
                b if is_lead_name_char(b) => {
                    self.parse_name_seg()?;
                    self.parse_pkg_length()?;
                }
                _ => return Err(AmlError::MalformedFieldList),
            }
        }
        Ok(())
    }
}

/// Placeholder removed: name-start detection is inlined via explicit byte
/// matches and [`is_lead_name_char`] so the alphabet lives in one place.
#[cfg(test)]
mod tests {
    use super::super::aml::{AmlBuilder, ResourceTemplate};
    use super::*;

    fn valid_name_value() -> Vec<u8> {
        let mut b = AmlBuilder::new();
        b.name_integer(b"TEST", 0x1234);
        b.into_bytes()
    }

    #[test]
    fn accepts_builder_output() {
        assert_eq!(validate_aml(&valid_name_value()), Ok(()));
    }

    #[test]
    fn accepts_name_with_buffer_value() {
        // `Name(_CRS, Buffer(){ ... })`: the value opcode is only peeked by the
        // `Name()` parser, so the buffer parser must see a consistent cursor.
        // Regression test: the value path used to re-read the already-peeked
        // opcode as the `PkgLength`, desyncing the whole stream.
        let mut b = AmlBuilder::new();
        let mut rt = ResourceTemplate::new();
        rt.io_port(0x3F8, 8);
        b.name_resource_template(b"_CRS", &rt);
        assert_eq!(validate_aml(&b.into_bytes()), Ok(()));
    }

    #[test]
    fn accepts_name_with_package_value() {
        let mut b = AmlBuilder::new();
        b.name_package(b"_S3_", &[1, 1, 0, 0]);
        assert_eq!(validate_aml(&b.into_bytes()), Ok(()));
    }

    /// Self-inclusive `PkgLength` encoder for the nesting test (mirrors the
    /// builder's; totals here stay in the 2-byte form).
    fn self_pkg_len(content_len: usize) -> Vec<u8> {
        fn raw(total: usize) -> Vec<u8> {
            let lo = u8::try_from(total & 0xFF).expect("total fits in two bytes");
            if total < 0x3F {
                vec![lo]
            } else {
                let hi = u8::try_from((total >> 4) & 0xFF).expect("total fits in two bytes");
                vec![(lo & 0x0F) | (1 << 6), hi]
            }
        }
        for field_len in 1..=2 {
            let encoded = raw(content_len + field_len);
            if encoded.len() == field_len {
                return encoded;
            }
        }
        unreachable!("nesting test overflowed the 2-byte PkgLength form");
    }

    #[test]
    fn pkg_length_single_byte_form_carries_six_bits() {
        // PkgLength 0x1A = 26: the single-byte form (bits 6-7 clear) carries 6
        // length bits, not 4. A ScopeOp spanning 26 bytes: name + 21 body bytes.
        // (Decoding only 4 bits yields 10 and desyncs the stream — the exact
        // bug this validator shipped with before the DSDT caught it.)
        let mut bytes = vec![0x10, 0x1A, b'_', b'S', b'0', b'_'];
        bytes.extend_from_slice(&[0x00; 21]);
        assert_eq!(bytes.len(), 1 + 26);
        assert_eq!(validate_aml(&bytes), Ok(()));
    }

    #[test]
    fn rejects_deeply_nested_packages() {
        // 80 nested single-element packages. Package elements recurse through
        // the package parser directly (never through the term-list depth
        // check), so the package parser enforces the cap itself — without it
        // this input overflows the stack instead of returning an error.
        let mut inner = vec![0x12];
        inner.extend_from_slice(&self_pkg_len(2));
        inner.extend_from_slice(&[0x01, 0x00]); // 1 element: ZeroOp
        for _ in 1..80 {
            let mut outer = vec![0x12];
            outer.extend_from_slice(&self_pkg_len(1 + inner.len()));
            outer.push(0x01); // NumElements
            outer.extend_from_slice(&inner);
            inner = outer;
        }
        assert_eq!(validate_aml(&inner), Err(AmlError::NestingTooDeep));
    }

    #[test]
    fn rejects_truncated_pkg_length() {
        // ScopeOp whose PkgLength claims 6 bytes from a 3-byte buffer: the
        // span overruns the input.
        assert_eq!(
            validate_aml(&[0x10, 0x06, 0x5F]),
            Err(AmlError::PkgLengthOverrun)
        );
        // A ScopeOp whose PkgLength is consistent but whose name is cut off
        // mid-NameSeg is truncated.
        assert_eq!(
            validate_aml(&[0x10, 0x03, b'_', b'S']),
            Err(AmlError::Truncated)
        );
    }

    #[test]
    fn rejects_pkg_length_overrun() {
        // PkgLength of 0x40 (2-byte form: 0x40|1<<6) claims 64 bytes from a
        // 3-byte buffer.
        assert_eq!(
            validate_aml(&[0x10, 0x41, 0x04]),
            Err(AmlError::PkgLengthOverrun)
        );
    }

    #[test]
    fn rejects_too_deep_nesting() {
        // 80 nested scopes built with the real builder so every PkgLength is
        // consistent; only the depth cap trips.
        let mut b = AmlBuilder::new();
        let mut handles = Vec::new();
        for _ in 0..80 {
            handles.push(b.scope_start(b"_S0_"));
        }
        b.raw(&[0x00]); // innermost: ZeroOp
        for h in handles.iter().rev() {
            b.scope_end(h);
        }
        assert_eq!(validate_aml(&b.into_bytes()), Err(AmlError::NestingTooDeep));
    }

    #[test]
    fn rejects_bad_name_char() {
        // NameOp with a digit as the lead character of the NameSeg.
        let mut bytes = vec![0x08, b'1', b'A', b'B', b'C', 0x00];
        assert_eq!(validate_aml(&bytes), Err(AmlError::InvalidName));
        // Underscore lead is fine.
        bytes[1] = b'_';
        assert_eq!(validate_aml(&bytes), Ok(()));
    }

    #[test]
    fn rejects_buffer_size_mismatch() {
        // BufferOp: PkgLength=4 (field+size+1 byte), BufferSize=2, one byte.
        assert_eq!(
            validate_aml(&[0x11, 0x04, 0x0A, 0x02, 0xAA]),
            Err(AmlError::BufferSizeMismatch)
        );
        // Declared 1, actual 1: valid.
        assert_eq!(validate_aml(&[0x11, 0x04, 0x0A, 0x01, 0xAA]), Ok(()));
    }

    #[test]
    fn rejects_package_count_mismatch() {
        // PackageOp claims 2 elements, carries 1.
        assert_eq!(
            validate_aml(&[0x12, 0x04, 0x02, 0x0A, 0x01]),
            Err(AmlError::PackageElementCountMismatch)
        );
        // Claims 1, carries 1.
        assert_eq!(validate_aml(&[0x12, 0x04, 0x01, 0x0A, 0x01]), Ok(()));
    }

    #[test]
    fn empty_input_is_an_empty_term_list() {
        assert_eq!(validate_aml(&[]), Ok(()));
    }

    #[test]
    fn table_payload_checks_header_and_checksum() {
        let mut table = vec![0u8; 40];
        table[0..4].copy_from_slice(b"DSDT");
        let len = u32::try_from(table.len()).expect("test table fits in u32");
        table[4..8].copy_from_slice(&len.to_le_bytes());
        // No checksum fix-up yet: must fail.
        assert_eq!(validate_table_payload(&table), Err(AmlError::BadChecksum));
        let sum: u8 = table.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        table[9] = table[9].wrapping_sub(sum);
        // Empty AML payload validates.
        assert_eq!(validate_table_payload(&table), Ok(()));
        // Corrupt the length field.
        table[4] ^= 0xFF;
        assert_eq!(
            validate_table_payload(&table),
            Err(AmlError::TableLengthMismatch)
        );
    }
}

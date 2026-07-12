//! Intel VT-x (VMX) capability decoding.
//!
//! The bare-metal VMX backend (Phase 6.2) programs VMXON/VMCS regions directly.
//! Before it can, it must read the VMX capability MSRs to learn three things it
//! needs for every region: the VMCS revision identifier it stamps into the
//! region header, how many bytes the region occupies, and the memory type the
//! region must be mapped with. This module decodes `IA32_VMX_BASIC` into those
//! fields.
//!
//! Reading the MSR itself (`rdmsr`) is the backend's job on real hardware and is
//! deliberately kept out of this pure, host-testable layer — the seam stays
//! ISA-detail-only per LOCKED PRINCIPLE 2, but the *arithmetic* is verified on
//! the dev host against Intel SDM Vol. 3, Appendix A.1.

/// MSR index of `IA32_VMX_BASIC` (Intel SDM Vol. 3, Appendix A.1).
pub const IA32_VMX_BASIC: u32 = 0x480;

/// The memory type the processor requires for VMCS / VMXON regions and the
/// structures they reference (`IA32_VMX_BASIC` bits 53:50).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcsMemoryType {
    /// Strong uncacheable (type 0).
    Uncacheable,
    /// Write-back (type 6) — what modern processors report.
    WriteBack,
    /// Any other encoding; reserved by the current SDM.
    Other(u8),
}

/// A decoded `IA32_VMX_BASIC` capability MSR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmxBasic {
    raw: u64,
}

impl VmxBasic {
    /// Wrap a raw `IA32_VMX_BASIC` MSR value read via `rdmsr`.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self { raw }
    }

    /// The raw MSR value.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.raw
    }

    /// VMCS revision identifier (bits 30:0).
    ///
    /// The backend writes this into the first 31 bits of every VMXON and VMCS
    /// region before executing `VMXON` / `VMPTRLD`; a region with the wrong
    /// revision id is rejected by the processor.
    #[must_use]
    pub fn revision_id(self) -> u32 {
        // Bits 30:0; bit 31 is reserved and always zero, so the mask fits u32.
        u32::try_from(self.raw & 0x7FFF_FFFF).unwrap_or(u32::MAX)
    }

    /// Number of bytes a VMXON / VMCS region occupies (bits 44:32).
    ///
    /// The SDM guarantees this never exceeds 4096.
    #[must_use]
    pub fn region_size(self) -> u32 {
        u32::try_from((self.raw >> 32) & 0x1FFF).unwrap_or(u32::MAX)
    }

    /// Whether VMXON / VMCS region physical addresses are limited to 32 bits
    /// (bit 48). When `false`, they may use the processor's full
    /// physical-address width.
    #[must_use]
    pub const fn phys_addr_width_limited_to_32(self) -> bool {
        (self.raw >> 48) & 1 != 0
    }

    /// The memory type required for VMCS / VMXON regions (bits 53:50).
    #[must_use]
    pub fn memory_type(self) -> VmcsMemoryType {
        match (self.raw >> 50) & 0xF {
            0 => VmcsMemoryType::Uncacheable,
            6 => VmcsMemoryType::WriteBack,
            other => VmcsMemoryType::Other(u8::try_from(other).unwrap_or(0xFF)),
        }
    }

    /// Whether the `IA32_VMX_TRUE_*` control-capability MSRs are supported
    /// (bit 55). When set, the backend must consult the `TRUE` MSRs so it can
    /// clear default-1 control bits the processor actually allows to be zero.
    #[must_use]
    pub const fn supports_true_controls(self) -> bool {
        (self.raw >> 55) & 1 != 0
    }
}

/// MSR index of `IA32_VMX_PINBASED_CTLS` (pin-based VM-execution controls).
pub const IA32_VMX_PINBASED_CTLS: u32 = 0x481;
/// MSR index of `IA32_VMX_PROCBASED_CTLS` (primary processor-based controls).
pub const IA32_VMX_PROCBASED_CTLS: u32 = 0x482;
/// MSR index of `IA32_VMX_EXIT_CTLS` (VM-exit controls).
pub const IA32_VMX_EXIT_CTLS: u32 = 0x483;
/// MSR index of `IA32_VMX_ENTRY_CTLS` (VM-entry controls).
pub const IA32_VMX_ENTRY_CTLS: u32 = 0x484;
/// MSR index of `IA32_VMX_TRUE_PINBASED_CTLS`.
pub const IA32_VMX_TRUE_PINBASED_CTLS: u32 = 0x48D;
/// MSR index of `IA32_VMX_TRUE_PROCBASED_CTLS`.
pub const IA32_VMX_TRUE_PROCBASED_CTLS: u32 = 0x48E;
/// MSR index of `IA32_VMX_TRUE_EXIT_CTLS`.
pub const IA32_VMX_TRUE_EXIT_CTLS: u32 = 0x48F;
/// MSR index of `IA32_VMX_TRUE_ENTRY_CTLS`.
pub const IA32_VMX_TRUE_ENTRY_CTLS: u32 = 0x490;

/// A decoded VMX control-capability MSR (pin-based, processor-based, VM-exit,
/// or VM-entry controls; Intel SDM Vol. 3, Section 24.6.2 / Appendix A.3).
///
/// The low dword lists the *allowed 0-settings*: a bit set there means the
/// corresponding control **must be 1**. The high dword lists the *allowed
/// 1-settings*: a bit clear there means the control **must be 0**. The backend
/// runs a desired control word through [`adjust`](Self::adjust) to obtain the
/// exact value the processor will accept in the VMCS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmxControlCaps {
    raw: u64,
}

impl VmxControlCaps {
    /// Wrap a raw VMX control-capability MSR value.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self { raw }
    }

    /// The controls that must be 1 (low dword — the allowed 0-settings).
    #[must_use]
    pub fn required_ones(self) -> u32 {
        u32::try_from(self.raw & 0xFFFF_FFFF).unwrap_or(u32::MAX)
    }

    /// The controls that may be 1 (high dword — the allowed 1-settings).
    #[must_use]
    pub fn allowed_ones(self) -> u32 {
        u32::try_from(self.raw >> 32).unwrap_or(u32::MAX)
    }

    /// Adjust a desired control word to the exact value the processor accepts:
    /// force every required-1 control on, then drop every control the processor
    /// does not allow to be 1.
    #[must_use]
    pub fn adjust(self, desired: u32) -> u32 {
        (desired | self.required_ones()) & self.allowed_ones()
    }

    /// Whether control `bit` is permitted to be 1.
    #[must_use]
    pub fn may_set(self, bit: u32) -> bool {
        bit < 32 && self.allowed_ones() & (1 << bit) != 0
    }

    /// Whether control `bit` is forced to 1 (cannot be cleared).
    #[must_use]
    pub fn must_set(self, bit: u32) -> bool {
        bit < 32 && self.required_ones() & (1 << bit) != 0
    }
}

/// The width class of a VMCS field (component-encoding bits 14:13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcsFieldWidth {
    /// 16-bit field.
    Bits16,
    /// 64-bit field (has a separate "high" access for its upper 32 bits).
    Bits64,
    /// 32-bit field.
    Bits32,
    /// Natural-width field (32 or 64 bits, matching the CPU mode).
    Natural,
}

/// The class of state a VMCS field belongs to (component-encoding bits 11:10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmcsFieldType {
    /// A control field.
    Control,
    /// A read-only VM-exit information field.
    ReadOnlyData,
    /// Guest-state field.
    GuestState,
    /// Host-state field.
    HostState,
}

/// A VMCS component-field encoding — the operand `VMREAD` / `VMWRITE` take to
/// address one field within the current VMCS (Intel SDM Vol. 3, Section 24.11.2
/// and Appendix B).
///
/// The 32-bit encoding packs an access type (bit 0: full vs. the high half of a
/// 64-bit field), an index (bits 9:1), a [`VmcsFieldType`] (bits 11:10), and a
/// [`VmcsFieldWidth`] (bits 14:13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmcsField(u32);

impl VmcsField {
    // A curated set of the field encodings the VMX backend programs first.
    /// Guest CR3 (natural width, guest state).
    pub const GUEST_CR3: Self = Self(0x6802);
    /// Guest RSP (natural width, guest state).
    pub const GUEST_RSP: Self = Self(0x681C);
    /// Guest RIP (natural width, guest state).
    pub const GUEST_RIP: Self = Self(0x681E);
    /// Guest RFLAGS (natural width, guest state).
    pub const GUEST_RFLAGS: Self = Self(0x6820);
    /// Host RSP (natural width, host state).
    pub const HOST_RSP: Self = Self(0x6C14);
    /// Host RIP (natural width, host state).
    pub const HOST_RIP: Self = Self(0x6C16);
    /// The VMCS link pointer (64-bit guest state); set to all-ones when unused.
    pub const VMCS_LINK_POINTER: Self = Self(0x2800);
    /// The EPT pointer (64-bit control field).
    pub const EPT_POINTER: Self = Self(0x201A);

    /// Wrap a raw 32-bit VMCS field encoding.
    #[must_use]
    pub const fn from_encoding(encoding: u32) -> Self {
        Self(encoding)
    }

    /// The raw 32-bit encoding (the `VMREAD` / `VMWRITE` operand).
    #[must_use]
    pub const fn encoding(self) -> u32 {
        self.0
    }

    /// Whether this encoding addresses the high 32 bits of a 64-bit field
    /// (access type, bit 0).
    #[must_use]
    pub const fn is_high_access(self) -> bool {
        self.0 & 1 != 0
    }

    /// The field index within its (type, width) group (bits 9:1).
    #[must_use]
    pub fn index(self) -> u16 {
        // Nine bits, so the value always fits a u16.
        u16::try_from((self.0 >> 1) & 0x1FF).unwrap_or(0)
    }

    /// The field's state class (bits 11:10).
    #[must_use]
    pub const fn field_type(self) -> VmcsFieldType {
        match (self.0 >> 10) & 0x3 {
            0 => VmcsFieldType::Control,
            1 => VmcsFieldType::ReadOnlyData,
            2 => VmcsFieldType::GuestState,
            _ => VmcsFieldType::HostState,
        }
    }

    /// The field's width class (bits 14:13).
    #[must_use]
    pub const fn width(self) -> VmcsFieldWidth {
        match (self.0 >> 13) & 0x3 {
            0 => VmcsFieldWidth::Bits16,
            1 => VmcsFieldWidth::Bits64,
            2 => VmcsFieldWidth::Bits32,
            _ => VmcsFieldWidth::Natural,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IA32_VMX_BASIC, IA32_VMX_ENTRY_CTLS, IA32_VMX_PINBASED_CTLS, IA32_VMX_PROCBASED_CTLS,
        IA32_VMX_TRUE_PINBASED_CTLS, VmcsField, VmcsFieldType, VmcsFieldWidth, VmcsMemoryType,
        VmxBasic, VmxControlCaps,
    };

    #[test]
    fn msr_index_matches_sdm() {
        assert_eq!(IA32_VMX_BASIC, 0x480);
    }

    #[test]
    fn decodes_writeback_region_with_true_controls() {
        // rev=1, region_size=0x400, memory_type=6 (WB), true-controls set,
        // bit 48 clear.
        let raw = 1 | (0x400_u64 << 32) | (6_u64 << 50) | (1_u64 << 55);
        let basic = VmxBasic::from_raw(raw);
        assert_eq!(basic.revision_id(), 1);
        assert_eq!(basic.region_size(), 0x400);
        assert_eq!(basic.memory_type(), VmcsMemoryType::WriteBack);
        assert!(basic.supports_true_controls());
        assert!(!basic.phys_addr_width_limited_to_32());
        assert_eq!(basic.raw(), raw);
    }

    #[test]
    fn decodes_uncacheable_32bit_region_without_true_controls() {
        // rev=0x1234, region_size=4096, memory_type=0 (UC), bit 48 set.
        let raw = 0x1234 | (0x1000_u64 << 32) | (1_u64 << 48);
        let basic = VmxBasic::from_raw(raw);
        assert_eq!(basic.revision_id(), 0x1234);
        assert_eq!(basic.region_size(), 4096);
        assert_eq!(basic.memory_type(), VmcsMemoryType::Uncacheable);
        assert!(basic.phys_addr_width_limited_to_32());
        assert!(!basic.supports_true_controls());
    }

    #[test]
    fn revision_id_ignores_reserved_high_bits() {
        // Setting bit 31 and higher bits must not leak into the 31-bit rev id.
        let raw = 0x7FFF_FFFF | (1_u64 << 31) | (0xFF_u64 << 56);
        assert_eq!(VmxBasic::from_raw(raw).revision_id(), 0x7FFF_FFFF);
    }

    #[test]
    fn reserved_memory_type_is_reported_verbatim() {
        let raw = 5_u64 << 50;
        assert_eq!(
            VmxBasic::from_raw(raw).memory_type(),
            VmcsMemoryType::Other(5)
        );
    }

    #[test]
    fn control_msr_indices_match_sdm() {
        assert_eq!(IA32_VMX_PINBASED_CTLS, 0x481);
        assert_eq!(IA32_VMX_PROCBASED_CTLS, 0x482);
        assert_eq!(IA32_VMX_ENTRY_CTLS, 0x484);
        assert_eq!(IA32_VMX_TRUE_PINBASED_CTLS, 0x48D);
    }

    #[test]
    fn control_caps_split_low_and_high_dwords() {
        // low dword (required-1) = bit 1; high dword (allowed-1) = bits 1,2,3.
        let raw = 0b0010_u64 | (0b1110_u64 << 32);
        let caps = VmxControlCaps::from_raw(raw);
        assert_eq!(caps.required_ones(), 0b0010);
        assert_eq!(caps.allowed_ones(), 0b1110);
    }

    #[test]
    fn adjust_forces_required_and_drops_disallowed() {
        let raw = 0b0010_u64 | (0b1110_u64 << 32);
        let caps = VmxControlCaps::from_raw(raw);
        // Nothing desired: only the required bit 1 comes back.
        assert_eq!(caps.adjust(0b0000), 0b0010);
        // Desiring allowed bit 2 keeps it and still forces bit 1.
        assert_eq!(caps.adjust(0b0100), 0b0110);
        // Desiring disallowed bit 0 drops it; bit 1 is still forced.
        assert_eq!(caps.adjust(0b0001), 0b0010);
    }

    #[test]
    fn may_set_and_must_set_report_bit_constraints() {
        let raw = 0b0010_u64 | (0b1110_u64 << 32);
        let caps = VmxControlCaps::from_raw(raw);
        assert!(caps.must_set(1));
        assert!(!caps.must_set(2));
        assert!(caps.may_set(2));
        assert!(!caps.may_set(0));
        // Out-of-range bit indices never claim a constraint.
        assert!(!caps.may_set(40));
        assert!(!caps.must_set(40));
    }

    #[test]
    fn vmcs_field_decodes_natural_guest_and_host_state() {
        // GUEST_RIP: natural width, guest state, full access.
        let rip = VmcsField::GUEST_RIP;
        assert_eq!(rip.width(), VmcsFieldWidth::Natural);
        assert_eq!(rip.field_type(), VmcsFieldType::GuestState);
        assert!(!rip.is_high_access());
        assert_eq!(rip.encoding(), 0x681E);

        // HOST_RIP: natural width, host state.
        let host_rip = VmcsField::HOST_RIP;
        assert_eq!(host_rip.width(), VmcsFieldWidth::Natural);
        assert_eq!(host_rip.field_type(), VmcsFieldType::HostState);
    }

    #[test]
    fn vmcs_field_decodes_64bit_control_and_guest_fields() {
        // EPT_POINTER: 64-bit control field.
        let eptp = VmcsField::EPT_POINTER;
        assert_eq!(eptp.width(), VmcsFieldWidth::Bits64);
        assert_eq!(eptp.field_type(), VmcsFieldType::Control);

        // VMCS_LINK_POINTER: 64-bit guest-state field.
        let link = VmcsField::VMCS_LINK_POINTER;
        assert_eq!(link.width(), VmcsFieldWidth::Bits64);
        assert_eq!(link.field_type(), VmcsFieldType::GuestState);
    }

    #[test]
    fn vmcs_field_high_access_bit_selects_upper_dword() {
        // Setting bit 0 of a 64-bit field encoding selects its high half.
        let link_high = VmcsField::from_encoding(VmcsField::VMCS_LINK_POINTER.encoding() | 1);
        assert!(link_high.is_high_access());
        assert_eq!(link_high.width(), VmcsFieldWidth::Bits64);
    }

    #[test]
    fn vmcs_field_index_extracts_bits_9_1() {
        // Encoding 0x681E → index bits 9:1 = 0x0F.
        assert_eq!(VmcsField::GUEST_RIP.index(), 0x0F);
        // Encoding 0x681C (GUEST_RSP) → index 0x0E.
        assert_eq!(VmcsField::GUEST_RSP.index(), 0x0E);
    }
}

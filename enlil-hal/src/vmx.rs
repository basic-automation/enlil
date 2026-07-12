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
    /// The guest-physical address that caused an EPT violation /
    /// misconfiguration (64-bit read-only field).
    pub const GUEST_PHYSICAL_ADDRESS: Self = Self(0x2400);

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

/// Basic VM-exit reasons (Intel SDM Vol. 3, Appendix C). Only the reasons the
/// backend routes first are named; any other exit arrives as its raw number.
pub mod exit_reason {
    /// Exception or non-maskable interrupt.
    pub const EXCEPTION_OR_NMI: u16 = 0;
    /// External interrupt.
    pub const EXTERNAL_INTERRUPT: u16 = 1;
    /// Triple fault.
    pub const TRIPLE_FAULT: u16 = 2;
    /// Interrupt window (guest ready to take an interrupt).
    pub const INTERRUPT_WINDOW: u16 = 7;
    /// `CPUID` executed by the guest.
    pub const CPUID: u16 = 10;
    /// `HLT` executed by the guest.
    pub const HLT: u16 = 12;
    /// `VMCALL` executed by the guest (hypercall).
    pub const VMCALL: u16 = 18;
    /// Control-register access (`MOV` to/from CR0/CR3/CR4/CR8).
    pub const CR_ACCESS: u16 = 28;
    /// I/O instruction (`IN`/`OUT`/`INS`/`OUTS`).
    pub const IO_INSTRUCTION: u16 = 30;
    /// `RDMSR` executed by the guest.
    pub const RDMSR: u16 = 31;
    /// `WRMSR` executed by the guest.
    pub const WRMSR: u16 = 32;
    /// EPT violation (guest access not permitted by the EPT paging structures).
    pub const EPT_VIOLATION: u16 = 48;
    /// EPT misconfiguration (malformed EPT entry).
    pub const EPT_MISCONFIG: u16 = 49;
}

/// A decoded VMCS VM-exit reason field (Intel SDM Vol. 3, Section 24.9.1).
///
/// Bits 15:0 are the basic exit reason; bit 29 flags an exit taken in VMX root
/// operation (SMM); bit 31 flags a VM-entry failure rather than a genuine VM
/// exit. The backend reads the field with `VMREAD` of [`Self::ENCODING`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmxExitReason(u32);

impl VmxExitReason {
    /// The VMCS field encoding of the VM-exit reason (a 32-bit read-only field).
    pub const ENCODING: u32 = 0x4402;

    /// Wrap the raw exit-reason field read from the VMCS.
    #[must_use]
    pub const fn from_field(raw: u32) -> Self {
        Self(raw)
    }

    /// The basic exit reason (bits 15:0) — compare against [`exit_reason`].
    #[must_use]
    pub fn basic_reason(self) -> u16 {
        u16::try_from(self.0 & 0xFFFF).unwrap_or(0)
    }

    /// Whether the exit occurred in VMX root operation (bit 29; SMM only).
    #[must_use]
    pub const fn in_vmx_root(self) -> bool {
        self.0 & (1 << 29) != 0
    }

    /// Whether this record is a VM-entry failure rather than a true VM exit
    /// (bit 31). The basic reason then identifies why entry failed.
    #[must_use]
    pub const fn is_vm_entry_failure(self) -> bool {
        self.0 & (1 << 31) != 0
    }
}

/// A decoded I/O-instruction VM-exit qualification (Intel SDM Vol. 3,
/// Table 27-5), read from the exit-qualification field on an
/// [`IO_INSTRUCTION`](exit_reason::IO_INSTRUCTION) VM exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoExitQualification(u64);

impl IoExitQualification {
    /// The VMCS field encoding of the exit qualification (natural width,
    /// read-only).
    pub const ENCODING: u32 = 0x6400;

    /// Wrap the raw exit-qualification field.
    #[must_use]
    pub const fn from_qualification(raw: u64) -> Self {
        Self(raw)
    }

    /// The access size in bytes (bits 2:0): 1, 2, or 4.
    #[must_use]
    pub const fn access_size(self) -> u8 {
        match self.0 & 0x7 {
            0 => 1,
            1 => 2,
            // 3 encodes a 4-byte access; other values are reserved.
            _ => 4,
        }
    }

    /// Whether the instruction is `IN`/`INS` (a guest read) rather than
    /// `OUT`/`OUTS` (bit 3).
    #[must_use]
    pub const fn is_in(self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// Whether this is a string I/O instruction, `INS`/`OUTS` (bit 4).
    #[must_use]
    pub const fn is_string(self) -> bool {
        self.0 & (1 << 4) != 0
    }

    /// Whether the instruction carried a `REP` prefix (bit 5).
    #[must_use]
    pub const fn is_rep(self) -> bool {
        self.0 & (1 << 5) != 0
    }

    /// The I/O port number (bits 31:16).
    #[must_use]
    pub fn port(self) -> u16 {
        u16::try_from((self.0 >> 16) & 0xFFFF).unwrap_or(0)
    }
}

/// Build the arch-neutral [`VmExit`](crate::VmExit) for an I/O-instruction VM
/// exit from its qualification and the guest accumulator (`RAX`).
///
/// The accumulator supplies the `OUT` data, masked to the access size. String
/// (`INS`/`OUTS`) I/O returns `None`: it moves data through guest memory and the
/// backend emulates it separately rather than as a single port access.
#[must_use]
pub fn io_exit_to_vmexit(qual: IoExitQualification, rax: u32) -> Option<crate::VmExit> {
    if qual.is_string() {
        return None;
    }
    let port = qual.port();
    let size = qual.access_size();
    if qual.is_in() {
        Some(crate::VmExit::IoIn { port, size })
    } else {
        let mask = match size {
            1 => 0xFF,
            2 => 0xFFFF,
            _ => 0xFFFF_FFFF,
        };
        Some(crate::VmExit::IoOut {
            port,
            size,
            data: rax & mask,
        })
    }
}

/// Map a VM-exit reason that needs no further VMCS reads onto the arch-neutral
/// [`VmExit`](crate::VmExit).
///
/// Returns `None` for reasons that require extra decoding the caller performs
/// with the exit qualification and guest registers (I/O — see
/// [`io_exit_to_vmexit`]; EPT violations; `CPUID` / MSR access), or that are
/// simply not routed yet.
#[must_use]
pub fn simple_exit_to_vmexit(reason: VmxExitReason) -> Option<crate::VmExit> {
    // A VM-entry failure is never a normal guest exit; the caller must abort.
    if reason.is_vm_entry_failure() {
        return None;
    }
    match reason.basic_reason() {
        exit_reason::HLT => Some(crate::VmExit::Hlt),
        exit_reason::TRIPLE_FAULT => Some(crate::VmExit::Shutdown),
        _ => None,
    }
}

/// A decoded EPT-violation exit qualification (Intel SDM Vol. 3, Table 28-7),
/// read from the exit-qualification field on an
/// [`EPT_VIOLATION`](exit_reason::EPT_VIOLATION) VM exit.
///
/// The faulting guest-physical address is read separately from
/// [`VmcsField::GUEST_PHYSICAL_ADDRESS`]; the access *size* comes from decoding
/// the faulting instruction, so producing an [`MmioRead`](crate::VmExit::MmioRead)
/// / [`MmioWrite`](crate::VmExit::MmioWrite) is left to the caller once it has
/// both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EptViolationQualification(u64);

impl EptViolationQualification {
    /// Wrap the raw exit-qualification field (the same VMCS field as
    /// [`IoExitQualification::ENCODING`], reinterpreted for an EPT violation).
    #[must_use]
    pub const fn from_qualification(raw: u64) -> Self {
        Self(raw)
    }

    /// Whether the faulting access was a data read (bit 0).
    #[must_use]
    pub const fn was_read(self) -> bool {
        self.0 & 1 != 0
    }

    /// Whether the faulting access was a data write (bit 1).
    #[must_use]
    pub const fn was_write(self) -> bool {
        self.0 & (1 << 1) != 0
    }

    /// Whether the faulting access was an instruction fetch (bit 2).
    #[must_use]
    pub const fn was_instruction_fetch(self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// Whether the guest-physical page is readable under EPT (bit 3).
    #[must_use]
    pub const fn ept_readable(self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// Whether the guest-physical page is writable under EPT (bit 4).
    #[must_use]
    pub const fn ept_writable(self) -> bool {
        self.0 & (1 << 4) != 0
    }

    /// Whether the guest-physical page is executable under EPT (bit 5).
    #[must_use]
    pub const fn ept_executable(self) -> bool {
        self.0 & (1 << 5) != 0
    }

    /// Whether the guest-linear-address field is valid (bit 7). It is invalid
    /// for an EPT violation on a paging-structure walk with no linear address.
    #[must_use]
    pub const fn guest_linear_valid(self) -> bool {
        self.0 & (1 << 7) != 0
    }

    /// Whether the violation was on the final GPA translation rather than a
    /// guest paging-structure entry (bit 8 clear, with bit 7 valid).
    #[must_use]
    pub const fn is_final_translation(self) -> bool {
        self.guest_linear_valid() && self.0 & (1 << 8) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IA32_VMX_BASIC, IA32_VMX_ENTRY_CTLS, IA32_VMX_PINBASED_CTLS, IA32_VMX_PROCBASED_CTLS,
        EptViolationQualification, IA32_VMX_TRUE_PINBASED_CTLS, IoExitQualification, VmcsField,
        VmcsFieldType, VmcsFieldWidth, VmcsMemoryType, VmxBasic, VmxControlCaps, VmxExitReason,
        exit_reason, io_exit_to_vmexit, simple_exit_to_vmexit,
    };
    use crate::VmExit;

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

    #[test]
    fn exit_reason_encoding_is_read_only_32bit() {
        // 0x4402: type read-only-data (bits 11:10 = 1), width 32-bit (bits 14:13 = 2).
        let field = VmcsField::from_encoding(VmxExitReason::ENCODING);
        assert_eq!(field.field_type(), VmcsFieldType::ReadOnlyData);
        assert_eq!(field.width(), VmcsFieldWidth::Bits32);
    }

    #[test]
    fn exit_reason_decodes_basic_reason() {
        let hlt = VmxExitReason::from_field(u32::from(exit_reason::HLT));
        assert_eq!(hlt.basic_reason(), exit_reason::HLT);
        assert!(!hlt.is_vm_entry_failure());
        assert!(!hlt.in_vmx_root());

        let io = VmxExitReason::from_field(u32::from(exit_reason::IO_INSTRUCTION));
        assert_eq!(io.basic_reason(), 30);
    }

    #[test]
    fn exit_reason_flags_vm_entry_failure_and_root() {
        // Bit 31 set with basic reason EPT_VIOLATION, plus the VMX-root bit 29.
        let raw = u32::from(exit_reason::EPT_VIOLATION) | (1 << 31) | (1 << 29);
        let reason = VmxExitReason::from_field(raw);
        assert_eq!(reason.basic_reason(), exit_reason::EPT_VIOLATION);
        assert!(reason.is_vm_entry_failure());
        assert!(reason.in_vmx_root());
    }

    #[test]
    fn io_qualification_encoding_is_natural_read_only() {
        // 0x6400: read-only-data (bits 11:10 = 1), natural width (bits 14:13 = 3).
        let field = VmcsField::from_encoding(IoExitQualification::ENCODING);
        assert_eq!(field.field_type(), VmcsFieldType::ReadOnlyData);
        assert_eq!(field.width(), VmcsFieldWidth::Natural);
    }

    #[test]
    fn io_qualification_decodes_out_byte_to_port() {
        // OUT to port 0x3F8, 1 byte: size bits=0, direction bit 3 clear,
        // port in bits 31:16.
        let qual = IoExitQualification::from_qualification(0x03F8 << 16);
        assert_eq!(qual.access_size(), 1);
        assert!(!qual.is_in());
        assert!(!qual.is_string());
        assert_eq!(qual.port(), 0x3F8);
    }

    #[test]
    fn io_qualification_decodes_in_dword() {
        // IN from port 0xCF8, 4 bytes: size bits=3, direction bit 3 set.
        let qual = IoExitQualification::from_qualification((0x0CF8 << 16) | 0b1000 | 0b011);
        assert_eq!(qual.access_size(), 4);
        assert!(qual.is_in());
        assert_eq!(qual.port(), 0xCF8);
    }

    #[test]
    fn io_exit_maps_out_to_vmexit_masking_data() {
        // OUT 1 byte to 0x80 with RAX = 0xDEAD_BEEF → only the low byte is data.
        let qual = IoExitQualification::from_qualification(0x0080 << 16);
        assert_eq!(
            io_exit_to_vmexit(qual, 0xDEAD_BEEF),
            Some(VmExit::IoOut {
                port: 0x80,
                size: 1,
                data: 0xEF,
            })
        );
    }

    #[test]
    fn io_exit_maps_in_and_skips_string_io() {
        // IN dword from 0xCFC → IoIn.
        let in_qual = IoExitQualification::from_qualification((0x0CFC << 16) | 0b1000 | 0b011);
        assert_eq!(
            io_exit_to_vmexit(in_qual, 0),
            Some(VmExit::IoIn {
                port: 0xCFC,
                size: 4,
            })
        );
        // A string OUTS (bit 4 set) is not a single-port access.
        let str_qual = IoExitQualification::from_qualification((0x0070 << 16) | 0b1_0000);
        assert_eq!(io_exit_to_vmexit(str_qual, 0), None);
    }

    #[test]
    fn simple_exit_maps_hlt_and_triple_fault() {
        let hlt = VmxExitReason::from_field(u32::from(exit_reason::HLT));
        assert_eq!(simple_exit_to_vmexit(hlt), Some(VmExit::Hlt));

        let triple = VmxExitReason::from_field(u32::from(exit_reason::TRIPLE_FAULT));
        assert_eq!(simple_exit_to_vmexit(triple), Some(VmExit::Shutdown));
    }

    #[test]
    fn simple_exit_defers_qualification_and_entry_failures() {
        // I/O needs the qualification, so it is not a "simple" exit.
        let io = VmxExitReason::from_field(u32::from(exit_reason::IO_INSTRUCTION));
        assert_eq!(simple_exit_to_vmexit(io), None);
        // A VM-entry failure, even for HLT, never maps to a normal exit.
        let failed_hlt =
            VmxExitReason::from_field(u32::from(exit_reason::HLT) | (1 << 31));
        assert_eq!(simple_exit_to_vmexit(failed_hlt), None);
    }

    #[test]
    fn guest_physical_address_field_is_64bit_read_only() {
        // 0x2400: read-only-data (bits 11:10 = 1), 64-bit width (bits 14:13 = 1).
        let field = VmcsField::GUEST_PHYSICAL_ADDRESS;
        assert_eq!(field.field_type(), VmcsFieldType::ReadOnlyData);
        assert_eq!(field.width(), VmcsFieldWidth::Bits64);
    }

    #[test]
    fn ept_violation_decodes_write_to_mapped_readable_page() {
        // Write access (bit 1), page readable (bit 3) but not writable (bit 4),
        // guest-linear valid (bit 7) on the final translation (bit 8 clear).
        let qual = EptViolationQualification::from_qualification(0b1000_1010);
        assert!(qual.was_write());
        assert!(!qual.was_read());
        assert!(!qual.was_instruction_fetch());
        assert!(qual.ept_readable());
        assert!(!qual.ept_writable());
        assert!(qual.guest_linear_valid());
        assert!(qual.is_final_translation());
    }

    #[test]
    fn ept_violation_flags_instruction_fetch_and_paging_walk() {
        // Instruction fetch (bit 2), page executable (bit 5), guest-linear
        // valid (bit 7), but the access was to a paging-structure entry (bit 8).
        let qual = EptViolationQualification::from_qualification(0b1_1010_0100);
        assert!(qual.was_instruction_fetch());
        assert!(qual.ept_executable());
        assert!(qual.guest_linear_valid());
        // Bit 8 set → not the final translation.
        assert!(!qual.is_final_translation());
    }
}

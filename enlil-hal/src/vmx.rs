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

#[cfg(test)]
mod tests {
    use super::{IA32_VMX_BASIC, VmcsMemoryType, VmxBasic};

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
}

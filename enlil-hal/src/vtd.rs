//! Intel VT-d DMA-remapping register and translation-structure encodings.
//!
//! The pure ISA half of Phase 6.4 (T-6.16): every remapping-hardware register
//! offset, capability bit, root/context entry layout, invalidation descriptor
//! and second-stage page-table format the driver programs (Intel VT-d
//! Architecture Specification §9 "Translation Structure Formats" and §10
//! "Register Descriptions"). The driver that *writes* these through volatile
//! MMIO lives in `enlil-boot::iommu`; this module is the ISA seam (LOCKED
//! PRINCIPLE 2), so the encodings are shared by the bare-metal kernel and any
//! host-side tooling instead of being re-derived per backend.
//!
//! Everything here is pure and host-testable: no MMIO, no allocation — the
//! second-stage builder writes into a caller-provided buffer, mirroring
//! [`crate::npt`]'s buffer style.

// ---------------------------------------------------------------------------
// Remapping-hardware register offsets (VT-d spec §10.4)
// ---------------------------------------------------------------------------

/// Version register: the VT-d architecture revision.
pub const REG_VER: u64 = 0x00;
/// Capability register: what the unit supports.
pub const REG_CAP: u64 = 0x08;
/// Extended capability register.
pub const REG_ECAP: u64 = 0x10;
/// Global command register — write 1 to a bit to request the action.
pub const REG_GCMD: u64 = 0x18;
/// Global status register — mirrors the completed `GCMD` actions.
pub const REG_GSTS: u64 = 0x1C;
/// Root-entry table address register.
pub const REG_RTADDR: u64 = 0x20;
/// Context-command register (context-cache invalidation).
pub const REG_CCMD: u64 = 0x28;
/// IOTLB invalidate-register offset on hardware predating the ECAP `IRO`
/// field — the invalidate-address register sits 8 bytes below it.
pub const REG_IOTLB_LEGACY: u64 = 0xB0;

// ---------------------------------------------------------------------------
// Global command / status bits (write-1-to-set in GCMD, mirrored in GSTS)
// ---------------------------------------------------------------------------

/// Set Interrupt Remap Table Pointer.
pub const GCMD_SIRTP: u32 = 1 << 24;
/// Interrupt Remapping Enable.
pub const GCMD_IRE: u32 = 1 << 25;
/// Queued Invalidation Enable.
pub const GCMD_QIE: u32 = 1 << 26;
/// Set Root Table Pointer: latch `RTADDR` into the unit.
pub const GCMD_SRTP: u32 = 1 << 30;
/// Translation Enable: turn DMA remapping on.
pub const GCMD_TE: u32 = 1 << 31;

// ---------------------------------------------------------------------------
// Capability register (CAP, offset 0x08)
// ---------------------------------------------------------------------------

/// Number of DMA domains the unit can track: `2^(4 + 2*ND)` (CAP bits 2:0).
#[must_use]
pub const fn cap_num_domains(cap: u64) -> u64 {
    1 << (4 + 2 * (cap & 0x7))
}

/// Caching Mode (CAP bit 7): when set, domain ID 0 is architecturally
/// reserved, so software must not use it.
#[must_use]
pub const fn cap_caching_mode(cap: u64) -> bool {
    cap & (1 << 7) != 0
}

/// Supported Adjusted Guest Address Widths (CAP bits 12:8): bit 8 = 30-bit,
/// bit 9 = 39-bit, bit 10 = 48-bit, bit 11 = 57-bit, bit 12 = 64-bit.
#[must_use]
pub const fn cap_sagaw(cap: u64) -> u64 {
    (cap >> 8) & 0x1F
}

/// Maximum Guest Address Width, in bits (CAP bits 21:16, plus one).
#[must_use]
pub const fn cap_mgaw(cap: u64) -> u64 {
    ((cap >> 16) & 0x3F) + 1
}

/// 2 MiB second-stage super pages (CAP bit 34).
#[must_use]
pub const fn cap_super_page_2mib(cap: u64) -> bool {
    cap & (1 << 34) != 0
}

/// 1 GiB second-stage super pages (CAP bit 35).
#[must_use]
pub const fn cap_super_page_1gib(cap: u64) -> bool {
    cap & (1 << 35) != 0
}

/// Fault-recording registers (CAP bits 55:48, plus one).
#[must_use]
pub const fn cap_num_fault_regs(cap: u64) -> u64 {
    ((cap >> 48) & 0xFF) + 1
}

/// Page-Selective Invalidation support (CAP bit 59).
#[must_use]
pub const fn cap_page_selective_invalidation(cap: u64) -> bool {
    cap & (1 << 59) != 0
}

/// Page tables are coherent with the CPU caches (CAP bit 60) — software need
/// not flush them for the IOMMU's walker.
#[must_use]
pub const fn cap_coherent(cap: u64) -> bool {
    cap & (1 << 60) != 0
}

// ---------------------------------------------------------------------------
// Extended capability register (ECAP, offset 0x10)
// ---------------------------------------------------------------------------

/// Queued Invalidation support (ECAP bit 0).
#[must_use]
pub const fn ecap_queued_invalidation(ecap: u64) -> bool {
    ecap & 1 != 0
}

/// Device-IOTLB support (ECAP bit 1).
#[must_use]
pub const fn ecap_device_iotlb(ecap: u64) -> bool {
    ecap & (1 << 1) != 0
}

/// Interrupt Remapping support (ECAP bit 3).
#[must_use]
pub const fn ecap_interrupt_remapping(ecap: u64) -> bool {
    ecap & (1 << 3) != 0
}

/// Extended Interrupt Mode (ECAP bit 4).
#[must_use]
pub const fn ecap_extended_interrupt_mode(ecap: u64) -> bool {
    ecap & (1 << 4) != 0
}

/// Pass-Through translation type in context entries (ECAP bit 6).
#[must_use]
pub const fn ecap_pass_through(ecap: u64) -> bool {
    ecap & (1 << 6) != 0
}

/// Snoop Control in page-table entries (ECAP bit 7).
#[must_use]
pub const fn ecap_snoop_control(ecap: u64) -> bool {
    ecap & (1 << 7) != 0
}

/// IOTLB register-block offset (ECAP bits 17:8).
///
/// In 16-byte units from the register base — the invalidate-address register;
/// the invalidate register itself is 8 bytes above it. Hardware predating the
/// field reports 0, for which the legacy [`REG_IOTLB_LEGACY`] offset applies.
#[must_use]
pub const fn ecap_iotlb_reg_offset(ecap: u64) -> u64 {
    let iro = (ecap >> 8) & 0x3FF;
    if iro == 0 { REG_IOTLB_LEGACY } else { iro * 16 }
}

// ---------------------------------------------------------------------------
// Translation structures: root table, context tables (VT-d spec §9.1–§9.3)
// ---------------------------------------------------------------------------

/// Entries in a root table (one per PCI bus) or a context table (one per
/// device/function).
pub const TABLE_ENTRIES: usize = 256;
/// Bytes per root/context entry.
pub const ENTRY_BYTES: usize = 16;
/// Bytes per root/context table.
pub const TABLE_BYTES: usize = TABLE_ENTRIES * ENTRY_BYTES;
/// A 4 KiB page — the size and alignment of every VT-d table.
pub const PAGE_SIZE: u64 = 4096;
/// [`PAGE_SIZE`] as a `usize`, for buffer indexing without a lossy `as` cast.
const PAGE_SIZE_USIZE: usize = 4096;
/// A 2 MiB huge page — the super-page leaf granularity.
pub const HUGE_2MIB: u64 = 2 * 1024 * 1024;
/// Entries per page table.
const ENTRIES_PER_TABLE: u64 = 512;

/// Bits 51:12 of an entry carry a table or frame physical address.
pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Root-table slot for `bus` — the root table is indexed by bus number.
#[must_use]
pub const fn root_index(bus: u8) -> usize {
    bus as usize
}

/// Context-table slot for a device/function — indexed by the 8-bit devfn.
#[must_use]
pub const fn context_index(device: u8, function: u8) -> usize {
    (device as usize) * 8 + (function as usize)
}

/// Encode a root entry: present, pointing at a 4 KiB-aligned context table.
///
/// The address is masked to bits 51:12 — the caller must pass a 4 KiB-aligned
/// physical address (a misaligned one would encode the wrong table).
#[must_use]
pub const fn root_entry(context_table_phys: u64) -> [u64; 2] {
    [(context_table_phys & ADDR_MASK) | 1, 0]
}

/// Whether a root entry is present.
#[must_use]
pub const fn root_entry_present(entry: &[u64; 2]) -> bool {
    entry[0] & 1 != 0
}

/// The context-table physical address a root entry points at.
#[must_use]
pub const fn root_entry_ctp(entry: &[u64; 2]) -> u64 {
    entry[0] & ADDR_MASK
}

/// Adjusted Guest Address Width: how many page-table levels the second-stage
/// walk uses (VT-d spec §9.3, `AW` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressWidth {
    /// 30-bit AGAW: 2-level walk.
    Bits30,
    /// 39-bit AGAW: 3-level walk.
    Bits39,
    /// 48-bit AGAW: 4-level walk.
    Bits48,
    /// 57-bit AGAW: 5-level walk.
    Bits57,
    /// 64-bit AGAW: 6-level walk.
    Bits64,
}

impl AddressWidth {
    /// The address width in bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        match self {
            Self::Bits30 => 30,
            Self::Bits39 => 39,
            Self::Bits48 => 48,
            Self::Bits57 => 57,
            Self::Bits64 => 64,
        }
    }

    /// Page-table levels in the walk.
    #[must_use]
    pub const fn levels(self) -> u8 {
        match self {
            Self::Bits30 => 2,
            Self::Bits39 => 3,
            Self::Bits48 => 4,
            Self::Bits57 => 5,
            Self::Bits64 => 6,
        }
    }

    /// The `AW` field encoding.
    #[must_use]
    pub const fn encode(self) -> u64 {
        match self {
            Self::Bits30 => 0,
            Self::Bits39 => 1,
            Self::Bits48 => 2,
            Self::Bits57 => 3,
            Self::Bits64 => 4,
        }
    }
}

/// Pick the widest address width the hardware supports.
///
/// `sagaw` is the extracted field from [`cap_sagaw`]: bit 0 = 30-bit,
/// bit 1 = 39-bit, bit 2 = 48-bit, bit 3 = 57-bit, bit 4 = 64-bit.
#[must_use]
pub const fn choose_address_width(sagaw: u64) -> Option<AddressWidth> {
    if sagaw & (1 << 4) != 0 {
        Some(AddressWidth::Bits64)
    } else if sagaw & (1 << 3) != 0 {
        Some(AddressWidth::Bits57)
    } else if sagaw & (1 << 2) != 0 {
        Some(AddressWidth::Bits48)
    } else if sagaw & (1 << 1) != 0 {
        Some(AddressWidth::Bits39)
    } else if sagaw & (1 << 0) != 0 {
        Some(AddressWidth::Bits30)
    } else {
        None
    }
}

/// Context-entry Translation Type (`TT`, bits 3:2 — VT-d spec §9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslationType {
    /// Untranslated requests walk the second-stage tables at `SSPTPTR`;
    /// translated requests are blocked.
    SecondStage = 0,
    /// Device-IOTLB (ATS) path.
    DeviceIotlb = 1,
    /// Requests bypass translation (identity).
    PassThrough = 2,
}

/// Encode a 16-byte context entry: present, translating through domain `did`'s
/// second-stage tables at `slptptr_phys` (4 KiB-aligned) with address width
/// `aw` and translation type `tt`.
///
/// The pointer is masked to bits 63:12 — the caller must pass a 4 KiB-aligned
/// physical address.
#[must_use]
pub const fn context_entry(
    did: u16,
    slptptr_phys: u64,
    aw: AddressWidth,
    tt: TranslationType,
) -> [u64; 2] {
    let lo = 1 | ((tt as u64) << 2) | (slptptr_phys & ADDR_MASK);
    let hi = aw.encode() | ((did as u64) << 8);
    [lo, hi]
}

/// Whether a context entry is present.
#[must_use]
pub const fn context_entry_present(entry: &[u64; 2]) -> bool {
    entry[0] & 1 != 0
}

/// The translation type (`TT`) a context entry carries.
#[must_use]
pub const fn context_entry_tt(entry: &[u64; 2]) -> u64 {
    (entry[0] >> 2) & 0x3
}

/// The second-stage root physical address a context entry points at.
#[must_use]
pub const fn context_entry_slptptr(entry: &[u64; 2]) -> u64 {
    entry[0] & ADDR_MASK
}

/// The address-width (`AW`) encoding a context entry carries.
#[must_use]
pub const fn context_entry_aw(entry: &[u64; 2]) -> u64 {
    entry[1] & 0x7
}

/// The domain ID a context entry carries.
#[must_use]
pub const fn context_entry_did(entry: &[u64; 2]) -> u64 {
    (entry[1] >> 8) & 0xFFFF
}

// ---------------------------------------------------------------------------
// Second-stage page-table entries (VT-d spec §9.8)
// ---------------------------------------------------------------------------

/// Second-stage page-table entry flag bits.
pub mod sl {
    /// Readable.
    pub const READ: u64 = 1 << 0;
    /// Writable.
    pub const WRITE: u64 = 1 << 1;
    /// Executable.
    pub const EXECUTE: u64 = 1 << 2;
    /// Ignore PAT (bit 6).
    pub const IGNORE_PAT: u64 = 1 << 6;
    /// Page Size (bit 7) in a non-leaf entry: this entry is a large-page leaf.
    pub const PAGE_SIZE: u64 = 1 << 7;
    /// Snoop control (bit 7) in a leaf entry.
    pub const SNOOP: u64 = 1 << 7;
}

/// A non-leaf entry: full permissions, pointing at the next 4 KiB-aligned
/// table (masked to bits 51:12 — must be 4 KiB-aligned).
#[must_use]
pub const fn sl_table_entry(child_phys: u64) -> u64 {
    (child_phys & ADDR_MASK) | sl::READ | sl::WRITE | sl::EXECUTE
}

/// A leaf entry mapping `frame_phys`: read/write — DMA never executes, so
/// leaves deliberately omit [`sl::EXECUTE`] — with UC memory type (`EMT` = 0);
/// `large` selects a 2 MiB page (`PS`) over a 4 KiB one.
///
/// The frame is masked to bits 51:12 — it must be aligned to the leaf size.
#[must_use]
pub const fn sl_leaf_entry(frame_phys: u64, large: bool) -> u64 {
    let entry = (frame_phys & ADDR_MASK) | sl::READ | sl::WRITE;
    if large { entry | sl::PAGE_SIZE } else { entry }
}

// ---------------------------------------------------------------------------
// Invalidation descriptors (VT-d spec §10.4.7, §10.4.8)
// ---------------------------------------------------------------------------

/// Context-cache invalidate in progress (CCMD bit 63).
pub const CCMD_ICC: u64 = 1 << 63;
/// Context-cache invalidation granularity (CCMD bits 62:61): global.
pub const CCMD_CIRG_GLOBAL: u64 = 0b01 << 61;
/// Context-cache invalidation granularity: domain-selective.
pub const CCMD_CIRG_DOMAIN: u64 = 0b10 << 61;
/// Context-cache invalidation granularity: device-selective.
pub const CCMD_CIRG_DEVICE: u64 = 0b11 << 61;

/// Global context-cache invalidation descriptor.
#[must_use]
pub const fn ccmd_global_invalidate() -> u64 {
    CCMD_ICC | CCMD_CIRG_GLOBAL
}

/// Domain-selective context-cache invalidation for `did`.
#[must_use]
pub const fn ccmd_domain_invalidate(did: u16) -> u64 {
    CCMD_ICC | CCMD_CIRG_DOMAIN | ((did as u64) << 32)
}

/// Device-selective context-cache invalidation: `did`'s entry for source ID
/// `sid` (`bus << 8 | devfn`).
#[must_use]
pub const fn ccmd_device_invalidate(did: u16, sid: u16) -> u64 {
    CCMD_ICC | CCMD_CIRG_DEVICE | ((did as u64) << 32) | ((sid as u64) << 16)
}

/// Whether a context-cache invalidation is still in progress.
#[must_use]
pub const fn ccmd_pending(ccmd: u64) -> bool {
    ccmd & CCMD_ICC != 0
}

/// IOTLB invalidate in progress (bit 63).
pub const IOTLB_IVT: u64 = 1 << 63;
/// IOTLB invalidation granularity (bits 61:60): global.
pub const IOTLB_IIRG_GLOBAL: u64 = 0b01 << 60;
/// IOTLB invalidation granularity: domain-selective.
pub const IOTLB_IIRG_DOMAIN: u64 = 0b10 << 60;

/// Global IOTLB invalidation descriptor.
#[must_use]
pub const fn iotlb_global_invalidate() -> u64 {
    IOTLB_IVT | IOTLB_IIRG_GLOBAL
}

/// Domain-selective IOTLB invalidation for `did`.
#[must_use]
pub const fn iotlb_domain_invalidate(did: u16) -> u64 {
    IOTLB_IVT | IOTLB_IIRG_DOMAIN | ((did as u64) << 32)
}

/// Whether an IOTLB invalidation is still in progress.
#[must_use]
pub const fn iotlb_pending(value: u64) -> bool {
    value & IOTLB_IVT != 0
}

/// The value to program into `RTADDR`: the 4 KiB-aligned root-table address,
/// in legacy translation-table mode (`TTM` = 0).
#[must_use]
pub const fn rtaddr_value(root_phys: u64) -> u64 {
    root_phys & ADDR_MASK
}

// ---------------------------------------------------------------------------
// Second-stage page-table builder
// ---------------------------------------------------------------------------

/// Where a built second-stage hierarchy lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecondStageLayout {
    /// Physical address of the root table — the context entry's `SSPTPTR`.
    pub root_phys: u64,
    /// 4 KiB tables written.
    pub table_count: usize,
    /// Bytes the tables occupy (`table_count * 4096`).
    pub bytes: usize,
}

/// Why building a second-stage map failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondStageError {
    /// `bytes_to_map` was zero — nothing to map.
    EmptyRegion,
    /// `buf_phys` was not 4 KiB-aligned, so entries would decode the wrong
    /// addresses.
    UnalignedBase,
    /// `levels` was not 2..=6.
    UnsupportedLevels {
        /// The rejected level count.
        levels: u8,
    },
    /// The region needs more than one root table at `levels`.
    MapTooLarge {
        /// The rejected region size.
        bytes_to_map: u64,
    },
    /// The tables need more than `buf` holds.
    TablesExceedBuffer {
        /// Byte offset one past the last table byte.
        needed: usize,
        /// The buffer length.
        have: usize,
    },
}

/// Table levels in the tree: `levels`, minus the PT level when 2 MiB
/// super-page leaves replace it (the PD *is* the leaf level then).
const fn table_levels(levels: u8, leaf_2mib: bool) -> usize {
    (levels as usize) - if leaf_2mib { 1 } else { 0 }
}

/// Per-level table counts for a `levels`-deep tree covering `nleaf` leaves,
/// index 0 being the root. `Err` when the region cannot be built.
///
/// With 2 MiB super-page leaves the PD *is* the leaf level, so the tree holds
/// one fewer table than `levels` (e.g. 48-bit AGAW: PML4 → PDPT → PD).
fn table_counts(
    bytes_to_map: u64,
    levels: u8,
    leaf_2mib: bool,
) -> Result<[u64; 6], SecondStageError> {
    if bytes_to_map == 0 {
        return Err(SecondStageError::EmptyRegion);
    }
    if !(2..=6).contains(&levels) {
        return Err(SecondStageError::UnsupportedLevels { levels });
    }
    // levels() counts the PT level even when super-pages replace it.
    let nlevels = table_levels(levels, leaf_2mib);
    let leaf_size = if leaf_2mib { HUGE_2MIB } else { PAGE_SIZE };
    let nleaf = bytes_to_map.div_ceil(leaf_size);
    let leaf_level = nlevels - 1;
    let mut counts = [0u64; 6];
    counts[leaf_level] = nleaf.div_ceil(ENTRIES_PER_TABLE);
    for level in (0..leaf_level).rev() {
        counts[level] = counts[level + 1].div_ceil(ENTRIES_PER_TABLE);
    }
    // A single root table must cover the whole region.
    if counts[0] != 1 {
        return Err(SecondStageError::MapTooLarge { bytes_to_map });
    }
    Ok(counts)
}

/// How many 4 KiB tables a second-stage identity map of `[0, bytes_to_map)`
/// needs at `levels` page-table levels, or why it cannot be built.
///
/// `leaf_2mib` selects 2 MiB super-page leaves (needs
/// [`cap_super_page_2mib`]); otherwise leaves are 4 KiB.
///
/// # Errors
///
/// The [`SecondStageError`] variants of [`table_counts`], except the buffer
/// is not involved here.
pub fn second_stage_table_count(
    bytes_to_map: u64,
    levels: u8,
    leaf_2mib: bool,
) -> Result<usize, SecondStageError> {
    let counts = table_counts(bytes_to_map, levels, leaf_2mib)?;
    let total: u64 = counts[..usize::from(levels)].iter().sum();
    usize::try_from(total).map_err(|_| SecondStageError::MapTooLarge { bytes_to_map })
}

/// Build a second-stage identity map of `[0, bytes_to_map)` — every DMA
/// address in the region translates to the same physical address.
///
/// The tables are written into the **front** of `buf`, which the caller says
/// begins at physical address `buf_phys` (4 KiB-aligned); intermediate entries
/// encode `buf_phys`-relative addresses. Only entries covering the region are
/// written, so `buf` must be zeroed — the rest decode as not-present.
/// `levels` is 2..=6 (see [`AddressWidth`]); `leaf_2mib` selects 2 MiB
/// super-page leaves, otherwise 4 KiB leaves.
///
/// Returns the [`SecondStageLayout`] whose `root_phys` is the value for the
/// context entry's `SSPTPTR` field.
///
/// # Errors
///
/// - [`SecondStageError::EmptyRegion`] if `bytes_to_map == 0`;
/// - [`SecondStageError::UnalignedBase`] if `buf_phys` is not 4 KiB-aligned;
/// - [`SecondStageError::UnsupportedLevels`] if `levels` is not 2..=6;
/// - [`SecondStageError::MapTooLarge`] if the region needs more than one root
///   table;
/// - [`SecondStageError::TablesExceedBuffer`] if the tables do not fit in `buf`.
pub fn build_second_stage_identity(
    buf: &mut [u8],
    buf_phys: u64,
    bytes_to_map: u64,
    levels: u8,
    leaf_2mib: bool,
) -> Result<SecondStageLayout, SecondStageError> {
    if buf_phys & 0xFFF != 0 {
        return Err(SecondStageError::UnalignedBase);
    }
    let counts = table_counts(bytes_to_map, levels, leaf_2mib)?;
    let nlevels = table_levels(levels, leaf_2mib);
    let table_count = second_stage_table_count(bytes_to_map, levels, leaf_2mib)?;
    let needed = table_count
        .checked_mul(PAGE_SIZE_USIZE)
        .ok_or(SecondStageError::MapTooLarge { bytes_to_map })?;
    if buf.len() < needed {
        return Err(SecondStageError::TablesExceedBuffer {
            needed,
            have: buf.len(),
        });
    }

    let leaf_size = if leaf_2mib { HUGE_2MIB } else { PAGE_SIZE };
    let nleaf = bytes_to_map.div_ceil(leaf_size);
    let leaf_level = nlevels - 1;

    // First page index of each level's tables.
    let mut page_base = [0u64; 6];
    let mut acc = 0u64;
    for (level, base) in page_base.iter_mut().take(nlevels).enumerate() {
        *base = acc;
        acc += counts[level];
    }

    for level in 0..nlevels {
        for t in 0..counts[level] {
            let base_off = usize::try_from(page_base[level] + t)
                .map_err(|_| SecondStageError::MapTooLarge { bytes_to_map })?
                * PAGE_SIZE_USIZE;
            for e in 0..ENTRIES_PER_TABLE {
                let child_no = t * ENTRIES_PER_TABLE + e;
                let at_leaf_level = level == leaf_level;
                let limit = if at_leaf_level {
                    nleaf
                } else {
                    counts[level + 1]
                };
                if child_no >= limit {
                    break;
                }
                let entry = if at_leaf_level {
                    // (nleaf - 1) * leaf_size < bytes_to_map <= u64::MAX, so
                    // this cannot overflow.
                    sl_leaf_entry(child_no * leaf_size, leaf_2mib)
                } else {
                    sl_table_entry(buf_phys + (page_base[level + 1] + child_no) * PAGE_SIZE)
                };
                let off = base_off
                    + usize::try_from(e)
                        .map_err(|_| SecondStageError::MapTooLarge { bytes_to_map })?
                        * 8;
                buf[off..off + 8].copy_from_slice(&entry.to_le_bytes());
            }
        }
    }

    Ok(SecondStageLayout {
        root_phys: buf_phys,
        table_count,
        bytes: needed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A real `CAP` value from a QEMU `-device intel-iommu` boot
    /// (`dmar0: reg_base_addr fed90000 ... cap 12008c22260286`).
    const QEMU_CAP: u64 = 0x0012_008c_2226_0286;

    #[test]
    fn cap_decoders_match_a_real_qemu_cap() {
        // SAGAW: only the 39-bit bit set.
        assert_eq!(cap_sagaw(QEMU_CAP), 1 << 1);
        assert_eq!(
            choose_address_width(cap_sagaw(QEMU_CAP)),
            Some(AddressWidth::Bits39)
        );
        // MGAW: 38 + 1 = 39 ("Host address width 39").
        assert_eq!(cap_mgaw(QEMU_CAP), 39);
        // SPS: 2 MiB + 1 GiB super pages.
        assert!(cap_super_page_2mib(QEMU_CAP));
        assert!(cap_super_page_1gib(QEMU_CAP));
        // NF = 0x12 → 19 fault-recording registers.
        assert_eq!(cap_num_fault_regs(QEMU_CAP), 0x13);
        assert!(!cap_page_selective_invalidation(QEMU_CAP));
        assert!(!cap_coherent(QEMU_CAP));
        // Bit 7 of the low byte (0x86) — QEMU reports Caching Mode, so
        // domain ID 0 is reserved and the driver must not use it.
        assert!(cap_caching_mode(QEMU_CAP));
    }

    #[test]
    fn ecap_decoders_and_iotlb_offset() {
        // QI + DI + IR + EIM + PT + SC, IRO = 0x10 → offset 0x100.
        let ecap: u64 = 0x1 | (1 << 1) | (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7) | (0x10 << 8);
        assert!(ecap_queued_invalidation(ecap));
        assert!(ecap_device_iotlb(ecap));
        assert!(ecap_interrupt_remapping(ecap));
        assert!(ecap_extended_interrupt_mode(ecap));
        assert!(ecap_pass_through(ecap));
        assert!(ecap_snoop_control(ecap));
        assert_eq!(ecap_iotlb_reg_offset(ecap), 0x100);
        // No IRO field → the legacy offset.
        assert_eq!(ecap_iotlb_reg_offset(0), REG_IOTLB_LEGACY);
        assert_eq!(ecap_iotlb_reg_offset(0b1010), REG_IOTLB_LEGACY);
    }

    #[test]
    fn choose_address_width_prefers_the_widest() {
        // Extracted SAGAW field values (bit 0 = 30-bit … bit 4 = 64-bit).
        assert_eq!(choose_address_width(0b1_1111), Some(AddressWidth::Bits64));
        assert_eq!(choose_address_width(0b0_1111), Some(AddressWidth::Bits57));
        assert_eq!(choose_address_width(0b0_0111), Some(AddressWidth::Bits48));
        assert_eq!(choose_address_width(0b0_0010), Some(AddressWidth::Bits39));
        assert_eq!(choose_address_width(0b0_0001), Some(AddressWidth::Bits30));
        assert_eq!(choose_address_width(0), None);
        assert_eq!(AddressWidth::Bits48.bits(), 48);
        assert_eq!(AddressWidth::Bits48.levels(), 4);
        assert_eq!(AddressWidth::Bits48.encode(), 2);
    }

    #[test]
    fn root_entry_encodes_present_and_context_pointer() {
        let entry = root_entry(0x1_2345_6000);
        assert!(root_entry_present(&entry));
        assert_eq!(root_entry_ctp(&entry), 0x1_2345_6000);
        assert_eq!(entry[1], 0);
        assert!(!root_entry_present(&[0, 0]));
        // The table index is the bus number.
        assert_eq!(root_index(0), 0);
        assert_eq!(root_index(255), 255);
        assert_eq!(context_index(0x1F, 3), 0x1F * 8 + 3);
    }

    #[test]
    fn context_entry_round_trips() {
        let entry = context_entry(
            0x1234,
            0xABCD_1000,
            AddressWidth::Bits48,
            TranslationType::SecondStage,
        );
        assert!(context_entry_present(&entry));
        assert_eq!(context_entry_tt(&entry), 0);
        assert_eq!(context_entry_slptptr(&entry), 0xABCD_1000);
        assert_eq!(context_entry_aw(&entry), AddressWidth::Bits48.encode());
        assert_eq!(context_entry_did(&entry), 0x1234);

        let pt = context_entry(
            7,
            0x2000,
            AddressWidth::Bits39,
            TranslationType::PassThrough,
        );
        assert_eq!(context_entry_tt(&pt), 2);
        assert_eq!(context_entry_aw(&pt), AddressWidth::Bits39.encode());
        assert_eq!(context_entry_did(&pt), 7);
    }

    #[test]
    fn invalidation_descriptors_encode_granularity() {
        assert_eq!(ccmd_global_invalidate(), CCMD_ICC | CCMD_CIRG_GLOBAL);
        assert!(ccmd_pending(ccmd_global_invalidate()));
        assert!(!ccmd_pending(0));
        let dev = ccmd_device_invalidate(2, 0x1F03);
        assert!(ccmd_pending(dev));
        assert_eq!(dev & CCMD_ICC, CCMD_ICC);
        assert_eq!(dev & (0b11 << 61), CCMD_CIRG_DEVICE);
        assert_eq!((dev >> 32) & 0xFFFF, 2);
        assert_eq!((dev >> 16) & 0xFFFF, 0x1F03);

        assert_eq!(iotlb_global_invalidate(), IOTLB_IVT | IOTLB_IIRG_GLOBAL);
        let dom = iotlb_domain_invalidate(2);
        assert!(iotlb_pending(dom));
        assert_eq!(dom & (0b11 << 60), IOTLB_IIRG_DOMAIN);
        assert_eq!((dom >> 32) & 0xFFFF, 2);
        assert_eq!(rtaddr_value(0x9_8765_4000), 0x9_8765_4000);
    }

    #[test]
    fn sl_entries_carry_dma_permissions() {
        let leaf = sl_leaf_entry(0x2_00000, true);
        assert_eq!(leaf & (sl::READ | sl::WRITE), sl::READ | sl::WRITE);
        // DMA never executes: leaves deliberately omit EXECUTE.
        assert_eq!(leaf & sl::EXECUTE, 0);
        assert_eq!(leaf & sl::PAGE_SIZE, sl::PAGE_SIZE);
        assert_eq!(leaf & ADDR_MASK, 0x2_00000);

        let leaf4k = sl_leaf_entry(0x3000, false);
        assert_eq!(leaf4k & sl::PAGE_SIZE, 0);

        let table = sl_table_entry(0x9_0000);
        assert_eq!(
            table & (sl::READ | sl::WRITE | sl::EXECUTE),
            sl::READ | sl::WRITE | sl::EXECUTE
        );
    }

    /// Read one little-endian `u64` out of a built table buffer.
    fn read64(buf: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
    }

    #[test]
    fn second_stage_identity_2mib_48bit() {
        let mut buf = vec![0u8; 4 * PAGE_SIZE_USIZE];
        let layout = build_second_stage_identity(&mut buf, 0x10_0000, 8 << 20, 4, true).unwrap();
        // PML4 + PDPT + one PD for 4 × 2 MiB leaves.
        assert_eq!(layout.table_count, 3);
        assert_eq!(layout.bytes, 3 * PAGE_SIZE_USIZE);
        assert_eq!(layout.root_phys, 0x10_0000);

        // PML4[0] → PDPT, PDPT[0] → PD.
        assert_eq!(read64(&buf, 0) & ADDR_MASK, 0x10_1000);
        assert_eq!(read64(&buf, PAGE_SIZE_USIZE) & ADDR_MASK, 0x10_2000);
        // PD leaves: identity frames with the PS bit, R|W, no X.
        for (i, frame) in [0x0u64, 0x20_0000, 0x40_0000, 0x60_0000].iter().enumerate() {
            let e = read64(&buf, 2 * PAGE_SIZE_USIZE + i * 8);
            assert_eq!(e & ADDR_MASK, *frame, "leaf {i} frame");
            assert_eq!(e & sl::PAGE_SIZE, sl::PAGE_SIZE, "leaf {i} PS bit");
            assert_eq!(e & sl::EXECUTE, 0, "leaf {i} no execute");
        }
        // The fifth leaf slot is untouched (zeroed).
        assert_eq!(read64(&buf, 2 * PAGE_SIZE_USIZE + 4 * 8), 0);
        // A non-leaf entry carries full permissions.
        assert_eq!(
            read64(&buf, 0) & (sl::READ | sl::WRITE | sl::EXECUTE),
            sl::READ | sl::WRITE | sl::EXECUTE
        );
    }

    #[test]
    fn second_stage_identity_4kib_fallback() {
        // 8 KiB with 4 KiB leaves at 4 levels: PML4 + PDPT + PD + one PT.
        let mut buf = vec![0u8; 8 * PAGE_SIZE_USIZE];
        let layout = build_second_stage_identity(&mut buf, 0x20_0000, 8192, 4, false).unwrap();
        assert_eq!(layout.table_count, 4);
        let pt = 3 * PAGE_SIZE_USIZE;
        let e0 = read64(&buf, pt);
        let e1 = read64(&buf, pt + 8);
        assert_eq!(e0 & ADDR_MASK, 0x0);
        assert_eq!(e1 & ADDR_MASK, 0x1000);
        assert_eq!(e0 & sl::PAGE_SIZE, 0, "4 KiB leaves have no PS bit");
        assert_eq!(read64(&buf, pt + 16), 0);
    }

    #[test]
    fn second_stage_identity_39bit_3level() {
        // The QEMU unit above only offers 39-bit: PDPT root → PD leaves.
        let mut buf = vec![0u8; 4 * PAGE_SIZE_USIZE];
        let layout = build_second_stage_identity(&mut buf, 0x30_0000, 4 << 20, 3, true).unwrap();
        assert_eq!(layout.table_count, 2);
        assert_eq!(read64(&buf, 0) & ADDR_MASK, 0x30_1000);
        assert_eq!(read64(&buf, PAGE_SIZE_USIZE) & ADDR_MASK, 0x0);
        assert_eq!(read64(&buf, PAGE_SIZE_USIZE + 8) & ADDR_MASK, 0x20_0000);
    }

    #[test]
    fn second_stage_builder_reports_its_footprint() {
        // 4 GiB at 48-bit with 2 MiB leaves: PML4 + PDPT + 4 PDs.
        assert_eq!(second_stage_table_count(4 << 30, 4, true), Ok(6));
        // Same span with 4 KiB leaves: +2048 PTs.
        assert_eq!(second_stage_table_count(4 << 30, 4, false), Ok(2054));
    }

    #[test]
    fn second_stage_rejects_bad_inputs() {
        let mut buf = vec![0u8; PAGE_SIZE_USIZE];
        assert_eq!(
            build_second_stage_identity(&mut buf, 0x1000, 0, 4, true),
            Err(SecondStageError::EmptyRegion)
        );
        assert_eq!(
            build_second_stage_identity(&mut buf, 0x1001, 4096, 4, true),
            Err(SecondStageError::UnalignedBase)
        );
        assert_eq!(
            build_second_stage_identity(&mut buf, 0x1000, 4096, 1, true),
            Err(SecondStageError::UnsupportedLevels { levels: 1 })
        );
        assert_eq!(
            build_second_stage_identity(&mut buf, 0x1000, 4096, 7, true),
            Err(SecondStageError::UnsupportedLevels { levels: 7 })
        );
        // 4 KiB leaves at 2 levels cover 1 GiB; 4 GiB needs a second root.
        assert_eq!(
            second_stage_table_count(4 << 30, 2, false),
            Err(SecondStageError::MapTooLarge {
                bytes_to_map: 4 << 30
            })
        );
        // The tables do not fit the buffer.
        assert_eq!(
            build_second_stage_identity(&mut buf, 0x1000, 8 << 20, 4, true),
            Err(SecondStageError::TablesExceedBuffer {
                needed: 3 * PAGE_SIZE_USIZE,
                have: PAGE_SIZE_USIZE,
            })
        );
    }

    #[test]
    fn cap_num_domains_and_mgaw_edge_cases() {
        assert_eq!(cap_num_domains(0), 16);
        assert_eq!(cap_num_domains(0x7), 1 << 18);
        assert_eq!(cap_mgaw(0), 1);
    }
}

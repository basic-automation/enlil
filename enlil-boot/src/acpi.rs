//! Minimal bare-metal ACPI discovery for the enlil kernel (Phase 6.3).
//!
//! The full ACPI/device-tree readers live in `enlil-devices`, but that is a
//! `std` crate (tokio, anyhow) the `no_std` boot kernel cannot link yet. So the
//! kernel carries this minimal walker — the same shape as its own heap/IDT — to
//! discover physical hardware from the firmware tables the UEFI stage handed
//! off: it validates the RSDP, follows the XSDT, and counts the enabled CPUs in
//! the MADT. The parsing is pure and host-tested against synthetic tables; only
//! the raw-pointer reads of the identity-mapped firmware tables are gated to
//! the firmware target.

/// Whether a byte span's 8-bit checksum is zero (every ACPI table sums to 0).
#[must_use]
pub fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) == 0
}

/// The 8-byte RSDP signature.
pub const RSDP_SIGNATURE: &[u8; 8] = b"RSD PTR ";

/// The XSDT signature.
pub const XSDT_SIGNATURE: &[u8; 4] = b"XSDT";

/// The MADT (APIC) table signature.
pub const MADT_SIGNATURE: &[u8; 4] = b"APIC";

/// The `MCFG` (PCI Express `ECAM`) table signature.
pub const MCFG_SIGNATURE: &[u8; 4] = b"MCFG";

/// The `DMAR` table signature — Intel VT-d DMA remapping.
pub const DMAR_SIGNATURE: &[u8; 4] = b"DMAR";

/// The `IVRS` table signature — AMD-Vi (I/O virtualization) remapping.
pub const IVRS_SIGNATURE: &[u8; 4] = b"IVRS";

/// Which IOMMU the firmware advertises (the DMA-remapping engine Phase 6.4
/// programs for per-guest device isolation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IommuKind {
    /// No IOMMU table found.
    #[default]
    None,
    /// Intel VT-d (a `DMAR` table).
    IntelVtd,
    /// AMD-Vi (an `IVRS` table).
    AmdVi,
}

impl IommuKind {
    /// A short human name for the serial report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::IntelVtd => "Intel VT-d",
            Self::AmdVi => "AMD-Vi",
        }
    }
}

/// Classify an SDT signature as an IOMMU table (or [`IommuKind::None`]).
#[must_use]
pub fn iommu_kind_from_signature(sig: [u8; 4]) -> IommuKind {
    if &sig == DMAR_SIGNATURE {
        IommuKind::IntelVtd
    } else if &sig == IVRS_SIGNATURE {
        IommuKind::AmdVi
    } else {
        IommuKind::None
    }
}

/// Length of an ACPI System Description Table header.
pub const SDT_HEADER_LEN: usize = 36;

/// Read a little-endian `u32` at `off`, or `None` past the end.
fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let raw = bytes.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes(raw.try_into().ok()?))
}

/// Read a little-endian `u64` at `off`, or `None` past the end.
fn read_u64(bytes: &[u8], off: usize) -> Option<u64> {
    let raw = bytes.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes(raw.try_into().ok()?))
}

/// One DMA-remapping hardware unit from a `DMAR` table (Intel VT-d spec §8.3).
///
/// A `DRHD` structure describes one physical IOMMU: where its register block
/// lives and which PCI segment and devices it covers. Programming DMA remapping
/// (ROADMAP 6.4) starts by finding these — every root table, context table and
/// page-table write goes through this register base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RemappingUnit {
    /// Physical base of the unit's memory-mapped register block.
    pub register_base: u64,
    /// PCI segment (domain) the unit covers.
    pub segment: u16,
    /// Whether the unit covers **all** devices in its segment not claimed by
    /// another unit (`INCLUDE_PCI_ALL`, flags bit 0) — the catch-all unit.
    pub covers_all: bool,
    /// Bytes of device-scope entries following the fixed `DRHD` fields, i.e. how
    /// many specific devices the unit is scoped to (0 when `covers_all`).
    pub device_scope_bytes: usize,
    /// Byte offset of this unit's first device-scope entry within the `DMAR`
    /// table — where [`dmar_device_scopes`] reads from.
    pub scope_offset: usize,
}

/// What kind of device a `DRHD` device-scope entry names (VT-d spec §8.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScopeKind {
    /// A PCI endpoint device.
    PciEndpoint,
    /// A PCI sub-hierarchy (a bridge and everything below it).
    PciSubHierarchy,
    /// An I/O APIC.
    IoApic,
    /// An HPET or other MSI-capable timer block.
    Hpet,
    /// An ACPI namespace device.
    AcpiNamespace,
    /// A type this decoder does not recognise.
    #[default]
    Unknown,
}

impl ScopeKind {
    /// Decode the device-scope type byte.
    #[must_use]
    pub const fn from_type(raw: u8) -> Self {
        match raw {
            1 => Self::PciEndpoint,
            2 => Self::PciSubHierarchy,
            3 => Self::IoApic,
            4 => Self::Hpet,
            5 => Self::AcpiNamespace,
            _ => Self::Unknown,
        }
    }

    /// A short human name for the serial report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PciEndpoint => "endpoint",
            Self::PciSubHierarchy => "bridge",
            Self::IoApic => "ioapic",
            Self::Hpet => "hpet",
            Self::AcpiNamespace => "acpi",
            Self::Unknown => "unknown",
        }
    }
}

/// One device a remapping unit is scoped to (VT-d spec §8.3.1).
///
/// Says *which* devices an IOMMU governs — the input to assigning a
/// passed-through device to a per-guest DMA domain (LOCKED PRINCIPLE 5). A
/// device not covered by any unit cannot be isolated, so this has to be read
/// before passthrough can be offered for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceScope {
    /// What kind of device the entry names.
    pub kind: ScopeKind,
    /// The I/O APIC or HPET number, for those kinds (0 otherwise).
    pub enumeration_id: u8,
    /// PCI bus the path starts from.
    pub start_bus: u8,
    /// Device number of the first path hop.
    pub device: u8,
    /// Function number of the first path hop.
    pub function: u8,
}

/// Parse the device-scope entries of one remapping unit into `out`, returning
/// how many were found.
///
/// `dmar` is the whole table and `unit` a [`RemappingUnit`] from
/// [`dmar_remapping_units`]. Each entry is `type(1) len(1) rsvd(2)
/// enum_id(1) start_bus(1)` followed by 2-byte `(device, function)` path hops;
/// the first hop is decoded, which for the overwhelmingly common
/// directly-attached case is the whole path. A malformed (too short) entry ends
/// the walk rather than looping.
#[must_use]
pub fn dmar_device_scopes(dmar: &[u8], unit: &RemappingUnit, out: &mut [DeviceScope]) -> usize {
    /// type(1) len(1) reserved(2) enumeration id(1) start bus(1).
    const SCOPE_HEADER_LEN: usize = 6;

    let end = unit
        .scope_offset
        .saturating_add(unit.device_scope_bytes)
        .min(dmar.len());
    let mut off = unit.scope_offset;
    let mut found = 0usize;

    while off + SCOPE_HEADER_LEN <= end {
        let entry_len = usize::from(dmar[off + 1]);
        if entry_len < SCOPE_HEADER_LEN || off + entry_len > end {
            break;
        }
        if let Some(slot) = out.get_mut(found) {
            *slot = DeviceScope {
                kind: ScopeKind::from_type(dmar[off]),
                enumeration_id: dmar[off + 4],
                start_bus: dmar[off + 5],
                // The first path hop, when the entry carries one.
                device: dmar.get(off + 6).copied().unwrap_or(0),
                function: dmar.get(off + 7).copied().unwrap_or(0),
            };
        }
        found += 1;
        off += entry_len;
    }
    found
}

/// The `DRHD` remapping-structure type in a `DMAR` table.
pub const DMAR_TYPE_DRHD: u16 = 0;

/// Offset of the first remapping structure in a `DMAR` table.
///
/// The `DMAR` header adds a host address width byte and a flags byte (plus 10
/// reserved) after the standard SDT header (VT-d spec §8.1).
pub const DMAR_STRUCTURES_OFFSET: usize = SDT_HEADER_LEN + 12;

/// Parse the `DMAR` table's body, collecting each DMA-remapping hardware unit
/// into `out`, and return how many were found.
///
/// Walks the variable-length remapping structures after the `DMAR` header,
/// selecting the `DRHD` (hardware unit definition) entries and decoding each
/// one's flags, segment and register base (VT-d spec §8.3). Other structure
/// types — reserved-memory regions, ATSR, RHSA — are skipped by their length.
///
/// The returned count is the true number present even if it exceeds `out`, so a
/// caller can tell it needs a bigger buffer. A zero-length or over-long
/// structure ends the walk rather than looping or reading past the table.
#[must_use]
pub fn dmar_remapping_units(dmar: &[u8], out: &mut [RemappingUnit]) -> usize {
    /// Fixed `DRHD` fields: type(2) len(2) flags(1) rsvd(1) segment(2) base(8).
    const DRHD_FIXED_LEN: usize = 16;

    let Some(length) = sdt_length(dmar) else {
        return 0;
    };
    let end = (length as usize).min(dmar.len());
    let mut off = DMAR_STRUCTURES_OFFSET;
    let mut found = 0usize;

    while off + 4 <= end {
        let (Some(kind), Some(struct_len)) = (read_u16(dmar, off), read_u16(dmar, off + 2)) else {
            break;
        };
        let struct_len = struct_len as usize;
        // A zero (or under-header) length would spin forever; an over-long one
        // would read past the table.
        if struct_len < 4 || off + struct_len > end {
            break;
        }
        if kind == DMAR_TYPE_DRHD && struct_len >= DRHD_FIXED_LEN {
            let flags = dmar.get(off + 4).copied().unwrap_or(0);
            let (Some(segment), Some(register_base)) =
                (read_u16(dmar, off + 6), read_u64(dmar, off + 8))
            else {
                break;
            };
            if let Some(slot) = out.get_mut(found) {
                *slot = RemappingUnit {
                    register_base,
                    segment,
                    covers_all: flags & 1 != 0,
                    device_scope_bytes: struct_len - DRHD_FIXED_LEN,
                    scope_offset: off + DRHD_FIXED_LEN,
                };
            }
            found += 1;
        }
        off += struct_len;
    }
    found
}

/// Read a little-endian `u16` at `off`, or `None` past the end.
fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    let raw = bytes.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes(raw.try_into().ok()?))
}

/// Validate an ACPI 2.0+ RSDP and return its XSDT physical address.
///
/// Checks the 8-byte signature, the revision (≥ 2, which guarantees an XSDT),
/// and the first-20-byte ACPI-1.0 checksum, then returns the 64-bit XSDT
/// address at offset 24. Returns `None` if the signature, revision, checksum,
/// or length are wrong.
#[must_use]
pub fn rsdp_xsdt_address(rsdp: &[u8]) -> Option<u64> {
    if rsdp.len() < 33 || &rsdp[..8] != RSDP_SIGNATURE {
        return None;
    }
    // ACPI 2.0+ (revision ≥ 2) is required for the XSDT at offset 24.
    if *rsdp.get(15)? < 2 {
        return None;
    }
    // The ACPI-1.0 checksum covers the first 20 bytes.
    if !checksum_ok(&rsdp[..20]) {
        return None;
    }
    read_u64(rsdp, 24)
}

/// The 4-byte signature of an SDT (its first four bytes), or `None`.
#[must_use]
pub fn sdt_signature(sdt: &[u8]) -> Option<[u8; 4]> {
    sdt.get(..4)?.try_into().ok()
}

/// The declared length of an SDT (the `u32` at offset 4), or `None`.
#[must_use]
pub fn sdt_length(sdt: &[u8]) -> Option<u32> {
    read_u32(sdt, 4)
}

/// The number of 64-bit table pointers an XSDT of `length` bytes holds:
/// `(length - header) / 8`.
#[must_use]
pub const fn xsdt_entry_count(length: u32) -> usize {
    (length as usize).saturating_sub(SDT_HEADER_LEN) / 8
}

/// The `i`th table pointer in an XSDT body, or `None` past the end.
#[must_use]
pub fn xsdt_entry(xsdt: &[u8], i: usize) -> Option<u64> {
    let off = SDT_HEADER_LEN.checked_add(i.checked_mul(8)?)?;
    read_u64(xsdt, off)
}

/// MADT interrupt-controller structure type: Processor Local APIC.
pub const MADT_LOCAL_APIC: u8 = 0;

/// MADT interrupt-controller structure type: Processor Local x2APIC.
pub const MADT_LOCAL_X2APIC: u8 = 9;

/// The "processor enabled" bit in a MADT local-APIC/x2APIC flags word.
pub const MADT_CPU_ENABLED: u32 = 1 << 0;

/// Count the enabled processors in a MADT.
///
/// Walks the interrupt-controller structures after the 44-byte MADT header
/// (36-byte SDT header + 4-byte local-APIC address + 4-byte flags), counting
/// Local APIC (type 0) and Local x2APIC (type 9) entries whose flags mark the
/// processor enabled. Each structure's `length` byte bounds the walk; a
/// zero-length structure stops it (a malformed table cannot spin).
#[must_use]
pub fn madt_enabled_cpu_count(madt: &[u8]) -> u32 {
    const MADT_STRUCTS_OFFSET: usize = 44;
    let mut count = 0;
    let mut off = MADT_STRUCTS_OFFSET;
    while off + 2 <= madt.len() {
        let kind = madt[off];
        let len = madt[off + 1] as usize;
        if len == 0 || off + len > madt.len() {
            break;
        }
        match kind {
            MADT_LOCAL_APIC => {
                // flags at struct offset 4 (u32).
                if let Some(flags) = read_u32(madt, off + 4)
                    && flags & MADT_CPU_ENABLED != 0
                {
                    count += 1;
                }
            }
            MADT_LOCAL_X2APIC => {
                // flags at struct offset 8 (u32).
                if let Some(flags) = read_u32(madt, off + 8)
                    && flags & MADT_CPU_ENABLED != 0
                {
                    count += 1;
                }
            }
            _ => {}
        }
        off += len;
    }
    count
}

/// Collect the enabled processors' APIC / x2APIC IDs from a MADT into `out`,
/// returning the total enabled count (which may exceed `out.len()`).
///
/// The AP inventory the SMP bring-up (INIT-SIPI-SIPI, ROADMAP 6.2) targets: the
/// BSP is one of these IDs, the rest are the application processors to wake.
/// Walks the same interrupt-controller structures as [`madt_enabled_cpu_count`],
/// reading the Local APIC ID (type 0, `u8` at struct offset 3) or the Local
/// x2APIC ID (type 9, `u32` at struct offset 4) of each enabled processor, in
/// table order. Writes up to `out.len()` ids but always returns the true count,
/// so a caller can tell its buffer was too small.
#[must_use]
pub fn madt_enabled_apic_ids(madt: &[u8], out: &mut [u32]) -> usize {
    const MADT_STRUCTS_OFFSET: usize = 44;
    let mut count = 0usize;
    let mut off = MADT_STRUCTS_OFFSET;
    while off + 2 <= madt.len() {
        let kind = madt[off];
        let len = madt[off + 1] as usize;
        if len == 0 || off + len > madt.len() {
            break;
        }
        let id_and_flags = match kind {
            MADT_LOCAL_APIC => madt
                .get(off + 3)
                .map(|&id| u32::from(id))
                .zip(read_u32(madt, off + 4)),
            MADT_LOCAL_X2APIC => read_u32(madt, off + 4).zip(read_u32(madt, off + 8)),
            _ => None,
        };
        if let Some((id, flags)) = id_and_flags
            && flags & MADT_CPU_ENABLED != 0
        {
            if let Some(slot) = out.get_mut(count) {
                *slot = id;
            }
            count += 1;
        }
        off += len;
    }
    count
}

/// The first `ECAM` allocation in an `MCFG`: `(base, segment, start_bus, end_bus)`.
///
/// The `MCFG` body is an 8-byte reserved field then 16-byte allocation entries:
/// `ECAM` base (`u64`) @0, PCI segment group (`u16`) @8, start bus @10, end bus
/// @11. Returns the first entry, or `None` if the table has none.
#[must_use]
pub fn mcfg_first_allocation(mcfg: &[u8]) -> Option<(u64, u16, u8, u8)> {
    const MCFG_ALLOCS_OFFSET: usize = SDT_HEADER_LEN + 8;
    let base = read_u64(mcfg, MCFG_ALLOCS_OFFSET)?;
    let segment = u16::from_le_bytes(
        mcfg.get(MCFG_ALLOCS_OFFSET + 8..MCFG_ALLOCS_OFFSET + 10)?
            .try_into()
            .ok()?,
    );
    let start_bus = *mcfg.get(MCFG_ALLOCS_OFFSET + 10)?;
    let end_bus = *mcfg.get(MCFG_ALLOCS_OFFSET + 11)?;
    Some((base, segment, start_bus, end_bus))
}

/// How many enabled-processor APIC IDs [`AcpiSummary`] records inline.
///
/// The BSP plus a handful of APs — enough to report the SMP inventory on serial
/// without a heap allocation. The true enabled count ([`AcpiSummary::enabled_cpus`])
/// may exceed this; only the first `MAX_REPORTED_APIC_IDS` ids are kept.
pub const MAX_REPORTED_APIC_IDS: usize = 8;

/// What the kernel discovered from the firmware ACPI tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AcpiSummary {
    /// Number of tables the XSDT references.
    pub tables: usize,
    /// Enabled processors counted in the MADT (0 if no MADT was found).
    pub enabled_cpus: u32,
    /// The first [`MAX_REPORTED_APIC_IDS`] enabled processors' APIC IDs — the
    /// AP inventory SMP bring-up targets (ROADMAP 6.2).
    pub apic_ids: [u32; MAX_REPORTED_APIC_IDS],
    /// How many entries of [`apic_ids`](Self::apic_ids) are valid
    /// (`min(enabled_cpus, MAX_REPORTED_APIC_IDS)`).
    pub apic_id_count: usize,
    /// PCI Express `ECAM` base physical address from the `MCFG` (0 if absent).
    pub ecam_base: u64,
    /// Highest PCI bus number the `ECAM` window covers (from the `MCFG`).
    pub ecam_end_bus: u8,
    /// The IOMMU the firmware advertises (`DMAR`/`IVRS`), or `None`.
    pub iommu: IommuKind,
    /// The first [`MAX_REPORTED_REMAPPING_UNITS`] DMA-remapping hardware units
    /// from the `DMAR` — the register blocks Phase 6.4 programs.
    pub remapping_units: [RemappingUnit; MAX_REPORTED_REMAPPING_UNITS],
    /// How many remapping units the `DMAR` declared (may exceed the array).
    pub remapping_unit_count: usize,
    /// The devices the **first** remapping unit is scoped to — which hardware
    /// that IOMMU governs, and so what can be isolated for passthrough.
    pub device_scopes: [DeviceScope; MAX_REPORTED_DEVICE_SCOPES],
    /// How many device scopes the first unit declared (may exceed the array).
    pub device_scope_count: usize,
}

/// How many device-scope entries the summary keeps for the first unit.
pub const MAX_REPORTED_DEVICE_SCOPES: usize = 4;

/// How many DMA-remapping hardware units the summary keeps.
///
/// Real platforms have one per PCI segment plus a catch-all; a handful is ample
/// for the boot report without a heap allocation.
pub const MAX_REPORTED_REMAPPING_UNITS: usize = 4;

#[cfg(target_os = "uefi")]
pub use hw::discover;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        AcpiSummary, DMAR_SIGNATURE, DeviceScope, IommuKind, MADT_SIGNATURE, MAX_REPORTED_APIC_IDS,
        MCFG_SIGNATURE, RemappingUnit, SDT_HEADER_LEN, dmar_device_scopes, dmar_remapping_units,
        iommu_kind_from_signature, madt_enabled_apic_ids, madt_enabled_cpu_count,
        mcfg_first_allocation, rsdp_xsdt_address, sdt_length, sdt_signature, xsdt_entry,
        xsdt_entry_count,
    };

    /// View `len` bytes of identity-mapped physical memory at `phys`.
    ///
    /// # Safety
    ///
    /// `phys` must point at `len` readable bytes of the live, identity-mapped
    /// firmware ACPI region (the kernel runs on the firmware's identity map).
    const unsafe fn phys_slice<'a>(phys: u64, len: usize) -> &'a [u8] {
        // SAFETY: the caller guarantees a live, readable identity-mapped span.
        unsafe { core::slice::from_raw_parts(phys as *const u8, len) }
    }

    /// Walk the firmware ACPI tables from the handed-off RSDP and summarize
    /// them, or `None` if the RSDP is invalid.
    ///
    /// Validates the RSDP, reads the XSDT (its header first, for the length,
    /// then its full body), counts the referenced tables, and — on finding the
    /// MADT — counts the enabled CPUs. All reads are of the identity-mapped
    /// firmware tables (the kernel has not yet replaced the firmware page
    /// tables), bounded by each table's declared length.
    #[must_use]
    pub fn discover(rsdp_phys: u64) -> Option<AcpiSummary> {
        if rsdp_phys == 0 {
            return None;
        }
        // The RSDP is 36 bytes for ACPI 2.0+.
        // SAFETY: the firmware handed off this RSDP pointer into mapped memory.
        let rsdp = unsafe { phys_slice(rsdp_phys, 36) };
        let xsdt_phys = rsdp_xsdt_address(rsdp)?;

        // Read the XSDT header for its length, then the full table.
        // SAFETY: xsdt_phys came from a validated RSDP; the header is mapped.
        let xsdt_hdr = unsafe { phys_slice(xsdt_phys, SDT_HEADER_LEN) };
        let xsdt_len = sdt_length(xsdt_hdr)?;
        // SAFETY: the XSDT spans xsdt_len mapped bytes from xsdt_phys.
        let xsdt = unsafe { phys_slice(xsdt_phys, xsdt_len as usize) };

        let mut summary = AcpiSummary {
            tables: xsdt_entry_count(xsdt_len),
            enabled_cpus: 0,
            apic_ids: [0; MAX_REPORTED_APIC_IDS],
            apic_id_count: 0,
            ecam_base: 0,
            ecam_end_bus: 0,
            iommu: IommuKind::None,
            remapping_units: [RemappingUnit::default(); super::MAX_REPORTED_REMAPPING_UNITS],
            remapping_unit_count: 0,
            device_scopes: [DeviceScope::default(); super::MAX_REPORTED_DEVICE_SCOPES],
            device_scope_count: 0,
        };

        // Scan the referenced tables for the MADT (enabled CPUs) and the MCFG
        // (PCIe ECAM window).
        let mut i = 0;
        while let Some(table_phys) = xsdt_entry(xsdt, i) {
            // SAFETY: each XSDT entry points at a mapped SDT; read its header.
            let hdr = unsafe { phys_slice(table_phys, SDT_HEADER_LEN) };
            let Some(sig) = sdt_signature(hdr) else {
                i += 1;
                continue;
            };
            match iommu_kind_from_signature(sig) {
                IommuKind::None => {}
                kind => summary.iommu = kind,
            }
            if let Some(len) = sdt_length(hdr) {
                // SAFETY: the table spans `len` mapped bytes from table_phys.
                let table = unsafe { phys_slice(table_phys, len as usize) };
                if sig == *MADT_SIGNATURE {
                    summary.enabled_cpus = madt_enabled_cpu_count(table);
                    let total = madt_enabled_apic_ids(table, &mut summary.apic_ids);
                    summary.apic_id_count = total.min(MAX_REPORTED_APIC_IDS);
                } else if sig == *MCFG_SIGNATURE
                    && let Some((base, _seg, _start, end)) = mcfg_first_allocation(table)
                {
                    summary.ecam_base = base;
                    summary.ecam_end_bus = end;
                } else if sig == *DMAR_SIGNATURE {
                    // The register blocks DMA remapping is programmed through,
                    // and which devices the first unit governs.
                    summary.remapping_unit_count =
                        dmar_remapping_units(table, &mut summary.remapping_units);
                    if summary.remapping_unit_count > 0 {
                        summary.device_scope_count = dmar_device_scopes(
                            table,
                            &summary.remapping_units[0],
                            &mut summary.device_scopes,
                        );
                    }
                }
            }
            i += 1;
        }
        Some(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal SDT header with `sig` and `length`.
    fn sdt_header(sig: [u8; 4], length: u32) -> [u8; SDT_HEADER_LEN] {
        let mut h = [0u8; SDT_HEADER_LEN];
        h[..4].copy_from_slice(&sig);
        h[4..8].copy_from_slice(&length.to_le_bytes());
        h
    }

    #[test]
    fn checksum_ok_sums_to_zero() {
        assert!(checksum_ok(&[0x10, 0x20, 0xD0])); // 0x100 = 0 mod 256
        assert!(!checksum_ok(&[0x10, 0x20, 0x00]));
    }

    #[test]
    fn rsdp_returns_the_xsdt_address_when_valid() {
        let mut rsdp = [0u8; 36];
        rsdp[..8].copy_from_slice(RSDP_SIGNATURE);
        rsdp[15] = 2; // revision 2 (ACPI 2.0+)
        rsdp[24..32].copy_from_slice(&0x7B7E_0000u64.to_le_bytes()); // XSDT addr
        // Fix the first-20-byte checksum to 0.
        let sum = rsdp[..20].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        rsdp[9] = rsdp[9].wrapping_sub(sum);
        assert_eq!(rsdp_xsdt_address(&rsdp), Some(0x7B7E_0000));
    }

    #[test]
    fn rsdp_rejects_bad_signature_revision_and_checksum() {
        let mut rsdp = [0u8; 36];
        rsdp[..8].copy_from_slice(RSDP_SIGNATURE);
        rsdp[15] = 2;
        // Checksum is nonzero (all zero XSDT addr, no fixup) → rejected.
        assert_eq!(rsdp_xsdt_address(&rsdp), None);
        // Wrong signature.
        let mut bad = rsdp;
        bad[0] = b'X';
        assert_eq!(rsdp_xsdt_address(&bad), None);
        // ACPI 1.0 revision (0) → no XSDT.
        let mut old = rsdp;
        old[15] = 0;
        assert_eq!(rsdp_xsdt_address(&old), None);
    }

    /// Build a DMAR table from raw remapping structures.
    fn dmar_table(structures: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = structures.concat();
        let length = u32::try_from(DMAR_STRUCTURES_OFFSET + body.len()).unwrap();
        let mut table = sdt_header(*DMAR_SIGNATURE, length).to_vec();
        table.extend_from_slice(&[0u8; 12]); // host addr width + flags + reserved
        table.extend_from_slice(&body);
        table
    }

    /// A DRHD structure with `scope_bytes` of trailing device-scope entries.
    fn drhd(flags: u8, segment: u16, base: u64, scope_bytes: usize) -> Vec<u8> {
        let len = u16::try_from(16 + scope_bytes).unwrap();
        let mut s = Vec::new();
        s.extend_from_slice(&DMAR_TYPE_DRHD.to_le_bytes());
        s.extend_from_slice(&len.to_le_bytes());
        s.push(flags);
        s.push(0); // reserved
        s.extend_from_slice(&segment.to_le_bytes());
        s.extend_from_slice(&base.to_le_bytes());
        s.extend_from_slice(&vec![0u8; scope_bytes]);
        s
    }

    /// A non-DRHD remapping structure (e.g. RMRR), which must be skipped.
    fn other_structure(kind: u16, len: u16) -> Vec<u8> {
        let mut s = Vec::new();
        s.extend_from_slice(&kind.to_le_bytes());
        s.extend_from_slice(&len.to_le_bytes());
        s.extend_from_slice(&vec![0u8; usize::from(len) - 4]);
        s
    }

    #[test]
    fn dmar_decodes_remapping_units_and_skips_other_structures() {
        let table = dmar_table(&[
            drhd(0, 0, 0xFED9_0000, 8),  // scoped to specific devices
            other_structure(1, 24),      // RMRR — skipped by length
            drhd(1, 0, 0xFED9_1000, 0),  // INCLUDE_PCI_ALL catch-all
            other_structure(2, 12),      // ATSR — skipped
            drhd(0, 3, 0xFED9_2000, 16), // a different PCI segment
        ]);

        let mut units = [RemappingUnit::default(); 4];
        assert_eq!(dmar_remapping_units(&table, &mut units), 3);

        assert_eq!(units[0].register_base, 0xFED9_0000);
        assert!(!units[0].covers_all);
        assert_eq!(units[0].device_scope_bytes, 8);

        assert!(units[1].covers_all, "INCLUDE_PCI_ALL not decoded");
        assert_eq!(units[1].register_base, 0xFED9_1000);
        assert_eq!(units[1].device_scope_bytes, 0);

        assert_eq!(units[2].segment, 3);
        assert_eq!(units[2].register_base, 0xFED9_2000);
    }

    /// A device-scope entry with one path hop.
    fn scope(kind: u8, enum_id: u8, bus: u8, dev: u8, func: u8) -> Vec<u8> {
        vec![kind, 8, 0, 0, enum_id, bus, dev, func]
    }

    #[test]
    fn dmar_decodes_the_devices_a_unit_is_scoped_to() {
        let scopes: Vec<u8> = [
            scope(1, 0, 0, 0x1D, 0), // PCI endpoint 00:1d.0
            scope(3, 2, 0, 0x1F, 7), // I/O APIC number 2
            scope(4, 0, 1, 0x00, 1), // HPET on bus 1
        ]
        .concat();
        let table = dmar_table(&[{
            let mut d = drhd(0, 0, 0xFED9_0000, 0);
            // Extend the DRHD to carry the scopes.
            let len = u16::try_from(16 + scopes.len()).unwrap();
            d[2..4].copy_from_slice(&len.to_le_bytes());
            d.extend_from_slice(&scopes);
            d
        }]);

        let mut units = [RemappingUnit::default(); 2];
        assert_eq!(dmar_remapping_units(&table, &mut units), 1);
        assert_eq!(units[0].device_scope_bytes, scopes.len());

        let mut found = [DeviceScope::default(); 4];
        assert_eq!(dmar_device_scopes(&table, &units[0], &mut found), 3);

        assert_eq!(found[0].kind, ScopeKind::PciEndpoint);
        assert_eq!(
            (found[0].start_bus, found[0].device, found[0].function),
            (0, 0x1D, 0)
        );
        assert_eq!(found[1].kind, ScopeKind::IoApic);
        assert_eq!(found[1].enumeration_id, 2);
        assert_eq!(found[2].kind, ScopeKind::Hpet);
        assert_eq!(found[2].start_bus, 1);
    }

    #[test]
    fn dmar_device_scopes_handles_no_scopes_and_malformed_entries() {
        // An INCLUDE_PCI_ALL unit has no scopes at all.
        let table = dmar_table(&[drhd(1, 0, 0xFED9_0000, 0)]);
        let mut units = [RemappingUnit::default(); 1];
        assert_eq!(dmar_remapping_units(&table, &mut units), 1);
        let mut found = [DeviceScope::default(); 4];
        assert!(units[0].covers_all);
        assert_eq!(dmar_device_scopes(&table, &units[0], &mut found), 0);

        // A zero-length scope entry ends the walk instead of spinning.
        let bad: Vec<u8> = vec![1, 0, 0, 0, 0, 0, 0, 0];
        let table = dmar_table(&[{
            let mut d = drhd(0, 0, 0x1000, 0);
            let len = u16::try_from(16 + bad.len()).unwrap();
            d[2..4].copy_from_slice(&len.to_le_bytes());
            d.extend_from_slice(&bad);
            d
        }]);
        assert_eq!(dmar_remapping_units(&table, &mut units), 1);
        assert_eq!(dmar_device_scopes(&table, &units[0], &mut found), 0);
    }

    #[test]
    fn scope_kind_decodes_the_vtd_type_codes() {
        assert_eq!(ScopeKind::from_type(1), ScopeKind::PciEndpoint);
        assert_eq!(ScopeKind::from_type(2), ScopeKind::PciSubHierarchy);
        assert_eq!(ScopeKind::from_type(3), ScopeKind::IoApic);
        assert_eq!(ScopeKind::from_type(4), ScopeKind::Hpet);
        assert_eq!(ScopeKind::from_type(5), ScopeKind::AcpiNamespace);
        assert_eq!(ScopeKind::from_type(9), ScopeKind::Unknown);
    }

    #[test]
    fn dmar_reports_the_true_count_past_a_short_buffer() {
        let table = dmar_table(&[
            drhd(0, 0, 0x1000, 0),
            drhd(0, 0, 0x2000, 0),
            drhd(0, 0, 0x3000, 0),
        ]);
        let mut one = [RemappingUnit::default(); 1];
        // The caller learns it needs a bigger buffer, and the first still lands.
        assert_eq!(dmar_remapping_units(&table, &mut one), 3);
        assert_eq!(one[0].register_base, 0x1000);
    }

    #[test]
    fn dmar_rejects_malformed_structures_without_looping_or_overreading() {
        // A zero-length structure would spin forever if not caught.
        let mut zero_len = dmar_table(&[drhd(0, 0, 0x1000, 0)]);
        zero_len.extend_from_slice(&[0u8, 0, 0, 0]); // type 0, length 0
        let mut units = [RemappingUnit::default(); 4];
        assert_eq!(dmar_remapping_units(&zero_len, &mut units), 1);

        // A structure claiming to run past the table end is refused.
        let mut overlong = dmar_table(&[drhd(0, 0, 0x1000, 0)]);
        let at = DMAR_STRUCTURES_OFFSET + 16;
        overlong.extend_from_slice(&[0u8, 0, 0xFF, 0xFF]);
        assert!(overlong.len() > at);
        assert_eq!(dmar_remapping_units(&overlong, &mut units), 1);

        // An empty body yields nothing.
        assert_eq!(dmar_remapping_units(&dmar_table(&[]), &mut units), 0);
        // A truncated table is not read past its end.
        assert_eq!(dmar_remapping_units(&[0u8; 8], &mut units), 0);
    }

    #[test]
    fn xsdt_entry_count_and_entries() {
        // Header + two 8-byte pointers.
        let length = u32::try_from(SDT_HEADER_LEN + 16).unwrap();
        let mut xsdt = sdt_header(*XSDT_SIGNATURE, length).to_vec();
        xsdt.extend_from_slice(&0x1111u64.to_le_bytes());
        xsdt.extend_from_slice(&0x2222u64.to_le_bytes());
        assert_eq!(xsdt_entry_count(length), 2);
        assert_eq!(xsdt_entry(&xsdt, 0), Some(0x1111));
        assert_eq!(xsdt_entry(&xsdt, 1), Some(0x2222));
        assert_eq!(xsdt_entry(&xsdt, 2), None);
    }

    #[test]
    fn madt_counts_only_enabled_apic_and_x2apic() {
        let mut madt = sdt_header(*MADT_SIGNATURE, 0).to_vec();
        madt.extend_from_slice(&[0u8; 8]); // local-APIC addr + flags → offset 44
        // Local APIC, enabled (type 0, len 8, flags bit0 set at struct off 4).
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 0, 0, 0x01, 0, 0, 0]);
        // Local APIC, disabled.
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 1, 1, 0x00, 0, 0, 0]);
        // Local x2APIC, enabled (type 9, len 16, flags at struct off 8).
        madt.extend_from_slice(&[
            MADT_LOCAL_X2APIC,
            16,
            0,
            0,
            0,
            0,
            0,
            0,
            0x01,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        // An unrelated structure (I/O APIC, type 1) is ignored.
        madt.extend_from_slice(&[1, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(madt_enabled_cpu_count(&madt), 2);
    }

    #[test]
    fn madt_collects_enabled_apic_ids_in_order() {
        let mut madt = sdt_header(*MADT_SIGNATURE, 0).to_vec();
        madt.extend_from_slice(&[0u8; 8]); // local-APIC addr + flags → offset 44
        // Local APIC id 5, enabled (type 0, len 8; id @off+3, flags @off+4).
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 0, 5, 0x01, 0, 0, 0]);
        // Local APIC id 3, disabled → skipped.
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 0, 3, 0x00, 0, 0, 0]);
        // Local x2APIC id 0x1000_0001, enabled (type 9, len 16; id @off+4).
        madt.extend_from_slice(&[
            MADT_LOCAL_X2APIC,
            16,
            0,
            0,
            0x01,
            0x00,
            0x00,
            0x10,
            0x01,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        let mut ids = [0u32; 4];
        let count = madt_enabled_apic_ids(&madt, &mut ids);
        assert_eq!(count, 2);
        assert_eq!(&ids[..count], &[5, 0x1000_0001]);
    }

    #[test]
    fn madt_apic_id_count_exceeds_a_small_buffer() {
        let mut madt = sdt_header(*MADT_SIGNATURE, 0).to_vec();
        madt.extend_from_slice(&[0u8; 8]);
        for id in 0..4u8 {
            madt.extend_from_slice(&[MADT_LOCAL_APIC, 8, 0, id, 0x01, 0, 0, 0]);
        }
        // Buffer holds only 2 — the true count (4) is still returned.
        let mut ids = [0u32; 2];
        let count = madt_enabled_apic_ids(&madt, &mut ids);
        assert_eq!(count, 4);
        assert_eq!(ids, [0, 1]);
    }

    #[test]
    fn mcfg_reads_the_first_ecam_allocation() {
        let mut mcfg = sdt_header(*MCFG_SIGNATURE, 0).to_vec();
        mcfg.extend_from_slice(&[0u8; 8]); // reserved
        // Allocation: base 0xB000_0000, segment 0, start bus 0, end bus 0xFF.
        mcfg.extend_from_slice(&0xB000_0000u64.to_le_bytes());
        mcfg.extend_from_slice(&0u16.to_le_bytes()); // segment
        mcfg.push(0x00); // start bus
        mcfg.push(0xFF); // end bus
        mcfg.extend_from_slice(&[0u8; 4]); // reserved
        assert_eq!(
            mcfg_first_allocation(&mcfg),
            Some((0xB000_0000, 0, 0x00, 0xFF))
        );
    }

    #[test]
    fn iommu_kind_classifies_dmar_and_ivrs() {
        assert_eq!(
            iommu_kind_from_signature(*DMAR_SIGNATURE),
            IommuKind::IntelVtd
        );
        assert_eq!(iommu_kind_from_signature(*IVRS_SIGNATURE), IommuKind::AmdVi);
        assert_eq!(iommu_kind_from_signature(*MADT_SIGNATURE), IommuKind::None);
        assert_eq!(IommuKind::default(), IommuKind::None);
    }

    #[test]
    fn mcfg_rejects_a_truncated_table() {
        let mcfg = sdt_header(*MCFG_SIGNATURE, 0).to_vec();
        assert_eq!(mcfg_first_allocation(&mcfg), None);
    }

    #[test]
    fn madt_stops_on_a_zero_length_structure() {
        let mut madt = sdt_header(*MADT_SIGNATURE, 0).to_vec();
        madt.extend_from_slice(&[0u8; 8]);
        // A malformed zero-length structure must not spin.
        madt.extend_from_slice(&[MADT_LOCAL_APIC, 0, 0, 0]);
        assert_eq!(madt_enabled_cpu_count(&madt), 0);
    }
}

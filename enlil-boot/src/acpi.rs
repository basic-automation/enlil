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

// ---------------------------------------------------------------------------
// `IVRS` (AMD-Vi) body parsing.
//
// The VT-d/`DMAR` side above is done; this is the AMD side — the IOMMU this
// project's physical hardware actually has. QEMU cannot emulate AMD-Vi, so
// this decode is host-tested against synthetic tables until a physical run
// (the roadmap's own precedent). Layouts follow the AMD I/O Virtualization
// Technology (IOMMU) Specification, §5.2 (IVRS / IVHD / IVMD).
// ---------------------------------------------------------------------------

/// Offset of the `IVinfo` field within an `IVRS` table.
///
/// The table is the 36-byte SDT header, then the 4-byte `IVinfo`, then 8
/// reserved bytes before the first IVDB (spec §5.2.1, Table 83).
pub const IVRS_INFO_OFFSET: usize = SDT_HEADER_LEN;

/// Offset of the first I/O Virtualization Definition Block (IVDB) in an
/// `IVRS` table: SDT header (36) + `IVinfo` (4) + reserved (8).
pub const IVRS_IVDBS_OFFSET: usize = SDT_HEADER_LEN + 12;

/// IVHD block type 10h: fixed-length assigned-DeviceID entries, legacy format.
pub const IVHD_TYPE_10: u8 = 0x10;
/// IVHD block type 11h: fixed-length entries plus IOMMU attributes and the EFR
/// images.
pub const IVHD_TYPE_11: u8 = 0x11;
/// IVHD block type 40h: mixed format — fixed entries plus variable-length ACPI
/// HID entries.
pub const IVHD_TYPE_40: u8 = 0x40;

/// IVMD block type 20h: memory definition for all peripherals.
pub const IVMD_TYPE_ALL: u8 = 0x20;
/// IVMD block type 21h: memory definition for one specified peripheral.
pub const IVMD_TYPE_ONE: u8 = 0x21;
/// IVMD block type 22h: memory definition for a peripheral range.
pub const IVMD_TYPE_RANGE: u8 = 0x22;

/// Offset of the first device entry in a type 10h IVHD (24-byte fixed header).
pub const IVHD_10_ENTRIES_OFFSET: usize = 24;
/// Offset of the first device entry in a type 11h/40h IVHD (40-byte fixed
/// header).
pub const IVHD_11_40_ENTRIES_OFFSET: usize = 40;

/// Fixed length of an IVMD block in bytes (spec Table 109).
pub const IVMD_BLOCK_LEN: usize = 32;

/// IVHD flag: recommended `HtTunEn` setting (bit 0).
pub const IVHD_FLAG_HT_TUN_EN: u8 = 1 << 0;
/// IVHD flag: recommended `PassPW` setting (bit 1).
pub const IVHD_FLAG_PASS_PW: u8 = 1 << 1;
/// IVHD flag: recommended `ResPassPW` setting (bit 2).
pub const IVHD_FLAG_RES_PASS_PW: u8 = 1 << 2;
/// IVHD flag: recommended `Isoc` setting (bit 3).
pub const IVHD_FLAG_ISOC: u8 = 1 << 3;
/// IVHD flag: remote IOTLB support, `IotlbSup` (bit 4).
pub const IVHD_FLAG_IOTLB_SUP: u8 = 1 << 4;
/// IVHD flag: recommended `Coherent` setting (bit 5).
pub const IVHD_FLAG_COHERENT: u8 = 1 << 5;
/// IVHD flag: `PreFSup` — type 10h only (bit 6).
pub const IVHD_FLAG_PRE_F_SUP: u8 = 1 << 6;
/// IVHD flag: `PPRSup` — type 10h only (bit 7).
pub const IVHD_FLAG_PPR_SUP: u8 = 1 << 7;

/// IVMD flag: unity mapping — virtual addresses must equal physical (bit 0).
pub const IVMD_FLAG_UNITY: u8 = 1 << 0;
/// IVMD flag: peripherals may read the range, `IR` (bit 1).
pub const IVMD_FLAG_IR: u8 = 1 << 1;
/// IVMD flag: peripherals may write the range, `IW` (bit 2).
pub const IVMD_FLAG_IW: u8 = 1 << 2;
/// IVMD flag: the range is excluded from every peripheral's address space
/// (bit 3).
pub const IVMD_FLAG_EXCLUSION: u8 = 1 << 3;

/// IVHD 4-byte device-entry type 1: the DTE setting applies to all `DeviceID`s
/// the IOMMU controls.
pub const IVHD_ENTRY_ALL: u8 = 1;
/// IVHD 4-byte device-entry type 2: the DTE setting applies to one `DeviceID`.
pub const IVHD_ENTRY_SELECT: u8 = 2;
/// IVHD 4-byte device-entry type 3: first `DeviceID` of an inclusive range (ends
/// at a type 4 entry).
pub const IVHD_ENTRY_RANGE_START: u8 = 3;
/// IVHD 4-byte device-entry type 4: last `DeviceID` of an inclusive range.
pub const IVHD_ENTRY_RANGE_END: u8 = 4;
/// IVHD 8-byte device-entry type 42h: alias select — the peripheral's `DeviceID`
/// is remapped to the entry's second `DeviceID` as its source ID.
pub const IVHD_ENTRY_ALIAS_SELECT: u8 = 0x42;
/// IVHD 8-byte device-entry type 43h: alias start of range (ends at a type 4
/// entry).
pub const IVHD_ENTRY_ALIAS_RANGE_START: u8 = 0x43;
/// IVHD 8-byte device-entry type 46h: extended select, with an extended DTE
/// setting word.
pub const IVHD_ENTRY_EXT_SELECT: u8 = 0x46;
/// IVHD 8-byte device-entry type 47h: extended start of range (ends at a type
/// 4 entry).
pub const IVHD_ENTRY_EXT_RANGE_START: u8 = 0x47;
/// IVHD 8-byte device-entry type 48h: special device — an I/O APIC or HPET
/// named by handle, not by PCI enumeration.
pub const IVHD_ENTRY_SPECIAL: u8 = 0x48;
/// IVHD variable-length device-entry type F0h: ACPI HID-named device.
pub const IVHD_ENTRY_ACPI_HID: u8 = 0xF0;

/// Special-device variety 01h: I/O APIC — the handle is the APIC ID from the
/// MADT.
pub const IVHD_SPECIAL_IOAPIC: u8 = 0x01;
/// Special-device variety 02h: HPET — the handle is the HPET number.
pub const IVHD_SPECIAL_HPET: u8 = 0x02;

/// The `IVinfo` field shared by every IOMMU in an `IVRS` table (spec Table 85).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IvrsInfo {
    /// Extended Feature Register support (`EFRSup`, bit 0).
    pub efr_supported: bool,
    /// Pre-boot DMA protection: the IOMMU remaps device-accessed memory after
    /// the OS loads (`DMA remap support`, bit 1).
    pub dma_remap: bool,
    /// Guest virtual address width (`GVAsize`, bits 7:5).
    pub gva_size: u8,
    /// System physical address width (`PAsize`, bits 14:8).
    pub pa_size: u8,
    /// Guest physical address width when guest translation is supported
    /// (`VAsize`, bits 21:15).
    pub va_size: u8,
    /// ATS response address-translation range reserved (`HtAtsResv`, bit 22).
    pub ht_ats_resv: bool,
}

/// Decode the `IVinfo` field at `IVRS` offset 36, or `None` when the table is
/// too short to hold it.
#[must_use]
pub fn ivrs_info(ivrs: &[u8]) -> Option<IvrsInfo> {
    let raw = read_u32(ivrs, IVRS_INFO_OFFSET)?;
    Some(IvrsInfo {
        efr_supported: raw & 0x1 != 0,
        dma_remap: raw & 0x2 != 0,
        gva_size: u8::try_from((raw >> 5) & 0x7).unwrap_or(0),
        pa_size: u8::try_from((raw >> 8) & 0x7f).unwrap_or(0),
        va_size: u8::try_from((raw >> 15) & 0x7f).unwrap_or(0),
        ht_ats_resv: raw & (0x1 << 22) != 0,
    })
}

/// One AMD-Vi IOMMU from an `IVRS` table's IVHD blocks (spec §5.2.2.1).
///
/// An IVHD block describes one physical IOMMU: where its register block lives
/// and which PCI segment and devices it governs. Programming DMA remapping on
/// AMD hardware (ROADMAP 6.4) starts here — every device-table and page-table
/// write goes through this MMIO base, and the IOMMU's own `DeviceID` selects its
/// PCI capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AmdIommuUnit {
    /// The IVHD block type: `0x10`, `0x11` or `0x40`.
    pub block_type: u8,
    /// Recommended IOMMU control-field settings (spec Tables 89/94).
    pub flags: u8,
    /// The IOMMU's own `DeviceID` (selects its PCI capability via `cap_offset`).
    pub iommu_devid: u16,
    /// Offset in PCI capability space of the IOMMU's control fields.
    pub cap_offset: u16,
    /// Physical base of the IOMMU's MMIO register block.
    pub mmio_base: u64,
    /// PCI segment group the IOMMU and its peripherals share.
    pub pci_segment: u16,
    /// IOMMU info: `UnitID` (bits 12:8) and event-log MSI number (bits 4:0).
    pub info: u16,
    /// The `u32` at IVHD offset 20: IOMMU Attributes (11h/40h) or IOMMU
    /// Feature Reporting (10h).
    pub attributes: u32,
    /// Image of the IOMMU Extended Feature Register (11h/40h; 0 for 10h).
    pub efr: u64,
    /// Image of the IOMMU Extended Feature 2 Register (11h/40h; 0 for 10h).
    pub efr2: u64,
    /// Bytes of device entries following the fixed IVHD fields.
    pub device_entry_bytes: usize,
    /// Byte offset of this unit's first device entry within the `IVRS`
    /// table — where [`ivrs_device_entries`] reads from.
    pub entry_offset: usize,
}

impl AmdIommuUnit {
    /// The IOMMU's `UnitID` number (`info` bits 12:8).
    #[must_use]
    pub const fn unit_id(self) -> u8 {
        self.info.to_le_bytes()[1] & 0x1f
    }

    /// The event-log MSI message number (`info` bits 4:0).
    #[must_use]
    pub const fn msi_num(self) -> u8 {
        self.info.to_le_bytes()[0] & 0x1f
    }
}

/// What kind of device an IVHD device entry names (spec §5.2.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IvrsEntryKind {
    /// All `DeviceID`s the IOMMU controls (type 1).
    All,
    /// One `DeviceID` (type 2).
    Select,
    /// First `DeviceID` of an inclusive range (type 3; ends at a
    /// [`RangeEnd`](Self::RangeEnd)).
    RangeStart,
    /// Last `DeviceID` of an inclusive range (type 4).
    RangeEnd,
    /// Alias select: the peripheral at [`IvrsDeviceEntry::devid`] uses
    /// [`IvrsDeviceEntry::devid_b`] as its source `DeviceID` (type 42h).
    AliasSelect,
    /// Alias start of range (type 43h; ends at a [`RangeEnd`](Self::RangeEnd)).
    AliasRangeStart,
    /// Extended select, with an extended DTE setting word (type 46h).
    ExtSelect,
    /// Extended start of range (type 47h; ends at a [`RangeEnd`](Self::RangeEnd)).
    ExtRangeStart,
    /// Special device: an I/O APIC or HPET named by handle (type 48h).
    Special,
    /// ACPI HID-named device, variable-length (type F0h).
    AcpiHid,
    /// A type this decoder does not recognise.
    #[default]
    Unknown,
}

impl IvrsEntryKind {
    /// Decode the device-entry type byte.
    #[must_use]
    pub const fn from_type(raw: u8) -> Self {
        match raw {
            IVHD_ENTRY_ALL => Self::All,
            IVHD_ENTRY_SELECT => Self::Select,
            IVHD_ENTRY_RANGE_START => Self::RangeStart,
            IVHD_ENTRY_RANGE_END => Self::RangeEnd,
            IVHD_ENTRY_ALIAS_SELECT => Self::AliasSelect,
            IVHD_ENTRY_ALIAS_RANGE_START => Self::AliasRangeStart,
            IVHD_ENTRY_EXT_SELECT => Self::ExtSelect,
            IVHD_ENTRY_EXT_RANGE_START => Self::ExtRangeStart,
            IVHD_ENTRY_SPECIAL => Self::Special,
            IVHD_ENTRY_ACPI_HID => Self::AcpiHid,
            _ => Self::Unknown,
        }
    }

    /// A short human name for the serial report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Select => "select",
            Self::RangeStart => "range-start",
            Self::RangeEnd => "range-end",
            Self::AliasSelect => "alias",
            Self::AliasRangeStart => "alias-range",
            Self::ExtSelect => "ext-select",
            Self::ExtRangeStart => "ext-range",
            Self::Special => "special",
            Self::AcpiHid => "acpi-hid",
            Self::Unknown => "unknown",
        }
    }
}

/// One device an AMD-Vi IOMMU governs, decoded from an IVHD device entry.
///
/// Says *which* devices an IOMMU virtualizes — the input to assigning a
/// passed-through device to a per-guest DMA domain (LOCKED PRINCIPLE 5). A
/// device no IOMMU covers cannot be isolated, so passthrough must not be
/// offered for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IvrsDeviceEntry {
    /// What kind of device the entry names.
    pub kind: IvrsEntryKind,
    /// The `DeviceID` (for alias entries, the peripheral's actual `DeviceID`).
    pub devid: u16,
    /// The alias source `DeviceID`: the `DeviceID` used as source by the
    /// peripheral (42h/43h), or the special device's source `DeviceID` (48h).
    pub devid_b: u16,
    /// The DTE setting byte (spec Table 103): LINT/NMI/ExtInt/INIT passthrough
    /// and system-management-message capability.
    pub dte: u8,
    /// The extended DTE setting word (46h/47h; bit 31 is `AtsDisabled`).
    pub ext_dte: u32,
    /// Special-device handle (48h): the I/O APIC ID or HPET number.
    pub handle: u8,
    /// Special-device variety (48h): `01h` I/O APIC, `02h` HPET.
    pub variety: u8,
    /// The ACPI Hardware ID bytes (F0h): an 8-byte ACPI/PNP string, or a
    /// 32-bit integer in the low 4 bytes.
    pub hid: u64,
    /// Length of the F0h entry's UID field in bytes (0 when absent).
    pub uid_len: u8,
}

/// Which peripherals an `IVMD` memory-definition block applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IvmdKind {
    /// Type 20h: all peripherals.
    All,
    /// Type 21h: the one peripheral in [`IvmdRange::devid`].
    Specified,
    /// Type 22h: the inclusive `DeviceID` range from
    /// [`IvmdRange::devid`] to [`IvmdRange::devid_end`].
    Range,
    /// A type this decoder does not recognise.
    #[default]
    Unknown,
}

impl IvmdKind {
    /// Decode the IVMD block type byte.
    #[must_use]
    pub const fn from_type(raw: u8) -> Self {
        match raw {
            IVMD_TYPE_ALL => Self::All,
            IVMD_TYPE_ONE => Self::Specified,
            IVMD_TYPE_RANGE => Self::Range,
            _ => Self::Unknown,
        }
    }

    /// A short human name for the serial report.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Specified => "specified",
            Self::Range => "range",
            Self::Unknown => "unknown",
        }
    }
}

/// One `IVMD` memory-definition block from an `IVRS` table (spec §5.2.2.2).
///
/// Firmware-declared memory ranges the IOMMU must (or must not) map for
/// peripherals: unity-mapped BIOS regions, and exclusion ranges DMA must never
/// touch. Programming the IOMMU (ROADMAP 6.4) has to honour these — mapping an
/// exclusion range into a guest would hand it firmware memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IvmdRange {
    /// Which peripherals the range applies to.
    pub kind: IvmdKind,
    /// Type 21h: the peripheral's `DeviceID`; type 22h: the range's first
    /// `DeviceID`.
    pub devid: u16,
    /// Type 22h: the range's last `DeviceID` (inclusive).
    pub devid_end: u16,
    /// System physical address where the range starts.
    pub start: u64,
    /// Length of the range in bytes.
    pub length: u64,
    /// The raw flags byte: `Unity`/`IR`/`IW`/`ExclusionRange`
    /// (`IVMD_FLAG_*`).
    pub flags: u8,
}

impl IvmdRange {
    /// Peripherals may read the range (`IR`).
    #[must_use]
    pub const fn readable(self) -> bool {
        self.flags & IVMD_FLAG_IR != 0
    }

    /// Peripherals may write the range (`IW`).
    #[must_use]
    pub const fn writable(self) -> bool {
        self.flags & IVMD_FLAG_IW != 0
    }

    /// Virtual addresses must equal physical addresses (`Unity`).
    #[must_use]
    pub const fn unity(self) -> bool {
        self.flags & IVMD_FLAG_UNITY != 0
    }

    /// The range is excluded from every peripheral's address space
    /// (`ExclusionRange`).
    #[must_use]
    pub const fn exclusion(self) -> bool {
        self.flags & IVMD_FLAG_EXCLUSION != 0
    }
}

/// Split an AMD-Vi `DeviceID` into `(bus, device, function)`.
///
/// The `DeviceID` packs the PCI BDF as `(bus << 8) | (device << 3) | function`
/// (spec §5.2.2.1).
#[must_use]
pub const fn ivrs_devid_bdf(devid: u16) -> (u8, u8, u8) {
    let bytes = devid.to_le_bytes();
    (bytes[1], (bytes[0] >> 3) & 0x1f, bytes[0] & 0x7)
}

/// The next IVDB at `*off`: `(block type, block start, block length)`.
///
/// Advances `*off` past the block. Returns `None` when the table ends, the
/// block header is truncated, or a block has a zero/under-header length or
/// runs past the table — a malformed block ends the walk rather than looping
/// or over-reading.
fn next_ivdb(ivrs: &[u8], off: &mut usize, end: usize) -> Option<(u8, usize, usize)> {
    if off.checked_add(4)? > end {
        return None;
    }
    let block_type = ivrs.get(*off).copied()?;
    let block_len = usize::from(read_u16(ivrs, off.checked_add(2)?)?);
    let block_end = off.checked_add(block_len)?;
    if block_len < 4 || block_end > end {
        return None;
    }
    let start = *off;
    *off = block_end;
    Some((block_type, start, block_len))
}

/// Parse the `IVRS` table's body, collecting each AMD-Vi IOMMU into `out`, and
/// return how many were found.
///
/// Walks the IVDBs after the 48-byte `IVRS` header, selecting the IVHD blocks
/// (types 10h/11h/40h) and decoding each one's flags, `DeviceID`, capability
/// offset, MMIO base, PCI segment, IOMMU info, attributes and EFR images
/// (spec §5.2.2.1). `IVMD` blocks and reserved types are skipped by their
/// length.
///
/// The returned count is the true number present even if it exceeds `out`, so
/// a caller can tell it needs a bigger buffer. A malformed block ends the walk
/// rather than looping or reading past the table.
#[must_use]
pub fn ivrs_iommu_units(ivrs: &[u8], out: &mut [AmdIommuUnit]) -> usize {
    let Some(length) = sdt_length(ivrs) else {
        return 0;
    };
    let end = (length as usize).min(ivrs.len());
    let mut off = IVRS_IVDBS_OFFSET;
    let mut found = 0usize;

    while let Some((block_type, start, block_len)) = next_ivdb(ivrs, &mut off, end) {
        let entries_offset = match block_type {
            IVHD_TYPE_10 => IVHD_10_ENTRIES_OFFSET,
            IVHD_TYPE_11 | IVHD_TYPE_40 => IVHD_11_40_ENTRIES_OFFSET,
            // `IVMD` blocks and reserved types describe no IOMMU: skip them.
            _ => continue,
        };
        if block_len < entries_offset {
            // Malformed: the fixed header does not fit; skip the block.
            continue;
        }
        // The fixed header fits inside the block, so these reads are in
        // bounds; the `let-else` is belt-and-braces.
        let (Some(iommu_devid), Some(cap_offset), Some(mmio_base), Some(pci_segment), Some(info)) = (
            read_u16(ivrs, start + 4),
            read_u16(ivrs, start + 6),
            read_u64(ivrs, start + 8),
            read_u16(ivrs, start + 16),
            read_u16(ivrs, start + 18),
        ) else {
            continue;
        };
        let (attributes, efr, efr2) = match block_type {
            IVHD_TYPE_11 | IVHD_TYPE_40 => {
                let (Some(attributes), Some(efr), Some(efr2)) = (
                    read_u32(ivrs, start + 20),
                    read_u64(ivrs, start + 24),
                    read_u64(ivrs, start + 32),
                ) else {
                    continue;
                };
                (attributes, efr, efr2)
            }
            // Type 10h carries the IOMMU Feature Reporting word at offset 20.
            _ => (read_u32(ivrs, start + 20).unwrap_or(0), 0, 0),
        };
        if let Some(slot) = out.get_mut(found) {
            *slot = AmdIommuUnit {
                block_type,
                flags: ivrs[start + 1],
                iommu_devid,
                cap_offset,
                mmio_base,
                pci_segment,
                info,
                attributes,
                efr,
                efr2,
                device_entry_bytes: block_len - entries_offset,
                entry_offset: start + entries_offset,
            };
        }
        found += 1;
    }
    found
}

/// Decode one IVHD device entry, already sliced to its exact length.
///
/// Returns `None` for the type-0 pad entry, which is alignment filler and
/// names no device. Reserved fixed-length types decode as
/// [`IvrsEntryKind::Unknown`] but still advance the walk.
fn decode_ivhd_entry(entry: &[u8]) -> Option<IvrsDeviceEntry> {
    let raw = entry.first().copied()?;
    if raw == 0 {
        return None;
    }
    let mut decoded = IvrsDeviceEntry {
        kind: IvrsEntryKind::from_type(raw),
        devid: u16::from_le_bytes([entry[1], entry[2]]),
        dte: entry[3],
        ..IvrsDeviceEntry::default()
    };
    match raw {
        IVHD_ENTRY_ALIAS_SELECT | IVHD_ENTRY_ALIAS_RANGE_START => {
            decoded.devid_b = u16::from_le_bytes([entry[5], entry[6]]);
        }
        IVHD_ENTRY_EXT_SELECT | IVHD_ENTRY_EXT_RANGE_START => {
            decoded.ext_dte = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]);
        }
        IVHD_ENTRY_SPECIAL => {
            decoded.handle = entry[4];
            decoded.devid_b = u16::from_le_bytes([entry[5], entry[6]]);
            decoded.variety = entry[7];
        }
        IVHD_ENTRY_ACPI_HID => {
            decoded.hid = u64::from_le_bytes(entry[4..12].try_into().ok()?);
            decoded.uid_len = entry[21];
        }
        _ => {}
    }
    Some(decoded)
}

/// Parse the device entries of one AMD-Vi IOMMU into `out`, returning how many
/// were found.
///
/// `ivrs` is the whole table and `unit` an [`AmdIommuUnit`] from
/// [`ivrs_iommu_units`]. Entry lengths come from the type byte's upper bits
/// (spec Table 100): types `00h`–`3Fh` are 4 bytes, `40h`–`7Fh` are 8 bytes,
/// and only `F0h` has a defined variable-length layout (`22 + UID length`).
/// A reserved variable-length type ends the walk — its size is unknowable, so
/// guessing would desynchronise every entry after it.
///
/// The returned count is the true number present even if it exceeds `out`.
#[must_use]
pub fn ivrs_device_entries(ivrs: &[u8], unit: &AmdIommuUnit, out: &mut [IvrsDeviceEntry]) -> usize {
    let end = unit
        .entry_offset
        .saturating_add(unit.device_entry_bytes)
        .min(ivrs.len());
    let mut off = unit.entry_offset;
    let mut found = 0usize;

    while off < end {
        let Some(raw) = ivrs.get(off).copied() else {
            break;
        };
        let entry_len = if raw < 0x40 {
            4
        } else if raw < 0x80 {
            8
        } else if raw == IVHD_ENTRY_ACPI_HID {
            let Some(uid_len) = ivrs.get(off + 21).copied() else {
                break;
            };
            22 + usize::from(uid_len)
        } else {
            // Reserved variable-length type: the size cannot be known, so the
            // walk ends rather than misaligning everything after it.
            break;
        };
        if off + entry_len > end {
            break;
        }
        if let Some(entry) = decode_ivhd_entry(&ivrs[off..off + entry_len]) {
            if let Some(slot) = out.get_mut(found) {
                *slot = entry;
            }
            found += 1;
        }
        off += entry_len;
    }
    found
}

/// Parse the `IVRS` table's `IVMD` memory-definition blocks into `out`,
/// returning how many were found.
///
/// Each IVMD (types 20h/21h/22h, always 32 bytes) names a physical range with
/// read/write/unity/exclusion permissions for all, one, or a range of
/// peripherals (spec §5.2.2.2). Other IVDB types are skipped by their length.
///
/// The returned count is the true number present even if it exceeds `out`. A
/// malformed block ends the walk rather than looping or reading past the
/// table.
#[must_use]
pub fn ivrs_memory_definitions(ivrs: &[u8], out: &mut [IvmdRange]) -> usize {
    let Some(length) = sdt_length(ivrs) else {
        return 0;
    };
    let end = (length as usize).min(ivrs.len());
    let mut off = IVRS_IVDBS_OFFSET;
    let mut found = 0usize;

    while let Some((block_type, start, block_len)) = next_ivdb(ivrs, &mut off, end) {
        let kind = IvmdKind::from_type(block_type);
        if kind == IvmdKind::Unknown || block_len != IVMD_BLOCK_LEN {
            continue;
        }
        let (Some(devid), Some(aux), Some(range_start), Some(range_length)) = (
            read_u16(ivrs, start + 4),
            read_u16(ivrs, start + 6),
            read_u64(ivrs, start + 16),
            read_u64(ivrs, start + 24),
        ) else {
            continue;
        };
        let flags = ivrs[start + 1];
        if let Some(slot) = out.get_mut(found) {
            *slot = IvmdRange {
                kind,
                devid,
                devid_end: aux,
                start: range_start,
                length: range_length,
                flags,
            };
        }
        found += 1;
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
    /// The first [`MAX_REPORTED_AMD_UNITS`] AMD-Vi IOMMUs from the `IVRS` —
    /// the MMIO register blocks Phase 6.4 programs on AMD hardware.
    pub amd_units: [AmdIommuUnit; MAX_REPORTED_AMD_UNITS],
    /// How many IVHD blocks the `IVRS` declared (may exceed the array).
    pub amd_unit_count: usize,
    /// The devices the **first** AMD-Vi IOMMU governs — which hardware it
    /// virtualizes, and so what can be isolated for passthrough.
    pub amd_device_entries: [IvrsDeviceEntry; MAX_REPORTED_AMD_ENTRIES],
    /// How many device entries the first IVHD declared (may exceed the array).
    pub amd_device_entry_count: usize,
    /// The first [`MAX_REPORTED_IVMDS`] `IVMD` memory definitions from the
    /// `IVRS` — unity/exclusion ranges DMA programming must honour.
    pub ivmds: [IvmdRange; MAX_REPORTED_IVMDS],
    /// How many `IVMD` blocks the `IVRS` declared (may exceed the array).
    pub ivmd_count: usize,
}

/// How many device-scope entries the summary keeps for the first unit.
pub const MAX_REPORTED_DEVICE_SCOPES: usize = 4;

/// How many DMA-remapping hardware units the summary keeps.
///
/// Real platforms have one per PCI segment plus a catch-all; a handful is ample
/// for the boot report without a heap allocation.
pub const MAX_REPORTED_REMAPPING_UNITS: usize = 4;

/// How many AMD-Vi IOMMUs the summary keeps.
///
/// Real AMD platforms have one IOMMU per PCI segment; a handful is ample for
/// the boot report without a heap allocation.
pub const MAX_REPORTED_AMD_UNITS: usize = 4;

/// How many IVHD device entries the summary keeps for the first AMD-Vi IOMMU.
pub const MAX_REPORTED_AMD_ENTRIES: usize = 8;

/// How many `IVMD` memory definitions the summary keeps.
pub const MAX_REPORTED_IVMDS: usize = 4;

#[cfg(target_os = "uefi")]
pub use hw::discover;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        AcpiSummary, AmdIommuUnit, DMAR_SIGNATURE, DeviceScope, IVRS_SIGNATURE, IommuKind,
        IvmdRange, IvrsDeviceEntry, MADT_SIGNATURE, MAX_REPORTED_APIC_IDS, MCFG_SIGNATURE,
        RemappingUnit, SDT_HEADER_LEN, dmar_device_scopes, dmar_remapping_units,
        iommu_kind_from_signature, ivrs_device_entries, ivrs_iommu_units, ivrs_memory_definitions,
        madt_enabled_apic_ids, madt_enabled_cpu_count, mcfg_first_allocation, rsdp_xsdt_address,
        sdt_length, sdt_signature, xsdt_entry, xsdt_entry_count,
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
            amd_units: [AmdIommuUnit::default(); super::MAX_REPORTED_AMD_UNITS],
            amd_unit_count: 0,
            amd_device_entries: [IvrsDeviceEntry::default(); super::MAX_REPORTED_AMD_ENTRIES],
            amd_device_entry_count: 0,
            ivmds: [IvmdRange::default(); super::MAX_REPORTED_IVMDS],
            ivmd_count: 0,
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
                } else if sig == *IVRS_SIGNATURE {
                    // The MMIO register blocks DMA remapping is programmed
                    // through on AMD hardware, which devices the first IOMMU
                    // governs, and the firmware's unity/exclusion ranges.
                    summary.amd_unit_count = ivrs_iommu_units(table, &mut summary.amd_units);
                    if summary.amd_unit_count > 0 {
                        summary.amd_device_entry_count = ivrs_device_entries(
                            table,
                            &summary.amd_units[0],
                            &mut summary.amd_device_entries,
                        );
                    }
                    summary.ivmd_count = ivrs_memory_definitions(table, &mut summary.ivmds);
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

#[cfg(test)]
mod ivrs_tests {
    use super::*;

    /// Build a minimal SDT header with `sig` and `length`.
    fn sdt_header(sig: [u8; 4], length: u32) -> [u8; SDT_HEADER_LEN] {
        let mut h = [0u8; SDT_HEADER_LEN];
        h[..4].copy_from_slice(&sig);
        h[4..8].copy_from_slice(&length.to_le_bytes());
        h
    }

    /// Build an `IVRS` table: SDT header + `IVinfo` + 8 reserved bytes + IVDBs.
    fn ivrs_table(ivinfo: u32, blocks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = blocks.concat();
        let length = u32::try_from(IVRS_IVDBS_OFFSET + body.len()).unwrap();
        let mut table = sdt_header(*IVRS_SIGNATURE, length).to_vec();
        table.extend_from_slice(&ivinfo.to_le_bytes());
        table.extend_from_slice(&[0u8; 8]); // reserved
        table.extend_from_slice(&body);
        table
    }

    /// The 19 bytes after an IVHD block's type byte: flags(1), length
    /// placeholder(2), `DeviceID`(2), capability offset(2), MMIO base(8),
    /// PCI segment(2), IOMMU info(2).
    fn ivhd_head(flags: u8, devid: u16, cap: u16, mmio: u64, seg: u16, info: u16) -> [u8; 19] {
        let mut h = [0u8; 19];
        h[0] = flags;
        // Bytes 1..3 stay zero: the length placeholder `ivhd` fills in.
        h[3..5].copy_from_slice(&devid.to_le_bytes());
        h[5..7].copy_from_slice(&cap.to_le_bytes());
        h[7..15].copy_from_slice(&mmio.to_le_bytes());
        h[15..17].copy_from_slice(&seg.to_le_bytes());
        h[17..19].copy_from_slice(&info.to_le_bytes());
        h
    }

    /// An IVHD block: type byte, `head`, `tail`, then raw device entries.
    ///
    /// `tail` is the bytes between offset 20 and the first device entry: the
    /// 4-byte feature-reporting word for 10h, or attributes(4) + EFR(8) +
    /// EFR2(8) for 11h/40h.
    fn ivhd(block_type: u8, head: &[u8; 19], tail: &[u8], entries: &[u8]) -> Vec<u8> {
        let fixed = match block_type {
            IVHD_TYPE_10 => IVHD_10_ENTRIES_OFFSET,
            _ => IVHD_11_40_ENTRIES_OFFSET,
        };
        assert_eq!(tail.len(), fixed - 20);
        let mut b = Vec::new();
        b.push(block_type);
        b.extend_from_slice(head);
        b.extend_from_slice(tail);
        b.extend_from_slice(entries);
        let len = u16::try_from(b.len()).unwrap();
        b[2..4].copy_from_slice(&len.to_le_bytes());
        b
    }

    /// A 4-byte IVHD device entry.
    fn entry4(raw: u8, devid: u16, dte: u8) -> Vec<u8> {
        let mut e = vec![raw];
        e.extend_from_slice(&devid.to_le_bytes());
        e.push(dte);
        e
    }

    /// An 8-byte IVHD device entry.
    fn entry8(raw: u8, payload: [u8; 7]) -> Vec<u8> {
        let mut e = vec![raw];
        e.extend_from_slice(&payload);
        e
    }

    /// A variable-length F0h ACPI HID entry. An empty `uid` means no UID field
    /// (format 0); otherwise the UID is a string (format 2).
    fn entry_f0(devid: u16, dte: u8, hid: [u8; 8], uid: &[u8]) -> Vec<u8> {
        let mut e = vec![IVHD_ENTRY_ACPI_HID];
        e.extend_from_slice(&devid.to_le_bytes());
        e.push(dte);
        e.extend_from_slice(&hid);
        e.extend_from_slice(&[0u8; 8]); // CID absent
        e.push(if uid.is_empty() { 0 } else { 2 }); // UID format
        e.push(u8::try_from(uid.len()).unwrap()); // UID length
        e.extend_from_slice(uid);
        e
    }

    /// An IVMD block (always 32 bytes).
    fn ivmd(block_type: u8, flags: u8, devid: u16, aux: u16, start: u64, len: u64) -> Vec<u8> {
        let mut b = vec![block_type, flags];
        b.extend_from_slice(&u16::try_from(IVMD_BLOCK_LEN).unwrap().to_le_bytes());
        b.extend_from_slice(&devid.to_le_bytes());
        b.extend_from_slice(&aux.to_le_bytes());
        b.extend_from_slice(&[0u8; 8]); // reserved
        b.extend_from_slice(&start.to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
        b
    }

    #[test]
    fn ivrs_info_decodes_the_ivinfo_field() {
        let ivinfo: u32 = 0x1                // EFRSup
            | 0x2                            // DMA remap support
            | (0x5 << 5)                     // GVAsize
            | (0x30 << 8)                    // PAsize
            | (0x40 << 15)                   // VAsize
            | (0x1 << 22); // HtAtsResv
        let table = ivrs_table(ivinfo, &[]);
        let info = ivrs_info(&table).unwrap();
        assert!(info.efr_supported);
        assert!(info.dma_remap);
        assert_eq!(info.gva_size, 0x5);
        assert_eq!(info.pa_size, 0x30);
        assert_eq!(info.va_size, 0x40);
        assert!(info.ht_ats_resv);

        let plain = ivrs_table(0, &[]);
        let info = ivrs_info(&plain).unwrap();
        assert!(!info.efr_supported);
        assert_eq!(info.va_size, 0);

        // Too short to hold the IVinfo field.
        assert_eq!(ivrs_info(&[0u8; 36]), None);
    }

    #[test]
    fn ivrs_decodes_iommu_units_from_ivhd_blocks() {
        let entries_a = [entry4(1, 0, 0), entry4(2, 0x0118, 0xC0)].concat();
        let table = ivrs_table(
            0x1, // EFRSup
            &[
                // Type 11h unit with attributes + both EFR images.
                ivhd(
                    IVHD_TYPE_11,
                    &ivhd_head(
                        IVHD_FLAG_COHERENT | IVHD_FLAG_IOTLB_SUP,
                        0x0002,
                        0x40,
                        0xFEB0_0000,
                        0,
                        0x0103,
                    ),
                    // UnitID 1, MSInum 3
                    &[
                        0x00, 0xAB, 0xCD, 0xEF, // attributes
                        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // EFR
                        0x00, 0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA, 0x99, // EFR2
                    ],
                    &entries_a,
                ),
                // An IVMD block interleaved: skipped by the unit walk.
                ivmd(IVMD_TYPE_ALL, 0x07, 0, 0, 0x1000, 0x2000),
                // Type 10h unit: feature reporting word, no EFR images.
                ivhd(
                    IVHD_TYPE_10,
                    &ivhd_head(
                        IVHD_FLAG_PPR_SUP | IVHD_FLAG_PRE_F_SUP,
                        0x0004,
                        0x40,
                        0xFEB1_0000,
                        0,
                        0x0205,
                    ),
                    &0x1234_5678u32.to_le_bytes(),
                    &[],
                ),
                // Type 40h unit.
                ivhd(
                    IVHD_TYPE_40,
                    &ivhd_head(IVHD_FLAG_COHERENT, 0x0006, 0x40, 0xFEB2_0000, 0, 0),
                    &[0u8; 20],
                    &[],
                ),
            ],
        );

        let mut units = [AmdIommuUnit::default(); 4];
        assert_eq!(ivrs_iommu_units(&table, &mut units), 3);

        let a = units[0];
        assert_eq!(a.block_type, IVHD_TYPE_11);
        assert_eq!(a.flags, IVHD_FLAG_COHERENT | IVHD_FLAG_IOTLB_SUP);
        assert_eq!(a.iommu_devid, 0x0002);
        assert_eq!(a.cap_offset, 0x40);
        assert_eq!(a.mmio_base, 0xFEB0_0000);
        assert_eq!(a.pci_segment, 0);
        assert_eq!(a.unit_id(), 1);
        assert_eq!(a.msi_num(), 3);
        assert_eq!(a.attributes, 0xEFCD_AB00);
        assert_eq!(a.efr, 0x1122_3344_5566_7788);
        assert_eq!(a.efr2, 0x99AA_BBCC_DDEE_FF00);
        assert_eq!(a.device_entry_bytes, entries_a.len());
        assert_eq!(
            a.entry_offset,
            IVRS_IVDBS_OFFSET + IVHD_11_40_ENTRIES_OFFSET
        );

        let b = units[1];
        assert_eq!(b.block_type, IVHD_TYPE_10);
        assert_eq!(b.flags, IVHD_FLAG_PPR_SUP | IVHD_FLAG_PRE_F_SUP);
        assert_eq!(b.iommu_devid, 0x0004);
        assert_eq!(b.mmio_base, 0xFEB1_0000);
        assert_eq!(b.attributes, 0x1234_5678, "10h feature reporting word");
        assert_eq!(b.efr, 0);
        assert_eq!(b.efr2, 0);
        assert_eq!(b.device_entry_bytes, 0);

        assert_eq!(units[2].block_type, IVHD_TYPE_40);
        assert_eq!(units[2].mmio_base, 0xFEB2_0000);
    }

    #[test]
    fn ivrs_unit_walk_skips_short_blocks_and_stops_on_malformed_ones() {
        let mut units = [AmdIommuUnit::default(); 4];

        // A type 11h block whose length cannot hold the fixed header is
        // skipped, not counted — the walk continues past it.
        let mut short = ivhd(
            IVHD_TYPE_11,
            &ivhd_head(0, 0x0002, 0x40, 0x1000, 0, 0),
            &[0u8; 20],
            &[],
        );
        short[2..4].copy_from_slice(&30u16.to_le_bytes());
        let mut table = ivrs_table(
            0,
            &[
                ivhd(
                    IVHD_TYPE_10,
                    &ivhd_head(0, 0x0004, 0x40, 0x2000, 0, 0),
                    &[0u8; 4],
                    &[],
                ),
                short,
            ],
        );
        assert_eq!(ivrs_iommu_units(&table, &mut units), 1);
        assert_eq!(units[0].mmio_base, 0x2000);

        // A zero-length block ends the walk instead of spinning.
        table.extend_from_slice(&[IVHD_TYPE_10, 0, 0, 0]);
        let new_len = u32::try_from(table.len()).unwrap();
        table[4..8].copy_from_slice(&new_len.to_le_bytes());
        assert_eq!(ivrs_iommu_units(&table, &mut units), 1);

        // A block claiming to run past the table end is refused.
        let mut overlong = ivrs_table(
            0,
            &[ivhd(
                IVHD_TYPE_10,
                &ivhd_head(0, 0x0004, 0x40, 0x2000, 0, 0),
                &[0u8; 4],
                &[],
            )],
        );
        overlong.extend_from_slice(&[IVHD_TYPE_11, 0, 0xFF, 0xFF]);
        let new_len = u32::try_from(overlong.len()).unwrap();
        overlong[4..8].copy_from_slice(&new_len.to_le_bytes());
        assert_eq!(ivrs_iommu_units(&overlong, &mut units), 1);

        // A truncated table is not read past its end.
        assert_eq!(ivrs_iommu_units(&[0u8; 8], &mut units), 0);
    }

    #[test]
    fn ivrs_decodes_device_entries() {
        let entries = [
            entry4(0, 0, 0), // pad: skipped, not counted
            entry4(1, 0, 0xC0),
            entry4(2, 0x0118, 0xC0),
            entry4(3, 0x0200, 0xC0),
            entry4(4, 0x02FF, 0x00),
            entry8(
                IVHD_ENTRY_ALIAS_SELECT,
                [0x18, 0x01, 0xC0, 0x00, 0x20, 0x00, 0x00],
            ),
            entry8(
                IVHD_ENTRY_ALIAS_RANGE_START,
                [0x00, 0x02, 0xC0, 0x00, 0x20, 0x00, 0x00],
            ),
            entry8(
                IVHD_ENTRY_EXT_SELECT,
                [0x18, 0x01, 0xC0, 0x01, 0x00, 0x00, 0x80],
            ),
            entry8(
                IVHD_ENTRY_EXT_RANGE_START,
                [0x00, 0x03, 0xC0, 0x00, 0x00, 0x00, 0x00],
            ),
            entry8(
                IVHD_ENTRY_SPECIAL,
                [0x00, 0x00, 0xC0, 0x02, 0x20, 0x00, 0x01],
            ),
            entry8(0x44, [0; 7]), // reserved fixed type: Unknown, still counted
            entry_f0(0x0005, 0xC0, *b"AMDI0040", &[]),
            entry_f0(0x0006, 0xC0, *b"AMDI0050", b"_SB.FUR0"),
        ]
        .concat();
        let table = ivrs_table(
            0x1,
            &[ivhd(
                IVHD_TYPE_11,
                &ivhd_head(0, 0x0002, 0x40, 0x1000, 0, 0),
                &[0u8; 20],
                &entries,
            )],
        );

        let mut units = [AmdIommuUnit::default(); 2];
        assert_eq!(ivrs_iommu_units(&table, &mut units), 1);

        let mut found = [IvrsDeviceEntry::default(); 16];
        assert_eq!(ivrs_device_entries(&table, &units[0], &mut found), 12);

        assert_eq!(found[0].kind, IvrsEntryKind::All);

        assert_eq!(found[1].kind, IvrsEntryKind::Select);
        assert_eq!(found[1].devid, 0x0118);
        assert_eq!(found[1].dte, 0xC0);
        assert_eq!(ivrs_devid_bdf(found[1].devid), (1, 3, 0));

        assert_eq!(found[2].kind, IvrsEntryKind::RangeStart);
        assert_eq!(found[2].devid, 0x0200);
        assert_eq!(found[3].kind, IvrsEntryKind::RangeEnd);
        assert_eq!(found[3].devid, 0x02FF);

        assert_eq!(found[4].kind, IvrsEntryKind::AliasSelect);
        assert_eq!(found[4].devid, 0x0118);
        assert_eq!(found[4].devid_b, 0x0020);

        assert_eq!(found[5].kind, IvrsEntryKind::AliasRangeStart);
        assert_eq!(found[5].devid, 0x0200);
        assert_eq!(found[5].devid_b, 0x0020);

        assert_eq!(found[6].kind, IvrsEntryKind::ExtSelect);
        assert_eq!(found[6].devid, 0x0118);
        assert_eq!(found[6].ext_dte, 0x8000_0001, "AtsDisabled bit");

        assert_eq!(found[7].kind, IvrsEntryKind::ExtRangeStart);
        assert_eq!(found[7].devid, 0x0300);

        assert_eq!(found[8].kind, IvrsEntryKind::Special);
        assert_eq!(found[8].handle, 2);
        assert_eq!(found[8].variety, IVHD_SPECIAL_IOAPIC);
        assert_eq!(found[8].devid_b, 0x0020);

        assert_eq!(found[9].kind, IvrsEntryKind::Unknown);

        assert_eq!(found[10].kind, IvrsEntryKind::AcpiHid);
        assert_eq!(found[10].devid, 0x0005);
        assert_eq!(found[10].hid.to_le_bytes(), *b"AMDI0040");
        assert_eq!(found[10].uid_len, 0);

        assert_eq!(found[11].kind, IvrsEntryKind::AcpiHid);
        assert_eq!(found[11].devid, 0x0006);
        assert_eq!(found[11].hid.to_le_bytes(), *b"AMDI0050");
        assert_eq!(found[11].uid_len, 8);
    }

    #[test]
    fn ivrs_device_entry_walk_stops_at_reserved_variable_types() {
        // 0x81 is a reserved variable-length type: its size is unknowable, so
        // the walk ends instead of misaligning everything after it.
        let entries = [
            entry4(2, 0x0118, 0),
            vec![0x81, 0, 0, 0],
            entry4(2, 0x0220, 0),
        ]
        .concat();
        let table = ivrs_table(
            0,
            &[ivhd(
                IVHD_TYPE_40,
                &ivhd_head(0, 0x0002, 0x40, 0x1000, 0, 0),
                &[0u8; 20],
                &entries,
            )],
        );

        let mut units = [AmdIommuUnit::default(); 1];
        assert_eq!(ivrs_iommu_units(&table, &mut units), 1);
        let mut found = [IvrsDeviceEntry::default(); 4];
        assert_eq!(ivrs_device_entries(&table, &units[0], &mut found), 1);
        assert_eq!(found[0].devid, 0x0118);
    }

    #[test]
    fn ivrs_device_entry_walk_stops_on_truncation() {
        // The last entry is cut off mid-way: the walk stops, no over-read.
        let entries = [entry4(2, 0x0118, 0), vec![0x02, 0x18]].concat();
        let table = ivrs_table(
            0,
            &[ivhd(
                IVHD_TYPE_10,
                &ivhd_head(0, 0x0002, 0x40, 0x1000, 0, 0),
                &[0u8; 4],
                &entries,
            )],
        );

        let mut units = [AmdIommuUnit::default(); 1];
        assert_eq!(ivrs_iommu_units(&table, &mut units), 1);
        let mut found = [IvrsDeviceEntry::default(); 4];
        assert_eq!(ivrs_device_entries(&table, &units[0], &mut found), 1);
    }

    #[test]
    fn ivrs_decodes_ivmd_ranges() {
        let table = ivrs_table(
            0,
            &[
                ivmd(
                    IVMD_TYPE_ALL,
                    IVMD_FLAG_UNITY | IVMD_FLAG_IR | IVMD_FLAG_IW,
                    0,
                    0,
                    0x1000,
                    0x2000,
                ),
                ivmd(IVMD_TYPE_ONE, IVMD_FLAG_IR, 0x0118, 0, 0xB_0000, 0x1_0000),
                ivmd(
                    IVMD_TYPE_RANGE,
                    IVMD_FLAG_EXCLUSION,
                    0x0200,
                    0x02FF,
                    0xC_0000,
                    0x1000,
                ),
                // Wrong length for an IVMD: skipped.
                {
                    let mut bad = ivmd(IVMD_TYPE_ONE, 0, 0, 0, 0, 0);
                    bad[2..4].copy_from_slice(&16u16.to_le_bytes());
                    bad.truncate(16);
                    bad
                },
            ],
        );

        let mut ranges = [IvmdRange::default(); 4];
        assert_eq!(ivrs_memory_definitions(&table, &mut ranges), 3);

        assert_eq!(ranges[0].kind, IvmdKind::All);
        assert_eq!((ranges[0].start, ranges[0].length), (0x1000, 0x2000));
        assert!(ranges[0].readable() && ranges[0].writable() && ranges[0].unity());
        assert!(!ranges[0].exclusion());

        assert_eq!(ranges[1].kind, IvmdKind::Specified);
        assert_eq!(ranges[1].devid, 0x0118);
        assert!(ranges[1].readable());
        assert!(!ranges[1].writable());

        assert_eq!(ranges[2].kind, IvmdKind::Range);
        assert_eq!(ranges[2].devid, 0x0200);
        assert_eq!(ranges[2].devid_end, 0x02FF);
        assert!(ranges[2].exclusion());
    }

    #[test]
    fn ivrs_reports_true_counts_past_short_buffers() {
        let table = ivrs_table(
            0,
            &[
                ivhd(
                    IVHD_TYPE_10,
                    &ivhd_head(0, 0x0002, 0x40, 0x1000, 0, 0),
                    &[0u8; 4],
                    &[],
                ),
                ivhd(
                    IVHD_TYPE_10,
                    &ivhd_head(0, 0x0004, 0x40, 0x2000, 0, 0),
                    &[0u8; 4],
                    &[],
                ),
                ivhd(
                    IVHD_TYPE_10,
                    &ivhd_head(0, 0x0006, 0x40, 0x3000, 0, 0),
                    &[0u8; 4],
                    &[],
                ),
                ivmd(IVMD_TYPE_ALL, 0, 0, 0, 0x1000, 0x1000),
                ivmd(IVMD_TYPE_ALL, 0, 0, 0, 0x2000, 0x1000),
            ],
        );

        let mut one = [AmdIommuUnit::default(); 1];
        assert_eq!(ivrs_iommu_units(&table, &mut one), 3);
        assert_eq!(one[0].mmio_base, 0x1000);

        let mut one_range = [IvmdRange::default(); 1];
        assert_eq!(ivrs_memory_definitions(&table, &mut one_range), 2);
        assert_eq!(one_range[0].start, 0x1000);

        // Entries too: three selects, one slot.
        let entries = [entry4(2, 1, 0), entry4(2, 2, 0), entry4(2, 3, 0)].concat();
        let table = ivrs_table(
            0,
            &[ivhd(
                IVHD_TYPE_10,
                &ivhd_head(0, 0x0002, 0x40, 0x1000, 0, 0),
                &[0u8; 4],
                &entries,
            )],
        );
        let mut units = [AmdIommuUnit::default(); 1];
        assert_eq!(ivrs_iommu_units(&table, &mut units), 1);
        let mut one_entry = [IvrsDeviceEntry::default(); 1];
        assert_eq!(ivrs_device_entries(&table, &units[0], &mut one_entry), 3);
        assert_eq!(one_entry[0].devid, 1);
    }

    #[test]
    fn ivrs_devid_bdf_splits_bus_device_function() {
        assert_eq!(ivrs_devid_bdf(0x0118), (1, 3, 0));
        assert_eq!(ivrs_devid_bdf(0xFFFF), (0xFF, 0x1F, 7));
        assert_eq!(ivrs_devid_bdf(0x0000), (0, 0, 0));
    }

    #[test]
    fn ivrs_entry_and_ivmd_kinds_decode_and_name() {
        assert_eq!(IvrsEntryKind::from_type(1), IvrsEntryKind::All);
        assert_eq!(IvrsEntryKind::from_type(4), IvrsEntryKind::RangeEnd);
        assert_eq!(IvrsEntryKind::from_type(0x42), IvrsEntryKind::AliasSelect);
        assert_eq!(IvrsEntryKind::from_type(0x47), IvrsEntryKind::ExtRangeStart);
        assert_eq!(IvrsEntryKind::from_type(0x48), IvrsEntryKind::Special);
        assert_eq!(IvrsEntryKind::from_type(0xF0), IvrsEntryKind::AcpiHid);
        assert_eq!(IvrsEntryKind::from_type(0x05), IvrsEntryKind::Unknown);
        assert_eq!(IvrsEntryKind::Unknown.name(), "unknown");
        assert_eq!(IvrsEntryKind::AcpiHid.name(), "acpi-hid");

        assert_eq!(IvmdKind::from_type(0x20), IvmdKind::All);
        assert_eq!(IvmdKind::from_type(0x21), IvmdKind::Specified);
        assert_eq!(IvmdKind::from_type(0x22), IvmdKind::Range);
        assert_eq!(IvmdKind::from_type(0x23), IvmdKind::Unknown);
        assert_eq!(IvmdKind::Range.name(), "range");
    }
}

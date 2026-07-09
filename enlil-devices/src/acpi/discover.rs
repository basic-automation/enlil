//! Runtime ACPI table discovery.
//!
//! The hypervisor delivers the ACPI tables to a guest via `fw_cfg`; the guest OS
//! reads them and places them in RAM wherever it likes, so the S3 resume path
//! cannot know the FACS address a priori — it must walk the guest's *live*
//! tables. This module does that walk: RSDP → XSDT/RSDT → FADT → FACS address,
//! over a view of guest RAM based at guest-physical 0.

use super::fadt::read_facs_address;

/// Read the ACPI table at guest-physical `gpa` from `mem` (RAM based at gpa 0),
/// sliced to the length in its SDT header (offset 4). `None` if the address is
/// out of range or the table overruns RAM.
fn table_at(mem: &[u8], gpa: u64) -> Option<&[u8]> {
    let start = usize::try_from(gpa).ok()?;
    let header = mem.get(start..start.checked_add(8)?)?;
    let length = u32::from_le_bytes(header[4..8].try_into().ok()?) as usize;
    mem.get(start..start.checked_add(length)?)
}

/// Find an ACPI table by its 4-byte signature in a guest's live tables.
///
/// Returns the guest-physical address of the first table whose signature matches
/// `signature` (e.g. `b"FACP"` for the FADT, `b"APIC"` for the MADT, `b"MCFG"`
/// for the `PCIe` ECAM table). Validates the RSDP at `rsdp_gpa`, follows it to
/// the XSDT (or the RSDT on an ACPI-1.0 RSDP), and scans the entries. `mem` is
/// guest RAM based at guest-physical 0. Returns `None` if any step is missing or
/// out of range. Checksums are not verified — this reads a table set enlil (or
/// the guest) built, not an adversarial one.
#[must_use]
pub fn find_table(mem: &[u8], rsdp_gpa: u64, signature: &[u8; 4]) -> Option<u64> {
    let start = usize::try_from(rsdp_gpa).ok()?;
    // ACPI 1.0 RSDP is 20 bytes (signature..RsdtAddress); 2.0+ extends to 36.
    let rsdp = mem.get(start..start.checked_add(20)?)?;
    if &rsdp[0..8] != b"RSD PTR " {
        return None;
    }
    let revision = rsdp[15];
    let legacy_rsdt = u64::from(u32::from_le_bytes(rsdp[16..20].try_into().ok()?));
    // Prefer the 64-bit XSDT (offset 24) on a 2.0+ RSDP when present.
    let desc_gpa = if revision >= 2
        && let Some(full) = mem.get(start..start.checked_add(32)?)
    {
        let ext_xsdt = u64::from_le_bytes(full[24..32].try_into().ok()?);
        if ext_xsdt != 0 { ext_xsdt } else { legacy_rsdt }
    } else {
        legacy_rsdt
    };

    let desc = table_at(mem, desc_gpa)?;
    if desc.len() < 36 {
        return None; // truncated system description table header
    }
    let entry_size = if &desc[0..4] == b"XSDT" { 8 } else { 4 };
    let mut off = 36; // past the 36-byte SDT header
    while off + entry_size <= desc.len() {
        let table_gpa = if entry_size == 8 {
            u64::from_le_bytes(desc[off..off + 8].try_into().ok()?)
        } else {
            u64::from(u32::from_le_bytes(desc[off..off + 4].try_into().ok()?))
        };
        off += entry_size;
        if let Some(table) = table_at(mem, table_gpa)
            && table.len() >= 4
            && &table[0..4] == signature
        {
            return Some(table_gpa);
        }
    }
    None
}

/// Find a guest's FACS physical address by walking its live ACPI tables.
///
/// Locates the FADT (signature `FACP`) via [`find_table`] and reads its FACS
/// pointer with [`read_facs_address`]. `mem` is guest RAM based at
/// guest-physical 0.
#[must_use]
pub fn find_facs_address(mem: &[u8], rsdp_gpa: u64) -> Option<u64> {
    let fadt_gpa = find_table(mem, rsdp_gpa, b"FACP")?;
    let fadt = table_at(mem, fadt_gpa)?;
    read_facs_address(fadt)
}

/// Discover the enabled-CPU count from the firmware's MADT (Phase 6.3).
///
/// Locates the MADT (signature `APIC`) via [`find_table`] and counts its enabled
/// processors with [`count_enabled_cpus`](super::madt::count_enabled_cpus).
/// `None` if no MADT is present.
#[must_use]
pub fn host_cpu_count(mem: &[u8], rsdp_gpa: u64) -> Option<usize> {
    let madt = table_at(mem, find_table(mem, rsdp_gpa, b"APIC")?)?;
    Some(super::madt::count_enabled_cpus(madt))
}

/// Discover the `PCIe` ECAM allocations from the firmware's MCFG (Phase 6.3).
///
/// Locates the MCFG (signature `MCFG`) via [`find_table`] and parses its
/// allocations with
/// [`parse_mcfg_allocations`](super::mcfg::parse_mcfg_allocations). Empty if no
/// MCFG is present.
#[must_use]
pub fn host_ecam_allocations(mem: &[u8], rsdp_gpa: u64) -> Vec<super::mcfg::McfgAllocation> {
    find_table(mem, rsdp_gpa, b"MCFG")
        .and_then(|gpa| table_at(mem, gpa))
        .map(super::mcfg::parse_mcfg_allocations)
        .unwrap_or_default()
}

/// Discover the host's PCI functions by walking every ECAM window (Phase 6.3).
///
/// Finds the MCFG's ECAM allocations ([`host_ecam_allocations`]) and walks each
/// window's config space with
/// [`walk_ecam_allocations`](crate::pci_discovery::walk_ecam_allocations),
/// returning every present function. `mem` must be memory based at physical
/// address 0 covering the ECAM apertures; functions whose config space is not
/// backed by `mem` are silently skipped. Empty if the firmware exposes no MCFG.
#[must_use]
pub fn host_pci_functions(mem: &[u8], rsdp_gpa: u64) -> Vec<crate::pci_discovery::PciFunction> {
    let allocations = host_ecam_allocations(mem, rsdp_gpa);
    crate::pci_discovery::walk_ecam_allocations(mem, &allocations)
}

/// Discover the host's xHCI USB controllers by walking the ECAM windows
/// (Phase 6.3 "USB controller discovery").
///
/// Finds the MCFG's ECAM allocations and returns every xHCI function paired
/// with its BAR0 MMIO base via
/// [`find_xhci_controllers`](crate::pci_discovery::find_xhci_controllers).
/// Empty if the firmware exposes no MCFG or no xHCI controller.
#[must_use]
pub fn host_xhci_controllers(
    mem: &[u8],
    rsdp_gpa: u64,
) -> Vec<crate::pci_discovery::XhciController> {
    let allocations = host_ecam_allocations(mem, rsdp_gpa);
    crate::pci_discovery::find_xhci_controllers(mem, &allocations)
}

/// Which IOMMU the firmware advertises, discovered from the ACPI tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IommuKind {
    /// Intel VT-d — advertised by the `DMAR` table.
    IntelVtd,
    /// AMD-Vi — advertised by the `IVRS` table.
    AmdVi,
}

/// Discover which IOMMU (if any) the firmware advertises (Phase 6.3 / 6.4).
///
/// A `DMAR` table means Intel VT-d, an `IVRS` table means AMD-Vi. `None` if
/// neither is present (no IOMMU, so DMA-remapped passthrough is unavailable).
#[must_use]
pub fn host_iommu_kind(mem: &[u8], rsdp_gpa: u64) -> Option<IommuKind> {
    if find_table(mem, rsdp_gpa, b"DMAR").is_some() {
        Some(IommuKind::IntelVtd)
    } else if find_table(mem, rsdp_gpa, b"IVRS").is_some() {
        Some(IommuKind::AmdVi)
    } else {
        None
    }
}

/// Discover the NUMA proximity domains from the firmware's SRAT (Phase 6.3).
///
/// Locates the SRAT (signature `SRAT`) via [`find_table`] and reads its domains
/// with [`numa_domains`](super::srat::numa_domains). Empty if no SRAT is present
/// (treat as a single implicit node).
#[must_use]
pub fn host_numa_domains(mem: &[u8], rsdp_gpa: u64) -> Vec<u32> {
    find_table(mem, rsdp_gpa, b"SRAT")
        .and_then(|gpa| table_at(mem, gpa))
        .map(super::srat::numa_domains)
        .unwrap_or_default()
}

/// Discover the NUMA memory ranges from the firmware's SRAT (Phase 6.3): which
/// proximity domain owns which physical RAM span.
///
/// Locates the SRAT via [`find_table`] and parses its enabled Memory Affinity
/// structures with [`memory_affinities`](super::srat::memory_affinities). Empty
/// if no SRAT is present.
#[must_use]
pub fn host_memory_affinities(mem: &[u8], rsdp_gpa: u64) -> Vec<super::srat::MemoryAffinity> {
    find_table(mem, rsdp_gpa, b"SRAT")
        .and_then(|gpa| table_at(mem, gpa))
        .map(super::srat::memory_affinities)
        .unwrap_or_default()
}

/// Discover the CPU→NUMA-node bindings from the firmware's SRAT (Phase 6.3):
/// which APIC ID belongs to which proximity domain.
///
/// Locates the SRAT via [`find_table`] and parses its enabled Processor Local
/// APIC Affinity structures with [`cpu_affinities`](super::srat::cpu_affinities).
/// Empty if no SRAT is present.
#[must_use]
pub fn host_cpu_affinities(mem: &[u8], rsdp_gpa: u64) -> Vec<super::srat::CpuAffinity> {
    find_table(mem, rsdp_gpa, b"SRAT")
        .and_then(|gpa| table_at(mem, gpa))
        .map(super::srat::cpu_affinities)
        .unwrap_or_default()
}

/// The host machine's topology as discovered from its ACPI tables — the input to
/// building Enlil's own device tree on the bare-metal boot path (Phase 6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTopology {
    /// Enabled CPU count from the MADT (0 if no MADT).
    pub cpu_count: usize,
    /// NUMA proximity domains from the SRAT (empty = one implicit node).
    pub numa_domains: Vec<u32>,
    /// The IOMMU the firmware advertises, if any.
    pub iommu: Option<IommuKind>,
    /// The `PCIe` ECAM allocations from the MCFG.
    pub ecam: Vec<super::mcfg::McfgAllocation>,
}

/// Discover the whole [`HostTopology`] from live ACPI tables in one call.
///
/// Runs every discovery — CPU count (MADT), NUMA domains (SRAT), IOMMU kind
/// (DMAR/IVRS), and `PCIe` ECAM (MCFG). `mem` is memory based at physical
/// address 0, `rsdp_gpa` the RSDP's address.
#[must_use]
pub fn discover_host_topology(mem: &[u8], rsdp_gpa: u64) -> HostTopology {
    HostTopology {
        cpu_count: host_cpu_count(mem, rsdp_gpa).unwrap_or(0),
        numa_domains: host_numa_domains(mem, rsdp_gpa),
        iommu: host_iommu_kind(mem, rsdp_gpa),
        ecam: host_ecam_allocations(mem, rsdp_gpa),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acpi::fadt::FadtBuilder;
    use crate::acpi::rsdp::RsdpBuilder;
    use crate::acpi::xsdt::XsdtBuilder;

    fn place(ram: &mut [u8], gpa: u64, bytes: &[u8]) {
        let o = usize::try_from(gpa).unwrap();
        ram[o..o + bytes.len()].copy_from_slice(bytes);
    }

    /// A minimal 36-byte SDT with the given signature and a correct length field,
    /// enough for the walker to identify it (no builder exists for DMAR/IVRS).
    fn fake_table(signature: [u8; 4]) -> Vec<u8> {
        let mut t = vec![0u8; 36];
        t[0..4].copy_from_slice(&signature);
        t[4..8].copy_from_slice(&36u32.to_le_bytes());
        t
    }

    #[test]
    fn discovers_the_whole_host_topology() {
        use crate::acpi::madt::MadtBuilder;
        use crate::acpi::mcfg::McfgBuilder;
        use crate::acpi::srat::{ProcessorAffinityEntry, SratBuilder};

        const MADT_GPA: u64 = 0x1000;
        const MCFG_GPA: u64 = 0x2000;
        const DMAR_GPA: u64 = 0x2800;
        const SRAT_GPA: u64 = 0x3000;
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let mut ram = vec![0u8; 0x6000];
        place(&mut ram, MADT_GPA, &MadtBuilder::standard(2).build());
        place(
            &mut ram,
            MCFG_GPA,
            &McfgBuilder::standard(0xE000_0000).build(),
        );
        place(&mut ram, DMAR_GPA, &fake_table(*b"DMAR"));
        place(
            &mut ram,
            SRAT_GPA,
            &SratBuilder::new()
                .add_processor(ProcessorAffinityEntry::new(0, 0, true))
                .add_processor(ProcessorAffinityEntry::new(1, 1, true))
                .build(),
        );
        place(
            &mut ram,
            XSDT_GPA,
            &XsdtBuilder::new()
                .add_tables(&[MADT_GPA, MCFG_GPA, DMAR_GPA, SRAT_GPA])
                .build(),
        );
        place(
            &mut ram,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );

        let topo = discover_host_topology(&ram, RSDP_GPA);
        assert_eq!(topo.cpu_count, 2);
        assert_eq!(topo.numa_domains, vec![0, 1]);
        assert_eq!(topo.iommu, Some(IommuKind::IntelVtd));
        assert_eq!(topo.ecam.len(), 1);
        assert_eq!(topo.ecam[0].base_address, 0xE000_0000);
    }

    #[test]
    fn discovers_the_iommu_kind() {
        const TBL_GPA: u64 = 0x2000;
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let build = |sig: [u8; 4]| {
            let mut ram = vec![0u8; 0x6000];
            place(&mut ram, TBL_GPA, &fake_table(sig));
            place(
                &mut ram,
                XSDT_GPA,
                &XsdtBuilder::new().add_table(TBL_GPA).build(),
            );
            place(
                &mut ram,
                RSDP_GPA,
                &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
            );
            ram
        };
        assert_eq!(
            host_iommu_kind(&build(*b"DMAR"), RSDP_GPA),
            Some(IommuKind::IntelVtd)
        );
        assert_eq!(
            host_iommu_kind(&build(*b"IVRS"), RSDP_GPA),
            Some(IommuKind::AmdVi)
        );
        // A table set with neither → no IOMMU.
        assert_eq!(host_iommu_kind(&build(*b"APIC"), RSDP_GPA), None);
    }

    #[test]
    fn walks_rsdp_xsdt_fadt_to_the_facs() {
        const FACS_GPA: u64 = 0x1000;
        const FADT_GPA: u64 = 0x2000;
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let mut ram = vec![0u8; 0x6000];

        place(
            &mut ram,
            FADT_GPA,
            &FadtBuilder::new(0x3000).firmware_ctrl(FACS_GPA).build(),
        );
        place(
            &mut ram,
            XSDT_GPA,
            &XsdtBuilder::new().add_table(FADT_GPA).build(),
        );
        place(
            &mut ram,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );

        assert_eq!(
            find_facs_address(&ram, RSDP_GPA),
            Some(FACS_GPA),
            "discovery walks RSDP → XSDT → FADT → FACS"
        );
    }

    #[test]
    fn find_table_locates_a_table_by_signature() {
        const FADT_GPA: u64 = 0x2000;
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let mut ram = vec![0u8; 0x6000];
        place(
            &mut ram,
            FADT_GPA,
            &FadtBuilder::new(0x3000).firmware_ctrl(0x1000).build(),
        );
        place(
            &mut ram,
            XSDT_GPA,
            &XsdtBuilder::new().add_table(FADT_GPA).build(),
        );
        place(
            &mut ram,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );
        // The FADT ("FACP") is found at its gpa; an absent table is None.
        assert_eq!(find_table(&ram, RSDP_GPA, b"FACP"), Some(FADT_GPA));
        assert_eq!(find_table(&ram, RSDP_GPA, b"APIC"), None, "no MADT present");
    }

    #[test]
    fn discovers_host_cpu_count_and_ecam_from_the_tables() {
        use crate::acpi::madt::MadtBuilder;
        use crate::acpi::mcfg::McfgBuilder;

        const MADT_GPA: u64 = 0x2000;
        const MCFG_GPA: u64 = 0x3000;
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let mut ram = vec![0u8; 0x6000];
        place(&mut ram, MADT_GPA, &MadtBuilder::standard(4).build());
        place(
            &mut ram,
            MCFG_GPA,
            &McfgBuilder::standard(0xE000_0000).build(),
        );
        place(
            &mut ram,
            XSDT_GPA,
            &XsdtBuilder::new()
                .add_table(MADT_GPA)
                .add_table(MCFG_GPA)
                .build(),
        );
        place(
            &mut ram,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );

        assert_eq!(
            host_cpu_count(&ram, RSDP_GPA),
            Some(4),
            "MADT enables 4 CPUs"
        );
        let ecam = host_ecam_allocations(&ram, RSDP_GPA);
        assert_eq!(ecam.len(), 1);
        assert_eq!(ecam[0].base_address, 0xE000_0000);
        // A table set without a MADT/MCFG → None / empty.
        let mut bare = vec![0u8; 0x6000];
        place(&mut bare, XSDT_GPA, &XsdtBuilder::new().build());
        place(
            &mut bare,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );
        assert_eq!(host_cpu_count(&bare, RSDP_GPA), None);
        assert!(host_ecam_allocations(&bare, RSDP_GPA).is_empty());
    }

    #[test]
    fn none_when_the_fadt_names_no_facs() {
        const FADT_GPA: u64 = 0x2000;
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let mut ram = vec![0u8; 0x6000];
        // FADT with FIRMWARE_CTRL left at 0 (no FACS).
        place(&mut ram, FADT_GPA, &FadtBuilder::new(0x3000).build());
        place(
            &mut ram,
            XSDT_GPA,
            &XsdtBuilder::new().add_table(FADT_GPA).build(),
        );
        place(
            &mut ram,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );
        assert_eq!(find_facs_address(&ram, RSDP_GPA), None);
    }

    #[test]
    fn rejects_a_bad_or_out_of_range_rsdp() {
        let ram = vec![0u8; 0x100];
        assert_eq!(find_facs_address(&ram, 0), None, "no RSD PTR signature");
        assert_eq!(
            find_facs_address(&ram, 0x1000),
            None,
            "rsdp gpa past the end of RAM"
        );
    }

    #[test]
    fn none_when_no_fadt_in_the_xsdt() {
        const XSDT_GPA: u64 = 0x4000;
        const RSDP_GPA: u64 = 0x5000;
        let mut ram = vec![0u8; 0x6000];
        // An XSDT that lists no tables at all.
        place(&mut ram, XSDT_GPA, &XsdtBuilder::new().build());
        place(
            &mut ram,
            RSDP_GPA,
            &RsdpBuilder::new().xsdt_address(XSDT_GPA).build(),
        );
        assert_eq!(find_facs_address(&ram, RSDP_GPA), None);
    }
}

//! Physical memory-map modelling and region carving (item 1.3).
//!
//! From a firmware/host memory map — the UEFI memory map on bare metal, an
//! E820-style table, or the host's view of RAM under KVM — the hypervisor carves
//! the non-overlapping physical regions it needs (its own heap, guest RAM, a
//! low DMA window, MMIO apertures). This module is the backend-neutral model and
//! the carving algorithm; the firmware-specific parsing that *produces* a
//! [`MemoryMap`] lands with the bare-metal boot path (Phase 6.3).

use super::PhysAddr;

/// The nature of a physical region, as a firmware map reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    /// Free RAM the hypervisor may carve and hand out.
    Usable,
    /// Firmware/hardware-reserved; never allocatable. Also the kind a carved
    /// span is retyped to so it is not handed out twice.
    Reserved,
    /// ACPI tables — reclaimable as RAM once parsed, but not while in use.
    AcpiReclaimable,
    /// ACPI non-volatile storage — must be preserved across boots.
    AcpiNvs,
    /// Defective RAM — never used.
    Bad,
    /// Memory-mapped I/O aperture — an address window, not backing RAM.
    Mmio,
}

impl MemoryKind {
    /// Whether a region of this kind is free RAM available for carving.
    #[must_use]
    pub const fn is_allocatable(self) -> bool {
        matches!(self, Self::Usable)
    }
}

/// A single contiguous physical region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    /// Base physical address (inclusive).
    pub base: PhysAddr,
    /// Length in bytes.
    pub size: u64,
    /// What the region is.
    pub kind: MemoryKind,
}

impl MemoryRegion {
    /// Construct a region.
    #[must_use]
    pub const fn new(base: PhysAddr, size: u64, kind: MemoryKind) -> Self {
        Self { base, size, kind }
    }

    /// One past the last byte (exclusive end).
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.base.as_u64() + self.size
    }

    /// Whether the region is zero-length.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Whether `addr` falls within the region.
    #[must_use]
    pub const fn contains(&self, addr: u64) -> bool {
        addr >= self.base.as_u64() && addr < self.end()
    }

    /// Whether two regions share any byte.
    #[must_use]
    pub const fn overlaps(&self, other: &Self) -> bool {
        self.base.as_u64() < other.end() && other.base.as_u64() < self.end()
    }
}

/// A physical memory map: a set of contiguous regions kept sorted by base
/// address, from which the hypervisor carves the regions it needs.
#[derive(Clone, Debug, Default)]
pub struct MemoryMap {
    regions: Vec<MemoryRegion>,
}

impl MemoryMap {
    /// An empty map.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }

    /// Build a map from `regions`, sorting them by base address.
    #[must_use]
    pub fn from_regions(mut regions: Vec<MemoryRegion>) -> Self {
        regions.sort_by_key(|r| r.base.as_u64());
        Self { regions }
    }

    /// Insert a region, keeping the map sorted by base address.
    pub fn add(&mut self, region: MemoryRegion) {
        let pos = self
            .regions
            .partition_point(|r| r.base.as_u64() <= region.base.as_u64());
        self.regions.insert(pos, region);
    }

    /// All regions, in ascending base-address order.
    #[must_use]
    pub fn regions(&self) -> &[MemoryRegion] {
        &self.regions
    }

    /// Iterator over the allocatable (usable RAM) regions.
    pub fn usable(&self) -> impl Iterator<Item = &MemoryRegion> {
        self.regions.iter().filter(|r| r.kind.is_allocatable())
    }

    /// Total free RAM available for carving.
    #[must_use]
    pub fn total_usable(&self) -> u64 {
        self.usable().map(|r| r.size).sum()
    }

    /// The largest single usable region, if any.
    #[must_use]
    pub fn largest_usable(&self) -> Option<&MemoryRegion> {
        self.usable().max_by_key(|r| r.size)
    }

    /// Carve `size` bytes of usable RAM aligned to `align`, retyping the carved
    /// span [`Reserved`](MemoryKind::Reserved) so it is never handed out twice,
    /// and return its base. First-fit in ascending address order; `None` if no
    /// usable region can satisfy it.
    ///
    /// # Panics
    /// Panics if `align` is not a power of two.
    pub fn carve(&mut self, size: u64, align: u64) -> Option<PhysAddr> {
        self.carve_below(size, align, u64::MAX)
    }

    /// Like [`carve`](Self::carve) but constrained so the carved span ends at or
    /// below `limit` — e.g. a 32-bit-DMA window under 4 GiB (`limit = 1 << 32`).
    ///
    /// # Panics
    /// Panics if `align` is not a power of two.
    pub fn carve_below(&mut self, size: u64, align: u64, limit: u64) -> Option<PhysAddr> {
        assert!(align.is_power_of_two(), "alignment must be a power of two");
        if size == 0 {
            return None;
        }
        for i in 0..self.regions.len() {
            let region = self.regions[i];
            if !region.kind.is_allocatable() {
                continue;
            }
            let start = region.base.align_up(align).as_u64();
            let Some(end) = start.checked_add(size) else {
                continue; // aligned start + size overflowed — try the next region
            };
            if end <= region.end() && end <= limit {
                self.split_reserve(i, start, size);
                return Some(PhysAddr::new(start));
            }
        }
        None
    }

    /// Split usable region `idx` so the span `[start, start+size)` becomes a
    /// [`Reserved`](MemoryKind::Reserved) region, preserving the usable remainder
    /// on either side. Keeps the map sorted and non-overlapping.
    fn split_reserve(&mut self, idx: usize, start: u64, size: u64) {
        let region = self.regions[idx];
        let end = start + size;
        let mut replacement = Vec::with_capacity(3);
        if start > region.base.as_u64() {
            replacement.push(MemoryRegion::new(
                region.base,
                start - region.base.as_u64(),
                region.kind,
            ));
        }
        replacement.push(MemoryRegion::new(
            PhysAddr::new(start),
            size,
            MemoryKind::Reserved,
        ));
        if end < region.end() {
            replacement.push(MemoryRegion::new(
                PhysAddr::new(end),
                region.end() - end,
                region.kind,
            ));
        }
        self.regions.splice(idx..=idx, replacement);
    }

    /// Debug/test invariant: regions are sorted by base and never overlap.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.regions
            .windows(2)
            .all(|w| w[0].end() <= w[1].base.as_u64())
    }

    /// Carve the hypervisor's boot-time physical regions from this map per `req`
    /// — a low DMA window (under 4 GiB), the hypervisor heap, and one disjoint
    /// RAM region per guest (each guest gets its own physical span; LOCKED
    /// PRINCIPLE 5 — isolation). Returns the assigned [`HypervisorRegions`], or
    /// `None` if the map cannot satisfy the whole request.
    ///
    /// All-or-nothing: the carves are tried on a scratch copy and committed only
    /// if every one succeeds, so a partial plan never mutates the map. The DMA
    /// window is carved first (it is the most constrained — it must fall under
    /// 4 GiB), then the heap, then each guest's RAM.
    ///
    /// # Panics
    /// Panics if `req.align` is not a power of two.
    #[must_use]
    pub fn plan_hypervisor_regions(
        &mut self,
        req: &MemoryPlanRequest,
    ) -> Option<HypervisorRegions> {
        let mut trial = self.clone();
        let dma_base = trial.carve_below(req.dma_size, req.align, 1u64 << 32)?;
        let heap_base = trial.carve(req.heap_size, req.align)?;
        let mut guest_ram = Vec::with_capacity(req.guest_ram_sizes.len());
        for &size in &req.guest_ram_sizes {
            let base = trial.carve(size, req.align)?;
            guest_ram.push(MemoryRegion::new(base, size, MemoryKind::Reserved));
        }
        // Every carve succeeded — commit the scratch map.
        *self = trial;
        Some(HypervisorRegions {
            dma: MemoryRegion::new(dma_base, req.dma_size, MemoryKind::Reserved),
            heap: MemoryRegion::new(heap_base, req.heap_size, MemoryKind::Reserved),
            guest_ram,
        })
    }
}

/// What [`MemoryMap::plan_hypervisor_regions`] should carve.
#[derive(Clone, Debug)]
pub struct MemoryPlanRequest {
    /// Hypervisor heap size in bytes.
    pub heap_size: u64,
    /// Low DMA-window size in bytes (carved under 4 GiB).
    pub dma_size: u64,
    /// Per-guest RAM sizes in bytes — one carved region each.
    pub guest_ram_sizes: Vec<u64>,
    /// Alignment applied to every carved region (a power of two, e.g. 2 MiB).
    pub align: u64,
}

/// The physical regions the hypervisor carved for itself at boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HypervisorRegions {
    /// Low DMA bounce-buffer window (under 4 GiB).
    pub dma: MemoryRegion,
    /// The hypervisor's own heap.
    pub heap: MemoryRegion,
    /// One disjoint RAM region per guest, in request order.
    pub guest_ram: Vec<MemoryRegion>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usable(base: u64, size: u64) -> MemoryRegion {
        MemoryRegion::new(PhysAddr::new(base), size, MemoryKind::Usable)
    }

    fn reserved(base: u64, size: u64) -> MemoryRegion {
        MemoryRegion::new(PhysAddr::new(base), size, MemoryKind::Reserved)
    }

    #[test]
    fn region_geometry() {
        let r = usable(0x1000, 0x1000);
        assert_eq!(r.end(), 0x2000);
        assert!(r.contains(0x1000) && r.contains(0x1FFF));
        assert!(!r.contains(0x2000) && !r.contains(0x0FFF));
        assert!(r.overlaps(&usable(0x1800, 0x1000)));
        assert!(
            !r.overlaps(&usable(0x2000, 0x1000)),
            "adjacent do not overlap"
        );
    }

    #[test]
    fn total_and_largest_usable_ignore_non_ram() {
        let map = MemoryMap::from_regions(vec![
            usable(0x0000, 0x1000),
            reserved(0x1000, 0x1000),
            usable(0x2000, 0x4000),
            MemoryRegion::new(PhysAddr::new(0x6000), 0x1000, MemoryKind::Mmio),
        ]);
        assert_eq!(map.total_usable(), 0x1000 + 0x4000);
        assert_eq!(map.largest_usable().unwrap().base, PhysAddr::new(0x2000));
    }

    #[test]
    fn carve_splits_a_usable_region_and_stays_consistent() {
        let mut map = MemoryMap::from_regions(vec![usable(0x1000, 0x1_0000)]);
        let before = map.total_usable();
        let base = map.carve(0x2000, 0x1000).expect("carve fits");
        assert_eq!(base, PhysAddr::new(0x1000), "first-fit at the region base");
        assert_eq!(
            map.total_usable(),
            before - 0x2000,
            "usable shrank by the carve"
        );
        assert!(map.is_consistent(), "map stays sorted and non-overlapping");
        // The carved span is now reserved, and the remainder is still usable.
        assert!(map.regions().iter().any(|r| r.base == PhysAddr::new(0x1000)
            && r.size == 0x2000
            && r.kind == MemoryKind::Reserved));
        assert!(
            map.regions()
                .iter()
                .any(|r| r.base == PhysAddr::new(0x3000) && r.kind == MemoryKind::Usable)
        );
    }

    #[test]
    fn carve_honors_alignment() {
        // A usable region whose base is not 64 KiB-aligned: the carve must round
        // the start up, leaving a usable sliver below it.
        let mut map = MemoryMap::from_regions(vec![usable(0x1000, 0x3_0000)]);
        let base = map.carve(0x1000, 0x1_0000).expect("aligned carve fits");
        assert_eq!(base, PhysAddr::new(0x1_0000), "rounded up to 64 KiB");
        assert!(base.is_aligned(0x1_0000));
        assert!(map.is_consistent());
        // The [0x1000, 0x10000) sliver stays usable.
        assert!(map.regions().iter().any(|r| r.base == PhysAddr::new(0x1000)
            && r.size == 0xF000
            && r.kind == MemoryKind::Usable));
    }

    #[test]
    fn carve_below_prefers_a_region_under_the_limit() {
        // One usable region above 4 GiB, one below: a DMA carve under 4 GiB must
        // land in the low one even though it comes second in scan order only if
        // sorted — here the low region is first, so also assert the high region
        // is rejected when it is the only option.
        let mut map = MemoryMap::from_regions(vec![
            usable(0x1000, 0x1000),           // too small for the request
            usable(0x1_0000_0000, 0x10_0000), // 4 GiB+, excluded by the limit
        ]);
        // 0x8000 bytes under 4 GiB cannot be satisfied: low region too small,
        // high region above the limit.
        assert!(map.carve_below(0x8000, 0x1000, 1 << 32).is_none());
        // Without the limit, the high region serves it.
        let base = map.carve(0x8000, 0x1000).expect("high region serves it");
        assert_eq!(base, PhysAddr::new(0x1_0000_0000));
    }

    #[test]
    fn carve_below_lands_in_the_low_region_when_it_fits() {
        let mut map = MemoryMap::from_regions(vec![
            usable(0x10_0000, 0x10_0000), // 1 MiB..2 MiB, under 4 GiB
            usable(0x1_0000_0000, 0x10_0000),
        ]);
        let base = map.carve_below(0x8000, 0x1000, 1 << 32).expect("low fits");
        assert_eq!(base, PhysAddr::new(0x10_0000));
        assert!(base.as_u64() + 0x8000 <= (1u64 << 32));
    }

    #[test]
    fn carve_fails_when_nothing_fits_and_zero_size_is_rejected() {
        let mut map = MemoryMap::from_regions(vec![usable(0x1000, 0x1000)]);
        assert!(
            map.carve(0x2000, 0x1000).is_none(),
            "request larger than any region"
        );
        assert!(
            map.carve(0, 0x1000).is_none(),
            "zero-size carve is rejected"
        );
        assert_eq!(
            map.total_usable(),
            0x1000,
            "failed carves leave the map untouched"
        );
    }

    #[test]
    fn plan_carves_disjoint_dma_heap_and_per_guest_regions() {
        // Big RAM above 4 GiB plus a small low region under it.
        let mut map = MemoryMap::from_regions(vec![
            usable(0x10_0000, 0x20_0000),       // 1 MiB..3 MiB, under 4 GiB
            usable(0x1_0000_0000, 0x1000_0000), // 4 GiB.., 256 MiB
        ]);
        let req = MemoryPlanRequest {
            heap_size: 0x40_0000, // 4 MiB
            dma_size: 0x10_0000,  // 1 MiB
            guest_ram_sizes: vec![0x100_0000, 0x80_0000],
            align: 0x20_0000, // 2 MiB
        };
        let plan = map.plan_hypervisor_regions(&req).expect("plan fits");

        // DMA is under 4 GiB; heap and guests are carved somewhere valid.
        assert!(plan.dma.end() <= (1u64 << 32), "DMA window under 4 GiB");
        assert_eq!(plan.dma.size, req.dma_size);
        assert_eq!(plan.heap.size, req.heap_size);
        assert_eq!(plan.guest_ram.len(), 2);

        // Every carved region is 2 MiB-aligned and mutually disjoint.
        let all = [plan.dma, plan.heap, plan.guest_ram[0], plan.guest_ram[1]];
        for r in &all {
            assert!(r.base.is_aligned(0x20_0000), "{r:?} aligned");
        }
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert!(!a.overlaps(b), "{a:?} and {b:?} must be disjoint");
            }
        }
        assert!(map.is_consistent(), "map stays consistent after planning");
    }

    #[test]
    fn plan_is_atomic_on_failure() {
        // Enough for DMA + heap but not the guest RAM: the whole plan must fail
        // and leave the map completely untouched.
        let mut map = MemoryMap::from_regions(vec![usable(0x20_0000, 0x60_0000)]); // 6 MiB
        let before = map.clone();
        let req = MemoryPlanRequest {
            heap_size: 0x40_0000,
            dma_size: 0x10_0000,
            guest_ram_sizes: vec![0x40_0000], // no room left for this
            align: 0x20_0000,
        };
        assert!(
            map.plan_hypervisor_regions(&req).is_none(),
            "plan cannot fit"
        );
        assert_eq!(
            map.regions(),
            before.regions(),
            "a failed plan leaves the map untouched"
        );
    }

    #[test]
    fn plan_dma_must_fit_under_four_gib() {
        // All RAM is above 4 GiB → the DMA window (and thus the plan) fails even
        // though there is plenty of RAM overall.
        let mut map = MemoryMap::from_regions(vec![usable(0x1_0000_0000, 0x1000_0000)]);
        let req = MemoryPlanRequest {
            heap_size: 0x20_0000,
            dma_size: 0x20_0000,
            guest_ram_sizes: vec![],
            align: 0x20_0000,
        };
        assert!(
            map.plan_hypervisor_regions(&req).is_none(),
            "DMA cannot be carved above 4 GiB"
        );
    }

    #[test]
    fn successive_carves_do_not_overlap() {
        let mut map = MemoryMap::from_regions(vec![usable(0x1000, 0x10_0000)]);
        let a = map.carve(0x1000, 0x1000).unwrap();
        let b = map.carve(0x1000, 0x1000).unwrap();
        assert_ne!(a, b);
        assert!(map.is_consistent());
        let ra = MemoryRegion::new(a, 0x1000, MemoryKind::Reserved);
        let rb = MemoryRegion::new(b, 0x1000, MemoryKind::Reserved);
        assert!(!ra.overlaps(&rb), "two carves never overlap");
    }
}

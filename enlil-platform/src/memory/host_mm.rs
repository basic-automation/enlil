//! Host virtual memory: a CR3-carrying address space with mmap-like semantics.
//!
//! [`paging`](super::paging) builds page tables; this module *owns* one.
//! [`HostAddressSpace`] carries the tables' memory image, the `CR3` value that
//! installs them, a bump pool of spare 4-KiB table frames for growing the
//! hierarchy, and a virtual-address manager with `mmap`/`munmap`/`mprotect`
//! semantics plus guard pages — the second half of roadmap item 1.3
//! ("host page tables (CR3), mmap-like semantics, guard pages").
//!
//! Two ways to get an address space:
//!
//! - [`HostAddressSpace::empty`] — a fresh, empty PML4 in a caller-provided
//!   table pool; every mapping is created with [`HostAddressSpace::mmap`] or
//!   [`HostAddressSpace::map_fixed`].
//! - [`HostAddressSpace::over_identity`] — adopt tables built by the
//!   [`paging`](super::paging) identity-map builders (e.g. the boot kernel's
//!   early map), then extend them with fine-grained mappings.
//!
//! Installing the tables is privileged: [`HostAddressSpace::install`] (and the
//! free [`load_cr3`], [`read_cr3`], [`HostAddressSpace::flush_page`],
//! [`HostAddressSpace::flush_all`]) execute `mov cr3` / `invlpg` and must only
//! run in ring 0. Everything else — mapping, unmapping, protection changes,
//! and the read-only [`HostAddressSpace::translate`] walk — is pure table
//! memory manipulation and is host-testable.
//!
//! Only the low canonical half (`[0, 2^48)`) is managed; the editor works at
//! 4-KiB granularity and refuses to descend through a 2 MiB / 1 GiB huge-page
//! leaf ([`MmError::HugePageOnPath`] — split the huge page first).

use super::{
    BitmapFrameAllocator, PhysAddr, PhysFrame, VirtAddr,
    paging::{
        PageTableLayout, flags, pd_index, pdpt_index, pml4_index, pt_index, pte_entry, table_entry,
    },
};

#[cfg(feature = "platform-baremetal")]
use alloc::vec::Vec;

/// 4-KiB page size.
const PAGE_SIZE: u64 = 4096;
/// 4-KiB page size as a `usize` (for [`Iterator::step_by` strides).
const PAGE_STEP: usize = 4096;
/// Bytes per page-table page.
const TABLE_BYTES: usize = 4096;
/// Physical-address bits `[51:12]` of a table pointer / 4-KiB frame.
const FRAME_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Physical-address bits `[51:21]` of a 2 MiB huge-page frame.
const HUGE_2MIB_ADDR_MASK: u64 = 0x000F_FFFF_FFE0_0000;
/// Physical-address bits `[51:30]` of a 1 GiB huge-page frame.
const HUGE_1GIB_ADDR_MASK: u64 = 0x000F_FFFF_C000_0000;
/// Byte offset within a 2 MiB huge page (bits `[20:0]`).
const HUGE_2MIB_OFFSET_MASK: u64 = 0x1F_FFFF;
/// Byte offset within a 1 GiB huge page (bits `[29:0]`).
const HUGE_1GIB_OFFSET_MASK: u64 = 0x3FFF_FFFF;
/// First non-canonical low-half address with 4-level paging (48-bit VAs).
const VA_LIMIT: u64 = 1 << 48;

/// mmap-style protection bits, translated to PTE flags by the mapper.
///
/// `READ` is implied by any mapping (a mapped page is always `PRESENT`); it
/// exists so callers can spell `prot::READ` explicitly.
pub mod prot {
    /// The pages may be read.
    pub const READ: u8 = 0b001;
    /// The pages may be written (sets `WRITABLE`).
    pub const WRITE: u8 = 0b010;
    /// The pages may be executed (clears `NO_EXECUTE`; requires `EFER.NXE`).
    pub const EXEC: u8 = 0b100;
}

/// Guard-page placement around an anonymous mapping.
///
/// Guard pages are *reserved* virtual pages left **not present**, so a touch
/// (e.g. a stack overflow running downhill) faults instead of silently
/// corrupting the neighbor. [`HostAddressSpace::translate`] proves a guard
/// absent without touching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GuardPages {
    /// Guard pages below the usable region.
    pub low: u64,
    /// Guard pages above the usable region.
    pub high: u64,
}

impl GuardPages {
    /// No guard pages.
    #[must_use]
    pub const fn none() -> Self {
        Self { low: 0, high: 0 }
    }

    /// One guard page below the usable region.
    #[must_use]
    pub const fn low() -> Self {
        Self { low: 1, high: 0 }
    }

    /// One guard page above the usable region.
    #[must_use]
    pub const fn high() -> Self {
        Self { low: 0, high: 1 }
    }

    /// One guard page on each side of the usable region.
    #[must_use]
    pub const fn both() -> Self {
        Self { low: 1, high: 1 }
    }

    /// Total guard pages (`low + high`).
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.low + self.high
    }
}

/// Options for [`HostAddressSpace::mmap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmapOptions {
    /// Protection bits ([`prot::READ`] / [`prot::WRITE`] / [`prot::EXEC`]).
    pub prot: u8,
    /// Guard pages around the mapping.
    pub guard: GuardPages,
    /// `Some(va)` places the mapping at exactly `va` (`MAP_FIXED`-like);
    /// `None` picks the first free virtual range that fits.
    pub va: Option<VirtAddr>,
    /// Extra PTE flag bits OR'd into every leaf (e.g. [`flags::NO_CACHE`] for
    /// an MMIO window). Address bits and `PRESENT`/`HUGE_PAGE` are managed by
    /// the mapper and must not be set here.
    pub extra_flags: u64,
}

impl MmapOptions {
    /// A plain anonymous mapping: first-fit VA, no guards, no extra flags.
    #[must_use]
    pub const fn new(prot: u8) -> Self {
        Self {
            prot,
            guard: GuardPages::none(),
            va: None,
            extra_flags: 0,
        }
    }
}

/// Why a host virtual-memory operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmError {
    /// The operation asked for zero pages.
    EmptyRegion,
    /// A virtual or physical address was not 4-KiB aligned.
    UnalignedAddress,
    /// A virtual address is not in the managed low canonical half
    /// (`va >= 2^48`).
    NonCanonicalAddress,
    /// Address arithmetic overflowed, or a table address fell outside the
    /// table image.
    AddressOverflow,
    /// The spare table-frame pool is exhausted — the hierarchy cannot grow.
    OutOfTableMemory,
    /// No free virtual-address gap fits the request.
    OutOfVirtualMemory,
    /// The [`FrameSource`] has no free physical frame left.
    OutOfPhysicalFrames,
    /// The range overlaps a live mapping (or a page is already mapped).
    OverlappingMapping,
    /// `munmap`/`mprotect`/`unmap_page` touched a page that is not mapped.
    PageNotMapped,
    /// A 4-KiB descent hit a 2 MiB or 1 GiB huge-page leaf — split the huge
    /// page before mapping beneath it.
    HugePageOnPath,
    /// The table-frame pool is not fully inside the table image.
    TablePoolOutOfImage,
}

/// A source of 4-KiB physical frames backing anonymous [`HostAddressSpace`]
/// mappings.
///
/// [`BitmapFrameAllocator`] implements this; a hypervisor can also back it
/// with the carved guest-RAM / heap region allocator.
pub trait FrameSource {
    /// Allocate one 4-KiB physical frame, or `None` when exhausted.
    fn alloc_frame(&mut self) -> Option<PhysFrame>;
    /// Return a frame previously handed out by [`alloc_frame`](Self::alloc_frame).
    fn free_frame(&mut self, frame: PhysFrame);
}

impl FrameSource for BitmapFrameAllocator {
    fn alloc_frame(&mut self) -> Option<PhysFrame> {
        self.allocate_frame()
    }

    fn free_frame(&mut self, frame: PhysFrame) {
        self.deallocate_frame(frame);
    }
}

/// Translate [`prot`] bits to PTE flag bits (always includes `PRESENT`; adds
/// `NO_EXECUTE` unless [`prot::EXEC`] is set).
const fn prot_flags(prot: u8) -> u64 {
    let mut f = flags::PRESENT;
    if prot & prot::WRITE != 0 {
        f |= flags::WRITABLE;
    }
    if prot & prot::EXEC == 0 {
        f |= flags::NO_EXECUTE;
    }
    f
}

/// A live mapping tracked by the virtual-address manager.
#[derive(Debug, Clone, Copy)]
struct Region {
    /// First usable virtual page.
    base: u64,
    /// Usable 4-KiB pages.
    pages: u64,
    /// Guard pages below `base` (reserved, never mapped).
    guard_low: u64,
    /// Guard pages above the usable pages (reserved, never mapped).
    guard_high: u64,
    /// Protection bits ([`prot`]).
    prot: u8,
    /// Whether the backing frames came from the [`FrameSource`] (`munmap`
    /// returns them) as opposed to [`HostAddressSpace::map_fixed`] (caller-owned).
    owned: bool,
}

impl Region {
    /// First reserved byte (start of the low guards).
    const fn reserved_base(&self) -> u64 {
        self.base - self.guard_low * PAGE_SIZE
    }

    /// Reserved 4-KiB pages (guards + usable).
    const fn reserved_pages(&self) -> u64 {
        self.guard_low + self.pages + self.guard_high
    }

    /// One past the last reserved byte.
    const fn reserved_end(&self) -> u64 {
        self.reserved_base() + self.reserved_pages() * PAGE_SIZE
    }
}

/// The public view of a tracked mapping, as returned by
/// [`HostAddressSpace::region_at`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionInfo {
    /// First usable virtual page.
    pub base: VirtAddr,
    /// Usable 4-KiB pages.
    pub pages: u64,
    /// Guard pages below [`base`](Self::base) (not present).
    pub guard_low: u64,
    /// Guard pages above the usable pages (not present).
    pub guard_high: u64,
    /// Protection bits ([`prot`]).
    pub prot: u8,
    /// Whether `munmap` returns the backing frames to the [`FrameSource`].
    pub frames_owned: bool,
}

impl From<Region> for RegionInfo {
    fn from(r: Region) -> Self {
        Self {
            base: VirtAddr::new(r.base),
            pages: r.pages,
            guard_low: r.guard_low,
            guard_high: r.guard_high,
            prot: r.prot,
            frames_owned: r.owned,
        }
    }
}

/// Load `cr3`, installing a new address space (and flushing every non-global
/// TLB entry).
///
/// This is the platform ISA seam for the CR3 install — `enlil-boot`'s page
/// tables go live through this function.
///
/// # Safety
///
/// `cr3` must be the physical address of a valid PML4 that maps everything the
/// CPU touches next (the running code, stack, and any data touched before the
/// next mapping change). A short or corrupt root faults on the next
/// instruction fetch.
pub unsafe fn load_cr3(cr3: u64) {
    // SAFETY: the caller guarantees `cr3` maps the running context.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
    }
}

/// Read the currently installed `CR3` (the live PML4's physical address).
///
/// This is safe to call — reading the register cannot violate memory safety —
/// but it faults outside ring 0, so it is only meaningful in the kernel.
#[must_use]
pub fn read_cr3() -> u64 {
    let cr3: u64;
    // SAFETY: `mov reg, cr3` has no side effects; it faults (not UB) if
    // executed without privilege.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, preserves_flags));
    }
    cr3
}

/// A host virtual address space: owned page tables plus mmap-like management.
///
/// The tables live in `image`, a byte slice indexed by *physical* address:
/// the table at physical `pa` is `image[pa - image_phys_base]`. `cr3` is the
/// PML4's physical address — the value [`install`](Self::install) loads.
///
/// The virtual-address manager tracks every live mapping in a sorted,
/// non-overlapping region list; `mmap` allocates virtual ranges first-fit and
/// `munmap` splits or drops the region records it overlaps.
#[derive(Debug)]
pub struct HostAddressSpace<'a> {
    /// Table memory image, indexed from `image_phys_base`.
    image: &'a mut [u8],
    /// Physical address of `image[0]`.
    image_phys_base: u64,
    /// Physical address of the PML4 — the CR3 value.
    cr3: u64,
    /// Byte offset (into `image`) of the table-frame pool's first page.
    table_pool_start: usize,
    /// Byte offset of the next free table page.
    table_cursor: usize,
    /// One past the last table-pool byte.
    table_limit: usize,
    /// First manageable virtual address (page-aligned).
    va_base: u64,
    /// One past the last manageable virtual byte.
    va_limit: u64,
    /// Live mappings, sorted by reserved base, non-overlapping.
    mappings: Vec<Region>,
}

/// A virtual address is manageable when it is 4-KiB aligned and in the low
/// canonical half.
const fn validate_va(va: u64) -> Result<(), MmError> {
    if va & (PAGE_SIZE - 1) != 0 {
        return Err(MmError::UnalignedAddress);
    }
    if va >= VA_LIMIT {
        return Err(MmError::NonCanonicalAddress);
    }
    Ok(())
}

// `elidable_lifetime_names` is a false positive here: eliding `'a` breaks the
// constructors below (the impl-header `'_` does not unify with the input
// `&mut [u8]` lifetime in `Result<Self, MmError>`).
#[allow(clippy::elidable_lifetime_names)]
impl<'a> HostAddressSpace<'a> {
    /// Build an empty address space: a zeroed PML4 at the start of the table
    /// pool, nothing mapped.
    ///
    /// - `image` / `image_phys_base`: the table memory image (see the type
    ///   docs); the table pool must lie fully inside it.
    /// - `table_pool_base` / `table_pool_pages`: the 4-KiB-aligned physical
    ///   range the hierarchy grows into (`table_pool_pages >= 1`; the first
    ///   page becomes the PML4, so [`cr3`](Self::cr3) == `table_pool_base`).
    /// - `va_base` / `va_len`: the manageable virtual range (page-aligned
    ///   base, inside the low canonical half).
    ///
    /// # Errors
    ///
    /// - [`MmError::EmptyRegion`] if `table_pool_pages == 0` or `va_len == 0`;
    /// - [`MmError::UnalignedAddress`] if the pool or VA base is not 4-KiB
    ///   aligned;
    /// - [`MmError::NonCanonicalAddress`] if the VA range leaves the low
    ///   canonical half;
    /// - [`MmError::AddressOverflow`] if the VA range overflows `u64`;
    /// - [`MmError::TablePoolOutOfImage`] if the pool is not inside `image`.
    pub fn empty(
        image: &'a mut [u8],
        image_phys_base: u64,
        table_pool_base: PhysAddr,
        table_pool_pages: u64,
        va_base: u64,
        va_len: u64,
    ) -> Result<Self, MmError> {
        if table_pool_pages == 0 || va_len == 0 {
            return Err(MmError::EmptyRegion);
        }
        if !table_pool_base.is_aligned(PAGE_SIZE) || va_base & (PAGE_SIZE - 1) != 0 {
            return Err(MmError::UnalignedAddress);
        }
        let va_limit = va_base
            .checked_add(va_len)
            .ok_or(MmError::AddressOverflow)?;
        if va_limit > VA_LIMIT {
            return Err(MmError::NonCanonicalAddress);
        }
        let mut space = Self {
            image,
            image_phys_base,
            cr3: table_pool_base.as_u64(),
            table_pool_start: 0,
            table_cursor: 0,
            table_limit: 0,
            va_base,
            va_limit,
            mappings: Vec::new(),
        };
        space.init_table_pool(table_pool_base, table_pool_pages, true)?;
        Ok(space)
    }

    /// Adopt page tables built by the [`paging`](super::paging) identity-map
    /// builders (e.g. [`build_identity_map_2mib`](super::paging::build_identity_map_2mib)).
    ///
    /// The builders index `mem` from physical 0, so `image` must be that same
    /// image and `layout.cr3` becomes this space's CR3. The adopted hierarchy
    /// cannot grow until [`set_table_pool`](Self::set_table_pool) provides
    /// spare table pages; mappings the builders already laid down are visible
    /// to [`translate`](Self::translate) but are *not* tracked as regions
    /// (they predate the VA manager — `munmap` will not free them).
    ///
    /// # Errors
    ///
    /// Same VA-range errors as [`empty`](Self::empty).
    pub fn over_identity(
        image: &'a mut [u8],
        layout: PageTableLayout,
        va_base: u64,
        va_len: u64,
    ) -> Result<Self, MmError> {
        if va_len == 0 {
            return Err(MmError::EmptyRegion);
        }
        if va_base & (PAGE_SIZE - 1) != 0 {
            return Err(MmError::UnalignedAddress);
        }
        let va_limit = va_base
            .checked_add(va_len)
            .ok_or(MmError::AddressOverflow)?;
        if va_limit > VA_LIMIT {
            return Err(MmError::NonCanonicalAddress);
        }
        Ok(Self {
            image,
            image_phys_base: 0,
            cr3: layout.cr3,
            table_pool_start: 0,
            table_cursor: 0,
            table_limit: 0,
            va_base,
            va_limit,
            mappings: Vec::new(),
        })
    }

    /// Provide (or replace) the spare table-frame pool the hierarchy grows
    /// into when [`mmap`](Self::mmap)/[`map_fixed`](Self::map_fixed) need new
    /// intermediate tables.
    ///
    /// The pool is a bump region: previously allocated tables are forgotten,
    /// so call this before mapping (or only with a pool that already holds the
    /// live tables, e.g. spare pages appended after an adopted builder map).
    ///
    /// # Errors
    ///
    /// - [`MmError::EmptyRegion`] if `pages == 0`;
    /// - [`MmError::UnalignedAddress`] if `base` is not 4-KiB aligned;
    /// - [`MmError::TablePoolOutOfImage`] if the pool is not inside the image.
    pub fn set_table_pool(&mut self, base: PhysAddr, pages: u64) -> Result<(), MmError> {
        self.init_table_pool(base, pages, false)
    }

    /// Shared pool initializer; `reserve_first` zeroes the first pool page as
    /// a fresh PML4 (the [`empty`](Self::empty) constructor).
    fn init_table_pool(
        &mut self,
        base: PhysAddr,
        pages: u64,
        reserve_first: bool,
    ) -> Result<(), MmError> {
        if pages == 0 {
            return Err(MmError::EmptyRegion);
        }
        if !base.is_aligned(PAGE_SIZE) {
            return Err(MmError::UnalignedAddress);
        }
        let start = usize::try_from(
            base.as_u64()
                .checked_sub(self.image_phys_base)
                .ok_or(MmError::TablePoolOutOfImage)?,
        )
        .map_err(|_| MmError::TablePoolOutOfImage)?;
        let len = usize::try_from(
            pages
                .checked_mul(PAGE_SIZE)
                .ok_or(MmError::AddressOverflow)?,
        )
        .map_err(|_| MmError::TablePoolOutOfImage)?;
        let end = start.checked_add(len).ok_or(MmError::TablePoolOutOfImage)?;
        if end > self.image.len() {
            return Err(MmError::TablePoolOutOfImage);
        }
        self.table_pool_start = start;
        self.table_cursor = start;
        self.table_limit = end;
        if reserve_first {
            // Fresh PML4: zero the page and advance past it.
            self.image[start..start + TABLE_BYTES].fill(0);
            self.table_cursor = start + TABLE_BYTES;
            self.cr3 = base.as_u64();
        }
        Ok(())
    }

    /// The CR3 value that installs these tables.
    #[must_use]
    pub const fn cr3(&self) -> u64 {
        self.cr3
    }

    /// Install this address space: `CR3 := self.cr3`.
    ///
    /// # Safety
    ///
    /// Same contract as [`load_cr3`]: the tables must map everything the CPU
    /// touches next, or the next instruction fetch faults.
    pub unsafe fn install(&self) {
        // SAFETY: the caller upholds the `load_cr3` contract for this space.
        unsafe { load_cr3(self.cr3) };
    }

    /// Invalidate the TLB entry for one page (`invlpg`).
    ///
    /// Call after editing the *live* tables (post-[`install`](Self::install)
    /// [`mmap`](Self::mmap)/[`munmap`](Self::munmap)/[`mprotect`](Self::mprotect))
    /// so the CPU drops the stale translation.
    ///
    /// # Safety
    ///
    /// Privileged: faults outside ring 0. No memory-safety contract beyond
    /// that — invalidating a TLB entry is always safe.
    pub unsafe fn flush_page(va: VirtAddr) {
        // SAFETY: `invlpg` only drops a cached translation.
        unsafe {
            core::arch::asm!("invlpg [{}]", in(reg) va.as_u64(), options(nostack, preserves_flags));
        }
    }

    /// Reload `CR3` with its current value, flushing every non-global TLB
    /// entry.
    ///
    /// # Safety
    ///
    /// Privileged: faults outside ring 0.
    pub unsafe fn flush_all() {
        // SAFETY: reloading the live root keeps the address space identical.
        unsafe { load_cr3(read_cr3()) };
    }

    /// Spare 4-KiB table pages still available for hierarchy growth.
    #[must_use]
    pub const fn table_pages_left(&self) -> usize {
        self.table_limit.saturating_sub(self.table_cursor) / TABLE_BYTES
    }

    /// Number of live tracked mappings.
    #[must_use]
    pub const fn region_count(&self) -> usize {
        self.mappings.len()
    }

    /// Byte offset in `image` of the table at physical `table_pa`.
    fn table_byte_offset(&self, table_pa: u64) -> Result<usize, MmError> {
        let off = usize::try_from(
            table_pa
                .checked_sub(self.image_phys_base)
                .ok_or(MmError::AddressOverflow)?,
        )
        .map_err(|_| MmError::AddressOverflow)?;
        off.checked_add(TABLE_BYTES)
            .filter(|end| *end <= self.image.len())
            .ok_or(MmError::AddressOverflow)?;
        Ok(off)
    }

    /// Read entry `index` of the table at physical `table_pa`.
    fn read_entry(&self, table_pa: u64, index: usize) -> Result<u64, MmError> {
        debug_assert!(index < 512);
        let off = self.table_byte_offset(table_pa)? + index * 8;
        let bytes: [u8; 8] = self
            .image
            .get(off..off + 8)
            .and_then(|s| s.try_into().ok())
            .ok_or(MmError::AddressOverflow)?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// Write entry `index` of the table at physical `table_pa`.
    fn write_entry(&mut self, table_pa: u64, index: usize, value: u64) -> Result<(), MmError> {
        debug_assert!(index < 512);
        let off = self.table_byte_offset(table_pa)? + index * 8;
        let slot = self
            .image
            .get_mut(off..off + 8)
            .ok_or(MmError::AddressOverflow)?;
        slot.copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Allocate a zeroed 4-KiB table frame from the pool, returning its
    /// physical address.
    fn alloc_table(&mut self) -> Result<u64, MmError> {
        let next = self
            .table_cursor
            .checked_add(TABLE_BYTES)
            .ok_or(MmError::OutOfTableMemory)?;
        if next > self.table_limit {
            return Err(MmError::OutOfTableMemory);
        }
        let off = self.table_cursor;
        self.table_cursor = next;
        self.image[off..off + TABLE_BYTES].fill(0);
        Ok(self.image_phys_base + off as u64)
    }

    /// Get (or allocate) the child table at `index` of `parent_pa`.
    fn child_table(&mut self, parent_pa: u64, index: usize) -> Result<u64, MmError> {
        let entry = self.read_entry(parent_pa, index)?;
        if entry & flags::PRESENT == 0 {
            let table = self.alloc_table()?;
            self.write_entry(
                parent_pa,
                index,
                table_entry(table, flags::PRESENT | flags::WRITABLE),
            )?;
            Ok(table)
        } else {
            if entry & flags::HUGE_PAGE != 0 {
                return Err(MmError::HugePageOnPath);
            }
            Ok(entry & FRAME_ADDR_MASK)
        }
    }

    /// Map one 4-KiB page `va -> pa` with `prot` protection bits.
    ///
    /// The intermediate tables are allocated from the pool on demand.
    ///
    /// # Errors
    ///
    /// - [`MmError::UnalignedAddress`] / [`MmError::NonCanonicalAddress`] for
    ///   a bad `va` or unaligned `pa`;
    /// - [`MmError::OverlappingMapping`] if the page is already mapped;
    /// - [`MmError::HugePageOnPath`] if a huge-page leaf blocks the descent;
    /// - [`MmError::OutOfTableMemory`] if the pool is exhausted.
    pub fn map_page(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        prot: u8,
        extra_flags: u64,
    ) -> Result<(), MmError> {
        let va = va.as_u64();
        validate_va(va)?;
        if !pa.is_aligned(PAGE_SIZE) {
            return Err(MmError::UnalignedAddress);
        }
        let pdpt = self.child_table(self.cr3, pml4_index(va))?;
        let pd = self.child_table(pdpt, pdpt_index(va))?;
        let pt = self.child_table(pd, pd_index(va))?;
        let index = pt_index(va);
        if self.read_entry(pt, index)? & flags::PRESENT != 0 {
            return Err(MmError::OverlappingMapping);
        }
        self.write_entry(
            pt,
            index,
            pte_entry(pa.as_u64(), prot_flags(prot) | extra_flags),
        )
    }

    /// Unmap one 4-KiB page, returning the frame that was mapped (if any).
    ///
    /// Never allocates: missing intermediate tables mean "not mapped".
    ///
    /// # Errors
    ///
    /// - [`MmError::UnalignedAddress`] / [`MmError::NonCanonicalAddress`] for
    ///   a bad `va`;
    /// - [`MmError::HugePageOnPath`] if a huge-page leaf blocks the descent
    ///   (use a huge-page-aware unmap instead of punching a 4-KiB hole).
    pub fn unmap_page(&mut self, va: VirtAddr) -> Result<Option<PhysFrame>, MmError> {
        let va = va.as_u64();
        validate_va(va)?;
        let pml4e = self.read_entry(self.cr3, pml4_index(va))?;
        if pml4e & flags::PRESENT == 0 {
            return Ok(None);
        }
        let pdpte = self.read_entry(pml4e & FRAME_ADDR_MASK, pdpt_index(va))?;
        if pdpte & flags::PRESENT == 0 {
            return Ok(None);
        }
        if pdpte & flags::HUGE_PAGE != 0 {
            return Err(MmError::HugePageOnPath);
        }
        let pde = self.read_entry(pdpte & FRAME_ADDR_MASK, pd_index(va))?;
        if pde & flags::PRESENT == 0 {
            return Ok(None);
        }
        if pde & flags::HUGE_PAGE != 0 {
            return Err(MmError::HugePageOnPath);
        }
        let pt = pde & FRAME_ADDR_MASK;
        let index = pt_index(va);
        let pte = self.read_entry(pt, index)?;
        if pte & flags::PRESENT == 0 {
            return Ok(None);
        }
        self.write_entry(pt, index, 0)?;
        Ok(Some(PhysFrame::containing_address(PhysAddr::new(
            pte & FRAME_ADDR_MASK,
        ))))
    }
}

// `elidable_lifetime_names` is a false positive here: eliding `'a` breaks the
// constructors below (the impl-header `'_` does not unify with the input
// `&mut [u8]` lifetime in `Result<Self, MmError>`).
#[allow(clippy::elidable_lifetime_names)]
impl<'a> HostAddressSpace<'a> {
    /// First-fit free virtual range of `pages` pages.
    fn find_free_va(&self, pages: u64) -> Result<u64, MmError> {
        let len = pages
            .checked_mul(PAGE_SIZE)
            .ok_or(MmError::AddressOverflow)?;
        let mut cursor = self.va_base;
        for r in &self.mappings {
            let base = r.reserved_base();
            if base > cursor && base - cursor >= len {
                return Ok(cursor);
            }
            let end = r.reserved_end();
            if end > cursor {
                cursor = end;
            }
        }
        if self.va_limit >= cursor && self.va_limit - cursor >= len {
            Ok(cursor)
        } else {
            Err(MmError::OutOfVirtualMemory)
        }
    }

    /// Reject `[base, end)` if it overlaps any tracked region's reserved
    /// extent (usable pages *or* guard pages).
    fn check_no_overlap(&self, base: u64, end: u64) -> Result<(), MmError> {
        for r in &self.mappings {
            if base < r.reserved_end() && r.reserved_base() < end {
                return Err(MmError::OverlappingMapping);
            }
        }
        Ok(())
    }

    /// Index of the region whose *usable* pages contain `va`, if any.
    fn region_index_of_usable(&self, va: u64) -> Option<usize> {
        self.mappings.iter().position(|r| {
            let end = r.base + r.pages * PAGE_SIZE;
            va >= r.base && va < end
        })
    }

    /// The PTE for `va`, or `None` when no 4-KiB leaf is present.
    ///
    /// # Errors
    ///
    /// [`MmError::HugePageOnPath`] if a 1 GiB / 2 MiB huge leaf covers `va`.
    fn leaf_entry(&self, va: u64) -> Result<Option<u64>, MmError> {
        let pml4e = self.read_entry(self.cr3, pml4_index(va))?;
        if pml4e & flags::PRESENT == 0 {
            return Ok(None);
        }
        let pdpte = self.read_entry(pml4e & FRAME_ADDR_MASK, pdpt_index(va))?;
        if pdpte & flags::PRESENT == 0 {
            return Ok(None);
        }
        if pdpte & flags::HUGE_PAGE != 0 {
            return Err(MmError::HugePageOnPath);
        }
        let pde = self.read_entry(pdpte & FRAME_ADDR_MASK, pd_index(va))?;
        if pde & flags::PRESENT == 0 {
            return Ok(None);
        }
        if pde & flags::HUGE_PAGE != 0 {
            return Err(MmError::HugePageOnPath);
        }
        let pte = self.read_entry(pde & FRAME_ADDR_MASK, pt_index(va))?;
        Ok(if pte & flags::PRESENT != 0 {
            Some(pte)
        } else {
            None
        })
    }

    /// The page table (physical address) holding `va`'s PTE, without
    /// allocating.
    fn pt_of(&self, va: u64) -> Result<u64, MmError> {
        let pml4e = self.read_entry(self.cr3, pml4_index(va))?;
        if pml4e & flags::PRESENT == 0 {
            return Err(MmError::PageNotMapped);
        }
        let pdpte = self.read_entry(pml4e & FRAME_ADDR_MASK, pdpt_index(va))?;
        if pdpte & flags::PRESENT == 0 || pdpte & flags::HUGE_PAGE != 0 {
            return Err(MmError::PageNotMapped);
        }
        let pde = self.read_entry(pdpte & FRAME_ADDR_MASK, pd_index(va))?;
        if pde & flags::PRESENT == 0 || pde & flags::HUGE_PAGE != 0 {
            return Err(MmError::PageNotMapped);
        }
        Ok(pde & FRAME_ADDR_MASK)
    }

    /// Rewrite one mapped page's protection bits, preserving its frame and
    /// all non-protection flag bits (`NO_CACHE`, `GLOBAL`, ...).
    fn reprotect_page(&mut self, va: u64, prot: u8) -> Result<(), MmError> {
        let pt = self.pt_of(va)?;
        let index = pt_index(va);
        let old = self.read_entry(pt, index)?;
        if old & flags::PRESENT == 0 {
            return Err(MmError::PageNotMapped);
        }
        let new = (old & !(flags::WRITABLE | flags::NO_EXECUTE)) | prot_flags(prot);
        self.write_entry(pt, index, new)
    }

    /// Map `pages` fresh physical frames from `frames` at a virtual address.
    ///
    /// With `opts.va == None` the address is picked first-fit; with
    /// `Some(va)` the mapping is placed exactly there (`MAP_FIXED`-like).
    /// `opts.guard` reserves unmapped guard pages around the usable range.
    /// Returns the first *usable* virtual page (after the low guards).
    ///
    /// On failure nothing is left half-mapped: pages mapped before the error
    /// are unmapped again and their frames returned to `frames`.
    ///
    /// After editing the *live* tables, call [`flush_page`](Self::flush_page)
    /// per page (or [`flush_all`](Self::flush_all)).
    ///
    /// # Errors
    ///
    /// - [`MmError::EmptyRegion`] if `pages == 0`;
    /// - [`MmError::UnalignedAddress`] / [`MmError::NonCanonicalAddress`] for
    ///   a bad fixed `va`;
    /// - [`MmError::OverlappingMapping`] if the fixed range overlaps a live
    ///   mapping;
    /// - [`MmError::OutOfVirtualMemory`] if no VA gap fits (or the fixed
    ///   range leaves the manageable range);
    /// - [`MmError::OutOfPhysicalFrames`] if `frames` is exhausted;
    /// - [`MmError::OutOfTableMemory`] if the table pool is exhausted;
    /// - [`MmError::AddressOverflow`] if the range arithmetic overflows.
    pub fn mmap<F: FrameSource>(
        &mut self,
        frames: &mut F,
        pages: u64,
        opts: MmapOptions,
    ) -> Result<VirtAddr, MmError> {
        if pages == 0 {
            return Err(MmError::EmptyRegion);
        }
        let reserved = opts
            .guard
            .count()
            .checked_add(pages)
            .ok_or(MmError::AddressOverflow)?;
        let reserved_bytes = reserved
            .checked_mul(PAGE_SIZE)
            .ok_or(MmError::AddressOverflow)?;
        let base = match opts.va {
            Some(va) => {
                let b = va.as_u64();
                validate_va(b)?;
                let end = b
                    .checked_add(reserved_bytes)
                    .ok_or(MmError::AddressOverflow)?;
                if end > self.va_limit {
                    return Err(MmError::OutOfVirtualMemory);
                }
                self.check_no_overlap(b, end)?;
                b
            }
            None => self.find_free_va(reserved)?,
        };
        let usable = base + opts.guard.low * PAGE_SIZE;

        // Allocate the frames before mapping anything, so a frame shortage
        // rolls back trivially.
        let mut phys = Vec::new();
        for _ in 0..pages {
            if let Some(frame) = frames.alloc_frame() {
                phys.push(frame);
            } else {
                for frame in phys {
                    frames.free_frame(frame);
                }
                return Err(MmError::OutOfPhysicalFrames);
            }
        }

        // Map; on table-pool exhaustion unmap what succeeded and return the
        // frames.
        let mut mapped = 0usize;
        let mut failed = None;
        for (i, frame) in phys.iter().enumerate() {
            let va = usable + (i as u64) * PAGE_SIZE;
            if let Err(e) = self.map_page(
                VirtAddr::new(va),
                frame.start_address(),
                opts.prot,
                opts.extra_flags,
            ) {
                failed = Some(e);
                break;
            }
            mapped = i + 1;
        }
        if let Some(e) = failed {
            for (i, frame) in phys.iter().enumerate().take(mapped) {
                let va = VirtAddr::new(usable + (i as u64) * PAGE_SIZE);
                let _ = self.unmap_page(va);
                frames.free_frame(*frame);
            }
            for frame in phys.iter().skip(mapped) {
                frames.free_frame(*frame);
            }
            return Err(e);
        }

        let region = Region {
            base: usable,
            pages,
            guard_low: opts.guard.low,
            guard_high: opts.guard.high,
            prot: opts.prot,
            owned: true,
        };
        let pos = self
            .mappings
            .iter()
            .position(|r| r.reserved_base() > region.reserved_base())
            .unwrap_or(self.mappings.len());
        self.mappings.insert(pos, region);
        self.coalesce();
        Ok(VirtAddr::new(usable))
    }

    /// Map an explicit physical range `[pa, pa + pages*4K)` at virtual
    /// `[va, va + pages*4K)` — the primitive for MMIO windows and other
    /// caller-owned memory (`extra_flags` typically [`flags::NO_CACHE`]).
    ///
    /// Unlike [`mmap`](Self::mmap) no frames are allocated and `munmap` will
    /// not free any: the physical memory stays the caller's.
    ///
    /// # Errors
    ///
    /// - [`MmError::EmptyRegion`] if `pages == 0`;
    /// - [`MmError::UnalignedAddress`] / [`MmError::NonCanonicalAddress`] for
    ///   bad addresses;
    /// - [`MmError::OverlappingMapping`] if any page is already mapped (a
    ///   huge-page leaf covering the range counts as overlapping);
    /// - [`MmError::OutOfVirtualMemory`] if the range leaves the manageable
    ///   range;
    /// - [`MmError::OutOfTableMemory`] if the table pool is exhausted;
    /// - [`MmError::AddressOverflow`] if the range arithmetic overflows.
    pub fn map_fixed(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        pages: u64,
        prot: u8,
        extra_flags: u64,
    ) -> Result<(), MmError> {
        if pages == 0 {
            return Err(MmError::EmptyRegion);
        }
        let va = va.as_u64();
        validate_va(va)?;
        if !pa.is_aligned(PAGE_SIZE) {
            return Err(MmError::UnalignedAddress);
        }
        let len = pages
            .checked_mul(PAGE_SIZE)
            .ok_or(MmError::AddressOverflow)?;
        let end = va.checked_add(len).ok_or(MmError::AddressOverflow)?;
        if end > self.va_limit {
            return Err(MmError::OutOfVirtualMemory);
        }
        self.check_no_overlap(va, end)?;
        // Pre-walk: reject before mutating anything if any leaf is live (or a
        // huge page covers the range).
        for v in (va..end).step_by(PAGE_STEP) {
            match self.leaf_entry(v) {
                Ok(None) => {}
                Ok(Some(_)) | Err(MmError::HugePageOnPath) => {
                    return Err(MmError::OverlappingMapping);
                }
                Err(e) => return Err(e),
            }
        }
        for (i, v) in (va..end).step_by(PAGE_STEP).enumerate() {
            let p = pa.as_u64() + (i as u64) * PAGE_SIZE;
            self.map_page(VirtAddr::new(v), PhysAddr::new(p), prot, extra_flags)?;
        }
        let region = Region {
            base: va,
            pages,
            guard_low: 0,
            guard_high: 0,
            prot,
            owned: false,
        };
        let pos = self
            .mappings
            .iter()
            .position(|r| r.reserved_base() > region.reserved_base())
            .unwrap_or(self.mappings.len());
        self.mappings.insert(pos, region);
        self.coalesce();
        Ok(())
    }

    /// Merge adjacent regions with identical protection and ownership when no
    /// guard page sits between them (keeps the region list tight after
    /// `mmap`/`mprotect` cover neighboring ranges).
    fn coalesce(&mut self) {
        let mut merged: Vec<Region> = Vec::new();
        for r in core::mem::take(&mut self.mappings) {
            match merged.last_mut() {
                Some(last)
                    if last.base + last.pages * PAGE_SIZE == r.base
                        && last.prot == r.prot
                        && last.owned == r.owned
                        && last.guard_high == 0
                        && r.guard_low == 0 =>
                {
                    last.pages += r.pages;
                    last.guard_high = r.guard_high;
                }
                _ => merged.push(r),
            }
        }
        self.mappings = merged;
    }

    /// Run `edit` over every usable page of every region overlapping
    /// `[start, end)`, then rebuild the region list.
    ///
    /// The overlapped middle is dropped when `remove_middle` is set, otherwise
    /// kept with `middle_prot`. Surviving edge pieces keep their outer guard
    /// pages; no new guards are created at split points.
    ///
    /// The region list is taken out during the edit so `edit` can freely
    /// borrow the address space; callers must have validated the range first
    /// (an `edit` failure leaves the space with the pages edited so far —
    /// unreachable for the validated `munmap`/`mprotect` paths).
    fn reshape<F>(
        &mut self,
        start: u64,
        end: u64,
        remove_middle: bool,
        middle_prot: Option<u8>,
        mut edit: F,
    ) -> Result<(), MmError>
    where
        F: FnMut(&mut Self, u64, &Region) -> Result<(), MmError>,
    {
        let regions = core::mem::take(&mut self.mappings);
        let mut kept = Vec::new();
        for r in regions {
            let rs = r.base;
            let re = r.base + r.pages * PAGE_SIZE;
            let os = start.max(rs);
            let oe = end.min(re);
            if os >= oe {
                kept.push(r);
                continue;
            }
            for v in (os..oe).step_by(PAGE_STEP) {
                edit(self, v, &r)?;
            }
            if os > rs {
                kept.push(Region {
                    base: rs,
                    pages: (os - rs) / PAGE_SIZE,
                    guard_low: r.guard_low,
                    guard_high: 0,
                    prot: r.prot,
                    owned: r.owned,
                });
            }
            if !remove_middle {
                kept.push(Region {
                    base: os,
                    pages: (oe - os) / PAGE_SIZE,
                    guard_low: 0,
                    guard_high: 0,
                    prot: middle_prot.unwrap_or(r.prot),
                    owned: r.owned,
                });
            }
            if oe < re {
                kept.push(Region {
                    base: oe,
                    pages: (re - oe) / PAGE_SIZE,
                    guard_low: 0,
                    guard_high: r.guard_high,
                    prot: r.prot,
                    owned: r.owned,
                });
            }
        }
        self.mappings = kept;
        self.coalesce();
        Ok(())
    }

    /// Validate an edit range for [`munmap`](Self::munmap)/[`mprotect`](Self::mprotect).
    ///
    /// Every page of `[va, va + pages*4K)` must be a *usable* mapped page of
    /// some region — guard pages are never mapped and cannot be edited.
    fn validate_edit_range(&self, va: VirtAddr, pages: u64) -> Result<(u64, u64), MmError> {
        if pages == 0 {
            return Err(MmError::EmptyRegion);
        }
        let start = va.as_u64();
        validate_va(start)?;
        let len = pages
            .checked_mul(PAGE_SIZE)
            .ok_or(MmError::AddressOverflow)?;
        let end = start.checked_add(len).ok_or(MmError::AddressOverflow)?;
        for v in (start..end).step_by(PAGE_STEP) {
            if self.region_index_of_usable(v).is_none() {
                return Err(MmError::PageNotMapped);
            }
        }
        Ok((start, end))
    }

    /// Unmap `[va, va + pages*4K)`.
    ///
    /// Backing frames of anonymous ([`mmap`](Self::mmap)) regions are returned
    /// to `frames`; [`map_fixed`](Self::map_fixed) regions just lose their
    /// mapping. Overlapping region records are split; a fully covered region
    /// is dropped. Guard pages need no handling — they were never mapped.
    ///
    /// After editing the *live* tables, flush the range (see
    /// [`flush_page`](Self::flush_page)).
    ///
    /// # Errors
    ///
    /// - [`MmError::EmptyRegion`] if `pages == 0`;
    /// - [`MmError::UnalignedAddress`] / [`MmError::NonCanonicalAddress`] /
    ///   [`MmError::AddressOverflow`] for a bad range;
    /// - [`MmError::PageNotMapped`] if any page is not a usable mapped page
    ///   (including guard pages).
    pub fn munmap<F: FrameSource>(
        &mut self,
        frames: &mut F,
        va: VirtAddr,
        pages: u64,
    ) -> Result<(), MmError> {
        let (start, end) = self.validate_edit_range(va, pages)?;
        self.reshape(start, end, true, None, |space, v, region| {
            let frame = space
                .unmap_page(VirtAddr::new(v))?
                // Unreachable: the range was validated against live regions.
                .ok_or(MmError::PageNotMapped)?;
            if region.owned {
                frames.free_frame(frame);
            }
            Ok(())
        })
    }

    /// Change the protection of `[va, va + pages*4K)` to `prot`.
    ///
    /// Only the `WRITABLE` / `NO_EXECUTE` leaf bits change; the frame and all
    /// other flag bits (`NO_CACHE`, `GLOBAL`, ...) are preserved. Partially
    /// covered regions are split so every record keeps truthful `prot`.
    ///
    /// # Errors
    ///
    /// Same as [`munmap`](Self::munmap), plus [`MmError::HugePageOnPath`] if a
    /// huge-page leaf covers the range (only 4-KiB leaves are reprotected).
    pub fn mprotect(&mut self, va: VirtAddr, pages: u64, prot: u8) -> Result<(), MmError> {
        let (start, end) = self.validate_edit_range(va, pages)?;
        self.reshape(start, end, false, Some(prot), |space, v, _| {
            space.reprotect_page(v, prot)
        })
    }

    /// Resolve `va` through the tables to its physical address (with the page
    /// offset), or `None` if it is not mapped.
    ///
    /// This walks the same tables the CPU walks — including 2 MiB and 1 GiB
    /// huge-page leaves — so it proves a guard page absent *without touching
    /// it* (touching it is exactly what must fault).
    #[must_use]
    pub fn translate(&self, va: VirtAddr) -> Option<PhysAddr> {
        let va = va.as_u64();
        if va >= VA_LIMIT {
            return None;
        }
        let pml4e = self.read_entry(self.cr3, pml4_index(va)).ok()?;
        if pml4e & flags::PRESENT == 0 {
            return None;
        }
        let pdpte = self
            .read_entry(pml4e & FRAME_ADDR_MASK, pdpt_index(va))
            .ok()?;
        if pdpte & flags::PRESENT == 0 {
            return None;
        }
        if pdpte & flags::HUGE_PAGE != 0 {
            return Some(PhysAddr::new(
                (pdpte & HUGE_1GIB_ADDR_MASK) | (va & HUGE_1GIB_OFFSET_MASK),
            ));
        }
        let pde = self
            .read_entry(pdpte & FRAME_ADDR_MASK, pd_index(va))
            .ok()?;
        if pde & flags::PRESENT == 0 {
            return None;
        }
        if pde & flags::HUGE_PAGE != 0 {
            return Some(PhysAddr::new(
                (pde & HUGE_2MIB_ADDR_MASK) | (va & HUGE_2MIB_OFFSET_MASK),
            ));
        }
        let pte = self.read_entry(pde & FRAME_ADDR_MASK, pt_index(va)).ok()?;
        if pte & flags::PRESENT == 0 {
            return None;
        }
        Some(PhysAddr::new(
            (pte & FRAME_ADDR_MASK) | (va & (PAGE_SIZE - 1)),
        ))
    }

    /// The tracked region whose *reserved* extent (usable pages plus guards)
    /// contains `va`, if any.
    #[must_use]
    pub fn region_at(&self, va: VirtAddr) -> Option<RegionInfo> {
        let va = va.as_u64();
        self.mappings
            .iter()
            .find(|r| va >= r.reserved_base() && va < r.reserved_end())
            .copied()
            .map(RegionInfo::from)
    }

    /// Whether `va` is a mapped, usable page.
    #[must_use]
    pub fn is_mapped(&self, va: VirtAddr) -> bool {
        self.region_index_of_usable(va.as_u64()).is_some() && self.translate(va).is_some()
    }

    /// Whether `va` is a guard page of some region (reserved, never mapped).
    #[must_use]
    pub fn is_guard(&self, va: VirtAddr) -> bool {
        let va = va.as_u64();
        self.mappings.iter().any(|r| {
            let rb = r.reserved_base();
            let re = r.reserved_end();
            va >= rb && va < re && (va < r.base || va >= r.base + r.pages * PAGE_SIZE)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::paging::{build_identity_map_2mib, flags as pg_flags};
    use super::*;

    /// A zeroed `'static` image of `bytes` bytes.
    ///
    /// Heap-allocated on purpose: a stack array this large trips
    /// `clippy::large_stack_arrays`.
    fn zeroed_image(bytes: usize) -> &'static mut [u8] {
        Box::leak(vec![0u8; bytes].into_boxed_slice())
    }

    /// A 16 MiB image with a 64-page table pool at offset 0 and a 64 MiB
    /// frame pool at 8 MiB — the standard test rig.
    struct Rig {
        space: HostAddressSpace<'static>,
        frames: BitmapFrameAllocator,
    }

    impl Rig {
        fn new(va_base: u64, va_len: u64) -> Self {
            let image = zeroed_image(16 * 1024 * 1024);
            let space =
                HostAddressSpace::empty(image, 0, PhysAddr::new(0), 64, va_base, va_len).unwrap();
            // Sanity: the pool is consumed from page 1 (page 0 is the PML4).
            assert_eq!(space.table_pages_left(), 63);
            let frames = BitmapFrameAllocator::new(PhysAddr::new(8 * 1024 * 1024), 4096 * 4096);
            Self { space, frames }
        }
    }

    #[test]
    fn empty_space_carries_cr3_and_empty_pool() {
        let rig = Rig::new(0x1_0000_0000, 0x1_0000_0000);
        assert_eq!(rig.space.cr3(), 0);
        assert_eq!(rig.space.region_count(), 0);
        assert_eq!(rig.space.table_pages_left(), 63);
    }

    #[test]
    fn empty_rejects_bad_inputs() {
        let try_empty = |pool_base: u64, pool_pages: u64, va_base: u64, va_len: u64| {
            let image: &mut [u8] = zeroed_image(65536);
            HostAddressSpace::empty(
                image,
                0,
                PhysAddr::new(pool_base),
                pool_pages,
                va_base,
                va_len,
            )
            .map(|_| ())
        };
        assert_eq!(try_empty(0, 0, 0x1000, 0x1000), Err(MmError::EmptyRegion));
        assert_eq!(
            try_empty(1, 4, 0x1000, 0x1000),
            Err(MmError::UnalignedAddress)
        );
        // Pool past the end of the image.
        assert_eq!(
            try_empty(0x10000, 4, 0x1000, 0x1000),
            Err(MmError::TablePoolOutOfImage)
        );
        // VA range leaving the low canonical half.
        assert_eq!(
            try_empty(0, 4, 1 << 48, 0x1000),
            Err(MmError::NonCanonicalAddress)
        );
        assert_eq!(try_empty(0, 4, 0x1000, 0), Err(MmError::EmptyRegion));
    }

    #[test]
    fn mmap_maps_frames_with_prot_flags() {
        let mut rig = Rig::new(0x1_0000_0000, 0x1_0000_0000);
        let va = rig
            .space
            .mmap(
                &mut rig.frames,
                3,
                MmapOptions::new(prot::READ | prot::WRITE),
            )
            .unwrap();
        assert_eq!(va, VirtAddr::new(0x1_0000_0000));
        assert_eq!(rig.space.region_count(), 1);

        // Frames are contiguous out of the fresh bitmap allocator.
        let p0 = rig.space.translate(va).unwrap();
        for i in 1..3u64 {
            let pi = rig
                .space
                .translate(VirtAddr::new(va.as_u64() + i * 4096))
                .unwrap();
            assert_eq!(pi.as_u64(), p0.as_u64() + i * 4096);
        }
        // PTE flags: PRESENT | WRITABLE | NO_EXECUTE (no EXEC requested).
        let pt = rig.space.pt_of(va.as_u64()).unwrap();
        let pte = rig.space.read_entry(pt, pt_index(va.as_u64())).unwrap();
        assert_eq!(
            pte & (pg_flags::PRESENT | pg_flags::WRITABLE | pg_flags::NO_EXECUTE),
            pg_flags::PRESENT | pg_flags::WRITABLE | pg_flags::NO_EXECUTE
        );
        assert!(rig.space.is_mapped(va));
    }

    #[test]
    fn mmap_exec_clears_no_execute() {
        let mut rig = Rig::new(0x2_0000_0000, 0x1_0000_0000);
        let va = rig
            .space
            .mmap(
                &mut rig.frames,
                1,
                MmapOptions::new(prot::READ | prot::EXEC),
            )
            .unwrap();
        let pt = rig.space.pt_of(va.as_u64()).unwrap();
        let pte = rig.space.read_entry(pt, pt_index(va.as_u64())).unwrap();
        assert_eq!(pte & pg_flags::NO_EXECUTE, 0);
        assert_eq!(pte & pg_flags::WRITABLE, 0); // read-only executable
    }

    #[test]
    fn mmap_first_fit_reuses_freed_va() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let a = rig
            .space
            .mmap(&mut rig.frames, 2, MmapOptions::new(prot::READ))
            .unwrap();
        let b = rig
            .space
            .mmap(&mut rig.frames, 2, MmapOptions::new(prot::READ))
            .unwrap();
        assert_eq!(b.as_u64(), a.as_u64() + 2 * 4096);
        rig.space.munmap(&mut rig.frames, a, 2).unwrap();
        // The freed gap is reused first-fit.
        let c = rig
            .space
            .mmap(&mut rig.frames, 1, MmapOptions::new(prot::READ))
            .unwrap();
        assert_eq!(c, a);
    }

    #[test]
    fn mmap_fixed_va_option_places_exactly() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let want = VirtAddr::new(0x1_0000_0000 + 0x5000);
        let got = rig
            .space
            .mmap(
                &mut rig.frames,
                1,
                MmapOptions {
                    va: Some(want),
                    ..MmapOptions::new(prot::READ | prot::WRITE)
                },
            )
            .unwrap();
        assert_eq!(got, want);
        // Overlapping the same VA is rejected.
        assert_eq!(
            rig.space.mmap(
                &mut rig.frames,
                1,
                MmapOptions {
                    va: Some(want),
                    ..MmapOptions::new(prot::READ)
                },
            ),
            Err(MmError::OverlappingMapping)
        );
    }

    #[test]
    fn guard_pages_are_reserved_but_not_mapped() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let va = rig
            .space
            .mmap(
                &mut rig.frames,
                2,
                MmapOptions {
                    guard: GuardPages::both(),
                    ..MmapOptions::new(prot::READ | prot::WRITE)
                },
            )
            .unwrap();
        // Low guard sits below the usable base; the usable base is bumped past it.
        assert_eq!(va.as_u64(), 0x1_0000_0000 + 4096);
        let low_guard = VirtAddr::new(va.as_u64() - 4096);
        let high_guard = VirtAddr::new(va.as_u64() + 2 * 4096);
        assert_eq!(rig.space.translate(low_guard), None);
        assert_eq!(rig.space.translate(high_guard), None);
        assert!(rig.space.is_guard(low_guard));
        assert!(rig.space.is_guard(high_guard));
        assert!(!rig.space.is_guard(va));
        assert!(rig.space.is_mapped(va));
        assert!(rig.space.is_mapped(VirtAddr::new(va.as_u64() + 4096)));
        // The region record carries the guards.
        let info = rig.space.region_at(low_guard).unwrap();
        assert_eq!(info.base, va);
        assert_eq!(info.pages, 2);
        assert_eq!((info.guard_low, info.guard_high), (1, 1));
        // munmap of the usable range drops the whole reservation, guards included.
        rig.space.munmap(&mut rig.frames, va, 2).unwrap();
        assert_eq!(rig.space.region_count(), 0);
        assert_eq!(rig.space.region_at(low_guard), None);
        // ...and the VA (including the guard slots) is reusable.
        let va2 = rig
            .space
            .mmap(&mut rig.frames, 4, MmapOptions::new(prot::READ))
            .unwrap();
        assert_eq!(va2.as_u64(), 0x1_0000_0000);
    }

    #[test]
    fn munmap_returns_frames_to_allocator() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let free_before = rig.frames.free_frames();
        let va = rig
            .space
            .mmap(
                &mut rig.frames,
                4,
                MmapOptions::new(prot::READ | prot::WRITE),
            )
            .unwrap();
        assert_eq!(rig.frames.free_frames(), free_before - 4);
        rig.space.munmap(&mut rig.frames, va, 4).unwrap();
        assert_eq!(rig.frames.free_frames(), free_before);
        assert_eq!(rig.space.translate(va), None);
    }

    #[test]
    fn munmap_partial_splits_region() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let va = rig
            .space
            .mmap(&mut rig.frames, 4, MmapOptions::new(prot::READ))
            .unwrap();
        // Unmap the middle two pages.
        rig.space
            .munmap(&mut rig.frames, VirtAddr::new(va.as_u64() + 4096), 2)
            .unwrap();
        assert_eq!(rig.space.region_count(), 2);
        assert!(rig.space.is_mapped(va));
        assert!(!rig.space.is_mapped(VirtAddr::new(va.as_u64() + 4096)));
        assert!(rig.space.is_mapped(VirtAddr::new(va.as_u64() + 3 * 4096)));
        // Frames of the unmapped pages were returned (2 of 4).
        let free_now = rig.frames.free_frames();
        rig.space.munmap(&mut rig.frames, va, 1).unwrap();
        rig.space
            .munmap(&mut rig.frames, VirtAddr::new(va.as_u64() + 3 * 4096), 1)
            .unwrap();
        assert_eq!(rig.frames.free_frames(), free_now + 2);
    }

    #[test]
    fn munmap_rejects_unmapped_and_guard_pages() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let va = rig
            .space
            .mmap(
                &mut rig.frames,
                1,
                MmapOptions {
                    guard: GuardPages::low(),
                    ..MmapOptions::new(prot::READ)
                },
            )
            .unwrap();
        // The guard page itself cannot be unmapped (it was never mapped).
        assert_eq!(
            rig.space
                .munmap(&mut rig.frames, VirtAddr::new(va.as_u64() - 4096), 1),
            Err(MmError::PageNotMapped)
        );
        // Neither can a page outside every region.
        assert_eq!(
            rig.space
                .munmap(&mut rig.frames, VirtAddr::new(0x1_0F00_0000), 1),
            Err(MmError::PageNotMapped)
        );
        // A range straddling the region edge is rejected too.
        assert_eq!(
            rig.space.munmap(&mut rig.frames, va, 2),
            Err(MmError::PageNotMapped)
        );
    }

    #[test]
    fn map_fixed_creates_an_mmio_window() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let free_before = rig.frames.free_frames();
        rig.space
            .map_fixed(
                VirtAddr::new(0x1_0000_0000),
                PhysAddr::new(0xFED0_0000),
                2,
                prot::READ | prot::WRITE,
                pg_flags::NO_CACHE,
            )
            .unwrap();
        assert_eq!(
            rig.space.translate(VirtAddr::new(0x1_0000_0000)),
            Some(PhysAddr::new(0xFED0_0000))
        );
        assert_eq!(
            rig.space
                .translate(VirtAddr::new(0x1_0000_0000 + 4096 + 0x123)),
            Some(PhysAddr::new(0xFED0_1000 + 0x123))
        );
        // NO_CACHE reached the leaf...
        let pt = rig.space.pt_of(0x1_0000_0000).unwrap();
        let pte = rig.space.read_entry(pt, pt_index(0x1_0000_0000)).unwrap();
        assert_ne!(pte & pg_flags::NO_CACHE, 0);
        // ...but no frames were consumed, and unmapping frees none.
        assert_eq!(rig.frames.free_frames(), free_before);
        let info = rig.space.region_at(VirtAddr::new(0x1_0000_0000)).unwrap();
        assert!(!info.frames_owned);
        rig.space
            .munmap(&mut rig.frames, VirtAddr::new(0x1_0000_0000), 2)
            .unwrap();
        assert_eq!(rig.frames.free_frames(), free_before);
    }

    #[test]
    fn map_fixed_rejects_overlap_and_bad_input() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let va = rig
            .space
            .mmap(&mut rig.frames, 2, MmapOptions::new(prot::READ))
            .unwrap();
        // Overlapping an anonymous mapping is rejected.
        assert_eq!(
            rig.space
                .map_fixed(va, PhysAddr::new(0xFED0_0000), 1, prot::READ, 0),
            Err(MmError::OverlappingMapping)
        );
        // So is overlapping a fixed mapping with another fixed mapping.
        rig.space
            .map_fixed(
                VirtAddr::new(0x1_0008_0000),
                PhysAddr::new(0xFED0_0000),
                1,
                prot::READ,
                0,
            )
            .unwrap();
        assert_eq!(
            rig.space.map_fixed(
                VirtAddr::new(0x1_0008_0000),
                PhysAddr::new(0xFED8_0000),
                1,
                prot::READ,
                0
            ),
            Err(MmError::OverlappingMapping)
        );
        assert_eq!(
            rig.space.map_fixed(
                VirtAddr::new(0x1_0008_0000 + 4096),
                PhysAddr::new(0xFED0_0000),
                0,
                prot::READ,
                0
            ),
            Err(MmError::EmptyRegion)
        );
        // Unaligned / non-canonical addresses are rejected.
        assert_eq!(
            rig.space.map_fixed(
                VirtAddr::new(0x1_0008_0001),
                PhysAddr::new(0xFED0_0000),
                1,
                prot::READ,
                0
            ),
            Err(MmError::UnalignedAddress)
        );
        assert_eq!(
            rig.space.map_fixed(
                VirtAddr::new(1 << 48),
                PhysAddr::new(0xFED0_0000),
                1,
                prot::READ,
                0
            ),
            Err(MmError::NonCanonicalAddress)
        );
        assert_eq!(
            rig.space.map_fixed(
                VirtAddr::new(0x1_0009_0000),
                PhysAddr::new(0xFED0_0001),
                1,
                prot::READ,
                0
            ),
            Err(MmError::UnalignedAddress)
        );
    }

    #[test]
    fn map_page_rejects_double_map() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        rig.space
            .map_page(
                VirtAddr::new(0x1_0000_0000),
                PhysAddr::new(0x9_0000),
                prot::READ,
                0,
            )
            .unwrap();
        assert_eq!(
            rig.space.map_page(
                VirtAddr::new(0x1_0000_0000),
                PhysAddr::new(0x9_1000),
                prot::READ,
                0
            ),
            Err(MmError::OverlappingMapping)
        );
    }

    #[test]
    fn mprotect_changes_flags_and_splits() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let va = rig
            .space
            .mmap(
                &mut rig.frames,
                3,
                MmapOptions::new(prot::READ | prot::WRITE),
            )
            .unwrap();
        // Downgrade the middle page to read-only.
        rig.space
            .mprotect(VirtAddr::new(va.as_u64() + 4096), 1, prot::READ)
            .unwrap();
        assert_eq!(rig.space.region_count(), 3);
        let pt = rig.space.pt_of(va.as_u64()).unwrap();
        let writable =
            |v: u64| rig.space.read_entry(pt, pt_index(v)).unwrap() & pg_flags::WRITABLE != 0;
        assert!(writable(va.as_u64()));
        assert!(!writable(va.as_u64() + 4096));
        assert!(writable(va.as_u64() + 2 * 4096));
        // The mapping itself is untouched.
        let p_before = rig.space.translate(va).unwrap();
        assert_eq!(rig.space.translate(va), Some(p_before));
        // Upgrade the whole range to RWX: one region again, NX clear.
        rig.space
            .mprotect(va, 3, prot::READ | prot::WRITE | prot::EXEC)
            .unwrap();
        assert_eq!(rig.space.region_count(), 1);
        let pte = rig
            .space
            .read_entry(pt, pt_index(va.as_u64() + 4096))
            .unwrap();
        assert_ne!(pte & pg_flags::WRITABLE, 0);
        assert_eq!(pte & pg_flags::NO_EXECUTE, 0);
        let info = rig.space.region_at(va).unwrap();
        assert_eq!(info.prot, prot::READ | prot::WRITE | prot::EXEC);
    }

    #[test]
    fn mprotect_rejects_unmapped() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let va = rig
            .space
            .mmap(&mut rig.frames, 1, MmapOptions::new(prot::READ))
            .unwrap();
        assert_eq!(
            rig.space
                .mprotect(VirtAddr::new(va.as_u64() + 4096), 1, prot::READ),
            Err(MmError::PageNotMapped)
        );
        // The failed mprotect changed nothing.
        assert_eq!(rig.space.region_count(), 1);
    }

    #[test]
    fn table_pool_exhaustion_fails_cleanly() {
        // One pool page = the PML4 only; the first mapping needs PDPT+PD+PT.
        let image = zeroed_image(65536);
        let mut space =
            HostAddressSpace::empty(image, 0, PhysAddr::new(0), 1, 0x1_0000_0000, 0x10_0000)
                .unwrap();
        assert_eq!(space.table_pages_left(), 0);
        let mut frames = BitmapFrameAllocator::new(PhysAddr::new(0x10_0000), 16 * 4096);
        assert_eq!(
            space.mmap(&mut frames, 1, MmapOptions::new(prot::READ)),
            Err(MmError::OutOfTableMemory)
        );
        // Nothing was recorded and no frame leaked.
        assert_eq!(space.region_count(), 0);
        assert_eq!(frames.free_frames(), 16);
    }

    #[test]
    fn frame_exhaustion_rolls_back() {
        let image = zeroed_image(16 * 1024 * 1024);
        let mut space =
            HostAddressSpace::empty(image, 0, PhysAddr::new(0), 64, 0x1_0000_0000, 0x10_0000)
                .unwrap();
        let mut frames = BitmapFrameAllocator::new(PhysAddr::new(0x10_0000), 2 * 4096);
        assert_eq!(
            space.mmap(&mut frames, 3, MmapOptions::new(prot::READ)),
            Err(MmError::OutOfPhysicalFrames)
        );
        // Both frames are back, nothing is mapped, no region was recorded.
        assert_eq!(frames.free_frames(), 2);
        assert_eq!(space.region_count(), 0);
        assert_eq!(space.translate(VirtAddr::new(0x1_0000_0000)), None);
    }

    #[test]
    fn over_identity_adopts_a_builder_map() {
        let mut mem = vec![0u8; 0x1_4000];
        let layout =
            build_identity_map_2mib(&mut mem, 0x1000, 4 * 1024 * 1024, pg_flags::WRITABLE).unwrap();
        // Move the image to a 'static home the address space can borrow.
        let image = zeroed_image(0x1_4000);
        image.copy_from_slice(&mem);
        let mut space = HostAddressSpace::over_identity(image, layout, 0, 4 * 1024 * 1024).unwrap();
        assert_eq!(space.cr3(), 0x1000);
        // The 2 MiB huge-leaf path translates identity addresses...
        assert_eq!(
            space.translate(VirtAddr::new(0x0020_0123)),
            Some(PhysAddr::new(0x0020_0123))
        );
        assert_eq!(
            space.translate(VirtAddr::new(0x003F_F000)),
            Some(PhysAddr::new(0x003F_F000))
        );
        // ...and unmapped addresses read back None.
        assert_eq!(space.translate(VirtAddr::new(0x0040_0000)), None);
        // A 4-KiB descent under a huge leaf is refused (split first).
        space.set_table_pool(PhysAddr::new(0xC000), 8).unwrap();
        assert_eq!(
            space.map_page(
                VirtAddr::new(0x1000),
                PhysAddr::new(0x9_0000),
                prot::READ,
                0
            ),
            Err(MmError::HugePageOnPath)
        );
        assert_eq!(
            space.unmap_page(VirtAddr::new(0x1000)),
            Err(MmError::HugePageOnPath)
        );
    }

    #[test]
    fn over_identity_rejects_bad_va_range() {
        let build = || {
            let image: &'static mut [u8] = Box::leak(Box::new([0u8; 0x4000]));
            let layout = build_identity_map_2mib(image, 0, 4096, pg_flags::WRITABLE).unwrap();
            (image, layout)
        };
        let (image, layout) = build();
        assert_eq!(
            HostAddressSpace::over_identity(image, layout, 0x1001, 0x1000).unwrap_err(),
            MmError::UnalignedAddress
        );
        let (image, layout) = build();
        assert_eq!(
            HostAddressSpace::over_identity(image, layout, 0, 0).unwrap_err(),
            MmError::EmptyRegion
        );
    }

    #[test]
    fn unmap_page_is_idempotent_on_unmapped() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        assert_eq!(rig.space.unmap_page(VirtAddr::new(0x1_0000_0000)), Ok(None));
        let va = rig
            .space
            .mmap(&mut rig.frames, 1, MmapOptions::new(prot::READ))
            .unwrap();
        let frame = rig.space.unmap_page(va).unwrap().unwrap();
        assert_eq!(frame.start_address().as_u64(), 8 * 1024 * 1024);
        // Second unmap finds nothing (the region record still exists — this is
        // the low-level primitive; `munmap` is the record-aware API).
        assert_eq!(rig.space.unmap_page(va), Ok(None));
    }

    #[test]
    fn translate_handles_offsets_and_unmapped() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let va = rig
            .space
            .mmap(&mut rig.frames, 1, MmapOptions::new(prot::READ))
            .unwrap();
        let pa = rig.space.translate(va).unwrap();
        // The page offset passes through.
        assert_eq!(
            rig.space.translate(VirtAddr::new(va.as_u64() + 0xABC)),
            Some(PhysAddr::new(pa.as_u64() + 0xABC))
        );
        assert_eq!(
            rig.space.translate(VirtAddr::new(0x1_0000_0000 + 0x8000)),
            None
        );
        assert_eq!(rig.space.translate(VirtAddr::new(u64::MAX)), None);
    }

    #[test]
    fn region_at_distinguishes_guard_and_outside() {
        let mut rig = Rig::new(0x1_0000_0000, 0x10_0000);
        let va = rig
            .space
            .mmap(
                &mut rig.frames,
                1,
                MmapOptions {
                    guard: GuardPages::high(),
                    ..MmapOptions::new(prot::READ)
                },
            )
            .unwrap();
        // Inside the usable page and inside the high guard: same region.
        assert_eq!(rig.space.region_at(va).map(|r| r.base), Some(va));
        assert_eq!(
            rig.space
                .region_at(VirtAddr::new(va.as_u64() + 4096))
                .map(|r| r.base),
            Some(va)
        );
        // Past the reservation: no region.
        assert_eq!(rig.space.region_at(VirtAddr::new(va.as_u64() + 8192)), None);
    }

    #[test]
    fn prot_flags_encode_protection_bits() {
        assert_eq!(
            prot_flags(prot::READ),
            pg_flags::PRESENT | pg_flags::NO_EXECUTE
        );
        assert_eq!(
            prot_flags(prot::READ | prot::WRITE),
            pg_flags::PRESENT | pg_flags::WRITABLE | pg_flags::NO_EXECUTE
        );
        assert_eq!(
            prot_flags(prot::READ | prot::WRITE | prot::EXEC),
            pg_flags::PRESENT | pg_flags::WRITABLE
        );
    }

    // `read_cr3` / `install` / `flush_page` / `flush_all` execute privileged
    // instructions (`mov cr3`, `invlpg`) that fault outside ring 0, so they
    // are not exercised by host tests by design. Their contract is: the tables
    // built above are correct (proven by `translate` walking the same entries
    // the CPU walks), and `cr3()` carries the PML4 physical address.
}

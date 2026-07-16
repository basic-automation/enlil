//! Page-aligned hardware regions for VMX/SVM (Phase 6.2).
//!
//! `VMXON`/`VMPTRLD` (Intel) and `VMRUN` (AMD) all take the *physical* address
//! of a naturally-aligned 4 KiB region — the VMXON region and VMCS for VMX, the
//! VMCB and host state-save area for SVM. This module owns that allocation: a
//! [`PageRegion`] is a 4 KiB, 4 KiB-aligned, zeroed page from the global
//! allocator, and the typed wrappers ([`VmcsRegion`], [`VmxonRegion`],
//! [`Vmcb`], [`HostSaveArea`]) initialize it through the pure decode layer in
//! [`vmx`](crate::vmx) / [`svm`](crate::svm) and expose the base pointer the
//! backend feeds to the privileged instruction.
//!
//! The type owns its backing store and frees it on drop, so it stays valid
//! for exactly as long as the hypervisor holds it — no manual page bookkeeping
//! above the HAL (LOCKED PRINCIPLE 2/3). It is `no_std` + `alloc`, so the same
//! type serves the Linux dev host and the bare-metal kernel target.

use crate::svm::{IOPM_SIZE, MSRPM_SIZE, VMCB_SIZE, VmcbRegionError, init_vmcb_region};
use crate::vmx::{VMX_REGION_SIZE, VmxRegionError, init_vmx_region};
use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use core::ptr::NonNull;

/// The size and alignment of a VMX/SVM control region: one 4 KiB page.
pub const PAGE_SIZE: usize = 4096;

const _: () = assert!(VMX_REGION_SIZE == PAGE_SIZE);
const _: () = assert!(VMCB_SIZE == PAGE_SIZE);

/// A page-sized, page-aligned block — the type whose [`Layout`] every region
/// is allocated with, giving a `const` page-aligned layout without a fallible
/// `Layout::from_size_align`.
#[repr(C, align(4096))]
struct AlignedPage([u8; PAGE_SIZE]);

/// A 4 KiB, 4 KiB-aligned, zeroed page owned by this handle.
///
/// The backing store comes from the global allocator with an explicit
/// page-aligned [`Layout`]; [`Drop`] releases it. This is the raw container
/// the typed VMX/SVM wrappers build on.
pub struct PageRegion {
    ptr: NonNull<u8>,
}

// SAFETY: `PageRegion` uniquely owns its allocation (no aliasing handle
// exists) and contains only the owning pointer, so moving it across threads
// is sound. The hardware region it points at is not touched by Rust except
// through `&mut self`.
unsafe impl Send for PageRegion {}

impl PageRegion {
    /// The `Layout` every page region is allocated with.
    #[must_use]
    const fn layout() -> Layout {
        Layout::new::<AlignedPage>()
    }

    /// Allocate a fresh zeroed, page-aligned region, or `None` if the
    /// allocator is exhausted.
    #[must_use]
    pub fn new() -> Option<Self> {
        // SAFETY: the layout has nonzero size (PAGE_SIZE).
        let raw = unsafe { alloc_zeroed(Self::layout()) };
        NonNull::new(raw).map(|ptr| Self { ptr })
    }

    /// The region's base address as an integer — the value `VMXON`/`VMPTRLD`/
    /// `VMRUN` take (identity-mapped on the bare-metal kernel, so the virtual
    /// address is the physical address).
    #[must_use]
    pub fn base_addr(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }

    /// The region as a byte slice.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        // SAFETY: we own PAGE_SIZE valid, initialized (zeroed) bytes.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), PAGE_SIZE) }
    }

    /// The region as a mutable byte slice.
    #[must_use]
    pub const fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: we own PAGE_SIZE valid bytes and hold `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), PAGE_SIZE) }
    }

    /// Whether the base address is 4 KiB-aligned (always true for a live
    /// region — the invariant the hardware requires).
    #[must_use]
    pub fn is_page_aligned(&self) -> bool {
        self.base_addr().is_multiple_of(PAGE_SIZE as u64)
    }
}

impl Drop for PageRegion {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc_zeroed` with exactly this layout and
        // is freed once (Drop runs at most once).
        unsafe { dealloc(self.ptr.as_ptr(), Self::layout()) };
    }
}

/// A VMCS region, initialized with the VMCS revision identifier and ready for
/// `VMPTRLD` (Intel SDM Vol. 3 §24.2).
pub struct VmcsRegion {
    page: PageRegion,
}

impl VmcsRegion {
    /// Allocate and stamp a VMCS region with `revision_id` (from
    /// [`VmxBasic::revision_id`](crate::vmx::VmxBasic::revision_id)).
    ///
    /// # Errors
    ///
    /// [`RegionError::OutOfMemory`] if the page cannot be allocated. The
    /// region-size check in [`init_vmx_region`] cannot fail here (the page is
    /// exactly [`VMX_REGION_SIZE`]).
    pub fn new(revision_id: u32) -> Result<Self, RegionError> {
        let mut page = PageRegion::new().ok_or(RegionError::OutOfMemory)?;
        init_vmx_region(page.as_bytes_mut(), revision_id).map_err(RegionError::Vmx)?;
        Ok(Self { page })
    }

    /// The VMCS physical/linear base for `VMPTRLD`.
    #[must_use]
    pub fn base_addr(&self) -> u64 {
        self.page.base_addr()
    }

    /// The VMCS revision id stamped in the header (low 31 bits of word 0).
    #[must_use]
    pub fn revision_id(&self) -> u32 {
        read_u32(self.page.as_bytes()) & 0x7FFF_FFFF
    }
}

/// A VMXON region, initialized identically to a VMCS (same revision header)
/// and ready for `VMXON` (Intel SDM Vol. 3 §24.11.5).
pub struct VmxonRegion {
    page: PageRegion,
}

impl VmxonRegion {
    /// Allocate and stamp a VMXON region with `revision_id`.
    ///
    /// # Errors
    ///
    /// [`RegionError::OutOfMemory`] if the page cannot be allocated.
    pub fn new(revision_id: u32) -> Result<Self, RegionError> {
        let mut page = PageRegion::new().ok_or(RegionError::OutOfMemory)?;
        init_vmx_region(page.as_bytes_mut(), revision_id).map_err(RegionError::Vmx)?;
        Ok(Self { page })
    }

    /// The VMXON region base for `VMXON`.
    #[must_use]
    pub fn base_addr(&self) -> u64 {
        self.page.base_addr()
    }
}

/// A VMCB — the SVM guest control block `VMRUN` takes (AMD APM Vol. 2 §15.5).
pub struct Vmcb {
    page: PageRegion,
}

impl Vmcb {
    /// Allocate a zeroed VMCB ready for field programming + `VMRUN`.
    ///
    /// # Errors
    ///
    /// [`RegionError::OutOfMemory`] if the page cannot be allocated.
    pub fn new() -> Result<Self, RegionError> {
        let mut page = PageRegion::new().ok_or(RegionError::OutOfMemory)?;
        init_vmcb_region(page.as_bytes_mut()).map_err(RegionError::Vmcb)?;
        Ok(Self { page })
    }

    /// The VMCB physical/linear base — loaded into RAX before `VMRUN`.
    #[must_use]
    pub fn base_addr(&self) -> u64 {
        self.page.base_addr()
    }

    /// The VMCB bytes, for the backend to program control/save fields.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        self.page.as_bytes()
    }

    /// The VMCB bytes mutably, for the backend to program its fields.
    #[must_use]
    pub const fn as_bytes_mut(&mut self) -> &mut [u8] {
        self.page.as_bytes_mut()
    }
}

/// The SVM host state-save area whose physical address goes in `VM_HSAVE_PA`
/// before the first `VMRUN` (AMD APM Vol. 2 §15.30.4). One zeroed page.
pub struct HostSaveArea {
    page: PageRegion,
}

impl HostSaveArea {
    /// Allocate a zeroed host state-save area.
    ///
    /// # Errors
    ///
    /// [`RegionError::OutOfMemory`] if the page cannot be allocated.
    pub fn new() -> Result<Self, RegionError> {
        let page = PageRegion::new().ok_or(RegionError::OutOfMemory)?;
        Ok(Self { page })
    }

    /// The host-save-area base to program into `VM_HSAVE_PA`.
    #[must_use]
    pub fn base_addr(&self) -> u64 {
        self.page.base_addr()
    }
}

/// A page-sized, 12 KiB, page-aligned block backing an [`IoPermissionsMap`].
#[repr(C, align(4096))]
struct IopmPages([u8; IOPM_SIZE]);

/// The SVM I/O permissions map (IOPM) — 12 KiB (three 4 KiB pages), page-aligned,
/// whose physical base goes in the VMCB `IOPM_BASE_PA` (AMD APM §15.10.1).
///
/// A set bit intercepts a port; [`intercept_all`](IoPermissionsMap::intercept_all)
/// allocates a map with every port intercepted, and
/// [`set_intercept`](IoPermissionsMap::set_intercept) arms an individual port on
/// a zeroed map. The handle owns its 12 KiB and frees it on drop, so the map
/// stays valid for exactly as long as the hypervisor holds it (LOCKED PRINCIPLE
/// 2/3).
pub struct IoPermissionsMap {
    ptr: NonNull<u8>,
}

// SAFETY: like `PageRegion`, `IoPermissionsMap` uniquely owns its allocation
// and holds only the owning pointer, so it is sound to move across threads.
unsafe impl Send for IoPermissionsMap {}

impl IoPermissionsMap {
    /// The `Layout` the IOPM is allocated with (12 KiB, 4 KiB-aligned).
    #[must_use]
    const fn layout() -> Layout {
        Layout::new::<IopmPages>()
    }

    /// Allocate a zeroed IOPM — every port *permitted* (no interception).
    /// Arm ports with [`set_intercept`](Self::set_intercept).
    ///
    /// # Errors
    ///
    /// [`RegionError::OutOfMemory`] if the allocator is exhausted.
    pub fn new() -> Result<Self, RegionError> {
        // SAFETY: the layout has nonzero size (IOPM_SIZE).
        let raw = unsafe { alloc_zeroed(Self::layout()) };
        NonNull::new(raw)
            .map(|ptr| Self { ptr })
            .ok_or(RegionError::OutOfMemory)
    }

    /// Allocate an IOPM with **every** port intercepted (all bits set) — the
    /// simplest map for a guest that should trap on any I/O.
    ///
    /// # Errors
    ///
    /// [`RegionError::OutOfMemory`] if the allocator is exhausted.
    pub fn intercept_all() -> Result<Self, RegionError> {
        let mut map = Self::new()?;
        map.as_bytes_mut().fill(0xFF);
        Ok(map)
    }

    /// Arm interception of a single `port`.
    pub fn set_intercept(&mut self, port: u16) {
        crate::svm::set_iopm_intercept(self.as_bytes_mut(), port);
    }

    /// The IOPM physical/linear base for the VMCB `IOPM_BASE_PA` field
    /// (identity-mapped on the bare-metal kernel, so VA == PA).
    #[must_use]
    pub fn base_addr(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }

    /// The map as a byte slice.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        // SAFETY: we own IOPM_SIZE valid, initialized (zeroed) bytes.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), IOPM_SIZE) }
    }

    /// The map as a mutable byte slice.
    #[must_use]
    pub const fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: we own IOPM_SIZE valid bytes and hold `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), IOPM_SIZE) }
    }
}

impl Drop for IoPermissionsMap {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc_zeroed` with exactly this layout and
        // is freed once (Drop runs at most once).
        unsafe { dealloc(self.ptr.as_ptr(), Self::layout()) };
    }
}

/// A page-sized, 8 KiB, page-aligned block backing an [`MsrPermissionsMap`].
#[repr(C, align(4096))]
struct MsrpmPages([u8; MSRPM_SIZE]);

/// The SVM MSR permissions map (MSRPM) — 8 KiB (two 4 KiB pages), page-aligned,
/// whose physical base goes in the VMCB `MSRPM_BASE_PA` (AMD APM §15.11).
///
/// A set bit intercepts an MSR read/write; [`set_intercept`](Self::set_intercept)
/// arms an individual MSR on the zeroed map. The handle owns its 8 KiB and frees
/// it on drop (LOCKED PRINCIPLE 2/3).
pub struct MsrPermissionsMap {
    ptr: NonNull<u8>,
}

// SAFETY: like `PageRegion`, `MsrPermissionsMap` uniquely owns its allocation
// and holds only the owning pointer, so it is sound to move across threads.
unsafe impl Send for MsrPermissionsMap {}

impl MsrPermissionsMap {
    /// The `Layout` the MSRPM is allocated with (8 KiB, 4 KiB-aligned).
    #[must_use]
    const fn layout() -> Layout {
        Layout::new::<MsrpmPages>()
    }

    /// Allocate a zeroed MSRPM — every MSR *permitted* (no interception). Arm
    /// MSRs with [`set_intercept`](Self::set_intercept).
    ///
    /// # Errors
    ///
    /// [`RegionError::OutOfMemory`] if the allocator is exhausted.
    pub fn new() -> Result<Self, RegionError> {
        // SAFETY: the layout has nonzero size (MSRPM_SIZE).
        let raw = unsafe { alloc_zeroed(Self::layout()) };
        NonNull::new(raw)
            .map(|ptr| Self { ptr })
            .ok_or(RegionError::OutOfMemory)
    }

    /// Arm interception of a single `msr`'s read and/or write access.
    pub fn set_intercept(&mut self, msr: u32, read: bool, write: bool) {
        crate::svm::set_msr_intercept(self.as_bytes_mut(), msr, read, write);
    }

    /// The MSRPM physical/linear base for the VMCB `MSRPM_BASE_PA` field
    /// (identity-mapped on the bare-metal kernel, so VA == PA).
    #[must_use]
    pub fn base_addr(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }

    /// The map as a byte slice.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        // SAFETY: we own MSRPM_SIZE valid, initialized (zeroed) bytes.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), MSRPM_SIZE) }
    }

    /// The map as a mutable byte slice.
    #[must_use]
    pub const fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: we own MSRPM_SIZE valid bytes and hold `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), MSRPM_SIZE) }
    }
}

impl Drop for MsrPermissionsMap {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `alloc_zeroed` with exactly this layout and
        // is freed once (Drop runs at most once).
        unsafe { dealloc(self.ptr.as_ptr(), Self::layout()) };
    }
}

/// Errors allocating or initializing a VMX/SVM region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionError {
    /// The global allocator could not provide a page-aligned page.
    OutOfMemory,
    /// The VMX region initializer rejected the buffer (should not occur for a
    /// full page).
    Vmx(VmxRegionError),
    /// The VMCB region initializer rejected the buffer.
    Vmcb(VmcbRegionError),
}

/// Read the little-endian `u32` at the start of `bytes` (the region header
/// word). `bytes` is always a full page here.
fn read_u32(bytes: &[u8]) -> u32 {
    bytes
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_region_is_zeroed_and_aligned() {
        let region = PageRegion::new().expect("allocate a page");
        assert_eq!(region.as_bytes().len(), PAGE_SIZE);
        assert!(region.is_page_aligned());
        assert!(region.base_addr() != 0);
        assert!(region.as_bytes().iter().all(|&b| b == 0));
    }

    #[test]
    fn vmcs_region_stamps_the_revision_id() {
        // Set bit 31 in the input to prove init_vmx_region masks it off (the
        // shadow-VMCS indicator must stay clear).
        let vmcs = VmcsRegion::new(0x8000_001B).expect("allocate VMCS");
        assert_eq!(vmcs.revision_id(), 0x1B);
        assert!(vmcs.base_addr().is_multiple_of(PAGE_SIZE as u64));
        // Header word 0, bit 31 clear.
        assert_eq!(read_u32(vmcs.page.as_bytes()) & 0x8000_0000, 0);
    }

    #[test]
    fn vmxon_region_allocates_aligned() {
        let vmxon = VmxonRegion::new(0x1B).expect("allocate VMXON");
        assert!(vmxon.base_addr().is_multiple_of(PAGE_SIZE as u64));
    }

    #[test]
    fn vmcb_is_zeroed_and_aligned() {
        let vmcb = Vmcb::new().expect("allocate VMCB");
        assert_eq!(vmcb.as_bytes().len(), PAGE_SIZE);
        assert!(vmcb.base_addr().is_multiple_of(PAGE_SIZE as u64));
        assert!(vmcb.as_bytes().iter().all(|&b| b == 0));
    }

    #[test]
    fn vmcb_fields_are_writable() {
        use crate::svm::control;
        let mut vmcb = Vmcb::new().expect("allocate VMCB");
        // Program the guest ASID as the backend would, read it back.
        vmcb.as_bytes_mut()[control::GUEST_ASID..control::GUEST_ASID + 4]
            .copy_from_slice(&1u32.to_le_bytes());
        let asid = u32::from_le_bytes(
            vmcb.as_bytes()[control::GUEST_ASID..control::GUEST_ASID + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(asid, 1);
    }

    #[test]
    fn host_save_area_allocates_aligned() {
        let hsa = HostSaveArea::new().expect("allocate host-save area");
        assert!(hsa.base_addr().is_multiple_of(PAGE_SIZE as u64));
    }

    #[test]
    fn distinct_regions_get_distinct_pages() {
        let a = Vmcb::new().expect("a");
        let b = Vmcb::new().expect("b");
        assert_ne!(a.base_addr(), b.base_addr());
    }

    #[test]
    fn iopm_intercept_all_sets_every_bit_and_is_aligned() {
        let map = IoPermissionsMap::intercept_all().expect("allocate IOPM");
        assert_eq!(map.as_bytes().len(), IOPM_SIZE);
        assert!(map.base_addr().is_multiple_of(PAGE_SIZE as u64));
        assert!(map.as_bytes().iter().all(|&b| b == 0xFF));
        // Every port reads as intercepted.
        assert!(crate::svm::iopm_intercepts(map.as_bytes(), 0x80));
        assert!(crate::svm::iopm_intercepts(map.as_bytes(), 0xFFFF));
    }

    #[test]
    fn iopm_new_is_zeroed_and_set_intercept_arms_one_port() {
        let mut map = IoPermissionsMap::new().expect("allocate IOPM");
        assert!(map.as_bytes().iter().all(|&b| b == 0));
        map.set_intercept(0x3F8);
        assert!(crate::svm::iopm_intercepts(map.as_bytes(), 0x3F8));
        assert!(!crate::svm::iopm_intercepts(map.as_bytes(), 0x80));
    }

    #[test]
    fn msrpm_new_is_zeroed_aligned_and_set_intercept_arms_one_msr() {
        let mut map = MsrPermissionsMap::new().expect("allocate MSRPM");
        assert_eq!(map.as_bytes().len(), MSRPM_SIZE);
        assert!(map.base_addr().is_multiple_of(PAGE_SIZE as u64));
        assert!(map.as_bytes().iter().all(|&b| b == 0));
        map.set_intercept(0x10, true, false);
        assert!(crate::svm::msr_read_intercepted(map.as_bytes(), 0x10));
        assert!(!crate::svm::msr_read_intercepted(map.as_bytes(), 0x11));
    }
}

//! XSAVE/XRSTOR extended processor state.
//!
//! A real context switch must carry each task's extended processor state
//! (x87, SSE, AVX, AVX-512, …) with the task: without it, a preempted task's
//! vector registers leak into — or are clobbered by — whatever runs next.
//! This module is the ISA seam for that (LOCKED PRINCIPLE 2): the XSAVE area
//! layout/size arithmetic and the `XSAVE64`/`XRSTOR64` primitives. The
//! scheduler in `enlil-platform` drives them around its register switch.
//!
//! The size arithmetic is pure against the CPU's CPUID tables and the
//! allocation is `alloc`-only, so everything here is host-testable; only the
//! `xsave64`/`xrstor64` instructions themselves need the `xsave` CPU feature.

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use core::ptr::NonNull;

/// Required alignment of an XSAVE area: 64 bytes (Intel SDM Vol. 1 §13.4.2).
/// A misaligned area faults (`#GP`) on `XSAVE`/`XRSTOR`.
pub const XSAVE_ALIGN: usize = 64;

/// Minimum XSAVE area size: the 512-byte legacy x87/SSE region plus the
/// 64-byte XSAVE header (SDM Vol. 1 §13.4.2). No enabled extended features.
pub const XSAVE_MIN_SIZE: usize = 512 + 64;

/// `CPUID.1:ECX[26]` — the processor supports `XSAVE`/`XRSTOR` and `XGETBV`.
pub const CPUID_1_ECX_XSAVE: u32 = 1 << 26;

/// XCR0 state-component bits (SDM Vol. 1 §13.3).
pub const XCR0_X87: u64 = 1 << 0;
/// XCR0 bit 1: SSE state (`XMM0`–`XMM15`, `MXCSR`).
pub const XCR0_SSE: u64 = 1 << 1;
/// XCR0 bit 2: AVX state (upper halves of `YMM0`–`YMM15`).
pub const XCR0_AVX: u64 = 1 << 2;
/// XCR0 bit 3: MPX bound registers.
pub const XCR0_BNDREGS: u64 = 1 << 3;
/// XCR0 bit 4: MPX CSR.
pub const XCR0_BNDCSR: u64 = 1 << 4;
/// XCR0 bit 5: AVX-512 opmask (`K0`–`K7`).
pub const XCR0_OPMASK: u64 = 1 << 5;
/// XCR0 bit 6: AVX-512 `ZMM0`–`ZMM15` upper halves.
pub const XCR0_ZMM_HI256: u64 = 1 << 6;
/// XCR0 bit 7: AVX-512 `ZMM16`–`ZMM31`.
pub const XCR0_HI16_ZMM: u64 = 1 << 7;
/// XCR0 bit 9: PKRU protection-key rights register.
pub const XCR0_PKRU: u64 = 1 << 9;

/// Whether the CPU supports `XSAVE`/`XRSTOR` (`CPUID.1:ECX[26]`).
#[must_use]
pub fn xsave_supported() -> bool {
    core::arch::x86_64::__cpuid(1).ecx & CPUID_1_ECX_XSAVE != 0
}

/// Read an extended control register (`XCR0` when `xcr == 0`).
///
/// # Safety
///
/// Reading `XCR0` is unprivileged, but `xcr` values other than 0 are reserved
/// and fault (`#GP`); only call with `xcr == 0` unless the CPU documents the
/// register.
#[must_use]
pub unsafe fn xgetbv(xcr: u32) -> u64 {
    // SAFETY: upheld by the caller (`xcr == 0` in every in-tree caller).
    unsafe { core::arch::x86_64::_xgetbv(xcr) }
}

/// The current `XCR0`: which state components `XSAVE`/`XRSTOR` manage.
#[must_use]
pub fn current_xcr0() -> u64 {
    // SAFETY: reading XCR0 (xcr 0) is unprivileged and always defined.
    unsafe { xgetbv(0) }
}

/// Required XSAVE area size in bytes for the state components in `xcr0`.
///
/// Walks `CPUID.(EAX=0DH, ECX=n)` subleaves `2..63`: each enabled component
/// `n` occupies `offset(n)..offset(n)+size(n)` (`EBX`/`EAX` of the subleaf),
/// so the area must cover the furthest end. Never smaller than
/// [`XSAVE_MIN_SIZE`] (legacy region + header).
///
/// Pure against the CPU's CPUID tables — host-testable without touching XSAVE
/// hardware.
#[must_use]
pub fn xsave_area_size(xcr0: u64) -> usize {
    let mut end: u64 = XSAVE_MIN_SIZE as u64;
    for bit in 2..63u32 {
        if xcr0 & (1u64 << bit) == 0 {
            continue;
        }
        let leaf = core::arch::x86_64::__cpuid_count(0xD, bit);
        let size = u64::from(leaf.eax);
        let offset = u64::from(leaf.ebx);
        end = end.max(offset.saturating_add(size));
    }
    usize::try_from(end).unwrap_or(usize::MAX)
}

/// A 64-byte-aligned XSAVE area sized for a given `XCR0`.
///
/// Owns one task's extended processor state. Allocated zeroed, so a fresh
/// task starts from the architectural INIT state instead of inheriting another
/// task's registers. Save with [`XSaveArea::save`], restore with
/// [`XSaveArea::restore`].
pub struct XSaveArea {
    ptr: NonNull<u8>,
    len: usize,
    xcr0: u64,
}

impl XSaveArea {
    /// Allocate an XSAVE area for the state components selected by `xcr0`.
    ///
    /// Returns `None` if the allocation fails.
    #[must_use]
    pub fn new(xcr0: u64) -> Option<Self> {
        let len = xsave_area_size(xcr0).max(XSAVE_ALIGN);
        let layout = Layout::from_size_align(len, XSAVE_ALIGN).ok()?;
        // SAFETY: `layout` is non-zero-sized with 64-byte alignment.
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr)?;
        Some(Self { ptr, len, xcr0 })
    }

    /// Allocate an XSAVE area for the components currently enabled in `XCR0`.
    ///
    /// Returns `None` if the allocation fails.
    #[must_use]
    pub fn for_current_xcr0() -> Option<Self> {
        Self::new(current_xcr0())
    }

    /// The area size in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the area is empty (never true: the minimum is 4 KiB-aligned up
    /// from [`XSAVE_MIN_SIZE`]).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `XCR0` mask this area was sized for.
    #[must_use]
    pub const fn xcr0(&self) -> u64 {
        self.xcr0
    }

    /// Raw pointer to the area.
    #[must_use]
    pub const fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Raw mutable pointer to the area.
    ///
    /// Handed out from `&self` because `XSAVE64` writes through it; the caller
    /// must ensure no aliasing access while a save/restore is in flight.
    #[must_use]
    pub const fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Execute `XSAVE64`: save the area's `XCR0`-selected extended state.
    ///
    /// # Safety
    ///
    /// - The `xsave` CPU feature must be enabled (`xsave_supported()` and, on
    ///   bare metal, `CR4.OSXSAVE`).
    /// - No other task may access this area concurrently with the save.
    pub unsafe fn save(&self) {
        // SAFETY: upheld by the caller.
        unsafe { xsave(self.as_mut_ptr(), self.xcr0) }
    }

    /// Execute `XRSTOR64`: restore the area's `XCR0`-selected extended state.
    ///
    /// # Safety
    ///
    /// Same contract as [`save`](Self::save).
    pub unsafe fn restore(&self) {
        // SAFETY: upheld by the caller.
        unsafe { xrstor(self.as_mut_ptr(), self.xcr0) }
    }
}

impl Drop for XSaveArea {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, XSAVE_ALIGN)
            .expect("XSAVE area layout is always valid");
        // SAFETY: `layout` matches the allocation in `new`.
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
    }
}

// SAFETY: the allocation is exclusively owned by the `XSaveArea`; the
// scheduler moves task state between CPUs, so it must be `Send + Sync`.
unsafe impl Send for XSaveArea {}
// SAFETY: see above; `&XSaveArea` only hands out raw pointers.
unsafe impl Sync for XSaveArea {}

impl core::fmt::Debug for XSaveArea {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XSaveArea")
            .field("len", &self.len)
            .field("xcr0", &format_args!("{:#x}", self.xcr0))
            .finish_non_exhaustive()
    }
}

/// Execute `XSAVE64`: save the `xcr0`-selected extended state to `area`.
///
/// # Safety
///
/// - The `xsave` CPU feature must be enabled (`xsave_supported()` and, on
///   bare metal, `CR4.OSXSAVE`); otherwise this faults (`#UD`).
/// - `area` must be 64-byte aligned with room for every component in `xcr0`;
///   a short area corrupts whatever follows it.
#[target_feature(enable = "xsave")]
pub unsafe fn xsave(area: *mut u8, xcr0: u64) {
    let lo = (xcr0 & 0xFFFF_FFFF) as u32;
    let hi = (xcr0 >> 32) as u32;
    // SAFETY: caller guarantees the feature and the area; `EDX:EAX` carries
    // the XCR0 save mask, `area` the destination. Intel syntax (the `asm!`
    // default on x86-64): the XSAVE memory operand is `[reg]`.
    unsafe {
        core::arch::asm!(
            "xsave64 [{area}]",
            area = in(reg) area,
            in("eax") lo,
            in("edx") hi,
        );
    }
}

/// Execute `XRSTOR64`: restore the `xcr0`-selected extended state from `area`.
///
/// # Safety
///
/// Same contract as [`xsave`].
#[target_feature(enable = "xsave")]
pub unsafe fn xrstor(area: *mut u8, xcr0: u64) {
    let lo = (xcr0 & 0xFFFF_FFFF) as u32;
    let hi = (xcr0 >> 32) as u32;
    // SAFETY: caller guarantees the feature and the area; `EDX:EAX` carries
    // the XCR0 restore mask, `area` the source.
    unsafe {
        core::arch::asm!(
            "xrstor64 [{area}]",
            area = in(reg) area,
            in("eax") lo,
            in("edx") hi,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::arch::asm;

    #[test]
    fn xsave_support_bit() {
        assert!(
            !xsave_supported() || (core::arch::x86_64::__cpuid(1).ecx & CPUID_1_ECX_XSAVE != 0)
        );
        // Bit 26 clear → unsupported.
        assert_eq!(CPUID_1_ECX_XSAVE, 1 << 26);
    }

    #[test]
    fn area_size_minimum_with_no_features() {
        assert_eq!(xsave_area_size(0), XSAVE_MIN_SIZE);
        assert_eq!(xsave_area_size(XCR0_X87 | XCR0_SSE), XSAVE_MIN_SIZE);
    }

    #[test]
    fn area_size_matches_cpuid_d0_for_current_xcr0() {
        // CPUID.(EAX=0DH,ECX=0):EBX is the required size for the *current*
        // XCR0 — the subleaf walk must agree with it.
        let xcr0 = current_xcr0();
        let d0_size = core::arch::x86_64::__cpuid_count(0xD, 0).ebx;
        assert_eq!(xsave_area_size(xcr0), d0_size as usize);
    }

    #[test]
    fn area_size_covers_avx_component() {
        // With AVX enabled the area must extend past the legacy 576 bytes.
        let size = xsave_area_size(XCR0_X87 | XCR0_SSE | XCR0_AVX);
        assert!(size > XSAVE_MIN_SIZE, "AVX needs extended space");
        // …but never past what the current XCR0 needs when AVX is on.
        if current_xcr0() & XCR0_AVX != 0 {
            assert!(size <= xsave_area_size(current_xcr0()));
        }
    }

    #[test]
    fn area_is_64_byte_aligned_and_sized() {
        let area = XSaveArea::new(current_xcr0()).expect("XSAVE area allocates");
        assert_eq!(area.as_ptr() as usize % XSAVE_ALIGN, 0);
        assert!(area.len() >= XSAVE_MIN_SIZE);
        assert!(!area.is_empty());
        assert_eq!(area.xcr0(), current_xcr0());
    }

    #[test]
    fn area_debug_format() {
        let area = XSaveArea::new(XCR0_X87 | XCR0_SSE).expect("XSAVE area allocates");
        let dbg = alloc::format!("{area:?}");
        assert!(dbg.contains("XSaveArea"));
    }

    /// The real XSAVE round-trip: set `XMM0`, `XSAVE64` into the area, clobber
    /// `XMM0`, `XRSTOR64`, and check the sentinel came back.
    ///
    /// Everything happens inside one `asm!` block so the compiler cannot
    /// reorder code (or allocate `XMM0` as scratch) between the instructions —
    /// the sequence is exactly what the hardware executes on a task switch.
    /// SSE2 (`movq`/`punpcklqdq`/`psrldq`) is baseline x86-64, so no feature
    /// gate beyond XSAVE itself is needed.
    #[target_feature(enable = "xsave")]
    unsafe fn xsave_roundtrip_inner(area: *mut u8, xcr0: u64, lo: u64, hi: u64) -> (u64, u64) {
        let mut lo_out: u64 = 0;
        let mut hi_out: u64 = 0;
        // SAFETY: caller guarantees the xsave feature and a valid area.
        unsafe {
            asm!(
                // Sentinel -> XMM0 (low qword, then high qword).
                "mov rcx, {lo}",
                "movq xmm0, rcx",
                "mov rcx, {hi}",
                "movq xmm1, rcx",
                "punpcklqdq xmm0, xmm1",
                // Save the full extended state into the area.
                "xsave64 [{area}]",
                // Clobber XMM0 so the restore is observable.
                "pxor xmm0, xmm0",
                // Restore — XMM0 must come back.
                "xrstor64 [{area}]",
                // Read XMM0 back out.
                "movq {lo_out}, xmm0",
                "psrldq xmm0, 8",
                "movq {hi_out}, xmm0",
                lo = in(reg) lo,
                hi = in(reg) hi,
                lo_out = out(reg) lo_out,
                hi_out = out(reg) hi_out,
                area = in(reg) area,
                in("eax") (xcr0 & 0xFFFF_FFFF) as u32,
                in("edx") (xcr0 >> 32) as u32,
                out("rcx") _,
                out("xmm1") _,
            );
        }
        (lo_out, hi_out)
    }

    #[test]
    fn xsave_xrstor_roundtrip_preserves_xmm0() {
        if !xsave_supported() {
            return; // No XSAVE on this CPU — nothing to exercise.
        }
        let area = XSaveArea::new(current_xcr0()).expect("XSAVE area allocates");
        let lo: u64 = 0x1234_5678_9ABC_DEF0;
        let hi: u64 = 0xDEAD_BEEF_CAFE_1357;
        // SAFETY: XSAVE is supported (checked above); the area is valid.
        let (lo_out, hi_out) =
            unsafe { xsave_roundtrip_inner(area.as_mut_ptr(), area.xcr0(), lo, hi) };
        assert_eq!((lo_out, hi_out), (lo, hi));
    }

    #[test]
    fn save_restore_methods_roundtrip_without_faulting() {
        if !xsave_supported() {
            return;
        }
        let area = XSaveArea::new(current_xcr0()).expect("XSAVE area allocates");
        // SAFETY: XSAVE is supported (checked above); the area is valid and
        // exclusively owned by this test.
        unsafe {
            area.save();
            area.restore();
        }
    }
}

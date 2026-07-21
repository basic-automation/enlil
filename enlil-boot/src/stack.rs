//! A kernel-owned stack with a guard page (Phase 6.2).
//!
//! After `ExitBootServices()` the kernel is still running on the stack the
//! firmware gave it, which lives in boot-services memory the kernel intends to
//! reclaim, and which has no guard page: a deep call chain or a large stack
//! frame walks straight off the bottom into whatever is below and corrupts it
//! silently. Neither is acceptable once the kernel owns the machine.
//!
//! This module allocates a stack the kernel owns and marks the page below it
//! **not present**, so an overflow takes a `#PF` — a diagnosable fault on a
//! known address — instead of scribbling on the neighbouring allocation. The
//! guard is real: the host map's 2 MiB huge page covering the stack is refined
//! to 4 KiB leaves ([`HostMap::split_to_4kib`](crate::paging::HostMap)) and the
//! guard slot left absent, which
//! [`HostMap::translate`](crate::paging::HostMap::translate) confirms by walking
//! the same tables the CPU does.
//!
//! [`run_on_guarded_stack`] switches `RSP` onto that stack for the duration of
//! one call and restores it afterwards — the primitive; adopting it permanently
//! for `kernel_entry` is the next step.

/// The usable stack size (2 MiB minus the guard page below it).
pub const STACK_BYTES: u64 = 2 * 1024 * 1024 - 4096;

/// Whether `rsp` lies within the usable span of a stack based at `base`.
///
/// The usable span is `[base + 4096, base + 2 MiB)` — the first page is the
/// guard. Pure so the range logic is host-testable.
#[must_use]
pub const fn rsp_in_stack(rsp: u64, base: u64) -> bool {
    rsp > base + 4096 && rsp <= base + 4096 + STACK_BYTES
}

/// The 16-byte-aligned initial stack pointer for a stack based at `base`.
///
/// x86-64 stacks grow down, so this is the top of the region.  requires
/// `RSP` to be 16-byte aligned immediately before a `call`.
#[must_use]
pub const fn stack_top(base: u64) -> u64 {
    (base + 4096 + STACK_BYTES) & !0xF
}

/// The guard page's address for a stack based at `base` — the lowest page,
/// which the stack grows down toward.
#[must_use]
pub const fn guard_page(base: u64) -> u64 {
    base
}

#[cfg(target_os = "uefi")]
pub use hw::{
    GuardedStack, allocate_guarded_stack, current_rsp, run_on_guarded_stack,
    switch_to_guarded_stack, usable_bytes,
};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{STACK_BYTES, guard_page, stack_top};
    use crate::paging::HostMap;
    use alloc::alloc::{Layout, alloc_zeroed};

    /// A kernel-owned stack whose lowest page is unmapped.
    #[derive(Debug, Clone, Copy)]
    pub struct GuardedStack {
        /// 2 MiB-aligned base of the region; also the guard page.
        pub base: u64,
        /// The initial (16-byte-aligned) stack pointer — the top.
        pub top: u64,
    }

    /// Allocate a 2 MiB-aligned stack region and unmap its lowest page.
    ///
    /// Returns `None` if the region could not be allocated, the host map had no
    /// spare page-table page, or — the check that makes this meaningful — the
    /// guard page did not actually come out unmapped, or a page that must stay
    /// mapped came out absent. The region is leaked: a stack must outlive the
    /// call that made it.
    #[must_use]
    pub fn allocate_guarded_stack(map: &mut HostMap) -> Option<GuardedStack> {
        const REGION: usize = 2 * 1024 * 1024;
        let layout = Layout::from_size_align(REGION, REGION).ok()?;
        // SAFETY: layout has a nonzero size; alloc_zeroed yields a block or null.
        let raw = unsafe { alloc_zeroed(layout) };
        if raw.is_null() {
            return None;
        }
        let base = raw as u64;
        let guard = guard_page(base);

        // SAFETY: the guard is the lowest page of a region allocated just above,
        // which nothing else references; the stack is placed strictly above it,
        // so the kernel never touches the guard except by overflowing — which is
        // precisely what must fault.
        unsafe { map.split_to_4kib(base, &[guard])? };

        // Prove it rather than assume it: the guard must be unmapped and the
        // first usable page must not be.
        if map.translate(guard).is_some() || map.translate(guard + 4096).is_none() {
            return None;
        }
        Some(GuardedStack {
            base,
            top: stack_top(base),
        })
    }

    /// Run `f` with `RSP` switched onto `stack`, restoring the caller's stack
    /// afterwards.
    ///
    /// `f` takes no arguments and returns nothing; it communicates through
    /// statics (the caller's locals live on the old stack, which is untouched
    /// and restored on return).
    ///
    /// # Safety
    ///
    /// `stack` must be a live, mapped, exclusively-owned stack region — one from
    /// [`allocate_guarded_stack`]. `f` must not itself switch stacks, and must
    /// not use more than [`STACK_BYTES`] (an overflow lands on the guard page and
    /// takes a `#PF`, which is the point).
    pub unsafe fn run_on_guarded_stack(stack: GuardedStack, f: extern "C" fn()) {
        // RBP is callee-saved, so it survives the call and can hold the old RSP;
        // it is pushed/popped by hand because the compiler may be using it as a
        // frame pointer. The called function clobbers the volatile registers.
        // SAFETY: the caller guarantees `stack` is a live owned stack whose top
        // is 16-byte aligned (`SysV`'s requirement before a `call`), and RSP is
        // restored from RBP before returning, so the caller's frame is intact.
        unsafe {
            core::arch::asm!(
                "push rbp",
                "mov rbp, rsp",
                "mov rsp, {top}",
                "call {func}",
                "mov rsp, rbp",
                "pop rbp",
                top = in(reg) stack.top,
                func = in(reg) f,
                clobber_abi("sysv64"),
            );
        }
    }

    /// Switch `RSP` onto `stack` **permanently** and continue in `f`, which never
    /// returns.
    ///
    /// Where [`run_on_guarded_stack`] borrows the stack for one call, this hands
    /// the rest of the kernel's life to it: everything after this point runs on
    /// memory the kernel owns, with a guard page beneath it, instead of on the
    /// firmware's boot-services stack. Nothing on the old stack is reachable
    /// afterwards, so `f` takes its inputs from statics.
    ///
    /// # Safety
    ///
    /// `stack` must be a live, mapped, exclusively-owned stack region — one from
    /// [`allocate_guarded_stack`]. The caller must not need anything on its own
    /// stack afterwards (locals, saved registers, return address): control never
    /// comes back. Any `&` borrow held across this call must point outside the
    /// old stack.
    pub unsafe fn switch_to_guarded_stack(stack: GuardedStack, f: extern "C" fn() -> !) -> ! {
        // `call` rather than `jmp`: it pushes a return address, leaving RSP
        // 8 mod 16 at `f`'s entry, exactly what SysV specifies. The pushed
        // address is never used — `f` diverges.
        // SAFETY: the caller guarantees `stack` is a live owned stack with a
        // 16-byte-aligned top, and that abandoning the current stack is intended.
        unsafe {
            core::arch::asm!(
                "mov rsp, {top}",
                "call {func}",
                top = in(reg) stack.top,
                func = in(reg) f,
                options(noreturn),
            );
        }
    }

    /// Read the current stack pointer.
    #[must_use]
    pub fn current_rsp() -> u64 {
        let rsp: u64;
        // SAFETY: a plain register read with no memory or flag effects.
        unsafe {
            core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags))
        };
        rsp
    }

    /// The usable byte span of a guarded stack (for reporting).
    #[must_use]
    pub const fn usable_bytes() -> u64 {
        STACK_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_top_is_above_the_guard_and_16_byte_aligned() {
        let base = 0x40_0000u64;
        let top = stack_top(base);
        assert_eq!(top % 16, 0, " requires 16-byte alignment before a call");
        assert!(top > base + 4096, "top must be above the guard page");
        assert_eq!(top, base + 2 * 1024 * 1024);
    }

    #[test]
    fn guard_page_is_the_lowest_page_of_the_region() {
        let base = 0x40_0000u64;
        assert_eq!(guard_page(base), base);
        // The stack grows down toward it, so it is below every usable address.
        assert!(!rsp_in_stack(guard_page(base), base));
        assert!(
            !rsp_in_stack(base + 4096, base),
            "guard page end is not usable"
        );
    }

    #[test]
    fn rsp_in_stack_accepts_the_usable_span_only() {
        let base = 0x40_0000u64;
        assert!(rsp_in_stack(stack_top(base), base), "the top is usable");
        assert!(rsp_in_stack(base + 8192, base));
        // One byte past the guard page is the first usable address.
        assert!(rsp_in_stack(base + 4097, base));
        // Outside the region entirely.
        assert!(!rsp_in_stack(base - 8, base));
        assert!(!rsp_in_stack(base + 2 * 1024 * 1024 + 8, base));
    }

    #[test]
    fn usable_span_is_the_region_minus_one_guard_page() {
        assert_eq!(STACK_BYTES + 4096, 2 * 1024 * 1024);
    }
}

//! Application-processor (AP) bring-up (Phase 6.2, SMP).
//!
//! The BSP wakes each AP with the `INIT`-`SIPI`-`SIPI` sequence (SDM Vol. 3
//! §9.4.4). An AP comes out of reset in **real mode**, executing at
//! `vector << 12` — so the code it starts on has to be 16-bit and live in a page
//! below 1 MiB that the firmware says is free
//! ([`find_ap_trampoline_page`](crate::kernel::find_ap_trampoline_page)).
//!
//! This module holds that first-instructions trampoline and the wake sequence.
//! The trampoline deliberately does the least possible: establish addressing,
//! bump a counter the BSP polls, and halt. That makes "did the AP actually
//! start?" a question with a yes/no answer before any of the harder work —
//! long-mode entry, per-CPU state, IST stacks — is layered on top. An AP that
//! faults here is silent, so the BSP's poll is bounded by a timeout rather than
//! waiting forever.

/// Offset within the trampoline page of the byte each started AP increments.
///
/// Far enough past the code that the two never collide, and reachable as a
/// 16-bit displacement once `DS` is the trampoline segment.
pub const AP_COUNTER_OFFSET: u64 = 0xF00;

/// The 16-bit real-mode code an AP executes as its first instructions.
///
/// Assembled by hand (there is no assembler in the build for a 16-bit blob):
///
/// ```text
///   8C C8              mov  ax, cs        ; CS = trampoline page >> 4 at entry
///   8E D8              mov  ds, ax        ; DS = CS, so [0xF00] is in this page
///   F0 FE 06 00 0F     lock inc byte [0x0F00]
///   F4                 hlt
///   EB FD              jmp  $-3           ; back to the hlt if ever woken
/// ```
///
/// `lock` because several APs may execute this at once, and the increment must
/// not lose a count. `hlt` in a loop rather than falling through: an AP with
/// nothing to do should idle, and must never run off the end into whatever
/// follows in the page.
pub const AP_TRAMPOLINE_CODE: [u8; 12] = [
    0x8C, 0xC8, // mov ax, cs
    0x8E, 0xD8, // mov ds, ax
    0xF0, 0xFE, 0x06, 0x00, 0x0F, // lock inc byte [0x0F00]
    0xF4, // hlt
    0xEB, 0xFD, // jmp $-3 — back to the hlt if ever woken
];

/// Offset of the `hlt` within [`AP_TRAMPOLINE_CODE`].
pub const AP_TRAMPOLINE_HLT_OFFSET: usize = 9;

/// How long to wait after the `INIT` IPI before the first `SIPI` (SDM: 10 ms).
pub const INIT_SETTLE_NS: u64 = 10_000_000;

/// How long to wait between the two `SIPI`s (SDM: 200 µs).
pub const SIPI_GAP_NS: u64 = 200_000;

/// How long to wait for a started AP to report in before giving up.
pub const AP_START_TIMEOUT_NS: u64 = 100_000_000;

/// Whether `code_len` bytes of trampoline can coexist with the counter byte in
/// one 4 KiB page.
#[must_use]
pub const fn trampoline_fits(code_len: u64) -> bool {
    code_len <= AP_COUNTER_OFFSET && AP_COUNTER_OFFSET < 4096
}

#[cfg(target_os = "uefi")]
pub use hw::start_aps;

#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        AP_COUNTER_OFFSET, AP_START_TIMEOUT_NS, AP_TRAMPOLINE_CODE, INIT_SETTLE_NS, SIPI_GAP_NS,
    };
    use crate::apic::{IA32_X2APIC_ICR, icr_init_assert, icr_startup};

    /// Write the x2APIC Interrupt Command Register, sending an IPI.
    ///
    /// # Safety
    ///
    /// x2APIC mode must be enabled (the ICR is an MSR only then), and `value`
    /// must be a well-formed ICR encoding — a malformed IPI can reset a CPU.
    unsafe fn write_icr(value: u64) {
        let low = value as u32;
        let high = (value >> 32) as u32;
        // SAFETY: caller guarantees x2APIC is on and `value` is well-formed.
        unsafe {
            core::arch::asm!(
                "wrmsr",
                in("ecx") IA32_X2APIC_ICR,
                in("eax") low,
                in("edx") high,
                options(nostack, preserves_flags),
            );
        }
    }

    /// Copy the real-mode trampoline into `page` and zero its counter byte.
    ///
    /// # Safety
    ///
    /// `page` must be a whole, free, identity-mapped 4 KiB page below 1 MiB that
    /// the kernel owns exclusively.
    unsafe fn install_trampoline(page: u64) {
        let base: *mut u8 =
            core::ptr::with_exposed_provenance_mut(usize::try_from(page).unwrap_or(0));
        // SAFETY: the caller guarantees `page` is a writable 4 KiB page the
        // kernel owns; every write below stays inside it.
        unsafe {
            for (i, byte) in AP_TRAMPOLINE_CODE.iter().enumerate() {
                base.add(i).write_volatile(*byte);
            }
            base.add(usize::try_from(AP_COUNTER_OFFSET).unwrap_or(0))
                .write_volatile(0);
        }
    }

    /// Read the trampoline's counter byte — how many APs have reported in.
    ///
    /// # Safety
    ///
    /// `page` must be the installed trampoline page.
    unsafe fn read_ap_count(page: u64) -> u8 {
        let counter: *const u8 = core::ptr::with_exposed_provenance(
            usize::try_from(page.saturating_add(AP_COUNTER_OFFSET)).unwrap_or(0),
        );
        // SAFETY: the caller guarantees the page is live and identity-mapped;
        // volatile because an AP writes it behind the compiler's back.
        unsafe { counter.read_volatile() }
    }

    /// Wake every AP in `apic_ids` except `bsp_id`, and return how many reported
    /// in before the timeout.
    ///
    /// Installs the trampoline, then for each AP sends `INIT` (assert), waits
    /// 10 ms, and sends two `SIPI`s 200 µs apart pointing at the trampoline page
    /// — the sequence SDM Vol. 3 §9.4.4 specifies. Then polls the trampoline's
    /// counter until every AP has reported or [`AP_START_TIMEOUT_NS`] elapses,
    /// so an AP that faults costs a bounded wait rather than a hang.
    ///
    /// Returns `(started, expected)`.
    ///
    /// # Safety
    ///
    /// x2APIC must be enabled, `tsc_hz` must be a real calibrated TSC frequency
    /// (the delays are timing-critical — too short and an AP misses the SIPI),
    /// and `page` must be an exclusively-owned, identity-mapped, free 4 KiB page
    /// below 1 MiB. Sending `INIT` to the BSP's own APIC id would reset it, so
    /// `bsp_id` is excluded.
    pub unsafe fn start_aps(page: u64, apic_ids: &[u32], bsp_id: u32, tsc_hz: u64) -> (u8, u8) {
        // SAFETY: the caller guarantees `page` is an owned, writable low page.
        unsafe { install_trampoline(page) };

        let start_page = u8::try_from(page >> 12).unwrap_or(0);
        let mut expected: u8 = 0;
        for &id in apic_ids {
            if id == bsp_id {
                continue; // INIT to ourselves is a reset
            }
            expected = expected.saturating_add(1);
            // SAFETY: x2APIC is on per the caller; these are the SDM's own
            // INIT-SIPI-SIPI encodings aimed at a non-BSP processor.
            unsafe {
                write_icr(icr_init_assert(id));
                crate::tsc::busy_sleep_ns(tsc_hz, INIT_SETTLE_NS);
                write_icr(icr_startup(id, start_page));
                crate::tsc::busy_sleep_ns(tsc_hz, SIPI_GAP_NS);
                write_icr(icr_startup(id, start_page));
            }
        }
        if expected == 0 {
            return (0, 0);
        }

        // Poll until every AP reports in or the timeout expires.
        let deadline_step = AP_START_TIMEOUT_NS / 100;
        let mut started = 0u8;
        for _ in 0..100 {
            // SAFETY: `page` is the trampoline page installed above.
            started = unsafe { read_ap_count(page) };
            if started >= expected {
                break;
            }
            crate::tsc::busy_sleep_ns(tsc_hz, deadline_step);
        }
        (started, expected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trampoline_encodes_the_documented_instructions() {
        // mov ax, cs / mov ds, ax — the AP must establish addressing itself,
        // since only CS is defined at real-mode entry.
        assert_eq!(&AP_TRAMPOLINE_CODE[0..2], &[0x8C, 0xC8]);
        assert_eq!(&AP_TRAMPOLINE_CODE[2..4], &[0x8E, 0xD8]);
        // lock inc byte [0x0F00] — the displacement must be the counter offset,
        // little-endian, or APs would bump a byte the BSP never reads.
        assert_eq!(AP_TRAMPOLINE_CODE[4], 0xF0, "lock prefix");
        assert_eq!(&AP_TRAMPOLINE_CODE[5..7], &[0xFE, 0x06]);
        let disp = u16::from_le_bytes([AP_TRAMPOLINE_CODE[7], AP_TRAMPOLINE_CODE[8]]);
        assert_eq!(u64::from(disp), AP_COUNTER_OFFSET);
        // hlt, then a jmp back to it.
        assert_eq!(AP_TRAMPOLINE_CODE[AP_TRAMPOLINE_HLT_OFFSET], 0xF4, "hlt");
    }

    #[test]
    fn trampoline_tail_jumps_back_to_the_hlt() {
        // The jmp follows the hlt; its rel8 is relative to the instruction after.
        let jmp_at = AP_TRAMPOLINE_HLT_OFFSET + 1;
        assert_eq!(AP_TRAMPOLINE_CODE[jmp_at], 0xEB, "jmp rel8");
        let rel8 = i64::from(AP_TRAMPOLINE_CODE[jmp_at + 1].cast_signed());
        let target = i64::try_from(jmp_at + 2).unwrap() + rel8;
        assert_eq!(
            target,
            i64::try_from(AP_TRAMPOLINE_HLT_OFFSET).unwrap(),
            "a woken AP must land back on its hlt, not run past the trampoline"
        );
        // The jmp is the last thing in the blob.
        assert_eq!(jmp_at + 2, AP_TRAMPOLINE_CODE.len());
    }

    #[test]
    fn code_and_counter_share_one_page_without_colliding() {
        assert!(trampoline_fits(AP_TRAMPOLINE_CODE.len() as u64));
        assert!(
            !trampoline_fits(AP_COUNTER_OFFSET + 1),
            "would overwrite it"
        );
        assert!(!trampoline_fits(8192), "past the page");
    }

    #[test]
    fn wake_delays_match_the_sdm() {
        // SDM Vol. 3 §9.4.4: 10 ms after INIT, 200 us between the two SIPIs.
        assert_eq!(INIT_SETTLE_NS, 10 * 1_000_000);
        assert_eq!(SIPI_GAP_NS, 200 * 1_000);
        // The poll must outlast the wake sequence itself, or it would give up
        // before a healthy AP could possibly have reported in.
        assert_eq!(
            AP_START_TIMEOUT_NS.max(INIT_SETTLE_NS + SIPI_GAP_NS),
            AP_START_TIMEOUT_NS
        );
    }
}

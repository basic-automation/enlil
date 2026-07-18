//! Calibrated TSC time backend for the enlil kernel (Phase 6.2).
//!
//! Bare-metal enlil needs a monotonic time source with no host OS underneath
//! it. The Time Stamp Counter is the cheapest one, but its frequency must be
//! discovered: on this AMD workstation CPUID leaf 0x15 (Intel's crystal/TSC
//! ratio) is absent, so the kernel calibrates the TSC against the fixed-rate
//! Programmable Interval Timer — the classic method Linux's `pit_calibrate_tsc`
//! uses. The counting arithmetic (PIT ticks for a target delay, TSC delta →
//! frequency) is pure and host-tested; only the port I/O + `rdtsc` that drive
//! the PIT and read the counter are gated to the firmware target.

/// The 8254 PIT input frequency: 1.193182 MHz, fixed by the PC architecture.
pub const PIT_FREQUENCY_HZ: u64 = 1_193_182;

/// The PIT reload count for a delay of `micros` microseconds.
///
/// `count = PIT_FREQUENCY_HZ * micros / 1_000_000`, clamped to the 16-bit
/// counter range (a count of 0 means 65536 on the 8254, so 1..=0xFFFF is the
/// usable span; the result is clamped into it).
#[must_use]
pub fn pit_count_for_micros(micros: u32) -> u16 {
    let count = PIT_FREQUENCY_HZ * u64::from(micros) / 1_000_000;
    // A count of 0 means 65536 on the 8254, so the usable span is 1..=0xFFFF.
    u16::try_from(count.clamp(1, 0xFFFF)).unwrap_or(0xFFFF)
}

/// The delay in nanoseconds a PIT reload `count` produces at [`PIT_FREQUENCY_HZ`].
///
/// The inverse of [`pit_count_for_micros`], carried at nanosecond resolution so
/// the frequency division below keeps its precision.
#[must_use]
pub const fn pit_delay_ns(count: u16) -> u64 {
    // count / PIT_FREQUENCY_HZ seconds, in ns: count * 1e9 / freq.
    (count as u64) * 1_000_000_000 / PIT_FREQUENCY_HZ
}

/// The TSC frequency in hertz from a `tsc_delta` observed over `elapsed_ns`.
///
/// `hz = tsc_delta * 1e9 / elapsed_ns`. Returns 0 for a zero interval (a failed
/// measurement) rather than dividing by zero.
#[must_use]
pub fn tsc_hz_from_delta(tsc_delta: u64, elapsed_ns: u64) -> u64 {
    if elapsed_ns == 0 {
        return 0;
    }
    // tsc_delta up to ~2^34 over a 10 ms window at 4 GHz; * 1e9 fits in u128.
    let hz = u128::from(tsc_delta) * 1_000_000_000 / u128::from(elapsed_ns);
    u64::try_from(hz).unwrap_or(u64::MAX)
}

/// A calibrated hertz value rendered in whole MHz (rounded to nearest).
#[must_use]
pub const fn hz_to_mhz_rounded(hz: u64) -> u64 {
    (hz + 500_000) / 1_000_000
}

/// Nanoseconds elapsed for a `ticks` TSC delta at `tsc_hz`.
///
/// `ns = ticks * 1e9 / tsc_hz`, carried through `u128` so a large delta does
/// not overflow. Returns 0 for an uncalibrated clock (`tsc_hz == 0`) rather
/// than dividing by zero — the monotonic clock the bare-metal kernel reads time
/// from once the TSC is calibrated.
#[must_use]
pub fn ticks_to_ns(ticks: u64, tsc_hz: u64) -> u64 {
    if tsc_hz == 0 {
        return 0;
    }
    let ns = u128::from(ticks) * 1_000_000_000 / u128::from(tsc_hz);
    u64::try_from(ns).unwrap_or(u64::MAX)
}

/// The TSC-tick delta spanning `ns` nanoseconds at `tsc_hz`.
///
/// The inverse of [`ticks_to_ns`] (`ticks = ns * tsc_hz / 1e9`): how many ticks
/// to spin for a `ns` delay. Returns 0 for an uncalibrated clock.
#[must_use]
pub fn ns_to_ticks(ns: u64, tsc_hz: u64) -> u64 {
    if tsc_hz == 0 {
        return 0;
    }
    let ticks = u128::from(ns) * u128::from(tsc_hz) / 1_000_000_000;
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

#[cfg(target_os = "uefi")]
pub use hw::{busy_sleep_ns, calibrate_tsc_hz, read_tsc};

#[cfg(target_os = "uefi")]
mod hw {
    use super::{ns_to_ticks, pit_count_for_micros, pit_delay_ns, tsc_hz_from_delta};

    /// PIT mode/command port.
    const PIT_COMMAND: u16 = 0x43;
    /// PIT channel 2 data port.
    const PIT_CH2_DATA: u16 = 0x42;
    /// Port 0x61: bit 0 gates PIT channel 2, bit 1 drives the speaker, bit 5 is
    /// the channel-2 output (OUT2) status.
    const PORT_61: u16 = 0x61;
    /// Channel 2, access lo+hi byte, mode 0 (interrupt on terminal count).
    const PIT_CH2_MODE0: u8 = 0b1011_0000;
    /// Port-0x61 bit 0 — channel-2 gate enable.
    const P61_CH2_GATE: u8 = 1 << 0;
    /// Port-0x61 bit 1 — speaker data (kept clear so calibration is silent).
    const P61_SPEAKER: u8 = 1 << 1;
    /// Port-0x61 bit 5 — channel-2 output; goes high when the count expires.
    const P61_CH2_OUT: u8 = 1 << 5;
    /// The calibration window in microseconds (10 ms — long enough to swamp
    /// port-I/O overhead, short enough to fit the 16-bit PIT counter).
    const CALIBRATION_MICROS: u32 = 10_000;
    /// Cap on the OUT2 poll so a dead PIT is reported, not an infinite spin.
    const POLL_CAP: u32 = 100_000_000;

    /// Read a byte from `port`.
    ///
    /// # Safety
    ///
    /// `port` must be a valid I/O port for a byte read at ring 0.
    unsafe fn inb(port: u16) -> u8 {
        let value: u8;
        unsafe {
            core::arch::asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    /// Write `value` to `port`.
    ///
    /// # Safety
    ///
    /// `port` must be a valid I/O port for a byte write at ring 0.
    unsafe fn outb(port: u16, value: u8) {
        unsafe {
            core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
        }
    }

    /// Read the TSC, serialized so surrounding loads/stores do not reorder
    /// across it (`lfence` before and after `rdtsc`) — the raw monotonic
    /// counter the kernel's time source reads.
    #[must_use]
    pub fn read_tsc() -> u64 {
        let (lo, hi): (u32, u32);
        // SAFETY: rdtsc is unprivileged; lfence bounds it against reordering.
        unsafe {
            core::arch::asm!(
                "lfence",
                "rdtsc",
                "lfence",
                out("eax") lo,
                out("edx") hi,
                options(nomem, nostack, preserves_flags),
            );
        }
        (u64::from(hi) << 32) | u64::from(lo)
    }

    /// Busy-wait until at least `ns` nanoseconds have elapsed on the calibrated
    /// TSC clock (`tsc_hz` from [`calibrate_tsc_hz`]).
    ///
    /// The bare-metal monotonic sleep: converts `ns` to a TSC-tick delta
    /// ([`ns_to_ticks`]) and spins reading [`read_tsc`] until the deadline. A
    /// spin (not a `hlt`) so it works before the scheduler exists and with
    /// interrupts masked; wrap-safe via `wrapping_sub`. An uncalibrated clock
    /// (`tsc_hz == 0`) returns immediately.
    pub fn busy_sleep_ns(tsc_hz: u64, ns: u64) {
        let ticks = ns_to_ticks(ns, tsc_hz);
        if ticks == 0 {
            return;
        }
        let start = read_tsc();
        while read_tsc().wrapping_sub(start) < ticks {
            core::hint::spin_loop();
        }
    }

    /// Calibrate the TSC frequency in hertz against the PIT, or `None` if the
    /// PIT output never asserted (a dead/absent timer).
    ///
    /// Gates PIT channel 2, programs it for a [`CALIBRATION_MICROS`] one-shot
    /// (mode 0), and reads the TSC immediately before starting and immediately
    /// after OUT2 asserts — the TSC delta over that known interval gives the
    /// frequency. The caller runs with interrupts masked so nothing perturbs
    /// the window.
    #[must_use]
    pub fn calibrate_tsc_hz() -> Option<u64> {
        let count = pit_count_for_micros(CALIBRATION_MICROS);
        // SAFETY: ring 0 after ExitBootServices; these are the architectural PIT
        // ports and port 0x61. Speaker stays off (P61_SPEAKER clear).
        unsafe {
            // Enable the channel-2 gate, speaker off.
            let p61 = (inb(PORT_61) & !P61_SPEAKER) | P61_CH2_GATE;
            outb(PORT_61, p61);
            // Program channel 2: mode 0, then load the count lo/hi. Writing the
            // count with the gate high starts the one-shot.
            outb(PIT_COMMAND, PIT_CH2_MODE0);
            outb(PIT_CH2_DATA, (count & 0xFF) as u8);
            outb(PIT_CH2_DATA, (count >> 8) as u8);

            let start = read_tsc();
            let mut polls = 0u32;
            while inb(PORT_61) & P61_CH2_OUT == 0 {
                polls += 1;
                if polls >= POLL_CAP {
                    return None;
                }
                core::hint::spin_loop();
            }
            let end = read_tsc();

            // Drop the gate again.
            outb(PORT_61, inb(PORT_61) & !P61_CH2_GATE);

            let elapsed_ns = pit_delay_ns(count);
            Some(tsc_hz_from_delta(end.wrapping_sub(start), elapsed_ns))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pit_count_matches_the_target_delay() {
        // 10 ms at 1.193182 MHz ≈ 11931 ticks.
        assert_eq!(pit_count_for_micros(10_000), 11931);
        // 1 ms ≈ 1193 ticks.
        assert_eq!(pit_count_for_micros(1_000), 1193);
    }

    #[test]
    fn pit_count_clamps_to_the_16bit_range() {
        // A very long delay would overflow the 16-bit counter → clamped.
        assert_eq!(pit_count_for_micros(1_000_000), 0xFFFF);
        // A zero delay clamps up to the minimum usable count.
        assert_eq!(pit_count_for_micros(0), 1);
    }

    #[test]
    fn delay_and_count_round_trip() {
        // 11931 ticks back to ~10 ms (within the integer-division rounding).
        let ns = pit_delay_ns(11931);
        assert!((9_990_000..=10_010_000).contains(&ns), "ns = {ns}");
    }

    #[test]
    fn tsc_hz_divides_delta_by_interval() {
        // 40_000_000 ticks over 10 ms → 4.0 GHz.
        assert_eq!(tsc_hz_from_delta(40_000_000, 10_000_000), 4_000_000_000);
        // A zero interval is a failed measurement, not a divide-by-zero.
        assert_eq!(tsc_hz_from_delta(123, 0), 0);
    }

    #[test]
    fn ticks_and_ns_round_trip() {
        let hz = 4_000_000_000; // 4 GHz
        // 4e9 ticks = 1 second = 1e9 ns.
        assert_eq!(ticks_to_ns(4_000_000_000, hz), 1_000_000_000);
        // 10 ms → 40e6 ticks.
        assert_eq!(ns_to_ticks(10_000_000, hz), 40_000_000);
        // Round-trip a 5 ms interval.
        assert_eq!(ticks_to_ns(ns_to_ticks(5_000_000, hz), hz), 5_000_000);
    }

    #[test]
    fn ticks_ns_handle_uncalibrated_clock() {
        // A zero frequency (uncalibrated) is not a divide-by-zero.
        assert_eq!(ticks_to_ns(123, 0), 0);
        assert_eq!(ns_to_ticks(123, 0), 0);
    }

    #[test]
    fn mhz_rounds_to_nearest() {
        assert_eq!(hz_to_mhz_rounded(3_600_000_000), 3600);
        assert_eq!(hz_to_mhz_rounded(3_599_600_000), 3600); // rounds up
        assert_eq!(hz_to_mhz_rounded(2_999_400_000), 2999); // rounds down
    }
}

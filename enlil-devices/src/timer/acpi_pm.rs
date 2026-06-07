//! ACPI Power Management Timer (`PM_TMR`).
//!
//! A free-running counter at the ACPI-fixed frequency of **3.579545 MHz** that an
//! OS reads to calibrate and cross-check its other timekeeping sources (TSC,
//! HPET). The FADT's `PM_TMR_BLK` field tells the guest which I/O port the timer
//! lives at (`enlil_devices::acpi::fadt` advertises `0x608`); without a device
//! answering there the guest reads open-bus `0xFFFF_FFFF` and its time
//! calibration diverges or hangs.
//!
//! **Width:** the timer is 24-bit by default, but the FADT we emit sets the
//! `TMR_VAL_EXT` feature flag, which promises the OS a **32-bit** counter — so
//! this model is a full 32-bit counter (no 24-bit wrap), keeping the hardware and
//! the table we hand the guest in agreement.
//!
//! The counter is read-only to the guest; the host advances it (`advance`) from
//! the run loop / a timer thread, mirroring how `SharedPit`/`SharedHpet` are
//! driven.

use crate::bus::PioDevice;
use std::sync::{Arc, Mutex};

/// ACPI PM timer frequency: the fixed 3.579545 MHz (a third of the NTSC color
/// burst), the rate the OS assumes for every `PM_TMR` reading.
pub const PM_TIMER_FREQ_HZ: u32 = 3_579_545;

/// I/O port of the `PM_TMR` register. Matches the `PM_TMR_BLK` the emitted FADT
/// advertises (`enlil_devices::acpi::fadt`), so the guest reads the timer where
/// the bus decodes it.
pub const PM_TIMER_PORT: u16 = 0x608;

/// The `PM_TMR` register is one 32-bit (four-byte) port.
const PM_TIMER_PORTS: u16 = 4;

/// The ACPI Power Management Timer: a free-running 32-bit up-counter.
#[derive(Debug, Clone, Default)]
pub struct AcpiPmTimer {
    /// The current counter value (all 32 bits significant — see the module docs).
    counter: u32,
}

impl AcpiPmTimer {
    /// A new timer reading zero.
    #[must_use]
    pub const fn new() -> Self {
        Self { counter: 0 }
    }

    /// The current 32-bit counter value the guest reads from `PM_TMR`.
    #[must_use]
    pub const fn read(&self) -> u32 {
        self.counter
    }

    /// Advance the free-running counter by `ticks` PM-timer ticks (each
    /// `1 / 3.579545 MHz ≈ 279 ns`), wrapping at 32 bits as the hardware does.
    pub const fn advance(&mut self, ticks: u32) {
        self.counter = self.counter.wrapping_add(ticks);
    }

    /// Convert a wall-clock duration in nanoseconds to the number of PM-timer
    /// ticks it represents, for a run loop that advances the counter from elapsed
    /// real time. Uses 128-bit intermediate math so the multiply cannot overflow.
    #[must_use]
    pub const fn ns_to_ticks(ns: u64) -> u32 {
        let ticks = (ns as u128 * PM_TIMER_FREQ_HZ as u128) / 1_000_000_000;
        // The counter is 32-bit and advance() wraps, so truncating the tick count
        // to 32 bits is correct (a longer gap simply wraps the counter).
        (ticks & 0xFFFF_FFFF) as u32
    }
}

/// A shared [`AcpiPmTimer`] behind an `Arc<Mutex<…>>`, mirroring `SharedPit`.
///
/// The [`AcpiPmTimerPort`] bus adapter and the run loop's `advance` driver hold
/// independent handles to one counter.
#[derive(Clone, Default)]
pub struct SharedAcpiPmTimer(Arc<Mutex<AcpiPmTimer>>);

impl SharedAcpiPmTimer {
    /// Wrap a fresh timer reading zero.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(AcpiPmTimer::new())))
    }

    /// Run `f` with exclusive access to the counter.
    ///
    /// # Panics
    /// Panics if the timer mutex has been poisoned by a prior panic while held.
    pub fn with<R>(&self, f: impl FnOnce(&mut AcpiPmTimer) -> R) -> R {
        f(&mut self.0.lock().expect("PM timer mutex poisoned"))
    }

    /// Advance the counter by `ticks` (the run loop / timer thread drives this).
    pub fn advance(&self, ticks: u32) {
        self.with(|t| t.advance(ticks));
    }

    /// The current counter value.
    #[must_use]
    pub fn read(&self) -> u32 {
        self.with(|t| t.read())
    }

    /// The `PM_TMR` register as a bus [`PioDevice`].
    #[must_use]
    pub fn port(&self) -> AcpiPmTimerPort {
        AcpiPmTimerPort {
            timer: self.clone(),
        }
    }
}

/// The `PM_TMR` four-byte register ([`PM_TIMER_PORT`]) as a bus [`PioDevice`].
///
/// Read-only: writes are ignored (the OS only ever reads the timer). A read at
/// any byte offset within the four-port window returns the corresponding bytes of
/// the 32-bit counter, so the guest can read it 8/16/32-bit.
pub struct AcpiPmTimerPort {
    timer: SharedAcpiPmTimer,
}

impl PioDevice for AcpiPmTimerPort {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        // Byte-steer within the 32-bit register; the bus takes the low `size`
        // bytes of what we return.
        let offset = u32::from(port - PM_TIMER_PORT);
        self.timer.read() >> (offset * 8)
    }

    fn pio_write(&mut self, _port: u16, _size: u8, _data: u32) {
        // The PM timer is read-only.
    }

    fn port_range(&self) -> (u16, u16) {
        (PM_TIMER_PORT, PM_TIMER_PORT + PM_TIMER_PORTS)
    }
}

#[cfg(test)]
mod tests {
    use super::{AcpiPmTimer, PM_TIMER_FREQ_HZ, PM_TIMER_PORT, SharedAcpiPmTimer};
    use crate::bus::PioDevice;

    #[test]
    fn claims_the_four_byte_pm_tmr_window() {
        let dev = SharedAcpiPmTimer::new().port();
        assert_eq!(dev.port_range(), (0x608, 0x60C));
    }

    #[test]
    fn the_counter_is_free_running_and_wraps_at_32_bits() {
        let mut t = AcpiPmTimer::new();
        assert_eq!(t.read(), 0);
        t.advance(100);
        assert_eq!(t.read(), 100);
        // Advance to just below the 32-bit boundary, then wrap.
        t.advance(u32::MAX - 100);
        assert_eq!(t.read(), u32::MAX);
        t.advance(2);
        assert_eq!(t.read(), 1, "32-bit counter wraps");
    }

    #[test]
    fn ns_convert_uses_the_acpi_frequency() {
        // One second is exactly the frequency's worth of ticks.
        assert_eq!(AcpiPmTimer::ns_to_ticks(1_000_000_000), PM_TIMER_FREQ_HZ);
        // ~279 ns is one tick.
        assert_eq!(AcpiPmTimer::ns_to_ticks(279), 0);
        assert_eq!(AcpiPmTimer::ns_to_ticks(280), 1);
    }

    #[test]
    fn reads_the_32bit_counter_through_the_port_at_any_offset() {
        let shared = SharedAcpiPmTimer::new();
        shared.advance(0x1234_5678);
        let mut dev = shared.port();

        // 32-bit read at the base returns the whole counter.
        assert_eq!(dev.pio_read(PM_TIMER_PORT, 4), 0x1234_5678);
        // A byte read at offset 1 returns the next byte in the low bits (the bus
        // then keeps just that byte).
        assert_eq!(dev.pio_read(PM_TIMER_PORT + 1, 1) & 0xFF, 0x56);
        assert_eq!(dev.pio_read(PM_TIMER_PORT + 3, 1) & 0xFF, 0x12);
    }

    #[test]
    fn writes_are_ignored_the_timer_is_read_only() {
        let shared = SharedAcpiPmTimer::new();
        shared.advance(42);
        let mut dev = shared.port();
        dev.pio_write(PM_TIMER_PORT, 4, 0xFFFF_FFFF);
        assert_eq!(
            shared.read(),
            42,
            "guest writes must not change the counter"
        );
    }
}

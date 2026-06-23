//! HPET (High Precision Event Timer) emulation.
//!
//! Provides a virtual HPET device with configurable timers,
//! periodic and one-shot modes, and interrupt routing.

use crate::bus::MmioDevice;
use std::sync::{Arc, Mutex};

/// Number of HPET timers.
const NUM_TIMERS: usize = 3;

/// Guest-physical base of the HPET register block.
///
/// Matches the address the ACPI HPET table advertises
/// ([`crate::acpi::hpet::HPET_BASE_ADDRESS`]), so a guest that discovers the HPET
/// from ACPI finds its registers where the bus decodes them. Cross-checked by a
/// unit test.
pub const HPET_MMIO_BASE: u64 = 0xFED0_0000;

/// Size of the HPET memory-mapped register block: the spec-mandated 1 KiB.
pub const HPET_MMIO_SIZE: u64 = 0x400;

/// Nanoseconds per HPET main-counter tick (the 10 MHz / 100 ns period this model
/// reports in its capability register). A run loop converts an elapsed-time delta
/// to counter ticks by dividing by this.
pub const HPET_TICK_NS: u64 = HPET_CLK_PERIOD_FS / 1_000_000;

/// HPET capability/ID register value.
/// Bits 31:16 = clock period in femtoseconds.
/// Bits 15:8 = number of timers - 1.
/// Bits 7:0 = revision ID.
/// HPET clock period in femtoseconds (10 MHz → 100 ns → `100_000_000` fs).
const HPET_CLK_PERIOD_FS: u64 = 100_000_000;

/// Supported IRQ routing mask for timers.
const TIMER_ROUTE_CAP: u32 = 0x000F_0000; // IRQs 16-19

/// HPET PCI vendor ID reported in the capability register (Intel). Mirrors the
/// ACPI HPET table's Event Timer Block ID so the table and the MMIO register a
/// guest reads agree (cross-checked by a test).
const HPET_VENDOR_ID: u64 = 0x8086;
/// `COUNT_SIZE_CAP` (bit 13): the main counter is 64-bit — which this model's
/// `counter: u64` and the [`HpetMmio`] 64-bit accessors actually implement.
const HPET_COUNT_SIZE_CAP: u64 = 1 << 13;
/// `LEG_RT_CAP` (bit 15): legacy-replacement routing is supported (config bit 1).
const HPET_LEG_RT_CAP: u64 = 1 << 15;

/// HPET capability register: revision 1, 3 timers, 64-bit counter, legacy-
/// replacement capable, Intel vendor. The low 32 bits mirror the ACPI HPET
/// table's Event Timer Block ID; the high 32 carry the clock period.
const HPET_CAP_VALUE: u64 = (HPET_CLK_PERIOD_FS << 32)
    | (HPET_VENDOR_ID << 16)
    | HPET_LEG_RT_CAP
    | HPET_COUNT_SIZE_CAP
    | ((NUM_TIMERS as u64 - 1) << 8)
    | 0x01;

/// Individual HPET timer state.
#[derive(Debug, Clone)]
pub struct HpetTimer {
    /// Timer configuration and capabilities register.
    pub config: u64,
    /// Comparator value.
    pub comparator: u64,
    /// FSB interrupt route register.
    pub fsb_route: u64,
    /// Period (the increment added to `comparator` after each fire) in periodic
    /// mode. Programmed via a comparator write while `TN_VAL_SET_CNF` is set, or
    /// any comparator write in periodic mode (per the IA-PC HPET spec §2.3.9.2.2).
    period: u64,
    /// Timer index (0-based).
    index: usize,
}

impl HpetTimer {
    const fn new(index: usize) -> Self {
        // Set the interrupt routing capability in bits 32-63.
        let cap = (TIMER_ROUTE_CAP as u64) << 32;
        Self {
            // Capabilities in upper 32 bits, timer starts masked (bit 14 clear = no interrupt).
            config: cap,
            comparator: 0,
            fsb_route: 0,
            period: 0,
            index,
        }
    }

    /// Whether this timer is in periodic mode.
    const fn is_periodic(&self) -> bool {
        self.config & (1 << 3) != 0
    }

    /// Whether interrupt generation is enabled.
    const fn interrupt_enabled(&self) -> bool {
        self.config & (1 << 2) != 0
    }

    /// Whether this timer uses level-triggered interrupts.
    const fn is_level_triggered(&self) -> bool {
        self.config & (1 << 1) != 0
    }

    /// Returns the configured IRQ routing for this timer.
    const fn irq_route(&self) -> u8 {
        ((self.config >> 9) & 0x1F) as u8
    }
}

/// HPET device state.
#[derive(Debug, Clone)]
pub struct Hpet {
    /// General configuration register.
    config: u64,
    /// Main counter value.
    counter: u64,
    /// Timer states.
    timers: Vec<HpetTimer>,
    /// General interrupt status register.
    interrupt_status: u64,
}

impl Default for Hpet {
    fn default() -> Self {
        Self::new()
    }
}

impl Hpet {
    /// Create a new HPET with default configuration.
    #[must_use]
    pub fn new() -> Self {
        let timers = (0..NUM_TIMERS).map(HpetTimer::new).collect();
        Self {
            config: 0,
            counter: 0,
            timers,
            interrupt_status: 0,
        }
    }

    /// Whether the HPET main counter is enabled (bit 0 of config).
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.config & 1 != 0
    }

    /// Whether legacy replacement routing is enabled (bit 1 of config).
    #[must_use]
    pub const fn legacy_routing(&self) -> bool {
        self.config & 2 != 0
    }

    /// Returns the current main counter value.
    #[must_use]
    pub const fn counter(&self) -> u64 {
        self.counter
    }

    /// Returns a reference to a timer by index.
    #[must_use]
    pub fn timer(&self, index: usize) -> Option<&HpetTimer> {
        self.timers.get(index)
    }

    /// Returns the number of timers.
    #[must_use]
    pub const fn num_timers(&self) -> usize {
        self.timers.len()
    }

    /// Read an HPET register at the given offset.
    #[must_use]
    pub fn read(&self, offset: u64) -> u64 {
        match offset {
            // General Capabilities and ID
            0x000 => HPET_CAP_VALUE,
            // General Configuration
            0x010 => self.config,
            // General Interrupt Status
            0x020 => self.interrupt_status,
            // Main Counter Value
            0x0F0 => self.counter,
            // Timer N registers (0x100 + N*0x20 + reg_offset)
            0x100..=0x1FF => {
                let timer_idx = ((offset - 0x100) / 0x20) as usize;
                let reg_offset = (offset - 0x100) % 0x20;
                self.timers
                    .get(timer_idx)
                    .map_or(0, |timer| match reg_offset {
                        0x00 => timer.config,
                        0x08 => timer.comparator,
                        0x10 => timer.fsb_route,
                        _ => 0,
                    })
            }
            _ => 0,
        }
    }

    /// Write an HPET register at the given offset.
    pub fn write(&mut self, offset: u64, value: u64) {
        match offset {
            // General Configuration
            0x010 => {
                let was_enabled = self.is_enabled();
                self.config = value & 0x3; // Only bits 0-1 are writable
                if !was_enabled && self.is_enabled() {
                    // Counter just enabled — timers start running
                    log::debug!("HPET: counter enabled");
                }
            }
            // General Interrupt Status (write-1-to-clear)
            0x020 => {
                self.interrupt_status &= !value;
            }
            // Main Counter Value (writable only when counter is disabled)
            0x0F0 if !self.is_enabled() => {
                self.counter = value;
            }
            // Timer N registers
            0x100..=0x1FF => {
                let timer_idx = ((offset - 0x100) / 0x20) as usize;
                let reg_offset = (offset - 0x100) % 0x20;
                if let Some(timer) = self.timers.get_mut(timer_idx) {
                    match reg_offset {
                        0x00 => {
                            // Timer config — preserve read-only bits
                            let read_only_mask: u64 = 0xFFFF_FFFF_0000_0000 | (1 << 4) | (1 << 5);
                            let writable_mask = !read_only_mask;
                            timer.config =
                                (timer.config & read_only_mask) | (value & writable_mask);
                        }
                        0x08 => {
                            // IA-PC HPET §2.3.9.2.2: the comparator (next-fire)
                            // is written for a one-shot timer, or for a periodic
                            // timer while TN_VAL_SET_CNF (bit 6) is set; a
                            // periodic timer always (re)programs its period from
                            // the same write so software can change the interval
                            // without disturbing the current comparator. The
                            // value-set bit auto-clears after the write.
                            if !timer.is_periodic() || timer.config & (1 << 6) != 0 {
                                timer.comparator = value;
                            }
                            if timer.is_periodic() {
                                timer.period = value;
                            }
                            timer.config &= !(1 << 6);
                        }
                        0x10 => timer.fsb_route = value,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// Advance the HPET counter by the given number of ticks.
    ///
    /// Returns a vector of (`timer_index`, `irq_number`) for timers that fired.
    pub fn tick(&mut self, ticks: u64) -> Vec<(usize, u8)> {
        if !self.is_enabled() {
            return Vec::new();
        }

        let old_counter = self.counter;
        self.counter = self.counter.wrapping_add(ticks);
        let mut fired = Vec::new();

        for timer in &mut self.timers {
            if !timer.interrupt_enabled() {
                continue;
            }

            let did_fire = if timer.is_periodic() {
                // Periodic: fire when the counter crosses the comparator, then
                // advance the comparator by the programmed period.
                if old_counter < timer.comparator && self.counter >= timer.comparator {
                    timer.comparator = timer.comparator.wrapping_add(timer.period);
                    true
                } else {
                    false
                }
            } else {
                // One-shot: fire when counter reaches comparator
                old_counter < timer.comparator && self.counter >= timer.comparator
            };

            if did_fire {
                let irq = timer.irq_route();
                if timer.is_level_triggered() {
                    self.interrupt_status |= 1 << timer.index;
                }
                fired.push((timer.index, irq));
            }
        }

        fired
    }

    /// Clear interrupt status for a given timer.
    pub const fn clear_interrupt(&mut self, timer_idx: usize) {
        if timer_idx < NUM_TIMERS {
            self.interrupt_status &= !(1 << timer_idx);
        }
    }
}

/// A shared [`Hpet`] behind an `Arc<Mutex<…>>`, mirroring `SharedPit`/`SharedRtc`.
///
/// The [`HpetMmio`] bus adapter and the run loop's `tick` driver hold independent
/// handles to one device, so the guest programs the timers through MMIO while a
/// timer thread advances the main counter.
#[derive(Clone)]
pub struct SharedHpet(Arc<Mutex<Hpet>>);

impl Default for SharedHpet {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedHpet {
    /// Wrap a fresh HPET (counter disabled, all timers masked).
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Hpet::new())))
    }

    /// Run `f` with exclusive access — to program a timer, read state, or attach
    /// the run loop.
    ///
    /// # Panics
    /// Panics if the HPET mutex has been poisoned by a prior panic while held.
    pub fn with<R>(&self, f: impl FnOnce(&mut Hpet) -> R) -> R {
        f(&mut self.0.lock().expect("HPET mutex poisoned"))
    }

    /// Advance the main counter by `ticks`, returning the `(timer, irq)` pairs of
    /// any timers that fired (for the run loop to deliver).
    #[must_use]
    pub fn tick(&self, ticks: u64) -> Vec<(usize, u8)> {
        self.with(|h| h.tick(ticks))
    }

    /// The HPET register block as a bus [`MmioDevice`] at [`HPET_MMIO_BASE`].
    #[must_use]
    pub fn mmio(&self) -> HpetMmio {
        HpetMmio { hpet: self.clone() }
    }
}

/// The HPET's 1 KiB register block ([`HPET_MMIO_BASE`]) as a bus [`MmioDevice`].
///
/// The [`Hpet`] model decodes only 8-byte-aligned register offsets and returns
/// the full 64-bit register; this adapter bridges that to the guest's 32-bit
/// *and* 64-bit MMIO accesses. A 32-bit read of the high dword (offset `… + 4`)
/// returns the upper half of the aligned register — important because a guest
/// reads the HPET capability's femtosecond clock period and a 64-bit counter as
/// two 32-bit halves. Sub-register writes are read-modify-write so a 32-bit store
/// to one half preserves the other (the model's read-only masks still apply).
pub struct HpetMmio {
    hpet: SharedHpet,
}

impl MmioDevice for HpetMmio {
    fn mmio_read(&mut self, offset: u64, _size: u8) -> u64 {
        let aligned = offset & !0x7;
        let full = self.hpet.with(|h| h.read(aligned));
        // A 4-byte access at the odd dword wants the high half; the bus then
        // takes the low `size` bytes of what we return.
        if offset & 0x4 != 0 { full >> 32 } else { full }
    }

    fn mmio_write(&mut self, offset: u64, size: u8, data: u64) {
        let aligned = offset & !0x7;
        if size >= 8 {
            // A full 64-bit store goes straight through.
            self.hpet.with(|h| h.write(aligned, data));
            return;
        }
        // Splice a 32-bit (or narrower) store into the right half of the 64-bit
        // register, preserving the other half.
        let dword = data & 0xFFFF_FFFF;
        self.hpet.with(|h| {
            let current = h.read(aligned);
            let merged = if offset & 0x4 != 0 {
                (current & 0xFFFF_FFFF) | (dword << 32)
            } else {
                (current & 0xFFFF_FFFF_0000_0000) | dword
            };
            h.write(aligned, merged);
        });
    }

    fn mmio_range(&self) -> (u64, u64) {
        (HPET_MMIO_BASE, HPET_MMIO_BASE + HPET_MMIO_SIZE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hpet_default_state() {
        let hpet = Hpet::new();
        assert!(!hpet.is_enabled());
        assert!(!hpet.legacy_routing());
        assert_eq!(hpet.counter(), 0);
        assert_eq!(hpet.num_timers(), NUM_TIMERS);
    }

    #[test]
    fn hpet_capabilities() {
        let hpet = Hpet::new();
        let cap = hpet.read(0x000);
        // Check clock period
        assert_eq!(cap >> 32, HPET_CLK_PERIOD_FS);
        // Check number of timers
        assert_eq!((cap >> 8) & 0x1F, (NUM_TIMERS as u64) - 1);
        // Check revision
        assert_eq!(cap & 0xFF, 1);
    }

    #[test]
    fn hpet_enable_disable() {
        let mut hpet = Hpet::new();
        hpet.write(0x010, 1);
        assert!(hpet.is_enabled());
        hpet.write(0x010, 0);
        assert!(!hpet.is_enabled());
    }

    #[test]
    fn hpet_counter_writable_when_disabled() {
        let mut hpet = Hpet::new();
        hpet.write(0x0F0, 42);
        assert_eq!(hpet.counter(), 42);
    }

    #[test]
    fn hpet_counter_not_writable_when_enabled() {
        let mut hpet = Hpet::new();
        hpet.write(0x010, 1); // Enable
        hpet.write(0x0F0, 42);
        assert_eq!(hpet.counter(), 0); // Should not change
    }

    #[test]
    fn hpet_timer_config_write() {
        let mut hpet = Hpet::new();
        // Enable interrupt for timer 0 (bit 2)
        hpet.write(0x100, 0x04);
        let timer = hpet.timer(0).unwrap();
        assert!(timer.interrupt_enabled());
    }

    #[test]
    fn hpet_one_shot_fires() {
        let mut hpet = Hpet::new();
        // Configure timer 0: enable interrupt (bit 2), route to IRQ 0
        hpet.write(0x100, 0x04);
        // Set comparator to 100
        hpet.write(0x108, 100);
        // Enable counter
        hpet.write(0x010, 1);
        // Tick past comparator
        let fired = hpet.tick(150);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, 0); // timer index
    }

    #[test]
    fn hpet_periodic_fires_each_period_with_setval_semantics() {
        let mut hpet = Hpet::new();
        // Timer 0: interrupt enable (bit 2), periodic (bit 3), value-set (bit 6).
        hpet.write(0x100, (1 << 2) | (1 << 3) | (1 << 6));
        // First comparator write while TN_VAL_SET is set: programs both the
        // first-fire comparator (100) and the period (100); the bit auto-clears.
        hpet.write(0x108, 100);
        assert_eq!(
            hpet.timer(0).unwrap().config & (1 << 6),
            0,
            "TN_VAL_SET_CNF auto-clears after the comparator write"
        );
        hpet.write(0x010, 1); // enable the main counter

        // Crosses 100 -> fires; comparator advances by the period to 200.
        assert_eq!(hpet.tick(150).len(), 1);
        assert_eq!(hpet.timer(0).unwrap().comparator, 200);
        // Crosses 200 (counter 250) -> fires; comparator -> 300.
        assert_eq!(hpet.tick(100).len(), 1);
        // No crossing yet (counter 290 < 300).
        assert_eq!(hpet.tick(40).len(), 0);
        // Crosses 300 (counter 310) -> fires; comparator -> 400.
        assert_eq!(hpet.tick(20).len(), 1);
        assert_eq!(hpet.timer(0).unwrap().comparator, 400);

        // A comparator write WITHOUT setting TN_VAL_SET changes only the period,
        // not the pending comparator.
        hpet.write(0x108, 50);
        assert_eq!(
            hpet.timer(0).unwrap().comparator,
            400,
            "comparator unchanged when TN_VAL_SET is clear"
        );
        // Counter is 310; cross 400 -> fires; comparator advances by the NEW
        // period (50) to 450.
        assert_eq!(hpet.tick(100).len(), 1);
        assert_eq!(hpet.timer(0).unwrap().comparator, 450);
    }

    #[test]
    fn hpet_tick_no_fire_when_disabled() {
        let mut hpet = Hpet::new();
        let fired = hpet.tick(100);
        assert!(fired.is_empty());
    }

    #[test]
    fn hpet_interrupt_status_clear() {
        let mut hpet = Hpet::new();
        // Configure timer 0: level-triggered (bit 1) + interrupt enable (bit 2)
        hpet.write(0x100, 0x06);
        hpet.write(0x108, 50);
        hpet.write(0x010, 1);
        let _ = hpet.tick(100);
        // Interrupt status should be set
        assert_ne!(hpet.read(0x020), 0);
        // Clear it
        hpet.clear_interrupt(0);
        assert_eq!(hpet.read(0x020), 0);
    }

    #[test]
    fn capability_register_matches_the_acpi_hpet_table_block_id() {
        // The capability register a guest reads from MMIO (low 32 bits) must agree
        // with the Event Timer Block ID the ACPI HPET table advertises — rev,
        // comparator count, 64-bit counter, legacy-replacement, vendor — or the OS
        // is told about a different HPET than the one at the registers.
        let cap_low = u32::try_from(HPET_CAP_VALUE & 0xFFFF_FFFF).unwrap();
        let table = crate::acpi::hpet::HpetBuilder::new().build();
        let block_id = u32::from_le_bytes(table[36..40].try_into().unwrap());
        assert_eq!(cap_low, block_id);
    }

    #[test]
    fn mmio_base_matches_the_acpi_hpet_table() {
        // The address the bus decodes must equal the one the ACPI HPET table
        // tells the guest about, or the guest looks in the wrong place.
        assert_eq!(HPET_MMIO_BASE, crate::acpi::hpet::HPET_BASE_ADDRESS);
    }

    #[test]
    fn mmio_adapter_claims_the_1kib_block_at_the_hpet_base() {
        let dev = SharedHpet::new().mmio();
        assert_eq!(dev.mmio_range(), (0xFED0_0000, 0xFED0_0400));
    }

    #[test]
    fn mmio_reads_the_capability_period_as_two_32bit_halves() {
        let mut dev = SharedHpet::new().mmio();
        // A 64-bit read of caps at offset 0 returns the whole register.
        assert_eq!(dev.mmio_read(0x000, 8), HPET_CAP_VALUE);
        // A 32-bit read of the high dword (offset 4) returns the femtosecond
        // clock period — the bus then keeps its low 4 bytes.
        assert_eq!(dev.mmio_read(0x004, 4), HPET_CLK_PERIOD_FS);
        // The low dword (offset 0) carries the revision/timer-count fields.
        assert_eq!(
            dev.mmio_read(0x000, 4) & 0xFFFF_FFFF,
            HPET_CAP_VALUE & 0xFFFF_FFFF
        );
    }

    #[test]
    fn mmio_counter_survives_two_32bit_write_halves() {
        let dev = SharedHpet::new();
        let mut mmio = dev.mmio();
        // Counter is writable only while disabled (config bit 0 clear at reset).
        // Write the low and high dwords of the 64-bit counter separately; each
        // partial store must preserve the other half.
        mmio.mmio_write(0x0F0, 4, 0xDEAD_BEEF);
        mmio.mmio_write(0x0F4, 4, 0x0000_1234);
        assert_eq!(dev.with(|h| h.counter()), 0x0000_1234_DEAD_BEEF);
        // And a 64-bit read brings it back whole.
        assert_eq!(mmio.mmio_read(0x0F0, 8), 0x0000_1234_DEAD_BEEF);
    }

    #[test]
    fn shared_hpet_ticks_through_the_handle() {
        let dev = SharedHpet::new();
        let mut mmio = dev.mmio();
        // Program timer 0 (enable interrupt) + comparator + enable counter via
        // the MMIO adapter, then tick from the shared handle.
        mmio.mmio_write(0x100, 4, 0x04);
        mmio.mmio_write(0x108, 8, 100);
        mmio.mmio_write(0x010, 4, 1);
        let fired = dev.tick(150);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].0, 0);
    }
}

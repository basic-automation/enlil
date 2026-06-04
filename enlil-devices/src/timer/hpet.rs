//! HPET (High Precision Event Timer) emulation.
//!
//! Provides a virtual HPET device with configurable timers,
//! periodic and one-shot modes, and interrupt routing.

/// Number of HPET timers.
const NUM_TIMERS: usize = 3;

/// HPET capability/ID register value.
/// Bits 31:16 = clock period in femtoseconds.
/// Bits 15:8 = number of timers - 1.
/// Bits 7:0 = revision ID.
/// HPET clock period in femtoseconds (10 MHz → 100 ns → `100_000_000` fs).
const HPET_CLK_PERIOD_FS: u64 = 100_000_000;

/// Supported IRQ routing mask for timers.
const TIMER_ROUTE_CAP: u32 = 0x000F_0000; // IRQs 16-19

/// HPET capability register: revision 1, 3 timers, 64-bit counter, legacy capable.
const HPET_CAP_VALUE: u64 = (HPET_CLK_PERIOD_FS << 32) | ((NUM_TIMERS as u64 - 1) << 8) | 0x01;

/// Individual HPET timer state.
#[derive(Debug, Clone)]
pub struct HpetTimer {
    /// Timer configuration and capabilities register.
    pub config: u64,
    /// Comparator value.
    pub comparator: u64,
    /// FSB interrupt route register.
    pub fsb_route: u64,
    /// Accumulated value for periodic mode.
    accumulator: u64,
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
            accumulator: 0,
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
                            timer.comparator = value;
                            if timer.is_periodic() {
                                timer.accumulator = value;
                            }
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
                // Periodic: check if counter crossed accumulator
                if old_counter < timer.accumulator && self.counter >= timer.accumulator {
                    timer.accumulator = timer.accumulator.wrapping_add(timer.comparator);
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
}

//! High Precision Event Timer (HPET) emulation.
//!
//! The HPET provides a main counter and up to 8 comparator timers.
//! It is memory-mapped at a configurable base address (typically 0xFED00000).
//!
//! Register layout (per HPET specification):
//!   0x000 — General Capabilities and ID
//!   0x010 — General Configuration
//!   0x020 — General Interrupt Status
//!   0x0F0 — Main Counter Value
//!   0x100 + 0x20*N — Timer N Config and Capability
//!   0x108 + 0x20*N — Timer N Comparator Value

/// HPET clock period in femtoseconds (10 MHz → 100 ns → 100_000_000 fs).
pub const HPET_CLK_PERIOD_FS: u64 = 100_000_000;

/// Number of timers supported by our HPET implementation.
pub const HPET_NUM_TIMERS: usize = 3;

/// HPET main counter frequency in Hz (10 MHz).
pub const HPET_FREQUENCY_HZ: u64 = 10_000_000;

/// Nanoseconds per HPET tick.
const NS_PER_TICK: u64 = 100; // 10 MHz → 100 ns

/// Timer routing capability (IRQs 0-23 allowed).
const TIMER_ROUTE_CAP: u32 = 0x00FF_FFF0;

/// Individual HPET timer (comparator).
#[derive(Debug, Clone)]
pub struct HpetTimer {
    /// Timer configuration and capability register.
    pub config: u64,
    /// Comparator value.
    pub comparator: u64,
    /// FSB interrupt route register.
    pub fsb_route: u64,
    /// Accumulated period for periodic timers.
    pub period: u64,
    /// Whether this timer has fired and the interrupt is pending.
    pub irq_pending: bool,
}

impl HpetTimer {
    fn new(index: usize) -> Self {
        // Set the interrupt routing capability in bits 32-63.
        let cap = (TIMER_ROUTE_CAP as u64) << 32;
        // Timer 0 and 1 support periodic mode (bit 4).
        let periodic_cap = if index < 2 { 1 << 4 } else { 0 };
        Self {
            config: cap | periodic_cap,
            comparator: 0,
            fsb_route: 0,
            period: 0,
            irq_pending: false,
        }
    }

    /// Whether this timer is in periodic mode.
    fn is_periodic(&self) -> bool {
        self.config & (1 << 3) != 0
    }

    /// Whether this timer's interrupt is enabled.
    fn interrupt_enabled(&self) -> bool {
        self.config & (1 << 2) != 0
    }

    /// Whether level-triggered interrupts are used.
    fn is_level_triggered(&self) -> bool {
        self.config & (1 << 1) != 0
    }

    /// Get the IRQ routing for this timer.
    fn irq_route(&self) -> u8 {
        ((self.config >> 9) & 0x1F) as u8
    }
}

/// High Precision Event Timer.
pub struct Hpet {
    /// General capabilities and ID register.
    capabilities: u64,
    /// General configuration register.
    config: u64,
    /// General interrupt status register.
    interrupt_status: u64,
    /// Main counter value.
    counter: u64,
    /// Individual timers.
    timers: Vec<HpetTimer>,
    /// Nanosecond accumulator for sub-tick precision.
    accumulator_ns: u64,
}

impl Hpet {
    /// Create a new HPET with default configuration.
    pub fn new() -> Self {
        let num_timers = HPET_NUM_TIMERS;
        // Capabilities register:
        //   bits 31:16 — vendor ID (0x8086 for "Intel-like")
        //   bits 15:13 — COUNTER_CLK_PERIOD in upper 32 bits
        //   bits 12:8  — number of timers minus 1
        //   bit 0      — revision (1)
        let cap_low = (((num_timers - 1) as u64) << 8) | 0x01;
        let cap_high = HPET_CLK_PERIOD_FS << 32;
        let capabilities = cap_high | (0x8086u64 << 16) | cap_low;

        let timers = (0..num_timers).map(HpetTimer::new).collect();

        Self {
            capabilities,
            config: 0,
            interrupt_status: 0,
            counter: 0,
            timers,
            accumulator_ns: 0,
        }
    }

    /// Whether the main counter is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config & 1 != 0
    }

    /// Whether legacy replacement routing is enabled.
    pub fn legacy_routing(&self) -> bool {
        self.config & 2 != 0
    }

    /// Read the main counter value.
    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// Get a reference to a timer.
    pub fn timer(&self, index: usize) -> Option<&HpetTimer> {
        self.timers.get(index)
    }

    /// Number of timers.
    pub fn num_timers(&self) -> usize {
        self.timers.len()
    }

    /// Read an HPET register by offset.
    pub fn read(&self, offset: u64) -> u64 {
        match offset {
            0x000 => self.capabilities,
            0x010 => self.config,
            0x020 => self.interrupt_status,
            0x0F0 => self.counter,
            _ => {
                // Timer registers: base 0x100, stride 0x20
                if offset >= 0x100 {
                    let timer_offset = offset - 0x100;
                    let timer_idx = (timer_offset / 0x20) as usize;
                    let reg_offset = timer_offset % 0x20;
                    if let Some(timer) = self.timers.get(timer_idx) {
                        match reg_offset {
                            0x00 => timer.config,
                            0x08 => timer.comparator,
                            0x10 => timer.fsb_route,
                            _ => 0,
                        }
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
        }
    }

    /// Write an HPET register by offset.
    pub fn write(&mut self, offset: u64, value: u64) {
        match offset {
            0x010 => {
                let was_enabled = self.is_enabled();
                self.config = value & 0x3; // Only bits 0-1 are writable
                if !was_enabled && self.is_enabled() {
                    log::debug!("HPET: main counter enabled");
                }
            }
            0x020 => {
                // Writing 1 clears the corresponding interrupt status bit.
                self.interrupt_status &= !value;
            }
            0x0F0 => {
                // Main counter is writable only when counter is halted.
                if !self.is_enabled() {
                    self.counter = value;
                }
            }
            _ => {
                if offset >= 0x100 {
                    let timer_offset = offset - 0x100;
                    let timer_idx = (timer_offset / 0x20) as usize;
                    let reg_offset = timer_offset % 0x20;
                    if let Some(timer) = self.timers.get_mut(timer_idx) {
                        match reg_offset {
                            0x00 => {
                                // Preserve read-only capability bits (32-63 and bit 4/5).
                                let ro_mask: u64 = 0xFFFF_FFFF_0000_0000 | (1 << 4) | (1 << 5);
                                let rw_mask = !ro_mask;
                                timer.config = (timer.config & ro_mask) | (value & rw_mask);

                                // If bit 6 (VAL_SET_CNF) is set, next comparator write
                                // sets the accumulator for periodic mode.
                                if value & (1 << 6) != 0 {
                                    timer.config &= !(1 << 6); // auto-clear
                                }
                            }
                            0x08 => {
                                timer.comparator = value;
                                if timer.is_periodic() {
                                    timer.period = value;
                                }
                            }
                            0x10 => {
                                timer.fsb_route = value;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    /// Advance the HPET by `ns` nanoseconds.
    ///
    /// Returns a vector of (timer_index, irq_number) for timers that fired.
    pub fn tick(&mut self, ns: u64) -> Vec<(usize, u8)> {
        if !self.is_enabled() {
            return Vec::new();
        }

        self.accumulator_ns += ns;
        let ticks = self.accumulator_ns / NS_PER_TICK;
        self.accumulator_ns %= NS_PER_TICK;

        if ticks == 0 {
            return Vec::new();
        }

        let old_counter = self.counter;
        self.counter = self.counter.wrapping_add(ticks);

        let mut fired = Vec::new();

        // Hoist these before the mutable borrow of self.timers.
        let legacy = self.legacy_routing();
        let interrupt_status = &mut self.interrupt_status;

        for (i, timer) in self.timers.iter_mut().enumerate() {
            if !timer.interrupt_enabled() {
                continue;
            }

            let comparator = timer.comparator;

            // Check if the counter crossed the comparator value.
            let crossed = if old_counter <= self.counter {
                // No wrap-around.
                old_counter < comparator && comparator <= self.counter
            } else {
                // Counter wrapped around.
                old_counter < comparator || comparator <= self.counter
            };

            if crossed {
                let irq = if legacy {
                    match i {
                        0 => 0,  // Timer 0 → IRQ 0 (replaces PIT)
                        1 => 8,  // Timer 1 → IRQ 8 (replaces RTC)
                        _ => timer.irq_route(),
                    }
                } else {
                    timer.irq_route()
                };

                if timer.is_level_triggered() {
                    *interrupt_status |= 1 << i;
                    timer.irq_pending = true;
                }

                fired.push((i, irq));

                if timer.is_periodic() && timer.period > 0 {
                    timer.comparator = timer.comparator.wrapping_add(timer.period);
                }
            }
        }

        fired
    }

    /// Reset the HPET to power-on state.
    pub fn reset(&mut self) {
        self.config = 0;
        self.interrupt_status = 0;
        self.counter = 0;
        self.accumulator_ns = 0;
        for (i, timer) in self.timers.iter_mut().enumerate() {
            *timer = HpetTimer::new(i);
        }
    }
}

impl Default for Hpet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hpet_creation() {
        let hpet = Hpet::new();
        assert!(!hpet.is_enabled());
        assert_eq!(hpet.counter(), 0);
        assert_eq!(hpet.num_timers(), HPET_NUM_TIMERS);
    }

    #[test]
    fn test_hpet_enable_disable() {
        let mut hpet = Hpet::new();
        hpet.write(0x010, 1); // Enable
        assert!(hpet.is_enabled());
        hpet.write(0x010, 0); // Disable
        assert!(!hpet.is_enabled());
    }

    #[test]
    fn test_counter_advances_when_enabled() {
        let mut hpet = Hpet::new();
        hpet.write(0x010, 1); // Enable
        let fired = hpet.tick(1000); // 1000 ns = 10 ticks
        assert_eq!(hpet.counter(), 10);
        // No timers configured, so nothing fires.
        assert!(fired.is_empty());
    }

    #[test]
    fn test_counter_halted_when_disabled() {
        let mut hpet = Hpet::new();
        hpet.tick(1000);
        assert_eq!(hpet.counter(), 0);
    }

    #[test]
    fn test_counter_writable_when_halted() {
        let mut hpet = Hpet::new();
        hpet.write(0x0F0, 42);
        assert_eq!(hpet.counter(), 42);
    }

    #[test]
    fn test_counter_not_writable_when_running() {
        let mut hpet = Hpet::new();
        hpet.write(0x010, 1); // Enable
        hpet.write(0x0F0, 42);
        assert_eq!(hpet.counter(), 0); // Should not change
    }

    #[test]
    fn test_one_shot_timer() {
        let mut hpet = Hpet::new();

        // Configure timer 0: enable interrupt, one-shot, route to IRQ 2.
        let timer_config = (1u64 << 2) | (2u64 << 9); // int enable + route to IRQ 2
        hpet.write(0x100, timer_config);
        // Set comparator to 5 ticks.
        hpet.write(0x108, 5);

        // Enable HPET.
        hpet.write(0x010, 1);

        // Tick 3 ticks — should not fire.
        let fired = hpet.tick(300);
        assert!(fired.is_empty());

        // Tick 3 more ticks (total 6) — should fire.
        let fired = hpet.tick(300);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0], (0, 2));
    }

    #[test]
    fn test_periodic_timer() {
        let mut hpet = Hpet::new();

        // Timer 0 supports periodic mode (bit 4 in capabilities).
        // Enable interrupt + periodic mode + route to IRQ 0.
        let timer_config = (1u64 << 2) | (1u64 << 3); // int enable + periodic
        hpet.write(0x100, timer_config);
        hpet.write(0x108, 10); // Period/comparator = 10 ticks

        hpet.write(0x010, 1); // Enable HPET

        // First fire at tick 10.
        let fired = hpet.tick(1000); // 10 ticks
        assert_eq!(fired.len(), 1);

        // Second fire at tick 20.
        let fired = hpet.tick(1000);
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn test_legacy_routing() {
        let mut hpet = Hpet::new();
        hpet.write(0x010, 3); // Enable + legacy routing

        assert!(hpet.is_enabled());
        assert!(hpet.legacy_routing());

        // Timer 0 interrupt → IRQ 0.
        let timer_config = 1u64 << 2; // int enable only
        hpet.write(0x100, timer_config);
        hpet.write(0x108, 5);

        let fired = hpet.tick(600); // 6 ticks
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0], (0, 0)); // Legacy: timer 0 → IRQ 0
    }

    #[test]
    fn test_interrupt_status_clear() {
        let mut hpet = Hpet::new();

        // Level-triggered timer.
        let timer_config = (1u64 << 2) | (1u64 << 1); // int enable + level triggered
        hpet.write(0x100, timer_config);
        hpet.write(0x108, 5);
        hpet.write(0x010, 1);

        hpet.tick(600);
        assert_ne!(hpet.read(0x020), 0); // Interrupt status set

        // Clear by writing 1.
        hpet.write(0x020, hpet.read(0x020));
        assert_eq!(hpet.read(0x020), 0);
    }

    #[test]
    fn test_reset() {
        let mut hpet = Hpet::new();
        hpet.write(0x010, 1);
        hpet.tick(1000);
        assert!(hpet.counter() > 0);

        hpet.reset();
        assert!(!hpet.is_enabled());
        assert_eq!(hpet.counter(), 0);
    }

    #[test]
    fn test_read_capabilities() {
        let hpet = Hpet::new();
        let cap = hpet.read(0x000);
        // Number of timers minus 1 in bits 12:8.
        let num_timers = ((cap >> 8) & 0x1F) as usize + 1;
        assert_eq!(num_timers, HPET_NUM_TIMERS);
    }
}

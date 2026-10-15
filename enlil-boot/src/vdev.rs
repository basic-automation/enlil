//! Virtual legacy interrupt hardware for a guest: the dual 8259A PIC and the
//! 8254 PIT.
//!
//! These are *state machines with plain-data state* and no I/O of their own:
//! the run loop feeds them port accesses and the current time, and asks them
//! which interrupt (if any) to inject. Keeping them pure makes them host-tested
//! and, because the state is plain data, serializable for live migration later.
//!
//! The PIC models what Linux's `i8259` driver exercises: the ICW1-4
//! initialization sequence, interrupt mask, edge/level trigger, fixed-priority
//! arbitration with the slave cascaded on IRQ 2, non-specific and specific EOI,
//! and OCW3 IRR/ISR reads. The PIT models channel 0 in the modes Linux uses as a
//! clock-event device (0, 2, 3), with counter latch and read-back of the live
//! count.

// ---------------------------------------------------------------------------
// 8259A PIC
// ---------------------------------------------------------------------------

/// One 8259A.
// The chip's initialization words genuinely are four independent mode bits
// (IC4, SNGL, AEOI and the OCW3 read select).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pic8259 {
    irr: u8,
    isr: u8,
    imr: u8,
    base: u8,
    /// Where in ICW2..ICW4 an initialization sequence is (`0` = not initializing).
    init_step: u8,
    icw4_needed: bool,
    single: bool,
    auto_eoi: bool,
    read_isr: bool,
    /// Level-triggered inputs (the ELCR, plus ICW1's LTIM applying to all).
    elcr: u8,
    /// Last level seen on each input, for edge detection.
    level: u8,
}

impl Pic8259 {
    /// A PIC in its power-on state: everything masked.
    #[must_use]
    pub const fn new(base: u8) -> Self {
        Self {
            irr: 0,
            isr: 0,
            imr: 0xFF,
            base,
            init_step: 0,
            icw4_needed: false,
            single: false,
            auto_eoi: false,
            read_isr: false,
            elcr: 0,
            level: 0,
        }
    }

    /// The interrupt vector base (ICW2).
    #[must_use]
    pub const fn base(&self) -> u8 {
        self.base
    }

    /// Drive input `irq` (0-7) to `level`. Edge-triggered inputs latch on the
    /// rising edge; level-triggered ones follow the line.
    pub const fn set_irq(&mut self, irq: u8, level: bool) {
        let bit = 1u8 << (irq & 7);
        let was = self.level & bit != 0;
        if level {
            self.level |= bit;
            if self.elcr & bit != 0 || !was {
                self.irr |= bit;
            }
        } else {
            self.level &= !bit;
            if self.elcr & bit != 0 {
                self.irr &= !bit;
            }
        }
    }

    /// The highest-priority unmasked request that outranks everything in
    /// service, as an input number (0-7).
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // trailing_zeros of a u8 is at most 8
    pub const fn pending(&self) -> Option<u8> {
        let req = self.irr & !self.imr;
        if req == 0 {
            return None;
        }
        let irq = req.trailing_zeros() as u8;
        // A bit in service at the same or higher priority (lower number) blocks.
        if self.isr != 0 && self.isr.trailing_zeros() as u8 <= irq {
            return None;
        }
        Some(irq)
    }

    /// Acknowledge input `irq`: move it from requested to in-service (unless
    /// auto-EOI) and return its vector.
    pub const fn ack(&mut self, irq: u8) -> u8 {
        let bit = 1u8 << (irq & 7);
        if self.elcr & bit == 0 {
            self.irr &= !bit;
        }
        if !self.auto_eoi {
            self.isr |= bit;
        }
        self.base | (irq & 7)
    }

    /// Whether any input is currently in service.
    #[must_use]
    pub const fn in_service(&self) -> u8 {
        self.isr
    }

    /// Write the command port (ICW1, OCW2 or OCW3).
    pub const fn write_cmd(&mut self, val: u8) {
        if val & 0x10 != 0 {
            // ICW1: restart initialization.
            self.irr = 0;
            self.isr = 0;
            self.imr = 0;
            self.level = 0;
            self.auto_eoi = false;
            self.read_isr = false;
            self.icw4_needed = val & 0x01 != 0;
            self.single = val & 0x02 != 0;
            if val & 0x08 != 0 {
                self.elcr = 0xFF; // LTIM: all inputs level-triggered
            }
            self.init_step = 2;
        } else if val & 0x08 != 0 {
            // OCW3: select what a command-port read returns.
            if val & 0x02 != 0 {
                self.read_isr = val & 0x01 != 0;
            }
        } else {
            // OCW2: end of interrupt.
            match val & 0xE0 {
                // Non-specific EOI (with or without rotate).
                0x20 | 0xA0 => {
                    if self.isr != 0 {
                        self.isr &= !(1 << self.isr.trailing_zeros());
                    }
                }
                // Specific EOI (with or without rotate).
                0x60 | 0xE0 => self.isr &= !(1 << (val & 7)),
                _ => {}
            }
        }
    }

    /// Write the data port (ICW2-4 during initialization, otherwise the mask).
    pub const fn write_data(&mut self, val: u8) {
        match self.init_step {
            2 => {
                self.base = val & 0xF8;
                self.init_step = if self.single {
                    if self.icw4_needed { 4 } else { 0 }
                } else {
                    3
                };
            }
            3 => {
                self.init_step = if self.icw4_needed { 4 } else { 0 };
            }
            4 => {
                self.auto_eoi = val & 0x02 != 0;
                self.init_step = 0;
            }
            _ => self.imr = val,
        }
    }

    /// Read the command port: the request or in-service register.
    #[must_use]
    pub const fn read_cmd(&self) -> u8 {
        if self.read_isr { self.isr } else { self.irr }
    }

    /// Read the data port: the interrupt mask register.
    #[must_use]
    pub const fn read_data(&self) -> u8 {
        self.imr
    }

    /// Write the edge/level control register.
    pub const fn set_elcr(&mut self, val: u8) {
        // IRQ 0, 1 and 2 are always edge-triggered.
        self.elcr = val & 0xF8;
    }
}

/// The PC's master + slave PIC pair (slave cascaded on the master's IRQ 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PicPair {
    master: Pic8259,
    slave: Pic8259,
}

impl Default for PicPair {
    fn default() -> Self {
        Self::new()
    }
}

impl PicPair {
    /// A power-on PIC pair (BIOS-default vector bases, all masked).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            master: Pic8259::new(0x08),
            slave: Pic8259::new(0x70),
        }
    }

    /// Drive PC interrupt line `irq` (0-15) to `level`.
    pub const fn set_irq(&mut self, irq: u8, level: bool) {
        if irq < 8 {
            self.master.set_irq(irq, level);
        } else {
            self.slave.set_irq(irq - 8, level);
        }
        // The slave's INT output feeds the master's IRQ 2.
        let cascade = self.slave.pending().is_some();
        self.master.set_irq(2, cascade);
    }

    /// The vector of the interrupt that would be delivered next, if any.
    #[must_use]
    pub const fn pending_vector(&self) -> Option<u8> {
        match self.master.pending() {
            Some(2) => match self.slave.pending() {
                Some(j) => Some(self.slave.base | j),
                None => None,
            },
            Some(i) => Some(self.master.base | i),
            None => None,
        }
    }

    /// Acknowledge the next interrupt (the INTA cycle): returns its vector and
    /// marks it in service.
    pub const fn ack(&mut self) -> Option<u8> {
        let Some(irq) = self.master.pending() else {
            return None;
        };
        if irq == 2 {
            let Some(j) = self.slave.pending() else {
                return None;
            };
            self.master.ack(2);
            let v = self.slave.ack(j);
            let cascade = self.slave.pending().is_some();
            self.master.set_irq(2, cascade);
            Some(v)
        } else {
            Some(self.master.ack(irq))
        }
    }

    /// Handle a read of one of the PIC's ports (`0x20/0x21`, `0xA0/0xA1`,
    /// `0x4D0/0x4D1`); `None` for any other port.
    #[must_use]
    pub const fn read(&self, port: u16) -> Option<u8> {
        Some(match port {
            0x20 => self.master.read_cmd(),
            0x21 => self.master.read_data(),
            0xA0 => self.slave.read_cmd(),
            0xA1 => self.slave.read_data(),
            0x4D0 => self.master.elcr,
            0x4D1 => self.slave.elcr,
            _ => return None,
        })
    }

    /// Handle a write to one of the PIC's ports; `false` for any other port.
    pub const fn write(&mut self, port: u16, val: u8) -> bool {
        match port {
            0x20 => self.master.write_cmd(val),
            0x21 => self.master.write_data(val),
            0xA0 => self.slave.write_cmd(val),
            0xA1 => self.slave.write_data(val),
            0x4D0 => self.master.set_elcr(val),
            0x4D1 => self.slave.set_elcr(val),
            _ => return false,
        }
        // Mask/EOI changes can change whether the cascade is asserted.
        let cascade = self.slave.pending().is_some();
        self.master.set_irq(2, cascade);
        true
    }
}

// ---------------------------------------------------------------------------
// 8254 PIT
// ---------------------------------------------------------------------------

/// The PIT's input clock in Hz.
pub const PIT_HZ: u64 = 1_193_182;

/// Channel-0 operating mode, as far as it matters for generating IRQ 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Interrupt once when the count expires (modes 0, 1, 4, 5).
    OneShot,
    /// Interrupt every `reload` ticks (modes 2 and 3).
    Periodic,
}

/// An 8254 timer's channel 0 (the IRQ 0 system tick) plus the speaker gate port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pit8254 {
    tsc_hz: u64,
    mode: Mode,
    access: u8,
    reload: u32,
    lo_pending: Option<u8>,
    latched: Option<u16>,
    read_hi_next: bool,
    start_tsc: u64,
    next_fire: Option<u64>,
    port61: u8,
}

impl Pit8254 {
    /// A PIT whose host clock runs at `tsc_hz` TSC ticks per second.
    #[must_use]
    pub const fn new(tsc_hz: u64) -> Self {
        Self {
            tsc_hz,
            mode: Mode::OneShot,
            access: 3,
            reload: 0x1_0000,
            lo_pending: None,
            latched: None,
            read_hi_next: false,
            start_tsc: 0,
            next_fire: None,
            port61: 0,
        }
    }

    /// PIT ticks -> TSC ticks.
    #[allow(clippy::cast_possible_truncation)] // a 16-bit count of PIT ticks, scaled by the TSC/PIT ratio, fits u64
    const fn ticks_to_tsc(&self, ticks: u64) -> u64 {
        (ticks as u128 * self.tsc_hz as u128 / PIT_HZ as u128) as u64
    }

    /// TSC ticks -> PIT ticks.
    #[allow(clippy::cast_possible_truncation)] // scaled down by tsc_hz/PIT_HZ, so it fits u64
    const fn tsc_to_ticks(&self, tsc: u64) -> u64 {
        (tsc as u128 * PIT_HZ as u128 / self.tsc_hz as u128) as u64
    }

    const fn period_tsc(&self) -> u64 {
        let t = self.ticks_to_tsc(self.reload as u64);
        if t == 0 { 1 } else { t }
    }

    /// The channel-0 count at `now` (what a counter-latch captures).
    #[must_use]
    pub const fn count(&self, now: u64) -> u16 {
        let elapsed = self.tsc_to_ticks(now.saturating_sub(self.start_tsc));
        let reload = self.reload as u64;
        let left = match self.mode {
            Mode::Periodic => reload - (elapsed % reload),
            Mode::OneShot => reload.saturating_sub(elapsed),
        };
        (left & 0xFFFF) as u16
    }

    /// Write the control word (port `0x43`).
    pub const fn write_control(&mut self, val: u8, now: u64) {
        let channel = val >> 6;
        let rw = (val >> 4) & 3;
        if channel != 0 {
            return; // channels 1 and 2 are not modelled
        }
        if rw == 0 {
            // Counter latch command.
            self.latched = Some(self.count(now));
            self.read_hi_next = false;
            return;
        }
        self.access = rw;
        self.lo_pending = None;
        self.mode = match (val >> 1) & 7 {
            2 | 3 | 6 | 7 => Mode::Periodic,
            _ => Mode::OneShot,
        };
        // A mode write stops the channel until a new count is loaded.
        self.next_fire = None;
    }

    /// Write channel 0's data port (`0x40`): a reload count, byte by byte.
    pub const fn write_data(&mut self, val: u8, now: u64) {
        let count = match self.access {
            1 => Some(val as u16),
            2 => Some((val as u16) << 8),
            _ => match self.lo_pending.take() {
                None => {
                    self.lo_pending = Some(val);
                    None
                }
                Some(lo) => Some(((val as u16) << 8) | lo as u16),
            },
        };
        if let Some(c) = count {
            self.reload = if c == 0 { 0x1_0000 } else { c as u32 };
            self.start_tsc = now;
            self.next_fire = Some(now + self.ticks_to_tsc(self.reload as u64));
        }
    }

    /// Read channel 0's data port (`0x40`): the latched count, else the live one.
    pub const fn read_data(&mut self, now: u64) -> u8 {
        let value = match self.latched {
            Some(v) => v,
            None => self.count(now),
        };
        let [lo, hi] = value.to_le_bytes();
        let byte = match self.access {
            1 => lo,
            2 => hi,
            _ => {
                let b = if self.read_hi_next { hi } else { lo };
                self.read_hi_next = !self.read_hi_next;
                b
            }
        };
        // The latch is released once both bytes have been read.
        if self.latched.is_some() && (self.access != 3 || !self.read_hi_next) {
            self.latched = None;
        }
        byte
    }

    /// Read the speaker/gate port (`0x61`): bit 4 toggles with the refresh
    /// clock, bit 5 is the (idle) channel-2 output.
    #[must_use]
    pub const fn read_port61(&self, now: u64) -> u8 {
        let refresh = (self.tsc_to_ticks(now) / 18) & 1;
        (self.port61 & 0x03) | ((refresh as u8) << 4)
    }

    /// Write the speaker/gate port (`0x61`).
    pub const fn write_port61(&mut self, val: u8) {
        self.port61 = val & 0x03;
    }

    /// Whether IRQ 0 is due at `now`; consumes the event. A periodic timer
    /// that fell behind coalesces the missed ticks into one interrupt.
    pub const fn poll(&mut self, now: u64) -> bool {
        let Some(due) = self.next_fire else {
            return false;
        };
        if now < due {
            return false;
        }
        self.next_fire = match self.mode {
            Mode::OneShot => None,
            Mode::Periodic => {
                let period = self.period_tsc();
                let missed = (now - due) / period + 1;
                Some(due + missed * period)
            }
        };
        true
    }

    /// The TSC value of the next IRQ 0, if the timer is running.
    #[must_use]
    pub const fn next_deadline(&self) -> Option<u64> {
        self.next_fire
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Program the PIC pair the way Linux's `init_8259A` does: master at
    /// vector 0x20, slave at 0x28, slave on IRQ 2, 8086 mode, all masked.
    fn linux_init(p: &mut PicPair) {
        p.write(0x21, 0xFF);
        p.write(0xA1, 0xFF);
        p.write(0x20, 0x11); // ICW1: edge, cascade, ICW4
        p.write(0x21, 0x20); // ICW2: vector base
        p.write(0x21, 0x04); // ICW3: slave on IRQ 2
        p.write(0x21, 0x01); // ICW4: 8086
        p.write(0xA0, 0x11);
        p.write(0xA1, 0x28);
        p.write(0xA1, 0x02); // ICW3: cascade identity 2
        p.write(0xA1, 0x01);
        p.write(0x21, 0xFF);
        p.write(0xA1, 0xFF);
    }

    fn unmask(p: &mut PicPair, irq: u8) {
        if irq < 8 {
            let m = p.read(0x21).unwrap();
            p.write(0x21, m & !(1 << irq));
        } else {
            let m = p.read(0xA1).unwrap();
            p.write(0xA1, m & !(1 << (irq - 8)));
            let mm = p.read(0x21).unwrap();
            p.write(0x21, mm & !(1 << 2));
        }
    }

    #[test]
    fn pic_starts_fully_masked() {
        let mut p = PicPair::new();
        p.set_irq(0, true);
        assert_eq!(p.pending_vector(), None);
        assert_eq!(p.read(0x21), Some(0xFF));
    }

    #[test]
    fn icw_sequence_sets_the_vector_bases_and_clears_the_masks() {
        let mut p = PicPair::new();
        linux_init(&mut p);
        // After init Linux masks everything again, then unmasks as it needs.
        unmask(&mut p, 0);
        p.set_irq(0, true);
        assert_eq!(p.pending_vector(), Some(0x20));
        let mut q = PicPair::new();
        linux_init(&mut q);
        unmask(&mut q, 4);
        q.set_irq(4, true);
        assert_eq!(q.pending_vector(), Some(0x24));
        let mut s = PicPair::new();
        linux_init(&mut s);
        unmask(&mut s, 12);
        s.set_irq(12, true);
        assert_eq!(s.pending_vector(), Some(0x2C), "slave IRQ 12 -> 0x28 + 4");
    }

    #[test]
    fn ack_moves_a_request_to_in_service_and_blocks_lower_priority() {
        let mut p = PicPair::new();
        linux_init(&mut p);
        unmask(&mut p, 0);
        unmask(&mut p, 4);
        p.set_irq(4, true);
        assert_eq!(p.ack(), Some(0x24));
        assert_eq!(p.pending_vector(), None, "nothing else pending");
        // A lower-priority request is blocked while IRQ 4 is in service...
        p.set_irq(5, true);
        unmask(&mut p, 5);
        assert_eq!(p.pending_vector(), None);
        // ...but a higher-priority one (IRQ 0) is not.
        p.set_irq(0, true);
        assert_eq!(p.pending_vector(), Some(0x20));
    }

    #[test]
    fn non_specific_eoi_releases_the_highest_priority_in_service() {
        let mut p = PicPair::new();
        linux_init(&mut p);
        unmask(&mut p, 4);
        unmask(&mut p, 5);
        p.set_irq(4, true);
        assert_eq!(p.ack(), Some(0x24));
        p.set_irq(5, true);
        assert_eq!(
            p.pending_vector(),
            None,
            "IRQ 5 blocked by IRQ 4 in service"
        );
        p.write(0x20, 0x20); // EOI
        assert_eq!(p.pending_vector(), Some(0x25));
    }

    #[test]
    fn specific_eoi_releases_only_the_named_input() {
        let mut p = PicPair::new();
        linux_init(&mut p);
        unmask(&mut p, 4);
        p.set_irq(4, true);
        assert_eq!(p.ack(), Some(0x24));
        p.write(0x20, 0x63); // specific EOI of IRQ 3: not in service, no effect
        p.set_irq(4, false);
        p.set_irq(4, true);
        assert_eq!(p.pending_vector(), None, "IRQ 4 still in service");
        p.write(0x20, 0x64); // specific EOI of IRQ 4
        assert_eq!(p.pending_vector(), Some(0x24));
    }

    #[test]
    fn edge_triggered_inputs_need_a_new_edge() {
        let mut p = PicPair::new();
        linux_init(&mut p);
        unmask(&mut p, 0);
        p.set_irq(0, true);
        assert_eq!(p.ack(), Some(0x20));
        p.write(0x20, 0x20);
        // The line is still high: no new edge, no new interrupt.
        p.set_irq(0, true);
        assert_eq!(p.pending_vector(), None);
        p.set_irq(0, false);
        p.set_irq(0, true);
        assert_eq!(p.pending_vector(), Some(0x20));
    }

    #[test]
    fn masking_defers_a_request_without_losing_it() {
        let mut p = PicPair::new();
        linux_init(&mut p);
        p.set_irq(4, true); // masked
        assert_eq!(p.pending_vector(), None);
        unmask(&mut p, 4);
        assert_eq!(p.pending_vector(), Some(0x24));
    }

    #[test]
    fn slave_interrupts_cascade_through_irq2() {
        let mut p = PicPair::new();
        linux_init(&mut p);
        unmask(&mut p, 12);
        p.set_irq(12, true);
        assert_eq!(p.pending_vector(), Some(0x2C));
        assert_eq!(p.ack(), Some(0x2C));
        assert_eq!(p.pending_vector(), None);
        // EOI both chips, as Linux does for a slave interrupt.
        p.write(0xA0, 0x64);
        p.write(0x20, 0x62);
        p.set_irq(12, false);
        p.set_irq(12, true);
        assert_eq!(p.pending_vector(), Some(0x2C));
    }

    #[test]
    fn ocw3_selects_irr_or_isr_for_command_port_reads() {
        let mut p = PicPair::new();
        linux_init(&mut p);
        unmask(&mut p, 4);
        p.set_irq(4, true);
        assert_eq!(p.read(0x20), Some(0x10), "IRR by default");
        p.ack();
        p.write(0x20, 0x0B); // OCW3: read ISR
        assert_eq!(p.read(0x20), Some(0x10));
        p.write(0x20, 0x0A); // OCW3: read IRR
        assert_eq!(p.read(0x20), Some(0x00), "request cleared by the ack");
    }

    #[test]
    fn level_triggered_inputs_follow_the_line() {
        let mut p = PicPair::new();
        linux_init(&mut p);
        unmask(&mut p, 5);
        p.write(0x4D0, 0x20); // IRQ 5 level-triggered
        p.set_irq(5, true);
        assert_eq!(p.pending_vector(), Some(0x25));
        p.set_irq(5, false);
        assert_eq!(p.pending_vector(), None, "request drops with the line");
        assert_eq!(p.read(0x4D0), Some(0x20));
        // IRQs 0-2 can never be level-triggered.
        p.write(0x4D0, 0xFF);
        assert_eq!(p.read(0x4D0), Some(0xF8));
    }

    #[test]
    fn unknown_ports_are_not_the_pics() {
        let mut p = PicPair::new();
        assert_eq!(p.read(0x22), None);
        assert!(!p.write(0x22, 1));
    }

    const HZ: u64 = 3_000_000_000;

    /// TSC ticks in `us` microseconds.
    const fn us(n: u64) -> u64 {
        n * (HZ / 1_000_000)
    }

    fn program(pit: &mut Pit8254, control: u8, count: u16, now: u64) {
        pit.write_control(control, now);
        let [lo, hi] = count.to_le_bytes();
        pit.write_data(lo, now);
        pit.write_data(hi, now);
    }

    #[test]
    fn periodic_mode_fires_every_period() {
        let mut pit = Pit8254::new(HZ);
        // Mode 2, lo/hi access, count 11932 ~= 100 Hz (10 ms).
        program(&mut pit, 0x34, 11932, 0);
        assert!(!pit.poll(us(9_000)));
        assert!(pit.poll(us(10_100)));
        assert!(!pit.poll(us(10_200)), "consumed");
        assert!(pit.poll(us(20_100)));
        assert!(pit.next_deadline().unwrap() > us(20_100));
    }

    #[test]
    fn periodic_mode_coalesces_missed_ticks_into_one() {
        let mut pit = Pit8254::new(HZ);
        program(&mut pit, 0x34, 11932, 0);
        // The guest was away for 55 ms: five ticks missed, one interrupt.
        assert!(pit.poll(us(55_000)));
        assert!(!pit.poll(us(55_100)));
        let next = pit.next_deadline().unwrap();
        assert!(
            next > us(55_000) && next <= us(65_000),
            "back on the 10 ms grid"
        );
    }

    #[test]
    fn one_shot_mode_fires_once_then_stops() {
        let mut pit = Pit8254::new(HZ);
        // Mode 0, count 1193 ~= 1 ms.
        program(&mut pit, 0x30, 1193, us(100));
        assert_eq!(pit.next_deadline(), Some(us(100) + pit.ticks_to_tsc(1193)));
        assert!(!pit.poll(us(500)));
        assert!(pit.poll(us(1_200)));
        assert!(!pit.poll(us(9_999_999)), "one-shot does not repeat");
        assert_eq!(pit.next_deadline(), None);
    }

    #[test]
    fn rewriting_a_one_shot_count_rearms_it() {
        let mut pit = Pit8254::new(HZ);
        program(&mut pit, 0x30, 1193, 0);
        assert!(pit.poll(us(2_000)));
        // Linux reprograms the next event by writing a fresh count.
        pit.write_data(0xA9, us(2_000));
        pit.write_data(0x04, us(2_000));
        assert!(pit.next_deadline().is_some());
        assert!(pit.poll(us(4_000)));
    }

    #[test]
    fn a_control_write_stops_the_channel_until_a_count_arrives() {
        let mut pit = Pit8254::new(HZ);
        program(&mut pit, 0x34, 11932, 0);
        pit.write_control(0x34, us(1));
        assert_eq!(pit.next_deadline(), None);
        assert!(!pit.poll(us(50_000)));
    }

    #[test]
    fn count_zero_means_65536() {
        let mut pit = Pit8254::new(HZ);
        program(&mut pit, 0x34, 0, 0);
        assert_eq!(pit.next_deadline(), Some(pit.ticks_to_tsc(0x1_0000)));
    }

    #[test]
    fn latched_count_reads_low_then_high_and_counts_down() {
        let mut pit = Pit8254::new(HZ);
        program(&mut pit, 0x34, 0xFFFF, 0);
        // 1 ms in: 1193 ticks elapsed.
        let now = us(1_000);
        pit.write_control(0x00, now); // latch channel 0
        let lo = pit.read_data(now);
        let hi = pit.read_data(now);
        let count = u16::from_le_bytes([lo, hi]);
        assert!(
            (0xFFFF - 1194..=0xFFFF - 1192).contains(&count),
            "{count:#x}"
        );
        // The latch is released: the next read is live and still counting down.
        let lo2 = pit.read_data(us(2_000));
        let hi2 = pit.read_data(us(2_000));
        assert!(u16::from_le_bytes([lo2, hi2]) < count);
    }

    #[test]
    fn single_byte_access_modes_load_one_byte() {
        let mut pit = Pit8254::new(HZ);
        pit.write_control(0x14, 0); // channel 0, lo-byte only, mode 2
        pit.write_data(100, 0);
        assert_eq!(pit.next_deadline(), Some(pit.ticks_to_tsc(100)));
        pit.write_control(0x24, 0); // hi-byte only
        pit.write_data(2, 0);
        assert_eq!(pit.next_deadline(), Some(pit.ticks_to_tsc(512)));
    }

    #[test]
    fn port61_reports_the_gate_and_a_toggling_refresh_bit() {
        let mut pit = Pit8254::new(HZ);
        pit.write_port61(0xFF);
        assert_eq!(
            pit.read_port61(0) & 0x03,
            0x03,
            "gate and speaker bits readable"
        );
        let a = pit.read_port61(0) & 0x10;
        let b = pit.read_port61(us(16)) & 0x10; // one refresh period (~15 us) later
        assert_ne!(a, b, "the refresh bit toggles with time");
        assert_eq!(pit.read_port61(0) & 0x20, 0, "channel 2 output idle");
    }

    #[test]
    fn other_channels_are_ignored() {
        let mut pit = Pit8254::new(HZ);
        program(&mut pit, 0x34, 11932, 0);
        let before = pit;
        pit.write_control(0xB6, 0); // channel 2
        assert_eq!(pit, before);
    }
}

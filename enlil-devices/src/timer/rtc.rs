//! Motorola MC146818 RTC / CMOS emulation.
//!
//! A PC's real-time clock and battery-backed CMOS RAM live behind two ports:
//! an **index** port `0x70` and a **data** port `0x71`. The index port's low
//! seven bits select one of 128 CMOS bytes; **bit 7 is the NMI-disable latch**,
//! not part of the address — a guest writing the index also enables/masks NMI,
//! so the two must be split out faithfully or NMI handling breaks.
//!
//! Bytes `0x00-0x09` are the time/date fields (seconds, minutes, hours, weekday,
//! day-of-month, month, year — plus the alarm shadows), `0x0A-0x0D` are status
//! registers A-D, and the rest is general-purpose NVRAM the BIOS uses for
//! equipment configuration and checksums. The chip's interrupt output is wired
//! to **IRQ8** (slave 8259 line 0 / GSI 8).
//!
//! Register B's mode bits change how the *same* underlying time reads back:
//! **DM** (bit 2) selects binary vs BCD, and **24/12** (bit 1) selects 24-hour
//! vs 12-hour with a high "PM" bit on the hours register. To keep that honest
//! the device stores the wall clock as canonical binary 24-hour fields
//! ([`RtcTime`]) and formats on read / parses on write per Register B.
//!
//! The wall clock is *injected* (a Unix-epoch second count) rather than read
//! from the host, so a guest's view of time is hypervisor-controlled and the
//! model is deterministic in tests. Unix seconds are converted to a civil
//! date with plain arithmetic (no time-crate dependency, `no_std`-friendly).

use std::sync::{Arc, Mutex};

use super::pit::IrqLine;
use crate::bus::PioDevice;
use crate::truncate::{u8_of, u16_of};

/// RTC / CMOS index port — low 7 bits select the register, bit 7 disables NMI.
pub const RTC_INDEX: u16 = 0x70;
/// RTC / CMOS data port — reads/writes the byte selected via [`RTC_INDEX`].
pub const RTC_DATA: u16 = 0x71;

/// RTC IRQ line: the chip's interrupt output is wired to ISA IRQ8.
pub const RTC_IRQ: u8 = 8;

// CMOS register indices for the clock/status block.
const REG_SECONDS: u8 = 0x00;
const REG_SECONDS_ALARM: u8 = 0x01;
const REG_MINUTES: u8 = 0x02;
const REG_MINUTES_ALARM: u8 = 0x03;
const REG_HOURS: u8 = 0x04;
const REG_HOURS_ALARM: u8 = 0x05;
const REG_WEEKDAY: u8 = 0x06;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_A: u8 = 0x0A;
const REG_B: u8 = 0x0B;
const REG_C: u8 = 0x0C;
const REG_D: u8 = 0x0D;
/// Century register (PC/AT convention; FADT points the OS here).
const REG_CENTURY: u8 = 0x32;

// Register A bits.
const REG_A_UIP: u8 = 0x80; // update in progress (read-only)
// Register B bits.
const REG_B_SET: u8 = 0x80; // halt updates while the OS sets the clock
const REG_B_PIE: u8 = 0x40; // periodic interrupt enable
const REG_B_AIE: u8 = 0x20; // alarm interrupt enable
const REG_B_UIE: u8 = 0x10; // update-ended interrupt enable
const REG_B_DM: u8 = 0x04; // data mode: 1 = binary, 0 = BCD
const REG_B_24H: u8 = 0x02; // 1 = 24-hour, 0 = 12-hour
// Register C bits (read-clears).
const REG_C_IRQF: u8 = 0x80; // any enabled flag is set
const REG_C_PF: u8 = 0x40; // periodic flag
const REG_C_AF: u8 = 0x20; // alarm flag
const REG_C_UF: u8 = 0x10; // update-ended flag
// Register D.
const REG_D_VRT: u8 = 0x80; // valid RAM and time (battery good)

/// Number of addressable CMOS bytes (index is 7 bits).
const CMOS_SIZE: usize = 128;

/// Broken-down wall-clock time in canonical binary, 24-hour form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcTime {
    pub second: u8,
    pub minute: u8,
    pub hour: u8,
    /// Day of week, 1 = Sunday .. 7 = Saturday (MC146818 convention).
    pub weekday: u8,
    pub day: u8,
    pub month: u8,
    /// Full year, e.g. 2026.
    pub year: u16,
}

impl RtcTime {
    /// Convert a Unix timestamp (seconds since 1970-01-01 UTC) to broken-down
    /// civil time, using the algorithm from Howard Hinnant's `chrono` notes
    /// (`civil_from_days`). An RTC only ever holds dates at or after the epoch,
    /// so the whole computation stays in unsigned arithmetic (no signed era
    /// correction needed) and narrows the bounded results explicitly.
    #[must_use]
    pub fn from_unix(secs: u64) -> Self {
        let days = secs / 86_400;
        let rem = secs % 86_400;

        let hour = u8_of(rem / 3600);
        let minute = u8_of((rem % 3600) / 60);
        let second = u8_of(rem % 60);

        // Day of week: 1970-01-01 was a Thursday. Sunday = 1 in MC146818 terms.
        let weekday = u8_of((days + 4) % 7 + 1);

        // civil_from_days: shift the epoch so the year starts in March, making
        // the leap day the last day of the 400-year cycle.
        let z = days + 719_468;
        let era = z / 146_097;
        let doe = z - era * 146_097; // [0, 146096]
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
        let mp = (5 * doy + 2) / 153; // [0, 11] (Mar = 0)
        let day = u8_of(doy - (153 * mp + 2) / 5 + 1); // [1, 31]
        let month = u8_of(if mp < 10 { mp + 3 } else { mp - 9 }); // [1, 12]
        let year = u16_of(if month <= 2 { y + 1 } else { y });

        Self {
            second,
            minute,
            hour,
            weekday,
            day,
            month,
            year,
        }
    }
}

/// Encode a value into Register-B format (BCD unless DM selects binary).
const fn encode(val: u8, binary: bool) -> u8 {
    if binary {
        val
    } else {
        ((val / 10) << 4) | (val % 10)
    }
}

/// Decode a value written in Register-B format back to canonical binary.
const fn decode(val: u8, binary: bool) -> u8 {
    if binary {
        val
    } else {
        ((val >> 4) * 10) + (val & 0x0F)
    }
}

/// The MC146818 RTC / CMOS.
pub struct Rtc146818 {
    /// 128 bytes of CMOS RAM. The clock/status registers are computed on the
    /// fly; the rest (alarm shadows, NVRAM, checksums) live here.
    ram: [u8; CMOS_SIZE],
    /// Last register selected through the index port (low 7 bits).
    index: u8,
    /// NMI-disable latch (index-port bit 7): `true` masks NMI.
    nmi_disabled: bool,
    /// The injected wall clock.
    time: RtcTime,
    /// Optional sink pulsed when the RTC asserts IRQ8.
    irq: Option<Box<dyn IrqLine>>,
}

impl Rtc146818 {
    /// A new RTC initialized to `time`, 24-hour binary mode (Register B
    /// `DM | 24H`), all interrupts disabled, and VRT (battery-good) set.
    #[must_use]
    pub fn new(time: RtcTime) -> Self {
        let mut ram = [0u8; CMOS_SIZE];
        ram[REG_B as usize] = REG_B_DM | REG_B_24H;
        ram[REG_D as usize] = REG_D_VRT;
        // Register A: a typical 32.768 kHz divider (010) and a 1024 Hz rate (0110).
        ram[REG_A as usize] = 0x26;
        Self {
            ram,
            index: 0,
            nmi_disabled: false,
            time,
            irq: None,
        }
    }

    /// Attach a sink pulsed when the RTC raises IRQ8 (see [`IrqLine`]).
    pub fn attach_irq(&mut self, line: Box<dyn IrqLine>) {
        self.irq = Some(line);
    }

    /// Whether NMI is currently masked via the index port's bit 7.
    #[must_use]
    pub const fn nmi_disabled(&self) -> bool {
        self.nmi_disabled
    }

    /// Replace the wall clock the guest reads (host-controlled time).
    pub const fn set_time(&mut self, time: RtcTime) {
        self.time = time;
    }

    /// The current wall clock.
    #[must_use]
    pub const fn time(&self) -> RtcTime {
        self.time
    }

    const fn reg_b(&self) -> u8 {
        self.ram[REG_B as usize]
    }

    /// `true` when Register B selects binary data mode (DM bit set).
    const fn binary_mode(&self) -> bool {
        self.reg_b() & REG_B_DM != 0
    }

    /// Format the hours register honoring the 24/12-hour Register-B bit.
    const fn read_hours(&self) -> u8 {
        let binary = self.binary_mode();
        if self.reg_b() & REG_B_24H != 0 {
            encode(self.time.hour, binary)
        } else {
            // 12-hour: 1..=12 with bit 7 set for PM.
            let pm = self.time.hour >= 12;
            let mut h = self.time.hour % 12;
            if h == 0 {
                h = 12;
            }
            encode(h, binary) | if pm { 0x80 } else { 0 }
        }
    }

    /// Parse a value written to the hours register back to canonical 24-hour.
    const fn write_hours(&mut self, val: u8) {
        let binary = self.binary_mode();
        if self.reg_b() & REG_B_24H != 0 {
            self.time.hour = decode(val, binary);
        } else {
            let pm = val & 0x80 != 0;
            let mut h = decode(val & 0x7F, binary) % 12;
            if pm {
                h += 12;
            }
            self.time.hour = h;
        }
    }

    /// Read the CMOS register currently selected by the index.
    #[must_use]
    pub fn read_data(&mut self) -> u8 {
        let binary = self.binary_mode();
        match self.index {
            REG_SECONDS => encode(self.time.second, binary),
            REG_MINUTES => encode(self.time.minute, binary),
            REG_HOURS => self.read_hours(),
            REG_WEEKDAY => encode(self.time.weekday, binary),
            REG_DAY => encode(self.time.day, binary),
            REG_MONTH => encode(self.time.month, binary),
            REG_YEAR => encode(u8_of(self.time.year % 100), binary),
            REG_CENTURY => encode(u8_of(self.time.year / 100), binary),
            REG_A => self.ram[REG_A as usize], // UIP modelled as never set (instant reads)
            REG_C => {
                // Register C is read-to-clear: return the flags, then clear them
                // and deassert IRQ8.
                let val = self.ram[REG_C as usize];
                self.ram[REG_C as usize] = 0;
                if let Some(irq) = &self.irq {
                    irq.set_level(false);
                }
                val
            }
            idx => self.ram[idx as usize],
        }
    }

    /// Write the CMOS register currently selected by the index.
    pub fn write_data(&mut self, val: u8) {
        let binary = self.binary_mode();
        match self.index {
            REG_SECONDS => self.time.second = decode(val, binary),
            REG_MINUTES => self.time.minute = decode(val, binary),
            REG_HOURS => self.write_hours(val),
            REG_WEEKDAY => self.time.weekday = decode(val, binary),
            REG_DAY => self.time.day = decode(val, binary),
            REG_MONTH => self.time.month = decode(val, binary),
            REG_YEAR => {
                let century = self.time.year / 100;
                self.time.year = century * 100 + u16::from(decode(val, binary));
            }
            REG_CENTURY => {
                let year = self.time.year % 100;
                self.time.year = u16::from(decode(val, binary)) * 100 + year;
            }
            REG_A => {
                // The UIP bit is read-only; keep the rest the guest writes.
                self.ram[REG_A as usize] = val & !REG_A_UIP;
            }
            REG_C | REG_D => {} // status C/D are read-only
            idx => self.ram[idx as usize] = val,
        }
    }

    /// Write the index port: select a register (low 7 bits) and latch the
    /// NMI-disable bit (bit 7).
    pub const fn write_index(&mut self, val: u8) {
        self.nmi_disabled = val & 0x80 != 0;
        self.index = val & 0x7F;
    }

    /// Advance the clock by one second and raise the update-ended (and, if the
    /// alarm matches, alarm) interrupt when enabled.
    ///
    /// While Register B's SET bit is high the OS is mid-update, so the clock is
    /// frozen and no update interrupt is generated — matching real hardware.
    /// Returns `true` if an enabled interrupt fired.
    pub fn tick_second(&mut self) -> bool {
        if self.reg_b() & REG_B_SET != 0 {
            return false;
        }
        self.advance_one_second();

        let mut flags = REG_C_UF; // update-ended always latches the UF flag
        if self.alarm_matches() {
            flags |= REG_C_AF;
        }

        let enables = self.reg_b();
        let mut asserted = false;
        let mut c = self.ram[REG_C as usize] | flags;
        // IRQF is set if a flag's matching enable is on.
        if (enables & REG_B_UIE != 0 && flags & REG_C_UF != 0)
            || (enables & REG_B_AIE != 0 && flags & REG_C_AF != 0)
        {
            c |= REG_C_IRQF;
            asserted = true;
        }
        self.ram[REG_C as usize] = c;
        if asserted && let Some(irq) = &self.irq {
            irq.set_level(true);
        }
        asserted
    }

    /// The periodic-interrupt frequency selected by Register A's rate-select
    /// bits (RS, bits 3:0), or `None` when RS is 0 (periodic disabled). With the
    /// standard 32.768 kHz time base: RS 1 = 256 Hz, RS 2 = 128 Hz, and RS 3..15
    /// = `32768 >> (RS-1)` Hz (8192 Hz down to 2 Hz). A timer driver reads this
    /// to decide how often to call [`tick_periodic`](Self::tick_periodic).
    #[must_use]
    pub const fn periodic_rate_hz(&self) -> Option<u32> {
        match self.ram[REG_A as usize] & 0x0F {
            0 => None,
            1 => Some(256),
            2 => Some(128),
            n => Some(32768u32 >> (n - 1)),
        }
    }

    /// Drive one periodic tick: latch Register C's periodic flag (PF) — which is
    /// set at the RS rate regardless of the enable — and, when Register B's PIE
    /// is set, also set IRQF and assert IRQ8. A timer driver calls this at
    /// [`periodic_rate_hz`](Self::periodic_rate_hz); reading Register C clears
    /// PF/IRQF and deasserts the line, exactly like the update/alarm sources.
    /// A no-op (returns `false`) when no rate is selected. Returns whether the
    /// interrupt line was asserted.
    pub fn tick_periodic(&mut self) -> bool {
        if self.periodic_rate_hz().is_none() {
            return false; // RS = 0: periodic timer off
        }
        let asserted = self.reg_b() & REG_B_PIE != 0;
        let mut c = self.ram[REG_C as usize] | REG_C_PF;
        if asserted {
            c |= REG_C_IRQF;
        }
        self.ram[REG_C as usize] = c;
        if asserted && let Some(irq) = &self.irq {
            irq.set_level(true);
        }
        asserted
    }

    /// Whether the current time matches the alarm shadow registers. A "don't
    /// care" alarm byte (the top two bits set, `0xC0`) matches any value.
    fn alarm_matches(&self) -> bool {
        let binary = self.binary_mode();
        let field = |stored: u8, alarm_idx: u8| {
            let alarm = self.ram[alarm_idx as usize];
            alarm & 0xC0 == 0xC0 || decode(alarm, binary) == stored
        };
        field(self.time.second, REG_SECONDS_ALARM)
            && field(self.time.minute, REG_MINUTES_ALARM)
            && field(self.time.hour, REG_HOURS_ALARM)
    }

    /// Increment the broken-down time by one second with calendar carry.
    const fn advance_one_second(&mut self) {
        let t = &mut self.time;
        t.second += 1;
        if t.second < 60 {
            return;
        }
        t.second = 0;
        t.minute += 1;
        if t.minute < 60 {
            return;
        }
        t.minute = 0;
        t.hour += 1;
        if t.hour < 24 {
            return;
        }
        t.hour = 0;
        t.weekday = if t.weekday >= 7 { 1 } else { t.weekday + 1 };
        t.day += 1;
        if t.day <= days_in_month(t.month, t.year) {
            return;
        }
        t.day = 1;
        t.month += 1;
        if t.month <= 12 {
            return;
        }
        t.month = 1;
        t.year += 1;
    }
}

/// A thread-safe, shareable handle to one [`Rtc146818`].
///
/// The wall clock is advanced from a timer thread while a guest reads the ports
/// from its vCPU thread, so the chip is guarded by a `Mutex` (mirroring
/// [`SharedPic`](crate::interrupt::SharedPic) /
/// [`SharedInterruptController`](crate::interrupt::SharedInterruptController)).
/// Cloning shares the same RTC: the [`RtcPort`] bus adapter and the timer-thread
/// `tick_second` driver hold independent handles to one chip.
#[derive(Clone)]
pub struct SharedRtc(Arc<Mutex<Rtc146818>>);

impl SharedRtc {
    /// Wrap a fresh RTC initialized to `time`.
    #[must_use]
    pub fn new(time: RtcTime) -> Self {
        Self(Arc::new(Mutex::new(Rtc146818::new(time))))
    }

    /// Run `f` with exclusive access — to attach the IRQ8 sink, advance the
    /// clock (`tick_second`), or inspect state.
    ///
    /// # Panics
    /// Panics if the RTC mutex has been poisoned by a prior panic while held.
    pub fn with<R>(&self, f: impl FnOnce(&mut Rtc146818) -> R) -> R {
        f(&mut self.0.lock().expect("RTC mutex poisoned"))
    }

    /// The index/data ports (`0x70`/`0x71`) as a bus [`PioDevice`].
    #[must_use]
    pub fn port(&self) -> RtcPort {
        RtcPort { rtc: self.clone() }
    }
}

/// The RTC's two ports (`0x70`/`0x71`) as a bus [`PioDevice`].
///
/// Byte-wide registers. The index port (`0x70`) is write-only on real hardware
/// — reads return open-bus `0xFF` — and also latches the NMI-disable bit; the
/// data port (`0x71`) reads/writes the selected CMOS byte.
pub struct RtcPort {
    rtc: SharedRtc,
}

impl PioDevice for RtcPort {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        match port {
            RTC_DATA => u32::from(self.rtc.with(Rtc146818::read_data)),
            _ => 0xFF, // index port is write-only
        }
    }

    fn pio_write(&mut self, port: u16, _size: u8, data: u32) {
        let byte = u8_of(data);
        match port {
            RTC_INDEX => self.rtc.with(|r| r.write_index(byte)),
            RTC_DATA => self.rtc.with(|r| r.write_data(byte)),
            _ => {}
        }
    }

    fn port_range(&self) -> (u16, u16) {
        (RTC_INDEX, RTC_DATA + 1)
    }
}

/// Days in `month` (1-12) of `year`, accounting for leap years.
const fn days_in_month(month: u8, year: u16) -> u8 {
    match month {
        2 => {
            if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test time: 2026-06-07 (Sunday) 13:14:15 UTC.
    fn sample() -> RtcTime {
        // Unix seconds for 2026-06-07T13:14:15Z.
        RtcTime::from_unix(1_780_838_055)
    }

    #[test]
    fn unix_to_civil_is_correct() {
        let t = sample();
        assert_eq!((t.year, t.month, t.day), (2026, 6, 7));
        assert_eq!((t.hour, t.minute, t.second), (13, 14, 15));
        assert_eq!(t.weekday, 1, "2026-06-07 is a Sunday (weekday 1)");
    }

    #[test]
    fn unix_epoch_is_thursday_1970() {
        let t = RtcTime::from_unix(0);
        assert_eq!((t.year, t.month, t.day), (1970, 1, 1));
        assert_eq!(t.weekday, 5, "1970-01-01 is a Thursday (weekday 5)");
    }

    #[test]
    fn reads_time_in_binary_mode() {
        let mut rtc = Rtc146818::new(sample());
        rtc.write_index(REG_HOURS);
        assert_eq!(rtc.read_data(), 13);
        rtc.write_index(REG_MINUTES);
        assert_eq!(rtc.read_data(), 14);
        rtc.write_index(REG_YEAR);
        assert_eq!(rtc.read_data(), 26);
        rtc.write_index(REG_CENTURY);
        assert_eq!(rtc.read_data(), 20);
    }

    #[test]
    fn reads_time_in_bcd_mode() {
        let mut rtc = Rtc146818::new(sample());
        // Clear DM -> BCD. Register B keeps 24-hour.
        rtc.write_index(REG_B);
        rtc.write_data(REG_B_24H);
        rtc.write_index(REG_SECONDS);
        assert_eq!(rtc.read_data(), 0x15); // 15 decimal -> 0x15 BCD
        rtc.write_index(REG_HOURS);
        assert_eq!(rtc.read_data(), 0x13);
    }

    #[test]
    fn twelve_hour_mode_sets_pm_bit() {
        let mut rtc = Rtc146818::new(sample()); // 13:00 -> 1 PM
        rtc.write_index(REG_B);
        rtc.write_data(REG_B_DM); // binary, 12-hour
        rtc.write_index(REG_HOURS);
        let h = rtc.read_data();
        assert_eq!(h & 0x7F, 1, "1 o'clock");
        assert_ne!(h & 0x80, 0, "PM bit set");
    }

    #[test]
    fn writing_the_clock_round_trips() {
        let mut rtc = Rtc146818::new(sample());
        rtc.write_index(REG_HOURS);
        rtc.write_data(7);
        rtc.write_index(REG_HOURS);
        assert_eq!(rtc.read_data(), 7);
        assert_eq!(rtc.time().hour, 7);
    }

    #[test]
    fn index_port_splits_nmi_disable_from_the_address() {
        let mut rtc = Rtc146818::new(sample());
        rtc.write_index(0x80 | REG_SECONDS); // NMI disabled, select seconds
        assert!(rtc.nmi_disabled());
        assert_eq!(rtc.read_data(), 15);
        rtc.write_index(REG_SECONDS); // NMI re-enabled
        assert!(!rtc.nmi_disabled());
    }

    #[test]
    fn nvram_bytes_store_and_load() {
        let mut rtc = Rtc146818::new(sample());
        rtc.write_index(0x40);
        rtc.write_data(0xAB);
        rtc.write_index(0x40);
        assert_eq!(rtc.read_data(), 0xAB);
    }

    #[test]
    fn register_c_is_read_to_clear() {
        let mut rtc = Rtc146818::new(sample());
        rtc.write_index(REG_B);
        rtc.write_data(REG_B_DM | REG_B_24H | REG_B_UIE); // enable update interrupt
        assert!(rtc.tick_second());
        rtc.write_index(REG_C);
        let c = rtc.read_data();
        assert_ne!(c & REG_C_UF, 0, "update flag latched");
        assert_ne!(c & REG_C_IRQF, 0, "IRQF set");
        // Second read returns cleared flags.
        rtc.write_index(REG_C);
        assert_eq!(rtc.read_data(), 0);
    }

    #[test]
    fn update_interrupt_pulses_irq8_and_clears_on_reg_c_read() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let log = Arc::clone(&log);
            move |level: bool| log.lock().unwrap().push(level)
        };
        let mut rtc = Rtc146818::new(sample());
        rtc.attach_irq(Box::new(sink));
        rtc.write_index(REG_B);
        rtc.write_data(REG_B_DM | REG_B_24H | REG_B_UIE);

        assert!(rtc.tick_second());
        rtc.write_index(REG_C);
        let _ = rtc.read_data();
        assert_eq!(&*log.lock().unwrap(), &[true, false]);
    }

    #[test]
    fn no_update_interrupt_while_set_bit_held() {
        let mut rtc = Rtc146818::new(sample());
        rtc.write_index(REG_B);
        rtc.write_data(REG_B_SET | REG_B_DM | REG_B_24H | REG_B_UIE);
        let before = rtc.time();
        assert!(!rtc.tick_second(), "no interrupt while SET held");
        assert_eq!(rtc.time(), before, "clock frozen while SET held");
    }

    #[test]
    fn tick_carries_across_a_minute() {
        let mut rtc = Rtc146818::new(RtcTime::from_unix(59)); // 1970-01-01 00:00:59
        assert!(!rtc.tick_second()); // UIE off -> no interrupt, but time advances
        assert_eq!((rtc.time().minute, rtc.time().second), (1, 0));
    }

    #[test]
    fn alarm_interrupt_fires_on_match() {
        let mut rtc = Rtc146818::new(RtcTime::from_unix(58)); // 00:00:58
        // Alarm at second 59 (binary mode), enable AIE.
        rtc.write_index(REG_SECONDS_ALARM);
        rtc.write_data(59);
        rtc.write_index(REG_MINUTES_ALARM);
        rtc.write_data(0xC0); // don't care
        rtc.write_index(REG_HOURS_ALARM);
        rtc.write_data(0xC0); // don't care
        rtc.write_index(REG_B);
        rtc.write_data(REG_B_DM | REG_B_24H | REG_B_AIE);

        assert!(rtc.tick_second(), "alarm second 59 matches");
        rtc.write_index(REG_C);
        assert_ne!(rtc.read_data() & REG_C_AF, 0);
    }

    #[test]
    fn register_d_reports_battery_good() {
        let mut rtc = Rtc146818::new(sample());
        rtc.write_index(REG_D);
        assert_ne!(rtc.read_data() & REG_D_VRT, 0);
    }

    #[test]
    fn leap_day_rolls_over_correctly() {
        // 2024-02-28 23:59:59 -> 2024-02-29 (2024 is a leap year).
        let mut rtc = Rtc146818::new(RtcTime::from_unix(1_709_164_799));
        assert_eq!((rtc.time().month, rtc.time().day), (2, 28));
        assert!(!rtc.tick_second());
        assert_eq!((rtc.time().month, rtc.time().day), (2, 29));
    }

    #[test]
    fn periodic_rate_decodes_register_a_rate_select() {
        let mut rtc = Rtc146818::new(sample());
        // Default Register A is 0x26 -> RS = 6 -> 1024 Hz.
        assert_eq!(rtc.periodic_rate_hz(), Some(1024));
        // RS = 0 disables the periodic timer.
        rtc.write_index(REG_A);
        rtc.write_data(0x20);
        assert_eq!(rtc.periodic_rate_hz(), None);
        // The special low rates and a fast rate.
        rtc.write_data(0x21); // RS=1
        assert_eq!(rtc.periodic_rate_hz(), Some(256));
        rtc.write_data(0x22); // RS=2
        assert_eq!(rtc.periodic_rate_hz(), Some(128));
        rtc.write_data(0x23); // RS=3
        assert_eq!(rtc.periodic_rate_hz(), Some(8192));
        rtc.write_data(0x2F); // RS=15
        assert_eq!(rtc.periodic_rate_hz(), Some(2));
    }

    #[test]
    fn periodic_flag_latches_without_pie_but_raises_no_irq() {
        let mut rtc = Rtc146818::new(sample()); // RS=6, PIE off
        // A tick latches PF in Register C but does not assert the interrupt.
        assert!(!rtc.tick_periodic());
        rtc.write_index(REG_C);
        let c = rtc.read_data();
        assert_ne!(c & REG_C_PF, 0, "periodic flag latches regardless of PIE");
        assert_eq!(c & REG_C_IRQF, 0, "no IRQF without PIE");
    }

    #[test]
    fn periodic_interrupt_pulses_irq8_and_clears_on_reg_c_read() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let log = Arc::clone(&log);
            move |level: bool| log.lock().unwrap().push(level)
        };
        let mut rtc = Rtc146818::new(sample()); // RS=6 (1024 Hz) by default
        rtc.attach_irq(Box::new(sink));

        // Enable PIE: a tick asserts IRQ8; reading Register C clears PF/IRQF and
        // deasserts the line.
        rtc.write_index(REG_B);
        rtc.write_data(REG_B_DM | REG_B_24H | REG_B_PIE);
        assert!(rtc.tick_periodic(), "periodic interrupt fires with PIE set");
        rtc.write_index(REG_C);
        let c = rtc.read_data();
        assert_ne!(c & REG_C_PF, 0);
        assert_ne!(c & REG_C_IRQF, 0);
        assert_eq!(&*log.lock().unwrap(), &[true, false]);
    }

    #[test]
    fn periodic_tick_is_a_noop_when_rate_select_is_zero() {
        let mut rtc = Rtc146818::new(sample());
        rtc.write_index(REG_A);
        rtc.write_data(0x20); // RS = 0: periodic off
        rtc.write_index(REG_B);
        rtc.write_data(REG_B_DM | REG_B_24H | REG_B_PIE);
        assert!(!rtc.tick_periodic(), "no periodic tick when RS=0");
        rtc.write_index(REG_C);
        assert_eq!(rtc.read_data() & REG_C_PF, 0, "no PF latched when RS=0");
    }
}

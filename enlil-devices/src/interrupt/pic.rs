//! Intel 8259A Programmable Interrupt Controller emulation.
//!
//! A PC/AT exposes **two** cascaded 8259As: a *master* on ports `0x20`/`0x21`
//! and a *slave* on ports `0xA0`/`0xA1`, with the slave's `INT` output wired
//! into the master's IR2 input. The 16 ISA interrupt lines map as IRQ0-7 →
//! master, IRQ8-15 → slave. Early boot (the BIOS and the first instructions of
//! a kernel) runs in this legacy PIC mode *before* the OS switches over to the
//! I/O APIC ([`super::ioapic`]); without a PIC model the legacy `0x20`/`0xA0`
//! ports read back as open-bus and timer/keyboard interrupts never reach the
//! guest, so it stalls before it can bring up the APIC. A transparent guest
//! must find a working 8259 here exactly as on bare metal.
//!
//! This models the programming interface a guest drives:
//! - **ICW1-ICW4** — the four-byte initialization sequence (cascade vs single,
//!   vector base, cascade wiring, 8086/auto-EOI mode).
//! - **OCW1** — the Interrupt Mask Register (data port after init).
//! - **OCW2** — end-of-interrupt (specific and non-specific).
//! - **OCW3** — read-register select (IRR/ISR), poll command, special mask mode.
//! - **IRR/ISR/IMR** with fully-nested fixed-priority resolution (IRQ0 highest)
//!   and the master↔slave cascade on IR2.
//!
//! Rotating-priority OCW2 variants (`R=1`) are accepted and treated as their
//! non-rotating equivalent — PC operating systems use fixed priority with
//! non-specific EOI, so this keeps the model honest and testable rather than
//! half-implementing rotation. Lines are modelled edge-triggered (the ISA
//! default); a request latches in the IRR and is cleared on acknowledge.
//!
//! [`SharedPic`] wraps a [`DualPic`] behind an `Arc<Mutex<_>>` (mirroring
//! [`SharedInterruptController`](super::SharedInterruptController)) and exposes
//! the two PIO port adapters a guest programs through ([`PicMasterPort`] /
//! [`PicSlavePort`]) plus a `.line(irq)` level-sink factory so device models
//! assert into the PIC exactly as they do the I/O APIC.

use std::sync::{Arc, Mutex};

use crate::bus::PioDevice;
use crate::truncate::u8_of;

/// Master PIC command port (ICW1 / OCW2 / OCW3).
pub const MASTER_CMD: u16 = 0x20;
/// Master PIC data port (ICW2-4 / OCW1 / IMR).
pub const MASTER_DATA: u16 = 0x21;
/// Slave PIC command port.
pub const SLAVE_CMD: u16 = 0xA0;
/// Slave PIC data port.
pub const SLAVE_DATA: u16 = 0xA1;

/// Master IR line the slave's `INT` output is cascaded into.
const CASCADE_IRQ: u8 = 2;

/// Steps of the ICW initialization sequence a write to the data port feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitStep {
    /// Operational: data-port writes are OCW1 (the mask register).
    Ready,
    /// Next data-port write is ICW2 (the vector base).
    Icw2,
    /// Next data-port write is ICW3 (cascade wiring).
    Icw3,
    /// Next data-port write is ICW4 (mode bits).
    Icw4,
}

/// OCW3-controlled read-back state of an 8259 (kept together so [`Pic8259`]
/// stays under the struct-bool threshold).
#[derive(Debug, Clone)]
struct ReadState {
    /// Read-register select: `true` reads ISR, `false` reads IRR.
    isr_selected: bool,
    /// Special mask mode: masked in-service levels stop inhibiting lower
    /// priorities.
    special_mask: bool,
    /// Poll mode armed: the next command-port read returns a poll word and
    /// acknowledges the highest-priority request.
    poll: bool,
}

impl ReadState {
    const fn new() -> Self {
        Self {
            isr_selected: false,
            special_mask: false,
            poll: false,
        }
    }
}

/// A single Intel 8259A.
///
/// Holds the three core registers (IRR/ISR/IMR), the latched initialization
/// configuration, and the small amount of OCW3 read-back state. Priority is
/// fully-nested and fixed with IR0 highest.
#[derive(Debug, Clone)]
pub struct Pic8259 {
    /// Interrupt Request Register — lines that have latched an edge.
    irr: u8,
    /// In-Service Register — interrupts currently being serviced.
    isr: u8,
    /// Interrupt Mask Register (OCW1) — set bits inhibit the line.
    imr: u8,
    /// Vector base programmed by ICW2; the delivered vector is `base + irq`.
    vector_base: u8,
    /// Where the next data-port write lands in the ICW sequence.
    init_step: InitStep,
    /// ICW1 bit 0 (IC4): an ICW4 byte follows.
    expect_icw4: bool,
    /// ICW1 bit 1 (SNGL): single mode, so ICW3 is skipped.
    single: bool,
    /// ICW4 bit 1 (AEOI): auto-EOI — clear the ISR bit on acknowledge.
    auto_eoi: bool,
    /// OCW3 read-register / special-mask / poll state.
    read: ReadState,
}

impl Pic8259 {
    /// A freshly-reset 8259 awaiting its ICW1 initialization. Until ICW2 is
    /// written the `vector_base` is 0 and all lines are unmasked.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            irr: 0,
            isr: 0,
            imr: 0,
            vector_base: 0,
            init_step: InitStep::Ready,
            expect_icw4: false,
            single: false,
            auto_eoi: false,
            read: ReadState::new(),
        }
    }

    /// The vector base (ICW2) currently programmed.
    #[must_use]
    pub const fn vector_base(&self) -> u8 {
        self.vector_base
    }

    /// Latch an edge on local line `irq` (0-7). A masked line still latches in
    /// the IRR — masking only inhibits *delivery*, matching real hardware.
    pub const fn raise(&mut self, irq: u8) {
        if irq < 8 {
            self.irr |= 1 << irq;
        }
    }

    /// Write to this chip's command port (`0x20`/`0xA0`).
    ///
    /// Bit 4 set selects ICW1 (start initialization); bit 3 set with bit 4 clear
    /// selects OCW3; both clear selects OCW2 (the EOI group).
    pub const fn write_command(&mut self, val: u8) {
        if val & 0x10 != 0 {
            self.write_icw1(val);
        } else if val & 0x08 != 0 {
            self.write_ocw3(val);
        } else {
            self.write_ocw2(val);
        }
    }

    /// ICW1: begin initialization. Clears the mask and in-service state, resets
    /// the read pointer to the IRR, and arms the data-port ICW sequence.
    const fn write_icw1(&mut self, val: u8) {
        self.expect_icw4 = val & 0x01 != 0;
        self.single = val & 0x02 != 0;
        self.imr = 0;
        self.isr = 0;
        self.read = ReadState::new();
        self.auto_eoi = false;
        self.init_step = InitStep::Icw2;
    }

    /// OCW2: end-of-interrupt / priority commands. `EOI` (bit 5) with `SL`
    /// (bit 6) clear is a non-specific EOI (clear the highest-priority ISR bit);
    /// with `SL` set it is a specific EOI of the encoded level. Rotate (bit 7)
    /// is accepted but treated as its non-rotating equivalent.
    const fn write_ocw2(&mut self, val: u8) {
        let specific = val & 0x40 != 0;
        let eoi = val & 0x20 != 0;
        if !eoi {
            // A pure rotate/set-priority command with no EOI: nothing to clear
            // in this fixed-priority model.
            return;
        }
        if specific {
            let level = val & 0x07;
            self.isr &= !(1 << level);
        } else {
            self.clear_highest_in_service();
        }
    }

    /// Clear the highest-priority (lowest-numbered) in-service bit.
    const fn clear_highest_in_service(&mut self) {
        if self.isr != 0 {
            // `isr` is non-zero here, so `trailing_zeros()` is in 0..=7.
            let level = (self.isr.trailing_zeros() & 0x07) as u8;
            self.isr &= !(1 << level);
        }
    }

    /// OCW3: read-register select, poll command, and special-mask mode.
    const fn write_ocw3(&mut self, val: u8) {
        // Bits 1:0 = RR:RIS — `10` selects IRR, `11` selects ISR (bit 1 set).
        if val & 0x02 != 0 {
            self.read.isr_selected = val & 0x01 != 0;
        }
        // Bit 2 = P (poll command): the next command-port read polls + acks.
        if val & 0x04 != 0 {
            self.read.poll = true;
        }
        // Bits 6:5 = ESMM:SMM — when ESMM is set, SMM sets special mask mode.
        if val & 0x40 != 0 {
            self.read.special_mask = val & 0x20 != 0;
        }
    }

    /// Write to this chip's data port (`0x21`/`0xA1`): an ICW byte while
    /// initializing, otherwise OCW1 (the interrupt mask register).
    pub const fn write_data(&mut self, val: u8) {
        match self.init_step {
            InitStep::Icw2 => {
                // ICW2: the upper five bits are the vector base; the low three
                // are filled in per-line on acknowledge.
                self.vector_base = val & 0xF8;
                self.init_step = if self.single {
                    if self.expect_icw4 {
                        InitStep::Icw4
                    } else {
                        InitStep::Ready
                    }
                } else {
                    InitStep::Icw3
                };
            }
            InitStep::Icw3 => {
                // ICW3 wires the cascade (master: slave-attach bitmap; slave:
                // cascade identity). Our cascade is fixed at master IR2, so the
                // byte is accepted but needs no stored state.
                self.init_step = if self.expect_icw4 {
                    InitStep::Icw4
                } else {
                    InitStep::Ready
                };
            }
            InitStep::Icw4 => {
                self.auto_eoi = val & 0x02 != 0;
                self.init_step = InitStep::Ready;
            }
            InitStep::Ready => {
                self.imr = val;
            }
        }
    }

    /// Read this chip's command port: the IRR or ISR per the OCW3 read select,
    /// or — if a poll command was issued — the poll word (and acknowledge).
    pub const fn read_command(&mut self) -> u8 {
        if self.read.poll {
            self.read.poll = false;
            return match self.acknowledge() {
                // Poll word: bit 7 set = interrupt pending, bits 2:0 = level.
                Some(irq) => 0x80 | irq,
                None => 0x00,
            };
        }
        if self.read.isr_selected {
            self.isr
        } else {
            self.irr
        }
    }

    /// Read this chip's data port: the interrupt mask register (OCW1).
    #[must_use]
    pub const fn read_data(&self) -> u8 {
        self.imr
    }

    /// The interrupt mask register.
    #[must_use]
    pub const fn imr(&self) -> u8 {
        self.imr
    }

    /// The in-service register.
    #[must_use]
    pub const fn isr(&self) -> u8 {
        self.isr
    }

    /// The interrupt request register.
    #[must_use]
    pub const fn irr(&self) -> u8 {
        self.irr
    }

    /// The highest-priority deliverable request given an effective request
    /// register `irr` (the master folds the slave's cascade into IR2 via this).
    ///
    /// Fully-nested fixed priority: a request is deliverable only if its level
    /// is strictly higher priority (lower-numbered) than every in-service level.
    /// Special mask mode drops masked in-service levels from that blocking set.
    const fn resolve(&self, irr: u8) -> Option<u8> {
        let requests = irr & !self.imr;
        if requests == 0 {
            return None;
        }
        let blocking = if self.read.special_mask {
            self.isr & !self.imr
        } else {
            self.isr
        };
        // Highest-priority (lowest index) level currently in service; 8 = none.
        // `blocking` is non-zero in the else arm, so `trailing_zeros()` is 0..=7.
        let ceiling = if blocking == 0 {
            8
        } else {
            (blocking.trailing_zeros() & 0x07) as u8
        };
        let mut irq = 0;
        while irq < ceiling {
            if requests & (1 << irq) != 0 {
                return Some(irq);
            }
            irq += 1;
        }
        None
    }

    /// Acknowledge (INTA) the highest-priority request on this chip in
    /// isolation: move it from the IRR to the ISR and return its level. Used
    /// directly for the slave and standalone (single) configurations; the
    /// cascade is coordinated by [`DualPic`].
    const fn acknowledge(&mut self) -> Option<u8> {
        match self.resolve(self.irr) {
            Some(irq) => {
                self.irr &= !(1 << irq);
                if !self.auto_eoi {
                    self.isr |= 1 << irq;
                }
                Some(irq)
            }
            None => None,
        }
    }
}

impl Default for Pic8259 {
    fn default() -> Self {
        Self::new()
    }
}

/// The cascaded master/slave 8259A pair of a PC/AT.
///
/// Lines 0-7 drive the master, 8-15 the slave; the slave's `INT` output is wired
/// to the master's IR2, so the master sees a pending slave interrupt as a
/// (level-like) request on IR2 that is computed on demand rather than latched.
/// This is the unit a guest programs through the four legacy ports and the unit
/// device IRQ lines assert into.
#[derive(Debug, Clone)]
pub struct DualPic {
    /// Master 8259 (IRQ0-7) on ports `0x20`/`0x21`.
    pub master: Pic8259,
    /// Slave 8259 (IRQ8-15) on ports `0xA0`/`0xA1`.
    pub slave: Pic8259,
}

impl DualPic {
    /// A fresh, uninitialized master/slave pair.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            master: Pic8259::new(),
            slave: Pic8259::new(),
        }
    }

    /// Latch an edge on ISA line `irq` (0-15): 0-7 → master, 8-15 → slave. The
    /// master's IR2 cascade input is derived on demand, so raising a slave line
    /// needs no explicit master bookkeeping. Out-of-range lines are ignored.
    pub const fn raise_irq(&mut self, irq: u8) {
        if irq < 8 {
            self.master.raise(irq);
        } else if irq < 16 {
            self.slave.raise(irq - 8);
        }
    }

    /// The master's effective request register: its own IRR with IR2 reflecting
    /// whether the slave currently has a deliverable request.
    const fn master_irr(&self) -> u8 {
        let mut irr = self.master.irr;
        if self.slave.resolve(self.slave.irr).is_some() {
            irr |= 1 << CASCADE_IRQ;
        } else {
            irr &= !(1 << CASCADE_IRQ);
        }
        irr
    }

    /// The interrupt vector the CPU would receive on the next INTA, without
    /// consuming it — i.e. whether (and with what vector) `INTR` is asserted.
    #[must_use]
    pub const fn pending_vector(&self) -> Option<u8> {
        match self.master.resolve(self.master_irr()) {
            Some(CASCADE_IRQ) => match self.slave.resolve(self.slave.irr) {
                Some(s) => Some(self.slave.vector_base.wrapping_add(s)),
                None => None,
            },
            Some(m) => Some(self.master.vector_base.wrapping_add(m)),
            None => None,
        }
    }

    /// Whether `INTR` is asserted to the CPU (a deliverable request exists).
    #[must_use]
    pub const fn has_interrupt(&self) -> bool {
        self.pending_vector().is_some()
    }

    /// Acknowledge the highest-priority pending interrupt (the CPU's INTA
    /// cycle): update the in-service state on the owning chip(s) and return the
    /// delivered vector. A cascaded interrupt sets the master's IR2 in service
    /// *and* services the slave, exactly as the two INTA pulses do on hardware.
    pub const fn acknowledge(&mut self) -> Option<u8> {
        match self.master.resolve(self.master_irr()) {
            Some(CASCADE_IRQ) => {
                // Master IR2 goes in service (level input — nothing to clear in
                // the master IRR), and the slave supplies the actual vector.
                if !self.master.auto_eoi {
                    self.master.isr |= 1 << CASCADE_IRQ;
                }
                match self.slave.acknowledge() {
                    Some(s) => Some(self.slave.vector_base.wrapping_add(s)),
                    None => None,
                }
            }
            Some(_) => match self.master.acknowledge() {
                Some(m) => Some(self.master.vector_base.wrapping_add(m)),
                None => None,
            },
            None => None,
        }
    }

    /// Route a guest read of one of the four legacy PIC ports.
    pub const fn read_port(&mut self, port: u16) -> u8 {
        match port {
            MASTER_CMD => self.master.read_command(),
            MASTER_DATA => self.master.read_data(),
            SLAVE_CMD => self.slave.read_command(),
            SLAVE_DATA => self.slave.read_data(),
            _ => 0xFF,
        }
    }

    /// Route a guest write to one of the four legacy PIC ports.
    pub const fn write_port(&mut self, port: u16, val: u8) {
        match port {
            MASTER_CMD => self.master.write_command(val),
            MASTER_DATA => self.master.write_data(val),
            SLAVE_CMD => self.slave.write_command(val),
            SLAVE_DATA => self.slave.write_data(val),
            _ => {}
        }
    }
}

impl Default for DualPic {
    fn default() -> Self {
        Self::new()
    }
}

/// A thread-safe, shareable handle to one [`DualPic`].
///
/// vCPUs run on separate threads and a device fires its IRQ from whichever
/// thread owns it, so the pair is guarded by a `Mutex` — matching the `Send`
/// bound the device `IrqLine` traits require. Cloning shares the same PIC. This
/// mirrors [`SharedInterruptController`](super::SharedInterruptController) so
/// the legacy PIC and the I/O APIC are wired the same way.
#[derive(Clone)]
pub struct SharedPic(Arc<Mutex<DualPic>>);

impl SharedPic {
    /// Wrap a fresh, uninitialized master/slave pair.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(DualPic::new())))
    }

    /// Wrap an already-built pair (e.g. one initialized for the PC/AT layout).
    #[must_use]
    pub fn from_pic(pic: DualPic) -> Self {
        Self(Arc::new(Mutex::new(pic)))
    }

    /// Run `f` with exclusive access to the PIC — to read the pending vector,
    /// acknowledge an INTA, program the masks, etc.
    ///
    /// # Panics
    /// Panics if the PIC mutex has been poisoned by a prior panic while the lock
    /// was held.
    pub fn with<R>(&self, f: impl FnOnce(&mut DualPic) -> R) -> R {
        f(&mut self.0.lock().expect("PIC mutex poisoned"))
    }

    /// Build an edge-triggered level sink for ISA line `irq`, suitable for
    /// `attach_irq0` / `attach_irq_line`.
    ///
    /// A rising edge (`set_level(true)`) latches the request via
    /// [`DualPic::raise_irq`]; a falling edge does nothing (the request is
    /// edge-latched in the IRR and cleared on acknowledge), so a device that
    /// pulses true-then-false produces exactly one request. The returned closure
    /// is `Send` and owns a clone of this handle, so it outlives the borrow, and
    /// satisfies both crate-local `IrqLine` traits via their `Fn(bool)+Send`
    /// blanket impls.
    pub fn line(&self, irq: u8) -> impl Fn(bool) + Send + use<> {
        let pic = self.clone();
        move |level: bool| {
            if level {
                pic.with(|p| p.raise_irq(irq));
            }
        }
    }

    /// The master 8259's PIO port adapter (`0x20`/`0x21`) for the bus.
    #[must_use]
    pub fn master_port(&self) -> PicMasterPort {
        PicMasterPort { pic: self.clone() }
    }

    /// The slave 8259's PIO port adapter (`0xA0`/`0xA1`) for the bus.
    #[must_use]
    pub fn slave_port(&self) -> PicSlavePort {
        PicSlavePort { pic: self.clone() }
    }
}

impl Default for SharedPic {
    fn default() -> Self {
        Self::new()
    }
}

/// The master 8259's two ports (`0x20`/`0x21`) as a bus [`PioDevice`].
///
/// The 8259 registers are byte-wide, so a guest accesses them one byte at a
/// time; reads return the value in the low byte of the `u32` and writes consume
/// the low byte. The bus only routes ports within the declared range, so every
/// `port` is one of the two master ports.
pub struct PicMasterPort {
    pic: SharedPic,
}

impl PioDevice for PicMasterPort {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        u32::from(self.pic.with(|p| p.read_port(port)))
    }

    fn pio_write(&mut self, port: u16, _size: u8, data: u32) {
        self.pic.with(|p| p.write_port(port, u8_of(data)));
    }

    fn port_range(&self) -> (u16, u16) {
        (MASTER_CMD, MASTER_CMD + 2)
    }
}

/// The slave 8259's two ports (`0xA0`/`0xA1`) as a bus [`PioDevice`].
///
/// Byte-wide like [`PicMasterPort`]; the bus routes only the two slave ports
/// here.
pub struct PicSlavePort {
    pic: SharedPic,
}

impl PioDevice for PicSlavePort {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        u32::from(self.pic.with(|p| p.read_port(port)))
    }

    fn pio_write(&mut self, port: u16, _size: u8, data: u32) {
        self.pic.with(|p| p.write_port(port, u8_of(data)));
    }

    fn port_range(&self) -> (u16, u16) {
        (SLAVE_CMD, SLAVE_CMD + 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the standard PC/AT initialization a BIOS performs: master to
    /// vectors `0x20-0x27`, slave to `0x28-0x2F`, slave on master IR2, 8086 mode.
    fn init_pc_at(pic: &mut DualPic) {
        // Master.
        pic.write_port(MASTER_CMD, 0x11); // ICW1: cascade, ICW4 to follow
        pic.write_port(MASTER_DATA, 0x20); // ICW2: vector base 0x20
        pic.write_port(MASTER_DATA, 0x04); // ICW3: slave on IR2
        pic.write_port(MASTER_DATA, 0x01); // ICW4: 8086 mode
        // Slave.
        pic.write_port(SLAVE_CMD, 0x11);
        pic.write_port(SLAVE_DATA, 0x28); // ICW2: vector base 0x28
        pic.write_port(SLAVE_DATA, 0x02); // ICW3: cascade identity 2
        pic.write_port(SLAVE_DATA, 0x01); // ICW4: 8086 mode
        // Unmask everything.
        pic.write_port(MASTER_DATA, 0x00);
        pic.write_port(SLAVE_DATA, 0x00);
    }

    #[test]
    fn init_sequence_sets_vector_bases() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        assert_eq!(pic.master.vector_base(), 0x20);
        assert_eq!(pic.slave.vector_base(), 0x28);
        // ICW2 keeps only the upper five bits.
        let mut chip = Pic8259::new();
        chip.write_command(0x11);
        chip.write_data(0x27); // low three bits ignored
        assert_eq!(chip.vector_base(), 0x20);
    }

    #[test]
    fn master_irq_delivers_base_plus_line() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.raise_irq(0); // IRQ0 (PIT)
        assert_eq!(pic.pending_vector(), Some(0x20));
        assert_eq!(pic.acknowledge(), Some(0x20));
        // Now in service, IRR cleared.
        assert_eq!(pic.master.isr(), 1 << 0);
        assert_eq!(pic.master.irr(), 0);
    }

    #[test]
    fn fixed_priority_prefers_the_lower_numbered_line() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.raise_irq(3);
        pic.raise_irq(1);
        // IRQ1 outranks IRQ3.
        assert_eq!(pic.pending_vector(), Some(0x21));
    }

    #[test]
    fn masked_line_does_not_deliver_but_still_latches() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.write_port(MASTER_DATA, 1 << 4); // mask IRQ4 only
        pic.raise_irq(4);
        assert_eq!(pic.pending_vector(), None);
        assert_eq!(pic.master.irr(), 1 << 4); // latched while masked
        // Unmasking it makes the latched request deliverable.
        pic.write_port(MASTER_DATA, 0x00);
        assert_eq!(pic.pending_vector(), Some(0x24));
    }

    #[test]
    fn in_service_blocks_equal_and_lower_priority_until_eoi() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.raise_irq(1);
        assert_eq!(pic.acknowledge(), Some(0x21));
        // A lower-priority IRQ3 is blocked while IRQ1 is in service...
        pic.raise_irq(3);
        assert_eq!(pic.pending_vector(), None);
        // ...but a higher-priority IRQ0 preempts it.
        pic.raise_irq(0);
        assert_eq!(pic.pending_vector(), Some(0x20));
        // Service + EOI IRQ0, then IRQ1, then IRQ3 finally becomes deliverable.
        assert_eq!(pic.acknowledge(), Some(0x20));
        pic.write_port(MASTER_CMD, 0x20); // non-specific EOI -> clears IRQ0
        assert_eq!(pic.pending_vector(), None); // still blocked by IRQ1
        pic.write_port(MASTER_CMD, 0x20); // EOI -> clears IRQ1
        assert_eq!(pic.pending_vector(), Some(0x23));
    }

    #[test]
    fn specific_eoi_clears_the_named_level() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.raise_irq(1);
        pic.raise_irq(0);
        assert_eq!(pic.acknowledge(), Some(0x20)); // IRQ0 first (higher prio)
        // While IRQ0 is in service, IRQ1 is blocked. Specific-EOI IRQ0.
        pic.write_port(MASTER_CMD, 0x60); // specific EOI level 0
        assert_eq!(pic.master.isr(), 0);
        assert_eq!(pic.pending_vector(), Some(0x21));
    }

    #[test]
    fn cascade_routes_a_slave_irq_through_master_ir2() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.raise_irq(12); // slave line 4 (e.g. PS/2 mouse)
        // Master sees the cascade on IR2; the delivered vector is the slave's.
        assert_eq!(pic.pending_vector(), Some(0x28 + 4));
        assert_eq!(pic.acknowledge(), Some(0x2C));
        // Both chips track it in service: master IR2, slave line 4.
        assert_eq!(pic.master.isr(), 1 << CASCADE_IRQ);
        assert_eq!(pic.slave.isr(), 1 << 4);
        assert_eq!(pic.slave.irr(), 0);
    }

    #[test]
    fn cascade_eoi_requires_both_chips() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.raise_irq(8); // slave line 0 (RTC)
        assert_eq!(pic.acknowledge(), Some(0x28));
        // A master EOI alone leaves the slave's ISR set, so the slave keeps the
        // master IR2 cascade asserted but blocked — a second slave IRQ can't
        // deliver until the slave is EOI'd too.
        pic.write_port(MASTER_CMD, 0x20); // EOI master IR2
        assert_eq!(pic.master.isr(), 0);
        pic.raise_irq(9);
        // Slave line 0 still in service blocks the lower-priority line 1.
        assert_eq!(pic.pending_vector(), None);
        pic.write_port(SLAVE_CMD, 0x20); // EOI slave line 0
        assert_eq!(pic.pending_vector(), Some(0x28 + 1));
    }

    #[test]
    fn master_higher_priority_preempts_an_in_service_slave_cascade() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.raise_irq(8); // slave -> master IR2
        assert_eq!(pic.acknowledge(), Some(0x28));
        // IRQ0/IRQ1 are higher priority than IR2, so they preempt.
        pic.raise_irq(1);
        assert_eq!(pic.pending_vector(), Some(0x21));
    }

    #[test]
    fn read_register_select_exposes_irr_then_isr() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.raise_irq(0);
        pic.raise_irq(1);
        // Default read pointer is the IRR.
        assert_eq!(pic.read_port(MASTER_CMD), 0b11);
        // Acknowledge IRQ0, then select ISR via OCW3 and read it back.
        assert_eq!(pic.acknowledge(), Some(0x20));
        pic.write_port(MASTER_CMD, 0x0B); // OCW3: read ISR
        assert_eq!(pic.read_port(MASTER_CMD), 1 << 0);
        // Re-select IRR (IRQ1 still pending).
        pic.write_port(MASTER_CMD, 0x0A); // OCW3: read IRR
        assert_eq!(pic.read_port(MASTER_CMD), 1 << 1);
    }

    #[test]
    fn imr_is_read_back_through_the_data_port() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.write_port(MASTER_DATA, 0xA5);
        assert_eq!(pic.read_port(MASTER_DATA), 0xA5);
    }

    #[test]
    fn poll_command_returns_the_pending_level_and_acknowledges() {
        let mut chip = Pic8259::new();
        chip.write_command(0x11); // ICW1
        chip.write_data(0x20); // ICW2
        chip.write_data(0x04); // ICW3
        chip.write_data(0x01); // ICW4
        chip.write_data(0x00); // unmask
        chip.raise(3);
        chip.write_command(0x0C); // OCW3 poll command
        let word = chip.read_command();
        assert_eq!(word & 0x80, 0x80, "interrupt-pending bit set");
        assert_eq!(word & 0x07, 3, "poll reports level 3");
        // The poll acted as an INTA: level 3 is now in service.
        assert_eq!(chip.isr(), 1 << 3);
    }

    #[test]
    fn auto_eoi_clears_in_service_on_acknowledge() {
        let mut chip = Pic8259::new();
        chip.write_command(0x11); // ICW1, ICW4 follows
        chip.write_data(0x20); // ICW2
        chip.write_data(0x04); // ICW3
        chip.write_data(0x03); // ICW4: 8086 mode + auto-EOI
        chip.write_data(0x00); // unmask
        chip.raise(5);
        assert_eq!(chip.acknowledge(), Some(5));
        // Auto-EOI: nothing stays in service.
        assert_eq!(chip.isr(), 0);
    }

    #[test]
    fn special_mask_mode_lets_lower_priority_lines_through() {
        let mut pic = DualPic::new();
        init_pc_at(&mut pic);
        pic.raise_irq(1);
        assert_eq!(pic.acknowledge(), Some(0x21)); // IRQ1 in service
        pic.raise_irq(3); // normally blocked by in-service IRQ1
        assert_eq!(pic.pending_vector(), None);
        // Enter special mask mode and mask IRQ1: its in-service bit no longer
        // blocks the lower-priority IRQ3.
        pic.write_port(MASTER_DATA, 1 << 1); // mask IRQ1
        pic.write_port(MASTER_CMD, 0x68); // OCW3: ESMM|SMM set
        assert_eq!(pic.pending_vector(), Some(0x23));
    }

    #[test]
    fn single_mode_skips_icw3() {
        // ICW1 with SNGL (bit1) set: the data sequence is ICW2 then ICW4, no
        // ICW3, after which the data port is the mask register again.
        let mut chip = Pic8259::new();
        chip.write_command(0x13); // ICW1: single, ICW4 follows
        chip.write_data(0x40); // ICW2: vector base 0x40
        chip.write_data(0x01); // ICW4
        chip.write_data(0x0F); // OCW1: mask low four lines
        assert_eq!(chip.vector_base(), 0x40);
        assert_eq!(chip.imr(), 0x0F);
    }

    #[test]
    fn uninitialized_ports_are_inert() {
        let mut pic = DualPic::new();
        assert_eq!(pic.read_port(0x1234), 0xFF);
        pic.write_port(0x1234, 0x55); // ignored, no panic
        assert!(!pic.has_interrupt());
    }

    /// Initialize a `SharedPic` for the PC/AT layout through its port adapters,
    /// the way a guest BIOS drives the bus byte-at-a-time.
    fn init_shared(pic: &SharedPic) {
        let mut m = pic.master_port();
        let mut s = pic.slave_port();
        m.pio_write(MASTER_CMD, 1, 0x11);
        m.pio_write(MASTER_DATA, 1, 0x20);
        m.pio_write(MASTER_DATA, 1, 0x04);
        m.pio_write(MASTER_DATA, 1, 0x01);
        s.pio_write(SLAVE_CMD, 1, 0x11);
        s.pio_write(SLAVE_DATA, 1, 0x28);
        s.pio_write(SLAVE_DATA, 1, 0x02);
        s.pio_write(SLAVE_DATA, 1, 0x01);
        m.pio_write(MASTER_DATA, 1, 0x00);
        s.pio_write(SLAVE_DATA, 1, 0x00);
    }

    #[test]
    fn port_adapters_claim_the_two_byte_ranges() {
        let pic = SharedPic::new();
        assert_eq!(pic.master_port().port_range(), (0x20, 0x22));
        assert_eq!(pic.slave_port().port_range(), (0xA0, 0xA2));
    }

    #[test]
    fn shared_line_raises_and_routes_through_the_programmed_pic() {
        let pic = SharedPic::new();
        init_shared(&pic);

        // No interrupt until a device asserts.
        assert!(!pic.with(|p| p.has_interrupt()));

        // A device pulses its line (true then false, as the PIT does); the PIC
        // latches exactly one edge and asserts INTR with the master's vector.
        let irq0 = pic.line(0);
        irq0(true);
        irq0(false);
        assert_eq!(pic.with(|p| p.pending_vector()), Some(0x20));
    }

    #[test]
    fn shared_cascade_line_delivers_a_slave_vector() {
        let pic = SharedPic::new();
        init_shared(&pic);

        // ISA line 12 (slave line 4) asserts; INTR carries the slave's vector.
        pic.line(12)(true);
        assert_eq!(pic.with(|p| p.pending_vector()), Some(0x28 + 4));
    }

    #[test]
    fn guest_reads_the_mask_back_through_the_master_port_adapter() {
        let pic = SharedPic::new();
        init_shared(&pic);
        let mut m = pic.master_port();
        m.pio_write(MASTER_DATA, 1, 0xC3); // OCW1: program the mask
        assert_eq!(m.pio_read(MASTER_DATA, 1) & 0xFF, 0xC3);
    }

    #[test]
    fn masked_shared_line_delivers_nothing() {
        let pic = SharedPic::new();
        init_shared(&pic);
        // Mask IRQ0 through the bus, then assert it: no INTR.
        pic.master_port().pio_write(MASTER_DATA, 1, 0x01);
        pic.line(0)(true);
        assert!(!pic.with(|p| p.has_interrupt()));
    }
}

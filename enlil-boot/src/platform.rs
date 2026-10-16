//! The legacy PC platform a firmware guest (OVMF) expects around its CPU.
//!
//! OVMF is written for QEMU's i440FX machine, and probes it the way a BIOS
//! probes hardware: an Intel 440FX host bridge at 00:00.0, a PIIX3 ISA bridge
//! at 00:01.0 (the serial console hangs off it), a PIIX4 power-management
//! function at 00:01.3 whose ACPI timer drives every firmware delay, the CMOS
//! RAM-size registers, the local APIC at `0xFEE00000`, and the flash that
//! holds the firmware itself at the top of the 4 GiB space.
//!
//! This module models exactly that and nothing more: the PCI identities and
//! the registers firmware programs, the ACPI PM timer, a register-file local
//! APIC with no interrupt delivery, and the flash layout. It is pure, so the
//! behaviour OVMF depends on is host-tested; the run loop only routes accesses
//! here.

use crate::fwcfg::FwCfg;
use crate::ioapic::{IOAPIC_BASE, IOAPIC_BYTES, IoApic};
use crate::pci_dev::PciFunction;

/// The PCI device number of the PIIX3/PIIX4 multi-function chip.
pub const PIIX_DEVICE: u8 = 1;
/// The local APIC's MMIO base (the architectural reset value).
pub const LAPIC_BASE: u64 = 0xFEE0_0000;
/// The local APIC's MMIO window.
pub const LAPIC_BYTES: u64 = 0x1000;
/// The ACPI PM timer's frequency in Hz.
pub const PM_TIMER_HZ: u64 = 3_579_545;
/// The PM timer is 24 bits wide (as on QEMU's PIIX4).
const PM_TIMER_MASK: u64 = 0x00FF_FFFF;
/// The top of the 4 GiB space, where the firmware's code flash ends.
pub const FLASH_TOP: u64 = 1 << 32;
/// The x86 reset vector's code-segment base.
pub const RESET_CS_BASE: u64 = 0xFFFF_0000;
/// The x86 reset vector's instruction pointer.
pub const RESET_RIP: u64 = 0xFFF0;
/// The largest firmware code image accepted.
const MAX_CODE_BYTES: u64 = 16 << 20;
/// Page granularity of the flash mappings.
const PAGE: u64 = 4096;

/// `PMBA` (PM I/O base address) in the PIIX4 PM function's config space.
const PIIX4_PMBA: usize = 0x40;
/// `PMREGMISC` in the PIIX4 PM function's config space; bit 0 enables the I/O
/// space decode.
const PIIX4_PMREGMISC: usize = 0x80;
/// Size of the PM I/O register block.
const PM_IO_BYTES: u16 = 0x40;
/// Offset of the PM timer within the PM I/O block.
const PM_TMR: usize = 0x08;
/// Offset of `PM1_CNT` within the PM I/O block.
const PM_CNT: usize = 0x04;
/// `PM1_CNT.SLP_EN`.
const SLP_EN: u16 = 1 << 13;
/// Config offset of the PIIX3 `PIRQ` route registers (`PIRQA`..`PIRQD`).
const PIIX3_PIRQ_ROUTE: usize = 0x60;
/// Port of the "system control A" register (fast A20).
const PORT_SYSCTL_A: u16 = 0x92;

/// Where the two flash images sit in guest-physical space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashLayout {
    /// Guest-physical base of the variable store.
    pub vars_gpa: u64,
    /// Bytes in the variable store.
    pub vars_len: u64,
    /// Guest-physical base of the code image.
    pub code_gpa: u64,
    /// Bytes in the code image.
    pub code_len: u64,
}

/// Place the firmware flash below 4 GiB.
///
/// The code image ends at 4 GiB (the reset vector is in its last 16 bytes) and
/// the variable store sits directly below it, which is the split-flash layout
/// OVMF's `.fdf` is built for. `None` when a length is zero, not page-sized, or
/// too large, or the pair would reach below `ram_end`.
#[must_use]
pub fn flash_layout(code_len: u64, vars_len: u64, ram_end: u64) -> Option<FlashLayout> {
    let ok = |n: u64| n != 0 && n.is_multiple_of(PAGE);
    if !ok(code_len) || !ok(vars_len) || code_len > MAX_CODE_BYTES || vars_len > MAX_CODE_BYTES {
        return None;
    }
    let code_gpa = FLASH_TOP - code_len;
    let vars_gpa = code_gpa.checked_sub(vars_len)?;
    (vars_gpa >= ram_end).then_some(FlashLayout {
        vars_gpa,
        vars_len,
        code_gpa,
        code_len,
    })
}

/// The frequency the local APIC timer counts at: 1 GHz, the 1 ns bus cycle QEMU
/// presents, which is what OVMF's timer driver sizes its 10 ms tick for.
pub const LAPIC_TIMER_HZ: u64 = 1_000_000_000;

/// `SVR` bit 8: the APIC is software-enabled.
const SVR_ENABLE: u32 = 1 << 8;
/// An LVT entry's mask bit.
const LVT_MASKED: u32 = 1 << 16;
/// `LVT` timer mode bit 17: periodic (clear = one-shot).
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
/// The register offsets the model treats specially.
const REG_TPR: usize = 0x80;
const REG_EOI: usize = 0xB0;
const REG_SVR: usize = 0xF0;
const REG_ISR: usize = 0x100;
const REG_IRR: usize = 0x200;
const REG_ICR_LOW: usize = 0x300;
const REG_LVT_TIMER: usize = 0x320;
const REG_LVT_LINT0: usize = 0x350;
const REG_TIMER_INIT: usize = 0x380;
const REG_TIMER_CURRENT: usize = 0x390;
const REG_TIMER_DIVIDE: usize = 0x3E0;

/// A local APIC with a working timer.
///
/// The register file firmware programs, plus the interrupt-request / in-service
/// state that delivers the timer's vector. Inter-processor interrupts go
/// nowhere (there is one CPU), and nothing is ever left pending in the ICR.
#[derive(Debug, Clone)]
pub struct Lapic {
    regs: [u32; 0x400],
    /// Interrupt-request and in-service bitmaps, bit `v` for vector `v`.
    irr: [u32; 8],
    isr: [u32; 8],
    /// The host TSC value the timer next expires at, while it runs.
    timer_next: Option<u64>,
    tsc_hz: u64,
}

impl Lapic {
    /// A reset local APIC timed by a host clock of `tsc_hz`: ID 0, version 0x14
    /// with 6 LVT entries, everything masked, spurious vector `0xFF`
    /// (software-disabled).
    #[must_use]
    pub fn new(tsc_hz: u64) -> Self {
        let mut regs = [0u32; 0x400];
        regs[0x30 / 4] = 0x0005_0014;
        regs[0xE0 / 4] = 0xFFFF_FFFF; // DFR
        regs[REG_SVR / 4] = 0xFF;
        for lvt in [0x2F0, 0x320, 0x330, 0x340, 0x350, 0x360, 0x370] {
            regs[lvt / 4] = LVT_MASKED;
        }
        Self {
            regs,
            irr: [0; 8],
            isr: [0; 8],
            timer_next: None,
            tsc_hz: tsc_hz.max(1),
        }
    }

    /// The timer's divide value from `TIMER_DIVIDE` (bits 0, 1, 3).
    const fn divide(&self) -> u64 {
        let d = self.regs[REG_TIMER_DIVIDE / 4];
        match (d & 0b11) | ((d >> 1) & 0b100) {
            0b000 => 2,
            0b001 => 4,
            0b010 => 8,
            0b011 => 16,
            0b100 => 32,
            0b101 => 64,
            0b110 => 128,
            _ => 1,
        }
    }

    /// Host TSC ticks for `count` timer counts at the current divide value.
    fn count_to_tsc(&self, count: u32) -> u64 {
        let ticks = u128::from(count) * u128::from(self.divide()) * u128::from(self.tsc_hz)
            / u128::from(LAPIC_TIMER_HZ);
        u64::try_from(ticks).unwrap_or(u64::MAX).max(1)
    }

    /// Whether the `LVT` timer entry delivers interrupts.
    const fn timer_unmasked(&self) -> bool {
        self.regs[REG_LVT_TIMER / 4] & LVT_MASKED == 0
    }

    /// Whether the timer reloads when it expires.
    const fn timer_periodic(&self) -> bool {
        self.regs[REG_LVT_TIMER / 4] & LVT_TIMER_PERIODIC != 0
    }

    /// The timer's current count at host time `now`.
    fn current_count(&self, now: u64) -> u32 {
        let Some(next) = self.timer_next else {
            return 0;
        };
        let left_tsc = u128::from(next.saturating_sub(now));
        let counts = left_tsc * u128::from(LAPIC_TIMER_HZ)
            / u128::from(self.tsc_hz)
            / u128::from(self.divide());
        u32::try_from(counts).unwrap_or(u32::MAX)
    }

    /// Mark `vector` requested (an interrupt arrives from an I/O APIC pin or a
    /// self-IPI).
    pub fn raise(&mut self, vector: u8) {
        self.irr[usize::from(vector >> 5)] |= 1 << (vector & 31);
    }

    /// The highest set vector in `bitmap`.
    fn highest(bitmap: &[u32; 8]) -> Option<u8> {
        (0..8u8).rev().find_map(|word| {
            let bits = bitmap[usize::from(word)];
            (bits != 0).then(|| word * 32 + (31 - u8::try_from(bits.leading_zeros()).unwrap_or(0)))
        })
    }

    /// Whether the APIC is software-enabled (`SVR` bit 8).
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.regs[REG_SVR / 4] & SVR_ENABLE != 0
    }

    /// Whether legacy-PIC interrupts reach the CPU: always while the APIC is
    /// off, and through `LINT0` (virtual wire) once it is on.
    #[must_use]
    pub const fn extint_enabled(&self) -> bool {
        !self.enabled() || self.regs[REG_LVT_LINT0 / 4] & LVT_MASKED == 0
    }

    /// The vector the APIC would deliver now: the highest requested vector whose
    /// priority class beats both the task priority and anything in service.
    #[must_use]
    pub fn pending_vector(&self) -> Option<u8> {
        if !self.enabled() {
            return None;
        }
        let v = Self::highest(&self.irr)?;
        let in_service = Self::highest(&self.isr).map_or(0, |s| s & 0xF0);
        let ppr = in_service.max(u8::try_from(self.regs[REG_TPR / 4] & 0xF0).unwrap_or(0));
        (v & 0xF0 > ppr).then_some(v)
    }

    /// The CPU accepts the pending vector: it moves from requested to in
    /// service.
    pub fn ack(&mut self) -> Option<u8> {
        let v = self.pending_vector()?;
        self.irr[usize::from(v >> 5)] &= !(1 << (v & 31));
        self.isr[usize::from(v >> 5)] |= 1 << (v & 31);
        Some(v)
    }

    /// End of interrupt: the highest in-service vector completes, and is
    /// returned (an I/O APIC retires level-triggered interrupts on it).
    fn eoi(&mut self) -> Option<u8> {
        let v = Self::highest(&self.isr)?;
        self.isr[usize::from(v >> 5)] &= !(1 << (v & 31));
        Some(v)
    }

    /// Run the timer to host time `now`, requesting its vector if it expired
    /// while unmasked. A periodic timer that fell behind coalesces the missed
    /// periods into the one interrupt.
    pub fn poll(&mut self, now: u64) {
        let Some(due) = self.timer_next else {
            return;
        };
        if now < due {
            return;
        }
        if self.timer_unmasked() {
            let vector = (self.regs[REG_LVT_TIMER / 4] & 0xFF) as u8;
            self.raise(vector);
        }
        self.timer_next = if self.timer_periodic() {
            let period = self.count_to_tsc(self.regs[REG_TIMER_INIT / 4]);
            let missed = (now - due) / period + 1;
            Some(due + missed * period)
        } else {
            None
        };
    }

    /// The host TSC value the timer next needs servicing at: only while it can
    /// raise an interrupt.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        self.timer_next.filter(|_| self.timer_unmasked())
    }

    /// A `size`-byte register read at `off` within the window, at host time
    /// `now` (the timer's current count depends on it).
    #[must_use]
    pub fn read(&self, off: u64, size: u8, now: u64) -> u64 {
        let Ok(reg) = usize::try_from(off & !3) else {
            return crate::insn::mask_to(u64::MAX, size);
        };
        let dword = match reg {
            REG_TIMER_CURRENT => self.current_count(now),
            r if (REG_ISR..REG_ISR + 0x80).contains(&r) => self.isr[(r - REG_ISR) / 16],
            r if (REG_IRR..REG_IRR + 0x80).contains(&r) => self.irr[(r - REG_IRR) / 16],
            r => match self.regs.get(r / 4) {
                Some(&v) => v,
                None => return crate::insn::mask_to(u64::MAX, size),
            },
        };
        crate::insn::mask_to(u64::from(dword) >> (8 * (off & 3)), size)
    }

    /// A `size`-byte register write of `value` at `off` within the window at
    /// host time `now`. Read-only registers ignore it; a write that is not an
    /// aligned dword is ignored (the hardware requires 32-bit accesses).
    ///
    /// Returns the vector an end-of-interrupt write retired, if it did.
    pub fn write(&mut self, off: u64, size: u8, value: u64, now: u64) -> Option<u8> {
        if size != 4 || off & 3 != 0 {
            return None;
        }
        let Ok(reg) = usize::try_from(off) else {
            return None;
        };
        let mut retired = None;
        #[allow(clippy::cast_possible_truncation)] // a 32-bit register write
        let v = value as u32;
        match reg {
            // ID, version, the read-only status registers and the timer's
            // current count.
            0x20 | 0x30 | 0x90..=0xA0 | 0xD0 | REG_ISR..=0x270 | REG_TIMER_CURRENT => {}
            REG_EOI => retired = self.eoi(),
            REG_ICR_LOW => {
                // Delivery status always reads idle. A fixed-delivery IPI to
                // self (shorthand 01) is the one IPI that can land.
                self.regs[reg / 4] = v & !(1 << 12);
                if (v >> 18) & 3 == 1 && (v >> 8).trailing_zeros() >= 3 {
                    self.raise((v & 0xFF) as u8);
                }
            }
            REG_TIMER_INIT => {
                self.regs[reg / 4] = v;
                self.timer_next = (v != 0).then(|| now.saturating_add(self.count_to_tsc(v)));
            }
            _ => {
                if let Some(slot) = self.regs.get_mut(reg / 4) {
                    *slot = v;
                }
            }
        }
        retired
    }
}

/// What a guest write to the platform's I/O space asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformEvent {
    /// Nothing for the run loop to act on.
    None,
    /// The guest entered the ACPI soft-off state (S5): power the machine off.
    PowerOff,
}

/// The i440FX/PIIX platform devices around the CPU.
#[derive(Debug, Clone)]
pub struct Platform {
    host: PciFunction,
    isa: PciFunction,
    pm: PciFunction,
    lapic: Lapic,
    ioapic: IoApic,
    /// The firmware-configuration device, once the machine has one to offer.
    fwcfg: Option<FwCfg>,
    sysctl_a: u8,
    pm_io: [u8; PM_IO_BYTES as usize],
    /// The host TSC value the PM timer counts from (set on first use).
    epoch: Option<u64>,
    tsc_hz: u64,
    ram_bytes: u64,
}

impl Platform {
    /// The platform for a machine with `ram_bytes` of RAM, timed by a host
    /// clock of `tsc_hz`.
    #[must_use]
    pub fn new(tsc_hz: u64, ram_bytes: u64) -> Self {
        // Intel 82441FX host bridge: the shadow-RAM (PAM) and SMRAM registers
        // are programmed by firmware and must hold what was written.
        let mut host = PciFunction::new(0x8086, 0x1237, [0x06, 0x00, 0x00], 2, (0x1AF4, 0x1100));
        host.set_writable(0x50, 0x30);
        // PIIX3 ISA bridge: multi-function; PIRQ routing and misc registers.
        let mut isa = PciFunction::new(0x8086, 0x7000, [0x06, 0x01, 0x00], 0, (0x1AF4, 0x1100));
        isa.preset(0x0E, &[0x80]);
        isa.set_writable(0x40, 0xC0);
        // The PCI interrupt routing registers reset to "disabled" (bit 7 set).
        isa.preset(PIIX3_PIRQ_ROUTE, &[0x80; 4]);
        // PIIX4 power management: the I/O base, enable bit and SMBus base.
        let mut pm = PciFunction::new(0x8086, 0x7113, [0x06, 0x80, 0x00], 3, (0x1AF4, 0x1100));
        pm.preset(PIIX4_PMBA, &[0x01]);
        pm.set_writable(PIIX4_PMBA, 4);
        pm.preset(PIIX4_PMBA, &[0x01]); // the I/O-space indicator bit stays set
        pm.set_writable(0x44, 4);
        pm.set_writable(PIIX4_PMREGMISC, 1);
        pm.set_writable(0x90, 4);
        pm.set_interrupt(0, 0);
        Self {
            host,
            isa,
            pm,
            lapic: Lapic::new(tsc_hz),
            ioapic: IoApic::new(),
            fwcfg: None,
            sysctl_a: 0,
            pm_io: [0; PM_IO_BYTES as usize],
            epoch: None,
            tsc_hz: tsc_hz.max(1),
            ram_bytes,
        }
    }

    /// Give the machine a `fw_cfg` device (ports `0x510`/`0x511`).
    pub fn set_fwcfg(&mut self, fwcfg: FwCfg) {
        self.fwcfg = Some(fwcfg);
    }

    /// The `fw_cfg` device, if the guest has one.
    #[must_use]
    pub const fn fwcfg(&self) -> Option<&FwCfg> {
        self.fwcfg.as_ref()
    }

    /// The `fw_cfg` device, mutably (to serve a DMA request).
    pub const fn fwcfg_mut(&mut self) -> Option<&mut FwCfg> {
        self.fwcfg.as_mut()
    }

    /// The local APIC.
    #[must_use]
    pub const fn lapic(&self) -> &Lapic {
        &self.lapic
    }

    /// The local APIC, mutably.
    pub const fn lapic_mut(&mut self) -> &mut Lapic {
        &mut self.lapic
    }

    /// The offset within the local APIC window if `gpa` is inside it.
    #[must_use]
    pub fn lapic_offset(gpa: u64) -> Option<u64> {
        gpa.checked_sub(LAPIC_BASE).filter(|&o| o < LAPIC_BYTES)
    }

    /// The offset within the I/O APIC window if `gpa` is inside it.
    #[must_use]
    pub fn ioapic_offset(gpa: u64) -> Option<u64> {
        gpa.checked_sub(IOAPIC_BASE).filter(|&o| o < IOAPIC_BYTES)
    }

    /// A `size`-byte local APIC register write; an end of interrupt is
    /// broadcast to the I/O APIC so level-triggered pins become deliverable
    /// again.
    pub fn lapic_write(&mut self, off: u64, size: u8, value: u64, now: u64) {
        if let Some(vector) = self.lapic.write(off, size, value, now) {
            self.ioapic.eoi(vector, &mut self.lapic);
        }
    }

    /// A `size`-byte I/O APIC register read.
    #[must_use]
    pub fn ioapic_read(&self, off: u64, size: u8) -> u64 {
        self.ioapic.read(off, size)
    }

    /// A `size`-byte I/O APIC register write (an unmask can deliver at once).
    pub fn ioapic_write(&mut self, off: u64, size: u8, value: u64) {
        self.ioapic.write(off, size, value, &mut self.lapic);
    }

    /// Drive I/O APIC input `pin` (a global system interrupt) to `level`.
    pub fn set_gsi(&mut self, pin: u8, level: bool) {
        self.ioapic.set_irq(pin, level, &mut self.lapic);
    }

    /// The platform PCI function at `device`/`function` on bus 0, if any.
    pub const fn function(&mut self, device: u8, function: u8) -> Option<&mut PciFunction> {
        match (device, function) {
            (0, 0) => Some(&mut self.host),
            (PIIX_DEVICE, 0) => Some(&mut self.isa),
            (PIIX_DEVICE, 3) => Some(&mut self.pm),
            _ => None,
        }
    }

    /// The legacy `IRQ` a PCI device's interrupt pin reaches through the PIIX3
    /// router, if firmware routed it.
    ///
    /// PCI `INTx` lines are swizzled across the four `PIRQ` inputs by device
    /// number (`INTA#` of device `n` is `PIRQ[(n + pin - 1) % 4]`), and each input
    /// goes to the ISA `IRQ` its route register names; bit 7 disables it. This is
    /// what firmware programs and then writes into each device's interrupt-line
    /// register, so a device must raise exactly this line.
    #[must_use]
    pub fn pirq_irq(&self, slot: u8, pin: u8) -> Option<u8> {
        let input = usize::from(slot.wrapping_add(pin).wrapping_sub(1) & 3);
        let route = self.isa.read(PIIX3_PIRQ_ROUTE + input, 1);
        let irq = u8::try_from(route & 0x0F).ok()?;
        // Bit 7 disables the route; IRQ 0, 1, 2, 8 and 13 are not routable.
        (route & 0x80 == 0 && !matches!(irq, 0..=2 | 8 | 13)).then_some(irq)
    }

    /// The PM I/O block's base port, once firmware has programmed `PMBA` and set
    /// `PMREGMISC.PMIOSE`. As on QEMU's PIIX4, the block decodes on that bit
    /// alone: the PCI command register's I/O enable does not gate it, and OVMF
    /// relies on that.
    #[must_use]
    pub fn pm_base(&self) -> Option<u16> {
        let base = u16::try_from(self.pm.read(PIIX4_PMBA, 4) & 0xFFC0).ok()?;
        let enabled = self.pm.read(PIIX4_PMREGMISC, 1) & 1 != 0;
        (enabled && base != 0).then_some(base)
    }

    /// The 24-bit ACPI PM timer value at host time `now` (a TSC reading).
    pub fn pm_timer(&mut self, now: u64) -> u32 {
        let epoch = *self.epoch.get_or_insert(now);
        let ticks = u128::from(now.saturating_sub(epoch)) * u128::from(PM_TIMER_HZ)
            / u128::from(self.tsc_hz);
        #[allow(clippy::cast_possible_truncation)] // masked to 24 bits
        let t = (ticks as u64 & PM_TIMER_MASK) as u32;
        t
    }

    /// Whether the platform owns I/O port `port`.
    #[must_use]
    pub fn claims(&self, port: u16) -> bool {
        (self.fwcfg.is_some() && FwCfg::claims(port))
            || port == PORT_SYSCTL_A
            || self
                .pm_base()
                .is_some_and(|b| port >= b && port < b + PM_IO_BYTES)
    }

    /// An `IN` of `size` bytes from a claimed `port` at host time `now`.
    pub fn io_read(&mut self, port: u16, size: u8, now: u64) -> u32 {
        if let Some(fw) = self.fwcfg.as_mut()
            && FwCfg::claims(port)
        {
            return fw.io_read(port, size);
        }
        if port == PORT_SYSCTL_A {
            return u32::from(self.sysctl_a);
        }
        let Some(base) = self.pm_base() else {
            return u32::MAX;
        };
        let off = usize::from(port - base);
        if (PM_TMR..PM_TMR + 4).contains(&off) {
            let v = self.pm_timer(now) >> (8 * (off - PM_TMR));
            return match size {
                1 => v & 0xFF,
                2 => v & 0xFFFF,
                _ => v,
            };
        }
        let mut v = 0u32;
        for i in 0..usize::from(size) {
            let byte = self.pm_io.get(off + i).copied().unwrap_or(0xFF);
            v |= u32::from(byte) << (8 * i);
        }
        v
    }

    /// An `OUT` of `size` bytes of `val` to a claimed `port`.
    pub fn io_write(&mut self, port: u16, size: u8, val: u32) -> PlatformEvent {
        if let Some(fw) = self.fwcfg.as_mut()
            && FwCfg::claims(port)
        {
            fw.io_write(port, size, val);
            return PlatformEvent::None;
        }
        if port == PORT_SYSCTL_A {
            self.sysctl_a = val.to_le_bytes()[0];
            return PlatformEvent::None;
        }
        let Some(base) = self.pm_base() else {
            return PlatformEvent::None;
        };
        let off = usize::from(port - base);
        for i in 0..usize::from(size) {
            let Some(slot) = self.pm_io.get_mut(off + i) else {
                continue;
            };
            // PMSTS (0..2) and GPSTS (0x0C..0x0E) are write-1-to-clear.
            let w1c = off + i < 2 || (0x0C..0x0E).contains(&(off + i));
            let byte = val.to_le_bytes()[i];
            *slot = if w1c { *slot & !byte } else { byte };
        }
        if off <= PM_CNT && PM_CNT < off + usize::from(size) {
            let cnt = u16::from_le_bytes([self.pm_io[PM_CNT], self.pm_io[PM_CNT + 1]]);
            // S5 (soft-off) is sleep type 0 on this platform.
            if cnt & SLP_EN != 0 && (cnt >> 10).trailing_zeros() >= 3 {
                return PlatformEvent::PowerOff;
            }
        }
        PlatformEvent::None
    }

    /// The platform's CMOS RAM-size registers; `None` for any other index.
    ///
    /// Firmware sizes memory from these: base memory (640 KiB), the extended
    /// memory below 16 MiB, and the RAM above 16 MiB in 64 KiB units.
    #[must_use]
    pub fn cmos(&self, index: u8) -> Option<u8> {
        let above_16m = self.ram_bytes.saturating_sub(16 << 20) >> 16;
        let above_16m = u16::try_from(above_16m).unwrap_or(u16::MAX);
        Some(match index & 0x7F {
            0x15 => 0x80,        // 640 KiB, low byte
            0x16 => 0x02,        // 640 KiB, high byte
            0x18 | 0x31 => 0x3C, // 15 MiB of extended memory, high byte
            0x34 => above_16m.to_le_bytes()[0],
            0x35 => above_16m.to_le_bytes()[1],
            // 15 MiB of extended memory (low byte is zero); nothing above 4 GiB.
            0x17 | 0x30 | 0x5B..=0x5D => 0,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn programmed(p: &mut Platform) {
        // What OVMF does: PMBA = 0xB000, enable decode and I/O space.
        let pm = p.function(PIIX_DEVICE, 3).unwrap();
        pm.write(0x40, 4, 0xB000);
        pm.write(0x80, 1, 1);
    }

    #[test]
    fn pci_interrupts_follow_the_piix3_routing_firmware_programs() {
        let mut p = Platform::new(1_000_000_000, 256 * MIB);
        // Reset: every route is disabled.
        assert_eq!(p.pirq_irq(3, 1), None);
        // What OVMF writes: PIRQA/B -> IRQ 10, PIRQC/D -> IRQ 11.
        let isa = p.function(PIIX_DEVICE, 0).unwrap();
        for (i, irq) in [10u32, 10, 11, 11].into_iter().enumerate() {
            isa.write(0x60 + i, 1, irq);
        }
        // INTA# of device 3 lands on PIRQD (device number swizzled), device 4 on PIRQA.
        assert_eq!(p.pirq_irq(3, 1), Some(11));
        assert_eq!(p.pirq_irq(4, 1), Some(10));
        assert_eq!(p.pirq_irq(5, 1), Some(10));
        assert_eq!(p.pirq_irq(6, 1), Some(11));
        // INTB# of device 3 is PIRQA.
        assert_eq!(p.pirq_irq(3, 2), Some(10));
        // A disabled route (bit 7) and an unroutable IRQ both mean "no interrupt".
        let isa = p.function(PIIX_DEVICE, 0).unwrap();
        isa.write(0x63, 1, 0x8B);
        assert_eq!(p.pirq_irq(3, 1), None);
        let isa = p.function(PIIX_DEVICE, 0).unwrap();
        isa.write(0x63, 1, 2);
        assert_eq!(p.pirq_irq(3, 1), None);
    }

    #[test]
    fn pm_io_decodes_without_the_pci_command_io_bit() {
        // OVMF programs PMBA and PMREGMISC but never sets the command register.
        let mut p = Platform::new(1_000_000_000, 256 * MIB);
        programmed(&mut p);
        assert_eq!(p.function(PIIX_DEVICE, 3).unwrap().command() & 1, 0);
        assert_eq!(p.pm_base(), Some(0xB000));
    }

    #[test]
    fn flash_sits_below_4gib_with_the_variable_store_under_the_code() {
        let l = flash_layout(0x37_C000, 0x8_4000, 256 * MIB).unwrap();
        assert_eq!(l.code_gpa + l.code_len, FLASH_TOP);
        assert_eq!(l.vars_gpa + l.vars_len, l.code_gpa);
        assert_eq!(l.vars_gpa, 0xFFC0_0000, "the 4 MiB split layout");
        assert_eq!(l.code_gpa, 0xFFC8_4000);
    }

    #[test]
    fn flash_layout_rejects_bad_sizes() {
        assert_eq!(flash_layout(0, 4096, 0), None);
        assert_eq!(flash_layout(4096, 0, 0), None);
        assert_eq!(flash_layout(4097, 4096, 0), None);
        assert_eq!(flash_layout(32 * MIB, 4096, 0), None);
        // Reaching down into RAM is refused.
        assert_eq!(flash_layout(8 * MIB, 8 * MIB, FLASH_TOP - 4 * MIB), None);
    }

    #[test]
    fn the_chipset_identifies_as_an_i440fx_with_piix() {
        let mut p = Platform::new(1_000_000_000, 256 * MIB);
        assert_eq!(p.function(0, 0).unwrap().read(0, 4), 0x1237_8086);
        assert_eq!(p.function(1, 0).unwrap().read(0, 4), 0x7000_8086);
        assert_eq!(p.function(1, 3).unwrap().read(0, 4), 0x7113_8086);
        // The ISA bridge is multi-function so firmware looks at function 3.
        assert_eq!(p.function(1, 0).unwrap().read(0x0E, 1), 0x80);
        assert!(p.function(1, 1).is_none());
        assert!(p.function(2, 0).is_none());
    }

    #[test]
    fn pm_io_decodes_only_after_firmware_programs_it() {
        let mut p = Platform::new(1_000_000_000, 256 * MIB);
        assert_eq!(p.pm_base(), None);
        assert!(!p.claims(0xB008));
        programmed(&mut p);
        assert_eq!(p.pm_base(), Some(0xB000));
        assert!(p.claims(0xB008));
        assert!(!p.claims(0xB040));
        assert!(p.claims(0x92));
    }

    #[test]
    fn the_pm_timer_ticks_at_3_579_545_hz_and_wraps_at_24_bits() {
        let mut p = Platform::new(1_000_000_000, 256 * MIB);
        programmed(&mut p);
        let t0 = p.io_read(0xB008, 4, 5_000);
        assert_eq!(t0, 0, "counts from the first read");
        // One second of host time is 3,579,545 timer ticks.
        let t1 = p.io_read(0xB008, 4, 5_000 + 1_000_000_000);
        assert_eq!(t1, 3_579_545);
        // 24-bit wrap: 2^24 ticks ~ 4.687 s.
        let t2 = p.io_read(0xB008, 4, 5_000 + 5_000_000_000);
        assert_eq!(u64::from(t2), (5 * 3_579_545) & 0xFF_FFFF);
        // A byte read of the timer's low byte is consistent with the dword.
        let low = p.io_read(0xB008, 1, 5_000 + 1_000_000_000);
        assert_eq!(low, 0x99, "3,579,545 is 0x369E99");
    }

    #[test]
    fn writing_soft_off_to_pm1_cnt_powers_the_machine_off() {
        let mut p = Platform::new(1_000_000_000, 256 * MIB);
        programmed(&mut p);
        // SLP_TYP = 1 (S1-ish) is not a power-off.
        assert_eq!(
            p.io_write(0xB004, 2, u32::from(SLP_EN) | (1 << 10)),
            PlatformEvent::None
        );
        // SLP_TYP = 0 with SLP_EN is S5.
        assert_eq!(
            p.io_write(0xB004, 2, u32::from(SLP_EN)),
            PlatformEvent::PowerOff
        );
        // Without SLP_EN nothing happens.
        assert_eq!(p.io_write(0xB004, 2, 1), PlatformEvent::None);
    }

    #[test]
    fn pm_status_bits_clear_when_written_with_ones() {
        let mut p = Platform::new(1_000_000_000, 256 * MIB);
        programmed(&mut p);
        // Registers other than the status ones just hold their value.
        p.io_write(0xB002, 2, 0x0123);
        assert_eq!(p.io_read(0xB002, 2, 0), 0x0123);
        p.io_write(0xB000, 2, 0xFFFF);
        assert_eq!(p.io_read(0xB000, 2, 0), 0, "status reads zero after W1C");
    }

    #[test]
    fn fw_cfg_answers_only_once_the_machine_has_one() {
        use crate::fwcfg::MachineInfo;
        let mut p = Platform::new(1_000_000_000, 256 * MIB);
        assert!(
            !p.claims(0x510) && !p.claims(0x511),
            "absent: reads fall through to all-ones"
        );
        p.set_fwcfg(FwCfg::new(MachineInfo {
            uuid: [7; 16],
            ram_bytes: 256 * MIB,
            cpus: 1,
        }));
        assert!(p.claims(0x510) && p.claims(0x511));
        p.io_write(0x510, 2, 0);
        let sig: Vec<u8> = (0..4)
            .map(|_| u8::try_from(p.io_read(0x511, 1, 0)).unwrap())
            .collect();
        assert_eq!(sig, b"QEMU");
        // The platform's own ports are unaffected.
        p.io_write(0x92, 1, 0x02);
        assert_eq!(p.io_read(0x92, 1, 0), 0x02);
    }

    #[test]
    fn system_control_a_holds_its_value() {
        let mut p = Platform::new(1_000_000_000, 256 * MIB);
        p.io_write(0x92, 1, 0x02);
        assert_eq!(p.io_read(0x92, 1, 0), 0x02);
    }

    #[test]
    fn cmos_reports_the_ram_size_in_the_registers_firmware_reads() {
        let p = Platform::new(1_000_000_000, 256 * MIB);
        // (256 - 16) MiB in 64 KiB units = 3840 = 0x0F00.
        assert_eq!(p.cmos(0x34), Some(0x00));
        assert_eq!(p.cmos(0x35), Some(0x0F));
        assert_eq!(p.cmos(0x15), Some(0x80));
        assert_eq!(p.cmos(0x16), Some(0x02));
        assert_eq!(p.cmos(0x5B), Some(0));
        assert_eq!(
            p.cmos(0x09),
            None,
            "date/time registers are not the platform's"
        );
    }

    const HZ: u64 = 1_000_000_000; // a 1 GHz host clock: 1 TSC tick = 1 timer count

    #[test]
    fn lapic_reads_as_a_reset_apic_with_read_only_identity() {
        let mut l = Lapic::new(HZ);
        assert_eq!(l.read(0x20, 4, 0), 0, "APIC ID 0");
        assert_eq!(l.read(0x30, 4, 0), 0x0005_0014);
        assert_eq!(l.read(0xF0, 4, 0), 0xFF);
        assert_eq!(l.read(0x320, 4, 0), 0x10000, "timer LVT starts masked");
        // The ID and version registers are read-only.
        l.write(0x20, 4, 0x0300_0000, 0);
        l.write(0x30, 4, 0, 0);
        assert_eq!(l.read(0x20, 4, 0), 0);
        assert_eq!(l.read(0x30, 4, 0), 0x0005_0014);
        // The spurious-vector register and TPR are writable.
        l.write(0xF0, 4, 0x1FF, 0);
        l.write(0x80, 4, 0x20, 0);
        assert_eq!(l.read(0xF0, 4, 0), 0x1FF);
        assert_eq!(l.read(0x80, 4, 0), 0x20);
        // An IPI is never left pending: delivery status stays idle.
        l.write(0x300, 4, 0x000C_4500 | (1 << 12), 0);
        assert_eq!(l.read(0x300, 4, 0) & (1 << 12), 0);
        // Sub-dword reads pick the right byte; sub-dword writes are ignored.
        assert_eq!(l.read(0x31, 1, 0), 0x00);
        assert_eq!(l.read(0x32, 1, 0), 0x05);
        l.write(0xF0, 1, 0, 0);
        assert_eq!(l.read(0xF0, 4, 0), 0x1FF);
        // Past the window reads as ones.
        assert_eq!(l.read(0x1000, 4, 0), 0xFFFF_FFFF);
    }

    /// Program the timer like OVMF: periodic, vector 0x20, divide by 1,
    /// `count` timer counts, APIC software-enabled, at host time `now`.
    fn start_periodic(l: &mut Lapic, count: u32, now: u64) {
        l.write(0xF0, 4, 0x1FF, now);
        l.write(0x3E0, 4, 0xB, now); // divide by 1
        l.write(0x320, 4, 0x20020, now); // periodic, vector 0x20, unmasked
        l.write(0x380, 4, u64::from(count), now);
    }

    #[test]
    fn a_periodic_timer_raises_its_vector_every_period() {
        let mut l = Lapic::new(HZ);
        start_periodic(&mut l, 10_000_000, 1_000); // 10 ms at 1 GHz
        assert_eq!(l.next_deadline(), Some(1_000 + 10_000_000));
        l.poll(5_000_000);
        assert_eq!(l.pending_vector(), None, "not due yet");
        l.poll(10_001_000);
        assert_eq!(l.pending_vector(), Some(0x20));
        assert_eq!(l.ack(), Some(0x20));
        assert_eq!(l.pending_vector(), None, "in service until EOI");
        // It reloads on the 10 ms grid.
        assert_eq!(l.next_deadline(), Some(1_000 + 20_000_000));
        l.poll(20_001_000);
        // The next tick is held back by the unfinished one until EOI.
        assert_eq!(l.pending_vector(), None);
        l.write(0xB0, 4, 0, 0);
        assert_eq!(l.pending_vector(), Some(0x20));
    }

    #[test]
    fn a_late_periodic_timer_coalesces_missed_periods() {
        let mut l = Lapic::new(HZ);
        start_periodic(&mut l, 1_000_000, 0); // 1 ms
        l.poll(5_500_000); // five periods late
        assert_eq!(l.ack(), Some(0x20));
        let next = l.next_deadline().unwrap();
        assert!(
            next > 5_500_000 && next <= 6_500_000,
            "back on the 1 ms grid: {next}"
        );
    }

    #[test]
    fn a_one_shot_timer_fires_once_and_stops() {
        let mut l = Lapic::new(HZ);
        l.write(0xF0, 4, 0x1FF, 0);
        l.write(0x3E0, 4, 0xB, 0);
        l.write(0x320, 4, 0x30, 0); // one-shot, vector 0x30
        l.write(0x380, 4, 1000, 0);
        l.poll(2000);
        assert_eq!(l.ack(), Some(0x30));
        assert_eq!(l.next_deadline(), None);
        assert_eq!(
            l.read(0x390, 4, 5000),
            0,
            "current count is zero once expired"
        );
    }

    #[test]
    fn a_masked_timer_never_interrupts_and_asks_for_no_deadline() {
        let mut l = Lapic::new(HZ);
        l.write(0xF0, 4, 0x1FF, 0);
        l.write(0x3E0, 4, 0xB, 0);
        l.write(0x320, 4, 0x20020 | 0x10000, 0); // masked
        l.write(0x380, 4, 1000, 0);
        assert_eq!(l.next_deadline(), None);
        l.poll(10_000);
        assert_eq!(l.pending_vector(), None);
    }

    #[test]
    fn the_divide_register_scales_the_period_and_the_current_count() {
        let mut l = Lapic::new(HZ);
        l.write(0xF0, 4, 0x1FF, 0);
        l.write(0x320, 4, 0x20020, 0);
        l.write(0x3E0, 4, 0x3, 0); // divide by 16
        l.write(0x380, 4, 1000, 100);
        assert_eq!(l.next_deadline(), Some(100 + 16_000));
        // Halfway through, half the counts remain.
        assert_eq!(l.read(0x390, 4, 100 + 8_000), 500);
        // Divide code 0b1011 (bit 3 set, bits 1:0 = 11) is divide by 1.
        l.write(0x3E0, 4, 0xB, 0);
        l.write(0x380, 4, 1000, 0);
        assert_eq!(l.next_deadline(), Some(1000));
    }

    #[test]
    fn the_timer_follows_a_host_clock_that_is_not_1_ghz() {
        // A 3 GHz host: 10 ms is 30 million TSC ticks.
        let mut l = Lapic::new(3_000_000_000);
        start_periodic(&mut l, 10_000_000, 0);
        assert_eq!(l.next_deadline(), Some(30_000_000));
    }

    #[test]
    fn interrupts_respect_the_enable_bit_task_priority_and_in_service_state() {
        let mut l = Lapic::new(HZ);
        l.raise(0x41);
        assert_eq!(
            l.pending_vector(),
            None,
            "software-disabled APIC delivers nothing"
        );
        l.write(0xF0, 4, 0x1FF, 0);
        assert_eq!(l.pending_vector(), Some(0x41));
        // A task priority at or above the vector's class blocks it.
        l.write(0x80, 4, 0x40, 0);
        assert_eq!(l.pending_vector(), None);
        l.write(0x80, 4, 0x30, 0);
        assert_eq!(l.ack(), Some(0x41));
        // While 0x41 is in service only a higher class gets through.
        l.raise(0x4F);
        l.raise(0x52);
        assert_eq!(l.pending_vector(), Some(0x52));
        assert_eq!(l.ack(), Some(0x52));
        assert_eq!(l.pending_vector(), None, "0x4F is the same class as 0x41");
        // EOI retires the highest in-service vector (0x52), then 0x41.
        l.write(0xB0, 4, 0, 0);
        l.write(0xB0, 4, 0, 0);
        assert_eq!(l.pending_vector(), Some(0x4F));
        // The request and in-service bitmaps read back through their registers.
        assert_eq!(
            l.read(0x200 + 2 * 16, 4, 0) & (1 << 15),
            1 << 15,
            "IRR bit for 0x4F"
        );
    }

    #[test]
    fn a_self_ipi_lands_but_other_ipis_go_nowhere() {
        let mut l = Lapic::new(HZ);
        l.write(0xF0, 4, 0x1FF, 0);
        // INIT to all excluding self, then SIPI: nothing is raised.
        l.write(0x300, 4, 0x000C_4500, 0);
        l.write(0x300, 4, 0x000C_4687, 0);
        assert_eq!(l.pending_vector(), None);
        // Fixed delivery, shorthand "self", vector 0x60.
        l.write(0x300, 4, 0x0004_0060, 0);
        assert_eq!(l.pending_vector(), Some(0x60));
    }

    #[test]
    fn legacy_interrupts_reach_the_cpu_until_the_apic_masks_the_virtual_wire() {
        let mut l = Lapic::new(HZ);
        assert!(
            l.extint_enabled(),
            "APIC off: the PIC is wired straight through"
        );
        l.write(0xF0, 4, 0x1FF, 0);
        assert!(!l.extint_enabled(), "APIC on with LINT0 masked at reset");
        l.write(0x350, 4, 0x700, 0); // ExtINT, unmasked: the virtual-wire mode
        assert!(l.extint_enabled());
    }
}

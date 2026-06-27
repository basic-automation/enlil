//! Virtual Local APIC (LAPIC) emulation.
//!
//! Implements the x86 Local APIC as defined in the Intel SDM Vol 3, Chapter 10.
//! Each vCPU gets its own LAPIC instance. The LAPIC handles:
//! - Interrupt acceptance and priority arbitration
//! - Timer (one-shot, periodic, TSC-deadline)
//! - IPI delivery
//! - EOI processing

use super::{DeliveryMode, InterruptEntry, TriggerMode};
use crate::truncate::{u8_of, u32_of};

/// Default xAPIC MMIO base (the reset value of the `IA32_APIC_BASE` MSR's
/// frame). All vCPUs see their own LAPIC at this physical address.
pub const LAPIC_BASE: u64 = 0xFEE0_0000;

// LAPIC register offsets (byte offsets from base 0xFEE00000)
pub const LAPIC_ID: u32 = 0x020;
pub const LAPIC_VERSION: u32 = 0x030;
pub const LAPIC_TPR: u32 = 0x080;
pub const LAPIC_APR: u32 = 0x090;
pub const LAPIC_PPR: u32 = 0x0A0;
pub const LAPIC_EOI: u32 = 0x0B0;
pub const LAPIC_LDR: u32 = 0x0D0;
pub const LAPIC_DFR: u32 = 0x0E0;
pub const LAPIC_SVR: u32 = 0x0F0;
pub const LAPIC_ISR_BASE: u32 = 0x100; // 0x100-0x170 (8 regs)
pub const LAPIC_TMR_BASE: u32 = 0x180; // 0x180-0x1F0 (8 regs)
pub const LAPIC_IRR_BASE: u32 = 0x200; // 0x200-0x270 (8 regs)
pub const LAPIC_ESR: u32 = 0x280;
pub const LAPIC_ICR_LOW: u32 = 0x300;
pub const LAPIC_ICR_HIGH: u32 = 0x310;
pub const LAPIC_LVT_TIMER: u32 = 0x320;
pub const LAPIC_LVT_THERMAL: u32 = 0x330;
pub const LAPIC_LVT_PERF: u32 = 0x340;
pub const LAPIC_LVT_LINT0: u32 = 0x350;
pub const LAPIC_LVT_LINT1: u32 = 0x360;
pub const LAPIC_LVT_ERROR: u32 = 0x370;
pub const LAPIC_TIMER_INIT: u32 = 0x380;
pub const LAPIC_TIMER_CURRENT: u32 = 0x390;
pub const LAPIC_TIMER_DIVIDE: u32 = 0x3E0;
pub const LAPIC_SELF_IPI: u32 = 0x3F0;

/// LAPIC timer modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerMode {
    OneShot = 0,
    Periodic = 1,
    TscDeadline = 2,
}

/// Virtual Local APIC for a single vCPU.
pub struct LocalApic {
    /// APIC ID for this LAPIC.
    id: u8,
    /// Whether the LAPIC is software-enabled.
    enabled: bool,
    /// Task Priority Register.
    tpr: u32,
    /// Logical Destination Register.
    ldr: u32,
    /// Destination Format Register.
    dfr: u32,
    /// Spurious Interrupt Vector Register.
    svr: u32,
    /// In-Service Register (256 bits = 8 x `u32`).
    isr: [u32; 8],
    /// Trigger Mode Register (256 bits).
    tmr: [u32; 8],
    /// Interrupt Request Register (256 bits).
    irr: [u32; 8],
    /// Error Status Register.
    esr: u32,
    /// Interrupt Command Register (64 bits).
    icr: u64,
    /// `LVT` Timer.
    lvt_timer: u32,
    /// `LVT` Thermal Sensor.
    lvt_thermal: u32,
    /// `LVT` Performance Counter.
    lvt_perf: u32,
    /// `LVT` LINT0.
    lvt_lint0: u32,
    /// `LVT` LINT1.
    lvt_lint1: u32,
    /// `LVT` Error.
    lvt_error: u32,
    /// Timer initial count.
    timer_initial: u32,
    /// Timer current count.
    timer_current: u32,
    /// Timer divide configuration (DCR).
    timer_divide: u32,
    /// Accumulated input ticks not yet consumed by the divide configuration —
    /// lets the divisor apply correctly across `timer_tick` batches smaller
    /// than the divisor.
    timer_divide_residual: u32,
    /// Timer mode.
    timer_mode: TimerMode,
    /// TSC deadline value.
    tsc_deadline: u64,
    /// Pending interrupt to inject into vCPU.
    pending_injection: Option<u8>,
}

impl LocalApic {
    /// Create a new `LocalApic` with the given APIC ID.
    #[must_use]
    pub const fn new(id: u8) -> Self {
        Self {
            id,
            enabled: false,
            tpr: 0,
            ldr: 0,
            dfr: 0xFFFF_FFFF, // Flat model
            svr: 0xFF,        // Disabled, vector 0xFF
            isr: [0; 8],
            tmr: [0; 8],
            irr: [0; 8],
            esr: 0,
            icr: 0,
            lvt_timer: 0x0001_0000, // Masked
            lvt_thermal: 0x0001_0000,
            lvt_perf: 0x0001_0000,
            lvt_lint0: 0x0001_0000,
            lvt_lint1: 0x0001_0000,
            lvt_error: 0x0001_0000,
            timer_initial: 0,
            timer_current: 0,
            timer_divide: 0,
            timer_divide_residual: 0,
            timer_mode: TimerMode::OneShot,
            tsc_deadline: 0,
            pending_injection: None,
        }
    }

    /// APIC ID.
    #[must_use]
    pub const fn id(&self) -> u8 {
        self.id
    }

    /// Current Interrupt Command Register value (the full 64-bit ICR).
    #[must_use]
    pub const fn icr(&self) -> u64 {
        self.icr
    }

    /// Highest-priority in-service vector (the highest set `ISR` bit), if any.
    ///
    /// This is the vector a bare `EOI` register write retires, so the
    /// controller can resolve which line to broadcast the EOI to before the
    /// `ISR` bit is cleared.
    #[must_use]
    pub fn in_service_vector(&self) -> Option<u8> {
        Self::highest_bit_in_register(&self.isr)
    }

    /// Whether the `LocalApic` is software-enabled (bit 8 of `SVR`).
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether this LAPIC is a member of the logical destination `dest` (an
    /// 8-bit Message Destination Address) under its current `DFR`/`LDR`.
    ///
    /// Intel SDM Vol. 3A §10.6.2.2: the **flat** model (`DFR[31:28] = 0xF`)
    /// treats `LDR[31:24]` as an 8-bit bitmask of logical APIC IDs — a LAPIC is
    /// addressed when any bit it owns is set in `dest`. The **cluster** model
    /// (`DFR[31:28] = 0x0`) splits both `dest` and `LDR[31:24]` into a 4-bit
    /// cluster ID (high nibble) and a 4-bit intra-cluster bitmask (low nibble),
    /// matching when the clusters are equal and the bitmasks overlap. A LAPIC
    /// whose `LDR` is still 0 (reset) matches no logical destination, exactly as
    /// real hardware drops a logical interrupt until the OS programs `LDR`.
    #[must_use]
    pub const fn matches_logical(&self, dest: u8) -> bool {
        let logical_id = (self.ldr >> 24) as u8;
        if (self.dfr >> 28) == 0xF {
            // Flat model: 8-bit bitmask intersection.
            logical_id & dest != 0
        } else {
            // Cluster model: equal cluster (high nibble) + overlapping mask.
            let cluster_match = logical_id & 0xF0 == dest & 0xF0;
            let mask_overlap = logical_id & dest & 0x0F != 0;
            cluster_match && mask_overlap
        }
    }

    /// Get the Task Priority Register value.
    #[must_use]
    pub const fn get_tpr(&self) -> u8 {
        (self.tpr & 0xFF) as u8
    }

    /// Set the Task Priority Register value.
    pub fn set_tpr(&mut self, value: u8) {
        self.tpr = u32::from(value);
    }

    /// Signal end-of-interrupt (public wrapper around `handle_eoi`).
    pub fn signal_eoi(&mut self) {
        self.handle_eoi();
    }

    /// Check if there's a pending interrupt that can be delivered.
    #[must_use]
    pub fn has_pending_interrupt(&self) -> bool {
        self.pending_interrupt().is_some()
    }

    /// Get the highest priority pending interrupt vector, if any.
    #[must_use]
    pub fn pending_vector(&self) -> Option<u8> {
        self.pending_interrupt()
    }

    /// Accept the highest priority pending interrupt (move from `IRR` to `ISR`).
    /// Returns the vector that was accepted, if any.
    #[must_use]
    pub fn accept_highest_interrupt(&mut self) -> Option<u8> {
        let vector = self.pending_interrupt()?;
        self.start_servicing(vector);
        Some(vector)
    }

    /// Read a LAPIC register by offset.
    #[must_use]
    pub fn read_register(&self, offset: u32) -> u32 {
        match offset {
            LAPIC_ID => u32::from(self.id) << 24,
            LAPIC_VERSION => {
                // Version 0x14 (Pentium 4+), max LVT entry = 5 (6 entries: 0-5)
                0x14 | (5 << 16)
            }
            LAPIC_TPR => self.tpr,
            LAPIC_APR => self.compute_apr(),
            LAPIC_PPR => self.compute_ppr(),
            LAPIC_LDR => self.ldr,
            LAPIC_DFR => self.dfr,
            LAPIC_SVR => self.svr,
            off if (LAPIC_ISR_BASE..LAPIC_ISR_BASE + 0x80).contains(&off) => {
                let idx = ((off - LAPIC_ISR_BASE) / 0x10) as usize;
                if idx < 8 { self.isr[idx] } else { 0 }
            }
            off if (LAPIC_TMR_BASE..LAPIC_TMR_BASE + 0x80).contains(&off) => {
                let idx = ((off - LAPIC_TMR_BASE) / 0x10) as usize;
                if idx < 8 { self.tmr[idx] } else { 0 }
            }
            off if (LAPIC_IRR_BASE..LAPIC_IRR_BASE + 0x80).contains(&off) => {
                let idx = ((off - LAPIC_IRR_BASE) / 0x10) as usize;
                if idx < 8 { self.irr[idx] } else { 0 }
            }
            LAPIC_ESR => self.esr,
            LAPIC_ICR_LOW => u32_of(self.icr),
            LAPIC_ICR_HIGH => (self.icr >> 32) as u32,
            LAPIC_LVT_TIMER => self.lvt_timer,
            LAPIC_LVT_THERMAL => self.lvt_thermal,
            LAPIC_LVT_PERF => self.lvt_perf,
            LAPIC_LVT_LINT0 => self.lvt_lint0,
            LAPIC_LVT_LINT1 => self.lvt_lint1,
            LAPIC_LVT_ERROR => self.lvt_error,
            LAPIC_TIMER_INIT => self.timer_initial,
            // Intel SDM Vol.3 §10.5.4: in TSC-deadline mode the current-count
            // register always reads 0 (the count is not used in that mode).
            LAPIC_TIMER_CURRENT => {
                if self.timer_mode == TimerMode::TscDeadline {
                    0
                } else {
                    self.timer_current
                }
            }
            LAPIC_TIMER_DIVIDE => self.timer_divide,
            _ => 0,
        }
    }

    /// Write a LAPIC register by offset.
    pub fn write_register(&mut self, offset: u32, value: u32) {
        match offset {
            LAPIC_ID => self.id = (value >> 24) as u8,
            LAPIC_TPR => self.tpr = value & 0xFF,
            LAPIC_LDR => self.ldr = value & 0xFF00_0000,
            LAPIC_DFR => self.dfr = value | 0x0FFF_FFFF,
            LAPIC_SVR => {
                self.svr = value;
                self.enabled = (value & 0x100) != 0;
            }
            LAPIC_EOI => self.handle_eoi(),
            LAPIC_ESR => {
                // Writing to `ESR` clears it, then re-reads show accumulated errors
                self.esr = 0;
            }
            LAPIC_ICR_LOW => {
                self.icr = (self.icr & 0xFFFF_FFFF_0000_0000) | u64::from(value);
                // Writing `ICR_LOW` triggers IPI delivery
            }
            LAPIC_ICR_HIGH => {
                self.icr = (self.icr & 0x0000_0000_FFFF_FFFF) | (u64::from(value) << 32);
            }
            LAPIC_LVT_TIMER => {
                self.lvt_timer = value;
                let new_mode = match (value >> 17) & 0x3 {
                    1 => TimerMode::Periodic,
                    2 => TimerMode::TscDeadline,
                    _ => TimerMode::OneShot,
                };
                // Intel SDM Vol.3 §10.5.4.1: changing the timer mode disarms a
                // pending TSC deadline (the IA32_TSC_DEADLINE MSR resets to 0).
                // Without this, a stale deadline armed before the switch could
                // still fire after the guest moved the timer to one-shot/periodic.
                if new_mode != self.timer_mode {
                    self.tsc_deadline = 0;
                }
                self.timer_mode = new_mode;
            }
            LAPIC_LVT_THERMAL | LAPIC_LVT_PERF | LAPIC_LVT_LINT0 | LAPIC_LVT_LINT1
            | LAPIC_LVT_ERROR => match offset {
                LAPIC_LVT_THERMAL => self.lvt_thermal = value,
                LAPIC_LVT_PERF => self.lvt_perf = value,
                LAPIC_LVT_LINT0 => self.lvt_lint0 = value,
                LAPIC_LVT_LINT1 => self.lvt_lint1 = value,
                LAPIC_LVT_ERROR => self.lvt_error = value,
                _ => unreachable!(),
            },
            LAPIC_TIMER_INIT => {
                // Intel SDM Vol.3 §10.5.4: in TSC-deadline mode writes to the
                // initial-count register are ignored (the timer is armed via the
                // IA32_TSC_DEADLINE MSR instead).
                if self.timer_mode != TimerMode::TscDeadline {
                    self.timer_initial = value;
                    self.timer_current = value;
                    self.timer_divide_residual = 0;
                }
            }
            LAPIC_TIMER_DIVIDE => {
                self.timer_divide = value;
                self.timer_divide_residual = 0;
            }
            LAPIC_SELF_IPI => {
                // x2APIC self-IPI: inject vector to self
                let vector = (value & 0xFF) as u8;
                let _ = self.accept_interrupt(&InterruptEntry {
                    vector,
                    delivery_mode: DeliveryMode::Fixed,
                    trigger_mode: TriggerMode::Edge,
                    level: true,
                });
            }
            _ => {}
        }
    }

    /// Accept an interrupt from the IOAPIC or IPI.
    /// Sets the corresponding bit in `IRR`.
    #[must_use]
    pub fn accept_interrupt(&mut self, entry: &InterruptEntry) -> bool {
        if !self.enabled {
            return false;
        }

        let vector = entry.vector;
        if vector < 16 {
            // Vectors 0-15 are reserved
            return false;
        }

        let reg_idx = (vector / 32) as usize;
        let bit = 1u32 << (vector % 32);

        // Set bit in IRR
        self.irr[reg_idx] |= bit;

        // Set TMR bit for level-triggered interrupts
        if entry.trigger_mode == TriggerMode::Level {
            self.tmr[reg_idx] |= bit;
        } else {
            self.tmr[reg_idx] &= !bit;
        }

        true
    }

    /// Accept an interrupt by vector number only (Fixed delivery, edge-triggered).
    #[must_use]
    pub fn accept_interrupt_vector(&mut self, vector: u8) -> bool {
        self.accept_interrupt(&InterruptEntry {
            vector,
            delivery_mode: DeliveryMode::Fixed,
            trigger_mode: TriggerMode::Edge,
            level: true,
        })
    }

    /// Check if there's a pending interrupt that can be delivered.
    /// Returns the vector if one is ready, considering TPR masking.
    #[must_use]
    pub fn pending_interrupt(&self) -> Option<u8> {
        if !self.enabled {
            return None;
        }

        let pending_vec = Self::highest_bit_in_register(&self.irr)?;
        let servicing_vec = Self::highest_bit_in_register(&self.isr).unwrap_or(0);
        let ppr = u8_of(self.compute_ppr());

        // Interrupt priority class = vector >> 4
        // Can deliver if IRR priority > PPR priority
        if (pending_vec >> 4) > (ppr >> 4) && (pending_vec >> 4) > (servicing_vec >> 4) {
            Some(pending_vec)
        } else {
            None
        }
    }

    /// Mark an interrupt as being serviced (move from `IRR` to `ISR`).
    /// Call this when actually injecting the interrupt into the vCPU.
    pub const fn start_servicing(&mut self, vector: u8) {
        let reg_idx = (vector / 32) as usize;
        let bit = 1u32 << (vector % 32);

        // Clear IRR, set ISR
        self.irr[reg_idx] &= !bit;
        self.isr[reg_idx] |= bit;
    }

    /// Get and clear the pending injection vector.
    #[must_use]
    pub const fn take_pending_injection(&mut self) -> Option<u8> {
        self.pending_injection.take()
    }

    /// Handle EOI write — clear the highest priority `ISR` bit.
    fn handle_eoi(&mut self) {
        if let Some(vector) = Self::highest_bit_in_register(&self.isr) {
            let reg_idx = (vector / 32) as usize;
            let bit = 1u32 << (vector % 32);
            self.isr[reg_idx] &= !bit;

            // For level-triggered interrupts, need to signal IOAPIC
            // (handled by the InterruptController)
        }
    }

    /// Compute Arbitration Priority Register.
    #[must_use]
    fn compute_apr(&self) -> u32 {
        // APR = max(TPR, highest ISR priority)
        let isr_prio = Self::highest_bit_in_register(&self.isr).map_or(0, |v| u32::from(v >> 4));
        let tpr_prio = self.tpr >> 4;
        if tpr_prio >= isr_prio {
            self.tpr
        } else {
            (isr_prio << 4) | (isr_prio & 0xF)
        }
    }

    /// Compute Processor Priority Register.
    #[must_use]
    fn compute_ppr(&self) -> u32 {
        let isrv = u32::from(Self::highest_bit_in_register(&self.isr).unwrap_or(0));
        let tpr = self.tpr;
        if (tpr >> 4) >= (isrv >> 4) {
            tpr
        } else {
            isrv & 0xF0
        }
    }

    /// Find the highest set bit across an 8-register (256-bit) bitmap.
    #[must_use]
    fn highest_bit_in_register(regs: &[u32; 8]) -> Option<u8> {
        for i in (0..8).rev() {
            if regs[i] != 0 {
                let bit = regs[i].ilog2();
                return Some((u8_of(i)) * 32 + u8_of(bit));
            }
        }
        None
    }

    /// The LAPIC timer's divisor, decoded from the Divide Configuration
    /// Register (Intel SDM Vol.3 §10.5.4).
    ///
    /// The divisor is encoded in bits 0, 1, and 3 (bit 2 is reserved): those
    /// three bits form a value whose `0b111` encoding means divide-by-1 and
    /// every other encoding `n` means divide-by-`2^(n+1)` — i.e. 1, 2, 4, 8,
    /// 16, 32, 64, 128.
    #[must_use]
    const fn timer_divisor(&self) -> u32 {
        let b3 = (self.timer_divide >> 3) & 1;
        let b1 = (self.timer_divide >> 1) & 1;
        let b0 = self.timer_divide & 1;
        let combined = (b3 << 2) | (b1 << 1) | b0;
        if combined == 0b111 {
            1
        } else {
            1u32 << (combined + 1)
        }
    }

    /// Timer tick — decrement current count and fire interrupt if needed.
    /// Returns `true` if a timer interrupt was generated.
    ///
    /// `ticks` is in input (bus) clocks; the Divide Configuration Register
    /// scales them down before they decrement the counter, with sub-divisor
    /// remainders carried in `timer_divide_residual` so a long run of small
    /// batches divides exactly like one large batch.
    #[must_use]
    pub fn timer_tick(&mut self, ticks: u32) -> bool {
        if self.timer_initial == 0 || self.timer_mode == TimerMode::TscDeadline {
            return false;
        }

        // Apply the divide configuration: only every `divisor` input clocks
        // advances the counter by one.
        let divisor = self.timer_divisor();
        let total = self.timer_divide_residual.saturating_add(ticks);
        let ticks = total / divisor;
        self.timer_divide_residual = total % divisor;
        if ticks == 0 {
            return false;
        }

        if self.timer_current <= ticks {
            // Timer expired
            let masked = (self.lvt_timer & 0x0001_0000) != 0;
            if !masked {
                let vector = (self.lvt_timer & 0xFF) as u8;
                let _ = self.accept_interrupt(&InterruptEntry {
                    vector,
                    delivery_mode: DeliveryMode::Fixed,
                    trigger_mode: TriggerMode::Edge,
                    level: true,
                });
            }

            if self.timer_mode == TimerMode::Periodic {
                // Carry the ticks beyond this expiry into the next period so a
                // large batch divides exactly like a sequence of small ones —
                // the phase never drifts. (timer_initial != 0 here: the count==0
                // early-return above rules it out.) The IRR coalesces the
                // multiple expiries a single batch may span into one pending
                // interrupt, exactly as hardware does when the guest has not yet
                // serviced the vector.
                let beyond = ticks - self.timer_current;
                let into_period = beyond % self.timer_initial;
                self.timer_current = self.timer_initial - into_period;
            } else {
                self.timer_current = 0;
            }

            true
        } else {
            self.timer_current -= ticks;
            false
        }
    }

    /// Check if a timer interrupt is pending (not masked).
    #[must_use]
    pub const fn timer_interrupt_pending(&self) -> bool {
        (self.lvt_timer & 0x0001_0000) == 0
    }

    /// TSC deadline value for TSC-deadline timer mode.
    #[must_use]
    pub const fn tsc_deadline(&self) -> u64 {
        self.tsc_deadline
    }

    /// Set TSC deadline value.
    pub const fn set_tsc_deadline(&mut self, value: u64) {
        self.tsc_deadline = value;
    }

    /// Advance the **TSC-deadline** timer against the guest's current TSC and
    /// fire the timer interrupt if the armed deadline has been reached.
    ///
    /// TSC-deadline mode (Intel SDM Vol.3 §10.5.4) is *not* driven by the
    /// bus-clock [`timer_tick`](Self::timer_tick) path — that method returns
    /// early for it. Instead the guest arms a one-shot interrupt by writing an
    /// absolute TSC value to the `IA32_TSC_DEADLINE` MSR, and the LAPIC fires
    /// once when the TSC reaches that value. A non-zero `tsc_deadline` is
    /// "armed"; writing 0 disarms it. On reaching the deadline the timer
    /// auto-disarms (resets to 0) — the guest re-arms for the next tick by
    /// writing the MSR again, exactly like hardware. The disarm happens whether
    /// or not the LVT is masked (the mask gates *delivery*, not the expiry).
    ///
    /// Returns `true` iff the deadline was reached this call (an interrupt was
    /// injected unless the LVT timer was masked, matching the bus-clock path).
    /// No-op returning `false` unless the LVT timer is in TSC-deadline mode with
    /// a non-zero deadline that `current_tsc` has reached.
    #[must_use]
    pub fn check_tsc_deadline(&mut self, current_tsc: u64) -> bool {
        if self.timer_mode != TimerMode::TscDeadline || self.tsc_deadline == 0 {
            return false;
        }
        if current_tsc < self.tsc_deadline {
            return false;
        }

        // Deadline reached: auto-disarm (one-shot), then inject unless masked.
        self.tsc_deadline = 0;
        let masked = (self.lvt_timer & 0x0001_0000) != 0;
        if !masked {
            let vector = (self.lvt_timer & 0xFF) as u8;
            let _ = self.accept_interrupt(&InterruptEntry {
                vector,
                delivery_mode: DeliveryMode::Fixed,
                trigger_mode: TriggerMode::Edge,
                level: true,
            });
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lapic_new() {
        let lapic = LocalApic::new(42);
        assert_eq!(lapic.id(), 42);
        assert!(!lapic.is_enabled());
    }

    #[test]
    fn test_matches_logical_flat_model() {
        // Default DFR is flat (0xFFFF_FFFF); LDR top byte is the logical-id mask.
        let mut lapic = LocalApic::new(0);
        lapic.write_register(LAPIC_LDR, 0x0400_0000); // logical id bit 2
        assert!(lapic.matches_logical(0x04));
        assert!(lapic.matches_logical(0x06)); // 0x02 | 0x04 — overlaps
        assert!(!lapic.matches_logical(0x02));
        assert!(lapic.matches_logical(0xFF)); // broadcast reaches a programmed LDR
        // A LAPIC that never programmed its LDR matches nothing — not even the
        // all-ones broadcast (0 & 0xFF == 0), exactly like real hardware.
        let blank = LocalApic::new(1);
        assert!(!blank.matches_logical(0x01));
        assert!(!blank.matches_logical(0xFF));
    }

    #[test]
    fn test_matches_logical_cluster_model() {
        // Cluster model: DFR[31:28] = 0x0. LDR high nibble = cluster, low
        // nibble = intra-cluster bitmask.
        let mut lapic = LocalApic::new(0);
        lapic.write_register(LAPIC_DFR, 0x0FFF_FFFF); // cluster model
        lapic.write_register(LAPIC_LDR, 0x2100_0000); // cluster 2, member bit 0
        assert!(lapic.matches_logical(0x21)); // same cluster, overlapping member
        assert!(!lapic.matches_logical(0x22)); // same cluster, different member
        assert!(!lapic.matches_logical(0x11)); // different cluster
        // All-ones is cluster 0xF / mask 0xF — it does not cross into cluster 2.
        assert!(!lapic.matches_logical(0xFF));
    }

    #[test]
    fn matches_logical_flat_and_cluster_models() {
        let mut lapic = LocalApic::new(0);
        // Reset LDR (0) matches no logical destination in either model.
        assert!(!lapic.matches_logical(0xFF));

        // Flat model: LDR[31:24] is an 8-bit bitmask. Owning bit 0x02 matches
        // any destination with that bit set, and nothing without it.
        lapic.write_register(LAPIC_DFR, 0xFFFF_FFFF);
        lapic.write_register(LAPIC_LDR, 0x02 << 24);
        assert!(lapic.matches_logical(0x02));
        assert!(lapic.matches_logical(0xFF));
        assert!(!lapic.matches_logical(0x04));

        // Cluster model (DFR[31:28] = 0): high nibble = cluster, low nibble =
        // intra-cluster bitmask. LDR cluster 2, mask bit 0x1.
        lapic.write_register(LAPIC_DFR, 0x0FFF_FFFF);
        lapic.write_register(LAPIC_LDR, 0x21 << 24);
        assert!(
            lapic.matches_logical(0x21),
            "same cluster, overlapping mask"
        );
        assert!(!lapic.matches_logical(0x11), "different cluster");
        assert!(!lapic.matches_logical(0x22), "same cluster, disjoint mask");
    }

    #[test]
    fn test_enable_disable() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF); // Set bit 8 to enable
        assert!(lapic.is_enabled());

        lapic.write_register(LAPIC_SVR, 0x0FF); // Clear bit 8
        assert!(!lapic.is_enabled());
    }

    #[test]
    fn test_accept_interrupt() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF); // Enable
        lapic.set_tpr(0);

        let entry = InterruptEntry {
            vector: 0x80,
            delivery_mode: DeliveryMode::Fixed,
            trigger_mode: TriggerMode::Edge,
            level: true,
        };

        assert!(lapic.accept_interrupt(&entry));
        assert!(lapic.has_pending_interrupt());
        assert_eq!(lapic.pending_vector(), Some(0x80));
    }

    #[test]
    fn test_vector_15_rejected() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);

        let entry = InterruptEntry {
            vector: 15,
            delivery_mode: DeliveryMode::Fixed,
            trigger_mode: TriggerMode::Edge,
            level: true,
        };

        assert!(!lapic.accept_interrupt(&entry));
    }

    #[test]
    fn test_priority_masking() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);
        lapic.set_tpr(0x80); // Set TPR to priority class 8

        let entry = InterruptEntry {
            vector: 0x70, // Priority class 7, lower than TPR
            delivery_mode: DeliveryMode::Fixed,
            trigger_mode: TriggerMode::Edge,
            level: true,
        };

        assert!(lapic.accept_interrupt(&entry));
        assert!(!lapic.has_pending_interrupt()); // Masked by TPR
    }

    #[test]
    fn test_eoi() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);

        let entry = InterruptEntry {
            vector: 0x80,
            delivery_mode: DeliveryMode::Fixed,
            trigger_mode: TriggerMode::Edge,
            level: true,
        };

        let _ = lapic.accept_interrupt(&entry);
        let vec = lapic.accept_highest_interrupt();
        assert_eq!(vec, Some(0x80));

        lapic.signal_eoi();
        assert!(!lapic.has_pending_interrupt());
    }

    #[test]
    fn test_simultaneous_priorities() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);
        lapic.set_tpr(0);

        let entry1 = InterruptEntry {
            vector: 0x80,
            delivery_mode: DeliveryMode::Fixed,
            trigger_mode: TriggerMode::Edge,
            level: true,
        };

        let entry2 = InterruptEntry {
            vector: 0x90,
            delivery_mode: DeliveryMode::Fixed,
            trigger_mode: TriggerMode::Edge,
            level: true,
        };

        let _ = lapic.accept_interrupt(&entry1);
        let _ = lapic.accept_interrupt(&entry2);

        // Should prefer higher vector
        assert_eq!(lapic.pending_vector(), Some(0x90));
    }

    #[test]
    fn test_timer_one_shot() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);

        lapic.write_register(LAPIC_LVT_TIMER, 0x20040); // Vector 0x40, one-shot
        lapic.write_register(LAPIC_TIMER_DIVIDE, 0b1011); // divide by 1
        lapic.write_register(LAPIC_TIMER_INIT, 10);

        assert!(!lapic.timer_tick(5));
        assert!(!lapic.has_pending_interrupt()); // Not fired yet

        assert!(lapic.timer_tick(10)); // Remaining 5 < 10
        assert!(lapic.has_pending_interrupt());
        assert_eq!(lapic.pending_vector(), Some(0x40));
    }

    #[test]
    fn test_timer_tsc_deadline_fires_once_and_auto_disarms() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);
        // LVT timer: vector 0x40, TSC-deadline mode (bits[18:17] = 0b10).
        lapic.write_register(LAPIC_LVT_TIMER, 0x40 | (2 << 17));

        // Disarmed (deadline 0): never fires regardless of the TSC.
        assert!(!lapic.check_tsc_deadline(1_000_000));

        // Arm the deadline; before it is reached nothing fires and it stays armed.
        lapic.set_tsc_deadline(1_000);
        assert!(!lapic.check_tsc_deadline(999));
        assert!(!lapic.has_pending_interrupt());
        assert_eq!(lapic.tsc_deadline(), 1_000, "must stay armed before expiry");

        // Reaching the deadline fires exactly once and auto-disarms (one-shot).
        assert!(lapic.check_tsc_deadline(1_000));
        assert!(lapic.has_pending_interrupt());
        assert_eq!(lapic.pending_vector(), Some(0x40));
        assert_eq!(
            lapic.tsc_deadline(),
            0,
            "deadline must auto-disarm on firing"
        );

        // A later TSC does not re-fire until the guest re-arms by writing the MSR.
        assert!(!lapic.check_tsc_deadline(2_000_000));
    }

    #[test]
    fn test_timer_tsc_deadline_masked_disarms_without_injecting() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);
        // TSC-deadline mode, masked (LVT bit 16 set): expiry disarms but the
        // interrupt is not delivered — the mask gates delivery, not the expiry.
        lapic.write_register(LAPIC_LVT_TIMER, 0x40 | (2 << 17) | 0x1_0000);
        lapic.set_tsc_deadline(500);

        assert!(
            lapic.check_tsc_deadline(500),
            "deadline reached -> reports expiry"
        );
        assert!(!lapic.has_pending_interrupt(), "masked LVT injects nothing");
        assert_eq!(lapic.tsc_deadline(), 0, "still auto-disarms while masked");
    }

    #[test]
    fn test_timer_tsc_deadline_ignored_in_oneshot_mode() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);
        // One-shot mode (default): the TSC-deadline path is inert even if a
        // deadline value happens to be set.
        lapic.write_register(LAPIC_LVT_TIMER, 0x40); // one-shot
        lapic.set_tsc_deadline(100);
        assert!(!lapic.check_tsc_deadline(1_000_000));
        assert!(!lapic.has_pending_interrupt());
        assert_eq!(
            lapic.tsc_deadline(),
            100,
            "non-deadline mode leaves it untouched"
        );
    }

    #[test]
    fn test_tsc_deadline_mode_ignores_the_initial_count_register() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);

        // One-shot first: the initial/current count behave normally.
        lapic.write_register(LAPIC_LVT_TIMER, 0x40); // one-shot
        lapic.write_register(LAPIC_TIMER_INIT, 500);
        assert_eq!(lapic.read_register(LAPIC_TIMER_CURRENT), 500);

        // Switch to TSC-deadline mode: current-count reads 0 (SDM §10.5.4)...
        lapic.write_register(LAPIC_LVT_TIMER, 0x40 | (2 << 17));
        assert_eq!(
            lapic.read_register(LAPIC_TIMER_CURRENT),
            0,
            "TSC-deadline mode: current count reads 0"
        );
        // ...and a write to the initial-count register is ignored.
        lapic.write_register(LAPIC_TIMER_INIT, 999);
        assert_eq!(
            lapic.read_register(LAPIC_TIMER_CURRENT),
            0,
            "TSC-deadline mode: initial-count write is ignored"
        );

        // Returning to one-shot, the count machinery works again.
        lapic.write_register(LAPIC_LVT_TIMER, 0x40);
        lapic.write_register(LAPIC_TIMER_INIT, 250);
        assert_eq!(lapic.read_register(LAPIC_TIMER_CURRENT), 250);
    }

    #[test]
    fn test_timer_mode_change_disarms_a_pending_tsc_deadline() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);
        // Arm a deadline in TSC-deadline mode.
        lapic.write_register(LAPIC_LVT_TIMER, 0x40 | (2 << 17));
        lapic.set_tsc_deadline(1000);
        assert_eq!(lapic.tsc_deadline(), 1000);

        // Switching the timer to one-shot must disarm the deadline (SDM §10.5.4.1).
        lapic.write_register(LAPIC_LVT_TIMER, 0x40); // one-shot
        assert_eq!(
            lapic.tsc_deadline(),
            0,
            "mode change must disarm the deadline"
        );

        // And a subsequent deadline check must not fire the stale value.
        lapic.write_register(LAPIC_LVT_TIMER, 0x40 | (2 << 17)); // back to TSC-deadline
        assert!(
            !lapic.check_tsc_deadline(1_000_000),
            "no stale deadline survives the mode round-trip"
        );

        // A rewrite that keeps the same mode must NOT disarm an armed deadline.
        lapic.set_tsc_deadline(2000);
        lapic.write_register(LAPIC_LVT_TIMER, 0x41 | (2 << 17)); // same mode, new vector
        assert_eq!(
            lapic.tsc_deadline(),
            2000,
            "a same-mode LVT rewrite must leave the deadline armed"
        );
    }

    #[test]
    fn test_timer_periodic_carries_phase_across_a_multi_period_batch() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);
        // Periodic mode (LVT bit 17 set), vector 0x40, divide-by-1, period 100.
        lapic.write_register(LAPIC_LVT_TIMER, 0x2_0040);
        lapic.write_register(LAPIC_TIMER_DIVIDE, 0b1011);
        lapic.write_register(LAPIC_TIMER_INIT, 100);

        // A batch covering 2.5 periods fires and leaves the counter at exactly
        // half a period — the remainder is carried, not reset to a full 100, so
        // the periodic phase does not drift when advance_clocks passes a large ns
        // delta. (The IRR coalesces the multiple expiries into one pending int.)
        assert!(lapic.timer_tick(250));
        assert_eq!(
            lapic.read_register(LAPIC_TIMER_CURRENT),
            50,
            "periodic reload must carry the 50-tick remainder into the next period"
        );

        // 50 ticks finish this period, the next 50 start the following one: the
        // phase stays aligned at 50 rather than snapping back to a full period.
        assert!(lapic.timer_tick(100));
        assert_eq!(lapic.read_register(LAPIC_TIMER_CURRENT), 50);
    }

    #[test]
    fn test_timer_divide_configuration_scales_input_clocks() {
        let mut lapic = LocalApic::new(1);
        lapic.write_register(LAPIC_SVR, 0x1FF);
        lapic.write_register(LAPIC_LVT_TIMER, 0x20040); // vector 0x40, one-shot
        lapic.write_register(LAPIC_TIMER_DIVIDE, 0b0011); // divide by 16
        lapic.write_register(LAPIC_TIMER_INIT, 4);

        // 16 input clocks = one counter tick; 63 clocks = 3 counter ticks with
        // 15 carried over — counter 4 -> 1, not yet fired.
        assert!(!lapic.timer_tick(63));
        assert!(!lapic.has_pending_interrupt());

        // One more clock completes the 4th divided tick (15 + 1 = 16) and the
        // counter (1) reaches it, firing.
        assert!(lapic.timer_tick(1));
        assert_eq!(lapic.pending_vector(), Some(0x40));
    }

    #[test]
    fn test_timer_divisor_decoding() {
        let mut lapic = LocalApic::new(0);
        for (dcr, divisor) in [
            (0b0000u32, 2u32),
            (0b0001, 4),
            (0b0010, 8),
            (0b0011, 16),
            (0b1000, 32),
            (0b1001, 64),
            (0b1010, 128),
            (0b1011, 1),
        ] {
            lapic.write_register(LAPIC_TIMER_DIVIDE, dcr);
            assert_eq!(lapic.timer_divisor(), divisor, "DCR {dcr:#06b}");
        }
    }
}

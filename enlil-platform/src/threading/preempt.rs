//! APIC-timer preemption: turning the LAPIC timer tick into a forced context
//! switch.
//!
//! The LAPIC timer (one-shot or TSC-deadline, programmed by the backend)
//! raises an interrupt every [`Quantum`]. This module provides the
//! host-testable pieces that turn that tick into preemption:
//!
//! - [`Quantum`] plus [`tsc_deadline_offset`]/[`oneshot_count`]: pure
//!   quantum → timer-tick arithmetic the backend feeds to the APIC.
//! - [`PreemptFlag`]: the per-CPU flag the timer IRQ handler sets and the
//!   scheduler's dispatch loop drains. A set flag means "the running task's
//!   quantum expired — switch to the next task instead of resuming it".
//!
//! On bare metal the LAPIC timer IRQ handler calls [`PreemptFlag::request`]
//! and then the scheduler performs a [`switch_task`](super::context::switch_task)
//! when [`PreemptFlag::take`] reports a tick — that check is what makes the
//! preemption *forced* rather than a cooperative yield.

use core::sync::atomic::{AtomicBool, Ordering};

/// How long a task may run before the APIC timer may preempt it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantum {
    micros: u64,
}

impl Quantum {
    /// The default quantum: 1 ms.
    pub const DEFAULT: Self = Self::from_millis(1);

    /// A quantum of `us` microseconds.
    #[must_use]
    pub const fn from_micros(us: u64) -> Self {
        Self { micros: us }
    }

    /// A quantum of `ms` milliseconds.
    #[must_use]
    pub const fn from_millis(ms: u64) -> Self {
        Self {
            micros: ms.saturating_mul(1000),
        }
    }

    /// The quantum in microseconds.
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.micros
    }
}

/// TSC ticks in `quantum` on a `tsc_hz`-Hz timestamp counter.
///
/// Feeds `IA32_TSC_DEADLINE`: the backend arms `rdtsc() +
/// tsc_deadline_offset(..)`. Saturates at `u64::MAX`; never returns 0 — a
/// zero deadline *disarms* the TSC-deadline timer, so the floor is one tick.
#[must_use]
pub fn tsc_deadline_offset(quantum: Quantum, tsc_hz: u64) -> u64 {
    let ticks = u128::from(quantum.as_micros()) * u128::from(tsc_hz) / 1_000_000;
    u64::try_from(ticks).unwrap_or(u64::MAX).max(1)
}

/// LAPIC one-shot initial count for `quantum` on a timer bus clock.
///
/// `divide` is the LAPIC divide-configuration divisor (1, 2, 4, 8, 16, 32,
/// 64 or 128); anything else is treated as 1. Saturates at `u32::MAX` and
/// floors at 1 — a zero count never fires.
#[must_use]
pub fn oneshot_count(quantum: Quantum, bus_hz: u64, divide: u32) -> u32 {
    let divide = u64::from(divide.max(1));
    let ticks =
        u128::from(quantum.as_micros()) * u128::from(bus_hz) / u128::from(divide) / 1_000_000;
    u32::try_from(ticks).unwrap_or(u32::MAX).max(1)
}

/// Per-CPU "the timer fired — yield at the next opportunity" flag.
///
/// The LAPIC timer IRQ handler calls [`request`](Self::request); the
/// scheduler's dispatch loop calls [`take`](Self::take) and, when it returns
/// `true`, switches to the next runnable task instead of resuming the
/// preempted one. That check is what turns a timer tick into a *forced*
/// context switch rather than a cooperative yield.
///
/// `Send + Sync` so it can live in a static or a per-CPU area.
#[derive(Debug, Default)]
pub struct PreemptFlag {
    requested: AtomicBool,
}

impl PreemptFlag {
    /// A clear flag.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
        }
    }

    /// Record a timer tick. Called from the LAPIC timer IRQ handler.
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    /// Take a pending preemption request, clearing the flag.
    ///
    /// Returns `true` iff the timer fired since the last `take`.
    pub fn take(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }

    /// Whether a request is pending, without clearing it.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantum_default_is_1ms() {
        assert_eq!(Quantum::DEFAULT.as_micros(), 1000);
        assert_eq!(Quantum::from_millis(1), Quantum::DEFAULT);
    }

    #[test]
    fn quantum_from_millis_saturates() {
        assert_eq!(Quantum::from_millis(u64::MAX).as_micros(), u64::MAX);
        assert_eq!(Quantum::from_micros(500).as_micros(), 500);
    }

    #[test]
    fn tsc_deadline_offset_1ms_at_3ghz() {
        assert_eq!(
            tsc_deadline_offset(Quantum::from_millis(1), 3_000_000_000),
            3_000_000
        );
    }

    #[test]
    fn tsc_deadline_offset_zero_quantum_floors_at_one() {
        // A zero deadline disarms the timer; the floor keeps it armed.
        assert_eq!(
            tsc_deadline_offset(Quantum::from_micros(0), 3_000_000_000),
            1
        );
    }

    #[test]
    fn tsc_deadline_offset_saturates() {
        assert_eq!(
            tsc_deadline_offset(Quantum::from_micros(u64::MAX), u64::MAX),
            u64::MAX
        );
    }

    #[test]
    fn oneshot_count_1ms_100mhz_div16() {
        // 100 MHz / 16 = 6.25 MHz timer; 1 ms = 6250 ticks.
        assert_eq!(
            oneshot_count(Quantum::from_millis(1), 100_000_000, 16),
            6250
        );
    }

    #[test]
    fn oneshot_count_zero_divide_treated_as_one() {
        assert_eq!(oneshot_count(Quantum::from_millis(1), 1_000_000, 0), 1000);
    }

    #[test]
    fn oneshot_count_floors_at_one() {
        assert_eq!(oneshot_count(Quantum::from_micros(0), 100_000_000, 16), 1);
    }

    #[test]
    fn oneshot_count_saturates() {
        assert_eq!(
            oneshot_count(Quantum::from_micros(u64::MAX), u64::MAX, 1),
            u32::MAX
        );
    }

    #[test]
    fn preempt_flag_request_take_cycle() {
        let flag = PreemptFlag::new();
        assert!(!flag.is_requested());
        assert!(!flag.take());

        flag.request();
        assert!(flag.is_requested());
        assert!(flag.take());
        // take() clears.
        assert!(!flag.is_requested());
        assert!(!flag.take());
    }

    #[test]
    fn preempt_flag_multiple_requests_coalesce() {
        let flag = PreemptFlag::new();
        flag.request();
        flag.request();
        // One take drains the (coalesced) request.
        assert!(flag.take());
        assert!(!flag.take());
    }

    #[test]
    fn preempt_flag_default_is_clear() {
        let flag = PreemptFlag::default();
        assert!(!flag.take());
    }
}

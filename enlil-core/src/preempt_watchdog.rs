//! Forced quantum preemption for the KVM run loop: the TSC-deadline watchdog.
//!
//! On the KVM path there is no host LAPIC the run loop can program — the
//! guest's LAPIC is emulated on the [`DeviceBus`](crate::device_bus::DeviceBus)
//! and only fires when `KVM_RUN` *returns*. A guest that spins without
//! exiting would therefore never be preempted by the cooperative
//! [`run_timesliced`](crate::run_loop::StealthRunLoop::run_timesliced) loop,
//! whose only preemption point is the `KVM_RUN` return.
//!
//! [`PreemptWatchdog`] closes that gap. It is the KVM-path counterpart of the
//! bare-metal LAPIC TSC-deadline timer IRQ (see
//! `enlil_platform::threading::preempt`): the run loop arms it with an
//! *absolute host-TSC deadline* computed by [`tsc_deadline_for_quantum`] — the
//! same [`tsc_deadline_offset`](enlil_platform::threading::preempt::tsc_deadline_offset)
//! math the bare-metal backend feeds `IA32_TSC_DEADLINE` — and when the
//! deadline passes, the watchdog thread kicks the running vCPU's
//! [`ImmediateExitKicker`](crate::kvm_backend::ImmediateExitKicker), so the
//! in-flight `KVM_RUN` returns at once with [`GuestExit::Interrupted`]. The
//! run loop tells the kick apart from a spurious host signal by reading the
//! kick byte back ([`KvmBackend::take_immediate_exit`](crate::kvm_backend::KvmBackend::take_immediate_exit))
//! and performs the context switch instead of re-entering — the forced
//! preemption item 2.3's remaining slice requires.

#[cfg(target_os = "linux")]
pub use linux::{ticks_to_duration, tsc_deadline_for_quantum, PreemptWatchdog};

#[cfg(target_os = "linux")]
mod linux {
    use crate::kvm_backend::ImmediateExitKicker;
    use crate::{Error, Result};
    use enlil_platform::threading::preempt::{tsc_deadline_offset, Quantum};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// The absolute host-TSC deadline ending a `quantum` that starts at TSC
    /// `now_tsc`, on a `tsc_hz`-Hz timestamp counter.
    ///
    /// `now_tsc + `[`tsc_deadline_offset`] — the userspace form of arming
    /// `IA32_TSC_DEADLINE`: an absolute deadline, not a relative count.
    /// Saturates instead of wrapping (a wrapped deadline would fire at once
    /// and spuriously preempt); never returns `now_tsc` itself for a zero
    /// quantum because `tsc_deadline_offset` floors at one tick (a zero TSC
    /// deadline would disarm a real APIC timer).
    #[must_use]
    pub fn tsc_deadline_for_quantum(now_tsc: u64, quantum: Quantum, tsc_hz: u64) -> u64 {
        now_tsc.saturating_add(tsc_deadline_offset(quantum, tsc_hz))
    }

    /// Wall-clock [`Duration`] for `ticks` timestamp-counter ticks at
    /// `tsc_hz` Hz — what the watchdog parks for while waiting out a quantum.
    ///
    /// Computed in `u128` and clamped to `u64::MAX` nanoseconds; a zero rate
    /// (no clock known) yields [`Duration::MAX`], i.e. the watchdog sleeps
    /// until re-armed rather than dividing by zero.
    #[must_use]
    pub fn ticks_to_duration(ticks: u64, tsc_hz: u64) -> Duration {
        if tsc_hz == 0 {
            return Duration::MAX;
        }
        let nanos = u128::from(ticks) * 1_000_000_000 / u128::from(tsc_hz);
        Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
    }

    /// A command to the watchdog thread.
    enum Command {
        /// Arm a quantum: kick `kicker`'s vCPU when the host TSC reaches
        /// `deadline_tsc`. Supersedes any previous arm; `prev_kicker` (the
        /// vCPU being switched out, if any) has its kick byte cleared as the
        /// new arm installs, so a late fire for the old quantum can never
        /// strand a set byte on a vCPU that is no longer running.
        Arm {
            kicker: ImmediateExitKicker,
            prev_kicker: Option<ImmediateExitKicker>,
            deadline_tsc: u64,
        },
        /// Drop the pending arm, clearing the armed vCPU's kick byte.
        Disarm,
        /// Tear the thread down.
        Shutdown,
    }

    /// Install `deadline_tsc` for `kicker`, clearing `prev_kicker`'s byte
    /// first. Split out so the arm path reads the same from every call site.
    fn install_arm(
        armed: &mut Option<(ImmediateExitKicker, u64)>,
        kicker: ImmediateExitKicker,
        prev_kicker: Option<ImmediateExitKicker>,
        deadline_tsc: u64,
    ) {
        if let Some(prev) = prev_kicker {
            prev.set(false);
        }
        *armed = Some((kicker, deadline_tsc));
    }

    /// The watchdog thread: park until the armed TSC deadline (or a command),
    /// then kick.
    fn watchdog_main(rx: mpsc::Receiver<Command>, tsc_hz: u64) {
        let mut armed: Option<(ImmediateExitKicker, u64)> = None;
        loop {
            match armed {
                None => match rx.recv() {
                    Ok(Command::Arm {
                        kicker,
                        prev_kicker,
                        deadline_tsc,
                    }) => install_arm(&mut armed, kicker, prev_kicker, deadline_tsc),
                    Ok(Command::Disarm) => {}
                    Ok(Command::Shutdown) | Err(_) => break,
                },
                Some((kicker, deadline)) => {
                    // SAFETY: `_rdtsc` is a baseline x86-64 instruction with no
                    // preconditions; the KVM backend only compiles for x86-64 Linux.
                    let now = unsafe { core::arch::x86_64::_rdtsc() };
                    if now >= deadline {
                        // Quantum expired: the kick. KVM polls
                        // `immediate_exit` in the shared kvm_run page, so the
                        // in-flight `KVM_RUN` returns at once with
                        // `GuestExit::Interrupted` — the forced preemption.
                        kicker.set(true);
                        armed = None;
                        continue;
                    }
                    // Park until the deadline (a spurious early wake just
                    // re-checks the TSC above); a command always wins over the
                    // wait, so re-arming can never strand a stale deadline.
                    match rx.recv_timeout(ticks_to_duration(deadline - now, tsc_hz)) {
                        Ok(Command::Arm {
                            kicker,
                            prev_kicker,
                            deadline_tsc,
                        }) => install_arm(&mut armed, kicker, prev_kicker, deadline_tsc),
                        Ok(Command::Disarm) => {
                            kicker.set(false);
                            armed = None;
                        }
                        Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                }
            }
        }
    }

    /// Forced-preemption watchdog for the time-sliced KVM run loop.
    ///
    /// Spawn it with [`spawn`](Self::spawn) once the host TSC frequency is
    /// known, [`arm`](Self::arm) it at the start of every vCPU quantum with
    /// the absolute TSC deadline from [`tsc_deadline_for_quantum`], and
    /// [`disarm`](Self::disarm) it when the run ends. Dropping it shuts the
    /// thread down (joining it), so a watchdog owned by
    /// [`StealthRunLoop`](crate::run_loop::StealthRunLoop) must be declared
    /// before the [`KvmBackend`](crate::kvm_backend::KvmBackend) field: the
    /// kickers the thread holds point into the backend's `kvm_run` mappings.
    pub struct PreemptWatchdog {
        tx: mpsc::Sender<Command>,
        thread: Option<JoinHandle<()>>,
        tsc_hz: u64,
    }

    impl PreemptWatchdog {
        /// Spawn the watchdog thread.
        ///
        /// # Errors
        /// Returns [`Error::Vcpu`] if `tsc_hz` is zero (no clock to arm
        /// deadlines against) or the thread cannot be spawned.
        pub fn spawn(tsc_hz: u64) -> Result<Self> {
            if tsc_hz == 0 {
                return Err(Error::Vcpu(
                    "preemption watchdog needs a nonzero host TSC frequency".to_string(),
                ));
            }
            let (tx, rx) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("enlil-preempt-watchdog".to_string())
                .spawn(move || watchdog_main(rx, tsc_hz))
                .map_err(|e| Error::Vcpu(format!("spawn preemption watchdog thread: {e}")))?;
            Ok(Self {
                tx,
                thread: Some(thread),
                tsc_hz,
            })
        }

        /// The host TSC frequency this watchdog arms deadlines against.
        #[must_use]
        pub fn tsc_hz(&self) -> u64 {
            self.tsc_hz
        }

        /// Arm a quantum for `kicker`'s vCPU: when the host TSC reaches
        /// `deadline_tsc` the watchdog kicks it out of `KVM_RUN`.
        ///
        /// Supersedes any previous arm. `prev_kicker` names the vCPU being
        /// switched out (if any); its kick byte is cleared as the new arm
        /// installs. A send failure only means the thread is already gone,
        /// which [`Drop`] also tolerates — hence no `Result`.
        pub fn arm(
            &self,
            kicker: ImmediateExitKicker,
            prev_kicker: Option<ImmediateExitKicker>,
            deadline_tsc: u64,
        ) {
            let _ = self.tx.send(Command::Arm {
                kicker,
                prev_kicker,
                deadline_tsc,
            });
        }

        /// Drop the pending arm, clearing the armed vCPU's kick byte. The run
        /// loop also clears the byte itself; both are idempotent.
        pub fn disarm(&self) {
            let _ = self.tx.send(Command::Disarm);
        }
    }

    impl Drop for PreemptWatchdog {
        fn drop(&mut self) {
            let _ = self.tx.send(Command::Shutdown);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::time::Instant;

        /// A test kicker over a caller-owned byte (no `/dev/kvm` needed).
        fn test_kicker(byte: &mut u8) -> ImmediateExitKicker {
            // SAFETY: `byte` outlives the watchdog in every test below (the
            // watchdog is dropped — joining its thread — before `byte`'s
            // scope ends), so the pointer stays valid for every use.
            unsafe { ImmediateExitKicker::new(core::ptr::addr_of_mut!(*byte)) }
        }

        fn rdtsc() -> u64 {
            // SAFETY: as in `watchdog_main`.
            unsafe { core::arch::x86_64::_rdtsc() }
        }

        #[test]
        fn spawn_rejects_zero_tsc_hz() {
            assert!(PreemptWatchdog::spawn(0).is_err());
        }

        #[test]
        fn tsc_deadline_for_quantum_adds_offset() {
            assert_eq!(
                tsc_deadline_for_quantum(100, Quantum::from_millis(1), 3_000_000_000),
                100 + 3_000_000
            );
        }

        #[test]
        fn tsc_deadline_for_quantum_saturates() {
            assert_eq!(
                tsc_deadline_for_quantum(u64::MAX - 5, Quantum::from_millis(1), 3_000_000_000),
                u64::MAX
            );
        }

        #[test]
        fn tsc_deadline_for_quantum_zero_quantum_floors_at_one_tick() {
            // A zero deadline would disarm a real APIC timer; the floor keeps
            // the watchdog armed instead.
            assert_eq!(
                tsc_deadline_for_quantum(100, Quantum::from_micros(0), 3_000_000_000),
                101
            );
        }

        #[test]
        fn ticks_to_duration_conversions() {
            assert_eq!(
                ticks_to_duration(3_000_000_000, 3_000_000_000),
                Duration::from_secs(1)
            );
            assert_eq!(ticks_to_duration(0, 3_000_000_000), Duration::ZERO);
            assert_eq!(ticks_to_duration(1, 0), Duration::MAX);
            // Saturating: u64::MAX ticks at 1 Hz would overflow u64 nanos.
            assert_eq!(
                ticks_to_duration(u64::MAX, 1),
                Duration::from_nanos(u64::MAX)
            );
        }

        /// Poll `is_armed` until `deadline` (wall-clock), panicking with
        /// `what` on timeout so a stuck watchdog fails instead of hanging the
        /// suite.
        #[track_caller]
        fn wait_for_kick(kicker: &ImmediateExitKicker, what: &str) {
            let start = Instant::now();
            while !kicker.is_armed() {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "watchdog never fired: {what}"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        #[test]
        fn watchdog_fires_kick_at_tsc_deadline() {
            let mut byte = 0u8;
            let kicker = test_kicker(&mut byte);
            let watchdog = PreemptWatchdog::spawn(3_000_000_000).expect("spawn watchdog");
            // 1 ms out at 3 GHz — the watchdog must set the byte on its own.
            watchdog.arm(kicker, None, rdtsc() + 3_000_000);
            wait_for_kick(&kicker, "1ms deadline");
            assert!(kicker.is_armed());
            drop(watchdog);
        }

        #[test]
        fn watchdog_disarm_cancels_pending_arm() {
            let mut byte = 0u8;
            let kicker = test_kicker(&mut byte);
            let watchdog = PreemptWatchdog::spawn(3_000_000_000).expect("spawn watchdog");
            // 60 s out: unreachable in the test, then disarm before it.
            watchdog.arm(kicker, None, rdtsc() + 180_000_000_000);
            watchdog.disarm();
            std::thread::sleep(Duration::from_millis(100));
            assert!(!kicker.is_armed(), "disarmed watchdog must not fire");
            drop(watchdog);
        }

        #[test]
        fn watchdog_rearm_supersedes_and_clears_previous_kick() {
            let mut byte_a = 0u8;
            let mut byte_b = 0u8;
            let kicker_a = test_kicker(&mut byte_a);
            let kicker_b = test_kicker(&mut byte_b);
            let watchdog = PreemptWatchdog::spawn(3_000_000_000).expect("spawn watchdog");

            // A's quantum fires...
            watchdog.arm(kicker_a, None, rdtsc() + 3_000_000);
            wait_for_kick(&kicker_a, "A's deadline");
            // ...then B's arm (prev = A) supersedes and clears A's byte.
            watchdog.arm(kicker_b, Some(kicker_a), rdtsc() + 180_000_000_000);
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !kicker_a.is_armed(),
                "re-arm must clear the previous vCPU's kick byte"
            );
            assert!(
                !kicker_b.is_armed(),
                "B's far-future deadline must not have fired"
            );
            drop(watchdog);
        }
    }
}

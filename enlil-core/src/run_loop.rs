//! The production vCPU run loop driver.
//!
//! Prior runs built the stealth plumbing as individual primitives:
//! [`StandardPc::install_stealth_msr_router`] (install the router + get the
//! shared timing handle), [`KvmBackend::enable_userspace_msr_exits`] +
//! [`KvmBackend::forward_msrs_to_userspace`] (route the MSR exits to userspace
//! and filter exactly the router's MSRs), [`KvmBackend::clear_cpuid_hypervisor_bit`]
//! (drop the `CPUID.1:ECX[31]` tell), and [`KvmBackend::run_vcpu_timed`] (run a
//! vCPU while driving the APERF/MPERF timing shadows around `KVM_RUN`). Every
//! end-to-end test wired these together by hand, in the right order, with the
//! PMC surface advanced from the returned cycle delta and the platform-event
//! latches polled each iteration.
//!
//! [`StealthRunLoop`] is that wiring, encapsulated once: it owns the
//! [`KvmBackend`] and the [`StandardPc`], performs the stealth setup in the
//! correct order at construction, and exposes a single-step `run_vcpu_once`
//! (and a bounded `run_vcpu_until_event`) that keeps the RDPMC surface in
//! lockstep with the timing shadows and surfaces the platform events a guest
//! raises. This is the architectural seam the per-vCPU stealth primitives were
//! always pointing at — the thing a real guest boot drives.
//!
//! It is `target_os = "linux"`-only because it is built directly on the KVM
//! backend; the platform-agnostic stealth state (the router, the timing
//! shadows, the PMC/LBR models) lives in [`crate::stealth_msr`] /
//! [`crate::timing_stealth`] / `enlil_devices::stealth` and compiles
//! everywhere.
//!
//! [`StandardPc::install_stealth_msr_router`]: crate::device_bus::StandardPc::install_stealth_msr_router
//! [`KvmBackend`]: crate::kvm_backend::KvmBackend
//! [`StandardPc`]: crate::device_bus::StandardPc

#[cfg(target_os = "linux")]
pub use linux::{LoopOutcome, RunStep, StealthRunLoop};

#[cfg(target_os = "linux")]
mod linux {
    use crate::device_bus::{PlatformEvent, StandardPc};
    use crate::kvm_backend::{GuestExit, KvmBackend};
    use crate::timing_stealth::VcpuTimingState;
    use crate::Result;
    use enlil_devices::stealth::lbr::LbrPlatform;
    use enlil_devices::stealth::pmc::PmcRateModel;
    use std::sync::Arc;

    /// The outcome of a single run-loop iteration.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RunStep {
        /// How the guest left the vCPU on this entry.
        pub exit: GuestExit,
        /// The in-guest reference-cycle delta this entry advanced the stealth
        /// time surfaces by (APERF/MPERF *and*, via the run loop, the RDPMC
        /// counters), straight from [`KvmBackend::run_vcpu_timed`].
        pub guest_cycles: u64,
        /// A platform-control event the guest raised that the run loop must act
        /// on (ACPI sleep/shutdown, CPU reset), or `None`. Drained from the
        /// [`StandardPc`] latches *after* the exit was serviced, so a write to
        /// `0x92`/`0xCF9`/the PM1a block on this entry is seen here.
        pub event: Option<PlatformEvent>,
    }

    /// How a managed run ([`StealthRunLoop::run_real_mode`]) ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LoopOutcome {
        /// The guest executed `HLT` and the run returned (no in-kernel IRQ chip,
        /// or nothing pending to wake it).
        Halted,
        /// The guest committed an ACPI sleep transition; the `SLP_TYP` value
        /// (the DSDT's `_S5` value, `5`, means **power off**).
        Shutdown(u8),
        /// `max_entries` elapsed without the guest halting or sleeping — the
        /// bound that keeps a never-halting or reboot-looping guest from
        /// spinning here forever.
        Exhausted,
    }

    impl RunStep {
        /// Whether this step is a terminal stopping point for a bounded run —
        /// the guest halted, or it raised a platform event (sleep/reset) the
        /// run loop owner must handle before re-entering.
        #[must_use]
        pub const fn is_stop(&self) -> bool {
            matches!(self.exit, GuestExit::Halted) || self.event.is_some()
        }
    }

    /// A vCPU run loop with the full MSR/CPUID/timing stealth stack wired in.
    ///
    /// Construct it with [`install`](Self::install) — which performs the
    /// backend-side stealth setup in the order KVM requires (enable userspace
    /// MSR exits, then install the MSR filter, both *before* any vCPU runs) —
    /// then [`create_vcpu`](Self::create_vcpu) the vCPUs,
    /// [`apply_cpuid_stealth`](Self::apply_cpuid_stealth) once they exist, and
    /// drive [`run_vcpu_once`](Self::run_vcpu_once) per entry.
    pub struct StealthRunLoop {
        backend: KvmBackend,
        pc: StandardPc,
        /// Shared with the installed router; the run loop drives it inside
        /// `run_vcpu_timed`.
        timing: Arc<VcpuTimingState>,
        /// The single rate model both stealth surfaces advance by, copied out
        /// of the router at install so `run_vcpu_once` need not re-borrow the
        /// bus to read it while it is borrowed as the exit handler.
        model: PmcRateModel,
    }

    impl StealthRunLoop {
        /// Wire the stealth MSR stack onto `backend` + `pc` and take ownership
        /// of both.
        ///
        /// In order: install the [`StealthMsrRouter`](crate::stealth_msr::StealthMsrRouter)
        /// on the bus for `platform` (seeding the timing shadows at the model
        /// ratio and handing back the shared timing handle), then on the
        /// backend [`enable_userspace_msr_exits`](KvmBackend::enable_userspace_msr_exits)
        /// and [`forward_msrs_to_userspace`](KvmBackend::forward_msrs_to_userspace)
        /// with *exactly* the router's
        /// [`filter_ranges`](crate::stealth_msr::StealthMsrRouter::filter_ranges)
        /// — the single source of truth that keeps the KVM filter and the
        /// router covering the same MSRs. Both backend steps must precede any
        /// `KVM_RUN`, which is why they belong at construction.
        ///
        /// # Errors
        /// Propagates the backend errors from
        /// [`enable_userspace_msr_exits`](KvmBackend::enable_userspace_msr_exits)
        /// (the `KVM_CAP_X86_USER_SPACE_MSR` capability) and
        /// [`forward_msrs_to_userspace`](KvmBackend::forward_msrs_to_userspace)
        /// (`KVM_X86_SET_MSR_FILTER`).
        pub fn install(
            backend: KvmBackend,
            mut pc: StandardPc,
            platform: LbrPlatform,
        ) -> Result<Self> {
            let timing = pc.install_stealth_msr_router(platform);
            // One borrow of the freshly-installed router: take both the model
            // and the MSR ranges that must match it.
            let (model, ranges) = {
                let router = pc
                    .bus
                    .stealth_msr_mut()
                    .expect("router installed by install_stealth_msr_router");
                (router.pmc.rate_model, router.filter_ranges())
            };
            backend.enable_userspace_msr_exits()?;
            backend.forward_msrs_to_userspace(&ranges)?;
            Ok(Self {
                backend,
                pc,
                timing,
                model,
            })
        }

        /// Create a vCPU with the given APIC id and return its index.
        ///
        /// # Errors
        /// Propagates [`KvmBackend::create_vcpu`].
        pub fn create_vcpu(&mut self, apic_id: u64) -> Result<usize> {
            self.backend.create_vcpu(apic_id)
        }

        /// Clear the `CPUID.1:ECX[31]` hypervisor-present bit on **all** vCPUs
        /// created so far — call once after the vCPUs exist.
        ///
        /// This is the minimal CPUID stealth; prefer
        /// [`apply_topology_stealth`](Self::apply_topology_stealth) when a
        /// [`CpuidStealthTable`](enlil_devices::stealth::cpuid::CpuidStealthTable)
        /// is available, since it also fixes the topology leaves (and subsumes
        /// this) so the guest does not read the host's logical-processor count.
        ///
        /// # Errors
        /// Propagates [`KvmBackend::clear_cpuid_hypervisor_bit`].
        pub fn apply_cpuid_stealth(&mut self) -> Result<()> {
            self.backend.clear_cpuid_hypervisor_bit()
        }

        /// Apply the full topology view from `table` to **all** vCPUs created so
        /// far: the guest sees its own `vcpu_count` topology (leaf `0xB`, leaf-`1`
        /// max-IDs) instead of the host's, and the hypervisor-present bit is
        /// cleared. Call once after the vCPUs exist; supersedes
        /// [`apply_cpuid_stealth`](Self::apply_cpuid_stealth).
        ///
        /// # Errors
        /// Propagates [`KvmBackend::apply_topology_stealth`].
        pub fn apply_topology_stealth(
            &mut self,
            table: &enlil_devices::stealth::cpuid::CpuidStealthTable,
        ) -> Result<()> {
            self.backend.apply_topology_stealth(table)
        }

        /// Apply the architectural-PMU view (leaf `0xA`) from `table` to **all**
        /// vCPUs, so the guest enumerates a PMU consistent with the RDPMC shadow
        /// the run loop serves through [`StealthMsrRouter`]. Without it the guest
        /// reads leaf `0xA` as all-zero ("PMU version 0"), a VM tell that
        /// contradicts the counters it can reach via RDPMC. Compose with
        /// [`apply_topology_stealth`](Self::apply_topology_stealth) for full CPUID
        /// stealth; call once after the vCPUs exist.
        ///
        /// # Errors
        /// Propagates [`KvmBackend::apply_pmu_stealth`].
        ///
        /// [`StealthMsrRouter`]: crate::stealth_msr::StealthMsrRouter
        pub fn apply_pmu_stealth(
            &mut self,
            table: &enlil_devices::stealth::cpuid::CpuidStealthTable,
        ) -> Result<()> {
            self.backend.apply_pmu_stealth(table)
        }

        /// Run vCPU `index` for one entry, then keep the stealth surfaces in
        /// lockstep and drain any platform event.
        ///
        /// Calls [`run_vcpu_timed`](KvmBackend::run_vcpu_timed) (which advances
        /// the shared APERF/MPERF timing shadows around `KVM_RUN`), feeds the
        /// returned reference-cycle delta to the router's
        /// [`PmcState::advance_counters`](enlil_devices::stealth::pmc::PmcState::advance_counters)
        /// so the RDPMC surface tracks the same model rate, then polls the
        /// [`StandardPc`] platform-event latches. The timing shadows are
        /// advanced *once* (inside `run_vcpu_timed`, via the shared handle); the
        /// run loop only advances the non-shared PMC counters here, so the two
        /// surfaces never double-count.
        ///
        /// # Errors
        /// Propagates [`KvmBackend::run_vcpu_timed`].
        pub fn run_vcpu_once(&mut self, index: usize) -> Result<RunStep> {
            let (exit, guest_cycles) =
                self.backend
                    .run_vcpu_timed(index, &mut self.pc.bus, &self.timing, &self.model)?;
            // The timing shadows were advanced inside run_vcpu_timed via the
            // shared Arc; advance only the (non-shared) PMC counters by the same
            // delta so RDPMC and APERF/MPERF stay consistent.
            if let Some(router) = self.pc.bus.stealth_msr_mut() {
                router.pmc.advance_counters(guest_cycles);
            }
            let event = self.pc.poll_platform_events();
            Ok(RunStep {
                exit,
                guest_cycles,
                event,
            })
        }

        /// Drive vCPU `index` until it reaches a stopping point — the guest
        /// halts or raises a platform event (see [`RunStep::is_stop`]) — or
        /// `max_entries` entries elapse, whichever comes first.
        ///
        /// Returns the final [`RunStep`] and the total guest reference cycles
        /// summed across every entry. `max_entries` is the bound that keeps a
        /// guest which never halts (e.g. one that spins, or idles in `HLT`
        /// under an in-kernel IRQ chip) from looping forever here; a real
        /// driver pairs this with [`KvmBackend::set_immediate_exit`] from a
        /// watchdog, but the bound alone makes the synchronous loop terminating
        /// and testable.
        ///
        /// # Errors
        /// Propagates [`run_vcpu_once`](Self::run_vcpu_once).
        pub fn run_vcpu_until_event(
            &mut self,
            index: usize,
            max_entries: usize,
        ) -> Result<(RunStep, u64)> {
            let mut total_cycles = 0u64;
            let mut last = RunStep {
                exit: GuestExit::Interrupted,
                guest_cycles: 0,
                event: None,
            };
            for _ in 0..max_entries {
                let step = self.run_vcpu_once(index)?;
                total_cycles = total_cycles.saturating_add(step.guest_cycles);
                let stop = step.is_stop();
                last = step;
                if stop {
                    break;
                }
            }
            Ok((last, total_cycles))
        }

        /// Drive a real-mode guest on vCPU `index`, *acting on* the platform
        /// events it raises the way real hardware does, until it shuts down,
        /// halts, or `max_entries` entries elapse.
        ///
        /// The vCPU must already be prepared at its initial entry (e.g. via
        /// [`KvmBackend::prepare_real_mode_vcpu`] on
        /// [`backend_mut`](Self::backend_mut)). Per entry, after
        /// [`run_vcpu_once`](Self::run_vcpu_once):
        ///
        /// - [`PlatformEvent::Reset`] (a `0x92` / `0xCF9` CPU reset) → re-prepare
        ///   the vCPU to `reset_entry` and continue — the guest reboots, exactly
        ///   as a real platform restarts the boot CPU at its reset vector.
        /// - [`PlatformEvent::Sleep`] (an ACPI `SLP_EN` commit) → return
        ///   [`LoopOutcome::Shutdown`] with the `SLP_TYP` (`5` = power off).
        /// - [`GuestExit::Halted`] with no event → return [`LoopOutcome::Halted`].
        ///
        /// `reset_entry` is usually the same guest-physical address the vCPU was
        /// first prepared at (firmware reset vector). Returns
        /// [`LoopOutcome::Exhausted`] if the bound is hit first.
        ///
        /// # Errors
        /// Propagates [`run_vcpu_once`](Self::run_vcpu_once) and
        /// [`KvmBackend::prepare_real_mode_vcpu`].
        pub fn run_real_mode(
            &mut self,
            index: usize,
            reset_entry: u64,
            max_entries: usize,
        ) -> Result<LoopOutcome> {
            for _ in 0..max_entries {
                let step = self.run_vcpu_once(index)?;
                match step.event {
                    Some(PlatformEvent::Sleep(slp_typ)) => {
                        return Ok(LoopOutcome::Shutdown(slp_typ));
                    }
                    Some(PlatformEvent::Reset) => {
                        // Reboot: restart the boot vCPU at its reset vector.
                        self.backend.prepare_real_mode_vcpu(index, reset_entry)?;
                        continue;
                    }
                    None => {}
                }
                if step.exit == GuestExit::Halted {
                    return Ok(LoopOutcome::Halted);
                }
            }
            Ok(LoopOutcome::Exhausted)
        }

        /// The shared timing handle the run loop drives — for a watchdog or test
        /// to read the live APERF/MPERF shadows.
        #[must_use]
        pub fn timing(&self) -> &Arc<VcpuTimingState> {
            &self.timing
        }

        /// The rate model both stealth surfaces advance by.
        #[must_use]
        pub const fn model(&self) -> PmcRateModel {
            self.model
        }

        /// Mutable access to the owned [`KvmBackend`] — for guest setup the run
        /// loop does not own (mapping memory, preparing vCPU registers).
        pub fn backend_mut(&mut self) -> &mut KvmBackend {
            &mut self.backend
        }

        /// Mutable access to the owned [`StandardPc`] — for device setup and
        /// inspection (the bus is also the exit handler `run_vcpu_once` binds).
        pub fn pc_mut(&mut self) -> &mut StandardPc {
            &mut self.pc
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::device_bus::DeviceBus;
    use crate::kvm_backend::{is_kvm_available, GuestExit, GuestRam, KvmBackend, VmExitHandler};
    use crate::serial::{SerialOutput, SerialOutputMode};
    use enlil_devices::stealth::lbr::LbrPlatform;
    use enlil_devices::stealth::timing::msr as timing_msr;
    use std::sync::{Arc, Mutex};

    // End-to-end through the driver: install the stealth stack, seed APERF, and
    // a real-mode guest that `rdmsr APERF; out 0x3F8` reads back the seeded low
    // byte — the same proof as device_bus's hand-wired full_stealth_stack test,
    // but routed entirely through StealthRunLoop::{install, run_vcpu_once}. This
    // is the encapsulation contract: the driver wires the MSR filter + router +
    // timing exactly as the manual sequence did.
    #[test]
    fn run_loop_serves_seeded_aperf_to_a_guest() {
        if !is_kvm_available() {
            eprintln!("skipping run_loop_serves_seeded_aperf_to_a_guest: no /dev/kvm");
            return;
        }

        // rdmsr(IA32_APERF=0xE8); out 0x3F8, al; hlt.
        #[rustfmt::skip]
        let code: [u8; 13] = [
            0x66, 0xB9, 0xE8, 0x00, 0x00, 0x00,
            0x0F, 0x32,
            0xBA, 0xF8, 0x03,
            0xEE,
            0xF4,
        ];
        const ENTRY: u64 = 0x1000;
        const APERF: u32 = 0xE8;

        let sink = Arc::new(Mutex::new(Vec::new()));
        let pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Shared(Arc::clone(&sink))),
            0,
            1,
        )
        .expect("build standard pc");

        let backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        let mut run = match StealthRunLoop::install(backend, pc, LbrPlatform::AmdSvm) {
            Ok(run) => run,
            Err(e) => {
                eprintln!("skipping run_loop_serves_seeded_aperf_to_a_guest: {e}");
                return;
            }
        };

        // Seed APERF with a known value (low byte 0xBE) the guest will read.
        run.timing().write_aperf(0x0000_0000_0000_00BE);

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();
        // SAFETY: `ram` outlives `run` within this test scope.
        unsafe {
            run.backend_mut()
                .map_memory(ENTRY, host_addr, ram.len() as u64)
        }
        .expect("map guest memory");
        run.create_vcpu(0).expect("create vcpu");
        run.apply_cpuid_stealth().expect("clear hypervisor bit");
        run.backend_mut()
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let mut saw_aperf = false;
        let mut halted = false;
        for _ in 0..100 {
            let step = run.run_vcpu_once(0).expect("run vcpu once");
            if let GuestExit::MsrRead { msr } = step.exit {
                if msr == APERF {
                    saw_aperf = true;
                }
            }
            if step.exit == GuestExit::Halted {
                halted = true;
                break;
            }
        }
        assert!(halted, "guest never reached HLT through the run loop");
        assert!(saw_aperf, "APERF was not forwarded through the run loop");
        assert_eq!(&*sink.lock().unwrap(), &[0xBE]);
    }

    // Full stealth through the driver: install the MSR stack, create two vCPUs,
    // apply the topology table, and a guest reading leaf 0xB subleaf 1 sees its
    // own 2-vCPU count (not the host's). Proves apply_topology_stealth is
    // reachable from the production path alongside the MSR/timing wiring.
    #[test]
    fn run_loop_applies_topology_stealth_to_a_multi_vcpu_guest() {
        use enlil_devices::stealth::cpuid::{CpuidStealthConfig, CpuidStealthTable};

        if !is_kvm_available() {
            eprintln!(
                "skipping run_loop_applies_topology_stealth_to_a_multi_vcpu_guest: no /dev/kvm"
            );
            return;
        }

        // 16-bit real-mode: cpuid(0xB, 1); out 0x3F8, bl; hlt.
        #[rustfmt::skip]
        let code: [u8; 21] = [
            0x66, 0xB8, 0x0B, 0x00, 0x00, 0x00, // mov eax, 0xB
            0x66, 0xB9, 0x01, 0x00, 0x00, 0x00, // mov ecx, 1
            0x0F, 0xA2,                         // cpuid
            0x88, 0xD8,                         // mov al, bl
            0xBA, 0xF8, 0x03,                   // mov dx, 0x3F8
            0xEE,                               // out dx, al
            0xF4,                               // hlt
        ];
        const ENTRY: u64 = 0x1000;
        const GUEST_VCPUS: u32 = 2;

        let sink = Arc::new(Mutex::new(Vec::new()));
        let pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Shared(Arc::clone(&sink))),
            0,
            1,
        )
        .expect("build standard pc");
        let backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        let mut run = match StealthRunLoop::install(backend, pc, LbrPlatform::AmdSvm) {
            Ok(run) => run,
            Err(e) => {
                eprintln!("skipping run_loop_applies_topology_stealth_...: {e}");
                return;
            }
        };

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();
        // SAFETY: `ram` outlives `run` within this test scope.
        unsafe {
            run.backend_mut()
                .map_memory(ENTRY, host_addr, ram.len() as u64)
        }
        .expect("map guest memory");
        run.create_vcpu(0).expect("create vcpu 0");
        run.create_vcpu(1).expect("create vcpu 1");
        let table = CpuidStealthTable::build(&CpuidStealthConfig::from_host(GUEST_VCPUS, 1));
        run.apply_topology_stealth(&table)
            .expect("apply topology stealth");
        run.backend_mut()
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let (last, _) = run.run_vcpu_until_event(0, 100).expect("run until event");
        assert_eq!(last.exit, GuestExit::Halted, "guest should reach HLT");
        assert_eq!(
            &*sink.lock().unwrap(),
            &[GUEST_VCPUS as u8],
            "guest reads its own vCPU count from leaf 0xB through the driver"
        );
    }

    // The driver keeps both stealth surfaces in lockstep: after running a guest
    // that executes real cycles, the run-loop-driven RDPMC counters and the
    // shared APERF/MPERF shadows both advanced and both encode the model ratio —
    // proving run_vcpu_once advances the (non-shared) PMC exactly once per entry
    // alongside the timing shadows, with no double-count.
    #[test]
    fn run_loop_keeps_pmc_and_timing_in_lockstep() {
        if !is_kvm_available() {
            eprintln!("skipping run_loop_keeps_pmc_and_timing_in_lockstep: no /dev/kvm");
            return;
        }

        // Real-mode: three COM1 writes (three IO exits) then HLT.
        #[rustfmt::skip]
        let code: [u8; 9] = [0xB0, 0x41, 0xBA, 0xF8, 0x03, 0xEE, 0xEE, 0xEE, 0xF4];
        const ENTRY: u64 = 0x1000;

        let pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            0,
            1,
        )
        .expect("build standard pc");
        let backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        let mut run = match StealthRunLoop::install(backend, pc, LbrPlatform::AmdSvm) {
            Ok(run) => run,
            Err(e) => {
                eprintln!("skipping run_loop_keeps_pmc_and_timing_in_lockstep: {e}");
                return;
            }
        };

        // Enable the core+ref fixed counters so RDPMC advances (a guest does
        // this via IA32_FIXED_CTR_CTRL=0x330 / GLOBAL_CTRL); set it on the
        // router's PMC directly for the test.
        run.pc_mut()
            .bus
            .stealth_msr_mut()
            .expect("router installed")
            .pmc
            .fixed_ctr_ctrl = 0x330;

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();
        // SAFETY: `ram` outlives `run` within this test scope.
        unsafe {
            run.backend_mut()
                .map_memory(ENTRY, host_addr, ram.len() as u64)
        }
        .expect("map guest memory");
        run.create_vcpu(0).expect("create vcpu");
        run.backend_mut()
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let (last, total_cycles) = run.run_vcpu_until_event(0, 100).expect("run until event");
        assert_eq!(last.exit, GuestExit::Halted, "guest should reach HLT");
        assert!(last.is_stop(), "HLT is a stopping point");
        assert!(total_cycles > 0, "run loop measured guest cycles");

        let model = run.model();
        let aperf = run.timing().read_aperf();
        let mperf = run.timing().read_mperf();
        assert!(aperf > 0 && mperf > 0, "timing shadows advanced");
        let ratio = aperf as f64 / mperf as f64;
        let model_ratio = model.core_per_kilo_ref as f64 / 1000.0;
        assert!(
            (ratio - model_ratio).abs() < 0.02,
            "APERF/MPERF ratio {ratio} tracks the model {model_ratio}"
        );

        // RDPMC core/ref, driven by the same per-entry deltas, tracks the same
        // model ratio — the two surfaces stayed in lockstep through the loop.
        let pmc = &run
            .pc_mut()
            .bus
            .stealth_msr_mut()
            .expect("router installed")
            .pmc;
        let pmc_core = pmc.read_pmc(0x4000_0001);
        let pmc_ref = pmc.read_pmc(0x4000_0002);
        assert!(pmc_core > 0 && pmc_ref > 0, "RDPMC advanced with the guest");
        assert!(
            (pmc_core as f64 / pmc_ref as f64 - model_ratio).abs() < 0.02,
            "RDPMC core/ref tracks the model ratio like APERF/MPERF"
        );

        // The shared timing handle and the bus agree on APERF — the run loop
        // drove the one the guest reads.
        assert_eq!(
            run.pc_mut().bus.rdmsr(timing_msr::IA32_APERF),
            Some(aperf),
            "the bus serves the same APERF the run loop advanced"
        );
    }

    // run_real_mode reboots on a 0xCF9 CPU reset: a guest that emits 'A' then
    // writes the RST_CNT reboot value to 0xCF9 is re-prepared at a second entry
    // that emits 'B' and halts — proving the loop acts on PlatformEvent::Reset by
    // restarting the boot vCPU at its reset vector, not just surfacing the event.
    #[test]
    fn run_real_mode_reboots_on_a_cf9_reset() {
        if !is_kvm_available() {
            eprintln!("skipping run_real_mode_reboots_on_a_cf9_reset: no /dev/kvm");
            return;
        }

        // Entry A @ 0x1000: out 0x3F8,'A'; out 0xCF9, 0x06 (SYS_RST|RST_CPU); hlt.
        #[rustfmt::skip]
        let code_a: [u8; 11] = [
            0xB0, 0x41,             // mov al, 'A'
            0xBA, 0xF8, 0x03,       // mov dx, 0x3F8
            0xEE,                   // out dx, al
            0xBA, 0xF9, 0x0C,       // mov dx, 0xCF9
            0xB0, 0x06,             // mov al, 0x06 (RST_CNT reboot)
            // (out dx,al on next byte)
        ];
        // The reboot OUT + a trailing hlt that is never reached (reset re-points
        // RIP before the next entry runs).
        #[rustfmt::skip]
        let code_a_tail: [u8; 2] = [0xEE, 0xF4]; // out dx, al ; hlt
                                                 // Entry B @ 0x1100: out 0x3F8,'B'; hlt.
        #[rustfmt::skip]
        let code_b: [u8; 6] = [0xB0, 0x42, 0xBA, 0xF8, 0x03, 0xEE]; // mov al,'B'; mov dx,0x3F8; out
        const ENTRY_A: u64 = 0x1000;
        const ENTRY_B: u64 = 0x1100;

        let sink = Arc::new(Mutex::new(Vec::new()));
        let pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Shared(Arc::clone(&sink))),
            0,
            1,
        )
        .expect("build standard pc");
        let backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        let mut run = match StealthRunLoop::install(backend, pc, LbrPlatform::AmdSvm) {
            Ok(run) => run,
            Err(e) => {
                eprintln!("skipping run_real_mode_reboots_on_a_cf9_reset: {e}");
                return;
            }
        };

        let mut ram = GuestRam::new(0x2000);
        {
            let mem = ram.as_mut_slice();
            mem[..code_a.len()].copy_from_slice(&code_a);
            mem[code_a.len()..code_a.len() + code_a_tail.len()].copy_from_slice(&code_a_tail);
            // Memory maps at ENTRY_A, so guest-physical ENTRY_B is at this offset.
            let b = (ENTRY_B - ENTRY_A) as usize;
            mem[b..b + code_b.len()].copy_from_slice(&code_b);
            mem[b + code_b.len()] = 0xF4; // hlt
        }
        let host_addr = ram.host_addr();
        // SAFETY: `ram` outlives `run` within this test scope.
        unsafe {
            run.backend_mut()
                .map_memory(ENTRY_A, host_addr, ram.len() as u64)
        }
        .expect("map guest memory");
        run.create_vcpu(0).expect("create vcpu");
        run.backend_mut()
            .prepare_real_mode_vcpu(0, ENTRY_A)
            .expect("set real-mode entry");

        // On reset, reboot to entry B (the test's "reset vector").
        let outcome = run.run_real_mode(0, ENTRY_B, 100).expect("run real mode");
        assert_eq!(outcome, LoopOutcome::Halted, "guest halts after the reboot");
        assert_eq!(
            &*sink.lock().unwrap(),
            b"AB",
            "guest emitted 'A', rebooted via 0xCF9, then emitted 'B'"
        );
    }

    // run_real_mode returns Shutdown on an ACPI S5 commit: a guest that writes
    // SLP_TYP=5 | SLP_EN to PM1a_CNT (0x604) powers off — the loop surfaces it as
    // LoopOutcome::Shutdown(5), the _S5 value the synthesized DSDT advertises.
    #[test]
    fn run_real_mode_shuts_down_on_acpi_s5() {
        if !is_kvm_available() {
            eprintln!("skipping run_real_mode_shuts_down_on_acpi_s5: no /dev/kvm");
            return;
        }

        // out 0x604, ax where ax = (5<<10)|(1<<13) = 0x3400 (SLP_TYP=5, SLP_EN).
        #[rustfmt::skip]
        let code: [u8; 8] = [
            0xBA, 0x04, 0x06,       // mov dx, 0x604
            0xB8, 0x00, 0x34,       // mov ax, 0x3400
            0xEF,                   // out dx, ax (word)
            0xF4,                   // hlt (not reached)
        ];
        const ENTRY: u64 = 0x1000;
        const S5: u8 = 5;

        let pc = DeviceBus::standard_pc_complete(
            SerialOutput::new("guest", SerialOutputMode::Null),
            0,
            1,
        )
        .expect("build standard pc");
        let backend = KvmBackend::new_without_irqchip().expect("create KVM VM");
        let mut run = match StealthRunLoop::install(backend, pc, LbrPlatform::AmdSvm) {
            Ok(run) => run,
            Err(e) => {
                eprintln!("skipping run_real_mode_shuts_down_on_acpi_s5: {e}");
                return;
            }
        };

        let mut ram = GuestRam::new(0x1000);
        ram.as_mut_slice()[..code.len()].copy_from_slice(&code);
        let host_addr = ram.host_addr();
        // SAFETY: `ram` outlives `run` within this test scope.
        unsafe {
            run.backend_mut()
                .map_memory(ENTRY, host_addr, ram.len() as u64)
        }
        .expect("map guest memory");
        run.create_vcpu(0).expect("create vcpu");
        run.backend_mut()
            .prepare_real_mode_vcpu(0, ENTRY)
            .expect("set real-mode entry");

        let outcome = run.run_real_mode(0, ENTRY, 100).expect("run real mode");
        assert_eq!(
            outcome,
            LoopOutcome::Shutdown(S5),
            "guest committed an S5 power-off via PM1a_CNT"
        );
    }
}

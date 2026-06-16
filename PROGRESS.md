# Enlil — Progress Log

Running log for the autonomous daily routine. Newest entry first. Each entry
records what was done, the research that informed it, exact test results, and
the recommended next step so the next run (which has no memory) can resume.

---

## 2026-06-16 — Session: the production run loop comes together — StealthRunLoop, live-guest topology stealth, reboot/shutdown handling (Phase 5)

**7 code/doc increments, each independently green and committed** (branch
`routine/enlil-2026-06-16`). This session built the **production vCPU run-loop
driver** the Phase-5 stealth primitives were always pointing at, applied **CPUID
topology stealth** to the live guest, and made the loop **act on** the guest's
reboot/shutdown events — all proven on `/dev/kvm` (read-writable this run,
`KVM_RW_OK`). It also fixed a latent timing-shadow bug the run loop surfaced and
corrected the nightly-rustfmt drift on `master`.

### Increments (commit — what)
1. `4daa018` — **`on_vmresume` must not zero the shadows before the first exit.**
   The timed loop calls `on_vmresume` before any `on_vmexit`; `last_exit_tsc` is
   still its init `0` (indistinguishable from a real exit at TSC 0, which the unit
   tests use), so `entry_tsc - 0` was a huge spurious overhead that zeroed APERF/
   MPERF on the first entry — wiping any seeded value. Added an `exit_seen` flag
   set by `on_vmexit`; the first `on_vmresume` records only the RIP and returns.
2. `26ff64d` — **`enlil-core::run_loop::StealthRunLoop`** — the production driver.
   Owns the `KvmBackend` + `StandardPc`; `install` wires the router on the bus then
   (KVM-required order, before any `KVM_RUN`) `enable_userspace_msr_exits` +
   `forward_msrs_to_userspace(router.filter_ranges())`; `run_vcpu_once` calls
   `run_vcpu_timed`, advances the (non-shared) PMC by the returned delta (timing
   advanced once via the shared `Arc` — no double-count), and drains the platform
   events into a `RunStep`; `run_vcpu_until_event` is a bounded loop. `target_os =
   linux`-only.
3. `2af6030` — **`KvmBackend::apply_topology_stealth(&CpuidStealthTable)`** —
   overrides leaf `0xB` + leaf-`1` max-IDs with the guest topology and clears the
   hypervisor bit. AMD KVM omits leaf `0xB` from `GET_SUPPORTED_CPUID`, so it
   *rebuilds* the array via `CpuId::from_entries`, adding the `0xB` subleaves with
   `SIGNIFCANT_INDEX` when absent rather than patching in place.
4. `d8f2bb4` — **wire `apply_topology_stealth` into `StealthRunLoop`** so the
   production path can apply full CPUID stealth (topology + hypervisor bit).
5. `7d6d688` — **docs**: ROADMAP 5.3 (topology applied to live guest) + 5.4
   (production run loop assembled) + RESEARCH 2026-06-16 (the two measured facts).
6. `4b7466b` — **`StealthRunLoop::run_real_mode`** + `LoopOutcome` — the managed
   loop *acts on* events: `0x92`/`0xCF9` reset → reboot the vCPU to its reset
   vector; ACPI `SLP_EN` → `Shutdown(slp_typ)` (`_S5` = power off); `HLT` →
   `Halted`; bounded → `Exhausted`.
7. `a6c7bf4` — **`cargo fmt --all`**: the unpinned `nightly` channel drifted —
   `stealth_msr.rs` (merged green in PR #29 under an older nightly) is now flagged
   by the current nightly rustfmt, as was this session's new code. Pure formatting.
8. `adcade5` — **docs**: ROADMAP 5.4 — `run_real_mode` closes the reset/sleep
   remaining item; only the threaded watchdog is left.

### Research (informed the build)
RESEARCH.md `2026-06-16`: (a) the `on_vmresume` first-entry zeroing bug and the
`exit_seen` fix; (b) **AMD KVM omits the Intel-style leaf `0xB`** from
`GET_SUPPORTED_CPUID` (measured: a 2-vCPU guest read it all-zero), forcing the
`CpuId::from_entries` rebuild path; `KVM_CPUID_FLAG_SIGNIFCANT_INDEX = 1`
(kvm-bindings 0.10, `CpuId = FamStructWrapper<kvm_cpuid2>`); (c) the run-loop
lockstep rationale (timing once via the shared `Arc`, PMC once in the loop).
No new third-party research — KVM API + the in-tree stealth specs from prior runs.

### Test results (exact)
- **`/dev/kvm`: read-writable (`KVM_RW_OK`).** All KVM/guest-boot tests **ran for
  real** (none skipped). New real-KVM tests this session, all passing:
  `run_loop_serves_seeded_aperf_to_a_guest`,
  `run_loop_keeps_pmc_and_timing_in_lockstep`,
  `run_loop_applies_topology_stealth_to_a_multi_vcpu_guest`,
  `topology_stealth_makes_the_guest_see_its_own_cpu_count` (2-vCPU guest reads
  `cpuid(0xB,1).EBX == 2`, not the host's count),
  `run_real_mode_reboots_on_a_cf9_reset` (guest emits 'A', reboots via 0xCF9,
  emits 'B', halts), `run_real_mode_shuts_down_on_acpi_s5` (PM1a_CNT S5 →
  `Shutdown(5)`). Plus a non-KVM unit test
  `first_resume_without_a_prior_exit_does_not_zero_the_shadows`. Prior guest-boot
  tests still pass.
- `cargo test -p enlil-core --lib`: **181 passed, 0 failed** (started 174).
- `cargo test -p enlil-devices --lib`: **760 passed, 0 failed** (unchanged — not
  touched this session).
- `cargo test --workspace`: **exit 0** (all crates).
- `cargo clippy --all-targets --workspace -- -D warnings`: **exit 0**.
- `cargo fmt --all -- --check`: **clean** (after increment 7).
- **Toolchains:** all increments built/tested on **Linux/WSL** (nightly
  `x86_64-unknown-linux-gnu`, rustfmt 1.9.0-nightly 2026-06-12). Every increment
  touching `enlil-core` **also built Windows-native** (`x86_64-pc-windows-msvc`
  via the `D:\Development\.enlil-win-target` 9p path), **exit 0** each time (after
  increments 1+2, 3, 4, 6). The KVM internals are `#[cfg(target_os = "linux")]`,
  so Windows compiles the platform-agnostic parts (`GuestExit`/`VmExitHandler`,
  the run-loop module's empty cfg shell) — confirmed clean.
  **Caveat:** CI uses `dtolnay/rust-toolchain@nightly` (latest nightly at CI time,
  ≥ 2026-06-16); local rustfmt is 2026-06-12. The fmt commit makes the tip clean
  under 2026-06-12; a few-days-newer nightly *could* in principle want a different
  wrap, but no further drift was observed on these files.

### STOP REASON
**Remaining unblocked work is architectural or blocked, not a clean small
increment.** The Phase-5 stealth vertical is now assembled end-to-end (production
run loop + topology stealth + reboot/shutdown), and the only remaining run-loop
item is the **threaded-vCPU watchdog**, which needs a multi-threaded vCPU
execution model + signal kick — an architectural change I won't start and leave
half-tested before the PR (the guardrail: don't stretch one increment across the
"still broken" line). The other roadmap items are **blocked** (libusb
`UsbDeviceModel` forwarder needs real USB hardware; TUI USB tab is separate UI)
or **vacuous-to-test on this host** (brand/vendor CPUID spoof — KVM-supported
brand already matches the host CPU). Wall-clock budget was not fully spent
(~2h40m); per the guardrail I handed off rather than rush an architectural or
host-blocked feature. Every increment is independently green; `master` stays
buildable; the branch is CI-parity green (fmt + clippy + workspace test).

### Recommended next step (tomorrow)
1. **Threaded-vCPU watchdog** (the last run-loop piece): run each vCPU on its own
   thread with the backend behind shared state, install a signal handler, and have
   a watchdog thread `set_immediate_exit` + signal a vCPU thread to kick it out of
   a blocking `KVM_RUN` (the synchronous primitive already exists). Decide the
   shared-state model for the per-vCPU router/timing first (see #3).
2. **CPUID leaf `0xA` (PMU) on the live guest** — so the advertised PMU version /
   counter counts match the RDPMC shadow's (`PmcRateModel`); an all-zero leaf 0xA
   is itself a cloud-VM tell (2026-06-10 research). Extend `apply_topology_stealth`
   (or a sibling) — the `CpuId::from_entries` rebuild path already handles a leaf
   KVM may omit. Non-vacuous where the host PMU ≠ the model's counter counts.
3. **Per-vCPU stealth state**: the run loop shares ONE router/timing across all
   vCPUs today; a multi-vCPU guest needs per-vCPU APERF/MPERF/PMC/LBR (each vCPU
   reads its own counters). This is the prerequisite for #1's threading.
4. Lower priority / blocked: libusb-backed `UsbDeviceModel` forwarder (USB
   hardware), TUI USB tab (UI), brand/vendor CPUID spoof (only once Enlil presents
   a different CPU identity than the host).
5. **Housekeeping:** consider pinning the `nightly` toolchain in
   `rust-toolchain.toml` to stop the recurring rustfmt drift (unpinned `nightly`
   means CI's rustfmt can reformat already-merged files; this run had to re-fmt
   `stealth_msr.rs`).

---

## 2026-06-15 — Session: the Phase-5 MSR/CPUID stealth stack, end to end on real KVM

**13 code + 2 doc increments, each independently green and committed** (branch
`routine/enlil-2026-06-15`, PR https://github.com/physics515/enlil/pull/29).
This session built the entire MSR-exit / timing /
PMC / LBR / CPUID **stealth integration** for the live KVM backend — from the
exit-vocabulary seam up through a platform install seam and a run loop that
drives the shadow counters — and proved each layer on `/dev/kvm` (which was
read-writable this run, `KVM_RW_OK`). It then corrected three roadmap notes that
prior runs had already addressed but left marked "remaining."

### Increments (commit — what)
1. `8d4cf36` — **model guest MSR exits in the KVM run-loop seam.** `GuestExit::MsrRead`/
   `MsrWrite`, `VmExitHandler::rdmsr`/`wrmsr` (default: refuse → `#GP`), dispatch of
   `VcpuExit::X86Rdmsr`/`X86Wrmsr`, and `KvmBackend::enable_userspace_msr_exits`
   (`KVM_CAP_X86_USER_SPACE_MSR`, unknown|filter reasons).
2. `fcba8e6` — **`enlil-core::stealth_msr::StealthMsrRouter`**: maps the MSR number space
   onto the stealth shadows (APERF/MPERF ↔ `VcpuTimingState`, PMC ↔ `PmcState`, LBR ↔
   `LbrState`) from one `PmcRateModel`. 9 unit tests incl. the APERF/MPERF↔RDPMC cross-check.
3. `5eaec2e` — **serve stealth MSRs through `DeviceBus`** (`rdmsr`/`wrmsr` delegate to the
   installed router; `set_stealth_msr_router`/`stealth_msr_mut`).
4. `785f3a8` — **AMD last-branch-record stealth** (this host is AMD SVM): `LbrState` gains
   the single LastBranch/LastInt pair (0x1DB–0x1DE); `sanitize_after_exit` branches on
   platform (AMD erases the branch pair, Intel the 32-entry stack); router routes the AMD MSRs.
5. `19553a4` — **clear the CPUID hypervisor-present bit on the live guest**
   (`clear_cpuid_hypervisor_bit`: `GET_SUPPORTED_CPUID` → clear leaf 1 ECX[31] → `SET_CPUID2`).
6. `c542889` — **`StandardPc::install_stealth_msr_router`**: one-call platform seam that
   builds + seeds the router (model ratio, not the 1.0 tell) and returns the shared timing `Arc`.
7. `2c9a251` — **docs** (RESEARCH 2026-06-15 + ROADMAP 5.3/5.4) grounding the seam in
   the KVM API / Intel SDM / AMD APM, with the measured KVM facts.
8. `8489754` — **`run_vcpu_timed`**: drive the timing shadows around `KVM_RUN`
   (`on_vmresume`/`advance`/`on_vmexit`) so APERF/MPERF hide exit overhead, ratio preserved.
9. `fb795f8` — **`KVM_X86_SET_MSR_FILTER`** (`forward_msrs_to_userspace`, raw ioctl via
   `vmm_sys_util::ioctl_iow_nr!`): forward the *KVM-known* stealth MSRs (APERF/MPERF/PMC/
   DEBUGCTL), which the cap alone does not reach.
10. `73e268b` — **docs** ROADMAP 5.4 (filter + timed loop landed).
11. `8ac6c14` — **`StealthMsrRouter::filter_ranges`**: platform-correct `(base,count)` MSR
    ranges — one source of truth shared by the router and the KVM filter (AMD vs Intel LBR).
12. `1eef647` — **end-to-end stealth stack test**: a guest `rdmsr` APERF reads the run-loop-
    seeded shadow (0xBE) through install + filter + DeviceBus router, echoed out COM1.
13. `3450094` — **`StealthMsrRouter::advance`**: drive APERF/MPERF and RDPMC from the same
    model + delta in one call.
14. `b94872a` — **`run_vcpu_timed` returns the guest-cycle delta** so the run loop drives the
    (non-shared) PMC counters in lockstep with the timing shadows.
15. `be3f462` — **docs**: corrected stale ROADMAP 5.1 (`_SRS`/`_DIS` PIRQRC reprogramming is
    already implemented + verified) and 5.2 (`enlil-core::smbios` duplicate already removed).

### Research (informed the build)
RESEARCH.md `2026-06-15`: KVM API (`KVM_CAP_X86_USER_SPACE_MSR` / `KVM_X86_SET_MSR_FILTER`,
`api.rst`); Intel SDM 3B / AMD APM on `IA32_APERF`/`MPERF` and the effective-frequency ratio
an IET detector reads; AMD APM Vol. 2 LBRV (single last-branch pair, 0x1DB–0x1DE); the CPUID
hypervisor-present bit. No new third-party research — these are the authoritative primary specs.

### Test results (exact)
- **`/dev/kvm`: read-writable (`KVM_RW_OK`).** All KVM/guest-boot tests **ran for real**
  (none skipped). New real-KVM tests this session, all passing:
  `rdmsr_is_forwarded_and_value_round_trips_to_guest`,
  `cpuid_stealth_installs_real_features_without_the_hypervisor_tell` (guest reads ECX[31]=0,
  EDX[4]/TSC=1), `msr_filter_forwards_a_kvm_known_msr` (APERF forwarded, 0xCD round-trips),
  `run_vcpu_timed_advances_shadows_at_the_model_ratio` (APERF/MPERF + RDPMC at the 1.15
  model ratio), `full_stealth_stack_serves_seeded_aperf_to_a_guest` (seeded 0xBE read back).
  Prior guest-boot tests (`serial_console_smoke`, `serial_input_path_smoke`, `mmio_path_smoke`,
  `immediate_exit_bounds_an_unending_run`, `guest_memory_*`) still pass.
- `cargo test -p enlil-core --lib`: **174 passed, 0 failed** (started 154).
- `cargo test -p enlil-devices --lib`: **760 passed, 0 failed** (started 756; +9 LBR/router).
- `cargo clippy` clean for `enlil-core` and `enlil-devices` (`--lib --tests`).
- **Toolchains:** all increments built/tested on **Linux/WSL** (nightly `x86_64-unknown-linux-gnu`);
  every increment touching the host-agnostic surface (`enlil-core` platform-agnostic exit
  model, `enlil-devices` `stealth::lbr`) **also built Windows-native** (`x86_64-pc-windows-msvc`
  via the `D:\Development\.enlil-win-target` 9p path), exit 0. The KVM backend internals
  (`#[cfg(target_os = "linux")]`) are Linux-only, so Windows compiles the agnostic parts only —
  confirmed clean each time.

### STOP REASON
**No tractable, cleanly-decomposable unblocked item remained in the in-flight phases** — the
wall-clock budget was *not* fully spent (~1 h elapsed). After completing the stealth vertical I
swept the codebase: there are **no `todo!`/`unimplemented!`/`FIXME` markers**, and the items the
roadmap still listed as "remaining" in Phases 4/5 are either already done (5.1 `_SRS`/`_DIS`,
5.2 SMBIOS dedup — corrected this run), **architectural / multi-session** (the production
run-loop *driver* that wires `enable_userspace_msr_exits` + `forward_msrs_to_userspace` +
`install_stealth_msr_router` together and ticks `advance` each iteration; the threaded-vCPU
watchdog), **hardware-blocked** (the libusb `UsbDeviceModel` forwarder needs real USB hardware),
**UI** (the TUI USB tab), or **vacuous-to-test on this host** (merging the full
`CpuidStealthTable` into KVM's supported set — KVM-supported already matches the host CPU for
the non-hypervisor leaves, so there is no observable delta to assert here). Per the guardrail I
handed off rather than start an architectural change or a hardware-/host-blocked feature I
couldn't finish and test cleanly. Every increment is independently green; `master` stays buildable.

### Recommended next step (tomorrow)
1. **Production run-loop driver** (the highest-value next step): a struct/fn that owns a
   `KvmBackend` + `StandardPc`, calls (in the right order) `create_vcpu` →
   `clear_cpuid_hypervisor_bit` and, before vCPUs, `enable_userspace_msr_exits` +
   `forward_msrs_to_userspace(router.filter_ranges())`, then loops
   `run_vcpu_timed` → feed the returned delta to `stealth_msr_mut().advance_counters` →
   `service_usb_dma` / `poll_platform_events` / `advance_clocks`. This is the architectural
   piece the stealth plumbing now waits on. Consider sharing `PmcState` (Arc) to let the loop
   advance both surfaces in one place, or keep the delta-return pattern.
2. **Threaded-vCPU watchdog** (signal-kick from a dedicated thread; the synchronous
   `set_immediate_exit` primitive already exists).
3. **libusb-backed `UsbDeviceModel` forwarder** (gate on real USB hardware).
4. **TUI USB tab** (the `enlil-mgmt::protocol` seam is ready).
5. Lower priority: merge the full `CpuidStealthTable` (vendor/brand/topology) into KVM's
   supported set (needs a `CpuidStealthTable::get(leaf,sub) -> Option<_>` accessor; test
   non-vacuously by shaping topology leaf 0xB for a >1-vCPU guest).



**6 increments, each independently green and committed** (branch
`routine/enlil-2026-06-14-2`, PR https://github.com/physics515/enlil/pull/28;
same-day rerun after PR #27 merged). This session
delivered **both** documented next-steps from the earlier run: guest-resident
transfer rings (the last xHCI ring still sourced from an internal queue) and the
production run-loop watchdog primitive.

### Increments (commit — what)
1. `4a33750` — **drive transfer rings from guest memory (opt-in).** Wires
   `gather_transfer_td` into the live path: `process_transfer_ring` drains the
   internal `submit_transfer` ring (refactored into `drain_internal_transfer_ring`)
   then, behind `set_guest_resident_transfers` (default OFF — all 753 tests
   unaffected), runs `process_guest_transfer_ring`: a persistent per-`(slot,dci)`
   `GuestRingCursor` gathers TDs and feeds the existing executors, bounded by
   `TRANSFER_BURST_LIMIT`, stopping at a STALL. Cursor invalidated at Configure
   Endpoint, Set TR Dequeue Pointer, Reset Device, Disable Slot.
2. `a4a17a8` — **enable guest-resident transfers on the run-loop xHCI**
   (`standard_pc_complete`) + a StandardPc end-to-end test: address a loopback,
   write a No-Op into EP0's guest ring, ring the doorbell over the BAR, and
   `service_usb_dma` completes it — no `submit_transfer`.
3. `7bd46e7` — **data-moving test**: a full GET_DESCRIPTOR control transfer
   (Setup→Data→Status, 3 TDs) fetched from the guest ring in one doorbell DMAs
   the device descriptor into a guest buffer — proving multi-TD cursor advance +
   real data movement, not just NoOps.
4. `dcb2c33` — **Set TR Dequeue Pointer works for guest-resident endpoints.**
   It used to ContextStateError without an internal ring; EP0 (context, no
   internal ring) could never be repointed. Now accepts an endpoint that exists
   as either an internal ring or a declared context, updates the context, and
   drops the cursor. Lifecycle test: consume from ring A, SetTRDequeue to ring B,
   next doorbell fetches from B.
5. `27d62f8` — **docs**: ROADMAP status (every xHCI ring now guest-resident;
   remaining Phase 4 = libusb forwarder + TUI tab) and a RESEARCH note grounding
   it in xHCI 1.2 §4.9 / §6.2.3.
6. `48ad93e` — **immediate-exit run-loop bound** (`KvmBackend::set_immediate_exit`):
   with the in-kernel IRQ chip a guest that idles in `HLT` (or spins) blocks
   `KVM_RUN` forever; armed, `run_vcpu` returns Interrupted at once. The
   synchronous watchdog primitive flagged in the 2026-06-14 RESEARCH note. Test:
   a `jmp $` guest that would hang is bounded.

### Research (informed the build)
RESEARCH.md `2026-06-14 (b)`: xHCI 1.2 §4.9 (transfer rings in guest memory) +
§6.2.3 (endpoint-context TR Dequeue Pointer), cross-checked vs ACRN's
doorbell-deferred processing. Drove the `GuestRingCursor`-per-endpoint design
and the Set-TR-Dequeue-for-context-only-endpoints fix.

### Test results (exact)
- **`/dev/kvm`: read-writable** (`KVM_RW_OK`). KVM tests ran for real:
  `immediate_exit_bounds_an_unending_run` (the `jmp $` guest is bounded, not
  hung), plus all prior guest-boot tests still pass.
- `cargo test -p enlil-devices --lib`: **756 passed, 0 failed** (started at 753;
  +gather-already-landed, +guest-resident control + cursor lifecycle tests).
- `cargo test -p enlil-core --lib`: **154 passed, 0 failed**.
- `cargo clippy` clean for `enlil-core` and `enlil-devices` (`--lib --tests`).
- **Toolchains:** all Linux/WSL (nightly `x86_64-unknown-linux-gnu`); the
  host-agnostic production increments (1, 2, 4) **also built Windows-native**
  (`x86_64-pc-windows-msvc`, exit 0). Increment 3 is test-only (cross-platform
  APIs); increment 6 is `#[cfg(target_os = "linux")]` (KVM), so Windows N/A.

### STOP REASON
Both concrete next-steps from PR #27 are done and the guest-resident transfer
feature is comprehensively tested. The remaining work does not decompose into a
clean, testable increment now: the **full threaded watchdog** (signal-kick from
a dedicated vCPU thread) needs a multi-threaded vCPU execution model — an
architectural change to the synchronous `run_vcpu`, multi-session; the **libusb
`UsbDeviceModel` forwarder** needs real USB hardware enumeration not available
in this headless WSL env; the **TUI USB tab** is separate UI work. Wall-clock
budget was not fully spent, but per the guardrail I handed off rather than rush
an architectural change or start a hardware-blocked feature.

### Recommended next step (tomorrow)
1. **Threaded vCPU run model + watchdog**: run each vCPU on its own thread with
   the backend behind shared state, install a signal handler, and have a
   watchdog thread `set_immediate_exit` + signal the vCPU thread to kick it out
   of `KVM_RUN` (the primitive landed this session is the synchronous half).
2. **libusb-backed `UsbDeviceModel` forwarder** for physical pass-through
   (needs a host with a spare USB device; gate the tests on hardware).
3. **TUI USB tab** (the `enlil-mgmt::protocol` seam is ready).

## 2026-06-14 — Session: the KVM run loop comes alive — guest-boot proofs, GuestMemory DMA, and the xHCI run-loop seams (Phase 0.2 / 4 / 5)

**11 increments, each independently green and committed** (branch
`routine/enlil-2026-06-14`, PR https://github.com/physics515/enlil/pull/27).
First run with `/dev/kvm` actually read-writable
(WSL2 Ubuntu, AMD SVM, nested virt, `kvm` group active), so the guest-boot path
that had always self-skipped finally ran for real — and immediately exposed a
hang that had silently broken the previous (2026-06-13) run.

**Salvage + root-cause (the headline).** The 2026-06-13 branch had uncommitted,
never-pushed work in the tree: a page-aligned `GuestRam` + `validate_region`
guard. Carried it onto today's branch. Running the suite for real showed the
`serial_console_smoke` guest-boot test *hangs* — and a hung test process from
Jun 13 was still alive, explaining why that run never committed/PR'd. Bisected
with a direct `kvm-ioctls` probe: tss-only boots `O`/`K`/`HLT` in <1 ms, but
adding `create_irq_chip` reproduces the hang exactly. Root cause: with the
in-kernel local APIC, KVM handles `HLT` itself (the vCPU parks waiting for an
interrupt) and never returns `KVM_EXIT_HLT`, so a "run a blob until it halts"
probe blocks forever in `KVM_RUN`.

### Increments (commit — what)
1. `c738a64` — **real-mode guest-boot smoke test actually boots under KVM.**
   Salvaged `GuestRam` (page-aligned; KVM requires page-aligned `userspace_addr`)
   + `validate_region`; split the backend into `new()` (production, in-kernel IRQ
   chip) and `new_without_irqchip()` (HLT exits to userspace) and pointed the
   smoke test at the latter.
2. `2f8c94e` — **prove the KVM input (`in`) path**: a guest reads the COM1 LSR
   and echoes it; the device-computed `0x60` (idle UART THRE|TEMT) round-trips
   device → `KVM_EXIT_IO`(in) → guest AL → out → sink.
3. `18746cd` — **prove the KVM MMIO path**: real-mode blob writes/reads a device
   at a low (16-bit reachable) address; both MMIO exits confirmed (KVM emulates
   real-mode MMIO instructions).
4. `0fa7299` — **`GuestMemory` DMA view**: `MemSlot` retains `host_addr`; a new
   `GuestMemory` implements enlil-devices' `DmaMemory` over the slot table
   (GPA→HVA, bounds-checked, multi-slot). `KvmBackend::guest_memory()`.
5. `1e19430` — **end-to-end DMA coherence**: a guest writes a byte into its RAM;
   the host reads it back through `guest_memory()` against the real slot table.
6. `8c5a182` — **`StandardPc::service_usb_dma`**: the run loop's USB DMA entry
   point. A guest doorbell write latches in `XhciMmio`; this drains it against
   guest memory (command/transfer rings → events) and reconciles `INTA#`.
   Integration test drives MMIO-doorbell → latch → service → Command Completion.
7. `55fc725` — **docs**: folded the KVM findings into RESEARCH.md (irqchip/HLT,
   page alignment, real-mode MMIO) and a Phase-4 ROADMAP status note.
8. `7a2905c` — **exercise device PIO on the production (`new()`, IRQ-chip)
   backend** — every other guest-boot test uses `new_without_irqchip`; this proves
   port I/O exits reach the DeviceBus even with the in-kernel APIC (stops at the
   output rather than waiting for the absorbed HLT).
9. `cd2c454` — **`StandardPc::flush_usb_events`**: the run-loop seam for events
   posted *outside* doorbell servicing (hot-plug Port Status Change). Test
   programs the interrupter via the BAR, hot-plugs a device, flushes exactly one
   PSC event (type 34) into the guest event ring.
10. `2c388b7` — **docs correction (honesty)**: my increment-7 note wrongly said
    command rings were internal. Verified the command ring *is* guest-resident
    (`GuestRingCursor` off `CRCR`) and device contexts come from `DCBAAP`;
    corrected the note to scope the real remaining gap (transfer rings).
11. `0c33578` — **`gather_transfer_td`**: the core mechanism for guest-resident
    transfer rings, landed as an isolated, fully-tested building block (chained
    TD by chain bit, Link/cycle inherited from `GuestRingCursor`, undecodable →
    `Err(addr)`) **without** touching the live `process_transfer_ring` (so the
    750 existing transfer tests are untouched).

### Research (informed the build)
RESEARCH.md `2026-06-14`: KVM API behaviours surfaced once `/dev/kvm` ran for
real — `KVM_CREATE_IRQCHIP` changing `HLT` semantics, `KVM_SET_USER_MEMORY_REGION`
page-alignment, KVM emulating real-mode MMIO. Grounded in the KVM API reference
and corroborated by the direct-`kvm-ioctls` bisection above.

### Test results (exact)
- **`/dev/kvm`: read-writable** (probe `KVM_RW_OK`, user in `kvm` group). The
  guest-boot / KVM tests **ran for real, not skipped**:
  - `device_bus::tests::serial_console_smoke` — PASS (boots, `out` "OK", HLT).
  - `serial_input_path_smoke` — PASS (LSR `0x60` round-trips through `in`).
  - `mmio_path_smoke` — PASS (MMIO write `0x55`, read `0x3C`).
  - `guest_memory_reads_what_the_guest_wrote` — PASS (guest write → host DMA read).
  - `serial_output_under_production_irqchip` — PASS (PIO on the in-kernel-IRQ-chip
    backend).
  - `xhci_doorbell_is_serviced_through_the_run_loop_dma_seam`,
    `xhci_hotplug_event_flushed_..._run_loop_seam` — PASS.
- `cargo test -p enlil-core --lib`: **152 passed, 0 failed**.
- `cargo test -p enlil-devices --lib`: **753 passed, 0 failed** (+3 gather tests).
- `cargo clippy` clean for every crate touched (`enlil-core`, `enlil-devices`,
  `--lib --tests`); workspace clippy green.
- **Toolchains:** all increments built/tested **Linux/WSL** (nightly,
  `x86_64-unknown-linux-gnu`); the host-agnostic ones (increments 6, 9, 11 —
  `service_usb_dma`, `flush_usb_events`, `gather_transfer_td`) were **also built
  Windows-native** (`x86_64-pc-windows-msvc`, exit 0) via the WSL-source +
  `D:\Development\.enlil-win-target` mechanism. Increments 1–5, 8 are entirely
  within `#[cfg(target_os = "linux")]` (KVM), so Windows does not apply.

### STOP REASON
Tractable, **regression-safe** work in this subsystem is done; the next step
(wiring `gather_transfer_td` into the live `process_transfer_ring`) is a careful
change to the most-tested code path and is **not safe to rush**. `configure_endpoint`
always inserts an empty internal ring entry with the endpoint's
`tr_dequeue_pointer`, so any auto-trigger for the guest path could fire spuriously
on existing tests whose dequeue pointer aims at zeroed memory (cycle/decode then
posts a TRB error) — it needs a per-endpoint cursor lifecycle and a trigger that
can't misfire, designed fresh rather than under time pressure. Wall-clock budget
was not fully spent, but per the guardrail ("leave it for tomorrow rather than
committing it half-done") I handed off rather than risk the transfer path.

### Recommended next step (tomorrow)
1. **Wire `gather_transfer_td` into `process_transfer_ring`** behind a
   per-endpoint `GuestRingCursor` (new `guest_transfer_rings: BTreeMap<(u8,u8),
   GuestRingCursor>`), built from the endpoint context's `tr_dequeue_pointer` +
   `dequeue_cycle_state`. Trigger it only for endpoints the guest actually drives
   in guest-memory mode (NOT auto on every empty internal ring — see STOP
   REASON), and invalidate the cursor at Configure Endpoint / Set TR Dequeue
   Pointer / Reset Endpoint / Disable Slot. Feed the gathered TD straight into
   the existing `execute_control_td` / `execute_normal_td`. Audit each existing
   transfer test's `tr_dequeue_pointer`/DCS before flipping the trigger.
2. **Production run-loop watchdog**: an `immediate_exit`/signal-based kick so the
   `new()` (IRQ-chip) backend can bound a vCPU that idles in `HLT` or fails to
   progress — flagged in the RESEARCH note.

## 2026-06-13 — Session: Phase 4.4 — xHCI Output Device Context write-back + the full slot/endpoint command set

**10 increments, each independently green and committed** (branch
`claude/wonderful-mccarthy-cx0zer`). This session closed the last open part of the
2026-06-12 hand-off's item #4 (*"Guest-memory-resident rings: CRCR/DCBAAP/ERSTBA
dereferencing through `DmaMemory`"*) — PR #24 had landed the command ring (CRCR) and event
ring (ERST/ERDP); this run added **DCBAAP / Output Device Context** write-back and then
completed the xHCI command set that depends on it. The `enlil-devices` lib test suite grew
**734 → 750** (+16 tests); `cargo test --workspace` aggregates **1010 passed, 0 failed,
1 ignored** (the ignored one is the `/dev/kvm` self-skip). `cargo build --workspace`,
`cargo fmt --all -- --check`, `cargo clippy --all-targets --workspace -- -D warnings` green
at every commit. `/dev/kvm` **still absent** (verified — no nested virt).

### Situation found
The controller read **input** device contexts (Address Device / Configure Endpoint) but
never wrote the **output** device context back through the DCBAA, so a real guest driver
would never see its device addressed/configured (it reads the output Slot/EP contexts to
confirm each command). Several enumeration/recovery commands were also undecoded
(`EvaluateContext`, `SetTrDequeuePointer`, `ResetDevice`) → `CommandTrb::from_trb` returned
`None` → `TrbError`, which would stall a real driver. Also found: the current toolchain's
clippy (`missing_const_for_fn`, nursery) newly flags two pre-existing `from_bytes` parsers
as const-eligible, so HEAD was not `clippy -D warnings`-clean on this runner.

### Increments (each its own green commit)
1. **`b3de331` (phase-4)** const-eligible `from_bytes` parsers — fixes clippy toolchain
   drift (`block.rs`, `net/header.rs`) so the workspace clippy gate is green again. *(Not a
   feature; a prerequisite so every later commit passes `clippy -D warnings`.)*
2. **`636b5c4` (4.4)** Output device context after **Address Device** — new `SlotContext`
   codec + `SlotState`, `device_context_pointer` (DCBAA deref) + `device_context_entry_offset`,
   and `publish_addressed_context`: copies the input slot/EP0 contexts with Slot State →
   Addressed and an assigned USB address; EP0 → Running. Threaded `&mut dyn DmaMemory`
   through `process_command_ring`/`execute_command`/`address_device`.
3. **`4a326e4` (4.4)** Output context after **Configure Endpoint** — Slot State →
   Configured, Context Entries = highest configured DCI, added EPs → Running, dropped EPs
   zeroed; the Deconfigure (DC=1) path returns the slot to Addressed.
4. **`0b28893` (4.4)** Output context after **Disable Slot** — slot → Disabled, address 0,
   entries 0, held endpoints zeroed.
5. **`3a203db` (4.4)** **Evaluate Context** decode + handler — re-evaluates EP0 Max Packet
   Size without changing state (the mid-enumeration MPS update).
6. **`a5ee258` (4.4)** **Set TR Dequeue Pointer** decode + handler — repoints a ring after
   a stop/halt (STALL recovery's second half); added `TrbRing::set_cycle_state` and the
   `ContextStateError` (19) completion code.
7. **`b8bd492` (4.4)** **Reset Device** decode + handler — slot → Default, address 0,
   non-control endpoints dropped, EP0 retained, ready for re-enumeration.
8. **`2514a01` (4.4)** **Stop Endpoint** → Stopped — added `EpState` enum + `publish_ep_state`
   (patches just the EP State field in the output context); Stop Endpoint pauses the ring
   and publishes Stopped.
9. **`279468a` (4.4)** STALL → Halted, Reset Endpoint → Running in the output context
   (threaded mem through the control-status/`stall_endpoint` path). Completes the
   Running/Stopped/Halted EP-state story.
10. **`6e00047` (4.4)** Address Device **BSR** (bit 9): slot-context-only setup → Default
    state at address 0 (the first pass Linux issues before the real addressing).

All commands are reachable both via the internal `submit_command` modelling path and the
guest-memory **CRCR** command ring (`process_command_ring` decodes via `CommandTrb::from_trb`
either way), so the whole loop — command ring fetched from guest memory, contexts read and
written through the DCBAA, events delivered to the ERST event ring — is now guest-memory
resident.

### Research (informed the build) — logged in RESEARCH.md → 2026-06-13
xHCI 1.2 §4.6.5 (Address Device + BSR), §4.6.6 (Configure Endpoint), §4.6.7 (Evaluate
Context), §4.6.9 (Stop Endpoint), §4.6.10 (Set TR Dequeue Pointer), §4.6.11 (Reset Device),
§6.1 (DCBAA; entry 0 = scratchpad), §6.2.1–6.2.3 (context layout — output contexts have no
Input Control Context prefix), Tables 6-4/6-8 (Slot/EP state encodings). No new external
literature changed the plan; the work is grounded in the primary spec.

### Test results (exact)
- `cargo build --workspace` OK · `cargo fmt --all -- --check` OK ·
  `cargo clippy --all-targets --workspace -- -D warnings` OK ·
  `cargo test --workspace` → all binaries pass, **0 failed, 1 ignored** (the `/dev/kvm`
  self-skip). New tests this session: **16** (controller + trb + context: output-context
  publishing for Address/Configure/Disable, Evaluate/SetTRDequeue/ResetDevice/StopEndpoint
  handlers + TRB round-trips, STALL/Reset EP-state, BSR, SlotContext/DCBAA codecs).
- `/dev/kvm`: **not run — absent (no nested virt)**, verified. no_std custom target: N/A
  (no crate is `#![no_std]`).

### Recommended next steps (tomorrow)
1. **Transfer rings resident in guest memory.** The command ring (CRCR) and device contexts
   (DCBAA) now live in guest memory, but **transfer rings still use the internal index-based
   `TrbRing`** fed by `submit_transfer` (with `base_addr` only for event-pointer reporting).
   Give each endpoint a `GuestRingCursor` (like the command ring) seeded from its EP context
   TR Dequeue Pointer + DCS, so `process_transfer_ring` fetches TDs from guest memory by
   cycle bit. This is the last piece making the *entire* xHCI path guest-memory driven and
   is what the KVM run loop will exercise. (Touches `submit_transfer`/`process_transfer_ring`/
   `gather_td` and many tests — scope it carefully; Set TR Dequeue Pointer then sets the
   cursor directly instead of resetting the internal ring.)
2. **TUI USB tab** (Phase 4.5 milestone UI): ratatui table over `UsbDeviceList`/
   `UsbHotplugNotice` + `UsbCommand{Reassign,Detach}`; the protocol seam landed 2026-06-12.
   The old `tui/mod.rs` was a corrupt placeholder (removed) — build fresh; also the core-side
   daemon answering `RequestUsbDevices` from `XhciRegistry::placements()` + `UsbMonitor`
   does not exist yet.
3. **Machine-identity profile coherence** (open since 2026-06-11): wire `from_host`
   CPUID/SMBIOS into the boot/`fw_cfg` delivery path so the run presents one coherent
   captured machine (AMD host → prefer `from_host` over the default Intel-Q35 profile).
4. **KVM run loop** still blocked on `/dev/kvm` (ask for a nested-virt runner). When it
   lands, the now-complete command/context/event guest-memory loop is ready to drive a real
   guest's xHCI enumeration.

---

## 2026-06-12 — Session: Phase 4 USB routing — TD processing, device models, routing→controller binding, hot-plug bridge

**6 increments, each independently green and committed.** Increments 1–3 went out as
**PR #22 (merged)**; increments 4–6 are on `claude/awesome-faraday-u1tb3x` → **PR #23**.
Workspace tests **923 → 975** (`cargo test --workspace`: 975 passed, 0 failed, 1 ignored =
the `/dev/kvm` self-skip). `cargo build --workspace`, `cargo fmt --all -- --check`,
`cargo clippy --all-targets --workspace -- -D warnings` green at every commit. `/dev/kvm`
**still absent** (verified — no nested virt).

### Increments (all in the morning hand-off's top-2 lane: Phase 4 xHCI/routing)
1. **`941f909` (phase-4.4) Transfer-ring (TD) processing.** `usb::xhci::transfer` (Setup/
   Data/Status/Normal TRB codecs, SETUP as IDT immediate data, DCI helpers, the
   **`DmaMemory` guest-memory seam** + Vec-backed test double); `usb::emulated` with the
   **`UsbDeviceModel` trait** (the future libusb boundary, ACRN's three transfer shapes) and
   a `LoopbackDevice`; controller grows per-(slot, DCI) transfer rings, an EP0
   Setup→Data→Status stage machine, scatter/gather, Transfer Events with the spec's 24-bit
   residual (`requested − transferred`, Short Packet on short TDs), STALL→halt with **Reset
   Endpoint actually recovering the ring**, Disable Slot dropping the slot machinery.
   Device-slot doorbells latch at register-write time; `service_doorbells(mem)` drains them.
2. **`fd0284c` (phase-4.5) Routing→controller binding.** `usb::registry::XhciRegistry`:
   guest→`SharedXhci` map; `attach` consults `RoutingState` and lands the device on the
   decided guest's lowest free protocol-matching port (rollback on unknown guest / full
   controller); `detach`; live `reassign` = virtual unplug/replug with real hot-plug events
   on both guests, failed moves replug on the original guest. Tests run the Phase 4.5
   milestone in miniature (two mice + two keyboards → two guests, live reassignment).
3. **`9c3bab0` (phase-4.3) Hot-plug bridge.** `usb::hotplug::HotplugDispatcher`: monitor
   callbacks (any thread) → mpsc channel → `service(&mut registry)` on the run-loop thread
   (the single thread owning the `Rc<RefCell>` controller handles); port-path→bus-address
   map so disconnects (which only name the port) find their placement; outcomes enum for
   the console's notification feed.
4. **`3b2aa35` (phase-4.5) Management-protocol USB controls + protocol-module revival.**
   `UsbDeviceEntry`, `ServerMessage::{UsbDeviceList, UsbHotplugNotice}`,
   `ClientMessage::{RequestUsbDevices, UsbCommand}` with `UsbAction::{Reassign, Detach}`.
   **Found:** `enlil-mgmt/src/protocol.rs` was never declared as a module — never compiled
   (missing `thiserror` dep, lossy cast, missing docs all latent); it is now the root of a
   real lib target. **Also found and removed:** `enlil-mgmt/src/tui/mod.rs` contained only
   an 80-byte tool-placeholder string (`[content omitted from context — 12779 bytes …]`) —
   same corruption pattern as the 2026-06-02 `kvm_backend.rs` incident; the real TUI was
   never committed.
5. **`05286a5` (phase-4.5) Emulated HID boot keyboard.** `EmulatedKeyboard`: canonical
   63-byte E.6 report descriptor, coherent device/config/HID/endpoint descriptor block
   (the shape stock Windows/Linux HID drivers class-match), `SET_IDLE`/`SET_PROTOCOL`,
   LED `SET_REPORT`, 6-key-rollover boot reports on the interrupt IN endpoint; end-to-end
   test delivers a key report into guest memory through the TD path.
6. **`fbbb6fa` (phase-4.4) Address Device parses the input context.** Doorbell 0 now
   latches like the rest (commands process in `service_doorbells`, with guest memory);
   `address_device` validates A0|A1 add flags (else the new `ParameterError`, xHCI code
   17) and reads the slot context's root-hub port number, **binding the model parked at
   that port to the slot** — the real enumeration flow. `attach_device_with_model` parks
   models at routing time; Disable Slot re-parks (guest can re-enumerate); registry
   threads models through attach/reassign.

### Research (informed the build) — logged in RESEARCH.md → 2026-06-12
- ACRN `xhci.c` TD assembly (chain bit, `USB_DATA_PART`/`USB_DATA_FULL`; control stages
  are separate TDs → per-EP0 state machine).
- Linux fix "xhci: Fix TRB transfer length macro used for Event TRB": event TRBs carry a
  **24-bit residual**, not the 17-bit requested length — drivers compute
  `transferred = requested − residual`; wrong residuals silently corrupt length accounting.
- xHCI §6.4.1.2.1 (SETUP is IDT immediate data; TRT field), §4.5.1 (DCI = ep×2+dir),
  §6.2.5.1 (input control context A0|A1 for Address Device), §6.2.2 (slot context dword 1
  bits 23:16 = root-hub port number).

### Test results (exact)
- `cargo build --workspace` OK · `cargo fmt --all -- --check` OK ·
  `cargo clippy --all-targets --workspace -- -D warnings` OK ·
  `cargo test --workspace` → **975 passed, 0 failed, 1 ignored**.
- `/dev/kvm`: **not run — absent (no nested virt)**, verified. no_std custom target: N/A
  (no crate is `#![no_std]`).

### Recommended next steps (tomorrow)
1. **libusb-backed `UsbDeviceModel`** (Phase 4 host side): a `rusb`-based forwarder
   implementing the three transfer shapes against a real device, feature-gated +
   self-skipping when no device/permission (like the KVM test). The seam is ready and
   tested; this is the last piece between the routing stack and physical hardware.
2. **Configure Endpoint context handling**: parse endpoint contexts (add flags A2+) out of
   the input context the same way Address Device now does — gives transfer rings their
   real EP types/max-packet instead of get-or-create.
3. **TUI USB tab** (Phase 4.5 milestone UI): ratatui table over
   `UsbDeviceList`/`UsbHotplugNotice` + `UsbCommand{Reassign,Detach}`; the protocol seam
   landed this session. Note the old `tui/mod.rs` was a corrupt placeholder (removed) —
   build fresh; also the core-side daemon that answers `RequestUsbDevices` from
   `XhciRegistry::placements()` + `UsbMonitor::devices()` does not exist yet.
4. **Guest-memory-resident rings** (CRCR/DCBAAP/ERSTBA dereferencing through `DmaMemory`)
   — the seam is in place; the doorbell/service split already matches the KVM exit shape.
5. Machine-identity profile coherence (hand-off item from 2026-06-11 #1) remains open:
   wire `from_host` CPUID/SMBIOS into the boot/fw_cfg delivery path.

---

## 2026-06-11 — Session: Q35/ICH9 chipset identity arc + host-machine identity capture + virtual xHCI assembly

**17 increments, each independently green and committed** (PR #21, branch
`claude/awesome-faraday-aomlqt`). Picked up the morning hand-off's #1 (q35 chipset
identity) and ran it to completion, then continued into the host-identity-capture
consolidation (#2 CPUID paths, SMBIOS) and the long-dormant Phase 4 xHCI assembly.
Workspace tests **897 → 922** (`cargo test --workspace`: 922 passed, 0 failed, 1 ignored =
the `/dev/kvm` self-skip). `cargo build --workspace`, `cargo fmt --all -- --check`,
`cargo clippy --all-targets --workspace -- -D warnings` green at every commit. `/dev/kvm`
**still absent** (verified). `iasl` 20230628 + `dmidecode` 3.5 installed and used.

Increments after the initial hand-off draft (this entry was extended in place):
- **`d68fb0d`** Pinned the MCFG ECAM base against the MCH PCIEXBAR (the ACPI half of
  the q35 ECAM cross-surface pair; device-bus half was in the identity commit).
- **`1b04abe`** xHCI now advertises Supported Protocol extended capabilities (`HCCPARAMS1`
  xECP was 0 — a controller no vendor ships): a USB 2.0 + USB 3.0 cap pair with the
  root-hub ports split by protocol; `attach_device` is protocol-aware (SS→USB3 ports,
  LS/FS/HS→USB2 ports).
- **`be6cb30`** PCI identity registers (Vendor/Device/Subsystem/Class IDs, Header Type)
  are now read-only to guest config writes — `PciConfigSpace::guest_write(offset, width,
  value)` is the single masked write path (also fixed a pre-existing byte/word ECAM write
  bypass). Protects all the identity the session programmed.
- **`9dee7f1`** PCI functions present a consistent capability list: `new()` no longer
  asserts the STATUS caps bit with a null pointer; `add_power_management_capability` installs
  a PM cap (ID 0x01) on the session's chipset functions so a guest walking the list finds a
  real terminating one. (Total now **19 increments, 923 tests**.)

### Arc 1 — Chipset identity is now coherently Q35/ICH9 (was a mixed-generation impossibility)
The platform mixed an i440FX host bridge (`8086:1237`, no ECAM) with PCIe ECAM/MCFG and a
PIIX3 ISA bridge at `00:01.0` — incoherent to anyone cross-checking IDs vs capabilities.
- **`22b7102`** Host bridge → Q35 MCH (`8086:29C0`, rev 02) with **PCIEXBAR** (config 0x60)
  seeded from the live `ecam_base`; ISA/LPC bridge → ICH9 LPC at `00:1F.0` (`8086:2918`).
  ICH9 PIRQ regs share PIIX3 offsets/semantics, so PirqRouter/ELCR/links/`_PRT` carry over
  unchanged. DSDT `_ADR` derives from the shared BDF; iasl re-validated. Cross-check tests
  at the pcie and device-bus layers (PCIEXBAR base == decoded ECAM window).
- **`a05bd09`** ICH9 SMBus host controller at `00:1F.3` (`8086:2930`) — full i801 register
  model, empty-bus semantics (probes complete DEV_ERR, not open-bus 0xFF); LPC gains the
  multifunction header bit. New crate module `enlil-devices::smbus`.
- **`4cb9a92`** SMBus completion interrupt delivers through the live PIRQ routing: factored
  `assert_pci_intx` into a shared `route_pci_intx` helper; SMBus INTB# level sink routes
  through it (INTREN-set drivers now get the interrupt, not a timeout).
- **`ca2204e`** Board subsystem IDs (`1043:8694`, ASUSTeK) on every onboard function;
  pinned against the SMBIOS default baseboard manufacturer.
- **`2fcef76`** LPC PMBASE/ACPI_CNTL encode the live ACPI PM block + SCI routing;
  `chipset::SCI_IRQ` is the single source the FADT's SCI_INT derives from.
- **`d68fb0d`** MCFG ECAM base pinned against the MCH PCIEXBAR (the ACPI half of the pair).

### Arc 2 — Capture the host machine's real identity (consolidation: one canonical path)
- **`f8eb7dd`** `CpuidStealthConfig::from_host` replaces the unused `enlil-core::cpuid::
  CpuidFilter` (deleted) — captures host CPUID (vendor/FMS/features/brand/Intel leaf-4 cache
  geometry with guest-topology sharing rewrite/leaf-0x16 freqs) and synthesizes every leaf
  with the consistency rules. Tested on the runner (itself a VM → negative reference for the
  hide-hypervisor path).
- **`53525c2`** `SmbiosConfig::from_host` reads `/sys/class/dmi/id` (board/BIOS/system
  strings, product UUID) + host CPU brand; guest topology for core/thread counts. Tested
  with a fake DMI dir (tempdir) + fallback.
- **`61b1ce8`** Leaf 7 captures host EBX/ECX, masked to a virtualizable, leaf-0xD-consistent
  allowlist (no AVX-512/TSX/SGX/PKU/LA57 — each pairs with absent state or an MSR surface).

### Arc 3 — AMD CPUID topology surface (Phase 5.4 sweep, AMD side)
- **`82c0959`** Vendor-correct max leaves (AMD 0x10/0x8000001F), TOPOEXT leaves
  0x8000001D/1E (cache geometry == legacy 0x80000005/6; SMT in 1E), CPB/EffFreq in
  0x80000007, NC/ApicIdSize in 0x80000008 — every leaf cross-checked against a partner.

### Arc 4 — Virtual xHCI controller assembled and mounted (Phase 4.4, was a placeholder)
- **`2c75780`** `VirtualXhciController` assembled from its modelled-but-unwired parts:
  one MMIO window per the capability block, real slot-pool command processing
  (Enable/Disable/NoSlotsAvailable), port connect → Port Status Change events.
  `CommandTrb::from_trb` added (missing decode half).
- **`ed2f781`** Mounted as a discrete Renesas uPD720202 (`1912:0015`) at `00:04.0` with a
  64 KiB BAR0 MMIO window (`usb::XhciMmio`) and INTA# through the live PIRQ routing;
  `StandardPc` carries the handle + `connect_usb_device`.
- **`ec57241`** Root-hub port allocator: `DeviceSpeed::xhci_speed_id` + `attach_device`
  (lowest free port by speed); `StandardPc::attach_usb_device` is the routing-engine seam.

### Test results (exact)
- `cargo build --workspace` OK · `cargo fmt --all -- --check` OK ·
  `cargo clippy --all-targets --workspace -- -D warnings` OK ·
  `cargo test --workspace` → **919 passed, 0 failed, 1 ignored**.
- iasl + dmidecode validation tests ran and pass. `/dev/kvm`: not run — absent (verified).
  no_std custom target: N/A (no crate is `#![no_std]`).

### Recommended next steps (tomorrow)
1. **Machine-identity profile coherence (flagged, not done):** the *default* SMBIOS profile
   is an AMD Ryzen/B650E board while the chipset is always Intel Q35 — when the host is AMD
   the run path should prefer `from_host` SMBIOS+CPUID and accept the Intel-chipset
   divergence (a real AMD chipset model is a large follow-up). Wire `from_host` into the
   actual boot/`fw_cfg` delivery path so the run path presents one coherent captured machine.
2. **xHCI transfer-ring (TD) processing** — the next Phase 4 increment: parse Normal/Setup/
   Data/Status TRBs off a transfer ring and forward to a real device via libusb (ACRN's
   model: `devicemodel/hw/pci/xhci.c`). Needs the device-context (DCBAA/input-context)
   handling that lives in guest memory → partly blocked on the KVM run loop, but the
   ring-parsing + an in-process loopback device is doable now and testable.
3. **Routing→controller binding:** a multi-guest controller registry so a `RoutingState`
   assignment calls `attach_usb_device` on the target guest's controller (the Phase 4.5
   milestone: two devices, two guests, live reassignment). Single-controller seam is ready.
4. **q35 fidelity remainder:** SATA at `1F.2` (needs an AHCI model — large). The subsystem
   IDs are now read-only to guests (`be6cb30`) and the functions carry a PM capability
   (`9dee7f1`). **Note on PCIe capabilities (corrected mid-session):** the chipset
   southbridge/host-bridge functions (MCH `00:00.0`, LPC `1F.0`, SMBus `1F.3`) are
   legitimately *conventional* PCI on a real Q35 — they correctly have NO PCI Express
   Capability, so the PM-cap-only treatment is right; do **not** add a PCIe cap to them. The
   real gap is **topology**: a discrete xHCI is a PCIe *endpoint behind a root port*, but our
   flat bus-0 model puts it directly on bus 0 with no root port. The proper fix is to model a
   PCIe root port (a Type 1 bridge with a PCI Express Capability, port-type `0x4`) and put
   the xHCI on its secondary bus with its own PCI Express Capability (endpoint, port-type
   `0x0`) — a larger topology pass, not a per-function cap add.
5. **KVM run loop** still blocked on `/dev/kvm` (ask for a nested-virt runner). When it
   lands: build the bus via `standard_pc_complete`, `KVM_SET_CPUID2` from a `from_host`
   `CpuidStealthTable`, deliver `from_host` SMBIOS via fw_cfg, drive the xHCI/SMBus/PM
   interrupts through the now-wired PIRQ routing.

---

## 2026-06-11 — Session: Q35/ICH9 chipset identity + leaf-0xB topology shift bug + CPUID surface guards

**4 commits, each independently green** (branch `claude/awesome-faraday-gpjjda`).
Workspace tests **897 → 902** (`cargo test --workspace`: 902 passed, 0 failed, 1 ignored =
the `/dev/kvm` self-skip). `cargo build --workspace`, `cargo fmt --all -- --check`, and
`cargo clippy --all-targets --workspace -- -D warnings` all green at every commit.
`/dev/kvm` **still absent** (verified — no nested virt). `acpica-tools` (`iasl` 20230628)
installed and used as a hard validator; `dmidecode` not needed this run.

### Increment 1 — Q35/ICH9 chipset identity (`c5a05aa`, the hand-off's top item)
The platform exposes an MCFG/ECAM window and a `PNP0A08` PCIe root in the DSDT — a
PCI-Express-era machine — but the host bridge still reported the legacy **i440FX** ID
(`8086:1237`) and the south bridge was a **PIIX3** at `00:01.0`, a chipset combo with **no**
PCIe/ECAM. That contradiction is a one-read VM tell (a guest reads MCFG, then the `00:00.0`
device ID, and sees a chipset that can't have ECAM).
- Host bridge now reports the **Q35 MCH** (`8086:29C0`); LPC interrupt-router bridge is the
  **ICH9 LPC** (`8086:2918`) at its canonical `00:1F.0` (was PIIX3 at `00:01.0`).
- `PIRQ[A-D]_ROUT` stay at config `0x60`-`0x63` (byte-identical layout on PIIX3 and ICH9,
  per the ICH9 datasheet §13), so the `PirqRouter` model and the DSDT live `_SRS`/`_CRS`
  link devices are **unchanged**; only the bridge identity + BDF moved. `ISA_` `_ADR` →
  `0x001F0000`.
- New constants `pcie::{Q35_MCH_DEVICE_ID, ICH9_LPC_DEVICE_ID, LPC_BRIDGE_BDF}`; renamed the
  old `PIIX3_ISA_BRIDGE_BDF`/`PIIX3_ISA_DEVICE_ID`. Whole DSDT **round-trips through `iasl`
  at 0 errors/warnings** (the `acpi_iasl_validation` integration test was actually run).

### Increment 2 — leaf-0xB topology shift overshoot (`eed6aa6`, a real bug)
The x2APIC topology shift in CPUID leaf 0xB (subleaf 0 SMT, subleaf 1 Core) was computed as
`32 - n.leading_zeros()` = `floor(log2(n)) + 1` — one bit too many for **exact powers of
two** (the common case). 8 logical processors → package shift 4 instead of 3; 4 cores → 3
instead of 2. A guest right-shifts its x2APIC ID by that field to derive the package ID, so
the extra bit corrupts the package boundary — a wrong, detectable topology. Fixed all three
sites (`enlil-core` `CpuidFilter::filter` + `generate_topology_entries`; `enlil-devices`
`stealth::cpuid::build_topology_leaves`) with a shared `ceil_log2(n)` (counts leading zeros
of `n-1`). Tests pin the corrected EAX shifts + a `ceil_log2` truth table.

### Increment 3 — cross-surface CPUID consistency guard (`b5e1b5e`)
Leaf 1 EBX[23:16] (max addressable logical-processor IDs, a power of two via
`next_power_of_two`) and leaf 0xB subleaf 1 EAX (package shift) both encode the package's
APIC-ID width; the fix above made them agree. Added a test pinning `max_ids == 2^shift`
across a vcpu/thread matrix (power-of-two and not) so neither surface can silently desync.

### Increment 4 — leaf-7 feature-comment correction + surface lock (`8ad35b7`)
`build_leaf_7`'s EBX `0x281` is bits 0/7/9 = **FSGSBASE, SMEP, ERMS**, but the comment
misnamed them "FSGSBASE, BMI1, AVX2" (the value never set BMI1/AVX2). Corrected the comment,
documented the leaf, and added a test pinning EBX to exactly those bits, asserting AVX2 stays
clear, and cross-checking leaf 0xD advertises AVX — a coherent AVX-but-not-AVX2 (Sandy/Ivy
Bridge) level rather than a mismatched one.

### Research (informed the build) — logged in RESEARCH.md → 2026-06-11
- **Intel ICH9 datasheet (316972-004) §13 LPC (D31:F0):** `PIRQ[A-D]_ROUT` at `0x60`-`0x63`,
  bit 7 = IRQEN, bits[3:0] = ISA IRQ — byte-identical to PIIX3 `PIRQRC[A-D]`, so the
  south-bridge identity swap needed no `PirqRouter` change.
- **QEMU `pc` (i440FX) vs `q35` taxonomy:** i440FX host bridge `8086:1237` predates PCIe / has
  no MCFG; Q35 MCH `8086:29C0` is the PCIe generation that ships ECAM + `PNP0A08`. → made the
  host-bridge ID consistent with the MCFG/PNP0A08 we already emit.
- **Passthrough-hardening practice (2024-25):** anti-detection setups standardise on `q35`
  precisely because i440FX is a known emulator fingerprint — corroborates this was a real,
  cheaply-probed tell, not a theoretical one.

### Test results (exact)
- `cargo build --workspace` OK · `cargo fmt --all -- --check` OK ·
  `cargo clippy --all-targets --workspace -- -D warnings` OK (0 warnings) ·
  `cargo test --workspace` → **902 passed, 0 failed, 1 ignored**.
- `cargo test -p enlil-devices --test acpi_iasl_validation` → **3 passed** (DSDT/SSDT/all
  tables round-trip through `iasl` clean *with the new `00:1F.0` `_ADR`*).
- `/dev/kvm`: **not run — absent (no nested virt)**, verified. no_std custom-target: N/A
  (no crate is `#![no_std]`).

### Recommended next steps (tomorrow)
1. **Complete the ICH9 south-bridge identity (multifunction).** Real ICH9 `1F.0` is
   multifunction with siblings `1F.2` SATA AHCI (`8086:2922`) and `1F.3` SMBus (`8086:2930`).
   We model only `1F.0`; absent siblings read all-ones (benign), but a complete south bridge
   would add them with the header-type multifunction bit set. **Caveat:** an AHCI/SMBus
   *config-only stub* with no working MMIO/IO can be a *worse* tell than absence (a guest
   driver attaches and fails) — only add a function if its register behaviour is modelled, or
   add SMBus (simpler) first with a minimal SMB host-controller I/O model.
2. **Reconcile the SMBIOS board identity with the chipset.** SMBIOS reports `ROG STRIX
   B650E-E` (an AMD AM5 board) while the chipset is now Intel Q35 and CPUID can be Intel —
   a cross-surface inconsistency. Decide the canonical machine identity (Intel Q35-class board
   + matching DMI strings, or keep AMD and revisit the chipset) — this is an owner-facing
   identity-policy choice, flag in the roadmap before changing.
3. **AMD CPUID profile gaps (needs a real AMD reference dump to validate):** `0x8000001D`
   (cache topology) / `0x8000001E` (extended APIC ID / CCX topology) TOPOEXT path, leaf-1
   max-addressable-IDs vs `0x8000001E` consistency, boost via `0x80000007` EDX[9]. Don't
   guess these without a hardware oracle — the Intel side is now internally consistent, the
   AMD side is conservative-but-thin.
4. **CPUID path consolidation** (flagged in prior hand-offs, still not done): `enlil-core::
   cpuid::CpuidFilter` (filters host entries; **no production users** — only tests) vs the
   canonical `enlil_devices::stealth::cpuid::CpuidStealthTable` (synthesises). Both now share
   the `ceil_log2` topology fix but remain parallel. The right merge is still "build a
   `CpuidStealthConfig` from host data, then synthesise." A deliberate refactor pass.
5. **KVM run loop** still blocked on `/dev/kvm` (ask for a nested-virt runner). When it lands:
   build the bus via `standard_pc_complete`, `KVM_SET_CPUID2` from `CpuidStealthTable`, and
   drive `PmcState::advance_counters` + `VcpuTimingState::advance` with the same
   `PmcRateModel` and ref-cycle delta per VMENTRY (consistency contract documented on both
   types and roadmap §5.4).

---

## 2026-06-10 (b) — Session: vPMU/CPUID cross-surface consistency arc + TPM/LBR consolidation + CI validator gates

**14 code/CI increments + docs/hand-off commits, each independently green** (PR #19, branch `claude/awesome-faraday-yud61k`).
Second session today; picked up the morning hand-off's items 1, 2, 4 and 5 and extended them
into a coherent arc: *every guest-visible surface that encodes the same fact must agree*.
Workspace tests **881 → 897** (`cargo test --workspace`: 897 passed, 0 failed, 1 ignored =
the `/dev/kvm` self-skip). `cargo build --workspace`, `cargo fmt --all -- --check`,
`cargo clippy --all-targets --workspace -- -D warnings` all green at every commit.
`/dev/kvm` **still absent** (verified). `iasl` + `dmidecode` installed and used locally.

### Arc 1 — CI + consolidation (morning hand-off items)
- **`8d0bb5a`** CI installs `acpica-tools` + `dmidecode` → the iasl/dmidecode integration
  tests are now hard gates (verified green on GitHub's runner); clippy/test aligned to
  `--workspace`.
- **`78bba2c`** removed duplicate `LbrSanitizer` (canonical: `enlil_devices::stealth::lbr`).
- **`5707341`** **one TPM now**: device CRB TPM gained real `NV_Write`/`NV_Read` (writes were
  silently discarded before — BitLocker sealing read back nothing; undefined index →
  `TPM_RC_HANDLE`) and a byte-vec `execute_command` front (replaces the old `TpmDispatcher`)
  sharing PCR/NV state with the MMIO front; duplicate `enlil-core::vtpm` deleted (its
  zero-filled EK/AIK/SRK stubs were *not* ported — still blocked on real RSA/ECC keygen).
- **`660c0eb`** `TPM2_GetCapability` returns real `TPM_CAP_PCRS` / `TPM_CAP_TPM_PROPERTIES` /
  `TPM_CAP_ALGS` data (TCG Part 3 §30.2 framing; property windowing pragmatically ignored).
- **`b7c4459`** removed the dead `exit_handler` placeholder (superseded by
  `device_bus::DeviceBus: VmExitHandler`, which already exists and is tested).

### Arc 2 — PMC/APERF cross-surface consistency (hand-off item 4)
- **`36b81d9`** `PmcRateModel`: fixed counters advance at *distinct* plausible rates
  (ref = TSC rate; core = 1.15×ref; instr = 1.31 IPC × core; TOPDOWN.SLOTS = 4×core; GP at
  core rate). Kills the IPC≡1.0 / core≡ref tell.
- **`e8659fe`** `VcpuTimingState::advance(ref_cycles, &PmcRateModel)`: the APERF/MPERF
  shadows advance at the same model's rates; test pins MSR surface == RDPMC surface exactly,
  and exit-hiding preserves the model ratio.

### Arc 3 — CPUID: populate every leaf a real OS parses, consistently (research-driven)
All layouts verified against Intel SDM / kernel parsers (see RESEARCH.md 2026-06-10 (c),(d));
runner's own cloud-VM CPUID used as the negative reference (it *shows* the leaf-0xA tell):
- **`ca0cc1a`** leaf 0xA (Intel): PMU version 5 matching `stealth::pmc` counts exactly
  (was all-zeros = "no PMU" — only vPMU-less VMs report that).
- **`6477fcc`** leaves 0x15/0x16 (Intel): TSC enumerated integer-exactly (24 MHz crystal,
  EAX=24, EBX=base MHz) — new config fields `base/max_frequency_mhz` (2800/3300); turbo
  headroom covers the PMC core/ref ratio (test-pinned).
- **`1fc6c02`** leaf 0x6: APERF/MPERF advertised (ECX[0], kernel `scattered.c` oracle),
  Intel IDA turbo backs max>base, ARAT on both vendors.
- **`b4ff02e`** leaves 0x2/0x4 (Intel): AL=01H + 0xFF descriptor; full leaf-4 hierarchy
  (32K L1d/L1i, 256K L2, 16M L3, sharing IDs track topology); dormant
  `CpuidStealthConfig::cache_info` now overrides as the pass-through path.
- **`5dad1d8`** 0x80000007 EDX[8] invariant TSC (both vendors); leaf 0xD subleaves 1/2 so
  the advertised AVX state is locatable (offset 576 + size 256 == subleaf-0 total, pinned).
- **`4b9a35a`** leaf-1 MONITOR bit cleared (leaf 0x5 is empty; KVM-default behavior).
- **`e200c29`** 0x80000005/6: Intel L2 == leaf-4 L2 (cross-check pinned); AMD legacy
  L1/TLB/L2/L3 populated (kernel `cacheinfo.c` unions + associativity-encoding table).

### Test results (exact)
- `cargo build --workspace` OK · `cargo fmt --all -- --check` OK ·
  `cargo clippy --all-targets --workspace -- -D warnings` OK ·
  `cargo test --workspace` → **897 passed, 0 failed, 1 ignored**.
- GitHub CI ("Check & Lint" incl. the new validator installs): **success** on the first
  batch; final batch pushed at end of session (check PR #19).
- `/dev/kvm`: **not run — absent (no nested virt)**, verified. no_std target: N/A.

### Recommended next steps (tomorrow)
1. **q35 chipset identity pass** — still the top unblocked architectural item (host bridge
   says i440FX 0x1237 while the platform exposes PCIe ECAM/MCFG + PIIX3 at 00:01.0). Do it
   as the *first* increment of a fresh session: touches `device_bus`, bridge BDF, host-bridge
   ID, several tests, reverts the 00:01.0 `_ADR` fix; iasl is available to re-validate.
2. **Consolidate the two CPUID paths** (flagged, not done): `enlil-core::cpuid::CpuidFilter`
   (filters host-provided entries, KVM_GET_SUPPORTED_CPUID-style, no users) vs the canonical
   `enlil_devices::stealth::cpuid::CpuidStealthTable` (synthesizes). The right merge is
   probably "build a `CpuidStealthConfig` *from host data*, then synthesize" — `cache_info`
   is already the bridge for leaf 4. Deliberate pass, same as the TPM was.
3. **CPUID gaps that remain**: AMD profile (max basic leaf should arguably be 0xD/0x10, no
   0xB enumeration check, 0x8000001D/0x8000001E TOPOEXT path, boost via 0x80000007 EDX[9]);
   Intel leaf 0x7 is conservative (no LA57 deliberately, but also no BMI2/ADX/SHA — fine
   until the vCPU actually executes those); leaf 0x16's bus 100 MHz vs leaf 0x15's 24 MHz
   crystal are both plausible but unverified against one physical reference dump — grab one
   from a real (non-VM) machine when available.
4. **KVM run loop** still blocked on `/dev/kvm` (ask for a nested-virt runner). When it
   lands: build bus via `standard_pc_complete`, `KVM_SET_CPUID2` from `CpuidStealthTable`,
   drive `PmcState::advance_counters` + `VcpuTimingState::advance` with the SAME
   `PmcRateModel` and ref-cycle delta per VMENTRY (the consistency contract is documented on
   both types and roadmap §5.4).
5. **TPM**: spec-exact command parsing (auth areas, GetCapability windowing) still wants a
   swtpm/guest oracle; EK/AIK/SRK need real keygen (RSA/ECC — large, separate).

---


## 2026-06-10 — Session: transparency-surface correctness sweep — CPUID, reprogrammable PIRQ links, timing/LBR stealth, and a real TPM (SHA-256)

**20 commits, each independently green.** `acpica-tools` (`iasl` 20230628) + `dmidecode`
3.5 install cleanly and were used as hard validators; the ACPI integration tests now
actually run (not self-skip). `/dev/kvm` is **still absent** (verified — no nested virt),
so KVM/guest-boot paths were not run. Workspace tests **860 → 881** (`cargo test
--workspace`: 881 passed, 0 failed, 1 ignored = the `/dev/kvm` self-skip). `cargo build
--workspace`, `cargo fmt --all -- --check`, and `cargo clippy --all-targets --workspace -- -D
warnings` all green. no_std custom-target: N/A (no crate is `#![no_std]`).

### Arc 1 — CPUID stealth correctness (`enlil-devices::stealth::cpuid`), web-researched
- **`304b445`** Out-of-range leaves now mirror **bare metal**: Intel returns the highest
  basic leaf's data for any out-of-range leaf (incl. the `0x4000_0000` hypervisor region);
  AMD returns zeros. The table previously returned **zeros** everywhere out of range — which
  no real Intel CPU does and is a detection vector. Verified with a real-CPU reference dump
  taken on the runner (`__cpuid_count`) + Intel SDM / QEMU's "return highest basic leaf"
  behaviour.
- **`86345ca`** Leaf `0x1` EBX[23:16] (max addressable IDs) now tracks `vcpu_count` (power-of-two,
  gated on HTT) instead of a fixed constant that disagreed with leaf-0xB topology.
- **`be2c3ae`** Removed the now-dead cached `0x4000_0000` zero entries (lookup routes that
  region through the out-of-range value).
- **`3cda674`** Leaf `0x80000008` fixed to 48/48 address sizes (was `0x3930` = 57-bit linear =
  phantom LA57 inconsistent with leaf-7 ECX; comment had the fields swapped).

### Arc 2 — Reprogrammable PIC interrupt link routing (ACPI / `iasl`-validated)
- **`28a3bea`** New `AmlBuilder` expression primitives: `OperationRegion`, `Field`, `Store`,
  `And`/`Or`/`ShiftLeft`/`Subtract`, `FindSetRightBit`, `CreateWordField`, `Return(name)`,
  rooted/multi-seg name paths, an `Operand` model — all byte-decode unit-tested.
- **`04ec209`** `LNKA-D` `_CRS`/`_DIS`/`_SRS` are now **live** against the PIIX3 PIRQRC
  registers (OperationRegion+Field over the ISA bridge config 0x60-0x63), so a PIC-mode guest
  can actually reroute a PCI interrupt and `_CRS` reflects it. The `_PRT` references the links
  by the rooted path `\_SB.PCI0.ISA_.LNKx`, emitted before the `_PRT`. Whole DSDT round-trips
  through `iasl` at 0 errors / 0 warnings.
- **`ae22dfa`** `_STA` reflects the route-disable bit (returns 0x09 when disabled) via a new
  `if_start()` (caller-built predicate). **`c3c6f55`** corrected a stale `_PIC`/`_PRT` doc.

### Arc 3 — Timing/branch stealth correctness (Phase 5.4, `enlil-core::timing_stealth`)
- **`4213a21`** APERF/MPERF VMEXIT hiding: the MPERF adjustment was an algebraic no-op
  (`aperf/(aperf/mperf)==mperf`), so MPERF never hid exit overhead and the APERF/MPERF ratio
  skewed. Now decrements both counters proportionally (ratio preserved).
- **`b0df46d`** LBR sanitizer overwrote only `from`, leaving the hypervisor address in `to`
  (the field a detector reads); now erases both endpoints, matching the canonical
  `enlil-devices::stealth::lbr`.

### Arc 4 — A real TPM: SHA-256 + PCR semantics (Phase 5.5)
- **`8505e33`** New dependency-free **SHA-256** (FIPS 180-4) in `enlil-devices::crypto`,
  verified against the FIPS vectors (empty, "abc", two-block, 56-byte boundary).
- **`c9cc40c`** `enlil-core::vtpm` PCR extend now uses real `SHA256(old || measurement)`
  instead of XOR. **`68c0083`** fixed wrong TPM2 command codes in the vTPM dispatcher
  (Extend 0x182 / Read 0x017E / GetCapability 0x017A — they were 0x17E/0x17F/0x100).
- **`889ad26`** The **device** TPM (CRB MMIO at 0xFED40000) now handles `PCR_Extend`/`PCR_Read`
  against its SHA-256 bank (were a permissive no-op). **`572de5d`** `GetRandom` returns a
  varying xorshift64* stream instead of a fixed `(i*7+13)` pattern.

### Arc 5 — Consolidation (the hand-off's dedup items)
- **`d94deab`** Removed the dead, duplicate `enlil-core::smbios` module (unused, no
  serialization path; canonical is `enlil-devices::smbios::SmbiosBuilder`).
- **`da318fd`** Removed the unused, buggy `CpuidCachingHelper` stub from `timing_stealth`
  (duplicated the canonical `CpuidStealthTable` with the all-zeros `0x4000_0000` tell).

### Research (informed the build)
`RESEARCH.md` → "2026-06-10" (CPUID out-of-range, web-researched) and "2026-06-10 (b)"
(reprogrammable PIRQ links, APERF/MPERF, LBR, TPM/SHA — primary specs: ACPI 6.x, PIIX3
datasheet, Intel SDM, TCG TPM 2.0, FIPS 180-4).

### Test results (exact)
- `cargo build --workspace` → OK. `cargo fmt --all -- --check` → OK. `cargo clippy
  --all-targets --workspace -- -D warnings` → OK. `cargo test --workspace` → **881 passed,
  0 failed, 1 ignored**.
- `iasl` round-trips all ACPI tables (incl. the new reprogrammable-link DSDT) at 0/0;
  `dmidecode` parses the SMBIOS cleanly. Both tools installed on the runner this session.
- `/dev/kvm`: **not run — absent (no nested virt)**, verified.

### Recommended next steps (tomorrow)
1. **CI should install `acpica-tools` + `dmidecode`** to make the ACPI/SMBIOS integration
   tests hard gates (they self-skip when absent; they were exercised this session).
2. **Two TPM models exist** — `enlil-core::vtpm::VirtualTpm` (management/dispatcher, byte-vec
   I/O) and `enlil-devices::tpm::VirtualTpm` (CRB MMIO at 0xFED40000). Both now share the
   `crypto::sha256` PCR semantics but are separate types. Decide which is canonical for the
   bus path and consolidate (the device one is bus-facing). **Flagged, not done** — needs a
   deliberate pass, not a drive-by.
3. **TPM `GetCapability` is a stub** on both (returns success with little/no `TPML_CAPABILITY_DATA`),
   and EK/AIK/SRK are zero-filled with no real keygen (needs RSA/ECC — a large crypto addition).
   The TPM2 command wire formats here are *pragmatic, not spec-exact* (auth area / digest list
   simplified); a real Windows guest will need spec-exact parsing — **best validated against a
   guest or swtpm, so blocked on a KVM runner / oracle.**
4. **PMC fixed counters** (`enlil-devices::stealth::pmc`) advance instructions-retired, core
   cycles, and reference cycles by the *same* `guest_cycles` (IPC exactly 1.0, core==ref) —
   analogous to the APERF/MPERF ratio tell, but "fixing" it needs a chosen plausible IPC /
   freq ratio. **Flagged** as a model refinement (not a clear-cut bug).
5. **`LbrSanitizer::expected_guest_branch_target`** field is now unused (the fix uses the guest
   RIP for both endpoints); either wire it (a real "next instruction" target) or drop it.
6. **q35 chipset identity** (host bridge advertises i440FX while exposing PCIe ECAM/PIIX3) and
   the **KVM run loop** remain the big items — q35 is a deliberate architectural pass; the KVM
   loop is blocked on `/dev/kvm`. **Ask for a nested-virt runner** to unblock guest-boot and the
   spec-exact-TPM validation.

## 2026-06-09 — Session: validate the firmware-description surface with real reference parsers (`iasl` + `dmidecode`), fix every bug they flag (Phase 0.2 / 5.1 / 5.2)

**The unlock:** `acpica-tools` (`iasl` 20230628) **and** `dmidecode` 3.5 both install cleanly
from the distro repo on this runner — the previous sessions kept deferring AML/ACPI
correctness because no ACPI disassembler was present. So for the first time the synthesized
ACPI and SMBIOS were parsed by the *same* reference tools a real OS uses. This caught a string
of latent bugs that byte-level unit tests had missed, and unblocked the long-deferred PIC-mode
`_PRT`. **15 commits, each independently green.** `/dev/kvm` is **still absent** (verified, no
nested virt). Workspace tests **839 → 860** (`cargo test --workspace`: 860 passed, 0 failed, 1
ignored — the `/dev/kvm` self-skip). All of `cargo fmt --all -- --check`, `cargo clippy
--all-targets --workspace -- -D warnings`, and `cargo test --workspace` are green.

### Arc 1 — `iasl` ACPI validation + the bugs it found
- **`4700b04`** validation harness + **drop spurious `_ADR` from PCI0**. The PCI host bridge
  carried both `_HID` and `_ADR` (ACPI §6.1: a Device uses one or the other; iasl warns 3073).
  Removed `_ADR` — DSDT now compiles 0 errors / 0 warnings.
- **`2964e45`** integration test that round-trips **every** table through `iasl -d` + recompile,
  asserting 0 errors / 0 warnings; self-skips when iasl absent.
- **`960528a`** **TPM2 was truncated**: declared revision 4 but emitted the 52-byte rev-3 body,
  so iasl reported "table terminates in the middle of a data structure." Emit the full 76-byte
  rev-4 layout (Start Method params + Laml/Lasa log fields).
- **`9ad349d`** **emit a FACS** + point the FADT's `FIRMWARE_CTRL`/`X_FIRMWARE_CTRL` at it (was
  zero — no FACS at all). 64-byte v2 structure, no header/checksum, 64-byte aligned in the
  XSDT→FADT padding.
- **`0519cfc`** test the **RSDP→XSDT→table→DSDT pointer chain** (every guest-physical pointer
  resolves to a signature-valid, checksummed table); **`<cross-table>`** DSDT processor devices
  ↔ SSDT power scopes stay in lock step.

### Arc 2 — interrupt routing: dual-mode `_PRT` + PCI link devices, end-to-end consistent
- **`8cb3618`** `_PRT` is now a **mode-selecting method** — `If (PICF) Return (APIC GSIs); Return
  (PIC IRQs)` — so a PIC-mode guest (or the window before `_PIC(1)`) gets working routing. New
  AML primitives: `if_name_start`/`if_end`, `return_routing_table`; shared `PIRQ_DEFAULT_IRQS`
  ([11,10,5,6]).
- **`<link devices>`** added the four **`PNP0C0F` link devices** (`LNKA-D`, `_PRS`/`_CRS`/`_STA`/
  `_DIS`/`_SRS`) and routed the PIC `_PRT` through them (Source = link `NameSeg`). New AML:
  `ResourceTemplate::irq_flags`, `return_routing_table_via_links`.
- **`<pirq program>`** `standard_pc_complete` now **programs the live `PIRQRC[A-D]`** registers to
  `PIRQ_DEFAULT_IRQS` (out of their 0x80 reset state) — the firmware step — so the `_PRT`, the
  link `_CRS`, the `PirqRouter`, and the config-space bytes all agree.
- **`417958d`** SSDT now defines **per-vCPU** `_PSS`/`_CST` (was CPU0-only on a false "Windows
  inherits" premise). iasl-validated across 1/2/4/16/255 vCPUs.
- **`4b4abdc`** fixed the **DSDT ISA bridge `_ADR`** from `0x001F0000` (ICH 1F.0) to `0x00010000`
  (the PIIX3 at 00:01.0 the bus actually mounts), sourced from a shared `pcie::PIIX3_ISA_BRIDGE_BDF`.

### Arc 3 — `dmidecode` SMBIOS validation + the bugs it found
- **`e03d0b9`** three fixes: (1) BIOS **"virtual machine" characteristic** bit cleared (a VM tell);
  (2) Type 3 missing the SMBIOS-2.7+ **SKU byte** (Length 22 vs 21 written → eaten string char +
  `<BAD INDEX>`); (3) Type 4 missing the SMBIOS-3.0 **16-bit core/thread counts** (Length 48 vs 42
  → shifted string table, wrong Manufacturer, `<BAD INDEX>` Part Number).
- **`f204b08`** `dmidecode --from-dump` integration test (no `<BAD INDEX>`, no "virtual machine",
  string table aligned); self-skips when dmidecode absent.
- **`315bc08`** folded the iasl findings into `RESEARCH.md` + `ROADMAP.md`.

### Test results (exact)
- `cargo build --workspace` → OK. `cargo fmt --all -- --check` → OK. `cargo clippy --all-targets
  --workspace -- -D warnings` → OK. `cargo test --workspace` → **860 passed, 0 failed, 1 ignored**.
- `iasl` round-trips **all** ACPI tables (and the DSDT under default + 8 config variants, 1-255
  vCPUs) at **0 errors / 0 warnings**; the DSDT+SSDT cross-disassemble cleanly (externals resolve).
- `dmidecode --from-dump` parses the SMBIOS table with **no `<BAD INDEX>`** and no VM tell.
- `/dev/kvm`: **not run — absent (no nested virt)**, verified. no_std custom-target: N/A (no crate
  is `#![no_std]`).

### Recommended next steps (tomorrow)
> The ACPI + SMBIOS firmware-description surface is now **reference-validated and clean**. CI
> should install `acpica-tools` + `dmidecode` to make the two integration tests hard gates.
1. **CPUID stealth pass (`enlil-devices::stealth::cpuid`).** Highest-value remaining transparency
   surface (primary VM-detection vector). Hypervisor bit / `0x40000000` zeroing are correct; two
   items need a **real-CPU reference dump** to fix confidently: leaf `0x1` `EBX`
   max-addressable-IDs is a fixed constant (should track `vcpu_count`/HTT), and leaf `0x80000008`'s
   address-size value vs. its comment look swapped. No `iasl`/`dmidecode`-style validator exists, so
   this is a careful, reference-backed pass — don't guess.
2. **Reprogrammable PIC link routing.** `_SRS`/`_DIS` on `LNKA-D` are accepted no-ops today. To let
   a PIC-mode guest *re*-route, give them an `OperationRegion(PCI_Config)` + `Field` over the bridge
   config (`0x60-0x63`) and the buffer-manipulation AML (`CreateField`/`FindSetRightBit`/`And`/`Or`/
   `Store`) — new `AmlBuilder` primitives, all `iasl`-validatable now.
3. **Chipset identity (cross-cutting, deferred).** Host bridge advertises `0x1237` (i440FX) while the
   platform exposes PCIe ECAM/MCFG and the LPC is at PIIX3 `00:01.0`. A fully consistent modern
   machine is **q35** (MCH `0x29C0`, ICH9 LPC at `00:1F.0`); converting touches `device_bus`, the
   bridge BDF, the host-bridge ID, several tests, and would revert the `00:01.0` `_ADR` fix — do it
   as a deliberate, focused architectural pass, not a drive-by.
4. **Consolidate the two SMBIOS builders** (`enlil-devices::smbios` canonical vs the unused
   `enlil-core::smbios`) and wire the canonical one into the boot/`fw_cfg` delivery path.
5. **KVM run loop** (blocked on `/dev/kvm`). Unchanged: ask for a nested-virt runner. When available,
   the `KvmBackend` builds its bus via `standard_pc_complete`, applies the `CpuidStealthTable` via
   `KVM_SET_CPUID2`, loads the now-clean ACPI table set at `table_base_address`, and drives
   `poll_platform_events`.

---

## 2026-06-08 (b) — Session: finish the legacy-PC transparency surface — 8237 DMA, `0xCF9` reboot, the `0xB2` ACPI-enable handshake (Phase 0.2)

A 7-increment session, each a full orient→build→verify trip and an independently-green commit,
forming **three coherent arcs** that close the remaining *unblocked* open-bus / boot-correctness
gaps a guest hits before it ever reaches userspace. `/dev/kvm` is **still absent** (verified: no
`/dev/kvm`); `iasl`/`acpidump` are **not installed**. Workspace tests **814 → 839** (`cargo test
--workspace`: 839 passed, 0 failed, 1 ignored — the one ignored is the `/dev/kvm` self-skip).
Picked up from the prior hand-off's option list: #1 (PIC-mode `_PRT`) stays deferred (needs
`iasl`), #4 (KVM run loop) stays blocked (no `/dev/kvm`), so I built #2 (8237 DMA) and then the
next viable unblocked items.

### Arc 1 — 8237A DMA subsystem (increments 1-3, the prior hand-off's #2)
1. **`536da02`** `enlil-devices::dma` — the two cascaded **8237A DMA controllers** (DMA-1 8-bit at
   `0x00-0x0F`; DMA-2 16-bit at `0xC0-0xDF`, registers on 2-byte spacing) + the **DMA page
   registers** (`0x80-0x8F`, with the PC/AT non-linear channel→port map). A faithful **passive**
   register model: base/current address+count per channel behind the shared byte-pointer
   flip-flop, command/status/request/single+all-mask/mode registers, master clear. *No transfer
   engine* — nothing in-tree owns a channel yet. 18 unit tests (decode-the-bytes).
2. **`4b1b22c`** wired into `DeviceBus::standard_pc_complete` (`add_dma_controllers`) and the
   three windows claimed in the DSDT's `SYSR` `_CRS` (`PNP0C02`), so an ISA-DMA probe (Linux
   always does at boot) reads coherent state, not open-bus `0xFF`.
3. **`8135a29`** `dma::transfer_address`/`transfer_byte_count` — the page-latch + channel-address
   → 24-bit physical-address composition, including DMA-2's word-addressing (`addr << 1`, A0=0)
   and ignored page bit 0. Completes the address model; the *data-movement* engine still waits on
   a consumer (floppy/SB16).

### Arc 2 — chipset Reset Control Register `0xCF9` (increments 4-5)
4. **`606cc6f`** `pcie::PciConfigIo` now decodes **`RST_CNT` at `0xCF9`** (`reboot=pci`, the path
   every modern OS uses). `0xCF9` physically sits inside the `CONFIG_ADDRESS` dword window, but a
   *byte* access hits `RST_CNT`, not config-address byte 1 — so a guest reading `0xCF9` gets the
   reset register (not a leaked config byte), and a `RST_CPU` (bit 2) write latches a reboot
   (`SYS_RST`/`FULL_RST` read back). Dword `CONFIG_ADDRESS` writes (`0xCF8`) and `0xCFA`/`0xCFB`
   are untouched. (Updated the one existing test that byte-read `0xCF9` — real HW never exposed
   config byte 1 there.)
5. **`86a79f3`** surfaced the latch (`PciResetControl`, a shared `Rc<RefCell>`) out of `add_pcie`
   via `DeviceBus::pci_reset_handle` (no signature change → no caller breakage), so
   `StandardPc::poll_platform_events` now returns `PlatformEvent::Reset` for the `0xCF9` path too
   (alongside `0x92`); **both reset latches are drained each poll** (`|`, not `||`) so neither is
   stranded.

### Arc 3 — SMI command port / ACPI-enable handshake (increment 6)
6. **`6b4640b`** `chipset::SmiCommandPort` (`0xB2`, the FADT's `SMI_CMD`), wired into
   `standard_pc_complete` (`add_smi_command`). **This was a real boot blocker:** the FADT
   advertises a non-zero `SMI_CMD` with `ACPI_ENABLE=0xA0`, so ACPICA enters ACPI mode by writing
   `0xA0` to `0xB2` and **polling `SCI_EN`** in `PM1a_CNT`. Port `0xB2` was unmodelled → the write
   hit open bus, `SCI_EN` never set, and the OS would abort with "Could not enable ACPI mode."
   There's no SMM, so the port now sets `SCI_EN` directly in the shared `PM1a` block
   (`AcpiPm1Block::set_acpi_mode`); `ACPI_DISABLE=0xA1` clears it. The FADT now sources
   `SMI_CMD`/`ACPI_ENABLE`/`ACPI_DISABLE` from the `chipset::{SMI_CMD_PORT,ACPI_ENABLE_VALUE,
   ACPI_DISABLE_VALUE}` constants the port uses, so the advertised handshake and the decoding
   hardware can't drift. Verified end-to-end through the bus (write `0xB2`, read `SCI_EN` set).

### Arc 2 (closeout) — drift-proof the FADT reset register (increment 7)
7. **`<this commit>`** The FADT already advertised `RESET_REG = io(0xCF9, 8)` / `RESET_VALUE =
   0x06` — increments 4-5 just made that advertisement *functional*. Sourced those FADT literals
   from the new `pcie::{RESET_CONTROL_PORT, RST_CNT_REBOOT_VALUE}` constants (mirroring how the PM
   ports + `SMI_CMD` already derive from device constants), so the ACPI-advertised reset register
   and the `0xCF9` hardware behind it provably can't drift. +2 cross-check tests (FADT reset reg
   ↔ pcie constant; FADT `SMI_CMD`/enable/disable ↔ chipset constants).

### Research (informed the build)
Logged under `RESEARCH.md` → "2026-06-08 (b)": Intel **8237A** datasheet + IBM PC/AT DMA wiring
(cascaded controllers, 2-byte DMA-2 stride, non-linear page-register map, word-addressing). The
`0xCF9` `RST_CNT` and `0xB2` `SMI_CMD` behaviours are from the PIIX/ICH datasheets + the ACPI
spec's mode-switch handshake (ACPICA `AcpiHwSetMode` writes `ACPI_ENABLE` to `SMI_CMD` and polls
`SCI_EN`) + QEMU's no-SMM `cf9`/`rcr` and SMI conventions — all stable, primary-source firmware
behaviour, not recent papers. Roadmap updated surgically at §0.2 (three dated status notes).

### Test results (exact)
- `cargo build --workspace` → **OK**.
- `cargo fmt --all -- --check` → **OK**.
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (new code clean under
  `enlil-devices`'s strict `deny(all, pedantic, nursery)`).
- `cargo test --workspace` → **839 passed, 0 failed, 1 ignored** (was 814). +25 tests
  (15 dma controllers/pages, 3 dma address-composition, 2 `0xCF9`, 2 SMI port, 1 core SMI bus
  integration, 2 FADT drift cross-checks; the `0xCF9` poll/handshake reused/extended existing
  tests).
- `/dev/kvm` paths: **not run — no `/dev/kvm` (no nested virt)**; verified absent.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.
- `iasl`/AML disassembler: **not installed** on the runner (no new raw-AML emission this session,
  so nothing newly needs it).

### Recommended next step (tomorrow)
> The legacy-PC / ACPI-init *open-bus* surface is now thoroughly covered (DMA, `0xCF9`, `0xB2`,
> PM1/PM_TMR/GPE0, PIC/PIT/RTC/PS2/HPET, PCI CAM+ECAM). What's left is either **blocked** (needs
> `/dev/kvm` or `iasl`) or **large/cross-cutting** (needs a PCI INTx source). There's no longer a
> small fully-unblocked legacy-register increment lying around — the next real progress wants one
> of: a KVM-capable runner, `acpica-tools`, or starting a virtio PCI device.
1. **8237 transfer engine + a consumer.** Deferred by design until a DMA *consumer* exists. If you
   add a **floppy controller (8272A, `0x3F0-0x3F7`, DMA ch 2)** or SB16, drive a real channel
   (decrement current addr/count via `transfer_address`/`transfer_byte_count`, raise TC, handle
   autoinit) at that point. Floppy is the classic unblocked legacy device still missing, though
   modern guests rarely need it (absence is also valid — no `PNP0700` is declared).
2. **PIC-mode `_PRT` via PCI Link Devices (still wants `iasl`).** Unchanged from prior hand-off:
   needs `If`/`Else`/`Store` control-flow AML helpers; **strongly want `acpica-tools` installed**
   first to validate (the PkgLength bug class is exactly what a disassembler catches). Defer if
   `iasl` still absent.
3. **Live PIRQ INTx source.** `assert_pci_intx` + the `_PRT` exist but nothing asserts INTx yet.
   When a PCI device model (virtio-blk/net) lands on the bus, route its INTx pin through
   `assert_pci_intx`. This is the prerequisite that unblocks the roadmap's named "live PIRQ path".
4. **KVM run loop (blocked on `/dev/kvm`).** Unchanged: when a nested-virt runner exists, have
   `KvmBackend` build its bus via `standard_pc_complete`, drive `advance_clocks`/`tick_rtc` from a
   timer thread, act on `poll_platform_events` (now incl. `0xCF9` reset + ACPI-enabled `SCI_EN`),
   and deliver interrupts through `KVM_IRQ_LINE`. **Consider asking for a KVM-enabled runner +
   `acpica-tools`** — those two unblock the largest remaining chunks (1-3 above are what's left
   without them, and none is a small fully-unblocked register increment).

---

## 2026-06-08 — Session: make the synthesized DSDT actually correct — AML PkgLength, `_CRS`, `_PRT`/`_PIC`, `_S5` (Phase 0.2 / 5.1)

An 11-increment session, each a full orient→build→verify trip and an independently-green commit,
forming one coherent arc: **the DSDT the previous sessions built had never been parsed by a real
ACPI interpreter (no `/dev/kvm`), and it turned out to be malformed — fix the foundational AML
encoder bug, then add the device-resource and interrupt-routing objects a guest actually needs to
discover and drive the hardware.** Every increment is pure-userspace, fully exercised by unit
tests that *decode the emitted bytes*. Workspace test count: **797 → 814 (+17)**. `/dev/kvm` is
**still absent on this runner (verified: no `/dev/kvm`, no vmx/svm in `/proc/cpuinfo`)**; the one
KVM-gated test self-skips (1 ignored). `iasl`/`acpidump` are **not installed** on the runner, so
AML was validated by hand-written decode tests rather than a reference disassembler.

### The headline finding (increment 1)
**`AmlBuilder::patch_pkg_length` produced a wrong, self-overshooting `PkgLength` for every
scope/device/method in the DSDT.** It encoded `total_len` *including* the 4 reserved bytes and
then shifted the body left without recomputing, so the field claimed `+shift` too many bytes. A
real ACPI interpreter reads `PkgLength` as "bytes from this field to package end," so it would
have run past every package and corrupted all following AML — i.e. the DSDT was unparseable. It
stayed latent only because no guest had ever booted. (`ssdt.rs`'s hand-rolled path already did the
self-reference correctly; the `AmlBuilder` used by the DSDT did not.) Fixed with
`encode_self_pkg_length` (counts the field's own size per ACPI §20.2.4) + decode-based regression
tests (1-byte, 2-byte, nested). **This was a prerequisite for everything else below.**

### Increments (each its own green commit)
1. **`38c850d` PkgLength self-reference fix** — see above.
2. **`a9064a0` `ResourceTemplate` + `name_resource_template`** — I/O Port (`0x47`), IRQ (`0x23`),
   Memory32Fixed (`0x86`) descriptors; wraps them in a `Buffer` with End Tag + self-consistent
   `PkgLength`/`BufferSize` (ACPI §6.4).
3. **`dfc7067` COM1 `_CRS`** — configured I/O window (8 ports) + IRQ from `DsdtConfig`.
4. **`11d82f8` RTC + PS/2 `_CRS`** — RTC 0x70-0x71/IRQ8; keyboard 0x60+0x64/IRQ1; mouse IRQ12
   (shares the i8042 ports), matching how real namespaces split the i8042.
5. **`3227e7c` HPET `_CRS`** — Memory32Fixed for the 1 KiB block at the fixed HPET base.
6. **`4befddb` `_S5` is now a Package** — `name_package`; `Name(_S5_, Package(){5,5,0,0})`. Was a
   bare integer, so a guest had **no usable S5 object and could not ACPI-shutdown**. SLP_TYP 5
   matches what `enlil_devices::chipset` captures as a shutdown request.
7. **`1c0a617` PCI root `_CRS`** — Word/DWord/QWord Address Space descriptors (`0x88`/`0x87`/
   `0x8A`); PCI0 produces bus 0-0xFF, the legacy I/O ports (split around the config aperture), and
   the 32-/64-bit MMIO holes from `DsdtConfig`. Required for Windows PCI enumeration.
8. **`f25ba01` `PNP0C02` motherboard-resources device** — `SYSR` claims the fixed legacy I/O Enlil
   models (both 8259s, PIT, ports 0x61/0x92, ELCR) so the guest's PnP manager treats them as
   consumed, not free.
9. **`e06defb` PCI `_PRT` (APIC mode)** — `name_routing_table` (package of integer sub-packages);
   PCI0's `_PRT` generated from `PirqRouter::device_gsi` so the DSDT and the live `assert_pci_intx`
   path agree by construction (swizzle → GSI 16-19, `{(slot<<16)|0xFFFF, pin, 0, GSI}`). Without
   `_PRT` a guest can route **no** PCI interrupt.
10. **`eca2ea3` `_PIC` method + `PICF` flag** — matches real firmware (a missing `_PIC` is a VM
    tell); stores the announced interrupt mode into `PICF` for a future mode-selecting `_PRT`.

(That's 10 code commits + this hand-off = the 11 increments; #1 spanned the fix and its tests.)

### Research (informed the build)
Logged under `RESEARCH.md` → "2026-06-08": ACPI 6.x §20.2.4 (self-inclusive PkgLength — the bug),
§6.4 (small/large/address-space resource descriptors + flags), §6.2.13/§5.8.1 (`_PRT`/`_PIC`),
§7.4.2.6 (`_Sx` packages). All stable firmware conventions — the authoritative sources are the
primary ACPI spec, not recent papers. Roadmap updated surgically at §5.1 (DSDT status + the
PIC-mode `_PRT` follow-up).

### Test results (exact)
- `cargo build --workspace` → **OK**.
- `cargo fmt --all -- --check` → **OK**.
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; new code lint-clean
  under `enlil-devices`'s strict `deny(all, pedantic, nursery)`).
- `cargo test --workspace` → **814 passed, 0 failed, 1 ignored** (was 797). +17 tests, all
  decode-the-emitted-bytes checks. The 1 ignored is the `/dev/kvm` self-skip.
- `/dev/kvm` paths: **not run — no `/dev/kvm` (no nested virt)**; verified absent on this runner.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.
- **`iasl`/AML disassembler: not available on the runner** — AML validated by hand-written decode
  tests (PkgLength self-consistency, descriptor field offsets, nested packages), not a reference
  tool. Worth installing `acpica-tools` on a future runner to cross-check.

### Recommended next step (tomorrow)
1. **PIC-mode `_PRT` via PCI Link Devices + a method-based `_PRT` (high value, medium-high effort,
   needs new AML helpers).** Add `AmlBuilder` control-flow helpers — `Store(arg, name)`, `If`/
   `Else` (with the same self-inclusive `PkgLength`), `Return(name-ref)`, `LEqual` — then model
   four `PNP0C0F` PCI Link Devices (`LNKA`-`LNKD`) whose `_STA`/`_DIS`/`_CRS`/`_PRS`/`_SRS` read
   and write the PIIX3 PIRQRC registers, and make `_PRT` a *method* that returns the APIC table
   when `PICF==1` and a link-device-sourced table otherwise. **Strongly want `iasl` on the runner
   first** to validate the control-flow AML (the PkgLength bug class is exactly what a disassembler
   catches). If `iasl` can't be installed, keep deferring and pick #2/#3.
2. **8237 DMA controller (medium effort, lower modern value, fully unblocked).** Ports 0x00-0x0F
   (ch 0-3), 0xC0-0xDF (ch 4-7), page registers 0x80-0x8F; command/status/mask/mode + the
   address/count flip-flop. Linux `request_region`s these at boot; modelling them removes an
   open-bus surface. Pure userspace. (Its I/O would also belong in the `SYSR` `_CRS` once modelled.)
3. **Live PIRQ from a real INTx source.** `assert_pci_intx` and the new `_PRT` both exist, but
   nothing in-tree asserts INTx yet. When a PCI device model (virtio-blk/net) is wired onto the
   bus, route its INTx pin through `assert_pci_intx`, and fold `assert_pci_intx` + the run-loop
   tick into the eventual KVM run loop.
4. **KVM run-loop binding (still blocked on `/dev/kvm`).** Unchanged from prior hand-offs: when a
   nested-virt runner exists, have `KvmBackend` build its bus via `standard_pc_complete`, drive
   `advance_clocks`/`tick_rtc` from a timer thread, act on `poll_platform_events`, and deliver
   interrupts through `KVM_IRQ_LINE`/the in-kernel chip. **If still no `/dev/kvm`, do #1-#3.**

---

## 2026-06-07 (d) — Session: assemble the complete transparent PC + its ACPI PM hardware, HPET, PIRQ, and the run-loop core (Phase 0.2)

An 18-increment session, each a full orient→build→verify trip and an independently-green commit,
forming one coherent arc: **take the legacy interrupt/device groundwork from the 06-07(c) hand-off
and finish the transparent standard PC — assemble it in one call, model the ACPI power-management
hardware a real OS drives (timer, shutdown, GPEs), mount and fully wire the HPET, build the PCI
interrupt router and its live path, fix the FADT/HPET transparency drifts, and stand up the
run-loop timekeeping + event core.** Every increment is pure-userspace, fully exercised by
unit/integration tests. Workspace test count: **751 → 797 (+46)**. `/dev/kvm` is **still absent on
this runner (verified: no `/dev/kvm`, no vmx/svm in `/proc/cpuinfo`)**; the one KVM-gated test
self-skips (1 ignored).

### Increments (each its own green commit)
1. **`standard_pc_complete` + `StandardPc` bundle** — one factory assembling COM1, PIT, RTC, PS/2,
   System Control Ports A/B, ACPI PM1a/PM_TMR/GPE0, both 8259s, the I/O APIC and PCIe, with **every**
   device IRQ teed into both controllers (`dual_irq_line` helper, also refactored into
   `standard_pc_with_dual_irq`). Returns shared handles the run loop binds to.
2. **System Control Port B (`0x61`)** — PIT-ch2 gate + PC speaker + refresh-clock(bit4)/OUT(bit5)
   readback, coupled to the shared PIT. (`enlil_devices::timer::speaker`.)
3. **System Control Port A (`0x92`)** — fast A20 (enabled) + fast-reset latch. (new
   `enlil_devices::chipset` module.)
4. **PIIX3 PIRQ router model** — `interrupt::pirq::PirqRouter`: PIRQRC registers + PCI slot/pin
   swizzle + PIC-mode ISA-IRQ / APIC-mode GSI-16-19 resolution, paired with the ELCR's level IRR.
5. **HPET on the MMIO bus** — `HpetMmio` at `0xFED0_0000`, bridging the model's aligned-register
   decode to 32/64-bit guest accesses (`SharedHpet`).
6. **ACPI PM timer (`0x608`)** — free-running 32-bit (per FADT `TMR_VAL_EXT`) 3.579545 MHz counter.
7. **ACPI PM1a event/control block (`0x600`/`0x604`)** — the `SLP_TYP|SLP_EN` **shutdown** path via a
   `take_sleep` latch; PWRBTN status; SCI_EN. (`SharedAcpiPm1Block`.)
8. **FADT: drop `HW_REDUCED_ACPI`** — it was set alongside the legacy PM hardware + `LEGACY_DEVICES`
   (mutually exclusive); clearing it makes the PM1a/PM_TMR/GPE0 blocks the OS will actually use.
9. **ACPI GPE0 block (`0x620`)** — status starts clear so ACPI init isn't flooded with phantom GPEs.
10. **FADT PM-block ports derived from the device models** — single source of truth, cross-checked.
11. **PIRQ registers located in the PIIX bridge config** — `create_isa_bridge` seeds PIRQRC=0x80,
    `PirqRouter::sync_from_config` loads routing a guest programmed.
12. **`StandardPc::advance_clocks`** — the run-loop timekeeping core: advance PIT/PM-timer/HPET
    coherently from one `ns` delta (`HPET_TICK_NS = 100`).
13. **HPET interrupt delivery (I/O APIC)** — `advance_clocks` delivers fired timers to their GSI.
14. **`poll_platform_events`** — typed run-loop API returning `Sleep(SLP_TYP)` / `Reset` from the
    PM1a + (now shared) port-A latches.
15. **Live PCI INTx routing** — `StandardPc::assert_pci_intx` reads routing live from the seeded PIIX
    bridge config, swizzles, and drives **both** controllers (8259 level via ELCR + I/O APIC GSI).
16. **HPET capability register fixed** — advertise the 64-bit counter / legacy-replacement / Intel
    vendor it actually implements (matched the ACPI HPET table; cross-checked).
17. **HPET legacy-replacement delivery** — timer 0→IRQ0, timer 1→IRQ8 into both controllers.
18. **DSDT describes the HPET (PNP0103)** — and uses the previously-dead `has_hpet` flag.

### Research (informed the build)
Logged under `RESEARCH.md` → "2026-06-07 (d)": ACPI 6.4 §4.8/§5.2.9 (PM register set; the
**`HW_REDUCED_ACPI` ⊕ legacy PM hardware** finding; `TMR_VAL_EXT`→32-bit PM timer; `SLP_TYP|SLP_EN`
shutdown; GPE-open-bus pitfall), the PIIX3 datasheet (PCI interrupt routing: swizzle, PIRQRC at
config 0x60-0x63, PCI IRQs are level, APIC mode→GSI 16-19), the 8254/ICH 0x61/0x92 chipset ports,
and IA-PC HPET 1.0a (1 KiB block, 32/64-bit register access). All stable silicon/firmware — the
authoritative sources are the primary specs, not recent papers.

### Test results (exact)
- `cargo build --workspace` → **OK**.
- `cargo fmt --all -- --check` → **OK**.
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; new code lint-clean under
  `enlil-devices`'s strict `deny(all, pedantic, nursery)` — narrowing via the `truncate` helpers,
  `const fn`/`#[must_use]`/`# Panics` where asked, hex bitwise literals, split doc paragraphs).
- `cargo test --workspace` → **797 passed, 0 failed, 1 ignored** (was 751). The 1 ignored is the
  `/dev/kvm` self-skip.
- `/dev/kvm` paths: **not run — no `/dev/kvm` (no nested virt)**; verified absent on this runner.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **DSDT `_CRS` resource descriptors (high value, medium effort).** The DSDT device objects (COM1,
   RTC, PS/2, HPET, PCI) have `_HID`+`_STA` but **no `_CRS`** — Windows especially assigns resources
   from `_CRS`. The `AmlBuilder` has no resource-descriptor helpers yet (only `raw`), so this is two
   steps: add `AmlBuilder` helpers to emit a `ResourceTemplate` buffer (IO port descriptor `0x47`,
   `Memory32Fixed` `0x86`, IRQ `0x22`/`0x23`, EndTag) — *verify the byte encoding carefully, ideally
   against `iasl`/an AML disassembler since we have no runtime ACPI* — then give each device its
   `_CRS` (COM1 `0x3F8`/8/IRQ4, RTC `0x70`/2/IRQ8, KBD `0x60`+`0x64`/IRQ1, HPET `Memory32Fixed`
   `0xFED0_0000`/0x400). Do it per-device as its own increment.
2. **8237 DMA controller (medium effort, lower modern value).** Ports `0x00`-`0x0F` (ch 0-3),
   `0xC0`-`0xDF` (ch 4-7), page registers `0x80`-`0x8F`. Linux `request_region`s these at boot;
   modelling the command/status/mask/mode + address/count flip-flop registers (storing writes,
   returning sane reads) removes the open-bus surface. Pure userspace, unblocked.
3. **Live PIRQ from a *real* `INTx` source.** `assert_pci_intx` works, but nothing in-tree asserts
   `INTx` yet. When a PCI device model (virtio-blk/net) is wired onto the bus, route its INTx pin
   through `assert_pci_intx`. Also fold `assert_pci_intx` into the eventual run loop.
4. **KVM run-loop binding (still blocked on `/dev/kvm`).** When a nested-virt runner exists: have
   `KvmBackend` build its bus via `standard_pc_complete`, drive `advance_clocks` + `tick_rtc` from a
   timer thread, act on `poll_platform_events` (`Sleep(5)`→power off, `Reset`→re-init the vCPU), and
   deliver HPET/PIT/RTC/PCI interrupts through `KVM_IRQ_LINE`/the in-kernel chip. **If still no
   `/dev/kvm`, do #1/#2.**

---

## 2026-06-07 (c) — Session: finish the interrupt-correctness gaps + the missing legacy PC devices (Phase 0.2)

A six-increment session, each a full orient→build→verify trip and an independently-green commit.
It started by closing the two interrupt-correctness items the 06-07(b) hand-off flagged (MADT
override, ELCR), then — with the interrupt subsystem solid and `/dev/kvm` still absent — moved on
to the legacy PC devices a guest touches at boot that Enlil hadn't modelled or bus-mounted yet
(RTC/CMOS, PS/2), and finally fixed the long-standing "the PIT can't be ticked once it's on the
bus" limitation. Every increment is pure-userspace, fully exercised by unit/integration tests.
Workspace test count: 717 → **751** (+34). `/dev/kvm` is **still absent on this runner (no nested
virt)**; the one `/dev/kvm`-gated test self-skips (1 ignored).

### Increment 1 — MADT interrupt-source override: ISA IRQ0 → GSI 2 (`2ca41f5`)
The MADT advertises the PC/AT timer on GSI 2 (`timer_override`), but the I/O APIC line wiring used
an identity ISA-IRQ→pin map, so a guest programming GSI 2's RTE for the timer (per the MADT) would
never get a PIT interrupt (the PIT drove pin 0). Added `enlil_devices::interrupt::isa_to_gsi` (the
single source of truth, cross-checked by a test against the GSI the emitted MADT advertises) and
`SharedInterruptController::isa_line`; wired the PIT through it in both I/O APIC factories. +5 tests.

### Increment 2 — 8259 ELCR + level-triggered IRR for PCI `INTx` (`924724b`)
The PIC was edge-only; PCI interrupts are level-triggered through the chipset ELCR (`0x4D0`/`0x4D1`).
Added per-chip `elcr`/`line_level` + `Pic8259::set_line` (edge line latches one request; level line's
IRR follows the input — withdrawn before INTA, re-armed after EOI while still asserted),
`DualPic::set_irq_level`/`write_elcr` with the PIIX hardwired-edge masks (master `0xF8`, slave
`0xDE`), `SharedPic::line` now forwards both edges, and an `ElcrPort` `PioDevice` mounted by
`add_pic`. +6 tests. (`c26479b` folded both into the ROADMAP 0.2 status.)

### Increment 3 — MC146818 RTC/CMOS device model (`e393123`)
No RTC existed (ports `0x70`/`0x71` read open-bus). Added `enlil_devices::timer::rtc::Rtc146818`:
128-byte CMOS, index/data ports with the **NMI-disable bit split out of the address**, the time/date
registers driven by an injected Unix-epoch wall clock (civil-date conversion by pure unsigned
arithmetic — no time-crate dep), status registers A-D with Register B's BCD/binary + 24/12-hour modes
and Register C's read-to-clear flags, and `tick_second` raising IRQ8 for update-ended + alarm
interrupts (frozen while SET held). `SharedRtc` + `RtcPort` bus adapter. +15 tests.

### Increment 4 — mount the RTC on the bus + IRQ8 wiring (`37415e1`)
`DeviceBus::add_rtc` mounts the `0x70`/`0x71` front-end; the caller owns the `SharedRtc` to drive the
clock and attach the IRQ8 sink. +2 integration tests (read time through the ports; IRQ8 update-ended
routes through a guest-programmed I/O APIC GSI-8 RTE into LAPIC 0).

### Increment 5 — i8042 PS/2 controller on the bus + IRQ1/IRQ12 (`a59afec`)
The i8042 model existed but wasn't bus-mountable or IRQ-wired. Added `SharedI8042` (holds the
controller + keyboard/mouse `IrqLine` sinks, reconciled from the pending flags after each access, so
the register model and its tests stayed untouched) and two **single-port** `PioDevice` adapters
(`Ps2DataPort` `0x60`, `Ps2CmdPort` `0x64`) that deliberately leave `0x61`-`0x63` (PC-speaker/NMI
ports) unclaimed. `DeviceBus::add_ps2`. +5 tests incl. a keyboard-IRQ1-through-the-I/O-APIC
integration test.

### Increment 6 — `SharedPit`: tick the PIT after it is bus-mounted (`711676c`)
A boxed `Pit` was reachable only through `PioDevice`, so nothing could call `Pit::tick` once mounted
(the limitation that blocked an assembled-bus IRQ0 test). Added `SharedPit` + `PitPort` +
`DeviceBus::add_pit_shared`. This also closed the loop increment 1 couldn't test: an end-to-end test
wires the PIT's IRQ0 via `isa_line(0)` to **GSI 2**, mounts it, programs channel 0 + GSI 2's RTE
through the bus, ticks the PIT from the handle, and observes the timer interrupt land at LAPIC 0 on
the overridden pin. +1 integration test.

### Research (informed the build)
Logged under `RESEARCH.md` → "2026-06-07 (c)": ACPI MADT Interrupt Source Override (spec §5.2.12.5),
PIIX3 ELCR / PCI level-triggered `INTx`, and the MC146818 RTC/CMOS register map. All ancient/stable
silicon + firmware conventions — no recent paper changes the models; the authoritative sources are
the primary datasheets and the ACPI spec.

### Test results (exact)
- `cargo build --workspace` → **OK**.
- `cargo fmt --all -- --check` → **OK**.
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean). New code is lint-clean
  under `enlil-devices`'s strict `deny(all, pedantic, nursery)` (no blanket allow): unsigned-only
  calendar math through the `truncate` helpers, `const fn` / `#[must_use]` / `# Panics` where clippy
  asked, `is_multiple_of`, let-chains.
- `cargo test --workspace` → **751 passed, 0 failed, 1 ignored** (was 717). The 1 ignored is the
  `/dev/kvm` self-skip.
- `/dev/kvm` paths: **not run — no `/dev/kvm` (no nested virt)**; they self-skip.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Wire RTC + PS/2 (and the shared PIT) into the `standard_pc_*` factories**, so the assembled
   "standard PC" is actually complete: mount the RTC/PS2 ports and tee IRQ8/IRQ1/IRQ12 into the
   PIC + I/O APIC the way the PIT/UART already are. Needs the factories to take `&SharedRtc` /
   `&SharedI8042` / `&SharedPit` (or to create+return them) — changes 3 signatures + ~6 test call
   sites, so do it as its own increment. Pure userspace, unblocked.
2. **System Control Port B (`0x61`) + PC speaker**, now tractable via `SharedPit`: bit 0 drives PIT
   channel-2 gate, bit 1 the speaker enable, bit 5 reads back PIT ch2 OUT, bit 4 toggles the refresh
   clock. Couples to `SharedPit` (channel 2). Small, pairs with this session. Also `0x92` (System
   Control Port A: fast A20 + fast reset) as a trivial standalone if more is wanted.
3. **PCI INTx → PIRQ routing (PIIX PIRQ router).** PCI devices' INTA-D pins route through the
   chipset PIRQ registers to ISA IRQs (level, via the ELCR built this session) and to I/O APIC GSIs
   16-19. This is the architecturally significant continuation of the interrupt work and the natural
   pairing with the ELCR. Larger; needs the PCI-to-ISA bridge config registers.
4. **KVM binding (still blocked on `/dev/kvm`).** When a nested-virt runner exists: bind the
   `SharedPic`/`SharedInterruptController`/`SharedRtc`/`SharedPit`/`SharedI8042` to the run loop, have
   `KvmBackend` build its bus via a `standard_pc_*` factory, and tick the PIT/RTC from a timer thread.

---

## 2026-06-07 (b) — Session: legacy 8259A PIC — chip model → bus front-end → factories (Phase 0.2)

A four-increment session, each a full orient→build→verify trip and an independently-green
commit, forming one coherent arc: **build the legacy dual-8259 PIC — the interrupt controller
early boot runs in *before* the OS switches to the I/O APIC — from the chip model up to a bus
config where one device line drives both controllers.** This took the 06-07 hand-off's
recommended next step #2 ("Legacy 8259 PIC, pure userspace, unblocked"). `/dev/kvm` is **still
absent on this runner (no nested virt)**, so every increment is pure-userspace, fully exercised
by unit/integration tests. Workspace test count: 693 → **717** (+24).

### Increment 1 — `Pic8259` + cascaded `DualPic` chip model (`55a8973`)
New `enlil_devices::interrupt::pic`. `Pic8259` single chip: the ICW1-4 init state machine
(cascade/single, vector base, cascade wiring, 8086/auto-EOI), OCW1 (mask) / OCW2 (specific +
non-specific EOI) / OCW3 (read-register select, poll command, special-mask mode), IRR/ISR/IMR,
and **fully-nested fixed-priority** resolution (IR0 highest — "lowest set ISR bit is the
ceiling"). `DualPic`: master (`0x20`/`0x21`) + slave (`0xA0`/`0xA1`) with the slave `INT` wired
to master IR2, **computed on demand** so the two-INTA cascade `acknowledge()` and the
both-chips EOI fall out naturally; `pending_vector()`/`has_interrupt()` expose the INTR line.
16 unit tests (init, fixed priority, mask-latches-but-blocks, in-service nesting, specific +
non-specific EOI, cascade routing + EOI + preemption, read-register select, poll, auto-EOI,
special-mask, single mode, inert uninitialized ports).

### Increment 2 — PIC bus front-end: `SharedPic` + PIO adapters + `.line()` (`f32f079`)
`SharedPic` (`Arc<Mutex<DualPic>>`, mirroring `SharedInterruptController`) with `.with()` locked
access, a `.line(irq)` `Fn(bool)+Send` level sink (rising edge → `raise_irq`; satisfies both
crate `IrqLine` traits via their blanket impls), and `PicMasterPort`/`PicSlavePort` `PioDevice`
adapters (byte-wide, two ports each) so a guest programs the PIC through the bus. +5 tests
(port ranges, shared-line raise+route, cascade slave vector, mask read-back through the adapter,
masked line delivers nothing).

### Increment 3 — `DeviceBus::standard_pc_with_pic` (early-boot PIC config) (`9218e77`)
In `enlil-core`. `add_pic` mounts both PIC port pairs; `standard_pc_with_pic` assembles COM1 +
PIT + PCIe + the four PIC ports and wires PIT→`IRQ_PIT`(0) / UART→`IRQ_COM1`(4) into the
`SharedPic` — the PIC counterpart to `standard_pc_with_interrupts`. +2 `DeviceBus` tests driving
the real `VmExitHandler` path: a guest runs the PC/AT ICW sequence, then enabling the UART THRE
interrupt asserts IRQ4 (vector 0x24, consumed via INTA `acknowledge`); a masked line is held off
the CPU until unmasked through the bus.

### Increment 4 — `standard_pc_with_dual_irq` (tee to PIC + I/O APIC) (`f744de5`)
The transparent, hardware-accurate config: each legacy device line is **teed into both** the
8259 PIC and the I/O APIC (a plain `Fn(bool)+Send` closure calling both per-controller sinks),
mirroring real hardware where an ISA line is wired to both controllers and the OS leaves one
path masked — so the boot-time PIC→APIC switchover is seamless. Mounts both front-ends. +1
`DeviceBus` test: one IRQ4 line event latches on the 8259 (vector 0x24) **and** routes through
the I/O APIC RTE to LAPIC 0 (vector 0x34) simultaneously.

### Research (informed the build)
Logged under `RESEARCH.md` → "2026-06-07 (b)". The 8259A is ancient, stable silicon — no recent
paper changes it; the authoritative sources are the **Intel 8259A datasheet** (ICW1-4/OCW1-3
register model, fully-nested fixed priority, ICW1 clears IMR + edge-sense latch) and the
**PC/AT dual-PIC cascade** wiring (slave INT is a *level* input to master IR2, so modelling IR2
"computed on demand from the slave's deliverable state" avoids the classic cascade-bookkeeping
bugs and makes the two-INTA acknowledge natural). Transparency note: a reset PIC must read back
inertly and deliver nothing until the full ICW sequence + unmask. Rotating-priority OCW2
variants are accepted but treated as their non-rotating equivalent (PC OSes use fixed priority +
non-specific EOI), flagged for a later increment.

### Test results (exact)
- `cargo build --workspace` → **OK**.
- `cargo fmt --all -- --check` → **OK**.
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean). The new code is
  lint-clean under the crate's strict `deny(all, pedantic, nursery)` with **zero** new `#[allow]`:
  grouped the OCW3 bools into a `ReadState` sub-struct (struct-bool threshold), masked the
  `trailing_zeros() & 0x07` casts, used `truncate::u8_of` for the byte-wide port writes, and
  dropped an unused `icw3` field (cascade is fixed at IR2).
- `cargo test --workspace` → **717 passed, 0 failed, 1 ignored** (was 693). The 1 ignored is the
  `/dev/kvm` self-skip. New: 21 `interrupt::pic` unit tests + 3 `device_bus` tests.
- `/dev/kvm` paths (`serial_console_smoke`, `kvm_create_vm_and_map_memory`): **not run — no
  `/dev/kvm` (no nested virt)**; they self-skip.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **MADT interrupt-source-override (pure userspace, unblocked, small).** Both I/O APIC factories
   identity-map ISA IRQ → I/O APIC pin, but a PC's MADT remaps ISA IRQ0 → GSI 2 (and marks
   IRQ0's PIC line). `enlil_devices::acpi` emits the MADT; make the factory's pin wiring and the
   emitted MADT agree (timer on GSI 2), and add an override-aware `.line()`/RTE mapping. This is
   the last correctness gap flagged in the ROADMAP 0.2 status note and pairs directly with this
   session's work.
2. **8259 ELCR (edge/level control register, `0x4D0`/`0x4D1`), unblocked.** PCI interrupts are
   level-triggered through the PIC; the ELCR selects per-line edge vs level. Our PIC is
   edge-only. Add the ELCR port pair + level-triggered IRR semantics (a level line re-asserts
   after EOI while still high). Small, self-contained, pairs with PCI INTx routing.
3. **KVM binding (blocked on `/dev/kvm`).** When a nested-virt runner exists, bind `SharedPic` /
   `SharedInterruptController` delivery to the in-kernel chip (`KVM_CREATE_IRQCHIP` already models
   the dual-8259) or route PIC INTR via LAPIC LINT0 ExtINT, and have `KvmBackend` build its bus
   via `standard_pc_with_dual_irq`. Add a `/dev/kvm`-gated boot-takes-an-interrupt test. **If
   still no `/dev/kvm`, skip and do #1/#2.**

---

## 2026-06-07 — Session: device IRQ delivery — line wiring, assembled-bus wiring, I/O APIC MMIO (Phase 0.2)

A multi-increment session. Three complete, independently-green, tested commits on the branch,
each a full orient→build→verify trip, forming one coherent arc: **make device interrupts actually
reach a vCPU, and let a guest program the routing.** This took the 06-06(c) recommended next
step #1, path (a) — "wire the IRQ lines to delivery (native/userspace), no KVM needed". `/dev/kvm`
is **still absent on this runner (no nested virt)**, so every increment is the pure-userspace half,
fully exercised by unit/integration tests. Test count: 683 → **693**.

### Increment 1 — `SharedInterruptController` + `clear_irq` (`d1423ad`)
Both the PIT (`attach_irq0`) and UART (`attach_irq_line`) exposed level sinks (`IrqLine`) but
nothing consumed them — interrupts went nowhere. Added
`enlil_devices::interrupt::SharedInterruptController` (`Arc<Mutex<InterruptController>>`, `Send` to
match the `IrqLine` bound) with:
- `.line(irq) -> impl Fn(bool) + Send + use<>`: a level sink that calls `deliver_irq(irq)` on a
  rising edge and the new `InterruptController::clear_irq(irq)` on a falling edge. It satisfies
  **both** `IrqLine` traits (PIT's in `enlil-devices`, UART's in `enlil-core`) through their
  `Fn(bool)+Send` blanket impls — one wiring path, no cross-crate dep.
- `.with(|c| …)` for locked access (program RTEs, read pending vectors), `.new`/`.from_controller`.
- `InterruptController::clear_irq` forwards to `IoApic::clear_irq` (no-op for edge-triggered ISA
  lines, correct deassert for a level-triggered RTE).
- +6 tests (rising-edge delivery, PIT true/false pulse still latches one edge, masked RTE delivers
  nothing, separate lines hit their own vectors, clones share one controller, level-triggered
  clear-on-falling-edge).

### Increment 2 — `DeviceBus::standard_pc_with_interrupts` (`fec39ac`)
`standard_pc` assembled COM1+PIT+PCIe but wired no interrupts. Added a sibling factory taking a
`&SharedInterruptController` that attaches PIT channel-0 → `IRQ_PIT` (0) and COM1 → `IRQ_COM1` (4)
**before** boxing them into the bus (you can't reach a device's non-`PioDevice` methods once boxed).
The caller owns the PIC. New `IRQ_PIT`/`IRQ_COM1` constants. +2 `DeviceBus` tests driving the *real*
bus: enabling the UART THRE interrupt via an `io_out` exit raises IRQ4 into LAPIC 0's IRR; a masked
RTE delivers nothing.

### Increment 3 — `IoApicMmio` front-end (`940aaf2`)
The I/O APIC had `mmio_read/write` methods but wasn't mounted as a bus device, so a guest couldn't
program the redirection table — RTEs stayed masked and *no* device IRQ could ever be delivered.
Added `IoApicMmio` (`MmioDevice` at `0xFEC0_0000`, one 4 KiB page) forwarding `IOREGSEL`/`IOWIN` to
the shared controller's I/O APIC; `DeviceBus::add_ioapic`; and `standard_pc_with_interrupts` now
mounts it. The increment-2 UART test was upgraded to program IRQ4's RTE **through the MMIO aperture**
(2 dword writes, as a guest OS does) — closing the loop end-to-end. +2 `interrupt::line` tests.

### Test results (exact)
- `cargo build` (workspace) → **OK**.
- `cargo fmt --all -- --check` → **OK** (ran `cargo fmt --all`).
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; `interrupt::line` is in
  the strict-deny `enlil-devices` crate — used `truncate::u32_of` for the dword write and `const fn`
  on `IoApicMmio::new`, no new `#[allow]`).
- `cargo test --workspace` → **693 passed, 0 failed, 1 ignored** (was 683; +10 across the three
  increments). The 1 ignored is the `/dev/kvm` self-skip.
- `/dev/kvm` paths (`serial_console_smoke`, `kvm_create_vm_and_map_memory`): **not run — no
  `/dev/kvm` (no nested virt)**; they self-skip.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Design note (so tomorrow doesn't re-litigate it)
The **LAPIC is deliberately NOT a bus `MmioDevice`.** It sits at one physical address
(`0xFEE0_0000`) for *every* vCPU and an access implicitly targets the accessing vCPU's LAPIC; a
shared `MmioBus` has no "current vCPU" notion. LAPIC MMIO belongs in the per-vCPU exit path (or the
in-kernel chip via `KVM_CREATE_IRQCHIP`). Only the genuinely-shared I/O APIC is on the bus. Today's
tests therefore enable LAPIC 0 via `pic.with(|c| c.lapics[0].write_register(0x0F0, …))` (the
`LAPIC_SVR` const isn't re-exported from `enlil-devices`).

### Recommended next step (tomorrow)
1. **KVM binding (blocked on `/dev/kvm`):** when a nested-virt runner is available, bind
   `SharedInterruptController` delivery to the in-kernel chip via `vm.set_irq_line(irq, level)` /
   `irqfd`+`EventFd`, and have `KvmBackend` build its bus through `standard_pc_with_interrupts`
   instead of hand-mounting a lone serial port in `serial_console_smoke`. Add a `/dev/kvm`-gated
   test that boots a blob, takes a timer/serial interrupt, and observes the vector. **If still no
   `/dev/kvm`, skip and pick #2/#3.**
2. **Legacy 8259 PIC (pure userspace, unblocked):** early boot starts in PIC mode before the OS
   switches to the I/O APIC. We have no dual-8259 emulation (ICW1-4/OCW, IRR/ISR/IMR, cascade,
   EOI) at `0x20`/`0xA0`. This is a real early-boot gap and pairs with today's interrupt work, but
   it's a sizeable single increment — scope it carefully.
3. **CMOS/RTC (`0x70`/`0x71`, unblocked):** another device a guest touches very early (and a
   common anti-detection probe — must read plausible values). Smaller than the 8259.
4. **PIT hardening (Firecracker #2777)** was reconsidered and **deferred**: its "forbid re-creating
   channels after boot" doesn't map cleanly to our model (no separate channel-create op distinct
   from a control-word write; a running OS legitimately reloads counts). Revisit only with a precise
   threat model and a "boot complete" signal from the run loop.

A multi-increment session (3–4h budget). Four complete, independently-green, tested commits on
the branch, each one trip through orient→build→verify. `/dev/kvm` is **still absent on this
runner (no nested virt)**, so all KVM/guest-boot paths self-skip — every increment below is the
pure-userspace half and is fully exercised by unit/integration tests. Test count: 672 → **683**.

### Increment 1 — ECAM MMIO front-end over a shared root complex (`d764622`)
The 06-06(b) recommended next step. `enlil-devices::pcie` had the legacy CAM (`0xCF8`/`0xCFC`)
front-end but no MMIO ECAM, and CAM owned its `PcieRootComplex` by value so a second front-end
couldn't share the device set.
- Refactored `PciConfigIo` to hold `SharedRootComplex` (`Rc<RefCell<PcieRootComplex>>`); kept
  `new(PcieRootComplex)` (wraps) and added `with_shared` + `shared()`. Removed the `&`-returning
  `root()`/`root_mut()` (can't borrow through a `RefCell`) — two tests updated to `shared().borrow()`.
- Added `EcamSpace` (`MmioDevice`): forwards `mmio_read/write` to `ecam_read/ecam_write` over a
  256-bus / 256 MiB window (`ECAM_SEGMENT_SIZE = 256<<20`) at the root's `ecam_base`. ECAM offset
  == config offset, so no second B/D/F decode. 8-byte access combines two dwords.
- `DeviceBus::add_pcie(root) -> SharedRootComplex` mounts CAM (PIO) + ECAM (MMIO) over one root
  and seeds a default Intel 440FX host bridge at 0:0.0 if absent.
- **Research** (RESEARCH.md 2026-06-06(c)): OSDev PCIe / Linux acpi-info confirm
  `phys = ecam_base + ((bus<<20)|(dev<<15)|(fn<<12)|reg)`; cloud-hypervisor `PciConfigMmio` over a
  shared `PciBus` is the canonical shared-store shape. +6 tests.

### Increment 2 — PIT channel-0 IRQ0 edge sink (`8431951`)
Carried-over step #1's platform-independent half. Added an `IrqLine` trait (`set_level(bool)`,
blanket `impl` for `Fn(bool)+Send`) in `enlil-devices::timer::pit` — mirrors the UART's IRQ4 line
but defined in `enlil-devices` because it's the lower crate (can't depend on `enlil-core`).
`Pit::attach_irq0` stores a `Box<dyn IrqLine>`; `Pit::tick` pulses `true`/`false` once per
channel-0 terminal-count edge, so a level-driven `set_irq_line(0, level)` or
`InterruptController::deliver_irq(0)` sees exactly one edge. +2 tests. (Actual KVM/native delivery
wiring still needs `/dev/kvm`.)

### Increment 3 — bounded 16550 RX FIFO with hardware-accurate overrun (`525c191`)
Outstanding since 06-05 (vm-superio issue #17). `UartState::inject_input` was unbounded
(`rx_fifo.extend`) → a guest that never drains COM1 grows host memory without bound. Capped at
`RX_FIFO_CAPACITY = 4096`; on overflow the incoming byte is dropped and the LSR **Overrun Error**
bit (`0x02`, sticky, cleared on LSR read) is set — the real 16550 behaviour, more transparent than
silently dropping the earliest input, and earliest bytes are preserved. Reused the existing
`lsr_overrides` field. +2 tests.

### Increment 4 — `DeviceBus::standard_pc` factory (`913689c`)
No assembled bus existed (the KVM smoke test hand-built one). `standard_pc(serial_output)` mounts
COM1 UART + 8254 PIT + PCIe (CAM+ECAM+host bridge) at canonical fixed addresses over one shared
root, returning `(DeviceBus, SharedRootComplex)`. `DEFAULT_ECAM_BASE = 0xB000_0000` matches the
MCFG table `enlil-devices::acpi` emits. Ties increments 1–3 into something a backend can
instantiate directly; advances the "Linux to serial shell" milestone. +1 test.

### Test results (exact)
- `cargo build` (workspace) → **OK**.
- `cargo fmt --all -- --check` → **OK** (after `cargo fmt --all`).
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; `pcie`/`pit` are in the
  strict-deny `enlil-devices` crate — used `u32_of` for the ECAM 8-byte split, no new `#[allow]`).
- `cargo test --workspace` → **683 passed, 0 failed** (was 672; +11 across the four increments).
- `/dev/kvm` paths (`serial_console_smoke`, `kvm_create_vm_and_map_memory`): **not run — no
  `/dev/kvm` (no nested virt)**; they self-skip.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Wire the IRQ lines to delivery.** Both the PIT (`attach_irq0`) and UART (`attach_irq_line`)
   now expose level sinks but nothing consumes them. Two viable paths: (a) **native/userspace** —
   wire them to `enlil_devices::interrupt::InterruptController::deliver_irq(0/4)` (no KVM needed,
   fully testable here: a tick-driven test asserting IRQ0 reaches a LAPIC IRR); (b) **KVM** —
   `vm.set_irq_line(0/4, level)` / `irqfd`, **needs a `/dev/kvm` runner**. Prefer (a) — it's
   unblocked and exercises the in-userspace interrupt path the native-VMX backend will use.
2. **Have the KVM backend build its bus via `DeviceBus::standard_pc`** instead of hand-mounting a
   lone serial port in `serial_console_smoke`, so the guest-boot path gets PIT + PCI for free
   (run only on a KVM runner).
3. **PIT hardening (Firecracker #2777):** once IRQ0 delivery is live, forbid (re)programming PIT
   channels after guest boot and stop channel-0 when idle. Needs a "boot complete" signal from the
   run loop, so sequence it after step 1/2.

## 2026-06-06 (b) — Legacy PCI Configuration Mechanism #1 (0xCF8/0xCFC) on the bus (Phase 0.2)

The previous run's recommended next step #1 (channel-0 → IRQ0 + UART IRQ4 wiring to KVM) is
**still blocked — no `/dev/kvm` on this runner (no nested virt)**. Took step #2 instead, which
the roadmap explicitly names next ("Then PCI config (`0xCF8/0xCFC`)"): the legacy PCI
**Configuration Mechanism #1** port pair. Pure userspace, fully testable here, mirrors the
06-04 serial / 06-06 PIT bus-mounts.

### Situation found
`enlil-devices::pcie` had a complete `PcieRootComplex` with device storage and an **ECAM** MMIO
decode (`ecam_read`/`ecam_write`), plus `PciConfigSpace` (per-function config space with BAR
size-detection) — but **nothing implemented the legacy CAM PIO front-end**. A guest BIOS that
enumerates PCI via `0xCF8`/`0xCFC` *before* bringing up ECAM saw open-bus `0xFF` on those ports
→ no host bridge, no devices found. Neither the ECAM nor the CAM front-end was mounted on any
bus yet; this run wires the CAM half (the one a legacy/early-boot guest hits first).

### What I did (one increment: PCI Mechanism #1 becomes a real bus device)
- **`enlil_devices::pcie::PciConfigIo`** — a `PioDevice` over `0xCF8..=0xCFF` (constants
  `CONFIG_ADDRESS_PORT` / `CONFIG_DATA_PORT`) wrapping a `PcieRootComplex`. A write to the
  `CONFIG_ADDRESS` window (`0xCF8`-`0xCFB`) latches `config_address` (sub-dword writes merge in
  place via `write_address`); the enable bit (31) is honoured. Reads/writes of the `CONFIG_DATA`
  window (`0xCFC`-`0xCFF`) are **byte-steered by port offset** (`reg | (port - 0xCFC)`) and the
  latched B/D/F + register are folded back into an ECAM-style offset (`target_offset`) so decode
  **reuses `PcieRootComplex::ecam_read`/`ecam_write`** — a single shared config-space decode
  path with the future ECAM front-end. A disabled config cycle reads open-bus and drops writes.
- **`DeviceBus::add_pci_config_io`** in `enlil-core/src/device_bus.rs` (thin wrapper over
  `add_pio`, mirrors `add_pit`/`add_serial`).

### Research (informed the design) — logged in `RESEARCH.md` (2026-06-06, Mechanism #1 entry)
PCI Local Bus spec / OSDev "PCI" / Wikipedia "PCI configuration space" (CONFIG_ADDRESS layout,
enable bit, **byte-steering pitfall** — low two reg bits are always zero so sub-dword CONFIG_DATA
access is selected by port offset, masked/shifted in software; Mechanism #1 reaches only the
first 256 bytes). cloud-hypervisor `pci::PciConfigIo` + rust-hypervisor-firmware `src/pci.rs`
(canonical Rust reference: thin address-latching front-end over a shared config store; reuse one
decode path for CAM + ECAM).

### Test results (exact)
- `cargo build` (workspace) → **OK** (0 errors).
- `cargo fmt --all -- --check` → **OK** (after `cargo fmt --all`).
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; `pcie` is in the
  strict-deny `enlil-devices` crate — masked truncating casts match the existing `ecam_read`
  idiom, no new `#[allow]`).
- `cargo test --workspace` → **672 passed, 0 failed** (was 662; +10). New: 9 `pcie` unit tests
  (`config_address_latches_and_reads_back`, `byte_writes_to_config_address_merge_in_place`,
  `mechanism1_reads_vendor_and_device_id`, `mechanism1_subword_access_steers_by_data_port_offset`,
  `disabled_config_cycle_reads_open_bus`, `mechanism1_absent_device_reads_all_ones`,
  `mechanism1_write_reaches_config_space`, `write_disabled_config_cycle_is_dropped`,
  `port_range_claims_the_eight_legacy_cam_ports`) + 1 `DeviceBus` integration test
  (`guest_enumerates_pci_through_cf8_cfc_on_the_bus`).
- `/dev/kvm` paths: **not run — no `/dev/kvm` (no nested virt)**; they self-skip.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Mount ECAM on the MMIO bus** over the *same* `PcieRootComplex` the CAM front-end wraps.
   This needs shared ownership of the root complex (the CAM `PciConfigIo` currently owns it by
   value) — wrap it in `Rc<RefCell<PcieRootComplex>>` (or an `Arc<Mutex>` if it must cross
   threads) and give both `PciConfigIo` and a new `EcamSpace` MMIO device handles, so CAM and
   ECAM mutate one device set. Add a host bridge (`create_host_bridge`) at BDF 0:0.0 by default
   so a guest finds *something* at boot. Pure userspace, fully testable here.
2. **Channel-0 → IRQ0 + UART IRQ4 wiring to KVM** (carried over, still blocked). Give `Pit` an
   `IrqLine` sink pulsed from `Pit::tick`; wire both it and the UART `IrqLine` to KVM via
   `set_irq_line(0/4, level)` / `irqfd`. **Needs a `/dev/kvm`-capable runner** — if still
   unavailable, do the platform-independent half (PIT `IrqLine` + a `tick`-driven edge test).
3. **Hardening (Firecracker #2777):** once IRQ0 is live, forbid (re)programming PIT channels
   after guest boot, and stop channel-0 when idle.

## 2026-06-06 — 8254 PIT on the device bus + read-back command (Phase 0.2)

The previous run's recommended next step was (1) wire the UART `IrqLine` to KVM — **blocked,
no `/dev/kvm` on this runner (no nested virt)** — or (2) the next early-boot PIO devices,
**PIT (`0x40-0x43`)** and PCI config (`0xCF8/0xCFC`). Picked (2)'s PIT half: it's pure
userspace, fully testable here, and mirrors the 06-04 serial bus-mount.

### Situation found
`enlil-devices::timer::Pit` was a complete-ish i8254 emulation (3 channels, modes 0-3 ticking,
single-channel latch, `channel0_frequency`) **but it implemented none of the bus traits** —
nothing called `PioDevice`, so a guest touching `0x40-0x43` got open-bus `0xFF` (a dead
giveaway / breaks early timer calibration). It also **ignored the read-back command**
(`write_command` returned early for `channel_idx == 3`), and had no null-count status bit.

### What I did (one increment: the PIT becomes a real, more-transparent bus device)
- **`impl PioDevice for Pit`** in `enlil-devices/src/timer/pit.rs` over `0x40..=0x43`
  (`PIT_PORT_BASE`/`PIT_PORT_COUNT` constants): byte-wide reads return the low byte of the
  `u32`, writes consume the low byte. The bus passes the absolute port, which `read_port`/
  `write_port` already decode (`port & 0x3`, `0x43` = command).
- **Read-back command** (`Pit::read_back`): control word bits 7-6 = `11` → `/COUNT` (bit 5
  low) latches each *selected* channel's current count, `/STATUS` (bit 4 low) latches a status
  byte; bits 3-1 select channels 2/1/0. A pending latch isn't overwritten by a second
  read-back (datasheet). `read_data` now delivers a latched **status byte before** any latched
  count.
- **Status byte + `null_count`**: added `CounterStatus { output, null_count }` (a sub-struct so
  `PitChannel` stays ≤ 3 bools — same pattern as the existing `ByteLatch`, avoids the
  zero-`#[allow]` `struct_excessive_bools` deny). `null_count` is set on a control-word write
  and cleared on count load; `status_byte()` packs OUT-pin/null-count/RW-access/mode/BCD per
  the 8254 datasheet. Added `ChannelMode::bits()` / `AccessMode::bits()` for clean enum→field
  encoding (no `as` casts).
- **`DeviceBus::add_pit(Pit)`** in `enlil-core/src/device_bus.rs` (thin wrapper over `add_pio`,
  mirrors `add_serial`).

### Research (informed the design) — logged in `RESEARCH.md` (2026-06-06)
Intel 8254 datasheet / OSDev PIT (read-back command + status-byte layout), QEMU/Linux
`i8254.c` + `KVM_CREATE_PIT2` (KVM normally emulates the PIT **in-kernel**, so it bypasses our
userspace bus — but Enlil's own native-VMX backend will have no in-kernel chip and **must**
carry a userspace PIT, so this is correct), and Firecracker #2777 (hardening: forbid creating
PIT channels after boot; channel-0 left running costs steal time).

### Test results (exact)
- `cargo build` (workspace) → **OK** (0 errors).
- `cargo fmt --all -- --check` → **OK**.
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; PIT is in the
  strict-deny `enlil-devices` crate; the `CounterStatus` extraction keeps it under the
  `struct_excessive_bools` threshold with no `#[allow]`).
- `cargo test --workspace` → **662 passed, 0 failed** (was 654; +8). New: 6 PIT unit tests
  (`test_pio_device_claims_four_command_ports`, `test_pio_write_programs_channel_low_byte_only`,
  `test_pio_read_returns_count_in_low_byte`, `test_read_back_latches_status_before_count`,
  `test_read_back_null_count_set_before_load`, `test_read_back_only_selected_channels`) + 2
  `DeviceBus` integration tests (`pit_on_the_bus_programs_and_reads_back_a_channel_count`,
  `pit_and_serial_coexist_on_the_pio_bus`).
- `/dev/kvm` paths (`serial_console_smoke`, `kvm_create_vm_and_map_memory`): **not run — no
  `/dev/kvm` (no nested virt)**; they self-skip. No PIT IRQ delivery was exercised (none was
  written this run).
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Channel-0 → IRQ0 + UART IRQ4 wiring.** Give `Pit` an `IrqLine`-style sink (mirror the
   UART's `attach_irq_line`) pulsed from `Pit::tick` when channel 0 reaches terminal count,
   and wire both it and the existing UART `IrqLine` to KVM via `vm.set_irq_line(0/4, level)`
   through the in-kernel IRQ chip (or `irqfd`/`EventFd`). Add a `/dev/kvm`-gated test. **Needs
   a KVM-capable runner** — if still unavailable, do the platform-independent half (the PIT
   `IrqLine` + a `tick`-driven edge test) and leave the `set_irq_line` call for the KVM runner.
2. **PCI config space (`0xCF8/0xCFC`)** as a `PioDevice` — the address/data port pair a guest
   uses to enumerate the PCI bus early in boot. Pure userspace, fully testable here.
3. **Hardening (Firecracker #2777):** once IRQ0 is live, forbid (re)programming PIT channels
   after guest boot, and stop channel-0 when idle to avoid steal time.

## 2026-06-05 — Interrupt-driven 16550: IER-honoured IIR + pluggable IRQ4 line (Phase 0.2)

The previous run's recommended next step: give `SerialPort` an IRQ4 sink fired on
RX-available / THR-empty, honouring IER and producing the correct IIR identification byte.
Done — the platform-independent half (fully testable here); the KVM `set_irq_line` wiring is
now the single remaining sub-task (Linux/`/dev/kvm`-only).

### Situation found
`UartState` was polled-only: it stored `ier`/`mcr` and a constant `iir = IIR_NO_INTERRUPT`
field that `read_register(IIR_REG)` returned verbatim — it never reflected pending sources and
never signalled an IRQ. Linux's 8250 driver can run polled (so the 06-04 smoke test works),
but its **default is interrupt-driven**, so a real serial shell needs IRQ4. No `UartState`
register field is referenced outside `serial.rs`, so the IIR could be made computed safely.

### What I did (one increment: the UART interrupt model + IRQ-line sink)
- **Computed IIR honouring IER** (`compute_iir`): highest-priority *enabled and pending*
  source — RX-available (`0x04`) outranks THR-empty (`0x02`); `0x01` = none. Removed the
  stored `iir` field.
- **THR-empty latch** (`thr_empty_pending`): set when ETBEI transitions on (THR is always
  empty in emulation) and re-armed on every TX (`write_data`); **cleared by an IIR read**
  when THRE is the reported source (per PC16550D / the linux-serial IIR/LSR-ordering pitfall).
  RX-available is level-derived from the FIFO + ERBFI (cleared by draining RBR, *not* by an
  IIR read).
- **`IrqLine` trait + pluggable sink:** `UartState::set_irq_line` / `SerialPort::attach_irq_line`
  store a `Box<dyn IrqLine>`; `update_irq` recomputes the level after *every* state-changing
  access (register read/write, `inject_input`, `read_byte`) and calls `set_level` only on a
  real edge — matching QEMU's `qemu_set_irq` and vm-superio's eventfd `Trigger`. Blanket
  `impl IrqLine for Fn(bool) + Send` so the KVM backend can wire it with a closure
  (`move |level| vm.set_irq_line(4, level)`). `interrupt_pending()` exposes the level for a
  poll-after-exit driver that doesn't use the callback.
- **IER/IIR bit constants** added (`IER_RX_AVAILABLE`/`IER_THR_EMPTY`/…, `IIR_THR_EMPTY`/
  `IIR_RX_AVAILABLE`). IIR FIFO bits 6-7 deliberately left clear (FCR unemulated, like
  vm-superio) so the guest probes us as a plain 8250 — works polled *or* interrupt-driven.

### Research (informed the design) — logged in `RESEARCH.md` (Part III, 2026-06-05)
rust-vmm **`vm-superio` `Serial`** + **PC16550D datasheet**: confirmed the IIR priority,
THRE-acknowledge-on-IIR-read semantics, and the `Trigger`-style level sink. **Pitfall**
(linux-serial "IIR/LSR out-of-sync"): the THRE latch must be re-evaluated relative to read
ordering — `update_irq` runs *after* each access mutation so the asserted level always matches
the computed IIR.

### Test results (exact)
- `cargo fmt --all -- --check` → **OK**
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean)
- `cargo test --workspace` → **654 passed, 0 failed** (was 647; +7). `enlil-core`: 122 (was 115).
  New: `test_no_interrupt_when_sources_disabled`, `test_rx_available_interrupt`,
  `test_thr_empty_interrupt_set_and_acked_by_iir_read`, `test_rx_outranks_thr_empty_in_iir`,
  `test_irq_line_edges_on_rx`, `test_irq_line_accepts_a_closure`,
  `test_serial_port_drives_irq_line`.
- `serial_console_smoke` + `kvm_create_vm_and_map_memory`: **self-skipped — `/dev/kvm` not
  present (no nested virt).** The interrupt logic added this run is pure userspace and is
  fully exercised; the KVM IRQ delivery path is not yet written.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Wire `IrqLine` to KVM.** In `KvmBackend`, after each `run_vcpu` (or via an `irqfd`/
   `EventFd` registered with `KVM_IRQFD`), assert/deassert IRQ4 through the in-kernel IRQ chip
   with `vm.set_irq_line(4, level)`. The simplest first cut: a poll-after-exit driver that
   calls `serial.interrupt_pending()` and `set_irq_line(4, that)`. Add a `/dev/kvm`-gated test
   that boots a blob enabling ETBEI/ERBFI and verifies an IRQ is taken (needs a KVM runner).
2. Then the next early-boot PIO devices: **PIT** (`0x40-0x43`) and **PCI config**
   (`0xCF8/0xCFC`) as `PioDevice`s on the bus.
3. Cap `UartState`'s RX `VecDeque` (drop-oldest) before wiring a real host-stdin source
   (vm-superio issue #17) — still outstanding.

---

## 2026-06-04 — 16550 UART on the device bus + real-mode KVM serial smoke test (Phase 0.2)

The previous run's recommended next step: register the 16550 UART as a `PioDevice` at
COM1 and add a `/dev/kvm`-gated test that boots a tiny blob writing to COM1. Done.

### Situation found
`enlil-core::serial` had a complete `UartState` (16550 register file: IER/IIR/LCR/LSR/
MCR/MSR/SCR + DLAB divisor, RX FIFO, pluggable `SerialOutput` sink) and a
`SerialMultiplexer`, but **nothing implemented `enlil_devices::bus::PioDevice`**, so the
UART could not be mounted on the real `DeviceBus`/`VmExitHandler` path landed on 06-03.
The `device_bus` tests used a hand-rolled `FakeSerial`, never the real UART.

### What I did (one increment: the UART becomes a real bus device, verified end-to-end)
- **`enlil-core::serial::SerialPort`** — a new `PioDevice` adapter bundling a `UartState`
  with its COM base port, claiming `[base, base+8)`. `pio_read/pio_write` map the absolute
  guest port to the 0–7 register offset (`port - base`) and forward to the UART; serial
  registers are byte-wide so writes take the low byte. `com1()` convenience ctor.
- **`DeviceBus::add_serial(SerialPort)`** helper (thin wrapper over `add_pio`).
- **`SerialOutputMode::Shared(Arc<Mutex<Vec<u8>>>)`** — a new sink mode that appends TX
  to a *caller-owned* buffer, so guest serial output stays observable after the device is
  moved into the bus (the host console/logger reads the same `Arc`). This is what made a
  real end-to-end assertion possible, and is the primitive the console/display path needs.
- **`KvmBackend::prepare_real_mode_vcpu(index, entry)`** — minimal flat 16-bit real-mode
  setup (all segment bases 0, `rip=entry`, `rflags=0x2`) so a small real-mode blob can run.
  Genuinely useful primitive (compiled, not executed here — no `/dev/kvm`).
- **Tests:** 5 `SerialPort` unit tests (range claim, port→offset mapping, TX to sink, LSR
  TX-ready, RX injection round-trip), 1 `Shared`-mode test, 1 `real_serial_uart_on_the_bus`
  (drives the real UART through the real `VmExitHandler` and asserts "OK" reached the
  shared sink — runs here, no KVM), and 1 `/dev/kvm`-gated **`serial_console_smoke`**:
  assembles `mov dx,0x3F8; mov al,'O'; out; mov al,'K'; out; hlt`, maps it at gphys 0x1000,
  runs it via `KvmBackend::run_vcpu` with the `DeviceBus` handler, and asserts the guest
  reached HLT and "OK" landed in the sink.

### Research (informed the design) — logged in `enlil-research-review.md` (2026-06-04)
rust-vmm **`vm-superio` `Serial`**: confirms our `UartState` register set is complete
(same registers; FCR need not be emulated — FIFO is always-on). **Pitfall surfaced:**
vm-superio raises RX/THR interrupts via a `Trigger`/eventfd; our UART is **polled-only**
(stores IER, never raises IRQ4). Linux's 8250 driver can run polled so a shell works, but
interrupt-driven mode needs IRQ4 into the in-kernel IRQ chip — logged as the next step.
Also noted vm-superio issue #17 (unbounded RX) → cap `inject_input` before wiring host stdin.

### Test results (exact)
- `cargo fmt --all -- --check` → **OK**
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean)
- `cargo test --workspace` → **647 passed, 0 failed** (was 639; +8). `enlil-core`: 115.
- `serial_console_smoke` + `kvm_create_vm_and_map_memory`: **self-skipped — `/dev/kvm` not
  present on this runner (no nested virt).** `prepare_real_mode_vcpu` and the run loop are
  compiled but NOT executed here; needs a KVM-capable runner to actually exercise.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet.

### Recommended next step (tomorrow)
1. **Interrupt-driven serial:** give `SerialPort` an IRQ4 sink (an `EventFd` raised via
   `KVM_IRQ_LINE` through the in-kernel IRQ chip) fired on RX-available / THR-empty,
   honoring IER and producing the correct IIR identification byte. This is what a default
   Linux 8250 driver expects and unblocks an actual interactive serial shell. The polled
   path already works (this run), so this is the next transparency increment.
2. Then the next early-boot PIO devices: **PIT** (`0x40-0x43`) and **PCI config**
   (`0xCF8/0xCFC`) as `PioDevice`s on the bus.
3. Cap `UartState`'s RX `VecDeque` (drop-oldest) before wiring a real host-stdin source
   (vm-superio issue #17).

---

## 2026-06-03 — Real device bus + KVM `VmExitHandler` bridge (Phase 2 / 0.2)

The previous run's recommended next step: give guest exits somewhere to go. Done —
the device bus is now a real dispatcher and the KVM backend is wired to it.

### Situation found
`enlil-devices::bus` was an **unused stub**: `PioBus`/`MmioBus` mapped a base address
to a device *index* (`BTreeMap<addr, usize>`) with **no owned devices, no dispatch, and
no upper range bound** — so `lookup(port)` returned the nearest-lower base even for a
port far past that device. `PioDevice`/`MmioDevice` traits existed but had zero
implementors or callers. `enlil-core`'s `kvm_backend::VmExitHandler` had nothing behind
it (`exit_handler.rs` is dead stubs). `enlil-core` did not depend on `enlil-devices`,
though `ARCHITECTURE.md` specifies `enlil-core ──► enlil-devices` and Phase 2 = "PIO/MMIO
exit handling in enlil-core".

### What I did (one increment: the PIO/MMIO dispatch path)
- **`enlil-devices/src/bus.rs` rewritten** into a real dispatching bus:
  - `PioBus`/`MmioBus` now **own** `Box<dyn PioDevice/MmioDevice>` keyed by base in a
    `BTreeMap`, registered over an explicit half-open range `[base, end)`. `register`
    **rejects empty ranges and overlaps** (checks the predecessor's `end` and the
    successor's `base`) → `Result<(), BusError>`.
  - Lookup finds the greatest base `<= addr` **and confirms `addr < end`** (fixes the
    stub's missing upper bound).
  - Byte-slice front door (`read(addr, &mut [u8])` / `write(addr, &[u8])`) matching the
    KVM exit / `VmExitHandler` shape, with little-endian width conversion to the typed
    `pio_read(port,size)->u32` / `mmio_read(offset,size)->u64` device methods. PIO passes
    the absolute port; MMIO passes `addr - base` as the offset.
  - **x86 open-bus semantics** for unmapped addresses: reads fill `0xFF`, writes dropped.
- **`enlil-core` ⇒ depends on `enlil-devices`** (architecturally-sanctioned edge) and a
  new **`enlil-core/src/device_bus.rs`**: `DeviceBus { pio, mmio }` with `add_pio`/
  `add_mmio` helpers and **`impl VmExitHandler for DeviceBus`** forwarding `io_in/io_out/
  mmio_read/mmio_write` straight to the bus — one bus, one decode path (rust-vmm
  `IoManager` model; see research note 2026-06-03).
- **Repo hygiene:** removed three stray `enlil-core/src/memory_dump_{1,2,3}.txt` dumps of
  `memory.rs` (junk from the `1f00b52` commit; not Rust, not referenced).

### Research (informed the design) — logged in `enlil-research-review.md` (2026-06-03)
rust-vmm `vm-device` `IoManager`/`PioManager`+`MmioManager` and Dragonball `dbs-device`
both register devices over an **address range** and route by the range that *contains* the
access — confirming the range-keyed, overlap-rejecting bus and the byte-slice/open-bus
front door over typed device accessors.

### Test results (exact)
- `cargo fmt --all -- --check` → **OK**
- `cargo clippy --all-targets --workspace -- -D warnings` → **OK** (clean; bus.rs is in
  the strict-deny `enlil-devices` crate — used edition-2024 let-chains to satisfy
  `collapsible_if`).
- `cargo test --workspace` → **639 passed, 0 failed** (was 630; +6 `bus` tests, +3
  `device_bus` bridge tests). New tests cover: PIO/MMIO read LE width, write reaching the
  owning device at the right port/offset/width (verified via `Rc<RefCell>` shared logs),
  open-bus on unmapped, overlap + empty-range rejection, and the `VmExitHandler` bridge
  end-to-end (a fake COM1 capturing guest bytes; MMIO offset routing).
- KVM `/dev/kvm` path: **not run (no nested virt on this runner)** — unchanged; this
  increment is pure userspace dispatch and needs no KVM.
- no_std custom-target build: **N/A** — no crate is `#![no_std]` yet (`bus.rs` uses
  `std::collections::BTreeMap`).

### Recommended next step (tomorrow)
1. **Register a real device on the bus.** Wrap the 16550 UART in `enlil-core::serial`
   (or a thin `enlil-devices` adapter) as a `PioDevice` at COM1 `0x3F8..0x400` and add it
   to `DeviceBus` so guest serial writes reach the console. Then a `/dev/kvm`-gated
   integration test that loads a tiny real-mode code blob which writes a byte to `0x3F8`
   and `hlt`s, runs it via `KvmBackend::run_vcpu` with the `DeviceBus` handler, and asserts
   the byte arrived — this finally exercises the full exit→device path and advances the
   Phase 0.2 "Linux to serial shell" milestone. (Needs a KVM-capable runner to actually
   run; otherwise it self-skips like the existing `kvm_create_vm_and_map_memory`.)
2. After serial: PIT (`0x40-0x43`) and the PCI config-space ports (`0xCF8/0xCFC`) are the
   next PIO devices a guest touches early in boot.

---

## 2026-06-02 (later still) — Zero `#[allow]`, strict clippy, all lints fixed for real

Per request: removed **every** `#[allow]` attribute (120 of them), kept the strict
`#![deny(clippy::all, clippy::pedantic, clippy::nursery)]`, and fixed every lint —
and every cascade lint the fixes produced — instead of suppressing.

- **Register/byte casts (~134):** added a small `enlil-devices::truncate` module
  (`Widen` trait + `u8_of`/`u16_of`/`u32_of`/`usize_of`, little-endian byte
  reconstruction — no lossy `as`, no panic) and rewrote intentional truncations to
  use it; signed TSC math uses `i64::cast_signed`/`u64::cast_unsigned`. (Renamed the
  trait method off `widen` to `to_u64` to dodge a future-std name collision.)
- **dead_code (~25):** re-exported genuinely-public-but-unreferenced API
  (`LoopbackBackend`, `PipeBackend`, `MsiCapability`, `IOAPIC_BASE`, `DeviceStatus`,
  `ClipboardEntry`), added accessors for stored-but-unread config fields, removed
  truly-unused private constants.
- **docs:** added accurate `# Panics`/`# Errors` sections (lock poisoning, asserts)
  and removed infallible `unwrap`s (`Trb::from_bytes` via explicit byte arrays).
- **structure:** `unused_self` → associated fns; `option_if_let_else` → `map_or`;
  `match_same_arms` combined; `unnecessary_wraps` (PS/2 `handle_data_byte` → `u8`);
  `significant_drop_tightening` (scoped lock guards); `vec_init_then_push` → `vec![]`;
  split two `too_many_lines` fns (`build_acpi_tables`, `CpuidStealthTable::build`);
  refactored two `struct_excessive_bools` (`PitChannel`→`ByteLatch`, `RedirectionEntry`
  →`RteFlags` sub-structs).

**Verified (exact CI commands):** `cargo fmt --all -- --check` OK · `cargo clippy
--all-targets --workspace -- -D warnings` OK · `cargo test --workspace` **630 passed,
0 failed**. `#[allow]` count in `*/src`: **0**. Strict deny groups still in place.

---

## 2026-06-02 (later) — Lint/format cleanup: CI "Check & Lint" is now green

Followed up on the build-fix PR by paying down the fmt + clippy debt that had
accumulated while the workspace was unbuildable (so CI had never passed).

- **`cargo fmt --all`** across the repo (87 files; formatting only).
- **Clippy under `deny(all, pedantic, nursery)`**: auto-fix pass + manual fixes.
  - `enlil-config/platform/hal/std/core`: fully fixed (no-op waker → `Waker::noop()`,
    slice instead of `&mut Vec`, iterator instead of indexed loop, `#[allow(dead_code)]`
    with rationale on in-progress Phase-5 table-synthesis scaffolding).
  - `enlil-devices` (the deep hardware-emulation crate): fixed the substantive lints
    (`semicolon_if_nothing_returned`, `unused_must_use` on an ignored `RoutingDecision`,
    `overly_complex_bool_expr`, `manual_clamp`, `manual_checked_ops`,
    `decimal_bitwise_operands`, `missing_const_for_fn` ×8, literal grouping,
    `items_after_statements`, unused vars/parens/mut, `dead_code` spec constants), and
    added a **curated, documented `#![allow(...)]`** for the pedantic/nursery lints that
    are domain-noise for register/byte code (`cast_possible_truncation`, `similar_names`,
    `unused_self`, `missing_panics_doc`, `match_same_arms`, `option_if_let_else`, …),
    consistent with the crate's existing `module_name_repetitions` allow. The strict
    `deny` groups stay in place so new code is still linted.
  - **Important footgun fixed:** the earlier `cargo clippy --fix` had stripped two
    test-only imports (`UsbDeviceClass`, `TrbType`), breaking the `enlil-devices` test
    build; restored them as test-scoped `use`s.
- **All three CI steps now pass locally** (the exact commands from `.github/workflows/ci.yml`):
  - `cargo fmt --all -- --check` → OK
  - `cargo clippy --all-targets --workspace -- -D warnings` → OK
  - `cargo test --workspace` → **629 passed, 0 failed**
- KVM `/dev/kvm` integration test still self-skips (no nested virt on this runner).

**Net:** PR #1 now contains the build fix **and** a green CI. The over-strict-but-never-
enforced lint policy was recalibrated to an enforceable one rather than churning 100+
register-code sites. Next step unchanged: wire the KVM backend into the run loop by
implementing `VmExitHandler` for `enlil-devices::bus::Bus`.

---

## 2026-06-02 — Restore buildable workspace + reconstruct KVM backend (Phase 0.2 / 5)

**Branch:** `claude/dazzling-heisenberg-quAdo` (harness policy requires a feature
branch + PR; DAILY-ROUTINE's "commit to master" was overridden by that policy).

### Situation found
The previous commit (`1f00b52 "Add KVM backend and Windows guest support"`) left
**`master` unbuildable**. `master` and the feature branch were identical. Root causes:

1. **`enlil-core/src/kvm_backend.rs` was corrupt** — it contained only the 80-byte
   string `[content omitted from context — 27455 bytes were written successfully to
   disk]` (a tool placeholder). The real ~27 KB of KVM code was *never committed*
   (git history confirms the blob was 80 bytes at the only commit that touched it).
   The em-dash in that placeholder was the `unknown start of token` compile error.
2. **`enlil-core/src/affinity.rs`** — `get_current_affinity()` was `const fn` but
   called a non-const function (`E0015`).
3. **`enlil-devices/src/net/tap.rs`** — imported a non-existent trait
   `NetworkBackend` (real name `NetBackend`), was missing the trait's
   `has_pending_rx` method, and used `libc` without it being a dependency.

### What I did (one increment: get master green again + a real KVM backend)
- **Reconstructed `kvm_backend.rs`** from scratch as a real, compiling backend:
  - Platform-independent exit model: `GuestExit` enum (+ `outcome()` →
    `RunOutcome::{Continue,Stopped}`) and a `VmExitHandler` trait
    (`io_in/io_out/mmio_read/mmio_write`) with a `RecordingHandler` for tests.
  - Linux/KVM impl (`#[cfg(target_os = "linux")]`): `KvmBackend::{new, map_memory,
    create_vcpu, run_vcpu, …}` over `kvm-ioctls`. `new()` opens `/dev/kvm`, checks
    API v12, creates the VM, sets the x86 TSS address, and creates the in-kernel
    IRQ chip. `run_vcpu()` translates every `VcpuExit` into a `GuestExit`,
    dispatching data transfer through the handler. `is_kvm_available()` gates tests.
- **Fixed `affinity.rs`**: dropped the erroneous `const`, removed an unused import.
- **Fixed `tap.rs`**: corrected the trait name, implemented `has_pending_rx` via
  non-blocking `poll(2)`, added `libc` to `enlil-devices` (linux deps), and cleaned
  the file's clippy issues in the code I touched (c-string literal instead of a
  fallible `CString::new().unwrap()`, inlined format args, `#[must_use]`, `# Errors`
  docs, lossless casts).
- **Repo hygiene**: removed ~54 tracked throwaway files from the repo root
  (`fix_*.ps1`, `count*.py`, `*_out.txt`, `kvm_parts/`, `assemble_kvm.py`, etc. —
  the prior run's failed piecewise-assembly scaffolding, with hardcoded `D:\` paths)
  and added `/scratch` to `.gitignore`.

### Research (informed the design)
Logged under `enlil-research-review.md` → "2026-06-02":
- rust-vmm **`vm-device` `IoManager`** uses exactly `pio/mmio _read/_write` dispatch
  → validates our `VmExitHandler` shape. Next integration must implement
  `VmExitHandler` *for* `enlil-devices::bus::Bus`, not a parallel decode path.
- `kvm-ioctls` `VcpuExit`: read exits' slices must be filled *before* the next
  `KVM_RUN` → our `run_vcpu` fills in place before returning the owned summary.
- `KVM_SET_USER_MEMORY_REGION2` + `guest_memfd` (2025) is for confidential VMs and
  is **incompatible** with host-side memory introspection; we deliberately use the
  classic `set_user_memory_region` so we can synthesise/inject ACPI/SMBIOS. The
  region2 path is a Phase 8/CVM concern only.

### Test results (exact)
- `cargo build` (whole workspace): **0 errors** (pre-existing dead-code warnings
  remain in untouched files: `smbios.rs`, `vtpm.rs`, `ps2/`, `usb/xhci/`, …).
- `cargo test` (whole workspace): **629 passed, 0 failed**, 17 test binaries.
- New `kvm_backend` tests: 5 passed (`terminal_exits_stop_the_loop`,
  `resumable_exits_continue_the_loop`, `recording_handler_captures_writes_and_read_sizes`,
  `default_handler_is_a_noop`, `kvm_create_vm_and_map_memory`).
- **`kvm_create_vm_and_map_memory` self-skipped: `/dev/kvm` is NOT present on this
  runner (no nested virt).** The real VM-creation / memory-map / vCPU path is
  compiled but **NOT executed** here — it prints "skipping" and returns. Not a
  fabricated pass; needs a KVM-capable runner to actually exercise.
- No_std custom-target build (`x86_64-unknown-enlil.json`): **N/A** — no crate is
  currently `#![no_std]`, so there is nothing to build for that target yet.

### Known issue for a future run (do NOT claim "complete" until addressed)
- **Workspace-wide clippy is NOT green.** Because the build had been broken,
  `cargo clippy` never ran across the tree; with `#![deny(clippy::all, pedantic,
  nursery)]` there are now **~200+ pre-existing lint errors** unmasked across
  `enlil-core` (`smbios.rs`, `vtpm.rs`, `acpi.rs`, …) and `enlil-devices`
  (`acpi/`, `usb/`, `ps2/`, `net/backend.rs`, …): mostly `missing_const_for_fn`,
  `doc_markdown` (backticks), `must_use_candidate`, `# Errors`/`# Panics` docs, and
  `cast_possible_truncation`. **My new/edited code is clippy-clean**; the debt is
  pre-existing. This is large and mechanical — do it crate-by-crate as its own
  increment(s), not in one shot.

### Recommended next step (tomorrow)
1. **Best next increment:** wire the KVM backend into the run loop — implement
   `VmExitHandler` for `enlil-devices::bus::Bus` (or a thin adapter) and replace
   the stub `enlil-core::exit_handler` functions, so serial/MMIO exits reach real
   devices. Add a `#[cfg]`-gated integration test that boots a tiny code blob to a
   `HLT` when `/dev/kvm` is available. This is the natural continuation and unblocks
   the Phase 0.2 "Linux to serial shell" milestone.
2. **Or, lower-risk:** start paying down the clippy debt one crate at a time
   (`enlil-core` first) so `cargo clippy` can become a real gate.
3. Consider asking for a KVM-enabled runner so the guest-boot path can actually run.

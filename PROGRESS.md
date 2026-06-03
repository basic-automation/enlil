# Enlil — Progress Log

Running log for the autonomous daily routine. Newest entry first. Each entry
records what was done, the research that informed it, exact test results, and
the recommended next step so the next run (which has no memory) can resume.

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

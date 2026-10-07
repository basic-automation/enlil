# KVM-to-bare-metal migration plan

Enlil develops on Linux+KVM and ships on bare metal. The two are not ports of
each other: they are the same crates compiled against different
`enlil-platform` feature flags and different `HypervisorBackend`
implementations. This chapter describes the migration as a sequence of
deliberate, verifiable steps rather than a flag day.

## Phase 0–5: hosted development (KVM)

- **Backend:** KVM via `kvm-ioctls`, gated to `cfg(target_os = "linux")` in
  `enlil-core`. The Linux host provides scheduling, memory management, and
  device I/O; Enlil implements the VMM logic (vCPU loop, exit handling,
  device emulation, memory partitioning).
- **Platform:** `platform-linux` — std-delegating implementations, so the full
  test suite runs on an ordinary dev machine.
- **Verification:** `cargo test --workspace` on Linux; KVM-dependent tests
  self-skip when `/dev/kvm` is absent (see [Testing](testing.md)). Guest-boot
  integration tests additionally need nested virtualization.

## Phase 6: bare-metal production

- **Backend:** native VMX (Intel) / SVM (AMD) `impl HypervisorBackend`,
  selected by target architecture, not by feature flag. The `enlil-boot` crate
  already proves the path: it enables AMD SVM and runs guests through a real
  `#VMEXIT` dispatch loop under QEMU+OVMF.
- **Platform:** `platform-baremetal` — raw-hardware implementations. The
  `memory`, `sync`, `threading`, and `time` modules already cross-compile for
  `x86_64-unknown-enlil`; `io` and `async_rt` are the remaining hosted-only
  modules (they need a `no_std` `IoError` type and a `BTreeMap`-based reactor).
- **Target:** the custom `x86_64-unknown-enlil.json` target spec with
  `-Z build-std`, recompiling `std`/`core`/`alloc` against the Enlil platform
  layer.

## The migration, step by step

1. **Complete the `no_std` platform surface.** Land the `no_std` `IoError` and
   the interrupt/IPI-driven reactor so `io` and `async_rt` cross-compile for
   `x86_64-unknown-enlil`. Everything above the platform then builds for bare
   metal unchanged.
2. **Wire the HAL edges.** Route `enlil-core`'s remaining direct KVM-ioctl and
   device-library call sites through `enlil-hal`'s `HypervisorBackend` trait,
   so the core never names KVM, VMCS, or EPT directly.
3. **Add the native backend.** Implement `HypervisorBackend` for VMX (and keep
   the SVM path proven by `enlil-boot`), behind `#[cfg(target_arch = …)]`.
   The KVM backend stays as the dev/test path — it is not deleted.
4. **Boot the full core on bare metal.** `enlil-boot`'s `kernel_entry` brings
   up memory, SMP, timers, and PCI; the core's VM lifecycle and device stack
   attach to the native backend instead of KVM.
5. **Parity gate.** The same `cargo test` suite that runs hosted must pass
   against the native backend on real hardware (or QEMU+OVMF where the device
   under test permits it). A backend-specific test may only diverge where the
   hardware genuinely differs, and the divergence must be named in the test.

## Non-goals

- There is no separate `backend-*` feature flag and there will not be one: the
  hypervisor backend is determined by the target OS/architecture.
- The KVM path is the permanent development backend, not a prototype to throw
  away. Keeping it compiling is part of every phase.

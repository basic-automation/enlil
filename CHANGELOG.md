# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] — 2026-10-10

The first stable release of Enlil. 1.0.0 marks the point where every roadmap
phase in scope for 1.0 has a landed, tested implementation behind it: a `no_std`
platform layer, a KVM-backed development VMM, a virtual device library, USB
peripheral routing, guest-transparency hardening, and a bare-metal UEFI boot
kernel that runs guests under AMD SVM — verified nightly under QEMU+OVMF.

### Platform layer (`enlil-platform`, `enlil-std`)

- Full `no_std` platform abstraction: work-stealing scheduler with priority
  inheritance, `no_std` async reactor (BTreeMap + spin-mutex), real context
  switch with VMX XSAVE state, `no_std` `io::Error` type, and a spinlock-based
  sleep lock.
- The boot kernel links `enlil-platform` directly (`platform-baremetal`): it
  drives the scheduler from the LAPIC TSC-deadline timer, carves a disjoint
  DMA / heap / per-guest RAM plan from a `MemoryMap` built off the real
  firmware map, and runs guests out of the planned region.
- Host page tables, `mmap`, and guard pages; region carving wired into boot;
  full `std::sys` platform bindings.

### Hardware abstraction (`enlil-hal`)

- `HypervisorBackend` portability trait; arch-neutral `VmExit` model.
- Intel VMX capability decode (capability MSRs, VMCS field/exit encodings,
  VMXON/VMCS region init) and AMD SVM decode (CPUID `Fn8000_000A`, `VM_CR`
  gates, VMCB control/save-area layout, `#VMEXIT` decode).
- VMCB *programming* layer (`program_minimal_hlt_guest`, segment/exit-field
  accessors), 4 KiB-aligned VMCS/VMXON/VMCB/host-save region types
  (`enlil-hal::region`), and an NPT identity-map builder (`enlil-hal::npt`).
- Mixed-granularity NPT mapper (2 MiB + 4 KiB ranges at non-zero base),
  NPT write-protection / per-epoch dirty-tracking bitmap, and an
  RDTSC-intercept run-loop path (`RunLoopExit::Rdtsc` + `emulate_rdtsc`).

### Hypervisor core (`enlil-core`, KVM-backed development VMM)

- Multi-guest CPU/memory partitioning on Linux KVM; SR-IOV NIC passthrough;
  NVMe namespace passthrough via VFIO.
- Managed vCPU run loop: stealth stack, per-entry platform-timer cadence,
  APIC timer / TSC-deadline preemption, vCPU state switch saving.
- Guest-boot orchestrator (`enlil-run`): config → `GuestRuntime` → run,
  `bzImage` kernel load (setup header / `boot_params` / initrd / E820,
  real · protected · long-mode entry), per-guest CPUID + vTPM + LBR
  transparency, periodic timer-driven guest preemption with tick delivery.
- Suspend/resume: vCPU state snapshot/restore plus automatic ACPI S3 resume
  (FACS waking-vector discovery; long-mode vCPU prepare for the 64-bit X
  waking vector).

### Virtual devices (`enlil-devices`)

- VirtIO net/block, interrupt controller, timers, PS/2, HDA audio, and
  storage devices; APICv posted-interrupt (Intel) and AVIC (AMD) acceleration.

### USB peripheral routing (Phase 4)

- Virtual xHCI with TRB-level emulation; config-driven `[usb.routing]` rules
  assembled into a controller-validated routing registry (wired from
  `enlil-config` into the daemon).
- Isochronous endpoint forwarding via async streams; whole-controller
  passthrough via VFIO with fallback; bare-metal xHCI port scan.
- Daemon-side USB command service with an end-to-end test
  (two mice + two keyboards routing milestone); interactive ratatui management
  app hosting the host-side UI.
- In-guest bridge agent with automated detection reporting (CPUID identity),
  packaged as `.deb`, `.rpm`, and `.msi` (`scripts/package-bridge-agent.sh`).

### Guest transparency (Phase 5)

- ACPI/SMBIOS synthesis (incl. S3/S4/S5 sleep states), CPUID identity +
  topology + timing + LBR stealth, per-guest vTPM with persistent NV state,
  independent endorsement keys, NIC-OUI transparency in config with safe MAC
  synthesis.
- Anti-analysis validation: pafish and al-khaser suites plus anti-cheat
  validation; ACPI AML fuzzing (badaml).
- The hypervisor bit, the empty `0x40000000` vendor leaf, and preserved
  Invariant-TSC are all verified from inside a real guest.

### Bare-metal boot (Phase 6)

- UEFI payload (`enlil-boot`): collects ACPI RSDP, GOP framebuffer, and the
  final UEFI memory map into a `BootHandoff`, calls `ExitBootServices()`,
  and transitions into the Enlil kernel.
- Kernel bring-up: own heap, IDT with IST stacks (`#DF`/`#PF`/`#GP`),
  x2APIC local APIC, TSC calibration against the PIT (monotonic nanosecond
  clock + busy-sleep), per-CPU GS-base TLS, own identity page tables with a
  null-page guard, kernel-owned stack with a guard page, AP bring-up via
  `INIT`-`SIPI`-`SIPI` from the MADT inventory, GOP color console
  (PixelFormat, full ASCII font, scrolling/cursor), and PCI enumeration via
  legacy `0xCF8`/`0xCFC` and ECAM MMIO.
- Real `#VMEXIT` dispatch engine under AMD SVM: VMRUN loop with a guest-GPR
  save/restore shell; CPUID answered with the hypervisor bit cleared
  (in-guest undetectable); port I/O decoded; RDMSR spoofed / WRMSR shadowed
  per-guest; nested page faults demand-mapped; native arithmetic runs with no
  `#VMEXIT`; VMSAVE/VMLOAD of guest FS/GS/TR/LDTR around VMRUN.
- Event delivery: `EVENTINJ` interrupt injection through the guest's real-mode
  IVT and 64-bit long-mode IDT (handler runs, IRETs, guest resumes);
  V_INTR masking honored while the guest has `IF=0`; forced preemption of a
  spinning guest; exception interception with error-code re-injection
  (`#PF`/`#GP` carrying the exact EXITINFO1 code); NPT write-protection with
  record-and-grant dirty tracking; `VMMCALL` paravirt hypercall; full
  64-bit long-mode guest with its CR3 walked through the NPT.
- Periodic LAPIC-timer preemption with tick delivery into the guest —
  the complete hardware-clock → virtual-interrupt → guest-handler path.
- Firmware-table discovery: MADT (CPUs + APIC IDs), MCFG (ECAM), DMAR/IVRS
  IOMMU units with device scopes, plus `discover::build_device_tree`
  (NUMA nodes via SRAT, PCI BAR walk incl. BAR sizing, `MemoryMap`
  carve/plan from E820 & UEFI maps).
- All of the above asserted nightly by `scripts/qemu-boot-test.sh`
  (KVM-accelerated QEMU+OVMF, nested `+svm`).

### Management & console (`enlil-mgmt`, `enlil-mgmt-proto`)

- Interactive ratatui management console (guest list, USB tab); management
  network driver; performance monitoring exposed via the console.

### Setup & packaging

- `enlil-setup` first-run wizard (hardware detection, resource allocation,
  guest configuration); USB live-boot image producing `EFI/BOOT/BOOTX64.EFI`;
  ESP layout with OVMF firmware + vars plumbing; developer docs (mdbook)
  scaffold.

### Release mechanics

- Workspace version `0.1.0` → `1.0.0` across all eleven crates; this
  changelog; README status promoted from pre-alpha to 1.0.0 with honest
  scope.

### Known limitations

- Several features landed with host-side unit tests but **no live proof on
  this development host** (no `/dev/kvm` / QEMU here): Windows/OVMF guest
  boot, Intel VMX instruction execution, multi-vCPU/SMP guest boot,
  guest-observed TSC offset on physical AMD, AP long-mode bring-up, and
  related LAPIC-timer proofs. Their boot proofs run on the nightly
  QEMU+OVMF host.
- **No independent security audit has been performed.** Do not run untrusted
  or hostile guests under Enlil, and do not rely on it to isolate workloads
  that matter. See [SECURITY.md](SECURITY.md).
- Out of scope for 1.0: full GPU sharing beyond Tier-1 passthrough and the
  VirtIO-GPU 2D backend, the compute fabric, ARM/RISC-V backends, and the
  multi-machine mesh (Phases 7–11). See [ROADMAP.md](ROADMAP.md).

[1.0.0]: https://github.com/basic-automation/enlil/releases/tag/v1.0.0

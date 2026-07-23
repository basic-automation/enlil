# Enlil

> *Enlil — the Sumerian god who separated heaven from earth, ruled the space between, and assigned domains to lesser gods.*

A portable, bare-metal-capable **Type-1 hypervisor written in Rust** that turns a single
x86 desktop into multiple transparent virtual PCs — with granular peripheral routing, GPU
sharing, and a path toward pooling hardware across many physical machines.

- **Language:** 100% Rust (`no_std` for the core, `std` for tooling)
- **Foundation:** the [RustVMM](https://github.com/rust-vmm) crate ecosystem
- **Target hardware:** x86-64 desktops with Intel VT-x/VT-d or AMD-V/AMD-Vi (ARM & RISC-V planned)
- **Boot model:** a UEFI application on a USB stick — non-destructive, coexists with your existing OS
- **License:** TBD (MIT or Apache-2.0 recommended)

---

## What is Enlil?

Most virtualization runs *N guests on 1 host*. Enlil's near-term goal is to make a single
machine feel like several independent PCs — each guest pinned to its own cores and memory,
each seeing what looks like real, dedicated hardware — and to do it **transparently** enough
that a guest OS (including Windows, with anti-VM-detection hardening) cannot tell it is
virtualized.

The longer-term vision is **logical machines over a physical pool**: Enlil mediates *both
directions* of every hardware interaction (guest→hardware traps and hardware→guest
interrupts) and can route them across a pool of nodes. That turns physical machines into
stateless hardware providers and each guest into a logical machine defined only by a
resource manifest and a routing table — hardware disaggregation *underneath unmodified
operating systems*. See the [**Core Model**](#core-model--logical-machines-over-a-physical-pool)
below for the full picture.

A defining usability constraint shapes the whole project: **Enlil must be testable without
modifying your system.** It boots from a USB stick as a standard UEFI application
(`/EFI/BOOT/BOOTX64.EFI`), loads its config and per-guest virtual firmware from that stick,
and leaves every existing drive and bootloader untouched. Unplug it and reboot to return to
bare metal instantly.

```
┌─────────────┐  ┌─────────────┐  ┌─────────────┐
│  Guest OS 1 │  │  Guest OS 2 │  │  Management  │
│ (Windows)   │  │  (Linux)    │  │   Console    │
├─────────────┤  ├─────────────┤  ├─────────────┤
│ Virtual HW  │  │ Virtual HW  │  │  Config API  │
│ vCPU · vGPU │  │ vCPU · vGPU │  │  USB Route   │
│ vUSB · vNIC │  │ vUSB · vNIC │  │  GPU Policy  │
├─────────────┴──┴─────────────┴──┴─────────────┤
│                  ENLIL CORE                    │
│   CPU/Mem Sched · GPU Arbiter · USB Router ·   │
│            Storage / Net Manager               │
├────────────────────────────────────────────────┤
│           UEFI Firmware (boot payload)         │
├────────────────────────────────────────────────┤
│                Physical Hardware               │
└────────────────────────────────────────────────┘
```

Each guest gets its own virtual UEFI (OVMF) and its own bootloader *inside* the VM — Enlil
sits below everything and never replaces GRUB or the Windows Boot Manager.

---

## Core Model — Logical Machines over a Physical Pool

The near-term product (transparent virtual PCs on one machine, described in [**What is Enlil?**](#what-is-enlil)) is one point on a much larger design. Enlil **mediates both directions of every hardware interaction** — the guest→hardware path (VM-exits, MMIO/PIO traps, hypercalls, VirtIO kicks) *and* the hardware→guest path (physical IRQs, DMA completions, input events, captured on the node that owns the device and injected as virtual interrupts on the node running the guest). Because both directions pass through Enlil, the mapping from a guest to the hardware it runs on is arbitrary and may cross machine and network boundaries.

That turns classic virtualization inside out. Instead of *N guests on 1 host*, Enlil aims for **N guests on M hosts, fully composable**: each physical machine becomes a stateless hardware node (a resource provider), and each guest becomes a **logical machine** defined only by a resource manifest plus a routing table. It is hardware disaggregation *underneath unmodified operating systems* — the ambition behind CXL, RDMA device pools, and LegoOS, but working transparently beneath stock Windows and Linux rather than requiring a custom OS.

Three physical laws bound what such a pool can actually do, and they shape the whole design.

### Interconnect latency sets the granularity of sharing

How tightly two nodes can share resources is governed by the latency of the link between them. Local DRAM is ~80 ns; a WAN round-trip is ~20–40 ms — roughly 100,000× slower — so a vCPU can never run against RAM that lives thousands of miles away.

| Interconnect | Added latency | What can be pooled transparently |
|---|---|---|
| On-board PCIe / **CXL** | sub-µs (~200–400 ns) | almost anything, including memory as a NUMA tier and live devices |
| **RDMA**, same datacenter | ~1–5 µs | storage, NIC, GPU-compute, cold-page memory — *not* a hot vCPU's RAM |
| **WAN**, 1000s of miles | ~20–40 ms RTT | coarse/async only: remote storage, streamed I/O, whole-guest migration, replication |

The rule that falls out: **a single kernel's tightly-coupled hot CPU+RAM working set stays on one node** (or one CXL-NUMA domain). Across distance you *move or replicate* a guest — you don't share its hot state. Everything else (devices, storage, GPU offload, spare capacity) is poolable with latency-aware placement. To make this practical, every virtual device is built as a front-end/back-end pair behind a pluggable transport (`local | CXL | RDMA | network`), so remoting a device is a transport swap rather than a rewrite.

### ISA sets the boundary of a logical machine

The latency law has a hardware-architecture twin: **a single kernel's execution cannot span an ISA boundary.** Page-table formats, the interrupt model, atomics, and the memory model are all ISA-fixed, and no machine description a stock OS accepts (ACPI or device tree) can express mixed-ISA SMP. So every logical machine carries a **placement key** — `(ISA, vendor, feature baseline)` — frozen at creation; its vCPUs run natively only on matching nodes, and that key defines its live-migration domain. Foreign-ISA nodes can still serve every *other* lane (device back-ends, memory tiers, fabric work units are ISA-blind).

A guest may optionally run via binary translation, expressed as a per-guest **execution mode**:

- **transparent** — vCPUs run natively on matching nodes; remote nodes contribute device back-ends only. This is the full-fidelity product.
- **hybrid** — native vCPUs plus a translated compute *device* (an opt-in paravirtual accelerator); the native surface stays fully stealthed.
- **mesh** — all vCPUs translated (JIT); location- and ISA-free, but a compatibility mode only, and explicitly detectable.

Each guest manifest declares the floor it will accept (e.g. `require = transparent` … `allow = mesh`) and the pool's composition decides what is achievable — Windows-for-ARM on an all-x86 pool admits only in mesh mode, if at all. Modes are switchable at runtime without a guest reboot. Crucially, **translation is a compatibility tool, never a transparency one**: per-block overhead and timing artifacts defeat the anti-detection guarantees, so the translated layer is always presented to the guest as a *device*, never as extra CPUs.

### Trust domains: your own pool vs. federating with others

Latency and ISA decide *what* can be pooled and *where* it runs natively. A third, orthogonal axis — the **trust domain** — decides *how* compute must be protected once a pool spans more than one owner.

- **An "Enlil device" = every node under one owner.** Your phone, desktop, and laptop join into a single pool that presents as one logical-machine device. Inside this boundary compute is **trusted** and shared in the clear — plaintext memory tiers, zero-copy fabric, native vCPU placement — exactly the Core Model above.
- **Federating across owners = untrusted compute.** Two Enlil devices can share capacity (borrow a friend's idle GPU or cores), but across that boundary a node you don't own must never see plaintext or be trusted for integrity. That is the **blind-compute** regime: secret-shared / MPC / ZK-verified work units, backed by attestation that the peer runs genuine Enlil.

Trust is orthogonal to latency — a node can be near-but-untrusted (a friend's LAN) or far-but-trusted (your own VPS) — so the placement key gains a trust dimension on top of `(latency class, ISA, vendor, feature baseline)`. Each logical machine declares which trust domains may serve it: a sensitive guest pins to `trust = owner-only`, while fungible, decomposable work may spill to `federated-blind` capacity. This is what extends *N guests on M hosts* to *M hosts owned by different people*.

### Transparency and guest-OS feasibility

Transparency is **per-guest-family**: Enlil presents whatever machine model each OS expects (PC/UEFI, ARM SoC, Apple platform) and defeats that family's specific VM-detection vectors, generalizing "transparent virtual PC" to a *transparent virtual machine, platform-shaped per guest*. How far that reaches today:

| Guest | Feasibility |
|---|---|
| Linux, *BSD (x86) | ✅ near-term |
| Windows (x86) | ✅ in scope, with VM-detection hardening |
| Android (x86) | ✅ near-term |
| Android (ARM) | ⏳ needs the ARM backend; Play Integrity is hardware-attested |
| macOS | ⚠️ Apple hardware only — license + SMC/board-id, a major transparency lift |
| iOS | ❌ research-only — Apple-signed boot chain + Secure Enclave, not transparently virtualizable on non-Apple hardware |

---

## Status

Enlil is **pre-alpha** and under active, incremental development. Work happens on a
KVM-backed development path on a Linux host first; the bare-metal VMX/SVM backend comes
later (this is the same pragmatic strategy Firecracker and Cloud Hypervisor used).

| Area | State |
|------|-------|
| Platform layer + custom `std` target (`x86_64-unknown-enlil`) | ✅ Implemented & tested (`enlil-platform`, `enlil-std`) |
| HAL portability trait (`HypervisorBackend`) | ✅ Defined (`enlil-hal`) |
| KVM-backed VMM: single & multi-guest, CPU/memory partitioning | ✅ Implemented on Linux (`enlil-core`) |
| Virtual device layer (VirtIO net/block, interrupts, timers, PS/2, HDA, storage) | ✅ Implemented (`enlil-devices`) |
| Managed vCPU run loop: stealth stack + per-entry platform-timer cadence & LAPIC TSC-deadline firing | ✅ Implemented on Linux (`enlil-core`, `StealthRunLoop`) |
| Guest-boot orchestrator: config→`GuestRuntime`→run, `bzImage` kernel load (setup-header/`boot_params`/initrd/E820, real·protected·long-mode entry), per-guest CPUID+vTPM+LBR transparency, guest-RAM I/O — driven by the `enlil-run` binary | ✅ Implemented on Linux (`enlil-core`, `orchestrator`), KVM-verified end to end |
| Suspend/resume: vCPU state snapshot/restore + **automatic** ACPI S3 resume — a guest that commits a suspend is resumed in place (FACS waking-vector discovery via the RSDP→XSDT→FADT→FACS walk, re-entry at the waking vector) with no teardown | ✅ Implemented on Linux (`enlil-core`, KVM-verified) |
| USB peripheral routing (virtual xHCI, TRB-level; config-driven `[usb.routing]` rules assembled into a controller-validated routing registry; Linux sysfs host enumeration + cadence-throttled hot-plug polling; console USB-tab + guest-list view-models/controllers) | 🚧 Substantially implemented |
| Windows transparency (ACPI/SMBIOS synthesis incl. S3/S4/S5 sleep states, CPUID identity+topology/timing/LBR stealth, per-guest vTPM with persistent NV state, independent endorsement keys & name-derived seeds; NIC-OUI transparency in config + safe MAC synthesis; anti-detection: IET APERF/MPERF divergence + CPUID hypervisor-tell decoders — with the hypervisor bit, empty `0x40000000` vendor leaf, and preserved Invariant-TSC all KVM-verified from inside a real guest) | 🚧 Building blocks implemented |
| Bare-metal UEFI boot (Phase 6) | 🚧 The bare-metal kernel now boots and brings the machine up under its own control — proven nightly under QEMU+OVMF. The UEFI payload's `efi_main` collects the ACPI RSDP + GOP framebuffer + final UEFI memory map into a `BootHandoff`, calls `ExitBootServices()`, prints its liveness banner to COM1 serial, and **transitions into the enlil kernel**, which then, with no firmware left: walks the handed-off memory map and reports usable RAM; installs its own **heap** (a switching global allocator: firmware pool → the kernel's linked-list heap in the largest conventional region) and proves a live allocation; loads its own **IDT** with exception handlers and self-tests it by taking an `int3`; enables the **local APIC** in x2APIC mode; enables **AMD SVM** (`EFER.SVME`) and programs the **`VM_HSAVE_PA`** host state-save area; **runs a guest through a real `#VMEXIT` dispatch engine** — it allocates the guest its own **isolated 2 MiB RAM window** and maps guest-physical 0 onto it (a non-identity NPT, so the guest sees its RAM at GPA 0 while it lives at a disjoint system-physical base — LOCKED PRINCIPLE 5), writes a real-mode program into it, and `VMRUN`s in a loop that routes every `#VMEXIT` through the HAL's arch-neutral `VmExit` model with a **guest-GPR save/restore shell** around each entry (VMRUN carries only RAX/RSP/RIP/RFLAGS; the shell swaps the other 14 registers so the guest keeps state across an intercepted-and-resumed instruction). The loop **emulates** the guest's intercepted instructions: **CPUID** answered with a stealthed host result — the guest reads `CPUID.1:ECX[31]` (hypervisor-present) as **0**, so it cannot detect enlil in-guest (LOCKED PRINCIPLE 1); **port I/O** decoded + captured; **RDMSR** answered with a stealth value and **WRMSR** shadowed per-guest and read back (the write never reaches host hardware); a **nested page fault** on an unmapped GPA is **demand-mapped** (fresh frame + NPT leaf) and the access re-executes; and a **native arithmetic loop** runs a taken branch to completion with **no `#VMEXIT`** — near-native guest execution — before the guest `HLT`s. The run shell brackets `VMRUN` with **`VMSAVE`/`VMLOAD`** so the guest's `FS`/`GS`/`TR`/`LDTR` + `SYSENTER` state is swapped in and out (a guest using segmentation/syscalls is safe); enlil **delivers interrupts into a guest and the guest handles and returns from them**: an `EVENTINJ`-injected interrupt vectors through the guest's real-mode IVT to a handler that does work in guest RAM (which the hypervisor reads back through the NPT window) and `IRET`s to resume the interrupted code; the same round-trip runs in **64-bit long mode through a real guest GDT + 64-bit IDT interrupt gate + `IRETQ`** (the way a real x86-64 OS handles interrupts); a **maskable virtual interrupt** posted via the VMCB `INT_CONTROL` (`V_INTR`) is correctly **held off while the guest masks interrupts** (`IF=0`) and delivered only when it `STI`s; and enlil **forcibly preempts a guest spinning in an infinite loop** that never yields by posting that virtual interrupt — pure time-slicing, the mechanism a scheduler quantum uses to reclaim a CPU. enlil **services a `VMMCALL` paravirt hypercall** (result returned in the guest `RAX`); and a full **64-bit long-mode guest** runs with paging on, its own `CR3` page-table walk resolved through the NPT (the mode a real x86-64 OS boots in). All boot-verified nightly on real nested SVM. The kernel also loads its **own GDT + TSS with three Interrupt Stack Table stacks** (the `#DF`, `#PF`, and `#GP` handlers each run on their own known-good stack, every IST-switch self-tested), arms the **LAPIC timer** in both one-shot and **TSC-deadline** mode (the precise-preemption clock a scheduler quantizes on — it even arms a 2 ms deadline from a nanosecond request and confirms via its own clock that it fired on time), **calibrates the TSC** against the fixed-rate PIT (~4.2 GHz measured) and turns it into a **monotonic nanosecond clock with a busy-sleep** (verified by a 5 ms sleep), installs a **per-CPU GS-base TLS** block, switches to its **own identity page tables** (off the firmware's, span derived from the memory map + framebuffer) with the low 2 MiB rendered at 4 KiB granularity and **virtual address 0 left unmapped as a null-page guard** (a stray kernel null-pointer dereference faults into the `#PF` handler instead of silently touching page 0), and can **refine any other 2 MiB region of that live map down to 4 KiB pages**, keeping the bulk huge-page-granular while a chosen region is managed page by page. It uses that to run on a **stack it owns with a guard page**: the whole rest of bring-up executes on kernel-allocated memory instead of the firmware's boot-services stack, with the page below it left unmapped so a stack overflow takes a diagnosable `#PF` (on its own IST stack) rather than silently corrupting its neighbour — the guard is *proven* absent by walking the live page tables, not by touching it. It **brings up the other CPUs**: the BSP finds a firmware-conventional page below 1 MiB for the AP startup trampoline (discovered from the real memory map, not a hardcoded address), installs a 16-bit real-mode trampoline, and wakes each application processor from the MADT inventory with **`INIT`-`SIPI`-`SIPI`** at the SDM's timings, each AP reporting in on a shared counter before a bounded timeout. It self-tests a **spinlock** primitive, and draws both a boot indicator and an **8×8-font text banner** to the **GOP framebuffer** (pixel-readback verified). It **discovers real hardware from the firmware tables** the UEFI stage handed off: ACPI RSDP→XSDT→**MADT** (enabled-CPU count **plus each processor's APIC ID** — the SMP AP inventory, booted under `-smp 2`), **MCFG** (the PCIe **ECAM** window), and **DMAR/IVRS** — for VT-d it parses the DMAR body into each **DMA-remapping hardware unit** (register block, PCI segment, catch-all flag) and decodes that unit's **device scopes** (which endpoints, bridges, I/O APICs and HPETs it governs — what can be placed in a per-guest DMA domain), verified nightly against an emulated VT-d IOMMU's real firmware table. It enumerates **PCI** both via the legacy `0xCF8`/`0xCFC` mechanism (host-bridge identity, device classes, MSI-X capability, and the first memory **BAR** — base **and size** via the standard all-ones write-probe, e.g. the real VGA BAR sized at 16 MiB, plus a **summary of every memory BAR on the bus** — their count and total MMIO footprint, the window the hypervisor must account for in config-space routing and passthrough) and via **ECAM MMIO** (all buses, extended config space). Each step is asserted on serial by the headless QEMU+OVMF harness (`scripts/qemu-boot-test.sh`, KVM-accelerated, nested `+svm`). The HAL carries the decode + region seams both backends need: `enlil-hal::vmx` (Intel VMX capability MSRs, VMCS field/exit encodings, VMXON/VMCS region init) and `enlil-hal::svm` (AMD SVM CPUID/`VM_CR` gates, the VMCB control/save-area layout, `#VMEXIT` decode), plus `enlil-hal::region` owning the 4 KiB-aligned VMCS/VMXON/VMCB/host-save pages, `enlil-hal::svm`'s VMCB *programming* layer (`program_minimal_hlt_guest` + segment/exit-field accessors) and `enlil-hal::npt`'s nested-page-table identity-map builder — all built for the `x86_64-unknown-enlil` kernel target. The kernel now **links `enlil-platform` directly** (Phase 1.2 — the crate is `no_std` under `platform-baremetal`, so its host-agnostic modules cross-compile for the bare-metal target): it drives enlil-platform's **work-stealing scheduler** (four tasks submitted out of order run Critical→Low on real hardware, their closures heap-boxed through the kernel's own allocator across the crate boundary), builds a **`MemoryMap` from the real firmware map** and carves a disjoint DMA / heap / per-guest RAM plan (cross-checked against the kernel's own memory walk — LOCKED PRINCIPLE 5) after marking the already-installed kernel heap as in-use, so the planner can never hand a guest memory the hypervisor is allocating from — and **guests now run out of that planned region** rather than the kernel heap, so a guest's nested mapping cannot reach the hypervisor's own allocator. It times a busy-sleep with enlil-platform's **`Instant`** monotonic clock — all boot-verified, so the boot kernel consumes shared platform primitives instead of re-deriving them. Still landed: `discover::build_device_tree` assembles a `HostDeviceTree` from the ACPI tables (CPUs with NUMA nodes via MADT + SRAT, per-node RAM ranges and distances via SRAT/SLIT, a full MCFG-ECAM PCI walk incl. BARs/xHCI/capabilities/SR-IOV, DMAR/IVRS IOMMU kind), plus `MemoryMap` carve/plan from E820 & UEFI maps and x86-64 identity page-table builders. Next: bringing each started AP from real mode into long mode with its own guarded stack and per-CPU state (multi-vCPU guests follow), programming the discovered **IOMMU** units' root/context tables for per-guest DMA isolation, and booting a real distro / privileged service VM |
| GPU sharing, compute fabric, ARM/RISC-V, multi-machine mesh | ⏳ Planned (Phases 7–11) |

> **Honest caveat:** the RustVMM/KVM dependencies in `enlil-core` are gated to
> `target_os = "linux"`, and live guest-boot/integration tests require KVM (nested
> virtualization). On other hosts the logic-level unit tests still build and run, but a real
> guest boot does not. Wiring `enlil-core` and `enlil-devices` fully through `enlil-hal` is
> in progress. See [`ROADMAP.md`](ROADMAP.md) for exactly what is built vs. planned.

---

## Architecture

Enlil is a Cargo workspace of focused crates layered over a custom platform abstraction. The
key idea — **platform-first** — is that every crate above the platform layer writes ordinary
Rust (`Vec`, `HashMap`, `Arc<Mutex<T>>`, `async`/`await`), whether the final target is Linux
or bare metal. The platform crate absorbs all the `no_std` complexity.

```
┌─────────────────────────────────────────────────────────────────┐
│                          Enlil Crates                           │
│  ┌──────────┐ ┌───────────┐ ┌────────────┐ ┌───────────────┐   │
│  │enlil-core│ │enlil-mgmt │ │enlil-config│ │ enlil-devices │   │
│  └────┬─────┘ └─────┬─────┘ └─────┬──────┘ └──────┬────────┘   │
│       │             │             │               │            │
│  ┌────┴─────────────┴─────────────┴───────────────┴────────┐   │
│  │                       enlil-hal                          │   │
│  │        HypervisorBackend trait (VMX / SVM / EL2 / H)     │   │
│  └──────────────────────────┬───────────────────────────────┘  │
│                             │                                   │
│  ┌──────────┐         ┌─────┴─────┐         ┌───────────────┐   │
│  │enlil-boot│         │enlil-setup│         │   enlil-std   │   │
│  │UEFI stub │         │ setup TUI │         │  std facade   │   │
│  └──────────┘         └───────────┘         └───────────────┘   │
├─────────────────────────────────────────────────────────────────┤
│                     Rust std / core / alloc                     │
│            (x86_64-unknown-enlil on bare metal, or              │
│              x86_64-unknown-linux-gnu when hosted)              │
├─────────────────────────────────────────────────────────────────┤
│                         enlil-platform                          │
│   Memory/Alloc · Threading · Sync · Async · Time · I/O          │
│   ┌─────────────────────────┐  ┌────────────────────────────┐   │
│   │   platform-linux        │  │   platform-baremetal       │   │
│   │   (delegates to std)    │  │   (raw HW implementations) │   │
│   └─────────────────────────┘  └────────────────────────────┘   │
├─────────────────────────────────────────────────────────────────┤
│           Hardware  ·  or  ·  Linux + KVM host                  │
│      x86_64 (VMX/EPT)  ·  ARM (EL2)  ·  RISC-V (H-ext)          │
└─────────────────────────────────────────────────────────────────┘
```

The second pillar is the **HAL**: `enlil-hal` defines a single architecture-neutral
`HypervisorBackend` trait, so the rest of the system never touches VMCS fields, EPT entries,
or KVM ioctls directly. Adding ARM EL2 or native VMX is "just" a new `impl HypervisorBackend`
behind a `#[cfg(target_arch = "…")]` gate — the core, device, config, and management crates
don't change.

```rust
pub trait HypervisorBackend: Send + Sync {
    type VCpu: Send;
    type PageTable: Send;
    type InterruptController: Send;

    fn create_vcpu(&self, config: &VCpuConfig) -> HalResult<Self::VCpu>;
    fn run_vcpu(&self, vcpu: &mut Self::VCpu) -> HalResult<VmExit>;
    fn handle_exit(&self, vcpu: &mut Self::VCpu, exit: &VmExit) -> HalResult<bool>;
    fn map_guest_memory(&self, page_table: &mut Self::PageTable, guest_addr: u64, host_addr: u64, size: u64, writable: bool) -> HalResult<()>;
    fn inject_interrupt(&self, vcpu: &mut Self::VCpu, controller: &Self::InterruptController, irq: u32) -> HalResult<()>;
}
```

### Crate map

| Crate | Role |
|-------|------|
| **`enlil-platform`** | The foundation. std-equivalent APIs (memory/alloc, threading + scheduler, sync, async runtime, time, I/O) for both hosted (`platform-linux`, delegates to `std`) and bare-metal (`platform-baremetal`, raw hardware) targets. Under `platform-baremetal` the crate is `#![no_std]` + `alloc`, so `memory`, `sync`, `threading`, and `time` cross-compile for `x86_64-unknown-enlil` / `x86_64-unknown-uefi` and are **linked + boot-driven by `enlil-boot`** (`io`/`async_rt` still hosted-only pending a `no_std` `IoError` / reactor). |
| **`enlil-std`** | A `std`-shaped facade (`collections`, `future`, `io`, `sync`, `thread`, `time`) over `enlil-platform`, with integration tests — proves the platform layer can carry ordinary Rust on the custom target. |
| **`enlil-hal`** | The architecture-neutral `HypervisorBackend` trait — the only place ISA-specific virtualization details are meant to appear. |
| **`enlil-core`** | The hypervisor core: VM lifecycle, vCPU management, memory partitioning, EPT, CPUID/SMBIOS/ACPI handling, serial console, timing stealth, vTPM. Carries the RustVMM stack (`kvm-ioctls`, `vm-memory`, `vm-superio`, `linux-loader`, …) gated to Linux for the KVM-backed dev path. |
| **`enlil-devices`** | The virtual device library: VirtIO net/block, full ACPI table synthesis, SMBIOS, interrupt controllers (LAPIC/IOAPIC/MSI), timers (PIT/HPET/TSC/paravirt), PS/2, HDA audio, qcow2/raw storage, a virtual **xHCI** USB stack with routing, an inter-guest **bridge** (clipboard, drag-and-drop, shared FS, notifications, URL/protocol-handler routing), a display compositor (Enlil Zones, incl. per-monitor multi-monitor tiling), and anti-detection **stealth** modules (CPUID, timing, LBR, PMC). |
| **`enlil-config`** | TOML guest definitions and validation (no overlapping CPU sets, memory, or device assignments). |
| **`enlil-mgmt`** | The management console — `clap` CLI + `ratatui`/`crossterm` TUI for live control (list/start/stop/attach/status). |
| **`enlil-setup`** | First-run setup wizard: detect hardware, define guests, assign resources, write `config.toml`. |
| **`enlil-boot`** | UEFI boot payload + early kernel for the bare-metal target (Phase 6). A real `efi_main` collects the boot handoff and calls `ExitBootServices()`, then `kernel_entry` brings the machine up under enlil's own control: memory-map walk, switching heap, IDT + `int3` self-test, own **GDT+TSS with three IST stacks** (`#DF`/`#PF`/`#GP` self-tested), spinlock, x2APIC + **LAPIC timer** (one-shot **and** TSC-deadline, incl. ns-precise deadline arming), **PIT-calibrated TSC** with a **monotonic ns clock + busy-sleep**, **per-CPU GS-base TLS**, its **own identity page tables** (with 2 MiB→4 KiB refinement) and a **kernel-owned guarded stack** it runs the rest of bring-up on, **SMP AP bring-up** (firmware-discovered low-memory trampoline + `INIT`-`SIPI`-`SIPI`), **ACPI/PCI hardware discovery** (MADT + **APIC-ID inventory** / MCFG / **DMAR remapping units + device scopes** + legacy & ECAM PCI, with memory-**BAR** read + write-probe **sizing**), an **8×8-font GOP text console**, and AMD SVM enable + `VM_HSAVE_PA` then **runs guests through a real `#VMEXIT` dispatch loop** — each guest in its own isolated RAM at GPA 0 (non-identity NPT), a guest-GPR save/restore shell + `VMSAVE`/`VMLOAD` around `VMRUN`, per-exit emulation (CPUID stealth, port I/O, RDMSR spoof + WRMSR shadow, NPF demand-paging, native compute loop, `VMMCALL` hypercall), `EVENTINJ` interrupt injection, and a full 64-bit **long-mode** guest. QEMU+OVMF boot-proven nightly. |

The intended layering is `enlil-mgmt`/`enlil-setup` → `enlil-config`/`enlil-core` →
`enlil-devices` → `enlil-hal` → `enlil-platform`, with `enlil-boot` as the bare-metal entry
point over `enlil-platform` + `enlil-core`. Some of those edges are still being wired through
the HAL (today `enlil-core` is the KVM-backed VMM and `enlil-devices` is a standalone library).

---

## Building

Enlil uses a pinned Rust **nightly** toolchain (see [`rust-toolchain.toml`](rust-toolchain.toml))
because the bare-metal path needs `-Z build-std` and some `asm`/`no_std` features.

```sh
# Hosted development build (Linux/KVM is the default backend)
cargo build
cargo test            # workspace unit + integration tests
cargo clippy
cargo fmt --check

# Bare-metal target — recompiles std against the Enlil platform layer
cargo build --target x86_64-unknown-enlil.json -Z build-std=core,alloc,std
```

| Target | Platform feature | Hypervisor backend | Use case |
|--------|------------------|--------------------|----------|
| `x86_64-unknown-linux-gnu` | `platform-linux` | KVM (via `cfg(target_os = "linux")` deps) | Development (Phases 0–5) |
| `x86_64-unknown-enlil` | `platform-baremetal` | Native VMX (bare-metal) | Production (Phase 6+) |

The **platform** is selected by feature flag at compile time. The **hypervisor backend**
(KVM vs. native VMX) is determined automatically by the target OS — there is no separate
`backend-*` feature flag:

| Feature flag | Effect |
|--------------|--------|
| `platform-linux` | Use the std-delegating platform (default for hosted builds) |
| `platform-baremetal` | Use the bare-metal platform implementations |

> The KVM-backed path requires a Linux host with `/dev/kvm`; live guest-boot tests
> additionally need nested virtualization. Where that isn't available, those tests are
> skipped rather than faked.

---

## Configuration

Guests are declared in TOML and validated by `enlil-config`. See
[`examples/two-guests.toml`](examples/two-guests.toml) for a complete example.

```toml
[[guest]]
name = "debian"
cpus = [0, 1]
memory_mb = 2048
kernel = "/boot/vmlinuz"
initrd = "/boot/initrd.img"
cmdline = "console=ttyS0 root=/dev/vda1"

[guest.storage]
type = "virtio-blk"
path = "/dev/sda2"

[guest.network]
type = "virtio-net"
tap = "tap0"

[[guest]]
name = "windows"
cpus = [2, 3]
memory_mb = 4096
# …
```

Validation enforces the invariants that make isolation real: CPU sets must not overlap
between guests, memory regions must not overlap, total allocation must fit physical memory
minus Enlil's own reservation, and device assignments must be exclusive.

---

## Repository layout

```
enlil/
├── Cargo.toml                  # Workspace root (resolver 2, edition 2024)
├── rust-toolchain.toml         # Pinned nightly
├── x86_64-unknown-enlil.json   # Custom bare-metal target spec
├── README.md                   # This file
├── ROADMAP.md                  # Development roadmap — a [ ]/[x] task checklist of all 11 phases
├── examples/                   # Sample guest configurations
├── enlil-platform/             # Platform abstraction (linux + baremetal backends)
├── enlil-std/                  # std-shaped facade over the platform layer
├── enlil-hal/                  # HypervisorBackend trait
├── enlil-core/                 # Hypervisor core (KVM-backed dev path)
├── enlil-devices/              # Virtual device library
├── enlil-config/               # TOML guest config + validation
├── enlil-mgmt/                 # Management CLI/TUI
├── enlil-setup/                # First-run setup wizard
└── enlil-boot/                 # UEFI boot payload + early kernel (Phase 6)
```

---

## Design principles

1. **Platform-first.** `enlil-platform` exists so every other crate can write normal Rust
   regardless of target. On Linux it's a thin shim over `std`; on bare metal it provides a
   global allocator, cooperative threading, spinlock-based sync, a hardware-counter timer,
   and UART I/O.
2. **Architecture portability via the HAL.** The `HypervisorBackend` trait is the only place
   ISA-specific virtualization appears. The core never touches VMCS/EPT/ioctls directly.
3. **Dual-backend strategy.** The same crate graph compiles both for a Linux/KVM development
   target and for the bare-metal `x86_64-unknown-enlil` production target.
4. **Custom target triple.** `x86_64-unknown-enlil.json` disables the default `std` sysroot
   and recompiles `std` against `enlil-platform` via `-Z build-std`.
5. **USB live boot.** Enlil *is* the bootloader — a UEFI application that boots from USB with
   zero modifications to existing drives, coexisting with Windows Boot Manager / GRUB.
6. **`cfg`-gated architecture code.** Every ISA-specific module sits behind
   `#[cfg(target_arch = "…")]`; `enlil-core` contains zero architecture-specific imports.

---

## Security model

- **Guest isolation is non-negotiable.** EPT/NPT enforces memory separation in hardware — no
  guest can read another guest's memory or Enlil's own.
- **CPU partitioning.** Each vCPU is pinned to a physical core; CPU sets must not overlap
  (validated by `enlil-config`).
- **Device isolation.** Passthrough devices use the IOMMU (VT-d / SMMU) to restrict DMA to
  the owning guest's memory.
- **Minimal attack surface.** In bare-metal mode Enlil runs no OS — it *is* the OS. No
  syscalls, no kernel modules, no userspace; the only ring-0/EL2 code is Enlil itself.
- **Rust safety.** `unsafe` is minimized and kept to small, audited blocks; it is concentrated in `enlil-platform` and `enlil-hal`, with some `unsafe` in `enlil-core`/`enlil-devices` for FFI and CPU instructions.

The roadmap goes further with confidential-VM support (AMD SEV-SNP / Intel TDX) and optional
ZK-proof attestation and cross-guest isolation proofs — see [`ROADMAP.md`](ROADMAP.md) (Phases 8.7 / 8.9 / 9.12).

---

## Documentation

| Document | What's in it |
|----------|--------------|
| **README.md** (this file) | Project overview — what Enlil is, its architecture, the crate layout, how to build, and where things stand today. |
| [**ROADMAP.md**](ROADMAP.md) | The development roadmap — a `[ ]`/`[x]` task checklist of every phase, from scaffold through to the multi-machine mesh. |

---

## License

Not yet finalized. The roadmap recommends **MIT** or **Apache-2.0** for maximum ecosystem
compatibility with the RustVMM crates Enlil builds on.

# Enlil — Bare-Metal Hypervisor

> *Enlil — the Sumerian god who separated heaven from earth, ruled the space between, and assigned domains to lesser gods.*

A Rust-based Type-1 hypervisor that turns a single x86 desktop into multiple transparent virtual PCs with granular peripheral routing and GPU sharing.

---

## Project Identity

- **Language:** 100% Rust (no_std for core, std for tooling)
- **Foundation:** RustVMM crate ecosystem
- **Target Hardware:** x86_64 desktops with Intel VT-x/VT-d or AMD-V/AMD-Vi
- **Guest OS Support:** OS-agnostic goal — any guest, transparently, by emulating the platform it expects. Near-term (x86): Linux, Windows, *BSD, x86 Android. Via Phase 10 (ARM): ARM Android, Apple-Silicon guests. Stretch: macOS (Apple hardware only — license + SMC/board-id). Research-only: iOS (Apple hardware root of trust — not transparently virtualizable on non-Apple HW). See **Core Model** below.
- **License:** TBD (recommend MIT or Apache 2.0 for max ecosystem compatibility)

---

## Architecture Overview

```
┌─────────────┐  ┌─────────────┐  ┌─────────────┐
│  Guest OS 1 │  │  Guest OS 2 │  │  Management  │
│ (Windows)   │  │  (Linux)    │  │   Console    │
├─────────────┤  ├─────────────┤  ├─────────────┤
│ Virtual HW  │  │ Virtual HW  │  │  Config API  │
│ vCPU, vGPU  │  │ vCPU, vGPU  │  │  USB Route   │
│ vUSB, vNIC  │  │ vUSB, vNIC  │  │  GPU Policy  │
├─────────────┴──┴─────────────┴──┴─────────────┤
│              ENLIL CORE                    │
│  ┌──────────┬──────────┬──────────┬──────────┐ │
│  │ CPU/Mem  │   GPU    │   USB    │ Storage/ │ │
│  │ Sched    │  Arbiter │  Router  │ Net Mgr  │ │
│  └──────────┴──────────┴──────────┴──────────┘ │
├─────────────────────────────────────────────────┤
│          UEFI Firmware (boot payload)           │
├─────────────────────────────────────────────────┤
│              Physical Hardware                  │
└─────────────────────────────────────────────────┘
```

**Boot Flow (power-on to running desktops):**
```
Power On → UEFI Firmware (motherboard, unchanged)
              │
              ├─ F12: Boot menu shows Enlil alongside existing entries
              │       (Windows Boot Manager, GRUB, etc. all still work)
              │
              ▼
         Enlil UEFI Binary (from USB drive or ESP)
              │
              ├── Reads /enlil/config.toml
              ├── Initializes hypervisor (1-2 seconds)
              └── Launches all configured guests in parallel
                    │                        │
                    ▼                        ▼
              ┌──────────┐            ┌──────────┐
              │ OVMF     │            │ OVMF     │
              │ (virtual │            │ (virtual │
              │  UEFI)   │            │  UEFI)   │
              │    │     │            │    │     │
              │    ▼     │            │    ▼     │
              │ GRUB /   │            │ Windows  │
              │ systemd- │            │ Boot     │
              │ boot     │            │ Manager  │
              │    │     │            │    │     │
              │    ▼     │            │    ▼     │
              │ Linux    │            │ Windows  │
              │ Desktop  │            │ Desktop  │
              └──────────┘            └──────────┘
              Monitor 1               Monitor 2
```

Each guest has its own virtual UEFI (OVMF) and its own bootloader inside the VM. Enlil doesn't replace GRUB — it sits below everything. Unplug the USB and reboot to return to your normal bare-metal setup instantly.

---

## Core Model — Logical Machines over a Physical Pool

Enlil mediates **both directions** of every hardware interaction and routes them across a
pool of physical machines, so the mapping between guests and hardware is arbitrary and may
cross machine and network boundaries.

- **OS → HW:** guest VM-exits / MMIO-PIO traps / hypercalls / VirtIO kicks are intercepted
  and routed to a device *back-end* that may live on another node.
- **HW → OS:** physical IRQs, DMA completions, and device/input events are captured on the
  node that owns the device, serialized, routed to the node running the target guest, and
  injected as virtual interrupts / virtqueue completions.

This makes physical machines **stateless hardware nodes** (resource providers) and each
guest a **logical machine** defined only by a resource manifest + a routing table. Classic
virtualization is *N guests on 1 host*; Enlil is *N guests on M hosts, fully composable* —
hardware disaggregation **underneath unmodified OSs** (the ambitious version of CXL / RDMA
device pools / LegoOS, none of which work transparently beneath stock Windows/Linux).

### The governing law: interconnect latency sets the granularity of sharing

| Interconnect | Added latency | What can be pooled transparently |
|---|---|---|
| On-board PCIe / **CXL** | sub-µs (~200–400 ns) | almost anything, incl. memory (CXL.mem as a NUMA tier) and live devices |
| **RDMA**, same datacenter | ~1–5 µs | storage, NIC, GPU-compute, cold-page memory — *not* a hot vCPU's RAM |
| **WAN**, 1000s of miles | ~20–40 ms RTT | coarse/async only: remote storage, streamed I/O, whole-guest migration, replication/failover |

Local DRAM is ~80 ns, so WAN is ~100,000× slower: a vCPU cannot run against RAM that is
1000s of miles away. **A single kernel's tightly-coupled hot CPU+RAM working set stays on
one node** (or a CXL-NUMA domain); across distance you *move or replicate* the guest, you
do not share its hot state. Everything else — devices, storage, GPU-offload, capacity — is
poolable, with **latency-aware placement**.

**Placement policy (heart of Phase 9):** classify each workload — coupled-and-hot → local;
decomposable / replicable → fabric. Enlil presents the aggregate as ordinary virtual
hardware to the guest; behind that front-end the fabric fulfills it with whichever pattern
fits the device's latency class.

**Design rule (applies from Phase 3 onward):** build *every* device as a front-end/back-end
pair behind a pluggable transport (`local | CXL | RDMA | network`), even while only `local`
is implemented — so remoting is a transport swap, not a rewrite. VirtIO / vhost-user / vDPA
is already exactly this shape.

### Distributed-compute planes (Phase 9 Fabric / Phase 11 Mesh)

Patterns for routing latency-tolerant work across heterogeneous, possibly untrusted nodes —
they succeed precisely because they don't share hot state:

- **Work-unit engine (à la Folding@home):** split a job into independent units, scatter to
  the pool, gather async, assign redundantly to beat stragglers/failures, checkpoint. This
  is how a latency-tolerant virtual device (a virtual compute queue or vGPU-offload surface)
  is fulfilled.
- **Resource market + reputation (à la Bittensor):** permissionless heterogeneous nodes
  advertise capability, get scored and compensated — the discovery, incentive, and
  trust-ranking plane that grows the pool beyond your own machines.
- **Verifiable execution + consensus (à la Ethereum):** validity/fraud proofs and BFT
  consensus let untrusted remote nodes run work you can trust without redoing it, and keep
  replicated guests consistent. See [`RESEARCH.md`](RESEARCH.md) → Part II.

### Transparency is per-guest-family

The hypervisor must present the machine model each OS expects (PC/UEFI, ARM SoC, Apple
platform) and defeat that family's VM-detection vectors (CPUID hypervisor bit, RDTSC/TSC
timing, VM-exit latency, ACPI/device signatures). "Transparent virtual PC" generalizes to a
**transparent virtual *machine*, platform-shaped per guest**.

### Guest OS feasibility tiers

| Guest | Feasibility | Notes |
|---|---|---|
| Linux, *BSD | ✅ near-term (x86) | BSD ≈ Linux on x86 — cheap win |
| Windows | ✅ in scope (Phase 5) | + VM-detection hardening |
| Android (x86) | ✅ near-term | Android-x86 / Bliss on the PC model |
| Android (ARM) | ⏳ needs Phase 10a (ARM) | Play Integrity is hardware-attested |
| macOS | ⚠️ Apple hardware only | license + SMC/board-id; major transparency lift |
| iOS | ❌ research-only | Apple-signed boot chain + Secure Enclave; not transparently virtualizable on non-Apple HW |

---

## RustVMM Crates We'll Use

| Crate | Purpose |
|-------|---------|
| `vmm-vcpu` | vCPU abstractions (Intel/AMD) |
| `vm-memory` | Guest memory management, GuestAddress types |
| `vm-superio` | Emulated legacy devices (i8042, UART, RTC) |
| `linux-loader` | Linux kernel/initrd loading (direct boot) |
| `vm-virtio` | VirtIO device backends (net, block, etc.) |
| `kvm-ioctls` | KVM interface (Phase 0 — development on Linux host) |
| `vm-allocator` | Resource allocation (IRQs, MMIO ranges, PIO) |
| `event-manager` | Async event loop for device I/O |
| `vm-device` | Device model traits and bus abstractions |

**Important Note:** RustVMM crates target KVM as the hardware interface. For our bare-metal target we'll initially develop as a KVM-backed VMM (like Firecracker/Cloud Hypervisor), then progressively replace the KVM layer with our own bare-metal VMX/SVM driver in later phases. This is the pragmatic path — get functionality working on KVM first, then go bare-metal. Phase 1 (Platform Layer) builds the abstraction that makes this transition seamless: all code above the platform layer is identical in KVM-backed and bare-metal modes.

---

## Phase 0 — Project Scaffold & Dev Environment

**Goal:** Repo structure, toolchain, CI, and a "hello world" VMM that boots a minimal Linux guest using KVM + RustVMM.

**Duration:** 2–3 weeks

### 0.1 Repository Setup
- Initialize Cargo workspace with these top-level crates:
  - `enlil-platform` — custom Rust `std` platform layer (threading, alloc, sync, async runtime) — **this is the foundation everything else builds on**
  - `enlil-hal` — Hardware Abstraction Layer trait definitions (architecture-neutral interface for VMX/SVM/EL2/H-extension) — **define this from day one for future ARM/RISC-V portability**
  - `enlil-core` — hypervisor core logic (VMM entry, vCPU management)
  - `enlil-devices` — virtual device backends (USB router, GPU arbiter, storage, net)
  - `enlil-config` — configuration parsing, guest definitions, peripheral routing rules
  - `enlil-mgmt` — management console (CLI/TUI for live control)
  - `enlil-boot` — UEFI boot payload (bare-metal target, Phase 6+)
- **Architecture portability from day one:**
  - Define the `HypervisorBackend` trait in `enlil-hal`:
    ```rust
    pub trait HypervisorBackend {
        type VCpu;
        type PageTable;
        type InterruptController;

        fn create_vcpu(&self, config: VCpuConfig) -> Result<Self::VCpu>;
        fn run_vcpu(&self, vcpu: &mut Self::VCpu) -> VmExit;
        fn handle_exit(&self, vcpu: &mut Self::VCpu, exit: VmExit);
        fn map_guest_memory(&self, pt: &mut Self::PageTable, gpa: u64, hpa: u64);
        fn inject_interrupt(&self, vcpu: &mut Self::VCpu, vector: u32);
    }
    ```
  - Only x86 implements this initially, but all `enlil-core` code programs against the trait
  - Keep all x86-specific code behind `#[cfg(target_arch = "x86_64")]` gates
  - This costs almost nothing now but saves massive refactoring when adding ARM/RISC-V (Phase 10)
- Set up CI (GitHub Actions): `cargo clippy`, `cargo test`, `cargo fmt`
- Pin Rust nightly toolchain (needed for some no_std and asm features later)
- Add `rust-vmm` crates as dependencies in `enlil-core`

### 0.2 Minimal KVM-Backed VMM
- Use `kvm-ioctls` to create a VM and vCPU
- Use `vm-memory` to set up guest RAM (mmap-backed GuestMemoryMmap)
- Use `linux-loader` to load a bzImage + initrd into guest memory
- Configure boot params (zero page), set up initial GDT/IDT, protected mode entry
- Run a single vCPU in a loop (KVM_RUN), handle KVM exits (IO, MMIO, HLT, shutdown)
- Use `vm-superio` for a serial console (COM1) so the guest can print to your terminal
- **Milestone:** Boot a minimal Linux kernel (e.g., a buildroot initramfs) to a shell over serial

> **Status (2026-06-02):** `enlil-core::kvm_backend` implements the backend skeleton —
> `KvmBackend::{new, map_memory, create_vcpu, run_vcpu}` over `kvm-ioctls`, with a
> hypervisor-agnostic `GuestExit` model and a `VmExitHandler` trait
> (`io_in/io_out/mmio_read/mmio_write`). Integration tests that touch `/dev/kvm`
> self-skip when nested virt is unavailable. `map_memory` intentionally uses the classic `set_user_memory_region`
> (hva-backed) so the host can synthesise/introspect guest memory for ACPI/SMBIOS
> injection; a `set_user_memory_region2` / `guest_memfd` path is only needed for
> confidential guests (Phase 8) and is incompatible with that introspection.
>
> **Status (2026-06-03):** the device bus is now real. `enlil-devices::bus`
> (`PioBus`/`MmioBus`) owns `Box<dyn PioDevice/MmioDevice>` registered over explicit
> address *ranges* (overlap-rejecting, upper-bound-checked) and dispatches byte-slice
> accesses with little-endian width conversion and x86 open-bus semantics for unmapped
> addresses. `enlil-core::device_bus::DeviceBus` bundles one of each and implements
> `VmExitHandler`, forwarding straight to the bus — the single decode path.
>
> **Status (2026-06-04):** the 16550 UART is now a real bus device.
> `enlil-core::serial::SerialPort` wraps `UartState` + a COM base port and implements
> `enlil_devices::bus::PioDevice` over `[base, base+8)`, mapping the absolute guest port
> to the register offset; `DeviceBus::with_serial`/`add_serial` mount it at COM1 `0x3F8`.
> A `/dev/kvm`-gated integration test (`serial_console_smoke`) assembles a tiny real-mode
> blob that `out`s "OK" to `0x3F8` and `hlt`s, runs it through `KvmBackend::run_vcpu` with
> the `DeviceBus` handler, and asserts the bytes reached the `Buffer`-mode sink (self-skips
> with no nested virt). **Pitfall flagged for next step:** `UartState` is *polled-only* —
> it stores IER/MCR but never raises IRQ4. Linux's 8250 driver auto-detects and can run in
> polled mode, so a serial shell works, but interrupt-driven mode (the default, and what
> vm-superio models via a `Trigger`/eventfd) needs the UART to signal IRQ4 into the
> in-kernel IRQ chip on RX-available / THR-empty. **Next:** give `SerialPort` an IRQ sink
> (raise IRQ4 via `KVM_IRQ_LINE`/an `EventFd`) wired through `DeviceBus`, honoring IER and
> the IIR identification byte; then PIT (`0x40-0x43`) and PCI config (`0xCF8/0xCFC`).

### 0.3 USB Live Boot & Non-Destructive Testing (CRITICAL FOR ADOPTION)

Enlil must be testable without modifying the user's existing system. This is the single most important usability feature for early adoption.

**USB Live Boot:**
- Enlil boots from a USB flash drive as a standard UEFI application (`/EFI/BOOT/BOOTX64.EFI`)
- User plugs in the USB, presses F12, selects it from the UEFI boot menu
- Enlil loads its config and OVMF firmware image from the USB drive
- On unplug + reboot, the machine is exactly as it was — zero modifications to existing drives
- This is the **default development and testing workflow** throughout all phases

**EFI System Partition layout (on USB or existing ESP):**
```
/EFI/BOOT/BOOTX64.EFI          ← Enlil UEFI binary
/enlil/config.toml              ← Guest definitions, resource allocation
/enlil/ovmf/OVMF_CODE.fd       ← Virtual UEFI firmware for guests
/enlil/ovmf/OVMF_VARS.fd       ← Per-guest UEFI variable store (template)
/enlil/isos/                    ← Optional: ISO images for fresh installs
/enlil/state/                   ← Per-guest persistent state (vTPM, UEFI vars)
```

**Coexistence with existing bootloaders:**
- Enlil is just another UEFI boot entry alongside Windows Boot Manager, GRUB, systemd-boot
- Users can F12 to pick: Enlil (both OSes as VMs) or any existing OS (boots bare metal as usual)
- Installing Enlil to the existing ESP (`efibootmgr` to add a boot entry) makes it available without a USB drive
- At no point does Enlil modify or replace existing bootloaders

**Booting an existing Windows/Linux installation under Enlil:**
- **Safest method:** Pass the physical NVMe controller through via IOMMU + pass the physical GPU through
  - Guest sees identical hardware to bare metal (same CPU, same NVMe, same GPU)
  - Only the SMBIOS/ACPI environment changes (OVMF instead of motherboard firmware)
  - Windows may reconfigure some devices on first boot, may require reactivation
  - Data path is bare-metal (IOMMU passthrough — Enlil is NOT in the I/O path, no corruption risk)
- **Windows activation preservation:** Craft SMBIOS tables (Phase 5.2) to match the physical motherboard's vendor, product, serial — Windows won't notice the change
- **Risk levels:**
  - Full NVMe + GPU passthrough: **Low risk** — hardware data path is identical to bare metal
  - NVMe passthrough + virtual GPU: **Low risk** — boots to basic display, needs GPU driver
  - VirtIO disk on raw partition: **Medium risk** — needs VirtIO driver injection or Windows will BSOD (INACCESSIBLE_BOOT_DEVICE), but no data corruption; bare-metal boot still works after
- **Recommended safe testing sequence:**
  1. Boot Enlil from USB → launch a fresh Linux from ISO (proves Enlil works, zero risk)
  2. Boot Enlil from USB → launch a fresh Windows from ISO onto a spare drive (proves Windows guest works)
  3. Clone existing Windows to a spare drive → boot the clone under Enlil (tests hardware change on a copy)
  4. Once confident → point Enlil at real drives with full passthrough

**First-run setup wizard (`enlil-setup`):**
- A minimal TUI that runs as the first guest (tiny Linux initramfs)
- Walks the user through:
  - Hardware detection: CPUs, RAM, GPUs, NVMe drives, USB controllers, IOMMU groups
  - Guest definition: "How many OSes? Which CPUs per guest? How much RAM?"
  - Storage: "Which drive for which guest? Passthrough or virtual disk?"
  - GPU: "Passthrough this GPU to which guest? Enable iGPU SR-IOV?"
  - USB: "Which ports for which guest?"
  - Writes `config.toml`, reboots into the real configuration
- Can also import existing disk images or ISOs
- **Goal:** A user with a USB drive and two ISOs goes from zero to two running desktops in under 30 minutes

### 0.4 Documentation
- Write [`README.md`](README.md) documenting the crate structure, layer stack, and design decisions
- Document the KVM-to-bare-metal migration plan (the phase progression in this roadmap)
- Set up mdbook or similar for developer docs

---

## Phase 1 — Enlil Platform Layer & Custom `std` Target (HIGH PRIORITY)

**Goal:** Build a custom Rust platform layer that enables full `std` at the hypervisor level — including threads, async, synchronization, and collections — even when running bare-metal. This is foundational infrastructure that every subsequent phase benefits from.

**Duration:** 4–6 weeks

**Why early:** Every line of code written after this phase gets to use `std`. Every data structure, every synchronization primitive, every async task. If deferred, we'd accumulate thousands of lines of `no_std` workarounds that would later need rewriting. Investing here first pays compound returns.

### 1.1 Architecture: The Platform Abstraction Layer

```
┌─────────────────────────────────────────────────────┐
│  All Enlil Crates (full std Rust)                   │
│  enlil-core, enlil-devices, enlil-config, etc.      │
│                                                     │
│  • Vec, HashMap, BTreeMap, String                   │
│  • std::thread::spawn()                             │
│  • std::sync::{Mutex, RwLock, Arc, Condvar}         │
│  • async/await + tokio-style executor               │
│  • std::time::{Instant, Duration}                   │
│  • std::io (for serial, framebuffer, VirtIO)        │
│  • println!() / log macros                          │
├─────────────────────────────────────────────────────┤
│  Rust std (compiled for x86_64-unknown-enlil)       │
│                                                     │
│  std::sys → enlil-platform implementations          │
├─────────────────────────────────────────────────────┤
│  enlil-platform (the adaptation layer)              │
│                                                     │
│  ┌──────────────┐ ┌────────────┐ ┌───────────────┐ │
│  │ Memory       │ │ Threading  │ │ Sync          │ │
│  │              │ │            │ │               │ │
│  │ GlobalAlloc  │ │ Per-CPU    │ │ Spinlock +    │ │
│  │ → buddy/slab │ │ run queues │ │ ticket locks  │ │
│  │ allocator    │ │ + work     │ │ Condvar via   │ │
│  │              │ │ stealing   │ │ event queues  │ │
│  └──────────────┘ └────────────┘ └───────────────┘ │
│  ┌──────────────┐ ┌────────────┐ ┌───────────────┐ │
│  │ Time         │ │ I/O        │ │ Async         │ │
│  │              │ │            │ │               │ │
│  │ TSC/HPET     │ │ Trait-     │ │ Lightweight   │ │
│  │ calibration  │ │ based I/O  │ │ executor      │ │
│  │ Instant →    │ │ backends   │ │ (no OS dep)   │ │
│  │ monotonic    │ │ (serial,   │ │ Waker → APIC  │ │
│  │ clock        │ │ framebuf)  │ │ IPI or event  │ │
│  └──────────────┘ └────────────┘ └───────────────┘ │
├─────────────────────────────────────────────────────┤
│  Backend (swappable):                               │
│  • Phase 0–5: Linux syscalls (development mode)     │
│  • Phase 6+: Bare-metal hardware (production mode)  │
└─────────────────────────────────────────────────────┘
```

### 1.2 Custom Target Triple: `x86_64-unknown-enlil`

Create a custom target specification for Rust:
```json
{
  "llvm-target": "x86_64-unknown-none",
  "data-layout": "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-f80:128-n8:16:32:64-S128",
  "arch": "x86_64",
  "target-endian": "little",
  "target-pointer-width": "64",
  "target-c-int-width": "32",
  "os": "enlil",
  "executables": true,
  "linker-flavor": "ld.lld",
  "linker": "rust-lld",
  "panic-strategy": "abort",
  "disable-redzone": true,
  "features": "+soft-float",
  "has-thread-local": true,
  "position-independent-executables": true
}
```

- Use `-Z build-std=std,core,alloc,panic_abort` to cross-compile `std` for our target
- Implement the `std::sys` platform bindings in `enlil-platform`
- Reference: Redox OS `relibc` and `redox_syscall` crate for how they wired `std` to their kernel

### 1.3 Memory Subsystem

**Global Allocator:**
- Implement a hybrid allocator: buddy allocator for large allocations (page-granularity), slab allocator for small objects (power-of-two size classes)
- Register via `#[global_allocator]` so `Vec`, `Box`, `String`, `HashMap` all work
- Port from Redox OS: their `ralloc` allocator is Rust-native and designed for OS-level use
- Support per-CPU arena allocators for hot-path allocation-free operation where needed

**Physical Memory Manager:**
- Takes the memory map from UEFI (or from Linux `/proc/iomem` during KVM development)
- Carves regions: hypervisor heap, guest RAM pools, DMA-safe zones, MMIO regions
- Tracks free/used pages with a bitmap or buddy tree
- Provides `PhysFrame` and `VirtAddr` types with safe conversion

**Virtual Memory:**
- Manage host page tables (CR3 for the hypervisor itself)
- Support `mmap`-like semantics for `std::io` memory-mapped operations
- Guard pages for stack overflow detection on hypervisor threads

### 1.4 Threading

This is where Enlil diverges most from a traditional OS kernel. We need threads not for user-space programs but for hypervisor-internal tasks: vCPU run loops, device backend I/O handlers, compute fabric JIT compilation, management console.

**Thread Model:**
```
Physical Core 0          Physical Core 1          Physical Core N
┌─────────────────┐     ┌─────────────────┐     ┌─────────────────┐
│ Per-CPU Scheduler│     │ Per-CPU Scheduler│     │ Per-CPU Scheduler│
│                 │     │                 │     │                 │
│ ┌─────────────┐ │     │ ┌─────────────┐ │     │ ┌─────────────┐ │
│ │ Run Queue   │ │     │ │ Run Queue   │ │     │ │ Run Queue   │ │
│ │             │ │     │ │             │ │     │ │             │ │
│ │ • vCPU task │ │     │ │ • vCPU task │ │     │ │ • Device IO │ │
│ │ • Device IO │ │     │ │ • JIT comp  │ │     │ │ • Mgmt TUI  │ │
│ │ • async poll│ │     │ │ • async poll│ │     │ │ • async poll│ │
│ └─────────────┘ │     └─────────────┘ │     │ └─────────────┘ │
│                 │     │                 │     │                 │
│ Work Stealing ◄─┼─────┼─► Work Stealing◄┼─────┼─► Work Stealing│
└─────────────────┘     └─────────────────┘     └─────────────────┘
```

- **Per-CPU run queues** with work-stealing (borrow from Redox's scheduler concepts and Tokio's work-stealing runtime architecture)
- Each physical core has a scheduler that multiplexes between:
  - **vCPU tasks:** Run a guest vCPU (VMLAUNCH/VMRESUME). These are the highest priority.
  - **Device tasks:** VirtIO backends, USB polling, GPU command processing
  - **Compute tasks:** Fabric JIT compilation, SPIR-V analysis
  - **System tasks:** Management console, logging, metrics
- Preemption via APIC timer interrupts
- **`std::thread::spawn()`** creates a new task, places it on the current core's run queue
- **Work stealing:** idle cores steal tasks from busy cores' queues (lock-free deque, à la Chase-Lev)
- Priority levels: `Critical` (vCPU) > `High` (device I/O) > `Normal` (compute) > `Low` (management)
- **Thread-local storage:** Use `GS` segment base per-CPU for fast TLS access (same mechanism Linux uses)

**Porting from Redox:**
- Redox's `kernel/src/scheme/` and `kernel/src/context/` implement context switching in pure Rust
- Their `switch_to()` saves/restores register state, FPU/SSE/AVX state via XSAVE
- Adapt their context switch to also handle VMX state (VMCS pointer, host/guest state areas)
- Their `WaitCondition` and `WaitQueue` types map to `std::sync::Condvar` semantics

### 1.5 Synchronization Primitives

Implement the primitives that `std::sync` compiles down to:

**Mutex:**
- Ticket lock for short critical sections (no scheduler interaction needed)
- Sleeping mutex for longer holds: thread yields and is woken via the scheduler's wait queue
- `std::sync::Mutex<T>` compiles against our implementation — all safety guarantees preserved
- Poison detection: track owning thread, set poison flag on panic

**RwLock:**
- Read-preferring or write-preferring (configurable)
- Multiple concurrent readers, exclusive writer
- Critical for: guest configuration (read often, rarely modified), kernel cache (read-heavy)

**Condvar:**
- Wait queue per condvar instance
- `wait()` atomically releases mutex + parks thread on wait queue
- `notify_one()` / `notify_all()` move threads from wait queue back to run queue
- Basis for channels, barriers, and other higher-level sync

**Atomic operations:**
- Rust's `std::sync::atomic` compiles directly to x86 `LOCK` prefix instructions — no platform layer needed
- `Arc<T>` works automatically (it's built on atomics)

**Channels (bonus):**
- Implement `std::sync::mpsc` — multi-producer, single-consumer channels
- Also provide `crossbeam`-style MPMC channels for the device backend event bus
- Lock-free where possible (compare-and-swap based queues)

### 1.6 Async Runtime

Build a lightweight async executor that runs at the hypervisor level. This is extremely valuable for I/O-heavy subsystems (VirtIO backends, USB polling, network switching, management console).

**Lesson from Microsoft OpenVMM (2024):** OpenVMM's team found that traditional thread-based VMM designs introduce jitter into guest scheduling. They built a custom async-first architecture specifically because fine-grained scheduling control was essential. Enlil's executor must learn from this.

**Architecture:**
```
┌────────────────────────────────────────────┐
│  async fn handle_virtio_request() {        │
│      let req = virtio_queue.next().await;  │
│      let result = process(req).await;      │
│      virtio_queue.complete(result).await;  │
│  }                                         │
├────────────────────────────────────────────┤
│  Enlil Async Executor                      │
│                                            │
│  • Per-CPU task queues (work-stealing)     │
│  • Priority-aware: vCPU > Device > Compute │
│  • Waker → sets "ready" flag + IPI         │
│  • Reactor: APIC timer + interrupt-driven  │
│  • Priority inheritance on lock contention │
│  • Integrates with thread scheduler        │
└────────────────────────────────────────────┘
```

- **Executor model:** Integrate with the per-CPU thread scheduler. Async tasks are cooperative (they yield at `.await` points). The scheduler runs the next ready task.
- **Priority-aware task scheduling (critical — from OpenVMM lessons):**
  - Tasks are tagged with priority levels: `Critical` (vCPU run loops), `High` (device I/O), `Normal` (compute fabric JIT), `Low` (management, logging)
  - A `Critical` task becoming ready can preempt a `Normal` task mid-execution (at the next `.await` point)
  - **Priority inheritance:** If a `Low`-priority task holds a mutex that a `Critical` task needs, temporarily boost the low-priority task to `Critical` to avoid priority inversion
  - This prevents device I/O backends from adding scheduling jitter to vCPU tasks
- **Waker implementation:** When an async task is waiting on I/O (e.g., VirtIO queue notification), the waker is triggered by the hardware interrupt handler. The interrupt sets the task as "ready" and sends an IPI (Inter-Processor Interrupt) if the task's CPU is running something else.
- **No full Tokio dependency** — Tokio is too heavy and assumes Linux. Instead, build a minimal executor inspired by:
  - `async-task` crate (lightweight task abstraction, no OS deps)
  - `smol` runtime architecture (simple, small, well-structured)
  - Redox's event-driven I/O model
  - Microsoft OpenVMM's task scheduling design (study `openvmm/` source)
- **`futures` crate** works as-is (it's `no_std` compatible) — gives us `Future`, `Stream`, `Sink` traits
- `async-channel` for async MPMC channels between subsystems

**What this enables:**
```rust
// VirtIO block device backend — clean, readable, efficient
async fn virtio_blk_handler(queue: VirtioQueue, disk: DiskBackend) {
    loop {
        let request = queue.next_request().await;
        match request.kind {
            Read { sector, count } => {
                let data = disk.read(sector, count).await;
                queue.complete(request, data).await;
            }
            Write { sector, data } => {
                disk.write(sector, &data).await;
                queue.complete_ok(request).await;
            }
        }
    }
}

// USB hot-plug monitor — runs as a long-lived async task
async fn usb_hotplug_monitor(xhci: XhciController, router: UsbRouter) {
    let mut events = xhci.port_status_changes();
    while let Some(event) = events.next().await {
        match event {
            PortConnect(port, device) => {
                let target_guest = router.resolve(&device);
                router.attach(device, target_guest).await;
                log::info!("USB {} → Guest {}", device, target_guest);
            }
            PortDisconnect(port) => {
                router.detach(port).await;
            }
        }
    }
}
```

### 1.7 Time

- Calibrate TSC frequency at boot (using CPUID leaf 0x15 or PIT-based calibration)
- Optionally calibrate against HPET for precision
- `std::time::Instant::now()` → reads TSC, converts to nanoseconds
- `std::time::Duration` works as-is (it's pure math)
- `std::thread::sleep()` → parks thread, sets APIC timer to wake after duration
- Provide a monotonic clock guarantee (TSC is monotonic on modern CPUs with `constant_tsc` + `nonstop_tsc` flags)

### 1.8 I/O Trait Layer

`std::io::Read` / `std::io::Write` need backing implementations. Provide a trait-based I/O registry:

```rust
// Platform I/O trait — backends register themselves
pub trait PlatformIo: Send + Sync {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn write(&self, buf: &[u8]) -> io::Result<usize>;
}

// Serial console (early boot, always available)
struct SerialPort { base: u16 }
impl PlatformIo for SerialPort { /* port I/O */ }

// Framebuffer console (after UEFI GOP init)
struct FramebufferConsole { fb: &'static mut [u8], width: usize, height: usize }
impl PlatformIo for FramebufferConsole { /* pixel writing + text renderer */ }

// This powers println!(), log::info!(), etc.
```

- `print!` / `println!` macros work from day one (output to serial in early boot, framebuffer later)
- `log` crate integration: `log::info!()`, `log::debug!()`, etc. with filtering
- Structured logging with per-subsystem tags (e.g., `[vcpu:0]`, `[usb]`, `[fabric]`)

### 1.9 Dual-Backend Strategy

The platform layer has **two backends**, swappable at compile time via Cargo features:

```toml
[features]
default = ["platform-linux"]
platform-linux = []      # Phase 0–5: backends to Linux syscalls
platform-baremetal = []  # Phase 6+:  backends to Enlil hardware primitives
```

- **`platform-linux`:** `GlobalAlloc` → `mmap`, threads → `pthread_create`, mutex → `futex`, time → `clock_gettime`. This means during KVM development, everything works on Linux but the code is written against our platform abstractions.
- **`platform-baremetal`:** `GlobalAlloc` → buddy allocator on physical memory, threads → per-CPU scheduler, mutex → spinlock/sleep-lock, time → TSC. Flipping this feature flag is the Phase 6 bare-metal transition.

The key insight: **all Enlil code above the platform layer is identical in both modes.** The vCPU scheduler, VirtIO backends, USB router, compute fabric, management console — all of it compiles and runs the same whether the platform backend is Linux or bare metal.

### 1.10 What We Port From Redox OS

Redox is the primary reference. Specific components to study and adapt:

| Redox Component | Location | What We Take |
|----------------|----------|--------------|
| `relibc` | `redox-os/relibc` | POSIX C library in Rust — shows how to wire `std::sys` to a custom kernel |
| `ralloc` | `redox-os/ralloc` | Pure Rust allocator — port directly for `GlobalAlloc` |
| Context switching | `kernel/src/context/` | Register save/restore, XSAVE, stack management — adapt for VMX state |
| Scheduler | `kernel/src/context/switch.rs` | Round-robin + priority scheduling — extend with work-stealing |
| Sync primitives | `kernel/src/sync/` | Wait queues, conditions — map to `std::sync` semantics |
| Event system | `kernel/src/scheme/event.rs` | Interrupt-driven I/O wakeup — basis for our async reactor |
| Time | `kernel/src/time.rs` | TSC calibration, monotonic clock — use directly |

### 1.11 Milestone

- Custom target `x86_64-unknown-enlil` compiles `std` successfully
- A test binary (running on Linux via the `platform-linux` backend) uses `std::thread::spawn`, `std::sync::Mutex`, `Vec`, `HashMap`, `async/await`, `println!()`, and `std::time::Instant` — all working through our platform layer
- Same test binary compiles for `platform-baremetal` (even if it can't run yet — proving the abstraction compiles)
- All subsequent phases use full `std` Rust from this point forward

---

## Phase 2 — Multi-Guest CPU & Memory Partitioning

**Goal:** Run two Linux guests simultaneously on the same host, each with dedicated CPU cores and isolated memory.

**Duration:** 4–6 weeks

### 2.1 Guest Configuration System
- Define a TOML/YAML config format:
  ```toml
  [guest.linux1]
  name = "Linux Workstation"
  cpus = [0, 1, 2, 3]          # Physical cores to pin
  memory_mb = 8192
  kernel = "/path/to/bzImage"
  initrd = "/path/to/initramfs"
  cmdline = "console=ttyS0 root=/dev/vda"

  [guest.linux2]
  name = "Linux Dev"
  cpus = [4, 5, 6, 7]
  memory_mb = 8192
  kernel = "/path/to/bzImage2"
  ```
- Parse config in `enlil-config`, validate no CPU/memory overlap

### 2.2 CPU Scheduling — Core Dedication
- For each guest, create N vCPUs and pin each to a physical core using `sched_setaffinity`
- Each vCPU runs in its own OS thread, pinned 1:1 to a physical core
- Expose only the assigned cores via crafted CPUID responses
- Handle CPUID interception to report correct topology (core count, package, cache)

### 2.3 CPU Scheduling — Time-Slicing Fallback
- When physical cores < total requested vCPUs, implement time-slicing:
  - Use a cooperative scheduler: each vCPU gets a time quantum (e.g., 10ms)
  - On quantum expiry, save full vCPU state (registers, MSRs, FPU/SSE/AVX via XSAVE)
  - Context-switch to next vCPU on that physical core
  - Use APIC timer or TSC deadline for preemption
- Config option: `scheduling = "dedicated" | "timeslice" | "auto"`
  - `auto`: dedicate when possible, timeslice remaining

### 2.4 Memory Isolation

**Design for future phases:** The EPT/NPT page table management code must support per-page write-protection toggling and dirty page bitmaps from day one. Three later phases depend on this: Phase 8.12 (snapshots use copy-on-write via EPT write-protect faults), Phase 11.5.1 (live migration uses iterative dirty page transfer), and Phase 11.10 (fault tolerance uses continuous dirty page streaming). Implement `ept_set_write_protect(guest, gpa)`, `ept_clear_write_protect(guest, gpa)`, and `ept_get_and_clear_dirty_bitmap(guest) → BitVec` as first-class operations on the memory manager, not afterthoughts.

- Use KVM's memory slot mechanism to give each guest a contiguous physical memory region
- Each guest sees memory starting at physical address 0 (via EPT/NPT)
- Implement a simple physical memory allocator in `enlil-core` that carves host RAM into guest regions
- Reserve memory for the hypervisor itself (management console, device backends, page tables)

### 2.5 Per-Guest Serial Console
- Each guest gets its own emulated COM1 (via `vm-superio`)
- Multiplex output to separate PTYs or a tmux-style management console
- **Milestone:** Two Linux guests running simultaneously, each with their own shell over serial, on dedicated cores

---

## Phase 3 — Virtual Device Layer & Storage

**Goal:** Give each guest block devices and network so they're usable systems, not just serial consoles.

**Duration:** 4–6 weeks

### 3.1 VirtIO Block Device

**Design for future phases:** The VirtIO-blk backend must use an abstract `StorageBackend` trait from day one. Phase 11.10a (Enlil Storage Pool) replaces single-file storage with a distributed block pool across multiple machines. If the VirtIO-blk code is hard-wired to qcow2 file I/O, the entire storage layer must be rewritten. Define the trait now:

```rust
trait StorageBackend: Send + Sync {
    async fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize>;
    async fn write(&self, offset: u64, buf: &[u8]) -> Result<usize>;
    async fn flush(&self) -> Result<()>;
    async fn trim(&self, offset: u64, len: u64) -> Result<()>;
    fn capacity(&self) -> u64;
}
```

Phase 3 implements `QcowBackend` and `RawFileBackend`. Phase 11 implements `PoolBackend`. The VirtIO-blk device code is identical in both cases.

- Use `vm-virtio` to implement virtio-blk backends
- Each guest config specifies disk images or raw partitions:
  ```toml
  [guest.linux1.disks]
  vda = { path = "/dev/nvme0n1p3", readonly = false }
  vdb = { path = "/images/data.qcow2", readonly = false }
  ```
- Support raw images and (stretch) qcow2
- For NVMe passthrough (better perf): use VFIO to pass entire NVMe namespaces when IOMMU is available

### 3.2 VirtIO Network
- Implement virtio-net backends
- Create a software virtual switch in the hypervisor:
  - Each guest gets a virtual NIC connected to the switch
  - Switch forwards to a host TAP device for external connectivity
  - Inter-guest traffic stays in-hypervisor (fast path)
- Support SR-IOV NIC passthrough when available (many server NICs, some desktop NICs)
- Config:
  ```toml
  [guest.linux1.net]
  eth0 = { mode = "bridge", bridge = "br0", mac = "52:54:00:01:00:01" }
  ```

### 3.3 Interrupt Virtualization
- Set up virtual IOAPIC and local APIC for each guest
- Use KVM's irqchip or implement split irqchip for finer control
- Configure MSI/MSI-X passthrough for assigned devices
- Intel: enable APICv / posted interrupts for direct interrupt delivery
- AMD: enable AVIC equivalent

### 3.4 Virtual Timer & Clock
- Provide each guest with:
  - Emulated PIT (i8254) — legacy, needed for BIOS-era boot
  - Emulated HPET
  - TSC offsetting so each guest's TSC starts at 0
  - KVM clock / Hyper-V reference TSC (for Linux/Windows paravirt clocks)
- Ensure RDTSC doesn't leak host timing (use TSC offset in VMCS)

### 3.5 Management Console v1
- Build a TUI (using `ratatui`) in `enlil-mgmt` that shows:
  - Running guests and their CPU/memory usage
  - Serial console access (tab between guests)
  - Basic controls: start, stop, reboot guest
- Connect via Unix socket from `enlil-core`
- **Milestone:** Two Linux guests with disks, networking, and a management TUI

### 3.6 Display Compositor — "Enlil Zones" (CRITICAL FOR SINGLE-MONITOR AND LAPTOP USE)

**Design for future phases:** The compositor must consume framebuffers via an abstract `FramebufferSource` trait, not hard-wire to local capture methods. Phase 11.9 adds remote framebuffer sources streamed over the mesh network (LZ4 on LAN, H.264/H.265 on WAN). If the compositor only understands local IVSHMEM or VirtIO-GPU render targets, adding network sources requires refactoring. Define the trait now:

```rust
trait FramebufferSource: Send + Sync {
    fn resolution(&self) -> (u32, u32);
    fn format(&self) -> PixelFormat;
    fn acquire_frame(&self) -> Result<FrameRef>;  // zero-copy for local, decoded buffer for network
    fn release_frame(&self, frame: FrameRef);
}
```

Phase 3 implements `IvshmemSource`, `VirtioGpuSource`, `CompositorOwnedSource`. Phase 11 adds `NetworkStreamSource`.

Without display composition, Enlil requires one physical monitor per guest. This rules out laptops entirely and makes single-monitor desktops awkward. The display compositor is what makes Enlil feel like a product, not a tech demo.

**Concept: FancyZones, but for entire operating systems.**

Instead of snapping *windows* into screen zones, you snap *entire guest OS desktops* into zones on your physical display(s). Each zone renders a guest's framebuffer, scaled to fit the zone dimensions. The user defines zone layouts — just like PowerToys FancyZones — and assigns guests to zones.

**Architecture:**
```
Physical Display(s)
         │
┌────────┴──────────────────────────────────────────┐
│              ENLIL DISPLAY COMPOSITOR              │
│                                                    │
│  ┌──────────────────────────────────────────────┐  │
│  │              Zone Layout Engine               │  │
│  │                                               │  │
│  │  Reads layout definitions from config.toml    │  │
│  │  Maps each zone to a guest's framebuffer      │  │
│  │  Handles resolution scaling per zone          │  │
│  │  Hotkey: cycle layouts instantly               │  │
│  └──────────────────┬───────────────────────────┘  │
│                     │                              │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐            │
│  │ Source 1 │  │ Source 2 │  │ Source 3 │           │
│  │ Guest A  │  │ Guest B  │  │ Mgmt TUI│           │
│  │ frame-   │  │ frame-   │  │ frame-  │           │
│  │ buffer   │  │ buffer   │  │ buffer  │           │
│  └─────────┘  └─────────┘  └─────────┘            │
│                                                    │
│  Framebuffer sources:                              │
│  • Passthrough GPU: capture via shared memory      │
│  • SR-IOV VF: read from VF's framebuffer           │
│  • VirtIO-GPU: compositor owns the render target   │
│  • Mediated/timeslice: hypervisor owns VRAM        │
└────────────────────────────────────────────────────┘
```

**Zone Layout Modes (inspired by FancyZones):**

```
┌─────────────────────────────┐    ┌─────────────────────────────┐
│                             │    │              │              │
│          FULLSCREEN         │    │   50 / 50    │   50 / 50   │
│         (Guest A)           │    │   (Guest A)  │  (Guest B)  │
│                             │    │              │              │
│   Hotkey to switch guest    │    │              │              │
└─────────────────────────────┘    └─────────────────────────────┘

┌─────────────────────────────┐    ┌─────────────────────────────┐
│                    │        │    │         │                   │
│    70 / 30         │ Guest  │    │ Guest A │                   │
│    (Guest A)       │   B    │    │ (small) │     Guest B       │
│                    │        │    │         │     (large)       │
│                    │        │    │         │                   │
└─────────────────────────────┘    └─────────────────────────────┘

┌─────────────────────────────┐    ┌─────────────────────────────┐
│           Guest A           │    │  Guest A  │  Guest B        │
├──────────┬──────────────────┤    ├───────────┤                 │
│ Guest B  │    Guest C       │    │  Guest C  │                 │
│          │    (or Mgmt)     │    │           │                 │
└──────────┴──────────────────┘    └───────────┴─────────────────┘

CUSTOM: user-defined grid/canvas layout (à la FancyZones editor)
```

**Multi-monitor support:**
- Each physical monitor gets its own independent zone layout
- A guest can span multiple monitors (zone stretches across displays)
- Or each monitor shows a different guest fullscreen
- Layout is per-monitor, stored in config.toml

**Implementation — Framebuffer Capture by GPU Strategy:**

| GPU Strategy | How Compositor Gets the Framebuffer |
|---|---|
| Full passthrough | Guest renders to physical GPU. Compositor captures via IVSHMEM-style shared memory (Looking Glass approach). Requires a lightweight agent in the guest OR use the GPU's display capture hardware (NVIDIA NVFBC, AMD VCE). |
| SR-IOV | Each VF has its own framebuffer. Compositor reads directly from VF's VRAM via the PF driver. No guest agent needed — the hypervisor owns the PF. |
| Mediated passthrough | Compositor owns the physical GPU. Each guest renders to a virtual framebuffer in VRAM that the compositor controls. Zero-copy composition. |
| Time-sliced | Same as mediated — compositor owns the framebuffer. |
| VirtIO-GPU (Venus/VirGL) | Compositor IS the render target. Guest sends Vulkan/OpenGL commands, compositor renders them into zone-sized render targets. Native integration. |

**The zero-copy fast path (mediated, time-sliced, VirtIO-GPU):** When Enlil owns the GPU (which is most modes except full passthrough), the compositor can render each guest's framebuffer directly as a texture in a composition pass. This is a single GPU draw call per frame — compositing overhead is negligible. Think of it like a Wayland compositor that composites windows, except each "window" is an entire OS.

**The passthrough path (full passthrough, SR-IOV):** When the guest owns its own GPU/VF, capturing the framebuffer requires either a guest-side agent (like Looking Glass) that copies the framebuffer to shared memory, or hardware capture. Enlil's architectural advantage: it can inject the shared memory region into the guest's address space without the guest knowing (EPT mapping), and the guest agent can be a tiny invisible driver — not a full application.

**Input routing to zones:**
- Mouse position determines which zone (and thus which guest) receives input
- Click inside a zone → that guest becomes the "focused" guest and receives all keyboard input
- Mouse moves between zones seamlessly — when the cursor crosses a zone boundary, input switches to the new guest's virtual mouse
- Hotkey (e.g., Ctrl+Ctrl double-tap) forces focus to a specific guest regardless of mouse position
- Each zone's guest sees its own mouse cursor at the correct position (scaled to the guest's native resolution)

**Configuration:**
```toml
[display]
mode = "zones"             # zones | dedicated | switcher

# Predefined layouts
[display.layouts.default]
name = "Side by Side"
zones = [
  { guest = "linux",   x = 0,    y = 0, width = 50, height = 100 },   # left half
  { guest = "windows", x = 50,   y = 0, width = 50, height = 100 },   # right half
]

[display.layouts.coding]
name = "Code Focus"
zones = [
  { guest = "linux",   x = 0,  y = 0, width = 70, height = 100 },     # main
  { guest = "windows", x = 70, y = 0, width = 30, height = 100 },     # sidebar
]

[display.layouts.fullscreen]
name = "Fullscreen Switcher"
zones = [
  { guest = "linux",   x = 0, y = 0, width = 100, height = 100 },     # fullscreen
]
# Ctrl+Ctrl cycles to next guest in fullscreen

# Hotkeys
[display.hotkeys]
cycle_layout = "Ctrl+Shift+Z"     # cycle through defined layouts
next_guest = "Ctrl+Ctrl"          # in fullscreen mode, switch guest
focus_guest_1 = "Ctrl+Alt+1"      # focus Linux zone
focus_guest_2 = "Ctrl+Alt+2"      # focus Windows zone
toggle_pip = "Ctrl+Alt+P"         # toggle picture-in-picture overlay

# Per-monitor layouts
[display.monitors.HDMI-1]
layout = "default"

[display.monitors.DP-1]
layout = "fullscreen"              # second monitor shows one guest fullscreen
```

**Stretch features:**
- **Picture-in-picture:** Small overlay of one guest on top of another (like a TV PiP). Hotkey toggles visibility and position.
- **Zone editor TUI:** Interactive zone editor in the management console — drag zone boundaries with mouse, save layout.
- **Resolution independence:** Each guest runs at its preferred native resolution. The compositor scales each framebuffer to fit its zone using GPU hardware scaling (bilinear/nearest-neighbor configurable). A 4K guest in a half-screen zone gets scaled cleanly.
- **Animated transitions:** Smooth animation when switching layouts (zones slide/resize over ~200ms). Makes layout switching feel polished.
- **Wallpaper/gap:** Configurable gap between zones with a wallpaper visible behind — visual separation between the two OSes.

**Why this is better than Looking Glass:**
- Looking Glass requires a guest-side application, a separate SPICE connection for input, and a host-side client application. It's a 3-piece stack.
- Enlil Zones is integrated into the hypervisor. No guest agent needed in most GPU modes. Input routing is handled at the hypervisor level. The compositor is a single component that owns the display pipeline.
- Looking Glass only works with GPU passthrough. Enlil Zones works with ALL GPU strategies including VirtIO-GPU, mediated, and time-sliced.

### 3.7 Inter-Guest Communication — "Enlil Bridge"

**Design for future phases:** All bridge subsystems must use an abstract `BridgeTransport` trait for message delivery. Phase 11 extends every bridge feature (clipboard, drag-drop, shared-fs, notifications, URL routing) across the mesh network to guests on different physical machines. If the bridge is hard-wired to local VirtIO queue calls, adding network transport requires refactoring every subsystem. Define the trait now:

```rust
trait BridgeTransport: Send + Sync {
    async fn send(&self, target_guest: GuestId, channel: BridgeChannel, msg: &[u8]) -> Result<()>;
    async fn recv(&self, channel: BridgeChannel) -> Result<(GuestId, Vec<u8>)>;
}
```

Phase 3 implements `LocalVirtioTransport` (direct VirtIO queue dispatch within one machine). Phase 11 adds `MeshNetworkTransport` (serialization over WireGuard/TCP to remote nodes). The bridge subsystem code (clipboard logic, drag-drop logic, etc.) is identical in both cases.

**Goal:** Make two OSes on one machine feel like one cohesive workstation. Without inter-guest communication, Enlil is two isolated PCs in a box. With it, it's a seamless multi-OS desktop.

**Architecture:**
```
┌────────────┐                              ┌────────────┐
│  Guest A   │                              │  Guest B   │
│  (Linux)   │                              │  (Windows) │
│            │                              │            │
│ ┌────────┐ │    ┌──────────────────┐      │ ┌────────┐ │
│ │ Enlil  │ │    │  ENLIL BRIDGE    │      │ │ Enlil  │ │
│ │ Bridge │◄├────┤  (hypervisor)    ├──────►│ Bridge │ │
│ │ Agent  │ │    │                  │      │ │ Agent  │ │
│ └────────┘ │    │  • Clipboard hub │      │ └────────┘ │
│            │    │  • VirtIO-fs     │      │            │
│  VirtIO    │    │  • Virtual switch│      │  VirtIO    │
│  queues    │    │  • Drag-drop mgr │      │  queues    │
│            │    │  • URL router    │      │            │
│            │    │  • Notify bridge │      │            │
└────────────┘    └──────────────────┘      └────────────┘
```

All inter-guest communication flows through the hypervisor via VirtIO queues. The hypervisor is the trusted intermediary — guests never communicate directly (preserving isolation guarantees). Each guest runs a lightweight "Enlil Bridge Agent" that interfaces with the OS-specific APIs (clipboard, drag-and-drop, notifications, etc.) and talks to the hypervisor over VirtIO.

**VirtIO Bridge Device:**
```
VirtIO Device ID: (custom, e.g., 0x4E42 — "NB" for eNlil Bridge)

Queues:
  Queue 0: ClipboardTx    (guest → hypervisor: clipboard content)
  Queue 1: ClipboardRx    (hypervisor → guest: clipboard content from other guest)
  Queue 2: DragDropTx     (guest → hypervisor: drag initiation + file references)
  Queue 3: DragDropRx     (hypervisor → guest: drop data from other guest)
  Queue 4: NotifyTx       (guest → hypervisor: notification to forward)
  Queue 5: NotifyRx       (hypervisor → guest: notification from other guest)
  Queue 6: ControlTx      (guest → hypervisor: URL/protocol handler requests)
  Queue 7: ControlRx      (hypervisor → guest: control responses)
```

#### 3.7.1 Clipboard Bridge

**What it does:** Copy text, images, or file references in Guest A → paste in Guest B. Works bidirectionally. Feels like a single computer.

**Implementation — Hypervisor side:**
- Maintains a shared clipboard buffer (text, rich text, HTML, images, file URI lists)
- When Guest A pushes new clipboard content via ClipboardTx queue, the hypervisor stores it and pushes it to Guest B via ClipboardRx queue
- Supports multiple MIME types simultaneously (plain text + HTML + image — guest picks what it needs)
- Maximum clipboard size: configurable (default 64MB — handles large images)
- Optional: clipboard history ring (last N items, accessible via hotkey)

**Implementation — Guest Agent (Linux):**
- Listens on Wayland clipboard protocol (`zwlr_data_control_manager_v1`) or X11 selection (XCB)
- On clipboard change: serialize content → push to VirtIO ClipboardTx queue
- On incoming clipboard from hypervisor: inject into Wayland/X11 clipboard
- Runs as a user-space daemon — no kernel module needed
- ~500 lines of Rust

**Implementation — Guest Agent (Windows):**
- Uses Win32 clipboard API (`AddClipboardFormatListener`, `SetClipboardData`, `GetClipboardData`)
- Monitors clipboard changes via `WM_CLIPBOARDUPDATE` messages
- On change: serialize content → push to VirtIO ClipboardTx queue
- On incoming: call `OpenClipboard` → `EmptyClipboard` → `SetClipboardData` → `CloseClipboard`
- Runs as a system tray application or Windows service
- ~400 lines of Rust (compiled to Windows via cross-compilation)

**Supported content types:**

| Type | MIME | Linux Format | Windows Format |
|------|------|-------------|----------------|
| Plain text | `text/plain` | `UTF8_STRING` | `CF_UNICODETEXT` |
| Rich text | `text/html` | `text/html` | `CF_HTML` |
| Images | `image/png` | `image/png` selection | `CF_DIB` / `CF_BITMAP` |
| File references | `text/uri-list` | `text/uri-list` | `CF_HDROP` (converted to shared-fs paths) |

**File reference handling:** When you copy a file path in Linux, the clipboard contains a `file://` URI. The hypervisor translates this to a path on the shared filesystem (Phase 3.7.3) so Windows can access it. E.g., `file:///home/user/doc.pdf` → the hypervisor copies the file to the shared folder → Windows clipboard receives `\\enlil-shared\clipboard\doc.pdf`.

**Security:**
- Clipboard content passes through the hypervisor — it can enforce policies (e.g., block clipboard sharing for specific guests, size limits, content filtering)
- Under CVM mode (Phase 8.7), clipboard sharing requires explicit guest opt-in (shared memory pages designated as unencrypted)
- Config option to disable clipboard sharing entirely per-guest

**Config:**
```toml
[bridge.clipboard]
enabled = true
max_size_mb = 64
history_size = 10              # number of items in clipboard history ring
file_copy_to_shared = true     # auto-copy referenced files to shared-fs
```

#### 3.7.2 Drag-and-Drop Between Guests

**What it does:** Drag a file from a Linux file manager, drop it onto a Windows application (or vice versa). The file is transferred through the shared filesystem.

**How it works with Enlil Zones (Phase 3.6):**
- The display compositor (Enlil Zones) knows which zone the mouse cursor is in
- When a drag operation begins in Guest A (detected by the bridge agent), the compositor shows a visual drag indicator
- As the cursor crosses into Guest B's zone, the compositor notifies Guest B's bridge agent that a drop is incoming
- On drop: the hypervisor copies the file to the shared filesystem and delivers the file path to Guest B's drop target

**Implementation — Hypervisor side:**
- Tracks drag state: `Idle` → `Dragging(source_guest, payload)` → `Hovering(target_guest)` → `Dropped`
- On drag start from Guest A: receives file URI + preview thumbnail via DragDropTx
- On cursor entering Guest B's zone: sends drag-enter event to Guest B via DragDropRx
- On drop: copies file to shared-fs (if not already there), sends file path to Guest B

**Implementation — Guest Agent (Linux):**
- Integrates with XDG drag-and-drop (via XDnD protocol on X11, or `wl_data_device` on Wayland)
- On drag initiation: captures source URI and MIME type, sends to hypervisor
- On incoming drop: accepts the drop by injecting a synthetic drop event with the shared-fs file path

**Implementation — Guest Agent (Windows):**
- Implements `IDropSource` and `IDropTarget` COM interfaces
- Registers as a global drop target overlay (transparent window that catches drops when the cursor enters from outside the guest's zone)
- On incoming drop: delivers the file via `CF_HDROP` with the shared-fs UNC path

**Limitations:**
- Drag-and-drop only works when Enlil Zones compositor is active (not in dedicated-monitor mode, because the hypervisor doesn't control the display)
- Cross-guest drag-and-drop of application-specific data (e.g., dragging a Photoshop layer) isn't supported — only files and text
- Large file transfers during drag may have a brief delay while copying to shared-fs

**Config:**
```toml
[bridge.drag_drop]
enabled = true
auto_copy = true               # copy files to shared-fs on drop (vs. move)
show_preview = true            # show thumbnail preview during cross-zone drag
```

#### 3.7.3 Shared Filesystem (VirtIO-fs)

**What it does:** A shared folder that both guests can read/write to simultaneously. The backbone for file transfers, clipboard file references, and drag-and-drop.

**Implementation:**
- Present a VirtIO-fs device to each guest backed by a hypervisor-managed directory
- VirtIO-fs protocol (defined by virtio spec 1.2+): FUSE-over-VirtIO with DAX (Direct Access) for mmap support
- Linux guest: `mount -t virtiofs enlil-shared /mnt/shared` (auto-mount via systemd unit included in bridge agent package)
- Windows guest: WinFsp (Windows File System Proxy) provides VirtIO-fs support as a user-space filesystem driver. The Enlil bridge agent installer bundles WinFsp and auto-configures the mount as drive letter `Z:` (or user-configurable)
- **Backing storage options:**
  - Directory on the ESP (simplest — works with USB live boot)
  - Dedicated partition on a physical drive (better performance)
  - RAM-backed tmpfs (fastest, but volatile — good for clipboard temp files)
  - Disk image file (qcow2 or raw — supports snapshots)

**Special directories within the shared filesystem:**
```
/enlil-shared/
├── user/              ← user files, persistent across reboots
├── clipboard/         ← temp storage for clipboard file references (auto-cleaned)
├── dragdrop/          ← temp storage for cross-guest drag-and-drop (auto-cleaned)
├── downloads/         ← shared downloads folder (both OSes can save here)
└── transfer/          ← manual file transfer inbox/outbox per guest
    ├── to-linux/
    └── to-windows/
```

**Performance:**
- VirtIO-fs with DAX (direct memory mapping): near-native filesystem performance for reads
- Write performance depends on backing storage (NVMe-backed > RAM-backed > ESP)
- For large file transfers: the virtual network (Phase 3.7.5) via SMB/NFS may be faster than VirtIO-fs for bulk copies

**File locking and consistency:**
- VirtIO-fs supports POSIX file locking (Linux) but Windows uses a different locking model (NTFS opportunistic locks)
- For safety: warn users that simultaneous writes to the same file from both guests may cause corruption
- Recommendation: use the shared folder for file exchange (copy in, copy out), not for live-editing the same file from both OSes

**Config:**
```toml
[bridge.shared_fs]
enabled = true
backing = "directory"          # directory | partition | ramdisk | image
path = "/enlil/shared"        # path to backing directory (or device/image)
mount_tag = "enlil-shared"    # VirtIO-fs mount tag (guests use this to mount)
ramdisk_mb = 512              # if backing = ramdisk, size in MB
auto_clean_temp = true         # periodically clean clipboard/ and dragdrop/ dirs
```

#### 3.7.4 Notification Forwarding

**What it does:** When Guest B (Windows) receives a notification (email, Teams message, system alert), optionally forward it to Guest A (Linux) so the user sees it even when Guest B's zone is not focused. And vice versa.

**Implementation — Hypervisor side:**
- Receives notification payloads from bridge agents (title, body, icon, urgency, source app)
- Routes notifications based on config rules (forward all, forward only from specific apps, forward only when guest zone is not focused)
- Delivers to target guest's bridge agent via NotifyRx queue

**Implementation — Guest Agent (Linux):**
- Listens for D-Bus `org.freedesktop.Notifications` signals to capture outgoing notifications
- Sends incoming forwarded notifications via `notify-send` or D-Bus `Notify` method
- Forwarded notifications are tagged with `[Windows]` or `[Guest B]` prefix to distinguish source

**Implementation — Guest Agent (Windows):**
- Uses Windows Notification Listener API (`Windows.UI.Notifications.Management`) to capture toast notifications (requires notification access capability)
- Sends incoming forwarded notifications as toast notifications via `ToastNotificationManager`
- Tags forwarded notifications with `[Linux]` or `[Guest A]` prefix

**Config:**
```toml
[bridge.notifications]
enabled = true
forward_to = ["all"]                    # forward to all other guests
filter_apps = []                        # empty = forward all; or ["Teams", "Slack"]
only_when_unfocused = true              # only forward when source guest's zone is not active
urgency_threshold = "normal"            # low | normal | critical — only forward above threshold
```

#### 3.7.5 Fast Inter-Guest Virtual Network

**What it does:** A virtual Ethernet link between guests at memory-copy speed. Both guests see each other on a LAN with sub-0.1ms latency and multi-gigabit throughput. No physical NIC involved.

**Implementation:**
- The virtual switch from Phase 3.2 routes inter-guest packets directly through shared memory (no host TAP device, no kernel networking stack)
- Each guest gets a VirtIO-net NIC connected to the virtual switch
- Guests are auto-assigned IPs on a private subnet (e.g., `10.enlil.0.0/24` — `10.0.100.1` for Guest A, `10.0.100.2` for Guest B)
- The bridge agent configures the network automatically:
  - Linux: `systemd-networkd` unit or `NetworkManager` connection profile
  - Windows: DHCP from the hypervisor's virtual switch, or static IP via the bridge agent installer

**What this enables:**
- **SMB/NFS file sharing:** Windows can access Linux files via `\\10.0.100.1\share` (Samba). Linux can access Windows shares via `smb://10.0.100.2/Users`. This is in addition to VirtIO-fs — useful for applications that expect network shares.
- **Development workflows:** Run a web server on Linux, test it in Windows browsers at `http://10.0.100.1:8080`
- **Database access:** PostgreSQL on Linux, application on Windows, connected via the virtual LAN
- **SSH/RDP between guests:** SSH from Windows into Linux at `10.0.100.1` with near-zero latency
- **Print sharing:** Share a printer connected to one guest with the other via the network

**Performance:**
- Memory-copy-based packet forwarding: measured throughput should exceed 10 Gbps between guests
- Latency: <0.1ms round-trip (vs. ~0.5ms for VirtIO-net through a host TAP device, or ~1ms for physical loopback)
- The hypervisor's virtual switch uses a zero-copy path: VirtIO-net TX descriptors from Guest A's ring buffer are mapped directly into Guest B's RX ring buffer via EPT page sharing (where safe) or fast memory copy

**Config:**
```toml
[bridge.network]
enabled = true
subnet = "10.0.100.0/24"
dhcp = true                    # hypervisor provides DHCP on the virtual switch
guest_ips = { linux = "10.0.100.1", windows = "10.0.100.2" }

# Optional: inter-guest traffic isolation (for CVM mode)
encrypt_inter_guest = false    # encrypt traffic between guests (for CVM scenarios)
```

#### 3.7.6 URL / Protocol Handler Routing

**What it does:** Click a URL in Linux → it opens in Windows' default browser (or vice versa). Click a `mailto:` link in Windows → it opens in Linux's email client. Any URI scheme can be routed between guests.

**Why this matters:** Users often have specific applications on specific OSes. You might run your IDE on Linux but need to test URLs in Windows' Edge/IE for compatibility. Or you run Outlook on Windows but want `mailto:` links from Linux to open there.

**Implementation — Hypervisor side:**
- Maintains a URI scheme routing table: `{ scheme: target_guest }`
- When a guest bridge agent sends a URL-open request via ControlTx, the hypervisor checks the routing table and forwards to the correct guest's ControlRx queue

**Implementation — Guest Agent (Linux):**
- Registers as the default handler for configured URI schemes via `xdg-mime` / `xdg-open`
- When a routed URL is handled: sends it to the hypervisor instead of opening locally
- When receiving a URL from the hypervisor: calls `xdg-open <url>` to open in the local default application

**Implementation — Guest Agent (Windows):**
- Registers protocol handlers in the Windows Registry (`HKEY_CLASSES_ROOT\<scheme>`)
- When a routed URL is handled: sends it to the hypervisor
- When receiving a URL from the hypervisor: calls `ShellExecute(url)` to open in the default application

**Config:**
```toml
[bridge.url_routing]
enabled = true

# Route specific URL schemes to specific guests
[bridge.url_routing.rules]
"http"   = "windows"           # HTTP links from any guest → open in Windows
"https"  = "windows"           # HTTPS links from any guest → open in Windows
"mailto" = "windows"           # mailto: links → open in Windows Outlook
"ssh"    = "linux"             # ssh:// links → open in Linux terminal
"vscode" = "linux"             # vscode:// links → open in Linux VS Code
"*"      = "local"             # default: open in the guest that initiated
```

#### 3.7.7 Bridge Agent Packaging & Installation

**The bridge agent is distributed as a single lightweight package per OS.**

**Linux package:**
- Formats: `.deb` (Debian/Ubuntu), `.rpm` (Fedora/RHEL), `.pkg.tar.zst` (Arch), Flatpak
- Installs:
  - `enlil-bridge` daemon (systemd service, auto-starts)
  - VirtIO-fs auto-mount systemd unit
  - Virtual network auto-configuration (NetworkManager or systemd-networkd)
  - xdg-open wrapper for URL routing
  - D-Bus notification listener
- Total installed size: <5MB
- Zero dependencies beyond what a standard Linux desktop already has

**Windows package:**
- Formats: `.msi` installer, or silent install via `enlil-bridge-setup.exe /S`
- Installs:
  - `enlil-bridge.exe` system tray application (auto-starts via registry Run key)
  - VirtIO-fs driver (bundled WinFsp) + auto-mount as drive letter
  - Virtual network static IP configuration
  - Protocol handler registry entries for URL routing
  - Clipboard listener COM component
  - Windows notification listener
- Total installed size: <10MB (including WinFsp)
- Can be pre-injected into a Windows image via `dism` (for the "boot existing Windows" workflow)

**Config location:**
- The bridge agent reads its config from the VirtIO control queue (pushed by the hypervisor from `config.toml`)
- No per-guest config files needed — the hypervisor is the single source of truth
- Agent auto-discovers the VirtIO bridge device via PCI enumeration on boot

**Milestone:** Copy a URL in Linux Firefox → it opens in Windows Edge. Drag a file from Nautilus into a Windows Explorer window. A Teams notification from the Windows guest pops up on the Linux desktop. Both guests access the same shared folder. Both guests ping each other at <0.1ms.

---

## Phase 4 — USB Peripheral Routing

**IMPLEMENTATION NOTE: Build Phase 4 BEFORE Phase 5 (Windows).** Windows guests almost always need USB devices during initial setup — a physical keyboard/mouse for installation, game controllers, USB audio. Without USB routing, Windows installation requires VirtIO-only input, which means installing VirtIO drivers during Windows Setup (a pain point that requires a custom driver ISO). With USB routing available first, you simply route a physical keyboard and mouse to the Windows guest during installation, and the standard Windows installer works out of the box.

**Goal:** Granular per-device USB routing so each guest gets specific physical USB devices.

**Duration:** 3–4 weeks

### 4.1 USB Subsystem Architecture
```
Physical USB Devices
       │
  ┌────┴────┐
  │ USB     │  Hypervisor enumerates all physical USB devices
  │ Monitor │  via host xHCI controller
  └────┬────┘
       │
  ┌────┴─────┐
  │ Routing  │  Policy engine: maps VID:PID or bus:port → guest
  │ Engine   │
  └────┬─────┘
       │
  ┌────┴───────────┬───────────────┐
  │ vXHCI Guest 1  │ vXHCI Guest 2 │  Each guest sees its own
  │ (mouse, kbd)   │ (mouse, kbd)  │  USB host controller
  └────────────────┴───────────────┘
```

### 4.2 Host USB Enumeration
- On startup, enumerate all USB devices via the physical xHCI controller
- Track device connect/disconnect events (hot-plug monitoring)
- Identify devices by: VID:PID, serial number, physical port path (bus topology)
- Maintain a live device inventory accessible from the management console

### 4.3 Routing Policy Engine
- Config-driven routing:
  ```toml
  [usb.routing]
  # Route by VID:PID
  "046d:c077" = "linux1"   # Logitech mouse → guest 1
  "046d:c534" = "linux2"   # Logitech receiver → guest 2

  # Route by physical port
  "1-1" = "linux1"         # USB port 1-1 always goes to guest 1
  "1-2" = "linux2"         # USB port 1-2 always goes to guest 2

  # Default policy
  default = "linux1"       # Unmatched devices go here
  ```
- Support live re-routing via management console (move a device between guests at runtime)
- Hot-plug events: when a new device is plugged in, apply routing rules and attach to correct guest

### 4.4 Virtual xHCI Controller
- Present each guest with an emulated xHCI (USB 3.x) host controller
- **Primary approach: Software-emulated xHCI with TRB-level interception (recommended)**
  - Use Intel ACRN's USB virtualization architecture as the primary reference — it has the most mature xHCI emulation of any lightweight hypervisor
  - ACRN's design: intercept xHCI doorbell register writes via EPT trap → parse TRB (Transfer Request Block) chains → forward USB transfers to physical devices via libusb → complete TRBs back to guest
  - This enables per-device routing from day one — each physical USB device can be independently assigned to any guest
  - Reference implementation: ACRN `devicemodel/hw/pci/xhci.c` and USB core abstraction
  - **Why not VFIO controller passthrough first:** Community experience across Proxmox, Unraid, and bare-metal KVM consistently shows USB controller FLR (Function Level Reset) instability. Controllers often fail to reset on VM reboot, requiring full host restart. VFIO also doesn't provide per-device granularity.
- **Fallback: VFIO controller passthrough** (for users with multiple physical USB controllers who prefer simplicity)
  - Assign entire xHCI controllers to guests via IOMMU
  - Only works when controllers are in separate IOMMU groups
  - Warn users about FLR stability risks in documentation

### 4.5 Management Console — USB Controls
- Add USB tab to TUI showing:
  - All physical USB devices with current routing assignment
  - Reassignment interface (select device → select target guest)
  - Hot-plug notifications
- **Milestone:** Two mice and two keyboards plugged in, each routed to a different guest, with live reassignment via TUI

---

## Phase 5 — Windows Guest Support & Transparency

**Goal:** Boot Windows as a guest with full transparency — the OS and applications must not detect the hypervisor.

**Duration:** 6–8 weeks

### 5.1 ACPI Table Synthesis
- Generate per-guest ACPI tables from scratch:
  - **RSDP** → **XSDT** → **FADT**, **MADT**, **DSDT**, **SSDT**, **MCFG**, **HPET**
  - MADT: define virtual APIC topology matching assigned vCPUs
  - DSDT/SSDT: AML bytecode defining virtual devices (PCI bus, ISA bus, power management)
  - MCFG: PCI Express config space for virtual PCI devices
- Use `acpi_tables` crate or write raw AML generation
- Tables must look like they came from a real motherboard vendor (use realistic OEM strings)

### 5.2 SMBIOS Synthesis
- Generate SMBIOS/DMI tables that report:
  - Plausible system manufacturer, product name, serial number
  - Real CPU model string (pass through from physical CPU)
  - Correct memory configuration matching allocated RAM
  - BIOS vendor string (match a common vendor like AMI or Phoenix)
- Windows reads these extensively during setup and activation

### 5.3 CPUID Stealth
- Intercept all CPUID exits and craft responses:
  - **Leaf 0x1, ECX bit 31:** Clear the hypervisor present bit
  - **Leaf 0x40000000–0x400000FF:** Return zeros (no hypervisor signature)
  - **Leaf 0x0:** Report correct vendor string (GenuineIntel / AuthenticAMD)
  - **Leaf 0x1:** Report correct family/model/stepping from physical CPU
  - **Leaf 0x4, 0xB:** Report virtual topology (only assigned cores)
  - **Leaf 0x80000002–0x80000004:** Pass through real CPU brand string
- Ensure all reserved/undefined leaves return 0 (some detectors check these)

### 5.4 Timing Stealth (Expanded — from 2024–2025 anti-cheat research)

**Basic TSC stealth:**
- RDTSC/RDTSCP: use TSC offsetting in VMCS to compensate for VM exit overhead
  - Measure average exit cost and subtract from TSC offset
  - Make time appear to flow continuously from the guest's perspective
- Disable or carefully handle RDTSC exit interception (prefer TSC offsetting over trapping)

**CPUID constant-time responses (critical):**
- Modern detectors measure how long CPUID takes — it's much slower under virtualization because it causes a VMEXIT
- Precompute ALL CPUID leaf results into a lookup table at guest initialization
- On CPUID VMEXIT, serve results from the table with minimal processing to reduce exit latency
- Goal: CPUID exit round-trip < 500 cycles (real hardware is ~100–200 cycles; poorly handled VMEXITs are 2000+)

**IA32_APERF/MPERF MSR emulation (critical — defeats IET divergence detection):**
- Anti-cheat IET (Instruction Execution Time) divergence tests compare the execution time of CPUID against a slow reference instruction using IA32_APERF (actual performance counter) instead of TSC
- APERF is harder to spoof than TSC because it counts actual CPU cycles, not wall-clock time
- Implementation: intercept RDMSR for IA32_APERF (0xE8) and IA32_MPERF (0xE7)
- Maintain shadow APERF/MPERF counters that:
  - Advance proportionally to TSC during guest execution
  - Do NOT advance during VMEXIT (to hide hypervisor overhead)
  - Maintain consistent APERF/MPERF ratio to avoid statistical anomaly detection
- This requires tracking cumulative VMEXIT time per-vCPU and subtracting it from the shadow counters

**LBR (Last Branch Record) save/restore (critical — defeats branch analysis detection):**
- Anti-cheats check the LBR stack after forcing a VMEXIT (via CPUID) to detect that a branch to the hypervisor occurred
- If the last branch target in the LBR doesn't match the expected next instruction after CPUID, a hypervisor is present
- **Intel VMX implementation:**
  - Enable LBR save/restore in VMCS VM-exit controls (bit 22) and VM-entry controls (bit 22)
  - Hardware automatically saves the full LBR stack on VMEXIT (MSRs 0x680–0x68F for FROM, 0x6C0–0x6CF for TO)
  - Hardware automatically restores LBR stack on VMRESUME
  - After save, sanitize the most recent LBR entry to remove the branch-to-hypervisor record
- **AMD SVM implementation:**
  - AMD SVM has native LBR Virtualization (LBRV) support — SVM feature bit 1
  - Enable LBRV in the VMCB control area (LBR_VIRTUALIZATION_ENABLE bit)
  - Hardware saves/restores DebugCtlMSR, LastBranchFromIP, LastBranchToIP, LastIntFromIP, LastIntToIP during VMRUN/#VMEXIT
  - Intercept RDMSR for DebugCtlMSR (0x1D9) to shadow the BTF (bit 1) and LBR enable (bit 0) bits
  - The BTF bit must also be shadowed because it changes single-stepping behavior and is a detection vector
  - Same sanitization of the most recent LBR entry needed as Intel
- **Both platforms:** The detection attack is identical (force VMEXIT via CPUID, inspect LBR). Only the hardware save/restore mechanisms differ (VMCS fields vs VMCB bits). The sanitization logic is shared code.

**PMC (Performance Monitoring Counter) awareness:**
- Some detectors use `RDPMC` to read hardware performance counters
- Virtualize PMC access to return consistent values that don't reveal VMEXIT overhead
- Either trap RDPMC and serve shadow values, or use PMC virtualization features (Intel VPMC)

### 5.5 Virtual TPM 2.0
- Required for Windows 11
- Implement a software TPM 2.0 (use `swtpm` as reference or integrate with the `tpm2-tss` ecosystem)
- Each guest gets its own virtual TPM with independent PCR banks, endorsement keys, etc.
- Expose via MMIO at standard TPM address (0xFED40000)
- Store TPM state persistently per guest (for BitLocker, Windows Hello, etc.)

### 5.6 Windows Boot Path
- Option A: OVMF (UEFI firmware for VMs) — boots Windows in UEFI mode
  - Provide virtual UEFI firmware (OVMF) to each guest
  - Pass our synthetic ACPI/SMBIOS tables through OVMF
- Option B: Direct Windows boot (harder — requires understanding Windows boot protocol)
- Start with OVMF (well-tested, supports Secure Boot)

### 5.7 Windows-Specific Virtual Devices
- Emulate or pass through:
  - Virtual GPU (see Phase 7) — Windows needs a display adapter for desktop
  - Virtual audio (HDA controller) or passthrough physical audio
  - PS/2 keyboard/mouse as fallback (Windows expects these early in boot)
  - PCI Express root complex
  - ACPI power management (S3/S4/S5 sleep states)

### 5.8 Anti-Detection Testing
- **Automated test suite** that runs inside the guest and checks for hypervisor presence:
  - CPUID checks (leaf 0x1 bit 31, leaf 0x40000000)
  - Timing checks (RDTSC delta around CPUID)
  - IET divergence test (CPUID vs slow instruction timing via APERF — custom implementation)
  - LBR stack analysis post-CPUID (verify last branch target is correct)
  - SMBIOS/ACPI string checks
  - Registry checks (Windows creates entries for Hyper-V, VMware, etc.)
  - Device driver checks (no virtio, vmware tools, etc. in device manager)
  - NIC MAC address prefix checks (avoid known VM vendor OUIs)
  - ACPI table structure validation (realistic table count, sizes, OEM strings)
- **Standard tools:**
  - `pafish` (Paranoid Fish) — comprehensive anti-VM detection tool, covers CPUID, RDTSC, registry, SMBIOS, mouse activity
  - `al-khaser` — advanced anti-VM/anti-debug tool with dozens of detection vectors
  - Custom IET divergence test using IA32_APERF/MPERF
- **ACPI AML fuzzing** (from BadAML research, ACM CCS 2025):
  - Fuzz our synthetic ACPI AML bytecode to ensure it doesn't introduce exploitable interfaces
  - The BadAML paper demonstrated compromising confidential VMs through malformed ACPI tables
  - Use `iasl` (Intel ACPI compiler) to validate all generated AML
- Test against common anti-cheat (BattlEye, EAC, Vanguard) and DRM software
- **Milestone:** Windows 11 installs and runs as a guest, passes pafish + al-khaser + custom IET test

---

## Phase 6 — Bare-Metal Boot (UEFI Payload)

**Goal:** Remove the Linux host dependency. Enlil boots directly from UEFI firmware as the first code that runs.

**Duration:** 8–12 weeks

### 6.1 UEFI Application
- Write a UEFI application in Rust using `uefi-rs` crate
- Boot flow:
  1. UEFI firmware initializes hardware, provides memory map, ACPI tables, PCI enumeration
  2. Our UEFI app is the boot payload (configured in UEFI boot manager)
  3. App consumes UEFI services: memory map, GOP (framebuffer), PCI protocol
  4. App calls ExitBootServices() — takes full control of hardware
  5. App transitions to our hypervisor kernel

### 6.2 Bare-Metal Kernel
- Replace KVM layer with direct VMX/SVM programming:
  - **Intel:** VMXON, VMCLEAR, VMPTRLD, VMLAUNCH/VMRESUME, VMREAD/VMWRITE
  - **AMD:** VMRUN, VMSAVE/VMLOAD, #VMEXIT handling
- **Activate the `platform-baremetal` backend** (built in Phase 1):
  - Swap `GlobalAlloc` from `mmap` → buddy allocator over physical memory
  - Swap threading from `pthread` → per-CPU scheduler with work-stealing
  - Swap sync primitives from `futex` → spinlock/sleep-lock implementations
  - Swap time from `clock_gettime` → calibrated TSC
  - Swap I/O from `write()` syscall → direct serial port / GOP framebuffer
- Implement the bare-metal specifics that the platform layer backends to:
  - Page table management (host page tables for hypervisor, EPT/NPT for guests)
  - Interrupt handling (IDT setup, APIC configuration, APIC timer for preemption)
  - Per-CPU data structures (one per physical core, GS-base for TLS)
- **Because the platform layer already exists, all Enlil code above it (Phases 2–5) works unchanged.** Only the platform backends need implementing — not the VMM logic itself. This is the payoff of investing in Phase 1 early.

### 6.3 Hardware Discovery (Without Linux)
- Parse ACPI tables from UEFI to discover:
  - CPU topology (MADT/SRAT)
  - PCI devices (MCFG for ECAM, walk PCI config space)
  - IOMMU (DMAR for Intel VT-d, IVRS for AMD-Vi)
  - Memory map (UEFI memory map + E820-style conversion)
  - USB controllers (PCI enumeration → xHCI BARs)
- Build our own device tree from this discovery

### 6.4 IOMMU Programming
- Program Intel VT-d (DMAR) or AMD-Vi (IVRS) directly:
  - Build DMA remapping tables (DRHD → context tables → page tables)
  - Assign PCI devices to guest IOMMU domains
  - Enable interrupt remapping
- This is critical for device passthrough security and GPU passthrough

### 6.5 Direct Device Management
- Take over all device management previously handled by Linux:
  - xHCI driver for USB (replaces libusb/VFIO)
  - NVMe driver for storage (or AHCI for SATA)
  - Network driver (minimal — for management network)
  - Framebuffer driver (from UEFI GOP) for management console display
- This is a substantial amount of driver code. Consider:
  - Porting minimal drivers from Redox OS (Rust-based OS)
  - Using a thin "service VM" (like Xen's Dom0) that runs Linux to handle drivers — this is the pragmatic Xen-style approach and may be necessary for driver coverage

### 6.6 Service VM Option (Recommended)
- Rather than writing every driver from scratch, boot a privileged lightweight Linux VM:
  - It has direct hardware access for driver support
  - It runs the management console
  - Guest VMs get devices through backends in the service VM
  - Similar to Xen Dom0 / Hyper-V parent partition
- This dramatically reduces the bare-metal driver burden
- Guest VMs are still fully isolated and transparent

### 6.7 Milestone
- Boot from UEFI → Enlil kernel → Launch service VM → Launch guest VMs
- All Phase 2–5 functionality works without a pre-existing OS

---

## Phase 7 — GPU Sharing

**Goal:** Multiple guests share GPU(s) with a tiered strategy based on hardware capabilities.

**Duration:** 12–16 weeks (ongoing research)

### 7.1 GPU Strategy Tier System

The user selects the strategy, but the system recommends based on detected hardware:

```
┌─────────────────────────────────────────────────┐
│            GPU Strategy Selection                │
├─────────────┬───────────────────────────────────┤
│ Tier 1      │ FULL PASSTHROUGH                  │
│ (Simplest)  │ 2+ GPUs: one per guest via IOMMU  │
│             │ Works NOW, best performance        │
├─────────────┼───────────────────────────────────┤
│ Tier 2a     │ SR-IOV PARTITIONING               │
│ (Intel iGPU │ Intel 12th gen+ iGPU: WORKING NOW │
│  NOW)       │ via i915-sriov-dkms, 1-2% overhead│
├─────────────┼───────────────────────────────────┤
│ Tier 2b     │ NVIDIA MIG (datacenter only)      │
│ (If HW      │ A100/A30/H100: HW-partitioned     │
│  supports)  │ GPU instances, 3-5% overhead       │
├─────────────┼───────────────────────────────────┤
│ Tier 2.5    │ VIRTIO-GPU (Venus/VirGL/vDRM)     │
│ (ANY GPU,   │ API-level virtualization via Mesa  │
│  Linux      │ Near-native perf, no HW support   │
│  guests)    │ needed. Linux guests only (2025).  │
├─────────────┼───────────────────────────────────┤
│ Tier 3      │ MEDIATED PASSTHROUGH              │
│ (Primary    │ Our custom implementation:         │
│  target)    │ intercept GPU commands at          │
│             │ submission layer, multiplex         │
├─────────────┼───────────────────────────────────┤
│ Tier 4      │ TIME-SLICED PASSTHROUGH           │
│ (Highest    │ Full GPU context save/restore,     │
│  compat)    │ round-robin between guests          │
└─────────────┴───────────────────────────────────┘
```

### 7.2 Tier 1 — Full Passthrough (Implement First)
- Detect all GPUs via PCI enumeration
- Use IOMMU to assign entire GPU (all functions in IOMMU group) to one guest
- Guest sees the real GPU, loads real drivers, full performance
- Other guest(s) get: secondary GPU, integrated graphics, or software framebuffer
- **AMD-specific notes:**
  - AMD Ryzen APU iGPU passthrough is unreliable — community reports consistent host freezes with VFIO on Vega/RDNA iGPUs. Do NOT rely on AMD iGPU passthrough as a primary strategy.
  - For AMD desktop users: the recommended early path is APU iGPU for one guest (host/service VM display) + discrete GPU full passthrough to the other guest. This sidesteps the iGPU passthrough instability.
  - AMD discrete GPU reset bugs (especially pre-RDNA2) can prevent VMs from rebooting cleanly. Detect GPU generation and warn users.
- Config:
  ```toml
  [guest.windows.gpu]
  mode = "passthrough"
  device = "0000:01:00.0"   # PCI BDF of the GPU
  ```
- Handle GPU reset on guest reboot (FLR — Function Level Reset)
- Handle GPU ROM (option ROM / vBIOS) loading for the guest

### 7.3 Tier 2a — Intel iGPU SR-IOV (Implement Early — Working on Consumer Hardware)

**This is the fastest path to GPU sharing on consumer hardware.**

- Intel 12th gen+ (Alder Lake, Raptor Lake, Meteor Lake) integrated GPUs support SR-IOV
- Community project `i915-sriov-dkms` (https://github.com/strongtz/i915-sriov-dkms) enables this TODAY
- Overhead: 1–2% (2025 Samara University GPU virtualization study)
- **Note:** Intel confirmed discrete Arc GPUs (Alchemist architecture) do NOT support SR-IOV
- Implementation:
  - Detect Intel iGPU with SR-IOV capability via PCI config space
  - Enable SR-IOV: write NumVFs to PCI SR-IOV capability structure
  - Each Virtual Function appears as a separate PCI device
  - Assign one VF per guest via IOMMU
  - Guest loads standard i915 driver — full transparency
- **Recommend implementing alongside Phase 2–3** (not waiting for Phase 7) because:
  - It's the lowest-complexity GPU sharing approach
  - Both guests get hardware-accelerated graphics immediately
  - Enables development and testing of multi-guest GPU workflows early
- Config:
  ```toml
  [gpu]
  strategy = "sriov"
  device = "0000:00:02.0"   # Intel iGPU
  vfs = 2                    # Number of virtual functions
  ```

### 7.3b Tier 2b — NVIDIA MIG (Datacenter GPUs Only)
- NVIDIA A100, A30, H100 support Multi-Instance GPU (MIG)
- Hardware-partitioned GPU instances with isolated memory, 3–5% overhead
- If Enlil detects a MIG-capable GPU:
  - Create MIG instances via NVIDIA Management Library (NVML)
  - Each MIG instance appears as a separate GPU to IOMMU
  - Assign instances to guests like regular passthrough
- Not applicable to consumer NVIDIA GPUs, but worth supporting for datacenter deployments

### 7.3c Tier 2c — AMD GPU SR-IOV via GIM (Emerging — Watch Closely)
- **Current state (2025):** AMD open-sourced their GPU-IOV Module (GIM) in April 2025 for SR-IOV virtualization
- GIM provides: GPU IOV virtualization, virtual function configuration, GPU scheduling for world switch, hang detection and FLR reset, PF/VF handshake
- Currently supports only Instinct MI300X (datacenter) on Ubuntu 22.04 with ROCm 6.4
- **AMD has confirmed Radeon (consumer discrete GPU) SR-IOV support is "in the roadmap"**
- When Radeon SR-IOV ships, Enlil should support it immediately — GIM's VF model is similar to Intel's
- Code: https://github.com/amd/MxGPU-Virtualization
- **AMD iGPU:** No SR-IOV support exists for AMD integrated graphics (Vega/RDNA iGPUs in Ryzen APUs). This is a hardware limitation. AMD APU users must use Tier 3/4 for GPU sharing, or dual-GPU passthrough if a discrete card is available.

### 7.3d Tier 2d — Other SR-IOV GPUs (Dynamic Detection)
- Probe PCI SR-IOV capability on all detected GPUs at boot
- NVIDIA enterprise GPUs (A100, etc.) — detect and enable if present
- Future Intel Arc GPUs may add SR-IOV — probe capabilities dynamically
- Any GPU advertising SR-IOV PCI capability: attempt to enable VFs and assign to guests

### 7.3e Tier 2.5 — VirtIO-GPU (Venus / VirGL / vDRM) — Works on ALL GPUs

**This is the most universal GPU sharing approach — no SR-IOV, no vendor-specific knowledge, works on any GPU with open-source drivers.**

VirtIO-GPU virtualizes graphics at the API level rather than the hardware level. The guest sends Vulkan/OpenGL commands over VirtIO, and the hypervisor (or service VM) executes them on the physical GPU. The guest doesn't need the physical GPU's driver — it uses Mesa's VirtIO-GPU drivers.

**Three sub-approaches (all via VirtIO-GPU):**

| Approach | API | Performance | Guest OS | Status (2025) |
|----------|-----|-------------|----------|---------------|
| **Venus** | Vulkan | Near-native for most apps | Linux only (Mesa) | Stable, merged in QEMU 9.2 (Nov 2024) |
| **VirGL** | OpenGL | Good, some overhead from shader retranslation | Linux only (Mesa) | Stable for years |
| **vDRM (native context)** | Native DRM | Sub-1% overhead | Linux only, same GPU vendor host/guest | Emerging, crosvm support, QEMU in progress |
| **Zink + Venus** | OpenGL→Vulkan | Good (OpenGL apps use Vulkan via Zink) | Linux only | Works today |

**Implementation for Enlil:**
- Implement VirtIO-GPU device backend in `enlil-devices` with virglrenderer integration
- Venus context: guest sends serialized Vulkan commands, hypervisor deserializes and executes on host Vulkan driver
- VirGL context: guest sends TGSI shader IR + OpenGL commands, virglrenderer translates and executes
- vDRM context: guest uses native GPU driver (e.g., amdgpu), commands pass through with minimal hypervisor involvement
- **Deep integration with Enlil Zones:** VirtIO-GPU is the ideal compositor source — the hypervisor owns the render target, so zone composition is zero-copy. Each guest renders into a compositor-owned texture.

**Limitations:**
- **Linux guests only** (2025 status) — Windows has no VirtIO-GPU Vulkan driver. For Windows guests, use Tier 1/2 passthrough or Tier 3/4 mediated/timesliced.
- Venus is tested primarily with AMD and Intel open-source Vulkan drivers. NVIDIA proprietary driver support is in progress.
- vDRM requires host and guest to use the same GPU vendor's driver.

**Why include this:** For Linux-Linux multi-guest setups (common: Linux development + Linux server, or two Linux desktops), this gives both guests hardware-accelerated Vulkan/OpenGL with zero custom code — just implement the VirtIO-GPU device backend and let Mesa handle the rest. It works on ANY GPU including AMD consumer GPUs where SR-IOV doesn't exist.

**Config:**
```toml
[guest.linux.gpu]
mode = "virtio-gpu"
venus = true                # Enable Vulkan via Venus
virgl = true                # Enable OpenGL via VirGL
hostmem_mb = 4096           # Shared memory for blob resources
```

### 7.4 Tier 3 — Mediated Passthrough (Primary Research Target)

This is the most complex but most universally applicable approach.

**Architecture:**
```
Guest 1 GPU Driver          Guest 2 GPU Driver
    │ (MMIO writes)              │ (MMIO writes)
    ▼                            ▼
┌──────────────────────────────────┐
│       GPU Command Interceptor     │
│  ┌─────────┐    ┌─────────────┐  │
│  │ Decoder  │    │  Scheduler  │  │
│  │ (vendor  │    │  (fair-share│  │
│  │ specific)│    │   queuing)  │  │
│  └────┬─────┘    └──────┬──────┘  │
│       └────────┬────────┘         │
│           ┌────▼────┐             │
│           │ VRAM    │             │
│           │ Manager │             │
│           └────┬────┘             │
└────────────────┼─────────────────┘
                 ▼
          Physical GPU
```

- **MMIO Trap:** Configure EPT/NPT to trap all GPU BAR (Base Address Register) accesses
- **Critical research finding (FGCS June 2025):** A 2025 benchmarking study of mediated passthrough rigidity shows that register-level MMIO interception has fundamental instability issues — subtle timing and ordering dependencies in GPU register accesses cause guest driver crashes. **Preferred approach: intercept at the command submission layer** (ring buffer doorbell writes) rather than individual MMIO register accesses. This is higher in the stack and more robust:
  - Trap only the doorbell MMIO write that signals "new commands in the ring buffer"
  - Parse the command buffer contents (which are in guest memory, accessible via EPT)
  - Validate, schedule, and forward command buffers to the physical GPU
  - This is similar to how ACRN intercepts USB at the TRB level — intercept at submission, not at every register access
- **Command Decode:** Understand GPU command buffer format (vendor-specific, partially documented):
  - Intel: relatively well-documented (open-source i915 driver, IGT GPU tools)
  - AMD: partially documented (open-source radeon/amdgpu drivers, register specs)
  - NVIDIA: poorly documented (nouveau reverse-engineering, some headers from NVIDIA open kernel modules)
- **VRAM Management:**
  - Partition VRAM between guests (static allocation or dynamic with overcommit)
  - Maintain per-guest VRAM page tables
  - Handle VRAM swaps on context switch
- **Display Multiplexing:**
  - Each guest renders to a virtual framebuffer in VRAM
  - Hypervisor composites outputs to physical display(s) or routes to separate monitors
  - Handle EDID synthesis (each guest thinks it has its own monitor)

**Vendor-Specific Notes:**
- **Start with Intel (integrated/Arc):** Best documentation, open-source driver stack, GVT-g exists as reference
- **Then AMD:** Reasonable documentation via open-source amdgpu driver, register programming guides
- **NVIDIA last:** Hardest due to proprietary drivers, but NVIDIA open-gpu-kernel-modules (2022+) provide some header-level insight

### 7.5 Tier 4 — Time-Sliced Passthrough (Highest Compatibility)

**Concept:** Each guest gets exclusive GPU access for a time slice, then we save GPU state and switch.

**Challenges & Approach:**
1. **GPU State Save/Restore:**
   - Save all GPU registers (MMIO-mapped state)
   - Save VRAM contents (potentially GBs — too slow to copy every switch)
   - Solution: use GPU page tables to remap VRAM regions instead of copying
   - Save GPU command queue state, fence values, interrupt state
2. **Time Quantum:**
   - Configure per-guest (e.g., 16ms = 1 frame at 60fps, or longer for background VMs)
   - Priority system: foreground guest gets longer slices
3. **GPU Reset Between Switches:**
   - May need to reset GPU command processor between guests
   - Use Function Level Reset (FLR) if available, or engine-level reset
4. **Display Handling:**
   - Only one guest's output is on the physical display at a time, OR
   - Use a composition buffer: render each guest to a framebuffer, composite in the hypervisor
5. **Fence/Sync:**
   - GPU commands are asynchronous — must wait for completion before switching
   - Drain command queues at switch points

**Implementation Strategy:**
- Start with "paused rendering" model: only the foreground guest renders, background guest is paused
- Evolve to true time-slicing with VRAM remapping
- This will work with ANY GPU (even proprietary NVIDIA) because we're treating the GPU as an opaque device — we don't need to understand its command format, just save/restore state

### 7.6 GPU Configuration
```toml
[gpu]
strategy = "auto"  # auto | passthrough | sriov | mediated | timeslice

[gpu.passthrough]
guest = "windows"
device = "0000:01:00.0"

[gpu.timeslice]
quantum_ms = 16
priority.windows = 3      # higher = more GPU time
priority.linux = 1

[gpu.mediated]
vram_split = { windows = "12GB", linux = "4GB" }
```

---

## Phase 8 — Polish, Hardening & Advanced Features

**Duration:** Ongoing

### 8.1 Live Migration Between Strategies
- Allow switching GPU strategy at runtime (e.g., switch from timeslice to passthrough when one guest shuts down)
- Allow live USB device reassignment (already in Phase 4)

### 8.2 Audio Subsystem

Both guests producing audio simultaneously (music on Linux, game on Windows) must work glitch-free with low latency. Audio latency above ~20ms is perceptible.

**Implementation:**
- Present a VirtIO-sound device to each guest (virtio spec 1.2+, Linux kernel driver since 5.14, Windows community driver available)
- Each guest sends PCM audio streams to the hypervisor via VirtIO-sound queues
- Hypervisor audio engine:
  - Receives PCM streams from all active guests
  - Mixes streams (sample-rate conversion if guests use different rates)
  - Sends mixed output to the physical audio hardware
  - Target: <10ms total latency (achievable with direct ALSA-like hardware access from the hypervisor, bypassing PulseAudio/PipeWire-style buffering)
- Per-guest volume control and muting via management console and hotkeys
- Microphone routing: physical mic input routed to one guest at a time (configurable, hotkey to switch)
- Optional: pass through the entire audio device to one guest via IOMMU (simplest, lowest latency, but only one guest gets audio)
- **Enlil Zones integration:** per-zone audio focus — the focused zone's guest gets microphone priority and louder audio, unfocused zones are optionally muted or reduced volume
- Config:
  ```toml
  [audio]
  mode = "mix"                 # mix | passthrough | per-zone
  output_device = "auto"       # auto-detect primary audio output
  latency_target_ms = 10
  mic_default_guest = "linux"  # which guest gets the mic by default
  mic_switch_hotkey = "Ctrl+Alt+M"
  [audio.volumes]
  linux = 100
  windows = 80
  ```

### 8.3 Display Routing
- Support multiple physical monitors: route each to a different guest
- Handle display hot-plug (EDID changes, resolution negotiation)
- KVM switch-like behavior: keyboard shortcut to swap which guest is on which monitor

### 8.4 Suspend/Resume & Hypervisor Live-Update
- Save full guest state to disk (hibernation)
- Resume guests after host power cycle
- Useful for: update host firmware, move guests to different hardware
- **Hypervisor live-update (from Rust-Shyper research, 2023/2024):**
  - Save full VM state (vCPU registers, memory mappings, device state)
  - Replace the Enlil hypervisor binary in memory with a new version
  - Restore all VMs on the new hypervisor version
  - Guests experience a brief pause (milliseconds) but do not reboot
  - Enables security patches and feature updates without guest downtime
  - Reference: Rust-Shyper (Computers & Security, 2024) implements this for ARM; adapt for x86 VMX/SVM

### 8.5 Performance Monitoring
- Per-guest CPU utilization, memory pressure, GPU usage, IO throughput
- Expose via management console metrics

### 8.6 Security Hardening
- Verify IOMMU is active and enforced (prevent DMA attacks between guests)
- Validate all guest interactions with virtual devices (fuzzing)

### 8.7 Confidential VM Support — AMD SEV-SNP & Intel TDX

**Goal:** Provide hardware-enforced memory encryption between guests, so even a compromised hypervisor cannot read guest memory.

**Context (from 2024–2025 research):** AMD SEV-SNP and Intel TDX are now in production across all major cloud providers. A December 2024 ACM SIGMETRICS paper provides comprehensive benchmarks showing 1–5% overhead for compute-bound workloads, with I/O overhead up to 60% for heavy network due to bounce buffers.

**AMD SEV-SNP support:**
- Each guest gets a unique encryption key managed by the AMD Secure Processor (ASP)
- Guest memory is encrypted in DRAM — hypervisor sees only ciphertext
- SNP adds integrity protection: hypervisor cannot modify guest pages without detection
- Implementation requirements:
  - Modify NPT management to mark guest pages as encrypted (C-bit in page table entries)
  - Implement GHCB (Guest-Hypervisor Communication Block) protocol for guest ↔ hypervisor communication
  - Handle #VC (VMM Communication Exception) in the guest — the guest raises #VC instead of VMEXIT for certain operations
  - Support attestation: guest can request a signed report from the ASP to verify it's running on genuine AMD hardware
  - VMPL (Virtual Machine Privilege Levels): support running a paravisor at VMPL0 inside the guest for unenlightened OS support

**Intel TDX support:**
- Each guest runs as a Trust Domain (TD) with hardware-isolated encrypted memory
- TDX Module (firmware running in SEAM mode) mediates all VMX operations
- Implementation requirements:
  - Use SEAMCALL instruction to interact with TDX Module (replaces direct VMCS manipulation)
  - Implement TDCALL handling for guest → hypervisor communication
  - Support TD Partitioning (TDX 1.5) for nested virtualization within a TD
  - Support attestation via Intel Trust Authority or standalone TDQE (TD Quoting Enclave)
  - EPT management changes: TDX Module controls EPT, not the hypervisor directly

**Constraints on other phases:**
- **Phase 9 (Compute Fabric):** Under CVM mode, the hypervisor CANNOT read guest memory. The zero-copy buffer approach in Phase 9.8 won't work — the fabric must use bounce buffers in shared unencrypted memory regions, or the guest must explicitly share buffer pages via the GHCB/TDCALL interface. Document this as an architecture constraint.
- **Phase 5 (Transparency):** Some CVM features conflict with stealth — attestation and encrypted memory are explicitly "I know I'm in a VM" features. CVM mode and stealth mode are mutually exclusive configurations.

**ZK Proof Attestation (novel — no existing hypervisor does this):**
- Current CVM attestation requires trusting AMD's Secure Processor or Intel's TDX Module — proprietary, closed-source firmware
- Enlil could generate a Zero-Knowledge proof that:
  - Its binary matches a known hash (proves correct hypervisor version without revealing code)
  - Guest memory isolation is correctly configured (EPT/NPT mappings are non-overlapping)
  - No unauthorized modifications have been made to the hypervisor state
- A remote verifier checks the proof without needing to trust AMD/Intel's attestation infrastructure
- This enables **vendor-independent attestation** — a guest can verify isolation guarantees even if the hardware vendor's firmware is compromised
- Implementation: use RISC Zero or SP1 zkVM frameworks to generate proofs of hypervisor state correctness
- This is active research in confidential computing — Enlil implementing it would be genuinely novel
- Performance consideration: proof generation is expensive (seconds, not microseconds). Attestation is infrequent (boot time, on-demand), so this is acceptable.

### 8.8 Paravisor Mode (Stretch Goal — from Microsoft OpenHCL architecture)

- Instead of running device backends in the hypervisor or service VM, run them *inside* the guest at a higher privilege level (VMPL0 on AMD SEV-SNP, TD partitioning on Intel TDX)
- The paravisor intercepts and translates hardware interfaces from within the guest
- Benefits: supports unenlightened guests (older Windows/Linux) without external ACPI/SMBIOS synthesis
- Reference: Microsoft OpenHCL (open source, Rust, MIT licensed) — over 1.5M Azure VMs run with this architecture
- This is a fundamentally different approach to guest transparency — instead of faking hardware from outside, translate it from inside

### 8.9 Cross-Guest Isolation Verification (ZK Proofs)

- **Problem:** In multi-tenant scenarios, Guest A wants cryptographic proof that Guest B cannot access its memory. Today, guests must simply trust the hypervisor.
- **Solution:** Enlil generates a ZK proof that:
  - Guest A's EPT/NPT page table mappings do not overlap with Guest B's
  - Guest A's IOMMU domain excludes all devices assigned to Guest B
  - Guest A's memory regions are not mapped into any other guest's address space
- The proof reveals nothing about Guest B's configuration — only that isolation is maintained
- Guest A can verify the proof using a lightweight verifier (no privileged access needed)
- This is stronger than hardware attestation: it proves a specific *property* (isolation) rather than just *identity* (this is the right hypervisor)
- Useful for: cloud hosting, regulated environments, multi-user desktop scenarios
- Could be extended to prove properties about USB routing (device X is exclusively assigned to Guest A) and GPU assignment

### 8.10 Laptop & Mobile Hardware Support

The entire roadmap to this point assumes a desktop tower with multiple GPUs, multiple NVMe drives, and multiple monitors. Laptops are the majority of computers sold. Making Enlil work on laptops makes it accessible to everyone.

**Single GPU (mandatory for laptops):**
- Most laptops have one GPU (iGPU only) or iGPU + dGPU with switchable graphics (NVIDIA Optimus / AMD Switchable)
- **iGPU-only laptops (Intel):** Use Intel iGPU SR-IOV (Tier 2a) — both guests get a hardware VF. This is the cleanest path.
- **iGPU-only laptops (AMD):** AMD iGPU has no SR-IOV. Use VirtIO-GPU Venus (Tier 2.5) for Linux guests, or Tier 3/4 (mediated/time-sliced) for Windows guests. The Enlil Zones compositor is essential here — it composites both guests' framebuffers using the single GPU.
- **iGPU + dGPU laptops (Optimus/Switchable):** Passthrough the dGPU to one guest via IOMMU, other guest uses iGPU (via SR-IOV if Intel, or VirtIO-GPU if AMD). This is equivalent to a two-GPU desktop setup. Enlil must handle the Optimus mux — ensure the dGPU is NOT muxed to the laptop panel (the iGPU drives the panel via Enlil Zones, the dGPU renders offscreen for its guest).
- **Single display mandatory:** Enlil Zones (Phase 3.6) is not optional on laptops — it's the only display path. Default to fullscreen-switcher mode (hotkey toggles between guests).

**WiFi management:**
- WiFi controllers are typically on the PCH and share an IOMMU group with other critical devices — VFIO passthrough is usually impossible
- Enlil must manage WiFi at the hypervisor/service VM level:
  - Service VM (Phase 6.6) runs a minimal Linux with NetworkManager/wpa_supplicant to manage the physical WiFi connection
  - Each guest gets a VirtIO-net NIC connected to a virtual switch
  - The virtual switch bridges to the WiFi connection managed by the service VM
  - Guests see a normal wired Ethernet NIC (via VirtIO-net) — they don't know WiFi is underneath
- Alternative for bare-metal mode (no service VM): implement a minimal WiFi stack at the hypervisor level using the `wifi-rs` or similar crate. This is a significant amount of work — WPA3 authentication, roaming, power management. The service VM approach is strongly recommended for WiFi.
- Config:
  ```toml
  [network.wifi]
  managed_by = "service_vm"        # service_vm | hypervisor (service_vm recommended)
  ssid = "auto"                    # auto = connect to previously saved networks
  bridge_to_guests = true          # all guests get internet via virtual switch
  ```

**Battery & power management:**
- Guests need to know battery status for power management (Windows will show "plugged in" otherwise)
- Enlil presents a virtual ACPI battery device to each guest:
  - Read physical battery status from ACPI (`/sys/class/power_supply/` on Linux host, or parse ACPI _BST/_BIF objects directly on bare metal)
  - Expose identical battery info to each guest via a virtual ACPI _BST (Battery Status) and _BIF (Battery Information) method in the guest's synthetic DSDT
  - Guests see correct charge level, charge/discharge rate, AC adapter status
- **Power policy:** When battery is low (<15%), Enlil can optionally:
  - Pause non-essential guests (save state, stop vCPUs)
  - Reduce CPU allocation to background guests
  - Notify the user via management console
- Config:
  ```toml
  [power]
  battery_passthrough = true       # expose real battery status to guests
  low_battery_action = "notify"    # notify | pause_background | none
  low_battery_threshold = 15       # percent
  ```

**Sleep / Suspend (S3/S4):**
- When the user closes the laptop lid or triggers sleep:
  - **Option A — Full suspend:** Save all guest vCPU state, flush all dirty pages, suspend all guests, then S3 the physical hardware. On resume, restore all guests. Guests experience a time jump (their clocks advance) but otherwise resume normally. This is the simplest and most reliable approach.
  - **Option B — Selective suspend:** Pause background guests (save state to RAM), keep the foreground guest running until the hardware enters S3. On resume, restore background guests.
  - Passthrough devices (GPU, NVMe) must handle S3 correctly — device-specific resume sequences needed. GPU resume is the most fragile part (PCIe FLR + firmware reload).
- Hibernation (S4): Save all guest state + guest memory to disk. Resume from cold boot by reloading. This is essentially the snapshot system (Phase 8.12) applied to all guests simultaneously.

**Thunderbolt / USB4:**
- Thunderbolt docks and eGPUs are hot-pluggable
- The USB routing engine (Phase 4) must handle Thunderbolt device trees — a single Thunderbolt port can present: a USB hub, an Ethernet adapter, a display output, an NVMe drive, and an eGPU
- When a Thunderbolt dock is connected, Enlil must:
  - Enumerate the device tree
  - Apply routing rules to each device within the tree
  - Handle the eGPU as a new GPU available for passthrough (GPU hot-plug)
- Thunderbolt security levels (SL0–SL3) may require user confirmation before exposing devices to guests

**Webcam:**
- Laptop webcams are USB devices — handled by the USB routing engine (Phase 4)
- Route the webcam to one guest at a time (can't split a camera feed)
- Optional: a virtual webcam device that the hypervisor splits to multiple guests (capture from physical camera, present a virtual V4L2/DirectShow device to each guest). This is a stretch feature — complex but very useful for video calls on both OSes.

**Keyboard / Trackpad:**
- Built-in laptop keyboard and trackpad are typically PS/2 or I2C HID devices, NOT USB
- Enlil must either:
  - Present virtual PS/2 keyboard/mouse to each guest (simple, handled by existing vm-superio emulation)
  - Use the Enlil Zones input router — physical keyboard/trackpad input goes to the focused zone's guest
- **Fn keys and special keys:** Media keys, brightness, volume, airplane mode — these should be handled by Enlil directly (not forwarded to any guest) to control hypervisor-level functions. E.g., volume keys control the Enlil audio mixer, brightness controls the physical display.

### 8.11 Mobile / Android Guest Support

**Goal:** Run Android as a third (or second) guest alongside Linux and/or Windows. Use Android apps on your desktop, in an Enlil Zone.

**Why this matters:** Many users need Android apps (banking, messaging, 2FA authenticators, mobile-only services) but don't want a phone at their desk. Running Android as an Enlil guest gives native-speed Android apps on a desktop machine, isolated from other OSes.

**Approach A — Android-x86 as a Full VM Guest (simplest):**
- Android-x86 / BlissOS are Android ports that run on x86 hardware
- Boot BlissOS as a regular Enlil guest — it's just another OS with its own OVMF virtual UEFI, its own VirtIO-net, its own disk
- GPU: VirtIO-GPU with Venus provides Vulkan acceleration (Mesa's Venus driver works with Android's graphics stack since Android uses Vulkan/OpenGL ES via ANGLE)
- Input: Virtual touchscreen emulated by the Enlil Zones compositor — mouse clicks translate to touch events. Pinch-to-zoom via keyboard modifiers (Ctrl+scroll) or trackpad gestures.
- Display: Runs in an Enlil Zone — can be side-by-side with Linux/Windows or fullscreen
- **Advantages:** Full Android system, Play Store (with GApps), full hardware abstraction, strong isolation from other guests
- **Disadvantages:** Higher resource usage (runs a full Android kernel + userspace), slower than containerized approaches
- Config:
  ```toml
  [guest.android]
  name = "Android"
  cpus = [6, 7]                    # 2 cores is plenty for most Android apps
  memory_mb = 4096
  firmware = "ovmf"
  disk = "/enlil/disks/android.qcow2"
  install_iso = "/enlil/isos/blissos-16.iso"

  [guest.android.gpu]
  mode = "virtio-gpu"
  venus = true

  [guest.android.input]
  touchscreen = true               # emulate touchscreen from mouse/trackpad
  ```

**Approach B — Waydroid Inside a Linux Guest (lightweight):**
- If one of your guests is already running Linux, Waydroid runs Android apps inside that Linux guest using Linux namespaces (containerized, not virtualized)
- Waydroid gives near-native performance because it shares the Linux guest's kernel
- Android apps appear as windows alongside Linux apps within the Linux guest's Enlil Zone
- **Advantages:** Lightweight (no separate VM), near-native performance, integrated with Linux desktop
- **Disadvantages:** Requires a Linux guest (not applicable for Windows-only setups), depends on guest kernel configuration (namespaces, binder support), not managed by Enlil directly (runs inside the guest)
- Enlil's role: ensure the Linux guest's kernel has the required features (CONFIG_ANDROID_BINDER_IPC, namespaces) and that VirtIO-GPU acceleration passes through to Waydroid's Android graphics stack

**Approach C — Android as a Lightweight Guest with Chromium OS-style Integration (ambitious, long-term):**
- Run a minimal Android image (no full Linux kernel — use Enlil's hypervisor as the "kernel" via VirtIO devices)
- Android's userspace (init, SurfaceFlinger, Zygote, ART runtime) runs directly on Enlil's virtual hardware
- Each Android app renders to its own buffer, composited by Enlil Zones alongside other guest OS zones
- Individual Android app windows could appear as separate entities in the compositor — not just one Android "screen" but individual app windows mixed with Linux/Windows windows
- This is how ChromeOS runs Android apps (via ARCVM) — a lightweight VM with deep compositor integration
- **Reference:** Google's crosvm + ARCVM architecture (open source) is the closest prior art
- **This is the most ambitious approach** and should be a Phase 10+ target

**Phone Mirroring / Scrcpy Integration (bonus):**
- Connect a physical Android phone via USB → mirror its screen into an Enlil Zone
- Use `scrcpy` (open source, works via ADB) running inside a guest or at the hypervisor level
- The phone's screen appears as another zone, with input routed to it
- Not a true "guest" — the phone runs its own OS, Enlil just mirrors the display and routes input
- Useful for: notifications, quick replies, file transfers from phone to desktop guests

**Android-specific Enlil Bridge integrations:**
- **Clipboard:** Android clipboard → synced with Linux/Windows clipboards via the Enlil Bridge. For Approach A (Android VM), the bridge agent runs as an Android system app. For Approach B (Waydroid), clipboard flows through the Linux host agent.
- **Notifications:** Android notifications forwarded to Linux/Windows guests via Enlil Bridge notification forwarding
- **File sharing:** Shared filesystem (VirtIO-fs in Approach A, or direct access via the Linux guest's filesystem in Approach B)
- **URL routing:** Click a link in Android → opens in Windows/Linux browser (via Enlil Bridge URL routing)

### 8.12 Snapshot & Rollback System

**Design for future phases — Checkpoint Engine:** This phase builds the core "checkpoint engine" that three later features reuse. Snapshots (this section), live migration (Phase 11.5.1), and fault tolerance (Phase 11.10) all share the same mechanism: EPT dirty page tracking → guest state serialization → compressed delta transfer. Design the engine with a pluggable `CheckpointDestination` trait:

```rust
trait CheckpointDestination: Send + Sync {
    async fn write_state(&mut self, state: &GuestState) -> Result<()>;
    async fn write_dirty_pages(&mut self, bitmap: &DirtyBitmap, pages: &[PageData]) -> Result<()>;
    async fn commit(&mut self) -> Result<()>;
}
```

Phase 8.12 implements `FileDestination` (write to local disk). Phase 11.5.1 implements `MigrationStreamDestination` (stream to target node — one-shot). Phase 11.10 implements `ReplicationStreamDestination` (continuous streaming to backup node with periodic commits). The checkpoint engine code (EPT dirty tracking, state capture, compression) is identical for all three.

**Goal:** Save full VM state at any point and restore it later. Essential for safe experimentation, OS updates, and the "boot existing OS" workflow.

**VM state snapshot:**
- Saves: vCPU registers, MSRs, VMCS/VMCB state, EPT/NPT page tables, full guest RAM contents, device model state (VirtIO queue positions, virtual disk state, virtual NIC state, USB device assignments)
- Stored as a single file on disk (compressed with zstd for speed)
- Uses the same serialization format as Phase 8.4 suspend/resume and hypervisor live-update (one format for all state persistence needs)
- Snapshot creation should be fast: use copy-on-write for memory (mark all guest EPT pages as read-only, snapshot the page table, copy pages only when the guest modifies them post-snapshot). This is the same technique used by Linux `fork()`.
- Config:
  ```toml
  [snapshots]
  enabled = true
  path = "/enlil/snapshots"
  max_per_guest = 10              # keep last 10 snapshots per guest
  auto_snapshot_before_update = true  # snapshot before OS updates
  ```

**Disk snapshot (for VirtIO block devices):**
- If using qcow2 disk images: qcow2 has native snapshot support (internal snapshots). Creating a disk snapshot is nearly instant (copy-on-write at the block layer).
- If using raw partitions: no disk snapshot possible at the block level. VM state snapshots still work (capture CPU + RAM), but the disk is live. Recommend qcow2 for users who want full rollback capability.
- If using NVMe passthrough: disk snapshots are impossible (the guest owns the hardware). Document this limitation clearly. VM state snapshots still capture CPU + RAM state.

**Operations:**
- `enlil snapshot create <guest> [name]` — create a named snapshot
- `enlil snapshot list <guest>` — list available snapshots
- `enlil snapshot restore <guest> <name>` — stop guest, restore state, resume
- `enlil snapshot delete <guest> <name>` — delete a snapshot
- Management TUI integration: snapshot/restore buttons per guest

**Use cases:**
- **Safe OS updates:** Auto-snapshot before Windows Update or `apt upgrade`. If the update breaks something, restore in seconds.
- **First boot under Enlil:** Snapshot the existing Windows installation before first VM boot. If Windows reactivation fails or something breaks, instant rollback.
- **Development:** Snapshot a clean state, run tests, restore to clean state. Repeat.
- **Checkpoints:** Save game state, experimental software installs, configuration changes. Roll back at will.

### 8.13 Plugin & Extension System

**Goal:** Allow third-party developers to extend Enlil without modifying core code — custom device backends, routing policies, monitoring integrations, GPU strategies, and automation scripts. The plugin system must be safe (a buggy plugin can't crash the hypervisor) and performant enough for the extension point's requirements.

**The core problem: performance tiers.**

Not all extension points have the same performance requirements. A vCPU exit handler runs millions of times per second and can't tolerate even 100ns of overhead. A USB routing policy runs once per device plug event and could tolerate 10ms. A monitoring exporter runs once per second and could tolerate 100ms. Using one plugin technology for all of these is the wrong approach.

**The answer: a tiered plugin architecture.**

```
┌─────────────────────────────────────────────────────────────┐
│                  ENLIL PLUGIN TIERS                          │
├──────────┬───────────────┬──────────────────────────────────┤
│ Tier     │ Technology    │ Use Cases                        │
├──────────┼───────────────┼──────────────────────────────────┤
│ NATIVE   │ Rust crates   │ Custom VirtIO device backends,   │
│ (0 over- │ compiled into │ GPU sharing strategies, custom   │
│  head)   │ Enlil binary  │ schedulers, EPT handlers,        │
│          │               │ anything on the vCPU hot path    │
├──────────┼───────────────┼──────────────────────────────────┤
│ WASM     │ Wasmtime      │ USB routing policies, display    │
│ (sand-   │ (AOT-compiled │ zone layout logic, notification  │
│  boxed,  │ WASM modules) │ filters, Enlil Bridge content    │
│  ~1.5x)  │               │ transforms, GPU strategy         │
│          │               │ selection, config preprocessors   │
├──────────┼───────────────┼──────────────────────────────────┤
│ SCRIPT   │ Lua (mlua) or │ Management automation, event     │
│ (inter-  │ Rhai embedded │ hooks, monitoring dashboards,    │
│  preted, │ scripting     │ alerting rules, config macros,   │
│  ~10x)   │               │ one-off admin tasks              │
└──────────┴───────────────┴──────────────────────────────────┘
```

#### 8.13.1 Why This Tiered Approach

**WASM performance reality (from research):**
- WASM with AOT compilation (Wasmtime) runs 1.3x–2.5x slower than native Rust across SPEC CPU benchmarks
- The overhead comes from: sandboxed memory (bounds checking), limited SIMD (128-bit vs native 256/512-bit), and function call overhead at the WASM↔host boundary
- For hot-path code (vCPU exit handling at >1M exits/sec), even 1.5x overhead adds unacceptable microseconds per exit
- For warm-path code (USB routing, policy decisions, per-frame display logic), 1.5x overhead on a function that takes 10μs natively means 15μs — imperceptible
- For cold-path code (monitoring exports, management API handlers), even 10x overhead is fine

**Why not WASM for everything:** A WASM-only plugin system would force performance-critical extensions (custom device backends, GPU command interceptors) to accept 1.5x–2.5x overhead. That's the difference between a device backend that can handle 1M IOPS and one that handles 500K IOPS. Users who need custom hot-path code would bypass the plugin system entirely.

**Why not native-only:** A native-only plugin system (Rust crates compiled into the binary) provides zero overhead but: requires recompiling Enlil for every plugin change, can't sandbox plugins (a buggy plugin crashes the hypervisor), and has a high barrier to entry (must know Rust, must build against Enlil's internal APIs).

**The tiered approach gives the right tool for each job:**
- Performance-critical? Write a Rust crate, compile it in. Zero overhead, full access.
- Needs sandboxing and hot-reload? Write a WASM module. 1.5x overhead, can't crash the hypervisor, loadable at runtime.
- Quick automation or one-off script? Write a Lua/Rhai script. 10x overhead, instant iteration, zero compilation.

#### 8.13.2 Native Plugins (Rust Crate Tier)

Native plugins are Rust crates that implement Enlil's extension traits and are compiled into the Enlil binary at build time. Zero runtime overhead — they're just normal Rust code.

**Extension trait examples:**
```rust
/// Custom VirtIO device backend
pub trait VirtioDevicePlugin: Send + Sync {
    fn device_id(&self) -> u32;
    fn num_queues(&self) -> usize;
    fn handle_queue(&self, queue_idx: usize, chain: DescriptorChain) -> PluginResult;
    fn handle_config_read(&self, offset: u64, data: &mut [u8]);
    fn handle_config_write(&self, offset: u64, data: &[u8]);
}

/// Custom GPU sharing strategy
pub trait GpuStrategyPlugin: Send + Sync {
    fn name(&self) -> &str;
    fn probe(&self, gpus: &[PciDevice]) -> bool;  // can this strategy work?
    fn init(&mut self, gpus: &[PciDevice], guests: &[GuestConfig]) -> PluginResult;
    fn handle_mmio(&mut self, guest: GuestId, addr: u64, data: &[u8], write: bool);
    fn present_frame(&self, guest: GuestId) -> Option<Framebuffer>;
}

/// Custom vCPU exit handler (EXTREME hot path — must be fast)
pub trait VcpuExitPlugin: Send + Sync {
    fn handle_exit(&self, vcpu: &mut VcpuState, exit: &VmExit) -> ExitAction;
}
```

**Plugin registration:**
```toml
# Cargo.toml — enable plugins at build time
[features]
plugin-my-custom-device = ["enlil-plugin-my-device"]
plugin-custom-gpu = ["enlil-plugin-gpu-strategy"]
```

**When to use:** Custom VirtIO devices (e.g., a specialized hardware accelerator), custom GPU sharing strategies for specific hardware, performance-critical vCPU exit handlers (e.g., custom CPUID emulation for specific anti-cheat bypasses), custom EPT fault handlers.

#### 8.13.3 WASM Plugins (Sandboxed Tier)

WASM plugins are `.wasm` modules loaded at runtime by Enlil's embedded Wasmtime runtime. They can't crash the hypervisor, can be hot-reloaded without restart, and are language-agnostic (write in Rust, C, Go, AssemblyScript, etc. and compile to WASM).

**Runtime: Wasmtime with AOT compilation.**
- Wasmtime is already a Rust-native WASM runtime (same project as Cranelift)
- AOT (Ahead-of-Time) compilation: WASM modules are compiled to native code at load time, not interpreted. This gets performance to 1.3x–1.5x native — acceptable for warm-path code.
- **WASI (WebAssembly System Interface):** Plugins get controlled access to: filesystem (read config files), network (send metrics), logging (write to Enlil's log), and a defined set of Enlil APIs exposed as WASI imports.

**Enlil Host API (exposed to WASM plugins as imports):**
```rust
/// Functions Enlil exposes to WASM plugins
#[wasm_import_module = "enlil"]
extern "C" {
    // Query system state
    fn enlil_get_guest_count() -> u32;
    fn enlil_get_guest_name(id: u32, buf: *mut u8, len: u32) -> u32;
    fn enlil_get_cpu_usage(guest_id: u32) -> f32;
    fn enlil_get_gpu_queue_depth() -> u32;

    // USB routing
    fn enlil_usb_get_devices(buf: *mut u8, len: u32) -> u32;
    fn enlil_usb_route_device(vid: u16, pid: u16, guest_id: u32) -> i32;

    // Display zones
    fn enlil_zone_set_layout(layout_json: *const u8, len: u32) -> i32;
    fn enlil_zone_get_focused_guest() -> u32;

    // Event subscription
    fn enlil_subscribe_event(event_type: u32, callback_id: u32) -> i32;

    // Logging
    fn enlil_log(level: u32, msg: *const u8, len: u32);
}
```

**Plugin lifecycle:**
1. User places `my-plugin.wasm` in `/enlil/plugins/`
2. On Enlil boot (or hot-reload command), Wasmtime AOT-compiles the module
3. Plugin's `init()` function is called — plugin subscribes to events it cares about
4. When subscribed events fire (USB plug, timer tick, zone focus change, etc.), Enlil calls the plugin's handler
5. Plugin can query Enlil state and take actions via the host API
6. `enlil plugin reload my-plugin` — hot-reload without guest disruption

**Example WASM plugin — smart USB routing:**
```rust
// Compiled to WASM — runs sandboxed inside Enlil
// Automatically routes gaming peripherals to the Windows guest

#[no_mangle]
pub extern "C" fn on_usb_connect(vid: u16, pid: u16, port: u32) {
    // Gaming mice (Logitech, Razer, SteelSeries)
    let gaming_vendors = [0x046d, 0x1532, 0x1038];

    if gaming_vendors.contains(&vid) {
        let windows_guest = 1; // guest ID for Windows
        unsafe { enlil_usb_route_device(vid, pid, windows_guest); }
        unsafe { enlil_log(1, b"Routed gaming device to Windows\0".as_ptr(), 33); }
    }
}
```

**Example WASM plugin — dynamic zone layout:**
```rust
// Switch display layout based on time of day
#[no_mangle]
pub extern "C" fn on_timer_tick() {
    let hour = get_current_hour();
    if hour >= 9 && hour < 17 {
        // Work hours: 70/30 split (Linux main, Windows sidebar)
        set_layout("coding");
    } else {
        // After hours: fullscreen Windows (gaming)
        set_layout("fullscreen-windows");
    }
}
```

**Performance budget:** Each WASM plugin call should complete within 100μs for event handlers. Enlil enforces a timeout — if a plugin exceeds it, the call is aborted and the plugin is flagged. This prevents a misbehaving plugin from stalling the hypervisor.

**Security:** WASM's sandboxed memory model means a plugin can only access memory explicitly shared with it via the host API. It cannot read guest memory, hypervisor memory, or other plugins' memory. A plugin crash (trap) is caught by Wasmtime and logged — it doesn't bring down Enlil.

**When to use:** USB routing rules, display zone automation, notification filtering, Enlil Bridge content transformation (e.g., clipboard format conversion), GPU strategy selection heuristics, custom metrics collection, event-driven automation.

#### 8.13.4 Script Plugins (Automation Tier)

For quick one-off tasks, event hooks, and management automation, embed a lightweight scripting language. No compilation needed — edit a script, it takes effect immediately.

**Runtime options:**
- **Rhai** (Rust-native scripting engine, no external dependencies, sandboxed, ~300KB) — recommended
- **Lua** (via `mlua` crate, well-known, fast for a scripting language, tiny runtime) — alternative

**Use cases:**
```rhai
// /enlil/scripts/auto-snapshot.rhai
// Auto-snapshot before Windows Update runs

fn on_event(event) {
    if event.type == "guest_process_start"
       && event.guest == "windows"
       && event.process_name == "wuauclt.exe" {
        enlil.snapshot_create("windows", "pre-update-auto");
        enlil.log("Auto-snapshot created before Windows Update");
    }
}
```

```rhai
// /enlil/scripts/power-policy.rhai
// Reduce Windows guest resources when on battery

fn on_event(event) {
    if event.type == "battery_change" {
        if event.percent < 20 {
            enlil.guest_set_cpus("windows", [6, 7]);     // reduce to 2 cores
            enlil.guest_set_memory("windows", 8192);      // reduce to 8GB
            enlil.log("Low battery: reduced Windows resources");
        } else {
            enlil.guest_set_cpus("windows", [4, 5, 6, 7]); // full 4 cores
            enlil.guest_set_memory("windows", 16384);       // full 16GB
        }
    }
}
```

**When to use:** Management automation, event hooks, alerting, custom power policies, scheduled tasks, one-off admin operations.

#### 8.13.5 Management API (REST/gRPC)

All plugin tiers and external tools can interact with Enlil via a management API exposed over a Unix socket (local) or TCP (remote, authenticated).

**API surface:**
- Guest lifecycle: start, stop, reboot, snapshot, restore
- Resource management: CPU allocation, memory, GPU assignment
- USB routing: list devices, route, re-route
- Display: get/set zone layouts, focus guest
- Metrics: CPU/GPU/memory/IO per guest, per-second resolution
- Plugin management: list, load, reload, unload
- Event stream: subscribe to real-time events (WebSocket or gRPC streaming)

**Formats:** JSON over REST (simple) or Protobuf over gRPC (efficient). Both over the same Unix socket.

**External integrations this enables:**
- **Prometheus/Grafana:** Export metrics in Prometheus format for dashboards
- **Home Assistant:** Control Enlil guests via home automation (e.g., "when I sit at my desk, boot the Windows guest")
- **Custom GUIs:** Build a web-based management dashboard that talks to the API
- **CI/CD:** Automated testing that creates snapshots, runs tests, restores (via API calls)

**Config:**
```toml
[plugins]
wasm_dir = "/enlil/plugins"
script_dir = "/enlil/scripts"
wasm_timeout_us = 100          # max microseconds per WASM plugin call
script_timeout_ms = 1000       # max milliseconds per script execution

[api]
enabled = true
socket = "/run/enlil/api.sock" # Unix socket for local access
tcp_port = 0                   # 0 = disabled; set to e.g. 9100 for remote access
auth = "token"                 # none | token | mtls
metrics_format = "prometheus"  # prometheus | json
```

---

## Phase 9 — Compute Fabric (Heterogeneous Work Routing)

**Goal:** Automatically route GPU compute workloads to whichever hardware (CPU or GPU) has capacity, transparently to applications. Applications use standard Vulkan/OpenCL APIs with zero code changes.

**Duration:** 12–16 weeks (after Phase 7 GPU infrastructure exists)

### 9.1 Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│                     GUEST OS                            │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌───────────────────────┐ │
│  │ App A    │  │ App B    │  │ App C                 │ │
│  │ (Vulkan  │  │ (OpenCL) │  │ (DirectCompute/CUDA)  │ │
│  │ Compute) │  │          │  │                       │ │
│  └────┬─────┘  └────┬─────┘  └───────────┬───────────┘ │
│       │              │                    │             │
│  ┌────┴──────────────┴────────────────────┴──────────┐  │
│  │          Enlil Compute ICD (Installable            │  │
│  │          Client Driver)                            │  │
│  │                                                    │  │
│  │  • Vulkan ICD — captures SPIR-V at pipeline create │  │
│  │  • OpenCL ICD — captures SPIR-V/CL kernels        │  │
│  │  • CUDA shim — translates PTX → SPIR-V (stretch)  │  │
│  │                                                    │  │
│  │  Forwards SPIR-V + buffer descriptors + dispatch   │  │
│  │  dimensions to hypervisor via VirtIO channel       │  │
│  └────────────────────┬───────────────────────────────┘  │
│                       │                                  │
├───────────────────────┼──────────────────────────────────┤
│  ENLIL HYPERVISOR     │ VirtIO Compute Queue             │
│                       ▼                                  │
│  ┌─────────────────────────────────────────────────────┐ │
│  │              COMPUTE FABRIC                         │ │
│  │                                                     │ │
│  │  ┌───────────┐  ┌────────────┐  ┌───────────────┐  │ │
│  │  │ Kernel    │  │ Work       │  │ Compilation   │  │ │
│  │  │ Registry  │  │ Router     │  │ Cache         │  │ │
│  │  │           │  │            │  │               │  │ │
│  │  │ SPIR-V    │  │ Scores     │  │ SPIR-V hash → │  │ │
│  │  │ hash →    │  │ workload   │  │ compiled      │  │ │
│  │  │ metadata  │  │ for CPU    │  │ native code   │  │ │
│  │  │ (dims,    │  │ vs GPU     │  │ (both CPU     │  │ │
│  │  │ memory    │  │ fitness    │  │ and GPU       │  │ │
│  │  │ pattern)  │  │            │  │ variants)     │  │ │
│  │  └───────────┘  └─────┬──────┘  └───────────────┘  │ │
│  │                   ┌───┴───┐                         │ │
│  │                   │       │                         │ │
│  │              ┌────▼──┐ ┌──▼─────┐                   │ │
│  │              │ CPU   │ │ GPU    │                   │ │
│  │              │ Back  │ │ Back   │                   │ │
│  │              │ end   │ │ end    │                   │ │
│  │              └───────┘ └────────┘                   │ │
│  └─────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────┘
```

### 9.2 Guest-Side: Enlil Compute ICD

The guest installs standard-looking drivers — no different from installing VirtIO drivers or any other hardware driver. Applications are completely unaware.

**Vulkan ICD (Installable Client Driver):**
- Implements the Vulkan API surface for compute operations
- When `vkCreateComputePipelines()` is called, the ICD captures the SPIR-V shader module *before* any compilation to native GPU ISA
- When `vkCmdDispatch()` is called, the ICD packages:
  - SPIR-V bytecode (or a hash if previously submitted)
  - Buffer bindings (descriptor set layout + bound memory regions)
  - Dispatch dimensions (workgroup count X/Y/Z, local size)
  - Push constant data
- Sends this package to the hypervisor via a VirtIO queue
- Blocks (or returns a fence) until the hypervisor signals completion
- Results appear in the guest's output buffers via shared memory mapping

**OpenCL ICD:**
- Same pattern: capture kernel source or SPIR-V at `clCreateProgramWithSource()` / `clCreateProgramWithIL()`
- Capture dispatch at `clEnqueueNDRangeKernel()`
- Forward to hypervisor via same VirtIO channel

**CUDA Shim (Deprioritized — industry converging on SPIR-V):**
- Microsoft announced (September 2024) that DirectX 12 will accept SPIR-V starting with Shader Model 7, replacing DXIL
- The SPIR-V backend became an official LLVM target (December 2024), providing unified compute/graphics coverage
- This means even Windows DirectCompute workloads will natively produce SPIR-V — the CUDA shim becomes less critical
- If still needed: intercept CUDA runtime API calls (`cudaLaunchKernel`), translate PTX → SPIR-V
- PTX → SPIR-V translation tooling exists but CUDA's memory model and API surface are large

**Platforms:**
- Linux guest: ship as `.so` ICD libraries, register via `/etc/vulkan/icd.d/` and `/etc/OpenCL/vendors/`
- Windows guest: ship as ICD DLLs, register via registry keys
- Both are well-defined ICD registration mechanisms used by all GPU vendors today

### 9.3 VirtIO Compute Device

Define a new VirtIO device type for the fabric communication channel:

```
VirtIO Device ID: (custom, e.g., 0x4E4C — "NL" for Enlil)

Queues:
  Queue 0: Submit    (guest → host: dispatch requests)
  Queue 1: Complete  (host → guest: completion notifications)
  Queue 2: Control   (bidirectional: capability negotiation, stats)

Submit Descriptor Format:
  ┌──────────────────────────────┐
  │ kernel_id: u64               │  SPIR-V hash (0 = new kernel)
  │ spir_v_len: u32              │  0 if kernel_id is cached
  │ spir_v_data: [u8]            │  SPIR-V bytecode (if new)
  │ dispatch_x: u32              │  Workgroup count X
  │ dispatch_y: u32              │  Workgroup count Y
  │ dispatch_z: u32              │  Workgroup count Z
  │ local_size_x: u32            │  Workgroup size X
  │ local_size_y: u32            │  Workgroup size Y
  │ local_size_z: u32            │  Workgroup size Z
  │ num_buffers: u32             │  Number of bound buffers
  │ buffers: [BufferBinding]     │  GPA + size + access flags
  │ push_constants: [u8]         │  Push constant data
  │ priority: u8                 │  Guest-assigned priority hint
  │ fence_id: u64                │  For async completion tracking
  └──────────────────────────────┘

Completion Descriptor Format:
  ┌──────────────────────────────┐
  │ fence_id: u64                │  Matches submitted fence
  │ status: u32                  │  Success / error code
  │ execution_ns: u64            │  Actual execution time
  │ backend: u8                  │  0=GPU, 1=CPU (for stats)
  └──────────────────────────────┘
```

### 9.4 Hypervisor-Side: Compute Fabric Core

**Kernel Registry:**
- Index SPIR-V kernels by hash (SHA-256 of bytecode)
- On first submission: analyze the SPIR-V to extract metadata:
  - Memory access patterns (sequential, strided, random)
  - Arithmetic intensity (ALU ops per memory op)
  - Control flow complexity (branches, loops)
  - Workgroup shared memory usage
  - Required capabilities (float64, atomics, etc.)
- Store analysis results for routing decisions on subsequent dispatches

**Compilation Cache:**
- For each unique SPIR-V kernel, compile and cache BOTH backends:
  - **GPU variant:** compile SPIR-V → native GPU ISA via the physical GPU's compiler (use Vulkan compute pipeline on the host side during KVM development, or vendor-specific compiler for bare-metal)
  - **CPU variant:** compile SPIR-V → native x86 SIMD code
- CPU compilation strategy (**recommend LLVM over cranelift** — see rationale below):
  - **Primary path (LLVM):** Use the now-official LLVM SPIR-V backend (promoted from experimental, Dec 2024) to lower SPIR-V → LLVM IR, then compile to x86 with AVX2/AVX-512 vectorization. Intel's oneAPI compiler already does exactly this via the `spir64_x86_64` target triple — it's a production-proven SPIR-V → x86 SIMD pipeline. Use LLVM via the `inkwell` Rust crate for LLVM bindings.
  - **Fallback path (cranelift):** Lighter weight, faster compilation, but weaker vectorization. Use for development/testing or when LLVM is too heavy.
  - **Why LLVM wins:** LLVM's SPIR-V backend understands GPU-style parallelism natively. Its auto-vectorizer produces better SIMD code than cranelift for parallel workloads. Intel has invested years optimizing the SPIR-V → x86 path through oneAPI/SYCL. We should reuse that work, not replicate it.
  - Map GPU workgroups → CPU threads (one thread per workgroup)
  - Map GPU invocations within a workgroup → SIMD lanes (AVX2: 8-wide, AVX-512: 16-wide)
  - Handle GPU shared memory (`workgroup` storage class) → thread-local stack allocation
  - **Warm-up strategy (from Intel oneAPI JIT research):** On first guest boot, pre-compile commonly-used SPIR-V kernels to both backends. Cache by SPIR-V hash. Support eager (all kernels at load time) and lazy (on first dispatch) JIT modes.
- Cache compiled variants to disk so reboot doesn't re-trigger JIT

### 9.5 Work Router

**Design for future phases:** The work router must accept `ComputeTarget` trait objects, not hard-code local GPU/CPU assumptions. Phase 11.8 extends the compute fabric across the mesh — dispatches can route to GPUs on remote machines. Phase 11.8a splits dispatches into work shares distributed across multiple targets. If the router only understands "local GPU" and "local CPU," adding remote targets and split dispatches requires rewriting the routing logic. Define the trait now:

```rust
trait ComputeTarget: Send + Sync {
    fn target_type(&self) -> TargetType;    // LocalGpu, LocalCpu, RemoteGpu, RemoteCpu
    fn estimated_latency(&self) -> Duration; // includes network RTT for remote targets
    fn available_capacity(&self) -> f64;     // 0.0 = saturated, 1.0 = idle
    fn capabilities(&self) -> &TargetCaps;   // VRAM size, compute units, supported features
    async fn dispatch(&self, kernel: &CompiledKernel, buffers: &BufferSet, grid: Grid) -> Result<()>;
}
```

Phase 9 implements `LocalGpuTarget` and `LocalCpuTarget`. Phase 11 adds `RemoteGpuTarget` (serializes dispatch over mesh) and `SplitTarget` (distributes workgroups across multiple targets).

The router makes per-dispatch decisions. It does NOT split a single dispatch across CPU and GPU (that would require complex synchronization). Each dispatch goes entirely to one backend.

**Decision inputs:**
- Kernel metadata (from analysis at registration time)
- Current GPU queue depth (how many dispatches are pending)
- Current CPU load (how many cores are available for compute)
- Dispatch size (total invocations = grid X × grid Y × grid Z × local size)
- Historical execution times for this kernel on each backend
- Per-guest priority weights from config

**Routing heuristics:**

```
Score_GPU = base_gpu_fitness(kernel_metadata)
          × gpu_capacity_factor(queue_depth)
          × size_factor(total_invocations)

Score_CPU = base_cpu_fitness(kernel_metadata)
          × cpu_capacity_factor(available_cores)
          × size_factor(total_invocations)

Route to: argmax(Score_GPU, Score_CPU)
```

Where:
- `base_gpu_fitness`: high for large parallel workloads, simple control flow, high arithmetic intensity
- `base_cpu_fitness`: high for small dispatches, complex branching, low invocation counts
- `gpu_capacity_factor`: decreases as GPU queue fills up (GPU is busy)
- `cpu_capacity_factor`: decreases as CPU cores are occupied by guest vCPUs
- `size_factor`: very small dispatches (< 1000 invocations) penalize GPU due to launch overhead

**Adaptive learning:**
- Track actual execution times per kernel per backend
- After N executions, replace heuristic scores with empirical measurements
- If a kernel consistently runs faster on CPU, always route there (and vice versa)

### 9.6 CPU Backend

**Architecture:**
```
SPIR-V Kernel
      │
      ▼
┌──────────────┐
│ LLVM SPIR-V  │  SPIR-V → LLVM IR → x86 SIMD
│ Backend      │  (official LLVM target since
│ (or spirv-   │   Dec 2024, Intel oneAPI proven)
│  cross +     │  GPU workgroup → function call
│  cranelift)  │  GPU invocation → SIMD lane
└──────┬───────┘
       │
       ▼
┌──────────────┐
│ LLVM JIT     │  LLVM IR → native x86 (AVX2/AVX-512)
│ (via inkwell │  Compile once, cache forever
│  Rust crate) │  Warm-up: pre-compile on guest boot
└──────┬───────┘
       │
       ▼
┌──────────────┐
│ Thread Pool   │  Steal work from guest vCPU cores
│              │  when they're idle, or use reserved
│ Workgroup N  │  compute cores
│   → Thread N │
└──────────────┘
```

- Maintain a thread pool sized to available CPU cores not assigned to guest vCPUs
- If `scheduling = "auto"` and cores are time-sliced, compute threads run at lower priority during guest vCPU time slices
- Support `workgroupBarrier()` by using OS-level barriers within each thread (each thread = one workgroup)
- Map SPIR-V buffer bindings to host memory via guest physical address → host virtual address translation (already available from EPT/NPT page tables)

### 9.7 GPU Backend

- During KVM development phase: create a Vulkan compute pipeline on the host for each kernel, dispatch via `vkCmdDispatch`
- During bare-metal phase: submit directly to physical GPU command queue
- Map guest buffers into GPU-accessible memory (requires IOMMU configuration)
- Handle GPU memory allocation for intermediate buffers

### 9.8 Buffer Memory Management

The critical challenge: guest buffers live in guest physical memory, but the CPU/GPU backends need to access them.

- **For CPU backend:** straightforward — the hypervisor already has the host virtual address for every guest physical page (via EPT/NPT). Just translate and access.
- **For GPU backend:** guest physical pages must be mapped into GPU-accessible address space via IOMMU. Pin the guest pages, create IOMMU mappings, pass GPU-visible addresses to the compute pipeline.
- **Zero-copy goal:** avoid copying buffer contents. Both backends should operate directly on guest memory where possible.
- **Coherence:** CPU backend results are immediately visible to the guest. GPU backend results may require a cache flush or IOMMU sync before signaling completion.
- **Confidential VM constraint (Phase 8.7 interaction):** Under AMD SEV-SNP or Intel TDX, guest memory is encrypted — the hypervisor sees only ciphertext. Zero-copy is impossible. Two alternatives:
  - **Bounce buffers:** Guest copies input data to a shared unencrypted memory region (designated via GHCB/TDCALL), fabric executes on the unencrypted copy, guest copies results back. Adds latency proportional to buffer size.
  - **Guest-initiated sharing:** Guest explicitly marks compute buffer pages as shared/unencrypted via the `VirtIO Compute` control queue. Fabric operates on these pages directly. Faster but requires guest cooperation (the Enlil ICD driver handles this transparently).
  - Implement dual paths: zero-copy for non-confidential guests, shared-page for confidential guests. The ICD detects CVM mode and selects automatically.

### 9.9 Cross-Guest Work Stealing (Stretch Goal)

When Guest A's GPU compute queue is saturated but Guest B's CPU cores are idle, the fabric could route Guest A's overflow to Guest B's CPU time — without either guest knowing.

- Requires careful memory isolation (Guest A's buffers must not leak to Guest B)
- Hypervisor-mediated buffer access: fabric copies input data to a neutral buffer, runs compute, copies results back
- Config: `[fabric] cross_guest_stealing = true | false`

### 9.10 Configuration

```toml
[fabric]
enabled = true
cross_guest_stealing = false

[fabric.routing]
strategy = "adaptive"           # adaptive | gpu_prefer | cpu_prefer | gpu_only | cpu_only
gpu_queue_depth_threshold = 8   # start routing to CPU when GPU has 8+ pending dispatches
min_invocations_for_gpu = 1024  # dispatches smaller than this default to CPU
learning_window = 100           # number of executions before switching to empirical routing

[fabric.cpu_backend]
max_threads = 4                 # max CPU threads for compute (0 = auto from idle cores)
simd = "auto"                   # auto | avx2 | avx512 | none
jit = "llvm"                    # llvm | cranelift (llvm recommended — official SPIR-V backend)

[fabric.gpu_backend]
reserved_vram_mb = 256          # VRAM reserved for fabric compute buffers

[fabric.cache]
path = "/var/enlil/kernel_cache"
max_size_mb = 512
```

### 9.11 What This Enables

Once the fabric is running, these scenarios work automatically with zero application changes:

- **ML Inference overflow:** A Windows guest runs an AI upscaler via Vulkan compute. The GPU is busy rendering a game. The fabric routes the upscaler kernel to idle CPU cores. The application sees slightly higher latency but never stalls.
- **Multi-guest load balancing:** Guest A is doing heavy GPU compute (video encoding). Guest B submits a small compute shader (image filter). The fabric routes Guest B's work to CPU instantly instead of queuing behind Guest A's work on the shared GPU.
- **GPU failure graceful degradation:** If the GPU hangs or is reset, the fabric automatically falls back to CPU for all compute dispatches. Applications experience slower performance but don't crash.
- **Development/testing:** A developer on a Linux guest writes Vulkan compute shaders. They work even if the GPU is passed through entirely to the Windows guest — the fabric runs them on CPU.

### 9.12 Verifiable Compute via ZK Proofs (Stretch Goal)

When the fabric routes a SPIR-V kernel to the CPU backend instead of the GPU, the guest has no way to verify that the results are correct — it's trusting the hypervisor's JIT compiler. For security-sensitive compute, this is insufficient.

**Solution:** Generate a Zero-Knowledge proof of correct execution alongside the compute result.

- For each SPIR-V kernel dispatch routed to the CPU backend, optionally generate a ZK proof that:
  - The SPIR-V bytecode was correctly compiled (JIT output matches expected semantics)
  - The input buffers were not modified during execution
  - The output buffers contain the result of correctly executing the kernel on the provided inputs
- The guest receives both the result and the proof via the VirtIO completion queue
- Guest-side verifier (in the Enlil Compute ICD) checks the proof before returning results to the application
- If verification fails, the ICD can re-dispatch to the GPU backend as a fallback

**Implementation approach:**
- Use RISC Zero zkVM or SP1 to execute a reference SPIR-V interpreter inside the ZK circuit
- The prover runs the SPIR-V kernel inside the zkVM and produces a receipt (proof)
- The verifier (in-guest) checks the receipt against the expected SPIR-V hash and I/O commitments
- **Performance:** ZK proof generation is 100–1000x slower than native execution. This is NOT for every dispatch — it's an optional mode for security-sensitive workloads (financial modeling, regulated ML inference, medical compute). Config:
  ```toml
  [fabric.verification]
  mode = "none"        # none | sample | always
  sample_rate = 0.01   # verify 1% of dispatches (statistical confidence)
  framework = "risc0"  # risc0 | sp1
  ```
- `sample` mode: randomly verify a fraction of dispatches. If any verification fails, switch to `always` mode and alert.

---

## Phase 10 — Architecture Portability (ARM & RISC-V)

**Goal:** Extend Enlil to run on AArch64 and RISC-V hardware, leveraging the HAL trait defined in Phase 0 and the platform abstraction from Phase 1.

**Duration:** 12–20 weeks per architecture (after Phase 8 is stable on x86)

**Prerequisites:** Phase 0–8 complete and stable on x86. The HAL trait (`HypervisorBackend`) and platform abstraction (`enlil-platform`) must be proven clean by having the entire x86 implementation behind them.

### 10.1 What's Architecture-Specific vs Architecture-Neutral

```
ARCHITECTURE-NEUTRAL (works unchanged on ARM/RISC-V):
├── enlil-config          — guest definitions, routing rules, TOML parsing
├── enlil-mgmt            — management TUI, monitoring, metrics
├── enlil-devices          — VirtIO backends, USB router, GPU arbiter
│   └── (VirtIO protocol is arch-neutral by design)
├── Phase 9 Compute Fabric — SPIR-V compilation, work routing
│   └── (LLVM targets ARM/RISC-V natively — CPU JIT works)
├── Phase 8.9 ZK Proofs   — verification is pure computation
└── All business logic above the HAL trait

ARCHITECTURE-SPECIFIC (must be re-implemented per arch):
├── enlil-hal impl         — VMX/SVM → ARM EL2 → RISC-V H extension
├── enlil-platform impl    — x86 TSC → ARM generic timer → RISC-V timer
│   ├── Memory management  — x86 page tables → ARM page tables → RISC-V Sv48
│   ├── Interrupt handling — APIC → GICv3/v4 → PLIC/ACLINT
│   ├── IOMMU             — VT-d/AMD-Vi → ARM SMMU → RISC-V IOMMU
│   └── Boot              — UEFI x86 → UEFI ARM / device tree → OpenSBI
└── enlil-boot             — architecture-specific UEFI/boot payload
```

### 10.2 Phase 10a — ARM (AArch64)

**Target hardware:**
- Development: Raspberry Pi 5 (Cortex-A76, GICv2), Apple M-series (via Hypervisor.framework for hosted mode)
- Production: Ampere Altra (datacenter ARM, GICv3, SMMU), NVIDIA Jetson (for embedded)

**ARM virtualization architecture (EL2):**
- ARM uses Exception Levels: EL0 (user), EL1 (kernel), EL2 (hypervisor), EL3 (secure monitor/firmware)
- Enlil runs at EL2 — equivalent to VMX root mode on Intel
- Guest OSes run at EL1, trapped to EL2 on sensitive operations
- Stage-2 page tables (equivalent to EPT/NPT) provide memory virtualization
- GICv3/v4 (Generic Interrupt Controller) provides native interrupt virtualization — GICv4 supports direct injection of virtual interrupts without hypervisor intervention (equivalent to Intel posted interrupts)

**Implementation:**
- Implement `ArmBackend` for the `HypervisorBackend` trait
- Port `enlil-platform` bare-metal backend:
  - Memory: ARM page table format (4KB/16KB/64KB granules, 4-level tables)
  - Timer: ARM Generic Timer (CNTPCT_EL0 for monotonic clock — equivalent to TSC)
  - Interrupts: GICv3 distributor + redistributor + CPU interface
  - IOMMU: ARM SMMU (System Memory Management Unit) for device passthrough
- **GPU on ARM:** ARM Mali GPUs don't support SR-IOV. Apple M-series GPUs are proprietary. Ampere servers typically have PCIe GPU slots (NVIDIA/AMD discrete). GPU strategy is mostly passthrough or compute fabric.
- **References:**
  - Rust-Shyper (AArch64 native, open source, tested on Jetson TX2)
  - Diosix (RISC-V but architectural patterns transfer)
  - Linux KVM ARM64 implementation in `arch/arm64/kvm/`

### 10.3 Phase 10b — RISC-V

**Target hardware:**
- Development: QEMU RISC-V (virt machine), SiFive HiFive Unmatched
- Production: RISC-V server hardware with H extension (emerging — limited options in 2025)

**RISC-V H extension:**
- H (Hypervisor) extension adds VS-mode (virtual supervisor) and VU-mode (virtual user)
- HS-mode (hypervisor supervisor) is where Enlil runs — equivalent to EL2 / VMX root
- `hgatp` CSR controls guest physical → host physical address translation (equivalent to EPT)
- Guest traps to HS-mode via the standard exception mechanism
- H extension is still ratified relatively recently — hardware support is growing but not ubiquitous

**Implementation:**
- Implement `RiscVBackend` for the `HypervisorBackend` trait
- Port `enlil-platform` bare-metal backend:
  - Memory: RISC-V Sv48 page tables (4-level, 4KB pages)
  - Timer: RISC-V `mtime`/`mtimecmp` for timer interrupts
  - Interrupts: PLIC (Platform-Level Interrupt Controller) or ACLINT
  - IOMMU: RISC-V IOMMU specification (still maturing)
- **References:**
  - Diosix: open-source bare-metal Rust hypervisor for RISC-V (primary reference)
  - Linux KVM RISC-V implementation in `arch/riscv/kvm/`

### 10.4 Phase 10c — Compute Fabric Cross-Architecture

- The LLVM SPIR-V → native code pipeline already targets ARM (NEON/SVE) and RISC-V (V extension) — the compute fabric's CPU JIT backend works on all architectures with no Enlil-level changes
- VirtIO protocol is architecture-neutral — the Enlil Compute ICD guest driver works on ARM/RISC-V Linux guests without modification
- The work router's heuristics may need re-tuning (ARM/RISC-V have different CPU/GPU performance profiles)

---

## Phase 11 — Enlil Mesh (Multi-Machine Distributed Hypervisor)

**The vision:** Multiple physical PCs, each running Enlil, form a single logical hypervisor. Guests can span machines, migrate between them, share GPUs across the network, and composite displays from multiple physical locations — all with cryptographic proof that every node in the mesh is running genuine, unmodified Enlil and maintaining guest isolation.

**Why ZK proofs make this fundamentally different:** Every existing multi-machine hypervisor (VMware vSphere/DRS, Proxmox cluster, XCP-ng pool, GiantVM) requires all nodes to implicitly trust each other or trust a central management server. If one node is compromised, every guest on every node is potentially compromised. Enlil Mesh eliminates this with the ZK attestation system from Phases 8.7/8.9: each node cryptographically proves its integrity to every other node. No trust assumptions. No central authority. A guest can verify — with a 200-byte SNARK proof — that every machine touching its memory, CPU, or GPU is running genuine Enlil with correct isolation.

This is the Ethereum model applied to infrastructure: replace N-of-N trust (every node re-verifies every other node's state) with 1-of-N proof (each node proves once, all others verify a tiny proof).

---

### 11.1 Network Tiers — Latency Determines Capability

Not all connections are equal. Enlil Mesh defines four network tiers, each enabling progressively more capabilities as latency decreases:

```
┌──────────────────────────────────────────────────────────────────────┐
│                     ENLIL MESH NETWORK TIERS                         │
│                                                                      │
│  Tier 4: INTERNET (WAN)     — 20–300ms RTT, 1–1000 Mbps            │
│  ├── Compute fabric offload (batch, async)                          │
│  ├── Cold/warm VM migration                                         │
│  ├── Shared filesystem sync (eventually consistent)                  │
│  ├── ZK attestation exchange                                        │
│  ├── Remote display streaming (compressed, Sunshine/Moonlight-style)│
│  └── Clipboard/notification forwarding                              │
│                                                                      │
│  Tier 3: LOCAL NETWORK (LAN) — 0.1–1ms RTT, 1–100 Gbps            │
│  ├── Everything in Tier 4, plus:                                    │
│  ├── Live VM migration (pre-copy, <1s downtime)                     │
│  ├── Compute fabric offload (interactive, sync)                     │
│  ├── Cross-machine Enlil Zones (framebuffer streaming, uncompressed)│
│  ├── Enlil Bridge across machines (clipboard, drag-drop, URL routing)│
│  ├── Cross-machine virtual network switch (10+ Gbps)                │
│  └── Shared filesystem (NFS-like, strongly consistent)              │
│                                                                      │
│  Tier 2: RDMA FABRIC        — 1–5μs RTT, 25–400 Gbps              │
│  ├── Everything in Tier 3, plus:                                    │
│  ├── Distributed shared memory (DSM) — guests can span machines     │
│  ├── GPU VRAM remote access (GPUDirect RDMA)                        │
│  ├── Sub-millisecond live migration                                 │
│  └── Cross-machine compute fabric with near-local latency           │
│                                                                      │
│  Tier 1: CXL FABRIC         — 200–500ns RTT, 128+ GT/s             │
│  ├── Everything in Tier 2, plus:                                    │
│  ├── Hardware cache-coherent shared memory (no DSM protocol needed) │
│  ├── Transparent EPT mapping to remote DRAM                         │
│  ├── GPU memory pooling across machines                             │
│  └── Guests indistinguishable from running on one large machine     │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

**Automatic tier detection:** When a new node joins the mesh, Enlil runs a latency/bandwidth probe (ICMP + TCP + optional RDMA verbs + CXL enumeration). The measured RTT and bandwidth determine which tier the link operates at. Tier can be overridden manually in config.

---

### 11.2 Mesh Discovery & Coordination (All Tiers)

**Peer-to-peer, no central server.** Enlil Mesh has no "vCenter" or master node. Every node is equal. Coordination uses a gossip protocol (inspired by SWIM/Memberlist) for:
- Node discovery (mDNS for LAN auto-discovery, explicit IP/hostname for WAN)
- Health monitoring (heartbeat, failure detection)
- Resource advertisement (each node publishes: CPU cores, RAM, GPUs, storage, current load)
- Configuration synchronization (mesh-wide guest placement policies)

**Mesh state is eventually consistent.** Each node maintains a local view of the mesh that converges via gossip. No distributed consensus protocol (Raft/Paxos) for the common case — too heavy for a desktop hypervisor. Raft is only used for the narrow case of coordinated live migration (where exactly-once semantics matter).

**WireGuard mesh for WAN:** All inter-node communication over WAN is encrypted via WireGuard tunnels (built into the Linux kernel, trivial to add in bare-metal mode as the WireGuard protocol is simple). LAN nodes can optionally skip encryption for performance (configurable).

**Config:**
```toml
[mesh]
enabled = true
node_name = "office-desktop"
listen_port = 4242
discovery = "mdns"              # mdns | static | hybrid
static_peers = [                # for WAN or when mDNS is unavailable
  "192.168.1.100:4242",
  "home.example.com:4242",
]
wireguard_key = "auto"          # auto-generated or specified
trust_mode = "zk"               # zk | psk | open (zk = full ZK attestation)
```

---

### 11.3 ZK-Verified Mesh Trust (All Tiers)

**The core innovation.** When Node B joins the mesh, every existing node (A, C, D...) requires Node B to present a ZK attestation proof (Phase 8.7) before accepting it. The proof demonstrates:
1. Node B is running a genuine, unmodified Enlil binary (binary hash proof)
2. Node B's EPT/NPT tables are correctly configured for its local guests (structure proof)
3. Node B's IOMMU is active and correctly isolating devices (IOMMU proof)

Verification of the ~200-byte SNARK proof takes ~2ms. No trust in Node B's self-report. No trust in a central authority. Pure math.

**Ongoing attestation:** Nodes periodically re-attest (configurable interval, default: every 10 minutes). If a node fails to re-attest, it's quarantined — its guests continue to run locally but no new cross-mesh operations (migration, compute offload) are accepted.

**Cross-machine isolation certificates (Phase 8.9 extension):** When Guest A spans Machine 1 and Machine 2, the isolation proof must cover EPT tables on both machines. Each machine generates its local isolation segment proof, then these are recursively aggregated (Phase 8.7 technique) into a single cluster-wide isolation certificate. Guest A receives one proof covering all machines — not N separate proofs.

**WAN trust degradation:** Over the Internet, additional risks exist (MITM, replay, network partition). WireGuard handles transport security. ZK attestation handles node integrity. For network partitions: a guest whose pages span two machines that lose connectivity is paused (not crashed) until the link recovers or an administrator intervenes. This is the same behavior as a CXL link failure — fail-safe, not fail-silent.

---

### 11.4 Tier 4 — Internet / WAN Capabilities (20–300ms RTT)

The highest-latency tier. Everything here must tolerate packet loss, variable latency, and limited bandwidth. No shared memory, no live migration, no real-time display composition.

**11.4.1 Async Compute Fabric Offload**

The compute fabric (Phase 9) naturally extends over WAN for batch/async workloads. A guest on Machine A submits a SPIR-V kernel dispatch. The work router sees that Machine B (across the Internet) has an idle A100 GPU. For large, latency-insensitive dispatches (ML training batches, offline rendering, Monte Carlo simulations):

1. Machine A serializes the SPIR-V kernel + input buffers + dispatch dimensions
2. Sends over WireGuard tunnel to Machine B
3. Machine B executes on its GPU, optionally generates a ZK proof of correct execution (Phase 9.12)
4. Machine B returns results + proof to Machine A
5. Machine A's guest receives results via VirtIO completion queue

The key insight: the ZK proof makes this trustless. Machine A doesn't need to trust Machine B's GPU output — it verifies the proof. This enables "borrow your friend's GPU over the Internet" with cryptographic guarantees.

**Latency budget:** For a 100ms RTT WAN link, a compute dispatch that takes 5 seconds on the remote GPU has only 2% network overhead. For dispatches taking <100ms, WAN overhead dominates — the work router won't send these over WAN (threshold: dispatch_time > 50× RTT).

**Bandwidth:** Input/output buffers are compressed (zstd) before transmission. For ML workloads, model weights are cached on the remote node (content-addressed, deduplicated) — only activations and gradients traverse the WAN.

**Config:**
```toml
[mesh.compute.wan]
enabled = true
min_dispatch_seconds = 5.0      # don't send short jobs over WAN
max_input_mb = 500              # cap on input buffer size
compression = "zstd"
cache_model_weights = true      # cache immutable data on remote
zk_verify = true                # require ZK proof for remote results
```

**11.4.2 Cold & Warm VM Migration**

Over WAN, live migration (transferring memory while the guest runs) is impractical for most guests — a 16GB guest at 100 Mbps takes 21 minutes. Instead:

- **Cold migration:** Guest is paused → state + disk image transferred → guest resumes on destination. Uses qcow2 backing file deduplication: if both nodes have the same base OS image, only the delta (dirty blocks) needs to transfer. For a Windows guest with a 60GB disk but only 5GB of user delta, migration takes ~7 minutes at 100 Mbps.

- **Warm migration:** Guest is paused → memory state transferred → guest resumes on destination with disk access forwarded back to source via NBD (Network Block Device) over WireGuard. Guest resumes quickly (only memory, ~16GB). Disk blocks are pulled on-demand and migrated in the background. Guest experiences high disk latency until background migration completes.

- **Follow-the-sun:** Enlil can schedule migrations based on time-of-day. Your work desktop migrates from your office PC to your home PC at 6pm, and back at 8am. Policy-driven, not manual.

**11.4.3 Shared Filesystem Sync (Eventually Consistent)**

The Enlil Bridge shared filesystem (Phase 3.7.3) extends across WAN using bidirectional file sync (rsync-like delta transfer, conflict resolution via last-writer-wins or user-specified policy). Not POSIX-consistent — eventually consistent. Good for documents, downloads, project files. Not suitable for databases or locks.

**11.4.4 Remote Display Streaming**

Enlil Zones (Phase 3.6) can display a remote guest's framebuffer via network streaming. Over WAN, this uses hardware H.264/H.265 encoding (GPU NVENC/VCE/QSV) → RTP/RTSP stream → decode on local machine. This is functionally identical to Sunshine/Moonlight or Parsec — the difference is that it's integrated into Enlil Zones as just another zone source alongside local framebuffers.

Latency target: <30ms encode-transmit-decode for 1080p60 on a <50ms RTT link. Acceptable for desktop use, not competitive gaming.

**11.4.5 Cross-Machine Enlil Bridge (Subset)**

Over WAN, the following Enlil Bridge features work with added latency:
- Clipboard sync (text/images — file references become download links)
- Notification forwarding
- URL/protocol routing (click a link on remote guest → opens in local browser)

Drag-and-drop is impractical over WAN (file transfer latency too high for interactive feel) — it falls back to shared filesystem copy.

---

### 11.5 Tier 3 — LAN Capabilities (0.1–1ms RTT, 1–100 Gbps)

The sweet spot for "two PCs in the same room." Ethernet (1GbE, 10GbE, 25GbE) or WiFi 6E/7 (for the adventurous). All Tier 4 capabilities are available with dramatically better performance, plus:

**11.5.1 Live VM Migration**

Pre-copy live migration: while the guest runs, memory pages are iteratively copied to the destination. After convergence (dirty rate < transfer rate), the guest is briefly paused (target: <500ms stun time on 10GbE), remaining dirty pages are transferred, and the guest resumes on the destination.

- **10GbE:** 16GB guest migrates in ~15 seconds with <200ms stun time
- **25GbE:** 16GB guest migrates in ~6 seconds with <100ms stun time
- **Network transparency:** VirtIO-net MAC address is preserved. For bridged networking, the virtual switch updates the MAC table. For NATted guests, no change needed. Guest IP doesn't change.
- **GPU state:** If the guest uses VirtIO-GPU (Venus/VirGL), GPU context migrates naturally (it's just memory). If the guest uses passthrough, the GPU is detached (FLR) on source, re-attached on destination. There's a visible GPU glitch during migration — this is unavoidable with passthrough. SR-IOV VFs can potentially be migrated using the device's migration support (Intel, AMD GIM).
- **Storage:** Guests on shared storage (NFS, iSCSI, or Enlil shared-fs) migrate with zero disk transfer. Guests on local storage use storage live migration (background block copy, similar to warm migration but the guest keeps running).

**11.5.2 Synchronous Compute Fabric**

On LAN, the compute fabric can handle interactive-latency dispatches. A shader dispatch that takes 10ms on the remote GPU has ~0.5ms network overhead on 10GbE (5% overhead) — acceptable for real-time rendering assist, interactive ML inference, or build acceleration.

The work router's latency threshold drops to `dispatch_time > 10× RTT` for LAN peers, enabling much more aggressive offloading than WAN.

**11.5.3 Cross-Machine Enlil Zones (Uncompressed)**

On LAN with 10GbE+, framebuffers can be streamed uncompressed or with lightweight compression (LZ4). A 4K@60 framebuffer is ~12 Gbps uncompressed (too much for 10GbE) or ~2–4 Gbps with LZ4 frame differencing. On 25GbE, even uncompressed 1440p@144 is feasible.

This means: Machine A has the gaming GPU, Machine B has the display. Enlil Zones on Machine B composites Machine A's guest framebuffer alongside its own local guests, all on one physical monitor. Input routing works seamlessly — mouse in Machine A's zone sends input events over the mesh to Machine A's guest.

**11.5.4 Full Enlil Bridge Across Machines**

All seven Enlil Bridge subsystems (Phase 3.7) work transparently across LAN:
- Clipboard: <10ms round-trip, feels instantaneous
- Drag-and-drop: file copies at network speed (1GB file in <1s on 10GbE)
- Shared filesystem: strongly consistent (POSIX semantics via VirtIO-fs with NFS-like network backend)
- Notifications: real-time
- URL routing: cross-machine protocol handling
- Virtual network: cross-machine guests communicate at 10+ Gbps via the mesh virtual switch

---

### 11.6 Tier 2 — RDMA Fabric Capabilities (1–5μs RTT, 25–400 Gbps)

InfiniBand or RoCEv2 (RDMA over Converged Ethernet). This is GiantVM territory — but with Rust, ZK proofs, and the full Enlil feature stack.

**11.6.1 Distributed Shared Memory (Guests Spanning Machines)**

The breakthrough capability: a single guest VM can have vCPUs and memory pages on multiple physical machines. This is what GiantVM proved is possible.

**Architecture:**
- Each Enlil node runs a DSM module that intercepts EPT page faults
- When a guest accesses a page that physically resides on another node, the EPT entry is marked not-present
- The page fault traps to Enlil, which fetches the page via RDMA read (one-sided, ~2μs)
- The fetched page is cached locally and the EPT entry updated
- Coherency protocol: Ivy-style (single-writer, multiple-reader) or MESI-like (read-sharing with invalidation on write)

**GiantVM's lesson:** DSM is the performance bottleneck. Memory-intensive workloads that frequently access cross-node pages collapse. GiantVM's DaS (DSM-aware Scheduler) mitigates this by scheduling guest threads near their data. Enlil should implement:
1. **Page placement heuristics:** track which vCPU most frequently accesses each page, migrate pages to that vCPU's physical node
2. **NUMA-like topology exposure:** present cross-node memory as a remote NUMA domain to the guest, letting the guest OS make NUMA-aware placement decisions
3. **Selective spanning:** not every guest should span machines. Only guests that need more resources than one machine can provide should use DSM. The config makes this explicit.

**Config:**
```toml
[[guest]]
name = "big-compute"
vcpus = 32                      # 16 on machine A, 16 on machine B
memory = "128GB"                # 64GB on each machine
span_nodes = ["office-desktop", "closet-workstation"]
dsm_coherency = "invalidate"    # invalidate | write-update
numa_expose = true              # expose cross-node topology to guest
```

**11.6.2 GPUDirect RDMA**

NVIDIA GPUDirect RDMA allows the GPU on Machine A to directly read/write memory on Machine B's NIC (bypassing CPU). This enables:
- Compute fabric dispatches where input/output buffers go directly from guest memory on Machine A to GPU VRAM on Machine B, zero-copy
- Cross-machine GPU-to-GPU communication for multi-GPU compute workloads

**11.6.3 Sub-Millisecond Live Migration**

With RDMA, memory pages transfer at ~400 Gbps (HDR InfiniBand). A 16GB guest migrates in ~0.3 seconds. Stun time can be <1ms with post-copy migration (guest resumes on destination immediately, pages pulled on-demand via RDMA).

---

### 11.7 Tier 1 — CXL Fabric Capabilities (200–500ns RTT, 128+ GT/s)

CXL 3.0+ with multi-host switching. This is the future (hardware shipping 2026–2027 for pooling). Enlil should be ready.

**11.7.1 Hardware-Coherent Shared Memory**

CXL provides hardware cache coherency across hosts — no DSM protocol needed. The CXL switch manages coherency in silicon. Enlil simply maps remote DRAM into a guest's EPT as a CXL address range, and the hardware handles the rest. This eliminates the DSM bottleneck that plagued GiantVM.

**Performance:** CXL memory access at 200–500ns is ~2–5× slower than local DRAM (~100ns) but ~1000× faster than RDMA DSM page faults (~2μs + protocol overhead). Memory-intensive workloads that collapsed on GiantVM will run at "remote NUMA" performance on CXL — degraded but functional.

**11.7.2 Transparent Multi-Machine Guests**

On CXL, a guest spanning two machines is nearly indistinguishable from a guest on one large machine with remote NUMA nodes. The guest OS's NUMA balancer handles page placement automatically. No special DSM protocol, no page fault interception, no coherency messages in software.

Enlil exposes the CXL topology to the guest via ACPI SRAT (System Resource Affinity Table) — Machine A's RAM as NUMA domain 0, Machine B's RAM as NUMA domain 1. The guest's scheduler and memory allocator do the right thing natively.

**11.7.3 GPU Memory Pooling**

CXL enables GPU VRAM on Machine A to be accessed as a memory pool by Machine B's GPU. This is the enabling technology for "virtual multi-GPU" — a guest sees one logical GPU but its VRAM is physically distributed across CXL-connected machines. The compute fabric's SPIR-V dispatcher can allocate buffers in any GPU's VRAM across the CXL fabric.

---

### 11.8 Mesh Compute Fabric (Extension of Phase 9)

The compute fabric becomes dramatically more powerful when it can route work across the mesh:

**Multi-machine work routing:**
```
Guest submits SPIR-V kernel dispatch
  → Enlil work router evaluates:
    Local GPU: RTX 3070 (busy, queue depth 5)
    LAN peer GPU: RTX 4090 (idle, 0.3ms RTT)
    WAN peer GPU: A100 (idle, 80ms RTT, ZK-verified)
  → Decision: route to LAN RTX 4090 (best latency × performance)
  → Execute remotely, return results
  → Optional: verify with ZK proof
```

**Mesh-wide GPU inventory:** Each node advertises its GPU capabilities (vendor, model, VRAM, compute units, current load) via the gossip protocol. The work router maintains a mesh-wide view of all available GPUs.

**Split dispatches:** Large dispatches (e.g., training a model) can be split across multiple GPUs on multiple machines. Each GPU computes a partition, results are reduced. This is conceptually similar to distributed training (Horovod/PyTorch DDP) but happens transparently at the hypervisor level — the guest's application thinks it has one GPU.

**ZK verification for untrusted remote compute:** When routing work to a WAN peer (especially one you don't physically control), the ZK proof from Phase 9.12 provides cryptographic assurance that the remote GPU produced the correct result. This enables trustless GPU sharing: lend your idle GPU to a friend, and they can verify your results are correct without trusting you.

---

### 11.9 Work Shares — Splitting Computation into Distributable Packets

**The fundamental question:** Can we take a single GPU compute dispatch and split it across multiple GPUs on multiple machines? The short answer: yes, for most workloads, but the efficiency depends entirely on the ratio of computation to data movement. The research is clear that data movement — not computation splitting — is the hard problem.

**11.9.1 Why GPU Workloads Are Naturally Splittable**

GPU compute is already organized for splitting. A Vulkan/SPIR-V compute dispatch consists of workgroups, and workgroups are explicitly designed to be independent — the GPU specification says workgroups "execute independently" and "one workgroup can't block another workgroup." This is the key insight: a dispatch of 1024 workgroups is already 1024 independent units of work. If Machine A dispatches 512 workgroups locally and sends 512 to Machine B, correctness is guaranteed by the GPU programming model itself.

At the SPIR-V level, a `vkCmdDispatch(groupCountX, groupCountY, groupCountZ)` call defines a 3D grid of workgroups. Each workgroup has:
- A `WorkgroupID` (its position in the grid)
- A local invocation count (threads within the workgroup, sharing workgroup-local memory)
- Access to global storage buffers (SSBOs) — but no guarantee of ordering with other workgroups

This means Enlil can split a dispatch at the workgroup boundary with zero semantic change: each workgroup's `WorkgroupID` is preserved, it accesses the same buffer offsets, and produces the same outputs. The only requirement is that all workgroups can access the input/output buffers.

**11.9.2 The Five Splitting Strategies**

Based on the current research (ASPLOS 2025 Helix, SOSP 2023 Sia, NVIDIA CUDA DTX architecture, FSDP/DDP for ML, and heterogeneous scheduling surveys), Enlil should implement five work-splitting strategies, selected automatically based on kernel analysis:

**Strategy 1: Workgroup-Level Partitioning (Embarrassingly Parallel)**
- **When:** Kernel reads input[GlobalInvocationID], writes output[GlobalInvocationID], no cross-workgroup dependencies
- **How:** Split the workgroup grid along the largest dimension. Machine A gets workgroups 0–511, Machine B gets 512–1023. Each machine receives the full input buffer (or just its slice if the access pattern is contiguous).
- **Data movement:** Input buffer slice (send) + output buffer slice (receive)
- **Efficiency:** Near-linear scaling if `compute_time >> data_transfer_time`
- **Examples:** Image processing, per-element transforms, Monte Carlo sampling, particle simulations without neighbor interaction

**Strategy 2: Data-Parallel Sharding (FSDP-style)**
- **When:** Kernel operates on a large data array with uniform access patterns per element
- **How:** Shard the input data across machines. Each machine processes its shard. If a reduction is needed (sum, max, average), use an AllReduce collective after local computation.
- **Data movement:** Shard distribution (once) + reduction result (small, after compute)
- **Efficiency:** Excellent for large datasets. The ML ecosystem proves this works at scale — FSDP trains models across thousands of GPUs by sharding parameters, gradients, and optimizer states.
- **Examples:** ML training batches, large matrix operations, statistical aggregations

**Strategy 3: Pipeline Splitting (Sequential Stages)**
- **When:** Kernel is actually a chain of dispatches where output of dispatch N feeds dispatch N+1
- **How:** Each stage runs on a different machine. Stage 1 on Machine A, Stage 2 on Machine B. Intermediate results stream between machines. If stages have different compute costs, the pipeline naturally load-balances to the slower stage (with buffering).
- **Data movement:** Intermediate results between stages (continuous streaming)
- **Efficiency:** Good when stages have similar runtimes and intermediate data is small relative to compute. This is how pipeline parallelism works in LLM training (Helix, ASPLOS 2025).
- **Examples:** Multi-pass rendering, iterative solvers where each pass is a dispatch, signal processing chains

**Strategy 4: Spatial Decomposition (Domain Splitting with Halo Exchange)**
- **When:** Kernel accesses neighbors (stencil patterns — physics simulations, convolutions, fluid dynamics)
- **How:** Split the domain into tiles. Each machine computes its tile plus a "halo" region (ghost cells) that overlaps with adjacent tiles. After each timestep/iteration, exchange updated halo regions between machines.
- **Data movement:** Halo regions after each iteration (proportional to surface area of the tile boundary, not volume)
- **Efficiency:** Scales well when tile volume >> halo surface area (i.e., large tiles). This is the classic MPI domain decomposition from HPC — decades of research prove it works.
- **Examples:** CFD (computational fluid dynamics), finite element analysis, weather simulation, physics engines, neural network convolutions

**Strategy 5: Task Graph Decomposition (DAG Scheduling)**
- **When:** The workload is a directed acyclic graph (DAG) of dependent compute tasks with varying sizes
- **How:** Enlil's work router analyzes the dependency graph. Independent tasks (no edges between them) can run in parallel on different machines. Dependent tasks are scheduled respecting topological order, with data transfers inserted for cross-machine edges.
- **Data movement:** Only along DAG edges that cross machine boundaries. Minimized by graph partitioning (minimize edge cuts).
- **Efficiency:** Depends on the graph structure. Wide, shallow graphs parallelize well. Deep, narrow graphs don't. This is what NVIDIA CUDA DTX is building toward — a unified runtime that schedules task graphs across hundreds of thousands of GPUs.
- **Examples:** Complex rendering pipelines, multi-kernel ML inference, scientific workflows

**11.9.3 Automatic Kernel Analysis (SPIR-V Static Analysis)**

Enlil's work router already receives SPIR-V before JIT compilation (Phase 9). It can perform static analysis on the SPIR-V to determine which splitting strategy to use:

```
SPIR-V Kernel Analysis Pipeline:
1. Parse SPIR-V module → extract:
   - Buffer access patterns (GlobalInvocationID-indexed? Neighbor access? Random?)
   - Workgroup shared memory usage (local barriers? distributed shared memory?)
   - Atomic operations (global atomics? workgroup-only atomics?)
   - Control flow (uniform across invocations? divergent?)

2. Classify kernel:
   - No cross-workgroup dependencies + contiguous access → Strategy 1 (embarrassingly parallel)
   - No cross-workgroup dependencies + reduction at end → Strategy 2 (data-parallel shard)
   - Stencil pattern (access [id-1], [id], [id+1]) → Strategy 4 (spatial decomposition)
   - Multiple kernels with buffer dependencies → Strategy 5 (task graph)
   - Unanalyzable / global atomics / random access → DON'T SPLIT (run on single GPU)

3. Estimate split efficiency:
   compute_time = workgroup_count × estimated_cycles_per_workgroup
   transfer_time = buffer_size × (1 / network_bandwidth) + network_latency
   split_efficiency = compute_time / (compute_time + transfer_time)
   
   If split_efficiency < 0.7 → don't split (overhead too high)
   If split_efficiency > 0.9 → split aggressively
   Between → split conservatively (fewer, larger chunks)
```

The key metric is the **compute-to-communication ratio** (CCR). Research consistently shows:
- CCR > 10: splitting is highly efficient (compute dominates)
- CCR 1–10: splitting works but gains diminish
- CCR < 1: splitting is counterproductive (data movement dominates)

**11.9.4 Latency-Aware Split Scheduling**

Different network tiers have dramatically different CCR thresholds:

| Network Tier | RTT | Bandwidth | Min Profitable Compute | Example |
|-------------|-----|-----------|----------------------|---------|
| CXL | 500ns | 128 GT/s | ~10μs | Any kernel with >100 workgroups |
| RDMA | 2μs | 400 Gbps | ~100μs | Medium shader dispatches |
| LAN 10GbE | 200μs | 10 Gbps | ~10ms | Large compute batches |
| LAN 1GbE | 500μs | 1 Gbps | ~50ms | ML training iterations |
| WAN fast | 20ms | 1 Gbps | ~1 second | Large ML batches, rendering |
| WAN slow | 200ms | 100 Mbps | ~30 seconds | Offline rendering, overnight training |

The work router uses these thresholds dynamically. For each potential split, it calculates:
```
net_speedup = (local_compute_time) / (max(local_partition_time, remote_partition_time) + transfer_time)
```
If `net_speedup < 1.2` (less than 20% improvement), it doesn't split — the coordination overhead isn't worth it.

**11.9.5 Buffer Management for Distributed Dispatches**

The hardest part of distributed compute isn't splitting the work — it's managing the data. Five approaches, matched to network tier:

1. **Full replication (WAN):** Send the entire input buffer to each remote node. Simple, wasteful of bandwidth, but correct. Best for small buffers or when the access pattern is unpredictable.

2. **Range slicing (LAN/RDMA):** Analyze the SPIR-V access pattern to determine which buffer range each workgroup partition accesses. Send only that range. For contiguous access patterns (Strategy 1), this is optimal — each node gets exactly the data it needs.

3. **Halo exchange (LAN/RDMA):** For stencil kernels (Strategy 4), send each node its domain tile plus halo ghost cells. After computation, exchange only the updated halo regions. The HPC community has decades of optimized implementations (MPI_Isend/MPI_Irecv patterns).

4. **On-demand paging (RDMA):** For unpredictable access patterns, start execution and pull pages via RDMA on fault — similar to DSM (Section 11.6.1). High overhead per fault but avoids sending unneeded data.

5. **Zero-copy shared (CXL):** On CXL fabric, buffers are hardware-coherent across machines. No explicit transfer needed. Each node's GPU accesses the buffer at CXL latency. This is the ideal case — splitting is pure workgroup partitioning with zero data movement overhead.

**Content-addressable buffer caching:** For iterative workloads (ML training), input data (model weights, datasets) is largely immutable between iterations. Enlil caches buffers on remote nodes using content-addressed hashing (Blake3). On subsequent dispatches, only deltas (updated gradients, changed parameters) need to transfer. This collapses the effective bandwidth cost by 10–100× for iterative workloads.

**11.9.6 Reduction and Gather Operations**

Many split dispatches produce partial results that must be combined:

- **Reduction (sum, max, min):** Each node computes a local partial result. A tree-reduce across the mesh combines partials. For N nodes, this takes log₂(N) network hops. On LAN with 4 nodes: 2 hops × 200μs = 400μs for the reduction — negligible for any dispatch taking >10ms.

- **Gather (concatenate results):** Each node writes to its output buffer range. The coordinator node gathers all ranges into the final output buffer. For contiguous output patterns, this is a simple multi-source copy.

- **Scatter-Gather (all-to-all):** Some algorithms need results from all nodes at all nodes (e.g., FFT butterfly). This requires AllGather or AllReduce collectives. Enlil implements these using the NCCL-style ring or tree algorithms, adapted for the mesh topology.

**11.9.7 What the Research Says: Can We Actually Do This Efficiently?**

**The answer is definitively yes, with caveats.** The research is mature and the results are clear:

1. **Data-parallel splitting is a solved problem.** PyTorch DDP, FSDP, and Horovod distribute ML training across thousands of GPUs with near-linear scaling for large batch sizes. The key requirement is that the compute-to-communication ratio is high enough — which it is for most ML training (minutes of compute per communication round).

2. **Spatial decomposition has 40+ years of MPI research behind it.** HPC applications (weather forecasting, molecular dynamics, CFD) routinely split domains across thousands of nodes. The efficiency depends on the ratio of halo surface area to domain volume — larger tiles = better efficiency.

3. **Workgroup-level splitting of GPU kernels is emerging.** NVIDIA's CUDA DTX (announced GTC 2025, shipping ~2027) is building exactly this — a distributed runtime that treats hundreds of thousands of GPUs as a single machine. Their architecture has two components: a unified machine model (all GPUs look the same) and a unified runtime (work distribution, topology-aware scheduling, fault resilience). NVIDIA's Stephen Jones (CUDA Architect): "What works well at scale is not necessarily what works well on a single GPU."

4. **Heterogeneous GPU clusters are actively researched.** Helix (ASPLOS 2025, CMU) models heterogeneous LLM serving as a max-flow problem — different GPU types have different throughputs, and the scheduler finds the optimal work distribution across mixed hardware. Sia (SOSP 2023, CMU) is a heterogeneity-aware scheduler that profiles performance across GPU types and adapts allocation in real-time. Both achieve significant gains over homogeneous-only scheduling.

5. **The fundamental limitation is data movement, not compute splitting.** Every paper in the field agrees: the bottleneck is moving data between nodes, not dividing the computation. Enlil's tiered network architecture directly addresses this — CXL eliminates data movement (zero-copy shared memory), RDMA minimizes it (2μs per page), and LAN/WAN require careful analysis of compute-to-communication ratio before splitting.

6. **Not all workloads are splittable.** Kernels with global atomic operations, unpredictable random access patterns, or tight cross-workgroup synchronization (barrier across all workgroups) cannot be efficiently split. Enlil's SPIR-V static analysis must correctly identify these and keep them on a single GPU. The Vulkan/SPIR-V specification itself helps here — it explicitly prohibits cross-workgroup barriers (a Metal limitation formalized into the spec), meaning any well-formed SPIR-V kernel already has workgroup-level independence.

**11.9.8 Config**

```toml
[mesh.compute.work_shares]
enabled = true
auto_split = true                     # analyze SPIR-V and split automatically
min_split_efficiency = 0.7            # don't split if estimated efficiency < 70%
max_split_nodes = 4                   # split across at most N nodes per dispatch
buffer_cache = true                   # cache immutable buffers on remote nodes
buffer_cache_max_mb = 2048            # max cache per remote node
strategies = ["embarrassingly_parallel", "data_parallel", "spatial", "pipeline", "task_graph"]
prefer_local = true                   # prefer local GPU even if slightly slower (avoid network jitter)
zk_verify_splits = false              # ZK-verify remote split results (expensive, for sensitive workloads)

[mesh.compute.work_shares.wan]
min_dispatch_seconds = 5.0            # don't split over WAN for short dispatches
max_input_mb = 500                    # cap input buffer size for WAN transfers
compression = "zstd"                  # compress buffers for WAN transfer
```

---

### 11.10 Mesh Seats — Display, Input, and Presence Management

**The core problem:** When two or more PCs form a mesh, there's one human and multiple physical locations with monitors, keyboards, and mice. The system needs to know: where are you? Which screens should show your workspace? Which keyboard are you typing on? And what happens when you stand up from your office desk and sit down at your home desk 30 minutes later?

**11.11.1 The Seat Model**

A **seat** is a physical location where a human interacts with the mesh. It consists of:
- One or more physical displays (connected to a specific mesh node)
- One keyboard and one mouse/trackpad (connected to a specific mesh node)
- Optionally: speakers, microphone, webcam, other peripherals

At any given moment, exactly one seat is the **active seat**. The active seat is where the human is. All display output is composited on the active seat's node and shown on the active seat's monitors. All input from the active seat's keyboard/mouse is routed to the focused guest (wherever that guest physically runs in the mesh). Audio output comes from the active seat's speakers.

This is the fundamental insight: **the seat is about the human's physical location, not where the guests run.** Your Linux dev guest might be running on your workstation in the closet, but if your active seat is your office desktop, you see that guest's framebuffer on your office monitor and type into it with your office keyboard. The framebuffer streams across the mesh (LAN: uncompressed, WAN: H.264/H.265). Input events stream the other direction.

**11.11.2 Display Modes**

Four display modes, selectable per-mesh:

**Mode A: Active Seat Only (default for single-user)**
Only the active seat's displays show anything. All other seats' monitors are dark (or show a lock screen / "session active elsewhere" message). This is the simplest model and the right default for one person with a work/home setup — when you're at the office, your home monitors are off. When you're at home, your office monitors are off.

All guest framebuffers are composited by the active seat's Enlil Zones instance. Local guests on the active seat's node deliver framebuffers directly (zero-copy). Remote guests on other nodes deliver framebuffers over the mesh network (LAN: LZ4/raw, WAN: H.264/H.265). The user sees one unified Enlil Zones workspace regardless of which nodes the guests run on.

**Mode B: Mirror**
All seats show the same Enlil Zones layout simultaneously. Input is accepted from any seat's keyboard/mouse (last-input-wins for keyboard focus, each seat can have its own mouse cursor or share one). This is useful for:
- Monitoring: leave a dashboard visible on both locations
- Presentation: show the same workspace on a projector and your desk
- Pair debugging: two people at two machines see and control the same workspace

Audio follows the active seat (most recent input source) or can be mirrored to all seats.

**Mode C: Extended Canvas (LAN only)**
The monitors at all seats form one large Enlil Zones canvas — like plugging your home monitors into your office PC as additional displays. A guest zone can span from your office 4K monitor to your home ultrawide. This only works on LAN with <2ms RTT; over WAN, cursor movement between distant monitors would feel sluggish (200ms to see the cursor appear on the remote monitor after crossing the edge).

In Extended Canvas mode, each seat contributes its displays to the canvas. The Enlil Zones compositor on the "primary" seat node manages the global layout, compositing remote framebuffers for remote displays and sending the composited output to each seat's node for display.

**Mode D: Independent Stations (default for multi-user)**
Every seat is an independent workstation with its own Enlil Zones layout, its own set of assigned guests, and its own input/audio. All seats are active simultaneously. There's no "active seat" concept — everyone is always active. This is the mode for households, small offices, and studios.

Each seat has guests **assigned** to it. Guests assigned to a seat are composited on that seat's local Enlil Zones instance, displayed on that seat's monitors, and controlled by that seat's keyboard/mouse. The assignment is explicit in config — each guest declares which seat it belongs to.

```
┌─────────────────────────────────────────────────────────────┐
│                   MODE D: INDEPENDENT STATIONS               │
│                                                              │
│  SEAT "living-room" (Machine A)    SEAT "office" (Machine B)│
│  ┌─────────┬─────────┐            ┌───────────────────┐     │
│  │ Ubuntu  │ Windows │            │   Fedora Dev      │     │
│  │ Desktop │ 11      │            │                   │     │
│  │         │         │            │                   │     │
│  └─────────┴─────────┘            └───────────────────┘     │
│  Keyboard A  Mouse A              Keyboard B  Mouse B       │
│  Speakers A  Mic A                Speakers B  Mic B         │
│                                                              │
│  ┌──────────────────────────────────────────────────────┐   │
│  │              SHARED MESH RESOURCES                    │   │
│  │  • Compute fabric (both GPUs available to both seats) │   │
│  │  • Enlil Bridge (clipboard, files, URL routing)       │   │
│  │  • Shared filesystem (common /shared directory)       │   │
│  │  • Virtual network (all guests can reach each other)  │   │
│  │  • ZK attestation (both nodes mutually verified)      │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

**What's shared vs. what's per-seat in Mode D:**

Per-seat (independent):
- Display output: each seat has its own Enlil Zones layout, its own zone assignments
- Input: each seat's keyboard/mouse only controls that seat's guests
- Audio: each seat has its own audio mixer, own speakers, own mic
- Guest assignment: each guest belongs to one seat (or is "floating" — see below)
- Zone layout: each seat can have fullscreen, 50/50, custom grid — independently configured

Shared across all seats (mesh-wide):
- Compute fabric: any seat's guest can offload work to any node's GPU. The living room GPU and office GPU are both in the mesh work pool.
- Enlil Bridge: clipboard copy on the living room Windows guest appears on the office Fedora guest. Drag-and-drop works across seats via the shared filesystem. URL routing works cross-seat.
- Shared filesystem: `/shared` directory is accessible from all guests on all seats. Files written by any guest are visible to all other guests (subject to mesh sync latency).
- Virtual network: all guests across all seats are on the same virtual network (10.0.100.0/24). SMB/SSH/HTTP between guests works regardless of which seat they're on.
- ZK attestation: all nodes mutually verified. A guest on Machine A has cryptographic proof that Machine B (which has access to shared resources) is running genuine Enlil.
- Notifications: configurable per-seat — forward all guest notifications to all seats, or only to the guest's assigned seat.

**Floating guests:** A guest can be marked as `seat = "floating"` — it's not tied to any seat's displays but still participates in the mesh. Useful for headless workloads: a build server guest, a database guest, or a compute-only guest that other guests communicate with over the virtual network. Floating guests have no display output and receive no input, but they're accessible over the virtual network and can use the compute fabric.

**Guest "visiting":** In Mode D, a seat can temporarily display a remote seat's guest — like reaching over to look at someone else's screen. The office user presses a hotkey and a picture-in-picture zone appears showing the living room's Windows guest framebuffer (streamed over the mesh). Input to that PiP zone can optionally be enabled (remote control) or read-only (just watching). This is opt-in and requires permission from the guest's assigned seat (configurable: `allow_visiting = true | prompt | false`).

**Config for Mode D:**
```toml
[mesh.seats]
display_mode = "independent"    # active_only | mirror | extended_canvas | independent

[[mesh.seats.seat]]
name = "living-room"
node = "living-room-pc"
displays = ["HDMI-1"]
input_devices = ["usb-keyboard-1", "usb-mouse-1"]
audio_output = "default"
allow_visiting = "prompt"       # other seats can request to view, user is prompted

[[mesh.seats.seat]]
name = "office"
node = "office-workstation"
displays = ["DP-1", "DP-2"]
input_devices = ["usb-keyboard-2", "usb-mouse-2"]
audio_output = "default"
allow_visiting = true           # other seats can view freely

# Guest assignments for Mode D
[[guest]]
name = "ubuntu-desktop"
seat = "living-room"
zone = { layout = "left-half" }

[[guest]]
name = "windows-11"
seat = "living-room"
zone = { layout = "right-half" }

[[guest]]
name = "fedora-dev"
seat = "office"
zone = { layout = "fullscreen" }

[[guest]]
name = "build-server"
seat = "floating"               # headless — no display, no input
vcpus = 8
memory = "16GB"
```

**Mode D + seat switching (hybrid):** Mode D can coexist with seat switching for specific guests. If you mark a guest as `seat = "follow-user"`, it behaves like Mode A for that guest — it follows you between seats while all other guests stay at their assigned seats. This lets you have a "personal" guest (your email/browser VM) that travels with you, while station-local guests (the living room media center, the office dev environment) stay put.

```toml
[[guest]]
name = "personal-browser"
seat = "follow-user"            # follows active seat (requires seat switching for this guest)
zone = { layout = "pip-bottom-right", size = "25%" }
```

**Mode D interaction with Enlil Bridge:**

Clipboard bridge in Mode D raises a question: if the living room user copies something, does it appear on the office clipboard too? Three options:

1. **Mesh-wide clipboard** (`clipboard_scope = "mesh"`): every copy on any guest on any seat propagates to all guests on all seats. Simple, but can be confusing if two people are copying at the same time.

2. **Per-seat clipboard** (`clipboard_scope = "seat"`): clipboard only propagates between guests on the same seat. Living room guests share a clipboard with each other; office guests share a clipboard with each other. Cross-seat clipboard requires explicit "send to other seat" action.

3. **Directed clipboard** (`clipboard_scope = "directed"`): clipboard propagates between guests that are in the same Enlil Bridge group. You can configure groups: living room Linux + office Fedora share a clipboard (they're both your dev machines), while living room Windows has its own clipboard (it's the family media machine).

```toml
[mesh.bridge.clipboard]
scope = "seat"                  # mesh | seat | directed

# For directed mode:
[[mesh.bridge.clipboard.group]]
name = "dev-machines"
guests = ["ubuntu-desktop", "fedora-dev"]

[[mesh.bridge.clipboard.group]]
name = "family"
guests = ["windows-11"]
```

**11.11.3 Guest Relocation — Moving OSs Between Machines**

Seat switching (11.9.4) moves *you* between machines — the guests stay where they are and only display/input is rerouted. Guest relocation is the opposite: the guest *actually moves* to a different physical machine, taking its vCPUs, memory, disk state, and device assignments with it. The OS continues running on the destination hardware.

This should feel as simple as dragging an app between monitors.

**Why relocate instead of just streaming?** Three reasons. First, performance: a guest running locally on a machine with a GPU gets native passthrough performance — no framebuffer encoding, no input latency, no network dependency. Streaming a remote guest works but it's always worse than running locally. Second, hardware access: if you want the guest to use a specific USB device, PCIe device, or GPU that's physically connected to a different machine, the guest must actually run on that machine. Third, resilience: a relocated guest survives the source machine being shut down, losing network, or crashing. A streamed guest doesn't.

**The three relocation speeds:**

**Instant Redirect (~100ms) — display-only, not a true move:**
The guest keeps running on its current node. Only the display/input/audio routing changes to the destination seat. The guest uses the destination machine's display but the source machine's CPU/GPU/memory. This is what seat switching already does. Good enough when network is fast (LAN) and you don't need local hardware access. The guest is still vulnerable to source node failure.

```
enlil guest redirect fedora-dev --to office
```

**Live Relocate (~5–30 seconds on LAN) — the guest actually moves while running:**
Full live migration. The guest's memory is iteratively copied to the destination node while the guest keeps running (pre-copy). After convergence, a brief stun (<500ms on 10GbE, <200ms on 25GbE), the remaining dirty pages transfer, and the guest resumes on the destination. The guest OS doesn't know anything happened — its TCP connections stay open, its processes keep running, its clocks don't skip.

```
enlil guest relocate fedora-dev --to office-workstation --mode live
```

What happens under the hood:
1. Destination node allocates memory, creates empty guest shell
2. Pre-copy phase: memory pages stream from source to destination (background, guest keeps running)
3. Iterative rounds: re-send pages dirtied since last round (converges as dirty rate decreases)
4. Stun: guest paused on source
5. Final delta: remaining dirty pages + CPU state + device state transferred
6. GPU handling:
   - VirtIO-GPU: GPU context is just memory — migrates with the guest, resumes seamlessly
   - Passthrough GPU: detach from source (FLR), re-attach on destination. Brief GPU glitch. If destination has a different GPU model, guest sees a "new device" plug event (driver reload)
   - SR-IOV: VF detached on source, new VF attached on destination. If same GPU model, transparent.
7. USB devices: devices on the source node are detached. Devices physically on the destination node can be re-routed to the arriving guest.
8. Storage:
   - Shared storage (NFS, iSCSI, Enlil shared-fs): zero disk transfer — both nodes access the same storage
   - Local storage with qcow2: background block copy continues after guest resumes (guest may hit slightly higher disk latency for uncached blocks until migration completes)
   - NVMe passthrough: cannot migrate disk — guest must be on shared storage or use the fast-suspend approach instead
9. Network: VirtIO-net MAC address preserved. Virtual switch updated. Guest IP unchanged.
10. Guest resumes on destination node. Enlil Zones on the destination seat picks up the guest's framebuffer automatically.

**Performance targets:**
| Guest RAM | 10GbE | 25GbE | RDMA (100 Gbps) |
|----------|-------|-------|-----------------|
| 4 GB | ~4s | ~2s | <0.5s |
| 16 GB | ~15s | ~6s | ~1.5s |
| 32 GB | ~30s | ~12s | ~3s |
| 64 GB | ~60s | ~25s | ~6s |

These assume moderate dirty rate (~200 MB/s). Idle guests converge faster. Heavily writing guests (database, compilation) take longer or may not converge — fall back to fast-suspend.

**Fast Suspend & Resume (~2–10 seconds on LAN) — pause, move, unpause:**
For situations where live migration is impractical (very high dirty rate, NVMe passthrough, WAN link), fast suspend & resume is the fallback:

1. Guest is suspended (all vCPUs stopped, state serialized)
2. Memory snapshot compressed with zstd and streamed to destination
3. Only dirty pages since last checkpoint are sent (if incremental checkpointing is enabled)
4. Guest resumes on destination

The guest experiences a visible pause — applications freeze, clocks skip, network connections may time out if the pause exceeds TCP keepalive. But it's dramatically faster than cold migration because the guest's RAM is compressed in flight (zstd typically achieves 2–4× compression on OS memory, so 16GB of RAM becomes ~5GB on the wire).

```
enlil guest relocate fedora-dev --to office-workstation --mode suspend
```

**Performance with compression:**
| Guest RAM | Compressed | 10GbE | 25GbE | 1 Gbps WAN |
|----------|-----------|-------|-------|------------|
| 4 GB | ~1.5 GB | ~1.5s | <1s | ~12s |
| 16 GB | ~5 GB | ~5s | ~2s | ~40s |
| 32 GB | ~10 GB | ~10s | ~4s | ~80s |

**The UX for relocation:**

The goal is that relocation feels like a first-class operation, not an infrastructure command. Five interfaces, all equivalent:

**1. Enlil Zones drag-and-drop (Mode D):**
In Independent Stations mode, the management overlay shows all seats. Grab a guest's zone title bar, drag it to another seat's display area, and drop. Enlil initiates live relocation (or fast-suspend for WAN) automatically. A progress indicator shows migration progress. The guest zone appears on the destination seat when migration completes.

**2. Management TUI:**
The TUI's guest list shows each guest, its current node, and available destination nodes. Select a guest, press `M` for move, pick a destination, choose mode (live/suspend/redirect). Progress bar shows transfer.

**3. CLI:**
```bash
# Live relocate to a specific node
enlil guest relocate windows-11 --to home-pc --mode live

# Fast suspend-resume to a WAN node
enlil guest relocate windows-11 --to home-pc --mode suspend

# Display-only redirect (no actual move)
enlil guest redirect windows-11 --to home

# Relocate to wherever I am (follows active seat)
enlil guest relocate windows-11 --to-seat active

# Relocate all guests assigned to a seat
enlil seat relocate-all office --to home-pc
```

**4. REST/gRPC API:**
For automation — scripts, phone apps, Home Assistant integration. Trigger relocation based on external events (VPN connected, time of day, GPS geofence).

**5. Automatic relocation policies:**
Guests can declare relocation rules:

```toml
[[guest]]
name = "work-windows"
auto_relocate = true
relocate_policy = "follow-seat"     # always live-relocate to active seat's node
relocate_mode = "live"              # live | suspend | redirect
relocate_timeout = 30               # seconds — if migration takes longer, fall back to suspend

[[guest]]
name = "dev-environment"
auto_relocate = true
relocate_policy = "schedule"
relocate_schedule = [
  { time = "08:00", to = "office-workstation" },
  { time = "18:00", to = "home-pc" },
]
relocate_mode = "suspend"           # WAN between office and home — use suspend

[[guest]]
name = "gaming-linux"
auto_relocate = false               # never auto-relocate — always stays on the GPU machine
```

**The "follow-seat" policy** is the key UX for the work/home scenario: when you switch seats (physically move from office to home), guests with `relocate_policy = "follow-seat"` automatically start live-migrating to your new seat's node. By the time you sit down, your dev environment is already running locally on the machine in front of you — with its full GPU, USB devices, and zero-latency display. No streaming latency, no network dependency.

The flow:
1. You leave the office. Your office seat detects idle (no input for 5 minutes).
2. You arrive home. Your home seat detects input (you move the mouse).
3. Seat switches to "home" (11.9.4).
4. Immediately: all guests display-redirect to home seat (instant, <100ms).
5. Background: guests with `follow-seat` policy begin live-migrating to home node.
6. After migration completes (5–30 seconds on LAN, longer on WAN): guests are now running locally on the home machine. Display switches from streamed to local (quality improves, latency drops to zero).
7. If the office machine is shut down after migration, nothing is affected — the guests are fully on the home machine now.

**Relocation with GPU reassignment:**
When a guest relocates from a machine with an RTX 4090 to a machine with an RTX 3060, the GPU changes. How this is handled depends on the GPU strategy:

- **VirtIO-GPU (Venus/VirGL):** The guest sees a virtual GPU. The backend switches from the source GPU to the destination GPU transparently. The guest's Vulkan/OpenGL contexts are re-established on the new hardware. Brief flicker during transition, but no driver reload.

- **Passthrough:** The source GPU is detached (FLR). The destination GPU is attached. The guest OS sees a device removal + new device arrival. GPU driver reloads. All GPU contexts (running applications) are lost — applications need to handle device-lost and re-create their contexts. This is disruptive but it's the same behavior as physically unplugging a GPU and plugging in a different one.

- **SR-IOV:** Similar to passthrough but if both machines have the same GPU model (same VF driver), the guest may not notice the swap. If different GPU models, same as passthrough — driver reload.

- **Compute fabric only (no display GPU):** Guest uses the compute fabric for GPU work. Relocating the guest doesn't change its GPU access — the compute fabric routes work to whatever GPU is available in the mesh. No disruption.

**Recommendation for smooth relocation:** Use VirtIO-GPU for guests that need to relocate frequently (dev environments, productivity VMs). Use passthrough only for guests that stay pinned to one machine (gaming, ML training with specific GPU requirements).

**11.11.4 Seat Switching — "Moving" Between Locations**

When you physically move from your office to your home, you need to switch your active seat. Four mechanisms, from manual to fully automatic:

**Manual hotkey:** Press a global hotkey (e.g., `Super+Shift+S`) at the destination seat's keyboard. The mesh routes all display/input/audio to the new seat. Instant on LAN (<100ms transition). On WAN, there's a brief recomposition delay (1–3 seconds as remote framebuffer streams are established via H.264/H.265 encoding).

**Presence detection (input activity):** If no input is received at the active seat for a configurable timeout (default: 5 minutes) AND input is detected at another seat, automatically switch. The first keypress or mouse movement at the new seat triggers the switch. This handles the "walked away from desk, sat down at home" case without any explicit action.

**Network proximity / Bluetooth beacon:** If the user carries a phone, Enlil can detect which seat the phone is near (via Bluetooth LE beacons at each seat, or WiFi proximity). When the phone moves from office to home, Enlil pre-warms the home seat's framebuffer streams so the transition is instant when the user sits down.

**Explicit API / management console:** Switch via the Enlil management TUI, CLI (`enlil seat switch home`), or REST API. For automation: a script that switches seat based on time of day, VPN connection, or other signals.

**Config:**
```toml
[mesh.seats.switching]
method = "input_activity"       # hotkey | input_activity | bluetooth | manual
hotkey = "Super+Shift+S"
idle_timeout_seconds = 300      # for input_activity mode
pre_warm = true                 # pre-establish streams to all seats for fast switch
```

**What happens during a seat switch:**
1. Active seat's displays go dark (or show lock screen)
2. New seat's Enlil Zones compositor activates
3. Framebuffer streams are established from all guest nodes to the new seat node
   - LAN: streams start in <100ms (just rerouting existing framebuffers)
   - WAN: 1–3 seconds (H.264 encoder startup, keyframe establishment)
4. Input routing switches to new seat's devices
5. Audio output switches to new seat's speakers/headphones
6. Microphone switches to new seat's mic
7. USB peripherals: devices that were routed to the old seat's guests remain on their guests (the USB routing is per-guest, not per-seat). However, the seat's local USB devices (keyboard, mouse) are automatically redirected.

**Guest perspective:** Guests don't know a seat switch happened. Their vCPUs keep running. Their framebuffers keep updating. The only change is which physical display receives their framebuffer and which physical input devices send them events. This is exactly how a KVM switch works — but over the network, across the mesh, with compositing.

**11.11.5 Per-Guest Seat Affinity**

Sometimes you want specific guests to only appear at specific seats. Your work Windows VM should only be visible at the office. Your gaming Linux VM should only be visible at home.

```toml
[[guest]]
name = "work-windows"
seat_affinity = ["office"]       # only visible at office seat
mesh_policy = "lan-ok"

[[guest]]
name = "gaming-linux"
seat_affinity = ["home"]         # only visible at home seat

[[guest]]
name = "dev-environment"
seat_affinity = ["any"]          # visible at any seat (default)
```

When you switch seats, guests with affinity to the old seat disappear from the Zones layout, and guests with affinity to the new seat appear. Guests with "any" affinity follow you between seats.

**Seat-locked guests** (affinity to a specific seat) can optionally continue displaying on their seat's monitors even when that seat is not active — useful for a dashboard or media player that should keep showing even when you're not sitting there.

**11.11.6 Input Routing Across the Mesh**

When you're at the home seat and the focused guest runs on the office node, input events flow:

```
Home keyboard → Home Enlil (input capture)
  → mesh network (WireGuard encrypted, <1ms on LAN, ~50ms on WAN)
    → Office Enlil (receives input event)
      → VirtIO input device on the focused guest
        → Guest OS processes keystroke
```

For keyboard input, even 50ms WAN latency is acceptable — humans don't notice <100ms input latency for typing. For mouse movement, WAN latency is noticeable but functional (similar to remote desktop). For gaming, only LAN (<2ms) or CXL/RDMA is acceptable.

Input routing respects Enlil Zones focus rules: mouse position determines which zone (and thus which guest) receives mouse events. Keyboard goes to the zone that has keyboard focus (click-to-focus, same as Phase 3.6). The only difference in mesh mode is that the input events may need to cross the network to reach the guest's node.

**11.11.7 Audio Follows Seat**

Audio output from all guests is mixed by the active seat's node (Phase 8.2 audio subsystem). For guests running on remote nodes, their VirtIO-sound PCM streams are forwarded over the mesh to the active seat's audio mixer. On LAN, this adds <1ms latency (PCM samples are tiny). On WAN, audio is compressed (Opus codec, ~20ms frame size) and streamed — similar to VoIP quality, which is fine for system sounds and video playback but not for music production or real-time audio synthesis.

Microphone follows the active seat in reverse: audio from the seat's mic is streamed to whichever guest has mic focus (per Enlil Zones audio focus rules from Phase 8.2).

**11.10.8 Mesh Display Streaming**

Framebuffers from remote guests need to reach the active seat's compositor. Two modes based on network tier:

**LAN streaming (Tier 3, <1ms RTT):**
- Framebuffer capture: same as local Enlil Zones (IVSHMEM for passthrough, VirtIO-GPU render target for VirtIO, compositor-owns-GPU for mediated)
- Encoding: none (raw pixels) or lightweight (LZ4 frame differencing)
- Transport: UDP multicast or direct TCP, unencrypted on trusted LAN (or WireGuard if configured)
- Bandwidth: 4K@60 raw = ~12 Gbps (needs 25GbE), 1080p@60 LZ4-diff = ~500 Mbps (fine on 1GbE)
- Latency: <3ms capture-to-display
- This is comparable to Looking Glass performance — near-zero latency, no compression artifacts

**WAN streaming (Tier 4, 20–300ms RTT):**
- Encoding: H.264/H.265 hardware encode (NVENC, AMD VCE, Intel QSV) on the guest's node
- Transport: RTP/RTSP over WireGuard tunnel
- Bandwidth: 4K@60 H.265 = ~15–30 Mbps, 1080p@60 H.264 = ~5–10 Mbps
- Latency: ~20–50ms encode-transmit-decode on <50ms RTT links
- Quality: configurable bitrate/quality tradeoff. High bitrate for text-heavy work, lower for video content.
- This is functionally identical to Parsec/Moonlight/Sunshine — but integrated into Enlil Zones as a zone source rather than a separate application

**Adaptive quality:** If network conditions degrade (packet loss, bandwidth drop), the encoder automatically reduces bitrate and resolution to maintain fluidity. If conditions improve, quality ramps back up. The user sees brief quality drops rather than freezes.

---

### 11.11 Mesh Fault Tolerance — Continuous State Mirroring

**The goal:** If a physical machine dies (power failure, hardware fault, kernel panic), the guests that were running on it continue on another mesh node with near-zero downtime and minimal data loss — like nothing happened. This is the RAID10 equivalent for virtual machines: state is continuously mirrored across machines so any single machine can die without losing a running OS.

**11.11.1 Three Levels of Protection**

Not every guest needs the same level of protection. More protection = more overhead. Enlil offers three levels, configured per-guest:

**Level 0: No Replication (default)**
Guest runs on one node only. If that node dies, the guest is lost (must be restarted from last snapshot, if any). Zero overhead. Appropriate for: ephemeral VMs, build workers, anything easily recreated.

**Level 1: Periodic Checkpoint Replication**
Enlil takes full-state checkpoints of the guest at a configurable interval (default: every 30 seconds) and streams them to a backup node. If the primary dies, the backup resumes from the last checkpoint. Data loss = at most one checkpoint interval. Failover time = 2–5 seconds (time to activate the backup and re-establish networking).

This is the right tradeoff for most desktop guests. A 30-second checkpoint interval means at worst you lose 30 seconds of work — roughly equivalent to "I forgot to save my document." The overhead is low because checkpoints are infrequent and only dirty pages are sent (not the full memory image every time).

**How it works under the hood:**
1. Enlil marks all guest EPT entries as read-only (write-protected)
2. When the guest writes a page, the EPT violation traps to Enlil, which marks the page as dirty and re-enables write access (copy-on-write tracking, same as Phase 8.12 snapshots)
3. At each checkpoint interval:
   - Guest is briefly paused (~1–5ms stun)
   - Dirty page bitmap is captured, CPU state serialized, device state captured
   - Guest resumes immediately
   - Dirty pages + state are compressed (zstd) and streamed to backup node in the background
   - Backup node applies the delta to its replica image
4. Network output from the guest is NOT buffered (unlike Remus) — this means the guest has full-speed networking, but if the primary dies mid-checkpoint, the backup state may be slightly behind what external clients have seen. For desktop use, this is acceptable (a web browser might reload a page; an SSH session might need to reconnect).
5. On primary failure: backup node detects failure via heartbeat timeout (default: 3 seconds), activates the replica guest, re-establishes network (gratuitous ARP for MAC takeover), and the guest resumes from the last committed checkpoint.

**Performance overhead:**
- Memory tracking: ~2–5% CPU overhead for EPT write-protection tracking (same technique as live migration pre-copy)
- Network bandwidth: proportional to dirty rate. A typical desktop guest dirties ~50–200 MB/s → 50–200 MB per 30-second checkpoint → ~15–50 MB compressed → trivial on 1GbE, invisible on 10GbE
- Stun time per checkpoint: 1–5ms (imperceptible to the user)
- Total overhead: 3–8% for typical desktop workloads. 10–15% for memory-intensive workloads (compilation, databases).

**Level 2: Continuous Replication (Remus-style)**
High-frequency checkpointing at 25–100ms intervals (10–40 checkpoints per second). Network output is buffered until each checkpoint is committed on the backup. If the primary dies, the backup has a consistent state from ≤25ms ago — virtually zero data loss. External clients see no inconsistency because all network output was held until the corresponding state was safely replicated.

This is the vSphere FT equivalent. Near-zero data loss, near-zero failover time (sub-second). But the overhead is significant:

**How it differs from Level 1:**
- Checkpoint interval is 25–100ms instead of 30 seconds → much more frequent EPT dirty tracking and delta transfer
- Network output buffering adds latency: every network packet the guest sends is held for up to one checkpoint interval before being released to the network. At 25ms intervals, this adds ~25ms to all network latency (noticeable for gaming, acceptable for everything else).
- Memory bandwidth: a guest dirtying 200 MB/s generates ~5–20 MB of dirty pages per 100ms checkpoint → 50–200 MB/s continuous replication stream. This requires 10GbE minimum.

**Performance overhead:**
- 10–30% CPU overhead (frequent EPT tracking + compression + network buffering)
- 25–100ms added network latency (output buffering)
- Requires 10GbE+ LAN between primary and backup (not practical over WAN)
- Appropriate for: mission-critical guests where any data loss is unacceptable (databases, financial applications, long-running simulations)

**Research basis:** Remus (USENIX NSDI 2008, Xen-based) proved this approach with 25ms intervals at 40 checkpoints/second. RemusDB showed 32% overhead for database workloads. Adaptive Remus dynamically adjusts checkpoint frequency based on workload characteristics — Enlil should implement this.

**11.11.2 What Gets Replicated**

A complete guest replica requires:

| Component | How Replicated | Size |
|-----------|---------------|------|
| Memory | Incremental dirty page deltas (EPT write-tracking) | Dirty rate × interval |
| CPU state | Full vCPU register dump at each checkpoint | ~4KB per vCPU |
| Device state | VirtIO device state serialization | ~64KB per device |
| Disk state | Two options (see below) | Varies |
| GPU state | Depends on GPU strategy (see below) | Varies |

**Disk replication options:**

1. **Shared storage (recommended):** Primary and backup access the same NFS/iSCSI/Enlil shared-fs storage. Disk writes go to shared storage visible to both nodes. Zero disk replication overhead. If primary dies, backup already has the disk. This is how vSphere FT works.

2. **DRBD-style block replication:** All disk writes are synchronously replicated to the backup node's local storage. Every write goes to both disks before the write is acknowledged to the guest. Adds write latency (~0.2ms on LAN) but provides complete independence from shared storage. Uses the same technique as Linux DRBD (Distributed Replicated Block Device). Good for: setups without shared storage (two desktop PCs with local SSDs).

3. **qcow2 delta shipping:** Primary and backup share a base qcow2 image (synced once). Only dirty blocks (qcow2 COW deltas) are continuously shipped to the backup. Lower bandwidth than full block replication but more complex. Suitable for WAN replication where bandwidth is limited.

**GPU state replication:**

This is the hard problem. GPU state is large (VRAM can be 8–24 GB) and changes rapidly.

- **VirtIO-GPU:** All GPU state is in guest memory (which is already replicated). The host-side virglrenderer state is reconstructable from the guest's Vulkan/OpenGL command stream. On failover, VirtIO-GPU contexts are re-established on the backup's GPU. Brief visual glitch, but the guest doesn't crash.

- **Passthrough GPU:** GPU VRAM is not part of guest memory and is not captured by EPT dirty tracking. Full VRAM replication would require reading all of VRAM at each checkpoint — too slow for high-frequency replication. Options: (a) don't replicate GPU state — on failover, the guest sees a GPU hot-remove/hot-add event, applications must handle device-lost. (b) Use GPU-specific snapshot APIs (NVIDIA's checkpoint/restore, POS from Huang et al. 2024) if available. (c) Accept that passthrough guests have weaker FT guarantees than VirtIO-GPU guests.

- **Compute fabric:** Compute work in flight at the time of failure is lost. The guest's application retries the dispatch. Since the compute fabric already handles transient failures (timeouts, retries), this is naturally resilient.

**Recommendation:** For guests that need FT, use VirtIO-GPU. Passthrough GPU is fundamentally at odds with continuous replication because VRAM state is opaque to the hypervisor.

**11.11.3 Failover Process**

When the primary node fails:

```
T+0ms:     Primary node stops sending heartbeats
T+3000ms:  Backup detects failure (heartbeat timeout, configurable)
T+3000ms:  Backup activates replica guest:
           - Loads last committed checkpoint state
           - Configures vCPUs, EPT, VirtIO devices
           - Sends gratuitous ARP for guest's MAC address (network takeover)
T+3200ms:  Guest resumes execution on backup node
           - Level 1: guest state is ≤30 seconds old, some work lost
           - Level 2: guest state is ≤25ms old, virtually nothing lost
T+3500ms:  Guest is fully running. TCP connections from external clients:
           - Level 1: may have timed out (retransmit/reconnect)
           - Level 2: preserved (output was buffered, sequence numbers are consistent)
T+5000ms:  Enlil attempts to establish a new backup replica on another mesh node
           (if available) to restore redundancy
```

Total failover time: ~3–5 seconds (dominated by heartbeat timeout). With aggressive heartbeat (500ms timeout), failover can be <1 second — but this risks false positives on busy networks.

**11.11.4 ZK-Verified Checkpoints (Enlil-Unique)**

In a mesh with untrusted nodes, a compromised primary could send poisoned checkpoints to the backup — injecting malware into what the backup thinks is a legitimate replica. Standard FT systems (vSphere, Remus) have no defense against this because they assume all nodes are trusted.

Enlil can use ZK proofs to verify checkpoint integrity:
- Every Nth checkpoint (configurable, default N=100, i.e., every ~50 seconds at Level 1, every ~2.5 seconds at Level 2) includes a ZK proof that:
  1. The checkpoint was produced by a genuine Enlil instance (binary attestation)
  2. The checkpoint's EPT state matches the guest's declared memory layout
  3. No unauthorized modifications were made to guest memory between checkpoints
- The backup verifies the proof before committing the checkpoint
- If verification fails, the backup rejects the checkpoint and alerts the mesh

This is NOT done on every checkpoint (too expensive — ZK proof generation takes seconds). It's a sampling-based integrity check. Between verified checkpoints, the backup trusts the primary's deltas (as in traditional FT). A compromised primary could inject malicious state for at most N checkpoint intervals before being caught.

**11.11.5 Multi-Node Redundancy**

Like RAID levels, you can replicate to more than one backup:

- **1 backup (RAID1 equivalent):** Survives one node failure. Default.
- **2 backups (RAID1 with 3 copies):** Survives two simultaneous node failures. Requires 3 mesh nodes. Doubles replication bandwidth.
- **N/2 + 1 quorum:** For large meshes (5+ nodes), use quorum-based replication. Checkpoints are committed when a majority of replicas acknowledge. Survives up to N/2 simultaneous failures.

For a typical 2–3 PC home/office mesh, single backup (2 copies total) is the right default.

**11.11.6 Config**

```toml
[[guest]]
name = "important-dev-env"
ft_level = 1                          # 0 = none, 1 = periodic checkpoint, 2 = continuous
ft_checkpoint_interval_ms = 30000     # Level 1: 30 seconds (default)
ft_backup_node = "home-pc"            # explicit backup, or "auto" to let mesh choose
ft_disk_replication = "shared"        # shared | drbd | qcow2_delta
ft_replicas = 1                       # number of backup copies

[[guest]]
name = "database-server"
ft_level = 2                          # continuous replication
ft_checkpoint_interval_ms = 50        # 50ms intervals (20 checkpoints/sec)
ft_backup_node = "auto"
ft_disk_replication = "drbd"          # synchronous block replication
ft_network_buffering = true           # buffer output until checkpoint commit
ft_zk_verify_interval = 100           # verify every 100th checkpoint with ZK proof

[[guest]]
name = "disposable-build-worker"
ft_level = 0                          # no replication — easily recreated
```

**11.11.7 FT + Guest Relocation Interaction**

When a guest with FT enabled is relocated (Section 11.9.3), the FT relationship must be updated:
1. Guest live-migrates from Node A to Node B
2. After migration completes, Node B becomes the new primary
3. A new backup is established: either Node A becomes the backup (if it's still alive), or another mesh node is selected
4. New initial checkpoint sync begins from Node B to the new backup
5. During the re-sync period (~30 seconds to transfer full dirty state), the guest runs with reduced redundancy (no backup). This window is flagged in the management UI.

If the guest uses `relocate_policy = "follow-seat"` with FT, the system must be careful not to relocate to the same node that's serving as the backup. The mesh coordinator ensures primary and backup are always on different physical machines.

---

### 11.12 Enlil Storage Pool — Distributed Mixed-Disk Storage

**The vision:** Every disk across every machine in the mesh — NVMe, SATA SSD, HDD, USB drive, doesn't matter the size, speed, or type — joins a single logical storage pool. Enlil auto-detects each disk's performance characteristics and intelligently places data based on access patterns. Guests see one fast, resilient filesystem. You never think about which physical disk holds what.

This is Unraid's "throw any disk in" philosophy, extended across multiple physical machines, with automatic performance tiering that Unraid doesn't do.

**11.12.1 Why Enlil Can Do This Better Than Existing Solutions**

Ceph, GlusterFS, and MooseFS are all distributed filesystems that pool disks across machines. But they all run as userspace software on top of a Linux kernel, which means:
- They can't bypass the OS I/O scheduler (adds latency)
- They rely on the kernel's disk driver for performance data (often inaccurate under load)
- They can't access disk firmware directly (no SMART probing at the block layer)
- They need complex deployment (Ceph notoriously requires dedicated nodes, monitors, OSDs)

Enlil runs below everything. It has direct access to every SATA/NVMe controller, every disk's SMART data, every PCIe lane. It can:
- Probe each disk's actual sequential/random read/write speed at boot (direct I/O, no OS overhead)
- Read SMART attributes (wear level, temperature, error count, power-on hours)
- Detect disk type from controller interface (NVMe vs SATA vs USB) and firmware (SSD vs HDD via rotational flag)
- Monitor real-time performance under load (queue depth, latency percentiles)
- Adjust placement decisions dynamically as disk performance changes (SSD degradation, HDD seeks under contention)

**11.12.2 Disk Classification**

At boot (or when a new disk is hot-plugged), Enlil runs a quick benchmark (~5 seconds) and reads SMART data to classify each disk:

```
┌──────────────────────────────────────────────────────────────┐
│                  ENLIL DISK CLASSIFICATION                    │
│                                                               │
│  Tier 0: NVMe SSD (local)     — 3–7 GB/s seq, <100μs lat    │
│  Tier 1: SATA SSD (local)     — 500–550 MB/s seq, <200μs    │
│  Tier 2: NVMe SSD (remote LAN)— effective 1 GB/s, +0.2ms    │
│  Tier 3: HDD 7200rpm (local)  — 150–250 MB/s seq, 4–8ms     │
│  Tier 4: HDD 5400rpm (local)  — 80–150 MB/s seq, 8–15ms     │
│  Tier 5: SATA SSD (remote LAN)— effective 500 MB/s, +0.2ms  │
│  Tier 6: USB 3.0 drive        — 100–400 MB/s, variable       │
│  Tier 7: HDD (remote LAN)     — effective 100 MB/s, +0.5ms   │
│  Tier 8: Any disk (remote WAN)— bandwidth-limited, +20ms+    │
│                                                               │
│  Classification is automatic. User can override per-disk.     │
└──────────────────────────────────────────────────────────────┘
```

Note that a remote NVMe over 10GbE LAN (Tier 2) may be faster than a local HDD (Tier 3) for random I/O but slower for large sequential reads. Enlil tracks both sequential and random performance separately and makes placement decisions accordingly.

**11.12.3 Storage Pool Architecture**

The pool combines ideas from Unraid, Ceph, and ZFS, simplified for desktop use:

**Block layer, not file layer.** Unlike Unraid (which stores whole files on individual disks) or GlusterFS (which hashes files to volumes), Enlil's pool operates at the block layer. Guests see virtual block devices (VirtIO-blk) backed by the pool. The pool stripes, mirrors, or erasure-codes blocks across disks transparently.

**Three data placement policies (per-guest, per-volume):**

**Policy 1: Performance-tiered (default for guest OS disks)**
Hot data (frequently accessed blocks) automatically migrates to the fastest available disk. Cold data sinks to slower/larger disks. This is Ceph's cache tiering concept, but at the hypervisor level with real-time performance telemetry.

How it works:
1. New writes go to the fastest available disk with space (Tier 0 NVMe first)
2. A background promoter/demoter tracks access frequency per block (4KB granularity)
3. Blocks accessed >N times in the last T seconds are promoted to a faster tier
4. Blocks not accessed for >T seconds are demoted to a slower tier
5. Guest OS boot disk and page file blocks are pinned to Tier 0/1 (always fast)

This means your Windows guest boots from NVMe speed even if most of its 100GB virtual disk is stored on a remote HDD. Only the 8GB of frequently-accessed OS blocks live on NVMe. The rest of the disk (program files you haven't launched, old downloads) sits on cheap HDD storage across the mesh.

**Policy 2: Capacity-optimized (for bulk storage, media, backups)**
Fills the largest available disks first (like Unraid's "fill-up" mode). No tiering — all blocks are treated equally. Maximizes usable space. Parity or erasure coding provides redundancy.

**Policy 3: Locality-pinned (for latency-sensitive guests)**
All blocks for a specific guest are pinned to disks on the same physical machine as the guest's vCPUs. No remote storage. Maximum performance, no network dependency. Falls back to local-only if the guest is not configured for mesh storage.

**11.12.4 Redundancy Modes**

Like RAID levels, but across machines:

| Mode | Description | Space Efficiency | Survives |
|------|------------|-----------------|----------|
| None | No redundancy. Data on one disk only. | 100% | Nothing |
| Mirror | Every block on 2 disks, preferably on different machines. | 50% | 1 disk or 1 machine failure |
| Parity (Unraid-style) | XOR parity across N data disks + 1 parity disk. Parity disk must be ≥ largest data disk. | N/(N+1) | 1 disk failure |
| Dual Parity | N data + 2 parity disks. | N/(N+2) | 2 disk failures |
| Erasure Coding | Reed-Solomon coded (e.g., 4+2 = 4 data + 2 parity shards). | configurable | Up to P shard failures |
| Cross-Machine Mirror | Each block exists on a disk on Machine A AND a disk on Machine B. | 50% | Entire machine failure |

**Recommended default:** Cross-Machine Mirror for guest OS disks (survives a whole PC dying), Parity for bulk data (space-efficient, survives 1 disk failure).

**11.12.5 Intelligent Disk Selection**

When the pool needs to write a block, Enlil's disk selector considers:

1. **Performance requirement:** Is this block on a hot path (guest OS, swap, database)? → fast disk. Cold data (backups, media archive)? → any disk with space.
2. **Redundancy requirement:** Does this block need a cross-machine copy? → place on different physical machines.
3. **Disk health:** SMART data shows pending sector reallocations or high wear? → avoid this disk for new writes, begin background migration of existing data off it. Alert the user.
4. **Space pressure:** Which disks have the most free space? Balance writes to avoid filling any single disk.
5. **Network tier:** Is this disk local or remote? What's the current network latency to the remote node? If network is congested, prefer local disks.
6. **Disk type matching:** SSDs get SSD-appropriate writes (respect TRIM, avoid write amplification). HDDs get sequential-friendly placement (batch small writes, defragment).

**11.12.6 Hot-Add, Hot-Remove, Mixed Everything**

The pool handles disk changes dynamically — no downtime, no rebuilds unless a disk actually fails:

**Adding a disk:** Plug in any disk (NVMe, SATA, USB) on any machine. Enlil detects it, benchmarks it (~5s), classifies it, and adds it to the pool. New writes immediately start using the new disk. Existing data is NOT rebalanced by default (avoid unnecessary I/O). Optional: background rebalance to spread data to the new disk.

**Removing a disk:** Tell Enlil to evacuate a disk (`enlil pool evacuate /dev/sdc`). All blocks on that disk are migrated to other disks in the background. Once evacuation completes, the disk is safe to physically remove. If the disk fails before evacuation: parity/mirror/erasure coding kicks in to reconstruct the missing blocks.

**Mixed sizes:** A 256GB NVMe, a 2TB SATA SSD, a 4TB HDD, and an 18TB HDD on three different machines all join the same pool. Total pool capacity = sum of all disks (minus redundancy overhead). No requirement for disks to match in size. The only Unraid-style restriction: in Parity mode, the parity disk(s) must be ≥ the largest data disk.

**Mixed machines:** Machine A contributes 1 NVMe + 1 HDD. Machine B contributes 2 SSDs. Machine C contributes 3 HDDs. All form one pool. Enlil knows the network latency between each machine and factors it into placement decisions. A guest on Machine A preferentially stores its hot data on Machine A's NVMe, but its cold data may live on Machine C's HDDs.

**11.12.7 Guest Virtual Disk Presentation**

Guests see standard VirtIO-blk devices. They don't know or care about the pool. A guest with a "200GB virtual disk" might have:
- 8GB of OS/boot blocks on Machine A's local NVMe (Tier 0)
- 15GB of application blocks on Machine A's local SATA SSD (Tier 1)
- 50GB of user data on Machine B's HDD (Tier 4) with a mirror on Machine C's HDD
- 127GB of cold/unused blocks on Machine C's 18TB archive HDD (Tier 4)

The guest sees a single 200GB disk with consistent NVMe-like performance for its working set (because hot blocks are on NVMe) and HDD-like performance for cold data (which the guest rarely accesses, so it doesn't notice).

**Thin provisioning:** Guest virtual disks are thin-provisioned by default. A "200GB" virtual disk starts at ~0 actual blocks and grows as the guest writes. The pool allocates blocks on-demand from the most appropriate disk.

**11.12.8 Config**

```toml
[mesh.storage_pool]
enabled = true
name = "enlil-pool"
default_redundancy = "cross_machine_mirror"   # none | mirror | parity | dual_parity | erasure | cross_machine_mirror
default_placement = "performance_tiered"       # performance_tiered | capacity_optimized | locality_pinned
tiering_promote_threshold = 5                  # accesses in last 60s to promote a block
tiering_demote_seconds = 3600                  # seconds without access to demote a block
smart_monitoring = true                        # monitor disk health, auto-evacuate failing disks
rebalance_on_add = false                       # don't rebalance when new disk added (save I/O)

# Per-disk overrides (optional — auto-detect is default)
[[mesh.storage_pool.disk]]
node = "office-desktop"
path = "/dev/nvme0n1"
role = "cache"                                 # cache (Tier 0/1 only, hot data) | data | parity | any
max_usage_percent = 90                         # leave 10% free for wear leveling

[[mesh.storage_pool.disk]]
node = "home-server"
path = "/dev/sda"
role = "parity"                                # designated parity disk (must be ≥ largest data disk)

[[mesh.storage_pool.disk]]
node = "home-server"
path = "/dev/sdb"
role = "data"                                  # general data storage

# Per-guest storage overrides
[[guest]]
name = "windows-gaming"
storage_placement = "locality_pinned"          # keep all blocks local for GPU passthrough latency
storage_redundancy = "none"                    # no redundancy (fast, non-critical — game installs can be re-downloaded)

[[guest]]
name = "family-photos"
storage_placement = "capacity_optimized"       # use cheapest storage
storage_redundancy = "cross_machine_mirror"    # mirror across machines (irreplaceable data)
```

**11.12.9 Implementation Notes**

The storage pool is implemented as a VirtIO-blk backend inside Enlil that translates guest block I/O into pool operations:

```
Guest writes block at LBA 0x1000
  → VirtIO-blk backend receives write request
  → Pool block mapper looks up LBA 0x1000 → physical location(s)
     (e.g., Machine A /dev/nvme0n1 offset 0x8000 + Machine B /dev/sdb offset 0x12000 for mirror)
  → Write to primary disk (local if possible)
  → Async replicate to mirror/parity disk(s)
  → Acknowledge to guest after primary write completes (async replication for speed)
     OR after both writes complete (sync replication for safety, configurable)
  → Background tiering daemon tracks access frequency, promotes/demotes blocks
```

For the block mapper, Enlil uses a B-tree mapping LBAs to physical (node, disk, offset) tuples, stored in a metadata region on the fastest local disk. The metadata itself is replicated across machines for resilience.

The cross-machine data path uses the same network transport as the rest of the mesh (WireGuard for WAN, direct TCP/RDMA for LAN). Block I/O over the network uses a simple protocol: `WRITE(block_id, data)`, `READ(block_id) → data`, `TRIM(block_id)`. No complex consensus protocol for the common case — single-writer semantics (each guest's virtual disk is written by only one node at a time). Consensus (Raft) is only needed for metadata updates and pool membership changes.

---

### 11.13 Mesh Security & Isolation

**Attack surface analysis:**
- Node compromise: a compromised node could lie about its state, inject malicious compute results, or snoop on in-transit data
- Mitigations: ZK attestation (node can't fake its state), WireGuard (encrypted transport), ZK compute verification (can't fake results)
- Network partition: mesh handles gracefully (guests that span partitioned nodes are paused, not corrupted)

**Guest-level mesh awareness (optional):**
- By default, guests don't know they're on a mesh. Cross-machine operations are transparent.
- Optional: guest can query mesh status via VirtIO control queue ("which node am I on?", "how many nodes in mesh?", "what GPUs are available?")
- CVM guests (SEV-SNP/TDX): cross-machine DSM requires special handling — encrypted memory pages must be decrypted before RDMA transfer, re-encrypted on arrival. This adds overhead but preserves confidentiality. ZK proofs verify that the remote node correctly handles encrypted pages.

**Per-guest mesh policy:**
```toml
[[guest]]
name = "sensitive-workstation"
mesh_policy = "local-only"      # never migrate, never span, never offload

[[guest]]
name = "dev-environment"
mesh_policy = "lan-ok"          # can migrate/span within LAN, not WAN

[[guest]]
name = "compute-worker"
mesh_policy = "any"             # can use any node in the mesh
zk_verify_remote = true         # require ZK proofs for remote compute
```

---

### 11.14 Implementation Approach

**Phase 11 builds on everything before it.** Each capability uses existing subsystems:

```
Mesh Discovery         → new (gossip protocol, ~2K lines Rust)
ZK Mesh Trust          → Phase 8.7 + 8.9 (existing ZK attestation, extended to multi-node)
WAN Compute Offload    → Phase 9 compute fabric + network serialization
Cold/Warm Migration    → Phase 8.12 snapshots + network transfer
Live Migration         → Phase 2 memory management + Phase 8.4 suspend/resume
DSM                    → Phase 2 EPT management + new coherency protocol (~5K lines)
Mesh Zones             → Phase 3.6 Enlil Zones + network framebuffer source
Mesh Bridge            → Phase 3.7 Enlil Bridge + network transport
GPU Offload            → Phase 9 compute fabric (work router already does this)
CXL Integration        → Phase 2 memory management + CXL enumeration driver
```

**Recommended implementation order — Phase 11 Sub-Phases:**

Phase 11 is too large to implement as a single block. It decomposes into six sub-phases that can be built partially in parallel after 11a:

```
Phase 11a — Mesh Foundation (M20–M22)
  1. Mesh discovery + WireGuard transport
  2. ZK attestation exchange
  3. WAN async compute offload (immediate value — "borrow a GPU")
  Dependencies: Phase 8.7 (ZK attestation), Phase 9 (compute fabric)

Phase 11b — Migration & Relocation (M23–M24, M31)
  4. Cold/warm migration
  5. LAN live migration
  6. Guest relocation UX (drag-and-drop, follow-seat policies)
  Dependencies: Phase 8.12 (checkpoint engine), Phase 11a

Phase 11c — Mesh Display & Bridge (M25–M26, M32)
  7. Cross-machine Enlil Zones (remote framebuffer streaming)
  8. Cross-machine Enlil Bridge (clipboard, files, URL routing)
  9. Seat modes (A/B/C/D), seat switching, independent stations
  Dependencies: Phase 3.6 (Enlil Zones), Phase 3.7 (Bridge), Phase 11a

Phase 11d — Storage Pool (M33–M34)
  10. Distributed block storage with cross-machine mirroring
  11. Performance tiering (NVMe → SSD → HDD auto-placement)
  12. Mixed-disk, mixed-size, mixed-machine pool management
  Dependencies: Phase 3.1 (VirtIO-blk + StorageBackend trait), Phase 11a

Phase 11e — Fault Tolerance (M29–M30)
  13. Level 1 periodic checkpoint replication
  14. Level 2 continuous replication (Remus-style)
  15. ZK-verified checkpoints
  Dependencies: Phase 8.12 (checkpoint engine), Phase 11b (migration)

Phase 11f — Advanced Fabric (M27–M28)
  16. RDMA distributed shared memory (guests spanning machines)
  17. CXL integration (hardware cache-coherent shared memory)
  Dependencies: Phase 11a, advanced hardware (RDMA NICs / CXL switches)
```

**Parallelism:** After 11a is complete, sub-phases 11b, 11c, and 11d have no mutual dependencies and can be built in parallel. 11e depends on 11b (reuses migration's checkpoint engine for replication). 11f depends only on 11a and available hardware.

---

### 11.15 Milestones

| ID | Milestone | Key Deliverable |
|----|-----------|----------------|
| M20 | Two Enlil nodes discover each other on LAN, exchange ZK attestation | Gossip protocol, WireGuard tunnel, attestation proof verification |
| M21 | SPIR-V compute dispatch routed from Machine A to Machine B's GPU over LAN | Mesh work router, network serialization, result return |
| M22 | Same as M21 but over WAN with ZK proof of correct execution | WAN compute offload, ZK verification, compression |
| M23 | Guest cold-migrated from Machine A to Machine B over LAN | Snapshot transfer, resume on destination |
| M24 | Guest live-migrated between LAN nodes with <500ms stun time | Pre-copy migration, EPT state transfer |
| M25 | Enlil Zones displays remote guest framebuffer from another LAN node | Network framebuffer source, input routing |
| M26 | Full Enlil Bridge works across LAN mesh (clipboard, drag-drop, shared-fs) | Network transport for all bridge subsystems |
| M27 | Guest spans two RDMA-connected machines with DSM | Distributed shared memory, EPT page fault interception, RDMA transport |
| M28 | CXL-connected machines provide hardware-coherent memory to spanning guest | CXL enumeration, EPT mapping to CXL address space |
| M29 | Guest with Level 1 FT survives primary node power-off, resumes on backup in <5 seconds | EPT dirty tracking, checkpoint streaming, heartbeat failover |
| M30 | Guest with Level 2 FT survives primary failure with <25ms data loss | Continuous replication, network output buffering, sub-second failover |
| M31 | Guest drag-relocated from one seat to another via Enlil Zones UI | Live migration triggered by Zone drag-and-drop, automatic seat reassignment |
| M32 | Independent Stations mode: two users simultaneously using different guests on different seats | Mode D display/input isolation, shared compute fabric, cross-seat Enlil Bridge |
| M33 | Storage pool spans two machines with cross-machine mirroring, guest boots from pooled storage | Block mapper, cross-machine write replication, VirtIO-blk pool backend |
| M34 | Automatic performance tiering promotes hot blocks to NVMe, demotes cold blocks to HDD | Access frequency tracking, background promotion/demotion, SMART health monitoring |

---

### 11.16 References

**Distributed Hypervisors:**
- **GiantVM:** https://giantvm.github.io/ — first distributed hypervisor (QEMU-KVM based, RDMA DSM, ACM TACO 2022)
- **GiantVM patent (US 10,853,119):** https://patents.justia.com/patent/10853119 — distributed QEMU architecture details
- **Aggregate VM (EuroSys 2023):** resource-borrowing hypervisor design for distributed VMs

**Fault Tolerance:**
- **Remus (USENIX NSDI 2008):** https://www.usenix.org/conference/nsdi-08/remus-high-availability-asynchronous-virtual-machine-replication — asynchronous VM replication, 40 checkpoints/sec, foundational paper
- **Adaptive Remus:** dynamic checkpoint frequency based on workload characteristics
- **VMware vSphere FT Architecture:** https://www.vmware.com/docs/vmware-vsphere6-ft-arch-perf — fast checkpointing, deterministic replay, production FT
- **RemusDB (VLDB 2011):** database-optimized VM replication, 32% overhead characterization
- **HydraVM:** storage-based checkpoint replication, eliminates dedicated backup memory reservation
- **FGBI (Fine-Grained Block Identification):** reduces FT downtime by 77% over LLM and 45% over Remus
- **POS (Parallel OS-level GPU C/R):** Huang et al. 2024, concurrent GPU checkpoint without stopping execution

**Distributed Storage:**
- **Unraid:** https://docs.unraid.net/ — mixed-disk storage pool with parity protection, cache tiering, whole-file-per-disk architecture
- **Ceph:** https://ceph.io/ — distributed object/block/file storage, CRUSH algorithm for placement, erasure coding, cache tiering (1 TiB/s demonstrated 2024)
- **MooseFS:** https://moosefs.com/ — distributed POSIX filesystem, tiered storage, erasure coding
- **SeaweedFS:** https://github.com/seaweedfs/seaweedfs — distributed storage with customizable tiering, erasure coding, S3-compatible
- **DRBD:** https://linbit.com/drbd/ — distributed replicated block device, synchronous replication for HA
- **bcachefs:** https://bcachefs.org/ — Linux filesystem with built-in tiering (SSD cache + HDD backing), checksums, erasure coding — architectural reference for single-node tiering

**CXL:**
- **CXL Consortium:** https://computeexpresslink.org/
- **CXL 4.0 Specification (Nov 2025):** 128 GT/s, bundled ports, multi-rack memory pooling
- **CXL Memory Pooling for AI (SC25 demo):** 3.8× speedup vs 200G RDMA for LLM inference
- **Pond: CXL-Based Memory Pooling (ASPLOS 2023):** https://dl.acm.org/doi/10.1145/3575693.3578835
- **Adaptive Coherence Management (HCDS 2025):** https://dl.acm.org/doi/10.1145/3723851.3723858

**Live Migration:**
- **Fast Transparent VM Migration in Edge Clouds:** https://cse.buffalo.edu/faculty/tkosar/cse710_spring19/chaufournier-sec17.pdf
- **CBase: Fast WAN VM Storage Migration:** https://doi.org/10.1016/j.jpdc.2018.10.006
- **CloudNet: Dynamic Pooling by Live WAN Migration (VEE 2011)**

**Mesh Networking:**
- **WireGuard:** https://www.wireguard.com/ — modern VPN protocol (Rust: `boringtun`)
- **SWIM protocol:** https://www.cs.cornell.edu/projects/Quicksilver/public_pdfs/SWIM.pdf — scalable failure detection and membership
- **Memberlist (HashiCorp):** https://github.com/hashicorp/memberlist — SWIM implementation (architectural reference)

---

## Key Technical Risks & Mitigations

| Risk | Severity | Mitigation |
|------|----------|------------|
| GPU mediated passthrough requires deep vendor-specific knowledge | Critical | Start with Intel (best docs), use open-source drivers as reference, fall back to time-slicing |
| Consumer GPUs lack SR-IOV | High | Tier system means we don't depend on it — it's opportunistic |
| NVIDIA proprietary driver detection of hypervisor | High | Stealth CPUID + timing mitigation; test extensively; time-sliced passthrough avoids MMIO interception |
| Bare-metal driver coverage | High | Service VM approach (Phase 6.6) dramatically reduces this |
| Windows activation / anti-piracy detection | Medium | Proper SMBIOS + ACPI + CPUID stealth; virtual TPM for consistent identity |
| Anti-cheat arms race | High | APERF/MPERF emulation + LBR save/restore + CPUID caching; maintain pafish/al-khaser/custom IET test suite; this is ongoing work |
| ACPI AML table synthesis introducing exploitable interfaces | Medium | Fuzz all AML bytecode with iasl; validate against BadAML attack vectors (ACM CCS 2025) |
| VRAM save/restore performance for time-slicing | Medium | VRAM page-table remapping instead of copying; prioritize foreground guest |
| SPIR-V → CPU JIT performance gap vs native GPU | Medium | Use LLVM (official SPIR-V backend, Dec 2024) — Intel's oneAPI proves this path works; adaptive router only routes to CPU when GPU is saturated |
| CUDA shim coverage for Windows workloads | Low (was Medium) | Microsoft adopting SPIR-V for DirectX SM7 makes CUDA shim less critical; industry converging on SPIR-V |
| Guest buffer zero-copy GPU access via IOMMU | Medium | Fall back to copy-in/copy-out if IOMMU mapping is infeasible; under CVM mode, use shared-page protocol |
| USB controller FLR instability with VFIO passthrough | Medium | Prioritize software xHCI (ACRN-style) over VFIO; VFIO as fallback only |
| CVM mode incompatible with zero-copy fabric buffers | Medium | Implement dual-path: zero-copy for normal guests, shared-page protocol for confidential guests |
| AMD iGPU passthrough instability (Vega/RDNA) | Medium | Do not rely on AMD iGPU passthrough; use dual-GPU (iGPU for host + dGPU passthrough) or Tier 3/4 sharing |
| AMD Radeon SR-IOV timeline unknown | Medium | GIM is open-source and Radeon is "on the roadmap" — architect to support it when it ships but don't block on it |
| ZK proof generation performance (100–1000x overhead) | Low | ZK is optional and infrequent (attestation at boot, sampled compute verification) — not on the hot path |
| DSM performance collapse on memory-intensive cross-node workloads | High | GiantVM proved this is real. Mitigations: NUMA-aware topology exposure, page migration heuristics, selective spanning (only span when needed). CXL eliminates this entirely. |
| CXL multi-host hardware not yet shipping for consumers | Medium | Phase 11 Tiers 2–4 work without CXL. CXL (Tier 1) is additive. RDMA (Tier 2) is available now. Ethernet (Tier 3–4) is universal. |
| Network partition during cross-machine guest operation | Medium | Fail-safe: spanning guests pause (not crash) on partition. Non-spanning guests unaffected. Configurable timeout before auto-migration back to single node. |
| WAN latency makes interactive compute offload impractical | Low | Work router enforces `dispatch_time > 50× RTT` threshold for WAN. Only batch/async workloads go over WAN. Users control policy. |
| Live migration GPU state loss during passthrough | Medium | VirtIO-GPU guests migrate cleanly. Passthrough guests experience GPU glitch (FLR + re-init). Document clearly; recommend VirtIO-GPU for guests that need migration. |
| Storage pool metadata corruption loses block location map for all guests | Critical | Triple-replicate metadata across mesh nodes. Write-ahead log for metadata journal. Regular metadata checksumming. Metadata is tiny (<1MB for millions of blocks) so replication cost is negligible. |
| Seat switching over WAN causes 1–3 second display gap (H.264 encoder startup) | Low | Pre-warm streams to all seats (already in design). Accept the gap for WAN — it's inherent to video encoding latency. Not fixable without pre-encoding. |
| Work share SPIR-V static analysis misclassifies kernel, splits unsplittable workload | Medium | Conservative default: don't split unless analysis is confident. Fallback: if remote partial result is inconsistent, re-execute entire dispatch locally. ZK verification catches incorrect remote results. |
| Distributed storage pool adds latency to every disk I/O (block mapper lookup) | Medium | Block mapper is a B-tree in RAM (~1μs lookup). Hot path: local writes to local disk go through mapper but no network hop. Network I/O only for cross-machine blocks. Benchmark and optimize the mapper before shipping. |
| Multiple users in Mode D accidentally interfere via shared clipboard or filesystem | Low | Per-seat clipboard scoping already designed (mesh / seat / directed modes). Shared filesystem uses Unix permissions — each seat's guests run as different UIDs. |
| ARM EL2 / GIC differences from x86 VMX / APIC | Medium | HAL trait defined in Phase 0 isolates architecture differences; Rust-Shyper as primary ARM reference |
| RISC-V H extension hardware availability | Low | RISC-V is long-term target; develop against QEMU initially; hardware maturing |
| Laptop GPU resume after S3 sleep (especially NVIDIA dGPU) | Medium | Test extensively per GPU generation; FLR + firmware reload sequence is GPU-specific |
| WiFi management without service VM (bare-metal mode) | High | Strongly recommend service VM for WiFi; bare-metal WiFi stack is a huge engineering effort |
| Android app compatibility (ARM-only apps on x86 VM) | Medium | BlissOS includes libhoudini/libndk ARM translation layer; Google Play may flag emulated environment |
| Snapshot restore with passthrough devices (GPU/NVMe state mismatch) | Medium | Device state drifts from snapshot — must FLR + reinit passthrough devices on restore; warn users |

---

## Development Milestones Summary

| # | Milestone | Validates |
|---|-----------|-----------|
| M0 | Single Linux guest boots to shell over serial (KVM-backed) | RustVMM integration, basic VMM loop |
| M0a | Enlil boots from USB flash drive on real hardware, launches one guest | UEFI boot chain, OVMF, non-destructive testing workflow |
| M1 | Full `std` Rust compiles for `x86_64-unknown-enlil` target, threads + async + sync working on Linux backend | Platform layer, custom target, dual-backend architecture |
| M2 | Two Linux guests run simultaneously on dedicated cores | CPU partitioning, memory isolation, multi-guest |
| M3 | Time-sliced vCPUs work when cores are overcommitted | Scheduler, context save/restore |
| M4 | Guests have disks and networking | VirtIO device layer, virtual switch |
| M4a | Enlil Zones: both guests visible on one monitor in side-by-side layout | Display compositor, framebuffer capture, zone rendering |
| M4b | Full Enlil Bridge: clipboard, drag-and-drop, shared folder, URL routing, notification forwarding between Linux and Windows guests | VirtIO bridge device, bridge agents (Linux + Windows), VirtIO-fs, virtual switch fast path |
| M5 | USB devices routed to specific guests | USB monitor, routing engine, virtual xHCI |
| M6 | Windows 11 installs and boots as a guest | ACPI/SMBIOS synthesis, vTPM, UEFI boot |
| M6a | Existing Windows installation boots under Enlil via NVMe passthrough | IOMMU passthrough, SMBIOS matching, Windows reactivation handling |
| M7 | Windows guest passes anti-VM detection (pafish + al-khaser + IET divergence) | CPUID stealth, APERF/MPERF emulation, LBR save/restore, full transparency |
| M8 | Boots from UEFI without host OS, platform-baremetal backend active | Bare-metal kernel, hardware discovery, platform layer bare-metal swap |
| M8a | Intel iGPU SR-IOV enabled, both guests have hardware-accelerated graphics | i915 SR-IOV VF creation, IOMMU VF assignment (can happen as early as Phase 2–3) |
| M9 | GPU passthrough to one guest (discrete GPU) | IOMMU GPU assignment, vBIOS, reset handling |
| M10 | GPU time-sliced between two guests | State save/restore, VRAM management |
| M11 | GPU mediated passthrough (Intel first) | Command-submission-layer interception, VRAM partitioning |
| M12 | Enlil Compute ICD installed in Linux guest, SPIR-V reaches hypervisor | VirtIO compute device, ICD packaging |
| M13 | SPIR-V kernel executes on CPU backend via LLVM JIT | LLVM SPIR-V backend, inkwell bindings, buffer mapping |
| M14 | Router dynamically selects CPU vs GPU per dispatch | Routing heuristics, adaptive learning, queue monitoring |
| M15 | Windows guest Vulkan compute app runs on fabric transparently | Windows ICD, cross-platform VirtIO driver |
| M16 | Guest runs with AMD SEV-SNP or Intel TDX memory encryption | CVM protocol (GHCB/TDCALL), encrypted EPT, attestation |
| M17 | Hypervisor live-update without guest reboot | State save, binary swap, state restore |
| M17a | Enlil runs on a laptop: single GPU (Zones), WiFi (service VM), battery, sleep/resume | Laptop hardware support, power management, WiFi bridging |
| M17b | Android guest (BlissOS) boots and runs apps in an Enlil Zone with touch emulation | Android-x86 VM, VirtIO-GPU Venus, touch input mapping |
| M17c | VM snapshot created and restored in <5 seconds | Copy-on-write memory snapshot, state serialization, qcow2 disk snapshot |
| M17d | WASM plugin loaded at runtime, handles USB routing event, hot-reloaded without guest disruption | Wasmtime AOT, host API, plugin lifecycle, sandboxing |
| M18 | ZK proof attestation: guest verifies Enlil integrity without trusting hardware vendor | RISC Zero/SP1 integration, proof generation, in-guest verifier |
| M19 | ZK cross-guest isolation proof: Guest A verifies Guest B cannot access its memory | EPT/NPT non-overlap proof, IOMMU domain exclusion proof |
| M20 | Two Enlil nodes discover each other on LAN, exchange ZK attestation proofs | Gossip protocol, WireGuard tunnel, ZK proof verification |
| M21 | SPIR-V compute dispatch routed from Machine A to Machine B's GPU over LAN | Mesh work router, serialization, remote execution, result return |
| M22 | WAN compute offload with ZK proof of correct remote execution | WAN transport, ZK verification, compression |
| M23 | Guest cold-migrated from Machine A to Machine B over LAN | Snapshot transfer, delta encoding, resume on destination |
| M24 | Guest live-migrated between LAN nodes with <500ms stun time | Pre-copy migration, EPT state transfer, network transparency |
| M25 | Enlil Zones displays remote guest framebuffer from another LAN node | Network framebuffer source, cross-machine input routing |
| M26 | Full Enlil Bridge works across LAN mesh | Network transport for clipboard, drag-drop, shared-fs, notifications |
| M27 | Guest spans two RDMA-connected machines with distributed shared memory | DSM coherency protocol, EPT page fault interception, RDMA transport |
| M28 | CXL-connected machines provide hardware-coherent shared memory to spanning guest | CXL enumeration, transparent EPT mapping to CXL address space |
| M29 | Guest with Level 1 FT survives primary node power-off, resumes on backup | EPT dirty tracking, checkpoint streaming, heartbeat failover |
| M30 | Guest with Level 2 FT survives primary failure with <25ms data loss | Continuous replication, network output buffering, sub-second failover |
| M31 | Guest drag-relocated between seats via Enlil Zones UI | Live migration triggered by Zone drag-and-drop, automatic seat reassignment |
| M32 | Independent Stations mode: two users on different seats simultaneously | Mode D display/input isolation, shared compute fabric, cross-seat Bridge |
| M33 | Storage pool spans two machines, guest boots from pooled storage | Block mapper, cross-machine write replication, VirtIO-blk pool backend |
| M34 | Automatic performance tiering: hot blocks on NVMe, cold on HDD | Access frequency tracking, background promotion/demotion, SMART monitoring |
| M35 | Enlil boots and runs two Linux guests on AArch64 hardware | ARM EL2 HAL backend, GICv3, Stage-2 page tables |
| M36 | Enlil boots and runs a Linux guest on RISC-V (QEMU) | RISC-V H extension HAL backend, Sv48 page tables |

---

## Immediate Next Steps (Weeks 1–2)

1. `cargo init --name enlil` with workspace layout including `enlil-platform`, `enlil-hal`, `enlil-core`
2. Define the `HypervisorBackend` trait in `enlil-hal` (architecture-neutral from day one)
3. Create the `x86_64-unknown-enlil.json` target spec, verify `cargo build -Z build-std` compiles `core` + `alloc` for it
4. Implement `GlobalAlloc` in `enlil-platform` backed by Linux `mmap` (the `platform-linux` backend) — this unlocks `Vec`, `String`, `Box`, `HashMap` immediately
5. Implement platform threading backed by `pthread` — this unlocks `std::thread::spawn`
6. Implement platform sync backed by `futex` — this unlocks `std::sync::Mutex`, `Arc`, `Condvar`
7. Write a test binary that uses `std::thread::spawn`, `Mutex<Vec<String>>`, and `println!()` compiled for `x86_64-unknown-enlil` running on the Linux backend — **this proves the platform layer works**
8. Add `kvm-ioctls`, `vm-memory`, `vm-superio`, `linux-loader` dependencies to `enlil-core`
9. Write the VMM main loop: create VM → create vCPU → load kernel → KVM_RUN → handle exits
10. Get a minimal Linux kernel (grab a prebuilt vmlinuz + buildroot initramfs) booting to a serial shell
11. **Build a USB live boot image** — format a FAT32 USB drive with the Enlil EFI binary + config + OVMF, boot it on real hardware, launch a guest. This is the first "holy shit it works" moment and validates the entire boot chain.
12. Celebrate — you have full `std` Rust, a working VMM, and a bootable USB image. Everything else is incremental from here.

---

## Reference Resources

**Platform Layer & Custom std:**
- **Redox OS:** https://gitlab.redox-os.org/redox-os/redox — primary reference for Rust OS with full `std`
- **Redox relibc:** https://gitlab.redox-os.org/redox-os/relibc — how to wire `std::sys` to a custom kernel
- **Redox ralloc:** https://gitlab.redox-os.org/redox-os/ralloc — pure Rust allocator, portable to bare-metal
- **Redox kernel scheduler:** https://gitlab.redox-os.org/redox-os/kernel — context switching, sync primitives in Rust
- **Rust custom targets:** https://doc.rust-lang.org/rustc/targets/custom.html — target spec JSON docs
- **`async-task`:** https://github.com/smol-rs/async-task — lightweight no-OS-dependency async task primitive
- **`smol` runtime:** https://github.com/smol-rs/smol — minimal async runtime, good architectural reference
- **Chase-Lev deque:** https://docs.rs/crossbeam-deque — lock-free work-stealing deque for scheduler

**Hypervisor & Virtualization:**

- **RustVMM Project:** https://github.com/rust-vmm — all crates
- **Cloud Hypervisor:** https://github.com/cloud-hypervisor/cloud-hypervisor — production Rust VMM built on RustVMM, excellent reference
- **Firecracker:** https://github.com/firecracker-microvm/firecracker — Amazon's microVM, simpler reference
- **Microsoft OpenVMM / OpenHCL:** https://github.com/microsoft/openvmm — Rust VMM with paravisor architecture, priority-aware async scheduling (study their task scheduler)
- **Microsoft Hyperlight:** https://github.com/hyperlight-dev/hyperlight — micro-VM isolation per function call, ultra-fast boot
- **Rust-Shyper:** https://www.sciencedirect.com/science/article/abs/pii/S1383762123001273 — Rust bare-metal hypervisor with VM migration and live-update (Computers & Security, 2024)
- **Hypervisor 101 in Rust:** https://github.com/tandasat/Hypervisor-101-in-Rust — practical one-day Rust hypervisor course for Intel/AMD, with full source
- **Intel ACRN:** https://projectacrn.github.io/ — embedded hypervisor with best-in-class USB virtualization (xHCI emulation, TRB-level interception)
- **Intel SDM Vol 3, Chapter 23–33:** VMX specification (the bible for bare-metal VMX programming)
- **AMD APM Vol 2, Chapter 15:** SVM specification
- **UEFI Specification:** https://uefi.org/specifications
- **ACPI Specification:** https://uefi.org/specifications (AML bytecode reference)
- **Intel GVT-g:** https://github.com/intel/gvt-linux — reference for GPU mediation on Intel
- **Bareflank:** https://github.com/Bareflank/hypervisor — C++ bare-metal hypervisor reference
- **uefi-rs:** https://github.com/rust-osdev/uefi-rs — Rust UEFI development

**GPU Virtualization:**
- **Intel iGPU SR-IOV DKMS:** https://github.com/strongtz/i915-sriov-dkms — enables GPU SR-IOV on consumer Intel 12th gen+ (working now)
- **AMD GIM (GPU-IOV Module):** https://github.com/amd/MxGPU-Virtualization — AMD's open-source SR-IOV GPU virtualization driver (MI300X, Radeon "in the roadmap")
- **GPU Virtualization Comparative Analysis (2025):** https://journals.ssau.ru/est/article/view/29605 — benchmarks passthrough, SR-IOV, MIG, vGPU overhead
- **Mediated Passthrough Rigidity (FGCS, June 2025):** benchmarks showing MMIO-level interception instability vs command-layer approach

**Confidential Computing:**
- **"Confidential VMs Explained" (ACM SIGMETRICS, Dec 2024):** https://dl.acm.org/doi/10.1145/3700418 — comprehensive SEV-SNP/TDX benchmarks and analysis
- **"BadAML" (ACM CCS, Nov 2025):** exploiting ACPI AML firmware interfaces to compromise CVMs — directly relevant to our ACPI synthesis
- **AMD SEV-SNP Programming Reference:** https://developer.amd.com/sev/
- **Intel TDX Module Specification:** https://www.intel.com/content/www/us/en/developer/tools/trust-domain-extensions/overview.html
- **AMD SVM Architecture Manual:** http://www.0x04.net/doc/amd/33047.pdf — LBR Virtualization, VMCB layout, nested paging

**Zero-Knowledge Proofs / Verifiable Computing:**
- **RISC Zero:** https://github.com/risc0/risc0 — zkVM for general-purpose verifiable computation (Rust-native)
- **SP1 (Succinct):** https://github.com/succinctlabs/sp1 — fast zkVM, RISC-V based prover
- **NoirVisor:** https://github.com/Zero-Tang/NoirVisor — x86 hypervisor with LBR virtualization reference (Intel + AMD)

**Anti-Detection / Stealth:**
- **Anti-Cheat VM Detection (secret.club):** https://secret.club/2020/04/13/how-anti-cheats-detect-system-emulation.html — IET divergence, LBR analysis, APERF detection
- **pafish (Paranoid Fish):** https://github.com/a0rtega/pafish — comprehensive anti-VM detection tool
- **al-khaser:** https://github.com/LordNoteworthy/al-khaser — advanced anti-VM/anti-debug detection suite
- **Kernel-Level Anti-Cheat Systematic Review (Dec 2025):** https://www.emergentmind.com/topics/kernel-level-anti-cheat-systems
- **AMD SVM LBR Virtualization:** SVM_FEATURE_LBRV (feature bit 1) — native LBR save/restore in VMCB control area

**Inter-Guest Communication:**
- **VirtIO-fs Specification:** https://virtio-fs.gitlab.io/ — FUSE-over-VirtIO shared filesystem
- **WinFsp (Windows File System Proxy):** https://winfsp.dev/ — user-space filesystem driver for Windows (enables VirtIO-fs on Windows guests)
- **Looking Glass:** https://looking-glass.io/ — framebuffer sharing via IVSHMEM (architectural reference for Enlil Zones)
- **VirtIO-GPU Venus (Mesa):** https://docs.mesa3d.org/drivers/venus.html — Vulkan virtualization protocol
- **virglrenderer:** https://gitlab.freedesktop.org/virgl/virglrenderer — host-side GPU command execution for VirtIO-GPU
- **SPICE Clipboard:** https://www.spice-space.org/ — reference for cross-VM clipboard sharing protocol

**Compute Fabric:**
- **SPIRV-Cross:** https://github.com/KhronosGroup/SPIRV-Cross — SPIR-V to C/C++/GLSL/HLSL/MSL transpiler
- **SPIRV-Tools:** https://github.com/KhronosGroup/SPIRV-Tools — SPIR-V optimizer, validator, linker
- **LLVM SPIR-V Backend (official, Dec 2024):** https://www.phoronix.com/news/Intel-LLVM-SPIR-V-Official-Plan — now the recommended SPIR-V → x86/ARM/RISC-V path
- **inkwell (LLVM Rust bindings):** https://github.com/TheDan64/inkwell — safe Rust wrapper for LLVM C API
- **Intel oneAPI SPIR-V JIT:** https://www.intel.com/content/www/us/en/docs/oneapi/optimization-guide-gpu/2024-1/jitting.html — production SPIR-V → x86 compilation reference
- **SoftCompute:** https://github.com/lighttransport/softcompute — CPU JIT execution of SPIR-V compute shaders (research prototype)
- **Microsoft DirectX SPIR-V Adoption (Sep 2024):** https://www.khronos.org/spirv/ — DirectX 12 SM7 will accept SPIR-V
- **Cranelift:** https://github.com/bytecodealliance/wasmtime/tree/main/cranelift — Rust JIT compiler backend (fallback option)
- **Vulkan ICD Loader:** https://github.com/KhronosGroup/Vulkan-Loader — reference for ICD registration
- **Bend / HVM2:** https://github.com/HigherOrderCO/Bend — research reference for hardware-agnostic compute via Interaction Combinators
- **Mesa / Lavapipe:** https://docs.mesa3d.org/drivers/llvmpipe.html — CPU-based Vulkan/OpenGL, reference for SPIR-V → CPU compilation

**Laptop & Mobile Support:**
- **NVIDIA Optimus / Switchable Graphics:** understand muxless vs muxed laptop GPU architectures for passthrough planning
- **ACPI Battery Interface (_BST/_BIF):** ACPI spec Chapter 10 — virtual battery device implementation
- **scrcpy:** https://github.com/Genymobile/scrcpy — Android phone screen mirroring via ADB (reference for phone mirroring feature)

**Android Guest Support:**
- **BlissOS:** https://blissos.org/ — Android-x86 based OS for VMs and bare-metal (recommended Android guest image)
- **Waydroid:** https://waydro.id/ — container-based Android on Linux (reference for lightweight Android integration)
- **Google crosvm + ARCVM:** https://chromium.googlesource.com/chromiumos/platform/crosvm — ChromeOS Android VM architecture (reference for deep compositor integration)
- **libhoudini / libndk:** ARM → x86 translation layers for Android (enables ARM-only apps on x86 VMs)

**Snapshots:**
- **qcow2 internal snapshots:** QEMU documentation — copy-on-write disk snapshot format
- **KVM state save/restore:** Linux KVM migration protocol — reference for VM state serialization

**Plugin System:**
- **Wasmtime:** https://github.com/bytecodealliance/wasmtime — Rust-native WASM runtime with AOT compilation (1.3–1.5x native perf)
- **WASI:** https://wasi.dev/ — WebAssembly System Interface specification for sandboxed system access
- **Microsoft Hyperlight:** https://github.com/hyperlight-dev/hyperlight — uses WASM for per-function hypervisor-level isolation (validates WASM-in-hypervisor approach)
- **Rhai:** https://github.com/rhaiscript/rhai — embedded scripting engine for Rust (sandboxed, no external deps)
- **mlua:** https://github.com/khvzak/mlua — Lua bindings for Rust (alternative scripting runtime)
- **"Not So Fast" (USENIX ATC 2019):** https://www.usenix.org/conference/atc19/presentation/jangda — WASM vs native performance analysis (45–55% overhead across SPEC CPU)
- **eBPF:** architectural inspiration — small verified programs running inside the kernel with controlled access to kernel state (similar pattern to WASM plugins in Enlil)

**Architecture Portability (ARM / RISC-V):**
- **Diosix:** https://diosix.org/ — open-source bare-metal Rust hypervisor for RISC-V (primary RISC-V reference)
- **Rust-Shyper (AArch64):** https://github.com/openeuler-mirror/rust_shyper — Rust type-1 hypervisor for ARM, tested on Jetson TX2
- **ARM Architecture Reference Manual:** EL2 virtualization, Stage-2 translation, GICv3/v4
- **RISC-V H Extension Specification:** https://github.com/riscv/riscv-isa-manual — hypervisor extension for RISC-V
- **Linux KVM ARM64:** `arch/arm64/kvm/` in Linux kernel — reference for ARM EL2 hypervisor implementation
- **Linux KVM RISC-V:** `arch/riscv/kvm/` in Linux kernel — reference for RISC-V H extension implementation

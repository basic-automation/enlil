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
operating systems*. See the [**Core Model**](ROADMAP.md#core-model--logical-machines-over-a-physical-pool)
in the roadmap for the full picture.

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
sits below everything and never replaces GRUB or the Windows Boot Manager. The full
power-on-to-desktops boot flow is documented in the
[roadmap](ROADMAP.md#architecture-overview).

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
| USB peripheral routing (virtual xHCI, TRB-level) | 🚧 Substantially implemented |
| Windows transparency (ACPI/SMBIOS synthesis, CPUID/timing/LBR stealth, vTPM) | 🚧 Building blocks implemented |
| Bare-metal UEFI boot (Phase 6) | ⏳ Stub (`enlil-boot`) — planned |
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
| **`enlil-platform`** | The foundation. std-equivalent APIs (memory/alloc, threading + scheduler, sync, async runtime, time, I/O) for both hosted (`platform-linux`, delegates to `std`) and bare-metal (`platform-baremetal`, raw hardware) targets. |
| **`enlil-std`** | A `std`-shaped facade (`collections`, `future`, `io`, `sync`, `thread`, `time`) over `enlil-platform`, with integration tests — proves the platform layer can carry ordinary Rust on the custom target. |
| **`enlil-hal`** | The architecture-neutral `HypervisorBackend` trait — the only place ISA-specific virtualization details are meant to appear. |
| **`enlil-core`** | The hypervisor core: VM lifecycle, vCPU management, memory partitioning, EPT, CPUID/SMBIOS/ACPI handling, serial console, timing stealth, vTPM. Carries the RustVMM stack (`kvm-ioctls`, `vm-memory`, `vm-superio`, `linux-loader`, …) gated to Linux for the KVM-backed dev path. |
| **`enlil-devices`** | The virtual device library: VirtIO net/block, full ACPI table synthesis, SMBIOS, interrupt controllers (LAPIC/IOAPIC/MSI), timers (PIT/HPET/TSC/paravirt), PS/2, HDA audio, qcow2/raw storage, a virtual **xHCI** USB stack with routing, an inter-guest **bridge** (clipboard, drag-and-drop, shared FS, notifications), a display compositor, and anti-detection **stealth** modules (CPUID, timing, LBR, PMC). |
| **`enlil-config`** | TOML guest definitions and validation (no overlapping CPU sets, memory, or device assignments). |
| **`enlil-mgmt`** | The management console — `clap` CLI + `ratatui`/`crossterm` TUI for live control (list/start/stop/attach/status). |
| **`enlil-setup`** | First-run setup wizard: detect hardware, define guests, assign resources, write `config.toml`. |
| **`enlil-boot`** | UEFI boot payload for the bare-metal target (Phase 6+). Currently a stub. |

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
├── ROADMAP.md                  # Development roadmap — all 11 phases as a checklist, milestones, risks, research refs
├── examples/                   # Sample guest configurations
├── enlil-platform/             # Platform abstraction (linux + baremetal backends)
├── enlil-std/                  # std-shaped facade over the platform layer
├── enlil-hal/                  # HypervisorBackend trait
├── enlil-core/                 # Hypervisor core (KVM-backed dev path)
├── enlil-devices/              # Virtual device library
├── enlil-config/               # TOML guest config + validation
├── enlil-mgmt/                 # Management CLI/TUI
├── enlil-setup/                # First-run setup wizard
└── enlil-boot/                 # UEFI boot payload (Phase 6+ stub)
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
ZK-proof attestation and cross-guest isolation proofs — see [`ROADMAP.md`](ROADMAP.md) (Phases 8.7 / 8.9 / 9.12 and the ZK references).

---

## Documentation

| Document | What's in it |
|----------|--------------|
| **README.md** (this file) | Project overview — what Enlil is, its architecture, the crate layout, how to build, and where things stand today. |
| [**ROADMAP.md**](ROADMAP.md) | The development roadmap — every phase from scaffold to multi-machine mesh, tracked as a checklist, with the detailed technical design, milestones, risks, and references behind each item. |

---

## License

Not yet finalized. The roadmap recommends **MIT** or **Apache-2.0** for maximum ecosystem
compatibility with the RustVMM crates Enlil builds on.

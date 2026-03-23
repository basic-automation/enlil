# Enlil Hypervisor — Architecture

> A portable, bare-metal-capable Type-1 hypervisor written in Rust.

---

## Layer Stack

```
┌─────────────────────────────────────────────────────────────────┐
│                        Enlil Crates                             │
│  ┌──────────┐ ┌───────────┐ ┌────────────┐ ┌───────────────┐   │
│  │enlil-core│ │enlil-mgmt │ │enlil-config│ │ enlil-devices │   │
│  └────┬─────┘ └─────┬─────┘ └─────┬──────┘ └──────┬────────┘   │
│       │              │             │               │            │
│  ┌────┴──────────────┴─────────────┴───────────────┴────────┐   │
│  │                      enlil-hal                           │   │
│  │         HypervisorBackend trait (VMX/SVM/EL2/H)          │   │
│  └──────────────────────────┬───────────────────────────────┘   │
│                             │                                   │
│  ┌──────────┐          ┌────┴─────┐          ┌──────────────┐   │
│  │enlil-boot│          │enlil-setup│         │  (all crates  │   │
│  │UEFI stub │          │setup TUI │          │  use full     │   │
│  └──────────┘          └──────────┘          │  std Rust)    │   │
│                                              └──────────────┘   │
├─────────────────────────────────────────────────────────────────┤
│                     Rust std / core / alloc                     │
│            (compiled for x86_64-unknown-enlil on bare-metal,    │
│             or normal x86_64-unknown-linux-gnu on hosted)       │
├─────────────────────────────────────────────────────────────────┤
│                       enlil-platform                            │
│  ┌──────────┐ ┌──────────┐ ┌──────┐ ┌───────┐ ┌────┐ ┌─────┐  │
│  │ Memory / │ │Threading │ │ Sync │ │ Async │ │Time│ │ I/O │  │
│  │  Alloc   │ │          │ │      │ │Runtime│ │    │ │     │  │
│  └──────────┘ └──────────┘ └──────┘ └───────┘ └────┘ └─────┘  │
│                                                                 │
│  ┌─────────────────────────┐  ┌────────────────────────────┐    │
│  │   platform-linux        │  │   platform-baremetal       │    │
│  │   (delegates to std)    │  │   (raw HW implementations) │    │
│  └─────────────────────────┘  └────────────────────────────┘    │
├─────────────────────────────────────────────────────────────────┤
│              Hardware  /  Linux + KVM Host                      │
│         x86_64 (VMX/EPT)  ·  ARM (EL2)  ·  RISC-V (H-ext)     │
└─────────────────────────────────────────────────────────────────┘
```

---

## Crate Map

### `enlil-platform` — Foundation

The custom Rust platform layer. Every other crate depends on this.

Provides std-equivalent APIs for both hosted and bare-metal targets:

| Module     | Hosted (platform-linux)       | Bare-metal (platform-baremetal)      |
|------------|-------------------------------|--------------------------------------|
| `memory`   | Delegates to `std::alloc`     | Bump/slab allocator over UEFI mmap   |
| `threading`| Delegates to `std::thread`    | Per-core scheduler, no preemption    |
| `sync`     | Delegates to `std::sync`      | Spinlocks, ticket locks              |
| `async`    | Tokio or custom executor      | Cooperative single-threaded executor |
| `time`     | `std::time` / `Instant`       | TSC / HPET / architectural timer     |
| `io`       | `std::fs`, `std::net`         | UART, VirtIO block/net               |

The bare-metal backend is selected by compiling with `--target x86_64-unknown-enlil`.

### `enlil-hal` — Hardware Abstraction Layer

Defines the architecture-neutral hypervisor interface:

```rust
pub trait HypervisorBackend {
    type VCpu;
    type PageTable;
    type InterruptController;

    fn create_vcpu(&self, id: u32) -> Result<Self::VCpu>;
    fn run_vcpu(&self, vcpu: &mut Self::VCpu) -> Result<VmExit>;
    fn handle_exit(&self, vcpu: &mut Self::VCpu, exit: VmExit) -> Result<Action>;
    fn map_guest_memory(
        &self,
        pt: &mut Self::PageTable,
        guest_phys: u64,
        host_phys: u64,
        size: u64,
        perms: Permissions,
    ) -> Result<()>;
    fn inject_interrupt(
        &self,
        vcpu: &mut Self::VCpu,
        ic: &Self::InterruptController,
        vector: u8,
    ) -> Result<()>;
}
```

**Implementations:**

| Phase   | Backend              | `VCpu`           | `PageTable`       |
|---------|----------------------|------------------|-------------------|
| 0–5     | `KvmBackend`         | KVM fd wrapper   | KVM slot-based    |
| 6+      | `VmxBackend`         | Raw VMCS         | EPT page tables   |
| Future  | `SvmBackend`         | Raw VMCB         | NPT page tables   |
| Future  | `ArmEl2Backend`      | vCPU context     | Stage-2 tables    |
| Future  | `RiscvHBackend`      | VS-mode context  | G-stage tables    |

All architecture-specific code is gated behind `#[cfg(target_arch = "...")]`.

### `enlil-core` — Hypervisor Core

The main hypervisor logic. Architecture-agnostic; talks only to `enlil-hal`.

**Responsibilities:**

- **VM lifecycle** — Create, start, pause, resume, destroy guest VMs.
- **vCPU management** — Pin vCPUs to physical cores, schedule across guests.
- **Memory management** — Build and maintain EPT/NPT mappings via the HAL. Enforce isolation: no guest can see another guest's memory.
- **CPUID emulation** — Intercept and filter CPUID leaves. Expose a consistent virtual CPU model to guests.
- **Serial console emulation** — 16550 UART via `vm-superio`. Connects guest serial to host PTY or management console.
- **Guest boot** — Load kernels (bzImage, ELF) into guest memory via `linux-loader`. Set up boot parameters, initial register state, e820 map.

**Key dependencies (Linux/KVM phase):**

- `kvm-ioctls` — KVM ioctl wrappers
- `vm-memory` — Guest memory model (`GuestMemoryMmap`)
- `linux-loader` — Kernel/initrd loading
- `vm-superio` — Serial port emulation

### `enlil-devices` — Virtual Devices

Device emulation backends, decoupled from the core.

**Current:**

- Device bus abstraction (PIO/MMIO dispatch)
- VirtIO block stub
- VirtIO net stub

**Planned:**

- USB controller + device router (passthrough physical USB to guests)
- GPU arbiter (mediated passthrough / SR-IOV)
- NVMe storage backend
- Virtio-net with tap/macvtap backend

### `enlil-config` — Configuration

TOML-based guest definitions.

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
# ...
```

**Validation rules:**

- CPU sets must not overlap between guests.
- Memory regions must not overlap.
- Total allocated memory must not exceed physical memory minus Enlil's own reservation.
- Device assignments must be exclusive.

### `enlil-mgmt` — Management Console

CLI and TUI for live hypervisor control.

**Commands:**

| Command              | Description                        |
|----------------------|------------------------------------|
| `enlil list`         | Show all VMs and their state       |
| `enlil start <name>` | Boot a guest VM                    |
| `enlil stop <name>`  | Graceful shutdown (ACPI) or force  |
| `enlil attach <name>`| Attach to guest serial console     |
| `enlil status`       | Host resource usage, CPU topology  |

In bare-metal mode, the management console runs on a dedicated UART or VGA text console.

### `enlil-boot` — UEFI Boot Payload

Stub for Phase 6+. Will become the UEFI application entry point:

1. UEFI hands off to `enlil-boot`
2. Retrieve memory map, ACPI tables, framebuffer info from UEFI
3. Exit boot services
4. Initialize `enlil-platform` bare-metal backend
5. Hand off to `enlil-core`

**Boot media:** USB stick with EFI System Partition containing `\EFI\BOOT\BOOTX64.EFI` (the Enlil binary). Zero modifications to existing drives on the host machine.

### `enlil-setup` — First-Run Setup Wizard

Interactive TUI for initial configuration.

**Flow:**

1. Detect hardware — CPUs, memory, storage devices, GPUs, USB controllers.
2. Define guests — name, CPU count, memory, kernel.
3. Assign resources — storage partitions, GPU passthrough, USB devices.
4. Validate — check for conflicts, sufficient resources.
5. Write `config.toml`.

---

## Key Design Decisions

### 1. Platform-First

`enlil-platform` exists so that every other crate can write normal Rust — `Vec`, `HashMap`, `Arc<Mutex<T>>`, `async/await` — regardless of whether the final target is Linux or bare-metal. The platform crate absorbs all the `#[no_std]` complexity.

On Linux, the platform layer is a thin shim over `std`. On bare-metal, it provides real implementations: a global allocator, cooperative threading, spinlock-based synchronization, a timer backed by hardware counters, and basic I/O over UART.

### 2. Architecture Portability via HAL

The `HypervisorBackend` trait in `enlil-hal` is the only place where ISA-specific virtualization details appear. `enlil-core` never touches VMCS fields, EPT entries, or KVM ioctls directly — it calls trait methods.

This means adding ARM EL2 support is "just" writing a new `impl HypervisorBackend` — the core, device, config, and management crates don't change.

All x86-specific code is behind `#[cfg(target_arch = "x86_64")]` gates. ARM code will go behind `#[cfg(target_arch = "aarch64")]`, RISC-V behind `#[cfg(target_arch = "riscv64")]`.

### 3. Dual-Backend Strategy

The same crate graph compiles for two targets:

| Target                       | Platform backend    | HAL backend    | Use case              |
|------------------------------|---------------------|----------------|-----------------------|
| `x86_64-unknown-linux-gnu`   | `platform-linux`    | `KvmBackend`   | Development, Phase 0–5|
| `x86_64-unknown-enlil`       | `platform-baremetal`| `VmxBackend`   | Production, Phase 6+  |

Feature flags and `cfg` attributes select the right backend at compile time. No runtime dispatch overhead.

### 4. Custom Target Triple

`x86_64-unknown-enlil` is a custom Rust target specification (JSON) that:

- Disables the default `std` sysroot
- Uses the Enlil platform layer as the OS abstraction
- Sets the linker to produce a static PIE or flat binary
- Configures the correct code model and relocation strategy for bare-metal

Rust `std` is recompiled against `enlil-platform` using `-Z build-std`.

### 5. USB Live Boot

Enlil is designed to boot from a USB stick as a UEFI application. This means:

- No bootloader chain (GRUB, etc.) — Enlil *is* the bootloader
- No modifications to existing drives on the host
- Plug in USB → select in BIOS → Enlil boots → guests start
- Configuration lives on the USB stick alongside the binary

### 6. cfg-Gated Architecture Code

Every piece of architecture-specific code is behind `#[cfg(target_arch = "...")]`:

```rust
#[cfg(target_arch = "x86_64")]
mod vmx;

#[cfg(target_arch = "aarch64")]
mod el2;

#[cfg(target_arch = "riscv64")]
mod hext;
```

This is enforced at the crate level. `enlil-core` contains zero architecture-specific imports.

---

## Dependency Graph

```
enlil-mgmt ──────┐
enlil-setup ─────┤
                  ▼
enlil-core ◄── enlil-config
  │   │
  │   ├──► enlil-devices
  │   │
  ▼   ▼
enlil-hal
  │
  ▼
enlil-platform
  │
  ├── platform-linux      (cfg: target_os = "linux")
  └── platform-baremetal   (cfg: target_os = "enlil")

enlil-boot ──► enlil-platform + enlil-core  (bare-metal entry point)
```

---

## KVM-to-Bare-Metal Migration Plan

### Phase 0 — Scaffold

- Set up workspace with all crates.
- Define `HypervisorBackend` trait in `enlil-hal`.
- Implement `enlil-platform` with `platform-linux` backend only.
- Stub out all crates with minimal types.

### Phase 1 — Single Guest on KVM

- Implement `KvmBackend` in `enlil-hal`.
- `enlil-core`: create a VM, load a kernel, run a single vCPU.
- Guest boots to serial console output.
- Memory management via KVM memory slots.

### Phase 2 — Device Emulation

- Serial console emulation (16550 UART via `vm-superio`).
- VirtIO block device (read-only disk image).
- PIO/MMIO exit handling in `enlil-core`.

### Phase 3 — Multi-Guest

- Multiple VMs running concurrently.
- Per-guest memory isolation (separate KVM memory slots).
- CPUID filtering per guest.
- `enlil-config` parsing and validation.

### Phase 4 — Management & Setup

- `enlil-mgmt` CLI: list, start, stop, attach.
- `enlil-setup` TUI: hardware detection, config generation.
- VirtIO networking with tap backend.

### Phase 5 — Hardening

- Resource limit enforcement.
- Guest isolation audit.
- Performance tuning (huge pages, vCPU pinning).
- Comprehensive test suite.

### Phase 6 — Bare-Metal Bootstrap

- Define `x86_64-unknown-enlil` target JSON.
- Implement `platform-baremetal`: global allocator, UART I/O, TSC timer.
- `enlil-boot`: UEFI application entry, memory map retrieval, boot services exit.
- Recompile `std` with `-Z build-std` against `enlil-platform`.
- Boot to a serial prompt on real hardware (no guests yet).

### Phase 7 — Native VMX

- Implement `VmxBackend` in `enlil-hal`:
  - VMXON / VMXOFF lifecycle
  - VMCS allocation and configuration
  - VMLAUNCH / VMRESUME
  - VM-exit handling
- EPT page table construction (map guest physical → host physical).
- Single guest boots on bare metal with native VMX.

### Phase 8 — Feature Parity

- Multi-guest on bare metal.
- Interrupt virtualization (posted interrupts, virtual APIC).
- Device passthrough (IOMMU / VT-d).
- USB device routing between guests.
- GPU arbitration (mediated passthrough or SR-IOV).

### Phase 9 — Production

- USB live boot image builder.
- Secure boot signing.
- Hot-plug support for devices.
- Live migration between hosts (stretch goal).
- ARM and RISC-V HAL backends.

---

## Build & Target Matrix

```
cargo build                                    # Linux/KVM (default)
cargo build --target x86_64-unknown-enlil \
            -Z build-std=core,alloc,std        # Bare-metal
```

| Feature Flag       | Effect                                      |
|--------------------|---------------------------------------------|
| `platform-linux`   | Use std-delegating platform (default)       |
| `platform-baremetal`| Use bare-metal platform implementations    |
| `backend-kvm`      | Compile KVM HAL backend                     |
| `backend-vmx`      | Compile native VMX HAL backend              |

---

## Directory Layout

```
enlil/
├── Cargo.toml              # Workspace root
├── ARCHITECTURE.md         # This file
├── config.toml             # Default guest configuration
├── crates/
│   ├── enlil-platform/     # Platform abstraction
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── linux/      # platform-linux backend
│   │   │   └── baremetal/  # platform-baremetal backend
│   │   └── Cargo.toml
│   ├── enlil-hal/          # Hardware abstraction layer
│   ├── enlil-core/         # Hypervisor core
│   ├── enlil-devices/      # Virtual device backends
│   ├── enlil-config/       # Configuration parsing
│   ├── enlil-mgmt/         # Management console
│   ├── enlil-boot/         # UEFI boot payload
│   └── enlil-setup/        # Setup wizard
└── targets/
    └── x86_64-unknown-enlil.json  # Custom target spec
```

---

## Security Model

- **Guest isolation is non-negotiable.** EPT/NPT enforces memory separation at the hardware level. No guest can access another guest's memory or Enlil's own memory.
- **CPU partitioning.** Each vCPU is pinned to a physical core. CPU sets must not overlap between guests (validated by `enlil-config`).
- **Device isolation.** Passthrough devices use IOMMU (VT-d / SMMU) to restrict DMA to the owning guest's memory.
- **Minimal attack surface.** Enlil runs no OS — it *is* the OS. No syscalls, no kernel modules, no userspace. The only code running at ring 0 / EL2 is Enlil itself.
- **Rust safety.** `unsafe` is confined to `enlil-platform` (allocator, threading primitives) and `enlil-hal` (VMX/SVM instructions, page table manipulation). All other crates are safe Rust.

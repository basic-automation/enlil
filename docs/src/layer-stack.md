# Layer stack

Enlil is built on two pillars: a **platform-first** layering and an
architecture-neutral **HAL**. The platform-first idea is that every crate above
the platform layer writes ordinary Rust — `Vec`, `HashMap`, `Arc<Mutex<T>>`,
`async`/`await` — whether the final target is Linux or bare metal. The platform
crate absorbs all the `no_std` complexity.

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

## The platform layer (`enlil-platform`)

Six modules — memory/alloc, threading + scheduler, sync, async runtime, time,
I/O — each with two backends selected by feature flag:

| Feature flag        | Effect                                                    |
|---------------------|-----------------------------------------------------------|
| `platform-linux`    | std-delegating platform (default for hosted builds)       |
| `platform-baremetal`| raw-hardware implementations; the crate becomes `#![no_std]` + `alloc` |

Under `platform-baremetal`, the `memory`, `sync`, `threading`, and `time`
modules already cross-compile for `x86_64-unknown-enlil` /
`x86_64-unknown-uefi` and are linked and boot-driven by `enlil-boot` (`io` and
`async_rt` are still hosted-only pending a `no_std` `IoError` type and a
`BTreeMap`-based reactor). The selection is by feature flag at compile time —
there is no runtime backend switch at this layer.

## The HAL (`enlil-hal`)

`enlil-hal` defines a single architecture-neutral `HypervisorBackend` trait, so
the rest of the system never touches VMCS fields, EPT entries, or KVM ioctls
directly. Adding ARM EL2 or native VMX is "just" a new
`impl HypervisorBackend` behind a `#[cfg(target_arch = "…")]` gate — the core,
device, config, and management crates don't change.

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

## Intended dependency direction

`enlil-mgmt` / `enlil-setup` → `enlil-config` / `enlil-core` → `enlil-devices`
→ `enlil-hal` → `enlil-platform`, with `enlil-boot` as the bare-metal entry
point over `enlil-platform` + `enlil-core`. Some of those edges are still being
wired through the HAL (today `enlil-core` is the KVM-backed VMM and
`enlil-devices` is a standalone library) — see the
[migration plan](migration-plan.md).

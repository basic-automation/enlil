# Crate structure

The workspace (`resolver = "2"`, edition 2024 unless noted) holds nine focused
crates. This chapter mirrors the README's crate map; the README remains the
canonical short reference.

## `enlil-platform` — the foundation

std-equivalent APIs (memory/alloc, threading + scheduler, sync, async runtime,
time, I/O) for both hosted and bare-metal targets. See the
[layer stack](layer-stack.md) for the `platform-linux` /
`platform-baremetal` feature-flag split. Key modules: `async_rt`, `io`,
`memory`, `sync`, `threading`, `time`, plus `init()` dispatch and
`backend_name()`.

## `enlil-std` — std facade

A `std`-shaped facade (`collections`, `future`, `io`, `sync`, `thread`, `time`)
over `enlil-platform`, with integration tests. It exists to prove the platform
layer can carry ordinary Rust on the custom target.

## `enlil-hal` — hardware abstraction

The architecture-neutral `HypervisorBackend` trait — the only place
ISA-specific virtualization details are meant to appear. Backends: KVM (via
`kvm-ioctls`, Linux-only) today; native VMX/SVM, ARM EL2, RISC-V H-extension
are future `impl`s behind `cfg` gates.

## `enlil-core` — hypervisor core

VM lifecycle, vCPU management, memory partitioning, EPT, CPUID/SMBIOS/ACPI
handling, serial console, timing stealth, vTPM. Carries the RustVMM stack
(`kvm-ioctls`, `vm-memory`, `vm-superio`, `linux-loader`, …) gated to Linux for
the KVM-backed dev path.

## `enlil-devices` — virtual device library

VirtIO net/block, full ACPI table synthesis, SMBIOS, interrupt controllers
(LAPIC/IOAPIC/MSI), timers (PIT/HPET/TSC/paravirt), PS/2, HDA audio, qcow2/raw
storage, a virtual **xHCI** USB stack with routing, an inter-guest **bridge**
(clipboard, drag-and-drop, shared FS, notifications, URL/protocol-handler
routing), a display compositor (Enlil Zones, including per-monitor multi-monitor
tiling), and anti-detection **stealth** modules (CPUID, timing, LBR, PMC).

## `enlil-config` — configuration

TOML guest definitions and validation: no overlapping CPU sets, memory, or
device assignments. Guests declare firmware (`[guest.<id>.firmware]` with OVMF
code/vars images) among other resources.

## `enlil-mgmt` — management console

`clap` CLI + `ratatui`/`crossterm` TUI for live control
(list/start/stop/attach/status).

## `enlil-setup` — first-run wizard

Detects hardware (`/sys`, `/proc`: CPU/RAM, NVMe drives, IOMMU groups, GPUs,
USB controllers, NICs), defines guests, assigns resources, and writes
`config.toml`. The interactive TUI prompts are still stubbed.

## `enlil-boot` — UEFI boot payload + early kernel

The bare-metal entry point. A real `efi_main` collects the boot handoff and
calls `ExitBootServices()`, then `kernel_entry` brings the machine up under
Enlil's own control: memory-map walk, switching heap, IDT/GDT+TSS,
x2APIC + LAPIC timer, PIT-calibrated TSC monotonic clock, per-CPU TLS, own
identity page tables, SMP AP bring-up, ACPI/PCI discovery, GOP text console,
and AMD SVM enable — then it runs guests through a real `#VMEXIT` dispatch
loop (per-exit emulation, `VMSAVE`/`VMLOAD` around `VMRUN`, `EVENTINJ`
interrupt injection, long-mode guests, NPT write-protection). QEMU+OVMF
boot-proven.

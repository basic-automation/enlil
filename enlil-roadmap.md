# Enlil — Bare-Metal Hypervisor

> *Enlil — the Sumerian god who separated heaven from earth, ruled the space between, and assigned domains to lesser gods.*

A Rust-based Type-1 hypervisor that turns a single x86 desktop into multiple transparent virtual PCs with granular peripheral routing and GPU sharing.

---

## Project Identity

- **Language:** 100% Rust (no_std for core, std for tooling)
- **Foundation:** RustVMM crate ecosystem
- **Target Hardware:** x86_64 desktops with Intel VT-x/VT-d or AMD-V/AMD-Vi
- **Guest OS Support:** Linux and Windows (transparent — guests must not detect the hypervisor)
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
  - `enlil-core` — hypervisor core logic (VMM entry, vCPU management)
  - `enlil-devices` — virtual device backends (USB router, GPU arbiter, storage, net)
  - `enlil-config` — configuration parsing, guest definitions, peripheral routing rules
  - `enlil-mgmt` — management console (CLI/TUI for live control)
  - `enlil-boot` — UEFI boot payload (bare-metal target, Phase 6+)
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

### 0.3 Documentation
- Write ARCHITECTURE.md documenting the crate structure and design decisions
- Document the KVM-to-bare-metal migration plan
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
│  • Waker → sets "ready" flag + IPI         │
│  • Reactor: APIC timer + interrupt-driven  │
│  • Integrates with thread scheduler        │
│    (async tasks are just lightweight        │
│     cooperative tasks within a thread)     │
└────────────────────────────────────────────┘
```

- **Executor model:** Integrate with the per-CPU thread scheduler. Async tasks are cooperative (they yield at `.await` points). The scheduler runs the next ready task.
- **Waker implementation:** When an async task is waiting on I/O (e.g., VirtIO queue notification), the waker is triggered by the hardware interrupt handler. The interrupt sets the task as "ready" and sends an IPI (Inter-Processor Interrupt) if the task's CPU is running something else.
- **No full Tokio dependency** — Tokio is too heavy and assumes Linux. Instead, build a minimal executor inspired by:
  - `async-task` crate (lightweight task abstraction, no OS deps)
  - `smol` runtime architecture (simple, small, well-structured)
  - Redox's event-driven I/O model
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

---

## Phase 4 — USB Peripheral Routing

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
- Two implementation strategies:
  - **VFIO USB passthrough** (simpler): if the host has multiple physical USB controllers, assign entire controllers to guests via IOMMU. Each guest sees a real controller.
  - **Software-emulated xHCI** (flexible): fully emulate xHCI in the hypervisor, forwarding individual device traffic to/from physical devices. This allows per-device routing but is more complex.
- Start with VFIO controller-level passthrough, then build software xHCI for per-device routing

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

### 5.4 Timing Stealth
- RDTSC/RDTSCP: use TSC offsetting in VMCS to compensate for VM exit overhead
  - Measure average exit cost and subtract from TSC offset
  - Make time appear to flow continuously from the guest's perspective
- Disable or carefully handle RDTSC exit interception (prefer TSC offsetting over trapping)
- Handle `cpuid` timing: some detectors measure how long CPUID takes (it's slower under virtualization)
  - Cache CPUID results in the hypervisor to minimize exit time

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
- Build a test suite that runs inside the guest and checks for hypervisor presence:
  - CPUID checks (leaf 0x1 bit 31, leaf 0x40000000)
  - Timing checks (RDTSC delta around CPUID)
  - SMBIOS/ACPI string checks
  - Registry checks (Windows creates entries for Hyper-V, VMware, etc.)
  - Device driver checks (no virtio, vmware tools, etc. in device manager)
- Test against common anti-cheat and DRM software
- **Milestone:** Windows 11 installs and runs as a guest, passes basic anti-VM detection tools

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
│ Tier 2      │ SR-IOV PARTITIONING               │
│ (If HW      │ GPU supports SR-IOV (Intel, some  │
│  supports)  │ NVIDIA enterprise): HW partition   │
│             │ into virtual functions              │
├─────────────┼───────────────────────────────────┤
│ Tier 3      │ MEDIATED PASSTHROUGH              │
│ (Primary    │ Our custom implementation:         │
│  target)    │ intercept GPU MMIO/commands,       │
│             │ multiplex at command stream level   │
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
- Config:
  ```toml
  [guest.windows.gpu]
  mode = "passthrough"
  device = "0000:01:00.0"   # PCI BDF of the GPU
  ```
- Handle GPU reset on guest reboot (FLR — Function Level Reset)
- Handle GPU ROM (option ROM / vBIOS) loading for the guest

### 7.3 Tier 2 — SR-IOV (Detect & Enable)
- Check PCI capabilities for SR-IOV support on detected GPUs
- If supported:
  - Enable SR-IOV via PCI config space (set NumVFs)
  - Each Virtual Function appears as a separate PCI device
  - Assign one VF per guest via IOMMU
- Intel (Xe/Arc GPUs with SR-IOV): most likely to work on consumer hardware soonish
- NVIDIA: only enterprise GPUs (A100, etc.) — detect and enable if present
- AMD: limited SR-IOV support — detect and enable if present

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

### 8.2 Audio
- Virtual HDA (Intel HD Audio) controller per guest, OR
- Pass through physical audio devices
- Handle audio mixing if both guests produce sound (hypervisor-level mixer)

### 8.3 Display Routing
- Support multiple physical monitors: route each to a different guest
- Handle display hot-plug (EDID changes, resolution negotiation)
- KVM switch-like behavior: keyboard shortcut to swap which guest is on which monitor

### 8.4 Suspend/Resume
- Save full guest state to disk (hibernation)
- Resume guests after host power cycle
- Useful for: update host firmware, move guests to different hardware

### 8.5 Performance Monitoring
- Per-guest CPU utilization, memory pressure, GPU usage, IO throughput
- Expose via management console metrics

### 8.6 Security Hardening
- Verify IOMMU is active and enforced (prevent DMA attacks between guests)
- Validate all guest interactions with virtual devices (fuzzing)
- Memory encryption (AMD SEV / Intel TDX awareness for future)

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

**CUDA Shim (Stretch Goal):**
- Intercept CUDA runtime API calls (`cudaLaunchKernel`)
- Translate PTX (NVIDIA's portable IR) to SPIR-V using existing tooling (`ptx-to-spirv` projects exist)
- Forward as normal SPIR-V dispatch
- This is the hardest because CUDA's memory model and API surface are large

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
- CPU compilation strategy:
  - Use `spirv-cross` to lower SPIR-V to C/C++ with explicit vectorization hints
  - JIT compile using `cranelift` or LLVM (via `inkwell` crate)
  - Map GPU workgroups → CPU threads (one thread per workgroup)
  - Map GPU invocations within a workgroup → SIMD lanes (AVX2: 8-wide, AVX-512: 16-wide)
  - Handle GPU shared memory (`workgroup` storage class) → thread-local stack allocation
- Cache compiled variants to disk so reboot doesn't re-trigger JIT

### 9.5 Work Router

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
│ spirv-cross   │  SPIR-V → C with SIMD intrinsics
│ (or custom    │  GPU workgroup → function call
│  lowering)    │  GPU invocation → SIMD lane
└──────┬───────┘
       │
       ▼
┌──────────────┐
│ JIT Compiler  │  C/IR → native x86 (AVX2/AVX-512)
│ (cranelift    │  Compile once, cache forever
│  or LLVM)     │
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
jit = "cranelift"               # cranelift | llvm

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

---

## Key Technical Risks & Mitigations

| Risk | Severity | Mitigation |
|------|----------|------------|
| GPU mediated passthrough requires deep vendor-specific knowledge | Critical | Start with Intel (best docs), use open-source drivers as reference, fall back to time-slicing |
| Consumer GPUs lack SR-IOV | High | Tier system means we don't depend on it — it's opportunistic |
| NVIDIA proprietary driver detection of hypervisor | High | Stealth CPUID + timing mitigation; test extensively; time-sliced passthrough avoids MMIO interception |
| Bare-metal driver coverage | High | Service VM approach (Phase 6.6) dramatically reduces this |
| Windows activation / anti-piracy detection | Medium | Proper SMBIOS + ACPI + CPUID stealth; virtual TPM for consistent identity |
| Anti-cheat arms race | Medium | Maintain test suite; this is ongoing work, not a one-time fix |
| VRAM save/restore performance for time-slicing | Medium | VRAM page-table remapping instead of copying; prioritize foreground guest |
| SPIR-V → CPU JIT performance gap vs native GPU | Medium | Adaptive router learns which kernels are CPU-viable; don't force it — only route to CPU when GPU is saturated |
| CUDA shim coverage for Windows workloads | Medium | Start with Vulkan/OpenCL (well-defined SPIR-V path); CUDA shim is a stretch goal |
| Guest buffer zero-copy GPU access via IOMMU | Medium | Fall back to copy-in/copy-out if IOMMU mapping is infeasible for specific allocations |

---

## Development Milestones Summary

| # | Milestone | Validates |
|---|-----------|-----------|
| M0 | Single Linux guest boots to shell over serial (KVM-backed) | RustVMM integration, basic VMM loop |
| M1 | Full `std` Rust compiles for `x86_64-unknown-enlil` target, threads + async + sync working on Linux backend | Platform layer, custom target, dual-backend architecture |
| M2 | Two Linux guests run simultaneously on dedicated cores | CPU partitioning, memory isolation, multi-guest |
| M3 | Time-sliced vCPUs work when cores are overcommitted | Scheduler, context save/restore |
| M4 | Guests have disks and networking | VirtIO device layer, virtual switch |
| M5 | USB devices routed to specific guests | USB monitor, routing engine, virtual xHCI |
| M6 | Windows 11 installs and boots as a guest | ACPI/SMBIOS synthesis, vTPM, UEFI boot |
| M7 | Windows guest passes anti-VM detection | CPUID stealth, timing stealth, full transparency |
| M8 | Boots from UEFI without host OS, platform-baremetal backend active | Bare-metal kernel, hardware discovery, platform layer bare-metal swap |
| M9 | GPU passthrough to one guest | IOMMU GPU assignment, vBIOS, reset handling |
| M10 | GPU time-sliced between two guests | State save/restore, VRAM management |
| M11 | GPU mediated passthrough (Intel first) | Command interception, VRAM partitioning |
| M12 | Enlil Compute ICD installed in Linux guest, SPIR-V reaches hypervisor | VirtIO compute device, ICD packaging |
| M13 | SPIR-V kernel executes on CPU backend via JIT | spirv-cross lowering, cranelift JIT, buffer mapping |
| M14 | Router dynamically selects CPU vs GPU per dispatch | Routing heuristics, adaptive learning, queue monitoring |
| M15 | Windows guest Vulkan compute app runs on fabric transparently | Windows ICD, cross-platform VirtIO driver |

---

## Immediate Next Steps (Weeks 1–2)

1. `cargo init --name enlil` with workspace layout including `enlil-platform` crate
2. Create the `x86_64-unknown-enlil.json` target spec, verify `cargo build -Z build-std` compiles `core` + `alloc` for it
3. Implement `GlobalAlloc` in `enlil-platform` backed by Linux `mmap` (the `platform-linux` backend) — this unlocks `Vec`, `String`, `Box`, `HashMap` immediately
4. Implement platform threading backed by `pthread` — this unlocks `std::thread::spawn`
5. Implement platform sync backed by `futex` — this unlocks `std::sync::Mutex`, `Arc`, `Condvar`
6. Write a test binary that uses `std::thread::spawn`, `Mutex<Vec<String>>`, and `println!()` compiled for `x86_64-unknown-enlil` running on the Linux backend — **this proves the platform layer works**
7. Add `kvm-ioctls`, `vm-memory`, `vm-superio`, `linux-loader` dependencies to `enlil-core`
8. Write the VMM main loop: create VM → create vCPU → load kernel → KVM_RUN → handle exits
9. Get a minimal Linux kernel (grab a prebuilt vmlinuz + buildroot initramfs) booting to a serial shell
10. Celebrate — you have full `std` Rust AND a working VMM, the two foundations everything else builds on

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
- **Intel SDM Vol 3, Chapter 23–33:** VMX specification (the bible for bare-metal VMX programming)
- **AMD APM Vol 2, Chapter 15:** SVM specification
- **UEFI Specification:** https://uefi.org/specifications
- **ACPI Specification:** https://uefi.org/specifications (AML bytecode reference)
- **Intel GVT-g:** https://github.com/intel/gvt-linux — reference for GPU mediation on Intel
- **Bareflank:** https://github.com/Bareflank/hypervisor — C++ bare-metal hypervisor reference
- **uefi-rs:** https://github.com/rust-osdev/uefi-rs — Rust UEFI development

**Compute Fabric:**
- **SPIRV-Cross:** https://github.com/KhronosGroup/SPIRV-Cross — SPIR-V to C/C++/GLSL/HLSL/MSL transpiler
- **SPIRV-Tools:** https://github.com/KhronosGroup/SPIRV-Tools — SPIR-V optimizer, validator, linker
- **Cranelift:** https://github.com/bytecodealliance/wasmtime/tree/main/cranelift — Rust JIT compiler backend
- **Vulkan ICD Loader:** https://github.com/KhronosGroup/Vulkan-Loader — reference for ICD registration
- **Bend / HVM2:** https://github.com/HigherOrderCO/Bend — research reference for hardware-agnostic compute via Interaction Combinators
- **Mesa / Lavapipe:** https://docs.mesa3d.org/drivers/llvmpipe.html — CPU-based Vulkan/OpenGL, reference for SPIR-V → CPU compilation

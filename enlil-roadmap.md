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
| `kvm-ioctls` | KVM interface (Phase 1 — development on Linux host) |
| `vm-allocator` | Resource allocation (IRQs, MMIO ranges, PIO) |
| `event-manager` | Async event loop for device I/O |
| `vm-device` | Device model traits and bus abstractions |

**Important Note:** RustVMM crates target KVM as the hardware interface. For our bare-metal target we'll initially develop as a KVM-backed VMM (like Firecracker/Cloud Hypervisor), then progressively replace the KVM layer with our own bare-metal VMX/SVM driver in later phases. This is the pragmatic path — get functionality working on KVM first, then go bare-metal.

---

## Phase 0 — Project Scaffold & Dev Environment

**Goal:** Repo structure, toolchain, CI, and a "hello world" VMM that boots a minimal Linux guest using KVM + RustVMM.

**Duration:** 2–3 weeks

### 0.1 Repository Setup
- Initialize Cargo workspace with these top-level crates:
  - `enlil-core` — hypervisor core logic (VMM entry, vCPU management)
  - `enlil-devices` — virtual device backends (USB router, GPU arbiter, storage, net)
  - `enlil-config` — configuration parsing, guest definitions, peripheral routing rules
  - `enlil-mgmt` — management console (CLI/TUI for live control)
  - `enlil-boot` — UEFI boot payload (bare-metal target, Phase 5+)
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

## Phase 1 — Multi-Guest CPU & Memory Partitioning

**Goal:** Run two Linux guests simultaneously on the same host, each with dedicated CPU cores and isolated memory.

**Duration:** 4–6 weeks

### 1.1 Guest Configuration System
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

### 1.2 CPU Scheduling — Core Dedication
- For each guest, create N vCPUs and pin each to a physical core using `sched_setaffinity`
- Each vCPU runs in its own OS thread, pinned 1:1 to a physical core
- Expose only the assigned cores via crafted CPUID responses
- Handle CPUID interception to report correct topology (core count, package, cache)

### 1.3 CPU Scheduling — Time-Slicing Fallback
- When physical cores < total requested vCPUs, implement time-slicing:
  - Use a cooperative scheduler: each vCPU gets a time quantum (e.g., 10ms)
  - On quantum expiry, save full vCPU state (registers, MSRs, FPU/SSE/AVX via XSAVE)
  - Context-switch to next vCPU on that physical core
  - Use APIC timer or TSC deadline for preemption
- Config option: `scheduling = "dedicated" | "timeslice" | "auto"`
  - `auto`: dedicate when possible, timeslice remaining

### 1.4 Memory Isolation
- Use KVM's memory slot mechanism to give each guest a contiguous physical memory region
- Each guest sees memory starting at physical address 0 (via EPT/NPT)
- Implement a simple physical memory allocator in `enlil-core` that carves host RAM into guest regions
- Reserve memory for the hypervisor itself (management console, device backends, page tables)

### 1.5 Per-Guest Serial Console
- Each guest gets its own emulated COM1 (via `vm-superio`)
- Multiplex output to separate PTYs or a tmux-style management console
- **Milestone:** Two Linux guests running simultaneously, each with their own shell over serial, on dedicated cores

---

## Phase 2 — Virtual Device Layer & Storage

**Goal:** Give each guest block devices and network so they're usable systems, not just serial consoles.

**Duration:** 4–6 weeks

### 2.1 VirtIO Block Device
- Use `vm-virtio` to implement virtio-blk backends
- Each guest config specifies disk images or raw partitions:
  ```toml
  [guest.linux1.disks]
  vda = { path = "/dev/nvme0n1p3", readonly = false }
  vdb = { path = "/images/data.qcow2", readonly = false }
  ```
- Support raw images and (stretch) qcow2
- For NVMe passthrough (better perf): use VFIO to pass entire NVMe namespaces when IOMMU is available

### 2.2 VirtIO Network
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

### 2.3 Interrupt Virtualization
- Set up virtual IOAPIC and local APIC for each guest
- Use KVM's irqchip or implement split irqchip for finer control
- Configure MSI/MSI-X passthrough for assigned devices
- Intel: enable APICv / posted interrupts for direct interrupt delivery
- AMD: enable AVIC equivalent

### 2.4 Virtual Timer & Clock
- Provide each guest with:
  - Emulated PIT (i8254) — legacy, needed for BIOS-era boot
  - Emulated HPET
  - TSC offsetting so each guest's TSC starts at 0
  - KVM clock / Hyper-V reference TSC (for Linux/Windows paravirt clocks)
- Ensure RDTSC doesn't leak host timing (use TSC offset in VMCS)

### 2.5 Management Console v1
- Build a TUI (using `ratatui`) in `enlil-mgmt` that shows:
  - Running guests and their CPU/memory usage
  - Serial console access (tab between guests)
  - Basic controls: start, stop, reboot guest
- Connect via Unix socket from `enlil-core`
- **Milestone:** Two Linux guests with disks, networking, and a management TUI

---

## Phase 3 — USB Peripheral Routing

**Goal:** Granular per-device USB routing so each guest gets specific physical USB devices.

**Duration:** 3–4 weeks

### 3.1 USB Subsystem Architecture
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

### 3.2 Host USB Enumeration
- On startup, enumerate all USB devices via the physical xHCI controller
- Track device connect/disconnect events (hot-plug monitoring)
- Identify devices by: VID:PID, serial number, physical port path (bus topology)
- Maintain a live device inventory accessible from the management console

### 3.3 Routing Policy Engine
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

### 3.4 Virtual xHCI Controller
- Present each guest with an emulated xHCI (USB 3.x) host controller
- Two implementation strategies:
  - **VFIO USB passthrough** (simpler): if the host has multiple physical USB controllers, assign entire controllers to guests via IOMMU. Each guest sees a real controller.
  - **Software-emulated xHCI** (flexible): fully emulate xHCI in the hypervisor, forwarding individual device traffic to/from physical devices. This allows per-device routing but is more complex.
- Start with VFIO controller-level passthrough, then build software xHCI for per-device routing

### 3.5 Management Console — USB Controls
- Add USB tab to TUI showing:
  - All physical USB devices with current routing assignment
  - Reassignment interface (select device → select target guest)
  - Hot-plug notifications
- **Milestone:** Two mice and two keyboards plugged in, each routed to a different guest, with live reassignment via TUI

---

## Phase 4 — Windows Guest Support & Transparency

**Goal:** Boot Windows as a guest with full transparency — the OS and applications must not detect the hypervisor.

**Duration:** 6–8 weeks

### 4.1 ACPI Table Synthesis
- Generate per-guest ACPI tables from scratch:
  - **RSDP** → **XSDT** → **FADT**, **MADT**, **DSDT**, **SSDT**, **MCFG**, **HPET**
  - MADT: define virtual APIC topology matching assigned vCPUs
  - DSDT/SSDT: AML bytecode defining virtual devices (PCI bus, ISA bus, power management)
  - MCFG: PCI Express config space for virtual PCI devices
- Use `acpi_tables` crate or write raw AML generation
- Tables must look like they came from a real motherboard vendor (use realistic OEM strings)

### 4.2 SMBIOS Synthesis
- Generate SMBIOS/DMI tables that report:
  - Plausible system manufacturer, product name, serial number
  - Real CPU model string (pass through from physical CPU)
  - Correct memory configuration matching allocated RAM
  - BIOS vendor string (match a common vendor like AMI or Phoenix)
- Windows reads these extensively during setup and activation

### 4.3 CPUID Stealth
- Intercept all CPUID exits and craft responses:
  - **Leaf 0x1, ECX bit 31:** Clear the hypervisor present bit
  - **Leaf 0x40000000–0x400000FF:** Return zeros (no hypervisor signature)
  - **Leaf 0x0:** Report correct vendor string (GenuineIntel / AuthenticAMD)
  - **Leaf 0x1:** Report correct family/model/stepping from physical CPU
  - **Leaf 0x4, 0xB:** Report virtual topology (only assigned cores)
  - **Leaf 0x80000002–0x80000004:** Pass through real CPU brand string
- Ensure all reserved/undefined leaves return 0 (some detectors check these)

### 4.4 Timing Stealth
- RDTSC/RDTSCP: use TSC offsetting in VMCS to compensate for VM exit overhead
  - Measure average exit cost and subtract from TSC offset
  - Make time appear to flow continuously from the guest's perspective
- Disable or carefully handle RDTSC exit interception (prefer TSC offsetting over trapping)
- Handle `cpuid` timing: some detectors measure how long CPUID takes (it's slower under virtualization)
  - Cache CPUID results in the hypervisor to minimize exit time

### 4.5 Virtual TPM 2.0
- Required for Windows 11
- Implement a software TPM 2.0 (use `swtpm` as reference or integrate with the `tpm2-tss` ecosystem)
- Each guest gets its own virtual TPM with independent PCR banks, endorsement keys, etc.
- Expose via MMIO at standard TPM address (0xFED40000)
- Store TPM state persistently per guest (for BitLocker, Windows Hello, etc.)

### 4.6 Windows Boot Path
- Option A: OVMF (UEFI firmware for VMs) — boots Windows in UEFI mode
  - Provide virtual UEFI firmware (OVMF) to each guest
  - Pass our synthetic ACPI/SMBIOS tables through OVMF
- Option B: Direct Windows boot (harder — requires understanding Windows boot protocol)
- Start with OVMF (well-tested, supports Secure Boot)

### 4.7 Windows-Specific Virtual Devices
- Emulate or pass through:
  - Virtual GPU (see Phase 6) — Windows needs a display adapter for desktop
  - Virtual audio (HDA controller) or passthrough physical audio
  - PS/2 keyboard/mouse as fallback (Windows expects these early in boot)
  - PCI Express root complex
  - ACPI power management (S3/S4/S5 sleep states)

### 4.8 Anti-Detection Testing
- Build a test suite that runs inside the guest and checks for hypervisor presence:
  - CPUID checks (leaf 0x1 bit 31, leaf 0x40000000)
  - Timing checks (RDTSC delta around CPUID)
  - SMBIOS/ACPI string checks
  - Registry checks (Windows creates entries for Hyper-V, VMware, etc.)
  - Device driver checks (no virtio, vmware tools, etc. in device manager)
- Test against common anti-cheat and DRM software
- **Milestone:** Windows 11 installs and runs as a guest, passes basic anti-VM detection tools

---

## Phase 5 — Bare-Metal Boot (UEFI Payload)

**Goal:** Remove the Linux host dependency. Enlil boots directly from UEFI firmware as the first code that runs.

**Duration:** 8–12 weeks

### 5.1 UEFI Application
- Write a UEFI application in Rust using `uefi-rs` crate
- Boot flow:
  1. UEFI firmware initializes hardware, provides memory map, ACPI tables, PCI enumeration
  2. Our UEFI app is the boot payload (configured in UEFI boot manager)
  3. App consumes UEFI services: memory map, GOP (framebuffer), PCI protocol
  4. App calls ExitBootServices() — takes full control of hardware
  5. App transitions to our hypervisor kernel

### 5.2 Bare-Metal Kernel
- Replace KVM layer with direct VMX/SVM programming:
  - **Intel:** VMXON, VMCLEAR, VMPTRLD, VMLAUNCH/VMRESUME, VMREAD/VMWRITE
  - **AMD:** VMRUN, VMSAVE/VMLOAD, #VMEXIT handling
- Implement our own:
  - Physical memory manager (buddy allocator or bitmap)
  - Page table management (host page tables for hypervisor, EPT/NPT for guests)
  - Interrupt handling (IDT setup, APIC configuration)
  - Per-CPU data structures (one per physical core)
  - Timer infrastructure (APIC timer for scheduling)
- **This is the hardest engineering phase** — essentially writing a minimal OS kernel

### 5.3 Hardware Discovery (Without Linux)
- Parse ACPI tables from UEFI to discover:
  - CPU topology (MADT/SRAT)
  - PCI devices (MCFG for ECAM, walk PCI config space)
  - IOMMU (DMAR for Intel VT-d, IVRS for AMD-Vi)
  - Memory map (UEFI memory map + E820-style conversion)
  - USB controllers (PCI enumeration → xHCI BARs)
- Build our own device tree from this discovery

### 5.4 IOMMU Programming
- Program Intel VT-d (DMAR) or AMD-Vi (IVRS) directly:
  - Build DMA remapping tables (DRHD → context tables → page tables)
  - Assign PCI devices to guest IOMMU domains
  - Enable interrupt remapping
- This is critical for device passthrough security and GPU passthrough

### 5.5 Direct Device Management
- Take over all device management previously handled by Linux:
  - xHCI driver for USB (replaces libusb/VFIO)
  - NVMe driver for storage (or AHCI for SATA)
  - Network driver (minimal — for management network)
  - Framebuffer driver (from UEFI GOP) for management console display
- This is a substantial amount of driver code. Consider:
  - Porting minimal drivers from Redox OS (Rust-based OS)
  - Using a thin "service VM" (like Xen's Dom0) that runs Linux to handle drivers — this is the pragmatic Xen-style approach and may be necessary for driver coverage

### 5.6 Service VM Option (Recommended)
- Rather than writing every driver from scratch, boot a privileged lightweight Linux VM:
  - It has direct hardware access for driver support
  - It runs the management console
  - Guest VMs get devices through backends in the service VM
  - Similar to Xen Dom0 / Hyper-V parent partition
- This dramatically reduces the bare-metal driver burden
- Guest VMs are still fully isolated and transparent

### 5.7 Milestone
- Boot from UEFI → Enlil kernel → Launch service VM → Launch guest VMs
- All Phase 1–4 functionality works without a pre-existing OS

---

## Phase 6 — GPU Sharing

**Goal:** Multiple guests share GPU(s) with a tiered strategy based on hardware capabilities.

**Duration:** 12–16 weeks (ongoing research)

### 6.1 GPU Strategy Tier System

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

### 6.2 Tier 1 — Full Passthrough (Implement First)
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

### 6.3 Tier 2 — SR-IOV (Detect & Enable)
- Check PCI capabilities for SR-IOV support on detected GPUs
- If supported:
  - Enable SR-IOV via PCI config space (set NumVFs)
  - Each Virtual Function appears as a separate PCI device
  - Assign one VF per guest via IOMMU
- Intel (Xe/Arc GPUs with SR-IOV): most likely to work on consumer hardware soonish
- NVIDIA: only enterprise GPUs (A100, etc.) — detect and enable if present
- AMD: limited SR-IOV support — detect and enable if present

### 6.4 Tier 3 — Mediated Passthrough (Primary Research Target)

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

### 6.5 Tier 4 — Time-Sliced Passthrough (Highest Compatibility)

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

### 6.6 GPU Configuration
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

## Phase 7 — Polish, Hardening & Advanced Features

**Duration:** Ongoing

### 7.1 Live Migration Between Strategies
- Allow switching GPU strategy at runtime (e.g., switch from timeslice to passthrough when one guest shuts down)
- Allow live USB device reassignment (already in Phase 3)

### 7.2 Audio
- Virtual HDA (Intel HD Audio) controller per guest, OR
- Pass through physical audio devices
- Handle audio mixing if both guests produce sound (hypervisor-level mixer)

### 7.3 Display Routing
- Support multiple physical monitors: route each to a different guest
- Handle display hot-plug (EDID changes, resolution negotiation)
- KVM switch-like behavior: keyboard shortcut to swap which guest is on which monitor

### 7.4 Suspend/Resume
- Save full guest state to disk (hibernation)
- Resume guests after host power cycle
- Useful for: update host firmware, move guests to different hardware

### 7.5 Performance Monitoring
- Per-guest CPU utilization, memory pressure, GPU usage, IO throughput
- Expose via management console metrics

### 7.6 Security Hardening
- Verify IOMMU is active and enforced (prevent DMA attacks between guests)
- Validate all guest interactions with virtual devices (fuzzing)
- Memory encryption (AMD SEV / Intel TDX awareness for future)

---

## Key Technical Risks & Mitigations

| Risk | Severity | Mitigation |
|------|----------|------------|
| GPU mediated passthrough requires deep vendor-specific knowledge | Critical | Start with Intel (best docs), use open-source drivers as reference, fall back to time-slicing |
| Consumer GPUs lack SR-IOV | High | Tier system means we don't depend on it — it's opportunistic |
| NVIDIA proprietary driver detection of hypervisor | High | Stealth CPUID + timing mitigation; test extensively; time-sliced passthrough avoids MMIO interception |
| Bare-metal driver coverage | High | Service VM approach (Phase 5.6) dramatically reduces this |
| Windows activation / anti-piracy detection | Medium | Proper SMBIOS + ACPI + CPUID stealth; virtual TPM for consistent identity |
| Anti-cheat arms race | Medium | Maintain test suite; this is ongoing work, not a one-time fix |
| VRAM save/restore performance for time-slicing | Medium | VRAM page-table remapping instead of copying; prioritize foreground guest |

---

## Development Milestones Summary

| # | Milestone | Validates |
|---|-----------|-----------|
| M0 | Single Linux guest boots to shell over serial (KVM-backed) | RustVMM integration, basic VMM loop |
| M1 | Two Linux guests run simultaneously on dedicated cores | CPU partitioning, memory isolation, multi-guest |
| M2 | Time-sliced vCPUs work when cores are overcommitted | Scheduler, context save/restore |
| M3 | Guests have disks and networking | VirtIO device layer, virtual switch |
| M4 | USB devices routed to specific guests | USB monitor, routing engine, virtual xHCI |
| M5 | Windows 11 installs and boots as a guest | ACPI/SMBIOS synthesis, vTPM, UEFI boot |
| M6 | Windows guest passes anti-VM detection | CPUID stealth, timing stealth, full transparency |
| M7 | Boots from UEFI without host OS | Bare-metal kernel, hardware discovery, IOMMU |
| M8 | GPU passthrough to one guest | IOMMU GPU assignment, vBIOS, reset handling |
| M9 | GPU time-sliced between two guests | State save/restore, VRAM management |
| M10 | GPU mediated passthrough (Intel first) | Command interception, VRAM partitioning |

---

## Immediate Next Steps (Week 1)

1. `cargo init --name enlil` with workspace layout
2. Add `kvm-ioctls`, `vm-memory`, `vm-superio`, `linux-loader` dependencies
3. Write the VMM main loop: create VM → create vCPU → load kernel → KVM_RUN → handle exits
4. Get a minimal Linux kernel (grab a prebuilt vmlinuz + buildroot initramfs) booting to a serial shell
5. Celebrate — you have a working VMM foundation to build everything else on

---

## Reference Resources

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

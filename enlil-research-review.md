# Enlil Roadmap — Research Review & Improvement Analysis

> Based on a review of recent scientific papers, industry developments (2024–2025), and open-source projects across all domains covered by the Enlil roadmap.

---

## 1. Microsoft OpenVMM & OpenHCL — The Paravisor Model

**What it is:** Microsoft open-sourced OpenVMM (Oct 2024), a modular Rust-based VMM, alongside OpenHCL, a paravisor that runs *inside* the guest at a higher privilege level than the guest OS. Over 1.5 million Azure VMs run with OpenHCL. OpenHCL uses OpenVMM to provide device emulation and translation from within the guest partition itself — essentially "virtual firmware."

**Key architectural insight:** OpenVMM requires fine-grained control over thread and task scheduling to avoid introducing jitter into guest VMs. They found that traditional thread-based designs were insufficient, which led them to a custom async-first architecture. They explicitly call out that they chose not to build on existing Rust VMMs (Cloud Hypervisor, Firecracker) because of these scheduling requirements.

**Improvement for Enlil:**

- **Phase 1 (Platform Layer):** OpenVMM's experience validates our investment in a custom async runtime. But we should go further — their lesson was that *scheduling granularity matters more than raw throughput*. The Enlil async executor should support **priority-aware task scheduling** where vCPU tasks can preempt device I/O tasks mid-await. Add priority inheritance for when a low-priority task holds a lock a vCPU task needs. This isn't in the current roadmap.

- **New concept — Paravisor mode:** Consider adding an OpenHCL-style mode where Enlil's device backends run *inside* a guest at a higher privilege level (VMPL on AMD, TDX partitioning on Intel), rather than only in the hypervisor or service VM. This would allow Enlil to provide services to unenlightened guests (older Windows/Linux versions) without modifying them. This is particularly relevant for Windows transparency — instead of crafting perfect synthetic ACPI tables externally, you could intercept and translate them from within. **Recommend adding as a Phase 8.7 stretch goal.**

---

## 2. Confidential Computing — AMD SEV-SNP & Intel TDX

**What it is:** Both AMD and Intel now ship hardware that encrypts VM memory so the hypervisor cannot read it. Intel TDX (Trust Domain Extensions) and AMD SEV-SNP (Secure Encrypted Virtualization - Secure Nested Paging) are in production on all major cloud providers as of 2024–2025. A December 2024 ACM SIGMETRICS paper ("Confidential VMs Explained") provides the most thorough empirical analysis to date.

**Key findings from research:**

- Performance overhead is workload-dependent: 1–5% for compute-bound, up to 60% for heavy network I/O due to bounce buffer requirements. Memory-intensive workloads can see up to 431% overhead in worst cases due to encrypted page management.
- VMEXITs are significantly more expensive under CVM modes because the hardware must save/restore encrypted state.
- The Trusted Computing Base (TCB) of a CVM includes the entire guest OS — millions of lines of code.
- A 2025 CCS paper ("BadAML") demonstrated exploiting legacy firmware interfaces (ACPI AML) to compromise confidential VMs — directly relevant to Enlil's ACPI synthesis work.

**Improvement for Enlil:**

- **Phase 5 (Windows/Transparency):** The BadAML paper is a warning — our synthetic ACPI tables must be carefully validated to avoid introducing exploitable firmware interfaces. Add ACPI AML fuzzing to Phase 5.8 (Anti-Detection Testing).

- **Phase 8 (Security Hardening):** The roadmap mentions "AMD SEV / Intel TDX awareness for future" but this is too vague. Recommend upgrading to a concrete sub-phase: **Phase 8.7 — Confidential VM Support.** On Intel TDX hardware, Enlil could run each guest as a Trust Domain, providing hardware-enforced memory encryption between guests — a massive security improvement over software isolation alone. On AMD SEV-SNP, each guest gets its own encryption key managed by the AMD Secure Processor. This requires:
  - Modifying EPT/NPT management to use encrypted pages
  - Implementing GHCB (AMD) or TDCALL (Intel) guest communication protocols
  - Handling the fact that VMEXITs work differently under CVM mode
  - Attestation support (guests can verify they're running on genuine hardware)

- **Performance implication for Phase 9 (Compute Fabric):** Under SEV-SNP/TDX, the hypervisor *cannot read guest memory*. This means the zero-copy buffer approach in Phase 9.8 won't work for confidential guests — the fabric would need to use bounce buffers or shared memory regions explicitly designated as unencrypted by the guest. This is a significant architectural constraint that should be documented now.

---

## 3. GPU Virtualization — Current State of the Art (2025)

**What research shows:**

- A 2025 paper from Samara University provides the most recent comparative analysis of consumer GPU virtualization methods. Overhead measurements: GPU passthrough 0%, SR-IOV 1–2%, MIG (NVIDIA Multi-Instance GPU) 3–5%, time-sliced vGPU 5–10%. VMware's 2024 MLPerf benchmarks show vGPU achieving 94–105% of bare-metal for inference.

- Intel iGPU SR-IOV is now working on consumer hardware (12th gen+) via community DKMS patches (i915-sriov-dkms project). This is huge — it means Tier 2 (SR-IOV) in our roadmap is viable on consumer Intel hardware *today*, not "soonish" as the roadmap says. However, Intel has confirmed that discrete Arc GPUs (Alchemist) will NOT support SR-IOV.

- A June 2025 paper in *Future Generation Computer Systems* thoroughly benchmarks mediated passthrough rigidity, providing detailed analysis of where mediated approaches break down.

- NVIDIA consumer GPUs still do not support SR-IOV or official vGPU. The vgpu-unlock community project works on Turing and older architectures but not Ampere+.

**Improvement for Enlil:**

- **Phase 7 (GPU Sharing), Tier 2:** Update to reflect that Intel iGPU SR-IOV is *working now* on 12th gen+ via the `i915-sriov-dkms` project. This should be an early implementation target — it's the lowest-effort GPU sharing approach that works on consumer hardware. Specifically recommend: detect Intel 12th+ gen iGPU → enable SR-IOV via kernel/driver → assign VFs to guests. This could work in Phase 2–3 timeframe rather than waiting for Phase 7.

- **Phase 7, Tier 3:** The mediated passthrough research suggests focusing on the command submission layer (ring buffers / doorbells) rather than trying to intercept at the MMIO register level. The 2025 paper confirms that MMIO-level interception has rigidity problems — subtle timing and ordering dependencies cause instability. A command-buffer-level intercept (similar to what ACRN does for USB via TRB interception) is more robust.

- **New: NVIDIA MIG consideration.** For users with NVIDIA Ampere+ datacenter GPUs (A100, A30, H100), MIG (Multi-Instance GPU) provides hardware-partitioned GPU instances with only 3–5% overhead. While not consumer hardware, if Enlil detects MIG-capable GPUs, it should expose MIG instances as separate virtual GPUs to guests. This is simpler than mediated passthrough and higher performance. Add as Tier 2.5.

---

## 4. SPIR-V as Universal Compute IR — Industry Convergence

**What's happened since the roadmap was written:**

- **Microsoft announced (September 2024) that DirectX 12 will accept SPIR-V** starting with Shader Model 7, replacing DXIL as the interchange format. This is a seismic shift — it means Windows applications will natively produce SPIR-V, eliminating the need for a CUDA/HLSL translation shim for DirectCompute workloads.

- The SPIR-V backend has been promoted from experimental to official status in LLVM (December 2024), providing a unified approach for diverse compute and graphics workloads.

- Intel's oneAPI/SYCL compiler already JIT-compiles SPIR-V to x86_64 CPU targets via the `spir64_x86_64` target triple. This is exactly the SPIR-V → CPU JIT pipeline that Phase 9.6 describes — and Intel has a production implementation we can study.

- A project called SoftCompute (lighttransport/softcompute on GitHub) directly implements CPU JIT execution of SPIR-V compute shaders, though it appears to be a research prototype.

**Improvement for Enlil:**

- **Phase 9 (Compute Fabric):** The Microsoft SPIR-V announcement dramatically strengthens our bet on SPIR-V as the interchange format. Once Shader Model 7 ships, even Windows DirectCompute workloads will produce SPIR-V natively. Update the roadmap to note that the "CUDA shim (Stretch Goal)" in 9.2 becomes less critical — the industry is converging on SPIR-V regardless.

- **Phase 9.6 (CPU Backend):** Instead of building a custom SPIR-V → x86 JIT from scratch, strongly consider using Intel's open-source `oneAPI` SPIR-V to x86 compilation pipeline, or the LLVM SPIR-V backend directly. Intel already solves the SPIR-V → AVX2/AVX-512 vectorization problem. Reusing their work (via LLVM's `spir64_x86_64` target) would save months of development and produce higher quality output than a custom cranelift-based JIT. **Recommend changing the default JIT from cranelift to LLVM** (via the `inkwell` Rust crate for LLVM bindings), specifically because the SPIR-V → LLVM → x86 path is now officially supported.

- **Phase 9.4 (Compilation Cache):** Add a warm-up strategy: on first guest boot, pre-compile commonly-used SPIR-V kernels (identified by profiling typical workloads) to both CPU and GPU backends. This eliminates JIT latency on first dispatch. Intel's oneAPI documentation describes eager vs. lazy JIT strategies that we should adopt.

---

## 5. USB Passthrough — ACRN's Architecture as Reference

**What research shows:**

- Intel's ACRN hypervisor (open source, designed for embedded/automotive) has the most mature USB virtualization architecture of any lightweight hypervisor. Their documentation describes a full virtual xHCI implementation that intercepts at the TRB (Transfer Request Block) level, routing individual USB devices to specific guest VMs via a port mapper backed by libusb.

- QEMU's xHCI emulation (nec-usb-xhci) is limited to 4 hardcoded ports per controller, which creates hub cascading issues for USB 3.0 devices.

- The VFIO community consistently reports stability issues with PCI USB controller passthrough — Function Level Reset (FLR) often fails, causing VMs to hang after reboot. This is a known problem across Proxmox, Unraid, and bare-metal KVM setups.

**Improvement for Enlil:**

- **Phase 4 (USB Routing):** Use ACRN's USB architecture as the primary reference instead of QEMU's. ACRN's TRB-level interception with per-port mapping to libusb is almost exactly what Enlil needs. Specifically, ACRN's `xhci.c` DM (device model) implementation handles doorbell register interception, TRB parsing, and USB core abstraction — all in a well-documented codebase designed for real-time embedded use.

- **Phase 4.4 (Virtual xHCI):** The roadmap says "Start with VFIO controller-level passthrough, then build software xHCI for per-device routing." Based on community reports of FLR instability with VFIO USB controller passthrough, recommend **prioritizing the software xHCI from the start**. The VFIO approach is fragile in practice and doesn't provide per-device granularity anyway. ACRN proves that a software xHCI backed by libusb is the robust path.

---

## 6. Rust Hypervisor Ecosystem — New Projects to Study

**Rust-Shyper (2023, Computers & Security journal):** A type-1 bare-metal Rust hypervisor targeting embedded mixed-criticality systems, tested on NVIDIA Jetson TX2. It supports VM migration and hypervisor live-update — the live-update feature is particularly relevant for Enlil (update the hypervisor without rebooting guests). They benchmark against KVM and Jailhouse, showing competitive performance.

**Microsoft Hyperlight (November 2024):** A Rust library for executing small functions in individual micro-VMs with per-function hypervisor isolation. Boot time is fast enough for per-request isolation. While the use case is different from Enlil, their approach to minimal VMM initialization is interesting for the compute fabric — could individual SPIR-V kernel dispatches benefit from lightweight isolation?

**Hypervisor 101 in Rust (tandasat):** A complete one-day course on hardware-assisted virtualization in Rust for Intel/AMD, with full source code. Excellent reference for Phase 6 bare-metal implementation — covers VMCS setup, VMLAUNCH, and exit handling in pure Rust.

**Improvement for Enlil:**

- **Phase 6:** Add Rust-Shyper's live-update mechanism to the roadmap. Being able to update the hypervisor without rebooting guests is a compelling feature. Rust-Shyper achieves this by saving full VM state, replacing the hypervisor binary in memory, and restoring VMs on the new version. This maps to Phase 8.4 (Suspend/Resume) but is more powerful — it's a hypervisor-only restart.

- **Phase 0:** Add `tandasat/Hypervisor-101-in-Rust` as a reference resource. It's the most practical Rust-specific hypervisor tutorial available.

---

## 7. Anti-Detection / Stealth — The Arms Race (2024–2025)

**What research shows:**

- A detailed 2020 article from secret.club (referenced in 2024 anti-cheat research) describes IET (Instruction Execution Time) divergence testing — comparing CPUID execution time against a deliberately slow instruction using IA32_APERF MSR instead of TSC. This is harder to spoof than RDTSC because APERF counter emulation is complex.

- LBR (Last Branch Record) stack analysis post-VMEXIT: anti-cheats check whether the last branch target after CPUID matches what it should be on real hardware. Open-source hypervisors typically don't save/restore LBR state, creating a detectable artifact.

- A December 2025 systematic review paper on kernel-level anti-cheat classifies systems with a rootkit score ≥4, noting that modern anti-cheats use boot-time installation, stealth callbacks, virtualized drivers, and VM-Exit detection.

- Practical KVM stealth patches exist — modifying `arch/x86/kvm/cpuid.c` to remove hypervisor leaf handlers, combined with RDTSC offset compensation, successfully bypasses BattlEye detection.

**Improvement for Enlil:**

- **Phase 5.4 (Timing Stealth):** The current roadmap only mentions RDTSC/RDTSCP interception. This is insufficient against modern detection. Add:
  - **IA32_APERF/MPERF MSR handling** — emulate these performance counters to maintain consistent ratios with TSC, preventing IET divergence detection.
  - **LBR save/restore across VMEXITs** — save the full LBR stack on VMEXIT, restore on VMRESUME. This prevents anti-cheats from detecting that a branch to the hypervisor occurred.
  - **CPUID caching with constant-time responses** — precompute all CPUID leaf results and serve them from a lookup table to minimize exit latency variance.

- **Phase 5.8 (Anti-Detection Testing):** Add the `pafish` tool (Paranoid Fish) as a mandatory test suite — it tests CPUID, RDTSC, registry, SMBIOS, NIC MAC, and other vectors. Also add a custom IET divergence test using APERF.

---

## 8. Summary of Recommended Roadmap Changes

| Priority | Change | Phase | Impact |
|----------|--------|-------|--------|
| **Critical** | Switch Phase 9.6 JIT from cranelift to LLVM (SPIR-V backend is now official) | 9.6 | Months of dev time saved, higher quality CPU JIT |
| **Critical** | Prioritize software xHCI over VFIO controller passthrough for USB | 4.4 | Avoids known FLR stability issues, enables per-device routing from day 1 |
| **High** | Add Intel iGPU SR-IOV (12th gen+) as an early GPU sharing target | 7.3 | Working consumer GPU sharing achievable in Phase 2–3 timeframe |
| **High** | Add priority-aware async task scheduling with priority inheritance | 1.6 | Prevents device I/O from adding jitter to vCPU scheduling |
| **High** | Expand timing stealth: APERF/MPERF emulation, LBR save/restore, CPUID caching | 5.4 | Required for modern anti-cheat bypassing |
| **High** | Note Microsoft's SPIR-V adoption for DirectX 12 Shader Model 7 | 9.2 | CUDA shim deprioritized; SPIR-V convergence validates architecture |
| **Medium** | Add Confidential VM support phase (SEV-SNP / TDX) | 8.7 | Hardware-enforced memory encryption between guests |
| **Medium** | Add hypervisor live-update (from Rust-Shyper research) | 8.4 | Update Enlil without rebooting guests |
| **Medium** | Add ACPI AML fuzzing to anti-detection testing (from BadAML paper) | 5.8 | Prevents synthetic ACPI tables from introducing exploitable interfaces |
| **Medium** | Document CVM constraint on compute fabric zero-copy buffers | 9.8 | Architecture must handle encrypted guest memory |
| **Low** | Add paravisor mode as stretch goal (from OpenHCL architecture) | 8.7 | Alternative approach to guest transparency |
| **Low** | Add NVIDIA MIG detection/support for datacenter GPUs | 7.3 | Opportunistic; only matters for enterprise hardware |
| **Low** | Add `pafish` and custom IET divergence test to anti-detection suite | 5.8 | More thorough stealth validation |

---

## References

- Microsoft OpenVMM / OpenHCL: https://github.com/microsoft/openvmm
- Microsoft Hyperlight: https://github.com/hyperlight-dev/hyperlight
- Rust-Shyper (Computers & Security, 2024): https://www.sciencedirect.com/science/article/abs/pii/S1383762123001273
- "Confidential VMs Explained" (ACM SIGMETRICS, Dec 2024): https://dl.acm.org/doi/10.1145/3700418
- "BadAML" (ACM CCS, Nov 2025): Exploiting Legacy Firmware Interfaces to Compromise CVMs
- GPU Virtualization Comparative Analysis (Vestnik Samara, 2025): https://journals.ssau.ru/est/article/view/29605
- Intel iGPU SR-IOV DKMS: https://github.com/strongtz/i915-sriov-dkms
- SPIR-V LLVM Backend Official Promotion (Dec 2024): https://www.phoronix.com/news/Intel-LLVM-SPIR-V-Official-Plan
- Microsoft DirectX SPIR-V Announcement (Sep 2024): https://www.khronos.org/spirv/
- ACRN USB Virtualization: https://projectacrn.github.io/latest/developer-guides/hld/usb-virt-hld.html
- Anti-Cheat VM Detection (secret.club): https://secret.club/2020/04/13/how-anti-cheats-detect-system-emulation.html
- Hypervisor 101 in Rust: https://github.com/tandasat/Hypervisor-101-in-Rust
- SoftCompute (SPIR-V CPU JIT): https://github.com/lighttransport/softcompute
- Mediated Passthrough Benchmarking (FGCS, Jun 2025): Future Generation Computer Systems

---

## 2026-06-02 — KVM Backend vCPU Run-Loop & Memory-Region API (Phase 0.2 / 5)

Context: reconstructed `enlil-core::kvm_backend` (the prior commit's file had been
clobbered with a tool placeholder and never contained real code). Researched the
current rust-vmm patterns to make sure the run-loop/exit model matches the ecosystem.

- **rust-vmm `vm-device` IoManager dispatch** —
  https://github.com/rust-vmm/vm-device — the canonical interface splits guest
  access handling into `pio_read/pio_write/mmio_read/mmio_write`. *How it changes
  the build:* validates our new `VmExitHandler` trait (io_in/io_out/mmio_read/
  mmio_write). When we wire `enlil-devices::bus::Bus` to the backend, implement
  `VmExitHandler` for the bus and forward to the existing device dispatch rather
  than inventing a parallel path — keep one bus, one address-decode.
- **`kvm-ioctls` `VcpuExit` semantics** —
  https://docs.rs/kvm-ioctls/latest/kvm_ioctls/enum.VcpuExit.html — for `IoIn`/
  `MmioRead` the handler must fill the provided slice *before* the next `KVM_RUN`;
  KVM returns those bytes to the guest. *Confirms* our dispatch fills the buffer
  in place inside `run_vcpu` before returning the owned `GuestExit` summary.
- **`KVM_SET_USER_MEMORY_REGION2` + `guest_memfd`** (QEMU/LKML 2025:
  https://lwn.net/Articles/938597/ , https://lkml.org/lkml/2025/8/22/472) — the
  newer memslot ioctl backs regions with private `guest_memfd` for confidential
  VMs (TDX/SEV-SNP) and supports per-page shared/private attributes + dirty-ring.
  *How it changes the build:* our `map_memory` deliberately uses the classic
  `set_user_memory_region` (hva-backed) — correct for Phase 5 transparency where
  the host must read/synthesise guest memory (ACPI/SMBIOS injection). Flag for
  Phase 8/CVM work: a second `map_private_memory` path on
  `set_user_memory_region2` will be needed if we ever target confidential guests,
  and it is incompatible with host-side memory introspection.

## 2026-06-03 — Device-bus address decode (PIO/MMIO dispatch) for the run loop (Phase 0.2 / 5)

Context: building the real `enlil-devices::bus` dispatcher that `VmExitHandler`
will forward to (the prior `PioBus`/`MmioBus` were base→index stubs with no device
storage and no upper-bound check). Confirmed the design against rust-vmm.

- **rust-vmm `vm-device` IoManager / Mut{Pio,Mmio}Device** —
  https://github.com/rust-vmm/vm-device/blob/main/README.md ,
  https://docs.rs/vm-device/latest/vm_device/ — canonical model: *separate* PIO and
  MMIO buses; devices are registered over an **address range**; on each access the
  manager checks a device is registered **for the requested address** and only then
  dispatches; `MutDevicePio`/`MutDeviceMmio` take `&mut self` (matches our
  `VmExitHandler`'s `&mut self`). *How it changes the build:* our `lookup` only tested
  `base <= port` (greatest lower bound) and never the **upper** bound — an access just
  past a device's last register would mis-route to that device. Fix: store
  `(base, len)` ranges and require `port < base + len`; return a `handled: bool` so
  the run loop can log/zero-or-0xFF unmapped accesses instead of silently mis-routing.
- **vm-superio `Serial` (16550A)** — https://github.com/rust-vmm/vm-superio ,
  https://docs.rs/vm-superio/ — TX is trivial (write THR byte straight to an
  `io::Write`); RX uses a bounded FIFO + an `interrupt_evt` to signal the driver;
  emulates DLL/IER/DLH/IIR/LCR/LSR/MCR/MSR/SR. *How it changes the build:* the COM1
  serial device (the 0.2 milestone "shell over serial") is a `PioDevice` over the
  8-port range `0x3F8..0x400`; our new range-checked bus must register exactly that
  span. Reuse `vm-superio::Serial` rather than re-emulating the UART when we add COM1.

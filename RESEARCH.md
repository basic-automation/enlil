# Enlil — Research Notes

> Research that informs the [Enlil roadmap](ROADMAP.md): recent scientific papers, industry
> developments (2024–2026), and open-source projects across every domain Enlil touches. Each
> finding is tied to **how it changes what we build**.

This document has two parts:

- **[Part I — Roadmap Research Review](#part-i--roadmap-research-review--improvement-analysis):**
  cross-domain findings (Rust VMMs, confidential computing, GPU virtualization, SPIR-V, USB
  passthrough, anti-detection) and the concrete roadmap changes they imply.
- **[Part II — ZK Proving Performance](#part-ii--zk-proving-performance-lessons-from-zkevm-engineering):**
  lessons from zkEVM performance engineering applied to Enlil's attestation, isolation, and
  verifiable-compute proofs (Phases 8.7, 8.9, 9.12).

---

# Part I — Roadmap Research Review & Improvement Analysis

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

# Part II — ZK Proving Performance: Lessons from zkEVM Engineering

> How the Ethereum ecosystem took ZK proving from 16 minutes to 16 seconds — and how Enlil can use the same methods.

---

## The zkEVM Performance Revolution (2023–2026)

In July 2025, the Ethereum Foundation published their realtime proving target: prove an entire Ethereum block within the 12-second slot time. Nine months later, the ecosystem achieved it — proving latency dropped from 16 minutes to under 10 seconds, costs collapsed 45×, and zkVMs now prove 99% of all Ethereum mainnet blocks within the target window.

This wasn't one breakthrough. It was the systematic application of seven interlocking techniques. Each one is directly applicable to Enlil's ZK proof system (Phases 8.7, 8.9, and 9.12).

---

## Technique 1: Precompiles (Specialized Circuits for Hot Operations)

**What zkEVM does:** The single biggest performance insight from SP1/Reth is that ~80% of time spent proving an Ethereum block goes to a handful of cryptographic operations — Keccak hashing, secp256k1 signature verification, and SHA-256. SP1's "precompile" system replaces these with hand-optimized circuits that prove these operations in near-constant time, rather than executing them instruction-by-instruction inside the general-purpose RISC-V VM. This delivers an order-of-magnitude cost reduction. The overhead of the general VM becomes negligible because cost is dominated by precompiled operations — the 80/20 principle.

**How Enlil uses this:** Enlil's ZK proofs have their own "hot operations" that dominate proving time:

- **Phase 8.7 (ZK Attestation):** The proof must verify that Enlil's binary matches a known hash, that EPT/NPT page tables are correctly configured, and that IOMMU DMA remapping tables are valid. The hot operations here are Merkle tree hashing over page table entries (tens of thousands of 4KB page mappings) and hash verification of the hypervisor binary.

- **Phase 8.9 (Isolation Proofs):** Proving EPT non-overlap between guests is essentially a set-disjointness proof over sorted page frame number lists. The hot path is comparison/sorting of PFN arrays.

- **Phase 9.12 (Verifiable Compute):** Proving correct SPIR-V execution. The hot operations are finite-field arithmetic (for GPU-style compute kernels) and memory access pattern verification.

**Recommendation:** Define Enlil-specific precompiles for RISC Zero or SP1:
1. **`enlil_merkle_ept`** — precompile for Merkle-hashing EPT/NPT page table structures. Instead of proving each page table entry hash step-by-step inside the RISC-V VM, use a dedicated circuit that takes the page table as input and produces a Merkle root in near-constant proof cost.
2. **`enlil_set_disjoint`** — precompile for proving two sorted integer arrays have no common elements (for EPT non-overlap). This is a linear scan with a simple constraint: for each pair of adjacent elements across the merged sorted list, prove they differ.
3. **`enlil_spir_v_alu`** — precompile for SPIR-V arithmetic operations (OpFAdd, OpFMul, OpIAdd, etc.). Rather than interpreting each SPIR-V instruction inside the zkVM, batch the ALU operations into a dedicated circuit.

SP1's precompile system is open-source and designed to be extended with custom circuits. RISC Zero's "accelerator circuits" serve the same purpose. Both are Rust-native, matching Enlil's language.

**Expected impact:** 5–10× reduction in proving time for attestation and isolation proofs. Potentially 50–100× for SPIR-V compute verification (where ALU operations dominate).

---

## Technique 2: Continuations (Splitting Large Proofs into Segments)

**What zkEVM does:** An Ethereum block can contain millions of operations. Proving this as a single monolithic trace would require hundreds of gigabytes of RAM and take hours. Instead, RISC Zero introduced "continuations" — the execution trace is automatically split into fixed-size segments (e.g., 2^20 cycles each), and each segment is proven independently. This has three effects: (a) memory usage becomes bounded by segment size, not total computation size; (b) segments can be proven in parallel across multiple machines/GPUs; (c) the naive super-linear scaling of proof time is broken — 2× more computation costs only 2× more proving time, not >2×.

**How Enlil uses this:** Enlil's proofs vary enormously in size:

- **EPT attestation** for a guest with 16GB RAM = ~4 million page table entries. Without continuations, this is one enormous trace. With continuations, it's split into segments of (say) 64K entries each, proven in parallel.

- **SPIR-V compute verification** for a large kernel dispatch might involve millions of shader invocations. Continuations let each workgroup be proven as a separate segment.

- **Cross-guest isolation** for 4 guests requires proving 4 × 4 = 16 pairwise non-overlaps (or more efficiently, proving that the union of all EPT mappings has no duplicates). This naturally segments by guest pair.

**Recommendation:** Design Enlil's ZK proofs with continuation-awareness from day one:
- EPT proofs: segment by page table level (PML4 → PDPT → PD → PT), proving each level independently and linking via Merkle commitments.
- SPIR-V proofs: segment by workgroup. Each workgroup is a continuation segment. The dispatcher proves workgroup assignment, each worker proves execution.
- Use RISC Zero's built-in continuation support (automatic as of 0.15+) or SP1's shard-based parallelism.

**Expected impact:** Linear scaling with hardware — 8 GPUs = ~8× faster proving. Memory usage bounded at ~4GB per segment regardless of total proof size.

---

## Technique 3: Recursive Proof Aggregation (Compress Many Proofs into One)

**What zkEVM does:** After continuations produce N segment proofs, recursive aggregation compresses them into a single proof. The prover verifies a STARK proof *inside* another STARK circuit, producing a new proof that attests to the validity of the original. By repeating this in a binary tree, N segment proofs become 1 proof in log₂(N) rounds. The final aggregated STARK is then wrapped into a SNARK (typically Groth16 over BN254) producing a ~200-byte proof that can be verified in milliseconds.

The pipeline: Execution → Segment STARKs (parallel) → Recursive aggregation (binary tree) → STARK-to-SNARK wrapping → Tiny final proof.

**How Enlil uses this:** Enlil generates multiple independent proofs that a verifier (a guest, a remote party) needs to check:

- At boot: hypervisor binary hash proof + EPT configuration proof + IOMMU configuration proof = 3 proofs that should be aggregated into 1 "Enlil is correctly configured" proof.
- For isolation: N pairwise non-overlap proofs aggregated into 1 "all guests are isolated" proof.
- For compute: M workgroup execution proofs aggregated into 1 "this kernel ran correctly" proof.

Without aggregation, the verifier must check each proof individually. With aggregation, verification is constant-time regardless of how many sub-proofs exist.

**Recommendation:**
1. **Boot attestation:** Generate segment proofs for binary integrity, EPT layout, and IOMMU configuration in parallel. Recursively aggregate into a single attestation receipt. Wrap to SNARK for compact storage. The guest stores one ~200-byte proof, not three multi-kilobyte proofs.
2. **Isolation proofs:** Same pattern — per-guest-pair proofs aggregated into a single "isolation certificate."
3. **Compute verification:** Per-workgroup proofs aggregated per-dispatch, then per-batch. A guest receives one proof per compute batch, not one per shader invocation.

Both RISC Zero and SP1 provide built-in recursion and aggregation primitives. RISC Zero's three-circuit architecture (RISC-V circuit → recursion circuit → SNARK circuit) is specifically designed for this pipeline.

**Expected impact:** Verification time becomes constant (~2ms) regardless of proof complexity. Proof size drops to ~200 bytes after SNARK wrapping. This makes in-guest verification practical even for resource-constrained guests.

---

## Technique 4: Lookup Arguments (Replacing Constraints with Table Lookups)

**What zkEVM does:** Many operations that are trivial for a CPU are expensive to express as arithmetic constraints in a ZK circuit. A 256-bit addition requires 200–300 constraints for bit decomposition and range checks. SHA-3/Keccak requires ~150,000 constraints per invocation. Lookup arguments (Plookup, LogUp) replace this explosion with a simple table membership check: precompute a table of all valid (input, output) pairs, and prove that each operation's values exist in the table. This collapses hundreds of constraints per operation into a single lookup.

LogUp (2022) improved on Plookup by using logarithmic derivatives, reducing the prover's commitment overhead by 3–4× and enabling efficient vector lookups. It's now the standard in zkEVM circuit design for range checks, bitwise operations (XOR, AND, shifts), and finite state machine transitions.

**How Enlil uses this:** Enlil's ZK circuits contain operations that are perfect lookup candidates:

- **Page table validation:** Each EPT entry has a fixed format — bits 0-11 are flags, bits 12-51 are the physical page frame number, bits 52-62 are reserved. Validating that an EPT entry is well-formed is a bitwise operation. With lookup tables, a single 4KB table of valid flag combinations replaces dozens of bit-decomposition constraints per entry.

- **IOMMU domain validation:** Each IOMMU context entry maps a BDF (bus/device/function) to a domain. Valid BDF values are a known, finite set. A lookup table of all PCIe BDFs in the system replaces per-entry range-check constraints.

- **SPIR-V opcode validation:** The SPIR-V instruction set has ~500 valid opcodes. Proving that each instruction in a kernel is a valid opcode is one lookup per instruction, not a 500-way conditional constraint.

- **Memory access pattern validation:** For SPIR-V compute verification, each memory access must be within the buffer's declared bounds. A range-check lookup table (all values 0 to buffer_size-1) replaces the constraint explosion of binary decomposition.

**Recommendation:** Use LogUp-based lookup arguments throughout Enlil's ZK circuits:
1. EPT flag validation table (2^12 = 4096 entries for all valid flag combinations).
2. SPIR-V opcode table (~500 entries).
3. Range-check tables for buffer bounds, PFN ranges, and IOMMU domain IDs.
4. Bitwise operation tables for any XOR/AND/shift needed in hash computations.

Both Plonky2/3 (used by Polygon) and SP1's AIR framework support lookup arguments natively.

**Expected impact:** 10–100× constraint reduction for bitwise-heavy operations (hash verification, flag checking). This directly translates to faster proving and lower memory usage.

---

## Technique 5: ZK-Friendly Hash Functions (Poseidon over Keccak)

**What zkEVM does:** Keccak-256 (Ethereum's native hash) is notoriously ZK-unfriendly — it uses bitwise operations (XOR, AND, rotations) that require ~150,000 arithmetic constraints per hash. The zkEVM community identified this as a primary bottleneck. Type 2 zkEVMs replace Keccak with ZK-friendly alternatives like Poseidon, which operates natively over prime fields and requires only ~300 constraints per hash — a 500× improvement.

Vitalik Buterin's March 2026 proposal (EIP-7864) calls for replacing Ethereum's entire Merkle Patricia Tree with a binary tree using Blake3 or Poseidon, specifically to accelerate proving by up to 100× over the Keccak-based structure. The state tree and VM together account for >80% of the proving bottleneck.

**How Enlil uses this:** Enlil's ZK proofs use hashing extensively:

- **Binary integrity:** Hashing the hypervisor binary to prove it matches a known value. This is potentially megabytes of data being hashed.
- **EPT Merkle trees:** Building Merkle trees over page table entries for compact attestation. The hash function choice determines proving speed.
- **IOMMU table integrity:** Same pattern as EPT — Merkle tree over DMAR/IVRS entries.

Currently, the roadmap doesn't specify which hash function the ZK proofs will use. If Enlil defaults to SHA-256 or Keccak (because they're ubiquitous), proving will be orders of magnitude slower than necessary.

**Recommendation:**
1. Use **Poseidon** as the hash function for all ZK-internal Merkle trees (EPT attestation, IOMMU verification, isolation proofs). Poseidon over the Goldilocks field (used by Plonky2/3) or BabyBear field (used by SP1/RISC Zero) is the fastest option inside a STARK circuit.
2. Use **Blake3** for hashing the hypervisor binary (it's faster than SHA-256 in both native execution and ZK circuits, and is Rust-native via the `blake3` crate).
3. **Do not use Keccak or SHA-256 inside ZK circuits** unless external compatibility requires it (e.g., matching an existing hash that a third party must verify with standard tools).

Both RISC Zero and SP1 include Poseidon as a built-in precompile with near-native circuit performance.

**Expected impact:** 100–500× reduction in hash-related proving cost. For EPT attestation of a 16GB guest (~4M page entries, ~4M hashes for the Merkle tree), this is the difference between minutes and seconds.

---

## Technique 6: GPU-Accelerated Proving

**What zkEVM does:** The two computational bottlenecks in STARK proof generation are Number-Theoretic Transforms (NTT, analogous to FFT over finite fields) and Merkle tree building. Together, these account for ~70% of prover runtime. Both are embarrassingly parallel. The zkEVM ecosystem has invested heavily in GPU proving:

- Polygon zkEVM prover: GPU acceleration on RTX 4090 reduced Merkle tree build time from 20.1s to 9.78s (51% reduction) and overall batch proof time from 133.3s to 58s.
- SP1 Turbo: CUDA-optimized GPU proving across clusters of parallelized GPUs. Single-GPU throughput of 900 KHz to several MHz. 16 GPUs achieve real-time Ethereum proving (<12 seconds).
- GPU-accelerated ZKPs are up to ~200× faster than CPU baselines (IEEE IISWC 2025).
- Ingonyama's ICICLE: open-source CUDA library for ZK primitives (NTT, MSM, Poseidon), integrated across multiple ZK frameworks.

The key insight: NTT dominates runtime as problem scales increase, contributing up to 91% of prover time at large scales. MSM (multi-scalar multiplication) is embarrassingly parallel but memory-bound. Both benefit enormously from GPU architecture.

**How Enlil uses this:** This is where Enlil has a unique advantage: **Enlil already controls the GPU**. As a hypervisor that manages GPU allocation across guests, Enlil can reserve GPU time for its own ZK proving without competing with guest workloads. In the tiered GPU architecture:

- **Mediated/time-sliced GPU modes:** Enlil owns the GPU scheduler. It can allocate proving work to idle GPU slots.
- **SR-IOV mode:** Reserve one VF (virtual function) for Enlil's prover.
- **Passthrough mode:** Use the host's iGPU (if available) or schedule proving during guest idle time.

**Recommendation:**
1. Integrate ICICLE (Ingonyama's CUDA library) for NTT and Poseidon GPU acceleration. It's Rust-friendly and already used by Polygon, Scroll, and others.
2. For SP1: use its native GPU proving support (CUDA-optimized, RTX 4090 as reference hardware).
3. For RISC Zero: use its native GPU acceleration (also CUDA-based, RTX 4090 reference).
4. In Enlil's GPU scheduler (Phase 7), add a `prover` priority class that can preempt low-priority guest GPU work for attestation/isolation proof generation.
5. For machines without discrete GPUs: use AVX-512 SIMD on the CPU. SP1 and RISC Zero both support AVX2/AVX-512 acceleration for NTT and Poseidon.

**Expected impact:** 10–200× reduction in proving time versus CPU-only. An attestation proof that takes 30 seconds on CPU could complete in <1 second on GPU.

---

## Technique 7: The "Glue + Coprocessor" Architecture

**What zkEVM does:** Vitalik Buterin's "Glue and Coprocessor" architecture (2024) formalized a pattern that the zkEVM ecosystem was already converging on: separate computation into a **general-purpose "glue" layer** (flexible, programmable, handles control flow and coordination) and **specialized "coprocessor" layers** (optimized for specific expensive operations). The EVM is the glue; precompiles are coprocessors. In ZK terms: the zkVM (RISC-V) is the glue; specialized circuits (Poseidon, secp256k1, Keccak) are coprocessors.

Brevis/Pico extended this into a modular "ProverChain" — a pipeline of pluggable proving backends (execution → recursion → compression), each optimized for its stage. Multiple field sizes (KoalaBear, BabyBear, Mersenne31) can be mixed in a single proving pipeline, using the most efficient field for each stage.

The March 2026 Buterin proposal takes this further: replace the EVM entirely with RISC-V as the base VM, because **ZK provers already internally use RISC-V**. Having the execution layer natively be RISC-V eliminates an entire translation layer and could reduce proving overhead by 50–100×. The EVM interpreter alone adds ~800× overhead to zkVM proving times.

**How Enlil uses this:** Enlil's ZK system should adopt the same layered architecture:

```
┌─────────────────────────────────────────────────┐
│           ENLIL ZK PROVING PIPELINE              │
│                                                  │
│  Stage 1: EXECUTION (general-purpose zkVM)       │
│  ├── RISC Zero or SP1 (RISC-V based)            │
│  ├── Proves: control flow, coordination logic    │
│  ├── Handles: "glue" between coprocessors        │
│  └── Field: BabyBear (SP1) or BabyBear (RZ)     │
│                                                  │
│  Stage 2: COPROCESSORS (specialized circuits)    │
│  ├── enlil_merkle_ept — EPT Merkle tree          │
│  ├── enlil_set_disjoint — isolation check        │
│  ├── enlil_poseidon — ZK-friendly hashing        │
│  ├── enlil_spir_v_alu — SPIR-V arithmetic        │
│  └── Each: hand-optimized, lookup-heavy          │
│                                                  │
│  Stage 3: RECURSION (aggregate segment proofs)   │
│  ├── Binary tree aggregation                     │
│  ├── Separate recursion circuit (smaller, faster)│
│  └── Field: may differ from Stage 1              │
│                                                  │
│  Stage 4: COMPRESSION (STARK → SNARK wrapping)   │
│  ├── Groth16 over BN254                          │
│  ├── ~200 byte final proof                       │
│  └── 2ms verification time                       │
│                                                  │
│  GPU ACCELERATION throughout (ICICLE/CUDA)       │
└─────────────────────────────────────────────────┘
```

**Recommendation:** Structure Enlil's ZK system as this four-stage pipeline from the start. Don't build a monolithic "prove everything in one circuit" approach — it won't scale.

**Expected impact:** This is the architecture that enables sub-second proving for attestation and isolation proofs. Without it, Enlil's ZK proofs will be measured in minutes. With it, seconds.

---

## Technique Summary: Combined Impact on Enlil

| Technique | zkEVM Gain | Enlil Application | Expected Enlil Gain |
|-----------|-----------|-------------------|-------------------|
| Precompiles | 10× for crypto-heavy blocks | EPT Merkle, set disjointness, SPIR-V ALU | 5–100× per proof type |
| Continuations | Linear scaling, bounded memory | Segment by page table level / workgroup | 4–8× with parallelism |
| Recursive aggregation | Constant verification time | Boot attestation, isolation certs, compute batches | O(1) verification |
| Lookup arguments | 10–100× constraint reduction | EPT flags, SPIR-V opcodes, range checks | 10–100× fewer constraints |
| ZK-friendly hashing | 100–500× over Keccak | All Merkle trees in attestation/isolation | 100–500× for hash-heavy proofs |
| GPU acceleration | 10–200× over CPU | Enlil controls the GPU — use idle capacity | 10–200× proving speed |
| Glue + coprocessor | 50–100× (removes VM interpreter layer) | Four-stage pipeline architecture | Architectural — enables all other gains |

**Compounded estimate for Phase 8.7 (ZK Attestation):**
- Current naive approach (RISC Zero, CPU, SHA-256, no precompiles): ~60–120 seconds for a 16GB guest
- With all techniques applied: Poseidon hashing (100×) + GPU proving (20×) + precompiles (5×) + continuations (parallel, 4×) → **sub-second attestation proving** is realistic on hardware with a single RTX 4090-class GPU

**Compounded estimate for Phase 9.12 (Verifiable Compute):**
- The roadmap correctly notes 100–1000× overhead. With precompiles for SPIR-V ALU ops and GPU-accelerated proving, this could drop to **10–50× overhead** for arithmetic-heavy kernels — still too expensive for every dispatch, but practical for sampled verification at much higher rates than currently planned.

---

## Concrete Roadmap Changes Recommended

### Phase 8.7 — ZK Proof Attestation

**Add to the implementation section:**

```
ZK Proving Architecture:
- Hash function: Poseidon (over BabyBear field for SP1, or Goldilocks for Plonky3)
  NOT SHA-256/Keccak — 100-500x slower in-circuit
- Precompiles:
  - enlil_merkle_ept: EPT/NPT Merkle tree (Poseidon-based)
  - enlil_binary_hash: hypervisor binary integrity (Blake3, pre-hashed in chunks)
  - enlil_iommu_verify: DMAR/IVRS table structure verification
- Continuations: segment by page table level
  PML4 proof || PDPT proof || PD proof || PT proof (4 parallel segments)
- Aggregation: recursive binary tree → single STARK → Groth16 wrap
- Final proof: ~200 bytes, verifiable in ~2ms
- GPU: use ICICLE for NTT/Poseidon, integrate with Enlil GPU scheduler
  Reserve GPU time via prover priority class
- Target: <1 second proof generation on RTX 4090-class GPU
  <10 seconds proof generation on CPU-only (AVX-512)
```

### Phase 8.9 — Cross-Guest Isolation Verification

**Add:**

```
- Isolation proof uses sorted-merge set-disjointness algorithm:
  1. Each guest's PFN list is sorted (proven via permutation argument)
  2. Merge N sorted lists into one (proven via lookup argument)
  3. Prove no adjacent elements in merged list are equal (single linear pass)
- Precompile: enlil_set_disjoint for the merge-and-compare step
- Lookup table: valid PFN range (physical memory map from UEFI)
- Proof segments: one per guest pair (N*(N-1)/2 segments, parallel)
- Aggregate all pair proofs into single "isolation certificate"
```

### Phase 9.12 — Verifiable Compute

**Revise overhead estimate and add:**

```
- SPIR-V precompiles for hot ALU operations reduce overhead from 100-1000x to 10-50x
  for arithmetic-heavy kernels (matrix multiply, reduction, convolution)
- Bitwise-heavy kernels (crypto, hashing) remain expensive: 50-200x
- Sampling rate can increase with lower overhead:
  sample_rate = "1/100"  →  "1/10" for arithmetic kernels (practical at 10-50x overhead)
- GPU proving for the prover itself:
  Enlil's GPU scheduler allocates proving work to idle GPU capacity
  On a time-sliced GPU, proving runs in Enlil's own time slice
```

---

## Key References (zkEVM Performance)

- **Ethereum Foundation zkEVM:** https://zkevm.ethereum.foundation/
- **Ethereum Foundation "Shipping an L1 zkEVM #2: Security"** (Dec 2025): https://blog.ethereum.org/2025/12/18/zkevm-security-foundations
- **SP1 Turbo:** https://blog.succinct.xyz/sp1-turbo/
- **SP1 Reth (precompile architecture):** https://blog.succinct.xyz/sp1-reth/
- **RISC Zero recursive proving:** https://dev.risczero.com/api/recursion
- **RISC Zero continuations:** https://dev.risczero.com/terminology
- **Vitalik Buterin: "The different types of ZK-EVMs"** (Aug 2022): https://vitalik.eth.limo/general/2022/08/04/zkevm.html
- **Vitalik Buterin: EVM → RISC-V proposal** (Apr 2025): https://ethereum-magicians.org/
- **Vitalik Buterin: binary state trees + RISC-V** (Mar 2026): https://www.theblock.co/post/391681
- **X Layer GPU acceleration of Polygon zkEVM prover:** https://medium.com/xlayer-official/unlocking-efficiency-how-x-layer-optimized-polygon-zkevm-prover-for-gpu-acceleration-50f49c066dc8
- **Orbiter Finance prover acceleration:** https://orbiter-finance.medium.com/the-acceleration-of-zkevm-prover-3551145b27ca
- **Paradigm: Hardware Acceleration for ZKPs:** https://www.paradigm.xyz/2022/04/zk-hardware
- **ICICLE (Ingonyama GPU library):** https://github.com/ingonyama-zk/icicle
- **Lookup arguments survey:** https://eprint.iacr.org/2025/1876
- **LogUp:** https://eprint.iacr.org/2022/1530
- **Plookup:** https://eprint.iacr.org/2020/315
- **Proof aggregation techniques (LambdaClass):** https://blog.lambdaclass.com/proof-aggregation-techniques/
- **Compiler optimizations for zkVMs (Jan 2026):** https://arxiv.org/html/2508.17518v2
- **GPU ZKP characterization (IEEE IISWC 2025):** https://arxiv.org/pdf/2509.22684
- **SoK: Understanding zkVM (2026):** https://eprint.iacr.org/2026/525
- **WebGPU ZK acceleration:** https://blog.zksecurity.xyz/posts/webgpu/
- **Brevis/Pico ProverChain architecture:** https://medium.com/@0xjacobzhao/brevis-research-report
- **Polygon proof composition/recursion/aggregation:** https://docs.polygon.technology/zkEVM/architecture/zkprover/stark-recursion/composition-recursion-aggregation/

---

## Bottom Line

The zkEVM ecosystem spent three years and hundreds of millions of dollars turning ZK proofs from "minutes" into "seconds." The techniques they developed are general-purpose and directly applicable to Enlil. The hypervisor domain is actually *easier* than the blockchain domain — Enlil's proofs are about static data structures (page tables, IOMMU tables, binary hashes) rather than arbitrary program execution. Enlil doesn't need to prove Turing-complete computation for attestation — just structural correctness of well-defined hardware configuration tables.

By adopting the precompile + continuation + recursion + lookup + Poseidon + GPU pipeline from day one, Enlil's ZK proofs can be fast enough to run at boot (attestation), on-demand (isolation verification), and at meaningful sampling rates (compute verification) — rather than being a theoretical feature that's too slow to use in practice.

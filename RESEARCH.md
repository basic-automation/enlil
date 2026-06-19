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

---

# Part III — Daily Routine Findings

> Dated notes from the autonomous daily routine. Each entry: source + how it changes what we build.

## 2026-06-05 — 16550 interrupt model (THRE/RDA, IIR priority, Trigger)

- **rust-vmm `vm-superio` `Serial`** (https://github.com/rust-vmm/vm-superio) and the
  **PC16550D datasheet** (https://www.scs.stanford.edu/10wi-cs140/pintos/specs/pc16550d.pdf):
  confirm the interrupt model for our move from polled to interrupt-driven serial. IIR reports
  the *highest-priority enabled+pending* source — RX-data-available (0x04) outranks
  THR-empty (0x02); bit 0 set (0x01) means "no interrupt". IER bit 0 (ERBFI) gates RX, bit 1
  (ETBEI) gates THRE. **Reading IIR acknowledges/clears a pending THRE interrupt** (only THRE);
  reading RBR clears RX when the FIFO drains. vm-superio drives the host IRQ via a `Trigger`
  (eventfd) — our analogue is a pluggable `IrqLine` sink the UART pulses on level change.
- **Pitfall (linux-serial "IIR/LSR out-of-sync", https://www.spinics.net/lists/linux-serial/msg03163.html):**
  the THRE-pending latch must be re-evaluated relative to LSR/IIR read ordering, or the 8250
  driver can wedge. Our `update_irq` recomputes the line level *after* each register access
  mutation (and after IIR-read clears the THRE latch), so the asserted level always matches the
  computed IIR — no stale edge.
- **FCR not emulated** (vm-superio): FIFO is treated as always-on; we keep IIR FIFO bits 6-7
  clear so a guest probes us as a plain 8250 (works polled *or* interrupt-driven), matching the
  existing register file. No change needed there.

**Changes what we build:** implement IER-honored IIR computation + a THRE latch + an `IrqLine`
sink in `enlil-core::serial::UartState`, asserting/deasserting on every state change. The
KVM `set_irq_line(4, level)` wiring through the in-kernel irqchip is the follow-on (Linux-only,
needs `/dev/kvm` to exercise).

## 2026-06-06 — i8254 PIT on the bus: read-back command + in-kernel-vs-userspace timer

- **Intel 8254 datasheet + OSDev "Programmable Interval Timer"**
  (https://wiki.osdev.org/Programmable_Interval_Timer): confirmed the **read-back command**
  (control word, bits 7-6 = `11`): bit 5 low (`/COUNT`) latches the current count of every
  selected channel, bit 4 low (`/STATUS`) latches a status byte, bits 3-1 select channels
  2/1/0. The **status byte** is bit7=OUT-pin, bit6=null-count (control word written but count
  not yet loaded), bits5-4=RW-access, bits3-1=mode, bit0=BCD. When both count and status are
  requested the status byte is delivered first on the next data-port read. Our `Pit` ignored
  the read-back command (`channel_idx == 3` → early return) — a transparency gap, since a guest
  probing timer state via read-back would read garbage.
- **QEMU `hw/timer/i8254.c` / Linux `arch/x86/kvm/i8254.c` + KVM API
  (`KVM_CREATE_PIT2`)** (https://www.kernel.org/doc/html/v5.10/virt/kvm/api.html): with KVM the
  PIT is normally emulated **in-kernel** (`KVM_CREATE_PIT2`) so it is not on our userspace bus
  at all; userspace only services PIT exits when the in-kernel model is disabled. Enlil's own
  native-VMX backend (Phase 5+, no in-kernel chip) **must** carry a userspace PIT, so mounting
  it on `DeviceBus` is correct and necessary even though the KVM path may bypass it.
- **Pitfall — Firecracker issue #2777**
  (https://github.com/firecracker-microvm/firecracker/issues/2777): allowing (re)creation of
  PIT timer channels after the guest kernel has booted is a hardening concern; channel-0 left
  running also costs steal time. Flagged for when channel-0 IRQ0 delivery is wired.

**Changes what we build:** implement the read-back command (count + status latching, status
delivered before count) and a `null_count` status bit in `enlil-devices::timer::Pit`, then
mount the PIT as a `PioDevice` on `0x40..=0x43`. Channel-0 → IRQ0 delivery (an `IrqLine`-style
sink mirroring the UART's IRQ4, driven off `Pit::tick`) is the follow-on, and needs the
KVM/native-VMX interrupt path to actually fire.

## 2026-06-06 — Legacy PCI Configuration Mechanism #1 (0xCF8/0xCFC) front-end

- **PCI Local Bus spec / OSDev "PCI" + Wikipedia "PCI configuration space"**
  (https://wiki.osdev.org/PCI, https://en.wikipedia.org/wiki/PCI_configuration_space):
  confirmed Mechanism #1's two 32-bit I/O ports — `CONFIG_ADDRESS` (`0xCF8`) and `CONFIG_DATA`
  (`0xCFC`). `CONFIG_ADDRESS` layout: bit 31 = enable, bits 23-16 = bus, 15-11 = device,
  10-8 = function, 7-2 = dword register select, 1-0 forced to `00`. A config cycle is only
  generated when the enable bit is set; otherwise `CONFIG_DATA` is open-bus. **Pitfall (byte
  steering):** since the low two register-select bits are always zero, a byte/word read of
  `CONFIG_DATA` is steered to the right sub-register by the *port offset within the
  `0xCFC`-`0xCFF` window* (`reg | (port - 0xCFC)`), not by the latched address — the emulator
  must do the masking/shifting in software. Mechanism #1 reaches only the first 256 bytes of
  config space (6-bit reg select), vs ECAM's full 4 KiB.
- **cloud-hypervisor `pci` crate `PciConfigIo` / rust-hypervisor-firmware `src/pci.rs`**
  (https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/pci/src/configuration.rs,
  https://github.com/cloud-hypervisor/rust-hypervisor-firmware/blob/main/src/pci.rs): the
  canonical Rust reference — a thin `CONFIG_ADDRESS`-latching front-end over a shared
  config-space store, exactly the shape Enlil needs. Confirms reusing one decode path for both
  the PIO (CAM) and MMIO (ECAM) front-ends rather than duplicating B/D/F decode.

**Changes what we build:** add `enlil_devices::pcie::PciConfigIo` as a `PioDevice` over
`0xCF8..=0xCFF` wrapping the existing `PcieRootComplex`, folding the latched B/D/F + register
back into the ECAM-style offset `PcieRootComplex::ecam_read/ecam_write` already decode (single
decode path). Without it the guest BIOS finds no host bridge at boot. **Follow-on:** mount ECAM
on the MMIO bus over the same root complex — which will need shared ownership (`Rc<RefCell>` or
equivalent) since both front-ends mutate the same device set.

## 2026-06-06 (c) — ECAM MMIO front-end over a shared root complex

- **PCIe ECAM addressing (OSDev "PCI Express" / Linux `PCI/acpi-info`)**
  (https://wiki.osdev.org/PCI_Express, https://www.kernel.org/doc/html/latest/PCI/acpi-info.html):
  re-confirmed the ECAM physical-address formula — `phys = ecam_base + ((bus << 20) | (device
  << 15) | (function << 12) | reg)` — which is **exactly** the offset layout
  `PcieRootComplex::ecam_read/ecam_write` already decode (`PciBdf::ecam_offset()` + reg). So the
  MMIO front-end is a thin forward: `mmio_read(offset, size) → ecam_read(offset, size)` with no
  new B/D/F decode. ECAM exposes the full 4 KiB extended config space (vs Mechanism #1's first
  256 bytes), and the MCFG ACPI table (`acpi::mcfg`, base `0xB000_0000`, buses 0..=255) is what
  tells the guest where the window lives — so the MMIO device must be mounted at that base over a
  256-bus / 256 MiB (`256 << 20`) window to match what we advertise.
- **cloud-hypervisor `PciConfigMmio` over a shared `PciBus` (`pci/src/bus.rs`)**: the canonical
  Rust shape — the MMIO (ECAM) and PIO (CAM) front-ends both hold a handle to **one** config
  store so a BAR programmed through either path is visible through the other. Confirms wrapping
  `PcieRootComplex` in `Rc<RefCell<…>>` and giving both `PciConfigIo` and a new `EcamSpace`
  clones of that handle, rather than two divergent device sets. (Single-threaded vCPU loop today,
  so `Rc<RefCell>`; revisit to `Arc<Mutex>` only when the bus must cross vCPU threads.)

**Changes what we build:** refactor `PciConfigIo` to hold `Rc<RefCell<PcieRootComplex>>`, add
`EcamSpace` as an `MmioDevice` over `[ecam_base, ecam_base + 256<<20)` forwarding to the shared
root, and a `DeviceBus::add_pcie(root)` helper that mounts CAM + ECAM over one root and seeds a
default host bridge at 0:0.0 so a guest finds something at boot. Nothing fundamentally new beyond
the 06-06 Mechanism #1 entry — this is the MMIO twin of the same single-decode-path design.

## 2026-06-07 — Device IRQ delivery: I/O APIC line wiring + MMIO front-end

Targeted check for today's step (wire PIT-IRQ0/UART-IRQ4 to interrupt delivery, then
let a guest program the routing). Most of the relevant primary material was already
logged (06-05 16550 IIR/Trigger, 06-06 i8254 in-kernel-vs-userspace, the LAPIC/IOAPIC
emulation that predates this routine). New, specifically-applied:

- **Intel 82093AA I/O APIC datasheet** (the part `enlil_devices::interrupt::ioapic`
  already models: version `0x11`, 24 RTEs, `IOREGSEL`@0x00 / `IOWIN`@0x10 at MMIO base
  `0xFEC0_0000`): a guest programs each redirection entry as **two 32-bit dword writes**
  (low = vector/delivery/mask/trigger, high = destination), selected via `IOREGSEL`. This
  is exactly the front-end an `MmioDevice` must expose; nothing decodes B/D/F, it's a thin
  forward to `IoApic::mmio_read/write(offset)`. RTEs reset **masked**, so a device that
  asserts before the OS programs the I/O APIC must deliver nothing — the transparent,
  hardware-accurate behaviour (and a free DoS guard against an un-acked line).
- **rust-vmm `vm-superio` `Trigger` + KVM `irqfd`/`KVM_IRQ_LINE` model** (already logged
  06-05): a device drives a *level* sink on real edges; the consumer routes it. Confirms
  the chosen shape — `SharedInterruptController::line(irq)` returns a `Fn(bool)+Send`
  level sink (rising edge → `deliver_irq`, falling → `clear_irq`) that satisfies **both**
  crate-local `IrqLine` traits via their blanket `impl … for F: Fn(bool)+Send`, so the PIT
  (enlil-devices) and UART (enlil-core) wire identically without a cross-crate dependency.
- **Why the LAPIC is *not* on the shared MMIO bus** (Intel SDM Vol.3 §10.4, KVM
  `KVM_CREATE_IRQCHIP`): the local APIC lives at the same physical address (`0xFEE0_0000`)
  for every CPU and an access implicitly targets the *accessing* vCPU's LAPIC. A shared
  `MmioBus` has no "current vCPU" notion, so LAPIC MMIO belongs in the per-vCPU exit path
  (or the in-kernel chip), **not** as a bus `MmioDevice` — only the I/O APIC (genuinely
  shared) is mounted on the bus.

**Changes what we build:** add `SharedInterruptController` (`Arc<Mutex<InterruptController>>`)
with a `.line(irq)` level-sink factory and a matching `clear_irq`; add `IoApicMmio` as an
`MmioDevice` at `0xFEC0_0000`; and a `DeviceBus::standard_pc_with_interrupts` that attaches
PIT→IRQ0 / UART→IRQ4 and mounts the I/O APIC aperture. Defer LAPIC MMIO to the per-vCPU path.

## 2026-06-07 (c) — MADT ISO + 8259 ELCR, then the MC146818 RTC/CMOS

Two interrupt-correctness items (MADT interrupt-source-override IRQ0→GSI 2; the chipset
ELCR making PCI `INTx` level-triggered) plus the start of the RTC/CMOS. All ancient,
stable silicon / firmware conventions — no recent paper changes the model; the
authoritative sources are the primary datasheets and the ACPI spec. Logged for the design
decisions that matter:

- **ACPI MADT Interrupt Source Override** (ACPI spec §5.2.12.5): on a PC the 8254 timer
  (ISA IRQ0) is reported as `(bus 0, source 0) → GSI 2`, so a guest in APIC mode programs
  I/O APIC pin **2** for the timer, not pin 0. The line wiring and the emitted MADT must
  agree or the timer interrupt is silently dropped. Polarity/trigger overrides (the
  active-low, level SCI) keep their GSI *number* — only IRQ0 renumbers. **Changes what we
  build:** a single `isa_to_gsi` map shared by the wiring (`isa_line`) and cross-checked
  against the MADT, so the two can't drift.
- **PIIX3 ELCR** (82371SB datasheet / "PCI interrupts are level-triggered, active-low"):
  the chipset Edge/Level Control Register (`0x4D0` master / `0x4D1` slave) selects per-line
  edge vs level for the 8259. A level line's request follows the input (withdrawn before
  INTA, re-armed after EOI while still asserted) — this is what lets a shared PCI `INTx`
  line work without being lost after the first EOI. IRQ0/1/2 (master) and IRQ8/13 (slave)
  are hardwired edge and read back 0. **Changes what we build:** add the ELCR register +
  level-triggered IRR semantics to `Pic8259`/`DualPic` and an `ElcrPort` bus device.
- **MC146818 RTC/CMOS** (Motorola MC146818A datasheet + the PC/AT CMOS map): ports
  `0x70` (index; **bit 7 is the NMI-disable**, not part of the CMOS address) / `0x71`
  (data). Registers `0x00-0x09` are the BCD/binary time fields, `0x0A-0x0D` are status
  A-D (A: UIP + rate select; B: SET/PIE/AIE/UIE + DM binary-vs-BCD + 24/12h + DSE; C:
  read-clears the IRQF/PF/AF/UF flags; D: VRT). RTC IRQ is **IRQ8** (slave PIC line 0 /
  GSI 8). Reg B's DM and 24/12h bits change how the *same* stored time reads back, so the
  device stores canonical binary fields and formats on read. **Changes what we build:** a
  new `enlil_devices::timer::rtc::Rtc146818` driven by an injected wall-clock (Unix
  seconds → civil date by pure arithmetic, no time-crate dep), with a `PioDevice` adapter
  and an `attach_irq8` `IrqLine`; wired into the bus / interrupt controllers in a later
  increment.

## 2026-06-07 (b) — Legacy 8259A PIC: the early-boot interrupt controller

Targeted check for the next unblocked step (per 06-07 hand-off #2): the dual-8259
PIC that early boot runs in *before* the OS brings up the I/O APIC. This is ancient,
stable silicon — no recent research changes the model; the authoritative sources are the
primary datasheet and the well-trodden PC/AT wiring. Logged because the design decisions
matter for transparency and for how it pairs with the existing APIC path:

- **Intel 8259A datasheet** (programmable interrupt controller): the four-byte ICW1-4
  init sequence (ICW1 on the command port with bit 4 set arms it; ICW2 = vector base in
  the upper 5 bits; ICW3 = cascade wiring; ICW4 = 8086/auto-EOI mode), then OCW1 (mask)
  on the data port and OCW2 (EOI)/OCW3 (read-select, poll, special-mask) on the command
  port. **Fully-nested fixed priority** with IR0 highest: a request is delivered only if
  strictly higher priority than every in-service level — modelled as "lowest set ISR bit
  is the ceiling". ICW1 clears the IMR and the edge-sense latch (datasheet §"Initialization
  Command Words"). Confirms our register/state layout (IRR/ISR/IMR + init step machine).
- **PC/AT dual-PIC cascade** (master `0x20`/`0x21`, slave `0xA0`/`0xA1`, slave INT → master
  IR2): the slave's `INT` output is a *level* input to the master's IR2, **not** a latched
  edge. Modelling IR2 as "computed on demand from the slave's deliverable state" (rather
  than persisting a master IRR bit 2) avoids the classic cascade-bookkeeping bugs and makes
  the two-INTA-pulse acknowledge fall out naturally: master IR2 → ISR, slave supplies the
  vector. An OS must EOI **both** chips for a cascaded line — matched by our test.
- **Transparency / DoS note:** a freshly-reset PIC must read back inertly and deliver
  nothing until a guest runs the full ICW sequence and unmasks lines — a device asserting
  before init must not reach the CPU, exactly as on bare metal. Rotating-priority OCW2
  variants are accepted but treated as their non-rotating equivalent: PC OSes use fixed
  priority + non-specific EOI, so this stays honest and fully testable rather than
  half-implementing rotation (flagged for a later increment if a guest ever needs it).

**Changes what we build:** add `enlil_devices::interrupt::pic` with a `Pic8259` single-chip
model (ICW1-4, OCW1/2/3, IRR/ISR/IMR, fully-nested resolution, poll, special-mask, auto-EOI)
and a cascaded `DualPic` (master/slave, IR2 cascade computed on demand, INTA `acknowledge()`
→ vector, `pending_vector()`/`has_interrupt()` for the INTR line). Pure-userspace, no KVM
needed. Next: a bus front-end (two PIO port adapters over a shared `DualPic`) + an `.line()`
sink so devices assert into the PIC the same way they do the I/O APIC, then a `standard_pc`
variant that mounts both. Eventual KVM binding routes PIC INTR through LAPIC LINT0 ExtINT
(or the in-kernel chip's `KVM_CREATE_IRQCHIP`, which already models the dual-8259).

---

## 2026-06-07 (d) — Completing the transparent legacy PC: chipset ports, ACPI PM hardware, PIRQ, HPET

A breadth session over well-stabilised silicon + firmware conventions — no recent paper
changes these models; the authoritative sources are the primary specs and datasheets. Logged
here so the next run doesn't re-research them.

- **ACPI 6.4 spec, §4.8 (ACPI hardware) & §5.2.9 (FADT).** The fixed PM registers an ACPI OS
  drives: **PM1a_EVT** (status W1C + enable, 0x600/4 here), **PM1a_CNT** (SCI_EN + SLP_TYP/
  SLP_EN, 0x604/2), **PM_TMR** (the 3.579545 MHz timer, 0x608/4), **GPE0_BLK** (N status + N
  enable bytes, 0x620/16). Key correctness points we folded in: (1) `SLP_TYP|SLP_EN` write to
  PM1a_CNT is the **shutdown** mechanism (the DSDT's `_S5` value), so leaving 0x604 open-bus
  hangs guest shutdown; (2) GPE0 status left open-bus reads 0xFF → the OS sees phantom GPEs
  and spins; (3) `TMR_VAL_EXT` (FADT.Flags bit 8) promises a **32-bit** PM timer, so the model
  must be 32-bit, not the 24-bit default; (4) **HW_REDUCED_ACPI (bit 20) is mutually exclusive
  with the legacy PM hardware + LEGACY_DEVICES** — a hardware-reduced OS ignores all of these
  blocks and drives sleep via SLEEP_*_REG, so a transparent full-hardware PC must clear it
  (it was wrongly set). SCI delivery from PM1/GPE events is a run-loop concern (deferred).
  **Changes what we build:** model PM1a/PM_TMR/GPE0 as bus devices, clear HW_REDUCED_ACPI, and
  derive the FADT's advertised ports from the device models so the table and the decoded
  hardware can't drift.

- **PIIX3 datasheet (Intel 290550-002), §PCI interrupt routing.** PCI devices assert one of
  four level-triggered pins INTA-D (config 0x3D); the south-bridge swizzles them by slot —
  `PIRQ[(slot + pin - 1) mod 4]` — and four PIRQRC registers (config 0x60-0x63: bit7=disabled,
  bits3:0=ISA IRQ) route each PIRQ line to a legacy IRQ. PCI IRQs are **level** (need the ELCR
  built last session). Non-PCI-routable IRQs are the hardwired 0/1/2/8/13. In APIC mode the
  four PIRQ lines instead wire straight to I/O APIC GSIs 16-19, bypassing the routing
  registers. **Changes what we build:** a `PirqRouter` model (registers + swizzle + PIC-mode
  ISA-IRQ / APIC-mode GSI resolution); the live path (a PIIX bridge device in config space
  whose 0x60-0x63 writes drive the router, and PCI INTx assertions driving SharedPic/ioapic)
  is the follow-up once a PCI device actually asserts INTx.

- **Intel 8254 (PIT) §System Control Port B / Intel ICH "NMI Status and Control" (0x61).**
  Bit 0 gates PIT channel 2 (the speaker tone), bit 1 is speaker-data-enable, bit 4 is the
  toggling DRAM-refresh clock (polled as a coarse delay), bit 5 mirrors PIT ch2 OUT, bits 6-7
  are the parity/IO-check error latches (no error → 0). **Port A (0x92):** bit 1 = fast A20
  (enabled post-firmware; KVM keeps A20 open), bit 0 = fast-reset edge. **Changes what we
  build:** the 0x61/0x92 chipset ports coupled to the shared PIT, filling the open-bus holes a
  guest hits at boot.

- **IA-PC HPET spec 1.0a.** 1 KiB MMIO block at 0xFED0_0000 (matches the ACPI HPET table);
  8-byte-aligned 64-bit registers accessed 32- or 64-bit (caps period and the main counter are
  read as two 32-bit halves on 32-bit access). **Changes what we build:** a HpetMmio adapter
  bridging the existing model's aligned decode to sub-register 32/64-bit guest accesses.

---

## 2026-06-08 — DSDT correctness: AML PkgLength, ACPI resource descriptors, _PRT/_PIC, _S5

A spec-driven session on the AML the DSDT emits — well-stabilised firmware
conventions, so the authoritative sources are the primary ACPI spec sections,
not recent papers. Logged so the next run doesn't re-derive them.

- **ACPI 6.x §20.2.4 (PkgLength encoding).** A `PkgLength` is *self-inclusive*:
  the encoded value counts from the field's own first byte to the end of the
  package, so it must include the size of the `PkgLength` field itself (1-4
  bytes). **Bug found & fixed:** `AmlBuilder::patch_pkg_length` encoded the total
  including the 4 reserved bytes and then shifted the body left without
  recomputing, overshooting by the shift amount — a real ACPI interpreter would
  read past every scope/device/method and corrupt all following AML. The DSDT had
  never been parsed (no `/dev/kvm`), so it stayed latent. `ssdt.rs` already did
  the self-reference correctly; the `AmlBuilder` (DSDT) path did not. **Changes
  what we build:** `encode_self_pkg_length` + decode-based regression tests; a
  prerequisite for every `_CRS`/`_PRT`/package below.

- **ACPI 6.x §6.4 (resource descriptors).** Small descriptors: I/O Port `0x47`
  (info, min/max u16, align, len), IRQ `0x22`/`0x23` (mask u16 + flags), End Tag
  `0x79` + checksum. Large: Memory32Fixed `0x86` (9-byte body), and the Address
  Space descriptors Word `0x88` / DWord `0x87` / QWord `0x8A` (res-type, general
  flags, type flags, then gran/min/max/xlat/len in the field width). A
  `ResourceTemplate` in ASL is just a `Buffer{ descriptors + End Tag }`. General
  flags `0x0C` = producer + min/max fixed (host-bridge windows); I/O type-flags
  `0x03` = entire range; memory type-flags bit0 = write status. **Changes what we
  build:** `ResourceTemplate` + `name_resource_template`, then `_CRS` for COM1/
  RTC/PS2/HPET (legacy devices) and the PCI root's resource-*producer* `_CRS`
  (bus-number + I/O + 32/64-bit MMIO windows) Windows needs to enumerate PCI; a
  `PNP0C02` motherboard-resources device claiming the fixed legacy controller I/O.

- **ACPI 6.x §6.2.13 (_PRT) + §5.8.1 (_PIC).** `_PRT` is a package of
  `{ Address=(dev<<16)|0xFFFF, Pin(0=INTA..3=INTD), Source, SourceIndex }`. With
  `Source=0` the routing is hard-wired and `SourceIndex` is the GSI (APIC mode).
  `_PIC(mode)` lets the OS announce PIC(0)/APIC(1) delivery; firmware stores it in
  a global a method-based `_PRT` branches on. **Changes what we build:** a static
  APIC-mode `_PRT` for PCI0 generated from `PirqRouter::device_gsi` (DSDT and the
  live `assert_pci_intx` path agree by construction: swizzle → GSI 16-19), plus a
  `_PIC` method + `PICF` flag matching real firmware. **Follow-up (pitfall):** a
  PIC-mode-only guest needs PCI Link Devices (`PNP0C0F` with `_CRS`/`_PRS`/`_SRS`
  over the programmable PIRQRC registers) and a `_PIC`-selected second `_PRT` —
  deferred because it needs If/Else/Store control-flow AML we can't validate
  without `iasl` on the runner.

- **ACPI 6.x §7.4.2.6 (_Sx).** `_S5` (and every `_Sx`) must be a **Package** of
  `{ PM1a_CNT.SLP_TYP, PM1b_CNT.SLP_TYP, ... }`, not an integer — the OS
  evaluates it and writes element 0 to PM1a_CNT to power off. The DSDT emitted
  `Name(_S5_, 0)`, so a guest had no usable S5 object and could not ACPI-shutdown.
  **Changes what we build:** `name_package` + `Name(_S5_, Package(){5,5,0,0})`,
  SLP_TYP 5 matching the value `enlil_devices::chipset` captures as a shutdown.

---

## 2026-06-08 (b) — 8237A DMA controller + DMA page registers (Phase 0.2)

- **Intel 8237A datasheet (DMA controller) + IBM PC/AT system architecture.** The
  PC/AT wires two cascaded 8237As: **DMA-1** (8-bit, channels 0-3) at ports
  `0x00-0x0F` and **DMA-2** (16-bit, channels 4-7) at `0xC0-0xDF` with its
  registers at 2-byte spacing (`offset = (port - 0xC0) >> 1`). Each channel has a
  16-bit base/current **address** and **count** register pair accessed through a
  shared **byte-pointer flip-flop** (low byte then high byte; cleared by a write
  to the clear-flip-flop register or by master clear). Per controller: command
  (W) / status (R) at offset 8, request (W) at 9, single-mask-bit (W) at 0x0A,
  mode (W) at 0x0B, clear-flip-flop (W) at 0x0C, master-clear (W) / temp (R) at
  0x0D, clear-mask (W) at 0x0E, all-mask (W) at 0x0F. Channel 4 (DMA-2 ch 0) is
  the cascade for DMA-1 and is masked/unusable for transfers.
- **DMA page registers (74LS612 / chipset, ports `0x80-0x8F`).** Latch A16-A23 of
  the 24-bit transfer address; the channel→port map is non-linear:
  ch0=`0x87`, ch1=`0x83`, ch2=`0x81`, ch3=`0x82`, ch5=`0x8B`, ch6=`0x89`,
  ch7=`0x8A`, refresh=`0x8F`; `0x80/0x84/0x85/0x86/0x88/0x8C-0x8E` are scratch
  (`0x80` is the classic POST diagnostic port). All read back what was written.
- **Changes what we build:** model the register *file* (flip-flop, command/status/
  mask/mode, base+current addr/count, page latches, master clear) so a guest that
  `request_region`s and probes ISA DMA at boot — Linux always does, via
  `reserve_dma_pages`/`dma_init` — sees coherent values instead of open-bus `0xFF`
  (an open-bus DMA window is a cheap VM tell). No transfer *engine* is needed:
  nothing in-tree (no floppy/SB16) drives a DMA channel yet, so this is a faithful
  passive register model, fully unblocked and pure-userspace. The claimed I/O
  (`0x00-0x0F`, `0x80-0x8F`, `0xC0-0xDF`) also belongs in the `SYSR` `_CRS`.

---

## 2026-06-09 — Reference-compiler validation of the synthesized ACPI (iasl/acpica-tools)

**Tooling unblock.** Prior sessions repeatedly deferred AML/ACPI correctness work and
the PIC-mode `_PRT` because no ACPI disassembler was installed; the DSDT had only ever
been checked by hand-written byte-decode tests, never parsed by a real interpreter
(`/dev/kvm` is still absent, so no guest has booted it either). `acpica-tools`
(`iasl` 20230628) **installs cleanly from the distro repo** on this runner, so the
emitted tables can now be round-tripped through the Intel ACPI compiler:
`iasl -d <table>.aml` disassembles and `iasl <table>.dsl` recompiles, with the summary
line tallying errors/warnings. This is the authoritative cross-check the hand-decode
tests only approximated.

- **ACPI 6.x §6.1 (`_HID` vs `_ADR`).** A `Device` is enumerated by *either* `_HID`
  (ACPI namespace) *or* `_ADR` (address on an enumerable parent bus), **not both**
  (iasl warns 3073). Our PCI root `PCI0` carried both; its parent is `\_SB`, not a PCI
  bus, so `_ADR=0` is meaningless and a firmware-description tell. **Changed what we
  build:** dropped `_ADR` from the host bridge (the ISA bridge keeps its real
  `_ADR=0x001F0000`); DSDT now compiles 0 errors / 0 warnings.
- **TCG ACPI Specification — TPM2 table, revision 4.** A revision-4 TPM2 is **not** the
  bare 52-byte revision-3 structure. After Start Method it carries a 12-byte *Start
  Method Specific Parameters* block, then *Log Area Minimum Length* (4) + *Log Area
  Start Address* (8) — **76 bytes** total (iasl's own `-T TPM2` template confirms
  `0x4C`). Emitting rev 4 with only 52 bytes leaves the table truncated mid-structure;
  iasl rejects it outright ("terminates in the middle of a data structure"). **Changed
  what we build:** emit the full 76-byte rev-4 layout (zeroed params/log by default,
  `log_area()` setter for a real TCG log).
- **Methodology, durable.** Added an integration test that round-trips *every* generated
  table through iasl and asserts 0 errors / 0 warnings, **self-skipping when iasl is
  absent** (mirroring the `/dev/kvm` test). Installing `acpica-tools` in CI makes the
  whole ACPI surface a hard gate — this is the regression net for the PkgLength/HID-ADR/
  truncation bug classes that only a real interpreter catches.
- **Unblocks:** the deferred **PIC-mode `_PRT` via PCI Link Devices** (needs If/Else AML
  + `LNKA-D` PNP0C0F devices) can now be authored against a validating compiler rather
  than blind byte emission — the next ACPI increment.

---

## 2026-06-09 (b) — `dmidecode` validates SMBIOS; FACS; link-device PCI routing

Same reference-parser approach, applied to the rest of the firmware-description surface
and finished against the validating tools.

- **`dmidecode --from-dump` (`dmidecode` 3.5) installs from the distro repo** — the DMI
  analog of `iasl`. Run over the synthesized SMBIOS (entry point at offset 0, structure
  table at `0x20`, the `--dump-bin` layout) it found three latent defects the per-field
  unit tests missed: (1) Type 0 BIOS Characteristics Extension Byte 2 **bit 4 = "describes
  a virtual machine"** was set — a direct VM tell; (2) Type 3 (System Enclosure) declared
  `Length` 22 but wrote 21 bytes — the SMBIOS 2.7+ **SKU Number** byte was missing, so the
  parser ate the first string's leading byte and read a `<BAD INDEX>` SKU; (3) Type 4
  (Processor) declared `Length` 48 (SMBIOS 3.0) but wrote 42 — the 16-bit **Core/Enabled/
  Thread Count 2** fields were missing, shifting the whole string table (the processor
  Manufacturer decoded as the CPU brand, Part Number as `<BAD INDEX>`). **Changed what we
  build:** all three fixed; an integration test now round-trips the table through
  `dmidecode` (no `<BAD INDEX>`, no "virtual machine"), self-skipping when absent.
- **FACS (ACPI 6.x §5.2.10).** The FADT's `FIRMWARE_CTRL`/`X_FIRMWARE_CTRL` were zero — no
  FACS was published. Every real PC firmware provides one (firmware waking vector, hardware
  signature, ACPI global lock); a zero pointer is a tell and a gap for the Windows target.
  The FACS uniquely has **no SDT header and no checksum**. **Changed what we build:** emit
  the 64-byte v2 FACS, 64-byte aligned in the XSDT→FADT padding, pointed to by both
  `FIRMWARE_CTRL` and `X_FIRMWARE_CTRL`.
- **PCI interrupt link devices (`PNP0C0F`, the PIIX/ICH pattern).** A `_PRT` with only
  integer sources lacks the `LNKA-D` link devices every real PIIX/ICH platform exposes
  (`_PRS`/`_CRS`/`_STA`/`_DIS`/`_SRS`). **Changed what we build:** the mode-selecting `_PRT`'s
  PIC branch now routes through `\LNK[A-D]` (Source = the link `NameSeg`); the links report
  the firmware-default IRQ in `_CRS`; and `standard_pc_complete` programs the live `PIRQRC[A-D]`
  registers to the same `PIRQ_DEFAULT_IRQS`, so the table, the links, the `PirqRouter`, and the
  bytes a guest reads from config space all agree by construction. All `iasl`-validated.
- **CPUID (`enlil-devices::stealth::cpuid`) — noted, not yet changed.** Hypervisor bit clear
  and `0x40000000` leaves zeroed are correct. Two items want a real-CPU reference before
  touching: leaf `0x1` `EBX` max-addressable-IDs is a fixed constant (doesn't track
  `vcpu_count`), and leaf `0x80000008`'s address-size value vs. its comment look swapped.
  There is no `iasl`/`dmidecode`-style validator for CPUID, so this is a focused future pass.

---

## 2026-06-10 — CPUID out-of-range semantics: the `0x40000000`-zeroing is itself a tell (Phase 0.2 / 5.x stealth)

Verified against a **real CPU reference dump** taken on this runner (`std::arch::x86_64::__cpuid_count`,
compiled with `rustc -O`) plus the Intel SDM / AMD APM and the Intel-vs-AMD distinguisher folklore.

- **Intel: every out-of-range leaf returns the highest *basic* leaf's data — not zeros.** On the
  reference Intel CPU (max basic leaf `0xD`, whose result was `eax=0x000000e7 ebx=0x00000a80
  ecx=0x00000a80 edx=0`), querying leaves `0x20`, `0x100`, `0x1337` (above the basic max, below
  `0x4000_0000`), and `0x8000_0009`…`0xFFFF_FFFF` (above the extended max) **all returned that same
  leaf-`0xD` value**. This is the documented Intel rule (Intel SDM Vol 2A, CPUID: "If a value entered
  for CPUID.EAX is higher than the maximum input value for basic or extended function… the data for
  the highest basic information leaf is returned"). QEMU implements exactly this
  (`target-i386: return highest basic leaf if eax out of range`, lists.gnu.org/archive/html/qemu-devel/2012-12).
- **AMD: out-of-range/undefined leaves return zeros** (AMD APM Vol 3; the standard/extended ranges are
  the only defined ones). So the correct stealth behaviour is **vendor-specific**, and the difference is
  itself a known Intel-vs-AMD probe.
- **The detection vector (websec.net "Ophion: Building a Stealth Intel VT-x Hypervisor"; CPUID
  Wikipedia).** A detector reads an obviously-bogus leaf (e.g. `0x13371337`) and compares it to
  `CPUID(0x4000_0000)`. On bare-metal **Intel** both return the highest-basic-leaf data and are equal;
  most hypervisors answer `0x4000_0000` with a vendor signature (or, like Enlil today, with zeros) while
  the bogus leaf returns *something else* — the mismatch (or the all-zeros, which no real Intel CPU ever
  returns for an out-of-range leaf) is the tell.
- **Changed what we build:** `CpuidStealthTable::lookup` was returning `CpuidResult::default()` (zeros)
  for the `0x4000_0000-0x4000_00FF` region *and* for every leaf outside the populated ranges. Replace
  that with **faithful out-of-range emulation**: precompute an `out_of_range` result at build time
  (Intel → the highest populated basic leaf's data; AMD → zeros) and return it for any leaf above the
  advertised basic max (including the hypervisor region while hiding) and above the extended max — so the
  hypervisor leaves are *indistinguishable from bare metal* on Intel and correctly zero on AMD. In-range
  but unpopulated leaves keep returning zeros (real CPUs do that for reserved leaves). No CPUID validator
  exists, so this is reference-dump-backed, not tool-validated.

---

## 2026-06-10 (b) — Reprogrammable PIRQ links, APERF/MPERF & LBR stealth, TPM SHA-256 (primary specs)

The rest of this session built against primary specs rather than new papers — logged here
so the next run sees the authoritative sources without re-deriving them.

- **Reprogrammable PCI interrupt links (ACPI 6.x §6.2.13 `_PRT`, PIIX3 datasheet PIRQRC).**
  A faithful `LNKA-D` (`PNP0C0F`) link device exposes its routing through an
  `OperationRegion(PCI_Config)` + `Field` over the PIIX3 config 0x60-0x63 (PIRQRC[A-D]),
  with `_CRS`/`_DIS`/`_SRS` reading and rewriting it (`PIRx & 0x0F` = IRQ, bit 7 = disable).
  **AML name-resolution pitfall (validated with iasl):** a `_PRT` is a Method, so a *relative*
  multi-seg `Source` path resolves under `…._PRT` (multi-seg names get no upward search) and
  fails — the link `Source` must be a **root-anchored** path (`\_SB.PCI0.ISA_.LNKx`), and the
  referenced device must be defined *before* the `_PRT` or a disassembler emits `External`
  and the round-trip breaks.
- **APERF/MPERF (Intel SDM Vol 3, IA32_APERF 0xE8 / IA32_MPERF 0xE7).** MPERF counts at the
  nominal/TSC rate; APERF at the core frequency, so the APERF/MPERF ratio is the
  frequency/utilization signal an IET divergence detector inspects. To hide a VMEXIT both
  shadow counters must be decremented and the ratio preserved (decrement MPERF by the TSC
  overhead, APERF by `ratio * overhead`).
- **LBR sanitization (Intel SDM, LBR MSRs 0x680-0x6CF; DebugCtl 0x1D9).** After a CPUID-forced
  VMEXIT the top LBR entry's **TO** holds the hypervisor entry — TO (not FROM) is the field a
  detector reads, so sanitization must overwrite TO (set FROM=TO=guest RIP → reads as a
  non-branch).
- **TPM 2.0 (TCG spec Part 1 §17.2 PCR_Extend, Part 2 command codes; FIPS 180-4 SHA-256).**
  `TPM2_PCR_Extend` = `SHA256(pcr_old || digest)` (needs a real hash, not a placeholder).
  Authoritative command codes: `PCR_Extend=0x0000_0182`, `PCR_Read=0x0000_017E`,
  `GetCapability=0x0000_017A`. `GetRandom` must vary across calls. No new dependency was
  added — SHA-256 is implemented in-tree (no_std-friendly), verified against FIPS vectors.

---

## 2026-06-10 (c) — vPMU as a transparency surface: distinct fixed-counter rates + CPUID leaf 0xA

Web research for the PMC-model increment, plus primary-source layout verification.

- **KVM passthrough vPMU (LWN, "KVM: x86/pmu: Introduce passthrough vPMU",
  <https://lwn.net/Articles/959653/>).** KVM's two vPMU models: trap-and-emulate (our shadow
  model — every PMC MSR access exits) vs. passthrough (guest owns the real GP counters and
  "some of the fixed counters"). Confirms the fixed counters are a first-class guest-visible
  surface; our trap path must therefore return *plausible* values, not placeholders.
- **IET divergence detection reads APERF (secret.club, "BattlEye hypervisor detection",
  <https://secret.club/2020/01/12/battleye-hypervisor-detection.html>); the rdtsc;cpuid;rdtsc
  timing attack is standard in BattlEye/EAC (VIC, arXiv:2502.12322,
  <https://arxiv.org/abs/2502.12322>).** Consequence for what we build: a detector can
  cross-check RDPMC's CPU_CLK_UNHALTED.THREAD/REF_TSC against APERF/MPERF — the two surfaces
  must encode the *same* core/ref ratio, and IPC ≡ 1.0 / core ≡ ref (all counters advancing
  by the same delta) is the RDPMC version of the APERF/MPERF-no-op tell. Drove the
  `PmcRateModel` (ref rate / core = 1.15×ref / instr = 1.31×core / slots = 4×core), with the
  seeding requirement documented on the type.
- **CPUID leaf 0xA layout (Intel SDM Vol 2A; cross-checked against Linux
  `arch/x86/include/asm/perf_event.h` `union cpuid10_{eax,ebx,edx}`).** EAX: version[7:0],
  GP-counter count[15:8], GP width[23:16], event-vector length[31:24]; EBX: 7
  event-unavailable bits; ECX (v5): supported-fixed-counter bitmask; EDX: fixed count[4:0],
  fixed width[12:5], AnyThread-deprecated[15]. **Tell found:** our table left 0xA unpopulated
  → all-zeros → "PMU version 0", which only vPMU-less VMs report (this runner's own cloud
  guest CPUID returns exactly that) and which contradicts the PMC shadow servicing RDPMC.
  Fixed: Intel tables advertise version 5 matching `stealth::pmc`'s counter counts.

---

## 2026-06-10 (d) — CPUID consistency sweep: the kernel parsers as layout oracles

The session's later CPUID work (leaves 0x2/0x4/0x5/0x6/0xD/0x15/0x16, 0x80000005-7) was
verified against the Linux kernel's own parsers — useful as free, precise "what does a real
OS read" oracles when the SDM/APM PDFs are paywalled/blocked from this runner:

- `arch/x86/kernel/cpu/scattered.c`: `X86_FEATURE_APERFMPERF` ← leaf 0x6 **ECX[0]**, both
  vendors (AMD calls it the effective-frequency interface). If we serve APERF/MPERF, leaf 6
  must advertise them; turbo (Intel IDA, EAX[1]) must back any max>base frequency claim.
- `arch/x86/include/asm/perf_event.h` `union cpuid10_*`: leaf 0xA layout (used 2026-06-10 (c)).
- `arch/x86/kernel/cpu/cacheinfo.c` `union l1_cache/l2_cache/l3_cache` + `assocs[]`: the
  legacy AMD Fn8000_0005/6 field layouts and the L2/L3 associativity *encoding* table
  (4→4-way, 6→8-way, 8→16-way, L3 size = size_encoded × 512 KiB).
- **Pattern worth keeping:** every populated leaf must be cross-checkable against every other
  surface that encodes the same fact (leaf 4 L2 ↔ 0x80000006 L2; leaf 0x16 turbo ↔ leaf 6 IDA
  ↔ PMC core/ref ratio; leaf 0xD subleaf 0 size ↔ subleaf 2 offset+size; leaf 1 MONITOR ↔
  leaf 5). The tests now pin each of these pairs.

---

## 2026-06-10 (e) — Heterogeneous-ISA pools: the execution-mode model (design discussion w/ owner)

Design session on what a mixed x86/ARM/RISC-V pool presents to a guest. Outcome folded into
ROADMAP.md → Core Model → "Execution modes" + Phase 11 intro. Prior art that shaped it:

- **TidalScale / ScaleMP (software-defined SMP):** single unmodified OS over multiple x86
  machines via page-granular software DSM + migrating vCPUs/pages. Proves single-kernel-over-
  N-nodes is possible and that page-fault-granularity coherence is the bottleneck (acceptable
  only for partitionable working sets). Same-ISA only. → mesh-mode baseline + its ceiling.
- **Rosetta 2 / FEX-Emu vs QEMU TCG:** the x86-on-ARM gap (~1.3–2× vs 5–20×) is mostly the
  memory model — x86 guests assume TSO; Apple ships hardware TSO mode, generic ARM needs
  per-access fencing. Translation direction matters (ARM-guest-on-x86 gets TSO ≥ weak for
  free). Rosetta is openly detectable (sysctl) → translation = compatibility, not stealth.
- **QEMU's KVM↔TCG state model:** native and translated execution share one architectural
  vCPU state definition, so native↔JIT handoff at an instruction boundary is a pause +
  register/FPU capture — the mechanism behind the runtime mode-switch requirement.
- **JIT-instrumented DSM (mesh-mode thesis):** a translator observes every load/store, so
  coherence can be word/object-granular with access-stream-driven co-scheduling — translation
  doesn't remove distance, it instruments it. This is the Phase 11 research track.

---

## 2026-06-11 — q35/ICH9 chipset identity: primary-source register map for the conversion (Phase 0.2 / 5.x stealth)

The platform's surfaces currently mix generations: PCIe ECAM/MCFG + an i440FX host bridge
(`8086:1237`, a 1996 chipset with **no** ECAM) + the PIIX3 ISA bridge at `00:01.0`. Any guest
(or detector) that cross-references the host bridge ID against the MCFG table sees an
impossible machine. Primary sources for the q35 conversion:

- **Intel 3 Series Express Chipset Family datasheet (Q35 MCH)** — host bridge `D0:F0` is
  `8086:29C0`; **PCIEXBAR** lives at MCH config `0x60-0x67` (bit 0 = enable, bits 2:1 = window
  size 00b=256 MiB, base bits 38:28). The MCH's own PCIEXBAR must agree with the MCFG table —
  one more "same fact, two surfaces" pair to pin. OVMF programs PCIEXBAR=`0xB0000000` on q35
  (edk2 `OvmfPkg/PlatformPei` MMCONFIG patch), which is exactly our `DEFAULT_ECAM_BASE`.
- **Intel ICH9 Family datasheet (316972)** — LPC interface bridge is `D31:F0` (`00:1F.0`),
  `8086:2918`, class `06 01`; **PIRQ[A-D]_ROUT at config `0x60-0x63` with byte semantics
  identical to PIIX3** (bit 7 = routing disable, reset `0x80`, bits 3:0 = ISA IRQ, IRQ
  0/1/2/8/13 reserved), plus **PIRQ[E-H]_ROUT at `0x68-0x6B`** (new vs PIIX3); PIRQA-H wire to
  I/O APIC inputs 16-23 in APIC mode. ELCR stays at `0x4D0/0x4D1`, RST_CNT at `0xCF9`.
- **QEMU `hw/pci-host/q35.c` / `hw/isa/lpc_ich9.c`** — confirms a production VMM models q35
  exactly this way (MCH `29C0` + ICH9 LPC `2918` at 1F.0, PIRQ regs 0x60/0x68); QEMU's tell is
  its `1af4:1100` *subsystem* IDs, which we leave unset for now (follow-up: source subsystem
  IDs from the SMBIOS board vendor across all functions — same-fact pair).

**How it changes what we build:** the existing `PirqRouter` (registers, semantics, swizzle,
GSI 16-19) carries over to ICH9 unchanged for A-D — the conversion is an identity/BDF pass
(host bridge ID, LPC at `00:1F.0`, DSDT `_ADR`, PCIEXBAR seeding), not an interrupt-model
rewrite. PIRQ E-H and the multifunction `00:1F.x` siblings a real ICH9 always has (SATA
`1F.2`, SMBus `1F.3` = `8086:2930`) are follow-up fidelity items, flagged in the roadmap.

Sources: https://www.intel.com/content/dam/doc/datasheet/io-controller-hub-9-datasheet.pdf ·
Intel 3-Series (Q35) chipset datasheet · https://github.com/qemu/qemu/blob/master/hw/pci-host/q35.c ·
https://mail-archive.com/edk2-devel@lists.01.org/msg08739.html

---

## 2026-06-11 (b) — AMD topology surface: TOPOEXT leaves, vendor-correct max leaves (Phase 5.4)

Continuation of the 2026-06-10 (d) "kernel parsers as layout oracles" sweep, AMD side:

- **AMD APM Vol 3 `Fn8000_001D`/`Fn8000_001E`** — the TOPOEXT cache-topology and extended
  APIC/core/node leaves. `Fn8000_001D` mirrors Intel leaf 4's field layout *except* EAX[31:26]
  (cores-per-package) which is reserved on AMD. Gated by `Fn8000_0001` ECX[22] (TOPOEXT) —
  Linux `cpu/cacheinfo.c` only parses 0x8000001D when the bit is set, and `cpu/topology_amd.c`
  parses 0x8000001E EBX[15:8]+1 as threads-per-core.
- **Max-leaf values are themselves a vendor fingerprint:** no AMD part reports basic max 0x16
  (that's Intel's frequency leaf; Zen reports 0x10) and AMD extended max runs to 0x8000001F+
  (SEV leaf). We now emit per-vendor maxes; 0x8000001F is deliberate in-range zeros (no
  SME/SEV claimed anywhere — internally consistent, though a real 7950X does advertise SME;
  noted as acceptable divergence until SEV is a feature we can virtualize).
- **`Fn8000_0007` EDX[9] CPB + EDX[10] EffFreq** — AMD's boost + read-only APERF/MPERF bits;
  pairs with the PMC rate model's core>ref ratio (Intel signals the same facts via leaf 6 IDA
  + leaf 6 ECX[0]). **`Fn8000_0008` ECX[7:0] NC / ECX[15:12] ApicIdSize** — the legacy
  topology source `kernel/cpu/topology.c` cross-checks against leaf 1 EBX and leaf 0xB.

**How it changes what we build:** every populated AMD leaf is now cross-checkable against the
surface that encodes the same fact (1D geometry ↔ legacy 5/6; 1E SMT ↔ leaf-1 HTT/0xB; NC ↔
vcpu count; CPB ↔ PMC ratios) — tests pin each pair, same discipline as the Intel sweep.

---

## 2026-06-11 — Chipset identity consistency: i440FX/PIIX3 vs Q35/ICH9 as a VM tell (Phase 0.2 / 5.x stealth)

The transparency question for this increment: does the emulated chipset's *identity* match the
*features* we expose? Primary sources, since chipset register maps are stable:

- **Intel ICH9 Family Datasheet (316972-004), §13 LPC (D31:F0).** The PIRQ routing registers
  `PIRQ[A-D]_ROUT` are at config `0x60`-`0x63` and `PIRQ[E-H]_ROUT` at `0x68`-`0x6B`; bit 7 is
  IRQEN (1 = *not* routed to the 8259), bits[3:0] select the ISA IRQ — **byte-identical** to the
  PIIX3 `PIRQRC[A-D]` layout we already model. So switching the south-bridge identity from PIIX3
  (`8086:7000`, `00:01.0`) to ICH9 LPC (`8086:2918`, `00:1F.0`) needs **no** change to the
  `PirqRouter` or the DSDT link devices — only the device ID and BDF move. → done this run.
- **QEMU machine-type taxonomy (`pc` i440FX vs `q35`).** i440FX is the pre-PCIe northbridge
  (host bridge `8086:1237`, no MCFG/ECAM); Q35's MCH (`8086:29C0`) is the PCI-Express generation
  and is what ships an ECAM window + `PNP0A08` root. Enlil already emits MCFG/ECAM and a
  `PNP0A08` DSDT root, so the i440FX host-bridge ID was internally contradictory — a guest that
  reads MCFG then the `00:00.0` device ID sees a chipset that can't have ECAM. → host bridge now
  reports the Q35 MCH; the MCFG/PNP0A08/host-bridge-ID triple is consistent.
- **Detection practice (passthrough-hardening guides, 2024-25).** Community anti-detection setups
  standardise on `q35` precisely because the i440FX identity is a known emulator fingerprint;
  this corroborates that the host-bridge/chipset ID is a real, cheaply-probed surface, not a
  theoretical one. → confirms priority of this fix over cosmetic stealth.
- **Follow-up the datasheet implies:** ICH9 1F.0 is multifunction (1F.2 SATA AHCI `8086:2922`,
  1F.3 SMBus `8086:2930`). We model only 1F.0; the absent siblings read all-ones (benign), but a
  complete south bridge would add them with the header-type multifunction bit set.

---

## 2026-06-13 — xHCI Device Context output write-back + the remaining slot/endpoint commands (Phase 4.4)

Targeted spec check before completing the guest-memory device-context loop (the broad
RustVMM/ACRN architecture case is already logged above; nothing new in the literature
changed the plan — this is grounded in the xHCI 1.2 specification, the authoritative
primary source for command semantics):

- **The Output Device Context is the controller→driver channel** (xHCI 1.2 §4.6.5,
  §6.2.1): after Address Device / Configure Endpoint the xHC *copies* the input contexts
  into the Output Device Context that `DCBAA[slot_id]` names, updating the controller-owned
  fields — **Slot State**, **USB Device Address**, and per-endpoint **EP State** — which the
  driver reads back to confirm the command. We had been reading input contexts but never
  writing outputs, so a real driver would never see its device addressed. → added a
  `SlotContext`/`EpState` codec, `DCBAA` dereferencing, and output write-back on every
  slot/endpoint command.
- **DCBAA layout** (§6.1): `DCBAAP` points at an array of 64-bit, 64-byte-aligned device
  context pointers; **entry 0 is the Scratchpad Buffer Array, not a slot** — slot N's
  context is at `DCBAAP + N*8`. Output (device) contexts have **no** Input Control Context
  prefix (unlike input contexts), so DCI N sits at `+N*0x20`. → `device_context_pointer` /
  `device_context_entry_offset`.
- **Slot/EP state machines** (Tables 6-4, 6-8): Slot States Disabled(0)/Default(1)/
  Addressed(2)/Configured(3); EP States Disabled(0)/Running(1)/Halted(2)/Stopped(3)/
  Error(4). Address Device→Addressed (or **Default** when **BSR**=1, control bit 9, §4.6.5,
  which Linux issues first to read the descriptor at address 0); Configure Endpoint→
  Configured; Deconfigure/Reset Device→Default; Disable Slot→Disabled; STALL→Halted, Reset
  Endpoint→Running; Stop Endpoint→Stopped.
- **Evaluate Context** (§4.6.7) re-evaluates only EP0 Max Packet Size + slot Max Exit
  Latency/Interrupter Target *without* changing state — issued mid-enumeration once the
  driver reads the real EP0 max packet size. **Set TR Dequeue Pointer** (§4.6.10) repoints a
  Stopped/Halted ring (DCS in parameter bit 0, pointer in bits 63:4) and is the second half
  of STALL recovery after Reset Endpoint; on a non-stopped/unconfigured endpoint it returns
  **Context State Error** (code 19). → both commands were undecoded (→ TRB error, stalling a
  real driver) and are now handled.

## 2026-06-12 — xHCI transfer-ring (TD) processing: ACRN's TD assembly + the spec's event-length semantics (Phase 4.4)

Targeted check before building the TD-processing increment (the broad ACRN-architecture
case was already logged under "USB Passthrough — ACRN's Architecture as Reference"):

- **ACRN `devicemodel/hw/pci/xhci.c` TD assembly** (https://github.com/projectacrn/acrn-hypervisor/blob/master/devicemodel/hw/pci/xhci.c):
  a TD is gathered TRB-by-TRB using the control-field **chain bit** — chained TRBs are
  appended as `USB_DATA_PART`, the first chain-clear TRB closes the TD (`USB_DATA_FULL`)
  and the whole buffer is handed to the USB core. Control transfers arrive as **three
  separate TDs** (Setup / Data / Status stages), so the device model keeps per-endpoint
  control state across TDs rather than expecting one chained mega-TD. → our
  `process_transfer_ring` mirrors this: chain-bit TD gathering + a per-EP0 control-stage
  state machine.
- **Transfer Event length field is 24-bit residual, not 17-bit TRB length** (Linux fix
  `usb: xhci: Fix TRB transfer length macro used for Event TRB`,
  https://lkml.iu.edu/hypermail/linux/kernel/1303.2/02331.html): event TRBs carry
  *untransferred* bytes in status[23:0] (`EVENT_TRB_LEN`), while transfer TRBs carry the
  *requested* length in status[16:0] (`TRB_LEN`) — drivers compute
  `transferred = requested - residual`. Posting a wrong residual silently corrupts every
  guest driver's length accounting. → our events post `requested - transferred` and the
  tests pin it.
- **Setup Stage carries the 8-byte packet as immediate data** (xHCI 1.2 §6.4.1.2.1): IDT
  set, parameter field = the raw `bmRequestType/bRequest/wValue/wIndex/wLength` packet,
  TRT in control[17:16] (0 = no data, 2 = OUT data, 3 = IN data). No guest-memory read is
  needed for the setup packet itself — only Data/Normal TRBs dereference guest buffers.
- **Endpoint addressing is by DCI** (xHCI §4.5.1): DCI = `ep_num * 2 + direction`
  (IN = 1), EP0 = DCI 1 — the doorbell's target field and the Transfer Event's
  endpoint_id are both DCIs. Direction is therefore derivable from DCI parity for
  bulk/interrupt rings (odd = IN), which the dispatcher uses.
- **Doorbell-deferred servicing matches the eventual KVM shape:** in ACRN/QEMU the
  device-slot doorbell write is the VM exit and ring processing happens with guest memory
  in hand. → device-slot doorbells latch pending in the `DoorbellArray` and a
  `service_doorbells(&mut dyn DmaMemory)` entry point drains them — exactly the call the
  KVM run loop will make; tests drive it with a Vec-backed memory.

## 2026-06-14 — KVM run loop: in-kernel IRQ chip vs HLT, memory-region alignment, real-mode device exits (Phase 0.2 / 4 / 5)

First run with `/dev/kvm` actually available end-to-end, so the previously
self-skipping guest-boot path ran for real and surfaced three KVM API
behaviours that shape the run loop. Primary source: the Linux KVM API
reference (`Documentation/virt/kvm/api.rst`), corroborated empirically by
bisecting a direct `kvm-ioctls` probe on this host (AMD SVM, nested virt).

- **`KVM_CREATE_IRQCHIP` changes `HLT` semantics.** With the in-kernel local
  APIC present, `HLT` is handled inside KVM — the vCPU halts waiting for an
  interrupt and `KVM_RUN` does **not** return `KVM_EXIT_HLT`. Without the IRQ
  chip, `HLT` exits to userspace. → the run loop can't treat "vCPU reached
  HLT" as an exit when the production IRQ chip is enabled; a guest that idles
  via `HLT` will block in `KVM_RUN` until an interrupt or a userspace kick
  (`KVM_SET_SIGNAL_MASK`/`immediate_exit`). We split the backend into `new()`
  (IRQ chip, production) and `new_without_irqchip()` (HLT exits to userspace,
  for self-contained code blobs / userspace-driven IRQs). A future increment
  should add an `immediate_exit`/signal-based watchdog so the production path
  can bound a non-progressing or idle vCPU.
- **`KVM_SET_USER_MEMORY_REGION` requires a page-aligned `userspace_addr`**
  (and page-aligned `guest_phys_addr`/`memory_size`); a plain `Vec<u8>` is
  only byte-aligned and the ioctl rejects it with `EINVAL`. → guest RAM must
  come from a page-aligned allocation (`GuestRam`), and a pre-ioctl
  `validate_region` guard turns the opaque `EINVAL` into an actionable error.
- **KVM emulates real-mode MMIO instructions** (e.g. `moffs` `mov`), so a
  16-bit guest blob can drive an MMIO device below 1 MiB and the access exits
  to userspace as `KVM_EXIT_MMIO`. → real-mode smoke tests can exercise the
  full MMIO device path (not just PIO) without entering protected mode, which
  is how the xHCI doorbell/ring DMA path is now tested end-to-end.

## 2026-06-14 (b) — xHCI transfer rings resident in guest memory (Phase 4)

Source: xHCI 1.2 §4.9 (Transfer Rings) and §6.2.3 (Endpoint Context — the TR
Dequeue Pointer + Dequeue Cycle State field), cross-checked against ACRN's
doorbell-deferred ring processing (already logged 2026-06-02).

- A guest's transfer ring lives entirely in guest memory; the controller learns
  its start address and initial cycle state from the **endpoint context's TR
  Dequeue Pointer** (set by Address Device for EP0 / Configure Endpoint for
  other endpoints, repositioned by Set TR Dequeue Pointer). → our controller
  now persists a per-`(slot, dci)` `GuestRingCursor` built from that field and
  advances it across doorbells, instead of an internal `submit_transfer` queue.
- **Set TR Dequeue Pointer applies to any declared endpoint, not only ones with
  an in-process ring.** EP0 has a context but (in our model) no internal ring,
  so the command must work off the endpoint context — which is also the natural
  home for the dequeue pointer once rings are guest-resident. → repointing now
  succeeds for context-only endpoints and updates the context (the cursor's
  source of truth); it remains a Context State Error only for wholly unknown
  endpoints.

## 2026-06-15 — Userspace MSR exits + the stealth MSR/CPUID seam (Phase 5)

Primary specs consulted to ground the MSR-exit / timing-stealth wiring built
this session (no new third-party research — these are the authoritative refs):

- **KVM API — `KVM_CAP_X86_USER_SPACE_MSR` / `KVM_X86_SET_MSR_FILTER`**
  (`Documentation/virt/kvm/api.rst`). Enabling the cap with the *unknown* /
  *filter* reason bits makes KVM forward guest `RDMSR`/`WRMSR` it does not
  emulate to userspace as `KVM_EXIT_X86_RDMSR`/`KVM_EXIT_X86_WRMSR`
  (kvm-ioctls `VcpuExit::X86Rdmsr`/`X86Wrmsr`, carrying `index`, a writable
  `data`, and an `error` byte that injects `#GP` when set). → modelled as
  `GuestExit::MsrRead`/`MsrWrite` + `VmExitHandler::rdmsr`/`wrmsr`, the seam the
  stealth shadows plug into. **Caveat that shapes the next step:** APERF/MPERF
  and the PMC MSRs are *known* to KVM, so forwarding *those* needs an actual
  `KVM_X86_SET_MSR_FILTER` bitmap, not just the cap — the unknown-reason path
  only reaches truly unmodelled MSRs. `enable_userspace_msr_exits` already
  requests the *filter* reason; installing the filter bitmap is the missing
  piece, and `kvm-ioctls` 0.19 has no wrapper for it (raw ioctl needed).

- **Intel SDM Vol. 3B / AMD APM — `IA32_APERF` (0xE8) & `IA32_MPERF` (0xE7).**
  APERF counts actual core-frequency cycles, MPERF the nominal/TSC rate; their
  *ratio* is the effective-frequency signal an IET divergence detector reads,
  and it must equal RDPMC's `CPU_CLK_UNHALTED.THREAD / REF_TSC`. → the
  `StealthMsrRouter` serves APERF/MPERF (from `VcpuTimingState`) and the PMC
  MSRs (from `PmcState`) from the *same* `PmcRateModel`, and
  `install_stealth_msr_router` seeds the shadows so the first guest read shows
  the non-unity model ratio, never the all-zero 1.0 identity (itself a tell).

- **AMD APM Vol. 2 — LBR Virtualization (LBRV).** AMD's basic LBRV exposes a
  *single* last-branch pair (`LastBranchFromIP` 0x1DB / `ToIP` 0x1DC) plus a
  last-interrupt pair (0x1DD/0x1DE), not Intel's 32-entry MSR stack; on
  `#VMEXIT`, `LastBranchToIP` is left pointing into the hypervisor. → on this
  AMD SVM host `LbrState` now models those four registers and
  `sanitize_after_exit` branches on platform: AMD erases the branch pair (to
  `guest_rip`), leaving the last-interrupt pair alone (a VMEXIT is not a guest
  IRQ); the Intel 32-entry stack path is unchanged. The router routes the four
  AMD LBR MSRs alongside the Intel FROM/TO/INFO blocks.

- **CPUID hypervisor-present bit (leaf 1 ECX[31]).** No bare-metal CPU sets it;
  the most-checked VM tell. → `clear_cpuid_hypervisor_bit` starts from
  `KVM_GET_SUPPORTED_CPUID`, clears the bit, and `KVM_SET_CPUID2`s it onto each
  vCPU. **Measured this run:** a bare KVM VM (no `SET_CPUID2`) does *not* set
  the bit by default, so the call's present value is installing the real host
  feature set *with the bit guaranteed clear* (proven: a guest reads ECX[31]=0
  and EDX[4]/TSC=1) — the 1→0 flip only matters once paravirt CPUID signature
  leaves are added. The supported set also omits the 0x4000_00xx hypervisor
  leaves, so they read out-of-range like bare metal for free.

## 2026-06-16 — Production run loop + applying topology stealth to the live guest (Phase 5)

Integration session; the primary refs were the KVM API and the in-tree stealth
state already grounded in prior entries. Two measured facts shaped the code:

- **`on_vmresume` had a first-entry zeroing bug.** `VcpuTimingState::on_vmresume`
  computes the exit overhead to hide as `entry_tsc - last_exit_tsc`. On the very
  first guest entry no `on_vmexit` has run, so `last_exit_tsc` is still its init
  value `0` — indistinguishable from a *real* exit at TSC 0 (which the unit tests
  legitimately use). The overhead then becomes the full `entry_tsc` (a multi-GHz
  absolute count), saturating both shadows to zero before the guest's first read.
  → Added an `exit_seen` flag set by `on_vmexit`, distinct from the timestamp; the
  first `on_vmresume` records only the RIP and returns. This is the seam the timed
  run loop (`run_vcpu_timed`) depends on for any seeded value to survive.

- **AMD KVM omits the Intel-style extended-topology leaf `0xB` from
  `KVM_GET_SUPPORTED_CPUID`.** Measured on this AMD SVM host: a 2-vCPU guest read
  `cpuid(0xB,1)` as all-zero after `SET_CPUID2` of the supported set, because no
  `0xB` entry existed to carry the topology. → `apply_topology_stealth` must
  *rebuild* the CPUID array via `CpuId::from_entries`, adding the `0xB` subleaves
  (flagged `KVM_CPUID_FLAG_SIGNIFCANT_INDEX`, value 1, kvm-bindings 0.10) when
  absent, not just patch entries in place. With the table built for the guest's
  topology, the guest then reads `EBX == 2` (its own vCPU count), not the host's
  much larger logical-processor count — the topology half of the CPUID-stealth
  table merge. `kvm_bindings::CpuId` is a `FamStructWrapper<kvm_cpuid2>`;
  `from_entries(&[kvm_cpuid_entry2])` is the supported rebuild path.

- **Production run loop assembled.** `StealthRunLoop` (`enlil-core::run_loop`)
  encapsulates the install-order the API requires (router on the bus, then
  `enable_userspace_msr_exits` + `forward_msrs_to_userspace` before any
  `KVM_RUN`) and the per-entry lockstep (timing advanced once inside
  `run_vcpu_timed` via the shared `Arc`; the non-shared PMC advanced once in the
  loop from the returned delta — advancing both via `StealthMsrRouter::advance`
  would double-count the timing surface).

---

## 2026-06-18 — What KVM's `GET_SUPPORTED_CPUID` mirrors vs. defaults, leaf by leaf (Phase 5)

No new third-party sources — this entry records facts **measured directly on this
AMD Ryzen 9 7950X3D host's `/dev/kvm`** while extending `apply_topology_stealth`
to a full live-guest CPUID-stealth pass. They matter because the fix for each
leaf depends on whether KVM *mirrors the host* (must override) or *defaults to a
neutral value* (override only when the guest wants non-default), and getting that
wrong yields either a vacuous patch or a missed tell:

- **Leaf `0xA` (architectural PMU): KVM omits it entirely on AMD.** A guest reads
  it all-zero = "PMU version 0", which only vPMU-less cloud VMs report and which
  contradicts the `stealth::pmc` RDPMC shadow. → must *insert* the table's leaf
  `0xA` (PMU v5, GP/fixed counts matching the shadow). The `CpuId::from_entries`
  rebuild already handles a KVM-absent leaf. Verified: Intel-presented guest then
  reads `cpuid(0xA).EAX[7:0] == 5`.
- **Leaf `0x8000_0008` `ECX[7:0]` (NC, core count): KVM mirrors the host.** A
  2-vCPU guest read the host's ~11, contradicting the leaf-1 EBX / leaf-0xB count
  the topology pass already fixed (the kernel cross-checks all three). → patch
  ECX from the table (NC + ApicIdSize), leaving the host-real `EAX` address
  widths. Verified: guest reads `NC == 1`.
- **Leaf `0x8000_001D` `EAX[25:14]` (per-cache NumSharingCache): KVM mirrors the
  host.** A 2-vCPU guest read its L3 as shared by every host thread. → rewrite
  only the sharing sub-field per cache (L1/L2 per core, L3 package-wide), matched
  to KVM's subleaves by index + cache type/level so host cache *sizes* stay. The
  sharing field is `(shared-1) << 14`, mask `0x03FF_C000`. Verified: guest L3
  shared by 2.
- **Leaf `0x8000_001E` `EBX[15:8]` (ThreadsPerComputeUnit-1, SMT width): KVM
  defaults to 0** regardless of vCPU count — it does *not* infer SMT from the
  number of vCPUs (measured via a no-stealth baseline guest). So this is the
  inverse case: vacuous for an SMT-1 guest, but for an SMT-2 guest the table's
  `1` must be installed or the SMT field contradicts the leaf-0xB SMT level and
  leaf-1 HTT bit. → patch only `EBX[15:8]` (mask `0x0000_FF00`), leaving the
  per-vCPU APIC/core-id fields KVM fills. Verified: SMT-2 guest reads `1`.

**General rule for live-guest CPUID stealth:** patch the *topology sub-field*
of a host-mirrored leaf and leave the hardware-backed remainder (sizes, address
widths, per-vCPU IDs) to KVM; insert whole leaves only when KVM omits them and
the value is fully synthetic (leaf `0xB`, leaf `0xA`). The AMD topology surface a
guest can cross-check is the set {leaf-1 EBX[23:16], leaf 0xB, `0x8000_0008` NC,
`0x8000_001D` sharing, `0x8000_001E` SMT} — they must all agree, and as of this
run `apply_topology_stealth` makes them agree in one rebuild.

## 2026-06-19 — AMD PMC MSR surface: legacy vs PerfMonV2 register map (Phase 5.4)

Measured on this host (`cpuid` leaf `0x8000_0000` → `max_ext = 0x8000_0021`, leaf
`0x8000_0022` = all-zeros): **this nested WSL host does NOT advertise AMD
PerfMonV2** (it is itself a guest under Hyper-V, which does not pass leaf
`0x8000_0022` through). So the CPUID leaf-`0x8000_0022` advertise + "KVM mirrors
host PerfMonV2" path the 2026-06-18 next-step proposed is **untestable on this
runner** and would advertise a feature the apparent host lacks — a tell, not a
fix. The genuinely testable, host-agnostic prerequisite is the AMD **PMC MSR**
surface, which the `StealthMsrRouter`/`PmcState` did **not** yet cover (Intel
`IA32_PMC0`/`PERFEVTSEL`/`FIXED_CTR` only):

- **AMD legacy (K7) PMCs** — `PerfEvtSel0..3` = `0xC001_0000..0xC001_0003`,
  `PerfCtr0..3` = `0xC001_0004..0xC001_0007` (4 counters). On real AMD parts the
  legacy block **aliases** the first four core counters (AMD APM vol. 2 §13.2).
- **AMD core / PerfMonV2 PMCs** — interleaved EvtSel/Ctr pairs:
  `PerfEvtSel[n] = 0xC001_0200 + 2n` (even), `PerfCtr[n] = 0xC001_0201 + 2n`
  (odd), n = 0..5 (6 counters on Zen).
- **PerfMonV2 global block** — `PerfCntrGlobalStatus = 0xC000_0300`,
  `PerfCntrGlobalCtl = 0xC000_0301`, `PerfCntrGlobalStatusClr = 0xC000_0302`
  (AMD PPR Family 19h). Global-ctl bit n enables core counter n, mirroring the
  role Intel `IA32_PERF_GLOBAL_CTRL` plays for the existing advance() gating.

Change made: `PmcState` now maps both AMD blocks (legacy + core) and the
PerfMonV2 global registers onto the existing shadow arrays — legacy n and core n
share index n, exactly the hardware aliasing — so RDMSR/WRMSR/RDPMC on an
AMD-presented guest read the model-driven shadow (hiding VMEXIT overhead) instead
of falling through to KVM. `StealthMsrRouter::filter_ranges` is now
platform-correct: it forwards the AMD PMC ranges on `AmdSvm` and the Intel PMC
ranges on `IntelVmx`, never the other platform's (forwarding the wrong vendor's
PMC MSRs would make non-existent registers readable — the same tell the LBR fork
already avoids). The synthetic leaf-`0x8000_0022` advertise is left for a host
that actually exposes PerfMonV2.

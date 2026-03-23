# Enlil ZK Research: Lessons from zkEVM Performance Engineering

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

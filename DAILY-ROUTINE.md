# Enlil — Daily Autonomous Development Routine

> This file is the single source of truth for the daily routine. Edit it to change
> what the routine does; the loop just says "follow this file."

**REPO:** `github.com/physics515/enlil` (branch `master`). Rust workspace, bare-metal
Type-1 hypervisor. Custom target `x86_64-unknown-enlil.json`; toolchain pinned in
`rust-toolchain.toml`.

**ENVIRONMENT:** This runs unattended on a cloud Linux runner with the repo checked out
(daily cron). Always `cargo build`, `cargo test`, and `cargo clippy`. If KVM / nested
virtualization is available, also run guest-boot / integration tests; otherwise mark them
"not run (no nested virt)" — never claim or fake a boot or benchmark you didn't run.

**NORTH STAR:** Enlil is a Rust Type-1 hypervisor that turns x86 hardware into multiple
transparent virtual PCs with granular peripheral routing and GPU sharing. Guests must
believe they're on bare metal. Hardware is virtualized and poolable, so machines can be
combined or split — including across a network (Phase 11, Enlil Mesh). Every run should
move some phase toward that end state.

---

Do these in order, in one focused session:

### 1. Orient (read before writing)
- Read `enlil-roadmap.md`, `ARCHITECTURE.md`, the latest `PHASE*-IMPLEMENTATION.md`, and
  `PROGRESS.md` (the running log) if it exists.
- Run `git log --oneline -15` to see what recent runs actually landed.
- Identify the current phase and the **single next unimplemented item** — the first
  sub-section under the current phase not yet built, verified against the code. (The
  roadmap is prose/section-based, not checkboxes. Phases 0–3 are done; Phase 4 USB
  routing / Phase 5 Windows guest + KVM backend were most recently in flight.)

### 2. Research (time-box ~30 min — it serves the build, it is not the deliverable)
- Search recent (≤ ~18 months) work relevant to **that specific next step**:
  Rust/RustVMM hypervisors, VT-x/VT-d & AMD-V/Vi, IOMMU & SR-IOV passthrough, GPU
  mediated passthrough / vGPU, VirtIO, hypervisor transparency / anti-detection, live
  migration, RDMA & disaggregated compute (Phases 9/11). Prefer arXiv and OSDI/SOSP/
  USENIX ATC papers and primary vendor docs (Intel SDM, AMD APM) over blogs.
- Append findings to `enlil-research-review.md` under a dated heading: source link + 1–2
  lines on **how it changes what we build**. Skip anything already logged there.
- If nothing new applies, write one line saying so and move on.

### 3. Fold insights into the roadmap
- Make **surgical** edits to `enlil-roadmap.md` only where research changes the plan
  (reprioritize, add a sub-task, flag a pitfall). Never rewrite it wholesale.

### 4. Build the next step
- Implement that one item in the right crate (`enlil-core`, `enlil-hal`, `enlil-devices`,
  `enlil-platform`, …) **with tests**. Prefer one small, complete, tested increment over
  a large unfinished one. Read neighboring modules and match existing style and crate
  boundaries before adding anything new.

### 5. Verify (a step isn't done until this is green)
- `cargo build` (+ `--target x86_64-unknown-enlil.json` for no_std crates), `cargo test`,
  and `cargo clippy` — fix warnings in code you touched.
- Record exactly which tests ran and which were skipped (KVM/guest-boot paths → "not run
  (no nested virt)" when unavailable). Never claim a boot or benchmark you didn't run.

### 6. Land it and hand off
- **Commit directly to `master`** (you opted for no PR), then `git push origin master`.
  Message format: `routine(phase-N): <what you did>`. Because there's no review gate,
  **only commit work that builds and passes the tests you could run** — if it's not
  green, commit nothing and record the blocker in `PROGRESS.md` instead.
- Append to `PROGRESS.md`: date, the step taken, research that informed it, exact test
  results, and the recommended next step for tomorrow. This file + `git log` are how the
  next run (which has no memory of today) resumes without redoing or re-researching work.

---

### Guardrails
- **One increment per run; depth over breadth.** It's an 11-phase road — a clean,
  documented trail beats a pile of half-finished features.
- **Never leave `master` unbuildable.** No green, no commit.
- **Keep the repo clean.** Put throwaway scripts/output in a git-ignored `scratch/` dir,
  never the repo root. Don't add to the existing `fix_*.ps1` / `count*.py` / `*_out.txt`
  clutter; clear some out if you have spare cycles.
- **Be honest.** Cite real sources; never fabricate results, benchmarks, or test passes.
- **Don't stall.** If the next step needs hardware/an environment you don't have,
  document the blocker in `PROGRESS.md` and pick the next viable item instead.

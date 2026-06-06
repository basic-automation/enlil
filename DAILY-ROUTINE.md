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

**NORTH STAR:** Enlil is a Rust Type-1 hypervisor that mediates *both directions* of every
hardware interaction (OS→HW and HW→OS) and routes them across a pool of physical nodes —
turning M machines into N transparent, composable *logical machines*. Guests must believe
they're on bare metal and must not detect the hypervisor. The goal is **OS-agnostic**:
Linux, Windows, *BSD, x86 Android near-term; ARM Android and Apple-Silicon via Phase 10;
macOS (Apple HW) and iOS as constrained/research targets. Interconnect latency sets what
can be pooled at what distance (PCIe/CXL → RDMA → WAN); a kernel's hot CPU+RAM working set
stays on one node, everything else is poolable with latency-aware placement. See
`ROADMAP.md` → **Core Model** for the authoritative architecture. Every run should
move some roadmap phase toward that end state.

---

## Session model — work the full 3–4 hours, many increments per run

A run is **not** one task. Budget **3–4 hours** of wall-clock work and land **as many complete,
tested increments as possible** in that window — typically several, not one. Each increment is
one trip through steps 1–6 below; when you finish one, **loop straight back to step 1** and pick
the next item. Keep going until the time budget is spent or you genuinely run out of tractable,
unblocked work.

The discipline that makes this breadth safe is that **every increment is independently green and
committed before you start the next** (step 6). A run is therefore a *sequence* of small,
complete, tested commits — never one sprawling unfinished change. Do a full orient + research
once at the start; between increments, re-orient only briefly (and research only when the next
item is in a genuinely new area) so the bulk of the budget goes into building and verifying.

Do these in order, looping for the whole session:

### 1. Orient (read before writing)
- Read `ROADMAP.md`, `README.md`, the latest `PHASE*-IMPLEMENTATION.md`, and
  `PROGRESS.md` (the running log) if it exists.
- Run `git log --oneline -15` to see what recent runs actually landed.
- Identify the current phase and the **single next unimplemented item** — the first
  sub-section under the current phase not yet built, verified against the code. (The
  roadmap is prose/section-based, not checkboxes. Phases 0–3 are done; Phase 4 USB
  routing / Phase 5 Windows guest + KVM backend were most recently in flight.)

### 2. Research (lean and proportionate — it serves the build, it is not the deliverable)
> Time-box the *first* increment's research to ~30 min; later increments in the same session
> get only a quick targeted check (skip entirely if already covered). Research is a tax on build
> time — across a 3–4 hour run it should stay a small fraction of the total.
- Search recent (≤ ~18 months) work relevant to **that specific next step**:
  Rust/RustVMM hypervisors, VT-x/VT-d & AMD-V/Vi, IOMMU & SR-IOV passthrough, GPU
  mediated passthrough / vGPU, VirtIO, hypervisor transparency / anti-detection, live
  migration, RDMA & disaggregated compute (Phases 9/11). Prefer arXiv and OSDI/SOSP/
  USENIX ATC papers and primary vendor docs (Intel SDM, AMD APM) over blogs.
- Append findings to `RESEARCH.md` under a dated heading: source link + 1–2
  lines on **how it changes what we build**. Skip anything already logged there.
- If nothing new applies, write one line saying so and move on.

### 3. Fold insights into the roadmap
- Make **surgical** edits to `ROADMAP.md` only where research changes the plan
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

### 6. Land each increment, then loop (hand off once at the end)
- **Commit each green increment on its own.** The remote environment automatically puts the
  run's commits on a branch and opens a PR — it does *not* push to `master` directly. Use commit
  message format `routine(phase-N): <what you did>`, **one commit per complete increment**.
  **Only commit work that builds and passes the tests you could run** — if an increment isn't
  green, commit nothing for it and record the blocker in `PROGRESS.md`.
- **Then loop.** If time remains in the 3–4 hour budget and there's a tractable, unblocked next
  item, go back to step 1 and build it as a *new* commit on the same branch. Don't stop after
  one. Stop only when the budget is spent, every remaining item is blocked, or the only work
  left is something you can't finish and test cleanly in the time remaining (in which case leave
  it for tomorrow rather than committing it half-done).
- **Hand off once, at the end.** Append a single dated `PROGRESS.md` entry covering the whole
  session: each increment landed (with its commit) and the research that informed it, exact test
  results, and the recommended next step(s) for tomorrow. This file + `git log` are how the next
  run (which has no memory of today) resumes without redoing or re-researching work.

---

### Guardrails
- **Many increments per run, but each one complete.** Fill the 3–4 hour budget with as many
  tested increments as you can — yet depth *per increment* still wins: a clean, documented trail
  of small green commits beats a pile of half-finished features. Never stretch a single
  increment across the "still broken" line just to claim another task.
- **Never leave `master` unbuildable.** No green, no commit — enforced per increment, so a late
  failure never poisons the earlier good commits already on the branch.
- **Keep the repo clean.** Put throwaway scripts/output in a git-ignored `scratch/` dir,
  never the repo root. Don't add to the existing `fix_*.ps1` / `count*.py` / `*_out.txt`
  clutter; clear some out if you have spare cycles.
- **Be honest.** Cite real sources; never fabricate results, benchmarks, or test passes.
- **Don't stall.** If the next step needs hardware/an environment you don't have,
  document the blocker in `PROGRESS.md` and pick the next viable item instead.

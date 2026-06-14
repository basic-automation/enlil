# Enlil — Daily Autonomous Development Routine

> This file is the single source of truth for the daily routine. Edit it to change
> what the routine does; the loop just says "follow this file."

**REPO:** `github.com/physics515/enlil` (branch `master`). Rust workspace, bare-metal
Type-1 hypervisor. Custom target `x86_64-unknown-enlil.json`; toolchain pinned in
`rust-toolchain.toml`.

**ENVIRONMENT (LOCAL + WSL + Windows, since 2026-06-12):** This runs unattended on the Windows
workstation as the Claude Code scheduled task `enlil-dev-routine`. It does its **Linux/KVM**
build/test + **all git/PR** work **inside the Ubuntu WSL2 distro** on a native clone at `~/enlil`
(ext4 — never build the Linux side on `/mnt/...`; the 9p bridge is slow and breaks cargo's
mtimes), driven as `wsl -d Ubuntu -- bash -lc 'cd ~/enlil && <cmd>'`; AND it runs **Windows-native
builds** for the host-agnostic crates + cross-targets (see **BUILD MATRIX** below) so the project
stays buildable on both toolchains, whichever a given change needs. The decisive reason for running here rather
than the old cloud runner: **`/dev/kvm` is available** (nested virtualization on, custom WSL
kernel with KVM built in), so the Linux KVM host backend (`enlil-platform` / `enlil-devices`,
the `target_os = "linux"` paths) builds AND the **guest-boot / KVM integration tests actually
RUN** every night — they are no longer auto-skipped as "no nested virt." Always `cargo build`,
`cargo test`, `cargo clippy`; run the guest-boot/KVM tests for real and record measured results.
Only mark a KVM test "not run" if `/dev/kvm` is genuinely absent that night (say exactly why) —
never claim or fake a boot or benchmark you didn't run.

**EXECUTION (local WSL mechanics):**
- Every command runs in WSL: `wsl -d Ubuntu -- bash -lc 'cd ~/enlil && <cmd>'`. `~/enlil` is a
  dedicated clone, separate from the user's Windows checkout — no worktree collision.
- **git auth is pre-wired:** WSL git uses the Windows credential manager (`credential.helper` →
  `git-credential-manager.exe`), so `git push` and `git credential fill` reuse the host's GitHub
  login with no prompt. There is **no `gh` inside WSL** — get a token with `gh.exe auth token`
  (the Windows gh, already authenticated) and open the PR via the GitHub REST API
  (`POST https://api.github.com/repos/physics515/enlil/pulls`).
- **KVM access** requires the WSL user in the `kvm` group (one-time, human: `sudo usermod -aG
  kvm physi` then `wsl --shutdown`). Probe each run with `test -w /dev/kvm`; if it fails the
  group step hasn't taken effect yet — log KVM tests "not run (kvm group pending)" and proceed
  with the rest rather than stalling.

**BUILD MATRIX — test on Windows AND Linux, whichever the change needs.** Enlil has no
Windows-*host* backend today (zero `target_os = "windows"`), but it is OS-agnostic by intent and
must stay buildable on both toolchains. Per increment, build/test in the environment(s) the change
actually targets — don't blindly run both, and don't skip a side a change clearly affects:
- **Linux / KVM host backend** — `enlil-platform` / `enlil-devices` `target_os = "linux"` paths,
  anything touching `/dev/kvm`, the guest-boot/integration tests: **WSL only** (`wsl -d Ubuntu --
  bash -lc 'cd ~/enlil && cargo build/test …'`). The primary environment and the only one with
  `/dev/kvm`.
- **Host-agnostic** — the `no_std` core (`enlil-core`/`enlil-hal`), the `x86_64-unknown-uefi`
  bare-metal payload and the custom `x86_64-unknown-enlil` target — **and** the std tooling crates
  (`enlil-config`/`enlil-mgmt`/`enlil-setup`/`enlil-std`): build on **BOTH** when touched (WSL +
  Windows-native), so a Linux-only dep or path can't silently break the Windows dev build. The
  Windows toolchain already has `x86_64-unknown-uefi` + `x86_64-pc-windows-msvc` installed.
- **Windows-native build mechanism** (no second clone — build against the WSL clone over 9p with a
  *Windows* target-dir so the two toolchains never stomp each other's `target/`), run from
  PowerShell on the Windows side:
  ```
  cargo build [-p CRATE] [--target x86_64-unknown-uefi] `
    --manifest-path \\wsl.localhost\Ubuntu\home\physi\enlil\Cargo.toml `
    --target-dir D:\Development\.enlil-win-target
  ```
  Pick the right `-p CRATE` / `--target` for what you touched — **do not** `--workspace` it (the
  Linux-only crates won't compile on Windows; that's expected, not a failure). Record which
  toolchain(s) you built/tested on; never claim a Windows pass you didn't run. (A known issue a
  Windows/uefi build surfaces: `enlil-core` currently pulls `tokio`, which won't cross-compile to
  `x86_64-unknown-uefi` — a real no_std-hygiene bug worth a fix-increment, not an infra problem.)

**Parallel background builds — fill the wait, land more.** The two toolchains (and slow cross-
target builds) are independent, so **run them concurrently in the background** and work the next
increment while they churn — the same "push where there is mush" idea as the board routine. Launch
the WSL build/test and the Windows build as background jobs, start implementing the next increment,
then collect each build's result when it finishes and commit that increment once its required
builds are actually green. Don't serialize the run through one-build-at-a-time waits; a 3–4 hour
session should overlap them. Each increment's commit stays gated on its own **real** green results
— never commit on a build still in flight or one you didn't read.

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

## Session model — work the full 3–4 wall-clock hours, many increments per run

A run is **not** one task, and it is **not** a fixed number of increments. The stopping
criterion is **elapsed wall-clock time: keep working until 3–4 hours have actually passed.**
Note the wall-clock time when you start; check it as you go and treat ~3–4 hours of real elapsed
time as the budget. **Do not stop after 3–4 increments (or any other count) — a handful of
commits is not "done" if only an hour has elapsed.** Land **as many complete, tested increments
as possible** in that window — typically many, not a few. Each increment is one trip through
steps 1–6 below; when you finish one, **loop straight back to step 1** and pick the next item.
Keep going until the wall-clock budget is genuinely spent, or you truly run out of tractable,
unblocked work (document the latter explicitly in `PROGRESS.md` rather than stopping early on a
hunch). When one roadmap area is exhausted, move to the next viable item anywhere on the
roadmap rather than ending the run.

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
- `cargo build` (+ the cross-target for no_std / bare-metal crates), `cargo test`, and `cargo
  clippy` — fix warnings in code you touched — **on the toolchain(s) the change needs per the
  BUILD MATRIX**: Linux/KVM paths → WSL; host-agnostic + std-tooling crates → WSL *and*
  Windows-native. Run the two toolchains' builds **concurrently in the background** and gate each
  increment's commit on its own real green results (parallel-background-builds, above).
- **Run the KVM / guest-boot tests for real** — `/dev/kvm` is available in this WSL environment.
  Record exactly which ran and their measured results; only mark one skipped if `/dev/kvm` is
  genuinely absent that night (e.g. `kvm` group pending — say why). Never claim a boot or
  benchmark you didn't run.

### 6. Land each increment, then loop (hand off once at the end)
- **Commit each green increment on its own**, on the run's branch `routine/enlil-<YYYY-MM-DD>`
  (same-day rerun: `-2`; **never push `master`**). Use commit message format
  `routine(phase-N): <what you did>`, **one commit per complete increment**, staging explicit
  paths (never `git add -A`). **Only commit work that builds and passes the tests you could
  run** — if an increment isn't green, commit nothing for it and record the blocker in
  `PROGRESS.md`.
- **Then loop.** As long as the **wall-clock budget (3–4 hours of real elapsed time) has not run
  out** and there's a tractable, unblocked next item, go back to step 1 and build it as a *new*
  commit on the same branch. Don't stop after a few increments — finishing the PIC (or any one
  feature) is not the end of the run if only an hour has passed; pick the next roadmap item and
  keep going. Stop only when ~3–4 hours have actually elapsed, every remaining item is blocked,
  or the only work left is something you can't finish and test cleanly in the time remaining (in
  which case leave it for tomorrow rather than committing it half-done).
- **Hand off once, at the end — log, push, open the PR.** Append a single dated `PROGRESS.md`
  entry covering the whole session: each increment landed (with its commit) and the research that
  informed it, exact test results (**including the KVM / guest-boot outcomes**), an explicit
  **STOP REASON** (wall-clock budget spent · no unblocked item left · environment blocker), and
  the recommended next step(s) for tomorrow. Then push the branch and **open the PR** (base
  `master`) via the GitHub REST API using the `gh.exe auth token` token — **the PR is the last
  act of the run, never the end of the first increment.** Never finish a run without a PR (or a
  logged reason you couldn't open one). This file + `git log` are how the next run (no memory of
  today) resumes without redoing or re-researching work.

---

### Guardrails
- **The budget is wall-clock time, not an increment count.** Fill the full **3–4 hours of real
  elapsed time** with as many tested increments as you can — measure the budget in hours, not in
  number of commits or "sessions," and don't wrap up after 3–4 increments when time remains.
  Depth *per increment* still wins: a clean, documented trail of small green commits beats a pile
  of half-finished features, and you never stretch a single increment across the "still broken"
  line just to claim another task. But "I did a few good increments" is **not** a reason to
  stop — only the clock or a genuine lack of unblocked work is.
- **Never leave `master` unbuildable.** No green, no commit — enforced per increment, so a late
  failure never poisons the earlier good commits already on the branch.
- **Keep the repo clean.** Put throwaway scripts/output in a git-ignored `scratch/` dir,
  never the repo root. Don't add to the existing `fix_*.ps1` / `count*.py` / `*_out.txt`
  clutter; clear some out if you have spare cycles.
- **Be honest.** Cite real sources; never fabricate results, benchmarks, or test passes.
- **Don't stall.** If the next step needs hardware/an environment you don't have,
  document the blocker in `PROGRESS.md` and pick the next viable item instead.

# Enlil Developer Documentation

Enlil is a portable, bare-metal-capable **Type-1 hypervisor written in Rust**
that turns a single x86 desktop into multiple transparent virtual PCs — with a
long-term path toward pooling hardware across many physical machines.

This book is the developer-facing companion to the repository's front door:

- [**README.md**](https://github.com/basic-automation/enlil) — project
  overview: what Enlil is, the core model, current status, how to build, and
  configuration.
- [**ROADMAP.md**](https://github.com/basic-automation/enlil/blob/master/ROADMAP.md) —
  the development roadmap: a `[ ]`/`[x]` task checklist of every phase, from
  scaffold through to the multi-machine mesh.
- [**SECURITY.md**](https://github.com/basic-automation/enlil/blob/master/SECURITY.md) —
  security model. Enlil is pre-1.0 research software: **do not run untrusted
  guests**.

## What this book covers

- [Layer stack](layer-stack.md) — the platform-first design: one codebase for
  hosted (Linux/KVM) development and bare-metal production, plus the HAL that
  keeps ISA-specific virtualization details behind a single trait.
- [Crate structure](crate-structure.md) — what each of the nine workspace
  crates owns.
- [KVM-to-bare-metal migration plan](migration-plan.md) — how the KVM-backed
  dev path becomes the native bare-metal hypervisor, phase by phase.
- [Dev environment](dev-environment.md) — the pinned nightly toolchain,
  rustup setup, and the mdbook version pin used to build this book.
- [Testing](testing.md) — how the test suite behaves with and without
  `/dev/kvm`, and why skips are never faked as passes.
- [Docs build & CI](docs-ci.md) — how the book itself is built and gated in
  CI, reproducibly.

## Conventions used in this project

- **Autonomy with receipts.** Work proceeds in focused branches with green
  `fmt`/`clippy`/`test` before anything is published.
- **Dated pins over floating ones.** The Rust toolchain is pinned to a dated
  nightly in `rust-toolchain.toml`; mdbook is pinned to 0.4.52 in CI. Bumping
  either is a deliberate, reviewed change.
- **Honest skips.** KVM-dependent tests self-skip when `/dev/kvm` is absent
  rather than failing or pretending. The same applies to docs: what is stubbed
  is marked stubbed.

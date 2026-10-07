# Dev environment

## Rust toolchain

Enlil pins a **dated nightly** in `rust-toolchain.toml` — currently
`nightly-2026-09-23` — with components `rustfmt`, `clippy`, `rust-src` and
targets `x86_64-unknown-linux-gnu` and `x86_64-unknown-uefi`. The bare-metal
path needs `-Z build-std` and some `asm`/`no_std` features, which is why a
nightly is required at all.

The pin is dated on purpose. A floating `nightly` once broke two unrelated
things in a single night (a `rustc-abi` rename in the custom target spec, and
a clippy pedantic/nursery tightening). Bumping the date is a deliberate,
reviewed change — the roadmap records it as its own task.

Setup:

```sh
# Install rustup if missing, then install exactly the pinned toolchain.
rustup show active-toolchain || rustup toolchain install
```

Run from the repo root, `rust-toolchain.toml` selects the pinned toolchain
automatically. Verify with `rustc --version` and confirm the components:

```sh
rustup component list --installed
```

## Standard commands

```sh
cargo build                    # hosted development build (Linux/KVM backend)
cargo test --workspace         # workspace unit + integration tests
cargo clippy --all-targets --workspace -- -D warnings
cargo fmt --all -- --check

# Bare-metal target — recompiles std against the Enlil platform layer
cargo build --target x86_64-unknown-enlil.json -Z build-std=core,alloc,std
```

## Building this book

mdbook is pinned to **0.4.52** (the last 0.4.x release). The pin is
deliberate: mdBook 0.5 removed or renamed several `book.toml` options, so an
unpinned install could break the docs build without any docs change. The same
pinned version is installed in CI (see [Docs build & CI](docs-ci.md)).

```sh
# One-time install of the pinned release binary:
MDBOOK_VERSION=0.4.52
curl -sSL "https://github.com/rust-lang/mdBook/releases/download/v${MDBOOK_VERSION}/mdbook-v${MDBOOK_VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
  | tar -xz -C "$HOME/.cargo/bin" mdbook

# Build and preview:
mdbook build docs     # renders to docs/book/
mdbook serve docs     # live-reload preview at http://localhost:3000
```

Do not add 0.5-only `book.toml` options while the pin is 0.4.52 — `mdbook
build` will fail, and CI gates on it.

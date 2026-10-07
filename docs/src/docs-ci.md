# Docs build & CI

## CI pipeline

`.github/workflows/ci.yml` runs a single `check` job on `ubuntu-latest` for
pushes and pull requests to `master`/`main`:

1. Install `acpica-tools` and `dmidecode` (makes the ACPI/SMBIOS integration
   tests hard gates instead of silent skips).
2. Install exactly the toolchain `rust-toolchain.toml` pins
   (`rustup show active-toolchain || rustup toolchain install`) — CI and local
   builds use the same compiler.
3. `cargo fmt --all -- --check`
4. `cargo clippy --all-targets --workspace -- -D warnings`
5. `cargo test --workspace`
6. Install the pinned mdbook release binary and run `mdbook build docs`.

The docs build is a first-class gate: a broken book fails CI exactly like a
broken test.

## Reproducible mdbook install

CI does not compile mdbook from source and does not use `latest`. It downloads
the pinned release tarball into `$HOME/.cargo/bin`:

```yaml
- name: Install pinned mdbook
  run: |
    MDBOOK_VERSION=0.4.52
    curl -sSL "https://github.com/rust-lang/mdBook/releases/download/v${MDBOOK_VERSION}/mdbook-v${MDBOOK_VERSION}-x86_64-unknown-linux-gnu.tar.gz" \
      | tar -xz -C "$HOME/.cargo/bin" mdbook
    mdbook --version
- name: Build developer docs
  run: mdbook build docs
```

The version appears in exactly two places — this file and the workflow — and
both must be bumped together. Bumping past 0.4.x requires auditing `book.toml`
against the new release's removed/renamed options (0.5 removed `multilingual`
and `copy-fonts`, and renamed `curly-quotes` to `smart-punctuation`).

## Local check

```sh
mdbook build docs 2>&1 | tail -3
```

should end with a clean build summary and no warnings. The rendered book lands
in `docs/book/` (gitignored build output — never committed).

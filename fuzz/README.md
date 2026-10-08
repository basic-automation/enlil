# BadAML fuzzing for Enlil's synthesized ACPI tables

Guests parse the AML bytecode in our DSDT/SSDT. This directory holds the
BadAML-style fuzzing setup for [T-5.3](../../ROADMAP.md): a libFuzzer target
that hammers the strict AML validator (`enlil_devices::acpi::aml_validate`)
with arbitrary bytes, plus a checked-in seed corpus.

## Layout

- `fuzz_targets/aml.rs` — the libFuzzer target. Feeds arbitrary input to
  `validate_aml` (raw AML) and `validate_table_payload` (whole ACPI table).
  Both entry points are total by construction: any panic, hang, or
  out-of-bounds access is a real malformed-table-handling defect.
- `corpus/aml/` — checked-in seed corpus (generate with
  `cargo run -p enlil-devices --example gen_aml_corpus`):
  - `valid_*` — real builder output (DSDT/SSDT across configs); the validator
    must accept all of these.
  - `malformed_*` — truncations, inflated `PkgLength`s, 200-deep nesting, bad
    names, garbage; the validator must reject all of these without panicking.
- `Cargo.toml` — detached from the main workspace (`[workspace]`), so the
  libFuzzer-instrumented build never leaks into normal builds.

## Running

```sh
cargo install cargo-fuzz
cargo fuzz run aml -- -max_total_time=300
```

Crashes land in `fuzz/artifacts/aml/`. Minimize interesting ones
(`cargo fuzz tmin aml fuzz/artifacts/aml/<file>`), promote them into
`fuzz/corpus/aml/`, and add a regression case to
`enlil-devices/tests/aml_badaml_fuzz.rs`.

## CI-friendly half

The deterministic half of this task needs no fuzzer installation and runs
under the normal test suite:

```sh
cargo test -p enlil-devices acpi
```

It classifies the whole corpus, validates generated tables across a config
matrix, and runs a seeded 10k-mutation campaign asserting the validator never
panics or hangs.

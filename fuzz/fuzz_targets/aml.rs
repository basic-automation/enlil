//! BadAML libFuzzer target: fuzz the guest-visible AML our tables synthesize.
//!
//! The target feeds arbitrary bytes to the strict AML validator
//! ([`enlil_devices::acpi::aml_validate`]) as both a raw AML stream and a
//! whole ACPI table. The validator is total by construction, so any panic,
//! hang, or out-of-bounds access libFuzzer observes is a real defect in the
//! malformed-table handling path — the class of bug that would otherwise
//! surface as a guest crash or a hypervisor-detection tell.
//!
//! Run with:
//!
//! ```sh
//! cargo install cargo-fuzz
//! cargo fuzz run aml -- -max_total_time=300
//! ```
//!
//! Seed corpus lives in `fuzz/corpus/aml/` (checked in). Reproducer crashes go
//! to `fuzz/artifacts/aml/`; minimize and promote interesting ones into the
//! corpus, and add a regression case to `enlil-devices/tests/aml_badaml_fuzz.rs`.

#![no_main]

use enlil_devices::acpi::aml_validate::{validate_aml, validate_table_payload};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Both entry points must be total: arbitrary input may only ever produce
    // Ok/Err, never a panic or unbounded work.
    let _ = validate_aml(data);
    let _ = validate_table_payload(data);
});

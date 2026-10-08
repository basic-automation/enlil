//! Generate the checked-in BadAML seed corpus (`fuzz/corpus/aml/`).
//!
//! The seeds are the starting inputs for both the libFuzzer target
//! (`fuzz/fuzz_targets/aml.rs`: `cargo fuzz run aml`) and the deterministic
//! mutation campaign (`tests/aml_badaml_fuzz.rs`, which runs under
//! `cargo test -p enlil-devices acpi`). They come in two flavors:
//!
//! * `valid_*` — real AML our builders emit (DSDT/SSDT across configs). The
//!   validator must accept every one of these; a rejection is a builder or
//!   validator bug.
//! * `malformed_*` — hand-built adversarial inputs (truncations, inflated
//!   `PkgLength`s, pathological nesting, bad names, garbage). The validator
//!   must reject every one without panicking.
//!
//! Regenerate with: `cargo run -p enlil-devices --example gen_aml_corpus`
//! then review the diff before committing — corpus changes should be
//! deliberate, since the regression tests pin their classification.

use std::path::Path;

use enlil_devices::acpi::aml::AmlBuilder;
use enlil_devices::acpi::dsdt::{DsdtBuilder, DsdtConfig};
use enlil_devices::acpi::ssdt::{CState, PState, SsdtBuilder};

/// Strip the 36-byte ACPI table header, leaving the raw AML payload.
fn aml_payload(table: &[u8]) -> &[u8] {
    &table[36..]
}

fn write_seed(dir: &Path, name: &str, bytes: &[u8]) {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    println!("wrote {} ({} bytes)", path.display(), bytes.len());
}

fn main() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let dir = manifest.join("../fuzz/corpus/aml");
    std::fs::create_dir_all(&dir).expect("create corpus dir");

    // --- Valid seeds: real builder output across configs. ---

    let default_dsdt = DsdtBuilder::new(DsdtConfig::default()).build();
    write_seed(&dir, "valid_dsdt_default.aml", aml_payload(&default_dsdt));

    let minimal = DsdtBuilder::new(DsdtConfig {
        vcpu_count: 1,
        has_hpet: false,
        has_rtc: false,
        has_ps2: false,
        ..DsdtConfig::default()
    })
    .build();
    write_seed(&dir, "valid_dsdt_minimal.aml", aml_payload(&minimal));

    let max = DsdtBuilder::new(DsdtConfig {
        vcpu_count: 8,
        ..DsdtConfig::default()
    })
    .build();
    write_seed(&dir, "valid_dsdt_max.aml", aml_payload(&max));

    let ssdt = SsdtBuilder::new(4).build();
    write_seed(&dir, "valid_ssdt.aml", aml_payload(&ssdt));

    let ssdt_pm = SsdtBuilder::new(2)
        .pstates(vec![
            PState {
                frequency_mhz: 4200,
                power_mw: 125_000,
                latency_us: 10,
                control: 0x2A,
                status: 0x2A,
            },
            PState {
                frequency_mhz: 800,
                power_mw: 15_000,
                latency_us: 10,
                control: 0x08,
                status: 0x08,
            },
        ])
        .cstates(vec![CState {
            ctype: 1,
            latency_us: 1,
            power_mw: 1_000,
            register_address: 0,
            register_bit_width: 0,
            address_space: 0x7F,
        }])
        .build();
    write_seed(&dir, "valid_ssdt_pstates.aml", aml_payload(&ssdt_pm));

    // --- Malformed seeds: must be rejected, never panic on. ---

    // Truncated mid-table.
    let cut = default_dsdt.len() / 2;
    write_seed(
        &dir,
        "malformed_truncated.bin",
        &aml_payload(&default_dsdt)[..cut.min(aml_payload(&default_dsdt).len())],
    );

    // First ScopeOp's PkgLength inflated to a 4-byte form claiming ~256 MiB.
    let mut overrun = aml_payload(&default_dsdt).to_vec();
    if let Some(pos) = overrun.iter().position(|&b| b == 0x10) {
        for (i, b) in [0xFFu8, 0xFF, 0xFF, 0xFF].iter().enumerate() {
            if pos + 1 + i < overrun.len() {
                overrun[pos + 1 + i] = *b;
            }
        }
    }
    write_seed(&dir, "malformed_pkglen_overrun.bin", &overrun);

    // 200 nested scopes: consistent PkgLengths, only the depth cap trips.
    let mut deep = AmlBuilder::new();
    let mut handles = Vec::new();
    for _ in 0..200 {
        handles.push(deep.scope_start(b"_S0_"));
    }
    deep.raw(&[0x00]);
    for h in handles.iter().rev() {
        deep.scope_end(h);
    }
    write_seed(&dir, "malformed_deep_nesting.bin", &deep.into_bytes());

    // NameOp with a digit as the NameSeg lead character.
    write_seed(
        &dir,
        "malformed_bad_name.bin",
        &[0x08, b'1', b'A', b'B', b'C', 0x00],
    );

    // A lone ScopeOp whose PkgLength claims far past the buffer.
    write_seed(
        &dir,
        "malformed_huge_pkglen.bin",
        &[0x10, 0xFF, 0xFF, 0xFF, 0xFF],
    );

    // Pure garbage.
    write_seed(&dir, "malformed_garbage.bin", &[0xFF; 128]);

    println!("done: corpus at {}", dir.display());
}

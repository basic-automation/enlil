//! Reference-compiler validation of the synthesized ACPI AML.
//!
//! The DSDT/SSDT this crate emits are hand-encoded AML bytecode. Unit tests in
//! the crate decode individual byte sequences, but until a *real* ACPI
//! interpreter parses the whole table end-to-end, structural bugs (a wrong
//! `PkgLength`, a malformed package, an illegal `_HID`/`_ADR` pairing) stay
//! latent — exactly the class of bug that only surfaces when a guest finally
//! boots. This test runs the Intel ACPI compiler (`iasl`, from `acpica-tools`)
//! over the emitted tables: it disassembles the AML and recompiles it, asserting
//! the round-trip reports **0 errors and 0 warnings**.
//!
//! `iasl` is not part of the Rust toolchain, so when it is absent the test
//! self-skips (printing a notice) rather than failing or fabricating a pass —
//! the same honesty as the `/dev/kvm` integration test. Install `acpica-tools`
//! to exercise it (CI can do so to make this a hard gate).

use std::path::{Path, PathBuf};
use std::process::Command;

use enlil_devices::acpi::{AcpiTableSetConfig, build_acpi_tables};

/// Returns `true` when an `iasl` binary is callable on this runner.
fn iasl_available() -> bool {
    Command::new("iasl")
        .arg("-v")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Make a unique, writable scratch directory for one table's round-trip so
/// concurrent test binaries never collide on `iasl`'s fixed output names.
fn scratch_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("enlil-iasl-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Disassemble `<dir>/<stem>.aml` then recompile the resulting `.dsl`, returning
/// the combined `iasl` stdout/stderr of the *recompile* (where errors/warnings
/// are tallied). Panics if either `iasl` invocation cannot be spawned.
fn iasl_round_trip(dir: &Path, stem: &str) -> String {
    // Disassemble: iasl writes <stem>.dsl into the working directory.
    let dis = Command::new("iasl")
        .current_dir(dir)
        .arg("-d")
        .arg(format!("{stem}.aml"))
        .output()
        .expect("spawn iasl -d");
    let dis_out =
        String::from_utf8_lossy(&dis.stdout).into_owned() + &String::from_utf8_lossy(&dis.stderr);
    assert!(
        dis.status.success(),
        "iasl failed to disassemble {stem}.aml:\n{dis_out}"
    );

    // Recompile the disassembly: this is where structural defects are reported.
    let asm = Command::new("iasl")
        .current_dir(dir)
        .arg(format!("{stem}.dsl"))
        .output()
        .expect("spawn iasl recompile");
    String::from_utf8_lossy(&asm.stdout).into_owned() + &String::from_utf8_lossy(&asm.stderr)
}

/// Assert that an `iasl` compile summary reports no errors and no warnings.
fn assert_clean(table: &str, report: &str) {
    // The summary line looks like:
    //   "Compilation successful. 0 Errors, 0 Warnings, 0 Remarks, 0 Optimizations"
    assert!(
        report.contains("0 Errors"),
        "{table}: iasl reported errors:\n{report}"
    );
    assert!(
        report.contains("0 Warnings"),
        "{table}: iasl reported warnings:\n{report}"
    );
}

/// Extract a named table's bytes (header length-trimmed) from a built table set.
fn table_bytes(set: &enlil_devices::acpi::AcpiTableSet, name: &str) -> Vec<u8> {
    let off = set
        .table_offsets
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("table {name} present in set"))
        .1;
    let tbl = &set.tables[off..];
    let len = u32::from_le_bytes(tbl[4..8].try_into().unwrap()) as usize;
    tbl[..len].to_vec()
}

/// The DSDT is the largest, most intricate hand-encoded AML object (PCI root,
/// `_PRT`, ISA devices, processors, `_S5`); a real ACPI compiler must parse and
/// recompile it with no errors and no warnings.
#[test]
fn dsdt_round_trips_through_iasl_clean() {
    if !iasl_available() {
        eprintln!("skipping: iasl (acpica-tools) not installed on this runner");
        return;
    }
    let set = build_acpi_tables(&AcpiTableSetConfig::default());
    let dir = scratch_dir("dsdt");
    std::fs::write(dir.join("DSDT.aml"), table_bytes(&set, "DSDT")).unwrap();
    let report = iasl_round_trip(&dir, "DSDT");
    assert_clean("DSDT", &report);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The SSDT carries the processor power-state AML; round-trip it the same way.
#[test]
fn ssdt_round_trips_through_iasl_clean() {
    if !iasl_available() {
        eprintln!("skipping: iasl (acpica-tools) not installed on this runner");
        return;
    }
    let set = build_acpi_tables(&AcpiTableSetConfig::default());
    let dir = scratch_dir("ssdt");
    std::fs::write(dir.join("SSDT.aml"), table_bytes(&set, "SSDT")).unwrap();
    let report = iasl_round_trip(&dir, "SSDT");
    assert_clean("SSDT", &report);
    let _ = std::fs::remove_dir_all(&dir);
}

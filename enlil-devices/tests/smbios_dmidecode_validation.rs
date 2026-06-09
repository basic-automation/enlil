//! Reference-parser validation of the synthesized SMBIOS/DMI table.
//!
//! The SMBIOS structures this crate emits are hand-packed binary records whose
//! `Length` fields and string indices must line up exactly, or a real DMI parser
//! mis-reads the string table (a too-short formatted area shifts every following
//! string; a too-long one eats the next string's bytes). Unit tests check
//! individual fields, but only a real consumer exercises the table end-to-end.
//! This test runs `dmidecode --from-dump` (the canonical DMI decoder) over the
//! emitted table and asserts it parses with no `<BAD INDEX>` string references —
//! and that the BIOS does not advertise the "virtual machine" characteristic,
//! the SMBIOS VM tell a transparent hypervisor must not expose.
//!
//! `dmidecode` is not part of the Rust toolchain, so when it is absent the test
//! self-skips (printing a notice) rather than failing — the same honesty as the
//! `iasl` and `/dev/kvm` integration tests. Install `dmidecode` (CI can) to make
//! this a hard gate.

use std::path::PathBuf;
use std::process::Command;

use enlil_devices::smbios::{SmbiosBuilder, SmbiosConfig};

/// Returns `true` when a `dmidecode` binary is callable on this runner.
fn dmidecode_available() -> bool {
    Command::new("dmidecode")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Lay out a `dmidecode --dump-bin`-style image: the 64-bit entry point at
/// offset 0, the structure table at offset 0x20, with the entry point's table
/// address pointing at 0x20 (which `--from-dump` treats as the in-file offset).
fn dump_image(builder: &SmbiosBuilder) -> Vec<u8> {
    let structures = builder.build_structures();
    let ep = builder.build_entry_point(0x20);
    let mut img = ep;
    img.resize(0x20, 0);
    img.extend_from_slice(&structures);
    img
}

fn dump_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("enlil-smbios-{}-{tag}.dump", std::process::id()));
    p
}

#[test]
fn smbios_parses_cleanly_under_dmidecode() {
    if !dmidecode_available() {
        eprintln!("skipping: dmidecode not installed on this runner");
        return;
    }
    let builder = SmbiosBuilder::new(SmbiosConfig::default());
    let path = dump_path("default");
    std::fs::write(&path, dump_image(&builder)).unwrap();

    let out = Command::new("dmidecode")
        .arg("--from-dump")
        .arg(&path)
        .output()
        .expect("spawn dmidecode");
    let report =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_file(&path);

    assert!(
        out.status.success(),
        "dmidecode failed to parse the SMBIOS dump:\n{report}"
    );
    // A wrong structure Length or string index shows up as a dangling reference.
    assert!(
        !report.contains("<BAD INDEX>"),
        "SMBIOS has a dangling string reference (wrong Length or index):\n{report}"
    );
    // The BIOS must not announce that this is a virtual machine.
    assert!(
        !report.to_lowercase().contains("virtual machine"),
        "SMBIOS must not advertise the 'virtual machine' BIOS characteristic:\n{report}"
    );
    // Sanity: the table decoded the expected identity, proving the string table is
    // aligned (a shifted table would corrupt these).
    assert!(
        report.contains("Advanced Micro Devices, Inc."),
        "processor manufacturer must decode correctly (string table aligned):\n{report}"
    );
}

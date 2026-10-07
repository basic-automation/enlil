//! Prints the per-guest UEFI firmware table of an Enlil config as TSV.
//!
//! One line per guest that has a `[guest.<id>.firmware]` block:
//! `<id>\t<code path>\t<vars path>`, sorted by guest id. The config is
//! loaded through [`enlil_config::load_config`], so malformed TOML and
//! validation failures (including the firmware checks) abort here with a
//! non-zero exit instead of producing a half-laid-out ESP.
//!
//! `scripts/make-guest-esp.sh` consumes this to stage each guest's OVMF CODE +
//! VARS onto the stick's ESP at `EFI/enlil/firmware/<id>/`.
//!
//! Guest ids become FAT directory names on the ESP, so ids with firmware are
//! additionally required to be FAT-safe here (letters, digits, `-`, `_`,
//! at most 32 chars — the same contract as `enlil_boot::firmware`): an id
//! containing `/` or `..` would escape the guest's firmware directory.
use std::path::Path;

fn usage() -> ! {
    eprintln!("usage: dump-firmware <config.toml>");
    std::process::exit(2);
}

/// Whether a guest id is safe to use as a directory name on the ESP.
///
/// Mirrors `enlil_boot::firmware::is_valid_guest_id` (the boot-side contract);
/// kept local so `enlil-config` does not depend on the UEFI payload crate.
fn is_esp_safe_guest_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn main() -> anyhow::Result<()> {
    let config_path = std::env::args().nth(1).unwrap_or_else(|| usage());
    let config = enlil_config::load_config(Path::new(&config_path))?;

    let mut ids: Vec<&String> = config
        .guest
        .iter()
        .filter_map(|(id, guest)| guest.firmware.as_ref().map(|_| id))
        .collect();
    ids.sort_unstable();
    for id in ids {
        if !is_esp_safe_guest_id(id) {
            anyhow::bail!(
                "Guest '{id}': id is not ESP-safe (letters, digits, '-', '_' only, max 32 chars); \
                 it becomes a directory name under EFI/enlil/firmware/"
            );
        }
        let firmware = config.guest[id].firmware.as_ref().unwrap_or_else(|| {
            // `ids` was built from guests that have firmware; unreachable.
            unreachable!("guest '{id}' lost its firmware block")
        });
        println!(
            "{id}\t{}\t{}",
            firmware.code.display(),
            firmware.vars.display()
        );
    }
    Ok(())
}

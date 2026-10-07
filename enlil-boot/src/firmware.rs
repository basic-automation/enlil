//! ESP layout contract for per-guest virtual UEFI firmware (Phase 0.3).
//!
//! Guests get their own virtual UEFI: the USB stick's ESP carries an OVMF
//! CODE + VARS pair per guest, staged by `scripts/make-guest-esp.sh` from each
//! guest's `[guest.<id>.firmware]` config block. This module is the boot-side
//! half of that contract — the path constants and guest-id rules both sides
//! agree on, so the layout script and the (future) firmware-loading code can
//! never drift apart.
//!
//! ```text
//! <ESP>/
//!   EFI/BOOT/BOOTX64.EFI                  host UEFI payload (this crate)
//!   EFI/enlil/config.toml                 hypervisor config (copy)
//!   EFI/enlil/firmware/<guest>/OVMF_CODE.fd   per-guest firmware, read-only code
//!   EFI/enlil/firmware/<guest>/OVMF_VARS.fd   per-guest variable-store template
//! ```
//!
//! The VARS file on the stick is a *template*: at guest launch the hypervisor
//! copies it to a writable per-boot vars file. The template itself is never
//! mapped writable and never shared between running guests.
//!
//! Pure `core` (no `alloc`, no `std`) so it compiles for the `uefi` target:
//! paths are written into caller-provided buffers.

/// The host UEFI boot application on the ESP.
pub const BOOT_APP_PATH: &str = "EFI/BOOT/BOOTX64.EFI";
/// The hypervisor config copy on the ESP.
pub const CONFIG_PATH: &str = "EFI/enlil/config.toml";
/// Root of the per-guest firmware tree on the ESP.
pub const FIRMWARE_ROOT: &str = "EFI/enlil/firmware";
/// File name of the read-only firmware code image in a guest's directory.
pub const CODE_FILE_NAME: &str = "OVMF_CODE.fd";
/// File name of the variable-store template in a guest's directory.
pub const VARS_FILE_NAME: &str = "OVMF_VARS.fd";
/// Longest guest id accepted as an ESP directory name.
pub const MAX_GUEST_ID_LEN: usize = 32;

/// Which of a guest's two firmware files a path names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareFile {
    /// The read-only OVMF code image (`OVMF_CODE.fd`).
    Code,
    /// The variable-store template (`OVMF_VARS.fd`).
    Vars,
}

impl FirmwareFile {
    /// The file name used on the ESP.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Code => CODE_FILE_NAME,
            Self::Vars => VARS_FILE_NAME,
        }
    }
}

/// Whether a guest id is safe to use as a directory name on the ESP.
///
/// FAT directory names must not contain separators or parent references, so
/// only ASCII letters, digits, `-` and `_` (up to [`MAX_GUEST_ID_LEN`] chars)
/// are accepted. `scripts/make-guest-esp.sh` enforces the same rule via
/// `enlil-config`'s `dump-firmware` example.
#[must_use]
pub fn is_valid_guest_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_GUEST_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Writes `EFI/enlil/firmware/<guest_id>` into `out`.
///
/// Returns the number of bytes written, or `None` when the id is invalid (see
/// [`is_valid_guest_id`]) or `out` is too small.
#[must_use]
pub fn guest_firmware_dir_into(guest_id: &str, out: &mut [u8]) -> Option<usize> {
    if !is_valid_guest_id(guest_id) {
        return None;
    }
    let mut used: usize = 0;
    for part in [FIRMWARE_ROOT, "/", guest_id] {
        let bytes = part.as_bytes();
        let end = used.checked_add(bytes.len())?;
        out.get_mut(used..end)?.copy_from_slice(bytes);
        used = end;
    }
    Some(used)
}

/// Writes `EFI/enlil/firmware/<guest_id>/<file>` into `out`.
///
/// Returns the number of bytes written, or `None` when the id is invalid or
/// `out` is too small.
#[must_use]
pub fn guest_firmware_path_into(
    guest_id: &str,
    file: FirmwareFile,
    out: &mut [u8],
) -> Option<usize> {
    let dir_len = guest_firmware_dir_into(guest_id, out)?;
    let name = file.file_name().as_bytes();
    let end = dir_len.checked_add(1)?.checked_add(name.len())?;
    out.get_mut(dir_len..dir_len + 1)?.copy_from_slice(b"/");
    out.get_mut(dir_len + 1..end)?.copy_from_slice(name);
    Some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_str(buf: &[u8], len: usize) -> &str {
        core::str::from_utf8(&buf[..len]).unwrap()
    }

    #[test]
    fn accepts_plain_ids_and_rejects_unsafe_ones() {
        assert!(is_valid_guest_id("linux1"));
        assert!(is_valid_guest_id("dev-server_2"));
        assert!(!is_valid_guest_id(""));
        assert!(!is_valid_guest_id("a/b"));
        assert!(!is_valid_guest_id(".."));
        assert!(!is_valid_guest_id("has space"));
        assert!(!is_valid_guest_id("semi;colon"));
        assert!(!is_valid_guest_id(&"x".repeat(MAX_GUEST_ID_LEN + 1)));
        assert!(is_valid_guest_id(&"x".repeat(MAX_GUEST_ID_LEN)));
    }

    #[test]
    fn dir_layout_matches_the_script_contract() {
        let mut buf = [0u8; 128];
        let len = guest_firmware_dir_into("linux1", &mut buf).unwrap();
        assert_eq!(as_str(&buf, len), "EFI/enlil/firmware/linux1");
    }

    #[test]
    fn file_paths_name_code_and_vars() {
        let mut buf = [0u8; 128];
        let len = guest_firmware_path_into("linux1", FirmwareFile::Code, &mut buf).unwrap();
        assert_eq!(as_str(&buf, len), "EFI/enlil/firmware/linux1/OVMF_CODE.fd");
        let len = guest_firmware_path_into("linux1", FirmwareFile::Vars, &mut buf).unwrap();
        assert_eq!(as_str(&buf, len), "EFI/enlil/firmware/linux1/OVMF_VARS.fd");
    }

    #[test]
    fn invalid_id_or_tiny_buffer_yields_none() {
        let mut buf = [0u8; 128];
        assert_eq!(guest_firmware_dir_into("../evil", &mut buf), None);
        assert_eq!(guest_firmware_dir_into("", &mut buf), None);
        let mut tiny = [0u8; 4];
        assert_eq!(guest_firmware_dir_into("linux1", &mut tiny), None);
        assert_eq!(
            guest_firmware_path_into("linux1", FirmwareFile::Code, &mut tiny),
            None
        );
    }
}

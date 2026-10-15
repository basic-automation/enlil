//! ESP payload loading: config + per-guest OVMF firmware (Phase 0.3 / T-0.6).
//!
//! `scripts/make-guest-esp.sh` stages the boot payload's inputs on the USB
//! stick's ESP:
//! ```text
//! <ESP>/
//!   EFI/enlil/config.toml                      hypervisor config (copy)
//!   EFI/enlil/firmware/<guest>/OVMF_CODE.fd    per-guest firmware, read-only code
//!   EFI/enlil/firmware/<guest>/OVMF_VARS.fd    per-guest variable-store template
//! ```
//! (path constants: [`firmware`](crate::firmware)).
//!
//! While boot services are up, the UEFI stage opens the volume it was loaded
//! from, reads the config into boot-services memory, scans it for
//! `[guest.<id>]` table headers, and reads each guest's OVMF CODE + VARS pair
//! into `LOADER_DATA` pages. The staged extents travel to the kernel in the
//! [`BootHandoff`](crate::handoff::BootHandoff), which reports them on serial
//! after `ExitBootServices()`.
//!
//! The guest-id scan is a deliberate structural subset of TOML, not a full
//! parser: the boot app only needs the guest ids to locate the firmware
//! directories, and the staged config is written by `enlil-setup` from the
//! validated `enlil-config` schema, so ids are bare FAT-safe names (see
//! [`firmware::is_valid_guest_id`](crate::firmware::is_valid_guest_id)). The
//! kernel parses the full config itself once it is up.

use crate::firmware;

/// Largest `config.toml` the boot app will stage (1 MiB — the real file is a
/// few KiB; anything bigger is a corrupt FAT or the wrong file).
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Largest single OVMF image the boot app will stage (16 MiB — the real
/// images are 2–4 MiB).
pub const MAX_FIRMWARE_BYTES: u64 = 16 * 1024 * 1024;
/// Largest Linux guest kernel image the boot app will stage (128 MiB).
pub const MAX_LINUX_KERNEL_BYTES: u64 = 128 * 1024 * 1024;
/// Largest Linux guest initrd the boot app will stage (512 MiB).
pub const MAX_LINUX_INITRD_BYTES: u64 = 512 * 1024 * 1024;
/// Largest Linux guest command line the boot app will stage (4 KiB).
pub const MAX_LINUX_CMDLINE_BYTES: u64 = 4096;
/// Largest Linux guest disk-assignment file the boot app will stage (16 KiB).
pub const MAX_LINUX_DISK_BYTES: u64 = 16 * 1024;

/// Calls `f` once per distinct `[guest.<id>]` table header in `config`.
///
/// Only bare FAT-safe ids are recognized: sub-tables such as
/// `[guest.<id>.firmware]`, quoted ids, and anything failing
/// [`firmware::is_valid_guest_id`] are skipped, and duplicates are reported
/// once. At most [`MAX_BOOT_GUESTS`](crate::handoff::MAX_BOOT_GUESTS) distinct
/// ids are tracked for de-duplication; further ids are still reported.
pub fn for_each_guest_id(config: &[u8], f: &mut dyn FnMut(&str)) {
    let mut seen = [([0u8; 33], false); crate::handoff::MAX_BOOT_GUESTS];
    let mut seen_count = 0usize;
    for line in config.split(|&b| b == b'\n') {
        let Some(id) = guest_header_id(line) else {
            continue;
        };
        if is_duplicate(id, &seen[..seen_count]) {
            continue;
        }
        if let Some(slot) = seen.get_mut(seen_count) {
            slot.0[..id.len()].copy_from_slice(id.as_bytes());
            slot.1 = true;
            seen_count += 1;
        }
        f(id);
    }
}

/// Whether `id` is already recorded in the NUL-padded `seen` slots.
fn is_duplicate(id: &str, seen: &[([u8; 33], bool)]) -> bool {
    seen.iter().any(|(slot, used)| {
        *used && {
            let len = slot.iter().position(|&b| b == 0).unwrap_or(slot.len());
            len == id.len() && slot[..len] == *id.as_bytes()
        }
    })
}

/// The guest id if `line` is a `[guest.<id>]` table header, else `None`.
fn guest_header_id(line: &[u8]) -> Option<&str> {
    // Strip a trailing `#` comment: TOML comments start at `#` outside
    // strings, and a table header line never contains a string.
    let line = line
        .iter()
        .position(|&b| b == b'#')
        .map_or(line, |i| &line[..i]);
    let line = trim_ascii(line);
    let inner = line.strip_prefix(b"[")?.strip_suffix(b"]")?;
    let inner = trim_ascii(inner);
    let id = inner.strip_prefix(b"guest.")?;
    // Reject sub-tables (`[guest.<id>.firmware]`) and empty ids. The FAT-safe
    // alphabet has no dots, so `is_valid_guest_id` rejects dotted tails too;
    // the explicit check documents the intent.
    if id.is_empty() || id.contains(&b'.') {
        return None;
    }
    let id = core::str::from_utf8(id).ok()?;
    firmware::is_valid_guest_id(id).then_some(id)
}

/// Leading/trailing ASCII whitespace (including `\r`) removed.
const fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = bytes {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    while let [rest @ .., last] = bytes {
        if last.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

/// Converts an ASCII ESP path (`EFI/enlil/config.toml`) to NUL-terminated
/// UTF-16 codes for the UEFI file protocol.
///
/// `/` separators become `\` (the UEFI canonical separator). The codes,
/// including the NUL terminator, are written to `out`; the return value is
/// the number of codes written. Returns `None` when the path is not pure
/// ASCII or `out` is too small.
#[must_use]
pub fn uefi_path_codes(ascii_path: &[u8], out: &mut [u16]) -> Option<usize> {
    if !ascii_path.is_ascii() || ascii_path.len() + 1 > out.len() {
        return None;
    }
    for (slot, &b) in out.iter_mut().zip(ascii_path.iter()) {
        *slot = u16::from(if b == b'/' { b'\\' } else { b });
    }
    *out.get_mut(ascii_path.len())? = 0;
    Some(ascii_path.len() + 1)
}

#[cfg(target_os = "uefi")]
pub use hw::{EspPayload, load_esp_payload};

/// Firmware-protocol ESP loading, alive only while boot services are up.
#[cfg(target_os = "uefi")]
mod hw {
    use super::{
        MAX_CONFIG_BYTES, MAX_FIRMWARE_BYTES, MAX_LINUX_CMDLINE_BYTES, MAX_LINUX_DISK_BYTES,
        MAX_LINUX_INITRD_BYTES, MAX_LINUX_KERNEL_BYTES, for_each_guest_id, uefi_path_codes,
    };
    use crate::firmware::{self, FirmwareFile};
    use crate::handoff::{GuestFirmware, LinuxPayload, MAX_BOOT_GUESTS, StagedFile};
    use uefi::boot::{self, AllocateType, MemoryType};
    use uefi::data_types::CStr16;
    use uefi::proto::loaded_image::LoadedImage;
    use uefi::proto::media::file::{
        Directory, File, FileAttribute, FileHandle, FileInfo, FileMode, RegularFile,
    };
    use uefi::proto::media::fs::SimpleFileSystem;
    use uefi::{Error, Status};

    /// A file staged from the ESP into boot-services (`LOADER_DATA`) memory.
    #[derive(Debug, Clone, Copy)]
    pub struct LoadedFile {
        /// Physical base of the staged bytes (`0` when the file was empty).
        pub base: u64,
        /// Number of valid bytes at `base`.
        pub len: usize,
    }

    /// Why an ESP file could not be staged.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum EspError {
        /// Our loaded image has no device handle, so the ESP is unreachable.
        NoBootDevice,
        /// The path was not ASCII or did not fit the conversion buffer.
        BadPath,
        /// The file does not exist on the ESP.
        NotFound,
        /// The opened handle is a directory, not a file.
        NotAFile,
        /// The file is larger than the caller's cap.
        TooLarge,
        /// `GetInfo()` failed or the info buffer was too small.
        InfoFailed,
        /// A UEFI boot-services call failed.
        Uefi(Status),
    }

    /// Everything the boot app staged from the ESP, ready for the handoff.
    pub struct EspPayload {
        /// The staged `config.toml` (`None` when the ESP had none).
        pub config: Option<LoadedFile>,
        /// Per-guest OVMF extents; only the first `guest_count` are valid.
        pub guests: [GuestFirmware; MAX_BOOT_GUESTS],
        /// Number of valid entries in `guests` (`<= MAX_BOOT_GUESTS`).
        pub guest_count: usize,
        /// A Linux guest (kernel / initrd / cmdline) staged from
        /// `EFI/enlil/linux/`; empty when the ESP has none.
        pub linux: LinuxPayload,
    }

    fn to_esp_error(e: Error) -> EspError {
        EspError::Uefi(e.status())
    }

    /// The ESP volume the boot application was loaded from.
    struct EspLoader {
        root: Directory,
    }

    impl EspLoader {
        /// Opens the ESP volume the boot application was loaded from.
        ///
        /// # Errors
        ///
        /// Returns [`EspError::NoBootDevice`] when our image has no device
        /// handle, or [`EspError::Uefi`] when a boot-services call fails.
        fn open() -> Result<Self, EspError> {
            let device = {
                let image = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
                    .map_err(to_esp_error)?;
                image.device().ok_or(EspError::NoBootDevice)?
            };
            let mut sfs =
                boot::open_protocol_exclusive::<SimpleFileSystem>(device).map_err(to_esp_error)?;
            let root = sfs.open_volume().map_err(to_esp_error)?;
            Ok(Self { root })
        }

        /// Reads `ascii_path` (e.g. `EFI/enlil/config.toml`) fully into
        /// `LOADER_DATA` pages.
        ///
        /// # Errors
        ///
        /// Returns [`EspError::NotFound`] when the file is absent,
        /// [`EspError::TooLarge`] when it exceeds `max_bytes`,
        /// [`EspError::BadPath`] for a non-ASCII/overlong path, and
        /// [`EspError::Uefi`] when a boot-services call fails.
        fn read_file(&mut self, ascii_path: &[u8], max_bytes: u64) -> Result<LoadedFile, EspError> {
            let mut file = self.open_regular_file(ascii_path)?;
            let size = file_size_of(&mut file)?;
            if size > max_bytes {
                return Err(EspError::TooLarge);
            }
            if size == 0 {
                return Ok(LoadedFile { base: 0, len: 0 });
            }
            let pages = usize::try_from(size.div_ceil(4096)).map_err(|_| EspError::TooLarge)?;
            let base = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, pages)
                .map_err(to_esp_error)?;
            // SAFETY: `allocate_pages` just returned `pages` live pages, and
            // `size` bytes of them are ours to fill; nothing else aliases
            // them.
            let buf = unsafe { core::slice::from_raw_parts_mut(base.as_ptr(), size as usize) };
            let mut read_total = 0usize;
            while read_total < buf.len() {
                let n = file.read(&mut buf[read_total..]).map_err(to_esp_error)?;
                if n == 0 {
                    break;
                }
                read_total += n;
            }
            Ok(LoadedFile {
                base: base.as_ptr() as u64,
                len: read_total,
            })
        }

        /// Opens `ascii_path` relative to the volume root as a regular file.
        fn open_regular_file(&mut self, ascii_path: &[u8]) -> Result<RegularFile, EspError> {
            let mut codes = [0u16; 160];
            let used = uefi_path_codes(ascii_path, &mut codes).ok_or(EspError::BadPath)?;
            let path = CStr16::from_u16_with_nul(&codes[..used]).map_err(|_| EspError::BadPath)?;
            let handle: FileHandle = self
                .root
                .open(path, FileMode::Read, FileAttribute::empty())
                .map_err(|e| match e.status() {
                    Status::NOT_FOUND => EspError::NotFound,
                    status => EspError::Uefi(status),
                })?;
            handle.into_regular_file().ok_or(EspError::NotAFile)
        }
    }

    /// The byte size of `file` via `GetInfo()`.
    fn file_size_of(file: &mut RegularFile) -> Result<u64, EspError> {
        // `FileInfo` is unsized (trailing name); 512 B holds the fixed part
        // plus any FAT file name. The `u64` backing keeps the 8-byte alignment
        // `GetInfo()` requires.
        let mut raw = [0u64; 64];
        let bytes: &mut [u8] = unsafe {
            core::slice::from_raw_parts_mut(
                raw.as_mut_ptr().cast::<u8>(),
                core::mem::size_of_val(&raw),
            )
        };
        let info = file
            .get_info::<FileInfo>(bytes)
            .map_err(|_| EspError::InfoFailed)?;
        Ok(info.file_size())
    }

    /// Loads the boot payload from the ESP: `config.toml` plus every
    /// `[guest.<id>]`'s OVMF CODE + VARS pair.
    ///
    /// Never fails the boot: anything missing or unreadable is logged and the
    /// corresponding payload field stays empty.
    #[must_use]
    pub fn load_esp_payload() -> EspPayload {
        let mut payload = EspPayload {
            config: None,
            guests: [GuestFirmware::empty(); MAX_BOOT_GUESTS],
            guest_count: 0,
            linux: LinuxPayload::EMPTY,
        };
        let mut loader = match EspLoader::open() {
            Ok(loader) => loader,
            Err(e) => {
                log::warn!(
                    "esp: cannot open the boot volume: {e:?}; skipping config + firmware load"
                );
                return payload;
            }
        };

        // 1. The hypervisor config.
        let mut path = [0u8; 128];
        let config_path_len = firmware::CONFIG_PATH.len();
        path[..config_path_len].copy_from_slice(firmware::CONFIG_PATH.as_bytes());
        match loader.read_file(&path[..config_path_len], MAX_CONFIG_BYTES) {
            Ok(file) => {
                log::info!(
                    "esp: staged {} ({} bytes) from the ESP",
                    firmware::CONFIG_PATH,
                    file.len
                );
                // SAFETY: the pages are live `LOADER_DATA` (never freed) and
                // `file.len` bytes were just read into them.
                let bytes =
                    unsafe { core::slice::from_raw_parts(file.base as *const u8, file.len) };
                collect_guest_ids(bytes, &mut payload);
                payload.config = Some(file);
            }
            Err(EspError::NotFound) => {
                log::warn!(
                    "esp: {} not present; booting without hypervisor config",
                    firmware::CONFIG_PATH
                );
            }
            Err(e) => {
                log::warn!("esp: failed to load {}: {e:?}", firmware::CONFIG_PATH);
            }
        }

        // 2. Each config guest's OVMF pair, located via the layout contract in
        // `crate::firmware` (the same paths `scripts/make-guest-esp.sh`
        // stages).
        let staged = payload.guest_count.min(MAX_BOOT_GUESTS);
        payload.guest_count = staged;
        for entry in payload.guests[..staged].iter_mut() {
            // Copy the id out so `id` borrows a local, not `entry`, which is
            // mutated below.
            let id_bytes = entry.id;
            let id_len = id_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(id_bytes.len());
            let id = core::str::from_utf8(&id_bytes[..id_len]).unwrap_or("?");
            load_guest_file(&mut loader, entry, id, FirmwareFile::Code);
            load_guest_file(&mut loader, entry, id, FirmwareFile::Vars);
        }

        // 3. An optional Linux guest: kernel, initrd, command line.
        payload.linux = load_linux_payload(&mut loader);
        payload
    }

    /// Stages the Linux guest files from `EFI/enlil/linux/`. Each file is
    /// optional; only a staged kernel makes the payload bootable.
    fn load_linux_payload(loader: &mut EspLoader) -> LinuxPayload {
        let kernel = stage_optional(loader, firmware::LINUX_KERNEL_PATH, MAX_LINUX_KERNEL_BYTES);
        if !kernel.is_staged() {
            return LinuxPayload::EMPTY;
        }
        LinuxPayload {
            kernel,
            initrd: stage_optional(loader, firmware::LINUX_INITRD_PATH, MAX_LINUX_INITRD_BYTES),
            cmdline: stage_optional(
                loader,
                firmware::LINUX_CMDLINE_PATH,
                MAX_LINUX_CMDLINE_BYTES,
            ),
            disk: stage_optional(loader, firmware::LINUX_DISK_PATH, MAX_LINUX_DISK_BYTES),
        }
    }

    /// Stages one optional ESP file, logging what happened. A missing file is
    /// not an error; an unreadable or oversized one is warned about.
    fn stage_optional(loader: &mut EspLoader, path: &str, max_bytes: u64) -> StagedFile {
        let mut buf = [0u8; 128];
        let bytes = path.as_bytes();
        let Some(dst) = buf.get_mut(..bytes.len()) else {
            log::warn!("esp: path too long: {path}");
            return StagedFile::EMPTY;
        };
        dst.copy_from_slice(bytes);
        match loader.read_file(&buf[..bytes.len()], max_bytes) {
            Ok(file) => {
                log::info!("esp: staged {path} ({} bytes)", file.len);
                StagedFile {
                    base: file.base,
                    len: file.len,
                }
            }
            Err(EspError::NotFound) => StagedFile::EMPTY,
            Err(e) => {
                log::warn!("esp: failed to load {path}: {e:?}");
                StagedFile::EMPTY
            }
        }
    }

    /// Records the distinct `[guest.<id>]` headers of `config` in `payload`.
    fn collect_guest_ids(config: &[u8], payload: &mut EspPayload) {
        let guests = &mut payload.guests;
        let mut count = 0usize;
        for_each_guest_id(config, &mut |id: &str| {
            if count >= MAX_BOOT_GUESTS {
                log::warn!("esp: more than {MAX_BOOT_GUESTS} guests; ignoring '{id}'");
                return;
            }
            if let Some(entry) = guests.get_mut(count) {
                entry.id[..id.len()].copy_from_slice(id.as_bytes());
                count += 1;
            }
        });
        payload.guest_count = count;
    }

    /// Stages one of a guest's OVMF files into `entry`, warning (not failing)
    /// when it is absent or unreadable.
    fn load_guest_file(
        loader: &mut EspLoader,
        entry: &mut GuestFirmware,
        id: &str,
        file: FirmwareFile,
    ) {
        let mut path = [0u8; 128];
        let Some(used) = firmware::guest_firmware_path_into(id, file, &mut path) else {
            log::warn!("esp: guest '{id}': cannot form a firmware path");
            return;
        };
        match loader.read_file(&path[..used], MAX_FIRMWARE_BYTES) {
            Ok(loaded) => {
                log::info!(
                    "esp: guest '{id}': staged {} ({} bytes)",
                    file.file_name(),
                    loaded.len
                );
                match file {
                    FirmwareFile::Code => {
                        entry.code_base = loaded.base;
                        entry.code_len = loaded.len;
                    }
                    FirmwareFile::Vars => {
                        entry.vars_base = loaded.base;
                        entry.vars_len = loaded.len;
                    }
                }
            }
            Err(EspError::NotFound) => {
                log::warn!("esp: guest '{id}': {} missing on the ESP", file.file_name());
            }
            Err(e) => {
                log::warn!("esp: guest '{id}': {} unreadable: {e:?}", file.file_name());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"# enlil hypervisor config\n\
        [guest.linux1]\n\
        cpus = 4\n\
        \n\
        [guest.linux1.firmware]\n\
        code = \"OVMF_CODE.fd\"\n\
        \n\
        [guest.win11] # trailing comment\n\
        cpus = 8\n\
        [guest.linux1] # duplicate\n\
        [guest.\"quoted\"]\n\
        [guest.bad/id]\n\
        [other.table]\n";

    fn collect(config: &[u8]) -> Vec<String> {
        let mut ids = Vec::new();
        for_each_guest_id(config, &mut |id: &str| ids.push(id.to_string()));
        ids
    }

    #[test]
    fn guest_scan_finds_direct_headers_only() {
        assert_eq!(collect(SAMPLE), vec!["linux1", "win11"]);
    }

    #[test]
    fn guest_scan_handles_whitespace_and_crlf() {
        let config = b"\r\n  [  guest.dev_box_2  ]  \r\n[guest.x]\r\n";
        assert_eq!(collect(config), vec!["dev_box_2", "x"]);
    }

    #[test]
    fn guest_scan_empty_config_yields_nothing() {
        assert_eq!(collect(b""), Vec::<String>::new());
        assert_eq!(
            collect(b"# just a comment\n[other]\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn uefi_path_codes_converts_separators_and_terminates() {
        let mut out = [0xFFFFu16; 64];
        let used = uefi_path_codes(b"EFI/enlil/config.toml", &mut out).unwrap();
        let codes: Vec<u16> = "EFI\\enlil\\config.toml".encode_utf16().collect();
        assert_eq!(&out[..used - 1], &codes[..]);
        assert_eq!(out[used - 1], 0);
        // Bytes past the NUL are untouched.
        assert_eq!(out[used], 0xFFFF);
    }

    #[test]
    fn uefi_path_codes_rejects_non_ascii_and_tiny_buffers() {
        let mut out = [0u16; 64];
        assert_eq!(uefi_path_codes(b"EFI/\xff/bad", &mut out), None);
        let mut tiny = [0u16; 4];
        assert_eq!(uefi_path_codes(b"EFI/enlil/config.toml", &mut tiny), None);
    }
}

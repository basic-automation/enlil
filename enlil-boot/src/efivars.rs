//! UEFI boot entries, written straight into a guest firmware's variable store.
//!
//! A guest running OVMF boots whatever its `BootOrder` names. A fresh variable
//! store has none, and OVMF then falls back to the removable-media path
//! (`\EFI\BOOT\BOOTX64.EFI`), which an installed Windows does not have: its
//! boot manager lives at `\EFI\Microsoft\Boot\bootmgfw.efi` and is reached only
//! through a boot entry. So the hypervisor writes that entry itself, before the
//! guest's first instruction: a `Boot####` variable holding an `EFI_LOAD_OPTION`
//! whose device path is the partition (by GPT identity, see [`crate::gpt`]) plus
//! the loader's file path, and a `BootOrder` that puts it first.
//!
//! The image is OVMF's `OVMF_VARS.fd`: a firmware volume holding one variable
//! store, in either the plain or the authenticated (Secure Boot) layout. Variables
//! are appended after the last one in the layout the firmware's own variable
//! driver reads (header, name, data, with no padding between them and the next
//! header aligned to four bytes); a superseded `BootOrder` is marked deleted the way the driver
//! marks it, not erased. Seeding is idempotent: an entry already present is
//! reused, so a store that is kept between boots does not collect duplicates.
//!
//! Pure and host-tested.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::gpt::Partition;

/// `EFI_GLOBAL_VARIABLE` (8BE4DF61-93CA-11D2-AA0D-00E098032B8C), on-disk byte order.
const GLOBAL_VARIABLE: [u8; 16] = [
    0x61, 0xDF, 0xE4, 0x8B, 0xCA, 0x93, 0xD2, 0x11, 0xAA, 0x0D, 0x00, 0xE0, 0x98, 0x03, 0x2B, 0x8C,
];
/// `gEfiAuthenticatedVariableGuid`: the store GUID of the authenticated layout.
const AUTH_STORE_GUID: [u8; 16] = [
    0x78, 0x2C, 0xF3, 0xAA, 0x7B, 0x94, 0x9A, 0x43, 0xA1, 0x80, 0x2E, 0x14, 0x4E, 0xC3, 0x77, 0x92,
];
/// `gEfiVariableGuid`: the store GUID of the plain layout.
const PLAIN_STORE_GUID: [u8; 16] = [
    0x16, 0x36, 0xCF, 0xDD, 0x75, 0x32, 0x64, 0x41, 0x98, 0xB6, 0xFE, 0x85, 0x70, 0x7F, 0xFE, 0x7D,
];

/// Offset of the `_FVH` signature in a firmware-volume header.
const FV_SIGNATURE_AT: usize = 40;
/// Offset of the header's own length (`u16`).
const FV_HEADER_LEN_AT: usize = 48;
/// Bytes of a `VARIABLE_STORE_HEADER`.
const STORE_HEADER_BYTES: usize = 28;
/// `VARIABLE_STORE_FORMATTED`.
const STORE_FORMATTED: u8 = 0x5A;
/// `VARIABLE_STORE_HEALTHY`.
const STORE_HEALTHY: u8 = 0xFE;

/// The start marker of every variable header (`0x55AA`).
const VAR_START_ID: u16 = 0x55AA;
/// `VAR_ADDED`: a complete, live variable.
const VAR_ADDED: u8 = 0x3F;
/// `VAR_IN_DELETED_TRANSITION` applied to a live variable: still live, a
/// deletion was under way.
const VAR_ADDED_IN_TRANSITION: u8 = VAR_ADDED & 0xFE;
/// Masked into a live variable's state to delete it (`VAR_DELETED`).
const VAR_DELETED: u8 = 0xFD;
/// Variables are 4-byte aligned.
const ALIGN: usize = 4;
/// `NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS`.
const ATTRIBUTES: u32 = 0x07;
/// `LOAD_OPTION_ACTIVE`.
const LOAD_OPTION_ACTIVE: u32 = 1;
/// The erased state of flash.
const ERASED: u8 = 0xFF;

/// What to boot: a loader file on a GPT partition.
#[derive(Debug, Clone, Copy)]
pub struct BootEntry<'a> {
    /// The name the firmware's boot menu shows.
    pub description: &'a str,
    /// The partition holding the loader (its GPT identity goes in the path).
    pub partition: Partition,
    /// The loader's path on that partition, e.g. `\EFI\Microsoft\Boot\bootmgfw.efi`
    /// (forward slashes are accepted and converted).
    pub loader: &'a str,
}

/// Why a boot entry could not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedError {
    /// The image does not start with a firmware-volume header.
    NotAFirmwareVolume,
    /// The volume does not hold a formatted, healthy variable store.
    NotAVariableStore,
    /// A variable header runs past the store: it cannot be trusted.
    Corrupt,
    /// The store has no room for the new variables.
    Full,
    /// The description or loader path is too long for a load option.
    TooLong,
}

/// `s` as little-endian UTF-16 with a terminating NUL.
fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(core::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// The device path of `loader` on `partition`, in the firmware's "short form".
///
/// A hard-drive node, a file-path node and the end node: the path starts at the
/// partition, so the firmware finds the disk by matching the partition's GUID
/// wherever the controller sits.
///
/// # Errors
///
/// [`SeedError::TooLong`] if the file-path node would not fit its 16-bit length.
pub fn device_path(partition: &Partition, loader: &str) -> Result<Vec<u8>, SeedError> {
    let mut dp = Vec::new();
    // Media / Hard Drive: number, start, size, signature, MBR type 2 (GPT),
    // signature type 2 (GUID).
    dp.extend_from_slice(&[0x04, 0x01, 42, 0]);
    dp.extend_from_slice(&partition.number.to_le_bytes());
    dp.extend_from_slice(&partition.first_lba.to_le_bytes());
    dp.extend_from_slice(&partition.sectors().to_le_bytes());
    dp.extend_from_slice(&partition.unique_guid);
    dp.extend_from_slice(&[2, 2]);
    // Media / File Path: the loader, with backslash separators and a leading one.
    let mut path = loader.replace('/', "\\");
    if !path.starts_with('\\') {
        path.insert(0, '\\');
    }
    let name = utf16z(&path);
    let node = u16::try_from(4 + name.len()).map_err(|_| SeedError::TooLong)?;
    dp.extend_from_slice(&[0x04, 0x04]);
    dp.extend_from_slice(&node.to_le_bytes());
    dp.extend_from_slice(&name);
    // End of the entire device path.
    dp.extend_from_slice(&[0x7F, 0xFF, 0x04, 0x00]);
    Ok(dp)
}

/// An `EFI_LOAD_OPTION`: attributes, the path-list length, the description and
/// the path list.
///
/// # Errors
///
/// [`SeedError::TooLong`] if the path list does not fit its 16-bit length.
pub fn load_option(entry: &BootEntry<'_>) -> Result<Vec<u8>, SeedError> {
    let dp = device_path(&entry.partition, entry.loader)?;
    let mut o = Vec::new();
    o.extend_from_slice(&LOAD_OPTION_ACTIVE.to_le_bytes());
    o.extend_from_slice(
        &u16::try_from(dp.len())
            .map_err(|_| SeedError::TooLong)?
            .to_le_bytes(),
    );
    o.extend_from_slice(&utf16z(entry.description));
    o.extend_from_slice(&dp);
    Ok(o)
}

/// Round `n` up to the variable alignment.
const fn align(n: usize) -> usize {
    n.next_multiple_of(ALIGN)
}

/// Where the store is and which layout it uses.
#[derive(Debug, Clone, Copy)]
struct Store {
    /// Offset of the first variable.
    first: usize,
    /// Offset one past the store.
    end: usize,
    /// The authenticated layout (60-byte variable headers) instead of the plain one (32).
    auth: bool,
}

impl Store {
    /// Locate the variable store in an `OVMF_VARS.fd` image.
    fn locate(image: &[u8]) -> Result<Self, SeedError> {
        if image.get(FV_SIGNATURE_AT..FV_SIGNATURE_AT + 4) != Some(b"_FVH") {
            return Err(SeedError::NotAFirmwareVolume);
        }
        let header = usize::from(u16::from_le_bytes(
            image
                .get(FV_HEADER_LEN_AT..FV_HEADER_LEN_AT + 2)
                .and_then(|b| b.try_into().ok())
                .ok_or(SeedError::NotAFirmwareVolume)?,
        ));
        let store = image
            .get(header..header + STORE_HEADER_BYTES)
            .ok_or(SeedError::NotAVariableStore)?;
        let guid: [u8; 16] = store[0..16].try_into().map_err(|_| SeedError::Corrupt)?;
        let auth = if guid == AUTH_STORE_GUID {
            true
        } else if guid == PLAIN_STORE_GUID {
            false
        } else {
            return Err(SeedError::NotAVariableStore);
        };
        if store[20] != STORE_FORMATTED || store[21] != STORE_HEALTHY {
            return Err(SeedError::NotAVariableStore);
        }
        let size = usize::try_from(u32::from_le_bytes(
            store[16..20].try_into().map_err(|_| SeedError::Corrupt)?,
        ))
        .map_err(|_| SeedError::Corrupt)?;
        let end = header + size;
        if size < STORE_HEADER_BYTES || end > image.len() {
            return Err(SeedError::NotAVariableStore);
        }
        Ok(Self {
            first: header + STORE_HEADER_BYTES,
            end,
            auth,
        })
    }

    /// Bytes of a variable header.
    const fn header_bytes(self) -> usize {
        if self.auth { 60 } else { 32 }
    }

    /// Offsets of the name size, data size and vendor GUID in a header.
    const fn fields(self) -> (usize, usize, usize) {
        if self.auth { (36, 40, 44) } else { (8, 12, 16) }
    }
}

/// A variable found in the store.
struct Var {
    /// Offset of its header.
    at: usize,
    state: u8,
    name: String,
    guid: [u8; 16],
    /// Offset and length of its data.
    data: (usize, usize),
}

impl Var {
    /// Whether the firmware still treats it as a variable.
    const fn live(&self) -> bool {
        self.state == VAR_ADDED || self.state == VAR_ADDED_IN_TRANSITION
    }
}

fn le32_at(image: &[u8], at: usize) -> Result<usize, SeedError> {
    let b = image.get(at..at + 4).ok_or(SeedError::Corrupt)?;
    let v = u32::from_le_bytes(b.try_into().map_err(|_| SeedError::Corrupt)?);
    usize::try_from(v).map_err(|_| SeedError::Corrupt)
}

/// Walk the store's variables; returns them and the offset where free space
/// (the first byte that does not start a variable) begins.
fn walk(image: &[u8], store: Store) -> Result<(Vec<Var>, usize), SeedError> {
    let (name_at, data_at, guid_at) = store.fields();
    let mut vars = Vec::new();
    let mut at = store.first;
    while at + store.header_bytes() <= store.end {
        if image[at..at + 2] != VAR_START_ID.to_le_bytes() {
            break;
        }
        let name_size = le32_at(image, at + name_at)?;
        let data_size = le32_at(image, at + data_at)?;
        // The name and data follow the header back to back (the firmware driver
        // pads neither on x86); only the *next header* is aligned.
        let name_off = at + store.header_bytes();
        let data_off = name_off.checked_add(name_size).ok_or(SeedError::Corrupt)?;
        let next = data_off
            .checked_add(data_size)
            .map(align)
            .ok_or(SeedError::Corrupt)?;
        if next > store.end || name_off + name_size > store.end {
            return Err(SeedError::Corrupt);
        }
        let units: Vec<u16> = image[name_off..name_off + name_size]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|&u| u != 0)
            .collect();
        vars.push(Var {
            at,
            state: image[at + 2],
            name: char::decode_utf16(units)
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
            guid: image[at + guid_at..at + guid_at + 16]
                .try_into()
                .map_err(|_| SeedError::Corrupt)?,
            data: (data_off, data_size),
        });
        at = next;
    }
    Ok((vars, at))
}

/// One complete variable, encoded as the firmware's variable driver stores it.
fn encode_variable(store: Store, name: &str, guid: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let name = utf16z(name);
    let (name_at, data_at, guid_at) = store.fields();
    let mut v = alloc::vec![ERASED; store.header_bytes()];
    v[0..2].copy_from_slice(&VAR_START_ID.to_le_bytes());
    v[2] = VAR_ADDED;
    v[3] = 0;
    v[4..8].copy_from_slice(&ATTRIBUTES.to_le_bytes());
    if store.auth {
        // Monotonic count, time stamp and public-key index: not authenticated.
        v[8..36].fill(0);
    }
    v[name_at..name_at + 4]
        .copy_from_slice(&u32::try_from(name.len()).unwrap_or(u32::MAX).to_le_bytes());
    v[data_at..data_at + 4]
        .copy_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes());
    v[guid_at..guid_at + 16].copy_from_slice(guid);
    // Name and data back to back; the padding that aligns the next header is
    // left erased, as the driver leaves it.
    v.extend_from_slice(&name);
    v.extend_from_slice(data);
    v.resize(align(v.len()), ERASED);
    v
}

/// The number in a `Boot####` variable name.
fn boot_number(name: &str) -> Option<u16> {
    let digits = name.strip_prefix("Boot")?;
    (digits.len() == 4 && digits.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| u16::from_str_radix(digits, 16).ok())
        .flatten()
}

/// Add `entry` to the variable store in `image` as the first boot option,
/// returning its `Boot####` number.
///
/// An entry already in the store with identical contents is reused (moved to the
/// front of `BootOrder` if it is not there). Otherwise the lowest free number is
/// taken; the previous `BootOrder` is marked deleted and a new one appended
/// with the entry first and the old order after it. Nothing is written unless
/// every variable fits.
///
/// # Errors
///
/// See [`SeedError`].
pub fn seed_boot_entry(image: &mut [u8], entry: &BootEntry<'_>) -> Result<u16, SeedError> {
    let store = Store::locate(image)?;
    let (vars, tail) = walk(image, store)?;
    let option = load_option(entry)?;

    let live_global = |v: &&Var| v.live() && v.guid == GLOBAL_VARIABLE;
    let data_of = |v: &Var| &image[v.data.0..v.data.0 + v.data.1];

    let order_var = vars
        .iter()
        .filter(live_global)
        .find(|v| v.name == "BootOrder");
    let order: Vec<u16> = order_var.map_or_else(Vec::new, |v| {
        data_of(v)
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect()
    });

    // The new variables, and which live ones they supersede.
    let existing = vars
        .iter()
        .filter(live_global)
        .filter_map(|v| Some((boot_number(&v.name)?, v)))
        .find(|(_, v)| data_of(v) == option.as_slice())
        .map(|(n, _)| n);
    let mut pending: Vec<Vec<u8>> = Vec::new();
    let number = if let Some(n) = existing {
        n
    } else {
        let used: Vec<u16> = vars
            .iter()
            .filter(live_global)
            .filter_map(|v| boot_number(&v.name))
            .collect();
        let n = (0..=u16::MAX)
            .find(|n| !used.contains(n))
            .ok_or(SeedError::Full)?;
        pending.push(encode_variable(
            store,
            &alloc::format!("Boot{n:04X}"),
            &GLOBAL_VARIABLE,
            &option,
        ));
        n
    };
    let rewrites_order = order.first() != Some(&number);
    if rewrites_order {
        let new_order: Vec<u8> = core::iter::once(number)
            .chain(order.iter().copied().filter(|&o| o != number))
            .flat_map(u16::to_le_bytes)
            .collect();
        pending.push(encode_variable(
            store,
            "BootOrder",
            &GLOBAL_VARIABLE,
            &new_order,
        ));
    }
    if pending.is_empty() {
        return Ok(number);
    }

    let total: usize = pending.iter().map(Vec::len).sum();
    let room = image.get(tail..store.end).ok_or(SeedError::Corrupt)?;
    if total > room.len() {
        return Err(SeedError::Full);
    }
    if room[..total].iter().any(|&b| b != ERASED) {
        // Free space that is not erased: a variable header we did not recognise.
        return Err(SeedError::Corrupt);
    }

    // Commit: retire the old order, then append.
    if rewrites_order && let Some(v) = order_var {
        image[v.at + 2] &= VAR_DELETED;
    }
    let mut at = tail;
    for item in pending {
        image[at..at + item.len()].copy_from_slice(&item);
        at += item.len();
    }
    Ok(number)
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // small, bounded test values
mod tests {
    use super::*;

    const STORE_AT: usize = 72;

    /// An `OVMF_VARS.fd`-shaped image: a 72-byte firmware-volume header, a store
    /// of `store_bytes` (header included) and a trailing region of erased flash
    /// standing in for the fault-tolerant-write space.
    fn blank_image(auth: bool, store_bytes: usize) -> Vec<u8> {
        let mut img = alloc::vec![ERASED; STORE_AT + store_bytes + 4096];
        img[..STORE_AT].fill(0);
        img[FV_SIGNATURE_AT..FV_SIGNATURE_AT + 4].copy_from_slice(b"_FVH");
        img[FV_HEADER_LEN_AT..FV_HEADER_LEN_AT + 2].copy_from_slice(&72u16.to_le_bytes());
        let guid = if auth {
            AUTH_STORE_GUID
        } else {
            PLAIN_STORE_GUID
        };
        let s = &mut img[STORE_AT..STORE_AT + STORE_HEADER_BYTES];
        s[0..16].copy_from_slice(&guid);
        s[16..20].copy_from_slice(&(store_bytes as u32).to_le_bytes());
        s[20] = STORE_FORMATTED;
        s[21] = STORE_HEALTHY;
        s[22..28].fill(0);
        img
    }

    fn partition(number: u32, first: u64, last: u64, seed: u8) -> Partition {
        Partition {
            number,
            type_guid: [0xAA; 16],
            unique_guid: core::array::from_fn(|i| seed.wrapping_add(i as u8)),
            first_lba: first,
            last_lba: last,
        }
    }

    fn windows() -> BootEntry<'static> {
        BootEntry {
            description: "Windows Boot Manager",
            partition: partition(1, 2048, 206_847, 0x10),
            loader: "\\EFI\\Microsoft\\Boot\\bootmgfw.efi",
        }
    }

    fn linux() -> BootEntry<'static> {
        BootEntry {
            description: "Linux",
            partition: partition(5, 4_000_000, 4_999_999, 0x40),
            loader: "/EFI/limine/limine.efi",
        }
    }

    /// An independent reader written from the firmware driver's own pointer
    /// arithmetic (`GET_PAD_SIZE` is zero on x86; only the next header aligns).
    fn read_all(img: &[u8]) -> Vec<(String, [u8; 16], u8, Vec<u8>)> {
        let hdr = 72usize;
        let auth = img[hdr..hdr + 16] == AUTH_STORE_GUID;
        let end = hdr + u32::from_le_bytes(img[hdr + 16..hdr + 20].try_into().unwrap()) as usize;
        let (head, ns, ds, gs) = if auth {
            (60, 36, 40, 44)
        } else {
            (32, 8, 12, 16)
        };
        let rd = |at: usize| u32::from_le_bytes(img[at..at + 4].try_into().unwrap()) as usize;
        let mut out = Vec::new();
        let mut at = hdr + STORE_HEADER_BYTES;
        while at + head <= end && img[at] == 0xAA && img[at + 1] == 0x55 {
            let (nsz, dsz) = (rd(at + ns), rd(at + ds));
            let name_at = at + head;
            let data_at = name_at + nsz;
            let units: Vec<u16> = img[name_at..name_at + nsz]
                .chunks(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0)
                .collect();
            out.push((
                String::from_utf16(&units).unwrap(),
                img[at + gs..at + gs + 16].try_into().unwrap(),
                img[at + 2],
                img[data_at..data_at + dsz].to_vec(),
            ));
            at = (data_at + dsz + 3) & !3;
        }
        out
    }

    fn live<'a>(
        all: &'a [(String, [u8; 16], u8, Vec<u8>)],
        name: &str,
    ) -> Vec<&'a (String, [u8; 16], u8, Vec<u8>)> {
        all.iter()
            .filter(|v| v.0 == name && (v.2 == VAR_ADDED || v.2 == VAR_ADDED_IN_TRANSITION))
            .collect()
    }

    fn order(img: &[u8]) -> Vec<u16> {
        let all = read_all(img);
        let o = live(&all, "BootOrder");
        assert_eq!(o.len(), 1, "exactly one live BootOrder");
        o[0].3
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    #[test]
    fn the_device_path_names_the_partition_and_the_loader() {
        let dp = device_path(&windows().partition, windows().loader).unwrap();
        // Hard-drive node.
        assert_eq!(&dp[0..4], &[0x04, 0x01, 42, 0]);
        assert_eq!(&dp[4..8], &1u32.to_le_bytes(), "partition number");
        assert_eq!(&dp[8..16], &2048u64.to_le_bytes(), "start");
        assert_eq!(&dp[16..24], &204_800u64.to_le_bytes(), "size in sectors");
        assert_eq!(&dp[24..40], &windows().partition.unique_guid, "signature");
        assert_eq!(&dp[40..42], &[2, 2], "GPT, GUID signature");
        // File-path node.
        let name = utf16z("\\EFI\\Microsoft\\Boot\\bootmgfw.efi");
        assert_eq!(&dp[42..44], &[0x04, 0x04]);
        assert_eq!(
            usize::from(u16::from_le_bytes([dp[44], dp[45]])),
            4 + name.len()
        );
        assert_eq!(&dp[46..46 + name.len()], name.as_slice());
        // End node.
        assert_eq!(&dp[46 + name.len()..], &[0x7F, 0xFF, 0x04, 0x00]);
    }

    #[test]
    fn slashes_are_converted_and_a_leading_separator_added() {
        let a = device_path(&linux().partition, "/EFI/limine/limine.efi").unwrap();
        let b = device_path(&linux().partition, "EFI\\limine\\limine.efi").unwrap();
        assert_eq!(a, b);
        assert!(a.windows(2).any(|w| w == [b'\\', 0]));
        assert!(!a.windows(2).any(|w| w == [b'/', 0]));
    }

    #[test]
    fn a_load_option_carries_attributes_length_description_and_path() {
        let o = load_option(&windows()).unwrap();
        let dp = device_path(&windows().partition, windows().loader).unwrap();
        assert_eq!(&o[0..4], &1u32.to_le_bytes(), "LOAD_OPTION_ACTIVE");
        assert_eq!(usize::from(u16::from_le_bytes([o[4], o[5]])), dp.len());
        let desc = utf16z("Windows Boot Manager");
        assert_eq!(&o[6..6 + desc.len()], desc.as_slice());
        assert_eq!(&o[6 + desc.len()..], dp.as_slice());
    }

    #[test]
    fn a_loader_path_too_long_for_a_node_is_refused() {
        let long = "a".repeat(40_000);
        assert_eq!(
            device_path(&windows().partition, &long),
            Err(SeedError::TooLong)
        );
    }

    fn seeds_a_blank_store(auth: bool) {
        let mut img = blank_image(auth, 0x4000);
        let before = img.clone();
        assert_eq!(seed_boot_entry(&mut img, &windows()), Ok(0));

        let all = read_all(&img);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "Boot0000");
        assert_eq!(all[1].0, "BootOrder");
        for v in &all {
            assert_eq!(v.1, GLOBAL_VARIABLE);
            assert_eq!(v.2, VAR_ADDED);
        }
        assert_eq!(all[0].3, load_option(&windows()).unwrap());
        assert_eq!(order(&img), [0]);
        // The attributes are non-volatile + boot-service + runtime.
        let first = STORE_AT + STORE_HEADER_BYTES;
        assert_eq!(&img[first..first + 2], &[0xAA, 0x55]);
        assert_eq!(&img[first + 4..first + 8], &7u32.to_le_bytes());
        // Nothing outside the store, and nothing before the first variable, moved.
        assert_eq!(&img[..first], &before[..first]);
        assert_eq!(&img[STORE_AT + 0x4000..], &before[STORE_AT + 0x4000..]);
    }

    #[test]
    fn a_blank_authenticated_store_gets_the_entry() {
        seeds_a_blank_store(true);
    }

    #[test]
    fn a_blank_plain_store_gets_the_entry() {
        seeds_a_blank_store(false);
    }

    #[test]
    fn a_second_entry_goes_first_and_the_old_order_is_retired() {
        let mut img = blank_image(true, 0x4000);
        assert_eq!(seed_boot_entry(&mut img, &windows()), Ok(0));
        assert_eq!(seed_boot_entry(&mut img, &linux()), Ok(1));
        assert_eq!(order(&img), [1, 0]);

        let all = read_all(&img);
        let names: Vec<&str> = all.iter().map(|v| v.0.as_str()).collect();
        assert_eq!(names, ["Boot0000", "BootOrder", "Boot0001", "BootOrder"]);
        assert_eq!(all[1].2, VAR_ADDED & VAR_DELETED, "old order deleted");
        assert_eq!(live(&all, "Boot0000").len(), 1);
        assert_eq!(live(&all, "Boot0001")[0].3, load_option(&linux()).unwrap());
    }

    #[test]
    fn seeding_the_same_entry_again_changes_nothing() {
        let mut img = blank_image(true, 0x4000);
        seed_boot_entry(&mut img, &windows()).unwrap();
        let once = img.clone();
        assert_eq!(seed_boot_entry(&mut img, &windows()), Ok(0));
        assert_eq!(img, once);
    }

    #[test]
    fn an_existing_entry_is_moved_to_the_front_not_duplicated() {
        let mut img = blank_image(false, 0x4000);
        seed_boot_entry(&mut img, &windows()).unwrap();
        seed_boot_entry(&mut img, &linux()).unwrap();
        assert_eq!(order(&img), [1, 0]);
        // Windows again: reuses Boot0000, only the order is rewritten.
        assert_eq!(seed_boot_entry(&mut img, &windows()), Ok(0));
        assert_eq!(order(&img), [0, 1]);
        let all = read_all(&img);
        assert_eq!(live(&all, "Boot0000").len(), 1);
        assert_eq!(live(&all, "Boot0001").len(), 1);
        assert_eq!(all.iter().filter(|v| v.0.starts_with("Boot0")).count(), 2);
    }

    #[test]
    fn other_variables_are_left_alone_and_numbers_skip_those_in_use() {
        let mut img = blank_image(true, 0x4000);
        let store = Store::locate(&img).unwrap();
        // A foreign variable and a pre-existing Boot0000 and Boot0002.
        let mut at = store.first;
        for (name, data) in [
            ("Timeout", &[5u8, 0][..]),
            ("Boot0000", &[1, 2, 3][..]),
            ("Boot0002", &[4, 5][..]),
            ("BootOrder", &[2, 0, 0, 0][..]),
        ] {
            let v = encode_variable(store, name, &GLOBAL_VARIABLE, data);
            img[at..at + v.len()].copy_from_slice(&v);
            at += v.len();
        }
        assert_eq!(seed_boot_entry(&mut img, &windows()), Ok(1));
        assert_eq!(order(&img), [1, 2, 0], "new entry first, old order kept");
        let all = read_all(&img);
        assert_eq!(live(&all, "Timeout")[0].3, [5, 0]);
        assert_eq!(live(&all, "Boot0000")[0].3, [1, 2, 3]);
        assert_eq!(live(&all, "Boot0002")[0].3, [4, 5]);
    }

    #[test]
    fn variables_stay_four_byte_aligned() {
        let mut img = blank_image(true, 0x4000);
        seed_boot_entry(&mut img, &windows()).unwrap();
        seed_boot_entry(&mut img, &linux()).unwrap();
        let store = Store::locate(&img).unwrap();
        let (_, tail) = walk(&img, store).unwrap();
        assert_eq!(tail % ALIGN, 0);
        assert!(img[tail..store.end].iter().all(|&b| b == ERASED));
    }

    #[test]
    fn images_that_are_not_variable_stores_are_refused() {
        let mut blank = alloc::vec![0u8; 4096];
        assert_eq!(
            seed_boot_entry(&mut blank, &windows()),
            Err(SeedError::NotAFirmwareVolume)
        );
        let mut wrong_guid = blank_image(true, 0x4000);
        wrong_guid[STORE_AT] ^= 1;
        assert_eq!(
            seed_boot_entry(&mut wrong_guid, &windows()),
            Err(SeedError::NotAVariableStore)
        );
        let mut unformatted = blank_image(true, 0x4000);
        unformatted[STORE_AT + 20] = 0xFF;
        assert_eq!(
            seed_boot_entry(&mut unformatted, &windows()),
            Err(SeedError::NotAVariableStore)
        );
        let mut too_big = blank_image(true, 0x4000);
        too_big[STORE_AT + 16..STORE_AT + 20].copy_from_slice(&0x0100_0000u32.to_le_bytes());
        assert_eq!(
            seed_boot_entry(&mut too_big, &windows()),
            Err(SeedError::NotAVariableStore)
        );
    }

    #[test]
    fn a_full_store_is_refused_without_writing() {
        // Room for the header and a few bytes only.
        let mut img = blank_image(true, STORE_HEADER_BYTES + 64);
        let before = img.clone();
        assert_eq!(seed_boot_entry(&mut img, &windows()), Err(SeedError::Full));
        assert_eq!(img, before);
    }

    #[test]
    fn a_corrupt_variable_header_is_refused_without_writing() {
        let mut img = blank_image(true, 0x4000);
        seed_boot_entry(&mut img, &windows()).unwrap();
        // Make the first variable's name size absurd.
        let first = STORE_AT + STORE_HEADER_BYTES;
        img[first + 36..first + 40].copy_from_slice(&0x00FF_FFFFu32.to_le_bytes());
        let before = img.clone();
        assert_eq!(seed_boot_entry(&mut img, &linux()), Err(SeedError::Corrupt));
        assert_eq!(img, before);
    }

    #[test]
    fn unerased_free_space_is_refused_without_writing() {
        let mut img = blank_image(true, 0x4000);
        // Junk just past where the variables would land, not a variable start.
        let first = STORE_AT + STORE_HEADER_BYTES;
        img[first] = 0x12;
        let before = img.clone();
        assert_eq!(
            seed_boot_entry(&mut img, &windows()),
            Err(SeedError::Corrupt)
        );
        assert_eq!(img, before);
    }
}

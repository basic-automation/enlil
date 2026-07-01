//! FPDT (Firmware Performance Data Table) builder — ACPI §5.2.23.
//!
//! The FPDT records firmware boot-performance timestamps. It is emitted by
//! essentially every UEFI firmware (it is vendor-neutral and CPU-vendor-neutral,
//! unlike AMD's `IVRS`/`CRAT` or Intel's `DMAR`), so its absence from a table
//! set that otherwise looks like a real board is a fidelity gap. Windows reads
//! it for boot-performance telemetry (the `Microsoft-Windows-Boot` ETW events).
//!
//! Structure: the FPDT itself (a normal XSDT-referenced SDT) carries a single
//! **Firmware Basic Boot Performance Pointer** record (type 0) whose 64-bit
//! pointer targets a separate **FBPT** (Firmware Basic Boot Performance Table)
//! blob elsewhere in memory — exactly the FADT→FACS shape. The FBPT holds the
//! **Firmware Basic Boot Performance Data** record (type 2) with the five boot
//! timestamps.

use super::tables::{AcpiSdtHeader, OemInfo};

/// Length of the Firmware Basic Boot Performance Pointer record (type 0).
const FBBP_POINTER_RECORD_LEN: u8 = 16;
/// Total FPDT length: 36-byte SDT header + one 16-byte pointer record. Fixed, so
/// the table-set layout can reserve the slot before the FBPT address is known.
pub const FPDT_LENGTH: u32 = 36 + FBBP_POINTER_RECORD_LEN as u32;
/// Byte offset of the 8-byte FBPT pointer field within the FPDT.
///
/// It follows the 36-byte SDT header and the record's type/len/rev/reserved
/// prefix. The table-set builder relocates this field, so it is exported for
/// the relocation map.
pub const FPDT_FBPT_POINTER_OFFSET: usize = 36 + 8;

/// Length of the Firmware Basic Boot Performance Data record (type 2).
const FBBP_DATA_RECORD_LEN: u8 = 48;
/// Total FBPT length: 8-byte "FBPT" header + one 48-byte data record.
const FBPT_LENGTH: u32 = 8 + FBBP_DATA_RECORD_LEN as u32;

/// FPDT table builder.
pub struct FpdtBuilder {
    oem: OemInfo,
    fbpt_address: u64,
}

impl FpdtBuilder {
    /// Create an FPDT builder. `fbpt_address` is the guest-physical address of
    /// the FBPT blob (relocated by the firmware table-loader like FADT→FACS).
    #[must_use]
    pub fn new() -> Self {
        Self {
            oem: OemInfo::default(),
            fbpt_address: 0,
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    /// Set the guest-physical address the Firmware Basic Boot Performance
    /// Pointer record points at (the FBPT).
    #[must_use]
    pub const fn fbpt_address(mut self, addr: u64) -> Self {
        self.fbpt_address = addr;
        self
    }

    /// Build the FPDT table as a byte vector (52 bytes).
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FPDT_LENGTH as usize);

        let header = AcpiSdtHeader::new(*b"FPDT", FPDT_LENGTH, 1, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Firmware Basic Boot Performance Pointer record (type 0x0000).
        buf.extend_from_slice(&0u16.to_le_bytes()); // Performance Record Type
        buf.push(FBBP_POINTER_RECORD_LEN); // Record Length
        buf.push(1); // Revision
        buf.extend_from_slice(&0u32.to_le_bytes()); // Reserved
        debug_assert_eq!(buf.len(), FPDT_FBPT_POINTER_OFFSET);
        buf.extend_from_slice(&self.fbpt_address.to_le_bytes()); // FBPT pointer

        // Fix up checksum (offset 9): all bytes must sum to 0 mod 256.
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for FpdtBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// FBPT (Firmware Basic Boot Performance Table) blob builder.
///
/// This is the target of the FPDT's pointer record, not an XSDT entry. It has
/// its own 8-byte "FBPT" header (signature + length, no checksum byte) and one
/// Firmware Basic Boot Performance Data record. Timestamps are in nanoseconds
/// (ACPI §5.2.23.4); the defaults describe a plausible ~1.6 s UEFI boot.
pub struct FbptBuilder {
    /// All timestamps are in nanoseconds (ACPI §5.2.23.4).
    reset_end: u64,
    load_image_start: u64,
    start_image_start: u64,
    exit_boot_services_entry: u64,
    exit_boot_services_exit: u64,
}

impl FbptBuilder {
    /// Create an FBPT builder with a plausible monotonically-increasing boot
    /// timeline (nanosecond timestamps).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            reset_end: 32_000_000,            // 32 ms after reset deassert
            load_image_start: 1_200_000_000,  // OS loader image load
            start_image_start: 1_260_000_000, // OS loader image start
            exit_boot_services_entry: 1_600_000_000,
            exit_boot_services_exit: 1_620_000_000,
        }
    }

    /// Build the FBPT blob as a byte vector (56 bytes).
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FBPT_LENGTH as usize);

        // FBPT header: signature + length (no OEM fields, no checksum byte).
        buf.extend_from_slice(b"FBPT");
        buf.extend_from_slice(&FBPT_LENGTH.to_le_bytes());

        // Firmware Basic Boot Performance Data record (type 0x0002).
        buf.extend_from_slice(&0x0002u16.to_le_bytes()); // Performance Record Type
        buf.push(FBBP_DATA_RECORD_LEN); // Record Length
        buf.push(2); // Revision
        buf.extend_from_slice(&0u32.to_le_bytes()); // Reserved
        buf.extend_from_slice(&self.reset_end.to_le_bytes());
        buf.extend_from_slice(&self.load_image_start.to_le_bytes());
        buf.extend_from_slice(&self.start_image_start.to_le_bytes());
        buf.extend_from_slice(&self.exit_boot_services_entry.to_le_bytes());
        buf.extend_from_slice(&self.exit_boot_services_exit.to_le_bytes());

        debug_assert_eq!(buf.len(), FBPT_LENGTH as usize);
        buf
    }
}

impl Default for FbptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fpdt_builds_with_signature_and_length() {
        let fpdt = FpdtBuilder::new().build();
        assert_eq!(fpdt.len(), 52);
        assert_eq!(&fpdt[0..4], b"FPDT");
        assert_eq!(u32::from_le_bytes(fpdt[4..8].try_into().unwrap()), 52);
        assert_eq!(fpdt[8], 1); // revision
    }

    #[test]
    fn fpdt_checksum_is_zero() {
        let fpdt = FpdtBuilder::new().fbpt_address(0xF000_1234).build();
        let sum: u8 = fpdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn fpdt_pointer_record_header_is_type0_len16_rev1() {
        let fpdt = FpdtBuilder::new().build();
        assert_eq!(u16::from_le_bytes(fpdt[36..38].try_into().unwrap()), 0); // type
        assert_eq!(fpdt[38], 16); // record length
        assert_eq!(fpdt[39], 1); // revision
        assert_eq!(u32::from_le_bytes(fpdt[40..44].try_into().unwrap()), 0); // reserved
    }

    #[test]
    fn fpdt_fbpt_pointer_is_stored_at_the_exported_offset() {
        let addr = 0x1234_5678_9ABC_DEF0u64;
        let fpdt = FpdtBuilder::new().fbpt_address(addr).build();
        assert_eq!(FPDT_FBPT_POINTER_OFFSET, 44);
        let stored = u64::from_le_bytes(
            fpdt[FPDT_FBPT_POINTER_OFFSET..FPDT_FBPT_POINTER_OFFSET + 8]
                .try_into()
                .unwrap(),
        );
        assert_eq!(stored, addr);
    }

    #[test]
    fn fbpt_builds_with_signature_length_and_data_record() {
        let fbpt = FbptBuilder::new().build();
        assert_eq!(fbpt.len(), 56);
        assert_eq!(&fbpt[0..4], b"FBPT");
        assert_eq!(u32::from_le_bytes(fbpt[4..8].try_into().unwrap()), 56);
        // Data record header: type 2, len 48, rev 2.
        assert_eq!(u16::from_le_bytes(fbpt[8..10].try_into().unwrap()), 2);
        assert_eq!(fbpt[10], 48);
        assert_eq!(fbpt[11], 2);
    }

    #[test]
    fn fbpt_timestamps_are_monotonically_increasing() {
        let fbpt = FbptBuilder::new().build();
        let ts = |o: usize| u64::from_le_bytes(fbpt[o..o + 8].try_into().unwrap());
        let reset_end = ts(16);
        let load_start = ts(24);
        let start_start = ts(32);
        let ebs_entry = ts(40);
        let ebs_exit = ts(48);
        assert!(reset_end < load_start);
        assert!(load_start < start_start);
        assert!(start_start < ebs_entry);
        assert!(ebs_entry < ebs_exit);
    }
}

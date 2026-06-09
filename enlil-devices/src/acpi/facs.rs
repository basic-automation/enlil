//! FACS (Firmware ACPI Control Structure) builder
//!
//! The FACS is the one ACPI structure the OS and firmware *share* read/write: it
//! holds the firmware waking vector used to resume from S3, the hardware
//! signature OSPM compares across a resume, and the ACPI global lock used to
//! arbitrate access to hardware shared between OSPM and firmware (SMM). It is
//! referenced only through the FADT's `FIRMWARE_CTRL` / `X_FIRMWARE_CTRL`
//! fields — never from the XSDT — and, uniquely among ACPI tables, it has **no
//! standard SDT header and no checksum** (ACPI 6.x §5.2.10).
//!
//! Every real PC firmware publishes a FACS; a FADT whose `FIRMWARE_CTRL` is zero
//! is both a correctness gap (no waking vector / global lock) and a firmware-
//! description tell. Enlil emits the modern 64-byte, version-2 structure.

/// Length of the version-2 FACS (ACPI 2.0+): 64 bytes.
pub const FACS_LENGTH: u32 = 64;

/// FACS version emitted (ACPI 2.0+ adds the 64-bit waking vector + OSPM flags).
pub const FACS_VERSION: u8 = 2;

/// FACS builder.
pub struct FacsBuilder {
    /// The hardware signature OSPM stores at boot and re-checks on S4 resume; a
    /// mismatch tells OSPM the hardware changed and the saved state is stale.
    hardware_signature: u32,
}

impl Default for FacsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl FacsBuilder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hardware_signature: 0,
        }
    }

    /// Set the hardware signature (the CRC the firmware computes over selected
    /// ACPI tables). Any stable non-secret value is fine; OSPM only compares it
    /// against the value it cached on a prior boot.
    #[must_use]
    pub const fn hardware_signature(mut self, sig: u32) -> Self {
        self.hardware_signature = sig;
        self
    }

    /// Build the 64-byte FACS. The waking vectors and global lock reset to zero —
    /// the OS programs the waking vector before entering S3 and owns the global
    /// lock at runtime.
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(FACS_LENGTH as usize);
        // Offset 0: Signature "FACS".
        buf.extend_from_slice(b"FACS");
        // Offset 4: Length.
        buf.extend_from_slice(&FACS_LENGTH.to_le_bytes());
        // Offset 8: Hardware Signature.
        buf.extend_from_slice(&self.hardware_signature.to_le_bytes());
        // Offset 12: Firmware Waking Vector (32-bit, set by OSPM for S3 resume).
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Offset 16: Global Lock (arbitrates OSPM/firmware shared-hardware access).
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Offset 20: Flags. Bit 0 (S4BIOS_F) clear: no S4 entry via the FACS.
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Offset 24: X Firmware Waking Vector (64-bit, set by OSPM).
        buf.extend_from_slice(&0u64.to_le_bytes());
        // Offset 32: Version.
        buf.push(FACS_VERSION);
        // Offset 33: Reserved (3 bytes).
        buf.extend_from_slice(&[0u8; 3]);
        // Offset 36: OSPM Flags (bit 0 = 64BIT_WAKE; OSPM writes it).
        buf.extend_from_slice(&0u32.to_le_bytes());
        // Offset 40: Reserved (24 bytes).
        buf.extend_from_slice(&[0u8; 24]);
        debug_assert_eq!(buf.len(), FACS_LENGTH as usize);
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facs_is_64_bytes_with_signature_and_version() {
        let facs = FacsBuilder::new().build();
        assert_eq!(facs.len(), 64);
        assert_eq!(&facs[0..4], b"FACS");
        assert_eq!(u32::from_le_bytes(facs[4..8].try_into().unwrap()), 64);
        assert_eq!(facs[32], FACS_VERSION);
        // The FACS carries no checksum — the byte sum is not constrained to zero.
    }

    #[test]
    fn facs_carries_the_hardware_signature() {
        let facs = FacsBuilder::new().hardware_signature(0xDEAD_BEEF).build();
        assert_eq!(
            u32::from_le_bytes(facs[8..12].try_into().unwrap()),
            0xDEAD_BEEF
        );
        // Waking vectors and global lock reset to zero (OSPM owns them).
        assert_eq!(u32::from_le_bytes(facs[12..16].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(facs[16..20].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(facs[24..32].try_into().unwrap()), 0);
    }
}

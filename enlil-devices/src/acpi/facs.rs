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

/// The S3-resume waking state OSPM programmed into a FACS.
///
/// Read back by the hypervisor on wake to decide where — and in what CPU mode —
/// to re-enter the guest (item 5.7). OSPM writes these fields into the shared
/// FACS just before committing the S3 (suspend-to-RAM) transition; on resume the
/// firmware (here, enlil) hands control to the waking vector, and the OS's own
/// trampoline restores the rest of its context from RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FacsWaking {
    /// 32-bit real-mode firmware waking vector (FACS offset 12).
    pub waking_vector: u32,
    /// 64-bit firmware waking vector (FACS offset 24), used when OSPM requests a
    /// non-real-mode resume.
    pub x_waking_vector: u64,
    /// OSPM `64BIT_WAKE` flag (FACS OSPM-flags offset 36, bit 0): OSPM set it to
    /// ask for control to return via the 64-bit `x_waking_vector`.
    pub ospm_64bit_wake: bool,
}

/// Where and in what mode to resume a guest from S3, decoded from a
/// [`FacsWaking`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeTarget {
    /// Re-enter in **real mode** at the 32-bit firmware waking vector.
    RealMode(u32),
    /// Re-enter via the 64-bit `x_waking_vector` (OSPM asked for it).
    Extended(u64),
    /// No waking vector was programmed — the guest never armed an S3 resume, so
    /// there is nothing to resume to.
    None,
}

impl FacsWaking {
    /// Parse the waking state from a FACS byte buffer, or `None` if the buffer is
    /// too short to be a valid FACS.
    #[must_use]
    pub fn from_facs(facs: &[u8]) -> Option<Self> {
        if facs.len() < FACS_LENGTH as usize {
            return None;
        }
        Some(Self {
            waking_vector: u32::from_le_bytes(facs[12..16].try_into().ok()?),
            x_waking_vector: u64::from_le_bytes(facs[24..32].try_into().ok()?),
            ospm_64bit_wake: facs[36] & 0x1 != 0,
        })
    }

    /// The address and CPU mode to resume at (ACPI 6.x §5.2.10): use the 64-bit
    /// `x_waking_vector` iff OSPM set the `64BIT_WAKE` flag *and* that vector is
    /// non-zero; otherwise the 32-bit real-mode waking vector; otherwise nothing
    /// is armed.
    #[must_use]
    pub const fn resume_target(&self) -> ResumeTarget {
        if self.ospm_64bit_wake && self.x_waking_vector != 0 {
            ResumeTarget::Extended(self.x_waking_vector)
        } else if self.waking_vector != 0 {
            ResumeTarget::RealMode(self.waking_vector)
        } else {
            ResumeTarget::None
        }
    }

    /// Whether the guest has armed an S3 resume (a waking vector is programmed).
    #[must_use]
    pub const fn is_armed(&self) -> bool {
        !matches!(self.resume_target(), ResumeTarget::None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a FACS then let OSPM program the S3 waking fields into it, the way a
    /// guest does before entering S3.
    fn facs_with_waking(vector: u32, x_vector: u64, wake64: bool) -> Vec<u8> {
        let mut facs = FacsBuilder::new().build();
        facs[12..16].copy_from_slice(&vector.to_le_bytes());
        facs[24..32].copy_from_slice(&x_vector.to_le_bytes());
        facs[36] = u8::from(wake64);
        facs
    }

    #[test]
    fn reads_the_real_mode_waking_vector() {
        let facs = facs_with_waking(0x8000, 0, false);
        let w = FacsWaking::from_facs(&facs).unwrap();
        assert_eq!(w.waking_vector, 0x8000);
        assert!(!w.ospm_64bit_wake);
        assert_eq!(w.resume_target(), ResumeTarget::RealMode(0x8000));
        assert!(w.is_armed());
    }

    #[test]
    fn prefers_the_x_vector_only_when_ospm_asks_and_it_is_set() {
        // 64BIT_WAKE set + X non-zero → Extended.
        let facs = facs_with_waking(0x8000, 0x1_0000_0000, true);
        let w = FacsWaking::from_facs(&facs).unwrap();
        assert_eq!(w.resume_target(), ResumeTarget::Extended(0x1_0000_0000));

        // 64BIT_WAKE set but X zero → fall back to the real-mode vector.
        let facs = facs_with_waking(0x8000, 0, true);
        let w = FacsWaking::from_facs(&facs).unwrap();
        assert_eq!(w.resume_target(), ResumeTarget::RealMode(0x8000));

        // X set but OSPM did not ask for 64-bit wake → real mode.
        let facs = facs_with_waking(0x8000, 0x1_0000_0000, false);
        let w = FacsWaking::from_facs(&facs).unwrap();
        assert_eq!(w.resume_target(), ResumeTarget::RealMode(0x8000));
    }

    #[test]
    fn a_freshly_built_facs_is_not_armed() {
        // OSPM has not programmed a waking vector yet.
        let facs = FacsBuilder::new().build();
        let w = FacsWaking::from_facs(&facs).unwrap();
        assert_eq!(w.resume_target(), ResumeTarget::None);
        assert!(!w.is_armed());
    }

    #[test]
    fn a_short_buffer_is_rejected() {
        assert!(FacsWaking::from_facs(&[0u8; 32]).is_none());
    }

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

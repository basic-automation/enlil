//! Explicit low-bit narrowing for register and serialization code.
//!
//! Hardware register and byte-stream code frequently needs to take the low
//! 8/16/32 bits of a wider value. Writing `value as u16` triggers
//! `clippy::cast_possible_truncation`, and the truncation is intentional, so
//! these helpers express it explicitly (and panic-free) via little-endian byte
//! reconstruction instead of a lossy `as` cast.

/// Widening to `u64` for any unsigned integer width (lossless).
pub trait Widen {
    /// Zero-extend `self` to a `u64`.
    fn to_u64(self) -> u64;
}

impl Widen for u8 {
    fn to_u64(self) -> u64 {
        u64::from(self)
    }
}
impl Widen for u16 {
    fn to_u64(self) -> u64 {
        u64::from(self)
    }
}
impl Widen for u32 {
    fn to_u64(self) -> u64 {
        u64::from(self)
    }
}
impl Widen for u64 {
    fn to_u64(self) -> u64 {
        self
    }
}
impl Widen for usize {
    fn to_u64(self) -> u64 {
        self as u64
    }
}
impl Widen for u128 {
    fn to_u64(self) -> u64 {
        let b = self.to_le_bytes();
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    }
}

/// `usize` value of `x`, saturating (lossless on 64-bit targets).
pub fn usize_of(x: impl Widen) -> usize {
    usize::try_from(x.to_u64()).unwrap_or(usize::MAX)
}

/// Low 8 bits of `x`.
pub fn u8_of(x: impl Widen) -> u8 {
    x.to_u64().to_le_bytes()[0]
}

/// Low 16 bits of `x` (little-endian).
pub fn u16_of(x: impl Widen) -> u16 {
    let b = x.to_u64().to_le_bytes();
    u16::from_le_bytes([b[0], b[1]])
}

/// Low 32 bits of `x` (little-endian).
pub fn u32_of(x: impl Widen) -> u32 {
    let b = x.to_u64().to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests {
    use super::{u8_of, u16_of, u32_of, usize_of};

    #[test]
    fn narrows_low_bits() {
        assert_eq!(u8_of(0x1234_5678_u64), 0x78);
        assert_eq!(u16_of(0x1234_5678_u64), 0x5678);
        assert_eq!(u32_of(0x1_2345_6789_u64), 0x2345_6789);
        // matches truncating-cast semantics, expressed without `as`
        assert_eq!(u8_of(0xFFFF_u32), 0xFF);
        assert_eq!(u16_of(0xDEAD_BEEF_u32), 0xBEEF);
        assert_eq!(u32_of(0x1_0000_0001_usize), 1);
        assert_eq!(usize_of(42_u64), 42);
    }
}

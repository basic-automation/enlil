//! SLIT (System Locality Information Table) builder
//!
//! Defines NUMA inter-node distances. Even single-node VMs should provide
//! this — Windows uses it for memory allocation optimization. The diagonal
//! distance (self-to-self) is always 10 per the ACPI spec.

use super::tables::{AcpiSdtHeader, OemInfo};
use crate::truncate::{u32_of, usize_of};

/// SLIT table builder
pub struct SlitBuilder {
    oem: OemInfo,
    /// Number of NUMA proximity domains
    locality_count: u64,
    /// Distance matrix (row-major, `locality_count` × `locality_count`)
    distances: Vec<u8>,
}

impl SlitBuilder {
    /// Create a single-domain SLIT (distance 10 to self)
    #[must_use]
    pub fn single_node() -> Self {
        Self {
            oem: OemInfo::default(),
            locality_count: 1,
            distances: vec![10],
        }
    }

    /// Create a multi-node SLIT with custom distance matrix
    ///
    /// `distances` must be `count * count` entries, row-major order.
    /// Diagonal entries should be 10 (self-distance).
    /// # Panics
    ///
    /// Panics if `distances.len()` is not `count * count`.
    #[must_use]
    pub fn multi_node(count: u64, distances: Vec<u8>) -> Self {
        assert_eq!(
            distances.len(),
            usize_of(count * count),
            "distance matrix must be count×count"
        );
        Self {
            oem: OemInfo::default(),
            locality_count: count,
            distances,
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    /// Build the SLIT table as bytes
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        // Header (36) + locality count (8) + distance matrix
        let total_length = 36 + 8 + self.distances.len();
        let mut buf = Vec::with_capacity(total_length);

        let header = AcpiSdtHeader::new(*b"SLIT", u32_of(total_length), 1, &self.oem);
        buf.extend_from_slice(&header.to_bytes());

        // Number of System Localities (8 bytes, u64 LE)
        buf.extend_from_slice(&self.locality_count.to_le_bytes());

        // Distance matrix
        buf.extend_from_slice(&self.distances);

        // Fix up checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for SlitBuilder {
    fn default() -> Self {
        Self::single_node()
    }
}

/// The NUMA node-to-node distance matrix parsed from a SLIT.
///
/// `distance(a, b)` is the relative latency from proximity domain `a` to `b`
/// (10 = local per ACPI; larger = farther). The placement and fabric logic
/// reads this to honor the north-star rule that a kernel's hot CPU+RAM working
/// set stays on one node and that interconnect latency bounds what can be
/// pooled — the SLIT is where the host advertises those relative distances.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalityMatrix {
    count: usize,
    /// Row-major `count × count` distances.
    distances: Vec<u8>,
}

impl LocalityMatrix {
    /// The number of proximity domains (localities) the matrix covers.
    #[must_use]
    pub const fn locality_count(&self) -> usize {
        self.count
    }

    /// The distance from domain `from` to domain `to`, or `None` if either
    /// index is out of range.
    #[must_use]
    pub fn distance(&self, from: usize, to: usize) -> Option<u8> {
        if from >= self.count || to >= self.count {
            return None;
        }
        self.distances.get(from * self.count + to).copied()
    }

    /// The distances from domain `from` to every domain (its matrix row), or
    /// `None` if `from` is out of range.
    #[must_use]
    pub fn row(&self, from: usize) -> Option<&[u8]> {
        if from >= self.count {
            return None;
        }
        self.distances
            .get(from * self.count..(from + 1) * self.count)
    }
}

/// Parse the NUMA distance matrix from a SLIT table (Phase 6.3 / fabric).
///
/// The SLIT is a 36-byte SDT header, an 8-byte locality count (`u64` LE) at
/// offset 36, then a `count × count` row-major matrix of distance bytes at
/// offset 44. Returns `None` if the table is truncated or its declared count
/// does not match the matrix length (a corrupt/hostile table), rather than
/// reading out of bounds.
#[must_use]
pub fn locality_distances(slit: &[u8]) -> Option<LocalityMatrix> {
    /// Offset of the 8-byte locality count.
    const COUNT_OFF: usize = 36;
    /// Offset of the distance matrix.
    const MATRIX_OFF: usize = 44;

    let count_bytes = slit.get(COUNT_OFF..MATRIX_OFF)?;
    let count = usize::try_from(u64::from_le_bytes(count_bytes.try_into().ok()?)).ok()?;
    let expected = count.checked_mul(count)?;
    let matrix = slit.get(MATRIX_OFF..MATRIX_OFF.checked_add(expected)?)?;
    Some(LocalityMatrix {
        count,
        distances: matrix.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slit_single_node_builds() {
        let slit = SlitBuilder::single_node().build();
        assert_eq!(&slit[0..4], b"SLIT");
        // Header(36) + locality_count(8) + 1 byte distance
        assert_eq!(slit.len(), 45);
    }

    #[test]
    fn slit_checksum() {
        let slit = SlitBuilder::single_node().build();
        let sum: u8 = slit.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn slit_locality_count() {
        let slit = SlitBuilder::single_node().build();
        let count = u64::from_le_bytes(slit[36..44].try_into().unwrap());
        assert_eq!(count, 1);
    }

    #[test]
    fn slit_self_distance() {
        let slit = SlitBuilder::single_node().build();
        assert_eq!(slit[44], 10, "self-distance must be 10");
    }

    #[test]
    fn slit_two_nodes() {
        // 2 nodes: self=10, cross=20
        let distances = vec![10, 20, 20, 10];
        let slit = SlitBuilder::multi_node(2, distances).build();
        assert_eq!(slit.len(), 36 + 8 + 4);
        let count = u64::from_le_bytes(slit[36..44].try_into().unwrap());
        assert_eq!(count, 2);
        assert_eq!(slit[44], 10);
        assert_eq!(slit[45], 20);
        assert_eq!(slit[46], 20);
        assert_eq!(slit[47], 10);
        let sum: u8 = slit.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn slit_length_field() {
        let slit = SlitBuilder::single_node().build();
        let length = u32::from_le_bytes(slit[4..8].try_into().unwrap());
        assert_eq!(length as usize, slit.len());
    }

    #[test]
    fn parses_the_single_node_distance_matrix() {
        let slit = SlitBuilder::single_node().build();
        let m = locality_distances(&slit).expect("parse");
        assert_eq!(m.locality_count(), 1);
        assert_eq!(m.distance(0, 0), Some(10));
        assert_eq!(m.distance(0, 1), None); // out of range
        assert_eq!(m.row(0), Some(&[10u8][..]));
    }

    #[test]
    fn parses_a_two_node_distance_matrix() {
        let slit = SlitBuilder::multi_node(2, vec![10, 21, 21, 10]).build();
        let m = locality_distances(&slit).expect("parse");
        assert_eq!(m.locality_count(), 2);
        assert_eq!(m.distance(0, 0), Some(10));
        assert_eq!(m.distance(0, 1), Some(21));
        assert_eq!(m.distance(1, 0), Some(21));
        assert_eq!(m.distance(1, 1), Some(10));
        assert_eq!(m.distance(2, 0), None);
        assert_eq!(m.row(1), Some(&[21u8, 10][..]));
    }

    #[test]
    fn rejects_a_truncated_or_mismatched_slit() {
        // Too short to hold the count field.
        assert!(locality_distances(&[0u8; 40]).is_none());
        // Declares 2 localities but carries only 2 matrix bytes (needs 4).
        let mut bad = vec![0u8; 44 + 2];
        bad[36..44].copy_from_slice(&2u64.to_le_bytes());
        assert!(locality_distances(&bad).is_none());
    }
}

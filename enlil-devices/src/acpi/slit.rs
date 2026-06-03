//! SLIT (System Locality Information Table) builder
//!
//! Defines NUMA inter-node distances. Even single-node VMs should provide
//! this — Windows uses it for memory allocation optimization. The diagonal
//! distance (self-to-self) is always 10 per the ACPI spec.

use super::tables::{AcpiSdtHeader, OemInfo};

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
    #[must_use]
    pub fn multi_node(count: u64, distances: Vec<u8>) -> Self {
        assert_eq!(
            distances.len(),
            (count * count) as usize,
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
    #[allow(clippy::cast_possible_truncation)]
    pub fn build(&self) -> Vec<u8> {
        // Header (36) + locality count (8) + distance matrix
        let total_length = 36 + 8 + self.distances.len();
        let mut buf = Vec::with_capacity(total_length);

        let header = AcpiSdtHeader::new(*b"SLIT", total_length as u32, 1, &self.oem);
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
}

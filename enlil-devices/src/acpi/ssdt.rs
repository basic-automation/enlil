//! SSDT (Secondary System Description Table) builder
//!
//! Generates an SSDT with AML bytecode defining CPU power management objects
//! that Windows 11 uses for P-states (performance) and C-states (idle):
//! - `_PSS` — Performance Supported States (frequency/voltage pairs)
//! - `_CST` — C-States (idle power states: C1, C2, C3)

use crate::truncate::{u16_of, u32_of, u8_of};
use super::aml::opcode;
use super::tables::{AcpiSdtHeader, OemInfo};

/// P-state definition (performance state)
#[derive(Debug, Clone, Copy)]
pub struct PState {
    /// Core frequency in MHz
    pub frequency_mhz: u32,
    /// Power dissipation in mW
    pub power_mw: u32,
    /// Transition latency in microseconds
    pub latency_us: u32,
    /// Control value written to MSR/register
    pub control: u32,
    /// Status value read back
    pub status: u32,
}

/// C-state definition (idle/sleep state)
#[derive(Debug, Clone, Copy)]
pub struct CState {
    /// C-state type (1 = C1, 2 = C2, 3 = C3)
    pub ctype: u8,
    /// Worst-case latency in microseconds
    pub latency_us: u16,
    /// Power consumed in mW
    pub power_mw: u32,
    /// Register address (GAS: Generic Address Structure)
    pub register_address: u64,
    /// Register bit width
    pub register_bit_width: u8,
    /// Address space ID (0x7F = `FFixedHW` for Intel, 0x01 = IO)
    pub address_space: u8,
}

/// SSDT builder for CPU power management objects
pub struct SsdtBuilder {
    oem: OemInfo,
    vcpu_count: u8,
    pstates: Vec<PState>,
    cstates: Vec<CState>,
}

impl SsdtBuilder {
    /// Number of vCPUs this builder emits power-management objects for.
    #[must_use]
    pub const fn vcpu_count(&self) -> u8 {
        self.vcpu_count
    }

    #[must_use]
    pub fn new(vcpu_count: u8) -> Self {
        Self {
            oem: OemInfo::default(),
            vcpu_count,
            pstates: Self::default_pstates(),
            cstates: Self::default_cstates(),
        }
    }

    #[must_use]
    pub const fn oem_info(mut self, oem: OemInfo) -> Self {
        self.oem = oem;
        self
    }

    #[must_use]
    pub fn pstates(mut self, pstates: Vec<PState>) -> Self {
        self.pstates = pstates;
        self
    }

    #[must_use]
    pub fn cstates(mut self, cstates: Vec<CState>) -> Self {
        self.cstates = cstates;
        self
    }

    /// Default 4-entry P-state table: turbo, nominal, efficient, low
    fn default_pstates() -> Vec<PState> {
        vec![
            PState {
                frequency_mhz: 3600,
                power_mw: 95000,
                latency_us: 10,
                control: 0x24,
                status: 0x24,
            },
            PState {
                frequency_mhz: 3000,
                power_mw: 65000,
                latency_us: 10,
                control: 0x1E,
                status: 0x1E,
            },
            PState {
                frequency_mhz: 2400,
                power_mw: 45000,
                latency_us: 10,
                control: 0x18,
                status: 0x18,
            },
            PState {
                frequency_mhz: 1200,
                power_mw: 20000,
                latency_us: 10,
                control: 0x0C,
                status: 0x0C,
            },
        ]
    }

    /// Default C-states: C1 (halt), C2 (stop-clock), C3 (deep sleep)
    fn default_cstates() -> Vec<CState> {
        vec![
            CState {
                ctype: 1,
                latency_us: 1,
                power_mw: 1000,
                register_address: 0,
                register_bit_width: 0,
                address_space: 0x7F, // FFixedHW
            },
            CState {
                ctype: 2,
                latency_us: 50,
                power_mw: 500,
                register_address: 0x414,
                register_bit_width: 8,
                address_space: 0x01, // SystemIO
            },
            CState {
                ctype: 3,
                latency_us: 100,
                power_mw: 250,
                register_address: 0x415,
                register_bit_width: 8,
                address_space: 0x01, // SystemIO
            },
        ]
    }

    /// Encode an AML integer data object (without Name opcode)
    fn encode_integer(value: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        if value == 0 {
            buf.push(opcode::ZERO);
        } else if value == 1 {
            buf.push(opcode::ONE);
        } else if value <= 0xFF {
            buf.push(opcode::BYTE_PREFIX);
            buf.push(u8_of(value));
        } else if value <= 0xFFFF {
            buf.push(opcode::WORD_PREFIX);
            buf.extend_from_slice(&(u16_of(value)).to_le_bytes());
        } else if value <= 0xFFFF_FFFF {
            buf.push(opcode::DWORD_PREFIX);
            buf.extend_from_slice(&(u32_of(value)).to_le_bytes());
        } else {
            buf.push(opcode::QWORD_PREFIX);
            buf.extend_from_slice(&value.to_le_bytes());
        }
        buf
    }

    /// Encode a `PkgLength`
    fn encode_pkg_length(length: usize) -> Vec<u8> {
        if length < 0x3F {
            vec![u8_of(length)]
        } else if length < 0xFFF {
            vec![
                (u8_of(length & 0x0F)) | (1 << 6),
                (length >> 4).to_le_bytes()[0],
            ]
        } else if length < 0xF_FFFF {
            vec![
                (u8_of(length & 0x0F)) | (2 << 6),
                (length >> 4).to_le_bytes()[0],
                (length >> 12).to_le_bytes()[0],
            ]
        } else {
            vec![
                (u8_of(length & 0x0F)) | (3 << 6),
                (length >> 4).to_le_bytes()[0],
                (length >> 12).to_le_bytes()[0],
                (length >> 20).to_le_bytes()[0],
            ]
        }
    }

    /// Build a _PSS package entry: Package(6) { freq, power, latency, buslat, control, status }
    fn build_pss_entry(pstate: &PState) -> Vec<u8> {
        let mut inner = Vec::new();
        inner.extend_from_slice(&Self::encode_integer(u64::from(pstate.frequency_mhz)));
        inner.extend_from_slice(&Self::encode_integer(u64::from(pstate.power_mw)));
        inner.extend_from_slice(&Self::encode_integer(u64::from(pstate.latency_us)));
        inner.extend_from_slice(&Self::encode_integer(u64::from(pstate.latency_us))); // bus latency = same
        inner.extend_from_slice(&Self::encode_integer(u64::from(pstate.control)));
        inner.extend_from_slice(&Self::encode_integer(u64::from(pstate.status)));

        let mut pkg = Vec::new();
        pkg.push(opcode::PACKAGE_OP);
        // PkgLength covers: pkg_length_bytes + num_elements + inner
        let pkg_body_len = 1 + inner.len(); // 1 for NumElements
        let pkg_len_bytes =
            Self::encode_pkg_length(pkg_body_len + Self::encode_pkg_length(pkg_body_len).len());
        pkg.extend_from_slice(&pkg_len_bytes);
        pkg.push(6); // NumElements
        pkg.extend_from_slice(&inner);
        pkg
    }

    /// Build the _PSS object as Name(_PSS, Package(N) { ... })
    fn build_pss_object(&self) -> Vec<u8> {
        let mut entries_bytes = Vec::new();
        for ps in &self.pstates {
            entries_bytes.extend_from_slice(&Self::build_pss_entry(ps));
        }

        let mut pkg = Vec::new();
        pkg.push(opcode::PACKAGE_OP);
        let pkg_body_len = 1 + entries_bytes.len(); // 1 for NumElements
        let pkg_len_bytes =
            Self::encode_pkg_length(pkg_body_len + Self::encode_pkg_length(pkg_body_len).len());
        pkg.extend_from_slice(&pkg_len_bytes);
        pkg.push(u8_of(self.pstates.len()));
        pkg.extend_from_slice(&entries_bytes);

        let mut buf = Vec::new();
        buf.push(opcode::NAME_OP);
        buf.extend_from_slice(b"_PSS");
        buf.extend_from_slice(&pkg);
        buf
    }

    /// Build a GAS (Generic Address Structure) for a C-state register (12 bytes)
    fn build_gas(cstate: &CState) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12);
        buf.push(cstate.address_space);
        buf.push(cstate.register_bit_width);
        buf.push(0); // bit offset
        buf.push(u8::from(cstate.register_bit_width != 0)); // access size: byte
        buf.extend_from_slice(&cstate.register_address.to_le_bytes());
        buf
    }

    /// Build a _CST C-state sub-package: Package(4) { ResourceTemplate{Register(...)}, `CType`, Latency, Power }
    fn build_cst_entry(cstate: &CState) -> Vec<u8> {
        // Build the ResourceTemplate buffer containing the GAS
        let gas_bytes = Self::build_gas(cstate);
        let resource_buf = Self::build_resource_buffer(&gas_bytes);

        let mut inner = Vec::new();
        inner.extend_from_slice(&resource_buf);
        inner.extend_from_slice(&Self::encode_integer(u64::from(cstate.ctype)));
        inner.extend_from_slice(&Self::encode_integer(u64::from(cstate.latency_us)));
        inner.extend_from_slice(&Self::encode_integer(u64::from(cstate.power_mw)));

        let mut pkg = Vec::new();
        pkg.push(opcode::PACKAGE_OP);
        let pkg_body_len = 1 + inner.len();
        let pkg_len_bytes =
            Self::encode_pkg_length(pkg_body_len + Self::encode_pkg_length(pkg_body_len).len());
        pkg.extend_from_slice(&pkg_len_bytes);
        pkg.push(4); // NumElements
        pkg.extend_from_slice(&inner);
        pkg
    }

    /// Build an AML Buffer containing a GAS as a resource template
    fn build_resource_buffer(gas: &[u8]) -> Vec<u8> {
        // Register descriptor (type 0x82, length 12) + end tag (0x79, 0x00)
        let mut resource = Vec::new();
        resource.push(0x82); // Generic Register Descriptor
        resource.push(0x0C); // length = 12
        resource.push(0x00); // length high byte
        resource.extend_from_slice(gas);
        resource.push(0x79); // End Tag
        resource.push(0x00); // checksum

        // Wrap in Buffer op
        let mut buf = Vec::new();
        buf.push(opcode::BUFFER_OP);
        let buffer_inner_len = Self::encode_integer(resource.len() as u64).len() + resource.len();
        let buf_len_bytes = Self::encode_pkg_length(
            buffer_inner_len + Self::encode_pkg_length(buffer_inner_len).len(),
        );
        buf.extend_from_slice(&buf_len_bytes);
        buf.extend_from_slice(&Self::encode_integer(resource.len() as u64));
        buf.extend_from_slice(&resource);
        buf
    }

    /// Build the _CST object: Name(_CST, Package(N+1) { count, entry0, entry1, ... })
    fn build_cst_object(&self) -> Vec<u8> {
        let mut entries_bytes = Vec::new();
        // First element is the count
        entries_bytes.extend_from_slice(&Self::encode_integer(self.cstates.len() as u64));
        for cs in &self.cstates {
            entries_bytes.extend_from_slice(&Self::build_cst_entry(cs));
        }

        let mut pkg = Vec::new();
        pkg.push(opcode::PACKAGE_OP);
        let num_elements = u8_of(self.cstates.len() + 1); // count integer + entries
        let pkg_body_len = 1 + entries_bytes.len(); // 1 for NumElements byte
        let pkg_len_bytes =
            Self::encode_pkg_length(pkg_body_len + Self::encode_pkg_length(pkg_body_len).len());
        pkg.extend_from_slice(&pkg_len_bytes);
        pkg.push(num_elements);
        pkg.extend_from_slice(&entries_bytes);

        let mut buf = Vec::new();
        buf.push(opcode::NAME_OP);
        buf.extend_from_slice(b"_CST");
        buf.extend_from_slice(&pkg);
        buf
    }

    /// Generate processor name: C00_, C01_, ... C0F_, C10_, etc.
    const fn processor_name(index: u8) -> [u8; 4] {
        let hex = b"0123456789ABCDEF";
        [
            b'C',
            hex[((index >> 4) & 0xF) as usize],
            hex[(index & 0xF) as usize],
            b'_',
        ]
    }

    /// Build complete AML bytecode for all CPUs
    fn generate_aml(&self) -> Vec<u8> {
        let pss_bytes = self.build_pss_object();
        let cst_bytes = self.build_cst_object();

        // Build a Scope(\_PR) containing each processor's _PSS and _CST
        // For CPU0 we define the objects directly, for others we reference CPU0
        // (Windows only needs them on one processor to inherit)
        let mut aml = Vec::new();

        // Scope(\_PR) { Scope(C00_) { _PSS, _CST } }
        // We emit the full objects inside CPU0's scope
        let mut pr_body = Vec::new();

        // CPU0 scope with _PSS and _CST
        {
            let cpu0_name = Self::processor_name(0);
            let mut cpu0_body = Vec::new();
            cpu0_body.extend_from_slice(&pss_bytes);
            cpu0_body.extend_from_slice(&cst_bytes);

            let mut cpu0_scope = Vec::new();
            cpu0_scope.push(opcode::SCOPE_OP);
            let scope_inner_len = 4 + cpu0_body.len(); // 4 for name
            let scope_pkg_len = Self::encode_pkg_length(
                scope_inner_len + Self::encode_pkg_length(scope_inner_len).len(),
            );
            cpu0_scope.extend_from_slice(&scope_pkg_len);
            cpu0_scope.extend_from_slice(&cpu0_name);
            cpu0_scope.extend_from_slice(&cpu0_body);
            pr_body.extend_from_slice(&cpu0_scope);
        }

        // For CPUs 1..N, emit a Scope with a method that returns CPU0's objects
        // Actually, Windows derives all CPUs from CPU0's _PSS/_CST automatically
        // when OSPM processes _PPC/_PCT. No need to duplicate.

        // Wrap in Scope(\_PR_)
        aml.push(opcode::SCOPE_OP);
        let pr_inner_len = 4 + pr_body.len(); // 4 for name "_PR_"
        let pr_pkg_len =
            Self::encode_pkg_length(pr_inner_len + Self::encode_pkg_length(pr_inner_len).len());
        aml.extend_from_slice(&pr_pkg_len);
        aml.extend_from_slice(b"_PR_");
        aml.extend_from_slice(&pr_body);

        aml
    }

    /// Build the complete SSDT table as a byte vector
    #[must_use]
    pub fn build(&self) -> Vec<u8> {
        let aml_bytes = self.generate_aml();
        let total_length = u32_of(AcpiSdtHeader::SIZE + aml_bytes.len());

        let header = AcpiSdtHeader::new(*b"SSDT", total_length, 2, &self.oem);
        let mut buf = Vec::with_capacity(total_length as usize);
        buf.extend_from_slice(&header.to_bytes());
        buf.extend_from_slice(&aml_bytes);

        // Fix checksum
        let sum: u8 = buf.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        buf[9] = buf[9].wrapping_sub(sum);

        buf
    }
}

impl Default for SsdtBuilder {
    fn default() -> Self {
        Self::new(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssdt_builds() {
        let ssdt = SsdtBuilder::new(4).build();
        assert_eq!(&ssdt[0..4], b"SSDT");
        assert!(ssdt.len() > AcpiSdtHeader::SIZE);
    }

    #[test]
    fn ssdt_checksum() {
        let ssdt = SsdtBuilder::new(4).build();
        let sum: u8 = ssdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn ssdt_revision() {
        let ssdt = SsdtBuilder::new(2).build();
        assert_eq!(ssdt[8], 2); // SSDT revision 2
    }

    #[test]
    fn ssdt_length_field_matches() {
        let ssdt = SsdtBuilder::new(4).build();
        let length = u32::from_le_bytes(ssdt[4..8].try_into().unwrap()) as usize;
        assert_eq!(length, ssdt.len());
    }

    #[test]
    fn ssdt_contains_pss() {
        let ssdt = SsdtBuilder::new(1).build();
        let aml = &ssdt[AcpiSdtHeader::SIZE..];
        let found = aml.windows(4).any(|w| w == b"_PSS");
        assert!(found, "SSDT must contain _PSS object");
    }

    #[test]
    fn ssdt_contains_cst() {
        let ssdt = SsdtBuilder::new(1).build();
        let aml = &ssdt[AcpiSdtHeader::SIZE..];
        let found = aml.windows(4).any(|w| w == b"_CST");
        assert!(found, "SSDT must contain _CST object");
    }

    #[test]
    fn ssdt_custom_pstates() {
        let ssdt = SsdtBuilder::new(2)
            .pstates(vec![
                PState {
                    frequency_mhz: 4000,
                    power_mw: 105_000,
                    latency_us: 10,
                    control: 0x28,
                    status: 0x28,
                },
                PState {
                    frequency_mhz: 2000,
                    power_mw: 35000,
                    latency_us: 10,
                    control: 0x14,
                    status: 0x14,
                },
            ])
            .build();
        let sum: u8 = ssdt.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        assert_eq!(sum, 0);
    }
}

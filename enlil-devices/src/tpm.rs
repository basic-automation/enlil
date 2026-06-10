//! Virtual TPM 2.0 device
//!
//! Required for Windows 11. Provides a software TPM 2.0 exposed via MMIO
//! at the standard address 0xFED40000. Each guest gets its own independent
//! virtual TPM with separate PCR banks, endorsement keys, etc.

use crate::truncate::{u8_of, u16_of, u32_of, usize_of};
use std::collections::HashMap;
/// Standard TPM MMIO base address
pub const TPM_MMIO_BASE: u64 = 0xFED4_0000;
/// TPM MMIO region size (4KB for CRB interface)
pub const TPM_MMIO_SIZE: u64 = 0x5000;

/// TPM interface type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpmInterface {
    /// Command Response Buffer (CRB) — preferred for Windows 11
    Crb,
    /// TIS (TPM Interface Specification) — legacy
    Tis,
}

/// TPM 2.0 command codes (subset)
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpmCommand {
    Startup = 0x0000_0144,
    Shutdown = 0x0000_0145,
    SelfTest = 0x0000_0143,
    GetCapability = 0x0000_017A,
    PcrExtend = 0x0000_0182,
    PcrRead = 0x0000_017E,
    GetRandom = 0x0000_017B,
    HashSequenceStart = 0x0000_0186,
    NvRead = 0x0000_014E,
    NvWrite = 0x0000_0137,
    Unknown(u32),
}

impl From<u32> for TpmCommand {
    fn from(value: u32) -> Self {
        match value {
            0x0000_0144 => Self::Startup,
            0x0000_0145 => Self::Shutdown,
            0x0000_0143 => Self::SelfTest,
            0x0000_017A => Self::GetCapability,
            0x0000_0182 => Self::PcrExtend,
            0x0000_017E => Self::PcrRead,
            0x0000_017B => Self::GetRandom,
            0x0000_0186 => Self::HashSequenceStart,
            0x0000_014E => Self::NvRead,
            0x0000_0137 => Self::NvWrite,
            other => Self::Unknown(other),
        }
    }
}

/// TPM CRB (Command Response Buffer) register offsets
pub mod crb_regs {
    /// Locality State
    pub const LOC_STATE: u64 = 0x00;
    /// Locality Control
    pub const LOC_CTRL: u64 = 0x08;
    /// Locality Status
    pub const LOC_STS: u64 = 0x0C;
    /// Interface ID
    pub const INTF_ID: u64 = 0x30;
    /// Control Extension
    pub const CTRL_EXT: u64 = 0x38;
    /// Control Request
    pub const CTRL_REQ: u64 = 0x40;
    /// Control Status
    pub const CTRL_STS: u64 = 0x44;
    /// Control Cancel
    pub const CTRL_CANCEL: u64 = 0x48;
    /// Control Start
    pub const CTRL_START: u64 = 0x4C;
    /// Interrupt Enable
    pub const INT_ENABLE: u64 = 0x50;
    /// Interrupt Status
    pub const INT_STS: u64 = 0x54;
    /// Command Size
    pub const CMD_SIZE: u64 = 0x58;
    /// Command Buffer Address
    pub const CMD_ADDR: u64 = 0x5C;
    /// Response Size
    pub const RSP_SIZE: u64 = 0x64;
    /// Response Address
    pub const RSP_ADDR: u64 = 0x68;
    /// Data buffer start (relative to MMIO base)
    pub const DATA_BUFFER: u64 = 0x80;
}

/// Number of PCR banks
pub const PCR_COUNT: usize = 24;
/// SHA-256 digest size
pub const SHA256_DIGEST_SIZE: usize = 32;

/// Per-guest virtual TPM state
#[derive(Debug, Clone)]
pub struct VirtualTpm {
    /// Interface type
    pub interface: TpmInterface,
    /// PCR bank (SHA-256)
    pub pcr_sha256: [[u8; SHA256_DIGEST_SIZE]; PCR_COUNT],
    /// TPM started flag
    pub started: bool,
    /// Self-test completed
    pub self_tested: bool,
    /// Current locality (0-4)
    pub locality: u8,
    /// CRB registers
    pub loc_state: u32,
    pub loc_ctrl: u32,
    pub loc_sts: u32,
    pub ctrl_sts: u32,
    pub ctrl_start: u32,
    /// Command buffer
    pub cmd_buffer: Vec<u8>,
    /// Response buffer
    pub rsp_buffer: Vec<u8>,
    /// Persistent state file path (for `BitLocker`, Windows Hello, etc.)
    pub state_path: Option<String>,
    /// NV index storage — persistent blobs a guest seals/reads back
    /// (`BitLocker` metadata, Windows Hello, EK certificate indices, …)
    nv_storage: HashMap<u32, Vec<u8>>,
    /// `GetRandom` PRNG state (xorshift64*); seeded deterministically at creation.
    rng_state: u64,
}

impl VirtualTpm {
    #[must_use]
    pub fn new(interface: TpmInterface) -> Self {
        Self {
            interface,
            pcr_sha256: [[0u8; SHA256_DIGEST_SIZE]; PCR_COUNT],
            started: false,
            self_tested: false,
            locality: 0,
            loc_state: 0x81, // TPM established, locality 0 active
            loc_ctrl: 0,
            loc_sts: 0x01, // Granted
            ctrl_sts: 0,   // Idle
            ctrl_start: 0,
            cmd_buffer: vec![0u8; 4096],
            rsp_buffer: vec![0u8; 4096],
            state_path: None,
            nv_storage: HashMap::new(),
            rng_state: 0x9E37_79B9_7F4A_7C15, // nonzero seed (xorshift requires it)
        }
    }

    /// Dispatch a raw TPM2 command buffer against this TPM and return the
    /// response bytes. This is the byte-vec front the management plane uses;
    /// it shares all state (PCR bank, NV storage, RNG) with the CRB MMIO
    /// front, so measurements made over either path agree.
    pub fn execute_command(&mut self, command: &[u8]) -> Vec<u8> {
        let len = command.len().min(self.cmd_buffer.len());
        self.cmd_buffer[..len].copy_from_slice(&command[..len]);
        self.process_command();
        let declared = usize_of(u32::from_be_bytes(
            self.rsp_buffer[2..6].try_into().unwrap_or([0; 4]),
        ));
        // A response is at least the 10-byte header; trust the declared size
        // only up to the buffer's bounds.
        let rsp_len = declared.clamp(10, self.rsp_buffer.len());
        self.rsp_buffer[..rsp_len].to_vec()
    }

    /// Handle MMIO read at given offset from `TPM_MMIO_BASE`
    #[must_use]
    pub fn read_register(&self, offset: u64, size: u8) -> u64 {
        match offset {
            crb_regs::LOC_STATE => u64::from(self.loc_state),
            crb_regs::LOC_CTRL => u64::from(self.loc_ctrl),
            crb_regs::LOC_STS => u64::from(self.loc_sts),
            crb_regs::INTF_ID => {
                // CRB interface, TPM 2.0
                0x0000_0001 // CRB active
            }
            crb_regs::CTRL_STS => u64::from(self.ctrl_sts),
            crb_regs::CTRL_START => u64::from(self.ctrl_start),
            crb_regs::CMD_SIZE | crb_regs::RSP_SIZE => 4096,
            crb_regs::CMD_ADDR | crb_regs::RSP_ADDR => TPM_MMIO_BASE + crb_regs::DATA_BUFFER,
            o if o >= crb_regs::DATA_BUFFER => {
                // Read from response buffer
                let buf_offset = usize_of(o - crb_regs::DATA_BUFFER);
                if buf_offset < self.rsp_buffer.len() {
                    match size {
                        1 => u64::from(self.rsp_buffer[buf_offset]),
                        2 => {
                            let bytes: [u8; 2] = self.rsp_buffer[buf_offset..buf_offset + 2]
                                .try_into()
                                .unwrap_or([0; 2]);
                            u64::from(u16::from_le_bytes(bytes))
                        }
                        4 => {
                            let bytes: [u8; 4] = self.rsp_buffer[buf_offset..buf_offset + 4]
                                .try_into()
                                .unwrap_or([0; 4]);
                            u64::from(u32::from_le_bytes(bytes))
                        }
                        _ => 0,
                    }
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// Handle MMIO write at given offset from `TPM_MMIO_BASE`
    pub fn write_register(&mut self, offset: u64, value: u64, size: u8) {
        match offset {
            crb_regs::LOC_CTRL => {
                self.loc_ctrl = u32_of(value);
                // Request use: bit 1
                if value & 2 != 0 {
                    self.loc_sts = 0x01; // Granted
                    self.loc_state = 0x81; // Active
                }
                // Relinquish: bit 0
                if value & 1 != 0 {
                    self.loc_sts = 0x00;
                }
            }
            crb_regs::CTRL_START => {
                if value & 1 != 0 {
                    // Execute command
                    self.process_command();
                    self.ctrl_start = 0; // Command complete
                }
            }
            crb_regs::CTRL_CANCEL => {
                // Cancel current command
                self.ctrl_start = 0;
                self.ctrl_sts = 0; // Idle
            }
            o if o >= crb_regs::DATA_BUFFER => {
                // Write to command buffer
                let buf_offset = usize_of(o - crb_regs::DATA_BUFFER);
                if buf_offset < self.cmd_buffer.len() {
                    match size {
                        1 => self.cmd_buffer[buf_offset] = u8_of(value),
                        2 => {
                            let bytes = (u16_of(value)).to_le_bytes();
                            self.cmd_buffer[buf_offset..buf_offset + 2].copy_from_slice(&bytes);
                        }
                        4 => {
                            let bytes = (u32_of(value)).to_le_bytes();
                            self.cmd_buffer[buf_offset..buf_offset + 4].copy_from_slice(&bytes);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// Process a TPM command from the command buffer
    fn process_command(&mut self) {
        if self.cmd_buffer.len() < 10 {
            self.write_error_response(0x0000_0101); // TPM_RC_FAILURE
            return;
        }

        // TPM command header: tag (2) + size (4) + command_code (4)
        let command_code = u32::from_be_bytes(self.cmd_buffer[6..10].try_into().unwrap_or([0; 4]));

        match TpmCommand::from(command_code) {
            TpmCommand::Startup => {
                self.started = true;
                self.write_success_response();
            }
            TpmCommand::Shutdown => {
                self.started = false;
                self.write_success_response();
            }
            TpmCommand::SelfTest => {
                self.self_tested = true;
                self.write_success_response();
            }
            TpmCommand::GetCapability => {
                self.handle_get_capability();
            }
            TpmCommand::GetRandom => {
                self.handle_get_random();
            }
            TpmCommand::PcrExtend => {
                self.handle_pcr_extend();
            }
            TpmCommand::PcrRead => {
                self.handle_pcr_read();
            }
            TpmCommand::NvWrite => {
                self.handle_nv_write();
            }
            TpmCommand::NvRead => {
                self.handle_nv_read();
            }
            _ => {
                // Return success for unknown commands (permissive mode)
                self.write_success_response();
            }
        }
    }

    /// Handle `TPM2_PCR_Extend`: `pcr[idx] = SHA256(pcr[idx] || digest)`.
    ///
    /// Command layout (the pragmatic convention shared with the management vTPM):
    /// header (10) + `pcrHandle` (4, big-endian) + a 4-byte field + the digest bytes
    /// to fold in, up to the header's declared size. The PCR bank is SHA-256, so the
    /// extension uses the real hash from [`crate::crypto::sha256`].
    fn handle_pcr_extend(&mut self) {
        let size = usize_of(u32::from_be_bytes(
            self.cmd_buffer[2..6].try_into().unwrap_or([0; 4]),
        ));
        if size < 18 || size > self.cmd_buffer.len() {
            self.write_error_response(0x0000_0101); // TPM_RC_FAILURE
            return;
        }
        let pcr_index = usize_of(u32::from_be_bytes(
            self.cmd_buffer[10..14].try_into().unwrap_or([0; 4]),
        ));
        if pcr_index >= PCR_COUNT {
            self.write_error_response(0x0000_0101);
            return;
        }
        let mut input = self.pcr_sha256[pcr_index].to_vec();
        input.extend_from_slice(&self.cmd_buffer[18..size]);
        self.pcr_sha256[pcr_index] = crate::crypto::sha256(&input);
        self.write_success_response();
    }

    /// Handle `TPM2_PCR_Read`: return the addressed PCR's 32-byte SHA-256 value.
    /// Command layout: header (10) + `pcrHandle` (4). Response: header (10) + digest.
    fn handle_pcr_read(&mut self) {
        if self.cmd_buffer.len() < 14 {
            self.write_error_response(0x0000_0101);
            return;
        }
        let pcr_index = usize_of(u32::from_be_bytes(
            self.cmd_buffer[10..14].try_into().unwrap_or([0; 4]),
        ));
        if pcr_index >= PCR_COUNT {
            self.write_error_response(0x0000_0101);
            return;
        }
        let pcr = self.pcr_sha256[pcr_index];
        let response_size = 10 + SHA256_DIGEST_SIZE;
        self.rsp_buffer[0..2].copy_from_slice(&[0x00, 0xC4]); // TPM_ST_NO_SESSIONS
        self.rsp_buffer[2..6].copy_from_slice(&u32_of(response_size).to_be_bytes());
        self.rsp_buffer[6..10].copy_from_slice(&0u32.to_be_bytes()); // success
        self.rsp_buffer[10..10 + SHA256_DIGEST_SIZE].copy_from_slice(&pcr);
    }

    /// Handle `TPM2_NV_Write`: store a blob under an NV index.
    ///
    /// Command layout (the pragmatic convention this TPM uses — auth areas
    /// elided): header (10) + `nvIndex` (4, big-endian) + the data bytes, up
    /// to the header's declared size. Previously `NV_Read`/`NV_Write` fell
    /// through to the permissive success path, silently discarding writes —
    /// a guest sealing `BitLocker` metadata would read back nothing.
    fn handle_nv_write(&mut self) {
        let size = usize_of(u32::from_be_bytes(
            self.cmd_buffer[2..6].try_into().unwrap_or([0; 4]),
        ));
        if size < 14 || size > self.cmd_buffer.len() {
            self.write_error_response(0x0000_0101); // TPM_RC_FAILURE
            return;
        }
        let nv_index = u32::from_be_bytes(self.cmd_buffer[10..14].try_into().unwrap_or([0; 4]));
        let data = self.cmd_buffer[14..size].to_vec();
        self.nv_storage.insert(nv_index, data);
        self.write_success_response();
    }

    /// Handle `TPM2_NV_Read`: return the blob stored under an NV index.
    /// Command layout: header (10) + `nvIndex` (4). Response: header (10) +
    /// length (2, big-endian) + data. An undefined index returns
    /// `TPM_RC_HANDLE` (0x18B), as on a real TPM.
    fn handle_nv_read(&mut self) {
        if self.cmd_buffer.len() < 14 {
            self.write_error_response(0x0000_0101);
            return;
        }
        let nv_index = u32::from_be_bytes(self.cmd_buffer[10..14].try_into().unwrap_or([0; 4]));
        let Some(data) = self.nv_storage.get(&nv_index).cloned() else {
            self.write_error_response(0x0000_018B); // TPM_RC_HANDLE
            return;
        };
        let max_data = self.rsp_buffer.len() - 12;
        let data_len = data.len().min(max_data);
        let response_size = 12 + data_len;
        self.rsp_buffer[0..2].copy_from_slice(&[0x00, 0xC4]); // TPM_ST_NO_SESSIONS
        self.rsp_buffer[2..6].copy_from_slice(&u32_of(response_size).to_be_bytes());
        self.rsp_buffer[6..10].copy_from_slice(&0u32.to_be_bytes()); // success
        self.rsp_buffer[10..12].copy_from_slice(&u16_of(data_len).to_be_bytes());
        self.rsp_buffer[12..12 + data_len].copy_from_slice(&data[..data_len]);
    }

    /// Write a TPM success response
    fn write_success_response(&mut self) {
        // Response: tag (2) + size (4) + response_code (4)
        let response = [
            0x00, 0xC4, // TPM_ST_NO_SESSIONS
            0x00, 0x00, 0x00, 0x0A, // Size = 10
            0x00, 0x00, 0x00, 0x00, // TPM_RC_SUCCESS
        ];
        self.rsp_buffer[..10].copy_from_slice(&response);
    }

    /// Write a TPM error response
    fn write_error_response(&mut self, error_code: u32) {
        let mut response = [0u8; 10];
        response[0..2].copy_from_slice(&[0x00, 0xC4]); // TPM_ST_NO_SESSIONS
        response[2..6].copy_from_slice(&10u32.to_be_bytes());
        response[6..10].copy_from_slice(&error_code.to_be_bytes());
        self.rsp_buffer[..10].copy_from_slice(&response);
    }

    /// Handle `TPM2_GetCapability` (TCG TPM 2.0 Part 3 §30.2).
    ///
    /// Command: header (10) + `capability` (4) + `property` (4) +
    /// `propertyCount` (4). Response: header (10) + `moreData` (1) +
    /// `TPMS_CAPABILITY_DATA` (`capability` (4) + the capability-specific
    /// list). Previously this returned a bare 10-byte success with **no**
    /// capability data at all — the first thing a Windows TPM driver does is
    /// query `TPM_CAP_PCRS` / `TPM_CAP_TPM_PROPERTIES`, and an empty body
    /// fails its parse. Supported here:
    ///
    /// - `TPM_CAP_PCRS` (0x5): one `TPMS_PCR_SELECTION` — SHA-256 bank, all
    ///   [`PCR_COUNT`] PCRs allocated.
    /// - `TPM_CAP_TPM_PROPERTIES` (0x6): a fixed property set (family "2.0",
    ///   spec level/revision, manufacturer, PCR count). Pragmatic deviation:
    ///   the `property`/`propertyCount` window is ignored and the full set is
    ///   returned (spec-exact windowing needs a guest/swtpm oracle to verify).
    /// - `TPM_CAP_ALGS` (0x0): one `TPMS_ALG_PROPERTY` for SHA-256.
    ///
    /// Anything else returns `TPM_RC_VALUE` for parameter 1 (0x1C4).
    fn handle_get_capability(&mut self) {
        if self.cmd_buffer.len() < 22 {
            self.write_error_response(0x0000_0101); // TPM_RC_FAILURE
            return;
        }
        let capability = u32::from_be_bytes(self.cmd_buffer[10..14].try_into().unwrap_or([0; 4]));

        // Capability-specific payload (the part after `capability` in
        // TPMS_CAPABILITY_DATA), all big-endian per the TPM wire format.
        let payload: Vec<u8> = match capability {
            // TPM_CAP_ALGS → TPML_ALG_PROPERTY: count + (alg, attributes)
            0x0000_0000 => {
                let mut p = 1u32.to_be_bytes().to_vec();
                p.extend_from_slice(&0x000Bu16.to_be_bytes()); // TPM_ALG_SHA256
                p.extend_from_slice(&0x0000_0004u32.to_be_bytes()); // attributes: hash
                p
            }
            // TPM_CAP_PCRS → TPML_PCR_SELECTION: count + TPMS_PCR_SELECTION
            0x0000_0005 => {
                let mut p = 1u32.to_be_bytes().to_vec();
                p.extend_from_slice(&0x000Bu16.to_be_bytes()); // hash = SHA-256
                p.push(3); // sizeofSelect: 3 bytes cover 24 PCRs
                p.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // all 24 PCRs allocated
                p
            }
            // TPM_CAP_TPM_PROPERTIES → TPML_TAGGED_TPM_PROPERTY:
            // count + count × (property, value)
            0x0000_0006 => {
                let props: [(u32, u32); 6] = [
                    (0x0000_0100, u32::from_be_bytes(*b"2.0\0")), // TPM_PT_FAMILY_INDICATOR
                    (0x0000_0101, 0),                             // TPM_PT_LEVEL
                    (0x0000_0102, 138),                           // TPM_PT_REVISION (1.38)
                    (0x0000_0105, u32::from_be_bytes(*b"INTC")),  // TPM_PT_MANUFACTURER
                    (0x0000_010B, 0x0007_0002),                   // TPM_PT_FIRMWARE_VERSION_1
                    (0x0000_0112, u32_of(PCR_COUNT)),             // TPM_PT_PCR_COUNT
                ];
                let mut p = u32_of(props.len()).to_be_bytes().to_vec();
                for (property, value) in props {
                    p.extend_from_slice(&property.to_be_bytes());
                    p.extend_from_slice(&value.to_be_bytes());
                }
                p
            }
            _ => {
                self.write_error_response(0x0000_01C4); // TPM_RC_VALUE, parameter 1
                return;
            }
        };

        let response_size = 10 + 1 + 4 + payload.len();
        self.rsp_buffer[0..2].copy_from_slice(&[0x00, 0xC4]); // TPM_ST_NO_SESSIONS
        self.rsp_buffer[2..6].copy_from_slice(&u32_of(response_size).to_be_bytes());
        self.rsp_buffer[6..10].copy_from_slice(&0u32.to_be_bytes()); // success
        self.rsp_buffer[10] = 0; // moreData = NO
        self.rsp_buffer[11..15].copy_from_slice(&capability.to_be_bytes());
        self.rsp_buffer[15..15 + payload.len()].copy_from_slice(&payload);
    }

    /// Handle `TPM2_GetRandom` — returns pseudo-random bytes
    fn handle_get_random(&mut self) {
        // Parse requested byte count from command
        let bytes_requested = if self.cmd_buffer.len() >= 12 {
            u16::from_be_bytes(self.cmd_buffer[10..12].try_into().unwrap_or([0; 2])) as usize
        } else {
            0
        };

        let bytes_requested = bytes_requested.min(48); // Cap at 48

        // Response: header (10) + digested (2) + size (2) + data
        let response_size = 10 + 2 + 2 + bytes_requested;
        self.rsp_buffer[0..2].copy_from_slice(&[0x00, 0xC4]); // tag
        self.rsp_buffer[2..6].copy_from_slice(&(u32_of(response_size)).to_be_bytes());
        self.rsp_buffer[6..10].copy_from_slice(&0u32.to_be_bytes()); // success
        self.rsp_buffer[10..12].copy_from_slice(&(u16_of(bytes_requested)).to_be_bytes());
        self.rsp_buffer[12..14].copy_from_slice(&(u16_of(bytes_requested)).to_be_bytes());

        // Fill from the per-instance PRNG so successive GetRandom calls differ (a fixed
        // pattern both breaks a guest seeding its CSPRNG and is a detection tell). The
        // stream is seeded deterministically; a production build should reseed from a
        // host entropy source.
        for i in 0..bytes_requested {
            self.rsp_buffer[14 + i] = self.next_random_byte();
        }
    }

    /// One step of an `xorshift64*` generator — fast, non-cryptographic; adequate for a
    /// model TPM's `GetRandom` where the requirement is that successive reads differ.
    fn next_random_byte(&mut self) -> u8 {
        let mut x = self.rng_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng_state = x;
        u8_of((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) & 0xFF)
    }
}

impl Default for VirtualTpm {
    fn default() -> Self {
        Self::new(TpmInterface::Crb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tpm_creates() {
        let tpm = VirtualTpm::new(TpmInterface::Crb);
        assert!(!tpm.started);
        assert_eq!(tpm.interface, TpmInterface::Crb);
    }

    #[test]
    fn tpm_startup_command() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        // Write TPM2_Startup command
        let cmd = [
            0x80, 0x01, // TPM_ST_NO_SESSIONS
            0x00, 0x00, 0x00, 0x0C, // Size = 12
            0x00, 0x00, 0x01, 0x44, // TPM_CC_Startup
            0x00, 0x00, // Startup type: Clear
        ];
        tpm.cmd_buffer[..cmd.len()].copy_from_slice(&cmd);
        tpm.process_command();

        assert!(tpm.started);
        // Check response is success
        let rc = u32::from_be_bytes(tpm.rsp_buffer[6..10].try_into().unwrap());
        assert_eq!(rc, 0);
    }

    #[test]
    fn tpm_register_read() {
        let tpm = VirtualTpm::new(TpmInterface::Crb);
        let loc_state = tpm.read_register(crb_regs::LOC_STATE, 4);
        assert_eq!(loc_state, 0x81); // Established, locality 0 active
    }

    #[test]
    fn tpm_locality_request() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        // Request locality
        tpm.write_register(crb_regs::LOC_CTRL, 2, 4);
        assert_eq!(tpm.loc_sts, 0x01); // Granted
    }

    #[test]
    fn tpm_get_random() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        tpm.started = true;

        let cmd = [
            0x80, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x01, 0x7B, // TPM_CC_GetRandom
            0x00, 0x10, // 16 bytes
        ];
        tpm.cmd_buffer[..cmd.len()].copy_from_slice(&cmd);
        tpm.process_command();

        let rc = u32::from_be_bytes(tpm.rsp_buffer[6..10].try_into().unwrap());
        assert_eq!(rc, 0); // Success
    }

    #[test]
    fn tpm_get_random_successive_calls_differ() {
        // GetRandom must not return a fixed byte pattern: successive calls must yield
        // different bytes (a constant stream breaks guest entropy and is a VM tell).
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        tpm.started = true;
        let cmd = [
            0x80, 0x01, 0x00, 0x00, 0x00, 0x0C, 0x00, 0x00, 0x01, 0x7B, // GetRandom
            0x00, 0x10, // 16 bytes
        ];
        tpm.cmd_buffer[..cmd.len()].copy_from_slice(&cmd);
        tpm.process_command();
        let first = tpm.rsp_buffer[14..30].to_vec();
        tpm.cmd_buffer[..cmd.len()].copy_from_slice(&cmd);
        tpm.process_command();
        let second = tpm.rsp_buffer[14..30].to_vec();
        assert_ne!(first, second, "successive GetRandom outputs must differ");
        // Not the old fixed (i*7+13) pattern.
        assert_ne!(
            first,
            (0..16u8)
                .map(|i| i.wrapping_mul(7).wrapping_add(13))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn tpm_pcr_extend_then_read_uses_real_sha256() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        tpm.started = true;

        // PCR_Extend(index=0) folding in a single digest byte 0xAB.
        // header(10) + pcrHandle(4=0) + 4-byte field + digest(1).
        let extend = [
            0x80, 0x01, 0x00, 0x00, 0x00, 0x13, // tag, size = 19
            0x00, 0x00, 0x01, 0x82, // TPM2_CC_PCR_Extend
            0x00, 0x00, 0x00, 0x00, // pcrHandle = 0
            0x00, 0x00, 0x00, 0x00, // skipped field
            0xAB, // digest byte
        ];
        tpm.cmd_buffer[..extend.len()].copy_from_slice(&extend);
        tpm.process_command();
        assert_eq!(
            u32::from_be_bytes(tpm.rsp_buffer[6..10].try_into().unwrap()),
            0,
            "extend succeeds"
        );

        // PCR0 must now equal SHA256(0x00*32 || 0xAB).
        let mut input = vec![0u8; 32];
        input.push(0xAB);
        let expected = crate::crypto::sha256(&input);
        assert_eq!(tpm.pcr_sha256[0], expected);

        // PCR_Read(index=0) returns that digest in the response body.
        let read = [
            0x80, 0x01, 0x00, 0x00, 0x00, 0x0E, // tag, size = 14
            0x00, 0x00, 0x01, 0x7E, // TPM2_CC_PCR_Read
            0x00, 0x00, 0x00, 0x00, // pcrHandle = 0
        ];
        tpm.cmd_buffer[..read.len()].copy_from_slice(&read);
        tpm.process_command();
        assert_eq!(
            u32::from_be_bytes(tpm.rsp_buffer[6..10].try_into().unwrap()),
            0
        );
        assert_eq!(&tpm.rsp_buffer[10..10 + 32], &expected);
    }

    /// Build a TPM2 command: 2-byte tag, 4-byte size, 4-byte command code, payload.
    fn tpm_cmd(cc: u32, payload: &[u8]) -> Vec<u8> {
        let mut c = vec![0x80, 0x01];
        c.extend_from_slice(&u32_of(10 + payload.len()).to_be_bytes());
        c.extend_from_slice(&cc.to_be_bytes());
        c.extend_from_slice(payload);
        c
    }

    #[test]
    fn execute_command_dispatches_and_returns_the_response() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        let rsp = tpm.execute_command(&tpm_cmd(0x0000_0144, &[0x00, 0x00])); // Startup(CLEAR)
        assert!(tpm.started, "byte-vec front drives the same TPM state");
        assert_eq!(rsp.len(), 10);
        assert_eq!(&rsp[6..10], &[0, 0, 0, 0], "success response code");
    }

    #[test]
    fn nv_write_then_read_round_trips() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        tpm.started = true;

        // NV_Write(index = 0x0100_0001, data = "sealed-blob")
        let mut payload = 0x0100_0001u32.to_be_bytes().to_vec();
        payload.extend_from_slice(b"sealed-blob");
        let rsp = tpm.execute_command(&tpm_cmd(0x0000_0137, &payload));
        assert_eq!(&rsp[6..10], &[0, 0, 0, 0], "NV_Write must succeed");

        // NV_Read(index = 0x0100_0001) returns the stored blob.
        let rsp = tpm.execute_command(&tpm_cmd(0x0000_014E, &0x0100_0001u32.to_be_bytes()));
        assert_eq!(&rsp[6..10], &[0, 0, 0, 0], "NV_Read must succeed");
        let len = usize::from(u16::from_be_bytes([rsp[10], rsp[11]]));
        assert_eq!(&rsp[12..12 + len], b"sealed-blob");
    }

    #[test]
    fn nv_read_of_undefined_index_returns_rc_handle() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        tpm.started = true;
        let rsp = tpm.execute_command(&tpm_cmd(0x0000_014E, &0x0100_0099u32.to_be_bytes()));
        assert_eq!(
            u32::from_be_bytes(rsp[6..10].try_into().unwrap()),
            0x0000_018B,
            "undefined NV index is TPM_RC_HANDLE, not permissive success"
        );
    }

    #[test]
    fn get_capability_pcrs_returns_a_sha256_bank_of_24() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        tpm.started = true;
        // GetCapability(TPM_CAP_PCRS, property=0, count=1)
        let mut payload = 0x0000_0005u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(&1u32.to_be_bytes());
        let rsp = tpm.execute_command(&tpm_cmd(0x0000_017A, &payload));

        assert_eq!(&rsp[6..10], &[0, 0, 0, 0], "must succeed");
        assert_eq!(rsp[10], 0, "moreData = NO");
        assert_eq!(&rsp[11..15], &0x0000_0005u32.to_be_bytes(), "capability");
        assert_eq!(&rsp[15..19], &1u32.to_be_bytes(), "one TPMS_PCR_SELECTION");
        assert_eq!(&rsp[19..21], &0x000Bu16.to_be_bytes(), "SHA-256 bank");
        assert_eq!(rsp[21], 3, "sizeofSelect");
        assert_eq!(&rsp[22..25], &[0xFF, 0xFF, 0xFF], "all 24 PCRs allocated");
    }

    #[test]
    fn get_capability_properties_carry_family_and_manufacturer() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        tpm.started = true;
        let mut payload = 0x0000_0006u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&0x100u32.to_be_bytes());
        payload.extend_from_slice(&8u32.to_be_bytes());
        let rsp = tpm.execute_command(&tpm_cmd(0x0000_017A, &payload));

        assert_eq!(&rsp[6..10], &[0, 0, 0, 0], "must succeed");
        let count = u32::from_be_bytes(rsp[15..19].try_into().unwrap());
        let mut props = std::collections::HashMap::new();
        for i in 0..usize_of(count) {
            let off = 19 + i * 8;
            props.insert(
                u32::from_be_bytes(rsp[off..off + 4].try_into().unwrap()),
                u32::from_be_bytes(rsp[off + 4..off + 8].try_into().unwrap()),
            );
        }
        assert_eq!(
            props.get(&0x100),
            Some(&u32::from_be_bytes(*b"2.0\0")),
            "TPM_PT_FAMILY_INDICATOR"
        );
        assert_eq!(
            props.get(&0x105),
            Some(&u32::from_be_bytes(*b"INTC")),
            "TPM_PT_MANUFACTURER"
        );
        assert_eq!(props.get(&0x112), Some(&24), "TPM_PT_PCR_COUNT");
    }

    #[test]
    fn get_capability_unknown_returns_rc_value() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        tpm.started = true;
        let mut payload = 0x0000_00FFu32.to_be_bytes().to_vec();
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(&1u32.to_be_bytes());
        let rsp = tpm.execute_command(&tpm_cmd(0x0000_017A, &payload));
        assert_eq!(
            u32::from_be_bytes(rsp[6..10].try_into().unwrap()),
            0x0000_01C4,
            "unsupported capability is TPM_RC_VALUE, not an empty success"
        );
    }

    #[test]
    fn tpm_crb_data_buffer_rw() {
        let mut tpm = VirtualTpm::new(TpmInterface::Crb);
        // Write to data buffer
        tpm.write_register(crb_regs::DATA_BUFFER, 0x42, 1);
        let val = tpm.read_register(crb_regs::DATA_BUFFER, 1);
        // Data buffer read returns from rsp_buffer, write goes to cmd_buffer
        // so they won't match, but both operations should not panic
        let _ = val; // only checking that the read/write path does not panic
    }
}

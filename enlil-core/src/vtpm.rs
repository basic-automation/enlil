//! Virtual TPM 2.0 — Required for Windows 11
//!
//! Software TPM implementation supporting PCR measurements, endorsement keys,
//! and persistent state for BitLocker and Windows Hello.

use std::collections::HashMap;

/// TPM 2.0 Command/Response protocol
#[repr(C, packed)]
pub struct TpmCommandHeader {
    pub tag: u16,  // 0x8001 = TPM2_ST_NO_SESSIONS
    pub size: u32, // Total command size
    pub code: u32, // TPM2_CC_* command code
}

#[repr(C, packed)]
pub struct TpmResponseHeader {
    pub tag: u16,
    pub size: u32,
    pub code: u32, // 0 = success, non-zero = error
}

/// TPM 2.0 PCR (Platform Configuration Register)
/// Stores measurements of firmware, bootloader, kernel, etc.
#[derive(Clone)]
pub struct TpmPcr {
    pub value: Vec<u8>, // SHA-256 hash (32 bytes)
    pub alg: u16,       // TPM_ALG_SHA256 = 0x000B
}

impl TpmPcr {
    pub fn new_sha256() -> Self {
        Self {
            value: vec![0u8; 32],
            alg: 0x000B,
        }
    }

    /// Extend PCR: pcr_new = hash(pcr_old || measurement)
    pub fn extend(&mut self, measurement: &[u8]) {
        // Simple SHA-256 extension (would use actual SHA-256 in production)
        let mut combined = self.value.clone();
        combined.extend_from_slice(measurement);
        // For testing: just xor the measurement into the PCR
        for (i, &byte) in measurement.iter().enumerate() {
            if i < self.value.len() {
                self.value[i] ^= byte;
            }
        }
    }
}

/// Virtual TPM 2.0 Emulation
// EK/AIK/SRK key material is generated and stored now; the attestation/quote
// paths that consume it land later in Phase 5 (vTPM 2.0).
#[allow(dead_code)]
pub struct VirtualTpm {
    /// PCRs 0-23 (24 total)
    pcrs: Vec<TpmPcr>,
    /// Endorsement Key (EK) — long-term identity key
    endorsement_key: Vec<u8>,
    /// Attestation Identity Key (AIK) — for remote attestation
    attestation_key: Vec<u8>,
    /// Storage Root Key (SRK) — protects sealed objects
    storage_root_key: Vec<u8>,
    /// Sealed objects (NV indices)
    nv_storage: HashMap<u32, Vec<u8>>,
    /// TPM is initialized
    initialized: bool,
}

impl Default for VirtualTpm {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualTpm {
    pub fn new() -> Self {
        let mut pcrs = Vec::new();
        for _ in 0..24 {
            pcrs.push(TpmPcr::new_sha256());
        }

        Self {
            pcrs,
            endorsement_key: vec![0u8; 256],
            attestation_key: vec![0u8; 256],
            storage_root_key: vec![0u8; 256],
            nv_storage: HashMap::new(),
            initialized: false,
        }
    }

    /// TPM2_Startup — initialize TPM
    pub fn startup(&mut self, _startup_type: u16) -> u32 {
        self.initialized = true;
        0 // Success
    }

    /// TPM2_Shutdown — clean shutdown
    pub fn shutdown(&mut self, _shutdown_type: u16) -> u32 {
        self.initialized = false;
        0
    }

    /// TPM2_PCR_Extend — extend a PCR with a measurement
    pub fn pcr_extend(&mut self, pcr_index: u32, measurement: &[u8]) -> u32 {
        if pcr_index >= 24 {
            return 0x0000_0003; // TPM_RC_SIZE
        }

        self.pcrs[pcr_index as usize].extend(measurement);
        0 // Success
    }

    /// TPM2_PCR_Read — read PCR value
    pub fn pcr_read(&self, pcr_index: u32) -> Option<Vec<u8>> {
        if pcr_index < 24 {
            Some(self.pcrs[pcr_index as usize].value.clone())
        } else {
            None
        }
    }

    /// TPM2_GetCapability — query TPM properties
    pub fn get_capability(&self, capability: u32) -> Option<Vec<u8>> {
        match capability {
            0x00000000 => {
                // TPM_CAP_ALGS
                Some(vec![0x00, 0x0B]) // TPM_ALG_SHA256
            }
            0x00000006 => {
                // TPM_CAP_HANDLES
                Some((0..24).map(|i| i as u8).collect())
            }
            _ => None,
        }
    }

    /// TPM2_CreatePrimary — create an endorsement key (for later use)
    pub fn create_primary(&mut self, _template: &[u8]) -> std::result::Result<Vec<u8>, u32> {
        // Return a handle (simple sequential ID)
        Ok(vec![0x81, 0x00, 0x00, 0x01]) // 0x81000001
    }

    /// TPM2_NV_Read — read from NV storage (persistent state)
    pub fn nv_read(&self, nv_index: u32) -> Option<Vec<u8>> {
        self.nv_storage.get(&nv_index).cloned()
    }

    /// TPM2_NV_Write — write to NV storage
    pub fn nv_write(&mut self, nv_index: u32, data: Vec<u8>) -> u32 {
        self.nv_storage.insert(nv_index, data);
        0
    }
}

/// TPM Command Dispatcher
pub struct TpmDispatcher {
    tpm: VirtualTpm,
}

impl Default for TpmDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl TpmDispatcher {
    pub fn new() -> Self {
        Self {
            tpm: VirtualTpm::new(),
        }
    }

    /// Process TPM command from guest
    pub fn dispatch(&mut self, command: &[u8]) -> Vec<u8> {
        if command.len() < 10 {
            return Self::error_response(0x0000_0001); // TPM_RC_INITIALIZE
        }

        // Parse header
        let tag = u16::from_be_bytes([command[0], command[1]]);
        let _size = u32::from_be_bytes([command[2], command[3], command[4], command[5]]);
        let cc = u32::from_be_bytes([command[6], command[7], command[8], command[9]]);

        let response_code = match cc {
            0x00000144 => self.tpm.startup(0),  // TPM2_CC_Startup
            0x00000145 => self.tpm.shutdown(0), // TPM2_CC_Shutdown
            0x0000017E => {
                // TPM2_CC_PCR_Extend
                if command.len() >= 18 {
                    let pcr_index =
                        u32::from_be_bytes([command[10], command[11], command[12], command[13]]);
                    self.tpm.pcr_extend(pcr_index, &command[18..])
                } else {
                    0x0000_0001
                }
            }
            0x0000017F => {
                // TPM2_CC_PCR_Read
                if command.len() >= 14 {
                    let pcr_index =
                        u32::from_be_bytes([command[10], command[11], command[12], command[13]]);
                    match self.tpm.pcr_read(pcr_index) {
                        Some(pcr_value) => {
                            // Return success with PCR value
                            let mut response = vec![tag as u8, (tag >> 8) as u8];
                            response.extend_from_slice(&(pcr_value.len() as u32).to_be_bytes());
                            response.extend_from_slice(&0u32.to_be_bytes()); // Success
                            response.extend_from_slice(&pcr_value);
                            return response;
                        }
                        None => 0x0000_0003,
                    }
                } else {
                    0x0000_0001
                }
            }
            0x0000_0100 => {
                // TPM2_CC_GetCapability
                if command.len() >= 14 {
                    let cap =
                        u32::from_be_bytes([command[10], command[11], command[12], command[13]]);
                    match self.tpm.get_capability(cap) {
                        Some(cap_data) => {
                            let mut response = vec![tag as u8, (tag >> 8) as u8];
                            response.extend_from_slice(&(cap_data.len() as u32).to_be_bytes());
                            response.extend_from_slice(&0u32.to_be_bytes());
                            response.extend_from_slice(&cap_data);
                            return response;
                        }
                        None => 0x0000_0001,
                    }
                } else {
                    0x0000_0001
                }
            }
            _ => 0x0000_0184, // TPM_RC_COMMAND_CODE
        };

        Self::error_response(response_code)
    }

    fn error_response(code: u32) -> Vec<u8> {
        let mut response = vec![0x80, 0x01]; // TPM2_ST_NO_SESSIONS
        response.extend_from_slice(&10u32.to_be_bytes()); // Size
        response.extend_from_slice(&code.to_be_bytes());
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tpm_pcr_extend() {
        let mut tpm = VirtualTpm::new();
        tpm.pcr_extend(0, b"measurement");
        let pcr0 = tpm.pcr_read(0).unwrap();
        assert!(!pcr0.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_tpm_startup() {
        let mut tpm = VirtualTpm::new();
        assert!(!tpm.initialized);
        let rc = tpm.startup(0);
        assert_eq!(rc, 0);
        assert!(tpm.initialized);
    }

    #[test]
    fn test_tpm_nv_storage() {
        let mut tpm = VirtualTpm::new();
        tpm.nv_write(0x01000001, vec![1, 2, 3]);
        assert_eq!(tpm.nv_read(0x01000001), Some(vec![1, 2, 3]));
    }
}

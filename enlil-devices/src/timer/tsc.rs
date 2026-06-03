//! TSC (Time Stamp Counter) management for guest VMs.
//!
//! Each vCPU gets its own TSC offset so that guests see a consistent
//! monotonic clock starting from zero at boot, regardless of when
//! the vCPU was actually created on the host.

/// Per-vCPU TSC configuration.
#[derive(Debug, Clone)]
pub struct TscState {
    /// Offset added to host TSC when guest reads RDTSC.
    /// `guest_tsc = host_tsc + offset`
    pub offset: i64,
    /// TSC frequency in Hz (as seen by the guest).
    pub frequency_hz: u64,
    /// Whether TSC scaling is enabled (for migration between different-speed hosts).
    pub scaling_enabled: bool,
    /// Scaling ratio as a fixed-point value (48.16 format).
    /// `guest_tsc = (host_tsc * ratio) >> 16`
    pub scaling_ratio: u64,
}

impl Default for TscState {
    fn default() -> Self {
        Self {
            offset: 0,
            frequency_hz: 0,
            scaling_enabled: false,
            scaling_ratio: 1 << 16, // 1.0 in 48.16 fixed point
        }
    }
}

/// Manages TSC state for all vCPUs in a VM.
pub struct TscManager {
    /// Host TSC frequency in Hz.
    host_frequency_hz: u64,
    /// Guest-visible TSC frequency.
    guest_frequency_hz: u64,
    /// Per-vCPU TSC state.
    vcpu_states: Vec<TscState>,
    /// Host TSC value at VM creation time (used to compute offsets).
    creation_tsc: u64,
}

impl TscManager {
    /// Create a new TSC manager.
    ///
    /// # Arguments
    ///
    /// * `host_freq` - Host TSC frequency in Hz (from CPUID or calibration).
    /// * `guest_freq` - Desired guest TSC frequency (0 = same as host).
    /// * `vcpu_count` - Number of vCPUs.
    #[must_use]
    pub fn new(host_freq: u64, guest_freq: u64, vcpu_count: usize) -> Self {
        let guest_freq = if guest_freq == 0 {
            host_freq
        } else {
            guest_freq
        };
        let needs_scaling = guest_freq != host_freq;

        // Calculate scaling ratio in 48.16 fixed point
        let ratio = if needs_scaling && host_freq > 0 {
            ((u128::from(guest_freq)) << 16) / (u128::from(host_freq))
        } else {
            1u128 << 16
        };

        // Read current TSC (simulated on non-x86)
        let creation_tsc = Self::read_host_tsc();

        let vcpu_states = (0..vcpu_count)
            .map(|_| TscState {
                // Offset so guest sees TSC starting near 0
                #[allow(clippy::cast_possible_wrap)]
                offset: -(creation_tsc as i64),
                frequency_hz: guest_freq,
                scaling_enabled: needs_scaling,
                scaling_ratio: u64::try_from(ratio).unwrap_or(u64::MAX),
            })
            .collect();

        Self {
            host_frequency_hz: host_freq,
            guest_frequency_hz: guest_freq,
            vcpu_states,
            creation_tsc,
        }
    }

    /// Get the TSC state for a specific vCPU.
    #[must_use]
    pub fn vcpu_state(&self, vcpu_id: usize) -> Option<&TscState> {
        self.vcpu_states.get(vcpu_id)
    }

    /// Get mutable TSC state for a specific vCPU.
    pub fn vcpu_state_mut(&mut self, vcpu_id: usize) -> Option<&mut TscState> {
        self.vcpu_states.get_mut(vcpu_id)
    }

    /// Compute the VMCS `TSC_OFFSET` value for a vCPU.
    #[must_use]
    pub fn vmcs_tsc_offset(&self, vcpu_id: usize) -> i64 {
        self.vcpu_states.get(vcpu_id).map_or(0, |s| s.offset)
    }

    /// Compute what TSC value the guest would see right now.
    #[must_use]
    pub fn guest_tsc_now(&self, vcpu_id: usize) -> u64 {
        let host_tsc = Self::read_host_tsc();
        let Some(state) = self.vcpu_states.get(vcpu_id) else {
            return 0;
        };

        if state.scaling_enabled {
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let scaled = ((u128::from(host_tsc)) * (u128::from(state.scaling_ratio))) >> 16;
            #[allow(
                clippy::cast_possible_wrap,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            return (scaled as i64 + state.offset) as u64;
        }
        #[allow(
            clippy::cast_possible_wrap,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        {
            (host_tsc as i64 + state.offset) as u64
        }
    }

    /// Synchronize a vCPU's TSC to a specific value (used after migration).
    pub fn set_guest_tsc(&mut self, vcpu_id: usize, guest_tsc: u64) {
        let host_tsc = Self::read_host_tsc();
        if let Some(state) = self.vcpu_states.get_mut(vcpu_id) {
            if state.scaling_enabled {
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                let scaled = ((u128::from(host_tsc)) * (u128::from(state.scaling_ratio))) >> 16;
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                {
                    state.offset = guest_tsc as i64 - scaled as i64;
                }
            } else {
                #[allow(clippy::cast_possible_wrap)]
                {
                    state.offset = guest_tsc as i64 - host_tsc as i64;
                }
            }
        }
    }

    /// Host TSC frequency.
    #[must_use]
    pub const fn host_frequency(&self) -> u64 {
        self.host_frequency_hz
    }

    /// Guest-visible TSC frequency.
    #[must_use]
    pub const fn guest_frequency(&self) -> u64 {
        self.guest_frequency_hz
    }

    /// Number of managed vCPUs.
    #[must_use]
    pub const fn vcpu_count(&self) -> usize {
        self.vcpu_states.len()
    }

    /// Host TSC value at VM creation time.
    #[must_use]
    pub const fn creation_tsc(&self) -> u64 {
        self.creation_tsc
    }

    /// Read the host TSC.
    #[cfg(target_arch = "x86_64")]
    fn read_host_tsc() -> u64 {
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn read_host_tsc() -> u64 {
        // Fallback: use monotonic time scaled to ~3GHz
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        // Simulate ~3GHz TSC
        #[allow(clippy::cast_possible_truncation)]
        {
            (nanos * 3 / 1_000_000_000) as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tsc_manager_creation() {
        let mgr = TscManager::new(3_000_000_000, 0, 4);
        assert_eq!(mgr.vcpu_count(), 4);
        assert_eq!(mgr.host_frequency(), 3_000_000_000);
        assert_eq!(mgr.guest_frequency(), 3_000_000_000);
    }

    #[test]
    fn test_tsc_no_scaling() {
        let mgr = TscManager::new(3_000_000_000, 3_000_000_000, 2);
        let state = mgr.vcpu_state(0).unwrap();
        assert!(!state.scaling_enabled);
        assert_eq!(state.scaling_ratio, 1 << 16);
    }

    #[test]
    fn test_tsc_with_scaling() {
        // Guest wants 2GHz TSC on 3GHz host
        let mgr = TscManager::new(3_000_000_000, 2_000_000_000, 1);
        let state = mgr.vcpu_state(0).unwrap();
        assert!(state.scaling_enabled);
        // Ratio should be ~0.667 in 48.16 = ~43690
        #[allow(clippy::cast_possible_truncation)]
        let expected_ratio: u64 = (((2_000_000_000u128) << 16) / 3_000_000_000u128) as u64;
        assert_eq!(state.scaling_ratio, expected_ratio);
    }

    #[test]
    fn test_vmcs_offset() {
        let mgr = TscManager::new(3_000_000_000, 0, 2);
        // Offset should be negative (guest TSC starts near 0)
        let offset = mgr.vmcs_tsc_offset(0);
        assert!(offset <= 0);
    }

    #[test]
    fn test_set_guest_tsc() {
        let mut mgr = TscManager::new(3_000_000_000, 0, 1);
        mgr.set_guest_tsc(0, 1_000_000);
        // After setting, guest_tsc_now should be close to 1_000_000
        let tsc = mgr.guest_tsc_now(0);
        // Allow some drift since host TSC advances between calls
        assert!(tsc >= 1_000_000);
        assert!(tsc < 1_000_000 + 1_000_000_000); // within 1 second of drift
    }

    #[test]
    fn test_invalid_vcpu() {
        let mgr = TscManager::new(3_000_000_000, 0, 2);
        assert!(mgr.vcpu_state(99).is_none());
        assert_eq!(mgr.vmcs_tsc_offset(99), 0);
        assert_eq!(mgr.guest_tsc_now(99), 0);
    }
}

//! vCPU management and scheduling abstractions.

use std::fmt;

/// How vCPUs are scheduled onto physical cores.
#[derive(Debug, Clone, PartialEq)]
pub enum SchedulingPolicy {
    /// 1:1 pinning — each vCPU gets an exclusive physical core.
    Dedicated,
    /// Multiple vCPUs share a physical core with time-slicing.
    TimeSlice { quantum_ms: u32 },
    /// Dedicate when possible, timeslice the remainder.
    Auto,
}

impl Default for SchedulingPolicy {
    fn default() -> Self {
        Self::Auto
    }
}

/// Configuration for a single vCPU.
#[derive(Debug, Clone)]
pub struct VcpuConfig {
    /// vCPU index within the guest.
    pub id: u32,
    /// Physical core to pin to (if dedicated scheduling).
    pub pinned_core: Option<u32>,
}

/// Runtime state of a vCPU.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VcpuState {
    Created,
    Running,
    Paused,
    Stopped,
}

impl fmt::Display for VcpuState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Running => write!(f, "running"),
            Self::Paused => write!(f, "paused"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

/// Manages a set of vCPUs for a single guest.
pub struct VcpuManager {
    configs: Vec<VcpuConfig>,
    states: Vec<VcpuState>,
    policy: SchedulingPolicy,
}

impl VcpuManager {
    /// Create a new vCPU manager from a list of physical core assignments.
    pub fn new(cores: &[u32], policy: SchedulingPolicy) -> Self {
        let configs: Vec<VcpuConfig> = cores
            .iter()
            .enumerate()
            .map(|(i, &core)| VcpuConfig {
                id: i as u32,
                pinned_core: Some(core),
            })
            .collect();
        let states = vec![VcpuState::Created; configs.len()];
        Self { configs, states, policy }
    }

    pub fn count(&self) -> usize {
        self.configs.len()
    }

    pub fn policy(&self) -> &SchedulingPolicy {
        &self.policy
    }

    pub fn configs(&self) -> &[VcpuConfig] {
        &self.configs
    }

    /// Get the set of physical cores this guest uses.
    pub fn physical_cores(&self) -> Vec<u32> {
        self.configs.iter().filter_map(|c| c.pinned_core).collect()
    }
}

//! VM instance management.

use crate::error::Error;
use crate::memory::GuestMemoryConfig;
use crate::vcpu::{SchedulingPolicy, VcpuManager};
use std::fmt;
use std::path::PathBuf;

/// Full configuration for a virtual machine.
#[derive(Debug, Clone)]
pub struct VmConfig {
    pub name: String,
    pub cpus: Vec<u32>,
    pub memory: GuestMemoryConfig,
    pub kernel: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
    pub cmdline: String,
    pub scheduling: SchedulingPolicy,
}

/// Runtime state of a VM.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VmState {
    Created,
    Booting,
    Running,
    Paused,
    Stopped,
    Failed,
}

impl fmt::Display for VmState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Booting => write!(f, "booting"),
            Self::Running => write!(f, "running"),
            Self::Paused => write!(f, "paused"),
            Self::Stopped => write!(f, "stopped"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// A virtual machine instance.
pub struct Vm {
    config: VmConfig,
    state: VmState,
    vcpu_manager: VcpuManager,
}

impl Vm {
    /// Create a new VM from configuration.
    pub fn new(config: VmConfig) -> Result<Self, Error> {
        if config.cpus.is_empty() {
            return Err(Error::Vm("guest must have at least one CPU".into()));
        }
        if config.memory.size_mb == 0 {
            return Err(Error::Memory("guest memory must be > 0".into()));
        }

        let vcpu_manager = VcpuManager::new(&config.cpus, config.scheduling.clone());

        Ok(Self {
            config,
            state: VmState::Created,
            vcpu_manager,
        })
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn state(&self) -> VmState {
        self.state
    }

    pub fn vcpu_count(&self) -> usize {
        self.vcpu_manager.count()
    }

    pub fn config(&self) -> &VmConfig {
        &self.config
    }

    pub fn physical_cores(&self) -> Vec<u32> {
        self.vcpu_manager.physical_cores()
    }
}

//! VM instance management.

use crate::error::Error;
use crate::memory::{GuestMemoryConfig, MemoryManager};
use crate::serial::{SerialConfig, SerialMultiplexer, SerialOutputMode};
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
    pub serial_output: SerialOutputMode,
}

/// Runtime state of a VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Hypervisor — manages all VMs and shared resources.
pub struct Hypervisor {
    vms: Vec<Vm>,
    memory_manager: MemoryManager,
    serial_mux: SerialMultiplexer,
}

impl Hypervisor {
    /// Create a new hypervisor with the given total host memory and reserved bytes.
    #[must_use]
    pub fn new(total_host_memory: u64, reserved_bytes: u64) -> Self {
        Self {
            vms: Vec::new(),
            memory_manager: MemoryManager::new(total_host_memory, reserved_bytes),
            serial_mux: SerialMultiplexer::new(),
        }
    }

    /// Add a VM from config. Allocates memory and registers serial output.
    ///
    /// # Errors
    ///
    /// Returns an error if memory allocation fails or the VM configuration is invalid.
    pub fn add_vm(&mut self, config: VmConfig) -> Result<usize, Error> {
        let guest_id = config.name.clone();

        // Allocate memory
        self.memory_manager
            .allocate(&guest_id, config.memory.size_mb * 1024 * 1024)
            .map_err(|e| Error::Memory(e.to_string()))?;

        // Register serial output
        let serial_config = SerialConfig {
            mode: config.serial_output.clone(),
            ..Default::default()
        };
        self.serial_mux.add_guest(&guest_id, &serial_config);

        let vm = Vm::new(config)?;
        let idx = self.vms.len();
        self.vms.push(vm);
        Ok(idx)
    }

    #[must_use]
    pub fn vm(&self, index: usize) -> Option<&Vm> {
        self.vms.get(index)
    }

    pub fn vm_mut(&mut self, index: usize) -> Option<&mut Vm> {
        self.vms.get_mut(index)
    }

    #[must_use]
    pub const fn vm_count(&self) -> usize {
        self.vms.len()
    }

    #[must_use]
    pub const fn memory_manager(&self) -> &MemoryManager {
        &self.memory_manager
    }

    pub const fn serial_mux(&mut self) -> &mut SerialMultiplexer {
        &mut self.serial_mux
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
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration has no CPUs or zero memory.
    pub fn new(config: VmConfig) -> Result<Self, Error> {
        if config.cpus.is_empty() {
            return Err(Error::Vm("guest must have at least one CPU".into()));
        }
        if config.memory.size_mb == 0 {
            return Err(Error::Memory("guest memory must be > 0".into()));
        }

        let vcpu_manager = VcpuManager::new(&config.name, &config.cpus, config.scheduling);

        Ok(Self {
            config,
            state: VmState::Created,
            vcpu_manager,
        })
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.config.name
    }

    #[must_use]
    pub const fn state(&self) -> VmState {
        self.state
    }

    pub const fn set_state(&mut self, state: VmState) {
        self.state = state;
    }

    #[must_use]
    pub const fn vcpu_count(&self) -> usize {
        self.vcpu_manager.count()
    }

    #[must_use]
    pub const fn config(&self) -> &VmConfig {
        &self.config
    }

    #[must_use]
    pub const fn vcpu_manager(&self) -> &VcpuManager {
        &self.vcpu_manager
    }

    pub const fn vcpu_manager_mut(&mut self) -> &mut VcpuManager {
        &mut self.vcpu_manager
    }

    #[must_use]
    pub fn physical_cores(&self) -> Vec<u32> {
        self.vcpu_manager.physical_cores()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(name: &str, cpus: Vec<u32>, memory_mb: u64) -> VmConfig {
        VmConfig {
            name: name.into(),
            cpus,
            memory: GuestMemoryConfig { size_mb: memory_mb },
            kernel: None,
            initrd: None,
            cmdline: "console=ttyS0".into(),
            scheduling: SchedulingPolicy::Dedicated,
            serial_output: SerialOutputMode::Null,
        }
    }

    #[test]
    fn create_vm() {
        let vm = Vm::new(test_config("test", vec![0, 1], 512)).unwrap();
        assert_eq!(vm.name(), "test");
        assert_eq!(vm.vcpu_count(), 2);
        assert_eq!(vm.state(), VmState::Created);
    }

    #[test]
    fn reject_empty_cpus() {
        assert!(Vm::new(test_config("bad", vec![], 512)).is_err());
    }

    #[test]
    fn reject_zero_memory() {
        assert!(Vm::new(test_config("bad", vec![0], 0)).is_err());
    }

    #[test]
    fn hypervisor_manages_multiple_vms() {
        // 16 GB host, 512 MB reserved
        let mut hv = Hypervisor::new(16 * 1024 * 1024 * 1024, 512 * 1024 * 1024);

        let idx0 = hv.add_vm(test_config("vm1", vec![0, 1], 4096)).unwrap();
        let idx1 = hv.add_vm(test_config("vm2", vec![2, 3], 4096)).unwrap();

        assert_eq!(hv.vm_count(), 2);
        assert_eq!(hv.vm(idx0).unwrap().name(), "vm1");
        assert_eq!(hv.vm(idx1).unwrap().name(), "vm2");

        // Memory regions should exist
        assert!(hv.memory_manager().get_region("vm1").is_some());
        assert!(hv.memory_manager().get_region("vm2").is_some());
    }

    #[test]
    fn hypervisor_serial_routing() {
        use crate::serial::DATA_REG;

        let mut hv = Hypervisor::new(8 * 1024 * 1024 * 1024, 256 * 1024 * 1024);

        hv.add_vm(VmConfig {
            serial_output: SerialOutputMode::Buffer,
            ..test_config("vm1", vec![0], 512)
        })
        .unwrap();

        // Write to serial via UART TX register and verify routing
        for &byte in b"hello" {
            hv.serial_mux().handle_write("vm1", DATA_REG, byte);
        }
        let uart = hv.serial_mux().get_uart("vm1").unwrap();
        let output = uart.output().buffer_contents();
        assert_eq!(output, b"hello");
    }

    #[test]
    fn vm_state_transitions() {
        let mut vm = Vm::new(test_config("test", vec![0], 512)).unwrap();
        assert_eq!(vm.state(), VmState::Created);
        vm.set_state(VmState::Booting);
        assert_eq!(vm.state(), VmState::Booting);
        vm.set_state(VmState::Running);
        assert_eq!(vm.state(), VmState::Running);
        vm.set_state(VmState::Paused);
        assert_eq!(vm.state(), VmState::Paused);
        vm.set_state(VmState::Stopped);
        assert_eq!(vm.state(), VmState::Stopped);
    }

    #[test]
    fn vm_exposes_physical_cores() {
        let vm = Vm::new(test_config("test", vec![2, 5, 7], 1024)).unwrap();
        assert_eq!(vm.physical_cores(), vec![2, 5, 7]);
    }
}

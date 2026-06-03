use std::collections::{HashMap, VecDeque};

/// Represents the state of a vCPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcpuState {
    /// vCPU created but not yet started.
    Created,
    /// vCPU is running.
    Running,
    /// vCPU is paused but can be resumed.
    Paused,
    /// vCPU is stopped and cannot be resumed.
    Stopped,
}

impl std::fmt::Display for VcpuState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "Created"),
            Self::Running => write!(f, "Running"),
            Self::Paused => write!(f, "Paused"),
            Self::Stopped => write!(f, "Stopped"),
        }
    }
}

/// Configuration for a vCPU.
#[derive(Debug, Clone)]
pub struct VcpuConfig {
    /// The ID of the vCPU.
    pub id: u32,
    /// Physical core the vCPU is pinned to.
    pub pinned_core: Option<u32>,
}

/// Affinity binding for a vCPU to a physical core.
#[derive(Debug, Clone)]
pub enum AffinityBinding {
    /// 1:1 pin to a physical core.
    Pinned { physical_core: u32 },
    /// Time-sliced on a physical core.
    Shared { physical_core: u32, quantum_ms: u64 },
}

/// Scheduling policy for vCPUs.
#[derive(Debug, Clone, Copy)]
pub enum SchedulingPolicy {
    /// 1:1 dedicated core per vCPU.
    Dedicated,
    /// Time-sliced with configurable quantum.
    TimeSlice { quantum_ms: u64 },
    /// Automatically choose based on available cores.
    Auto,
}

/// The resolved scheduling plan.
#[derive(Debug, Clone)]
pub struct SchedulingPlan {
    /// Affinity bindings for each vCPU.
    pub bindings: Vec<AffinityBinding>,
    /// The effective policy used.
    pub effective_policy: SchedulingPolicy,
}

const DEFAULT_QUANTUM_MS: u64 = 10;

/// Manages time-slice scheduling for a single physical core.
pub struct TimeSliceScheduler {
    physical_core: u32,
    quantum_ms: u64,
    run_queue: VecDeque<(String, u32)>, // (guest_id, vcpu_id)
    current: usize,
    contexts: HashMap<(String, u32), VcpuContext>,
}

/// Context saved for a paused vCPU.
#[derive(Debug, Clone)]
pub struct VcpuContext {
    /// Placeholder for vCPU context data.
    pub data: Vec<u8>,
}

impl TimeSliceScheduler {
    /// Create a new scheduler for a physical core.
    #[must_use]
    pub fn new(physical_core: u32, quantum_ms: u64) -> Self {
        Self {
            physical_core,
            quantum_ms,
            run_queue: VecDeque::new(),
            current: 0,
            contexts: HashMap::new(),
        }
    }

    /// Get the physical core ID.
    #[must_use]
    pub const fn physical_core(&self) -> u32 {
        self.physical_core
    }

    /// Get the time quantum.
    #[must_use]
    pub const fn quantum_ms(&self) -> u64 {
        self.quantum_ms
    }

    /// Add a vCPU to the scheduler.
    pub fn add_vcpu(&mut self, guest_id: &str, vcpu_id: u32) {
        self.run_queue.push_back((guest_id.to_string(), vcpu_id));
    }

    /// Remove a vCPU from the scheduler.
    pub fn remove_vcpu(&mut self, guest_id: &str, vcpu_id: u32) -> bool {
        let pos = self
            .run_queue
            .iter()
            .position(|(g, v)| g == guest_id && *v == vcpu_id);
        if let Some(p) = pos {
            self.run_queue.remove(p);
            if self.current > p {
                self.current -= 1;
            }
            self.current = self.current.min(self.run_queue.len().saturating_sub(1));
            true
        } else {
            false
        }
    }

    /// Get the current vCPU.
    #[must_use]
    pub fn current_vcpu(&self) -> Option<&(String, u32)> {
        if self.run_queue.is_empty() {
            None
        } else {
            self.run_queue.get(self.current)
        }
    }

    /// Switch to the next vCPU.
    pub fn switch_next(&mut self) -> Option<&(String, u32)> {
        if self.run_queue.is_empty() {
            return None;
        }
        self.current = (self.current + 1) % self.run_queue.len();
        self.run_queue.get(self.current)
    }

    /// Save context for the current vCPU.
    pub fn save_context(&mut self, guest_id: &str, vcpu_id: u32, ctx: VcpuContext) {
        self.contexts.insert((guest_id.to_string(), vcpu_id), ctx);
    }

    /// Get saved context for a vCPU.
    #[must_use]
    pub fn get_context(&self, guest_id: &str, vcpu_id: u32) -> Option<&VcpuContext> {
        self.contexts.get(&(guest_id.to_string(), vcpu_id))
    }

    /// Number of vCPUs sharing this core.
    #[must_use]
    pub fn vcpu_count(&self) -> usize {
        self.run_queue.len()
    }
}

/// Manages a set of vCPUs for a single guest.
pub struct VcpuManager {
    guest_id: String,
    configs: Vec<VcpuConfig>,
    states: Vec<VcpuState>,
    policy: SchedulingPolicy,
    plan: Option<SchedulingPlan>,
}

impl VcpuManager {
    /// Create a new vCPU manager from a list of physical core assignments.
    #[must_use]
    pub fn new(guest_id: &str, cores: &[u32], policy: SchedulingPolicy) -> Self {
        let configs: Vec<VcpuConfig> = cores
            .iter()
            .enumerate()
            .map(|(i, &core)| {
                #[allow(clippy::cast_possible_truncation)]
                let id = i as u32;
                VcpuConfig {
                    id,
                    pinned_core: Some(core),
                }
            })
            .collect();
        let states = vec![VcpuState::Created; configs.len()];
        Self {
            guest_id: guest_id.to_string(),
            configs,
            states,
            policy,
            plan: None,
        }
    }

    #[must_use]
    pub fn guest_id(&self) -> &str {
        &self.guest_id
    }

    #[must_use]
    pub const fn count(&self) -> usize {
        self.configs.len()
    }

    #[must_use]
    pub const fn policy(&self) -> &SchedulingPolicy {
        &self.policy
    }

    #[must_use]
    pub fn configs(&self) -> &[VcpuConfig] {
        &self.configs
    }

    /// Get the state of a specific vCPU.
    #[must_use]
    pub fn get_state(&self, vcpu_id: u32) -> Option<VcpuState> {
        self.states.get(vcpu_id as usize).copied()
    }

    /// Transition a vCPU to Running.
    ///
    /// # Errors
    ///
    /// Returns an error string if the vCPU is not found or is in a stopped state.
    pub fn start_vcpu(&mut self, vcpu_id: u32) -> Result<(), String> {
        let state = self
            .states
            .get_mut(vcpu_id as usize)
            .ok_or_else(|| format!("vCPU {vcpu_id} not found"))?;
        match *state {
            VcpuState::Created | VcpuState::Paused => {
                *state = VcpuState::Running;
                Ok(())
            }
            VcpuState::Running => Ok(()), // already running
            VcpuState::Stopped => Err(format!("vCPU {vcpu_id} is stopped, cannot start")),
        }
    }

    /// Pause a running vCPU.
    ///
    /// # Errors
    ///
    /// Returns an error string if the vCPU is not found or not in a pauseable state.
    pub fn pause_vcpu(&mut self, vcpu_id: u32) -> Result<(), String> {
        let state = self
            .states
            .get_mut(vcpu_id as usize)
            .ok_or_else(|| format!("vCPU {vcpu_id} not found"))?;
        match *state {
            VcpuState::Running => {
                *state = VcpuState::Paused;
                Ok(())
            }
            VcpuState::Paused => Ok(()),
            _ => Err(format!("vCPU {vcpu_id} in state {state}, cannot pause")),
        }
    }

    /// Stop a vCPU.
    ///
    /// # Errors
    ///
    /// Returns an error string if the vCPU is not found.
    pub fn stop_vcpu(&mut self, vcpu_id: u32) -> Result<(), String> {
        let state = self
            .states
            .get_mut(vcpu_id as usize)
            .ok_or_else(|| format!("vCPU {vcpu_id} not found"))?;
        *state = VcpuState::Stopped;
        Ok(())
    }

    /// Start all vCPUs.
    pub fn start_all(&mut self) {
        for state in &mut self.states {
            if *state == VcpuState::Created || *state == VcpuState::Paused {
                *state = VcpuState::Running;
            }
        }
    }

    /// Stop all vCPUs.
    pub fn stop_all(&mut self) {
        for state in &mut self.states {
            *state = VcpuState::Stopped;
        }
    }

    /// Get the set of physical cores this guest uses.
    #[must_use]
    pub fn physical_cores(&self) -> Vec<u32> {
        self.configs.iter().filter_map(|c| c.pinned_core).collect()
    }

    /// Resolve scheduling policy given the available physical cores on the system.
    /// For Auto mode: if we have enough cores, use Dedicated; otherwise `TimeSlice`.
    pub fn resolve_scheduling(&mut self, available_cores: &[u32]) -> SchedulingPlan {
        let requested = &self.configs;
        let plan = match &self.policy {
            SchedulingPolicy::Dedicated => {
                // 1:1 pin each vCPU to its assigned core
                let bindings = requested
                    .iter()
                    .map(|cfg| AffinityBinding::Pinned {
                        physical_core: cfg.pinned_core.unwrap_or(0),
                    })
                    .collect();
                SchedulingPlan {
                    bindings,
                    effective_policy: SchedulingPolicy::Dedicated,
                }
            }
            SchedulingPolicy::TimeSlice { quantum_ms } => {
                // All vCPUs time-share across available cores
                let q = *quantum_ms;
                let bindings = requested
                    .iter()
                    .enumerate()
                    .map(|(i, _cfg)| {
                        let core = available_cores[i % available_cores.len()];
                        AffinityBinding::Shared {
                            physical_core: core,
                            quantum_ms: q,
                        }
                    })
                    .collect();
                SchedulingPlan {
                    bindings,
                    effective_policy: SchedulingPolicy::TimeSlice { quantum_ms: q },
                }
            }
            SchedulingPolicy::Auto => {
                // Check if we have enough dedicated cores
                let can_dedicate = requested.iter().all(|cfg| {
                    cfg.pinned_core
                        .is_some_and(|c| available_cores.contains(&c))
                });

                if can_dedicate {
                    let bindings = requested
                        .iter()
                        .map(|cfg| AffinityBinding::Pinned {
                            physical_core: cfg.pinned_core.unwrap_or(0),
                        })
                        .collect();
                    SchedulingPlan {
                        bindings,
                        effective_policy: SchedulingPolicy::Dedicated,
                    }
                } else {
                    // Fall back to time-slicing
                    let bindings = requested
                        .iter()
                        .enumerate()
                        .map(|(i, _cfg)| {
                            let core = if available_cores.is_empty() {
                                0
                            } else {
                                available_cores[i % available_cores.len()]
                            };
                            AffinityBinding::Shared {
                                physical_core: core,
                                quantum_ms: DEFAULT_QUANTUM_MS,
                            }
                        })
                        .collect();
                    SchedulingPlan {
                        bindings,
                        effective_policy: SchedulingPolicy::TimeSlice {
                            quantum_ms: DEFAULT_QUANTUM_MS,
                        },
                    }
                }
            }
        };

        self.plan = Some(plan.clone());
        plan
    }

    /// Get the resolved scheduling plan, if any.
    #[must_use]
    pub const fn plan(&self) -> Option<&SchedulingPlan> {
        self.plan.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_vcpu_manager() {
        let mgr = VcpuManager::new("test", &[0, 1, 2, 3], SchedulingPolicy::Dedicated);
        assert_eq!(mgr.count(), 4);
        assert_eq!(mgr.guest_id(), "test");
        assert_eq!(mgr.physical_cores(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn initial_state_is_created() {
        let mgr = VcpuManager::new("test", &[0, 1], SchedulingPolicy::Dedicated);
        assert_eq!(mgr.get_state(0), Some(VcpuState::Created));
        assert_eq!(mgr.get_state(1), Some(VcpuState::Created));
        assert_eq!(mgr.get_state(2), None); // doesn't exist
    }

    #[test]
    fn start_pause_stop_lifecycle() {
        let mut mgr = VcpuManager::new("test", &[0], SchedulingPolicy::Dedicated);
        assert_eq!(mgr.get_state(0), Some(VcpuState::Created));

        mgr.start_vcpu(0).unwrap();
        assert_eq!(mgr.get_state(0), Some(VcpuState::Running));

        mgr.pause_vcpu(0).unwrap();
        assert_eq!(mgr.get_state(0), Some(VcpuState::Paused));

        mgr.start_vcpu(0).unwrap(); // resume from paused
        assert_eq!(mgr.get_state(0), Some(VcpuState::Running));

        mgr.stop_vcpu(0).unwrap();
        assert_eq!(mgr.get_state(0), Some(VcpuState::Stopped));
    }

    #[test]
    fn start_all_vcpus() {
        let mut mgr = VcpuManager::new("test", &[0, 1, 2], SchedulingPolicy::Dedicated);
        mgr.start_all();
        assert_eq!(mgr.get_state(0), Some(VcpuState::Running));
        assert_eq!(mgr.get_state(1), Some(VcpuState::Running));
        assert_eq!(mgr.get_state(2), Some(VcpuState::Running));
    }

    #[test]
    fn stop_all_vcpus() {
        let mut mgr = VcpuManager::new("test", &[0, 1, 2], SchedulingPolicy::Dedicated);
        mgr.start_all();
        mgr.stop_all();
        assert_eq!(mgr.get_state(0), Some(VcpuState::Stopped));
        assert_eq!(mgr.get_state(1), Some(VcpuState::Stopped));
        assert_eq!(mgr.get_state(2), Some(VcpuState::Stopped));
    }

    #[test]
    fn timeslice_scheduler_roundrobin() {
        let mut sched = TimeSliceScheduler::new(0, 10);
        sched.add_vcpu("g1", 0);
        sched.add_vcpu("g1", 1);
        sched.add_vcpu("g2", 0);

        assert_eq!(sched.current_vcpu(), Some(&("g1".to_string(), 0)));
        assert_eq!(sched.switch_next(), Some(&("g1".to_string(), 1)));
        assert_eq!(sched.switch_next(), Some(&("g2".to_string(), 0)));
        assert_eq!(sched.switch_next(), Some(&("g1".to_string(), 0)));
    }

    #[test]
    fn timeslice_scheduler_remove() {
        let mut sched = TimeSliceScheduler::new(0, 10);
        sched.add_vcpu("g1", 0);
        sched.add_vcpu("g1", 1);
        assert_eq!(sched.vcpu_count(), 2);

        sched.remove_vcpu("g1", 0);
        assert_eq!(sched.vcpu_count(), 1);
        assert!(!sched.remove_vcpu("g1", 0)); // already removed
    }

    #[test]
    fn scheduling_plan_dedicated() {
        let mut mgr = VcpuManager::new("test", &[0, 1], SchedulingPolicy::Dedicated);
        let plan = mgr.resolve_scheduling(&[0, 1, 2, 3]);
        assert_eq!(plan.bindings.len(), 2);
    }

    #[test]
    fn resolve_auto_falls_back_to_timeslice() {
        let mut mgr = VcpuManager::new("test", &[0, 1, 2], SchedulingPolicy::Auto);
        let plan = mgr.resolve_scheduling(&[0]); // only 1 core
        match plan.effective_policy {
            SchedulingPolicy::TimeSlice { .. } => {} // good
            _ => panic!("should have fallen back to timeslice"),
        }
    }
}

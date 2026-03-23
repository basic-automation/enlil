//! vCPU management and scheduling abstractions.
//!
//! Provides core dedication (1:1 pinning), time-slicing fallback,
//! and automatic scheduling resolution for Phase 1.

use std::collections::HashMap;
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

/// Default time-slice quantum in milliseconds.
pub const DEFAULT_QUANTUM_MS: u32 = 10;

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

/// Describes how a vCPU is bound to a physical core.
#[derive(Debug, Clone, PartialEq)]
pub enum AffinityBinding {
    /// Exclusive 1:1 pin to a physical core.
    Pinned { physical_core: u32 },
    /// Shares a physical core with other vCPUs via time-slicing.
    Shared { physical_core: u32, quantum_ms: u32 },
}

/// Resolved scheduling plan for a set of vCPUs.
/// Maps each vCPU id to its affinity binding.
#[derive(Debug, Clone)]
pub struct SchedulingPlan {
    pub bindings: Vec<AffinityBinding>,
    pub effective_policy: SchedulingPolicy,
}

/// Saved vCPU execution context for time-slice context switching.
/// This is the abstraction layer — actual register save/restore
/// will be implemented via KVM ioctls (Linux) or VMCS/VMCB (bare-metal).
#[derive(Debug, Clone, Default)]
pub struct VcpuContext {
    /// General-purpose registers.
    pub gp_regs: GpRegisters,
    /// Segment registers.
    pub seg_regs: SegmentRegisters,
    /// Control registers.
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    /// EFER MSR.
    pub efer: u64,
    /// Instruction pointer and flags.
    pub rip: u64,
    pub rflags: u64,
    /// FPU/SSE/AVX state (opaque blob, sized for XSAVE area).
    /// 4096 bytes covers XSAVE with AVX-512.
    pub xsave_area: Vec<u8>,
}

/// x86-64 general-purpose registers.
#[derive(Debug, Clone, Default)]
pub struct GpRegisters {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

/// x86-64 segment registers.
#[derive(Debug, Clone, Default)]
pub struct SegmentRegisters {
    pub cs: SegmentReg,
    pub ds: SegmentReg,
    pub es: SegmentReg,
    pub fs: SegmentReg,
    pub gs: SegmentReg,
    pub ss: SegmentReg,
}

/// A single segment register.
#[derive(Debug, Clone, Default)]
pub struct SegmentReg {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub attrib: u16,
}

/// Time-slice scheduler for a single physical core.
/// Manages a round-robin queue of vCPUs that share the core.
#[derive(Debug)]
pub struct TimeSliceScheduler {
    /// Physical core this scheduler manages.
    pub physical_core: u32,
    /// Time quantum per vCPU in milliseconds.
    pub quantum_ms: u32,
    /// Queue of (guest_id, vcpu_id) waiting to run.
    run_queue: Vec<(String, u32)>,
    /// Index of the currently running vCPU in the queue.
    current: usize,
    /// Saved contexts for each vCPU, keyed by (guest_id, vcpu_id).
    contexts: HashMap<(String, u32), VcpuContext>,
}

impl TimeSliceScheduler {
    pub fn new(physical_core: u32, quantum_ms: u32) -> Self {
        Self {
            physical_core,
            quantum_ms,
            run_queue: Vec::new(),
            current: 0,
            contexts: HashMap::new(),
        }
    }

    /// Add a vCPU to this scheduler's run queue.
    pub fn add_vcpu(&mut self, guest_id: &str, vcpu_id: u32) {
        let key = (guest_id.to_string(), vcpu_id);
        if !self.run_queue.contains(&key) {
            self.run_queue.push(key.clone());
            self.contexts.insert(key, VcpuContext::default());
        }
    }

    /// Remove a vCPU from the run queue.
    pub fn remove_vcpu(&mut self, guest_id: &str, vcpu_id: u32) -> bool {
        let key = (guest_id.to_string(), vcpu_id);
        if let Some(pos) = self.run_queue.iter().position(|k| *k == key) {
            self.run_queue.remove(pos);
            self.contexts.remove(&key);
            if self.current >= self.run_queue.len() && !self.run_queue.is_empty() {
                self.current = 0;
            }
            true
        } else {
            false
        }
    }

    /// Get the currently scheduled vCPU.
    pub fn current_vcpu(&self) -> Option<&(String, u32)> {
        self.run_queue.get(self.current)
    }

    /// Advance to the next vCPU in the round-robin queue.
    /// Returns the new current vCPU, or None if queue is empty.
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
    pub fn get_context(&self, guest_id: &str, vcpu_id: u32) -> Option<&VcpuContext> {
        self.contexts.get(&(guest_id.to_string(), vcpu_id))
    }

    /// Number of vCPUs sharing this core.
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
    pub fn new(guest_id: &str, cores: &[u32], policy: SchedulingPolicy) -> Self {
        let configs: Vec<VcpuConfig> = cores
            .iter()
            .enumerate()
            .map(|(i, &core)| VcpuConfig {
                id: i as u32,
                pinned_core: Some(core),
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

    pub fn guest_id(&self) -> &str {
        &self.guest_id
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

    /// Get the state of a specific vCPU.
    pub fn get_state(&self, vcpu_id: u32) -> Option<VcpuState> {
        self.states.get(vcpu_id as usize).copied()
    }

    /// Transition a vCPU to Running.
    pub fn start_vcpu(&mut self, vcpu_id: u32) -> Result<(), String> {
        let state = self.states.get_mut(vcpu_id as usize)
            .ok_or_else(|| format!("vCPU {} not found", vcpu_id))?;
        match *state {
            VcpuState::Created | VcpuState::Paused => {
                *state = VcpuState::Running;
                Ok(())
            }
            VcpuState::Running => Ok(()), // already running
            VcpuState::Stopped => Err(format!("vCPU {} is stopped, cannot start", vcpu_id)),
        }
    }

    /// Pause a running vCPU.
    pub fn pause_vcpu(&mut self, vcpu_id: u32) -> Result<(), String> {
        let state = self.states.get_mut(vcpu_id as usize)
            .ok_or_else(|| format!("vCPU {} not found", vcpu_id))?;
        match *state {
            VcpuState::Running => {
                *state = VcpuState::Paused;
                Ok(())
            }
            VcpuState::Paused => Ok(()),
            _ => Err(format!("vCPU {} in state {}, cannot pause", vcpu_id, state)),
        }
    }

    /// Stop a vCPU.
    pub fn stop_vcpu(&mut self, vcpu_id: u32) -> Result<(), String> {
        let state = self.states.get_mut(vcpu_id as usize)
            .ok_or_else(|| format!("vCPU {} not found", vcpu_id))?;
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
    pub fn physical_cores(&self) -> Vec<u32> {
        self.configs.iter().filter_map(|c| c.pinned_core).collect()
    }

    /// Resolve scheduling policy given the available physical cores on the system.
    /// For Auto mode: if we have enough cores, use Dedicated; otherwise TimeSlice.
    pub fn resolve_scheduling(&mut self, available_cores: &[u32]) -> SchedulingPlan {
        let requested = &self.configs;
        let plan = match &self.policy {
            SchedulingPolicy::Dedicated => {
                // 1:1 pin each vCPU to its assigned core
                let bindings = requested.iter().map(|cfg| {
                    AffinityBinding::Pinned {
                        physical_core: cfg.pinned_core.unwrap_or(0),
                    }
                }).collect();
                SchedulingPlan {
                    bindings,
                    effective_policy: SchedulingPolicy::Dedicated,
                }
            }
            SchedulingPolicy::TimeSlice { quantum_ms } => {
                // All vCPUs time-share across available cores
                let q = *quantum_ms;
                let bindings = requested.iter().enumerate().map(|(i, _cfg)| {
                    let core = available_cores[i % available_cores.len()];
                    AffinityBinding::Shared { physical_core: core, quantum_ms: q }
                }).collect();
                SchedulingPlan {
                    bindings,
                    effective_policy: SchedulingPolicy::TimeSlice { quantum_ms: q },
                }
            }
            SchedulingPolicy::Auto => {
                // Check if we have enough dedicated cores
                let can_dedicate = requested.iter().all(|cfg| {
                    cfg.pinned_core.map_or(false, |c| available_cores.contains(&c))
                });

                if can_dedicate {
                    let bindings = requested.iter().map(|cfg| {
                        AffinityBinding::Pinned {
                            physical_core: cfg.pinned_core.unwrap_or(0),
                        }
                    }).collect();
                    SchedulingPlan {
                        bindings,
                        effective_policy: SchedulingPolicy::Dedicated,
                    }
                } else {
                    // Fall back to time-slicing
                    let bindings = requested.iter().enumerate().map(|(i, _cfg)| {
                        let core = if available_cores.is_empty() {
                            0
                        } else {
                            available_cores[i % available_cores.len()]
                        };
                        AffinityBinding::Shared {
                            physical_core: core,
                            quantum_ms: DEFAULT_QUANTUM_MS,
                        }
                    }).collect();
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
    pub fn plan(&self) -> Option<&SchedulingPlan> {
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

        // Can't start a stopped vCPU
        assert!(mgr.start_vcpu(0).is_err());
    }

    #[test]
    fn start_all_stop_all() {
        let mut mgr = VcpuManager::new("test", &[0, 1, 2], SchedulingPolicy::Dedicated);
        mgr.start_all();
        for i in 0..3 {
            assert_eq!(mgr.get_state(i), Some(VcpuState::Running));
        }
        mgr.stop_all();
        for i in 0..3 {
            assert_eq!(mgr.get_state(i), Some(VcpuState::Stopped));
        }
    }

    #[test]
    fn resolve_dedicated_when_cores_available() {
        let mut mgr = VcpuManager::new("test", &[0, 1], SchedulingPolicy::Auto);
        let plan = mgr.resolve_scheduling(&[0, 1, 2, 3]);
        assert_eq!(plan.effective_policy, SchedulingPolicy::Dedicated);
        assert_eq!(plan.bindings.len(), 2);
        assert_eq!(plan.bindings[0], AffinityBinding::Pinned { physical_core: 0 });
        assert_eq!(plan.bindings[1], AffinityBinding::Pinned { physical_core: 1 });
    }

    #[test]
    fn resolve_timeslice_when_cores_unavailable() {
        let mut mgr = VcpuManager::new("test", &[4, 5], SchedulingPolicy::Auto);
        // Available cores don't include 4 and 5
        let plan = mgr.resolve_scheduling(&[0, 1]);
        assert_eq!(
            plan.effective_policy,
            SchedulingPolicy::TimeSlice { quantum_ms: DEFAULT_QUANTUM_MS }
        );
        assert_eq!(plan.bindings.len(), 2);
        match &plan.bindings[0] {
            AffinityBinding::Shared { quantum_ms, .. } => {
                assert_eq!(*quantum_ms, DEFAULT_QUANTUM_MS);
            }
            _ => panic!("expected Shared binding"),
        }
    }

    #[test]
    fn explicit_dedicated_policy() {
        let mut mgr = VcpuManager::new("test", &[0, 1], SchedulingPolicy::Dedicated);
        let plan = mgr.resolve_scheduling(&[0, 1, 2, 3]);
        assert_eq!(plan.effective_policy, SchedulingPolicy::Dedicated);
    }

    #[test]
    fn explicit_timeslice_policy() {
        let mut mgr = VcpuManager::new("test", &[0, 1], SchedulingPolicy::TimeSlice { quantum_ms: 5 });
        let plan = mgr.resolve_scheduling(&[0, 1]);
        assert_eq!(
            plan.effective_policy,
            SchedulingPolicy::TimeSlice { quantum_ms: 5 }
        );
    }

    #[test]
    fn timeslice_scheduler_round_robin() {
        let mut sched = TimeSliceScheduler::new(0, 10);
        sched.add_vcpu("guest1", 0);
        sched.add_vcpu("guest2", 0);

        assert_eq!(sched.current_vcpu(), Some(&("guest1".into(), 0)));
        sched.switch_next();
        assert_eq!(sched.current_vcpu(), Some(&("guest2".into(), 0)));
        sched.switch_next();
        assert_eq!(sched.current_vcpu(), Some(&("guest1".into(), 0)));
    }

    #[test]
    fn timeslice_scheduler_add_remove() {
        let mut sched = TimeSliceScheduler::new(0, 10);
        sched.add_vcpu("g1", 0);
        sched.add_vcpu("g1", 1);
        sched.add_vcpu("g2", 0);
        assert_eq!(sched.vcpu_count(), 3);

        sched.remove_vcpu("g1", 1);
        assert_eq!(sched.vcpu_count(), 2);

        // Duplicate add is a no-op
        sched.add_vcpu("g1", 0);
        assert_eq!(sched.vcpu_count(), 2);
    }

    #[test]
    fn timeslice_scheduler_context_save_restore() {
        let mut sched = TimeSliceScheduler::new(0, 10);
        sched.add_vcpu("guest1", 0);

        let mut ctx = VcpuContext::default();
        ctx.rip = 0xDEADBEEF;
        ctx.gp_regs.rax = 42;
        sched.save_context("guest1", 0, ctx);

        let restored = sched.get_context("guest1", 0).unwrap();
        assert_eq!(restored.rip, 0xDEADBEEF);
        assert_eq!(restored.gp_regs.rax, 42);
    }

    #[test]
    fn timeslice_scheduler_empty_queue() {
        let mut sched = TimeSliceScheduler::new(0, 10);
        assert_eq!(sched.current_vcpu(), None);
        assert_eq!(sched.switch_next(), None);
    }

    #[test]
    fn vcpu_context_default() {
        let ctx = VcpuContext::default();
        assert_eq!(ctx.rip, 0);
        assert_eq!(ctx.rflags, 0);
        assert_eq!(ctx.gp_regs.rax, 0);
        assert!(ctx.xsave_area.is_empty());
    }

    #[test]
    fn plan_is_stored() {
        let mut mgr = VcpuManager::new("test", &[0], SchedulingPolicy::Dedicated);
        assert!(mgr.plan().is_none());
        mgr.resolve_scheduling(&[0, 1]);
        assert!(mgr.plan().is_some());
    }
}

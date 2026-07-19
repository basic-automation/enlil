//! Threading subsystem for the Enlil platform.
//!
//! Provides priority-based task scheduling with per-CPU run queues and
//! work-stealing. On hosted platforms (feature `linux`), delegates to
//! `std::thread`. On bare-metal, uses a cooperative per-CPU scheduler.
//!
//! # Architecture
//!
//! ```text
//!  ┌──────────┐
//!  │ Scheduler│──── manages ────┬─── RunQueue(cpu 0)
//!  └──────────┘                 ├─── RunQueue(cpu 1)
//!                               └─── RunQueue(cpu N)
//!
//!  Each RunQueue is a priority-sorted deque. Idle CPUs steal from
//!  the tail of the busiest neighbour (Chase-Lev concept).
//! ```

pub mod scheduler;

use crate::sync::Mutex;
use core::fmt;

#[cfg(feature = "platform-linux")]
use std::{boxed::Box, collections::VecDeque, string::String, sync::Arc, vec::Vec};

#[cfg(feature = "platform-baremetal")]
use alloc::{boxed::Box, collections::VecDeque, string::String, sync::Arc, vec::Vec};

// Re-exports
pub use scheduler::Scheduler;

// ---------------------------------------------------------------------------
// Task priority
// ---------------------------------------------------------------------------

/// Priority levels for scheduled tasks.
///
/// Lower numeric value == higher urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
#[derive(Default)]
pub enum Priority {
    /// vCPU execution — must pre-empt everything else.
    Critical = 0,
    /// Device I/O completions and interrupt bottom-halves.
    High = 1,
    /// General compute work (default).
    #[default]
    Normal = 2,
    /// Background / management housekeeping.
    Low = 3,
}

impl Priority {
    /// Total number of priority levels.
    pub const COUNT: usize = 4;

    /// Convert from a raw `u8`. Returns `None` for out-of-range values.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Critical),
            1 => Some(Self::High),
            2 => Some(Self::Normal),
            3 => Some(Self::Low),
            _ => None,
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Critical => write!(f, "critical"),
            Self::High => write!(f, "high"),
            Self::Normal => write!(f, "normal"),
            Self::Low => write!(f, "low"),
        }
    }
}

// ---------------------------------------------------------------------------
// CPU affinity
// ---------------------------------------------------------------------------

/// Specifies which CPU(s) a task may run on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CpuAffinity {
    /// The scheduler may place the task on any CPU.
    #[default]
    Any,
    /// Pin to a specific CPU index.
    Pinned(usize),
    /// Pin to one of the given CPU indices (scheduler picks).
    Set(Vec<usize>),
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

/// A unit of work that can be submitted to the scheduler.
///
/// Wraps a `FnOnce()` closure together with scheduling metadata.
pub struct Task {
    /// Human-readable label (for logging / debugging).
    pub name: String,
    /// Scheduling priority.
    pub priority: Priority,
    /// CPU affinity constraint.
    pub affinity: CpuAffinity,
    /// The actual work. `Option` so we can `.take()` it exactly once.
    work: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl Task {
    /// Create a new task with the given name, priority, affinity, and closure.
    pub fn new<F>(name: impl Into<String>, priority: Priority, affinity: CpuAffinity, f: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self {
            name: name.into(),
            priority,
            affinity,
            work: Some(Box::new(f)),
        }
    }

    /// Convenience: create a `Normal`-priority, `Any`-affinity task.
    pub fn spawn<F>(name: impl Into<String>, f: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::new(name, Priority::Normal, CpuAffinity::Any, f)
    }

    /// Execute the task, consuming the inner closure.
    ///
    /// Returns `true` if the closure was present and executed, `false` if the
    /// task was already consumed.
    pub fn run(&mut self) -> bool {
        if let Some(f) = self.work.take() {
            f();
            true
        } else {
            log::warn!("task '{}' has already been consumed", self.name);
            false
        }
    }

    /// Returns `true` if the closure has not yet been consumed.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.work.is_some()
    }
}

impl fmt::Debug for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("name", &self.name)
            .field("priority", &self.priority)
            .field("affinity", &self.affinity)
            .field("pending", &self.is_pending())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Per-CPU run queue
// ---------------------------------------------------------------------------

/// A per-CPU run queue.
///
/// Internally holds one `VecDeque<Task>` per priority level.  Push goes to the
/// back; pop takes from the front of the highest-priority non-empty queue.
///
/// Work-stealing takes from the **back** of the lowest-priority non-empty
/// queue (Chase-Lev style: owner pops front, thieves pop back).
pub struct RunQueue {
    /// The CPU index this queue belongs to.
    pub cpu: usize,
    /// One deque per priority level, indexed by `Priority as usize`.
    queues: [VecDeque<Task>; Priority::COUNT],
}

impl RunQueue {
    /// Create an empty run queue for the given CPU.
    #[must_use]
    pub const fn new(cpu: usize) -> Self {
        Self {
            cpu,
            queues: [
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
                VecDeque::new(),
            ],
        }
    }

    /// Push a task into the appropriate priority band.
    pub fn push(&mut self, task: Task) {
        let idx = task.priority as usize;
        log::trace!(
            "cpu{}: enqueue '{}' @ {}",
            self.cpu,
            task.name,
            task.priority
        );
        self.queues[idx].push_back(task);
    }

    /// Pop the highest-priority task from the front.
    pub fn pop(&mut self) -> Option<Task> {
        for q in &mut self.queues {
            if let Some(task) = q.pop_front() {
                return Some(task);
            }
        }
        None
    }

    /// Steal a task from the back of the lowest-priority non-empty queue.
    ///
    /// This is the "thief" side of the Chase-Lev concept: the owning CPU
    /// pops from the front while remote CPUs steal from the back, minimising
    /// contention.
    pub fn steal(&mut self) -> Option<Task> {
        for q in self.queues.iter_mut().rev() {
            if let Some(task) = q.pop_back() {
                return Some(task);
            }
        }
        None
    }

    /// Total number of pending tasks across all priority levels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queues.iter().map(VecDeque::len).sum()
    }

    /// Returns `true` if there are no pending tasks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for RunQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunQueue")
            .field("cpu", &self.cpu)
            .field("critical", &self.queues[0].len())
            .field("high", &self.queues[1].len())
            .field("normal", &self.queues[2].len())
            .field("low", &self.queues[3].len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Thread-local storage helpers (GS-segment concept with hosted fallback)
// ---------------------------------------------------------------------------

/// Per-CPU context stored in thread-local storage.
///
/// On bare-metal x86-64 this would live at the address pointed to by the GS
/// segment base (written via `wrmsrq(IA32_GS_BASE, …)`).  On hosted
/// platforms we fall back to `std::thread_local!`.
#[derive(Debug, Clone)]
pub struct CpuLocal {
    /// Logical CPU index.
    pub cpu_id: usize,
    /// Monotonic tick counter (placeholder).
    pub ticks: u64,
}

impl CpuLocal {
    #[must_use]
    pub const fn new(cpu_id: usize) -> Self {
        Self { cpu_id, ticks: 0 }
    }
}

// Hosted fallback: ordinary thread-local. On bare metal the per-CPU block lives
// at the GS-segment base (enlil-boot::percpu writes IA32_GS_BASE), so these
// std::thread_local!-backed accessors are gated to the hosted backend.
#[cfg(feature = "platform-linux")]
thread_local! {
    static CPU_LOCAL: std::cell::RefCell<CpuLocal> = const { std::cell::RefCell::new(CpuLocal::new(0)) };
}

/// Initialise thread-local CPU context for the calling thread.
#[cfg(feature = "platform-linux")]
pub fn init_cpu_local(cpu_id: usize) {
    CPU_LOCAL.with(|c| {
        let mut local = c.borrow_mut();
        local.cpu_id = cpu_id;
        local.ticks = 0;
    });
    log::debug!("cpu_local initialised for cpu {cpu_id}");
}

/// Read the current CPU id from thread-local storage.
#[cfg(feature = "platform-linux")]
#[must_use]
pub fn current_cpu_id() -> usize {
    CPU_LOCAL.with(|c| c.borrow().cpu_id)
}

/// Increment the tick counter and return the new value.
#[cfg(feature = "platform-linux")]
#[must_use]
pub fn tick() -> u64 {
    CPU_LOCAL.with(|c| {
        let mut local = c.borrow_mut();
        local.ticks += 1;
        local.ticks
    })
}

// ---------------------------------------------------------------------------
// Platform-gated backend: hosted (std::thread)
// ---------------------------------------------------------------------------

/// Spawn a task on a real OS thread (hosted mode).
///
/// This is the backend used when running under a hosted OS (Linux, Windows,
/// macOS).  Each task gets its own `std::thread`.
///
/// # Panics
///
/// Panics if the OS thread cannot be spawned.
#[cfg(feature = "platform-linux")]
#[must_use]
pub fn spawn_hosted(task: Task) -> std::thread::JoinHandle<()> {
    let name = task.name.clone();
    std::thread::Builder::new()
        .name(name.clone())
        .spawn(move || {
            let mut t = task;
            log::debug!("hosted: running task '{}'", t.name);
            t.run();
        })
        .unwrap_or_else(|e| panic!("failed to spawn thread for task '{name}': {e}"))
}

// ---------------------------------------------------------------------------
// Platform-gated backend: bare-metal stub
// ---------------------------------------------------------------------------

/// Shared run-queue set for the bare-metal scheduler stub.
///
/// In a real bare-metal environment this would be lock-free per-CPU storage
/// accessed via the GS segment.  Here we use `Arc<Mutex<…>>` so the concept
/// compiles and can be tested on any host.
#[derive(Clone)]
pub struct BareMetalScheduler {
    queues: Arc<Vec<Mutex<RunQueue>>>,
}

impl BareMetalScheduler {
    /// Create a scheduler with `num_cpus` run queues.
    #[must_use]
    pub fn new(num_cpus: usize) -> Self {
        let queues: Vec<Mutex<RunQueue>> = (0..num_cpus)
            .map(|i| Mutex::new(RunQueue::new(i)))
            .collect();
        Self {
            queues: Arc::new(queues),
        }
    }

    /// Submit a task, respecting its affinity.
    ///
    /// # Errors
    ///
    /// Returns an error if the pinned CPU index is out of range or
    /// no valid CPU exists in the affinity set.
    ///
    /// # Panics
    ///
    /// Panics if a queue mutex is poisoned.
    pub fn submit(&self, task: Task) -> Result<(), &'static str> {
        let target = match &task.affinity {
            CpuAffinity::Pinned(cpu) => {
                if *cpu >= self.queues.len() {
                    return Err("pinned CPU index out of range");
                }
                *cpu
            }
            CpuAffinity::Set(cpus) => {
                // Pick the least-loaded from the set.
                let mut best = None;
                for &cpu in cpus {
                    if cpu >= self.queues.len() {
                        continue;
                    }
                    let len = self.queues[cpu].lock().len();
                    match best {
                        None => best = Some((cpu, len)),
                        Some((_, best_len)) if len < best_len => best = Some((cpu, len)),
                        _ => {}
                    }
                }
                best.map(|(cpu, _)| cpu)
                    .ok_or("no valid CPU in affinity set")?
            }
            CpuAffinity::Any => {
                // Least-loaded.
                self.queues
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, q)| q.lock().len())
                    .map(|(i, _)| i)
                    .unwrap()
            }
        };

        log::debug!(
            "bare-metal: submit '{}' -> cpu {} ({})",
            task.name,
            target,
            task.priority
        );
        self.queues[target].lock().push(task);
        Ok(())
    }

    /// Run one task on the given CPU. Returns `true` if work was found.
    ///
    /// If the local queue is empty, attempts to steal from the busiest
    /// neighbour.
    ///
    /// # Panics
    ///
    /// Panics if a queue mutex is poisoned.
    #[must_use]
    pub fn run_one(&self, cpu: usize) -> bool {
        // Try local queue first.
        {
            let mut q = self.queues[cpu].lock();
            if let Some(mut task) = q.pop() {
                log::trace!("cpu{}: local run '{}'", cpu, task.name);
                task.run();
                return true;
            }
        }

        // Work-stealing: find the fullest neighbour.
        let mut victim = None;
        let mut victim_len = 0;
        for (i, q) in self.queues.iter().enumerate() {
            if i == cpu {
                continue;
            }
            let len = q.lock().len();
            if len > victim_len {
                victim = Some(i);
                victim_len = len;
            }
        }

        if let Some(v) = victim {
            let mut vq = self.queues[v].lock();
            if let Some(mut task) = vq.steal() {
                log::trace!("cpu{}: stole '{}' from cpu{}", cpu, task.name, v);
                task.run();
                return true;
            }
        }

        false
    }

    /// Number of CPUs.
    #[must_use]
    pub fn num_cpus(&self) -> usize {
        self.queues.len()
    }

    /// Total pending tasks across all CPUs.
    ///
    /// # Panics
    ///
    /// Panics if a queue mutex is poisoned.
    #[must_use]
    pub fn total_pending(&self) -> usize {
        self.queues.iter().map(|q| q.lock().len()).sum()
    }
}

impl fmt::Debug for BareMetalScheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BareMetalScheduler")
            .field("num_cpus", &self.queues.len())
            .field("total_pending", &self.total_pending())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn priority_ordering() {
        assert!(Priority::Critical < Priority::High);
        assert!(Priority::High < Priority::Normal);
        assert!(Priority::Normal < Priority::Low);
    }

    #[test]
    fn priority_from_u8_roundtrip() {
        for i in 0..4u8 {
            let p = Priority::from_u8(i).unwrap();
            assert_eq!(p as u8, i);
        }
        assert!(Priority::from_u8(4).is_none());
        assert!(Priority::from_u8(255).is_none());
    }

    #[test]
    fn priority_display() {
        assert_eq!(format!("{}", Priority::Critical), "critical");
        assert_eq!(format!("{}", Priority::Low), "low");
    }

    #[test]
    fn task_run_once() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let mut task = Task::spawn("test", move || {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert!(task.is_pending());
        assert!(task.run());
        assert!(!task.is_pending());
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // Second run should be a no-op.
        assert!(!task.run());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn task_debug_format() {
        let task = Task::spawn("dbg-test", || {});
        let dbg = format!("{task:?}");
        assert!(dbg.contains("dbg-test"));
        assert!(dbg.contains("Normal"));
    }

    #[test]
    fn runqueue_push_pop_priority_order() {
        let mut rq = RunQueue::new(0);

        rq.push(Task::new("low", Priority::Low, CpuAffinity::Any, || {}));
        rq.push(Task::new(
            "crit",
            Priority::Critical,
            CpuAffinity::Any,
            || {},
        ));
        rq.push(Task::new("norm", Priority::Normal, CpuAffinity::Any, || {}));

        assert_eq!(rq.len(), 3);

        // Should come out in priority order: Critical, Normal, Low.
        assert_eq!(rq.pop().unwrap().name, "crit");
        assert_eq!(rq.pop().unwrap().name, "norm");
        assert_eq!(rq.pop().unwrap().name, "low");
        assert!(rq.pop().is_none());
        assert!(rq.is_empty());
    }

    #[test]
    fn runqueue_fifo_within_priority() {
        let mut rq = RunQueue::new(0);
        rq.push(Task::spawn("a", || {}));
        rq.push(Task::spawn("b", || {}));
        rq.push(Task::spawn("c", || {}));

        assert_eq!(rq.pop().unwrap().name, "a");
        assert_eq!(rq.pop().unwrap().name, "b");
        assert_eq!(rq.pop().unwrap().name, "c");
    }

    #[test]
    fn runqueue_steal_takes_from_back() {
        let mut rq = RunQueue::new(0);
        rq.push(Task::spawn("first", || {}));
        rq.push(Task::spawn("second", || {}));
        rq.push(Task::spawn("third", || {}));

        // Steal takes from back of the lowest-priority non-empty queue.
        let stolen = rq.steal().unwrap();
        assert_eq!(stolen.name, "third");

        // Pop still takes from front.
        let popped = rq.pop().unwrap();
        assert_eq!(popped.name, "first");
    }

    #[test]
    fn runqueue_steal_prefers_low_priority() {
        let mut rq = RunQueue::new(0);
        rq.push(Task::new(
            "crit",
            Priority::Critical,
            CpuAffinity::Any,
            || {},
        ));
        rq.push(Task::new("low", Priority::Low, CpuAffinity::Any, || {}));

        // Steal should take from the lowest priority first (Low before Critical).
        let stolen = rq.steal().unwrap();
        assert_eq!(stolen.name, "low");
    }

    #[test]
    fn runqueue_debug() {
        let rq = RunQueue::new(7);
        let dbg = format!("{rq:?}");
        assert!(dbg.contains("cpu: 7"));
    }

    #[test]
    fn cpu_local_init_and_read() {
        init_cpu_local(42);
        assert_eq!(current_cpu_id(), 42);
    }

    #[test]
    fn cpu_local_tick() {
        init_cpu_local(0);
        let t1 = tick();
        let t2 = tick();
        assert_eq!(t2, t1 + 1);
    }

    #[test]
    fn spawn_hosted_runs_closure() {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let handle = spawn_hosted(Task::spawn("hosted-test", move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        handle.join().unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bare_metal_submit_and_run() {
        let sched = BareMetalScheduler::new(2);
        let counter = Arc::new(AtomicUsize::new(0));

        for i in 0..4 {
            let c = counter.clone();
            let task = Task::spawn(format!("task-{i}"), move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
            sched.submit(task).unwrap();
        }

        assert_eq!(sched.total_pending(), 4);

        // Drain all work.
        let mut ran = 0;
        for cpu in 0..2 {
            while sched.run_one(cpu) {
                ran += 1;
            }
        }

        assert_eq!(ran, 4);
        assert_eq!(counter.load(Ordering::SeqCst), 4);
        assert_eq!(sched.total_pending(), 0);
    }

    #[test]
    fn bare_metal_pinned_affinity() {
        let sched = BareMetalScheduler::new(4);
        let task = Task::new("pinned", Priority::High, CpuAffinity::Pinned(2), || {});
        sched.submit(task).unwrap();

        // Only cpu 2 should have work.
        for i in 0..4 {
            let len = sched.queues[i].lock().len();
            if i == 2 {
                assert_eq!(len, 1);
            } else {
                assert_eq!(len, 0);
            }
        }
    }

    #[test]
    fn bare_metal_pinned_out_of_range() {
        let sched = BareMetalScheduler::new(2);
        let task = Task::new("bad-pin", Priority::Normal, CpuAffinity::Pinned(99), || {});
        assert!(sched.submit(task).is_err());
    }

    #[test]
    fn bare_metal_set_affinity() {
        let sched = BareMetalScheduler::new(4);

        // Pre-load cpu 1 so cpu 3 should be preferred.
        for _ in 0..3 {
            sched
                .submit(Task::new(
                    "filler",
                    Priority::Low,
                    CpuAffinity::Pinned(1),
                    || {},
                ))
                .unwrap();
        }

        let task = Task::new(
            "set-aff",
            Priority::Normal,
            CpuAffinity::Set(vec![1, 3]),
            || {},
        );
        sched.submit(task).unwrap();

        // cpu 3 should have the task (less loaded than cpu 1).
        let len3 = sched.queues[3].lock().len();
        assert_eq!(len3, 1);
    }

    #[test]
    fn bare_metal_work_stealing() {
        let sched = BareMetalScheduler::new(2);
        let counter = Arc::new(AtomicUsize::new(0));

        // Put all work on cpu 0.
        for i in 0..4 {
            let c = counter.clone();
            sched
                .submit(Task::new(
                    format!("ws-{i}"),
                    Priority::Normal,
                    CpuAffinity::Pinned(0),
                    move || {
                        c.fetch_add(1, Ordering::SeqCst);
                    },
                ))
                .unwrap();
        }

        // cpu 1 should be able to steal work.
        assert!(sched.run_one(1));
        assert!(counter.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn bare_metal_debug() {
        let sched = BareMetalScheduler::new(2);
        let dbg = format!("{sched:?}");
        assert!(dbg.contains("BareMetalScheduler"));
        assert!(dbg.contains("num_cpus: 2"));
    }

    #[test]
    fn affinity_default_is_any() {
        assert_eq!(CpuAffinity::default(), CpuAffinity::Any);
    }

    #[test]
    fn priority_default_is_normal() {
        assert_eq!(Priority::default(), Priority::Normal);
    }
}

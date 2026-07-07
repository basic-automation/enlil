//! Priority Scheduler with Per-CPU Run Queues and Work Stealing
//!
//! Implements the scheduling model from the Enlil roadmap (Section 1.4):
//! - Per-CPU run queues with priority levels
//! - Work-stealing between idle and busy cores
//! - Priority levels: Critical (vCPU) > High (device I/O) > Normal (compute) > Low (management)

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Task priority levels, ordered from highest to lowest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Priority {
    /// vCPU tasks — VMLAUNCH/VMRESUME. Always run first.
    Critical = 0,
    /// Device I/O — `VirtIO` backends, USB polling, GPU commands.
    High = 1,
    /// Compute — Fabric JIT compilation, SPIR-V analysis.
    Normal = 2,
    /// Management — Console, logging, metrics.
    Low = 3,
}

impl Priority {
    pub const COUNT: usize = 4;

    #[must_use]
    pub const fn as_index(self) -> usize {
        self as usize
    }
}

/// A schedulable task.
pub struct SchedulerTask {
    /// Unique task ID.
    pub id: usize,
    /// Task priority.
    pub priority: Priority,
    /// The work to execute.
    pub work: Box<dyn FnOnce() + Send>,
}

impl SchedulerTask {
    pub fn new(id: usize, priority: Priority, work: impl FnOnce() + Send + 'static) -> Self {
        Self {
            id,
            priority,
            work: Box::new(work),
        }
    }
}

impl std::fmt::Debug for SchedulerTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerTask")
            .field("id", &self.id)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

/// Per-CPU run queue with priority lanes.
///
/// Each CPU core has one of these. Tasks are dequeued in priority order:
/// Critical first, then High, Normal, Low.
pub struct RunQueue {
    /// One deque per priority level.
    queues: [std::sync::Mutex<VecDeque<SchedulerTask>>; Priority::COUNT],
    /// Number of tasks across all priority levels.
    len: AtomicUsize,
    /// CPU core ID this queue belongs to.
    cpu_id: usize,
}

impl RunQueue {
    /// Create a new empty run queue for the given CPU.
    #[must_use]
    pub const fn new(cpu_id: usize) -> Self {
        Self {
            queues: [
                std::sync::Mutex::new(VecDeque::new()),
                std::sync::Mutex::new(VecDeque::new()),
                std::sync::Mutex::new(VecDeque::new()),
                std::sync::Mutex::new(VecDeque::new()),
            ],
            len: AtomicUsize::new(0),
            cpu_id,
        }
    }

    /// Push a task onto this run queue.
    ///
    /// # Panics
    ///
    /// Panics if a queue mutex is poisoned.
    pub fn push(&self, task: SchedulerTask) {
        let idx = task.priority.as_index();
        self.queues[idx].lock().unwrap().push_back(task);
        self.len.fetch_add(1, Ordering::Relaxed);
    }

    /// Pop the highest-priority task from this run queue.
    ///
    /// # Panics
    ///
    /// Panics if a queue mutex is poisoned.
    pub fn pop(&self) -> Option<SchedulerTask> {
        for queue in &self.queues {
            let task = {
                let mut q = queue.lock().unwrap();
                q.pop_front()
            };
            if let Some(task) = task {
                self.len.fetch_sub(1, Ordering::Relaxed);
                return Some(task);
            }
        }
        None
    }

    /// Steal a task from this run queue (called by idle cores).
    ///
    /// Steals from the back of the lowest-priority non-empty queue
    /// to minimize impact on the owning core's hot tasks.
    ///
    /// # Panics
    ///
    /// Panics if a queue mutex is poisoned.
    pub fn steal(&self) -> Option<SchedulerTask> {
        for queue in self.queues.iter().rev() {
            let task = {
                let mut q = queue.lock().unwrap();
                q.pop_back()
            };
            if let Some(task) = task {
                self.len.fetch_sub(1, Ordering::Relaxed);
                return Some(task);
            }
        }
        None
    }

    /// Returns the number of tasks in this run queue.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Returns true if this run queue is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the CPU ID this queue belongs to.
    pub const fn cpu_id(&self) -> usize {
        self.cpu_id
    }
}

/// The work-stealing scheduler.
///
/// Manages per-CPU run queues and coordinates work stealing between cores.
pub struct Scheduler {
    /// Per-CPU run queues.
    run_queues: Vec<Arc<RunQueue>>,
    /// Next task ID counter.
    next_task_id: AtomicUsize,
    /// Whether the scheduler is running.
    running: AtomicBool,
}

impl Scheduler {
    /// Create a new scheduler with the given number of CPUs.
    #[must_use]
    pub fn new(num_cpus: usize) -> Self {
        let run_queues = (0..num_cpus)
            .map(|cpu| Arc::new(RunQueue::new(cpu)))
            .collect();
        Self {
            run_queues,
            next_task_id: AtomicUsize::new(0),
            running: AtomicBool::new(false),
        }
    }

    /// Submit a task to a specific CPU's run queue.
    pub fn submit_to(
        &self,
        cpu: usize,
        priority: Priority,
        work: impl FnOnce() + Send + 'static,
    ) -> usize {
        let id = self.next_task_id.fetch_add(1, Ordering::Relaxed);
        let task = SchedulerTask::new(id, priority, work);
        self.run_queues[cpu % self.run_queues.len()].push(task);
        id
    }

    /// Submit a task to the least-loaded CPU's run queue.
    pub fn submit(&self, priority: Priority, work: impl FnOnce() + Send + 'static) -> usize {
        let cpu = self.least_loaded_cpu();
        self.submit_to(cpu, priority, work)
    }

    /// Find the CPU with the fewest queued tasks.
    fn least_loaded_cpu(&self) -> usize {
        self.run_queues
            .iter()
            .enumerate()
            .min_by_key(|(_, rq)| rq.len())
            .map_or(0, |(i, _)| i)
    }

    /// Attempt to steal a task for the given CPU from another CPU's queue.
    ///
    /// Tries to steal from the most-loaded queue first.
    pub fn try_steal(&self, for_cpu: usize) -> Option<SchedulerTask> {
        let mut candidates: Vec<(usize, usize)> = self
            .run_queues
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != for_cpu)
            .map(|(i, rq)| (i, rq.len()))
            .filter(|(_, len)| *len > 1) // Only steal if victim has >1 task
            .collect();

        // Sort by load descending — steal from busiest first.
        candidates.sort_by_key(|a| std::cmp::Reverse(a.1));

        for (cpu, _) in candidates {
            if let Some(task) = self.run_queues[cpu].steal() {
                return Some(task);
            }
        }
        None
    }

    /// Get the run queue for a specific CPU.
    pub fn run_queue(&self, cpu: usize) -> &Arc<RunQueue> {
        &self.run_queues[cpu]
    }

    /// Returns the number of CPUs this scheduler manages.
    pub const fn num_cpus(&self) -> usize {
        self.run_queues.len()
    }

    /// Returns total tasks across all run queues.
    pub fn total_tasks(&self) -> usize {
        self.run_queues.iter().map(|rq| rq.len()).sum()
    }

    /// Mark the scheduler as running.
    pub fn start(&self) {
        self.running.store(true, Ordering::SeqCst);
    }

    /// Mark the scheduler as stopped.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Returns true if the scheduler is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

/// Priority-inheritance tracker for a single lock (roadmap 1.6 — priority
/// inheritance on lock contention).
///
/// Priority inversion happens when a low-priority task holds a lock that a
/// high-priority task needs: the high task is blocked behind the low one, which
/// a medium task can then preempt indefinitely. The fix is *donation* — while a
/// higher-priority task waits, the holder runs at that higher priority so it
/// finishes and releases the lock quickly. This type tracks a holder's base
/// priority and the priorities of the tasks currently blocked on the lock, and
/// reports the [`effective`](Self::effective) priority the scheduler should run
/// the holder at.
///
/// [`Priority`] orders the *highest* priority as the smallest discriminant
/// (`Critical == 0`), so "the highest waiting priority" is the minimum — the
/// tracker donates via [`Ord::min`]. Waiters form a multiset: two tasks blocked
/// at the same priority both count, so releasing one keeps the boost while the
/// other still waits.
///
/// This is the backend-neutral bookkeeping; wiring it into the platform
/// [`Mutex`](crate::sync::Mutex) (arm a waiter on block, drop it on acquire, and
/// re-target the scheduler at the effective priority) is the next slice.
#[derive(Debug, Clone)]
pub struct PriorityInheritance {
    base: Priority,
    waiters: Vec<Priority>,
}

impl PriorityInheritance {
    /// Track a lock held by a task whose own priority is `base`.
    #[must_use]
    pub const fn new(base: Priority) -> Self {
        Self {
            base,
            waiters: Vec::new(),
        }
    }

    /// Record a task of priority `p` blocking on the lock.
    pub fn add_waiter(&mut self, p: Priority) {
        self.waiters.push(p);
    }

    /// Drop one waiter of priority `p` (e.g. it was granted the lock or timed
    /// out). Returns `true` if such a waiter was tracked. Multiset semantics: a
    /// second waiter at the same priority keeps the donation alive.
    pub fn remove_waiter(&mut self, p: Priority) -> bool {
        if let Some(i) = self.waiters.iter().position(|&w| w == p) {
            self.waiters.swap_remove(i);
            true
        } else {
            false
        }
    }

    /// Update the holder's own (base) priority.
    pub const fn set_base(&mut self, base: Priority) {
        self.base = base;
    }

    /// The holder's own priority, ignoring any donation.
    #[must_use]
    pub const fn base(&self) -> Priority {
        self.base
    }

    /// The priority the holder should actually run at: its base, boosted to the
    /// highest-priority waiter (the minimum [`Priority`] discriminant).
    #[must_use]
    pub fn effective(&self) -> Priority {
        self.waiters
            .iter()
            .copied()
            .min()
            .map_or(self.base, |highest| highest.min(self.base))
    }

    /// Whether a waiter is currently donating a higher priority than the base.
    #[must_use]
    pub fn is_boosted(&self) -> bool {
        self.effective() != self.base
    }

    /// Number of tasks currently blocked on the lock.
    #[must_use]
    pub const fn waiter_count(&self) -> usize {
        self.waiters.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn run_queue_priority_ordering() {
        let rq = RunQueue::new(0);

        // Push tasks in reverse priority order.
        rq.push(SchedulerTask::new(1, Priority::Low, || {}));
        rq.push(SchedulerTask::new(2, Priority::Normal, || {}));
        rq.push(SchedulerTask::new(3, Priority::Critical, || {}));
        rq.push(SchedulerTask::new(4, Priority::High, || {}));

        // Pop should return in priority order.
        assert_eq!(rq.pop().unwrap().priority, Priority::Critical);
        assert_eq!(rq.pop().unwrap().priority, Priority::High);
        assert_eq!(rq.pop().unwrap().priority, Priority::Normal);
        assert_eq!(rq.pop().unwrap().priority, Priority::Low);
        assert!(rq.pop().is_none());
    }

    #[test]
    fn run_queue_fifo_within_priority() {
        let rq = RunQueue::new(0);

        rq.push(SchedulerTask::new(1, Priority::Normal, || {}));
        rq.push(SchedulerTask::new(2, Priority::Normal, || {}));
        rq.push(SchedulerTask::new(3, Priority::Normal, || {}));

        assert_eq!(rq.pop().unwrap().id, 1);
        assert_eq!(rq.pop().unwrap().id, 2);
        assert_eq!(rq.pop().unwrap().id, 3);
    }

    #[test]
    fn work_stealing() {
        let sched = Scheduler::new(4);

        // Load up CPU 0 with tasks.
        for i in 0..10 {
            sched.submit_to(0, Priority::Normal, move || {
                let _ = i;
            });
        }

        assert_eq!(sched.run_queue(0).len(), 10);

        // CPU 1 steals a task from CPU 0.
        let stolen = sched.try_steal(1);
        assert!(stolen.is_some());
        assert_eq!(sched.run_queue(0).len(), 9);
    }

    #[test]
    fn least_loaded_balancing() {
        let sched = Scheduler::new(3);

        // Submit 3 tasks — should spread across CPUs.
        sched.submit(Priority::Normal, || {});
        sched.submit(Priority::Normal, || {});
        sched.submit(Priority::Normal, || {});

        // Each CPU should have roughly 1 task.
        let total: usize = (0..3).map(|i| sched.run_queue(i).len()).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn scheduler_lifecycle() {
        let sched = Scheduler::new(2);
        assert!(!sched.is_running());

        sched.start();
        assert!(sched.is_running());

        sched.stop();
        assert!(!sched.is_running());
    }

    #[test]
    fn task_execution() {
        let counter = Arc::new(AtomicU64::new(0));
        let sched = Scheduler::new(2);

        for _ in 0..5 {
            let c = counter.clone();
            sched.submit(Priority::High, move || {
                c.fetch_add(1, Ordering::SeqCst);
            });
        }

        // Drain and execute all tasks.
        let mut executed = 0;
        for cpu in 0..sched.num_cpus() {
            while let Some(task) = sched.run_queue(cpu).pop() {
                (task.work)();
                executed += 1;
            }
        }

        assert_eq!(executed, 5);
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn steal_respects_threshold() {
        let sched = Scheduler::new(2);

        // Only 1 task on CPU 0 — shouldn't be stolen (threshold is >1).
        sched.submit_to(0, Priority::Normal, || {});
        assert!(sched.try_steal(1).is_none());

        // Add another — now stealing should work.
        sched.submit_to(0, Priority::Normal, || {});
        assert!(sched.try_steal(1).is_some());
    }

    #[test]
    fn inheritance_boosts_holder_to_highest_waiter() {
        let mut pi = PriorityInheritance::new(Priority::Low);
        assert_eq!(pi.effective(), Priority::Low, "no waiters → base priority");
        assert!(!pi.is_boosted());

        // A Normal waiter boosts a Low holder to Normal.
        pi.add_waiter(Priority::Normal);
        assert_eq!(pi.effective(), Priority::Normal);
        assert!(pi.is_boosted());

        // A Critical waiter boosts further (Critical is the highest = smallest).
        pi.add_waiter(Priority::Critical);
        assert_eq!(pi.effective(), Priority::Critical);
        assert_eq!(pi.waiter_count(), 2);
    }

    #[test]
    fn inheritance_never_lowers_below_the_base() {
        // A holder already at High is not dragged down by a Low waiter.
        let mut pi = PriorityInheritance::new(Priority::High);
        pi.add_waiter(Priority::Low);
        assert_eq!(
            pi.effective(),
            Priority::High,
            "base wins over a lower waiter"
        );
        assert!(!pi.is_boosted());
    }

    #[test]
    fn inheritance_multiset_keeps_boost_until_last_equal_waiter_leaves() {
        let mut pi = PriorityInheritance::new(Priority::Low);
        pi.add_waiter(Priority::High);
        pi.add_waiter(Priority::High);
        assert_eq!(pi.effective(), Priority::High);

        // Removing one High waiter keeps the boost (the other still waits).
        assert!(pi.remove_waiter(Priority::High));
        assert_eq!(
            pi.effective(),
            Priority::High,
            "second waiter still donates"
        );
        // Removing the last drops back to base.
        assert!(pi.remove_waiter(Priority::High));
        assert_eq!(pi.effective(), Priority::Low);
        assert!(
            !pi.remove_waiter(Priority::High),
            "no waiter left to remove"
        );
    }

    #[test]
    fn inheritance_tracks_base_changes() {
        let mut pi = PriorityInheritance::new(Priority::Low);
        pi.add_waiter(Priority::Normal);
        assert_eq!(pi.effective(), Priority::Normal);
        // If the holder's own priority is raised above the waiter, no donation.
        pi.set_base(Priority::Critical);
        assert_eq!(pi.base(), Priority::Critical);
        assert_eq!(pi.effective(), Priority::Critical);
        assert!(!pi.is_boosted());
    }
}

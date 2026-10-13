//! Redox kernel-context-derived task contexts.
//!
//! Ports the context model from Redox OS's kernel (`kernel/src/context`):
//! every schedulable task is a [`Context`] with a kernel-managed
//! [`ContextId`], a lifecycle [`Status`], an owned stack, and its register
//! state. The mechanism underneath — the raw register swap — is the
//! platform's [`switch_task`](crate::threading::context::switch_task); this
//! module adds Redox's *object model* on top: ID allocation with the
//! reserved kernel ID, status transitions, and an arena ([`Contexts`]) that
//! owns the contexts it switches between.
//!
//! A context's register frame is initialized through
//! [`TaskFiber`](crate::threading::context::TaskFiber), so a freshly spawned
//! context enters its entry point on the first switch, and a returning entry
//! marks the context [`Status::Exited`] and resumes the scheduler context.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use crate::threading::context::{TaskContext, TaskEntry, TaskFiber};

/// Kernel-managed context identifier.
///
/// Like Redox's context IDs, `0` is reserved for the kernel/boot context;
/// [`Contexts::spawn`] hands out IDs starting at `1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContextId(u32);

impl ContextId {
    /// The reserved kernel context ID. Never handed out by
    /// [`Contexts::spawn`].
    pub const KERNEL: Self = Self(0);

    /// Whether this is the reserved kernel ID.
    #[must_use]
    pub const fn is_kernel(self) -> bool {
        self.0 == 0
    }

    /// The raw numeric ID.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Lifecycle state of a [`Context`], mirroring Redox's `context::Status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Ready to run (or currently running).
    Runnable,
    /// Parked waiting for an event — see
    /// [`WaitQueue`](crate::redox::sync::WaitQueue).
    Blocked,
    /// The entry function returned; the context will never run again.
    Exited,
}

/// One schedulable task: identity, lifecycle, stack, and register state.
///
/// The context owns its stack for its whole life, and the register frame's
/// self-pointer is written after boxing (see [`Context::spawn`]), so a
/// `Context` is safe to move inside its `Box` but must never be moved out of
/// it once spawned.
pub struct Context {
    id: ContextId,
    status: Status,
    name: String,
    stack: Vec<u8>,
    fiber: TaskFiber,
}

impl Context {
    /// Build a context whose first switch enters `entry(arg)`.
    ///
    /// The context is boxed *before* the fiber frame is initialized, so the
    /// frame's self-pointer observes the box's stable heap address. `sched`
    /// is the scheduler context control returns to when `entry` finishes;
    /// the caller must keep it alive until the context exits.
    ///
    /// # Panics
    ///
    /// Panics if `stack_size` is smaller than 64 bytes (the fiber frame does
    /// not fit).
    #[must_use]
    pub fn spawn(
        id: ContextId,
        name: &str,
        stack_size: usize,
        entry: TaskEntry,
        arg: *mut u8,
        sched: *const TaskContext,
    ) -> Box<Self> {
        let mut ctx = Box::new(Self {
            id,
            status: Status::Runnable,
            name: String::from(name),
            stack: alloc::vec![0u8; stack_size],
            fiber: TaskFiber::new(),
        });
        // Disjoint field borrows through the box: the stack stays put while
        // the fiber frame is written, and the frame's self-pointer names the
        // fiber's address inside this box.
        TaskFiber::init(&mut ctx.fiber, &mut ctx.stack, entry, arg, sched);
        ctx
    }

    /// This context's ID.
    #[must_use]
    pub const fn id(&self) -> ContextId {
        self.id
    }

    /// This context's lifecycle state.
    #[must_use]
    pub const fn status(&self) -> Status {
        self.status
    }

    /// Move the context to a new lifecycle state.
    pub const fn set_status(&mut self, status: Status) {
        self.status = status;
    }

    /// The human-readable name given at spawn.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the entry function has returned.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.fiber.is_done()
    }

    /// Pointer to the switchable register state, for
    /// [`Contexts::switch_to`].
    #[must_use]
    pub const fn task_ptr(&self) -> *const TaskContext {
        core::ptr::addr_of!(self.fiber.task)
    }
}

/// Arena of live contexts — the analog of Redox's context table.
///
/// Slot `i` holds the context with [`ContextId`] `i`; slot 0 stays empty
/// because the kernel ID is never handed out.
pub struct Contexts {
    slots: Vec<Option<Box<Context>>>,
    next_id: u32,
}

impl Contexts {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            next_id: 1,
        }
    }

    /// Spawn a context and return its ID (never [`ContextId::KERNEL`]).
    ///
    /// See [`Context::spawn`] for the `entry`/`arg`/`sched` contract.
    ///
    /// # Panics
    ///
    /// Panics if `stack_size` is smaller than 64 bytes, or on context-ID
    /// exhaustion (practically unreachable).
    pub fn spawn(
        &mut self,
        name: &str,
        stack_size: usize,
        entry: TaskEntry,
        arg: *mut u8,
        sched: *const TaskContext,
    ) -> ContextId {
        let raw = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("context ID space exhausted");
        let id = ContextId(raw);
        let slot = raw as usize;
        if slot >= self.slots.len() {
            self.slots.resize_with(slot + 1, || None);
        }
        self.slots[slot] = Some(Context::spawn(id, name, stack_size, entry, arg, sched));
        id
    }

    /// Look up a context by ID.
    #[must_use]
    pub fn get(&self, id: ContextId) -> Option<&Context> {
        self.slots.get(id.raw() as usize).and_then(Option::as_deref)
    }

    /// Look up a context by ID, mutably.
    fn get_mut(&mut self, id: ContextId) -> Option<&mut Context> {
        self.slots
            .get_mut(id.raw() as usize)
            .and_then(Option::as_deref_mut)
    }

    /// The lifecycle state of `id`, or `None` when unknown.
    #[must_use]
    pub fn status(&self, id: ContextId) -> Option<Status> {
        self.get(id).map(Context::status)
    }

    /// Switch from `from` to the context `to`.
    ///
    /// Returns `false` — without switching — when `to` is unknown or already
    /// [`Status::Exited`]. On return, a context whose entry finished is moved
    /// to [`Status::Exited`].
    ///
    /// # Safety
    ///
    /// `from` must point at a live [`TaskContext`] (typically the
    /// scheduler's) whose address stays valid across the switch, and caller
    /// and target must share the address space. Not reentrant: the target
    /// must not call `switch_to` on this same table while it runs.
    pub unsafe fn switch_to(&mut self, from: *mut TaskContext, to: ContextId) -> bool {
        let Some(ctx) = self.get_mut(to) else {
            return false;
        };
        if ctx.status == Status::Exited {
            return false;
        }
        let next = ctx.task_ptr();
        // SAFETY: upheld by the caller; `next` is a live fiber owned by this
        // table, which outlives the switch.
        unsafe { crate::threading::context::switch_task(from, next) };
        // The borrow of the context ends across the raw switch above (its
        // last use is `task_ptr`); re-borrow to refresh the status.
        if let Some(ctx) = self.get_mut(to)
            && ctx.is_done()
        {
            ctx.set_status(Status::Exited);
        }
        true
    }

    /// Remove `id` from the table and return it. Only meaningful for
    /// [`Status::Exited`] contexts; anything removed is simply dropped.
    pub fn reap(&mut self, id: ContextId) -> Option<Box<Context>> {
        self.slots.get_mut(id.raw() as usize)?.take()
    }

    /// Number of contexts currently in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Whether the table holds no contexts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Contexts {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::threading::context::switch_task;
    use core::sync::atomic::{AtomicUsize, Ordering};

    static COUNT: AtomicUsize = AtomicUsize::new(0);

    /// Control block for [`ping_entry`].
    struct PingCtl {
        me: *mut TaskContext,
        peer: *const TaskContext,
        rounds: usize,
    }

    /// Bumps the counter, bounces to the peer `rounds` times, then returns.
    ///
    /// # Safety
    ///
    /// `arg` must be a live `*mut PingCtl`, as the tests below pass.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe extern "C" fn ping_entry(arg: *mut u8) {
        // SAFETY: the tests pass `addr_of_mut!(PingCtl)` — 8-byte aligned.
        let ctl = unsafe { &mut *arg.cast::<PingCtl>() };
        for _ in 0..ctl.rounds {
            COUNT.fetch_add(1, Ordering::SeqCst);
            // SAFETY: `me`/`peer` are live fibers per the test setup.
            unsafe { switch_task(ctl.me, ctl.peer) };
        }
        COUNT.fetch_add(1000, Ordering::SeqCst);
    }

    /// A trivial entry that just records it ran.
    ///
    /// # Safety
    ///
    /// `arg` must be a live `*mut AtomicUsize`, as the tests below pass.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe extern "C" fn count_entry(arg: *mut u8) {
        // SAFETY: the tests pass an `AtomicUsize` pointer.
        let counter = unsafe { &*arg.cast::<AtomicUsize>() };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    fn spawn_two(
        table: &mut Contexts,
        sched: *const TaskContext,
        ctl_a: *mut u8,
        ctl_b: *mut u8,
    ) -> (ContextId, ContextId) {
        let a = table.spawn("ping-a", 65536, ping_entry, ctl_a, sched);
        let b = table.spawn("ping-b", 65536, ping_entry, ctl_b, sched);
        assert!(!a.is_kernel() && !b.is_kernel());
        assert_ne!(a, b);
        (a, b)
    }

    #[test]
    fn spawn_switch_exit_and_reap() {
        let mut sched = TaskContext::empty();
        let sched_ptr: *const TaskContext = core::ptr::addr_of!(sched);
        let mut table = Contexts::new();
        assert!(table.is_empty());

        let counter = AtomicUsize::new(0);
        let id = table.spawn(
            "counter",
            65536,
            count_entry,
            core::ptr::addr_of!(counter).cast_mut().cast::<u8>(),
            sched_ptr,
        );
        assert_eq!(table.len(), 1);
        assert_eq!(table.status(id), Some(Status::Runnable));
        assert_eq!(table.get(id).unwrap().name(), "counter");

        // SAFETY: `sched` is a live stack-local task context.
        let switched = unsafe { table.switch_to(core::ptr::addr_of_mut!(sched), id) };
        assert!(switched);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(table.status(id), Some(Status::Exited));
        assert!(table.get(id).unwrap().is_done());

        // An exited context refuses to run again.
        let again = unsafe { table.switch_to(core::ptr::addr_of_mut!(sched), id) };
        assert!(!again);

        let ctx = table.reap(id).unwrap();
        assert_eq!(ctx.id(), id);
        assert!(table.is_empty());
    }

    #[test]
    fn two_contexts_ping_pong() {
        let mut sched = TaskContext::empty();
        let sched_ptr: *const TaskContext = core::ptr::addr_of!(sched);
        let mut table = Contexts::new();
        let mut ctl_a = PingCtl {
            me: core::ptr::null_mut(),
            peer: core::ptr::null(),
            rounds: 5,
        };
        let mut ctl_b = PingCtl {
            me: core::ptr::null_mut(),
            peer: core::ptr::null(),
            rounds: 5,
        };
        let (a, b) = spawn_two(
            &mut table,
            sched_ptr,
            core::ptr::addr_of_mut!(ctl_a).cast::<u8>(),
            core::ptr::addr_of_mut!(ctl_b).cast::<u8>(),
        );
        // Wire the peers now that both fibers exist. The fibers observe these
        // through the raw pointers handed to `spawn`; the asserts below are
        // real reads that also validate the wiring.
        ctl_a.me = table.get(a).unwrap().task_ptr().cast_mut();
        ctl_a.peer = table.get(b).unwrap().task_ptr();
        ctl_b.me = table.get(b).unwrap().task_ptr().cast_mut();
        ctl_b.peer = table.get(a).unwrap().task_ptr();
        assert!(!ctl_a.me.is_null() && !ctl_a.peer.is_null());
        assert!(!ctl_b.me.is_null() && !ctl_b.peer.is_null());

        COUNT.store(0, Ordering::SeqCst);
        // SAFETY: `sched` is a live stack-local task context.
        assert!(unsafe { table.switch_to(core::ptr::addr_of_mut!(sched), a) });
        // A ran to completion, but B is still suspended mid-entry.
        assert!(table.get(a).unwrap().is_done());
        assert!(!table.get(b).unwrap().is_done());
        assert!(unsafe { table.switch_to(core::ptr::addr_of_mut!(sched), b) });
        // 5 rounds × 2 contexts = 10 bumps, plus 1000 per finished entry.
        assert_eq!(COUNT.load(Ordering::SeqCst), 10 + 2000);
        assert_eq!(table.status(a), Some(Status::Exited));
        assert_eq!(table.status(b), Some(Status::Exited));
    }

    #[test]
    fn status_transitions_and_unknown_ids() {
        let mut sched = TaskContext::empty();
        let sched_ptr: *const TaskContext = core::ptr::addr_of!(sched);
        let mut table = Contexts::new();
        let counter = AtomicUsize::new(0);
        let id = table.spawn(
            "blocker",
            65536,
            count_entry,
            core::ptr::addr_of!(counter).cast_mut().cast::<u8>(),
            sched_ptr,
        );

        table.get_mut(id).unwrap().set_status(Status::Blocked);
        assert_eq!(table.status(id), Some(Status::Blocked));

        // Unknown IDs: no context, no switch, nothing reaped.
        let unknown = ContextId(9999);
        assert_eq!(table.status(unknown), None);
        assert!(table.get(unknown).is_none());
        assert!(table.reap(unknown).is_none());
        // SAFETY: `sched` is live; the target does not exist.
        assert!(!unsafe { table.switch_to(core::ptr::addr_of_mut!(sched), unknown) });

        // A blocked (but not exited) context still switches.
        assert!(unsafe { table.switch_to(core::ptr::addr_of_mut!(sched), id) });
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn kernel_id_is_reserved() {
        assert!(ContextId::KERNEL.is_kernel());
        assert_eq!(ContextId::KERNEL.raw(), 0);
        let mut table = Contexts::new();
        let sched = TaskContext::empty();
        let id = table.spawn(
            "x",
            65536,
            count_entry,
            core::ptr::null_mut(),
            core::ptr::addr_of!(sched),
        );
        assert_ne!(id, ContextId::KERNEL);
        assert!(table.get(ContextId::KERNEL).is_none());
    }
}

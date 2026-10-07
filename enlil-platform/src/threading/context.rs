//! Real task context switching: registers, stacks, and XSAVE state.
//!
//! The scheduling model ([`scheduler`], [`RunQueue`](super::RunQueue),
//! [`BareMetalScheduler`](super::BareMetalScheduler)) decides *which* task
//! runs next; this module performs the actual switch — saving the outgoing
//! task's registers and resuming the incoming task's — plus the XSAVE
//! save/restore that keeps each task's FPU/SSE/AVX state with the task across
//! preemptions.
//!
//! A switch has two halves:
//!
//! - [`switch_context`] swaps the callee-saved registers and the stack
//!   pointer in a handful of instructions (a `#[unsafe(naked)]` routine).
//! - [`switch_task`] additionally saves/restores the tasks'
//!   [`XSaveArea`](enlil_hal::xsave::XSaveArea)s, so extended processor state
//!   follows the task instead of leaking into whatever runs next.
//!
//! [`TaskFiber`] bundles a task's stack with its initial register frame: the
//! first switch into a fiber enters its entry point, and when the entry
//! returns the fiber is marked done and control returns to the scheduler
//! context it was launched from. [`preempt`](super::preempt) is what decides
//! *when* a running task is switched away from.

use core::arch::naked_asm;
use enlil_hal::xsave::XSaveArea;

/// The register state [`switch_context`] swaps: the stack pointer plus the
/// resume address.
///
/// Only `rsp` takes part in the switch itself — the callee-saved registers
/// travel on the stacks, pushed and popped by the switch routine. `rip`
/// records where a freshly built fiber resumes, for debugging.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// Stack pointer. A saved context's `rsp` is whatever it was inside
    /// [`switch_context`]; a fresh fiber's is built by [`TaskFiber::init`].
    pub rsp: u64,
    /// Resume address. Informational: the switch pops it from the new stack.
    pub rip: u64,
}

impl Context {
    /// A zeroed context. Not switchable until installed by [`switch_context`]
    /// (as `prev`) or built by [`TaskFiber::init`].
    #[must_use]
    pub const fn zero() -> Self {
        Self { rsp: 0, rip: 0 }
    }
}

/// Switch register state from `prev` to `next`.
///
/// Pushes the callee-saved registers (`rbx`, `rbp`, `r12`–`r15`) and records
/// the stack pointer in `prev->rsp`; then loads `next->rsp`, pops the saved
/// registers, and `ret`s into the new task. When the new task later switches
/// back, execution resumes right after this call with the old task's
/// registers intact.
///
/// Intel syntax (the `asm!` default on x86-64).
///
/// # Safety
///
/// - Both pointers must be valid for reads and writes.
/// - `next->rsp` must point at a stack built by [`TaskFiber::init`] or saved
///   by an earlier `switch_context` call, with at least 56 readable bytes
///   below it holding the register frame the pops consume.
/// - Caller and target must share the address space.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(prev: *mut Context, next: *const Context) {
    naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, [rsi]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
    );
}

/// A task's full switchable state: registers plus extended processor state.
#[repr(C)]
pub struct TaskContext {
    /// Register file and stack. Must be the first field: the trampoline and
    /// [`TaskFiber`] treat `*mut TaskContext` as `*mut Context`.
    pub regs: Context,
    /// The task's FPU/SSE/AVX/… state, swapped by [`switch_task`]. `None`
    /// for tasks that never touch extended state.
    pub xsave: Option<XSaveArea>,
}

impl TaskContext {
    /// A task context with no extended state. Not switchable until its
    /// `regs` are installed (see [`Context::zero`]).
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            regs: Context::zero(),
            xsave: None,
        }
    }
}

/// Switch from `prev` to `next`, preserving extended processor state.
///
/// Saves `prev`'s XSAVE area (when present) and restores `next`'s (when
/// present) — eager save/restore, always correct — then swaps registers via
/// [`switch_context`].
///
/// # Safety
///
/// The [`switch_context`] contract, plus: the `xsave` CPU feature must be
/// enabled whenever either side carries an XSAVE area.
pub unsafe fn switch_task(prev: *mut TaskContext, next: *const TaskContext) {
    // SAFETY: upheld by the caller (see above).
    unsafe {
        if let Some(area) = (*prev).xsave.as_ref() {
            area.save();
        }
        if let Some(area) = (*next).xsave.as_ref() {
            area.restore();
        }
        switch_context(
            core::ptr::addr_of_mut!((*prev).regs),
            core::ptr::addr_of!((*next).regs),
        );
    }
}

/// Entry point for a fiber: `unsafe extern "C" fn(arg: *mut u8)`.
pub type TaskEntry = unsafe extern "C" fn(*mut u8);

/// A task with its own stack, ready to be switched to.
///
/// The first [`switch_task`]/[`switch_context`] into the fiber enters
/// `entry`; when `entry` returns, the fiber is marked done and control
/// returns to the scheduler context given to [`init`](Self::init).
#[repr(C)]
pub struct TaskFiber {
    /// Switchable state. First field: the trampoline treats `*mut TaskFiber`
    /// as `*mut TaskContext`.
    pub task: TaskContext,
    /// Set once `entry` has returned. A done fiber must not be resumed.
    pub done: bool,
}

impl TaskFiber {
    /// A fiber with no stack or extended state — call [`init`](Self::init)
    /// before switching to it.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            task: TaskContext::empty(),
            done: false,
        }
    }

    /// Build the initial stack frame so the first switch enters `entry(arg)`.
    ///
    /// Consumes the top 56 bytes of `stack` for the register frame (kept
    /// 16-byte aligned); the remainder is the task's stack. `sched` is the
    /// context control returns to when `entry` finishes.
    ///
    /// # Panics
    ///
    /// Panics if `stack` is shorter than 64 bytes.
    pub fn init(
        &mut self,
        stack: &mut [u8],
        entry: TaskEntry,
        arg: *mut u8,
        sched: *const TaskContext,
    ) {
        assert!(stack.len() >= 64, "fiber stack must be at least 64 bytes");
        // Top of stack, rounded down to 16; the 56-byte frame leaves
        // `rsp % 16 == 8`, exactly what `switch_context` saves — so the
        // trampoline's `call entry` is ABI-aligned.
        let top = stack.as_mut_ptr() as usize + stack.len();
        let sp = (top & !0xF) - 56;
        // SAFETY: `sp..sp+56` lies inside `stack` (it is 64+ bytes); the frame
        // is written once, before any switch can read it.
        unsafe {
            let frame = sp as *mut u64;
            // Popped by `switch_context` in order: r15, r14, r13, r12, rbp,
            // rbx, then `ret` into `rip`.
            frame.add(0).write(core::ptr::from_mut(self) as u64); // r15: fiber (exit's prev)
            frame.add(1).write(sched as u64); // r14: scheduler ctx (exit's next)
            frame.add(2).write(arg as u64); // r13: entry argument
            frame.add(3).write(entry as usize as u64); // r12: entry point
            frame.add(4).write(0); // rbp
            frame.add(5).write(0); // rbx
            frame.add(6).write(task_trampoline as *const u8 as u64); // rip
        }
        self.task.regs = Context {
            rsp: sp as u64,
            rip: task_trampoline as *const u8 as u64,
        };
        self.done = false;
    }

    /// Whether the fiber's entry function has returned.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }
}

impl Default for TaskFiber {
    fn default() -> Self {
        Self::new()
    }
}

/// First code a fiber runs: calls the entry point, then exits to the scheduler.
///
/// Entered via `ret` from [`switch_context`] with the initial register frame
/// popped: `r12` = entry, `r13` = arg, `r14` = scheduler context,
/// `r15` = fiber. (`r12`–`r15` are callee-saved, so `entry` preserves them
/// for the exit path.)
#[unsafe(naked)]
unsafe extern "C" fn task_trampoline() {
    naked_asm!(
        "mov rdi, r13",
        "call r12",
        "mov rdi, r15",
        "mov rsi, r14",
        "jmp {exit}",
        exit = sym fiber_exit,
    );
}

/// Runs when a fiber's entry returns: marks it done and switches back to the
/// scheduler context that launched it.
///
/// # Safety
///
/// `fiber` must point to the live [`TaskFiber`] whose entry just returned;
/// `sched` to the scheduler context to resume. Never returns.
unsafe extern "C" fn fiber_exit(fiber: *mut TaskFiber, sched: *const TaskContext) -> ! {
    // SAFETY: upheld by the trampoline (see above).
    unsafe {
        (*fiber).done = true;
        switch_task(core::ptr::addr_of_mut!((*fiber).task), sched);
    }
    // A finished fiber is never resumed; park fail-safe rather than falling
    // through into reclaimed stack.
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    // -- ping-pong: two fibers interleaved on separate stacks ----------------

    /// Control block handed to [`ping_entry`] as `arg`.
    #[repr(C)]
    struct PingCtl {
        me: *mut TaskContext,
        peer: *const TaskContext,
        rounds: usize,
    }

    static PING_COUNT: AtomicUsize = AtomicUsize::new(0);

    /// Bumps the counter, switches to the peer, `rounds` times, then returns
    /// (→ the fiber is marked done and control returns to the scheduler).
    ///
    /// # Safety
    ///
    /// `arg` must be a live `*mut PingCtl`, as the tests below pass.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe extern "C" fn ping_entry(arg: *mut u8) {
        // SAFETY: the tests pass `addr_of_mut!(PingCtl)` — 8-byte aligned.
        let ctl = unsafe { &mut *arg.cast::<PingCtl>() };
        for _ in 0..ctl.rounds {
            PING_COUNT.fetch_add(1, Ordering::SeqCst);
            unsafe { switch_task(ctl.me, ctl.peer) };
        }
        PING_COUNT.fetch_add(1000, Ordering::SeqCst);
    }

    #[test]
    fn fibers_ping_pong_and_exit() {
        let mut sched = TaskContext::empty();
        let mut fa = TaskFiber::new();
        let mut fb = TaskFiber::new();
        let mut stack_a = vec![0u8; 65536];
        let mut stack_b = vec![0u8; 65536];

        let me_a: *mut TaskContext = core::ptr::addr_of_mut!(fa.task);
        let me_b: *mut TaskContext = core::ptr::addr_of_mut!(fb.task);
        let mut ctl_a = PingCtl {
            me: me_a,
            peer: core::ptr::addr_of!(fb.task),
            rounds: 3,
        };
        let mut ctl_b = PingCtl {
            me: me_b,
            peer: core::ptr::addr_of!(fa.task),
            rounds: 3,
        };
        let sched_ptr: *const TaskContext = core::ptr::addr_of!(sched);
        fa.init(
            &mut stack_a,
            ping_entry,
            core::ptr::addr_of_mut!(ctl_a).cast::<u8>(),
            sched_ptr,
        );
        fb.init(
            &mut stack_b,
            ping_entry,
            core::ptr::addr_of_mut!(ctl_b).cast::<u8>(),
            sched_ptr,
        );

        PING_COUNT.store(0, Ordering::SeqCst);
        unsafe {
            switch_task(core::ptr::addr_of_mut!(sched), core::ptr::addr_of!(fa.task));
            // A ran to completion, but B is still suspended mid-entry.
            assert!(fa.is_done());
            assert!(!fb.is_done());
            switch_task(core::ptr::addr_of_mut!(sched), core::ptr::addr_of!(fb.task));
        }
        assert!(fb.is_done());
        // 3 rounds × 2 fibers = 6 bumps, plus 1000 per finished entry.
        assert_eq!(PING_COUNT.load(Ordering::SeqCst), 2006);
    }

    // -- stack isolation -----------------------------------------------------

    static STACK_ADDRS: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

    #[repr(C)]
    struct AddrCtl {
        me: *mut TaskContext,
        sched: *const TaskContext,
        id: usize,
    }

    /// Records the address of a stack local, then yields to the scheduler.
    ///
    /// # Safety
    ///
    /// `arg` must be a live `*const AddrCtl`, as the test below passes.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe extern "C" fn addr_entry(arg: *mut u8) {
        // SAFETY: the test passes `addr_of!(AddrCtl)` — 8-byte aligned.
        let ctl = unsafe { &*arg.cast_const().cast::<AddrCtl>() };
        let local = 0u64;
        STACK_ADDRS[ctl.id].store(core::ptr::addr_of!(local) as usize, Ordering::SeqCst);
        unsafe { switch_task(ctl.me, ctl.sched) };
    }

    #[test]
    fn fiber_stacks_are_isolated() {
        let mut sched = TaskContext::empty();
        let mut fa = TaskFiber::new();
        let mut fb = TaskFiber::new();
        let mut stack_a = vec![0u8; 65536];
        let mut stack_b = vec![0u8; 65536];

        let me_a: *mut TaskContext = core::ptr::addr_of_mut!(fa.task);
        let me_b: *mut TaskContext = core::ptr::addr_of_mut!(fb.task);
        let sched_ptr: *const TaskContext = core::ptr::addr_of!(sched);
        let ctl_a = AddrCtl {
            me: me_a,
            sched: sched_ptr,
            id: 0,
        };
        let ctl_b = AddrCtl {
            me: me_b,
            sched: sched_ptr,
            id: 1,
        };
        fa.init(
            &mut stack_a,
            addr_entry,
            core::ptr::addr_of!(ctl_a).cast_mut().cast::<u8>(),
            sched_ptr,
        );
        fb.init(
            &mut stack_b,
            addr_entry,
            core::ptr::addr_of!(ctl_b).cast_mut().cast::<u8>(),
            sched_ptr,
        );

        unsafe {
            switch_task(core::ptr::addr_of_mut!(sched), core::ptr::addr_of!(fa.task));
            switch_task(core::ptr::addr_of_mut!(sched), core::ptr::addr_of!(fb.task));
        }

        let a = STACK_ADDRS[0].load(Ordering::SeqCst);
        let b = STACK_ADDRS[1].load(Ordering::SeqCst);
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        // Different stacks: the addresses are far apart…
        assert!(a.abs_diff(b) > 4096);
        // …and each lies inside its own stack allocation.
        assert!(stack_a.as_ptr_range().contains(&(a as *const u8)));
        assert!(stack_b.as_ptr_range().contains(&(b as *const u8)));
    }

    // -- XSAVE follows the task ----------------------------------------------

    static FPU_SEEN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

    #[repr(C)]
    struct FpuCtl {
        me: *mut TaskContext,
        sched: *const TaskContext,
        sentinel: u64,
        id: usize,
    }

    /// Writes `sentinel` to `XMM0`, yields, and on resume records what
    /// survived in `XMM0` — proving extended state follows the task.
    ///
    /// # Safety
    ///
    /// `arg` must be a live `*const FpuCtl`, as the test below passes.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe extern "C" fn fpu_entry(arg: *mut u8) {
        // SAFETY: the test passes `addr_of!(FpuCtl)` — 8-byte aligned.
        let ctl = unsafe { &*arg.cast_const().cast::<FpuCtl>() };
        unsafe {
            // `movq` to an XMM register is SSE2 (baseline x86-64).
            core::arch::asm!(
                "movq xmm0, {s}",
                s = in(reg) ctl.sentinel,
                options(nostack),
            );
            switch_task(ctl.me, ctl.sched);
            let mut out: u64 = 0;
            core::arch::asm!(
                "movq {o}, xmm0",
                o = out(reg) out,
                options(nostack),
            );
            FPU_SEEN[ctl.id].store(out, Ordering::SeqCst);
        }
    }

    #[test]
    fn switch_task_preserves_fpu_per_task() {
        use enlil_hal::xsave::{XSaveArea, xsave_supported};
        if !xsave_supported() {
            return; // No XSAVE on this CPU — nothing to exercise.
        }
        let mut sched = TaskContext::empty();
        let mut fa = TaskFiber::new();
        let mut fb = TaskFiber::new();
        fa.task.xsave = XSaveArea::for_current_xcr0();
        fb.task.xsave = XSaveArea::for_current_xcr0();
        assert!(fa.task.xsave.is_some() && fb.task.xsave.is_some());
        let mut stack_a = vec![0u8; 65536];
        let mut stack_b = vec![0u8; 65536];

        let me_a: *mut TaskContext = core::ptr::addr_of_mut!(fa.task);
        let me_b: *mut TaskContext = core::ptr::addr_of_mut!(fb.task);
        let sched_ptr: *const TaskContext = core::ptr::addr_of!(sched);
        let ctl_a = FpuCtl {
            me: me_a,
            sched: sched_ptr,
            sentinel: 0xA0A0_A0A0_A0A0_A0A0,
            id: 0,
        };
        let ctl_b = FpuCtl {
            me: me_b,
            sched: sched_ptr,
            sentinel: 0xB0B0_B0B0_B0B0_B0B0,
            id: 1,
        };
        fa.init(
            &mut stack_a,
            fpu_entry,
            core::ptr::addr_of!(ctl_a).cast_mut().cast::<u8>(),
            sched_ptr,
        );
        fb.init(
            &mut stack_b,
            fpu_entry,
            core::ptr::addr_of!(ctl_b).cast_mut().cast::<u8>(),
            sched_ptr,
        );

        unsafe {
            switch_task(core::ptr::addr_of_mut!(sched), core::ptr::addr_of!(fa.task)); // A: set sentinel, yield
            switch_task(core::ptr::addr_of_mut!(sched), core::ptr::addr_of!(fb.task)); // B: set sentinel, yield
            switch_task(core::ptr::addr_of_mut!(sched), core::ptr::addr_of!(fa.task)); // A: read back, done
            switch_task(core::ptr::addr_of_mut!(sched), core::ptr::addr_of!(fb.task)); // B: read back, done
        }
        assert_eq!(FPU_SEEN[0].load(Ordering::SeqCst), 0xA0A0_A0A0_A0A0_A0A0);
        assert_eq!(FPU_SEEN[1].load(Ordering::SeqCst), 0xB0B0_B0B0_B0B0_B0B0);
        assert!(fa.is_done() && fb.is_done());
    }

    // -- misc ------------------------------------------------------------------

    #[test]
    fn context_zero_is_not_switchable_but_debuggable() {
        let ctx = Context::zero();
        assert_eq!((ctx.rsp, ctx.rip), (0, 0));
        let dbg = format!("{ctx:?}");
        assert!(dbg.contains("Context"));
    }

    #[test]
    fn task_fiber_new_is_not_done() {
        let fiber = TaskFiber::new();
        assert!(!fiber.is_done());
        assert!(fiber.task.xsave.is_none());
        let _ = TaskFiber::default();
    }

    #[test]
    #[should_panic(expected = "fiber stack must be at least 64 bytes")]
    fn init_panics_on_tiny_stack() {
        let mut fiber = TaskFiber::new();
        let mut stack = vec![0u8; 63];
        fiber.init(
            &mut stack,
            ping_entry,
            core::ptr::null_mut(),
            core::ptr::null(),
        );
    }
}

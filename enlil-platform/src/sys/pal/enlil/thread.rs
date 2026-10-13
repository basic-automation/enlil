//! Threading bindings for the bare-metal Enlil kernel.
//!
//! There are no OS threads here: [`spawn`] submits a task to the global
//! [`BareMetalScheduler`](crate::threading::BareMetalScheduler), and
//! [`JoinHandle::join`] pumps that scheduler until the task completes — the
//! same cooperative model `enlil-std` used before `sys` existed. The
//! scheduler's home is now this module, so every `sys::thread` consumer
//! shares one task queue.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use crate::io::{IoError, IoErrorKind, IoResult};
use crate::sync::Mutex;
use crate::threading::{BareMetalScheduler, Task};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Lazily-initialized global scheduler backing [`spawn`].
///
/// `no_std` has no `std::sync::OnceLock`; `spin::Once` (already a platform
/// dependency) fills the role.
static SCHEDULER: spin::Once<BareMetalScheduler> = spin::Once::new();

/// The shared scheduler, initialized on first use.
fn scheduler() -> &'static BareMetalScheduler {
    SCHEDULER.call_once(|| BareMetalScheduler::new(1))
}

/// Opaque thread identifier, unique per spawned task (ids start at 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(u64);

impl ThreadId {
    /// The raw numeric id.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A handle to a thread: its id and name.
#[derive(Debug, Clone)]
pub struct Thread {
    id: ThreadId,
    name: Option<String>,
}

impl Thread {
    /// The thread's id.
    #[must_use]
    pub const fn id(&self) -> ThreadId {
        self.id
    }

    /// The thread's name, if one was given at spawn.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Handle for a spawned task; [`join`](JoinHandle::join) pumps the
/// scheduler until the task completes.
pub struct JoinHandle<T> {
    thread: Thread,
    result: Arc<Mutex<Option<T>>>,
}

impl<T> JoinHandle<T> {
    /// The spawned thread's handle.
    #[must_use]
    pub const fn thread(&self) -> &Thread {
        &self.thread
    }

    /// Pump the scheduler until the task finishes and return its result.
    ///
    /// With the bare-metal `panic = "abort"` target strategy a panicking
    /// task aborts the kernel instead of unwinding, so the error case —
    /// the task vanished without producing a result — is unreachable in
    /// practice; it exists for API parity with the Linux PAL.
    ///
    /// # Errors
    ///
    /// Returns an error if the task vanished without producing a result.
    pub fn join(self) -> IoResult<T> {
        let sched = scheduler();
        loop {
            if let Some(val) = self.result.lock().take() {
                return Ok(val);
            }
            if !sched.run_one(0) {
                // No runnable task and no result yet — spin briefly.
                // (Once preemption lands, the timer tick wakes us.)
                core::hint::spin_loop();
            }
        }
    }
}

/// Spawn a task running `f`, returning a [`JoinHandle`].
///
/// The task is submitted to the global scheduler; it runs when
/// [`JoinHandle::join`] (or any other scheduler pump) executes it.
///
/// # Errors
///
/// Returns an error if the scheduler rejects the task (e.g. its run queue
/// is full).
pub fn spawn<F, T>(name: Option<&str>, f: F) -> IoResult<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let id = ThreadId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let owned_name = name.map_or_else(|| String::from("enlil-sys-thread"), String::from);
    let result = Arc::new(Mutex::new(None));
    let result_clone = Arc::clone(&result);

    let task = Task::spawn(owned_name.clone(), move || {
        let val = f();
        *result_clone.lock() = Some(val);
    });
    scheduler()
        .submit(task)
        .map_err(|e| IoError::with_message(IoErrorKind::Other, e))?;

    Ok(JoinHandle {
        thread: Thread {
            id,
            name: Some(owned_name),
        },
        result,
    })
}

/// Returns a handle to the calling execution context.
///
/// There is no OS thread identity on bare metal; id 0 is the calling
/// context (boot/main) and is never handed out by [`spawn`] (ids start
/// at 1). The name is always `None`: contexts are unnamed.
#[must_use]
pub const fn current() -> Thread {
    Thread {
        id: ThreadId(0),
        name: None,
    }
}

/// Cooperatively yields the remainder of the current scheduling quantum.
pub fn yield_now() {
    crate::threading::yield_now();
}

/// Puts the current context to sleep for at least `dur` (TSC spin).
pub fn sleep(dur: Duration) {
    crate::time::sleep(dur);
}

/// Number of CPUs the scheduler was created with.
///
/// # Errors
///
/// This backend never fails; the `IoResult` is for API parity with Linux.
pub fn available_parallelism() -> IoResult<usize> {
    Ok(scheduler().num_cpus())
}

//! Synchronization Primitives
//!
//! Platform-abstracted Mutex, `RwLock`, Condvar, and Channel.
//!
//! - **Linux:** Delegates to `std::sync` (pthread-backed).
//! - **Bare-metal:** Uses `spin` crate for spinlock-based implementations.
//!
//! Plus the backend-agnostic raw primitives, usable on every target with no
//! feature gates:
//!
//! - [`SpinLock`]: a test-and-set spinlock for short, bounded critical
//!   sections.
//! - [`SleepLock`]: a blocking mutex — contended waiters sleep (futex on
//!   Linux, scheduler yield / `hlt` on bare metal) instead of spinning.

pub mod sleep;
pub mod spinlock;

pub use sleep::{SleepGuard, SleepLock};
pub use spinlock::{SpinGuard, SpinLock};

use core::ops::{Deref, DerefMut};

use crate::threading::scheduler::{self, PriorityInheritance};

// ===========================================================================
// Mutex
// ===========================================================================

/// Raw lock guarding a [`Mutex`]'s priority-inheritance state.
///
/// Deliberately a raw `std`/`spin` mutex rather than the platform [`Mutex`]
/// itself, so updating the bookkeeping never recurses into the donation
/// logic. It is a leaf lock: never held while acquiring any other lock.
#[cfg(feature = "platform-linux")]
type PiLock = std::sync::Mutex<PriorityInheritance>;
#[cfg(feature = "platform-baremetal")]
type PiLock = spin::Mutex<PriorityInheritance>;

/// A platform-abstracted mutual exclusion lock.
///
/// The mutex carries priority-inheritance bookkeeping
/// ([`scheduler::PriorityInheritance`]): while a thread with a registered
/// scheduling priority (see [`crate::threading::current_task_priority`]) is
/// blocked in [`Mutex::lock`], its priority is donated to the lock holder, and
/// [`Mutex::effective_priority`] reports the priority the scheduler should run
/// the holder at until the lock is released.
pub struct Mutex<T> {
    #[cfg(feature = "platform-linux")]
    inner: std::sync::Mutex<T>,
    #[cfg(feature = "platform-baremetal")]
    inner: spin::Mutex<T>,
    /// Holder base priority + blocked-waiter multiset for priority donation.
    pi: PiLock,
}

// Safety: inner types are already Send + Sync where T: Send.
unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// Creates a new mutex wrapping the given value.
    pub const fn new(value: T) -> Self {
        Self {
            #[cfg(feature = "platform-linux")]
            inner: std::sync::Mutex::new(value),
            #[cfg(feature = "platform-baremetal")]
            inner: spin::Mutex::new(value),
            pi: PiLock::new(PriorityInheritance::new(scheduler::Priority::Normal)),
        }
    }

    /// Locks the priority-inheritance state.
    ///
    /// # Panics
    ///
    /// Panics if the underlying PI mutex is poisoned (Linux).
    #[cfg(feature = "platform-linux")]
    fn pi_state(&self) -> std::sync::MutexGuard<'_, PriorityInheritance> {
        self.pi.lock().unwrap()
    }

    /// Locks the priority-inheritance state.
    #[cfg(feature = "platform-baremetal")]
    fn pi_state(&self) -> spin::MutexGuard<'_, PriorityInheritance> {
        self.pi.lock()
    }

    /// The calling thread's priority in scheduler terms, if registered.
    fn waiter_priority() -> Option<scheduler::Priority> {
        crate::threading::current_task_priority().map(scheduler::Priority::from)
    }

    /// Records a successful acquire in the inheritance bookkeeping: drops our
    /// waiter registration if we armed one while blocked, then refreshes the
    /// holder's base priority to the acquirer's own.
    fn note_acquired(&self, waiter: Option<scheduler::Priority>) {
        if let Some(p) = waiter {
            self.pi_state().remove_waiter(p);
        }
        refresh_holder_base(&self.pi);
    }
}

/// Refreshes a mutex's inheritance holder base to the calling thread's
/// registered priority (or `Normal` when unregistered).
///
/// Used on acquire paths that bypass [`Mutex::lock`] — e.g. the re-acquire
/// inside [`Condvar::wait`] — so the tracker's base always reflects the
/// actual holder.
fn refresh_holder_base(pi: &PiLock) {
    let base = crate::threading::current_task_priority()
        .map_or(scheduler::Priority::Normal, scheduler::Priority::from);
    #[cfg(feature = "platform-linux")]
    pi.lock().unwrap().set_base(base);
    #[cfg(feature = "platform-baremetal")]
    pi.lock().set_base(base);
}

impl<T> Mutex<T> {
    /// Acquires the mutex, blocking until available.
    ///
    /// If the lock is contended and the calling thread has a registered
    /// scheduling priority, the thread is registered as a priority-inheritance
    /// waiter while it blocks — donating its priority to the holder — and the
    /// registration is dropped once the lock is acquired.
    ///
    /// # Panics
    ///
    /// Panics if the underlying mutex is poisoned (Linux).
    pub fn lock(&self) -> MutexGuard<'_, T> {
        let waiter = Self::waiter_priority();

        // Fast path: uncontended acquire. No waiter to arm; just record the
        // new holder in the inheritance bookkeeping.
        #[cfg(feature = "platform-linux")]
        match self.inner.try_lock() {
            Ok(inner) => {
                self.note_acquired(waiter);
                return MutexGuard {
                    inner: MutexGuardInner::Linux(inner),
                    pi: &self.pi,
                };
            }
            Err(std::sync::TryLockError::Poisoned(e)) => panic!("mutex poisoned: {e}"),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        #[cfg(feature = "platform-baremetal")]
        if let Some(inner) = self.inner.try_lock() {
            self.note_acquired(waiter);
            return MutexGuard {
                inner: MutexGuardInner::Baremetal(inner),
                pi: &self.pi,
            };
        }

        // Slow path: the lock is contended. Arm a waiter so the holder runs
        // at our priority while we block (if our priority is known), then
        // block on the raw inner lock directly.
        if let Some(p) = waiter {
            self.pi_state().add_waiter(p);
        }

        #[cfg(feature = "platform-linux")]
        let inner = self.inner.lock().unwrap();
        #[cfg(feature = "platform-baremetal")]
        let inner = self.inner.lock();

        // Acquired: drop our waiter registration and record ourselves as the
        // new holder.
        self.note_acquired(waiter);

        MutexGuard {
            #[cfg(feature = "platform-linux")]
            inner: MutexGuardInner::Linux(inner),
            #[cfg(feature = "platform-baremetal")]
            inner: MutexGuardInner::Baremetal(inner),
            pi: &self.pi,
        }
    }

    /// Attempts to acquire the mutex without blocking.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        let waiter = Self::waiter_priority();
        #[cfg(feature = "platform-linux")]
        {
            let inner = self.inner.try_lock().ok()?;
            self.note_acquired(waiter);
            Some(MutexGuard {
                inner: MutexGuardInner::Linux(inner),
                pi: &self.pi,
            })
        }
        #[cfg(feature = "platform-baremetal")]
        {
            let inner = self.inner.try_lock()?;
            self.note_acquired(waiter);
            Some(MutexGuard {
                inner: MutexGuardInner::Baremetal(inner),
                pi: &self.pi,
            })
        }
    }

    /// The priority the current lock holder should run at: its own base
    /// priority, boosted to the highest-priority blocked waiter while one
    /// exists.
    ///
    /// This is the scheduler's re-targeting input for priority inheritance.
    /// While a higher-priority thread is blocked in [`Mutex::lock`], the
    /// dispatch loop should run the holder at this priority so it releases
    /// the lock promptly instead of being preempted by medium-priority work.
    /// The donation is armed and dropped automatically by `lock()`; waiters
    /// whose threads never registered a priority donate nothing.
    #[must_use]
    pub fn effective_priority(&self) -> scheduler::Priority {
        self.pi_state().effective()
    }

    /// Number of threads currently blocked in [`Mutex::lock`] with a
    /// registered priority — i.e. the active priority donors.
    #[must_use]
    pub fn waiter_count(&self) -> usize {
        self.pi_state().waiter_count()
    }
}

/// RAII guard for `Mutex`.
pub struct MutexGuard<'a, T> {
    inner: MutexGuardInner<'a, T>,
    /// Inheritance state of the mutex this guard was acquired from. Lets
    /// [`Condvar::wait`] refresh the holder's base priority when it
    /// re-acquires the mutex after blocking.
    ///
    /// Only *read* on Linux: the bare-metal `Condvar::wait` never releases
    /// the mutex, so there is no re-acquire to refresh (hence the
    /// `dead_code` allow on bare-metal builds).
    #[cfg_attr(feature = "platform-baremetal", allow(dead_code))]
    pi: &'a PiLock,
}

enum MutexGuardInner<'a, T> {
    #[cfg(feature = "platform-linux")]
    Linux(std::sync::MutexGuard<'a, T>),
    #[cfg(feature = "platform-baremetal")]
    Baremetal(spin::MutexGuard<'a, T>),
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.inner {
            #[cfg(feature = "platform-linux")]
            MutexGuardInner::Linux(g) => g.deref(),
            #[cfg(feature = "platform-baremetal")]
            MutexGuardInner::Baremetal(g) => g.deref(),
        }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match &mut self.inner {
            #[cfg(feature = "platform-linux")]
            MutexGuardInner::Linux(g) => g.deref_mut(),
            #[cfg(feature = "platform-baremetal")]
            MutexGuardInner::Baremetal(g) => g.deref_mut(),
        }
    }
}

// ===========================================================================
// RwLock
// ===========================================================================

/// A platform-abstracted reader-writer lock.
pub struct RwLock<T> {
    #[cfg(feature = "platform-linux")]
    inner: std::sync::RwLock<T>,
    #[cfg(feature = "platform-baremetal")]
    inner: spin::RwLock<T>,
}

unsafe impl<T: Send> Send for RwLock<T> {}
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            #[cfg(feature = "platform-linux")]
            inner: std::sync::RwLock::new(value),
            #[cfg(feature = "platform-baremetal")]
            inner: spin::RwLock::new(value),
        }
    }
}

impl<T> RwLock<T> {
    /// Acquires a shared read lock.
    ///
    /// # Panics
    ///
    /// Panics if the underlying lock is poisoned (Linux).
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        #[cfg(feature = "platform-linux")]
        {
            RwLockReadGuard {
                inner: RwLockReadGuardInner::Linux(self.inner.read().unwrap()),
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            RwLockReadGuard {
                inner: RwLockReadGuardInner::Baremetal(self.inner.read()),
            }
        }
    }

    /// Acquires an exclusive write lock.
    ///
    /// # Panics
    ///
    /// Panics if the underlying lock is poisoned (Linux).
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        #[cfg(feature = "platform-linux")]
        {
            RwLockWriteGuard {
                inner: RwLockWriteGuardInner::Linux(self.inner.write().unwrap()),
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            RwLockWriteGuard {
                inner: RwLockWriteGuardInner::Baremetal(self.inner.write()),
            }
        }
    }

    /// Attempts to acquire a shared read lock without blocking.
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        #[cfg(feature = "platform-linux")]
        {
            self.inner.try_read().ok().map(|g| RwLockReadGuard {
                inner: RwLockReadGuardInner::Linux(g),
            })
        }
        #[cfg(feature = "platform-baremetal")]
        {
            self.inner.try_read().map(|g| RwLockReadGuard {
                inner: RwLockReadGuardInner::Baremetal(g),
            })
        }
    }

    /// Attempts to acquire an exclusive write lock without blocking.
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        #[cfg(feature = "platform-linux")]
        {
            self.inner.try_write().ok().map(|g| RwLockWriteGuard {
                inner: RwLockWriteGuardInner::Linux(g),
            })
        }
        #[cfg(feature = "platform-baremetal")]
        {
            self.inner.try_write().map(|g| RwLockWriteGuard {
                inner: RwLockWriteGuardInner::Baremetal(g),
            })
        }
    }
}

/// RAII guard for shared read access.
pub struct RwLockReadGuard<'a, T> {
    inner: RwLockReadGuardInner<'a, T>,
}

enum RwLockReadGuardInner<'a, T> {
    #[cfg(feature = "platform-linux")]
    Linux(std::sync::RwLockReadGuard<'a, T>),
    #[cfg(feature = "platform-baremetal")]
    Baremetal(spin::RwLockReadGuard<'a, T>),
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.inner {
            #[cfg(feature = "platform-linux")]
            RwLockReadGuardInner::Linux(g) => g.deref(),
            #[cfg(feature = "platform-baremetal")]
            RwLockReadGuardInner::Baremetal(g) => g.deref(),
        }
    }
}

/// RAII guard for exclusive write access.
pub struct RwLockWriteGuard<'a, T> {
    inner: RwLockWriteGuardInner<'a, T>,
}

enum RwLockWriteGuardInner<'a, T> {
    #[cfg(feature = "platform-linux")]
    Linux(std::sync::RwLockWriteGuard<'a, T>),
    #[cfg(feature = "platform-baremetal")]
    Baremetal(spin::RwLockWriteGuard<'a, T>),
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match &self.inner {
            #[cfg(feature = "platform-linux")]
            RwLockWriteGuardInner::Linux(g) => g.deref(),
            #[cfg(feature = "platform-baremetal")]
            RwLockWriteGuardInner::Baremetal(g) => g.deref(),
        }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match &mut self.inner {
            #[cfg(feature = "platform-linux")]
            RwLockWriteGuardInner::Linux(g) => g.deref_mut(),
            #[cfg(feature = "platform-baremetal")]
            RwLockWriteGuardInner::Baremetal(g) => g.deref_mut(),
        }
    }
}

// ===========================================================================
// Condvar
// ===========================================================================

/// A platform-abstracted condition variable.
///
/// On Linux: delegates to `std::sync::Condvar`.
/// On bare-metal: spin-waits (sufficient for Phase 1; proper wait queues in Phase 6).
pub struct Condvar {
    #[cfg(feature = "platform-linux")]
    inner: std::sync::Condvar,
    #[cfg(feature = "platform-baremetal")]
    _phantom: (),
}

impl Condvar {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            #[cfg(feature = "platform-linux")]
            inner: std::sync::Condvar::new(),
            #[cfg(feature = "platform-baremetal")]
            _phantom: (),
        }
    }

    /// Blocks the current thread until notified.
    ///
    /// # Panics
    ///
    /// Panics if the underlying condvar wait returns a poisoned error.
    ///
    /// The mutex guard is released while waiting and re-acquired before returning.
    ///
    /// On Linux: delegates to `std::sync::Condvar::wait` by extracting the inner
    /// `std::sync::MutexGuard`, waiting, and rewrapping.
    ///
    /// On bare-metal: immediately returns the guard (spin-wait semantics).
    /// Callers should use `wait` inside a loop that checks a predicate.
    #[cfg(feature = "platform-linux")]
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let pi = guard.pi;
        match guard.inner {
            MutexGuardInner::Linux(std_guard) => {
                let std_guard = self.inner.wait(std_guard).unwrap();
                // Re-acquired after blocking: refresh the holder's base
                // priority, since another thread may have held the mutex
                // while we waited.
                refresh_holder_base(pi);
                MutexGuard {
                    inner: MutexGuardInner::Linux(std_guard),
                    pi,
                }
            }
        }
    }

    /// Blocks the current thread until notified (bare-metal: spin-wait, returns immediately).
    #[cfg(feature = "platform-baremetal")]
    #[must_use]
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        // Phase 1: immediately return the guard.
        // Callers must use wait in a loop checking a predicate.
        core::hint::spin_loop();
        guard
    }

    /// Blocks until the predicate returns `true`.
    ///
    /// This is the preferred way to use a condition variable — it handles
    /// spurious wakeups automatically.
    pub fn wait_while<'a, T, F>(
        &self,
        mut guard: MutexGuard<'a, T>,
        mut predicate: F,
    ) -> MutexGuard<'a, T>
    where
        F: FnMut(&T) -> bool,
    {
        while predicate(&*guard) {
            guard = self.wait(guard);
        }
        guard
    }

    /// Wakes one waiting thread.
    #[cfg(feature = "platform-linux")]
    pub fn notify_one(&self) {
        self.inner.notify_one();
    }

    /// Wakes one waiting thread (bare-metal: no-op — the spin-wait `Condvar`
    /// keeps no wait queue, so waiters re-check their predicate on their own).
    #[cfg(feature = "platform-baremetal")]
    pub const fn notify_one(&self) {}

    /// Wakes all waiting threads.
    #[cfg(feature = "platform-linux")]
    pub fn notify_all(&self) {
        self.inner.notify_all();
    }

    /// Wakes all waiting threads (bare-metal: no-op — see [`Self::notify_one`]).
    #[cfg(feature = "platform-baremetal")]
    pub const fn notify_all(&self) {}
}

impl Default for Condvar {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// Channel — Bounded MPSC
// ===========================================================================

use core::fmt;

#[cfg(feature = "platform-linux")]
use std::{collections::VecDeque, sync::Arc};

#[cfg(feature = "platform-baremetal")]
use alloc::{collections::VecDeque, sync::Arc};

/// Internal shared state for a bounded channel.
struct ChannelInner<T> {
    buffer: VecDeque<T>,
    capacity: usize,
    closed: bool,
    /// Number of active senders. When this drops to 0, the channel is disconnected.
    sender_count: usize,
}

/// Shared state wrapped for thread-safe access.
///
/// On Linux: uses `std::sync::Mutex` + `std::sync::Condvar` directly for
/// correct blocking semantics without needing to unwrap our platform Mutex.
///
/// On bare-metal: uses our platform `Mutex` + `Condvar` (spin-based).
struct Shared<T> {
    #[cfg(feature = "platform-linux")]
    state: std::sync::Mutex<ChannelInner<T>>,
    #[cfg(feature = "platform-linux")]
    not_empty: std::sync::Condvar,
    #[cfg(feature = "platform-linux")]
    not_full: std::sync::Condvar,

    #[cfg(feature = "platform-baremetal")]
    state: Mutex<ChannelInner<T>>,
    #[cfg(feature = "platform-baremetal")]
    not_empty: Condvar,
    #[cfg(feature = "platform-baremetal")]
    not_full: Condvar,
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Error returned by `Sender::send` when the channel is closed.
#[derive(Debug, PartialEq, Eq)]
pub struct SendError<T>(pub T);

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sending on a closed channel")
    }
}

/// Error returned by `Sender::try_send`.
#[derive(Debug, PartialEq, Eq)]
pub enum TrySendError<T> {
    /// The channel buffer is full.
    Full(T),
    /// The channel is closed (receiver dropped).
    Closed(T),
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => write!(f, "channel is full"),
            Self::Closed(_) => write!(f, "sending on a closed channel"),
        }
    }
}

/// Error returned by `Receiver::recv` when all senders are dropped and the
/// buffer is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecvError;

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "receiving on a closed channel")
    }
}

/// Error returned by `Receiver::try_recv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryRecvError {
    /// The channel buffer is empty but senders still exist.
    Empty,
    /// All senders have been dropped and the buffer is empty.
    Disconnected,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "channel is empty"),
            Self::Disconnected => write!(f, "channel is disconnected"),
        }
    }
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// The sending half of a bounded channel. Cloneable (multiple producers).
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

// Sender is Clone — this is the "MP" in MPSC.
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        #[cfg(feature = "platform-linux")]
        {
            let mut inner = self.shared.state.lock().unwrap();
            inner.sender_count += 1;
        }
        #[cfg(feature = "platform-baremetal")]
        {
            let mut inner = self.shared.state.lock();
            inner.sender_count += 1;
        }
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        #[cfg(feature = "platform-linux")]
        {
            let mut inner = self.shared.state.lock().unwrap();
            inner.sender_count -= 1;
            if inner.sender_count == 0 {
                inner.closed = true;
                // Wake the receiver so it can observe disconnection.
                drop(inner);
                self.shared.not_empty.notify_all();
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            let mut inner = self.shared.state.lock();
            inner.sender_count -= 1;
            if inner.sender_count == 0 {
                inner.closed = true;
                drop(inner);
                self.shared.not_empty.notify_all();
            }
        }
    }
}

impl<T> Sender<T> {
    /// Sends a value, blocking until space is available.
    ///
    /// # Errors
    ///
    /// Returns `Err(SendError(value))` if the channel is closed (receiver dropped).
    ///
    /// # Panics
    ///
    /// Panics if the underlying mutex is poisoned (Linux mode).
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        #[cfg(feature = "platform-linux")]
        {
            let mut inner = self.shared.state.lock().unwrap();
            loop {
                if inner.closed {
                    return Err(SendError(value));
                }
                if inner.buffer.len() < inner.capacity {
                    inner.buffer.push_back(value);
                    self.shared.not_empty.notify_one();
                    return Ok(());
                }
                inner = self.shared.not_full.wait(inner).unwrap();
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            loop {
                {
                    let mut inner = self.shared.state.lock();
                    if inner.closed {
                        return Err(SendError(value));
                    }
                    if inner.buffer.len() < inner.capacity {
                        inner.buffer.push_back(value);
                        self.shared.not_empty.notify_one();
                        return Ok(());
                    }
                }
                core::hint::spin_loop();
            }
        }
    }

    /// Attempts to send without blocking.
    ///
    /// # Errors
    ///
    /// Returns `Err(TrySendError::Closed(value))` if the channel is closed, or
    /// `Err(TrySendError::Full(value))` if the buffer is at capacity.
    ///
    /// # Panics
    ///
    /// Panics if the underlying mutex is poisoned (Linux mode).
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        #[cfg(feature = "platform-linux")]
        let mut inner = self.shared.state.lock().unwrap();
        #[cfg(feature = "platform-baremetal")]
        let mut inner = self.shared.state.lock();

        if inner.closed {
            return Err(TrySendError::Closed(value));
        }
        if inner.buffer.len() >= inner.capacity {
            return Err(TrySendError::Full(value));
        }
        inner.buffer.push_back(value);
        drop(inner);
        self.shared.not_empty.notify_one();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

/// The receiving half of a bounded channel. NOT cloneable (single consumer).
pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        #[cfg(feature = "platform-linux")]
        {
            {
                let mut inner = self.shared.state.lock().unwrap();
                inner.closed = true;
            }
            // Wake all blocked senders so they can observe the closure.
            self.shared.not_full.notify_all();
        }
        #[cfg(feature = "platform-baremetal")]
        {
            let mut inner = self.shared.state.lock();
            inner.closed = true;
            self.shared.not_full.notify_all();
        }
    }
}

impl<T> Receiver<T> {
    /// Receives a value, blocking until one is available.
    ///
    /// # Errors
    ///
    /// Returns `Err(RecvError)` if all senders have been dropped and the buffer
    /// is empty.
    ///
    /// # Panics
    ///
    /// Panics if the underlying mutex is poisoned (Linux mode).
    pub fn recv(&self) -> Result<T, RecvError> {
        #[cfg(feature = "platform-linux")]
        {
            let mut inner = self.shared.state.lock().unwrap();
            loop {
                if let Some(val) = inner.buffer.pop_front() {
                    self.shared.not_full.notify_one();
                    return Ok(val);
                }
                if inner.closed {
                    return Err(RecvError);
                }
                inner = self.shared.not_empty.wait(inner).unwrap();
            }
        }
        #[cfg(feature = "platform-baremetal")]
        {
            loop {
                {
                    let mut inner = self.shared.state.lock();
                    if let Some(val) = inner.buffer.pop_front() {
                        self.shared.not_full.notify_one();
                        return Ok(val);
                    }
                    if inner.closed {
                        return Err(RecvError);
                    }
                }
                core::hint::spin_loop();
            }
        }
    }

    /// Attempts to receive without blocking.
    ///
    /// # Errors
    ///
    /// Returns `Err(TryRecvError::Disconnected)` if the channel is closed, or
    /// `Err(TryRecvError::Empty)` if no values are available.
    ///
    /// # Panics
    ///
    /// Panics if the underlying mutex is poisoned (Linux mode).
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        #[cfg(feature = "platform-linux")]
        let mut inner = self.shared.state.lock().unwrap();
        #[cfg(feature = "platform-baremetal")]
        let mut inner = self.shared.state.lock();

        inner.buffer.pop_front().map_or_else(
            || {
                if inner.closed {
                    Err(TryRecvError::Disconnected)
                } else {
                    Err(TryRecvError::Empty)
                }
            },
            |val| {
                self.shared.not_full.notify_one();
                Ok(val)
            },
        )
    }
}

// ---------------------------------------------------------------------------
// Constructor
// ---------------------------------------------------------------------------

/// Creates a bounded MPSC channel with the given capacity.
///
/// Returns a `(Sender<T>, Receiver<T>)` pair.
///
/// # Panics
///
/// Panics if `capacity` is 0.
#[must_use]
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "channel capacity must be > 0");

    let inner = ChannelInner {
        buffer: VecDeque::with_capacity(capacity),
        capacity,
        closed: false,
        sender_count: 1,
    };

    let shared = Arc::new(Shared {
        #[cfg(feature = "platform-linux")]
        state: std::sync::Mutex::new(inner),
        #[cfg(feature = "platform-linux")]
        not_empty: std::sync::Condvar::new(),
        #[cfg(feature = "platform-linux")]
        not_full: std::sync::Condvar::new(),

        #[cfg(feature = "platform-baremetal")]
        state: Mutex::new(inner),
        #[cfg(feature = "platform-baremetal")]
        not_empty: Condvar::new(),
        #[cfg(feature = "platform-baremetal")]
        not_full: Condvar::new(),
    });

    let sender = Sender {
        shared: Arc::clone(&shared),
    };
    let receiver = Receiver { shared };

    (sender, receiver)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::threading::scheduler;
    use std::sync::Arc;

    /// Register (or clear) the current test thread's task priority.
    fn set_thread_priority(p: Option<crate::threading::Priority>) {
        crate::threading::set_current_task_priority(p);
    }

    #[test]
    fn mutex_basic() {
        let m = Mutex::new(42);
        {
            let mut g = m.lock();
            assert_eq!(*g, 42);
            *g = 100;
        }
        assert_eq!(*m.lock(), 100);
    }

    #[test]
    fn mutex_try_lock() {
        let m = Mutex::new(0);
        let g = m.lock();
        assert!(m.try_lock().is_none());
        drop(g);
        assert!(m.try_lock().is_some());
    }

    #[test]
    fn mutex_threaded() {
        let m = Arc::new(Mutex::new(0u64));
        let mut handles = vec![];

        for _ in 0..10 {
            let m = Arc::clone(&m);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    *m.lock() += 1;
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*m.lock(), 10_000);
    }

    #[test]
    fn mutex_pi_uncontended_acquire_records_holder_base() {
        set_thread_priority(Some(crate::threading::Priority::High));
        let m = Mutex::new(0);
        {
            let _guard = m.lock();
            assert_eq!(m.effective_priority(), scheduler::Priority::High);
            assert_eq!(m.waiter_count(), 0);
        }
        set_thread_priority(None);
    }

    #[test]
    fn mutex_pi_try_lock_records_holder_base() {
        set_thread_priority(Some(crate::threading::Priority::Critical));
        let m = Mutex::new(0);
        let _guard = m.try_lock().expect("uncontended try_lock succeeds");
        assert_eq!(m.effective_priority(), scheduler::Priority::Critical);
        assert_eq!(m.waiter_count(), 0);
        set_thread_priority(None);
    }

    #[test]
    fn mutex_pi_unregistered_thread_donates_nothing() {
        set_thread_priority(None);
        let m = Mutex::new(0);
        let _guard = m.lock();
        // No registered priority: holder base falls back to Normal, no waiter.
        assert_eq!(m.effective_priority(), scheduler::Priority::Normal);
        assert_eq!(m.waiter_count(), 0);
    }

    #[test]
    fn mutex_pi_blocked_waiter_boosts_holder_until_acquire() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        // Main thread holds the mutex at Low priority.
        set_thread_priority(Some(crate::threading::Priority::Low));
        let m = Arc::new(Mutex::new(0u32));
        let holder = m.lock();
        assert_eq!(m.effective_priority(), scheduler::Priority::Low);

        // A Critical thread blocks on the mutex; while blocked it donates its
        // priority to the holder.
        let m2 = Arc::clone(&m);
        let acquired = Arc::new(AtomicBool::new(false));
        let acquired_flag = Arc::clone(&acquired);
        let waiter = std::thread::spawn(move || {
            set_thread_priority(Some(crate::threading::Priority::Critical));
            let _guard = m2.lock();
            // Acquired: waiter registration dropped, holder base = Critical.
            assert_eq!(m2.effective_priority(), scheduler::Priority::Critical);
            assert_eq!(m2.waiter_count(), 0);
            acquired_flag.store(true, Ordering::SeqCst);
        });

        // Wait until the waiter arms its donation (bounded poll).
        let deadline = Instant::now() + Duration::from_secs(10);
        while m.effective_priority() != scheduler::Priority::Critical {
            assert!(
                Instant::now() < deadline,
                "blocked waiter never armed its priority donation"
            );
            std::thread::yield_now();
        }
        assert_eq!(m.waiter_count(), 1);

        // Release: the waiter acquires, drops its registration, becomes holder.
        drop(holder);
        waiter.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst));
        assert_eq!(m.waiter_count(), 0);

        set_thread_priority(None);
    }

    // The guard is intentionally held across the spawn below: B's try_lock
    // spin can only succeed once A releases the mutex inside wait(), which
    // is what forces the wait()/re-acquire path (and the PI base refresh)
    // to be exercised deterministically.
    #[allow(clippy::significant_drop_tightening)]
    #[test]
    fn condvar_wait_refreshes_pi_holder_base() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let pair = Arc::new((Mutex::new(0u32), Condvar::new()));
        let pair_b = Arc::clone(&pair);
        let b_done = Arc::new(AtomicBool::new(false));
        let b_done_b = Arc::clone(&b_done);

        // Main thread: Critical holder that is about to wait on the condvar.
        set_thread_priority(Some(crate::threading::Priority::Critical));
        let (lock, cvar) = &*pair;
        let guard = lock.lock();
        assert_eq!(lock.effective_priority(), scheduler::Priority::Critical);

        // Thread B: spins until A blocks in wait() (mutex becomes free), holds
        // the mutex briefly at Low — moving the PI base to Low — then wakes A.
        let handle = std::thread::spawn(move || {
            set_thread_priority(Some(crate::threading::Priority::Low));
            let (lock_b, cvar_b) = &*pair_b;
            let b_guard = loop {
                if let Some(g) = lock_b.try_lock() {
                    break g;
                }
                std::thread::yield_now();
            };
            assert_eq!(lock_b.effective_priority(), scheduler::Priority::Low);
            b_done_b.store(true, Ordering::SeqCst);
            drop(b_guard);
            cvar_b.notify_one();
        });

        let guard = cvar.wait_while(guard, |_| !b_done.load(Ordering::SeqCst));
        // wait() re-acquired the mutex: the holder base must reflect A again,
        // not the stale Low left behind by B.
        assert_eq!(lock.effective_priority(), scheduler::Priority::Critical);
        assert_eq!(lock.waiter_count(), 0);
        drop(guard);
        handle.join().unwrap();

        set_thread_priority(None);
    }

    #[test]
    fn rwlock_basic() {
        let rw = RwLock::new(42);
        assert_eq!(*rw.read(), 42);
        *rw.write() = 100;
        assert_eq!(*rw.read(), 100);
    }

    #[test]
    fn rwlock_concurrent_readers() {
        let rw = Arc::new(RwLock::new(42));
        let mut handles = vec![];

        for _ in 0..10 {
            let rw = Arc::clone(&rw);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    let _ = *rw.read();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn condvar_notify() {
        let cv = Condvar::new();
        cv.notify_one();
        cv.notify_all();
    }

    #[test]
    fn condvar_wait_notify() {
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let pair2 = Arc::clone(&pair);

        let handle = std::thread::spawn(move || {
            let (lock, cvar) = &*pair2;
            let mut started = lock.lock();
            *started = true;
            drop(started);
            cvar.notify_one();
        });

        let (lock, cvar) = &*pair;
        let guard = cvar.wait_while(lock.lock(), |started| !*started);
        assert!(*guard);
        drop(guard);
        handle.join().unwrap();
    }

    #[test]
    fn channel_basic() {
        let (tx, rx) = channel(8);
        tx.send(42).unwrap();
        tx.send(99).unwrap();
        assert_eq!(rx.recv().unwrap(), 42);
        assert_eq!(rx.recv().unwrap(), 99);
    }

    #[test]
    fn channel_bounded_blocking() {
        let (tx, rx) = channel(2);
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        // Buffer is full — try_send should fail.
        assert!(matches!(tx.try_send(3), Err(TrySendError::Full(3))));
        // Drain one, then try_send should succeed.
        assert_eq!(rx.recv().unwrap(), 1);
        tx.try_send(3).unwrap();
        assert_eq!(rx.recv().unwrap(), 2);
        assert_eq!(rx.recv().unwrap(), 3);
    }

    #[test]
    fn channel_mpsc() {
        let (tx, rx) = channel(64);
        let mut handles = vec![];

        for i in 0..4 {
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || {
                for j in 0..25 {
                    tx.send(i * 25 + j).unwrap();
                }
            }));
        }
        // Drop the original sender so the channel closes when threads finish.
        drop(tx);

        let mut received = vec![];
        while let Ok(val) = rx.recv() {
            received.push(val);
        }

        for h in handles {
            h.join().unwrap();
        }

        received.sort_unstable();
        assert_eq!(received, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn channel_recv_disconnected() {
        let (tx, rx) = channel::<i32>(4);
        drop(tx);
        assert_eq!(rx.recv(), Err(RecvError));
    }

    #[test]
    fn channel_try_recv_empty() {
        let (tx, rx) = channel::<i32>(4);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        drop(tx);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }
}

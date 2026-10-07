//! Thread spawning backed by enlil-platform.
//!
//! Provides a `spawn()` function matching `std::thread::spawn` semantics,
//! but routing through the enlil platform threading layer.
//!
//! On the hosted (`platform-linux`) backend each task runs on a real OS thread
//! via [`enlil_platform::threading::spawn_hosted`]. On the bare-metal backend
//! tasks are submitted to a lazily-initialized global
//! [`enlil_platform::threading::BareMetalScheduler`] and pumped to completion
//! by [`JoinHandle::join`], so the same `spawn`/`join` source builds and runs
//! on the `x86_64-unknown-enlil` target.

#[cfg(feature = "platform-baremetal")]
use alloc::boxed::Box;
#[cfg(feature = "platform-baremetal")]
use alloc::string::String;
#[cfg(feature = "platform-baremetal")]
use alloc::sync::Arc;
#[cfg(feature = "platform-linux")]
use core::any::Any;
#[cfg(feature = "platform-baremetal")]
use core::any::Any;
#[cfg(feature = "platform-baremetal")]
use enlil_platform::sync::Mutex;
#[cfg(feature = "platform-baremetal")]
use enlil_platform::threading::BareMetalScheduler;
#[cfg(feature = "platform-linux")]
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Hosted backend (`platform-linux`)
// ---------------------------------------------------------------------------

/// Handle to a spawned thread, similar to `std::thread::JoinHandle`.
#[cfg(feature = "platform-linux")]
pub struct JoinHandle<T> {
    inner: std::thread::JoinHandle<()>,
    result: Arc<Mutex<Option<T>>>,
}

#[cfg(feature = "platform-linux")]
impl<T> JoinHandle<T> {
    /// Block until the thread finishes and return its result.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the spawned thread panicked.
    ///
    /// # Panics
    ///
    /// Panics if the result mutex is poisoned or the thread completed
    /// without producing a result (should not happen in normal operation).
    pub fn join(self) -> Result<T, Box<dyn Any + Send>> {
        self.inner.join()?;
        let val = self
            .result
            .lock()
            .unwrap()
            .take()
            .expect("thread completed but produced no result");
        Ok(val)
    }
}

/// Spawn a new thread, returning a `JoinHandle`.
///
/// This mirrors `std::thread::spawn` but routes through `enlil_platform::threading`.
/// On the linux backend, this ultimately uses `std::thread` under the hood.
/// On bare-metal, it uses the platform scheduler.
///
/// # Panics
///
/// Panics if the result mutex is poisoned when the thread completes.
#[cfg(feature = "platform-linux")]
#[must_use]
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let result = Arc::new(Mutex::new(None));
    let result_clone = result.clone();

    // Create an enlil-platform Task that captures the closure
    let task = enlil_platform::threading::Task::spawn("enlil-std-thread", move || {
        let val = f();
        *result_clone.lock().unwrap() = Some(val);
    });

    // Use the platform's hosted spawn mechanism
    let handle = enlil_platform::threading::spawn_hosted(task);

    JoinHandle {
        inner: handle,
        result,
    }
}

// ---------------------------------------------------------------------------
// Bare-metal backend (`platform-baremetal`)
// ---------------------------------------------------------------------------

/// Lazily-initialized global scheduler for bare-metal `spawn`.
///
/// `no_std` has no `std::sync::OnceLock`, so this uses `spin::Once` (already a
/// workspace dependency of the platform layer).
#[cfg(feature = "platform-baremetal")]
static SCHEDULER: spin::Once<BareMetalScheduler> = spin::Once::new();

/// The shared bare-metal scheduler, initialized on first use.
#[cfg(feature = "platform-baremetal")]
fn scheduler() -> &'static BareMetalScheduler {
    SCHEDULER.call_once(|| BareMetalScheduler::new(1))
}

/// Handle to a spawned bare-metal task, similar to `std::thread::JoinHandle`.
///
/// `join` pumps the global [`BareMetalScheduler`] until the task's result is
/// available.
#[cfg(feature = "platform-baremetal")]
pub struct JoinHandle<T> {
    result: Arc<Mutex<Option<T>>>,
}

#[cfg(feature = "platform-baremetal")]
impl<T> JoinHandle<T> {
    /// Pump the scheduler until the task finishes and return its result.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the spawned task panicked. With the bare-metal
    /// `panic = "abort"` target strategy a panic never unwinds, so the error
    /// variant is unreachable in practice; it exists for API parity with
    /// `std::thread::JoinHandle::join`.
    pub fn join(self) -> Result<T, Box<dyn Any + Send>> {
        let sched = scheduler();
        loop {
            {
                let mut guard = self.result.lock();
                if let Some(val) = guard.take() {
                    return Ok(val);
                }
            }
            if !sched.run_one(0) {
                // No runnable task and no result yet — spin briefly and retry.
                core::hint::spin_loop();
            }
        }
    }
}

/// Spawn a new bare-metal task, returning a `JoinHandle`.
///
/// The task is submitted to the global [`BareMetalScheduler`]; it runs when
/// [`JoinHandle::join`] (or any other scheduler pump) executes it.
///
/// # Panics
///
/// Panics if the task cannot be submitted to the scheduler.
#[cfg(feature = "platform-baremetal")]
#[must_use]
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let result = Arc::new(Mutex::new(None));
    let result_clone = Arc::clone(&result);

    let task = enlil_platform::threading::Task::spawn("enlil-std-thread", move || {
        let val = f();
        *result_clone.lock() = Some(val);
    });

    scheduler()
        .submit(task)
        .expect("bare-metal scheduler rejected enlil-std task");

    JoinHandle { result }
}

/// Put the current thread to sleep for the given duration.
pub fn sleep(dur: core::time::Duration) {
    enlil_platform::time::sleep(dur);
}

/// Get the current thread's name.
///
/// Delegates to `std` on the hosted backend; bare-metal tasks are unnamed, so
/// this returns `None` there.
#[cfg(feature = "platform-linux")]
#[must_use]
pub fn current_thread_name() -> Option<String> {
    std::thread::current()
        .name()
        .map(std::string::ToString::to_string)
}

/// Get the current thread's name (bare-metal: tasks are unnamed).
#[cfg(feature = "platform-baremetal")]
#[must_use]
pub fn current_thread_name() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_join() {
        let handle = spawn(|| 42);
        assert_eq!(handle.join().unwrap(), 42);
    }

    #[test]
    fn spawn_with_move() {
        let data = [1, 2, 3];
        let handle = spawn(move || data.iter().sum::<i32>());
        assert_eq!(handle.join().unwrap(), 6);
    }

    #[test]
    fn spawn_string_result() {
        let handle = spawn(|| String::from("hello from enlil thread"));
        let result = handle.join().unwrap();
        assert!(result.contains("enlil"));
    }

    #[test]
    fn spawn_multiple_threads() {
        let mut handles = vec![];
        for i in 0..4 {
            handles.push(spawn(move || i * i));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(results, vec![0, 1, 4, 9]);
    }
}

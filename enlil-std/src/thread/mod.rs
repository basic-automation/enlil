//! Thread spawning backed by `enlil_platform::sys::thread`.
//!
//! Provides `spawn`/`sleep`/`current_thread_name` with `std::thread`-like
//! semantics, routed through the platform's `sys` bindings — the
//! `std::sys`-equivalent layer both backends implement — so this module is
//! backend-agnostic: one implementation serves Linux and
//! `x86_64-unknown-enlil`.
//!
//! On Linux each task runs on a real OS thread; on bare metal tasks are
//! submitted to the `sys` scheduler and pumped to completion by
//! [`JoinHandle::join`].

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;
use core::time::Duration;

use enlil_platform::sync::Mutex;
use enlil_platform::sys::thread as sys_thread;

/// Handle to a spawned thread, similar to `std::thread::JoinHandle`.
pub struct JoinHandle<T> {
    inner: sys_thread::JoinHandle<()>,
    result: Arc<Mutex<Option<T>>>,
}

impl<T> JoinHandle<T> {
    /// Block until the thread finishes and return its result.
    ///
    /// On bare metal this pumps the `sys` scheduler until the task
    /// completes.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the spawned thread panicked or vanished without
    /// producing a result.
    ///
    /// # Panics
    ///
    /// Panics if the thread completed without producing a result (should
    /// not happen in normal operation).
    pub fn join(self) -> Result<T, Box<dyn Any + Send>> {
        self.inner
            .join()
            .map_err(|e| Box::new(e) as Box<dyn Any + Send>)?;
        let val = self
            .result
            .lock()
            .take()
            .expect("thread completed but produced no result");
        Ok(val)
    }
}

/// Spawn a new thread, returning a `JoinHandle`.
///
/// Mirrors `std::thread::spawn`, routed through
/// `enlil_platform::sys::thread::spawn`.
///
/// # Panics
///
/// Panics if the platform refuses to spawn the thread.
#[must_use]
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let result = Arc::new(Mutex::new(None));
    let result_clone = Arc::clone(&result);

    let inner = sys_thread::spawn(Some("enlil-std-thread"), move || {
        let val = f();
        *result_clone.lock() = Some(val);
    })
    .expect("enlil sys thread spawn failed");

    JoinHandle { inner, result }
}

/// Put the current thread to sleep for the given duration.
pub fn sleep(dur: Duration) {
    sys_thread::sleep(dur);
}

/// Get the current thread's name.
///
/// Delegates to the `sys` thread handle; bare-metal contexts are unnamed,
/// so this returns `None` there.
#[must_use]
pub fn current_thread_name() -> Option<String> {
    sys_thread::current().name().map(String::from)
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

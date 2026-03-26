//! Thread spawning backed by enlil-platform.
//!
//! Provides a `spawn()` function matching `std::thread::spawn` semantics,
//! but routing through the enlil platform threading layer.

use std::sync::{Arc, Mutex};

/// Handle to a spawned thread, similar to `std::thread::JoinHandle`.
pub struct JoinHandle<T> {
    inner: std::thread::JoinHandle<()>,
    result: Arc<Mutex<Option<T>>>,
}

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
    pub fn join(self) -> Result<T, Box<dyn std::any::Any + Send>> {
        self.inner.join()?;
        let val = self.result.lock().unwrap().take()
            .expect("thread completed but produced no result");
        Ok(val)
    }
}

/// Spawn a new thread, returning a `JoinHandle`.
///
/// This mirrors `std::thread::spawn` but routes through `enlil_platform::threading`.
/// On the linux backend, this ultimately uses `std::thread` under the hood.
/// On bare-metal, it would use the platform scheduler.
///
/// # Panics
///
/// Panics if the result mutex is poisoned when the thread completes.
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

/// Put the current thread to sleep for the given duration.
pub fn sleep(dur: std::time::Duration) {
    enlil_platform::time::sleep(dur);
}

/// Get the current thread's ID (delegates to std on linux backend).
#[must_use]
pub fn current_thread_name() -> Option<String> {
    std::thread::current().name().map(std::string::ToString::to_string)
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

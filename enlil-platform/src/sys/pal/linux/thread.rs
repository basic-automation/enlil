//! Threading bindings — mirrors `std::sys::thread`'s role.
//!
//! Same signatures on both PALs; here backed by `std::thread`. The
//! bare-metal twin ([`super::super::enlil::thread`]) submits to the
//! platform scheduler instead.

use alloc::string::String;
use alloc::sync::Arc;
use core::cell::Cell;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use crate::io::{IoError, IoErrorKind, IoResult};
use crate::sync::Mutex;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CURRENT_ID: Cell<u64> = const { Cell::new(0) };
}

/// Opaque thread identifier, unique per spawned thread within a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(u64);

impl ThreadId {
    /// The raw numeric id.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

fn fresh_id() -> ThreadId {
    ThreadId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
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

/// Handle for a spawned thread; [`join`](JoinHandle::join) waits for it.
pub struct JoinHandle<T> {
    thread: Thread,
    inner: std::thread::JoinHandle<()>,
    result: Arc<Mutex<Option<T>>>,
}

impl<T> JoinHandle<T> {
    /// The spawned thread's handle.
    #[must_use]
    pub const fn thread(&self) -> &Thread {
        &self.thread
    }

    /// Wait for the thread to finish and return its result.
    ///
    /// # Errors
    ///
    /// Returns an error if the thread panicked or vanished without
    /// producing a result.
    pub fn join(self) -> IoResult<T> {
        self.inner
            .join()
            .map_err(|_| IoError::new(IoErrorKind::Other))?;
        self.result
            .lock()
            .take()
            .ok_or_else(|| IoError::with_message(IoErrorKind::Other, "thread produced no result"))
    }
}

/// Spawn a thread running `f`, mirroring `std::thread::spawn`.
///
/// Unlike `std`, spawn can fail (resource exhaustion), so this returns
/// [`IoResult`]; the bare-metal PAL needs the `Err` case.
///
/// # Errors
///
/// Returns an error if the OS refused to create the thread.
pub fn spawn<F, T>(name: Option<&str>, f: F) -> IoResult<JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let id = fresh_id();
    let owned_name = name.map(String::from);
    let result = Arc::new(Mutex::new(None));
    let result_clone = Arc::clone(&result);

    let mut builder = std::thread::Builder::new();
    if let Some(n) = &owned_name {
        builder = builder.name(n.clone());
    }
    let inner = builder
        .spawn(move || {
            CURRENT_ID.with(|c| c.set(id.0));
            let val = f();
            *result_clone.lock() = Some(val);
        })
        .map_err(IoError::from)?;

    Ok(JoinHandle {
        thread: Thread {
            id,
            name: owned_name,
        },
        inner,
        result,
    })
}

/// Returns a handle to the calling thread.
///
/// The id is stable for the lifetime of the thread: the first call on a
/// thread mints an id and pins it with a thread-local.
#[must_use]
pub fn current() -> Thread {
    let id = CURRENT_ID.with(|c| {
        let existing = c.get();
        if existing == 0 {
            let fresh = fresh_id().0;
            c.set(fresh);
            fresh
        } else {
            existing
        }
    });
    Thread {
        id: ThreadId(id),
        name: std::thread::current().name().map(String::from),
    }
}

/// Cooperatively yields the remainder of the current scheduling quantum.
pub fn yield_now() {
    std::thread::yield_now();
}

/// Puts the current thread to sleep for at least `dur`.
pub fn sleep(dur: Duration) {
    std::thread::sleep(dur);
}

/// Number of hardware threads the OS reports.
///
/// # Errors
///
/// Returns an error if the OS does not report a thread count.
pub fn available_parallelism() -> IoResult<usize> {
    std::thread::available_parallelism()
        .map_err(IoError::from)
        .map(std::num::NonZero::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_join_roundtrip() {
        let h = spawn(Some("sys-test"), || 40 + 2).unwrap();
        assert_eq!(h.thread().name(), Some("sys-test"));
        assert_eq!(h.join().unwrap(), 42);
    }

    #[test]
    fn spawn_unnamed() {
        let h = spawn(None, || String::from("hi")).unwrap();
        assert_eq!(h.thread().name(), None);
        assert_eq!(h.join().unwrap(), "hi");
    }

    #[test]
    fn current_id_is_stable() {
        assert_eq!(current().id(), current().id());
        assert_ne!(current().id().raw(), 0);
    }

    #[test]
    fn spawned_ids_are_unique() {
        let a = spawn(None, || current().id()).unwrap().join().unwrap();
        let b = spawn(None, || current().id()).unwrap().join().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn parallelism_nonzero() {
        assert!(available_parallelism().unwrap() >= 1);
    }

    #[test]
    fn sleep_and_yield() {
        yield_now();
        sleep(Duration::from_millis(1));
    }
}

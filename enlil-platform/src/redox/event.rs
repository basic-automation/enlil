//! Redox kernel `EventQueue` port.
//!
//! Ports the event queue from Redox OS's kernel (`kernel/src/event.rs`):
//! tasks [`register`](EventQueue::register) interest in event IDs, producers
//! [`trigger`](EventQueue::trigger) readiness on them, and consumers drain
//! the queue with [`read`](EventQueue::read) /
//! [`try_read`](EventQueue::try_read). Like Redox's queue, triggering an ID
//! nobody registered is an error, and unregistering drops that ID's pending
//! events.
//!
//! Blocking note: [`EventQueue::read`] sleeps on the platform
//! [`Condvar`](crate::sync::Condvar), which on bare metal is a spin-wait
//! fallback — call `read` there only when an event is already pending (or
//! another core is guaranteed to trigger one), and prefer `try_read` in
//! interrupt-adjacent paths.

use alloc::collections::{BTreeSet, VecDeque};
use core::fmt;
use core::ops::BitOr;

use crate::sync::{Condvar, Mutex};

/// Event identifier, as in Redox's `Event { id, flags, data }`.
pub type EventId = usize;

/// Readiness flags carried by an [`Event`], mirroring Redox's `EVENT_READ` /
/// `EVENT_WRITE` constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventFlags(u32);

impl EventFlags {
    /// The source has data to read.
    pub const READ: Self = Self(1);
    /// The source can accept writes.
    pub const WRITE: Self = Self(2);
    /// The source is in an error state.
    pub const ERROR: Self = Self(4);

    /// No flags set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The raw flag bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether all of `other`'s flags are set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for EventFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl Default for EventFlags {
    fn default() -> Self {
        Self::empty()
    }
}

/// One queued readiness notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// The ID passed to [`EventQueue::register`].
    pub id: EventId,
    /// What became ready.
    pub flags: EventFlags,
    /// Source-defined payload (Redox carries the scheme's data word here).
    pub data: usize,
}

/// Errors from [`EventQueue`] registration and triggering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventError {
    /// `trigger`/`unregister` for an ID with no registration.
    NotRegistered,
    /// `register` for an ID that is already registered.
    AlreadyRegistered,
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRegistered => write!(f, "event ID is not registered"),
            Self::AlreadyRegistered => write!(f, "event ID is already registered"),
        }
    }
}

#[cfg(feature = "platform-linux")]
impl std::error::Error for EventError {}

struct Inner {
    registered: BTreeSet<EventId>,
    pending: VecDeque<Event>,
}

/// A Redox-style event queue: registered IDs, triggered readiness, FIFO
/// delivery.
///
/// Cloning is deliberately unsupported — an event queue names one consumer,
/// the way Redox binds a queue to its context.
pub struct EventQueue {
    inner: Mutex<Inner>,
    readable: Condvar,
}

impl EventQueue {
    /// An empty queue with no registrations.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                registered: BTreeSet::new(),
                pending: VecDeque::new(),
            }),
            readable: Condvar::new(),
        }
    }

    /// Register interest in `id`.
    ///
    /// # Errors
    ///
    /// [`EventError::AlreadyRegistered`] when `id` is already registered.
    pub fn register(&self, id: EventId) -> Result<(), EventError> {
        let mut inner = self.inner.lock();
        if inner.registered.contains(&id) {
            return Err(EventError::AlreadyRegistered);
        }
        inner.registered.insert(id);
        drop(inner);
        Ok(())
    }

    /// Drop a registration, discarding `id`'s pending events.
    ///
    /// # Errors
    ///
    /// [`EventError::NotRegistered`] when `id` was never registered.
    pub fn unregister(&self, id: EventId) -> Result<(), EventError> {
        let mut inner = self.inner.lock();
        if !inner.registered.remove(&id) {
            return Err(EventError::NotRegistered);
        }
        inner.pending.retain(|event| event.id != id);
        drop(inner);
        Ok(())
    }

    /// Queue a readiness notification for `id` and wake one reader.
    ///
    /// # Errors
    ///
    /// [`EventError::NotRegistered`] when `id` has no registration.
    pub fn trigger(&self, id: EventId, flags: EventFlags, data: usize) -> Result<(), EventError> {
        let mut inner = self.inner.lock();
        if !inner.registered.contains(&id) {
            return Err(EventError::NotRegistered);
        }
        inner.pending.push_back(Event { id, flags, data });
        drop(inner);
        self.readable.notify_one();
        Ok(())
    }

    /// Pop the oldest pending event, or `None` when the queue is empty.
    #[must_use]
    pub fn try_read(&self) -> Option<Event> {
        self.inner.lock().pending.pop_front()
    }

    /// Block until an event is available, then return it.
    ///
    /// On bare metal this spins inside the condvar fallback — see the module
    /// documentation before calling it there.
    pub fn read(&self) -> Event {
        let mut guard = self.inner.lock();
        loop {
            if let Some(event) = guard.pending.pop_front() {
                return event;
            }
            guard = self.readable.wait(guard);
        }
    }

    /// Number of events currently pending.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().pending.len()
    }

    /// Whether no event is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether `id` currently has a registration.
    #[must_use]
    pub fn is_registered(&self, id: EventId) -> bool {
        self.inner.lock().registered.contains(&id)
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the Linux-gated tests below use `Vec`.
    #[cfg(feature = "platform-linux")]
    use alloc::vec::Vec;

    #[test]
    fn register_trigger_read_roundtrip() {
        let q = EventQueue::new();
        assert!(q.is_empty());
        q.register(7).unwrap();
        assert!(q.is_registered(7));
        // Double registration is an error.
        assert_eq!(q.register(7), Err(EventError::AlreadyRegistered));

        q.trigger(7, EventFlags::READ, 0xDEAD).unwrap();
        q.trigger(7, EventFlags::WRITE, 0xBEEF).unwrap();
        assert_eq!(q.len(), 2);

        // FIFO delivery.
        let first = q.read();
        assert_eq!(
            first,
            Event {
                id: 7,
                flags: EventFlags::READ,
                data: 0xDEAD
            }
        );
        let second = q.try_read().unwrap();
        assert_eq!(second.flags, EventFlags::WRITE);
        assert_eq!(second.data, 0xBEEF);
        assert!(q.try_read().is_none());
        assert!(q.is_empty());
    }

    #[test]
    fn trigger_without_registration_fails() {
        let q = EventQueue::new();
        assert_eq!(
            q.trigger(3, EventFlags::READ, 0),
            Err(EventError::NotRegistered)
        );
        assert_eq!(q.unregister(3), Err(EventError::NotRegistered));
        assert!(q.is_empty());
    }

    #[test]
    fn unregister_drops_pending_events() {
        let q = EventQueue::new();
        q.register(1).unwrap();
        q.register(2).unwrap();
        q.trigger(1, EventFlags::READ, 11).unwrap();
        q.trigger(2, EventFlags::READ, 22).unwrap();
        q.unregister(1).unwrap();
        assert!(!q.is_registered(1));
        // Only ID 2's event survives.
        assert_eq!(q.len(), 1);
        assert_eq!(q.try_read().unwrap().id, 2);
    }

    #[test]
    fn event_flags_combine() {
        let flags = EventFlags::READ | EventFlags::WRITE;
        assert!(flags.contains(EventFlags::READ));
        assert!(flags.contains(EventFlags::WRITE));
        assert!(!flags.contains(EventFlags::ERROR));
        assert_eq!(EventFlags::empty().bits(), 0);
        assert_eq!(EventFlags::default(), EventFlags::empty());
    }

    #[cfg(feature = "platform-linux")]
    #[test]
    fn blocking_read_wakes_on_trigger() {
        use std::sync::Arc;
        let q = Arc::new(EventQueue::new());
        q.register(9).unwrap();
        let q2 = Arc::clone(&q);
        let handle = std::thread::spawn(move || {
            let event = q2.read();
            assert_eq!(event.id, 9);
            event.data
        });
        // Give the reader a moment to park, then trigger.
        std::thread::sleep(core::time::Duration::from_millis(50));
        q.trigger(9, EventFlags::READ, 42).unwrap();
        assert_eq!(handle.join().unwrap(), 42);
    }

    #[cfg(feature = "platform-linux")]
    #[test]
    fn concurrent_producers_all_delivered() {
        use std::sync::Arc;
        let q = Arc::new(EventQueue::new());
        for id in 0..4usize {
            q.register(id).unwrap();
        }
        let mut handles = Vec::new();
        for id in 0..4usize {
            let q2 = Arc::clone(&q);
            handles.push(std::thread::spawn(move || {
                for n in 0..25usize {
                    q2.trigger(id, EventFlags::READ, n).unwrap();
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(q.len(), 100);
        let mut counts = [0usize; 4];
        while let Some(event) = q.try_read() {
            counts[event.id] += 1;
        }
        assert_eq!(counts, [25, 25, 25, 25]);
    }
}

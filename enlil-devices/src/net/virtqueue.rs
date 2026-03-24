//! Virtqueue abstraction for TX and RX queues.
//!
//! This is a simplified, self-contained virtqueue model that tracks
//! descriptor rings and available/used indices. The actual guest memory
//! interaction will be wired through the hypervisor's memory subsystem.

use std::collections::VecDeque;
use std::fmt;

use thiserror::Error;

/// Errors from virtqueue operations.
#[derive(Debug, Error)]
pub enum VirtqueueError {
    #[error("queue is full (capacity {capacity})")]
    QueueFull { capacity: u16 },

    #[error("queue is empty")]
    QueueEmpty,

    #[error("invalid descriptor index {index} (max {max})")]
    InvalidDescriptor { index: u16, max: u16 },

    #[error("buffer too small: need {needed}, have {available}")]
    BufferTooSmall { needed: usize, available: usize },
}

/// A single descriptor in the virtqueue.
#[derive(Debug, Clone)]
pub struct VirtqDesc {
    /// Index of this descriptor in the ring.
    pub index: u16,
    /// Data buffer.
    pub data: Vec<u8>,
    /// Whether this is writable by the device (for RX).
    pub writable: bool,
}

/// A simplified virtqueue for modeling TX/RX without real guest memory.
///
/// In production, this would map to actual virtqueue rings in guest RAM.
/// This implementation uses a `VecDeque` to model the available/used ring
/// semantics for testing and development.
pub struct Virtqueue {
    /// Queue name (e.g., "rx", "tx").
    name: String,
    /// Maximum number of descriptors.
    capacity: u16,
    /// Pending descriptors (available ring).
    available: VecDeque<VirtqDesc>,
    /// Completed descriptors (used ring).
    used: VecDeque<VirtqDesc>,
    /// Next descriptor index to allocate.
    next_index: u16,
    /// Whether the queue is enabled.
    enabled: bool,
}

impl Virtqueue {
    /// Create a new virtqueue with the given name and capacity.
    ///
    /// Standard VirtIO queue sizes are powers of 2, max 32768.
    pub fn new(name: impl Into<String>, capacity: u16) -> Self {
        Self {
            name: name.into(),
            capacity,
            available: VecDeque::with_capacity(capacity as usize),
            used: VecDeque::with_capacity(capacity as usize),
            next_index: 0,
            enabled: false,
        }
    }

    /// Enable the queue (guest has configured it).
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// Disable the queue.
    pub fn disable(&mut self) {
        self.enabled = false;
    }

    /// Whether the queue is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Queue name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Queue capacity.
    pub fn capacity(&self) -> u16 {
        self.capacity
    }

    /// Number of available (pending) descriptors.
    pub fn available_count(&self) -> usize {
        self.available.len()
    }

    /// Number of used (completed) descriptors.
    pub fn used_count(&self) -> usize {
        self.used.len()
    }

    /// Push a buffer into the available ring (guest → device).
    ///
    /// For TX: the guest places a frame to send.
    /// For RX: the guest provides an empty buffer for the device to fill.
    pub fn push_available(&mut self, data: Vec<u8>, writable: bool) -> Result<u16, VirtqueueError> {
        if self.available.len() >= self.capacity as usize {
            return Err(VirtqueueError::QueueFull {
                capacity: self.capacity,
            });
        }

        let index = self.next_index;
        self.next_index = self.next_index.wrapping_add(1);

        self.available.push_back(VirtqDesc {
            index,
            data,
            writable,
        });

        Ok(index)
    }

    /// Pop the next available descriptor (device processing).
    pub fn pop_available(&mut self) -> Result<VirtqDesc, VirtqueueError> {
        self.available.pop_front().ok_or(VirtqueueError::QueueEmpty)
    }

    /// Peek at the next available descriptor without removing it.
    pub fn peek_available(&self) -> Option<&VirtqDesc> {
        self.available.front()
    }

    /// Push a completed descriptor into the used ring (device → guest).
    pub fn push_used(&mut self, desc: VirtqDesc) {
        self.used.push_back(desc);
    }

    /// Pop a completed descriptor from the used ring (guest consumption).
    pub fn pop_used(&mut self) -> Result<VirtqDesc, VirtqueueError> {
        self.used.pop_front().ok_or(VirtqueueError::QueueEmpty)
    }

    /// Check if there are available descriptors to process.
    pub fn has_available(&self) -> bool {
        !self.available.is_empty()
    }

    /// Check if there are used descriptors to consume.
    pub fn has_used(&self) -> bool {
        !self.used.is_empty()
    }

    /// Reset the queue to initial state.
    pub fn reset(&mut self) {
        self.available.clear();
        self.used.clear();
        self.next_index = 0;
        self.enabled = false;
    }
}

impl fmt::Debug for Virtqueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Virtqueue")
            .field("name", &self.name)
            .field("capacity", &self.capacity)
            .field("available", &self.available.len())
            .field("used", &self.used.len())
            .field("enabled", &self.enabled)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_push_pop() {
        let mut vq = Virtqueue::new("tx", 256);
        vq.enable();

        let idx = vq.push_available(vec![1, 2, 3], false).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(vq.available_count(), 1);

        let desc = vq.pop_available().unwrap();
        assert_eq!(desc.data, vec![1, 2, 3]);
        assert_eq!(desc.index, 0);
        assert!(!desc.writable);
    }

    #[test]
    fn used_ring() {
        let mut vq = Virtqueue::new("rx", 256);
        vq.enable();

        vq.push_available(vec![0; 1500], true).unwrap();
        let mut desc = vq.pop_available().unwrap();

        // Device fills the buffer.
        desc.data = vec![0xDE, 0xAD];
        vq.push_used(desc);

        assert!(vq.has_used());
        let used = vq.pop_used().unwrap();
        assert_eq!(used.data, vec![0xDE, 0xAD]);
    }

    #[test]
    fn queue_full() {
        let mut vq = Virtqueue::new("tx", 2);
        vq.push_available(vec![], false).unwrap();
        vq.push_available(vec![], false).unwrap();
        let err = vq.push_available(vec![], false).unwrap_err();
        assert!(matches!(err, VirtqueueError::QueueFull { capacity: 2 }));
    }

    #[test]
    fn queue_empty() {
        let mut vq = Virtqueue::new("tx", 256);
        let err = vq.pop_available().unwrap_err();
        assert!(matches!(err, VirtqueueError::QueueEmpty));
    }

    #[test]
    fn reset() {
        let mut vq = Virtqueue::new("tx", 256);
        vq.enable();
        vq.push_available(vec![1], false).unwrap();
        vq.reset();
        assert!(!vq.is_enabled());
        assert_eq!(vq.available_count(), 0);
    }
}

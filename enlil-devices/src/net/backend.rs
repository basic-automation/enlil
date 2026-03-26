//! Network backend trait and null backend.
//!
//! Backends handle the actual delivery of Ethernet frames
//! to/from the outside world (or nowhere, in the null case).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Trait for network backends that send/receive raw Ethernet frames.
///
/// Implementations include:
/// - `NullBackend`: drops everything (testing)
/// - `TapBackend`: Linux TAP device (production)
pub trait NetBackend: Send {
    /// Send an Ethernet frame out through this backend.
    /// Returns the number of bytes written, or an error.
    fn send(&mut self, frame: &[u8]) -> std::io::Result<usize>;

    /// Receive an Ethernet frame from this backend.
    /// Returns `Ok(n)` with n bytes written into `buf`, or
    /// `Ok(0)` if no frame is available (non-blocking).
    fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;

    /// Returns true if this backend has a frame ready to read.
    fn has_pending_rx(&self) -> bool;

    /// Human-readable name for logging.
    fn backend_name(&self) -> &str;
}

// ---------------------------------------------------------------------------
// Null Backend â€” cross-platform, for testing
// ---------------------------------------------------------------------------

/// A null network backend that drops all transmitted frames
/// and never delivers any received frames.
///
/// Useful for testing VirtIO-net device logic in isolation.
pub struct NullBackend {
    tx_count: u64,
    tx_bytes: u64,
}

impl NullBackend {
    pub fn new() -> Self {
        Self {
            tx_count: 0,
            tx_bytes: 0,
        }
    }

    /// Number of frames sent (dropped) through this backend.
    pub fn tx_count(&self) -> u64 {
        self.tx_count
    }

    /// Total bytes sent (dropped) through this backend.
    pub fn tx_bytes(&self) -> u64 {
        self.tx_bytes
    }
}

impl Default for NullBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NetBackend for NullBackend {
    fn send(&mut self, frame: &[u8]) -> std::io::Result<usize> {
        self.tx_count += 1;
        self.tx_bytes += frame.len() as u64;
        log::trace!("null backend: dropped {} byte frame", frame.len());
        Ok(frame.len())
    }

    fn recv(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        // Never delivers any frames.
        Ok(0)
    }

    fn has_pending_rx(&self) -> bool {
        false
    }

    fn backend_name(&self) -> &str {
        "null"
    }
}

// ---------------------------------------------------------------------------
// Loopback Backend â€” for testing inter-guest communication
// ---------------------------------------------------------------------------

/// A loopback backend that echoes transmitted frames back as received frames.
/// Useful for testing the full TXâ†’RX path.
#[allow(dead_code)]
pub struct LoopbackBackend {
    queue: VecDeque<Vec<u8>>,
}

#[allow(dead_code)]
impl LoopbackBackend {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }
}

impl Default for LoopbackBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NetBackend for LoopbackBackend {
    fn send(&mut self, frame: &[u8]) -> std::io::Result<usize> {
        self.queue.push_back(frame.to_vec());
        Ok(frame.len())
    }

    fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(frame) = self.queue.pop_front() {
            let len = frame.len().min(buf.len());
            buf[..len].copy_from_slice(&frame[..len]);
            Ok(len)
        } else {
            Ok(0)
        }
    }

    fn has_pending_rx(&self) -> bool {
        !self.queue.is_empty()
    }

    fn backend_name(&self) -> &str {
        "loopback"
    }
}

// ---------------------------------------------------------------------------
// Shared pipe backend â€” for connecting two endpoints in tests
// ---------------------------------------------------------------------------

/// One end of a shared-memory pipe for connecting two net devices in tests.
/// Frames sent on one end appear as received on the other.
#[allow(dead_code)]
pub struct PipeBackend {
    /// Frames we send go into the peer's rx queue.
    peer_rx: Arc<Mutex<VecDeque<Vec<u8>>>>,
    /// Our rx queue â€” the peer sends into this.
    our_rx: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

#[allow(dead_code)]
impl PipeBackend {
    /// Create a connected pair of pipe backends.
    pub fn pair() -> (Self, Self) {
        let q1 = Arc::new(Mutex::new(VecDeque::new()));
        let q2 = Arc::new(Mutex::new(VecDeque::new()));
        let a = Self {
            peer_rx: Arc::clone(&q2),
            our_rx: Arc::clone(&q1),
        };
        let b = Self {
            peer_rx: Arc::clone(&q1),
            our_rx: Arc::clone(&q2),
        };
        (a, b)
    }
}

impl NetBackend for PipeBackend {
    fn send(&mut self, frame: &[u8]) -> std::io::Result<usize> {
        self.peer_rx
            .lock()
            .unwrap()
            .push_back(frame.to_vec());
        Ok(frame.len())
    }

    fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(frame) = self.our_rx.lock().unwrap().pop_front() {
            let len = frame.len().min(buf.len());
            buf[..len].copy_from_slice(&frame[..len]);
            Ok(len)
        } else {
            Ok(0)
        }
    }

    fn has_pending_rx(&self) -> bool {
        !self.our_rx.lock().unwrap().is_empty()
    }

    fn backend_name(&self) -> &str {
        "pipe"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_backend_roundtrip() {
        let mut lb = LoopbackBackend::new();
        let data = b"hello loopback";
        assert!(lb.send(data).is_ok());
        let mut buf = [0u8; 64];
        let n = lb.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], data);
    }

    #[test]
    fn pipe_backend_pair() {
        let (mut a, mut b) = PipeBackend::pair();
        let data = b"pipe test";
        assert!(a.send(data).is_ok());
        let mut buf = [0u8; 64];
        let n = b.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], data);
    }
}

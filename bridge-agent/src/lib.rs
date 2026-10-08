#![deny(clippy::all, clippy::pedantic, clippy::nursery)]

//! bridge-agent: in-guest client for the Enlil inter-guest bridge.
//!
//! The Enlil hypervisor exposes an inter-guest bridge to each guest as a
//! `VirtIO` bridge device with eight queues (see
//! [`enlil_devices::bridge::transport`]). This crate is the *guest side* of
//! that bridge: it frames clipboard, drag-and-drop, and shared-filesystem
//! payloads (see [`protocol`]) and exchanges them through any
//! [`BridgeTransport`](enlil_devices::bridge::transport::BridgeTransport)
//! implementation via [`AgentClient`].
//!
//! In production the transport is backed by the guest's `VirtIO` bridge
//! device driver, which implements the `BridgeTransport` trait over the
//! device's MMIO/PCI queues. For development, testing, and the binary's
//! `--self-test` mode, [`LocalVirtioTransport`] provides an in-process
//! loopback with the same queue semantics.

pub mod protocol;
pub mod stealth;

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use enlil_devices::bridge::{
    clipboard::ClipboardContent,
    dragdrop::DragPayload,
    transport::{
        BridgeChannel, BridgeMessage, BridgeTransport, GuestId, LocalVirtioTransport, MessageHeader,
    },
};

pub use protocol::{
    SharedFsOp, decode_clipboard, decode_drag_payload, decode_sharedfs_op, encode_clipboard,
    encode_drag_payload, encode_sharedfs_op,
};

/// Errors produced by the bridge agent.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// A channel payload could not be encoded or decoded.
    #[error("bridge protocol error: {0}")]
    Protocol(String),
    /// The underlying [`BridgeTransport`] reported a failure.
    #[error("bridge transport error: {0}")]
    Transport(String),
    /// A local I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// In-guest client for the Enlil inter-guest bridge.
///
/// Wraps a [`BridgeTransport`] and owns this guest's identity plus a
/// monotonically increasing message sequence counter. All sends are
/// non-blocking; all receives poll the transport without blocking.
pub struct AgentClient {
    transport: Arc<dyn BridgeTransport>,
    guest_id: GuestId,
    next_seq: AtomicU64,
}

impl AgentClient {
    /// Create a client bound to `transport` as `guest_id`.
    ///
    /// For development and self-test, pass an [`Arc`] wrapping a
    /// [`LocalVirtioTransport`]; in a real guest, pass the guest's `VirtIO`
    /// bridge-device transport.
    #[must_use]
    pub fn new(transport: Arc<dyn BridgeTransport>, guest_id: u16) -> Self {
        Self {
            transport,
            guest_id: GuestId::new(guest_id),
            next_seq: AtomicU64::new(0),
        }
    }

    /// This guest's bridge identity.
    #[must_use]
    pub const fn guest_id(&self) -> GuestId {
        self.guest_id
    }

    /// Send raw `payload` bytes to guest `dst` on `channel`.
    ///
    /// Returns the sequence number assigned to the message.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Protocol`] if the payload does not fit the
    /// 32-bit frame length field, or [`AgentError::Transport`] if the
    /// transport rejects the message.
    pub fn send_to(
        &self,
        dst: u16,
        channel: BridgeChannel,
        payload: Vec<u8>,
    ) -> Result<u64, AgentError> {
        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            AgentError::Protocol(format!("payload too large: {} bytes", payload.len()))
        })?;
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let header =
            MessageHeader::new(self.guest_id, GuestId::new(dst), channel, payload_len, seq);
        let msg = BridgeMessage::new(header, payload).map_err(AgentError::Protocol)?;
        self.transport.send(msg).map_err(AgentError::Transport)?;
        Ok(seq)
    }

    /// Send clipboard `content` to guest `dst`.
    ///
    /// Returns the sequence number assigned to the message.
    ///
    /// # Errors
    ///
    /// See [`AgentClient::send_to`].
    pub fn send_clipboard(&self, dst: u16, content: &ClipboardContent) -> Result<u64, AgentError> {
        self.send_to(dst, BridgeChannel::Clipboard, encode_clipboard(content))
    }

    /// Send UTF-8 clipboard text to guest `dst`.
    ///
    /// Returns the sequence number assigned to the message.
    ///
    /// # Errors
    ///
    /// See [`AgentClient::send_to`].
    pub fn send_clipboard_text(&self, dst: u16, text: &str) -> Result<u64, AgentError> {
        self.send_clipboard(dst, &ClipboardContent::Text(text.to_string()))
    }

    /// Send a drag-and-drop payload to guest `dst`.
    ///
    /// Returns the sequence number assigned to the message.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Protocol`] if the payload does not encode, or
    /// [`AgentError::Transport`] if the transport rejects the message.
    pub fn send_drag_payload(&self, dst: u16, payload: &DragPayload) -> Result<u64, AgentError> {
        let bytes = encode_drag_payload(payload)?;
        self.send_to(dst, BridgeChannel::DragDrop, bytes)
    }

    /// Send a shared-filesystem operation to guest `dst`.
    ///
    /// Returns the sequence number assigned to the message.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError::Protocol`] if the op does not encode, or
    /// [`AgentError::Transport`] if the transport rejects the message.
    pub fn send_sharedfs_op(&self, dst: u16, op: &SharedFsOp) -> Result<u64, AgentError> {
        let bytes = encode_sharedfs_op(op)?;
        self.send_to(dst, BridgeChannel::SharedFs, bytes)
    }

    /// Receive one pending message, or `None` when no message is queued.
    #[must_use]
    pub fn recv(&self) -> Option<BridgeMessage> {
        self.transport.recv()
    }

    /// Drain every pending message, oldest first.
    #[must_use]
    pub fn drain(&self) -> Vec<BridgeMessage> {
        let mut out = Vec::new();
        while let Some(msg) = self.transport.recv() {
            out.push(msg);
        }
        out
    }
}

/// Build a loopback [`AgentClient`] for `guest_id`.
///
/// The client talks to itself through a [`LocalVirtioTransport`]: what it
/// sends, it can receive back. Used by the binary's `--self-test` mode and by
/// unit tests.
#[must_use]
pub fn loopback_client(guest_id: u16) -> AgentClient {
    AgentClient::new(Arc::new(LocalVirtioTransport::new(4096)), guest_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_clipboard_round_trip() {
        let client = loopback_client(1);
        let seq = client.send_clipboard_text(2, "hello from guest 1").unwrap();
        let msg = client.recv().expect("expected a message");
        assert_eq!(msg.header.channel, BridgeChannel::Clipboard);
        assert_eq!(msg.header.src, GuestId::new(1));
        assert_eq!(msg.header.dst, GuestId::new(2));
        assert_eq!(msg.header.seq, seq);
        let content = decode_clipboard(&msg.payload).unwrap();
        assert_eq!(
            content,
            ClipboardContent::Text("hello from guest 1".to_string())
        );
    }

    #[test]
    fn client_seq_increments() {
        let client = loopback_client(1);
        let first = client.send_clipboard_text(2, "a").unwrap();
        let second = client.send_clipboard_text(2, "b").unwrap();
        assert_eq!(second, first + 1);
    }

    #[test]
    fn client_drag_payload_round_trip() {
        let client = loopback_client(3);
        let payload = DragPayload::new(
            vec!["file:///drop/report.pdf".to_string()],
            vec!["application/pdf".to_string()],
        );
        client.send_drag_payload(4, &payload).unwrap();
        let msg = client.recv().expect("expected a message");
        assert_eq!(msg.header.channel, BridgeChannel::DragDrop);
        let decoded = decode_drag_payload(&msg.payload).unwrap();
        assert_eq!(decoded.file_uris, payload.file_uris);
        assert_eq!(decoded.mime_types, payload.mime_types);
    }

    #[test]
    fn client_sharedfs_put_round_trip() {
        let client = loopback_client(5);
        let op = SharedFsOp::Put {
            name: "staged.bin".to_string(),
            data: vec![1, 2, 3, 4],
        };
        client.send_sharedfs_op(6, &op).unwrap();
        let msg = client.recv().expect("expected a message");
        assert_eq!(msg.header.channel, BridgeChannel::SharedFs);
        let decoded = decode_sharedfs_op(&msg.payload).unwrap();
        assert_eq!(decoded, op);
    }

    #[test]
    fn client_recv_empty_returns_none() {
        let client = loopback_client(1);
        assert!(client.recv().is_none());
        assert!(client.drain().is_empty());
    }

    #[test]
    fn client_drain_returns_all_pending() {
        let client = loopback_client(1);
        client.send_clipboard_text(2, "one").unwrap();
        client.send_clipboard_text(2, "two").unwrap();
        let drained = client.drain();
        assert_eq!(drained.len(), 2);
        assert!(client.recv().is_none());
    }

    #[test]
    fn client_guest_id() {
        let client = loopback_client(42);
        assert_eq!(client.guest_id(), GuestId::new(42));
    }
}

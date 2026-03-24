//! Core transport layer for Enlil Bridge inter-guest communication system.
//!
//! This module implements the message delivery infrastructure for guest-to-guest
//! communication, with async-ready design for Phase 11 mesh networks.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Guest identifier for bridge routing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct GuestId(u16);

impl GuestId {
    /// Create a new GuestId.
    pub fn new(id: u16) -> Self {
        GuestId(id)
    }

    /// Get the numeric ID.
    pub fn id(&self) -> u16 {
        self.0
    }
}

/// Bridge channel types for different communication patterns.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BridgeChannel {
    /// Clipboard data exchange
    Clipboard = 0,
    /// Drag and drop operations
    DragDrop = 1,
    /// Notification delivery
    Notify = 2,
    /// Shared filesystem operations
    SharedFs = 3,
    /// Fast network path
    FastNet = 4,
    /// URL routing
    UrlRoute = 5,
    /// Control transmission
    ControlTx = 6,
    /// Control reception
    ControlRx = 7,
}

impl BridgeChannel {
    /// Get all channel variants.
    pub fn all() -> &'static [BridgeChannel] {
        &[
            BridgeChannel::Clipboard,
            BridgeChannel::DragDrop,
            BridgeChannel::Notify,
            BridgeChannel::SharedFs,
            BridgeChannel::FastNet,
            BridgeChannel::UrlRoute,
            BridgeChannel::ControlTx,
            BridgeChannel::ControlRx,
        ]
    }

    /// Get the queue index for this channel.
    pub fn queue_index(&self) -> usize {
        *self as usize
    }
}

/// Message frame header for bridge transport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageHeader {
    /// Source guest ID
    pub src: GuestId,
    /// Destination guest ID
    pub dst: GuestId,
    /// Channel for this message
    pub channel: BridgeChannel,
    /// Total payload length in bytes
    pub payload_len: u32,
    /// Sequence number for ordering
    pub seq: u64,
    /// Flags (reserved, must be 0)
    pub flags: u8,
}

impl MessageHeader {
    /// Create a new message header.
    pub fn new(
        src: GuestId,
        dst: GuestId,
        channel: BridgeChannel,
        payload_len: u32,
        seq: u64,
    ) -> Self {
        MessageHeader {
            src,
            dst,
            channel,
            payload_len,
            seq,
            flags: 0,
        }
    }

    /// Serialize header to bytes (fixed 32 bytes).
    pub fn to_bytes(&self) -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf[0..2].copy_from_slice(&self.src.0.to_le_bytes());
        buf[2..4].copy_from_slice(&self.dst.0.to_le_bytes());
        buf[4] = self.channel as u8;
        buf[5] = 0; // reserved
        buf[6..10].copy_from_slice(&self.payload_len.to_le_bytes());
        buf[10..18].copy_from_slice(&self.seq.to_le_bytes());
        buf[18] = self.flags;
        buf[19..32].fill(0); // padding
        buf
    }

    /// Deserialize header from bytes.
    pub fn from_bytes(buf: &[u8; 32]) -> Result<Self, String> {
        let src = GuestId(u16::from_le_bytes([buf[0], buf[1]]));
        let dst = GuestId(u16::from_le_bytes([buf[2], buf[3]]));

        let channel = match buf[4] {
            0 => BridgeChannel::Clipboard,
            1 => BridgeChannel::DragDrop,
            2 => BridgeChannel::Notify,
            3 => BridgeChannel::SharedFs,
            4 => BridgeChannel::FastNet,
            5 => BridgeChannel::UrlRoute,
            6 => BridgeChannel::ControlTx,
            7 => BridgeChannel::ControlRx,
            _ => return Err(format!("Invalid channel index: {}", buf[4])),
        };

        let payload_len = u32::from_le_bytes([buf[6], buf[7], buf[8], buf[9]]);
        let seq = u64::from_le_bytes([
            buf[10], buf[11], buf[12], buf[13], buf[14], buf[15], buf[16], buf[17],
        ]);
        let flags = buf[18];

        Ok(MessageHeader {
            src,
            dst,
            channel,
            payload_len,
            seq,
            flags,
        })
    }
}

/// Bridge message frame (header + payload).
#[derive(Clone, Debug)]
pub struct BridgeMessage {
    /// Message header
    pub header: MessageHeader,
    /// Payload data
    pub payload: Vec<u8>,
}

impl BridgeMessage {
    /// Create a new bridge message.
    pub fn new(header: MessageHeader, payload: Vec<u8>) -> Result<Self, String> {
        if header.payload_len as usize != payload.len() {
            return Err(format!(
                "Payload length mismatch: header={}, actual={}",
                header.payload_len,
                payload.len()
            ));
        }
        Ok(BridgeMessage { header, payload })
    }

    /// Get total frame size (header + payload).
    pub fn frame_size(&self) -> usize {
        32 + self.payload.len()
    }

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.frame_size());
        buf.extend_from_slice(&self.header.to_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Deserialize from bytes.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, String> {
        if buf.len() < 32 {
            return Err(format!(
                "Buffer too short for header: {} < 32",
                buf.len()
            ));
        }

        let mut header_buf = [0u8; 32];
        header_buf.copy_from_slice(&buf[0..32]);
        let header = MessageHeader::from_bytes(&header_buf)?;

        let payload_len = header.payload_len as usize;
        if buf.len() < 32 + payload_len {
            return Err(format!(
                "Buffer too short for payload: {} < {}",
                buf.len(),
                32 + payload_len
            ));
        }

        let payload = buf[32..32 + payload_len].to_vec();
        BridgeMessage::new(header, payload)
    }
}

/// Core transport trait for message delivery.
/// Implementations may be synchronous or async-compatible.
pub trait BridgeTransport: Send + Sync {
    /// Send a message through the transport.
    /// Returns true if enqueued successfully.
    fn send(&self, msg: BridgeMessage) -> Result<(), String>;

    /// Receive a message from the transport (non-blocking).
    /// Returns Some(msg) if a message is available, None if queue is empty.
    fn recv(&self) -> Option<BridgeMessage>;

    /// Check if transport is ready to send.
    fn is_ready(&self) -> bool;

    /// Flush any pending messages.
    fn flush(&self) -> Result<(), String>;

    /// Get queue depth for a specific channel.
    fn queue_depth(&self, channel: BridgeChannel) -> usize;
}

/// Local VirtIO-based transport implementation.
/// Implements direct queue dispatch within a single machine.
pub struct LocalVirtioTransport {
    /// 8 queues, one per BridgeChannel
    queues: [Arc<Mutex<VecDeque<BridgeMessage>>>; 8],
    /// Maximum queue depth per channel
    max_queue_depth: usize,
}

impl LocalVirtioTransport {
    /// Create a new LocalVirtioTransport with specified max queue depth.
    pub fn new(max_queue_depth: usize) -> Self {
        LocalVirtioTransport {
            queues: [
                Arc::new(Mutex::new(VecDeque::new())),
                Arc::new(Mutex::new(VecDeque::new())),
                Arc::new(Mutex::new(VecDeque::new())),
                Arc::new(Mutex::new(VecDeque::new())),
                Arc::new(Mutex::new(VecDeque::new())),
                Arc::new(Mutex::new(VecDeque::new())),
                Arc::new(Mutex::new(VecDeque::new())),
                Arc::new(Mutex::new(VecDeque::new())),
            ],
            max_queue_depth,
        }
    }

    /// Get queue for a channel.
    fn get_queue(&self, channel: BridgeChannel) -> &Arc<Mutex<VecDeque<BridgeMessage>>> {
        &self.queues[channel.queue_index()]
    }
}

impl BridgeTransport for LocalVirtioTransport {
    fn send(&self, msg: BridgeMessage) -> Result<(), String> {
        let queue = self.get_queue(msg.header.channel);
        let mut q = queue.lock().map_err(|e| format!("Queue lock poisoned: {}", e))?;

        if q.len() >= self.max_queue_depth {
            return Err("Queue full".to_string());
        }

        q.push_back(msg);
        Ok(())
    }

    fn recv(&self) -> Option<BridgeMessage> {
        // Round-robin across all queues
        for channel in BridgeChannel::all() {
            let queue = self.get_queue(*channel);
            if let Ok(mut q) = queue.lock() {
                if let Some(msg) = q.pop_front() {
                    return Some(msg);
                }
            }
        }
        None
    }

    fn is_ready(&self) -> bool {
        // Transport is ready if at least one queue has space
        for channel in BridgeChannel::all() {
            let queue = self.get_queue(*channel);
            if let Ok(q) = queue.lock() {
                if q.len() < self.max_queue_depth {
                    return true;
                }
            }
        }
        false
    }

    fn flush(&self) -> Result<(), String> {
        // LocalVirtio is synchronous; flush is a no-op
        Ok(())
    }

    fn queue_depth(&self, channel: BridgeChannel) -> usize {
        let queue = self.get_queue(channel);
        queue.lock().map(|q| q.len()).unwrap_or(0)
    }
}

/// VirtIO Bridge Device with 8 queues for guest communication.
pub struct VirtioBridgeDevice {
    transport: Arc<dyn BridgeTransport>,
}

impl VirtioBridgeDevice {
    /// Create a new VirtIO Bridge Device with a transport.
    pub fn new(transport: Arc<dyn BridgeTransport>) -> Self {
        VirtioBridgeDevice { transport }
    }

    /// Send a message via the device.
    pub fn send(&self, msg: BridgeMessage) -> Result<(), String> {
        self.transport.send(msg)
    }

    /// Receive a message from the device.
    pub fn recv(&self) -> Option<BridgeMessage> {
        self.transport.recv()
    }

    /// Get transport reference.
    pub fn transport(&self) -> &Arc<dyn BridgeTransport> {
        &self.transport
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guest_id_creation() {
        let gid = GuestId::new(42);
        assert_eq!(gid.id(), 42);
        assert_eq!(gid, GuestId(42));
    }

    #[test]
    fn test_bridge_channel_queue_index() {
        assert_eq!(BridgeChannel::Clipboard.queue_index(), 0);
        assert_eq!(BridgeChannel::DragDrop.queue_index(), 1);
        assert_eq!(BridgeChannel::Notify.queue_index(), 2);
        assert_eq!(BridgeChannel::SharedFs.queue_index(), 3);
        assert_eq!(BridgeChannel::FastNet.queue_index(), 4);
        assert_eq!(BridgeChannel::UrlRoute.queue_index(), 5);
        assert_eq!(BridgeChannel::ControlTx.queue_index(), 6);
        assert_eq!(BridgeChannel::ControlRx.queue_index(), 7);
    }

    #[test]
    fn test_bridge_channel_all() {
        let all = BridgeChannel::all();
        assert_eq!(all.len(), 8);
    }

    #[test]
    fn test_message_header_serialization() {
        let header = MessageHeader::new(
            GuestId::new(1),
            GuestId::new(2),
            BridgeChannel::Clipboard,
            100,
            0x0102030405060708,
        );

        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), 32);

        let header2 = MessageHeader::from_bytes(&bytes).unwrap();
        assert_eq!(header2.src, GuestId::new(1));
        assert_eq!(header2.dst, GuestId::new(2));
        assert_eq!(header2.channel, BridgeChannel::Clipboard);
        assert_eq!(header2.payload_len, 100);
        assert_eq!(header2.seq, 0x0102030405060708);
    }

    #[test]
    fn test_message_header_invalid_channel() {
        let mut buf = [0u8; 32];
        buf[4] = 255; // Invalid channel
        let result = MessageHeader::from_bytes(&buf);
        assert!(result.is_err());
    }

    #[test]
    fn test_bridge_message_creation() {
        let header = MessageHeader::new(
            GuestId::new(1),
            GuestId::new(2),
            BridgeChannel::Notify,
            10,
            0,
        );
        let payload = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let msg = BridgeMessage::new(header, payload).unwrap();

        assert_eq!(msg.frame_size(), 32 + 10);
        assert_eq!(msg.payload.len(), 10);
    }

    #[test]
    fn test_bridge_message_length_mismatch() {
        let header = MessageHeader::new(
            GuestId::new(1),
            GuestId::new(2),
            BridgeChannel::Notify,
            20,
            0,
        );
        let payload = vec![1, 2, 3, 4, 5];
        let result = BridgeMessage::new(header, payload);
        assert!(result.is_err());
    }

    #[test]
    fn test_bridge_message_serialization() {
        let header = MessageHeader::new(
            GuestId::new(5),
            GuestId::new(10),
            BridgeChannel::FastNet,
            5,
            999,
        );
        let payload = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let msg = BridgeMessage::new(header, payload).unwrap();

        let bytes = msg.to_bytes();
        assert_eq!(bytes.len(), 37);

        let msg2 = BridgeMessage::from_bytes(&bytes).unwrap();
        assert_eq!(msg2.header.src, GuestId::new(5));
        assert_eq!(msg2.header.dst, GuestId::new(10));
        assert_eq!(msg2.payload, vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
    }

    #[test]
    fn test_local_virtio_transport_send_recv() {
        let transport = LocalVirtioTransport::new(10);
        let header = MessageHeader::new(
            GuestId::new(1),
            GuestId::new(2),
            BridgeChannel::Clipboard,
            3,
            0,
        );
        let msg = BridgeMessage::new(header, vec![1, 2, 3]).unwrap();

        assert!(transport.send(msg.clone()).is_ok());
        let received = transport.recv();
        assert!(received.is_some());

        let received_msg = received.unwrap();
        assert_eq!(received_msg.header.src, GuestId::new(1));
        assert_eq!(received_msg.header.dst, GuestId::new(2));
        assert_eq!(received_msg.payload, vec![1, 2, 3]);
    }

    #[test]
    fn test_local_virtio_transport_queue_full() {
        let transport = LocalVirtioTransport::new(2);

        let header1 = MessageHeader::new(
            GuestId::new(1),
            GuestId::new(2),
            BridgeChannel::DragDrop,
            1,
            0,
        );
        let msg1 = BridgeMessage::new(header1, vec![1]).unwrap();
        assert!(transport.send(msg1).is_ok());

        let header2 = MessageHeader::new(
            GuestId::new(1),
            GuestId::new(2),
            BridgeChannel::DragDrop,
            1,
            1,
        );
        let msg2 = BridgeMessage::new(header2, vec![2]).unwrap();
        assert!(transport.send(msg2).is_ok());

        let header3 = MessageHeader::new(
            GuestId::new(1),
            GuestId::new(2),
            BridgeChannel::DragDrop,
            1,
            2,
        );
        let msg3 = BridgeMessage::new(header3, vec![3]).unwrap();
        assert!(transport.send(msg3).is_err());
    }

    #[test]
    fn test_local_virtio_transport_queue_depth() {
        let transport = LocalVirtioTransport::new(10);

        let header = MessageHeader::new(
            GuestId::new(1),
            GuestId::new(2),
            BridgeChannel::Notify,
            1,
            0,
        );
        let msg = BridgeMessage::new(header, vec![42]).unwrap();

        assert_eq!(transport.queue_depth(BridgeChannel::Notify), 0);
        transport.send(msg).unwrap();
        assert_eq!(transport.queue_depth(BridgeChannel::Notify), 1);
        transport.recv();
        assert_eq!(transport.queue_depth(BridgeChannel::Notify), 0);
    }

    #[test]
    fn test_virtio_bridge_device() {
        let transport = Arc::new(LocalVirtioTransport::new(10));
        let device = VirtioBridgeDevice::new(transport);

        let header = MessageHeader::new(
            GuestId::new(3),
            GuestId::new(4),
            BridgeChannel::SharedFs,
            4,
            0,
        );
        let msg = BridgeMessage::new(header, vec![10, 20, 30, 40]).unwrap();

        assert!(device.send(msg).is_ok());
        let received = device.recv();
        assert!(received.is_some());

        let received_msg = received.unwrap();
        assert_eq!(received_msg.header.src, GuestId::new(3));
        assert_eq!(received_msg.header.dst, GuestId::new(4));
        assert_eq!(received_msg.header.channel, BridgeChannel::SharedFs);
    }

    #[test]
    fn test_transport_is_ready() {
        let transport = LocalVirtioTransport::new(2);
        assert!(transport.is_ready());

        let header = MessageHeader::new(
            GuestId::new(1),
            GuestId::new(2),
            BridgeChannel::ControlTx,
            1,
            0,
        );
        let msg1 = BridgeMessage::new(header, vec![1]).unwrap();
        let msg2 = BridgeMessage::new(
            MessageHeader::new(GuestId::new(1), GuestId::new(2), BridgeChannel::ControlTx, 1, 1),
            vec![2],
        )
        .unwrap();

        transport.send(msg1).unwrap();
        transport.send(msg2).unwrap();

        // Queue is full for ControlTx, but other channels have space
        assert!(transport.is_ready());
    }
}

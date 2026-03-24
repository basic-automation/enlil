//! Communication protocol between enlil-core and enlil-mgmt.
//!
//! Messages are length-prefixed JSON: `[4-byte LE length][JSON payload]`.
//! Connection abstraction supports TCP (cross-platform), Unix sockets (Linux),
//! and named pipes (Windows).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Unique identifier for a guest VM.
pub type GuestId = String;

/// Status of a guest VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestState {
    Running,
    Stopped,
    Paused,
}

impl std::fmt::Display for GuestState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuestState::Running => write!(f, "Running"),
            GuestState::Stopped => write!(f, "Stopped"),
            GuestState::Paused => write!(f, "Paused"),
        }
    }
}

/// Snapshot of a single guest's status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestStatus {
    pub id: GuestId,
    pub name: String,
    pub state: GuestState,
    /// CPU usage as a percentage (0.0 – 100.0).
    pub cpu_percent: f64,
    /// Memory used in MiB.
    pub memory_used_mib: u64,
    /// Total memory allocated in MiB.
    pub memory_total_mib: u64,
    /// Uptime in seconds (0 when stopped).
    pub uptime_secs: u64,
}

// ---------------------------------------------------------------------------
// Wire messages
// ---------------------------------------------------------------------------

/// Messages sent from enlil-core → enlil-mgmt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Full status update for all guests.
    StatusUpdate(Vec<GuestStatus>),
    /// Serial output from a guest.
    SerialData { guest_id: GuestId, data: Vec<u8> },
    /// Response to a command.
    CommandResponse { id: u64, success: bool, message: String },
}

/// Messages sent from enlil-mgmt → enlil-core.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Request a status snapshot.
    RequestStatus,
    /// Send serial input to a guest.
    SerialInput { guest_id: GuestId, data: Vec<u8> },
    /// Guest lifecycle command.
    GuestCommand { id: u64, guest_id: GuestId, action: GuestAction },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestAction {
    Start,
    Stop,
    Reboot,
}

impl std::fmt::Display for GuestAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuestAction::Start => write!(f, "Start"),
            GuestAction::Stop => write!(f, "Stop"),
            GuestAction::Reboot => write!(f, "Reboot"),
        }
    }
}

// ---------------------------------------------------------------------------
// Codec – length-prefixed JSON
// ---------------------------------------------------------------------------

/// Encode a message to bytes: `[4-byte LE length][JSON]`.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let json = serde_json::to_vec(msg)?;
    let len = (json.len() as u32).to_le_bytes();
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Streaming decoder that accumulates bytes and yields complete messages.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: VecDeque<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self { buf: VecDeque::new() }
    }

    /// Push raw bytes into the decoder.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend(data);
    }

    /// Try to decode the next complete message. Returns `None` if not enough
    /// data is available yet.
    pub fn decode<T: for<'de> Deserialize<'de>>(&mut self) -> Result<Option<T>, ProtocolError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len_bytes: [u8; 4] = [self.buf[0], self.buf[1], self.buf[2], self.buf[3]];
        let len = u32::from_le_bytes(len_bytes) as usize;

        if len > 16 * 1024 * 1024 {
            return Err(ProtocolError::FrameTooLarge(len));
        }

        if self.buf.len() < 4 + len {
            return Ok(None);
        }

        // Drain the frame.
        let _ = self.buf.drain(..4);
        let json_bytes: Vec<u8> = self.buf.drain(..len).collect();
        let msg = serde_json::from_slice(&json_bytes)?;
        Ok(Some(msg))
    }
}

// ---------------------------------------------------------------------------
// Connection abstraction
// ---------------------------------------------------------------------------

/// A connection to the enlil-core daemon.
pub struct Connection {
    stream: TcpStream,
    decoder: FrameDecoder,
    read_buf: Vec<u8>,
}

impl Connection {
    /// Connect via TCP (works on all platforms).
    pub async fn connect_tcp(addr: &str) -> Result<Self, ProtocolError> {
        let stream = TcpStream::connect(addr).await?;
        Ok(Self {
            stream,
            decoder: FrameDecoder::new(),
            read_buf: vec![0u8; 8192],
        })
    }

    /// Send a client message.
    pub async fn send(&mut self, msg: &ClientMessage) -> Result<(), ProtocolError> {
        let bytes = encode(msg)?;
        self.stream.write_all(&bytes).await?;
        Ok(())
    }

    /// Receive the next server message (blocks until one is available).
    pub async fn recv(&mut self) -> Result<ServerMessage, ProtocolError> {
        loop {
            if let Some(msg) = self.decoder.decode()? {
                return Ok(msg);
            }
            let n = self.stream.read(&mut self.read_buf).await?;
            if n == 0 {
                return Err(ProtocolError::Disconnected);
            }
            self.decoder.push(&self.read_buf[..n]);
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Frame too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("Disconnected")]
    Disconnected,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_server_message() {
        let msg = ServerMessage::StatusUpdate(vec![
            GuestStatus {
                id: "vm-1".into(),
                name: "Ubuntu 24.04".into(),
                state: GuestState::Running,
                cpu_percent: 42.5,
                memory_used_mib: 1024,
                memory_total_mib: 4096,
                uptime_secs: 3600,
            },
            GuestStatus {
                id: "vm-2".into(),
                name: "Windows 11".into(),
                state: GuestState::Stopped,
                cpu_percent: 0.0,
                memory_used_mib: 0,
                memory_total_mib: 8192,
                uptime_secs: 0,
            },
        ]);

        let encoded = encode(&msg).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&encoded);
        let decoded: ServerMessage = decoder.decode().unwrap().unwrap();

        match decoded {
            ServerMessage::StatusUpdate(guests) => {
                assert_eq!(guests.len(), 2);
                assert_eq!(guests[0].id, "vm-1");
                assert_eq!(guests[0].state, GuestState::Running);
                assert_eq!(guests[1].state, GuestState::Stopped);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_client_message() {
        let msg = ClientMessage::GuestCommand {
            id: 42,
            guest_id: "vm-1".into(),
            action: GuestAction::Reboot,
        };
        let encoded = encode(&msg).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&encoded);
        let decoded: ClientMessage = decoder.decode().unwrap().unwrap();

        match decoded {
            ClientMessage::GuestCommand { id, guest_id, action } => {
                assert_eq!(id, 42);
                assert_eq!(guest_id, "vm-1");
                assert_eq!(action, GuestAction::Reboot);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn partial_frame_decoding() {
        let msg = ServerMessage::CommandResponse {
            id: 1,
            success: true,
            message: "ok".into(),
        };
        let encoded = encode(&msg).unwrap();

        let mut decoder = FrameDecoder::new();

        // Feed one byte at a time.
        for (i, &byte) in encoded.iter().enumerate() {
            decoder.push(&[byte]);
            if i < encoded.len() - 1 {
                let result: Option<ServerMessage> = decoder.decode().unwrap();
                assert!(result.is_none(), "should not decode at byte {i}");
            }
        }

        let decoded: ServerMessage = decoder.decode().unwrap().unwrap();
        match decoded {
            ServerMessage::CommandResponse { id, success, message } => {
                assert_eq!(id, 1);
                assert!(success);
                assert_eq!(message, "ok");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn multiple_frames_in_buffer() {
        let m1 = ClientMessage::RequestStatus;
        let m2 = ClientMessage::SerialInput {
            guest_id: "vm-1".into(),
            data: b"hello".to_vec(),
        };

        let mut combined = encode(&m1).unwrap();
        combined.extend_from_slice(&encode(&m2).unwrap());

        let mut decoder = FrameDecoder::new();
        decoder.push(&combined);

        let d1: ClientMessage = decoder.decode().unwrap().unwrap();
        assert!(matches!(d1, ClientMessage::RequestStatus));

        let d2: ClientMessage = decoder.decode().unwrap().unwrap();
        match d2 {
            ClientMessage::SerialInput { guest_id, data } => {
                assert_eq!(guest_id, "vm-1");
                assert_eq!(data, b"hello");
            }
            _ => panic!("wrong variant"),
        }

        let d3: Option<ClientMessage> = decoder.decode().unwrap();
        assert!(d3.is_none());
    }

    #[test]
    fn frame_too_large_rejected() {
        // Craft a header claiming 32 MiB payload.
        let fake_len: u32 = 32 * 1024 * 1024;
        let mut buf = Vec::new();
        buf.extend_from_slice(&fake_len.to_le_bytes());
        buf.extend_from_slice(b"{}");

        let mut decoder = FrameDecoder::new();
        decoder.push(&buf);
        let result: Result<Option<ServerMessage>, _> = decoder.decode();
        assert!(matches!(result, Err(ProtocolError::FrameTooLarge(_))));
    }

    #[test]
    fn serial_data_roundtrip() {
        let msg = ServerMessage::SerialData {
            guest_id: "vm-3".into(),
            data: vec![0x1b, 0x5b, 0x31, 0x6d], // ESC[1m
        };
        let encoded = encode(&msg).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&encoded);
        let decoded: ServerMessage = decoder.decode().unwrap().unwrap();
        match decoded {
            ServerMessage::SerialData { guest_id, data } => {
                assert_eq!(guest_id, "vm-3");
                assert_eq!(data, vec![0x1b, 0x5b, 0x31, 0x6d]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn guest_state_display() {
        assert_eq!(GuestState::Running.to_string(), "Running");
        assert_eq!(GuestState::Stopped.to_string(), "Stopped");
        assert_eq!(GuestState::Paused.to_string(), "Paused");
    }

    #[test]
    fn guest_action_display() {
        assert_eq!(GuestAction::Start.to_string(), "Start");
        assert_eq!(GuestAction::Stop.to_string(), "Stop");
        assert_eq!(GuestAction::Reboot.to_string(), "Reboot");
    }
}

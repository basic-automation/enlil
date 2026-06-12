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
            Self::Running => write!(f, "Running"),
            Self::Stopped => write!(f, "Stopped"),
            Self::Paused => write!(f, "Paused"),
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

/// One physical USB device as the console's USB tab shows it: identity from
/// the host monitor, plus where the routing engine currently places it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbDeviceEntry {
    /// Host bus address (the routing engine's device key).
    pub bus_addr: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub product: Option<String>,
    pub manufacturer: Option<String>,
    pub serial: Option<String>,
    /// Physical port path (e.g. "1-1", "2-3.1").
    pub port_path: Option<String>,
    /// Negotiated speed, display form ("Low", "High", "Super", ...).
    pub speed: String,
    /// Guest currently holding the device (`None` = with the hypervisor).
    pub assigned_guest: Option<GuestId>,
    /// Root-hub port index on that guest's virtual controller.
    pub guest_port: Option<usize>,
}

impl std::fmt::Display for UsbDeviceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04x}:{:04x} {} [{}]",
            self.vendor_id,
            self.product_id,
            self.product.as_deref().unwrap_or("Unknown Device"),
            self.assigned_guest.as_deref().unwrap_or("unassigned"),
        )
    }
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
    CommandResponse {
        id: u64,
        success: bool,
        message: String,
    },
    /// Full USB inventory with current routing assignments (answer to
    /// [`ClientMessage::RequestUsbDevices`], and pushed after changes).
    UsbDeviceList(Vec<UsbDeviceEntry>),
    /// A USB hot-plug or routing change the console surfaces as a
    /// notification. `device` is `None` for a disconnect.
    UsbHotplugNotice {
        message: String,
        device: Option<UsbDeviceEntry>,
    },
}

/// Messages sent from enlil-mgmt → enlil-core.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Request a status snapshot.
    RequestStatus,
    /// Send serial input to a guest.
    SerialInput { guest_id: GuestId, data: Vec<u8> },
    /// Guest lifecycle command.
    GuestCommand {
        id: u64,
        guest_id: GuestId,
        action: GuestAction,
    },
    /// Request the USB inventory.
    RequestUsbDevices,
    /// USB routing command (answered by a `CommandResponse` with the same
    /// `id`, followed by a fresh `UsbDeviceList`).
    UsbCommand { id: u64, action: UsbAction },
}

/// A USB routing action from the console's USB tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsbAction {
    /// Live-move a device to another guest (virtual unplug + replug).
    Reassign { bus_addr: u8, target_guest: GuestId },
    /// Detach a device from its guest (back to the hypervisor).
    Detach { bus_addr: u8 },
}

impl std::fmt::Display for UsbAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reassign {
                bus_addr,
                target_guest,
            } => write!(f, "Reassign device {bus_addr} -> {target_guest}"),
            Self::Detach { bus_addr } => write!(f, "Detach device {bus_addr}"),
        }
    }
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
            Self::Start => write!(f, "Start"),
            Self::Stop => write!(f, "Stop"),
            Self::Reboot => write!(f, "Reboot"),
        }
    }
}

// ---------------------------------------------------------------------------
// Codec – length-prefixed JSON
// ---------------------------------------------------------------------------

/// Encode a message to bytes: `[4-byte LE length][JSON]`.
///
/// # Errors
///
/// Returns [`ProtocolError::Json`] if the message cannot be serialized, or
/// [`ProtocolError::FrameTooLarge`] if it exceeds the 4-byte length prefix's
/// frame limit.
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let json = serde_json::to_vec(msg)?;
    let len = u32::try_from(json.len())
        .map_err(|_| ProtocolError::FrameTooLarge(json.len()))?
        .to_le_bytes();
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
    /// An empty decoder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: VecDeque::new(),
        }
    }

    /// Push raw bytes into the decoder.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend(data);
    }

    /// Try to decode the next complete message. Returns `None` if not enough
    /// data is available yet.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::FrameTooLarge`] if the frame header claims
    /// more than the 16 MiB limit, or [`ProtocolError::Json`] if the payload
    /// is not valid JSON for `T`.
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
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Io`] if the connection cannot be established.
    pub async fn connect_tcp(addr: &str) -> Result<Self, ProtocolError> {
        let stream = TcpStream::connect(addr).await?;
        Ok(Self {
            stream,
            decoder: FrameDecoder::new(),
            read_buf: vec![0u8; 8192],
        })
    }

    /// Send a client message.
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Json`] if the message cannot be serialized,
    /// or [`ProtocolError::Io`] if the write fails.
    pub async fn send(&mut self, msg: &ClientMessage) -> Result<(), ProtocolError> {
        let bytes = encode(msg)?;
        self.stream.write_all(&bytes).await?;
        Ok(())
    }

    /// Receive the next server message (blocks until one is available).
    ///
    /// # Errors
    ///
    /// Returns [`ProtocolError::Disconnected`] when the peer closes the
    /// stream, or any decode/IO error from the incoming frame.
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
        let got: ServerMessage = decoder.decode().unwrap().unwrap();

        match got {
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
        let got: ClientMessage = decoder.decode().unwrap().unwrap();

        match got {
            ClientMessage::GuestCommand {
                id,
                guest_id,
                action,
            } => {
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

        let got: ServerMessage = decoder.decode().unwrap().unwrap();
        match got {
            ServerMessage::CommandResponse {
                id,
                success,
                message,
            } => {
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
        let got: ServerMessage = decoder.decode().unwrap().unwrap();
        match got {
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

    fn mouse_entry() -> UsbDeviceEntry {
        UsbDeviceEntry {
            bus_addr: 3,
            vendor_id: 0x046d,
            product_id: 0xc077,
            product: Some("M105 Mouse".into()),
            manufacturer: Some("Logitech".into()),
            serial: None,
            port_path: Some("1-1".into()),
            speed: "Low".into(),
            assigned_guest: Some("linux1".into()),
            guest_port: Some(0),
        }
    }

    #[test]
    fn roundtrip_usb_device_list() {
        let msg = ServerMessage::UsbDeviceList(vec![
            mouse_entry(),
            UsbDeviceEntry {
                bus_addr: 4,
                assigned_guest: None,
                guest_port: None,
                ..mouse_entry()
            },
        ]);
        let encoded = encode(&msg).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&encoded);
        let got: ServerMessage = decoder.decode().unwrap().unwrap();
        match got {
            ServerMessage::UsbDeviceList(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0], mouse_entry());
                assert_eq!(entries[1].assigned_guest, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_usb_command() {
        let msg = ClientMessage::UsbCommand {
            id: 7,
            action: UsbAction::Reassign {
                bus_addr: 3,
                target_guest: "linux2".into(),
            },
        };
        let encoded = encode(&msg).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&encoded);
        let got: ClientMessage = decoder.decode().unwrap().unwrap();
        match got {
            ClientMessage::UsbCommand { id, action } => {
                assert_eq!(id, 7);
                assert_eq!(
                    action,
                    UsbAction::Reassign {
                        bus_addr: 3,
                        target_guest: "linux2".into()
                    }
                );
            }
            _ => panic!("wrong variant"),
        }

        // RequestUsbDevices is a bare variant like RequestStatus.
        let encoded = encode(&ClientMessage::RequestUsbDevices).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&encoded);
        let got: ClientMessage = decoder.decode().unwrap().unwrap();
        assert!(matches!(got, ClientMessage::RequestUsbDevices));
    }

    #[test]
    fn roundtrip_usb_hotplug_notice() {
        let msg = ServerMessage::UsbHotplugNotice {
            message: "046d:c077 attached to linux1 port 0".into(),
            device: Some(mouse_entry()),
        };
        let encoded = encode(&msg).unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&encoded);
        let got: ServerMessage = decoder.decode().unwrap().unwrap();
        match got {
            ServerMessage::UsbHotplugNotice { message, device } => {
                assert!(message.contains("attached"));
                assert_eq!(device, Some(mouse_entry()));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn usb_display_forms() {
        assert_eq!(mouse_entry().to_string(), "046d:c077 M105 Mouse [linux1]");
        let unassigned = UsbDeviceEntry {
            assigned_guest: None,
            product: None,
            ..mouse_entry()
        };
        assert_eq!(
            unassigned.to_string(),
            "046d:c077 Unknown Device [unassigned]"
        );
        assert_eq!(
            UsbAction::Reassign {
                bus_addr: 3,
                target_guest: "linux2".into()
            }
            .to_string(),
            "Reassign device 3 -> linux2"
        );
        assert_eq!(
            UsbAction::Detach { bus_addr: 3 }.to_string(),
            "Detach device 3"
        );
    }
}

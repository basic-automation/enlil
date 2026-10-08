//! Wire-format encodings for bridge-agent channel payloads.
//!
//! Each [`BridgeChannel`](enlil_devices::bridge::transport::BridgeChannel)
//! carries opaque bytes inside a
//! [`BridgeMessage`](enlil_devices::bridge::transport::BridgeMessage) frame.
//! This module defines the payload layout that the in-guest agent and its
//! host-side peers agree on for the clipboard, drag-and-drop, and shared-fs
//! channels. All multi-byte integers are little-endian.
//!
//! # Clipboard channel
//!
//! `[content_type: u8][data: bytes…]`, where `content_type` is
//! `0 = text`, `1 = image`, `2 = file-ref`, `3 = html`, `4 = rich-text`.
//! Text-like variants carry UTF-8; binary variants carry raw bytes.
//!
//! # Drag-and-drop channel
//!
//! ```text
//! [uri_count: u32][uri: u16 len + bytes]…
//! [mime_count: u32][mime: u16 len + bytes]…
//! [thumbnail: u8]            // 0 = absent, 1 = present
//! [thumb_len: u32][thumb bytes…]   // only when thumbnail == 1
//! ```
//!
//! # Shared-fs channel
//!
//! ```text
//! [opcode: u8]               // 0 = put, 1 = get, 2 = list, 3 = delete
//! [name_len: u16][name: UTF-8 bytes]
//! [data_len: u32][data bytes…]
//! ```
//! `list` carries an empty name and no data; `get`/`delete` carry a name and
//! no data; `put` carries a name and the file bytes.

use enlil_devices::bridge::{clipboard::ClipboardContent, dragdrop::DragPayload};

use super::AgentError;

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

const CLIPBOARD_TEXT: u8 = 0;
const CLIPBOARD_IMAGE: u8 = 1;
const CLIPBOARD_FILEREF: u8 = 2;
const CLIPBOARD_HTML: u8 = 3;
const CLIPBOARD_RICHTEXT: u8 = 4;

/// Encode a [`ClipboardContent`] into clipboard-channel payload bytes.
#[must_use]
pub fn encode_clipboard(content: &ClipboardContent) -> Vec<u8> {
    let (tag, data): (u8, &[u8]) = match content {
        ClipboardContent::Text(s) => (CLIPBOARD_TEXT, s.as_bytes()),
        ClipboardContent::Image(d) => (CLIPBOARD_IMAGE, d),
        ClipboardContent::FileRef(s) => (CLIPBOARD_FILEREF, s.as_bytes()),
        ClipboardContent::Html(s) => (CLIPBOARD_HTML, s.as_bytes()),
        ClipboardContent::RichText(d) => (CLIPBOARD_RICHTEXT, d),
    };
    let mut out = Vec::with_capacity(1 + data.len());
    out.push(tag);
    out.extend_from_slice(data);
    out
}

/// Decode clipboard-channel payload bytes into a [`ClipboardContent`].
///
/// # Errors
///
/// Returns [`AgentError::Protocol`] when the payload is empty, carries an
/// unknown content-type tag, or carries invalid UTF-8 for a text variant.
pub fn decode_clipboard(payload: &[u8]) -> Result<ClipboardContent, AgentError> {
    let (&tag, data) = payload
        .split_first()
        .ok_or_else(|| AgentError::Protocol("empty clipboard payload".to_string()))?;
    match tag {
        CLIPBOARD_TEXT => Ok(ClipboardContent::Text(decode_text(data)?.to_string())),
        CLIPBOARD_IMAGE => Ok(ClipboardContent::Image(data.to_vec())),
        CLIPBOARD_FILEREF => Ok(ClipboardContent::FileRef(decode_text(data)?.to_string())),
        CLIPBOARD_HTML => Ok(ClipboardContent::Html(decode_text(data)?.to_string())),
        CLIPBOARD_RICHTEXT => Ok(ClipboardContent::RichText(data.to_vec())),
        other => Err(AgentError::Protocol(format!(
            "unknown clipboard content type: {other}"
        ))),
    }
}

fn decode_text(data: &[u8]) -> Result<&str, AgentError> {
    std::str::from_utf8(data)
        .map_err(|e| AgentError::Protocol(format!("invalid UTF-8 in clipboard payload: {e}")))
}

// ---------------------------------------------------------------------------
// Drag-and-drop
// ---------------------------------------------------------------------------

/// Encode a [`DragPayload`] into drag-and-drop-channel payload bytes.
///
/// # Errors
///
/// Returns [`AgentError::Protocol`] when a list length or blob length does not
/// fit its wire field.
pub fn encode_drag_payload(payload: &DragPayload) -> Result<Vec<u8>, AgentError> {
    let mut out = Vec::new();
    push_str_list(&mut out, &payload.file_uris)?;
    push_str_list(&mut out, &payload.mime_types)?;
    match &payload.preview_thumbnail {
        Some(thumb) => {
            out.push(1);
            let len = u32::try_from(thumb.len()).map_err(|_| {
                AgentError::Protocol(format!("thumbnail too large: {} bytes", thumb.len()))
            })?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(thumb);
        }
        None => out.push(0),
    }
    Ok(out)
}

fn push_str_list(out: &mut Vec<u8>, items: &[String]) -> Result<(), AgentError> {
    let count = u32::try_from(items.len()).map_err(|_| {
        AgentError::Protocol(format!("too many strings in drag payload: {}", items.len()))
    })?;
    out.extend_from_slice(&count.to_le_bytes());
    for item in items {
        let bytes = item.as_bytes();
        let len = u16::try_from(bytes.len()).map_err(|_| {
            AgentError::Protocol(format!(
                "drag payload string too long: {} bytes",
                bytes.len()
            ))
        })?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(bytes);
    }
    Ok(())
}

/// Decode drag-and-drop-channel payload bytes into a [`DragPayload`].
///
/// # Errors
///
/// Returns [`AgentError::Protocol`] when the payload is truncated, carries an
/// invalid thumbnail flag, is not valid UTF-8 where strings are expected, or
/// has trailing bytes.
pub fn decode_drag_payload(payload: &[u8]) -> Result<DragPayload, AgentError> {
    let mut cursor = Cursor::new(payload);
    let file_uris = cursor.str_list()?;
    let mime_types = cursor.str_list()?;
    let thumb_flag = cursor.u8()?;
    let preview_thumbnail = match thumb_flag {
        0 => None,
        1 => {
            let len = cursor.u32_le()?;
            let len = usize::try_from(len).map_err(|_| {
                AgentError::Protocol(format!("thumbnail length does not fit: {len}"))
            })?;
            Some(cursor.take(len)?.to_vec())
        }
        other => {
            return Err(AgentError::Protocol(format!(
                "invalid drag payload thumbnail flag: {other}"
            )));
        }
    };
    cursor.expect_end("drag payload")?;
    let mut decoded = DragPayload::new(file_uris, mime_types);
    if let Some(thumb) = preview_thumbnail {
        decoded = decoded.with_thumbnail(thumb);
    }
    Ok(decoded)
}

// ---------------------------------------------------------------------------
// Shared filesystem
// ---------------------------------------------------------------------------

const SHAREDFS_PUT: u8 = 0;
const SHAREDFS_GET: u8 = 1;
const SHAREDFS_LIST: u8 = 2;
const SHAREDFS_DELETE: u8 = 3;

/// A shared-filesystem operation carried on the shared-fs channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedFsOp {
    /// Stage file bytes under `name` in the shared area.
    Put { name: String, data: Vec<u8> },
    /// Request the file bytes staged under `name`.
    Get { name: String },
    /// Request a listing of staged names.
    List,
    /// Remove the staged entry `name`.
    Delete { name: String },
}

/// Encode a [`SharedFsOp`] into shared-fs-channel payload bytes.
///
/// # Errors
///
/// Returns [`AgentError::Protocol`] when the name or data length does not fit
/// its wire field.
pub fn encode_sharedfs_op(op: &SharedFsOp) -> Result<Vec<u8>, AgentError> {
    let (tag, name, data): (u8, &str, &[u8]) = match op {
        SharedFsOp::Put { name, data } => (SHAREDFS_PUT, name, data),
        SharedFsOp::Get { name } => (SHAREDFS_GET, name, &[]),
        SharedFsOp::List => (SHAREDFS_LIST, "", &[]),
        SharedFsOp::Delete { name } => (SHAREDFS_DELETE, name, &[]),
    };
    let name_bytes = name.as_bytes();
    let name_len = u16::try_from(name_bytes.len()).map_err(|_| {
        AgentError::Protocol(format!(
            "shared-fs name too long: {} bytes",
            name_bytes.len()
        ))
    })?;
    let data_len = u32::try_from(data.len()).map_err(|_| {
        AgentError::Protocol(format!("shared-fs data too large: {} bytes", data.len()))
    })?;
    let mut out = Vec::with_capacity(1 + 2 + name_bytes.len() + 4 + data.len());
    out.push(tag);
    out.extend_from_slice(&name_len.to_le_bytes());
    out.extend_from_slice(name_bytes);
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(data);
    Ok(out)
}

/// Decode shared-fs-channel payload bytes into a [`SharedFsOp`].
///
/// # Errors
///
/// Returns [`AgentError::Protocol`] when the payload is truncated, carries an
/// unknown opcode, carries an invalid UTF-8 name, violates the per-opcode
/// shape (e.g. data on a `get`), or has trailing bytes.
pub fn decode_sharedfs_op(payload: &[u8]) -> Result<SharedFsOp, AgentError> {
    let mut cursor = Cursor::new(payload);
    let tag = cursor.u8()?;
    let name_len = cursor.u16_le()?;
    let name_bytes = cursor.take(usize::from(name_len))?;
    let name = std::str::from_utf8(name_bytes)
        .map_err(|e| AgentError::Protocol(format!("invalid UTF-8 in shared-fs name: {e}")))?
        .to_string();
    let data_len = cursor.u32_le()?;
    let data_len = usize::try_from(data_len).map_err(|_| {
        AgentError::Protocol(format!("shared-fs data length does not fit: {data_len}"))
    })?;
    let data = cursor.take(data_len)?.to_vec();
    cursor.expect_end("shared-fs payload")?;
    match tag {
        SHAREDFS_PUT => Ok(SharedFsOp::Put { name, data }),
        SHAREDFS_GET if data.is_empty() => Ok(SharedFsOp::Get { name }),
        SHAREDFS_LIST if name.is_empty() && data.is_empty() => Ok(SharedFsOp::List),
        SHAREDFS_DELETE if data.is_empty() => Ok(SharedFsOp::Delete { name }),
        SHAREDFS_GET | SHAREDFS_LIST | SHAREDFS_DELETE => Err(AgentError::Protocol(format!(
            "malformed shared-fs op {tag}: unexpected name/data shape"
        ))),
        other => Err(AgentError::Protocol(format!(
            "unknown shared-fs opcode: {other}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Little-endian cursor
// ---------------------------------------------------------------------------

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], AgentError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| AgentError::Protocol("payload offset overflow".to_string()))?;
        if end > self.buf.len() {
            return Err(AgentError::Protocol(format!(
                "truncated payload: need {end} bytes, have {}",
                self.buf.len()
            )));
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, AgentError> {
        self.take(1).map(|b| b[0])
    }

    fn u16_le(&mut self) -> Result<u16, AgentError> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32_le(&mut self) -> Result<u32, AgentError> {
        self.take(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn str_list(&mut self) -> Result<Vec<String>, AgentError> {
        let count = self.u32_le()?;
        let mut items = Vec::new();
        for _ in 0..count {
            let len = self.u16_le()?;
            let bytes = self.take(usize::from(len))?;
            let item = std::str::from_utf8(bytes)
                .map_err(|e| AgentError::Protocol(format!("invalid UTF-8 in string list: {e}")))?
                .to_string();
            items.push(item);
        }
        Ok(items)
    }

    fn expect_end(&self, what: &str) -> Result<(), AgentError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(AgentError::Protocol(format!(
                "trailing bytes in {what}: {} extra",
                self.buf.len() - self.pos
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_text_round_trip() {
        let content = ClipboardContent::Text("hello, bridge".to_string());
        let decoded = decode_clipboard(&encode_clipboard(&content)).unwrap();
        assert_eq!(decoded, content);
    }

    #[test]
    fn clipboard_all_variants_round_trip() {
        for content in [
            ClipboardContent::Text("t".to_string()),
            ClipboardContent::Image(vec![0x89, 0x50, 0x4E, 0x47]),
            ClipboardContent::FileRef("/tmp/shared/file.txt".to_string()),
            ClipboardContent::Html("<b>hi</b>".to_string()),
            ClipboardContent::RichText(vec![1, 2, 3]),
        ] {
            let decoded = decode_clipboard(&encode_clipboard(&content)).unwrap();
            assert_eq!(decoded, content);
        }
    }

    #[test]
    fn clipboard_empty_payload_errors() {
        assert!(decode_clipboard(&[]).is_err());
    }

    #[test]
    fn clipboard_unknown_tag_errors() {
        assert!(decode_clipboard(&[9, 1, 2, 3]).is_err());
    }

    #[test]
    fn clipboard_invalid_utf8_errors() {
        assert!(decode_clipboard(&[CLIPBOARD_TEXT, 0xFF, 0xFE]).is_err());
        // Binary variants accept arbitrary bytes.
        assert!(decode_clipboard(&[CLIPBOARD_IMAGE, 0xFF, 0xFE]).is_ok());
    }

    #[test]
    fn drag_payload_round_trip_with_thumbnail() {
        let payload = DragPayload::new(
            vec!["file:///home/guest/a.txt".to_string()],
            vec!["text/plain".to_string()],
        )
        .with_thumbnail(vec![9, 8, 7]);
        let decoded = decode_drag_payload(&encode_drag_payload(&payload).unwrap()).unwrap();
        assert_eq!(decoded.file_uris, payload.file_uris);
        assert_eq!(decoded.mime_types, payload.mime_types);
        assert_eq!(decoded.preview_thumbnail, Some(vec![9, 8, 7]));
    }

    #[test]
    fn drag_payload_round_trip_without_thumbnail() {
        let payload = DragPayload::new(vec![], vec!["image/png".to_string()]);
        let decoded = decode_drag_payload(&encode_drag_payload(&payload).unwrap()).unwrap();
        assert_eq!(decoded.file_uris, Vec::<String>::new());
        assert_eq!(decoded.mime_types, vec!["image/png".to_string()]);
        assert!(decoded.preview_thumbnail.is_none());
    }

    #[test]
    fn drag_payload_truncated_errors() {
        let payload = DragPayload::new(
            vec!["file:///a".to_string()],
            vec!["text/plain".to_string()],
        );
        let mut bytes = encode_drag_payload(&payload).unwrap();
        bytes.truncate(bytes.len() - 2);
        assert!(decode_drag_payload(&bytes).is_err());
    }

    #[test]
    fn drag_payload_bad_thumb_flag_errors() {
        let mut bytes = encode_drag_payload(&DragPayload::new(vec![], vec![])).unwrap();
        let last = bytes.len() - 1;
        bytes[last] = 7;
        assert!(decode_drag_payload(&bytes).is_err());
    }

    #[test]
    fn sharedfs_put_round_trip() {
        let op = SharedFsOp::Put {
            name: "notes.txt".to_string(),
            data: b"shared bytes".to_vec(),
        };
        let decoded = decode_sharedfs_op(&encode_sharedfs_op(&op).unwrap()).unwrap();
        assert_eq!(decoded, op);
    }

    #[test]
    fn sharedfs_other_ops_round_trip() {
        for op in [
            SharedFsOp::Get {
                name: "notes.txt".to_string(),
            },
            SharedFsOp::List,
            SharedFsOp::Delete {
                name: "old.bin".to_string(),
            },
        ] {
            let decoded = decode_sharedfs_op(&encode_sharedfs_op(&op).unwrap()).unwrap();
            assert_eq!(decoded, op);
        }
    }

    #[test]
    fn sharedfs_unknown_opcode_errors() {
        assert!(decode_sharedfs_op(&[9, 0, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn sharedfs_get_with_data_errors() {
        let mut bytes = encode_sharedfs_op(&SharedFsOp::Get {
            name: "x".to_string(),
        })
        .unwrap();
        // Append one data byte and fix the data length field (offset 1+2+1=4).
        bytes.extend_from_slice(&[0xAA]);
        let data_len_offset = 1 + 2 + 1;
        bytes[data_len_offset..data_len_offset + 4].copy_from_slice(&1u32.to_le_bytes());
        assert!(decode_sharedfs_op(&bytes).is_err());
    }

    #[test]
    fn sharedfs_trailing_bytes_error() {
        let mut bytes = encode_sharedfs_op(&SharedFsOp::List).unwrap();
        bytes.push(0xFF);
        assert!(decode_sharedfs_op(&bytes).is_err());
    }
}

//! `VirtIO`-GPU basic 2D backend (T-7.3e minimal).
//!
//! Guest-facing `VirtIO`-GPU device (`VirtIO` device ID 16) implementing the 2D
//! command set of the virtio-gpu protocol on a framebuffer backing store:
//!
//! - control queue: `GET_DISPLAY_INFO`, `RESOURCE_CREATE_2D`,
//!   `RESOURCE_UNREF`, `SET_SCANOUT`, `RESOURCE_FLUSH`, `TRANSFER_TO_HOST_2D`,
//!   `RESOURCE_ATTACH_BACKING`, `RESOURCE_DETACH_BACKING`, `GET_CAPSET_INFO`,
//!   `GET_CAPSET`, `GET_EDID`
//! - cursor queue: `CURSOR_SET`, `CURSOR_MOVE` (position tracked; the cursor
//!   is not composited by this backend)
//!
//! 3D/GL contexts (`CTX_*`, `RESOURCE_CREATE_3D`, `SUBMIT_3D`), virglrenderer,
//! Venus, `VirGL` and vDRM native contexts are post-1.0: those commands are
//! decoded and answered with `ERR_INVALID_PARAMETER` so a guest driver gets a
//! clean, spec-shaped refusal instead of silence.
//!
//! Like the other device models in this crate (see `net::virtqueue`), the
//! queue layer is self-contained: guest RAM is not plumbed through on this
//! host, so the driver hands the device a request buffer and a response sink,
//! and `TRANSFER_TO_HOST_2D` pixel data is carried in the tail of the request
//! buffer. The host memory-subsystem wiring will replace this shim without
//! changing the command dispatch or the resource/scanout state machine.
//!
//! On `RESOURCE_FLUSH` the flushed resource's backing store is submitted as a
//! frame to the attached [`VirtioGpuSource`](crate::display::VirtioGpuSource),
//! which feeds the Enlil Zones compositor. Zero-copy texture sharing with the
//! compositor is post-1.0; the submit path copies the frame.

use std::collections::HashMap;
use std::sync::Arc;

use thiserror::Error;

use super::display::{PixelFormat, VirtioGpuSource};
use super::truncate::{Widen, u32_of, usize_of};
use super::virtio::VirtioDeviceType;

/// Device feature bits (`virtio_gpu` feature negotiation).
pub mod features {
    /// 3D virgl rendering offload (not offered by this backend).
    pub const VIRGL: u64 = 1 << 0;
    /// EDID-backed display modes (not offered by this backend).
    pub const EDID: u64 = 1 << 1;
    /// Resource UUID export (not offered by this backend).
    pub const RESOURCE_UUID: u64 = 1 << 2;
    /// Blob resources (not offered by this backend).
    pub const RESOURCE_BLOB: u64 = 1 << 3;
    /// Context init ioctl passthrough (not offered by this backend).
    pub const CONTEXT_INIT: u64 = 1 << 4;
}

/// Control-queue command types (`VIRTIO_GPU_CMD_*`).
pub mod cmd {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x0107;
    pub const GET_CAPSET_INFO: u32 = 0x0108;
    pub const GET_CAPSET: u32 = 0x0109;
    pub const GET_EDID: u32 = 0x010A;
    pub const CTX_CREATE: u32 = 0x0200;
    pub const CTX_DESTROY: u32 = 0x0201;
    pub const CTX_ATTACH_RESOURCE: u32 = 0x0202;
    pub const CTX_DETACH_RESOURCE: u32 = 0x0203;
    pub const RESOURCE_CREATE_3D: u32 = 0x0204;
    pub const TRANSFER_FROM_HOST_3D: u32 = 0x0205;
    pub const SUBMIT_3D: u32 = 0x0207;
    pub const CURSOR_SET: u32 = 0x0300;
    pub const CURSOR_MOVE: u32 = 0x0301;
}

/// Response types (`VIRTIO_GPU_RESP_*`).
pub mod resp {
    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
    pub const OK_CAPSET_INFO: u32 = 0x1102;
    pub const OK_CAPSET: u32 = 0x1103;
    pub const OK_EDID: u32 = 0x1104;
    pub const OK_RESOURCE_UUID: u32 = 0x1105;
    pub const OK_MAP_INFO: u32 = 0x1106;
    pub const ERR_UNSPEC: u32 = 0x1200;
    pub const ERR_OUT_OF_MEMORY: u32 = 0x1201;
    pub const ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
    pub const ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
    pub const ERR_INVALID_CONTEXT_ID: u32 = 0x1204;
    pub const ERR_INVALID_PARAMETER: u32 = 0x1205;
}

/// 2D pixel formats (`virtio_gpu_formats`). All are 32 bits per pixel.
pub mod format {
    pub const B8G8R8A8_UNORM: u32 = 1;
    pub const B8G8R8X8_UNORM: u32 = 2;
    pub const A8R8G8B8_UNORM: u32 = 3;
    pub const X8R8G8B8_UNORM: u32 = 4;
    pub const R8G8B8A8_UNORM: u32 = 67;
    pub const X8B8G8R8_UNORM: u32 = 68;
    pub const A8B8G8R8_UNORM: u32 = 121;
    pub const R8G8B8X8_UNORM: u32 = 134;
}

/// `virtio_gpu_config` register offsets.
pub mod config {
    pub const EVENTS_READ: u64 = 0;
    pub const EVENTS_CLEAR: u64 = 4;
    pub const NUM_SCANOUTS: u64 = 8;
    pub const NUM_CAPSETS: u64 = 12;
    /// `VIRTIO_GPU_EVENT_DISPLAY`: display configuration changed.
    pub const EVENT_DISPLAY: u32 = 1 << 0;
}

/// Size of `virtio_gpu_ctrl_hdr` in bytes.
pub const CTRL_HDR_SIZE: usize = 24;
/// Number of display entries in a `GET_DISPLAY_INFO` response.
pub const MAX_SCANOUTS: u32 = 16;
/// Largest resource dimension this backend accepts (pixels).
pub const MAX_DIMENSION: u32 = 16384;
/// Largest single 2D resource backing store (bytes).
pub const MAX_RESOURCE_BYTES: u64 = 256 * 1024 * 1024;
/// Device-wide guest-visible graphics memory budget (bytes).
pub const DEVICE_MEMORY_BUDGET: u64 = 512 * 1024 * 1024;
/// Bytes per pixel for every format in [`format`].
const BYTES_PER_PIXEL: u64 = 4;

/// Errors from `VirtIO`-GPU command handling.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum GpuError {
    /// The request buffer is shorter than the command needs.
    #[error("request too short: need {need} bytes, have {have}")]
    ShortRequest { need: usize, have: usize },
    /// The command type is not part of the virtio-gpu protocol.
    #[error("unknown command type {0:#06x}")]
    UnknownCommand(u32),
    /// The command names a resource that does not exist.
    #[error("invalid resource id {0}")]
    InvalidResource(u32),
    /// The command names a scanout that does not exist.
    #[error("invalid scanout id {0}")]
    InvalidScanout(u32),
    /// A command parameter is out of range or inconsistent.
    #[error("invalid parameter: {0}")]
    InvalidParameter(&'static str),
    /// The resource has no backing store attached.
    #[error("resource {0} has no backing store attached")]
    NoBacking(u32),
    /// The request exceeds the device graphics-memory budget.
    #[error("out of device memory: need {need} bytes of a {budget}-byte budget")]
    OutOfMemory { need: u64, budget: u64 },
    /// 3D/virgl contexts are post-1.0 and refused by this backend.
    #[error("3D/virgl contexts are post-1.0 and not supported by this backend")]
    ContextsUnsupported,
}

impl GpuError {
    /// The `VIRTIO_GPU_RESP_*` code a guest driver should see for this error.
    #[must_use]
    pub const fn resp_code(self) -> u32 {
        match self {
            Self::ShortRequest { .. } | Self::UnknownCommand(_) => resp::ERR_UNSPEC,
            Self::InvalidResource(_) => resp::ERR_INVALID_RESOURCE_ID,
            Self::InvalidScanout(_) => resp::ERR_INVALID_SCANOUT_ID,
            Self::InvalidParameter(_) | Self::NoBacking(_) | Self::ContextsUnsupported => {
                resp::ERR_INVALID_PARAMETER
            }
            Self::OutOfMemory { .. } => resp::ERR_OUT_OF_MEMORY,
        }
    }
}

/// Read a little-endian `u32` at `off`.
///
/// # Errors
///
/// Returns [`GpuError::ShortRequest`] when fewer than four bytes are available.
fn le_u32(buf: &[u8], off: usize) -> Result<u32, GpuError> {
    let end = off.saturating_add(4);
    let bytes = buf.get(off..end).ok_or(GpuError::ShortRequest {
        need: end,
        have: buf.len(),
    })?;
    match bytes {
        [a, b, c, d] => Ok(u32::from_le_bytes([*a, *b, *c, *d])),
        _ => Err(GpuError::ShortRequest {
            need: end,
            have: buf.len(),
        }),
    }
}

/// `virtio_gpu_ctrl_hdr`: the 24-byte header prefixing every request and response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtrlHeader {
    /// Command or response type.
    pub cmd_type: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

impl CtrlHeader {
    /// A blank header used when the request itself cannot be decoded.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            cmd_type: 0,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            ring_idx: 0,
        }
    }

    /// Decode a header from the front of `buf`.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::ShortRequest`] when `buf` is shorter than
    /// [`CTRL_HDR_SIZE`].
    pub fn decode(buf: &[u8]) -> Result<Self, GpuError> {
        if buf.len() < CTRL_HDR_SIZE {
            return Err(GpuError::ShortRequest {
                need: CTRL_HDR_SIZE,
                have: buf.len(),
            });
        }
        Ok(Self {
            cmd_type: le_u32(buf, 0)?,
            flags: le_u32(buf, 4)?,
            fence_id: {
                let lo = u64::from(le_u32(buf, 8)?);
                let hi = u64::from(le_u32(buf, 12)?);
                lo | (hi << 32)
            },
            ctx_id: le_u32(buf, 16)?,
            ring_idx: buf.get(20).copied().unwrap_or(0),
        })
    }

    /// Append the 24-byte little-endian encoding to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.cmd_type.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.fence_id.to_le_bytes());
        out.extend_from_slice(&self.ctx_id.to_le_bytes());
        out.push(self.ring_idx);
        out.extend_from_slice(&[0u8; 3]);
    }
}

/// Write a response for `req`: header with `resp_type`, `fence_id`/`ctx_id`
/// echoed from the request, then `body`.
fn respond(resp: &mut Vec<u8>, req: &CtrlHeader, resp_type: u32, body: &[u8]) {
    resp.clear();
    CtrlHeader {
        cmd_type: resp_type,
        flags: 0,
        fence_id: req.fence_id,
        ctx_id: req.ctx_id,
        ring_idx: 0,
    }
    .encode(resp);
    resp.extend_from_slice(body);
}

/// `VIRTIO_GPU_CMD_RESOURCE_CREATE_2D` payload (16 bytes after the header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceCreate2D {
    pub resource_id: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

impl ResourceCreate2D {
    /// Decode the payload.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::ShortRequest`] when the payload is too short.
    pub fn decode(buf: &[u8]) -> Result<Self, GpuError> {
        Ok(Self {
            resource_id: le_u32(buf, 0)?,
            format: le_u32(buf, 4)?,
            width: le_u32(buf, 8)?,
            height: le_u32(buf, 12)?,
        })
    }
}

/// `VIRTIO_GPU_CMD_SET_SCANOUT` payload (24 bytes after the header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetScanout {
    pub resource_id: u32,
    pub scanout_id: u32,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
}

impl SetScanout {
    /// Decode the payload.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::ShortRequest`] when the payload is too short.
    pub fn decode(buf: &[u8]) -> Result<Self, GpuError> {
        Ok(Self {
            resource_id: le_u32(buf, 0)?,
            scanout_id: le_u32(buf, 4)?,
            width: le_u32(buf, 8)?,
            height: le_u32(buf, 12)?,
            x: le_u32(buf, 16)?,
            y: le_u32(buf, 20)?,
        })
    }
}

/// `VIRTIO_GPU_CMD_RESOURCE_FLUSH` payload (24 bytes after the header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceFlush {
    pub resource_id: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl ResourceFlush {
    /// Decode the payload.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::ShortRequest`] when the payload is too short.
    pub fn decode(buf: &[u8]) -> Result<Self, GpuError> {
        Ok(Self {
            resource_id: le_u32(buf, 0)?,
            x: le_u32(buf, 8)?,
            y: le_u32(buf, 12)?,
            width: le_u32(buf, 16)?,
            height: le_u32(buf, 20)?,
        })
    }
}

/// `VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D` payload (32 bytes after the header).
///
/// Pixel data (`width * height * 4` bytes) follows the payload in the request
/// buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferToHost2D {
    pub resource_id: u32,
    /// Byte offset into the resource where the transfer lands.
    pub offset: u64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl TransferToHost2D {
    /// Decode the payload.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::ShortRequest`] when the payload is too short.
    pub fn decode(buf: &[u8]) -> Result<Self, GpuError> {
        Ok(Self {
            resource_id: le_u32(buf, 0)?,
            offset: {
                let lo = u64::from(le_u32(buf, 8)?);
                let hi = u64::from(le_u32(buf, 12)?);
                lo | (hi << 32)
            },
            x: le_u32(buf, 16)?,
            y: le_u32(buf, 20)?,
            width: le_u32(buf, 24)?,
            height: le_u32(buf, 28)?,
        })
    }
}

/// `VIRTIO_GPU_CMD_CURSOR_SET` payload (24 bytes after the header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSet {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
    pub resource_id: u32,
    pub hot_x: u32,
    pub hot_y: u32,
}

impl CursorSet {
    /// Decode the payload.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::ShortRequest`] when the payload is too short.
    pub fn decode(buf: &[u8]) -> Result<Self, GpuError> {
        Ok(Self {
            scanout_id: le_u32(buf, 0)?,
            x: le_u32(buf, 4)?,
            y: le_u32(buf, 8)?,
            resource_id: le_u32(buf, 12)?,
            hot_x: le_u32(buf, 16)?,
            hot_y: le_u32(buf, 20)?,
        })
    }
}

/// `VIRTIO_GPU_CMD_CURSOR_MOVE` payload (16 bytes after the header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorMove {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
}

impl CursorMove {
    /// Decode the payload.
    ///
    /// # Errors
    ///
    /// Returns [`GpuError::ShortRequest`] when the payload is too short.
    pub fn decode(buf: &[u8]) -> Result<Self, GpuError> {
        Ok(Self {
            scanout_id: le_u32(buf, 0)?,
            x: le_u32(buf, 4)?,
            y: le_u32(buf, 8)?,
        })
    }
}

/// Check that `format` is one of the 32-bit 2D formats this backend supports.
const fn check_format(format: u32) -> Result<(), GpuError> {
    match format {
        format::B8G8R8A8_UNORM
        | format::B8G8R8X8_UNORM
        | format::A8R8G8B8_UNORM
        | format::X8R8G8B8_UNORM
        | format::R8G8B8A8_UNORM
        | format::X8B8G8R8_UNORM
        | format::A8B8G8R8_UNORM
        | format::R8G8B8X8_UNORM => Ok(()),
        _ => Err(GpuError::InvalidParameter("unsupported 2D pixel format")),
    }
}

/// Map a virtio-gpu 2D format to the compositor's [`PixelFormat`].
///
/// The alpha/X channel is passed through untouched; the compositor treats all
/// of these as 32-bit packed pixels.
const fn pixel_format_of(format: u32) -> Option<PixelFormat> {
    match format {
        format::B8G8R8A8_UNORM | format::B8G8R8X8_UNORM | format::X8B8G8R8_UNORM => {
            Some(PixelFormat::BGRA8888)
        }
        format::A8R8G8B8_UNORM | format::R8G8B8A8_UNORM | format::A8B8G8R8_UNORM => {
            Some(PixelFormat::RGBA8888)
        }
        format::X8R8G8B8_UNORM => Some(PixelFormat::XRGB8888),
        format::R8G8B8X8_UNORM => Some(PixelFormat::RGBA8888),
        _ => None,
    }
}

/// A guest 2D resource: metadata plus an optional host backing store.
#[derive(Debug)]
struct GpuResource {
    format: u32,
    width: u32,
    height: u32,
    /// Bytes reserved against the device memory budget (allocated at attach).
    reserved_bytes: u64,
    backing: Vec<u8>,
    backing_attached: bool,
}

/// One display output driven by a resource.
#[derive(Debug, Clone, Copy)]
struct Scanout {
    id: u32,
    resource_id: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    enabled: bool,
}

/// Cursor state tracked from the cursor queue.
#[derive(Debug, Clone, Copy, Default)]
pub struct CursorState {
    pub scanout_id: u32,
    pub x: u32,
    pub y: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub resource_id: u32,
    pub visible: bool,
}

/// Device counters.
#[derive(Debug, Clone, Default)]
pub struct GpuStats {
    /// Control-queue commands processed (including failed ones).
    pub commands: u64,
    /// Cursor-queue commands processed.
    pub cursor_commands: u64,
    /// 2D resources currently... (cumulative) created.
    pub resources_created: u64,
    /// `TRANSFER_TO_HOST_2D` commands completed.
    pub transfers: u64,
    /// Pixel bytes copied by transfers.
    pub transfer_bytes: u64,
    /// `RESOURCE_FLUSH` commands completed.
    pub flushes: u64,
    /// Commands answered with an error response.
    pub errors: u64,
}

/// `VirtIO`-GPU basic 2D device.
///
/// Owns the resource table and the scanout list, decodes the 2D command set,
/// and submits flushed frames to the attached
/// [`VirtioGpuSource`](crate::display::VirtioGpuSource) for the compositor.
pub struct VirtioGpu2D {
    resources: HashMap<u32, GpuResource>,
    scanouts: Vec<Scanout>,
    preferred_width: u32,
    preferred_height: u32,
    display: Option<Arc<VirtioGpuSource>>,
    events: u32,
    mem_used: u64,
    cursor: CursorState,
    stats: GpuStats,
}

impl VirtioGpu2D {
    /// Create a device whose preferred (initial) mode is `width`×`height`.
    ///
    /// Dimensions are clamped to `1..=MAX_DIMENSION`. Scanout 0 starts
    /// disabled; the guest enables it with `SET_SCANOUT`.
    #[must_use]
    pub fn new(preferred_width: u32, preferred_height: u32) -> Self {
        let width = preferred_width.clamp(1, MAX_DIMENSION);
        let height = preferred_height.clamp(1, MAX_DIMENSION);
        Self {
            resources: HashMap::new(),
            scanouts: vec![Scanout {
                id: 0,
                resource_id: 0,
                x: 0,
                y: 0,
                width,
                height,
                enabled: false,
            }],
            preferred_width: width,
            preferred_height: height,
            display: None,
            events: 0,
            mem_used: 0,
            cursor: CursorState::default(),
            stats: GpuStats::default(),
        }
    }

    /// Attach the compositor framebuffer source that receives flushed frames.
    pub fn attach_display(&mut self, display: Arc<VirtioGpuSource>) {
        self.display = Some(display);
    }

    /// The `VirtIO` device type advertised on the transport.
    #[must_use]
    pub const fn device_type() -> VirtioDeviceType {
        VirtioDeviceType::GPU
    }

    /// Device feature bits. The minimal 2D backend offers none: no virgl, no
    /// EDID, no blob resources, no context init.
    #[must_use]
    pub const fn features() -> u64 {
        0
    }

    /// Number of scanouts in `virtio_gpu_config`.
    #[must_use]
    pub const fn num_scanouts(&self) -> u32 {
        1
    }

    /// Number of 3D capsets in `virtio_gpu_config` (always zero here).
    #[must_use]
    pub const fn num_capsets(&self) -> u32 {
        0
    }

    /// Read the `virtio_gpu_config` space.
    #[must_use]
    pub fn read_config(&self, offset: u64, _size: u8) -> u64 {
        match offset {
            config::EVENTS_READ => u64::from(self.events),
            config::NUM_SCANOUTS => u64::from(self.num_scanouts()),
            config::NUM_CAPSETS => u64::from(self.num_capsets()),
            _ => 0,
        }
    }

    /// Write the `virtio_gpu_config` space (only `EVENTS_CLEAR` is writable).
    pub fn write_config(&mut self, offset: u64, _size: u8, value: u64) {
        if offset == config::EVENTS_CLEAR {
            self.events &= !u32_of(value);
        }
    }

    /// Number of live resources.
    #[must_use]
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Current cursor state.
    #[must_use]
    pub const fn cursor(&self) -> CursorState {
        self.cursor
    }

    /// Device counters.
    #[must_use]
    pub const fn stats(&self) -> &GpuStats {
        &self.stats
    }

    /// Whether scanout `id` is currently enabled.
    #[must_use]
    pub fn scanout_enabled(&self, id: u32) -> bool {
        self.scanouts.iter().any(|s| s.id == id && s.enabled)
    }

    /// Dimensions of resource `id`, if it exists.
    #[must_use]
    pub fn resource_dims(&self, id: u32) -> Option<(u32, u32)> {
        self.resources.get(&id).map(|r| (r.width, r.height))
    }

    /// Backing-store bytes of resource `id`, if attached.
    #[must_use]
    pub fn resource_backing(&self, id: u32) -> Option<&[u8]> {
        self.resources
            .get(&id)
            .filter(|r| r.backing_attached)
            .map(|r| r.backing.as_slice())
    }

    /// Reset the device: drop all resources, disable scanouts, clear events.
    ///
    /// The attached display source and the preferred mode survive a reset.
    pub fn reset(&mut self) {
        let width = self.preferred_width;
        let height = self.preferred_height;
        self.resources.clear();
        self.scanouts = vec![Scanout {
            id: 0,
            resource_id: 0,
            x: 0,
            y: 0,
            width,
            height,
            enabled: false,
        }];
        self.events = 0;
        self.mem_used = 0;
        self.cursor = CursorState::default();
        self.stats = GpuStats::default();
    }

    /// Process one control-queue request.
    ///
    /// `req` is the request buffer (header + payload, plus pixel data for
    /// `TRANSFER_TO_HOST_2D`); the encoded response is written into `resp`.
    /// Malformed or unsupported commands produce a spec-shaped error
    /// response; this function itself does not fail.
    pub fn process_control(&mut self, req: &[u8], resp: &mut Vec<u8>) {
        self.stats.commands += 1;
        if let Err(err) = self.dispatch_control(req, resp) {
            self.stats.errors += 1;
            let hdr = CtrlHeader::decode(req).unwrap_or_else(|_| CtrlHeader::empty());
            respond(resp, &hdr, err.resp_code(), &[]);
        }
    }

    /// Process one cursor-queue request, writing the response into `resp`.
    pub fn process_cursor(&mut self, req: &[u8], resp: &mut Vec<u8>) {
        self.stats.cursor_commands += 1;
        if let Err(err) = self.dispatch_cursor(req, resp) {
            self.stats.errors += 1;
            let hdr = CtrlHeader::decode(req).unwrap_or_else(|_| CtrlHeader::empty());
            respond(resp, &hdr, err.resp_code(), &[]);
        }
    }

    /// Reserve `bytes` against the device memory budget.
    fn reserve(&mut self, bytes: u64) -> Result<(), GpuError> {
        if bytes > MAX_RESOURCE_BYTES {
            return Err(GpuError::OutOfMemory {
                need: bytes,
                budget: MAX_RESOURCE_BYTES,
            });
        }
        let total = self
            .mem_used
            .checked_add(bytes)
            .ok_or(GpuError::OutOfMemory {
                need: bytes,
                budget: DEVICE_MEMORY_BUDGET,
            })?;
        if total > DEVICE_MEMORY_BUDGET {
            return Err(GpuError::OutOfMemory {
                need: bytes,
                budget: DEVICE_MEMORY_BUDGET,
            });
        }
        self.mem_used = total;
        Ok(())
    }

    /// Release a previous reservation.
    const fn release(&mut self, bytes: u64) {
        self.mem_used = self.mem_used.saturating_sub(bytes);
    }

    fn dispatch_control(&mut self, req: &[u8], resp: &mut Vec<u8>) -> Result<(), GpuError> {
        let hdr = CtrlHeader::decode(req)?;
        let payload = req.get(CTRL_HDR_SIZE..).ok_or(GpuError::ShortRequest {
            need: CTRL_HDR_SIZE,
            have: req.len(),
        })?;
        match hdr.cmd_type {
            cmd::GET_DISPLAY_INFO => {
                self.cmd_get_display_info(&hdr, resp);
                Ok(())
            }
            cmd::RESOURCE_CREATE_2D => {
                let c = ResourceCreate2D::decode(payload)?;
                self.cmd_resource_create_2d(&hdr, resp, &c)
            }
            cmd::RESOURCE_UNREF => self.cmd_resource_unref(&hdr, resp, payload),
            cmd::SET_SCANOUT => {
                let c = SetScanout::decode(payload)?;
                self.cmd_set_scanout(&hdr, resp, &c)
            }
            cmd::RESOURCE_FLUSH => {
                let c = ResourceFlush::decode(payload)?;
                self.cmd_resource_flush(&hdr, resp, &c)
            }
            cmd::TRANSFER_TO_HOST_2D => {
                let c = TransferToHost2D::decode(payload)?;
                self.cmd_transfer_to_host_2d(&hdr, resp, req, &c)
            }
            cmd::RESOURCE_ATTACH_BACKING => self.cmd_attach_backing(&hdr, resp, payload),
            cmd::RESOURCE_DETACH_BACKING => self.cmd_detach_backing(&hdr, resp, payload),
            cmd::GET_CAPSET_INFO | cmd::GET_CAPSET => Err(GpuError::InvalidParameter(
                "no 3D capsets in the 2D backend",
            )),
            cmd::GET_EDID => Err(GpuError::InvalidParameter("EDID is not advertised")),
            cmd::CTX_CREATE
            | cmd::CTX_DESTROY
            | cmd::CTX_ATTACH_RESOURCE
            | cmd::CTX_DETACH_RESOURCE
            | cmd::RESOURCE_CREATE_3D
            | cmd::TRANSFER_FROM_HOST_3D
            | cmd::SUBMIT_3D => Err(GpuError::ContextsUnsupported),
            _ => Err(GpuError::UnknownCommand(hdr.cmd_type)),
        }
    }

    fn dispatch_cursor(&mut self, req: &[u8], resp: &mut Vec<u8>) -> Result<(), GpuError> {
        let hdr = CtrlHeader::decode(req)?;
        let payload = req.get(CTRL_HDR_SIZE..).ok_or(GpuError::ShortRequest {
            need: CTRL_HDR_SIZE,
            have: req.len(),
        })?;
        match hdr.cmd_type {
            cmd::CURSOR_SET => {
                let c = CursorSet::decode(payload)?;
                self.cmd_cursor_set(&hdr, resp, &c)
            }
            cmd::CURSOR_MOVE => {
                let c = CursorMove::decode(payload)?;
                self.cmd_cursor_move(&hdr, resp, &c)
            }
            _ => Err(GpuError::UnknownCommand(hdr.cmd_type)),
        }
    }

    fn cmd_get_display_info(&self, hdr: &CtrlHeader, resp: &mut Vec<u8>) {
        let mut body = Vec::with_capacity(usize_of(MAX_SCANOUTS) * 20);
        for id in 0..MAX_SCANOUTS {
            let (x, y, width, height, enabled) = self
                .scanouts
                .iter()
                .find(|s| s.id == id)
                .map_or((0, 0, 0, 0, 0), |s| {
                    (s.x, s.y, s.width, s.height, u32::from(s.enabled))
                });
            body.extend_from_slice(&x.to_le_bytes());
            body.extend_from_slice(&y.to_le_bytes());
            body.extend_from_slice(&width.to_le_bytes());
            body.extend_from_slice(&height.to_le_bytes());
            body.extend_from_slice(&enabled.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes()); // flags
        }
        respond(resp, hdr, resp::OK_DISPLAY_INFO, &body);
    }

    fn cmd_resource_create_2d(
        &mut self,
        hdr: &CtrlHeader,
        resp: &mut Vec<u8>,
        c: &ResourceCreate2D,
    ) -> Result<(), GpuError> {
        check_format(c.format)?;
        if c.resource_id == 0 {
            return Err(GpuError::InvalidParameter("resource id 0 is reserved"));
        }
        if self.resources.contains_key(&c.resource_id) {
            return Err(GpuError::InvalidParameter("resource id already in use"));
        }
        if !(1..=MAX_DIMENSION).contains(&c.width) || !(1..=MAX_DIMENSION).contains(&c.height) {
            return Err(GpuError::InvalidParameter("dimensions out of range"));
        }
        let bytes = u64::from(c.width) * u64::from(c.height) * BYTES_PER_PIXEL;
        self.reserve(bytes)?;
        self.resources.insert(
            c.resource_id,
            GpuResource {
                format: c.format,
                width: c.width,
                height: c.height,
                reserved_bytes: bytes,
                backing: Vec::new(),
                backing_attached: false,
            },
        );
        self.stats.resources_created += 1;
        respond(resp, hdr, resp::OK_NODATA, &[]);
        Ok(())
    }

    fn cmd_resource_unref(
        &mut self,
        hdr: &CtrlHeader,
        resp: &mut Vec<u8>,
        payload: &[u8],
    ) -> Result<(), GpuError> {
        let id = le_u32(payload, 0)?;
        let resource = self
            .resources
            .remove(&id)
            .ok_or(GpuError::InvalidResource(id))?;
        self.release(resource.reserved_bytes);
        for scanout in &mut self.scanouts {
            if scanout.resource_id == id {
                scanout.resource_id = 0;
                scanout.enabled = false;
            }
        }
        self.events |= config::EVENT_DISPLAY;
        respond(resp, hdr, resp::OK_NODATA, &[]);
        Ok(())
    }

    fn cmd_set_scanout(
        &mut self,
        hdr: &CtrlHeader,
        resp: &mut Vec<u8>,
        c: &SetScanout,
    ) -> Result<(), GpuError> {
        if c.resource_id != 0 && !self.resources.contains_key(&c.resource_id) {
            return Err(GpuError::InvalidResource(c.resource_id));
        }
        if c.width == 0 || c.height == 0 {
            return Err(GpuError::InvalidParameter(
                "scanout dimensions must be non-zero",
            ));
        }
        if c.width > MAX_DIMENSION || c.height > MAX_DIMENSION {
            return Err(GpuError::InvalidParameter(
                "scanout dimensions out of range",
            ));
        }
        let scanout = self
            .scanouts
            .iter_mut()
            .find(|s| s.id == c.scanout_id)
            .ok_or(GpuError::InvalidScanout(c.scanout_id))?;
        if c.resource_id == 0 {
            scanout.enabled = false;
            scanout.resource_id = 0;
        } else {
            scanout.resource_id = c.resource_id;
            scanout.x = c.x;
            scanout.y = c.y;
            scanout.width = c.width;
            scanout.height = c.height;
            scanout.enabled = true;
        }
        self.events |= config::EVENT_DISPLAY;
        respond(resp, hdr, resp::OK_NODATA, &[]);
        Ok(())
    }

    fn cmd_resource_flush(
        &mut self,
        hdr: &CtrlHeader,
        resp: &mut Vec<u8>,
        c: &ResourceFlush,
    ) -> Result<(), GpuError> {
        let resource = self
            .resources
            .get(&c.resource_id)
            .ok_or(GpuError::InvalidResource(c.resource_id))?;
        if !resource.backing_attached {
            return Err(GpuError::NoBacking(c.resource_id));
        }
        check_rect_in_resource(c.x, c.y, c.width, c.height, resource.width, resource.height)?;
        if let Some(display) = &self.display
            && pixel_format_of(resource.format).is_some()
        {
            display.submit_frame(resource.width, resource.height, resource.backing.clone());
        }
        self.stats.flushes += 1;
        respond(resp, hdr, resp::OK_NODATA, &[]);
        Ok(())
    }

    fn cmd_transfer_to_host_2d(
        &mut self,
        hdr: &CtrlHeader,
        resp: &mut Vec<u8>,
        req: &[u8],
        t: &TransferToHost2D,
    ) -> Result<(), GpuError> {
        // Pixel data follows the 32-byte payload.
        let data_off = CTRL_HDR_SIZE + 32;
        let row_bytes = u64::from(t.width) * BYTES_PER_PIXEL;
        let total_bytes = row_bytes
            .checked_mul(u64::from(t.height))
            .ok_or(GpuError::InvalidParameter("transfer dimensions overflow"))?;
        let data_end = data_off
            .checked_add(usize_of(total_bytes))
            .ok_or(GpuError::InvalidParameter("transfer size overflow"))?;
        let data = req.get(data_off..data_end).ok_or(GpuError::ShortRequest {
            need: data_end,
            have: req.len(),
        })?;
        let resource = self
            .resources
            .get_mut(&t.resource_id)
            .ok_or(GpuError::InvalidResource(t.resource_id))?;
        if !resource.backing_attached {
            return Err(GpuError::NoBacking(t.resource_id));
        }
        check_rect_in_resource(t.x, t.y, t.width, t.height, resource.width, resource.height)?;
        let stride = u64::from(resource.width) * BYTES_PER_PIXEL;
        let backing_len = resource.backing.len().to_u64();
        for row in 0..u64::from(t.height) {
            let dst_y = u64::from(t.y)
                .checked_add(row)
                .ok_or(GpuError::InvalidParameter("transfer row overflow"))?;
            let dst = t
                .offset
                .checked_add(
                    dst_y
                        .checked_mul(stride)
                        .ok_or(GpuError::InvalidParameter("transfer row overflow"))?,
                )
                .and_then(|v| v.checked_add(u64::from(t.x) * BYTES_PER_PIXEL))
                .ok_or(GpuError::InvalidParameter("transfer offset overflow"))?;
            let dst_end = dst
                .checked_add(row_bytes)
                .ok_or(GpuError::InvalidParameter("transfer offset overflow"))?;
            if dst_end > backing_len {
                return Err(GpuError::InvalidParameter("transfer exceeds backing store"));
            }
            let src = usize_of(row * row_bytes);
            let (d0, d1) = (usize_of(dst), usize_of(dst_end));
            resource.backing[d0..d1].copy_from_slice(&data[src..src + usize_of(row_bytes)]);
        }
        self.stats.transfers += 1;
        self.stats.transfer_bytes += total_bytes;
        respond(resp, hdr, resp::OK_NODATA, &[]);
        Ok(())
    }

    fn cmd_attach_backing(
        &mut self,
        hdr: &CtrlHeader,
        resp: &mut Vec<u8>,
        payload: &[u8],
    ) -> Result<(), GpuError> {
        let id = le_u32(payload, 0)?;
        let resource = self
            .resources
            .get_mut(&id)
            .ok_or(GpuError::InvalidResource(id))?;
        if !resource.backing_attached {
            let bytes = usize_of(resource.reserved_bytes);
            resource.backing = vec![0u8; bytes];
            resource.backing_attached = true;
        }
        respond(resp, hdr, resp::OK_NODATA, &[]);
        Ok(())
    }

    fn cmd_detach_backing(
        &mut self,
        hdr: &CtrlHeader,
        resp: &mut Vec<u8>,
        payload: &[u8],
    ) -> Result<(), GpuError> {
        let id = le_u32(payload, 0)?;
        let resource = self
            .resources
            .get_mut(&id)
            .ok_or(GpuError::InvalidResource(id))?;
        resource.backing = Vec::new();
        resource.backing_attached = false;
        respond(resp, hdr, resp::OK_NODATA, &[]);
        Ok(())
    }

    fn cmd_cursor_set(
        &mut self,
        hdr: &CtrlHeader,
        resp: &mut Vec<u8>,
        c: &CursorSet,
    ) -> Result<(), GpuError> {
        if !self.scanouts.iter().any(|s| s.id == c.scanout_id) {
            return Err(GpuError::InvalidScanout(c.scanout_id));
        }
        self.cursor = CursorState {
            scanout_id: c.scanout_id,
            x: c.x,
            y: c.y,
            hot_x: c.hot_x,
            hot_y: c.hot_y,
            resource_id: c.resource_id,
            visible: c.resource_id != 0,
        };
        respond(resp, hdr, resp::OK_NODATA, &[]);
        Ok(())
    }

    fn cmd_cursor_move(
        &mut self,
        hdr: &CtrlHeader,
        resp: &mut Vec<u8>,
        c: &CursorMove,
    ) -> Result<(), GpuError> {
        if !self.scanouts.iter().any(|s| s.id == c.scanout_id) {
            return Err(GpuError::InvalidScanout(c.scanout_id));
        }
        self.cursor.scanout_id = c.scanout_id;
        self.cursor.x = c.x;
        self.cursor.y = c.y;
        respond(resp, hdr, resp::OK_NODATA, &[]);
        Ok(())
    }
}

/// Check that the `(x, y, w, h)` rectangle lies inside a `rw`×`rh` resource.
fn check_rect_in_resource(
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    rw: u32,
    rh: u32,
) -> Result<(), GpuError> {
    if w == 0 || h == 0 {
        return Err(GpuError::InvalidParameter("rectangle must be non-empty"));
    }
    let x1 = u64::from(x)
        .checked_add(u64::from(w))
        .ok_or(GpuError::InvalidParameter("rectangle overflows"))?;
    let y1 = u64::from(y)
        .checked_add(u64::from(h))
        .ok_or(GpuError::InvalidParameter("rectangle overflows"))?;
    if x1 > u64::from(rw) || y1 > u64::from(rh) {
        return Err(GpuError::InvalidParameter("rectangle outside resource"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::FramebufferSource;

    fn request(cmd_type: u32, payload: &[u8]) -> Vec<u8> {
        let mut req = Vec::new();
        CtrlHeader {
            cmd_type,
            flags: 0,
            fence_id: 0xA5A5,
            ctx_id: 7,
            ring_idx: 0,
        }
        .encode(&mut req);
        req.extend_from_slice(payload);
        req
    }

    fn resp_type(resp: &[u8]) -> u32 {
        CtrlHeader::decode(resp)
            .expect("response must decode")
            .cmd_type
    }

    fn fence_of(resp: &[u8]) -> u64 {
        CtrlHeader::decode(resp)
            .expect("response must decode")
            .fence_id
    }

    fn u32s(vals: &[u32]) -> Vec<u8> {
        let mut v = Vec::new();
        for x in vals {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v
    }

    fn make_device() -> (VirtioGpu2D, Vec<u8>) {
        (VirtioGpu2D::new(1920, 1080), Vec::new())
    }

    #[test]
    fn header_roundtrip() {
        let hdr = CtrlHeader {
            cmd_type: cmd::GET_DISPLAY_INFO,
            flags: 3,
            fence_id: 0xDEAD_BEEF_CAFE,
            ctx_id: 42,
            ring_idx: 1,
        };
        let mut buf = Vec::new();
        hdr.encode(&mut buf);
        assert_eq!(buf.len(), CTRL_HDR_SIZE);
        assert_eq!(CtrlHeader::decode(&buf).unwrap(), hdr);
    }

    #[test]
    fn short_request_is_err_unspec() {
        let (mut dev, mut resp) = make_device();
        dev.process_control(&[1, 2, 3], &mut resp);
        assert_eq!(resp_type(&resp), resp::ERR_UNSPEC);
        assert_eq!(dev.stats().errors, 1);
    }

    #[test]
    fn unknown_command_is_err_unspec() {
        let (mut dev, mut resp) = make_device();
        dev.process_control(&request(0x9999, &[]), &mut resp);
        assert_eq!(resp_type(&resp), resp::ERR_UNSPEC);
    }

    #[test]
    fn get_display_info_reports_preferred_mode() {
        let (mut dev, mut resp) = make_device();
        dev.process_control(&request(cmd::GET_DISPLAY_INFO, &[]), &mut resp);
        assert_eq!(resp_type(&resp), resp::OK_DISPLAY_INFO);
        assert_eq!(fence_of(&resp), 0xA5A5);
        // 24-byte header + 16 displays * 24 bytes (rect + enabled + flags).
        assert_eq!(resp.len(), CTRL_HDR_SIZE + 16 * 24);
        let body = &resp[CTRL_HDR_SIZE..];
        // Display 0: disabled until SET_SCANOUT, but carries the preferred mode.
        assert_eq!(le_u32(body, 8).unwrap(), 1920);
        assert_eq!(le_u32(body, 12).unwrap(), 1080);
        assert_eq!(le_u32(body, 16).unwrap(), 0); // enabled
        // Display 1: fully zeroed.
        assert!(body[24..48].iter().all(|b| *b == 0));
    }

    #[test]
    fn create_rejects_bad_parameters() {
        let (mut dev, mut resp) = make_device();
        // Unknown format.
        dev.process_control(
            &request(cmd::RESOURCE_CREATE_2D, &u32s(&[1, 0xFFFF, 64, 64])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_PARAMETER);
        // Zero width.
        dev.process_control(
            &request(
                cmd::RESOURCE_CREATE_2D,
                &u32s(&[1, format::B8G8R8A8_UNORM, 0, 64]),
            ),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_PARAMETER);
        // Zero resource id.
        dev.process_control(
            &request(
                cmd::RESOURCE_CREATE_2D,
                &u32s(&[0, format::B8G8R8A8_UNORM, 64, 64]),
            ),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_PARAMETER);
        // Dimension over the limit.
        dev.process_control(
            &request(
                cmd::RESOURCE_CREATE_2D,
                &u32s(&[1, format::B8G8R8A8_UNORM, MAX_DIMENSION + 1, 64]),
            ),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_PARAMETER);
        assert_eq!(dev.resource_count(), 0);
    }

    #[test]
    fn create_duplicate_id_rejected() {
        let (mut dev, mut resp) = make_device();
        let payload = u32s(&[5, format::X8R8G8B8_UNORM, 64, 64]);
        dev.process_control(&request(cmd::RESOURCE_CREATE_2D, &payload), &mut resp);
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        dev.process_control(&request(cmd::RESOURCE_CREATE_2D, &payload), &mut resp);
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_PARAMETER);
        assert_eq!(dev.resource_count(), 1);
    }

    #[test]
    fn create_huge_resource_is_out_of_memory() {
        let (mut dev, mut resp) = make_device();
        // 16384x16384x4 = 1 GiB > 256 MiB per-resource cap.
        dev.process_control(
            &request(
                cmd::RESOURCE_CREATE_2D,
                &u32s(&[9, format::B8G8R8A8_UNORM, MAX_DIMENSION, MAX_DIMENSION]),
            ),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::ERR_OUT_OF_MEMORY);
        assert_eq!(dev.resource_count(), 0);
    }

    #[test]
    fn full_2d_pipeline_submits_frame() {
        let (mut dev, mut resp) = make_device();
        let display = Arc::new(VirtioGpuSource::new(PixelFormat::BGRA8888, 4));
        dev.attach_display(Arc::clone(&display));

        // CREATE_2D: id 1, B8G8R8A8, 4x2.
        dev.process_control(
            &request(
                cmd::RESOURCE_CREATE_2D,
                &u32s(&[1, format::B8G8R8A8_UNORM, 4, 2]),
            ),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        assert_eq!(dev.resource_dims(1), Some((4, 2)));

        // ATTACH_BACKING.
        dev.process_control(
            &request(cmd::RESOURCE_ATTACH_BACKING, &u32s(&[1, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);

        // TRANSFER_TO_HOST_2D: 4x2 pixels of 0x11_22_33_44.
        // Payload: resource_id, padding, offset u64, x, y, w, h.
        let mut xfer = u32s(&[1, 0, 0, 0, 0, 0, 4, 2]);
        let pixel = [0x44u8, 0x33, 0x22, 0x11];
        let mut pixdata = Vec::with_capacity(32);
        for _ in 0..8 {
            pixdata.extend_from_slice(&pixel);
        }
        xfer.extend_from_slice(&pixdata);
        dev.process_control(&request(cmd::TRANSFER_TO_HOST_2D, &xfer), &mut resp);
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        assert_eq!(dev.stats().transfers, 1);
        assert_eq!(dev.stats().transfer_bytes, 32);

        // SET_SCANOUT: bind resource 1 to scanout 0.
        dev.process_control(
            &request(cmd::SET_SCANOUT, &u32s(&[1, 0, 4, 2, 0, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        assert!(dev.scanout_enabled(0));
        // Display-change event raised.
        assert_eq!(
            dev.read_config(config::EVENTS_READ, 4),
            u64::from(config::EVENT_DISPLAY)
        );
        dev.write_config(config::EVENTS_CLEAR, 4, u64::from(config::EVENT_DISPLAY));
        assert_eq!(dev.read_config(config::EVENTS_READ, 4), 0);

        // FLUSH submits the frame to the compositor source.
        dev.process_control(
            &request(cmd::RESOURCE_FLUSH, &u32s(&[1, 0, 0, 0, 4, 2])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        assert_eq!(dev.stats().flushes, 1);
        let frame = display.current_frame().expect("flush must submit a frame");
        assert_eq!((frame.width, frame.height), (4, 2));
        let data = display
            .get_frame_data(frame.id)
            .expect("frame data present");
        assert_eq!(data, pixdata);

        // DETACH_BACKING + UNREF tear everything down.
        dev.process_control(
            &request(cmd::RESOURCE_DETACH_BACKING, &u32s(&[1, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        assert!(dev.resource_backing(1).is_none());
        dev.process_control(&request(cmd::RESOURCE_UNREF, &u32s(&[1, 0])), &mut resp);
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        assert_eq!(dev.resource_count(), 0);
        assert!(!dev.scanout_enabled(0));
    }

    #[test]
    fn transfer_partial_rect_with_offset() {
        let (mut dev, mut resp) = make_device();
        dev.process_control(
            &request(
                cmd::RESOURCE_CREATE_2D,
                &u32s(&[2, format::R8G8B8A8_UNORM, 4, 4]),
            ),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        dev.process_control(
            &request(cmd::RESOURCE_ATTACH_BACKING, &u32s(&[2, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);

        // Write a 2x2 block at (1,1) with byte offset 0.
        let mut xfer = Vec::new();
        xfer.extend_from_slice(&2u32.to_le_bytes()); // resource_id
        xfer.extend_from_slice(&0u32.to_le_bytes()); // padding
        xfer.extend_from_slice(&0u64.to_le_bytes()); // offset
        xfer.extend_from_slice(&1u32.to_le_bytes()); // x
        xfer.extend_from_slice(&1u32.to_le_bytes()); // y
        xfer.extend_from_slice(&2u32.to_le_bytes()); // w
        xfer.extend_from_slice(&2u32.to_le_bytes()); // h
        xfer.extend_from_slice(&[0xAA; 16]);
        dev.process_control(&request(cmd::TRANSFER_TO_HOST_2D, &xfer), &mut resp);
        assert_eq!(resp_type(&resp), resp::OK_NODATA);

        let backing = dev.resource_backing(2).expect("backing attached").to_vec();
        // Row 1 (bytes 16..32): columns 1..3 are 0xAA.
        assert_eq!(&backing[16..20], &[0, 0, 0, 0]);
        assert_eq!(&backing[20..28], &[0xAA; 8]);
        assert_eq!(&backing[28..32], &[0, 0, 0, 0]);
        // Row 0 untouched.
        assert!(backing[0..16].iter().all(|b| *b == 0));
    }

    #[test]
    fn transfer_rejects_out_of_bounds_and_missing_backing() {
        let (mut dev, mut resp) = make_device();
        dev.process_control(
            &request(
                cmd::RESOURCE_CREATE_2D,
                &u32s(&[3, format::B8G8R8A8_UNORM, 4, 4]),
            ),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);

        // No backing attached yet.
        let xfer = [
            u32s(&[3, 0]),
            0u64.to_le_bytes().to_vec(),
            u32s(&[0, 0, 4, 4]),
        ]
        .concat();
        let mut full = xfer;
        full.extend_from_slice(&[0u8; 64]);
        dev.process_control(&request(cmd::TRANSFER_TO_HOST_2D, &full), &mut resp);
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_PARAMETER);

        dev.process_control(
            &request(cmd::RESOURCE_ATTACH_BACKING, &u32s(&[3, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);

        // Rectangle extends past the resource.
        let mut oob = [
            u32s(&[3, 0]),
            0u64.to_le_bytes().to_vec(),
            u32s(&[3, 3, 4, 4]),
        ]
        .concat();
        oob.extend_from_slice(&[0u8; 64]);
        dev.process_control(&request(cmd::TRANSFER_TO_HOST_2D, &oob), &mut resp);
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_PARAMETER);

        // Truncated pixel data.
        let short = [
            u32s(&[3, 0]),
            0u64.to_le_bytes().to_vec(),
            u32s(&[0, 0, 4, 4]),
        ]
        .concat();
        dev.process_control(&request(cmd::TRANSFER_TO_HOST_2D, &short), &mut resp);
        assert_eq!(resp_type(&resp), resp::ERR_UNSPEC); // ShortRequest
    }

    #[test]
    fn flush_unknown_resource_errors() {
        let (mut dev, mut resp) = make_device();
        dev.process_control(
            &request(cmd::RESOURCE_FLUSH, &u32s(&[77, 0, 0, 0, 4, 4])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_RESOURCE_ID);
    }

    #[test]
    fn set_scanout_validates_ids() {
        let (mut dev, mut resp) = make_device();
        // Unknown resource.
        dev.process_control(
            &request(cmd::SET_SCANOUT, &u32s(&[9, 0, 4, 4, 0, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_RESOURCE_ID);
        // Unknown scanout.
        dev.process_control(
            &request(
                cmd::RESOURCE_CREATE_2D,
                &u32s(&[4, format::B8G8R8A8_UNORM, 4, 4]),
            ),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        dev.process_control(
            &request(cmd::SET_SCANOUT, &u32s(&[4, 3, 4, 4, 0, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_SCANOUT_ID);
        // Resource id 0 disables the scanout.
        dev.process_control(
            &request(cmd::SET_SCANOUT, &u32s(&[0, 0, 4, 4, 0, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        assert!(!dev.scanout_enabled(0));
    }

    #[test]
    fn capset_edid_and_3d_are_cleanly_refused() {
        let (mut dev, mut resp) = make_device();
        for cmd_type in [
            cmd::GET_CAPSET_INFO,
            cmd::GET_CAPSET,
            cmd::GET_EDID,
            cmd::CTX_CREATE,
            cmd::CTX_DESTROY,
            cmd::CTX_ATTACH_RESOURCE,
            cmd::CTX_DETACH_RESOURCE,
            cmd::RESOURCE_CREATE_3D,
            cmd::TRANSFER_FROM_HOST_3D,
            cmd::SUBMIT_3D,
        ] {
            dev.process_control(&request(cmd_type, &u32s(&[0, 0, 0, 0])), &mut resp);
            assert_eq!(
                resp_type(&resp),
                resp::ERR_INVALID_PARAMETER,
                "cmd {cmd_type:#x}"
            );
        }
        assert_eq!(dev.stats().errors, 10);
    }

    #[test]
    fn cursor_set_and_move_tracked() {
        let (mut dev, mut resp) = make_device();
        dev.process_cursor(
            &request(cmd::CURSOR_SET, &u32s(&[0, 100, 200, 7, 3, 4])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        let cursor = dev.cursor();
        assert_eq!((cursor.scanout_id, cursor.x, cursor.y), (0, 100, 200));
        assert_eq!((cursor.hot_x, cursor.hot_y), (3, 4));
        assert_eq!(cursor.resource_id, 7);
        assert!(cursor.visible);

        dev.process_cursor(
            &request(cmd::CURSOR_MOVE, &u32s(&[0, 150, 250, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        let cursor = dev.cursor();
        assert_eq!((cursor.x, cursor.y), (150, 250));

        // Hiding the cursor.
        dev.process_cursor(
            &request(cmd::CURSOR_SET, &u32s(&[0, 0, 0, 0, 0, 0])),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        assert!(!dev.cursor().visible);

        // Unknown scanout rejected.
        dev.process_cursor(&request(cmd::CURSOR_MOVE, &u32s(&[5, 0, 0, 0])), &mut resp);
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_SCANOUT_ID);
        // Unknown cursor command rejected.
        dev.process_cursor(&request(0x03FF, &[]), &mut resp);
        assert_eq!(resp_type(&resp), resp::ERR_UNSPEC);
    }

    #[test]
    fn config_space_reports_topology() {
        let (dev, _) = make_device();
        assert_eq!(dev.read_config(config::EVENTS_READ, 4), 0);
        assert_eq!(dev.read_config(config::NUM_SCANOUTS, 4), 1);
        assert_eq!(dev.read_config(config::NUM_CAPSETS, 4), 0);
        assert_eq!(dev.read_config(0x40, 4), 0);
        assert_eq!(VirtioGpu2D::device_type(), VirtioDeviceType::GPU);
        assert_eq!(VirtioGpu2D::features(), 0);
    }

    #[test]
    fn reset_clears_resources_and_scanouts() {
        let (mut dev, mut resp) = make_device();
        dev.process_control(
            &request(
                cmd::RESOURCE_CREATE_2D,
                &u32s(&[1, format::B8G8R8A8_UNORM, 8, 8]),
            ),
            &mut resp,
        );
        assert_eq!(resp_type(&resp), resp::OK_NODATA);
        dev.process_control(
            &request(cmd::SET_SCANOUT, &u32s(&[1, 0, 8, 8, 0, 0])),
            &mut resp,
        );
        assert!(dev.scanout_enabled(0));
        dev.reset();
        assert_eq!(dev.resource_count(), 0);
        assert!(!dev.scanout_enabled(0));
        assert_eq!(dev.read_config(config::EVENTS_READ, 4), 0);
        assert_eq!(dev.stats().commands, 0);
    }

    #[test]
    fn unref_unknown_resource_errors() {
        let (mut dev, mut resp) = make_device();
        dev.process_control(&request(cmd::RESOURCE_UNREF, &u32s(&[123, 0])), &mut resp);
        assert_eq!(resp_type(&resp), resp::ERR_INVALID_RESOURCE_ID);
    }

    #[test]
    fn error_codes_map_to_spec_responses() {
        assert_eq!(GpuError::UnknownCommand(0).resp_code(), resp::ERR_UNSPEC);
        assert_eq!(
            GpuError::ShortRequest { need: 1, have: 0 }.resp_code(),
            resp::ERR_UNSPEC
        );
        assert_eq!(
            GpuError::InvalidResource(1).resp_code(),
            resp::ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            GpuError::InvalidScanout(2).resp_code(),
            resp::ERR_INVALID_SCANOUT_ID
        );
        assert_eq!(
            GpuError::InvalidParameter("x").resp_code(),
            resp::ERR_INVALID_PARAMETER
        );
        assert_eq!(
            GpuError::NoBacking(1).resp_code(),
            resp::ERR_INVALID_PARAMETER
        );
        assert_eq!(
            GpuError::ContextsUnsupported.resp_code(),
            resp::ERR_INVALID_PARAMETER
        );
        assert_eq!(
            GpuError::OutOfMemory { need: 1, budget: 1 }.resp_code(),
            resp::ERR_OUT_OF_MEMORY
        );
    }
}

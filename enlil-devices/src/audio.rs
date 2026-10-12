//! `VirtIO`-sound device emulation and the hypervisor audio mixing engine.
//!
//! Phase 8.2 (minimal): presents a VirtIO-sound device (spec 1.2+, device ID
//! 25) to each guest with PCM stream queues, and mixes every guest's PCM in
//! the hypervisor with sample-rate conversion into one physical output.
//!
//! Layout:
//! - Protocol constants and little-endian config/info structs for the
//!   VirtIO-sound control queue (`SND_R_*` / `SND_S_*`).
//! - [`VirtioSoundDevice`]: per-guest device state — config space, PCM
//!   streams (set-params/prepare/start/stop state machine), jack/channel-map
//!   info, TX PCM decode, RX capture buffers, and the event queue.
//! - [`AudioMixer`]: the hypervisor mixing engine. Integer-only linear
//!   resampling (Q16 fixed point), per-source volume/mute, clipping mixer,
//!   one period (10 ms at 48 kHz) per [`AudioMixer::tick`].
//! - [`AudioBackend`]: pluggable physical output. [`NullAudioBackend`]
//!   discards frames on headless hosts; real ALSA/PipeWire sinks are a
//!   follow-up.
//! - [`AudioEngine`]: owns the mixer plus every guest device and wires TX
//!   PCM into mixer sources and host capture into RX streams.
//!
//! Out of scope for 1.0 (1.1): microphone routing/hotkey switching,
//! full IOMMU audio-device passthrough, and per-zone audio focus.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use thiserror::Error;

// ---------------------------------------------------------------------------
// VirtIO-sound protocol constants (virtio spec 1.2, section 5.14)
// ---------------------------------------------------------------------------

/// `VirtIO` device ID for sound (spec 1.2, section 5.14).
pub const VIRTIO_SND_DEVICE_ID: u32 = 25;

/// Number of virtqueues: control, event, TX, RX.
pub const VIRTIO_SND_NUM_QUEUES: usize = 4;
/// Control queue index: guest requests, device responses.
pub const VIRTIO_SND_QUEUE_CONTROL: usize = 0;
/// Event queue index: device-to-guest notifications.
pub const VIRTIO_SND_QUEUE_EVENT: usize = 1;
/// TX queue index: guest PCM playback data.
pub const VIRTIO_SND_QUEUE_TX: usize = 2;
/// RX queue index: guest PCM capture data.
pub const VIRTIO_SND_QUEUE_RX: usize = 3;

/// Control request: query jack information.
pub const SND_R_JACK_INFO: u32 = 1;
/// Control request: remap a jack (unsupported in 1.0).
pub const SND_R_JACK_REMAP: u32 = 2;
/// Control request: query PCM stream information.
pub const SND_R_PCM_INFO: u32 = 0x0100;
/// Control request: set PCM stream parameters.
pub const SND_R_PCM_SET_PARAMS: u32 = 0x0101;
/// Control request: prepare a PCM stream.
pub const SND_R_PCM_PREPARE: u32 = 0x0102;
/// Control request: release a PCM stream.
pub const SND_R_PCM_RELEASE: u32 = 0x0103;
/// Control request: start a PCM stream.
pub const SND_R_PCM_START: u32 = 0x0104;
/// Control request: stop a PCM stream.
pub const SND_R_PCM_STOP: u32 = 0x0105;
/// Control request: pause a PCM stream.
pub const SND_R_PCM_PAUSE: u32 = 0x0106;
/// Control request: unpause a PCM stream.
pub const SND_R_PCM_UNPAUSE: u32 = 0x0107;
/// Control request: PCM overrun/underrun notification (device-to-guest only).
pub const SND_R_PCM_XRUN: u32 = 0x0108;
/// Control request: query channel-map information.
pub const SND_R_CHMAP_INFO: u32 = 0x0200;

/// Control status: request completed successfully.
pub const SND_S_OK: u32 = 0x8000;
/// Control status: malformed request.
pub const SND_S_BAD_MSG: u32 = 0x8001;
/// Control status: request not supported.
pub const SND_S_NOT_SUPP: u32 = 0x8002;
/// Control status: I/O error (e.g. invalid stream state transition).
pub const SND_S_IO_ERR: u32 = 0x8003;

/// Event: jack plugged/unplugged.
pub const SND_EVT_JACK: u32 = 0x1000;
/// Event: PCM stream state notification (start/stop/xrun).
pub const SND_EVT_PCM: u32 = 0x1001;

/// PCM stream direction: playback (guest to host).
pub const SND_D_OUTPUT: u8 = 0;
/// PCM stream direction: capture (host to guest).
pub const SND_D_INPUT: u8 = 1;

// PCM format indices (`SND_D_PCM_FMT_*`); the formats bitmask is `1 << index`.
/// IMA ADPCM (advertised only if a codec backend exists; not decoded here).
pub const SND_FMT_IMA_ADPCM: u8 = 0;
/// mu-law (not decoded in 1.0).
pub const SND_FMT_MU_LAW: u8 = 1;
/// A-law (not decoded in 1.0).
pub const SND_FMT_A_LAW: u8 = 2;
/// Signed 8-bit.
pub const SND_FMT_S8: u8 = 3;
/// Unsigned 8-bit.
pub const SND_FMT_U8: u8 = 4;
/// Signed 16-bit little-endian.
pub const SND_FMT_S16_LE: u8 = 5;
/// Signed 16-bit big-endian.
pub const SND_FMT_S16_BE: u8 = 6;
/// Unsigned 16-bit little-endian.
pub const SND_FMT_U16_LE: u8 = 7;
/// Unsigned 16-bit big-endian.
pub const SND_FMT_U16_BE: u8 = 8;
/// Signed 24-bit little-endian (packed in 3 bytes).
pub const SND_FMT_S24_LE: u8 = 9;
/// Signed 24-bit big-endian (packed in 3 bytes).
pub const SND_FMT_S24_BE: u8 = 10;
/// Unsigned 24-bit little-endian (packed in 3 bytes).
pub const SND_FMT_U24_LE: u8 = 11;
/// Unsigned 24-bit big-endian (packed in 3 bytes).
pub const SND_FMT_U24_BE: u8 = 12;
/// Signed 32-bit little-endian.
pub const SND_FMT_S32_LE: u8 = 13;
/// Signed 32-bit big-endian.
pub const SND_FMT_S32_BE: u8 = 14;
/// Unsigned 32-bit little-endian.
pub const SND_FMT_U32_LE: u8 = 15;
/// Unsigned 32-bit big-endian.
pub const SND_FMT_U32_BE: u8 = 16;
/// 32-bit float little-endian.
pub const SND_FMT_FLOAT_LE: u8 = 17;
/// 32-bit float big-endian.
pub const SND_FMT_FLOAT_BE: u8 = 18;
/// 64-bit float little-endian.
pub const SND_FMT_FLOAT64_LE: u8 = 19;
/// 64-bit float big-endian.
pub const SND_FMT_FLOAT64_BE: u8 = 20;

/// PCM sample-rate table indexed by `SND_D_PCM_RATE_*` (`rates` bitmask bit).
pub const SND_RATES_HZ: [u32; 15] = [
    5512, 8000, 11_025, 16_000, 22_050, 32_000, 44_100, 48_000, 64_000, 88_200, 96_000, 176_400,
    192_000, 352_800, 384_000,
];

/// Default mixer output rate: 48 kHz.
pub const MIXER_OUTPUT_RATE_HZ: u32 = 48_000;
/// Default mixer output channels: stereo.
pub const MIXER_OUTPUT_CHANNELS: u8 = 2;
/// Default mixer period: 480 frames at 48 kHz = 10 ms of audio per tick.
pub const MIXER_PERIOD_FRAMES: usize = 480;

// Channel-map positions (`SND_D_CHMAP_*`).
/// Front left.
pub const CHMAP_FL: u8 = 3;
/// Front right.
pub const CHMAP_FR: u8 = 4;
/// Maximum entries in a channel map.
pub const CHMAP_MAX_SIZE: usize = 18;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Audio subsystem errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AudioError {
    /// No device with this id is attached to the engine.
    #[error("unknown audio device id {0}")]
    UnknownDevice(u32),
    /// No stream with this id exists on the device.
    #[error("unknown PCM stream id {0}")]
    UnknownStream(u32),
    /// Operation requires an output (playback) stream.
    #[error("stream {0} is not an output stream")]
    NotOutputStream(u32),
    /// Operation requires an input (capture) stream.
    #[error("stream {0} is not an input stream")]
    NotInputStream(u32),
    /// TX submitted while the stream is not running.
    #[error("stream {0} is not running")]
    StreamNotRunning(u32),
    /// Malformed control or PCM payload.
    #[error("malformed audio payload: {0}")]
    MalformedPayload(&'static str),
    /// PCM format index is not in the stream's advertised formats mask.
    #[error("unsupported PCM format index {0}")]
    UnsupportedFormat(u8),
    /// Sample-rate index is not in the stream's advertised rates mask.
    #[error("unsupported sample-rate index {0}")]
    UnsupportedRate(u8),
    /// Channel count outside the stream's `[channels_min, channels_max]`.
    #[error("channel count {0} out of range")]
    BadChannels(u8),
    /// No mixer source with this id.
    #[error("unknown mixer source {0}")]
    UnknownSource(u64),
    /// Backend refused the frame write.
    #[error("audio backend write failed")]
    BackendWrite,
}

// ---------------------------------------------------------------------------
// Little-endian protocol structs
// ---------------------------------------------------------------------------

/// PCM stream direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PcmDirection {
    /// Playback: guest produces PCM, hypervisor consumes.
    Output = SND_D_OUTPUT,
    /// Capture: hypervisor produces PCM, guest consumes.
    Input = SND_D_INPUT,
}

impl PcmDirection {
    /// Wire value of the direction.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// `virtio_snd_config`: the device configuration space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SndConfig {
    /// Number of jacks.
    pub jacks: u32,
    /// Number of PCM streams.
    pub streams: u32,
    /// Number of channel maps.
    pub chmaps: u32,
    /// Number of PCM groups (spec 1.2; 0 when unused).
    pub groups: u32,
}

impl SndConfig {
    /// Encoded length in bytes.
    pub const ENCODED_LEN: usize = 16;

    /// Encode the config space as little-endian bytes.
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; Self::ENCODED_LEN] {
        let jacks = self.jacks.to_le_bytes();
        let streams = self.streams.to_le_bytes();
        let chmaps = self.chmaps.to_le_bytes();
        let groups = self.groups.to_le_bytes();
        [
            jacks[0], jacks[1], jacks[2], jacks[3], streams[0], streams[1], streams[2], streams[3],
            chmaps[0], chmaps[1], chmaps[2], chmaps[3], groups[0], groups[1], groups[2], groups[3],
        ]
    }
}

/// `virtio_snd_pcm_info`: one PCM stream descriptor (32 bytes on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmStreamInfo {
    /// HDA function node id (0 when the stream is not tied to HDA).
    pub hda_fn_nid: u32,
    /// Stream feature bits (`1 << SND_D_PCM_FEATURE_*`).
    pub features: u32,
    /// Supported formats (`1 << SND_FMT_*`).
    pub formats: u64,
    /// Supported sample rates (`1 << rate index`).
    pub rates: u64,
    /// Stream direction.
    pub direction: PcmDirection,
    /// Minimum channel count.
    pub channels_min: u8,
    /// Maximum channel count.
    pub channels_max: u8,
}

impl PcmStreamInfo {
    /// Encoded length in bytes.
    pub const ENCODED_LEN: usize = 32;

    /// Encode as little-endian bytes per the spec layout.
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; Self::ENCODED_LEN] {
        let nid = self.hda_fn_nid.to_le_bytes();
        let features = self.features.to_le_bytes();
        let formats = self.formats.to_le_bytes();
        let rates = self.rates.to_le_bytes();
        let direction = self.direction.as_u8();
        [
            nid[0],
            nid[1],
            nid[2],
            nid[3],
            features[0],
            features[1],
            features[2],
            features[3],
            formats[0],
            formats[1],
            formats[2],
            formats[3],
            formats[4],
            formats[5],
            formats[6],
            formats[7],
            rates[0],
            rates[1],
            rates[2],
            rates[3],
            rates[4],
            rates[5],
            rates[6],
            rates[7],
            direction,
            self.channels_min,
            self.channels_max,
            0,
            0,
            0,
            0,
            0,
        ]
    }

    /// Whether this format index is advertised by the stream.
    #[must_use]
    pub const fn supports_format(self, format: u8) -> bool {
        format < 64 && (self.formats & (1u64 << format)) != 0
    }

    /// Whether this rate index is advertised by the stream.
    #[must_use]
    pub const fn supports_rate(self, rate: u8) -> bool {
        (rate as usize) < SND_RATES_HZ.len() && (self.rates & (1u64 << rate)) != 0
    }

    /// Sample rate in Hz for a rate index, or `None` when out of range.
    #[must_use]
    pub const fn rate_hz(rate: u8) -> Option<u32> {
        if (rate as usize) < SND_RATES_HZ.len() {
            Some(SND_RATES_HZ[rate as usize])
        } else {
            None
        }
    }
}

/// `virtio_snd_jack_info`: one jack descriptor (32 bytes on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JackInfo {
    /// HDA register value.
    pub hda_reg: u32,
    /// HDA node id.
    pub hda_nid: u32,
    /// HDA pin sense.
    pub hda_pin_sense: u32,
    /// Jack features (`1 << SND_D_JACK_FEATURE_*`).
    pub features: u32,
    /// HDA function type.
    pub hda_function: u32,
    /// HDA amplifier capabilities.
    pub hda_amp: u32,
    /// Jack label length hint.
    pub length: u32,
}

impl JackInfo {
    /// Encoded length in bytes.
    pub const ENCODED_LEN: usize = 32;

    /// Encode as little-endian bytes per the spec layout.
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; Self::ENCODED_LEN] {
        let reg = self.hda_reg.to_le_bytes();
        let nid = self.hda_nid.to_le_bytes();
        let sense = self.hda_pin_sense.to_le_bytes();
        let features = self.features.to_le_bytes();
        let function = self.hda_function.to_le_bytes();
        let amp = self.hda_amp.to_le_bytes();
        let length = self.length.to_le_bytes();
        [
            reg[0],
            reg[1],
            reg[2],
            reg[3],
            nid[0],
            nid[1],
            nid[2],
            nid[3],
            sense[0],
            sense[1],
            sense[2],
            sense[3],
            features[0],
            features[1],
            features[2],
            features[3],
            function[0],
            function[1],
            function[2],
            function[3],
            amp[0],
            amp[1],
            amp[2],
            amp[3],
            length[0],
            length[1],
            length[2],
            length[3],
            0,
            0,
            0,
            0,
        ]
    }
}

/// `virtio_snd_chmap_info`: one channel-map descriptor (23 bytes on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChmapInfo {
    /// HDA register value.
    pub hda_reg: u32,
    /// Channel positions, `channels` entries valid.
    pub positions: [u8; CHMAP_MAX_SIZE],
    /// Number of valid position entries.
    pub channels: u8,
}

impl ChmapInfo {
    /// Encoded length in bytes.
    pub const ENCODED_LEN: usize = 4 + 1 + CHMAP_MAX_SIZE;

    /// Encode as little-endian bytes per the spec layout.
    #[must_use]
    pub fn to_le_bytes(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..4].copy_from_slice(&self.hda_reg.to_le_bytes());
        out[4] = self.channels;
        out[5..].copy_from_slice(&self.positions);
        out
    }

    /// Stereo front-left/front-right map.
    #[must_use]
    pub const fn stereo() -> Self {
        let mut positions = [0u8; CHMAP_MAX_SIZE];
        positions[0] = CHMAP_FL;
        positions[1] = CHMAP_FR;
        Self {
            hda_reg: 0,
            positions,
            channels: 2,
        }
    }
}

/// `virtio_snd_pcm_set_params`: PCM stream parameters (16 bytes on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmSetParams {
    /// Requested buffer size in bytes.
    pub buffer_bytes: u32,
    /// Requested period size in bytes.
    pub period_bytes: u32,
    /// Stream features (`1 << SND_D_PCM_FEATURE_*`).
    pub features: u32,
    /// Channel count.
    pub channels: u8,
    /// Format index (`SND_FMT_*`).
    pub format: u8,
    /// Rate index (`SND_D_PCM_RATE_*`).
    pub rate: u8,
}

impl PcmSetParams {
    /// Encoded length in bytes.
    pub const ENCODED_LEN: usize = 16;

    /// Decode from the 16-byte wire representation.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::MalformedPayload`] when `bytes` is shorter than
    /// [`PcmSetParams::ENCODED_LEN`].
    pub fn from_le_bytes(bytes: &[u8]) -> Result<Self, AudioError> {
        if bytes.len() < Self::ENCODED_LEN {
            return Err(AudioError::MalformedPayload("PCM set-params too short"));
        }
        let u32_at =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        Ok(Self {
            buffer_bytes: u32_at(0),
            period_bytes: u32_at(4),
            features: u32_at(8),
            channels: bytes[12],
            format: bytes[13],
            rate: bytes[14],
        })
    }

    /// Sample rate in Hz, or `None` for an out-of-range rate index.
    #[must_use]
    pub const fn rate_hz(self) -> Option<u32> {
        PcmStreamInfo::rate_hz(self.rate)
    }
}

/// `virtio_snd_query_info` payload: `start_id` + `count` (8 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryInfo {
    /// First entry id to return.
    pub start_id: u32,
    /// Maximum number of entries to return.
    pub count: u32,
}

impl QueryInfo {
    /// Encoded length in bytes.
    pub const ENCODED_LEN: usize = 8;

    /// Decode from the 8-byte wire representation.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::MalformedPayload`] when `bytes` is shorter than
    /// [`QueryInfo::ENCODED_LEN`].
    pub fn from_le_bytes(bytes: &[u8]) -> Result<Self, AudioError> {
        if bytes.len() < Self::ENCODED_LEN {
            return Err(AudioError::MalformedPayload("query-info too short"));
        }
        let u32_at =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        Ok(Self {
            start_id: u32_at(0),
            count: u32_at(4),
        })
    }
}

/// `virtio_snd_pcm_hdr` payload: the stream id (4 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmHdr {
    /// Target stream id.
    pub stream_id: u32,
}

impl PcmHdr {
    /// Encoded length in bytes.
    pub const ENCODED_LEN: usize = 4;

    /// Decode from the 4-byte wire representation.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::MalformedPayload`] when `bytes` is shorter than
    /// [`PcmHdr::ENCODED_LEN`].
    pub const fn from_le_bytes(bytes: &[u8]) -> Result<Self, AudioError> {
        if bytes.len() < Self::ENCODED_LEN {
            return Err(AudioError::MalformedPayload("PCM header too short"));
        }
        Ok(Self {
            stream_id: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        })
    }
}

/// A device-to-guest event for the event queue (`code` + id, 8 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SndEvent {
    /// Event code (`SND_EVT_*`).
    pub code: u32,
    /// Jack id or stream id, depending on `code`.
    pub id: u32,
}

impl SndEvent {
    /// Encode as little-endian bytes.
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; 8] {
        let code = self.code.to_le_bytes();
        let id = self.id.to_le_bytes();
        [
            code[0], code[1], code[2], code[3], id[0], id[1], id[2], id[3],
        ]
    }
}

/// Control-queue response: a status code plus an optional payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlResponse {
    /// Status code (`SND_S_*`).
    pub status: u32,
    /// Response payload (info entries, empty for pure acknowledgements).
    pub payload: Vec<u8>,
}

impl ControlResponse {
    /// Build a status-only response.
    #[must_use]
    pub const fn status(status: u32) -> Self {
        Self {
            status,
            payload: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// PCM format decoding
// ---------------------------------------------------------------------------

/// Bytes per sample for the formats this device decodes.
const fn bytes_per_sample(format: u8) -> Option<usize> {
    match format {
        SND_FMT_S8 | SND_FMT_U8 => Some(1),
        SND_FMT_S16_LE | SND_FMT_S16_BE | SND_FMT_U16_LE | SND_FMT_U16_BE => Some(2),
        SND_FMT_S24_LE | SND_FMT_S24_BE | SND_FMT_U24_LE | SND_FMT_U24_BE => Some(3),
        SND_FMT_S32_LE | SND_FMT_S32_BE | SND_FMT_U32_LE | SND_FMT_U32_BE | SND_FMT_FLOAT_LE
        | SND_FMT_FLOAT_BE => Some(4),
        SND_FMT_FLOAT64_LE | SND_FMT_FLOAT64_BE => Some(8),
        _ => None,
    }
}

/// Decode one sample of `format` at `bytes` into `i16`.
///
/// Every arm yields an `i32` already inside the `i16` range; the single
/// conversion at the end is infallible after clamping.
fn decode_sample(format: u8, bytes: &[u8]) -> Option<i16> {
    let s32 = match format {
        SND_FMT_S8 => i32::from(i8::from_ne_bytes([bytes[0]])) << 8,
        SND_FMT_U8 => (i32::from(bytes[0]) - 128) << 8,
        SND_FMT_S16_LE => i32::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        SND_FMT_S16_BE => i32::from(i16::from_be_bytes([bytes[0], bytes[1]])),
        SND_FMT_U16_LE => i32::from(u16::from_le_bytes([bytes[0], bytes[1]])) - 32768,
        SND_FMT_U16_BE => i32::from(u16::from_be_bytes([bytes[0], bytes[1]])) - 32768,
        // 24-bit packed: value sits in the low 24 bits, sign at bit 23.
        // `(v << 8) >> 16` sign-extends then scales to 16 bits.
        SND_FMT_S24_LE => (i32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) << 8) >> 16,
        SND_FMT_S24_BE => (i32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]) << 8) >> 16,
        SND_FMT_U24_LE => {
            let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]) >> 8;
            i32::try_from(raw).unwrap_or(0xFFFF) - 32_768
        }
        SND_FMT_U24_BE => {
            let raw = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8;
            i32::try_from(raw).unwrap_or(0xFFFF) - 32_768
        }
        SND_FMT_S32_LE => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 16,
        SND_FMT_S32_BE => i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 16,
        SND_FMT_U32_LE => {
            let raw = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 16;
            i32::try_from(raw).unwrap_or(0xFFFF) - 32_768
        }
        SND_FMT_U32_BE => {
            let raw = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 16;
            i32::try_from(raw).unwrap_or(0xFFFF) - 32_768
        }
        SND_FMT_FLOAT_LE => {
            let v = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            float_to_i16(f64::from(v))
        }
        SND_FMT_FLOAT_BE => {
            let v = f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            float_to_i16(f64::from(v))
        }
        SND_FMT_FLOAT64_LE => {
            let v = f64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
            float_to_i16(v)
        }
        SND_FMT_FLOAT64_BE => {
            let v = f64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
            float_to_i16(v)
        }
        _ => return None,
    };
    let clamped = s32.clamp(i32::from(i16::MIN), i32::from(i16::MAX));
    Some(i16::try_from(clamped).unwrap_or(0))
}

/// Convert a `-1.0..=1.0` float sample to an `i16`-range `i32` with clamping.
#[allow(clippy::cast_possible_truncation)] // clamped to the i16 range before casting
fn float_to_i16(v: f64) -> i32 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i32
}

/// Decode interleaved PCM `data` into `i16` samples.
///
/// The buffer must hold a whole number of frames (`channels` samples each).
///
/// # Errors
///
/// Returns [`AudioError::UnsupportedFormat`] for formats this device does not
/// decode, or [`AudioError::MalformedPayload`] when `data` is not an integral
/// number of frames.
pub fn decode_pcm_frames(format: u8, channels: u8, data: &[u8]) -> Result<Vec<i16>, AudioError> {
    let bps = bytes_per_sample(format).ok_or(AudioError::UnsupportedFormat(format))?;
    let channels_usize = usize::from(channels.max(1));
    let frame_bytes = bps * channels_usize;
    if frame_bytes == 0 || !data.len().is_multiple_of(frame_bytes) {
        return Err(AudioError::MalformedPayload(
            "PCM data is not frame-aligned",
        ));
    }
    let mut out = Vec::with_capacity(data.len() / bps);
    for frame in data.chunks_exact(frame_bytes) {
        for sample in frame.chunks_exact(bps) {
            out.push(decode_sample(format, sample).ok_or(AudioError::UnsupportedFormat(format))?);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// VirtIO-sound device
// ---------------------------------------------------------------------------

/// Runtime state of one PCM stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum StreamState {
    /// No parameters accepted yet.
    #[default]
    Idle,
    /// Parameters accepted via `SND_R_PCM_SET_PARAMS`.
    Configured,
    /// `SND_R_PCM_PREPARE` completed.
    Prepared,
    /// Stream is producing/consuming PCM.
    Running,
    /// Stream paused (logically still running).
    Paused,
}

/// One PCM stream on a [`VirtioSoundDevice`].
#[derive(Debug)]
struct PcmStream {
    /// Static stream descriptor.
    info: PcmStreamInfo,
    /// Accepted parameters.
    params: Option<PcmSetParams>,
    /// State-machine state.
    state: StreamState,
    /// Mixer source id for output streams (assigned by [`AudioEngine`]).
    mixer_source: Option<u64>,
    /// Decoded TX frames waiting to be handed to the mixer.
    pending_tx: VecDeque<i16>,
    /// Captured RX frames waiting for the guest to read.
    rx_buffer: VecDeque<i16>,
}

impl PcmStream {
    fn new(info: PcmStreamInfo) -> Self {
        Self {
            info,
            params: None,
            state: StreamState::default(),
            mixer_source: None,
            pending_tx: VecDeque::new(),
            rx_buffer: VecDeque::new(),
        }
    }

    fn is_output(&self) -> bool {
        self.info.direction == PcmDirection::Output
    }
}

/// Per-device statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SndStats {
    /// Control requests handled.
    pub control_requests: u64,
    /// Control requests rejected with an error status.
    pub control_errors: u64,
    /// TX PCM frames accepted from the guest.
    pub tx_frames: u64,
    /// RX PCM frames delivered to the guest.
    pub rx_frames: u64,
    /// TX submissions dropped because the stream was not running.
    pub tx_xruns: u64,
}

/// A `VirtIO`-sound device presented to one guest.
///
/// Owns the control-queue protocol state (config space, stream state
/// machines, jack/channel-map tables, event queue). PCM transport to the
/// hypervisor mixer is wired by [`AudioEngine`], which assigns mixer source
/// ids and drains `pending_tx` every tick.
pub struct VirtioSoundDevice {
    name: String,
    streams: Vec<PcmStream>,
    jacks: Vec<JackInfo>,
    chmaps: Vec<ChmapInfo>,
    events: VecDeque<SndEvent>,
    stats: SndStats,
}

impl VirtioSoundDevice {
    /// Build a device with one stereo output stream, one stereo input stream,
    /// one jack and one stereo channel map.
    #[must_use]
    pub fn new(name: &str) -> Self {
        let pcm_formats = (1u64 << SND_FMT_S8)
            | (1u64 << SND_FMT_U8)
            | (1u64 << SND_FMT_S16_LE)
            | (1u64 << SND_FMT_S16_BE)
            | (1u64 << SND_FMT_U16_LE)
            | (1u64 << SND_FMT_U16_BE)
            | (1u64 << SND_FMT_S24_LE)
            | (1u64 << SND_FMT_S24_BE)
            | (1u64 << SND_FMT_U24_LE)
            | (1u64 << SND_FMT_U24_BE)
            | (1u64 << SND_FMT_S32_LE)
            | (1u64 << SND_FMT_S32_BE)
            | (1u64 << SND_FMT_U32_LE)
            | (1u64 << SND_FMT_U32_BE)
            | (1u64 << SND_FMT_FLOAT_LE)
            | (1u64 << SND_FMT_FLOAT64_LE);
        // 8 kHz .. 192 kHz (rate indices 1..=12).
        let pcm_rates = 0b1_1111_1111_1110u64;
        let output = PcmStreamInfo {
            hda_fn_nid: 0,
            features: 0,
            formats: pcm_formats,
            rates: pcm_rates,
            direction: PcmDirection::Output,
            channels_min: 1,
            channels_max: 2,
        };
        let input = PcmStreamInfo {
            direction: PcmDirection::Input,
            ..output
        };
        Self {
            name: name.to_owned(),
            streams: vec![PcmStream::new(output), PcmStream::new(input)],
            jacks: vec![JackInfo {
                hda_reg: 1,
                hda_nid: 0x1b,
                hda_pin_sense: 0,
                features: 0,
                hda_function: 0,
                hda_amp: 0,
                length: 0,
            }],
            chmaps: vec![ChmapInfo::stereo()],
            events: VecDeque::new(),
            stats: SndStats::default(),
        }
    }

    /// Device name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `VirtIO` device id (25).
    #[must_use]
    pub const fn device_id(&self) -> u32 {
        VIRTIO_SND_DEVICE_ID
    }

    /// Device configuration space bytes.
    #[must_use]
    pub fn config_bytes(&self) -> [u8; SndConfig::ENCODED_LEN] {
        SndConfig {
            jacks: u32::try_from(self.jacks.len()).unwrap_or(u32::MAX),
            streams: u32::try_from(self.streams.len()).unwrap_or(u32::MAX),
            chmaps: u32::try_from(self.chmaps.len()).unwrap_or(u32::MAX),
            groups: 0,
        }
        .to_le_bytes()
    }

    /// Number of PCM streams.
    #[must_use]
    pub const fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Device statistics.
    #[must_use]
    pub const fn stats(&self) -> &SndStats {
        &self.stats
    }

    /// Pop the oldest pending event for the event queue, if any.
    pub fn pop_event(&mut self) -> Option<SndEvent> {
        self.events.pop_front()
    }

    /// Assign a mixer source id to an output stream (called by [`AudioEngine`]).
    pub(crate) fn set_mixer_source(&mut self, stream_id: usize, source: u64) {
        if let Some(stream) = self.streams.get_mut(stream_id) {
            stream.mixer_source = Some(source);
        }
    }

    /// Mixer source id of an output stream.
    pub(crate) fn mixer_source(&self, stream_id: usize) -> Option<u64> {
        self.streams.get(stream_id).and_then(|s| s.mixer_source)
    }

    fn stream_mut(&mut self, stream_id: u32) -> Result<&mut PcmStream, AudioError> {
        let idx = usize::try_from(stream_id).map_err(|_| AudioError::UnknownStream(stream_id))?;
        self.streams
            .get_mut(idx)
            .ok_or(AudioError::UnknownStream(stream_id))
    }

    fn push_event(&mut self, code: u32, id: u32) {
        // The event queue is finite; drop the oldest event on overflow rather
        // than growing without bound.
        if self.events.len() >= 64 {
            self.events.pop_front();
        }
        self.events.push_back(SndEvent { code, id });
    }

    /// Handle one control-queue request.
    ///
    /// `code` is the `virtio_snd_hdr.code`; `request` is the remaining
    /// request payload (the header itself is already consumed).
    pub fn handle_control(&mut self, code: u32, request: &[u8]) -> ControlResponse {
        self.stats.control_requests += 1;
        let response = match code {
            SND_R_JACK_INFO => self.query_jack_info(request),
            SND_R_PCM_INFO => self.query_pcm_info(request),
            SND_R_CHMAP_INFO => self.query_chmap_info(request),
            SND_R_JACK_REMAP => ControlResponse::status(SND_S_NOT_SUPP),
            SND_R_PCM_SET_PARAMS => self.pcm_set_params(request),
            SND_R_PCM_PREPARE | SND_R_PCM_RELEASE | SND_R_PCM_START | SND_R_PCM_STOP
            | SND_R_PCM_PAUSE | SND_R_PCM_UNPAUSE => self.pcm_state_control(code, request),
            _ => ControlResponse::status(SND_S_BAD_MSG),
        };
        if response.status != SND_S_OK {
            self.stats.control_errors += 1;
        }
        response
    }

    fn query_jack_info(&self, request: &[u8]) -> ControlResponse {
        let Ok(query) = QueryInfo::from_le_bytes(request) else {
            return ControlResponse::status(SND_S_BAD_MSG);
        };
        encode_info_query(&self.jacks, query, JackInfo::to_le_bytes)
    }

    fn query_pcm_info(&self, request: &[u8]) -> ControlResponse {
        let Ok(query) = QueryInfo::from_le_bytes(request) else {
            return ControlResponse::status(SND_S_BAD_MSG);
        };
        let infos: Vec<PcmStreamInfo> = self.streams.iter().map(|s| s.info).collect();
        encode_info_query(&infos, query, PcmStreamInfo::to_le_bytes)
    }

    fn query_chmap_info(&self, request: &[u8]) -> ControlResponse {
        let Ok(query) = QueryInfo::from_le_bytes(request) else {
            return ControlResponse::status(SND_S_BAD_MSG);
        };
        encode_info_query(&self.chmaps, query, ChmapInfo::to_le_bytes)
    }

    fn pcm_set_params(&mut self, request: &[u8]) -> ControlResponse {
        if request.len() < PcmHdr::ENCODED_LEN + PcmSetParams::ENCODED_LEN {
            return ControlResponse::status(SND_S_BAD_MSG);
        }
        let stream_id = u32::from_le_bytes([request[0], request[1], request[2], request[3]]);
        let Ok(params) = PcmSetParams::from_le_bytes(&request[PcmHdr::ENCODED_LEN..]) else {
            return ControlResponse::status(SND_S_BAD_MSG);
        };
        let Ok(stream) = self.stream_mut(stream_id) else {
            return ControlResponse::status(SND_S_BAD_MSG);
        };
        if !stream.info.supports_format(params.format) {
            return ControlResponse::status(SND_S_NOT_SUPP);
        }
        if !stream.info.supports_rate(params.rate) {
            return ControlResponse::status(SND_S_NOT_SUPP);
        }
        if params.channels < stream.info.channels_min || params.channels > stream.info.channels_max
        {
            return ControlResponse::status(SND_S_NOT_SUPP);
        }
        if params.period_bytes == 0 || params.buffer_bytes < params.period_bytes {
            return ControlResponse::status(SND_S_BAD_MSG);
        }
        stream.params = Some(params);
        stream.state = StreamState::Configured;
        stream.pending_tx.clear();
        stream.rx_buffer.clear();
        ControlResponse::status(SND_S_OK)
    }

    fn pcm_state_control(&mut self, code: u32, request: &[u8]) -> ControlResponse {
        let Ok(hdr) = PcmHdr::from_le_bytes(request) else {
            return ControlResponse::status(SND_S_BAD_MSG);
        };
        let Ok(stream) = self.stream_mut(hdr.stream_id) else {
            return ControlResponse::status(SND_S_BAD_MSG);
        };
        let next = match code {
            SND_R_PCM_PREPARE => match stream.state {
                StreamState::Idle => None,
                _ => Some(StreamState::Prepared),
            },
            SND_R_PCM_RELEASE => Some(match stream.state {
                StreamState::Idle => StreamState::Idle,
                _ => StreamState::Configured,
            }),
            SND_R_PCM_START => match stream.state {
                StreamState::Prepared | StreamState::Running | StreamState::Paused => {
                    Some(StreamState::Running)
                }
                _ => None,
            },
            SND_R_PCM_STOP => Some(match stream.state {
                StreamState::Idle => StreamState::Idle,
                StreamState::Configured => StreamState::Configured,
                _ => StreamState::Prepared,
            }),
            SND_R_PCM_PAUSE => match stream.state {
                StreamState::Running => Some(StreamState::Paused),
                _ => None,
            },
            SND_R_PCM_UNPAUSE => match stream.state {
                StreamState::Paused => Some(StreamState::Running),
                _ => None,
            },
            _ => None,
        };
        let Some(next) = next else {
            return ControlResponse::status(SND_S_IO_ERR);
        };
        stream.state = next;
        if code == SND_R_PCM_START || code == SND_R_PCM_STOP {
            self.push_event(SND_EVT_PCM, hdr.stream_id);
        }
        ControlResponse::status(SND_S_OK)
    }

    /// Queue decoded TX PCM frames for the mixer. The guest must have the
    /// stream running; otherwise an xrun is recorded and the data is dropped.
    pub(crate) fn submit_tx_bytes(
        &mut self,
        stream_id: u32,
        data: &[u8],
    ) -> Result<usize, AudioError> {
        let stream = self.stream_mut(stream_id)?;
        if !stream.is_output() {
            return Err(AudioError::NotOutputStream(stream_id));
        }
        let params = stream
            .params
            .ok_or(AudioError::StreamNotRunning(stream_id))?;
        if stream.state != StreamState::Running {
            self.stats.tx_xruns += 1;
            self.push_event(SND_EVT_PCM, stream_id);
            return Err(AudioError::StreamNotRunning(stream_id));
        }
        let samples = decode_pcm_frames(params.format, params.channels, data)?;
        let frames = samples.len() / usize::from(params.channels.max(1));
        self.stats.tx_frames += u64::try_from(frames).unwrap_or(u64::MAX);
        // Bound the queue at ~2 s of audio; drop the oldest on overflow.
        let cap = usize::try_from(params.rate_hz().unwrap_or(MIXER_OUTPUT_RATE_HZ))
            .unwrap_or(MIXER_OUTPUT_RATE_HZ as usize)
            * usize::from(params.channels.max(1))
            * 2;
        let stream = self.stream_mut(stream_id)?;
        stream.pending_tx.extend(samples);
        while stream.pending_tx.len() > cap {
            stream.pending_tx.pop_front();
        }
        Ok(frames)
    }

    /// Drain queued TX frames for one output stream.
    pub(crate) fn drain_tx(&mut self, stream_id: usize) -> Vec<i16> {
        self.streams
            .get_mut(stream_id)
            .map_or_else(Vec::new, |s| s.pending_tx.drain(..).collect())
    }

    /// Read up to `max_frames` captured frames from an input stream's RX buffer.
    pub(crate) fn read_rx(
        &mut self,
        stream_id: u32,
        max_frames: usize,
    ) -> Result<Vec<i16>, AudioError> {
        let stream = self.stream_mut(stream_id)?;
        if !stream.is_output() {
            let params = stream.params;
            let channels = usize::from(params.map_or(2, |p| p.channels.max(1)));
            let want = max_frames * channels;
            let have = stream.rx_buffer.len().min(want);
            let out: Vec<i16> = stream.rx_buffer.drain(..have).collect();
            self.stats.rx_frames += u64::try_from(out.len() / channels.max(1)).unwrap_or(u64::MAX);
            return Ok(out);
        }
        Err(AudioError::NotInputStream(stream_id))
    }

    /// Append host-captured samples to every input stream's RX buffer.
    pub(crate) fn push_capture(&mut self, samples: &[i16]) {
        for stream in &mut self.streams {
            if !stream.is_output() {
                stream.rx_buffer.extend(samples.iter().copied());
                // Bound at ~2 s of stereo 48 kHz.
                while stream.rx_buffer.len() > MIXER_OUTPUT_RATE_HZ as usize * 2 * 2 {
                    stream.rx_buffer.pop_front();
                }
            }
        }
    }
}

/// Encode a `SND_R_*_INFO` query response over `entries`.
fn encode_info_query<T, const N: usize>(
    entries: &[T],
    query: QueryInfo,
    encode: fn(T) -> [u8; N],
) -> ControlResponse
where
    T: Copy,
{
    let start = usize::try_from(query.start_id).unwrap_or(usize::MAX);
    if start > entries.len() {
        return ControlResponse::status(SND_S_BAD_MSG);
    }
    let count = usize::try_from(query.count)
        .unwrap_or(0)
        .min(entries.len() - start);
    let mut payload = Vec::with_capacity(count * N);
    for entry in entries.iter().skip(start).take(count) {
        payload.extend_from_slice(&encode(*entry));
    }
    ControlResponse {
        status: SND_S_OK,
        payload,
    }
}

// ---------------------------------------------------------------------------
// Audio backends (physical output)
// ---------------------------------------------------------------------------

/// Physical audio output sink: consumes interleaved `i16` PCM frames at the
/// mixer's output rate and channel count.
pub trait AudioBackend {
    /// Output sample rate in Hz.
    fn sample_rate(&self) -> u32;
    /// Output channel count.
    fn channels(&self) -> u8;
    /// Write interleaved frames. Returns the number of frames accepted.
    fn write_frames(&mut self, frames: &[i16]) -> usize;
}

/// Backend that discards all audio. Used on headless hosts and in tests that
/// only exercise the mixer itself.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullAudioBackend {
    /// Sample rate reported to the mixer.
    pub sample_rate: u32,
    /// Channel count reported to the mixer.
    pub channels: u8,
    /// Total frames discarded so far.
    pub frames_written: u64,
}

impl NullAudioBackend {
    /// Build a null backend matching the default mixer output format.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sample_rate: MIXER_OUTPUT_RATE_HZ,
            channels: MIXER_OUTPUT_CHANNELS,
            frames_written: 0,
        }
    }
}

impl AudioBackend for NullAudioBackend {
    fn sample_rate(&self) -> u32 {
        if self.sample_rate == 0 {
            MIXER_OUTPUT_RATE_HZ
        } else {
            self.sample_rate
        }
    }

    fn channels(&self) -> u8 {
        if self.channels == 0 {
            MIXER_OUTPUT_CHANNELS
        } else {
            self.channels
        }
    }

    fn write_frames(&mut self, frames: &[i16]) -> usize {
        let channels = usize::from(self.channels());
        let accepted = frames.len() / channels.max(1);
        self.frames_written += u64::try_from(accepted).unwrap_or(u64::MAX);
        accepted
    }
}

/// Backend that records written frames in memory. Used by tests; also handy
/// for capturing mixed output without physical hardware.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VecAudioBackend {
    /// Recorded interleaved samples.
    pub frames: Vec<i16>,
    /// Sample rate reported to the mixer.
    pub sample_rate: u32,
    /// Channel count reported to the mixer.
    pub channels: u8,
}

impl VecAudioBackend {
    /// Build a recording backend matching the default mixer output format.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frames: Vec::new(),
            sample_rate: MIXER_OUTPUT_RATE_HZ,
            channels: MIXER_OUTPUT_CHANNELS,
        }
    }
}

impl AudioBackend for VecAudioBackend {
    fn sample_rate(&self) -> u32 {
        if self.sample_rate == 0 {
            MIXER_OUTPUT_RATE_HZ
        } else {
            self.sample_rate
        }
    }

    fn channels(&self) -> u8 {
        if self.channels == 0 {
            MIXER_OUTPUT_CHANNELS
        } else {
            self.channels
        }
    }

    fn write_frames(&mut self, frames: &[i16]) -> usize {
        self.frames.extend_from_slice(frames);
        frames.len() / usize::from(self.channels()).max(1)
    }
}

// ---------------------------------------------------------------------------
// Hypervisor mixing engine
// ---------------------------------------------------------------------------

/// Q16 fixed-point unity gain: `volume_q16 == 0x1_0000` passes samples through.
const VOLUME_UNITY_Q16: u32 = 0x1_0000;

/// Convert a `0.0..=1.0` volume to Q16 fixed point.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to [0.0, 1.0]; exact in range
fn volume_to_q16(volume: f32) -> u32 {
    (volume.clamp(0.0, 1.0) * 65536.0).round() as u32
}

/// Format of one mixer input source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSpec {
    /// Source sample rate in Hz.
    pub sample_rate: u32,
    /// Source channel count.
    pub channels: u8,
}

/// Runtime state of one mixer source (typically one guest output stream).
#[derive(Debug)]
struct MixerSource {
    spec: SourceSpec,
    /// Queued interleaved source-rate samples.
    fifo: VecDeque<i16>,
    /// Per-source gain in Q16 (`0x1_0000` = unity).
    volume_q16: u32,
    /// Muted sources contribute nothing.
    muted: bool,
    /// Absolute resample position in source frames, Q16 fixed point.
    phase: u64,
    /// Source frames already drained from `fifo`.
    consumed_frames: u64,
    /// Output frames rendered as silence for lack of input.
    underruns: u64,
}

impl MixerSource {
    const fn new(spec: SourceSpec) -> Self {
        Self {
            spec,
            fifo: VecDeque::new(),
            volume_q16: VOLUME_UNITY_Q16,
            muted: false,
            phase: 0,
            consumed_frames: 0,
            underruns: 0,
        }
    }

    /// Q16 source frames advanced per output frame.
    fn step_q16(&self, output_rate: u32) -> u64 {
        if output_rate == 0 {
            return 0;
        }
        (u64::from(self.spec.sample_rate.max(1)) << 16) / u64::from(output_rate)
    }

    /// Frames currently buffered.
    fn buffered_frames(&self) -> usize {
        self.fifo.len() / usize::from(self.spec.channels.max(1))
    }
}

/// Per-tick report from [`AudioMixer::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickStats {
    /// Output frames rendered this tick.
    pub frames_rendered: usize,
    /// Sources mixed (unmuted with nonzero volume).
    pub sources_mixed: usize,
    /// Sources that underran at least once this tick.
    pub underrun_sources: usize,
    /// Wall-clock time spent rendering.
    pub elapsed: Duration,
}

/// The hypervisor audio mixing engine.
///
/// Mixes any number of PCM sources (one per guest output stream) into a
/// single output stream: integer-only linear resampling from each source
/// rate to the output rate, per-source volume/mute, saturating mix into
/// `i16`. One [`AudioMixer::tick`] renders exactly `period_frames`
/// (10 ms at 48 kHz), which bounds the output latency.
pub struct AudioMixer {
    sources: HashMap<u64, MixerSource>,
    next_source_id: u64,
    output_rate: u32,
    output_channels: u8,
    period_frames: usize,
}

impl AudioMixer {
    /// Build a mixer with the default output format (48 kHz stereo, 10 ms periods).
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(
            MIXER_OUTPUT_RATE_HZ,
            MIXER_OUTPUT_CHANNELS,
            MIXER_PERIOD_FRAMES,
        )
    }

    /// Build a mixer with an explicit output format.
    #[must_use]
    pub fn with_config(output_rate: u32, output_channels: u8, period_frames: usize) -> Self {
        Self {
            sources: HashMap::new(),
            next_source_id: 1,
            output_rate: output_rate.max(1),
            output_channels: output_channels.max(1),
            period_frames: period_frames.max(1),
        }
    }

    /// Output sample rate in Hz.
    #[must_use]
    pub const fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// Output channel count.
    #[must_use]
    pub const fn output_channels(&self) -> u8 {
        self.output_channels
    }

    /// Frames rendered per tick.
    #[must_use]
    pub const fn period_frames(&self) -> usize {
        self.period_frames
    }

    /// Number of registered sources.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// Whether any source is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Register a source; returns its id.
    pub fn add_source(&mut self, spec: SourceSpec) -> u64 {
        let id = self.next_source_id;
        self.next_source_id = self.next_source_id.wrapping_add(1).max(1);
        self.sources.insert(id, MixerSource::new(spec));
        id
    }

    /// Remove a source.
    pub fn remove_source(&mut self, id: u64) -> bool {
        self.sources.remove(&id).is_some()
    }

    /// Queue interleaved source-rate samples. Returns samples accepted.
    ///
    /// The FIFO is capped at ~2 s of audio; the oldest samples are dropped
    /// on overflow so a stalled mixer cannot grow memory without bound.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownSource`] when no source has `id`.
    pub fn push_samples(&mut self, id: u64, samples: &[i16]) -> Result<usize, AudioError> {
        let source = self
            .sources
            .get_mut(&id)
            .ok_or(AudioError::UnknownSource(id))?;
        let cap = usize::try_from(source.spec.sample_rate)
            .unwrap_or(MIXER_OUTPUT_RATE_HZ as usize)
            .saturating_mul(usize::from(source.spec.channels.max(1)))
            .saturating_mul(2);
        source.fifo.extend(samples.iter().copied());
        while source.fifo.len() > cap {
            source.fifo.pop_front();
        }
        Ok(samples.len())
    }

    /// Set per-source gain (`0.0` = silence, `1.0` = unity; clamped).
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownSource`] when no source has `id`.
    pub fn set_volume(&mut self, id: u64, volume: f32) -> Result<(), AudioError> {
        let source = self
            .sources
            .get_mut(&id)
            .ok_or(AudioError::UnknownSource(id))?;
        source.volume_q16 = volume_to_q16(volume);
        Ok(())
    }

    /// Mute or unmute a source.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownSource`] when no source has `id`.
    pub fn set_mute(&mut self, id: u64, muted: bool) -> Result<(), AudioError> {
        let source = self
            .sources
            .get_mut(&id)
            .ok_or(AudioError::UnknownSource(id))?;
        source.muted = muted;
        Ok(())
    }

    /// Total underrun frames recorded for a source.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownSource`] when no source has `id`.
    pub fn underruns(&self, id: u64) -> Result<u64, AudioError> {
        self.sources
            .get(&id)
            .map(|s| s.underruns)
            .ok_or(AudioError::UnknownSource(id))
    }

    /// Render one period into `backend`.
    ///
    /// Every source is linearly resampled (Q16 fixed point) to the output
    /// rate, channel-mapped (mono duplicates to stereo; wider sources take
    /// the first `output_channels`), gained, and saturating-mixed. Missing
    /// input renders as silence and counts as an underrun.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::BackendWrite`] when the backend does not accept
    /// the full period.
    pub fn tick(&mut self, backend: &mut dyn AudioBackend) -> Result<TickStats, AudioError> {
        let started = Instant::now();
        let out_ch = usize::from(self.output_channels);
        let period_frames = self.period_frames;
        let output_rate = self.output_rate;
        let output_channels = self.output_channels;
        let mut acc = vec![0i32; period_frames * out_ch];
        let mut sources_mixed = 0usize;
        let mut underrun_sources = 0usize;

        for source in self.sources.values_mut() {
            if source.muted || source.volume_q16 == 0 {
                continue;
            }
            sources_mixed += 1;
            if Self::render_source(
                source,
                &mut acc,
                period_frames,
                output_rate,
                output_channels,
            ) {
                underrun_sources += 1;
            }
        }

        let mut out = Vec::with_capacity(acc.len());
        for v in acc {
            let clamped = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX));
            out.push(i16::try_from(clamped).unwrap_or(0));
        }
        let frames = backend.write_frames(&out);
        if frames != self.period_frames {
            return Err(AudioError::BackendWrite);
        }
        Ok(TickStats {
            frames_rendered: frames,
            sources_mixed,
            underrun_sources,
            elapsed: started.elapsed(),
        })
    }

    /// Render one source into the accumulator. Returns `true` on underrun.
    fn render_source(
        source: &mut MixerSource,
        acc: &mut [i32],
        period_frames: usize,
        output_rate: u32,
        output_channels: u8,
    ) -> bool {
        let in_ch = usize::from(source.spec.channels.max(1));
        let out_ch = usize::from(output_channels);
        let step = source.step_q16(output_rate);
        let vol = i32::try_from(source.volume_q16.min(VOLUME_UNITY_Q16)).unwrap_or(i32::MAX);
        let mut underran = false;

        for frame in 0..period_frames {
            let pos = source.phase >> 16;
            let frac = u32::try_from(source.phase & 0xFFFF).unwrap_or(0);
            let have =
                source.consumed_frames + u64::try_from(source.buffered_frames()).unwrap_or(0);
            if pos >= have {
                // No input at all: silence + underrun.
                source.underruns += 1;
                underran = true;
                source.phase += step;
                continue;
            }
            let base = usize::try_from(pos - source.consumed_frames).unwrap_or(usize::MAX);
            // At the very edge of the buffered input the `s1` sample has not
            // arrived yet; repeat `s0` rather than dropping an audible frame.
            let base1 = if pos + 1 < have { base + 1 } else { base };
            let inv_frac = 65_536 - i32::try_from(frac).unwrap_or(0);
            let frac_i = i32::try_from(frac).unwrap_or(0);
            for c in 0..out_ch {
                let sc = c.min(in_ch - 1);
                let s0 = i32::from(source.fifo[base * in_ch + sc]);
                let s1 = i32::from(source.fifo[base1 * in_ch + sc]);
                // Linear interpolation in Q16, then Q16 gain, all integer.
                let interp = (s0 * inv_frac + s1 * frac_i) >> 16;
                acc[frame * out_ch + c] += (interp * vol) >> 16;
            }
            source.phase += step;
        }

        // Drain frames that interpolation can no longer reach, keeping the
        // frame at the final position (it may be reused as `s0` next tick).
        let final_pos = source.phase >> 16;
        if final_pos > source.consumed_frames {
            let drain_frames = usize::try_from(final_pos - source.consumed_frames).unwrap_or(0);
            let drain_samples = drain_frames.saturating_mul(in_ch).min(source.fifo.len());
            source.fifo.drain(..drain_samples);
            source.consumed_frames += u64::try_from(drain_samples / in_ch.max(1)).unwrap_or(0);
        }
        underran
    }
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Audio engine: devices + mixer wiring
// ---------------------------------------------------------------------------

/// The hypervisor-wide audio engine.
///
/// Owns the [`AudioMixer`] and every guest's [`VirtioSoundDevice`]. TX PCM
/// submitted through a device lands in that device's mixer source; each
/// [`AudioEngine::tick`] renders one 10 ms period to the backend and
/// distributes host-captured audio to input streams' RX buffers.
pub struct AudioEngine {
    mixer: AudioMixer,
    devices: Vec<VirtioSoundDevice>,
}

impl AudioEngine {
    /// Build an engine with a default mixer and no devices.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mixer: AudioMixer::new(),
            devices: Vec::new(),
        }
    }

    /// Attach a guest device. Each output stream gets a mixer source fed at
    /// the stream's negotiated rate once `SND_R_PCM_SET_PARAMS` lands
    /// (defaulting to 48 kHz stereo until then).
    pub fn add_device(&mut self, name: &str) -> u32 {
        let mut device = VirtioSoundDevice::new(name);
        for stream_id in 0..device.stream_count() {
            if device.streams[stream_id].is_output() {
                let source = self.mixer.add_source(SourceSpec {
                    sample_rate: MIXER_OUTPUT_RATE_HZ,
                    channels: 2,
                });
                device.set_mixer_source(stream_id, source);
            }
        }
        let id = u32::try_from(self.devices.len()).unwrap_or(u32::MAX);
        self.devices.push(device);
        id
    }

    /// Borrow a device.
    #[must_use]
    pub fn device(&self, id: u32) -> Option<&VirtioSoundDevice> {
        self.devices.get(usize::try_from(id).unwrap_or(usize::MAX))
    }

    /// Mutably borrow a device.
    pub fn device_mut(&mut self, id: u32) -> Option<&mut VirtioSoundDevice> {
        self.devices
            .get_mut(usize::try_from(id).unwrap_or(usize::MAX))
    }

    /// Number of attached devices.
    #[must_use]
    pub const fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Borrow the mixer.
    #[must_use]
    pub const fn mixer(&self) -> &AudioMixer {
        &self.mixer
    }

    /// Mutably borrow the mixer.
    pub const fn mixer_mut(&mut self) -> &mut AudioMixer {
        &mut self.mixer
    }

    /// Handle a control-queue request for a device.
    pub fn control(&mut self, device_id: u32, code: u32, request: &[u8]) -> ControlResponse {
        let Some(device) = self.device_mut(device_id) else {
            return ControlResponse::status(SND_S_BAD_MSG);
        };
        let response = device.handle_control(code, request);
        // Re-target the mixer source when stream parameters change.
        if code == SND_R_PCM_SET_PARAMS && response.status == SND_S_OK {
            self.retarget_source(device_id, request);
        }
        response
    }

    /// Re-create the mixer source for a stream after `SET_PARAMS` so the
    /// resampler tracks the negotiated rate/channel count.
    fn retarget_source(&mut self, device_id: u32, request: &[u8]) {
        let stream_id = u32::from_le_bytes([request[0], request[1], request[2], request[3]]);
        let Ok(params) = PcmSetParams::from_le_bytes(&request[PcmHdr::ENCODED_LEN..]) else {
            return;
        };
        let rate = params.rate_hz().unwrap_or(MIXER_OUTPUT_RATE_HZ);
        let channels = params.channels.max(1);
        let old = self
            .device(device_id)
            .and_then(|d| d.mixer_source(usize::try_from(stream_id).unwrap_or(usize::MAX)));
        if let Some(old_id) = old {
            self.mixer.remove_source(old_id);
        }
        let new_id = self.mixer.add_source(SourceSpec {
            sample_rate: rate,
            channels,
        });
        if let Some(device) = self.device_mut(device_id) {
            device.set_mixer_source(usize::try_from(stream_id).unwrap_or(usize::MAX), new_id);
        }
    }

    /// Submit TX-queue PCM bytes for a device output stream.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownDevice`] for an unknown device,
    /// [`AudioError::UnknownStream`] for an unknown stream,
    /// [`AudioError::NotOutputStream`] for an input stream,
    /// [`AudioError::StreamNotRunning`] when the stream is not running, or a
    /// decode error for malformed PCM.
    pub fn submit_tx(
        &mut self,
        device_id: u32,
        stream_id: u32,
        data: &[u8],
    ) -> Result<usize, AudioError> {
        let device = self
            .device_mut(device_id)
            .ok_or(AudioError::UnknownDevice(device_id))?;
        let frames = device.submit_tx_bytes(stream_id, data)?;
        let source = device
            .mixer_source(usize::try_from(stream_id).unwrap_or(usize::MAX))
            .ok_or(AudioError::NotOutputStream(stream_id))?;
        let samples = device.drain_tx(usize::try_from(stream_id).unwrap_or(usize::MAX));
        self.mixer.push_samples(source, &samples)?;
        Ok(frames)
    }

    /// Read captured PCM from a device input stream's RX buffer.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownDevice`] for an unknown device,
    /// [`AudioError::UnknownStream`] for an unknown stream, or
    /// [`AudioError::NotInputStream`] for an output stream.
    pub fn read_rx(
        &mut self,
        device_id: u32,
        stream_id: u32,
        max_frames: usize,
    ) -> Result<Vec<i16>, AudioError> {
        self.device_mut(device_id)
            .ok_or(AudioError::UnknownDevice(device_id))?
            .read_rx(stream_id, max_frames)
    }

    /// Set per-guest-stream playback volume (`0.0`..=`1.0`).
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownDevice`] for an unknown device or
    /// [`AudioError::UnknownStream`] for an unknown stream.
    pub fn set_volume(
        &mut self,
        device_id: u32,
        stream_id: u32,
        volume: f32,
    ) -> Result<(), AudioError> {
        let source = self
            .device(device_id)
            .ok_or(AudioError::UnknownDevice(device_id))?
            .mixer_source(usize::try_from(stream_id).unwrap_or(usize::MAX))
            .ok_or(AudioError::UnknownStream(stream_id))?;
        self.mixer.set_volume(source, volume)
    }

    /// Mute or unmute a guest stream in the mix.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownDevice`] for an unknown device or
    /// [`AudioError::UnknownStream`] for an unknown stream.
    pub fn set_mute(
        &mut self,
        device_id: u32,
        stream_id: u32,
        muted: bool,
    ) -> Result<(), AudioError> {
        let source = self
            .device(device_id)
            .ok_or(AudioError::UnknownDevice(device_id))?
            .mixer_source(usize::try_from(stream_id).unwrap_or(usize::MAX))
            .ok_or(AudioError::UnknownStream(stream_id))?;
        self.mixer.set_mute(source, muted)
    }

    /// Feed host-captured (microphone) stereo 48 kHz PCM into every input
    /// stream's RX buffer. Per-guest mic routing is a 1.1 feature; for 1.0
    /// the capture is broadcast.
    pub fn push_capture(&mut self, samples: &[i16]) {
        for device in &mut self.devices {
            device.push_capture(samples);
        }
    }

    /// Render one 10 ms period of mixed audio to `backend`.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::BackendWrite`] when the backend does not accept
    /// the full period.
    pub fn tick(&mut self, backend: &mut dyn AudioBackend) -> Result<TickStats, AudioError> {
        self.mixer.tick(backend)
    }
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `SND_R_PCM_SET_PARAMS` request bytes: stream id + params.
    fn set_params_payload(stream_id: u32, channels: u8, format: u8, rate: u8) -> Vec<u8> {
        let mut p = Vec::with_capacity(20);
        p.extend_from_slice(&stream_id.to_le_bytes());
        p.extend_from_slice(&8192u32.to_le_bytes()); // buffer_bytes
        p.extend_from_slice(&1920u32.to_le_bytes()); // period_bytes
        p.extend_from_slice(&0u32.to_le_bytes()); // features
        p.push(channels);
        p.push(format);
        p.push(rate);
        p.push(0); // pad
        p
    }

    /// Build a `SND_R_*_INFO` query payload.
    fn query_payload(start_id: u32, count: u32) -> Vec<u8> {
        let mut p = Vec::with_capacity(8);
        p.extend_from_slice(&start_id.to_le_bytes());
        p.extend_from_slice(&count.to_le_bytes());
        p
    }

    /// Bring stream 0 of `device` to the running state (`S16_LE` stereo 48 kHz).
    fn start_output_stream(device: &mut VirtioSoundDevice) {
        let params = set_params_payload(0, 2, SND_FMT_S16_LE, 7);
        assert_eq!(
            device.handle_control(SND_R_PCM_SET_PARAMS, &params).status,
            SND_S_OK
        );
        let hdr = 0u32.to_le_bytes();
        assert_eq!(
            device.handle_control(SND_R_PCM_PREPARE, &hdr).status,
            SND_S_OK
        );
        assert_eq!(
            device.handle_control(SND_R_PCM_START, &hdr).status,
            SND_S_OK
        );
    }

    /// Engine with one device whose output stream is running.
    fn running_engine() -> (AudioEngine, u32) {
        let mut engine = AudioEngine::new();
        let dev = engine.add_device("guest-0");
        let params = set_params_payload(0, 2, SND_FMT_S16_LE, 7);
        assert_eq!(
            engine.control(dev, SND_R_PCM_SET_PARAMS, &params).status,
            SND_S_OK
        );
        let hdr = 0u32.to_le_bytes();
        assert_eq!(
            engine.control(dev, SND_R_PCM_PREPARE, &hdr).status,
            SND_S_OK
        );
        assert_eq!(engine.control(dev, SND_R_PCM_START, &hdr).status, SND_S_OK);
        (engine, dev)
    }

    /// Encode `frames` stereo `i16` samples as `S16_LE` bytes.
    fn s16le_bytes(samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn device_id_and_config_space() {
        let device = VirtioSoundDevice::new("guest-0");
        assert_eq!(device.device_id(), VIRTIO_SND_DEVICE_ID);
        assert_eq!(device.device_id(), 25);
        let cfg = device.config_bytes();
        assert_eq!(cfg.len(), SndConfig::ENCODED_LEN);
        let u32_at = |o: usize| u32::from_le_bytes([cfg[o], cfg[o + 1], cfg[o + 2], cfg[o + 3]]);
        assert_eq!(u32_at(0), 1); // jacks
        assert_eq!(u32_at(4), 2); // streams
        assert_eq!(u32_at(8), 1); // chmaps
    }

    #[test]
    fn wire_struct_sizes_match_spec() {
        assert_eq!(SndConfig::ENCODED_LEN, 16);
        assert_eq!(PcmStreamInfo::ENCODED_LEN, 32);
        assert_eq!(JackInfo::ENCODED_LEN, 32);
        assert_eq!(ChmapInfo::ENCODED_LEN, 23);
        assert_eq!(PcmSetParams::ENCODED_LEN, 16);
        assert_eq!(QueryInfo::ENCODED_LEN, 8);
        assert_eq!(PcmHdr::ENCODED_LEN, 4);
        assert_eq!(SndEvent { code: 0, id: 0 }.to_le_bytes().len(), 8);
    }

    #[test]
    fn pcm_info_query_returns_both_streams() {
        let mut device = VirtioSoundDevice::new("guest-0");
        let resp = device.handle_control(SND_R_PCM_INFO, &query_payload(0, 8));
        assert_eq!(resp.status, SND_S_OK);
        assert_eq!(resp.payload.len(), 2 * PcmStreamInfo::ENCODED_LEN);
        // First stream is the output stream.
        assert_eq!(resp.payload[24], SND_D_OUTPUT);
        // Second stream is the input stream.
        assert_eq!(resp.payload[32 + 24], SND_D_INPUT);
        // Both advertise S16_LE and 48 kHz.
        let formats = u64::from_le_bytes(resp.payload[8..16].try_into().unwrap());
        assert_ne!(formats & (1u64 << SND_FMT_S16_LE), 0);
        let rates = u64::from_le_bytes(resp.payload[16..24].try_into().unwrap());
        assert_ne!(rates & (1u64 << 7), 0); // 48 kHz is rate index 7
    }

    #[test]
    fn info_query_pagination_and_bounds() {
        let mut device = VirtioSoundDevice::new("guest-0");
        // Second entry only.
        let resp = device.handle_control(SND_R_PCM_INFO, &query_payload(1, 8));
        assert_eq!(resp.status, SND_S_OK);
        assert_eq!(resp.payload.len(), PcmStreamInfo::ENCODED_LEN);
        assert_eq!(resp.payload[24], SND_D_INPUT);
        // Start past the end is malformed.
        let resp = device.handle_control(SND_R_PCM_INFO, &query_payload(9, 8));
        assert_eq!(resp.status, SND_S_BAD_MSG);
        // Short query is malformed.
        let resp = device.handle_control(SND_R_PCM_INFO, &[1, 2, 3]);
        assert_eq!(resp.status, SND_S_BAD_MSG);
        // Jack and chmap queries work too.
        let resp = device.handle_control(SND_R_JACK_INFO, &query_payload(0, 4));
        assert_eq!(resp.status, SND_S_OK);
        assert_eq!(resp.payload.len(), JackInfo::ENCODED_LEN);
        let resp = device.handle_control(SND_R_CHMAP_INFO, &query_payload(0, 4));
        assert_eq!(resp.status, SND_S_OK);
        assert_eq!(resp.payload.len(), ChmapInfo::ENCODED_LEN);
        // Stereo chmap carries front-left/front-right.
        assert_eq!(resp.payload[4], 2);
        assert_eq!(resp.payload[5], CHMAP_FL);
        assert_eq!(resp.payload[6], CHMAP_FR);
    }

    #[test]
    fn control_state_machine() {
        let mut device = VirtioSoundDevice::new("guest-0");
        let hdr = 0u32.to_le_bytes();
        // Start without prepare is an I/O error.
        assert_eq!(
            device.handle_control(SND_R_PCM_START, &hdr).status,
            SND_S_IO_ERR
        );
        // Prepare without params is an I/O error.
        assert_eq!(
            device.handle_control(SND_R_PCM_PREPARE, &hdr).status,
            SND_S_IO_ERR
        );
        start_output_stream(&mut device);
        // Start emitted a PCM event.
        let event = device.pop_event().expect("expected start event");
        assert_eq!(event.code, SND_EVT_PCM);
        assert_eq!(event.id, 0);
        // Pause/unpause round-trip.
        assert_eq!(
            device.handle_control(SND_R_PCM_PAUSE, &hdr).status,
            SND_S_OK
        );
        assert_eq!(
            device.handle_control(SND_R_PCM_UNPAUSE, &hdr).status,
            SND_S_OK
        );
        // Stop then release; start afterwards fails (not prepared).
        assert_eq!(device.handle_control(SND_R_PCM_STOP, &hdr).status, SND_S_OK);
        assert_eq!(
            device.handle_control(SND_R_PCM_RELEASE, &hdr).status,
            SND_S_OK
        );
        assert_eq!(
            device.handle_control(SND_R_PCM_START, &hdr).status,
            SND_S_IO_ERR
        );
    }

    #[test]
    fn control_rejects_bad_requests() {
        let mut device = VirtioSoundDevice::new("guest-0");
        // Unknown request code.
        assert_eq!(device.handle_control(0x9999, &[]).status, SND_S_BAD_MSG);
        // Jack remap is not supported in 1.0.
        assert_eq!(
            device.handle_control(SND_R_JACK_REMAP, &[]).status,
            SND_S_NOT_SUPP
        );
        // XRUN is device-to-guest only.
        assert_eq!(
            device.handle_control(SND_R_PCM_XRUN, &[]).status,
            SND_S_BAD_MSG
        );
        // Unknown stream id.
        let hdr = 42u32.to_le_bytes();
        assert_eq!(
            device.handle_control(SND_R_PCM_PREPARE, &hdr).status,
            SND_S_BAD_MSG
        );
        // Short set-params payload.
        assert_eq!(
            device.handle_control(SND_R_PCM_SET_PARAMS, &[1, 2]).status,
            SND_S_BAD_MSG
        );
    }

    #[test]
    fn set_params_validation() {
        let mut device = VirtioSoundDevice::new("guest-0");
        // Unsupported format (mu-law is not advertised).
        let bad_fmt = set_params_payload(0, 2, SND_FMT_MU_LAW, 7);
        assert_eq!(
            device.handle_control(SND_R_PCM_SET_PARAMS, &bad_fmt).status,
            SND_S_NOT_SUPP
        );
        // Unsupported rate index.
        let bad_rate = set_params_payload(0, 2, SND_FMT_S16_LE, 14);
        assert_eq!(
            device
                .handle_control(SND_R_PCM_SET_PARAMS, &bad_rate)
                .status,
            SND_S_NOT_SUPP
        );
        // Channel count out of range.
        let bad_ch = set_params_payload(0, 6, SND_FMT_S16_LE, 7);
        assert_eq!(
            device.handle_control(SND_R_PCM_SET_PARAMS, &bad_ch).status,
            SND_S_NOT_SUPP
        );
        // Zero period is malformed.
        let mut zero_period = set_params_payload(0, 2, SND_FMT_S16_LE, 7);
        zero_period[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            device
                .handle_control(SND_R_PCM_SET_PARAMS, &zero_period)
                .status,
            SND_S_BAD_MSG
        );
    }

    #[test]
    fn pcm_format_decoding() {
        // S8 / U8.
        assert_eq!(decode_pcm_frames(SND_FMT_S8, 1, &[0x00]).unwrap(), vec![0]);
        assert_eq!(
            decode_pcm_frames(SND_FMT_S8, 1, &[0x80]).unwrap(),
            vec![-32768]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_S8, 1, &[0x7F]).unwrap(),
            vec![32512]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_U8, 1, &[0x00]).unwrap(),
            vec![-32768]
        );
        assert_eq!(decode_pcm_frames(SND_FMT_U8, 1, &[0x80]).unwrap(), vec![0]);
        // S16 LE/BE and U16 LE.
        assert_eq!(
            decode_pcm_frames(SND_FMT_S16_LE, 1, &[0x00, 0x80]).unwrap(),
            vec![-32768]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_S16_LE, 1, &[0xFF, 0x7F]).unwrap(),
            vec![32767]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_S16_BE, 1, &[0x80, 0x00]).unwrap(),
            vec![-32768]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_U16_LE, 1, &[0x00, 0x80]).unwrap(),
            vec![0]
        );
        // S24 packed little-endian: 0x800000 -> -32768, 0x7FFFFF -> 32767.
        assert_eq!(
            decode_pcm_frames(SND_FMT_S24_LE, 1, &[0x00, 0x00, 0x80]).unwrap(),
            vec![-32768]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_S24_LE, 1, &[0xFF, 0xFF, 0x7F]).unwrap(),
            vec![32767]
        );
        // S32 LE: top 16 bits survive.
        assert_eq!(
            decode_pcm_frames(SND_FMT_S32_LE, 1, &[0x00, 0x00, 0x00, 0x80]).unwrap(),
            vec![-32768]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_S32_LE, 1, &[0xFF, 0xFF, 0xFF, 0x7F]).unwrap(),
            vec![32767]
        );
        // Float: 1.0 -> 32767, 0.5 -> 16384, -1.0 -> -32767.
        assert_eq!(
            decode_pcm_frames(SND_FMT_FLOAT_LE, 1, &1.0f32.to_le_bytes()).unwrap(),
            vec![32767]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_FLOAT_LE, 1, &0.5f32.to_le_bytes()).unwrap(),
            vec![16384]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_FLOAT_LE, 1, &(-1.0f32).to_le_bytes()).unwrap(),
            vec![-32767]
        );
        assert_eq!(
            decode_pcm_frames(SND_FMT_FLOAT64_LE, 1, &0.25f64.to_le_bytes()).unwrap(),
            vec![8192]
        );
        // Multi-channel interleaving is preserved.
        let stereo = decode_pcm_frames(SND_FMT_S16_LE, 2, &[0x01, 0x00, 0x02, 0x00]).unwrap();
        assert_eq!(stereo, vec![1, 2]);
        // Errors: unaligned data, unknown format.
        assert!(decode_pcm_frames(SND_FMT_S16_LE, 1, &[0x01, 0x02, 0x03]).is_err());
        assert_eq!(
            decode_pcm_frames(SND_FMT_IMA_ADPCM, 1, &[0x00]).unwrap_err(),
            AudioError::UnsupportedFormat(SND_FMT_IMA_ADPCM)
        );
    }

    #[test]
    fn tx_into_mixer_end_to_end() {
        let (mut engine, dev) = running_engine();
        // One 10 ms period of DC 10000, stereo S16_LE.
        let samples = vec![10_000i16; MIXER_PERIOD_FRAMES * 2];
        let frames = engine.submit_tx(dev, 0, &s16le_bytes(&samples)).unwrap();
        assert_eq!(frames, MIXER_PERIOD_FRAMES);

        let mut backend = VecAudioBackend::new();
        let stats = engine.tick(&mut backend).unwrap();
        assert_eq!(stats.frames_rendered, MIXER_PERIOD_FRAMES);
        assert_eq!(stats.sources_mixed, 1);
        assert_eq!(stats.underrun_sources, 0);
        assert_eq!(backend.frames.len(), MIXER_PERIOD_FRAMES * 2);
        // Same-rate passthrough is bit-exact for DC.
        assert!(backend.frames.iter().all(|&s| s == 10_000));
    }

    #[test]
    fn tx_requires_running_stream() {
        let mut engine = AudioEngine::new();
        let dev = engine.add_device("guest-0");
        // Stream with no parameters at all: rejected, no xrun counted.
        let samples = vec![1000i16; 96];
        let err = engine
            .submit_tx(dev, 0, &s16le_bytes(&samples))
            .unwrap_err();
        assert_eq!(err, AudioError::StreamNotRunning(0));
        assert_eq!(engine.device(dev).unwrap().stats().tx_xruns, 0);
        // Configured and prepared but not started: TX is rejected and an
        // xrun is recorded.
        let params = set_params_payload(0, 2, SND_FMT_S16_LE, 7);
        assert_eq!(
            engine.control(dev, SND_R_PCM_SET_PARAMS, &params).status,
            SND_S_OK
        );
        let hdr = 0u32.to_le_bytes();
        assert_eq!(
            engine.control(dev, SND_R_PCM_PREPARE, &hdr).status,
            SND_S_OK
        );
        let err = engine
            .submit_tx(dev, 0, &s16le_bytes(&samples))
            .unwrap_err();
        assert_eq!(err, AudioError::StreamNotRunning(0));
        assert_eq!(engine.device(dev).unwrap().stats().tx_xruns, 1);
        // TX to the input stream is rejected.
        let err = engine
            .submit_tx(dev, 1, &s16le_bytes(&samples))
            .unwrap_err();
        assert!(matches!(
            err,
            AudioError::NotOutputStream(1) | AudioError::StreamNotRunning(1)
        ));
        // Unknown device.
        assert_eq!(
            engine.submit_tx(99, 0, &[]).unwrap_err(),
            AudioError::UnknownDevice(99)
        );
    }

    #[test]
    fn mixer_sums_two_guests_and_clips() {
        let (mut engine, dev0) = running_engine();
        let dev1 = engine.add_device("guest-1");
        let params = set_params_payload(0, 2, SND_FMT_S16_LE, 7);
        assert_eq!(
            engine.control(dev1, SND_R_PCM_SET_PARAMS, &params).status,
            SND_S_OK
        );
        let hdr = 0u32.to_le_bytes();
        assert_eq!(
            engine.control(dev1, SND_R_PCM_PREPARE, &hdr).status,
            SND_S_OK
        );
        assert_eq!(engine.control(dev1, SND_R_PCM_START, &hdr).status, SND_S_OK);

        let a = vec![10_000i16; MIXER_PERIOD_FRAMES * 2];
        let b = vec![5_000i16; MIXER_PERIOD_FRAMES * 2];
        engine.submit_tx(dev0, 0, &s16le_bytes(&a)).unwrap();
        engine.submit_tx(dev1, 0, &s16le_bytes(&b)).unwrap();

        let mut backend = VecAudioBackend::new();
        let stats = engine.tick(&mut backend).unwrap();
        assert_eq!(stats.sources_mixed, 2);
        assert!(backend.frames.iter().all(|&s| s == 15_000));

        // Clipping: 30000 + 30000 saturates at i16::MAX.
        let loud = vec![30_000i16; MIXER_PERIOD_FRAMES * 2];
        engine.submit_tx(dev0, 0, &s16le_bytes(&loud)).unwrap();
        engine.submit_tx(dev1, 0, &s16le_bytes(&loud)).unwrap();
        let mut backend = VecAudioBackend::new();
        engine.tick(&mut backend).unwrap();
        assert!(backend.frames.iter().all(|&s| s == i16::MAX));
    }

    #[test]
    fn mixer_resamples_dc_exactly() {
        // DC passes through linear resampling unchanged at any rate.
        let mut mixer = AudioMixer::new();
        for rate in [8000u32, 22050, 44100, 48000, 96000] {
            let id = mixer.add_source(SourceSpec {
                sample_rate: rate,
                channels: 2,
            });
            // One second of DC 7000.
            let n = rate as usize * 2;
            mixer.push_samples(id, &vec![7000i16; n]).unwrap();
            let mut backend = VecAudioBackend::new();
            let stats = mixer.tick(&mut backend).unwrap();
            assert_eq!(stats.frames_rendered, MIXER_PERIOD_FRAMES);
            assert!(backend.frames.iter().all(|&s| s == 7000), "rate {rate}");
            mixer.remove_source(id);
        }
    }

    #[test]
    fn mixer_maps_mono_to_stereo() {
        let mut mixer = AudioMixer::new();
        let id = mixer.add_source(SourceSpec {
            sample_rate: 48_000,
            channels: 1,
        });
        mixer.push_samples(id, &vec![1234i16; 480]).unwrap();
        let mut backend = VecAudioBackend::new();
        mixer.tick(&mut backend).unwrap();
        assert_eq!(backend.frames.len(), 960);
        for pair in backend.frames.as_chunks::<2>().0 {
            assert_eq!(pair, &[1234, 1234]);
        }
    }

    #[test]
    fn mixer_volume_and_mute() {
        let (mut engine, dev) = running_engine();
        let dc = vec![10_000i16; MIXER_PERIOD_FRAMES * 2];

        engine.set_volume(dev, 0, 0.5).unwrap();
        engine.submit_tx(dev, 0, &s16le_bytes(&dc)).unwrap();
        let mut backend = VecAudioBackend::new();
        engine.tick(&mut backend).unwrap();
        assert!(backend.frames.iter().all(|&s| s == 5000));

        engine.set_volume(dev, 0, 0.0).unwrap();
        engine.submit_tx(dev, 0, &s16le_bytes(&dc)).unwrap();
        let mut backend = VecAudioBackend::new();
        engine.tick(&mut backend).unwrap();
        assert!(backend.frames.iter().all(|&s| s == 0));

        engine.set_volume(dev, 0, 1.0).unwrap();
        engine.set_mute(dev, 0, true).unwrap();
        engine.submit_tx(dev, 0, &s16le_bytes(&dc)).unwrap();
        let mut backend = VecAudioBackend::new();
        let stats = engine.tick(&mut backend).unwrap();
        assert_eq!(stats.sources_mixed, 0);
        assert!(backend.frames.iter().all(|&s| s == 0));

        // Unknown ids are rejected.
        assert_eq!(
            engine.set_volume(99, 0, 1.0).unwrap_err(),
            AudioError::UnknownDevice(99)
        );
        assert_eq!(
            engine.set_volume(dev, 7, 1.0).unwrap_err(),
            AudioError::UnknownStream(7)
        );
    }

    #[test]
    fn mixer_reports_underruns_on_starved_source() {
        let mut mixer = AudioMixer::new();
        mixer.add_source(SourceSpec {
            sample_rate: 48_000,
            channels: 2,
        });
        let mut backend = NullAudioBackend::new();
        let stats = mixer.tick(&mut backend).unwrap();
        assert_eq!(stats.underrun_sources, 1);
        assert_eq!(backend.frames_written, MIXER_PERIOD_FRAMES as u64);
    }

    #[test]
    fn mixer_tick_stays_within_latency_budget() {
        // 10 ms of audio per tick must render in well under 10 ms of
        // wall-clock time; this is the <10 ms output-latency requirement.
        let mut mixer = AudioMixer::new();
        for rate in [8000u32, 32000, 44100, 48000] {
            let id = mixer.add_source(SourceSpec {
                sample_rate: rate,
                channels: 2,
            });
            mixer
                .push_samples(id, &vec![1000i16; rate as usize * 2 * 2])
                .unwrap();
        }
        let mut backend = NullAudioBackend::new();
        for _ in 0..20 {
            let stats = mixer.tick(&mut backend).unwrap();
            assert_eq!(stats.frames_rendered, MIXER_PERIOD_FRAMES);
            assert!(
                stats.elapsed < Duration::from_millis(10),
                "tick took {:?}, exceeding the 10 ms budget",
                stats.elapsed
            );
        }
    }

    #[test]
    fn rx_capture_broadcast_to_input_streams() {
        let mut engine = AudioEngine::new();
        let dev = engine.add_device("guest-0");
        engine.push_capture(&[1, 2, 3, 4, 5, 6]);
        let got = engine.read_rx(dev, 1, 10).unwrap();
        assert_eq!(got, vec![1, 2, 3, 4, 5, 6]);
        // Buffer drained by the read.
        let got = engine.read_rx(dev, 1, 10).unwrap();
        assert!(got.is_empty());
        // Reading from the output stream is rejected.
        assert_eq!(
            engine.read_rx(dev, 0, 10).unwrap_err(),
            AudioError::NotInputStream(0)
        );
        let device = engine.device(dev).unwrap();
        assert_eq!(device.stats().rx_frames, 3);
    }

    #[test]
    fn engine_rejects_unknown_device_control() {
        let mut engine = AudioEngine::new();
        let resp = engine.control(77, SND_R_PCM_INFO, &query_payload(0, 4));
        assert_eq!(resp.status, SND_S_BAD_MSG);
    }

    #[test]
    fn null_backend_counts_frames() {
        let mut backend = NullAudioBackend::new();
        assert_eq!(backend.sample_rate(), MIXER_OUTPUT_RATE_HZ);
        assert_eq!(backend.channels(), MIXER_OUTPUT_CHANNELS);
        assert_eq!(backend.write_frames(&[1, 2, 3, 4]), 2);
        assert_eq!(backend.frames_written, 2);
    }
}

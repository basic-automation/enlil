//! HDA codec emulation
//!
//! Emulates a Realtek ALC892-compatible codec with output and input widgets.
//! Windows loads the inbox HD Audio class driver for this.

use crate::truncate::u32_of;
/// Codec address on the HDA link
pub const CODEC_ADDRESS: u8 = 0;

/// Standard HDA widget types
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetType {
    AudioOutput = 0x0,
    AudioInput = 0x1,
    AudioMixer = 0x2,
    AudioSelector = 0x3,
    PinComplex = 0x4,
    PowerWidget = 0x5,
    VolumeKnob = 0x6,
    BeepGenerator = 0x7,
    VendorDefined = 0xF,
}

/// HDA codec node/widget
#[derive(Debug, Clone)]
pub struct HdaWidget {
    /// Node ID (NID)
    pub nid: u8,
    /// Widget type
    pub widget_type: WidgetType,
    /// Widget capabilities
    pub capabilities: u32,
    /// Pin configuration default (for pin complexes)
    pub pin_config: u32,
    /// Connection list
    pub connections: Vec<u8>,
    /// Current gain/mute state
    pub amp_gain: u16,
    /// Stream and channel assignment
    pub stream_channel: u8,
    /// Format
    pub format: u16,
    /// Pin Widget Control (verb 0x707/0xF07): output/input/HP-amp enables and
    /// `VRef`. Reset 0 (disabled); the guest driver programs it.
    pub pin_control: u8,
    /// Power State (verb 0x705/0xF05): D0..D3. Reset 0 (D0).
    pub power_state: u8,
}

/// HDA Codec state
#[derive(Debug, Clone)]
pub struct HdaCodec {
    /// Codec vendor/device ID
    pub vendor_id: u32,
    /// Revision ID
    pub revision_id: u32,
    /// Subordinate node count (function group)
    pub subordinate_node_count: u16,
    /// Starting node ID
    pub start_nid: u8,
    /// Widgets
    pub widgets: Vec<HdaWidget>,
}

impl HdaCodec {
    /// Create a Realtek ALC892-like codec
    #[must_use]
    pub fn new_realtek() -> Self {
        let widgets = vec![
            // NID 0x02: Audio Output (DAC)
            HdaWidget {
                nid: 0x02,
                widget_type: WidgetType::AudioOutput,
                capabilities: 0x0001_0041, // Stereo, Digital
                pin_config: 0,
                connections: Vec::new(),
                amp_gain: 0x7F,
                stream_channel: 0,
                format: 0x0011, // 48kHz 16-bit stereo
                pin_control: 0,
                power_state: 0,
            },
            // NID 0x03: Audio Output (DAC) - secondary
            HdaWidget {
                nid: 0x03,
                widget_type: WidgetType::AudioOutput,
                capabilities: 0x0001_0041,
                pin_config: 0,
                connections: Vec::new(),
                amp_gain: 0x7F,
                stream_channel: 0,
                format: 0x0011,
                pin_control: 0,
                power_state: 0,
            },
            // NID 0x08: Audio Input (ADC)
            HdaWidget {
                nid: 0x08,
                widget_type: WidgetType::AudioInput,
                capabilities: 0x0010_0341, // Stereo, In Amp
                pin_config: 0,
                connections: vec![0x18, 0x19],
                amp_gain: 0x7F,
                stream_channel: 0,
                format: 0x0011,
                pin_control: 0,
                power_state: 0,
            },
            // NID 0x14: Pin Complex (Line Out)
            HdaWidget {
                nid: 0x14,
                widget_type: WidgetType::PinComplex,
                capabilities: 0x0040_0000, // Output capable
                pin_config: 0x0121_4010,   // Line out, front, jack
                connections: vec![0x02],
                amp_gain: 0,
                stream_channel: 0,
                format: 0,
                pin_control: 0,
                power_state: 0,
            },
            // NID 0x18: Pin Complex (Mic In)
            HdaWidget {
                nid: 0x18,
                widget_type: WidgetType::PinComplex,
                capabilities: 0x0020_0000, // Input capable
                pin_config: 0x01A1_9030,   // Mic in, front, jack
                connections: Vec::new(),
                amp_gain: 0,
                stream_channel: 0,
                format: 0,
                pin_control: 0,
                power_state: 0,
            },
            // NID 0x19: Pin Complex (Front Mic)
            HdaWidget {
                nid: 0x19,
                widget_type: WidgetType::PinComplex,
                capabilities: 0x0020_0000,
                pin_config: 0x02A1_9040, // Mic in, front, jack
                connections: Vec::new(),
                amp_gain: 0,
                stream_channel: 0,
                format: 0,
                pin_control: 0,
                power_state: 0,
            },
        ];

        Self {
            vendor_id: 0x10EC_0892, // Realtek ALC892
            revision_id: 0x0010_0302,
            subordinate_node_count: 0x0001, // 1 function group
            start_nid: 0x01,
            widgets,
        }
    }

    /// Root node (NID 0) `GET_PARAMETER` responses.
    #[must_use]
    const fn root_param(&self, param: u32) -> u32 {
        match param {
            0x00 => self.vendor_id,   // Vendor ID
            0x02 => self.revision_id, // Revision ID
            0x04 => 0x0001_0001,      // Subordinate node count
            _ => 0,
        }
    }

    /// Audio Function Group (NID 1) `GET_PARAMETER` responses.
    #[must_use]
    fn afg_param(&self, param: u32) -> u32 {
        match param {
            // Node count: start NID (0x02) in [23:16] and widget count in [7:0].
            0x04 => (0x02 << 16) | u32_of(self.widgets.len()),
            // Function group type (Audio), supported PCM sizes/rates, and
            // supported stream formats all report 1 here.
            0x05 | 0x08 | 0x09 => 0x01,
            0x0A => 0x0001_0041, // Audio widget capabilities (for the group)
            _ => 0,
        }
    }

    /// Process a codec verb and return the response
    #[must_use]
    pub fn process_verb(&mut self, verb: u32) -> u32 {
        // bits 28-31 are the codec address (single codec here, so ignored)
        let nid = ((verb >> 20) & 0x7F) as u8;
        let payload = verb & 0xFFFFF;

        // Get verb ID — HDA uses two encodings (HDA spec §7.3.3):
        // - 4-bit verb [19:16] + 16-bit payload [15:0]: ONLY 0x2 (Set Converter
        //   Format) and 0x3 (Set Amplifier Gain/Mute).
        // - 12-bit verb [19:8] + 8-bit payload [7:0]: everything else, including
        //   the 0x4xx-0x7xx Set verbs (e.g. 0x705 Set Power State, 0x706 Set
        //   Stream/Channel, 0x707 Set Pin Widget Control) and the 0xF.. Gets.
        // The decode must key on exactly {0x2, 0x3}: the old `1..=7` heuristic
        // mis-classified every 0x4xx-0x7xx Set verb as a 4-bit verb 0x7, so those
        // sets were silently dropped (their handlers below were unreachable).
        let verb_high = (payload >> 16) & 0xF;
        let verb_id = if verb_high == 0x2 || verb_high == 0x3 {
            // 4-bit verb + 16-bit payload
            verb_high
        } else {
            // 12-bit verb + 8-bit payload
            (payload >> 8) & 0xFFF
        };

        match (nid, verb_id) {
            // Root node (NID 0) and Audio Function Group (NID 1) parameters.
            (0x00, 0xF00) => self.root_param(payload & 0xFF),
            (0x01, 0xF00) => self.afg_param(payload & 0xFF),
            // Widget parameter query
            (nid, 0xF00) => {
                let param = payload & 0xFF;
                self.widgets
                    .iter()
                    .find(|w| w.nid == nid)
                    .map_or(0, |w| match param {
                        0x09 => w.capabilities,
                        0x0C => w.pin_config,
                        // Connection list length
                        0x0E => u32_of(w.connections.len()),
                        _ => 0,
                    })
            }
            // Get connection list entry
            (nid, 0xF02) => self.widgets.iter().find(|w| w.nid == nid).map_or(0, |w| {
                let idx = (payload & 0xFF) as usize;
                if idx < w.connections.len() {
                    u32::from(w.connections[idx])
                } else {
                    0
                }
            }),
            // Get/Set converter stream/channel
            (nid, 0xF06) => self
                .widgets
                .iter()
                .find(|w| w.nid == nid)
                .map_or(0, |w| u32::from(w.stream_channel)),
            (nid, 0x706) => {
                if let Some(w) = self.widgets.iter_mut().find(|w| w.nid == nid) {
                    w.stream_channel = (payload & 0xFF) as u8;
                }
                0
            }
            // Get Converter Format (12-bit verb 0xA) — the 16-bit stream format
            // the driver programs alongside the stream/channel on stream setup.
            (nid, 0x00A) => self
                .widgets
                .iter()
                .find(|w| w.nid == nid)
                .map_or(0, |w| u32::from(w.format)),
            // Set Converter Format (4-bit verb 0x2, 16-bit format payload).
            (nid, 0x2) => {
                if let Some(w) = self.widgets.iter_mut().find(|w| w.nid == nid) {
                    w.format = (payload & 0xFFFF) as u16;
                }
                0
            }
            // Get Pin Widget Control — return what the guest programmed.
            (nid, 0xF07) => self
                .widgets
                .iter()
                .find(|w| w.nid == nid)
                .map_or(0, |w| u32::from(w.pin_control)),
            // Set Pin Widget Control — store the output/input/HP enables + VRef.
            (nid, 0x707) => {
                if let Some(w) = self.widgets.iter_mut().find(|w| w.nid == nid) {
                    w.pin_control = (payload & 0xFF) as u8;
                }
                0
            }
            // Get Power State — PS-Act [7:4] and PS-Set [3:0]; we settle instantly
            // so the actual state always equals the requested one.
            (nid, 0xF05) => self.widgets.iter().find(|w| w.nid == nid).map_or(0, |w| {
                let ps = u32::from(w.power_state);
                (ps << 4) | ps
            }),
            // Set Power State — store D0..D3 from the low nibble.
            (nid, 0x705) => {
                if let Some(w) = self.widgets.iter_mut().find(|w| w.nid == nid) {
                    w.power_state = (payload & 0x0F) as u8;
                }
                0
            }
            // Get/Set amplifier gain/mute
            (_, 0xB | 0x3) => 0x7F, // Max gain, unmuted
            // Config default
            (nid, 0xF1C) => self
                .widgets
                .iter()
                .find(|w| w.nid == nid)
                .map_or(0, |w| w.pin_config),
            _ => 0,
        }
    }
}

impl Default for HdaCodec {
    fn default() -> Self {
        Self::new_realtek()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_creates() {
        let codec = HdaCodec::new_realtek();
        assert_eq!(codec.vendor_id, 0x10EC_0892);
        assert!(!codec.widgets.is_empty());
    }

    #[test]
    fn vendor_id_query() {
        let mut codec = HdaCodec::new_realtek();
        // Verb: NID=0, VerbID=F00, Param=00 (Vendor ID)
        let verb = 0x000F_0000;
        let response = codec.process_verb(verb);
        assert_eq!(response, 0x10EC_0892);
    }

    #[test]
    fn pin_config_query() {
        let mut codec = HdaCodec::new_realtek();
        // Query NID 0x14 pin config default
        let verb = (0x14u32 << 20) | 0x000F_1C00;
        let response = codec.process_verb(verb);
        assert_ne!(response, 0, "Pin config should be non-zero for line out");
    }

    /// Set/Get Pin Widget Control (the 12-bit 0x707/0xF07 verbs) round-trips per
    /// widget — previously the decoder mis-classified 0x707 as a 4-bit verb and
    /// dropped it, returning a hardcoded 0x40 to every Get.
    #[test]
    fn pin_widget_control_round_trips() {
        let mut codec = HdaCodec::new_realtek();
        let nid = 0x14u32; // Line Out pin complex
        // Reset value is 0 (disabled), not the old hardcoded 0x40.
        assert_eq!(codec.process_verb((nid << 20) | 0x000F_0700), 0);
        // Set Pin Widget Control = 0x40 (OUT enable).
        assert_eq!(codec.process_verb((nid << 20) | 0x0007_0740), 0);
        // Get returns exactly what was programmed.
        assert_eq!(codec.process_verb((nid << 20) | 0x000F_0700), 0x40);
        // A different pin is independent.
        assert_eq!(codec.process_verb((0x18u32 << 20) | 0x000F_0700), 0);
    }

    /// Set/Get Power State (0x705/0xF05) round-trips; Get reports the settled
    /// actual state in [7:4] and the set state in [3:0].
    #[test]
    fn power_state_round_trips() {
        let mut codec = HdaCodec::new_realtek();
        let nid = 0x02u32; // a DAC
        assert_eq!(
            codec.process_verb((nid << 20) | 0x000F_0500),
            0x00,
            "reset D0"
        );
        // Set Power State D3.
        assert_eq!(codec.process_verb((nid << 20) | 0x0007_0503), 0);
        // PS-Act and PS-Set both D3 -> 0x33.
        assert_eq!(codec.process_verb((nid << 20) | 0x000F_0500), 0x33);
    }

    /// The verb decoder now routes 12-bit Set verbs (0x4xx-0x7xx) to their
    /// handlers instead of collapsing them to a 4-bit verb 0x7: Set Converter
    /// Stream/Channel (0x706) actually updates the converter, and the 4-bit Set
    /// Amp verb (0x3) still decodes correctly.
    #[test]
    fn twelve_bit_set_verbs_decode_and_apply() {
        let mut codec = HdaCodec::new_realtek();
        let nid = 0x02u32;
        // Set Converter Stream/Channel = 0x10 (stream 1, channel 0).
        assert_eq!(codec.process_verb((nid << 20) | 0x0007_0610), 0);
        assert_eq!(
            codec.process_verb((nid << 20) | 0x000F_0600),
            0x10,
            "0x706 set reached its handler"
        );
        // The 4-bit Set Amp Gain verb (0x3) is still classified as 4-bit and
        // returns the codec's amp response rather than being treated as 12-bit.
        assert_eq!(codec.process_verb((nid << 20) | 0x0003_B000), 0x7F);
    }

    /// Set/Get Converter Format round-trips: the 4-bit Set verb (0x2) stores the
    /// 16-bit stream format and the 12-bit Get verb (0xA) reads it back.
    #[test]
    fn converter_format_round_trips() {
        let mut codec = HdaCodec::new_realtek();
        let nid = 0x02u32; // a DAC, reset format 0x0011 (48 kHz/16-bit stereo)
        // Get Converter Format (verb 0xA -> bits[19:8] = 0x00A).
        assert_eq!(codec.process_verb((nid << 20) | 0x0000_0A00), 0x0011);
        // Set Converter Format (4-bit verb 0x2, 16-bit payload) = 0x4031.
        assert_eq!(codec.process_verb((nid << 20) | 0x0002_4031), 0);
        assert_eq!(codec.process_verb((nid << 20) | 0x0000_0A00), 0x4031);
    }
}

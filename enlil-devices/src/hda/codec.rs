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
        let mut widgets = Vec::new();

        // NID 0x02: Audio Output (DAC)
        widgets.push(HdaWidget {
            nid: 0x02,
            widget_type: WidgetType::AudioOutput,
            capabilities: 0x0001_0041, // Stereo, Digital
            pin_config: 0,
            connections: Vec::new(),
            amp_gain: 0x7F,
            stream_channel: 0,
            format: 0x0011, // 48kHz 16-bit stereo
        });

        // NID 0x03: Audio Output (DAC) - secondary
        widgets.push(HdaWidget {
            nid: 0x03,
            widget_type: WidgetType::AudioOutput,
            capabilities: 0x0001_0041,
            pin_config: 0,
            connections: Vec::new(),
            amp_gain: 0x7F,
            stream_channel: 0,
            format: 0x0011,
        });

        // NID 0x08: Audio Input (ADC)
        widgets.push(HdaWidget {
            nid: 0x08,
            widget_type: WidgetType::AudioInput,
            capabilities: 0x0010_0341, // Stereo, In Amp
            pin_config: 0,
            connections: vec![0x18, 0x19],
            amp_gain: 0x7F,
            stream_channel: 0,
            format: 0x0011,
        });

        // NID 0x14: Pin Complex (Line Out)
        widgets.push(HdaWidget {
            nid: 0x14,
            widget_type: WidgetType::PinComplex,
            capabilities: 0x0040_0000, // Output capable
            pin_config: 0x0121_4010,   // Line out, front, jack
            connections: vec![0x02],
            amp_gain: 0,
            stream_channel: 0,
            format: 0,
        });

        // NID 0x18: Pin Complex (Mic In)
        widgets.push(HdaWidget {
            nid: 0x18,
            widget_type: WidgetType::PinComplex,
            capabilities: 0x0020_0000, // Input capable
            pin_config: 0x01A1_9030,   // Mic in, front, jack
            connections: Vec::new(),
            amp_gain: 0,
            stream_channel: 0,
            format: 0,
        });

        // NID 0x19: Pin Complex (Front Mic)
        widgets.push(HdaWidget {
            nid: 0x19,
            widget_type: WidgetType::PinComplex,
            capabilities: 0x0020_0000,
            pin_config: 0x02A1_9040, // Mic in, front, jack
            connections: Vec::new(),
            amp_gain: 0,
            stream_channel: 0,
            format: 0,
        });

        Self {
            vendor_id: 0x10EC_0892, // Realtek ALC892
            revision_id: 0x0010_0302,
            subordinate_node_count: 0x0001, // 1 function group
            start_nid: 0x01,
            widgets,
        }
    }

    /// Process a codec verb and return the response
    #[must_use]
    pub fn process_verb(&mut self, verb: u32) -> u32 {
        // bits 28-31 are the codec address (single codec here, so ignored)
        let nid = ((verb >> 20) & 0x7F) as u8;
        let payload = verb & 0xFFFFF;

        // Get verb ID — HDA uses two encodings:
        // - 12-bit verb [19:8] + 8-bit param [7:0] for get/set verbs (0x200-0xFFF)
        // - 4-bit verb [19:16] + 16-bit param [15:0] for set verbs (0x1-0x7)
        let verb_high = (payload >> 16) & 0xF;
        let verb_id = if (1..=7).contains(&verb_high) {
            // 4-bit verb + 16-bit payload
            verb_high
        } else {
            // 12-bit verb + 8-bit payload
            (payload >> 8) & 0xFFF
        };

        match (nid, verb_id) {
            // Root node (NID 0): parameters
            (0x00, 0xF00) => {
                let param = payload & 0xFF;
                match param {
                    0x00 => self.vendor_id,   // Vendor ID
                    0x02 => self.revision_id, // Revision ID
                    0x04 => 0x0001_0001,      // Subordinate node count
                    _ => 0,
                }
            }
            // Audio Function Group (NID 1)
            (0x01, 0xF00) => {
                let param = payload & 0xFF;
                match param {
                    0x04 => {
                        // Node count: start NID and count
                        let count = u32_of(self.widgets.len());
                        (0x02 << 16) | count
                    }
                    0x05 => 0x01,        // Function Group Type: Audio
                    0x08 => 0x0001,      // Supported PCM sizes/rates
                    0x09 => 0x0001,      // Supported stream formats
                    0x0A => 0x0001_0041, // Audio widget capabilities (for the group)
                    _ => 0,
                }
            }
            // Widget parameter query
            (nid, 0xF00) => {
                let param = payload & 0xFF;
                if let Some(w) = self.widgets.iter().find(|w| w.nid == nid) {
                    match param {
                        0x09 => w.capabilities,
                        0x0C => w.pin_config,
                        0x0E => {
                            // Connection list length
                            u32_of(w.connections.len())
                        }
                        _ => 0,
                    }
                } else {
                    0
                }
            }
            // Get connection list entry
            (nid, 0xF02) => {
                if let Some(w) = self.widgets.iter().find(|w| w.nid == nid) {
                    let idx = (payload & 0xFF) as usize;
                    if idx < w.connections.len() {
                        u32::from(w.connections[idx])
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
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
            // Get/Set pin widget control
            (_, 0xF07 | 0x707) => 0x40, // OUT enabled
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
}

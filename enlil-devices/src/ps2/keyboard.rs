//! PS/2 keyboard device emulation
//!
//! Implements scancode set 2 translation and standard keyboard commands
//! (reset, set LEDs, identify, enable/disable scanning).

use std::collections::VecDeque;

/// Keyboard commands (sent by guest)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardCommand {
    /// Set LEDs (scroll/num/caps lock)
    SetLeds = 0xED,
    /// Echo (diagnostic)
    Echo = 0xEE,
    /// Set scancode set
    SetScancodeSet = 0xF0,
    /// Identify keyboard
    Identify = 0xF2,
    /// Set typematic rate/delay
    SetTypematic = 0xF3,
    /// Enable scanning
    EnableScan = 0xF4,
    /// Disable scanning
    DisableScan = 0xF5,
    /// Set defaults
    SetDefaults = 0xF6,
    /// Resend last byte
    Resend = 0xFE,
    /// Reset and self-test
    Reset = 0xFF,
}

/// PS/2 keyboard acknowledgment
const ACK: u8 = 0xFA;
/// Self-test passed
const SELF_TEST_PASSED: u8 = 0xAA;

/// PS/2 Keyboard device
#[derive(Debug, Clone)]
pub struct Ps2Keyboard {
    /// Output queue (scancodes to be read by the controller)
    output_queue: VecDeque<u8>,
    /// Whether scanning is enabled
    scanning_enabled: bool,
    /// Current LED state (bits: 0=scroll, 1=num, 2=caps)
    led_state: u8,
    /// Waiting for data byte (after a command that expects one)
    awaiting_data_for: Option<u8>,
    /// Current scancode set (1, 2, or 3)
    scancode_set: u8,
}

impl Ps2Keyboard {
    #[must_use]
    pub fn new() -> Self {
        Self {
            output_queue: VecDeque::with_capacity(16),
            scanning_enabled: true,
            led_state: 0,
            awaiting_data_for: None,
            scancode_set: 2,
        }
    }

    /// Process a command byte from the controller. Returns an immediate response byte if any.
    pub fn receive_command(&mut self, data: u8) -> Option<u8> {
        // If we're waiting for a data byte from a previous command
        if let Some(cmd) = self.awaiting_data_for.take() {
            return Some(self.handle_data_byte(cmd, data));
        }

        match data {
            0xED => {
                // Set LEDs — needs a data byte
                self.awaiting_data_for = Some(data);
                Some(ACK)
            }
            0xEE => {
                // Echo
                Some(0xEE)
            }
            0xF0 => {
                // Get/set scancode set — needs data byte
                self.awaiting_data_for = Some(data);
                Some(ACK)
            }
            0xF2 => {
                // Identify — respond with ACK then keyboard ID
                self.output_queue.push_back(0xAB);
                self.output_queue.push_back(0x83); // MF2 keyboard
                Some(ACK)
            }
            0xF3 => {
                // Set typematic — needs data byte
                self.awaiting_data_for = Some(data);
                Some(ACK)
            }
            0xF4 => {
                // Enable scanning
                self.scanning_enabled = true;
                Some(ACK)
            }
            0xF5 => {
                // Disable scanning
                self.scanning_enabled = false;
                Some(ACK)
            }
            0xF6 => {
                // Set defaults
                self.scanning_enabled = true;
                self.scancode_set = 2;
                Some(ACK)
            }
            0xFE => {
                // Resend — we don't track last byte, just ACK
                Some(ACK)
            }
            0xFF => {
                // Reset
                self.scanning_enabled = false;
                self.output_queue.push_back(SELF_TEST_PASSED);
                Some(ACK)
            }
            _ => {
                // Unknown command
                Some(ACK)
            }
        }
    }

    /// Handle a data byte following a command
    fn handle_data_byte(&mut self, cmd: u8, data: u8) -> u8 {
        match cmd {
            0xED => {
                self.led_state = data & 0x07;
            }
            0xF0 => {
                if data == 0 {
                    // Get current scancode set
                    self.output_queue.push_back(self.scancode_set);
                } else {
                    self.scancode_set = data.clamp(1, 3);
                }
            }
            // 0xF3 (typematic rate) and any other command: just acknowledge.
            _ => {}
        }
        ACK
    }

    /// Inject a scancode from the host (set 1 make/break code)
    pub fn inject_scancode(&mut self, scancode: u8) {
        if self.scanning_enabled {
            self.output_queue.push_back(scancode);
        }
    }

    /// Dequeue the next scancode byte
    #[must_use]
    pub fn dequeue_scancode(&mut self) -> Option<u8> {
        self.output_queue.pop_front()
    }

    /// Check if there's data waiting
    #[must_use]
    pub fn has_data(&self) -> bool {
        !self.output_queue.is_empty()
    }
}

impl Default for Ps2Keyboard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_returns_ack_then_self_test() {
        let mut kb = Ps2Keyboard::new();
        let ack = kb.receive_command(0xFF);
        assert_eq!(ack, Some(ACK));
        assert_eq!(kb.dequeue_scancode(), Some(SELF_TEST_PASSED));
    }

    #[test]
    fn identify_returns_keyboard_id() {
        let mut kb = Ps2Keyboard::new();
        let ack = kb.receive_command(0xF2);
        assert_eq!(ack, Some(ACK));
        assert_eq!(kb.dequeue_scancode(), Some(0xAB));
        assert_eq!(kb.dequeue_scancode(), Some(0x83));
    }

    #[test]
    fn echo_returns_echo() {
        let mut kb = Ps2Keyboard::new();
        assert_eq!(kb.receive_command(0xEE), Some(0xEE));
    }

    #[test]
    fn disable_scanning_blocks_scancodes() {
        let mut kb = Ps2Keyboard::new();
        kb.receive_command(0xF5); // Disable
        kb.inject_scancode(0x1E);
        assert!(!kb.has_data());
    }

    #[test]
    fn enable_scanning_allows_scancodes() {
        let mut kb = Ps2Keyboard::new();
        kb.receive_command(0xF4); // Enable
        kb.inject_scancode(0x1E);
        assert!(kb.has_data());
        assert_eq!(kb.dequeue_scancode(), Some(0x1E));
    }

    #[test]
    fn set_leds() {
        let mut kb = Ps2Keyboard::new();
        assert_eq!(kb.receive_command(0xED), Some(ACK));
        assert_eq!(kb.handle_data_byte(0xED, 0x07), ACK);
        assert_eq!(kb.led_state, 0x07);
    }

    #[test]
    fn get_scancode_set() {
        let mut kb = Ps2Keyboard::new();
        assert_eq!(kb.receive_command(0xF0), Some(ACK));
        // Send 0 to query current set
        kb.handle_data_byte(0xF0, 0);
        assert_eq!(kb.dequeue_scancode(), Some(2)); // Default is set 2
    }
}

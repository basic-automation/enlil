//! PS/2 mouse emulation
//!
//! Standard 3-button PS/2 mouse with scroll wheel (Intellimouse protocol).
//! Used early in Windows boot before USB drivers load.

use std::collections::VecDeque;

/// PS/2 mouse state
#[derive(Debug, Clone)]
pub struct Ps2Mouse {
    /// Mouse ID (0 = standard, 3 = Intellimouse with scroll)
    mouse_id: u8,
    /// Sample rate
    sample_rate: u8,
    /// Resolution
    resolution: u8,
    /// Scaling (1:1 or 2:1)
    scaling_2to1: bool,
    /// Reporting enabled
    reporting_enabled: bool,
    /// Output queue
    output_queue: VecDeque<u8>,
    /// Current command state (for multi-byte commands)
    expecting_data: Option<MouseCommand>,
    /// Button state
    buttons: u8,
    /// Intellimouse detection sequence counter
    intellimouse_seq: u8,
}

/// Mouse commands from host
#[derive(Debug, Clone, Copy)]
enum MouseCommand {
    SetSampleRate,
    SetResolution,
}

impl Ps2Mouse {
    #[must_use]
    pub fn new() -> Self {
        Self {
            mouse_id: 0,
            sample_rate: 100,
            resolution: 2,
            scaling_2to1: false,
            reporting_enabled: false,
            output_queue: VecDeque::with_capacity(64),
            expecting_data: None,
            buttons: 0,
            intellimouse_seq: 0,
        }
    }

    /// Receive a command byte from the host. Returns response byte if any.
    pub fn receive_command(&mut self, data: u8) -> Option<u8> {
        if let Some(cmd) = self.expecting_data.take() {
            return Some(self.handle_data_byte(cmd, data));
        }

        match data {
            0xFF => {
                // Reset: restore the full power-on factory state. The old handler
                // left `scaling_2to1` (and any in-progress command / queued bytes)
                // untouched, yet set-defaults (0xF6) clears scaling — so a guest
                // that set 2:1 scaling, reset, then read status (0xE9) saw the
                // stale 2:1 bit, an observable divergence. Reset is a superset of
                // set-defaults, so clear scaling too and flush the buffer.
                self.mouse_id = 0;
                self.sample_rate = 100;
                self.resolution = 2;
                self.scaling_2to1 = false;
                self.reporting_enabled = false;
                self.intellimouse_seq = 0;
                self.expecting_data = None;
                self.output_queue.clear();
                // Queue: ACK + BAT completion + mouse ID
                self.output_queue.push_back(0xAA); // BAT OK
                self.output_queue.push_back(self.mouse_id);
                Some(0xFA) // ACK
            }
            0xFE => {
                // Resend — just ACK
                Some(0xFA)
            }
            0xF6 => {
                // Set defaults
                self.sample_rate = 100;
                self.resolution = 2;
                self.scaling_2to1 = false;
                self.reporting_enabled = false;
                Some(0xFA)
            }
            0xF5 => {
                // Disable data reporting
                self.reporting_enabled = false;
                Some(0xFA)
            }
            0xF4 => {
                // Enable data reporting
                self.reporting_enabled = true;
                Some(0xFA)
            }
            0xF3 => {
                // Set sample rate (next byte is rate)
                self.expecting_data = Some(MouseCommand::SetSampleRate);
                Some(0xFA)
            }
            0xF2 => {
                // Get device ID
                self.output_queue.push_back(self.mouse_id);
                Some(0xFA)
            }
            0xF0 => {
                // Set remote mode (we stay in stream mode, just ACK)
                Some(0xFA)
            }
            0xEE => {
                // Set wrap mode
                Some(0xFA)
            }
            0xEC => {
                // Reset wrap mode
                Some(0xFA)
            }
            0xEB => {
                // Read data — send current state
                let packet = self.build_packet(self.buttons, 0, 0, 0);
                for b in &packet {
                    self.output_queue.push_back(*b);
                }
                Some(0xFA)
            }
            0xEA => {
                // Set stream mode (default)
                Some(0xFA)
            }
            0xE9 => {
                // Status request
                let status = if self.reporting_enabled { 0x20 } else { 0x00 }
                    | if self.scaling_2to1 { 0x10 } else { 0x00 }
                    | (self.buttons & 0x07);
                self.output_queue.push_back(status);
                self.output_queue.push_back(self.resolution);
                self.output_queue.push_back(self.sample_rate);
                Some(0xFA)
            }
            0xE8 => {
                // Set resolution (next byte)
                self.expecting_data = Some(MouseCommand::SetResolution);
                Some(0xFA)
            }
            0xE7 => {
                // Set scaling 2:1
                self.scaling_2to1 = true;
                Some(0xFA)
            }
            0xE6 => {
                // Set scaling 1:1
                self.scaling_2to1 = false;
                Some(0xFA)
            }
            _ => {
                // Unknown — NACK
                Some(0xFE)
            }
        }
    }

    /// Handle data byte for multi-byte commands
    const fn handle_data_byte(&mut self, cmd: MouseCommand, data: u8) -> u8 {
        match cmd {
            MouseCommand::SetSampleRate => {
                self.sample_rate = data;
                // Intellimouse detection: set rates 200, 100, 80 in sequence
                match (self.intellimouse_seq, data) {
                    (0, 200) => self.intellimouse_seq = 1,
                    (1, 100) => self.intellimouse_seq = 2,
                    (2, 80) => {
                        self.mouse_id = 3; // Intellimouse with scroll wheel
                        self.intellimouse_seq = 0;
                    }
                    _ => self.intellimouse_seq = 0,
                }
            }
            MouseCommand::SetResolution => {
                self.resolution = data;
            }
        }
        0xFA
    }

    /// Inject mouse movement from host input.
    pub fn inject_movement(&mut self, buttons: u8, dx: i16, dy: i16) {
        self.inject_movement_wheel(buttons, dx, dy, 0);
    }

    /// Inject mouse movement plus a scroll-wheel delta (`dz`). The wheel delta
    /// is only reported when the guest has switched the mouse into `IntelliMouse`
    /// mode (device id 3); on a standard mouse it is ignored. Per the protocol
    /// the wheel field is a signed 4-bit value, so `dz` is clamped to -8..=7.
    pub fn inject_movement_wheel(&mut self, buttons: u8, dx: i16, dy: i16, dz: i16) {
        self.buttons = buttons;
        if self.reporting_enabled {
            let packet = self.build_packet(buttons, dx, dy, dz);
            for b in &packet {
                self.output_queue.push_back(*b);
            }
        }
    }

    /// Build a PS/2 mouse packet
    fn build_packet(&self, buttons: u8, dx: i16, dy: i16, dz: i16) -> Vec<u8> {
        let horiz = dx.clamp(-256, 255);
        let vert = dy.clamp(-256, 255);

        let mut byte0: u8 = 0x08; // Always-set bit 3
        byte0 |= buttons & 0x07; // Button bits
        if horiz < 0 {
            byte0 |= 0x10; // X sign
        }
        if vert < 0 {
            byte0 |= 0x20; // Y sign
        }
        if horiz.unsigned_abs() > 255 {
            byte0 |= 0x40; // X overflow
        }
        if vert.unsigned_abs() > 255 {
            byte0 |= 0x80; // Y overflow
        }

        let mut packet = vec![byte0, horiz.to_le_bytes()[0], vert.to_le_bytes()[0]];

        // Intellimouse (id 3): 4th byte carries the signed wheel delta. The
        // standard wheel field is a 4-bit signed value (-8..=7) sign-extended to
        // the byte, which the guest driver reads as a signed quantity.
        if self.mouse_id == 3 {
            let z = i8::try_from(dz.clamp(-8, 7)).unwrap_or(0);
            packet.push(z.to_le_bytes()[0]);
        }

        packet
    }

    /// Dequeue a byte for the host to read
    pub fn dequeue_byte(&mut self) -> Option<u8> {
        self.output_queue.pop_front()
    }
}

impl Default for Ps2Mouse {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_response() {
        let mut mouse = Ps2Mouse::new();
        let ack = mouse.receive_command(0xFF);
        assert_eq!(ack, Some(0xFA));
        assert_eq!(mouse.dequeue_byte(), Some(0xAA)); // BAT OK
        assert_eq!(mouse.dequeue_byte(), Some(0x00)); // Mouse ID
    }

    #[test]
    fn reset_clears_scaling_to_factory_default() {
        let mut mouse = Ps2Mouse::new();
        // Enable 2:1 scaling (0xE7) and confirm the status request reflects it.
        assert_eq!(mouse.receive_command(0xE7), Some(0xFA));
        assert!(mouse.scaling_2to1);
        assert_eq!(mouse.receive_command(0xE9), Some(0xFA)); // status request
        let status = mouse.dequeue_byte().unwrap();
        assert_eq!(status & 0x10, 0x10, "2:1 scaling bit set before reset");

        // Reset restores factory defaults, including scaling 1:1, and flushes
        // the buffer so 0xAA (BAT OK) is the next byte read.
        assert_eq!(mouse.receive_command(0xFF), Some(0xFA));
        assert!(!mouse.scaling_2to1, "reset must clear 2:1 scaling");
        assert_eq!(mouse.dequeue_byte(), Some(0xAA));
        assert_eq!(mouse.dequeue_byte(), Some(0x00)); // mouse ID

        // The status request now reports 1:1 scaling.
        assert_eq!(mouse.receive_command(0xE9), Some(0xFA));
        let status = mouse.dequeue_byte().unwrap();
        assert_eq!(status & 0x10, 0, "1:1 scaling after reset");
    }

    #[test]
    fn get_device_id() {
        let mut mouse = Ps2Mouse::new();
        let ack = mouse.receive_command(0xF2);
        assert_eq!(ack, Some(0xFA));
        assert_eq!(mouse.dequeue_byte(), Some(0x00)); // Standard mouse
    }

    #[test]
    fn enable_reporting() {
        let mut mouse = Ps2Mouse::new();
        let ack = mouse.receive_command(0xF4);
        assert_eq!(ack, Some(0xFA));
        assert!(mouse.reporting_enabled);
    }

    #[test]
    fn intellimouse_detection() {
        let mut mouse = Ps2Mouse::new();
        // Send the magic sequence: set rate 200, 100, 80
        mouse.receive_command(0xF3); // Set sample rate
        mouse.receive_command(200);
        mouse.receive_command(0xF3);
        mouse.receive_command(100);
        mouse.receive_command(0xF3);
        mouse.receive_command(80);

        assert_eq!(mouse.mouse_id, 3); // Intellimouse
    }

    #[test]
    fn mouse_movement_packet() {
        let mut mouse = Ps2Mouse::new();
        mouse.receive_command(0xF4); // Enable reporting
        mouse.inject_movement(0x01, 10, -5); // Left button, move right and up

        let byte0 = mouse.dequeue_byte().unwrap();
        assert_ne!(byte0 & 0x01, 0); // Left button
        assert_eq!(byte0 & 0x10, 0); // X positive
        assert_ne!(byte0 & 0x20, 0); // Y negative
    }

    fn to_intellimouse(mouse: &mut Ps2Mouse) {
        for r in [200u8, 100, 80] {
            mouse.receive_command(0xF3);
            mouse.receive_command(r);
        }
        assert_eq!(mouse.mouse_id, 3);
    }

    #[test]
    fn intellimouse_reports_scroll_in_a_4th_byte() {
        let mut mouse = Ps2Mouse::new();
        to_intellimouse(&mut mouse);
        mouse.receive_command(0xF4); // enable reporting

        // Scroll up one detent (dz = -1 in PS/2 convention is wheel-down; here we
        // just check the signed value round-trips).
        mouse.inject_movement_wheel(0, 0, 0, -1);
        let pkt: Vec<u8> = std::iter::from_fn(|| mouse.dequeue_byte()).collect();
        assert_eq!(pkt.len(), 4, "intellimouse emits a 4-byte packet");
        assert_eq!(pkt[3], 0xFF, "wheel delta -1 sign-extends to 0xFF");

        // Positive detent.
        mouse.inject_movement_wheel(0, 0, 0, 3);
        let pkt: Vec<u8> = std::iter::from_fn(|| mouse.dequeue_byte()).collect();
        assert_eq!(pkt[3], 0x03);

        // Out-of-range delta clamps to the 4-bit signed range.
        mouse.inject_movement_wheel(0, 0, 0, 100);
        let pkt: Vec<u8> = std::iter::from_fn(|| mouse.dequeue_byte()).collect();
        assert_eq!(pkt[3], 0x07, "clamped to +7");
    }

    #[test]
    fn standard_mouse_ignores_scroll() {
        let mut mouse = Ps2Mouse::new(); // id 0
        mouse.receive_command(0xF4);
        mouse.inject_movement_wheel(0, 1, 1, 5);
        let pkt: Vec<u8> = std::iter::from_fn(|| mouse.dequeue_byte()).collect();
        assert_eq!(pkt.len(), 3, "no wheel byte without intellimouse mode");
    }
}

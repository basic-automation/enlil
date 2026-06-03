//! PS/2 keyboard and mouse controller emulation
//!
//! Windows expects PS/2 input devices early in boot before USB drivers load.
//! This implements the i8042 controller (port 0x60/0x64) with keyboard and
//! mouse channel support.

pub mod keyboard;
pub mod mouse;

/// i8042 controller ports
pub const DATA_PORT: u16 = 0x60;
pub const STATUS_CMD_PORT: u16 = 0x64;

/// i8042 controller status register bits
const STATUS_OUTPUT_FULL: u8 = 0x01;
const STATUS_SYSTEM_FLAG: u8 = 0x04;
const STATUS_MOUSE_OUTPUT: u8 = 0x20;

/// i8042 controller commands (written to port 0x64)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerCommand {
    ReadConfig = 0x20,
    WriteConfig = 0x60,
    DisableMouse = 0xA7,
    EnableMouse = 0xA8,
    TestMouse = 0xA9,
    SelfTest = 0xAA,
    TestKeyboard = 0xAB,
    DisableKeyboard = 0xAD,
    EnableKeyboard = 0xAE,
    ReadOutputPort = 0xD0,
    WriteOutputPort = 0xD1,
    WriteMouseBuffer = 0xD4,
}

/// Controller configuration byte bits
const CFG_KBD_INTERRUPT: u8 = 0x01;
const CFG_MOUSE_INTERRUPT: u8 = 0x02;
const CFG_SYSTEM_FLAG: u8 = 0x04;
const CFG_KBD_DISABLE: u8 = 0x10;
const CFG_MOUSE_DISABLE: u8 = 0x20;
const CFG_TRANSLATION: u8 = 0x40;

/// PS/2 controller (i8042) state
#[derive(Debug, Clone)]
pub struct I8042Controller {
    /// Configuration byte
    config: u8,
    /// Status register
    status: u8,
    /// Output buffer (data to be read by guest)
    output_buffer: u8,
    /// Pending command (waiting for data byte)
    pending_command: Option<u8>,
    /// Keyboard device
    keyboard: keyboard::Ps2Keyboard,
    /// Mouse device
    mouse: mouse::Ps2Mouse,
    /// Keyboard IRQ pending
    pub kbd_irq_pending: bool,
    /// Mouse IRQ pending
    pub mouse_irq_pending: bool,
    /// Whether output is from mouse (for `STATUS_MOUSE_OUTPUT` bit)
    output_is_mouse: bool,
}

impl I8042Controller {
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: CFG_KBD_INTERRUPT | CFG_MOUSE_INTERRUPT | CFG_SYSTEM_FLAG | CFG_TRANSLATION,
            status: STATUS_SYSTEM_FLAG,
            output_buffer: 0,
            pending_command: None,
            keyboard: keyboard::Ps2Keyboard::new(),
            mouse: mouse::Ps2Mouse::new(),
            kbd_irq_pending: false,
            mouse_irq_pending: false,
            output_is_mouse: false,
        }
    }

    /// Read from port 0x60 (data port)
    #[must_use]
    pub const fn read_data(&mut self) -> u8 {
        self.status &= !STATUS_OUTPUT_FULL;
        self.status &= !STATUS_MOUSE_OUTPUT;
        self.kbd_irq_pending = false;
        self.mouse_irq_pending = false;
        self.output_buffer
    }

    /// Read from port 0x64 (status register)
    #[must_use]
    pub const fn read_status(&self) -> u8 {
        self.status
    }

    /// Write to port 0x60 (data port)
    pub fn write_data(&mut self, data: u8) {
        if let Some(cmd) = self.pending_command.take() {
            self.handle_command_data(cmd, data);
        } else {
            // Data to keyboard
            if let Some(response) = self.keyboard.receive_command(data) {
                self.queue_keyboard_output(response);
            }
        }
    }

    /// Write to port 0x64 (command port)
    pub const fn write_command(&mut self, cmd: u8) {
        match cmd {
            0x20 => {
                // Read configuration byte
                self.queue_keyboard_output(self.config);
            }
            0x60 => {
                // Write configuration byte — needs data
                self.pending_command = Some(cmd);
            }
            0xA7 => {
                // Disable mouse
                self.config |= CFG_MOUSE_DISABLE;
            }
            0xA8 => {
                // Enable mouse
                self.config &= !CFG_MOUSE_DISABLE;
            }
            0xA9 => {
                // Test mouse port — 0x00 = passed
                self.queue_keyboard_output(0x00);
            }
            0xAA => {
                // Self test — 0x55 = passed
                self.queue_keyboard_output(0x55);
            }
            0xAB => {
                // Test keyboard port — 0x00 = passed
                self.queue_keyboard_output(0x00);
            }
            0xAD => {
                // Disable keyboard
                self.config |= CFG_KBD_DISABLE;
            }
            0xAE => {
                // Enable keyboard
                self.config &= !CFG_KBD_DISABLE;
            }
            0xD0 => {
                // Read output port
                let mut out = 0x01; // System reset line high
                if (self.status & STATUS_OUTPUT_FULL) != 0 {
                    out |= 0x10;
                }
                self.queue_keyboard_output(out);
            }
            0xD1 => {
                // Write output port — needs data
                self.pending_command = Some(cmd);
            }
            0xD4 => {
                // Write to mouse — needs data
                self.pending_command = Some(cmd);
            }
            _ => {
                // Unknown commands silently ignored
            }
        }
    }

    /// Handle data byte for a pending command
    fn handle_command_data(&mut self, cmd: u8, data: u8) {
        match cmd {
            0x60 => {
                // Write configuration byte
                self.config = data;
            }
            0xD1 => {
                // Write output port
                // Bit 0 = system reset (0 = reset)
                // We ignore reset requests in emulation
            }
            0xD4 => {
                // Forward data to mouse
                if let Some(response) = self.mouse.receive_command(data) {
                    self.queue_mouse_output(response);
                }
            }
            _ => {}
        }
    }

    /// Queue data from keyboard into output buffer
    const fn queue_keyboard_output(&mut self, data: u8) {
        self.output_buffer = data;
        self.status |= STATUS_OUTPUT_FULL;
        self.status &= !STATUS_MOUSE_OUTPUT;
        self.output_is_mouse = false;
        if (self.config & CFG_KBD_INTERRUPT) != 0 {
            self.kbd_irq_pending = true;
        }
    }

    /// Queue data from mouse into output buffer
    const fn queue_mouse_output(&mut self, data: u8) {
        self.output_buffer = data;
        self.status |= STATUS_OUTPUT_FULL;
        self.status |= STATUS_MOUSE_OUTPUT;
        self.output_is_mouse = true;
        if (self.config & CFG_MOUSE_INTERRUPT) != 0 {
            self.mouse_irq_pending = true;
        }
    }

    /// Inject a keyboard scancode (from host input)
    pub fn inject_key(&mut self, scancode: u8) {
        self.keyboard.inject_scancode(scancode);
        if let Some(sc) = self.keyboard.dequeue_scancode() {
            self.queue_keyboard_output(sc);
        }
    }

    /// Inject mouse movement/button data
    pub fn inject_mouse_packet(&mut self, buttons: u8, dx: i16, dy: i16) {
        self.mouse.inject_movement(buttons, dx, dy);
        if let Some(byte) = self.mouse.dequeue_byte() {
            self.queue_mouse_output(byte);
        }
    }

    /// Handle PIO read
    #[must_use]
    pub const fn pio_read(&mut self, port: u16) -> u8 {
        match port {
            DATA_PORT => self.read_data(),
            STATUS_CMD_PORT => self.read_status(),
            _ => 0xFF,
        }
    }

    /// Handle PIO write
    pub fn pio_write(&mut self, port: u16, data: u8) {
        match port {
            DATA_PORT => self.write_data(data),
            STATUS_CMD_PORT => self.write_command(data),
            _ => {}
        }
    }
}

impl Default for I8042Controller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let ctrl = I8042Controller::new();
        assert_eq!(ctrl.read_status() & STATUS_SYSTEM_FLAG, STATUS_SYSTEM_FLAG);
        assert_eq!(ctrl.read_status() & STATUS_OUTPUT_FULL, 0);
    }

    #[test]
    fn self_test_passes() {
        let mut ctrl = I8042Controller::new();
        ctrl.write_command(0xAA);
        assert_eq!(ctrl.read_status() & STATUS_OUTPUT_FULL, STATUS_OUTPUT_FULL);
        assert_eq!(ctrl.read_data(), 0x55);
    }

    #[test]
    fn keyboard_port_test() {
        let mut ctrl = I8042Controller::new();
        ctrl.write_command(0xAB);
        assert_eq!(ctrl.read_data(), 0x00);
    }

    #[test]
    fn mouse_port_test() {
        let mut ctrl = I8042Controller::new();
        ctrl.write_command(0xA9);
        assert_eq!(ctrl.read_data(), 0x00);
    }

    #[test]
    fn config_read_write() {
        let mut ctrl = I8042Controller::new();
        ctrl.write_command(0x20); // Read config
        let _old_config = ctrl.read_data();

        ctrl.write_command(0x60); // Write config
        ctrl.write_data(0x47);

        ctrl.write_command(0x20);
        assert_eq!(ctrl.read_data(), 0x47);
    }

    #[test]
    fn keyboard_disable_enable() {
        let mut ctrl = I8042Controller::new();
        ctrl.write_command(0xAD); // Disable
        ctrl.write_command(0x20);
        let config = ctrl.read_data();
        assert_ne!(config & CFG_KBD_DISABLE, 0);

        ctrl.write_command(0xAE); // Enable
        ctrl.write_command(0x20);
        let config = ctrl.read_data();
        assert_eq!(config & CFG_KBD_DISABLE, 0);
    }

    #[test]
    fn inject_key_produces_irq() {
        let mut ctrl = I8042Controller::new();
        ctrl.inject_key(0x1E); // 'A' key make code
        assert!(ctrl.kbd_irq_pending);
        assert_eq!(ctrl.read_status() & STATUS_OUTPUT_FULL, STATUS_OUTPUT_FULL);
    }

    #[test]
    fn pio_interface() {
        let mut ctrl = I8042Controller::new();
        ctrl.pio_write(STATUS_CMD_PORT, 0xAA);
        let result = ctrl.pio_read(DATA_PORT);
        assert_eq!(result, 0x55);
    }

    #[test]
    fn mouse_write_via_d4() {
        let mut ctrl = I8042Controller::new();
        ctrl.write_command(0xD4); // Write to mouse
        ctrl.write_data(0xFF); // Reset command
        // Mouse should respond with ACK
        assert_eq!(
            ctrl.read_status() & STATUS_MOUSE_OUTPUT,
            STATUS_MOUSE_OUTPUT
        );
    }
}

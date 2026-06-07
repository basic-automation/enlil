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
            0xD4 => {
                // Forward data to mouse
                if let Some(response) = self.mouse.receive_command(data) {
                    self.queue_mouse_output(response);
                }
            }
            // 0xD1 (write output port): bit 0 is system reset, which we ignore.
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

/// Keyboard ISA IRQ line (IR1 on the master 8259 / GSI 1).
pub const PS2_KBD_IRQ: u8 = 1;
/// Mouse ISA IRQ line (IR4 on the slave 8259 / GSI 12).
pub const PS2_MOUSE_IRQ: u8 = 12;

use std::sync::{Arc, Mutex};

use crate::bus::PioDevice;
use crate::timer::pit::IrqLine;
use crate::truncate::u8_of;

/// The controller plus its two interrupt sinks, reconciled after every access.
struct I8042Inner {
    ctrl: I8042Controller,
    kbd_irq: Option<Box<dyn IrqLine>>,
    mouse_irq: Option<Box<dyn IrqLine>>,
}

impl I8042Inner {
    /// Drive the IRQ1/IRQ12 lines from the controller's pending flags. The
    /// 8042 asserts a line while its output buffer holds data for that channel
    /// (and the matching interrupt is enabled) and deasserts it once the guest
    /// reads the data port — exactly what the flags track, so reconciling them
    /// after each operation keeps the lines correct without touching the
    /// register model.
    fn sync_irqs(&self) {
        if let Some(line) = &self.kbd_irq {
            line.set_level(self.ctrl.kbd_irq_pending);
        }
        if let Some(line) = &self.mouse_irq {
            line.set_level(self.ctrl.mouse_irq_pending);
        }
    }
}

/// A thread-safe, shareable handle to one [`I8042Controller`] with its IRQ
/// lines, plus the bus port adapters a guest drives.
///
/// Mirrors [`SharedPic`](crate::interrupt::SharedPic) /
/// [`SharedRtc`](crate::timer::SharedRtc): the data/command port adapters and
/// the host-input injector hold independent clones of one controller, and every
/// access reconciles the keyboard (IRQ1) and mouse (IRQ12) lines.
#[derive(Clone)]
pub struct SharedI8042(Arc<Mutex<I8042Inner>>);

impl SharedI8042 {
    /// Wrap a fresh i8042 with no interrupt sinks attached yet.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(I8042Inner {
            ctrl: I8042Controller::new(),
            kbd_irq: None,
            mouse_irq: None,
        })))
    }

    /// Run `f` with exclusive access to the controller, then reconcile the IRQ
    /// lines from its pending flags.
    ///
    /// # Panics
    /// Panics if the controller mutex has been poisoned by a prior panic.
    pub fn with<R>(&self, f: impl FnOnce(&mut I8042Controller) -> R) -> R {
        let mut inner = self.0.lock().expect("i8042 mutex poisoned");
        let r = f(&mut inner.ctrl);
        inner.sync_irqs();
        r
    }

    /// Attach the keyboard IRQ1 sink (see [`IrqLine`]); reconciles immediately.
    ///
    /// # Panics
    /// Panics if the controller mutex has been poisoned by a prior panic.
    pub fn attach_kbd_irq(&self, line: Box<dyn IrqLine>) {
        let mut inner = self.0.lock().expect("i8042 mutex poisoned");
        inner.kbd_irq = Some(line);
        inner.sync_irqs();
    }

    /// Attach the mouse IRQ12 sink (see [`IrqLine`]); reconciles immediately.
    ///
    /// # Panics
    /// Panics if the controller mutex has been poisoned by a prior panic.
    pub fn attach_mouse_irq(&self, line: Box<dyn IrqLine>) {
        let mut inner = self.0.lock().expect("i8042 mutex poisoned");
        inner.mouse_irq = Some(line);
        inner.sync_irqs();
    }

    /// Inject a host keyboard scancode and reconcile the IRQ lines.
    pub fn inject_key(&self, scancode: u8) {
        self.with(|c| c.inject_key(scancode));
    }

    /// Inject host mouse movement/buttons and reconcile the IRQ lines.
    pub fn inject_mouse_packet(&self, buttons: u8, dx: i16, dy: i16) {
        self.with(|c| c.inject_mouse_packet(buttons, dx, dy));
    }

    /// The data port (`0x60`) as a bus [`PioDevice`].
    #[must_use]
    pub fn data_port(&self) -> Ps2DataPort {
        Ps2DataPort { ps2: self.clone() }
    }

    /// The status/command port (`0x64`) as a bus [`PioDevice`].
    #[must_use]
    pub fn cmd_port(&self) -> Ps2CmdPort {
        Ps2CmdPort { ps2: self.clone() }
    }
}

impl Default for SharedI8042 {
    fn default() -> Self {
        Self::new()
    }
}

/// The i8042 data port (`0x60`) as a bus [`PioDevice`].
///
/// A single byte-wide port — `0x61`-`0x63` belong to other chipset functions
/// (the PC-speaker / NMI status ports), so the adapter claims only `0x60` and
/// the command adapter only `0x64`, never the gap between them.
pub struct Ps2DataPort {
    ps2: SharedI8042,
}

impl PioDevice for Ps2DataPort {
    fn pio_read(&mut self, _port: u16, _size: u8) -> u32 {
        u32::from(self.ps2.with(I8042Controller::read_data))
    }

    fn pio_write(&mut self, _port: u16, _size: u8, data: u32) {
        self.ps2.with(|c| c.write_data(u8_of(data)));
    }

    fn port_range(&self) -> (u16, u16) {
        (DATA_PORT, DATA_PORT + 1)
    }
}

/// The i8042 status/command port (`0x64`) as a bus [`PioDevice`]: a read
/// returns the status register, a write is a controller command.
pub struct Ps2CmdPort {
    ps2: SharedI8042,
}

impl PioDevice for Ps2CmdPort {
    fn pio_read(&mut self, _port: u16, _size: u8) -> u32 {
        u32::from(self.ps2.with(|c| c.read_status()))
    }

    fn pio_write(&mut self, _port: u16, _size: u8, data: u32) {
        self.ps2.with(|c| c.write_command(u8_of(data)));
    }

    fn port_range(&self) -> (u16, u16) {
        (STATUS_CMD_PORT, STATUS_CMD_PORT + 1)
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

    /// Record every IRQ level transition driven onto a sink.
    fn irq_log() -> (Arc<Mutex<Vec<bool>>>, impl Fn(bool) + Send) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let log = Arc::clone(&log);
            move |level: bool| log.lock().unwrap().push(level)
        };
        (log, sink)
    }

    #[test]
    fn keyboard_injection_asserts_irq1_then_clears_on_data_read() {
        let ps2 = SharedI8042::new();
        let (log, sink) = irq_log();
        ps2.attach_kbd_irq(Box::new(sink));
        // Attaching with no pending output reconciles to a low line.
        assert_eq!(&*log.lock().unwrap(), &[false]);

        // A host keypress fills the output buffer and raises IRQ1.
        ps2.inject_key(0x1E);
        assert_eq!(log.lock().unwrap().last(), Some(&true));

        // Reading the data port (0x60) through the bus clears it -> IRQ1 low.
        let mut port = ps2.data_port();
        assert_eq!(port.pio_read(DATA_PORT, 1) & 0xFF, 0x1E);
        assert_eq!(log.lock().unwrap().last(), Some(&false));
    }

    #[test]
    fn port_adapters_claim_only_0x60_and_0x64() {
        let ps2 = SharedI8042::new();
        // Never 0x61-0x63 (PC-speaker / NMI status ports).
        assert_eq!(ps2.data_port().port_range(), (0x60, 0x61));
        assert_eq!(ps2.cmd_port().port_range(), (0x64, 0x65));
    }

    #[test]
    fn self_test_through_the_bus_ports() {
        let ps2 = SharedI8042::new();
        let mut cmd = ps2.cmd_port();
        let mut data = ps2.data_port();
        cmd.pio_write(STATUS_CMD_PORT, 1, 0xAA); // self-test command
        // Status shows output-buffer-full, data port returns 0x55 (passed).
        assert_ne!(
            cmd.pio_read(STATUS_CMD_PORT, 1) & u32::from(STATUS_OUTPUT_FULL),
            0
        );
        assert_eq!(data.pio_read(DATA_PORT, 1) & 0xFF, 0x55);
    }

    #[test]
    fn mouse_packet_drives_irq12_when_enabled() {
        let ps2 = SharedI8042::new();
        let (log, sink) = irq_log();
        ps2.attach_mouse_irq(Box::new(sink));
        // Enable mouse data reporting (0xF4) through the controller (0xD4 routes
        // the next data byte to the mouse), draining the ACK first.
        ps2.with(|c| {
            c.write_command(0xD4);
            c.write_data(0xF4);
            let _ack = c.read_data();
        });
        ps2.inject_mouse_packet(0, 4, -3);
        assert_eq!(log.lock().unwrap().last(), Some(&true));
    }
}

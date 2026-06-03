//! xHCI Register model — Capability, Operational, Port, and Runtime registers.
//!
//! These registers form the programming interface between the guest OS driver
//! and our virtual xHCI controller. Guest MMIO accesses to the xHCI BAR are
//! trapped via EPT and dispatched to these register structures.
//!
//! Reference: xHCI specification 1.2, sections 5.1–5.5.

use crate::truncate::u32_of;
use std::fmt;

// ---------------------------------------------------------------------------
// Capability Registers (xHCI 5.3) — Read-Only
// ---------------------------------------------------------------------------

/// xHCI Capability Registers — define the controller's static capabilities.
///
/// These are read-only from the guest's perspective. Our virtual controller
/// presents fixed capability values that describe the emulated hardware.
#[derive(Debug, Clone)]
pub struct CapabilityRegisters {
    /// Length of this capability register block in bytes.
    pub caplength: u8,
    /// xHCI BCD version (0x0110 = 1.1.0, 0x0120 = 1.2.0).
    pub hciversion: u16,
    /// Structural Parameters 1: max slots, max interrupters, max ports.
    pub hcsparams1: u32,
    /// Structural Parameters 2: IST, ERST max, SPB max.
    pub hcsparams2: u32,
    /// Structural Parameters 3: U1/U2 exit latencies.
    pub hcsparams3: u32,
    /// Capability Parameters 1: feature flags.
    pub hccparams1: u32,
    /// Doorbell array offset from base.
    pub dboff: u32,
    /// Runtime register space offset from base.
    pub rtsoff: u32,
    /// Capability Parameters 2: extended feature flags.
    pub hccparams2: u32,
}

impl CapabilityRegisters {
    /// Create capability registers for a typical virtual xHCI controller.
    ///
    /// - 32 device slots
    /// - 1 interrupter
    /// - Configurable number of ports
    #[must_use]
    pub fn new_virtual(num_ports: u8) -> Self {
        let max_slots: u8 = 32;
        let max_intrs: u16 = 1;

        // HCSPARAMS1: MaxSlots[7:0], MaxIntrs[18:8], MaxPorts[31:24]
        let hcsparams1 =
            u32::from(max_slots) | (u32::from(max_intrs) << 8) | (u32::from(num_ports) << 24);

        // HCSPARAMS2: IST=1, ERST_Max=4 (16 entries), SPB_Max=0
        let hcsparams2 = 0x1 | (4 << 4);

        // HCSPARAMS3: U1 device exit latency = 10µs, U2 = 2047µs
        let hcsparams3 = 0x0A | (0x7FF << 16);

        // HCCPARAMS1: AC64=1 (64-bit addressing), CSZ=1 (64-byte context)
        let caps1 = 0x1 | (1 << 2);

        Self {
            caplength: 0x20,
            hciversion: 0x0110,
            hcsparams1,
            hcsparams2,
            hcsparams3,
            hccparams1: caps1,
            dboff: 0x2000,
            rtsoff: 0x1000,
            hccparams2: 0,
        }
    }

    /// Maximum number of device slots.
    #[must_use]
    pub const fn max_slots(&self) -> u8 {
        (self.hcsparams1 & 0xFF) as u8
    }

    /// Maximum number of interrupters.
    #[must_use]
    pub const fn max_interrupters(&self) -> u16 {
        ((self.hcsparams1 >> 8) & 0x7FF) as u16
    }

    /// Maximum number of ports.
    #[must_use]
    pub const fn max_ports(&self) -> u8 {
        ((self.hcsparams1 >> 24) & 0xFF) as u8
    }

    /// Whether 64-bit addressing is supported (AC64).
    #[must_use]
    pub const fn supports_64bit(&self) -> bool {
        (self.hccparams1 & 1) != 0
    }

    /// Whether 64-byte context structures are used (CSZ).
    #[must_use]
    pub const fn context_size_64(&self) -> bool {
        (self.hccparams1 & (1 << 2)) != 0
    }

    /// Read a capability register by byte offset.
    #[must_use]
    pub fn read(&self, offset: u32) -> u32 {
        match offset {
            0x00 => u32::from(self.caplength) | (u32::from(self.hciversion) << 16),
            0x04 => self.hcsparams1,
            0x08 => self.hcsparams2,
            0x0C => self.hcsparams3,
            0x10 => self.hccparams1,
            0x14 => self.dboff,
            0x18 => self.rtsoff,
            0x1C => self.hccparams2,
            _ => 0,
        }
    }
}

impl fmt::Display for CapabilityRegisters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "xHCI v{}.{}.{}: {} slots, {} intrs, {} ports",
            self.hciversion >> 8,
            (self.hciversion >> 4) & 0xF,
            self.hciversion & 0xF,
            self.max_slots(),
            self.max_interrupters(),
            self.max_ports()
        )
    }
}

// ---------------------------------------------------------------------------
// Port State (xHCI 5.4.8)
// ---------------------------------------------------------------------------

/// USB port link state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PortState {
    /// No device connected.
    Disconnected = 0,
    /// Device present, not yet enabled.
    Disabled = 1,
    /// Port is being reset.
    Resetting = 2,
    /// Port is enabled — device is operational.
    Enabled = 3,
    /// Port is suspended (U3).
    Suspended = 4,
    /// Port is in compliance mode (error recovery).
    Compliance = 5,
}

impl fmt::Display for PortState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => write!(f, "Disconnected"),
            Self::Disabled => write!(f, "Disabled"),
            Self::Resetting => write!(f, "Resetting"),
            Self::Enabled => write!(f, "Enabled"),
            Self::Suspended => write!(f, "Suspended"),
            Self::Compliance => write!(f, "Compliance"),
        }
    }
}

// ---------------------------------------------------------------------------
// Port Register Set (xHCI 5.4.8)
// ---------------------------------------------------------------------------

/// Register set for a single USB port (PORTSC + companions).
#[derive(Debug, Clone)]
pub struct PortRegisterSet {
    /// Port Status and Control register.
    pub portsc: u32,
    /// Port Power Management Status and Control.
    pub portpmsc: u32,
    /// Port Link Info.
    pub portli: u32,
    /// Port Hardware LPM Control.
    pub porthlpmc: u32,
    /// Cached port state (derived from PORTSC).
    state: PortState,
    /// Port speed (from PORTSC bits [13:10]).
    speed: u8,
}

impl PortRegisterSet {
    /// Create a new port register set in the disconnected state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            portsc: 0,
            portpmsc: 0,
            portli: 0,
            porthlpmc: 0,
            state: PortState::Disconnected,
            speed: 0,
        }
    }

    /// Get the current port state.
    #[must_use]
    pub const fn state(&self) -> PortState {
        self.state
    }

    /// Check if a device is connected (CCS bit).
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        (self.portsc & 1) != 0
    }

    /// Check if the port is enabled (PED bit).
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        (self.portsc & (1 << 1)) != 0
    }

    /// Check if port power is on (PP bit).
    #[must_use]
    pub const fn is_powered(&self) -> bool {
        (self.portsc & (1 << 9)) != 0
    }

    /// Get the port speed (bits [13:10]).
    #[must_use]
    pub const fn port_speed(&self) -> u8 {
        ((self.portsc >> 10) & 0xF) as u8
    }

    /// Check if Connect Status Change is set (CSC, bit 17).
    #[must_use]
    pub const fn connect_status_change(&self) -> bool {
        (self.portsc & (1 << 17)) != 0
    }

    /// Simulate a device connection at the given speed code.
    ///
    /// Speed codes: 1=FS, 2=LS, 3=HS, 4=SS, 5=SSP
    pub fn connect_device(&mut self, speed: u8) {
        self.speed = speed;
        // CCS=1, Port Speed, PP=1, CSC=1
        self.portsc = 1 // CCS
            | (u32::from(speed) << 10) // Speed
            | (1 << 9) // PP
            | (1 << 17); // CSC
        self.state = PortState::Disabled;
    }

    /// Simulate a device disconnection.
    pub const fn disconnect_device(&mut self) {
        self.speed = 0;
        // CCS=0, PP=1, CSC=1
        self.portsc = (1 << 9) | (1 << 17);
        self.state = PortState::Disconnected;
    }

    /// Simulate a port reset completion.
    pub fn complete_reset(&mut self) {
        if self.is_connected() {
            // CCS=1, PED=1, Speed preserved, PP=1, PRC=1
            self.portsc = 1 // CCS
                | (1 << 1) // PED
                | (u32::from(self.speed) << 10) // Speed
                | (1 << 9) // PP
                | (1 << 21); // PRC (Port Reset Change)
            self.state = PortState::Enabled;
        }
    }

    /// Write to PORTSC — handles write-1-to-clear and read-only bits.
    pub fn write_portsc(&mut self, value: u32) {
        // Write-1-to-clear bits: CSC(17), PEC(18), WRC(19), OCC(20), PRC(21), PLC(22), CEC(23)
        let w1c_mask: u32 = 0x00FE_0000;
        let w1c_bits = value & w1c_mask;
        self.portsc &= !w1c_bits;

        // PR (bit 4): if set, initiate port reset
        if value & (1 << 4) != 0 {
            self.state = PortState::Resetting;
            // In a real implementation, we'd schedule the reset completion.
            // For virtual controllers, complete immediately.
            self.complete_reset();
        }

        // PP (bit 9): writable
        if value & (1 << 9) != 0 {
            self.portsc |= 1 << 9;
        } else {
            self.portsc &= !(1 << 9);
        }
    }
}

impl Default for PortRegisterSet {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Operational Registers (xHCI 5.4)
// ---------------------------------------------------------------------------

/// xHCI Operational Registers — the main control interface.
#[derive(Debug, Clone)]
pub struct OperationalRegisters {
    /// USB Command register.
    pub usbcmd: u32,
    /// USB Status register.
    pub usbsts: u32,
    /// Page Size register (read-only, always 4096).
    pub pagesize: u32,
    /// Device Notification Control.
    pub dnctrl: u32,
    /// Command Ring Control Register (64-bit).
    pub crcr: u64,
    /// Device Context Base Address Array Pointer (64-bit).
    pub dcbaap: u64,
    /// Configure register (`MaxSlotsEn`).
    pub config: u32,
    /// Port register sets.
    pub ports: Vec<PortRegisterSet>,
}

impl OperationalRegisters {
    /// Create operational registers with the given number of ports.
    #[must_use]
    pub fn new(num_ports: usize) -> Self {
        Self {
            usbcmd: 0,
            usbsts: 0x0001, // HCHalted = 1 on reset
            pagesize: 1,    // 4096 bytes
            dnctrl: 0,
            crcr: 0,
            dcbaap: 0,
            config: 0,
            ports: vec![PortRegisterSet::new(); num_ports],
        }
    }

    /// Check if the controller is running (R/S bit in USBCMD).
    #[must_use]
    pub const fn is_running(&self) -> bool {
        (self.usbcmd & 1) != 0
    }

    /// Check if the controller is halted (HCH bit in USBSTS).
    #[must_use]
    pub const fn is_halted(&self) -> bool {
        (self.usbsts & 1) != 0
    }

    /// Check if host system error occurred (HSE bit in USBSTS).
    #[must_use]
    pub const fn host_system_error(&self) -> bool {
        (self.usbsts & (1 << 2)) != 0
    }

    /// Get the maximum number of enabled device slots.
    #[must_use]
    pub const fn max_slots_enabled(&self) -> u8 {
        (self.config & 0xFF) as u8
    }

    /// Write to USBCMD. Handles Run/Stop and HCRST.
    pub fn write_usbcmd(&mut self, value: u32) {
        let was_running = self.is_running();
        self.usbcmd = value;

        // HCRST (bit 1): host controller reset
        if value & 2 != 0 {
            self.reset();
            return;
        }

        // R/S (bit 0): run/stop
        if (value & 1) != 0 && !was_running {
            // Starting: clear HCH
            self.usbsts &= !1;
        } else if (value & 1) == 0 && was_running {
            // Stopping: set HCH
            self.usbsts |= 1;
        }
    }

    /// Write to USBSTS — write-1-to-clear semantics for event bits.
    pub const fn write_usbsts(&mut self, value: u32) {
        // HSE(2), EINT(3), PCD(4) are write-1-to-clear
        let w1c_mask: u32 = 0x1C;
        let w1c_bits = value & w1c_mask;
        self.usbsts &= !w1c_bits;
    }

    /// Read an operational register by byte offset.
    #[must_use]
    pub fn read(&self, offset: u32) -> u32 {
        match offset {
            0x00 => self.usbcmd,
            0x04 => self.usbsts,
            0x08 => self.pagesize,
            0x14 => self.dnctrl,
            0x18 => u32_of(self.crcr),
            0x1C => (self.crcr >> 32) as u32,
            0x30 => u32_of(self.dcbaap),
            0x34 => (self.dcbaap >> 32) as u32,
            0x38 => self.config,
            offset if offset >= 0x400 => {
                // Port registers start at offset 0x400, each port is 16 bytes
                let port_offset = offset - 0x400;
                let port_idx = (port_offset / 16) as usize;
                let reg_offset = port_offset % 16;
                self.ports.get(port_idx).map_or(0, |port| match reg_offset {
                    0 => port.portsc,
                    4 => port.portpmsc,
                    8 => port.portli,
                    12 => port.porthlpmc,
                    _ => 0,
                })
            }
            _ => 0,
        }
    }

    /// Reset all operational registers to power-on defaults.
    pub fn reset(&mut self) {
        self.usbcmd = 0;
        self.usbsts = 0x0001; // HCH=1
        self.dnctrl = 0;
        self.crcr = 0;
        self.dcbaap = 0;
        self.config = 0;
        for port in &mut self.ports {
            *port = PortRegisterSet::new();
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime Registers (xHCI 5.5)
// ---------------------------------------------------------------------------

/// xHCI Runtime Registers.
#[derive(Debug)]
pub struct RuntimeRegisters {
    /// Microframe Index register (14-bit counter).
    pub mfindex: u32,
}

impl RuntimeRegisters {
    /// Create default runtime registers.
    #[must_use]
    pub const fn new() -> Self {
        Self { mfindex: 0 }
    }

    /// Advance the microframe index (called periodically at 125µs intervals).
    pub const fn tick(&mut self) {
        self.mfindex = (self.mfindex + 1) & 0x3FFF;
    }
}

impl Default for RuntimeRegisters {
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

    #[test]
    fn capability_registers_defaults() {
        let caps = CapabilityRegisters::new_virtual(8);
        assert_eq!(caps.max_slots(), 32);
        assert_eq!(caps.max_interrupters(), 1);
        assert_eq!(caps.max_ports(), 8);
        assert!(caps.supports_64bit());
        assert!(caps.context_size_64());
    }

    #[test]
    fn capability_register_read() {
        let caps = CapabilityRegisters::new_virtual(4);
        let val = caps.read(0x00);
        assert_eq!(val & 0xFF, 0x20); // CAPLENGTH
        assert_eq!((val >> 16) & 0xFFFF, 0x0110); // HCIVERSION
    }

    #[test]
    fn capability_display() {
        let caps = CapabilityRegisters::new_virtual(4);
        let s = caps.to_string();
        assert!(s.contains("xHCI"));
        assert!(s.contains("32 slots"));
    }

    #[test]
    fn port_connect_disconnect() {
        let mut port = PortRegisterSet::new();
        assert!(!port.is_connected());
        assert_eq!(port.state(), PortState::Disconnected);

        port.connect_device(3); // High-speed
        assert!(port.is_connected());
        assert_eq!(port.state(), PortState::Disabled);
        assert_eq!(port.port_speed(), 3);
        assert!(port.connect_status_change());

        port.disconnect_device();
        assert!(!port.is_connected());
        assert_eq!(port.state(), PortState::Disconnected);
    }

    #[test]
    fn port_reset_enables() {
        let mut port = PortRegisterSet::new();
        port.connect_device(4); // SuperSpeed
        assert!(!port.is_enabled());

        // Write PR bit to trigger reset
        port.write_portsc(1 << 4);
        assert!(port.is_enabled());
        assert_eq!(port.state(), PortState::Enabled);
    }

    #[test]
    fn portsc_write_1_to_clear() {
        let mut port = PortRegisterSet::new();
        port.connect_device(3);
        assert!(port.connect_status_change());

        // Write 1 to CSC bit to clear it
        port.write_portsc(1 << 17);
        assert!(!port.connect_status_change());
    }

    #[test]
    fn operational_registers_halted_on_reset() {
        let ops = OperationalRegisters::new(4);
        assert!(ops.is_halted());
        assert!(!ops.is_running());
    }

    #[test]
    fn operational_run_stop() {
        let mut ops = OperationalRegisters::new(4);
        ops.write_usbcmd(1); // Set R/S bit
        assert!(ops.is_running());
        assert!(!ops.is_halted());

        ops.write_usbcmd(0); // Clear R/S bit
        assert!(!ops.is_running());
        assert!(ops.is_halted());
    }

    #[test]
    fn operational_host_reset() {
        let mut ops = OperationalRegisters::new(4);
        ops.write_usbcmd(1); // Start
        ops.config = 8;
        ops.dcbaap = 0xDEAD_BEEF;

        ops.write_usbcmd(2); // HCRST
        assert_eq!(ops.config, 0);
        assert_eq!(ops.dcbaap, 0);
        assert!(ops.is_halted());
    }

    #[test]
    fn operational_register_read() {
        let mut ops = OperationalRegisters::new(4);
        ops.ports[0].connect_device(3);

        let portsc = ops.read(0x400); // Port 0 PORTSC
        assert_ne!(portsc, 0);
        assert_eq!(portsc & 1, 1); // CCS
    }

    #[test]
    fn runtime_tick() {
        let mut rt = RuntimeRegisters::new();
        assert_eq!(rt.mfindex, 0);
        rt.tick();
        assert_eq!(rt.mfindex, 1);

        rt.mfindex = 0x3FFF;
        rt.tick();
        assert_eq!(rt.mfindex, 0); // Wraps at 14 bits
    }

    #[test]
    fn port_state_display() {
        assert_eq!(PortState::Enabled.to_string(), "Enabled");
        assert_eq!(PortState::Disconnected.to_string(), "Disconnected");
    }
}

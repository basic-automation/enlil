//! Chipset system-control ports that aren't owned by a specific peripheral.
//!
//! These are the small fixed-function I/O ports a PC chipset (the PIIX/ICH
//! south-bridge, historically the PS/2 controller's "port A") exposes for
//! platform control — distinct from device register files like the PIT or RTC:
//! - **System Control Port A** (`0x92`): the fast-A20 / fast-reset register every
//!   x86 boot path touches ([`SystemControlPortA`]).
//! - **ACPI `PM1a` event/control block** (`0x600`/`0x604`): the registers an OS
//!   uses to enter a sleep state — most importantly `S5` soft-off, i.e. shutdown
//!   ([`AcpiPm1Block`]).

use crate::bus::PioDevice;
use crate::truncate::{u8_of, u16_of};
use std::sync::{Arc, Mutex};

/// The ACPI SCI's ISA IRQ — the value the FADT advertises (`SCI_INT`), the
/// LPC bridge's `ACPI_CNTL` register encodes, and the PIRQ defaults avoid.
pub const SCI_IRQ: u8 = 9;

/// The port [`SystemControlPortA`] claims.
pub const PORT_A: u16 = 0x92;

/// Bit 0: fast INIT (CPU reset). Writing 1 requests a reset; it reads back 0.
const FAST_RESET: u8 = 1 << 0;
/// Bit 1: fast A20 gate. When set, the A20 address line is enabled (unmasked).
const A20_GATE: u8 = 1 << 1;

/// **System Control Port A** (`0x92`) as a bus [`PioDevice`].
///
/// This is the fast path for the two things early boot used to drive through the
/// keyboard controller: opening the **A20 gate** (bit 1) and pulsing a **CPU
/// reset** (bit 0). The slow i8042 route still works, but every modern boot path
/// (and most BIOSes) prefers `0x92` because it's a single `out` rather than the
/// keyboard-controller command dance.
///
/// - **A20 (bit 1):** read/write, and defaults to **enabled**. Enlil runs no
///   legacy BIOS that performs the real-mode A20 handshake, and KVM keeps A20
///   open, so the gate is modelled as already unmasked — a guest that reads
///   `0x92` to confirm A20 finds it set, and one that writes the bit sees it
///   stick, both matching a post-firmware machine.
/// - **Fast reset (bit 0):** write-1 latches a [reset request](Self::take_reset)
///   for the run loop to act on (reinitialise the vCPU to its reset vector); the
///   bit is edge-triggered, so it always reads back 0.
/// - Other bits are stored and read back unchanged (reserved / lock bits).
pub struct SystemControlPortA {
    /// Stored bits 1-7 (bit 0, the reset edge, is never stored).
    value: u8,
    /// Set when bit 0 was written 1; consumed by [`take_reset`](Self::take_reset).
    reset_requested: bool,
}

impl Default for SystemControlPortA {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemControlPortA {
    /// A new port with the A20 gate already enabled (post-firmware default) and
    /// no pending reset.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            value: A20_GATE,
            reset_requested: false,
        }
    }

    /// Whether the A20 address line is currently enabled (bit 1).
    #[must_use]
    pub const fn a20_enabled(&self) -> bool {
        self.value & A20_GATE != 0
    }

    /// Consume a pending fast-reset request: returns `true` exactly once after a
    /// guest writes bit 0 = 1, so the backend's run loop can reset the vCPU and
    /// then clear the latch.
    pub const fn take_reset(&mut self) -> bool {
        let pending = self.reset_requested;
        self.reset_requested = false;
        pending
    }
}

impl PioDevice for SystemControlPortA {
    fn pio_read(&mut self, _port: u16, _size: u8) -> u32 {
        // Bit 0 is the reset edge — it always reads back 0.
        u32::from(self.value & !FAST_RESET)
    }

    fn pio_write(&mut self, _port: u16, _size: u8, data: u32) {
        let byte = u8_of(data);
        if byte & FAST_RESET != 0 {
            self.reset_requested = true;
        }
        // Persist everything except the reset edge.
        self.value = byte & !FAST_RESET;
    }

    fn port_range(&self) -> (u16, u16) {
        (PORT_A, PORT_A + 1)
    }
}

/// A shared [`SystemControlPortA`] behind an `Arc<Mutex<…>>`.
///
/// The [`SysCtlAPort`] bus adapter handles the guest's `0x92` accesses while the
/// run loop holds a clone to poll [`take_reset`](Self::take_reset) (and reset the
/// vCPU) and to read the live A20 state.
#[derive(Clone, Default)]
pub struct SharedSystemControlPortA(Arc<Mutex<SystemControlPortA>>);

impl SharedSystemControlPortA {
    /// Wrap a fresh port (A20 enabled, no pending reset).
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(SystemControlPortA::new())))
    }

    /// Run `f` with exclusive access to the port.
    ///
    /// # Panics
    /// Panics if the mutex has been poisoned by a prior panic while held.
    pub fn with<R>(&self, f: impl FnOnce(&mut SystemControlPortA) -> R) -> R {
        f(&mut self.0.lock().expect("port A mutex poisoned"))
    }

    /// Consume a pending fast-reset request (see [`SystemControlPortA::take_reset`]).
    pub fn take_reset(&self) -> bool {
        self.with(SystemControlPortA::take_reset)
    }

    /// Whether the A20 gate is currently enabled.
    #[must_use]
    pub fn a20_enabled(&self) -> bool {
        self.with(|p| p.a20_enabled())
    }

    /// The `0x92` port as a bus [`PioDevice`].
    #[must_use]
    pub fn port(&self) -> SysCtlAPort {
        SysCtlAPort {
            inner: self.clone(),
        }
    }
}

/// System Control Port A (`0x92`) as a bus [`PioDevice`], backed by a
/// [`SharedSystemControlPortA`] so the guest's fast-reset write is visible to the
/// run loop.
pub struct SysCtlAPort {
    inner: SharedSystemControlPortA,
}

impl PioDevice for SysCtlAPort {
    fn pio_read(&mut self, port: u16, size: u8) -> u32 {
        self.inner.with(|p| p.pio_read(port, size))
    }

    fn pio_write(&mut self, port: u16, size: u8, data: u32) {
        self.inner.with(|p| p.pio_write(port, size, data));
    }

    fn port_range(&self) -> (u16, u16) {
        (PORT_A, PORT_A + 1)
    }
}

/// I/O port of the **`PM1a` event block** (`PM1a_EVT_BLK`).
///
/// A 16-bit status register (write-1-to-clear) at `0x600` followed by a 16-bit
/// enable register at `0x602`. Matches the address + 4-byte length the emitted
/// FADT advertises.
pub const PM1_EVT_PORT: u16 = 0x600;

/// I/O port of the **`PM1a` control block** (`PM1a_CNT_BLK`).
///
/// A 16-bit control register at `0x604` (2-byte length per the FADT) carrying
/// `SCI_EN` and the `SLP_TYP`/`SLP_EN` sleep-transition fields.
pub const PM1_CNT_PORT: u16 = 0x604;

/// One-past-the-end of the contiguous `PM1a` register block (`0x600`..`0x606`).
const PM1_BLOCK_END: u16 = PM1_CNT_PORT + 2;

/// `PWRBTN_STS`/`PWRBTN_EN` — the power-button event (bit 8).
const PM1_PWRBTN: u16 = 1 << 8;

// PM1 control bits.
/// `SCI_EN` (bit 0): set once the OS has switched the platform into ACPI mode.
const PM1_CNT_SCI_EN: u16 = 1 << 0;
/// `SLP_EN` (bit 13): writing 1 commits the `SLP_TYP` sleep transition.
const PM1_CNT_SLP_EN: u16 = 1 << 13;
/// `SLP_TYP` (bits 10-12): which sleep state to enter (the DSDT's `_Sx` value).
const PM1_CNT_SLP_TYP_MASK: u16 = 0x7 << 10;
/// `SLP_TYP` field shift.
const PM1_CNT_SLP_TYP_SHIFT: u16 = 10;
/// Bits the guest can store in the control register (everything but the
/// write-only `SLP_EN` edge and the `SLP_TYP` selector, which we capture).
const PM1_CNT_STORED_MASK: u16 = !(PM1_CNT_SLP_EN | PM1_CNT_SLP_TYP_MASK);

/// The **ACPI `PM1a` event + control block** as a bus [`PioDevice`].
///
/// This is the register set an ACPI OS drives to change the system power state
/// (it spans `0x600`..`0x606`). The one that matters most for a usable guest is
/// **shutdown**: the OS writes `SLP_TYP` (the `_S5` value) together with `SLP_EN`
/// to the control register (`0x604`), which on real hardware powers the machine
/// off. We capture that write as a
/// [sleep request](Self::take_sleep) for the run loop to act on (tear the guest
/// down), rather than letting the access fall into open bus where a shutdown
/// would simply hang.
///
/// The event registers are modelled faithfully enough for an OS to probe them:
/// `PM1a_STS` (`0x600`) is write-1-to-clear, `PM1a_EN` (`0x602`) is read/write,
/// and the host can raise the power-button event ([`press_power_button`]) so a
/// guest can shut down in response to a virtual power button. Generating the SCI
/// interrupt itself is a run-loop concern (it needs the interrupt controller) and
/// is deferred.
///
/// [`press_power_button`]: Self::press_power_button
#[derive(Debug, Clone, Default)]
pub struct AcpiPm1Block {
    /// `PM1a_STS` (write-1-to-clear status bits).
    status: u16,
    /// `PM1a_EN` (interrupt-enable bits).
    enable: u16,
    /// `PM1a_CNT` stored bits (`SCI_EN`, `BM_RLD`, …; not `SLP_EN`/`SLP_TYP`).
    control: u16,
    /// The `SLP_TYP` captured when the guest last committed a sleep transition
    /// (`SLP_EN` written 1), pending consumption by the run loop.
    sleep_request: Option<u8>,
}

impl AcpiPm1Block {
    /// A new block with all registers clear and no pending sleep.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            status: 0,
            enable: 0,
            control: 0,
            sleep_request: None,
        }
    }

    /// Whether the OS has switched the platform into ACPI mode (`SCI_EN`).
    #[must_use]
    pub const fn sci_enabled(&self) -> bool {
        self.control & PM1_CNT_SCI_EN != 0
    }

    /// Set or clear `SCI_EN` — i.e. enter or leave ACPI mode. On real hardware
    /// the OS can't write `SCI_EN` in `PM1a_CNT` directly; it asks the chipset's
    /// SMI handler to do it by writing `ACPI_ENABLE`/`ACPI_DISABLE` to the SMI
    /// command port (`0xB2`). [`SmiCommandPort`] calls this in response to that
    /// write, so ACPICA's mode-switch handshake (write `0xA0`, poll `SCI_EN`)
    /// completes instead of timing out.
    pub const fn set_acpi_mode(&mut self, enabled: bool) {
        if enabled {
            self.control |= PM1_CNT_SCI_EN;
        } else {
            self.control &= !PM1_CNT_SCI_EN;
        }
    }

    /// Raise the power-button status bit, as if the host pressed the (virtual)
    /// power button. An ACPI OS with `PWRBTN_EN` set treats this as a request to
    /// shut down, and responds by writing the S5 sleep transition.
    pub const fn press_power_button(&mut self) {
        self.status |= PM1_PWRBTN;
    }

    /// Consume a pending sleep transition: returns the `SLP_TYP` value the guest
    /// committed (exactly once) so the run loop can enter that sleep state — for
    /// the DSDT's `_S5` value, power the guest off.
    pub const fn take_sleep(&mut self) -> Option<u8> {
        let pending = self.sleep_request;
        self.sleep_request = None;
        pending
    }

    /// The 32-bit view of the event block: `PM1a_STS` in the low half, `PM1a_EN`
    /// in the high half (the block is 4 bytes, so a 32-bit access spans both).
    const fn evt_dword(&self) -> u32 {
        (self.status as u32) | ((self.enable as u32) << 16)
    }

    /// Apply a write to `PM1a_STS` (write-1-to-clear).
    const fn write_status(&mut self, value: u16) {
        self.status &= !value;
    }

    /// Apply a write to `PM1a_CNT`: store the persistent bits, and if `SLP_EN` is
    /// set, capture `SLP_TYP` as a pending sleep request.
    fn write_control(&mut self, value: u16) {
        if value & PM1_CNT_SLP_EN != 0 {
            self.sleep_request = Some(u8_of(
                (value & PM1_CNT_SLP_TYP_MASK) >> PM1_CNT_SLP_TYP_SHIFT,
            ));
        }
        self.control = value & PM1_CNT_STORED_MASK;
    }
}

impl PioDevice for AcpiPm1Block {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        match port {
            // Event block: a 32-bit window over status (low) + enable (high).
            PM1_EVT_PORT..PM1_CNT_PORT => {
                let off = u32::from(port - PM1_EVT_PORT);
                self.evt_dword() >> (off * 8)
            }
            // Control register (SLP_EN reads back 0 — it is write-only).
            PM1_CNT_PORT..PM1_BLOCK_END => {
                let off = u32::from(port - PM1_CNT_PORT);
                u32::from(self.control) >> (off * 8)
            }
            _ => 0xFFFF_FFFF,
        }
    }

    fn pio_write(&mut self, port: u16, size: u8, data: u32) {
        match port {
            // Status (0x600) — and, for a 32-bit access, the enable in the high
            // half. A 16-bit write touches only the status.
            PM1_EVT_PORT => {
                self.write_status(u16_of(data));
                if size >= 4 {
                    self.enable = u16_of(data >> 16);
                }
            }
            // Enable register (0x602).
            0x602 => self.enable = u16_of(data),
            // Control register (0x604).
            PM1_CNT_PORT => self.write_control(u16_of(data)),
            _ => {}
        }
    }

    fn port_range(&self) -> (u16, u16) {
        (PM1_EVT_PORT, PM1_BLOCK_END)
    }
}

/// A shared [`AcpiPm1Block`] behind an `Arc<Mutex<…>>`, mirroring the other shared
/// devices.
///
/// The [`Pm1Port`] bus adapter handles the guest's register accesses while the
/// run loop holds a clone to poll [`take_sleep`](Self::take_sleep) (and act on a
/// shutdown) and to inject a virtual power-button press.
#[derive(Clone, Default)]
pub struct SharedAcpiPm1Block(Arc<Mutex<AcpiPm1Block>>);

impl SharedAcpiPm1Block {
    /// Wrap a fresh PM1 block.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(AcpiPm1Block::new())))
    }

    /// Run `f` with exclusive access to the block.
    ///
    /// # Panics
    /// Panics if the mutex has been poisoned by a prior panic while held.
    pub fn with<R>(&self, f: impl FnOnce(&mut AcpiPm1Block) -> R) -> R {
        f(&mut self.0.lock().expect("PM1 block mutex poisoned"))
    }

    /// Consume a pending sleep transition (see [`AcpiPm1Block::take_sleep`]) — the
    /// run loop polls this and powers the guest off on the `_S5` value.
    pub fn take_sleep(&self) -> Option<u8> {
        self.with(AcpiPm1Block::take_sleep)
    }

    /// Inject a virtual power-button press (see
    /// [`AcpiPm1Block::press_power_button`]).
    pub fn press_power_button(&self) {
        self.with(AcpiPm1Block::press_power_button);
    }

    /// The `PM1a` register block as a bus [`PioDevice`].
    #[must_use]
    pub fn port(&self) -> Pm1Port {
        Pm1Port { pm1: self.clone() }
    }

    /// The SMI command port (`0xB2`) as a bus [`PioDevice`], driving this PM1
    /// block's `SCI_EN` so the OS's ACPI-enable handshake completes.
    #[must_use]
    pub fn smi_command_port(&self) -> SmiCommandPort {
        SmiCommandPort {
            pm1: self.clone(),
            last_command: 0,
        }
    }
}

/// I/O port of the chipset **SMI command register** (`SMI_CMD`), the FADT's
/// `SMI_CMD` field. An ACPI OS writes here to ask the platform to switch ACPI
/// mode on or off.
pub const SMI_CMD_PORT: u16 = 0xB2;

/// The `SMI_CMD` value (`ACPI_ENABLE`) the OS writes to enter ACPI mode — matches
/// the FADT's `ACPI_ENABLE` field.
pub const ACPI_ENABLE_VALUE: u8 = 0xA0;

/// The `SMI_CMD` value (`ACPI_DISABLE`) the OS writes to leave ACPI mode — matches
/// the FADT's `ACPI_DISABLE` field.
pub const ACPI_DISABLE_VALUE: u8 = 0xA1;

/// The **SMI command port** (`0xB2`, `SMI_CMD`) as a bus [`PioDevice`].
///
/// The FADT advertises a non-zero `SMI_CMD` with `ACPI_ENABLE`/`ACPI_DISABLE`
/// values, so an ACPI OS (ACPICA) switches into ACPI mode by writing
/// [`ACPI_ENABLE_VALUE`] here and then **polling `SCI_EN`** in `PM1a_CNT` until it
/// reads back set. On real hardware the chipset's SMI handler sets `SCI_EN` in
/// response; there is no SMM here, so this port performs that side effect
/// directly. Without it the write would fall into open bus, `SCI_EN` would never
/// set, and the OS would abort ACPI init with "Could not enable ACPI mode" — a
/// hard boot failure and an obvious VM tell. Writing [`ACPI_DISABLE_VALUE`] clears
/// `SCI_EN` (legacy mode); any other command byte is stored but otherwise ignored.
pub struct SmiCommandPort {
    /// The PM1 block whose `SCI_EN` this port toggles.
    pm1: SharedAcpiPm1Block,
    /// The last command byte written (read back from `0xB2`).
    last_command: u8,
}

impl PioDevice for SmiCommandPort {
    fn pio_read(&mut self, _port: u16, _size: u8) -> u32 {
        u32::from(self.last_command)
    }

    fn pio_write(&mut self, _port: u16, _size: u8, data: u32) {
        let command = u8_of(data);
        self.last_command = command;
        match command {
            ACPI_ENABLE_VALUE => self.pm1.with(|b| b.set_acpi_mode(true)),
            ACPI_DISABLE_VALUE => self.pm1.with(|b| b.set_acpi_mode(false)),
            _ => {}
        }
    }

    fn port_range(&self) -> (u16, u16) {
        (SMI_CMD_PORT, SMI_CMD_PORT + 1)
    }
}

/// The `PM1a` register block (`0x600`..`0x606`) as a bus [`PioDevice`], backed by
/// a [`SharedAcpiPm1Block`] so a guest's sleep write is visible to the run loop.
pub struct Pm1Port {
    pm1: SharedAcpiPm1Block,
}

impl PioDevice for Pm1Port {
    fn pio_read(&mut self, port: u16, size: u8) -> u32 {
        self.pm1.with(|b| b.pio_read(port, size))
    }

    fn pio_write(&mut self, port: u16, size: u8, data: u32) {
        self.pm1.with(|b| b.pio_write(port, size, data));
    }

    fn port_range(&self) -> (u16, u16) {
        (PM1_EVT_PORT, PM1_BLOCK_END)
    }
}

/// I/O port of the **`GPE0` block** (`GPE0_BLK`), the General-Purpose Event
/// register block the FADT advertises (length 16 here: 8 status bytes followed by
/// 8 enable bytes).
pub const GPE0_PORT: u16 = 0x620;

/// Number of status (and, separately, enable) bytes in the `GPE0` block — half of
/// the 16-byte block length the FADT advertises.
const GPE0_HALF: u16 = 8;

/// One-past-the-end of the `GPE0` block (`0x620`..`0x630`).
const GPE0_END: u16 = GPE0_PORT + 2 * GPE0_HALF;

/// The **ACPI General-Purpose Event 0 block** (`GPE0_BLK`) as a bus [`PioDevice`].
///
/// A guest's ACPICA reads and clears these registers during ACPI init. The block
/// is two byte arrays: a write-1-to-clear **status** array (`0x620`..`0x627`) and
/// a read/write **enable** array (`0x628`..`0x62F`). Modelling it matters even
/// with no GPEs wired: left as open bus the **status** bytes read back `0xFF`, so
/// the OS would see every GPE asserted and spin trying to dispatch handlers for
/// events that never happened. Here the status starts clear (no event pending)
/// and the enable array round-trips, so ACPI init is quiet. The host raises a
/// real event with [`raise_gpe`](Self::raise_gpe); generating the SCI from it is a
/// run-loop concern and is deferred.
#[derive(Debug, Clone, Default)]
pub struct Gpe0Block {
    /// `GPE0_STS` — write-1-to-clear status bits, one bit per GPE.
    status: [u8; GPE0_HALF as usize],
    /// `GPE0_EN` — per-GPE interrupt-enable bits.
    enable: [u8; GPE0_HALF as usize],
}

impl Gpe0Block {
    /// A new block with no events pending and all GPEs disabled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            status: [0; GPE0_HALF as usize],
            enable: [0; GPE0_HALF as usize],
        }
    }

    /// Assert general-purpose event number `gpe` (sets its status bit), as a
    /// host-side wake/notify source would. The run loop later turns an asserted,
    /// enabled GPE into an SCI.
    pub const fn raise_gpe(&mut self, gpe: usize) {
        let byte = gpe / 8;
        if byte < self.status.len() {
            self.status[byte] |= 1 << (gpe % 8);
        }
    }
}

impl PioDevice for Gpe0Block {
    fn pio_read(&mut self, port: u16, _size: u8) -> u32 {
        let (arr, base) = if port < GPE0_PORT + GPE0_HALF {
            (&self.status, GPE0_PORT)
        } else {
            (&self.enable, GPE0_PORT + GPE0_HALF)
        };
        let off = usize::from(port - base);
        // Pack up to four bytes from the array; the bus keeps the low `size`.
        let mut val = 0u32;
        let mut i = 0;
        while i < 4 && off + i < arr.len() {
            val |= u32::from(arr[off + i]) << (i * 8);
            i += 1;
        }
        val
    }

    fn pio_write(&mut self, port: u16, size: u8, data: u32) {
        let count = if size == 0 { 1 } else { size };
        for i in 0..u16::from(count) {
            let p = port + i;
            if p >= GPE0_END {
                break;
            }
            let byte = u8_of(data >> (i * 8));
            if p < GPE0_PORT + GPE0_HALF {
                // Status: write-1-to-clear.
                self.status[usize::from(p - GPE0_PORT)] &= !byte;
            } else {
                // Enable: stored.
                self.enable[usize::from(p - (GPE0_PORT + GPE0_HALF))] = byte;
            }
        }
    }

    fn port_range(&self) -> (u16, u16) {
        (GPE0_PORT, GPE0_END)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        A20_GATE, ACPI_DISABLE_VALUE, ACPI_ENABLE_VALUE, AcpiPm1Block, GPE0_PORT, Gpe0Block,
        PM1_CNT_PORT, PM1_EVT_PORT, PORT_A, SMI_CMD_PORT, SharedAcpiPm1Block,
        SharedSystemControlPortA, SystemControlPortA,
    };
    use crate::bus::PioDevice;

    #[test]
    fn claims_only_port_0x92() {
        assert_eq!(SystemControlPortA::new().port_range(), (0x92, 0x93));
    }

    #[test]
    fn a20_is_enabled_by_default_and_reads_back_set() {
        let mut port = SystemControlPortA::new();
        assert!(port.a20_enabled());
        assert_ne!(port.pio_read(PORT_A, 1) & u32::from(A20_GATE), 0);
    }

    #[test]
    fn writing_bit1_toggles_the_a20_gate() {
        let mut port = SystemControlPortA::new();
        // Clear A20.
        port.pio_write(PORT_A, 1, 0x00);
        assert!(!port.a20_enabled());
        assert_eq!(port.pio_read(PORT_A, 1) & u32::from(A20_GATE), 0);
        // Re-enable it.
        port.pio_write(PORT_A, 1, u32::from(A20_GATE));
        assert!(port.a20_enabled());
    }

    #[test]
    fn writing_bit0_latches_a_one_shot_reset_request() {
        let mut port = SystemControlPortA::new();
        assert!(!port.take_reset(), "no reset pending at start");

        // Pulse fast reset together with A20 enabled.
        port.pio_write(PORT_A, 1, 0x03);
        assert!(port.take_reset(), "reset latched");
        assert!(!port.take_reset(), "and consumed exactly once");
        // A20 still set; the reset edge did not persist.
        assert!(port.a20_enabled());
    }

    #[test]
    fn the_reset_bit_always_reads_back_zero() {
        let mut port = SystemControlPortA::new();
        port.pio_write(PORT_A, 1, 0xFF);
        assert_eq!(port.pio_read(PORT_A, 1) & 0x01, 0, "bit 0 reads 0");
    }

    #[test]
    fn pm1_block_claims_0x600_through_0x605() {
        assert_eq!(AcpiPm1Block::new().port_range(), (0x600, 0x606));
    }

    #[test]
    fn writing_slp_typ_and_slp_en_latches_a_one_shot_sleep_request() {
        let mut pm1 = AcpiPm1Block::new();
        assert_eq!(pm1.take_sleep(), None);

        // The DSDT's _S5 object typically yields SLP_TYP = 5; an OS shutting down
        // writes (5 << 10) | SLP_EN(1<<13) to the control register.
        let s5 = (5u16 << 10) | (1 << 13);
        pm1.pio_write(PM1_CNT_PORT, 2, u32::from(s5));
        assert_eq!(pm1.take_sleep(), Some(5), "S5 sleep type captured");
        assert_eq!(pm1.take_sleep(), None, "consumed exactly once");

        // SLP_EN is write-only: the control register reads it back as 0.
        assert_eq!(pm1.pio_read(PM1_CNT_PORT, 2) & (1 << 13), 0);
    }

    #[test]
    fn sci_enable_sticks_but_sleep_bits_do_not() {
        let mut pm1 = AcpiPm1Block::new();
        // Entering ACPI mode sets SCI_EN (bit 0); it persists and reads back.
        pm1.pio_write(PM1_CNT_PORT, 2, 1);
        assert!(pm1.sci_enabled());
        assert_eq!(pm1.pio_read(PM1_CNT_PORT, 2) & 1, 1);
        // A later S5 write must not capture a sleep unless SLP_EN is set.
        pm1.pio_write(PM1_CNT_PORT, 2, (5 << 10) | 1);
        assert_eq!(pm1.take_sleep(), None, "no SLP_EN, no transition");
    }

    #[test]
    fn smi_command_acpi_enable_sets_sci_en() {
        let pm1 = SharedAcpiPm1Block::new();
        let mut smi = pm1.smi_command_port();
        assert_eq!(PioDevice::port_range(&smi), (0xB2, 0xB3));
        assert!(!pm1.with(|b| b.sci_enabled()));

        // The OS writes ACPI_ENABLE (0xA0) to 0xB2 — SCI_EN must come up so its
        // poll of PM1a_CNT succeeds.
        smi.pio_write(SMI_CMD_PORT, 1, u32::from(ACPI_ENABLE_VALUE));
        assert!(pm1.with(|b| b.sci_enabled()), "ACPI mode entered");
        // The command byte reads back.
        assert_eq!(
            smi.pio_read(SMI_CMD_PORT, 1) & 0xFF,
            u32::from(ACPI_ENABLE_VALUE)
        );

        // ACPI_DISABLE (0xA1) drops back to legacy mode.
        smi.pio_write(SMI_CMD_PORT, 1, u32::from(ACPI_DISABLE_VALUE));
        assert!(!pm1.with(|b| b.sci_enabled()), "ACPI mode left");
    }

    #[test]
    fn smi_command_ignores_unknown_bytes() {
        let pm1 = SharedAcpiPm1Block::new();
        let mut smi = pm1.smi_command_port();
        smi.pio_write(SMI_CMD_PORT, 1, 0x12);
        assert!(!pm1.with(|b| b.sci_enabled()), "unknown command is a no-op");
        assert_eq!(smi.pio_read(SMI_CMD_PORT, 1) & 0xFF, 0x12);
    }

    #[test]
    fn power_button_status_is_write_one_to_clear() {
        let mut pm1 = AcpiPm1Block::new();
        pm1.press_power_button();
        // PWRBTN_STS (bit 8) is set in the status register at 0x600.
        assert_ne!(pm1.pio_read(PM1_EVT_PORT, 2) & (1 << 8), 0);
        // Write 1 to that bit to clear it (ACPI write-1-to-clear semantics).
        pm1.pio_write(PM1_EVT_PORT, 2, 1 << 8);
        assert_eq!(pm1.pio_read(PM1_EVT_PORT, 2) & (1 << 8), 0);
    }

    #[test]
    fn enable_register_round_trips_through_the_high_half() {
        let mut pm1 = AcpiPm1Block::new();
        // PM1a_EN is at 0x602; a 16-bit write there sets the enable bits.
        pm1.pio_write(0x602, 2, 1 << 8); // PWRBTN_EN
        assert_eq!(pm1.pio_read(0x602, 2) & 0xFFFF, 1 << 8);
        // A 32-bit read of the event block sees enable in the high half.
        let evt = pm1.pio_read(PM1_EVT_PORT, 4);
        assert_eq!(evt >> 16, u32::from(1u16 << 8));
    }

    #[test]
    fn shared_port_a_reset_through_the_bus_reaches_the_run_loop_handle() {
        let shared = SharedSystemControlPortA::new();
        let mut port = shared.port();
        assert!(shared.a20_enabled());
        assert!(!shared.take_reset());

        // A guest pulses fast reset (bit 0) through the bus port.
        port.pio_write(PORT_A, 1, 0x01);
        assert!(shared.take_reset(), "reset seen by the run-loop handle");
        assert!(!shared.take_reset(), "consumed once");
    }

    #[test]
    fn gpe0_claims_the_16_byte_block_and_starts_quiet() {
        let mut gpe = Gpe0Block::new();
        assert_eq!(gpe.port_range(), (0x620, 0x630));
        // Status reads back 0 (no event pending) rather than open-bus 0xFF, so
        // ACPI init does not see phantom GPEs.
        for off in 0..8u16 {
            assert_eq!(gpe.pio_read(GPE0_PORT + off, 1) & 0xFF, 0);
        }
    }

    #[test]
    fn gpe0_status_is_write_one_to_clear_and_enable_round_trips() {
        let mut gpe = Gpe0Block::new();
        // Raise GPE 9 (status byte 1, bit 1) as a host event would.
        gpe.raise_gpe(9);
        assert_eq!(gpe.pio_read(GPE0_PORT + 1, 1) & 0xFF, 1 << 1);
        // Write-1-to-clear it through the status port.
        gpe.pio_write(GPE0_PORT + 1, 1, 1 << 1);
        assert_eq!(gpe.pio_read(GPE0_PORT + 1, 1) & 0xFF, 0);

        // The enable array (second half) stores what the guest writes.
        gpe.pio_write(GPE0_PORT + 8, 1, 0xAA);
        assert_eq!(gpe.pio_read(GPE0_PORT + 8, 1) & 0xFF, 0xAA);
    }

    #[test]
    fn shutdown_through_the_bus_port_reaches_the_run_loop_handle() {
        // The end-to-end shutdown path: a guest writes the S5 transition through
        // the bus adapter, and the run loop's clone observes the sleep request.
        let pm1 = SharedAcpiPm1Block::new();
        let mut port = pm1.port();
        assert_eq!(pm1.take_sleep(), None);

        let s5 = (5u16 << 10) | (1 << 13); // SLP_TYP=5 | SLP_EN
        port.pio_write(PM1_CNT_PORT, 2, u32::from(s5));
        assert_eq!(pm1.take_sleep(), Some(5), "guest S5 write seen by the host");
    }
}

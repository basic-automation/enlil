//! Bare-metal xHCI (USB 3.x) host controller driver — Phase 6.5, T-6.18.
//!
//! This is the production-path replacement for libusb/VFIO on bare metal:
//! the driver that owns the physical xHCI controller once the kernel has taken
//! the machine from UEFI. It is written for the `no_std` firmware target
//! (`core` + `alloc` only) and every piece of wire-format logic is pure and
//! host-tested — only the volatile register accesses are gated to
//! `target_os = "uefi"`.
//!
//! # Layout
//!
//! * Register map — capability/operational/port/runtime/doorbell offsets and
//!   `const` field decoders (xHCI 1.2 §5.3–5.5). Pure.
//! * [`Trb`] — the 16-byte Transfer Request Block wire format: builders for
//!   transfer TRBs (Setup/Data/Status/Normal/Link), command TRBs
//!   (Enable Slot, Address Device, Evaluate Context, No-Op) and decoders for
//!   event TRBs (Transfer/Command-Completion/Port-Status-Change). Pure.
//! * [`ProducerRing`] / [`ConsumerRing`] — the cycle-bit enqueue/dequeue
//!   discipline for command, transfer and event rings (xHCI §4.9). Pure.
//! * [`Mmio`] — volatile register window; [`VolatileMmio`] on the firmware
//!   target, [`SliceMmio`] (a byte slice) for host tests.
//! * [`DmaRegion`] — page-aligned, physically-contiguous DMA memory. The
//!   kernel identity-maps all of RAM, so virtual == physical here.
//! * [`Op`] — the controller's operational face: probe, reset, run, port
//!   scan. This is what the boot kernel drives (init + port routing).
//! * [`Controller`] — the full driver: command/event rings, slot management,
//!   control and bulk transfer submission, hot-plug event handling.
//! * USB enumeration helpers — setup packets and the device-descriptor
//!   decode used by the Address Device / `GET_DESCRIPTOR` flow.
//!
//! The wire formats mirror `enlil-devices`' virtual xHCI device model
//! (`usb::xhci::{trb, ring, registers, context}`) — the two sides speak the
//! same TRBs, contexts and register layouts, one as the emulated device, one
//! as the bare-metal host driver.
//!
//! Reference: xHCI specification 1.2 (register/TRB/context layouts).

// The allocator paths differ between the firmware target (`alloc`, via the
// crate root's `extern crate alloc`) and the host (where `alloc` is reached
// through `std`).
#[cfg(target_os = "uefi")]
use alloc::alloc::{Layout, alloc_zeroed, dealloc};
#[cfg(not(target_os = "uefi"))]
use std::alloc::{Layout, alloc_zeroed, dealloc};

// ---------------------------------------------------------------------------
// 1. Register map (xHCI 1.2 §5.3–5.5) — pure offsets and field decoders
// ---------------------------------------------------------------------------

/// PCI base class: serial bus controllers.
pub const PCI_CLASS_SERIAL_BUS: u8 = 0x0C;
/// PCI subclass: USB controllers.
pub const PCI_SUBCLASS_USB: u8 = 0x03;
/// PCI programming interface: xHCI.
pub const PCI_PROGIF_XHCI: u8 = 0x30;

/// Whether a PCI class triple identifies an xHCI host controller.
///
/// Pure so the PCI scan and the host tests share the predicate.
#[must_use]
pub const fn is_xhci_function(class: u8, subclass: u8, prog_if: u8) -> bool {
    class == PCI_CLASS_SERIAL_BUS && subclass == PCI_SUBCLASS_USB && prog_if == PCI_PROGIF_XHCI
}

/// Extract an 8-bit field: `(value >> shift) & 0xFF`.
#[must_use]
pub const fn u8_field(value: u32, shift: u32) -> u8 {
    ((value >> shift) & 0xFF) as u8
}

/// Extract a 16-bit field: `(value >> shift) & 0xFFFF`.
#[must_use]
pub const fn u16_field(value: u32, shift: u32) -> u16 {
    ((value >> shift) & 0xFFFF) as u16
}

// --- Capability registers (offsets from the MMIO base, xHCI §5.3) ---
//
// Dword 0 packs CAPLENGTH (§5.3.1, byte 0x00) and HCIVERSION (§5.3.2, bytes
// 0x02–0x03); `probe_caps` decodes both from the one dword read.
/// HCSPARAMS1: structural parameters 1 (u32 at 0x04).
pub const CAP_HCSPARAMS1: u32 = 0x04;
/// HCSPARAMS2: structural parameters 2 (u32 at 0x08).
pub const CAP_HCSPARAMS2: u32 = 0x08;
/// HCCPARAMS1: capability parameters 1 (u32 at 0x10).
pub const CAP_HCCPARAMS1: u32 = 0x10;
/// DBOFF: doorbell array offset (u32 at 0x14).
pub const CAP_DBOFF: u32 = 0x14;
/// RTSOFF: runtime register space offset (u32 at 0x18).
pub const CAP_RTSOFF: u32 = 0x18;

/// HCSPARAMS1: `MaxSlots` field (bits 7:0).
#[must_use]
pub const fn hcsparams1_max_slots(v: u32) -> u8 {
    u8_field(v, 0)
}

/// HCSPARAMS1: `MaxIntrs` field (bits 18:8).
#[must_use]
pub const fn hcsparams1_max_interrupters(v: u32) -> u16 {
    u16_field(v, 8) & 0x07FF
}

/// HCSPARAMS1: `MaxPorts` field (bits 31:24).
#[must_use]
pub const fn hcsparams1_max_ports(v: u32) -> u8 {
    u8_field(v, 24)
}

/// HCSPARAMS2: ERST Max field (bits 7:4) — log2 of the max ERST entries.
#[must_use]
pub const fn hcsparams2_erst_max(v: u32) -> u8 {
    u8_field(v, 4) & 0x0F
}

/// HCCPARAMS1 bit 0 — 64-bit addressing capability.
pub const HCCPARAMS1_AC64: u32 = 1 << 0;
/// HCCPARAMS1 bit 2 — 64-byte device/endpoint contexts (else 32-byte).
pub const HCCPARAMS1_CSZ: u32 = 1 << 2;
/// HCCPARAMS1 bits 31:16 — xHCI Extended Capabilities pointer (dwords).
#[must_use]
pub const fn hccparams1_xecp(v: u32) -> u16 {
    u16_field(v, 16)
}

/// DBOFF holds the doorbell array's byte offset with the low two bits
/// reserved (xHCI §5.3.7: dword offset, i.e. value already shifted).
#[must_use]
pub const fn dboff_to_bytes(v: u32) -> u32 {
    v & !0x3
}

/// RTSOFF holds the runtime space's byte offset with the low five bits
/// reserved (xHCI §5.3.8: 32-byte granularity).
#[must_use]
pub const fn rtsoff_to_bytes(v: u32) -> u32 {
    v & !0x1F
}

// --- Operational registers (offsets from the operational base, xHCI §5.4) ---

/// USBCMD: command register (0x00).
pub const OP_USBCMD: u32 = 0x00;
/// USBSTS: status register (0x04).
pub const OP_USBSTS: u32 = 0x04;
/// PAGESIZE: supported page sizes (0x08).
pub const OP_PAGESIZE: u32 = 0x08;
/// CRCR: command ring control, 64-bit (0x18).
pub const OP_CRCR: u32 = 0x18;
/// DCBAAP: device context base address array pointer, 64-bit (0x30).
pub const OP_DCBAAP: u32 = 0x30;
/// CONFIG: configure register (0x38).
pub const OP_CONFIG: u32 = 0x38;

/// USBCMD bit 0 — Run/Stop.
pub const USBCMD_RS: u32 = 1 << 0;
/// USBCMD bit 1 — Host Controller Reset.
pub const USBCMD_HCRST: u32 = 1 << 1;
/// USBCMD bit 2 — Interrupter Enable.
pub const USBCMD_INTE: u32 = 1 << 2;

/// USBSTS bit 0 — `HCHalted`.
pub const USBSTS_HCHALTED: u32 = 1 << 0;
/// USBSTS bit 11 — Controller Not Ready.
pub const USBSTS_CNR: u32 = 1 << 11;

/// CRCR bit 0 — Ring Cycle State.
pub const CRCR_RCS: u64 = 1 << 0;
/// CRCR pointer mask (bits 63:6).
pub const CRCR_PTR_MASK: u64 = !0x3F;

/// CONFIG `MaxSlotsEn` field (bits 7:0).
#[must_use]
pub const fn config_max_slots_en(v: u32) -> u8 {
    u8_field(v, 0)
}

// --- Port register sets (xHCI §5.4.8) ---

/// Port register sets start here, past the operational registers.
pub const PORT_REGS_OFFSET: u32 = 0x400;
/// Byte stride between consecutive port register sets.
pub const PORT_REGS_STRIDE: u32 = 0x10;

/// Byte offset of port `port`'s PORTSC (ports are 1-based).
#[must_use]
pub const fn portsc_offset(port: u8) -> u32 {
    PORT_REGS_OFFSET + (port as u32 - 1) * PORT_REGS_STRIDE
}

/// PORTSC bit 0 — Current Connect Status.
pub const PORTSC_CCS: u32 = 1 << 0;
/// PORTSC bit 1 — Port Enabled/Disabled.
pub const PORTSC_PED: u32 = 1 << 1;
/// PORTSC bit 4 — Port Reset.
pub const PORTSC_PR: u32 = 1 << 4;
/// PORTSC bits 8:5 — Port Link State.
pub const PORTSC_PLS_SHIFT: u32 = 5;
pub const PORTSC_PLS_MASK: u32 = 0xF << PORTSC_PLS_SHIFT;
/// PORTSC bit 9 — Port Power.
pub const PORTSC_PP: u32 = 1 << 9;
/// PORTSC bits 13:10 — Port Speed ID.
pub const PORTSC_SPEED_SHIFT: u32 = 10;
pub const PORTSC_SPEED_MASK: u32 = 0xF << PORTSC_SPEED_SHIFT;
/// PORTSC write-1-to-clear change bits: CSC/PEC/WRC/OCC/PRC/PLC/CEC.
pub const PORTSC_CHANGE_BITS: u32 = 0x00FE_0000;

/// Port Link State: U0 (active).
pub const PLS_U0: u8 = 0;
/// Port Link State: Polling.
pub const PLS_POLLING: u8 = 7;

/// USB speed IDs (xHCI §5.4.8 / USB).
pub const SPEED_FULL: u8 = 1;
pub const SPEED_LOW: u8 = 2;
pub const SPEED_HIGH: u8 = 3;
pub const SPEED_SUPER: u8 = 4;
pub const SPEED_SUPER_PLUS: u8 = 5;

/// A decoded port speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSpeed {
    /// Full Speed (12 Mb/s).
    Full,
    /// Low Speed (1.5 Mb/s).
    Low,
    /// High Speed (480 Mb/s).
    High,
    /// `SuperSpeed` (5 Gb/s).
    Super,
    /// `SuperSpeedPlus` (10 Gb/s).
    SuperPlus,
    /// Reserved/unknown speed ID.
    Unknown,
}

impl PortSpeed {
    /// Decode a PORTSC speed ID.
    #[must_use]
    pub const fn from_id(id: u8) -> Self {
        match id {
            SPEED_FULL => Self::Full,
            SPEED_LOW => Self::Low,
            SPEED_HIGH => Self::High,
            SPEED_SUPER => Self::Super,
            SPEED_SUPER_PLUS => Self::SuperPlus,
            _ => Self::Unknown,
        }
    }
}

/// Decoded PORTSC state for one root-hub port.
///
/// Carries the raw register; the state bits decode through the methods, so
/// the struct never accumulates stale copies of the individual flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortInfo {
    /// 1-based port number.
    pub port: u8,
    /// The raw PORTSC dword.
    pub portsc: u32,
}

impl PortInfo {
    /// A device is attached (CCS).
    #[must_use]
    pub const fn connected(self) -> bool {
        self.portsc & PORTSC_CCS != 0
    }

    /// The port finished reset and is usable (PED).
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.portsc & PORTSC_PED != 0
    }

    /// The port is powered (PP).
    #[must_use]
    pub const fn powered(self) -> bool {
        self.portsc & PORTSC_PP != 0
    }

    /// Port Link State (`PLS_*`).
    #[must_use]
    pub const fn link_state(self) -> u8 {
        ((self.portsc & PORTSC_PLS_MASK) >> PORTSC_PLS_SHIFT) as u8
    }

    /// Negotiated speed.
    #[must_use]
    pub const fn speed(self) -> PortSpeed {
        PortSpeed::from_id(((self.portsc & PORTSC_SPEED_MASK) >> PORTSC_SPEED_SHIFT) as u8)
    }

    /// Any change bit set (CSC/PEC/WRC/OCC/PRC/PLC/CEC) — needs acknowledgement.
    #[must_use]
    pub const fn change_pending(self) -> bool {
        self.portsc & PORTSC_CHANGE_BITS != 0
    }
}

/// Decode one PORTSC dword into [`PortInfo`].
#[must_use]
pub const fn decode_portsc(port: u8, portsc: u32) -> PortInfo {
    PortInfo { port, portsc }
}

// --- Runtime registers (xHCI §5.5.2) ---

/// Interrupter 0's register set starts 0x20 past the runtime base.
pub const INTR_SET_OFFSET: u32 = 0x20;
/// Stride between interrupter register sets.
pub const INTR_SET_STRIDE: u32 = 0x20;

/// IMAN: interrupter management (+0x00 in the set).
pub const INTR_IMAN: u32 = 0x00;
/// IMOD: interrupter moderation (+0x04).
pub const INTR_IMOD: u32 = 0x04;
/// ERSTSZ: event ring segment table size (+0x08).
pub const INTR_ERSTSZ: u32 = 0x08;
/// ERSTBA: event ring segment table base, 64-bit (+0x10).
pub const INTR_ERSTBA: u32 = 0x10;
/// ERDP: event ring dequeue pointer, 64-bit (+0x18).
pub const INTR_ERDP: u32 = 0x18;

/// IMAN bit 0 — Interrupt Pending (write-1-to-clear).
pub const IMAN_IP: u32 = 1 << 0;
/// IMAN bit 1 — Interrupt Enable.
pub const IMAN_IE: u32 = 1 << 1;

/// ERDP bit 3 — Event Handler Busy (write-1-to-clear).
pub const ERDP_EHB: u64 = 1 << 3;
/// ERDP dequeue-pointer mask (bits 63:4).
pub const ERDP_PTR_MASK: u64 = !0xF;

/// Byte offset of interrupter `n`'s register set from the MMIO base.
#[must_use]
pub const fn interrupter_base(rts_offset: u32, n: u8) -> u32 {
    rts_offset + INTR_SET_OFFSET + n as u32 * INTR_SET_STRIDE
}

// --- Doorbell registers (xHCI §5.6) ---

/// Doorbell register stride: one dword per slot.
pub const DOORBELL_STRIDE: u32 = 4;

/// Byte offset of slot `slot`'s doorbell (slot 0 = the host controller).
#[must_use]
pub const fn doorbell_offset(db_offset: u32, slot: u8) -> u32 {
    db_offset + slot as u32 * DOORBELL_STRIDE
}

/// Doorbell target: the host controller command ring.
pub const DB_TARGET_HOST: u32 = 0;
/// Build a doorbell value: target in bits 7:0, stream ID in bits 15:8.
///
/// The target is the endpoint index, i.e. DCI − 1 (EP0, DCI 1, is target 0);
/// see [`db_target_for_dci`].
#[must_use]
pub const fn doorbell_value(target: u32, stream_id: u16) -> u32 {
    (target & 0xFF) | (stream_id as u32) << 8
}

// ---------------------------------------------------------------------------
// 2. TRB wire format (xHCI 1.2 §6.4) — 16 bytes: parameter/status/control
// ---------------------------------------------------------------------------

/// TRB type codes (control bits 15:10).
pub mod trb_type {
    /// Normal transfer TRB.
    pub const NORMAL: u32 = 1;
    /// Setup Stage TRB.
    pub const SETUP_STAGE: u32 = 2;
    /// Data Stage TRB.
    pub const DATA_STAGE: u32 = 3;
    /// Status Stage TRB.
    pub const STATUS_STAGE: u32 = 4;
    /// Link TRB.
    pub const LINK: u32 = 6;
    /// Enable Slot command.
    pub const ENABLE_SLOT: u32 = 9;
    /// Disable Slot command.
    pub const DISABLE_SLOT: u32 = 10;
    /// Address Device command.
    pub const ADDRESS_DEVICE: u32 = 11;
    /// Configure Endpoint command.
    pub const CONFIGURE_ENDPOINT: u32 = 12;
    /// Evaluate Context command.
    pub const EVALUATE_CONTEXT: u32 = 13;
    /// Reset Endpoint command.
    pub const RESET_ENDPOINT: u32 = 14;
    /// Stop Endpoint command.
    pub const STOP_ENDPOINT: u32 = 15;
    /// Set TR Dequeue Pointer command.
    pub const SET_TR_DEQUEUE: u32 = 16;
    /// Reset Device command.
    pub const RESET_DEVICE: u32 = 17;
    /// No-Op command.
    pub const NOOP_COMMAND: u32 = 23;
    /// Transfer Event.
    pub const TRANSFER_EVENT: u32 = 32;
    /// Command Completion Event.
    pub const COMMAND_COMPLETION: u32 = 33;
    /// Port Status Change Event.
    pub const PORT_STATUS_CHANGE: u32 = 34;
}

/// Control dword (DW3) bit positions shared by TRB kinds.
pub mod trb_ctl {
    /// Bit 0 — Cycle bit.
    pub const CYCLE: u32 = 1 << 0;
    /// Link TRB bit 1 — Toggle Cycle.
    pub const TOGGLE_CYCLE: u32 = 1 << 1;
    /// Bit 2 — Evaluate Next TRB.
    pub const ENT: u32 = 1 << 2;
    /// Bit 3 — Interrupt-on Short Packet.
    pub const ISP: u32 = 1 << 3;
    /// Bit 4 — Chain bit.
    pub const CHAIN: u32 = 1 << 4;
    /// Bit 5 — Interrupt On Completion.
    pub const IOC: u32 = 1 << 5;
    /// Bit 6 — Immediate Data.
    pub const IDT: u32 = 1 << 6;
    /// Shift of the TRB Type field.
    pub const TYPE_SHIFT: u32 = 10;
    /// Shift of the Interrupter Target field.
    pub const INTR_TARGET_SHIFT: u32 = 16;
    /// Status Stage bit 16 — direction (1 = IN status).
    pub const STATUS_DIR_IN: u32 = 1 << 16;
    /// Address Device bit 9 — Block Set Address Request.
    pub const BSR: u32 = 1 << 9;
    /// Shift of the Slot ID field in command/event TRBs.
    pub const SLOT_SHIFT: u32 = 24;
    /// Shift of the Endpoint ID field in transfer events.
    pub const ENDPOINT_SHIFT: u32 = 16;
}

/// Setup Stage transfer-type (TRT) values (DW3 bits 23:22).
pub mod trt {
    /// No data stage.
    pub const NO_DATA: u32 = 0;
    /// OUT data stage.
    pub const OUT: u32 = 2;
    /// IN data stage.
    pub const IN: u32 = 3;
}

/// A 16-byte Transfer Request Block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Trb {
    /// Parameter dword pair (bytes 0–7).
    pub parameter: u64,
    /// Status dword (bytes 8–11).
    pub status: u32,
    /// Control dword (bytes 12–15).
    pub control: u32,
}

impl Trb {
    /// The all-zero TRB (a cycle-bit-0 placeholder).
    #[must_use]
    pub const fn zeroed() -> Self {
        Self {
            parameter: 0,
            status: 0,
            control: 0,
        }
    }

    /// Decode from 16 little-endian bytes.
    #[must_use]
    pub const fn from_bytes(bytes: &[u8; 16]) -> Self {
        Self {
            parameter: u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            status: u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            control: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
        }
    }

    /// Encode to 16 little-endian bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        let p = self.parameter.to_le_bytes();
        let s = self.status.to_le_bytes();
        let c = self.control.to_le_bytes();
        [
            p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7], s[0], s[1], s[2], s[3], c[0], c[1],
            c[2], c[3],
        ]
    }

    /// The Cycle bit (control bit 0).
    #[must_use]
    pub const fn cycle_bit(self) -> bool {
        self.control & trb_ctl::CYCLE != 0
    }

    /// Set the Cycle bit.
    #[must_use]
    pub const fn with_cycle(mut self, cycle: bool) -> Self {
        if cycle {
            self.control |= trb_ctl::CYCLE;
        } else {
            self.control &= !trb_ctl::CYCLE;
        }
        self
    }

    /// The TRB type (control bits 15:10).
    #[must_use]
    pub const fn trb_type(self) -> u32 {
        (self.control >> trb_ctl::TYPE_SHIFT) & 0x3F
    }

    /// Slot ID field (control bits 31:24) — command and event TRBs.
    #[must_use]
    pub const fn slot_id(self) -> u8 {
        u8_field(self.control, trb_ctl::SLOT_SHIFT)
    }

    /// Completion code (status bits 31:24) — event TRBs.
    #[must_use]
    pub const fn completion_code(self) -> u8 {
        u8_field(self.status, 24)
    }

    /// Transfer length / residue (status bits 23:0) — transfer events.
    #[must_use]
    pub const fn transfer_residue(self) -> u32 {
        self.status & 0x00FF_FFFF
    }

    /// Endpoint ID (control bits 20:16) — transfer events.
    #[must_use]
    pub const fn endpoint_id(self) -> u8 {
        u8_field(self.control, trb_ctl::ENDPOINT_SHIFT) & 0x1F
    }

    /// Port ID (parameter bits 31:24) — port status change events.
    #[must_use]
    pub const fn event_port_id(self) -> u8 {
        ((self.parameter >> 24) & 0xFF) as u8
    }

    /// Command TRB pointer (parameter) — command completion events.
    #[must_use]
    pub const fn command_trb_pointer(self) -> u64 {
        self.parameter
    }

    /// A Normal transfer TRB (xHCI §6.4.1.1): bulk/interrupt data.
    #[must_use]
    pub const fn normal(data_phys: u64, length: u32, ioc: bool, chain: bool, cycle: bool) -> Self {
        let mut control = trb_type::NORMAL << trb_ctl::TYPE_SHIFT;
        if ioc {
            control |= trb_ctl::IOC;
        }
        if chain {
            control |= trb_ctl::CHAIN;
        }
        Self {
            parameter: data_phys,
            status: length & 0x1_FFFF,
            control,
        }
        .with_cycle(cycle)
    }

    /// A Setup Stage TRB (xHCI §6.4.1.2.1): the 8-byte setup packet.
    #[must_use]
    pub const fn setup(setup: &[u8; 8], trt: u32, cycle: bool) -> Self {
        let parameter = u64::from_le_bytes(*setup);
        let control =
            (trb_type::SETUP_STAGE << trb_ctl::TYPE_SHIFT) | trb_ctl::CHAIN | ((trt & 0x3) << 22);
        Self {
            parameter,
            status: 8, // TRB Transfer Length: the setup packet is 8 bytes
            control,
        }
        .with_cycle(cycle)
    }

    /// A Data Stage TRB (xHCI §6.4.1.2.2).
    #[must_use]
    pub const fn data(data_phys: u64, length: u32, ioc: bool, chain: bool, cycle: bool) -> Self {
        let mut control = trb_type::DATA_STAGE << trb_ctl::TYPE_SHIFT;
        if ioc {
            control |= trb_ctl::IOC;
        }
        if chain {
            control |= trb_ctl::CHAIN;
        }
        Self {
            parameter: data_phys,
            status: length & 0x1_FFFF,
            control,
        }
        .with_cycle(cycle)
    }

    /// A Status Stage TRB (xHCI §6.4.1.2.3): `dir_in` selects IN status.
    #[must_use]
    pub const fn status_stage(dir_in: bool, cycle: bool) -> Self {
        let mut control = trb_type::STATUS_STAGE << trb_ctl::TYPE_SHIFT | trb_ctl::IOC;
        if dir_in {
            control |= trb_ctl::STATUS_DIR_IN;
        }
        Self {
            parameter: 0,
            status: 0,
            control,
        }
        .with_cycle(cycle)
    }

    /// A Link TRB chaining a ring segment (xHCI §6.4.4.1).
    #[must_use]
    pub const fn link(segment_phys: u64, toggle_cycle: bool, cycle: bool) -> Self {
        let mut control = trb_type::LINK << trb_ctl::TYPE_SHIFT;
        if toggle_cycle {
            control |= trb_ctl::TOGGLE_CYCLE;
        }
        Self {
            parameter: segment_phys & !0xF,
            status: 0,
            control,
        }
        .with_cycle(cycle)
    }

    /// An Enable Slot command (xHCI §6.4.3.1).
    #[must_use]
    pub const fn enable_slot(cycle: bool) -> Self {
        Self {
            parameter: 0,
            status: 0,
            control: trb_type::ENABLE_SLOT << trb_ctl::TYPE_SHIFT,
        }
        .with_cycle(cycle)
    }

    /// A Disable Slot command (xHCI §6.4.3.2).
    #[must_use]
    pub const fn disable_slot(slot: u8, cycle: bool) -> Self {
        Self {
            parameter: 0,
            status: 0,
            control: (trb_type::DISABLE_SLOT << trb_ctl::TYPE_SHIFT)
                | (slot as u32) << trb_ctl::SLOT_SHIFT,
        }
        .with_cycle(cycle)
    }

    /// An Address Device command (xHCI §6.4.3.3).
    ///
    /// `bsr` (Block Set Address Request): with it set the device keeps
    /// address 0 — the first step of enumeration, used to read the device
    /// descriptor before an address is assigned.
    #[must_use]
    pub const fn address_device(slot: u8, input_context_phys: u64, bsr: bool, cycle: bool) -> Self {
        let mut control = (trb_type::ADDRESS_DEVICE << trb_ctl::TYPE_SHIFT)
            | (slot as u32) << trb_ctl::SLOT_SHIFT;
        if bsr {
            control |= trb_ctl::BSR;
        }
        Self {
            parameter: input_context_phys & !0xF,
            status: 0,
            control,
        }
        .with_cycle(cycle)
    }

    /// An Evaluate Context command (xHCI §6.4.3.6): re-evaluate the input
    /// context's slot/EP0 fields (e.g. EP0 max packet size after the
    /// descriptor read) without changing device state.
    #[must_use]
    pub const fn evaluate_context(slot: u8, input_context_phys: u64, cycle: bool) -> Self {
        Self {
            parameter: input_context_phys & !0xF,
            status: 0,
            control: (trb_type::EVALUATE_CONTEXT << trb_ctl::TYPE_SHIFT)
                | (slot as u32) << trb_ctl::SLOT_SHIFT,
        }
        .with_cycle(cycle)
    }

    /// A No-Op command (xHCI §6.4.3.16).
    #[must_use]
    pub const fn noop_command(cycle: bool) -> Self {
        Self {
            parameter: 0,
            status: 0,
            control: trb_type::NOOP_COMMAND << trb_ctl::TYPE_SHIFT,
        }
        .with_cycle(cycle)
    }

    /// A Transfer Event TRB (xHCI §6.4.2.1), as a controller would emit it.
    #[must_use]
    pub const fn transfer_event(
        trb_phys: u64,
        residue: u32,
        completion_code: u8,
        slot: u8,
        endpoint: u8,
        cycle: bool,
    ) -> Self {
        Self {
            parameter: trb_phys,
            status: (residue & 0x00FF_FFFF) | (completion_code as u32) << 24,
            control: (trb_type::TRANSFER_EVENT << trb_ctl::TYPE_SHIFT)
                | ((endpoint as u32 & 0x1F) << trb_ctl::ENDPOINT_SHIFT)
                | (slot as u32) << trb_ctl::SLOT_SHIFT,
        }
        .with_cycle(cycle)
    }

    /// A Command Completion Event TRB (xHCI §6.4.2.2).
    #[must_use]
    pub const fn command_completion(
        command_trb_phys: u64,
        completion_code: u8,
        slot: u8,
        cycle: bool,
    ) -> Self {
        Self {
            parameter: command_trb_phys,
            status: (completion_code as u32) << 24,
            control: (trb_type::COMMAND_COMPLETION << trb_ctl::TYPE_SHIFT)
                | (slot as u32) << trb_ctl::SLOT_SHIFT,
        }
        .with_cycle(cycle)
    }

    /// A Port Status Change Event TRB (xHCI §6.4.2.3).
    #[must_use]
    pub const fn port_status_change(port: u8, cycle: bool) -> Self {
        Self {
            parameter: (port as u64) << 24,
            status: 1 << 24, // completion code: success
            control: trb_type::PORT_STATUS_CHANGE << trb_ctl::TYPE_SHIFT,
        }
        .with_cycle(cycle)
    }
}

/// Completion code 1: success.
pub const CC_SUCCESS: u8 = 1;
/// Completion code 13: short packet.
pub const CC_SHORT_PACKET: u8 = 13;

/// Whether a completion code means the transfer/command worked.
#[must_use]
pub const fn completion_ok(code: u8) -> bool {
    code == CC_SUCCESS || code == CC_SHORT_PACKET
}

// ---------------------------------------------------------------------------
// 3. TRB rings (xHCI §4.9) — the cycle-bit producer/consumer discipline
// ---------------------------------------------------------------------------

/// Error constructing a ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    /// Fewer than two TRB slots (one payload slot plus the link TRB).
    TooSmall,
}

/// Producer side of a command or transfer ring.
///
/// The last slot is a Link TRB back to slot 0 (Toggle Cycle set). When the
/// enqueue cursor reaches it, the link's cycle bit is updated to the lap's
/// cycle state, the producer cycle toggles, and enqueue wraps to 0 —
/// exactly the xHCI §4.9.2 wrap protocol. Operates on a plain TRB slice so
/// it is fully host-testable; the live driver backs it with [`DmaRegion`].
pub struct ProducerRing<'a> {
    trbs: &'a mut [Trb],
    /// Physical address of slot 0 (for the link TRB and CRCR reporting).
    base_phys: u64,
    /// Index of the next TRB to write; never the link slot.
    enqueue: usize,
    /// Producer Cycle State.
    cycle: bool,
}

impl<'a> ProducerRing<'a> {
    /// Build a ring over `trbs` (needs ≥ 2 slots) at DMA address
    /// `base_phys`, with initial producer cycle state `cycle`.
    ///
    /// # Errors
    ///
    /// [`RingError::TooSmall`] when the slice holds fewer than two TRBs.
    pub fn new(trbs: &'a mut [Trb], base_phys: u64, cycle: bool) -> Result<Self, RingError> {
        if trbs.len() < 2 {
            return Err(RingError::TooSmall);
        }
        let link = trbs.len() - 1;
        for trb in trbs.iter_mut() {
            *trb = Trb::zeroed();
        }
        trbs[link] = Trb::link(base_phys, true, false);
        Ok(Self {
            trbs,
            base_phys,
            enqueue: 0,
            cycle,
        })
    }

    /// Number of payload TRB slots (excluding the link slot).
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.trbs.len() - 1
    }

    /// Current producer cycle state.
    #[must_use]
    pub const fn cycle(&self) -> bool {
        self.cycle
    }

    /// Physical address the next TRB will be written to.
    #[must_use]
    pub const fn next_phys(&self) -> u64 {
        self.base_phys + (self.enqueue as u64) * 16
    }

    /// Enqueue a TRB built from raw fields; returns the physical address it
    /// was written to (what completion events point back at).
    ///
    /// `control` carries every DW3 bit except the cycle bit, which is set
    /// from the producer cycle state.
    #[allow(clippy::missing_const_for_fn)]
    pub fn enqueue_raw(&mut self, parameter: u64, status: u32, control: u32) -> u64 {
        let link = self.trbs.len() - 1;
        if self.enqueue == link {
            // Wrap: retire the lap by stamping the link TRB with this lap's
            // cycle state, then toggle for the next lap (xHCI §4.9.2).
            self.trbs[link] = self.trbs[link].with_cycle(self.cycle);
            self.cycle = !self.cycle;
            self.enqueue = 0;
        }
        let addr = self.next_phys();
        let trb = Trb {
            parameter,
            status,
            control,
        }
        .with_cycle(self.cycle);
        self.trbs[self.enqueue] = trb;
        self.enqueue += 1;
        addr
    }

    /// Enqueue a fully-built [`Trb`] (its cycle bit is replaced).
    pub fn enqueue_trb(&mut self, trb: Trb) -> u64 {
        self.enqueue_raw(trb.parameter, trb.status, trb.control & !trb_ctl::CYCLE)
    }

    /// The CRCR dequeue value for this ring: current enqueue address plus
    /// the Ring Cycle State bit — what software programs after a stop.
    #[must_use]
    pub const fn crcr_value(&self) -> u64 {
        let mut v = self.next_phys() & CRCR_PTR_MASK;
        if self.cycle {
            v |= CRCR_RCS;
        }
        v
    }

    /// Copy of the TRB at `idx` (test inspection).
    #[cfg(test)]
    #[must_use]
    pub const fn slot(&self, idx: usize) -> Trb {
        self.trbs[idx]
    }
}

/// Consumer side of the event ring (xHCI §4.9.3).
///
/// The event ring is a single segment (ERSTSZ = 1); the consumer walks it
/// linearly and wraps, toggling the Consumer Cycle State each lap. A TRB
/// belongs to the consumer while its cycle bit matches the CCS.
pub struct ConsumerRing<'a> {
    trbs: &'a mut [Trb],
    base_phys: u64,
    dequeue: usize,
    cycle: bool,
}

impl<'a> ConsumerRing<'a> {
    /// Build an event ring over `trbs` (needs ≥ 1 slot) at DMA address
    /// `base_phys`, with initial consumer cycle state `cycle`.
    ///
    /// # Errors
    ///
    /// [`RingError::TooSmall`] when the slice is empty.
    pub fn new(trbs: &'a mut [Trb], base_phys: u64, cycle: bool) -> Result<Self, RingError> {
        if trbs.is_empty() {
            return Err(RingError::TooSmall);
        }
        for trb in trbs.iter_mut() {
            *trb = Trb::zeroed();
        }
        Ok(Self {
            trbs,
            base_phys,
            dequeue: 0,
            cycle,
        })
    }

    /// Number of event TRB slots.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.trbs.len()
    }

    /// Whether the ring holds no slots (never true after [`Self::new`]).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.trbs.is_empty()
    }

    /// Current consumer cycle state.
    #[must_use]
    pub const fn cycle(&self) -> bool {
        self.cycle
    }

    /// Physical address of the dequeue cursor (for ERDP).
    #[must_use]
    pub const fn dequeue_phys(&self) -> u64 {
        self.base_phys + (self.dequeue as u64) * 16
    }

    /// Non-blocking dequeue: the next event TRB, or `None` when its cycle
    /// bit says the ring holds nothing new.
    #[allow(clippy::missing_const_for_fn)]
    pub fn try_dequeue(&mut self) -> Option<Trb> {
        let trb = self.trbs[self.dequeue];
        if trb.cycle_bit() != self.cycle {
            return None;
        }
        self.dequeue += 1;
        if self.dequeue == self.trbs.len() {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        Some(trb)
    }

    /// Copy of the TRB at `idx` (test inspection).
    #[cfg(test)]
    #[must_use]
    pub const fn slot(&self, idx: usize) -> Trb {
        self.trbs[idx]
    }

    /// Overwrite the TRB at `idx` (the test's fake hardware).
    #[cfg(test)]
    #[allow(clippy::missing_const_for_fn)]
    pub fn poke(&mut self, idx: usize, trb: Trb) {
        self.trbs[idx] = trb;
    }
}

// ---------------------------------------------------------------------------
// 4. MMIO access — volatile on hardware, a byte slice in host tests
// ---------------------------------------------------------------------------

/// Volatile access to an xHCI register window.
///
/// Reads and writes take `&self`: register windows are interior-mutable by
/// nature (volatile), which keeps the driver borrow story simple.
pub trait Mmio {
    /// Read the 32-bit register at byte `offset` from the window base.
    fn read32(&self, offset: u32) -> u32;
    /// Write the 32-bit register at byte `offset`.
    fn write32(&mut self, offset: u32, value: u32);
    /// Read the 64-bit register at byte `offset`.
    fn read64(&self, offset: u32) -> u64 {
        let lo = u64::from(self.read32(offset));
        let hi = u64::from(self.read32(offset + 4));
        lo | (hi << 32)
    }
    /// Write the 64-bit register at byte `offset`.
    fn write64(&mut self, offset: u32, value: u64) {
        self.write32(offset, (value & 0xFFFF_FFFF) as u32);
        self.write32(offset + 4, (value >> 32) as u32);
    }
}

/// Volatile MMIO window at a physical base — the live hardware path.
///
/// Only constructible on the firmware target, where the kernel owns the
/// machine and the window is identity-mapped.
#[cfg(target_os = "uefi")]
#[derive(Debug, Clone, Copy)]
pub struct VolatileMmio {
    base: u64,
}

#[cfg(target_os = "uefi")]
impl VolatileMmio {
    /// A window at physical `base`.
    ///
    /// # Safety
    ///
    /// `base` must be the MMIO base of an xHCI controller whose register
    /// window is mapped and exclusively owned by the caller.
    #[must_use]
    pub unsafe fn at(base: u64) -> Self {
        Self { base }
    }
}

#[cfg(target_os = "uefi")]
impl Mmio for VolatileMmio {
    fn read32(&self, offset: u32) -> u32 {
        // SAFETY: constructed from a mapped xHCI MMIO base; volatile so the
        // compiler never elides or reorders register accesses.
        unsafe { core::ptr::read_volatile((self.base + u64::from(offset)) as *const u32) }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        // SAFETY: as for `read32`.
        unsafe { core::ptr::write_volatile((self.base + u64::from(offset)) as *mut u32, value) }
    }
}

/// A byte-slice MMIO window — the host-test stand-in for hardware.
///
/// Lets the whole driver (probe, reset, ring programming, event handling)
/// run against a `Vec<u8>` with assertions on the programmed registers.
#[cfg(test)]
pub struct SliceMmio<'a> {
    bytes: &'a mut [u8],
}

#[cfg(test)]
impl<'a> SliceMmio<'a> {
    /// A window over `bytes` (little-endian register image).
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes }
    }
}

#[cfg(test)]
impl Mmio for SliceMmio<'_> {
    fn read32(&self, offset: u32) -> u32 {
        let o = offset as usize;
        u32::from_le_bytes(self.bytes[o..o + 4].try_into().unwrap_or([0; 4]))
    }

    fn write32(&mut self, offset: u32, value: u32) {
        let o = offset as usize;
        self.bytes[o..o + 4].copy_from_slice(&value.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// 5. DMA memory — page-aligned, physically contiguous
// ---------------------------------------------------------------------------

/// DMA allocation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaError {
    /// Zero pages requested.
    ZeroPages,
    /// The allocator refused the request.
    AllocFailed,
}

/// A DMA region: 4 KiB-aligned, physically contiguous, zeroed.
///
/// The kernel identity-maps all of RAM (firmware tables before
/// `bring_up_paging`, the kernel's own tables after), so the virtual address
/// doubles as the DMA (physical) address. Regions are freed on drop.
pub struct DmaRegion {
    ptr: *mut u8,
    pages: usize,
}

impl DmaRegion {
    /// Allocate `pages` zeroed 4 KiB pages.
    ///
    /// # Errors
    ///
    /// [`DmaError::ZeroPages`] for a zero request, [`DmaError::AllocFailed`]
    /// when the global allocator refuses.
    pub fn alloc_pages(pages: usize) -> Result<Self, DmaError> {
        if pages == 0 {
            return Err(DmaError::ZeroPages);
        }
        let layout =
            Layout::from_size_align(pages * 4096, 4096).map_err(|_| DmaError::AllocFailed)?;
        // SAFETY: non-zero size, 4 KiB alignment; zeroed on success.
        let ptr = unsafe { alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(DmaError::AllocFailed);
        }
        Ok(Self { ptr, pages })
    }

    /// The DMA (physical) address of the region.
    #[must_use]
    pub fn phys(&self) -> u64 {
        self.ptr as u64
    }

    /// Byte length.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.pages * 4096
    }

    /// Whether the region is empty (never true after [`Self::alloc_pages`]).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.pages == 0
    }

    /// The region as a mutable TRB slice (for rings).
    ///
    /// # Panics
    ///
    /// Panics if the region is not a whole number of TRBs (it always is:
    /// page-sized).
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn as_trb_slice(&mut self) -> &mut [Trb] {
        let n = self.len() / 16;
        // SAFETY: the region is 4 KiB-aligned by construction (`Layout`
        // in `alloc_pages`), so the `*mut u8` -> `*mut Trb` cast upholds
        // `Trb`'s 8-byte alignment; `len` bytes are owned by this region and
        // `Trb` is 16 bytes with no padding or invariants.
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            core::slice::from_raw_parts_mut(self.ptr.cast::<Trb>(), n)
        }
    }

    /// The region as a mutable byte slice.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `len` bytes owned by this region.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len()) }
    }
}

impl Drop for DmaRegion {
    fn drop(&mut self) {
        let layout =
            Layout::from_size_align(self.pages * 4096, 4096).expect("DmaRegion layout valid");
        // SAFETY: the layout matches the allocation in `alloc_pages`.
        unsafe { dealloc(self.ptr, layout) }
    }
}

// SAFETY: the region owns its pages; moving it across threads is safe, and
// the driver only touches it from one core during bring-up.
unsafe impl Send for DmaRegion {}

// ---------------------------------------------------------------------------
// 6. Controller bring-up — probe, reset, run, port scan
// ---------------------------------------------------------------------------

/// Capability snapshot read from a controller's register window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    caplength: u8,
    hciversion: u16,
    hcsparams1: u32,
    hcsparams2: u32,
    hccparams1: u32,
    db_offset: u32,
    rts_offset: u32,
}

impl Caps {
    /// Capability register block length (the operational base offset).
    #[must_use]
    pub const fn caplength(self) -> u8 {
        self.caplength
    }

    /// BCD interface version (e.g. 0x0110 = 1.1).
    #[must_use]
    pub const fn hciversion(self) -> u16 {
        self.hciversion
    }

    /// Maximum device slots.
    #[must_use]
    pub const fn max_slots(self) -> u8 {
        hcsparams1_max_slots(self.hcsparams1)
    }

    /// Maximum interrupters.
    #[must_use]
    pub const fn max_interrupters(self) -> u16 {
        hcsparams1_max_interrupters(self.hcsparams1)
    }

    /// Number of root-hub ports.
    #[must_use]
    pub const fn max_ports(self) -> u8 {
        hcsparams1_max_ports(self.hcsparams1)
    }

    /// Log2 of the maximum ERST entries.
    #[must_use]
    pub const fn erst_max(self) -> u8 {
        hcsparams2_erst_max(self.hcsparams2)
    }

    /// 64-bit addressing support.
    #[must_use]
    pub const fn addressing_64(self) -> bool {
        self.hccparams1 & HCCPARAMS1_AC64 != 0
    }

    /// Device/endpoint context size in bytes (32 or 64).
    #[must_use]
    pub const fn context_bytes(self) -> usize {
        if self.hccparams1 & HCCPARAMS1_CSZ != 0 {
            64
        } else {
            32
        }
    }

    /// Extended capabilities pointer (dwords), 0 when absent.
    #[must_use]
    pub const fn xecp(self) -> u16 {
        hccparams1_xecp(self.hccparams1)
    }

    /// Operational register base (offset from the MMIO base).
    #[must_use]
    pub const fn op_offset(self) -> u32 {
        self.caplength as u32
    }

    /// Doorbell array base (offset from the MMIO base).
    #[must_use]
    pub const fn db_offset(self) -> u32 {
        self.db_offset
    }

    /// Runtime register base (offset from the MMIO base).
    #[must_use]
    pub const fn rts_offset(self) -> u32 {
        self.rts_offset
    }
}

/// Capability-probe failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// CAPLENGTH below the 0x20-byte capability block (not an xHCI window).
    BadCapLength,
    /// Unrecognized interface version.
    BadVersion,
    /// No root-hub ports declared.
    NoPorts,
    /// No device slots declared.
    NoSlots,
}

/// Read and validate a controller's capability registers.
///
/// # Errors
///
/// [`ProbeError`] when the window does not look like an xHCI controller.
pub fn probe_caps(mmio: &impl Mmio) -> Result<Caps, ProbeError> {
    // Dword 0 packs CAPLENGTH (byte 0) and HCIVERSION (bytes 2–3).
    let cap0 = mmio.read32(0);
    let caplength = (cap0 & 0xFF) as u8;
    if caplength < 0x20 {
        return Err(ProbeError::BadCapLength);
    }
    let hciversion = ((cap0 >> 16) & 0xFFFF) as u16;
    match hciversion {
        0x0100 | 0x0110 | 0x0120 => {}
        _ => return Err(ProbeError::BadVersion),
    }
    let hcsparams1 = mmio.read32(CAP_HCSPARAMS1);
    if hcsparams1_max_ports(hcsparams1) == 0 {
        return Err(ProbeError::NoPorts);
    }
    if hcsparams1_max_slots(hcsparams1) == 0 {
        return Err(ProbeError::NoSlots);
    }
    Ok(Caps {
        caplength,
        hciversion,
        hcsparams1,
        hcsparams2: mmio.read32(CAP_HCSPARAMS2),
        hccparams1: mmio.read32(CAP_HCCPARAMS1),
        db_offset: dboff_to_bytes(mmio.read32(CAP_DBOFF)),
        rts_offset: rtsoff_to_bytes(mmio.read32(CAP_RTSOFF)),
    })
}

/// Reset failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetError {
    /// CNR stayed set for the whole poll budget.
    ControllerNotReady,
}

/// Run failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunError {
    /// `HCHalted` stayed set for the whole poll budget.
    StillHalted,
}

/// The controller's operational face: probe, reset, run, port scan.
///
/// This is what the boot kernel drives directly — init plus port routing —
/// without allocating the DMA rings the full [`Controller`] needs.
pub struct Op<M: Mmio> {
    mmio: M,
    caps: Caps,
}

impl<M: Mmio> Op<M> {
    /// Probe the capability registers behind `mmio`.
    ///
    /// # Errors
    ///
    /// [`ProbeError`] when the window is not an xHCI controller.
    pub fn new(mmio: M) -> Result<Self, ProbeError> {
        let caps = probe_caps(&mmio)?;
        Ok(Self { mmio, caps })
    }

    /// The probed capabilities.
    #[must_use]
    pub const fn caps(&self) -> &Caps {
        &self.caps
    }

    /// USBCMD value.
    #[must_use]
    pub fn usbcmd(&self) -> u32 {
        self.mmio.read32(self.caps.op_offset() + OP_USBCMD)
    }

    /// USBSTS value.
    #[must_use]
    pub fn usbsts(&self) -> u32 {
        self.mmio.read32(self.caps.op_offset() + OP_USBSTS)
    }

    /// Whether the controller reports halted.
    #[must_use]
    pub fn halted(&self) -> bool {
        self.usbsts() & USBSTS_HCHALTED != 0
    }

    /// Host-controller reset: set HCRST, then poll for CNR clear.
    ///
    /// Bounded: at most `max_polls` status reads, so a wedged controller is
    /// reported, never a hang.
    ///
    /// # Errors
    ///
    /// [`ResetError::ControllerNotReady`] when CNR never clears.
    pub fn reset(&mut self, max_polls: u32) -> Result<(), ResetError> {
        let op = self.caps.op_offset();
        self.mmio
            .write32(op + OP_USBCMD, self.usbcmd() | USBCMD_HCRST);
        for _ in 0..max_polls {
            if self.usbsts() & USBSTS_CNR == 0 {
                return Ok(());
            }
        }
        Err(ResetError::ControllerNotReady)
    }

    /// Start the schedule: set Run/Stop, then poll for `HCHalted` clear.
    ///
    /// # Errors
    ///
    /// [`RunError::StillHalted`] when the halted bit never clears.
    pub fn run(&mut self, max_polls: u32) -> Result<(), RunError> {
        let op = self.caps.op_offset();
        self.mmio.write32(op + OP_USBCMD, self.usbcmd() | USBCMD_RS);
        for _ in 0..max_polls {
            if !self.halted() {
                return Ok(());
            }
        }
        Err(RunError::StillHalted)
    }

    /// Read and decode one root-hub port (1-based); `None` past the end.
    #[must_use]
    pub fn port(&self, port: u8) -> Option<PortInfo> {
        if port == 0 || port > self.caps.max_ports() {
            return None;
        }
        let portsc = self
            .mmio
            .read32(self.caps.op_offset() + portsc_offset(port));
        Some(decode_portsc(port, portsc))
    }

    /// Acknowledge a port's change bits (write-1-to-clear CSC/PEC/WRC/OCC/
    /// PRC/PLC/CEC) after handling a hot-plug event.
    pub fn ack_port_change(&mut self, port: u8) {
        if port == 0 || port > self.caps.max_ports() {
            return;
        }
        let off = self.caps.op_offset() + portsc_offset(port);
        let portsc = self.mmio.read32(off);
        self.mmio.write32(off, portsc | PORTSC_CHANGE_BITS);
    }

    /// Ring a doorbell: slot 0 targets the host controller, other slots
    /// target an endpoint's transfer ring.
    pub fn doorbell(&mut self, slot: u8, target: u32) {
        self.mmio.write32(
            doorbell_offset(self.caps.db_offset(), slot),
            doorbell_value(target, 0),
        );
    }

    /// Consume into the full driver, keeping the probed window.
    pub const fn into_full(self) -> FullInit<M> {
        FullInit { op: self }
    }
}

/// Intermediate: a probed [`Op`] ready for DMA bring-up.
pub struct FullInit<M: Mmio> {
    op: Op<M>,
}

// ---------------------------------------------------------------------------
// 7. Full driver — command/event rings, slots, transfers, hot-plug
// ---------------------------------------------------------------------------

/// Driver bring-up failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// Capability probe failed.
    Probe(ProbeError),
    /// DMA allocation failed.
    Dma(DmaError),
    /// The controller does not advertise 4 KiB page support.
    No4kPage,
    /// Reset failed.
    Reset(ResetError),
    /// The schedule would not start.
    Run(RunError),
}

/// Command/event wait failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
    /// No matching event within the poll budget.
    Timeout,
    /// The controller reported a failing completion code.
    Failed(u8),
}

/// A decoded event-ring entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A transfer TRB completed.
    Transfer {
        /// Physical address of the completed TRB.
        trb_phys: u64,
        /// Untransferred bytes.
        residue: u32,
        /// Completion code.
        code: u8,
        /// Device slot.
        slot: u8,
        /// Endpoint ID (DCI).
        endpoint: u8,
    },
    /// A command completed.
    CommandCompletion {
        /// Physical address of the completed command TRB.
        command_phys: u64,
        /// Completion code.
        code: u8,
        /// Assigned slot (Enable Slot) or target slot.
        slot: u8,
    },
    /// A port's status changed — re-scan the port (hot-plug).
    PortStatusChange {
        /// 1-based port number.
        port: u8,
    },
    /// An unrecognized event TRB (reserved for future handling).
    Unknown {
        /// Raw TRB type.
        trb_type: u32,
    },
}

/// Decode an event TRB into [`Event`].
#[must_use]
pub const fn decode_event(trb: Trb) -> Event {
    match trb.trb_type() {
        trb_type::TRANSFER_EVENT => Event::Transfer {
            trb_phys: trb.parameter,
            residue: trb.transfer_residue(),
            code: trb.completion_code(),
            slot: trb.slot_id(),
            endpoint: trb.endpoint_id(),
        },
        trb_type::COMMAND_COMPLETION => Event::CommandCompletion {
            command_phys: trb.command_trb_pointer(),
            code: trb.completion_code(),
            slot: trb.slot_id(),
        },
        trb_type::PORT_STATUS_CHANGE => Event::PortStatusChange {
            port: trb.event_port_id(),
        },
        other => Event::Unknown { trb_type: other },
    }
}

/// Number of TRBs in the event ring segment (one page) — the ERST size field.
pub const EVT_RING_TRBS: u32 = 256;

/// The full xHCI host driver.
///
/// Owns the command ring, the event ring (+ its one-entry segment table),
/// and the Device Context Base Address Array. Transfer rings are
/// caller-owned ([`TransferRing`]) — one per endpoint — so the driver never
/// has to guess their lifetime.
pub struct Controller<M: Mmio> {
    op: Op<M>,
    cmd: ProducerRing<'static>,
    evt: ConsumerRing<'static>,
    /// Owns the command-ring pages (kept alive for the rings' lifetime).
    /// Never read: the fields exist so the DMA outlives the rings'
    /// `'static` borrow (drop order follows declaration order).
    #[allow(dead_code)]
    cmd_dma: DmaRegion,
    /// Owns the event-ring pages.
    #[allow(dead_code)]
    evt_dma: DmaRegion,
    /// Owns the ERST (single 16-byte entry).
    #[allow(dead_code)]
    erst_dma: DmaRegion,
    /// Owns the Device Context Base Address Array.
    #[allow(dead_code)]
    dcbaa_dma: DmaRegion,
}

impl<M: Mmio> Controller<M> {
    /// Bring up the full driver on a probed [`Op`]: reset, program the
    /// DMA structures (DCBAA, command ring, event ring + ERST, interrupter),
    /// and start the schedule.
    ///
    /// # Errors
    ///
    /// [`InitError`] when DMA allocation fails, the controller lacks 4 KiB
    /// page support, or reset/run do not complete within their poll budgets.
    pub fn start(full: FullInit<M>, max_polls: u32) -> Result<Self, InitError> {
        let mut op = full.op;
        let caps = *op.caps();

        op.reset(max_polls).map_err(InitError::Reset)?;

        // DMA structures. One page each: command ring, event ring, ERST
        // (a single 16-byte entry), DCBAA (256 8-byte entries).
        let mut cmd_mem = DmaRegion::alloc_pages(1).map_err(InitError::Dma)?;
        let mut evt_mem = DmaRegion::alloc_pages(1).map_err(InitError::Dma)?;
        let mut erst_mem = DmaRegion::alloc_pages(1).map_err(InitError::Dma)?;
        let dcbaa_mem = DmaRegion::alloc_pages(1).map_err(InitError::Dma)?;

        // The controller must support 4 KiB pages (PAGESIZE bit 0); the
        // rings and contexts are all page-sized.
        let pagesize = op.mmio.read32(caps.op_offset() + OP_PAGESIZE);
        if pagesize & 1 == 0 {
            return Err(InitError::No4kPage);
        }

        // Program the operational registers (xHCI §4.2).
        let op_off = caps.op_offset();
        op.mmio.write64(op_off + OP_DCBAAP, dcbaa_mem.phys());
        op.mmio
            .write32(op_off + OP_CONFIG, u32::from(caps.max_slots().min(32)));

        // Command ring: the producer starts with cycle state 1.
        let cmd_phys = cmd_mem.phys();
        let cmd = ProducerRing::new(cmd_mem.as_trb_slice(), cmd_phys, true)
            .map_err(|_| InitError::Dma(DmaError::AllocFailed))?;
        op.mmio.write64(op_off + OP_CRCR, cmd.crcr_value());

        // Event ring: single-segment ERST, consumer cycle state 1.
        let evt_phys = evt_mem.phys();
        let evt = ConsumerRing::new(evt_mem.as_trb_slice(), evt_phys, true)
            .map_err(|_| InitError::Dma(DmaError::AllocFailed))?;
        {
            let erst = erst_mem.as_bytes_mut();
            erst[0..8].copy_from_slice(&evt_phys.to_le_bytes());
            erst[8..12].copy_from_slice(&EVT_RING_TRBS.to_le_bytes());
        }
        let intr = interrupter_base(caps.rts_offset(), 0);
        op.mmio.write32(intr + INTR_ERSTSZ, 1);
        op.mmio.write64(intr + INTR_ERSTBA, erst_mem.phys());
        op.mmio
            .write64(intr + INTR_ERDP, evt.dequeue_phys() | ERDP_EHB);
        // Clear any pending interrupt, then enable the interrupter.
        op.mmio.write32(intr + INTR_IMAN, IMAN_IP | IMAN_IE);
        op.mmio
            .write32(op_off + OP_USBCMD, op.usbcmd() | USBCMD_RS | USBCMD_INTE);

        op.run(max_polls).map_err(InitError::Run)?;

        // The rings borrow DMA memory the controller owns; extending the
        // borrow to 'static is sound because the regions are dropped with
        // the controller itself (drop order: fields before `_cmd_mem`).
        let cmd = unsafe { extend_ring(cmd) };
        let evt = unsafe { extend_ring_consumer(evt) };

        Ok(Self {
            op,
            cmd,
            evt,
            cmd_dma: cmd_mem,
            evt_dma: evt_mem,
            erst_dma: erst_mem,
            dcbaa_dma: dcbaa_mem,
        })
    }

    /// The probed capabilities.
    #[must_use]
    pub const fn caps(&self) -> &Caps {
        self.op.caps()
    }

    /// Read and decode one root-hub port (1-based).
    #[must_use]
    pub fn port(&self, port: u8) -> Option<PortInfo> {
        self.op.port(port)
    }

    /// Acknowledge a port's change bits after handling a hot-plug event.
    pub fn ack_port_change(&mut self, port: u8) {
        self.op.ack_port_change(port);
    }

    /// Submit a command TRB on the command ring and ring doorbell 0.
    ///
    /// Returns the physical address the command was written to, for
    /// matching its Command Completion event.
    pub fn submit_command(&mut self, trb: Trb) -> u64 {
        let addr = self.op_enqueue(trb);
        self.op.doorbell(0, DB_TARGET_HOST);
        addr
    }

    /// Raw enqueue without the doorbell (for batching, then ring manually).
    pub fn op_enqueue(&mut self, trb: Trb) -> u64 {
        self.cmd.enqueue_trb(trb)
    }

    /// Non-blocking event poll: dequeue one event, decode it, and advance
    /// ERDP past it (clearing Event Handler Busy).
    pub fn poll_event(&mut self) -> Option<Event> {
        let trb = self.evt.try_dequeue()?;
        let event = decode_event(trb);
        let caps = *self.op.caps();
        let intr = interrupter_base(caps.rts_offset(), 0);
        self.op.mmio.write64(
            intr + INTR_ERDP,
            (self.evt.dequeue_phys() & ERDP_PTR_MASK) | ERDP_EHB,
        );
        // Clear the interrupt-pending bit (write-1-to-clear).
        self.op.mmio.write32(
            intr + INTR_IMAN,
            self.op.mmio.read32(intr + INTR_IMAN) | IMAN_IP,
        );
        Some(event)
    }

    /// Wait for the Command Completion event matching `command_phys`.
    ///
    /// Other events seen while waiting are decoded and returned to the
    /// caller through `on_event` so hot-plug notifications are not lost.
    ///
    /// # Errors
    ///
    /// [`WaitError::Timeout`] after `max_polls` empty polls,
    /// [`WaitError::Failed`] when the completion code is not success.
    pub fn wait_command_completion(
        &mut self,
        command_phys: u64,
        max_polls: u32,
        mut on_event: impl FnMut(Event),
    ) -> Result<u8, WaitError> {
        for _ in 0..max_polls {
            if let Some(event) = self.poll_event() {
                match event {
                    Event::CommandCompletion {
                        command_phys: p,
                        code,
                        slot,
                    } if p == command_phys => {
                        if completion_ok(code) {
                            return Ok(slot);
                        }
                        return Err(WaitError::Failed(code));
                    }
                    other => on_event(other),
                }
            }
        }
        Err(WaitError::Timeout)
    }

    /// Enable a device slot (xHCI §4.3): returns the assigned slot ID.
    ///
    /// # Errors
    ///
    /// [`WaitError`] when the completion never arrives or fails.
    pub fn enable_slot(&mut self, max_polls: u32) -> Result<u8, WaitError> {
        let addr = self.submit_command(Trb::enable_slot(self.cmd.cycle()));
        self.wait_command_completion(addr, max_polls, |_| {})
    }

    /// Address a device (xHCI §4.3): program its input context.
    ///
    /// # Errors
    ///
    /// [`WaitError`] when the completion never arrives or fails.
    pub fn address_device(
        &mut self,
        slot: u8,
        input_context_phys: u64,
        bsr: bool,
        max_polls: u32,
    ) -> Result<(), WaitError> {
        let addr = self.submit_command(Trb::address_device(
            slot,
            input_context_phys,
            bsr,
            self.cmd.cycle(),
        ));
        self.wait_command_completion(addr, max_polls, |_| {})
            .map(|_| ())
    }
}

/// Extend a producer ring's borrow to the controller's lifetime.
///
/// # Safety
///
/// The caller must ensure the backing DMA region outlives the returned
/// ring — here, the region is owned by the same struct and dropped after
/// the ring (field declaration order).
unsafe fn extend_ring(ring: ProducerRing<'_>) -> ProducerRing<'static> {
    unsafe { core::mem::transmute::<ProducerRing<'_>, ProducerRing<'static>>(ring) }
}

/// Extend a consumer ring's borrow to the controller's lifetime.
///
/// # Safety
///
/// As for [`extend_ring`].
unsafe fn extend_ring_consumer(ring: ConsumerRing<'_>) -> ConsumerRing<'static> {
    unsafe { core::mem::transmute::<ConsumerRing<'_>, ConsumerRing<'static>>(ring) }
}

// ---------------------------------------------------------------------------
// 8. Transfer rings, transfers, enumeration, hot-plug
// ---------------------------------------------------------------------------

/// A caller-owned per-endpoint transfer ring (xHCI §4.9).
///
/// The driver never guesses transfer-ring lifetimes: whoever enables the
/// endpoint owns its ring and hands it to the transfer calls.
pub struct TransferRing<'a> {
    inner: ProducerRing<'a>,
}

impl<'a> TransferRing<'a> {
    /// Build a transfer ring over `trbs` at DMA address `base_phys`.
    ///
    /// # Errors
    ///
    /// [`RingError::TooSmall`] when the slice holds fewer than two TRBs.
    pub fn new(trbs: &'a mut [Trb], base_phys: u64) -> Result<Self, RingError> {
        Ok(Self {
            inner: ProducerRing::new(trbs, base_phys, true)?,
        })
    }

    /// Enqueue a TRB; returns the physical address written.
    pub fn enqueue_trb(&mut self, trb: Trb) -> u64 {
        self.inner.enqueue_trb(trb)
    }

    /// Current producer cycle state.
    #[must_use]
    pub const fn cycle(&self) -> bool {
        self.inner.cycle()
    }

    /// The TR Dequeue Pointer value for the endpoint context: dequeue
    /// address (bits 63:4) with the Dequeue Cycle State in bit 0.
    #[must_use]
    pub const fn dequeue_pointer(&self) -> u64 {
        (self.inner.next_phys() & !0xF) | self.inner.cycle() as u64
    }

    /// Copy of the TRB at `idx` (test inspection).
    #[cfg(test)]
    #[must_use]
    pub const fn slot(&self, idx: usize) -> Trb {
        self.inner.slot(idx)
    }
}

/// Doorbell target for a slot's endpoint from its DCI (DCI 1 = EP0 → 0).
#[must_use]
pub const fn db_target_for_dci(dci: u8) -> u32 {
    dci.saturating_sub(1) as u32
}

impl<M: Mmio> Controller<M> {
    /// Wait for the Transfer event on (`slot`, `endpoint_dci`).
    ///
    /// Synchronous use only: the first matching Transfer event is taken as
    /// this TD's completion. Anything else seen while waiting goes to
    /// `on_event` (hot-plug notifications are never swallowed).
    ///
    /// Returns the residue (untransferred bytes).
    ///
    /// # Errors
    ///
    /// [`WaitError::Timeout`] after `max_polls` empty polls,
    /// [`WaitError::Failed`] on a failing completion code.
    pub fn wait_transfer(
        &mut self,
        slot: u8,
        endpoint_dci: u8,
        max_polls: u32,
        mut on_event: impl FnMut(Event),
    ) -> Result<u32, WaitError> {
        for _ in 0..max_polls {
            if let Some(event) = self.poll_event() {
                match event {
                    Event::Transfer {
                        residue,
                        code,
                        slot: s,
                        endpoint: e,
                        ..
                    } if s == slot && e == endpoint_dci => {
                        if completion_ok(code) {
                            return Ok(residue);
                        }
                        return Err(WaitError::Failed(code));
                    }
                    other => on_event(other),
                }
            }
        }
        Err(WaitError::Timeout)
    }

    /// Run a control transfer on EP0 (DCI 1): Setup → [Data] → Status.
    ///
    /// `data_phys`/`data_len` describe the data stage (`data_len` 0 means
    /// no data stage); `dir_in` selects an IN data stage. Returns the
    /// residue reported by the Transfer event.
    ///
    /// # Errors
    ///
    /// [`WaitError`] on timeout or a failing completion code.
    #[allow(clippy::too_many_arguments)]
    pub fn control_transfer(
        &mut self,
        slot: u8,
        xfer: &mut TransferRing<'_>,
        setup: &[u8; 8],
        data_phys: u64,
        data_len: u32,
        dir_in: bool,
        max_polls: u32,
    ) -> Result<u32, WaitError> {
        let cycle = xfer.cycle();
        let trt = match (data_len, dir_in) {
            (0, _) => trt::NO_DATA,
            (_, true) => trt::IN,
            (_, false) => trt::OUT,
        };
        xfer.enqueue_trb(Trb::setup(setup, trt, cycle));
        if data_len > 0 {
            xfer.enqueue_trb(Trb::data(data_phys, data_len, false, true, cycle));
        }
        // The status stage runs opposite the data stage; a control transfer
        // with no data stage ends with IN status.
        let status_in = data_len == 0 || !dir_in;
        xfer.enqueue_trb(Trb::status_stage(status_in, cycle));
        self.op.doorbell(slot, db_target_for_dci(1));
        self.wait_transfer(slot, 1, max_polls, |_| {})
    }

    /// Run a bulk transfer: one Normal TRB with interrupt-on-completion.
    ///
    /// `endpoint_dci` is the endpoint's Device Context Index (EP0 = 1).
    /// Returns the residue.
    ///
    /// # Errors
    ///
    /// [`WaitError`] on timeout or a failing completion code.
    pub fn bulk_transfer(
        &mut self,
        slot: u8,
        endpoint_dci: u8,
        xfer: &mut TransferRing<'_>,
        data_phys: u64,
        len: u32,
        max_polls: u32,
    ) -> Result<u32, WaitError> {
        let cycle = xfer.cycle();
        xfer.enqueue_trb(Trb::normal(data_phys, len, true, false, cycle));
        self.op.doorbell(slot, db_target_for_dci(endpoint_dci));
        self.wait_transfer(slot, endpoint_dci, max_polls, |_| {})
    }

    /// Handle a Port Status Change event: re-read the port and acknowledge
    /// its change bits (hot-plug, §4.19).
    pub fn handle_port_event(&mut self, port: u8) -> Option<PortHotplug> {
        let info = self.op.port(port)?;
        self.op.ack_port_change(port);
        Some(PortHotplug {
            port,
            connected: info.connected(),
            info,
        })
    }

    /// Copy of the command-ring TRB at `idx` (test inspection).
    #[cfg(test)]
    #[must_use]
    const fn cmd_slot(&self, idx: usize) -> Trb {
        self.cmd.slot(idx)
    }

    /// Overwrite the event-ring TRB at `idx` (the test's fake hardware).
    #[cfg(test)]
    fn evt_poke(&mut self, idx: usize, trb: Trb) {
        self.evt.poke(idx, trb);
    }
}

/// A hot-plug change observed on a root-hub port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortHotplug {
    /// 1-based port number.
    pub port: u8,
    /// Whether a device is now attached.
    pub connected: bool,
    /// The re-read port state.
    pub info: PortInfo,
}

// ---------------------------------------------------------------------------
// 9. USB enumeration helpers — setup packets, descriptors, input contexts
// ---------------------------------------------------------------------------

/// USB standard request codes (USB 2.0 §9.4).
pub mod usb_request {
    /// `GET_STATUS`.
    pub const GET_STATUS: u8 = 0;
    /// `SET_ADDRESS`.
    pub const SET_ADDRESS: u8 = 5;
    /// `GET_DESCRIPTOR`.
    pub const GET_DESCRIPTOR: u8 = 6;
    /// `SET_CONFIGURATION`.
    pub const SET_CONFIGURATION: u8 = 9;
}

/// USB descriptor types (USB 2.0 §9.5).
pub mod usb_desc_type {
    /// Device descriptor.
    pub const DEVICE: u8 = 1;
    /// Configuration descriptor.
    pub const CONFIGURATION: u8 = 2;
    /// String descriptor.
    pub const STRING: u8 = 3;
}

/// bmRequestType: IN, standard, device.
pub const REQTYPE_IN_DEVICE: u8 = 0x80;
/// bmRequestType: OUT, standard, device.
pub const REQTYPE_OUT_DEVICE: u8 = 0x00;

/// An 8-byte USB setup packet (USB 2.0 §9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupPacket(pub [u8; 8]);

impl SetupPacket {
    /// `GET_DESCRIPTOR` for `length` bytes of descriptor (`desc_type`,
    /// `index`), language `lang_id` (strings only).
    #[must_use]
    pub const fn get_descriptor(desc_type: u8, index: u8, lang_id: u16, length: u16) -> Self {
        let value = (desc_type as u16) << 8 | index as u16;
        Self([
            REQTYPE_IN_DEVICE,
            usb_request::GET_DESCRIPTOR,
            (value & 0xFF) as u8,
            (value >> 8) as u8,
            (lang_id & 0xFF) as u8,
            (lang_id >> 8) as u8,
            (length & 0xFF) as u8,
            (length >> 8) as u8,
        ])
    }

    /// `SET_CONFIGURATION` (USB 2.0 §9.4.7).
    #[must_use]
    pub const fn set_configuration(config: u8) -> Self {
        Self([
            REQTYPE_OUT_DEVICE,
            usb_request::SET_CONFIGURATION,
            config,
            0,
            0,
            0,
            0,
            0,
        ])
    }
}

/// A decoded USB device descriptor (USB 2.0 §9.6.1, 18 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceDescriptor {
    /// bcdUSB.
    pub usb_version: u16,
    /// Device class/subclass/protocol.
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    /// bMaxPacketSize0.
    pub max_packet_0: u8,
    /// idVendor.
    pub vendor: u16,
    /// idProduct.
    pub product: u16,
    /// bcdDevice.
    pub device_version: u16,
    /// bNumConfigurations.
    pub num_configurations: u8,
}

/// Decode an 18-byte device descriptor; `None` on wrong length/type.
#[must_use]
pub fn decode_device_descriptor(bytes: &[u8]) -> Option<DeviceDescriptor> {
    if bytes.len() < 18 || bytes[0] != 18 || bytes[1] != usb_desc_type::DEVICE {
        return None;
    }
    let u16le = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    Some(DeviceDescriptor {
        usb_version: u16le(2),
        class: bytes[4],
        subclass: bytes[5],
        protocol: bytes[6],
        max_packet_0: bytes[7],
        vendor: u16le(8),
        product: u16le(10),
        device_version: u16le(12),
        num_configurations: bytes[17],
    })
}

/// Slot context fields for the Address Device input context (xHCI §6.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotContextFields {
    /// Route string (0 for a root-hub port device).
    pub route_string: u32,
    /// PORTSC speed ID.
    pub speed: u8,
    /// Context Entries: highest DCI in use (1 for EP0-only).
    pub context_entries: u8,
    /// 1-based root-hub port number.
    pub root_hub_port: u8,
}

/// EP0 context fields for the Address Device input context (xHCI §6.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ep0ContextFields {
    /// EP0 max packet size (from the device descriptor).
    pub max_packet_size: u16,
    /// Transfer-ring dequeue pointer with the DCS in bit 0.
    pub tr_dequeue: u64,
    /// Average TRB length hint.
    pub average_trb_length: u16,
}

/// Endpoint type: control.
pub const EP_TYPE_CONTROL: u8 = 4;

/// Build an Address Device input context into `words` (dwords).
///
/// Layout: input control context (drop = 0, add = A0|A1), then the slot
/// context and the EP0 context, each `ctx_bytes` (32 or 64) apart. Returns
/// `false` when `words` is too small or `ctx_bytes` is invalid.
pub fn build_input_context(
    words: &mut [u32],
    ctx_bytes: usize,
    slot: &SlotContextFields,
    ep0: &Ep0ContextFields,
) -> bool {
    if ctx_bytes != 32 && ctx_bytes != 64 {
        return false;
    }
    let ctx_words = ctx_bytes / 4;
    if words.len() < 3 * ctx_words {
        return false;
    }
    for w in words.iter_mut() {
        *w = 0;
    }
    // Input control context: drop nothing, add the slot (DCI 0) and EP0
    // (DCI 1) contexts.
    words[0] = 0;
    words[1] = 0x3;

    // Slot context (DCI 0).
    let s = ctx_words;
    words[s] = (slot.route_string & 0x000F_FFFF)
        | (u32::from(slot.speed & 0xF) << 20)
        | (u32::from(slot.context_entries & 0x1F) << 27);
    words[s + 1] = u32::from(slot.root_hub_port) << 16;
    // DW2: TT fields + interrupter target — none for a root-port device.
    words[s + 3] = 0; // USB device address 0: the controller assigns it.

    // Endpoint 0 context (DCI 1): control, error count 3, MPS from the
    // descriptor, dequeue pointer with the DCS in bit 0.
    let e = 2 * ctx_words;
    words[e] = 0; // interval 0
    words[e + 1] =
        (3 << 1) | (u32::from(EP_TYPE_CONTROL) << 3) | (u32::from(ep0.max_packet_size) << 16);
    words[e + 2] = (ep0.tr_dequeue & 0xFFFF_FFFF) as u32;
    words[e + 3] = (ep0.tr_dequeue >> 32) as u32;
    words[e + 4] = u32::from(ep0.average_trb_length);

    true
}

/// Build an Evaluate Context input context: same as
/// [`build_input_context`] but only EP0 is added (A1), so the Address
/// Device slot state is left alone while EP0's max packet size updates.
pub fn build_evaluate_context(
    words: &mut [u32],
    ctx_bytes: usize,
    slot: &SlotContextFields,
    ep0: &Ep0ContextFields,
) -> bool {
    if !build_input_context(words, ctx_bytes, slot, ep0) {
        return false;
    }
    words[1] = 0x2; // add EP0 (DCI 1) only
    true
}

// ---------------------------------------------------------------------------
// 10. Host tests (run on the dev machine; `cargo test -p enlil-boot xhci`)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A canned xHCI register window: 0x3000 bytes covering the capability,
    /// operational (caplength 0x20), runtime (0x1000) and doorbell (0x2000)
    /// regions of a typical controller.
    struct FakeWindow {
        bytes: Vec<u8>,
    }

    impl FakeWindow {
        fn xhci() -> Self {
            let mut w = Self {
                bytes: vec![0u8; 0x3000],
            };
            // Dword 0: CAPLENGTH 0x20, HCIVERSION 0x0110.
            w.write32(0x00, 0x0110_0020);
            // HCSPARAMS1: 8 slots, 1 interrupter, 4 ports.
            w.write32(CAP_HCSPARAMS1, 8 | (1 << 8) | (4 << 24));
            w.write32(CAP_HCSPARAMS2, 0);
            // HCCPARAMS1: 64-bit addressing, 64-byte contexts.
            w.write32(CAP_HCCPARAMS1, HCCPARAMS1_AC64 | HCCPARAMS1_CSZ);
            w.write32(CAP_DBOFF, 0x2000);
            w.write32(CAP_RTSOFF, 0x1000);
            // PAGESIZE: 4 KiB supported.
            w.write32(0x20 + OP_PAGESIZE, 1);
            w
        }

        fn write32(&mut self, offset: u32, value: u32) {
            let o = offset as usize;
            self.bytes[o..o + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    /// Probe an [`Op`] over a fake window (borrows the window).
    fn probe_op(w: &mut FakeWindow) -> Op<SliceMmio<'_>> {
        Op::new(SliceMmio::new(&mut w.bytes)).expect("fake xHCI probes")
    }

    /// A started controller on a fake window (borrows the window).
    fn start_ctl(w: &mut FakeWindow) -> Controller<SliceMmio<'_>> {
        let op = probe_op(w);
        Controller::start(op.into_full(), 64).expect("fake xHCI starts")
    }

    /// Plant an event TRB at event-ring slot `idx`, as the fake hardware
    /// would (cycle bit = current consumer cycle state).
    fn plant_event<M: Mmio>(ctl: &mut Controller<M>, idx: usize, trb: Trb) {
        let cycle = ctl.evt.cycle();
        ctl.evt_poke(idx, trb.with_cycle(cycle));
    }

    // --- Register map ------------------------------------------------------

    #[test]
    fn pci_identity_predicate() {
        assert!(is_xhci_function(0x0C, 0x03, 0x30));
        assert!(!is_xhci_function(0x0C, 0x03, 0x20)); // EHCI
        assert!(!is_xhci_function(0x0C, 0x03, 0x00)); // UHCI
        assert!(!is_xhci_function(0x01, 0x06, 0x01)); // AHCI
    }

    #[test]
    fn hcsparams1_fields() {
        let v: u32 = 31 | (3 << 8) | (4 << 24);
        assert_eq!(hcsparams1_max_slots(v), 31);
        assert_eq!(hcsparams1_max_interrupters(v), 3);
        assert_eq!(hcsparams1_max_ports(v), 4);
    }

    #[test]
    fn offsets_and_masks() {
        assert_eq!(portsc_offset(1), 0x400);
        assert_eq!(portsc_offset(4), 0x430);
        assert_eq!(dboff_to_bytes(0x2000), 0x2000);
        assert_eq!(dboff_to_bytes(0x2003), 0x2000);
        assert_eq!(rtsoff_to_bytes(0x1000), 0x1000);
        assert_eq!(rtsoff_to_bytes(0x101F), 0x1000);
        assert_eq!(interrupter_base(0x1000, 0), 0x1020);
        assert_eq!(interrupter_base(0x1000, 1), 0x1040);
        assert_eq!(doorbell_offset(0x2000, 0), 0x2000);
        assert_eq!(doorbell_offset(0x2000, 3), 0x200C);
        assert_eq!(doorbell_value(0, 0), 0);
        assert_eq!(db_target_for_dci(1), 0); // EP0
        assert_eq!(db_target_for_dci(4), 3);
        assert_eq!(hccparams1_xecp(0x1234_0000), 0x1234);
    }

    #[test]
    fn portsc_decode() {
        // Connected, enabled, powered, U0, SuperSpeed.
        let raw =
            PORTSC_CCS | PORTSC_PED | PORTSC_PP | (u32::from(SPEED_SUPER) << PORTSC_SPEED_SHIFT);
        let info = decode_portsc(2, raw);
        assert_eq!(info.port, 2);
        assert!(info.connected() && info.enabled() && info.powered());
        assert_eq!(info.link_state(), PLS_U0);
        assert_eq!(info.speed(), PortSpeed::Super);
        assert!(!info.change_pending());

        let change = PORTSC_CCS | (1 << 17); // CSC
        let info = decode_portsc(1, change);
        assert!(info.change_pending());

        assert_eq!(PortSpeed::from_id(1), PortSpeed::Full);
        assert_eq!(PortSpeed::from_id(2), PortSpeed::Low);
        assert_eq!(PortSpeed::from_id(3), PortSpeed::High);
        assert_eq!(PortSpeed::from_id(5), PortSpeed::SuperPlus);
        assert_eq!(PortSpeed::from_id(9), PortSpeed::Unknown);
    }

    // --- TRBs ---------------------------------------------------------------

    #[test]
    fn trb_byte_round_trip() {
        let t = Trb {
            parameter: 0x1234_5678_9ABC_DEF0,
            status: 0xA5A5_5A5A,
            control: 0xDEAD_BEEF,
        };
        assert_eq!(Trb::from_bytes(&t.to_bytes()), t);
    }

    #[test]
    fn setup_data_status_td() {
        let setup = [0x80, 6, 0, 1, 0, 0, 0x12, 0];
        let s = Trb::setup(&setup, trt::IN, true);
        assert_eq!(s.trb_type(), trb_type::SETUP_STAGE);
        assert!(s.cycle_bit());
        assert_eq!(s.parameter, u64::from_le_bytes(setup));
        assert_eq!(s.status, 8);
        assert_ne!(s.control & trb_ctl::CHAIN, 0);

        let d = Trb::data(0x1_0000, 18, false, true, true);
        assert_eq!(d.trb_type(), trb_type::DATA_STAGE);
        assert_eq!(d.parameter, 0x1_0000);
        assert_eq!(d.status, 18);
        assert_ne!(d.control & trb_ctl::CHAIN, 0);
        assert_eq!(d.control & trb_ctl::IOC, 0);

        let st = Trb::status_stage(false, true); // OUT status after IN data
        assert_eq!(st.trb_type(), trb_type::STATUS_STAGE);
        assert_ne!(st.control & trb_ctl::IOC, 0);
        assert_eq!(st.control & trb_ctl::STATUS_DIR_IN, 0);
        let st_in = Trb::status_stage(true, false);
        assert!(!st_in.cycle_bit());
        assert_ne!(st_in.control & trb_ctl::STATUS_DIR_IN, 0);
    }

    #[test]
    fn normal_and_link_trbs() {
        let n = Trb::normal(0x2000, 512, true, false, true);
        assert_eq!(n.trb_type(), trb_type::NORMAL);
        assert_eq!(n.status, 512);
        assert_ne!(n.control & trb_ctl::IOC, 0);

        let l = Trb::link(0x3000, true, false);
        assert_eq!(l.trb_type(), trb_type::LINK);
        assert_eq!(l.parameter, 0x3000);
        assert_ne!(l.control & trb_ctl::TOGGLE_CYCLE, 0);
        assert!(!l.cycle_bit());
    }

    #[test]
    fn command_trbs() {
        let e = Trb::enable_slot(true);
        assert_eq!(e.trb_type(), trb_type::ENABLE_SLOT);
        assert!(e.cycle_bit());

        let a = Trb::address_device(3, 0x9_0000, true, false);
        assert_eq!(a.trb_type(), trb_type::ADDRESS_DEVICE);
        assert_eq!(a.slot_id(), 3);
        assert_eq!(a.parameter, 0x9_0000);
        assert_ne!(a.control & trb_ctl::BSR, 0);

        let a2 = Trb::address_device(3, 0x9_0000, false, false);
        assert_eq!(a2.control & trb_ctl::BSR, 0);

        let ev = Trb::evaluate_context(2, 0x8_0000, true);
        assert_eq!(ev.trb_type(), trb_type::EVALUATE_CONTEXT);
        assert_eq!(ev.slot_id(), 2);

        let d = Trb::disable_slot(5, true);
        assert_eq!(d.trb_type(), trb_type::DISABLE_SLOT);
        assert_eq!(d.slot_id(), 5);

        let n = Trb::noop_command(true);
        assert_eq!(n.trb_type(), trb_type::NOOP_COMMAND);
    }

    #[test]
    fn event_trb_decode() {
        let t = Trb::transfer_event(0x4_0000, 0, CC_SUCCESS, 2, 1, true);
        assert_eq!(t.trb_type(), trb_type::TRANSFER_EVENT);
        match decode_event(t) {
            Event::Transfer {
                trb_phys,
                residue,
                code,
                slot,
                endpoint,
            } => {
                assert_eq!(trb_phys, 0x4_0000);
                assert_eq!(residue, 0);
                assert_eq!(code, CC_SUCCESS);
                assert_eq!(slot, 2);
                assert_eq!(endpoint, 1);
            }
            other => panic!("unexpected {other:?}"),
        }

        let c = Trb::command_completion(0x5_0000, CC_SUCCESS, 7, true);
        match decode_event(c) {
            Event::CommandCompletion {
                command_phys,
                code,
                slot,
            } => {
                assert_eq!(command_phys, 0x5_0000);
                assert_eq!(code, CC_SUCCESS);
                assert_eq!(slot, 7);
            }
            other => panic!("unexpected {other:?}"),
        }

        let p = Trb::port_status_change(3, true);
        assert_eq!(decode_event(p), Event::PortStatusChange { port: 3 });

        let u = Trb {
            parameter: 0,
            status: 0,
            control: 63 << trb_ctl::TYPE_SHIFT,
        }
        .with_cycle(true);
        assert_eq!(decode_event(u), Event::Unknown { trb_type: 63 });

        assert!(completion_ok(CC_SUCCESS));
        assert!(completion_ok(CC_SHORT_PACKET));
        assert!(!completion_ok(6)); // stall
    }

    // --- Rings ----------------------------------------------------------------

    #[test]
    fn producer_ring_enqueue_and_wrap() {
        let mut backing = [Trb::zeroed(); 4]; // 3 payload slots + link
        let mut ring = ProducerRing::new(&mut backing, 0x1_0000, true).unwrap();
        assert_eq!(ring.capacity(), 3);

        let a0 = ring.enqueue_raw(0xAA, 1, trb_type::NORMAL << trb_ctl::TYPE_SHIFT);
        let a1 = ring.enqueue_raw(0xBB, 2, trb_type::NORMAL << trb_ctl::TYPE_SHIFT);
        let a2 = ring.enqueue_raw(0xCC, 3, trb_type::NORMAL << trb_ctl::TYPE_SHIFT);
        assert_eq!((a0, a1, a2), (0x1_0000, 0x1_0010, 0x1_0020));
        assert!(ring.slot(0).cycle_bit() && ring.slot(1).cycle_bit() && ring.slot(2).cycle_bit());

        // The link TRB points back at slot 0 with Toggle Cycle.
        assert_eq!(ring.slot(3).trb_type(), trb_type::LINK);
        assert_eq!(ring.slot(3).parameter, 0x1_0000);

        // Wrapping stamps the link with this lap's cycle and toggles.
        let a3 = ring.enqueue_raw(0xDD, 4, trb_type::NORMAL << trb_ctl::TYPE_SHIFT);
        assert_eq!(a3, 0x1_0000);
        assert!(ring.slot(3).cycle_bit()); // retired lap's cycle
        assert!(!ring.cycle());
        assert!(!ring.slot(0).cycle_bit());
        assert_eq!(ring.slot(0).parameter, 0xDD);
    }

    #[test]
    fn producer_ring_rejects_tiny() {
        let mut backing = [Trb::zeroed(); 1];
        assert_eq!(
            ProducerRing::new(&mut backing, 0, true).map(|_| ()),
            Err(RingError::TooSmall)
        );
    }

    #[test]
    fn consumer_ring_dequeue_and_wrap() {
        let mut backing = [Trb::zeroed(); 4];
        let mut ring = ConsumerRing::new(&mut backing, 0x2_0000, true).unwrap();
        assert_eq!(ring.len(), 4);
        assert!(!ring.is_empty());

        // Nothing queued: cycle mismatch.
        assert_eq!(ring.try_dequeue(), None);

        // Queue two events with the consumer's cycle state.
        ring.poke(0, Trb::port_status_change(1, true));
        ring.poke(1, Trb::port_status_change(2, true));
        assert_eq!(ring.try_dequeue().map(Trb::event_port_id), Some(1));
        assert_eq!(ring.try_dequeue().map(Trb::event_port_id), Some(2));
        assert_eq!(ring.try_dequeue(), None);

        // Fill a fresh segment and drain it: the lap toggles CCS.
        let mut backing2 = [Trb::zeroed(); 4];
        let mut ring2 = ConsumerRing::new(&mut backing2, 0x3_0000, true).unwrap();
        for i in 0u8..4 {
            ring2.poke(usize::from(i), Trb::port_status_change(i, true));
        }
        for _ in 0..4 {
            assert!(ring2.try_dequeue().is_some());
        }
        assert!(!ring2.cycle());
        // New-lap events need the toggled cycle bit.
        ring2.poke(0, Trb::port_status_change(9, true));
        assert_eq!(ring2.try_dequeue(), None);
        ring2.poke(0, Trb::port_status_change(9, false));
        assert_eq!(ring2.try_dequeue().map(Trb::event_port_id), Some(9));
    }

    // --- MMIO + DMA ------------------------------------------------------------

    #[test]
    fn slice_mmio_round_trip() {
        let mut bytes = vec![0u8; 64];
        let mut mmio = SliceMmio::new(&mut bytes);
        mmio.write32(0x10, 0xDEAD_BEEF);
        assert_eq!(mmio.read32(0x10), 0xDEAD_BEEF);
        mmio.write64(0x20, 0x1234_5678_9ABC_DEF0);
        assert_eq!(mmio.read64(0x20), 0x1234_5678_9ABC_DEF0);
    }

    #[test]
    fn dma_region_pages() {
        let mut r = DmaRegion::alloc_pages(2).unwrap();
        assert_eq!(r.len(), 8192);
        assert!(!r.is_empty());
        assert_eq!(r.phys() % 4096, 0);
        assert!(r.as_bytes_mut().iter().all(|b| *b == 0));
        assert_eq!(r.as_trb_slice().len(), 8192 / 16);
        assert_eq!(
            DmaRegion::alloc_pages(0).map(|_| ()),
            Err(DmaError::ZeroPages)
        );
    }

    #[test]
    fn probe_decodes_caps() {
        let mut w = FakeWindow::xhci();
        let op = probe_op(&mut w);
        let caps = op.caps();
        assert_eq!(caps.hciversion(), 0x0110);
        assert_eq!(caps.max_slots(), 8);
        assert_eq!(caps.max_ports(), 4);
        assert_eq!(caps.max_interrupters(), 1);
        assert_eq!(caps.op_offset(), 0x20);
        assert_eq!(caps.db_offset(), 0x2000);
        assert_eq!(caps.rts_offset(), 0x1000);
        assert_eq!(caps.context_bytes(), 64);
        assert!(caps.addressing_64());
        assert_eq!(caps.caplength(), 0x20);
    }

    #[test]
    fn probe_rejects_bad_windows() {
        // Bad CAPLENGTH.
        let mut w = FakeWindow::xhci();
        w.write32(0x00, 0x0110_0010);
        assert_eq!(
            probe_caps(&SliceMmio::new(&mut w.bytes)),
            Err(ProbeError::BadCapLength)
        );
        // Bad version.
        let mut w = FakeWindow::xhci();
        w.write32(0x00, 0x0090_0020);
        assert_eq!(
            probe_caps(&SliceMmio::new(&mut w.bytes)),
            Err(ProbeError::BadVersion)
        );
        // No ports.
        let mut w = FakeWindow::xhci();
        w.write32(CAP_HCSPARAMS1, 8 | (1 << 8));
        assert_eq!(
            probe_caps(&SliceMmio::new(&mut w.bytes)),
            Err(ProbeError::NoPorts)
        );
        // No slots.
        let mut w = FakeWindow::xhci();
        w.write32(CAP_HCSPARAMS1, (1 << 8) | (4 << 24));
        assert_eq!(
            probe_caps(&SliceMmio::new(&mut w.bytes)),
            Err(ProbeError::NoSlots)
        );
    }

    #[test]
    fn reset_and_run_sequences() {
        let mut w = FakeWindow::xhci();
        let mut op = probe_op(&mut w);

        // A controller stuck not-ready reports instead of hanging.
        op.mmio
            .write32(op.caps().op_offset() + OP_USBSTS, USBSTS_CNR);
        assert_eq!(op.reset(8), Err(ResetError::ControllerNotReady));

        // Ready: HCRST is set, CNR clear → Ok.
        op.mmio.write32(op.caps().op_offset() + OP_USBSTS, 0);
        assert_eq!(op.reset(8), Ok(()));
        assert_ne!(op.usbcmd() & USBCMD_HCRST, 0);

        // A controller stuck halted reports instead of hanging.
        op.mmio
            .write32(op.caps().op_offset() + OP_USBSTS, USBSTS_HCHALTED);
        assert!(op.halted());
        assert_eq!(op.run(8), Err(RunError::StillHalted));

        op.mmio.write32(op.caps().op_offset() + OP_USBSTS, 0);
        assert_eq!(op.run(8), Ok(()));
        assert_ne!(op.usbcmd() & USBCMD_RS, 0);
    }

    #[test]
    fn port_scan_and_ack() {
        let mut w = FakeWindow::xhci();
        let mut op = probe_op(&mut w);
        let op_off = op.caps().op_offset();

        op.mmio.write32(
            op_off + portsc_offset(1),
            PORTSC_CCS | PORTSC_PED | PORTSC_PP | (u32::from(SPEED_HIGH) << PORTSC_SPEED_SHIFT),
        );
        op.mmio
            .write32(op_off + portsc_offset(2), PORTSC_PP | (1 << 17)); // CSC pending

        let p1 = op.port(1).unwrap();
        assert!(p1.connected() && p1.enabled());
        assert_eq!(p1.speed(), PortSpeed::High);
        assert!(!p1.change_pending());

        let p2 = op.port(2).unwrap();
        assert!(!p2.connected() && p2.change_pending());

        assert_eq!(op.port(0), None);
        assert_eq!(op.port(5), None); // only 4 ports

        op.ack_port_change(2);
        let acked = op.mmio.read32(op_off + portsc_offset(2));
        assert_ne!(acked & PORTSC_CHANGE_BITS, 0); // write-1-to-clear
    }

    // --- Full driver against the fake window ------------------------------------

    #[test]
    fn controller_start_programs_dma() {
        let mut w = FakeWindow::xhci();
        let ctl = start_ctl(&mut w);
        let caps = *ctl.caps();
        let op_off = caps.op_offset();
        let intr = interrupter_base(caps.rts_offset(), 0);

        assert_eq!(ctl.op.mmio.read64(op_off + OP_DCBAAP), ctl.dcbaa_dma.phys());
        let crcr = ctl.op.mmio.read64(op_off + OP_CRCR);
        assert_eq!(crcr & CRCR_PTR_MASK, ctl.cmd_dma.phys());
        assert_ne!(crcr & CRCR_RCS, 0); // producer cycle state 1
        assert_eq!(ctl.op.mmio.read32(op_off + OP_CONFIG) & 0xFF, 8);

        assert_eq!(ctl.op.mmio.read32(intr + INTR_ERSTSZ), 1);
        assert_eq!(
            ctl.op.mmio.read64(intr + INTR_ERSTBA) & !0x3F,
            ctl.erst_dma.phys()
        );
        assert_eq!(
            ctl.op.mmio.read64(intr + INTR_ERDP) & ERDP_PTR_MASK,
            ctl.evt_dma.phys()
        );
        assert_ne!(ctl.op.mmio.read32(intr + INTR_IMAN) & IMAN_IE, 0);
        let usbcmd = ctl.op.mmio.read32(op_off + OP_USBCMD);
        assert_ne!(usbcmd & USBCMD_RS, 0);
        assert_ne!(usbcmd & USBCMD_INTE, 0);
    }

    #[test]
    fn controller_start_rejects_no_4k_page() {
        let mut w = FakeWindow::xhci();
        // PAGESIZE without bit 0: no 4 KiB support.
        w.write32(0x20 + OP_PAGESIZE, 0b10);
        let op = probe_op(&mut w);
        assert_eq!(
            Controller::start(op.into_full(), 64).map(|_| ()),
            Err(InitError::No4kPage)
        );
    }

    #[test]
    fn enable_slot_round_trip() {
        let mut w = FakeWindow::xhci();
        let mut ctl = start_ctl(&mut w);

        // Submit, then have the "hardware" complete it with slot 3.
        let addr = ctl.submit_command(Trb::enable_slot(ctl.cmd.cycle()));
        assert_eq!(addr, ctl.cmd_dma.phys());
        assert_eq!(ctl.cmd_slot(0).trb_type(), trb_type::ENABLE_SLOT);
        // Doorbell 0 was rung for the host controller target.
        assert_eq!(
            ctl.op.mmio.read32(ctl.caps().db_offset()),
            doorbell_value(DB_TARGET_HOST, 0)
        );

        plant_event(
            &mut ctl,
            0,
            Trb::command_completion(addr, CC_SUCCESS, 3, true),
        );
        assert_eq!(ctl.wait_command_completion(addr, 64, |_| {}), Ok(3));
    }

    #[test]
    fn command_wait_timeout_and_failure() {
        let mut w = FakeWindow::xhci();
        let mut ctl = start_ctl(&mut w);
        assert_eq!(
            ctl.wait_command_completion(0x1234, 8, |_| {}),
            Err(WaitError::Timeout)
        );

        let addr = ctl.submit_command(Trb::noop_command(ctl.cmd.cycle()));
        plant_event(&mut ctl, 0, Trb::command_completion(addr, 9, 0, true)); // no slots
        assert_eq!(
            ctl.wait_command_completion(addr, 64, |_| {}),
            Err(WaitError::Failed(9))
        );
    }

    #[test]
    fn command_wait_routes_other_events() {
        let mut w = FakeWindow::xhci();
        let mut ctl = start_ctl(&mut w);
        let addr = ctl.submit_command(Trb::noop_command(ctl.cmd.cycle()));
        // A hot-plug event arrives first, then the completion.
        plant_event(&mut ctl, 0, Trb::port_status_change(2, true));
        plant_event(
            &mut ctl,
            1,
            Trb::command_completion(addr, CC_SUCCESS, 1, true),
        );
        let mut saw_hotplug = false;
        let slot = ctl
            .wait_command_completion(addr, 64, |e| {
                assert_eq!(e, Event::PortStatusChange { port: 2 });
                saw_hotplug = true;
            })
            .expect("completion arrives after the hot-plug event");
        assert!(saw_hotplug);
        assert_eq!(slot, 1);
    }

    #[test]
    fn control_transfer_td_shape() {
        let mut w = FakeWindow::xhci();
        let mut ctl = start_ctl(&mut w);
        let mut xmem = DmaRegion::alloc_pages(1).unwrap();
        let xphys = xmem.phys();
        let mut xfer = TransferRing::new(xmem.as_trb_slice(), xphys).unwrap();

        // The "hardware" will complete the TD with no residue.
        plant_event(
            &mut ctl,
            0,
            Trb::transfer_event(xphys + 32, 0, CC_SUCCESS, 2, 1, true),
        );

        let setup = SetupPacket::get_descriptor(usb_desc_type::DEVICE, 0, 0, 18).0;
        let residue = ctl
            .control_transfer(2, &mut xfer, &setup, 0x6_0000, 18, true, 64)
            .expect("control transfer completes");
        assert_eq!(residue, 0);

        // TD shape: Setup(IN) → Data → Status(OUT).
        assert_eq!(xfer.slot(0).trb_type(), trb_type::SETUP_STAGE);
        assert_eq!(xfer.slot(0).parameter, u64::from_le_bytes(setup));
        assert_eq!(xfer.slot(1).trb_type(), trb_type::DATA_STAGE);
        assert_eq!(xfer.slot(1).parameter, 0x6_0000);
        assert_eq!(xfer.slot(1).status, 18);
        assert_ne!(xfer.slot(1).control & trb_ctl::CHAIN, 0);
        assert_eq!(xfer.slot(2).trb_type(), trb_type::STATUS_STAGE);
        assert_eq!(xfer.slot(2).control & trb_ctl::STATUS_DIR_IN, 0); // OUT status
        assert_ne!(xfer.slot(2).control & trb_ctl::IOC, 0);

        // EP0's doorbell (target 0) was rung for slot 2.
        assert_eq!(
            ctl.op.mmio.read32(ctl.caps().db_offset() + 2 * 4),
            doorbell_value(0, 0)
        );
    }

    #[test]
    fn control_transfer_no_data_stage() {
        let mut w = FakeWindow::xhci();
        let mut ctl = start_ctl(&mut w);
        let mut xmem = DmaRegion::alloc_pages(1).unwrap();
        let xphys = xmem.phys();
        let mut xfer = TransferRing::new(xmem.as_trb_slice(), xphys).unwrap();

        plant_event(
            &mut ctl,
            0,
            Trb::transfer_event(xphys + 16, 0, CC_SUCCESS, 1, 1, true),
        );
        let setup = SetupPacket::set_configuration(1).0;
        ctl.control_transfer(1, &mut xfer, &setup, 0, 0, false, 64)
            .expect("set configuration completes");

        assert_eq!(xfer.slot(0).trb_type(), trb_type::SETUP_STAGE);
        // No data stage: Setup → Status(IN).
        assert_eq!(xfer.slot(1).trb_type(), trb_type::STATUS_STAGE);
        assert_ne!(xfer.slot(1).control & trb_ctl::STATUS_DIR_IN, 0);
    }

    #[test]
    fn bulk_transfer_shape() {
        let mut w = FakeWindow::xhci();
        let mut ctl = start_ctl(&mut w);
        let mut xmem = DmaRegion::alloc_pages(1).unwrap();
        let xphys = xmem.phys();
        let mut xfer = TransferRing::new(xmem.as_trb_slice(), xphys).unwrap();

        plant_event(
            &mut ctl,
            0,
            Trb::transfer_event(xphys, 0, CC_SUCCESS, 2, 4, true),
        );
        let residue = ctl
            .bulk_transfer(2, 4, &mut xfer, 0x7_0000, 512, 64)
            .expect("bulk transfer completes");
        assert_eq!(residue, 0);

        assert_eq!(xfer.slot(0).trb_type(), trb_type::NORMAL);
        assert_eq!(xfer.slot(0).parameter, 0x7_0000);
        assert_eq!(xfer.slot(0).status, 512);
        assert_ne!(xfer.slot(0).control & trb_ctl::IOC, 0);

        // Slot 2's doorbell rung with the DCI-1 target for DCI 4.
        assert_eq!(
            ctl.op.mmio.read32(ctl.caps().db_offset() + 2 * 4),
            doorbell_value(db_target_for_dci(4), 0)
        );
    }

    #[test]
    fn transfer_ring_dequeue_pointer() {
        let mut xmem = DmaRegion::alloc_pages(1).unwrap();
        let xphys = xmem.phys();
        let xfer = TransferRing::new(xmem.as_trb_slice(), xphys).unwrap();
        // DCS (bit 0) = producer cycle state 1; address 16-byte aligned.
        assert_eq!(xfer.dequeue_pointer(), (xphys & !0xF) | 1);
    }

    #[test]
    fn hotplug_event_flow() {
        let mut w = FakeWindow::xhci();
        let mut ctl = start_ctl(&mut w);
        plant_event(&mut ctl, 0, Trb::port_status_change(3, true));

        let event = ctl.poll_event().expect("hot-plug event");
        assert_eq!(event, Event::PortStatusChange { port: 3 });

        // The newly attached device shows CCS; the driver re-reads and acks.
        ctl.op.mmio.write32(
            ctl.caps().op_offset() + portsc_offset(3),
            PORTSC_CCS | PORTSC_PP | (1 << 17), // CSC pending
        );
        let hotplug = ctl.handle_port_event(3).expect("port re-read");
        assert_eq!(hotplug.port, 3);
        assert!(hotplug.connected);
        let acked = ctl
            .op
            .mmio
            .read32(ctl.caps().op_offset() + portsc_offset(3));
        assert_ne!(acked & PORTSC_CHANGE_BITS, 0);

        assert_eq!(ctl.handle_port_event(9), None); // past the end
    }

    // --- USB enumeration helpers -------------------------------------------------

    #[test]
    fn setup_packets() {
        let get = SetupPacket::get_descriptor(usb_desc_type::DEVICE, 0, 0, 18);
        assert_eq!(get.0, [0x80, 6, 0x00, 0x01, 0, 0, 18, 0]);
        let set = SetupPacket::set_configuration(1);
        assert_eq!(set.0, [0x00, 9, 1, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn device_descriptor_decode() {
        let bytes: [u8; 18] = [
            18, 1, 0x00, 0x02, 0, 0, 0, 64, 0x46, 0x1D, 0xC8, 0x0B, 0x00, 0x01, 0, 0, 0, 1,
        ];
        let d = decode_device_descriptor(&bytes).expect("decodes");
        assert_eq!(d.usb_version, 0x0200);
        assert_eq!(d.max_packet_0, 64);
        assert_eq!(d.vendor, 0x1D46);
        assert_eq!(d.product, 0x0BC8);
        assert_eq!(d.num_configurations, 1);

        assert_eq!(decode_device_descriptor(&bytes[..10]), None);
        let mut bad = bytes;
        bad[1] = usb_desc_type::CONFIGURATION;
        assert_eq!(decode_device_descriptor(&bad), None);
        let mut bad_len = bytes;
        bad_len[0] = 9;
        assert_eq!(decode_device_descriptor(&bad_len), None);
    }

    #[test]
    fn input_context_32byte() {
        let slot = SlotContextFields {
            route_string: 0,
            speed: SPEED_HIGH,
            context_entries: 1,
            root_hub_port: 2,
        };
        let ep0 = Ep0ContextFields {
            max_packet_size: 64,
            tr_dequeue: 0xA_0001, // DCS = 1
            average_trb_length: 8,
        };
        let mut words = [0u32; 24];
        assert!(build_input_context(&mut words, 32, &slot, &ep0));

        // Input control context: drop 0, add A0|A1.
        assert_eq!(words[0], 0);
        assert_eq!(words[1], 0x3);

        // Slot context at +0x20: route 0, speed 3, entries 1, port 2.
        assert_eq!(words[8], (3 << 20) | (1 << 27));
        assert_eq!(words[9], 2 << 16);
        assert_eq!(words[11], 0);

        // EP0 context at +0x40: control, MPS 64, dequeue|DCS.
        assert_eq!(words[16], 0);
        assert_eq!(
            words[17],
            (3 << 1) | (u32::from(EP_TYPE_CONTROL) << 3) | (64 << 16)
        );
        assert_eq!(words[18], 0xA_0001);
        assert_eq!(words[19], 0);
        assert_eq!(words[20], 8);
    }

    #[test]
    fn input_context_64byte_and_evaluate() {
        let slot = SlotContextFields {
            route_string: 0,
            speed: SPEED_SUPER,
            context_entries: 1,
            root_hub_port: 1,
        };
        let ep0 = Ep0ContextFields {
            max_packet_size: 512,
            tr_dequeue: 0xB_0000,
            average_trb_length: 8,
        };
        let mut words = [0xFFFF_FFFFu32; 48];
        assert!(build_input_context(&mut words, 64, &slot, &ep0));
        // Slot at +0x40 (word 16), EP0 at +0x80 (word 32).
        assert_eq!(words[16], (4 << 20) | (1 << 27));
        assert_eq!(
            words[33],
            (3 << 1) | (u32::from(EP_TYPE_CONTROL) << 3) | (512 << 16)
        );

        let mut eval = [0u32; 48];
        assert!(build_evaluate_context(&mut eval, 64, &slot, &ep0));
        assert_eq!(eval[1], 0x2); // add EP0 only

        // Rejects bad sizes.
        let mut small = [0u32; 8];
        assert!(!build_input_context(&mut small, 32, &slot, &ep0));
        let mut bad = [0u32; 48];
        assert!(!build_input_context(&mut bad, 48, &slot, &ep0));
    }
}

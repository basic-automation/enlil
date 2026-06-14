//! Transfer-ring TRB codecs and the guest-memory seam for TD processing.
//!
//! A Transfer Descriptor (TD) is one or more transfer TRBs gathered by the
//! control field's **chain bit** — chained TRBs extend the TD, the first
//! chain-clear TRB closes it (ACRN's `USB_DATA_PART`/`USB_DATA_FULL` model).
//! Control transfers arrive as **three separate TDs** (Setup, Data, Status
//! stages — xHCI §4.11.2.2), so the controller keeps per-endpoint control
//! state across TDs rather than expecting one chained mega-TD.
//!
//! Only Data/Normal TRBs dereference guest buffers; the Setup Stage TRB
//! carries its 8-byte packet as **immediate data** in the parameter field
//! (IDT set, xHCI §6.4.1.2.1). Guest-buffer access goes through the
//! [`DmaMemory`] seam: the KVM run loop will pass real guest RAM, tests pass
//! a [`VecDmaMemory`].

use super::ring::GuestRingCursor;
use super::trb::{Trb, TrbType};
use crate::truncate::{u8_of, usize_of};

// ---------------------------------------------------------------------------
// SETUP packet
// ---------------------------------------------------------------------------

/// The 8-byte USB SETUP packet (USB 2.0 §9.3), as carried immediate in a
/// Setup Stage TRB's parameter field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupPacket {
    /// `bmRequestType`: direction (bit 7), type (bits 6:5), recipient (4:0).
    pub request_type: u8,
    /// `bRequest`: the request code (e.g. 6 = `GET_DESCRIPTOR`).
    pub request: u8,
    /// `wValue`: request-specific (e.g. descriptor type/index).
    pub value: u16,
    /// `wIndex`: request-specific (e.g. interface or endpoint number).
    pub index: u16,
    /// `wLength`: number of bytes in the data stage.
    pub length: u16,
}

impl SetupPacket {
    /// Pack into the little-endian u64 the Setup Stage TRB parameter carries
    /// (byte 0 = `bmRequestType`, ..., bytes 6-7 = `wLength`).
    #[must_use]
    pub fn to_le_u64(&self) -> u64 {
        u64::from(self.request_type)
            | u64::from(self.request) << 8
            | u64::from(self.value) << 16
            | u64::from(self.index) << 32
            | u64::from(self.length) << 48
    }

    /// Unpack from the Setup Stage TRB parameter field.
    #[must_use]
    pub const fn from_le_u64(raw: u64) -> Self {
        let b = raw.to_le_bytes();
        Self {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }

    /// Whether the data stage (if any) moves device-to-host (`bmRequestType`
    /// bit 7).
    #[must_use]
    pub const fn is_device_to_host(&self) -> bool {
        self.request_type & 0x80 != 0
    }
}

// ---------------------------------------------------------------------------
// Setup Stage transfer type
// ---------------------------------------------------------------------------

/// Setup Stage TRB Transfer Type field (control bits 17:16, xHCI Table 6-26):
/// tells the controller whether — and in which direction — a Data Stage
/// follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransferType {
    /// No Data Stage follows.
    NoData = 0,
    /// An OUT Data Stage follows.
    OutData = 2,
    /// An IN Data Stage follows.
    InData = 3,
}

impl TransferType {
    /// Decode from the raw 2-bit field (value 1 is reserved → `None`).
    #[must_use]
    pub const fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::NoData),
            2 => Some(Self::OutData),
            3 => Some(Self::InData),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Decoded transfer-ring TRBs
// ---------------------------------------------------------------------------

/// Control-field bit positions shared by the transfer TRB layouts.
const BIT_ISP: u32 = 1 << 2;
const BIT_CHAIN: u32 = 1 << 4;
const BIT_IOC: u32 = 1 << 5;
const BIT_IDT: u32 = 1 << 6;
const BIT_DIR: u32 = 1 << 16;
/// Transfer TRBs carry the requested length in status bits 16:0 (`TRB_LEN`);
/// note Transfer *Event* TRBs use a 24-bit residual field instead.
const TRB_LEN_MASK: u32 = 0x0001_FFFF;

/// A decoded transfer-ring TRB — the typed view [`from_trb`](Self::from_trb)
/// gives TD processing (and [`to_trb`](Self::to_trb) gives tests and the
/// guest-enqueue model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferTrb {
    /// Setup Stage (type 2): the 8-byte SETUP packet, immediate.
    Setup {
        /// The SETUP packet (from the parameter field's immediate data).
        packet: SetupPacket,
        /// Whether/which Data Stage follows.
        transfer_type: TransferType,
        /// Interrupt-on-completion.
        ioc: bool,
    },
    /// Data Stage (type 3): one buffer of the control transfer's data.
    Data {
        /// Guest physical address of the data buffer.
        buffer: u64,
        /// Requested transfer length in bytes.
        length: u32,
        /// Direction: `true` = IN (device-to-host).
        dir_in: bool,
        /// Chain bit — this TRB extends the TD.
        chain: bool,
        /// Interrupt-on-completion.
        ioc: bool,
    },
    /// Status Stage (type 4): the zero-length handshake closing a control
    /// transfer.
    Status {
        /// Direction of the status packet: `true` = IN.
        dir_in: bool,
        /// Interrupt-on-completion.
        ioc: bool,
    },
    /// Normal (type 1): bulk/interrupt data.
    Normal {
        /// Guest physical address of the data buffer.
        buffer: u64,
        /// Requested transfer length in bytes.
        length: u32,
        /// Chain bit — this TRB extends the TD.
        chain: bool,
        /// Interrupt-on-completion.
        ioc: bool,
        /// Interrupt-on-short-packet.
        isp: bool,
    },
    /// No Op (type 8): completes without moving data.
    NoOp {
        /// Interrupt-on-completion.
        ioc: bool,
    },
}

impl TransferTrb {
    /// Decode a raw TRB fetched off a transfer ring. Returns `None` for
    /// types that are not (modelled) transfer TRBs — the caller reports a
    /// TRB error, as real hardware does.
    #[must_use]
    pub fn from_trb(trb: &Trb) -> Option<Self> {
        let c = trb.control;
        match trb.decoded_type() {
            TrbType::SetupStage => {
                // The SETUP packet is always immediate data (IDT, §6.4.1.2.1);
                // a Setup Stage TRB without IDT is malformed.
                if c & BIT_IDT == 0 {
                    return None;
                }
                Some(Self::Setup {
                    packet: SetupPacket::from_le_u64(trb.parameter),
                    transfer_type: TransferType::from_raw(u8_of((c >> 16) & 0x3))?,
                    ioc: c & BIT_IOC != 0,
                })
            }
            TrbType::DataStage => Some(Self::Data {
                buffer: trb.parameter,
                length: trb.status & TRB_LEN_MASK,
                dir_in: c & BIT_DIR != 0,
                chain: c & BIT_CHAIN != 0,
                ioc: c & BIT_IOC != 0,
            }),
            TrbType::StatusStage => Some(Self::Status {
                dir_in: c & BIT_DIR != 0,
                ioc: c & BIT_IOC != 0,
            }),
            TrbType::Normal => Some(Self::Normal {
                buffer: trb.parameter,
                length: trb.status & TRB_LEN_MASK,
                chain: c & BIT_CHAIN != 0,
                ioc: c & BIT_IOC != 0,
                isp: c & BIT_ISP != 0,
            }),
            TrbType::NoOp => Some(Self::NoOp {
                ioc: c & BIT_IOC != 0,
            }),
            _ => None,
        }
    }

    /// Encode into a raw TRB (the inverse of [`from_trb`](Self::from_trb)).
    #[must_use]
    pub fn to_trb(&self, cycle: bool) -> Trb {
        let mut trb = Trb::zeroed();
        trb.set_cycle_bit(cycle);
        match self {
            Self::Setup {
                packet,
                transfer_type,
                ioc,
            } => {
                trb.set_trb_type(TrbType::SetupStage);
                trb.parameter = packet.to_le_u64();
                trb.status = 8; // TRB transfer length is always 8 (§6.4.1.2.1)
                trb.control |= BIT_IDT | (u32::from(*transfer_type as u8) << 16);
                if *ioc {
                    trb.control |= BIT_IOC;
                }
            }
            Self::Data {
                buffer,
                length,
                dir_in,
                chain,
                ioc,
            } => {
                trb.set_trb_type(TrbType::DataStage);
                trb.parameter = *buffer;
                trb.status = length & TRB_LEN_MASK;
                if *dir_in {
                    trb.control |= BIT_DIR;
                }
                if *chain {
                    trb.control |= BIT_CHAIN;
                }
                if *ioc {
                    trb.control |= BIT_IOC;
                }
            }
            Self::Status { dir_in, ioc } => {
                trb.set_trb_type(TrbType::StatusStage);
                if *dir_in {
                    trb.control |= BIT_DIR;
                }
                if *ioc {
                    trb.control |= BIT_IOC;
                }
            }
            Self::Normal {
                buffer,
                length,
                chain,
                ioc,
                isp,
            } => {
                trb.set_trb_type(TrbType::Normal);
                trb.parameter = *buffer;
                trb.status = length & TRB_LEN_MASK;
                if *chain {
                    trb.control |= BIT_CHAIN;
                }
                if *ioc {
                    trb.control |= BIT_IOC;
                }
                if *isp {
                    trb.control |= BIT_ISP;
                }
            }
            Self::NoOp { ioc } => {
                trb.set_trb_type(TrbType::NoOp);
                if *ioc {
                    trb.control |= BIT_IOC;
                }
            }
        }
        trb
    }

    /// Whether this TRB's chain bit extends the TD into the next TRB.
    #[must_use]
    pub const fn chains(&self) -> bool {
        match self {
            Self::Data { chain, .. } | Self::Normal { chain, .. } => *chain,
            Self::Setup { .. } | Self::Status { .. } | Self::NoOp { .. } => false,
        }
    }

    /// Whether this TRB requests an interrupt on completion.
    #[must_use]
    pub const fn interrupt_on_completion(&self) -> bool {
        match self {
            Self::Setup { ioc, .. }
            | Self::Data { ioc, .. }
            | Self::Status { ioc, .. }
            | Self::Normal { ioc, .. }
            | Self::NoOp { ioc } => *ioc,
        }
    }
}

// ---------------------------------------------------------------------------
// Guest-memory seam
// ---------------------------------------------------------------------------

/// Guest physical memory as TD processing sees it: Data/Normal TRB buffers
/// are read and written through this seam. The KVM run loop passes the
/// guest's RAM mapping; tests pass a [`VecDmaMemory`].
pub trait DmaMemory {
    /// Read `buf.len()` bytes at guest physical `addr`. Returns `false` if
    /// the range is not backed by memory (the access is then a transaction
    /// error on the USB side).
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool;

    /// Write `data` at guest physical `addr`. Returns `false` if the range
    /// is not backed by memory.
    fn write(&mut self, addr: u64, data: &[u8]) -> bool;
}

/// A `Vec`-backed [`DmaMemory`] covering `base..base + len` — the test
/// double standing in for guest RAM.
#[derive(Debug)]
pub struct VecDmaMemory {
    base: u64,
    bytes: Vec<u8>,
}

impl VecDmaMemory {
    /// `len` zeroed bytes of "guest RAM" starting at guest physical `base`.
    #[must_use]
    pub fn new(base: u64, len: usize) -> Self {
        Self {
            base,
            bytes: vec![0; len],
        }
    }

    /// The backing bytes (for assertions).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Offset of `addr` within the backing buffer, if `count` bytes from it
    /// fit.
    fn offset_of(&self, addr: u64, count: usize) -> Option<usize> {
        let offset = usize_of(addr.checked_sub(self.base)?);
        (offset.checked_add(count)? <= self.bytes.len()).then_some(offset)
    }
}

impl DmaMemory for VecDmaMemory {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let Some(offset) = self.offset_of(addr, buf.len()) else {
            return false;
        };
        buf.copy_from_slice(&self.bytes[offset..offset + buf.len()]);
        true
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> bool {
        let Some(offset) = self.offset_of(addr, data.len()) else {
            return false;
        };
        self.bytes[offset..offset + data.len()].copy_from_slice(data);
        true
    }
}

// ---------------------------------------------------------------------------
// Guest-resident transfer-ring TD gathering
// ---------------------------------------------------------------------------

/// Gather one Transfer Descriptor from a **guest-resident** transfer ring.
///
/// This is the guest-memory counterpart of the controller's internal
/// `gather_td`: instead of dequeuing from an in-process `TransferRing`, it
/// pulls TRBs from guest memory through a [`GuestRingCursor`] (which follows
/// Link TRBs and honours the Consumer Cycle State — that behaviour is tested
/// with the cursor itself). TRBs chain while the control **chain bit** is set;
/// the first chain-clear TRB closes the TD (ACRN's `USB_DATA_PART`/
/// `USB_DATA_FULL` model). Each entry pairs the guest address the TRB was
/// fetched from (what a Transfer Event reports) with the decoded
/// [`TransferTrb`].
///
/// Returns the gathered TRBs — empty when the ring is exhausted (the next
/// TRB's cycle bit says so), or a short TD if the ring runs out mid-chain — or
/// the guest address of the first undecodable TRB as `Err`, matching the
/// internal path's TRB-error reporting. It is the eventual replacement source
/// for `process_transfer_ring`'s internal ring once the per-endpoint cursor
/// lifecycle is wired in.
///
/// # Errors
/// Returns the guest address of the first TRB that does not decode to a
/// [`TransferTrb`].
pub fn gather_transfer_td(
    cursor: &mut GuestRingCursor,
    mem: &dyn DmaMemory,
) -> Result<Vec<(u64, TransferTrb)>, u64> {
    let mut td = Vec::new();
    while let Some((address, raw)) = cursor.fetch(mem) {
        match TransferTrb::from_trb(&raw) {
            Some(trb) => {
                let chains = trb.chains();
                td.push((address, trb));
                if !chains {
                    break;
                }
            }
            None => return Err(address),
        }
    }
    Ok(td)
}

// ---------------------------------------------------------------------------
// DCI helpers
// ---------------------------------------------------------------------------

/// Endpoints are addressed by Device Context Index (xHCI §4.5.1):
/// `DCI = endpoint_number * 2 + direction` (IN = 1), so EP0 (bidirectional
/// control) is DCI 1 and, for `DCI >= 2`, odd DCIs are IN endpoints.
#[must_use]
pub const fn dci_is_in(dci: u8) -> bool {
    dci % 2 == 1
}

/// The USB endpoint number a DCI addresses (EP0 for DCI 0/1).
#[must_use]
pub const fn dci_endpoint_number(dci: u8) -> u8 {
    dci / 2
}

/// The control endpoint's DCI.
pub const CONTROL_DCI: u8 = 1;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const GET_DESCRIPTOR: SetupPacket = SetupPacket {
        request_type: 0x80,
        request: 6,
        value: 0x0100,
        index: 0,
        length: 18,
    };

    #[test]
    fn setup_packet_round_trips_through_the_immediate_u64() {
        let raw = GET_DESCRIPTOR.to_le_u64();
        // Byte layout per USB §9.3: bmRequestType, bRequest, wValue LE, ...
        assert_eq!(raw.to_le_bytes(), [0x80, 6, 0x00, 0x01, 0, 0, 18, 0]);
        assert_eq!(SetupPacket::from_le_u64(raw), GET_DESCRIPTOR);
        assert!(GET_DESCRIPTOR.is_device_to_host());
    }

    #[test]
    fn setup_stage_trb_round_trips_and_requires_idt() {
        let setup = TransferTrb::Setup {
            packet: GET_DESCRIPTOR,
            transfer_type: TransferType::InData,
            ioc: false,
        };
        let trb = setup.to_trb(true);
        assert_eq!(trb.decoded_type(), TrbType::SetupStage);
        assert_eq!(trb.status, 8, "setup TRB length is always 8");
        assert_eq!(TransferTrb::from_trb(&trb), Some(setup));

        // Without IDT the packet is not immediate — malformed, refuse it.
        let mut no_idt = trb;
        no_idt.control &= !(1 << 6);
        assert_eq!(TransferTrb::from_trb(&no_idt), None);
    }

    #[test]
    fn data_status_normal_round_trip() {
        let cases = [
            TransferTrb::Data {
                buffer: 0x4000,
                length: 18,
                dir_in: true,
                chain: false,
                ioc: true,
            },
            TransferTrb::Status {
                dir_in: false,
                ioc: true,
            },
            TransferTrb::Normal {
                buffer: 0x8000,
                length: 512,
                chain: true,
                ioc: false,
                isp: true,
            },
            TransferTrb::NoOp { ioc: true },
        ];
        for case in cases {
            assert_eq!(TransferTrb::from_trb(&case.to_trb(true)), Some(case));
        }
    }

    #[test]
    fn non_transfer_trbs_do_not_decode() {
        let link = Trb::new(TrbType::Link);
        assert_eq!(TransferTrb::from_trb(&link), None);
        let cmd = Trb::new(TrbType::EnableSlotCommand);
        assert_eq!(TransferTrb::from_trb(&cmd), None);
    }

    #[test]
    fn chain_and_ioc_accessors() {
        let chained = TransferTrb::Normal {
            buffer: 0,
            length: 8,
            chain: true,
            ioc: false,
            isp: false,
        };
        assert!(chained.chains());
        assert!(!chained.interrupt_on_completion());
        let status = TransferTrb::Status {
            dir_in: true,
            ioc: true,
        };
        assert!(!status.chains());
        assert!(status.interrupt_on_completion());
    }

    #[test]
    fn dci_convention() {
        assert!(dci_is_in(CONTROL_DCI));
        assert_eq!(dci_endpoint_number(CONTROL_DCI), 0);
        // EP1 OUT = DCI 2, EP1 IN = DCI 3 (xHCI §4.5.1).
        assert!(!dci_is_in(2));
        assert_eq!(dci_endpoint_number(2), 1);
        assert!(dci_is_in(3));
        assert_eq!(dci_endpoint_number(3), 1);
    }

    #[test]
    fn vec_dma_memory_bounds() {
        let mut mem = VecDmaMemory::new(0x1000, 16);
        assert!(mem.write(0x1000, &[1, 2, 3]));
        assert!(mem.write(0x100D, &[9, 9, 9])); // last 3 bytes
        assert!(!mem.write(0x100E, &[1, 2, 3])); // overruns
        assert!(!mem.write(0xFFF, &[1])); // below base

        let mut buf = [0u8; 3];
        assert!(mem.read(0x1000, &mut buf));
        assert_eq!(buf, [1, 2, 3]);
        assert!(!mem.read(0x100E, &mut buf));
    }

    #[test]
    fn transfer_type_reserved_value_is_rejected() {
        assert_eq!(TransferType::from_raw(1), None);
        assert_eq!(TransferType::from_raw(3), Some(TransferType::InData));
    }

    // -- guest-resident TD gathering ----------------------------------------

    /// Write a transfer TRB into the guest ring at `addr` with cycle `cycle`.
    fn put(mem: &mut VecDmaMemory, addr: u64, t: &TransferTrb, cycle: bool) {
        assert!(mem.write(addr, &t.to_trb(cycle).to_bytes()));
    }

    fn normal(buffer: u64, chain: bool) -> TransferTrb {
        TransferTrb::Normal {
            buffer,
            length: 8,
            chain,
            ioc: !chain,
            isp: false,
        }
    }

    #[test]
    fn gather_transfer_td_gathers_a_chained_td_and_advances() {
        let mut mem = VecDmaMemory::new(0x1000, 0x100);
        // Three chained TRBs; the last clears the chain bit, closing the TD.
        put(&mut mem, 0x1000, &normal(0xAA00, true), true);
        put(&mut mem, 0x1010, &normal(0xBB00, true), true);
        put(&mut mem, 0x1020, &normal(0xCC00, false), true);

        let mut cursor = GuestRingCursor::new(0x1000, true);
        let td = gather_transfer_td(&mut cursor, &mem).unwrap();

        assert_eq!(td.len(), 3);
        assert_eq!(td[0].0, 0x1000);
        assert_eq!(td[1].0, 0x1010);
        assert_eq!(td[2].0, 0x1020);
        assert!(matches!(td[0].1, TransferTrb::Normal { buffer: 0xAA00, .. }));
        assert!(matches!(td[2].1, TransferTrb::Normal { buffer: 0xCC00, chain: false, .. }));
        // The cursor consumed exactly the three TRBs.
        assert_eq!(cursor.dequeue_pointer(), 0x1030);
    }

    #[test]
    fn gather_transfer_td_stops_at_cycle_exhaustion() {
        let mut mem = VecDmaMemory::new(0x1000, 0x100);
        // One self-contained TD, then a slot left at cycle 0 (zeroed memory).
        put(&mut mem, 0x1000, &normal(0xAA00, false), true);

        let mut cursor = GuestRingCursor::new(0x1000, true);
        let td = gather_transfer_td(&mut cursor, &mem).unwrap();
        assert_eq!(td.len(), 1);

        // The next slot's cycle bit (0) mismatches the cursor's CCS (1): the
        // ring is exhausted, so the next gather yields an empty TD.
        let empty = gather_transfer_td(&mut cursor, &mem).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn gather_transfer_td_reports_an_undecodable_trb() {
        let mut mem = VecDmaMemory::new(0x1000, 0x100);
        // A TRB of a type that is neither a transfer TRB nor a Link (type 5),
        // with the matching cycle bit so the cursor hands it back.
        let mut bad = Trb::zeroed();
        bad.control = (5 << 10) | 1;
        assert!(mem.write(0x1000, &bad.to_bytes()));

        let mut cursor = GuestRingCursor::new(0x1000, true);
        assert_eq!(gather_transfer_td(&mut cursor, &mem), Err(0x1000));
    }
}

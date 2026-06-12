//! The USB device-model seam behind the virtual xHCI's transfer rings.
//!
//! TD processing terminates transfers against a [`UsbDeviceModel`]: the
//! controller decodes TRBs, moves bytes through guest memory, and calls one
//! of the three transfer methods. This is the boundary where physical-device
//! forwarding plugs in later (ACRN forwards the same three shapes to libusb);
//! until then the in-process models below make the whole path testable.

use std::collections::VecDeque;
use std::fmt;

use super::xhci::transfer::SetupPacket;

/// Outcome of one USB transfer against a device model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbTransferResult {
    /// The device accepted `n` bytes (OUT / no-data control).
    Ack(usize),
    /// The device produced data (IN / device-to-host control).
    Data(Vec<u8>),
    /// The device stalled the endpoint — the controller halts it and posts a
    /// Stall Error transfer event.
    Stall,
    /// The transaction failed on the bus.
    Error,
}

/// A USB device as the virtual xHCI's transfer path sees it.
///
/// One implementation per kind of backing: the in-process models below for
/// tests and synthetic devices, a libusb-backed forwarder for routed physical
/// devices (Phase 4 follow-up).
pub trait UsbDeviceModel: fmt::Debug {
    /// Execute a control transfer. `data_out` carries the OUT data stage's
    /// bytes (empty for IN and no-data requests); an IN request answers with
    /// [`UsbTransferResult::Data`].
    fn control(&mut self, setup: &SetupPacket, data_out: &[u8]) -> UsbTransferResult;

    /// Bulk/interrupt OUT: the host sends `data` to `endpoint`.
    fn transfer_out(&mut self, endpoint: u8, data: &[u8]) -> UsbTransferResult;

    /// Bulk/interrupt IN: the host asks `endpoint` for up to `max_len` bytes.
    fn transfer_in(&mut self, endpoint: u8, max_len: usize) -> UsbTransferResult;
}

// ---------------------------------------------------------------------------
// Loopback device
// ---------------------------------------------------------------------------

/// Standard request codes the loopback's control endpoint understands.
const REQ_SET_ADDRESS: u8 = 5;
const REQ_GET_DESCRIPTOR: u8 = 6;
const REQ_SET_CONFIGURATION: u8 = 9;

/// Device descriptor type (`wValue` high byte of `GET_DESCRIPTOR`).
const DESCRIPTOR_TYPE_DEVICE: u8 = 1;

/// An in-process loopback device: bulk OUT data is queued per endpoint
/// number and read back by bulk IN on the same endpoint.
///
/// The control endpoint answers the bring-up requests a driver issues
/// (`GET_DESCRIPTOR` for the device descriptor, `SET_ADDRESS`,
/// `SET_CONFIGURATION`) and stalls everything else, as a real device does
/// with an unsupported request.
#[derive(Debug, Default)]
pub struct LoopbackDevice {
    /// Vendor ID reported in the device descriptor.
    vendor_id: u16,
    /// Product ID reported in the device descriptor.
    product_id: u16,
    /// Per-endpoint-number loopback queues (OUT fills, IN drains).
    queues: std::collections::BTreeMap<u8, VecDeque<u8>>,
}

impl LoopbackDevice {
    /// A loopback device reporting `vendor_id:product_id`.
    #[must_use]
    pub const fn new(vendor_id: u16, product_id: u16) -> Self {
        Self {
            vendor_id,
            product_id,
            queues: std::collections::BTreeMap::new(),
        }
    }

    /// The 18-byte USB 2.0 device descriptor (USB §9.6.1).
    const fn device_descriptor(&self) -> [u8; 18] {
        let vid = self.vendor_id.to_le_bytes();
        let pid = self.product_id.to_le_bytes();
        [
            18, // bLength
            1,  // bDescriptorType: DEVICE
            0x00, 0x02, // bcdUSB 2.0
            0xFF, // bDeviceClass: vendor-specific
            0,    // bDeviceSubClass
            0,    // bDeviceProtocol
            64,   // bMaxPacketSize0
            vid[0], vid[1], pid[0], pid[1], 0x00, 0x01, // bcdDevice 1.0
            0,    // iManufacturer
            0,    // iProduct
            0,    // iSerialNumber
            1,    // bNumConfigurations
        ]
    }

    /// Bytes currently queued on an endpoint (for assertions).
    #[must_use]
    pub fn queued(&self, endpoint: u8) -> usize {
        self.queues.get(&endpoint).map_or(0, VecDeque::len)
    }
}

impl UsbDeviceModel for LoopbackDevice {
    fn control(&mut self, setup: &SetupPacket, _data_out: &[u8]) -> UsbTransferResult {
        match (setup.request_type, setup.request) {
            (0x80, REQ_GET_DESCRIPTOR)
                if crate::truncate::u8_of(setup.value >> 8) == DESCRIPTOR_TYPE_DEVICE =>
            {
                let descriptor = self.device_descriptor();
                let len = usize::from(setup.length).min(descriptor.len());
                UsbTransferResult::Data(descriptor[..len].to_vec())
            }
            (0x00, REQ_SET_ADDRESS | REQ_SET_CONFIGURATION) => UsbTransferResult::Ack(0),
            _ => UsbTransferResult::Stall,
        }
    }

    fn transfer_out(&mut self, endpoint: u8, data: &[u8]) -> UsbTransferResult {
        self.queues.entry(endpoint).or_default().extend(data);
        UsbTransferResult::Ack(data.len())
    }

    fn transfer_in(&mut self, endpoint: u8, max_len: usize) -> UsbTransferResult {
        let queue = self.queues.entry(endpoint).or_default();
        let take = max_len.min(queue.len());
        UsbTransferResult::Data(queue.drain(..take).collect())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn get_device_descriptor(length: u16) -> SetupPacket {
        SetupPacket {
            request_type: 0x80,
            request: REQ_GET_DESCRIPTOR,
            value: u16::from(DESCRIPTOR_TYPE_DEVICE) << 8,
            index: 0,
            length,
        }
    }

    #[test]
    fn loopback_echoes_per_endpoint() {
        let mut dev = LoopbackDevice::new(0x1234, 0x5678);
        assert_eq!(dev.transfer_out(1, b"hello"), UsbTransferResult::Ack(5));
        assert_eq!(dev.transfer_out(2, b"other"), UsbTransferResult::Ack(5));

        // IN drains only its own endpoint's queue, bounded by max_len.
        assert_eq!(
            dev.transfer_in(1, 3),
            UsbTransferResult::Data(b"hel".to_vec())
        );
        assert_eq!(
            dev.transfer_in(1, 64),
            UsbTransferResult::Data(b"lo".to_vec())
        );
        assert_eq!(dev.queued(2), 5);
    }

    #[test]
    fn control_answers_device_descriptor_and_honours_wlength() {
        let mut dev = LoopbackDevice::new(0x1234, 0x5678);
        let UsbTransferResult::Data(full) = dev.control(&get_device_descriptor(18), &[]) else {
            panic!("expected descriptor data");
        };
        assert_eq!(full.len(), 18);
        assert_eq!(full[0], 18);
        assert_eq!(&full[8..12], &[0x34, 0x12, 0x78, 0x56], "VID/PID LE");

        // A driver's first read asks for just 8 bytes.
        let UsbTransferResult::Data(head) = dev.control(&get_device_descriptor(8), &[]) else {
            panic!("expected descriptor data");
        };
        assert_eq!(head.len(), 8);
    }

    #[test]
    fn unsupported_control_request_stalls() {
        let mut dev = LoopbackDevice::new(0, 0);
        let vendor_weird = SetupPacket {
            request_type: 0xC0,
            request: 0x42,
            value: 0,
            index: 0,
            length: 4,
        };
        assert_eq!(dev.control(&vendor_weird, &[]), UsbTransferResult::Stall);
    }

    #[test]
    fn set_address_and_configuration_ack() {
        let mut dev = LoopbackDevice::new(0, 0);
        let set_address = SetupPacket {
            request_type: 0,
            request: REQ_SET_ADDRESS,
            value: 7,
            index: 0,
            length: 0,
        };
        assert_eq!(dev.control(&set_address, &[]), UsbTransferResult::Ack(0));
    }
}

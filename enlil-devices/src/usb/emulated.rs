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
// HID boot keyboard
// ---------------------------------------------------------------------------

/// Descriptor types used by the keyboard's `GET_DESCRIPTOR` handling.
const DESCRIPTOR_TYPE_CONFIGURATION: u8 = 2;
const DESCRIPTOR_TYPE_HID_REPORT: u8 = 0x22;

/// HID class requests (HID 1.11 §7.2).
const REQ_HID_SET_REPORT: u8 = 0x09;
const REQ_HID_SET_IDLE: u8 = 0x0A;
const REQ_HID_SET_PROTOCOL: u8 = 0x0B;

/// The boot keyboard's interrupt IN endpoint number (EP1 IN — DCI 3 on the
/// transfer rings).
pub const KEYBOARD_INTERRUPT_ENDPOINT: u8 = 1;

/// The canonical boot-keyboard report descriptor (HID 1.11 Appendix E.6):
/// 8 modifier bits, 1 reserved byte, 5 LED output bits + 3 padding, and a
/// 6-key rollover array. Windows and Linux both class-match this exact
/// shape, so a routed keyboard enumerates with the stock HID driver.
const BOOT_KEYBOARD_REPORT_DESCRIPTOR: [u8; 63] = [
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x05, 0x07, //   Usage Page (Key Codes)
    0x19, 0xE0, //   Usage Minimum (224: LeftControl)
    0x29, 0xE7, //   Usage Maximum (231: Right GUI)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x08, //   Report Count (8)
    0x81, 0x02, //   Input (Data, Variable, Absolute): modifiers
    0x95, 0x01, //   Report Count (1)
    0x75, 0x08, //   Report Size (8)
    0x81, 0x01, //   Input (Constant): reserved byte
    0x95, 0x05, //   Report Count (5)
    0x75, 0x01, //   Report Size (1)
    0x05, 0x08, //   Usage Page (LEDs)
    0x19, 0x01, //   Usage Minimum (1: Num Lock)
    0x29, 0x05, //   Usage Maximum (5: Kana)
    0x91, 0x02, //   Output (Data, Variable, Absolute): LEDs
    0x95, 0x01, //   Report Count (1)
    0x75, 0x03, //   Report Size (3)
    0x91, 0x01, //   Output (Constant): LED padding
    0x95, 0x06, //   Report Count (6)
    0x75, 0x08, //   Report Size (8)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x65, //   Logical Maximum (101)
    0x05, 0x07, //   Usage Page (Key Codes)
    0x19, 0x00, //   Usage Minimum (0)
    0x29, 0x65, //   Usage Maximum (101)
    0x81, 0x00, //   Input (Data, Array): 6-key rollover
    0xC0, // End Collection
];

/// An emulated USB HID **boot keyboard** — the device the Phase 4.5
/// milestone routes to guests.
///
/// Speaks the standard bring-up sequence (device/configuration/report
/// descriptors, `SET_IDLE`/`SET_PROTOCOL`, LED `SET_REPORT`) and delivers
/// 8-byte boot reports on its interrupt IN endpoint as keys are pressed
/// and released.
#[derive(Debug, Default)]
pub struct EmulatedKeyboard {
    /// Vendor ID reported in the device descriptor.
    vendor_id: u16,
    /// Product ID reported in the device descriptor.
    product_id: u16,
    /// Pending 8-byte input reports, oldest first.
    reports: VecDeque<[u8; 8]>,
    /// Modifier byte of the current key state.
    modifiers: u8,
    /// Up to six concurrently held key usages (boot protocol rollover).
    held: Vec<u8>,
    /// LED state the host last wrote via `SET_REPORT` (bit 0 = Num Lock).
    leds: u8,
}

impl EmulatedKeyboard {
    /// A keyboard reporting `vendor_id:product_id`.
    #[must_use]
    pub fn new(vendor_id: u16, product_id: u16) -> Self {
        Self {
            vendor_id,
            product_id,
            ..Self::default()
        }
    }

    /// The 18-byte device descriptor: class 0 at device level (HID lives at
    /// the interface), full speed, one configuration.
    const fn device_descriptor(&self) -> [u8; 18] {
        let vid = self.vendor_id.to_le_bytes();
        let pid = self.product_id.to_le_bytes();
        [
            18, // bLength
            DESCRIPTOR_TYPE_DEVICE,
            0x00,
            0x02, // bcdUSB 2.0
            0,    // bDeviceClass: per interface
            0,    // bDeviceSubClass
            0,    // bDeviceProtocol
            8,    // bMaxPacketSize0 (low/full-speed HID convention)
            vid[0],
            vid[1],
            pid[0],
            pid[1],
            0x00,
            0x01, // bcdDevice 1.0
            0,    // iManufacturer
            0,    // iProduct
            0,    // iSerialNumber
            1,    // bNumConfigurations
        ]
    }

    /// The full configuration block a `GET_DESCRIPTOR(Configuration)`
    /// returns: configuration + interface (HID boot keyboard) + HID class
    /// descriptor + interrupt IN endpoint, `wTotalLength` = 34.
    fn configuration_descriptor() -> [u8; 34] {
        let report_len =
            u16::try_from(BOOT_KEYBOARD_REPORT_DESCRIPTOR.len()).map_or([0; 2], u16::to_le_bytes);
        [
            // Configuration descriptor (USB §9.6.3).
            9,
            DESCRIPTOR_TYPE_CONFIGURATION,
            34,
            0,    // wTotalLength
            1,    // bNumInterfaces
            1,    // bConfigurationValue
            0,    // iConfiguration
            0xA0, // bmAttributes: bus powered, remote wakeup
            50,   // bMaxPower: 100 mA
            // Interface descriptor: HID / boot / keyboard.
            9,
            4, // INTERFACE
            0, // bInterfaceNumber
            0, // bAlternateSetting
            1, // bNumEndpoints
            3, // bInterfaceClass: HID
            1, // bInterfaceSubClass: boot
            1, // bInterfaceProtocol: keyboard
            0, // iInterface
            // HID class descriptor (HID 1.11 §6.2.1).
            9,
            0x21, // HID
            0x11,
            0x01, // bcdHID 1.11
            0,    // bCountryCode
            1,    // bNumDescriptors
            DESCRIPTOR_TYPE_HID_REPORT,
            report_len[0],
            report_len[1],
            // Endpoint descriptor: EP1 IN, interrupt, 8 bytes, 10 ms.
            7,
            5,    // ENDPOINT
            0x81, // EP1 IN
            3,    // interrupt
            8,
            0,  // wMaxPacketSize
            10, // bInterval
        ]
    }

    /// The 8-byte boot input report for the current key state.
    fn current_report(&self) -> [u8; 8] {
        let mut report = [0_u8; 8];
        report[0] = self.modifiers;
        for (slot, usage) in report[2..].iter_mut().zip(self.held.iter()) {
            *slot = *usage;
        }
        report
    }

    /// Queue the current state as an input report (one per state change,
    /// exactly as a real keyboard interrupts).
    fn queue_report(&mut self) {
        self.reports.push_back(self.current_report());
    }

    /// Press a key by HID usage code (e.g. 0x04 = 'A'). Up to six keys
    /// roll over, per the boot protocol.
    pub fn press_key(&mut self, usage: u8) {
        if !self.held.contains(&usage) && self.held.len() < 6 {
            self.held.push(usage);
            self.queue_report();
        }
    }

    /// Release a key by HID usage code.
    pub fn release_key(&mut self, usage: u8) {
        if let Some(index) = self.held.iter().position(|&u| u == usage) {
            self.held.remove(index);
            self.queue_report();
        }
    }

    /// Set the modifier byte (bit 0 = `LeftControl` ... bit 7 = `Right GUI`).
    pub fn set_modifiers(&mut self, modifiers: u8) {
        if self.modifiers != modifiers {
            self.modifiers = modifiers;
            self.queue_report();
        }
    }

    /// The LED state the guest last wrote (bit 0 = Num Lock, 1 = Caps
    /// Lock, 2 = Scroll Lock) — where a physical-keyboard backend would
    /// light the LEDs.
    #[must_use]
    pub const fn leds(&self) -> u8 {
        self.leds
    }

    /// Number of input reports waiting for the interrupt endpoint.
    #[must_use]
    pub fn pending_reports(&self) -> usize {
        self.reports.len()
    }

    fn truncated(bytes: &[u8], requested: u16) -> UsbTransferResult {
        let len = usize::from(requested).min(bytes.len());
        UsbTransferResult::Data(bytes[..len].to_vec())
    }
}

impl UsbDeviceModel for EmulatedKeyboard {
    fn control(&mut self, setup: &SetupPacket, data_out: &[u8]) -> UsbTransferResult {
        let descriptor_type = crate::truncate::u8_of(setup.value >> 8);
        match (setup.request_type, setup.request) {
            // Standard device-level GET_DESCRIPTOR.
            (0x80, REQ_GET_DESCRIPTOR) if descriptor_type == DESCRIPTOR_TYPE_DEVICE => {
                Self::truncated(&self.device_descriptor(), setup.length)
            }
            (0x80, REQ_GET_DESCRIPTOR) if descriptor_type == DESCRIPTOR_TYPE_CONFIGURATION => {
                Self::truncated(&Self::configuration_descriptor(), setup.length)
            }
            // Interface-level GET_DESCRIPTOR for the HID report descriptor.
            (0x81, REQ_GET_DESCRIPTOR) if descriptor_type == DESCRIPTOR_TYPE_HID_REPORT => {
                Self::truncated(&BOOT_KEYBOARD_REPORT_DESCRIPTOR, setup.length)
            }
            // Standard device requests + the HID class requests a keyboard
            // driver issues during bring-up.
            (0x00, REQ_SET_ADDRESS | REQ_SET_CONFIGURATION)
            | (0x21, REQ_HID_SET_IDLE | REQ_HID_SET_PROTOCOL) => UsbTransferResult::Ack(0),
            // SET_REPORT(Output): the LED state.
            (0x21, REQ_HID_SET_REPORT) => {
                if let Some(&leds) = data_out.first() {
                    self.leds = leds;
                }
                UsbTransferResult::Ack(data_out.len())
            }
            _ => UsbTransferResult::Stall,
        }
    }

    fn transfer_out(&mut self, _endpoint: u8, _data: &[u8]) -> UsbTransferResult {
        // The boot keyboard has no OUT endpoint (LEDs go via SET_REPORT).
        UsbTransferResult::Stall
    }

    fn transfer_in(&mut self, endpoint: u8, max_len: usize) -> UsbTransferResult {
        if endpoint != KEYBOARD_INTERRUPT_ENDPOINT {
            return UsbTransferResult::Stall;
        }
        // No pending report reads as a zero-length result (the polled
        // interrupt endpoint's NAK, as this model surfaces it).
        self.reports.pop_front().map_or_else(
            || UsbTransferResult::Data(Vec::new()),
            |report| UsbTransferResult::Data(report[..max_len.min(report.len())].to_vec()),
        )
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

    fn get_descriptor(request_type: u8, descriptor_type: u8, length: u16) -> SetupPacket {
        SetupPacket {
            request_type,
            request: REQ_GET_DESCRIPTOR,
            value: u16::from(descriptor_type) << 8,
            index: 0,
            length,
        }
    }

    #[test]
    fn keyboard_descriptors_are_coherent() {
        let mut kbd = EmulatedKeyboard::new(0x046D, 0xC534);

        // Device descriptor: class 0 (per interface), VID/PID as built.
        let UsbTransferResult::Data(dev) =
            kbd.control(&get_descriptor(0x80, DESCRIPTOR_TYPE_DEVICE, 18), &[])
        else {
            panic!("expected device descriptor");
        };
        assert_eq!(dev.len(), 18);
        assert_eq!(dev[4], 0, "bDeviceClass: per interface");
        assert_eq!(&dev[8..12], &[0x6D, 0x04, 0x34, 0xC5]);

        // Configuration block: wTotalLength == returned length, HID/boot/
        // keyboard interface triple, and the HID descriptor's report length
        // matches the actual report descriptor.
        let UsbTransferResult::Data(config) = kbd.control(
            &get_descriptor(0x80, DESCRIPTOR_TYPE_CONFIGURATION, 255),
            &[],
        ) else {
            panic!("expected configuration descriptor");
        };
        assert_eq!(config.len(), 34);
        assert_eq!(
            u16::from_le_bytes([config[2], config[3]]),
            34,
            "wTotalLength"
        );
        assert_eq!(&config[14..17], &[3, 1, 1], "HID class, boot, keyboard");
        let hid_report_len = u16::from_le_bytes([config[25], config[26]]);

        let UsbTransferResult::Data(report) =
            kbd.control(&get_descriptor(0x81, DESCRIPTOR_TYPE_HID_REPORT, 255), &[])
        else {
            panic!("expected report descriptor");
        };
        assert_eq!(usize::from(hid_report_len), report.len());
        assert_eq!(report.len(), 63);
        assert_eq!(report[0..4], [0x05, 0x01, 0x09, 0x06], "keyboard usage");

        // A driver's staged read honours wLength.
        let UsbTransferResult::Data(head) =
            kbd.control(&get_descriptor(0x80, DESCRIPTOR_TYPE_CONFIGURATION, 9), &[])
        else {
            panic!("expected truncated configuration");
        };
        assert_eq!(head.len(), 9);
    }

    #[test]
    fn keyboard_keys_become_boot_reports_in_order() {
        let mut kbd = EmulatedKeyboard::new(0, 0);
        kbd.set_modifiers(0x02); // LeftShift
        kbd.press_key(0x04); // 'A'
        kbd.release_key(0x04);
        kbd.set_modifiers(0);
        assert_eq!(kbd.pending_reports(), 4);

        let read =
            |kbd: &mut EmulatedKeyboard| match kbd.transfer_in(KEYBOARD_INTERRUPT_ENDPOINT, 8) {
                UsbTransferResult::Data(bytes) => bytes,
                other => panic!("expected report data, got {other:?}"),
            };
        assert_eq!(read(&mut kbd), vec![0x02, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(read(&mut kbd), vec![0x02, 0, 0x04, 0, 0, 0, 0, 0]);
        assert_eq!(read(&mut kbd), vec![0x02, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(read(&mut kbd), vec![0, 0, 0, 0, 0, 0, 0, 0]);
        // Idle endpoint reads as a zero-length result.
        assert_eq!(read(&mut kbd), Vec::<u8>::new());

        // Rollover: a seventh key is dropped, duplicates don't re-report.
        for usage in 4..10 {
            kbd.press_key(usage);
        }
        kbd.press_key(10);
        kbd.press_key(4);
        assert_eq!(kbd.pending_reports(), 6);
    }

    #[test]
    fn keyboard_led_set_report_and_stalls() {
        let mut kbd = EmulatedKeyboard::new(0, 0);
        let set_report = SetupPacket {
            request_type: 0x21,
            request: REQ_HID_SET_REPORT,
            value: 0x0200, // Output report
            index: 0,
            length: 1,
        };
        assert_eq!(
            kbd.control(&set_report, &[0b101]),
            UsbTransferResult::Ack(1)
        );
        assert_eq!(kbd.leds(), 0b101, "Num + Scroll Lock");

        // No OUT endpoint; unknown control requests stall.
        assert_eq!(kbd.transfer_out(1, &[0]), UsbTransferResult::Stall);
        let weird = SetupPacket {
            request_type: 0xC0,
            request: 0x99,
            value: 0,
            index: 0,
            length: 0,
        };
        assert_eq!(kbd.control(&weird, &[]), UsbTransferResult::Stall);
    }

    /// End-to-end: a keyboard routed onto a controller delivers a key
    /// report into guest memory through the interrupt IN transfer ring.
    #[test]
    fn keyboard_report_reaches_guest_memory_through_the_xhci() {
        use crate::usb::CommandTrb;
        use crate::usb::controller::VirtualXhciController;
        use crate::usb::xhci::transfer::TransferTrb;
        use crate::usb::xhci::{EventTrb, TrbCompletionCode, VecDmaMemory};

        let mut c = VirtualXhciController::new(4);
        let op = u32::from(c.caps.caplength);
        c.write_register(op + 0x38, 8); // CONFIG
        c.write_register(op, 1); // run
        c.submit_command(&CommandTrb::EnableSlot);
        c.write_register(c.caps.dboff, 0);
        let _ = c.pop_event();

        let mut kbd = EmulatedKeyboard::new(0x046D, 0xC534);
        kbd.press_key(0x04);
        assert!(c.bind_device_model(1, Box::new(kbd)));

        // Driver posts an 8-byte interrupt IN TRB on EP1 IN (DCI 3).
        let mut mem = VecDmaMemory::new(0x1000, 64);
        assert!(c.submit_transfer(
            1,
            3,
            &TransferTrb::Normal {
                buffer: 0x1000,
                length: 8,
                chain: false,
                ioc: true,
                isp: false,
            },
        ));
        c.write_register(c.caps.dboff + 4, 3);
        c.service_doorbells(&mut mem);

        match c.pop_event() {
            Some(EventTrb::TransferEvent {
                completion_code,
                transfer_length,
                endpoint_id,
                ..
            }) => {
                assert_eq!(completion_code as u8, TrbCompletionCode::Success as u8);
                assert_eq!(transfer_length, 0, "full 8-byte report");
                assert_eq!(endpoint_id, 3);
            }
            other => panic!("expected a transfer event, got {other:?}"),
        }
        assert_eq!(&mem.bytes()[..8], &[0, 0, 0x04, 0, 0, 0, 0, 0]);
    }
}

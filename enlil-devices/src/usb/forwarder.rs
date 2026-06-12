//! libusb-backed physical-device forwarder — the host side of Phase 4.
//!
//! Implements [`UsbDeviceModel`] over a real device opened through libusb
//! (the `rusb` binding), completing the path the routing stack was built
//! for: the virtual xHCI decodes the guest's TRBs and the three transfer
//! shapes land on physical hardware, exactly as ACRN's device model
//! forwards them.
//!
//! Two requests never reach the device raw: `SET_ADDRESS` is acknowledged
//! locally (the host kernel owns real bus addressing), and
//! `SET_CONFIGURATION` is mapped to libusb's configuration call only when
//! the host's active configuration differs. Endpoint transfer kinds are
//! captured from the active configuration descriptor at open time, so
//! bulk and interrupt TDs dispatch to the matching libusb call.
//!
//! Isochronous endpoints are not forwarded yet: libusb's synchronous API
//! has no isochronous shape, so those TDs fail as transaction errors until
//! the async transfer path lands (audio/video class devices — a Phase 4
//! follow-up).

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use rusb::{Context, DeviceHandle, UsbContext};

use super::emulated::{UsbDeviceModel, UsbTransferResult};
use super::types::{UsbError, UsbResult};
use super::xhci::transfer::SetupPacket;
use crate::truncate::u8_of;

/// Per-transfer timeout. Interrupt IN polls on idle HID devices time out
/// routinely; the controller surfaces that as a transaction error and the
/// guest's driver simply polls again.
const TRANSFER_TIMEOUT: Duration = Duration::from_millis(1000);

/// `bRequest` codes the forwarder intercepts (USB 2.0 §9.4).
const REQ_SET_ADDRESS: u8 = 5;
const REQ_SET_CONFIGURATION: u8 = 9;

/// How an endpoint moves data — captured from the active configuration
/// descriptor so TDs dispatch to the matching libusb call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EndpointKind {
    /// Bulk endpoint.
    Bulk,
    /// Interrupt endpoint.
    Interrupt,
    /// Isochronous endpoint (not forwardable over the sync API).
    Isochronous,
}

/// A physical USB device forwarded through libusb.
pub struct LibusbDevice {
    /// The open libusb handle (kernel driver auto-detached, interfaces
    /// claimed).
    handle: DeviceHandle<Context>,
    /// Transfer kind per (endpoint number, is IN) — from the active
    /// configuration descriptor.
    endpoint_kinds: BTreeMap<(u8, bool), EndpointKind>,
}

impl fmt::Debug for LibusbDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LibusbDevice")
            .field("endpoint_kinds", &self.endpoint_kinds)
            .finish_non_exhaustive()
    }
}

impl LibusbDevice {
    /// Open the first device matching `vendor_id:product_id`.
    ///
    /// # Errors
    ///
    /// [`UsbError::DeviceNotFound`] if no matching device is present (or
    /// it cannot be opened — libusb does not distinguish), [`UsbError::Io`]
    /// for libusb context/descriptor/claim failures.
    pub fn open(vendor_id: u16, product_id: u16) -> UsbResult<Self> {
        let context = Context::new().map_err(io_error)?;
        let handle = context
            .open_device_with_vid_pid(vendor_id, product_id)
            .ok_or(UsbError::DeviceNotFound)?;
        Self::from_handle(handle)
    }

    /// Wrap an already-open handle: auto-detach the kernel driver, capture
    /// the endpoint map from the active configuration, and claim its
    /// interfaces.
    ///
    /// # Errors
    ///
    /// [`UsbError::Io`] if the configuration descriptor cannot be read or
    /// an interface cannot be claimed.
    pub fn from_handle(handle: DeviceHandle<Context>) -> UsbResult<Self> {
        // Not supported on every platform; claiming will then fail loudly
        // if a kernel driver really holds the interface.
        let _ = handle.set_auto_detach_kernel_driver(true);
        let config = handle
            .device()
            .active_config_descriptor()
            .map_err(io_error)?;
        let mut endpoint_kinds = BTreeMap::new();
        let mut interfaces = Vec::new();
        for interface in config.interfaces() {
            interfaces.push(interface.number());
            for descriptor in interface.descriptors() {
                for endpoint in descriptor.endpoint_descriptors() {
                    let kind = match endpoint.transfer_type() {
                        rusb::TransferType::Bulk => EndpointKind::Bulk,
                        rusb::TransferType::Interrupt => EndpointKind::Interrupt,
                        rusb::TransferType::Isochronous => EndpointKind::Isochronous,
                        rusb::TransferType::Control => continue,
                    };
                    let is_in = endpoint.direction() == rusb::Direction::In;
                    endpoint_kinds.insert((endpoint.number(), is_in), kind);
                }
            }
        }
        for number in interfaces {
            handle.claim_interface(number).map_err(io_error)?;
        }
        Ok(Self {
            handle,
            endpoint_kinds,
        })
    }

    /// `SET_CONFIGURATION` arrives from a guest re-running enumeration; the
    /// host already configured the device, so only a genuine change touches
    /// libusb (which would otherwise return Busy on claimed interfaces).
    fn set_configuration(&self, value: u8) -> UsbTransferResult {
        match self.handle.active_configuration() {
            Ok(active) if active == value => UsbTransferResult::Ack(0),
            Ok(_) => match self.handle.set_active_configuration(value) {
                Ok(()) => UsbTransferResult::Ack(0),
                Err(err) => transfer_error(err),
            },
            Err(err) => transfer_error(err),
        }
    }
}

/// Map a libusb transfer failure onto the USB-side outcome: a STALL comes
/// back as `LIBUSB_ERROR_PIPE`, everything else (timeout, disconnect, I/O)
/// is a transaction error.
const fn transfer_error(err: rusb::Error) -> UsbTransferResult {
    match err {
        rusb::Error::Pipe => UsbTransferResult::Stall,
        _ => UsbTransferResult::Error,
    }
}

/// Wrap a libusb setup failure as a [`UsbError::Io`].
fn io_error(err: rusb::Error) -> UsbError {
    UsbError::Io(err.to_string())
}

impl UsbDeviceModel for LibusbDevice {
    fn control(&mut self, setup: &SetupPacket, data_out: &[u8]) -> UsbTransferResult {
        // Standard device-level requests the host kernel owns.
        if setup.request_type == 0x00 {
            match setup.request {
                REQ_SET_ADDRESS => return UsbTransferResult::Ack(0),
                REQ_SET_CONFIGURATION => return self.set_configuration(u8_of(setup.value)),
                _ => {}
            }
        }
        if setup.is_device_to_host() {
            let mut buf = vec![0_u8; usize::from(setup.length)];
            match self.handle.read_control(
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                &mut buf,
                TRANSFER_TIMEOUT,
            ) {
                Ok(n) => {
                    buf.truncate(n);
                    UsbTransferResult::Data(buf)
                }
                Err(err) => transfer_error(err),
            }
        } else {
            match self.handle.write_control(
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                data_out,
                TRANSFER_TIMEOUT,
            ) {
                Ok(n) => UsbTransferResult::Ack(n),
                Err(err) => transfer_error(err),
            }
        }
    }

    fn transfer_out(&mut self, endpoint: u8, data: &[u8]) -> UsbTransferResult {
        let address = endpoint & 0x0F;
        let result = match self.endpoint_kinds.get(&(endpoint, false)) {
            Some(EndpointKind::Bulk) => self.handle.write_bulk(address, data, TRANSFER_TIMEOUT),
            Some(EndpointKind::Interrupt) => {
                self.handle.write_interrupt(address, data, TRANSFER_TIMEOUT)
            }
            Some(EndpointKind::Isochronous) => {
                log::warn!("isochronous OUT on EP{endpoint} not forwardable over the sync API");
                return UsbTransferResult::Error;
            }
            // An endpoint the device's configuration does not declare.
            None => return UsbTransferResult::Error,
        };
        match result {
            Ok(n) => UsbTransferResult::Ack(n),
            Err(err) => transfer_error(err),
        }
    }

    fn transfer_in(&mut self, endpoint: u8, max_len: usize) -> UsbTransferResult {
        let address = endpoint | 0x80;
        let mut buf = vec![0_u8; max_len];
        let result = match self.endpoint_kinds.get(&(endpoint, true)) {
            Some(EndpointKind::Bulk) => self.handle.read_bulk(address, &mut buf, TRANSFER_TIMEOUT),
            Some(EndpointKind::Interrupt) => {
                self.handle
                    .read_interrupt(address, &mut buf, TRANSFER_TIMEOUT)
            }
            Some(EndpointKind::Isochronous) => {
                log::warn!("isochronous IN on EP{endpoint} not forwardable over the sync API");
                return UsbTransferResult::Error;
            }
            None => return UsbTransferResult::Error,
        };
        match result {
            Ok(n) => {
                buf.truncate(n);
                UsbTransferResult::Data(buf)
            }
            Err(err) => transfer_error(err),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_maps_to_stall_everything_else_to_transaction_error() {
        assert_eq!(transfer_error(rusb::Error::Pipe), UsbTransferResult::Stall);
        for err in [
            rusb::Error::Timeout,
            rusb::Error::NoDevice,
            rusb::Error::Io,
            rusb::Error::Busy,
        ] {
            assert_eq!(transfer_error(err), UsbTransferResult::Error);
        }
    }

    #[test]
    fn opening_an_absent_device_reports_not_found() {
        // 0xDEAD:0xBEEF is not a real assignment; on a runner with no USB
        // subsystem at all the context itself may fail instead — both are
        // errors, never a handle.
        match LibusbDevice::open(0xDEAD, 0xBEEF) {
            Err(UsbError::DeviceNotFound | UsbError::Io(_)) => {}
            Ok(device) => panic!("opened a device that cannot exist: {device:?}"),
            Err(other) => panic!("unexpected error kind: {other}"),
        }
    }

    /// End-to-end against real hardware: forward `GET_DESCRIPTOR(Device)`
    /// to the first openable device. Self-skips when the runner has no
    /// USB devices or no permission to open them (the usual CI case),
    /// like the KVM smoke test does without `/dev/kvm`.
    #[test]
    fn forwards_get_descriptor_to_a_real_device_when_present() {
        let Ok(context) = Context::new() else {
            eprintln!("skipping: no usable libusb context on this runner");
            return;
        };
        let Ok(devices) = context.devices() else {
            eprintln!("skipping: cannot enumerate USB devices on this runner");
            return;
        };
        for device in devices.iter() {
            let Ok(handle) = device.open() else { continue };
            let Ok(mut forwarder) = LibusbDevice::from_handle(handle) else {
                continue;
            };
            let setup = SetupPacket {
                request_type: 0x80,
                request: 6,    // GET_DESCRIPTOR
                value: 0x0100, // Device descriptor, index 0
                index: 0,
                length: 18,
            };
            match forwarder.control(&setup, &[]) {
                UsbTransferResult::Data(bytes) => {
                    assert_eq!(bytes.len(), 18, "full device descriptor");
                    assert_eq!(bytes[0], 18, "bLength");
                    assert_eq!(bytes[1], 1, "bDescriptorType DEVICE");
                    return;
                }
                other => panic!("GET_DESCRIPTOR against a real device failed: {other:?}"),
            }
        }
        eprintln!("skipping: no openable USB device on this runner (none present or no access)");
    }
}

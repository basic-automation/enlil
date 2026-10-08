//! Asynchronous isochronous transfer path over libusb's raw transfer API.
//!
//! libusb's synchronous API (what `rusb` exposes) has no isochronous
//! shape, so the forwarder routes isochronous TDs here instead: one
//! `libusb_transfer` carrying `N` iso packet descriptors, submitted through
//! `libusb_submit_transfer` and reaped by pumping the event loop on the
//! calling thread until the completion callback fires or the transfer
//! timeout elapses. This is the path audio/video-class devices need —
//! periodic, loss-tolerant frames that bulk and interrupt transfers cannot
//! express.
//!
//! The raw FFI comes from `rusb::ffi` (the `libusb1-sys` re-export), so it
//! drives the same vendored libusb instance the synchronous calls use; no
//! second copy of the library is linked.
//!
//! Isochronous transfers are fire-and-observe: per-packet errors mean
//! dropped frames, not a failed transfer. An OUT reports how many bytes
//! actually went out; an IN concatenates the payloads of the packets that
//! completed. Only a bus-level failure (disconnect, overall timeout) is a
//! transaction error.

use std::ffi::{c_int, c_uint, c_void};
use std::os::raw::c_uchar;
use std::time::{Duration, Instant};

use rusb::ffi;

use super::emulated::UsbTransferResult;

/// Mask for the per-packet byte count in `wMaxPacketSize` (bits 10:0; bits
/// 12:11 encode the high-speed transactions-per-microframe multiplier,
/// which sizes the host-controller schedule, not the packet buffer).
const MAX_PACKET_SIZE_MASK: u16 = 0x07ff;

/// How long one `libusb_handle_events_timeout_completed` call may block:
/// short enough to notice completion and enforce the deadline promptly,
/// long enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Completion flag the transfer callback writes through `user_data`.
extern "system" fn iso_completion_callback(transfer: *mut ffi::libusb_transfer) {
    // The callback runs on the thread pumping `libusb_handle_events` —
    // the same thread that submitted the transfer — so a plain flag
    // through `user_data` needs no synchronization.
    unsafe {
        let completed = (*transfer).user_data.cast::<c_int>();
        if !completed.is_null() {
            *completed = 1;
        }
    }
}

/// RAII wrapper: every allocated transfer is freed exactly once, on every
/// path (submission failure, timeout, or success).
struct IsoTransfer {
    raw: *mut ffi::libusb_transfer,
}

impl IsoTransfer {
    /// Wrap a transfer allocated by `libusb_alloc_transfer`.
    ///
    /// # Safety
    ///
    /// `raw` must be a live transfer no one else will free.
    const unsafe fn wrap(raw: *mut ffi::libusb_transfer) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self { raw })
        }
    }
}

impl Drop for IsoTransfer {
    fn drop(&mut self) {
        // Safe: on every path that reaches here the transfer is no longer
        // in flight — it completed, was cancelled and reaped, or was
        // never submitted.
        unsafe {
            ffi::libusb_free_transfer(self.raw);
        }
    }
}

/// Split `data_len` bytes into isochronous packets of `max_packet_size`
/// bytes: `(packet count, total buffer bytes)`.
///
/// Returns `None` when the endpoint reports a zero packet size (a broken
/// descriptor — dividing by it would be nonsense) or there is nothing to
/// move (`data_len == 0` is handled by the callers before submitting).
fn iso_packet_layout(data_len: usize, max_packet_size: u16) -> Option<(usize, usize)> {
    let packet = usize::from(max_packet_size & MAX_PACKET_SIZE_MASK);
    if packet == 0 || data_len == 0 {
        return None;
    }
    let packets = data_len.div_ceil(packet);
    Some((packets, packets * packet))
}

/// Saturating `Duration` → libusb millisecond timeout.
fn timeout_ms(timeout: Duration) -> c_uint {
    c_uint::try_from(timeout.as_millis()).unwrap_or(c_uint::MAX)
}

/// One poll slice as a `timeval`.
fn poll_timeval() -> libc::timeval {
    libc::timeval {
        tv_sec: 0,
        tv_usec: i64::try_from(POLL_INTERVAL.as_micros()).unwrap_or(i64::MAX),
    }
}

/// Pump the libusb event loop until the transfer's callback fires or
/// `timeout` elapses. Returns `true` when the transfer completed.
///
/// # Safety
///
/// `ctx` must be a live libusb context; `transfer` a submitted transfer
/// whose `user_data` points at `completed`, which stays alive for the call.
unsafe fn pump_until_complete(
    ctx: *mut ffi::libusb_context,
    transfer: *mut ffi::libusb_transfer,
    completed: &mut c_int,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let tv = poll_timeval();
        // SAFETY: `ctx` and `completed` are live per the caller's contract;
        // the call only blocks up to `POLL_INTERVAL`.
        let rc = unsafe {
            ffi::libusb_handle_events_timeout_completed(ctx, &raw const tv, &raw mut *completed)
        };
        // A nonzero return means the event machinery itself broke; the
        // transfer cannot complete after that.
        if rc != 0 {
            return false;
        }
        if *completed != 0 {
            return true;
        }
        if Instant::now() >= deadline {
            // Ask libusb to abort promptly, then reap the completion so
            // the transfer is no longer in flight before it is freed.
            unsafe {
                ffi::libusb_cancel_transfer(transfer);
            }
            let reap_deadline = Instant::now() + POLL_INTERVAL * 10;
            while *completed == 0 && Instant::now() < reap_deadline {
                let tv = poll_timeval();
                let rc = unsafe {
                    ffi::libusb_handle_events_timeout_completed(
                        ctx,
                        &raw const tv,
                        &raw mut *completed,
                    )
                };
                if rc != 0 {
                    break;
                }
            }
            return false;
        }
    }
}

/// Fill and submit one isochronous transfer over `buffer`, then pump until
/// it completes. `packet_len` is applied to every packet except the last,
/// which uses `last_packet_len` — for OUT that is the short final frame so
/// no padding bytes go on the wire; for IN both are the full packet size.
///
/// The completion flag lives in this frame so it outlives the whole
/// submit → pump sequence.
///
/// # Safety
///
/// `dev` must be an open device handle and `ctx` its live context;
/// `buffer` must stay alive until the transfer completes.
#[allow(clippy::too_many_arguments)]
unsafe fn run_iso_transfer(
    dev: *mut ffi::libusb_device_handle,
    ctx: *mut ffi::libusb_context,
    endpoint: u8,
    buffer: *mut c_uchar,
    total_len: usize,
    num_packets: usize,
    packet_len: u16,
    last_packet_len: u16,
    timeout: Duration,
) -> Result<IsoTransfer, UsbTransferResult> {
    let num_packets_cint = c_int::try_from(num_packets).unwrap_or(c_int::MAX);
    let total_len_cint = c_int::try_from(total_len).unwrap_or(c_int::MAX);
    // SAFETY: freshly allocated by libusb; ownership moves into the
    // `IsoTransfer`, which frees it exactly once.
    let Some(transfer) =
        (unsafe { IsoTransfer::wrap(ffi::libusb_alloc_transfer(num_packets_cint)) })
    else {
        return Err(UsbTransferResult::Error);
    };
    let mut completed: c_int = 0;
    // SAFETY: `transfer.raw` is live; `buffer` outlives the pump below;
    // `completed` is a stack slot in this frame, alive until the pump
    // returns, which is when the transfer stops being in flight.
    unsafe {
        ffi::libusb_fill_iso_transfer(
            transfer.raw,
            dev,
            endpoint,
            buffer,
            total_len_cint,
            num_packets_cint,
            iso_completion_callback,
            (&raw mut completed).cast::<c_void>(),
            timeout_ms(timeout),
        );
        ffi::libusb_set_iso_packet_lengths(transfer.raw, c_uint::from(packet_len));
        if last_packet_len != packet_len {
            let last = c_int::try_from(num_packets).unwrap_or(c_int::MAX) - 1;
            if last >= 0 {
                let desc = (*transfer.raw)
                    .iso_packet_desc
                    .as_mut_ptr()
                    .offset(isize::try_from(last).unwrap_or(0));
                (*desc).length = c_uint::from(last_packet_len);
            }
        }
        if ffi::libusb_submit_transfer(transfer.raw) != 0 {
            return Err(UsbTransferResult::Error);
        }
    }
    // SAFETY: `transfer` is submitted, `completed` is alive in this frame,
    // and `ctx` is live per the caller's contract.
    if !unsafe { pump_until_complete(ctx, transfer.raw, &mut completed, timeout) } {
        return Err(UsbTransferResult::Error);
    }
    // SAFETY: the transfer completed; its status and descriptors are
    // readable until it is freed.
    let status = unsafe { (*transfer.raw).status };
    if status != ffi::constants::LIBUSB_TRANSFER_COMPLETED {
        return Err(UsbTransferResult::Error);
    }
    Ok(transfer)
}

/// Run one isochronous OUT transfer: `data` to `endpoint` (full address,
/// direction bit clear), chunked into `max_packet_size`-byte packets.
///
/// # Safety
///
/// `dev` must be an open device handle and `ctx` its live context.
pub(crate) unsafe fn iso_transfer_out(
    dev: *mut ffi::libusb_device_handle,
    ctx: *mut ffi::libusb_context,
    endpoint: u8,
    data: &[u8],
    max_packet_size: u16,
    timeout: Duration,
) -> UsbTransferResult {
    if data.is_empty() {
        // Nothing to send: acknowledge without touching the bus.
        return UsbTransferResult::Ack(0);
    }
    let packet_size = max_packet_size & MAX_PACKET_SIZE_MASK;
    let Some((num_packets, total_len)) = iso_packet_layout(data.len(), max_packet_size) else {
        // Broken endpoint descriptor — fail before any libusb call.
        return UsbTransferResult::Error;
    };
    let mut buf = vec![0_u8; total_len];
    buf[..data.len()].copy_from_slice(data);
    // The last packet carries the remainder — narrowed before submit so
    // no padding bytes go on the wire.
    let packet = usize::from(packet_size);
    let last_packet_len = u16::try_from(data.len() - (num_packets - 1) * packet)
        .unwrap_or(u16::MAX)
        .min(packet_size);

    // SAFETY: `dev`/`ctx` are live per the caller's contract; `buf`
    // outlives the transfer, which completes before this returns.
    let transfer = match unsafe {
        run_iso_transfer(
            dev,
            ctx,
            endpoint,
            buf.as_mut_ptr().cast::<c_uchar>(),
            total_len,
            num_packets,
            packet_size,
            last_packet_len,
            timeout,
        )
    } {
        Ok(transfer) => transfer,
        Err(result) => return result,
    };
    // Isochronous is loss-tolerant: acknowledge what actually went out.
    let mut sent = 0_usize;
    for i in 0..num_packets {
        // SAFETY: completed transfer; descriptor `i` is in bounds.
        let desc = unsafe { &*(*transfer.raw).iso_packet_desc.as_ptr().add(i) };
        if desc.status == ffi::constants::LIBUSB_TRANSFER_COMPLETED {
            sent += usize::try_from(desc.actual_length).unwrap_or(0);
        }
    }
    UsbTransferResult::Ack(sent)
}

/// Run one isochronous IN transfer: up to `max_len` bytes from `endpoint`
/// (full address, direction bit set), in `max_packet_size`-byte packets.
///
/// # Safety
///
/// `dev` must be an open device handle and `ctx` its live context.
pub(crate) unsafe fn iso_transfer_in(
    dev: *mut ffi::libusb_device_handle,
    ctx: *mut ffi::libusb_context,
    endpoint: u8,
    max_len: usize,
    max_packet_size: u16,
    timeout: Duration,
) -> UsbTransferResult {
    let packet_size = max_packet_size & MAX_PACKET_SIZE_MASK;
    let Some((num_packets, total_len)) = iso_packet_layout(max_len.max(1), max_packet_size) else {
        return UsbTransferResult::Error;
    };
    let mut buf = vec![0_u8; total_len];

    // SAFETY: `dev`/`ctx` are live per the caller's contract; `buf`
    // outlives the transfer, which completes before this returns.
    let transfer = match unsafe {
        run_iso_transfer(
            dev,
            ctx,
            endpoint,
            buf.as_mut_ptr().cast::<c_uchar>(),
            total_len,
            num_packets,
            packet_size,
            packet_size,
            timeout,
        )
    } {
        Ok(transfer) => transfer,
        Err(result) => return result,
    };
    // Concatenate the payloads of the packets that arrived; dropped
    // frames simply contribute nothing.
    let mut out = Vec::with_capacity(total_len.min(max_len));
    for i in 0..num_packets {
        // SAFETY: completed transfer; descriptor `i` is in bounds and its
        // packet buffer is readable for `actual_length` bytes.
        let (status, actual_length, packet_buf) = unsafe {
            let desc = &*(*transfer.raw).iso_packet_desc.as_ptr().add(i);
            let packet_buf = ffi::libusb_get_iso_packet_buffer(
                transfer.raw,
                c_uint::try_from(i).unwrap_or(c_uint::MAX),
            );
            (desc.status, desc.actual_length, packet_buf)
        };
        if status != ffi::constants::LIBUSB_TRANSFER_COMPLETED || actual_length == 0 {
            continue;
        }
        if packet_buf.is_null() {
            continue;
        }
        let len = (actual_length as usize).min(max_len.saturating_sub(out.len()));
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(packet_buf.cast::<u8>(), len) });
        if out.len() >= max_len {
            break;
        }
    }
    UsbTransferResult::Data(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_layout_chunks_by_max_packet_size() {
        // Exact multiples need no padding packet.
        assert_eq!(iso_packet_layout(1024, 1024), Some((1, 1024)));
        assert_eq!(iso_packet_layout(2048, 1024), Some((2, 2048)));
        // Remainders round up to one more packet.
        assert_eq!(iso_packet_layout(1025, 1024), Some((2, 2048)));
        assert_eq!(iso_packet_layout(1, 1024), Some((1, 1024)));
        // High-speed multiplier bits (12:11) are not part of the size.
        assert_eq!(iso_packet_layout(1024, 0x0800 | 1024), Some((1, 1024)));
        assert_eq!(iso_packet_layout(2048, 0x1000 | 512), Some((4, 2048)));
    }

    #[test]
    fn packet_layout_rejects_degenerate_inputs() {
        // Zero packet size: broken descriptor, never divide by it.
        assert_eq!(iso_packet_layout(1024, 0), None);
        assert_eq!(iso_packet_layout(1024, 0x1800), None);
        // Nothing to move: the callers short-circuit before submitting.
        assert_eq!(iso_packet_layout(0, 1024), None);
    }

    #[test]
    fn empty_out_is_an_ack_without_bus_traffic() {
        // No device involved: an empty OUT must not touch libusb at all.
        let result = unsafe {
            iso_transfer_out(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0x01,
                &[],
                1024,
                Duration::from_millis(10),
            )
        };
        assert_eq!(result, UsbTransferResult::Ack(0));
    }

    #[test]
    fn broken_descriptor_out_is_an_error_without_bus_traffic() {
        // A zero max-packet-size must fail before any libusb call.
        let result = unsafe {
            iso_transfer_out(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0x01,
                &[1, 2, 3],
                0,
                Duration::from_millis(10),
            )
        };
        assert_eq!(result, UsbTransferResult::Error);
    }

    #[test]
    fn broken_descriptor_in_is_an_error_without_bus_traffic() {
        let result = unsafe {
            iso_transfer_in(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0x81,
                1024,
                0,
                Duration::from_millis(10),
            )
        };
        assert_eq!(result, UsbTransferResult::Error);
    }
}

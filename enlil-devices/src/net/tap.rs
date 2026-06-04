//! TAP network backend (Linux only).
//!
//! Opens a TAP device and reads/writes Ethernet frames through it.
//! Can be attached to a Linux bridge for external network connectivity.

#![cfg(target_os = "linux")]

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

use super::backend::NetBackend;

/// Flags for TAP device configuration.
const IFF_TAP: libc::c_short = 0x0002;
const IFF_NO_PI: libc::c_short = 0x1000;
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;

/// A TAP network device backend.
///
/// Provides raw Ethernet frame I/O through a Linux TAP interface.
pub struct TapBackend {
    fd: RawFd,
    name: String,
}

impl TapBackend {
    /// Open or create a TAP device with the given name.
    ///
    /// If `name` is empty, the kernel assigns a name (tap0, tap1, etc.).
    ///
    /// # Errors
    /// Returns an error if `/dev/net/tun` cannot be opened or the `TUNSETIFF`
    /// ioctl fails (e.g. insufficient privileges).
    pub fn new(name: &str) -> io::Result<Self> {
        // Open /dev/net/tun (c-string literal avoids a fallible allocation).
        let fd = unsafe { libc::open(c"/dev/net/tun".as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Prepare ifreq structure
        // struct ifreq is 40 bytes: 16 bytes name + 24 bytes union
        let mut ifr = [0u8; 40];

        // Copy interface name (max 15 chars + null)
        let name_bytes = name.as_bytes();
        let copy_len = name_bytes.len().min(15);
        ifr[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        // Set flags: IFF_TAP | IFF_NO_PI (no packet info header)
        let flags = (IFF_TAP | IFF_NO_PI) as u16;
        ifr[16..18].copy_from_slice(&flags.to_le_bytes());

        // Issue ioctl
        let ret = unsafe { libc::ioctl(fd, TUNSETIFF as _, ifr.as_mut_ptr()) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }

        // Extract the assigned name
        let name_end = ifr.iter().take(16).position(|&b| b == 0).unwrap_or(16);
        let assigned_name = String::from_utf8_lossy(&ifr[..name_end]).to_string();

        log::info!("TAP device opened: {assigned_name} (fd={fd})");

        Ok(Self {
            fd,
            name: assigned_name,
        })
    }

    /// Get the TAP interface name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Attach this TAP device to a Linux bridge interface.
    ///
    /// Equivalent to `brctl addif <bridge> <tap>` or `ip link set <tap> master <bridge>`.
    ///
    /// # Errors
    /// Returns an error if the control socket cannot be opened or the
    /// `SIOCBRADDIF` ioctl fails.
    pub fn attach_to_bridge(&self, bridge_name: &str) -> io::Result<()> {
        // SIOCBRADDIF: add an interface to a bridge.
        const SIOCBRADDIF: libc::c_ulong = 0x89a2;

        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if sock < 0 {
            return Err(io::Error::last_os_error());
        }

        // Get the interface index for the TAP device
        let ifindex = self.get_ifindex(sock)?;

        // Prepare ifreq for the bridge
        let mut ifr = [0u8; 40];
        let bridge_bytes = bridge_name.as_bytes();
        let copy_len = bridge_bytes.len().min(15);
        ifr[..copy_len].copy_from_slice(&bridge_bytes[..copy_len]);

        // Set ifr_ifindex (offset 16 in the union). i32 and u32 share a
        // little-endian byte layout, so no lossy cast is needed.
        ifr[16..20].copy_from_slice(&ifindex.to_le_bytes());

        let ret = unsafe { libc::ioctl(sock, SIOCBRADDIF as _, ifr.as_mut_ptr()) };
        unsafe {
            libc::close(sock);
        }

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        log::info!("TAP {} attached to bridge {bridge_name}", self.name);
        Ok(())
    }

    /// Bring the TAP interface up.
    ///
    /// # Errors
    /// Returns an error if the control socket cannot be opened or either of the
    /// `SIOCGIFFLAGS` / `SIOCSIFFLAGS` ioctls fails.
    pub fn set_up(&self) -> io::Result<()> {
        // IFF_UP | IFF_RUNNING live in the low 16 bits of the flags field;
        // declare them as u16 to avoid a lossy cast from c_int.
        const IFF_UP: u16 = 0x1;
        const IFF_RUNNING: u16 = 0x40;
        // Get/set interface flags ioctls.
        const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
        const SIOCSIFFLAGS: libc::c_ulong = 0x8914;

        let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if sock < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut ifr = [0u8; 40];
        let name_bytes = self.name.as_bytes();
        let copy_len = name_bytes.len().min(15);
        ifr[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        let ret = unsafe { libc::ioctl(sock, SIOCGIFFLAGS as _, ifr.as_mut_ptr()) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(sock);
            }
            return Err(err);
        }

        // Set IFF_UP | IFF_RUNNING.
        let mut flags = u16::from_le_bytes([ifr[16], ifr[17]]);
        flags |= IFF_UP | IFF_RUNNING;
        ifr[16..18].copy_from_slice(&flags.to_le_bytes());

        let ret = unsafe { libc::ioctl(sock, SIOCSIFFLAGS as _, ifr.as_mut_ptr()) };
        unsafe {
            libc::close(sock);
        }

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn get_ifindex(&self, sock: RawFd) -> io::Result<i32> {
        // SIOCGIFINDEX: resolve interface name to index.
        const SIOCGIFINDEX: libc::c_ulong = 0x8933;

        let mut ifr = [0u8; 40];
        let name_bytes = self.name.as_bytes();
        let copy_len = name_bytes.len().min(15);
        ifr[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        let ret = unsafe { libc::ioctl(sock, SIOCGIFINDEX as _, ifr.as_mut_ptr()) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(i32::from_le_bytes([ifr[16], ifr[17], ifr[18], ifr[19]]))
    }
}

impl NetBackend for TapBackend {
    fn send(&mut self, frame: &[u8]) -> io::Result<usize> {
        let ret =
            unsafe { libc::write(self.fd, frame.as_ptr().cast::<libc::c_void>(), frame.len()) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(0);
            }
            return Err(err);
        }
        // `ret >= 0` here, so the conversion is non-lossy.
        Ok(ret.cast_unsigned())
    }

    fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let ret =
            unsafe { libc::read(self.fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock {
                return Ok(0);
            }
            return Err(err);
        }
        // `ret >= 0` here, so the conversion is non-lossy.
        Ok(ret.cast_unsigned())
    }

    fn has_pending_rx(&self) -> bool {
        // Non-blocking readiness check via poll(2) with a zero timeout.
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&raw mut pfd, 1, 0) };
        ret > 0 && (pfd.revents & libc::POLLIN) != 0
    }

    fn backend_name(&self) -> &'static str {
        "tap"
    }
}

impl AsRawFd for TapBackend {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for TapBackend {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
        log::debug!("TAP device {} closed", self.name);
    }
}

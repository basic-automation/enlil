//! Networking bindings for the bare-metal kernel: signatures only.
//!
//! Enlil has no NIC driver / network stack yet, so every operation reports
//! [`Unsupported`](crate::io::IoErrorKind::Unsupported). The types exist
//! with real signatures so portable code compiles and can probe for
//! support at runtime.

use alloc::string::String;

use crate::io::IoResult;
use crate::sys::unsupported;

/// TCP listener. [`bind`](TcpListener::bind) always fails (no NIC driver).
#[derive(Debug)]
pub struct TcpListener {
    _private: (),
}

impl TcpListener {
    /// Always fails: no network stack on bare-metal Enlil.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn bind(_addr: &str) -> IoResult<Self> {
        unsupported("no network stack on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `bind` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn accept(&self) -> IoResult<(TcpStream, String)> {
        unsupported("no network stack on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `bind` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn local_addr(&self) -> IoResult<String> {
        unsupported("no network stack on bare-metal enlil")
    }
}

/// TCP stream. Uninhabited in practice.
#[derive(Debug)]
pub struct TcpStream {
    _private: (),
}

impl TcpStream {
    /// Always fails: no network stack on bare-metal Enlil.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn connect(_addr: &str) -> IoResult<Self> {
        unsupported("no network stack on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `connect` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn read(&mut self, _buf: &mut [u8]) -> IoResult<usize> {
        unsupported("no network stack on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `connect` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn write(&mut self, _buf: &[u8]) -> IoResult<usize> {
        unsupported("no network stack on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `connect` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn peer_addr(&self) -> IoResult<String> {
        unsupported("no network stack on bare-metal enlil")
    }
}

/// UDP socket. [`bind`](UdpSocket::bind) always fails (no NIC driver).
#[derive(Debug)]
pub struct UdpSocket {
    _private: (),
}

impl UdpSocket {
    /// Always fails: no network stack on bare-metal Enlil.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn bind(_addr: &str) -> IoResult<Self> {
        unsupported("no network stack on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `bind` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn send_to(&self, _buf: &[u8], _target: &str) -> IoResult<usize> {
        unsupported("no network stack on bare-metal enlil")
    }

    /// Stub for API parity; unreachable since `bind` never succeeds.
    ///
    /// # Errors
    ///
    /// Always returns [`IoErrorKind::Unsupported`](crate::io::IoErrorKind::Unsupported).
    pub const fn recv_from(&mut self, _buf: &mut [u8]) -> IoResult<(usize, String)> {
        unsupported("no network stack on bare-metal enlil")
    }
}

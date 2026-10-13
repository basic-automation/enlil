//! Networking bindings — mirrors `std::sys::net`'s role.

use crate::io::{IoError, IoResult};

/// TCP listener.
#[derive(Debug)]
pub struct TcpListener {
    inner: std::net::TcpListener,
}

impl TcpListener {
    /// Binds to `addr` (`"127.0.0.1:0"` picks an ephemeral port).
    ///
    /// # Errors
    ///
    /// Returns an error if the bind fails.
    pub fn bind(addr: &str) -> IoResult<Self> {
        std::net::TcpListener::bind(addr)
            .map(|inner| Self { inner })
            .map_err(IoError::from)
    }

    /// Accepts one connection, returning the stream and the peer address.
    ///
    /// # Errors
    ///
    /// Returns an error if the accept fails.
    pub fn accept(&self) -> IoResult<(TcpStream, String)> {
        self.inner
            .accept()
            .map(|(s, a)| (TcpStream { inner: s }, a.to_string()))
            .map_err(IoError::from)
    }

    /// The local address this listener is bound to.
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be queried.
    pub fn local_addr(&self) -> IoResult<String> {
        self.inner
            .local_addr()
            .map(|a| a.to_string())
            .map_err(IoError::from)
    }
}

/// TCP stream.
#[derive(Debug)]
pub struct TcpStream {
    inner: std::net::TcpStream,
}

impl TcpStream {
    /// Connects to `addr`.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection fails.
    pub fn connect(addr: &str) -> IoResult<Self> {
        std::net::TcpStream::connect(addr)
            .map(|inner| Self { inner })
            .map_err(IoError::from)
    }

    /// Reads bytes into `buf`, returning the number read.
    ///
    /// # Errors
    ///
    /// Returns an error if the read fails.
    pub fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        use std::io::Read;
        self.inner.read(buf).map_err(IoError::from)
    }

    /// Writes `buf`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        use std::io::Write;
        self.inner.write(buf).map_err(IoError::from)
    }

    /// The peer's address.
    ///
    /// # Errors
    ///
    /// Returns an error if the address cannot be queried.
    pub fn peer_addr(&self) -> IoResult<String> {
        self.inner
            .peer_addr()
            .map(|a| a.to_string())
            .map_err(IoError::from)
    }
}

/// UDP socket.
#[derive(Debug)]
pub struct UdpSocket {
    inner: std::net::UdpSocket,
}

impl UdpSocket {
    /// Binds to `addr` (`"127.0.0.1:0"` picks an ephemeral port).
    ///
    /// # Errors
    ///
    /// Returns an error if the bind fails.
    pub fn bind(addr: &str) -> IoResult<Self> {
        std::net::UdpSocket::bind(addr)
            .map(|inner| Self { inner })
            .map_err(IoError::from)
    }

    /// Sends `buf` to `target`.
    ///
    /// # Errors
    ///
    /// Returns an error if the send fails.
    pub fn send_to(&self, buf: &[u8], target: &str) -> IoResult<usize> {
        self.inner.send_to(buf, target).map_err(IoError::from)
    }

    /// Receives a datagram, returning its length and the sender's address.
    ///
    /// # Errors
    ///
    /// Returns an error if the receive fails.
    pub fn recv_from(&mut self, buf: &mut [u8]) -> IoResult<(usize, String)> {
        self.inner
            .recv_from(buf)
            .map(|(n, a)| (n, a.to_string()))
            .map_err(IoError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(&addr).unwrap();
        let (mut server, peer) = listener.accept().unwrap();
        assert!(peer.starts_with("127.0.0.1:"));
        assert!(client.peer_addr().unwrap().starts_with("127.0.0.1:"));
        assert_eq!(client.write(b"ping").unwrap(), 4);
        let mut buf = [0u8; 8];
        assert_eq!(server.read(&mut buf).unwrap(), 4);
        assert_eq!(&buf[..4], b"ping");
    }

    #[test]
    fn udp_loopback() {
        use crate::io::IoErrorKind;

        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let target = format!(
            "127.0.0.1:{}",
            b.inner.local_addr().expect("udp addr").port()
        );
        // Sandboxed environments may deny UDP sends outright; treat that as
        // a skip rather than a failure (the binds above already proved the
        // socket path works).
        let n = match a.send_to(b"datagram", &target) {
            Err(e) if e.kind() == IoErrorKind::PermissionDenied => return,
            other => other.unwrap(),
        };
        assert_eq!(n, 8);
        let mut buf = [0u8; 16];
        let (n, from) = b.recv_from(&mut buf).unwrap();
        assert_eq!(n, 8);
        assert_eq!(&buf[..8], b"datagram");
        assert!(from.starts_with("127.0.0.1:"));
    }
}

//! Linux epoll event source that drives the [`Reactor`](super::Reactor).
//!
//! The reactor itself is backend-neutral: it tracks which tasks wait on which
//! sources and fires their wakers when a source is signalled ready via
//! [`Reactor::mark_ready`](super::Reactor::mark_ready). This module is the
//! Linux OS event source that produces those signals (item 1.6) — the layer the
//! reactor's docs say "layers on top". It owns an epoll instance, registers
//! file descriptors against reactor tokens (stored in the epoll event's `data`
//! field), and on [`poll`](EpollPoller::poll) translates every ready descriptor
//! back into a `mark_ready(token)` call.
//!
//! Typical wiring: a task registers its source with the reactor to get a token,
//! adds its fd here with that token, and the executor's run loop calls
//! [`poll`](EpollPoller::poll) to block until the kernel reports readiness and
//! wake the waiting tasks. The bare-metal backend replaces this with a device
//! interrupt + IPI source producing the same `mark_ready` calls.

use std::io;
use std::os::unix::io::RawFd;

use super::Reactor;

/// An epoll instance that marks reactor tokens ready when their file
/// descriptors become readable/writable.
pub struct EpollPoller {
    epfd: RawFd,
}

impl EpollPoller {
    /// Create a new epoll instance (with `EPOLL_CLOEXEC` so it is not inherited
    /// across `exec`).
    ///
    /// # Errors
    /// Returns the OS error if `epoll_create1` fails (e.g. the process fd limit
    /// is exhausted).
    pub fn new() -> io::Result<Self> {
        // SAFETY: epoll_create1 takes a flags int and returns a new fd or -1.
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { epfd })
    }

    /// Register `fd` for the given epoll `events` (e.g. `libc::EPOLLIN`),
    /// associating it with reactor `token`. When the kernel reports the fd
    /// ready, [`poll`](Self::poll) calls `Reactor::mark_ready(token)`.
    ///
    /// # Errors
    /// Returns the OS error if `epoll_ctl(EPOLL_CTL_ADD)` fails (e.g. `fd` is
    /// already registered or is not pollable).
    pub fn add(&self, fd: RawFd, token: usize, events: u32) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events,
            u64: token as u64,
        };
        // SAFETY: `ev` outlives the call; epoll copies it.
        let rc = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_ADD, fd, &raw mut ev) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Register `fd` for read readiness (`EPOLLIN`) against `token`.
    ///
    /// # Errors
    /// As [`add`](Self::add).
    pub fn add_reader(&self, fd: RawFd, token: usize) -> io::Result<()> {
        self.add(fd, token, libc::EPOLLIN as u32)
    }

    /// Change the events and/or token an already-registered `fd` is watched
    /// for.
    ///
    /// # Errors
    /// Returns the OS error if `epoll_ctl(EPOLL_CTL_MOD)` fails (e.g. `fd` is
    /// not registered).
    pub fn modify(&self, fd: RawFd, token: usize, events: u32) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events,
            u64: token as u64,
        };
        // SAFETY: `ev` outlives the call; epoll copies it.
        let rc = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_MOD, fd, &raw mut ev) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Stop watching `fd`.
    ///
    /// # Errors
    /// Returns the OS error if `epoll_ctl(EPOLL_CTL_DEL)` fails (e.g. `fd` was
    /// not registered).
    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        // A zeroed event satisfies pre-2.6.9 kernels that reject a NULL pointer
        // on DEL; modern kernels ignore it.
        let mut ev: libc::epoll_event = unsafe { std::mem::zeroed() };
        // SAFETY: `ev` outlives the call.
        let rc = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, fd, &raw mut ev) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Wait up to `timeout_ms` (`None` = block indefinitely) for I/O events and
    /// mark each ready descriptor's token on `reactor`, waking the tasks
    /// waiting on them. Returns the number of tokens marked newly ready.
    ///
    /// A `EINTR` (interrupted by a signal) is not an error — it returns `Ok(0)`
    /// so the caller can loop.
    ///
    /// # Errors
    /// Returns the OS error if `epoll_wait` fails for any reason other than
    /// `EINTR`.
    pub fn poll(&self, reactor: &Reactor, timeout_ms: Option<i32>) -> io::Result<usize> {
        /// Ready descriptors drained per `epoll_wait`; more are collected on the
        /// next call.
        const MAX_EVENTS: usize = 64;
        let mut events: [libc::epoll_event; MAX_EVENTS] = unsafe { std::mem::zeroed() };
        let timeout = timeout_ms.unwrap_or(-1);
        // SAFETY: `events` has room for MAX_EVENTS entries.
        let n = unsafe {
            libc::epoll_wait(
                self.epfd,
                events.as_mut_ptr(),
                MAX_EVENTS as libc::c_int,
                timeout,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(0);
            }
            return Err(err);
        }
        // `n >= 0` here (the negative case returned above), so the count fits.
        let ready = usize::try_from(n).unwrap_or(0);
        let mut marked = 0;
        for ev in &events[..ready] {
            // epoll_event is `#[repr(C, packed)]`; read the data field with an
            // unaligned read rather than forming a reference to it.
            // SAFETY: `ev` points at a valid, initialized event.
            let token_u64 = unsafe { (&raw const ev.u64).read_unaligned() };
            // Tokens originate as `usize` (see `add`), so this round-trips; an
            // impossible >usize::MAX value falls back to an unknown token.
            let token = usize::try_from(token_u64).unwrap_or(usize::MAX);
            if reactor.mark_ready(token) {
                marked += 1;
            }
        }
        Ok(marked)
    }

    /// The underlying epoll file descriptor (e.g. to nest in another epoll).
    #[must_use]
    pub const fn as_raw_fd(&self) -> RawFd {
        self.epfd
    }
}

impl Drop for EpollPoller {
    fn drop(&mut self) {
        // SAFETY: `epfd` was created by this poller and is not closed elsewhere.
        unsafe {
            libc::close(self.epfd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a `pipe2(O_CLOEXEC)`; returns `(read_fd, write_fd)`.
    fn pipe() -> (RawFd, RawFd) {
        let mut fds = [0 as RawFd; 2];
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        assert_eq!(rc, 0, "pipe2 failed: {}", io::Error::last_os_error());
        fds.into()
    }

    /// Register a file descriptor as an opaque reactor source.
    fn register_fd(reactor: &Reactor, fd: RawFd) -> usize {
        reactor.register(usize::try_from(fd).expect("fd is non-negative"))
    }

    fn close(fd: RawFd) {
        unsafe { libc::close(fd) };
    }

    #[test]
    fn a_readable_fd_marks_its_reactor_token_ready() {
        let reactor = Reactor::new();
        let (rd, wr) = pipe();
        let token = register_fd(&reactor, rd);

        let poller = EpollPoller::new().expect("epoll");
        poller.add_reader(rd, token).expect("add reader");

        // Nothing written yet: a short poll times out and marks nothing.
        assert_eq!(poller.poll(&reactor, Some(0)).expect("poll"), 0);
        assert!(!reactor.is_ready(token));

        // Write a byte: the read end becomes readable and the token is marked.
        assert_eq!(unsafe { libc::write(wr, [42u8].as_ptr().cast(), 1) }, 1);
        assert_eq!(poller.poll(&reactor, Some(100)).expect("poll"), 1);
        assert!(reactor.is_ready(token), "token marked ready by epoll");

        close(rd);
        close(wr);
    }

    #[test]
    fn removing_an_fd_stops_its_events() {
        let reactor = Reactor::new();
        let (rd, wr) = pipe();
        let token = register_fd(&reactor, rd);
        let poller = EpollPoller::new().expect("epoll");
        poller.add_reader(rd, token).expect("add");
        poller.remove(rd).expect("remove");

        assert_eq!(unsafe { libc::write(wr, [1u8].as_ptr().cast(), 1) }, 1);
        // With the fd removed, epoll reports nothing even though it is readable.
        assert_eq!(poller.poll(&reactor, Some(50)).expect("poll"), 0);
        assert!(!reactor.is_ready(token));

        close(rd);
        close(wr);
    }

    #[test]
    fn two_fds_mark_two_distinct_tokens() {
        let reactor = Reactor::new();
        let (rd1, wr1) = pipe();
        let (rd2, wr2) = pipe();
        let t1 = register_fd(&reactor, rd1);
        let t2 = register_fd(&reactor, rd2);
        let poller = EpollPoller::new().expect("epoll");
        poller.add_reader(rd1, t1).expect("add1");
        poller.add_reader(rd2, t2).expect("add2");

        assert_eq!(unsafe { libc::write(wr1, [1u8].as_ptr().cast(), 1) }, 1);
        assert_eq!(unsafe { libc::write(wr2, [1u8].as_ptr().cast(), 1) }, 1);
        let marked = poller.poll(&reactor, Some(100)).expect("poll");
        assert_eq!(marked, 2);
        assert!(reactor.is_ready(t1));
        assert!(reactor.is_ready(t2));

        for fd in [rd1, wr1, rd2, wr2] {
            close(fd);
        }
    }
}

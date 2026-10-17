//! The relay's stream through the kernel's own interface: a non-blocking TCP
//! socket, connected in the background and watched in the same poll as the
//! wake (docs/02-io-shell.md 6).
//!
//! **Every send passes `MSG_NOSIGNAL`.** A send on a stream the relay has
//! closed raises a signal whose disposition is the host process's, not ours,
//! and its default ends the process.

use core::net::SocketAddr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::socket::{socklen, to_storage};
use super::wake::Wake;
use crate::shell::{MIN_WAIT_MS, Ready};
use crate::socket::DSCP_EF;
use crate::stream::Written;

/// The stream's socket and the wake it shares the loop's wait with.
#[derive(Debug)]
pub(crate) struct StreamIo {
    fd: OwnedFd,
    wake: Wake,
    connecting: bool,
}

impl StreamIo {
    /// A dual-stack TCP socket, its connect to `server` begun.
    pub(crate) fn connect(server: SocketAddr, wake: Wake) -> io::Result<Self> {
        // SAFETY: socket(2) with constant arguments; the descriptor is handed
        // straight to OwnedFd, which closes it on drop.
        let raw = unsafe {
            libc::socket(
                libc::AF_INET6,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                libc::IPPROTO_TCP,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut io = Self {
            // SAFETY: a fresh descriptor we own and have registered nowhere.
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            wake,
            connecting: true,
        };
        io.set_int(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 0)?;
        // A pass's messages are written together already; nothing waits to
        // gather more.
        io.set_int(libc::IPPROTO_TCP, libc::TCP_NODELAY, 1)?;
        // Marked at open, as the datagram socket is. No buffer sizes: the
        // stack tunes a stream's own.
        io.set_int(libc::IPPROTO_IP, libc::IP_TOS, DSCP_EF)?;
        io.set_int(libc::IPPROTO_IPV6, libc::IPV6_TCLASS, DSCP_EF)?;
        let (addr, len) = to_storage(server);
        // SAFETY: a sockaddr_in6 and its exact length.
        let rc = unsafe { libc::connect(raw, (&raw const addr).cast(), len) };
        if rc == 0 {
            io.connecting = false;
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(error);
            }
        }
        Ok(io)
    }

    pub(crate) fn wake(&self) -> &Wake {
        &self.wake
    }

    pub(crate) fn take_wake(&mut self) -> io::Result<bool> {
        self.wake.take()
    }

    pub(crate) fn connected(&self) -> bool {
        !self.connecting
    }

    /// Never: a send here is taken at once, in part or not at all.
    pub(crate) fn sending(&self) -> bool {
        false
    }

    /// Never: nothing is held for the wait to finish.
    pub(crate) fn finished(&mut self) -> Option<io::Result<usize>> {
        None
    }

    /// The flow is marked at open; nothing to do per destination.
    pub(crate) fn mark(&mut self, _to: SocketAddr) {}

    /// Wait for the stream -- bytes, the connect's end, room to write while
    /// `writing` -- or the wake, or the deadline; at once when `now`.
    pub(crate) fn wait(&mut self, timeout_ms: f64, writing: bool, now: bool) -> io::Result<Ready> {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "clamped to the wait bounds by the caller"
        )]
        let timeout = if now {
            0
        } else {
            // Rounded up, never truncated, as the datagram wait is.
            timeout_ms.max(MIN_WAIT_MS).ceil() as libc::c_int
        };
        let mut events = libc::POLLIN;
        if self.connecting || writing {
            events |= libc::POLLOUT;
        }
        let mut fds = [
            libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events,
                revents: 0,
            },
            libc::pollfd {
                fd: self.wake.raw(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two fully initialised descriptors and their count.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if rc < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
            return Ok(Ready::default());
        }
        let [stream, wake] = fds;
        if self.connecting && stream.revents != 0 {
            // Connected, or refused: the socket's pending error says which.
            let error = self.get_int(libc::SOL_SOCKET, libc::SO_ERROR)?;
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            self.connecting = false;
        }
        Ok(Ready {
            socket: stream.revents != 0 || now,
            wake: wake.revents != 0,
        })
    }

    /// Read what has arrived into `into`: zero is the stream's end, and
    /// `WouldBlock` that nothing is there.
    pub(crate) fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: a writable buffer and its exact length.
            let rc = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    into.as_mut_ptr().cast(),
                    into.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if let Ok(read) = usize::try_from(rc) {
                return Ok(read);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Write as much of `from` as the socket takes now.
    pub(crate) fn write(&mut self, from: &[u8]) -> io::Result<Written> {
        loop {
            // SAFETY: a readable buffer and its exact length.
            let rc = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    from.as_ptr().cast(),
                    from.len(),
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if let Ok(taken) = usize::try_from(rc) {
                return Ok(Written::Now(taken));
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(Written::Now(0)),
                _ => return Err(error),
            }
        }
    }

    fn set_int(&self, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> io::Result<()> {
        // SAFETY: an int option and its exact size.
        let rc = unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                level,
                name,
                (&raw const value).cast(),
                socklen(core::mem::size_of::<libc::c_int>()),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn get_int(&self, level: libc::c_int, name: libc::c_int) -> io::Result<libc::c_int> {
        let mut value: libc::c_int = 0;
        let mut len = socklen(core::mem::size_of::<libc::c_int>());
        // SAFETY: an int option buffer and its length, both writable.
        let rc = unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                level,
                name,
                (&raw mut value).cast(),
                &raw mut len,
            )
        };
        if rc == 0 {
            Ok(value)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

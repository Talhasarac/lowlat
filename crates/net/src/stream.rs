//! The relay's stream: one TCP connection carrying the relay's messages both
//! ways, for a client whose network blocks UDP (docs/03-connectivity.md 7.4).
//!
//! The platform's half (`crate::sys::StreamIo`) owns the socket and its place
//! in the loop's one wait -- a descriptor beside the wake's, or the stream's
//! own key on the completion port -- so the stream is a socket of the loop, not
//! a second loop. This half owns the bytes: one fixed receive buffer the
//! messages are found in, in place, and one fixed staging buffer a pass's
//! messages are written from with one call. Nothing here allocates after
//! construction.
//!
//! **Each message is found from its own length** (`lowlat_core::turn::frame`),
//! so a read may end anywhere: a message cut by the buffer's end is moved to the
//! front, and one longer than anything the endpoint takes, or on a channel the
//! standard reserves, is skipped by its length and counted, never stored. A
//! head that is neither a message nor channel data ends the stream.
//!
//! **The core's output waits while a write does.** A pass's messages are staged
//! and written a staging at a time, the next staged once the last is written
//! whole; while any of them is still unwritten nothing more is staged, so what
//! the core has handed over is never dropped, and a write that makes no
//! progress for five seconds ends the stream.

use core::net::{IpAddr, SocketAddr};
use core::time::Duration;
use std::io;
use std::time::Instant;

use lowlat_core::turn;

use crate::shell::Ready;
use crate::socket::{RECV_BATCH, RECV_SLOT};
use crate::sys::StreamIo;
use crate::wake::Wake;

/// The receive buffer: room for fifty floor-sized messages per read.
const RECEIVE: usize = 64 * 1024;

/// The staging buffer a pass's messages are written from.
const STAGING: usize = 32 * 1024;

/// The longest message taken whole: the receive slot, which holds the largest
/// datagram with a relay's framing, plus channel data's padding.
const LARGEST: usize = RECV_SLOT + 3;

/// Messages handed out per drain, as many datagrams as a batched receive.
const FRAMES: usize = RECV_BATCH;

/// A write that makes no progress this long ends the stream.
pub const STALL: Duration = Duration::from_secs(5);

/// How a write went: taken at once, in part or not at all, or held by the
/// platform until it completes on the loop's wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Written {
    Now(usize),
    #[cfg_attr(
        not(windows),
        allow(dead_code, reason = "only a completion port holds a send")
    )]
    Pending,
}

/// One stream to the relay, and the bytes it carries.
pub struct Stream {
    /// First, so it drops first: a platform that holds an operation into the
    /// buffers below takes it back before they go.
    io: StreamIo,
    server: SocketAddr,
    receive: Box<[u8]>,
    /// The bytes held: `start..end`.
    start: usize,
    end: usize,
    /// Where the messages the last drain handed out end; the next drain lets
    /// go of everything before it.
    consumed: usize,
    /// Bytes of a message being skipped still to come.
    skip: usize,
    /// The messages the last drain found, as offset and length.
    frames: Box<[(usize, usize)]>,
    found: usize,
    /// The last read filled the buffer, so more may be waiting.
    filled: bool,
    /// The stream ended after the messages now held; said by the next drain.
    ended: bool,
    skipped: u64,
    staging: Box<[u8]>,
    staged: usize,
    written: usize,
    /// A flush left bytes unwritten: nothing more is staged until they go.
    blocked: bool,
    /// When a write last made progress, or the first message went into an
    /// empty staging.
    progress: Instant,
    stall: Duration,
}

impl core::fmt::Debug for Stream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Stream")
            .field("io", &self.io)
            .field("server", &self.server)
            .field("held", &(self.end - self.start))
            .field("staged", &self.staged)
            .field("blocked", &self.blocked)
            .field("ended", &self.ended)
            .field("skipped", &self.skipped)
            .finish_non_exhaustive()
    }
}

impl Stream {
    /// Start connecting to the relay at `server`. The connect completes on the
    /// loop's waits, never here; nothing is staged until it has.
    pub fn connect(server: SocketAddr, wake: Wake) -> io::Result<Self> {
        Self::with_stall(server, wake, STALL)
    }

    /// The same, with a stall bound of `stall`, for a test that cannot wait
    /// five seconds to see a stalled write end the stream.
    pub fn with_stall(server: SocketAddr, wake: Wake, stall: Duration) -> io::Result<Self> {
        Ok(Self {
            io: StreamIo::connect(server, wake)?,
            server,
            receive: vec![0u8; RECEIVE].into_boxed_slice(),
            start: 0,
            end: 0,
            consumed: 0,
            skip: 0,
            frames: vec![(0, 0); FRAMES].into_boxed_slice(),
            found: 0,
            filled: false,
            ended: false,
            skipped: 0,
            staging: vec![0u8; STAGING].into_boxed_slice(),
            staged: 0,
            written: 0,
            blocked: false,
            progress: Instant::now(),
            stall,
        })
    }

    /// The relay this stream reaches.
    pub fn server(&self) -> SocketAddr {
        self.server
    }

    /// Whether the connect has completed.
    pub fn connected(&self) -> bool {
        self.io.connected()
    }

    /// Messages discarded whole: on a reserved channel, or longer than the
    /// endpoint takes.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    pub(crate) fn wake(&self) -> &Wake {
        self.io.wake()
    }

    /// Consume the pending wake, if any; before the application is pulled.
    pub fn take_wake(&mut self) -> io::Result<bool> {
        self.io.take_wake()
    }

    /// Wait for the stream or the wake, or the deadline. A write stalled past
    /// its bound, a failed connect or a broken stream is the error.
    pub fn wait(&mut self, timeout_ms: f64) -> io::Result<Ready> {
        if self.blocked && self.progress.elapsed() >= self.stall {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.io.wait(timeout_ms, self.blocked, self.ended)
    }

    /// Read what has arrived and find the messages in it. Returns how many
    /// were found; [`Stream::frames`] hands them out until the next drain.
    pub fn drain(&mut self) -> io::Result<usize> {
        self.found = 0;
        self.start = self.consumed;
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        } else if self.receive.len() - self.end < LARGEST {
            // A message cut by the buffer's end, moved to the front so the rest
            // of it has room behind it.
            self.receive.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        self.consumed = self.start;
        self.filled = false;
        // Past the end there is nothing more to read, only what is held.
        if !self.ended {
            let free = self.receive.len() - self.end;
            // A read into no room reads nothing, which would pass for the end.
            let read = match free {
                0 => Err(io::ErrorKind::WouldBlock.into()),
                _ => self
                    .io
                    .read(self.receive.get_mut(self.end..).unwrap_or_default()),
            };
            match read {
                Ok(0) => self.ended = true,
                Ok(read) => {
                    self.end += read;
                    self.filled = read == free;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        self.frame()?;
        // The end is said by the first drain that finds nothing before it, so
        // every message that arrived ahead of it is handed out first, however
        // many drains that takes.
        if self.ended && self.found == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        Ok(self.found)
    }

    /// Find the messages held, in place.
    fn frame(&mut self) -> io::Result<()> {
        let mut at = self.start;
        while at < self.end {
            if self.skip > 0 {
                let taken = self.skip.min(self.end - at);
                at += taken;
                self.skip -= taken;
                continue;
            }
            let head = self.receive.get(at..self.end).unwrap_or_default();
            let Some(frame) =
                turn::frame(head).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?
            else {
                break;
            };
            if frame.reserved || frame.wire > LARGEST {
                self.skipped += 1;
                self.skip = frame.wire;
                continue;
            }
            if self.end - at < frame.wire || self.found == FRAMES {
                break;
            }
            if let Some(slot) = self.frames.get_mut(self.found) {
                *slot = (at, frame.len);
            }
            self.found += 1;
            at += frame.wire;
        }
        self.consumed = at;
        Ok(())
    }

    /// True when the last drain may have left more behind: the read filled
    /// the buffer, or the messages filled the batch.
    pub fn saturated(&self) -> bool {
        self.filled || self.found == FRAMES
    }

    /// The messages the last drain found, each with the relay as its source.
    pub fn frames(&self) -> impl Iterator<Item = (SocketAddr, Option<IpAddr>, &[u8])> {
        self.frames
            .get(..self.found)
            .unwrap_or_default()
            .iter()
            .filter_map(move |&(at, len)| {
                Some((self.server, None, self.receive.get(at..at + len)?))
            })
    }

    /// Room for one more message, as large as the largest: none while the
    /// connect is pending, while a write is, or once the staging is short.
    pub fn room(&mut self) -> Option<&mut [u8]> {
        if !self.io.connected() || self.blocked || self.io.sending() {
            return None;
        }
        if self.staging.len() - self.staged < LARGEST {
            return None;
        }
        self.staging.get_mut(self.staged..)
    }

    /// The `len` bytes just written into [`Stream::room`] are a message.
    pub fn commit(&mut self, len: usize) {
        if self.staged == 0 {
            // The stall is measured from the first message a write owes.
            self.progress = Instant::now();
        }
        self.staged = (self.staged + len).min(self.staging.len());
    }

    /// Write what is staged, as far as the socket takes it now. Returns the
    /// bytes written.
    pub fn flush(&mut self) -> io::Result<usize> {
        let mut wrote = 0;
        if let Some(done) = self.io.finished() {
            let taken = done?;
            self.written = (self.written + taken).min(self.staged);
            wrote += taken;
            self.progress = Instant::now();
        }
        while self.written < self.staged && !self.io.sending() {
            let unwritten = self
                .staging
                .get(self.written..self.staged)
                .unwrap_or_default();
            match self.io.write(unwritten)? {
                Written::Now(0) | Written::Pending => break,
                Written::Now(taken) => {
                    self.written += taken;
                    wrote += taken;
                    self.progress = Instant::now();
                }
            }
        }
        if self.written == self.staged {
            self.written = 0;
            self.staged = 0;
        }
        self.blocked = self.written < self.staged;
        Ok(wrote)
    }

    /// Stage the messages `next` writes into the room it is given, one a call,
    /// and write them, until `next` has none or the stream takes no more now.
    /// A staging that fills is written, and staging goes on once all of it
    /// has gone, so a burst larger than the staging waits for no later pass.
    /// Returns how many messages were staged.
    pub fn send_from(
        &mut self,
        mut next: impl FnMut(&mut [u8]) -> Option<usize>,
    ) -> io::Result<usize> {
        let mut staged = 0;
        loop {
            let Some(room) = self.room() else {
                self.flush()?;
                if self.room().is_some() {
                    continue;
                }
                return Ok(staged);
            };
            let Some(len) = next(room) else {
                break;
            };
            self.commit(len);
            staged += 1;
        }
        self.flush()?;
        Ok(staged)
    }

    /// Whether a flush left bytes unwritten.
    pub fn writing(&self) -> bool {
        self.blocked
    }

    /// Mark the stream's flow where the platform marks per destination.
    pub(crate) fn mark(&mut self) {
        self.io.mark(self.server);
    }
}

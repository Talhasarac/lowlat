//! What a guest's devices hand back, whatever the platform builds them on:
//! what an application wrote to a pad, a pad's reports on their way to an
//! application that holds the device, and what a pad was asked to vibrate at.

use lowlat_core::pad::{self, INPUT_LEN, Product};

/// The largest report the device is written: the DualSense's output report
/// is 63 bytes, the feature writes at most 64.
pub const WRITTEN_MAX: usize = pad::FEATURE_MAX;

/// What an application on this host wrote to the device, for the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub kind: pad::OutputKind,
    pub len: usize,
    /// The report, identifier byte first: the USB form.
    pub report: [u8; WRITTEN_MAX],
}

/// What a pad is asked to vibrate at, on the way back to its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rumble {
    /// The peer's own identifier for the pad, so it needs no mapping back.
    pub pad: u32,
    /// **Eight bits each, as the peer carries them.** The kernel counts a
    /// motor in sixteen and the peer widens what it is sent by repeating the
    /// byte, so taking the high byte is exactly the inverse and a value
    /// survives the round trip unchanged.
    pub large: u8,
    pub small: u8,
}

/// What an application wrote to one of a guest's HID pads, for the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadWritten {
    pub pad: u32,
    pub written: Written,
}

/// A report pad's report on its way to the application rather than into a
/// device of this host's own, and the end of such a pad (docs/05-host.md
/// section 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarded {
    /// An input report, the whole USB form.
    Input {
        pad: u32,
        product: Product,
        report: [u8; INPUT_LEN],
    },
    /// A feature report: `len` bytes, identifier byte first.
    Feature {
        pad: u32,
        product: Product,
        len: usize,
        report: [u8; INPUT_LEN],
    },
    /// The pad is gone: unplugged by its peer, or its guest left. Told after
    /// the pad's last report, so a device destroyed on it has seen them all.
    Unplugged { pad: u32, product: Product },
}

/// Where forwarded reports go. A call rather than a queue: the application's
/// queue is across a boundary this crate does not see, and the report is in
/// it one hop after it was parsed rather than two.
pub struct Forward(pub Box<dyn FnMut(Forwarded) + Send>);

impl core::fmt::Debug for Forward {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Forward")
    }
}

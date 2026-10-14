//! The devices a guest's input lands on, on Windows: none are built yet
//! (docs/impl-plan-windows.md, W2.3).
//!
//! **Refused at creation**, so a guest is admitted without input rather than
//! handed devices that quietly swallow what it sends.

use crate::event::{Device, Event, Sink};
use crate::handback::{Forward, PadWritten, Rumble};

/// Why a guest's devices could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No input device is built for this platform.
    NotBuilt,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotBuilt => f.write_str("no input device is built for this platform"),
        }
    }
}

impl std::error::Error for Error {}

/// One guest's devices. None can be made, so none exists: every method below
/// is unreachable by construction rather than by a check.
#[derive(Debug)]
pub struct Devices {
    none: core::convert::Infallible,
}

impl Devices {
    pub fn create(_guest: &str) -> Result<Self, Error> {
        Err(Error::NotBuilt)
    }

    pub fn forward_reports(&mut self, _to: Forward) {
        match self.none {}
    }

    pub fn written(&mut self) -> Option<PadWritten> {
        match self.none {}
    }

    pub fn tick(&mut self) {
        match self.none {}
    }

    pub fn rumble(&mut self) -> Option<Rumble> {
        match self.none {}
    }
}

impl Sink for Devices {
    fn emit(&mut self, _device: Device, _events: &[Event]) {
        match self.none {}
    }
}

/// Whether a text console would turn the attention chord into a restart.
/// Windows has no such console.
#[must_use]
pub fn console_takes_the_chord() -> bool {
    false
}

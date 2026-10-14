//! The display on Windows: none is built yet (docs/impl-plan-windows.md, W2.2).
//!
//! **Nothing lists and nothing opens**, so a stream on this platform runs on
//! the synthetic source or not at all.

use lowlat_capture::{Placement, Pointer};
use lowlat_common::clock::Time;

use super::{Acquired, Capturable, Selectable};

/// What went wrong.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Error {
    /// No display is built for this platform.
    NotBuilt,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotBuilt => f.write_str("no display is built for this platform"),
        }
    }
}

impl std::error::Error for Error {}

/// What an encoder calls a converted frame it has been handed. There are
/// none, since nothing is converted.
#[derive(Debug)]
pub enum Registration {}

/// A display of this host's. None can be opened, so none exists: every method
/// below is unreachable by construction rather than by a check.
#[derive(Debug)]
pub struct Display {
    none: core::convert::Infallible,
}

impl Display {
    /// Nothing, since no display is built here.
    pub fn outputs() -> Vec<Selectable> {
        Vec::new()
    }

    /// Nothing this host could capture, which is the honest answer while no
    /// display is built for the platform.
    pub fn capturable() -> Capturable {
        Capturable::NothingLit
    }

    pub fn preferred() -> Option<String> {
        None
    }

    pub fn shape_of_display(_wanted: Option<&str>) -> Option<(u32, u32, u32)> {
        None
    }

    pub fn place(&self) -> Option<Placement> {
        match self.none {}
    }

    pub fn selected(&self) -> Option<&str> {
        match self.none {}
    }

    pub fn presented(&self) -> Option<&Registration> {
        match self.none {}
    }

    pub fn acquire(&mut self) -> Result<Time, Error> {
        match self.none {}
    }

    pub fn converted(&mut self) -> Result<Option<Acquired>, Error> {
        match self.none {}
    }

    pub fn pointer(&mut self) -> Option<Pointer> {
        match self.none {}
    }

    pub fn attached(&self) -> bool {
        match self.none {}
    }

    pub fn arm_vblank(&self) -> Result<(), Error> {
        match self.none {}
    }

    pub fn wait_present(&self, _timeout_ms: i32) -> Option<bool> {
        match self.none {}
    }

    pub fn poke(&mut self) -> Result<(), Error> {
        match self.none {}
    }

    pub fn take_resize(&mut self) -> bool {
        match self.none {}
    }

    pub fn pointer_image(&self) -> &[u8] {
        match self.none {}
    }

    pub fn set_pointer_turn(&mut self, _turn: lowlat_core::video::Rotation) {
        match self.none {}
    }
}

//! The session's side of this host, on Windows: there is none. The host runs
//! in the session itself, with no session helper and no tray
//! (docs/impl-plan-windows.md, W2).

use std::collections::{HashMap, HashSet};

use lowlat_host::admission::Admission;

use crate::app;
use crate::service::Introduced;

/// What the session's side keeps between the service's passes: nothing.
pub(crate) struct Side;

impl Side {
    pub(crate) fn new() -> Self {
        Self
    }

    /// One pass of the service's loop, on the session's side: nothing to do.
    pub(crate) fn pass(
        &mut self,
        _seam: &mut Admission,
        _settings: &app::Settings,
        _peers: &HashMap<String, Introduced>,
        _established: &HashSet<String>,
    ) {
    }
}

/// No channel: no session program connects.
pub(crate) fn listen() {}

pub(crate) fn session() -> ! {
    refuse("session")
}

pub(crate) fn tray() -> ! {
    refuse("tray")
}

fn refuse(role: &str) -> ! {
    lowlat_common::log_error!("lowlatd: the {role} role is not built for this platform");
    std::process::exit(2);
}

/// No session program to take a guest's clipboard.
pub(crate) fn clipboard(_body: &[u8]) -> bool {
    false
}

/// Nothing on this platform relays a guest's request for a mode.
pub(crate) fn ask_mode(
    _output: &str,
    _size: Option<(u32, u32)>,
    _rotation: Option<u8>,
) -> Result<(), &'static str> {
    Err("nothing on this platform relays a mode request")
}

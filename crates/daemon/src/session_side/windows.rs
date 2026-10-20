//! The session's side of this host, on Windows: no program of its own. The
//! host runs in the session itself, with no session helper and no tray
//! (docs/impl-plan-windows.md, W2), and puts a guest's mode on a display
//! itself (W3.10).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use lowlat_host::admission::Admission;
use lowlat_host::display::{Display, VIRTUAL};
use lowlat_host::video::Rotation;
use lowlat_vdd_ctl::{Controller, Error, Mode, Turn, put_size, put_turn};

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

/// A request for a mode being put, one at a time.
static PUTTING: AtomicBool = AtomicBool::new(false);

/// Put a size, a turn or both on the output the system names `output`,
/// `\\.\DISPLAYn`: **this host runs in the session**, so it puts them itself
/// where Linux asks the session's helper. On a thread of its own, since the
/// system's display configuration and the driver's controller each take a
/// while to answer, one request at a time; what came of it is a line of the
/// log. Never saved to the system's store but as a kept monitor's own
/// arrangement is.
pub(crate) fn ask_mode(
    output: &str,
    size: Option<(u32, u32)>,
    rotation: Option<u8>,
) -> Result<(), &'static str> {
    if PUTTING.swap(true, Ordering::AcqRel) {
        return Err("a request is already outstanding");
    }
    let output = output.to_string();
    let spawned = std::thread::Builder::new()
        .name("lowlatd-mode".to_string())
        .spawn(move || {
            put(&output, size, rotation.map(turn_of));
            PUTTING.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        PUTTING.store(false, Ordering::Release);
        return Err("no thread to put it on");
    }
    Ok(())
}

/// The controller's name for a turn the wire numbers, digit for digit.
fn turn_of(rotation: u8) -> Turn {
    match Rotation::from_bits(rotation) {
        Rotation::Deg90 => Turn::Quarter,
        Rotation::Deg180 => Turn::Half,
        Rotation::Deg270 => Turn::ThreeQuarters,
        Rotation::None | Rotation::Unknown => Turn::Upright,
    }
}

/// Where a guest's mode for an output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Road {
    /// The host's own display: its turn as the desk turns it, through the
    /// system's configuration; its size is the library's (`virtual_*`).
    Own,
    /// A monitor of the display driver kept by no handle: through the
    /// controller, which adds a size the monitor does not list by plugging
    /// it again.
    Kept(u8),
    /// A monitor of the driver another program plugged, bound to it: not
    /// this host's to change.
    Bound,
    /// Any other display -- a panel, another vendor's virtual display -- put
    /// through the system's configuration.
    Display,
}

/// Which road `output` takes, among the driver's `monitors` as the system's
/// name for each, its slot, and whether it is kept; `own` when it is the host's
/// own display.
fn road<'a>(
    monitors: impl IntoIterator<Item = (&'a str, u8, bool)>,
    output: &str,
    own: bool,
) -> Road {
    if own {
        return Road::Own;
    }
    match monitors
        .into_iter()
        .find(|(name, _, _)| !name.is_empty() && name.eq_ignore_ascii_case(output))
    {
        Some((_, slot, true)) => Road::Kept(slot),
        Some((_, _, false)) => Road::Bound,
        None => Road::Display,
    }
}

/// Put what was asked, size first and then the turn, and say what came of it.
fn put(output: &str, size: Option<(u32, u32)>, turn: Option<Turn>) {
    let own = Display::outputs()
        .iter()
        .any(|listed| listed.id == VIRTUAL && listed.connector.eq_ignore_ascii_case(output));
    let mut controller = Controller::open().ok();
    let monitors = controller
        .as_ref()
        .and_then(|controller| controller.list().ok())
        .unwrap_or_default();
    let found = road(
        monitors
            .iter()
            .map(|monitor| (monitor.name.as_str(), monitor.slot, monitor.kept)),
        output,
        own,
    );
    match found {
        Road::Bound => lowlat_common::log_warn!(
            "lowlatd: {output} is a monitor another program plugged, not put at {size:?} \
             turn={turn:?}"
        ),
        Road::Own => {
            if let Some(turn) = turn {
                turned(output, turn, put_turn(output, turn));
            }
        }
        Road::Kept(slot) => {
            let Some(controller) = controller.as_mut() else {
                return;
            };
            if let Some((width, height)) = size {
                // **At the refresh it runs**: a size the monitor does not list
                // is added at it.
                let refresh = monitors
                    .iter()
                    .find(|monitor| monitor.slot == slot)
                    .map_or(0, |monitor| monitor.mode.refresh_hz);
                let mode = Mode::new(width, height, refresh);
                let done = controller.set_mode(slot, mode);
                sized(output, (width, height), done.map(|monitor| monitor.mode));
            }
            if let Some(turn) = turn {
                turned(output, turn, controller.set_turn(slot, turn).map(|_| ()));
            }
        }
        Road::Display => {
            if let Some((width, height)) = size {
                sized(output, (width, height), put_size(output, width, height));
            }
            if let Some(turn) = turn {
                turned(output, turn, put_turn(output, turn));
            }
        }
    }
}

/// Say what came of a size asked of `output`.
fn sized(output: &str, (width, height): (u32, u32), done: Result<Mode, Error>) {
    match done {
        Ok(mode) => lowlat_common::log_info!("lowlatd: {output} put at {mode}"),
        Err(error) => lowlat_common::log_warn!(
            "lowlatd: {output} not put at {width}x{height}, error={error:?}"
        ),
    }
}

/// Say what came of a turn asked of `output`.
fn turned(output: &str, turn: Turn, done: Result<(), Error>) {
    match done {
        Ok(()) => lowlat_common::log_info!("lowlatd: {output} turned {turn:?}"),
        Err(error) => {
            lowlat_common::log_warn!("lowlatd: {output} not turned {turn:?}, error={error:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A guest's mode goes where the display's owner lets it**: the host's
    /// own display by the system's configuration whatever the driver lists, a
    /// kept monitor through the controller, a monitor another program plugged
    /// nowhere, and a display the driver does not list as any display.
    #[test]
    fn a_mode_goes_by_whose_the_display_is() {
        let monitors = [
            (r"\.\DISPLAY42", 0, true),
            (r"\.\DISPLAY43", 15, false),
            ("", 3, true),
        ];
        assert_eq!(road(monitors, r"\.\DISPLAY43", true), Road::Own);
        assert_eq!(road(monitors, r"\.\display42", false), Road::Kept(0));
        assert_eq!(road(monitors, r"\.\DISPLAY43", false), Road::Bound);
        assert_eq!(road(monitors, r"\.\DISPLAY1", false), Road::Display);
        assert_eq!(
            road(monitors, "", false),
            Road::Display,
            "an unshown monitor's empty name named something"
        );
    }

    /// The wire's turns are the controller's, digit for digit.
    #[test]
    fn a_turn_is_the_wires_digit_for_digit() {
        for (bits, turn) in [
            (1, Turn::Upright),
            (2, Turn::Quarter),
            (3, Turn::Half),
            (4, Turn::ThreeQuarters),
            (0, Turn::Upright),
        ] {
            assert_eq!(turn_of(bits), turn, "{bits}");
        }
    }
}

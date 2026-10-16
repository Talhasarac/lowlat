//! The host's own display on Windows: one monitor of the host's display
//! driver, plugged by the host and bound to it, so it goes when the host does
//! however the host ends, and held at the mode the host wants
//! (docs/11-drivers.md section 1, docs/06-api.md section 14).
//!
//! **Its calls wait for the system**, up to the controller's bound, so they
//! run on the stream's own thread and never on a caller's.

use lowlat_vdd_ctl::{Controller, Error, Lifetime, MAX_MONITORS, Mode};

use super::sys::publish_own;

/// The size the display is first plugged at when nothing asked for one.
pub(crate) const FIRST_SIZE: (u32, u32) = (1920, 1080);

/// The host's own display.
#[derive(Debug, Default)]
pub(crate) struct Own {
    controller: Option<Controller>,
    /// The slot the monitor is in, while one is plugged.
    slot: Option<u8>,
    /// The mode the host wants it at, once it wants one.
    wanted: Option<Mode>,
    /// Why the last attempt failed, said once rather than every pass.
    refused: Option<Error>,
}

impl Own {
    /// Whether a monitor is plugged.
    pub(crate) fn plugged(&self) -> bool {
        self.slot.is_some()
    }

    /// The mode the host wants, once it wants one.
    pub(crate) fn wanted(&self) -> Option<Mode> {
        self.wanted
    }

    /// Have the display at `mode`: plugged at it where there is none, else
    /// put there when the system has it at anything else -- which another
    /// monitor's arrival and a program leaving exclusive fullscreen both do,
    /// putting a saved mode back. Answers whether it is at `mode` now.
    pub(crate) fn hold(&mut self, mode: Mode) -> bool {
        if !mode.is_valid() {
            self.refuse("a mode no monitor of this display may be", Error::Mode);
            return false;
        }
        self.wanted = Some(mode);
        let (Some(slot), Some(controller)) = (self.slot, self.controller.as_mut()) else {
            return self.plug(mode);
        };
        let monitor = match controller.monitor(slot) {
            Ok(monitor) => monitor,
            // Gone underneath the host, as a restart of the driver takes it:
            // plugged again, which is what keeping it for the host's life is.
            Err(Error::NoMonitor) => {
                lowlat_common::log_warn!("display: the host's own display went, plugging it again");
                self.slot = None;
                publish_own(None);
                return self.plug(mode);
            }
            Err(error) => {
                self.refuse("the host's own display could not be read", error);
                return false;
            }
        };
        if monitor.active && !monitor.name.is_empty() {
            publish_own(Some(&monitor.name));
        }
        if monitor.active && monitor.mode == mode {
            return true;
        }
        match controller.set_mode(slot, mode) {
            Ok(now) => {
                lowlat_common::log_info!(
                    "display: the host's own display put at {mode}, was {} {}",
                    monitor.mode,
                    if monitor.active { "shown" } else { "not shown" }
                );
                if !now.name.is_empty() {
                    publish_own(Some(&now.name));
                }
                self.refused = None;
                true
            }
            Err(error) => {
                self.refuse("the host's own display could not be put at its mode", error);
                false
            }
        }
    }

    /// Plug the monitor at `mode`, bound to this host, in the highest free
    /// slot: an application's first free one is never the host's, and the
    /// host's display keeps one identity the system remembers it by.
    fn plug(&mut self, mode: Mode) -> bool {
        if self.controller.is_none() {
            match Controller::open() {
                Ok(controller) => self.controller = Some(controller),
                Err(error) => {
                    self.refuse("the host's own display driver could not be opened", error);
                    return false;
                }
            }
        }
        let Some(controller) = self.controller.as_mut() else {
            return false;
        };
        let taken: Vec<u8> = controller
            .list()
            .map(|monitors| monitors.iter().map(|monitor| monitor.slot).collect())
            .unwrap_or_default();
        let free = (0..MAX_MONITORS)
            .rev()
            .filter_map(|slot| u8::try_from(slot).ok())
            .filter(|slot| !taken.contains(slot));
        for slot in free {
            match controller.plug(mode, Lifetime::Bound, false, Some(slot)) {
                Ok(monitor) => {
                    lowlat_common::log_info!(
                        "display: the host's own display plugged, slot={slot} {mode} {}",
                        monitor.name
                    );
                    self.slot = Some(slot);
                    if !monitor.name.is_empty() {
                        publish_own(Some(&monitor.name));
                    }
                    self.refused = None;
                    return true;
                }
                // Taken between the list and the plug, by another program.
                Err(Error::SlotTaken) => {}
                // **Plugged, and the system has not finished.** The slot is
                // the host's; the next look finds it shown and puts its mode.
                Err(Error::NotReady) => {
                    lowlat_common::log_warn!(
                        "display: the host's own display plugged in slot {slot}, the system has \
                         not shown it yet"
                    );
                    self.slot = Some(slot);
                    return false;
                }
                Err(error) => {
                    self.refuse("the host's own display could not be plugged", error);
                    return false;
                }
            }
        }
        self.refuse("the host's own display has no free slot", Error::Full);
        false
    }

    /// Say why, once until something succeeds or the reason changes.
    fn refuse(&mut self, what: &str, error: Error) {
        if self.refused != Some(error) {
            self.refused = Some(error);
            lowlat_common::log_warn!("display: {what}, {error:?}");
        }
    }
}

impl Drop for Own {
    /// **The controller's handle unplugs the monitor as it closes**, the
    /// driver's own rule for a bound one; what is left is to stop naming it.
    fn drop(&mut self) {
        if self.slot.is_some() {
            publish_own(None);
        }
    }
}

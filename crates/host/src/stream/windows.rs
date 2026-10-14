//! What a stream is built on, on Windows: no encoder is built yet
//! (docs/impl-plan-windows.md, W2.1). The loop above is written once; a
//! platform writes this module.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use lowlat_core::control::status;

use super::{Config, Exit, Join, Roster, Shared};

/// Refuse the stream: there is no encoder to build, so its guests are told
/// the encoder is unavailable, as on a machine whose device cannot encode.
pub(super) fn build(
    _shared: &Arc<Shared>,
    _arrivals: &mpsc::Receiver<Join>,
    _config: &Config,
    _roster: &mut Roster,
) -> Exit {
    // **Said once.** The loop asks again at its own pace while a refusal is
    // young, and the answer does not change.
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        lowlat_common::log_error!("stream: no encoder is built for this platform");
    }
    Exit::Failed(status::ENCODER_UNAVAILABLE)
}

/// No encoder here codes full chroma, so the offer is refused with that gate.
pub(super) fn chroma_census(_config: &Config) -> Result<(), &'static str> {
    Err("no encoder is built for this platform")
}

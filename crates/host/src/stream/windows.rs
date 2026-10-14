//! What a stream is built on, on Windows: the vendor's encoder on a D3D11
//! device, fed by the synthetic source until the display is built
//! (docs/impl-plan-windows.md, W2.2). The loop above is written once; a
//! platform writes this module.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use lowlat_core::control::status;

use super::{
    Backend, Codec, Config, Exit, FromDevice, Join, Roster, Shared, encode_loop, settle_pace,
    start_bps,
};

/// The vendor whose encoder this platform builds.
const NVIDIA: u32 = 0x10de;

/// Build the stream's device and encoder for `config` and run it until it
/// exits: the vendor's encoder on a device of its own, the generator its
/// source. A display, or the open backend by name, is refused rather than
/// answered with something else.
pub(super) fn build(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: &Config,
    roster: &mut Roster,
) -> Exit {
    lowlat_common::log_info!(
        "stream: encoding w={} h={} fps={} ceiling_mbps={:.1} codec={:?} backend={:?}",
        config.width,
        config.height,
        config.fps,
        config.configured_mbps,
        config.codec,
        Backend::Vendor
    );
    // **Said once each.** The loop asks again at its own pace while a refusal
    // is young, and neither answer changes.
    static NO_DISPLAY: AtomicBool = AtomicBool::new(false);
    static NO_OPEN: AtomicBool = AtomicBool::new(false);
    if config.display {
        if !NO_DISPLAY.swap(true, Ordering::Relaxed) {
            lowlat_common::log_error!("stream: no display is built for this platform yet");
        }
        return Exit::Failed(status::CAPTURE_UNAVAILABLE);
    }
    if config.backend == Some(Backend::Open) {
        if !NO_OPEN.swap(true, Ordering::Relaxed) {
            lowlat_common::log_error!("stream: the open backend is not built for this platform");
        }
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    }
    run_vendor(shared, arrivals, config.clone(), roster)
}

/// No encoder here takes full chroma yet: the generator's frames carry two
/// planes, and the layout this device takes full chroma in is the display's
/// conversion's to settle. So the offer is refused, with that gate.
pub(super) fn chroma_census(_config: &Config) -> Result<(), &'static str> {
    Err("the vendor encoder on this platform, which takes no full chroma before its conversion")
}

fn run_vendor(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
) -> Exit {
    let Ok(d3d11) = lowlat_encode::d3d11::D3d11::load() else {
        lowlat_common::log_error!("stream: graphics libraries unavailable, nothing will encode");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    // **The first of the vendor's adapters that renders**, in the system's
    // high-performance order. From the display's step on the display decides:
    // a capture and its encoder share one device.
    let Some(adapter) = d3d11.adapters().ok().and_then(|adapters| {
        adapters
            .into_iter()
            .find(|adapter| adapter.decodes_here() && adapter.vendor == NVIDIA)
    }) else {
        lowlat_common::log_error!("stream: no adapter of the encoder's vendor renders here");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let device = match d3d11.open(adapter.luid) {
        Ok(device) => device,
        Err(error) => {
            lowlat_common::log_error!("stream: no device on {}, {error}", adapter.luid);
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    let Ok(api) = lowlat_encode::nvenc::Api::load() else {
        lowlat_common::log_error!("stream: encoder runtime unavailable");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let session = match api.open_session(Arc::new(device)) {
        Ok(session) => session,
        Err(error) => {
            lowlat_common::log_error!("stream: encode session could not be opened, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    lowlat_common::log_info!(
        "stream: encoding on {} ({})",
        adapter.description,
        adapter.luid
    );
    // **The generator's picture is the configured one**, so the size is said
    // before any guest is seated, as it is once a display has answered.
    let (width, height) = (config.width, config.height);
    shared.publish_picture(width, height);
    let config = Config {
        width,
        height,
        fps: settle_pace(shared, config.fps, 0),
        ..config
    };
    let encoder = session.initialize(lowlat_encode::nvenc::Config {
        codec: match config.codec {
            Codec::H264 => lowlat_encode::nvenc::Codec::H264,
            Codec::H265 => lowlat_encode::nvenc::Codec::H265,
        },
        width,
        height,
        fps: config.fps,
        bitrate_bps: start_bps(&config),
        min_qp: config.quality.min_qp(),
        thorough: config.quality.thorough(),
        // Full chroma is granted per session, never configured; the census
        // that gates it refuses it here.
        chroma: if config.chroma_444 {
            lowlat_encode::nvenc::Chroma::Yuv444
        } else {
            lowlat_encode::nvenc::Chroma::Yuv420
        },
        depth: if config.ten_bit {
            lowlat_encode::nvenc::Depth::Ten
        } else {
            lowlat_encode::nvenc::Depth::Eight
        },
    });
    let mut encoder = match encoder {
        Ok(encoder) => encoder,
        Err(error) => {
            lowlat_common::log_error!("stream: encoder could not be configured, {error}");
            return Exit::Failed(status::ENCODER_UNAVAILABLE);
        }
    };
    lowlat_common::log_info!(
        "stream: quality={:?}, quantiser floor {}",
        config.quality,
        config.quality.min_qp()
    );
    encode_loop(shared, arrivals, config, roster, &mut encoder, None)
}

impl FromDevice for lowlat_encode::nvenc::Encoder<'_> {
    fn submit_from_device(
        &mut self,
        registration: &crate::display::Registration,
        _force_keyframe: bool,
    ) -> bool {
        // No display hands a picture over on this platform yet, so no
        // registration exists to be refused.
        match *registration {}
    }
}

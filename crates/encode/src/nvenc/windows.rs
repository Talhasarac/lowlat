//! The encoder's device on Windows: a D3D11 device, its inputs textures of
//! that device, and completion reported by the encoder itself through an
//! event per picture.
//!
//! **The collect asks the encoder's own event.** The runtime runs
//! asynchronously here and sets an event as each picture finishes, so a poll
//! asks that event without sleeping and the lock behind it has nothing left to
//! wait for -- where on Linux the marker passes before the encoding engine is
//! done and the lock absorbs the rest.
//!
//! A frame from ordinary memory, the synthetic source's, is written through a
//! staging texture per slot and copied whole into the slot's texture: never
//! into one plane of it, which drops the colour on some drivers.

use std::sync::Arc;

use lowlat_drivers::d3d11::{Com, Device, Event};
use lowlat_drivers::vcall;

use super::{Api, Chroma, Config, Depth, Encoder, Error, IN_FLIGHT, Input, Session, widen};
use crate::ffi::d3d11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, DXGI_FORMAT,
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC, ID3D11Resource, ID3D11Texture2D,
};
use crate::ffi::nvenc::{
    NV_ENC_DEVICE_TYPE_DIRECTX, NV_ENC_EVENT_PARAMS, NV_ENC_INPUT_IMAGE,
    NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, NV_ENC_PIC_PARAMS,
    NV_ENC_REGISTER_RESOURCE, NV_ENC_SUCCESS, NVENCAPI_VERSION,
};
use crate::ffi::versions::{
    NV_ENC_EVENT_PARAMS_VER, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, NV_ENC_REGISTER_RESOURCE_VER,
};

/// How long a teardown waits for each picture still in the encoder before it
/// lets go of the picture's input anyway: many times one encode, and short
/// enough that a device that stopped answering does not hold up a rebuild.
const SETTLE: core::time::Duration = core::time::Duration::from_millis(100);

impl<'a> Api {
    /// Open a session on a D3D11 device.
    ///
    /// The device is shared rather than borrowed: an encoder outliving the
    /// device it was opened on is a use-after-free inside the driver, and the
    /// device is the capture's as well.
    pub fn open_session(&'a self, device: Arc<Device>) -> Result<Session<'a>, Error> {
        let open = self
            .functions
            .nvEncOpenEncodeSessionEx
            .ok_or(Error::MissingSymbol)?;

        // SAFETY: plain data; zero is every field unset.
        let mut params = unsafe { core::mem::zeroed::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>() };
        params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
        params.device = device.device().cast();
        params.apiVersion = NVENCAPI_VERSION;

        let mut encoder: *mut core::ffi::c_void = core::ptr::null_mut();
        // SAFETY: both pointers are to live storage for the duration of the
        // call and the block is stamped; the device is live while `device` is
        // held, which the session does from here on.
        let status = unsafe { open(&raw mut params, &raw mut encoder) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }

        Ok(Session {
            api: self,
            encoder,
            device,
        })
    }
}

impl<'a> Session<'a> {
    /// Configure the encoder for low latency and initialise it, each
    /// picture's completion reported through an event.
    pub fn initialize(self, config: Config) -> Result<Encoder<'a>, Error> {
        let (encode_config, init_params, outputs) = self.configure(&config, true)?;
        let completion = Completion::new()?;
        let mut encoder = Encoder {
            session: self,
            config,
            completion,
            encode_config,
            init_params,
            outputs,
            next_output: 0,
            inputs: Vec::with_capacity(IN_FLIGHT),
            pending: std::collections::VecDeque::with_capacity(IN_FLIGHT),
            collected: Vec::new(),
            last_lock: None,
        };
        // Told to the encoder once they belong to a value whose teardown takes
        // them back, so a failure part way leaves nothing registered.
        encoder.register_events()?;
        encoder.allocate_inputs()?;
        Ok(encoder)
    }
}

/// The events the encoder sets as each picture finishes, one per slot.
#[derive(Debug)]
pub(super) struct Completion {
    events: [Event; IN_FLIGHT],
    /// Which slots' pictures have been seen finished since they were queued.
    /// Asking an event takes its set, so a second poll of one picture -- after
    /// a lock that failed -- has to remember the first one's answer.
    seen: [bool; IN_FLIGHT],
    /// How many of the events the encoder has been told of, which a teardown
    /// takes back.
    registered: usize,
}

impl Completion {
    fn new() -> Result<Self, Error> {
        let event = || Event::new().map_err(Error::Device);
        Ok(Self {
            events: [event()?, event()?, event()?, event()?],
            seen: [false; IN_FLIGHT],
            registered: 0,
        })
    }
}

/// One slot of the input pool: the encoder's texture, registered, and the
/// staging texture a frame from ordinary memory is written through.
#[derive(Debug)]
pub(super) struct InputSlot {
    texture: Com<ID3D11Texture2D>,
    staging: Com<ID3D11Texture2D>,
    pub(super) input: Input,
}

impl Encoder<'_> {
    /// Tell the encoder of each slot's event, which it requires before a
    /// picture may name one.
    fn register_events(&mut self) -> Result<(), Error> {
        let register = self
            .functions()
            .nvEncRegisterAsyncEvent
            .ok_or(Error::MissingSymbol)?;
        while self.completion.registered < IN_FLIGHT {
            let event = &self.completion.events[self.completion.registered];
            // SAFETY: plain data; zero is every field unset.
            let mut params = unsafe { core::mem::zeroed::<NV_ENC_EVENT_PARAMS>() };
            params.version = NV_ENC_EVENT_PARAMS_VER;
            params.completionEvent = event.handle();
            // SAFETY: the encoder is live and the block is stamped and live
            // for the call; the event outlives its registration, which
            // `settle` takes back before the events close.
            let status = unsafe { register(self.session.encoder, &raw mut params) };
            if status != NV_ENC_SUCCESS {
                return Err(Error::Status(status));
            }
            self.completion.registered += 1;
        }
        Ok(())
    }

    /// One texture per in-flight picture at the session's size and depth,
    /// registered once, each with the staging texture a frame from ordinary
    /// memory is written through.
    fn allocate_inputs(&mut self) -> Result<(), Error> {
        let format = match (self.config.chroma, self.config.depth) {
            (Chroma::Yuv420, Depth::Eight) => DXGI_FORMAT_NV12,
            (Chroma::Yuv420, Depth::Ten) => DXGI_FORMAT_P010,
            // **Refused, not made.** Every frame offered here comes from
            // ordinary memory, which carries two planes, so a full-chroma
            // session could never be fed; the layout its textures take is the
            // conversion's to settle.
            (Chroma::Yuv444, _) => return Err(Error::ChromaMismatch),
        };
        let device = Arc::clone(&self.session.device);
        let texture = describe(format, self.config, D3D11_USAGE_DEFAULT);
        let staging = describe(format, self.config, D3D11_USAGE_STAGING);
        for _ in 0..IN_FLIGHT {
            let texture = device.texture(&texture).map_err(Error::Device)?;
            let staging = device.texture(&staging).map_err(Error::Device)?;
            let input = self.register(&texture)?;
            self.inputs.push(InputSlot {
                texture,
                staging,
                input,
            });
        }
        Ok(())
    }

    /// Register one of the session's textures as an input.
    ///
    /// **No pitch**: the runtime reads a texture's layout from the texture.
    fn register(&self, texture: &Com<ID3D11Texture2D>) -> Result<Input, Error> {
        let register = self
            .functions()
            .nvEncRegisterResource
            .ok_or(Error::MissingSymbol)?;
        // SAFETY: plain data; zero is every field unset.
        let mut params = unsafe { core::mem::zeroed::<NV_ENC_REGISTER_RESOURCE>() };
        params.version = NV_ENC_REGISTER_RESOURCE_VER;
        params.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        params.width = self.config.width;
        params.height = self.config.height;
        params.resourceToRegister = texture.as_ptr().cast();
        params.bufferFormat = self.config.chroma.buffer_format(self.config.depth);
        params.bufferUsage = NV_ENC_INPUT_IMAGE;
        // SAFETY: the encoder and the texture are live and the block is
        // stamped and live for the call; the texture outlives the
        // registration, which the teardown takes back first.
        let status = unsafe { register(self.session.encoder, &raw mut params) };
        if status != NV_ENC_SUCCESS {
            return Err(Error::Status(status));
        }
        Ok(Input {
            registered: params.registeredResource,
            width: self.config.width,
            height: self.config.height,
            pitch: params.pitch,
        })
    }

    /// Write a frame from ordinary memory into one slot: its staging texture
    /// mapped and written row by row, then copied whole into the slot's.
    ///
    /// The mapping lays a picture out as the texture holds it: the luma rows
    /// at the start, `RowPitch` apart, and the chroma rows directly after the
    /// last of them.
    pub(super) fn upload(
        &self,
        slot: usize,
        frame: &lowlat_capture::Frame<'_>,
    ) -> Result<(), Error> {
        // As on Linux: two planes cannot carry full chroma.
        if self.config.chroma == Chroma::Yuv444 {
            return Err(Error::ChromaMismatch);
        }
        let held = self.inputs.get(slot).ok_or(Error::NoContext)?;
        let width = usize::try_from(frame.width).unwrap_or(0);
        let rows = usize::try_from(frame.height).unwrap_or(0);
        let chroma_width = width.div_ceil(2) * 2;
        let chroma_rows = rows.div_ceil(2);
        let sample = self.config.depth.bytes_per_sample();
        // Widened as on Linux for a ten-bit session: each sample high in its
        // word, which is the texture's own layout too.
        let widened;
        let ((luma, luma_stride), (chroma, chroma_stride)) = if self.config.depth.ten_bit() {
            widened = (
                widen(&frame.luma, width, rows),
                widen(&frame.chroma, chroma_width, chroma_rows),
            );
            (
                (&widened.0[..], width * 2),
                (&widened.1[..], chroma_width * 2),
            )
        } else {
            (
                (frame.luma.bytes, frame.luma.stride),
                (frame.chroma.bytes, frame.chroma.stride),
            )
        };
        let height = usize::try_from(self.config.height).unwrap_or(0);

        let context = self.session.device.context();
        let staging = held.staging.as_ptr().cast::<ID3D11Resource>();
        // SAFETY: plain data the call fills.
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { core::mem::zeroed() };
        // SAFETY: the device's context, on the thread that drives this
        // encoder; a live staging texture the processor may write; a live
        // output.
        let hr = unsafe {
            vcall!(
                context,
                Map,
                staging,
                0,
                D3D11_MAP_WRITE,
                0,
                &raw mut mapped
            )
        }
        .ok_or(Error::MissingSymbol)?;
        if hr < 0 {
            return Err(Error::Device(lowlat_drivers::d3d11::Error::Status(hr)));
        }
        let pitch = usize::try_from(mapped.RowPitch).unwrap_or(0);
        let base = mapped.pData.cast::<u8>();
        // SAFETY: the mapping holds the texture's `height` luma rows and its
        // half as many chroma rows after them, `pitch` bytes each, and every
        // write stays inside one row and inside those rows. The texture
        // copied from is the one just unmapped, into the slot's own, which no
        // picture in flight is reading: the slot is reused only once its
        // picture has been collected.
        unsafe {
            write_rows(
                base,
                pitch,
                luma,
                luma_stride,
                width * sample,
                rows.min(height),
            );
            write_rows(
                base.add(pitch * height),
                pitch,
                chroma,
                chroma_stride,
                chroma_width * sample,
                chroma_rows.min(height / 2),
            );
            vcall!(context, Unmap, staging, 0);
            vcall!(
                context,
                CopyResource,
                held.texture.as_ptr().cast::<ID3D11Resource>(),
                staging
            );
        }
        Ok(())
    }

    /// Name the slot's event on the picture, for the encoder to set when the
    /// picture is finished.
    pub(super) fn arm(&mut self, slot: usize, pic: &mut NV_ENC_PIC_PARAMS) {
        self.completion.seen[slot] = false;
        pic.completionEvent = self.completion.events[slot].handle();
    }

    /// Nothing to record: the encoder sets the event itself.
    pub(super) fn mark(&self, _slot: usize) -> Result<(), Error> {
        Ok(())
    }

    /// Whether the slot's picture is finished: its event asked, never slept
    /// on.
    pub(super) fn passed(&mut self, slot: usize) -> Result<bool, Error> {
        let completion = &mut self.completion;
        if !completion.seen[slot] {
            completion.seen[slot] = completion.events[slot].try_wait();
        }
        Ok(completion.seen[slot])
    }

    /// Sleep on the oldest picture's event until the encoder sets it or
    /// `limit` passes; false only with nothing in flight. What the event said
    /// is kept for the collect, since asking it again would find it taken.
    pub(super) fn wait_finished(&mut self, limit: core::time::Duration) -> bool {
        let Some(slot) = self.pending.front().map(|entry| entry.event) else {
            return false;
        };
        let completion = &mut self.completion;
        if !completion.seen[slot] {
            completion.seen[slot] = completion.events[slot].wait(limit);
        }
        true
    }

    /// Before a teardown: each picture still in the encoder seen finished, or
    /// given up on after a bound, then the events taken back from the encoder.
    pub(super) fn settle(&mut self) {
        for entry in &self.pending {
            let slot = entry.event;
            if !self.completion.seen[slot] {
                self.completion.seen[slot] = self.completion.events[slot].wait(SETTLE);
            }
        }
        let Some(unregister) = self.functions().nvEncUnregisterAsyncEvent else {
            return;
        };
        for event in &self.completion.events[..self.completion.registered] {
            // SAFETY: plain data; zero is every field unset.
            let mut params = unsafe { core::mem::zeroed::<NV_ENC_EVENT_PARAMS>() };
            params.version = NV_ENC_EVENT_PARAMS_VER;
            params.completionEvent = event.handle();
            // SAFETY: the encoder is live and the event is one it was told of.
            unsafe { unregister(self.session.encoder, &raw mut params) };
        }
        self.completion.registered = 0;
    }
}

/// A texture of the session's size and format, as `usage` makes it: one the
/// device draws into and the encoder reads, or one the processor writes.
fn describe(format: DXGI_FORMAT, config: Config, usage: D3D11_USAGE) -> D3D11_TEXTURE2D_DESC {
    let (bind, cpu) = if usage == D3D11_USAGE_STAGING {
        (0, D3D11_CPU_ACCESS_WRITE)
    } else {
        (D3D11_BIND_RENDER_TARGET, 0)
    };
    D3D11_TEXTURE2D_DESC {
        Width: config.width,
        Height: config.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: u32::try_from(bind).unwrap_or(0),
        CPUAccessFlags: u32::try_from(cpu).unwrap_or(0),
        MiscFlags: 0,
    }
}

/// Copy `rows` rows of `bytes` bytes each from `from`, `stride` apart, to
/// `to`, `pitch` apart. A row the source does not hold whole ends the copy.
///
/// # Safety
///
/// `to` is writable for `rows` rows of `pitch` bytes.
unsafe fn write_rows(
    to: *mut u8,
    pitch: usize,
    from: &[u8],
    stride: usize,
    bytes: usize,
    rows: usize,
) {
    let bytes = bytes.min(pitch);
    for row in 0..rows {
        let Some(source) = from.get(row * stride..row * stride + bytes) else {
            break;
        };
        // SAFETY: the caller's contract, and `bytes` is at most one row.
        unsafe { core::ptr::copy_nonoverlapping(source.as_ptr(), to.add(row * pitch), bytes) };
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use lowlat_drivers::d3d11::D3d11;

    use super::*;
    use crate::Poll;
    use crate::nvenc::Codec;

    /// A device on the first adapter of the vendor's that renders, as the
    /// stream opens one.
    fn device() -> Arc<Device> {
        let d3d11 = D3d11::load().expect("the system's libraries");
        let adapter = d3d11
            .adapters()
            .expect("the walk")
            .into_iter()
            .find(|a| a.decodes_here() && a.vendor == 0x10de)
            .expect("an adapter of the encoder's vendor");
        println!("on {} {:?}", adapter.luid, adapter.description);
        Arc::new(d3d11.open(adapter.luid).expect("a device"))
    }

    fn config(codec: Codec) -> Config {
        Config {
            codec,
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 20_000_000,
            min_qp: crate::DEFAULT_MIN_QP,
            thorough: false,
            chroma: Chroma::Yuv420,
            depth: Depth::Eight,
        }
    }

    /// **A session on a graphics device reports completion by event**, which
    /// is what the collect here is built on, and changes its rate live, which
    /// is the congestion controller's one actuator.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn a_session_on_a_graphics_device_reports_completion_by_event() {
        let api = Api::load().expect("encoder runtime");
        println!(
            "driver interface {}, built against {}",
            api.driver_version(),
            super::super::Version::COMPILED
        );
        let session = api.open_session(device()).expect("session");
        for codec in [Codec::H264, Codec::H265] {
            let caps = session.caps(codec).expect("caps");
            println!("{codec:?}: {caps:?}");
            assert!(caps.async_encode, "{codec:?}: no completion by event");
            assert!(caps.dynamic_bitrate, "{codec:?}: no live rate change");
        }
    }

    /// **The collect asks the encoder's event, never waiting.** A not-ready
    /// answer is a wait with no time in it, so it is bounded by a system call
    /// however much is queued behind it; a ready one is the lock, timed here
    /// against Linux's, which absorbs what its marker did not see.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_collect_does_not_block() {
        let api = Api::load().expect("encoder runtime");
        let session = api.open_session(device()).expect("session");
        let config = config(Codec::H264);
        let mut encoder = session.initialize(config).expect("initialize");
        let mut source = lowlat_capture::synthetic::Synthetic::new(config.width, config.height);

        // Fill the queue. The first is forced to a keyframe; the rest are not.
        for index in 0..IN_FLIGHT {
            encoder
                .submit(&source.acquire(), index == 0)
                .expect("submit");
        }
        assert_eq!(encoder.in_flight(), IN_FLIGHT);
        assert_eq!(
            encoder.submit(&source.acquire(), false).unwrap_err(),
            Error::QueueFull,
            "the queue accepted more than it holds"
        );

        let (mut slowest, mut slowest_pending) = (Duration::ZERO, Duration::ZERO);
        let (mut polls, mut pending, mut collected, mut keyframes) = (0usize, 0usize, 0, 0);
        let mut first_unit = 0u8;
        let drain_started = Instant::now();
        while collected < IN_FLIGHT {
            let started = Instant::now();
            let polled = encoder.poll().expect("poll");
            let took = started.elapsed();
            polls += 1;
            match polled {
                Poll::Ready {
                    bitstream,
                    keyframe,
                } => {
                    if collected == 0 {
                        assert_eq!(&bitstream[..4], &[0, 0, 0, 1], "no start code");
                        first_unit = bitstream[4] & 0x1F;
                    }
                    keyframes += usize::from(keyframe);
                    collected += 1;
                    slowest = slowest.max(took);
                }
                Poll::Pending => {
                    pending += 1;
                    slowest_pending = slowest_pending.max(took);
                    std::hint::spin_loop();
                }
            }
        }
        println!(
            "slowest collect {slowest:?}, drain of {IN_FLIGHT} {:?}, polls {polls} of which \
             {pending} pending (slowest pending {slowest_pending:?}), keyframes {keyframes}, \
             first unit type {first_unit}",
            drain_started.elapsed()
        );
        assert_eq!(first_unit, 7, "first unit is not a parameter set");
        assert_eq!(keyframes, 1, "expected exactly the forced keyframe");
        assert!(
            slowest_pending < Duration::from_micros(50),
            "a not-ready answer took {slowest_pending:?}, so the probe waited"
        );
        assert!(
            pending > polls / 2,
            "only {pending} of {polls} polls were not-ready, so the probe is not gating anything"
        );
        // **The lock behind a set event has nothing left to wait for.**
        // Measured 18 to 24 us over six runs at 1080p on the development
        // machine, where a picture of this burst takes about 0.15 ms: a lock
        // that waited on the engine again would take most of that.
        assert!(
            slowest < Duration::from_micros(100),
            "a ready collect took {slowest:?}, so the lock waited on the encode"
        );
    }

    /// **No keyframe crosses a rate change**, on this device as on Linux's: a
    /// rate that moves on every picture across the controller's range, changed
    /// with the queue full behind it as the loop changes it, and one refresh in
    /// the whole run, the forced first.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn no_keyframe_crosses_a_bitrate_change() {
        const PICTURES: usize = 120;
        let api = Api::load().expect("encoder runtime");
        let session = api.open_session(device()).expect("session");
        let config = config(Codec::H264);
        let mut encoder = session.initialize(config).expect("initialize");
        let mut source = lowlat_capture::synthetic::Synthetic::new(config.width, config.height);

        let mut stream = Vec::new();
        let (mut submitted, mut collected, mut reported) = (0usize, 0usize, 0usize);
        while collected < PICTURES {
            if submitted < PICTURES && submitted - collected < IN_FLIGHT {
                encoder
                    .submit(&source.acquire(), submitted == 0)
                    .expect("submit");
                submitted += 1;
            }
            let Poll::Ready {
                bitstream,
                keyframe,
            } = encoder.poll().expect("poll")
            else {
                std::hint::spin_loop();
                continue;
            };
            stream.extend_from_slice(bitstream);
            reported += usize::from(keyframe);
            // Through the trait, which is what the loop calls, with the rest of
            // the queue still in the encoder.
            let step = u32::try_from(collected % 20).unwrap_or(0);
            crate::Encoder::reconfigure(&mut encoder, 2_000_000 + step * 2_000_000)
                .expect("reconfigure");
            collected += 1;
        }
        // Escaping makes a three-byte start code impossible inside a payload,
        // so these counts are exact.
        let units = |kind: u8| {
            stream
                .windows(4)
                .filter(|w| w[..3] == [0, 0, 1] && w[3] & 0x1F == kind)
                .count()
        };
        let (refreshes, predicted) = (units(5), units(1));
        println!(
            "{} bytes, {} pictures, {refreshes} coded refreshes, {reported} reported",
            stream.len(),
            refreshes + predicted
        );
        assert_eq!(refreshes + predicted, PICTURES, "the wrong picture count");
        assert_eq!(refreshes, 1, "a refresh crossed a rate change");
        assert_eq!(reported, 1, "a keyframe was reported across a rate change");
    }

    /// What each preset the configuration may start from turns on that costs
    /// a pass: a second pass, a lookahead, adaptive quantisation. The fastest
    /// and the most thorough are the two the quality setting picks between;
    /// the middle is the one this backend used to start from at every setting.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_presets_the_configuration_starts_from() {
        use crate::ffi::guids::{
            NV_ENC_PRESET_P1_GUID, NV_ENC_PRESET_P4_GUID, NV_ENC_PRESET_P7_GUID,
        };
        use crate::ffi::nvenc as f;
        let api = Api::load().expect("encoder runtime");
        let session = api.open_session(device()).expect("session");
        let get = api
            .functions()
            .nvEncGetEncodePresetConfigEx
            .expect("the entry point");
        let presets = [
            ("P1", NV_ENC_PRESET_P1_GUID),
            ("P4", NV_ENC_PRESET_P4_GUID),
            ("P7", NV_ENC_PRESET_P7_GUID),
        ];
        for ((name, guid), codec) in presets
            .into_iter()
            .flat_map(|p| [(p, Codec::H264), (p, Codec::H265)])
        {
            // SAFETY: plain data; zero is every field unset.
            let mut preset = unsafe { core::mem::zeroed::<f::NV_ENC_PRESET_CONFIG>() };
            preset.version = crate::ffi::versions::NV_ENC_PRESET_CONFIG_VER;
            preset.presetCfg.version = crate::ffi::versions::NV_ENC_CONFIG_VER;
            // SAFETY: the encoder is live and the block is stamped.
            let status = unsafe {
                get(
                    session.encoder,
                    codec.guid(),
                    guid,
                    f::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &raw mut preset,
                )
            };
            assert_eq!(status, NV_ENC_SUCCESS);
            let rc = &preset.presetCfg.rcParams;
            println!(
                "{codec:?} {name} ultra-low-latency: multiPass {} lookahead {} depth {} aq {} \
                 temporal aq {} gop {} frameIntervalP {}",
                rc.multiPass,
                rc.enableLookahead(),
                rc.lookaheadDepth,
                rc.enableAQ(),
                rc.enableTemporalAQ(),
                preset.presetCfg.gopLength,
                preset.presetCfg.frameIntervalP
            );
        }
    }

    /// **How long a picture takes at the stream's own pace**: one every 16.7
    /// ms, the submit timed apart from the wait for its event, which is asked
    /// without pause so nothing here rounds it. `LOWLAT_WAIT_EVENT` sleeps on
    /// the event first, as the stream's loop does; `LOWLAT_POLL_SLEEP` polls
    /// between millisecond sleeps instead, as it used to.
    ///
    /// **Run it for a minute** (`LOWLAT_PICTURES=3600`, reported each 600 as
    /// the stream reports): within ten seconds of a load this light the driver
    /// drops the card's clocks -- memory to 810 MHz from 14 GHz -- and only
    /// the figure after that is the stream's. Measured at 1080p on the
    /// development machine after the drop, sleeping on the event: 3.4 ms with
    /// the fastest preset in one pass, where the middle preset's two passes,
    /// polled, took 7.6.
    ///
    /// `LOWLAT_NO_UPLOAD` times the encoder alone, each slot's texture encoded
    /// again as it stands.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn the_latency_at_sixty_pictures_a_second() {
        let pictures: usize = std::env::var("LOWLAT_PICTURES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        // `LOWLAT_SIZE=2560x1440` and `LOWLAT_FPS=120` load the card as a
        // larger desktop at a faster refresh would.
        let (width, height) = std::env::var("LOWLAT_SIZE")
            .ok()
            .and_then(|v| {
                let (w, h) = v.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or((1920, 1080));
        let fps: u32 = std::env::var("LOWLAT_FPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        let api = Api::load().expect("encoder runtime");
        let session = api.open_session(device()).expect("session");
        let config = Config {
            width,
            height,
            fps,
            bitrate_bps: 10_000_000,
            ..config(Codec::H264)
        };
        let mut encoder = session.initialize(config).expect("initialize");
        let mut source = lowlat_capture::synthetic::Synthetic::new(config.width, config.height);
        let period = Duration::from_secs(1) / fps;
        let (mut submits, mut waits) = (Vec::new(), Vec::new());
        let mut next = Instant::now();
        let at = |samples: &mut Vec<Duration>, percent: usize| {
            samples.sort_unstable();
            let index = (samples.len() - 1) * percent / 100;
            samples[index]
        };
        for index in 0..pictures {
            if index > 0 && index % 600 == 0 {
                let mut window = waits[index - 600..].to_vec();
                println!(
                    "  pictures {}..{index}: submit to collected p50 {:?} p99 {:?}",
                    index - 600,
                    at(&mut window, 50),
                    at(&mut window, 99)
                );
            }
            lowlat_common::clock::precise_sleep(next.saturating_duration_since(Instant::now()));
            next += period;
            let frame = source.acquire();
            let began = Instant::now();
            // `LOWLAT_NO_UPLOAD`: past the first round, each slot's texture is
            // encoded again as it stands, so the encoder alone is timed --
            // the way a picture already on the device would be handed over.
            if std::env::var_os("LOWLAT_NO_UPLOAD").is_some() && index >= IN_FLIGHT {
                let held = &encoder.inputs[encoder.next_output].input;
                let input = Input {
                    registered: held.registered,
                    width: held.width,
                    height: held.height,
                    pitch: held.pitch,
                };
                encoder.submit_registered(&input, false).expect("submit");
            } else {
                encoder.submit(&frame, index == 0).expect("submit");
            }
            let submitted = Instant::now();
            let nap = std::env::var_os("LOWLAT_POLL_SLEEP").is_some();
            // `LOWLAT_WAIT_EVENT`: sleep on the picture's own event first, as
            // the stream's loop does, which wakes when the encoder sets it
            // rather than at a poll.
            if std::env::var_os("LOWLAT_WAIT_EVENT").is_some() {
                crate::Encoder::wait_finished(&mut encoder, Duration::from_millis(100));
            }
            while !matches!(encoder.poll().expect("poll"), Poll::Ready { .. }) {
                if nap {
                    std::thread::sleep(Duration::from_millis(1));
                } else {
                    std::hint::spin_loop();
                }
            }
            submits.push(submitted - began);
            waits.push(submitted.elapsed());
        }
        println!(
            "submit p50 {:?} p99 {:?}; submit to collected p50 {:?} p95 {:?} p99 {:?}",
            at(&mut submits, 50),
            at(&mut submits, 99),
            at(&mut waits, 50),
            at(&mut waits, 95),
            at(&mut waits, 99)
        );
    }
}

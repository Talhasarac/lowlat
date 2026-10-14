//! The display on Windows: one output's desktop as the system duplicates it,
//! converted on the output's own adapter straight into the encoder's inputs,
//! and the pointer the system draws over it (docs/05-host.md sections 2, 3
//! and 8).
//!
//! **The duplication answers only when something changed**, where a display's
//! vblank answers every refresh, so the loop's present is mapped onto it: a
//! new image is a present that changed the picture; a wait past the frame's
//! deadline that sees no composition for a refresh is a present of the same
//! picture, so a still desktop keeps the frame rate's refine frames and the
//! pointer's cadence; and a frame that carries only the pointer is no present
//! at all, so the wait goes on and still lands on the next composition.
//!
//! **Everything here runs on the stream's own thread.** The duplication, the
//! desktop it reads and the coordinates the pointer is read in are that
//! thread's state ([`lowlat_capture::duplication`]), taken when the display
//! opens and given back when it goes.

use std::sync::Arc;

use lowlat_capture::d3d11::{Converter, Layout, Target};
use lowlat_capture::duplication::{
    self, Awake, Aware, Cursor, Duplication, Frame, InputDesktop, Shape,
};
use lowlat_capture::{Placement, Pointer, Reported};
use lowlat_common::clock::{self, Time};
use lowlat_core::video::Rotation;
use lowlat_drivers::d3d11::{Com, D3d11, Device, Fence, Output, Span, Timer};
use lowlat_drivers::ffi::d3d11::{
    DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE270, ID3D11Texture2D,
    IDXGIOutput,
};

use super::{Acquired, Capturable, Selectable};
use crate::timing::Stage;

/// How long after a refusal the duplication is asked for again, while the
/// desktop is out of reach: often enough that the lock screen's arrival
/// shows within a few frames, not so often that a desktop that stays out of
/// reach costs a call every frame.
const RETRY_MS: f64 = 50.0;

/// One refresh, where nothing says: a refresh is how long a wait past a
/// deadline goes before it is a present of the same picture.
const PERIOD_MS: f64 = 1000.0 / 60.0;

/// Pictures converted between one report of where the time went and the
/// next: ten seconds at sixty frames.
const REPORT_EVERY: u32 = 600;

/// The pointer's state must hold for longer than this, with a change pending,
/// before it is said, and is said once (docs/05-host.md section 8.3).
const SETTLE_MS: f64 = 18.0;

/// What went wrong.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Error {
    /// The system's graphics interface, or a call on the device.
    Device(lowlat_drivers::d3d11::Error),
    /// The duplication.
    Duplication(duplication::Error),
    /// No output by that name is part of the desktop, or it moved to
    /// another adapter.
    NoOutput,
    /// The encoder would not take a target.
    Register,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Device(error) => write!(f, "device: {error}"),
            Self::Duplication(error) => write!(f, "duplication: {error}"),
            Self::NoOutput => f.write_str("no such output is part of the desktop"),
            Self::Register => f.write_str("the encoder refused a target on its own device"),
        }
    }
}

impl std::error::Error for Error {}

impl From<lowlat_drivers::d3d11::Error> for Error {
    fn from(error: lowlat_drivers::d3d11::Error) -> Self {
        Self::Device(error)
    }
}

impl From<duplication::Error> for Error {
    fn from(error: duplication::Error) -> Self {
        Self::Duplication(error)
    }
}

/// What an encoder calls a converted frame it has been handed: one of its
/// inputs, registered once.
pub type Registration = lowlat_encode::nvenc::Input;

/// Whether `wanted` names `name`: the system's own name, its `\\.\` prefix
/// optional, in any case, as the system treats it.
fn named(wanted: &str, name: &str) -> bool {
    wanted
        .strip_prefix(r"\\.\")
        .unwrap_or(wanted)
        .eq_ignore_ascii_case(name)
}

/// Every output the system lists, read per-monitor aware so the rectangles
/// are physical pixels whatever the calling thread was.
fn listed() -> Vec<Output> {
    let _aware = Aware::per_monitor();
    D3d11::load()
        .and_then(|d3d11| d3d11.outputs())
        .unwrap_or_default()
}

/// The size of the picture an output's duplication hands over: the panel's,
/// so on a quarter turn the desktop's sides the other way round.
fn picture_of(output: &Output) -> (u32, u32) {
    let width = u32::try_from(output.right.saturating_sub(output.left)).unwrap_or(0);
    let height = u32::try_from(output.bottom.saturating_sub(output.top)).unwrap_or(0);
    if matches!(
        output.rotation,
        DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270
    ) {
        (height, width)
    } else {
        (width, height)
    }
}

/// The turn the system reports, as the video header carries it: the same
/// one-based numbers, unspecified read as upright.
fn rotation_of(rotation: DXGI_MODE_ROTATION) -> Rotation {
    match u8::try_from(rotation).map(Rotation::from_bits) {
        Ok(Rotation::Unknown) | Err(_) => Rotation::None,
        Ok(known) => known,
    }
}

/// Where `output` sits in the desktop the attached outputs make together,
/// measured from that desktop's own corner: a display left of the primary
/// has a negative left edge, which the corner absorbs.
fn place_of(output: &Output, listed: &[Output]) -> Placement {
    let lit = || listed.iter().filter(|o| o.attached);
    let left = lit().map(|o| o.left).min().unwrap_or(output.left);
    let top = lit().map(|o| o.top).min().unwrap_or(output.top);
    let right = lit().map(|o| o.right).max().unwrap_or(output.right);
    let bottom = lit().map(|o| o.bottom).max().unwrap_or(output.bottom);
    let span = |from: i32, to: i32| u32::try_from(to.saturating_sub(from)).unwrap_or(0);
    Placement {
        x: span(left, output.left),
        y: span(top, output.top),
        width: span(output.left, output.right),
        height: span(output.top, output.bottom),
        desktop_width: span(left, right),
        desktop_height: span(top, bottom),
        rotation: rotation_of(output.rotation),
    }
}

/// The output at the desktop's corner, which on this system is the primary
/// display; the first attached one where none sits there.
fn primary(listed: &[Output]) -> Option<&Output> {
    listed
        .iter()
        .filter(|o| o.attached)
        .find(|o| o.left == 0 && o.top == 0)
        .or_else(|| listed.iter().find(|o| o.attached))
}

/// The output a request names, or with none the primary; only one that is
/// part of the desktop.
fn chosen<'a>(listed: &'a [Output], wanted: Option<&str>) -> Option<&'a Output> {
    match wanted {
        Some(wanted) => listed.iter().find(|o| o.attached && named(wanted, &o.name)),
        None => primary(listed),
    }
}

/// Presents a second, rounded to the nearest as a mode's rate is, or zero
/// where the system will not say.
fn hz_of(name: &str) -> u32 {
    duplication::refresh_of(name).map_or(0, |(numerator, denominator)| {
        (numerator + denominator / 2) / denominator.max(1)
    })
}

/// One refresh in milliseconds, from a rate the system gave as a ratio.
fn period_of((numerator, denominator): (u32, u32)) -> Option<f64> {
    (numerator > 0 && denominator > 0)
        .then(|| 1000.0 * f64::from(denominator) / f64::from(numerator))
}

/// What one look at the duplication found.
#[derive(Debug)]
enum Looked {
    /// A new image, kept for the tick.
    Image(Kept),
    /// A frame of the pointer alone, taken and given back.
    Pointer,
    /// Nothing in the time.
    Nothing,
}

/// A frame a wait kept for the tick: a new image, not yet converted.
#[derive(Debug)]
struct Kept {
    image: Com<ID3D11Texture2D>,
    /// When the system presented it, by the performance counter.
    presented: i64,
    /// When the duplication handed it over.
    at: Time,
}

/// The pointer's three bits and when they are said (docs/05-host.md section
/// 8): hidden from the duplication's own visibility of it, relative from
/// the show state or a picture of one value, suppressed from the show
/// state's touch flag; and one message once the whole state has held for
/// longer than [`SETTLE_MS`] with a change pending, never for a position
/// alone.
#[derive(Debug, Default)]
struct Settle {
    /// The state and what is pending, one word: the three bits as the wire
    /// carries them, whether the picture is one value, and below them the
    /// changes not yet said.
    word: u16,
    /// The word as last seen while a change is pending, and since when.
    last: Option<(u16, f64)>,
}

/// The duplication says the pointer is not over the picture.
const HIDDEN: u16 = 0x0200;
/// Something wants the pointer's motion.
const RELATIVE: u16 = 0x0100;
/// The system holds the pointer back while a touch or a pen drives.
const SUPPRESSED: u16 = 0x0800;
/// The picture is one value throughout.
const ONE_VALUE: u16 = 0x0400;
/// Pending: a new picture.
const PICTURE: u16 = 0x0002;
/// Pending: relative turned.
const RELATIVE_TURNED: u16 = 0x0004;
/// Pending: relative turned off.
const RELEASED: u16 = 0x0001;
/// Pending: hidden turned.
const HIDDEN_TURNED: u16 = 0x0008;
/// Everything pending.
const PENDING: u16 = 0x00FF;

impl Settle {
    /// Take one look into account: what the frames since the last said of
    /// the overlay and a new picture, and the show state now. Whether a
    /// message is owed, and if so whether it carries a new picture.
    ///
    /// **A change and its undoing inside the window say nothing**: each is
    /// a toggle of the bit and of its pending mark, so the pair cancels.
    fn look(
        &mut self,
        visible: Option<bool>,
        picture: Option<bool>,
        cursor: Cursor,
        now_ms: f64,
    ) -> Option<bool> {
        if let Some(visible) = visible
            && !visible != (self.word & HIDDEN != 0)
        {
            self.word ^= HIDDEN | HIDDEN_TURNED;
        }
        if let Some(one_value) = picture {
            self.word |= PICTURE;
            if one_value != (self.word & ONE_VALUE != 0) {
                self.word ^= ONE_VALUE;
            }
        }
        let relative = self.word & RELATIVE != 0;
        let turn = if self.word & ONE_VALUE != 0 {
            !relative
        } else {
            cursor.showing == relative
        };
        if turn {
            self.word = (self.word & !RELEASED) ^ (RELATIVE | RELATIVE_TURNED);
            if self.word & (RELATIVE | RELATIVE_TURNED) == RELATIVE_TURNED {
                self.word |= RELEASED;
            }
        }
        if cursor.suppressed {
            self.word |= SUPPRESSED;
        } else {
            self.word &= !SUPPRESSED;
        }
        if self.word & PENDING == 0 {
            self.last = None;
            return None;
        }
        let since = match self.last {
            Some((word, since)) if word == self.word => since,
            _ => {
                self.last = Some((self.word, now_ms));
                now_ms
            }
        };
        if now_ms - since <= SETTLE_MS {
            return None;
        }
        let fresh = self.word & PICTURE != 0;
        self.word &= !PENDING;
        self.last = None;
        Some(fresh)
    }

    fn hidden(&self) -> bool {
        self.word & HIDDEN != 0
    }

    fn relative(&self) -> bool {
        self.word & RELATIVE != 0
    }

    fn suppressed(&self) -> bool {
        self.word & SUPPRESSED != 0
    }
}

/// The pointer's picture and what the frames said of it since the last look.
#[derive(Debug)]
struct Watch {
    settle: Settle,
    /// The duplication's visibility of the pointer, from the last frame that
    /// said anything of it; nothing before one has.
    visible: Option<bool>,
    /// A picture not yet looked at, and whether it is one value.
    picture: Option<bool>,
    /// The shape as the system wrote it, its RGBA, and its PNG: allocated
    /// for the largest the wire carries, once.
    raw: Vec<u8>,
    rgba: Vec<u8>,
    png: Vec<u8>,
    used: usize,
    checksum: u32,
    width: u16,
    height: u16,
    hot_x: u16,
    hot_y: u16,
    /// Whether a shape the wire cannot carry has been said, once.
    refused: bool,
}

impl Watch {
    fn new() -> Self {
        let side = duplication::POINTER_LIMIT as usize;
        Self {
            settle: Settle::default(),
            visible: None,
            picture: None,
            raw: vec![0; side * side * 4],
            rgba: Vec::with_capacity(side * side * 4),
            png: vec![
                0;
                lowlat_core::png::upper_bound(
                    duplication::POINTER_LIMIT,
                    duplication::POINTER_LIMIT,
                )
            ],
            used: 0,
            checksum: 0,
            width: 0,
            height: 0,
            hot_x: 0,
            hot_y: 0,
            refused: false,
        }
    }

    /// Take a new shape the system wrote into `raw`: the picture it makes,
    /// named by its PNG, and the hotspot inside it. The same picture at the
    /// same hotspot is not new.
    fn adopt(&mut self, shape: &Shape) {
        let Some((width, height)) = duplication::pointer_rgba(shape, &self.raw, &mut self.rgba)
        else {
            if !self.refused {
                self.refused = true;
                lowlat_common::log_warn!(
                    "display: a pointer the wire cannot carry, w={} h={}, keeping the last",
                    shape.width,
                    shape.height
                );
            }
            return;
        };
        let stride = usize::try_from(width).unwrap_or(0) * 4;
        let Ok(used) = lowlat_core::png::encode(&self.rgba, width, height, stride, &mut self.png)
        else {
            return;
        };
        let checksum = lowlat_core::crc32::of(self.png.get(..used).unwrap_or_default());
        let hot = |at: i32, side: u32| {
            u16::try_from(at.clamp(0, i32::try_from(side).unwrap_or(1) - 1)).unwrap_or(0)
        };
        let (hot_x, hot_y) = (hot(shape.hot_x, width), hot(shape.hot_y, height));
        if (checksum, hot_x, hot_y) == (self.checksum, self.hot_x, self.hot_y) {
            return;
        }
        self.used = used;
        self.checksum = checksum;
        self.width = u16::try_from(width).unwrap_or(u16::MAX);
        self.height = u16::try_from(height).unwrap_or(u16::MAX);
        self.hot_x = hot_x;
        self.hot_y = hot_y;
        self.picture = Some(duplication::one_value(&self.rgba));
    }
}

/// Where the time goes between a composition and the encoder's input.
#[derive(Debug)]
struct Timing {
    /// The system's present to the duplication handing the image over.
    handed: Stage,
    /// The present to the conversion queued on the device.
    queued: Stage,
    /// The conversion on the device's own clock.
    converting: Stage,
    fence: Option<Fence>,
    timer: Option<Timer>,
    /// The last value the fence was asked to reach, and whether a span is
    /// closed and not yet read.
    signalled: u64,
    timing: bool,
    pictures: u32,
}

impl Timing {
    fn new(device: &Device) -> Self {
        Self {
            handed: Stage::new(),
            queued: Stage::new(),
            converting: Stage::new(),
            fence: device.fence(0).ok(),
            timer: device.timer().ok(),
            signalled: 0,
            timing: false,
            pictures: 0,
        }
    }

    /// Read the span last closed, once the device has passed it: whether the
    /// next conversion may be timed, since the timer holds one span.
    fn read(&mut self, device: &Device) -> bool {
        let Some(timer) = &self.timer else {
            return false;
        };
        if !self.timing {
            return true;
        }
        match device.span(timer) {
            Ok(Span::Pending) => return false,
            Ok(Span::Micros(us)) => self.converting.record(f64::from(us) / 1000.0),
            Ok(Span::Unsteady) | Err(_) => {}
        }
        self.timing = false;
        true
    }

    fn report(&mut self, name: &str) {
        self.pictures = self.pictures.wrapping_add(1);
        if self.pictures % REPORT_EVERY != 0 {
            return;
        }
        let (handed, queued, converting) = (
            self.handed.percentiles(),
            self.queued.percentiles(),
            self.converting.percentiles(),
        );
        lowlat_common::log_info!(
            "display: {name} composition to handed over ms p50/p95/p99 {:.3}/{:.3}/{:.3}, \
             to the conversion queued {:.3}/{:.3}/{:.3}, conversion on the device \
             {:.3}/{:.3}/{:.3}, pictures={}",
            handed.p50,
            handed.p95,
            handed.p99,
            queued.p50,
            queued.p95,
            queued.p99,
            converting.p50,
            converting.p95,
            converting.p99,
            self.pictures
        );
    }
}

/// The desktop, ready to hand pictures to an encoder.
///
/// **Fields drop in the order written**: the duplication first, so the frame
/// it holds goes back before anything it lent is let go, and the thread's own
/// state last, after everything made under it.
pub struct Display {
    duplication: Option<Duplication>,
    kept: Option<Kept>,
    converter: Converter,
    targets: Vec<Target>,
    registrations: Vec<Registration>,
    device: Arc<Device>,
    d3d11: D3d11,
    interface: Com<IDXGIOutput>,
    /// The output as it was when the display opened.
    output: Output,
    place: Placement,
    /// The picture size the targets and the encoder were built for.
    size: (u32, u32),
    period_ms: f64,
    /// Pictures converted, which names the next target.
    next: usize,
    /// When the wait for a present past the deadline began.
    waiting_since: Option<Time>,
    /// What the last acquire converted, for `converted` to hand back.
    flight: Option<Acquired>,
    /// When the duplication was last refused, while it is.
    refused_at: Option<Time>,
    last_refusal: Option<duplication::Error>,
    /// The duplication was lost to a switch of desktop, so the next is made
    /// on the input desktop.
    reattach: bool,
    resized: bool,
    watch: Watch,
    timing: Timing,
    /// What the pointer's settling is measured from.
    origin: Time,
    _awake: Awake,
    desktop: Option<InputDesktop>,
    _aware: Aware,
}

impl core::fmt::Debug for Display {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Display")
            .field("output", &self.output.name)
            .field("size", &self.size)
            .field("duplicated", &self.duplication.is_some())
            .finish_non_exhaustive()
    }
}

impl Display {
    /// Every output that is part of the desktop, by the name one is asked for
    /// by, at the size its picture is.
    pub fn outputs() -> Vec<Selectable> {
        let listed = listed();
        listed
            .iter()
            .filter(|o| o.attached)
            .map(|o| {
                let (width, height) = picture_of(o);
                Selectable {
                    id: o.name.clone(),
                    connector: o.name.clone(),
                    width,
                    height,
                    refresh_hz: hz_of(&o.name),
                    place: Some(place_of(o, &listed)),
                }
            })
            .collect()
    }

    /// Whether this process could capture right now. **Nothing lit** is no
    /// output that is part of the desktop. **Not reachable** is an output
    /// whose desktop this process cannot see: one outside the console's
    /// session, which a service and a remote session are.
    pub fn capturable() -> Capturable {
        if !listed().iter().any(|o| o.attached) {
            Capturable::NothingLit
        } else if !duplication::in_console_session() {
            Capturable::NotReachable
        } else {
            Capturable::Yes
        }
    }

    /// The output this host captures when it is asked for nothing: the one
    /// at the desktop's corner, from the same rule [`Display::open`] follows.
    pub fn preferred() -> Option<String> {
        primary(&listed()).map(|o| o.name.clone())
    }

    /// The picture the output asked for hands over, and how many times a
    /// second it presents, before anything is built on it.
    pub fn shape_of_display(wanted: Option<&str>) -> Option<(u32, u32, u32)> {
        let listed = listed();
        let output = chosen(&listed, wanted)?;
        let (width, height) = picture_of(output);
        Some((width, height, hz_of(&output.name)))
    }

    /// The output a request names, or the primary, with the adapter that
    /// scans it out: what decides the device everything is built on.
    pub fn locate(wanted: Option<&str>) -> Option<Output> {
        chosen(&listed(), wanted).cloned()
    }

    /// Prepare to capture the output named `name` on `device`, its own
    /// adapter's, into targets in `layout` for a `size` picture, each
    /// registered with `encoder` once.
    ///
    /// **A desktop out of reach is not a failure here.** The lock screen or
    /// an elevation prompt may be up, and a process that cannot reach it
    /// answers the duplication with a refusal that passes; the display opens
    /// without it and asks again as it goes, which is the loop's held path.
    pub fn open(
        device: &Arc<Device>,
        name: &str,
        layout: Layout,
        size: (u32, u32),
        encoder: &lowlat_encode::nvenc::Encoder<'_>,
    ) -> Result<Self, Error> {
        let aware = Aware::per_monitor();
        let d3d11 = D3d11::load()?;
        let listed = d3d11.outputs()?;
        let (interface, output) = d3d11
            .output(name)?
            .filter(|(_, o)| o.attached && o.adapter.luid == device.adapter.luid)
            .ok_or(Error::NoOutput)?;
        let converter = Converter::new(device, layout)?;
        let mut targets = Vec::with_capacity(lowlat_encode::nvenc::IN_FLIGHT);
        let mut registrations = Vec::with_capacity(lowlat_encode::nvenc::IN_FLIGHT);
        for _ in 0..lowlat_encode::nvenc::IN_FLIGHT {
            let target = Target::new(device, layout, size.0, size.1)?;
            registrations.push(
                encoder
                    .register_texture(target.texture())
                    .map_err(|_| Error::Register)?,
            );
            targets.push(target);
        }
        let mut display = Self {
            duplication: None,
            kept: None,
            converter,
            targets,
            registrations,
            device: Arc::clone(device),
            place: place_of(&output, &listed),
            interface,
            output,
            d3d11,
            size,
            period_ms: PERIOD_MS,
            next: 0,
            waiting_since: None,
            flight: None,
            refused_at: None,
            last_refusal: None,
            reattach: false,
            resized: false,
            watch: Watch::new(),
            timing: Timing::new(device),
            origin: Time::now(),
            _awake: Awake::hold(),
            desktop: None,
            _aware: aware,
        };
        match display.duplicate() {
            Ok(()) => {}
            Err(Error::Duplication(duplication::Error::Busy(_) | duplication::Error::Lost)) => {
                lowlat_common::log_info!(
                    "display: the desktop of {name} is out of reach for now, asking again as it goes"
                );
            }
            Err(error) => return Err(error),
        }
        lowlat_common::log_info!(
            "display: {name} {}x{} at {},{} of a {}x{} desktop, turned {:?}, {} target(s) of \
             {layout:?} on {}",
            size.0,
            size.1,
            display.place.x,
            display.place.y,
            display.place.desktop_width,
            display.place.desktop_height,
            display.place.rotation,
            display.targets.len(),
            display.device.adapter.description
        );
        Ok(display)
    }

    /// Make the duplication, where there is none: on the input desktop when
    /// the one this thread is on refuses, which is what a switch to the lock
    /// screen or an elevation prompt and back looks like. Asked at most once
    /// a [`RETRY_MS`] while it is refused.
    fn duplicate(&mut self) -> Result<(), Error> {
        if self.duplication.is_some() {
            return Ok(());
        }
        if let (Some(at), Some(refusal)) = (self.refused_at, self.last_refusal)
            && clock::elapsed_ms(at) < RETRY_MS
        {
            return Err(Error::Duplication(refusal));
        }
        // **A duplication lost to a switch is made on the input desktop**,
        // never on the one this thread was on: made there, it is lost again
        // at its first frame, for as long as the switch lasts.
        let lost = core::mem::replace(&mut self.reattach, false);
        if lost {
            match InputDesktop::attach() {
                Ok(desktop) => self.desktop = Some(desktop),
                Err(error) => {
                    lowlat_common::log_warn!(
                        "display: the input desktop could not be attached, {error}"
                    );
                }
            }
        }
        let made = match Duplication::new(&self.device, &self.interface) {
            Err(duplication::Error::Busy(_) | duplication::Error::Lost) => {
                InputDesktop::attach().and_then(|desktop| {
                    // The desktop this thread left is closed with the old
                    // handle: the thread is on the new one now.
                    self.desktop = Some(desktop);
                    Duplication::new(&self.device, &self.interface)
                })
            }
            other => other,
        };
        match made {
            Ok(made) => {
                let mode = made.mode();
                self.period_ms = period_of(mode.refresh).unwrap_or(PERIOD_MS);
                // **A picture of another size, or turned another way, is a
                // rebuild**: the targets and the encoder are built for one,
                // and the turn is what the place declares.
                if (mode.width, mode.height) != self.size || mode.rotation != self.output.rotation {
                    lowlat_common::log_info!(
                        "display: {} is {}x{} turned {} now, was {}x{} turned {}",
                        self.output.name,
                        mode.width,
                        mode.height,
                        mode.rotation,
                        self.size.0,
                        self.size.1,
                        self.output.rotation
                    );
                    self.resized = true;
                }
                // **Said once a switch, with the desktop it landed on**: a
                // switch the duplication follows at once is otherwise silent,
                // and the lock screen and the way back look like nothing.
                let refused = self.refused_at.take().is_some();
                if lost || refused {
                    let on = self
                        .desktop
                        .as_ref()
                        .map_or_else(|| "unattached".to_string(), InputDesktop::name);
                    lowlat_common::log_info!(
                        "display: {} duplicated again, desktop={on}",
                        self.output.name
                    );
                }
                self.last_refusal = None;
                self.converter.forget();
                self.duplication = Some(made);
                Ok(())
            }
            Err(refusal) => {
                self.refused_at = Some(Time::now());
                self.last_refusal = Some(refusal);
                Err(Error::Duplication(refusal))
            }
        }
    }

    /// The duplication answered with a failure: let it go, and with it what
    /// it lent, to be made again as it goes.
    fn lose(&mut self, error: duplication::Error) {
        if error == duplication::Error::Lost {
            self.reattach = true;
        } else {
            lowlat_common::log_warn!("display: {} duplication failed, {error}", self.output.name);
        }
        self.kept = None;
        self.duplication = None;
        self.converter.forget();
    }

    /// What one frame says of the pointer, taken while the frame is held: its
    /// visibility, and a new shape when it carries one.
    fn note_pointer(watch: &mut Watch, source: &Duplication, frame: &Frame) {
        let Some(seen) = frame.pointer else {
            return;
        };
        watch.visible = Some(seen.visible);
        if seen.shape_bytes > 0 {
            match source.shape(&mut watch.raw, seen.shape_bytes) {
                Ok(shape) => watch.adopt(&shape),
                Err(error) => {
                    lowlat_common::log_warn!(
                        "display: the pointer's shape could not be read, {error}"
                    );
                }
            }
        }
    }

    /// Where the captured output sits in the desktop.
    pub fn place(&self) -> Option<Placement> {
        Some(self.place)
    }

    /// What is being captured, by the name [`Display::outputs`] gives.
    pub fn selected(&self) -> Option<&str> {
        Some(&self.output.name)
    }

    /// The registration the last conversion wrote into.
    pub fn presented(&self) -> Option<&Registration> {
        let slot = self.next.checked_sub(1)? % self.registrations.len().max(1);
        self.registrations.get(slot)
    }

    /// Convert the image a wait kept, or one composed since, into the next
    /// target, and let the frame go at once: the conversion is queued before
    /// the release, which is the texture's own lock and so ordered on the
    /// device behind it. **Nothing new is not a failure**: the last picture
    /// converted stands, unchanged. A desktop out of reach is, and the loop
    /// holds the last picture while it is.
    pub fn acquire(&mut self) -> Result<Time, Error> {
        self.waiting_since = None;
        self.flight = None;
        self.duplicate()?;
        let kept = match self.kept.take() {
            Some(kept) => kept,
            // A composition since the wait gave up, asked for without
            // waiting.
            None => match self.look(0)? {
                Looked::Image(kept) => kept,
                Looked::Pointer | Looked::Nothing => return Ok(Time::now()),
            },
        };
        let slot = self.next % self.targets.len().max(1);
        let target = self.targets.get(slot).ok_or(Error::Register)?;
        let timed = self.timing.read(&self.device);
        if timed && let Some(timer) = &self.timing.timer {
            self.device.begin_span(timer);
        }
        let converted = self
            .converter
            .convert(&self.device, kept.image.as_ptr(), target);
        if timed
            && let (Some(fence), Some(timer)) = (&self.timing.fence, &self.timing.timer)
            && self
                .device
                .signal_timed(fence, self.timing.signalled + 1, timer)
                .is_ok()
        {
            self.timing.signalled += 1;
            self.timing.timing = true;
        }
        if let Some(duplication) = self.duplication.as_mut() {
            duplication.release();
        }
        converted?;
        self.next = self.next.wrapping_add(1);
        self.timing
            .queued
            .record(duplication::age_ms(kept.presented));
        self.timing.report(&self.output.name);
        self.flight = Some(Acquired {
            at: kept.at,
            changed: true,
        });
        Ok(kept.at)
    }

    /// The next frame within `timeout_ms`, kept if it carries an image; what
    /// it says of the pointer is taken either way, and a frame of the pointer
    /// alone goes back at once.
    fn look(&mut self, timeout_ms: u32) -> Result<Looked, Error> {
        let Some(source) = self.duplication.as_mut() else {
            return Ok(Looked::Nothing);
        };
        match source.next(timeout_ms) {
            Ok(Some(frame)) => {
                Self::note_pointer(&mut self.watch, source, &frame);
                match frame.image {
                    Some(image) => {
                        self.timing
                            .handed
                            .record(duplication::age_ms(frame.presented));
                        Ok(Looked::Image(Kept {
                            image,
                            presented: frame.presented,
                            at: Time::now(),
                        }))
                    }
                    None => {
                        source.release();
                        Ok(Looked::Pointer)
                    }
                }
            }
            Ok(None) => Ok(Looked::Nothing),
            Err(error) => {
                self.lose(error);
                Err(Error::Duplication(error))
            }
        }
    }

    /// What the last acquire converted, if it converted anything: nothing
    /// is a picture that did not change, which the loop reads as such.
    pub fn converted(&mut self) -> Result<Option<Acquired>, Error> {
        Ok(self.flight.take())
    }

    /// The pointer, when a message about it is owed: once its state has held
    /// long enough with a change pending (`Settle`), read on this thread's
    /// desktop. **Nothing outside the captured output**, and nothing is taken
    /// into account while it is there: a pointer on another screen is not
    /// this stream's to describe.
    pub fn pointer(&mut self) -> Option<Pointer> {
        let cursor = duplication::cursor()?;
        let output = &self.output;
        if cursor.x < output.left
            || cursor.x >= output.right
            || cursor.y < output.top
            || cursor.y >= output.bottom
        {
            return None;
        }
        let (x, y) = (cursor.x - output.left, cursor.y - output.top);
        let now_ms = clock::elapsed_ms(self.origin);
        let watch = &mut self.watch;
        let picture = watch.picture.take();
        let fresh = watch.settle.look(watch.visible, picture, cursor, now_ms)?;
        Some(Pointer {
            x: x - i32::from(watch.hot_x),
            y: y - i32::from(watch.hot_y),
            width: watch.width,
            height: watch.height,
            checksum: watch.checksum,
            fresh: fresh && watch.used > 0,
            looked: true,
            reported: Some(Reported {
                x: u16::try_from(x).unwrap_or(u16::MAX),
                y: u16::try_from(y).unwrap_or(u16::MAX),
                hot_x: watch.hot_x,
                hot_y: watch.hot_y,
                hidden: watch.settle.hidden(),
                relative: watch.settle.relative(),
                suppressed: watch.settle.suppressed(),
            }),
        })
    }

    /// Whether the output being captured is still part of the desktop, on
    /// the adapter it was opened on, and the device still there.
    pub fn attached(&self) -> bool {
        if self.device.lost() {
            return false;
        }
        match self.d3d11.output(&self.output.name) {
            Ok(Some((_, now))) => now.attached && now.adapter.luid == self.output.adapter.luid,
            Ok(None) => false,
            // A system that cannot answer is not evidence that the output
            // went, and ending a session on it would be worse.
            Err(_) => true,
        }
    }

    /// Nothing to arm: the duplication's wait is the present.
    pub fn arm_vblank(&self) -> Result<(), Error> {
        Ok(())
    }

    /// Wait up to `timeout_ms` for a present past the frame's deadline, which
    /// the loop asks about only once the deadline has passed.
    ///
    /// A new image is kept for the tick: true. A refresh past the deadline
    /// with nothing composed is a present of the same picture: true, and the
    /// tick converts nothing. A frame of the pointer alone is taken and given
    /// back, and the wait goes on to the end of the time: false if that was
    /// all. Nothing at all in the time: nothing. **A duplication lost or out
    /// of reach is a present**, so the tick runs and its acquire asks again.
    pub fn wait_present(&mut self, timeout_ms: i32) -> Option<bool> {
        if self.kept.is_some() || self.duplication.is_none() {
            return Some(true);
        }
        let began = *self.waiting_since.get_or_insert_with(Time::now);
        let called = Time::now();
        let mut moved = false;
        loop {
            let now = Time::now();
            let bound = self.period_ms - clock::diff_ms(began, now);
            if bound <= 0.0 {
                return Some(true);
            }
            let budget = f64::from(timeout_ms.max(1)) - clock::diff_ms(called, now);
            if budget <= 0.0 {
                return moved.then_some(false);
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive wait of at most a refresh or the loop's timeout"
            )]
            let wait = budget.min(bound).ceil().max(1.0) as u32;
            match self.look(wait) {
                Ok(Looked::Image(kept)) => {
                    self.kept = Some(kept);
                    return Some(true);
                }
                Ok(Looked::Pointer) => moved = true,
                Ok(Looked::Nothing) => {}
                Err(_) => return Some(true),
            }
        }
    }

    /// Nothing to wake: this device has no compute block that sleeps between
    /// frames the way an integrated one's does.
    pub fn poke(&mut self) -> Result<(), Error> {
        Ok(())
    }

    /// Whether the picture changed size or turn, clearing the answer.
    pub fn take_resize(&mut self) -> bool {
        core::mem::replace(&mut self.resized, false)
    }

    /// The picture the last message owed named, in the form the wire
    /// carries it.
    pub fn pointer_image(&self) -> &[u8] {
        self.watch.png.get(..self.watch.used).unwrap_or_default()
    }

    /// Nothing to turn: the system reports the pointer, its picture and its
    /// position in the desktop's own orientation.
    pub fn set_pointer_turn(&mut self, _turn: Rotation) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(showing: bool) -> Cursor {
        Cursor {
            x: 10,
            y: 10,
            showing,
            suppressed: false,
        }
    }

    /// **One message per settled change, and only after the whole state has
    /// held for longer than the window**: the first look at a new picture
    /// owes nothing yet, the state held past the window owes one carrying
    /// the picture, and nothing after it until something else changes.
    #[test]
    fn a_change_is_said_once_after_it_has_held() {
        let mut settle = Settle::default();
        assert_eq!(
            settle.look(Some(true), Some(false), cursor(true), 0.0),
            None
        );
        assert_eq!(settle.look(Some(true), None, cursor(true), SETTLE_MS), None);
        assert_eq!(
            settle.look(Some(true), None, cursor(true), SETTLE_MS + 1.0),
            Some(true),
            "a held change with a picture"
        );
        for at in [40.0, 80.0, 400.0] {
            assert_eq!(settle.look(Some(true), None, cursor(true), at), None);
        }
        assert!(!settle.hidden() && !settle.relative());
    }

    /// **A change and its undoing inside the window say nothing**, where a
    /// change that holds is said with the bit it set.
    #[test]
    fn a_flip_and_back_inside_the_window_says_nothing() {
        let mut settle = Settle::default();
        assert_eq!(settle.look(Some(false), None, cursor(true), 0.0), None);
        assert_eq!(settle.look(Some(true), None, cursor(true), 5.0), None);
        assert_eq!(settle.look(Some(true), None, cursor(true), 100.0), None);
        assert!(!settle.hidden());

        assert_eq!(settle.look(Some(false), None, cursor(true), 200.0), None);
        assert_eq!(
            settle.look(Some(false), None, cursor(true), 230.0),
            Some(false),
            "held, and no picture with it"
        );
        assert!(settle.hidden());
    }

    /// **Relative follows the show state, and a picture of one value forces
    /// it on** whatever the show state says; showing again and a picture
    /// with something to see turn it off.
    #[test]
    fn relative_follows_the_show_state_or_a_picture_of_one_value() {
        let mut settle = Settle::default();
        settle.look(Some(true), None, cursor(false), 0.0);
        assert_eq!(
            settle.look(Some(true), None, cursor(false), 20.0),
            Some(false)
        );
        assert!(settle.relative(), "a hidden pointer is relative");

        settle.look(Some(true), None, cursor(true), 40.0);
        settle.look(Some(true), None, cursor(true), 60.0);
        assert!(!settle.relative(), "shown again it is not");

        settle.look(Some(true), Some(true), cursor(true), 100.0);
        assert_eq!(
            settle.look(Some(true), None, cursor(true), 120.0),
            Some(true)
        );
        assert!(
            settle.relative(),
            "a blank picture is relative though shown"
        );

        settle.look(Some(true), Some(false), cursor(true), 200.0);
        settle.look(Some(true), None, cursor(true), 220.0);
        assert!(!settle.relative(), "a picture to see turns it off");
    }

    /// **Suppression rides with the next message and owes none of its own**,
    /// and a position alone owes nothing either: neither is a change the rule
    /// marks.
    #[test]
    fn suppression_and_position_owe_no_message_of_their_own() {
        let mut settle = Settle::default();
        let mut touched = cursor(true);
        touched.suppressed = true;
        assert_eq!(settle.look(Some(true), None, touched, 0.0), None);
        assert_eq!(settle.look(Some(true), None, touched, 100.0), None);
        assert!(settle.suppressed());
        let mut moved = cursor(true);
        moved.x = 500;
        assert_eq!(settle.look(Some(true), None, moved, 200.0), None);
    }

    /// The name a request carries matches the system's own, prefixed or
    /// not, in any case, and nothing else.
    #[test]
    fn an_output_is_asked_for_by_the_systems_own_name() {
        assert!(named("DISPLAY1", "DISPLAY1"));
        assert!(named(r"\\.\DISPLAY1", "DISPLAY1"));
        assert!(named("display1", "DISPLAY1"));
        assert!(!named("DISPLAY12", "DISPLAY1"));
        assert!(!named("DISPLAY", "DISPLAY1"));
    }

    fn output(name: &str, left: i32, top: i32, width: i32, height: i32) -> Output {
        Output {
            name: name.to_string(),
            left,
            top,
            right: left + width,
            bottom: top + height,
            attached: true,
            rotation: lowlat_drivers::ffi::d3d11::DXGI_MODE_ROTATION_IDENTITY,
            adapter: lowlat_drivers::d3d11::Adapter {
                luid: lowlat_drivers::d3d11::Luid { high: 0, low: 1 },
                vendor: 0x10de,
                device: 0,
                subsystem: 0,
                revision: 0,
                description: String::new(),
                driver: None,
                renders: true,
                software: false,
                indirect: false,
                integrated: false,
            },
        }
    }

    /// **A place is measured from the desktop's own corner**, so a display
    /// left of the primary -- negative on the system's own axes -- moves the
    /// corner rather than going below zero; a turned output's picture is its
    /// desktop rectangle the other way round.
    #[test]
    fn a_place_is_measured_from_the_desktops_corner() {
        let left = output("DISPLAY2", -1536, 200, 1536, 864);
        let primary_one = output("DISPLAY1", 0, 0, 2560, 1440);
        let listed = [primary_one.clone(), left.clone()];
        let place = place_of(&primary_one, &listed);
        assert_eq!((place.x, place.y), (1536, 0));
        assert_eq!((place.desktop_width, place.desktop_height), (4096, 1440));
        let place = place_of(&left, &listed);
        assert_eq!(
            (place.x, place.y, place.width, place.height),
            (0, 200, 1536, 864)
        );
        assert_eq!(primary(&listed).map(|o| o.name.as_str()), Some("DISPLAY1"));

        let mut turned = output("DISPLAY3", 0, 0, 1080, 1920);
        turned.rotation = DXGI_MODE_ROTATION_ROTATE90;
        assert_eq!(picture_of(&turned), (1920, 1080));
        assert_eq!(rotation_of(turned.rotation), Rotation::Deg90);
        assert_eq!(rotation_of(0), Rotation::None, "unspecified is upright");
    }

    /// **The desktop as the loop drives it, without a network**: the primary
    /// output opened on its own adapter's device with the vendor's encoder,
    /// the first present taken at once, then the loop's own sequence -- wait
    /// for a present, acquire, collect the conversion, submit the last
    /// converted picture, collect the encode -- for `LOWLAT_TICKS` ticks
    /// (default 240) at the display's own pace. Every tick must present
    /// within a refresh or two of asking, a still desktop included; the first
    /// picture must be the desktop and every submit an access unit.
    /// `LOWLAT_LAYOUT` names the layout (nv12, p010, ayuv, y410). Needs the
    /// vendor's GPU and an interactive desktop: `cargo test -p lowlat-host
    /// display -- --ignored --nocapture`.
    #[test]
    #[ignore = "requires the vendor's GPU, a display and an interactive desktop"]
    fn the_desktop_converts_and_encodes_at_the_displays_pace() {
        use lowlat_encode::nvenc;
        // As the library and the daemon hold it: without it every wait here
        // ends at the system's 15.6 ms tick.
        let _timer = clock::TimerResolution::raise();
        let ticks: usize = std::env::var("LOWLAT_TICKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(240);
        let layout = match std::env::var("LOWLAT_LAYOUT").as_deref() {
            Ok("p010") => Layout::P010,
            Ok("ayuv") => Layout::Ayuv,
            Ok("y410") => Layout::Y410,
            _ => Layout::Nv12,
        };
        let output = Display::locate(None).expect("an output");
        let (width, height, hz) = Display::shape_of_display(Some(&output.name)).expect("a shape");
        println!(
            "{} {width}x{height} at {hz} Hz on {}",
            output.name, output.adapter.description
        );
        let d3d11 = D3d11::load().expect("the system's libraries");
        let device = Arc::new(d3d11.open(output.adapter.luid).expect("a device"));
        let api = nvenc::Api::load().expect("the encoder's runtime");
        let mut display: Display;
        let session = api.open_session(Arc::clone(&device)).expect("a session");
        let mut encoder = session
            .initialize(nvenc::Config {
                codec: nvenc::Codec::H265,
                width,
                height,
                fps: hz.max(1),
                bitrate_bps: 20_000_000,
                min_qp: lowlat_encode::DEFAULT_MIN_QP,
                thorough: false,
                chroma: if layout.full_chroma() {
                    nvenc::Chroma::Yuv444
                } else {
                    nvenc::Chroma::Yuv420
                },
                depth: if matches!(layout, Layout::P010 | Layout::Y410) {
                    nvenc::Depth::Ten
                } else {
                    nvenc::Depth::Eight
                },
            })
            .expect("an encoder");
        display = Display::open(&device, &output.name, layout, (width, height), &encoder)
            .expect("the display");

        let began = Time::now();
        let first = display.wait_present(150);
        println!(
            "first present {first:?} after {:.2} ms",
            clock::elapsed_ms(began)
        );
        assert_eq!(first, Some(true), "the first present did not come at once");
        let interval = 1000.0 / f64::from(hz.max(1));
        let (mut changed, mut unchanged, mut units, mut late) = (0, 0, 0, 0);
        let mut waits = Stage::new();
        let mut deadline = clock::elapsed_ms(began);
        for tick in 0..ticks {
            if tick > 0 {
                // The loop's own shape: sleep to the deadline, then wait for
                // the present.
                let until = deadline - clock::elapsed_ms(began);
                if until > 0.0 {
                    clock::precise_sleep(core::time::Duration::from_secs_f64(until / 1000.0));
                }
                let asked = Time::now();
                while display.wait_present(20) != Some(true) {}
                let waited = clock::elapsed_ms(asked);
                waits.record(waited);
                if waited > 2.0 * interval + 2.0 {
                    late += 1;
                }
            }
            deadline += interval;
            display.acquire().expect("an acquire");
            match display.converted().expect("the conversion") {
                Some(got) if got.changed => changed += 1,
                _ => unchanged += 1,
            }
            if let Some(input) = display.presented() {
                encoder
                    .submit_registered(input, tick == 0)
                    .expect("a submit");
                loop {
                    match encoder.poll().expect("a collect") {
                        lowlat_encode::Poll::Ready { bitstream, .. } => {
                            assert!(!bitstream.is_empty());
                            units += 1;
                            break;
                        }
                        lowlat_encode::Poll::Pending => std::hint::spin_loop(),
                    }
                }
            }
            if let Some(pointer) = display.pointer() {
                println!("  pointer message at tick {tick}: {:?}", pointer.reported);
            }
        }
        let waited = waits.percentiles();
        println!(
            "{ticks} ticks in {:.0} ms: {changed} changed, {unchanged} the same, {units} units; \
             wait for a present ms p50/p95/p99 {:.2}/{:.2}/{:.2}, {late} late",
            clock::elapsed_ms(began),
            waited.p50,
            waited.p95,
            waited.p99
        );
        let (handed, queued) = (
            display.timing.handed.percentiles(),
            display.timing.queued.percentiles(),
        );
        println!(
            "composition to handed over ms p50/p95/p99 {:.3}/{:.3}/{:.3}; to the conversion \
             queued {:.3}/{:.3}/{:.3}",
            handed.p50, handed.p95, handed.p99, queued.p50, queued.p95, queued.p99
        );
        assert!(changed >= 1, "the desktop was never converted");
        assert_eq!(units, ticks, "a tick went unencoded");
        assert_eq!(late, 0, "a present took longer than two refreshes");
    }
}

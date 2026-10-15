//! The session's sound server, through its PulseAudio interface.
//!
//! **What a device name means here.** None follows the default output, which is
//! resolved once at connect and does **not** follow it afterwards on its own:
//! this loop is told when the server's own state changes and reconnects. A
//! named device is checked by reading back what the connection landed on,
//! because a name that does not resolve is **substituted rather than refused**,
//! so there is nothing to detect in what the call returns.

use std::ffi::{CStr, CString, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::{Frames, Output, Settings, Wanted};
use crate::pulse::{self, Pulse};
use crate::{Error, FRAME_BYTES};

/// The device name that means "whichever output is the default one". The
/// server resolves it, which is why finding the monitor costs no enumeration.
const DEFAULT_MONITOR: &CStr = c"@DEFAULT_MONITOR@";

/// How long the loop may sleep before it looks at its flags again, and
/// therefore the worst case for noticing a stop.
const ITERATE_US: c_int = 100_000;

/// How long to wait for a connection or a stream before giving up.
const READY_MS: f64 = 5_000.0;

/// How long the restore of somebody's speakers may take.
///
/// **Shorter, because it runs while everything is stopping.** A sound server
/// that has itself gone away would otherwise hold a teardown for the full wait
/// above, twice, to undo something that no longer exists.
const RESTORE_MS: f64 = 1_000.0;

/// **Resolved once for the process and never unloaded.**
///
/// The interface is reached from callbacks it invokes and from the drop of
/// values it created, neither of which can be handed a reference. A process-
/// wide table costs one indirection on a path that runs fifty times a second,
/// and keeping the library mapped for the life of the process is the point
/// rather than a cost: unmapping it while one of its own callbacks might still
/// run is the failure this avoids.
static PULSE: OnceLock<Pulse> = OnceLock::new();

fn pulse() -> Option<&'static Pulse> {
    PULSE.get()
}

/// Load the sound server's client library, once for the process.
pub(super) fn prepare() -> Result<(), Error> {
    if PULSE.get().is_none() {
        // A second thread racing this one loses and drops its copy, which
        // costs a load and nothing else.
        let _ = PULSE.set(Pulse::load()?);
    }
    Ok(())
}

/// How an owner asks the loop to stop: a flag the loop reads between
/// iterations, so a stop is noticed within one of them.
#[derive(Debug, Default)]
pub(super) struct Stop(AtomicBool);

impl Stop {
    pub(super) fn new() -> Result<Self, Error> {
        Ok(Self::default())
    }

    pub(super) fn request(&self) {
        self.0.store(true, Ordering::Release);
    }
}

impl core::ops::Deref for Stop {
    type Target = AtomicBool;

    fn deref(&self) -> &AtomicBool {
        &self.0
    }
}

/// Everything the loop and its callbacks share.
///
/// **Reached only through the raw pointer given to the interface**, including
/// by the loop itself. Holding a reference to the box as well would invalidate
/// that pointer the moment it was used.
struct State<S> {
    frames: Frames<S>,
    /// The stream was moved to another device by somebody else.
    moved: bool,
    /// The server's own state changed, which is how a new default output
    /// arrives.
    server_changed: bool,
}

pub(super) fn run<S>(
    config: &Settings,
    stop: &Stop,
    device: &Mutex<String>,
    report: &std::sync::mpsc::Sender<Result<(), Error>>,
    sink: S,
) where
    S: FnMut(&[u8]),
{
    let stop: &AtomicBool = stop;
    let state = Box::into_raw(Box::new(State {
        frames: Frames::new(sink),
        moved: false,
        server_changed: false,
    }));
    let opaque = state.cast::<c_void>();
    let outcome = serve::<S>(config, stop, device, report, opaque);
    if let Err(error) = outcome {
        lowlat_common::log_warn!("audio: capture ended, {error}");
    }
    // SAFETY: the box was leaked above and nothing else took ownership; every
    // callback that could reach it has been cleared by the stream's drop.
    drop(unsafe { Box::from_raw(state) });
}

/// The loop, from connecting to the last frame.
fn serve<S>(
    config: &Settings,
    stop: &AtomicBool,
    device: &Mutex<String>,
    report: &std::sync::mpsc::Sender<Result<(), Error>>,
    opaque: *mut c_void,
) -> Result<(), Error>
where
    S: FnMut(&[u8]),
{
    let session = match Session::open::<S>(config, opaque).and_then(|session| {
        session.wait_ready(stop)?;
        Ok(session)
    }) {
        Ok(session) => session,
        Err(error) => {
            let _ = report.send(Err(error));
            return Err(error);
        }
    };
    let mut stream = match Stream::open::<S>(&session, config, opaque, stop) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = report.send(Err(error));
            return Err(error);
        }
    };
    publish(&stream, device);
    let _ = report.send(Ok(()));
    let mut muted = if config.live().mute_local {
        mute_local(&session, &stream.name(), stop)
    } else {
        LocalMute::default()
    };

    // **However this loop leaves, the speakers come back.** A return on an
    // error path that skipped the restore would silence somebody's machine
    // until they noticed and fixed it by hand.
    let outcome = serve_loop::<S>(
        &session,
        &mut stream,
        config,
        stop,
        device,
        opaque,
        &mut muted,
    );
    restore_local(&session, &muted);
    outcome
}

/// The loop proper, so that whatever it returns the speakers are restored.
fn serve_loop<S>(
    session: &Session,
    stream: &mut Stream,
    config: &Settings,
    stop: &AtomicBool,
    device: &Mutex<String>,
    opaque: *mut c_void,
    muted: &mut LocalMute,
) -> Result<(), Error>
where
    S: FnMut(&[u8]),
{
    let mut seen = config.wanted.generation();
    while !stop.load(Ordering::Acquire) {
        session.iterate();
        if !stream.ready() {
            return Err(Error::Read(session.errno()));
        }

        // **What an owner asked for, acted on between iterations.** The
        // connection belongs to this thread, so a change is a request rather
        // than something another thread may apply.
        let now = config.wanted.generation();
        if now != seen {
            seen = now;
            let live = config.live();
            let before = stream.name();
            // The device is compared by what the stream landed on, not by what
            // was asked for: following the default output means those differ.
            let moving = live
                .device
                .as_deref()
                .filter(|name| !name.is_empty())
                .is_some_and(|name| name != before);
            if moving {
                restore_local(session, muted);
                // **A device that will not open keeps the one that is
                // working.** The name may have gone away between the
                // enumeration and the request, and ending the capture over it
                // would take the sound from every guest to honour a change
                // that failed.
                match Stream::open::<S>(session, config, opaque, stop) {
                    Ok(fresh) => {
                        *stream = fresh;
                        publish(stream, device);
                        lowlat_common::log_info!("audio: now capturing {}", stream.name());
                    }
                    Err(error) => lowlat_common::log_warn!(
                        "audio: the device asked for would not open, keeping {}, error={error}",
                        stream.name()
                    ),
                }
                *muted = LocalMute::default();
            }
            match (live.mute_local, muted.applied) {
                (true, false) => *muted = mute_local(session, &stream.name(), stop),
                (false, true) => {
                    restore_local(session, muted);
                    *muted = LocalMute::default();
                }
                _ => {}
            }
        }

        // **Read the flags after dispatching, never during it.** A callback
        // holds the state while it runs, so nothing else may.
        // SAFETY: the pointer is the leaked state, alive until `run` returns,
        // and this thread is the only one that touches it.
        let state = unsafe { &mut *opaque.cast::<State<S>>() };
        let moved = core::mem::take(&mut state.moved);
        let changed = core::mem::take(&mut state.server_changed);

        if moved {
            publish(stream, device);
            lowlat_common::log_info!("audio: the stream was moved to {}", stream.name());
        }
        // A new default output only concerns a stream that is following one.
        if changed && config.wanted_device().is_none() {
            let before = stream.name();
            // **The speakers move with the device.** A host that followed the
            // default output and left the old sink muted would silence a
            // machine nobody is streaming from.
            restore_local(session, muted);
            // **The new default may not be ready yet**, and a stream that is
            // still delivering is worth more than the one that was asked for:
            // a failure here leaves the old device in place, and a device that
            // has genuinely gone stops delivering on its own, which is the
            // path that ends this loop and has the capture taken again.
            match Stream::open::<S>(session, config, opaque, stop) {
                Ok(fresh) => *stream = fresh,
                Err(error) => lowlat_common::log_warn!(
                    "audio: the new default output would not open, keeping {}, error={error}",
                    stream.name()
                ),
            }
            let after = stream.name();
            if before != after {
                lowlat_common::log_info!("audio: the default output is now {after}");
            }
            publish(stream, device);
            *muted = if config.live().mute_local {
                mute_local(session, &after, stop)
            } else {
                LocalMute::default()
            };
        }
    }
    Ok(())
}

/// Silence the speakers behind the device being captured.
///
/// **Only what this host did is remembered**, so only that is ever undone. A
/// device that is not a monitor has no speakers to silence and is left alone,
/// which is the right answer for a host told to capture a microphone.
fn mute_local(session: &Session, monitor: &str, stop: &AtomicBool) -> LocalMute {
    let stop = Some(stop);
    let mut state = LocalMute::default();
    let Ok(monitor) = CString::new(monitor) else {
        return state;
    };
    let Some(source) = session.describe(false, &monitor, stop, READY_MS) else {
        lowlat_common::log_warn!("audio: the source did not describe itself, speakers left alone");
        return state;
    };
    let Some(sink) = source.paired else {
        // Not a monitor: there is nothing at the desk playing what we hear.
        return state;
    };
    let Some(described) = session.describe(true, &sink, stop, READY_MS) else {
        return state;
    };
    if !described.mutes_itself {
        // **A device whose mute reaches the capture is left alone.** Where the
        // device mutes itself the tap is ahead of it, which is what this
        // feature rests on; where the server mutes the mix instead, the same
        // mix feeds the monitor, and silencing the speakers would silence
        // every guest. That is the opposite of what was asked for, so it is
        // refused rather than done.
        lowlat_common::log_warn!(
            "audio: the speakers cannot be silenced without silencing the guests, sink={}, left alone",
            sink.to_string_lossy()
        );
        state.sink = Some(sink);
        return state;
    }
    if described.mute {
        // **Already muted, by somebody who is not us.** Leave it, and leave the
        // record empty so that nothing is undone later.
        state.sink = Some(sink);
        return state;
    }
    if session.set_sink_mute(&sink, true, stop, READY_MS).is_some() {
        lowlat_common::log_info!(
            "audio: the speakers are silenced, sink={}",
            sink.to_string_lossy()
        );
        state.applied = true;
    }
    state.sink = Some(sink);
    state
}

/// Put the speakers back, if this host is what silenced them.
fn restore_local(session: &Session, state: &LocalMute) {
    if !state.applied {
        return;
    }
    let Some(sink) = state.sink.as_deref() else {
        return;
    };
    // **Still muted, and by us.** Somebody who unmuted during the session is
    // not re-muted, and one who muted their own speakers keeps them muted.
    if session
        .describe(true, sink, None, RESTORE_MS)
        .is_some_and(|described| described.mute)
        && session
            .set_sink_mute(sink, false, None, RESTORE_MS)
            .is_some()
    {
        lowlat_common::log_info!(
            "audio: the speakers are back, sink={}",
            sink.to_string_lossy()
        );
    }
}

/// Record which device the stream is on, for whoever asks.
fn publish(stream: &Stream, device: &Mutex<String>) {
    let name = stream.name();
    match device.lock() {
        Ok(mut held) => *held = name,
        Err(held) => *held.into_inner() = name,
    }
}

/// Ask the server one question and wait for the answer.
///
/// The interface answers asynchronously and this loop is the one that drives
/// it, so the wait is turns of that loop rather than a block: an operation
/// nobody iterates never completes.
struct Ask<T> {
    answer: Option<T>,
    done: bool,
    /// The name this asked about, so the callback can check that the structure
    /// it is reading is laid out where it thinks.
    asked: Option<CString>,
}

impl<T> Ask<T> {
    fn about(name: Option<&CStr>) -> Self {
        Self {
            answer: None,
            done: false,
            asked: name.map(CStr::to_owned),
        }
    }
}

/// The device description a query answered with, reduced to what is read.
struct Described {
    mute: bool,
    /// Whether this device applies its own mute, which is what decides
    /// whether muting it reaches a capture of its monitor.
    mutes_itself: bool,
    paired: Option<CString>,
    /// Whether the name at offset zero was the one asked for, which is the
    /// check that the transcribed layout is being read correctly.
    matched: bool,
}

/// The callback both device queries use.
///
/// **It checks the layout it is reading**, because a structure transcribed from
/// a header is right until the day it is not: the name it was asked for has to
/// come back at offset zero, and anything else means the other fields are being
/// read from the wrong place and must not be acted on.
unsafe extern "C" fn on_device(
    _context: *mut pulse::Context,
    info: *const pulse::DeviceInfo,
    end: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: the pointer is the `Ask` the query was issued with, which lives
    // on the stack of the call that is driving this loop.
    let ask = unsafe { &mut *opaque.cast::<Ask<Described>>() };
    if end != 0 || info.is_null() {
        ask.done = true;
        return;
    }
    // SAFETY: the interface guarantees the pointer for the call's duration.
    let info = unsafe { &*info };
    let paired = if info.paired_name.is_null() {
        None
    } else {
        // SAFETY: a non-null name from the interface is NUL terminated.
        Some(unsafe { CStr::from_ptr(info.paired_name) }.to_owned())
    };
    let name = if info.name.is_null() {
        None
    } else {
        // SAFETY: a non-null name from the interface is NUL terminated.
        Some(unsafe { CStr::from_ptr(info.name) }.to_owned())
    };
    ask.answer = Some(Described {
        mute: info.mute != 0,
        mutes_itself: info.flags & pulse::DEVICE_MUTES_ITSELF != 0,
        paired,
        matched: name == ask.asked,
    });
}

/// The callback a setting reports through. Nothing is read from it; the answer
/// is that it finished.
unsafe extern "C" fn on_done(_context: *mut pulse::Context, _success: c_int, opaque: *mut c_void) {
    // SAFETY: as in `on_device`.
    let ask = unsafe { &mut *opaque.cast::<Ask<()>>() };
    ask.done = true;
}

/// Speakers this host silenced, and what to put back.
///
/// **Restore, never unmute.** Undoing more than was done switches on the
/// speakers of somebody who muted them for their own reasons, in their absence,
/// with the sound as the first they know of it.
#[derive(Debug, Default)]
struct LocalMute {
    /// The sink behind the monitor being captured, when there is one.
    sink: Option<CString>,
    /// Whether this host is the one that muted it.
    applied: bool,
}

/// The loop and the connection to the server.
struct Session {
    mainloop: *mut pulse::MainLoop,
    context: *mut pulse::Context,
}

impl Session {
    fn open<S: FnMut(&[u8])>(config: &Settings, opaque: *mut c_void) -> Result<Self, Error> {
        let pulse = pulse().ok_or(Error::Unavailable)?;
        let server = config
            .server
            .as_ref()
            .filter(|named| !named.is_empty())
            .and_then(|named| CString::new(named.as_str()).ok());
        // SAFETY: the loop and the context are created here and freed in
        // `Drop`; the server string outlives the connect call, which copies it.
        unsafe {
            let mainloop = (pulse.mainloop_new)();
            if mainloop.is_null() {
                return Err(Error::Unavailable);
            }
            let api = (pulse.mainloop_get_api)(mainloop);
            let context = (pulse.context_new)(api, c"lowlat".as_ptr());
            if context.is_null() {
                (pulse.mainloop_free)(mainloop);
                return Err(Error::Unavailable);
            }
            (pulse.context_set_subscribe_callback)(context, Some(on_subscribe::<S>), opaque);
            let named = server.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
            if (pulse.context_connect)(context, named, pulse::CONTEXT_NOAUTOSPAWN, std::ptr::null())
                < 0
            {
                let code = (pulse.context_errno)(context);
                (pulse.context_unref)(context);
                (pulse.mainloop_free)(mainloop);
                return Err(Error::Refused(code));
            }
            Ok(Self { mainloop, context })
        }
    }

    /// Run the loop until the connection is up, fails, or the wait is over.
    fn wait_ready(&self, stop: &AtomicBool) -> Result<(), Error> {
        let pulse = pulse().ok_or(Error::Unavailable)?;
        let began = lowlat_common::clock::Time::now();
        loop {
            // SAFETY: the context is live until this value is dropped.
            let state = unsafe { (pulse.context_get_state)(self.context) };
            if state == pulse::CONTEXT_READY {
                self.subscribe();
                return Ok(());
            }
            if state == pulse::CONTEXT_FAILED || state == pulse::CONTEXT_TERMINATED {
                // SAFETY: as above.
                return Err(Error::Refused(unsafe {
                    (pulse.context_errno)(self.context)
                }));
            }
            if stop.load(Ordering::Acquire) || lowlat_common::clock::elapsed_ms(began) > READY_MS {
                return Err(Error::Refused(0));
            }
            self.iterate();
        }
    }

    /// Ask to be told when the server's own state changes, which is where a new
    /// default output comes from.
    fn subscribe(&self) {
        let Some(pulse) = pulse() else {
            return;
        };
        // SAFETY: the context is ready. The operation is discarded rather than
        // waited on: nothing here depends on when the subscription takes
        // effect, only that it does.
        unsafe {
            let op = (pulse.context_subscribe)(
                self.context,
                pulse::SUBSCRIPTION_MASK_SERVER,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            if !op.is_null() {
                (pulse.operation_unref)(op);
            }
        }
    }

    /// The server's own code for the last failure.
    fn errno(&self) -> i32 {
        let Some(pulse) = pulse() else {
            return 0;
        };
        // SAFETY: the context is live until this value is dropped.
        unsafe { (pulse.context_errno)(self.context) }
    }

    /// Ask about a device by name, and run the loop until it answers.
    ///
    /// **The name it answers with is checked against the name asked for**,
    /// which is what makes reading a transcribed structure safe: a mismatch
    /// says the layout is wrong and the answer is discarded rather than acted
    /// on.
    fn describe(
        &self,
        sink: bool,
        name: &CStr,
        stop: Option<&AtomicBool>,
        within: f64,
    ) -> Option<Described> {
        let pulse = pulse()?;
        let mut ask = Ask::<Described>::about(Some(name));
        // **Reached only through the pointer, including from here.** A
        // reference taken to it afterwards would invalidate the one the
        // interface is writing through.
        let ask: *mut Ask<Described> = &raw mut ask;
        let query = if sink {
            pulse.sink_info_by_name
        } else {
            pulse.source_info_by_name
        };
        // SAFETY: the context is ready, the name outlives the call, and the
        // `Ask` outlives every callback because this function waits for it.
        unsafe {
            let op = query(
                self.context,
                name.as_ptr(),
                Some(on_device),
                ask.cast::<c_void>(),
            );
            if op.is_null() {
                return None;
            }
            (pulse.operation_unref)(op);
        }
        self.settle(ask, stop, within)?;
        // SAFETY: the query has finished, so nothing else holds it; it lives
        // on this stack frame until the function returns.
        let answer = unsafe { (*ask).answer.take() };
        let wanted = answer.filter(|described| described.matched);
        if wanted.is_none() {
            lowlat_common::log_warn!(
                "audio: {} did not describe itself as asked",
                name.to_string_lossy()
            );
        }
        wanted
    }

    /// Mute or unmute a sink by name, and wait for it to take effect.
    fn set_sink_mute(
        &self,
        sink: &CStr,
        mute: bool,
        stop: Option<&AtomicBool>,
        within: f64,
    ) -> Option<()> {
        let pulse = pulse()?;
        let mut ask = Ask::<()>::about(None);
        let ask: *mut Ask<()> = &raw mut ask;
        // SAFETY: as in `describe`.
        unsafe {
            let op = (pulse.set_sink_mute_by_name)(
                self.context,
                sink.as_ptr(),
                c_int::from(mute),
                Some(on_done),
                ask.cast::<c_void>(),
            );
            if op.is_null() {
                return None;
            }
            (pulse.operation_unref)(op);
        }
        self.settle(ask, stop, within)
    }

    /// Turn the loop until the answer arrives, or give up.
    ///
    /// **The flag is written by a callback the interface runs**, which is why
    /// it is read through the pointer rather than a reference and why nothing
    /// here can see it change.
    fn settle<T>(&self, ask: *mut Ask<T>, stop: Option<&AtomicBool>, within: f64) -> Option<()> {
        let began = lowlat_common::clock::Time::now();
        loop {
            // SAFETY: the caller owns the value this points at for the whole
            // of this call, and only this thread touches it -- the callbacks
            // run inside `iterate`, below, and never in parallel with it.
            if unsafe { (*ask).done } {
                return Some(());
            }
            // **A teardown is not a reason to abandon this.** Restoring what
            // this host changed runs while the loop is already stopping, so it
            // passes no flag and is bounded only by time; a wait that gave up
            // on the stop flag would leave somebody's speakers muted, which is
            // exactly what it did before this line said so.
            let cancelled = stop.is_some_and(|stop| stop.load(Ordering::Acquire));
            if cancelled || lowlat_common::clock::elapsed_ms(began) > within {
                return None;
            }
            self.iterate();
        }
    }

    /// One turn of the loop, bounded so a stop is noticed even when the server
    /// has gone quiet.
    fn iterate(&self) {
        let Some(pulse) = pulse() else {
            return;
        };
        // SAFETY: the three calls are one iteration decomposed, and they are
        // only ever made from the thread that created the loop.
        unsafe {
            if (pulse.mainloop_prepare)(self.mainloop, ITERATE_US) < 0 {
                return;
            }
            if (pulse.mainloop_poll)(self.mainloop) < 0 {
                return;
            }
            (pulse.mainloop_dispatch)(self.mainloop);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let Some(pulse) = pulse() else {
            return;
        };
        // SAFETY: both handles were created in `open` and are dropped once.
        unsafe {
            (pulse.context_disconnect)(self.context);
            (pulse.context_unref)(self.context);
            (pulse.mainloop_free)(self.mainloop);
        }
    }
}

/// The recording stream itself.
struct Stream {
    stream: *mut pulse::Stream,
}

impl Stream {
    fn open<S: FnMut(&[u8])>(
        session: &Session,
        config: &Settings,
        opaque: *mut c_void,
        stop: &AtomicBool,
    ) -> Result<Self, Error> {
        let pulse = pulse().ok_or(Error::Unavailable)?;
        let spec = pulse::SampleSpec {
            format: pulse::SAMPLE_S16LE,
            rate: crate::SAMPLE_RATE,
            channels: u8::try_from(crate::CHANNELS).unwrap_or(2),
        };
        let attr = pulse::BufferAttr::fragments_of(u32::try_from(FRAME_BYTES).unwrap_or(u32::MAX));
        let asked = config
            .wanted_device()
            .and_then(|name| CString::new(name).ok());
        let device = asked.as_deref().unwrap_or(DEFAULT_MONITOR);

        // SAFETY: the specification and the attributes outlive the calls, which
        // copy them; a null channel map asks for the default layout of the
        // channel count.
        let stream = unsafe {
            let stream = (pulse.stream_new)(
                session.context,
                c"desktop".as_ptr(),
                &spec,
                std::ptr::null(),
            );
            if stream.is_null() {
                return Err(Error::Refused((pulse.context_errno)(session.context)));
            }
            let stream = Self { stream };
            (pulse.stream_set_read_callback)(stream.stream, Some(on_read::<S>), opaque);
            (pulse.stream_set_moved_callback)(stream.stream, Some(on_moved::<S>), opaque);
            if (pulse.stream_connect_record)(
                stream.stream,
                device.as_ptr(),
                &attr,
                pulse::STREAM_ADJUST_LATENCY,
            ) < 0
            {
                return Err(Error::Refused((pulse.context_errno)(session.context)));
            }
            stream
        };

        let began = lowlat_common::clock::Time::now();
        loop {
            // SAFETY: the stream is live until it is dropped.
            let state = unsafe { (pulse.stream_get_state)(stream.stream) };
            if state == pulse::STREAM_READY {
                break;
            }
            if state == pulse::STREAM_FAILED || state == pulse::STREAM_TERMINATED {
                // SAFETY: as above.
                return Err(Error::Refused(unsafe {
                    (pulse.context_errno)(session.context)
                }));
            }
            if stop.load(Ordering::Acquire) || lowlat_common::clock::elapsed_ms(began) > READY_MS {
                return Err(Error::Refused(0));
            }
            session.iterate();
        }

        // **A name that does not resolve is substituted, not refused**, so what
        // is checked is where the stream landed rather than what the call
        // returned.
        if let Some(asked) = asked.as_deref() {
            let got = stream.name();
            if got.as_bytes() != asked.to_bytes() {
                lowlat_common::log_warn!(
                    "audio: asked for {} and the server offered {got}",
                    asked.to_string_lossy()
                );
                return Err(Error::NoSuchDevice);
            }
        }
        Ok(stream)
    }

    /// Whether the stream is still delivering.
    ///
    /// **Checked every pass rather than trusted.** A server that goes away
    /// leaves a stream that reads nothing for ever, which is indistinguishable
    /// from a desktop in silence and is the one failure a caller most needs
    /// told about.
    fn ready(&self) -> bool {
        let Some(pulse) = pulse() else {
            return false;
        };
        // SAFETY: the stream is live until it is dropped.
        unsafe { (pulse.stream_get_state)(self.stream) == pulse::STREAM_READY }
    }

    /// The device this stream is on, as the server names it.
    fn name(&self) -> String {
        let Some(pulse) = pulse() else {
            return String::new();
        };
        // SAFETY: the stream is live, and the string belongs to the interface
        // and is copied here rather than kept.
        unsafe {
            let name = (pulse.stream_get_device_name)(self.stream);
            if name.is_null() {
                return String::new();
            }
            CStr::from_ptr(name).to_string_lossy().into_owned()
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let Some(pulse) = pulse() else {
            return;
        };
        // SAFETY: created in `open`, dropped once. **The callbacks are cleared
        // first**, so nothing can be delivered into a state that is going away.
        unsafe {
            (pulse.stream_set_read_callback)(self.stream, None, std::ptr::null_mut());
            (pulse.stream_set_moved_callback)(self.stream, None, std::ptr::null_mut());
            (pulse.stream_disconnect)(self.stream);
            (pulse.stream_unref)(self.stream);
        }
    }
}

/// Fragments arrived. Take everything available and hand on whole frames.
unsafe extern "C" fn on_read<S: FnMut(&[u8])>(
    stream: *mut pulse::Stream,
    _bytes: usize,
    opaque: *mut c_void,
) {
    let Some(pulse) = pulse() else {
        return;
    };
    // SAFETY: the pointer is the leaked state, which outlives the stream, and
    // nothing else holds a reference to it while a callback runs.
    let state = unsafe { &mut *opaque.cast::<State<S>>() };
    loop {
        let mut data: *const c_void = std::ptr::null();
        let mut len: usize = 0;
        // SAFETY: both out-parameters are valid for the call.
        if unsafe { (pulse.stream_peek)(stream, &mut data, &mut len) } < 0 || len == 0 {
            return;
        }
        if data.is_null() {
            // A null pointer with a length is a gap the server could not fill.
            // Those samples are simply gone; dropping it is all there is to do.
            lowlat_common::log_warn!("audio: the source dropped {len} bytes");
        } else {
            // SAFETY: the interface guarantees `len` readable bytes at `data`
            // until the matching drop below.
            let fragment = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
            state.frames.take(fragment);
        }
        // SAFETY: exactly one drop per successful peek, which is the rule.
        unsafe {
            (pulse.stream_drop)(stream);
        }
    }
}

/// Somebody moved this stream to another device.
unsafe extern "C" fn on_moved<S: FnMut(&[u8])>(_stream: *mut pulse::Stream, opaque: *mut c_void) {
    // SAFETY: as in `on_read`.
    let state = unsafe { &mut *opaque.cast::<State<S>>() };
    state.moved = true;
}

/// The server's own state changed, which includes its default output.
unsafe extern "C" fn on_subscribe<S: FnMut(&[u8])>(
    _context: *mut pulse::Context,
    _event: u32,
    _index: u32,
    opaque: *mut c_void,
) {
    // SAFETY: as in `on_read`.
    let state = unsafe { &mut *opaque.cast::<State<S>>() };
    state.server_changed = true;
}

/// Every output on this machine that a host could capture.
///
/// **The identity is the monitor and the name is the speaker's**, because those
/// are two different things and an application needs both: one goes back into
/// the configuration, and the other is what a person chooses from.
///
/// Opens its own short-lived connection, so it answers before a host is started
/// and without disturbing one that is running.
pub fn outputs(server: Option<&str>) -> Result<Vec<Output>, Error> {
    prepare()?;
    let pulse = pulse().ok_or(Error::Unavailable)?;
    let stop = AtomicBool::new(false);
    let config = Settings {
        server: server.map(str::to_owned),
        wanted: Arc::new(Wanted::default()),
    };
    let session = Session::open::<fn(&[u8])>(&config, std::ptr::null_mut())?;
    session.wait_ready(&stop)?;

    let mut ask = Listing::default();
    let ask: *mut Listing = &raw mut ask;
    // SAFETY: the listing outlives the query, because this waits for it; the
    // context is ready and belongs to this thread.
    unsafe {
        let op = (pulse.sink_info_list)(session.context, Some(on_listed), ask.cast::<c_void>());
        if op.is_null() {
            return Err(Error::Refused(session.errno()));
        }
        (pulse.operation_unref)(op);
    }
    let began = lowlat_common::clock::Time::now();
    loop {
        // SAFETY: as above.
        if unsafe { (*ask).done } {
            break;
        }
        if lowlat_common::clock::elapsed_ms(began) > READY_MS {
            return Err(Error::Refused(session.errno()));
        }
        session.iterate();
    }
    // SAFETY: the query has finished and nothing else holds the listing.
    Ok(unsafe { core::mem::take(&mut (*ask).found) })
}

/// What a listing has collected so far.
#[derive(Debug, Default)]
struct Listing {
    found: Vec<Output>,
    done: bool,
}

/// One entry of a listing, or its end.
unsafe extern "C" fn on_listed(
    _context: *mut pulse::Context,
    info: *const pulse::DeviceInfo,
    end: c_int,
    opaque: *mut c_void,
) {
    // SAFETY: the pointer is the listing the query was issued with.
    let listing = unsafe { &mut *opaque.cast::<Listing>() };
    if end != 0 || info.is_null() {
        listing.done = true;
        return;
    }
    // SAFETY: the interface guarantees the pointer for the call's duration.
    let info = unsafe { &*info };
    // **An output with no monitor cannot be captured**, so it is not offered.
    if info.paired_name.is_null() {
        return;
    }
    // SAFETY: non-null strings from the interface are NUL terminated.
    let id = unsafe { CStr::from_ptr(info.paired_name) }
        .to_string_lossy()
        .into_owned();
    let name = if info.description.is_null() {
        id.clone()
    } else {
        // SAFETY: as above.
        unsafe { CStr::from_ptr(info.description) }
            .to_string_lossy()
            .into_owned()
    };
    listing.found.push(Output { id, name });
}

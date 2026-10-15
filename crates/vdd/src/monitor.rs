//! The adapter and its monitors: the slots, plugging and unplugging, and the
//! modes the system asks for (docs/11-drivers.md sections 1.3 and 1.5).
//!
//! **No framework call is made with the lock held.** Creating a monitor makes
//! the system ask for its modes, its departure can unassign its swapchain, and
//! a swapchain's teardown can call back again on another thread; each of those
//! takes the lock, so every call that can lead to one is made between two
//! holds of it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use lowlat_common::{log_info, log_warn};
use lowlat_vdd_proto::control::{Entry, List, Plugged};
use lowlat_vdd_proto::edid;
use lowlat_vdd_proto::modes::{Mode, ModeList};
use lowlat_vdd_proto::{MAX_MONITORS, container_id};

use crate::ffi::{
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI, GUID, IDARG_IN_ADAPTER_INIT, IDARG_IN_MONITORCREATE,
    IDARG_OUT_ADAPTER_INIT, IDARG_OUT_MONITORARRIVAL, IDARG_OUT_MONITORCREATE, IDDCX_ADAPTER__,
    IDDCX_ADAPTER_CAPS, IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE, IDDCX_ENDPOINT_DIAGNOSTIC_INFO,
    IDDCX_ENDPOINT_VERSION, IDDCX_FEATURE_IMPLEMENTATION_NONE, IDDCX_MONITOR__,
    IDDCX_MONITOR_DESCRIPTION, IDDCX_MONITOR_DESCRIPTION_TYPE_EDID, IDDCX_MONITOR_INFO,
    IDDCX_TRANSMISSION_TYPE_WIRED_OTHER, LUID, NTSTATUS, STATUS_ACCESS_DENIED,
    STATUS_INSUFFICIENT_RESOURCES, STATUS_INVALID_DEVICE_STATE, STATUS_INVALID_PARAMETER,
    STATUS_SUCCESS, WDFDEVICE__, WDFFILEOBJECT__, idd_adapter_init_async, idd_monitor_arrival,
    idd_monitor_create, idd_monitor_departure, nt_success, object_attributes, size_of,
    wdf_object_delete,
};
use crate::swapchain::Worker;

/// A framework handle: the name of an object the framework owns.
#[derive(Debug)]
pub(crate) struct Handle<T>(pub(crate) *mut T);

impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        core::ptr::eq(self.0, other.0)
    }
}

impl<T> Eq for Handle<T> {}

impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Handle<T> {}

// SAFETY: a handle is a name for an object the framework owns and
// serialises; passing the name between threads is how its calls are made.
unsafe impl<T> Send for Handle<T> {}

impl<T> Handle<T> {
    pub(crate) const NONE: Self = Self(core::ptr::null_mut());

    pub(crate) fn is_none(self) -> bool {
        self.0.is_null()
    }
}

/// What a monitor's swapchain worker and the list share.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    /// Pictures the system handed over, across every swapchain.
    pub(crate) frames: AtomicU64,
    /// The GPU the last swapchain renders on.
    pub(crate) render_luid: AtomicU64,
    /// A swapchain is assigned and its device set.
    pub(crate) assigned: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Free,
    Arriving,
    Live,
    Departing,
}

#[derive(Debug)]
struct Slot {
    phase: Phase,
    monitor: Handle<IDDCX_MONITOR__>,
    /// The handle that plugged it, whose end unplugs it.
    owner: Handle<WDFFILEOBJECT__>,
    modes: Option<ModeList>,
    committed: Option<Mode>,
    os_target_id: u32,
    os_adapter_luid: u64,
    stats: Arc<Stats>,
    worker: Option<Worker>,
}

impl Slot {
    fn free() -> Self {
        Self {
            phase: Phase::Free,
            monitor: Handle::NONE,
            owner: Handle::NONE,
            modes: None,
            committed: None,
            os_target_id: 0,
            os_adapter_luid: 0,
            stats: Arc::default(),
            worker: None,
        }
    }
}

#[derive(Debug)]
struct State {
    adapter: Handle<IDDCX_ADAPTER__>,
    ready: bool,
    slots: [Slot; MAX_MONITORS],
}

impl State {
    fn new() -> Self {
        Self {
            adapter: Handle::NONE,
            ready: false,
            slots: core::array::from_fn(|_| Slot::free()),
        }
    }
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// The state. A thread that panicked holding it ended the process, so a
/// poisoned lock is still a consistent one.
fn lock() -> MutexGuard<'static, State> {
    STATE
        .get_or_init(|| Mutex::new(State::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Each slot's EDID, built once: the system may read a description after the
/// call that handed it over.
fn edid_for(slot: u8) -> &'static [u8; edid::LEN] {
    static EDIDS: OnceLock<[[u8; edid::LEN]; MAX_MONITORS]> = OnceLock::new();
    let all = EDIDS
        .get_or_init(|| core::array::from_fn(|i| edid::edid(u8::try_from(i).unwrap_or(u8::MAX))));
    let last = all.len() - 1;
    all.get(usize::from(slot)).unwrap_or(&all[last])
}

pub(crate) fn guid(id: lowlat_vdd_proto::Guid) -> GUID {
    GUID {
        Data1: id.data1,
        Data2: id.data2,
        Data3: id.data3,
        Data4: id.data4,
    }
}

pub(crate) fn luid_value(luid: LUID) -> u64 {
    u64::from(luid.LowPart) | (u64::from(u32::from_ne_bytes(luid.HighPart.to_ne_bytes())) << 32)
}

/// A new device in this host process: nothing of a previous one carries over.
pub(crate) fn reset() {
    *lock() = State::new();
}

const fn wide<const N: usize>(text: &str) -> [u16; N] {
    let bytes = text.as_bytes();
    let mut out = [0u16; N];
    let mut i = 0;
    while i < bytes.len() && i + 1 < N {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

static NAME: [u16; 16] = wide("lowlat");
static MODEL: [u16; 32] = wide("lowlat virtual display");

/// Initialise the adapter, once per device: the device's first power-up.
pub(crate) fn start_adapter(device: Handle<WDFDEVICE__>) -> NTSTATUS {
    if !lock().adapter.is_none() {
        return STATUS_SUCCESS;
    }
    let mut version = IDDCX_ENDPOINT_VERSION {
        Size: size_of::<IDDCX_ENDPOINT_VERSION>(),
        MajorVer: 0,
        MinorVer: 1,
        ..Default::default()
    };
    let mut caps = IDDCX_ADAPTER_CAPS {
        Size: size_of::<IDDCX_ADAPTER_CAPS>(),
        // Without it the system sometimes refuses a monitor, and a desktop
        // smaller than the mode is scaled rather than given a mode of its own.
        Flags: IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE,
        MaxMonitorsSupported: u32::try_from(MAX_MONITORS).unwrap_or(1),
        EndPointDiagnostics: IDDCX_ENDPOINT_DIAGNOSTIC_INFO {
            Size: size_of::<IDDCX_ENDPOINT_DIAGNOSTIC_INFO>(),
            TransmissionType: IDDCX_TRANSMISSION_TYPE_WIRED_OTHER,
            pEndPointFriendlyName: NAME.as_ptr(),
            pEndPointModelName: MODEL.as_ptr(),
            pEndPointManufacturerName: NAME.as_ptr(),
            pHardwareVersion: &raw mut version,
            pFirmwareVersion: &raw mut version,
            GammaSupport: IDDCX_FEATURE_IMPLEMENTATION_NONE,
        },
        // One picture again after the desktop stills; the host's own clock
        // makes any more it wants.
        StaticDesktopReencodeFrameCount: 1,
        ..Default::default()
    };
    let mut attributes = object_attributes();
    let args = IDARG_IN_ADAPTER_INIT {
        WdfDevice: device.0,
        pCaps: &raw mut caps,
        ObjectAttributes: &raw mut attributes,
    };
    let mut out = IDARG_OUT_ADAPTER_INIT::default();
    // SAFETY: live arguments for the call; the class copies the capabilities.
    let status = unsafe { idd_adapter_init_async(&raw const args, &raw mut out) };
    if !nt_success(status) {
        log_warn!(
            "vdd: adapter init refused, status={:#x}",
            status_word(status)
        );
        return status;
    }
    lock().adapter = Handle(out.AdapterObject);
    log_info!("vdd: adapter init started");
    STATUS_SUCCESS
}

/// The class's word on the adapter's initialisation.
pub(crate) fn adapter_ready(status: NTSTATUS) {
    lock().ready = nt_success(status);
    if nt_success(status) {
        log_info!("vdd: adapter ready");
    } else {
        log_warn!(
            "vdd: adapter init failed, status={:#x}",
            status_word(status)
        );
    }
}

pub(crate) fn status_word(status: i32) -> u32 {
    u32::from_ne_bytes(status.to_ne_bytes())
}

fn release_slot(index: usize) {
    if let Some(slot) = lock().slots.get_mut(index) {
        *slot = Slot::free();
    }
}

/// Plug a monitor bound to `owner` at `mode`.
pub(crate) fn plug(owner: Handle<WDFFILEOBJECT__>, mode: Mode) -> Result<Plugged, NTSTATUS> {
    let (adapter, index) = {
        let mut state = lock();
        if !state.ready || state.adapter.is_none() {
            return Err(STATUS_INVALID_DEVICE_STATE);
        }
        let index = state
            .slots
            .iter()
            .position(|slot| slot.phase == Phase::Free)
            .ok_or(STATUS_INSUFFICIENT_RESOURCES)?;
        let slot = state
            .slots
            .get_mut(index)
            .ok_or(STATUS_INSUFFICIENT_RESOURCES)?;
        *slot = Slot {
            phase: Phase::Arriving,
            owner,
            modes: Some(ModeList::for_plug(mode)),
            ..Slot::free()
        };
        (state.adapter, index)
    };
    let slot = u8::try_from(index).map_err(|_| STATUS_INSUFFICIENT_RESOURCES)?;
    let block = edid_for(slot);
    let mut info = IDDCX_MONITOR_INFO {
        Size: size_of::<IDDCX_MONITOR_INFO>(),
        MonitorType: DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
        ConnectorIndex: u32::from(slot),
        MonitorDescription: IDDCX_MONITOR_DESCRIPTION {
            Size: size_of::<IDDCX_MONITOR_DESCRIPTION>(),
            Type: IDDCX_MONITOR_DESCRIPTION_TYPE_EDID,
            DataSize: u32::try_from(block.len()).unwrap_or(0),
            pData: block.as_ptr().cast_mut().cast(),
        },
        MonitorContainerId: guid(container_id(slot)),
    };
    let mut attributes = object_attributes();
    let args = IDARG_IN_MONITORCREATE {
        ObjectAttributes: &raw mut attributes,
        pMonitorInfo: &raw mut info,
    };
    let mut created = IDARG_OUT_MONITORCREATE::default();
    // SAFETY: live arguments; the description is static.
    let status = unsafe { idd_monitor_create(adapter.0, &raw const args, &raw mut created) };
    if !nt_success(status) {
        release_slot(index);
        log_warn!(
            "vdd: monitor create refused, slot={index} status={:#x}",
            status_word(status)
        );
        return Err(status);
    }
    let monitor = Handle(created.MonitorObject);
    // Recorded before the arrival, which can assign a swapchain that has to
    // find its slot.
    if let Some(entry) = lock().slots.get_mut(index) {
        entry.monitor = monitor;
    }
    let mut arrived = IDARG_OUT_MONITORARRIVAL::default();
    // SAFETY: a monitor the class just created.
    let status = unsafe { idd_monitor_arrival(monitor.0, &raw mut arrived) };
    if !nt_success(status) {
        // A monitor that never arrived is the driver's object to delete.
        // SAFETY: the monitor the class created for this driver.
        unsafe { wdf_object_delete(monitor.0.cast()) };
        release_slot(index);
        log_warn!(
            "vdd: monitor arrival refused, slot={index} status={:#x}",
            status_word(status)
        );
        return Err(status);
    }
    let luid = luid_value(arrived.OsAdapterLuid);
    let orphaned = match lock().slots.get_mut(index) {
        Some(entry) if entry.owner == owner => {
            entry.phase = Phase::Live;
            entry.os_target_id = arrived.OsTargetId;
            entry.os_adapter_luid = luid;
            false
        }
        Some(entry) => {
            entry.phase = Phase::Departing;
            true
        }
        None => false,
    };
    if orphaned {
        // The handle that asked ended while the monitor arrived.
        depart(index, monitor);
        return Err(STATUS_INVALID_DEVICE_STATE);
    }
    log_info!(
        "vdd: plugged, slot={index} mode={}x{}@{} target={} adapter={luid:#018x}",
        mode.width,
        mode.height,
        mode.refresh_hz,
        arrived.OsTargetId
    );
    Ok(Plugged {
        slot: u32::from(slot),
        os_target_id: arrived.OsTargetId,
        os_adapter_luid: luid,
    })
}

/// Unplug a monitor `owner` plugged.
pub(crate) fn unplug(owner: Handle<WDFFILEOBJECT__>, index: usize) -> Result<(), NTSTATUS> {
    let monitor = {
        let mut state = lock();
        let slot = state.slots.get_mut(index).ok_or(STATUS_INVALID_PARAMETER)?;
        if slot.phase != Phase::Live {
            return Err(STATUS_INVALID_PARAMETER);
        }
        if slot.owner != owner {
            return Err(STATUS_ACCESS_DENIED);
        }
        slot.phase = Phase::Departing;
        slot.monitor
    };
    depart(index, monitor);
    Ok(())
}

/// Every monitor `owner` plugged: its handle has ended. One still arriving is
/// left to its plug, which departs it once it has arrived.
pub(crate) fn release_owner(owner: Handle<WDFFILEOBJECT__>) {
    let leaving: Vec<(usize, Handle<IDDCX_MONITOR__>)> = {
        let mut state = lock();
        let mut leaving = Vec::new();
        for (index, slot) in state.slots.iter_mut().enumerate() {
            if slot.owner != owner {
                continue;
            }
            match slot.phase {
                Phase::Live => {
                    slot.phase = Phase::Departing;
                    leaving.push((index, slot.monitor));
                }
                Phase::Arriving => slot.owner = Handle::NONE,
                Phase::Free | Phase::Departing => {}
            }
        }
        leaving
    };
    for (index, monitor) in leaving {
        depart(index, monitor);
    }
}

fn depart(index: usize, monitor: Handle<IDDCX_MONITOR__>) {
    // SAFETY: a monitor that arrived and has not departed.
    let status = unsafe { idd_monitor_departure(monitor.0) };
    if !nt_success(status) {
        log_warn!(
            "vdd: departure refused, slot={index} status={:#x}",
            status_word(status)
        );
    }
    // A swapchain the departure did not unassign is stopped here, outside
    // the lock, as an unassignment stops one.
    let worker = lock()
        .slots
        .get_mut(index)
        .and_then(|slot| slot.worker.take());
    if let Some(worker) = worker {
        worker.stop();
    }
    release_slot(index);
    log_info!("vdd: unplugged, slot={index}");
}

/// Every slot, as `asker` sees it.
pub(crate) fn list(asker: Handle<WDFFILEOBJECT__>) -> List {
    let state = lock();
    let mut list = List::default();
    for ((index, slot), entry) in state.slots.iter().enumerate().zip(list.entries.iter_mut()) {
        entry.slot = u32::try_from(index).unwrap_or(u32::MAX);
        if slot.phase == Phase::Free {
            continue;
        }
        let mode = slot
            .committed
            .or(slot.modes.map(|modes| modes.preferred()))
            .unwrap_or_default();
        entry.flags = Entry::IN_USE
            | if slot.owner == asker { Entry::OWNED } else { 0 }
            | if slot.stats.assigned.load(Ordering::Relaxed) {
                Entry::SWAPCHAIN
            } else {
                0
            };
        entry.width = mode.width;
        entry.height = mode.height;
        entry.refresh_hz = mode.refresh_hz;
        entry.os_target_id = slot.os_target_id;
        entry.render_luid = slot.stats.render_luid.load(Ordering::Relaxed);
        entry.frames = slot.stats.frames.load(Ordering::Relaxed);
    }
    list
}

/// The modes of the monitor whose description this is.
pub(crate) fn modes_for_description(description: &[u8]) -> Option<ModeList> {
    let slot = usize::from(edid::slot_of(description)?);
    lock().slots.get(slot)?.modes
}

fn find(state: &State, monitor: Handle<IDDCX_MONITOR__>) -> Option<usize> {
    if monitor.is_none() {
        return None;
    }
    state.slots.iter().position(|slot| slot.monitor == monitor)
}

/// The modes of this monitor.
pub(crate) fn modes_for_monitor(monitor: Handle<IDDCX_MONITOR__>) -> Option<ModeList> {
    let state = lock();
    let index = find(&state, monitor)?;
    state.slots.get(index)?.modes
}

/// The mode the system committed this monitor to.
pub(crate) fn committed(monitor: Handle<IDDCX_MONITOR__>, mode: Mode) {
    let mut state = lock();
    if let Some(index) = find(&state, monitor) {
        if let Some(slot) = state.slots.get_mut(index) {
            slot.committed = Some(mode);
        }
        log_info!(
            "vdd: mode committed, slot={index} mode={}x{}@{}",
            mode.width,
            mode.height,
            mode.refresh_hz
        );
    }
}

/// The slot and the counters a swapchain for this monitor reports into.
pub(crate) fn stats_for(monitor: Handle<IDDCX_MONITOR__>) -> Option<(usize, Arc<Stats>)> {
    let state = lock();
    let index = find(&state, monitor)?;
    Some((index, Arc::clone(&state.slots.get(index)?.stats)))
}

/// Keep the worker of this monitor's new swapchain, handing back one that was
/// still there -- or this one, when the monitor is gone -- to be stopped
/// outside the lock.
pub(crate) fn attach_worker(monitor: Handle<IDDCX_MONITOR__>, worker: Worker) -> Option<Worker> {
    let mut state = lock();
    let Some(index) = find(&state, monitor) else {
        return Some(worker);
    };
    match state.slots.get_mut(index) {
        Some(slot) => slot.worker.replace(worker),
        None => Some(worker),
    }
}

/// Take this monitor's worker and its slot, the worker to be stopped outside
/// the lock.
pub(crate) fn detach_worker(monitor: Handle<IDDCX_MONITOR__>) -> Option<(usize, Worker)> {
    let mut state = lock();
    let index = find(&state, monitor)?;
    let worker = state.slots.get_mut(index)?.worker.take()?;
    Some((index, worker))
}

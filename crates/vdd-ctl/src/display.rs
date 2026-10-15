//! The system's display configuration, as it concerns one of the driver's
//! monitors: whether it is shown, where and at what mode, making it active,
//! and setting its mode.
//!
//! **A monitor is found by its adapter and target**, the pair the driver's
//! plug answers with, never by its `\\.\DISPLAYn` name, which the system
//! hands out afresh, nor by its EDID.

use lowlat_vdd_proto::modes::Mode;
use windows_sys::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ALL_PATHS, QDC_ONLY_ACTIVE_PATHS, QUERY_DISPLAY_CONFIG_FLAGS, QueryDisplayConfig,
    SDC_ALLOW_CHANGES, SDC_APPLY, SDC_SAVE_TO_DATABASE, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
    SetDisplayConfig,
};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LUID};
use windows_sys::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
    DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_MODE_IDX_INVALID, DM_DISPLAYFREQUENCY,
    DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
};

/// One of the driver's monitors as the system shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Shown {
    /// The system's name for the display, `\\.\DISPLAYn`.
    pub(crate) name: String,
    /// The display's top left on the desktop.
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) mode: Mode,
}

/// The configuration, as asked.
struct Config {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

fn query(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Option<Config> {
    // The configuration can grow between the two calls; ask again when it has.
    for _ in 0..4 {
        let (mut paths_len, mut modes_len) = (0u32, 0u32);
        // SAFETY: live outputs.
        if unsafe { GetDisplayConfigBufferSizes(flags, &raw mut paths_len, &raw mut modes_len) }
            != 0
        {
            return None;
        }
        // SAFETY: plain data, for which zero is a value.
        let mut paths = vec![unsafe { core::mem::zeroed() }; usize::try_from(paths_len).ok()?];
        // SAFETY: as above.
        let mut modes = vec![unsafe { core::mem::zeroed() }; usize::try_from(modes_len).ok()?];
        // SAFETY: buffers of the counts given; no topology asked for.
        let status = unsafe {
            QueryDisplayConfig(
                flags,
                &raw mut paths_len,
                paths.as_mut_ptr(),
                &raw mut modes_len,
                modes.as_mut_ptr(),
                core::ptr::null_mut(),
            )
        };
        if status == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if status != 0 {
            return None;
        }
        paths.truncate(usize::try_from(paths_len).ok()?);
        modes.truncate(usize::try_from(modes_len).ok()?);
        return Some(Config { paths, modes });
    }
    None
}

fn value(luid: LUID) -> u64 {
    u64::from(luid.LowPart) | (u64::from(u32::from_ne_bytes(luid.HighPart.to_ne_bytes())) << 32)
}

fn targets(path: &DISPLAYCONFIG_PATH_INFO, adapter: u64, target: u32) -> bool {
    value(path.targetInfo.adapterId) == adapter && path.targetInfo.id == target
}

fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> String {
    // SAFETY: plain data, for which zero is a value.
    let mut name: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { core::mem::zeroed() };
    name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    name.header.size = u32::try_from(size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>()).unwrap_or(0);
    name.header.adapterId = path.sourceInfo.adapterId;
    name.header.id = path.sourceInfo.id;
    // SAFETY: a request of the size it says.
    if unsafe { DisplayConfigGetDeviceInfo(&raw mut name.header) } != 0 {
        return String::new();
    }
    let end = name
        .viewGdiDeviceName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(name.viewGdiDeviceName.len());
    String::from_utf16_lossy(name.viewGdiDeviceName.get(..end).unwrap_or(&[]))
}

/// The monitor at `adapter`'s `target`, when the system shows it.
pub(crate) fn shown(adapter: u64, target: u32) -> Option<Shown> {
    let config = query(QDC_ONLY_ACTIVE_PATHS)?;
    let path = config
        .paths
        .iter()
        .find(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 && targets(p, adapter, target))?;
    // SAFETY: the plain index, as the configuration was asked without the
    // virtual-mode layout.
    let source = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
    let mode = config
        .modes
        .get(usize::try_from(source).ok()?)
        .filter(|m| m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)?;
    // SAFETY: a source mode, as its type says.
    let source = unsafe { mode.Anonymous.sourceMode };
    let rate = path.targetInfo.refreshRate;
    let refresh_hz = rate
        .Numerator
        .saturating_add(rate.Denominator / 2)
        .checked_div(rate.Denominator)
        .unwrap_or(0);
    Some(Shown {
        name: source_name(path),
        x: source.position.x,
        y: source.position.y,
        mode: Mode::new(source.width, source.height, refresh_hz),
    })
}

/// Whether a display of another adapter, and not a virtual one, is active:
/// what makes an arrangement safe to save (a saved one that only virtual
/// displays satisfy holds a later boot waiting for their driver).
pub(crate) fn others_active(adapter: u64) -> bool {
    query(QDC_ONLY_ACTIVE_PATHS).is_some_and(|config| {
        config.paths.iter().any(|p| {
            p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
                && value(p.targetInfo.adapterId) != adapter
                && p.targetInfo.outputTechnology != DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED
                && p.targetInfo.outputTechnology != DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_VIRTUAL
        })
    })
}

/// Make the monitor active beside what is: the active paths and one more,
/// from a source of its adapter no active path uses, the system choosing the
/// modes and the place. Saved to the system's store only when asked.
pub(crate) fn activate(adapter: u64, target: u32, save: bool) -> Result<(), u32> {
    let active = query(QDC_ONLY_ACTIVE_PATHS).ok_or(1u32)?;
    let all = query(QDC_ALL_PATHS).ok_or(1u32)?;
    let used = |source: u32| {
        active
            .paths
            .iter()
            .any(|p| value(p.sourceInfo.adapterId) == adapter && p.sourceInfo.id == source)
    };
    let mut path = *all
        .paths
        .iter()
        .filter(|p| targets(p, adapter, target) && value(p.sourceInfo.adapterId) == adapter)
        .filter(|p| !used(p.sourceInfo.id))
        .min_by_key(|p| p.sourceInfo.id)
        .ok_or(1u32)?;
    path.flags = DISPLAYCONFIG_PATH_ACTIVE;
    path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    let mut paths = active.paths;
    paths.push(path);
    let mut flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
    if save {
        flags |= SDC_SAVE_TO_DATABASE;
    }
    // SAFETY: the paths and modes of one query, and the path added from
    // another of the same moment.
    let status = unsafe {
        SetDisplayConfig(
            u32::try_from(paths.len()).unwrap_or(0),
            paths.as_ptr(),
            u32::try_from(active.modes.len()).unwrap_or(0),
            active.modes.as_ptr(),
            flags,
        )
    };
    if status != 0 {
        return Err(u32::try_from(status).unwrap_or(1));
    }
    Ok(())
}

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(core::iter::once(0)).collect()
}

/// Whether the display named `name` offers `mode` now.
pub(crate) fn offers(name: &str, mode: Mode) -> bool {
    let name = wide(name);
    // SAFETY: plain data, for which zero is a value.
    let mut dm: DEVMODEW = unsafe { core::mem::zeroed() };
    dm.dmSize = u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0);
    for index in 0..u32::from(u16::MAX) {
        // SAFETY: a terminated name and a structure of the size it says.
        let ok = unsafe {
            EnumDisplaySettingsW(
                name.as_ptr(),
                index as ENUM_DISPLAY_SETTINGS_MODE,
                &raw mut dm,
            )
        };
        if ok == 0 {
            return false;
        }
        if (dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency)
            == (mode.width, mode.height, mode.refresh_hz)
        {
            return true;
        }
    }
    false
}

/// Put the display named `name` at `mode`, saved to the system's store only
/// when asked.
pub(crate) fn set_mode(name: &str, mode: Mode, save: bool) -> Result<(), i32> {
    let name = wide(name);
    // SAFETY: plain data, for which zero is a value.
    let mut dm: DEVMODEW = unsafe { core::mem::zeroed() };
    dm.dmSize = u16::try_from(size_of::<DEVMODEW>()).unwrap_or(0);
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    dm.dmPelsWidth = mode.width;
    dm.dmPelsHeight = mode.height;
    dm.dmDisplayFrequency = mode.refresh_hz;
    let flags = if save { CDS_UPDATEREGISTRY } else { 0 };
    // SAFETY: a terminated name and a filled structure; no window.
    let status = unsafe {
        ChangeDisplaySettingsExW(
            name.as_ptr(),
            &raw const dm,
            core::ptr::null_mut(),
            flags,
            core::ptr::null(),
        )
    };
    if status != DISP_CHANGE_SUCCESSFUL {
        return Err(status);
    }
    Ok(())
}

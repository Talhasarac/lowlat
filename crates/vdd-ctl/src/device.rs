//! The driver's control interface: found, opened, and asked.

use lowlat_vdd_proto::CONTROL_INTERFACE;
use lowlat_vdd_proto::control::{
    FrameChannel, Frames, IOCTL_FRAMES, IOCTL_LIST, IOCTL_PLUG, IOCTL_RELOAD, IOCTL_REPLUG,
    IOCTL_UNPLUG, IOCTL_VERSION, List, Plug, Plugged, Replug, Unplug, VERSION, Version,
};
use lowlat_vdd_proto::modes::Mode;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_SUCCESS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BAD_COMMAND, ERROR_BUSY, ERROR_INVALID_PARAMETER,
    ERROR_NO_SYSTEM_RESOURCES, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::core::GUID;

use crate::Error;

/// An open handle on the driver: what it plugs bound is its own.
#[derive(Debug)]
pub(crate) struct Device(HANDLE);

// SAFETY: a file handle any thread may use; the controller's calls on one
// handle take turns above this.
unsafe impl Send for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: a handle this owns, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The interface's path, the first the system lists.
fn interface() -> Option<Vec<u16>> {
    let id = CONTROL_INTERFACE;
    let guid = GUID {
        data1: id.data1,
        data2: id.data2,
        data3: id.data3,
        data4: id.data4,
    };
    let mut len = 0u32;
    // SAFETY: live outputs; no device instance named.
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &raw mut len,
            &raw const guid,
            core::ptr::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS || len <= 1 {
        return None;
    }
    let mut list = vec![0u16; usize::try_from(len).ok()?];
    // SAFETY: a buffer of the length asked for.
    let cr = unsafe {
        CM_Get_Device_Interface_ListW(
            &raw const guid,
            core::ptr::null(),
            list.as_mut_ptr(),
            len,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    if cr != CR_SUCCESS {
        return None;
    }
    let end = list.iter().position(|&c| c == 0)?;
    list.truncate(end + 1);
    (end > 0).then_some(list)
}

impl Device {
    /// Open the driver and check it speaks this controller's protocol.
    pub(crate) fn open() -> Result<Self, Error> {
        let path = interface().ok_or(Error::Absent)?;
        // SAFETY: a terminated path; no security attributes, no template.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                core::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                core::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: no precondition.
            return Err(match unsafe { GetLastError() } {
                ERROR_ACCESS_DENIED => Error::Denied,
                code => Error::Io(code),
            });
        }
        let device = Self(handle);
        let mut out = [0u8; Version::LEN];
        let len = device.ask(IOCTL_VERSION, &[], &mut out)?;
        let version = Version::decode(out.get(..len).unwrap_or(&[]))
            .map_err(|_| Error::Version(0))?
            .version;
        if version != VERSION {
            return Err(Error::Version(version));
        }
        Ok(device)
    }

    /// One request, answered with the reply's length or the system's error.
    fn ask(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, Error> {
        let mut got = 0u32;
        // SAFETY: live buffers of the lengths given; a synchronous handle.
        let ok = unsafe {
            DeviceIoControl(
                self.0,
                code,
                input.as_ptr().cast(),
                u32::try_from(input.len()).unwrap_or(0),
                output.as_mut_ptr().cast(),
                u32::try_from(output.len()).unwrap_or(0),
                &raw mut got,
                core::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: no precondition.
            return Err(Error::Io(unsafe { GetLastError() }));
        }
        Ok(usize::try_from(got).unwrap_or(0))
    }

    /// The driver's answers, read for what they mean for a monitor.
    fn monitor_error(error: Error, slot_asked: bool) -> Error {
        match error {
            Error::Io(ERROR_ACCESS_DENIED) => Error::NotOwner,
            Error::Io(ERROR_NO_SYSTEM_RESOURCES) => Error::Full,
            Error::Io(ERROR_BUSY) if slot_asked => Error::SlotTaken,
            Error::Io(ERROR_BUSY) => Error::Busy,
            Error::Io(ERROR_INVALID_PARAMETER) => Error::NoMonitor,
            Error::Io(ERROR_BAD_COMMAND) => Error::Starting,
            other => other,
        }
    }

    pub(crate) fn plug(&self, plug: Plug) -> Result<Plugged, Error> {
        let mut input = [0u8; Plug::LEN];
        plug.encode(&mut input).map_err(|_| Error::Argument)?;
        let mut out = [0u8; Plugged::LEN];
        let len = self
            .ask(IOCTL_PLUG, &input, &mut out)
            .map_err(|e| Self::monitor_error(e, plug.slot.is_some()))?;
        Plugged::decode(out.get(..len).unwrap_or(&[])).map_err(|_| Error::Version(VERSION))
    }

    pub(crate) fn unplug(&self, slot: u8) -> Result<(), Error> {
        let mut input = [0u8; Unplug::LEN];
        Unplug { slot }
            .encode(&mut input)
            .map_err(|_| Error::Argument)?;
        self.ask(IOCTL_UNPLUG, &input, &mut [])
            .map_err(|e| Self::monitor_error(e, false))?;
        Ok(())
    }

    pub(crate) fn replug(&self, slot: u8, mode: Mode) -> Result<Plugged, Error> {
        let mut input = [0u8; Replug::LEN];
        Replug { slot, mode }
            .encode(&mut input)
            .map_err(|_| Error::Argument)?;
        let mut out = [0u8; Plugged::LEN];
        let len = self
            .ask(IOCTL_REPLUG, &input, &mut out)
            .map_err(|e| Self::monitor_error(e, false))?;
        Plugged::decode(out.get(..len).unwrap_or(&[])).map_err(|_| Error::Version(VERSION))
    }

    pub(crate) fn list(&self) -> Result<List, Error> {
        let mut out = vec![0u8; List::LEN];
        let len = self.ask(IOCTL_LIST, &[], &mut out)?;
        List::decode(out.get(..len).unwrap_or(&[])).map_err(|_| Error::Version(VERSION))
    }

    pub(crate) fn reload(&self) -> Result<(), Error> {
        self.ask(IOCTL_RELOAD, &[], &mut [])?;
        Ok(())
    }

    pub(crate) fn frames(&self, slot: u8) -> Result<FrameChannel, Error> {
        let mut input = [0u8; Frames::LEN];
        Frames { slot }
            .encode(&mut input)
            .map_err(|_| Error::Argument)?;
        let mut out = [0u8; FrameChannel::LEN];
        let len = self
            .ask(IOCTL_FRAMES, &input, &mut out)
            .map_err(|e| Self::monitor_error(e, false))?;
        FrameChannel::decode(out.get(..len).unwrap_or(&[])).map_err(|_| Error::Version(VERSION))
    }
}

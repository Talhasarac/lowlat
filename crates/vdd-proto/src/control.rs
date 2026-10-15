//! The control interface: version, plug, unplug and list.
//!
//! Every request and reply is a fixed sequence of little-endian words with its
//! own length first, read and written field by field. A request comes from any
//! interactive user (docs/11-drivers.md section 1.2), so the driver parses it
//! and never casts the buffer; a request whose length is not the one this
//! version defines is refused rather than read in part.

use crate::MAX_MONITORS;
use crate::modes::Mode;

/// The protocol a driver and a controller speak. A controller asks first and
/// refuses a driver whose version it does not know.
pub const VERSION: u32 = 1;

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const METHOD_BUFFERED: u32 = 0;
/// Read and write access, so a handle opened for neither cannot ask.
const FILE_READ_WRITE_ACCESS: u32 = 3;

const fn code(function: u32) -> u32 {
    (FILE_DEVICE_UNKNOWN << 16)
        | (FILE_READ_WRITE_ACCESS << 14)
        | ((0x800 + function) << 2)
        | METHOD_BUFFERED
}

/// Answers [`Version`].
pub const IOCTL_VERSION: u32 = code(1);
/// Takes [`Plug`], answers [`Plugged`].
pub const IOCTL_PLUG: u32 = code(2);
/// Takes [`Unplug`].
pub const IOCTL_UNPLUG: u32 = code(3);
/// Answers [`List`].
pub const IOCTL_LIST: u32 = code(4);

/// Why a request was not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The buffer is shorter than its length word says, or than the request.
    Short,
    /// The length word is not this version's.
    Length,
    /// A value outside what a monitor can be.
    Range,
    /// The reply does not fit the caller's buffer.
    Room,
}

fn get_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let word = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(word.try_into().ok()?))
}

fn put_u32(out: &mut [u8], at: usize, value: u32) -> Option<()> {
    out.get_mut(at..at.checked_add(4)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

fn put_u64(out: &mut [u8], at: usize, value: u64) -> Option<()> {
    out.get_mut(at..at.checked_add(8)?)?
        .copy_from_slice(&value.to_le_bytes());
    Some(())
}

fn get_u64(bytes: &[u8], at: usize) -> Option<u64> {
    let word = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(word.try_into().ok()?))
}

/// The length word a request must carry, checked against the buffer too.
fn sized(bytes: &[u8], len: usize) -> Result<(), Refused> {
    let said = get_u32(bytes, 0).ok_or(Refused::Short)?;
    if usize::try_from(said).ok() != Some(len) {
        return Err(Refused::Length);
    }
    if bytes.len() < len {
        return Err(Refused::Short);
    }
    Ok(())
}

fn length(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// The driver's answer to [`IOCTL_VERSION`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub version: u32,
    pub max_monitors: u32,
}

impl Version {
    pub const LEN: usize = 12;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, self.version).ok_or(Refused::Room)?;
        put_u32(out, 8, self.max_monitors).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        Ok(Self {
            version: get_u32(bytes, 4).ok_or(Refused::Short)?,
            max_monitors: get_u32(bytes, 8).ok_or(Refused::Short)?,
        })
    }
}

/// Plug a monitor that lives as long as the handle asking: its close, or its
/// process's end, unplugs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plug {
    /// The mode it arrives in, first and preferred in its list.
    pub mode: Mode,
}

impl Plug {
    pub const LEN: usize = 16;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, self.mode.width).ok_or(Refused::Room)?;
        put_u32(out, 8, self.mode.height).ok_or(Refused::Room)?;
        put_u32(out, 12, self.mode.refresh_hz).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        let mode = Mode {
            width: get_u32(bytes, 4).ok_or(Refused::Short)?,
            height: get_u32(bytes, 8).ok_or(Refused::Short)?,
            refresh_hz: get_u32(bytes, 12).ok_or(Refused::Short)?,
        };
        if !mode.is_valid() {
            return Err(Refused::Range);
        }
        Ok(Self { mode })
    }
}

/// The driver's answer to [`Plug`]: the slot, and where the system put the
/// monitor -- its adapter and its target -- so the controller can find the
/// display the system made of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plugged {
    pub slot: u32,
    pub os_target_id: u32,
    pub os_adapter_luid: u64,
}

impl Plugged {
    pub const LEN: usize = 20;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, self.slot).ok_or(Refused::Room)?;
        put_u32(out, 8, self.os_target_id).ok_or(Refused::Room)?;
        put_u64(out, 12, self.os_adapter_luid).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        Ok(Self {
            slot: get_u32(bytes, 4).ok_or(Refused::Short)?,
            os_target_id: get_u32(bytes, 8).ok_or(Refused::Short)?,
            os_adapter_luid: get_u64(bytes, 12).ok_or(Refused::Short)?,
        })
    }
}

/// Unplug a monitor this handle plugged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unplug {
    pub slot: u32,
}

impl Unplug {
    pub const LEN: usize = 8;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, self.slot).ok_or(Refused::Room)?;
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        let slot = get_u32(bytes, 4).ok_or(Refused::Short)?;
        if usize::try_from(slot).map_or(true, |slot| slot >= MAX_MONITORS) {
            return Err(Refused::Range);
        }
        Ok(Self { slot })
    }
}

/// One slot as [`IOCTL_LIST`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Entry {
    pub slot: u32,
    /// [`Entry::IN_USE`], [`Entry::OWNED`], [`Entry::SWAPCHAIN`].
    pub flags: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub os_target_id: u32,
    /// The GPU the system renders the monitor's desktop on, from its last
    /// swapchain; zero before one.
    pub render_luid: u64,
    /// Pictures the system handed over, from every swapchain the monitor had.
    pub frames: u64,
}

impl Entry {
    pub const IN_USE: u32 = 1;
    /// Plugged through the handle asking.
    pub const OWNED: u32 = 2;
    /// A swapchain is assigned now.
    pub const SWAPCHAIN: u32 = 4;
    pub const LEN: usize = 40;

    fn encode(&self, out: &mut [u8]) -> Option<()> {
        put_u32(out, 0, self.slot)?;
        put_u32(out, 4, self.flags)?;
        put_u32(out, 8, self.width)?;
        put_u32(out, 12, self.height)?;
        put_u32(out, 16, self.refresh_hz)?;
        put_u32(out, 20, self.os_target_id)?;
        put_u64(out, 24, self.render_luid)?;
        put_u64(out, 32, self.frames)
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            slot: get_u32(bytes, 0)?,
            flags: get_u32(bytes, 4)?,
            width: get_u32(bytes, 8)?,
            height: get_u32(bytes, 12)?,
            refresh_hz: get_u32(bytes, 16)?,
            os_target_id: get_u32(bytes, 20)?,
            render_luid: get_u64(bytes, 24)?,
            frames: get_u64(bytes, 32)?,
        })
    }
}

/// The driver's answer to [`IOCTL_LIST`]: every slot, in use or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct List {
    pub entries: [Entry; MAX_MONITORS],
}

impl List {
    pub const LEN: usize = 8 + MAX_MONITORS * Entry::LEN;

    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Refused> {
        if out.len() < Self::LEN {
            return Err(Refused::Room);
        }
        put_u32(out, 0, length(Self::LEN)).ok_or(Refused::Room)?;
        put_u32(out, 4, length(MAX_MONITORS)).ok_or(Refused::Room)?;
        for (entry, at) in self.entries.iter().zip((8..).step_by(Entry::LEN)) {
            let room = out.get_mut(at..).ok_or(Refused::Room)?;
            entry.encode(room).ok_or(Refused::Room)?;
        }
        Ok(Self::LEN)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refused> {
        sized(bytes, Self::LEN)?;
        let count = get_u32(bytes, 4).ok_or(Refused::Short)?;
        if usize::try_from(count).ok() != Some(MAX_MONITORS) {
            return Err(Refused::Length);
        }
        let mut list = Self::default();
        for (entry, at) in list.entries.iter_mut().zip((8..).step_by(Entry::LEN)) {
            let bytes = bytes.get(at..).ok_or(Refused::Short)?;
            *entry = Entry::decode(bytes).ok_or(Refused::Short)?;
        }
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_the_systems_buffered_read_write_form() {
        // CTL_CODE(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED,
        // FILE_READ_ACCESS | FILE_WRITE_ACCESS), as the system's macro makes it.
        assert_eq!(IOCTL_VERSION, 0x0022_E004);
        assert_eq!(IOCTL_PLUG, 0x0022_E008);
        assert_eq!(IOCTL_UNPLUG, 0x0022_E00C);
        assert_eq!(IOCTL_LIST, 0x0022_E010);
    }

    #[test]
    fn a_plug_round_trips_and_its_mode_is_checked() {
        let plug = Plug {
            mode: Mode::new(2560, 1440, 120),
        };
        let mut buf = [0u8; Plug::LEN];
        assert_eq!(plug.encode(&mut buf), Ok(Plug::LEN));
        assert_eq!(Plug::decode(&buf), Ok(plug));

        let tiny = Plug {
            mode: Mode::new(16, 16, 60),
        };
        tiny.encode(&mut buf).expect("room");
        assert_eq!(Plug::decode(&buf), Err(Refused::Range));
    }

    #[test]
    fn a_request_of_another_length_is_refused_not_read_in_part() {
        let mut buf = [0u8; Plug::LEN + 4];
        Plug {
            mode: Mode::new(1920, 1080, 60),
        }
        .encode(&mut buf)
        .expect("room");
        assert!(Plug::decode(&buf).is_ok(), "trailing room is the caller's");
        buf[0] = 20;
        assert_eq!(Plug::decode(&buf), Err(Refused::Length));
        assert_eq!(Plug::decode(&buf[..10]), Err(Refused::Length));
        let short = [16u8, 0, 0, 0, 0, 0];
        assert_eq!(Plug::decode(&short), Err(Refused::Short));
        assert_eq!(Plug::decode(&[]), Err(Refused::Short));
    }

    #[test]
    fn an_unplug_past_the_last_slot_is_refused() {
        let mut buf = [0u8; Unplug::LEN];
        Unplug { slot: 15 }.encode(&mut buf).expect("room");
        assert_eq!(Unplug::decode(&buf), Ok(Unplug { slot: 15 }));
        Unplug { slot: 16 }.encode(&mut buf).expect("room");
        assert_eq!(Unplug::decode(&buf), Err(Refused::Range));
    }

    #[test]
    fn replies_round_trip_and_refuse_a_short_buffer() {
        let version = Version {
            version: VERSION,
            max_monitors: 16,
        };
        let mut buf = [0u8; 64];
        assert_eq!(version.encode(&mut buf), Ok(Version::LEN));
        assert_eq!(Version::decode(&buf), Ok(version));
        assert_eq!(version.encode(&mut buf[..8]), Err(Refused::Room));

        let plugged = Plugged {
            slot: 3,
            os_target_id: 0x1234,
            os_adapter_luid: 0x0000_0001_0002_0003,
        };
        assert_eq!(plugged.encode(&mut buf), Ok(Plugged::LEN));
        assert_eq!(Plugged::decode(&buf), Ok(plugged));

        let mut list = List::default();
        list.entries[2] = Entry {
            slot: 2,
            flags: Entry::IN_USE | Entry::OWNED,
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            os_target_id: 7,
            render_luid: 0xdead_beef,
            frames: 1 << 40,
        };
        let mut big = [0u8; List::LEN];
        assert_eq!(list.encode(&mut big), Ok(List::LEN));
        assert_eq!(List::decode(&big), Ok(list));
        let mut fewer = big;
        fewer[4] = 15;
        assert_eq!(
            List::decode(&fewer),
            Err(Refused::Length),
            "a list of another count"
        );
        assert_eq!(list.encode(&mut big[..List::LEN - 1]), Err(Refused::Room));
    }
}

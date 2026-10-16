//! `lowlatd display`: the host's virtual displays from a shell, through the
//! driver's controller (docs/11-drivers.md section 1.9). A person's own
//! displays are kept -- a command's process ends with it -- and `hold` keeps a
//! bound one for as long as it runs.
//!
//!   lowlatd display list
//!   lowlatd display add WxH[@Hz] [--restore] [--slot N]
//!   lowlatd display hold WxH[@Hz] [--slot N] [--for SECONDS]
//!   lowlatd display remove SLOT
//!   lowlatd display mode SLOT WxH[@Hz]
//!   lowlatd display modes [add|remove WxH@Hz]
//!   lowlatd display gpu [auto|VVVV:DDDD]
//!   lowlatd display pointer [hardware|drawn]
//!   lowlatd display edid [FILE|none]

const USAGE: &str = "usage:
  lowlatd display list
  lowlatd display add WxH[@Hz] [--restore] [--slot N]
  lowlatd display hold WxH[@Hz] [--slot N] [--for SECONDS]
  lowlatd display remove SLOT
  lowlatd display mode SLOT WxH[@Hz]
  lowlatd display modes [add|remove WxH@Hz]
  lowlatd display gpu [auto|VVVV:DDDD]
  lowlatd display pointer [hardware|drawn]
  lowlatd display edid [FILE|none]";

/// Run a display command and end the process with its outcome: 0 done, 1
/// refused, 2 a command line not understood.
pub(crate) fn main(args: &[String]) -> ! {
    let code = match imp::run(args) {
        Ok(()) => 0,
        Err(Fail::Usage) => {
            eprintln!("{USAGE}");
            2
        }
        Err(Fail::Refused(why)) => {
            eprintln!("lowlatd display: {why}");
            1
        }
    };
    std::process::exit(code)
}

enum Fail {
    Usage,
    Refused(String),
}

#[cfg(not(windows))]
mod imp {
    use super::Fail;

    pub(super) fn run(args: &[String]) -> Result<(), Fail> {
        if args.is_empty() {
            return Err(Fail::Usage);
        }
        Err(Fail::Refused(
            "the virtual displays are the Windows driver's; there is none here".into(),
        ))
    }
}

#[cfg(windows)]
mod imp {
    use std::io::BufRead;
    use std::time::Duration;

    use lowlat_vdd_ctl::{Controller, Error, Gpu, Lifetime, Mode, Monitor, Pointer};

    use super::Fail;

    fn refused(error: Error) -> Fail {
        Fail::Refused(match error {
            Error::Absent => "the display driver is not installed or not started".into(),
            Error::Version(v) => format!("the display driver speaks version {v}, not this one's"),
            Error::Denied => "the display driver refused this account".into(),
            Error::Starting => "the display driver is still starting".into(),
            Error::Full => "every slot is taken".into(),
            Error::SlotTaken => "that slot is taken".into(),
            Error::NotOwner => "that display belongs to another program".into(),
            Error::NoMonitor => "no display at that slot".into(),
            Error::Mode => "no display can be that mode".into(),
            Error::Argument => "those options contradict each other".into(),
            Error::Busy => "that display is being plugged again; try again".into(),
            Error::Display(code) => format!("the system's display configuration refused ({code})"),
            Error::NotReady => {
                "the display is plugged but the system has not finished; `list` shows it".into()
            }
            Error::File => {
                "the settings file could not be read or written (is the driver installed?)".into()
            }
            Error::Io(code) => format!("the display driver answered error {code}"),
        })
    }

    /// `WxH` or `WxH@Hz`, 60 when no rate is given.
    fn mode(text: &str) -> Result<Mode, Fail> {
        let full = if text.contains('@') {
            text.to_string()
        } else {
            format!("{text}@60")
        };
        let mode = Mode::parse(&full).ok_or(Fail::Usage)?;
        if !mode.is_valid() {
            return Err(Fail::Refused(format!(
                "{mode} is outside 640x480 to 7680x4320 at 24 to 480 Hz"
            )));
        }
        Ok(mode)
    }

    fn slot(text: &str) -> Result<u8, Fail> {
        text.parse::<u8>()
            .ok()
            .filter(|slot| usize::from(*slot) < lowlat_vdd_ctl::MAX_MONITORS)
            .ok_or(Fail::Usage)
    }

    /// The value after `--name`, and whether the flag was there.
    fn option<'a>(args: &'a [String], name: &str) -> Result<Option<&'a str>, Fail> {
        match args.iter().position(|a| a == name) {
            None => Ok(None),
            Some(at) => args
                .get(at + 1)
                .map(|value| Some(value.as_str()))
                .ok_or(Fail::Usage),
        }
    }

    fn show(monitor: &Monitor) {
        let lifetime = match (monitor.kept, monitor.restore, monitor.owned) {
            (true, true, _) => "kept, restored",
            (true, false, _) => "kept",
            (false, _, true) => "bound to this command",
            (false, _, false) => "bound to another program",
        };
        if monitor.active {
            println!(
                "slot {}: {} at {},{} {} ({lifetime})",
                monitor.slot, monitor.mode, monitor.x, monitor.y, monitor.name
            );
        } else {
            println!(
                "slot {}: {} not shown ({lifetime})",
                monitor.slot, monitor.mode
            );
        }
    }

    pub(super) fn run(args: &[String]) -> Result<(), Fail> {
        let verb = args.first().map(String::as_str).ok_or(Fail::Usage)?;
        let rest = args.get(1..).unwrap_or(&[]);
        match verb {
            "list" => {
                let controller = Controller::open().map_err(refused)?;
                let monitors = controller.list().map_err(refused)?;
                if monitors.is_empty() {
                    println!("no virtual displays");
                }
                monitors.iter().for_each(show);
                Ok(())
            }
            "add" => {
                let asked = mode(rest.first().ok_or(Fail::Usage)?)?;
                let restore = rest.iter().any(|a| a == "--restore");
                let at = option(rest, "--slot")?.map(slot).transpose()?;
                let mut controller = Controller::open().map_err(refused)?;
                let monitor = controller
                    .plug(asked, Lifetime::Kept, restore, at)
                    .map_err(refused)?;
                show(&monitor);
                Ok(())
            }
            "hold" => {
                let asked = mode(rest.first().ok_or(Fail::Usage)?)?;
                let at = option(rest, "--slot")?.map(slot).transpose()?;
                let seconds = option(rest, "--for")?
                    .map(|s| s.parse::<u64>().map_err(|_| Fail::Usage))
                    .transpose()?;
                let mut controller = Controller::open().map_err(refused)?;
                let monitor = controller
                    .plug(asked, Lifetime::Bound, false, at)
                    .map_err(refused)?;
                show(&monitor);
                match seconds {
                    Some(seconds) => std::thread::sleep(Duration::from_secs(seconds)),
                    None => {
                        println!("held; press Enter to unplug");
                        let _ = std::io::stdin().lock().lines().next();
                    }
                }
                // Closing the handle unplugs it; said here so a person sees it.
                drop(controller);
                println!("slot {}: unplugged", monitor.slot);
                Ok(())
            }
            "remove" => {
                let at = slot(rest.first().ok_or(Fail::Usage)?)?;
                let mut controller = Controller::open().map_err(refused)?;
                controller.unplug(at).map_err(refused)?;
                println!("slot {at}: removed");
                Ok(())
            }
            "mode" => {
                let at = slot(rest.first().ok_or(Fail::Usage)?)?;
                let asked = mode(rest.get(1).ok_or(Fail::Usage)?)?;
                let mut controller = Controller::open().map_err(refused)?;
                let monitor = controller.set_mode(at, asked).map_err(refused)?;
                show(&monitor);
                Ok(())
            }
            "modes" => match rest.first().map(String::as_str) {
                None => {
                    let settings = lowlat_vdd_ctl::settings().map_err(refused)?;
                    if settings.modes().is_empty() {
                        println!("no custom modes");
                    }
                    for mode in settings.modes() {
                        println!("{mode}");
                    }
                    Ok(())
                }
                Some(change @ ("add" | "remove")) => {
                    let asked = mode(rest.get(1).ok_or(Fail::Usage)?)?;
                    let mut controller = Controller::open().map_err(refused)?;
                    let mut done = false;
                    controller
                        .change_settings(|settings| {
                            done = if change == "add" {
                                settings.add_mode(asked)
                            } else {
                                settings.remove_mode(asked)
                            };
                        })
                        .map_err(refused)?;
                    if !done {
                        return Err(Fail::Refused(if change == "add" {
                            "the file holds as many custom modes as it takes".into()
                        } else {
                            format!("{asked} is not a custom mode")
                        }));
                    }
                    println!("{change}ed {asked}; displays plugged from now list it");
                    Ok(())
                }
                Some(_) => Err(Fail::Usage),
            },
            "gpu" => match rest.first().map(String::as_str) {
                None => {
                    let settings = lowlat_vdd_ctl::settings().map_err(refused)?;
                    match settings.gpu {
                        Some(gpu) => println!("gpu {:04x}:{:04x}", gpu.vendor, gpu.device),
                        None => println!("gpu auto (the high-performance one)"),
                    }
                    for info in lowlat_vdd_ctl::gpus() {
                        println!(
                            "  {:04x}:{:04x} {}",
                            info.gpu.vendor, info.gpu.device, info.name
                        );
                    }
                    Ok(())
                }
                Some(choice) => {
                    let gpu = if choice == "auto" {
                        None
                    } else {
                        let (vendor, device) = choice.split_once(':').ok_or(Fail::Usage)?;
                        Some(Gpu {
                            vendor: u16::from_str_radix(vendor, 16).map_err(|_| Fail::Usage)?,
                            device: u16::from_str_radix(device, 16).map_err(|_| Fail::Usage)?,
                        })
                    };
                    let mut controller = Controller::open().map_err(refused)?;
                    controller
                        .change_settings(|settings| settings.gpu = gpu)
                        .map_err(refused)?;
                    println!("gpu {choice}; the displays move to it now");
                    Ok(())
                }
            },
            "pointer" => match rest.first().map(String::as_str) {
                None => {
                    let settings = lowlat_vdd_ctl::settings().map_err(refused)?;
                    println!(
                        "pointer {}",
                        if settings.pointer == Pointer::Drawn {
                            "drawn"
                        } else {
                            "hardware"
                        }
                    );
                    Ok(())
                }
                Some(choice @ ("hardware" | "drawn")) => {
                    let pointer = if choice == "drawn" {
                        Pointer::Drawn
                    } else {
                        Pointer::Hardware
                    };
                    let mut controller = Controller::open().map_err(refused)?;
                    controller
                        .change_settings(|settings| settings.pointer = pointer)
                        .map_err(refused)?;
                    println!("pointer {choice}; displays take it at their next swapchain");
                    Ok(())
                }
                Some(_) => Err(Fail::Usage),
            },
            "edid" => match rest.first().map(String::as_str) {
                None => {
                    let settings = lowlat_vdd_ctl::settings().map_err(refused)?;
                    println!(
                        "edid {}",
                        if settings.edid.is_some() {
                            "dumped (from a file)"
                        } else {
                            "the driver's own"
                        }
                    );
                    Ok(())
                }
                Some(choice) => {
                    let edid = if choice == "none" {
                        None
                    } else {
                        let bytes = std::fs::read(choice)
                            .map_err(|e| Fail::Refused(format!("{choice}: {e}")))?;
                        Some(lowlat_vdd_ctl::Edid::parse(&bytes).ok_or_else(|| {
                            Fail::Refused(format!(
                                "{choice} is not an EDID of one or two valid blocks"
                            ))
                        })?)
                    };
                    let mut controller = Controller::open().map_err(refused)?;
                    controller
                        .change_settings(|settings| settings.edid = edid)
                        .map_err(refused)?;
                    println!(
                        "edid {choice}; displays plugged from now carry it, each a new monitor to the system"
                    );
                    Ok(())
                }
            },
            _ => Err(Fail::Usage),
        }
    }
}

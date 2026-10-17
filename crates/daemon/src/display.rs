//! `lowlatd display`: the host's virtual displays from a shell, through the
//! driver's controller (docs/11-drivers.md section 1.9). The commands are the
//! controller's own (`lowlat_vdd_ctl::cli`), which the program `lowlat-vdd`
//! takes too.

/// Run a display command and end the process with its outcome: 0 done, 1
/// refused, 2 a command line not understood.
#[cfg(windows)]
pub(crate) fn main(args: &[String]) -> ! {
    std::process::exit(lowlat_vdd_ctl::cli::run("lowlatd display", args))
}

/// The commands, named for a person who asks with nothing: the Windows
/// driver's.
#[cfg(not(windows))]
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

/// Off Windows there is no driver: no command line is answered with the
/// commands, any other refused.
#[cfg(not(windows))]
pub(crate) fn main(args: &[String]) -> ! {
    if args.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    eprintln!("lowlatd display: the virtual displays are the Windows driver's; there is none here");
    std::process::exit(1)
}

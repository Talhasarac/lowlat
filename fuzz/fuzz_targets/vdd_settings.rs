//! The display driver's settings file is written by any user of the machine
//! and read by the driver when it starts (docs/11-drivers.md section 1.4), so
//! its reader meets whatever bytes a user chose.
//!
//! Whatever is read must write back and read again as itself: a file the
//! driver accepted is one the controller can rewrite without losing a line it
//! understood.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_vdd_proto::settings::{MAX_FILE, Settings};

fuzz_target!(|data: &[u8]| {
    let (settings, _) = Settings::parse(data);
    let mut out = vec![0u8; MAX_FILE];
    let len = settings
        .write(&mut out)
        .expect("what was read fits the file");
    let (again, skipped) = Settings::parse(&out[..len]);
    assert_eq!(skipped, 0, "a line the writer wrote is not one the reader reads");
    assert_eq!(again, settings, "a file that does not read back as itself");
});

//! Link the frameworks' stub libraries from the driver kit that
//! scripts/fetch-wdk.py unpacks, or LOWLAT_WDK_DIR names.
//!
//! The user-mode framework's stub carries the entry point the driver host
//! calls -- it binds to the framework, then calls the driver's own
//! `DriverEntry` -- and exports it itself; nothing in the driver refers to it,
//! so the stub is linked whole. The indirect display class's stub carries that
//! class's function table. Without the kit the crate still checks, and the
//! link says what is missing.

use std::path::PathBuf;

const KIT: &str = "10.0.26100.6584";

fn main() {
    println!("cargo:rerun-if-env-changed=LOWLAT_WDK_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let kit = match std::env::var_os("LOWLAT_WDK_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let manifest =
                PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
            manifest.join("../../target/wdk").join(KIT)
        }
    };
    let lib = kit.join("c").join("Lib");
    let umdf = lib.join("wdf/umdf/x64/2.25");
    let idd = lib.join("10.0.26100.0/um/x64/iddcx/1.4");
    if !umdf.join("WdfDriverStubUm.lib").is_file() || !idd.join("iddcxstub.lib").is_file() {
        println!(
            "cargo:warning=no driver kit at {}: run scripts/fetch-wdk.py, or the driver will not link",
            kit.display()
        );
        return;
    }
    println!("cargo:rustc-link-search=native={}", umdf.display());
    println!("cargo:rustc-link-search=native={}", idd.display());
    println!("cargo:rustc-link-lib=static:+whole-archive=WdfDriverStubUm");
    println!("cargo:rustc-link-lib=static=iddcxstub");
}

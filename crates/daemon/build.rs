//! The program's manifest on Windows, embedded by the linker into this
//! binary alone: per-monitor awareness for the whole process
//! (docs/impl-plan-windows.md W2). Nothing is compiled and nothing else is
//! linked here.

fn main() {
    println!("cargo:rerun-if-changed=lowlatd.manifest");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let manifest = std::path::Path::new(&dir).join("lowlatd.manifest");
    println!("cargo:rustc-link-arg-bin=lowlatd=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=lowlatd=/MANIFESTINPUT:{}",
        manifest.display()
    );
}

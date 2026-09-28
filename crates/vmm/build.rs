//! Selects the hypervisor backend for the target: `cfg(hv = "...")`, plus `cfg(hv)` when
//! there is one. Exactly one backend drives VMs on a given OS and architecture
//! (docs/design/architecture.md D13); targets without one still build, and report that
//! VMs cannot run there.

// Cargo reads build-script directives from stdout.
#![allow(clippy::print_stdout)]

fn main() {
    println!("cargo::rustc-check-cfg=cfg(hv, values(none(), \"hvf\"))");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if let Some(backend) = match (os.as_str(), arch.as_str()) {
        ("macos", "aarch64") => Some("hvf"),
        _ => None,
    } {
        println!("cargo::rustc-cfg=hv");
        println!("cargo::rustc-cfg=hv=\"{backend}\"");
    }
}

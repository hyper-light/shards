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
        if backend == "hvf" {
            compile_hvf_shim();
        }
    }
}

/// Builds src/hv/hvf/simd.c into a static library with the host C compiler, the one
/// rustc already links macOS binaries with.
fn compile_hvf_shim() {
    let src = "src/hv/hvf/simd.c";
    println!("cargo::rerun-if-changed={src}");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    let obj = out.join("simd.o");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let run = |cmd: &mut std::process::Command| match cmd.status() {
        Ok(s) if s.success() => {}
        other => {
            println!("cargo::error=running {cmd:?}: {other:?}");
            std::process::exit(1);
        }
    };
    run(std::process::Command::new(&cc)
        .args(["-c", "-O2", "-Wall", "-Wextra", "-Werror", "-arch", "arm64"])
        .arg("-mmacosx-version-min=15.0")
        .arg(src)
        .arg("-o")
        .arg(&obj));
    run(std::process::Command::new("ar")
        .arg("crs")
        .arg(out.join("libshardshvf.a"))
        .arg(&obj));
    println!("cargo::rustc-link-search=native={}", out.display());
    println!("cargo::rustc-link-lib=static=shardshvf");
}

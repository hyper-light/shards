//! Selects the hypervisor backend for the target: `cfg(hv = "...")`, plus `cfg(hv)` when
//! there is one. Exactly one backend drives VMs on a given OS and architecture
//! (docs/design/architecture.md D13); targets without one still build, and report that
//! VMs cannot run there.

// Cargo reads build-script directives from stdout.
#![allow(clippy::print_stdout)]

fn main() {
    println!("cargo::rustc-check-cfg=cfg(hv, values(none(), \"hvf\", \"kvm\", \"whp\"))");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if let Some(backend) = match (os.as_str(), arch.as_str()) {
        ("macos", "aarch64") => Some("hvf"),
        ("linux", "x86_64" | "aarch64") => Some("kvm"),
        ("windows", "x86_64") => Some("whp"),
        _ => None,
    } {
        println!("cargo::rustc-cfg=hv");
        println!("cargo::rustc-cfg=hv=\"{backend}\"");
        if backend == "hvf" {
            compile_hvf_shim();
        }
    }
    if std::env::var_os("CARGO_FEATURE_GPU").is_some() && os == "macos" {
        link_gpu();
    }
}

/// Links the GPU's renderer (D144): virglrenderer's Venus, libepoxy and MoltenVK, static,
/// from the directory SHARDS_GPU_LIBS names (scripts/gpu-libs builds it), and the system
/// frameworks MoltenVK needs (PM M214). Without the directory nothing is linked: a check
/// build of the feature needs none, and a build that links fails on the renderer's names.
fn link_gpu() {
    println!("cargo::rerun-if-env-changed=SHARDS_GPU_LIBS");
    let Some(dir) = std::env::var_os("SHARDS_GPU_LIBS") else {
        println!("cargo::warning=the gpu feature links the renderer from SHARDS_GPU_LIBS, which is not set");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    for lib in ["virglrenderer", "epoxy", "MoltenVK"] {
        println!(
            "cargo::rerun-if-changed={}",
            dir.join(format!("lib{lib}.a")).display()
        );
    }
    println!("cargo::rustc-link-search=native={}", dir.display());
    for lib in ["virglrenderer", "epoxy", "MoltenVK"] {
        println!("cargo::rustc-link-lib=static={lib}");
    }
    for framework in [
        "Metal",
        "QuartzCore",
        "IOSurface",
        "Foundation",
        "IOKit",
        "CoreGraphics",
        "AppKit",
    ] {
        println!("cargo::rustc-link-lib=framework={framework}");
    }
    println!("cargo::rustc-link-lib=c++");
    // MoltenVK's `@available` checks call compiler-rt's __isPlatformVersionAtLeast, which
    // clang links into every Darwin program (clang's Darwin.cpp, AddLinkRuntimeLibArgs:
    // libclang_rt.osx.a) and rustc's link does not: std's weak copy is gone under release's
    // fat LTO, which sees no Rust caller of it. A link-lib, which reaches the binaries this
    // library is linked into, as a build script's link-arg does not.
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let resources = std::process::Command::new(&cc)
        .arg("-print-resource-dir")
        .output();
    let darwin = match resources {
        Ok(out) if out.status.success() => {
            std::path::Path::new(String::from_utf8_lossy(&out.stdout).trim()).join("lib/darwin")
        }
        _ => {
            println!(
                "cargo::warning=the gpu feature links compiler-rt, which {cc} -print-resource-dir did not say"
            );
            return;
        }
    };
    let rt = darwin.join("libclang_rt.osx.a");
    if !rt.exists() {
        println!(
            "cargo::warning=the gpu feature links compiler-rt, which is not at {}",
            rt.display()
        );
        return;
    }
    println!("cargo::rerun-if-changed={}", rt.display());
    println!("cargo::rustc-link-search=native={}", darwin.display());
    println!("cargo::rustc-link-lib=static=clang_rt.osx");
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

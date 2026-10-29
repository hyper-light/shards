//! Builds what `shards` carries inside it, so that one binary is all there is to install
//! (docs/design/architecture.md D36):
//! - shards-init, the guest's PID 1, for this target's architecture, so that `shards`
//!   carries the init its guests run and the two always match (D28).
//!   `SHARDS_INIT_BINARY` names a prebuilt one instead.
//! - shards-server, the in-VM server (D60), likewise; `SHARDS_SERVER_BINARY` names a
//!   prebuilt one.
//! - The VM process (`shards-vm`) and the network process (`shards-net`), which `shards`
//!   writes out once per build and starts (src/helpers.rs). Built by a nested cargo for
//!   this target and profile into `target/helpers`, kept between builds, on Linux static
//!   (musl) whatever C library `shards` links (PM M281); on macOS the VM
//!   process signed with its App Sandbox entitlements first, so that its signature travels
//!   inside `shards`. `SHARDS_HELPERS_DIR` names prebuilt ones; `SHARDS_HELPERS=skip`
//!   carries none, for check builds of other targets (scripts/lint), and such a `shards`
//!   refuses what needs them.
//! - The GPU's VM process (`shards-vm-gpu`, D144, D201, D202): the VM process built with
//!   its renderer, on Apple silicon, on Linux with glibc and on x86_64 Windows where
//!   `SHARDS_GPU_LIBS` names the renderer's libraries (scripts/gpu-libs), into
//!   `target/helpers-gpu`, signed as the VM process is on macOS; or `SHARDS_HELPERS_DIR`'s.
//!   Without it, `shards` carries none and refuses a GPU.
//!
//! The init is built by a nested cargo, for `<arch>-unknown-linux-musl` with the `guest`
//! profile, into a target directory of its own under `OUT_DIR`. It names its linker,
//! `rust-lld`, rather than rely on `.cargo/config.toml`, which `cargo install` from
//! elsewhere does not read [PM M36]. The binary is copied to `OUT_DIR/shards-init`, where
//! `shardsd` includes it; an environment variable naming it would reach the package's
//! tests and `cargo run` too, which cargo runs with its build scripts' `rustc-env`.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    if let Err(e) = build() {
        let mut out = io::stdout().lock();
        for line in e.lines() {
            let _ = writeln!(out, "cargo::error={line}");
        }
    }
}

fn build() -> Result<(), String> {
    helpers()?;
    let mut out = io::stdout().lock();
    // The compiler that built it, which `version` names where Docker names Go's.
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    if let Ok(o) = std::process::Command::new(&rustc).arg("--version").output()
        && let Ok(v) = String::from_utf8(o.stdout)
    {
        let _ = writeln!(out, "cargo::rustc-env=SHARDS_RUSTC={}", v.trim());
    }
    for var in ["CC_x86_64_unknown_linux_musl", "CC_aarch64_unknown_linux_musl"] {
        let _ = writeln!(out, "cargo::rerun-if-env-changed={var}");
    }
    let var = |name: &str| env::var_os(name).ok_or_else(|| format!("{name} is not set"));
    let out_dir = PathBuf::from(var("OUT_DIR")?);
    // Each guest binary it carries: shards-init, and the in-VM server (D60), whose
    // instances init starts from a read-only device of their own.
    let guests: [(&str, &str, &str, &[&str]); 2] = [
        (
            "shards-init",
            "SHARDS_INIT_BINARY",
            "init",
            &["crates/init", "crates/abi", "crates/cmdline"],
        ),
        (
            "shards-server",
            "SHARDS_SERVER_BINARY",
            "server",
            &["crates/server", "crates/init/src/json.rs", "vendor"],
        ),
    ];
    for (package, given_var, dir, inputs) in guests {
        let _ = writeln!(out, "cargo::rerun-if-env-changed={given_var}");
        let built = match env::var_os(given_var).filter(|p| !p.is_empty()) {
            Some(given) => {
                let path = PathBuf::from(given);
                // A build script runs in its package's directory, not where cargo was run.
                if !path.is_absolute() || !path.is_file() {
                    return Err(format!(
                        "{given_var}: {} is not an absolute path to a file",
                        path.display()
                    ));
                }
                let _ = writeln!(out, "cargo::rerun-if-changed={}", path.display());
                path
            }
            None => {
                let arch =
                    env::var("CARGO_CFG_TARGET_ARCH").map_err(|e| format!("CARGO_CFG_TARGET_ARCH: {e}"))?;
                if arch != "x86_64" && arch != "aarch64" {
                    return Err(format!("shards guests are x86_64 or aarch64, not {arch}"));
                }
                let root = Path::new(&var("CARGO_MANIFEST_DIR")?).join("..").join("..");
                for input in inputs.iter().chain(&["Cargo.toml", "Cargo.lock"]) {
                    let _ = writeln!(out, "cargo::rerun-if-changed={}", root.join(input).display());
                }
                compile(
                    &root,
                    &arch,
                    &out_dir.join(dir),
                    var("CARGO")?,
                    package,
                    given_var,
                )?
            }
        };
        let to = out_dir.join(package);
        fs::copy(&built, &to).map_err(|e| format!("copying {} to {}: {e}", built.display(), to.display()))?;
    }
    Ok(())
}

/// Runs the nested build and returns the binary it made.
fn compile(
    root: &Path,
    arch: &str,
    target_dir: &Path,
    cargo: OsString,
    package: &str,
    given_var: &str,
) -> Result<PathBuf, String> {
    let triple = format!("{arch}-unknown-linux-musl");
    let linker = format!(
        "CARGO_TARGET_{}_LINKER",
        triple.to_ascii_uppercase().replace('-', "_")
    );
    // A test build's guests as its own code is built, without fat LTO (Cargo.toml,
    // `test-release`); every other build's, small and stripped.
    let profile = if profile()? == "test-release" {
        "guest-test"
    } else {
        "guest"
    };
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .args(["build", "--locked", "--package", package, "--profile", profile])
        .args(["--target", &triple, "--target-dir"])
        .arg(target_dir)
        .env(linker, "rust-lld")
        // The version the in-VM server says it is (MCP's `serverInfo`): shards', whose part
        // it is.
        .env("SHARDS_VERSION", env!("CARGO_PKG_VERSION"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // The outer build's flags and tools are for its own target (cargo-xwin names clang-cl
    // for every target as TARGET_CC). Its library path stays: through rustup it holds the
    // toolchain's, where rust-lld finds libLLVM on macOS.
    for name in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "TARGET_CC",
        "TARGET_CXX",
        "TARGET_AR",
        "TARGET_CFLAGS",
        "TARGET_CXXFLAGS",
    ] {
        command.env_remove(name);
    }
    // Its bytes name a template (run.rs, `template`), so they must not depend on where
    // the checkout or cargo's home is: the source paths rustc embeds, in panic locations
    // among others, are remapped to fixed names (rustc `--remap-path-prefix`). Separated by
    // 0x1f, as CARGO_ENCODED_RUSTFLAGS takes them, so a path may hold spaces.
    let mut remaps = vec![format!("--remap-path-prefix={}=/shards", root.display())];
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")));
    if let Some(home) = cargo_home {
        remaps.push(format!("--remap-path-prefix={}=/cargo", home.display()));
    }
    command.env("CARGO_ENCODED_RUSTFLAGS", remaps.join("\x1f"));
    // AWS-LC's C, for the TLS of the in-VM server (AGENTFILE_ARCH.md §12 answer 18), by
    // zig on every host (scripts/zig-cc, which drops the Rust triple cc-rs passes): one
    // compiler, so that init's bytes, which name its template, are the same wherever it
    // is built; its paths remapped as the Rust's are (`-ffile-prefix-map`). A compiler
    // named for the target already is used instead.
    let var = triple.replace('-', "_");
    if std::env::var_os(format!("CC_{var}")).is_none() {
        let tool = |name: &str| {
            let script = root.join("scripts").join(name);
            if cfg!(windows) {
                format!("sh {}", sh_path(&script))
            } else {
                script.display().to_string()
            }
        };
        command
            .env(format!("CC_{var}"), tool("zig-cc"))
            .env(format!("AR_{var}"), tool("zig-ar"))
            .env("ZIG_TARGET", format!("{arch}-linux-musl"));
    }
    let maps: Vec<String> = remaps
        .iter()
        .filter_map(|r| r.strip_prefix("--remap-path-prefix="))
        .map(|m| format!("-ffile-prefix-map={m}"))
        .collect();
    command.env(format!("CFLAGS_{var}"), maps.join(" "));
    let output = command.output().map_err(|e| format!("building {package}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let mut message = format!("building {package} for {triple} failed");
        if stderr.contains("E0463") {
            message.push_str(&format!(
                "\nthe Rust standard library for {triple} is missing: rustup target add {triple}"
            ));
        }
        if stderr.contains("aws-lc") || stderr.contains("zig") {
            message.push_str(&format!(
                "\nits C (AWS-LC) is compiled by zig 0.16.0: install it (https://ziglang.org/download/, \
                 `brew install zig`), or name a compiler with CC_{var}"
            ));
        }
        message.push_str(&format!("\nor name a prebuilt one with {given_var}"));
        for line in tail.iter().rev() {
            message.push('\n');
            message.push_str(line);
        }
        return Err(message);
    }
    let binary = target_dir.join(&triple).join(profile).join(package);
    if !binary.is_file() {
        return Err(format!("building {package} made no {}", binary.display()));
    }
    Ok(binary)
}

/// The helpers `shards` carries, in `OUT_DIR/helpers.rs`: each one's bytes and SHA-256.
fn helpers() -> Result<(), String> {
    let mut out = io::stdout().lock();
    for name in ["SHARDS_HELPERS", "SHARDS_HELPERS_DIR"] {
        let _ = writeln!(out, "cargo::rerun-if-env-changed={name}");
    }
    let var = |name: &str| env::var_os(name).ok_or_else(|| format!("{name} is not set"));
    let out_dir = PathBuf::from(var("OUT_DIR")?);
    let exe = if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows") {
        ".exe"
    } else {
        ""
    };
    let names = ["shards-vm", "shards-net"];
    let skip = env::var("SHARDS_HELPERS").is_ok_and(|v| v == "skip");
    let _ = writeln!(out, "cargo::rerun-if-env-changed=SHARDS_GPU_LIBS");
    let built: Option<PathBuf> = if skip {
        None
    } else if let Some(dir) = env::var_os("SHARDS_HELPERS_DIR").filter(|d| !d.is_empty()) {
        let dir = PathBuf::from(dir);
        if !dir.is_absolute() {
            return Err(format!(
                "SHARDS_HELPERS_DIR: {} is not an absolute path",
                dir.display()
            ));
        }
        for name in names {
            let _ = writeln!(
                out,
                "cargo::rerun-if-changed={}",
                dir.join(format!("{name}{exe}")).display()
            );
        }
        Some(dir)
    } else {
        Some(compile_helpers(&mut out)?)
    };
    let dir = out_dir.join("helpers");
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut code = String::from("// What `shards` carries: written by build.rs.\n");
    code.push_str(&format!("pub const PRESENT: bool = {};\n", built.is_some()));
    for name in names {
        let to = dir.join(format!("{name}{exe}"));
        match &built {
            Some(from) => {
                let from = from.join(format!("{name}{exe}"));
                fs::copy(&from, &to).map_err(|e| format!("copying {}: {e}", from.display()))?;
                sign(name, &to)?;
            }
            None => fs::write(&to, b"").map_err(|e| format!("{}: {e}", to.display()))?,
        }
        let bytes = fs::read(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        let digest: String = {
            use sha2::Digest as _;
            sha2::Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let ident = name.replace('-', "_").to_ascii_uppercase();
        code.push_str(&format!(
            "pub const {ident}: &[u8] = include_bytes!({:?});\npub const {ident}_SHA256: &str = {digest:?};\n",
            to.display().to_string()
        ));
    }
    // The GPU's VM process: SHARDS_HELPERS_DIR's where it has one, else built where the
    // renderer's libraries are given, else none (an empty file).
    let gpu: Option<PathBuf> = match &built {
        None => None,
        Some(dir) if env::var_os("SHARDS_HELPERS_DIR").is_some_and(|d| !d.is_empty()) => {
            let given = dir.join(format!("shards-vm-gpu{exe}"));
            given.is_file().then_some(given)
        }
        Some(_) => match env::var_os("SHARDS_GPU_LIBS").filter(|d| !d.is_empty()) {
            Some(libs) if var("TARGET")?.to_str().is_some_and(gpu_target) => {
                Some(compile_gpu_helper(&mut out, &PathBuf::from(libs))?)
            }
            _ => None,
        },
    };
    let to = dir.join("shards-vm-gpu");
    match &gpu {
        Some(from) => {
            fs::copy(from, &to).map_err(|e| format!("copying {}: {e}", from.display()))?;
            sign("shards-vm-gpu", &to)?;
        }
        None => fs::write(&to, b"").map_err(|e| format!("{}: {e}", to.display()))?,
    }
    let bytes = fs::read(&to).map_err(|e| format!("{}: {e}", to.display()))?;
    let digest: String = {
        use sha2::Digest as _;
        sha2::Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    code.push_str(&format!(
        "pub const SHARDS_VM_GPU: &[u8] = include_bytes!({:?});\npub const SHARDS_VM_GPU_SHA256: &str = {digest:?};\n",
        to.display().to_string()
    ));
    fs::write(out_dir.join("helpers.rs"), code).map_err(|e| format!("helpers.rs: {e}"))?;
    // For the package's tests and benchmarks: the copies carried, signed as they are.
    let _ = writeln!(out, "cargo::rustc-env=SHARDS_HELPERS_CARRIED={}", dir.display());
    Ok(())
}

/// Builds the helpers with a nested cargo, for this target and profile, into
/// `target/helpers`; returns the directory holding them. On Linux they are static, for
/// the target's architecture with musl, whatever C library `shards` itself links: no
/// shared library is mapped into every VM process, as Firecracker ships, which kept glibc's
/// pages resident in each (1.1 to 1.9 MiB more, PM M281), and they start sooner. musl's
/// targets are linked by rust-lld, as the init is: nothing of the host's toolchain.
fn compile_helpers(out: &mut impl io::Write) -> Result<PathBuf, String> {
    let var = |name: &str| env::var(name).map_err(|e| format!("{name}: {e}"));
    let root = Path::new(&var("CARGO_MANIFEST_DIR")?).join("..").join("..");
    // Every crate of the workspace's the helpers are built of, on any target (`cargo tree
    // -p shards-vm-process -p shards-net-process --target ...`), and the sources of shards'
    // own they include by path.
    for input in [
        "crates/vm-process",
        "crates/net-process",
        "crates/afunix",
        "crates/apple",
        "crates/vmm",
        "crates/net",
        "crates/netring",
        "crates/dns",
        "crates/ipc",
        "crates/abi",
        "crates/cmdline",
        "crates/syscall-filter",
        "crates/template",
        "crates/shards/src/confine.rs",
        "crates/shards/src/grant.rs",
        "crates/shards/src/grant_ask.rs",
        "crates/shards/src/segments.rs",
        "crates/shards/src/spec.rs",
        "crates/shards/src/terminal.rs",
        "crates/shards/src/vm_run.rs",
        "crates/shards/src/warm.rs",
        "crates/shards/src/workload.rs",
        "resources/vm-Info.plist",
        "resources/net-Info.plist",
        "resources/vm.entitlements",
        "resources/net.entitlements",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        let _ = writeln!(out, "cargo::rerun-if-changed={}", root.join(input).display());
    }
    let built_for = var("TARGET")?;
    let target = built_for
        .strip_suffix("-unknown-linux-gnu")
        .map_or_else(|| built_for.clone(), |arch| format!("{arch}-unknown-linux-musl"));
    let profile = profile()?;
    let dir_name = if profile == "dev" {
        "debug"
    } else {
        profile.as_str()
    };
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
        .join("helpers");
    let mut command = Command::new(var("CARGO")?);
    if target.ends_with("-unknown-linux-musl") {
        command.env(
            format!(
                "CARGO_TARGET_{}_LINKER",
                target.to_ascii_uppercase().replace('-', "_")
            ),
            "rust-lld",
        );
    }
    command
        .current_dir(&root)
        .args([
            "build",
            "--locked",
            "-p",
            "shards-vm-process",
            "-p",
            "shards-net-process",
            "--profile",
        ])
        .arg(&profile)
        .args(["--target", &target, "--target-dir"])
        .arg(&target_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .map_err(|e| format!("building the VM and network processes: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let mut message = format!("building the VM and network processes for {target} failed");
        message.push_str("\nor name prebuilt ones with SHARDS_HELPERS_DIR");
        for line in tail.iter().rev() {
            message.push('\n');
            message.push_str(line);
        }
        return Err(message);
    }
    Ok(target_dir.join(&target).join(dir_name))
}

/// Whether `target` carries a GPU's VM process where its renderer's libraries are given
/// (D144, D201, D202): Apple silicon's, Linux's with glibc, which a GPU's VM process links,
/// dynamic, as it opens the host's Vulkan driver, a library of the host's C library (on
/// Linux the other helpers are static, PM M281), and x86_64 Windows's, whose VMs WHP
/// runs.
fn gpu_target(target: &str) -> bool {
    matches!(
        target,
        "aarch64-apple-darwin"
            | "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-pc-windows-msvc"
    )
}

/// Builds the GPU's VM process (D144, D201): the VM process with its renderer, linked
/// from the libraries in `libs`, for this profile into `target/helpers-gpu`; returns its
/// binary.
fn compile_gpu_helper(out: &mut impl io::Write, libs: &Path) -> Result<PathBuf, String> {
    let var = |name: &str| env::var(name).map_err(|e| format!("{name}: {e}"));
    if !libs.is_absolute() {
        return Err(format!(
            "SHARDS_GPU_LIBS: {} is not an absolute path",
            libs.display()
        ));
    }
    let os = var("CARGO_CFG_TARGET_OS")?;
    let given: &[&str] = match os.as_str() {
        "macos" => &["libvirglrenderer.a", "libepoxy.a", "libMoltenVK.a"],
        "windows" => &["virglrenderer.lib", "epoxy.lib"],
        _ => &["libvirglrenderer.a", "librender_server.a", "libepoxy.a"],
    };
    for lib in given {
        let _ = writeln!(out, "cargo::rerun-if-changed={}", libs.join(lib).display());
    }
    let root = Path::new(&var("CARGO_MANIFEST_DIR")?).join("..").join("..");
    let target = var("TARGET")?;
    let profile = profile()?;
    let dir_name = if profile == "dev" {
        "debug"
    } else {
        profile.as_str()
    };
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
        .join("helpers-gpu");
    let output = Command::new(var("CARGO")?)
        .current_dir(&root)
        .args([
            "build",
            "--locked",
            "-p",
            "shards-vm-process",
            "--features",
            "gpu",
            "--profile",
        ])
        .arg(&profile)
        .args(["--target", &target, "--target-dir"])
        .arg(&target_dir)
        .env("SHARDS_GPU_LIBS", libs)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("building the GPU's VM process: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let mut message = format!("building the GPU's VM process for {target} failed");
        for line in tail.iter().rev() {
            message.push('\n');
            message.push_str(line);
        }
        return Err(message);
    }
    let exe = if os == "windows" { ".exe" } else { "" };
    Ok(target_dir
        .join(&target)
        .join(dir_name)
        .join(format!("shards-vm{exe}")))
}

/// The profile this build runs under, by its name: OUT_DIR is
/// `<target>/[<triple>/]<profile>/build/<package>/out`, its directory `debug` for `dev`.
/// PROFILE says only `release` or `debug`, for every profile that inherits from either
/// (Cargo's reference, "Environment variables Cargo sets for build scripts"), and so not
/// `test-release`.
fn profile() -> Result<String, String> {
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let dir = out
        .ancestors()
        .nth(3)
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("no profile in {}", out.display()))?;
    Ok(if dir == "debug" { "dev" } else { dir }.to_string())
}

/// On macOS, signs a helper as releases are: the VM process in App Sandbox, with the
/// hypervisor entitlement and Hardened Runtime (resources/vm.entitlements); the network
/// process in App Sandbox with its network's client and server and no file
/// (resources/net.entitlements, D31).
fn sign(name: &str, path: &Path) -> Result<(), String> {
    if !env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") || !cfg!(target_os = "macos") {
        return Ok(());
    }
    let resources =
        Path::new(&env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?).join("../../resources");
    let mut command = Command::new("codesign");
    let entitlements = match name {
        "shards-vm" | "shards-vm-gpu" => Some("vm.entitlements"),
        "shards-net" => Some("net.entitlements"),
        _ => None,
    };
    if let Some(file) = entitlements {
        command
            .arg("--entitlements")
            .arg(resources.join(file))
            .args(["-o", "runtime"]);
    }
    let status = command
        .args(["--force", "-s", "-"])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("codesign {}: {e}", path.display()))?;
    if !status.success() {
        return Err(format!("codesign {} failed", path.display()));
    }
    Ok(())
}

/// `path` as a POSIX shell on Windows (Git's, MSYS) takes it: forward slashes, and no
/// verbatim prefix (`\\?\`), whose backslashes it reads as escapes.
fn sh_path(path: &Path) -> String {
    let text = path.display().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).replace('\\', "/")
}

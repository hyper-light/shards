//! Builds what `shards` carries inside it, so that one binary is all there is to install
//! (docs/design/architecture.md D36):
//! - shards-init, the guest's PID 1, for this target's architecture, so that `shards`
//!   carries the init its guests run and the two always match (D28).
//!   `SHARDS_INIT_BINARY` names a prebuilt one instead.
//! - The VM process (`shards-vm`) and the network process (`shards-net`), which `shards`
//!   writes out once per build and starts (src/helpers.rs). Built by a nested cargo for
//!   this target and profile into `target/helpers`, kept between builds; on macOS the VM
//!   process signed with its App Sandbox entitlements first, so that its signature travels
//!   inside `shards`. `SHARDS_HELPERS_DIR` names prebuilt ones; `SHARDS_HELPERS=skip`
//!   carries none, for check builds of other targets (scripts/lint), and such a `shards`
//!   refuses what needs them.
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
    let _ = writeln!(out, "cargo::rerun-if-env-changed=SHARDS_INIT_BINARY");
    // Only Unix hosts run VMs from `shardsd` yet.
    if env::var("CARGO_CFG_TARGET_FAMILY").is_ok_and(|f| !f.split(',').any(|f| f == "unix")) {
        return Ok(());
    }
    let var = |name: &str| env::var_os(name).ok_or_else(|| format!("{name} is not set"));
    let out_dir = PathBuf::from(var("OUT_DIR")?);
    let init = match env::var_os("SHARDS_INIT_BINARY").filter(|p| !p.is_empty()) {
        Some(given) => {
            let path = PathBuf::from(given);
            // A build script runs in its package's directory, not where cargo was run.
            if !path.is_absolute() || !path.is_file() {
                return Err(format!(
                    "SHARDS_INIT_BINARY: {} is not an absolute path to a file",
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
            for input in [
                "crates/init",
                "crates/abi",
                "crates/cmdline",
                "Cargo.toml",
                "Cargo.lock",
            ] {
                let _ = writeln!(out, "cargo::rerun-if-changed={}", root.join(input).display());
            }
            compile(&root, &arch, &out_dir.join("init"), var("CARGO")?)?
        }
    };
    let to = out_dir.join("shards-init");
    fs::copy(&init, &to).map_err(|e| format!("copying {} to {}: {e}", init.display(), to.display()))?;
    Ok(())
}

/// Runs the nested build and returns the binary it made.
fn compile(root: &Path, arch: &str, target_dir: &Path, cargo: OsString) -> Result<PathBuf, String> {
    let triple = format!("{arch}-unknown-linux-musl");
    let linker = format!(
        "CARGO_TARGET_{}_LINKER",
        triple.to_ascii_uppercase().replace('-', "_")
    );
    let mut command = Command::new(cargo);
    command
        .current_dir(root)
        .args([
            "build",
            "--locked",
            "--package",
            "shards-init",
            "--profile",
            "guest",
        ])
        .args(["--target", &triple, "--target-dir"])
        .arg(target_dir)
        .env(linker, "rust-lld")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // The outer build's flags are for its own target. Its library path stays: through
    // rustup it holds the toolchain's, where rust-lld finds libLLVM on macOS.
    for name in ["CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS"] {
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
    let output = command
        .output()
        .map_err(|e| format!("building shards-init: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let mut message = format!("building shards-init for {triple} failed");
        if stderr.contains("E0463") {
            message.push_str(&format!(
                "\nthe Rust standard library for {triple} is missing: rustup target add {triple}"
            ));
        }
        message.push_str("\nor name a prebuilt one with SHARDS_INIT_BINARY");
        for line in tail.iter().rev() {
            message.push('\n');
            message.push_str(line);
        }
        return Err(message);
    }
    let binary = target_dir.join(&triple).join("guest").join("shards-init");
    if !binary.is_file() {
        return Err(format!("building shards-init made no {}", binary.display()));
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
    fs::write(out_dir.join("helpers.rs"), code).map_err(|e| format!("helpers.rs: {e}"))?;
    // For the package's tests and benchmarks: the copies carried, signed as they are.
    let _ = writeln!(out, "cargo::rustc-env=SHARDS_HELPERS_CARRIED={}", dir.display());
    Ok(())
}

/// Builds the helpers with a nested cargo, for this target and profile, into
/// `target/helpers`; returns the directory holding them.
fn compile_helpers(out: &mut impl io::Write) -> Result<PathBuf, String> {
    let var = |name: &str| env::var(name).map_err(|e| format!("{name}: {e}"));
    let root = Path::new(&var("CARGO_MANIFEST_DIR")?).join("..").join("..");
    for input in [
        "crates/vm-process",
        "crates/net-process",
        "crates/vmm",
        "crates/net",
        "crates/netring",
        "crates/ipc",
        "crates/abi",
        "crates/cmdline",
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
        "resources/vm.entitlements",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        let _ = writeln!(out, "cargo::rerun-if-changed={}", root.join(input).display());
    }
    let target = var("TARGET")?;
    // Cargo names the dev profile `debug` here; any other is its own name.
    let profile = match var("PROFILE")?.as_str() {
        "debug" => "dev".to_string(),
        other => other.to_string(),
    };
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

/// On macOS, signs a helper as releases are: the VM process in App Sandbox, with the
/// hypervisor entitlement and Hardened Runtime (resources/vm.entitlements); the network
/// process with none.
fn sign(name: &str, path: &Path) -> Result<(), String> {
    if !env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") || !cfg!(target_os = "macos") {
        return Ok(());
    }
    let resources =
        Path::new(&env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?).join("../../resources");
    let mut command = Command::new("codesign");
    if name == "shards-vm" {
        command
            .arg("--entitlements")
            .arg(resources.join("vm.entitlements"))
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

//! Builds shards-init, the guest's PID 1, for this target's architecture, so that
//! `shardsd` carries the init its guests run and the two always match
//! (docs/design/architecture.md D28). `SHARDS_INIT_BINARY` names a prebuilt one instead.
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
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "cargo::rerun-if-env-changed=SHARDS_INIT_BINARY");
    // shards-vm runs in App Sandbox on macOS, which takes a tool's identity from an
    // Info.plist in its __TEXT,__info_plist section: without one, it is killed at launch
    // (docs/research/macos-confinement.md §2).
    if env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") {
        let manifest =
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?);
        let plist = manifest
            .join("..")
            .join("..")
            .join("resources")
            .join("vm-Info.plist");
        let plist = plist
            .canonicalize()
            .map_err(|e| format!("{}: {e}", plist.display()))?;
        let _ = writeln!(out, "cargo::rerun-if-changed={}", plist.display());
        let _ = writeln!(
            out,
            "cargo::rustc-link-arg-bin=shards-vm=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
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

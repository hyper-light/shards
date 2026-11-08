//! The identity of this contract: a 64-bit FNV-1a hash of every source file of this crate,
//! by path, so that any change to what the host and the guest share gives another
//! (docs/design/architecture.md D28). shards-init announces it on the control page, and the
//! host refuses to hand a workload to an init that announced another.
//!
//! `SHARDS_ABI_IDENTITY` (hexadecimal) overrides it, so tests can build an init that
//! speaks for another shards.

use std::env;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

fn main() {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "cargo::rerun-if-changed=src");
    let _ = writeln!(out, "cargo::rerun-if-env-changed=SHARDS_ABI_IDENTITY");
    match identity() {
        Ok(identity) => {
            if let Err(e) = write(identity) {
                let _ = writeln!(out, "cargo::error={e}");
            }
        }
        Err(e) => {
            let _ = writeln!(out, "cargo::error={e}");
        }
    }
}

fn identity() -> Result<u64, String> {
    if let Some(given) = env::var("SHARDS_ABI_IDENTITY").ok().filter(|v| !v.is_empty()) {
        let digits = given.trim_start_matches("0x");
        return u64::from_str_radix(digits, 16).map_err(|e| format!("SHARDS_ABI_IDENTITY {given:?}: {e}"));
    }
    let src =
        Path::new(&env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is not set")?).join("src");
    let mut files = Vec::new();
    collect(&src, &mut files).map_err(|e| format!("{}: {e}", src.display()))?;
    files.sort();
    let mut hash = OFFSET;
    for file in &files {
        let relative = file
            .strip_prefix(&src)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let contents = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        for byte in relative.bytes().chain([0]).chain(contents).chain([0]) {
            hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
        }
    }
    Ok(hash)
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, files)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn write(identity: u64) -> Result<(), String> {
    let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?).join("identity.rs");
    let text = format!(
        "/// The identity of this contract, from its sources (build.rs). The host refuses a guest\n\
         /// whose init announced another on the control page ([`control::ABI`]).\n\
         pub const IDENTITY: u64 = {identity:#018x};\n"
    );
    fs::write(&out, text).map_err(|e| format!("{}: {e}", out.display()))
}

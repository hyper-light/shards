//! On macOS the VM process runs in App Sandbox, which takes a tool's identity from an
//! Info.plist in its __TEXT,__info_plist section: without one it is killed at launch
//! (docs/research/macos-confinement.md §2).

use std::io::Write as _;
use std::path::PathBuf;

fn main() {
    let mut out = std::io::stdout().lock();
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") {
        let Some(manifest) = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
            let _ = writeln!(out, "cargo::error=CARGO_MANIFEST_DIR is not set");
            return;
        };
        let plist = manifest.join("../../resources/vm-Info.plist");
        match plist.canonicalize() {
            Ok(plist) => {
                let _ = writeln!(out, "cargo::rerun-if-changed={}", plist.display());
                let _ = writeln!(
                    out,
                    "cargo::rustc-link-arg-bin=shards-vm=-Wl,-sectcreate,__TEXT,__info_plist,{}",
                    plist.display()
                );
            }
            Err(e) => {
                let _ = writeln!(out, "cargo::error={}: {e}", plist.display());
            }
        }
    }
}

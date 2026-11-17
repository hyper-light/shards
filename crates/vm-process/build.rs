//! On macOS the VM process runs in App Sandbox, which takes a tool's identity from an
//! Info.plist in its __TEXT,__info_plist section: without one it is killed at launch
//! (docs/research/macos-confinement.md §2).
//!
//! On Linux its relative relocations are packed (DT_RELR, D36): its static-PIE start reads
//! every one before `main`, and what it reads stays resident, file-backed, in every VM. Where
//! an order is kept for its architecture, the functions its boot runs are laid out first,
//! together (D36): a read fault maps the 64 KiB of cached pages around it, so a boot whose
//! code lies scattered keeps nearly every page of its binary.

use std::io::Write as _;
use std::path::{Path, PathBuf};

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
    if packs_relocations() {
        let _ = writeln!(out, "cargo::rustc-link-arg-bin=shards-vm=--pack-dyn-relocs=relr");
        if let Some(order) = boot_order() {
            // Named only where it is there: a path that is not makes every build run this
            // script again.
            let _ = writeln!(out, "cargo::rerun-if-changed={}", order.display());
            let _ = writeln!(
                out,
                "cargo::rustc-link-arg-bin=shards-vm=--symbol-ordering-file={}",
                order.display()
            );
            // A symbol it names that this build no longer has is passed over.
            let _ = writeln!(
                out,
                "cargo::rustc-link-arg-bin=shards-vm=--no-warn-symbol-ordering"
            );
        }
    }
}

/// The order kept for this architecture's VM process, `order/<arch>.txt` beside this script:
/// the functions on the pages a boot touched, by their symbols, made by
/// docs/research/measurements/vm-text/order.py, for LLD's `--symbol-ordering-file`. None
/// where none is kept.
fn boot_order() -> Option<PathBuf> {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")?);
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").ok()?;
    let order = manifest.join("order").join(format!("{arch}.txt"));
    order.is_file().then_some(order)
}

/// Whether the link packs the VM process's relative relocations: musl's static PIE, whose
/// start (rcrt1.o's `_dlstart_c`) applies DT_RELR, linked by rust-lld itself, which takes
/// LLD's own option, as .cargo/config.toml and crates/shards/build.rs name it (D36); not
/// the GPU's VM process, which links its host's C library through the host's C compiler
/// (D201).
fn packs_relocations() -> bool {
    let is = |name: &str, value: &str| std::env::var(name).is_ok_and(|v| v == value);
    let rust_lld = std::env::var_os("RUSTC_LINKER").is_some_and(|linker| {
        Path::new(&linker)
            .file_stem()
            .is_some_and(|stem| stem == "rust-lld")
    });
    is("CARGO_CFG_TARGET_OS", "linux")
        && is("CARGO_CFG_TARGET_ENV", "musl")
        && std::env::var_os("CARGO_FEATURE_GPU").is_none()
        && rust_lld
}

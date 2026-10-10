//! What a package manager's cache costs on a filesystem (platform-measurements.md M140):
//! many small files made, read and moved, then large ones written and read, each phase
//! timed by CLOCK_MONOTONIC. Run in a Linux guest, built statically:
//!
//!     rustc --edition 2024 -O --target aarch64-unknown-linux-musl -C linker=rust-lld fsbench.rs
//!     fsbench REPS DIR...
//!     fsbench fill DIR
//!     fsbench reread REPS DIR...
//!
//! Each repetition runs every phase in each DIR, the DIRs taken in turn from a different
//! one each time, so that no medium always goes first. A line for each phase:
//! `DIR<TAB>PHASE<TAB>NANOSECONDS`. `fill` leaves a cache's worth of files in DIR/fill (the
//! small files and the large); `reread` reads them back in each DIR, filling first any
//! DIR that has none, and saying so (`DIR<TAB>filled<TAB>0`): a cache a build left,
//! against one in the step's own memory.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Instant;

/// Small files: a Go module cache's or npm's, many of a few KiB in many directories.
const SMALL: usize = 10_000;
const SMALL_BYTES: usize = 4096;
const DIRS: usize = 100;
/// Large files: a pip wheel's or an apt package's, written and read a MiB at a time.
const LARGE: usize = 2;
const LARGE_MIB: usize = 128;

fn phase(dir: &str, name: &str, f: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    let t = Instant::now();
    f()?;
    println!("{dir}\t{name}\t{}", t.elapsed().as_nanos());
    Ok(())
}

fn run(dir: &str) -> std::io::Result<()> {
    let root = Path::new(dir).join("fsbench");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root)?;
    let file = |i: usize| root.join(format!("d{}", i % DIRS)).join(format!("f{i}"));
    let data = vec![0x5au8; SMALL_BYTES];
    phase(dir, "create", || {
        for d in 0..DIRS {
            fs::create_dir(root.join(format!("d{d}")))?;
        }
        for i in 0..SMALL {
            File::create(file(i))?.write_all(&data)?;
        }
        Ok(())
    })?;
    phase(dir, "stat", || {
        for i in 0..SMALL {
            fs::symlink_metadata(file(i))?;
        }
        Ok(())
    })?;
    let mut buf = vec![0u8; 1 << 20];
    phase(dir, "read", || {
        for i in 0..SMALL {
            let mut f = File::open(file(i))?;
            while f.read(&mut buf)? > 0 {}
        }
        Ok(())
    })?;
    phase(dir, "rename", || {
        for i in 0..SMALL {
            let from = file(i);
            fs::rename(&from, from.with_extension("r"))?;
        }
        Ok(())
    })?;
    phase(dir, "delete", || {
        for i in 0..SMALL {
            fs::remove_file(file(i).with_extension("r"))?;
        }
        for d in 0..DIRS {
            fs::remove_dir(root.join(format!("d{d}")))?;
        }
        Ok(())
    })?;
    let chunk = vec![0xa5u8; 1 << 20];
    phase(dir, "bigwrite", || {
        for i in 0..LARGE {
            let mut f = File::create(root.join(format!("big{i}")))?;
            for _ in 0..LARGE_MIB {
                f.write_all(&chunk)?;
            }
        }
        Ok(())
    })?;
    phase(dir, "bigread", || {
        for i in 0..LARGE {
            let mut f = File::open(root.join(format!("big{i}")))?;
            while f.read(&mut buf)? > 0 {}
        }
        Ok(())
    })?;
    fs::remove_dir_all(&root)
}

/// The small files and the large ones, left in DIR/fill.
fn fill(dir: &str) -> std::io::Result<()> {
    let root = Path::new(dir).join("fill");
    fs::create_dir(&root)?;
    let data = vec![0x5au8; SMALL_BYTES];
    for d in 0..DIRS {
        fs::create_dir(root.join(format!("d{d}")))?;
    }
    for i in 0..SMALL {
        File::create(root.join(format!("d{}", i % DIRS)).join(format!("f{i}")))?.write_all(&data)?;
    }
    let chunk = vec![0xa5u8; 1 << 20];
    for i in 0..LARGE {
        let mut f = File::create(root.join(format!("big{i}")))?;
        for _ in 0..LARGE_MIB {
            f.write_all(&chunk)?;
        }
    }
    Ok(())
}

/// The files `fill` left, each looked up and read.
fn reread(dir: &str) -> std::io::Result<()> {
    let root = Path::new(dir).join("fill");
    let file = |i: usize| root.join(format!("d{}", i % DIRS)).join(format!("f{i}"));
    phase(dir, "restat", || {
        for i in 0..SMALL {
            fs::symlink_metadata(file(i))?;
        }
        Ok(())
    })?;
    let mut buf = vec![0u8; 1 << 20];
    phase(dir, "reread", || {
        for i in 0..SMALL {
            let mut f = File::open(file(i))?;
            while f.read(&mut buf)? > 0 {}
        }
        Ok(())
    })?;
    phase(dir, "bigreread", || {
        for i in 0..LARGE {
            let mut f = File::open(root.join(format!("big{i}")))?;
            while f.read(&mut buf)? > 0 {}
        }
        Ok(())
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fail = |dir: &str, e: std::io::Error| -> ! {
        eprintln!("fsbench: {dir}: {e}");
        std::process::exit(1);
    };
    let (mode, rest): (&str, &[String]) = match args.first().map(String::as_str) {
        Some(m @ ("fill" | "reread")) => (m, args.get(1..).unwrap_or_default()),
        _ => ("run", &args),
    };
    if mode == "fill" {
        for dir in rest {
            if let Err(e) = fill(dir) {
                fail(dir, e);
            }
        }
        return;
    }
    let (Some(reps), dirs) = (rest.first().and_then(|r| r.parse::<usize>().ok()), rest.get(1..).unwrap_or_default()) else {
        eprintln!("usage: fsbench REPS DIR... | fill DIR... | reread REPS DIR...");
        std::process::exit(2);
    };
    if mode == "reread" {
        for dir in dirs {
            if !Path::new(dir).join("fill").exists() {
                println!("{dir}\tfilled\t0");
                if let Err(e) = fill(dir) {
                    fail(dir, e);
                }
            }
        }
    }
    for rep in 0..reps {
        for k in 0..dirs.len() {
            let Some(dir) = dirs.get((rep + k) % dirs.len()) else {
                continue;
            };
            let done = if mode == "reread" { reread(dir) } else { run(dir) };
            if let Err(e) = done {
                fail(dir, e);
            }
        }
    }
}

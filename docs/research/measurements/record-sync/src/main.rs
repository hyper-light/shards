//! What publishing a container's record costs at each level of durability (audit A15,
//! PM M46). A record of 420 bytes, as `containers/ID/config.json` holds, is written to a
//! temporary sibling and renamed over the last, in a directory of its own per level. Each
//! level runs alone, in blocks of N/2, in two rounds of opposite order: a drive flush one
//! level asks for delays the writes of the next, so levels that take turns publish by
//! publish measure each other.
//!
//! - `rename`: written and renamed; whole after a process crash.
//! - `fsync, rename`: fsync(2) first. On Linux that is the device's flush too; on macOS it
//!   is not, and it orders nothing against later writes (fsync(2), macOS).
//! - `barrier, rename` (macOS): `F_BARRIERFSYNC` first: the data before the rename
//!   (fcntl(2), macOS).
//! - `full, rename`: `F_FULLFSYNC` first on macOS, fdatasync(2) on Linux: on stable storage.
//! - `full, rename, dir`: and the directory synced after, so the rename is too.
//! - `create, dir`: a new directory and its first record, synced, then its parent.
//! - `remove`: a directory renamed aside, removed, and its parent synced.

use std::fs::{self, File};
use std::io::Write as _;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RECORD: usize = 420;

#[derive(Clone, Copy, PartialEq)]
enum Sync {
    None,
    Fsync,
    #[cfg(target_os = "macos")]
    Barrier,
    Full,
}

fn sync(file: &File, how: Sync) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fsync(2), fdatasync(2) and fcntl(2) on a descriptor we own.
    let r = unsafe {
        match how {
            Sync::None => 0,
            Sync::Fsync => libc::fsync(fd),
            #[cfg(target_os = "macos")]
            Sync::Barrier => libc::fcntl(fd, libc::F_BARRIERFSYNC),
            #[cfg(target_os = "macos")]
            Sync::Full => libc::fcntl(fd, libc::F_FULLFSYNC),
            #[cfg(not(target_os = "macos"))]
            Sync::Full => libc::fdatasync(fd),
        }
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn sync_dir(dir: &Path, how: Sync) -> std::io::Result<()> {
    sync(&File::open(dir)?, how)
}

/// Writes `bytes` to `dir/config.json` through a temporary sibling, synced by `how`.
fn publish(dir: &Path, bytes: &[u8], how: Sync) -> std::io::Result<()> {
    let tmp = dir.join("config.json.new");
    let mut f = File::create(&tmp)?;
    f.write_all(bytes)?;
    sync(&f, how)?;
    drop(f);
    fs::rename(&tmp, dir.join("config.json"))
}

struct Level {
    name: &'static str,
    samples: Vec<Duration>,
    run: Box<dyn FnMut(usize) -> std::io::Result<()>>,
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).map_or("/tmp/record-sync", String::as_str));
    let n: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(1000);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    let record = {
        let mut r = br#"{"id":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","name":"focused_turing","image":"127.0.0.1:5000/test/image:v1","command":["/bin/testguest","report"],"created":1759200000000000000,"state":"running","started":1759200000001000000,"finished":null,"exit_code":null,"auto_remove":false}"#.to_vec();
        r.resize(RECORD, b' ');
        r
    };
    let full = Sync::Full;
    let mut levels: Vec<Level> = Vec::new();
    let mut level = |name: &'static str, run: Box<dyn FnMut(usize) -> std::io::Result<()>>| {
        levels.push(Level { name, samples: Vec::with_capacity(n), run });
    };
    for (name, how, dir_too) in [
        ("rename", Sync::None, false),
        ("fsync, rename", Sync::Fsync, false),
        #[cfg(target_os = "macos")]
        ("barrier, rename", Sync::Barrier, false),
        ("full, rename", Sync::Full, false),
        ("full, rename, dir", Sync::Full, true),
    ] {
        let dir = root.join(name.replace(", ", "-"));
        fs::create_dir_all(&dir)?;
        let bytes = record.clone();
        level(
            name,
            Box::new(move |_| {
                publish(&dir, &bytes, how)?;
                if dir_too { sync_dir(&dir, how) } else { Ok(()) }
            }),
        );
    }
    let parent = root.join("containers");
    fs::create_dir_all(&parent)?;
    {
        let (parent, bytes) = (parent.clone(), record.clone());
        level(
            "create, dir",
            Box::new(move |i| {
                let dir = parent.join(format!("c{i}"));
                fs::create_dir(&dir)?;
                publish(&dir, &bytes, full)?;
                sync_dir(&dir, full)?;
                sync_dir(&parent, full)
            }),
        );
    }
    {
        let parent = parent.clone();
        level(
            "remove",
            Box::new(move |i| {
                let dir = parent.join(format!("c{i}"));
                let aside = parent.join(format!(".c{i}.removing"));
                fs::rename(&dir, &aside)?;
                sync_dir(&parent, full)?;
                fs::remove_dir_all(&aside)
            }),
        );
    }
    let k = levels.len();
    let half = n / 2;
    for round in 0..2 {
        // Records in one order, then the other; creating before removing, always.
        let mut order: Vec<usize> = (0..k - 2).collect();
        if round == 1 {
            order.reverse();
        }
        order.extend([k - 2, k - 1]);
        for j in order {
            let l = &mut levels[j];
            for i in round * half..(round + 1) * half {
                let t0 = Instant::now();
                (l.run)(i)?;
                l.samples.push(t0.elapsed());
            }
        }
    }
    let host = std::process::Command::new("uname").arg("-srm").output()?;
    println!("host: {}", String::from_utf8_lossy(&host.stdout).trim());
    println!("dir: {}", root.display());
    println!("| Level | n | p50 | p90 | p99 | max |");
    println!("|---|---|---|---|---|---|");
    for l in &mut levels {
        l.samples.sort();
        let at = |q: f64| l.samples[((l.samples.len() as f64 - 1.0) * q).round() as usize];
        let us = |d: Duration| format!("{:.0}", d.as_secs_f64() * 1e6);
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            l.name,
            l.samples.len(),
            us(at(0.5)),
            us(at(0.9)),
            us(at(0.99)),
            us(*l.samples.last().unwrap_or(&Duration::ZERO))
        );
    }
    let _ = fs::remove_dir_all(&root);
    Ok(())
}

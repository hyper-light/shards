//! PM M623: what a killed process's flock(2) locks and its exit say of each other as its
//! kernel lets them go (D185, *A killed daemon's stop*). A daemon locks its home's lock first
//! and its stopping lock as it closes; `stop daemon` took the stopping lock let go of, or the
//! daemon's exit begun, for its home's lock let go of. Killed, does either come first?
//!
//! A child (this program, `--child`) locks `home.lock`, opens FILLER descriptors of
//! /dev/null and starts THREADS threads that sleep, then locks `stopping.lock`, says it is
//! ready and sleeps. The parent, ROUNDS times for each of two observers in turn, starts it,
//! waits for its word, and sends it SIGTERM, which it does not handle (as kiln's sweep sends a
//! daemon); then, as its observer fires, asks for `home.lock` at once, shared and not
//! waiting, counting the times it was still held:
//! - `stopping`: a thread's shared lock of `stopping.lock`, waited for from before the signal;
//! - `exit`: the child seen exiting: on macOS proc_pid_rusage's `ri_proc_exit_abstime` set
//!   (shards_ipc's `exited`); on Linux its /proc stat state Z, its other threads gone.
//!
//! rustc -O --edition 2024 main.rs -o /tmp/flock-kill && /tmp/flock-kill DIR [ROUNDS] [FILLER] [THREADS]

use std::fs::{File, TryLockError};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(target_os = "macos")]
#[repr(C)]
struct RusageInfoV0 {
    uuid: [u8; 16],
    times: [u64; 8],
    start_abstime: u64,
    exit_abstime: u64,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV0) -> i32;
}

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

const SIGTERM: i32 = 15;

/// Whether process `pid` is seen exiting, as shards_ipc's `Named::lives` sees it gone.
#[cfg(target_os = "macos")]
fn exiting(pid: i32) -> bool {
    let mut info = RusageInfoV0 {
        uuid: [0; 16],
        times: [0; 8],
        start_abstime: 0,
        exit_abstime: 0,
    };
    // SAFETY: proc_pid_rusage(3) of RUSAGE_INFO_V0 (0) into a struct of its layout.
    unsafe { proc_pid_rusage(pid, 0, &raw mut info) == 0 && info.exit_abstime != 0 }
}

#[cfg(target_os = "linux")]
fn exiting(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let mut fields = stat
        .rsplit_once(')')
        .map_or("", |(_, rest)| rest)
        .split_ascii_whitespace();
    let zombie = fields.next() == Some("Z");
    zombie
        && fields
            .nth(16)
            .and_then(|n| n.parse::<u64>().ok())
            .is_some_and(|n| n <= 1)
}

/// Whether `path`'s lock is held: asked shared, not waiting, through a file of its own.
fn held(path: &Path) -> std::io::Result<bool> {
    let file = File::open(path)?;
    match file.try_lock_shared() {
        Ok(()) => {
            file.unlock()?;
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

fn child(dir: &Path, filler: usize, threads: usize) -> std::io::Result<()> {
    let home = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("home.lock"))?;
    home.lock()?;
    let opened: Vec<File> = (0..filler)
        .map(|_| File::open("/dev/null"))
        .collect::<Result<_, _>>()?;
    for _ in 0..threads {
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        });
    }
    let stopping = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("stopping.lock"))?;
    stopping.lock()?;
    println!("ready");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
        let _ = (&home, &opened, &stopping);
    }
}

/// One round with `observer`: whether `home.lock` was held as it fired.
fn round(dir: &Path, observer: &str, filler: usize, threads: usize) -> std::io::Result<bool> {
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--child")
        .arg(dir)
        .arg(filler.to_string())
        .arg(threads.to_string())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut word = String::new();
    BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?,
    )
    .read_line(&mut word)?;
    if word.trim() != "ready" {
        return Err(std::io::Error::other(format!("child said {word:?}")));
    }
    let pid = i32::try_from(child.id()).map_err(std::io::Error::other)?;
    let home = dir.join("home.lock");
    let seen = if observer == "stopping" {
        let stopping = File::open(dir.join("stopping.lock"))?;
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| -> std::io::Result<bool> {
                stopping.lock_shared()?;
                let seen = held(&home);
                stopping.unlock()?;
                seen
            });
            // SAFETY: kill(2) of our own child, not yet reaped.
            if unsafe { kill(pid, SIGTERM) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            waiter
                .join()
                .map_err(|_| std::io::Error::other("waiter panicked"))?
        })?
    } else {
        // SAFETY: kill(2) of our own child, not yet reaped.
        if unsafe { kill(pid, SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        while !exiting(pid) {
            std::hint::spin_loop();
        }
        held(&home)?
    };
    child.wait()?;
    Ok(seen)
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |i: usize, default: usize| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(default);
    if args.first().map(String::as_str) == Some("--child") {
        let dir = args
            .get(1)
            .ok_or_else(|| std::io::Error::other("--child DIR FILLER THREADS"))?;
        return child(Path::new(dir), number(2, 0), number(3, 0));
    }
    let dir = args
        .first()
        .ok_or_else(|| std::io::Error::other("usage: flock-kill DIR [ROUNDS] [FILLER] [THREADS]"))?;
    let (rounds, filler, threads) = (number(1, 200), number(2, 0), number(3, 0));
    let mut counts = [("stopping", 0u64), ("exit", 0u64)];
    for _ in 0..rounds {
        for (observer, count) in &mut counts {
            *count += u64::from(round(Path::new(dir), observer, filler, threads)?);
        }
    }
    for (observer, count) in counts {
        println!(
            "observer={observer} rounds={rounds} filler={filler} threads={threads} home_lock_still_held={count}"
        );
    }
    Ok(())
}

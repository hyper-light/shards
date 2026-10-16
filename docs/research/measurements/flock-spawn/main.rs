//! PM M244: whether a flock(2) lock outlives the close of its descriptor while another
//! thread of the process starts a program (std's Command, posix_spawn on macOS, fork and
//! exec or clone on Linux), and whether unlocking first makes it go at once.
//!
//! One thread starts `true` over and over; the main thread, `ROUNDS` times for each way in
//! turn, opens a file (close-on-exec, as std opens every file), locks it, lets it go by
//! closing it or by unlocking it and then closing it, and at once asks for the lock through
//! a descriptor of its own, without waiting. It counts the times the lock was still held.
//!
//! rustc -O --edition 2024 main.rs -o /tmp/flock-spawn && /tmp/flock-spawn DIR [ROUNDS]

use std::fs::{File, TryLockError};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

fn held_after(path: &Path, unlock_first: bool) -> std::io::Result<bool> {
    let first = File::options().create(true).truncate(false).write(true).open(path)?;
    first.lock()?;
    if unlock_first {
        first.unlock()?;
    }
    drop(first);
    let second = File::options().write(true).open(path)?;
    match second.try_lock() {
        Ok(()) => {
            second.unlock()?;
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let dir = args.next().ok_or_else(|| std::io::Error::other("usage: flock-spawn DIR [ROUNDS]"))?;
    let rounds: u64 = args
        .next()
        .and_then(|r| r.to_str()?.parse().ok())
        .unwrap_or(20_000);
    let path = Path::new(&dir).join("lock");
    let stop = AtomicBool::new(false);
    let spawned = AtomicU64::new(0);
    let started = Instant::now();
    let (closed, unlocked) = std::thread::scope(|scope| {
        let spawner = scope.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                if std::process::Command::new("true").status().is_ok() {
                    spawned.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        let mut closed = 0u64;
        let mut unlocked = 0u64;
        let mut result = Ok(());
        for _ in 0..rounds {
            match (held_after(&path, false), held_after(&path, true)) {
                (Ok(a), Ok(b)) => {
                    closed += u64::from(a);
                    unlocked += u64::from(b);
                }
                (Err(e), _) | (_, Err(e)) => {
                    result = Err(e);
                    break;
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        let _ = spawner.join();
        result.map(|()| (closed, unlocked))
    })?;
    println!(
        "rounds {rounds} each way, {} programs started in {:.1} s",
        spawned.load(Ordering::Relaxed),
        started.elapsed().as_secs_f64()
    );
    println!("still held after close: {closed}");
    println!("still held after unlock and close: {unlocked}");
    Ok(())
}

//! Minimal leveled logging to stderr, or where [`to`] sends it, filtered by `SHARDS_LOG`
//! (error|warn|info|debug).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

static LEVEL: AtomicU8 = AtomicU8::new(0);
static START: OnceLock<Instant> = OnceLock::new();
/// Where lines go in stderr's place, once [`to`] has said. Held while a line is written,
/// so that the switch comes between lines, never in one.
static SINK: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Sends what is logged from now on to `file`, not stderr: a warm VM's, which takes its
/// client's stdio for its own, keeps its daemon's log (review 8.10). Once; later calls
/// change nothing. A line being written to stderr is written whole first, so that none
/// goes on into what stderr becomes after this returns.
pub fn to(file: std::fs::File) {
    let mut sink = SINK.lock().unwrap_or_else(PoisonError::into_inner);
    if sink.is_none() {
        *sink = Some(file);
    }
}

/// Reads `SHARDS_LOG` once; later calls are no-ops.
pub fn init() {
    START.get_or_init(Instant::now);
    let level = match std::env::var("SHARDS_LOG").as_deref() {
        Ok("error") => Level::Error,
        Ok("info") => Level::Info,
        Ok("debug") => Level::Debug,
        _ => Level::Warn,
    };
    let _ = LEVEL.compare_exchange(0, level as u8, Ordering::Relaxed, Ordering::Relaxed);
}

pub fn enabled(level: Level) -> bool {
    let current = LEVEL.load(Ordering::Relaxed);
    level as u8 <= if current == 0 { Level::Warn as u8 } else { current }
}

/// trace.patch: the host's monotonic clock, in microseconds, so that every process's
/// readings compare (docs/research/measurements/start-path).
pub fn uptime_us() -> u128 {
    let _ = STARTED_US.get_or_init(monotonic_us);
    monotonic_us()
}

static STARTED_US: OnceLock<u128> = OnceLock::new();

/// trace.patch: this process's first reading.
pub fn started_us() -> u128 {
    *STARTED_US.get_or_init(monotonic_us)
}

#[allow(unsafe_code, clippy::cast_sign_loss)]
fn monotonic_us() -> u128 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid timespec to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u128 * 1_000_000 + ts.tv_nsec as u128 / 1000
}

/// trace.patch: a step of a run, on stderr (a daemon's: its log).
pub fn trace(step: &str) {
    use std::io::Write;
    let line = format!("shards-trace {} {step} {}\n", std::process::id(), monotonic_us());
    let _ = std::io::stderr().write_all(line.as_bytes());
}

pub fn write(level: Level, args: std::fmt::Arguments<'_>) {
    let tag = match level {
        Level::Error => "ERROR",
        Level::Warn => "WARN",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
    };
    use std::io::Write;
    // A line a write, so that other processes' lines on the same file do not split it,
    // and under the sink's lock, so that [`to`] does not (review 8.10). Logging never
    // fails the caller, even with stderr closed.
    // Its process's ID first: the lines of every VM a daemon starts share its log.
    let line = format!("[{} {:>10}us {tag}] {args}\n", std::process::id(), uptime_us());
    let sink = SINK.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = match sink.as_ref() {
        Some(file) => (&*file).write_all(line.as_bytes()),
        None => std::io::stderr().write_all(line.as_bytes()),
    };
}

#[macro_export]
macro_rules! log {
    ($level:expr, $($arg:tt)*) => {
        if $crate::log::enabled($level) {
            $crate::log::write($level, format_args!($($arg)*));
        }
    };
}
#[macro_export]
macro_rules! error { ($($arg:tt)*) => { $crate::log!($crate::log::Level::Error, $($arg)*) }; }
#[macro_export]
macro_rules! warn { ($($arg:tt)*) => { $crate::log!($crate::log::Level::Warn, $($arg)*) }; }
#[macro_export]
macro_rules! info { ($($arg:tt)*) => { $crate::log!($crate::log::Level::Info, $($arg)*) }; }
#[macro_export]
macro_rules! debug { ($($arg:tt)*) => { $crate::log!($crate::log::Level::Debug, $($arg)*) }; }

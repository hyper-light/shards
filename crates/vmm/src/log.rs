//! Minimal leveled logging to stderr, or where [`to`] sends it, filtered by `SHARDS_LOG`
//! (error|warn|info|debug).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
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
/// Where lines go in stderr's place, once [`to`] has said.
static SINK: OnceLock<std::fs::File> = OnceLock::new();

/// Sends what is logged from now on to `file`, not stderr: a warm VM's, which takes its
/// client's stdio for its own, keeps its daemon's log (review 8.10). Once; later calls
/// change nothing.
pub fn to(file: std::fs::File) {
    let _ = SINK.set(file);
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

/// Microseconds since `init`.
pub fn uptime_us() -> u128 {
    START.get_or_init(Instant::now).elapsed().as_micros()
}

pub fn write(level: Level, args: std::fmt::Arguments<'_>) {
    let tag = match level {
        Level::Error => "ERROR",
        Level::Warn => "WARN",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
    };
    use std::io::Write;
    // Logging never fails the caller, even with stderr closed.
    match SINK.get() {
        // A line a write, so that other processes' lines on the same file do not split
        // it.
        Some(file) => {
            let line = format!("[{:>10}us {tag}] {args}\n", uptime_us());
            let _ = (&*file).write_all(line.as_bytes());
        }
        None => {
            let _ = writeln!(std::io::stderr().lock(), "[{:>10}us {tag}] {args}", uptime_us());
        }
    }
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

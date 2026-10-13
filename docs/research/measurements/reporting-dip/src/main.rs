//! What a guest reads of its free memory as a pass of free page reporting holds it
//! (D151, PM M224): touches MIB MiB and frees it, which has its kernel report what it
//! freed two seconds on, then reads `MemAvailable` and `MemFree` (/proc/meminfo), and
//! `page_reporting.held` where the kernel has it (resources/kernel/patches/0006), every
//! millisecond for SECONDS seconds. Prints the lowest and highest of each, in KiB, and how
//! many readings saw pages held: a kernel that counts what a pass holds as free shows no
//! dip as it holds them.
//!
//!     reporting-dip MIB SECONDS

use std::io::Write as _;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let number = |i: usize| args.get(i).and_then(|a| a.parse::<u64>().ok()).unwrap_or(0);
    let (mib, seconds) = (number(1), number(2));
    let held: Vec<Vec<u8>> = (0..mib).map(|_| vec![1u8; 1 << 20]).collect();
    drop(held);
    let meminfo = |key: &str, text: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|kb| kb.trim().parse().ok())
    };
    let (mut available, mut free) = ((u64::MAX, 0u64), (u64::MAX, 0u64));
    let (mut held_most, mut held_seen, mut readings) = (0u64, 0u64, 0u64);
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo")
            && let (Some(a), Some(f)) = (meminfo("MemAvailable:", &text), meminfo("MemFree:", &text))
        {
            available = (available.0.min(a), available.1.max(a));
            free = (free.0.min(f), free.1.max(f));
            readings += 1;
        }
        if let Ok(pages) = std::fs::read_to_string("/sys/module/page_reporting/parameters/held")
            && let Ok(pages) = pages.trim().parse::<u64>()
        {
            held_most = held_most.max(pages * 4);
            held_seen += u64::from(pages > 0);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = writeln!(
        std::io::stdout(),
        "readings {readings} available {} {} free {} {} held_most_kib {held_most} held_seen {held_seen}",
        available.0,
        available.1,
        free.0,
        free.1
    );
}

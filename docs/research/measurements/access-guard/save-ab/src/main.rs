//! One sample of `GuestMemory::save`: RAM of `--mib` MiB, a nonzero byte in every
//! `--every`th host page (the rest untouched, or, with `--touch-all`, touched and left
//! zero), saved to a fresh file. Prints `{"save_us": N}`. Touches pages through
//! `host_ptr`, which every revision under comparison has.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::time::Instant;

use shards_vmm::memory::GuestMemory;

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let mib: usize = arg("--mib").unwrap().parse().unwrap();
    let every: usize = arg("--every").unwrap().parse().unwrap();
    let touch_all = std::env::args().any(|a| a == "--touch-all");
    let out = arg("--out").unwrap();
    let page = 16 << 10;
    let len = mib << 20;
    let base = 0x8000_0000u64;
    let mem = GuestMemory::anonymous(&[(base, len)]).unwrap();
    for i in 0..len / page {
        let at = base + (i * page) as u64;
        let p = mem.host_ptr(at, 1).unwrap();
        // SAFETY: one byte of the guest RAM just reserved, which nothing else accesses.
        unsafe {
            if i % every == 0 {
                p.write_volatile(1);
            } else if touch_all {
                p.write_volatile(0);
            }
        }
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&out)
        .unwrap();
    let t0 = Instant::now();
    mem.save(&file).unwrap();
    let us = t0.elapsed().as_micros();
    drop(file);
    let _ = std::fs::remove_file(&out);
    // The process's peak resident set, after the save: what the save made the host give
    // it besides the pages written before (ru_maxrss, bytes on macOS, KiB on Linux).
    // SAFETY: getrusage(2) filling a struct on this stack.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let kib = if cfg!(target_os = "macos") { usage.ru_maxrss / 1024 } else { usage.ru_maxrss };
    println!("{{\"save_us\": {us}, \"peak_kib\": {kib}}}");
}

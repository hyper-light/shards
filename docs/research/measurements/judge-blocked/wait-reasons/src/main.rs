//! Where a Windows thread waits as D215's reader sees it (crates/api progress.rs's Windows
//! `hold`): each thread of a child, its state and wait reason (KWAIT_REASON) read from
//! NtQuerySystemInformation(SystemProcessInformation) as often as the call answers, and
//! whether its process's record's counts grew between reads: its I/O operations and bytes
//! (`Reserved7`: read, write and other operation counts, then their transfer counts) and its
//! hard faults (`Reserved1[8..12]`, `HardFaultCount`, as System Informer's phnt names them).
//! Children: `flush` writes 4 KiB at its file's start, then FlushFileBuffers, again and
//! again; `pipe` reads a pipe no one writes; `sleep` sleeps. For each, of its busiest thread:
//! how many reads found it in each state and reason, and in how many of those its process's
//! counts had grown since the read before.
//!
//!   cargo run --release --offline -- [READS]
use std::collections::BTreeMap;
use std::io::Read as _;
use std::os::windows::fs::FileExt as _;
use std::process::{Command, Stdio};
use std::time::Instant;
use windows_sys::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
use windows_sys::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH;
use windows_sys::Win32::System::WindowsProgramming::{SYSTEM_PROCESS_INFORMATION, SYSTEM_THREAD_INFORMATION};

/// One thread of the child as a read saw it.
struct Seen {
    tid: usize,
    cpu: i64,
    state: u32,
    reason: u32,
}

/// The child's threads and its counts (I/O operations and bytes, hard faults), as one read
/// of the system's records saw them.
fn read(pid: u32) -> Option<(Vec<Seen>, [i64; 7])> {
    let mut words: Vec<u64> = Vec::new();
    let mut needed = 0u32;
    loop {
        let bytes = u32::try_from(words.len() * 8).ok()?;
        // SAFETY: `bytes` bytes of words for the call to fill, and a u32 for what it needs.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                words.as_mut_ptr().cast(),
                bytes,
                &raw mut needed,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH {
            let want = (needed as usize).div_ceil(8).max(words.len() * 2).max(1);
            words.resize(want, 0);
            continue;
        }
        if status < 0 {
            return None;
        }
        break;
    }
    // SAFETY: the words' bytes, which any u8 may view.
    let all = unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 8) };
    let (process, each) = (
        size_of::<SYSTEM_PROCESS_INFORMATION>(),
        size_of::<SYSTEM_THREAD_INFORMATION>(),
    );
    let mut at = 0usize;
    while let Some(record) = all.get(at..).filter(|r| r.len() >= process) {
        // SAFETY: a whole SYSTEM_PROCESS_INFORMATION, plain data, read where it lies.
        let info = unsafe {
            record
                .as_ptr()
                .cast::<SYSTEM_PROCESS_INFORMATION>()
                .read_unaligned()
        };
        if info.UniqueProcessId as usize == pid as usize {
            let mut threads = Vec::new();
            for n in 0..info.NumberOfThreads as usize {
                let Some(entry) = record.get(process + n * each..).filter(|e| e.len() >= each) else {
                    break;
                };
                // SAFETY: a whole SYSTEM_THREAD_INFORMATION, plain data, read where it lies.
                let t = unsafe {
                    entry
                        .as_ptr()
                        .cast::<SYSTEM_THREAD_INFORMATION>()
                        .read_unaligned()
                };
                threads.push(Seen {
                    tid: t.ClientId.UniqueThread as usize,
                    cpu: t.Reserved1[0] + t.Reserved1[1],
                    state: t.ThreadState,
                    reason: t.WaitReason,
                });
            }
            let hard = u32::from_le_bytes(info.Reserved1[8..12].try_into().ok()?);
            let r = info.Reserved7;
            return Some((threads, [r[0], r[1], r[2], r[3], r[4], r[5], i64::from(hard)]));
        }
        match info.NextEntryOffset {
            0 => return None,
            next => at += next as usize,
        }
    }
    None
}

fn named(state: u32, reason: u32) -> String {
    const STATES: [&str; 10] = [
        "initialized",
        "ready",
        "running",
        "standby",
        "terminated",
        "waiting",
        "transition",
        "deferred-ready",
        "gate-wait",
        "waiting-for-swap",
    ];
    const REASONS: [&str; 43] = [
        "Executive",
        "FreePage",
        "PageIn",
        "PoolAllocation",
        "DelayExecution",
        "Suspended",
        "UserRequest",
        "WrExecutive",
        "WrFreePage",
        "WrPageIn",
        "WrPoolAllocation",
        "WrDelayExecution",
        "WrSuspended",
        "WrUserRequest",
        "WrEventPair",
        "WrQueue",
        "WrLpcReceive",
        "WrLpcReply",
        "WrVirtualMemory",
        "WrPageOut",
        "WrRendezvous",
        "WrKeyedEvent",
        "WrTerminated",
        "WrProcessInSwap",
        "WrCpuRateControl",
        "WrCalloutStack",
        "WrKernel",
        "WrResource",
        "WrPushLock",
        "WrMutex",
        "WrQuantumEnd",
        "WrDispatchInt",
        "WrPreempted",
        "WrYieldExecution",
        "WrFastMutex",
        "WrGuardedMutex",
        "WrRundown",
        "WrAlertByThreadId",
        "WrDeferredPreempt",
        "WrPhysicalFault",
        "WrIoRing",
        "WrMdlCache",
        "WrRcu",
    ];
    let s = STATES.get(state as usize).copied().unwrap_or("?");
    if state == 5 {
        format!("{s}/{}", REASONS.get(reason as usize).copied().unwrap_or("?"))
    } else {
        s.to_string()
    }
}

fn measure(mode: &str, reads: usize) {
    let me = std::env::current_exe().unwrap();
    let mut child = Command::new(me)
        .arg(format!("child-{mode}"))
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    // Its threads as each read found them, by thread: state and reason, and whether its
    // counts grew since the read before.
    let mut by: BTreeMap<usize, (i64, BTreeMap<String, (usize, usize)>)> = BTreeMap::new();
    let mut before: Option<[i64; 7]> = None;
    let began = Instant::now();
    let mut done = 0;
    while done < reads {
        let Some((threads, counts)) = read(pid) else {
            continue;
        };
        let grew = before.is_some_and(|b| counts.iter().zip(b).any(|(n, b)| *n > b));
        before = Some(counts);
        for t in threads {
            let entry = by.entry(t.tid).or_default();
            entry.0 = t.cpu;
            let slot = entry.1.entry(named(t.state, t.reason)).or_default();
            slot.0 += 1;
            slot.1 += usize::from(grew);
        }
        done += 1;
    }
    let took = began.elapsed();
    drop(child.stdin.take());
    let _ = child.kill();
    let _ = child.wait();
    let counts = before.unwrap_or_default();
    println!(
        "== {mode}: {reads} reads in {took:?} ({:?} each); its counts by its end: read ops {}, write ops {}, other ops {}, read bytes {}, written bytes {}, other bytes {}, hard faults {}",
        took / reads as u32,
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4],
        counts[5],
        counts[6]
    );
    let busiest = by.iter().max_by_key(|(_, (cpu, _))| *cpu).map(|(tid, _)| *tid);
    for (tid, (cpu, states)) in &by {
        let mark = if Some(*tid) == busiest { " (busiest)" } else { "" };
        let mut states: Vec<_> = states.iter().collect();
        states.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
        let said: Vec<String> = states
            .iter()
            .map(|(s, (n, g))| format!("{s} {n} (grew {g})"))
            .collect();
        println!(
            "  thread {tid}{mark}, {:.1} ms CPU: {}",
            *cpu as f64 / 1e4,
            said.join(", ")
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let stdin_ends = || {
        std::thread::spawn(|| {
            let _ = std::io::stdin().read(&mut [0u8; 1]);
            std::process::exit(0);
        })
    };
    match args.get(1).map(String::as_str) {
        Some("child-flush") => {
            stdin_ends();
            let file = std::fs::File::create(format!("flushed-{}.bin", std::process::id())).unwrap();
            let page = [1u8; 4096];
            loop {
                file.seek_write(&page, 0).unwrap();
                file.sync_all().unwrap();
            }
        }
        Some("child-pipe") => {
            stdin_ends();
            let (mut reader, _writer) = std::io::pipe().unwrap();
            let _ = reader.read(&mut [0u8; 1]);
        }
        Some("child-sleep") => {
            stdin_ends();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        _ => {
            let reads = args.get(1).and_then(|n| n.parse().ok()).unwrap_or(2000);
            for mode in ["flush", "pipe", "sleep"] {
                measure(mode, reads);
            }
            for e in std::fs::read_dir(".").unwrap().flatten() {
                if e.file_name().to_string_lossy().starts_with("flushed-") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

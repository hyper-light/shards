//! PM M528: whether a parent-ID walk of Windows' processes takes an orphan for the child of a
//! process started after the orphan's parent ended, as the E2E harness's walk did
//! (crates/shards/tests/common `processes`, `under`): on windows-2025 it counted a ping that
//! the daemon tests' sleeper (`cmd /c ping -n 601 127.0.0.1`, cmd.exe ended alone) left
//! among a direct boot's processes, then failed the boot for leaving it running.
//!
//! Each attempt starts cmd.exe running ping, as the sleeper does, and ends cmd.exe alone,
//! which orphans ping. A marker thread's ID is freed just before cmd.exe's (its last handle
//! closed). Windows hands out process and thread IDs from one table, the longest-freed first:
//! once the marker's ID comes back to a new thread, cmd.exe's is the next free one, and the
//! next process takes it, unless something else on the host takes an ID in between. For each
//! attempt it prints what Toolhelp32 lists under that process by parent ID alone (the
//! harness's walk before) and by the creation-time rule (psutil's `Process.children`: a
//! parent that started after its child is not its parent). A `REUSED` line is a
//! reproduction.
//!
//! `pid-reuse [ATTEMPTS]`, 20 by default.

use std::os::windows::process::CommandExt as _;
use std::process::Command;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentThreadId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, TerminateProcess,
};

/// Every process: its ID, its parent's as Toolhelp32 lists it, its executable's name.
fn snapshot() -> Vec<(u32, u32, String)> {
    // SAFETY: a snapshot of every process, closed below.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    assert_ne!(snap, INVALID_HANDLE_VALUE, "CreateToolhelp32Snapshot");
    let mut e = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    // SAFETY: the snapshot, and an entry of its own size.
    let mut more = unsafe { Process32FirstW(snap, &raw mut e) } != 0;
    while more {
        let end = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
        out.push((
            e.th32ProcessID,
            e.th32ParentProcessID,
            String::from_utf16_lossy(&e.szExeFile[..end]),
        ));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snap, &raw mut e) } != 0;
    }
    // SAFETY: the snapshot taken above, closed once.
    unsafe { CloseHandle(snap) };
    out
}

/// When process `pid` started (GetProcessTimes), in 100 ns since 1601.
fn started(pid: u32) -> Option<u64> {
    // SAFETY: a handle of one right, closed below.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut t = [FILETIME::default(); 4];
    let [c, x, k, u] = &mut t;
    // SAFETY: the handle opened above and four times for the call to fill.
    let ok = unsafe { GetProcessTimes(h, c, x, k, u) } != 0;
    // SAFETY: the handle opened above, closed once.
    unsafe { CloseHandle(h) };
    ok.then(|| (u64::from(t[0].dwHighDateTime) << 32) | u64::from(t[0].dwLowDateTime))
}

/// The ID of a new thread, which ends and is joined: its ID freed again.
fn new_thread_id() -> u32 {
    // SAFETY: no arguments.
    std::thread::spawn(|| unsafe { GetCurrentThreadId() }).join().expect("a thread")
}

fn end(pid: u32) {
    // SAFETY: a handle of one right, used once and closed.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 1);
            CloseHandle(h);
        }
    }
}

fn main() {
    let attempts: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(20);
    let root = std::env::var("SystemRoot").expect("SystemRoot");
    let (cmd_exe, ping_exe) = (format!(r"{root}\System32\cmd.exe"), format!(r"{root}\System32\PING.EXE"));
    let mut reused = 0;
    for attempt in 0..attempts {
        let mut cmd = Command::new(&cmd_exe)
            .raw_arg("/c ping -n 120 127.0.0.1 >nul")
            .spawn()
            .expect("cmd.exe");
        let dead = cmd.id();
        let until = Instant::now() + Duration::from_secs(10);
        let orphan = loop {
            let listed = snapshot();
            let ping = listed
                .iter()
                .find(|(_, parent, name)| *parent == dead && name.eq_ignore_ascii_case("PING.EXE"));
            if let Some((pid, ..)) = ping {
                break *pid;
            }
            assert!(Instant::now() < until, "no ping under cmd.exe {dead}");
            std::thread::sleep(Duration::from_millis(10));
        };
        cmd.kill().expect("ending cmd.exe");
        cmd.wait().expect("cmd.exe's end");
        let marker = new_thread_id();
        drop(cmd);
        let began = Instant::now();
        let mut threads = 0u64;
        let came_back = loop {
            threads += 1;
            if new_thread_id() == marker {
                break true;
            }
            if began.elapsed() > Duration::from_secs(60) {
                break false;
            }
        };
        let mut taker = Command::new(&ping_exe)
            .args(["-n", "30", "127.0.0.1"])
            .spawn()
            .expect("ping");
        let took = taker.id();
        let listed = snapshot();
        let by_id: Vec<u32> = listed.iter().filter(|(_, p, _)| *p == took).map(|(c, ..)| *c).collect();
        let by_rule: Vec<u32> = listed
            .iter()
            .filter(|(child, p, _)| {
                *p == took && !matches!((started(took), started(*child)), (Some(p), Some(c)) if p > c)
            })
            .map(|(c, ..)| *c)
            .collect();
        let tag = if took == dead { "REUSED" } else { "other" };
        if took == dead {
            reused += 1;
        }
        println!(
            "{tag} attempt {attempt}: cmd.exe {dead} orphaned ping {orphan} (started {:?}); marker {marker} back {came_back} after {threads} threads in {:?}; next process {took} (started {:?}); under it by parent ID: {by_id:?}; by the rule: {by_rule:?}",
            started(orphan),
            began.elapsed(),
            started(took)
        );
        let _ = taker.kill();
        let _ = taker.wait();
        end(orphan);
    }
    println!("{reused} of {attempts} attempts reused cmd.exe's ID");
}

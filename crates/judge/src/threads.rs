//! Windows' listing of every process's threads, one read of it, and D215's one rule for what
//! a thread in it shows: wanting a CPU, waiting on its host's paging, or suspended. Windows
//! keeps a thread's state in that listing alone, every process's, 13.1 to 13.8 ms a read on
//! jupiter (PM M606). shards-api's judge reads a VM process's threads through it (its
//! `Listing`, read once for every reading of one look: its `Followed`'s hold, threads and
//! trace, and its party's account), and the tests' party (shards-testwait) its own members,
//! so that one listing rule holds both (ROOT, 2026-11-17).

use std::cell::Cell;

use windows_sys::Wdk::System::SystemInformation::{NtQuerySystemInformation, SystemProcessInformation};
use windows_sys::Wdk::System::SystemServices::{
    FreePage, PageIn, PoolAllocation, Suspended, WrFreePage, WrPageIn, WrPoolAllocation, WrSuspended,
};
use windows_sys::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH;
use windows_sys::Win32::System::WindowsProgramming::{SYSTEM_PROCESS_INFORMATION, SYSTEM_THREAD_INFORMATION};

/// What [`threads_held`] read of the threads it was asked of: how many the listing held, how
/// many of those wanted or had a CPU (ready, running, standby), and how many waited on their
/// host's paging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadsHeld {
    pub read: usize,
    pub runnable: usize,
    pub paging: usize,
}

// A thread's state in its record (`SYSTEM_THREAD_INFORMATION.ThreadState`), as WMI's
// Win32_Thread class documents its values: ready (1), running (2) and standby (3) want or
// have a CPU; transition (6) waits "for resources other than the processor", its kernel
// stack brought back into memory; waiting (5) says on what by its wait reason.
pub const READY: u32 = 1;
pub const RUNNING: u32 = 2;
pub const STANDBY: u32 = 3;
pub const WAITING: u32 = 5;
pub const TRANSITION: u32 = 6;

/// Whether a thread in `state` wants or has a CPU: ready, running or standby (WMI's
/// `Win32_Thread`). One rule for each Windows reading of D215's holds (shards-api's
/// `Followed::hold` and `runnable_threads`, [`threads_held`]). A thread leaves ready only by
/// running or ending, so one its host has kept from a CPU since it last ran is in it still as a
/// look reads it.
pub fn wants_cpu(state: u32) -> bool {
    matches!(state, READY | RUNNING | STANDBY)
}

/// What threads `tids` of process `pid` are doing as one read of the system's whole listing
/// shows them (D215): of those it lists, how many want or have a CPU and how many wait on
/// their host's paging, each classed as shards-api's `Followed` classes a VM process's
/// threads ([`wants_cpu`], [`pages`]): one rule for its judge and for a waiter that follows
/// threads of its own process, as the tests' parties do (shards-testwait). A waiter reads it
/// only at a look that found its threads ran nothing. None where it could not be read.
pub fn threads_held(pid: u32, tids: &[u32]) -> Option<ThreadsHeld> {
    let records = records().ok()?;
    let mut held = ThreadsHeld::default();
    each_thread(&records, |of, t| {
        let tid = u32::try_from(t.ClientId.UniqueThread as usize).unwrap_or(u32::MAX);
        if of != pid || !tids.contains(&tid) {
            return;
        }
        held.read = held.read.saturating_add(1);
        held.runnable = held
            .runnable
            .saturating_add(usize::from(wants_cpu(t.ThreadState)));
        held.paging = held
            .paging
            .saturating_add(usize::from(pages(t.ThreadState, t.WaitReason)));
    });
    Some(held)
}

/// Each thread of process `pid` as one read of the system's whole listing, `words`
/// ([`records`]), shows it (D215): its ID, its state and, where it waits, its wait reason, as
/// the listing's records keep them (`SYSTEM_THREAD_INFORMATION`'s ThreadState and WaitReason,
/// named as WMI's `Win32_Thread` and the WDK's KWAIT_REASON name them). For a refusal's account
/// of its party, so that it says what each of its threads was doing as it was refused (ROOT,
/// 2026-11-18: CI 44189693085's moved.rs and snapshot.rs storm refusals said "0 of 3 threads
/// runnable" and no more). None where the listing lists no thread of `pid`.
pub fn threads_said(words: &[u64], pid: u32) -> Option<String> {
    let mut said = Vec::new();
    each_thread(words, |of, t| {
        if of != pid {
            return;
        }
        let tid = t.ClientId.UniqueThread as usize;
        said.push(match (t.ThreadState, wait_reason(t.WaitReason)) {
            (WAITING, Some(reason)) => format!("{tid} waiting ({reason})"),
            (WAITING, None) => format!("{tid} waiting (reason {})", t.WaitReason),
            (state, _) => format!("{tid} {}", state_name(state)),
        });
    });
    (!said.is_empty()).then(|| said.join(", "))
}

/// A thread's state by WMI's `Win32_Thread` name for it, else its number.
fn state_name(state: u32) -> String {
    match state {
        READY => "ready".to_string(),
        RUNNING => "running".to_string(),
        STANDBY => "standby".to_string(),
        WAITING => "waiting".to_string(),
        TRANSITION => "in transition".to_string(),
        other => format!("in state {other}"),
    }
}

/// A waiting thread's reason by the WDK's name for it (wdm.h KWAIT_REASON), none for a
/// value it does not name.
fn wait_reason(reason: u32) -> Option<&'static str> {
    use windows_sys::Wdk::System::SystemServices as kw;
    const REASONS: &[(i32, &str)] = &[
        (kw::Executive, "Executive"),
        (kw::FreePage, "FreePage"),
        (kw::PageIn, "PageIn"),
        (kw::PoolAllocation, "PoolAllocation"),
        (kw::DelayExecution, "DelayExecution"),
        (kw::Suspended, "Suspended"),
        (kw::UserRequest, "UserRequest"),
        (kw::WrExecutive, "WrExecutive"),
        (kw::WrFreePage, "WrFreePage"),
        (kw::WrPageIn, "WrPageIn"),
        (kw::WrPoolAllocation, "WrPoolAllocation"),
        (kw::WrDelayExecution, "WrDelayExecution"),
        (kw::WrSuspended, "WrSuspended"),
        (kw::WrUserRequest, "WrUserRequest"),
        (kw::WrQueue, "WrQueue"),
        (kw::WrLpcReceive, "WrLpcReceive"),
        (kw::WrLpcReply, "WrLpcReply"),
        (kw::WrVirtualMemory, "WrVirtualMemory"),
        (kw::WrPageOut, "WrPageOut"),
        (kw::WrRendezvous, "WrRendezvous"),
        (kw::WrKeyedEvent, "WrKeyedEvent"),
        (kw::WrTerminated, "WrTerminated"),
        (kw::WrProcessInSwap, "WrProcessInSwap"),
        (kw::WrCpuRateControl, "WrCpuRateControl"),
        (kw::WrCalloutStack, "WrCalloutStack"),
        (kw::WrKernel, "WrKernel"),
        (kw::WrResource, "WrResource"),
        (kw::WrPushLock, "WrPushLock"),
        (kw::WrMutex, "WrMutex"),
        (kw::WrQuantumEnd, "WrQuantumEnd"),
        (kw::WrDispatchInt, "WrDispatchInt"),
        (kw::WrPreempted, "WrPreempted"),
        (kw::WrYieldExecution, "WrYieldExecution"),
        (kw::WrFastMutex, "WrFastMutex"),
        (kw::WrGuardedMutex, "WrGuardedMutex"),
        (kw::WrRundown, "WrRundown"),
        (kw::WrAlertByThreadId, "WrAlertByThreadId"),
        (kw::WrDeferredPreempt, "WrDeferredPreempt"),
        (kw::WrPhysicalFault, "WrPhysicalFault"),
        (kw::WrIoRing, "WrIoRing"),
        (kw::WrMdlCache, "WrMdlCache"),
    ];
    let reason = i32::try_from(reason).ok()?;
    REASONS
        .iter()
        .find(|(value, _)| *value == reason)
        .map(|(_, name)| *name)
}

/// Whether a thread in `state`, waiting for `reason`, waits on its host's paging: its
/// kernel stack brought back into memory (transition), a page brought in, or a free page or
/// pool found for it ([`on_memory`]). Its host's disks serve each, a page-in being their
/// read and a page freed by writing it out, and no count of its process's shows them, so
/// such a wait is its host's only while those disks complete I/O (shards-api's
/// `Hold::held_since`), as
/// a Linux `D` and a macOS uninterruptible wait are: one that no disk serves is still.
pub fn pages(state: u32, reason: u32) -> bool {
    state == TRANSITION || (state == WAITING && on_memory(reason))
}

/// Whether a thread's wait `reason` is on its host's memory: a page brought in, or a free
/// page or pool found for it (wdm.h's KWAIT_REASON: PageIn, FreePage, PoolAllocation, and
/// their Wr forms).
fn on_memory(reason: u32) -> bool {
    i32::try_from(reason).is_ok_and(|reason| {
        [
            FreePage,
            PageIn,
            PoolAllocation,
            WrFreePage,
            WrPageIn,
            WrPoolAllocation,
        ]
        .contains(&reason)
    })
}

/// Whether a thread's wait `reason` is its suspension.
pub fn is_suspension(reason: u32) -> bool {
    i32::try_from(reason).is_ok_and(|reason| [Suspended, WrSuspended].contains(&reason))
}

thread_local! {
    /// How many times this thread has read the system's listing ([`records`]).
    static READS: Cell<u64> = const { Cell::new(0) };
}

/// How many times the calling thread has read the system's listing ([`records`]), each
/// 13.1 to 13.8 ms of CPU on jupiter (PM M606): what a test counts a look's reads by (D215:
/// one a look, whatever its party's size). Counted per thread, as a look runs on its waiter's
/// alone, so that the tests beside it, reading on threads of their own, count none of theirs.
pub fn reads() -> u64 {
    READS.with(Cell::get)
}

/// The system's process records (NtQuerySystemInformation, SystemProcessInformation), in
/// words of 8 bytes, as they hold pointers; or why they could not be read: the call's
/// NTSTATUS, or room for them that could not be had. Asked again with room for what the call
/// said they need, and at least twice the last, until they fit. Each call is one read
/// ([`reads`]).
pub fn records() -> Result<Vec<u64>, String> {
    READS.with(|reads| reads.set(reads.get().saturating_add(1)));
    let mut words: Vec<u64> = Vec::new();
    loop {
        let bytes = words
            .len()
            .checked_mul(size_of::<u64>())
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| {
                format!(
                    "the system's thread listing: {} words, past a call's u32 of bytes",
                    words.len()
                )
            })?;
        let mut needed = 0u32;
        // SAFETY: `bytes` bytes of the words for the call to fill (none, as it is first
        // asked), and a u32 for the size it needs.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                words.as_mut_ptr().cast(),
                bytes,
                &raw mut needed,
            )
        };
        let needed = usize::try_from(needed)
            .map_err(|e| format!("the system's thread listing: {needed} bytes needed: {e}"))?;
        if status == STATUS_INFO_LENGTH_MISMATCH {
            let want = needed
                .div_ceil(size_of::<u64>())
                .max(words.len().saturating_mul(2))
                .max(1);
            words
                .try_reserve_exact(want.saturating_sub(words.len()))
                .map_err(|e| format!("the system's thread listing: room for {want} words: {e}"))?;
            words.resize(want, 0);
            continue;
        }
        if status < 0 {
            return Err(format!(
                "the system's thread listing: NtQuerySystemInformation(SystemProcessInformation) failed, NTSTATUS {status:#010x}"
            ));
        }
        words.truncate(needed.div_ceil(size_of::<u64>()).min(words.len()));
        return Ok(words);
    }
}

/// Each thread of processes `pids` but thread `except` (a waiter's own, where one asks), as one
/// read of the system's whole listing, `words` ([`records`]), shows it, for D215's rule of a
/// thread its host keeps from any CPU (shards-api's `starved`: one wanting a CPU now whose
/// count has not moved since the read before): by its ID, a count that moves whenever it runs,
/// and whether it wants a CPU it does not have (ready or standby: [`wants_cpu`], not running).
/// The count is its switches to a CPU (its record's ContextSwitches, windows-sys's `Reserved3`
/// of SYSTEM_THREAD_INFORMATION, phnt ntexapi.h), doubled, and one more while it runs: it
/// moves as the thread is switched in, and as it stops running, so that one preempted since
/// the read before, its switches the same, still shows its run. Its times would not do: they
/// move in its clock's ticks (15.625 ms), and show a thread that ran less than one as unrun.
/// One rule with Linux's and macOS's readings of a process's threads (shards-api's
/// `Watch::starved` of a VM process's, the tests' machine parties, D209 Part 5).
pub fn threads_switched(
    words: &[u64],
    pids: &[u32],
    except: Option<u32>,
) -> std::collections::BTreeMap<u64, (u64, bool)> {
    let mut threads = std::collections::BTreeMap::new();
    each_thread(words, |of, t| {
        let tid = t.ClientId.UniqueThread as usize;
        if !pids.contains(&of) || except.is_some_and(|except| tid == except as usize) {
            return;
        }
        let running = t.ThreadState == RUNNING;
        let count = u64::from(t.Reserved3)
            .saturating_mul(2)
            .saturating_add(u64::from(running));
        threads.insert(tid as u64, (count, wants_cpu(t.ThreadState) && !running));
    });
    threads
}

/// Each thread the records in `words` hold: its process's id and its thread's record
/// (state, wait reason, times, context switches). A record that would pass the records'
/// end ends them.
pub fn each_thread(words: &[u64], mut thread: impl FnMut(u32, &SYSTEM_THREAD_INFORMATION)) {
    let Some(len) = words.len().checked_mul(size_of::<u64>()) else {
        return;
    };
    // SAFETY: the words' `len` bytes, which any u8 may view, borrowed as long as they are.
    let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), len) };
    let process = size_of::<SYSTEM_PROCESS_INFORMATION>();
    let each = size_of::<SYSTEM_THREAD_INFORMATION>();
    let mut at = 0usize;
    while let Some(record) = bytes.get(at..).filter(|record| record.len() >= process) {
        // SAFETY: `record` holds a whole SYSTEM_PROCESS_INFORMATION, plain data, read where
        // it lies, aligned or not.
        let info = unsafe {
            record
                .as_ptr()
                .cast::<SYSTEM_PROCESS_INFORMATION>()
                .read_unaligned()
        };
        let pid = u32::try_from(info.UniqueProcessId as usize).unwrap_or(u32::MAX);
        for n in 0..usize::try_from(info.NumberOfThreads).unwrap_or(0) {
            let Some(entry) = n
                .checked_mul(each)
                .and_then(|offset| offset.checked_add(process))
                .and_then(|offset| record.get(offset..))
                .filter(|entry| entry.len() >= each)
            else {
                break;
            };
            // SAFETY: `entry` holds a whole SYSTEM_THREAD_INFORMATION, plain data, read
            // where it lies, aligned or not.
            let t = unsafe {
                entry
                    .as_ptr()
                    .cast::<SYSTEM_THREAD_INFORMATION>()
                    .read_unaligned()
            };
            thread(pid, &t);
        }
        let next = usize::try_from(info.NextEntryOffset).unwrap_or(0);
        match at.checked_add(next) {
            Some(after) if next > 0 => at = after,
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On Windows a thread waits in its kernel on its host's paging where its kernel stack is
    /// brought back into memory (transition) or it waits for a page brought in, a free page or
    /// pool, their Wr forms too (KWAIT_REASON); not where it is ready, nor where it waits on
    /// anything else: a flush's and an unanswered pipe's `Executive` (PM M606), a sleep's
    /// `UserRequest`, a suspension. Before 2026-11-17 a paging wait was its host's hold
    /// whatever its disks did, so a wait no disk served was never refused.
    #[test]
    fn a_thread_waiting_on_its_hosts_paging_waits_in_its_kernel() {
        use windows_sys::Wdk::System::SystemServices::{
            Executive, FreePage, PageIn, PoolAllocation, Suspended, UserRequest, WrPageIn,
        };
        let reason = |r: i32| u32::try_from(r).unwrap();
        let (ready, waiting, transition) = (1, 5, 6);
        for paged in [PageIn, WrPageIn, FreePage, PoolAllocation] {
            assert!(pages(waiting, reason(paged)), "reason {paged}");
        }
        assert!(pages(transition, reason(Executive)), "its stack paged out");
        for other in [Executive, UserRequest, Suspended] {
            assert!(!pages(waiting, reason(other)), "reason {other}");
        }
        assert!(!pages(ready, reason(PageIn)), "ready");
    }
}

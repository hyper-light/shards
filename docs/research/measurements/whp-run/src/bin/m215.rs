//! What changing a socket's registration with a completion port does (PM M215, D142).
//! shards' poller (crates/net/src/poll.rs) registers each socket level-triggered under its
//! token and changes the registration as the socket's interest changes; a connection that
//! wants nothing for a while (its guest's window full, nothing for its host) waits for
//! nothing, then for something again. ProcessSocketNotifications' documentation says a
//! re-registration overwrites the registration, a registration may filter no events, a
//! disabled one must not be enabled again until its DISABLE notice comes, and a removed one
//! posts a REMOVE notice. What each way of waiting for nothing, then for something, does:
//!
//! - **remove, enable**: OP_REMOVE, then OP_ENABLE at once, as the poller did;
//! - **remove, its notice, enable**: OP_REMOVE, its REMOVE notice taken, then OP_ENABLE;
//! - **disable, enable**: OP_DISABLE, then OP_ENABLE at once;
//! - **filter none, enable**: OP_ENABLE filtering no events, then OP_ENABLE filtering IN.
//!
//! And one-shot registrations (SOCK_NOTIFY_TRIGGER_ONESHOT, level), which a notice disables
//! and an OP_ENABLE arms again, Microsoft's pattern for a registration re-armed as each
//! notice is handled: whether a disarmed one is told nothing, whether one armed again at
//! once is told what still holds, whether one armed again for output is told that, and
//! whether one armed is overwritten by an OP_ENABLE of another filter.
//!
//! Each 200 times on a fresh loopback connection: each change's result, whether a byte
//! sent while the registration waited for nothing was told within 20 ms (it must not be),
//! and whether it was told within a second once it waited for it again (it must be); and
//! what each change costs.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::collections::BTreeMap;
use std::io::Write as _;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Networking::WinSock::*;
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY};

const ROUNDS: usize = 200;
const KEY: usize = 7;

fn wsa() -> String {
    // SAFETY: no arguments.
    let e = unsafe { WSAGetLastError() };
    format!("WSA error {e} ({})", std::io::Error::from_raw_os_error(e))
}

fn listener() -> Result<(SOCKET, SOCKADDR_IN), String> {
    // SAFETY: plain arguments.
    let l = unsafe {
        WSASocketW(
            i32::from(AF_INET),
            SOCK_STREAM,
            0,
            std::ptr::null(),
            0,
            WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if l == INVALID_SOCKET {
        return Err(format!("socket: {}", wsa()));
    }
    let mut a = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: 0,
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes([127, 0, 0, 1]),
            },
        },
        sin_zero: [0; 8],
    };
    let mut len = std::mem::size_of::<SOCKADDR_IN>() as i32;
    // SAFETY: an address of its own length, then its name into one.
    unsafe {
        if bind(l, (&raw const a).cast(), len) != 0 || listen(l, SOMAXCONN as i32) != 0 {
            return Err(format!("bind/listen: {}", wsa()));
        }
        getsockname(l, (&raw mut a).cast(), &raw mut len);
    }
    Ok((l, a))
}

/// A connected pair over the loopback: (client, accepted), the accepted one non-blocking,
/// as the poller's sockets are.
fn pair(l: SOCKET, at: &SOCKADDR_IN) -> Result<(SOCKET, SOCKET), String> {
    // SAFETY: plain arguments; an address of its own length; an accept on our listener.
    unsafe {
        let c = WSASocketW(
            i32::from(AF_INET),
            SOCK_STREAM,
            0,
            std::ptr::null(),
            0,
            WSA_FLAG_NO_HANDLE_INHERIT,
        );
        if c == INVALID_SOCKET {
            return Err(format!("socket: {}", wsa()));
        }
        if connect(
            c,
            (at as *const SOCKADDR_IN).cast(),
            std::mem::size_of::<SOCKADDR_IN>() as i32,
        ) != 0
        {
            return Err(format!("connect: {}", wsa()));
        }
        let a = accept(l, std::ptr::null_mut(), std::ptr::null_mut());
        if a == INVALID_SOCKET {
            return Err(format!("accept: {}", wsa()));
        }
        let mut one: u32 = 1;
        ioctlsocket(a, FIONBIO, &raw mut one);
        Ok((c, a))
    }
}

/// Closes `s` at once (a linger of 0): no TIME_WAIT holds its port for the next round.
fn close_now(s: SOCKET) {
    let l = LINGER {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: an option of its own size on our socket, then its close.
    unsafe {
        setsockopt(
            s,
            SOL_SOCKET,
            SO_LINGER,
            (&raw const l).cast(),
            std::mem::size_of::<LINGER>() as i32,
        );
        closesocket(s);
    }
}

/// One registration change of `s` with `port`: the call's result, then the registration's,
/// and what it took in µs.
fn change(port: HANDLE, s: SOCKET, filter: u32, operation: u32) -> (u32, u32, f64) {
    change_as(
        port,
        s,
        filter,
        operation,
        SOCK_NOTIFY_TRIGGER_PERSISTENT | SOCK_NOTIFY_TRIGGER_LEVEL,
    )
}

/// [`change`], its trigger `flags`.
fn change_as(port: HANDLE, s: SOCKET, filter: u32, operation: u32, flags: u32) -> (u32, u32, f64) {
    let mut reg = SOCK_NOTIFY_REGISTRATION {
        socket: s,
        completionKey: std::ptr::without_provenance_mut(KEY),
        eventFilter: filter as u16,
        operation: operation as u8,
        triggerFlags: flags as u8,
        registrationResult: 0,
    };
    let t = Instant::now();
    // SAFETY: one registration of our socket with our port; no wait.
    let r = unsafe {
        ProcessSocketNotifications(
            port,
            1,
            &raw mut reg,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (r, reg.registrationResult, t.elapsed().as_secs_f64() * 1e6)
}

/// The notices `port` has for our key within `ms`: each one's event bits.
fn notices(port: HANDLE, ms: u32) -> Vec<u32> {
    notices_until(port, ms, 0)
}

/// The notices `port` has for our key until one has a bit of `want`, or `ms` pass (all of
/// them, with `want` 0): a level-triggered registration is told again each time it is
/// dequeued while what it waits for holds, so the port is never quiet of it.
fn notices_until(port: HANDLE, ms: u32, want: u32) -> Vec<u32> {
    let mut got = Vec::new();
    let mut entries = [OVERLAPPED_ENTRY::default(); 16];
    let end = Instant::now() + std::time::Duration::from_millis(u64::from(ms));
    loop {
        let left = end.saturating_duration_since(Instant::now());
        let wait = u32::try_from(left.as_millis()).unwrap_or(ms);
        let mut n = 0u32;
        // SAFETY: our port and an array of its own length.
        let ok = unsafe { GetQueuedCompletionStatusEx(port, entries.as_mut_ptr(), 16, &raw mut n, wait, 0) };
        if ok == 0 {
            return got;
        }
        for e in entries.iter().take(n as usize) {
            if e.lpCompletionKey == KEY {
                got.push(e.dwNumberOfBytesTransferred);
            }
        }
        if want != 0 && got.iter().any(|b| b & want != 0) || left.is_zero() {
            return got;
        }
    }
}

/// How a registration waits for nothing before it waits for its socket's input again.
#[derive(Clone, Copy, Debug)]
enum Way {
    Removed,
    RemovedNoticed,
    Disabled,
    FilteredNone,
}

#[derive(Default)]
struct Tally {
    /// Each change's (call result, registration result), by step, and how often.
    results: BTreeMap<(&'static str, u32, u32), usize>,
    /// Rounds where the byte sent while it waited for nothing was told then.
    told_while_none: usize,
    /// Rounds where the byte was told once it waited for it again.
    told_after: usize,
    costs: BTreeMap<&'static str, Vec<f64>>,
}

impl Tally {
    fn note(&mut self, step: &'static str, (call, result, us): (u32, u32, f64)) {
        *self.results.entry((step, call, result)).or_default() += 1;
        self.costs.entry(step).or_default().push(us);
    }
}

fn round(l: SOCKET, at: &SOCKADDR_IN, way: Way, tally: &mut Tally) -> Result<(), String> {
    let (c, a) = pair(l, at)?;
    // SAFETY: a new port.
    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    if port.is_null() {
        return Err("CreateIoCompletionPort".into());
    }
    let input = SOCK_NOTIFY_REGISTER_EVENT_IN | SOCK_NOTIFY_REGISTER_EVENT_HANGUP;
    tally.note("enable", change(port, a, input, SOCK_NOTIFY_OP_ENABLE));
    match way {
        Way::Removed | Way::RemovedNoticed => {
            tally.note("remove", change(port, a, 0, SOCK_NOTIFY_OP_REMOVE));
        }
        Way::Disabled => tally.note("disable", change(port, a, 0, SOCK_NOTIFY_OP_DISABLE)),
        Way::FilteredNone => tally.note("filter none", change(port, a, 0, SOCK_NOTIFY_OP_ENABLE)),
    }
    if let Way::RemovedNoticed = way {
        let seen = notices_until(port, 1000, SOCK_NOTIFY_EVENT_REMOVE);
        if !seen.iter().any(|b| b & SOCK_NOTIFY_EVENT_REMOVE != 0) {
            *tally.results.entry(("no remove notice", 0, 0)).or_default() += 1;
        }
    }
    // A byte while it waits for nothing: no notice of it may come.
    let b = [1u8];
    // SAFETY: a byte of ours.
    unsafe { send(c, b.as_ptr(), 1, 0) };
    if notices(port, 20).iter().any(|b| b & SOCK_NOTIFY_EVENT_IN != 0) {
        tally.told_while_none += 1;
    }
    tally.note("enable again", change(port, a, input, SOCK_NOTIFY_OP_ENABLE));
    if notices_until(port, 1000, SOCK_NOTIFY_EVENT_IN)
        .iter()
        .any(|b| b & SOCK_NOTIFY_EVENT_IN != 0)
    {
        tally.told_after += 1;
    }
    change(port, a, 0, SOCK_NOTIFY_OP_REMOVE);
    close_now(a);
    close_now(c);
    // SAFETY: our port.
    unsafe { CloseHandle(port) };
    Ok(())
}

/// One-shot registrations: what each step was told, by round.
#[derive(Default)]
struct OneShot {
    results: BTreeMap<(&'static str, u32, u32), usize>,
    told_first: usize,
    told_disarmed: usize,
    told_again: usize,
    told_out: usize,
    told_overwritten: usize,
}

fn oneshot_round(l: SOCKET, at: &SOCKADDR_IN, t: &mut OneShot) -> Result<(), String> {
    let (c, a) = pair(l, at)?;
    // SAFETY: a new port.
    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    if port.is_null() {
        return Err("CreateIoCompletionPort".into());
    }
    let once = SOCK_NOTIFY_TRIGGER_ONESHOT | SOCK_NOTIFY_TRIGGER_LEVEL;
    let input = SOCK_NOTIFY_REGISTER_EVENT_IN | SOCK_NOTIFY_REGISTER_EVENT_HANGUP;
    let output = SOCK_NOTIFY_REGISTER_EVENT_OUT | SOCK_NOTIFY_REGISTER_EVENT_HANGUP;
    let mut note = |step: &'static str, (call, result, _): (u32, u32, f64)| {
        *t.results.entry((step, call, result)).or_default() += 1;
    };
    let told = |port: HANDLE, ms: u32, bit: u32| notices_until(port, ms, bit).iter().any(|b| b & bit != 0);
    note("arm", change_as(port, a, input, SOCK_NOTIFY_OP_ENABLE, once));
    let b = [1u8];
    // SAFETY: a byte of ours.
    unsafe { send(c, b.as_ptr(), 1, 0) };
    t.told_first += usize::from(told(port, 1000, SOCK_NOTIFY_EVENT_IN));
    // Disarmed by that notice: told nothing more, though the byte is still there.
    t.told_disarmed += usize::from(told(port, 20, SOCK_NOTIFY_EVENT_IN));
    note(
        "arm again",
        change_as(port, a, input, SOCK_NOTIFY_OP_ENABLE, once),
    );
    t.told_again += usize::from(told(port, 1000, SOCK_NOTIFY_EVENT_IN));
    note(
        "arm for output",
        change_as(port, a, output, SOCK_NOTIFY_OP_ENABLE, once),
    );
    t.told_out += usize::from(told(port, 1000, SOCK_NOTIFY_EVENT_OUT));
    // Armed for input with nothing to read, then overwritten for output, which holds.
    let mut got = [0u8; 1];
    // SAFETY: a byte's room.
    unsafe { recv(a, got.as_mut_ptr(), 1, 0) };
    note(
        "arm, nothing to read",
        change_as(port, a, input, SOCK_NOTIFY_OP_ENABLE, once),
    );
    note(
        "overwrite",
        change_as(port, a, output, SOCK_NOTIFY_OP_ENABLE, once),
    );
    t.told_overwritten += usize::from(told(port, 1000, SOCK_NOTIFY_EVENT_OUT));
    change(port, a, 0, SOCK_NOTIFY_OP_REMOVE);
    close_now(a);
    close_now(c);
    // SAFETY: our port.
    unsafe { CloseHandle(port) };
    Ok(())
}

fn main() {
    let mut data = WSADATA::default();
    // SAFETY: Winsock 2.2's start.
    if unsafe { WSAStartup(0x0202, &raw mut data) } != 0 {
        let _ = writeln!(std::io::stdout(), "WSAStartup failed");
        std::process::exit(1);
    }
    let (l, at) = match listener() {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(std::io::stdout(), "M215: {e}");
            std::process::exit(1);
        }
    };
    for way in [
        Way::Removed,
        Way::RemovedNoticed,
        Way::Disabled,
        Way::FilteredNone,
    ] {
        let mut tally = Tally::default();
        for _ in 0..ROUNDS {
            if let Err(e) = round(l, &at, way, &mut tally) {
                let _ = writeln!(std::io::stdout(), "M215 {way:?}: {e}");
                std::process::exit(1);
            }
        }
        let mut out = std::io::stdout();
        let _ = writeln!(
            out,
            "M215 {way:?}: of {ROUNDS} rounds, told while waiting for nothing {}, told once waiting again {}",
            tally.told_while_none, tally.told_after
        );
        for ((step, call, result), n) in &tally.results {
            let _ = writeln!(
                out,
                "M215 {way:?}:   {step}: call {call} registration {result}: {n}"
            );
        }
        for (step, mut us) in tally.costs {
            us.sort_by(f64::total_cmp);
            let q = |p: usize| {
                us.get((us.len() * p / 100).min(us.len() - 1))
                    .copied()
                    .unwrap_or(0.0)
            };
            let _ = writeln!(
                out,
                "M215 {way:?}:   {step:<14} p50 {:6.1} p90 {:6.1} p99 {:6.1} max {:7.1} us (n={})",
                q(50),
                q(90),
                q(99),
                us.last().copied().unwrap_or(0.0),
                us.len()
            );
        }
    }
    let mut t = OneShot::default();
    for _ in 0..ROUNDS {
        if let Err(e) = oneshot_round(l, &at, &mut t) {
            let _ = writeln!(std::io::stdout(), "M215 one-shot: {e}");
            std::process::exit(1);
        }
    }
    let mut out = std::io::stdout();
    let _ = writeln!(
        out,
        "M215 one-shot: of {ROUNDS} rounds, told first {}, told while disarmed {}, told once armed again {}, told output once armed for it {}, told output once overwritten for it {}",
        t.told_first, t.told_disarmed, t.told_again, t.told_out, t.told_overwritten
    );
    for ((step, call, result), n) in &t.results {
        let _ = writeln!(
            out,
            "M215 one-shot:   {step}: call {call} registration {result}: {n}"
        );
    }
}

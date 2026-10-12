//! What waking a network process's loop costs on Windows, by how it waits (PM M213, D142).
//! Its loop waits on its sockets, its VM's doorbell and its VM's process.
//!
//! - Sockets beside N idle connections (N = 0, 100, 1,000, 10,000), a byte's round trip on
//!   one more: the loop waits as shards' poller does (crates/net/src/poll.rs), one event
//!   every socket's network events set (WSAEventSelect) and a look at every socket after
//!   each wake (WSAPoll); or on an I/O completion port its sockets are registered with
//!   (ProcessSocketNotifications, level-triggered), which names those that are ready.
//! - A doorbell's round trip, nothing else waited on: two auto-reset events, as shards'
//!   ring rings (PM M204); a byte on an AF_UNIX socket, waited on with recv and with a
//!   completion port; and an event a thread-pool wait turns into a completion packet
//!   (RegisterWaitForSingleObject, PostQueuedCompletionStatus), as a loop on a port would
//!   need for the ring's events.
//!
//! Each case 10,000 round trips between two threads, timed by the side that starts them.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::io::Write as _;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Networking::WinSock::*;
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY, PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, RegisterWaitForSingleObject, SetEvent, UnregisterWaitEx, WT_EXECUTEINWAITTHREAD,
    WaitForSingleObject,
};

const ROUNDS: usize = 10_000;

fn wsa() -> String {
    // SAFETY: no arguments.
    let e = unsafe { WSAGetLastError() };
    format!("WSA error {e} ({})", std::io::Error::from_raw_os_error(e))
}

/// Round trips in µs, as p50 / p90 / p99 / max.
fn report(what: &str, mut us: Vec<f64>) {
    us.sort_by(f64::total_cmp);
    let q = |p: usize| {
        us.get((us.len() * p / 100).min(us.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0)
    };
    let _ = writeln!(
        std::io::stdout(),
        "{what:<52} p50 {:7.1} p90 {:7.1} p99 {:7.1} max {:8.1} us (n={})",
        q(50),
        q(90),
        q(99),
        us.last().copied().unwrap_or(0.0),
        us.len()
    );
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

/// A connected pair over the loopback: (client, accepted).
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
        let one: u32 = 1;
        setsockopt(c, IPPROTO_TCP, TCP_NODELAY, (&raw const one).cast(), 4);
        setsockopt(a, IPPROTO_TCP, TCP_NODELAY, (&raw const one).cast(), 4);
        Ok((c, a))
    }
}

/// Closes `s` at once (a linger of 0): no TIME_WAIT holds its port for the next case.
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

fn byte_send(s: SOCKET) -> bool {
    let b = [1u8];
    // SAFETY: a byte of ours.
    unsafe { send(s, b.as_ptr(), 1, 0) == 1 }
}

fn byte_recv(s: SOCKET) -> bool {
    let mut b = [0u8];
    // SAFETY: a byte's room.
    unsafe { recv(s, b.as_mut_ptr(), 1, 0) == 1 }
}

/// How the loop side waits on its sockets.
#[derive(Clone, Copy)]
enum Waits {
    /// One event and a look at every socket (shards' poller).
    EventPoll,
    /// A completion port that names the ready ones.
    Notifications,
    /// The one event to wake on, as shards' poller has, and the completion port, asked
    /// without waiting, to name the ready ones: the doorbell's event and the VM's process
    /// stay waited on beside it.
    EventNotifications,
}

/// Registers `served` with a new completion port, level-triggered, each under its index.
fn register(served: &[SOCKET]) -> Result<HANDLE, String> {
    // SAFETY: a new port.
    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    if port.is_null() {
        return Err("CreateIoCompletionPort".into());
    }
    let mut regs: Vec<SOCK_NOTIFY_REGISTRATION> = served
        .iter()
        .enumerate()
        .map(|(i, &socket)| SOCK_NOTIFY_REGISTRATION {
            socket,
            completionKey: std::ptr::without_provenance_mut(i),
            eventFilter: (SOCK_NOTIFY_REGISTER_EVENT_IN | SOCK_NOTIFY_REGISTER_EVENT_HANGUP) as u16,
            operation: SOCK_NOTIFY_OP_ENABLE as u8,
            triggerFlags: (SOCK_NOTIFY_TRIGGER_PERSISTENT | SOCK_NOTIFY_TRIGGER_LEVEL) as u8,
            registrationResult: 0,
        })
        .collect();
    for chunk in regs.chunks_mut(1024) {
        // SAFETY: registrations of our sockets with our port; no wait.
        let r = unsafe {
            ProcessSocketNotifications(
                port,
                chunk.len() as u32,
                chunk.as_mut_ptr(),
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if r != 0 && r != 258 {
            let each = chunk.first().map_or(0, |c| c.registrationResult);
            return Err(format!(
                "ProcessSocketNotifications: {r} (a registration's result {each})"
            ));
        }
    }
    Ok(port)
}

/// A byte's round trip on one connection beside `idle` others, the loop waiting as `how`.
fn beside(idle: usize, how: Waits) -> Result<Vec<f64>, String> {
    let (l, at) = listener()?;
    let mut clients = Vec::with_capacity(idle + 1);
    let mut served = Vec::with_capacity(idle + 1);
    for _ in 0..=idle {
        let (c, a) = pair(l, &at)?;
        clients.push(c);
        served.push(a);
    }
    let active = idle;
    let (Some(&ping), Some(&pong)) = (clients.get(active), served.get(active)) else {
        return Err("no active pair".into());
    };
    let result = std::thread::scope(|s| -> Result<Vec<f64>, String> {
        let server = s.spawn(|| -> Result<(), String> {
            match how {
                Waits::EventPoll => {
                    // SAFETY: a manual-reset event of ours.
                    let ev = unsafe { WSACreateEvent() };
                    for &x in &served {
                        // SAFETY: our socket and event.
                        if unsafe { WSAEventSelect(x, ev, (FD_READ | FD_CLOSE) as i32) } != 0 {
                            return Err(format!("WSAEventSelect: {}", wsa()));
                        }
                    }
                    let mut fds: Vec<WSAPOLLFD> = served
                        .iter()
                        .map(|&fd| WSAPOLLFD {
                            fd,
                            events: POLLRDNORM,
                            revents: 0,
                        })
                        .collect();
                    let mut answered = 0;
                    while answered < ROUNDS {
                        // SAFETY: our event; an array of its own length, no wait.
                        unsafe { WSAResetEvent(ev) };
                        for f in &mut fds {
                            f.revents = 0;
                        }
                        // SAFETY: an array of its own length; no wait.
                        let n = unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, 0) };
                        if n < 0 {
                            return Err(format!("WSAPoll: {}", wsa()));
                        }
                        if fds
                            .get(active)
                            .is_some_and(|f| f.revents & (POLLHUP | POLLERR) != 0)
                        {
                            return Err("the pinging side went".into());
                        }
                        if fds.get(active).is_some_and(|f| f.revents & POLLRDNORM != 0) {
                            if !byte_recv(pong) || !byte_send(pong) {
                                return Err(format!("echo: {}", wsa()));
                            }
                            answered += 1;
                            continue;
                        }
                        let h = ev as HANDLE;
                        // SAFETY: our event, as the handle it is.
                        unsafe { WSAWaitForMultipleEvents(1, &raw const h, 0, INFINITE, 0) };
                    }
                    // SAFETY: our event.
                    unsafe { WSACloseEvent(ev) };
                }
                Waits::Notifications => {
                    // SAFETY: a new port.
                    let port =
                        unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
                    if port.is_null() {
                        return Err("CreateIoCompletionPort".into());
                    }
                    let mut regs: Vec<SOCK_NOTIFY_REGISTRATION> = served
                        .iter()
                        .enumerate()
                        .map(|(i, &socket)| SOCK_NOTIFY_REGISTRATION {
                            socket,
                            completionKey: std::ptr::without_provenance_mut(i),
                            eventFilter: (SOCK_NOTIFY_REGISTER_EVENT_IN | SOCK_NOTIFY_REGISTER_EVENT_HANGUP)
                                as u16,
                            operation: SOCK_NOTIFY_OP_ENABLE as u8,
                            triggerFlags: (SOCK_NOTIFY_TRIGGER_PERSISTENT | SOCK_NOTIFY_TRIGGER_LEVEL) as u8,
                            registrationResult: 0,
                        })
                        .collect();
                    for chunk in regs.chunks_mut(1024) {
                        // SAFETY: registrations of our sockets with our port; no wait.
                        let r = unsafe {
                            ProcessSocketNotifications(
                                port,
                                chunk.len() as u32,
                                chunk.as_mut_ptr(),
                                0,
                                0,
                                std::ptr::null_mut(),
                                std::ptr::null_mut(),
                            )
                        };
                        if r != 0 && r != 258 {
                            return Err(format!("ProcessSocketNotifications: {r}"));
                        }
                    }
                    let mut entries = vec![OVERLAPPED_ENTRY::default(); 64];
                    let mut answered = 0;
                    while answered < ROUNDS {
                        let mut got = 0u32;
                        // SAFETY: our port and an array of its own length.
                        let r = unsafe {
                            ProcessSocketNotifications(
                                port,
                                0,
                                std::ptr::null_mut(),
                                INFINITE,
                                entries.len() as u32,
                                entries.as_mut_ptr(),
                                &raw mut got,
                            )
                        };
                        if r != 0 {
                            return Err(format!("ProcessSocketNotifications wait: {r}"));
                        }
                        for e in entries.iter().take(got as usize) {
                            if e.lpCompletionKey == active {
                                if !byte_recv(pong) || !byte_send(pong) {
                                    return Err(format!("echo: {}", wsa()));
                                }
                                answered += 1;
                            }
                        }
                    }
                    // SAFETY: our port.
                    unsafe { CloseHandle(port) };
                }
                Waits::EventNotifications => {
                    // SAFETY: a manual-reset event of ours.
                    let ev = unsafe { WSACreateEvent() };
                    for &x in &served {
                        // SAFETY: our socket and event.
                        if unsafe { WSAEventSelect(x, ev, (FD_READ | FD_CLOSE) as i32) } != 0 {
                            return Err(format!("WSAEventSelect: {}", wsa()));
                        }
                    }
                    let port = register(&served)?;
                    let mut entries = vec![OVERLAPPED_ENTRY::default(); 64];
                    let (mut answered, mut empty) = (0, 0u32);
                    while answered < ROUNDS {
                        // SAFETY: our event.
                        unsafe { WSAResetEvent(ev) };
                        let mut got = 0u32;
                        // SAFETY: our port and an array of its own length; no wait.
                        let r = unsafe {
                            ProcessSocketNotifications(
                                port,
                                0,
                                std::ptr::null_mut(),
                                0,
                                entries.len() as u32,
                                entries.as_mut_ptr(),
                                &raw mut got,
                            )
                        };
                        if r != 0 && r != 258 {
                            return Err(format!("ProcessSocketNotifications look: {r}"));
                        }
                        if entries
                            .iter()
                            .take(got as usize)
                            .any(|e| e.lpCompletionKey == active)
                        {
                            if !byte_recv(pong) || !byte_send(pong) {
                                return Err(format!("echo: {}", wsa()));
                            }
                            answered += 1;
                            empty = 0;
                            continue;
                        }
                        // A wake the port named nothing for, again and again: the two do
                        // not hold one socket together. A wait of 100 ms at most, so that
                        // an event that never comes is seen too.
                        empty += 1;
                        if empty > 50 {
                            return Err(format!("the port named nothing after {answered} round trips"));
                        }
                        let h = ev as HANDLE;
                        // SAFETY: our event, as the handle it is.
                        unsafe { WSAWaitForMultipleEvents(1, &raw const h, 0, 100, 0) };
                    }
                    // SAFETY: our port and event.
                    unsafe {
                        CloseHandle(port);
                        WSACloseEvent(ev);
                    }
                }
            }
            Ok(())
        });
        let mut us = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let t = Instant::now();
            if !byte_send(ping) || !byte_recv(ping) {
                // SAFETY: our socket: its end ends the loop's side.
                unsafe { shutdown(ping, SD_BOTH) };
                break;
            }
            us.push(t.elapsed().as_secs_f64() * 1e6);
        }
        server.join().map_err(|_| "the loop's thread".to_string())??;
        Ok(us)
    });
    for s in clients.into_iter().chain(served) {
        close_now(s);
    }
    // SAFETY: our listener.
    unsafe { closesocket(l) };
    result
}

fn event() -> HANDLE {
    // SAFETY: an unnamed auto-reset event.
    unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) }
}

/// Two auto-reset events, rung in turn.
fn events() -> Vec<f64> {
    let (a, b) = (event(), event());
    let (a_, b_) = (a as usize, b as usize);
    std::thread::scope(|s| {
        s.spawn(move || {
            for _ in 0..ROUNDS {
                // SAFETY: our events.
                unsafe {
                    WaitForSingleObject(a_ as HANDLE, INFINITE);
                    SetEvent(b_ as HANDLE);
                }
            }
        });
        (0..ROUNDS)
            .map(|_| {
                let t = Instant::now();
                // SAFETY: our events.
                unsafe {
                    SetEvent(a);
                    WaitForSingleObject(b, INFINITE);
                }
                t.elapsed().as_secs_f64() * 1e6
            })
            .collect()
    })
}

unsafe extern "system" fn to_port(context: *mut core::ffi::c_void, _: bool) {
    // SAFETY: the port the registration was given, a packet with no overlapped.
    unsafe { PostQueuedCompletionStatus(context as HANDLE, 0, 1, std::ptr::null()) };
}

/// An event a thread-pool wait turns into a completion packet, answered by an event.
fn event_to_port() -> Result<Vec<f64>, String> {
    let (a, b) = (event(), event());
    // SAFETY: a new port.
    let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
    let mut wait: HANDLE = std::ptr::null_mut();
    // SAFETY: our event and port, the callback posting to it in the wait's own thread.
    if unsafe {
        RegisterWaitForSingleObject(
            &raw mut wait,
            a,
            Some(to_port),
            port,
            INFINITE,
            WT_EXECUTEINWAITTHREAD,
        )
    } == 0
    {
        return Err("RegisterWaitForSingleObject".into());
    }
    let (port_, b_) = (port as usize, b as usize);
    let us = std::thread::scope(|s| {
        s.spawn(move || {
            let mut entries = [OVERLAPPED_ENTRY::default(); 4];
            let mut done = 0;
            while done < ROUNDS {
                let mut got = 0u32;
                // SAFETY: our port.
                unsafe {
                    GetQueuedCompletionStatusEx(
                        port_ as HANDLE,
                        entries.as_mut_ptr(),
                        4,
                        &raw mut got,
                        INFINITE,
                        0,
                    );
                }
                for _ in 0..got {
                    // SAFETY: our event.
                    unsafe { SetEvent(b_ as HANDLE) };
                    done += 1;
                }
            }
        });
        (0..ROUNDS)
            .map(|_| {
                let t = Instant::now();
                // SAFETY: our events.
                unsafe {
                    SetEvent(a);
                    WaitForSingleObject(b, INFINITE);
                }
                t.elapsed().as_secs_f64() * 1e6
            })
            .collect()
    });
    // SAFETY: our wait, ended before the event goes; our handles.
    unsafe {
        UnregisterWaitEx(wait, INVALID_HANDLE_VALUE);
        CloseHandle(port);
    }
    Ok(us)
}

/// An AF_UNIX socket pair, as shards' afunix makes one: a listener at a path, a connect and
/// an accept.
fn unix_pair(dir: &std::path::Path) -> Result<(SOCKET, SOCKET), String> {
    let path = dir.join("m213.sock");
    let _ = std::fs::remove_file(&path);
    let mut addr = SOCKADDR_UN {
        sun_family: AF_UNIX,
        sun_path: [0; 108],
    };
    let bytes = path.to_string_lossy().into_owned().into_bytes();
    for (to, b) in addr.sun_path.iter_mut().zip(&bytes) {
        *to = *b as i8;
    }
    let len = std::mem::size_of::<SOCKADDR_UN>() as i32;
    // SAFETY: plain arguments; an address of its own length; our sockets.
    unsafe {
        let l = WSASocketW(
            i32::from(AF_UNIX),
            SOCK_STREAM,
            0,
            std::ptr::null(),
            0,
            WSA_FLAG_NO_HANDLE_INHERIT,
        );
        let c = WSASocketW(
            i32::from(AF_UNIX),
            SOCK_STREAM,
            0,
            std::ptr::null(),
            0,
            WSA_FLAG_NO_HANDLE_INHERIT,
        );
        if l == INVALID_SOCKET || c == INVALID_SOCKET {
            return Err(format!("AF_UNIX socket: {}", wsa()));
        }
        if bind(l, (&raw const addr).cast(), len) != 0 || listen(l, 1) != 0 {
            return Err(format!("AF_UNIX bind/listen: {}", wsa()));
        }
        if connect(c, (&raw const addr).cast(), len) != 0 {
            return Err(format!("AF_UNIX connect: {}", wsa()));
        }
        let a = accept(l, std::ptr::null_mut(), std::ptr::null_mut());
        closesocket(l);
        let _ = std::fs::remove_file(&path);
        if a == INVALID_SOCKET {
            return Err(format!("AF_UNIX accept: {}", wsa()));
        }
        Ok((c, a))
    }
}

/// A byte's round trip on an AF_UNIX pair, the answering side waiting in recv, or on a
/// completion port.
fn unix(dir: &std::path::Path, port_side: bool) -> Result<Vec<f64>, String> {
    let (c, a) = unix_pair(dir)?;
    let us = std::thread::scope(|s| -> Result<Vec<f64>, String> {
        let server = s.spawn(move || -> Result<(), String> {
            if !port_side {
                for _ in 0..ROUNDS {
                    if !byte_recv(a) || !byte_send(a) {
                        return Err(format!("echo: {}", wsa()));
                    }
                }
                return Ok(());
            }
            // SAFETY: a new port; our socket registered with it.
            let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
            let mut reg = SOCK_NOTIFY_REGISTRATION {
                socket: a,
                completionKey: std::ptr::null_mut(),
                eventFilter: SOCK_NOTIFY_REGISTER_EVENT_IN as u16,
                operation: SOCK_NOTIFY_OP_ENABLE as u8,
                triggerFlags: (SOCK_NOTIFY_TRIGGER_PERSISTENT | SOCK_NOTIFY_TRIGGER_LEVEL) as u8,
                registrationResult: 0,
            };
            // SAFETY: one registration, no wait.
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
            if r != 0 && r != 258 {
                return Err(format!(
                    "ProcessSocketNotifications: {r} ({})",
                    reg.registrationResult
                ));
            }
            let mut entries = [OVERLAPPED_ENTRY::default(); 4];
            let mut done = 0;
            while done < ROUNDS {
                let mut got = 0u32;
                // SAFETY: our port.
                let r = unsafe {
                    ProcessSocketNotifications(
                        port,
                        0,
                        std::ptr::null_mut(),
                        INFINITE,
                        4,
                        entries.as_mut_ptr(),
                        &raw mut got,
                    )
                };
                if r != 0 {
                    return Err(format!("ProcessSocketNotifications wait: {r}"));
                }
                if got > 0 {
                    if !byte_recv(a) || !byte_send(a) {
                        return Err(format!("echo: {}", wsa()));
                    }
                    done += 1;
                }
            }
            // SAFETY: our port.
            unsafe { CloseHandle(port) };
            Ok(())
        });
        let mut us = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let t = Instant::now();
            if !byte_send(c) || !byte_recv(c) {
                // SAFETY: our socket: its end ends the answering side.
                unsafe { shutdown(c, SD_BOTH) };
                break;
            }
            us.push(t.elapsed().as_secs_f64() * 1e6);
        }
        server.join().map_err(|_| "the answering thread".to_string())??;
        Ok(us)
    });
    // SAFETY: our sockets.
    unsafe {
        closesocket(c);
        closesocket(a);
    }
    us
}

fn main() {
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    unsafe { WSAStartup(0x0202, &raw mut data) };
    let mut out = std::io::stdout();
    let _ = writeln!(out, "# M213: waking a network process's loop on Windows");
    report("doorbell: two auto-reset events", events());
    match event_to_port() {
        Ok(us) => report("doorbell: event -> thread-pool wait -> port", us),
        Err(e) => {
            let _ = writeln!(out, "doorbell: event -> port: {e}");
        }
    }
    let dir = std::env::temp_dir();
    for (what, port) in [
        ("doorbell: AF_UNIX byte, recv", false),
        ("doorbell: AF_UNIX byte, port", true),
    ] {
        match unix(&dir, port) {
            Ok(us) => report(what, us),
            Err(e) => {
                let _ = writeln!(out, "{what}: {e}");
            }
        }
    }
    for idle in [0usize, 100, 1_000, 10_000] {
        for (name, how) in [
            ("event + WSAPoll", Waits::EventPoll),
            (
                "event + ProcessSocketNotifications look",
                Waits::EventNotifications,
            ),
            ("ProcessSocketNotifications", Waits::Notifications),
        ] {
            let what = format!("socket beside {idle} idle: {name}");
            match beside(idle, how) {
                Ok(us) => report(&what, us),
                Err(e) => {
                    let _ = writeln!(out, "{what}: {e}");
                }
            }
        }
    }
}

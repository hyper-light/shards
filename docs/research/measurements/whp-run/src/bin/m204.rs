//! What a VM's network process may do on Windows confined as D141 confines a VM process,
//! before D142 is built on it (PM M204). A network process parses what a guest and the
//! Internet send, so it is confined too; it must reach the Internet, serve what a run
//! publishes, and exchange frames with its VM process through shared memory and doorbells.
//! A child in an AppContainer of its own, in a Job, under the mitigations (M201), is made
//! twice: holding no capability, then holding the network capabilities (internetClient,
//! internetClientServer, privateNetworkClientServer). Each time it tries:
//!
//! - a TCP connection to an Internet address its spawner resolved, and a DNS query by UDP
//!   to a public resolver;
//! - a connection to a loopback listener of its spawner's, as a guest's connection to a
//!   host service would be, and one from its spawner to a loopback listener of its own, as a
//!   published port's would be; and to its own listener on a non-loopback address;
//! - accepting on a loopback listener its spawner bound and shared with it
//!   (WSADuplicateSocketW), its spawner connecting.
//!
//! Then frames' doorbells: a section (pagefile-backed shared memory) and two events, handed
//! by DuplicateHandle; the child counts in the memory as the doorbells ring, 10,000 round
//! trips timed at the spawner, each a SetEvent and a wait on each side.

// Windows' constants, matched by their own names.
#![allow(non_upper_case_globals)]

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddrV4, ToSocketAddrs};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Networking::WinSock::*;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE,
    UnmapViewOfFile,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetExitCodeProcess, INFINITE, ResumeThread, SetEvent, WaitForSingleObject,
};
use whp_run::confine::{Container, job, last};

/// The doorbells' round trips.
const ROUNDS: usize = 10_000;
/// The shared memory: a page for the count, as netring's controls share a page.
const SECTION: usize = 64 << 10;

/// A capability SID S-1-15-3-`rid`, as bytes: the network capabilities are 1
/// (internetClient), 2 (internetClientServer) and 3 (privateNetworkClientServer).
fn capability(rid: u32) -> Vec<u8> {
    let mut sid = vec![1u8, 2, 0, 0, 0, 0, 0, 15];
    sid.extend(3u32.to_le_bytes());
    sid.extend(rid.to_le_bytes());
    sid
}

fn startup() {
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    unsafe { WSAStartup(0x0202, &raw mut data) };
}

fn wsa() -> String {
    // SAFETY: no arguments.
    let e = unsafe { WSAGetLastError() };
    format!("WSA error {e} ({})", std::io::Error::from_raw_os_error(e))
}

fn sockaddr(a: SocketAddrV4) -> SOCKADDR_IN {
    SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: a.port().to_be(),
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes(a.ip().octets()),
            },
        },
        sin_zero: [0; 8],
    }
}

fn socket(kind: WINSOCK_SOCKET_TYPE) -> Result<SOCKET, String> {
    // SAFETY: plain arguments; no child inherits it.
    let s = unsafe { WSASocketW(i32::from(AF_INET), kind, 0, std::ptr::null(), 0, WSA_FLAG_NO_HANDLE_INHERIT) };
    if s == INVALID_SOCKET {
        return Err(format!("WSASocketW: {}", wsa()));
    }
    let ms: u32 = 5_000;
    // SAFETY: an option of its own size.
    unsafe { setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, (&raw const ms).cast(), 4) };
    Ok(s)
}

fn close(s: SOCKET) {
    // SAFETY: a socket of ours.
    unsafe { closesocket(s) };
}

/// A TCP connection to `to`, timed.
fn connect_to(to: SocketAddrV4) -> Result<(SOCKET, Duration), String> {
    let s = socket(SOCK_STREAM)?;
    let a = sockaddr(to);
    let t = Instant::now();
    // SAFETY: an address of its own length.
    if unsafe { connect(s, (&raw const a).cast(), std::mem::size_of::<SOCKADDR_IN>() as i32) } != 0 {
        let e = wsa();
        close(s);
        return Err(e);
    }
    Ok((s, t.elapsed()))
}

/// A TCP listener at `at`, and the address it took.
fn listen_at(at: SocketAddrV4) -> Result<(SOCKET, SocketAddrV4), String> {
    let s = socket(SOCK_STREAM)?;
    let a = sockaddr(at);
    // SAFETY: an address of its own length; then the name it took, into one.
    unsafe {
        if bind(s, (&raw const a).cast(), std::mem::size_of::<SOCKADDR_IN>() as i32) != 0 {
            let e = format!("bind {at}: {}", wsa());
            close(s);
            return Err(e);
        }
        if listen(s, 8) != 0 {
            let e = format!("listen: {}", wsa());
            close(s);
            return Err(e);
        }
        let mut name = SOCKADDR_IN::default();
        let mut len = std::mem::size_of::<SOCKADDR_IN>() as i32;
        getsockname(s, (&raw mut name).cast(), &raw mut len);
        let port = u16::from_be(name.sin_port);
        Ok((s, SocketAddrV4::new(*at.ip(), port)))
    }
}

/// The next connection on `l` within `within`, or why there was none.
fn accept_within(l: SOCKET, within: Duration) -> Result<SOCKET, String> {
    let mut on = 1u32;
    // SAFETY: our listener, polled from here on.
    unsafe { ioctlsocket(l, FIONBIO, &raw mut on) };
    let until = Instant::now() + within;
    loop {
        // SAFETY: a listening socket; no address asked.
        let a = unsafe { accept(l, std::ptr::null_mut(), std::ptr::null_mut()) };
        if a != INVALID_SOCKET {
            let mut off = 0u32;
            // SAFETY: back to blocking.
            unsafe { ioctlsocket(a, FIONBIO, &raw mut off) };
            return Ok(a);
        }
        // SAFETY: no arguments.
        let e = unsafe { WSAGetLastError() };
        if e != WSAEWOULDBLOCK || Instant::now() > until {
            return Err(if e == WSAEWOULDBLOCK { "nothing came".into() } else { wsa() });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A DNS query for `github.com`'s A records, by UDP, to `resolver`: how many bytes came back.
fn dns(resolver: SocketAddrV4) -> Result<usize, String> {
    let s = socket(SOCK_DGRAM)?;
    let mut query = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in ["github", "com"] {
        query.push(label.len() as u8);
        query.extend(label.bytes());
    }
    query.extend([0, 0, 1, 0, 1]);
    let a = sockaddr(resolver);
    // SAFETY: the query's bytes, to an address of its own length.
    let sent = unsafe {
        sendto(s, query.as_ptr(), query.len() as i32, 0, (&raw const a).cast(), std::mem::size_of::<SOCKADDR_IN>() as i32)
    };
    if sent < 0 {
        let e = format!("sendto: {}", wsa());
        close(s);
        return Err(e);
    }
    let mut buf = [0u8; 512];
    // SAFETY: a buffer of its length.
    let n = unsafe { recv(s, buf.as_mut_ptr(), buf.len() as i32, 0) };
    let result = if n > 0 && buf[..2] == [0x12, 0x34] { Ok(n as usize) } else { Err(format!("recv {n}: {}", wsa())) };
    close(s);
    result
}

/// The address this host is reached at past its loopback: the one a UDP socket "connected"
/// to a public address is given.
fn outward_address() -> Result<Ipv4Addr, String> {
    let s = socket(SOCK_DGRAM)?;
    let a = sockaddr(SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53));
    // SAFETY: an address of its own length; then our name, into one.
    unsafe {
        connect(s, (&raw const a).cast(), std::mem::size_of::<SOCKADDR_IN>() as i32);
        let mut name = SOCKADDR_IN::default();
        let mut len = std::mem::size_of::<SOCKADDR_IN>() as i32;
        getsockname(s, (&raw mut name).cast(), &raw mut len);
        close(s);
        Ok(Ipv4Addr::from(name.sin_addr.S_un.S_addr.to_ne_bytes()))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

// ---- The child ----

fn child(pipe: usize) {
    startup();
    let say = |line: String| {
        let _ = writeln!(std::io::stdout(), "child: {line}");
    };
    let mut buf = vec![0u8; 8 << 10];
    let mut got = 0usize;
    loop {
        let mut n = 0u32;
        // SAFETY: the pipe's read end, given us; the rest of our buffer.
        let ok = unsafe { ReadFile(pipe as HANDLE, buf[got..].as_mut_ptr(), (buf.len() - got) as u32, &raw mut n, std::ptr::null_mut()) };
        if ok == 0 || n == 0 {
            break;
        }
        got += n as usize;
    }
    let text = String::from_utf8_lossy(&buf[..got]).into_owned();
    let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{name}="))).unwrap_or("").to_string();
    let addr = |name: &str| field(name).parse::<SocketAddrV4>().ok();

    if let Some(internet) = addr("internet") {
        say(format!("TCP to the Internet ({internet}): {:?}", connect_to(internet).map(|(s, t)| {
            close(s);
            format!("connected in {} us", t.as_micros())
        })));
    }
    say(format!("DNS by UDP to 1.1.1.1:53: {:?}", dns(SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 53))));
    if let Some(host) = addr("host") {
        say(format!("TCP to the spawner's loopback listener: {:?}", connect_to(host).map(|(s, _)| {
            close(s);
            "connected"
        })));
    }
    // Its own listeners, which the spawner dials once told their ports on stdout, then says
    // what it got by its own pipe: a line each.
    match (listen_at(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)), field("outward").parse::<Ipv4Addr>()) {
        (Ok((loopback, lport)), Ok(outward)) => {
            let outward_listener = listen_at(SocketAddrV4::new(outward, 0));
            let oport = outward_listener.as_ref().map(|(_, a)| a.port()).unwrap_or(0);
            say(format!("listening at {lport} and {outward}:{oport}"));
            say(format!("PORTS {} {oport}", lport.port()));
            let _ = std::io::stdout().flush();
            say(format!("its loopback listener accepted: {:?}", accept_within(loopback, Duration::from_secs(10)).map(close)));
            match outward_listener {
                Ok((l, _)) => say(format!("its outward listener accepted: {:?}", accept_within(l, Duration::from_secs(10)).map(close))),
                Err(e) => say(format!("its outward listener: {e}")),
            }
        }
        (l, o) => say(format!("listening: {:?} {:?}", l.map(|_| ()), o)),
    }
    // A listener its spawner bound and shared.
    match unhex(&field("shared")) {
        Some(info) if info.len() == std::mem::size_of::<WSAPROTOCOL_INFOW>() => {
            let mut parsed = WSAPROTOCOL_INFOW::default();
            // SAFETY: the structure's own size, from bytes of that length.
            unsafe { std::ptr::copy_nonoverlapping(info.as_ptr(), (&raw mut parsed).cast::<u8>(), info.len()) };
            // SAFETY: the info as WSADuplicateSocketW wrote it for this process.
            let s = unsafe { WSASocketW(FROM_PROTOCOL_INFO, FROM_PROTOCOL_INFO, FROM_PROTOCOL_INFO, &raw const parsed, 0, WSA_FLAG_NO_HANDLE_INHERIT) };
            if s == INVALID_SOCKET {
                say(format!("the shared listener: {}", wsa()));
            } else {
                say("SHARED READY".into());
                let _ = std::io::stdout().flush();
                say(format!("the shared listener accepted: {:?}", accept_within(s, Duration::from_secs(10)).map(close)));
            }
        }
        _ => say("the shared listener: no info".into()),
    }
    // The doorbells.
    let section = field("section").parse::<usize>().unwrap_or(0) as HANDLE;
    let (ring, sleep) = (field("ring").parse::<usize>().unwrap_or(0) as HANDLE, field("sleep").parse::<usize>().unwrap_or(0) as HANDLE);
    // SAFETY: the section given us, mapped whole.
    let view = unsafe { MapViewOfFile(section, FILE_MAP_ALL_ACCESS, 0, 0, SECTION) };
    if view.Value.is_null() {
        say(format!("mapping the section: {}", last()));
        return;
    }
    let count = view.Value.cast::<u64>();
    let mut rounds = 0;
    loop {
        // SAFETY: an event given us.
        if unsafe { WaitForSingleObject(sleep, 10_000) } != WAIT_OBJECT_0 {
            break;
        }
        // SAFETY: the count's place in the view, which only this side writes between rings.
        let seen = unsafe { std::ptr::read_volatile(count) };
        if seen == u64::MAX {
            break;
        }
        // SAFETY: as above.
        unsafe { std::ptr::write_volatile(count, seen + 1) };
        rounds += 1;
        // SAFETY: an event given us.
        unsafe { SetEvent(ring) };
    }
    say(format!("doorbell rounds answered: {rounds}"));
    // SAFETY: our view.
    unsafe { UnmapViewOfFile(view) };
}

// ---- The spawner ----

fn dup_into(h: HANDLE, process: HANDLE) -> Result<usize, String> {
    let mut out: HANDLE = std::ptr::null_mut();
    // SAFETY: our handle, duplicated into a process we made, of the same access.
    if unsafe { DuplicateHandle(GetCurrentProcess(), h, process, &raw mut out, 0, 0, DUPLICATE_SAME_ACCESS) } == 0 {
        return Err(format!("DuplicateHandle: {}", last()));
    }
    Ok(out as usize)
}

fn event() -> Result<HANDLE, String> {
    // SAFETY: an auto-reset event, unnamed.
    let e = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
    if e.is_null() {
        return Err(format!("CreateEventW: {}", last()));
    }
    Ok(e)
}

fn confined_run(label: &str, caps: &[Vec<u8>], internet: Option<SocketAddrV4>, outward: Ipv4Addr) -> Result<(), String> {
    println!("== {label}");
    let (host, host_addr) = listen_at(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let (shared, shared_addr) = listen_at(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    // SAFETY: a pagefile-backed section of SECTION bytes, closed below.
    let section = unsafe { CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, SECTION as u32, std::ptr::null()) };
    if section.is_null() {
        return Err(format!("CreateFileMappingW: {}", last()));
    }
    // SAFETY: the section, mapped whole.
    let view: MEMORY_MAPPED_VIEW_ADDRESS = unsafe { MapViewOfFile(section, FILE_MAP_ALL_ACCESS, 0, 0, SECTION) };
    if view.Value.is_null() {
        return Err(format!("MapViewOfFile: {}", last()));
    }
    let (ring, sleep) = (event()?, event()?);
    let container = Container::new("m204", label)?;
    let job = job()?;
    let (mut pr, mut pw): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: two handles for the call to fill.
    if unsafe { CreatePipe(&raw mut pr, &raw mut pw, std::ptr::null(), 0) } == 0 {
        return Err(format!("CreatePipe: {}", last()));
    }
    // The child's output: a pipe of its own, read here, for its ports.
    let (mut or, mut ow): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: as above.
    if unsafe { CreatePipe(&raw mut or, &raw mut ow, std::ptr::null(), 0) } == 0 {
        return Err(format!("CreatePipe: {}", last()));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let pi = whp_run::confine::spawn_with_output(&exe, &format!("child {}", pr as usize), container.sid, caps, job, &[pr], ow)?;
    let mut info = WSAPROTOCOL_INFOW::default();
    // SAFETY: our listener; a structure for the call to fill.
    if unsafe { WSADuplicateSocketW(shared, pi.dwProcessId, &raw mut info) } != 0 {
        return Err(format!("WSADuplicateSocketW: {}", wsa()));
    }
    // SAFETY: the structure's own bytes.
    let info = unsafe { std::slice::from_raw_parts((&raw const info).cast::<u8>(), std::mem::size_of::<WSAPROTOCOL_INFOW>()) };
    let given = format!(
        "{}host={host_addr}\noutward={outward}\nshared={}\nsection={}\nring={}\nsleep={}\n",
        internet.map(|a| format!("internet={a}\n")).unwrap_or_default(),
        hex(info),
        dup_into(section, pi.hProcess)?,
        dup_into(ring, pi.hProcess)?,
        dup_into(sleep, pi.hProcess)?,
    );
    // SAFETY: our pipe ends; the grants' bytes.
    unsafe {
        let mut written = 0u32;
        WriteFile(pw, given.as_ptr(), given.len() as u32, &raw mut written, std::ptr::null_mut());
        CloseHandle(pw);
        CloseHandle(pr);
        CloseHandle(ow);
        ResumeThread(pi.hThread);
    }
    // The child's lines, echoed; its ports dialled as it names them.
    let or = or as usize;
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let mut byte = [0u8; 1];
        let mut line = Vec::new();
        loop {
            let mut n = 0u32;
            // SAFETY: the read end of the child's output; one byte.
            if unsafe { ReadFile(or as HANDLE, byte.as_mut_ptr(), 1, &raw mut n, std::ptr::null_mut()) } == 0 || n == 0 {
                break;
            }
            if byte[0] != b'\n' {
                line.push(byte[0]);
                continue;
            }
            let l = String::from_utf8_lossy(&line).trim_end().to_string();
            line.clear();
            println!("{l}");
            if let Some(ports) = l.strip_prefix("child: PORTS ") {
                let mut it = ports.split(' ').filter_map(|p| p.parse::<u16>().ok());
                let (lp, op) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
                let loopback = connect_to(SocketAddrV4::new(Ipv4Addr::LOCALHOST, lp));
                println!("spawner: dialled its loopback listener: {:?}", loopback.as_ref().map(|_| "connected"));
                if let Ok((s, _)) = loopback {
                    close(s);
                }
                let outward_dial = connect_to(SocketAddrV4::new(outward, op));
                println!("spawner: dialled its outward listener: {:?}", outward_dial.as_ref().map(|_| "connected"));
                if let Ok((s, _)) = outward_dial {
                    close(s);
                }
            }
            if l == "child: SHARED READY" {
                let d = connect_to(shared_addr);
                println!("spawner: dialled the shared listener: {:?}", d.as_ref().map(|_| "connected"));
                if let Ok((s, _)) = d {
                    close(s);
                }
            }
            text.push(l);
        }
        // SAFETY: the read end, closed once the child's output ended.
        unsafe { CloseHandle(or as HANDLE) };
        text
    });
    // The child's own listeners and the shared one are served by the reader; what comes
    // to ours, we take.
    let host_took = accept_within(host, Duration::from_secs(15));
    println!("spawner: its loopback listener took the child's connection: {:?}", host_took.as_ref().map(|_| ()));
    if let Ok(s) = host_took {
        close(s);
    }
    // The doorbells: a first round, untimed, which waits out the child's tests before them;
    // then the rounds timed.
    let count = view.Value.cast::<u64>();
    // SAFETY: the count's place, which only this side writes between rings; our events.
    let first = unsafe {
        std::ptr::write_volatile(count, u64::MAX - 1);
        SetEvent(sleep);
        WaitForSingleObject(ring, 60_000) == WAIT_OBJECT_0
    };
    if !first {
        println!("spawner: the child never rang");
    }
    let mut rounds = Vec::with_capacity(ROUNDS);
    let mut answered = first;
    for i in 0..if first { ROUNDS } else { 0 } {
        let t = Instant::now();
        // SAFETY: the count's place, which only this side writes between rings; our events.
        unsafe {
            std::ptr::write_volatile(count, i as u64);
            SetEvent(sleep);
            if WaitForSingleObject(ring, 20_000) != WAIT_OBJECT_0 {
                answered = false;
            }
        }
        rounds.push(t.elapsed());
        // SAFETY: as above.
        if !answered || unsafe { std::ptr::read_volatile(count) } != i as u64 + 1 {
            println!("spawner: round {i} unanswered");
            break;
        }
    }
    println!("spawner: doorbell round trips: {}", whp_run::summary(&mut rounds));
    // SAFETY: the count's place; our event: the child's last round.
    unsafe {
        std::ptr::write_volatile(count, u64::MAX);
        SetEvent(sleep);
    }
    let mut code = 0u32;
    // SAFETY: the child's handles, closed once it has ended; then ours.
    unsafe {
        WaitForSingleObject(pi.hProcess, INFINITE);
        GetExitCodeProcess(pi.hProcess, &raw mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        CloseHandle(job);
        CloseHandle(ring);
        CloseHandle(sleep);
        UnmapViewOfFile(view);
        CloseHandle(section);
    }
    let _ = reader.join();
    println!("spawner: the child exited {code:#x}");
    close(host);
    close(shared);
    drop(container);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("child") {
        child(args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0));
        return;
    }
    startup();
    let internet = ("github.com", 443)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.find_map(|a| match a {
            std::net::SocketAddr::V4(v4) => Some(v4),
            std::net::SocketAddr::V6(_) => None,
        }));
    let outward = match outward_address() {
        Ok(a) => a,
        Err(e) => {
            println!("the outward address: {e}");
            Ipv4Addr::LOCALHOST
        }
    };
    println!("the Internet's address: {internet:?}; this host's outward address: {outward}");
    let unconfined = connect_to(internet.unwrap_or(SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 443)));
    println!("unconfined, TCP to the Internet: {:?}", unconfined.map(|(s, t)| {
        close(s);
        format!("connected in {} us", t.as_micros())
    }));
    println!("unconfined, DNS by UDP: {:?}", dns(SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 53)));
    let network = [capability(1), capability(2), capability(3)];
    for (label, caps) in [("none", &[][..]), ("network", &network[..])] {
        if let Err(e) = confined_run(label, caps, internet, outward) {
            println!("{label}: {e}");
        }
    }
}

//! AF_UNIX stream sockets on a real Windows host (PM M199), before shards' Windows
//! transport (its vsock device's host side, its builder's sockets) is built on them:
//!
//! - a connected pair's cost, made as Windows must make one: no socketpair(), so a
//!   listener at a path, a connect, an accept, the path removed;
//! - half-close: whether a peer reads end-of-stream after shutdown(SD_SEND), and the other
//!   direction still carries data;
//! - what WSAPoll reports of one: before data, with data, after the peer's half-close and
//!   close, and writability as its send buffer fills and drains;
//! - scatter-gather through WSASend and WSARecv;
//! - what WSAEventSelect records of one: FD_WRITE as associated and after a send refused
//!   for want of room, FD_READ, FD_CLOSE on the peer's half-close, and whether a wait to
//!   write on a half-closed socket waits rather than wakes at once;
//! - a socket's inheritance into a child process, which writes through it;
//! - what a peer reads of what a process wrote before it exited: having closed nothing,
//!   having closed its socket, having shut its writing down, or both; and what a child
//!   writes through a socket whose parent closed its own copy first, by closesocket or by
//!   CloseHandle;
//! - throughput (1 GiB in 64 KiB writes) and a one-byte round trip's latency, beside
//!   loopback TCP's;
//! - with `m205` (PM M205), a connect that does not wait: what it returns where a listener
//!   has room, where nothing listens and where the backlog is full, and how soon its
//!   socket may be written; and whether a blocking one waits on a full backlog;
//! - with `m207` (PM M207), whether a send waits while another thread waits to read the
//!   same connection: a WSAPoll for its data, a blocking recv, or one of SO_RCVTIMEO, on the
//!   socket or on a copy WSADuplicateSocketW made of it, the send through either; AF_UNIX
//!   and loopback TCP, made without WSA_FLAG_OVERLAPPED and with it;
//! - with `m358` (PM M358), what a peer reads of what a socket sent before it closed
//!   holding some of the peer's data unread: shut down first or not, drained first, the
//!   peer's data sent after the shutdown, the peer partly read, shut down both ways, with
//!   SO_LINGER; and where the peer writes after the close. AF_UNIX and loopback TCP.

use std::time::{Duration, Instant};

use windows_sys::Win32::Networking::WinSock::*;

const ROUNDS: usize = 10_000;
const PAIRS: usize = 100;
const VOLUME: usize = 1 << 30;
const CHUNK: usize = 64 << 10;

fn error(what: &str) -> String {
    // SAFETY: no arguments.
    format!("{what}: WSA error {}", unsafe { WSAGetLastError() })
}

fn startup() -> Result<(), String> {
    let mut data = WSADATA::default();
    // SAFETY: a WSADATA for the call to fill.
    if unsafe { WSAStartup(0x0202, &raw mut data) } != 0 {
        return Err("WSAStartup".into());
    }
    Ok(())
}

fn socket(family: u16) -> Result<SOCKET, String> {
    // SAFETY: plain arguments; the socket is inheritable, as no flag says otherwise.
    let s = unsafe { WSASocketW(i32::from(family), SOCK_STREAM, 0, std::ptr::null(), 0, 0) };
    if s == INVALID_SOCKET {
        return Err(error("WSASocketW"));
    }
    Ok(s)
}

fn unix_addr(path: &str) -> SOCKADDR_UN {
    let mut a = SOCKADDR_UN {
        sun_family: AF_UNIX,
        sun_path: [0; 108],
    };
    for (d, b) in a.sun_path.iter_mut().zip(path.bytes()) {
        *d = b as i8;
    }
    a
}

fn close(s: SOCKET) {
    // SAFETY: a socket this probe made.
    unsafe { closesocket(s) };
}

/// A connected AF_UNIX pair, through a listener at a fresh path in `dir`.
fn unix_pair(dir: &std::path::Path, n: usize) -> Result<(SOCKET, SOCKET), String> {
    let path = dir.join(format!("m199-{}-{n}.sock", std::process::id()));
    let path_s = path.to_string_lossy().to_string();
    let addr = unix_addr(&path_s);
    let len = std::mem::size_of::<SOCKADDR_UN>() as i32;
    let l = socket(AF_UNIX)?;
    // SAFETY: an address of its own length.
    if unsafe { bind(l, (&raw const addr).cast(), len) } != 0 {
        let e = error(&format!("bind {path_s}"));
        close(l);
        return Err(e);
    }
    // SAFETY: a bound socket.
    if unsafe { listen(l, 1) } != 0 {
        close(l);
        return Err(error("listen"));
    }
    let c = socket(AF_UNIX)?;
    // SAFETY: as bind.
    if unsafe { connect(c, (&raw const addr).cast(), len) } != 0 {
        let e = error("connect");
        close(l);
        close(c);
        return Err(e);
    }
    // SAFETY: a listening socket; no address asked.
    let a = unsafe { accept(l, std::ptr::null_mut(), std::ptr::null_mut()) };
    close(l);
    let _ = std::fs::remove_file(&path);
    if a == INVALID_SOCKET {
        close(c);
        return Err(error("accept"));
    }
    Ok((c, a))
}

/// A connected loopback TCP pair.
fn tcp_pair() -> Result<(SOCKET, SOCKET), String> {
    let mut addr = SOCKADDR_IN {
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
    let l = socket(AF_INET)?;
    // SAFETY: an address of its own length, filled back by getsockname.
    unsafe {
        if bind(l, (&raw const addr).cast(), len) != 0
            || listen(l, 1) != 0
            || getsockname(l, (&raw mut addr).cast(), &raw mut len) != 0
        {
            close(l);
            return Err(error("tcp listener"));
        }
    }
    let c = socket(AF_INET)?;
    // SAFETY: as above.
    if unsafe { connect(c, (&raw const addr).cast(), len) } != 0 {
        return Err(error("tcp connect"));
    }
    // SAFETY: a listening socket.
    let a = unsafe { accept(l, std::ptr::null_mut(), std::ptr::null_mut()) };
    close(l);
    if a == INVALID_SOCKET {
        return Err(error("tcp accept"));
    }
    let one: u32 = 1;
    // SAFETY: TCP_NODELAY on our own sockets, so a byte goes at once.
    unsafe {
        setsockopt(c, IPPROTO_TCP, TCP_NODELAY, (&raw const one).cast(), 4);
        setsockopt(a, IPPROTO_TCP, TCP_NODELAY, (&raw const one).cast(), 4);
    }
    Ok((c, a))
}

fn send_all(s: SOCKET, mut buf: &[u8]) -> Result<(), String> {
    while !buf.is_empty() {
        let n = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY: `n` bytes of `buf`.
        let sent = unsafe { send(s, buf.as_ptr(), n, 0) };
        if sent <= 0 {
            return Err(error("send"));
        }
        buf = &buf[sent as usize..];
    }
    Ok(())
}

fn recv_some(s: SOCKET, buf: &mut [u8]) -> Result<usize, String> {
    // SAFETY: `buf.len()` bytes of `buf`.
    let n = unsafe { recv(s, buf.as_mut_ptr(), buf.len().min(i32::MAX as usize) as i32, 0) };
    if n < 0 {
        return Err(error("recv"));
    }
    Ok(n as usize)
}

fn nonblocking(s: SOCKET) -> Result<(), String> {
    let mut on: u32 = 1;
    // SAFETY: FIONBIO with a u32.
    if unsafe { ioctlsocket(s, FIONBIO, &raw mut on) } != 0 {
        return Err(error("FIONBIO"));
    }
    Ok(())
}

/// What WSAPoll reports of `s` for reads and writes, now.
fn polled(s: SOCKET) -> String {
    let mut fd = WSAPOLLFD {
        fd: s,
        events: POLLRDNORM | POLLWRNORM,
        revents: 0,
    };
    // SAFETY: one descriptor, no wait.
    let n = unsafe { WSAPoll(&raw mut fd, 1, 0) };
    let bits = [
        (POLLRDNORM, "RDNORM"),
        (POLLRDBAND, "RDBAND"),
        (POLLWRNORM, "WRNORM"),
        (POLLHUP, "HUP"),
        (POLLERR, "ERR"),
        (POLLNVAL, "NVAL"),
    ];
    let names: Vec<&str> = bits
        .iter()
        .filter(|(b, _)| fd.revents & *b != 0)
        .map(|(_, n)| *n)
        .collect();
    format!("n={n} revents={:#x} [{}]", fd.revents, names.join("|"))
}

/// What WSAEventSelect recorded for `s` within `ms` milliseconds: the event reset, then
/// the socket's network events read (and cleared).
fn events(s: SOCKET, ev: WSAEVENT, ms: u32) -> String {
    let handle = ev as windows_sys::Win32::Foundation::HANDLE;
    // SAFETY: one event handle of ours.
    let waited = unsafe { WSAWaitForMultipleEvents(1, &raw const handle, 0, ms, 0) };
    if waited == WSA_WAIT_TIMEOUT {
        return "timeout".into();
    }
    // SAFETY: as above.
    unsafe { WSAResetEvent(ev) };
    let mut ne = WSANETWORKEVENTS::default();
    // SAFETY: a socket of ours, its events read into `ne`; the event already reset.
    if unsafe { WSAEnumNetworkEvents(s, 0, &raw mut ne) } != 0 {
        return error("WSAEnumNetworkEvents");
    }
    let names: Vec<&str> = [(FD_READ, "READ"), (FD_WRITE, "WRITE"), (FD_CLOSE, "CLOSE")]
        .iter()
        .filter(|(b, _)| ne.lNetworkEvents as u32 & *b != 0)
        .map(|(_, n)| *n)
        .collect();
    format!("[{}]", names.join("|"))
}

/// Fills `s`'s send buffer until a send is refused for want of room.
fn fill(s: SOCKET) -> usize {
    let chunk = [5u8; 4096];
    let mut filled = 0usize;
    while filled < 256 << 20 {
        // SAFETY: 4096 bytes of `chunk`.
        let n = unsafe { send(s, chunk.as_ptr(), chunk.len() as i32, 0) };
        if n <= 0 {
            break;
        }
        filled += n as usize;
    }
    filled
}

fn drain(s: SOCKET, mut left: usize) -> Result<(), String> {
    let mut sink = vec![0u8; 1 << 20];
    while left > 0 {
        let n = recv_some(s, &mut sink)?;
        if n == 0 {
            break;
        }
        left = left.saturating_sub(n);
    }
    Ok(())
}

fn event_select(dir: &std::path::Path) -> Result<(), String> {
    let (a, b) = unix_pair(dir, PAIRS + 10)?;
    // SAFETY: no arguments.
    let ev = unsafe { WSACreateEvent() };
    // SAFETY: a socket and an event of ours.
    if unsafe { WSAEventSelect(b, ev, (FD_READ | FD_WRITE | FD_CLOSE) as i32) } != 0 {
        return Err(error("WSAEventSelect"));
    }
    println!("select associated: {}", events(b, ev, 0));
    send_all(a, b"x")?;
    println!("select data: {}", events(b, ev, 1000));
    let mut one = [0u8; 1];
    let _ = recv_some(b, &mut one)?;
    let filled = fill(b);
    println!("select full ({filled} bytes): {}", events(b, ev, 100));
    drain(a, filled)?;
    println!("select drained: {}", events(b, ev, 1000));
    // SAFETY: our own socket.
    unsafe { shutdown(a, SD_SEND) };
    println!("select peer half-closed: {}", events(b, ev, 1000));
    let eof = recv_some(b, &mut one)?;
    let filled = fill(b);
    println!(
        "select full after end-of-stream (read {eof}, {filled} bytes): {}",
        events(b, ev, 100)
    );
    drain(a, filled)?;
    println!("select drained after end-of-stream: {}", events(b, ev, 1000));
    close(a);
    close(b);
    Ok(())
}

fn summary(v: &mut [Duration]) -> String {
    v.sort();
    let n = v.len();
    if n == 0 {
        return "n=0".into();
    }
    let rank = |p: usize| v[(n * p).div_ceil(100).clamp(1, n) - 1].as_secs_f64() * 1e6;
    format!(
        "n={n} p50={:.2} p90={:.2} p99={:.2} max={:.2}",
        rank(50),
        rank(90),
        rank(99),
        rank(100)
    )
}

fn throughput(label: &str, (a, b): (SOCKET, SOCKET)) -> Result<(), String> {
    let t = Instant::now();
    let total = std::thread::scope(|s| -> Result<usize, String> {
        let reader = s.spawn(move || -> Result<usize, String> {
            let mut buf = vec![0u8; CHUNK];
            let mut got = 0;
            while got < VOLUME {
                let n = recv_some(b, &mut buf)?;
                if n == 0 {
                    break;
                }
                got += n;
            }
            Ok(got)
        });
        let chunk = vec![7u8; CHUNK];
        for _ in 0..VOLUME / CHUNK {
            send_all(a, &chunk)?;
        }
        reader.join().map_err(|_| "reader".to_string())?
    })?;
    let secs = t.elapsed().as_secs_f64();
    println!(
        "throughput {label}: {total} bytes in {secs:.3} s = {:.0} MB/s",
        total as f64 / secs / 1e6
    );
    close(a);
    close(b);
    Ok(())
}

fn latency(label: &str, (a, b): (SOCKET, SOCKET)) -> Result<(), String> {
    let mut times = Vec::with_capacity(ROUNDS);
    std::thread::scope(|s| -> Result<(), String> {
        let echo = s.spawn(move || -> Result<(), String> {
            let mut byte = [0u8; 1];
            for _ in 0..ROUNDS {
                if recv_some(b, &mut byte)? != 1 {
                    return Err("echo: short".into());
                }
                send_all(b, &byte)?;
            }
            Ok(())
        });
        let mut byte = [1u8; 1];
        for _ in 0..ROUNDS {
            let t = Instant::now();
            send_all(a, &byte)?;
            if recv_some(a, &mut byte)? != 1 {
                return Err("ping: short".into());
            }
            times.push(t.elapsed());
        }
        echo.join().map_err(|_| "echo".to_string())?
    })?;
    println!("latency {label} {}", summary(&mut times));
    close(a);
    close(b);
    Ok(())
}

/// A child writing through an inherited socket, and how it ends; what the parent reads once
/// it has gone.
fn exits(dir: &std::path::Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    for (n, mode) in ["exit", "close", "shutdown", "shutdown-close"].iter().enumerate() {
        let (a, b) = unix_pair(dir, PAIRS + 20 + n)?;
        let status = std::process::Command::new(&exe)
            .args(["child-exit", &(a as usize).to_string(), mode])
            .status()
            .map_err(|e| e.to_string())?;
        close(a);
        std::thread::sleep(Duration::from_millis(100));
        let mut buf = [0u8; 64];
        let got = recv_some(b, &mut buf);
        println!(
            "exit after write ({mode}): child {status}; read {:?}",
            got.map(|n| String::from_utf8_lossy(&buf[..n]).into_owned())
        );
        close(b);
    }
    // The writer's exit closing the socket's last reference: the parent's copy closed
    // before it writes, read only once it has gone.
    for (n, mode) in ["exit", "shutdown-close"].iter().enumerate() {
        let (a, b) = unix_pair(dir, PAIRS + 40 + n)?;
        let mut child = std::process::Command::new(&exe)
            .args(["child-exit", &(a as usize).to_string(), mode])
            .spawn()
            .map_err(|e| e.to_string())?;
        close(a);
        let status = child.wait().map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(100));
        let mut buf = [0u8; 64];
        let got = recv_some(b, &mut buf);
        println!(
            "last reference closed by the writer's exit ({mode}): child {status}; read {:?}",
            got.map(|n| String::from_utf8_lossy(&buf[..n]).into_owned())
        );
        close(b);
    }
    for (n, how) in ["closesocket", "CloseHandle"].iter().enumerate() {
        let (a, b) = unix_pair(dir, PAIRS + 30 + n)?;
        let mut child = std::process::Command::new(&exe)
            .args(["child-late", &(a as usize).to_string()])
            .spawn()
            .map_err(|e| e.to_string())?;
        // SAFETY: our own copy of the socket the child inherited.
        unsafe {
            if *how == "closesocket" {
                closesocket(a);
            } else {
                windows_sys::Win32::Foundation::CloseHandle(a as windows_sys::Win32::Foundation::HANDLE);
            }
        }
        let mut buf = [0u8; 64];
        let got = recv_some(b, &mut buf);
        let status = child.wait().map_err(|e| e.to_string())?;
        println!(
            "parent's copy closed by {how} first: read {:?}; child {status}",
            got.map(|n| String::from_utf8_lossy(&buf[..n]).into_owned())
        );
        close(b);
    }
    Ok(())
}

fn probe() -> Result<(), String> {
    startup()?;
    let dir = std::env::temp_dir();
    // A pair's cost.
    let mut made = Vec::with_capacity(PAIRS);
    for n in 0..PAIRS {
        let t = Instant::now();
        let (a, b) = unix_pair(&dir, n)?;
        made.push(t.elapsed());
        close(a);
        close(b);
    }
    println!("unix pair {}", summary(&mut made));
    // Half-close.
    let (a, b) = unix_pair(&dir, PAIRS)?;
    send_all(a, b"one")?;
    // SAFETY: our own socket.
    let shut = unsafe { shutdown(a, SD_SEND) };
    let mut buf = [0u8; 16];
    let first = recv_some(b, &mut buf)?;
    let eof = recv_some(b, &mut buf)?;
    send_all(b, b"back")?;
    let back = recv_some(a, &mut buf)?;
    println!("half-close: shutdown={shut} peer read {first} then {eof} (0 is end-of-stream); the other way {back} bytes");
    close(a);
    close(b);
    // WSAPoll.
    let (a, b) = unix_pair(&dir, PAIRS + 1)?;
    nonblocking(b)?;
    println!("poll quiet: {}", polled(b));
    send_all(a, b"x")?;
    println!("poll data: {}", polled(b));
    let _ = recv_some(b, &mut buf)?;
    println!("poll drained: {}", polled(b));
    // SAFETY: our own socket.
    unsafe { shutdown(a, SD_SEND) };
    println!("poll peer half-closed: {}", polled(b));
    close(a);
    std::thread::sleep(Duration::from_millis(20));
    println!("poll peer closed: {}", polled(b));
    close(b);
    let (a, b) = unix_pair(&dir, PAIRS + 2)?;
    nonblocking(a)?;
    let chunk = [9u8; 4096];
    let mut filled = 0usize;
    loop {
        // SAFETY: 4096 bytes of `chunk`.
        let n = unsafe { send(a, chunk.as_ptr(), chunk.len() as i32, 0) };
        if n <= 0 {
            break;
        }
        filled += n as usize;
        if filled > 256 << 20 {
            break;
        }
    }
    // SAFETY: no arguments.
    println!("poll write: send buffer full after {filled} bytes (last error {}): {}", unsafe { WSAGetLastError() }, polled(a));
    let mut sink = vec![0u8; 1 << 20];
    let mut drained = 0;
    while drained < filled {
        let n = recv_some(b, &mut sink)?;
        if n == 0 {
            break;
        }
        drained += n;
    }
    std::thread::sleep(Duration::from_millis(20));
    println!("poll write drained: {}", polled(a));
    close(a);
    close(b);
    // Scatter-gather.
    let (a, b) = unix_pair(&dir, PAIRS + 3)?;
    let parts: [&[u8]; 3] = [b"first,", b"second,", b"third"];
    let bufs: Vec<WSABUF> = parts
        .iter()
        .map(|p| WSABUF {
            len: p.len() as u32,
            buf: p.as_ptr().cast_mut(),
        })
        .collect();
    let mut sent = 0u32;
    // SAFETY: three buffers, synchronous.
    let r = unsafe { WSASend(a, bufs.as_ptr(), 3, &raw mut sent, 0, std::ptr::null_mut(), None) };
    let (mut x, mut y, mut z) = ([0u8; 6], [0u8; 7], [0u8; 5]);
    let rbufs = [
        WSABUF { len: 6, buf: x.as_mut_ptr() },
        WSABUF { len: 7, buf: y.as_mut_ptr() },
        WSABUF { len: 5, buf: z.as_mut_ptr() },
    ];
    let (mut got, mut flags) = (0u32, 0u32);
    std::thread::sleep(Duration::from_millis(20));
    // SAFETY: three buffers, synchronous.
    let r2 = unsafe { WSARecv(b, rbufs.as_ptr(), 3, &raw mut got, &raw mut flags, std::ptr::null_mut(), None) };
    println!(
        "scatter-gather: send={r} sent={sent} recv={r2} got={got} parts={:?} {:?} {:?}",
        String::from_utf8_lossy(&x),
        String::from_utf8_lossy(&y),
        String::from_utf8_lossy(&z)
    );
    close(a);
    close(b);
    event_select(&dir)?;
    exits(&dir)?;
    // Inheritance: the child writes through an inherited socket.
    let (a, b) = unix_pair(&dir, PAIRS + 4)?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let status = std::process::Command::new(exe)
        .arg("child")
        .arg((a as usize).to_string())
        .status()
        .map_err(|e| e.to_string())?;
    close(a);
    let n = recv_some(b, &mut buf)?;
    println!(
        "inherit: child exited {status}; parent read {n} bytes {:?}",
        String::from_utf8_lossy(&buf[..n])
    );
    close(b);
    // Throughput and latency, AF_UNIX beside loopback TCP.
    throughput("unix", unix_pair(&dir, PAIRS + 5)?)?;
    throughput("tcp", tcp_pair()?)?;
    latency("unix", unix_pair(&dir, PAIRS + 6)?)?;
    latency("tcp", tcp_pair()?)?;
    Ok(())
}

/// A listener at a path of its own in `dir`, with a backlog of `backlog`.
fn listener_at(dir: &std::path::Path, name: &str, backlog: i32) -> Result<(SOCKET, String), String> {
    let path = dir.join(format!("m205-{}-{name}.sock", std::process::id()));
    let path_s = path.to_string_lossy().to_string();
    let addr = unix_addr(&path_s);
    let l = socket(AF_UNIX)?;
    // SAFETY: an address of its own length; a bound socket.
    unsafe {
        if bind(l, (&raw const addr).cast(), std::mem::size_of::<SOCKADDR_UN>() as i32) != 0 {
            let e = error(&format!("bind {path_s}"));
            close(l);
            return Err(e);
        }
        if listen(l, backlog) != 0 {
            close(l);
            return Err(error("listen"));
        }
    }
    Ok((l, path_s))
}

/// A connect to `path` without waiting: its socket, and what connect returned (0, or the
/// WSA error).
fn connect_nowait(path: &str) -> Result<(SOCKET, i32), String> {
    let c = socket(AF_UNIX)?;
    nonblocking(c)?;
    let addr = unix_addr(path);
    // SAFETY: an address of its own length.
    let r = unsafe { connect(c, (&raw const addr).cast(), std::mem::size_of::<SOCKADDR_UN>() as i32) };
    // SAFETY: no arguments.
    Ok((c, if r == 0 { 0 } else { unsafe { WSAGetLastError() } }))
}

/// How long until WSAPoll says `s` may be written (its connect done), or errs, up to `ms`.
fn until_writable(s: SOCKET, ms: i32) -> (Duration, String) {
    let t = Instant::now();
    let mut fd = WSAPOLLFD {
        fd: s,
        events: POLLWRNORM,
        revents: 0,
    };
    // SAFETY: one descriptor.
    let n = unsafe { WSAPoll(&raw mut fd, 1, ms) };
    (t.elapsed(), format!("n={n} revents={:#x}", fd.revents))
}

/// A nonblocking AF_UNIX connect (PM M205): what it returns where a listener has room, and
/// how long its socket takes to be writable; where nothing listens; where the backlog is
/// full, nonblocking and blocking, and whether a pending one completes once the listener
/// takes one.
fn connects(dir: &std::path::Path) -> Result<(), String> {
    let (l, path) = listener_at(dir, "room", 512)?;
    let mut codes: std::collections::BTreeMap<i32, usize> = std::collections::BTreeMap::new();
    let mut writable = Vec::new();
    let mut polls = std::collections::BTreeMap::new();
    for _ in 0..1000 {
        let (c, code) = connect_nowait(&path)?;
        *codes.entry(code).or_default() += 1;
        let (took, polled) = until_writable(c, 1000);
        writable.push(took);
        *polls.entry(polled).or_insert(0usize) += 1;
        // SAFETY: a listening socket; no address asked.
        let a = unsafe { accept(l, std::ptr::null_mut(), std::ptr::null_mut()) };
        if a != INVALID_SOCKET {
            close(a);
        }
        close(c);
    }
    println!("M205 a listener with room: connect returned {codes:?} (0 or the WSA error); writable after {}; polls {polls:?}", summary(&mut writable));
    close(l);
    let (gone, gone_path) = listener_at(dir, "gone", 1)?;
    close(gone);
    let (c, code) = connect_nowait(&gone_path)?;
    let (took, polled) = until_writable(c, 1000);
    println!("M205 nothing listening (the path left): connect returned {code}; {polled} after {took:?}");
    close(c);
    // A full backlog: connects made and never accepted until one pends or is refused.
    let (full, full_path) = listener_at(dir, "full", 1)?;
    let mut held = Vec::new();
    let mut seen = Vec::new();
    for i in 0..64 {
        let (c, code) = connect_nowait(&full_path)?;
        let (took, polled) = until_writable(c, 200);
        seen.push(format!("#{i}: {code}, {polled} in {} us", took.as_micros()));
        held.push(c);
        if code != 0 && polled.starts_with("n=0") {
            break;
        }
    }
    println!("M205 a backlog of 1, none accepted: {}", seen.join("; "));
    // The last, pending or refused: does it complete once the listener takes one?
    // SAFETY: a listening socket; no address asked.
    let a = unsafe { accept(full, std::ptr::null_mut(), std::ptr::null_mut()) };
    if let Some(&last) = held.last() {
        let (took, polled) = until_writable(last, 1000);
        println!("M205 after one accept: the last connect's socket {polled} after {took:?}");
    }
    if a != INVALID_SOCKET {
        close(a);
    }
    for c in held {
        close(c);
    }
    // A blocking connect to a full backlog: does it wait, until an accept makes room?
    let (b, b_path) = listener_at(dir, "blocking", 1)?;
    let mut fill = Vec::new();
    for _ in 0..8 {
        let (c, _) = connect_nowait(&b_path)?;
        fill.push(c);
    }
    let started = Instant::now();
    let accepter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        // SAFETY: a listening socket; no address asked.
        let a = unsafe { accept(b, std::ptr::null_mut(), std::ptr::null_mut()) };
        (a, b)
    });
    let blocking = socket(AF_UNIX)?;
    let addr = unix_addr(&b_path);
    // SAFETY: an address of its own length; a blocking socket.
    let r = unsafe { connect(blocking, (&raw const addr).cast(), std::mem::size_of::<SOCKADDR_UN>() as i32) };
    // SAFETY: no arguments.
    let code = if r == 0 { 0 } else { unsafe { WSAGetLastError() } };
    println!(
        "M205 a blocking connect to a full backlog: returned {code} after {:?} (an accept came at 500 ms)",
        started.elapsed()
    );
    if let Ok((a, b)) = accepter.join() {
        if a != INVALID_SOCKET {
            close(a);
        }
        close(b);
    }
    close(blocking);
    for c in fill {
        close(c);
    }
    Ok(())
}

/// An AF_UNIX or loopback TCP socket made with `flags` (WSA_FLAG_OVERLAPPED or none).
fn socket_with(family: u16, flags: u32) -> Result<SOCKET, String> {
    // SAFETY: plain arguments.
    let s = unsafe { WSASocketW(i32::from(family), SOCK_STREAM, 0, std::ptr::null(), 0, flags) };
    if s == INVALID_SOCKET {
        return Err(error("WSASocketW"));
    }
    Ok(s)
}

/// A connected pair of `family`, its sockets made with `flags`; an accepted socket takes
/// its listener's.
fn pair_with(dir: &std::path::Path, n: usize, family: u16, flags: u32) -> Result<(SOCKET, SOCKET), String> {
    let l = socket_with(family, flags)?;
    let c = socket_with(family, flags)?;
    let path = dir.join(format!("m207-{}-{n}.sock", std::process::id()));
    let unix = unix_addr(&path.to_string_lossy());
    let mut tcp = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: 0,
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes([127, 0, 0, 1]),
            },
        },
        sin_zero: [0; 8],
    };
    let (addr, mut len): (*mut SOCKADDR, i32) = if family == AF_UNIX {
        ((&raw const unix).cast_mut().cast(), std::mem::size_of::<SOCKADDR_UN>() as i32)
    } else {
        ((&raw mut tcp).cast(), std::mem::size_of::<SOCKADDR_IN>() as i32)
    };
    // SAFETY: an address of its own length, which getsockname fills back for TCP's port.
    let a = unsafe {
        if bind(l, addr, len) != 0 || listen(l, 1) != 0 || getsockname(l, addr, &raw mut len) != 0 {
            let e = error("listener");
            close(l);
            close(c);
            return Err(e);
        }
        if connect(c, addr, len) != 0 {
            let e = error("connect");
            close(l);
            close(c);
            return Err(e);
        }
        accept(l, std::ptr::null_mut(), std::ptr::null_mut())
    };
    close(l);
    let _ = std::fs::remove_file(&path);
    if a == INVALID_SOCKET {
        close(c);
        return Err(error("accept"));
    }
    Ok((c, a))
}

/// `s` again, in this process, as WSADuplicateSocketW and WSASocketW(FROM_PROTOCOL_INFO)
/// make a socket's copy, with `flags`.
fn duplicate(s: SOCKET, flags: u32) -> Result<SOCKET, String> {
    let mut info = WSAPROTOCOL_INFOW::default();
    // SAFETY: our socket; a structure for the call to fill.
    if unsafe { WSADuplicateSocketW(s, std::process::id(), &raw mut info) } != 0 {
        return Err(error("WSADuplicateSocketW"));
    }
    // SAFETY: the info just written, as Winsock's documentation has a process use it.
    let d = unsafe {
        WSASocketW(
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            FROM_PROTOCOL_INFO,
            &raw const info,
            0,
            flags,
        )
    };
    if d == INVALID_SOCKET {
        return Err(error("WSASocketW(FROM_PROTOCOL_INFO)"));
    }
    Ok(d)
}

/// How long the wait one thread makes on a connection lasts (a WSAPoll's, or a recv's
/// SO_RCVTIMEO); when the peer sends the byte that ends any wait still waiting; and the
/// longest the peer waits for what this thread sent.
const WAITER_MS: u32 = 3000;
const RELEASE_MS: u64 = 4000;
const PEER_MS: i32 = 6000;

/// M207: one thread waits to read a connection (a WSAPoll of `WAITER_MS` for its data, a
/// blocking recv, or one of SO_RCVTIMEO `WAITER_MS`), on the socket or its copy; 100 ms on,
/// this thread sends a byte through the socket or its copy, and the peer waits for it;
/// at `RELEASE_MS` another thread of the peer's sends the byte that ends a wait still
/// waiting. Sockets made without WSA_FLAG_OVERLAPPED and with it. Each: how long the send
/// took, when the peer had the byte, and when and how the waiter returned.
fn concurrent(dir: &std::path::Path) -> Result<(), String> {
    let mut n = 0;
    for (transport, family) in [("unix", AF_UNIX), ("tcp", AF_INET)] {
        for (made, flags) in [("synchronous", 0), ("overlapped", WSA_FLAG_OVERLAPPED)] {
            for wait in ["poll", "recv", "recv-timeout"] {
                for (wait_on, send_by) in [("socket", "socket"), ("socket", "copy"), ("copy", "socket")] {
                    n += 1;
                    let (a, b) = pair_with(dir, n, family, flags)?;
                    let copy = duplicate(a, flags)?;
                    let waiter = if wait_on == "copy" { copy } else { a };
                    let sender = if send_by == "copy" { copy } else { a };
                    let (sent, took, seen, polled, waited) = std::thread::scope(|scope| {
                        let w = scope.spawn(move || {
                            let t = Instant::now();
                            let what = if wait == "poll" {
                                let mut fd = WSAPOLLFD {
                                    fd: waiter,
                                    events: POLLRDNORM,
                                    revents: 0,
                                };
                                // SAFETY: one entry, our socket.
                                let r = unsafe { WSAPoll(&raw mut fd, 1, WAITER_MS as i32) };
                                format!("poll {r} revents {:#x}", fd.revents)
                            } else {
                                if wait == "recv-timeout" {
                                    let ms = WAITER_MS;
                                    // SAFETY: an option of its own size, on our socket.
                                    unsafe { setsockopt(waiter, SOL_SOCKET, SO_RCVTIMEO, (&raw const ms).cast(), 4) };
                                }
                                let mut byte = [0u8; 1];
                                format!("recv {:?}", recv_some(waiter, &mut byte))
                            };
                            (t.elapsed(), what)
                        });
                        scope.spawn(move || {
                            std::thread::sleep(Duration::from_millis(RELEASE_MS));
                            let _ = send_all(b, b"y");
                        });
                        std::thread::sleep(Duration::from_millis(100));
                        let t = Instant::now();
                        let sent = send_all(sender, b"x");
                        let took = t.elapsed();
                        let mut fd = WSAPOLLFD {
                            fd: b,
                            events: POLLRDNORM,
                            revents: 0,
                        };
                        // SAFETY: one entry, our socket.
                        let polled = unsafe { WSAPoll(&raw mut fd, 1, PEER_MS) };
                        let seen = t.elapsed();
                        let waited = w.join().unwrap_or((Duration::ZERO, "panicked".into()));
                        (sent, took, seen, polled, waited)
                    });
                    println!(
                        "m207 {transport} {made} {wait} on the {wait_on}, send by the {send_by}: send {sent:?} \
                         in {:.1} ms; the peer had it after {:.1} ms (poll {polled}); the waiter's {} after \
                         {:.1} ms",
                        took.as_secs_f64() * 1e3,
                        seen.as_secs_f64() * 1e3,
                        waited.1,
                        waited.0.as_secs_f64() * 1e3
                    );
                    close(copy);
                    close(a);
                    close(b);
                }
            }
        }
    }
    Ok(())
}

/// What M358's peer is sent before the close: a command's output and its exit status.
const SENT: &[u8] = b"a command's output, then its exit status, sent before the close";

/// M358: what a peer reads of what a socket sent before it closed, where the closer had
/// left some of what the peer sent it unread, and where the peer writes after the close.
/// The closer is the connecting end, as the vsock device's host side is for a guest's
/// connection; the peer reads 50 ms after the case's last step, until end-of-stream or an
/// error. AF_UNIX and loopback TCP.
fn closes(dir: &std::path::Path) -> Result<(), String> {
    let cases = [
        "clean",
        "unread",
        "unread-not-shut",
        "drained",
        "written-after-the-close",
        "unread-after-shutdown",
        "partly-read",
        "shut-both-ways",
        "lingering",
    ];
    let mut n = 0;
    for (transport, family) in [("unix", AF_UNIX), ("tcp", AF_INET)] {
        for case in cases {
            n += 1;
            let (closer, peer) = pair_with(dir, n, family, 0)?;
            let timeout: u32 = 2000;
            // SAFETY: an option of its own size, on our socket.
            unsafe { setsockopt(peer, SOL_SOCKET, SO_RCVTIMEO, (&raw const timeout).cast(), 4) };
            let mut buf = [0u8; 4096];
            let mut read = 0;
            let mut late = String::new();
            if !matches!(case, "clean" | "written-after-the-close" | "unread-after-shutdown") {
                send_all(peer, b"stdin")?;
            }
            send_all(closer, SENT)?;
            match case {
                "drained" => {
                    nonblocking(closer)?;
                    let mut taken = 0;
                    while let Ok(k @ 1..) = recv_some(closer, &mut buf) {
                        taken += k;
                    }
                    late = format!(" (the closer took {taken} bytes first)");
                }
                "partly-read" => read += recv_some(peer, &mut buf[..10])?,
                "lingering" => {
                    let linger = LINGER {
                        l_onoff: 1,
                        l_linger: 5,
                    };
                    // SAFETY: an option of its own size, on our socket.
                    unsafe {
                        setsockopt(
                            closer,
                            SOL_SOCKET,
                            SO_LINGER,
                            (&raw const linger).cast(),
                            std::mem::size_of::<LINGER>() as i32,
                        )
                    };
                }
                _ => {}
            }
            match case {
                "unread-not-shut" => {}
                // SAFETY: our socket.
                "shut-both-ways" => unsafe {
                    shutdown(closer, SD_BOTH);
                },
                "unread-after-shutdown" => {
                    // SAFETY: our socket.
                    unsafe { shutdown(closer, SD_SEND) };
                    send_all(peer, b"stdin")?;
                }
                // SAFETY: our socket.
                _ => unsafe {
                    shutdown(closer, SD_SEND);
                },
            }
            close(closer);
            if case == "written-after-the-close" {
                std::thread::sleep(Duration::from_millis(50));
                late = format!(" (the peer's send after the close: {:?})", send_all(peer, b"stdin"));
            }
            std::thread::sleep(Duration::from_millis(50));
            let end = loop {
                match recv_some(peer, &mut buf) {
                    Ok(0) => break "end of stream".to_string(),
                    Ok(k) => read += k,
                    Err(e) => break e,
                }
            };
            println!(
                "m358 {transport} {case}: the peer read {read} of {} bytes, then {end}{late}",
                SENT.len()
            );
            close(peer);
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("m358") {
        let dir = std::env::temp_dir().join(format!("m358-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        if startup().is_ok()
            && let Err(e) = closes(&dir)
        {
            println!("M358 failed: {e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    if args.get(1).map(String::as_str) == Some("m207") {
        let dir = std::env::temp_dir().join(format!("m207-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        if startup().is_ok()
            && let Err(e) = concurrent(&dir)
        {
            println!("M207 failed: {e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    if args.get(1).map(String::as_str) == Some("m205") {
        let dir = std::env::temp_dir().join(format!("m205-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        if startup().is_ok()
            && let Err(e) = connects(&dir)
        {
            println!("M205 failed: {e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    if args.get(1).map(String::as_str) == Some("child-exit") {
        let _ = startup();
        let s = args.get(2).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) as SOCKET;
        let _ = send_all(s, b"written before the end");
        // SAFETY: the inherited socket, ended as the mode says.
        unsafe {
            match args.get(3).map(String::as_str) {
                Some("close") => {
                    closesocket(s);
                }
                Some("shutdown") => {
                    shutdown(s, SD_SEND);
                }
                Some("shutdown-close") => {
                    shutdown(s, SD_SEND);
                    closesocket(s);
                }
                _ => {}
            }
        }
        std::process::exit(0);
    }
    if args.get(1).map(String::as_str) == Some("child-late") {
        let _ = startup();
        std::thread::sleep(Duration::from_millis(300));
        let s = args.get(2).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) as SOCKET;
        let r = send_all(s, b"after the parent closed");
        std::thread::sleep(Duration::from_millis(300));
        std::process::exit(if r.is_ok() { 0 } else { 3 });
    }
    if args.get(1).map(String::as_str) == Some("child") {
        let _ = startup();
        let s = args.get(2).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) as SOCKET;
        let r = send_all(s, b"from the child");
        std::process::exit(if r.is_ok() { 0 } else { 3 });
    }
    if let Err(e) = probe() {
        println!("probe failed: {e}");
    }
}

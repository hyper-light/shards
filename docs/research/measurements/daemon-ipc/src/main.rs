//! M23 (docs/research/platform-measurements.md): the host-side cost of a warm-pool
//! daemon's handoff, and of the client process that asks for it. See run.sh.
//!
//! Roles, by the first argument:
//! - `daemon SOCKET MODE`: accepts clients at SOCKET and reads each one's request and the
//!   three fds sent with it. MODE `handoff` passes the client's connection and those fds to
//!   a pre-spawned `worker` over a socket pair, and the worker answers; `direct` answers
//!   the client itself.
//! - `worker FD`: answers each client handed over on FD, on the client's own connection.
//! - `client SOCKET`: connects, sends a request with its stdin, stdout and stderr, and waits
//!   for the answer.
//! - `noop`: exits at once.
//! - `bench N DIR [PROGRAM ARG...]`: runs both daemons with their sockets in DIR, then
//!   prints percentiles for N in-process requests and N client processes of each kind,
//!   interleaved, next to N runs of `noop` and of PROGRAM, if given.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const REQUEST: usize = 256;
const ANSWER: usize = 8;
const MAX_FDS: usize = 4;

/// Sends `data` with `fds` attached (SCM_RIGHTS).
fn send_with_fds(sock: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: data.as_ptr() as *mut libc::c_void,
        iov_len: data.len(),
    };
    let payload = std::mem::size_of_val(fds) as u32;
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(payload) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: an all-zero msghdr is valid; every pointer set below outlives the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer holds one header plus `payload` bytes.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(payload) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast::<RawFd>(), fds.len());
    }
    // SAFETY: a fully initialized msghdr.
    let n = unsafe { libc::sendmsg(sock, &msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n as usize != data.len() {
        return Err(io::Error::other("short sendmsg"));
    }
    Ok(())
}

/// Receives one message into `buf` and the fds attached to it.
fn recv_with_fds(sock: RawFd, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut control = vec![0u8; space];
    // SAFETY: as in send_with_fds.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: a fully initialized msghdr over live buffers.
    let n = unsafe { libc::recvmsg(sock, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: walking the control messages the kernel wrote into our buffer.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let bytes = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for i in 0..bytes / std::mem::size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(*data.add(i)));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    Ok((n as usize, fds))
}

fn read_request(conn: &UnixStream) -> io::Result<Vec<OwnedFd>> {
    let mut buf = [0u8; REQUEST];
    let (n, fds) = recv_with_fds(conn.as_raw_fd(), &mut buf)?;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    // A stream may deliver the rest of the request separately; the fds come with its first byte.
    let mut reader = conn;
    reader.read_exact(&mut buf[n..])?;
    Ok(fds)
}

fn daemon(socket: &Path, mode: &str) -> io::Result<()> {
    let listener = UnixListener::bind(socket)?;
    let worker = if mode == "handoff" {
        let mut pair = [0; 2];
        // SAFETY: socketpair(2) fills both descriptors; neither is close-on-exec yet.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let [ours, theirs] = pair;
        // SAFETY: fcntl on our own descriptor.
        unsafe { libc::fcntl(ours, libc::F_SETFD, libc::FD_CLOEXEC) };
        let child = Command::new(std::env::current_exe()?)
            .args(["worker", &theirs.to_string()])
            .spawn()?;
        // SAFETY: the child holds its copy; ours is closed once.
        unsafe { libc::close(theirs) };
        // SAFETY: a descriptor we own.
        Some((unsafe { UnixStream::from_raw_fd(ours) }, child))
    } else {
        None
    };
    for conn in listener.incoming() {
        // A connection that brings no request (the bench's readiness probe) is dropped.
        let Ok(conn) = conn else { continue };
        let Ok(fds) = read_request(&conn) else { continue };
        match &worker {
            Some((to_worker, _)) => {
                let mut raw = vec![conn.as_raw_fd()];
                raw.extend(fds.iter().map(AsRawFd::as_raw_fd));
                send_with_fds(to_worker.as_raw_fd(), &[0u8; REQUEST], &raw)?;
            }
            None => (&conn).write_all(&[0u8; ANSWER])?,
        }
    }
    Ok(())
}

fn worker(fd: RawFd) -> io::Result<()> {
    // SAFETY: the descriptor the daemon left open for us.
    let from_daemon = unsafe { UnixStream::from_raw_fd(fd) };
    loop {
        let mut buf = [0u8; REQUEST];
        let (n, mut fds) = recv_with_fds(from_daemon.as_raw_fd(), &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        (&from_daemon).read_exact(&mut buf[n..])?;
        if fds.is_empty() {
            return Err(io::Error::other("a handoff without the client's connection"));
        }
        let conn = UnixStream::from(fds.remove(0));
        (&conn).write_all(&[0u8; ANSWER])?;
    }
}

/// One request: connect, send the request with our stdio, read the answer.
fn client(socket: &Path) -> io::Result<()> {
    let conn = UnixStream::connect(socket)?;
    send_with_fds(conn.as_raw_fd(), &[1u8; REQUEST], &[0, 1, 2])?;
    let mut answer = [0u8; ANSWER];
    (&conn).read_exact(&mut answer)
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

fn row(name: &str, mut v: Vec<f64>) {
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[((p / 100.0 * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
    println!(
        "{name:<24} n={:<5} p50 {:>8.1} p90 {:>8.1} p99 {:>8.1} max {:>8.1} us",
        v.len(),
        at(50.0),
        at(90.0),
        at(99.0),
        v[v.len() - 1]
    );
}

fn wait_for(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(socket).is_err() {
        assert!(Instant::now() < deadline, "{} never listened", socket.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn spawn_wall(program: &str, args: &[&str]) -> f64 {
    let start = Instant::now();
    let status = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .status()
        .expect("spawning");
    assert!(status.success(), "{program} {args:?}: {status}");
    micros(start.elapsed())
}

fn bench(n: usize, dir: &Path, other: &[String]) -> io::Result<()> {
    let me = std::env::current_exe()?;
    let me = me.to_str().ok_or_else(|| io::Error::other("non-UTF-8 path"))?;
    let (direct, handoff) = (dir.join("direct.sock"), dir.join("handoff.sock"));
    let mut daemons: Vec<Child> = Vec::new();
    for (socket, mode) in [(&direct, "direct"), (&handoff, "handoff")] {
        daemons.push(
            Command::new(me)
                .args(["daemon", socket.to_str().unwrap_or_default(), mode])
                .spawn()?,
        );
        wait_for(socket);
    }
    let (mut in_direct, mut in_handoff) = (Vec::new(), Vec::new());
    for i in 0..n + 20 {
        for (socket, out) in [(&direct, &mut in_direct), (&handoff, &mut in_handoff)] {
            let start = Instant::now();
            client(socket)?;
            if i >= 20 {
                out.push(micros(start.elapsed()));
            }
        }
    }
    let (mut p_direct, mut p_handoff, mut p_noop, mut p_other) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let other_args: Vec<&str> = other.iter().skip(1).map(String::as_str).collect();
    for i in 0..n + 3 {
        let d = spawn_wall(me, &["client", direct.to_str().unwrap_or_default()]);
        let h = spawn_wall(me, &["client", handoff.to_str().unwrap_or_default()]);
        let z = spawn_wall(me, &["noop"]);
        let o = other.first().map(|p| spawn_wall(p, &other_args));
        if i >= 3 {
            p_direct.push(d);
            p_handoff.push(h);
            p_noop.push(z);
            p_other.extend(o);
        }
    }
    for d in &mut daemons {
        let _ = d.kill();
        let _ = d.wait();
    }
    row("request, direct", in_direct);
    row("request, handoff", in_handoff);
    row("client process, direct", p_direct);
    row("client process, handoff", p_handoff);
    row("noop process", p_noop);
    if let Some(program) = other.first() {
        row(&format!("{program} {}", other_args.join(" ")), p_other);
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("daemon") => daemon(Path::new(&args[2]), &args[3]),
        Some("worker") => worker(args[2].parse().map_err(io::Error::other)?),
        Some("client") => client(Path::new(&args[2])),
        Some("noop") => Ok(()),
        Some("bench") => bench(args[2].parse().map_err(io::Error::other)?, Path::new(&args[3]), &args[4..]),
        _ => Err(io::Error::other("usage: daemon-ipc daemon|worker|client|noop|bench ...")),
    }
}

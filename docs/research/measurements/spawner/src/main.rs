// What a child holds of what its parent lets go of while it is made, and what making the
// children in a process that holds nothing else costs (PM M158). A child holds every
// descriptor its parent had at the spawn until it execs (M134); the daemon binds runs'
// published ports, and holds its clients' descriptors, while its other threads spawn VMs.
//
// The main thread lets go of a listener (then binds its port again) or of a pipe's write
// end (then reads the pipe for its end), again and again, and times each that something
// else still holds, while THREADS threads have /usr/bin/true spawned as shards_ipc::spawn
// spawns (posix_spawn, three descriptors given at 3 to 5, POSIX_SPAWN_CLOEXEC_DEFAULT on
// macOS): here (`direct`), with FDS more descriptors open, as a daemon holds its VMs'; or
// by a spawner (`spawner`), forked before this process opened anything else, to which
// each thread sends the three descriptors and which answers once posix_spawn returns.
// Also timed: posix_spawn itself, here or there; and, for `spawner`, the round trip alone
// (a request whose three descriptors the spawner closes and answers at once), first.
//
//     cargo run --release -- direct|spawner THREADS SECONDS FDS
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const TRUE: &[u8] = b"/usr/bin/true\0";

/// /usr/bin/true, spawned with `fds` at 3, 4 and 5, as shards_ipc::spawn spawns: its pid,
/// and how long posix_spawn took.
fn spawn(fds: &[RawFd]) -> (libc::pid_t, Duration) {
    // SAFETY: attributes and actions initialized before use and destroyed once; every
    // pointer lives until posix_spawn returns.
    unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        libc::posix_spawn_file_actions_init(&mut actions);
        for (i, fd) in fds.iter().enumerate() {
            libc::posix_spawn_file_actions_adddup2(&mut actions, *fd, 3 + i as RawFd);
        }
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        libc::posix_spawnattr_init(&mut attr);
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::posix_spawnattr_setsigmask(&mut attr, &none);
        let mut all: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all);
        libc::posix_spawnattr_setsigdefault(&mut attr, &all);
        #[cfg(target_vendor = "apple")]
        let flags = libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF | libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
        #[cfg(not(target_vendor = "apple"))]
        let flags = libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF;
        libc::posix_spawnattr_setflags(&mut attr, flags as libc::c_short);
        let argv = [TRUE.as_ptr() as *mut libc::c_char, std::ptr::null_mut()];
        let envp: [*mut libc::c_char; 1] = [std::ptr::null_mut()];
        let mut pid = 0;
        let began = Instant::now();
        let rc = libc::posix_spawn(&mut pid, TRUE.as_ptr().cast(), &actions, &attr, argv.as_ptr(), envp.as_ptr());
        let took = began.elapsed();
        libc::posix_spawnattr_destroy(&mut attr);
        libc::posix_spawn_file_actions_destroy(&mut actions);
        assert_eq!(rc, 0, "posix_spawn: {}", std::io::Error::from_raw_os_error(rc));
        (pid, took)
    }
}

fn reap(pid: libc::pid_t) {
    let mut status = 0;
    // SAFETY: waits for our own child into a local.
    unsafe { libc::waitpid(pid, &mut status, 0) };
}

/// Sends `byte` with `fds`.
fn send_fds(sock: RawFd, byte: u8, fds: &[RawFd]) {
    // SAFETY: the message's buffers are locals that outlive sendmsg; the control buffer is
    // CMSG_SPACE for the descriptors written into it.
    unsafe {
        let mut data = [byte];
        let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: 1 };
        let bytes = std::mem::size_of_val(fds) as u32;
        let mut control = vec![0u8; libc::CMSG_SPACE(bytes) as usize];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len() as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(bytes) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast(), fds.len());
        assert_eq!(libc::sendmsg(sock, &msg, 0), 1, "sendmsg: {}", std::io::Error::last_os_error());
    }
}

/// A byte and the descriptors that came with it; `None` at the connection's end.
fn recv_fds(sock: RawFd) -> Option<(u8, Vec<OwnedFd>)> {
    // SAFETY: as send_fds; each descriptor received is owned here from then on.
    unsafe {
        let mut data = [0u8];
        let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: 1 };
        let mut control = vec![0u8; libc::CMSG_SPACE(8 * size_of::<RawFd>() as u32) as usize];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = control.len() as _;
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        if libc::recvmsg(sock, &mut msg, flags) <= 0 {
            return None;
        }
        let mut fds = Vec::new();
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let n = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / size_of::<RawFd>();
                let at = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..n {
                    let fd = std::ptr::read_unaligned(at.add(i));
                    #[cfg(not(target_os = "linux"))]
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        Some((data[0], fds))
    }
}

/// The spawner: a thread for each connection, until each ends. `p` is answered at once;
/// anything else is a spawn, answered with how long posix_spawn took once it returns.
fn spawner(conns: Vec<UnixStream>) -> ! {
    std::thread::scope(|s| {
        for conn in &conns {
            s.spawn(move || {
                while let Some((byte, fds)) = recv_fds(conn.as_raw_fd()) {
                    let mut reply = [0u8; 8];
                    if byte == b'p' {
                        drop(fds);
                        let _ = (&*conn).write_all(&reply);
                        continue;
                    }
                    let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
                    let (pid, took) = spawn(&raw);
                    drop(fds);
                    reply.copy_from_slice(&(took.as_nanos() as u64).to_le_bytes());
                    let _ = (&*conn).write_all(&reply);
                    reap(pid);
                }
            });
        }
    });
    // SAFETY: ends the forked spawner without running the parent's exit handlers.
    unsafe { libc::_exit(0) }
}

fn null() -> OwnedFd {
    std::fs::File::open("/dev/null").unwrap().into()
}

/// A listener at `port` let go of, and the port bound again: `None` if the first bind
/// failed (a hold of the last round's), else how long the port stayed bound, if it did.
fn listener_round(port: u16) -> Option<Option<Duration>> {
    let first = std::net::TcpListener::bind(("0.0.0.0", port)).ok()?;
    drop(first);
    if std::net::TcpListener::bind(("0.0.0.0", port)).is_ok() {
        return Some(None);
    }
    let began = Instant::now();
    while std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {}
    Some(Some(began.elapsed()))
}

/// A pipe's write end let go of: how long its read end waited for the pipe's end, if it
/// did.
fn pipe_round() -> Option<Duration> {
    let (mut reader, writer) = std::io::pipe().unwrap();
    // SAFETY: fcntl(2) on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(reader.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    drop(writer);
    let mut byte = [0u8];
    match reader.read(&mut byte) {
        Ok(_) => None,
        Err(e) if e.kind() == ErrorKind::WouldBlock => {
            let began = Instant::now();
            while matches!(reader.read(&mut byte), Err(ref e) if e.kind() == ErrorKind::WouldBlock) {}
            Some(began.elapsed())
        }
        Err(e) => panic!("reading the pipe: {e}"),
    }
}

/// n / p50 / p90 / p99 / max of `v`, in `unit` (`us` or `ms`).
fn stats(mut v: Vec<Duration>, unit: &str) -> String {
    if v.is_empty() {
        return "n 0".into();
    }
    v.sort();
    let scale = if unit == "ms" { 1e3 } else { 1e6 };
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize].as_secs_f64() * scale;
    format!(
        "n {} p50 {:.2} p90 {:.2} p99 {:.2} max {:.2} {unit}",
        v.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        at(1.0)
    )
}

fn main() {
    let arg = |i: usize| std::env::args().nth(i);
    let mode = arg(1).unwrap_or_else(|| "direct".into());
    let threads: usize = arg(2).and_then(|v| v.parse().ok()).unwrap_or(4);
    let secs: u64 = arg(3).and_then(|v| v.parse().ok()).unwrap_or(10);
    let extra: usize = arg(4).and_then(|v| v.parse().ok()).unwrap_or(0);
    let via_spawner = match mode.as_str() {
        "direct" => false,
        "spawner" => true,
        other => panic!("unknown mode {other:?}: direct or spawner"),
    };
    // The spawner first, while this process holds nothing but its connections to it.
    let mut conns = Vec::new();
    if via_spawner {
        let (ours, theirs): (Vec<_>, Vec<_>) = (0..threads).map(|_| UnixStream::pair().unwrap()).unzip();
        // SAFETY: fork(2) while this process has one thread; the child runs only the
        // spawner, which ends with _exit.
        match unsafe { libc::fork() } {
            0 => {
                drop(ours);
                spawner(theirs)
            }
            -1 => panic!("fork: {}", std::io::Error::last_os_error()),
            _ => conns = ours,
        }
    }
    let held: Vec<OwnedFd> = (0..extra).map(|_| null()).collect();
    let mut pings = Vec::new();
    for conn in &conns {
        let given = [null(), null(), null()];
        let raw = given.each_ref().map(|f| f.as_raw_fd());
        for _ in 0..2_000 {
            let began = Instant::now();
            send_fds(conn.as_raw_fd(), b'p', &raw);
            let mut reply = [0u8; 8];
            (&*conn).read_exact(&mut reply).unwrap();
            pings.push(began.elapsed());
        }
    }
    let stop = AtomicBool::new(false);
    let spawns = Mutex::new(Vec::<Duration>::new());
    let (mut listener_holds, mut pipe_holds) = (Vec::new(), Vec::new());
    let (mut listener_drops, mut pipe_drops) = (0u64, 0u64);
    std::thread::scope(|s| {
        for t in 0..threads {
            let (stop, spawns, conn) = (&stop, &spawns, conns.get(t));
            s.spawn(move || {
                let given = [null(), null(), null()];
                let raw = given.each_ref().map(|f| f.as_raw_fd());
                let mut took = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    match conn {
                        None => {
                            let (pid, t) = spawn(&raw);
                            took.push(t);
                            reap(pid);
                        }
                        Some(conn) => {
                            send_fds(conn.as_raw_fd(), b's', &raw);
                            let mut reply = [0u8; 8];
                            (&*conn).read_exact(&mut reply).unwrap();
                            took.push(Duration::from_nanos(u64::from_le_bytes(reply)));
                        }
                    }
                }
                spawns.lock().unwrap().extend(took);
            });
        }
        let port = std::net::TcpListener::bind("0.0.0.0:0").unwrap().local_addr().unwrap().port();
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if let Some(hold) = listener_round(port) {
                listener_drops += 1;
                listener_holds.extend(hold);
            }
            pipe_drops += 1;
            pipe_holds.extend(pipe_round());
        }
        stop.store(true, Ordering::Relaxed);
    });
    drop(held);
    let mut load = [0f64; 3];
    // SAFETY: getloadavg(3) into a local array of three.
    unsafe { libc::getloadavg(load.as_mut_ptr(), 3) };
    println!("{mode}, {threads} spawning threads, {extra} more descriptors, {secs} s, load {load:.1?}");
    if via_spawner {
        println!("  round trip alone: {}", stats(pings, "us"));
    }
    println!("  posix_spawn: {}", stats(spawns.into_inner().unwrap(), "us"));
    println!(
        "  listener: {listener_drops} let go, {} held: {}",
        listener_holds.len(),
        stats(listener_holds, "ms")
    );
    println!("  pipe: {pipe_drops} let go, {} held: {}", pipe_holds.len(), stats(pipe_holds, "ms"));
}

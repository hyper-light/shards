//! The test guest run as a workload, not as PID 1: `shards-testguest <mode> [args]`, run
//! inside an image by shards-init (crates/shards/tests/run.rs).

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicI32, Ordering};

use shards_testguest::fill;

/// SIGPIPE's disposition as the workload started: 0 default, 1 ignored, 2 handled. Rust's
/// runtime ignores SIGPIPE before main, so a constructor records it first.
static SIGPIPE_AT_START: AtomicI32 = AtomicI32::new(-1);

extern "C" fn record_sigpipe() {
    // SAFETY: sigaction(2) only reads the disposition.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut old) == 0 {
            let v = match old.sa_sigaction {
                libc::SIG_DFL => 0,
                libc::SIG_IGN => 1,
                _ => 2,
            };
            SIGPIPE_AT_START.store(v, Ordering::Relaxed);
        }
    }
}

#[used]
#[unsafe(link_section = ".init_array")]
static RECORD_SIGPIPE: extern "C" fn() = record_sigpipe;

pub fn main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    let code = match arg(0) {
        "report" => report(),
        "cat" => cat(),
        "stat" => stat(args.get(1..).unwrap_or_default()),
        "mtime" => {
            use std::os::unix::fs::MetadataExt as _;
            for p in args.get(1..).unwrap_or_default() {
                let t = std::fs::symlink_metadata(p).map_or(-1, |m| m.mtime());
                let _ = writeln!(io::stdout(), "{p} {t}");
            }
            0
        }
        "stderr" => {
            let _ = io::stderr().write_all(arg(1).as_bytes());
            0
        }
        "exit" => arg(1).parse().unwrap_or(1),
        // Touches N MiB, a page at a time, then says so: what a memory limit stops.
        "alloc" => {
            let mib: usize = arg(1).parse().unwrap_or(0);
            let mut held: Vec<Vec<u8>> = Vec::new();
            for _ in 0..mib {
                let mut chunk = vec![0u8; 1 << 20];
                for page in chunk.chunks_mut(4096) {
                    if let Some(b) = page.first_mut() {
                        *b = 1;
                    }
                }
                held.push(chunk);
            }
            let _ = writeln!(io::stdout(), "allocated {}", held.len());
            0
        }
        "kill" => {
            // SAFETY: raise(2) on ourselves.
            unsafe { libc::raise(libc::SIGKILL) };
            1
        }
        "bulk" => bulk(arg(1).parse().unwrap_or(0), arg(2).parse().unwrap_or(0)),
        "hash" => {
            let hash =
                shards_testguest::pattern_hash(arg(2).parse().unwrap_or(0), arg(1).parse().unwrap_or(0));
            let _ = writeln!(io::stdout(), "{hash:016x}");
            0
        }
        "clean-cache" => clean_cache(arg(1)),
        "orphan" => orphan(),
        "trap" => trap(arg(1)),
        "ignore" => ignore(arg(1)),
        "tty" => tty(arg(1)),
        "vsock" => vsock(args.get(1..).unwrap_or_default()),
        "loopback" => loopback(),
        "fs" => fs(args.get(1..).unwrap_or_default()),
        "tcp" => tcp(arg(1)),
        "ask" => ask(arg(1)),
        "udp" => udp(arg(1)),
        "serve" => serve(arg(1), arg(2).parse().unwrap_or(1)),
        "agent" => agent(),
        "vsock-agent" => vsock_agent(arg(1).parse().unwrap_or(1028)),
        "hold" => hold(arg(1)),
        "udp-echo" => udp_echo(arg(1), arg(2).parse().unwrap_or(1)),
        "spin" => {
            let _ = writeln!(io::stdout(), "ready");
            let mut n = 0u64;
            loop {
                n = std::hint::black_box(n.wrapping_add(1));
            }
        }
        "confined" => confined(args.get(1..).unwrap_or_default()),
        "await" => await_process(arg(1), arg(2).parse().unwrap_or(1), arg(3)),
        "sleep" => {
            let _ = writeln!(io::stdout(), "ready");
            loop {
                // SAFETY: blocks until a signal arrives.
                unsafe { libc::pause() };
            }
        }
        other => {
            let _ = writeln!(io::stderr(), "unknown mode {other:?}");
            2
        }
    };
    std::process::exit(code)
}

/// Prints each path as lstat(2) sees it: `PATH TYPE MODE UID:GID SIZE`, then `= TEXT`
/// for a file, its bytes with newlines as `\n`, or `-> TARGET` for a symlink.
fn stat(paths: &[String]) -> i32 {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let mut out = String::new();
    let mut code = 0;
    for p in paths {
        let m = match std::fs::symlink_metadata(p) {
            Ok(m) => m,
            Err(e) => {
                out.push_str(&format!("{p} missing {e}\n"));
                code = 1;
                continue;
            }
        };
        let t = m.file_type();
        let kind = if t.is_dir() {
            "dir"
        } else if t.is_symlink() {
            "symlink"
        } else if t.is_file() {
            "file"
        } else if t.is_fifo() {
            "fifo"
        } else {
            "other"
        };
        out.push_str(&format!(
            "{p} {kind} {:o} {}:{} {}\n",
            m.mode() & 0o7777,
            m.uid(),
            m.gid(),
            m.size()
        ));
        if t.is_file() {
            match std::fs::read(p) {
                Ok(b) => out.push_str(&format!(
                    "= {}\n",
                    String::from_utf8_lossy(&b).replace('\n', "\\n")
                )),
                Err(e) => out.push_str(&format!("= unreadable {e}\n")),
            }
        } else if t.is_symlink()
            && let Ok(target) = std::fs::read_link(p)
        {
            out.push_str(&format!("-> {}\n", target.display()));
        }
    }
    let _ = io::stdout().write_all(out.as_bytes());
    code
}

/// Cleans the data cache over every page of `path`, mapped read-only and never read, as a
/// JIT or a kernel making a page executable does: `DC CVAU` on arm64, `CLFLUSH` on x86_64.
/// From an image served with DAX, each page is the image's own, in memory the host maps.
/// Prints how many pages it cleaned.
fn clean_cache(path: &str) -> i32 {
    use std::os::fd::AsRawFd as _;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            let _ = writeln!(io::stderr(), "{path}: {e}");
            return 1;
        }
    };
    let len = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    // SAFETY: sysconf(3) has no preconditions.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096);
    if len == 0 {
        let _ = writeln!(io::stderr(), "{path}: empty");
        return 1;
    }
    // SAFETY: a read-only shared mapping of a file held open, unmapped below.
    let at = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if at == libc::MAP_FAILED {
        let _ = writeln!(io::stderr(), "mmap: {}", io::Error::last_os_error());
        return 1;
    }
    let mut cleaned = 0;
    for offset in (0..len).step_by(page) {
        let line = at.cast::<u8>().wrapping_add(offset);
        // SAFETY: an address in the mapping; cleaning the cache reads and writes no data.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!("dc cvau, {0}", in(reg) line, options(nostack, preserves_flags));
        }
        // SAFETY: as above.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::asm!("clflush [{0}]", in(reg) line, options(nostack, preserves_flags));
        }
        cleaned += 1;
    }
    // SAFETY: as above; the barrier completes the maintenance.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
    // SAFETY: the mapping made above.
    unsafe { libc::munmap(at, len) };
    let _ = writeln!(io::stdout(), "cleaned {cleaned}");
    0
}

/// Prints who and where the workload is, one `key value` per line, and proves the root
/// filesystem is writable.
fn report() -> i32 {
    let mut out = String::new();
    // SAFETY: plain getters.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let mut groups = [0 as libc::gid_t; 64];
    // SAFETY: getgroups(2) writes at most 64 entries.
    let n = unsafe { libc::getgroups(64, groups.as_mut_ptr()) };
    let groups: Vec<String> = groups
        .iter()
        .take(usize::try_from(n).unwrap_or(0))
        .map(u32::to_string)
        .collect();
    out.push_str(&format!("uid {uid}\ngid {gid}\ngroups {}\n", groups.join(",")));
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    out.push_str(&format!("cwd {cwd}\n"));
    // SAFETY: an all-zero utsname is a valid out-parameter.
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::uname(&mut uts) };
    // SAFETY: uname(2) NUL-terminates nodename.
    let host = unsafe { std::ffi::CStr::from_ptr(uts.nodename.as_ptr()) }.to_string_lossy();
    out.push_str(&format!("hostname {host}\n"));
    out.push_str(&format!("pid {}\n", std::process::id()));
    // What its stdin is: Docker gives /dev/null without `-i` or a terminal.
    let stdin = std::fs::read_link("/proc/self/fd/0")
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let stdin = if stdin == "/dev/null" {
        "null"
    } else if stdin.starts_with("pipe:") {
        "pipe"
    } else if stdin.starts_with("/dev/pts/") {
        "tty"
    } else {
        "other"
    };
    out.push_str(&format!("stdin {stdin}\n"));
    // The kernel's own account of the process: umask, capabilities, seccomp.
    for line in std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
    {
        if let Some((k, v)) = line.split_once(':')
            && matches!(
                k,
                "Umask" | "CapInh" | "CapPrm" | "CapEff" | "CapBnd" | "CapAmb" | "NoNewPrivs" | "Seccomp"
            )
        {
            out.push_str(&format!("{} {}\n", k.to_lowercase(), v.trim()));
        }
    }
    // Its resource limits, as `ulimit` and `--ulimit` name them: soft and hard, -1 for
    // none.
    for (name, resource) in [
        ("nofile", libc::RLIMIT_NOFILE),
        ("core", libc::RLIMIT_CORE),
        ("as", libc::RLIMIT_AS),
        ("rttime", libc::RLIMIT_RTTIME),
    ] {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit(2) writing into a limit of ours.
        if unsafe { libc::getrlimit(resource, &mut limit) } == 0 {
            let shown = |v: libc::rlim_t| if v == libc::RLIM_INFINITY { -1 } else { v as i64 };
            out.push_str(&format!(
                "rlimit-{name} {}:{}\n",
                shown(limit.rlim_cur),
                shown(limit.rlim_max)
            ));
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    out.push_str(&format!("realtime {now}\n"));
    for (k, v) in std::env::vars() {
        out.push_str(&format!("env {k}={v}\n"));
    }
    let sigpipe = match SIGPIPE_AT_START.load(Ordering::Relaxed) {
        0 => "default",
        1 => "ignored",
        _ => "other",
    };
    out.push_str(&format!("sigpipe {sigpipe}\n"));
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let blocked = status
        .lines()
        .find_map(|l| l.strip_prefix("SigBlk:"))
        .unwrap_or_default();
    out.push_str(&format!("sigblk {}\n", blocked.trim()));
    let existed = std::path::Path::new("/written").exists();
    out.push_str(&format!("existed {existed}\n"));
    let written =
        std::fs::write("/written", b"x").is_ok() && std::fs::read("/written").is_ok_and(|d| d == b"x");
    out.push_str(&format!("writable {written}\n"));
    // What a container's /dev holds, and its paths read-only and masked.
    let mut devs: Vec<String> = std::fs::read_dir("/dev")
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    devs.sort();
    out.push_str(&format!("devs {}\n", devs.join(",")));
    let sysctl = std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/sys/kernel/domainname")
        .is_ok();
    out.push_str(&format!("sysctl_writable {sysctl}\n"));
    let kcore = {
        use std::os::unix::fs::FileTypeExt as _;
        std::fs::metadata("/proc/kcore").is_ok_and(|m| m.file_type().is_char_device())
    };
    out.push_str(&format!("kcore_masked {kcore}\n"));
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    for target in [
        "/",
        "/proc",
        "/sys",
        "/dev",
        "/dev/pts",
        "/dev/shm",
        "/dev/mqueue",
    ] {
        let fstype = mounts
            .lines()
            .filter_map(|l| {
                let mut f = l.split(' ');
                let (_, t, fs) = (f.next(), f.next(), f.next());
                (t == Some(target)).then_some(fs.unwrap_or_default())
            })
            .next_back()
            .unwrap_or("none");
        out.push_str(&format!("mount {target} {fstype}\n"));
    }
    let _ = io::stdout().write_all(out.as_bytes());
    0
}

/// Connects to `addr` (HOST:PORT, a name resolved as the C library resolves it), says
/// nothing, its side closed, and prints what comes back until the far end closes: `ask
/// BYTES`, or `ask error E`.
fn ask(addr: &str) -> i32 {
    use std::io::Read as _;
    let mut got = Vec::new();
    let asked = std::net::TcpStream::connect(addr).and_then(|mut s| {
        s.shutdown(std::net::Shutdown::Write)?;
        s.read_to_end(&mut got)
    });
    match asked {
        Ok(_) => {
            let _ = writeln!(io::stdout(), "ask {}", String::from_utf8_lossy(&got).trim_end());
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "ask error {e}");
            1
        }
    }
}

/// Connects to `addr` (IP:PORT), reads until the far end closes, and prints what came:
/// `tcp N BYTES` then the bytes.
fn tcp(addr: &str) -> i32 {
    use std::io::Read as _;
    let mut got = Vec::new();
    match std::net::TcpStream::connect(addr).and_then(|mut s| s.read_to_end(&mut got)) {
        Ok(n) => {
            let _ = writeln!(
                io::stdout(),
                "tcp {n} {}",
                String::from_utf8_lossy(&got).trim_end()
            );
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "tcp error {e}");
            1
        }
    }
}

/// Sends a datagram to `addr` from a connected socket, and waits up to 5 s for an answer:
/// `udp N` its length, or `udp error E`, as an ICMP error or the wait ends it.
fn udp(addr: &str) -> i32 {
    let answered = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
        s.connect(addr)?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        s.send(b"?")?;
        let mut buf = [0u8; 512];
        s.recv(&mut buf)
    });
    match answered {
        Ok(n) => {
            let _ = writeln!(io::stdout(), "udp {n}");
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "udp error {e}");
            1
        }
    }
}

/// Listens on TCP `port` at every address, says `ready`, then serves `connections`
/// connections in turn: to each, `from IP\n` (its peer's address), then all it sends
/// back, until it closes its side.
/// Dials the host's vsock `port` as a builder's agent relay does, with a token it was never
/// given, and asks for the agent's keys: `vsock-agent refused` if the host closes it
/// unanswered, `vsock-agent answered` if it answers, `vsock-agent dial ERROR` if it cannot
/// dial.
fn vsock_agent(port: u32) -> i32 {
    use std::io::Read as _;
    use std::os::fd::FromRawFd as _;
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        let _ = writeln!(io::stdout(), "vsock-agent dial {}", io::Error::last_os_error());
        return 1;
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let mut sock = unsafe { std::fs::File::from_raw_fd(fd) };
    // SAFETY: an all-zero sockaddr_vm is a valid value.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = libc::VMADDR_CID_HOST;
    addr.svm_port = port;
    // SAFETY: connect(2) with an address of its own type and size.
    if unsafe {
        libc::connect(
            fd,
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    } != 0
    {
        let _ = writeln!(io::stdout(), "vsock-agent dial {}", io::Error::last_os_error());
        return 1;
    }
    let id = b"default";
    let mut hello = vec![0u8; 16];
    hello.extend_from_slice(&(id.len() as u16).to_be_bytes());
    hello.extend_from_slice(id);
    hello.extend_from_slice(&[0, 0, 0, 1, 11]);
    let _ = sock.write_all(&hello);
    let mut answer = [0u8; 1];
    let said = match sock.read(&mut answer) {
        Ok(0) | Err(_) => "refused",
        Ok(_) => "answered",
    };
    let _ = writeln!(io::stdout(), "vsock-agent {said}");
    0
}

/// Speaks to the SSH agent at `SSH_AUTH_SOCK`: lists its keys (`agent keys N COMMENT...`),
/// signs with the first (`agent signed` or `agent sign refused`), and asks it to forget
/// them all (`agent remove refused` or `agent removed`).
fn agent() -> i32 {
    use std::io::Read as _;
    let Some(path) = std::env::var_os("SSH_AUTH_SOCK") else {
        let _ = writeln!(io::stdout(), "agent no SSH_AUTH_SOCK");
        return 1;
    };
    let mut c = match std::os::unix::net::UnixStream::connect(&path) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(io::stdout(), "agent connect {e}");
            return 1;
        }
    };
    let mut ask = |body: &[u8]| -> io::Result<Vec<u8>> {
        c.write_all(&(body.len() as u32).to_be_bytes())?;
        c.write_all(body)?;
        let mut n = [0u8; 4];
        c.read_exact(&mut n)?;
        let mut answer = vec![0u8; u32::from_be_bytes(n) as usize];
        c.read_exact(&mut answer)?;
        Ok(answer)
    };
    let take = |b: &[u8], at: &mut usize| -> Option<Vec<u8>> {
        let n = u32::from_be_bytes(b.get(*at..*at + 4)?.try_into().ok()?) as usize;
        let v = b.get(*at + 4..*at + 4 + n)?.to_vec();
        *at += 4 + n;
        Some(v)
    };
    let Ok(list) = ask(&[11]) else { return 1 };
    let mut out = String::new();
    let mut first_key = None;
    if list.first() == Some(&12) {
        let n = u32::from_be_bytes(list.get(1..5).and_then(|b| b.try_into().ok()).unwrap_or([0; 4]));
        let mut at = 5;
        let mut comments = Vec::new();
        for _ in 0..n {
            let (Some(key), Some(comment)) = (take(&list, &mut at), take(&list, &mut at)) else {
                break;
            };
            first_key.get_or_insert(key);
            comments.push(String::from_utf8_lossy(&comment).into_owned());
        }
        out.push_str(&format!("agent keys {n} {}\n", comments.join(" ")));
    }
    if let Some(key) = first_key {
        let mut body = vec![13];
        body.extend_from_slice(&(key.len() as u32).to_be_bytes());
        body.extend_from_slice(&key);
        body.extend_from_slice(&5u32.to_be_bytes());
        body.extend_from_slice(b"hello");
        body.extend_from_slice(&0u32.to_be_bytes());
        match ask(&body) {
            Ok(a) if a.first() == Some(&14) => out.push_str("agent signed\n"),
            _ => out.push_str("agent sign refused\n"),
        }
    }
    match ask(&[19]) {
        Ok(a) if a.first() == Some(&5) => out.push_str("agent remove refused\n"),
        _ => out.push_str("agent removed\n"),
    }
    let _ = io::stdout().write_all(out.as_bytes());
    0
}

fn serve(port: &str, connections: usize) -> i32 {
    let listener = match std::net::TcpListener::bind(format!("0.0.0.0:{port}")) {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(io::stdout(), "serve error {e}");
            return 1;
        }
    };
    let _ = writeln!(io::stdout(), "ready");
    for _ in 0..connections {
        let served = listener.accept().and_then(|(mut c, peer)| {
            writeln!(c, "from {}", peer.ip())?;
            let mut read = c.try_clone()?;
            let echoed = io::copy(&mut read, &mut c)?;
            // What it had of the connection, for a test to tell a stream cut on its way in
            // from one cut on its way back.
            let _ = writeln!(io::stdout(), "served {echoed}");
            c.shutdown(std::net::Shutdown::Write)
        });
        if let Err(e) = served {
            let _ = writeln!(io::stdout(), "serve error {e}");
            return 1;
        }
    }
    0
}

/// Listens on TCP `port` at every address, says `ready`, then holds every connection it
/// is given, echoing what each sends, until it is killed: many idle connections beside
/// the few that talk. On epoll(7), so that the connections held cost it nothing an
/// event; its descriptor limit raised to the hard one first.
fn hold(port: &str) -> i32 {
    use std::collections::HashMap;
    use std::os::fd::AsRawFd as _;
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) and setrlimit(2) of our own limit, through a local.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
            lim.rlim_cur = lim.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
    let listener = match std::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .and_then(|l| l.set_nonblocking(true).map(|()| l))
    {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(io::stdout(), "hold error {e}");
            return 1;
        }
    };
    // SAFETY: epoll_create1(2) with a flag.
    let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    let watch = |fd: i32, token: u64| {
        let mut e = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: token,
        };
        // SAFETY: epoll_ctl(2) adding a descriptor we hold.
        unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut e) == 0 }
    };
    if ep < 0 || !watch(listener.as_raw_fd(), u64::MAX) {
        let _ = writeln!(io::stdout(), "hold error {}", io::Error::last_os_error());
        return 1;
    }
    let _ = writeln!(io::stdout(), "ready");
    let mut held: HashMap<u64, std::net::TcpStream> = HashMap::new();
    let mut next = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    // SAFETY: epoll_event is plain data, for which all zeroes is a value.
    let mut events: [libc::epoll_event; 64] = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: epoll_wait(2) into a buffer of 64 events.
        let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), 64, -1) };
        for e in events.iter().take(usize::try_from(n).unwrap_or(0)) {
            let token = e.u64;
            if token == u64::MAX {
                while let Ok((c, _)) = listener.accept() {
                    if c.set_nonblocking(true).is_ok() && watch(c.as_raw_fd(), next) {
                        held.insert(next, c);
                        next += 1;
                    }
                }
                continue;
            }
            let Some(c) = held.get_mut(&token) else { continue };
            loop {
                match c.read(&mut buf) {
                    Ok(0) => {
                        held.remove(&token);
                        break;
                    }
                    Ok(got) => {
                        let _ = c.set_nonblocking(false);
                        let _ = c.write_all(buf.get(..got).unwrap_or_default());
                        let _ = c.set_nonblocking(true);
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

/// Binds UDP `port` at every address, says `ready`, then answers `datagrams` datagrams
/// in turn, each with `from IP:PORT ` (its sender) and what it held.
fn udp_echo(port: &str, datagrams: usize) -> i32 {
    let sock = match std::net::UdpSocket::bind(format!("0.0.0.0:{port}")) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(io::stdout(), "udp error {e}");
            return 1;
        }
    };
    let _ = writeln!(io::stdout(), "ready");
    let mut buf = [0u8; 2048];
    for _ in 0..datagrams {
        let answered = sock.recv_from(&mut buf).and_then(|(n, peer)| {
            let mut answer = format!("from {peer} ").into_bytes();
            answer.extend_from_slice(buf.get(..n).unwrap_or_default());
            sock.send_to(&answer, peer)
        });
        if let Err(e) = answered {
            let _ = writeln!(io::stdout(), "udp error {e}");
            return 1;
        }
    }
    0
}

/// File operations, in order: `mkdir:P`, `write:P=DATA`, `link:OLD:NEW`, `symlink:T:P`,
/// `rm:P`, `rmdir:P` (and what it holds), `chmod:OCTAL:P`, `mknod:b|c:MAJOR:MINOR:P`,
/// `open:r|w:P`, `dev:P`, which prints a device node's type, numbers and mode, `print:P`,
/// which prints a file, and `readn:N:P`, which reads N bytes of P. Stops
/// at the first that fails, saying which.
fn fs(ops: &[String]) -> i32 {
    use std::os::unix::fs::PermissionsExt as _;
    for op in ops {
        let (kind, rest) = op.split_once(':').unwrap_or((op.as_str(), ""));
        let done = match kind {
            "mkdir" => std::fs::create_dir(rest),
            "write" => {
                let (p, data) = rest.split_once('=').unwrap_or((rest, ""));
                std::fs::write(p, data)
            }
            "link" => {
                let (a, b) = rest.split_once(':').unwrap_or((rest, ""));
                std::fs::hard_link(a, b)
            }
            "symlink" => {
                let (t, p) = rest.split_once(':').unwrap_or((rest, ""));
                std::os::unix::fs::symlink(t, p)
            }
            "rm" => std::fs::remove_file(rest),
            "rmdir" => std::fs::remove_dir_all(rest),
            "chmod" => {
                let (m, p) = rest.split_once(':').unwrap_or((rest, ""));
                u32::from_str_radix(m, 8)
                    .map_err(io::Error::other)
                    .and_then(|m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)))
            }
            "mknod" => mknod(rest),
            "open" => {
                let (m, p) = rest.split_once(':').unwrap_or((rest, ""));
                std::fs::OpenOptions::new()
                    .read(m == "r")
                    .write(m == "w")
                    .open(p)
                    .map(drop)
            }
            "print" => std::fs::read(rest).map(|b| {
                let _ = io::stdout().write_all(&b);
            }),
            "readn" => {
                let (n, p) = rest.split_once(':').unwrap_or((rest, ""));
                n.parse::<u64>().map_err(io::Error::other).and_then(|n| {
                    let mut f = std::fs::File::open(p)?.take(n);
                    let read = io::copy(&mut f, &mut io::sink())?;
                    if read == n {
                        Ok(())
                    } else {
                        Err(io::Error::other(format!("{read} bytes")))
                    }
                })
            }
            "dev" => std::fs::symlink_metadata(rest).map(|m| {
                use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
                let t = if m.file_type().is_block_device() { 'b' } else { 'c' };
                let rdev = m.rdev();
                let _ = writeln!(
                    io::stdout(),
                    "{rest} {t} {}:{} {:o}",
                    libc::major(rdev),
                    libc::minor(rdev),
                    m.mode() & 0o7777
                );
            }),
            _ => Err(io::Error::other("an unknown operation")),
        };
        if let Err(e) = done {
            let _ = writeln!(io::stderr(), "{op}: {e}");
            return 1;
        }
    }
    0
}

/// `KIND:MAJOR:MINOR:PATH`'s node, mode 0600.
fn mknod(spec: &str) -> io::Result<()> {
    let mut parts = spec.splitn(4, ':');
    let bad = || io::Error::other("a malformed mknod");
    let kind = match parts.next() {
        Some("b") => libc::S_IFBLK,
        Some("c") => libc::S_IFCHR,
        _ => return Err(bad()),
    };
    let major: u32 = parts.next().and_then(|n| n.parse().ok()).ok_or_else(bad)?;
    let minor: u32 = parts.next().and_then(|n| n.parse().ok()).ok_or_else(bad)?;
    let path = std::ffi::CString::new(parts.next().ok_or_else(bad)?).map_err(|_| bad())?;
    // SAFETY: mknod(2) of a NUL-terminated path.
    if unsafe { libc::mknod(path.as_ptr(), kind | 0o600, libc::makedev(major, minor)) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends a byte to itself over TCP on 127.0.0.1, on ::1, and from any address to its own
/// host name as the C library resolves it, and prints how each went.
fn loopback() -> i32 {
    use std::net::{TcpListener, TcpStream, ToSocketAddrs};
    let mut code = 0;
    let mut name = [0u8; 256];
    // SAFETY: a buffer of the given length.
    let named = unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == 0;
    let end = name.iter().position(|&b| b == 0).unwrap_or(0);
    let own = String::from_utf8_lossy(name.get(..end).unwrap_or_default()).into_owned();
    for addr in ["127.0.0.1:0", "[::1]:0", "own name"] {
        let tried = (|| -> io::Result<()> {
            let (listener, to) = if addr == "own name" {
                if !named {
                    return Err(io::Error::last_os_error());
                }
                let listener = TcpListener::bind("0.0.0.0:0")?;
                let port = listener.local_addr()?.port();
                let to = (own.as_str(), port)
                    .to_socket_addrs()?
                    .next()
                    .ok_or_else(|| io::Error::other("no address"))?;
                (listener, to)
            } else {
                let listener = TcpListener::bind(addr)?;
                let to = listener.local_addr()?;
                (listener, to)
            };
            let mut client = TcpStream::connect(to)?;
            client.write_all(b"x")?;
            let (mut server, _) = listener.accept()?;
            let mut byte = [0u8; 1];
            server.read_exact(&mut byte)?;
            if byte != *b"x" {
                return Err(io::Error::other("a different byte"));
            }
            Ok(())
        })();
        let _ = match tried {
            Ok(()) => writeln!(io::stdout(), "{addr} ok"),
            Err(e) => {
                code = 1;
                writeln!(io::stdout(), "{addr} {e}")
            }
        };
    }
    code
}

fn cat() -> i32 {
    let mut buf = vec![0u8; 64 * 1024];
    let (mut stdin, mut stdout) = (io::stdin().lock(), io::stdout().lock());
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => return 0,
            Ok(n) => {
                if stdout.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    return 1;
                }
            }
            Err(_) => return 1,
        }
    }
}

/// Writes `len` pattern bytes of `salt` to stdout.
fn bulk(len: u64, salt: u64) -> i32 {
    let mut buf = vec![0u8; 64 * 1024];
    let mut stdout = io::stdout().lock();
    let mut at = 0;
    while at < len {
        let n = (len - at).min(buf.len() as u64) as usize;
        let chunk = buf.get_mut(..n).unwrap_or_default();
        fill(salt, at, chunk);
        if stdout.write_all(chunk).is_err() {
            return 1;
        }
        at += n as u64;
    }
    0
}

/// Leaves a child that holds stdout open and never exits, then exits: the run must still
/// end, as a container ends with its main process.
fn orphan() -> i32 {
    // SAFETY: fork(2); the child only sleeps.
    if unsafe { libc::fork() } == 0 {
        loop {
            // SAFETY: blocks until a signal arrives.
            unsafe { libc::pause() };
        }
    }
    let _ = io::stdout().write_all(b"parent done\n");
    0
}

/// Dials each host vsock port (CID 2) and prints `PORT connected`, then what the host
/// sends within a second (`read N bytes`, `closed` or `nothing`), or `PORT refused ERRNO`:
/// what a workload reaches of the host when nothing confines it.
fn vsock(ports: &[String]) -> i32 {
    for port in ports {
        let Ok(n) = port.parse::<u32>() else {
            return 2;
        };
        // SAFETY: socket(2), connect(2), setsockopt(2) and read(2) on a descriptor this
        // function owns and closes, with a sockaddr_vm it initializes.
        let line = unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                format!("{n} socket {}", io::Error::last_os_error())
            } else {
                let mut addr: libc::sockaddr_vm = std::mem::zeroed();
                addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
                addr.svm_cid = libc::VMADDR_CID_HOST;
                addr.svm_port = n;
                let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
                let line = if libc::connect(fd, (&raw const addr).cast(), len) == 0 {
                    let wait = libc::timeval {
                        tv_sec: 1,
                        tv_usec: 0,
                    };
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_RCVTIMEO,
                        (&raw const wait).cast(),
                        std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                    );
                    let mut buf = [0u8; 4096];
                    match libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) {
                        0 => format!("{n} connected closed"),
                        r if r > 0 => format!("{n} connected read {r} bytes"),
                        _ => format!("{n} connected nothing"),
                    }
                } else {
                    format!("{n} refused {}", io::Error::last_os_error())
                };
                libc::close(fd);
                line
            }
        };
        let _ = writeln!(io::stdout(), "{line}");
    }
    0
}

/// What a terminal workload sees, one `key value` per line: whether each of its stdio is
/// a terminal, whether that is its session's controlling terminal with it in the
/// foreground, the terminal's size and TERM. Then `winch` waits for SIGWINCH and reports
/// the new size, and `read` reads a line from stdin and reports it.
fn tty(then: &str) -> i32 {
    fn size() -> String {
        // SAFETY: TIOCGWINSZ fills a zeroed winsize.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) } != 0 {
            return "none".into();
        }
        format!("{} {}", ws.ws_row, ws.ws_col)
    }
    // SIGWINCH is blocked from the start, so none is lost before sigsuspend(2) waits.
    // SAFETY: sigset operations on locals; the handler only stores an atomic.
    let waiting = unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGWINCH);
        let mut before: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut before);
        libc::signal(
            libc::SIGWINCH,
            caught as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::sigdelset(&mut before, libc::SIGWINCH);
        before
    };
    let mut out = String::new();
    for (fd, name) in [(0, "stdin"), (1, "stdout"), (2, "stderr")] {
        // SAFETY: isatty(3) on a standard descriptor.
        let tty = unsafe { libc::isatty(fd) } == 1;
        out.push_str(&format!("{name} tty {tty}\n"));
    }
    let controlling = std::fs::File::open("/dev/tty").is_ok();
    // SAFETY: plain getters.
    let foreground = unsafe { libc::tcgetpgrp(0) == libc::getpgrp() };
    out.push_str(&format!("controlling {controlling}\nforeground {foreground}\n"));
    out.push_str(&format!("size {}\n", size()));
    let term = std::env::var("TERM").unwrap_or_default();
    out.push_str(&format!("term {term}\nready\n"));
    let _ = io::stdout().write_all(out.as_bytes());
    match then {
        "winch" => {
            while CAUGHT.load(Ordering::Relaxed) == 0 {
                // SAFETY: waits with SIGWINCH unblocked, on a valid set.
                unsafe { libc::sigsuspend(&waiting) };
            }
            let _ = writeln!(io::stdout(), "resized {}", size());
        }
        "read" => {
            let mut line = String::new();
            let _ = io::stdin().read_line(&mut line);
            let _ = writeln!(io::stdout(), "read {}", line.trim_end());
        }
        _ => {}
    }
    0
}

/// The signal `trap` caught, by its number.
static CAUGHT: AtomicI32 = AtomicI32::new(0);

extern "C" fn caught(sig: libc::c_int) {
    CAUGHT.store(sig, Ordering::Relaxed);
}

/// Catches the named signal, says `ready`, and reports the number it arrives with.
/// Ignores the signal named, says it is ready, and waits for ever: what only another
/// signal ends.
fn ignore(name: &str) -> i32 {
    let sig = match name {
        "TERM" => libc::SIGTERM,
        "USR1" => libc::SIGUSR1,
        _ => return 2,
    };
    // SAFETY: a disposition of this process's own.
    unsafe { libc::signal(sig, libc::SIG_IGN) };
    let _ = writeln!(io::stdout(), "ready");
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}

fn trap(name: &str) -> i32 {
    let sig = match name {
        "INT" => libc::SIGINT,
        "TERM" => libc::SIGTERM,
        "USR1" => libc::SIGUSR1,
        _ => return 2,
    };
    // The signal stays blocked but while sigsuspend(2) waits for it: one that arrived
    // between a check of CAUGHT and pause(2) would leave pause waiting for ever.
    // SAFETY: sigset operations on locals, and an async-signal-safe handler that only
    // stores an atomic.
    let waiting = unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, sig);
        let mut before: libc::sigset_t = std::mem::zeroed();
        libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut before);
        libc::signal(sig, caught as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::sigdelset(&mut before, sig);
        before
    };
    let _ = writeln!(io::stdout(), "ready");
    while CAUGHT.load(Ordering::Relaxed) == 0 {
        // SAFETY: waits with `sig` unblocked, on a valid set.
        unsafe { libc::sigsuspend(&waiting) };
    }
    let _ = writeln!(io::stdout(), "got {}", CAUGHT.load(Ordering::Relaxed));
    0
}

/// As an agent's first process (D59): what its domain gives it, a line each, `confined`
/// first, then `comm` set to `confined-ready` and held until killed. Each path in `see`
/// is listed or its errno said; each in `write` written or its errno said.
fn confined(args: &[String]) -> i32 {
    let mut out = String::new();
    // SAFETY: plain getters, and getgroups(2) into 64 entries.
    let (uid, gid, groups) = unsafe {
        let mut g = [0 as libc::gid_t; 64];
        let n = libc::getgroups(64, g.as_mut_ptr());
        (libc::getuid(), libc::getgid(), usize::try_from(n).unwrap_or(0))
    };
    out.push_str(&format!("confined uid={uid} gid={gid} groups={groups}\n"));
    let procs = std::fs::read_dir("/proc")
        .map(|d| {
            d.flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .bytes()
                        .all(|b| b.is_ascii_digit())
                })
                .count()
        })
        .unwrap_or(0);
    out.push_str(&format!("confined pid={} procs={procs}\n", std::process::id()));
    // SAFETY: an all-zero utsname is a valid out-parameter, and uname(2) NUL-terminates.
    let host = unsafe {
        let mut uts: libc::utsname = std::mem::zeroed();
        libc::uname(&mut uts);
        std::ffi::CStr::from_ptr(uts.nodename.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    out.push_str(&format!("confined hostname={host}\n"));
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |k: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()))
            .unwrap_or_default()
    };
    out.push_str(&format!(
        "confined caps eff={} prm={} inh={} bnd={} amb={} nnp={}\n",
        field("CapEff:"),
        field("CapPrm:"),
        field("CapInh:"),
        field("CapBnd:"),
        field("CapAmb:"),
        field("NoNewPrivs:")
    ));
    let errno = |e: &io::Error| format!("errno {}", e.raw_os_error().unwrap_or(0));
    let mut mode: &str = "";
    for a in args {
        match a.as_str() {
            "see" | "write" | "bind" | "connect" | "call" | "listen" | "reach" | "unreach" | "cat"
            | "resolve" => mode = a.as_str(),
            name if mode == "resolve" => {
                let c = std::ffi::CString::new(name).unwrap_or_default();
                let mut res: *mut libc::addrinfo = std::ptr::null_mut();
                // SAFETY: getaddrinfo(3) of a NUL-terminated name, its list freed after.
                let mut rcs = Vec::new();
                for stream in [false, true] {
                    // SAFETY: an all-zero addrinfo is valid hints.
                    let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
                    hints.ai_socktype = if stream { libc::SOCK_STREAM } else { 0 };
                    // SAFETY: getaddrinfo(3) of a NUL-terminated name, its list freed after.
                    let rc = unsafe {
                        let rc =
                            libc::getaddrinfo(c.as_ptr(), std::ptr::null(), &raw const hints, &raw mut res);
                        if rc == 0 {
                            libc::freeaddrinfo(res);
                        }
                        rc
                    };
                    rcs.push(rc.to_string());
                }
                out.push_str(&format!("confined resolve {name}: {}\n", rcs.join(",")));
            }
            path if mode == "cat" => match std::fs::read_to_string(path) {
                Ok(text) => {
                    for line in text.lines() {
                        out.push_str(&format!("confined cat {path}: {line}\n"));
                    }
                }
                Err(e) => out.push_str(&format!("confined cat {path}: {}\n", errno(&e))),
            },
            port if mode == "listen" => {
                let said = match std::net::TcpListener::bind(format!("0.0.0.0:{port}")) {
                    Ok(l) => {
                        // Answers every connection, for as long as the agent runs.
                        let _ = std::thread::Builder::new().spawn(move || for _ in l.incoming() {});
                        "ok".to_string()
                    }
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined listen {port}: {said}\n"));
            }
            addr if mode == "reach" || mode == "unreach" => {
                use std::net::ToSocketAddrs as _;
                let once = |wait: u64| -> Result<(), io::Error> {
                    let to = addr
                        .to_socket_addrs()?
                        .next()
                        .ok_or_else(|| io::Error::other("no address"))?;
                    std::net::TcpStream::connect_timeout(&to, std::time::Duration::from_secs(wait)).map(drop)
                };
                let said = |r: Result<(), io::Error>| match r {
                    Ok(()) => "ok".to_string(),
                    Err(e) if e.kind() == io::ErrorKind::TimedOut => "timeout".to_string(),
                    Err(e) if e.raw_os_error().is_none() => format!("{e}"),
                    Err(e) => errno(&e),
                };
                let result = if mode == "reach" {
                    // Until it listens: a refusal still says packets arrive.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        let r = once(2);
                        if r.is_ok() || std::time::Instant::now() >= deadline {
                            break r;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                } else {
                    once(3)
                };
                out.push_str(&format!("confined {mode} {addr}: {}\n", said(result)));
            }
            what if mode == "call" => {
                // SAFETY: each a system call with constant or null arguments, its
                // descriptor, if any, closed after.
                let (rc, err) = unsafe {
                    let rc = match what {
                        "socket-vsock" => libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) as libc::c_long,
                        "socket-netlink" => libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, 0) as libc::c_long,
                        "socket-unix" => libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) as libc::c_long,
                        "socket-inet6" => libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) as libc::c_long,
                        "io_uring_setup" => {
                            libc::syscall(libc::SYS_io_uring_setup, 1u32, std::ptr::null_mut::<u8>())
                        }
                        // KEYCTL_GET_KEYRING_ID of the session keyring, made if absent.
                        "keyctl" => libc::syscall(libc::SYS_keyctl, 0, -3i32, 1),
                        "userfaultfd" => libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC),
                        "fork" => {
                            let pid = libc::fork();
                            if pid == 0 {
                                libc::_exit(0);
                            }
                            if pid > 0 {
                                libc::waitpid(pid, std::ptr::null_mut(), 0);
                                0
                            } else {
                                -1
                            }
                        }
                        "thread" => match std::thread::Builder::new().spawn(|| ()) {
                            Ok(t) => i64::from(t.join().is_ok()) - 1,
                            Err(_) => -1,
                        },
                        _ => -1,
                    };
                    (rc, io::Error::last_os_error())
                };
                let said = if rc >= 0 {
                    if !matches!(what, "keyctl" | "fork" | "thread") {
                        // SAFETY: closing the descriptor just made.
                        unsafe { libc::close(rc as libc::c_int) };
                    }
                    "ok".to_string()
                } else {
                    errno(&err)
                };
                out.push_str(&format!("confined call {what}: {said}\n"));
            }
            addr if mode == "bind" => {
                let said = match std::net::TcpListener::bind(addr) {
                    Ok(_) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined bind {addr}: {said}\n"));
            }
            addr if mode == "connect" => {
                let said = match std::net::TcpStream::connect(addr) {
                    Ok(_) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined connect {addr}: {said}\n"));
            }
            path if mode == "see" => {
                let said = match std::fs::read_dir(path) {
                    Ok(d) => {
                        let mut names: Vec<String> = d
                            .flatten()
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect();
                        names.sort();
                        names.join(",")
                    }
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined see {path}: {said}\n"));
            }
            path => {
                let said = match std::fs::write(path, "x") {
                    Ok(()) => "ok".to_string(),
                    Err(e) => errno(&e),
                };
                out.push_str(&format!("confined write {path}: {said}\n"));
            }
        }
    }
    let ifaces: Vec<String> = std::fs::read_to_string("/proc/net/dev")
        .unwrap_or_default()
        .lines()
        .skip(2)
        .filter_map(|l| l.split(':').next().map(|n| n.trim().to_string()))
        .collect();
    out.push_str(&format!("confined net={}\n", ifaces.join(",")));
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stdout().flush();
    // SAFETY: prctl(2) naming this thread, with a NUL-terminated name of 15 bytes or fewer.
    unsafe { libc::prctl(libc::PR_SET_NAME, c"confined-ready".as_ptr()) };
    loop {
        // SAFETY: blocks until a signal arrives.
        unsafe { libc::pause() };
    }
}

/// Waits, 60 s at most, until `count` processes whose `comm` is `name` exist; then says
/// whether it reaches `then`, an address, where one is given.
fn await_process(name: &str, count: usize, then: &str) -> i32 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        let found = std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim_end() == name))
            .count();
        if found >= count {
            // Then, where an address is given, whether this process reaches it, in 3 s.
            if !then.is_empty() {
                use std::net::ToSocketAddrs as _;
                let said = match then.to_socket_addrs().map(|mut a| a.next()) {
                    Ok(Some(to)) => {
                        match std::net::TcpStream::connect_timeout(&to, std::time::Duration::from_secs(3)) {
                            Ok(_) => "ok".to_string(),
                            Err(e) if e.kind() == io::ErrorKind::TimedOut => "timeout".to_string(),
                            Err(e) => format!("errno {}", e.raw_os_error().unwrap_or(0)),
                        }
                    }
                    Ok(None) => "no address".to_string(),
                    Err(e) => e.to_string(),
                };
                let _ = writeln!(io::stdout(), "await unreach {then}: {said}");
            }
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let _ = writeln!(io::stdout(), "await timeout {name}");
    1
}

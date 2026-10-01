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
        "stderr" => {
            let _ = io::stderr().write_all(arg(1).as_bytes());
            0
        }
        "exit" => arg(1).parse().unwrap_or(1),
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
        "tty" => tty(arg(1)),
        "vsock" => vsock(args.get(1..).unwrap_or_default()),
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

/// Copies stdin to stdout.
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

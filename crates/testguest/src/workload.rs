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
        "orphan" => orphan(),
        "trap" => trap(arg(1)),
        "tty" => tty(arg(1)),
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

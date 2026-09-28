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

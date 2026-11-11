//! Finding 11 on macOS, which has no pidfd: managed_cleanup.rs's identity (PID and the start
//! time proc_pidinfo(2) reports) and its check's decision, run against a reaped child, an
//! unreaped zombie, a running child, and a live process whose start time differs, as one that
//! reused a reaped child's PID would. The check asserts there; `rescued` returns its decision.
use std::io;
use std::process::Command;

// ---- verbatim from crates/shards/tests/managed_cleanup.rs (target_os = "macos") ----
#[derive(Clone, Copy)]
struct ProcessIdentity {
    pid: libc::pid_t,
    started: (u64, u64),
}

impl ProcessIdentity {
    fn of(pid: u32) -> io::Result<Self> {
        let pid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
        let started = started(pid).ok_or_else(io::Error::last_os_error)?;
        Ok(Self { pid, started })
    }
}

fn started(pid: libc::pid_t) -> Option<(u64, u64)> {
    // SAFETY: proc_bsdinfo is plain data, for which all zeroes is a value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: a buffer of exactly `size` bytes for proc_pidinfo(2) to fill.
    let filled = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 1, (&raw mut info).cast(), size) };
    (filled == size).then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

// Returned::check (macOS), its assertion turned into the returned decision.
fn rescued(identity: ProcessIdentity) -> bool {
    let ProcessIdentity { pid, started: at } = identity;
    let unreaped = started(pid) == Some(at);
    if unreaped {
        let mut status = 0;
        // SAFETY: an unreaped child of this process keeps its PID until reaped here.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, &raw mut status, 0);
        }
    }
    unreaped
}
// ---- end verbatim ----

fn main() -> io::Result<()> {
    let mut reaped = Command::new("/bin/sh").args(["-c", "exit 0"]).spawn()?;
    let id = ProcessIdentity::of(reaped.id())?;
    reaped.wait()?;
    println!("reaped child {}: rescue needed = {} (expected false)", id.pid, rescued(id));

    let zombie = Command::new("/bin/sh").args(["-c", "exit 0"]).spawn()?;
    let id = ProcessIdentity::of(zombie.id())?;
    // SAFETY: siginfo_t is plain data; waitid(2) with WNOWAIT waits for the exit and leaves the
    // child unreaped.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let pid = libc::id_t::try_from(id.pid).map_err(io::Error::other)?;
    // SAFETY: as above, for this child alone.
    let waited = unsafe { libc::waitid(libc::P_PID, pid, &raw mut info, libc::WEXITED | libc::WNOWAIT) };
    println!(
        "zombie child {} (waitid WNOWAIT {waited}): rescue needed = {} (expected true; the rescue reaps it)",
        id.pid,
        rescued(id)
    );
    drop(zombie);

    let running = Command::new("/bin/sleep").arg("30").spawn()?;
    let id = ProcessIdentity::of(running.id())?;
    println!("running child {}: rescue needed = {} (expected true; the rescue ends and reaps it)", id.pid, rescued(id));
    drop(running);

    let mut other = Command::new("/bin/sleep").arg("30").spawn()?;
    let live = ProcessIdentity::of(other.id())?;
    let earlier = ProcessIdentity {
        pid: live.pid,
        started: (live.started.0.saturating_sub(1), live.started.1),
    };
    let decision = rescued(earlier);
    println!(
        "live process {} named by an identity with another start time: rescue needed = {decision} (expected false); left running = {} (expected true)",
        live.pid,
        other.try_wait()?.is_none()
    );
    other.kill()?;
    other.wait()?;
    Ok(())
}

//! A build's own writes to its host's disk, made outside the budget it is held to (D188,
//! PM M442), on Linux.
//!
//! A build held to a budget (its cgroup's, or one above it, `memory.high` or `memory.max`)
//! wrote the files it makes (a `RUN` step's changes, its layers, its image's root
//! filesystem) at its disk's pace, where with none they went at its memory's: the page
//! cache a write dirties is charged to the writer's cgroup, and the kernel holds a cgroup's
//! dirty pages to its own share of what its budget leaves it. `balance_dirty_pages`
//! (mm/page-writeback.c, Linux 7.0) throttles a writer against its own cgroup's domain as
//! well as the host's (`mdtc`, `mem_cgroup_wb_domain` of the writer's own wb,
//! `wb_get_create_current` in `balance_dirty_pages_ratelimited_flags`), whose memory
//! available to dirty is its file pages and no more of its headroom than the budget leaves
//! (`mdtc_calc_avail`: `filepages + min(headroom, other_clean)`; mm/memcontrol.c,
//! `mem_cgroup_wb_stats`: the headroom is `min(memory.max, memory.high)` less what the
//! cgroup uses, the least of it and each above it). A budget its build fills leaves no
//! headroom and few file pages, so its writer passes `dirty_ratio`'s share of them
//! (`domain_dirty_limits`) after a few megabytes and is paced to its writeback; what it
//! dirties past the budget waits besides in `memory.high`'s throttle until it is written
//! back. With no budget the same pages are the host's to dirty, flushed after.
//!
//! So a build held to one hands its writes to a process of its own placed outside it: its
//! user's manager makes that process a scope of its own in the user's application slice
//! (systemd.special(7), `app.slice`), as the daemon places a spawner (D183), and where that
//! scope is not under the budget it writes what the build gives it, each write to the
//! build's own open file (passed with it, SCM_RIGHTS), at that file's offset, as the build
//! would have (`shards_image::through`). The page cache they dirty is charged to it, and
//! paced by the host's dirty limit alone, as BuildKit's own writes are made by buildkitd,
//! never in the cgroup its `RUN` steps are given (docker build `--cgroup-parent`: "the
//! parent cgroup for the RUN instructions"). The builder's VM process, whose memory the
//! budget holds, stays in it. Where nothing places it there (no user manager), the build
//! writes its files itself.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, FromRawFd as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, PoisonError};

/// Build → writer: bytes to write to the descriptor that comes with them.
const WRITE: u8 = 1;
/// Writer → build: how many of them it wrote (8 bytes, big-endian).
const WROTE: u8 = 2;
/// Writer → build: why it wrote none: the error's number (4 bytes, big-endian, 0 for
/// none), then its words.
const FAILED: u8 = 3;

/// The most bytes one write hands over: a message's most.
const CHUNK: usize = shards_ipc::MAX_PAYLOAD;

/// Where cgroup v2 is mounted.
const ROOT: &str = "/sys/fs/cgroup";

/// The slice the writer's scope is made in: the user's manager's own for applications
/// (systemd.special(7)).
const SLICE: &str = "app.slice";

/// The writer outside the build's budget: its connection, and its process.
struct Outside {
    conn: Mutex<UnixStream>,
    _child: shards_ipc::Child,
}

impl shards_image::through::Writer for Outside {
    fn write(&self, file: &File, buf: &[u8]) -> io::Result<usize> {
        let chunk = buf.get(..buf.len().min(CHUNK)).unwrap_or_default();
        let conn = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        shards_ipc::send(&conn, WRITE, chunk, &[file.as_fd()])?;
        let answer = shards_ipc::recv(&conn)?.ok_or_else(|| io::Error::other("the build's writer ended"))?;
        answered(answer.kind, &answer.payload, chunk.len())
    }
}

/// A writer's answer to a write of `asked` bytes: how many it wrote, or why it wrote none.
fn answered(kind: u8, payload: &[u8], asked: usize) -> io::Result<usize> {
    let malformed = || io::Error::other("the build's writer answered a write malformed");
    match kind {
        WROTE => payload
            .first_chunk::<8>()
            .and_then(|n| usize::try_from(u64::from_be_bytes(*n)).ok())
            .filter(|&n| n <= asked)
            .ok_or_else(malformed),
        FAILED => {
            let (code, words) = payload.split_first_chunk::<4>().ok_or_else(malformed)?;
            match i32::from_be_bytes(*code) {
                0 => Err(io::Error::other(String::from_utf8_lossy(words).into_owned())),
                code => Err(io::Error::from_raw_os_error(code)),
            }
        }
        _ => Err(malformed()),
    }
}

/// Hands this build's writes to a writer outside the budget it is held to, where one holds
/// it and its user's manager places the writer outside: from here on, for the build's life.
/// Where none holds it, or nothing places the writer there, its writes stay its own.
pub fn begin() {
    // Once a process: each platform of a build of several is built in it.
    static BEGUN: std::sync::Once = std::sync::Once::new();
    BEGUN.call_once(begin_once);
}

fn begin_once() {
    let Some(own) = cgroup_of("self") else {
        return;
    };
    let Some(budget) = budgeted_at(&own, Path::new(ROOT)) else {
        return;
    };
    match start(&budget) {
        Ok(outside) => {
            if !shards_image::through::set(Box::new(outside)) {
                shards_vmm::debug!("a build's writes were handed to a writer already");
            }
        }
        Err(e) => shards_vmm::debug!("a build's writes stay its own, under its budget: {e}"),
    }
}

/// The writer, started and placed outside `budget`.
fn start(budget: &Path) -> Result<Outside, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a connection: {e}"))?;
    let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let err = io::stderr();
    let fds = [
        (null.as_fd(), 0),
        (err.as_fd(), 1),
        (err.as_fd(), 2),
        (theirs.as_fd(), 3),
    ];
    let child = shards_ipc::spawn_in(&exe, &[OsStr::new("build-writer")], &fds, false, &[])
        .map_err(|e| format!("starting it: {e}"))?;
    drop(theirs);
    let ended = |child: shards_ipc::Child, ours: UnixStream, why: String| {
        // Its connection closed, it ends.
        drop(ours);
        let _ = child.wait();
        Err(why)
    };
    let pid = child.id();
    // SAFETY: getuid(2) takes nothing and cannot fail.
    let uid = unsafe { libc::getuid() };
    let unit = format!("shards-build-writer-{}.scope", std::process::id());
    let description = "shards: a build's writes, outside its budget";
    let placed = shards_bus::user_bus(uid)
        .and_then(|mut bus| shards_bus::place(&mut bus, &unit, SLICE, pid, description, false));
    if let Err(e) = placed {
        return ended(
            child,
            ours,
            format!("your user's systemd manager placed no writer: {e}"),
        );
    }
    match cgroup_of(&pid.to_string()) {
        Some(at) if !at.starts_with(budget) => Ok(Outside {
            conn: Mutex::new(ours),
            _child: child,
        }),
        Some(at) => ended(
            child,
            ours,
            format!(
                "its scope, {}, is under the budget's {}",
                at.display(),
                budget.display()
            ),
        ),
        None => ended(child, ours, "its cgroup could not be read".into()),
    }
}

/// The cgroup v2 directory of process `pid` (`self` for this one), as its /proc entry says.
fn cgroup_of(pid: &str) -> Option<PathBuf> {
    let said = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = said.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new(ROOT).join(path.trim_start_matches('/')))
}

/// The lowest cgroup at or above `dir`, below `root`, that holds what is in it to a budget:
/// a `memory.high` or `memory.max` other than `max`.
fn budgeted_at(dir: &Path, root: &Path) -> Option<PathBuf> {
    let limited = |file: &Path| {
        std::fs::read_to_string(file)
            .ok()
            .is_some_and(|v| v.trim().parse::<u64>().is_ok())
    };
    let mut at = dir;
    while at.starts_with(root) && at != root {
        if limited(&at.join("memory.high")) || limited(&at.join("memory.max")) {
            return Some(at.to_path_buf());
        }
        at = at.parent()?;
    }
    None
}

/// `shards build-writer`: writes what the build that started it hands it on descriptor 3,
/// each to the file that comes with it, until the build closes its end. Not for people to
/// run.
pub fn serve() -> ExitCode {
    let failed = |e: &str| {
        let _ = writeln!(io::stderr(), "shards: a build's writer: {e}");
        ExitCode::FAILURE
    };
    // SAFETY: fcntl(2) asks whether descriptor 3 is open.
    if unsafe { libc::fcntl(3, libc::F_GETFD) } < 0 {
        return failed("descriptor 3 is not the build's");
    }
    // SAFETY: the connection's end the build left at descriptor 3 for this process alone.
    let conn = unsafe { UnixStream::from_raw_fd(3) };
    loop {
        let m = match shards_ipc::recv(&conn) {
            Ok(Some(m)) => m,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => return failed(&e.to_string()),
        };
        let (kind, payload) = match (m.kind, m.fds.into_iter().next()) {
            (WRITE, Some(fd)) => wrote(&mut File::from(fd), &m.payload),
            (WRITE, None) => refused(0, "a write with no file"),
            (other, _) => refused(0, &format!("message {other}")),
        };
        if let Err(e) = shards_ipc::send(&conn, kind, &payload, &[]) {
            return failed(&e.to_string());
        }
    }
}

/// Writes `buf` to `file` at its offset, as much as it takes: the answer, how many bytes
/// it wrote, or why it wrote none.
fn wrote(file: &mut File, buf: &[u8]) -> (u8, Vec<u8>) {
    let mut done = 0usize;
    while done < buf.len() {
        match file.write(buf.get(done..).unwrap_or_default()) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if done == 0 => return refused(e.raw_os_error().unwrap_or(0), &e.to_string()),
            // What it wrote is said; the error comes again with the rest.
            Err(_) => break,
        }
    }
    (WROTE, (done as u64).to_be_bytes().to_vec())
}

fn refused(code: i32, why: &str) -> (u8, Vec<u8>) {
    let mut payload = code.to_be_bytes().to_vec();
    payload.extend_from_slice(why.as_bytes());
    (FAILED, payload)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Read as _, Seek as _};

    /// A budget is found at the cgroup, or the lowest above it, whose `memory.high` or
    /// `memory.max` is a number; `max` holds nothing, and the root is never one.
    #[test]
    fn a_budget_is_the_lowest_limit_above_the_build() {
        let dir = shards_testdir::TempDir::new("outside-budget").unwrap();
        let root = dir.path().join("cg");
        let slice = root.join("budget.slice");
        let scope = slice.join("run-1.scope");
        std::fs::create_dir_all(&scope).unwrap();
        for (at, high, max) in [
            (&root, "1024", "1024"),
            (&slice, "max", "max"),
            (&scope, "max", "max"),
        ] {
            std::fs::write(at.join("memory.high"), format!("{high}\n")).unwrap();
            std::fs::write(at.join("memory.max"), format!("{max}\n")).unwrap();
        }
        assert_eq!(budgeted_at(&scope, &root), None, "the root holds no budget");
        std::fs::write(slice.join("memory.max"), "1073741824\n").unwrap();
        assert_eq!(budgeted_at(&scope, &root), Some(slice.clone()));
        std::fs::write(scope.join("memory.high"), "536870912\n").unwrap();
        assert_eq!(budgeted_at(&scope, &root), Some(scope.clone()));
        assert_eq!(budgeted_at(&dir.path().join("elsewhere"), &root), None);
    }

    /// The writer writes each file handed it at the offset its description has, as the
    /// build itself would, and answers how much; a write it cannot make is answered with
    /// why, its error's kind kept.
    #[test]
    fn a_write_lands_where_the_build_would_have_made_it() {
        let dir = shards_testdir::TempDir::new("outside-write").unwrap();
        let path = dir.path().join("f");
        let mut f = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.write_all(b"abc").unwrap();
        let (kind, payload) = wrote(&mut f.try_clone().unwrap(), b"defg");
        assert_eq!(answered(kind, &payload, 4).unwrap(), 4);
        let mut back = String::new();
        f.seek(io::SeekFrom::Start(0)).unwrap();
        f.read_to_string(&mut back).unwrap();
        assert_eq!(back, "abcdefg");
        let read_only = File::open(&path).unwrap();
        let (kind, payload) = wrote(&mut read_only.try_clone().unwrap(), b"x");
        assert_eq!(kind, FAILED);
        let e = answered(kind, &payload, 1).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EBADF));
        assert!(
            answered(WROTE, &9u64.to_be_bytes(), 4).is_err(),
            "more than was asked"
        );
        assert!(answered(7, b"", 4).is_err());
    }
}

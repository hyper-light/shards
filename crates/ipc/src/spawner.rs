//! The process that makes a process's children, once [`start_spawner`] has made one (PM
//! M158).
//!
//! A child holds every descriptor its parent had when it was made, close-on-exec ones
//! too, until it execs: XNU copies the table into it before exec closes them, whatever
//! `POSIX_SPAWN_CLOEXEC_DEFAULT` says, and Linux's posix_spawn clones it (M134). What a
//! process lets go of while another of its threads spawns stays open in that child until
//! its exec is done: a listener's port stays bound, a pipe's reader waits for its end.
//! The daemon lets go of runs' published listeners, and its clients' stdio, while it
//! spawns VMs; on an M5 Max, 13 to 20 in 100 listeners let go of so were held, up to 42
//! ms, and some pipes up to 20 ms.
//!
//! A spawner is made while its process holds nothing it lets go of, and holds nothing but
//! what it is given for each child while that child is made: what its process lets go of,
//! no child holds. Its children are its process's to watch and signal by pid, as the
//! process's own are: it reaps none until the process asks ([`Child::wait`]), so a pid
//! stays its child's until then. A spawn through it costs a round trip more (9 to 39 µs
//! at p50 there), and saves more than that where its process holds many descriptors,
//! each copied into every child the process makes (posix_spawn 165 to 187 µs at p50 in
//! the spawner, 376 to 415 µs in a process holding a thousand more).

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd as _, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::{Child, MAX_FDS, Message, recv, send};

// The spawner's first connection carries CONNECT alone: a connection to serve, its one
// descriptor. On those, it is asked SPAWN, STATUS or REAP, and answers DONE or FAILED.
const CONNECT: u8 = 1;
/// A child to make: [`encode`]'s request, with the first of the descriptors it is given,
/// the rest following in MORE messages, as many as one carries each.
const SPAWN: u8 = 2;
const MORE: u8 = 3;
/// Whether child PID (i32) has ended, waited for if the byte after it is 1: a byte, 1 if
/// it has, and its status (i32), the child left unreaped.
const STATUS: u8 = 4;
/// Child PID (i32), reaped once it has ended: its status (i32).
const REAP: u8 = 5;
const DONE: u8 = 6;
/// What was asked failed: the OS's error number (i32), 0 for none, then what it was.
const FAILED: u8 = 7;

/// A process's spawner: the process, its first connection, its connections free for a
/// request, how to make another, and which of the process's spawners it is (a child names
/// the one that made it).
struct Spawner {
    process: Child,
    first: UnixStream,
    free: Vec<UnixStream>,
    program: PathBuf,
    args: Vec<OsString>,
    generation: u64,
}

static SPAWNER: Mutex<Option<Spawner>> = Mutex::new(None);

fn current() -> MutexGuard<'static, Option<Spawner>> {
    SPAWNER.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Makes this process's spawner, `program` run with `args`, which calls
/// [`serve_spawner`]: from here on, [`spawn`](crate::spawn) has it make every child. Called
/// before this process holds anything its children must not: the spawner holds what this
/// process holds as it is made, until its own exec.
pub fn start_spawner(program: &Path, args: &[&OsStr]) -> io::Result<()> {
    let spawner = make(program, args.iter().map(|a| a.to_os_string()).collect(), 1)?;
    *current() = Some(spawner);
    Ok(())
}

/// Spawner number `generation`: given /dev/null and this process's stderr for stdio,
/// its first connection at descriptor 3, and no environment.
fn make(program: &Path, args: Vec<OsString>, generation: u64) -> io::Result<Spawner> {
    let (first, theirs) = UnixStream::pair()?;
    let null = std::fs::File::open("/dev/null")?;
    let err = io::stderr();
    let given: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
    let process = crate::unix::spawn_here(
        program,
        &given,
        &[
            (null.as_fd(), 0),
            (err.as_fd(), 1),
            (err.as_fd(), 2),
            (theirs.as_fd(), 3),
        ],
        false,
        Vec::new(),
    )?;
    Ok(Spawner {
        process,
        first,
        free: Vec::new(),
        program: program.to_path_buf(),
        args,
        generation,
    })
}

/// A child of this process's spawner, made as [`spawn`](crate::spawn) makes one with
/// `env` for its environment; `None` if this process has none, and makes its own.
pub(crate) fn spawn(
    program: &Path,
    args: &[&OsStr],
    fds: &[(BorrowedFd<'_>, RawFd)],
    detach: bool,
    env: &[(OsString, OsString)],
) -> Option<io::Result<Child>> {
    let generation = current().as_ref()?.generation;
    Some((|| {
        let request = encode(program, args, fds, detach, env)?;
        let given: Vec<BorrowedFd<'_>> = fds.iter().map(|(fd, _)| *fd).collect();
        let (answer, by) = match call(generation, SPAWN, &request, &given) {
            Ok(answer) => (answer, generation),
            // One that has gone is made again, and asked again: it never made this child.
            Err(e) => match replaced(generation)? {
                Some(next) => (call(next, SPAWN, &request, &given)?, next),
                None => return Err(e),
            },
        };
        let pid = answer
            .first_chunk::<4>()
            .map(|p| libc::pid_t::from_be_bytes(*p))
            .filter(|p| *p > 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the spawner's answer"))?;
        Ok(Child::new(pid, Some(by)))
    })())
}

/// Whether child `pid` of spawner `by` has ended, waiting for it to if `wait`: its status,
/// left unreaped; `None` while it runs.
pub(crate) fn status(by: u64, pid: libc::pid_t, wait: bool) -> io::Result<Option<i32>> {
    let mut asked = pid.to_be_bytes().to_vec();
    asked.push(u8::from(wait));
    let answer = call(by, STATUS, &asked, &[])?;
    match answer.split_first() {
        Some((0, _)) => Ok(None),
        Some((1, status)) => Ok(Some(status_in(status)?)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "the spawner's answer")),
    }
}

/// Child `pid` of spawner `by`, reaped once it has ended: its status.
pub(crate) fn reap(by: u64, pid: libc::pid_t) -> io::Result<i32> {
    status_in(&call(by, REAP, &pid.to_be_bytes(), &[])?)
}

/// Whether spawner `by` is this process's still, which reaps none of its children unasked.
pub(crate) fn serving(by: u64) -> bool {
    current().as_ref().is_some_and(|s| s.generation == by)
}

fn status_in(bytes: &[u8]) -> io::Result<i32> {
    bytes
        .first_chunk::<4>()
        .map(|s| i32::from_be_bytes(*s))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the spawner's answer"))
}

/// Asks spawner `by` `kind`, with `payload` and `fds`, on a connection of its own: its
/// answer. The connection serves the next request once this one has its answer.
fn call(by: u64, kind: u8, payload: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<Vec<u8>> {
    let (conn, theirs) = connection(by)?;
    let (first, rest) = fds.split_at(fds.len().min(MAX_FDS));
    send(&conn, kind, payload, first)?;
    for more in rest.chunks(MAX_FDS) {
        send(&conn, MORE, &[], more)?;
    }
    let answer = match recv(&conn)? {
        Some(Message {
            kind: DONE, payload, ..
        }) => Ok(payload),
        Some(Message {
            kind: FAILED,
            payload,
            ..
        }) => Err(failure(&payload)),
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the spawner answered something else",
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the spawner has gone",
            ));
        }
    };
    // Answered on, the connection is the spawner's: its end, sent over the first, is no
    // longer in flight (M24).
    drop(theirs);
    if let Some(s) = current().as_mut().filter(|s| s.generation == by) {
        s.free.push(conn);
    }
    answer
}

/// A connection to spawner `by`: one free, or one made now and sent to it, with the end
/// it was sent, which this process keeps until the spawner has answered on it: macOS
/// flushes a socket in flight that no process holds (M24).
fn connection(by: u64) -> io::Result<(UnixStream, Option<UnixStream>)> {
    let mut held = current();
    let s = held
        .as_mut()
        .filter(|s| s.generation == by)
        .ok_or_else(|| io::Error::other("the spawner that made it has gone"))?;
    if let Some(conn) = s.free.pop() {
        return Ok((conn, None));
    }
    let (ours, theirs) = UnixStream::pair()?;
    send(&s.first, CONNECT, &[], &[theirs.as_fd()])?;
    Ok((ours, Some(theirs)))
}

/// If spawner `by` has ended, another in its place, unless one is already: the one there
/// is now. `None` while `by` lives: what failed was not its end.
fn replaced(by: u64) -> io::Result<Option<u64>> {
    let mut held = current();
    let Some(s) = held.as_mut() else {
        return Ok(None);
    };
    if s.generation != by {
        return Ok(Some(s.generation));
    }
    if s.process.try_wait().is_none() {
        return Ok(None);
    }
    *s = make(&s.program, s.args.clone(), by + 1)?;
    Ok(Some(by + 1))
}

/// An error the spawner sent: its OS error, or what it said.
fn failure(payload: &[u8]) -> io::Error {
    match payload.split_first_chunk::<4>() {
        Some((code, _)) if i32::from_be_bytes(*code) != 0 => {
            io::Error::from_raw_os_error(i32::from_be_bytes(*code))
        }
        Some((_, said)) => io::Error::other(String::from_utf8_lossy(said).into_owned()),
        None => io::Error::other("the spawner failed"),
    }
}

/// What a spawn asks: whether to detach, the program, its arguments, its environment and
/// the numbers its descriptors go to, each length a big-endian u32.
fn encode(
    program: &Path,
    args: &[&OsStr],
    fds: &[(BorrowedFd<'_>, RawFd)],
    detach: bool,
    env: &[(OsString, OsString)],
) -> io::Result<Vec<u8>> {
    let mut out = vec![u8::from(detach)];
    let put = |out: &mut Vec<u8>, bytes: &[u8]| -> io::Result<()> {
        let len = u32::try_from(bytes.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(bytes);
        Ok(())
    };
    let count = |n: usize| u32::try_from(n).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput));
    put(&mut out, program.as_os_str().as_bytes())?;
    out.extend_from_slice(&count(args.len())?.to_be_bytes());
    for a in args {
        put(&mut out, a.as_bytes())?;
    }
    out.extend_from_slice(&count(env.len())?.to_be_bytes());
    for (k, v) in env {
        put(&mut out, k.as_bytes())?;
        put(&mut out, v.as_bytes())?;
    }
    out.extend_from_slice(&count(fds.len())?.to_be_bytes());
    for (_, target) in fds {
        out.extend_from_slice(&target.to_be_bytes());
    }
    Ok(out)
}

/// A spawn's request, as the spawner reads it.
struct Request {
    detach: bool,
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    targets: Vec<RawFd>,
}

fn decode(mut at: &[u8]) -> Option<Request> {
    fn number(at: &mut &[u8]) -> Option<u32> {
        let (n, rest) = at.split_first_chunk::<4>()?;
        *at = rest;
        Some(u32::from_be_bytes(*n))
    }
    fn bytes(at: &mut &[u8]) -> Option<OsString> {
        let len = usize::try_from(number(at)?).ok()?;
        let (b, rest) = at.split_at_checked(len)?;
        *at = rest;
        Some(OsString::from_vec(b.to_vec()))
    }
    let (&detach, rest) = at.split_first()?;
    at = rest;
    let program = PathBuf::from(bytes(&mut at)?);
    let args = (0..number(&mut at)?)
        .map(|_| bytes(&mut at))
        .collect::<Option<_>>()?;
    let env = (0..number(&mut at)?)
        .map(|_| Some((bytes(&mut at)?, bytes(&mut at)?)))
        .collect::<Option<_>>()?;
    let targets = (0..number(&mut at)?)
        .map(|_| number(&mut at).map(|t| t as RawFd))
        .collect::<Option<_>>()?;
    at.is_empty().then_some(Request {
        detach: detach == 1,
        program,
        args,
        env,
        targets,
    })
}

/// The spawner's work, in the process [`start_spawner`] makes, its first connection at
/// descriptor 3: a thread for each connection it is sent, until the first ends, its
/// parent gone; what it made is init's then.
pub fn serve_spawner() -> io::Result<()> {
    // A terminal's signals to its group, or a stop's, are its parent's to act on, which
    // may yet need it to reap what it made. Its children's signals are their own
    // (POSIX_SPAWN_SETSIGDEF).
    for signal in [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM] {
        // SAFETY: signal(2) setting a disposition, with no handler.
        unsafe { libc::signal(signal, libc::SIG_IGN) };
    }
    // SAFETY: descriptor 3 is the connection the spawner is made with, which nothing else
    // here holds.
    let first = unsafe { UnixStream::from_raw_fd(3) };
    while let Some(m) = recv(&first)? {
        if m.kind != CONNECT {
            continue;
        }
        for conn in m.fds {
            // One that has no thread goes: its request fails, as a spawn would.
            let _ = std::thread::Builder::new()
                .name("spawn".into())
                .spawn(move || serve(UnixStream::from(conn)));
        }
    }
    Ok(())
}

/// Answers `conn`'s requests until it ends.
fn serve(conn: UnixStream) {
    while let Ok(Some(m)) = recv(&conn) {
        let answer = match m.kind {
            SPAWN => made(&conn, m),
            STATUS => match (m.payload.first_chunk::<4>(), m.payload.get(4)) {
                (Some(pid), Some(&wait)) => {
                    crate::unix::status_of(i32::from_be_bytes(*pid), wait == 1).map(|s| match s {
                        Some(status) => [&[1u8][..], &status.to_be_bytes()].concat(),
                        None => vec![0],
                    })
                }
                _ => Err(io::Error::from(io::ErrorKind::InvalidData)),
            },
            REAP => match m.payload.first_chunk::<4>() {
                Some(pid) => crate::unix::reap(i32::from_be_bytes(*pid)).map(|s| s.to_be_bytes().to_vec()),
                None => Err(io::Error::from(io::ErrorKind::InvalidData)),
            },
            _ => Err(io::Error::from(io::ErrorKind::InvalidData)),
        };
        let sent = match answer {
            Ok(payload) => send(&conn, DONE, &payload, &[]),
            Err(e) => {
                let code = e.raw_os_error().unwrap_or(0).to_be_bytes();
                send(
                    &conn,
                    FAILED,
                    &[&code[..], e.to_string().as_bytes()].concat(),
                    &[],
                )
            }
        };
        if sent.is_err() {
            return;
        }
    }
}

/// The child `m` asks for, its descriptors read to the last: its pid. It is reaped when
/// asked (REAP), never before, so its pid stays its own until then.
fn made(conn: &UnixStream, m: Message) -> io::Result<Vec<u8>> {
    let request = decode(&m.payload).ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let mut fds = m.fds;
    while fds.len() < request.targets.len() {
        match recv(conn)? {
            Some(more) if more.kind == MORE && !more.fds.is_empty() => fds.extend(more.fds),
            _ => return Err(io::Error::from(io::ErrorKind::InvalidData)),
        }
    }
    if fds.len() != request.targets.len() {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let given: Vec<(BorrowedFd<'_>, RawFd)> = fds
        .iter()
        .map(AsFd::as_fd)
        .zip(request.targets.iter().copied())
        .collect();
    let args: Vec<&OsStr> = request.args.iter().map(OsString::as_os_str).collect();
    let child = crate::unix::spawn_here(&request.program, &args, &given, request.detach, request.env)?;
    Ok(child.id().to_be_bytes().to_vec())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::fs::File;
    use std::io::Read;
    use std::net::TcpListener;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    /// Set for the tests a test runs in a process of their own, which has a spawner.
    const WITH_SPAWNER: &str = "SHARDS_SPAWNER_TEST";

    /// Runs test `name` in a process of its own, this test binary, with a spawner: one
    /// process-wide, which a run of the tests would share between its tests, and other
    /// tests spawn here, holding what this process lets go of.
    fn in_own_process(name: &str) {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
            .env(WITH_SPAWNER, "1")
            .output()
            .unwrap();
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{name}:\n{stdout}\n{stderr}"
        );
    }

    /// Makes this process's spawner as the daemon does, this test binary running
    /// [`spawner`]: false in a run of the tests, where the tests that ask do nothing.
    fn start() -> bool {
        if std::env::var_os(WITH_SPAWNER).is_none() {
            return false;
        }
        let exe = std::env::current_exe().unwrap();
        let args = [
            "spawner::tests::spawner",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]
        .map(OsStr::new);
        start_spawner(&exe, &args).unwrap();
        true
    }

    /// The spawner [`start`] makes, its first connection at descriptor 3.
    #[test]
    #[ignore = "the spawner the tests here make, run by them"]
    fn spawner() {
        // SAFETY: an all-zero stat is valid; fstat(2) fills it for descriptor 3.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::fstat(3, &mut stat) } != 0 || stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
            return;
        }
        serve_spawner().unwrap();
    }

    /// A spawner's children are made as this process makes its own: what they are given,
    /// at the numbers given, more than a message carries; their environment; their
    /// statuses, signals and sessions. And the spawner is their parent.
    #[test]
    fn a_spawner_makes_children_as_this_process_would() {
        in_own_process("spawner::tests::children");
    }

    #[test]
    #[ignore = "run by a_spawner_makes_children_as_this_process_would, with a spawner"]
    fn children() {
        if !start() {
            return;
        }
        use crate::unix::tests as own;
        own::children_get_the_descriptors_they_are_given();
        own::children_get_swapped_descriptors_each_where_it_belongs();
        own::a_descriptor_given_its_own_number_reaches_the_child();
        #[cfg(target_vendor = "apple")]
        own::children_get_nothing_else();
        own::a_child_spawned_in_an_environment_has_it_alone();
        own::statuses_and_signals();
        own::detached_children_lead_their_own_session();
        own::a_reaped_child_is_never_signalled();
        let spawner = current().as_ref().unwrap().process.id();
        let (mut r, w) = std::io::pipe().unwrap();
        let child = crate::spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), "echo $PPID >&3".as_ref()],
            &[(w.as_fd(), 3)],
            false,
        )
        .unwrap();
        drop(w);
        assert_eq!(child.wait().unwrap(), 0);
        let mut parent = String::new();
        r.read_to_string(&mut parent).unwrap();
        assert_eq!(
            parent.trim(),
            spawner.to_string(),
            "made here, not by the spawner"
        );
        // More descriptors than a message carries, each at its number. Through /dev/fd:
        // dash takes no descriptor above 9 in `>&N`.
        let pipes: Vec<_> = (0..MAX_FDS + 3).map(|_| std::io::pipe().unwrap()).collect();
        let given: Vec<_> = (3..).zip(&pipes).map(|(n, (_, w))| (w.as_fd(), n)).collect();
        let script: Vec<String> = (3..3 + pipes.len())
            .map(|n| format!("echo {n} >/dev/fd/{n}"))
            .collect();
        let child = crate::spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), script.join("; ").as_ref()],
            &given,
            false,
        )
        .unwrap();
        drop(given);
        let readers: Vec<_> = pipes.into_iter().map(|(r, _)| r).collect();
        assert_eq!(child.wait().unwrap(), 0);
        for (n, mut r) in (3..).zip(readers) {
            let mut said = String::new();
            r.read_to_string(&mut said).unwrap();
            assert_eq!(said, format!("{n}\n"));
        }
        // Running, then ended, and reaped once.
        let child = crate::spawn(Path::new("/bin/sleep"), &["30".as_ref()], &[], false).unwrap();
        assert_eq!(child.try_wait(), None);
        child.kill(libc::SIGKILL).unwrap();
        assert_eq!(child.ended_status().unwrap(), 128 + libc::SIGKILL);
        assert_eq!(child.try_wait(), Some(128 + libc::SIGKILL));
    }

    /// A spawner that has gone is made again for the next child; what it made is init's,
    /// and signalled no more, its pid perhaps another process's.
    #[test]
    fn a_spawner_that_has_gone_is_made_again() {
        in_own_process("spawner::tests::gone");
    }

    #[test]
    #[ignore = "run by a_spawner_that_has_gone_is_made_again, with a spawner"]
    fn gone() {
        if !start() {
            return;
        }
        let orphan = crate::spawn(Path::new("/bin/sleep"), &["5".as_ref()], &[], false).unwrap();
        let first = {
            let held = current();
            let s = held.as_ref().unwrap();
            // SAFETY: kill(2) of the spawner, this process's own child, not reaped.
            let killed = unsafe { libc::kill(s.process.id() as libc::pid_t, libc::SIGKILL) };
            assert_eq!(killed, 0);
            s.process.ended().unwrap();
            s.process.id()
        };
        let child = crate::spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), "exit 3".as_ref()],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(child.wait().unwrap(), 3);
        assert_ne!(current().as_ref().unwrap().process.id(), first);
        assert!(orphan.kill(libc::SIGKILL).is_err());
        assert!(orphan.wait().is_err());
    }

    /// What this process lets go of while children are made, no child holds (M158): a
    /// listener's port binds again at once, and a pipe's reader has its end at once, while
    /// four threads have children made as fast as they can.
    #[test]
    fn what_this_process_lets_go_of_no_child_holds() {
        in_own_process("spawner::tests::let_go");
    }

    #[test]
    #[ignore = "run by what_this_process_lets_go_of_no_child_holds, with a spawner"]
    fn let_go() {
        if !start() {
            return;
        }
        let truth = ["/usr/bin/true", "/bin/true"]
            .map(Path::new)
            .into_iter()
            .find(|p| p.exists())
            .unwrap();
        let null = File::open("/dev/null").unwrap();
        let (stop, spawned) = (AtomicBool::new(false), AtomicU64::new(0));
        let (mut rounds, mut listeners, mut pipes) = (0u64, 0u64, 0u64);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let child = crate::spawn(truth, &[], &[(null.as_fd(), 3)], false).unwrap();
                        child.wait().unwrap();
                        spawned.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            let port = TcpListener::bind("0.0.0.0:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let end = Instant::now() + Duration::from_secs(2);
            while Instant::now() < end {
                rounds += 1;
                if TcpListener::bind(("0.0.0.0", port)).is_err() {
                    listeners += 1;
                    while TcpListener::bind(("0.0.0.0", port)).is_err() {}
                }
                let (mut r, w) = std::io::pipe().unwrap();
                // SAFETY: fcntl(2) on a descriptor we own.
                unsafe {
                    let flags = libc::fcntl(r.as_raw_fd(), libc::F_GETFL);
                    libc::fcntl(r.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
                drop(w);
                if !matches!(r.read(&mut [0u8]), Ok(0)) {
                    pipes += 1;
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
        let spawned = spawned.into_inner();
        assert!(spawned >= 100, "{spawned} children made: too few to tell");
        assert_eq!(
            (listeners, pipes),
            (0, 0),
            "listeners and pipes held, of {rounds} each let go of while {spawned} children were made"
        );
    }
}

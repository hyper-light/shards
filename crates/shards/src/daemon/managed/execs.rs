//! A run's exec host (D238, plan step 11): each exec its agent-control endpoint hands it
//! ([`exec::Start`]), charged to the run's budget, started in its member through the run's VM
//! process as `shards exec --for` starts one (`EXEC_RUN` with `in-member=`, D243) and told its
//! start ([`shards_ipc::EXEC_TOLD`]), its stdio carried between step 5's streams and the VM
//! process's pipes, its steering passed on, and how it went said.
//!
//! - **As whom.** As its member runs, so an ask whose spec sets what a member's exec does not
//!   take (a user, privileges, the run's own settings) is refused before anything is made
//!   ([`member_spec`]); its member refuses again what it has not.
//! - **What it costs.** What it holds while it runs ([`cost`]): its streams' records and its
//!   pumps' buffers, the windows its lanes add to the connection ([`exec::lane_bytes`]), and
//!   the kernel's queues of its connection and pipes, each measured or bounded by its
//!   kernel's source ([`pipe_bytes`]). The run's budget has room for it or it is refused
//!   `Capacity`.
//! - **Its end.** Its exit is told once its outputs have ended, every byte its VM process
//!   wrote taken. Its streams' controls are held until its steering closes, as a control
//!   dropped revokes its stream ([`exec::Start::steer`]). Its steering closed before its end,
//!   its controller's connection gone or its lane stopped, hangs it up: SIGHUP, as a
//!   terminal's hangup tells its session (POSIX.1-2024, General Terminal Interface, "Modem
//!   Disconnect"), its output let go of and its stdin ended.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use shards_abi::run::{self, Spec, managed::InputEnd, members};
use shards_ipc::Stream;
use shards_ipc::fd::{AsFd as _, AsRawFd, BorrowedFd};
use shards_remote::control::exec::{self, Replies, Reply, Start, Steer, Unstarted};
use shards_remote::managed::{Producer, input};

use super::{Admission, Charge, ipc_buffers, log};

/// Linux's SIGHUP, which the guest's init gives the exec (include/uapi/asm-generic/signal.h).
const SIGHUP: u32 = 1;
/// Linux's EPIPE and EIO (include/uapi/asm-generic/errno-base.h): how an exec's stdin ended
/// where its pipe refused a write, its reader gone or any other failure.
const EPIPE: i32 = 32;
const EIO: i32 = 5;
/// The most an xnu pipe's buffer grows to, which macOS says of no pipe: 64 KiB
/// (`BIG_PIPE_SIZE`, xnu-12377.101.15 bsd/sys/pipe.h l. 80, as synced.rs cites it).
#[cfg(target_os = "macos")]
const XNU_PIPE_MOST: u64 = 64 << 10;

/// How the host starts an exec in its run's microVM: its VM process handed the member's exec,
/// its `EXEC_RUN` flags, its stdio and its client's end of its connection
/// (`Daemon::launch_exec`).
pub(super) type Launch<'a> =
    dyn Fn(&Spec, &[String], u8, [BorrowedFd<'_>; 3], &Stream) -> io::Result<()> + Sync + 'a;

/// Hosts each exec `starts` brings, on a thread of its own, until the endpoint that brings them
/// has ended and so has every exec it brought: each charged to `charge`, its streams admitted
/// as `admissions` says, started through `launch`.
pub(super) fn host(
    starts: &mpsc::Receiver<Start>,
    admissions: exec::Admissions,
    charge: &Charge,
    launch: &Launch<'_>,
) {
    std::thread::scope(|scope| {
        while let Ok(start) = starts.recv() {
            let (number, replies) = (start.exec, start.replies.try_clone());
            let spawned = std::thread::Builder::new()
                .name("agent-exec".into())
                .spawn_scoped(scope, move || serve(start, admissions, charge, launch));
            if let Err(e) = spawned {
                log(format!("agent-control exec {number}: its thread: {e}"));
                if let Ok(replies) = replies {
                    replies.tell(Reply::Unstarted(Unstarted::Failed));
                }
            }
        }
    });
}

/// The spec of step 6's member exec that an ask of `spec` in `member` is (D243): its command,
/// environment, working directory, terminal, stdin and terminal types as its controller gave
/// them, run in its member (`in-member=`); none where it sets anything else, its user and
/// privileges being its member's and the rest its run's.
fn member_spec(member: u32, spec: Spec) -> Option<Spec> {
    let Spec {
        argv,
        env,
        cwd,
        user,
        hostname,
        tty,
        resolv,
        stdin,
        builtin,
        hosts,
        domainname,
        cgroup,
        setup,
        terms,
    } = spec;
    let its_members = user.is_empty()
        && hostname.is_empty()
        && resolv.is_none()
        && builtin == 0
        && hosts.is_empty()
        && domainname.is_empty()
        && cgroup.is_empty()
        && setup.is_empty();
    its_members.then(|| Spec {
        argv,
        env,
        cwd,
        tty,
        stdin,
        terms,
        setup: vec![members::exec_in(member)],
        ..Spec::default()
    })
}

/// The host's ends of an exec's connection and pipes.
struct Ours {
    conn: Stream,
    stdout: io::PipeReader,
    stderr: Option<io::PipeReader>,
    stdin: Option<io::PipeWriter>,
}

/// What an exec's VM process is handed: its client's end of the connection, the pipes' other
/// ends, the null device for a stdin it reads none of. Dropped once handed, so that the pipes
/// end as the VM process lets its ends go.
struct Theirs {
    conn: Stream,
    stdin: TheirStdin,
    stdout: io::PipeWriter,
    stderr: Option<io::PipeWriter>,
}

enum TheirStdin {
    Pipe(io::PipeReader),
    Null(File),
}

impl Theirs {
    /// Its stdin, stdout and stderr, as `EXEC_RUN` takes them: a terminal's errors are its
    /// output.
    fn stdio(&self) -> [BorrowedFd<'_>; 3] {
        let stdin = match &self.stdin {
            TheirStdin::Pipe(pipe) => pipe.as_fd(),
            TheirStdin::Null(null) => null.as_fd(),
        };
        let stdout = self.stdout.as_fd();
        let stderr = self.stderr.as_ref().map_or(stdout, |pipe| pipe.as_fd());
        [stdin, stdout, stderr]
    }
}

/// An exec's connection and pipes: a stderr pipe where it has a stderr (no terminal), and a
/// stdin pipe where it reads its stdin.
fn made(stderr: bool, stdin: bool) -> io::Result<(Ours, Theirs)> {
    let (conn, their_conn) = Stream::pair()?;
    let (stdout, their_stdout) = io::pipe()?;
    let (stderr, their_stderr) = match stderr {
        true => {
            let (ours, theirs) = io::pipe()?;
            (Some(ours), Some(theirs))
        }
        false => (None, None),
    };
    let (stdin, their_stdin) = match stdin {
        true => {
            let (theirs, ours) = io::pipe()?;
            (Some(ours), TheirStdin::Pipe(theirs))
        }
        false => (None, TheirStdin::Null(File::open(crate::os::NULL)?)),
    };
    Ok((
        Ours {
            conn,
            stdout,
            stderr,
            stdin,
        },
        Theirs {
            conn: their_conn,
            stdin: their_stdin,
            stdout: their_stdout,
            stderr: their_stderr,
        },
    ))
}

/// What an exec holds while it runs, as its charge counts it: each output's record and its
/// pump's buffer of the same size; its stdin's record; its lanes' windows at the endpoint,
/// `lanes`; the kernel's queues of its connection, both ends, and of each pipe; the host's
/// record of it.
fn cost(
    ours: &Ours,
    theirs: &Theirs,
    outputs: &[&Producer],
    stdin: Option<&input::Control>,
    lanes: u64,
) -> io::Result<u64> {
    let overflow = || io::Error::other("an exec's charge overflows");
    let host = u64::try_from(size_of::<Start>()).map_err(io::Error::other)?;
    let mut sum = lanes.checked_add(host).ok_or_else(overflow)?;
    for producer in outputs {
        let record = u64::try_from(producer.storage_capacity()).map_err(io::Error::other)?;
        sum = sum
            .checked_add(record)
            .and_then(|s| s.checked_add(record))
            .ok_or_else(overflow)?;
    }
    if let Some(stdin) = stdin {
        sum = sum.checked_add(u64::from(stdin.room())).ok_or_else(overflow)?;
    }
    let pipes = [
        Some(pipe_bytes(&ours.stdout)?),
        ours.stderr.as_ref().map(pipe_bytes).transpose()?,
        ours.stdin.as_ref().map(pipe_bytes).transpose()?,
    ];
    let connection = [ipc_buffers(&ours.conn)?, ipc_buffers(&theirs.conn)?];
    pipes
        .into_iter()
        .flatten()
        .chain(connection)
        .try_fold(sum, u64::checked_add)
        .ok_or_else(overflow)
}

/// The most a pipe's kernel buffer holds: Linux's capacity as its kernel says it
/// (`F_GETPIPE_SZ`, fcntl(2)); macOS's most, which it says of no pipe ([`XNU_PIPE_MOST`]);
/// Windows' in and out buffers as it says them (GetNamedPipeInfo: std's pipes are named pipes
/// of their own).
fn pipe_bytes(pipe: &impl AsRawFd) -> io::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: fcntl(2) of a pipe this process holds open, which touches no memory of ours.
        let size = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETPIPE_SZ) };
        u64::try_from(size).map_err(|_| io::Error::last_os_error())
    }
    #[cfg(target_os = "macos")]
    {
        let _ = pipe.as_raw_fd();
        Ok(XNU_PIPE_MOST)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Pipes::GetNamedPipeInfo;
        let (mut input, mut output) = (0u32, 0u32);
        // SAFETY: a pipe this process holds open, and outputs of the sizes asked, no flags.
        let rc = unsafe {
            GetNamedPipeInfo(
                pipe.as_raw_fd(),
                std::ptr::null_mut(),
                &raw mut output,
                &raw mut input,
                std::ptr::null_mut(),
            )
        };
        if rc == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(u64::from(input) + u64::from(output))
    }
}

/// The run's budget, charged for one exec while it runs and given back as it drops.
struct Held<'a> {
    charge: &'a Charge,
    bytes: u64,
}

impl<'a> Held<'a> {
    fn new(charge: &'a Charge, bytes: u64) -> Result<Held<'a>, Admission> {
        charge.grow(bytes)?;
        Ok(Held { charge, bytes })
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let _ = self.charge.release(self.bytes);
    }
}

/// A zeroed buffer of `len` bytes, or none where it cannot be allocated.
fn buffer(len: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).ok()?;
    bytes.resize(len, 0);
    Some(bytes)
}

/// How an exec went, as its VM process said it.
enum End {
    /// It started and exited with this status.
    Exited(i32),
    /// It never started, for this reason, its guest's words with it.
    Unstarted(Unstarted, Vec<u8>),
    /// It started, and its connection ended before its exit: its VM process is gone.
    Lost,
}

/// What an exec that never started tells its controller, by the run protocol's class of its
/// guest's failure: its member's refusal or its daemon's (D243, D115) is `Refused`; what its
/// runtime could not start, or a guest never asked, `Failed`.
fn unstarted(class: Option<u8>) -> Unstarted {
    match class {
        Some(run::exec_failed::MEMBER | run::exec_failed::DAEMON) => Unstarted::Refused,
        _ => Unstarted::Failed,
    }
}

/// Reads the exec's connection to its end, its start told as it comes (`EXEC_TOLD`).
fn hear(conn: &Stream, replies: &Replies) -> End {
    let mut started = false;
    let mut why: Option<(u8, Vec<u8>)> = None;
    while let Ok(Some(said)) = shards_ipc::recv(conn) {
        match said.kind {
            shards_ipc::kind::STARTED if !started => {
                started = true;
                replies.tell(Reply::Started);
            }
            shards_ipc::kind::EXEC_UNSTARTED => {
                why = said
                    .payload
                    .split_first()
                    .map(|(class, words)| (*class, words.to_vec()));
            }
            shards_ipc::kind::EXIT => {
                return match (started, said.payload.first()) {
                    (true, Some(status)) => End::Exited(i32::from(*status)),
                    (true, None) => End::Lost,
                    (false, _) => {
                        let class = why.as_ref().map(|(class, _)| *class);
                        End::Unstarted(unstarted(class), why.map(|(_, words)| words).unwrap_or_default())
                    }
                };
            }
            _ => {}
        }
    }
    match started {
        true => End::Lost,
        false => End::Unstarted(Unstarted::Failed, Vec::new()),
    }
}

/// Carries an exec's output from `pipe` to its stream as it comes, a read a record, each once
/// its producer has room: until the VM process's end of the pipe closes, or its stream is let
/// go. Read into `buffer` first, so that a record is taken only for bytes there are: one taken
/// and never submitted would keep its stream from finishing.
fn pump(producer: &Producer, mut pipe: io::PipeReader, buffer: &mut [u8]) {
    loop {
        let n = match pipe.read(buffer) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let Some(bytes) = buffer.get(..n) else {
            return;
        };
        if producer.wait_capacity().is_err() {
            return;
        }
        let Ok(Some(mut record)) = producer.take_buffer() else {
            return;
        };
        record.clear();
        record.extend_from_slice(bytes);
        if producer.submit(record).is_err() {
            return;
        }
    }
}

/// Writes the controller's stdin to the exec's pipe as it comes, each record told taken once
/// the pipe has it all, and its end or abort as the pipe's close, told once closed: what the
/// exec reads of what the pipe holds is its own. A write the pipe refuses ends the input,
/// [`EPIPE`] where its reader is gone (the exec's stdin, or its VM process, ended), else
/// [`EIO`].
fn feed(relay: &input::Relay, control: &input::Control, pipe: io::PipeWriter) {
    let mut pipe = Some(pipe);
    let mut written = 0u64;
    loop {
        match relay.next() {
            input::Next::Record { bytes, .. } => {
                let wrote = match pipe.as_mut() {
                    Some(pipe) => pipe.write_all(&bytes),
                    None => Err(io::ErrorKind::BrokenPipe.into()),
                };
                match wrote {
                    Ok(()) => {
                        written = written.saturating_add(bytes.len() as u64);
                        relay.delivered(bytes);
                    }
                    Err(e) => {
                        pipe = None;
                        let errno = if e.kind() == io::ErrorKind::BrokenPipe {
                            EPIPE
                        } else {
                            EIO
                        };
                        control.ended(written, InputEnd::Aborted(errno));
                    }
                }
            }
            input::Next::End => {
                pipe = None;
                control.ended(written, InputEnd::Closed);
            }
            input::Next::Abort(errno) => {
                pipe = None;
                control.ended(written, InputEnd::Aborted(errno));
            }
            input::Next::Gone => return,
        }
    }
}

/// Passes the controller's steering to the exec on its connection as it comes, each told
/// taken once sent; and hangs the exec up where its steering closes (the endpoint has let it
/// go) before it `ended`.
fn steer_on(steer: mpsc::Receiver<Steer>, conn: Stream, replies: Replies, ended: &AtomicBool) {
    while let Ok(taken) = steer.recv() {
        let _ = match taken {
            Steer::Resize(size) => shards_ipc::send(&conn, shards_ipc::kind::RESIZE, &size.encode(), &[]),
            Steer::Signal(signal) => {
                shards_ipc::send(&conn, shards_ipc::kind::SIGNAL, &signal.to_be_bytes(), &[])
            }
        };
        replies.took();
    }
    if !ended.load(Ordering::Acquire) {
        let _ = shards_ipc::send(&conn, shards_ipc::kind::SIGNAL, &SIGHUP.to_be_bytes(), &[]);
    }
}

/// One exec, from its ask to its end: refused, charged, made, started and served, and how it
/// went told.
fn serve(start: Start, admissions: exec::Admissions, charge: &Charge, launch: &Launch<'_>) {
    let Start {
        ask: exec::Ask { member, kind, .. },
        exec: number,
        stdout,
        stderr,
        stdin,
        steer,
        replies,
    } = start;
    let say = |what: &str| log(format!("agent-control exec {number} in member {member}: {what}"));
    let refuse = |why: Unstarted, what: &str| {
        say(what);
        replies.tell(Reply::Unstarted(why));
    };
    let spec = match kind {
        exec::Kind::Exec(spec) => *spec,
        // A member's MCP bridge is made only where its run asks for one (D254), which no run
        // of this daemon's can yet: its member has none to connect to.
        exec::Kind::Mcp => {
            return refuse(
                Unstarted::Refused,
                "an MCP connection: its member has no MCP bridge",
            );
        }
    };
    let argv: Vec<String> = spec
        .argv
        .iter()
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    let Some(spec) = member_spec(member, spec) else {
        return refuse(
            Unstarted::Refused,
            "it sets what a member's command does not take",
        );
    };
    let (ours, theirs) = match made(stderr.is_some(), stdin.is_some()) {
        Ok(made) => made,
        Err(e) => return refuse(Unstarted::Failed, &format!("its stdio: {e}")),
    };
    let outputs: Vec<&Producer> = std::iter::once(&stdout.0)
        .chain(stderr.as_ref().map(|(producer, _)| producer))
        .collect();
    let charged = exec::lane_bytes(admissions, stderr.is_some(), stdin.is_some())
        .ok_or_else(|| io::Error::other("its lanes' windows overflow"))
        .and_then(|lanes| {
            cost(
                &ours,
                &theirs,
                &outputs,
                stdin.as_ref().map(|(_, control)| control),
                lanes,
            )
        });
    let bytes = match charged {
        Ok(bytes) => bytes,
        Err(e) => return refuse(Unstarted::Failed, &format!("its cost: {e}")),
    };
    let _held = match Held::new(charge, bytes) {
        Ok(held) => held,
        Err(e) => return refuse(Unstarted::Capacity, &format!("{bytes} bytes: {e:?}")),
    };
    let Some(mut out_buffer) = buffer(stdout.0.storage_capacity()) else {
        return refuse(Unstarted::Failed, "its output's buffer");
    };
    let mut err_buffer = match &stderr {
        Some((producer, _)) => match buffer(producer.storage_capacity()) {
            Some(buffer) => Some(buffer),
            None => return refuse(Unstarted::Failed, "its errors' buffer"),
        },
        None => None,
    };
    let (steered, steering) = match (replies.try_clone(), ours.conn.try_clone()) {
        (Ok(steered), Ok(steering)) => (steered, steering),
        _ => return refuse(Unstarted::Failed, "its steering"),
    };
    let flags = shards_ipc::EXEC_TOLD
        | if spec.stdin {
            shards_ipc::EXEC_INTERACTIVE
        } else {
            0
        };
    let ended = AtomicBool::new(false);
    let Ours {
        conn,
        stdout: out_pipe,
        stderr: err_pipe,
        stdin: in_pipe,
    } = ours;
    std::thread::scope(|s| {
        let ended = &ended;
        // Its helpers before it starts, so that nothing it does goes unserved.
        let steer_thread = std::thread::Builder::new()
            .name("exec-steer".into())
            .spawn_scoped(s, move || steer_on(steer, steering, steered, ended));
        let out_pump = std::thread::Builder::new()
            .name("exec-stdout".into())
            .spawn_scoped(s, || pump(&stdout.0, out_pipe, &mut out_buffer));
        let err_pump = match (err_pipe, stderr.as_ref(), err_buffer.as_mut()) {
            (Some(pipe), Some((producer, _)), Some(buffer)) => Some(
                std::thread::Builder::new()
                    .name("exec-stderr".into())
                    .spawn_scoped(s, move || pump(producer, pipe, buffer)),
            ),
            _ => None,
        };
        let feeder = match (in_pipe, stdin.as_ref()) {
            (Some(pipe), Some((relay, control))) => Some(
                std::thread::Builder::new()
                    .name("exec-stdin".into())
                    .spawn_scoped(s, move || feed(relay, control, pipe)),
            ),
            _ => None,
        };
        let spawned = steer_thread.is_ok()
            && out_pump.is_ok()
            && err_pump.as_ref().is_none_or(Result::is_ok)
            && feeder.as_ref().is_none_or(Result::is_ok);
        if !spawned {
            ended.store(true, Ordering::Release);
            return refuse(Unstarted::Failed, "its threads");
        }
        if let Err(e) = launch(&spec, &argv, flags, theirs.stdio(), &theirs.conn) {
            ended.store(true, Ordering::Release);
            return refuse(Unstarted::Failed, &format!("its start: {e}"));
        }
        drop(theirs);
        match hear(&conn, &replies) {
            End::Exited(status) => {
                // Every byte its VM process wrote, taken; its outputs finished; its stdin, where
                // it is still open, ended as no longer read.
                if let Ok(pump) = out_pump {
                    let _ = pump.join();
                }
                if let Some(Ok(pump)) = err_pump {
                    let _ = pump.join();
                }
                let _ = stdout.1.finish();
                if let Some((_, control)) = &stderr {
                    let _ = control.finish();
                }
                if let Some((_, control)) = &stdin
                    && let input::State::Open { at } = control.state()
                {
                    control.ended(at, InputEnd::Aborted(EPIPE));
                }
                ended.store(true, Ordering::Release);
                replies.tell(Reply::Exited(status));
            }
            End::Unstarted(why, words) => {
                ended.store(true, Ordering::Release);
                refuse(why, &format!("not started: {}", String::from_utf8_lossy(&words)));
            }
            End::Lost => {
                ended.store(true, Ordering::Release);
                say("its VM process ended before it did");
                stdout.1.abort();
                if let Some((_, control)) = &stderr {
                    control.abort();
                }
            }
        }
    });
    // Its steering closed, every helper ended: its streams' controls go now.
    drop((stdout, stderr, stdin));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A member's exec takes its command, environment, working directory, terminal, stdin and
    /// terminal types, and runs in its member; a spec that sets anything else is none of a
    /// member's.
    #[test]
    fn a_members_exec_takes_its_command_and_stdio_alone() {
        let asked = Spec {
            argv: vec![b"sh".to_vec(), b"-c".to_vec(), b"printf \xff".to_vec()],
            env: vec![b"A=b".to_vec()],
            cwd: b"/work".to_vec(),
            tty: Some(run::Size { rows: 24, cols: 80 }),
            stdin: true,
            terms: vec![b"xterm-256color".to_vec()],
            ..Spec::default()
        };
        assert_eq!(
            member_spec(3, asked.clone()),
            Some(Spec {
                setup: vec![b"in-member=3".to_vec()],
                ..asked.clone()
            })
        );
        let widened: [fn(&mut Spec); 8] = [
            |s| s.user = b"0".to_vec(),
            |s| s.hostname = b"h".to_vec(),
            |s| s.resolv = Some(Vec::new()),
            |s| s.builtin = run::builtin::NOTICE,
            |s| s.hosts = vec![b"1.2.3.4\th".to_vec()],
            |s| s.domainname = b"d".to_vec(),
            |s| s.cgroup = vec![b"pids.max=1".to_vec()],
            |s| s.setup = vec![b"caps=1".to_vec()],
        ];
        for widen in widened {
            let mut spec = asked.clone();
            widen(&mut spec);
            assert_eq!(member_spec(3, spec.clone()), None, "{spec:?}");
        }
    }

    /// A guest's refusal is typed by its class: its member's or its daemon's `Refused`, its
    /// runtime's, or a guest never asked, `Failed`.
    #[test]
    fn a_guests_refusal_is_typed_by_its_class() {
        assert_eq!(unstarted(Some(run::exec_failed::MEMBER)), Unstarted::Refused);
        assert_eq!(unstarted(Some(run::exec_failed::DAEMON)), Unstarted::Refused);
        assert_eq!(unstarted(Some(run::exec_failed::RUNTIME)), Unstarted::Failed);
        assert_eq!(unstarted(None), Unstarted::Failed);
    }

    /// A pipe's buffer is measured where its kernel says it, and bounded by its source where
    /// it does not: never nothing.
    #[test]
    fn a_pipes_buffer_is_measured_or_bounded() {
        let (reader, writer) = io::pipe().unwrap();
        let bytes = pipe_bytes(&reader).unwrap();
        assert!(bytes > 0);
        assert_eq!(pipe_bytes(&writer).unwrap(), bytes);
    }
}

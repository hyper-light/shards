//! Container commands the daemon runs for its clients (`shards ps`, `wait`, `logs`, `rm`,
//! `stop`, `kill`), answering on the client's stdout and stderr as `docker` answers
//! (docs/design/architecture.md D27). The client has read the command line already
//! (shards_cmdline); the daemon reads it again by the same rules, and does what dockerd
//! would, in the order and with the words the Docker CLI and dockerd use.

use std::io::Read as _;
use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::MutexGuard;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use shards_cmdline::commands::{
    self, COMMIT, CONTAINER_INSPECT, CONTAINER_PRUNE, DIFF, EVENTS, EXPORT, HISTORY, IMAGE_INSPECT,
    IMAGE_PRUNE, IMAGES, INFO, KILL, LOAD, LOGS, PAUSE, PORT, PS, PULL, PUSH, RENAME, RM, RMI, SAVE, STATS,
    STOP, SYSTEM_DF, SYSTEM_PRUNE, TAG, TOP, UNPAUSE, WAIT,
};
use shards_cmdline::flags::{self, Outcome, Parsed};
use shards_cmdline::{gotime, width};
use shards_ipc::kind;

use super::logs::{self, LogFile, Piece, Reader};
use super::{Daemon, RunState, STOP_GRACE, lock};
use crate::containers::{Container, Registry, Removal, State as Life, now};
use crate::spec::{LOG_STDERR, LOG_STDOUT};

/// How long a command may take to end after SIGKILL before its VM goes too, and how long
/// the VM may take then (moby daemon/kill.go, kill).
const KILL_WAIT: Duration = Duration::from_secs(10);
const LAST_WAIT: Duration = Duration::from_secs(2);
/// How long `stop` waits after a signal it could not send (moby daemon/stop.go).
const UNSENT_WAIT: Duration = Duration::from_secs(2);

/// How a container [`end_all`](Daemon::end_all) ended came to its end: it had already,
/// by the signal it was sent, by SIGKILL asked for, by SIGKILL once its grace was up, by
/// its VM killed, or it was not heard to end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum How {
    Already,
    Signal,
    Kill,
    Escalated,
    Vm,
    Lost,
}

/// A stop timeout's seconds as dockerd waits them: Go multiplies them into nanoseconds,
/// wrapping (daemon/stop.go); a negative number waits for ever.
fn grace_of(seconds: i64) -> Option<Duration> {
    (seconds >= 0).then(|| {
        let ns = seconds.wrapping_mul(1_000_000_000);
        Duration::from_nanos(u64::try_from(ns).unwrap_or(0))
    })
}

/// What `stop`, `kill` and `rm` make of one argument before any container is ended: an
/// answer now, or a container to end, with its signal and grace, and what to answer once
/// it has ended (`true`) or would not (`false`).
pub(super) enum Step<'a> {
    Now(Result<bool, String>),
    End {
        id: String,
        linux: u32,
        grace: Option<Duration>,
        then: Then<'a>,
    },
}

/// What a step answers once its container has ended, or would not.
pub(super) type Then<'a> = Box<dyn FnOnce(bool) -> Result<bool, String> + 'a>;

/// The client's end: what a command prints goes there.
pub(super) struct Reply<'a>(pub &'a UnixStream);

impl Reply<'_> {
    pub(super) fn out(&self, line: &str) {
        let _ = self.bytes(LOG_STDOUT, format!("{line}\n").as_bytes());
    }

    /// What a command found, for a client on a colour terminal to lay out
    /// (`kind::SHEET`).
    pub(super) fn sheet(&self, sheet: &shards_ipc::Sheet) {
        let _ = shards_ipc::send(self.0, kind::SHEET, &sheet.encode(), &[]);
    }

    /// [`sheet`](Self::sheet), saying whether the client took it: one that has gone, did not.
    pub(super) fn sheet_taken(&self, sheet: &shards_ipc::Sheet) -> bool {
        shards_ipc::send(self.0, kind::SHEET, &sheet.encode(), &[]).is_ok()
    }

    /// A step of a pull, for a client on a colour terminal to show (`kind::PROGRESS`).
    pub(super) fn progress(&self, event: &shards_ipc::Progress) {
        let _ = shards_ipc::send(self.0, kind::PROGRESS, &event.encode(), &[]);
    }

    pub(super) fn err(&self, line: &str) {
        let _ = self.bytes(LOG_STDERR, format!("{line}\n").as_bytes());
    }

    /// Bytes for the client's stdout (`stream` 1) or stderr, as they are, in messages of
    /// at most what one may carry: a line of any length arrives whole, and the client
    /// writes the pieces as they come (audit A08). An error means the client did not get
    /// them all.
    pub(super) fn bytes(&self, stream: u8, bytes: &[u8]) -> io::Result<()> {
        let which = if stream == LOG_STDERR {
            kind::ERR
        } else {
            kind::OUT
        };
        for piece in bytes.chunks(shards_ipc::MAX_PAYLOAD) {
            shards_ipc::send(self.0, which, piece, &[])?;
        }
        Ok(())
    }
}

/// What `logs` answers when its output did not all reach the client: status 1. A client
/// that hung up is nobody's to tell; anything else is said on stderr, if that still
/// arrives, and in the daemon's log.
fn undelivered(e: &io::Error, reply: &Reply<'_>) -> u8 {
    let hung_up = matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
    );
    if !hung_up {
        super::log(format!("sending a container's logs: {e}"));
        reply.err(&format!("shards: sending the logs: {e}"));
    }
    1
}

/// Linux's signals by name, as dockerd takes them (moby/sys/signal v0.7.1,
/// signal_linux.go SignalMap), the real-time ones aside.
const SIGNALS: [(&str, i64); 34] = [
    ("ABRT", 6),
    ("ALRM", 14),
    ("BUS", 7),
    ("CHLD", 17),
    ("CLD", 17),
    ("CONT", 18),
    ("FPE", 8),
    ("HUP", 1),
    ("ILL", 4),
    ("INT", 2),
    ("IO", 29),
    ("IOT", 6),
    ("KILL", 9),
    ("PIPE", 13),
    ("POLL", 29),
    ("PROF", 27),
    ("PWR", 30),
    ("QUIT", 3),
    ("SEGV", 11),
    ("STKFLT", 16),
    ("STOP", 19),
    ("SYS", 31),
    ("TERM", 15),
    ("TRAP", 5),
    ("TSTP", 20),
    ("TTIN", 21),
    ("TTOU", 22),
    ("URG", 23),
    ("USR1", 10),
    ("USR2", 12),
    ("VTALRM", 26),
    ("WINCH", 28),
    ("XCPU", 24),
    ("XFSZ", 25),
];
/// The real-time signals, `RTMIN` to `RTMAX`, as signal_linux.go numbers them.
const RTMIN: i64 = 34;
const RTMAX: i64 = 64;

/// A signal as dockerd reads it (moby/sys/signal ParseSignal): a number other than 0, or a
/// name in any case, with or without `SIG`; `RTMIN+n` and `RTMAX-n` for n up to 15 and 14.
pub(super) fn parse_signal(given: &str) -> Result<i64, String> {
    let invalid = || format!("invalid signal: {given}");
    if let Some(n) = atoi(given) {
        return if n == 0 { Err(invalid()) } else { Ok(n) };
    }
    let upper = given.to_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    if let Some(&(_, n)) = SIGNALS.iter().find(|(s, _)| *s == name) {
        return Ok(n);
    }
    let real_time = match name {
        "RTMIN" => Some(RTMIN),
        "RTMAX" => Some(RTMAX),
        _ => name
            .strip_prefix("RTMIN+")
            .and_then(|k| decimal(k).filter(|k| (1..=15).contains(k)))
            .map(|k| RTMIN + k)
            .or_else(|| {
                name.strip_prefix("RTMAX-")
                    .and_then(|k| decimal(k).filter(|k| (1..=14).contains(k)))
                    .map(|k| RTMAX - k)
            }),
    };
    real_time.ok_or_else(invalid)
}

/// `k` if it is written as Go writes the number `k`: `RTMIN+01` is no signal's name.
fn decimal(k: &str) -> Option<i64> {
    k.parse::<i64>().ok().filter(|n| n.to_string() == k)
}

/// Go's `strconv.Atoi`: decimal digits after an optional sign, in range.
fn atoi(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.strip_prefix('+').unwrap_or(s).parse().ok()
}

/// Whether dockerd on Linux takes `n` for a signal (moby/sys/signal
/// ValidSignalForPlatform): it names one in the map above.
fn linux_signal(n: i64) -> Option<u32> {
    let known = SIGNALS.iter().any(|&(_, s)| s == n) || (RTMIN..=RTMAX).contains(&n);
    known.then(|| u32::try_from(n).ok()).flatten()
}

impl<D: crate::containers::Disk> Daemon<D> {
    /// `shards exec`: runs a command in a running container, as `docker exec` does. Its
    /// spec starts from the container's run, as dockerd composes an exec's (moby
    /// daemon/exec.go, ContainerExecCreate): the run's environment then `-e`'s, its user
    /// and working directory unless `-u` and `-w` say otherwise, its host name, and TERM
    /// for a terminal of its own. It goes to the container's VM with the client's
    /// connection and stdio, and the VM answers the client. What the daemon refuses, it
    /// says on the client's stderr, then `EXIT` 1.
    pub(super) fn exec(&self, message: &shards_ipc::Message, conn: &UnixStream) {
        let stderr = message
            .fds
            .get(2)
            .and_then(|fd| fd.try_clone().ok())
            .map(std::fs::File::from);
        let refuse = |why: &str| {
            if let Some(mut err) = stderr.as_ref() {
                let _ = writeln!(err, "{why}");
            }
            let _ = shards_ipc::send(conn, kind::EXIT, &[1], &[]);
        };
        let Some(exec) = shards_ipc::Exec::decode(&message.payload) else {
            return refuse("shards: a malformed exec");
        };
        let [stdin, stdout, stderr] = match <&[std::os::fd::OwnedFd; 3]>::try_from(message.fds.as_slice()) {
            Ok(fds) => fds,
            Err(_) => return refuse("shards: an exec without the client's stdio"),
        };
        let id = match self.resolve(&exec.container) {
            Ok(id) => id,
            Err(e) => return refuse(&e),
        };
        // As the CLI refuses it once its inspect has found the container, before the exec
        // is made, and not with -d, which attaches nothing (docker/cli exec.go RunExec,
        // streams/in.go CheckTty).
        if exec.tty.is_some() && exec.interactive && !exec.detach && !exec.stdin_terminal {
            return refuse("cannot attach stdin to a TTY-enabled container because stdin is not a terminal");
        }
        // One being started is seen through, as `docker exec` finds it started or not.
        self.await_start(&id);
        // What the exec needs of the run, taken out of `runs` before its inbox is locked:
        // its follower holds the inbox while it ends the run, which takes `runs`.
        if lock(&self.paused).contains(&id) {
            return refuse(&format!(
                "Error response from daemon: Container {id} is paused, unpause the container before exec"
            ));
        }
        let (base, socket, inbox) = match lock(&self.runs).get(&id) {
            Some(RunState::Tracked(run)) => (run.base.clone(), run.socket.clone(), run.inbox.clone()),
            _ => {
                return refuse(&format!(
                    "Error response from daemon: container {id} is not running"
                ));
            }
        };
        let base = &base.options;
        let options = crate::spec::Options {
            argv: exec.cmd,
            env: base.env.clone(),
            exec_env: exec.env,
            workdir: if exec.workdir.is_empty() {
                base.workdir.clone()
            } else {
                exec.workdir
            },
            user: if exec.user.is_empty() {
                base.user.clone()
            } else {
                exec.user
            },
            hostname: base.hostname.clone(),
            interactive: exec.interactive,
            tty: exec.tty.map(|(rows, cols)| shards_abi::run::Size { rows, cols }),
        };
        let mut spec = match crate::spec::spec(&options, |_| None) {
            Ok(spec) => spec,
            Err(e) => return refuse(&format!("Error response from daemon: {e}")),
        };
        // `--privileged`: every capability (moby daemon/exec_linux.go); the rest of its
        // process is the workload's.
        if exec.privileged {
            let all = (1u64 << shards_abi::run::CAP_NAMES.len()) - 1;
            spec.setup = vec![format!("caps={all}").into_bytes()];
        }
        let mut flags = 0;
        if exec.interactive {
            flags |= shards_ipc::EXEC_INTERACTIVE;
        }
        if exec.detach {
            flags |= shards_ipc::EXEC_DETACHED;
        }
        let number = self.next_exec.fetch_add(1, Ordering::Relaxed);
        let mut payload = Vec::with_capacity(9 + spec.encoded_len().unwrap_or(0));
        payload.extend_from_slice(&number.to_be_bytes());
        payload.push(flags);
        spec.encode_into(&mut payload);
        // Held until the VM says it has it (`EXEC_TAKEN`), or its run ends.
        let held = match conn.try_clone() {
            Ok(held) => held,
            Err(e) => {
                return refuse(&format!(
                    "Error response from daemon: holding the connection: {e}"
                ));
            }
        };
        let exec_id = self.exec_events(&id, &options.argv);
        {
            let mut inbox = lock(&inbox);
            inbox.execs_in_flight.push((number, held));
            if let Some(exec_id) = exec_id {
                inbox.exec_ids.push((number, exec_id));
            }
        }
        let fds = [conn.as_fd(), stdin.as_fd(), stdout.as_fd(), stderr.as_fd()];
        if let Err(e) = socket.send(kind::EXEC_RUN, &payload, &fds) {
            let mut held = lock(&inbox);
            held.execs_in_flight.retain(|(n, _)| *n != number);
            held.exec_ids.retain(|(n, _)| *n != number);
            drop(held);
            refuse(&format!(
                "Error response from daemon: the container's microVM: {e}"
            ));
        }
    }

    /// Logs exec `argv`'s creation and start in container `id` as dockerd does (moby
    /// daemon/exec.go: `exec_create: ENTRYPOINT ARGS`, then `exec_start`, each with its
    /// ID), and returns the ID, for its `exec_die`; none if no ID could be made.
    pub(super) fn exec_events(&self, id: &str, argv: &[String]) -> Option<String> {
        let exec_id = crate::containers::new_id().ok()?;
        let (entry, args) = argv.split_first().map_or(("", &[][..]), |(e, a)| (e.as_str(), a));
        let line = format!("{entry} {}", args.join(" "));
        for action in ["exec_create", "exec_start"] {
            self.container_event(id, format!("{action}: {line}"), &[("execID", exec_id.clone())]);
        }
        Some(exec_id)
    }

    /// Runs container command `argv` for a client, answering on `reply`, and returns its
    /// exit status, the client being `asker`.
    pub(super) fn command(&self, argv: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        // `shards cp`'s steps, which its client takes one at a time (cli/cp.rs).
        if let Some((first, rest)) = argv.split_first()
            && first == COPY_STEP
        {
            return self.copy_step(rest, asker, reply);
        }
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let Some((command, path, named)) = commands::find(&words) else {
            reply.err(&format!("shards: no container command in {argv:?}"));
            return 1;
        };
        let rest = argv.get(named..).unwrap_or_default();
        let parsed = match flags::parse(command, path, rest, &flags::value) {
            Outcome::Run(parsed) => parsed,
            // The client answers these itself; this is what it would have said.
            Outcome::Help { .. } => {
                let _ = reply.bytes(LOG_STDOUT, flags::help(command, path, 80).as_bytes());
                return 0;
            }
            Outcome::Fail { text, status, .. } => {
                reply.err(&text);
                return status;
            }
        };
        // What any client has seen of its run, an answer that reads containers includes:
        // `images` and `rmi` read which use an image too.
        if ![&TAG, &IMAGE_INSPECT, &SAVE, &LOAD, &PULL, &PUSH]
            .iter()
            .any(|c| std::ptr::eq(command, *c))
        {
            self.settle();
        }
        if std::ptr::eq(command, &PS) {
            self.ps(&parsed, asker, reply)
        } else if std::ptr::eq(command, &WAIT) {
            self.wait(&parsed.args, asker.client, reply)
        } else if std::ptr::eq(command, &LOGS) {
            self.logs(&parsed, asker, reply)
        } else if std::ptr::eq(command, &RM) {
            self.rm(&parsed, asker.styled(), reply)
        } else if std::ptr::eq(command, &STOP) {
            self.stop(&parsed, asker.styled(), reply)
        } else if std::ptr::eq(command, &KILL) {
            self.kill(&parsed, asker.styled(), reply)
        } else if std::ptr::eq(command, &PORT) {
            self.port(&parsed.args, asker.styled(), reply)
        } else if std::ptr::eq(command, &IMAGES) {
            self.images(&parsed, asker, reply)
        } else if std::ptr::eq(command, &TAG) {
            self.tag(&parsed.args, asker.styled(), reply)
        } else if std::ptr::eq(command, &RMI) {
            self.rmi(&parsed, asker.styled(), &asker.registry_env, reply)
        } else if std::ptr::eq(command, &IMAGE_INSPECT) {
            self.image_inspect(&parsed, asker.styled(), reply)
        } else if std::ptr::eq(command, &CONTAINER_PRUNE) {
            self.prune(false, true, &parsed, asker, reply)
        } else if std::ptr::eq(command, &IMAGE_PRUNE) {
            self.prune(true, false, &parsed, asker, reply)
        } else if std::ptr::eq(command, &SYSTEM_PRUNE) {
            self.prune(true, true, &parsed, asker, reply)
        } else if std::ptr::eq(command, &SYSTEM_DF) {
            self.system_df(&parsed, asker, reply)
        } else if std::ptr::eq(command, &COMMIT) {
            self.commit_image(&parsed, asker, reply)
        } else if std::ptr::eq(command, &EXPORT) {
            self.export(&parsed.args, asker, reply)
        } else if std::ptr::eq(command, &INFO) {
            self.info(&parsed, asker, reply)
        } else if std::ptr::eq(command, &EVENTS) {
            self.events(&parsed, asker, reply)
        } else if std::ptr::eq(command, &DIFF) {
            self.diff(&parsed.args, asker.styled(), reply)
        } else if std::ptr::eq(command, &TOP) {
            self.top(&parsed, asker, reply)
        } else if std::ptr::eq(command, &PAUSE) {
            self.pause(&parsed, true, asker.styled(), reply)
        } else if std::ptr::eq(command, &UNPAUSE) {
            self.pause(&parsed, false, asker.styled(), reply)
        } else if std::ptr::eq(command, &STATS) {
            self.stats(&parsed, asker, reply)
        } else if std::ptr::eq(command, &RENAME) {
            self.rename(&parsed.args, reply)
        } else if std::ptr::eq(command, &HISTORY) {
            self.history(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::INSPECT) {
            self.inspect_any(&parsed, asker, reply)
        } else if std::ptr::eq(command, &CONTAINER_INSPECT) {
            self.container_inspect(&parsed, asker.styled(), reply)
        } else if std::ptr::eq(command, &SAVE) {
            self.save(&parsed.args, asker, reply)
        } else if std::ptr::eq(command, &LOAD) {
            self.load(asker, reply)
        } else if std::ptr::eq(command, &PULL) {
            self.pull(&parsed, asker, reply)
        } else if std::ptr::eq(command, &PUSH) {
            self.push(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::IMPORT) {
            self.import(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::UPDATE) {
            self.update(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::VOLUME_CREATE) {
            self.volume_create(&parsed, asker.styled(), reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::VOLUME_LS) {
            self.volume_ls(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::VOLUME_INSPECT) {
            self.volume_inspect(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::VOLUME_RM) {
            self.volume_rm(&parsed, asker.styled(), reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::VOLUME_PRUNE) {
            self.volume_prune(&parsed, asker, reply)
        } else {
            reply.err(&format!("shards: {path} is not a container command"));
            1
        }
    }

    /// `shards export CONTAINER` (moby daemon/export.go, ContainerExport): the
    /// microVM's files as a tar archive, written by its init straight to where the
    /// client asked (`-o`'s file or its stdout), through no copy here: a stopped one's in
    /// a VM visiting them (visit.rs).
    fn export(&self, args: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        let (Some(given), Some(out)) = (args.first(), asker.files.first()) else {
            reply.err("shards: export: the client sent nowhere to write");
            return 1;
        };
        let refuse = |said: String| {
            reply.err(&format!("Error response from daemon: {said}"));
            1
        };
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        self.await_start(&id);
        if !self.reachable(&id) {
            return refuse(format!(
                "container {id}: its files cannot be reached (see the daemon's log)"
            ));
        }
        if lock(&self.paused).contains(&id) {
            return refuse(format!(
                "container {id} is paused: its files can be exported once it is unpaused"
            ));
        }
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::EXPORT,
            ..Default::default()
        };
        let pieces = std::fs::File::open("/dev/null").and_then(|null| Ok((null, std::io::pipe()?)));
        let (null, (mut said, into)) = match pieces {
            Ok(p) => p,
            Err(e) => return refuse(format!("Error exporting container {given}: {e}")),
        };
        let ended = self.exec_on(&id, &spec, false, [null.as_fd(), out.as_fd(), into.as_fd()]);
        drop(into);
        let mut why = String::new();
        let _ = said.read_to_string(&mut why);
        match ended {
            Ok(e) if e.status == Some(0) => {
                self.container_event(&id, "export", &[]);
                0
            }
            Ok(_) => refuse(format!("Error exporting container {given}: {}", why.trim())),
            Err(e) => refuse(format!("Error exporting container {given}: {e}")),
        }
    }

    /// A step of `shards cp` in container `container`, as dockerd's archive routes take
    /// them (moby daemon/archive.go): `stat PATH`, its stat as a JSON line; `archive
    /// PATH`, a tar archive of it to the asker's file; `extract PATH UIDGID OVERWRITE`,
    /// the archive the asker's file holds unpacked there, owned by the container's user
    /// with `UIDGID` 1. Done in its microVM (init copy.rs): a stopped one's in a VM
    /// visiting its files (visit.rs), which keeps what was copied in as it ends.
    fn copy_step(&self, args: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        let (Some(step), Some(given), Some(path)) = (args.first(), args.get(1), args.get(2)) else {
            return 1;
        };
        let refuse = |said: String| {
            reply.err(&format!("Error response from daemon: {said}"));
            1
        };
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        self.await_start(&id);
        if !self.reachable(&id) {
            return refuse(format!(
                "container {id}: its files cannot be reached (see the daemon's log)"
            ));
        }
        if lock(&self.paused).contains(&id) {
            return refuse(format!(
                "container {id} is paused: its files can be copied once it is unpaused"
            ));
        }
        use shards_abi::run::builtin;
        let (kind, mut argv) = match step.as_str() {
            "stat" => (builtin::STAT, vec![path.as_bytes().to_vec()]),
            "archive" => (builtin::ARCHIVE, vec![path.as_bytes().to_vec()]),
            "extract" => (builtin::EXTRACT, vec![path.as_bytes().to_vec()]),
            _ => return 1,
        };
        if kind == builtin::EXTRACT {
            // checkWritablePath (daemon/archive_unix.go): a read-only root takes nothing.
            let dir = lock(&self.containers).dir(&id);
            let read_only = std::fs::read(dir.join(super::REQUEST))
                .ok()
                .and_then(|b| shards_ipc::Run::decode(&b))
                .is_some_and(|r| r.read_only);
            if read_only {
                return refuse("container rootfs is marked read-only".into());
            }
            // `-a`: everything the container's user's (archive_tarcopyoptions_unix.go).
            let user = if args.get(3).is_some_and(|a| a == "1") {
                self.container_user(&id)
            } else {
                String::new()
            };
            argv.push(user.into_bytes());
            argv.push(args.get(4).map_or(b"0".to_vec(), |a| a.as_bytes().to_vec()));
        }
        let spec = shards_abi::run::Spec {
            builtin: kind,
            argv,
            ..Default::default()
        };
        let (status, out, said) = if kind == builtin::STAT {
            match self.exec_quietly(&id, &spec, None, 1 << 16, super::TAKE_TIMEOUT) {
                Ok(q) => (q.status, q.output, Vec::new()),
                Err(e) => return refuse(e.to_string()),
            }
        } else {
            let Some(file) = asker.files.first() else {
                return refuse("shards: cp: the client sent no file".into());
            };
            let pieces = std::fs::File::open("/dev/null").and_then(|null| Ok((null, std::io::pipe()?)));
            let (null, (mut errs, into)) = match pieces {
                Ok(p) => p,
                Err(e) => return refuse(e.to_string()),
            };
            let stdio = if kind == builtin::EXTRACT {
                [file.as_fd(), null.as_fd(), into.as_fd()]
            } else {
                [null.as_fd(), file.as_fd(), into.as_fd()]
            };
            let ended = self.exec_on(&id, &spec, kind == builtin::EXTRACT, stdio);
            drop(into);
            let mut said = Vec::new();
            let _ = errs.read_to_end(&mut said);
            match ended {
                Ok(e) => (e.status, Vec::new(), said),
                Err(e) => return refuse(e.to_string()),
            }
        };
        let why = String::from_utf8_lossy(if said.is_empty() { &out } else { &said })
            .trim()
            .to_string();
        match status {
            Some(0) => {
                if kind == builtin::STAT {
                    let _ = reply.bytes(LOG_STDOUT, &out);
                } else {
                    let action = if kind == builtin::ARCHIVE {
                        "archive-path"
                    } else {
                        "extract-to-dir"
                    };
                    self.container_event(&id, action, &[]);
                }
                0
            }
            Some(2) => refuse(format!("Could not find the file {path} in container {given}")),
            Some(3) => refuse(why),
            _ => refuse(if why.is_empty() {
                format!("container {id}: the copy failed")
            } else {
                why
            }),
        }
    }

    /// The user container `id` runs as: its run's, else its image's.
    fn container_user(&self, id: &str) -> String {
        let dir = lock(&self.containers).dir(id);
        let run = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b));
        match run {
            Some(r) if !r.user.is_empty() => r.user,
            _ => lock(&self.containers)
                .get(id)
                .and_then(|c| c.image_id.clone())
                .and_then(|image| {
                    let store = self.store().ok()??;
                    let found = store
                        .images()
                        .ok()?
                        .into_iter()
                        .find(|i| i.id.to_string() == image)?;
                    let config: serde_json::Value = serde_json::from_slice(found.config.as_deref()?).ok()?;
                    config.pointer("/config/User")?.as_str().map(str::to_string)
                })
                .unwrap_or_default(),
        }
    }

    /// `shards diff CONTAINER` (moby daemon/changes.go, ContainerChanges): what the
    /// microVM changed of its image's files, as its init finds them (init changes.rs),
    /// one `KIND PATH` a line as docker/cli prints them (diff.go): a stopped one's in a VM
    /// visiting its files (visit.rs).
    fn diff(&self, args: &[String], styled: bool, reply: &Reply<'_>) -> u8 {
        let Some(given) = args.first() else {
            return 1;
        };
        let refuse = |said: String| {
            reply.err(&format!("Error response from daemon: {said}"));
            1
        };
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        self.await_start(&id);
        if !self.reachable(&id) {
            return refuse(format!(
                "container {id}: its files cannot be reached (see the daemon's log)"
            ));
        }
        if lock(&self.paused).contains(&id) {
            return refuse(format!(
                "container {id} is paused: its changes can be read once it is unpaused"
            ));
        }
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::CHANGES,
            ..Default::default()
        };
        // As they come: a microVM's changes may be many.
        let ended = if styled {
            let mut page = DiffPage::default();
            let mut head = Some(given.clone());
            let ended = self.exec_streamed(&id, &spec, None, None, &mut |chunk| {
                let rows = page.take(chunk);
                let mut sheet = shards_ipc::Sheet::new("diff");
                if let Some(name) = head.take() {
                    sheet.record(&[("kind", "head".into()), ("name", name)]);
                }
                for (kind, path) in rows {
                    sheet.record(&[("change", kind), ("path", path)]);
                }
                reply.sheet_taken(&sheet)
            });
            let mut sheet = shards_ipc::Sheet::new("diff");
            let [a, c, d] = page.counts;
            sheet.record(&[
                ("kind", "end".into()),
                ("added", a.to_string()),
                ("changed", c.to_string()),
                ("deleted", d.to_string()),
            ]);
            if ended.as_ref().is_ok_and(|e| e.status == Some(0)) {
                reply.sheet(&sheet);
            }
            ended
        } else {
            self.exec_streamed(&id, &spec, None, None, &mut |chunk| {
                reply.bytes(LOG_STDOUT, chunk).is_ok()
            })
        };
        match ended {
            Ok(e) if e.status == Some(0) => 0,
            Ok(_) => refuse(format!("container {id}: its changes could not all be read")),
            Err(e) => refuse(format!("container {id}: reading its changes: {e}")),
        }
    }

    /// `shards top CONTAINER [ps OPTIONS]` (moby daemon/top_unix.go, ContainerTop): the
    /// microVM's processes as procps's `ps` lays them out with the options given (`-ef`
    /// if none), from what its init reads of them in `/proc` (daemon/top.rs), printed as
    /// docker/cli prints them (top.go: a tabwriter of minimum width 20, padding 3). Users
    /// and groups are named as the microVM's own `/etc/passwd` and `/etc/group` name
    /// them, where dockerd names them as its host does. A paused microVM cannot be read,
    /// where dockerd's ps reads a frozen cgroup's processes: it says so.
    fn top(&self, parsed: &Parsed, asker: &Asker, reply: &Reply<'_>) -> u8 {
        let Some((given, options)) = parsed.args.split_first() else {
            return 1;
        };
        let refuse = |said: String| {
            reply.err(&format!("Error response from daemon: {said}"));
            1
        };
        // As the client sends them (one query value, joined by spaces) and dockerd splits
        // them again, on each space.
        let joined = options.join(" ");
        let joined = match (joined.is_empty(), asker.styled()) {
            (false, _) => joined,
            // On a colour terminal, more than `-ef` says: each one's state, its share of
            // a CPU, its memory and how long it has run, under its parent.
            (true, true) => "-o pid,ppid,user,stat,%cpu,rss,etime,args".to_string(),
            (true, false) => "-ef".to_string(),
        };
        if let Err(e) = ps_args_allowed(&joined) {
            return refuse(e);
        }
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        self.await_start(&id);
        if !self.running(&id) {
            return refuse(format!("container {id} is not running"));
        }
        if lock(&self.paused).contains(&id) {
            return refuse(format!(
                "container {id} is paused: its processes can be listed once it is unpaused"
            ));
        }
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::PROCESSES,
            ..Default::default()
        };
        let quiet = match self.exec_quietly(&id, &spec, None, TOP_LIMIT, super::TAKE_TIMEOUT) {
            Ok(q) if q.status == Some(0) && !q.more => q,
            Ok(_) => return refuse(format!("container {id}: its processes could not be read")),
            Err(e) => return refuse(format!("container {id}: reading its processes: {e}")),
        };
        let args: Vec<&str> = joined.split(' ').collect();
        let listed =
            super::top::ps(&quiet.output, &args, asker.utc_offset).and_then(|out| super::top::table(&out));
        let table = match listed {
            Ok(t) => t,
            Err(e) => return refuse(e),
        };
        self.container_event(&id, "top", &[]);
        if asker.styled() {
            let mut sheet = shards_ipc::Sheet::new("top");
            sheet.record(&[
                ("kind", "head".into()),
                ("name", given.clone()),
                ("titles", table.titles.join("\t")),
            ]);
            for p in &table.processes {
                sheet.record(&[("fields", p.join("\t"))]);
            }
            reply.sheet(&sheet);
            return 0;
        }
        let rows: Vec<Vec<String>> = std::iter::once(table.titles).chain(table.processes).collect();
        let mut text = String::new();
        for line in tabulate_with(&rows, 20, asker.east_asian) {
            text.push_str(&line);
            text.push('\n');
        }
        let _ = reply.bytes(LOG_STDOUT, text.as_bytes());
        0
    }

    /// `shards stats [CONTAINER...]` (docker/cli cli/command/container/stats.go): what each
    /// microVM takes of the host, measured from its VM process each second: CPU as the
    /// share of one CPU its process had over the second, memory as its resident size
    /// against the microVM's. Streamed until the client goes, or once with `--no-stream`.
    /// What only the guest knows (network and block I/O, PIDs) is `--`, as docker/cli
    /// shows what it lacks.
    fn stats(&self, parsed: &Parsed, asker: &Asker, reply: &Reply<'_>) -> u8 {
        let limit = shards_vmm::vm::MEMORY_MIB.saturating_mul(1 << 20);
        let all = parsed.bool("all");
        let trunc = !parsed.bool("no-trunc");
        // Those named, each found as every command finds one; else every one listed.
        let mut named = Vec::new();
        for given in &parsed.args {
            match self.resolve_held(lock(&self.containers), given).1 {
                Ok(id) => named.push(id),
                Err(said) => {
                    reply.err(&said);
                    return 1;
                }
            }
        }
        // Each one shown: its ID, name and, while it runs, its VM process.
        let targets = || -> Vec<(String, String, Option<u32>)> {
            let runs = lock(&self.runs);
            let mut list: Vec<&Container> = Vec::new();
            let containers = lock(&self.containers);
            for c in containers.all() {
                let running = c.state == Life::Running;
                if (named.is_empty() && (all || running)) || named.contains(&c.id) {
                    list.push(c);
                }
            }
            list.sort_by_key(|c| std::cmp::Reverse(c.created));
            list.iter()
                .map(|c| {
                    let pid = match runs.get(&c.id) {
                        Some(RunState::Tracked(t)) => Some(t.vm.id()),
                        _ => None,
                    };
                    (c.id.clone(), c.name.clone(), pid)
                })
                .collect()
        };
        let mut last: std::collections::HashMap<String, (u64, Instant)> = std::collections::HashMap::new();
        let mut sample = || -> Vec<Sampled> {
            targets()
                .into_iter()
                .map(|(id, name, pid)| {
                    let Some(u) = pid.and_then(|p| shards_vmm::platform::process_usage(p).ok()) else {
                        return Sampled { id, name, used: None };
                    };
                    let now = Instant::now();
                    let cpu = match last.insert(id.clone(), (u.cpu_ns, now)) {
                        Some((before, at)) => {
                            #[allow(clippy::cast_precision_loss)]
                            let share = u.cpu_ns.saturating_sub(before) as f64
                                / now.duration_since(at).as_nanos().max(1) as f64;
                            share * 100.0
                        }
                        None => 0.0,
                    };
                    Sampled {
                        id,
                        name,
                        used: Some((cpu, u.resident)),
                    }
                })
                .collect()
        };
        // The first sample is a baseline: CPU is measured over the second after it.
        sample();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let rows = sample();
            #[allow(clippy::cast_precision_loss)]
            let share = |mem: u64| mem as f64 / limit.max(1) as f64 * 100.0;
            let taken = if asker.styled() {
                let mut sheet = shards_ipc::Sheet::new("stats");
                for Sampled { id, name, used } in &rows {
                    let (cpu, mem) = used.unwrap_or_default();
                    sheet.record(&[
                        ("running", used.is_some().to_string()),
                        ("id", id.get(..12).unwrap_or(id).to_string()),
                        ("name", name.clone()),
                        ("cpu", format!("{cpu:.2}")),
                        ("mem", binary_size(mem)),
                        ("limit", binary_size(limit)),
                        ("share", format!("{:.2}", share(mem))),
                    ]);
                }
                reply.sheet_taken(&sheet)
            } else {
                let mut table = vec![[
                    "CONTAINER ID".to_string(),
                    "NAME".into(),
                    "CPU %".into(),
                    "MEM USAGE / LIMIT".into(),
                    "MEM %".into(),
                    "NET I/O".into(),
                    "BLOCK I/O".into(),
                    "PIDS".into(),
                ]];
                for Sampled { id, name, used } in &rows {
                    let (cpu, mem) = used.unwrap_or_default();
                    table.push([
                        if trunc {
                            id.get(..12).unwrap_or(id).to_string()
                        } else {
                            id.clone()
                        },
                        name.clone(),
                        format!("{cpu:.2}%"),
                        format!("{} / {}", binary_size(mem), binary_size(limit)),
                        format!("{:.2}%", share(mem)),
                        "--".into(),
                        "--".into(),
                        "--".into(),
                    ]);
                }
                let mut text = String::new();
                // On a terminal each table replaces the last, as docker/cli clears it.
                if asker.terminal && !parsed.bool("no-stream") {
                    text.push_str("\x1b[2J\x1b[H");
                }
                for line in tabulate(&table, asker.east_asian) {
                    text.push_str(&line);
                    text.push('\n');
                }
                reply.bytes(LOG_STDOUT, text.as_bytes()).is_ok()
            };
            if !taken || parsed.bool("no-stream") {
                return 0;
            }
        }
    }

    /// `shards rename CONTAINER NEW_NAME` (moby daemon/rename.go, ContainerRename): the
    /// new name checked as a new container's is, refused if it is the old one or held; the
    /// old one free at once; the record written soon.
    fn rename(&self, args: &[String], reply: &Reply<'_>) -> u8 {
        let (Some(given), Some(new)) = (args.first(), args.get(1)) else {
            return 1;
        };
        let refuse = |said: String| {
            reply.err(&format!("Error response from daemon: {said}"));
            1
        };
        if !crate::containers::valid_name(new) {
            return refuse(format!(
                "Invalid container name ({new}), only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"
            ));
        }
        let new = new.strip_prefix('/').unwrap_or(new).to_string();
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => {
                return refuse(
                    e.strip_prefix("Error response from daemon: ")
                        .unwrap_or(&e)
                        .to_string(),
                );
            }
        };
        let old = {
            let mut registry = lock(&self.containers);
            let Some(old) = registry.get(&id).map(|c| c.name.clone()) else {
                return refuse(format!("No such container: {given}"));
            };
            if old == new {
                return refuse("Renaming a container with the same name as its current name".into());
            }
            if let Some(holder) = registry.name_taken(&new) {
                let holder = holder.id.clone();
                return refuse(format!(
                    "Error when allocating new name: Conflict. The container name \"/{new}\" is already in use by container \"{holder}\". You have to remove (or rename) that container to be able to reuse that name."
                ));
            }
            if let Err(e) = registry.rename(&id, &new) {
                return refuse(e.to_string());
            }
            old
        };
        self.record_soon(&id, Vec::new());
        // dockerd's names begin with `/` (rename.go: `oldName = ctr.Name`).
        self.container_event(&id, "rename", &[("oldName", format!("/{old}"))]);
        0
    }

    /// `shards port CONTAINER [PORT]` (docker/cli cli/command/container/port.go): each
    /// published port of a running container as `PORT/PROTO -> HOST:PORT`, or with PORT
    /// the host addresses of that one, in natural order.
    fn port(&self, args: &[String], styled: bool, reply: &Reply<'_>) -> u8 {
        let (Some(reference), wanted) = (args.first(), args.get(1).filter(|p| !p.is_empty())) else {
            return 1;
        };
        let id = match self.resolve(reference) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        let wanted = match wanted.map(|p| shards_cmdline::ports::parse_port(p)).transpose() {
            Ok(wanted) => wanted,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        // dockerd's NetworkSettings.Ports: a running container's bindings.
        let ports = match lock(&self.containers).get(&id) {
            Some(c) if c.state == Life::Running => c.ports.clone(),
            _ => Vec::new(),
        };
        let mut lines: Vec<String> = ports
            .iter()
            .filter_map(|p| {
                let at = std::net::SocketAddr::new(p.ip?, p.public);
                let port = shards_cmdline::ports::Port {
                    number: p.private,
                    proto: p.proto.clone(),
                };
                match &wanted {
                    Some(w) => (*w == port).then(|| at.to_string()),
                    None => Some(format!("{port} -> {at}")),
                }
            })
            .collect();
        if lines.is_empty() {
            if let Some(w) = args.get(1).filter(|_| wanted.is_some()) {
                reply.err(&format!("no public port '{w}' published for {reference}"));
                return 1;
            }
            return 0;
        }
        lines.sort_by(|a, b| shards_cmdline::ports::natural_compare(a, b));
        if styled {
            let mut sheet = shards_ipc::Sheet::new("port");
            sheet.record(&[("kind", "head".into()), ("target", reference.clone())]);
            for line in lines {
                sheet.record(&[("mapping", line)]);
            }
            reply.sheet(&sheet);
            return 0;
        }
        for line in lines {
            reply.out(&line);
        }
        0
    }

    /// The ID of the container `reference` names: all of its ID, its name, or the start of
    /// its ID and of no other's (moby daemon/container.go, GetContainer).
    /// `args`, each name of no microVM but of an image some run from said as those
    /// microVMs' names: `shards stop ubuntu:latest` stops what runs from it. With
    /// `running`, only the running ones. A name of a microVM is itself.
    fn by_image(&self, args: &[String], running: bool) -> Vec<String> {
        use shards_image::reference::Reference;
        let familiar = |name: &str| {
            Reference::parse_normalized(name)
                .map(|r| r.tag_name_only().familiar())
                .unwrap_or_else(|_| name.to_string())
        };
        let mut out = Vec::with_capacity(args.len());
        for arg in args {
            if self.resolve(arg).is_ok() {
                out.push(arg.clone());
                continue;
            }
            let wanted = familiar(arg.trim());
            let mut runs: Vec<(u128, String)> = lock(&self.containers)
                .all()
                .filter(|c| !running || c.state == Life::Running)
                .filter(|c| familiar(&c.image) == wanted)
                .map(|c| (c.created, c.name.clone()))
                .collect();
            if runs.is_empty() {
                // Neither: it fails as a name, in dockerd's words.
                out.push(arg.clone());
                continue;
            }
            runs.sort();
            out.extend(runs.into_iter().map(|(_, name)| name));
        }
        out
    }

    pub(super) fn resolve(&self, reference: &str) -> Result<String, String> {
        self.resolve_held(lock(&self.containers), reference).1
    }

    /// [`resolve`](Self::resolve) in `registry`, handed back with the answer, which holds
    /// for as long as the caller holds it.
    pub(super) fn resolve_held<'a>(
        &'a self,
        mut registry: MutexGuard<'a, Registry>,
        reference: &str,
    ) -> (MutexGuard<'a, Registry>, Result<String, String>) {
        // As the Docker CLI's client sends a reference (moby client utils.go, trimID): its
        // spaces trimmed, and none refused.
        let reference = reference.trim();
        if reference.is_empty() {
            return (
                registry,
                Err("invalid container name or ID: value is empty".into()),
            );
        }
        // A container exists from its creation, as dockerd's does: one whose record is
        // still being written is waited for, not missed.
        while registry.arriving_as(reference) {
            registry = self
                .arrived
                .wait(registry)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let found = if registry.get(reference).is_some() {
            Ok(reference.to_string())
        } else if let Some(c) = registry.named(reference.strip_prefix('/').unwrap_or(reference)) {
            Ok(c.id.clone())
        } else {
            let mut matching = registry.id_prefixed(reference);
            match (matching.next(), matching.next()) {
                (Some(_), Some(_)) => Err(format!(
                    "Error response from daemon: multiple IDs found with provided prefix: {reference}"
                )),
                (Some(only), None) => Ok(only.id.clone()),
                (None, _) => Err(format!(
                    "Error response from daemon: No such container: {reference}"
                )),
            }
        };
        (registry, found)
    }

    /// Whether the container with `id` runs: its run was handed over, and has not ended.
    pub(super) fn running(&self, id: &str) -> bool {
        matches!(lock(&self.runs).get(id), Some(RunState::Tracked(t)) if !t.visit)
    }

    /// Whether container `id` has a VM to read its files in: its run's, or a visit's.
    pub(super) fn reachable(&self, id: &str) -> bool {
        matches!(lock(&self.runs).get(id), Some(RunState::Tracked(_)))
    }

    /// Sends Linux signal `linux` to the command of the container with `id`; whether it
    /// could.
    ///
    /// A paused container is let go on once sent SIGKILL or its own stop signal, as
    /// dockerd resumes one (moby daemon/kill.go, killWithSignal): the signal waits in its
    /// socket and is read as it runs again. Other signals wait for `unpause`.
    fn signal(&self, id: &str, linux: u32) -> bool {
        // Sent once the runs' lock is let go of: no send waits under it.
        let socket = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => t.socket.clone(),
            _ => return false,
        };
        let sent = socket.send(kind::SIGNAL, &linux.to_be_bytes(), &[]).is_ok();
        if sent {
            self.container_event(id, "kill", &[("signal", linux.to_string())]);
        }
        if lock(&self.paused).contains(id) {
            let own = lock(&self.containers).made(id).and_then(|c| c.stop_signal);
            if linux == 9 || own.is_none_or(|s| s == i64::from(linux)) {
                self.thaw(id);
            }
        }
        sent
    }

    /// Lets paused container `id`'s VM go on (SIGCONT); whether it could.
    fn thaw(&self, id: &str) -> bool {
        let resumed = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => t.vm.kill(libc::SIGCONT).is_ok(),
            _ => false,
        };
        lock(&self.paused).remove(id);
        resumed
    }

    /// `shards pause` and `unpause` (moby daemon/pause.go, unpause.go): each running
    /// container's whole microVM frozen where it is, its vCPUs and devices alike, by
    /// stopping its VM process (SIGSTOP), as dockerd freezes a container's cgroup; or let
    /// go on (SIGCONT). A frozen microVM takes no CPU, keeps its memory, and hears what it
    /// is sent once it goes on. Answers as the CLI does (pause.go): each name done, then
    /// the errors.
    fn pause(&self, parsed: &Parsed, freeze: bool, styled: bool, reply: &Reply<'_>) -> u8 {
        let args = self.by_image(&parsed.args, true);
        self.each(
            if freeze { "pause" } else { "unpause" },
            styled,
            &args,
            &|reference| {
                let id = match self.resolve(reference.trim()) {
                    Ok(id) => id,
                    Err(e) => return Step::Now(Err(e)),
                };
                self.await_start(&id);
                if !self.running(&id) {
                    return Step::Now(Err(format!(
                        "Error response from daemon: container {id} is not running"
                    )));
                }
                let paused = lock(&self.paused).contains(&id);
                if freeze {
                    if paused {
                        return Step::Now(Err(format!(
                            "Error response from daemon: container {id} is already paused"
                        )));
                    }
                    let frozen = match lock(&self.runs).get(&id) {
                        Some(RunState::Tracked(t)) => t.vm.kill(libc::SIGSTOP),
                        _ => Err(io::Error::from(io::ErrorKind::NotFound)),
                    };
                    return Step::Now(match frozen {
                        Ok(()) => {
                            lock(&self.paused).insert(id.clone());
                            self.container_event(&id, "pause", &[]);
                            Ok(true)
                        }
                        Err(e) => Err(format!(
                            "Error response from daemon: cannot pause container {id}: {e}"
                        )),
                    });
                }
                if !paused {
                    return Step::Now(Err(format!(
                        "Error response from daemon: Container {id} is not paused"
                    )));
                }
                Step::Now(if self.thaw(&id) {
                    self.container_event(&id, "unpause", &[]);
                    Ok(true)
                } else {
                    Err(format!(
                        "Error response from daemon: Cannot unpause container {id}: it is not running"
                    ))
                })
            },
            reply,
        )
    }

    /// Runs `op` for each of `args`, in their order, then ends every container the steps
    /// name at once ([`end_all`](Self::end_all)): docker/cli acts on 50 at a time
    /// (cli/command/container/utils.go, parallelOperation), so that stopping 200 took four
    /// graces; here it takes the longest one, with no thread for each. Answers as the CLI
    /// does (stop.go, kill.go, rm.go): each success prints its argument as given, once it
    /// and those before it are done, unless it has nothing to say; the errors follow, one
    /// per line, and make the status 1.
    fn each<'a>(
        &'a self,
        verb: &str,
        styled: bool,
        args: &[String],
        op: &dyn Fn(&str) -> Step<'a>,
        reply: &Reply<'_>,
    ) -> u8 {
        let began = Instant::now();
        // How each ended and when, for a client on a colour terminal.
        let mut hows: Vec<(Option<How>, Duration)> = vec![(None, Duration::ZERO); args.len()];
        let mut results: Vec<Option<Result<bool, String>>> = Vec::with_capacity(args.len());
        let mut thens: Vec<Option<Then<'a>>> = Vec::with_capacity(args.len());
        let mut ending = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            match op(arg) {
                Step::Now(result) => {
                    results.push(Some(result));
                    thens.push(None);
                }
                Step::End {
                    id,
                    linux,
                    grace,
                    then,
                } => {
                    results.push(None);
                    thens.push(Some(then));
                    ending.push((i, id, linux, grace));
                }
            }
        }
        if styled {
            return self.each_shown(verb, args, results, thens, ending, began, &mut hows, reply);
        }
        let mut errors = Vec::new();
        let mut said = 0;
        // Each success, once it and those before it are done.
        let mut say = |results: &mut [Option<Result<bool, String>>], errors: &mut Vec<String>| {
            while let Some(Some(result)) = results.get_mut(said).map(Option::take) {
                match result {
                    Ok(true) => reply.out(args.get(said).map(String::as_str).unwrap_or_default()),
                    Ok(false) => {}
                    Err(e) => errors.push(e),
                }
                said += 1;
            }
        };
        say(&mut results, &mut errors);
        self.end_all(ending, &mut |i, ended, _| {
            let result = match thens.get_mut(i).and_then(Option::take) {
                Some(then) => then(ended),
                None => Ok(ended),
            };
            if let Some(slot) = results.get_mut(i) {
                *slot = Some(result);
            }
            say(&mut results, &mut errors);
        });
        for e in &errors {
            reply.err(e);
        }
        u8::from(!errors.is_empty())
    }

    /// [`each`](Self::each), for a client on a colour terminal: every target's outcome,
    /// how it ended and in how long, as a sheet once all are done.
    #[allow(clippy::too_many_arguments)]
    fn each_shown<'a>(
        &'a self,
        verb: &str,
        args: &[String],
        mut results: Vec<Option<Result<bool, String>>>,
        mut thens: Vec<Option<Then<'a>>>,
        ending: Vec<(usize, String, u32, Option<Duration>)>,
        began: Instant,
        hows: &mut [(Option<How>, Duration)],
        reply: &Reply<'_>,
    ) -> u8 {
        self.end_all(ending, &mut |i, ended, how| {
            let result = match thens.get_mut(i).and_then(Option::take) {
                Some(then) => then(ended),
                None => Ok(ended),
            };
            if let Some(slot) = results.get_mut(i) {
                *slot = Some(result);
            }
            if let Some(h) = hows.get_mut(i) {
                *h = (Some(how), began.elapsed());
            }
        });
        let mut sheet = shards_ipc::Sheet::new("ended");
        sheet.record(&[("kind", "head".into()), ("verb", verb.into())]);
        let mut failed = false;
        for (i, arg) in args.iter().enumerate() {
            let (how, took) = hows.get(i).copied().unwrap_or((None, Duration::ZERO));
            let (outcome, said) = match results.get_mut(i).and_then(Option::take) {
                Some(Ok(true)) => ("ok", String::new()),
                Some(Ok(false)) => ("none", String::new()),
                Some(Err(e)) => {
                    failed = true;
                    (
                        "error",
                        e.strip_prefix("Error response from daemon: ")
                            .unwrap_or(&e)
                            .to_string(),
                    )
                }
                None => ("none", String::new()),
            };
            let how = match how {
                None => "now",
                Some(How::Already) => "already",
                Some(How::Signal) => "signal",
                Some(How::Kill) => "kill",
                Some(How::Escalated) => "escalated",
                Some(How::Vm) => "vm",
                Some(How::Lost) => "lost",
            };
            sheet.record(&[
                ("target", arg.clone()),
                ("outcome", outcome.into()),
                ("how", how.into()),
                ("ms", took.as_millis().to_string()),
                ("error", said),
            ]);
        }
        reply.sheet(&sheet);
        u8::from(failed)
    }

    /// Ends every container of `targets` (its index, ID, signal and grace) at once, from
    /// one loop, as dockerd ends one (moby daemon/stop.go, containerStop): the signal, then SIGKILL once its grace
    /// is up (2 s if it could not be sent), then its VM after 10 s more, then giving up
    /// after 2 s. Each end is heard on a socket of its own, registered before anything is
    /// sent, so none is missed; `done` hears each container's index once, with whether it
    /// ended, as it does.
    fn end_all(
        &self,
        targets: Vec<(usize, String, u32, Option<Duration>)>,
        done: &mut dyn FnMut(usize, bool, How),
    ) {
        enum Phase {
            Signalled,
            Killed,
            Escalated,
            VmKilled,
        }
        let how = |p: &Phase| match p {
            Phase::Signalled => How::Signal,
            Phase::Killed => How::Kill,
            Phase::Escalated => How::Escalated,
            Phase::VmKilled => How::Vm,
        };
        struct Ending {
            index: usize,
            id: String,
            waiter: u64,
            told: UnixStream,
            phase: Phase,
            deadline: Option<Instant>,
        }
        let after = |d: Duration| Instant::now().checked_add(d);
        let mut going = Vec::with_capacity(targets.len());
        for (index, id, linux, grace) in targets {
            let (waiter, told) = match self.wake_at_end(&id) {
                Ok(Some(w)) => w,
                // Not running: ended already.
                Ok(None) => {
                    done(index, true, How::Already);
                    continue;
                }
                Err(e) => {
                    super::log(format!("container {id}: waiting for its end: {e}"));
                    done(index, false, How::Lost);
                    continue;
                }
            };
            let (phase, deadline) = if linux == 9 {
                self.signal(&id, 9);
                (Phase::Killed, after(KILL_WAIT))
            } else if self.signal(&id, linux) {
                (Phase::Signalled, grace.and_then(after))
            } else {
                (Phase::Signalled, after(UNSENT_WAIT))
            };
            going.push(Ending {
                index,
                id,
                waiter,
                told,
                phase,
                deadline,
            });
        }
        let mut polled: Vec<libc::pollfd> = Vec::with_capacity(going.len());
        while !going.is_empty() {
            // Escalate whatever is due.
            let now = Instant::now();
            let mut i = 0;
            while let Some(e) = going.get_mut(i) {
                if e.deadline.is_some_and(|d| d <= now) {
                    match e.phase {
                        Phase::Signalled => {
                            self.signal(&e.id, 9);
                            e.phase = Phase::Escalated;
                            e.deadline = after(KILL_WAIT);
                        }
                        Phase::Killed | Phase::Escalated => {
                            if let Some(RunState::Tracked(t)) = lock(&self.runs).get(&e.id) {
                                let _ = t.vm.kill(libc::SIGKILL);
                            }
                            e.phase = Phase::VmKilled;
                            e.deadline = after(LAST_WAIT);
                        }
                        Phase::VmKilled => {
                            let gone = going.swap_remove(i);
                            self.forget_waiter(&gone.id, gone.waiter);
                            // Its end may have come as it was given up.
                            let _ = gone.told.set_nonblocking(true);
                            let mut byte = [0u8; 1];
                            let ended = matches!((&gone.told).read(&mut byte), Ok(1));
                            done(gone.index, ended, if ended { How::Vm } else { How::Lost });
                            continue;
                        }
                    }
                }
                i += 1;
            }
            if going.is_empty() {
                break;
            }
            let wait = going.iter().filter_map(|e| e.deadline).min().map_or(-1, |d| {
                let left = d.saturating_duration_since(Instant::now());
                libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
            });
            polled.clear();
            polled.extend(going.iter().map(|e| libc::pollfd {
                fd: e.told.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }));
            let n = libc::nfds_t::try_from(polled.len()).unwrap_or(libc::nfds_t::MAX);
            // SAFETY: poll(2) on pollfds of sockets `going` holds open.
            if unsafe { libc::poll(polled.as_mut_ptr(), n, wait) } < 0
                && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
            {
                // Nothing to wait on: what is left is given up, as its time would be.
                for gone in going.drain(..) {
                    self.forget_waiter(&gone.id, gone.waiter);
                    done(gone.index, false, How::Lost);
                }
                break;
            }
            // Those whose end came, from the back, so the indices stay good.
            for k in (0..polled.len()).rev() {
                if polled.get(k).is_some_and(|p| p.revents != 0) {
                    let gone = going.swap_remove(k);
                    let mut byte = [0u8; 1];
                    let ended = matches!((&gone.told).read(&mut byte), Ok(1));
                    if !ended {
                        self.forget_waiter(&gone.id, gone.waiter);
                    }
                    done(
                        gone.index,
                        ended,
                        if ended { how(&gone.phase) } else { How::Lost },
                    );
                }
            }
        }
    }

    /// `shards wait`: for each container in turn, its exit code once it stops. Found and
    /// waited for under one hold of the records, as dockerd holds a container's state
    /// once it has found it: an end in between, of a `--rm` container above all, is not
    /// missed.
    fn wait(&self, references: &[String], client: u64, reply: &Reply<'_>) -> u8 {
        // Its client waits long: it shuts no other out (review 7.9).
        self.waits_long(client);
        let mut errors = Vec::new();
        for reference in references {
            let (registry, found) = self.resolve_held(lock(&self.containers), reference);
            match found {
                Ok(id) => {
                    // Its client may hang up first: then nobody reads the rest.
                    let Some(code) = self.await_exit_held(registry, &id, None, Some(reply.0)) else {
                        return 1;
                    };
                    reply.out(&code.to_string());
                }
                Err(e) => {
                    drop(registry);
                    errors.push(e);
                }
            }
        }
        for e in &errors {
            reply.err(e);
        }
        u8::from(!errors.is_empty())
    }

    /// `shards rm [-f]`: removes containers that have stopped; with `-f`, kills the ones
    /// still running first, and says nothing of those not there. The CLI trims `/` from
    /// both ends of each argument (docker/cli rm.go; moby daemon/delete.go). A container
    /// whose run no VM has yet been committed to has not started: it goes, and its run
    /// never starts, as dockerd removes a created container. One whose run is being handed
    /// over is removed once it is known whether it started, as dockerd's removal waits
    /// for a start under way (moby daemon/start.go holds the container's lock throughout).
    fn rm(&self, parsed: &Parsed, styled: bool, reply: &Reply<'_>) -> u8 {
        let force = parsed.bool("force");
        let volumes = parsed.bool("volumes");
        let args = self.by_image(&parsed.args, false);
        self.each(
            "rm",
            styled,
            &args,
            &|given| {
                let reference = given.trim_matches('/');
                if reference.is_empty() {
                    return Step::Now(Err("container name cannot be empty".into()));
                }
                // Then as the CLI's client sends it (`resolve`), dockerd's words name it.
                let reference = reference.trim().to_string();
                let id = match self.resolve(&reference) {
                    Ok(id) => id,
                    Err(_) if force => return Step::Now(Ok(false)),
                    Err(e) => return Step::Now(Err(e)),
                };
                let cannot = move |reference: &str, why: &str| {
                    format!("Error response from daemon: cannot remove container \"{reference}\": {why}")
                };
                if !lock(&self.removing).insert(id.clone()) {
                    return Step::Now(Err(format!(
                        "Error response from daemon: removal of container {reference} is already in progress"
                    )));
                }
                // Taken out of sight, set aside out of the registry's lock, which every
                // run's end takes, then made durable: an `rm` answered is never undone by
                // a crash. Ends its removal's being in progress, whatever the answer.
                let finish = {
                    let (id, reference) = (id.clone(), reference.clone());
                    move |removal: Option<Removal>| {
                        let answer = match removal {
                            None => Ok(true),
                            Some(removal) => self
                                .set_aside(&removal)
                                .map_err(|e| cannot(&reference, &e.to_string()))
                                .and_then(|()| {
                                    let completed = self.complete(&removal);
                                    if volumes && !removal.container.auto_remove {
                                        self.drop_anonymous(&removal.container);
                                    }
                                    completed.map(|()| true).map_err(|e| {
                                        format!(
                                            "Error response from daemon: container \"{reference}\" is removed, but its removal may not outlast a crash: {e}"
                                        )
                                    })
                                }),
                        };
                        lock(&self.removing).remove(&id);
                        answer
                    }
                };
                if let Some(removal) = self.cancel_start(&id) {
                    return Step::Now(finish(Some(removal)));
                }
                // One waiting to restart is running, to dockerd's rm (cleanupContainer).
                if self.is_restarting(&id) {
                    if !force {
                        lock(&self.removing).remove(&id);
                        return Step::Now(Err(cannot(
                            &reference,
                            "container is restarting: stop the container before removing or force remove",
                        )));
                    }
                    self.exit_on_next(&id, 9);
                }
                if lock(&self.containers).get(&id).is_none() {
                    return Step::Now(finish(None));
                }
                if self.running(&id) {
                    if !force && lock(&self.paused).contains(&id) {
                        lock(&self.removing).remove(&id);
                        return Step::Now(Err(cannot(
                            &reference,
                            "container is paused and must be unpaused first",
                        )));
                    }
                    if !force {
                        lock(&self.removing).remove(&id);
                        return Step::Now(Err(cannot(
                            &reference,
                            "container is running: stop the container before removing or force remove",
                        )));
                    }
                    self.exit_on_next(&id, 9);
                    let taken = id.clone();
                    return Step::End {
                        id: id.clone(),
                        linux: 9,
                        grace: None,
                        then: Box::new(move |ended| {
                            if !ended {
                                lock(&self.removing).remove(&taken);
                                return Err(cannot(
                                    &reference,
                                    "could not kill container: tried to kill container, but did not receive an exit event",
                                ));
                            }
                            let removal = lock(&self.containers).take_out(&taken);
                            finish(removal)
                        }),
                    };
                }
                let removal = lock(&self.containers).take_out(&id);
                Step::Now(finish(removal))
            },
            reply,
        )
    }

    /// `shards stop [-t SECONDS] [-s SIGNAL]`: the signal (SIGTERM), then SIGKILL once the
    /// time (10 s) is up, as dockerd stops a container (moby daemon/stop.go). A negative
    /// time waits for ever; a stopped container stops again without complaint. A container
    /// still starting is stopped once it runs: `shards run -d` prints its ID before then,
    /// where `docker run -d` prints it after.
    fn stop(&self, parsed: &Parsed, styled: bool, reply: &Reply<'_>) -> u8 {
        if parsed.changed("time") && parsed.changed("timeout") {
            reply.err("conflicting options: cannot specify both --timeout and --time");
            return 1;
        }
        let told = (parsed.changed("timeout") || parsed.changed("time")).then(|| parsed.int("timeout"));
        let signal = parsed.string("signal");
        let args = self.by_image(&parsed.args, true);
        self.each(
            "stop",
            styled,
            &args,
            &|reference| {
                // As the Docker CLI's client sends it (`resolve`), dockerd's words name it.
                let reference = reference.trim();
                let id = match self.resolve(reference) {
                    Ok(id) => id,
                    Err(e) => return Step::Now(Err(e)),
                };
                self.await_start(&id);
                // One waiting to restart is stopped by its stop signal: no restart, and
                // stopped by hand (kill.go, ExitOnNext).
                if self.is_restarting(&id) {
                    let own = self.own_stop(&id).0.unwrap_or(9);
                    self.exit_on_next(&id, own);
                    self.container_event(&id, "stop", &[]);
                    return Step::Now(Ok(true));
                }
                if !self.running(&id) {
                    return Step::Now(Ok(true));
                }
                let cannot = |why: &str| {
                    format!("Error response from daemon: cannot stop container: {reference}: {why}")
                };
                // Unless told, the container's own (`own_stop`).
                let (own_signal, own_grace) = self.own_stop(&id);
                let grace = told.map_or(own_grace, grace_of);
                let linux = if signal.is_empty() {
                    own_signal
                } else {
                    // A number Linux has no signal for cannot be sent.
                    match parse_signal(signal) {
                        Ok(n) => u32::try_from(n).ok().filter(|n| (1..=64).contains(n)),
                        Err(e) => return Step::Now(Err(cannot(&e))),
                    }
                };
                // No signal Linux has: killed, after 2 s.
                let (linux, grace) = linux.map_or((9, Some(UNSENT_WAIT)), |l| (l, grace));
                self.exit_on_next(&id, linux);
                let unheard = cannot("tried to kill container, but did not receive an exit event");
                let stopped = id.clone();
                Step::End {
                    id,
                    linux,
                    grace,
                    then: Box::new(move |ended| {
                        if !ended {
                            return Err(unheard);
                        }
                        self.container_event(&stopped, "stop", &[]);
                        Ok(true)
                    }),
                }
            },
            reply,
        )
    }

    /// Stops container `given` if it runs, for `restart` (moby daemon/restart.go,
    /// containerRestart: containerStop first): by `signal` and `timeout` where given,
    /// else as it stops; its ID, or what dockerd says.
    pub(super) fn stop_for_restart(
        &self,
        given: &str,
        signal: Option<&str>,
        timeout: Option<i64>,
    ) -> Result<String, String> {
        let id = self.resolve(given)?;
        self.await_start(&id);
        if !self.running(&id) {
            return Ok(id);
        }
        let cannot =
            |why: &str| format!("Error response from daemon: Cannot restart container {given}: {why}");
        let (own_signal, own_grace) = self.own_stop(&id);
        let grace = timeout.map_or(own_grace, grace_of);
        let linux = match signal {
            None => own_signal,
            Some(s) => match parse_signal(s) {
                Ok(n) => u32::try_from(n).ok().filter(|n| (1..=64).contains(n)),
                Err(e) => return Err(cannot(&e)),
            },
        };
        let (linux, grace) = linux.map_or((9, Some(UNSENT_WAIT)), |l| (l, grace));
        let mut ended = false;
        self.end_all(vec![(0, id.clone(), linux, grace)], &mut |_, e, _| ended = e);
        if !ended {
            return Err(cannot(
                "tried to kill container, but did not receive an exit event",
            ));
        }
        self.container_event(&id, "stop", &[]);
        Ok(id)
    }

    /// The stop container `id` asks for when nothing else is told, as dockerd reads it
    /// (moby daemon/stop.go, container.StopSignal and StopTimeout): its stop signal, else
    /// SIGTERM, `None` where Linux has no signal of its number; and how long its command
    /// may take to end after it: its stop timeout, else 10 s, for ever if negative. Set
    /// as the container was made, and read so: its record may not be written yet.
    pub(super) fn own_stop(&self, id: &str) -> (Option<u32>, Option<Duration>) {
        let (signal, timeout) = lock(&self.containers)
            .made(id)
            .map(|c| (c.stop_signal, c.stop_timeout))
            .unwrap_or_default();
        let signal = u32::try_from(signal.unwrap_or(15))
            .ok()
            .filter(|n| (1..=64).contains(n));
        (signal, timeout.map_or(Some(STOP_GRACE), grace_of))
    }

    /// `shards kill [-s SIGNAL]`: the signal (SIGKILL) to each running container's command.
    /// SIGKILL waits for the end, as dockerd's kill does; other signals are only sent
    /// (moby daemon/kill.go ContainerKill). Every error names its container (moby
    /// container_routes.go postContainersKill). A container still starting is signalled
    /// once it runs, as `stop` stops it.
    fn kill(&self, parsed: &Parsed, styled: bool, reply: &Reply<'_>) -> u8 {
        let signal = parsed.string("signal");
        let args = self.by_image(&parsed.args, true);
        self.each(
            "kill",
            styled,
            &args,
            &|reference| {
                // As the Docker CLI's client sends it (`resolve`), dockerd's words name it.
                let reference = reference.trim();
                let cannot = |why: &str| {
                    format!("Error response from daemon: cannot kill container: {reference}: {why}")
                };
                let linux = if signal.is_empty() {
                    9
                } else {
                    let n = match parse_signal(signal) {
                        Ok(n) => n,
                        Err(e) => return Step::Now(Err(cannot(&e))),
                    };
                    match linux_signal(n) {
                        Some(l) => l,
                        None => {
                            return Step::Now(Err(cannot(&format!(
                                "the linux daemon does not support signal {n}"
                            ))));
                        }
                    }
                };
                // dockerd's refusals in its own words for a kill; the client's as they are.
                let id = match self.resolve(reference) {
                    Ok(id) => id,
                    Err(e) => {
                        return Step::Now(Err(match e.strip_prefix("Error response from daemon: ") {
                            Some(said) => cannot(said),
                            None => e,
                        }));
                    }
                };
                let not_running = cannot(&format!("container {id} is not running"));
                self.await_start(&id);
                // One waiting to restart runs, to dockerd: the kill stops it there.
                if self.is_restarting(&id) {
                    self.exit_on_next(&id, linux);
                    return Step::Now(Ok(true));
                }
                if !self.running(&id) {
                    return Step::Now(Err(not_running));
                }
                self.exit_on_next(&id, linux);
                if linux == 9 {
                    let unheard = cannot("tried to kill container, but did not receive an exit event");
                    return Step::End {
                        id,
                        linux: 9,
                        grace: None,
                        then: Box::new(move |ended| if ended { Ok(true) } else { Err(unheard) }),
                    };
                }
                Step::Now(if self.signal(&id, linux) {
                    Ok(true)
                } else {
                    Err(not_running)
                })
            },
            reply,
        )
    }

    /// `shards ps`: the containers, as `docker ps` lists them (docker/cli v29.8.1
    /// cli/command/formatter/container.go): the running ones, or all with `-a`, or the
    /// last `-n` made (`-l`: one); newest first; with `-q` their IDs alone.
    fn ps(&self, parsed: &Parsed, asker: &Asker, reply: &Reply<'_>) -> u8 {
        let east_asian = asker.east_asian;
        let trunc = !parsed.bool("no-trunc");
        let (list, all) = match self.ps_listing(parsed) {
            Ok(l) => (l.containers, l.all),
            Err(e) => {
                reply.err(&format!("Error response from daemon: {e}"));
                return 1;
            }
        };
        let at = now();
        let removing = lock(&self.removing).clone();
        let paused = lock(&self.paused).clone();
        let health = lock(&self.health);
        let listed: Vec<Listed> = list
            .iter()
            .map(|c| Listed {
                command: command_line(&c.command),
                status: status(
                    c,
                    at,
                    health.get(&c.id).map(|h| h.status),
                    removing.contains(&c.id),
                    paused.contains(&c.id),
                ),
                // dockerd lists ports while a container runs.
                ports: if c.state == Life::Running {
                    displayable_ports(
                        c.ports
                            .iter()
                            .map(|p| PortSummary {
                                ip: p.ip,
                                private: p.private,
                                public: p.public,
                                proto: p.proto.clone(),
                            })
                            .collect(),
                    )
                } else {
                    String::new()
                },
            })
            .collect();
        // A colour terminal gets shards' page: each microVM a record.
        if asker.styled() && !parsed.bool("quiet") && parsed.string("format").is_empty() {
            let mut sheet = shards_ipc::Sheet::new("ps");
            let running = list.iter().filter(|c| c.state == Life::Running).count();
            let total = lock(&self.containers).all().count();
            sheet.record(&[
                ("kind", "head".into()),
                ("running", running.to_string()),
                ("total", total.to_string()),
                ("all", all.to_string()),
            ]);
            for (c, l) in list.iter().zip(&listed) {
                let state = match c.state {
                    Life::Running if paused.contains(&c.id) => "paused",
                    Life::Running => "running",
                    Life::Created => "created",
                    _ if c.restart.restarting => "restarting",
                    _ => "exited",
                };
                sheet.record(&[
                    ("name", c.name.clone()),
                    ("id", c.id.get(..12).unwrap_or(&c.id).to_string()),
                    ("image", c.image.clone()),
                    ("command", l.command.clone()),
                    ("state", state.into()),
                    ("status", l.status.clone()),
                    ("exit", c.exit_code.map(|e| e.to_string()).unwrap_or_default()),
                    ("ports", l.ports.clone()),
                    ("created", (c.created / 1_000_000_000).to_string()),
                ]);
            }
            reply.sheet(&sheet);
            return 0;
        }
        // Anywhere else, the rows, which the client lays out as the CLI does
        // (cli/listing.rs): in its clock and zone, with its `--format`.
        drop(health);
        let mut rows = self.container_rows(&list);
        // Sizes where `--size` asks, or a format shows them unless `-q` (list.go,
        // buildContainerListOptions).
        let format = parsed.string("format");
        let clock = shards_cmdline::format::Clock {
            now: 0,
            zone: &shards_cmdline::format::utc,
        };
        let shown =
            !format.is_empty() && shards_cmdline::format::container::check(format, &clock).unwrap_or(false);
        if parsed.bool("size") || (shown && !parsed.bool("quiet")) {
            for (row, c) in rows.iter_mut().zip(&list) {
                let (rw, root) = self.container_sizes(c);
                row["size_rw"] = serde_json::json!(rw);
                row["size_root_fs"] = serde_json::json!(root);
            }
        }
        let mut sheet = shards_ipc::Sheet::new("ps-rows");
        sheet.record(&[("rows", serde_json::Value::Array(rows).to_string())]);
        reply.sheet(&sheet);
        let _ = (trunc, east_asian);
        0
    }

    /// Containers `list` as rows for a client to lay out (cli/listing.rs): what dockerd's
    /// container list says of each, its status as of now.
    pub(super) fn container_rows(&self, list: &[Container]) -> Vec<serde_json::Value> {
        let at = now();
        let removing = lock(&self.removing).clone();
        let paused = lock(&self.paused).clone();
        let health = lock(&self.health);
        list.iter()
            .map(|c| {
                let ports: Vec<serde_json::Value> = if c.state == Life::Running {
                    c.ports
                        .iter()
                        .map(|p| {
                            serde_json::json!({
                                "ip": p.ip.map(|ip| ip.to_string()),
                                "private": p.private,
                                "public": p.public,
                                "type": p.proto,
                            })
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let state = match c.state {
                    Life::Running if paused.contains(&c.id) => "paused",
                    Life::Running => "running",
                    Life::Created => "created",
                    _ if c.restart.restarting => "restarting",
                    _ => "exited",
                };
                let checked = health.get(&c.id).map(|h| h.status);
                serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    "image": c.image,
                    "image_id": c.image_id.clone().unwrap_or_default(),
                    "command": command_line(&c.command),
                    "created": i64::try_from(c.created / 1_000_000_000).unwrap_or(0),
                    "ports": ports,
                    "labels": c.labels,
                    "state": state,
                    "status": status(c, at, checked, removing.contains(&c.id), paused.contains(&c.id)),
                    "health": checked.map_or("", |s| match s {
                        super::health::Status::Starting => "starting",
                        super::health::Status::Healthy => "healthy",
                        super::health::Status::Unhealthy => "unhealthy",
                    }),
                })
            })
            .collect()
    }

    /// `shards logs [-f] [-t] [--details] [--tail N] [--since T] [--until T]`: a
    /// container's output, stdout to stdout and stderr to stderr, in the order it arrived;
    /// the last N lines with `--tail`, of which those from `--since` on and to `--until`,
    /// each after its time with `-t`, and more as it comes until the container ends with
    /// `-f`. Lines carry no attributes here, so `--details` adds only the space that would
    /// follow them (moby daemon/server/httputils/logstream/logstream.go).
    fn logs(&self, parsed: &Parsed, asker: &Asker, reply: &Reply<'_>) -> u8 {
        let Some(reference) = parsed.args.first() else {
            return 1;
        };
        // `all`, a negative count or anything but a number means every line (moby
        // client/container_logs.go; daemon/logger/loggerutils/logfile.go).
        let tail = parsed
            .string("tail")
            .parse::<i64>()
            .ok()
            .and_then(|n| usize::try_from(n).ok());
        let shown = Shown {
            stamps: parsed.bool("timestamps"),
            details: parsed.bool("details"),
        };
        let follow = parsed.bool("follow");
        // The CLI finds the container before it reads the times (docker/cli logs.go).
        let id = match self.resolve(reference) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        let mut window = Window::default();
        for (flag, bound) in [("since", &mut window.since), ("until", &mut window.until)] {
            let given = parsed.string(flag);
            if given.is_empty() {
                continue;
            }
            // What the client makes of it, then what dockerd makes of that (moby
            // client/container_logs.go; daemon/server/router/container/container_routes.go).
            let sent = match gotime::get_timestamp(given, i128::from(asker.now), i64::from(asker.utc_offset))
            {
                Ok(sent) => sent,
                Err(e) => {
                    reply.err(&format!("invalid value for \"{flag}\": {e}"));
                    return 1;
                }
            };
            if flag == "until" && sent == "0" {
                continue;
            }
            match gotime::parse_unix_timestamp(&sent) {
                Ok(at) => *bound = at,
                Err(e) => {
                    reply.err(&format!(
                        "Error response from daemon: invalid value for \"{flag}\": {e}"
                    ));
                    return 1;
                }
            }
        }
        let dir = lock(&self.containers).dir(&id);
        let log = match LogFile::open(&dir) {
            Ok(log) => log,
            Err(e) => {
                reply.err(&format!(
                    "Error response from daemon: container {id}: its log: {e}"
                ));
                return 1;
            }
        };
        let mut show = Show {
            reply,
            shown,
            window,
            admitted: [Admit::Pass, Admit::Pass],
            stream: LOG_STDOUT,
            gathered: Vec::new(),
            unsent: None,
        };
        // Without -f, what is there is all there is: a line in progress too.
        let ended = !follow || !self.running(&id);
        let read = (|| -> io::Result<()> {
            let mut reader = match tail {
                Some(n) => Reader::from(logs::tail(&log, n as u64, ended)?),
                None => Reader::new(),
            };
            if !show.read(&mut reader, &log)? {
                return Ok(());
            }
            if ended {
                show.finish(&mut reader)?;
                return Ok(());
            }
            // Followed as it grows: woken by an append to the segment it is read from, by
            // a segment's coming, by the run's end, or by the client's hanging up, not by
            // a timer (audit A12). Each watch comes before a read, so no append between
            // them goes unseen.
            let mut watch = shards_vmm::platform::FileWatch::new(log.dir())?;
            let mut watched = None;
            let Some((number, end)) = self.wake_at_end(&id)? else {
                if show.read(&mut reader, &log)? {
                    show.finish(&mut reader)?;
                }
                return Ok(());
            };
            // Its client waits long: it shuts no other out (review 7.9).
            self.waits_long(asker.client);
            let followed = (|| -> io::Result<()> {
                loop {
                    if !show.read(&mut reader, &log)? {
                        return Ok(());
                    }
                    if let Some(segment) = reader.segment()
                        && watched != Some(segment.seq())
                    {
                        watch.file(segment.index())?;
                        watched = Some(segment.seq());
                        continue;
                    }
                    let ready = wait_readable(&[watch.fd(), end.as_fd(), reply.0.as_fd()])?;
                    // Once the container has ended, what is left of a line is all of it.
                    if ready[1] {
                        if show.read(&mut reader, &log)? {
                            show.finish(&mut reader)?;
                        }
                        return Ok(());
                    }
                    // A client that hangs up ends its follow, output or none (audit A07).
                    if ready[2] {
                        return Ok(());
                    }
                    watch.clear();
                }
            })();
            self.forget_waiter(&id, number);
            followed
        })();
        if let Some(e) = show.unsent {
            return undelivered(&e, reply);
        }
        if let Err(e) = read {
            reply.err(&format!(
                "Error response from daemon: container {id}: its log: {e}"
            ));
            return 1;
        }
        // Output a log could not keep, it does not pretend to hold (audit A12).
        let lost = lock(&self.containers).get(&id).map_or(0, |c| c.log_lost);
        if lost > 0 {
            reply.err(&format!(
                "shards: {lost} bytes of container {id}'s output could not be kept in its log"
            ));
            return 1;
        }
        0
    }
}

/// What `logs` shows its client: the lines in its window, each after its prefix, gathered
/// into messages of one stream of up to [`logs::CHUNK`] bytes, sent as they fill, as the
/// stream changes, and once what there is to read is read. A message a piece cost a send
/// and the client a read for every line (review 7.10, PM M95).
struct Show<'r, 'a> {
    reply: &'r Reply<'a>,
    shown: Shown,
    window: Window,
    /// A line's window is decided at its first piece, and holds for the rest of it.
    admitted: [Admit; 2],
    /// The stream of what is gathered.
    stream: u8,
    gathered: Vec<u8>,
    /// Why the client was not sent what it was to be, if it was not.
    unsent: Option<io::Error>,
}

impl Show<'_, '_> {
    /// Reads what `reader` has of `log` and sends it: whether to go on.
    fn read(&mut self, reader: &mut Reader, log: &LogFile) -> io::Result<bool> {
        let go_on = reader.read(log, &mut |piece| self.piece(&piece))?;
        Ok(self.send() && go_on)
    }

    /// Takes what is left of each stream's unfinished line, and sends it.
    fn finish(&mut self, reader: &mut Reader) -> io::Result<bool> {
        let go_on = reader.finish(&mut |piece| self.piece(&piece))?;
        Ok(self.send() && go_on)
    }

    fn piece(&mut self, piece: &Piece<'_>) -> io::Result<bool> {
        let s = usize::from(piece.stream == LOG_STDERR);
        let Some(decision) = self.admitted.get_mut(s) else {
            return Ok(false);
        };
        if piece.first {
            *decision = self.window.admit(piece.at);
        }
        match decision {
            Admit::Skip => Ok(true),
            Admit::Stop => Ok(false),
            Admit::Pass => {
                if piece.stream != self.stream && !self.send() {
                    return Ok(false);
                }
                self.stream = piece.stream;
                if piece.first && self.shown.stamps {
                    self.gathered.extend_from_slice(rfc3339_nano(piece.at).as_bytes());
                    self.gathered.push(b' ');
                }
                if piece.first && self.shown.details {
                    self.gathered.push(b' ');
                }
                self.gathered.extend_from_slice(piece.bytes);
                Ok(self.gathered.len() < logs::CHUNK || self.send())
            }
        }
    }

    /// Sends what is gathered: whether it went.
    fn send(&mut self) -> bool {
        if self.gathered.is_empty() || self.unsent.is_some() {
            return self.unsent.is_none();
        }
        let sent = self.reply.bytes(self.stream, &self.gathered);
        self.gathered.clear();
        match sent {
            Ok(()) => true,
            Err(e) => {
                self.unsent = Some(e);
                false
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {

    #[test]
    fn top_refuses_pid_headers_on_other_fields_as_dockerd_does() {
        // moby daemon/top_unix_test.go, TestContainerTopValidatePSArgs, and the regexp's
        // backtracking.
        for ok in [
            "-ef",
            "-o pid=PID",
            "-o pid=PID,user=USER",
            "-o pid=",
            "pid=PID",
            "-o  pid= PID",
        ] {
            assert_eq!(ps_args_allowed(ok), Ok(()), "{ok}");
        }
        // TestContainerTopValidatePSArgs's own, at the pinned moby.
        for (args, refused) in [
            ("ae -o uid=PID", true),
            ("ae -o \"uid= PID\"", true),
            ("ae -o \"uid=\u{2003}PID\"", false),
            ("ae o uid=PID", true),
            ("aeo uid=PID", true),
            ("ae -O uid=PID", true),
            ("ae -o pid=PID2 -o uid=PID", true),
            ("ae -o pid=PID", false),
            ("ae -o pid=PID -o uid=PIDX", true),
            ("aeo pid=PID", false),
            ("ae", false),
            ("", false),
        ] {
            assert_eq!(ps_args_allowed(args).is_err(), refused, "{args}");
        }
        for (bad, said) in [
            (
                "-o pid=PID,user=PID",
                r#"specifying "pid=PID,user=PID" is not allowed"#,
            ),
            ("-o user=PID", r#"specifying "user=PID" is not allowed"#),
            ("-o user= PID", r#"specifying "user=PID" is not allowed"#),
            ("-o x=PIDy=z", r#"specifying "x=PIDy=z" is not allowed"#),
            (
                "-ef -o pid=PID -o user=PIDS",
                r#"specifying "user=PIDS" is not allowed"#,
            ),
        ] {
            assert_eq!(ps_args_allowed(bad), Err(said.to_string()), "{bad}");
        }
    }

    #[test]
    fn memory_is_shown_as_go_units_shows_it() {
        // go-units BytesSize: %.4g of binary units.
        assert_eq!(binary_size(0), "0B");
        assert_eq!(binary_size(1023), "1023B");
        assert_eq!(binary_size(1536), "1.5KiB");
        assert_eq!(binary_size(256 << 20), "256MiB");
        assert_eq!(binary_size(12_345_678), "11.77MiB");
        assert_eq!(binary_size(1 << 30), "1GiB");
    }

    use super::*;

    /// `logs` gathers output into messages of one stream, sent as one fills past a chunk
    /// and as the stream changes: in order, where it sent a message a line (review 7.10).
    #[test]
    fn logs_output_is_gathered_by_stream_in_order() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        // Read as it is sent: more than a socket's buffer goes.
        let reading = std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut out = Vec::new();
            while let Some(m) = shards_ipc::recv(&theirs).unwrap() {
                got.push((m.kind, m.payload.len()));
                if m.kind == kind::OUT {
                    out.extend_from_slice(&m.payload);
                }
            }
            (got, out)
        });
        let reply = Reply(&ours);
        let mut show = Show {
            reply: &reply,
            shown: Shown {
                stamps: false,
                details: false,
            },
            window: Window::default(),
            admitted: [Admit::Pass, Admit::Pass],
            stream: LOG_STDOUT,
            gathered: Vec::new(),
            unsent: None,
        };
        fn piece(stream: u8, bytes: &[u8]) -> Piece<'_> {
            Piece {
                stream,
                at: 0,
                first: true,
                bytes,
            }
        }
        let mut sent = Vec::new();
        for n in 0..10_000 {
            let line = format!("{n:09}\n").into_bytes();
            assert!(show.piece(&piece(LOG_STDOUT, &line)).unwrap());
            sent.extend_from_slice(&line);
        }
        assert!(show.piece(&piece(LOG_STDERR, b"err\n")).unwrap());
        assert!(show.piece(&piece(LOG_STDOUT, b"out\n")).unwrap());
        assert!(show.send());
        drop(show);
        drop(ours);
        let (got, out) = reading.join().unwrap();
        assert_eq!(
            got,
            [
                (kind::OUT, 65_540),
                (kind::OUT, 34_460),
                (kind::ERR, 4),
                (kind::OUT, 4)
            ]
        );
        sent.extend_from_slice(b"out\n");
        assert_eq!(out, sent);
    }

    #[test]
    fn times_print_as_docker_logs_prints_them() {
        assert_eq!(rfc3339_nano(0), "1970-01-01T00:00:00.000000000Z");
        // 2026-09-29T09:12:34.000000005Z
        assert_eq!(
            rfc3339_nano(1_790_673_154_000_000_005),
            "2026-09-29T09:12:34.000000005Z"
        );
        // A leap day.
        assert_eq!(
            rfc3339_nano(1_709_164_800_000_000_000),
            "2024-02-29T00:00:00.000000000Z"
        );
    }

    #[test]
    fn log_windows_filter_as_dockerd_forwards() {
        let mut window = Window {
            since: Some(10),
            until: Some(20),
        };
        let seen: Vec<Admit> = [5, 12, 8, 20, 21, 15]
            .iter()
            .map(|&at| window.admit(at))
            .collect();
        // Once a line has passed `since`, an earlier time passes too.
        assert_eq!(
            seen,
            [
                Admit::Skip,
                Admit::Pass,
                Admit::Pass,
                Admit::Pass,
                Admit::Stop,
                Admit::Pass
            ]
        );
        assert_eq!(Window::default().admit(0), Admit::Pass);
    }

    /// Every table the Docker CLI printed of the same containers (scripts/docker-cli/
    /// ps_test.go), shards prints byte for byte; and go-units' durations likewise.
    /// The PORTS column as docker/cli's DisplayablePorts writes it, for each set of ports in
    /// docker-ps.json.
    #[test]
    fn ports_show_as_docker_ps_shows_them() {
        let golden: serde_json::Value = serde_json::from_str(include_str!("docker-ps.json")).unwrap();
        for set in golden["ports"].as_array().unwrap() {
            assert_eq!(
                displayable_ports(port_summaries(&set["ports"])),
                set["shown"].as_str().unwrap()
            );
        }
    }

    fn port_summaries(ports: &serde_json::Value) -> Vec<PortSummary> {
        ports
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|p| PortSummary {
                ip: p["ip"].as_str().unwrap().parse().ok(),
                private: u16::try_from(p["private"].as_u64().unwrap()).unwrap(),
                public: u16::try_from(p["public"].as_u64().unwrap()).unwrap(),
                proto: p["type"].as_str().unwrap().to_string(),
            })
            .collect()
    }

    #[test]
    fn durations_read_as_go_units_writes_them() {
        let s = 1_000_000_000u128;
        for (ns, words) in [
            (s / 2, "Less than a second"),
            (s, "1 second"),
            (59 * s, "59 seconds"),
            (60 * s, "About a minute"),
            (119 * s, "About a minute"),
            (120 * s, "2 minutes"),
            (3599 * s, "59 minutes"),
            (3600 * s, "About an hour"),
            (5399 * s, "About an hour"),
            (5400 * s, "2 hours"),
            (47 * 3600 * s, "47 hours"),
            (48 * 3600 * s, "2 days"),
            (14 * 24 * 3600 * s, "2 weeks"),
            (60 * 24 * 3600 * s, "2 months"),
            (730 * 24 * 3600 * s, "2 years"),
        ] {
            assert_eq!(human_duration(ns), words, "{ns}");
        }
    }

    #[test]
    fn commands_show_as_dockerd_lists_them() {
        let argv = |w: &[&str]| w.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(command_line(&argv(&["sh", "-c", "echo hi"])), "sh -c 'echo hi'");
    }

    #[test]
    fn tables_align_as_go_tabwriter_aligns_them() {
        let rows = [
            ["CONTAINER ID", "IMAGE", "NAMES"].map(String::from),
            ["0123456789ab", "alpine", "web"].map(String::from),
        ];
        assert_eq!(
            tabulate(&rows, false),
            ["CONTAINER ID   IMAGE     NAMES", "0123456789ab   alpine    web"]
        );
        // The ellipsis is ambiguous: two columns in an East Asian locale.
        let cut = [["\"ab\u{2026}\"", "x"].map(String::from)];
        assert_eq!(tabulate(&cut, false), ["\"ab\u{2026}\"     x"]);
        assert_eq!(tabulate(&cut, true), ["\"ab\u{2026}\"    x"]);
    }

    #[test]
    fn signals_are_taken_as_dockerd_takes_them() {
        for (given, n) in [
            ("KILL", 9),
            ("SIGTERM", 15),
            ("term", 15),
            ("sigusr1", 10),
            ("9", 9),
            ("+9", 9),
            ("-9", -9),
            ("99", 99),
            ("cld", 17),
            ("RTMIN", 34),
            ("SIGRTMIN+15", 49),
            ("RTMAX-14", 50),
            ("rtmax", 64),
        ] {
            assert_eq!(parse_signal(given), Ok(n), "{given}");
        }
        for bad in [
            "", "0", "SIG", "NOPE", "RTMIN+16", "RTMIN+01", "RTMAX-0", "1.5", " 9",
        ] {
            assert_eq!(parse_signal(bad), Err(format!("invalid signal: {bad}")), "{bad}");
        }
        assert_eq!(linux_signal(31), Some(31));
        assert_eq!(linux_signal(34), Some(34));
        for bad in [32, 33, 65, -9] {
            assert_eq!(linux_signal(bad), None, "{bad}");
        }
    }
}

/// What dockerd says of a container it lists: its command as one line, its status in
/// words, and its ports.
struct Listed {
    command: String,
    status: String,
    ports: String,
}

/// A port as dockerd lists a running container's: published at a host address and port,
/// or exposed alone (no address, public port 0).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PortSummary {
    pub ip: Option<std::net::IpAddr>,
    pub private: u16,
    pub public: u16,
    pub proto: String,
}

/// The PORTS column of `docker ps` (docker/cli cli/command/formatter/container.go,
/// DisplayablePorts): ports sorted by private port, address, public port and protocol;
/// those published on their own number grouped into ranges per address and protocol,
/// as are those exposed alone; the rest as `address:public->private/protocol` after them.
pub(super) fn displayable_ports(mut ports: Vec<PortSummary>) -> String {
    ports.sort_by(|a, b| (a.private, a.ip, a.public, &a.proto).cmp(&(b.private, b.ip, b.public, &b.proto)));
    // A host and port as Go's net.JoinHostPort writes them: IPv6 in brackets.
    let join = |ip: &std::net::IpAddr, port: &str| match ip {
        std::net::IpAddr::V6(v6) => format!("[{v6}]:{port}"),
        std::net::IpAddr::V4(v4) => format!("{v4}:{port}"),
    };
    let form = |key: &(Option<std::net::IpAddr>, String), first: u16, last: u16| {
        let group = if first == last {
            first.to_string()
        } else {
            format!("{first}-{last}")
        };
        match &key.0 {
            Some(ip) => format!("{}->{group}/{}", join(ip, &group), key.1),
            None => format!("{group}/{}", key.1),
        }
    };
    let mut groups: Vec<((Option<std::net::IpAddr>, String), u16, u16)> = Vec::new();
    let (mut result, mut mappings) = (Vec::new(), Vec::new());
    for p in &ports {
        if let Some(ip) = &p.ip
            && p.public != p.private
        {
            mappings.push(format!(
                "{}->{}/{}",
                join(ip, &p.public.to_string()),
                p.private,
                p.proto
            ));
            continue;
        }
        let key = (p.ip, p.proto.clone());
        match groups.iter_mut().find(|(k, _, _)| *k == key) {
            None => groups.push((key, p.private, p.private)),
            Some((_, _, last)) if p.private == last.wrapping_add(1) => *last = p.private,
            Some(group) => {
                result.push(form(&group.0, group.1, group.2));
                (group.1, group.2) = (p.private, p.private);
            }
        }
    }
    result.extend(groups.iter().map(|(k, first, last)| form(k, *first, *last)));
    result.extend(mappings);
    result.join(", ")
}

/// A container's command as the API shows it: the path, then the arguments, each in single
/// quotes if it holds a space (moby daemon/container/view.go).
fn command_line(argv: &[String]) -> String {
    let Some((path, args)) = argv.split_first() else {
        return String::new();
    };
    let mut line = path.clone();
    for arg in args {
        line.push(' ');
        if arg.contains(' ') {
            line.push('\'');
            line.push_str(arg);
            line.push('\'');
        } else {
            line.push_str(arg);
        }
    }
    line
}

/// A microVM `stats` sampled: its ID and name, and while it runs, its share of a CPU in
/// percent and its resident bytes.
struct Sampled {
    id: String,
    name: String,
    used: Option<(f64, u64)>,
}

/// go-units' BytesSize (`%.4g` and binary units), as `stats` shows memory: 1.5MiB, 256MiB.
pub(super) fn binary_size(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    #[allow(clippy::cast_precision_loss)]
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    // Four significant digits, trailing zeros dropped.
    let digits = if size >= 1000.0 {
        0
    } else if size >= 100.0 {
        1
    } else if size >= 10.0 {
        2
    } else {
        3
    };
    let shown = format!("{size:.digits$}");
    let shown = if shown.contains('.') {
        shown.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        shown
    };
    format!("{shown}{}", UNITS.get(unit).unwrap_or(&""))
}

/// A duration in nanoseconds, in words, as go-units v0.5.0 `HumanDuration` puts it.
pub(super) fn human_duration(ns: u128) -> String {
    let seconds = ns / 1_000_000_000;
    let minutes = seconds / 60;
    // Go rounds the hours: int(d.Hours() + 0.5).
    let hours = (ns + 1_800_000_000_000) / 3_600_000_000_000;
    match () {
        () if seconds < 1 => "Less than a second".into(),
        () if seconds == 1 => "1 second".into(),
        () if seconds < 60 => format!("{seconds} seconds"),
        () if minutes == 1 => "About a minute".into(),
        () if minutes < 60 => format!("{minutes} minutes"),
        () if hours == 1 => "About an hour".into(),
        () if hours < 48 => format!("{hours} hours"),
        () if hours < 24 * 7 * 2 => format!("{} days", hours / 24),
        () if hours < 24 * 30 * 2 => format!("{} weeks", hours / 24 / 7),
        () if hours < 24 * 365 * 2 => format!("{} months", hours / 24 / 30),
        () => format!("{} years", ns / 3_600_000_000_000 / 24 / 365),
    }
}

/// A container's status as dockerd words it (moby daemon/container/state.go): up for how
/// long, with its health if it has a check; else being removed, if it is; or exited with
/// what status how long ago, or created and never started.
fn status(
    c: &Container,
    at: u128,
    health: Option<super::health::Status>,
    removing: bool,
    paused: bool,
) -> String {
    match (c.state, c.started, c.finished) {
        (Life::Running, Some(started), _) => {
            let up = human_duration(at.saturating_sub(started));
            // moby daemon/container/state.go, String: paused before healthy.
            if paused {
                return format!("Up {up} (Paused)");
            }
            match health {
                Some(h) => format!("Up {up} ({})", h.shown()),
                None => format!("Up {up}"),
            }
        }
        // moby State.String: one waiting to restart, by how it ended and how long ago.
        (_, Some(_), Some(finished)) if c.restart.restarting => format!(
            "Restarting ({}) {} ago",
            c.exit_code.unwrap_or(0),
            human_duration(at.saturating_sub(finished))
        ),
        _ if removing => "Removal In Progress".into(),
        (_, None, _) => "Created".into(),
        (_, Some(_), Some(finished)) => format!(
            "Exited ({}) {} ago",
            c.exit_code.unwrap_or(0),
            human_duration(at.saturating_sub(finished))
        ),
        (_, Some(_), None) => String::new(),
    }
}

/// `rows` aligned as the Docker CLI's tabwriter aligns them (minimum width 10, padding 3,
/// spaces; docker/cli cli/command/formatter/tabwriter): each column but the last is as
/// wide as its widest cell plus 3, and at least 10, in go-runewidth's columns.
pub(super) fn tabulate<const N: usize>(rows: &[[String; N]], east_asian: bool) -> Vec<String> {
    tabulate_with(rows, 10, east_asian)
}

/// [`tabulate`], with columns at least `min` wide, for rows of any one length.
pub(super) fn tabulate_with<R: AsRef<[String]>>(rows: &[R], min: usize, east_asian: bool) -> Vec<String> {
    let cell_width = |cell: &str| width::string_width(cell, east_asian);
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (i, cell) in row.as_ref().iter().enumerate() {
            let w = (cell_width(cell) + 3).max(min);
            match widths.get_mut(i) {
                Some(have) => *have = (*have).max(w),
                None => widths.push(w),
            }
        }
    }
    rows.iter()
        .map(|row| {
            let row = row.as_ref();
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                line.push_str(cell);
                if i + 1 < row.len() {
                    let pad = widths
                        .get(i)
                        .copied()
                        .unwrap_or(min)
                        .saturating_sub(cell_width(cell));
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            line
        })
        .collect()
}

/// `diff`'s lines, taken whole from the pieces they come in, and counted by kind (added,
/// changed, deleted), for a client on a colour terminal.
#[derive(Default)]
struct DiffPage {
    partial: Vec<u8>,
    counts: [u64; 3],
}

impl DiffPage {
    /// The whole lines `chunk` ends, each its kind and path.
    fn take(&mut self, chunk: &[u8]) -> Vec<(String, String)> {
        self.partial.extend_from_slice(chunk);
        let Some(last) = self.partial.iter().rposition(|&b| b == b'\n') else {
            return Vec::new();
        };
        let whole: Vec<u8> = self.partial.drain(..=last).collect();
        whole
            .split(|&b| b == b'\n')
            .filter_map(|line| {
                let (&[kind, _], path) = line.split_first_chunk::<2>()?;
                let at = match kind {
                    b'A' => 0,
                    b'C' => 1,
                    _ => 2,
                };
                if let Some(n) = self.counts.get_mut(at) {
                    *n += 1;
                }
                Some((
                    char::from(kind).to_string(),
                    String::from_utf8_lossy(path).into_owned(),
                ))
            })
            .collect()
    }
}

/// The first word of `shards cp`'s steps, which no command line has.
pub(crate) const COPY_STEP: &str = "\u{0}cp";

/// The most of a microVM's process dump `top` takes: some 2 KiB a process.
const TOP_LIMIT: usize = 16 << 20;

/// dockerd's check of `top`'s ps options (top_unix.go, validatePSArgs): over each match
/// of `\s+([^\s]*)=\s*(PID[^\s]*)`, a field headed `PID…` must be `pid`, which dockerd
/// finds its processes by.
fn ps_args_allowed(args: &str) -> Result<(), String> {
    let space = |c: char| matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ');
    let mut rest = args;
    // Each match begins at a run of spaces; matches do not overlap.
    while let Some(at) = rest.find(space) {
        let after = rest.get(at..).unwrap_or_default().trim_start_matches(space);
        let word_end = after.find(space).unwrap_or(after.len());
        let word = after.get(..word_end).unwrap_or_default();
        // `[^\s]*=` takes as much of the word as leaves a value naming PID: the last `=`
        // that does; one ending the word may have its value after spaces.
        let found = word.rmatch_indices('=').find_map(|(eq, _)| {
            let key = word.get(..eq).unwrap_or_default();
            let value = word.get(eq + 1..).unwrap_or_default();
            if value.starts_with("PID") {
                return Some((key, value, word_end));
            }
            if !value.is_empty() {
                return None;
            }
            let tail = after.get(word_end..).unwrap_or_default();
            let next = tail.trim_start_matches(space);
            let value = next
                .get(..next.find(space).unwrap_or(next.len()))
                .unwrap_or_default();
            value
                .starts_with("PID")
                .then(|| (key, value, after.len() - next.len() + value.len()))
        });
        match found {
            Some((key, value, _)) if key != "pid" => {
                return Err(format!("specifying \"{key}={value}\" is not allowed"));
            }
            Some((_, _, end)) => rest = after.get(end..).unwrap_or_default(),
            None => rest = after.get(word_end..).unwrap_or_default(),
        }
    }
    Ok(())
}

/// Who asked for a container command, and what of theirs shapes the answer.
pub(super) struct Asker {
    /// Their number among the daemon's clients: what a shutdown cancels is theirs by it.
    pub client: u64,
    /// Their [`shards_ipc::REGISTRY_ENV`]: a registry is reached as they would reach it.
    pub registry_env: Vec<String>,
    /// Their locale is East Asian, for `ps`'s widths.
    pub east_asian: bool,
    /// Their clock, nanoseconds since the epoch, and their zone's offset east of UTC in
    /// seconds, for `logs --since` and `--until`.
    pub now: i64,
    pub utc_offset: i32,
    /// Their stdout: a terminal of so many columns, and whether colours are welcome.
    pub terminal: bool,
    pub width: u16,
    pub color: bool,
    /// What they sent to be written: `save`'s archive's destination.
    pub files: Vec<OwnedFd>,
}

impl Asker {
    /// Whether their stdout is a colour terminal, where shards draws its own pages.
    pub(super) fn styled(&self) -> bool {
        self.terminal && self.color
    }
}

/// The times `logs` shows lines between, as dockerd's log forwarder keeps them (moby
/// daemon/logger/loggerutils/logfile.go, forwarder.Do), in nanoseconds since the epoch.
#[derive(Default)]
struct Window {
    since: Option<i128>,
    until: Option<i128>,
}

/// What `Window::admit` makes of a line.
#[derive(Debug, PartialEq, Eq)]
enum Admit {
    Pass,
    Skip,
    /// This line and every one after it are past `until`.
    Stop,
}

impl Window {
    /// Whether the line that arrived `at` shows. Lines before `since` are skipped until
    /// one is not: after that none is, as times need not rise from line to line. The
    /// first line after `until` ends the output.
    fn admit(&mut self, at: u64) -> Admit {
        let at = i128::from(at);
        if let Some(since) = self.since {
            if at < since {
                return Admit::Skip;
            }
            self.since = None;
        }
        if self.until.is_some_and(|until| at > until) {
            return Admit::Stop;
        }
        Admit::Pass
    }
}

/// Waits until one of `fds` is readable, or hung up; which are.
fn wait_readable(fds: &[BorrowedFd<'_>; 3]) -> io::Result<[bool; 3]> {
    let mut polled = fds.map(|fd| libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    });
    loop {
        // SAFETY: poll(2) on three pollfds of descriptors the caller holds open.
        let n = unsafe { libc::poll(polled.as_mut_ptr(), 3, -1) };
        if n >= 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(polled.map(|p| p.revents != 0))
}

/// What `docker logs` puts before each line: its time with `-t`, and with `--details` its
/// attributes and a space.
#[derive(Clone, Copy)]
struct Shown {
    stamps: bool,
    details: bool,
}

/// A time in nanoseconds since the Unix epoch as RFC 3339 in UTC with nine digits of
/// fraction, as `docker logs -t` prints it (moby jsonmessage.RFC3339NanoFixed).
pub(super) fn rfc3339_nano(ns: u64) -> String {
    let secs = ns / 1_000_000_000;
    let (days, day) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant, "chrono-Compatible Low-Level
    // Date Algorithms", days_from_civil's inverse).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:09}Z",
        day / 3600,
        day % 3600 / 60,
        day % 60,
        ns % 1_000_000_000
    )
}

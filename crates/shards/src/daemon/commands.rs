//! Container commands the daemon runs for its clients (`shards list vms`, `wait`, `logs`, `rm`,
//! `stop`, `kill`), answering on the client's stdout and stderr as `docker` answers
//! (docs/design/architecture.md D27). The client has read the command line already
//! (shards_cmdline); the daemon reads it again by the same rules, and does what dockerd
//! would, in the order and with the words the Docker CLI and dockerd use.

use std::io::Read as _;
use std::io::{self, Write as _};
use std::sync::MutexGuard;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use shards_cmdline::commands::{
    self, COMMIT, CONTAINER_INSPECT, CONTAINER_PRUNE, DIFF, EVENTS, EXPORT, HISTORY, IMAGE_INSPECT,
    IMAGE_PRUNE, IMAGES, INFO, KILL, LOAD, LOGS, PAUSE, PORT, PS, PULL, PUSH, READ_PORTS, READ_PROCESSES,
    RENAME, RM, RMI, SAVE, STATS, STOP, SYSTEM_DF, SYSTEM_PRUNE, TAG, TOP, UNPAUSE, WAIT,
};
use shards_cmdline::flags::{self, Outcome, Parsed};
use shards_cmdline::{gotime, width};
use shards_ipc::Stream as UnixStream;
use shards_ipc::fd::{AsFd as _, OwnedFd};
// What Windows' daemon waits on together is its own streams (D195), which shards_ipc
// watches.
#[cfg(windows)]
use shards_ipc::Poller;
use shards_ipc::kind;
#[cfg(unix)]
use shards_vmm::platform::Poller;

use super::logs::{self, LogFile, Piece, Reader};
use super::{Daemon, RunState, STOP_GRACE, lock};
use crate::containers::{Container, Registry, Removal, State as Life, now};
use crate::spec::{LOG_STDERR, LOG_STDOUT};

use super::ends::{End, Followed};

/// How a microVM [`end_all`](Daemon::end_all) ended came to its end: it had already,
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

/// What `stop`, `kill` and `rm` make of one argument before any microVM is ended: an
/// answer now, or a microVM to end, with its signal and grace, and what to answer once
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

/// What a step answers once its microVM has ended, or would not.
pub(super) type Then<'a> = Box<dyn FnOnce(bool) -> Result<bool, String> + 'a>;

/// Why `exec --for NAME` runs nothing (D243), each with the name given: a member's command
/// is confined as its member is, so it takes no privilege and no user of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MemberExec {
    NoMember(super::devices::NoMember),
    Privileged(String),
    User(String),
}

impl std::fmt::Display for MemberExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemberExec::NoMember(e) => write!(f, "{e}"),
            MemberExec::Privileged(n) => write!(
                f,
                "--for {n}: a command run in it is confined as it is, so not --privileged"
            ),
            MemberExec::User(n) => write!(
                f,
                "--for {n}: a command run in it runs as it does, so takes no --user"
            ),
        }
    }
}

/// What `exec --for` asks that its member has not (D243): privileges, or a user, of its
/// own. Its init refuses both again.
fn held_as_its_member(exec: &shards_ipc::Exec) -> Result<(), MemberExec> {
    if exec.privileged {
        return Err(MemberExec::Privileged(exec.member.clone()));
    }
    if !exec.user.is_empty() {
        return Err(MemberExec::User(exec.member.clone()));
    }
    Ok(())
}

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

/// A signal as clients name it, read as moby/sys/signal's signal.go ParseSignal reads it
/// (the meaning `kill -s` and `--stop-signal` scripts give it): a number other than 0, or a
/// name in any case, with or without `SIG`; `RTMIN+n` and `RTMAX-n` for n up to 15 and 14.
pub(super) fn parse_signal(given: &str) -> Result<i64, String> {
    let invalid = || format!("{given} is no signal: give its name (TERM, SIGKILL, RTMIN+3) or its number");
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
    /// `shards exec vm`: runs a command in a running microVM, as `docker exec` does. Its
    /// spec starts from the microVM's run, as dockerd composes an exec's (moby
    /// daemon/exec.go, ContainerExecCreate): the run's environment then `-e`'s, its user
    /// and working directory unless `-u` and `-w` say otherwise, its host name, and TERM
    /// for a terminal of its own. It goes to the microVM's VM with the client's
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
        let [stdin, stdout, stderr] = match <&[OwnedFd; 3]>::try_from(message.fds.as_slice()) {
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
            return refuse(
                "-it: stdin is not a terminal, so it cannot be the command's: leave out -t, or run it from a terminal",
            );
        }
        // One being started is seen through, as `docker exec` finds it started or not.
        self.await_start(&id);
        // What the exec needs of the run, taken out of `runs` before its inbox is locked:
        // its follower holds the inbox while it ends the run, which takes `runs`.
        if lock(&self.paused).contains(&id) {
            return refuse(&format!(
                "shards: {} is paused: unpause it to run a command in it",
                exec.container
            ));
        }
        let (base, socket, inbox) = match lock(&self.runs).get(&id) {
            Some(RunState::Tracked(run)) => (run.base.clone(), run.socket.clone(), run.inbox.clone()),
            _ => {
                return refuse(&format!("shards: {} is not running", exec.container));
            }
        };
        // In one of its image's agents or harnesses (D243): as that member runs, so with no
        // user or privilege of its own, and nothing of the run's own command's; its init
        // refuses both again.
        let member = match self.exec_member(&id, &exec) {
            Ok(member) => member,
            Err(e) => return refuse(&format!("shards: {e}")),
        };
        // Beside its image's agents and harnesses, an exec is held to what its run was
        // (D115): not privileged, and none of their IDs; its init refuses both again.
        if let Some(a) = base.agentfile.as_ref().filter(|a| a.domains > 0) {
            let refused = if exec.privileged {
                Err(shards_abi::run::beside::privileged("exec"))
            } else {
                a.ids(&exec.user, &[])
            };
            if let Err(e) = refused {
                return refuse(&format!("shards: {e}"));
            }
        }
        let base = &base.options;
        let in_member = member.is_some();
        let options = crate::spec::Options {
            argv: exec.cmd,
            env: if in_member { Vec::new() } else { base.env.clone() },
            exec_env: exec.env,
            // A member's command works where its member does unless it names a place.
            workdir: if exec.workdir.is_empty() && !in_member {
                base.workdir.clone()
            } else {
                exec.workdir
            },
            user: if exec.user.is_empty() && !in_member {
                base.user.clone()
            } else {
                exec.user
            },
            hostname: base.hostname.clone(),
            interactive: exec.interactive,
            tty: exec.tty.map(|(rows, cols)| shards_abi::run::Size { rows, cols }),
            terms: exec.terms,
            member: in_member,
        };
        let mut spec = match crate::spec::spec(&options, |_| None) {
            Ok(spec) => spec,
            Err(e) => return refuse(&format!("shards: {e}")),
        };
        // `--privileged`: every capability (moby daemon/exec_linux.go); the rest of its
        // process is the workload's.
        if exec.privileged {
            let all = (1u64 << shards_abi::run::CAP_NAMES.len()) - 1;
            spec.setup = vec![format!("caps={all}").into_bytes()];
        }
        if let Some(n) = member {
            spec.setup = vec![shards_abi::run::members::exec_in(n)];
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
                return refuse(&format!("shards: holding the connection: {e}"));
            }
        };
        let handed = match shards_ipc::handed(conn) {
            Ok(handed) => handed,
            Err(e) => return refuse(&format!("shards: handing on the connection: {e}")),
        };
        // An Engine API exec said its `exec_create` as it was made, with the ID its client
        // has; `shards exec vm` makes and starts one at once.
        let exec_id = if exec.id.is_empty() {
            self.exec_events(&id, &options.argv)
        } else {
            self.exec_event(&id, &options.argv, "exec_start", &exec.id);
            Some(exec.id)
        };
        {
            let mut inbox = lock(&inbox);
            inbox.execs_in_flight.push((number, held));
            if let Some(exec_id) = exec_id {
                inbox.exec_ids.push((number, exec_id));
            }
            // Its client waits on its end: the listener looks at its microVM until then.
            if !exec.detach {
                inbox.waited_execs.push(number);
            }
        }
        if !exec.detach {
            self.listener_wake.wake();
        }
        let fds = [handed.as_fd(), stdin.as_fd(), stdout.as_fd(), stderr.as_fd()];
        if let Err(e) = socket.send(kind::EXEC_RUN, &payload, &fds) {
            let mut held = lock(&inbox);
            held.execs_in_flight.retain(|(n, _)| *n != number);
            held.exec_ids.retain(|(n, _)| *n != number);
            held.waited_execs.retain(|n| *n != number);
            drop(held);
            refuse(&format!("shards: the microVM: {e}"));
        }
    }

    /// The member `exec --for` runs its command in (D243), where it names one: an agent or
    /// harness of the microVM's image that runs a command of its own, and the exec asking
    /// nothing that member has not.
    fn exec_member(&self, id: &str, exec: &shards_ipc::Exec) -> Result<Option<u32>, MemberExec> {
        if exec.member.is_empty() {
            return Ok(None);
        }
        held_as_its_member(exec)?;
        self.running_member(id, &exec.member)
            .map(Some)
            .map_err(MemberExec::NoMember)
    }

    /// `shards attach`: the client attached to a running microVM's command, as dockerd's
    /// ContainerAttach attaches one (moby daemon/attach.go), refused as docker/cli refuses
    /// one it has inspected (attach.go inspectContainerAndCheckState). The client goes to
    /// the microVM's VM, which answers it, as an exec's does.
    pub(super) fn attach(&self, message: &shards_ipc::Message, conn: &UnixStream) {
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
        let Some(attach) = shards_ipc::Attach::decode(&message.payload) else {
            return refuse("shards: a malformed attach");
        };
        let [stdin, stdout, stderr] = match <&[OwnedFd; 3]>::try_from(message.fds.as_slice()) {
            Ok(fds) => fds,
            Err(_) => return refuse("shards: an attach without the client's stdio"),
        };
        let id = match self.resolve(&attach.container) {
            Ok(id) => id,
            Err(e) => return refuse(&e),
        };
        self.await_start(&id);
        // A restarting microVM is not running: between its runs it has no command to attach.
        if self.is_restarting(&id) {
            return refuse("cannot attach to a restarting container, wait until it is running");
        }
        if lock(&self.paused).contains(&id) {
            return refuse("cannot attach to a paused container, unpause it first");
        }
        let (base, socket, inbox) = match lock(&self.runs).get(&id) {
            Some(RunState::Tracked(run)) => (run.base.clone(), run.socket.clone(), run.inbox.clone()),
            _ => return refuse("cannot attach to a stopped container, start it first"),
        };
        // Its stdin goes in where the microVM's command reads one (Config.OpenStdin).
        let flags = if attach.stdin && base.options.interactive {
            shards_ipc::ATTACH_STDIN
        } else {
            0
        };
        let number = self.next_exec.fetch_add(1, Ordering::Relaxed);
        let mut payload = number.to_be_bytes().to_vec();
        payload.push(flags);
        // Held until the VM says it has it (`EXEC_TAKEN`), or its run ends.
        let held = match conn.try_clone() {
            Ok(held) => held,
            Err(e) => {
                return refuse(&format!("shards: holding the connection: {e}"));
            }
        };
        let handed = match shards_ipc::handed(conn) {
            Ok(handed) => handed,
            Err(e) => return refuse(&format!("shards: handing on the connection: {e}")),
        };
        lock(&inbox).execs_in_flight.push((number, held));
        let fds = [handed.as_fd(), stdin.as_fd(), stdout.as_fd(), stderr.as_fd()];
        if let Err(e) = socket.send(kind::ATTACH_RUN, &payload, &fds) {
            lock(&inbox).execs_in_flight.retain(|(n, _)| *n != number);
            refuse(&format!("shards: the microVM: {e}"));
        }
    }

    /// Logs exec `exec_id`'s `action` of `argv` in microVM `id` as dockerd does (moby
    /// daemon/exec.go: `exec_create: ENTRYPOINT ARGS`, then `exec_start: …`, each with its
    /// ID).
    pub(super) fn exec_event(&self, id: &str, argv: &[String], action: &str, exec_id: &str) {
        let (entry, args) = argv.split_first().map_or(("", &[][..]), |(e, a)| (e.as_str(), a));
        let line = format!("{entry} {}", args.join(" "));
        self.container_event(
            id,
            format!("{action}: {line}"),
            &[("execID", exec_id.to_string())],
        );
    }

    /// Logs exec `argv`'s creation and start in microVM `id`, as `shards exec vm` makes and
    /// starts one at once ([`exec_event`](Self::exec_event)), and returns its ID, for its
    /// `exec_die`; none if no ID could be made.
    pub(super) fn exec_events(&self, id: &str, argv: &[String]) -> Option<String> {
        let exec_id = crate::containers::new_id().ok()?;
        for action in ["exec_create", "exec_start"] {
            self.exec_event(id, argv, action, &exec_id);
        }
        Some(exec_id)
    }

    /// Runs command `argv` on microVMs for a client, answering on `reply`, and returns its
    /// exit status, the client being `asker`.
    pub(super) fn command(&self, argv: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        // `shards copy vm`'s steps, which its client takes one at a time (cli/cp.rs).
        if let Some((first, rest)) = argv.split_first()
            && first == COPY_STEP
        {
            return self.copy_step(rest, asker, reply);
        }
        // `shards move vm`'s steps (D176 part 6, cli/moves.rs).
        if let Some((first, rest)) = argv.split_first()
            && first == shards_ipc::MOVE_STEP
        {
            return self.move_step(rest, asker, reply);
        }
        // A build's names, each one's microVM published to the local store (D216).
        if let Some((first, names)) = argv.split_first()
            && first == shards_ipc::PUBLISH_STEP
        {
            for name in names {
                crate::pull::publish_name(&self.home, name, &asker.registry_env);
            }
            return 0;
        }
        // The microVMs a client's names reach: an image's name, its running ones (D216).
        if let Some((first, names)) = argv.split_first()
            && first == shards_ipc::NAMES_STEP
        {
            for name in self.by_image(names, true) {
                reply.out(&name);
            }
            return 0;
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
        // What any client has seen of its run, an answer that reads microVMs includes:
        // `images` and `rmi` read which use an image too.
        if ![&TAG, &IMAGE_INSPECT, &SAVE, &LOAD, &PULL, &PUSH]
            .iter()
            .any(|c| std::ptr::eq(command, *c))
        {
            self.settle();
        }
        // shards' grammar's `--convert`: each image the command names that is not yet a
        // microVM here is made one first, pulled and converted as `run` makes one.
        if parsed.bool("convert") {
            self.convert_named(command, &parsed, asker, reply);
        }
        if std::ptr::eq(command, &PS) {
            self.ps(&parsed, asker, reply)
        } else if std::ptr::eq(command, &WAIT) {
            self.wait(&self.by_image(&parsed.args, true), asker.client, reply)
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
        } else if std::ptr::eq(command, &READ_PORTS) {
            self.read_ports(&parsed.args, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::SAVE_VM) {
            self.save_vm(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::CHECKPOINT_CREATE) {
            self.checkpoint_create(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::CHECKPOINT_LS) {
            self.checkpoint_ls(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::CHECKPOINT_RM) {
            self.checkpoint_rm(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::SNAPSHOT_LS) {
            self.list_snapshots(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::SNAPSHOT_RM) {
            self.remove_snapshots(&parsed, reply)
        } else if std::ptr::eq(command, &READ_PROCESSES) {
            self.read_processes(&parsed.args, false, asker.client, reply)
        } else if std::ptr::eq(command, &commands::READ_METRICS) {
            self.read_metrics(&parsed.args, asker, reply)
        } else if std::ptr::eq(command, &commands::WATCH_PROCESSES) {
            self.read_processes(&parsed.args, true, asker.client, reply)
        } else if std::ptr::eq(command, &IMAGES) {
            self.images(&parsed, asker, reply)
        } else if std::ptr::eq(command, &TAG) {
            self.tag(&parsed.args, asker.styled(), &asker.registry_env, reply)
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
        } else if std::ptr::eq(command, &shards_cmdline::commands::BUILDER_PRUNE) {
            self.builder_prune(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::BUILDER_DU) {
            self.builder_du(&parsed, asker, reply)
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
            self.container_inspect(&parsed, asker.styled(), reply, parsed.bool("convert"))
        } else if std::ptr::eq(command, &SAVE) {
            self.save(&parsed.args, parsed.many("platform"), asker, reply)
        } else if std::ptr::eq(command, &LOAD) {
            self.load(parsed.many("platform"), asker, reply)
        } else if std::ptr::eq(command, &PULL) {
            self.pull(&parsed, asker, reply)
        } else if std::ptr::eq(command, &PUSH) {
            self.push(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::IMPORT) {
            self.import(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::UPDATE) {
            self.update(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_CREATE) {
            self.network_create(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_LS) {
            self.network_ls(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_INSPECT) {
            self.network_inspect(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_RM) {
            self.network_rm(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_PRUNE) {
            self.network_prune(&parsed, asker, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_CONNECT) {
            self.network_connect(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::NETWORK_DISCONNECT) {
            self.network_disconnect(&parsed, reply)
        } else if std::ptr::eq(command, &shards_cmdline::commands::VOLUME_CREATE) {
            self.volume_create(&parsed, asker.styled(), &asker.whose, reply)
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

    /// `shards export vm CONTAINER` (moby daemon/export.go, ContainerExport): the
    /// microVM's files as a tar archive, written by its init straight to where the
    /// client asked (`-o`'s file or its stdout), through no copy here: a stopped one's in
    /// a VM visiting them (visit.rs).
    fn export(&self, args: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        let (Some(given), Some(out)) = (args.first(), asker.files.first()) else {
            reply.err("shards: export: the client sent nowhere to write");
            return 1;
        };
        let refuse = |said: String| {
            reply.err(&format!("shards: {said}"));
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
                "microVM {id}: its files cannot be reached (see the daemon's log)"
            ));
        }
        if lock(&self.paused).contains(&id) {
            return refuse(format!(
                "microVM {id} is paused: its files can be exported once it is unpaused"
            ));
        }
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::EXPORT,
            ..Default::default()
        };
        let pieces = std::fs::File::open(crate::os::NULL).and_then(|null| Ok((null, std::io::pipe()?)));
        let (null, (mut said, into)) = match pieces {
            Ok(p) => p,
            Err(e) => return refuse(format!("Error exporting container {given}: {e}")),
        };
        let ended = self.exec_on(
            &id,
            &spec,
            false,
            [null.as_fd(), out.as_fd(), into.as_fd()],
            super::health::Wait::Ending,
        );
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

    /// A step of `shards copy vm` in microVM `container`, as dockerd's archive routes take
    /// them (moby daemon/archive.go): `stat PATH`, its stat as a JSON line; `archive
    /// PATH`, a tar archive of it to the asker's file; `extract PATH UIDGID OVERWRITE`,
    /// the archive the asker's file holds unpacked there, owned by the microVM's user
    /// with `UIDGID` 1. Done in its microVM (init copy.rs): a stopped one's in a VM
    /// visiting its files (visit.rs), which keeps what was copied in as it ends.
    fn copy_step(&self, args: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        let (Some(step), Some(given), Some(path)) = (args.first(), args.get(1), args.get(2)) else {
            return 1;
        };
        let refuse = |said: String| {
            reply.err(&format!("shards: {said}"));
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
                "microVM {id}: its files cannot be reached (see the daemon's log)"
            ));
        }
        if lock(&self.paused).contains(&id) {
            return refuse(format!(
                "microVM {id} is paused: its files can be copied once it is unpaused"
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
            // A read-only root, or volume, takes nothing: refused by init where the path
            // resolves, on the mounts its command sees (copy.rs, `writable`).
            // `-a`: everything the microVM's user's (archive_tarcopyoptions_unix.go).
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
            match self.exec_answered(&id, &spec, 1 << 16) {
                Ok(q) => (q.status, q.output, Vec::new()),
                Err(e) => return refuse(e.to_string()),
            }
        } else {
            let Some(file) = asker.files.first() else {
                return refuse("shards: cp: the client sent no file".into());
            };
            let pieces = std::fs::File::open(crate::os::NULL).and_then(|null| Ok((null, std::io::pipe()?)));
            let (null, (mut errs, into)) = match pieces {
                Ok(p) => p,
                Err(e) => return refuse(e.to_string()),
            };
            let stdio = if kind == builtin::EXTRACT {
                [file.as_fd(), null.as_fd(), into.as_fd()]
            } else {
                [null.as_fd(), file.as_fd(), into.as_fd()]
            };
            let ended = self.exec_on(
                &id,
                &spec,
                kind == builtin::EXTRACT,
                stdio,
                super::health::Wait::Ending,
            );
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
                format!("microVM {id}: the copy failed")
            } else {
                why
            }),
        }
    }

    /// The user microVM `id` runs as: its run's, else its image's.
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

    /// `shards read changes CONTAINER` (moby daemon/changes.go, ContainerChanges): what the
    /// microVM changed of its image's files, as its init finds them (init changes.rs),
    /// one `KIND PATH` a line as docker/cli prints them (diff.go): a stopped one's in a VM
    /// visiting its files (visit.rs).
    fn diff(&self, args: &[String], styled: bool, reply: &Reply<'_>) -> u8 {
        let Some(given) = args.first() else {
            return 1;
        };
        let refuse = |said: String| {
            reply.err(&format!("shards: {said}"));
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
                "microVM {id}: its files cannot be reached (see the daemon's log)"
            ));
        }
        if lock(&self.paused).contains(&id) {
            return refuse(format!(
                "microVM {id} is paused: its changes can be read once it is unpaused"
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
            Ok(_) => refuse(format!("microVM {id}: its changes could not all be read")),
            Err(e) => refuse(format!("microVM {id}: reading its changes: {e}")),
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
            reply.err(&format!("shards: {said}"));
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
        let table = match self.top_table(given, &id, &joined, asker.utc_offset) {
            Ok(t) => t,
            Err(e) => return refuse(e),
        };
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

    /// The processes of microVM `id` (named `given`) as `ps` lays them out with options
    /// `joined`, in a zone `utc_offset` seconds east of UTC, from what its init reads of
    /// them in `/proc`; or why not.
    pub(super) fn top_table(
        &self,
        given: &str,
        id: &str,
        joined: &str,
        utc_offset: i32,
    ) -> Result<super::top::Table, String> {
        ps_args_allowed(joined)?;
        self.await_start(id);
        if !self.running(id) {
            return Err(format!("{given} is not running"));
        }
        if lock(&self.paused).contains(id) {
            return Err(format!(
                "{given} is paused: its processes can be listed once it is unpaused"
            ));
        }
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::PROCESSES,
            ..Default::default()
        };
        let quiet = match self.exec_answered(id, &spec, top_limit()) {
            Ok(q) if q.status == Some(0) && !q.more => q,
            Ok(_) => return Err(format!("{given}: its processes could not be read")),
            Err(e) => return Err(format!("{given}: reading its processes: {e}")),
        };
        let args: Vec<&str> = joined.split(' ').collect();
        let table =
            super::top::ps(&quiet.output, &args, utc_offset).and_then(|out| super::top::table(&out))?;
        self.container_event(id, "top", &[]);
        Ok(table)
    }

    /// `shards watch vms [CONTAINER...]` (docker/cli cli/command/container/stats.go): what each
    /// microVM takes of the host, measured from its VM process each second: CPU as the
    /// share of one CPU its process had over the second, memory as its resident size
    /// against the microVM's. Streamed until the client goes, or once with `--no-stream`.
    /// What only the guest knows (network and block I/O, PIDs) is `--`, the mark the
    /// stats table reads as missing (docker/cli cli/command/container/formatter_stats.go).
    fn stats(&self, parsed: &Parsed, asker: &Asker, reply: &Reply<'_>) -> u8 {
        let all = parsed.bool("all");
        let trunc = !parsed.bool("no-trunc");
        // Those named, each found as every command finds one; else every one listed.
        let mut named = Vec::new();
        let mut given_as: Vec<(String, String)> = Vec::new();
        for given in &parsed.args {
            match self.resolve_held(lock(&self.containers), given).1 {
                Ok(id) => {
                    given_as.push((id.clone(), given.clone()));
                    named.push(id);
                }
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
                    // Whether it runs, so is sampled: a joiner's (D119) from its own cgroup,
                    // as init's STATS reads it in the joiner's context.
                    let pid = match runs.get(&c.id) {
                        Some(RunState::Tracked(t)) => Some(t.vm.id()),
                        _ => None,
                    };
                    (c.id.clone(), c.name.clone(), pid)
                })
                .collect()
        };
        // Its Container: as it was asked for, else its full ID (docker/cli stats.go).
        let named_as = |id: &str| {
            given_as
                .iter()
                .find(|(i, _)| i == id)
                .map_or_else(|| id.to_string(), |(_, g)| g.clone())
        };
        // Each running one's guest sampled at once, as dockerd's collector samples each
        // container's cgroup (daemon/stats/collector.go): STATS, once a second.
        // A paused one's VM cannot answer: its last sample stands, taken as it was
        // paused, its CPU idle meanwhile.
        let take = |targets: &[(String, String, Option<u32>)]| -> Vec<Option<Usage>> {
            let paused = lock(&self.paused).clone();
            std::thread::scope(|scope| {
                let asked: Vec<_> = targets
                    .iter()
                    .map(|(id, _, pid)| {
                        pid.map(|_| {
                            let paused = paused.contains(id);
                            scope.spawn(move || {
                                if paused {
                                    lock(&LAST_USAGE).get(id).cloned()
                                } else {
                                    self.usage_of(id)
                                }
                            })
                        })
                    })
                    .collect();
                asked
                    .into_iter()
                    .map(|h| h.and_then(|h| h.join().ok().flatten()))
                    .collect()
            })
        };
        let mut last: std::collections::HashMap<String, (Usage, Instant)> = std::collections::HashMap::new();
        // The rows, and whether each running one's CPU was measured: a sample after one
        // before it. Where the one before was not taken, its 0% would be no measure.
        let mut sample = || -> (Vec<shards_cmdline::format::stats::Stats>, bool) {
            let targets = targets();
            let used = take(&targets);
            let mut measured = true;
            let rows = targets
                .into_iter()
                .zip(used)
                .map(|((id, name, pid), u)| {
                    let container = named_as(&id);
                    let Some(u) = u else {
                        // Not running: nothing to sample, zeros (a stopped one); one whose
                        // sample failed: invalid, as the CLI shows "--".
                        return shards_cmdline::format::stats::Stats {
                            container,
                            name,
                            id,
                            invalid: pid.is_some(),
                            ..Default::default()
                        };
                    };
                    let now = Instant::now();
                    let before = last.insert(id.clone(), (u.clone(), now));
                    measured &= before.is_some();
                    let elapsed = before.as_ref().map(|(_, at)| now.duration_since(*at));
                    u.stats(before.as_ref().map(|(b, _)| b).zip(elapsed), container, name, id)
                })
                .collect();
            (rows, measured)
        };
        // The first sample is a baseline: CPU is measured over the second after it. With
        // `--no-stream`, a running one whose baseline was not taken is sampled once more a
        // second later, the sample just taken its baseline, so that a one-off row is
        // measured as a streamed one is (moby 3f38b15 daemon/stats.go answers a one-off
        // request with two samples too): then each row is measured, or, its last sample
        // not taken, unmeasured ("--").
        let mut resampled = false;
        sample();
        let source = match parsed.string("format") {
            "" => shards_cmdline::format::TABLE,
            f => f,
        };
        let format = shards_cmdline::format::stats::format(source);
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let (rows, measured) = sample();
            if !measured && parsed.bool("no-stream") && !resampled {
                resampled = true;
                continue;
            }
            let taken = if asker.styled() && parsed.string("format").is_empty() {
                let mut sheet = shards_ipc::Sheet::new("stats");
                for r in &rows {
                    sheet.record(&[
                        ("running", (!r.invalid && r.memory_limit > 0.0).to_string()),
                        ("id", r.id.get(..12).unwrap_or(&r.id).to_string()),
                        ("name", r.name.clone()),
                        ("cpu", format!("{:.2}", r.cpu_percentage)),
                        ("mem", shards_cmdline::format::units_bytes_size(r.memory)),
                        ("limit", shards_cmdline::format::units_bytes_size(r.memory_limit)),
                        ("share", format!("{:.2}", r.memory_percentage)),
                    ]);
                }
                reply.sheet_taken(&sheet)
            } else {
                let clock = shards_cmdline::format::Clock {
                    now: 0,
                    zone: &shards_cmdline::format::utc,
                };
                let ctx = shards_cmdline::format::Context {
                    format: &format,
                    trunc,
                    east_asian: asker.east_asian,
                    clock: &clock,
                };
                let mut text = String::new();
                // On a terminal each table replaces the last, the screen cleared and homed
                // (ECMA-48 ED and CUP), so that the live view stays in place.
                if asker.terminal && !parsed.bool("no-stream") {
                    text.push_str("\x1b[2J\x1b[H");
                }
                if let Err(e) = shards_cmdline::format::stats::write(&ctx, &rows, &mut text) {
                    reply.err(&format!("template parsing error: {e}"));
                    return 1;
                }
                reply.bytes(LOG_STDOUT, text.as_bytes()).is_ok()
            };
            if !taken || parsed.bool("no-stream") {
                return 0;
            }
        }
    }

    /// `shards rename vm CONTAINER NEW_NAME` (moby daemon/rename.go, ContainerRename): the
    /// new name checked as a new microVM's is, refused if it is the old one or held; the
    /// old one free at once; the record written soon.
    fn rename(&self, args: &[String], reply: &Reply<'_>) -> u8 {
        let (Some(given), Some(new)) = (args.first(), args.get(1)) else {
            return 1;
        };
        let refuse = |said: String| {
            reply.err(&format!("shards: {said}"));
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
                return refuse(e.strip_prefix("shards: ").unwrap_or(&e).to_string());
            }
        };
        let old = {
            let mut registry = lock(&self.containers);
            let Some(old) = registry.get(&id).map(|c| c.name.clone()) else {
                return refuse(format!("No such container: {given}"));
            };
            if old == new {
                return refuse(format!("{given} is named {new} already"));
            }
            if let Some(holder) = registry.name_taken(&new) {
                let holder = holder.id.clone();
                return refuse(format!(
                    "{new}: the name is in use by microVM {}: remove or rename it to use the name",
                    holder.get(..12).unwrap_or(&holder)
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
    /// published port of a running microVM as `PORT/PROTO -> HOST:PORT`, or with PORT
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
        // dockerd's NetworkSettings.Ports: a running microVM's bindings.
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

    /// `shards read ports MICROVM` (D122): each published port of a running microVM and
    /// what it reaches, the agent or harness its image's Agentfile names for it (at its own
    /// port, where it maps one), else the run's own command.
    fn read_ports(&self, args: &[String], reply: &Reply<'_>) -> u8 {
        use shards_agentfile::agentfile::TargetKind;
        let Some(reference) = args.first() else {
            return 1;
        };
        let id = match self.resolve(reference) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        let (ports, made) = match lock(&self.containers).get(&id) {
            Some(c) if c.state == Life::Running => (c.ports.clone(), c.engine_made.clone()),
            _ => (Vec::new(), None),
        };
        // A joiner its microVM's engine made, a member of that microVM's switch (D181):
        // who reaches each port it declares or listens on.
        if let Some(made) = made.filter(|m| !m.addresses.is_empty()) {
            return self.member_ports(&id, &made, &ports, reply);
        }
        let receivers = match lock(&self.runs).get(&id) {
            Some(RunState::Tracked(run)) => run
                .base
                .agentfile
                .as_ref()
                .map(|a| a.receivers.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut rows: Vec<[String; 3]> = ports
            .iter()
            .filter_map(|p| {
                let at = std::net::SocketAddr::new(p.ip?, p.public);
                let proto = if p.proto == "udp" { 17 } else { 6 };
                // Each member the port is for; of several, the one listening takes each
                // connection (D122).
                let each: Vec<String> = crate::agentfile::receiving(&receivers, proto, p.private)
                    .map(|(r, own)| {
                        let kind = match r.kind {
                            TargetKind::Agent => "agent",
                            TargetKind::Harness => "harness",
                        };
                        let name = String::from_utf8_lossy(&r.name);
                        if own == p.private {
                            format!("{kind} {name}")
                        } else {
                            format!("{kind} {name}, at {own}/{}", p.proto)
                        }
                    })
                    .collect();
                let reaches = match each.as_slice() {
                    [] => "the command".to_string(),
                    [one] => one.clone(),
                    several => format!("the one listening of {}", several.join("; ")),
                };
                Some([format!("{}/{}", p.private, p.proto), at.to_string(), reaches])
            })
            .collect();
        rows.extend(self.managed.published(&id));
        rows.sort_by(|[a, at_a, _], [b, at_b, _]| {
            shards_cmdline::ports::natural_compare(a, b).then_with(|| at_a.cmp(at_b))
        });
        let (port, published) = rows
            .iter()
            .fold(("PORT".len(), "PUBLISHED".len()), |(p, a), [x, y, _]| {
                (p.max(x.len()), a.max(y.len()))
            });
        reply.out(&format!(
            "{:<port$}  {:<published$}  REACHES",
            "PORT", "PUBLISHED"
        ));
        for [p, at, reaches] in rows {
            reply.out(&format!("{p:<port$}  {at:<published$}  {reaches}"));
        }
        0
    }

    /// `shards read ports` of member `id` of its microVM's switch (D181): each port it
    /// declares or listens on, where it is published, and who reaches it there: the
    /// command where it publishes it, its peers on each of its networks whose members are
    /// peers where it declares it, and no one where it listens without declaring it, with
    /// what would.
    fn member_ports(
        &self,
        id: &str,
        made: &crate::containers::EngineMade,
        ports: &[crate::containers::PortRecord],
        reply: &Reply<'_>,
    ) -> u8 {
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::LISTENS,
            ..Default::default()
        };
        let listening: Vec<(u16, String)> = match self.exec_answered(id, &spec, 1 << 16) {
            Ok(quiet) if quiet.status == Some(0) => String::from_utf8_lossy(&quiet.output)
                .lines()
                .filter_map(|l| {
                    let (proto, port) = l.split_once(' ')?;
                    Some((port.parse().ok()?, proto.to_string()))
                })
                .collect(),
            Ok(quiet) => {
                reply.err(&format!(
                    "shards: what {id} listens on: {}",
                    String::from_utf8_lossy(&quiet.output).trim()
                ));
                return 1;
            }
            Err(e) => {
                reply.err(&format!("shards: what {id} listens on: {e}"));
                return 1;
            }
        };
        // Its networks whose members are its peers.
        let peered: Vec<String> = {
            let registry = lock(&self.containers);
            let networks = registry
                .get(&made.of)
                .map(|p| p.engine_networks.clone())
                .unwrap_or_default();
            made.addresses
                .iter()
                .filter(|a| networks.iter().any(|n| n.name == a.network && n.peers))
                .map(|a| a.network.clone())
                .collect()
        };
        let mut seen: Vec<(u16, String)> = ports.iter().map(|p| (p.private, p.proto.clone())).collect();
        seen.extend(listening.iter().cloned());
        seen.sort();
        seen.dedup();
        let rows: Vec<[String; 3]> = seen
            .into_iter()
            .map(|(port, proto)| {
                let published: Vec<String> = ports
                    .iter()
                    .filter(|p| p.private == port && p.proto == proto)
                    .filter_map(|p| Some(std::net::SocketAddr::new(p.ip?, p.public).to_string()))
                    .collect();
                let declared = ports.iter().any(|p| p.private == port && p.proto == proto);
                let peers = (!peered.is_empty()).then(|| format!("its peers on {}", peered.join(", ")));
                let by = match (published.is_empty(), declared, peers) {
                    (false, _, Some(peers)) => format!("the command, and {peers}"),
                    (false, _, None) => "the command".to_string(),
                    (true, true, Some(peers)) => peers,
                    (true, true, None) => "no one: its networks make no peers of their members".to_string(),
                    (true, false, _) => {
                        "no one: it listens, and declares it not (EXPOSE, expose: or --expose would)"
                            .to_string()
                    }
                };
                [
                    format!("{port}/{proto}"),
                    if published.is_empty() {
                        "-".to_string()
                    } else {
                        published.join(", ")
                    },
                    by,
                ]
            })
            .collect();
        let (port_w, published_w) = rows
            .iter()
            .fold(("PORT".len(), "PUBLISHED".len()), |(p, a), [x, y, _]| {
                (p.max(x.len()), a.max(y.len()))
            });
        reply.out(&format!(
            "{:<port_w$}  {:<published_w$}  REACHED BY",
            "PORT", "PUBLISHED"
        ));
        for [p, at, by] in rows {
            reply.out(&format!("{p:<port_w$}  {at:<published_w$}  {by}"));
        }
        0
    }

    /// The ID of the microVM `reference` names: all of its ID, its name, or the start of
    /// its ID and of no other's (moby daemon/container.go, GetContainer).
    /// `args`, each name of no microVM but of an image some run from said as those
    /// microVMs' names: `shards stop vm ubuntu:latest` stops what runs from it. With
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
                // Neither: it fails as a name, in the words scripts match (`no_such`).
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
        // A microVM exists from its creation: one whose record is still being written is
        // waited for, not missed (seen 2026-10-02 under the parallel suite).
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
                    "shards: {reference} begins more than one microVM's ID: give more of it"
                )),
                (Some(only), None) => Ok(only.id.clone()),
                (None, _) => Err(format!("shards: No such container: {reference}")),
            }
        };
        (registry, found)
    }

    /// Whether the microVM with `id` runs: its run was handed over, and has not ended.
    pub(super) fn running(&self, id: &str) -> bool {
        matches!(lock(&self.runs).get(id), Some(RunState::Tracked(t)) if !t.visit)
    }

    /// Freezes the cgroup of joiner `id` (D119) in its provider's guest, or
    /// thaws it: init's FREEZE, waited for to its end, which comes as the joiner's
    /// processes have left the kernel work they were in, or as they end (its kill), with
    /// no deadline of the daemon's (init's `freeze`, PM M482).
    fn freeze(&self, id: &str, frozen: bool) -> Result<(), String> {
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::FREEZE,
            argv: vec![if frozen { b"1".to_vec() } else { b"0".to_vec() }],
            ..Default::default()
        };
        let mut said = Vec::new();
        let ended = self
            .exec_streamed(id, &spec, None, None, &mut |chunk| {
                let room = (1usize << 12).saturating_sub(said.len());
                said.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
                true
            })
            .map_err(|e| e.to_string())?;
        match ended.status {
            Some(0) => Ok(()),
            _ => Err(String::from_utf8_lossy(&said).trim().to_string()),
        }
    }

    /// Pauses the microVM of run `id` where it is (`frozen`), every vCPU held out of its
    /// guest, or lets it go on: its VM asked (`PAUSE`), and its answer (`PAUSED`) waited
    /// for, as a save's is. A VM that ends first did neither.
    pub(super) fn pause_vm(&self, id: &str, frozen: bool) -> Result<(), String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (inbox, socket) = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => (t.inbox.clone(), t.socket.clone()),
            _ => return Err("it is not running".into()),
        };
        let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (told, answer) = std::sync::mpsc::channel();
        {
            let mut inbox = lock(&inbox);
            if inbox.ended {
                return Err("it is not running".into());
            }
            inbox.pausing.push((number, told));
        }
        let ask = [&[u8::from(frozen)][..], &number.to_be_bytes()].concat();
        if let Err(e) = socket.send(shards_ipc::kind::PAUSE, &ask, &[]) {
            lock(&inbox).pausing.retain(|(n, _)| *n != number);
            return Err(format!("asking its microVM: {e}"));
        }
        answer
            .recv()
            .unwrap_or_else(|_| Err("its microVM stopped".into()))
    }

    /// What each vCPU of run `id`'s microVM has run of its guest, and what those it no longer
    /// has ran: its VM asked (`GUEST_TIME`), on a joiner's (D119) link its provider's, and its
    /// answer (`GUEST_TIMED`) waited for as long as a warm VM may take to say it has taken a
    /// run, as it answers on receipt too.
    pub(super) fn guest_times_of(&self, id: &str) -> Result<shards_vmm::vm::GuestTimes, String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (inbox, socket) = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => (t.inbox.clone(), t.socket.clone()),
            _ => return Err("it is not running".into()),
        };
        let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (told, answer) = std::sync::mpsc::channel();
        {
            let mut inbox = lock(&inbox);
            if inbox.ended {
                return Err("it is not running".into());
            }
            inbox.guest_times.push((number, told));
        }
        if let Err(e) = socket.send(shards_ipc::kind::GUEST_TIME, &number.to_be_bytes(), &[]) {
            lock(&inbox).guest_times.retain(|(n, _)| *n != number);
            return Err(format!("asking its microVM: {e}"));
        }
        let answered = answer.recv_timeout(super::TAKE_TIMEOUT);
        if answered.is_err() {
            lock(&inbox).guest_times.retain(|(n, _)| *n != number);
        }
        match answered {
            Ok(said) => said,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "its microVM did not answer in {} s",
                super::TAKE_TIMEOUT.as_secs()
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err("its microVM stopped".into()),
        }
    }

    /// Whether run `id` runs joined to another's network (D119): in its provider's
    /// microVM, keeping no layer of its own yet.
    pub(super) fn joined_run(&self, id: &str) -> bool {
        matches!(lock(&self.runs).get(id), Some(RunState::Tracked(t)) if t.joined.is_some())
    }

    /// Whether microVM `id` has a VM to read its files in: its run's, or a visit's.
    pub(super) fn reachable(&self, id: &str) -> bool {
        matches!(lock(&self.runs).get(id), Some(RunState::Tracked(_)))
    }

    /// Sends Linux signal `linux` to the command of the microVM with `id`; whether it
    /// could.
    ///
    /// A paused microVM is let go on once sent SIGKILL or its own stop signal, as
    /// dockerd resumes one (moby daemon/kill.go, killWithSignal): the signal waits in its
    /// socket and is read as it runs again. Other signals wait for `unpause`.
    pub(super) fn signal(&self, id: &str, linux: u32) -> bool {
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

    /// SIGKILL sent to the command of run `id` ([`signal`](Self::signal)): how long its
    /// teardown is given before it is looked at (this host's slice, D217: none while this
    /// home has heard no teardown, which is then followed by its liveness alone), and the
    /// teardown followed from then on, in its microVM where that is its own. A SIGKILL that
    /// cannot be sent leaves its guest nothing to end it by, nor to say its end on: no wait.
    fn kill_command(&self, id: &str) -> (Option<Duration>, Followed) {
        let own = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => t.joined.is_none().then(|| t.vm.id()),
            _ => None,
        };
        let teardown = Followed::of(own);
        let wait = if self.signal(id, 9) {
            self.ends.slice(End::Teardown)
        } else {
            Some(Duration::ZERO)
        };
        (wait, teardown)
    }

    /// Lets paused microVM `id` go on ([`pause_vm`](Self::pause_vm)); whether
    /// it could.
    fn thaw(&self, id: &str) -> bool {
        // A joiner's cgroup is thawed in its provider's guest (D119): its VM is its
        // provider's, which goes on as it did.
        let resumed = if self.joined_run(id) {
            self.freeze(id, false).is_ok()
        } else {
            self.pause_vm(id, false).is_ok()
        };
        lock(&self.paused).remove(id);
        resumed
    }

    /// `shards pause vm` and `unpause` (moby daemon/pause.go, unpause.go): each running
    /// microVM frozen whole where it is, every vCPU held out of its guest by its VMM,
    /// which needs no cgroup and holds on every hypervisor alike (WHP's
    /// host stops a process no documented way); or let go on. A frozen microVM runs none
    /// of its guest, keeps its memory, and hears what it is sent once it goes on, its VM
    /// answering its daemon throughout. Answers as the CLI does (pause.go): each name done,
    /// then the errors.
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
                let shown = reference.trim();
                if !self.running(&id) {
                    return Step::Now(Err(format!("shards: {shown} is not running")));
                }
                let paused = lock(&self.paused).contains(&id);
                if freeze {
                    if paused {
                        return Step::Now(Err(format!("shards: {shown} is paused already")));
                    }
                    // Its last sample, for `stats` to show while it cannot answer.
                    let _ = self.usage_of(&id);
                    // A joiner's VM is its provider's (D119): its own cgroup is frozen in
                    // the guest, as Docker freezes a container's, and the VM goes on.
                    let frozen = if self.joined_run(&id) {
                        self.freeze(&id, true)
                    } else {
                        self.pause_vm(&id, true)
                    };
                    return Step::Now(match frozen {
                        Ok(()) => {
                            lock(&self.paused).insert(id.clone());
                            self.container_event(&id, "pause", &[]);
                            Ok(true)
                        }
                        Err(e) => Err(format!("shards: cannot pause {shown}: {e}")),
                    });
                }
                if !paused {
                    return Step::Now(Err(format!("shards: {shown} is not paused")));
                }
                Step::Now(if self.thaw(&id) {
                    self.container_event(&id, "unpause", &[]);
                    Ok(true)
                } else {
                    Err(format!("shards: cannot unpause {shown}: it is not running"))
                })
            },
            reply,
        )
    }

    /// Runs `op` for each of `args`, in their order, then ends every microVM the steps
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
                    ("error", e.strip_prefix("shards: ").unwrap_or(&e).to_string())
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

    /// Ends every microVM's command of `targets` (its index, ID, signal and grace) at once,
    /// from one loop: the signal, then SIGKILL once its grace is up, then its VM once its
    /// guest's teardown has gone on no more for a slice, then giving up once the killed VM
    /// process has gone on with its end no more for a slice, each slice this host's own
    /// and each end heard kept by it ([`super::ends`], D217). A signal that cannot be sent
    /// kills its VM at once: its guest can be told nothing, nor tell its end. Each end is heard on a socket of its
    /// own, registered before anything is sent, so none is missed; `done` hears each
    /// index once, with whether it ended, as it does.
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
            /// Its end followed once its command is sent SIGKILL, and again once its VM
            /// is killed.
            followed: Followed,
            /// When it was sent SIGKILL, or its VM killed: how long its end took.
            since: Instant,
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
            // A signal that could not be sent went nowhere: its run's connection to its VM
            // is gone, which its guest says its end on too, so no end can come from the
            // guest, and its VM is killed at once.
            let (phase, deadline, followed) = if linux == 9 {
                let (wait, teardown) = self.kill_command(&id);
                (Phase::Killed, wait.and_then(after), teardown)
            } else if self.signal(&id, linux) {
                (Phase::Signalled, grace.and_then(after), Followed::of(None))
            } else {
                (Phase::Killed, after(Duration::ZERO), Followed::of(None))
            };
            going.push(Ending {
                index,
                id,
                waiter,
                told,
                phase,
                deadline,
                followed,
                since: Instant::now(),
            });
        }
        // Their ends, waited on together by their indices, however many: a wait of Windows'
        // takes 64 handles at most, where its poller takes any number, as Unix's poll(2) does.
        let poller = Poller::new().and_then(|poller| {
            for e in &going {
                poller.add(&e.told, e.index as u64)?;
            }
            Ok(poller)
        });
        let poller = match poller {
            Ok(poller) => poller,
            Err(e) => {
                super::log(format!("waiting for containers' ends: {e}"));
                // Nothing to wait on: what is left is given up, as its time would be.
                for gone in going.drain(..) {
                    self.forget_waiter(&gone.id, gone.waiter);
                    done(gone.index, false, How::Lost);
                }
                return;
            }
        };
        let mut ready = Vec::new();
        while !going.is_empty() {
            // Escalate whatever is due.
            let now = Instant::now();
            let mut i = 0;
            while let Some(e) = going.get_mut(i) {
                if e.deadline.is_some_and(|d| d <= now) {
                    match e.phase {
                        Phase::Signalled => {
                            let (wait, teardown) = self.kill_command(&e.id);
                            e.phase = Phase::Escalated;
                            e.deadline = wait.and_then(after);
                            (e.followed, e.since) = (teardown, Instant::now());
                        }
                        Phase::Killed | Phase::Escalated => {
                            if let Some(look) = e.followed.goes_on() {
                                // Its guest is still at its end: another slice.
                                super::log(format!(
                                    "container {}: its command not yet ended {:?} after SIGKILL, its microVM going on with its end: {look}",
                                    e.id,
                                    Duration::from_secs(e.since.elapsed().as_secs())
                                ));
                                e.deadline = self.ends.slice(End::Teardown).and_then(after);
                                i += 1;
                                continue;
                            }
                            // A joiner's VM is its provider's (D119), which its end must
                            // not take: its own SIGKILL again, on its own connection.
                            let (socket, vm) = match lock(&self.runs).get(&e.id) {
                                Some(RunState::Tracked(t)) if t.joined.is_none() => {
                                    super::log(format!(
                                        "container {}: its command not ended {:?} after it was signalled, its microVM gone on with nothing of its end for {:?}: its microVM killed",
                                        e.id,
                                        Duration::from_secs(e.since.elapsed().as_secs()),
                                        // Its deadline came: the home has a slice.
                                        self.ends.slice(End::Teardown).unwrap_or_default()
                                    ));
                                    if let Err(err) = t.vm.kill(shards_ipc::signal::SIGKILL) {
                                        super::log(format!("container {}: killing its microVM: {err}", e.id));
                                    }
                                    (None, Some(t.vm.id()))
                                }
                                Some(RunState::Tracked(t)) => (Some(t.socket.clone()), None),
                                _ => (None, None),
                            };
                            if let Some(socket) = socket {
                                let _ = socket.send(kind::SIGNAL, &9u32.to_be_bytes(), &[]);
                            }
                            e.phase = Phase::VmKilled;
                            (e.followed, e.since) = (Followed::of(vm), Instant::now());
                            e.deadline = self.ends.slice(End::Exit).and_then(after);
                        }
                        Phase::VmKilled => {
                            if let Some(look) = e.followed.goes_on() {
                                // Its host still ending it: another slice.
                                super::log(format!(
                                    "container {}: its microVM not yet ended {:?} after its kill, its process going on with its end: {look}",
                                    e.id,
                                    Duration::from_secs(e.since.elapsed().as_secs())
                                ));
                                e.deadline = self.ends.slice(End::Exit).and_then(after);
                                i += 1;
                                continue;
                            }
                            let gone = going.swap_remove(i);
                            let _ = poller.remove(&gone.told);
                            self.forget_waiter(&gone.id, gone.waiter);
                            // Its end may have come as it was given up.
                            let _ = gone.told.set_nonblocking(true);
                            let mut byte = [0u8; 1];
                            let ended = matches!((&gone.told).read(&mut byte), Ok(1));
                            if ended {
                                if let Err(e) = self.ends.heard(End::Exit, gone.since.elapsed()) {
                                    super::log(e);
                                }
                            } else {
                                super::log(format!(
                                    "microVM {}: no end heard {:?} after it was killed, its process gone on with nothing of its end for {:?}: {}",
                                    gone.id,
                                    Duration::from_secs(gone.since.elapsed().as_secs()),
                                    // Its deadline came: the home has a slice.
                                    self.ends.slice(End::Exit).unwrap_or_default(),
                                    self.unheard(&gone.id)
                                ));
                            }
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
            let timeout = going
                .iter()
                .filter_map(|e| e.deadline)
                .min()
                .map(|d| d.saturating_duration_since(Instant::now()));
            if let Err(e) = poller.wait(&mut ready, timeout) {
                super::log(format!("waiting for containers' ends: {e}"));
                // Nothing to wait on: what is left is given up, as its time would be.
                for gone in going.drain(..) {
                    self.forget_waiter(&gone.id, gone.waiter);
                    done(gone.index, false, How::Lost);
                }
                break;
            }
            // Those whose end came.
            for &token in &ready {
                let Some(k) = going.iter().position(|e| e.index as u64 == token) else {
                    continue;
                };
                let gone = going.swap_remove(k);
                let _ = poller.remove(&gone.told);
                let mut byte = [0u8; 1];
                let ended = matches!((&gone.told).read(&mut byte), Ok(1));
                if !ended {
                    self.forget_waiter(&gone.id, gone.waiter);
                }
                // What this host's ends take, kept for the next (D217).
                let kept = match gone.phase {
                    _ if !ended => Ok(()),
                    Phase::Killed | Phase::Escalated => self.ends.heard(End::Teardown, gone.since.elapsed()),
                    Phase::VmKilled => self.ends.heard(End::Exit, gone.since.elapsed()),
                    Phase::Signalled => Ok(()),
                };
                if let Err(e) = kept {
                    super::log(e);
                }
                done(
                    gone.index,
                    ended,
                    if ended { how(&gone.phase) } else { How::Lost },
                );
            }
        }
    }

    /// `shards wait vm`: for each microVM in turn, its exit code once it stops. Found and
    /// waited for under one hold of the records, so that the state found is the state
    /// waited on: an end in between, of a `--rm` microVM above all, is not
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

    /// `shards remove vm [-f]`: removes microVMs that have stopped; with `-f`, kills the ones
    /// still running first, and says nothing of those not there. The CLI trims `/` from
    /// both ends of each argument (docker/cli rm.go; moby daemon/delete.go). A microVM
    /// whose run no VM has yet been committed to has not started: it goes, and its run
    /// never starts. One whose run is being handed over is removed once it is known
    /// whether it started, so that no VM goes on running a removed microVM's command.
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
                // Then trimmed as the CLI's client sends it (`resolve`, moby client utils.go
                // trimID), which names it in its refusals.
                let reference = reference.trim().to_string();
                let id = match self.resolve(&reference) {
                    Ok(id) => id,
                    Err(_) if force => return Step::Now(Ok(false)),
                    Err(e) => return Step::Now(Err(e)),
                };
                let cannot = move |reference: &str, why: &str| format!("shards: cannot remove {reference}: {why}");
                if !lock(&self.removing).insert(id.clone()) {
                    return Step::Now(Err(format!("shards: {reference} is being removed already")));
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
                                            "shards: {reference} is removed, but its removal may not outlast a crash: {e}"
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
                            "it is restarting: stop it first, or remove it with -f",
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
                            "it is paused: unpause it first",
                        )));
                    }
                    if !force {
                        lock(&self.removing).remove(&id);
                        return Step::Now(Err(cannot(
                            &reference,
                            "it is running: stop it first, or remove it with -f",
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

    /// `shards stop vm [-t SECONDS] [-s SIGNAL]`: the signal (SIGTERM), then SIGKILL once the
    /// time (10 s) is up, as dockerd stops a container (moby daemon/stop.go). A negative
    /// time waits for ever; a stopped microVM stops again without complaint. A microVM
    /// still starting is stopped once it runs: `shards run vm -d` prints its ID before then,
    /// where `docker run -d` prints it after.
    fn stop(&self, parsed: &Parsed, styled: bool, reply: &Reply<'_>) -> u8 {
        if parsed.changed("time") && parsed.changed("timeout") {
            reply.err("--timeout and --time each give the grace: give one");
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
                // Trimmed as the CLI's client sends it (`resolve`, moby client utils.go
                // trimID), which names it in its refusals.
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
                let cannot = |why: &str| format!("shards: cannot stop {reference}: {why}");
                // Unless told, the microVM's own (`own_stop`).
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
                // No signal Linux has: SIGKILL, as `end_all` sends it (its grace is moot).
                let (linux, grace) = linux.map_or((9, None), |l| (l, grace));
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

    /// Stops microVM `given` if it runs, for `restart` (moby daemon/restart.go,
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
        let cannot = |why: &str| format!("shards: cannot restart {given}: {why}");
        let (own_signal, own_grace) = self.own_stop(&id);
        let grace = timeout.map_or(own_grace, grace_of);
        let linux = match signal {
            None => own_signal,
            Some(s) => match parse_signal(s) {
                Ok(n) => u32::try_from(n).ok().filter(|n| (1..=64).contains(n)),
                Err(e) => return Err(cannot(&e)),
            },
        };
        let (linux, grace) = linux.map_or((9, None), |l| (l, grace));
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

    /// Stops run `id` as it stops (its own signal, then SIGKILL past its grace,
    /// [`own_stop`](Self::own_stop)), for a failure of the daemon's own that leaves its run no
    /// way on (managed.rs `capture_failed`, D258): once it runs, where it is still starting, as
    /// `stop` stops one; nothing of one that has ended. Its restart is its policy's, as a run
    /// that fails is.
    pub(super) fn stop_for_failure(&self, id: &str) {
        self.await_start(id);
        if !self.running(id) {
            return;
        }
        let (signal, grace) = self.own_stop(id);
        let (linux, grace) = signal.map_or((9, None), |l| (l, grace));
        self.end_all(vec![(0, id.to_owned(), linux, grace)], &mut |_, _, _| {});
    }

    /// The stop microVM `id` asks for when nothing else is told, as dockerd reads it
    /// (moby daemon/stop.go, container.StopSignal and StopTimeout): its stop signal, else
    /// SIGTERM, `None` where Linux has no signal of its number; and how long its command
    /// may take to end after it: its stop timeout, else 10 s, for ever if negative. Set
    /// as the microVM was made, and read so: its record may not be written yet.
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

    /// `shards kill vm [-s SIGNAL]`: the signal (SIGKILL) to each running microVM's command.
    /// SIGKILL waits for the end, as dockerd's kill does; other signals are only sent
    /// (moby daemon/kill.go ContainerKill). Every error names its microVM (moby
    /// container_routes.go postContainersKill). A microVM still starting is signalled
    /// once it runs, as `stop` stops it.
    fn kill(&self, parsed: &Parsed, styled: bool, reply: &Reply<'_>) -> u8 {
        let signal = parsed.string("signal");
        let args = self.by_image(&parsed.args, true);
        self.each(
            "kill",
            styled,
            &args,
            &|reference| {
                // Trimmed as the CLI's client sends it (`resolve`, moby client utils.go
                // trimID), which names it in its refusals.
                let reference = reference.trim();
                let cannot = |why: &str| format!("shards: cannot kill {reference}: {why}");
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
                        return Step::Now(Err(match e.strip_prefix("shards: ") {
                            Some(said) => cannot(said),
                            None => e,
                        }));
                    }
                };
                let not_running = cannot("it is not running");
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

    /// `shards list vms`: the microVMs, as `docker ps` lists them (docker/cli v29.8.1
    /// cli/command/formatter/container.go): the running ones, or all with `-a`, or the
    /// last `-n` made (`-l`: one); newest first; with `-q` their IDs alone.
    fn ps(&self, parsed: &Parsed, asker: &Asker, reply: &Reply<'_>) -> u8 {
        let east_asian = asker.east_asian;
        let trunc = !parsed.bool("no-trunc");
        let (list, all) = match self.ps_listing(parsed) {
            Ok(l) => (l.containers, l.all),
            Err(e) => {
                reply.err(&format!("shards: {e}"));
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

    /// `shards read logs [-f] [-t] [--details] [--tail N] [--since T] [--until T]`: a
    /// microVM's output, stdout to stdout and stderr to stderr, in the order it arrived;
    /// the last N lines with `--tail`, of which those from `--since` on and to `--until`,
    /// each after its time with `-t`, and more as it comes until the microVM ends with
    /// `-f`. Lines carry no attributes here, so `--details` adds only the space that would
    /// follow them (moby daemon/server/httputils/logstream/logstream.go). Then its members'
    /// that the daemon drained (D240), each line on stderr after its member's label, as a
    /// client attached to the run is shown them, each member's log as the run's own is read.
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
        let mut shown = Shown {
            stamps: parsed.bool("timestamps"),
            details: parsed.bool("details"),
            attributes: Vec::new(),
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
        // Its log as its run keeps it (D161): none where it was run with none.
        let dir = lock(&self.containers).dir(&id);
        let request = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b));
        if let Some(r) = &request
            && crate::logging::driver(r) == "none"
        {
            reply.err(&format!(
                "shards: {reference} keeps no log: it was run with --log-driver none"
            ));
            return 1;
        }
        // One whose driver sends it elsewhere, its local copy kept from `logs` (D163).
        if let Some(r) = &request
            && crate::logging::kept(r, super::DEFAULT_LOGS)
                .ok()
                .flatten()
                .is_some_and(|k| !k.cached)
        {
            reply.err(&format!(
                "shards: {reference} keeps no copy of its log: it was run with --log-opt cache-disabled=true, and its log goes to {} alone",
                crate::logging::driver(r)
            ));
            return 1;
        }
        if shown.details
            && let Some(r) = request.as_ref().filter(|r| !r.log_opts.is_empty())
        {
            match self.log_attributes(&id, r) {
                Ok(a) => shown.attributes = a,
                Err(e) => {
                    reply.err(&format!("shards: {reference}: its log's attributes: {e}"));
                    return 1;
                }
            }
        }
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
                    reply.err(&format!("shards: --{flag} {given}: {e}"));
                    return 1;
                }
            };
            if flag == "until" && sent == "0" {
                continue;
            }
            match gotime::parse_unix_timestamp(&sent) {
                Ok(at) => *bound = at,
                Err(e) => {
                    reply.err(&format!("shards: --{flag} {given}: {e}"));
                    return 1;
                }
            }
        }
        let dir = lock(&self.containers).dir(&id);
        let log = match LogFile::open(&dir) {
            Ok(log) => log,
            Err(e) => {
                reply.err(&format!("shards: {reference}: its log: {e}"));
                return 1;
            }
        };
        // Its members' kept lines (D240), each after its label on stderr, as a client attached
        // to the run is shown them: after the run's own, each member's in its declared order.
        let members = match super::managed::member_logs(&dir) {
            Ok(members) => members,
            Err(e) => {
                reply.err(&format!("shards: {reference}: its members' logs: {e}"));
                return 1;
            }
        };
        // What the run's one log cap, shared by its command and its members, let go of each log
        // (D240): said first, on stderr, each where it let some go.
        let owners =
            std::iter::once((None, &log)).chain(members.iter().map(|(label, log)| (Some(label), log)));
        for (label, kept) in owners {
            let whose = label.map_or_else(|| "its".to_string(), |label| format!("[{label}]'s"));
            match super::managed::capped(kept.dir()) {
                Ok(capped) if capped.segments > 0 => reply.err(&format!(
                    "shards: {reference}: {whose} older output went to its run's one log cap, shared by its command and its members: {} bytes, {} segments",
                    capped.bytes, capped.segments
                )),
                Ok(_) => {}
                Err(e) => reply.err(&format!(
                    "shards: {reference}: {whose} log cap's record: {e}"
                )),
            }
        }
        let show = |label: Option<&str>| Show {
            reply,
            shown: shown.clone(),
            window,
            admitted: [Admit::Pass, Admit::Pass],
            stream: LOG_STDOUT,
            gathered: Vec::new(),
            unsent: None,
            label: label.map(|l| format!("[{l}] ").into_bytes()),
        };
        let mut followed: Vec<LogSource<'_, '_>> = std::iter::once((None, log))
            .chain(members.into_iter().map(|(label, log)| (Some(label), log)))
            .map(|(label, log)| LogSource {
                show: show(label.as_deref()),
                log,
                reader: Reader::new(),
                watch: None,
                watched: None,
                done: false,
            })
            .collect();
        // Without -f, what is there is all there is: a line in progress too.
        let ended = !follow || !self.running(&id);
        let read = (|| -> io::Result<()> {
            for each in &mut followed {
                if let Some(n) = tail {
                    each.reader = Reader::from(logs::tail(&each.log, n as u64, ended)?);
                }
            }
            if ended {
                for each in &mut followed {
                    if !each.read_to_end()? {
                        return Ok(());
                    }
                }
                return Ok(());
            }
            for each in &mut followed {
                if !each.read()? {
                    return Ok(());
                }
            }
            // Followed as they grow: woken by an append to the segment each is read from, by
            // a segment's coming, by the run's end, or by the client's hanging up, not by
            // a timer (audit A12). Each watch comes before a read, so no append between
            // them goes unseen. Where a log has no watch (on Linux, its user's inotify
            // instances spent: PM M257), they are read again every UNWATCHED, as the listener
            // looks, rather than end the follow.
            for each in &mut followed {
                each.watch = shards_vmm::platform::FileWatch::new(each.log.dir()).ok();
            }
            let Some((number, end)) = self.wake_at_end(&id)? else {
                for each in &mut followed {
                    if !each.read_to_end()? {
                        return Ok(());
                    }
                }
                return Ok(());
            };
            // Its client waits long: it shuts no other out (review 7.9).
            self.waits_long(asker.client);
            let outcome = (|| -> io::Result<()> {
                loop {
                    let mut rewatched = false;
                    for each in &mut followed {
                        if !each.read()? {
                            return Ok(());
                        }
                        rewatched |= each.rewatch()?;
                    }
                    // Each past `--until`: nothing more is shown.
                    if followed.iter().all(|each| each.done) {
                        return Ok(());
                    }
                    if rewatched {
                        continue;
                    }
                    let watches: Vec<&shards_vmm::platform::FileWatch> =
                        followed.iter().filter_map(|each| each.watch.as_ref()).collect();
                    let unwatched = followed.iter().any(|each| each.watch.is_none());
                    let ready =
                        wait_readable(&watches, &end, reply.0, unwatched.then_some(super::UNWATCHED))?;
                    // Once the microVM has ended, what is left of a line is all of it; and
                    // its members' logs are whole, its end told once they are (D240).
                    if ready[1] {
                        for each in &mut followed {
                            if !each.read_to_end()? {
                                return Ok(());
                            }
                        }
                        return Ok(());
                    }
                    // A client that hangs up ends its follow, output or none (audit A07).
                    if ready[2] {
                        return Ok(());
                    }
                    for each in &followed {
                        if let Some(watch) = &each.watch {
                            watch.clear();
                        }
                    }
                }
            })();
            self.forget_waiter(&id, number);
            outcome
        })();
        if let Some(e) = followed.iter_mut().find_map(|each| each.show.unsent.take()) {
            return undelivered(&e, reply);
        }
        if let Err(e) = read {
            reply.err(&format!("shards: {reference}: its log: {e}"));
            return 1;
        }
        // Output a log could not keep, it does not pretend to hold (audit A12); what a
        // non-blocking log dropped, as its run asked, it says too (D162).
        let (lost, dropped) = lock(&self.containers)
            .get(&id)
            .map_or((0, 0), |c| (c.log_lost, c.log_dropped));
        if dropped > 0 {
            reply.err(&format!(
                "shards: {dropped} bytes of {reference}'s output were dropped, as they came while its log's queue was full (--log-opt mode=non-blocking)"
            ));
        }
        if lost > 0 {
            reply.err(&format!(
                "shards: {lost} bytes of {reference}'s output could not be kept in its log"
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
    /// A member's log's (D240): what goes before each of its lines, `[agent main] `, which
    /// go to stderr, as a client attached to the run is shown them (drain.rs): each piece a
    /// line, one cut short at its log's end ended.
    label: Option<Vec<u8>>,
}

/// A log `logs` reads: its run's own, or one of its members' (D240), and how far its follow
/// has come.
struct LogSource<'r, 'a> {
    show: Show<'r, 'a>,
    log: LogFile,
    reader: Reader,
    watch: Option<shards_vmm::platform::FileWatch>,
    /// The segment its watch watches.
    watched: Option<u64>,
    /// Its lines have passed `--until`: nothing more of it is shown.
    done: bool,
}

impl LogSource<'_, '_> {
    /// Shows what its log has whole: whether to go on, as long as its client takes what it
    /// is sent.
    fn read(&mut self) -> io::Result<bool> {
        if !self.done && !self.show.read(&mut self.reader, &self.log)? {
            self.done = true;
        }
        Ok(self.show.unsent.is_none())
    }

    /// [`read`](Self::read), and what is left of its unfinished lines: its log has ended, or
    /// what is there is all there is.
    fn read_to_end(&mut self) -> io::Result<bool> {
        if !self.done && self.show.read(&mut self.reader, &self.log)? {
            self.show.finish(&mut self.reader)?;
        }
        self.done = true;
        Ok(self.show.unsent.is_none())
    }

    /// Watches the segment its reader reads, where its watch is of another: whether it does
    /// now, and so must be read again before the follow waits.
    fn rewatch(&mut self) -> io::Result<bool> {
        if let Some(segment) = self.reader.segment()
            && self.watched != Some(segment.seq())
            && let Some(watch) = self.watch.as_mut()
        {
            watch.file(segment.index())?;
            self.watched = Some(segment.seq());
            return Ok(true);
        }
        Ok(false)
    }
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
                // A member's every piece is a line of its own, on stderr.
                let labelled = self.label.is_some();
                let stream = if labelled { LOG_STDERR } else { piece.stream };
                if stream != self.stream && !self.send() {
                    return Ok(false);
                }
                self.stream = stream;
                let starts = piece.first || labelled;
                if starts && self.shown.stamps {
                    self.gathered.extend_from_slice(rfc3339_nano(piece.at).as_bytes());
                    self.gathered.push(b' ');
                }
                if starts && self.shown.details {
                    self.gathered.extend_from_slice(&self.shown.attributes);
                    self.gathered.push(b' ');
                }
                if let Some(label) = &self.label {
                    self.gathered.extend_from_slice(label);
                }
                self.gathered.extend_from_slice(piece.bytes);
                if labelled && piece.bytes.last() != Some(&b'\n') {
                    self.gathered.push(b'\n');
                }
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
    fn top_refuses_pid_headers_on_other_fields() {
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

    /// `exec --for` takes no privilege and no user of its own: its member's are its (D243).
    #[test]
    fn a_member_exec_takes_nothing_its_member_has_not() {
        let exec = |privileged: bool, user: &str| shards_ipc::Exec {
            member: "codex".into(),
            privileged,
            user: user.into(),
            ..shards_ipc::Exec::default()
        };
        assert_eq!(super::held_as_its_member(&exec(false, "")), Ok(()));
        for (privileged, user, refused) in [
            (true, "", super::MemberExec::Privileged("codex".into())),
            (false, "0", super::MemberExec::User("codex".into())),
            (true, "0", super::MemberExec::Privileged("codex".into())),
        ] {
            assert_eq!(
                super::held_as_its_member(&exec(privileged, user)),
                Err(refused),
                "{privileged} {user:?}"
            );
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
                attributes: Vec::new(),
            },
            window: Window::default(),
            admitted: [Admit::Pass, Admit::Pass],
            stream: LOG_STDOUT,
            gathered: Vec::new(),
            unsent: None,
            label: None,
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
    fn log_timestamps_print_in_full() {
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
    fn log_windows_filter_what_is_forwarded() {
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
    fn ps_tables_and_ports_print_byte_for_byte() {
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
    fn commands_show_quoted_in_listings() {
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
            tabulate_with(&rows, 10, false),
            ["CONTAINER ID   IMAGE     NAMES", "0123456789ab   alpine    web"]
        );
        // The ellipsis is ambiguous: two columns in an East Asian locale.
        let cut = [["\"ab\u{2026}\"", "x"].map(String::from)];
        assert_eq!(tabulate_with(&cut, 10, false), ["\"ab\u{2026}\"     x"]);
        assert_eq!(tabulate_with(&cut, 10, true), ["\"ab\u{2026}\"    x"]);
    }

    #[test]
    fn signals_are_taken_by_name_and_number() {
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
            assert_eq!(
                parse_signal(bad),
                Err(format!(
                    "{bad} is no signal: give its name (TERM, SIGKILL, RTMIN+3) or its number"
                )),
                "{bad}"
            );
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

/// A port of a running microVM, in the shape `ps` and `port` answer (moby docker-v29.3.1
/// api/types/container/port.go `Port`): published at a host address and port,
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

/// A microVM's command as the API shows it: the path, then the arguments, each in single
/// quotes if it holds a space (moby daemon/container/view.go).
pub(super) fn command_line(argv: &[String]) -> String {
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
impl<D: crate::containers::Disk> Daemon<D> {
    /// The images `command` names in `parsed` that are not microVMs here yet, made ones:
    /// pulled and converted (run::find_image), the pull said on stderr as `run` says it.
    /// One that cannot be is left for the command to refuse in its words.
    fn convert_named(
        &self,
        command: &shards_cmdline::flags::Command,
        parsed: &shards_cmdline::flags::Parsed,
        asker: &Asker,
        reply: &Reply<'_>,
    ) {
        let images: Vec<&String> = if std::ptr::eq(command, &CONTAINER_INSPECT) {
            parsed.args.iter().filter(|a| self.resolve(a).is_err()).collect()
        } else if std::ptr::eq(command, &IMAGE_INSPECT) || std::ptr::eq(command, &SAVE) {
            parsed.args.iter().collect()
        } else {
            parsed.args.first().into_iter().collect()
        };
        let Ok(targets) = crate::pull::targets("") else {
            return;
        };
        let cancel = shards_registry::http::Cancel::new();
        let say = |line: &str| reply.err(line);
        for image in images {
            let _ = crate::run::find_image(
                image,
                shards_ipc::Pull::Missing,
                &targets,
                &asker.registry_env,
                &self.home,
                &say,
                &cancel,
            );
        }
    }
}

/// Each microVM's last sample, for one whose VM is paused and cannot be asked: taken
/// as it is paused, and as `stats` samples it; a removed one's goes as `stats` next looks.
static LAST_USAGE: std::sync::Mutex<std::collections::BTreeMap<String, Usage>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

impl<D: crate::containers::Disk> Daemon<D> {
    /// Container `id`'s use of its microVM, as its guest's STATS says it; kept as its last.
    pub(super) fn usage_of(&self, id: &str) -> Option<Usage> {
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::STATS,
            ..Default::default()
        };
        // A sample not taken is said in the daemon's log: the row shows it unmeasured.
        let u = match self.exec_answered(id, &spec, 1 << 12) {
            Ok(q) if q.status == Some(0) => Usage::parse(&String::from_utf8_lossy(&q.output)),
            Ok(q) => {
                super::log(format!("container {id}: its stats: status {:?}", q.status));
                return None;
            }
            Err(e) => {
                super::log(format!("container {id}: its stats: {e}"));
                return None;
            }
        };
        let mut last = lock(&LAST_USAGE);
        last.retain(|known, _| lock(&self.containers).get(known).is_some());
        last.insert(id.to_string(), u.clone());
        Some(u)
    }
}

/// A workload's use of its microVM, as shards-init's STATS says it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Usage {
    pub cpu: u64,
    pub system: u64,
    pub online: u64,
    pub memory: u64,
    pub inactive_file: u64,
    pub limit: u64,
    pub pids: u64,
    pub read: u64,
    pub write: u64,
    pub rx: u64,
    pub tx: u64,
}

impl Usage {
    fn parse(text: &str) -> Usage {
        let mut u = Usage::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once(' ') else {
                continue;
            };
            let v: u64 = v.trim().parse().unwrap_or(0);
            match k {
                "cpu" => u.cpu = v,
                "system" => u.system = v,
                "online" => u.online = v,
                "memory" => u.memory = v,
                "inactive_file" => u.inactive_file = v,
                "limit" => u.limit = v,
                "pids" => u.pids = v,
                "read" => u.read = v,
                "write" => u.write = v,
                "rx" => u.rx = v,
                "tx" => u.tx = v,
                _ => {}
            }
        }
        u
    }

    /// The CLI's figures of this sample, `elapsed` after `before` (docker/cli
    /// container/stats_helpers.go): CPU as calculateCPUPercentUnix works it out, its
    /// time over the system's times its CPUs, the system's being the time between the
    /// samples on every CPU, which here is the host's clock, as the guest's /proc/stat
    /// on a loaded host does not keep (measured: 329% and 2503% for one spinning vCPU);
    /// memory less its inactive file pages (calculateMemUsageUnixNoCache, cgroup v2), and
    /// its share of the limit.
    #[allow(clippy::cast_precision_loss)]
    fn stats(
        &self,
        before: Option<(&Usage, Duration)>,
        container: String,
        name: String,
        id: String,
    ) -> shards_cmdline::format::stats::Stats {
        let cpu_percentage = match before {
            Some((b, elapsed)) if self.cpu > b.cpu && !elapsed.is_zero() => {
                (self.cpu - b.cpu) as f64 / elapsed.as_nanos() as f64 * 100.0
            }
            _ => 0.0,
        };
        let memory = if self.inactive_file < self.memory {
            self.memory - self.inactive_file
        } else {
            self.memory
        };
        let memory_percentage = if self.limit == 0 {
            0.0
        } else {
            memory as f64 / self.limit as f64 * 100.0
        };
        shards_cmdline::format::stats::Stats {
            container,
            name,
            id,
            cpu_percentage,
            memory: memory as f64,
            memory_limit: self.limit as f64,
            memory_percentage,
            network_rx: self.rx as f64,
            network_tx: self.tx as f64,
            block_read: self.read as f64,
            block_write: self.write as f64,
            pids: self.pids,
            invalid: false,
        }
    }
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

/// A microVM's status as dockerd words it (moby daemon/container/state.go): up for how
/// long, with its health if it has a check; else being removed, if it is; or exited with
/// what status how long ago, or created and never started.
pub(super) fn status(
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

/// `rows` aligned as piped tables are read (padding 3, spaces; docker/cli
/// cli/command/formatter/tabwriter/tabwriter.go): each column but the last is as wide as
/// its widest cell plus 3, and at least `min` (the CLI's is 10), in go-runewidth's columns.
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

pub(crate) use shards_ipc::COPY_STEP;

/// The most of a microVM's process dump `top` takes: shards-init makes it whole in its
/// guest's memory before it sends it (init procs.rs `dump`), so no more than the guest
/// holds, which its host holds; read as far as the daemon can hold now
/// ([`crate::build::holdable`]).
fn top_limit() -> usize {
    usize::try_from(crate::build::holdable()).unwrap_or(usize::MAX)
}

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

/// Who asked for a command on microVMs, and what of theirs shapes the answer.
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
    /// The client of another machine's they ask for through its context (D228), empty for
    /// this machine's own user: what it reaches of this machine its owner granted it.
    pub whose: String,
}

impl Asker {
    /// Whether their stdout is a colour terminal, where shards draws its own pages.
    pub(super) fn styled(&self) -> bool {
        self.terminal && self.color
    }
}

/// The times `logs` shows lines between, as dockerd's log forwarder keeps them (moby
/// daemon/logger/loggerutils/logfile.go, forwarder.Do), in nanoseconds since the epoch.
#[derive(Default, Clone, Copy)]
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

/// Waits, for `timeout` at most, until a follow has more to do: one of its logs' `watches`
/// has seen a change, the run has ended (`end`), or its `client` has said more or hung up;
/// which have, the watches as one.
#[cfg(unix)]
pub(super) fn wait_readable(
    watches: &[&shards_vmm::platform::FileWatch],
    end: &UnixStream,
    client: &UnixStream,
    timeout: Option<Duration>,
) -> io::Result<[bool; 3]> {
    use std::os::fd::AsRawFd as _;
    let mut polled = Vec::new();
    polled
        .try_reserve_exact(watches.len().saturating_add(2))
        .map_err(io::Error::other)?;
    let fds = watches
        .iter()
        .map(|watch| watch.fd())
        .chain([end.as_fd(), client.as_fd()]);
    polled.extend(fds.map(|fd| libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }));
    let count = libc::nfds_t::try_from(polled.len()).map_err(io::Error::other)?;
    // Rounded up: a wait of 0 ms would return at once, and spin.
    let ms = timeout.map_or(-1, |t| {
        libc::c_int::try_from(t.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
    });
    loop {
        // SAFETY: poll(2) on `count` pollfds, of descriptors the caller holds open.
        let n = unsafe { libc::poll(polled.as_mut_ptr(), count, ms) };
        if n >= 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    let (watched, streams) = polled.split_at(watches.len());
    let ready = |at: usize| streams.get(at).is_some_and(|p| p.revents != 0);
    Ok([watched.iter().any(|p| p.revents != 0), ready(0), ready(1)])
}

/// Waits, for `timeout` at most, until a follow has more to do, as Unix's does: a watch's
/// event, and the streams by reads of no bytes (`shards_ipc::await_any`).
#[cfg(windows)]
pub(super) fn wait_readable(
    watches: &[&shards_vmm::platform::FileWatch],
    end: &UnixStream,
    client: &UnixStream,
    timeout: Option<Duration>,
) -> io::Result<[bool; 3]> {
    let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
    let events: Vec<_> = watches.iter().map(|watch| watch.fd()).collect();
    let (streams, signalled) = shards_ipc::await_any(&[end, client], &events, deadline)?;
    Ok([
        signalled.iter().any(|&s| s),
        streams.first().copied().unwrap_or(false),
        streams.get(1).copied().unwrap_or(false),
    ])
}

/// What `docker logs` puts before each line: its time with `-t`, and with `--details` its
/// attributes and a space.
#[derive(Clone)]
struct Shown {
    stamps: bool,
    details: bool,
    /// The attributes `--details` shows before a line, in the form scripts read (D161, moby's
    /// attrsByteSlice): sorted by name, each `NAME=VALUE` query-escaped, joined
    /// by commas (D161).
    attributes: Vec<u8>,
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

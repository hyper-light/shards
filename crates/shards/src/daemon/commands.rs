//! Container commands the daemon runs for its clients (`shards ps`, `wait`, `logs`, `rm`,
//! `stop`, `kill`), answering on the client's stdout and stderr as `docker` answers
//! (docs/design/architecture.md D27). The client has read the command line already
//! (shards_cmdline); the daemon reads it again by the same rules, and does what dockerd
//! would, in the order and with the words the Docker CLI and dockerd use.

use std::io::{self, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use shards_cmdline::commands::{
    self, IMAGE_INSPECT, IMAGES, KILL, LOAD, LOGS, PORT, PS, PUSH, RM, RMI, SAVE, STOP, TAG, WAIT,
};
use shards_cmdline::flags::{self, Outcome, Parsed};
use shards_cmdline::{go, gotime, width};
use shards_ipc::kind;

use super::logs::{self, LogFile, Piece, Reader};
use super::{Daemon, RunState, STOP_GRACE, lock};
use crate::containers::{Container, Removal, State as Life, now};
use crate::spec::{LOG_STDERR, LOG_STDOUT};

/// How long a command may take to end after SIGKILL before its VM goes too, and how long
/// the VM may take then (moby daemon/kill.go, kill).
const KILL_WAIT: Duration = Duration::from_secs(10);
const LAST_WAIT: Duration = Duration::from_secs(2);
/// How long `stop` waits after a signal it could not send (moby daemon/stop.go).
const UNSENT_WAIT: Duration = Duration::from_secs(2);
/// How many containers `stop`, `kill` and `rm` act on at once (docker/cli
/// cli/command/container/utils.go, parallelOperation).
const AT_ONCE: usize = 50;

/// The client's end: what a command prints goes there.
pub(super) struct Reply<'a>(pub &'a UnixStream);

impl Reply<'_> {
    pub(super) fn out(&self, line: &str) {
        let _ = self.bytes(LOG_STDOUT, format!("{line}\n").as_bytes());
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
        // One being started is seen through, as `docker exec` finds it started or not.
        self.await_start(&id);
        // What the exec needs of the run, taken out of `runs` before its inbox is locked:
        // its follower holds the inbox while it ends the run, which takes `runs`.
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
        let spec = match crate::spec::spec(&options, |_| None) {
            Ok(spec) => spec,
            Err(e) => return refuse(&format!("Error response from daemon: {e}")),
        };
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
        lock(&inbox).execs_in_flight.push((number, held));
        let fds = [conn.as_fd(), stdin.as_fd(), stdout.as_fd(), stderr.as_fd()];
        if let Err(e) = socket.send(kind::EXEC_RUN, &payload, &fds) {
            lock(&inbox).execs_in_flight.retain(|(n, _)| *n != number);
            refuse(&format!(
                "Error response from daemon: the container's microVM: {e}"
            ));
        }
    }

    /// Runs container command `argv` for a client, answering on `reply`, and returns its
    /// exit status, the client being `asker`.
    pub(super) fn command(&self, argv: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        // What any client has seen of its run, the answer includes.
        self.settle();
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let Some((command, path, named)) = commands::find(&words) else {
            reply.err(&format!("shards: no container command in {argv:?}"));
            return 1;
        };
        let rest = argv.get(named..).unwrap_or_default();
        let parsed = match flags::parse(command, path, rest, &|_, value| Ok(value.to_string())) {
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
        if std::ptr::eq(command, &PS) {
            self.ps(&parsed, asker.east_asian, reply)
        } else if std::ptr::eq(command, &WAIT) {
            self.wait(&parsed.args, reply)
        } else if std::ptr::eq(command, &LOGS) {
            self.logs(&parsed, asker, reply)
        } else if std::ptr::eq(command, &RM) {
            self.rm(&parsed, reply)
        } else if std::ptr::eq(command, &STOP) {
            self.stop(&parsed, reply)
        } else if std::ptr::eq(command, &KILL) {
            self.kill(&parsed, reply)
        } else if std::ptr::eq(command, &PORT) {
            self.port(&parsed.args, reply)
        } else if std::ptr::eq(command, &IMAGES) {
            self.images(&parsed, asker, reply)
        } else if std::ptr::eq(command, &TAG) {
            self.tag(&parsed.args, reply)
        } else if std::ptr::eq(command, &RMI) {
            self.rmi(&parsed, reply)
        } else if std::ptr::eq(command, &IMAGE_INSPECT) {
            self.image_inspect(&parsed.args, reply)
        } else if std::ptr::eq(command, &SAVE) {
            self.save(&parsed.args, asker, reply)
        } else if std::ptr::eq(command, &LOAD) {
            self.load(asker, reply)
        } else if std::ptr::eq(command, &PUSH) {
            self.push(&parsed, asker, reply)
        } else {
            reply.err(&format!("shards: {path} is not a container command"));
            1
        }
    }

    /// `shards port CONTAINER [PORT]` (docker/cli cli/command/container/port.go): each
    /// published port of a running container as `PORT/PROTO -> HOST:PORT`, or with PORT
    /// the host addresses of that one, in natural order.
    fn port(&self, args: &[String], reply: &Reply<'_>) -> u8 {
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
        for line in lines {
            reply.out(&line);
        }
        0
    }

    /// The ID of the container `reference` names: all of its ID, its name, or the start of
    /// its ID and of no other's (moby daemon/container.go, GetContainer).
    pub(super) fn resolve(&self, reference: &str) -> Result<String, String> {
        // A container exists from its creation, as dockerd's does: one whose record is
        // still being written is waited for, not missed.
        let mut registry = lock(&self.containers);
        while !reference.is_empty() && registry.arriving_as(reference) {
            registry = self
                .arrived
                .wait(registry)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if !reference.is_empty() {
            if registry.get(reference).is_some() {
                return Ok(reference.to_string());
            }
            let name = reference.strip_prefix('/').unwrap_or(reference);
            if let Some(c) = registry.named(name) {
                return Ok(c.id.clone());
            }
            let mut matching = registry.all().filter(|c| c.id.starts_with(reference));
            if let Some(first) = matching.next() {
                if matching.next().is_some() {
                    return Err(format!(
                        "Error response from daemon: multiple IDs found with provided prefix: {reference}"
                    ));
                }
                return Ok(first.id.clone());
            }
        }
        Err(format!(
            "Error response from daemon: No such container: {reference}"
        ))
    }

    /// Whether the container with `id` runs: its run was handed over, and has not ended.
    fn running(&self, id: &str) -> bool {
        matches!(lock(&self.runs).get(id), Some(RunState::Tracked(_)))
    }

    /// Sends Linux signal `linux` to the command of the container with `id`; whether it
    /// could.
    fn signal(&self, id: &str, linux: u32) -> bool {
        // Sent once the runs' lock is let go of: no send waits under it.
        let socket = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => t.socket.clone(),
            _ => return false,
        };
        socket.send(kind::SIGNAL, &linux.to_be_bytes(), &[]).is_ok()
    }

    /// Ends the command of the running container with `id` as dockerd does (moby
    /// daemon/stop.go containerStop, daemon/kill.go kill): signal `linux`, then wait up to
    /// `grace` (for ever if `None`, 2 s if the signal could not be sent), then SIGKILL and
    /// up to 10 s more, then the VM itself and 2 s more. Whether the command ended. Its
    /// client hanging up changes nothing: "Cancelling the request should not cancel the
    /// stop" (moby daemon/stop.go, containerStop).
    fn end(&self, id: &str, linux: u32, grace: Option<Duration>) -> bool {
        if linux != 9 {
            let grace = if self.signal(id, linux) {
                grace
            } else {
                Some(UNSENT_WAIT)
            };
            if self.await_exit(id, grace, None).is_some() {
                return true;
            }
        }
        self.signal(id, 9);
        if self.await_exit(id, Some(KILL_WAIT), None).is_some() {
            return true;
        }
        if let Some(RunState::Tracked(t)) = lock(&self.runs).get(id) {
            let _ = t.vm.kill(libc::SIGKILL);
        }
        self.await_exit(id, Some(LAST_WAIT), None).is_some()
    }

    /// Runs `op` for each of `args`, up to 50 at once and in their order, and answers as
    /// the Docker CLI does (cli/command/container/utils.go parallelOperation; stop.go,
    /// kill.go, rm.go): each success prints its argument as given, once it and those
    /// before it are done, unless `op` says it has nothing to say; the errors follow,
    /// one per line, and make the status 1.
    fn each(
        &self,
        args: &[String],
        op: &(dyn Fn(&str) -> Result<bool, String> + Sync),
        reply: &Reply<'_>,
    ) -> u8 {
        let done: Mutex<Vec<Option<Result<bool, String>>>> = Mutex::new(vec![None; args.len()]);
        let changed = Condvar::new();
        let next = AtomicUsize::new(0);
        let work = || {
            loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(arg) = args.get(i) else {
                    return;
                };
                let result = op(arg);
                if let Some(slot) = lock(&done).get_mut(i) {
                    *slot = Some(result);
                }
                changed.notify_all();
            }
        };
        let mut errors = Vec::new();
        std::thread::scope(|scope| {
            let workers = (0..args.len().min(AT_ONCE))
                .filter(|_| {
                    std::thread::Builder::new()
                        .name("container".into())
                        .spawn_scoped(scope, work)
                        .is_ok()
                })
                .count();
            // With no thread to be had, one at a time, here.
            if workers == 0 {
                work();
            }
            for (i, arg) in args.iter().enumerate() {
                let mut all = lock(&done);
                let result = loop {
                    match all.get_mut(i).map(Option::take) {
                        Some(Some(result)) => break result,
                        Some(None) => {
                            all = changed
                                .wait(all)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        None => break Ok(false),
                    }
                };
                drop(all);
                match result {
                    Ok(true) => reply.out(arg),
                    Ok(false) => {}
                    Err(e) => errors.push(e),
                }
            }
        });
        for e in &errors {
            reply.err(e);
        }
        u8::from(!errors.is_empty())
    }

    /// `shards wait`: for each container in turn, its exit code once it stops.
    fn wait(&self, references: &[String], reply: &Reply<'_>) -> u8 {
        let mut errors = Vec::new();
        for reference in references {
            match self.resolve(reference) {
                Ok(id) => {
                    // Its client may hang up first: then nobody reads the rest.
                    let Some(code) = self.await_exit(&id, None, Some(reply.0)) else {
                        return 1;
                    };
                    reply.out(&code.to_string());
                }
                Err(e) => errors.push(e),
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
    fn rm(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let force = parsed.bool("force");
        self.each(
            &parsed.args,
            &|given| {
                let reference = given.trim_matches('/');
                if reference.is_empty() {
                    return Err("container name cannot be empty".into());
                }
                let id = match self.resolve(reference) {
                    Ok(id) => id,
                    Err(_) if force => return Ok(false),
                    Err(e) => return Err(e),
                };
                let cannot = |why: &str| {
                    format!("Error response from daemon: cannot remove container \"{reference}\": {why}")
                };
                if !lock(&self.removing).insert(id.clone()) {
                    return Err(format!(
                        "Error response from daemon: removal of container {reference} is already in progress"
                    ));
                }
                // Out of sight, then durable: an `rm` answered is never undone by a crash.
                let complete = |removal: Removal| {
                    self.complete(&removal).map(|()| true).map_err(|e| {
                        format!(
                            "Error response from daemon: container \"{reference}\" is removed, but its removal may not outlast a crash: {e}"
                        )
                    })
                };
                let removed = (|| {
                    if let Some(removal) = self.cancel_start(&id).map_err(|e| cannot(&e.to_string()))? {
                        return complete(removal);
                    }
                    if lock(&self.containers).get(&id).is_none() {
                        return Ok(true);
                    }
                    if self.running(&id) {
                        if !force {
                            return Err(cannot(
                                "container is running: stop the container before removing or force remove",
                            ));
                        }
                        if !self.end(&id, 9, None) {
                            return Err(cannot(
                                "could not kill container: tried to kill container, but did not receive an exit event",
                            ));
                        }
                    }
                    let removal = lock(&self.containers)
                        .remove(&self.disk, &id)
                        .map_err(|e| cannot(&e.to_string()))?;
                    removal.map_or(Ok(true), complete)
                })();
                lock(&self.removing).remove(&id);
                removed
            },
            reply,
        )
    }

    /// `shards stop [-t SECONDS] [-s SIGNAL]`: the signal (SIGTERM), then SIGKILL once the
    /// time (10 s) is up, as dockerd stops a container (moby daemon/stop.go). A negative
    /// time waits for ever; a stopped container stops again without complaint. A container
    /// still starting is stopped once it runs: `shards run -d` prints its ID before then,
    /// where `docker run -d` prints it after.
    fn stop(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        if parsed.changed("time") && parsed.changed("timeout") {
            reply.err("conflicting options: cannot specify both --timeout and --time");
            return 1;
        }
        // Go multiplies the seconds into nanoseconds, wrapping (daemon/stop.go); a negative
        // number waits for ever.
        let grace_of = |seconds: i64| {
            (seconds >= 0).then(|| {
                let ns = seconds.wrapping_mul(1_000_000_000);
                Duration::from_nanos(u64::try_from(ns).unwrap_or(0))
            })
        };
        let told = (parsed.changed("timeout") || parsed.changed("time")).then(|| parsed.int("timeout"));
        let signal = parsed.string("signal");
        self.each(
            &parsed.args,
            &|reference| {
                let id = self.resolve(reference)?;
                self.await_start(&id);
                if !self.running(&id) {
                    return Ok(true);
                }
                let cannot = |why: &str| {
                    format!("Error response from daemon: cannot stop container: {reference}: {why}")
                };
                // Unless told, the container's own: its stop signal (SIGTERM if it has
                // none or one no longer valid) and its stop timeout, else 10 seconds
                // (moby daemon/stop.go, container.StopSignal and StopTimeout).
                let (own_signal, own_timeout) = lock(&self.containers)
                    .get(&id)
                    .map(|c| (c.stop_signal, c.stop_timeout))
                    .unwrap_or_default();
                let grace = match told.or(own_timeout) {
                    Some(seconds) => grace_of(seconds),
                    None => Some(STOP_GRACE),
                };
                let linux = if signal.is_empty() {
                    own_signal.unwrap_or(15)
                } else {
                    // A number Linux has no signal for cannot be sent.
                    parse_signal(signal).map_err(|e| cannot(&e))?
                };
                match u32::try_from(linux).ok().filter(|n| (1..=64).contains(n)) {
                    Some(linux) if self.end(&id, linux, grace) => Ok(true),
                    None if self.end(&id, 9, Some(UNSENT_WAIT)) => Ok(true),
                    _ => Err(cannot(
                        "tried to kill container, but did not receive an exit event",
                    )),
                }
            },
            reply,
        )
    }

    /// `shards kill [-s SIGNAL]`: the signal (SIGKILL) to each running container's command.
    /// SIGKILL waits for the end, as dockerd's kill does; other signals are only sent
    /// (moby daemon/kill.go ContainerKill). Every error names its container (moby
    /// container_routes.go postContainersKill). A container still starting is signalled
    /// once it runs, as `stop` stops it.
    fn kill(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let signal = parsed.string("signal");
        self.each(
            &parsed.args,
            &|reference| {
                let cannot = |why: &str| {
                    format!("Error response from daemon: cannot kill container: {reference}: {why}")
                };
                let linux = if signal.is_empty() {
                    9
                } else {
                    let n = parse_signal(signal).map_err(|e| cannot(&e))?;
                    linux_signal(n)
                        .ok_or_else(|| cannot(&format!("the linux daemon does not support signal {n}")))?
                };
                let id = self
                    .resolve(reference)
                    .map_err(|e| cannot(e.trim_start_matches("Error response from daemon: ")))?;
                let not_running = || cannot(&format!("container {id} is not running"));
                self.await_start(&id);
                if !self.running(&id) {
                    return Err(not_running());
                }
                if linux == 9 {
                    if !self.end(&id, 9, None) {
                        return Err(cannot(
                            "tried to kill container, but did not receive an exit event",
                        ));
                    }
                } else if !self.signal(&id, linux) {
                    return Err(not_running());
                }
                Ok(true)
            },
            reply,
        )
    }

    /// `shards ps`: the containers, as `docker ps` lists them (docker/cli v29.8.1
    /// cli/command/formatter/container.go): the running ones, or all with `-a`, or the
    /// last `-n` made (`-l`: one); newest first; with `-q` their IDs alone.
    fn ps(&self, parsed: &Parsed, east_asian: bool, reply: &Reply<'_>) -> u8 {
        // `-l` is `-n 1`, unless `-n` says otherwise (docker/cli list.go).
        let last = match parsed.int("last") {
            -1 if parsed.bool("latest") => 1,
            n => n,
        };
        let last = usize::try_from(last).ok().filter(|&n| n > 0);
        let all = parsed.bool("all") || last.is_some();
        let trunc = !parsed.bool("no-trunc");
        let mut list: Vec<Container> = lock(&self.containers)
            .all()
            .filter(|c| all || c.state == Life::Running)
            .cloned()
            .collect();
        list.sort_by_key(|c| std::cmp::Reverse(c.created));
        list.truncate(last.unwrap_or(usize::MAX));
        let at = now();
        let health = lock(&self.health);
        let listed: Vec<Listed> = list
            .iter()
            .map(|c| Listed {
                id: c.id.clone(),
                image: c.image.clone(),
                command: command_line(&c.command),
                created: c.created,
                status: status(c, at, health.get(&c.id).map(|h| h.status)),
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
                name: c.name.clone(),
            })
            .collect();
        let shown = Listing {
            trunc,
            quiet: parsed.bool("quiet"),
            east_asian,
        };
        for line in ps_lines(&listed, at, shown) {
            reply.out(&line);
        }
        0
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
        // A line's window is decided at its first piece, and holds for the rest of it.
        let mut admitted = [Admit::Pass, Admit::Pass];
        let mut unsent: Option<io::Error> = None;
        let mut each = |piece: Piece<'_>| -> io::Result<bool> {
            let s = usize::from(piece.stream == LOG_STDERR);
            let Some(decision) = admitted.get_mut(s) else {
                return Ok(false);
            };
            if piece.first {
                *decision = window.admit(piece.at);
            }
            match decision {
                Admit::Skip => Ok(true),
                Admit::Stop => Ok(false),
                Admit::Pass => match send(reply, shown, &piece) {
                    Ok(()) => Ok(true),
                    Err(e) => {
                        unsent = Some(e);
                        Ok(false)
                    }
                },
            }
        };
        // Without -f, what is there is all there is: a line in progress too.
        let ended = !follow || !self.running(&id);
        let read = (|| -> io::Result<()> {
            let mut reader = match tail {
                Some(n) => Reader::from(logs::tail(&log, n as u64, ended)?),
                None => Reader::new(),
            };
            if !reader.read(&log, &mut each)? {
                return Ok(());
            }
            if ended {
                reader.finish(&mut each)?;
                return Ok(());
            }
            // Followed as it grows: woken by an append to the segment it is read from, by
            // a segment's coming, by the run's end, or by the client's hanging up, not by
            // a timer (audit A12). Each watch comes before a read, so no append between
            // them goes unseen.
            let mut watch = shards_vmm::platform::FileWatch::new(log.dir())?;
            let mut watched = None;
            let Some((number, end)) = self.wake_at_end(&id)? else {
                reader.read(&log, &mut each)?;
                reader.finish(&mut each)?;
                return Ok(());
            };
            let followed = (|| -> io::Result<()> {
                loop {
                    if !reader.read(&log, &mut each)? {
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
                        if reader.read(&log, &mut each)? {
                            reader.finish(&mut each)?;
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
        if let Some(e) = unsent {
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

/// Sends `piece` of a line to the client, after the line's prefix if it is the first.
fn send(reply: &Reply<'_>, shown: Shown, piece: &Piece<'_>) -> io::Result<()> {
    if !piece.first || !shown.stamps && !shown.details {
        return reply.bytes(piece.stream, piece.bytes);
    }
    let mut line = Vec::with_capacity(piece.bytes.len() + 32);
    if shown.stamps {
        line.extend_from_slice(rfc3339_nano(piece.at).as_bytes());
        line.push(b' ');
    }
    if shown.details {
        line.push(b' ');
    }
    line.extend_from_slice(piece.bytes);
    reply.bytes(piece.stream, &line)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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
    fn ps_prints_what_the_docker_cli_prints() {
        let golden: serde_json::Value = serde_json::from_str(include_str!("docker-ps.json")).unwrap();
        let s = 1_000_000_000u128;
        for d in golden["durations"].as_array().unwrap() {
            let seconds = u128::from(d["seconds"].as_u64().unwrap());
            assert_eq!(
                human_duration(seconds * s),
                d["text"].as_str().unwrap(),
                "{seconds} s"
            );
        }
        let tables = golden["tables"].as_array().unwrap();
        assert_eq!(tables.len(), 16);
        let at = 1_790_673_154 * s;
        for t in tables {
            let text = |v: &serde_json::Value| v.as_str().unwrap().to_string();
            let list: Vec<Listed> = t["containers"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(|c| Listed {
                    id: text(&c["id"]),
                    image: text(&c["image"]),
                    command: text(&c["command"]),
                    created: at - u128::from(c["ago"].as_u64().unwrap()) * s,
                    status: text(&c["status"]),
                    ports: displayable_ports(port_summaries(&c["ports"])),
                    name: text(&c["name"]),
                })
                .collect();
            let shown = Listing {
                trunc: t["trunc"].as_bool().unwrap(),
                quiet: t["quiet"].as_bool().unwrap(),
                east_asian: t["east_asian"].as_bool().unwrap(),
            };
            let printed: String = ps_lines(&list, at, shown)
                .iter()
                .map(|l| format!("{l}\n"))
                .collect();
            assert_eq!(
                printed,
                t["output"].as_str().unwrap(),
                "trunc {} quiet {} east_asian {}",
                shown.trunc,
                shown.quiet,
                shown.east_asian
            );
        }
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
    fn commands_and_images_show_as_docker_ps_shows_them() {
        let argv = |w: &[&str]| w.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(command_line(&argv(&["sh", "-c", "echo hi"])), "sh -c 'echo hi'");
        for (stored, shown) in [
            ("alpine", "alpine"),
            ("alpine:3.20", "alpine:3.20"),
            ("docker.io/library/alpine:latest", "alpine:latest"),
            ("ghcr.io/a/b:v1", "ghcr.io/a/b:v1"),
            (
                "alpine:3.20@sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "alpine:3.20",
            ),
            ("", "<no image>"),
            ("NOT/valid", "NOT/valid"),
        ] {
            assert_eq!(image(stored, true), shown, "{stored}");
        }
        assert_eq!(
            image("docker.io/library/alpine", false),
            "docker.io/library/alpine"
        );
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

/// A container as dockerd lists it to the CLI: its command as one line, its status in
/// words, and when it was made, in nanoseconds since the epoch.
struct Listed {
    id: String,
    image: String,
    command: String,
    created: u128,
    status: String,
    ports: String,
    name: String,
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

/// How `ps` was asked to list: `--no-trunc` or not, `-q`, and the client's locale.
#[derive(Clone, Copy)]
struct Listing {
    trunc: bool,
    quiet: bool,
    east_asian: bool,
}

/// `list` as `docker ps` prints it at `at` (docker/cli cli/command/formatter/container.go,
/// the default table): with `-q` the IDs alone, else a table under its header.
fn ps_lines(list: &[Listed], at: u128, shown: Listing) -> Vec<String> {
    let id = |c: &Listed| {
        if shown.trunc {
            c.id.get(..12).unwrap_or(&c.id).to_string()
        } else {
            c.id.clone()
        }
    };
    if shown.quiet {
        return list.iter().map(id).collect();
    }
    let mut rows = vec![
        [
            "CONTAINER ID",
            "IMAGE",
            "COMMAND",
            "CREATED",
            "STATUS",
            "PORTS",
            "NAMES",
        ]
        .map(String::from),
    ];
    for c in list {
        let command = if shown.trunc {
            width::ellipsis(&c.command, 20)
        } else {
            c.command.clone()
        };
        // The API gives creation times in whole seconds.
        let created = c.created / 1_000_000_000 * 1_000_000_000;
        rows.push([
            id(c),
            image(&c.image, shown.trunc),
            go::quote(&command),
            format!("{} ago", human_duration(at.saturating_sub(created))),
            c.status.clone(),
            c.ports.clone(),
            c.name.clone(),
        ]);
    }
    tabulate(&rows, shown.east_asian)
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

/// A container's image as `docker ps` shows it (docker/cli formatter/container.go,
/// Image): as given with `--no-trunc`; else its familiar name, without the digest but
/// with the tag.
fn image(stored: &str, trunc: bool) -> String {
    if stored.is_empty() {
        return "<no image>".into();
    }
    if !trunc {
        return stored.to_string();
    }
    match shards_image::reference::Reference::parse_normalized(stored) {
        Ok(mut reference) => {
            reference.digest = None;
            reference.familiar()
        }
        Err(_) => stored.to_string(),
    }
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
/// long, with its health if it has a check, or exited with what status how long ago, or
/// created and never started.
fn status(c: &Container, at: u128, health: Option<super::health::Status>) -> String {
    match (c.state, c.started, c.finished) {
        (Life::Running, Some(started), _) => {
            let up = human_duration(at.saturating_sub(started));
            match health {
                Some(h) => format!("Up {up} ({})", h.shown()),
                None => format!("Up {up}"),
            }
        }
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
    let cell_width = |cell: &str| width::string_width(cell, east_asian);
    let mut widths = [10usize; N];
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(cell_width(cell) + 3);
        }
    }
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                line.push_str(cell);
                if i + 1 < N {
                    let pad = widths
                        .get(i)
                        .copied()
                        .unwrap_or(10)
                        .saturating_sub(cell_width(cell));
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            line
        })
        .collect()
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

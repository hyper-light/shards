//! A microVM's health, as dockerd keeps it (moby daemon/health.go, docker-v29.3.1):
//! probes a configured interval apart, and the start interval within the start period
//! while the microVM is starting; each probe's result recorded, the last five kept;
//! failures counted in a streak that makes it unhealthy at the retries, except within the
//! start period while it has never been healthy. Like dockerd's, it lives in the daemon's
//! memory, not on disk: a probe a few seconds apart writes nothing.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;
use std::time::Instant;

use std::io::Read as _;

use shards_api::{Follow, Followed, ProcessFailure, Record, Stall, Unfollowed};
use shards_ipc::Health as Config;
use shards_ipc::fd::AsFd as _;

use super::{RunState, Threads, lock};

/// What dockerd uses where a check leaves a setting at 0.
const INTERVAL: Duration = Duration::from_secs(30);
const TIMEOUT: Duration = Duration::from_secs(30);
const START_INTERVAL: Duration = Duration::from_secs(5);
const RETRIES: u64 = 3;
/// The bytes of a probe's output kept, and the results kept.
pub(super) const MAX_OUTPUT: usize = 4096;
const MAX_LOG: usize = 5;

/// How a microVM's health checks have gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Starting,
    Healthy,
    Unhealthy,
}

impl Status {
    /// As `docker ps` shows it after "Up ...".
    pub(super) fn shown(self) -> &'static str {
        match self {
            Status::Starting => "health: starting",
            Status::Healthy => "healthy",
            Status::Unhealthy => "unhealthy",
        }
    }
}

/// When the checks of running microVMs are due, soonest first, for one thread to run
/// them (`check_all`): a thread a microVM with a check would bound the microVMs the
/// daemon can check to the threads a process may have (PM M89), and wake every one of
/// them at each run's start and end.
#[derive(Default)]
pub(super) struct Checks {
    due: Mutex<BinaryHeap<Reverse<(Instant, String)>>>,
    changed: Condvar,
    started: AtomicBool,
    ended: AtomicBool,
}

impl Checks {
    /// Ends the checks' thread: a test's daemon's, as its scope ends.
    #[cfg(test)]
    pub(super) fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        let _due = lock(&self.due);
        self.changed.notify_all();
    }
}

/// How an exec run by the daemon itself ended: its status if it did, and why the daemon
/// ended it, where its wait did.
pub(super) struct Ended {
    pub status: Option<i64>,
    pub cut: Option<Cut>,
}

/// What an exec run by the daemon itself said ([`Daemon::exec_quietly`]): its output, cut
/// at its limit (`more` if there was more), its status if it ended, and why the daemon
/// ended it, where its wait did.
pub(super) struct Quiet {
    pub output: Vec<u8>,
    pub more: bool,
    pub status: Option<i64>,
    pub cut: Option<Cut>,
}

/// Why the daemon ended an exec of its own, told SIGKILL, before its end came: what its
/// [`Wait`] came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cut {
    /// Its [`Wait::For`] passed with no end.
    Timeout(Duration),
    /// Its microVM's process ran no CPU at all over a whole W* or more since the look before,
    /// past the slowest answer of the exec's kind seen ([`Wait::WhileRuns`]).
    Stalled(Stall),
    /// Its microVM's process could not be read while it lived ([`Wait::WhileRuns`]): never
    /// taken for no progress.
    Unread { pid: u32, failure: ProcessFailure },
}

impl From<Unfollowed> for Cut {
    fn from(unfollowed: Unfollowed) -> Cut {
        match unfollowed {
            Unfollowed::Stalled(stall) => Cut::Stalled(stall),
            Unfollowed::Process { pid, failure, .. } => Cut::Unread { pid, failure },
        }
    }
}

/// Its cause, as a log or a client is told it: "{cut}, and so it was ended".
impl std::fmt::Display for Cut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Cut::Timeout(timeout) => write!(f, "no answer within {timeout:?}"),
            Cut::Stalled(stall) => write!(
                f,
                "its microVM ran no CPU in {:?} past the slowest answer seen ({:?}), {:?} into the \
                 wait (the least a microVM ran and still answered: {:?} in {:?})",
                stall.over(),
                stall.slowest(),
                stall.waited(),
                stall.least().cpu,
                stall.least().over
            ),
            Cut::Unread { pid, failure } => {
                write!(f, "its microVM's process {pid} could not be read: {failure}")
            }
        }
    }
}

/// What this daemon has learned, in its memory, of each kind of answer it waits on from a
/// microVM's init while the microVM's process runs ([`Wait::WhileRuns`]): each kind apart, as
/// each takes its own time (D188; D215's records). None is in source: each is cold until its
/// kind's first answer.
#[derive(Debug)]
pub(super) struct Answers {
    /// What it holds cold given back (`RECLAIM`).
    pub reclaim: Record,
    /// The room its budget leaves it, told (`ROOM`).
    pub room: Record,
    /// Its run's metrics, read (`METRICS`).
    pub metrics: Record,
    /// A starting VM's READY (`daemon::ready`).
    pub ready: Record,
    /// A visit's VM holding its files, its STARTED after its RUN (`visit::await_holding`).
    pub held: Record,
    /// Each request for access a starting VM makes, from the answer before or its spawn
    /// (`daemon::grant`, macOS).
    #[cfg(target_os = "macos")]
    pub grants: Record,
    /// Each of init's builtins its quiet work asks (`exec_answered`: `STAT`, `TOP`,
    /// `STATS`, a member's ports, a network's switch, a layer's size, an update), by its
    /// byte.
    pub builtins: Box<[Record; 256]>,
}

impl Default for Answers {
    fn default() -> Answers {
        Answers {
            reclaim: Record::new(),
            room: Record::new(),
            metrics: Record::new(),
            ready: Record::new(),
            held: Record::new(),
            #[cfg(target_os = "macos")]
            grants: Record::new(),
            builtins: Box::new([const { Record::new() }; 256]),
        }
    }
}

/// One probe's result: when it ran, its exit code (-1 if it timed out or could not run),
/// and what it wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Probe {
    pub start_ns: u128,
    pub end_ns: u128,
    pub exit_code: i64,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct State {
    pub status: Status,
    pub failing_streak: u64,
    pub log: VecDeque<Probe>,
}

impl Default for State {
    fn default() -> State {
        State {
            status: Status::Starting,
            failing_streak: 0,
            log: VecDeque::with_capacity(MAX_LOG),
        }
    }
}

/// A setting in nanoseconds, or `default` where it is 0 (timeoutWithDefault).
fn or(ns: i64, default: Duration) -> Duration {
    match u64::try_from(ns) {
        Ok(0) | Err(_) => default,
        Ok(ns) => Duration::from_nanos(ns),
    }
}

/// The command a check runs, if it runs one: `CMD`'s words, or `CMD-SHELL`'s command in
/// `shell`; none for `NONE`, an empty test, or a kind dockerd does not know (getProbe).
pub(super) fn command(cfg: &Config, shell: &[String]) -> Option<Vec<String>> {
    let (kind, rest) = cfg.test.split_first()?;
    match kind.as_str() {
        "CMD" => Some(rest.to_vec()),
        "CMD-SHELL" => Some(shell.iter().chain(rest).cloned().collect()),
        _ => None,
    }
}

/// How long one probe may run.
pub(super) fn timeout(cfg: &Config) -> Duration {
    or(cfg.timeout, TIMEOUT)
}

/// How long to wait for the next probe, `since_start` after the microVM started
/// (getInterval): the start interval within the start period while it is starting.
pub(super) fn interval(cfg: &Config, since_start: Duration, status: Status) -> Duration {
    let start_period = or(cfg.start_period, Duration::ZERO);
    if since_start < start_period && status == Status::Starting {
        or(cfg.start_interval, START_INTERVAL)
    } else {
        or(cfg.interval, INTERVAL)
    }
}

impl State {
    /// Records `probe`, which began `since_start` after the microVM started
    /// (handleProbeResult): a success makes it healthy and ends the streak; a failure,
    /// any code but 0, adds to the streak, unless the microVM is still starting within
    /// its start period, and makes it unhealthy once the streak reaches the retries.
    pub(super) fn record(&mut self, cfg: &Config, probe: Probe, since_start: Duration) {
        if self.log.len() >= MAX_LOG {
            self.log.pop_front();
        }
        let healthy = probe.exit_code == 0;
        self.log.push_back(probe);
        if healthy {
            self.failing_streak = 0;
            self.status = Status::Healthy;
            return;
        }
        let start_period = or(cfg.start_period, Duration::ZERO);
        if self.status == Status::Starting && since_start < start_period {
            return;
        }
        self.failing_streak += 1;
        let retries = u64::try_from(cfg.retries)
            .ok()
            .filter(|&r| r > 0)
            .unwrap_or(RETRIES);
        if self.failing_streak >= retries {
            self.status = Status::Unhealthy;
        }
    }
}

/// A probe's output as inspect's `Health.Log` answers it (D170, moby's limitedBuffer):
/// its first [`MAX_OUTPUT`] bytes, then `...` if it wrote more, as text.
pub(super) fn kept_output(bytes: &[u8], more: bool) -> String {
    let mut out = String::from_utf8_lossy(bytes).into_owned();
    if more {
        out.push_str("...");
    }
    out
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// Checks microVM `id`'s health while it runs: one probe at a time, so that a slow
    /// probe never piles up beside the next, the next an interval after the last ended; on
    /// the checks' thread, which the first starts. Nothing for a microVM without a check.
    pub(super) fn watch_health<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, id: &str) {
        let Some((cfg, _)) = self.check_of(id) else {
            return;
        };
        lock(&self.health).insert(id.to_string(), State::default());
        self.due_after(id, interval(&cfg, Duration::ZERO, Status::Starting));
        let c = &self.checks;
        if !c.started.swap(true, Ordering::SeqCst)
            && let Err(e) = std::thread::Builder::new()
                .name("health".into())
                .spawn_scoped(threads, move || self.check_all(threads))
        {
            c.started.store(false, Ordering::SeqCst);
            super::log(format!("the health checks' thread: {e}"));
        }
    }

    /// Container `id`'s check and the command it runs, while its run goes on.
    fn check_of(&self, id: &str) -> Option<(Config, Vec<String>)> {
        match lock(&self.runs).get(id) {
            Some(RunState::Tracked(run)) => run
                .base
                .health
                .as_ref()
                .and_then(|(cfg, shell)| Some((cfg.clone(), command(cfg, shell)?))),
            _ => None,
        }
    }

    /// Makes microVM `id`'s next probe due `wait` from now.
    fn due_after(&self, id: &str, wait: Duration) {
        let Some(at) = Instant::now().checked_add(wait) else {
            return;
        };
        lock(&self.checks.due).push(Reverse((at, id.to_string())));
        self.checks.changed.notify_all();
    }

    /// How long ago microVM `id` started.
    fn since_start(&self, id: &str) -> Duration {
        let started = lock(&self.containers).get(id).and_then(|c| c.started);
        let ns = started.map_or(0, |s| crate::spec::now().saturating_sub(s));
        Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
    }

    /// Runs each probe as it comes due, on a thread of its own, until [`Checks::end`].
    fn check_all<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let c = &self.checks;
        loop {
            let id = {
                let mut due = lock(&c.due);
                loop {
                    if c.ended.load(Ordering::SeqCst) {
                        return;
                    }
                    let now = Instant::now();
                    due = match due.peek() {
                        None => c.changed.wait(due).unwrap_or_else(PoisonError::into_inner),
                        Some(Reverse((at, _))) if *at > now => {
                            let wait = at.saturating_duration_since(now);
                            super::timed(Some(wait), || c.changed.wait_timeout(due, wait))
                                .unwrap_or_else(PoisonError::into_inner)
                                .0
                        }
                        Some(_) => match due.pop() {
                            Some(Reverse((_, id))) => break id,
                            None => continue,
                        },
                    };
                }
            };
            // A run that has ended is checked no more.
            let Some((cfg, argv)) = self.check_of(&id) else {
                lock(&self.health).remove(&id);
                continue;
            };
            let next = interval(&cfg, self.since_start(&id), Status::Starting);
            // A frozen microVM is not probed: dockerd stops its monitor while a container
            // is paused (moby daemon/health.go, updateHealthMonitor).
            if lock(&self.paused).contains(&id) {
                self.due_after(&id, next);
                continue;
            }
            let watched = id.clone();
            if let Err(e) = std::thread::Builder::new()
                .name("probe".into())
                .spawn_scoped(threads, move || self.probe_once(&watched, &cfg, &argv))
            {
                super::log(format!("container {id}: a health probe's thread: {e}"));
                self.due_after(&id, next);
            }
        }
    }

    /// One probe of microVM `id`, recorded, and the next made due an interval after it
    /// ends; nothing of one that ends after the run did, whose result says nothing of a run
    /// that has gone.
    fn probe_once(&self, id: &str, cfg: &Config, argv: &[String]) {
        let since = self.since_start(id);
        let probe = self.probe(id, argv, cfg);
        if !matches!(lock(&self.runs).get(id), Some(RunState::Tracked(_))) {
            lock(&self.health).remove(id);
            return;
        }
        let (was, status) = {
            let mut health = lock(&self.health);
            let Some(state) = health.get_mut(id) else {
                return;
            };
            let was = state.status;
            state.record(cfg, probe, since);
            (was, state.status)
        };
        // moby daemon/health.go, handleProbeResult: a change of status is an event.
        if status != was {
            let named = match status {
                Status::Starting => "starting",
                Status::Healthy => "healthy",
                Status::Unhealthy => "unhealthy",
            };
            self.container_event(id, format!("health_status: {named}"), &[]);
        }
        self.due_after(id, interval(cfg, self.since_start(id), status));
    }

    /// One probe of microVM `id`: `argv` run beside its workload as an exec without
    /// stdin or a terminal, as the microVM's user in its working directory with its
    /// environment (cmdProbe.run); its stdout and stderr kept together; killed at the
    /// check's timeout, whose result is -1 and says so.
    fn probe(&self, id: &str, argv: &[String], cfg: &Config) -> Probe {
        let start_ns = crate::spec::now();
        let failed = |why: String| Probe {
            start_ns,
            end_ns: crate::spec::now(),
            exit_code: -1,
            output: why,
        };
        let spec = {
            let base = match lock(&self.runs).get(id) {
                Some(RunState::Tracked(run)) => run.base.clone(),
                _ => return failed("the container is not running".into()),
            };
            let base = &base.options;
            let options = crate::spec::Options {
                argv: argv.to_vec(),
                env: base.env.clone(),
                exec_env: Vec::new(),
                workdir: base.workdir.clone(),
                user: base.user.clone(),
                hostname: base.hostname.clone(),
                interactive: false,
                tty: None,
                terms: Vec::new(),
                member: false,
            };
            match crate::spec::spec(&options, |_| None) {
                Ok(spec) => spec,
                Err(e) => return failed(e),
            }
        };
        let timeout = timeout(cfg);
        let quiet = match self.exec_quietly(id, &spec, Some(argv), MAX_OUTPUT, timeout) {
            Ok(q) => q,
            Err(e) => return failed(format!("starting the health check: {e}")),
        };
        let said = kept_output(&quiet.output, quiet.more);
        match (&quiet.cut, quiet.status) {
            (Some(_), _) => {
                let limit = shards_cmdline::gotime::format_duration(
                    i64::try_from(timeout.as_nanos()).unwrap_or(i64::MAX),
                );
                failed(if said.is_empty() {
                    format!("Health check exceeded timeout ({limit})")
                } else {
                    format!("Health check exceeded timeout ({limit}): {said}")
                })
            }
            (None, Some(code)) => Probe {
                start_ns,
                end_ns: crate::spec::now(),
                exit_code: code,
                output: said,
            },
            (None, None) => failed("the microVM ended the check".into()),
        }
    }

    /// An exec of `spec` in microVM `id`, beside its workload, without stdin or a
    /// terminal: its stdout and stderr together, the first `limit` bytes of them kept;
    /// killed once `timeout` has passed, then waited for, so that none outlives its
    /// asker. An error if it could not be sent. A command (`argv`) has its exec events,
    /// as a health probe has dockerd's; init's own work has none.
    pub(super) fn exec_quietly(
        &self,
        id: &str,
        spec: &shards_abi::run::Spec,
        argv: Option<&[String]>,
        limit: usize,
        timeout: Duration,
    ) -> std::io::Result<Quiet> {
        self.exec_waiting(id, spec, argv, limit, Wait::For(timeout))
    }

    /// Init's own work in microVM `id`, `spec` (a builtin, no command), as
    /// [`exec_quietly`](Self::exec_quietly) does it, the first `limit` bytes of its answer
    /// kept: waited for while its microVM's process runs ([`answered_wait`](Self::answered_wait)).
    /// It was `TAKE_TIMEOUT`'s 10 s, picked.
    pub(super) fn exec_answered(
        &self,
        id: &str,
        spec: &shards_abi::run::Spec,
        limit: usize,
    ) -> std::io::Result<Quiet> {
        let vm = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => Some(t.vm.id()),
            _ => None,
        };
        self.exec_waiting(id, spec, None, limit, self.answered_wait(vm, spec.builtin))
    }

    /// How init's answer of `builtin` is waited for in a microVM whose process is `vm`: while
    /// it runs, judged by that builtin's record (D215's judge, [`Wait::WhileRuns`]): before
    /// its first answer, its liveness alone; then past the slowest seen, ended where a whole
    /// one shows it ran no CPU, its host not the reason (R03, ROOT's (3), 2026-11-13). With no
    /// process to follow, its end alone.
    pub(super) fn answered_wait(&self, vm: Option<u32>, builtin: u8) -> Wait<'_> {
        match (vm, self.answers.builtins.get(usize::from(builtin))) {
            (Some(vm), Some(record)) => Wait::WhileRuns(vm, record),
            _ => Wait::Ending,
        }
    }

    /// [`exec_quietly`](Self::exec_quietly), waited for as `wait` says.
    pub(super) fn exec_waiting(
        &self,
        id: &str,
        spec: &shards_abi::run::Spec,
        argv: Option<&[String]>,
        limit: usize,
        wait: Wait<'_>,
    ) -> std::io::Result<Quiet> {
        let mut output = Vec::with_capacity(limit.min(1 << 16));
        let mut more = false;
        let ended = self.exec_followed(id, spec, argv, wait, &mut |chunk| {
            let room = limit.saturating_sub(output.len());
            output.extend_from_slice(chunk.get(..room.min(chunk.len())).unwrap_or_default());
            more |= chunk.len() > room;
            true
        })?;
        Ok(Quiet {
            output,
            more,
            status: ended.status,
            cut: ended.cut,
        })
    }

    /// [`exec_quietly`](Self::exec_quietly), its output handed to `sink` as it comes,
    /// unlimited; killed, then waited for, once `timeout` (if any) has passed or `sink`
    /// wants no more.
    pub(super) fn exec_streamed(
        &self,
        id: &str,
        spec: &shards_abi::run::Spec,
        argv: Option<&[String]>,
        timeout: Option<Duration>,
        sink: &mut (dyn FnMut(&[u8]) -> bool + Send),
    ) -> std::io::Result<Ended> {
        self.exec_followed(id, spec, argv, timeout.map_or(Wait::Ending, Wait::For), sink)
    }

    /// [`exec_streamed`](Self::exec_streamed), waited for as `wait` says.
    fn exec_followed(
        &self,
        id: &str,
        spec: &shards_abi::run::Spec,
        argv: Option<&[String]>,
        wait: Wait<'_>,
        sink: &mut (dyn FnMut(&[u8]) -> bool + Send),
    ) -> std::io::Result<Ended> {
        let (mut out, into) = std::io::pipe()?;
        let null = std::fs::File::open(crate::os::NULL)?;
        let ours = self.send_exec(id, spec, argv, false, [null.as_fd(), into.as_fd(), into.as_fd()])?;
        // The VM holds the rest: its end sees the output end.
        drop((into, null));
        Ok(std::thread::scope(|scope| {
            let control = &ours;
            let reader = scope.spawn(move || {
                let mut buf = [0u8; 16 << 10];
                while let Ok(n @ 1..) = out.read(&mut buf) {
                    if !sink(buf.get(..n).unwrap_or_default()) {
                        // Nobody wants the rest: the exec goes, and its output with it.
                        kill(control);
                        break;
                    }
                }
            });
            let ended = await_exit(&ours, wait);
            let _ = reader.join();
            ended
        }))
    }

    /// An exec of `spec` in microVM `id` on the asker's own descriptors: what it
    /// reads (with `input`, its stdin; else nothing), and where its output and errors
    /// go; waited for as `wait` says.
    pub(super) fn exec_on(
        &self,
        id: &str,
        spec: &shards_abi::run::Spec,
        input: bool,
        stdio: [shards_ipc::fd::BorrowedFd<'_>; 3],
        wait: Wait<'_>,
    ) -> std::io::Result<Ended> {
        let ours = self.send_exec(id, spec, None, input, stdio)?;
        Ok(await_exit(&ours, wait))
    }

    /// Starts the NOTICE built-in of the guest of the run whose inbox is `inbox`, which the
    /// caller holds (D235): its stdin `pipe`, on which a share process tells what its host
    /// changes, its output the daemon's log; the connection its end is told on, to be held
    /// while it runs. Sent as [`send_exec`](Self::send_exec) sends an exec, through the
    /// inbox itself.
    pub(super) fn notice_exec(
        &self,
        inbox: &mut super::Inbox,
        pipe: shards_ipc::fd::OwnedFd,
    ) -> std::io::Result<shards_ipc::Stream> {
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::NOTICE,
            ..Default::default()
        };
        let (ours, theirs) = shards_ipc::Stream::pair()?;
        let held = theirs.try_clone()?;
        let handed = shards_ipc::handed(&theirs)?;
        let number = self.next_exec.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut payload = Vec::with_capacity(9 + spec.encoded_len().unwrap_or(0));
        payload.extend_from_slice(&number.to_be_bytes());
        payload.push(shards_ipc::EXEC_INTERACTIVE);
        spec.encode_into(&mut payload);
        inbox.execs_in_flight.push((number, held));
        let log = std::io::stderr();
        let fds = [handed.as_fd(), pipe.as_fd(), log.as_fd(), log.as_fd()];
        if let Err(e) = inbox.socket.send(shards_ipc::kind::EXEC_RUN, &payload, &fds) {
            inbox.execs_in_flight.retain(|(n, _)| *n != number);
            return Err(e);
        }
        Ok(ours)
    }

    /// [`notice_exec`](Self::notice_exec) of running microVM `id`, its connection held
    /// with its run: a share plugged into it as it runs (D235).
    pub(super) fn notice_into(&self, id: &str, pipe: shards_ipc::fd::OwnedFd) -> std::io::Result<()> {
        let inbox = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(run)) => run.inbox.clone(),
            _ => return Err(std::io::Error::other("the container is not running")),
        };
        let mut inbox = lock(&inbox);
        let told = self.notice_exec(&mut inbox, pipe)?;
        inbox.notices.push(told);
        Ok(())
    }

    /// Sends microVM `id`'s VM an exec of `spec` with `stdio`, logging its events if
    /// it is a command (`argv`); returns the connection its end is told on.
    fn send_exec(
        &self,
        id: &str,
        spec: &shards_abi::run::Spec,
        argv: Option<&[String]>,
        input: bool,
        stdio: [shards_ipc::fd::BorrowedFd<'_>; 3],
    ) -> std::io::Result<shards_ipc::Stream> {
        let (ours, theirs) = shards_ipc::Stream::pair()?;
        let flags = if input { shards_ipc::EXEC_INTERACTIVE } else { 0 };
        self.hand_exec(id, spec, argv, flags, stdio, &theirs)?;
        Ok(ours)
    }

    /// Hands microVM `id`'s VM an exec of `spec` (`EXEC_RUN`, its `flags`) with `stdio` and
    /// `theirs`, its client's end of the connection its end is told on, logging its events if
    /// it is a command (`argv`).
    pub(super) fn hand_exec(
        &self,
        id: &str,
        spec: &shards_abi::run::Spec,
        argv: Option<&[String]>,
        flags: u8,
        stdio: [shards_ipc::fd::BorrowedFd<'_>; 3],
        theirs: &shards_ipc::Stream,
    ) -> std::io::Result<()> {
        let held = theirs.try_clone()?;
        let handed = shards_ipc::handed(theirs)?;
        // Taken out of `runs` before the inbox is locked (commands.rs, exec).
        let (socket, inbox) = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(run)) => (run.socket.clone(), run.inbox.clone()),
            _ => return Err(std::io::Error::other("the container is not running")),
        };
        let number = self.next_exec.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut payload = Vec::with_capacity(9 + spec.encoded_len().unwrap_or(0));
        payload.extend_from_slice(&number.to_be_bytes());
        payload.push(flags);
        spec.encode_into(&mut payload);
        let exec_id = argv.and_then(|argv| self.exec_events(id, argv));
        {
            let mut inbox = lock(&inbox);
            inbox.execs_in_flight.push((number, held));
            if let Some(exec_id) = exec_id {
                inbox.exec_ids.push((number, exec_id));
            }
        }
        let [stdin, stdout, stderr] = stdio;
        let fds = [handed.as_fd(), stdin, stdout, stderr];
        if let Err(e) = socket.send(shards_ipc::kind::EXEC_RUN, &payload, &fds) {
            let mut held = lock(&inbox);
            held.execs_in_flight.retain(|(n, _)| *n != number);
            held.exec_ids.retain(|(n, _)| *n != number);
            return Err(e);
        }
        Ok(())
    }
}

/// Ends an exec run by the daemon: SIGKILL, on the connection its end is told on.
fn kill(conn: &shards_ipc::Stream) {
    let _ = shards_ipc::send(conn, shards_ipc::kind::SIGNAL, &9u32.to_be_bytes(), &[]);
}

/// How long the daemon waits for the end of an exec of its own.
#[derive(Debug, Clone, Copy)]
pub(super) enum Wait<'a> {
    /// Until it ends.
    Ending,
    /// This long with no word of it, then it is ended.
    For(Duration),
    /// While VM process `.0` lives, followed as shards-api follows a guest's VM process
    /// (D215's judge), by the record `.1` of the exec's kind of answer ([`Answers`]), which
    /// its own answer teaches: before its kind's first answer, liveness alone; then past the
    /// slowest answer seen (W*), looked at once each W* and ended where it ran no CPU at all
    /// since the look before ([`Cut::Stalled`]), or where it could not be read
    /// ([`Cut::Unread`]). A microVM its host holds at a `memory.high` is slowed there for as
    /// long as it is over it, and the answer of one asked to give back what would bring it
    /// under comes as its host lets it run (D188; PM M467). An answer there as the wait
    /// begins teaches nothing: it was waited for not at all (D222).
    WhileRuns(u32, &'a Record),
}

/// Waits on `ours` for an exec's end, ending it (SIGKILL) as `wait` says, and waiting on for
/// its end then, so that none outlives its asker.
fn await_exit(ours: &shards_ipc::Stream, wait: Wait<'_>) -> Ended {
    let (mut follow, mut cut) = match wait {
        // Its answer there as its wait begins was waited for not at all: read as it is, it
        // teaches its kind nothing (D222's wait of nothing), as a slot's rest is learned only
        // where it was waited for (D185's `Asked::waited`).
        Wait::WhileRuns(..) if super::readable_within(ours, Some(Duration::ZERO)) => (None, None),
        Wait::WhileRuns(vm, record) => match following(vm, record) {
            Ok(follow) => (follow, None),
            Err(cut) => (None, Some(cut)),
        },
        Wait::Ending | Wait::For(_) => (None, None),
    };
    if cut.is_some() {
        kill(ours);
    }
    let status = loop {
        let read_for = match (&cut, wait) {
            // Ended, or with no clock: until its end.
            (Some(_), _) | (None, Wait::Ending) => None,
            (None, Wait::For(timeout)) => Some(timeout),
            // Until the judge's next look, made first where it is due now: none while the
            // wait follows liveness alone (its kind cold, or its process gone).
            (None, Wait::WhileRuns(..)) => {
                let left = follow
                    .as_ref()
                    .and_then(Follow::due)
                    .map(|due| due.saturating_duration_since(Instant::now()));
                if left.is_some_and(|left| left.is_zero()) {
                    if let Some(Err(why)) = follow.as_mut().map(Follow::look) {
                        cut = Some(Cut::from(why));
                        kill(ours);
                    }
                    continue;
                }
                left
            }
        };
        let _ = ours.set_read_timeout(read_for);
        match shards_ipc::recv(ours) {
            Ok(Some(m)) if m.kind == shards_ipc::kind::EXIT => {
                // Its own answer, not an end the daemon made: its kind learns from it.
                if cut.is_none()
                    && let Some(follow) = follow.as_mut()
                {
                    follow.answered();
                }
                break m.payload.first().map(|&s| i64::from(s));
            }
            Ok(Some(_)) => {}
            Err(e)
                if cut.is_none()
                    && matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
            {
                match wait {
                    Wait::For(timeout) => {
                        cut = Some(Cut::Timeout(timeout));
                        kill(ours);
                    }
                    // Its look is due: made as the loop goes round.
                    Wait::WhileRuns(..) => {}
                    // A read with no timeout set times out only on a descriptor that does not
                    // block, which ends the wait as any failed read does.
                    Wait::Ending => break None,
                }
            }
            Ok(None) | Err(_) => break None,
        }
    };
    Ended { status, cut }
}

/// A wait on VM process `vm` judged by `record`, begun now: none where the process has gone
/// already, whose end is the exec's; why not, where it cannot be read.
fn following(vm: u32, record: &Record) -> Result<Option<Follow<'_>>, Cut> {
    match Followed::of_pid(vm).map_err(|failure| Cut::Unread { pid: vm, failure })? {
        Some(followed) => Follow::begin(record, followed).map_err(Cut::from),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record of one kind of answer, the slowest `slowest` long, its VM process having run
    /// a thousandth of it meanwhile.
    #[cfg(unix)]
    fn learned(slowest: Duration) -> Record {
        let record = Record::new();
        record.teach(slowest, slowest / 1000);
        record
    }

    /// Waits for `wait` in an exec whose VM side `theirs` is answered by `answer`, on a
    /// thread of its own.
    fn waited(wait: Wait<'_>, answer: impl FnOnce(shards_ipc::Stream) + Send) -> Ended {
        let (ours, theirs) = shards_ipc::Stream::pair().unwrap();
        std::thread::scope(|s| {
            s.spawn(|| answer(theirs));
            await_exit(&ours, wait)
        })
    }

    /// The exec's answer, `status`, said `after` its wait began.
    fn answers(status: u8, after: Duration) -> impl FnOnce(shards_ipc::Stream) + Send {
        move |theirs| {
            std::thread::sleep(after);
            shards_ipc::send(&theirs, shards_ipc::kind::EXIT, &[status], &[]).unwrap();
        }
    }

    /// The kill a wait sends, then the end, 137, it waits on for.
    fn killed(theirs: shards_ipc::Stream) {
        let said = shards_ipc::recv(&theirs).unwrap().unwrap();
        assert_eq!(said.kind, shards_ipc::kind::SIGNAL);
        assert_eq!(said.payload, 9u32.to_be_bytes());
        shards_ipc::send(&theirs, shards_ipc::kind::EXIT, &[137], &[]).unwrap();
    }

    #[cfg(unix)]
    fn sleeper() -> Killed {
        Killed(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        )
    }

    /// An exec whose microVM's process runs nothing, its kind of answer not yet seen, is
    /// waited for until it answers: a cold wait follows the process's liveness alone, and its
    /// answer teaches its kind (D215's judge; red with PM M357's `IDLE_VM_CPU`, which ended it
    /// at its first 10 s, here a fifth of a second).
    #[cfg(unix)]
    #[test]
    fn a_cold_wait_follows_its_microvm_while_it_lives() {
        let idle = sleeper();
        let record = Record::new();
        let after = Duration::from_millis(800);
        let ended = waited(Wait::WhileRuns(idle.0.id(), &record), answers(7, after));
        assert_eq!((ended.status, ended.cut), (Some(7), None));
        let shards_api::Seen::Learned { slowest, .. } = record.seen() else {
            panic!("its answer taught nothing");
        };
        assert!(slowest >= after, "{slowest:?}");
    }

    /// An exec's answer there as its wait begins was waited for not at all: it is taken, and
    /// teaches its kind nothing (D222's wait of nothing), whose W* of next to nothing would
    /// have its kind's next asks looked at each microsecond. Its microVM's process is this
    /// one, which lives throughout.
    #[test]
    fn an_exit_said_before_its_wait_teaches_nothing() {
        let record = Record::new();
        let (ours, theirs) = shards_ipc::Stream::pair().unwrap();
        shards_ipc::send(&theirs, shards_ipc::kind::EXIT, &[7], &[]).unwrap();
        let ended = await_exit(&ours, Wait::WhileRuns(std::process::id(), &record));
        assert_eq!((ended.status, ended.cut), (Some(7), None));
        assert_eq!(record.seen(), shards_api::Seen::Cold);
    }

    /// Set in the child of `a_microvm_that_runs_at_any_rate_is_waited_for`: it runs a CPU a
    /// fifth of a millisecond each tenth of a second, until it is killed. Unix's, as its test is.
    #[cfg(unix)]
    const TRICKLE: &str = "SHARDS_HEALTH_TRICKLE";

    /// A microVM process that runs at any rate is waited for while it runs, past its kind's
    /// slowest answer: here a fiftieth of a CPU, below PM M357's idle microVM's 63 ms in
    /// 10 s, with an answer three and a half times the slowest seen (D215's judge; red with
    /// `IDLE_VM_CPU`, which ended it at its first look).
    #[cfg(unix)]
    #[test]
    fn a_microvm_that_runs_at_any_rate_is_waited_for() {
        if std::env::var_os(TRICKLE).is_some() {
            loop {
                let began = Instant::now();
                while began.elapsed() < Duration::from_micros(200) {
                    std::hint::spin_loop();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let trickler = Killed(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "daemon::health::tests::a_microvm_that_runs_at_any_rate_is_waited_for",
                    "--nocapture",
                ])
                .env(TRICKLE, "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let slowest = Duration::from_secs(1);
        let record = learned(slowest);
        let ended = waited(
            Wait::WhileRuns(trickler.0.id(), &record),
            answers(7, slowest * 7 / 2),
        );
        assert_eq!((ended.status, ended.cut), (Some(7), None));
        let shards_api::Seen::Learned { slowest: now, .. } = record.seen() else {
            panic!("its record was lost");
        };
        assert!(now >= slowest * 7 / 2, "its answer, slower, raised W*: {now:?}");
    }

    /// Past its kind's slowest answer, an exec whose microVM's process runs is waited for, and
    /// one whose process ran no CPU at all over a whole W* is ended: told SIGKILL, its end
    /// still waited for, the stall said typed (how far in, over how long, R* and W*), and
    /// nothing taught by an end the daemon made (D215's judge; PM M467).
    #[cfg(unix)]
    #[test]
    fn an_exec_is_ended_once_its_microvm_runs_nothing_for_a_whole_w_star() {
        let slowest = Duration::from_millis(50);
        let record = learned(slowest);
        let busy = std::sync::atomic::AtomicBool::new(true);
        std::thread::scope(|s| {
            s.spawn(|| {
                while busy.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            });
            let ended = waited(
                Wait::WhileRuns(std::process::id(), &record),
                answers(7, slowest * 8),
            );
            busy.store(false, Ordering::Relaxed);
            assert_eq!((ended.status, ended.cut), (Some(7), None));
        });
        let record = learned(slowest);
        let idle = sleeper();
        let ended = waited(Wait::WhileRuns(idle.0.id(), &record), killed);
        assert_eq!(ended.status, Some(137));
        let Some(Cut::Stalled(stall)) = &ended.cut else {
            panic!("{:?}", ended.cut);
        };
        assert_eq!(stall.slowest(), slowest);
        assert!(stall.over() >= slowest, "{stall:?}");
        assert!(stall.waited() >= stall.over(), "{stall:?}");
        assert_eq!(stall.least().over, slowest);
        assert_eq!(
            record.seen(),
            learned(slowest).seen(),
            "an end it made taught nothing"
        );
    }

    /// A microVM process that cannot be read is never taken for one that ran nothing: its
    /// exec is ended at once, saying why, typed (D215's `Process`). macOS's reader refuses a
    /// pid past a C int's; Linux's finds no such process, whose wait follows its liveness.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unread_microvm_process_ends_its_exec_typed() {
        let record = learned(Duration::from_millis(50));
        let ended = waited(Wait::WhileRuns(u32::MAX, &record), killed);
        assert_eq!(
            (ended.status, ended.cut),
            (
                Some(137),
                Some(Cut::Unread {
                    pid: u32::MAX,
                    failure: shards_api::ProcessFailure::PidOutOfRange {
                        operation: shards_api::ProcessOperation::ProcPidRusage
                    }
                })
            )
        );
    }

    /// A wait with a timeout ends its exec once that passes with no word of it, saying so;
    /// one with none waits for its end.
    #[test]
    fn a_timed_wait_ends_its_exec_and_an_untimed_one_waits() {
        let timeout = Duration::from_millis(50);
        let ended = waited(Wait::For(timeout), killed);
        assert_eq!(
            (ended.status, ended.cut),
            (Some(137), Some(Cut::Timeout(timeout)))
        );
        let ended = waited(Wait::Ending, answers(0, timeout * 4));
        assert_eq!((ended.status, ended.cut), (Some(0), None));
    }

    /// A child killed as it drops, of the Unix tests.
    #[cfg(unix)]
    struct Killed(std::process::Child);

    #[cfg(unix)]
    impl Drop for Killed {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn probe(exit_code: i64) -> Probe {
        Probe {
            start_ns: 0,
            end_ns: 0,
            exit_code,
            output: String::new(),
        }
    }

    const SECOND: i64 = 1_000_000_000;

    /// handleProbeResult's rules, case by case.
    #[test]
    fn probe_results_move_the_health_status() {
        let cfg = Config {
            retries: 2,
            ..Config::default()
        };
        let mut s = State::default();
        s.record(&cfg, probe(1), Duration::from_secs(1));
        assert_eq!((s.status, s.failing_streak), (Status::Starting, 1));
        s.record(&cfg, probe(3), Duration::from_secs(2));
        assert_eq!((s.status, s.failing_streak), (Status::Unhealthy, 2));
        s.record(&cfg, probe(0), Duration::from_secs(3));
        assert_eq!((s.status, s.failing_streak), (Status::Healthy, 0));
        // Retries 0 is dockerd's 3.
        let mut s = State::default();
        for _ in 0..2 {
            s.record(&Config::default(), probe(-1), Duration::ZERO);
        }
        assert_eq!(s.status, Status::Starting);
        s.record(&Config::default(), probe(-1), Duration::ZERO);
        assert_eq!(s.status, Status::Unhealthy);
    }

    /// Failures within the start period count only once the microVM has been healthy.
    #[test]
    fn the_start_period_forgives_failures_until_the_first_success() {
        let cfg = Config {
            start_period: 10 * SECOND,
            retries: 1,
            ..Config::default()
        };
        let mut s = State::default();
        s.record(&cfg, probe(1), Duration::from_secs(5));
        assert_eq!((s.status, s.failing_streak), (Status::Starting, 0));
        s.record(&cfg, probe(0), Duration::from_secs(6));
        s.record(&cfg, probe(1), Duration::from_secs(7));
        assert_eq!((s.status, s.failing_streak), (Status::Unhealthy, 1));
        let mut late = State::default();
        late.record(&cfg, probe(1), Duration::from_secs(11));
        assert_eq!(late.status, Status::Unhealthy);
    }

    #[test]
    fn the_last_five_results_are_kept() {
        let mut s = State::default();
        for code in 0..8 {
            s.record(&Config::default(), probe(code), Duration::ZERO);
        }
        let kept: Vec<i64> = s.log.iter().map(|p| p.exit_code).collect();
        assert_eq!(kept, [3, 4, 5, 6, 7]);
    }

    /// getInterval and the defaults of timeoutWithDefault.
    #[test]
    fn probes_come_at_their_intervals() {
        let cfg = Config {
            interval: 2 * SECOND,
            start_period: 10 * SECOND,
            start_interval: SECOND,
            ..Config::default()
        };
        let at = Duration::from_secs;
        assert_eq!(interval(&cfg, at(1), Status::Starting), at(1));
        assert_eq!(interval(&cfg, at(1), Status::Healthy), at(2));
        assert_eq!(interval(&cfg, at(11), Status::Starting), at(2));
        assert_eq!(interval(&Config::default(), at(0), Status::Starting), at(30));
        let started = Config {
            start_period: 10 * SECOND,
            ..Config::default()
        };
        assert_eq!(interval(&started, at(0), Status::Starting), at(5));
        assert_eq!(timeout(&Config::default()), at(30));
    }

    #[test]
    fn checks_run_their_command_or_none() {
        let sh = ["/bin/sh".to_string(), "-c".to_string()];
        let cfg = |t: &[&str]| Config {
            test: t.iter().map(|s| s.to_string()).collect(),
            ..Config::default()
        };
        assert_eq!(
            command(&cfg(&["CMD", "a", "b"]), &sh),
            Some(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            command(&cfg(&["CMD-SHELL", "a b"]), &sh),
            Some(vec!["/bin/sh".into(), "-c".into(), "a b".into()])
        );
        for none in [&["NONE"][..], &[], &["WHAT", "x"]] {
            assert_eq!(command(&cfg(none), &sh), None);
        }
    }

    #[test]
    fn output_is_kept_to_its_limit() {
        assert_eq!(kept_output(b"ok", false), "ok");
        assert_eq!(kept_output(b"ok", true), "ok...");
    }
}

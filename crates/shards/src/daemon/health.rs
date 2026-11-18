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
                 wait ({}){}",
                stall.over(),
                stall.slowest(),
                stall.waited(),
                stall.least().map_or_else(
                    || "no answer of its kind had its CPU read".to_string(),
                    |least| format!(
                        "the least a microVM ran and still answered: {} in {:?}",
                        least.work, least.over
                    )
                ),
                stall.account()
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
    /// A warm VM's taking of a run: the run's handoff sent, to its TAKEN (`daemon::hand_over`).
    pub take: Record,
    /// A microVM's join share's taking of a joiner's link: its door's `JOIN_LINK` sent
    /// (`daemon::give_join_volumes`).
    pub door: Record,
    /// A join share's taking of a joiner's volume: its `JOIN_VOLUME` sent, to its `TAKEN`
    /// (`daemon::give_join_volume`).
    pub link: Record,
    /// A visit's VM holding its files, its STARTED after its RUN (`visit::await_holding`).
    pub held: Record,
    /// Room in a run's socket for what the daemon sends its VM (`daemon::RunSocket`, Unix):
    /// one record for every run's, as a run's own would be cold each run.
    pub run_send: Record,
    /// A VM's guest times, its `GUEST_TIMED` after its `GUEST_TIME` (`commands::guest_times_of`).
    pub guest_time: Record,
    /// A run's VM process's end once its command's end has come, which its told end, volumes
    /// and share processes wait for on Windows (`daemon::await_parked`, D281).
    #[cfg(windows)]
    pub vm_end: Record,
    /// A starting VM's launch, its spawn to its first request for access (`daemon::launch`,
    /// macOS): the host's to admit (PM M166), followed by its liveness alone, said once past
    /// its slowest.
    #[cfg(target_os = "macos")]
    pub launches: Record,
    /// Each request for access a starting VM makes after its first, from the answer before
    /// (`daemon::grant`, macOS).
    #[cfg(target_os = "macos")]
    pub grants: Record,
    /// Each of init's builtins its quiet work asks (`exec_answered`: `STAT`, `TOP`,
    /// `STATS`, a member's ports, a network's switch, a layer's size, an update), by its
    /// byte.
    pub builtins: Box<[Record; 256]>,
    /// A network's switch (`NETWORK`) asked each of [`NETWORK_VERBS`], each its own kind: a
    /// `make` records a network in init's memory, a `connect` gives a running member its
    /// address in three namespaces and its grants again (init links.rs). One record of them
    /// all took makes' answers for a connect's slowest (CI 43968984258: a connect ended 5.7 ms
    /// into a stillness, past makes' 3.0 ms).
    pub networks: [Record; NETWORK_VERBS.len()],
}

/// What a network's switch is asked (init members.rs `network`), each judged by its own
/// record ([`Answers::networks`]).
pub(super) const NETWORK_VERBS: [&str; 5] = ["default", "make", "remove", "connect", "disconnect"];

impl Default for Answers {
    fn default() -> Answers {
        Answers {
            reclaim: Record::new(),
            room: Record::new(),
            metrics: Record::new(),
            ready: Record::new(),
            take: Record::new(),
            door: Record::new(),
            link: Record::new(),
            held: Record::new(),
            run_send: Record::new(),
            guest_time: Record::new(),
            #[cfg(windows)]
            vm_end: Record::new(),
            #[cfg(target_os = "macos")]
            launches: Record::new(),
            #[cfg(target_os = "macos")]
            grants: Record::new(),
            builtins: Box::new([const { Record::new() }; 256]),
            networks: [const { Record::new() }; NETWORK_VERBS.len()],
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
            && let Err(e) = crate::daemon::DaemonThread::new()
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
            if let Err(e) = crate::daemon::DaemonThread::new()
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
        self.exec_waiting(id, spec, None, limit, self.answered_wait(vm, spec))
    }

    /// How init's answer of `spec`'s builtin is waited for in a microVM whose process is
    /// `vm`: while it runs, judged by its kind's record (D215's judge, [`Wait::WhileRuns`]),
    /// the builtin's, or a network verb's own ([`Answers::networks`]): before its first
    /// answer, its liveness alone; then past the slowest seen, ended where a whole one shows it
    /// ran no CPU, its host not the reason (R03, ROOT's (3), 2026-11-13). With no process to
    /// follow, its end alone.
    pub(super) fn answered_wait(&self, vm: Option<u32>, spec: &shards_abi::run::Spec) -> Wait<'_> {
        let verb = match (
            spec.builtin == shards_abi::run::builtin::NETWORK,
            spec.argv.first(),
        ) {
            (true, Some(verb)) => NETWORK_VERBS.iter().position(|v| v.as_bytes() == verb.as_slice()),
            _ => None,
        };
        let record = match verb {
            Some(at) => self.answers.networks.get(at),
            None => self.answers.builtins.get(usize::from(spec.builtin)),
        };
        match (vm, record) {
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
        let begun = Begun::now(wait);
        let ours = self.send_exec(id, spec, argv, false, [null.as_fd(), into.as_fd(), into.as_fd()])?;
        // The VM holds the rest: its end sees the output end.
        drop((into, null));
        std::thread::scope(|scope| {
            let control = &ours;
            let reader = crate::daemon::DaemonThread::new()
                .name("exec output".into())
                .spawn_scoped(scope, move || {
                    let mut buf = [0u8; 16 << 10];
                    while let Ok(n @ 1..) = out.read(&mut buf) {
                        if !sink(buf.get(..n).unwrap_or_default()) {
                            // Nobody wants the rest: the exec goes, and its output with it.
                            kill(control);
                            break;
                        }
                    }
                });
            let reader = match reader {
                Ok(reader) => reader,
                // No thread to read it: the exec goes, as one whose output nobody wants.
                Err(e) => {
                    kill(control);
                    return Err(e);
                }
            };
            let ended = await_exit(&ours, begun);
            let _ = reader.join();
            Ok(ended)
        })
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
        let begun = Begun::now(wait);
        let ours = self.send_exec(id, spec, None, input, stdio)?;
        Ok(await_exit(&ours, begun))
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
        if let Err(e) = inbox
            .socket
            .send(&self.answers.run_send, shards_ipc::kind::EXEC_RUN, &payload, &fds)
        {
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
        if let Err(e) = socket.send(&self.answers.run_send, shards_ipc::kind::EXEC_RUN, &payload, &fds) {
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
#[derive(Clone, Copy)]
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
    /// under comes as its host lets it run (D188; PM M467). Its clock is taken before its
    /// exec is asked ([`Begun`]), so its answer, however soon, teaches its time from the ask,
    /// and its VM process is read only once its wait has lasted W* ([`read_at`]).
    WhileRuns(u32, &'a Record),
    /// A group member's word, judged as WhileRuns, with its refusal told immediately:
    /// the group must not invent an elapsed-time verdict while this wait still progresses,
    /// nor wait for a stalled guest to acknowledge its exec's kill before judging the round.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    GroupWord(u32, &'a Record, &'a (dyn Fn(&Cut) + Sync)),
}

/// A wait for an exec's end, its clock taken before its exec is asked (D188, amended by
/// ci-fixes and by own-35-ci-green): each answer teaches its kind its time from the ask
/// ([`teach_answer`]), and its microVM's process is read only once the wait has lasted W*
/// ([`read_at`], D215's rule). Its process read before the ask, as it was from 2026-11-16,
/// held every ask for that read: on Windows a ToolHelp snapshot of every process and the
/// system's records of every process and thread, each read of which took 13.1 to 13.8 ms on
/// jupiter (PM M606). Read after its ask with no clock before it, as before that, an answer
/// there first taught nothing, and on Windows arm64 its output reader's spawn outlasted a
/// stand-in's answer every time (CI 43968984258: no give-back learned in ~24k passes).
struct Begun<'a> {
    wait: Wait<'a>,
    /// When its exec was asked, near enough: taken before the ask.
    asked: Instant,
}

impl<'a> Begun<'a> {
    fn now(wait: Wait<'a>) -> Begun<'a> {
        Begun {
            wait,
            asked: Instant::now(),
        }
    }
}

/// What an answer to an ask made at `asked` teaches its kind, `record`: an exec's (D188,
/// amended 2026-11-17) and a judged exchange's ([`Judged`]; D215, amended 2026-11-17). Its
/// time from the ask, its slowest, however it was waited for; and, where its party's process
/// was followed from a look before the answer (`follow`), the work read from then, with its
/// rate (D215: only work read through an answer teaches R* and C*). An answer that came
/// before any read of its party ([`read_at`]) teaches its slowest alone, with no read.
pub(super) fn teach_answer(record: &Record, asked: Instant, follow: Option<&mut Follow<'_>>) {
    if let Some(follow) = follow {
        follow.answered();
    }
    record.teach_unrated(asked.elapsed());
}

/// When a judged wait reads its party's process ([`read_at`]).
pub(super) enum ReadAt {
    /// Not while its kind has no answer yet: its answer alone, until it comes or its party ends.
    Never,
    /// Not before this moment, its wait then W* long: its answer alone until then.
    Until(Instant),
    /// Now: its wait has lasted W*.
    Now,
}

/// When a wait asked at `asked`, whose kind's answers `record` keeps, first reads its party's
/// process: only once it has lasted W*, its kind's slowest answer timed from the ask, as
/// before then its judge could refuse nothing (D215, amended 2026-11-17, ROOT's rulings: one
/// rule for every judged wait, an exec's, a visit's hold and a judged exchange's). Until then
/// it waits on its answer alone, and an answer within W* teaches W* alone, with no read of
/// its party ([`teach_answer`]). The one change in behaviour is that a stalled party is
/// refused about one W* later; on Windows each read is a ToolHelp snapshot of every process
/// and the system's records of every process and thread (PM M606: 13.1 to 13.8 ms a read).
pub(super) fn read_at(record: &Record, asked: Instant) -> ReadAt {
    let shards_api::Seen::Learned { slowest, .. } = record.seen() else {
        return ReadAt::Never;
    };
    match asked.checked_add(slowest) {
        Some(due) if Instant::now() < due => ReadAt::Until(due),
        _ => ReadAt::Now,
    }
}

/// Waits on `ours` for an exec's end, ending it (SIGKILL) as its wait, `begun`, says, and
/// waiting on for its end then, so that none outlives its asker. A wait that follows its
/// microVM reads its process only once it has lasted W* ([`read_at`]); an answer read before
/// then teaches its time from the ask alone ([`teach_answer`]).
fn await_exit(ours: &shards_ipc::Stream, begun: Begun<'_>) -> Ended {
    let Begun { wait, asked } = begun;
    // Its microVM's process, followed once its wait has lasted W* (within: none where its
    // process had gone); none before.
    let mut follow: Option<Option<Follow<'_>>> = None;
    let mut cut: Option<Cut> = None;
    let status = 'wait: loop {
        let read_for = match (&cut, wait) {
            // Ended, or with no clock: until its end.
            (Some(_), _) | (None, Wait::Ending) => None,
            (None, Wait::For(timeout)) => Some(timeout),
            (None, Wait::WhileRuns(vm, record) | Wait::GroupWord(vm, record, _)) => 'read: {
                // Until its wait has lasted W*, its answer alone, waited for until then; its
                // kind with no answer yet, its answer alone throughout. Past W*, its process
                // followed from then, or the exec ended where its process cannot be read. A
                // read timeout is never zero, which sockets refuse: none left is past W*.
                if follow.is_none() {
                    match read_at(record, asked) {
                        ReadAt::Never => break 'read None,
                        ReadAt::Until(due) => {
                            let left = due.saturating_duration_since(Instant::now());
                            if !left.is_zero() {
                                break 'read Some(left);
                            }
                        }
                        ReadAt::Now => {}
                    }
                    match following(vm, record) {
                        Ok(followed) => follow = Some(followed),
                        Err(why) => {
                            if let Wait::GroupWord(_, _, refused) = wait {
                                refused(&why);
                            }
                            cut = Some(why);
                            kill(ours);
                            continue 'wait;
                        }
                    }
                }
                // Until the judge's next look, made first where it is due now: none while the
                // wait follows liveness alone (its process gone).
                let left = follow
                    .as_ref()
                    .and_then(Option::as_ref)
                    .and_then(Follow::due)
                    .map(|due| due.saturating_duration_since(Instant::now()));
                if left.is_some_and(|left| left.is_zero()) {
                    if let Some(Err(why)) = follow.as_mut().and_then(Option::as_mut).map(Follow::look) {
                        cut = Some(Cut::from(why));
                        if let (Wait::GroupWord(_, _, refused), Some(cut)) = (wait, &cut) {
                            refused(cut);
                        }
                        kill(ours);
                    }
                    continue 'wait;
                }
                left
            }
        };
        let _ = ours.set_read_timeout(read_for);
        match shards_ipc::recv(ours) {
            Ok(Some(m)) if m.kind == shards_ipc::kind::EXIT => {
                // Its own answer, not an end the daemon made: its kind learns from it.
                if cut.is_none()
                    && let Wait::WhileRuns(_, record) | Wait::GroupWord(_, record, _) = wait
                {
                    teach_answer(record, asked, follow.as_mut().and_then(Option::as_mut));
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
                    Wait::WhileRuns(..) | Wait::GroupWord(..) => {}
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

/// A wait on a socket whose party is process `pid`, judged as D215 judges a wait on a VM
/// process ([`Wait::WhileRuns`]), by the record of its kind of answer (runs-11 `TAKE_TIMEOUT`):
/// shards_ipc's judge ([`Judged::judge`]) for its judged sends and receives. Before its kind's
/// first answer, and once its party has gone (its socket's end then ends the wait), the
/// socket's readiness alone; then its party looked at once each W* and the wait ended, typed
/// ([`Stalled`]), where it ran nothing since the look before or could not be read.
///
/// Its clock is taken as it is made, before its ask, and its party's process is read only
/// once its wait has lasted W*, its kind's slowest answer timed from the ask: before then its
/// judge could refuse nothing. It is asked only once a look finds its socket not ready
/// ([`shards_ipc::Judge`]), and an answer within W* teaches its kind its time from the ask
/// alone, with no read of its party ([`teach_answer`]; D215, amended 2026-11-17, ROOT's
/// rulings). Read as its wait began, as it was, before its RUN was even sent, its process held
/// every run's handoff for that read, and the answer read it again: on Windows a ToolHelp
/// snapshot of every process with each, and the system's records of every process and thread,
/// each read of which took 13.1 to 13.8 ms on jupiter (PM M606).
pub(super) struct Judged<'r> {
    pid: u32,
    record: &'r Record,
    /// When its exchange began, near enough: taken as it was made, before its ask.
    asked: Instant,
    /// Its party's process, followed from its first ask past W* (within: none where its
    /// process had gone); none before.
    follow: Option<Option<Follow<'r>>>,
}

impl<'r> Judged<'r> {
    pub(super) fn of(pid: u32, record: &'r Record) -> Judged<'r> {
        Judged {
            pid,
            record,
            asked: Instant::now(),
            follow: None,
        }
    }

    /// When its wait next asks, or why it ends: asked once a look finds its socket not ready,
    /// and at each time it named that passed with the socket not ready still. Until its wait
    /// has lasted W*, its socket's readiness alone, asked again at W*; a kind with no answer
    /// yet, its readiness alone throughout. Past W*, its party's process followed from then.
    pub(super) fn judge(&mut self) -> std::io::Result<Option<Instant>> {
        if self.follow.is_none() {
            match read_at(self.record, self.asked) {
                ReadAt::Never => return Ok(None),
                ReadAt::Until(due) => return Ok(Some(due)),
                ReadAt::Now => {}
            }
            let followed = following(self.pid, self.record).map_err(Stalled::error)?;
            let due = followed.as_ref().and_then(Follow::due);
            self.follow = Some(followed);
            return Ok(due);
        }
        let Some(Some(follow)) = self.follow.as_mut() else {
            return Ok(None);
        };
        follow.look().map_err(|why| Stalled::error(Cut::from(why)))?;
        Ok(follow.due())
    }

    /// Its party answered: its kind learns from the answer, its time from the ask, and the
    /// work its follow read, where one began ([`teach_answer`]).
    pub(super) fn answered(&mut self) {
        teach_answer(
            self.record,
            self.asked,
            self.follow.as_mut().and_then(Option::as_mut),
        );
    }
}

/// A judged wait's end ([`Judged`]): its party stalled, or could not be read, as [`Cut`] says.
#[derive(Debug)]
pub(super) struct Stalled(pub(super) Cut);

impl Stalled {
    fn error(cut: Cut) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::TimedOut, Stalled(cut))
    }
}

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Cut::Stalled(stall) => write!(
                f,
                "its process ran no CPU in {:?} past the slowest answer seen ({:?}), {:?} into the wait{}",
                stall.over(),
                stall.slowest(),
                stall.waited(),
                stall.account()
            ),
            other => write!(f, "{other}"),
        }
    }
}

impl std::error::Error for Stalled {}

/// A wait on VM process `vm` judged by `record`, begun now: none where the process has gone
/// already, whose end is the exec's; why not, where it cannot be read.
fn following(vm: u32, record: &Record) -> Result<Option<Follow<'_>>, Cut> {
    match Followed::of_pid(vm).map_err(|failure| Cut::Unread { pid: vm, failure })? {
        Some(followed) => Follow::begin(record, followed, shards_api::Bound::Rate).map_err(Cut::from),
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
        record.teach(slowest, shards_api::Work::of_cpu(slowest / 1000));
        record
    }

    /// Waits for `wait` in an exec whose VM side `theirs` is answered by `answer`, on a
    /// thread of its own that is asked, as the daemon asks its exec, once the wait has begun.
    fn waited(wait: Wait<'_>, answer: impl FnOnce(shards_ipc::Stream) + Send) -> Ended {
        let (ours, theirs) = shards_ipc::Stream::pair().unwrap();
        let (go, gone) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            crate::daemon::party::spawn_in(s, move || {
                if gone.recv().is_ok() {
                    answer(theirs);
                }
            });
            let begun = Begun::now(wait);
            go.send(()).unwrap();
            await_exit(&ours, begun)
        })
    }

    /// The exec's answer, `status`, said `after` it was asked.
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
    #[test]
    fn a_group_word_reports_its_refusal_before_the_kill_is_acknowledged() {
        let idle = sleeper();
        let record = learned(Duration::from_millis(50));
        let refused = std::sync::atomic::AtomicBool::new(false);
        let report = |cut: &Cut| {
            assert!(matches!(cut, Cut::Stalled(_)));
            refused.store(true, Ordering::SeqCst);
        };
        let ended = waited(Wait::GroupWord(idle.0.id(), &record, &report), |theirs| {
            let kill = shards_ipc::recv(&theirs).unwrap().unwrap();
            assert_eq!(kill.kind, shards_ipc::kind::SIGNAL);
            assert!(
                refused.load(Ordering::SeqCst),
                "the group still waits for its own verdict"
            );
            shards_ipc::send(&theirs, shards_ipc::kind::EXIT, &[137], &[]).unwrap();
        });
        assert!(matches!(ended.cut, Some(Cut::Stalled(_))));
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
    /// waited for until it answers: a cold wait waits on its answer alone, its microVM's
    /// liveness its connection's end, and its answer teaches its kind (D215's judge; red with
    /// PM M357's `IDLE_VM_CPU`, which ended it at its first 10 s, here a fifth of a second).
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
        assert!(slowest >= after, "{slowest:?}, its time not from its ask");
    }

    /// An answer there before its waiter reads teaches its kind its time from the ask, whose
    /// clock is taken before it (D188, amended by ci-fixes and by own-35-ci-green), and its
    /// slowest alone: its microVM's process is read only once its wait has lasted W*
    /// ([`read_at`]), and its kind, cold, has no W* yet. Read before the ask, it held every ask
    /// for the read, on Windows 13.1 to 13.8 ms of the system's records (PM M606), and taught
    /// this answer the CPU this busy test process ran meanwhile. Waited for with no clock before
    /// the ask, such an answer taught nothing, and a microVM that answered faster than its
    /// waiter's reader was spawned never taught its kind (CI 43968984258, Windows arm64). Its
    /// microVM's process is this one, which lives throughout.
    #[test]
    fn an_answer_there_before_its_waiter_reads_teaches_from_its_ask() {
        let record = Record::new();
        let (ours, theirs) = shards_ipc::Stream::pair().unwrap();
        let begun = Begun::now(Wait::WhileRuns(std::process::id(), &record));
        let began = Instant::now();
        // The ask's way to its microVM, which answers as it reads it.
        std::thread::sleep(Duration::from_millis(1));
        shards_ipc::send(&theirs, shards_ipc::kind::EXIT, &[7], &[]).unwrap();
        let answered = began.elapsed();
        let ended = await_exit(&ours, begun);
        assert_eq!((ended.status, ended.cut), (Some(7), None));
        let shards_api::Seen::Learned { slowest, rated } = record.seen() else {
            panic!("an answer within its wait taught nothing");
        };
        assert!(
            slowest >= answered,
            "{slowest:?} from its ask, its answer {answered:?} past it"
        );
        assert_eq!(rated, None, "its process read before its waiter first looked");
    }

    /// A take there at its waiter's first look teaches its kind its time from the ask, whose
    /// clock its judge takes as it is made, and its slowest alone: shards_ipc's judged waits
    /// look before they ask their judge, which reads its party's process only at its first ask,
    /// and none came (D215, amended 2026-11-17). Asked as the wait began, the judge read the
    /// process before every run's TAKEN, on Windows a ToolHelp snapshot of every process and
    /// 13.1 to 13.8 ms of the system's records (PM M606), and taught such a take the CPU this
    /// busy test process ran meanwhile, timed from that read. Its party is this process, which
    /// lives throughout.
    #[test]
    fn a_take_there_at_its_waiters_first_look_teaches_from_its_ask() {
        let record = Record::new();
        let (ours, theirs) = shards_ipc::Stream::pair().unwrap();
        let mut judged = Judged::of(std::process::id(), &record);
        let began = Instant::now();
        // The run's way to its VM, which says it took it as it reads it.
        std::thread::sleep(Duration::from_millis(1));
        shards_ipc::send(&theirs, shards_ipc::kind::TAKEN, &[], &[]).unwrap();
        let answered = began.elapsed();
        let taken = shards_ipc::recv_judged(&ours, &mut || judged.judge())
            .unwrap()
            .unwrap();
        assert_eq!(taken.kind, shards_ipc::kind::TAKEN);
        judged.answered();
        let shards_api::Seen::Learned { slowest, rated } = record.seen() else {
            panic!("a take there at its waiter's first look taught nothing");
        };
        assert!(
            slowest >= answered,
            "{slowest:?} from its ask, its answer {answered:?} past it"
        );
        assert_eq!(rated, None, "its process read before its waiter first looked");
    }

    /// An answer within its kind's W* comes with no read of its party's process: its judge,
    /// asked as a look finds its socket not ready, reads the process only once its wait has
    /// lasted W* (D215, amended 2026-11-17, ROOT's ruling), as before then it could refuse
    /// nothing; until then its wait is the socket's readiness, asked again at W*. The answer
    /// teaches W* alone. Past W*, its next ask follows the process. Before, its first ask read
    /// the process, on Windows a ToolHelp snapshot of every process and 13.1 to 13.8 ms of the
    /// system's records (PM M606), and the answer read it again. Its party is this process,
    /// which lives throughout.
    #[test]
    fn an_answer_within_w_star_reads_no_process() {
        let record = Record::new();
        let slowest = Duration::from_secs(10);
        record.teach_unrated(slowest);
        let mut judged = Judged::of(std::process::id(), &record);
        let due = judged.judge().unwrap();
        assert!(judged.follow.is_none(), "its process read within W*");
        assert_eq!(due, Some(judged.asked + slowest), "not asked again at W*");
        judged.answered();
        assert_eq!(
            record.seen(),
            shards_api::Seen::Learned { slowest, rated: None },
            "an answer within W* taught a rate"
        );
        let record = Record::new();
        let slowest = Duration::from_millis(1);
        record.teach_unrated(slowest);
        let mut judged = Judged::of(std::process::id(), &record);
        std::thread::sleep(slowest * 2);
        judged.judge().unwrap();
        assert!(
            matches!(judged.follow, Some(Some(_))),
            "past W*, its process not followed"
        );
    }

    /// An exec answered within its kind's W* comes with no read of its microVM's process
    /// (D188, amended 2026-11-17: D215's rule, [`read_at`]): its wait waits on its answer
    /// alone until W*, and reads only past it, so the answer teaches W* alone. Before, its
    /// first look that found nothing read the process, and the answer read it again and taught
    /// the CPU it ran meanwhile: here this test process's, a thread of it busy until it
    /// answers.
    #[test]
    fn an_exec_answered_within_w_star_reads_no_process() {
        let record = Record::new();
        let slowest = Duration::from_secs(10);
        record.teach_unrated(slowest);
        let ended = waited(Wait::WhileRuns(std::process::id(), &record), |theirs| {
            let busy = Instant::now();
            while busy.elapsed() < Duration::from_millis(50) {
                std::hint::spin_loop();
            }
            shards_ipc::send(&theirs, shards_ipc::kind::EXIT, &[7], &[]).unwrap();
        });
        assert_eq!((ended.status, ended.cut), (Some(7), None));
        assert_eq!(
            record.seen(),
            shards_api::Seen::Learned { slowest, rated: None },
            "its process read within W*"
        );
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
        let began = Instant::now();
        let ended = waited(
            Wait::WhileRuns(trickler.0.id(), &record),
            answers(7, slowest * 7 / 2),
        );
        let wall = began.elapsed();
        assert_eq!((ended.status, ended.cut), (Some(7), None));
        let shards_api::Seen::Learned { slowest: now, .. } = record.seen() else {
            panic!("its record was lost");
        };
        // What the wait learned is its own answer's time from its begin, before the answer
        // was asked: the answer's own 3.5 s at least, within the wait's whole.
        assert!(
            now >= slowest * 7 / 2 && now <= wall,
            "its answer, slower, raised W*: {now:?} of {wall:?}"
        );
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
            crate::daemon::party::spawn_in(s, || {
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
        assert_eq!(stall.least().map(|least| least.over), Some(slowest));
        assert_eq!(
            record.seen(),
            learned(slowest).seen(),
            "an end it made taught nothing"
        );
    }

    /// b26 (ROOT's HVF batch at 1f71dea19, moved.rs:645, containers.rs:2257): a kind whose W*
    /// was microseconds refused a live VM process 16.6 µs past its first look, a window in
    /// which its reading (macOS: a clock tick's steps) shows nothing of a run. A wait is
    /// judged over no window shorter than its VM process's reading shows, its kernel's tick
    /// (`Watch::resolution`), whatever W* says: a process that runs nothing is refused only
    /// once a whole tick has shown it still. Red with the reading's unit alone (42 ns on
    /// macOS, 1 ns on Linux) as its resolution: refused microseconds in.
    #[cfg(unix)]
    #[test]
    fn a_wait_is_never_refused_within_what_its_reading_resolves() {
        let idle = sleeper();
        let tick = tick();
        let slowest = Duration::from_micros(10);
        let record = learned(slowest);
        let ended = waited(Wait::WhileRuns(idle.0.id(), &record), killed);
        let Some(Cut::Stalled(stall)) = &ended.cut else {
            panic!("{:?}", ended.cut);
        };
        assert!(
            stall.over() >= tick,
            "{stall:?} within its kernel's tick {tick:?}"
        );
        assert_eq!(stall.slowest(), slowest);
        let resolution = shards_api::Followed::of_pid(idle.0.id())
            .unwrap()
            .and_then(|followed| shards_api::Watch::of(followed, &shards_api::Listing::new()).unwrap())
            .unwrap()
            .resolution();
        assert!(
            resolution >= tick,
            "{resolution:?} below its kernel's tick {tick:?}"
        );
    }

    /// The kernel's tick, read here apart from shards-api: macOS's `kern.clockrate` `tick`.
    #[cfg(target_os = "macos")]
    fn tick() -> Duration {
        let mut info = [0 as libc::c_int; 5];
        let mut len = size_of_val(&info);
        // SAFETY: sysctlbyname(3) into a local struct clockinfo's five ints.
        let code = unsafe {
            libc::sysctlbyname(
                c"kern.clockrate".as_ptr(),
                info.as_mut_ptr().cast(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        assert_eq!((code, len), (0, size_of_val(&info)));
        Duration::from_micros(u64::try_from(info[1]).unwrap())
    }

    /// The kernel's tick, read here apart from shards-api: Linux's CLOCK_MONOTONIC_COARSE
    /// resolution, one jiffy.
    #[cfg(target_os = "linux")]
    fn tick() -> Duration {
        // SAFETY: a timespec, plain data, for clock_getres(2) to fill.
        let mut at: libc::timespec = unsafe { std::mem::zeroed() };
        // SAFETY: clock_getres(2) into a local timespec.
        let code = unsafe { libc::clock_getres(libc::CLOCK_MONOTONIC_COARSE, &raw mut at) };
        assert_eq!(code, 0);
        Duration::new(
            u64::try_from(at.tv_sec).unwrap(),
            u32::try_from(at.tv_nsec).unwrap(),
        )
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

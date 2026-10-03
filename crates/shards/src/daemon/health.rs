//! A container's health, as dockerd keeps it (moby daemon/health.go, docker-v29.3.1):
//! probes a configured interval apart, and the start interval within the start period
//! while the container is starting; each probe's result recorded, the last five kept;
//! failures counted in a streak that makes it unhealthy at the retries, except within the
//! start period while it has never been healthy. Like dockerd's, it lives in the daemon's
//! memory, not on disk: a probe a few seconds apart writes nothing.

use std::collections::VecDeque;
use std::time::Duration;

use std::io::Read as _;
use std::os::fd::AsFd as _;

use shards_ipc::Health as Config;

use super::{RunState, lock};

/// What dockerd uses where a check leaves a setting at 0.
const INTERVAL: Duration = Duration::from_secs(30);
const TIMEOUT: Duration = Duration::from_secs(30);
const START_INTERVAL: Duration = Duration::from_secs(5);
const RETRIES: u64 = 3;
/// The bytes of a probe's output kept, and the results kept.
pub(super) const MAX_OUTPUT: usize = 4096;
const MAX_LOG: usize = 5;

/// How a container's health checks have gone.
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

/// How long to wait for the next probe, `since_start` after the container started
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
    /// Records `probe`, which began `since_start` after the container started
    /// (handleProbeResult): a success makes it healthy and ends the streak; a failure,
    /// any code but 0, adds to the streak, unless the container is still starting within
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

/// A probe's output as dockerd keeps it: its first [`MAX_OUTPUT`] bytes, then `...` if it
/// wrote more (limitedBuffer), as text.
pub(super) fn kept_output(bytes: &[u8], more: bool) -> String {
    let mut out = String::from_utf8_lossy(bytes).into_owned();
    if more {
        out.push_str("...");
    }
    out
}

impl<D: crate::containers::Disk> super::Daemon<D> {
    /// Checks container `id`'s health while it runs, as dockerd's monitor does: one probe
    /// at a time, the next an interval after the last ended. Ends with the run; nothing
    /// happens for a container without a check.
    pub(super) fn monitor_health(&self, id: &str) {
        let check = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(run)) => run
                .base
                .health
                .as_ref()
                .and_then(|(cfg, shell)| Some((cfg.clone(), command(cfg, shell)?))),
            _ => None,
        };
        let Some((cfg, argv)) = check else {
            return;
        };
        lock(&self.health).insert(id.to_string(), State::default());
        let since_start = || {
            let started = lock(&self.containers).get(id).and_then(|c| c.started);
            let now = crate::spec::now();
            let ns = started.map_or(0, |s| now.saturating_sub(s));
            Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX))
        };
        loop {
            let status = lock(&self.health).get(id).map_or(Status::Starting, |s| s.status);
            if !self.runs_for(id, interval(&cfg, since_start(), status)) {
                break;
            }
            let since = since_start();
            let probe = self.probe(id, &argv, &cfg);
            // A result after the run ended counts for nothing, as dockerd drops one.
            if !matches!(lock(&self.runs).get(id), Some(RunState::Tracked(_))) {
                break;
            }
            if let Some(state) = lock(&self.health).get_mut(id) {
                state.record(&cfg, probe, since);
            }
        }
        lock(&self.health).remove(id);
    }

    /// Waits `wait`, or until run `id` ends: whether it still runs.
    fn runs_for(&self, id: &str, wait: Duration) -> bool {
        let deadline = std::time::Instant::now() + wait;
        let mut runs = lock(&self.runs);
        loop {
            if !matches!(runs.get(id), Some(RunState::Tracked(_))) {
                return false;
            }
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return true;
            };
            runs = self
                .resolved
                .wait_timeout(runs, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// One probe of container `id`: `argv` run beside its workload as an exec without
    /// stdin or a terminal, as the container's user in its working directory with its
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
        let pieces = (|| -> std::io::Result<_> {
            let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
            let (out, into) = std::io::pipe()?;
            let null = std::fs::File::open("/dev/null")?;
            let held = theirs.try_clone()?;
            Ok((ours, theirs, out, into, null, held))
        })();
        let (ours, theirs, mut out, into, null, held) = match pieces {
            Ok(p) => p,
            Err(e) => return failed(format!("starting the health check: {e}")),
        };
        {
            // Taken out of `runs` before the inbox is locked (commands.rs, exec).
            let (base, socket, inbox) = match lock(&self.runs).get(id) {
                Some(RunState::Tracked(run)) => (run.base.clone(), run.socket.clone(), run.inbox.clone()),
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
            };
            let spec = match crate::spec::spec(&options, |_| None) {
                Ok(spec) => spec,
                Err(e) => return failed(e),
            };
            let number = self.next_exec.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut payload = Vec::with_capacity(9 + spec.encoded_len().unwrap_or(0));
            payload.extend_from_slice(&number.to_be_bytes());
            payload.push(0);
            spec.encode_into(&mut payload);
            lock(&inbox).execs_in_flight.push((number, held));
            let fds = [theirs.as_fd(), null.as_fd(), into.as_fd(), into.as_fd()];
            if let Err(e) = socket.send(shards_ipc::kind::EXEC_RUN, &payload, &fds) {
                lock(&inbox).execs_in_flight.retain(|(n, _)| *n != number);
                return failed(format!("starting the health check: {e}"));
            }
        }
        // The VM holds the rest: its end sees the output end.
        drop((theirs, into, null));
        let timeout = timeout(cfg);
        std::thread::scope(|scope| {
            let output = scope.spawn(move || {
                let mut kept = Vec::with_capacity(MAX_OUTPUT);
                let mut buf = [0u8; 4096];
                let mut more = false;
                while let Ok(n @ 1..) = out.read(&mut buf) {
                    let room = MAX_OUTPUT.saturating_sub(kept.len());
                    let chunk = buf.get(..n).unwrap_or_default();
                    kept.extend_from_slice(chunk.get(..room.min(n)).unwrap_or_default());
                    more |= n > room;
                }
                (kept, more)
            });
            let _ = ours.set_read_timeout(Some(timeout));
            let mut timed_out = false;
            let status = loop {
                match shards_ipc::recv(&ours) {
                    Ok(Some(m)) if m.kind == shards_ipc::kind::EXIT => {
                        break m.payload.first().map(|&s| i64::from(s));
                    }
                    Ok(Some(_)) => {}
                    Err(e)
                        if !timed_out
                            && matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                    {
                        // Past its timeout: killed, then waited for, so that no probe
                        // outlives its check.
                        timed_out = true;
                        let _ = shards_ipc::send(&ours, shards_ipc::kind::SIGNAL, &9u32.to_be_bytes(), &[]);
                        let _ = ours.set_read_timeout(None);
                    }
                    Ok(None) | Err(_) => break None,
                }
            };
            let (kept, more) = output.join().unwrap_or_default();
            let said = kept_output(&kept, more);
            match (timed_out, status) {
                (true, _) => {
                    let limit = shards_cmdline::gotime::format_duration(
                        i64::try_from(timeout.as_nanos()).unwrap_or(i64::MAX),
                    );
                    failed(if said.is_empty() {
                        format!("Health check exceeded timeout ({limit})")
                    } else {
                        format!("Health check exceeded timeout ({limit}): {said}")
                    })
                }
                (false, Some(code)) => Probe {
                    start_ns,
                    end_ns: crate::spec::now(),
                    exit_code: code,
                    output: said,
                },
                (false, None) => failed("the container's microVM ended the check".into()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn results_move_the_status_as_dockerd_moves_it() {
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

    /// Failures within the start period count only once the container has been healthy.
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
    fn probes_come_as_often_as_dockerd_sends_them() {
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

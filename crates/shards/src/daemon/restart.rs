//! Restart policies (`--restart`), as dockerd's restart manager keeps them (moby
//! docker-v29.8.1 daemon/internal/restartmanager; daemon/monitor.go handleContainerExit;
//! kill.go and container.go ExitOnNext; start.go ResetRestartManager; daemon.go restore):
//! a container whose command ends is started again where its policy says, after a wait
//! that doubles from 100 ms to a minute, and starts over once a run lasted 10 s; a stop
//! or kill by its stop signal or SIGKILL cancels the next; a stop or kill marks it stopped
//! by hand, which `unless-stopped` heeds; `shards start` starts its count over. The wait
//! is a deadline of the followers' loop, and the start goes through the daemon's own
//! door, as a client's detached `shards start` would.

use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use shards_ipc::{Run, kind};

use super::{Daemon, Threads, lock, log};
use crate::containers::{Container, Disk, State as Life};

/// restartmanager's first wait, the longest, and how long a run lasts for the wait to
/// start over.
const FIRST_WAIT: Duration = Duration::from_millis(100);
const LONGEST_WAIT: Duration = Duration::from_secs(60);
const RAN_LONG_NS: u128 = 10_000_000_000;

/// What a container's restart manager keeps between its runs: its last wait, and whether
/// its next restart is cancelled.
#[derive(Debug, Default)]
pub(super) struct Manager {
    wait: Duration,
    cancelled: bool,
}

/// ShouldRestart's decision, and its wait: the wait is worked out (and kept) whatever is
/// decided, as restartmanager does.
fn decide(
    manager: &mut Manager,
    c: &Container,
    exit: u8,
    ran_ns: Option<u128>,
    stopping: bool,
) -> Option<Duration> {
    let r = &c.restart;
    if matches!(r.policy.as_str(), "" | "no") || manager.cancelled {
        return None;
    }
    if ran_ns.is_some_and(|ns| ns >= RAN_LONG_NS) {
        manager.wait = Duration::ZERO;
    }
    manager.wait = if manager.wait.is_zero() {
        FIRST_WAIT
    } else if manager.wait < LONGEST_WAIT {
        manager.wait.saturating_mul(2)
    } else {
        manager.wait
    }
    .min(LONGEST_WAIT);
    // A daemon stopping stops its containers by hand (monitor.go).
    let by_hand = stopping || r.manually_stopped;
    let restart = match r.policy.as_str() {
        "always" => true,
        "unless-stopped" => !by_hand,
        "on-failure" => {
            let below = r.max == 0 || i64::try_from(r.count).is_ok_and(|n| n < r.max);
            below && exit != 0
        }
        _ => false,
    };
    restart.then_some(manager.wait)
}

impl<D: Disk> Daemon<D> {
    /// Whether container `id`, whose run ended with `exit` after `ran_ns`, starts again by
    /// its policy, and after how long. Its count goes up as it is recorded restarting.
    pub(super) fn should_restart(&self, id: &str, exit: u8, ran_ns: Option<u128>) -> Option<Duration> {
        let c = lock(&self.containers).made(id).cloned()?;
        let stopping = self.stopping.load(Ordering::SeqCst);
        let mut managers = lock(&self.restarts);
        let manager = managers.entry(id.to_string()).or_default();
        decide(manager, &c, exit, ran_ns, stopping)
    }

    /// Starts container `id` again after `wait`, on the followers' loop.
    pub(super) fn schedule_restart(&self, id: &str, wait: Duration) {
        let f = &self.followers;
        let token = f.next_token();
        lock(&f.restarts).insert(token, id.to_string());
        lock(&f.deadlines).push(std::cmp::Reverse((Instant::now() + wait, token)));
        f.wake();
    }

    /// Container `id`'s wait to restart is up: started again, as a detached `shards start`
    /// starts it, unless a stop, kill, removal or the daemon's own stop came first; then
    /// it is stopped, as dockerd sets it once its restart is cancelled.
    pub(super) fn restart_due<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, id: &str) {
        let cancelled = lock(&self.restarts).get(id).is_some_and(|m| m.cancelled);
        let waiting = lock(&self.containers)
            .get(id)
            .is_some_and(|c| c.restart.restarting && c.state == Life::Exited);
        if !waiting {
            return;
        }
        if cancelled || self.stopping.load(Ordering::SeqCst) {
            self.set_restarting(id, false);
            return;
        }
        if let Err(e) = self.start_again(threads, id) {
            log(format!("container {id}: restarting it: {e}"));
            self.set_restarting(id, false);
        }
    }

    /// Container `id` restarting or not, as its record says.
    fn set_restarting(&self, id: &str, restarting: bool) {
        let changed = lock(&self.containers).change(id, |c| c.restart.restarting = restarting);
        match changed {
            Ok(()) => self.record_soon(id, Vec::new()),
            Err(e) => log(format!("container {id}: {e}")),
        }
    }

    /// Sends the daemon its own detached `start` of container `id`: the run is handled
    /// and followed as any client's, its output only to its log.
    fn start_again<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, id: &str) -> Result<(), String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a connection: {e}"))?;
        let null = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .map_err(|e| format!("/dev/null: {e}"))
        };
        let (stdin, stdout, stderr) = (null()?, null()?, null()?);
        let run = Run {
            again: Some(id.to_string()),
            detach: true,
            daemon: self.identity,
            restart_policy: ("no".into(), 0),
            ..Run::default()
        };
        lock(&self.restarting_now).insert(id.to_string());
        let fds = [stdin.as_fd(), stdout.as_fd(), stderr.as_fd()];
        if let Err(e) = shards_ipc::send(&ours, kind::START, &run.encode(), &fds) {
            lock(&self.restarting_now).remove(id);
            return Err(format!("its start: {e}"));
        }
        // What the daemon answers this client is read and let go, to its end.
        let drained = std::thread::Builder::new()
            .name("restart".into())
            .spawn_scoped(
                threads,
                move || while let Ok(Some(_)) = shards_ipc::recv(&ours) {},
            );
        if let Err(e) = drained {
            lock(&self.restarting_now).remove(id);
            return Err(format!("its client's thread: {e}"));
        }
        self.take(threads, theirs);
        Ok(())
    }

    /// A stop or kill of container `id` by `linux`, as kill.go has it: stopped by hand
    /// unless the daemon is stopping, and its next restart cancelled where the signal is
    /// its stop signal or SIGKILL (ExitOnNext). One waiting to restart is stopped now.
    pub(super) fn exit_on_next(&self, id: &str, linux: u32) {
        let (own, _) = self.own_stop(id);
        if linux == 9 || Some(linux) == own {
            lock(&self.restarts).entry(id.to_string()).or_default().cancelled = true;
        }
        let stopping = self.stopping.load(Ordering::SeqCst);
        let changed = lock(&self.containers).change(id, |c| {
            if !stopping {
                c.restart.manually_stopped = true;
            }
            c.restart.restarting = false;
        });
        if changed.is_ok() {
            self.record_soon(id, Vec::new());
        }
    }

    /// Container `id` waits to restart.
    pub(super) fn is_restarting(&self, id: &str) -> bool {
        lock(&self.containers)
            .made(id)
            .is_some_and(|c| c.state == Life::Exited && c.restart.restarting)
    }

    /// Whether container `id`'s run that started was the daemon's restart, which keeps
    /// its count; one `shards start` or `run` asked for starts its manager over
    /// (ResetRestartManager(true)), so that it waits 100 ms first again.
    pub(super) fn restarted_by_policy(&self, id: &str) -> bool {
        let ours = lock(&self.restarting_now).remove(id);
        if !ours {
            lock(&self.restarts).remove(id);
        }
        ours
    }

    /// As the daemon starts, the containers its last stop ended that their policies start
    /// again (daemon.go restore, ShouldRestart): those started before, not stopped by
    /// hand where that counts, at once.
    pub(super) fn restart_at_start<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let due: Vec<String> = lock(&self.containers)
            .all()
            .filter(|c| c.started.is_some() && c.state != Life::Running)
            .filter(|c| {
                let mut fresh = Manager::default();
                decide(&mut fresh, c, c.exit_code.unwrap_or(0), None, false).is_some()
            })
            .map(|c| c.id.clone())
            .collect();
        if due.is_empty() {
            return;
        }
        for id in due {
            self.set_restarting(&id, true);
            self.schedule_restart(&id, Duration::ZERO);
        }
        self.start_followers(threads);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(policy: &str, max: i64, count: u64, by_hand: bool) -> Container {
        let mut c: Container = serde_json::from_value(serde_json::json!({
            "id": "c", "name": "n", "image": "i", "command": [], "created": 1, "state": "exited",
            "started": 1, "finished": 2, "exit_code": 1, "auto_remove": false,
        }))
        .unwrap();
        c.restart = crate::containers::Restart {
            policy: policy.into(),
            max,
            count,
            manually_stopped: by_hand,
            restarting: false,
        };
        c
    }

    /// restartmanager_test.go's cases: the policies, the doubling wait, its start over
    /// after a long run, and the retry limit.
    #[test]
    fn restarts_are_decided_as_dockerds_restart_manager_decides() {
        let mut m = Manager::default();
        let always = container("always", 0, 0, false);
        let waits: Vec<_> = (0..12)
            .map(|_| decide(&mut m, &always, 0, Some(0), false))
            .collect();
        assert_eq!(waits[0], Some(Duration::from_millis(100)));
        assert_eq!(waits[1], Some(Duration::from_millis(200)));
        assert_eq!(waits[9], Some(Duration::from_millis(51_200)));
        assert_eq!(
            waits[10],
            Some(LONGEST_WAIT),
            "doubled past a minute, it stays there"
        );
        assert_eq!(waits[11], Some(LONGEST_WAIT));
        assert_eq!(
            decide(&mut m, &always, 0, Some(RAN_LONG_NS), false),
            Some(FIRST_WAIT)
        );
        let mut m = Manager::default();
        assert_eq!(
            decide(&mut m, &container("no", 0, 0, false), 1, None, false),
            None
        );
        assert_eq!(
            decide(&mut m, &container("unless-stopped", 0, 0, true), 1, None, false),
            None
        );
        assert!(decide(&mut m, &container("unless-stopped", 0, 0, false), 0, None, false).is_some());
        assert_eq!(
            decide(&mut m, &container("unless-stopped", 0, 0, false), 0, None, true),
            None
        );
        assert!(decide(&mut m, &container("always", 0, 0, true), 0, None, true).is_some());
        assert_eq!(
            decide(&mut m, &container("on-failure", 0, 0, false), 0, None, false),
            None
        );
        assert!(decide(&mut m, &container("on-failure", 3, 2, false), 1, None, false).is_some());
        assert_eq!(
            decide(&mut m, &container("on-failure", 3, 3, false), 1, None, false),
            None
        );
        m.cancelled = true;
        assert_eq!(decide(&mut m, &always, 0, None, false), None);
    }
}

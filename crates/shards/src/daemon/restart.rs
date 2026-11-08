//! Restart policies (`--restart`): a microVM whose command ends is started again where its
//! policy says, after a wait (D219). What each policy restarts, and what stops and starts
//! do to it, are dockerd's restart manager's (moby docker-v29.8.1
//! daemon/internal/restartmanager; daemon/monitor.go handleContainerExit; kill.go and
//! container.go ExitOnNext; start.go ResetRestartManager; daemon.go restore), as scripts
//! read them: `always`, `unless-stopped` unless stopped by hand, `on-failure[:N]` on a
//! failure below its count; a stop or kill by its stop signal or SIGKILL cancels the next;
//! `shards start vm` starts its count and its waits over; `RestartCount` goes up and `ps` and
//! `inspect` say `restarting` while it waits, and each wait is twice the last, as
//! dockerd's are.
//!
//! How long the waits are is this host's, learned as it runs microVMs, where dockerd's
//! are figures (100 ms first, a minute the longest, from scratch after a run of 10 s):
//!
//! - **A restart's cost** is the host CPU time its VM process spent: the restore that made
//!   it ready, and its run (`Costs::ended`), read from the process as its run ends
//!   (`process_usage`: /proc, proc_pidinfo, GetProcessTimes). The ended run's own is the
//!   cost of its next; where it cannot be read, the host's last, smoothed as refill times
//!   are (RFC 6298 §2, α 1/8), and where none has been read, the run's own time.
//! - **An idle microVM's cost** is the CPU a warm VM spends a second, from ready to
//!   claimed, over the host's warm VMs (`Costs::claimed`): the slope of their CPU over
//!   their time warm, fitted by least squares, so that what a VM still does as it becomes
//!   ready, which a short wait would count many times over, falls in the fit's intercept
//!   (PM M484: the CPU a second over all of them read 23.8 ms where a microVM sleeping
//!   costs 6.4); their CPU over their time where no slope can be fitted yet.
//! - **The first wait** is the restart's cost at the share of a CPU the run's limits give
//!   it, so that a crash loop's restarts take no more of its host than its own limits let
//!   its command take; each next is twice the last (binary exponential backoff, Metcalfe
//!   and Boggs, CACM 19(7), 1976), so that a loop's restarts from its first failure to any
//!   time `t` number at most log2(t / first) + 1, and a restart after the failure's cause
//!   is gone comes within as long again as the loop had failed.
//! - **The longest** is the restart's cost at the idle microVM's rate: past it, a microVM
//!   failing for good costs its host what one idling would. A run that lasts as long paid
//!   for its restart as an idle microVM would, and its waits start over.
//! - **Cold start.** What the host learned is kept in the home (`restart-costs`), for the
//!   next daemon. Before any warm VM of this home's has idled and been claimed, no wait is
//!   capped: they double on.
//!
//! PM M484 measured what this learns against each host's whole process tree, on Linux by
//! its cgroup too.
//!
//! The wait is a deadline of the followers' loop, and the start goes through the daemon's
//! own door, as a client's detached `shards start vm` would.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use shards_ipc::Stream as UnixStream;
use shards_ipc::fd::AsFd as _;
use shards_ipc::{Run, kind};

use super::{Daemon, Threads, lock, log};
use crate::containers::{Container, Disk, State as Life};

/// The home's file of what restarts cost on this host: a restart's CPU in ns, smoothed,
/// then the warm VMs' [`Fit`]: their count, and the sums of their times warm, their CPU,
/// their times squared and their times by their CPU, in seconds; a line, apart by spaces.
const COSTS: &str = "restart-costs";

/// The host CPU time process `pid` has had, in ns.
fn cpu_of(pid: u32) -> Option<u64> {
    shards_vmm::platform::process_usage(pid).ok().map(|u| u.cpu_ns)
}

/// What restarts cost on this host, as its microVMs show it.
#[derive(Debug, Default)]
pub(super) struct Costs {
    /// Warm VMs in pools, by pid: when each became ready, and its CPU then.
    warm: HashMap<u32, (Instant, u64)>,
    /// VMs claimed from a pool, by pid: their CPU as they became ready, and as claimed.
    claimed: HashMap<u32, (u64, u64)>,
    /// What warm VMs spent idling, by how long they idled.
    idle: Fit,
    /// A restart's CPU in ns, smoothed: none before one was read.
    restart: Option<u64>,
}

impl Costs {
    /// What the home's last daemon learned, or nothing.
    pub(super) fn load(home: &Path) -> Costs {
        let text = std::fs::read_to_string(home.join(COSTS)).unwrap_or_default();
        let mut words = text.split_whitespace();
        let restart = words.next().and_then(|w| w.parse::<u64>().ok());
        let sums: Vec<f64> = words
            .filter_map(|w| w.parse().ok())
            .filter(|x: &f64| x.is_finite())
            .collect();
        match (restart, sums.as_slice()) {
            (Some(restart), &[n, t, c, tt, tc]) => Costs {
                restart: Some(restart).filter(|&ns| ns > 0),
                idle: Fit { n, t, c, tt, tc },
                ..Costs::default()
            },
            _ => Costs::default(),
        }
    }

    /// Kept in the home, whole or not at all.
    fn save(&self, home: &Path) {
        let Fit { n, t, c, tt, tc } = self.idle;
        let text = format!("{} {n} {t} {c} {tt} {tc}\n", self.restart.unwrap_or(0));
        let (path, temporary) = (home.join(COSTS), home.join(format!("{COSTS}.new")));
        let saved = std::fs::write(&temporary, text).and_then(|()| std::fs::rename(&temporary, &path));
        if let Err(e) = saved {
            log(format!("{}: {e}", path.display()));
        }
    }

    /// Warm VM `pid` is ready, at `now`, with `cpu` ns spent; `live` are the pools' VMs,
    /// whose entries are kept.
    fn readied(&mut self, pid: u32, now: Instant, cpu: u64, live: &[u32]) {
        self.warm.retain(|p, _| live.contains(p));
        self.warm.insert(pid, (now, cpu));
    }

    /// Warm VM `pid` was claimed by a run at `now`, with `cpu` ns spent: what it spent
    /// since ready, idle, counted.
    fn claimed(&mut self, pid: u32, now: Instant, cpu: u64) {
        let Some((ready_at, ready_cpu)) = self.warm.remove(&pid) else {
            return;
        };
        let spent = Duration::from_nanos(cpu.saturating_sub(ready_cpu));
        self.idle.add(now.saturating_duration_since(ready_at), spent);
        self.claimed.insert(pid, (ready_cpu, cpu));
    }

    /// The run of VM `pid` ended, with `cpu` ns spent: what it cost, its restore and its
    /// run, its idling in a pool left out; the host's smoothed restart cost moved toward it.
    fn ended(&mut self, pid: u32, cpu: Option<u64>) -> Option<u64> {
        let claimed = self.claimed.remove(&pid);
        let cost = match (cpu, claimed) {
            (Some(end), Some((ready, claim))) => ready.saturating_add(end.saturating_sub(claim)),
            (Some(end), None) => end,
            (None, _) => return None,
        };
        self.restart = Some(self.restart.map_or(cost, |s| {
            let s = u128::from(s);
            u64::try_from((s * 7 + u128::from(cost)) / 8).unwrap_or(u64::MAX)
        }));
        Some(cost)
    }

    /// An idle microVM's CPU a second, once one was measured.
    fn idle(&self) -> Option<f64> {
        self.idle.rate()
    }
}

/// A least-squares fit of warm VMs' CPU, `c`, over their times warm, `t`, in seconds: the
/// count of them and the sums it needs.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Fit {
    n: f64,
    t: f64,
    c: f64,
    tt: f64,
    tc: f64,
}

impl Fit {
    /// A warm VM that spent `cpu` in `warm`.
    fn add(&mut self, warm: Duration, cpu: Duration) {
        let (t, c) = (warm.as_secs_f64(), cpu.as_secs_f64());
        self.n += 1.0;
        self.t += t;
        self.c += c;
        self.tt += t * t;
        self.tc += t * c;
    }

    /// CPU a second: the fit's slope, where its times differ and it rises; else their CPU
    /// over their time; none before any time.
    fn rate(&self) -> Option<f64> {
        let spread = self.n * self.tt - self.t * self.t;
        let slope = (self.n * self.tc - self.t * self.c) / spread;
        let rate = if self.n >= 2.0 && spread > 0.0 && slope > 0.0 {
            slope
        } else {
            self.c / self.t
        };
        (rate.is_finite() && rate >= 0.0 && self.t > 0.0).then_some(rate)
    }
}

/// What a microVM's restarts keep between its runs: its last wait, and whether its next
/// restart is cancelled.
#[derive(Debug, Default)]
pub(super) struct Manager {
    wait: Duration,
    cancelled: bool,
}

/// A restart's waits, from its `cost` in CPU time: the first, at `allowance` millionths of
/// a CPU, and the longest, at an `idle` microVM's CPU a second, none where none was
/// measured or it spent none.
fn waits(cost: Duration, allowance: u64, idle: Option<f64>) -> (Duration, Option<Duration>) {
    let ns = cost.as_nanos();
    let first = ns.saturating_mul(1_000_000) / u128::from(allowance.max(1));
    let first = Duration::from_nanos(u64::try_from(first).unwrap_or(u64::MAX));
    // Its CPU in ns a second, whole: `as` saturates, and takes NaN to 0.
    let longest = idle
        .map(|rate| (rate * 1e9).round() as u128)
        .filter(|&per_second| per_second > 0)
        .map(|per_second| {
            let longest = ns.saturating_mul(1_000_000_000) / per_second;
            Duration::from_nanos(u64::try_from(longest).unwrap_or(u64::MAX))
        });
    (first, longest)
}

/// Whether a microVM whose run ended with `exit` after `ran` starts again by its policy,
/// and after how long, its restart costing `cost` of CPU, its run's limits allowing it
/// `allowance` millionths of a CPU (`resources::cpu_allowance_micros`), and an idle microVM
/// costing `idle`: its first wait, then twice its last, up to the longest (or its first,
/// where that is longer); from its first again after a run of the longest or more, or,
/// with no longest, of its last wait or more.
#[allow(clippy::too_many_arguments)]
fn decide(
    manager: &mut Manager,
    c: &Container,
    exit: u8,
    ran: Option<Duration>,
    stopping: bool,
    allowance: u64,
    cost: Duration,
    idle: Option<f64>,
) -> Option<Duration> {
    let r = &c.restart;
    if matches!(r.policy.as_str(), "" | "no") || manager.cancelled {
        return None;
    }
    // A daemon stopping stops its microVMs by hand, as `unless-stopped` reads it.
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
    if !restart {
        return None;
    }
    let (first, longest) = waits(cost, allowance, idle);
    // Before an idle microVM is measured, a run as long as its last wait.
    if ran.is_some_and(|ran| ran >= longest.unwrap_or(manager.wait)) {
        manager.wait = Duration::ZERO;
    }
    manager.wait = if manager.wait.is_zero() {
        first
    } else {
        let doubled = manager.wait.saturating_mul(2);
        longest.map_or(doubled, |l| doubled.min(l)).max(first)
    };
    Some(manager.wait)
}

impl<D: Disk> Daemon<D> {
    /// Warm VM `pid` is ready in a pool, among `live` (restart.rs `Costs`).
    pub(super) fn vm_readied(&self, pid: u32, live: &[u32]) {
        if let Some(cpu) = cpu_of(pid) {
            lock(&self.costs).readied(pid, Instant::now(), cpu, live);
        }
    }

    /// Warm VM `pid` was claimed by a run.
    pub(super) fn vm_claimed(&self, pid: u32) {
        if let Some(cpu) = cpu_of(pid) {
            lock(&self.costs).claimed(pid, Instant::now(), cpu);
        }
    }

    /// The run of VM `pid` ended: its cost, as its process says it, if it can.
    pub(super) fn vm_ended(&self, pid: u32) -> Option<Duration> {
        let cpu = cpu_of(pid);
        lock(&self.costs).ended(pid, cpu).map(Duration::from_nanos)
    }

    /// Whether container `id`, whose run ended with `exit` after `ran_ns`, costing `cost`
    /// of CPU, starts again by its policy, and after how long. Its count goes up as it is
    /// recorded restarting.
    pub(super) fn should_restart(
        &self,
        id: &str,
        exit: u8,
        ran_ns: Option<u128>,
        cost: Option<Duration>,
    ) -> Option<Duration> {
        let (c, dir) = {
            let containers = lock(&self.containers);
            (containers.made(id).cloned()?, containers.dir(id))
        };
        let host = crate::resources::host_cpus();
        // Its limits as its request keeps them; every CPU where it keeps none.
        let allowance = std::fs::read(dir.join(super::REQUEST))
            .ok()
            .and_then(|b| Run::decode(&b))
            .map_or(
                crate::resources::cpu_allowance_micros(&Default::default(), host),
                |run| crate::resources::cpu_allowance_micros(&run.resources, host),
            );
        let ran = ran_ns.map(|ns| Duration::from_nanos(u64::try_from(ns).unwrap_or(u64::MAX)));
        let (cost, idle) = {
            let costs = lock(&self.costs);
            costs.save(&self.home);
            let host_cost = costs.restart.map(Duration::from_nanos);
            (cost.or(host_cost).or(ran).unwrap_or_default(), costs.idle())
        };
        let stopping = self.stopping.load(Ordering::SeqCst);
        let mut managers = lock(&self.restarts);
        let manager = managers.entry(id.to_string()).or_default();
        let wait = decide(manager, &c, exit, ran, stopping, allowance, cost, idle);
        if let Some(wait) = wait {
            let (_, longest) = waits(cost, allowance, idle);
            log(format!(
                "container {id}: restarts in {wait:?}: its restart {cost:?} of CPU, at most {longest:?} apart"
            ));
        }
        wait
    }

    /// Starts container `id` again after `wait`, on the followers' loop.
    pub(super) fn schedule_restart(&self, id: &str, wait: Duration) {
        let f = &self.followers;
        let token = f.next_token();
        lock(&f.restarts).insert(token, id.to_string());
        // A wait past what an Instant holds is as long as one can be here.
        let now = Instant::now();
        let at = now
            .checked_add(wait)
            .or_else(|| now.checked_add(Duration::from_secs(u64::from(u32::MAX))))
            .unwrap_or(now);
        lock(&f.deadlines).push(std::cmp::Reverse((at, token)));
        f.wake();
    }

    /// Container `id`'s wait to restart is up: started again, as a detached `shards start vm`
    /// starts it, unless a stop, kill, removal or the daemon's own stop came first; then
    /// it is stopped, so that a cancelled restart leaves it as its stop asked (D27).
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
                .open(crate::os::NULL)
                .map_err(|e| format!("{}: {e}", crate::os::NULL))
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
    /// its count; one `shards start vm` or `run` asked for starts its restarts over, so that
    /// its next waits its first wait again.
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
                decide(
                    &mut fresh,
                    c,
                    c.exit_code.unwrap_or(0),
                    None,
                    false,
                    u64::MAX,
                    Duration::ZERO,
                    None,
                )
                .is_some()
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

    /// What each policy restarts: `always`; `unless-stopped` unless stopped by hand (the
    /// daemon's own stop counting); `on-failure` on a failure, below its count; none once
    /// cancelled.
    #[test]
    fn restarts_are_decided_by_policy() {
        let d = |c: &Container, exit, stopping| {
            decide(
                &mut Manager::default(),
                c,
                exit,
                None,
                stopping,
                u64::MAX,
                MS,
                None,
            )
            .is_some()
        };
        assert!(!d(&container("no", 0, 0, false), 1, false));
        assert!(!d(&container("", 0, 0, false), 1, false));
        assert!(!d(&container("unless-stopped", 0, 0, true), 1, false));
        assert!(d(&container("unless-stopped", 0, 0, false), 0, false));
        assert!(!d(&container("unless-stopped", 0, 0, false), 0, true));
        assert!(d(&container("always", 0, 0, true), 0, true));
        assert!(!d(&container("on-failure", 0, 0, false), 0, false));
        assert!(d(&container("on-failure", 3, 2, false), 1, false));
        assert!(!d(&container("on-failure", 3, 3, false), 1, false));
        let mut m = Manager {
            cancelled: true,
            ..Manager::default()
        };
        assert_eq!(
            decide(
                &mut m,
                &container("always", 0, 0, false),
                0,
                None,
                false,
                u64::MAX,
                MS,
                None
            ),
            None
        );
    }

    const MS: Duration = Duration::from_millis(1);

    /// The waits' basis: the first a restart's CPU time at the run's share of a CPU; each
    /// twice the last, as dockerd's are; the longest the restart's CPU time at an idle
    /// microVM's rate, so that a microVM failing for good costs its host what one idling
    /// does; from the first again after a run of the longest or more.
    #[test]
    fn restart_waits_come_from_what_a_restart_costs() {
        let always = container("always", 0, 0, false);
        // A restart of 80 ms of CPU, every CPU of a host of 8, an idle microVM 10 ms a
        // second: at first 10 ms, at most 8 s.
        let (cost, every, idle) = (MS * 80, 8_000_000, Some(0.01));
        let (first, longest) = (MS * 10, Duration::from_secs(8));
        assert_eq!(waits(cost, every, idle), (first, Some(longest)));
        let mut m = Manager::default();
        let next = |m: &mut Manager, ran| decide(m, &always, 1, ran, false, every, cost, idle).unwrap();
        let waits_: Vec<_> = (0..16).map(|_| next(&mut m, Some(MS))).collect();
        assert_eq!(&waits_[..4], &[first, first * 2, first * 4, first * 8]);
        for pair in waits_.windows(2) {
            assert!(pair[1] == pair[0] * 2 || pair[1] == longest, "{pair:?}");
        }
        assert_eq!(waits_[15], longest, "doubled past the longest, it stays there");
        // A loop's restarts from its first failure: within log2(t / first) + 1 while its
        // waits grow.
        let grown = waits_.iter().position(|&w| w == longest).unwrap();
        let ramp: Duration = waits_[..grown].iter().sum();
        let bound = (ramp.as_nanos() / first.as_nanos()).ilog2() + 1;
        assert!(
            u32::try_from(grown).unwrap() <= bound,
            "{grown} restarts, {bound}"
        );
        // A run of the longest paid for its restart: its waits start over.
        assert_eq!(next(&mut m, Some(longest / 2)), longest);
        assert_eq!(next(&mut m, Some(longest)), first);
        // `--cpus 0.05`: its restart's CPU at its share, 1.6 s.
        let mut m = Manager::default();
        let small = decide(&mut m, &always, 1, None, false, 50_000, cost, idle).unwrap();
        assert_eq!(small, MS * 1_600);
        // A share so small that its first is past the longest waits its first, each time.
        let mut m = Manager::default();
        let tiny = decide(&mut m, &always, 1, None, false, 1, cost, idle).unwrap();
        assert!(tiny > longest);
        assert_eq!(decide(&mut m, &always, 1, None, false, 1, cost, idle), Some(tiny));
    }

    /// Before an idle microVM was measured (a cold start), the waits double on, and start
    /// over after a run as long as the last.
    #[test]
    fn a_cold_host_doubles_its_waits_on() {
        let always = container("always", 0, 0, false);
        let mut m = Manager::default();
        let next = |m: &mut Manager, ran| decide(m, &always, 1, ran, false, 1_000_000, MS, None).unwrap();
        let waits_: Vec<_> = (0..40).map(|_| next(&mut m, Some(Duration::ZERO))).collect();
        assert_eq!(waits_[0], MS);
        let longest = Duration::from_millis(1 << 39);
        assert_eq!(waits_[39], longest);
        assert_eq!(next(&mut m, Some(longest)), MS);
        // An idle microVM that spent no CPU leaves the waits uncapped too.
        assert_eq!(waits(MS, 1_000_000, Some(0.0)), (MS, None));
    }

    /// What a host learns: a warm VM's idling from ready to claimed; its run's cost, its
    /// restore and its run without its idling, smoothed into the host's; kept in the home.
    #[test]
    fn a_host_learns_what_restarts_cost() {
        let t0 = Instant::now();
        let mut costs = Costs::default();
        assert_eq!(costs.idle(), None);
        // Ready at 30 ms of CPU, claimed 2 s later at 50 ms, ended at 90 ms.
        costs.readied(7, t0, 30_000_000, &[]);
        costs.readied(8, t0, 1, &[7]);
        costs.claimed(7, t0 + Duration::from_secs(2), 50_000_000);
        assert_eq!(costs.idle(), Some(0.01));
        assert_eq!(costs.ended(7, Some(90_000_000)), Some(70_000_000));
        assert_eq!(costs.restart, Some(70_000_000));
        // A cold boot's VM, never pooled: its whole CPU; smoothed into the host's.
        assert_eq!(costs.ended(9, Some(150_000_000)), Some(150_000_000));
        assert_eq!(costs.restart, Some(80_000_000));
        // One whose CPU cannot be read says nothing, and leaves the host's.
        assert_eq!(costs.ended(10, None), None);
        assert_eq!(costs.restart, Some(80_000_000));
        // Claimed without being seen ready: nothing to learn.
        costs.claimed(11, t0, 5);
        assert_eq!(costs.idle(), Some(0.01));
        // Warm VMs that left their pools are let go of as the next is ready.
        costs.readied(12, t0, 1, &[12]);
        assert_eq!(costs.warm.keys().copied().collect::<Vec<_>>(), vec![12]);
        let home = shards_testdir::TempDir::new("restart-costs").unwrap();
        costs.save(home.path());
        let loaded = Costs::load(home.path());
        assert_eq!((loaded.restart, loaded.idle), (Some(80_000_000), costs.idle));
        assert_eq!(Costs::load(&home.path().join("none")).restart, None);
        std::fs::write(home.path().join(COSTS), "garbled").unwrap();
        assert_eq!(Costs::load(home.path()).idle(), None);
    }

    /// An idle microVM's rate is the slope of warm VMs' CPU over their time warm: what each
    /// does as it becomes ready (20 ms here) falls in the intercept, where their CPU over
    /// their time would count it many times over for short waits.
    #[test]
    fn idle_is_the_slope_of_warm_vms_cpu() {
        let mut fit = Fit::default();
        assert_eq!(fit.rate(), None);
        let warm = |fit: &mut Fit, s: f64| {
            fit.add(
                Duration::from_secs_f64(s),
                Duration::from_secs_f64(0.02 + 0.006 * s),
            )
        };
        warm(&mut fit, 0.1);
        assert!(
            (fit.rate().unwrap() - 0.206).abs() < 1e-9,
            "one alone: its CPU over its time"
        );
        warm(&mut fit, 0.1);
        assert!(
            (fit.rate().unwrap() - 0.206).abs() < 1e-9,
            "times alike fit no slope"
        );
        warm(&mut fit, 1.0);
        warm(&mut fit, 10.0);
        assert!((fit.rate().unwrap() - 0.006).abs() < 1e-9, "{:?}", fit.rate());
        let ratio = fit.c / fit.t;
        assert!(ratio > 0.007, "{ratio}");
    }

    /// This host's own VM processes' CPU can be read, so that what restarts cost is
    /// learned on every platform: this process's, as it burns some.
    #[test]
    fn a_processs_cpu_is_read_on_this_host() {
        let pid = std::process::id();
        let before = cpu_of(pid).unwrap();
        let t0 = Instant::now();
        let mut x = 0u64;
        while t0.elapsed() < Duration::from_millis(100) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        assert!(cpu_of(pid).unwrap() > before);
    }
}

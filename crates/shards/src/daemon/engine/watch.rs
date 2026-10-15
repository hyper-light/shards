//! Watching microVMs through the API (D170): `shards stats vm`, what each costs, and
//! `shards events system`, what happens to them.

use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::{Value, json};
use shards_ipc::kind;

use super::http::{Answer, Stream};
use super::{Call, Done, refused};

use crate::containers::Disk;
use crate::daemon::{Daemon, RunState, Threads, lock};

/// How often a stream of stats is sampled: as `shards stats vm` samples, once a second.
const EVERY: Duration = Duration::from_secs(1);

/// Waits up to `limit` for `client` to go, or to send something, which a client that
/// waits sends none of: whether it did.
fn gone_within(client: &UnixStream, limit: Duration) -> bool {
    let mut polled = libc::pollfd {
        fd: client.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = libc::c_int::try_from(limit.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: poll(2) on one pollfd of a descriptor this function's caller holds open.
    unsafe { libc::poll(&raw mut polled, 1, ms) > 0 }
}

/// A sample of a microVM: its workload's use as its guest's cgroup has it, its own cost
/// to the host as the host has it (its VM process), and when it was taken, on the host's
/// clock, which the guest's own does not keep on a loaded host.
struct Sample {
    guest: crate::daemon::commands::Usage,
    host: Option<shards_vmm::platform::Usage>,
    /// Nanoseconds on the host's monotonic clock.
    at: u64,
    /// Its wall time, for the API's `read`.
    read: u128,
}

impl<D: Disk> Daemon<D> {
    /// A sample of microVM `id`, zeros where it does not run.
    fn sample(&self, id: &str) -> Sample {
        let pid = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => Some(t.vm.id()),
            _ => None,
        };
        let guest = pid.and_then(|_| self.usage_of(id)).unwrap_or_default();
        let host = pid.and_then(|pid| shards_vmm::platform::process_usage(pid).ok());
        Sample {
            guest,
            host,
            at: u64::try_from(shards_vmm::log::uptime_us().saturating_mul(1000)).unwrap_or(u64::MAX),
            read: crate::containers::now(),
        }
    }

    /// `GET /containers/{id}/stats`: `shards stats vm`. What a microVM costs, a sample a
    /// second while `stream` (the default), else one, its CPU over the second before it
    /// unless `one-shot`. Its workload's use is its guest cgroup's; its system time is the
    /// host's clock times its vCPUs, so that a client's percentage of the two is its share
    /// of the vCPUs it had, as `shards stats vm` works it out. Beside it, `microvm`: what it
    /// costs the host, its VM process's own memory and its CPU time, vCPUs and devices
    /// together.
    pub(super) fn engine_stats(&self, conn: &UnixStream, given: &str, call: &Call<'_>) -> Done {
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return Done::Answer(refused(&e, 500)),
        };
        let name = lock(&self.containers)
            .get(&id)
            .map(|c| format!("/{}", c.name))
            .unwrap_or_default();
        let stream = call.query("stream").is_none() || call.flag("stream");
        let one_shot = call.flag("one-shot");
        let Ok(mut out) = Stream::start(conn, call.head, 200, "application/json") else {
            return Done::Close;
        };
        let mut before = None;
        if !stream && !one_shot {
            before = Some(self.sample(&id));
            if gone_within(conn, EVERY) {
                return Done::Close;
            }
        }
        loop {
            let now = self.sample(&id);
            if out.json(&document(&id, &name, &now, before.as_ref())).is_err() {
                return Done::Close;
            }
            if !stream {
                break;
            }
            before = Some(now);
            if gone_within(conn, EVERY) {
                return Done::Close;
            }
        }
        match out.end() {
            Ok(()) => Done::Keep,
            Err(_) => Done::Close,
        }
    }

    /// `GET /events`: `shards events system`. What happens to microVMs and images, as it
    /// happens, from `since` until `until` (or the client goes), by the query's filters,
    /// each an API message.
    pub(super) fn engine_events<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        call: &Call<'_>,
    ) -> Done {
        let filters = match super::containers::filters(call.query("filters")) {
            Ok(f) => f.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            Err(e) => return Done::Answer(Answer::error(400, e)),
        };
        let (since, until) = (
            call.query("since").unwrap_or_default(),
            call.query("until").unwrap_or_default(),
        );
        // Its times checked first, as the command checks them: its head is written at
        // once, before any event, and nothing can be refused after it.
        let now = i128::try_from(crate::containers::now()).unwrap_or(i128::MAX);
        let offset = i64::from(crate::cli::utc_offset());
        if let Err(e) = crate::daemon::events::bounds([(since, "since"), (until, "until")], now, offset) {
            return Done::Answer(Answer::error(400, e));
        }
        let mut argv = vec!["events", "--format", "{{json .}}"];
        for (value, flag) in [(since, "--since"), (until, "--until")] {
            if !value.is_empty() {
                argv.extend([flag, value]);
            }
        }
        for f in &filters {
            argv.extend(["--filter", f]);
        }
        let ipc = match self.command_conn(threads, &argv) {
            Ok(ipc) => ipc,
            Err(e) => return Done::Answer(Answer::error(500, e)),
        };
        let Ok(mut out) = Stream::start(conn, call.head, 200, "application/json") else {
            return Done::Close;
        };
        loop {
            let m = match shards_ipc::recv(&ipc) {
                Ok(Some(m)) => m,
                Ok(None) | Err(_) => return Done::Close,
            };
            match m.kind {
                kind::OUT => {
                    if out.send(&m.payload).is_err() {
                        return Done::Close;
                    }
                }
                kind::END => break,
                _ => {}
            }
        }
        match out.end() {
            Ok(()) => Done::Keep,
            Err(_) => Done::Close,
        }
    }
}

/// A sample `now`, after `before` where there is one, as the API's stats are laid out.
fn document(id: &str, name: &str, now: &Sample, before: Option<&Sample>) -> Value {
    let g = &now.guest;
    let cpu = |s: Option<&Sample>| match s {
        Some(s) => json!({
            "cpu_usage": { "total_usage": s.guest.cpu, "usage_in_kernelmode": 0, "usage_in_usermode": 0 },
            "system_cpu_usage": s.at.saturating_mul(s.guest.online),
            "online_cpus": s.guest.online,
            "throttling_data": { "periods": 0, "throttled_periods": 0, "throttled_time": 0 },
        }),
        None => json!({
            "cpu_usage": { "total_usage": 0, "usage_in_kernelmode": 0, "usage_in_usermode": 0 },
            "throttling_data": { "periods": 0, "throttled_periods": 0, "throttled_time": 0 },
        }),
    };
    let time = |ns: Option<u128>| crate::daemon::inspect_doc::go_time(ns);
    let mut doc = json!({
        "name": name,
        "id": id,
        "read": time(Some(now.read)),
        "preread": time(before.map(|b| b.read)),
        "pids_stats": { "current": g.pids },
        "blkio_stats": {
            "io_service_bytes_recursive": [
                { "major": 0, "minor": 0, "op": "read", "value": g.read },
                { "major": 0, "minor": 0, "op": "write", "value": g.write },
            ],
        },
        "num_procs": 0,
        "storage_stats": {},
        "cpu_stats": cpu(Some(now)),
        "precpu_stats": cpu(before),
        "memory_stats": {
            "usage": g.memory,
            "stats": { "inactive_file": g.inactive_file },
            "limit": g.limit,
        },
        "networks": {
            "eth0": {
                "rx_bytes": g.rx, "rx_packets": 0, "rx_errors": 0, "rx_dropped": 0,
                "tx_bytes": g.tx, "tx_packets": 0, "tx_errors": 0, "tx_dropped": 0,
            },
        },
    });
    if let (Some(host), Some(d)) = (now.host, doc.as_object_mut()) {
        d.insert(
            "microvm".into(),
            json!({ "footprint": host.footprint, "resident": host.resident, "cpu_ns": host.cpu_ns }),
        );
    }
    doc
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A client's CPU percentage of two samples, worked out as the API's clients work it
    /// out (cpu delta over system delta, times the CPUs), is the share of its vCPUs the
    /// workload had over the host's time between them.
    #[test]
    fn a_clients_cpu_percentage_is_the_share_of_its_vcpus() {
        let sample = |cpu: u64, at: u64| Sample {
            guest: crate::daemon::commands::Usage {
                cpu,
                online: 4,
                ..Default::default()
            },
            host: None,
            at,
            read: 0,
        };
        // One vCPU of four busy for the whole second.
        let (before, now) = (
            sample(5_000_000_000, 10_000_000_000),
            sample(6_000_000_000, 11_000_000_000),
        );
        let d = document("id", "/n", &now, Some(&before));
        let cpu = |k: &str| d[k]["cpu_usage"]["total_usage"].as_u64().unwrap() as f64;
        let system = |k: &str| d[k]["system_cpu_usage"].as_u64().unwrap() as f64;
        let online = d["cpu_stats"]["online_cpus"].as_u64().unwrap() as f64;
        let percent = (cpu("cpu_stats") - cpu("precpu_stats"))
            / (system("cpu_stats") - system("precpu_stats"))
            * online
            * 100.0;
        assert!((percent - 100.0).abs() < 1e-9, "{percent}");
        // No sample before: none to work a percentage from.
        assert!(
            document("id", "/n", &now, None)["precpu_stats"]
                .get("system_cpu_usage")
                .is_none()
        );
    }
}

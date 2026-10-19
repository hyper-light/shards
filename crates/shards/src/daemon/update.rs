//! `shards update` (moby docker-v29.8.1 daemon/update.go, ContainerUpdate; container
//! UpdateContainer): a container's limits and restart policy changed, checked as at create,
//! merged into what it has, kept, and written to its running workload's cgroup at once.

use shards_cmdline::flags::Parsed;
use shards_ipc::Resources;

use super::commands::Reply;
use super::{Daemon, REQUEST, RunState, lock};
use crate::containers::Disk;

/// The resources `update` asks for (docker/cli update.go): each given, or none.
fn asked(parsed: &Parsed) -> Resources {
    let number = |name: &str| parsed.string(name).parse::<i64>().unwrap_or(0);
    Resources {
        memory: number("memory"),
        memory_reservation: number("memory-reservation"),
        memory_swap: number("memory-swap"),
        nano_cpus: number("cpus"),
        cpu_shares: parsed.int("cpu-shares"),
        cpu_period: parsed.int("cpu-period"),
        cpu_quota: parsed.int("cpu-quota"),
        cpuset_cpus: parsed.string("cpuset-cpus").to_string(),
        cpuset_mems: parsed.string("cpuset-mems").to_string(),
        pids_limit: parsed.int("pids-limit"),
        blkio_weight: parsed.string("blkio-weight").parse().unwrap_or(0),
        ..Resources::default()
    }
}

/// What `new` gives over what `had`, as UpdateContainer merges them and in its order,
/// refused where they conflict, in shards' words (D158).
fn merge(had: &Resources, new: &Resources) -> Result<Resources, String> {
    let conflict = |why: &str| Err(why.to_string());
    if new.nano_cpus > 0 && had.cpu_period > 0 {
        return conflict("--cpus: its CPU time is set by --cpu-period already: update that and --cpu-quota");
    }
    if new.nano_cpus > 0 && had.cpu_quota > 0 {
        return conflict("--cpus: its CPU time is set by --cpu-quota already: update that and --cpu-period");
    }
    if new.cpu_period > 0 && had.nano_cpus > 0 {
        return conflict("--cpu-period: its CPU time is set by --cpus already: update --cpus");
    }
    if new.cpu_quota > 0 && had.nano_cpus > 0 {
        return conflict("--cpu-quota: its CPU time is set by --cpus already: update --cpus");
    }
    let mut r = had.clone();
    if new.blkio_weight != 0 {
        r.blkio_weight = new.blkio_weight;
    }
    if new.cpu_shares != 0 {
        r.cpu_shares = new.cpu_shares;
    }
    if new.nano_cpus != 0 {
        r.nano_cpus = new.nano_cpus;
    }
    if new.cpu_period != 0 {
        r.cpu_period = new.cpu_period;
    }
    if new.cpu_quota != 0 {
        r.cpu_quota = new.cpu_quota;
    }
    if !new.cpuset_cpus.is_empty() {
        r.cpuset_cpus.clone_from(&new.cpuset_cpus);
    }
    if !new.cpuset_mems.is_empty() {
        r.cpuset_mems.clone_from(&new.cpuset_mems);
    }
    if new.memory != 0 {
        // dockerd keeps the swap it worked out at create (twice the memory), or none.
        let swap = crate::resources::memory_swap(had);
        if new.memory > swap && new.memory_swap == 0 {
            return Err(format!(
                "--memory {}: past its --memory-swap {swap}, the memory and the swap together: give --memory-swap too",
                new.memory
            ));
        }
        r.memory = new.memory;
        // The swap it had stays what it was worked out to be, not twice the new memory.
        if new.memory_swap == 0 && had.memory_swap == 0 {
            r.memory_swap = swap;
        }
    }
    if new.memory_swap != 0 {
        r.memory_swap = new.memory_swap;
    }
    if new.memory_reservation != 0 {
        r.memory_reservation = new.memory_reservation;
    }
    if new.pids_limit != 0 {
        r.pids_limit = new.pids_limit;
    }
    Ok(r)
}

impl<D: Disk> Daemon<D> {
    /// `shards update [OPTIONS] CONTAINER...`: each container updated said by the name
    /// it was given, then dockerd's warnings; every error after, each its own.
    pub(super) fn update(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let new = asked(parsed);
        let policy = parsed.string("restart");
        let policy = if policy.is_empty() {
            None
        } else {
            match restart_policy(policy) {
                Ok(p) => Some(p),
                Err(e) => {
                    reply.err(&e);
                    return 1;
                }
            }
        };
        // verifyContainerSettings, of what is asked: its warnings, or its refusal.
        let checked =
            crate::resources::verify(&new, crate::resources::host_cpus(), true).and_then(|warnings| {
                if let Some(p) = &policy {
                    super::validate_restart_policy(p)?;
                }
                Ok(warnings)
            });
        let mut warnings = Vec::new();
        let mut status = 0;
        for given in &parsed.args {
            let done = checked.clone().and_then(|w| {
                warnings.extend(w);
                self.update_one(given, &new, policy.as_ref())
            });
            match done {
                Ok(()) => reply.out(given),
                Err(e) => {
                    reply.err(&format!("shards: {e}"));
                    status = 1;
                }
            }
        }
        if !warnings.is_empty() {
            reply.out(&warnings.join("\n"));
        }
        status
    }

    /// Container `given` updated: merged, kept with its request (which its next start
    /// and inspect read), and written to its workload's cgroup if it runs.
    fn update_one(&self, given: &str, new: &Resources, policy: Option<&(String, i64)>) -> Result<(), String> {
        let id = self
            .resolve(given)
            .map_err(|e| e.strip_prefix("shards: ").unwrap_or(&e).to_string())?;
        let cannot = |e: String| format!("cannot update {given}: {e}");
        if lock(&self.removing).contains(&id) {
            return Err(cannot("it is being removed".into()));
        }
        let dir = lock(&self.containers).dir(&id);
        let mut run = std::fs::read(dir.join(REQUEST))
            .ok()
            .and_then(|b| shards_ipc::Run::decode(&b))
            .ok_or_else(|| cannot("its request is not kept".into()))?;
        let merged = merge(&run.resources, new).map_err(cannot)?;
        if let Some(p) = policy {
            let auto_remove = lock(&self.containers).get(&id).is_some_and(|c| c.auto_remove);
            if auto_remove && !matches!(p.0.as_str(), "" | "no") {
                return Err(cannot(
                    "--restart: it is removed as it ends (--rm), so it is never restarted".into(),
                ));
            }
            run.restart_policy = p.clone();
        }
        // Written to the running workload first: a limit it refuses changes nothing.
        let restarting = self.is_restarting(&id);
        if self.running(&id) && !restarting {
            // A microVM's own run's limit is the most its guest plugs (D148): its VM told
            // the new most first, on the link the exec below comes on after it, so that
            // what its guest reports it may plug is the new (D149), up or down. A joiner's
            // limit is its own cgroup's alone; its microVM's most its provider's.
            if merged.memory != run.resources.memory {
                let own = match lock(&self.runs).get(&id) {
                    Some(RunState::Tracked(t)) if t.joined.is_none() => Some(t.socket.clone()),
                    _ => None,
                };
                if let Some(socket) = own {
                    let (_, boot, most) = crate::run::sizes(&merged, crate::build::host_memory());
                    let bytes = most.saturating_sub(boot).saturating_mul(1 << 20);
                    socket
                        .send(shards_ipc::kind::PLUG, &bytes.to_be_bytes(), &[])
                        .map_err(|e| cannot(format!("its microVM: {e}")))?;
                }
            }
            let spec = shards_abi::run::Spec {
                builtin: shards_abi::run::builtin::CGROUP,
                argv: crate::resources::cgroup(&merged, crate::resources::host_cpus()),
                ..Default::default()
            };
            let q = self
                .exec_quietly(&id, &spec, None, 1 << 12, super::TAKE_TIMEOUT)
                .map_err(|e| cannot(e.to_string()))?;
            if q.status != Some(0) {
                return Err(cannot(String::from_utf8_lossy(&q.output).trim().to_string()));
            }
        }
        run.resources = merged;
        std::fs::write(dir.join(REQUEST), run.encode()).map_err(|e| cannot(e.to_string()))?;
        if let Some(p) = policy {
            let changed = lock(&self.containers).change(&id, |c| {
                c.restart.policy.clone_from(&p.0);
                c.restart.max = p.1;
            });
            changed.map_err(|e| cannot(e.to_string()))?;
            self.record_soon(&id, Vec::new());
        }
        self.container_event(&id, "update", &[]);
        Ok(())
    }
}

/// opts.ParseRestartPolicy, as the CLI reads `--restart` (cli/request.rs's twin).
fn restart_policy(policy: &str) -> Result<(String, i64), String> {
    let (name, count) = policy.split_once(':').unwrap_or((policy, ""));
    if policy.contains(':') && name.is_empty() {
        return Err("invalid restart policy format: no policy provided before colon".into());
    }
    let count = if count.is_empty() {
        0
    } else {
        shards_cmdline::go::parse_int10(count).map_err(|_| {
            "invalid restart policy format: maximum retry count must be an integer".to_string()
        })?
    };
    Ok((name.to_string(), count))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Updates merged as dockerd's UpdateContainer merges them (probed on Docker Engine
    /// 29.3.1): a memory limit over no swap refused, with swap taken; CPU limits that
    /// conflict with what is set refused, in shards' words.
    #[test]
    fn updates_merge_with_the_limits_set() {
        let none = Resources::default();
        let memory = Resources {
            memory: 64 << 20,
            ..Resources::default()
        };
        assert_eq!(
            merge(&none, &memory),
            Err("--memory 67108864: past its --memory-swap 0, the memory and the swap together: give --memory-swap too".into())
        );
        let with_swap = Resources {
            memory_swap: 128 << 20,
            ..memory.clone()
        };
        let merged = merge(&none, &with_swap).unwrap();
        assert_eq!((merged.memory, merged.memory_swap), (64 << 20, 128 << 20));
        // Under the swap create worked out (twice the memory), a smaller limit passes.
        let had = Resources {
            memory: 64 << 20,
            ..Resources::default()
        };
        let lower = Resources {
            memory: 32 << 20,
            ..Resources::default()
        };
        let merged = merge(&had, &lower).unwrap();
        assert_eq!((merged.memory, merged.memory_swap), (32 << 20, 128 << 20));
        let cpus = Resources {
            nano_cpus: 1_000_000_000,
            ..Resources::default()
        };
        let period = Resources {
            cpu_period: 50_000,
            ..Resources::default()
        };
        assert_eq!(
            merge(&cpus, &period),
            Err("--cpu-period: its CPU time is set by --cpus already: update --cpus".into())
        );
        assert_eq!(
            merge(&period, &cpus),
            Err("--cpus: its CPU time is set by --cpu-period already: update that and --cpu-quota".into())
        );
    }
}

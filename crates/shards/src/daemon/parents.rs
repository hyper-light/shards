//! The groups of the host a run's microVM is made under (D183, `--cgroup-parent`): on
//! Linux, a slice of the user's systemd manager, where a spawner of the daemon's, placed
//! once as the one process of a scope of its own there, makes every process of each
//! microVM under it, each born there and charged there from its first page (PM M248: a
//! process moved keeps its memory where it was, and each move waits out an RCU grace
//! period); elsewhere, refused.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use shards_ipc::{Group, Run};

use super::lock;

/// A run's `--cgroup-parent`, refused where it cannot be served, before anything starts.
pub(super) fn check(run: &Run) -> Result<(), String> {
    let given = &run.cgroup_parent;
    if given.is_empty() {
        return Ok(());
    }
    if !cfg!(target_os = "linux") {
        return Err(format!(
            "--cgroup-parent {given}: this host has no group of processes a microVM's can be made in and held to; its microVM's own limits are its --memory and --cpus"
        ));
    }
    if !is_slice_name(given) {
        return Err(format!(
            "--cgroup-parent {given}: your user's cgroups are its systemd manager's, which makes them for its slices: name the slice its microVM's processes go under, as NAME.slice (a '-' nests one within another: a-b.slice is within a.slice)"
        ));
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getuid(2) takes nothing and cannot fail.
        let bus = shards_bus::user_bus_path(unsafe { libc::getuid() });
        if !bus.exists() {
            return Err(format!(
                "--cgroup-parent {given}: no systemd manager of your user's runs here to place its microVM under a slice of: its bus, {}, is not there",
                bus.display()
            ));
        }
    }
    Ok(())
}

/// Whether `name` names a slice as systemd's own check has it (src/basic/unit-name.c,
/// `slice_name_is_valid`, `unit_name_is_valid`): `-.slice`, the root slice; or a name
/// shorter than 256 bytes ending `.slice`, before it letters, digits and `:-_.\` alone,
/// no `-` first or last, and none beside another.
pub(super) fn is_slice_name(name: &str) -> bool {
    if name == "-.slice" {
        return true;
    }
    let Some(prefix) = name.strip_suffix(".slice") else {
        return false;
    };
    name.len() < 256
        && !prefix.is_empty()
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b":-_.\\".contains(&b))
        && !prefix.starts_with('-')
        && !prefix.ends_with('-')
        && !prefix.contains("--")
}

/// A parent's spawner: being placed by the run that first asked for it, or placed.
#[derive(Debug)]
enum Entry {
    Placing,
    Placed(Group),
}

/// Each parent's spawner (D183), made and placed once, as its first run asks: the
/// placement of one keeps no run under another waiting.
#[derive(Debug, Default)]
pub(super) struct Parents {
    by_slice: Mutex<HashMap<String, Entry>>,
    placed: Condvar,
    /// Spawners placed so far, which name their scopes apart.
    made: AtomicU64,
}

impl Parents {
    /// `slice`'s spawner: placed already, or made and placed now, as the one process of a
    /// scope of its own in `slice`. One whose placement the user's manager refuses is
    /// ended, and what the manager said is said.
    pub(super) fn group(&self, slice: &str, home: &Path) -> Result<Group, String> {
        let mut held = lock(&self.by_slice);
        loop {
            match held.get(slice) {
                Some(Entry::Placed(group)) => return Ok(*group),
                Some(Entry::Placing) => {
                    held = self.placed.wait(held).unwrap_or_else(PoisonError::into_inner);
                }
                None => break,
            }
        }
        held.insert(slice.to_string(), Entry::Placing);
        drop(held);
        let made = self.place(slice, home);
        let mut held = lock(&self.by_slice);
        match &made {
            Ok(group) => {
                held.insert(slice.to_string(), Entry::Placed(*group));
            }
            Err(_) => {
                held.remove(slice);
            }
        }
        drop(held);
        self.placed.notify_all();
        made
    }

    /// Forgets `slice`'s spawner `group`, which has gone (its slice stopped, say), and
    /// ends it: the next run under `slice` places another.
    pub(super) fn forget(&self, slice: &str, group: Group) {
        let mut held = lock(&self.by_slice);
        if matches!(held.get(slice), Some(Entry::Placed(g)) if *g == group) {
            held.remove(slice);
        }
        drop(held);
        shards_ipc::end_group(group);
    }

    #[cfg(target_os = "linux")]
    fn place(&self, slice: &str, home: &Path) -> Result<Group, String> {
        let refused = |e: &dyn std::fmt::Display| {
            format!("--cgroup-parent {slice}: your user's systemd manager placed no microVM there: {e}")
        };
        let group = shards_ipc::start_group().map_err(|e| refused(&e))?;
        let n = self.made.fetch_add(1, Ordering::Relaxed);
        let unit = format!("shards-{}-{n}.scope", std::process::id());
        let description = format!("shards: microVMs of {}", home.display());
        // SAFETY: getuid(2) takes nothing and cannot fail.
        let uid = unsafe { libc::getuid() };
        let placed = shards_bus::user_bus(uid)
            .and_then(|mut bus| shards_bus::place(&mut bus, &unit, slice, group.pid(), &description, false));
        match placed {
            Ok(()) => Ok(group),
            Err(e) => {
                shards_ipc::end_group(group);
                Err(refused(&e))
            }
        }
    }

    /// Where no user's manager places a process: refused by [`check`] before any run asks.
    #[cfg(not(target_os = "linux"))]
    fn place(&self, slice: &str, _home: &Path) -> Result<Group, String> {
        let _ = self.made.load(Ordering::Relaxed);
        Err(format!(
            "--cgroup-parent {slice}: this host has no group of processes a microVM's can be made in"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_is_named_as_systemd_names_one() {
        for good in [
            "agents.slice",
            "agents-ci.slice",
            "a.b.slice",
            "a:b_c\\x2d.slice",
            "-.slice",
        ] {
            assert!(is_slice_name(good), "{good}");
        }
        let long = format!("{}.slice", "a".repeat(250));
        for bad in [
            "agents",
            "/agents",
            "/user.slice/agents.slice",
            ".slice",
            "-agents.slice",
            "agents-.slice",
            "a--b.slice",
            "a b.slice",
            "a@b.slice",
            "agents.service",
            long.as_str(),
        ] {
            assert!(!is_slice_name(bad), "{bad}");
        }
        assert!(is_slice_name(&format!("{}.slice", "a".repeat(249))));
    }

    #[test]
    fn a_parent_is_refused_before_anything_starts_where_it_cannot_be_served() {
        let run = |parent: &str| Run {
            cgroup_parent: parent.to_string(),
            ..Run::default()
        };
        assert_eq!(check(&run("")), Ok(()));
        let refused = check(&run("/x")).unwrap_err();
        if cfg!(target_os = "linux") {
            assert!(refused.contains("NAME.slice"), "{refused}");
            // Where the user's manager has a bus, a slice; where it has none, refused for that.
            match check(&run("agents.slice")) {
                Ok(()) => {}
                Err(why) => assert!(why.contains("no systemd manager of your user's"), "{why}"),
            }
        } else {
            assert!(refused.contains("--memory"), "{refused}");
            assert!(check(&run("agents.slice")).is_err());
        }
    }
}

//! The groups of the host a run's microVM is made under (D183, `--cgroup-parent`): on
//! Linux, a slice of the user's systemd manager, where a spawner of the daemon's, placed
//! once as the one process of a scope of its own there, makes every process of each
//! microVM under it, each born there and charged there from its first page (PM M248: a
//! process moved keeps its memory where it was, and each move waits out an RCU grace
//! period); or a cgroup delegated to the user, named by its path, where the spawner is
//! moved once into a leaf of its own (slice 2); elsewhere, refused.

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
    // Where the host has no group a user's tools cap, one shards keeps (D183, slice 3).
    #[cfg(any(target_os = "macos", windows))]
    {
        if !super::groups::is_group_name(given) {
            return Err(format!(
                "--cgroup-parent {given}: on this host a group shards keeps, groups/{given} of its home, is named by letters, digits, '.', '-' and '_' alone"
            ));
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        if !cfg!(target_os = "linux") {
            return Err(format!(
                "--cgroup-parent {given}: this host has no group of processes a microVM's can be made in and held to; its microVM's own limits are its --memory and --cpus"
            ));
        }
        // A cgroup delegated to the user, by its path (slice 2): asked of no manager.
        if is_cgroup_path(given) {
            #[cfg(target_os = "linux")]
            return delegated::check(given);
        }
        if !is_slice_name(given) {
            return Err(format!(
                "--cgroup-parent {given}: your user's cgroups are its systemd manager's, which makes them for its slices: name the slice its microVM's processes go under, as NAME.slice (a '-' nests one within another: a-b.slice is within a.slice), or a cgroup delegated to you by its path from the hierarchy's root, as /PATH"
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
}

/// Whether `name` names a slice as systemd's own check has it (src/basic/unit-name.c,
/// `slice_name_is_valid`, `unit_name_is_valid`): `-.slice`, the root slice; or a name
/// shorter than 256 bytes ending `.slice`, before it letters, digits and `:-_.\` alone,
/// no `-` first or last, and none beside another.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
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

/// Whether `given` names a cgroup by its path from the hierarchy's root, as
/// `/proc/PID/cgroup` says one (D183, slice 2), rather than a slice of the user's manager.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
fn is_cgroup_path(given: &str) -> bool {
    given.starts_with('/')
}

/// A parent's spawner: being placed by the run that first asked for it, or placed. None on
/// macOS, whose parents are groups shards keeps (groups.rs).
#[derive(Debug)]
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
enum Entry {
    Placing,
    Placed(Group),
}

/// Each parent's spawner (D183), made and placed once, as its first run asks: the
/// placement of one keeps no run under another waiting.
#[derive(Debug, Default)]
pub(super) struct Parents {
    by_slice: Mutex<HashMap<String, Entry>>,
    #[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
    placed: Condvar,
    /// Spawners placed so far, which name their scopes apart.
    #[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
    made: AtomicU64,
    /// The leaves made in delegated cgroups (slice 2), each removed once it is empty, as
    /// the daemon exits ([`close`](Self::close)): no manager collects them.
    #[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
    leaves: Mutex<Vec<std::path::PathBuf>>,
}

impl Parents {
    /// `slice`'s spawner: placed already, or made and placed now, as the one process of a
    /// scope of its own in `slice`. One whose placement the user's manager refuses is
    /// ended, and what the manager said is said.
    #[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
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

    /// `slice`'s spawner, where it is placed already: none where it is not, or is being
    /// placed.
    pub(super) fn placed(&self, slice: &str) -> Option<Group> {
        match lock(&self.by_slice).get(slice) {
            Some(Entry::Placed(group)) => Some(*group),
            _ => None,
        }
    }

    /// Forgets `slice`'s spawner `group`, which has gone (its slice stopped, say), and
    /// ends it: the next run under `slice` places another.
    #[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
    pub(super) fn forget(&self, slice: &str, group: Group) {
        let mut held = lock(&self.by_slice);
        if matches!(held.get(slice), Some(Entry::Placed(g)) if *g == group) {
            held.remove(slice);
        }
        drop(held);
        shards_ipc::end_group(group);
    }

    /// Ends every parent's spawner, and removes each leaf made in a delegated cgroup once
    /// what was made in it has ended (slice 2), as the daemon exits: a slice's scope its
    /// manager collects (`CollectMode`), a leaf no one would. One whose processes outlive
    /// [`delegated::EMPTIED`] is left to the next daemon placing a spawner beside it.
    pub(super) fn close(&self) {
        let placed: Vec<Group> = lock(&self.by_slice)
            .drain()
            .filter_map(|(_, e)| match e {
                Entry::Placed(g) => Some(g),
                Entry::Placing => None,
            })
            .collect();
        for group in placed {
            shards_ipc::end_group(group);
        }
        #[cfg(target_os = "linux")]
        for leaf in lock(&self.leaves).drain(..) {
            if let Err(e) = delegated::remove(&leaf) {
                super::log(format!("{}: {e}", leaf.display()));
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn place(&self, slice: &str, home: &Path) -> Result<Group, String> {
        if is_cgroup_path(slice) {
            return self.place_delegated(slice);
        }
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

    /// A spawner moved, before it makes anything, into a leaf of its own in delegated
    /// cgroup `path` (slice 2): `shards-PID-N`, made by the daemon, as the kernel lets the
    /// user's processes be moved only within what is delegated to it (cgroup-v2.rst,
    /// "Delegation Containment"). The move is the one PM M248 measured (8.4 ms at p50 by
    /// hand), once per parent; its children are born in the leaf.
    #[cfg(target_os = "linux")]
    fn place_delegated(&self, path: &str) -> Result<Group, String> {
        let refused = |e: &dyn std::fmt::Display| {
            format!("--cgroup-parent {path}: no microVM can be made in this cgroup: {e}")
        };
        let dir = delegated::dir_of(path).map_err(|e| format!("--cgroup-parent {path}: {e}"))?;
        delegated::sweep(&dir);
        let group = shards_ipc::start_group().map_err(|e| refused(&e))?;
        let n = self.made.fetch_add(1, Ordering::Relaxed);
        let leaf = dir.join(format!("{}{}-{n}", delegated::LEAF, std::process::id()));
        if let Err(e) = std::fs::create_dir(&leaf) {
            shards_ipc::end_group(group);
            return Err(refused(&format_args!("{}: {e}", leaf.display())));
        }
        lock(&self.leaves).push(leaf.clone());
        let procs = leaf.join("cgroup.procs");
        if let Err(e) = std::fs::write(&procs, group.pid().to_string()) {
            shards_ipc::end_group(group);
            let _ = std::fs::remove_dir(&leaf);
            return Err(refused(&format_args!(
                "{}: {e} (the kernel moves a process only within a sub-hierarchy delegated to its user, the daemon's own cgroup in it)",
                procs.display()
            )));
        }
        Ok(group)
    }

    /// Where no user's manager places a process: refused by [`check`] before any run asks.
    #[cfg(not(target_os = "linux"))]
    #[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
    fn place(&self, slice: &str, _home: &Path) -> Result<Group, String> {
        let _ = self.made.load(Ordering::Relaxed);
        Err(format!(
            "--cgroup-parent {slice}: this host has no group of processes a microVM's can be made in"
        ))
    }
}

/// Cgroups delegated to the user where no manager of its places a process (D183, slice 2):
/// a cgroup namespace's root, a container's own, or a sub-hierarchy an administrator gave
/// the user, its `cgroup.procs` and directories theirs (cgroup-v2.rst, "Model of
/// Delegation", "Delegation Containment").
#[cfg(target_os = "linux")]
mod delegated {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::super::budget::ROOT;

    /// The start of every leaf's name, its daemon's PID after it.
    pub(super) const LEAF: &str = "shards-";

    /// How long a daemon exiting waits for what was made in a leaf to end before it
    /// leaves it: past its runs' stops (`stop_runs`), only its warm VMs, told to end
    /// as it exits, and the spawner itself, which it has reaped by then.
    pub(super) const EMPTIED: Duration = Duration::from_secs(10);

    /// The directory of the cgroup `path` names: a path from the hierarchy's root, no
    /// component of it `.` or `..`, on a cgroup v2 hierarchy.
    pub(super) fn dir_of(path: &str) -> Result<PathBuf, String> {
        let mut dir = PathBuf::from(ROOT);
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if part == "." || part == ".." {
                return Err(
                    "a cgroup is named by its path from the hierarchy's root, as /proc/self/cgroup says one, with no '.' or '..' in it".into(),
                );
            }
            dir.push(part);
        }
        if !Path::new(ROOT).join("cgroup.controllers").exists() {
            return Err(format!(
                "this host has no cgroup v2 hierarchy at {ROOT} to make its microVM's processes in"
            ));
        }
        if !dir.join("cgroup.procs").exists() {
            return Err(format!("no cgroup is there: {} is none", dir.display()));
        }
        Ok(dir)
    }

    /// The cgroup `path` names, refused before anything starts where the daemon cannot
    /// make a leaf in it (mkdir(2): write and search on its directory) or move a process
    /// of its own there (the `cgroup.procs` of the nearest cgroup the daemon's and it are
    /// both within, its user's: cgroup-v2.rst, "Delegation Containment").
    pub(super) fn check(path: &str) -> Result<(), String> {
        let at = |e: String| format!("--cgroup-parent {path}: {e}");
        let dir = dir_of(path).map_err(at)?;
        if !writable(&dir, libc::W_OK | libc::X_OK) {
            return Err(at(format!(
                "your user may make no cgroup in it: {} is not delegated to you",
                dir.display()
            )));
        }
        let own = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|c| c.lines().find_map(|l| l.strip_prefix("0::")).map(str::to_string))
            .ok_or_else(|| at("the daemon's own cgroup: /proc/self/cgroup says none".into()))?;
        let common = common_ancestor(&own, path);
        let procs = Path::new(ROOT)
            .join(common.trim_start_matches('/'))
            .join("cgroup.procs");
        if !writable(&procs, libc::W_OK) {
            return Err(at(format!(
                "the daemon, in cgroup {own}, may move no process of its into it: the cgroup both are within, {common}, is not delegated to your user ({} is not yours to write)",
                procs.display()
            )));
        }
        Ok(())
    }

    /// The nearest cgroup both paths are within: their longest common run of components.
    pub(super) fn common_ancestor(a: &str, b: &str) -> String {
        let parts = |p: &str| -> Vec<String> {
            p.split('/')
                .filter(|c| !c.is_empty())
                .map(str::to_string)
                .collect()
        };
        let (a, b) = (parts(a), parts(b));
        let shared: Vec<&str> = a
            .iter()
            .zip(&b)
            .take_while(|(x, y)| x == y)
            .map(|(x, _)| x.as_str())
            .collect();
        format!("/{}", shared.join("/"))
    }

    /// Whether this process may access `path` as `mode` asks (access(2), with its real
    /// IDs, which are the daemon's effective ones).
    fn writable(path: &Path, mode: libc::c_int) -> bool {
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: access(2) of a NUL-terminated path this function holds.
        unsafe { libc::access(c.as_ptr(), mode) == 0 }
    }

    /// Removes the leaves in `dir` of daemons no longer running (one killed, which
    /// removed none): a leaf still holding processes is refused by rmdir(2), and stays.
    pub(super) fn sweep(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .and_then(|n| n.strip_prefix(LEAF))
                .and_then(|n| n.split_once('-'))
                .and_then(|(pid, _)| pid.parse::<libc::pid_t>().ok())
                .filter(|pid| *pid > 0)
            else {
                continue;
            };
            // SAFETY: kill(2) with signal 0 sends nothing: whether `pid` is a process.
            let gone = unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if gone {
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }

    /// Removes `leaf` once nothing is in it (its `cgroup.events` saying `populated 0`),
    /// waiting at most [`EMPTIED`].
    pub(super) fn remove(leaf: &Path) -> std::io::Result<()> {
        let events = leaf.join("cgroup.events");
        let since = Instant::now();
        loop {
            let text = match std::fs::read_to_string(&events) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            };
            if text.lines().any(|l| l == "populated 0") {
                return std::fs::remove_dir(leaf);
            }
            if since.elapsed() >= EMPTIED {
                return Err(std::io::Error::other(format!(
                    "its processes ran on {EMPTIED:?} past the daemon's end: left for the next daemon placing beside it"
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
            // A path names a delegated cgroup (slice 2): none is at /x.
            assert!(refused.contains("no cgroup is there"), "{refused}");
            assert!(check(&run("/a/../b")).unwrap_err().contains("'..'"));
            let named = check(&run("agents")).unwrap_err();
            assert!(named.contains("NAME.slice") && named.contains("/PATH"), "{named}");
            // Where the user's manager has a bus, a slice; where it has none, refused for that.
            match check(&run("agents.slice")) {
                Ok(()) => {}
                Err(why) => assert!(why.contains("no systemd manager of your user's"), "{why}"),
            }
        } else if cfg!(any(target_os = "macos", windows)) {
            // A group shards keeps, named as a file of the home is (D183, slice 3).
            assert!(refused.contains("groups/"), "{refused}");
            assert_eq!(check(&run("agents.slice")), Ok(()));
            assert_eq!(check(&run("agents")), Ok(()));
            assert!(check(&run("..")).is_err());
        } else {
            assert!(refused.contains("--memory"), "{refused}");
            assert!(check(&run("agents.slice")).is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_delegated_cgroup_is_found_within_what_its_user_may_move_between() {
        use super::delegated::common_ancestor;
        assert_eq!(common_ancestor("/a/b/daemon", "/a/b/agents"), "/a/b");
        assert_eq!(common_ancestor("/a/b", "/a/b/agents"), "/a/b");
        assert_eq!(common_ancestor("/a/b/agents", "/a/b/"), "/a/b");
        assert_eq!(common_ancestor("/a/bc", "/a/b"), "/a");
        assert_eq!(common_ancestor("/x", "/y"), "/");
        assert_eq!(common_ancestor("/", "/y"), "/");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn leaves_of_daemons_gone_are_swept_and_others_kept() {
        let dir = shards_testdir::TempDir::new("leaves").unwrap();
        // A PID no process has: past the kernel's limit (PID_MAX_LIMIT, 4,194,304).
        let gone = dir.join(format!("{}4194305-0", super::delegated::LEAF));
        let mine = dir.join(format!("{}{}-0", super::delegated::LEAF, std::process::id()));
        let other = dir.join("agents");
        for d in [&gone, &mine, &other] {
            std::fs::create_dir(d).unwrap();
        }
        super::delegated::sweep(&dir);
        assert!(!gone.exists());
        assert!(mine.exists() && other.exists());
    }
}

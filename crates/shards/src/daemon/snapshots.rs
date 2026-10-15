//! A running microVM saved whole (D176): `shards save vm`. Its snapshots are in its
//! container's directory, `snapshots/NAME`, each a snapshot directory as
//! `shards_vmm::snapshot` keeps one: a generation the daemon prepares, which the microVM's
//! VM process fills, as it writes only what it is handed (D30), and the daemon commits.

use std::os::fd::{AsFd as _, BorrowedFd};
use std::sync::mpsc;

use shards_cmdline::flags::Parsed;
use shards_ipc::kind;

use super::commands::Reply;
use super::{Daemon, Inbox, RunState, lock};
use crate::containers::Disk;

/// Where a container's snapshots are, in its directory.
pub(super) const SNAPSHOTS: &str = "snapshots";

/// A save a run's VM was asked for (`kind::SAVE`), waiting for its answer
/// (`kind::SAVED`).
pub(super) struct Saving {
    /// Its answer: written, or why not. A VM that ends first did not write it.
    pub(super) told: mpsc::Sender<Result<(), String>>,
    /// The snapshot it is, where the microVM stops once saved.
    pub(super) stops_as: Option<String>,
}

/// Run `inbox`'s VM's answer to its save, `payload` empty for written, else why not: told
/// to the save that waits for it, and, for one that stops the microVM, kept for its end,
/// which comes after it.
pub(super) fn answered(inbox: &mut Inbox, payload: &[u8]) {
    let Some(saving) = inbox.saving.take() else {
        return;
    };
    let answer = if payload.is_empty() {
        inbox.saved_as = saving.stops_as;
        Ok(())
    } else {
        Err(String::from_utf8_lossy(payload).into_owned())
    };
    let _ = saving.told.send(answer);
}

/// Whether `name` may name a snapshot, as a checkpoint's name may (moby daemon/checkpoint.go,
/// validateCheckpointID): `[a-zA-Z0-9][a-zA-Z0-9_.-]+`, and no `.` last.
fn valid(name: &str) -> bool {
    !name.starts_with('/') && crate::containers::valid_name(name) && !name.ends_with('.')
}

/// A name for a snapshot saved without one: the time in UTC, to the second, as RFC 3339
/// writes it with its colons made dashes, `-N` after it where one has it already.
fn fresh(taken: &dyn Fn(&str) -> bool) -> String {
    let now = i64::try_from(crate::containers::now() / 1_000_000_000).unwrap_or(i64::MAX);
    let stamp = shards_cmdline::format::rfc3339_at(now, 0).replace(':', "-");
    (1..)
        .map(|n| {
            if n == 1 {
                stamp.clone()
            } else {
                format!("{stamp}-{n}")
            }
        })
        .find(|name| !taken(name))
        .unwrap_or(stamp)
}

impl<D: Disk> Daemon<D> {
    /// `shards save vm MICROVM [SNAPSHOT] [--leave-running]` (D176): the running microVM
    /// saved whole, its memory, every process and pty, its devices and its run's swap as
    /// they are, its vCPUs parked for the save; then stopped, as `docker checkpoint
    /// create` stops a container, or left running. Prints the snapshot's name.
    pub(super) fn save_vm(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        match self.save_microvm(parsed) {
            Ok(name) => {
                reply.out(&name);
                0
            }
            Err(e) => {
                reply.err(&e);
                1
            }
        }
    }

    fn save_microvm(&self, parsed: &Parsed) -> Result<String, String> {
        let reference = parsed.args.first().map(|r| r.trim()).unwrap_or_default();
        let leave_running = parsed.bool("leave-running");
        let id = self.resolve(reference)?;
        self.await_start(&id);
        if !self.running(&id) {
            return Err(format!("shards: {reference} is not running"));
        }
        // A joiner's processes are its provider's microVM's, which a save of that one
        // keeps (D119).
        if self.joined_run(&id) {
            return Err(format!(
                "shards: {reference} runs in another container's microVM: save that microVM"
            ));
        }
        let dir = lock(&self.containers).dir(&id).join(SNAPSHOTS);
        let name = match parsed.args.get(1) {
            Some(name) if valid(name) => name.clone(),
            Some(name) => {
                return Err(format!(
                    "shards: {name:?} cannot name a snapshot: a letter or digit, then letters, digits, '_', '.' or '-', and no '.' last"
                ));
            }
            None => fresh(&|name| dir.join(name).exists()),
        };
        let at = dir.join(&name);
        if at.exists() {
            return Err(format!("shards: {reference} has a snapshot named {name} already"));
        }
        let failed = |e: &dyn std::fmt::Display| format!("shards: cannot save {reference}: {e}");
        if let Err(e) = shards_vmm::platform::create_private_dir(&dir)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(failed(&e));
        }
        // With room for the run's swap, which a run without one leaves empty.
        let staged = shards_vmm::snapshot::prepare(&at, true).map_err(|e| failed(&e))?;
        let asked = self.ask_save(&id, &staged, &name, leave_running);
        let saved = asked.and_then(|()| staged.commit());
        if let Err(e) = saved {
            // Nothing of it stays: a name it took is free again.
            let _ = std::fs::remove_dir_all(&at);
            // One its save stopped keeps no snapshot it could resume.
            if !leave_running {
                let _ = lock(&self.containers).change(&id, |c| {
                    if c.saved.as_deref() == Some(name.as_str()) {
                        c.saved = None;
                    }
                });
            }
            return Err(failed(&e));
        }
        // moby's event for a checkpoint (daemon/checkpoint.go).
        self.container_event(&id, "checkpoint", &[]);
        Ok(name)
    }

    /// Asks container `id`'s VM to fill `staged`, its snapshot `snapshot`, and waits for its
    /// answer: a save that stops the microVM unless `leave_running`. A frozen microVM goes
    /// on for the moment its save takes to park its vCPUs, and is frozen again after one
    /// that leaves it running.
    fn ask_save(
        &self,
        id: &str,
        staged: &shards_vmm::snapshot::Staged,
        snapshot: &str,
        leave_running: bool,
    ) -> Result<(), String> {
        let (inbox, socket, vm) = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) => (t.inbox.clone(), t.socket.clone(), t.vm.clone()),
            _ => return Err("it is not running".into()),
        };
        let microvm = lock(&self.containers)
            .made(id)
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let stops_as = (!leave_running).then(|| snapshot.to_string());
        let (told, answer) = mpsc::channel();
        {
            let mut inbox = lock(&inbox);
            if inbox.ended {
                return Err("it is not running".into());
            }
            if inbox.saving.is_some() {
                return Err("it is being saved already".into());
            }
            inbox.saving = Some(Saving { told, stops_as });
        }
        let files = staged.handed()?;
        let fds: Vec<BorrowedFd<'_>> = files.iter().map(|f| f.as_fd()).collect();
        let flags = if leave_running {
            shards_ipc::SAVE_LEAVE_RUNNING
        } else {
            0
        };
        let mut payload = vec![flags];
        for (i, name) in [staged.name(), snapshot, &microvm].into_iter().enumerate() {
            if i > 0 {
                payload.push(0);
            }
            payload.extend_from_slice(name.as_bytes());
        }
        if let Err(e) = socket.send(kind::SAVE, &payload, &fds) {
            lock(&inbox).saving = None;
            return Err(format!("asking its microVM: {e}"));
        }
        let frozen = lock(&self.paused).contains(id);
        if frozen {
            let _ = vm.kill(libc::SIGCONT);
        }
        // As long as the save takes, which is its memory's writing: its VM answers, or
        // ends, and the run's end tells this then.
        let answered = answer
            .recv()
            .unwrap_or_else(|_| Err("its microVM stopped".into()));
        if frozen && leave_running && lock(&self.paused).contains(id) {
            let _ = vm.kill(libc::SIGSTOP);
        }
        answered
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A snapshot is named as a checkpoint is, and one saved without a name takes the
    /// time's, made unique.
    #[test]
    fn snapshots_are_named_as_checkpoints_are() {
        for good in ["s1", "first", "a.b-c_d", "2026-10-15T12-30-05Z"] {
            assert!(valid(good), "{good}");
        }
        for bad in ["", "a", "-a", ".a", "a.", "a/b", "/ab", "a b", "a:b"] {
            assert!(!valid(bad), "{bad}");
        }
        let stamp = fresh(&|_| false);
        assert!(valid(&stamp), "{stamp}");
        assert!(stamp.ends_with('Z') && !stamp.contains(':'), "{stamp}");
        let next = fresh(&|name| name == stamp);
        assert_ne!(next, stamp);
        // Unless the second turned between the two.
        if next.starts_with(&stamp) {
            assert_eq!(next, format!("{stamp}-2"));
        }
    }
}

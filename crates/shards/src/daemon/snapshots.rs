//! A running microVM saved whole (D176): `shards save vm`. Its snapshots are in its
//! container's directory, `snapshots/NAME`, each a snapshot directory as
//! `shards_vmm::snapshot` keeps one: a generation the daemon prepares, which the microVM's
//! VM process fills, as it writes only what it is handed (D30), and the daemon commits.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use serde::{Deserialize, Serialize};
use shards_cmdline::flags::Parsed;
use shards_ipc::fd::{AsFd as _, BorrowedFd};
use shards_ipc::kind;

use super::commands::Reply;
use super::{Daemon, Inbox, RunState, lock};
use crate::containers::Disk;

/// Where a container's snapshots are, in its directory.
pub(super) const SNAPSHOTS: &str = "snapshots";

/// The file a snapshot's generation keeps its [`Resume`] in.
pub(super) const RESUME: &str = "resume";

/// What a snapshot of a run keeps beside what its VM wrote, for its restore: the save's
/// tokens, which only the guest's init holds, and with which the restored guest dials the
/// host again; and its interface's identity, which its guest holds as it was: its MAC,
/// and its addresses on the network it is on, which its restore asks for again as `--ip`
/// asks for one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Resume {
    /// The two tokens, the workload connection's then the signals', in hex.
    pub(super) tokens: String,
    /// Its MAC on its network, where it has an interface of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) mac: Option<String>,
    /// The user network it is on, by ID, and its addresses there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) network: Option<Address>,
    /// Its microVM's own engine's token (D181), in hex, which its guest's relay says on
    /// each connection, as it did before the save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) engine: Option<String>,
}

/// A microVM's addresses on a user network (D46).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Address {
    pub(super) network: String,
    pub(super) ipv4: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(super) ipv6: String,
}

/// `bytes` in lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The bytes `text`, in hex, says; none for text that is not hex.
pub(super) fn unhex(text: &str) -> Option<Vec<u8>> {
    let digits = text.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

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

/// Why a microVM was not saved (D176), said in the words of the command that asked:
/// shards' own `save vm`, or Docker's `checkpoint create` and the Engine API's, dockerd's.
pub(super) enum Unsaved {
    /// No container its reference names, as the resolve says it.
    Unknown(String),
    /// By its ID.
    NotRunning(String),
    /// A container joined to another's microVM (D119).
    Joined,
    /// On the host's network (D124).
    HostNetwork,
    /// A name no snapshot may have.
    Invalid(String),
    /// A name one of its snapshots has.
    Exists(String),
    /// Why its save failed.
    Failed(String),
}

impl Unsaved {
    /// In shards' words, of the microVM `reference` names.
    pub(super) fn ours(&self, reference: &str) -> String {
        match self {
            Unsaved::Unknown(e) => e.clone(),
            Unsaved::NotRunning(_) => format!("shards: {reference} is not running"),
            Unsaved::Joined => {
                format!("shards: {reference} runs in another container's microVM: save that microVM")
            }
            Unsaved::HostNetwork => format!(
                "shards: {reference} is on the host's network, whose sockets of the host's no snapshot keeps"
            ),
            Unsaved::Invalid(name) => format!(
                "shards: {name:?} cannot name a snapshot: a letter or digit, then letters, digits, '_', '.' or '-', and no '.' last"
            ),
            Unsaved::Exists(name) => format!("shards: {reference} has a snapshot named {name} already"),
            Unsaved::Failed(e) => format!("shards: cannot save {reference}: {e}"),
        }
    }

    /// What a `checkpoint create` command line, or the Engine API's, is refused with, of
    /// microVM `reference` and checkpoint `asked`, without the prefix each surface gives it:
    /// the words a `docker` script reads (moby daemon/checkpoint.go, CheckpointCreate).
    pub(super) fn checkpoint_refusal(&self, reference: &str, asked: &str) -> String {
        match self {
            Unsaved::Unknown(e) => e.strip_prefix("shards: ").unwrap_or(e).to_string(),
            // container.GetRunningTask's errNotRunning.
            Unsaved::NotRunning(id) => format!("container {id} is not running"),
            Unsaved::Invalid(_) => invalid_checkpoint(asked),
            Unsaved::Exists(name) => format!(
                "cannot checkpoint container {reference}: checkpoint with name {name} already exists for container {reference}"
            ),
            // Its task's CreateCheckpoint failing, as dockerd says it.
            Unsaved::Joined => format!(
                "Cannot checkpoint container {reference}: it runs in another container's microVM: checkpoint that one"
            ),
            Unsaved::HostNetwork => format!(
                "Cannot checkpoint container {reference}: it is on the host's network, whose sockets of the host's no checkpoint keeps"
            ),
            Unsaved::Failed(e) => format!("Cannot checkpoint container {reference}: {e}"),
        }
    }

    /// The Engine API's status for it, as dockerd's errdefs give it.
    pub(super) fn status(&self) -> u16 {
        match self {
            Unsaved::Unknown(_) => 404,
            Unsaved::NotRunning(_) => 409,
            Unsaved::Invalid(_) => 400,
            _ => 500,
        }
    }
}

/// Snapshot `at` removed, its pointer first: one cut short is then no snapshot a restore
/// takes, but a directory the next daemon's start removes ([`remove_unsaved`]).
fn remove(at: &std::path::Path) -> std::io::Result<()> {
    std::fs::remove_file(shards_vmm::snapshot::pointer(at))?;
    std::fs::remove_dir_all(at)
}

/// Removes the snapshots in container directory `dir` with no generation in place (D176),
/// as a daemon starts, before it serves: what a save its daemon's end cut short left, its
/// name free again, and what a removal cut short left, which takes the pointer first. What
/// cannot be removed is noted.
pub(super) fn remove_unsaved(dir: &std::path::Path, note: &mut dyn FnMut(String)) {
    let snapshots = dir.join(SNAPSHOTS);
    let entries = match std::fs::read_dir(&snapshots) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            note(format!("{}: {e}", snapshots.display()));
            return;
        }
    };
    for entry in entries {
        let at = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                note(format!("{}: {e}", snapshots.display()));
                continue;
            }
        };
        match std::fs::symlink_metadata(shards_vmm::snapshot::pointer(&at)) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Err(e) = std::fs::remove_dir_all(&at) {
                    note(format!("{}: {e}", at.display()));
                }
            }
            Err(e) => note(format!("{}: {e}", at.display())),
        }
    }
}

/// Removes from the home's private directory `dir` the files share processes made for
/// restored guests (D176) that a crash left before removing them, as a daemon starts, no
/// share process of its own running yet. Unix's alone: Windows runs no share process yet
/// (D38).
#[cfg(unix)]
pub(super) fn remove_ghosts(dir: &std::path::Path, note: &mut dyn FnMut(String)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|n| n.starts_with(shards_vmm::devices::virtio::fs::server::GHOST_PREFIX))
            && let Err(e) = std::fs::remove_file(entry.path())
        {
            note(format!("{}: {e}", entry.path().display()));
        }
    }
}

/// Whether `asked` may name a checkpoint (moby validateCheckpointID).
pub(super) fn valid_checkpoint(asked: &str) -> bool {
    valid(asked)
}

/// moby's validateCheckpointID's refusal of `asked`.
pub(super) fn invalid_checkpoint(asked: &str) -> String {
    format!(
        "invalid checkpoint ID {}, only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed",
        shards_cmdline::go::quote(asked)
    )
}

/// Whether `name` may name a snapshot, as a checkpoint's name may (moby daemon/checkpoint.go,
/// validateCheckpointID): `[a-zA-Z0-9][a-zA-Z0-9_.-]+`, and no `.` last.
fn valid(name: &str) -> bool {
    !name.starts_with('/') && crate::containers::valid_name(name) && !name.ends_with('.')
}

/// A name for a snapshot saved without one, which `claim` takes: the time in UTC, to the
/// second, as RFC 3339 writes it with its colons made dashes, `-N` after it where one has
/// it already (`claim` failing with AlreadyExists). Any other failure is the save's.
fn fresh(claim: &mut dyn FnMut(&str) -> std::io::Result<()>) -> std::io::Result<String> {
    let now = i64::try_from(crate::containers::now() / 1_000_000_000).unwrap_or(i64::MAX);
    let stamp = shards_cmdline::format::rfc3339_at(now, 0).replace(':', "-");
    for n in 1..=u64::MAX {
        let name = if n == 1 {
            stamp.clone()
        } else {
            format!("{stamp}-{n}")
        };
        match claim(&name) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            claimed => return claimed.map(|()| name),
        }
    }
    Err(std::io::Error::other("every name of the time is taken"))
}

/// `run`, a container's request, as its microVM resumed from a snapshot has its interface
/// (D176): its own endpoint given the guest's MAC, as `--mac-address` gives it one, and its
/// addresses on its network, as `--ip` and `--ip6` ask for them.
pub(super) fn resume_interface(run: &mut shards_ipc::Run, resume: &Resume) {
    if resume.mac.is_none() && resume.network.is_none() {
        return;
    }
    if crate::setup::own_endpoint(run).is_none() {
        run.endpoints.push(shards_ipc::Endpoint {
            network: run.network.clone(),
            ..shards_ipc::Endpoint::default()
        });
    }
    let ours = |n: &str| {
        n == run.network
            || matches!(
                (n, run.network.as_str()),
                ("bridge", "default") | ("default", "bridge")
            )
    };
    if let Some(e) = run.endpoints.iter_mut().find(|e| ours(&e.network)) {
        if let Some(mac) = &resume.mac {
            e.mac.clone_from(mac);
        }
        if let Some(address) = &resume.network {
            e.ipv4.clone_from(&address.ipv4);
            e.ipv6.clone_from(&address.ipv6);
        }
    }
}

/// A copy of the swap a snapshot's `generation` kept (D176), at `at`, for a run resumed
/// from it, only this user's and as sparse as the swap was; none where it kept none.
pub(super) fn copy_swap(generation: &Path, at: &Path) -> std::io::Result<Option<std::fs::File>> {
    let kept = std::fs::File::open(shards_vmm::snapshot::swap_of(generation))?;
    let len = kept.metadata()?.len();
    if len == 0 {
        return Ok(None);
    }
    let copy = crate::os::owner_only(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true),
    )
    .open(at)?;
    shards_vmm::platform::copy_used(&kept, &copy, len)?;
    Ok(Some(copy))
}

/// When a generation named `name` was written, in nanoseconds since the epoch: `g-` and the
/// time, in hex (vmm snapshot.rs, `fresh_name`).
fn saved_at(name: &str) -> Option<i64> {
    let hex = name.strip_prefix("g-")?.split('-').next()?;
    i64::try_from(u128::from_str_radix(hex, 16).ok()?).ok()
}

/// The disk the files in `generation` take: their blocks, as sparse as each is.
fn on_disk(generation: &Path) -> u64 {
    std::fs::read_dir(generation)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| Some(crate::os::on_disk(&e.path(), &e.metadata().ok()?)))
                .fold(0, u64::saturating_add)
        })
        .unwrap_or(0)
}

/// A snapshot of a container to resume it from (D176): its directory, its generation,
/// and what it kept for its restore, its tokens read.
pub(super) struct Resumable {
    pub(super) dir: PathBuf,
    pub(super) generation: PathBuf,
    pub(super) resume: Resume,
    pub(super) tokens: Vec<u8>,
    pub(super) engine: Option<[u8; shards_abi::run::TOKEN]>,
}

/// The most a resume record holds: its tokens, a MAC, and a network's ID and addresses.
const MAX_RESUME: u64 = 4 << 10;

/// The newest snapshot in `snapshots`, a container's: the one whose generation was written
/// last, which its name says (`g-` and the time in nanoseconds, in hex).
fn newest(snapshots: &Path) -> Option<String> {
    std::fs::read_dir(snapshots)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let generation = shards_vmm::snapshot::generation(&entry.path()).ok()?;
            Some((generation.file_name()?.to_string_lossy().into_owned(), name))
        })
        .max()
        .map(|(_, name)| name)
}

impl<D: Disk> Daemon<D> {
    /// Container `id`'s snapshot `asked`, or its newest where `asked` is empty, to resume
    /// it from, `given` naming the container as its client did: why not, where it has none.
    pub(super) fn resumable(&self, id: &str, given: &str, asked: &str) -> Result<Resumable, String> {
        let snapshots = lock(&self.containers).dir(id).join(SNAPSHOTS);
        let name = match asked {
            "" => newest(&snapshots).ok_or_else(|| format!("{given} has no snapshot to restore"))?,
            asked if valid(asked) => asked.to_string(),
            asked => return Err(format!("{asked:?} names no snapshot")),
        };
        let dir = snapshots.join(&name);
        let generation = shards_vmm::snapshot::generation(&dir)
            .map_err(|_| format!("{given} has no snapshot named {name}"))?;
        let unusable = |e: &dyn std::fmt::Display| format!("{given}'s snapshot {name}: {e}");
        let record = shards_vmm::platform::read_input(&generation.join(RESUME), MAX_RESUME)
            .map_err(|e| unusable(&e))?
            .ok_or_else(|| unusable(&"it was saved by no run"))?;
        let resume: Resume = serde_json::from_slice(&record).map_err(|e| unusable(&e))?;
        let tokens = unhex(&resume.tokens)
            .filter(|t| t.len() == 2 * shards_abi::run::TOKEN)
            .ok_or_else(|| unusable(&"its tokens"))?;
        let engine = match &resume.engine {
            Some(hex) => Some(
                unhex(hex)
                    .and_then(|t| <[u8; shards_abi::run::TOKEN]>::try_from(t).ok())
                    .ok_or_else(|| unusable(&"its engine's token"))?,
            ),
            None => None,
        };
        Ok(Resumable {
            dir,
            generation,
            resume,
            tokens,
            engine,
        })
    }

    /// A VM restoring `r`, a snapshot of a run, for that run resumed (D176), ready: given
    /// the files the snapshot restores against, as it names them, which the daemon wrote;
    /// in the GPU's VM process where it has a GPU; its network process framing to `mac`,
    /// the guest's own; made under `parent` (D183).
    pub(super) fn resumed_vm<'s, 'e>(
        &'s self,
        threads: &'s super::Threads<'s, 'e>,
        r: &Resumable,
        mac: Option<[u8; 6]>,
        parent: &str,
    ) -> Result<super::Ready, String> {
        let pinned = shards_vmm::snapshot::read(&r.dir)?;
        let mut args: Vec<OsString> = vec![
            "restore".into(),
            r.dir.clone().into_os_string(),
            "--resume".into(),
            super::VM_LINK.into(),
        ];
        for (path, read_only) in shards_vmm::snapshot::backing_of(&pinned) {
            let mut backing = path.into_os_string();
            if read_only {
                backing.push(":ro");
            }
            args.extend(["--backing".into(), backing]);
        }
        if pinned.snapshot.config.gpu.is_some() {
            args.push("--gpu".into());
        }
        let net = pinned.snapshot.config.net.map(|saved| mac.unwrap_or(saved));
        // Its writable layer (D190): a clone of the save's, or a file it copies it into.
        let layer = (pinned.snapshot.config.layer_mib > 0)
            .then(|| super::layers::LayerFile::for_restore(&self.home, &pinned.path))
            .transpose()?;
        drop(pinned);
        self.start_for_run(threads, &args, net, parent, layer)
    }

    /// `shards list snapshots MICROVM` (D176): its snapshots, newest first, each one's name,
    /// when it was saved, and the disk its files take.
    pub(super) fn list_snapshots(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let reference = parsed.args.first().map(|r| r.trim()).unwrap_or_default();
        let id = match self.resolve(reference) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        let dir = lock(&self.containers).dir(&id).join(SNAPSHOTS);
        let now = i64::try_from(crate::containers::now()).unwrap_or(i64::MAX);
        let mut rows: Vec<(String, [String; 3])> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| {
                        let name = entry.file_name().into_string().ok()?;
                        let generation = shards_vmm::snapshot::generation(&entry.path()).ok()?;
                        let written = generation.file_name()?.to_string_lossy().into_owned();
                        let saved = saved_at(&written).map_or_else(
                            || "-".to_string(),
                            |at| {
                                format!(
                                    "{} ago",
                                    shards_cmdline::format::human_duration(now.saturating_sub(at))
                                )
                            },
                        );
                        let size = shards_cmdline::format::human_size(on_disk(&generation) as f64);
                        Some((written, [name, saved, size]))
                    })
                    .collect()
            })
            .unwrap_or_default();
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        let widths = rows.iter().fold(
            ["SNAPSHOT".len(), "SAVED".len()],
            |[n, s], (_, [name, saved, _])| [n.max(name.len()), s.max(saved.len())],
        );
        let [n, s] = widths;
        reply.out(&format!("{:<n$}   {:<s$}   SIZE", "SNAPSHOT", "SAVED"));
        for (_, [name, saved, size]) in rows {
            reply.out(&format!("{name:<n$}   {saved:<s$}   {size}"));
        }
        0
    }

    /// `shards remove snapshot MICROVM SNAPSHOT...` (D176): each named, as given, once
    /// removed; then the errors, and the status 1 where there was one.
    pub(super) fn remove_snapshots(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let reference = parsed.args.first().map(|r| r.trim()).unwrap_or_default();
        let id = match self.resolve(reference) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        let dir = lock(&self.containers).dir(&id).join(SNAPSHOTS);
        let mut errors = Vec::new();
        for name in parsed.args.iter().skip(1) {
            let at = dir.join(name);
            if !valid(name) || shards_vmm::snapshot::generation(&at).is_err() {
                errors.push(format!("shards: {reference} has no snapshot named {name}"));
                continue;
            }
            match remove(&at) {
                Ok(()) => reply.out(name),
                Err(e) => errors.push(format!("shards: removing {reference}'s snapshot {name}: {e}")),
            }
        }
        for e in &errors {
            reply.err(e);
        }
        u8::from(!errors.is_empty())
    }

    /// `shards save vm MICROVM [SNAPSHOT] [--leave-running]` (D176): the running microVM
    /// saved whole, its memory, every process and pty, its devices and its run's swap as
    /// they are, its vCPUs parked for the save; then stopped, as `docker checkpoint
    /// create` stops a container, or left running. Prints the snapshot's name.
    pub(super) fn save_vm(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let reference = parsed.args.first().map(|r| r.trim()).unwrap_or_default();
        let asked = parsed.args.get(1).map(String::as_str);
        match self.save_snapshot(reference, asked, parsed.bool("leave-running")) {
            Ok((_, name)) => {
                reply.out(&name);
                0
            }
            Err(e) => {
                reply.err(&e.ours(reference));
                1
            }
        }
    }

    /// `shards checkpoint ls CONTAINER`, Docker's word for `list snapshots` (D176): its
    /// checkpoints' names in their names' order, as dockerd reads them (os.ReadDir) and
    /// docker/cli's table writes them (checkpoint/formatter.go).
    pub(super) fn checkpoint_ls(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let reference = parsed.args.first().map(|r| r.trim()).unwrap_or_default();
        let id = match self.resolve(reference) {
            Ok(id) => id,
            Err(e) => {
                reply.err(&e);
                return 1;
            }
        };
        reply.out("CHECKPOINT NAME");
        for name in self.snapshot_names(&id) {
            reply.out(&name);
        }
        0
    }

    /// `shards checkpoint rm CONTAINER CHECKPOINT`, Docker's word for `remove snapshot`
    /// (D176), its errors dockerd's (CheckpointDelete): nothing once removed.
    pub(super) fn checkpoint_rm(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let reference = parsed.args.first().map(|r| r.trim()).unwrap_or_default();
        let asked = parsed.args.get(1).map(String::as_str).unwrap_or_default();
        match self.remove_checkpoint(reference, asked) {
            Ok(()) => 0,
            Err((_, e)) => {
                reply.err(&format!("shards: {e}"));
                1
            }
        }
    }

    /// Container `reference`'s snapshot `asked` removed, as dockerd's CheckpointDelete
    /// removes a checkpoint: why not, in its words, with the Engine API's status.
    pub(super) fn remove_checkpoint(&self, reference: &str, asked: &str) -> Result<(), (u16, String)> {
        if !valid(asked) {
            return Err((400, invalid_checkpoint(asked)));
        }
        let id = self
            .resolve(reference)
            .map_err(|e| (404, e.strip_prefix("shards: ").unwrap_or(&e).to_string()))?;
        let at = lock(&self.containers).dir(&id).join(SNAPSHOTS).join(asked);
        if shards_vmm::snapshot::generation(&at).is_err() {
            return Err((
                500,
                format!("checkpoint {asked} does not exist for container {reference}"),
            ));
        }
        remove(&at).map_err(|e| (500, e.to_string()))
    }

    /// The names of container `id`'s snapshots, in their order.
    pub(super) fn snapshot_names(&self, id: &str) -> Vec<String> {
        let dir = lock(&self.containers).dir(id).join(SNAPSHOTS);
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| shards_vmm::snapshot::generation(&e.path()).is_ok())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// `shards checkpoint create [--leave-running] CONTAINER CHECKPOINT`, Docker's word for
    /// `save vm` (D176), its errors dockerd's (moby daemon/checkpoint.go): the checkpoint's
    /// name once saved, as docker/cli prints it.
    pub(super) fn checkpoint_create(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let reference = parsed.args.first().map(|r| r.trim()).unwrap_or_default();
        let asked = parsed.args.get(1).map(String::as_str).unwrap_or_default();
        match self.save_snapshot(reference, Some(asked), parsed.bool("leave-running")) {
            Ok((_, name)) => {
                reply.out(&name);
                0
            }
            Err(e) => {
                reply.err(&format!("shards: {}", e.checkpoint_refusal(reference, asked)));
                1
            }
        }
    }

    /// The running microVM `reference` names saved as snapshot `asked`, or as the time's
    /// where no name is asked for (D176): its ID and the snapshot's name, or why not.
    pub(super) fn save_snapshot(
        &self,
        reference: &str,
        asked: Option<&str>,
        leave_running: bool,
    ) -> Result<(String, String), Unsaved> {
        // A name no snapshot may have is refused before anything is looked up, as dockerd
        // refuses a checkpoint's (moby daemon/checkpoint.go, CheckpointCreate).
        if let Some(name) = asked
            && !valid(name)
        {
            return Err(Unsaved::Invalid(name.to_string()));
        }
        let id = self.resolve(reference).map_err(Unsaved::Unknown)?;
        self.await_start(&id);
        if !self.running(&id) {
            return Err(Unsaved::NotRunning(id));
        }
        // A joiner's processes are its provider's microVM's, which a save of that one
        // keeps (D119).
        if self.joined_run(&id) {
            return Err(Unsaved::Joined);
        }
        // On the host's network its guest's sockets are the host's, the daemon's to bind
        // (D124): a restored copy would hold sockets no host socket answers.
        let (inbox, mac) = match lock(&self.runs).get(&id) {
            Some(RunState::Tracked(t)) => (t.inbox.clone(), t.mac),
            _ => return Err(Unsaved::NotRunning(id)),
        };
        if lock(&inbox).host_net.is_some() {
            return Err(Unsaved::HostNetwork);
        }
        let failed = |e: &dyn std::fmt::Display| Unsaved::Failed(e.to_string());
        // Its tokens, which its guest is given, and its interface's identity, kept beside
        // it for its restore.
        let mut tokens = [0u8; 2 * shards_abi::run::TOKEN];
        shards_vmm::platform::fill_random(&mut tokens).map_err(|e| failed(&format!("its tokens: {e}")))?;
        let engine = lock(&inbox).engine.as_ref().map(|e| hex(&e.token));
        let resume = Resume {
            tokens: hex(&tokens),
            engine,
            mac: mac.map(|m| shards_net::Mac(m).to_string()),
            network: self.membership(&id).map(|(network, me)| Address {
                network: network.id,
                ipv4: me.ip.to_string(),
                ipv6: me.ip6.map(|ip| ip.to_string()).unwrap_or_default(),
            }),
        };
        let record = serde_json::to_vec(&resume).map_err(|e| failed(&e))?;
        let dir = lock(&self.containers).dir(&id).join(SNAPSHOTS);
        if let Err(e) = shards_vmm::platform::create_private_dir(&dir)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(failed(&e));
        }
        // Its name is taken as its directory is made: two saves never share one, where
        // dockerd's look and then make, and one that fails removes only what it made.
        let mut claim = |name: &str| std::fs::create_dir(dir.join(name));
        let name = match asked {
            Some(name) => match claim(name) {
                Ok(()) => name.to_string(),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(Unsaved::Exists(name.to_string()));
                }
                Err(e) => return Err(failed(&e)),
            },
            None => fresh(&mut claim).map_err(|e| failed(&e))?,
        };
        let at = dir.join(&name);
        // With room for the run's swap, which a run without one leaves empty.
        let saved = shards_vmm::snapshot::prepare(&at, true).and_then(|mut staged| {
            self.ask_save(&id, &staged, &name, &tokens, leave_running)?;
            staged.add(RESUME, &record)?;
            staged.commit()
        });
        if let Err(e) = saved {
            // Nothing of it stays: the name it took is free again.
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
        // Stopped by its save, it is stopped in its record before the save is said done.
        if !leave_running {
            self.await_end(&id);
        }
        // moby's event for a checkpoint (daemon/checkpoint.go).
        self.container_event(&id, "checkpoint", &[]);
        Ok((id, name))
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
        tokens: &[u8],
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
        payload.extend_from_slice(tokens);
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
            let _ = vm.kill(shards_ipc::signal::SIGCONT);
        }
        // As long as the save takes, which is its memory's writing: its VM answers, or
        // ends, and the run's end tells this then.
        let answered = answer
            .recv()
            .unwrap_or_else(|_| Err("its microVM stopped".into()));
        if frozen && leave_running && lock(&self.paused).contains(id) {
            let _ = vm.kill(shards_ipc::signal::SIGSTOP);
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
        let stamp = fresh(&mut |_| Ok(())).unwrap();
        assert!(valid(&stamp), "{stamp}");
        assert!(stamp.ends_with('Z') && !stamp.contains(':'), "{stamp}");
        let mut taken = |name: &str| {
            if name == stamp {
                Err(std::io::ErrorKind::AlreadyExists.into())
            } else {
                Ok(())
            }
        };
        let next = fresh(&mut taken).unwrap();
        assert_ne!(next, stamp);
        // Unless the second turned between the two.
        if next.starts_with(&stamp) {
            assert_eq!(next, format!("{stamp}-2"));
        }
        // A name that cannot be made for another reason fails the save, which takes no
        // other.
        let mut tried = 0;
        let failing = fresh(&mut |_| {
            tried += 1;
            Err(std::io::ErrorKind::PermissionDenied.into())
        });
        assert_eq!(
            (failing.map_err(|e| e.kind()), tried),
            (Err(std::io::ErrorKind::PermissionDenied), 1)
        );
        // A name refused is quoted as Go quotes it, as dockerd's refusal does.
        assert_eq!(
            invalid_checkpoint("a\x1bb"),
            "invalid checkpoint ID \"a\\x1bb\", only [a-zA-Z0-9][a-zA-Z0-9_.-] are allowed"
        );
    }

    /// A daemon's start removes what its end cut short of a snapshot's save or removal: a
    /// snapshot with no generation in place, never one with its pointer.
    #[test]
    fn a_start_removes_snapshots_cut_short() {
        let dir = shards_testdir::TempDir::new("cut").unwrap();
        let snapshots = dir.path().join(SNAPSHOTS);
        for (name, file) in [
            ("kept", "g-1/memory"),
            ("kept", shards_vmm::snapshot::CURRENT),
            ("cut", ".g-2.tmp/memory"),
            ("half", "g-3/memory"),
        ] {
            let at = snapshots.join(name).join(file);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, b"g-1\n").unwrap();
        }
        let mut notes = Vec::new();
        remove_unsaved(dir.path(), &mut |n| notes.push(n));
        assert!(notes.is_empty(), "{notes:?}");
        assert!(snapshots.join("kept/g-1/memory").exists());
        assert!(!snapshots.join("cut").exists() && !snapshots.join("half").exists());
        // A container with none has nothing to remove, and nothing to say.
        let none = shards_testdir::TempDir::new("none").unwrap();
        remove_unsaved(none.path(), &mut |n| notes.push(n));
        assert!(notes.is_empty(), "{notes:?}");
    }
}

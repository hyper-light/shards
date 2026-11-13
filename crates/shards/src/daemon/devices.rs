//! Devices plugged into a running microVM, and its disks grown (D214): `shards plug disk
//! MICROVM FILE:PATH[:ro]`, `shards plug volume MICROVM SOURCE:PATH[:ro]`, `shards unplug
//! disk|volume MICROVM PATH`, `shards grow disk MICROVM PATH SIZE`, and on Linux `shards
//! plug|unplug function MICROVM FUNCTION`.
//!
//! Each is asked of the run's VM first (`DEVICE`), which plugs the device into one of its
//! microVM's PCI slots and answers its place, then of its guest's init (`builtin::DEVICE`),
//! which finds it by a rescan and mounts it at PATH in the run's workload; let go of the
//! other way round. A disk is a file holding a file system, made empty and ext4 where
//! `--size` asks for one that is not there; a volume a host directory or a named volume,
//! served by a share process of its own. A host's function, bound to vfio-pci, is opened
//! here, the confined VM process opening no device, and its VFIO files handed to the VM,
//! which plugs it in behind its microVM's IOMMU; its guest's init gives it to vfio-pci,
//! makes its group's node in the workload's `/dev` and widens the workload's device
//! filter to it, as a microVM given it at its start has it (D44, D154). What is plugged
//! into a run is kept as long as it runs, by the path it is mounted at, or a function by
//! its host's name.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use shards_cmdline::flags::Parsed;
use shards_ipc::{device, kind};

use super::commands::Reply;
use super::{Daemon, RunState, Threads, lock};
use crate::containers::Disk;

/// The client a device command is asked for, and what its owner granted it (D228); none
/// for this machine's own user.
type Whom<'a> = Option<(&'a str, &'a crate::client_grants::Grants)>;

/// Where the store keeps volume `name`'s files, where it is one.
fn store_dir_of(home: &Path, name: &str) -> Option<PathBuf> {
    let store = crate::volumes::Store::new(home);
    store.get(name).map(|_| store.data(name))
}

/// What is plugged into a run's microVM, by the path its workload has it at.
#[derive(Debug, Clone)]
pub(super) struct Plugged {
    pub what: What,
    /// Its place on the microVM's PCI bus (`devfn`), and its guest's address of it.
    pub devfn: u8,
    pub address: String,
    /// A disk's file.
    pub file: Option<PathBuf>,
    /// A host's function's node in the workload and what it was had by.
    pub function: Option<Function>,
    /// The agent or harness a volume is plugged into (D235), by its member index; none for
    /// the microVM's command.
    pub member: Option<u32>,
}

/// A host's function plugged into a run (D214): what its user named it by (its host's
/// name, or its VFIO device node), its host IOMMU group's node, and the node its workload
/// has of it, with the device rule that allows that (`c MAJ:MIN rwm`); and, for one had
/// by its device node, the workload's `/dev/iommu`, which it is taken through too.
#[derive(Debug, Clone)]
pub(super) struct Function {
    pub given: String,
    #[cfg_attr(
        not(target_os = "linux"),
        allow(
            dead_code,
            reason = "read where a function is plugged, which Linux alone does (D214)"
        )
    )]
    pub group: PathBuf,
    pub node: String,
    pub rule: String,
    pub iommu: Option<Iommu>,
}

/// The workload's `/dev/iommu`, given it while a function had by its device node is
/// plugged into its run (D214): its device rule, and how init gives it back as the last
/// such function goes, as the workload had it before the first: none (`made`), or its
/// owner (`UID:GID`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Iommu {
    pub rule: String,
    pub before: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum What {
    Disk,
    Volume,
    Function,
}

/// Each running microVM's plugged devices, by microVM ID, then by path.
pub(super) type PluggedMap = HashMap<String, HashMap<String, Plugged>>;

/// How long its guest is given to find, mount or let go of a device.
const GUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `spec`, `SOURCE:PATH[:ro|:rw]`: its source, its path in the microVM, and whether it is
/// read-only. The path is the guest's, absolute; the source anything before it, a Windows
/// drive's path among them (`C:\data:/data`).
fn split_spec(spec: &str) -> Result<(String, String, bool), String> {
    let (rest, read_only) = match spec.rsplit_once(':') {
        Some((rest, "ro")) => (rest, true),
        Some((rest, "rw")) => (rest, false),
        _ => (spec, false),
    };
    let (source, path) = rest
        .rsplit_once(":/")
        .ok_or_else(|| format!("{spec:?}: SOURCE:PATH, PATH absolute in the microVM"))?;
    if source.is_empty() {
        return Err(format!("{spec:?}: no source"));
    }
    Ok((source.to_string(), format!("/{path}"), read_only))
}

/// Why `--for NAME` names no member to plug into (D235) or run a command in (D243), each
/// with the name given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NoMember {
    /// Its image declares no agent or harness.
    NoneDeclared(String),
    /// None of its agents or harnesses is named so.
    Unnamed(String),
    /// It is files alone: it runs no command of its own.
    Idle(String),
    /// Its index is past what a member's may be.
    Range(String),
}

impl std::fmt::Display for NoMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoMember::NoneDeclared(n) => write!(f, "--for {n}: its image has no agents or harnesses"),
            NoMember::Unnamed(n) => write!(f, "--for {n}: no agent or harness of its image is named so"),
            NoMember::Idle(n) => write!(f, "--for {n}: it runs no command of its own"),
            NoMember::Range(n) => write!(f, "--for {n}: past the members' indices"),
        }
    }
}

/// The member index of `name` among an image's agents and harnesses (`labels`, `agent
/// NAME` and `harness NAME` by index, agentfile.rs `managed_plan`), as an Agentfile's
/// `VOLUME … FOR` names one, where it is one of those that run (`active`).
fn member_named(labels: &[String], active: &[u32], name: &str) -> Result<u32, NoMember> {
    let index = labels
        .iter()
        .position(|l| {
            l.split_once(' ')
                .is_some_and(|(kind, n)| matches!(kind, "agent" | "harness") && n == name)
        })
        .ok_or_else(|| NoMember::Unnamed(name.into()))?;
    let index = u32::try_from(index).map_err(|_| NoMember::Range(name.into()))?;
    if !active.contains(&index) {
        return Err(NoMember::Idle(name.into()));
    }
    Ok(index)
}

/// A size as `--size` and `grow disk` take one: bytes, or with a unit of 1024's powers
/// (`k`, `m`, `g`, `t`, with or without `b`), the units `--memory` takes, so that a size
/// reads alike in every flag of a run (go-units v0.5.0 RAMInBytes, `ram_in_bytes`).
pub(super) fn bytes_of(size: &str) -> Result<u64, String> {
    let lower = size.trim().to_ascii_lowercase();
    let digits = lower.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = lower.get(digits.len()..).unwrap_or_default();
    let n: u64 = digits.parse().map_err(|_| format!("{size:?}: not a size"))?;
    let shift = match unit.trim_end_matches('b') {
        "" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        "t" => 40,
        _ => return Err(format!("{size:?}: not a size")),
    };
    n.checked_mul(1 << shift)
        .ok_or_else(|| format!("{size:?}: too large"))
}

/// An empty ext4 file system of `bytes`, made at `path`, which is not there: a plugged
/// disk's, as a run's writable layer is made (D190), sparse.
fn made_disk(path: &Path, bytes: u64) -> Result<(), String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let bytes = bytes / 4096 * 4096;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| at(&e))?;
    let made = (|| {
        shards_vmm::platform::sparse(&file).map_err(|e| at(&e))?;
        file.set_len(bytes).map_err(|e| at(&e))?;
        let mut ids = [0u8; 32];
        shards_vmm::platform::fill_random(&mut ids).map_err(|e| at(&e))?;
        let (mut uuid, mut hash_seed) = ([0u8; 16], [0u8; 16]);
        uuid.copy_from_slice(ids.get(..16).unwrap_or_default());
        hash_seed.copy_from_slice(ids.get(16..).unwrap_or_default());
        if let Some(version) = uuid.get_mut(6) {
            *version = (*version & 0x0F) | 0x40;
        }
        if let Some(variant) = uuid.get_mut(8) {
            *variant = (*variant & 0x3F) | 0x80;
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let options = shards_ext4::Options {
            bytes,
            uuid,
            hash_seed,
            time,
        };
        shards_ext4::format(&mut file, &options).map_err(|e| at(&e))?;
        file.sync_all().map_err(|e| at(&e))
    })();
    if made.is_err() {
        let _ = std::fs::remove_file(path);
    }
    made
}

impl<D: Disk> Daemon<D> {
    /// Serves `argv` where it is a device command (`plug`, `unplug`, `grow`), which a share
    /// process it starts needs `threads` for: its status; none where it is another.
    pub(super) fn device_command<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        argv: &[String],
        whose: &str,
        reply: &Reply<'_>,
    ) -> Option<u8> {
        use shards_cmdline::commands::{
            GROW_DISK, PLUG_DISK, PLUG_FUNCTION, PLUG_VOLUME, UNPLUG_DISK, UNPLUG_FUNCTION, UNPLUG_VOLUME,
        };
        use shards_cmdline::flags::{self, Outcome};
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let (command, path, named) = shards_cmdline::commands::find(&words)?;
        let ours = [
            &PLUG_DISK,
            &PLUG_VOLUME,
            &PLUG_FUNCTION,
            &UNPLUG_DISK,
            &UNPLUG_VOLUME,
            &UNPLUG_FUNCTION,
            &GROW_DISK,
        ];
        if !ours.iter().any(|c| std::ptr::eq(command, *c)) {
            return None;
        }
        let parsed = match flags::parse(
            command,
            path,
            argv.get(named..).unwrap_or_default(),
            &flags::value,
        ) {
            Outcome::Run(parsed) => parsed,
            Outcome::Help { .. } => {
                reply.out(&flags::help(command, path, 80));
                return Some(0);
            }
            Outcome::Fail { text, status, .. } => {
                reply.err(&text);
                return Some(status);
            }
        };
        self.settle();
        // A client's, through its context: what it plugs in of this machine is what its
        // owner granted it (D228).
        let grants = (!whose.is_empty()).then(|| crate::client_grants::load(&self.home, whose));
        let whom = grants.as_ref().map(|g| (whose, g));
        Some(if std::ptr::eq(command, &PLUG_DISK) {
            self.plug(threads, What::Disk, &parsed, whom, reply)
        } else if std::ptr::eq(command, &PLUG_VOLUME) {
            self.plug(threads, What::Volume, &parsed, whom, reply)
        } else if std::ptr::eq(command, &PLUG_FUNCTION) {
            self.plug(threads, What::Function, &parsed, whom, reply)
        } else if std::ptr::eq(command, &UNPLUG_DISK) {
            self.unplug(What::Disk, &parsed, reply)
        } else if std::ptr::eq(command, &UNPLUG_VOLUME) {
            self.unplug(What::Volume, &parsed, reply)
        } else if std::ptr::eq(command, &UNPLUG_FUNCTION) {
            self.unplug(What::Function, &parsed, reply)
        } else {
            self.grow(&parsed, reply)
        })
    }

    /// Asks the VM of running microVM `id` device step `step` (`DEVICE`), with `fds`,
    /// and waits for its answer (`DEVICED`), as a pause is.
    fn ask_device(
        &self,
        id: &str,
        step: &[u8],
        fds: &[shards_ipc::fd::BorrowedFd<'_>],
    ) -> Result<String, String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (inbox, socket) = match lock(&self.runs).get(id) {
            Some(RunState::Tracked(t)) if t.joined.is_none() => (t.inbox.clone(), t.socket.clone()),
            Some(RunState::Tracked(_)) => {
                return Err("it runs in another's microVM: plug into that one".into());
            }
            _ => return Err("it is not running".into()),
        };
        let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (told, answer) = mpsc::channel();
        {
            let mut inbox = lock(&inbox);
            if inbox.ended {
                return Err("it is not running".into());
            }
            inbox.devices.push((number, told));
        }
        let ask = [&number.to_be_bytes()[..], step].concat();
        if let Err(e) = socket.send(kind::DEVICE, &ask, fds) {
            lock(&inbox).devices.retain(|(n, _)| *n != number);
            return Err(format!("asking its microVM: {e}"));
        }
        answer
            .recv()
            .unwrap_or_else(|_| Err("its microVM stopped".into()))
            .map_err(|e| format!("its microVM: {e}"))
    }

    /// Runs init's DEVICE built-in with `args` in running microVM `id`'s guest: its
    /// answer, or why it failed.
    fn guest_device(&self, id: &str, args: &[&str]) -> Result<String, String> {
        let spec = shards_abi::run::Spec {
            builtin: shards_abi::run::builtin::DEVICE,
            argv: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
            ..Default::default()
        };
        let quiet = self
            .exec_quietly(id, &spec, None, 1 << 12, GUEST_TIMEOUT)
            .map_err(|e| format!("its guest: {e}"))?;
        let said = String::from_utf8_lossy(&quiet.output).trim().to_string();
        match quiet.status {
            Some(0) => Ok(said),
            _ => Err(format!("its guest: {said}")),
        }
    }

    /// The member index of `name`, an agent or harness of running microVM `id`'s image that
    /// runs (D235, D243).
    pub(super) fn running_member(&self, id: &str, name: &str) -> Result<u32, NoMember> {
        let runs = lock(&self.runs);
        let agentfile = match runs.get(id) {
            Some(RunState::Tracked(run)) => run.base.agentfile.as_ref(),
            _ => None,
        };
        let a = agentfile.ok_or_else(|| NoMember::NoneDeclared(name.into()))?;
        member_named(&a.managed_labels, &a.active_members, name)
    }

    /// Container `given`, running, with its microVM's own run: its ID.
    fn running_own(&self, given: &str) -> Result<String, String> {
        let id = self
            .resolve(given)
            .map_err(|e| e.strip_prefix("shards: ").unwrap_or(&e).to_string())?;
        if !self.running(&id) || self.is_restarting(&id) {
            return Err(format!("{given} is not running"));
        }
        if lock(&self.paused).contains(&id) {
            return Err(format!("{given} is paused: unpause it first"));
        }
        // What was plugged into runs that are gone, gone with them.
        lock(&self.plugged).retain(|run, _| self.running(run));
        Ok(id)
    }

    /// `shards plug disk MICROVM FILE:PATH[:ro]` and `shards plug volume MICROVM
    /// SOURCE:PATH[:ro]` (D214): the device plugged into the microVM, its guest's mount of
    /// it at PATH; said as PATH and the device its guest has.
    pub(super) fn plug<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        what: What,
        parsed: &Parsed,
        whom: Whom<'_>,
        reply: &Reply<'_>,
    ) -> u8 {
        let (Some(given), Some(spec)) = (parsed.args.first(), parsed.args.get(1)) else {
            reply.err(match what {
                What::Function => "shards: plug function takes a microVM and a host's PCI function",
                _ => "shards: plug takes a microVM and SOURCE:PATH",
            });
            return 1;
        };
        let done = self.running_own(given).and_then(|id| {
            match what {
                What::Function => {
                    if let Some((client, grants)) = whom {
                        crate::client_grants::refuse_function(spec, client, grants)?;
                    }
                    self.plug_function(&id, spec)
                }
                _ => self.plug_one(
                    threads,
                    &id,
                    what,
                    spec,
                    (parsed.string("size"), parsed.string("for")),
                    whom,
                ),
            }
            .map(|said| (id, said))
        });
        match done {
            Ok((id, said)) => {
                self.container_event(&id, "plug", &[("path", said.clone())]);
                reply.out(&said);
                0
            }
            Err(e) => {
                reply.err(&format!("shards: cannot plug into {given}: {e}"));
                1
            }
        }
    }

    fn plug_one<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        what: What,
        spec: &str,
        (size, member): (&str, &str),
        whom: Whom<'_>,
    ) -> Result<String, String> {
        let (source, path, read_only) = split_spec(spec)?;
        if lock(&self.plugged).get(id).is_some_and(|p| p.contains_key(&path)) {
            return Err(format!("{path} has a device plugged in already"));
        }
        // An agent or harness of its image (D235), named before anything is plugged in.
        let member = match member {
            "" => None,
            name => Some(self.running_member(id, name).map_err(|e| e.to_string())?),
        };
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let name = format!(
            "shards-plug{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let flag = if read_only { "ro" } else { "rw" };
        // A share's notices of what its host changes (D235), told its guest once mounted.
        let mut notices = None;
        let (answer, file) = match what {
            What::Disk => {
                let file = PathBuf::from(&source);
                if !file.is_absolute() {
                    return Err(format!("{source}: a disk's file is given by its absolute path"));
                }
                // A client's: a file of this machine's only in a directory its owner granted
                // it, judged before it is made and again as it is open (D228).
                let shown = format!("plug disk {source}");
                if let Some((client, grants)) = whom {
                    crate::client_grants::refuse_path(client, grants, &shown, &file, !read_only)?;
                }
                if !size.is_empty() && !file.exists() {
                    made_disk(&file, bytes_of(size)?)?;
                }
                let opened = std::fs::OpenOptions::new()
                    .read(true)
                    .write(!read_only)
                    .open(&file)
                    .map_err(|e| format!("{source}: {e}"))?;
                if let Some((client, grants)) = whom {
                    crate::client_grants::refuse_opened(client, grants, &shown, &opened, !read_only)?;
                }
                let flags = if read_only { device::READ_ONLY } else { 0 };
                let step = [&[device::DISK, flags][..], name.as_bytes()].concat();
                use shards_ipc::fd::AsFd as _;
                (self.ask_device(id, &step, &[opened.as_fd()])?, Some(file))
            }
            What::Volume => {
                let (answer, told) = self.plug_share(threads, id, &source, read_only, &name, whom)?;
                notices = told;
                (answer, None)
            }
            What::Function => return Err("a function is plugged in by plug function".into()),
        };
        let (devfn, address) = answer
            .split_once(' ')
            .and_then(|(d, a)| Some((d.parse::<u8>().ok()?, a.to_string())))
            .ok_or_else(|| format!("its microVM answered {answer:?}"))?;
        let index = member.map(|m| m.to_string());
        let into: Vec<&str> = index.iter().flat_map(|m| ["member", m.as_str()]).collect();
        let mounted = match what {
            What::Volume => self.guest_device(
                id,
                &[&["share", &address, &name, &path, flag][..], &into].concat(),
            ),
            _ => self.guest_device(id, &["disk", &address, &path, flag]),
        };
        let device = match mounted {
            Ok(device) => device,
            Err(e) => {
                // Taken off again, its guest having found it or not.
                let _ = self.guest_device(id, &[&["unmount", &address, &path][..], &into].concat());
                let _ = self.ask_device(id, &[device::UNPLUG, devfn], &[]);
                return Err(e);
            }
        };
        if let Some(pipe) = notices
            && let Err(e) = self.notice_into(id, pipe)
        {
            super::log(format!(
                "container {id}: its guest is told none of {path}'s changes: {e}"
            ));
        }
        lock(&self.plugged).entry(id.to_string()).or_default().insert(
            path.clone(),
            Plugged {
                what,
                devfn,
                address: address.clone(),
                file,
                function: None,
                member,
            },
        );
        Ok(if device.is_empty() {
            format!("{path} {address}")
        } else {
            format!("{path} /dev/{device} {address}")
        })
    }

    /// A share of `source`, a host's absolute directory or a named volume, served by a
    /// share process of its own (share.rs), its stream given the VM of running microVM
    /// `id` as a device of `tag`, as a run's are (D207): the VM's answer, and the pipe its
    /// share process tells what its host changes on (D235), none for a client's. On Unix
    /// its connection, which the share process sends and waits to be taken; on Windows the
    /// stream itself, a duplex pipe made here whose server end the share process was given
    /// as it started (D197).
    fn plug_share<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        source: &str,
        read_only: bool,
        tag: &str,
        whom: Whom<'_>,
    ) -> Result<(String, Option<shards_ipc::fd::OwnedFd>), String> {
        use shards_ipc::fd::AsFd as _;
        // A client's directory is its own, served from it (D228): the VM is given the stream
        // its client serves it on, as a run's share of it is, and no share process serves it
        // here. A path of this machine's a client names is the client's, never this one's.
        let client = whom.map_or("", |(client, _)| client);
        if Path::new(source).is_absolute() || source.starts_with(crate::client_grants::RELAYED) {
            crate::client_grants::refuse_bind(client, source)?;
        }
        if let Some(relay) = crate::client_grants::relay_of(source) {
            if relay.file {
                return Err(format!("{}: a file is no volume: plug its directory", relay.here));
            }
            let end = super::remote::take_relay(client, &relay.token)?;
            let step = [&[device::SHARE][..], tag.as_bytes()].concat();
            // Its share is the client's to make read-only, its server told so; the guest
            // mounts it as asked too.
            let _ = read_only;
            return Ok((self.ask_device(id, &step, &[end.as_fd()])?, None));
        }
        let dir = if Path::new(source).is_absolute() {
            PathBuf::from(source)
        } else {
            let store = crate::volumes::Store::new(&self.home);
            store
                .get(source)
                .ok_or_else(|| format!("no volume {source}: a volume is made first, or a host directory given by its absolute path"))?;
            store.data(source)
        };
        // A directory opens on Windows only with backup semantics (`platform::open_dir`).
        let opened = shards_vmm::platform::open_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !opened.metadata().map_err(|e| e.to_string())?.is_dir() {
            return Err(format!("{}: not a directory", dir.display()));
        }
        // A volume a client plugs in is the store's, or a directory of this machine's its
        // owner granted it, judged as it is open (D228).
        if let Some((client, grants)) = whom
            && store_dir_of(&self.home, source) != Some(dir.clone())
        {
            crate::client_grants::refuse_opened(
                client,
                grants,
                &format!("volume {source}"),
                &opened,
                !read_only,
            )?;
        }
        let dirs = [(
            shards_ipc::fd::OwnedFd::from(opened),
            read_only,
            std::ffi::OsString::new(),
        )];
        let parent = self.parent_of(id).unwrap_or_default();
        let tags = [tag.to_string()];
        let (ends, child, notices) = self.started_shares(threads, &dirs, &parent, false, Some(&tags))?;
        // On Windows none where the share is a client's; this one is the machine's.
        #[cfg(unix)]
        let child = Some(child);
        lock(&self.shared_by)
            .entry(id.to_string())
            .or_default()
            .extend(child);
        let step = [&[device::SHARE][..], tag.as_bytes()].concat();
        #[cfg(unix)]
        {
            let link = ends;
            let ends = shards_ipc::recv(&link)
                .map_err(|e| format!("its share process: {e}"))?
                .filter(|m| m.kind == kind::SHARE_ENDS)
                .ok_or("its share process sent no connection")?;
            let conn = ends.fds.first().ok_or("its share process sent no connection")?;
            let answer = self.ask_device(id, &step, &[conn.as_fd()]);
            let _ = shards_ipc::send(&link, kind::TAKEN, &[], &[]);
            Ok((answer?, notices))
        }
        #[cfg(windows)]
        {
            let stream = ends.first().ok_or("no stream was made for its share")?;
            Ok((self.ask_device(id, &step, &[stream.as_fd()])?, notices))
        }
    }

    /// `shards unplug disk|volume MICROVM PATH` (D214): its guest's mount at PATH
    /// unmounted, refused while anything there is in use, the device let go and taken off
    /// its microVM's bus.
    pub(super) fn unplug(&self, what: What, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let (Some(given), Some(path)) = (parsed.args.first(), parsed.args.get(1)) else {
            reply.err(match what {
                What::Function => {
                    "shards: unplug function takes a microVM and a host's PCI function plugged into it"
                }
                _ => "shards: unplug takes a microVM and the path a device is plugged in at",
            });
            return 1;
        };
        let done = self.running_own(given).and_then(|id| {
            if what == What::Function {
                return self.unplug_function(&id, path);
            }
            let plugged = lock(&self.plugged)
                .get(&id)
                .and_then(|p| p.get(path.as_str()))
                .filter(|p| p.what == what)
                .cloned()
                .ok_or_else(|| format!("no {} is plugged in at {path}", noun(what)))?;
            let index = plugged.member.map(|m| m.to_string());
            let from: Vec<&str> = index.iter().flat_map(|m| ["member", m.as_str()]).collect();
            self.guest_device(
                &id,
                &[&["unmount", plugged.address.as_str(), path][..], &from].concat(),
            )?;
            self.ask_device(&id, &[device::UNPLUG, plugged.devfn], &[])?;
            if let Some(p) = lock(&self.plugged).get_mut(&id) {
                p.remove(path.as_str());
            }
            self.container_event(&id, "unplug", &[("path", path.clone())]);
            Ok(())
        });
        match done {
            Ok(()) => {
                reply.out(path);
                0
            }
            Err(e) => {
                reply.err(&format!("shards: cannot unplug from {given}: {e}"));
                1
            }
        }
    }

    /// `shards grow disk MICROVM PATH SIZE` (D214): its file grown to SIZE, its driver told,
    /// and its ext4 file system grown online to fill it; said as its size in bytes.
    pub(super) fn grow(&self, parsed: &Parsed, reply: &Reply<'_>) -> u8 {
        let (Some(given), Some(path), Some(size)) =
            (parsed.args.first(), parsed.args.get(1), parsed.args.get(2))
        else {
            reply.err("shards: grow disk takes a microVM, the path its disk is plugged in at, and a size");
            return 1;
        };
        let done = self.running_own(given).and_then(|id| {
            let bytes = bytes_of(size)?;
            let plugged = lock(&self.plugged)
                .get(&id)
                .and_then(|p| p.get(path.as_str()))
                .filter(|p| p.what == What::Disk)
                .cloned()
                .ok_or_else(|| format!("no disk is plugged in at {path}"))?;
            let file = plugged.file.ok_or("its file is not known")?;
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .open(&file)
                .map_err(|e| format!("{}: {e}", file.display()))?;
            let now = opened.metadata().map_err(|e| e.to_string())?.len();
            let bytes = bytes / 512 * 512;
            if bytes < now {
                return Err(format!(
                    "{size}: less than its {now} bytes, and a disk only grows"
                ));
            }
            opened
                .set_len(bytes)
                .map_err(|e| format!("{}: {e}", file.display()))?;
            let grown = self.ask_device(&id, &[device::GROW, plugged.devfn], &[])?;
            let sectors = grown
                .parse::<u64>()
                .map_err(|_| format!("its microVM answered {grown:?}"))?
                / 512;
            self.guest_device(&id, &["grow", &plugged.address, path, &sectors.to_string()])?;
            self.container_event(&id, "grow", &[("path", path.clone())]);
            Ok(grown)
        });
        match done {
            Ok(grown) => {
                reply.out(&grown);
                0
            }
            Err(e) => {
                reply.err(&format!("shards: cannot grow a disk of {given}: {e}"));
                1
            }
        }
    }
}

fn noun(what: What) -> &'static str {
    match what {
        What::Disk => "disk",
        What::Volume => "volume",
        What::Function => "function",
    }
}

/// The device rules of the functions plugged into a run (D214), `plugged`, but the one
/// plugged in as `but`: what its workload's device filter allows past its run's own.
fn function_rules(plugged: Option<&HashMap<String, Plugged>>, but: &str) -> Vec<String> {
    let mut functions: Vec<(&String, &Function)> = plugged
        .into_iter()
        .flatten()
        .filter(|(name, _)| name.as_str() != but)
        .filter_map(|(name, p)| Some((name, p.function.as_ref()?)))
        .collect();
    functions.sort_by(|a, b| a.0.cmp(b.0));
    let mut rules = Vec::new();
    for (_, f) in functions {
        allowing(&mut rules, &f.rule);
        if let Some(iommu) = &f.iommu {
            allowing(&mut rules, &iommu.rule);
        }
    }
    rules
}

/// `rule` added to `rules` where it is not among them: `/dev/iommu`'s once, however many
/// functions are had through it.
fn allowing(rules: &mut Vec<String>, rule: &str) {
    if !rules.iter().any(|r| r == rule) {
        rules.push(rule.to_string());
    }
}

/// The workload's `/dev/iommu` as a function plugged into its run already gives it, but
/// `but`'s: the one whose `before` says what the workload had before any.
fn iommu_given(plugged: Option<&HashMap<String, Plugged>>, but: &str) -> Option<Iommu> {
    plugged?
        .iter()
        .filter(|(name, _)| name.as_str() != but)
        .find_map(|(_, p)| p.function.as_ref()?.iommu.clone())
}

/// What init's `function` step answers, `PATH c MAJ:MIN`, then `/dev/iommu c MAJ:MIN
/// BEFORE` for a function by its device node: the node and its device rule, and the
/// workload's `/dev/iommu` with what it was before.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "init answers it where a function is plugged, which Linux alone does (D214)"
    )
)]
fn node_of(said: &str) -> Option<(String, String, Option<Iommu>)> {
    let rule = |kind: &str, numbers: &str| {
        let (major, minor) = numbers.split_once(':')?;
        (kind == "c" && major.parse::<u32>().is_ok() && minor.parse::<u32>().is_ok())
            .then(|| format!("c {major}:{minor} rwm"))
    };
    let owner = |before: &str| {
        before == "made"
            || before
                .split_once(':')
                .is_some_and(|(u, g)| u.parse::<u32>().is_ok() && g.parse::<u32>().is_ok())
    };
    match said.split(' ').collect::<Vec<_>>().as_slice() {
        [path, kind, numbers] if path.starts_with("/dev/vfio/") => {
            Some((path.to_string(), rule(kind, numbers)?, None))
        }
        [
            path,
            kind,
            numbers,
            "/dev/iommu",
            iommu_kind,
            iommu_numbers,
            before,
        ] if path.starts_with("/dev/vfio/devices/") && owner(before) => {
            let iommu = Iommu {
                rule: rule(iommu_kind, iommu_numbers)?,
                before: before.to_string(),
            };
            Some((path.to_string(), rule(kind, numbers)?, Some(iommu)))
        }
        _ => None,
    }
}

/// Init's `release` step of the function at `address`, its node `node` in the workload,
/// and `/dev/iommu` given back as `iommu` says it was, where the last function had through
/// it goes.
fn release<'a>(address: &'a str, node: &'a str, iommu: Option<&'a Iommu>) -> Vec<&'a str> {
    let mut step = vec!["release", address, node];
    if let Some(i) = iommu {
        step.extend(["iommu", i.before.as_str()]);
    }
    step
}

impl<D: Disk> Daemon<D> {
    /// `shards plug function MICROVM FUNCTION` (D214, Linux): the host's function FUNCTION,
    /// by its name (`0000:01:00.0`) or its VFIO device node (`/dev/vfio/devices/vfioN`),
    /// bound to vfio-pci, plugged into running microVM `id` behind its IOMMU,
    /// which a microVM booted to take functions has its slots behind ([`crate::run::
    /// takes_functions`]); its guest's vfio-pci given it, its node made in the workload's
    /// `/dev` and allowed by its device filter. Refused, as a run's start refuses one
    /// (`run::vfio_groups`), where its group has no IOMMU, whose DMA reaches all of the
    /// host's memory. Said as FUNCTION, the node its workload has, and its guest's address.
    #[cfg(target_os = "linux")]
    fn plug_function(&self, id: &str, given: &str) -> Result<String, String> {
        use shards_ipc::fd::AsFd as _;
        let by_node = given.starts_with("/dev/vfio/devices/");
        let f = match by_node {
            true => shards_vmm::vm::Vfio::of_cdev(Path::new(given))?,
            false => shards_vmm::vm::Vfio::of(given)?,
        };
        if f.noiommu {
            return Err(format!(
                "{given}: its group {} has no IOMMU, whose DMA reaches all of the host's memory: a microVM is given none",
                f.group.display()
            ));
        }
        let driver = std::fs::read_link(format!("/sys/bus/pci/devices/{}/driver", f.name)).ok();
        let bound = driver
            .as_ref()
            .and_then(|d| d.file_name())
            .and_then(|d| d.to_str())
            .is_some_and(|d| d.ends_with("vfio-pci") || d.ends_with("vfio_pci"));
        if !bound {
            return Err(format!(
                "{given}: not bound to vfio-pci, which its host gives a function a process may take (vfio.rst)"
            ));
        }
        if let Some(plugged) = lock(&self.plugged).get(id) {
            for (name, p) in plugged {
                let Some(other) = &p.function else {
                    continue;
                };
                if *name == f.name {
                    return Err(format!("{given} is plugged into it already"));
                }
                if other.group == f.group {
                    return Err(format!(
                        "{given}: its IOMMU group {} is plugged into it already, through {name}: a microVM takes a group whole, as its guest does",
                        f.group.display()
                    ));
                }
            }
        }
        let open = |path: &Path| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|e| {
                    // VFIO opens a group for one process at a time (vfio_group_fops_open).
                    let hint = match e.raw_os_error() {
                        Some(libc::EBUSY) => {
                            ": another process has it, as VFIO gives a group to one at a time"
                        }
                        _ => "",
                    };
                    format!("{}: {e}{hint}", path.display())
                })
        };
        let (node, [first, second]) = match &f.cdev {
            Some(cdev) => (cdev.clone(), [open(Path::new("/dev/iommu"))?, open(cdev)?]),
            None => (
                f.group.clone(),
                [open(Path::new("/dev/vfio/vfio"))?, open(&f.group)?],
            ),
        };
        let flags = if by_node { device::BY_NODE } else { 0 };
        let step = [
            &[device::FUNCTION, flags][..],
            f.name.as_bytes(),
            &[0],
            node.as_os_str().as_encoded_bytes(),
        ]
        .concat();
        let answer = self.ask_device(id, &step, &[first.as_fd(), second.as_fd()])?;
        // The VM has them now; the host's function is its while it runs.
        drop((first, second));
        let (devfn, address) = answer
            .split_once(' ')
            .and_then(|(d, a)| Some((d.parse::<u8>().ok()?, a.to_string())))
            .ok_or_else(|| format!("its microVM answered {answer:?}"))?;
        let kind = if by_node { "node" } else { "group" };
        let given_up = |e: String| {
            let _ = self.guest_device(id, &["release", &address]);
            let _ = self.ask_device(id, &[device::UNPLUG, devfn], &[]);
            e
        };
        let found = self
            .guest_device(id, &["function", &address, kind, given])
            .map_err(given_up)?;
        let (workload_node, rule, iommu) =
            node_of(&found).ok_or_else(|| given_up(format!("its guest answered {found:?}")))?;
        // The workload's /dev/iommu as it was before it was given it, which the first
        // function had through it says: init finds it given for a second.
        let already = iommu_given(lock(&self.plugged).get(id), &f.name);
        let iommu = iommu.map(|i| match &already {
            Some(first) => Iommu {
                rule: i.rule,
                before: first.before.clone(),
            },
            None => i,
        });
        let mut rules = function_rules(lock(&self.plugged).get(id), &f.name);
        allowing(&mut rules, &rule);
        if let Some(i) = &iommu {
            allowing(&mut rules, &i.rule);
        }
        let mut allow = vec!["allow"];
        allow.extend(rules.iter().map(String::as_str));
        if let Err(e) = self.guest_device(id, &allow) {
            let back = iommu.as_ref().filter(|_| already.is_none());
            let _ = self.guest_device(id, &release(&address, &workload_node, back));
            let _ = self.ask_device(id, &[device::UNPLUG, devfn], &[]);
            return Err(e);
        }
        lock(&self.plugged).entry(id.to_string()).or_default().insert(
            f.name.clone(),
            Plugged {
                what: What::Function,
                devfn,
                address: address.clone(),
                file: None,
                function: Some(Function {
                    given: given.to_string(),
                    group: f.group.clone(),
                    node: workload_node.clone(),
                    rule,
                    iommu,
                }),
                member: None,
            },
        );
        Ok(format!("{given} {workload_node} {address}"))
    }

    #[cfg(not(target_os = "linux"))]
    fn plug_function(&self, _id: &str, given: &str) -> Result<String, String> {
        Err(format!("{given}: {}", shards_vmm::vm::passthrough_refused()))
    }

    /// `shards unplug function MICROVM FUNCTION` (D214): its node taken from the
    /// workload's device filter first, so that nothing opens it anew, then refused,
    /// the filter as it was, while anything in the microVM holds it or its VFIO device
    /// open; else its node taken out of the workload's `/dev`, its guest's function let go
    /// of, and it is taken off the microVM's bus, the host's again.
    fn unplug_function(&self, id: &str, given: &str) -> Result<(), String> {
        let (name, plugged) = lock(&self.plugged)
            .get(id)
            .and_then(|p| {
                p.iter().find(|(name, p)| {
                    p.what == What::Function
                        && (name.as_str() == given || p.function.as_ref().is_some_and(|f| f.given == given))
                })
            })
            .map(|(name, p)| (name.clone(), p.clone()))
            .ok_or_else(|| format!("no function {given} is plugged into it"))?;
        let function = plugged.function.as_ref().ok_or("its node is not known")?;
        let rest = function_rules(lock(&self.plugged).get(id), &name);
        // The workload's /dev/iommu given back as it was, where no other function plugged
        // is had through it.
        let others = iommu_given(lock(&self.plugged).get(id), &name);
        let back = function.iommu.as_ref().filter(|_| others.is_none());
        let allow = |rules: &[String]| {
            let mut args = vec!["allow"];
            args.extend(rules.iter().map(String::as_str));
            self.guest_device(id, &args)
        };
        allow(&rest)?;
        if let Err(e) = self.guest_device(id, &release(&plugged.address, &function.node, back)) {
            let mut all = rest.clone();
            allowing(&mut all, &function.rule);
            if let Some(i) = &function.iommu {
                allowing(&mut all, &i.rule);
            }
            let _ = allow(&all);
            return Err(e);
        }
        self.ask_device(id, &[device::UNPLUG, plugged.devfn], &[])?;
        if let Some(p) = lock(&self.plugged).get_mut(id) {
            p.remove(&name);
        }
        self.container_event(id, "unplug", &[("path", given.to_string())]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--for` names an agent or harness as an Agentfile's `FOR` does, by its name alone,
    /// and finds its member index, agents first; one of no such name, or one that runs no
    /// command, is refused.
    #[test]
    fn a_member_is_found_by_its_name() {
        let labels: Vec<String> = ["agent main", "agent tools", "harness claude"]
            .iter()
            .map(|l| l.to_string())
            .collect();
        let active = [0, 2];
        assert_eq!(member_named(&labels, &active, "main"), Ok(0));
        assert_eq!(member_named(&labels, &active, "claude"), Ok(2));
        assert_eq!(
            member_named(&labels, &active, "tools"),
            Err(NoMember::Idle("tools".into()))
        );
        assert_eq!(
            member_named(&labels, &active, "codex"),
            Err(NoMember::Unnamed("codex".into()))
        );
        assert_eq!(
            member_named(&labels, &active, "agent main"),
            Err(NoMember::Unnamed("agent main".into()))
        );
        assert!(
            NoMember::Idle("tools".into())
                .to_string()
                .contains("runs no command")
        );
    }

    /// A spec's source is all before its absolute path, a drive's colon among it; its mode
    /// last.
    #[test]
    fn a_spec_is_split_at_its_path() {
        assert_eq!(
            split_spec("/a/b.img:/data"),
            Ok(("/a/b.img".into(), "/data".into(), false))
        );
        assert_eq!(
            split_spec("/a:/data/x:ro"),
            Ok(("/a".into(), "/data/x".into(), true))
        );
        assert_eq!(
            split_spec(r"C:\d\b.img:/data:rw"),
            Ok((r"C:\d\b.img".into(), "/data".into(), false))
        );
        assert_eq!(split_spec("vol:/v"), Ok(("vol".into(), "/v".into(), false)));
        assert!(split_spec("/a/b.img").is_err());
        assert!(split_spec(":/data").is_err());
    }

    /// Init's answer of a function's node reads as its path and its rule, a VFIO node's
    /// alone, and, after a device node, the workload's `/dev/iommu`'s rule and what it was
    /// before; the rules a run's filter keeps are every other function's, in order,
    /// `/dev/iommu`'s once; it is given back by the last function had through it alone.
    #[test]
    fn a_functions_node_and_rules_are_read() {
        assert_eq!(
            node_of("/dev/vfio/0 c 241:0"),
            Some(("/dev/vfio/0".into(), "c 241:0 rwm".into(), None))
        );
        assert_eq!(
            node_of("/dev/vfio/devices/vfio0 c 238:0"),
            Some(("/dev/vfio/devices/vfio0".into(), "c 238:0 rwm".into(), None))
        );
        let iommu = |rule: &str, before: &str| Iommu {
            rule: rule.into(),
            before: before.into(),
        };
        assert_eq!(
            node_of("/dev/vfio/devices/vfio0 c 238:0 /dev/iommu c 10:123 made"),
            Some((
                "/dev/vfio/devices/vfio0".into(),
                "c 238:0 rwm".into(),
                Some(iommu("c 10:123 rwm", "made"))
            ))
        );
        assert_eq!(
            node_of("/dev/vfio/devices/vfio0 c 238:0 /dev/iommu c 10:123 0:0").and_then(|n| n.2),
            Some(iommu("c 10:123 rwm", "0:0"))
        );
        for wrong in [
            "/dev/sda b 8:0",
            "/dev/vfio/0 c 241",
            "/dev/vfio/0 c x:0",
            "/dev/null c 1:3",
            "/dev/vfio/0 c 241:0 /dev/iommu c 10:123 made",
            "/dev/vfio/devices/vfio0 c 238:0 /dev/iommu c 10:123",
            "/dev/vfio/devices/vfio0 c 238:0 /dev/iommu c 10:123 root",
            "/dev/vfio/devices/vfio0 c 238:0 /dev/iommu b 10:123 made",
            "/dev/vfio/devices/vfio0 c 238:0 /dev/null c 1:3 made",
            "",
        ] {
            assert_eq!(node_of(wrong), None, "{wrong}");
        }
        let by_node = |name: &str, rule: &str, before: &str| {
            (
                name.to_string(),
                Plugged {
                    what: What::Function,
                    devfn: 0,
                    address: String::new(),
                    file: None,
                    function: Some(Function {
                        given: name.into(),
                        group: PathBuf::new(),
                        node: String::new(),
                        rule: rule.into(),
                        iommu: Some(iommu("c 10:123 rwm", before)),
                    }),
                    member: None,
                },
            )
        };
        let two: HashMap<String, Plugged> = [
            by_node("0000:01:00.0", "c 238:0 rwm", "made"),
            by_node("0000:02:00.0", "c 238:1 rwm", "made"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            function_rules(Some(&two), ""),
            ["c 238:0 rwm", "c 10:123 rwm", "c 238:1 rwm"]
        );
        assert_eq!(
            function_rules(Some(&two), "0000:01:00.0"),
            ["c 238:1 rwm", "c 10:123 rwm"]
        );
        assert!(iommu_given(Some(&two), "0000:01:00.0").is_some(), "another's");
        let one: HashMap<String, Plugged> = [by_node("0000:01:00.0", "c 238:0 rwm", "0:0")]
            .into_iter()
            .collect();
        assert_eq!(iommu_given(Some(&one), "0000:01:00.0"), None, "its own alone");
        assert_eq!(
            release("0000:00:01.0", "/dev/vfio/devices/vfio0", None),
            ["release", "0000:00:01.0", "/dev/vfio/devices/vfio0"]
        );
        assert_eq!(
            release(
                "0000:00:01.0",
                "/dev/vfio/devices/vfio0",
                Some(&iommu("c 10:123 rwm", "0:0"))
            ),
            [
                "release",
                "0000:00:01.0",
                "/dev/vfio/devices/vfio0",
                "iommu",
                "0:0"
            ]
        );
        let function = |name: &str, rule: &str| {
            (
                name.to_string(),
                Plugged {
                    what: What::Function,
                    devfn: 0,
                    address: String::new(),
                    file: None,
                    function: Some(Function {
                        given: name.into(),
                        group: PathBuf::new(),
                        node: String::new(),
                        rule: rule.into(),
                        iommu: None,
                    }),
                    member: None,
                },
            )
        };
        let mut plugged: HashMap<String, Plugged> = [
            function("0000:02:00.0", "c 241:1 rwm"),
            function("0000:01:00.0", "c 241:0 rwm"),
        ]
        .into_iter()
        .collect();
        plugged.insert(
            "/data".into(),
            Plugged {
                what: What::Disk,
                devfn: 0,
                address: String::new(),
                file: None,
                function: None,
                member: None,
            },
        );
        assert_eq!(function_rules(Some(&plugged), ""), ["c 241:0 rwm", "c 241:1 rwm"]);
        assert_eq!(function_rules(Some(&plugged), "0000:01:00.0"), ["c 241:1 rwm"]);
        assert!(function_rules(None, "").is_empty());
    }

    /// Sizes in bytes or 1024's powers.
    #[test]
    fn sizes_read_in_bytes_or_binary_units() {
        assert_eq!(bytes_of("512"), Ok(512));
        assert_eq!(bytes_of("64m"), Ok(64 << 20));
        assert_eq!(bytes_of("1GB"), Ok(1 << 30));
        assert!(bytes_of("1x").is_err());
        assert!(bytes_of("m").is_err());
    }
}

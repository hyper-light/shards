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
//! filter to it, as a container given it at its start has it (D44, D154). What is plugged
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
}

/// A host's function plugged into a run (D214): what its user named it by (its host's
/// name, or its VFIO device node), its host IOMMU group's node, and the node its workload
/// has of it, with the device rule that allows that (`c MAJ:MIN rwm`).
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum What {
    Disk,
    Volume,
    Function,
}

/// Each running microVM's plugged devices, by container ID, then by path.
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

/// A size as `--size` and `grow disk` take one: bytes, or with a unit of 1024's powers
/// (`k`, `m`, `g`, `t`, with or without `b`), as Docker's sizes are read (go-units
/// RAMInBytes).
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
        Some(if std::ptr::eq(command, &PLUG_DISK) {
            self.plug(threads, What::Disk, &parsed, reply)
        } else if std::ptr::eq(command, &PLUG_VOLUME) {
            self.plug(threads, What::Volume, &parsed, reply)
        } else if std::ptr::eq(command, &PLUG_FUNCTION) {
            self.plug(threads, What::Function, &parsed, reply)
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

    /// Asks the VM of running container `id` device step `step` (`DEVICE`), with `fds`,
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

    /// Runs init's DEVICE built-in with `args` in running container `id`'s guest: its
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
                What::Function => self.plug_function(&id, spec),
                _ => self.plug_one(threads, &id, what, spec, parsed.string("size")),
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
        size: &str,
    ) -> Result<String, String> {
        let (source, path, read_only) = split_spec(spec)?;
        if lock(&self.plugged).get(id).is_some_and(|p| p.contains_key(&path)) {
            return Err(format!("{path} has a device plugged in already"));
        }
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let name = format!(
            "shards-plug{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let flag = if read_only { "ro" } else { "rw" };
        let (answer, file) = match what {
            What::Disk => {
                let file = PathBuf::from(&source);
                if !file.is_absolute() {
                    return Err(format!("{source}: a disk's file is given by its absolute path"));
                }
                if !size.is_empty() && !file.exists() {
                    made_disk(&file, bytes_of(size)?)?;
                }
                let opened = std::fs::OpenOptions::new()
                    .read(true)
                    .write(!read_only)
                    .open(&file)
                    .map_err(|e| format!("{source}: {e}"))?;
                let flags = if read_only { device::READ_ONLY } else { 0 };
                let step = [&[device::DISK, flags][..], name.as_bytes()].concat();
                use shards_ipc::fd::AsFd as _;
                (self.ask_device(id, &step, &[opened.as_fd()])?, Some(file))
            }
            What::Volume => (self.plug_share(threads, id, &source, read_only, &name)?, None),
            What::Function => return Err("a function is plugged in by plug function".into()),
        };
        let (devfn, address) = answer
            .split_once(' ')
            .and_then(|(d, a)| Some((d.parse::<u8>().ok()?, a.to_string())))
            .ok_or_else(|| format!("its microVM answered {answer:?}"))?;
        let mounted = match what {
            What::Volume => self.guest_device(id, &["share", &address, &name, &path, flag]),
            _ => self.guest_device(id, &["disk", &address, &path, flag]),
        };
        let device = match mounted {
            Ok(device) => device,
            Err(e) => {
                // Taken off again, its guest having found it or not.
                let _ = self.guest_device(id, &["unmount", &address, &path]);
                let _ = self.ask_device(id, &[device::UNPLUG, devfn], &[]);
                return Err(e);
            }
        };
        lock(&self.plugged).entry(id.to_string()).or_default().insert(
            path.clone(),
            Plugged {
                what,
                devfn,
                address: address.clone(),
                file,
                function: None,
            },
        );
        Ok(if device.is_empty() {
            format!("{path} {address}")
        } else {
            format!("{path} /dev/{device} {address}")
        })
    }

    /// A share of `source`, a host's absolute directory or a named volume, served by a
    /// share process of its own (share.rs), its stream given the VM of running container
    /// `id` as a device of `tag`, as a run's are (D207): the VM's answer. On Unix its
    /// connection, which the share process sends and waits to be taken; on Windows the
    /// stream itself, a duplex pipe made here whose server end the share process was given
    /// as it started (D197).
    fn plug_share<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        source: &str,
        read_only: bool,
        tag: &str,
    ) -> Result<String, String> {
        use shards_ipc::fd::AsFd as _;
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
        let dirs = [(
            shards_ipc::fd::OwnedFd::from(opened),
            read_only,
            std::ffi::OsString::new(),
        )];
        let parent = self.parent_of(id).unwrap_or_default();
        let (ends, child) = self.started_shares(threads, &dirs, &parent, false)?;
        lock(&self.shared_by)
            .entry(id.to_string())
            .or_default()
            .push(child);
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
            answer
        }
        #[cfg(windows)]
        {
            let stream = ends.first().ok_or("no stream was made for its share")?;
            self.ask_device(id, &step, &[stream.as_fd()])
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
            self.guest_device(&id, &["unmount", &plugged.address, path])?;
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
    let mut rules: Vec<(&String, String)> = plugged
        .into_iter()
        .flatten()
        .filter(|(name, _)| name.as_str() != but)
        .filter_map(|(name, p)| Some((name, p.function.as_ref()?.rule.clone())))
        .collect();
    rules.sort();
    rules.into_iter().map(|(_, r)| r).collect()
}

/// What init's `function` step answers, `PATH c MAJ:MIN`: the node, and its device rule.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "init answers it where a function is plugged, which Linux alone does (D214)"
    )
)]
fn node_of(said: &str) -> Option<(String, String)> {
    let (path, rest) = said.split_once(' ')?;
    let (kind, numbers) = rest.split_once(' ')?;
    let (major, minor) = numbers.split_once(':')?;
    (kind == "c"
        && path.starts_with("/dev/vfio/")
        && major.parse::<u32>().is_ok()
        && minor.parse::<u32>().is_ok())
    .then(|| (path.to_string(), format!("c {major}:{minor} rwm")))
}

impl<D: Disk> Daemon<D> {
    /// `shards plug function MICROVM FUNCTION` (D214, Linux): the host's function FUNCTION,
    /// by its name (`0000:01:00.0`) or its VFIO device node (`/dev/vfio/devices/vfioN`),
    /// bound to vfio-pci, plugged into running container `id`'s microVM behind its IOMMU,
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
        let (workload_node, rule) =
            node_of(&found).ok_or_else(|| given_up(format!("its guest answered {found:?}")))?;
        let mut rules = function_rules(lock(&self.plugged).get(id), &f.name);
        rules.push(rule.clone());
        let mut allow = vec!["allow"];
        allow.extend(rules.iter().map(String::as_str));
        if let Err(e) = self.guest_device(id, &allow) {
            let _ = self.guest_device(id, &["release", &address, &workload_node]);
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
                }),
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
        let allow = |rules: &[String]| {
            let mut args = vec!["allow"];
            args.extend(rules.iter().map(String::as_str));
            self.guest_device(id, &args)
        };
        allow(&rest)?;
        if let Err(e) = self.guest_device(id, &["release", &plugged.address, &function.node]) {
            let mut all = rest.clone();
            all.push(function.rule.clone());
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
    /// alone; the rules a run's filter keeps are every other function's, in order.
    #[test]
    fn a_functions_node_and_rules_are_read() {
        assert_eq!(
            node_of("/dev/vfio/0 c 241:0"),
            Some(("/dev/vfio/0".into(), "c 241:0 rwm".into()))
        );
        assert_eq!(
            node_of("/dev/vfio/devices/vfio0 c 238:0"),
            Some(("/dev/vfio/devices/vfio0".into(), "c 238:0 rwm".into()))
        );
        for wrong in [
            "/dev/sda b 8:0",
            "/dev/vfio/0 c 241",
            "/dev/vfio/0 c x:0",
            "/dev/null c 1:3",
            "",
        ] {
            assert_eq!(node_of(wrong), None, "{wrong}");
        }
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
                    }),
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
            },
        );
        assert_eq!(function_rules(Some(&plugged), ""), ["c 241:0 rwm", "c 241:1 rwm"]);
        assert_eq!(function_rules(Some(&plugged), "0000:01:00.0"), ["c 241:1 rwm"]);
        assert!(function_rules(None, "").is_empty());
    }

    /// Sizes in bytes or 1024's powers.
    #[test]
    fn sizes_read_as_dockers_do() {
        assert_eq!(bytes_of("512"), Ok(512));
        assert_eq!(bytes_of("64m"), Ok(64 << 20));
        assert_eq!(bytes_of("1GB"), Ok(1 << 30));
        assert!(bytes_of("1x").is_err());
        assert!(bytes_of("m").is_err());
    }
}

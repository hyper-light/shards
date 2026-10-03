//! Containers, as `docker run` leaves them (docs/design/architecture.md D27): each run is
//! one, with an ID and a name, running until its command ends, then exited until
//! `shards rm` removes it, or at once with `--rm`. The daemon keeps them, and writes each
//! to `containers/ID/config.json` in the home, so that they outlive it ([`Registry`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a container is in its life (moby api/types/container/state.go).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Created,
    Running,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    /// 64 hex digits.
    pub id: String,
    pub name: String,
    /// The image as the run named it.
    pub image: String,
    /// What runs: the entrypoint, then the command.
    pub command: Vec<String>,
    /// Nanoseconds since the Unix epoch.
    pub created: u128,
    pub state: State,
    pub started: Option<u128>,
    pub finished: Option<u128>,
    pub exit_code: Option<u8>,
    /// `--rm`: removed once it ends.
    pub auto_remove: bool,
    /// Bytes of its output its log could not keep (audit A12).
    #[serde(default)]
    pub log_lost: u64,
    /// What `stop` sends it unless told: `--stop-signal`'s, or its image's `StopSignal`,
    /// read once as it is created (moby container.StopSignal; SIGTERM if neither).
    #[serde(default)]
    pub stop_signal: Option<i64>,
    /// How long `stop` waits before SIGKILL unless told: `--stop-timeout`, in seconds,
    /// negative for ever (moby container.StopTimeout; 10 if not given).
    #[serde(default)]
    pub stop_timeout: Option<i64>,
    /// Its ports while it runs: each published at a host address and port, or exposed
    /// alone (no address, public port 0), as dockerd lists them.
    #[serde(default)]
    pub ports: Vec<PortRecord>,
    /// Its image's ID, what its reference resolved to as it was made (`images` counts
    /// the containers of each); none in a record from before shards kept it.
    #[serde(default)]
    pub image_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRecord {
    pub ip: Option<std::net::IpAddr>,
    pub private: u16,
    pub public: u16,
    pub proto: String,
}

pub use crate::spec::now;

/// A new container ID: 32 random bytes in hex, as moby's `stringid.GenerateRandomID`
/// makes them, drawn again while the first 12 digits are all decimal: the short ID names
/// the container's host, and a hostname must not look like a number
/// (moby daemon/internal/stringid/stringid.go).
pub fn new_id() -> io::Result<String> {
    loop {
        let mut bytes = [0u8; 32];
        shards_vmm::platform::fill_random(&mut bytes)?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if !id.bytes().take(12).all(|b| b.is_ascii_digit()) {
            return Ok(id);
        }
    }
}

/// Whether `name` may name a container: `[a-zA-Z0-9][a-zA-Z0-9_.-]+`, after an optional
/// `/` (moby daemon/names/names.go).
pub fn valid_name(name: &str) -> bool {
    let name = name.strip_prefix('/').unwrap_or(name).as_bytes();
    let Some((first, rest)) = name.split_first() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && !rest.is_empty()
        && rest
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// A container's record, in its directory, and the name it is written under first.
const RECORD: &str = "config.json";
const NEW_RECORD: &str = "config.json.new";
/// The largest record read. A run's command arrives in one message
/// (`shards_ipc::MAX_PAYLOAD`), and JSON writes a byte as at most 6.
const MAX_RECORD: u64 = 8 << 20;

/// What the registry does to the disk, so that a test can fail it, cut it short or lose
/// power under it. [`Real`] is the host's filesystem.
pub trait Disk: Send + Sync + std::fmt::Debug + 'static {
    /// Makes `dir`, private to its user, if it is not there.
    fn create_dir(&self, dir: &Path) -> io::Result<()>;
    /// The names in `dir`.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
    /// The file at `path`, at most `max` bytes of it.
    fn read(&self, path: &Path, max: u64) -> io::Result<Vec<u8>>;
    /// Makes the file at `path` hold `bytes`, created if need be.
    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&self, path: &Path) -> io::Result<()>;
    /// Makes the entries of `dir` durable (`platform::sync_dir`).
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

/// The host's filesystem.
#[derive(Debug)]
pub struct Real;

impl Disk for Real {
    fn create_dir(&self, dir: &Path) -> io::Result<()> {
        shards_vmm::platform::create_private_dir(dir)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn read(&self, path: &Path, max: u64) -> io::Result<Vec<u8>> {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(max.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: over {max} bytes", path.display()),
            ));
        }
        Ok(bytes)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_dir_all(path)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        shards_vmm::platform::sync_dir(dir)
    }
}

/// The daemon's containers, by ID, each kept in its directory under `containers` in the
/// home (docs/design/architecture.md D27, "Durability"):
/// - A container is seen once its record is written, and a removal changes what is seen
///   only once the container's directory is set aside, so what a command sees or is told
///   outlives a crash of the daemon. A container is reserved first, its name held, and its
///   record written beside the run's start, which never waits for it: a write waits on
///   whatever else the filesystem is doing, milliseconds at a busy host's p90 (PM M46).
///   What happens to a reserved container waits in its reservation, for the record.
/// - What happened to a container's run is kept at once, since it happened
///   ([`change`](Self::change)), and its record is written after, outside the lock the
///   registry is kept under ([`snapshot`](Self::snapshot), [`written`](Self::written)); a
///   record not written yet, or that could not be, is behind, and is written again until
///   it is not.
/// - Records are written and renamed into place, not synced: syncing one costs 8.5 ms on
///   macOS (PM M46). A removal is synced before its name is let go, so that a power loss
///   cannot bring back a container whose name another has taken.
#[derive(Debug)]
pub struct Registry {
    root: PathBuf,
    by_id: BTreeMap<String, Container>,
    /// The IDs of those in sight, by name: a command finds its container by name without
    /// looking at every other, as dockerd's name registrar does (PM M88).
    by_name: HashMap<String, String>,
    /// The containers whose records are behind.
    behind: BTreeSet<String>,
    /// Containers being removed, by ID: set aside, their names held until the removal is
    /// durable.
    leaving: BTreeMap<String, Container>,
    /// Containers reserved, by ID, whose records are being written: their names held,
    /// and not yet seen.
    arriving: BTreeMap<String, Container>,
}

impl Registry {
    /// The containers kept under `containers` in `home`, as dockerd restores its own when
    /// it starts (moby daemon/daemon.go restore). What `note` hears goes to the log.
    pub fn open(home: &Path, note: &mut dyn FnMut(String)) -> io::Result<Registry> {
        Registry::open_on(home.join("containers"), &Real, note)
    }

    /// The containers kept in `root` on `disk`, reconciled with whatever a crash or a
    /// power loss left of them:
    /// - A run no daemon follows can no longer be followed: a container running exited,
    ///   with 255 for the status nobody saw, and one created with no status yet has 255,
    ///   as one whose run did not start has its status.
    /// - A `--rm` container that no longer runs goes, and so does a removal that was cut
    ///   short.
    /// - A directory with no record is a spare a daemon made and never used, or a
    ///   container cut short before its record was written, and goes too.
    /// - A record that cannot be read gives way to its next version, whole, where a power
    ///   loss kept that from its rename; otherwise it is left as it is, and noted, as
    ///   dockerd leaves a container it cannot load.
    pub fn open_on(root: PathBuf, disk: &dyn Disk, note: &mut dyn FnMut(String)) -> io::Result<Registry> {
        disk.create_dir(&root)?;
        let mut registry = Registry {
            root,
            by_id: BTreeMap::new(),
            by_name: HashMap::new(),
            behind: BTreeSet::new(),
            leaving: BTreeMap::new(),
            arriving: BTreeMap::new(),
        };
        let mut removed = false;
        for name in disk.list(&registry.root)? {
            let dir = registry.root.join(&name);
            if name.starts_with('.') {
                if name.ends_with(".removing") {
                    removed |= Registry::gone(disk, &dir, note);
                }
                continue;
            }
            let c = match load(disk, &dir, &name, RECORD) {
                Loaded::Record(c) => {
                    // A next version a crash kept from its rename: never seen.
                    match disk.remove_file(&dir.join(NEW_RECORD)) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => note(format!("{}: {e}", dir.join(NEW_RECORD).display())),
                    }
                    *c
                }
                Loaded::Missing => {
                    removed |= Registry::gone(disk, &dir, note);
                    continue;
                }
                // A record whose rename outlasted a power loss, and its bytes did not: the
                // next version, whole, is the container's, if a power loss kept it from
                // its rename.
                Loaded::Unreadable(why) => match load(disk, &dir, &name, NEW_RECORD) {
                    Loaded::Record(c) => {
                        note(format!("{}: {why}; its next version is taken", dir.display()));
                        if let Err(e) = disk.rename(&dir.join(NEW_RECORD), &dir.join(RECORD)) {
                            note(format!("{}: {e}", dir.join(NEW_RECORD).display()));
                        }
                        *c
                    }
                    Loaded::Missing | Loaded::Unreadable(_) => {
                        note(format!("{}: {why}; left as it is", dir.display()));
                        continue;
                    }
                },
            };
            if c.auto_remove {
                removed |= Registry::gone(disk, &dir, note);
                continue;
            }
            registry.see(c);
        }
        if removed && let Err(e) = disk.sync_dir(&registry.root) {
            note(format!("{}: {e}", registry.root.display()));
        }
        let lost: Vec<String> = registry
            .by_id
            .values()
            .filter(|c| c.state == State::Running || (c.state == State::Created && c.exit_code.is_none()))
            .map(|c| c.id.clone())
            .collect();
        let at = now();
        for id in lost {
            let ended = registry.update(disk, &id, |c| {
                if c.state == State::Running {
                    c.state = State::Exited;
                    c.finished = c.finished.or(Some(at));
                }
                c.exit_code = Some(255);
            });
            if let Err(e) = ended {
                note(format!("container {id}: its record is behind: {e}"));
            }
        }
        Ok(registry)
    }

    /// Removes `dir` and all it holds; whether it went.
    fn gone(disk: &dyn Disk, dir: &Path, note: &mut dyn FnMut(String)) -> bool {
        match disk.remove_dir_all(dir) {
            Ok(()) => true,
            Err(e) => {
                note(format!("{}: {e}", dir.display()));
                false
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&Container> {
        self.by_id.get(id)
    }

    /// The container with `id`, its record written yet or not: what was set as it was
    /// made, which a run may need before its record is written.
    pub fn made(&self, id: &str) -> Option<&Container> {
        self.by_id.get(id).or_else(|| self.arriving.get(id))
    }

    pub fn all(&self) -> impl Iterator<Item = &Container> {
        self.by_id.values()
    }

    /// The container named `name`, of those in sight.
    pub fn named(&self, name: &str) -> Option<&Container> {
        self.by_id.get(self.by_name.get(name)?)
    }

    /// Those in sight whose IDs start with `prefix`, found as their IDs are ordered.
    pub fn id_prefixed<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a Container> {
        use std::ops::Bound;
        self.by_id
            .range::<str, _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(move |(id, _)| id.starts_with(prefix))
            .map(|(_, c)| c)
    }

    /// The container holding `name`, for a new container: one in sight, one reserved, or
    /// one whose removal is not yet durable.
    pub fn name_taken(&self, name: &str) -> Option<&Container> {
        self.named(name).or_else(|| {
            self.arriving
                .values()
                .chain(self.leaving.values())
                .find(|c| c.name == name)
        })
    }

    /// Reserves `c`, in the directory made for it: its name is held at once, and it is
    /// seen once its record is written ([`arrival`](Self::arrival), then
    /// [`admit`](Self::admit)).
    pub fn reserve(&mut self, c: Container) {
        self.arriving.insert(c.id.clone(), c);
    }

    /// Whether any container is reserved, its record not yet written.
    /// Whether a container being removed still holds `name`.
    pub fn is_leaving_name(&self, name: &str) -> bool {
        self.leaving.values().any(|c| c.name == name)
    }

    pub fn any_arriving(&self) -> bool {
        !self.arriving.is_empty()
    }

    /// Whether a reserved container, its record not yet written, may be what `reference`
    /// names: its ID, its name, or the start of its ID.
    pub fn arriving_as(&self, reference: &str) -> bool {
        let name = reference.strip_prefix('/').unwrap_or(reference);
        self.arriving
            .values()
            .any(|c| c.id.starts_with(reference) || c.name == name)
    }

    /// Whether the container with `id` is reserved, its record not yet written.
    pub fn is_arriving(&self, id: &str) -> bool {
        self.arriving.contains_key(id)
    }

    /// What writes the record of the reserved container with `id`, and the record, for a
    /// write outside the registry's lock.
    pub fn arrival(&self, id: &str) -> Option<(Recorder, Container)> {
        let c = self.arriving.get(id)?.clone();
        let recorder = Recorder {
            root: self.root.clone(),
        };
        Some((recorder, c))
    }

    /// Lets the reserved container with `id` be seen, its record `written`, unless it
    /// changed since: then it is to be written again, and this is false.
    pub fn admit(&mut self, id: &str, written: &Container) -> bool {
        if self.arriving.get(id).is_some_and(|c| c != written) {
            return false;
        }
        if let Some(c) = self.arriving.remove(id) {
            self.see(c);
        }
        true
    }

    /// Puts `c` in sight.
    fn see(&mut self, c: Container) {
        self.by_name.insert(c.name.clone(), c.id.clone());
        self.by_id.insert(c.id.clone(), c);
    }

    /// Lets the reserved container with `id` be seen though its record could not be
    /// written: it exists, and its record is behind.
    pub fn admit_behind(&mut self, id: &str) {
        if let Some(c) = self.arriving.remove(id) {
            self.behind.insert(c.id.clone());
            self.see(c);
        }
    }

    /// Keeps what happened to the container with `id`: `f` changes it, and the change
    /// stands, since it happened; its record is behind until it is written. A container
    /// with no record is an error: whoever changes one owns it until it goes (audit A06).
    pub fn change(&mut self, id: &str, f: impl FnOnce(&mut Container)) -> io::Result<()> {
        // A reserved container's change waits in its reservation, for its record.
        if let Some(c) = self.arriving.get_mut(id) {
            f(c);
            return Ok(());
        }
        let c = self
            .by_id
            .get_mut(id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no record of the container"))?;
        f(c);
        self.behind.insert(id.to_string());
        Ok(())
    }

    /// [`change`](Self::change), its record written at once: as the registry opens. An
    /// error says the record could not be written, and is behind.
    pub fn update(&mut self, disk: &dyn Disk, id: &str, f: impl FnOnce(&mut Container)) -> io::Result<()> {
        self.change(id, f)?;
        let Some(c) = self.by_id.get(id) else {
            return Ok(());
        };
        write(disk, &self.root, c)?;
        self.behind.remove(id);
        Ok(())
    }

    /// The record of the container with `id` as it stands, and what writes it, for a write
    /// outside the registry's lock ([`written`](Self::written)).
    pub fn snapshot(&self, id: &str) -> Option<(Recorder, Container)> {
        let c = self.by_id.get(id)?.clone();
        let recorder = Recorder {
            root: self.root.clone(),
        };
        Some((recorder, c))
    }

    /// The container with `id` had its record written as `written`: it is behind no more,
    /// unless it has changed since, to be written again.
    pub fn written(&mut self, id: &str, written: &Container) {
        if self.by_id.get(id).is_none_or(|c| c == written) {
            self.behind.remove(id);
        }
    }

    /// The containers whose records are behind.
    pub fn behind(&self) -> impl Iterator<Item = &String> {
        self.behind.iter()
    }

    /// Takes the container with `id` out of sight: its directory is set aside first, so
    /// that a failure leaves it as it was. Its name stays held until the removal is
    /// durable ([`Removal::sync`], then [`release`](Self::release)).
    pub fn remove(&mut self, disk: &dyn Disk, id: &str) -> io::Result<Option<Removal>> {
        if !self.by_id.contains_key(id) {
            return Ok(None);
        }
        let aside = self.root.join(format!(".{id}.removing"));
        match disk.rename(&self.root.join(id), &aside) {
            Ok(()) => {}
            // Its directory went by other hands: nothing to set aside.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let Some(c) = self.by_id.remove(id) else {
            return Ok(None);
        };
        self.by_name.remove(&c.name);
        self.behind.remove(id);
        self.leaving.insert(id.to_string(), c.clone());
        Ok(Some(Removal {
            container: c,
            root: self.root.clone(),
            aside,
        }))
    }

    /// Lets the name of a container whose removal is durable go.
    pub fn release(&mut self, id: &str) {
        self.leaving.remove(id);
    }

    /// The directory kept for the container with `id`.
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
}

/// What a container's directory holds of its record.
enum Loaded {
    /// Boxed: a record is many times the size of the other answers.
    Record(Box<Container>),
    Missing,
    /// Why it cannot be read.
    Unreadable(String),
}

/// The record in `file` of the directory `dir`, which is named for its container.
fn load(disk: &dyn Disk, dir: &Path, name: &str, file: &str) -> Loaded {
    match disk.read(&dir.join(file), MAX_RECORD) {
        Ok(bytes) => match serde_json::from_slice::<Container>(&bytes) {
            Ok(c) if c.id == name => Loaded::Record(Box::new(c)),
            Ok(c) => Loaded::Unreadable(format!("its record is container {}'s", c.id)),
            Err(e) => Loaded::Unreadable(format!("its record cannot be read ({e})")),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Loaded::Missing,
        Err(e) => Loaded::Unreadable(e.to_string()),
    }
}

/// Writes a reserved container's record ([`Registry::arrival`]).
#[derive(Debug)]
pub struct Recorder {
    root: PathBuf,
}

impl Recorder {
    pub fn write(&self, disk: &dyn Disk, c: &Container) -> io::Result<()> {
        write(disk, &self.root, c)
    }
}

/// Writes `c`'s record into its directory under `root`: whole, then renamed over the last.
fn write(disk: &dyn Disk, root: &Path, c: &Container) -> io::Result<()> {
    let dir = root.join(&c.id);
    let bytes = serde_json::to_vec(c).map_err(io::Error::other)?;
    disk.write(&dir.join(NEW_RECORD), &bytes)?;
    disk.rename(&dir.join(NEW_RECORD), &dir.join(RECORD))
}

/// A container taken out of sight, its directory set aside ([`Registry::remove`]).
#[derive(Debug)]
pub struct Removal {
    pub container: Container,
    root: PathBuf,
    aside: PathBuf,
}

impl Removal {
    /// Makes the removal durable: the containers' directory is synced, so that no crash
    /// brings the container back. Slow (PM M46), so done outside the registry's lock.
    pub fn sync(&self, disk: &dyn Disk) -> io::Result<()> {
        disk.sync_dir(&self.root)
    }

    /// Deletes what was set aside. What a failure leaves, the next start deletes.
    pub fn delete(&self, disk: &dyn Disk) -> io::Result<()> {
        match disk.remove_dir_all(&self.aside) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A home of its own, removed when dropped, whether its test passes or panics.
    struct TempHome(PathBuf);

    impl std::ops::Deref for TempHome {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for TempHome {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_home(tag: &str) -> TempHome {
        let dir = std::env::temp_dir().join(format!("shards-containers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempHome(dir)
    }

    fn container(id: &str, name: &str, state: State) -> Container {
        Container {
            id: id.into(),
            name: name.into(),
            image: "alpine".into(),
            command: vec!["sh".into()],
            created: 1,
            state,
            started: Some(2),
            finished: None,
            exit_code: None,
            auto_remove: false,
            log_lost: 0,
            stop_signal: None,
            stop_timeout: None,
            ports: Vec::new(),
            image_id: None,
        }
    }

    /// Creates `c` as the daemon does: reserved, its record written, then seen; or, if the
    /// write fails, seen with its record behind.
    fn create(r: &mut Registry, disk: &dyn Disk, c: Container) -> io::Result<()> {
        let id = c.id.clone();
        r.reserve(c);
        let Some((recorder, c)) = r.arrival(&id) else {
            return Err(io::Error::other("not reserved"));
        };
        match recorder.write(disk, &c) {
            Ok(()) if r.admit(&id, &c) => Ok(()),
            Ok(()) => Err(io::Error::other("changed as it was written")),
            Err(e) => {
                r.admit_behind(&id);
                Err(e)
            }
        }
    }

    /// Opens the registry in `home`, failing the test on anything noted.
    fn open(home: &Path) -> Registry {
        Registry::open(home, &mut |note| panic!("noted: {note}")).unwrap()
    }

    /// Makes the directory a spare would have made for `id`, then creates `c` in it.
    fn made(r: &mut Registry, c: Container) {
        std::fs::create_dir_all(r.dir(&c.id)).unwrap();
        create(r, &Real, c).unwrap();
    }

    /// The host's filesystem, which fails its `at`th operation (counting from 0), and with
    /// `crash` every one after too, as a process that died there does none of them. It
    /// logs what it does.
    #[derive(Debug, Default)]
    struct Faulty {
        ops: AtomicUsize,
        at: Option<usize>,
        crash: bool,
        log: Mutex<Vec<String>>,
    }

    impl Faulty {
        fn at(at: usize, crash: bool) -> Faulty {
            Faulty {
                at: Some(at),
                crash,
                ..Faulty::default()
            }
        }

        fn op(&self, what: &str, path: &Path) -> io::Result<()> {
            let n = self.ops.fetch_add(1, Ordering::SeqCst);
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            self.log
                .lock()
                .unwrap()
                .push(format!("{what} {}", name.unwrap_or_default()));
            match self.at {
                Some(at) if n == at || (self.crash && n > at) => {
                    Err(io::Error::other(format!("injected at {what}")))
                }
                _ => Ok(()),
            }
        }
    }

    impl Disk for Faulty {
        fn create_dir(&self, dir: &Path) -> io::Result<()> {
            self.op("create_dir", dir)?;
            Real.create_dir(dir)
        }
        fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
            self.op("list", dir)?;
            Real.list(dir)
        }
        fn read(&self, path: &Path, max: u64) -> io::Result<Vec<u8>> {
            self.op("read", path)?;
            Real.read(path, max)
        }
        fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.op("write", path)?;
            Real.write(path, bytes)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.op("rename", to)?;
            Real.rename(from, to)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.op("remove_file", path)?;
            Real.remove_file(path)
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.op("remove_dir_all", path)?;
            Real.remove_dir_all(path)
        }
        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.op("sync_dir", dir)?;
            Real.sync_dir(dir)
        }
    }

    fn faulty(home: &Path, disk: &Faulty) -> Registry {
        Registry::open_on(home.join("containers"), disk, &mut |note| panic!("noted: {note}")).unwrap()
    }

    #[test]
    fn names_are_what_dockerd_accepts() {
        for ok in ["ab", "a1", "web.1", "a_b-c", "/web", "X9"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "a", "/", "_a", ".a", "-a", "a b", "a/b", "ä1", "a!"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn ids_are_64_hex_digits_and_differ() {
        let (a, b) = (new_id().unwrap(), new_id().unwrap());
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, b);
    }

    #[test]
    fn short_ids_are_never_all_digits() {
        for _ in 0..2000 {
            let id = new_id().unwrap();
            assert!(!id.bytes().take(12).all(|b| b.is_ascii_digit()), "{id}");
        }
    }

    /// A change to a container with no record fails: whatever changes a container owns
    /// it until it goes, so one missing is a broken invariant, not nothing to do (audit
    /// A06).
    #[test]
    fn a_container_with_no_record_cannot_change() {
        let home = temp_home("missing");
        let mut registry = open(&home);
        let e = registry
            .update(&Real, "gone", |c| c.exit_code = Some(1))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        made(&mut registry, container("here", "here", State::Created));
        registry.update(&Real, "here", |c| c.exit_code = Some(1)).unwrap();
        assert_eq!(registry.get("here").unwrap().exit_code, Some(1));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Containers are found by name, and by the start of their IDs, as they come and go:
    /// one reserved is not yet in sight though its name is held; one removed leaves sight
    /// at once, its name held until the removal is durable; those reopened are found as
    /// before.
    #[test]
    fn containers_are_found_by_name_and_id_prefix_as_they_come_and_go() {
        let home = temp_home("found");
        let mut r = open(&home);
        made(&mut r, container("ab12", "web", State::Created));
        made(&mut r, container("ab34", "db", State::Created));
        made(&mut r, container("cd56", "cache", State::Created));
        let ids = |r: &Registry, p: &str| r.id_prefixed(p).map(|c| c.id.clone()).collect::<Vec<_>>();
        assert_eq!(r.named("db").map(|c| c.id.as_str()), Some("ab34"));
        assert_eq!(ids(&r, "ab"), ["ab12", "ab34"]);
        assert_eq!(ids(&r, "ab3"), ["ab34"]);
        assert_eq!(ids(&r, "cd56"), ["cd56"]);
        assert!(ids(&r, "e").is_empty());
        assert!(ids(&r, "ab345").is_empty());
        r.reserve(container("ef78", "queue", State::Created));
        assert!(r.named("queue").is_none());
        assert_eq!(r.name_taken("queue").map(|c| c.id.as_str()), Some("ef78"));
        let removal = r.remove(&Real, "ab34").unwrap().unwrap();
        assert!(r.named("db").is_none());
        assert_eq!(r.by_name.len(), 2, "a name kept for a container out of sight");
        assert_eq!(ids(&r, "ab"), ["ab12"]);
        assert_eq!(r.name_taken("db").map(|c| c.id.as_str()), Some("ab34"));
        removal.sync(&Real).unwrap();
        removal.delete(&Real).unwrap();
        r.release("ab34");
        assert!(r.name_taken("db").is_none());
        drop(r);
        let r = open(&home);
        assert_eq!(r.named("web").map(|c| c.id.as_str()), Some("ab12"));
        assert_eq!(r.named("cache").map(|c| c.id.as_str()), Some("cd56"));
        assert!(r.named("db").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn containers_outlive_their_daemon_and_running_ones_end() {
        let home = temp_home("reopen");
        let mut r = open(&home);
        let removed = Container {
            auto_remove: true,
            ..container("cc", "three", State::Exited)
        };
        let failed = Container {
            exit_code: Some(127),
            started: None,
            ..container("dd", "four", State::Created)
        };
        for c in [
            container("aa", "one", State::Exited),
            container("bb", "two", State::Running),
            removed,
            failed.clone(),
            Container {
                started: None,
                ..container("ee", "five", State::Created)
            },
        ] {
            made(&mut r, c);
        }
        let again = open(&home);
        assert_eq!(again.get("aa"), r.get("aa"));
        let (bb, ee) = (again.get("bb").unwrap(), again.get("ee").unwrap());
        assert_eq!(
            (bb.state, bb.exit_code),
            (State::Exited, Some(255)),
            "a run no daemon follows cannot be running"
        );
        assert!(bb.finished.is_some());
        assert_eq!(
            (ee.state, ee.exit_code, ee.finished),
            (State::Created, Some(255), None),
            "nor yet to start"
        );
        assert_eq!(
            again.get("dd"),
            Some(&failed),
            "a run that never started stays so"
        );
        assert!(again.get("cc").is_none(), "--rm containers go");
        assert!(!home.join("containers/cc").exists());
        assert_eq!(
            open(&home).get("bb"),
            again.get("bb"),
            "the lost run's end is kept"
        );
        assert!(again.name_taken("one").is_some());
        let mut again = again;
        let removal = again.remove(&Real, "aa").unwrap().unwrap();
        removal.sync(&Real).unwrap();
        again.release("aa");
        removal.delete(&Real).unwrap();
        assert!(!home.join("containers/aa").exists());
        assert!(open(&home).get("aa").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A container is seen once its record is written, its name held from its
    /// reservation; what happens to it meanwhile waits in the reservation, and is written
    /// before it is seen. One whose record cannot be written is seen, its record behind,
    /// since its run may have started (audit A15).
    #[test]
    fn a_container_is_seen_once_its_record_is_written() {
        let home = temp_home("create");
        let mut r = open(&home);
        std::fs::create_dir_all(r.dir("aa")).unwrap();
        r.reserve(container("aa", "one", State::Created));
        assert!(r.get("aa").is_none() && r.named("one").is_none(), "not seen yet");
        assert!(r.name_taken("one").is_some(), "its name held");
        let (recorder, first) = r.arrival("aa").unwrap();
        r.update(&Real, "aa", |c| c.state = State::Running).unwrap();
        assert!(r.get("aa").is_none(), "a change is no record");
        recorder.write(&Real, &first).unwrap();
        assert!(!r.admit("aa", &first), "changed as it was written");
        assert!(r.get("aa").is_none());
        let (recorder, second) = r.arrival("aa").unwrap();
        assert_eq!(second.state, State::Running);
        recorder.write(&Real, &second).unwrap();
        assert!(r.admit("aa", &second));
        assert_eq!(r.get("aa").map(|c| c.state), Some(State::Running));
        assert_eq!(
            open(&home).get("aa").map(|c| (c.state, c.exit_code)),
            Some((State::Exited, Some(255))),
            "its record was the change's"
        );
        drop(r);

        // Opening makes and lists the directory, reads the record the last opening
        // reconciled, and removes a leftover; the next write is the create's.
        let disk = Faulty::at(4, false);
        let mut r = faulty(&home, &disk);
        std::fs::create_dir_all(r.dir("bb")).unwrap();
        let e = create(&mut r, &disk, container("bb", "two", State::Created)).unwrap_err();
        assert!(e.to_string().contains("injected at write"), "{e}");
        assert!(r.get("bb").is_some(), "seen, its record behind");
        assert_eq!(r.behind().collect::<Vec<_>>(), ["bb"]);
        let (recorder, c) = r.snapshot("bb").unwrap();
        recorder.write(&disk, &c).unwrap();
        r.written("bb", &c);
        assert_eq!(r.behind().count(), 0);
        assert!(open(&home).get("bb").is_some());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// What happened to a container stands even when its record cannot be written; the
    /// record is behind until it is written again, and then outlives the daemon.
    #[test]
    fn a_record_that_cannot_be_written_is_behind_until_it_is() {
        let home = temp_home("behind");
        let mut r = open(&home);
        made(&mut r, container("aa", "one", State::Running));
        drop(r);
        // Opening makes and lists the directory, reads the record, removes a leftover,
        // and writes and renames the record of the run it could not follow; then the
        // update writes and renames, and the rename fails.
        let disk = Faulty::at(7, false);
        let mut r = faulty(&home, &disk);
        // The reopening ended the run it could not follow; the next writes are ours.
        assert_eq!(r.get("aa").unwrap().exit_code, Some(255));
        let e = r.update(&disk, "aa", |c| c.exit_code = Some(3)).unwrap_err();
        assert!(e.to_string().contains("injected at rename"), "{e}");
        assert_eq!(r.get("aa").unwrap().exit_code, Some(3), "it happened");
        assert_eq!(open(&home).get("aa").unwrap().exit_code, Some(255), "behind");
        assert_eq!(r.behind().collect::<Vec<_>>(), ["aa"]);
        // Written as it stood, then changed: behind still, until written as it stands.
        let (recorder, stood) = r.snapshot("aa").unwrap();
        r.change("aa", |c| c.exit_code = Some(4)).unwrap();
        recorder.write(&disk, &stood).unwrap();
        r.written("aa", &stood);
        assert_eq!(
            r.behind().collect::<Vec<_>>(),
            ["aa"],
            "written as it no longer stands"
        );
        let (recorder, stands) = r.snapshot("aa").unwrap();
        recorder.write(&disk, &stands).unwrap();
        r.written("aa", &stands);
        assert_eq!(r.behind().count(), 0);
        assert_eq!(open(&home).get("aa").unwrap().exit_code, Some(4));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A removal that fails changes nothing: the container is seen, with its name, and
    /// can be removed again (audit A15).
    #[test]
    fn a_removal_that_fails_leaves_the_container_as_it_was() {
        let home = temp_home("remove");
        let mut r = open(&home);
        made(&mut r, container("aa", "one", State::Exited));
        drop(r);
        let disk = Faulty::at(4, false);
        let mut r = faulty(&home, &disk);
        let e = r.remove(&disk, "aa").unwrap_err();
        assert!(e.to_string().contains("injected at rename"), "{e}");
        assert!(r.get("aa").is_some());
        assert!(r.name_taken("one").is_some());
        assert!(open(&home).get("aa").is_some());
        let removal = r.remove(&disk, "aa").unwrap().unwrap();
        assert!(r.get("aa").is_none());
        removal.sync(&disk).unwrap();
        r.release("aa");
        removal.delete(&disk).unwrap();
        assert!(open(&home).get("aa").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A container's name is held until its removal is durable: a power loss before then
    /// could bring the container back, next to another that took its name.
    #[test]
    fn a_name_is_held_until_its_removal_is_durable() {
        let home = temp_home("leaving");
        let mut r = open(&home);
        made(&mut r, container("aa", "one", State::Exited));
        let removal = r.remove(&Real, "aa").unwrap().unwrap();
        assert!(r.get("aa").is_none(), "out of sight at once");
        assert!(r.named("one").is_none(), "and by its name");
        assert_eq!(r.name_taken("one").map(|c| c.id.as_str()), Some("aa"));
        removal.sync(&Real).unwrap();
        r.release("aa");
        assert!(r.name_taken("one").is_none());
        // Its files, left by a delete that failed, go at the next start.
        assert!(home.join("containers/.aa.removing").is_dir());
        assert!(open(&home).get("aa").is_none());
        assert!(!home.join("containers/.aa.removing").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The next start reconciles what a crash or a power loss may leave: a removal and a
    /// write cut short, a spare, a record torn or empty, one in another's directory, and a
    /// record torn whose next version is whole.
    #[test]
    fn a_start_reconciles_what_crashes_leave() {
        let home = temp_home("leftovers");
        let mut r = open(&home);
        made(&mut r, container("aa", "one", State::Exited));
        drop(r);
        let root = home.join("containers");
        std::fs::write(root.join("aa/config.json.new"), b"{\"id\":").unwrap();
        std::fs::create_dir_all(root.join(".bb.removing")).unwrap();
        std::fs::write(root.join(".bb.removing/log"), b"x").unwrap();
        std::fs::create_dir_all(root.join("spare")).unwrap();
        std::fs::write(root.join("spare/log"), b"").unwrap();
        for (dir, record) in [
            ("empty", &b""[..]),
            ("torn", br#"{"id":"torn","na"#),
            ("moved", &std::fs::read(root.join("aa/config.json")).unwrap()[..]),
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("config.json"), record).unwrap();
        }
        let next = Container {
            exit_code: Some(4),
            ..container("cc", "three", State::Exited)
        };
        std::fs::create_dir_all(root.join("cc")).unwrap();
        std::fs::write(root.join("cc/config.json"), b"").unwrap();
        std::fs::write(
            root.join("cc/config.json.new"),
            serde_json::to_vec(&next).unwrap(),
        )
        .unwrap();
        let mut notes = Vec::new();
        let again = Registry::open(&home, &mut |n| notes.push(n)).unwrap();
        assert_eq!(
            again.all().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["aa", "cc"]
        );
        assert_eq!(again.get("cc"), Some(&next), "its next version is taken");
        assert!(!root.join("cc/config.json.new").exists());
        assert_eq!(
            Registry::open(&home, &mut drop).unwrap().get("cc"),
            Some(&next),
            "and kept"
        );
        assert!(
            notes.iter().any(|n| n.ends_with("its next version is taken")),
            "{notes:?}"
        );
        notes.retain(|n| !n.ends_with("its next version is taken"));
        assert!(!root.join("aa/config.json.new").exists());
        assert!(!root.join(".bb.removing").exists());
        assert!(!root.join("spare").exists());
        assert_eq!(notes.len(), 3, "{notes:?}");
        for dir in ["empty", "torn", "moved"] {
            assert!(root.join(dir).is_dir(), "{dir}: left as it is");
            assert!(
                notes.iter().any(|n| n.contains(&format!("containers/{dir}"))),
                "{notes:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A crash at any step of a container's life leaves what the next start reconciles:
    /// the container not seen until its record was written, seen after that with the
    /// last status written or 255, and gone once its directory was set aside; the other
    /// containers as they were; nothing left over (audit A15).
    #[test]
    fn a_crash_anywhere_in_a_life_is_reconciled() {
        /// One life: made, created, running, exited with 3, removed; or with `--rm`,
        /// removed at its end.
        fn live(r: &mut Registry, disk: &dyn Disk, auto_remove: bool) {
            let c = Container {
                auto_remove,
                started: None,
                ..container("life", "life", State::Created)
            };
            if disk.create_dir(&r.dir("life")).is_err() || create(r, disk, c).is_err() {
                return;
            }
            let _ = r.update(disk, "life", |c| c.state = State::Running);
            let removal = if auto_remove {
                r.remove(disk, "life")
            } else {
                let _ = r.update(disk, "life", |c| {
                    c.state = State::Exited;
                    c.exit_code = Some(3);
                });
                r.remove(disk, "life")
            };
            if let Ok(Some(removal)) = removal
                && removal.sync(disk).is_ok()
            {
                r.release("life");
                let _ = removal.delete(disk);
            }
        }
        for auto_remove in [false, true] {
            let home = temp_home(&format!("crash-{auto_remove}"));
            let mut r = open(&home);
            made(&mut r, container("other", "other", State::Exited));
            drop(r);
            // A life without a crash, for its steps.
            let disk = Faulty::default();
            let mut r = faulty(&home, &disk);
            let opened = disk.ops.load(Ordering::SeqCst);
            live(&mut r, &disk, auto_remove);
            let steps: Vec<String> = disk.log.lock().unwrap()[opened..].to_vec();
            let at = |what: &str, n: usize| {
                steps
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.starts_with(what))
                    .nth(n)
                    .map(|(i, _)| i)
                    .unwrap()
            };
            let created = at("rename config.json", 0);
            let running = at("rename config.json", 1);
            let exited = at("rename config.json", if auto_remove { 1 } else { 2 });
            let set_aside = at("rename .life.removing", 0);
            let _ = std::fs::remove_dir_all(home.join("containers"));
            for crash in 0..=steps.len() {
                let _ = std::fs::remove_dir_all(home.join("containers"));
                let mut r = open(&home);
                made(&mut r, container("other", "other", State::Exited));
                drop(r);
                let disk = Faulty::default();
                let r = faulty(&home, &disk);
                let opened = disk.ops.load(Ordering::SeqCst);
                drop(r);
                let disk = Faulty::at(opened + crash, true);
                let mut r =
                    Registry::open_on(home.join("containers"), &disk, &mut |n| panic!("{n}")).unwrap();
                live(&mut r, &disk, auto_remove);
                drop(r);
                let again = open(&home);
                let what = format!("--rm {auto_remove}, a crash at step {crash} of {steps:?}");
                assert!(again.get("other").is_some(), "{what}");
                let life = again.get("life");
                // A `--rm` container whose run no daemon follows goes, as dockerd removes
                // one it restores (moby daemon/daemon.go restore).
                if auto_remove || crash <= created || crash > set_aside {
                    assert!(life.is_none(), "{what}: {life:?}");
                } else {
                    let life = life.unwrap_or_else(|| panic!("{what}: not seen"));
                    let seen = match crash {
                        n if n <= running => (State::Created, Some(255)),
                        n if n <= exited => (State::Exited, Some(255)),
                        _ => (State::Exited, Some(3)),
                    };
                    assert_eq!((life.state, life.exit_code), seen, "{what}");
                }
                let left: Vec<String> = Real
                    .list(&home.join("containers"))
                    .unwrap()
                    .into_iter()
                    .filter(|n| n.starts_with('.'))
                    .collect();
                assert!(left.is_empty(), "{what}: {left:?}");
                assert!(!home.join("containers/life/config.json.new").exists(), "{what}");
            }
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    /// A filesystem as a power loss leaves it, after the abstract persistence model of
    /// ALICE (Pillai et al., "All File Systems Are Not Created Equal", OSDI 2014), taken at
    /// its weakest: an operation is on stable storage once a sync of the directory whose
    /// entry it changes follows it; any others may be, or not, in any combination, applied
    /// in order where they still can be; and a file written without a sync of its own may
    /// hold what was written, or nothing.
    #[derive(Debug, Default)]
    struct Lossy {
        model: Mutex<Model>,
    }

    #[derive(Debug, Default, Clone)]
    struct Model {
        /// What processes see.
        seen: Tree,
        /// What was on stable storage when the operations began.
        stable: Tree,
        ops: Vec<Op>,
    }

    type Tree = BTreeMap<PathBuf, Option<Vec<u8>>>;

    #[derive(Debug, Clone)]
    enum Op {
        CreateDir(PathBuf),
        Write(PathBuf, Vec<u8>),
        Rename(PathBuf, PathBuf),
        RemoveFile(PathBuf),
        RemoveDirAll(PathBuf),
        SyncDir(PathBuf),
    }

    impl Op {
        /// The directories whose entries it changes.
        fn parents(&self) -> Vec<&Path> {
            let parent = |p: &Path| p.parent().map(Path::to_path_buf);
            let _ = parent;
            match self {
                Op::CreateDir(p) | Op::Write(p, _) | Op::RemoveFile(p) | Op::RemoveDirAll(p) => {
                    p.parent().into_iter().collect()
                }
                Op::Rename(from, to) => from.parent().into_iter().chain(to.parent()).collect(),
                Op::SyncDir(_) => Vec::new(),
            }
        }
    }

    /// Applies `op` to `tree` where it can apply: its source and its target's directory
    /// are there. A write lands whole, or empty if `empty`.
    fn apply(tree: &mut Tree, op: &Op, empty: bool) -> bool {
        let has_dir = |t: &Tree, p: &Path| p.parent().is_none_or(|d| matches!(t.get(d), Some(None)));
        match op {
            Op::CreateDir(p) => {
                if !has_dir(tree, p) {
                    return false;
                }
                tree.entry(p.clone()).or_insert(None);
            }
            Op::Write(p, bytes) => {
                if !has_dir(tree, p) || matches!(tree.get(p), Some(None)) {
                    return false;
                }
                tree.insert(p.clone(), Some(if empty { Vec::new() } else { bytes.clone() }));
            }
            Op::Rename(from, to) => {
                if !tree.contains_key(from) || !has_dir(tree, to) || matches!(tree.get(to), Some(None)) {
                    return false;
                }
                let moved: Vec<(PathBuf, Option<Vec<u8>>)> = tree
                    .range(from.clone()..)
                    .take_while(|(k, _)| k.starts_with(from))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (k, v) in moved {
                    tree.remove(&k);
                    let rest = k.strip_prefix(from).unwrap();
                    let at = if rest.as_os_str().is_empty() {
                        to.clone()
                    } else {
                        to.join(rest)
                    };
                    tree.insert(at, v);
                }
            }
            Op::RemoveFile(p) => {
                if !matches!(tree.get(p), Some(Some(_))) {
                    return false;
                }
                tree.remove(p);
            }
            Op::RemoveDirAll(p) => {
                if !tree.contains_key(p) {
                    return false;
                }
                let gone: Vec<PathBuf> = tree
                    .range(p.clone()..)
                    .take_while(|(k, _)| k.starts_with(p))
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in gone {
                    tree.remove(&k);
                }
            }
            Op::SyncDir(_) => {}
        }
        true
    }

    impl Lossy {
        fn with(tree: Tree) -> Lossy {
            Lossy {
                model: Mutex::new(Model {
                    seen: tree.clone(),
                    stable: tree,
                    ops: Vec::new(),
                }),
            }
        }

        fn run(&self, op: Op) -> io::Result<()> {
            let mut m = self.model.lock().unwrap();
            if !apply(&mut m.seen, &op, false) {
                return Err(io::ErrorKind::NotFound.into());
            }
            m.ops.push(op);
            Ok(())
        }

        /// Every state a power loss after the first `k` operations may leave.
        fn after_power_loss(&self, k: usize, each: &mut dyn FnMut(Tree)) {
            let m = self.model.lock().unwrap();
            let ops = &m.ops[..k];
            let mut forced = vec![false; k];
            for (j, op) in ops.iter().enumerate() {
                if let Op::SyncDir(d) = op {
                    for (i, earlier) in ops[..j].iter().enumerate() {
                        if earlier.parents().contains(&d.as_path()) {
                            forced[i] = true;
                        }
                    }
                }
            }
            let free: Vec<usize> = (0..k)
                .filter(|&i| !forced[i] && !matches!(ops[i], Op::SyncDir(_)))
                .collect();
            let writes: Vec<usize> = (0..k).filter(|&i| matches!(ops[i], Op::Write(..))).collect();
            assert!(free.len() <= 16 && writes.len() <= 8, "too many states");
            for chosen in 0u32..1 << free.len() {
                for empties in 0u32..1 << writes.len() {
                    let mut tree = m.stable.clone();
                    for (i, op) in ops.iter().enumerate() {
                        let kept = forced[i]
                            || free
                                .iter()
                                .position(|&f| f == i)
                                .is_some_and(|b| chosen & (1 << b) != 0);
                        let empty = writes
                            .iter()
                            .position(|&w| w == i)
                            .is_some_and(|b| empties & (1 << b) != 0);
                        if kept {
                            apply(&mut tree, op, empty);
                        }
                    }
                    each(tree);
                }
            }
        }
    }

    impl Disk for Lossy {
        fn create_dir(&self, dir: &Path) -> io::Result<()> {
            if matches!(self.model.lock().unwrap().seen.get(dir), Some(None)) {
                return Ok(());
            }
            self.run(Op::CreateDir(dir.to_path_buf()))
        }
        fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
            let m = self.model.lock().unwrap();
            if !matches!(m.seen.get(dir), Some(None)) {
                return Err(io::ErrorKind::NotFound.into());
            }
            Ok(m.seen
                .keys()
                .filter(|k| k.parent() == Some(dir))
                .filter_map(|k| k.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect())
        }
        fn read(&self, path: &Path, _max: u64) -> io::Result<Vec<u8>> {
            match self.model.lock().unwrap().seen.get(path) {
                Some(Some(bytes)) => Ok(bytes.clone()),
                _ => Err(io::ErrorKind::NotFound.into()),
            }
        }
        fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            self.run(Op::Write(path.to_path_buf(), bytes.to_vec()))
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.run(Op::Rename(from.to_path_buf(), to.to_path_buf()))
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.run(Op::RemoveFile(path.to_path_buf()))
        }
        fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
            self.run(Op::RemoveDirAll(path.to_path_buf()))
        }
        fn sync_dir(&self, dir: &Path) -> io::Result<()> {
            self.run(Op::SyncDir(dir.to_path_buf()))
        }
    }

    /// A power loss anywhere in a container's life, and in the life of the next container
    /// to take its name, leaves what the next start reconciles, in every state the model
    /// allows (audit A15): the start succeeds; what it shows was written whole; a container
    /// whose removal was made durable never comes back, so no two containers share a name;
    /// the containers that were on stable storage stay; and nothing is left over.
    #[test]
    fn a_power_loss_anywhere_leaves_what_a_start_reconciles() {
        let root = PathBuf::from("/model/containers");
        let mut base = Tree::new();
        base.insert(PathBuf::from("/model"), None);
        // `other` was created long enough ago to be on stable storage.
        let setup = Lossy::with(base);
        let mut r = Registry::open_on(root.clone(), &setup, &mut |n| panic!("{n}")).unwrap();
        setup.create_dir(&r.dir("other")).unwrap();
        create(&mut r, &setup, container("other", "other", State::Exited)).unwrap();
        let stable = setup.model.lock().unwrap().seen.clone();

        let disk = Lossy::with(stable.clone());
        let mut r = Registry::open_on(root.clone(), &disk, &mut |n| panic!("{n}")).unwrap();
        let mut written: Vec<(String, Vec<u8>)> = Vec::new();
        let mut keep = |r: &Registry, id: &str| {
            written.push((id.to_string(), serde_json::to_vec(r.get(id).unwrap()).unwrap()));
        };
        let life = |id: &str| Container {
            started: None,
            ..container(id, "life", State::Created)
        };
        disk.create_dir(&r.dir("one")).unwrap();
        create(&mut r, &disk, life("one")).unwrap();
        keep(&r, "one");
        r.update(&disk, "one", |c| c.state = State::Running).unwrap();
        keep(&r, "one");
        r.update(&disk, "one", |c| {
            c.state = State::Exited;
            c.exit_code = Some(3);
        })
        .unwrap();
        keep(&r, "one");
        let removal = r.remove(&disk, "one").unwrap().unwrap();
        removal.sync(&disk).unwrap();
        let durable = disk.model.lock().unwrap().ops.len();
        r.release("one");
        removal.delete(&disk).unwrap();
        disk.create_dir(&r.dir("two")).unwrap();
        create(&mut r, &disk, life("two")).unwrap();
        keep(&r, "two");
        let ops = disk.model.lock().unwrap().ops.clone();

        let mut states = 0;
        for k in 0..=ops.len() {
            disk.after_power_loss(k, &mut |tree| {
                states += 1;
                let what = format!("a power loss after {k} of {ops:?}\nleaving {tree:?}");
                // A record torn whose next version is whole: the container is the next
                // version's.
                let whole = |path: PathBuf, id: &str| {
                    tree.get(&path)
                        .and_then(|b| b.as_ref())
                        .and_then(|b| serde_json::from_slice::<Container>(b).ok())
                        .is_some_and(|c| c.id == id)
                };
                let recovered: Vec<String> = ["one", "two"]
                    .into_iter()
                    .filter(|id| {
                        let dir = root.join(id);
                        matches!(tree.get(&dir.join(RECORD)), Some(Some(_)))
                            && !whole(dir.join(RECORD), id)
                            && whole(dir.join(NEW_RECORD), id)
                    })
                    .map(str::to_string)
                    .collect();
                let lossy = Lossy::with(tree.clone());
                let mut notes = Vec::new();
                let again = Registry::open_on(root.clone(), &lossy, &mut |n| notes.push(n))
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
                assert!(again.get("other").is_some(), "{what}");
                for id in &recovered {
                    assert!(again.get(id).is_some(), "{what}: {id} not recovered");
                }
                let mut names: Vec<&str> = again.all().map(|c| c.name.as_str()).collect();
                names.sort_unstable();
                let count = names.len();
                names.dedup();
                assert_eq!(names.len(), count, "{what}: names shared");
                if k >= durable {
                    assert!(again.get("one").is_none(), "{what}: a durable removal undone");
                }
                // What it shows was written whole: its record, reconciled, came from one.
                for c in again.all().filter(|c| c.id != "other") {
                    let seen = serde_json::to_vec(&Container {
                        exit_code: None,
                        finished: None,
                        ..c.clone()
                    })
                    .unwrap();
                    assert!(
                        written.iter().any(|(id, w)| {
                            let w: Container = serde_json::from_slice(w).unwrap();
                            id == &c.id
                                && serde_json::to_vec(&Container {
                                    exit_code: None,
                                    finished: None,
                                    state: c.state,
                                    ..w
                                })
                                .unwrap()
                                    == seen
                        }),
                        "{what}: {c:?}"
                    );
                }
                // Nothing is left over but in a directory left as it is, and noted.
                let left_as_is: Vec<PathBuf> = notes
                    .iter()
                    .filter(|n| n.ends_with("left as it is"))
                    .filter_map(|n| n.split(": ").next().map(PathBuf::from))
                    .collect();
                let left = lossy.model.lock().unwrap().seen.clone();
                for path in left.keys() {
                    let name = path.file_name().unwrap().to_string_lossy();
                    let noted = path.parent().is_some_and(|d| left_as_is.iter().any(|l| l == d));
                    assert!(
                        !name.ends_with(".removing") && (name != NEW_RECORD || noted),
                        "{what}: {path:?} left over"
                    );
                }
                for note in &notes {
                    assert!(note.contains("cannot be read"), "{what}: {note}");
                }
            });
        }
        assert!(states > 1000, "{states} states");
    }
}

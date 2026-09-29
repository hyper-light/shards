//! Containers, as `docker run` leaves them (docs/design/architecture.md D27): each run is
//! one, with an ID and a name, running until its command ends, then exited until
//! `shards rm` removes it, or at once with `--rm`. The daemon keeps them, and writes each
//! to `containers/ID/config.json` in the home, so that exited ones outlive it.

use std::collections::BTreeMap;
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

/// The daemon's containers, by ID.
#[derive(Debug, Default)]
pub struct Registry {
    root: PathBuf,
    by_id: BTreeMap<String, Container>,
}

impl Registry {
    /// The containers kept under `containers` in `home`, as dockerd restores its own when
    /// it starts (moby daemon/daemon.go restore). One left running by a daemon that is
    /// gone can no longer be followed: it exited, with 255 for the status nobody saw. A
    /// `--rm` one that no longer runs goes. A directory with no record is a spare a daemon
    /// made and never used, and goes too.
    pub fn open(home: &Path) -> io::Result<Registry> {
        let root = home.join("containers");
        shards_vmm::platform::create_private_dir(&root)?;
        let mut by_id = BTreeMap::new();
        for entry in std::fs::read_dir(&root)? {
            let dir = entry?.path();
            let Ok(bytes) = std::fs::read(dir.join("config.json")) else {
                let _ = std::fs::remove_dir_all(&dir);
                continue;
            };
            let Ok(mut c) = serde_json::from_slice::<Container>(&bytes) else {
                continue;
            };
            if c.auto_remove {
                let _ = std::fs::remove_dir_all(&dir);
                continue;
            }
            let lost = c.state == State::Running;
            if lost {
                c.state = State::Exited;
                c.exit_code = Some(255);
                c.finished = c.finished.or(Some(now()));
            }
            by_id.insert(c.id.clone(), c);
        }
        let registry = Registry { root, by_id };
        let lost: Vec<String> = registry
            .by_id
            .values()
            .filter(|c| c.exit_code == Some(255) && c.state == State::Exited)
            .map(|c| c.id.clone())
            .collect();
        for id in lost {
            registry.save(&id)?;
        }
        Ok(registry)
    }

    pub fn get(&self, id: &str) -> Option<&Container> {
        self.by_id.get(id)
    }

    pub fn all(&self) -> impl Iterator<Item = &Container> {
        self.by_id.values()
    }

    pub fn name_taken(&self, name: &str) -> Option<&Container> {
        self.by_id.values().find(|c| c.name == name)
    }

    /// Adds `c` without writing it yet: a run's request path only reserves its name.
    pub fn reserve(&mut self, c: Container) {
        self.by_id.insert(c.id.clone(), c);
    }

    /// Changes the container with `id` by `f`, and writes it. A container with no record
    /// is an error: whoever changes one owns it until it goes (audit A06).
    pub fn update(&mut self, id: &str, f: impl FnOnce(&mut Container)) -> io::Result<()> {
        let c = self
            .by_id
            .get_mut(id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no record of the container"))?;
        f(c);
        self.save(id)
    }

    /// Writes the container with `id` to its directory, replacing what was there at once.
    fn save(&self, id: &str) -> io::Result<()> {
        let Some(c) = self.by_id.get(id) else {
            return Ok(());
        };
        let dir = self.root.join(id);
        shards_vmm::platform::create_private_dir(&dir)?;
        let bytes = serde_json::to_vec(c).map_err(io::Error::other)?;
        let temp = dir.join("config.json.new");
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, dir.join("config.json"))
    }

    /// Removes the container with `id`, and everything kept for it.
    pub fn remove(&mut self, id: &str) -> io::Result<Option<Container>> {
        let removed = self.by_id.remove(id);
        if removed.is_some() {
            match std::fs::remove_dir_all(self.root.join(id)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(removed)
    }

    /// The directory kept for the container with `id`.
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shards-containers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        }
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
        let mut registry = Registry::open(&home).unwrap();
        let e = registry.update("gone", |c| c.exit_code = Some(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        registry.reserve(container("here", "here", State::Created));
        registry.update("here", |c| c.exit_code = Some(1)).unwrap();
        assert_eq!(registry.get("here").unwrap().exit_code, Some(1));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn containers_outlive_their_daemon_and_running_ones_end() {
        let home = temp_home("reopen");
        let mut r = Registry::open(&home).unwrap();
        let removed = Container {
            auto_remove: true,
            ..container("cc", "three", State::Exited)
        };
        for c in [
            container("aa", "one", State::Exited),
            container("bb", "two", State::Running),
            removed,
        ] {
            let id = c.id.clone();
            r.reserve(c);
            r.update(&id, |_| {}).unwrap();
        }
        let again = Registry::open(&home).unwrap();
        assert_eq!(again.get("aa"), r.get("aa"));
        let two = again.get("bb").unwrap();
        assert_eq!(
            (two.state, two.exit_code),
            (State::Exited, Some(255)),
            "a run no daemon follows cannot be running"
        );
        assert!(two.finished.is_some());
        assert!(again.get("cc").is_none(), "--rm containers go");
        assert!(!home.join("containers/cc").exists());
        assert_eq!(
            Registry::open(&home).unwrap().get("bb"),
            Some(two),
            "the lost run's end is kept"
        );
        assert!(again.name_taken("one").is_some());
        let mut again = again;
        assert!(again.remove("aa").unwrap().is_some());
        assert!(!home.join("containers/aa").exists());
        assert!(Registry::open(&home).unwrap().get("aa").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }
}

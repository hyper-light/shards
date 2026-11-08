//! What a destination has shown it takes of a message, as shards runs (D163): the longest
//! message, as written, it took, and the shortest it refused (HEC's 413, a forward input's
//! missing ack, a receiver that ends its connection on a message it will not take). Where
//! no protocol states a bound, a driver starts from its receiver's documented one and
//! narrows it by these answers, its next message's most between the two, so that within a
//! handful of long lines it is what the destination takes. Kept per destination in a file
//! of the home (`log-bounds/`), so that a daemon started again, or another run sent to the
//! same place, starts from what was learned. Only a refusal narrows it: nothing a receiver
//! does says it takes more than it took, and the file, removed, starts it again.

use std::io;
use std::path::{Path, PathBuf};

/// The directory of the home the bounds are kept in.
const DIR: &str = "log-bounds";

#[derive(Debug)]
pub(super) struct Learned {
    /// Its file, where it is kept; none for one kept nowhere (tests).
    path: Option<PathBuf>,
    /// The bound its driver starts from: its receiver's documented one.
    stated: usize,
    /// The longest message the destination took since the shortest refused was found.
    took: usize,
    /// The shortest message it refused.
    refused: Option<usize>,
}

impl Learned {
    /// What `home` has kept of the destination `driver` sends to at `address`, a driver
    /// whose receiver's documented bound is `stated`; that bound where nothing is kept.
    pub(super) fn open(home: &Path, driver: &str, address: &str, stated: usize) -> Learned {
        let name: String = format!("{driver}-{address}")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = home.join(DIR).join(name);
        let (took, refused) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| {
                let mut words = text.split_whitespace();
                let took = words.next()?.parse().ok()?;
                let refused = words.next()?.parse().ok()?;
                Some((took, Some(refused)))
            })
            .unwrap_or((0, None));
        Learned {
            path: Some(path),
            stated,
            took,
            refused,
        }
    }

    /// One kept nowhere, from `stated`.
    #[cfg(test)]
    pub(super) fn new(stated: usize) -> Learned {
        Learned {
            path: None,
            stated,
            took: 0,
            refused: None,
        }
    }

    /// The most a message may take, as written, to be tried next: the stated bound until a
    /// message is refused; then the longest taken where nothing lies between it and the
    /// shortest refused, else halfway between.
    pub(super) fn most(&self) -> usize {
        match self.refused {
            None => self.stated,
            Some(r) if r <= self.took.saturating_add(1) => self.took.max(1),
            Some(r) => self.took + (r - self.took) / 2,
        }
    }

    /// The longest message the destination took that is known to be past nothing refused.
    pub(super) fn took(&self) -> usize {
        self.took
    }

    /// The destination took a message of `bytes`.
    pub(super) fn taken(&mut self, bytes: usize) {
        if self.refused.is_some_and(|r| bytes < r) && bytes > self.took {
            self.took = bytes;
            self.keep();
        }
    }

    /// The destination refused a message of `bytes`: what it took before, where no longer
    /// than this, still stands; where longer, it is forgotten (the receiver's bound has
    /// come down), and learned again.
    pub(super) fn refused(&mut self, bytes: usize) {
        if bytes <= self.took {
            self.took = 0;
        }
        self.refused = Some(self.refused.map_or(bytes, |r| r.min(bytes)));
        self.keep();
    }

    /// Writes what is learned to its file, whole or not at all (a file of its own,
    /// renamed over the last). One that cannot be written leaves the last: the daemon's
    /// next start learns it again.
    fn keep(&self) {
        let (Some(path), Some(refused)) = (&self.path, self.refused) else {
            return;
        };
        let write = || -> io::Result<()> {
            let dir = path.parent().ok_or_else(|| io::Error::other("no directory"))?;
            std::fs::create_dir_all(dir)?;
            let new = path.with_extension("new");
            std::fs::write(&new, format!("{} {refused}\n", self.took))?;
            std::fs::rename(&new, path)
        };
        let _ = write();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Do: answer a destination's bound as a receiver that takes 6,000 bytes would, each
    /// message the most it says; open it again from its home; then answer as one whose
    /// bound came down to 2,000. Expect the stated bound first, then within a handful of
    /// refusals exactly 6,000, kept and read back as such; then 2,000.
    #[test]
    fn a_destinations_bound_is_learned_from_its_refusals_and_kept() {
        let home = shards_testdir::TempDir::new("learned").unwrap();
        let mut l = Learned::open(&home, "gelf", "tcp://h:12201", 2 << 20);
        assert_eq!(l.most(), 2 << 20);
        let answer = |l: &mut Learned, takes: usize| {
            let mut refusals = 0;
            for _ in 0..64 {
                let n = l.most();
                if n > takes {
                    l.refused(n);
                    refusals += 1;
                } else {
                    l.taken(n);
                }
            }
            refusals
        };
        assert!(answer(&mut l, 6000) <= 24);
        assert_eq!(l.most(), 6000);
        let again = Learned::open(&home, "gelf", "tcp://h:12201", 2 << 20);
        assert_eq!((again.most(), again.took()), (6000, 6000));
        // Another destination is its own.
        assert_eq!(
            Learned::open(&home, "gelf", "tcp://other:1", 2 << 20).most(),
            2 << 20
        );
        let mut l = again;
        answer(&mut l, 2000);
        assert_eq!(l.most(), 2000);
    }
}

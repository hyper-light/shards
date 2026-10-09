//! Sigstore's trusted root as BuildKit's policy helpers get it (policy-helpers
//! roots/roots.go TrustProvider over sigstore-go pkg/tuf): a cache seeded from the root
//! this binary carries, read from the cache first, refreshed from the repository, with
//! policy-helpers' fetcher, which keeps every root it downloads so that a later offline
//! start can still rotate through them.

use std::path::{Path, PathBuf};

use crate::Error;
use crate::updater::{Fetch, FetchError, Updater};

/// Sigstore's public-good TUF repository (sigstore-go DefaultMirror).
pub const SIGSTORE: &str = "https://tuf-repo-cdn.sigstore.dev";

/// The repository's files this binary carries (policy-helpers roots/tuf-root), each
/// placed in a fresh cache.
pub const EMBEDDED: &[(&str, &[u8])] = &[
    ("root.json", include_bytes!("../roots/sigstore/root.json")),
    (
        "timestamp.json",
        include_bytes!("../roots/sigstore/timestamp.json"),
    ),
    ("snapshot.json", include_bytes!("../roots/sigstore/snapshot.json")),
    ("targets.json", include_bytes!("../roots/sigstore/targets.json")),
    (
        "targets/trusted_root.json",
        include_bytes!("../roots/sigstore/targets/trusted_root.json"),
    ),
];

/// URLToPath: the cache directory's name for a repository.
pub fn url_to_path(url: &str) -> String {
    let u = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    u.replace(['/', ':'], "-").to_lowercase()
}

/// policy-helpers' airgappedFetcher: online, every root version downloaded is kept under
/// `roots/`; offline, the cached timestamp and kept roots are served, and nothing else is
/// there (404).
pub struct Airgapped<'f> {
    pub base: String,
    pub cache: PathBuf,
    pub online: &'f dyn Fetch,
    pub is_online: bool,
}

impl std::fmt::Debug for Airgapped<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Airgapped").field("base", &self.base).finish()
    }
}

impl Fetch for Airgapped<'_> {
    fn fetch(&self, url: &str, max: u64) -> Result<Vec<u8>, FetchError> {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let base = path.rsplit('/').next().unwrap_or(path);
        if self.is_online {
            let data = self.online.fetch(url, max)?;
            if base.ends_with(".root.json") {
                let dir = self.cache.join("roots");
                std::fs::create_dir_all(&dir)
                    .and_then(|()| std::fs::write(dir.join(base), &data))
                    .map_err(|e| {
                        FetchError::Other(format!("caching root file in trust provider cache: {e}"))
                    })?;
            }
            return Ok(data);
        }
        if url == format!("{}/timestamp.json", self.base)
            && let Ok(data) = std::fs::read(self.cache.join("timestamp.json"))
        {
            return Ok(data);
        }
        if base.ends_with(".root.json")
            && url == format!("{}/{base}", self.base)
            && let Ok(data) = std::fs::read(self.cache.join("roots").join(base))
        {
            return Ok(data);
        }
        Err(FetchError::Status {
            url: String::new(),
            code: 404,
        })
    }
}

/// How the trusted root was had (roots.Status): when the repository was last read, or why
/// it could not be.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub error: Option<String>,
    /// Seconds since 1970, UTC.
    pub last_updated: Option<i64>,
}

fn seed(cache: &Path) -> Result<(), Error> {
    if std::fs::symlink_metadata(cache.join("root.json")).is_ok() {
        return Ok(());
    }
    for (name, data) in EMBEDDED {
        let path = cache.join(name);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::Io(format!("{}: {e}", dir.display())))?;
        }
        std::fs::write(&path, data).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    }
    Ok(())
}

/// sigstore-go's tuf.New with ForceCache: the cache alone if it is still trusted, the
/// repository where not, each failure in its words.
fn client<'f>(cache: &Path, fetch: &'f dyn Fetch, now: (i64, u32)) -> Result<Updater<'f>, Error> {
    let root = EMBEDDED.first().map(|(_, d)| *d).unwrap_or_default();
    let mut local = Updater::new(root, cache, SIGSTORE, fetch, true, now)
        .map_err(|e| Error::Other(format!("failed to create initial TUF updater: {e}")))?;
    if local.refresh().is_ok() {
        return Ok(local);
    }
    online(cache, fetch, now).map_err(|e| Error::Other(format!("failed to load metadata: {e}")))
}

/// Client.Refresh: a new updater, online.
fn online<'f>(cache: &Path, fetch: &'f dyn Fetch, now: (i64, u32)) -> Result<Updater<'f>, Error> {
    let root = EMBEDDED.first().map(|(_, d)| *d).unwrap_or_default();
    let mut u = Updater::new(root, cache, SIGSTORE, fetch, false, now)
        .map_err(|e| Error::Other(format!("failed to create tuf updater: {e}")))?;
    u.refresh()
        .map_err(|e| Error::Other(format!("tuf refresh failed: {e}")))?;
    Ok(u)
}

/// TrustProvider.TrustedRoot: `trusted_root.json` from Sigstore's repository by way of
/// the cache under `cache_path` (the state directory's `tuf`), and how it was had. As the
/// provider starts: the cache, or the repository, or failing both the cache offline;
/// then its update, the repository read again, whose failure is the status's while the
/// client it started with serves. buildx waits five seconds for the update before it
/// serves from the cache; shards waits for it to end, which the fetcher bounds (D104).
pub fn trusted_root(
    cache_path: &Path,
    fetch: &dyn Fetch,
    now: (i64, u32),
) -> (Result<Vec<u8>, Error>, Status) {
    let cache = cache_path.join(url_to_path(SIGSTORE));
    let mut status = Status::default();
    if let Err(e) = std::fs::create_dir_all(&cache) {
        return (
            Err(Error::Io(format!(
                "creating cache directory for trust provider: {e}"
            ))),
            status,
        );
    }
    let _lock = match lock(cache_path) {
        Ok(l) => l,
        Err(e) => return (Err(e), status),
    };
    if let Err(e) = seed(&cache) {
        return (
            Err(Error::Io(format!(
                "initializing cache directory for trust provider with embedded root: {e}"
            ))),
            status,
        );
    }
    let air = |is_online| Airgapped {
        base: SIGSTORE.to_string(),
        cache: cache.clone(),
        online: fetch,
        is_online,
    };
    let (on, off) = (air(true), air(false));
    let started = match client(&cache, &on, now) {
        Ok(u) => u,
        Err(_) => match client(&cache, &off, now) {
            Ok(u) => u,
            Err(e) => return (Err(e), status),
        },
    };
    let mut current = match client(&cache, &on, now).and_then(|_| online(&cache, &on, now)) {
        Ok(u) => {
            status.last_updated = Some(now.0);
            u
        }
        Err(e) => {
            status.error = Some(e.to_string());
            started
        }
    };
    let target = match current.target_info("trusted_root.json") {
        Ok(t) => t,
        Err(e) => {
            return (
                Err(Error::Other(format!(
                    "getting info for target \"trusted_root.json\": {e}"
                ))),
                status,
            );
        }
    };
    if let Some(data) = current.cached_target(&target) {
        return (Ok(data), status);
    }
    match current.download_target(&target) {
        Ok(data) => (Ok(data), status),
        Err(e) => (
            Err(Error::Other(format!(
                "failed to download target file trusted_root.json - {e}"
            ))),
            status,
        ),
    }
}

/// The provider's lock on its cache (flock of `.lock`), held while it is read and written.
fn lock(cache_path: &Path) -> Result<std::fs::File, Error> {
    let path = cache_path.join(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| Error::Io(format!("acquiring lock on trust provider cache: {e}")))?;
    file.lock()
        .map_err(|e| Error::Io(format!("acquiring lock on trust provider cache: {e}")))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A repository nobody can reach.
    struct Unreachable;

    impl Fetch for Unreachable {
        fn fetch(&self, url: &str, _max: u64) -> Result<Vec<u8>, FetchError> {
            Err(FetchError::Other(format!("Get \"{url}\": unreachable")))
        }
    }

    fn when(rfc3339: &str) -> (i64, u32) {
        shards_dockerfile::go::parse_rfc3339(rfc3339.as_bytes())
            .unwrap()
            .unix()
    }

    /// The root this binary carries verifies itself, and its timestamp, snapshot and
    /// targets chain to the trusted root it carries, while that timestamp is current:
    /// from the cache alone, the repository unreachable, which the status says.
    #[test]
    fn the_carried_root_serves_its_trusted_root_while_current() {
        let dir = std::env::temp_dir().join(format!("shards-tuf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (got, status) = trusted_root(&dir, &Unreachable, when("2026-09-01T00:00:00Z"));
        let want = EMBEDDED
            .iter()
            .find(|(n, _)| *n == "targets/trusted_root.json")
            .unwrap()
            .1;
        assert_eq!(got.unwrap(), want);
        assert!(status.error.unwrap().contains("unreachable"));
        assert_eq!(status.last_updated, None);
        // Past its timestamp's expiry, the cache alone serves nothing: offline, the
        // snapshot that timestamp names is not there to fetch (go-tuf's own words, the
        // offline fetcher's 404 naming no URL).
        let (got, _) = trusted_root(&dir, &Unreachable, when("2026-10-09T00:00:00Z"));
        assert_eq!(
            got.unwrap_err().to_string(),
            "failed to load metadata: tuf refresh failed: failed to download , http status code: 404"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

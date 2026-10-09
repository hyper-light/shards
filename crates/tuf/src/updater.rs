//! The TUF client workflow as go-tuf v2's updater runs it (updater/updater.go): root
//! versions in turn, then timestamp, snapshot and targets, each from the local cache where
//! it is still trusted and from the repository where not, then a target found by
//! preorder depth-first search of delegations, its length and hashes checked. And the
//! clients over it: sigstore-go's (pkg/tuf/client.go: the cache first, the repository on
//! a refresh) and policy-helpers' offline fetcher (roots/roots.go airgappedFetcher).

use std::path::{Path, PathBuf};

use crate::Error;
use crate::metadata::{Body, ROOT, SNAPSHOT, TARGETS, TIMESTAMP, TargetFile};
use crate::trusted::{Trusted, target_in_pattern};

/// What a download fails with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// A status other than 200 (ErrDownloadHTTP).
    Status { url: String, code: u16 },
    /// More than the most allowed (ErrDownloadLengthMismatch).
    TooLong { url: String, length: u64, max: u64 },
    /// Anything else: the transport's own words.
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Status { url, code } => {
                write!(f, "failed to download {url}, http status code: {code}")
            }
            FetchError::TooLong { url, length, max } => write!(
                f,
                "download length mismatch error: download failed for {url}, length {length} is larger than expected {max}"
            ),
            FetchError::Other(s) => f.write_str(s),
        }
    }
}

/// Downloads a URL's body of at most `max` bytes (DefaultFetcher.DownloadFile).
pub trait Fetch {
    fn fetch(&self, url: &str, max: u64) -> Result<Vec<u8>, FetchError>;
}

/// UpdaterConfig's limits.
pub const MAX_ROOT_ROTATIONS: i64 = 256;
pub const MAX_DELEGATIONS: usize = 32;
pub const ROOT_MAX: u64 = 512_000;
pub const TIMESTAMP_MAX: u64 = 16_384;
pub const SNAPSHOT_MAX: u64 = 2_000_000;
pub const TARGETS_MAX: u64 = 5_000_000;

/// url.PathEscape of a role's name.
fn path_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b',' | b':' | b'=' | b'@'
            );
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn with_slash(url: &str) -> String {
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

/// An updater: its trusted set, where it caches, and where it fetches from.
pub struct Updater<'f> {
    pub trusted: Trusted,
    metadata_dir: PathBuf,
    targets_dir: PathBuf,
    metadata_url: String,
    targets_url: String,
    fetch: &'f dyn Fetch,
    /// UnsafeLocalMode: the cache alone, no repository.
    local_only: bool,
}

impl std::fmt::Debug for Updater<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Updater")
            .field("metadata_dir", &self.metadata_dir)
            .finish()
    }
}

impl<'f> Updater<'f> {
    /// New: `root` trusted, and written to the cache as root.json.
    pub fn new(
        root: &[u8],
        metadata_dir: &Path,
        remote: &str,
        fetch: &'f dyn Fetch,
        local_only: bool,
        now: (i64, u32),
    ) -> Result<Updater<'f>, Error> {
        let trusted = Trusted::new(root, now)?;
        let u = Updater {
            trusted,
            metadata_dir: metadata_dir.to_path_buf(),
            targets_dir: metadata_dir.join("targets"),
            metadata_url: remote.to_string(),
            targets_url: format!("{}targets", with_slash(remote)),
            fetch,
            local_only,
        };
        std::fs::create_dir_all(&u.metadata_dir)
            .map_err(|e| Error::Io(format!("{}: {e}", u.metadata_dir.display())))?;
        std::fs::create_dir_all(&u.targets_dir)
            .map_err(|e| Error::Io(format!("{}: {e}", u.targets_dir.display())))?;
        u.persist(ROOT, root)?;
        Ok(u)
    }

    fn local(&self, role: &str) -> Option<Vec<u8>> {
        std::fs::read(self.metadata_dir.join(format!("{role}.json"))).ok()
    }

    /// persistMetadata: written whole beside it, then renamed over it, then read back.
    fn persist(&self, role: &str, data: &[u8]) -> Result<(), Error> {
        let name = self.metadata_dir.join(format!("{}.json", path_escape(role)));
        let tmp = self.metadata_dir.join(format!(
            "tuf_tmp{}{:x}",
            std::process::id(),
            aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data)
                .as_ref()
                .first()
                .copied()
                .unwrap_or(0)
        ));
        let io = |e: std::io::Error| Error::Io(format!("{}: {e}", name.display()));
        std::fs::write(&tmp, data).map_err(io)?;
        if let Err(e) = std::fs::rename(&tmp, &name) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e));
        }
        if std::fs::read(&name).map_err(io)? != data {
            return Err(Error::Io("failed to persist metadata".into()));
        }
        Ok(())
    }

    fn download(&self, role: &str, max: u64, version: Option<i64>) -> Result<Vec<u8>, Error> {
        let base = with_slash(&self.metadata_url);
        let url = match version {
            None => format!("{base}{}.json", path_escape(role)),
            Some(v) => format!("{base}{v}.{}.json", path_escape(role)),
        };
        self.fetch.fetch(&url, max).map_err(Error::Fetch)
    }

    /// Refresh: online, or from the cache alone (unsafeLocalRefresh).
    pub fn refresh(&mut self) -> Result<(), Error> {
        if self.local_only {
            let read = |u: &Updater<'_>, role: &str| {
                u.local(role).ok_or_else(|| {
                    Error::Io(format!(
                        "open {}: no such file or directory",
                        u.metadata_dir.join(format!("{role}.json")).display()
                    ))
                })
            };
            let ts = read(self, TIMESTAMP)?;
            self.trusted.update_timestamp(&ts)?;
            let snap = read(self, SNAPSHOT)?;
            self.trusted.update_snapshot(&snap, false)?;
            let targets = read(self, TARGETS)?;
            self.trusted.update_targets(&targets, TARGETS, ROOT)?;
            return Ok(());
        }
        self.load_root()?;
        self.load_timestamp()?;
        self.load_snapshot()?;
        self.load_targets(TARGETS, ROOT)?;
        Ok(())
    }

    fn load_root(&mut self) -> Result<(), Error> {
        let lower = self.trusted.root.signed.version + 1;
        for next in lower..lower + MAX_ROOT_ROTATIONS {
            match self.download(ROOT, ROOT_MAX, Some(next)) {
                Ok(data) => {
                    self.trusted.update_root(&data)?;
                    self.persist(ROOT, &data)?;
                }
                Err(Error::Fetch(FetchError::Status { code: 404, .. })) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn load_timestamp(&mut self) -> Result<(), Error> {
        if let Some(data) = self.local(TIMESTAMP) {
            // A local copy that is not valid (a repository error) is left for the new one;
            // any other failure, as an expired one, is said.
            match self.trusted.update_timestamp(&data) {
                Ok(()) => {}
                Err(e) if e.is_repository() => {}
                Err(e) => return Err(e),
            }
        }
        let data = self.download(TIMESTAMP, TIMESTAMP_MAX, None)?;
        match self.trusted.update_timestamp(&data) {
            Ok(()) => {}
            Err(Error::EqualVersion(_)) => return Ok(()),
            Err(e) => return Err(e),
        }
        self.persist(TIMESTAMP, &data)
    }

    fn load_snapshot(&mut self) -> Result<(), Error> {
        if let Some(data) = self.local(SNAPSHOT) {
            match self.trusted.update_snapshot(&data, true) {
                Ok(()) => return Ok(()),
                Err(e) if e.is_repository() => {}
                Err(e) => return Err(e),
            }
        }
        let Some(ts) = &self.trusted.timestamp else {
            return Err(Error::Other("trusted timestamp not set".into()));
        };
        let meta = ts.signed.meta("snapshot.json").cloned();
        let length = meta.as_ref().map_or(0, |m| m.length);
        let length = if length == 0 {
            SNAPSHOT_MAX
        } else {
            u64::try_from(length).unwrap_or(0)
        };
        let version = self
            .trusted
            .root_consistent()
            .then(|| meta.as_ref().map_or(0, |m| m.version));
        let data = self.download(SNAPSHOT, length, version)?;
        self.trusted.update_snapshot(&data, false)?;
        self.persist(SNAPSHOT, &data)
    }

    fn load_targets(&mut self, role: &str, parent: &str) -> Result<(), Error> {
        if self.trusted.targets_of(role).is_some() {
            return Ok(());
        }
        if let Some(data) = self.local(role) {
            match self.trusted.update_targets(&data, role, parent) {
                Ok(()) => return Ok(()),
                Err(e) if e.is_repository() => {}
                Err(e) => return Err(e),
            }
        }
        let Some(snap) = &self.trusted.snapshot else {
            return Err(Error::Other("trusted snapshot not set".into()));
        };
        let meta = snap
            .signed
            .meta(&format!("{role}.json"))
            .cloned()
            .ok_or_else(|| Error::Other(format!("role {role} not found in snapshot")))?;
        let length = if meta.length == 0 {
            TARGETS_MAX
        } else {
            u64::try_from(meta.length).unwrap_or(0)
        };
        let version = self.trusted.root_consistent().then_some(meta.version);
        let data = self.download(role, length, version)?;
        self.trusted.update_targets(&data, role, parent)?;
        self.persist(role, &data)
    }

    /// GetTargetInfo: the target's description, found by preorder depth-first search of
    /// the delegations, refreshing first if no targets are trusted yet.
    pub fn target_info(&mut self, path: &str) -> Result<TargetFile, Error> {
        if self.trusted.targets_of(TARGETS).is_none() {
            self.refresh()?;
        }
        let mut to_visit: Vec<(String, String)> = vec![(TARGETS.into(), ROOT.into())];
        let mut visited: Vec<String> = Vec::new();
        while visited.len() <= MAX_DELEGATIONS && !to_visit.is_empty() {
            let Some((role, parent)) = to_visit.pop() else {
                break;
            };
            if visited.contains(&role) {
                continue;
            }
            self.load_targets(&role, &parent)?;
            let Some(targets) = self.trusted.targets_of(&role) else {
                break;
            };
            if let Body::Targets {
                targets: files,
                delegations,
            } = &targets.signed.body
            {
                if let Some(found) = files
                    .as_ref()
                    .and_then(|f| f.iter().rev().find(|(k, _)| k == path))
                {
                    return found
                        .1
                        .clone()
                        .ok_or_else(|| Error::Other(format!("target {path} is null")));
                }
                visited.push(role.clone());
                if let Some(d) = delegations {
                    let mut children: Vec<(String, String)> = Vec::new();
                    if let Some(roles) = &d.roles {
                        for r in roles {
                            let delegated = match (&r.paths, &r.path_hash_prefixes) {
                                (Some(paths), _) if !paths.is_empty() => {
                                    paths.iter().any(|p| target_in_pattern(path, p))
                                }
                                (_, Some(prefixes)) if !prefixes.is_empty() => {
                                    use base64::Engine as _;
                                    let h = aws_lc_rs::digest::digest(
                                        &aws_lc_rs::digest::SHA256,
                                        path.as_bytes(),
                                    );
                                    let enc = base64::engine::general_purpose::URL_SAFE.encode(h.as_ref());
                                    prefixes.iter().any(|p| enc.starts_with(p.as_str()))
                                }
                                _ => false,
                            };
                            if delegated {
                                children.push((r.name.clone(), role.clone()));
                                if r.terminating {
                                    to_visit.clear();
                                    break;
                                }
                            }
                        }
                    } else if let Some(s) = &d.succinct {
                        children.push((succinct_role(s.bit_length, &s.name_prefix, path), role.clone()));
                        to_visit.clear();
                    }
                    children.reverse();
                    to_visit.extend(children);
                }
            }
        }
        Err(Error::Other(format!("target {path} not found")))
    }

    fn target_path(&self, t: &TargetFile) -> PathBuf {
        self.targets_dir.join(path_escape(&t.path))
    }

    /// FindCachedTarget: the cached copy, where its length and hashes still check.
    pub fn cached_target(&self, t: &TargetFile) -> Option<Vec<u8>> {
        let data = std::fs::read(self.target_path(t)).ok()?;
        t.verify(&data).ok().map(|()| data)
    }

    /// DownloadTarget: fetched by its hash-prefixed name under consistent snapshots,
    /// checked, then cached.
    pub fn download_target(&self, t: &TargetFile) -> Result<Vec<u8>, Error> {
        let mut remote = t.path.clone();
        if self.trusted.root_consistent()
            && let Some((_, h)) = t.hashes.as_ref().and_then(|h| h.first())
        {
            let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
            remote = match t.path.rsplit_once('/') {
                Some((dir, base)) => format!("{dir}/{hex}.{base}"),
                None => format!("{hex}.{}", t.path),
            };
        }
        let url = format!("{}{remote}", with_slash(&self.targets_url));
        let length = u64::try_from(t.length).unwrap_or(0);
        let data = self.fetch.fetch(&url, length).map_err(Error::Fetch)?;
        t.verify(&data)?;
        let path = self.target_path(t);
        std::fs::write(&path, &data).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        Ok(data)
    }
}

/// SuccinctRoles.GetRolesForTarget: the bin a path's SHA-256 falls in.
fn succinct_role(bit_length: i64, prefix: &str, path: &str) -> String {
    let bits = u32::try_from(bit_length.clamp(1, 32)).unwrap_or(32);
    let bins: u64 = 1u64 << bits;
    let suffix_len = format!("{:x}", bins - 1).len();
    let h = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, path.as_bytes());
    let first = h
        .as_ref()
        .first_chunk::<4>()
        .map_or(0, |b| u32::from_be_bytes(*b));
    let bin = if bits == 32 { first } else { first >> (32 - bits) };
    format!("{prefix}-{bin:0suffix_len$x}")
}

impl Trusted {
    fn root_consistent(&self) -> bool {
        matches!(
            self.root.signed.body,
            Body::Root {
                consistent_snapshot: true,
                ..
            }
        )
    }
}

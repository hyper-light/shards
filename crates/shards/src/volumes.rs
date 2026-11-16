//! Volumes and bind mounts, as dockerd reads and keeps them (moby docker-v29.8.1
//! daemon/volume/mounts/linux_parser.go, validate.go, volume_copy.go; daemon/volumes.go
//! registerMountPoints; daemon/volume/local): `-v`'s binds and volumes, `--mount`'s
//! mounts, `--volumes-from`, and an image's `VOLUME`s, each a mount point; named and
//! anonymous volumes kept in the home, as the local driver keeps them under its root. A
//! microVM reaches each over virtio-fs (D38).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shards_api::volumes::ClearCause;
use shards_cmdline::mounts::{self, Mount, clean};
use shards_image::store::Locked;

/// A mount point of a microVM, in the shape `inspect`'s `Mounts` answers
/// (volumemounts.MountPoint, D38).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountPoint {
    /// `bind`, `volume` or `tmpfs`.
    #[serde(rename = "Type")]
    pub kind: String,
    /// A volume's name.
    #[serde(rename = "Name", default)]
    pub name: String,
    /// The host path: a bind's source, a volume's data.
    #[serde(rename = "Source", default)]
    pub source: String,
    #[serde(rename = "Destination")]
    pub destination: String,
    #[serde(rename = "Driver", default)]
    pub driver: String,
    #[serde(rename = "Mode", default)]
    pub mode: String,
    #[serde(rename = "RW")]
    pub rw: bool,
    #[serde(rename = "Propagation", default)]
    pub propagation: String,
    /// Copy the image's files at the destination into the volume where it is empty.
    #[serde(rename = "CopyData", default)]
    pub copy_data: bool,
    /// The `--mount` it was made of, as the client sent it, or none for `-v`'s.
    #[serde(rename = "Spec", default)]
    pub spec: String,
    /// A bind whose source is made where it is missing (`-v`'s, not `--mount`'s).
    #[serde(rename = "CreateSource", default)]
    pub create_source: bool,
    /// A `--mount type=tmpfs`'s options, as ConvertTmpfsOptions writes them.
    #[serde(rename = "TmpfsOptions", default)]
    pub tmpfs: String,
    /// One of the image's: its agents and harnesses are given it too, as its Agentfile
    /// grants (AGENTFILE_ARCH.md §4.5, D109). Not Docker's: inspect leaves it out.
    #[serde(rename = "ShardsDomains", default)]
    pub domains: bool,
    /// Scoped by its Agentfile (`VOLUME … FOR`): for the agents and harnesses it names
    /// alone, so the run's own root does not mount it.
    #[serde(rename = "ShardsDomainsOnly", default)]
    pub domains_only: bool,
    /// A CDI mount's `nosuid`, `nodev` and `noexec` (D146): made as the run starts, never
    /// kept.
    #[serde(skip)]
    pub flags: Vec<String>,
    /// A client's bind of its own directory, or of a file alone, through its context
    /// (D228): its relay's token, by which the run's start finds the stream the client
    /// serves it on, its `source` the client's path. Not Docker's: inspect leaves it out.
    #[serde(rename = "ShardsRelay", default, skip_serializing_if = "String::is_empty")]
    pub relay: String,
    /// A file a client binds alone: its one name, which its relay serves of its directory.
    #[serde(rename = "ShardsRelayOnly", default, skip_serializing_if = "String::is_empty")]
    pub relay_only: String,
    /// One of CDI's (D146): its host's path is its device's, granted with it (D228).
    #[serde(skip)]
    pub cdi: bool,
}

/// CDI's mounts (D146), as CDI's Apply adds them to a container's spec: each a bind of
/// its host path at its container path, read-only with `ro`, of the propagation its
/// options name (rprivate where none), its `nosuid`, `nodev` and `noexec` the guest's
/// mount's. A microVM's record keeps none of them, so `inspect` lists none (D146).
pub fn cdi_points(given: &[crate::build::cdi::Mount]) -> Result<Vec<MountPoint>, String> {
    given
        .iter()
        .map(|m| {
            if !matches!(m.kind.as_str(), "" | "bind") {
                return Err(format!(
                    "CDI device injection failed: a {} mount at {}, where a microVM's run takes a host's binds alone",
                    m.kind, m.container_path
                ));
            }
            let has = |o: &str| m.options.iter().any(|g| g == o);
            Ok(MountPoint {
                kind: "bind".into(),
                source: m.host_path.clone(),
                destination: m.container_path.clone(),
                rw: !has("ro"),
                propagation: m
                    .options
                    .iter()
                    .find(|o| PROPAGATIONS.contains(&o.as_str()))
                    .cloned()
                    .unwrap_or_else(|| "rprivate".into()),
                flags: ["nosuid", "nodev", "noexec"]
                    .into_iter()
                    .filter(|f| has(f))
                    .map(String::from)
                    .collect(),
                cdi: true,
                ..MountPoint::default()
            })
        })
        .collect()
}

/// Why a run's mount cannot stand in for a volume its image's Agentfile gives its agents
/// and harnesses (D109): what stands in is given to them, and a tmpfs or a file is
/// mounted in the run's own root alone.
fn cannot_stand_in(what: &str, dest: &str) -> String {
    format!(
        "{what} cannot stand in for {dest}: the image's Agentfile gives the volume there to its agents and harnesses, and {what} is not given to them; mount a volume or a directory there (-v or --mount)"
    )
}

fn invalid_spec(spec: &str) -> String {
    format!("invalid volume specification: '{spec}'")
}

fn invalid_mode(mode: &str) -> String {
    format!("invalid mode: {mode}")
}

const PROPAGATIONS: [&str; 6] = ["private", "rprivate", "slave", "rslave", "shared", "rshared"];

/// linuxValidMountMode: rw or ro, a label, a propagation, `nocopy` and a consistency,
/// each once at most; and, past Docker's, Podman's `exec` or `noexec` (D279: whether its
/// files may be run from it), once at most, which Docker's daemon refuses as an invalid mode.
fn valid_mode(mode: &str) -> bool {
    if mode.is_empty() {
        return true;
    }
    let mut counts = [0u32; 6];
    for o in mode.split(',') {
        let slot = if matches!(o, "rw" | "ro") {
            0
        } else if matches!(o, "Z" | "z") {
            1
        } else if PROPAGATIONS.contains(&o) {
            2
        } else if o == "nocopy" {
            3
        } else if matches!(o, "consistent" | "cached" | "delegated") {
            4
        } else if matches!(o, "exec" | "noexec") {
            5
        } else {
            return false;
        };
        if let Some(c) = counts.get_mut(slot) {
            *c += 1;
        }
    }
    counts.iter().all(|&c| c <= 1)
}

fn propagation_of(mode: &str) -> Option<String> {
    mode.split(',')
        .find(|o| PROPAGATIONS.contains(o))
        .map(str::to_string)
}

/// errMountConfig's words: `invalid mount config for type "T": ...`.
fn config_error(kind: &str, why: &str) -> String {
    format!(
        "invalid mount config for type {}: {why}",
        shards_cmdline::go::quote(kind)
    )
}

/// Whose paths a bind's sources are (D207): this host's, checked to be there (`--mount`)
/// or not (`-v`, which makes them), or a microVM's own (D181), Linux's on every host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sources {
    Host { check: bool },
    Microvm,
}

impl Sources {
    /// Whether they are a Windows host's: each from its drive's root (`C:\src`), as moby
    /// reads them for Linux containers on Windows (lcow_parser.go's rxHostDir).
    fn drives(self) -> bool {
        cfg!(windows) && matches!(self, Sources::Host { .. })
    }
}

/// How long the absolute prefix of a Windows drive's path that `s` starts with is: `C:\`
/// or `C:/`, verbatim (`\\?\C:\`) or not.
fn drive_len(s: &str) -> Option<usize> {
    let rest = s.strip_prefix(r"\\?\").unwrap_or(s);
    match rest.as_bytes() {
        [d, b':', b'\\' | b'/', ..] if d.is_ascii_alphabetic() => Some(s.len() - rest.len() + 3),
        _ => None,
    }
}

/// A Windows drive's path as Go's filepath.Clean leaves one there: `\` between names, `.`
/// dropped and `..` taken back, to the drive's root at most. Its case kept: moby's
/// parser lowercases a whole `-v` (windows_parser.go splitRawSpec), and an NTFS
/// directory can tell case (FILE_CS_FLAG_CASE_SENSITIVE_DIR).
fn clean_drive(p: &str) -> String {
    let (root, rest) = p.split_at_checked(drive_len(p).unwrap_or(0)).unwrap_or(("", p));
    let mut names: Vec<&str> = Vec::new();
    for name in rest.split(['\\', '/']) {
        match name {
            "" | "." => {}
            ".." => {
                names.pop();
            }
            name => names.push(name),
        }
    }
    format!("{}{}", root.replace('/', "\\"), names.join("\\"))
}

/// validateMountConfigImpl, for what a mount point needs whatever its kind.
fn validate(m: &Mount, sources: Sources) -> Result<(), String> {
    let fail = |why: &str| Err(config_error(&m.kind, why));
    if m.kind != "bind" && m.bind.is_some() {
        return fail("field BindOptions must not be specified");
    }
    if m.kind != "volume" && m.volume.is_some() {
        return fail("field VolumeOptions must not be specified");
    }
    if m.kind != "image" && m.image.is_some() {
        return fail("field ImageOptions must not be specified");
    }
    if m.kind != "tmpfs" && m.tmpfs.is_some() {
        return fail("field TmpfsOptions must not be specified");
    }
    if m.target.is_empty() {
        return fail("field Target must not be empty");
    }
    if clean(&m.target.replace('\\', "/")) == "/" {
        return fail("invalid specification: destination can't be '/'");
    }
    if !m.target.replace('\\', "/").starts_with('/') {
        return fail(&format!(
            "invalid mount path: '{}' mount path must be absolute",
            m.target.replace('\\', "/")
        ));
    }
    match m.kind.as_str() {
        "bind" => {
            if m.source.is_empty() {
                return fail("field Source must not be empty");
            }
            if let Some(b) = &m.bind
                && !b.propagation.is_empty()
                && !PROPAGATIONS.contains(&b.propagation.as_str())
            {
                return fail(&format!("invalid propagation mode: {}", b.propagation));
            }
            if sources.drives() {
                if drive_len(&m.source).is_none() {
                    return fail(&format!(
                        "invalid mount path: '{}' mount path must be absolute: a path of this Windows host's is one from its drive's root (C:\\src)",
                        m.source
                    ));
                }
            } else if !m.source.replace('\\', "/").starts_with('/') {
                return fail(&format!(
                    "invalid mount path: '{}' mount path must be absolute",
                    m.source.replace('\\', "/")
                ));
            }
            let create = m.bind.as_ref().is_some_and(|b| b.create_mountpoint);
            if sources == (Sources::Host { check: true }) && !create && std::fs::metadata(&m.source).is_err()
            {
                return fail(&format!("bind source path does not exist: {}", m.source));
            }
        }
        "volume" => {
            if let Some(v) = &m.volume
                && !v.subpath.is_empty()
            {
                if m.source.is_empty() {
                    return fail("must not set Subpath when using anonymous volumes");
                }
                if !is_local(&v.subpath) {
                    return fail("subpath must be a relative path within the volume");
                }
            }
        }
        "tmpfs" => {
            if !m.source.is_empty() {
                return fail("field Source must not be specified");
            }
        }
        "image" => {
            if m.source.is_empty() {
                return fail("field Source must not be empty");
            }
        }
        _ => return fail("mount type unknown"),
    }
    Ok(())
}

/// Go's filepath.IsLocal: relative, not empty, and not reaching out with `..`.
fn is_local(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && {
        let c = clean(p);
        c != ".." && !c.starts_with("../")
    }
}

/// ConvertTmpfsOptions: `ro`, `mode=`, `size=` with the largest unit dividing it.
fn tmpfs_options(m: &Mount) -> String {
    let mut out: Vec<String> = Vec::new();
    if m.read_only {
        out.push("ro".into());
    }
    if let Some(t) = &m.tmpfs {
        if t.mode != 0 {
            out.push(format!("mode={:o}", t.mode));
        }
        if t.size_bytes != 0 {
            let (mut size, mut suffix) = (t.size_bytes, "");
            for (s, d) in [("g", 1i64 << 30), ("m", 1 << 20), ("k", 1 << 10)] {
                if size % d == 0 {
                    size /= d;
                    suffix = s;
                    break;
                }
            }
            out.push(format!("size={size}{suffix}"));
        }
    }
    out.join(",")
}

/// parseMountSpec: a validated mount's mount point.
fn mount_point(m: &Mount, sources: Sources) -> Result<MountPoint, String> {
    validate(m, sources)?;
    let mut mp = MountPoint {
        kind: m.kind.clone(),
        rw: !m.read_only,
        destination: clean(&m.target.replace('\\', "/")),
        ..MountPoint::default()
    };
    match m.kind.as_str() {
        "volume" => {
            mp.name = m.source.clone();
            mp.copy_data = !m.volume.as_ref().is_some_and(|v| v.no_copy);
            if let Some((name, _)) = m.volume.as_ref().and_then(|v| v.driver.as_ref()) {
                mp.driver = name.clone();
            }
        }
        "bind" => {
            mp.source = if sources.drives() {
                clean_drive(&m.source)
            } else {
                clean(&m.source.replace('\\', "/"))
            };
            mp.propagation = m
                .bind
                .as_ref()
                .map(|b| b.propagation.clone())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "rprivate".into());
        }
        "tmpfs" => mp.tmpfs = tmpfs_options(m),
        _ => {}
    }
    Ok(mp)
}

/// `-v`'s `[SOURCE:]DEST[:MODE]` split at each colon, one of a Windows drive's aside
/// where `drives` (`C:\src:/dst:ro`, `\\?\C:\src:/dst`), as lcow_parser.go's rxSource
/// takes a source.
fn split_raw(raw: &str, drives: bool) -> Vec<&str> {
    let drive = drives.then(|| drive_len(raw)).flatten();
    if let Some(colon) = drive.and_then(|at| Some(at + raw.get(at..)?.find(':')?))
        && let Some((source, rest)) = raw.split_at_checked(colon)
    {
        let mut parts = vec![source];
        parts.extend(rest.get(1..).unwrap_or_default().splitn(3, ':'));
        return parts;
    }
    raw.splitn(4, ':').collect()
}

/// ParseMountRaw: a `-v` bind or volume, `[SOURCE:]DEST[:MODE]`, its source a path of
/// this host's.
pub fn parse_raw(raw: &str, driver: &str) -> Result<MountPoint, String> {
    parse_raw_of(raw, driver, Sources::Host { check: false })
}

/// [`parse_raw`] of a `-v` whose source is a path of a microVM's own files (D181).
pub fn parse_raw_within(raw: &str) -> Result<MountPoint, String> {
    parse_raw_of(raw, "", Sources::Microvm)
}

fn parse_raw_of(raw: &str, driver: &str, sources: Sources) -> Result<MountPoint, String> {
    let parts = split_raw(raw, sources.drives());
    if parts.first().is_none_or(|p| p.is_empty()) {
        return Err(invalid_spec(raw));
    }
    let mut m = Mount::default();
    let mut mode = "";
    match parts.as_slice() {
        [dest] => m.target = (*dest).to_string(),
        [source, dest] => {
            if valid_mode(dest) {
                return Err(invalid_spec(raw));
            }
            (m.source, m.target) = ((*source).to_string(), (*dest).to_string());
        }
        [source, dest, m_] => {
            (m.source, m.target) = ((*source).to_string(), (*dest).to_string());
            mode = m_;
        }
        _ => return Err(invalid_spec(raw)),
    }
    if !valid_mode(mode) {
        return Err(invalid_mode(mode));
    }
    // A path from a root is a bind's source, a Windows host's from a drive's (one from no
    // drive's, `\src` or `/src`, refused by `validate`), and anything else a volume's name.
    let drives = sources.drives();
    m.kind = if m.source.starts_with('/')
        || drives && (m.source.starts_with('\\') || drive_len(&m.source).is_some())
    {
        "bind"
    } else {
        "volume"
    }
    .into();
    m.read_only = mode.split(',').any(|o| o == "ro");
    if !driver.is_empty() && m.kind == "volume" {
        m.volume.get_or_insert_with(Default::default).driver = Some((driver.to_string(), BTreeMap::new()));
    }
    if mode.split(',').any(|o| o == "nocopy") {
        m.volume.get_or_insert_with(Default::default).no_copy = true;
    }
    if let Some(p) = propagation_of(mode) {
        m.bind.get_or_insert_with(Default::default).propagation = p;
    }
    let mut mp = mount_point(&m, sources).map_err(|e| format!("{}: {e}", invalid_spec(raw)))?;
    mp.mode = mode.to_string();
    mp.create_source = mp.kind == "bind";
    Ok(mp)
}

/// ParseMountSpec: a `--mount`, as the client sent it.
pub fn parse_spec(sent: &str) -> Result<MountPoint, String> {
    parse_spec_checked(sent, Sources::Host { check: true })
}

/// [`parse_spec`] of a `--mount` whose source is none of this host's, but a path of a
/// microVM's own files (D181): every check but this host's having it.
pub fn parse_spec_within(sent: &str) -> Result<MountPoint, String> {
    parse_spec_checked(sent, Sources::Microvm)
}

fn parse_spec_checked(sent: &str, sources: Sources) -> Result<MountPoint, String> {
    let m = mounts::parse_mount(sent, None)?;
    let mut mp = mount_point(&m, sources)?;
    mp.spec = sent.to_string();
    mp.create_source = m.bind.as_ref().is_some_and(|b| b.create_mountpoint);
    Ok(mp)
}

/// ParseVolumesFrom: `CONTAINER[:MODE]`, its mode `rw` where none is given.
pub fn parse_volumes_from(spec: &str) -> Result<(String, String), String> {
    if spec.is_empty() {
        return Err("volumes-from specification cannot be an empty string".into());
    }
    let (id, mode) = spec.split_once(':').unwrap_or((spec, ""));
    if mode.is_empty() {
        return Ok((id.to_string(), "rw".into()));
    }
    if !valid_mode(mode) || propagation_of(mode).is_some() || mode.split(',').any(|o| o == "nocopy") {
        return Err(invalid_mode(mode));
    }
    Ok((id.to_string(), mode.to_string()))
}

/// The local driver's volumes, under the home (`volumes/NAME/_data`), each with what it
/// was made with (`opts.json`).
pub struct Store {
    root: PathBuf,
}

/// What the local driver keeps of a volume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Labels", default)]
    pub labels: BTreeMap<String, String>,
    #[serde(rename = "Options", default)]
    pub options: BTreeMap<String, String>,
    /// When it was made, in nanoseconds since the epoch.
    #[serde(rename = "Created", default)]
    pub created: u128,
    /// Made for one microVM, by none's name.
    #[serde(rename = "Anonymous", default)]
    pub anonymous: bool,
    /// The microVM whose own engine made it (D181), by its ID: that engine's alone to take
    /// and to change, and gone with that microVM; none for the host's own.
    #[serde(rename = "Of", default, skip_serializing_if = "String::is_empty")]
    pub of: String,
    /// Its name in that engine, by which that engine's clients find it whatever it is
    /// named here; none for an anonymous one.
    #[serde(rename = "There", default, skip_serializing_if = "String::is_empty")]
    pub there: String,
}

const RESTRICTED: &str = "[a-zA-Z0-9][a-zA-Z0-9_.-]";

/// The label of a volume made for a microVM without a name (moby
/// daemon/volume/service AnonymousLabel), which `volume prune` takes without `--all`.
pub const ANONYMOUS: &str = "com.docker.volume.anonymous";

/// The label of a named volume an image's Agentfile gives some of its agents and
/// harnesses alone (`VOLUME name path FOR …`, D109): set as a run of it first mounts the
/// volume, so that another run takes it only as its own image's Agentfile gives it (D115).
/// In the namespace a build's and a run's labels may not use (D109).
pub const SCOPED: &str = "vnd.osi.agentfile.scoped";

/// Refuses a run's own mount of volume `v` at `dest` where an image's Agentfile scoped
/// it ([`SCOPED`]), unless the run's own image's Agentfile names it there: a stand-in
/// for its own volume (D109). An orchestrator's plug of it into a running run is a mount of
/// the run's own, refused alike (D271).
pub(crate) fn scoped_to(
    v: &Volume,
    dest: &str,
    agent_volumes: Option<&[crate::agentfile::AgentVolume]>,
) -> Result<(), String> {
    if !v.labels.contains_key(SCOPED) {
        return Ok(());
    }
    let own = agent_volumes.unwrap_or_default().iter().any(|a| {
        a.name.as_deref() == Some(v.name.as_str()) && a.paths.iter().any(|p| clean(p) == clean(dest))
    });
    if own {
        return Ok(());
    }
    Err(format!(
        "cannot mount volume {} at {dest}: an image's Agentfile gives it to its agents and harnesses alone, and a run mounts it only where its own image's Agentfile puts it",
        v.name
    ))
}

/// Refusing label `key`, in the namespace of shards' build (D109).
pub fn reserved(key: &str) -> String {
    format!(
        "label {key:?} is reserved: shards sets the {} labels from an image's Agentfile, and no other may set one",
        String::from_utf8_lossy(shards_agentfile::agentfile::LABEL_PREFIX)
    )
}

/// One volume change at a time, and none while a microVM's mount points are taken: as
/// the local driver's lock and dockerd's reference counts keep a volume in use from
/// being removed.
static CHANGES: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds volume changes off until it is dropped.
pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    CHANGES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Volume {
    /// Made for a microVM, by none's name.
    pub fn is_anonymous(&self) -> bool {
        self.anonymous || self.labels.contains_key(ANONYMOUS)
    }

    /// What its files may take (D281): its `size` option's bytes, read as the local driver
    /// reads it (go-units' RAMInBytes); none where it has none, or one of no bytes.
    pub fn bound(&self) -> Result<Option<u64>, String> {
        let Some(size) = self.options.get("size") else {
            return Ok(None);
        };
        let bytes = shards_cmdline::resources::ram_in_bytes(size)?;
        Ok(u64::try_from(bytes).ok().filter(|b| *b > 0))
    }
}

/// The local driver's validateOpts (daemon/volume/local/local_unix.go): `type`, `o`,
/// `device` and `size`, `device` and `type` given together and with `o`. A size is the
/// volume's bound, which the servers of its shares hold as its files are written (D281),
/// where the local driver wants its host's quotas and refuses one without them.
pub fn validate_options(opts: &BTreeMap<String, String>) -> Result<(), String> {
    for k in opts.keys() {
        if !matches!(k.as_str(), "type" | "o" | "device" | "size") {
            return Err(format!("invalid option: {}", shards_cmdline::go::quote(k)));
        }
    }
    if let Some(size) = opts.get("size") {
        shards_cmdline::resources::ram_in_bytes(size)?;
    }
    // mandatoryOpts, a Go map: whichever is missing first; each check names one.
    for (opt, required) in [
        ("device", &["type"][..]),
        ("type", &["device"][..]),
        ("o", &["device", "type"][..]),
    ] {
        if opts.contains_key(opt) {
            for r in required {
                if !opts.contains_key(*r) {
                    return Err(format!(
                        "missing required option: {}",
                        shards_cmdline::go::quote(r)
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Where a home keeps its volumes' files, every run's that has any (`Store`): a directory
/// whose volumes guests reach.
pub(crate) const ROOT: &str = "volumes";

impl Store {
    pub fn new(home: &Path) -> Store {
        Store {
            root: home.join(ROOT),
        }
    }

    /// Where volume `name`'s files are.
    pub fn data(&self, name: &str) -> PathBuf {
        self.root.join(name).join("_data")
    }

    /// Where volume `name`'s holders hold it (D281): a lock file beside it, `.NAME.lock`, a
    /// name no volume takes (each begins with a letter or a digit) and no removal's
    /// (`.NAME.removing`). Never removed, so that every hold and every clearing of the name
    /// lock one file.
    fn holders(&self, name: &str) -> PathBuf {
        self.root.join(format!(".{name}.lock"))
    }

    /// Volume `name` held for a run that mounts or plugs it (D281), beside every other run's
    /// hold: its lock file, made where missing with the store's directory, taken shared
    /// (flock(2), LockFileEx on Windows); none at once where a clearing of it holds it. Let go
    /// of as it drops, or as its process ends, however it ends. A run's holds are its
    /// [`Holds`].
    pub fn hold(&self, name: &str) -> Result<Option<Locked>, String> {
        crate::networks::make_dir(&self.root).map_err(|e| format!("{}: {e}", self.root.display()))?;
        self.locking(name, Locked::try_share)
    }

    /// Volume `name` held alone for its clearing (D281): none at once where a run holds it,
    /// never waiting for that run to end.
    pub fn alone(&self, name: &str) -> Result<Option<Locked>, String> {
        self.locking(name, Locked::try_hold)
    }

    /// Where volume `name`'s count is kept (D281): `.NAME.charge`, beside its lock file, a
    /// name no volume takes and no removal's. Never removed, so that every holder of the
    /// name counts in one file.
    fn counts(&self, name: &str) -> PathBuf {
        self.root.join(format!(".{name}.charge"))
    }

    /// Volume `name`'s count file, made where missing, opened to be mapped.
    fn count_file(&self, name: &str) -> Result<std::fs::File, String> {
        let path = self.counts(name);
        std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Volume `name`, held ([`Holds`]), counted for a run whose share of it serves `dir`
    /// (D281), beside every other run that does: its count file held shared for the hold's
    /// life, and its bound in force. Its first holder, which finds none other holding it (an
    /// exclusive try, never waited for), sets the bound `asked` names where it is another,
    /// and walks `dir` for what its files take where it is bounded, writing that before any
    /// server serves them; a holder that comes as the walk runs waits for it, as the lock
    /// wakes it. So a holder that ended without giving back what it held, however it ended,
    /// leaves no count past the volume's next first holder. Refused where `asked` names a
    /// bound other than the one its holders serve it with. Callers hold no [`lock`], which
    /// a change of its bound takes.
    pub fn count(&self, name: &str, dir: &Path, asked: Option<u64>) -> Result<(Locked, Option<u64>), String> {
        use shards_vmm::devices::virtio::fs::{bound, server};
        let said = |e: std::io::Error| format!("{}: {e}", self.counts(name).display());
        loop {
            if let Some(alone) = Locked::try_hold(self.count_file(name)?).map_err(said)?
                && self.bound_to(name, asked)?.is_some()
            {
                let opened =
                    shards_vmm::platform::open_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                let walked = server::charge_of_dir(opened).map_err(|e| format!("volume {name}: {e}"))?;
                bound::store(&alone, walked).map_err(said)?;
            }
            let held = Locked::shared(self.count_file(name)?).map_err(said)?;
            let limit = self
                .get(name)
                .ok_or_else(|| format!("get {name}: no such volume"))?
                .bound()?;
            if let Some(asked) = asked
                && Some(asked) != limit
            {
                return Err(format!(
                    "volume {name} is served with {}: its bound changes only while no run holds it",
                    limit.map_or("no bound".to_string(), |l| format!("a bound of {l} bytes"))
                ));
            }
            if limit.is_none() || bound::counted(&held).map_err(said)? {
                return Ok((held, limit));
            }
            // Its walker ended before it wrote what it found: walked again.
        }
    }

    /// What volume `v`'s files take by its bound's measure (D281), as its listing reads it:
    /// the count its servers share, where a run holds it bounded; else walked now. A named
    /// one none holds is walked holding its count file alone, the count written as a first
    /// holder writes it, so that a holder that waited meanwhile finds it counted; an
    /// anonymous one, which keeps no count file, is walked alone.
    pub fn charge(&self, v: &Volume) -> Result<u64, String> {
        use shards_vmm::devices::virtio::fs::{bound, server};
        let name = v.name.as_str();
        let walk = || -> Result<u64, String> {
            let data = self.data(name);
            let dir =
                shards_vmm::platform::open_dir(&data).map_err(|e| format!("{}: {e}", data.display()))?;
            server::charge_of_dir(dir).map_err(|e| format!("volume {name}: {e}"))
        };
        if v.is_anonymous() {
            return walk();
        }
        let said = |e: std::io::Error| format!("{}: {e}", self.counts(name).display());
        match Locked::try_hold(self.count_file(name)?).map_err(said)? {
            Some(alone) => {
                let walked = walk()?;
                bound::store(&alone, walked).map_err(said)?;
                Ok(walked)
            }
            None => {
                let file = self.count_file(name)?;
                if v.bound()?.is_some() && bound::counted(&file).map_err(said)? {
                    bound::load(&file).map_err(said)
                } else {
                    walk()
                }
            }
        }
    }

    /// Volume `name`'s bound: `asked` where it names another, its options written so under
    /// [`lock`], by its first holder, as none other holds it; else its own.
    fn bound_to(&self, name: &str, asked: Option<u64>) -> Result<Option<u64>, String> {
        let own = self
            .get(name)
            .ok_or_else(|| format!("get {name}: no such volume"))?
            .bound()?;
        match asked {
            Some(asked) if Some(asked) != own => {
                let _held = lock();
                let mut v = self
                    .get(name)
                    .ok_or_else(|| format!("get {name}: no such volume"))?;
                v.options.insert("size".into(), asked.to_string());
                let text = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
                durably(&self.root.join(name), "opts.json", &text)
                    .map_err(|e| format!("volume {name}: {e}"))?;
                Ok(Some(asked))
            }
            _ => Ok(own),
        }
    }

    /// Volume `name`'s lock file ([`holders`](Self::holders)), made where missing, as `take`
    /// takes it.
    fn locking(
        &self,
        name: &str,
        take: fn(std::fs::File) -> std::io::Result<Option<Locked>>,
    ) -> Result<Option<Locked>, String> {
        let path = self.holders(name);
        let said = |e: std::io::Error| format!("{}: {e}", path.display());
        let file = std::fs::File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(said)?;
        take(file).map_err(said)
    }

    /// The local driver's validateName.
    pub(crate) fn valid(name: &str) -> Result<(), String> {
        if name.chars().count() == 1 {
            return Err(
                "volume name is too short, names should be at least two alphanumeric characters".into(),
            );
        }
        let mut chars = name.chars();
        let ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if !ok {
            return Err(format!(
                "{} includes invalid characters for a local volume name, only {} are allowed. If you intended to pass a host directory, use absolute path",
                shards_cmdline::go::quote(name),
                shards_cmdline::go::quote(RESTRICTED)
            ));
        }
        Ok(())
    }

    /// Volume `name`, made if it is not there (as the local driver's Create): itself, and
    /// whether it was made now. An empty name is an anonymous volume's, named at random
    /// and labelled so. One made for microVM `of`'s own engine is that microVM's (D181),
    /// and that engine takes no other: a volume of the name that is not its own is
    /// refused. Callers hold [`lock`].
    pub fn create(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        options: &BTreeMap<String, String>,
        of: &str,
    ) -> Result<(Volume, bool), String> {
        self.make(name, labels, options, of, "")
    }

    /// The volume microVM `of`'s own engine calls `there` (D181): the one made for it
    /// before, whatever it is named here, so that a renamed microVM keeps its volumes;
    /// else made, named after the microVM, `prefix`, as its containers are
    /// (`PREFIX.THERE`), or `PREFIX.THERE.N`, the first that is free, where another has
    /// that name: what it is named here is only how it is shown. Itself, and whether it
    /// was made now. Callers hold [`lock`].
    pub fn own(
        &self,
        of: &str,
        prefix: &str,
        there: &str,
        labels: &BTreeMap<String, String>,
        options: &BTreeMap<String, String>,
    ) -> Result<(Volume, bool), String> {
        let named = format!("{prefix}.{there}");
        let found = self
            .get(&named)
            .filter(|v| v.of == of && v.there == there)
            .or_else(|| self.list().into_iter().find(|v| v.of == of && v.there == there));
        if let Some(v) = found {
            return Ok((v, false));
        }
        let free = std::iter::once(named.clone())
            .chain((2u32..=u32::MAX).map(|n| format!("{named}.{n}")))
            .find(|n| self.get(n).is_none() && !self.root.join(n).exists())
            .unwrap_or(named);
        self.make(&free, labels, options, of, there)
    }

    fn make(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        options: &BTreeMap<String, String>,
        of: &str,
        there: &str,
    ) -> Result<(Volume, bool), String> {
        let mut labels = labels.clone();
        let (name, anonymous) = if name.is_empty() {
            let mut bytes = [0u8; 32];
            getrandom(&mut bytes)?;
            labels.insert(ANONYMOUS.into(), String::new());
            (bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(), true)
        } else {
            Store::valid(name).map_err(|e| format!("create {name}: {e}"))?;
            (name.to_string(), false)
        };
        validate_options(options).map_err(|e| format!("create {name}: {e}"))?;
        if let Some(v) = self.get(&name) {
            if !of.is_empty() && v.of != of {
                return Err(format!(
                    "create {name}: a volume of this name is there already, and it is not this microVM's"
                ));
            }
            return Ok((v, false));
        }
        let dir = self.root.join(&name);
        // Each level made in turn, the home not among them: a removed home stays removed.
        [&self.root, &dir, &self.data(&name)]
            .into_iter()
            .try_for_each(|d| crate::networks::make_dir(d))
            .map_err(|e| format!("error while creating volume root path '{}': {e}", dir.display()))?;
        let v = Volume {
            name: name.clone(),
            labels,
            options: options.clone(),
            created: crate::containers::now(),
            anonymous,
            of: of.to_string(),
            there: there.to_string(),
        };
        let text = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
        durably(&dir, "opts.json", &text).map_err(|e| format!("volume {name}: {e}"))?;
        Ok((v, true))
    }

    /// Gives volume `name` label `key`, empty, where it has none, its options kept.
    /// Callers hold [`lock`].
    pub fn mark(&self, name: &str, key: &str) -> Result<(), String> {
        let mut v = self
            .get(name)
            .ok_or_else(|| format!("get {name}: no such volume"))?;
        if v.labels.contains_key(key) {
            return Ok(());
        }
        v.labels.insert(key.to_string(), String::new());
        let text = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
        durably(&self.root.join(name), "opts.json", &text).map_err(|e| format!("volume {name}: {e}"))
    }

    pub fn get(&self, name: &str) -> Option<Volume> {
        // A name is one component, never `.` or `..`, nor one being removed.
        if name.is_empty() || name.starts_with('.') || name.contains('/') {
            return None;
        }
        let text = std::fs::read(self.root.join(name).join("opts.json")).ok()?;
        serde_json::from_slice(&text).ok()
    }

    /// Every volume, by name.
    pub fn list(&self) -> Vec<Volume> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<Volume> = entries
            .flatten()
            .filter_map(|e| self.get(&e.file_name().to_string_lossy()))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The bytes volume `name`'s files take (directory.Size: each file's size, a hard
    /// link's once), -1 where they cannot be read.
    pub fn size(&self, name: &str) -> i64 {
        let mut seen = std::collections::HashSet::new();
        dir_size(&self.data(name), &mut seen).map_or(-1, |n| i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// Removes volume `name` and its files: gone at once, renamed aside, then deleted,
    /// whatever modes its guests gave its directories. What a crash leaves aside, the next
    /// removal of anything here deletes.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        if self.get(name).is_none() {
            return Err(format!("get {name}: no such volume"));
        }
        let aside = self.root.join(format!(".{name}.removing"));
        std::fs::rename(self.root.join(name), &aside).map_err(|e| format!("remove {name}: {e}"))?;
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for e in entries.flatten() {
                let n = e.file_name();
                let n = n.to_string_lossy();
                if n.starts_with('.') && n.ends_with(".removing") {
                    remove_tree(&e.path()).map_err(|e| format!("remove {name}: {e}"))?;
                }
            }
        }
        Ok(())
    }

    /// Removes volume `name` and its files ([`remove`](Self::remove)) where no run holds it
    /// (D281), as a clearing, `volume rm` and `volume prune` remove one: held alone at once,
    /// never waiting, through its removal, so that no run's start or plug takes it meanwhile;
    /// refused `InUse` where a run that mounts or plugged it holds it, or another removal does,
    /// its files kept. An anonymous volume is held alone only where a plug of it by its name made
    /// its lock file: no start holds one ([`Holds::found`]), and a lock file made here would
    /// outlive it. Callers hold [`lock`].
    pub fn remove_unheld(&self, name: &str) -> Result<(), (ClearCause, String)> {
        let unknown = || (ClearCause::Unknown, format!("get {name}: no such volume"));
        // Looked for first, so that no lock file is made for a volume that is not there.
        let v = self.get(name).ok_or_else(unknown)?;
        let holders = self.holders(name);
        let takes_lock = !v.is_anonymous()
            || holders
                .try_exists()
                .map_err(|e| (ClearCause::Store, format!("{}: {e}", holders.display())))?;
        let _alone = if takes_lock {
            let alone = self.alone(name).map_err(|e| (ClearCause::Store, e))?;
            Some(alone.ok_or_else(|| {
                (
                    ClearCause::InUse,
                    format!(
                        "remove {name}: volume is in use: a run that mounts or plugged it holds it, or another removal"
                    ),
                )
            })?)
        } else {
            None
        };
        // Another removal may have taken it while this one waited for none.
        if self.get(name).is_none() {
            return Err(unknown());
        }
        self.remove(name).map_err(|e| (ClearCause::Store, e))
    }
}

/// The named volumes of the store a run holds for its life (D281), each once, by its name: those
/// it mounts as it starts ([`open_for`]) and those plugged into it as it runs, a daemon's run's
/// and an orchestrator's alike. A removal of one ([`Store::remove_unheld`]: a clearing, `volume
/// rm`, `volume prune`) is refused while it is held; each is let go of as this drops, or as its
/// process ends, however it ends.
#[derive(Debug, Default)]
pub struct Holds(BTreeMap<String, Held>);

/// A run's hold of one volume (D281): its lock file shared, so that no clearing removes it;
/// and once its run serves it, its count file shared, with the bound it is served with
/// ([`Store::count`]).
#[derive(Debug)]
struct Held {
    _volume: Locked,
    count: Option<(Locked, Option<u64>)>,
}

/// What a share of a bounded volume is served with (D281): its place among the shares its
/// share process is given, the bytes its files may take, and its count file, which the share
/// process maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bounded {
    pub share: usize,
    pub limit: u64,
    pub count: PathBuf,
}

impl Holds {
    /// Volume `name` of `store` held ([`Store::hold`]), once however often its run mounts or
    /// plugs it; refused at once where a clearing of it holds it, never waiting for that.
    pub fn take(&mut self, store: &Store, name: &str) -> Result<(), String> {
        if !self.0.contains_key(name) {
            let held = store
                .hold(name)?
                .ok_or_else(|| format!("volume {name} is being cleared"))?;
            self.0.insert(
                name.to_string(),
                Held {
                    _volume: held,
                    count: None,
                },
            );
        }
        Ok(())
    }

    /// Volume `name` of `store`, held, counted as its run's share of it serves `dir`
    /// ([`Store::count`]), once however often its run serves it: its bound in force and its
    /// count file, which each share of it is served with, none where it has no bound; refused
    /// where `asked` names another bound than the one it is served with.
    pub fn count(
        &mut self,
        store: &Store,
        name: &str,
        dir: &Path,
        asked: Option<u64>,
    ) -> Result<Option<(u64, PathBuf)>, String> {
        let held = self
            .0
            .get_mut(name)
            .ok_or_else(|| format!("volume {name} is not held"))?;
        let limit = match held.count.as_ref().map(|(_, limit)| *limit) {
            Some(limit) if asked.is_some_and(|a| Some(a) != limit) => {
                return Err(format!(
                    "volume {name} is served with {}: its bound changes only while no run holds it",
                    limit.map_or("no bound".to_string(), |l| format!("a bound of {l} bytes"))
                ));
            }
            Some(limit) => limit,
            None => {
                let (counted, limit) = store.count(name, dir, asked)?;
                held.count = Some((counted, limit));
                limit
            }
        };
        Ok(limit.map(|l| (l, store.counts(name))))
    }

    /// Volume `name` of `store` as it is there, held as [`take`](Self::take) holds it where it
    /// is a named one, then found again: refused where a clearing took it meanwhile. None where
    /// it is not there, for which no hold's file is made. An anonymous one, its run's own and
    /// gone with it, is held by none: its name is never asked again, and its lock file would
    /// outlive it.
    pub fn found(&mut self, store: &Store, name: &str) -> Result<Option<Volume>, String> {
        match store.get(name) {
            Some(v) if !v.is_anonymous() => {
                self.take(store, name)?;
                store
                    .get(name)
                    .map(Some)
                    .ok_or_else(|| format!("volume {name} was cleared as it was held"))
            }
            other => Ok(other),
        }
    }

    /// `other`'s holds, this run's from now: a count of one it holds already, kept where it
    /// has none.
    pub fn extend(&mut self, other: Holds) {
        for (name, held) in other.0 {
            match self.0.entry(name) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(held);
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    if e.get().count.is_none() {
                        e.get_mut().count = held.count;
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub fn holds(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
}

fn dir_size(dir: &Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> std::io::Result<u64> {
    let mut total = 0u64;
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let meta = std::fs::symlink_metadata(e.path())?;
        if meta.is_dir() {
            total = total.saturating_add(dir_size(&e.path(), seen)?);
        } else if linked(&meta, &e.path()).is_none_or(|id| seen.insert(id)) {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

/// The file at `path` by its device and inode, where other names may be its too, so that
/// it counts once: one of fewer than two links is none. Windows' link counts are std's
/// unstable `windows_by_handle`, so each file is known by its identity there.
#[cfg(unix)]
fn linked(meta: &std::fs::Metadata, _path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    (meta.nlink() >= 2).then(|| (meta.dev(), meta.ino()))
}

#[cfg(windows)]
fn linked(_meta: &std::fs::Metadata, path: &Path) -> Option<(u64, u64)> {
    crate::os::path_id(path)
}

/// Deletes `path` and all under it: std's on Windows, which removes a link, not what it
/// leads to (`remove_dir_all`), and where a directory's attributes keep no one from
/// emptying it, as Unix's modes do.
#[cfg(windows)]
fn remove_tree(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Deletes `path` and all under it, its directories first made this user's to empty.
#[cfg(unix)]
fn remove_tree(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return std::fs::remove_file(path);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    for e in std::fs::read_dir(path)? {
        remove_tree(&e?.path())?;
    }
    std::fs::remove_dir(path)
}

/// `bytes` as `dir`'s `name`, whole or not at all, and kept: written aside, synced, renamed
/// over, the directory synced.
fn durably(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let aside = dir.join(format!("{name}.new"));
    let mut f = std::fs::File::create(&aside)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&aside, dir.join(name))?;
    shards_vmm::platform::sync_dir(dir)
}

fn getrandom(buf: &mut [u8]) -> Result<(), String> {
    shards_vmm::platform::fill_random(buf).map_err(|e| format!("a volume's name: {e}"))
}

/// What a microVM mounts: registerMountPoints' order, `--volumes-from`'s first, then
/// `-v`'s, then `--mount`'s, then the image's `VOLUME`s and `-v DEST`'s not otherwise
/// mounted, each as an anonymous volume; volumes made as they are named. `from` gives
/// another microVM's mount points by name or ID. A run a microVM's own engine made
/// (D181) has that microVM's volumes alone, found by the names its engine gave them, and
/// made after `prefix`, the microVM's name, where they are not there.
pub fn register(
    store: &Store,
    run: &shards_ipc::Run,
    image_volumes: &[String],
    agent_volumes: Option<&[crate::agentfile::AgentVolume]>,
    from: &dyn Fn(&str) -> Result<Vec<MountPoint>, String>,
    prefix: &str,
) -> Result<Vec<(MountPoint, bool)>, String> {
    let mut points: Vec<(MountPoint, bool)> = Vec::new();
    match register_into(
        &mut points,
        store,
        run,
        image_volumes,
        agent_volumes,
        from,
        prefix,
    ) {
        Ok(()) => Ok(points),
        // The anonymous volumes it made go with the microVM it could not make.
        Err(e) => {
            for (p, made) in &points {
                if *made && store.get(&p.name).is_some_and(|v| v.anonymous) {
                    let _ = store.remove(&p.name);
                }
            }
            Err(e)
        }
    }
}

/// A bind's mount point `mp` of a run of `client` (empty for this machine's own user's), as
/// it crosses (D228): a client's directory, served from it by the relay its source names,
/// its source then the client's path; refused where it is no client's of its own, or
/// where a relay comes from no client.
fn relay(mp: &mut MountPoint, client: &str) -> Result<(), String> {
    crate::client_grants::refuse_bind(client, &mp.source)?;
    if let Some(r) = crate::client_grants::relay_of(&mp.source) {
        mp.relay_only = r.only();
        mp.relay = r.token;
        mp.source = r.here;
        mp.create_source = false;
    }
    Ok(())
}

/// Why client `client`, holding `grants`, may not have `points` mounted, judged by their
/// paths before anything is opened, as its run is made (D228): a bind of this machine's
/// (one `--volumes-from` took of another run) or a volume of a directory of this
/// machine's (`o=bind`) only where a grant holds it, a CDI device's own mounts with their
/// device; and another client's bind of its directory never, as its client serves it to
/// the run that named it alone. [`open_for`] judges each again by what it opens.
pub fn refuse_points(
    points: &[MountPoint],
    store: &Store,
    client: &str,
    grants: &crate::client_grants::Grants,
) -> Result<(), String> {
    for p in points {
        if !p.relay.is_empty() {
            continue;
        }
        match p.kind.as_str() {
            "bind" if !p.cdi => crate::client_grants::refuse_path(
                client,
                grants,
                &format!("bind {}", p.source),
                Path::new(&p.source),
                p.rw,
            )?,
            "volume" => {
                if let Some(device) = bound_device(store, &p.name) {
                    crate::client_grants::refuse_path(
                        client,
                        grants,
                        &format!("volume {}", p.name),
                        Path::new(&device),
                        p.rw,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The host directory volume `name` binds (`o=bind`, its `device`), none for the store's
/// own.
fn bound_device(store: &Store, name: &str) -> Option<String> {
    let options = store.get(name)?.options;
    let bound = options
        .get("o")
        .is_some_and(|o| o.split(',').any(|f| f == "bind" || f == "rbind"));
    bound.then(|| options.get("device").cloned().unwrap_or_default())
}

/// Why client `client`, holding `grants`, may not make a volume of `options` (D228): one of
/// a host directory (`o=bind`, its `device`) is of this machine's, which a grant must hold.
pub fn refuse_options(
    options: &BTreeMap<String, String>,
    name: &str,
    client: &str,
    grants: &crate::client_grants::Grants,
) -> Result<(), String> {
    let Some(device) = options.get("device") else {
        return Ok(());
    };
    crate::client_grants::refuse_path(
        client,
        grants,
        &format!("volume {name}: device={device}"),
        Path::new(device),
        true,
    )
}

fn register_into(
    points: &mut Vec<(MountPoint, bool)>,
    store: &Store,
    run: &shards_ipc::Run,
    image_volumes: &[String],
    agent_volumes: Option<&[crate::agentfile::AgentVolume]>,
    from: &dyn Fn(&str) -> Result<Vec<MountPoint>, String>,
    prefix: &str,
) -> Result<(), String> {
    // The microVM whose own engine made the run, whose volumes these are (D181): a named
    // one is its own of that name, made where it is not there.
    let of = run
        .engine_made
        .as_ref()
        .map_or("", |(provider, _)| provider.as_str());
    let named = |name: &str, labels: &BTreeMap<String, String>, options: &BTreeMap<String, String>| {
        if of.is_empty() || name.is_empty() {
            store.create(name, labels, options, of)
        } else {
            store.own(of, prefix, name, labels, options)
        }
    };
    let (binds, specs, volumes_from, driver, tmpfs) = (
        &run.binds,
        &run.mounts,
        &run.volumes_from,
        run.volume_driver.as_str(),
        &run.tmpfs,
    );
    let set = |points: &mut Vec<(MountPoint, bool)>, mp: MountPoint, made: bool| {
        points.retain(|(p, _)| p.destination != mp.destination);
        points.push((mp, made));
    };
    for v in volumes_from {
        let (id, mode) = parse_volumes_from(v)?;
        for m in from(&id)? {
            // One its image's Agentfile gives its agents and harnesses alone is theirs: no
            // other run takes it (D115), as theirs is never in that microVM's own root.
            if m.domains_only {
                return Err(format!(
                    "cannot take the volumes of {id}: its image's Agentfile gives the volume at {} to its agents and harnesses alone",
                    m.destination
                ));
            }
            // A client's bind of its own directory is served to the run that named it
            // alone, by that client (D228).
            if !m.relay.is_empty() {
                return Err(format!(
                    "cannot take the volumes of {id}: its bind at {} is a directory of its client's, {}, which that client serves to that run alone",
                    m.destination, m.source
                ));
            }
            let cp = MountPoint {
                rw: m.rw && !mode.split(',').any(|o| o == "ro"),
                copy_data: false,
                // Whom it was for was that microVM's image's to say: this run's image
                // says it here (below), and the volume is mounted as any other is.
                domains: false,
                domains_only: false,
                ..m
            };
            set(points, cp, false);
        }
    }
    let tmpfs_dests: Vec<String> = tmpfs
        .iter()
        .map(|t| t.split_once(':').map_or(t.as_str(), |(d, _)| d).to_string())
        .collect();
    let mut bound: Vec<String> = Vec::new();
    for b in binds {
        // A client's own directory's relay is no path of this host's (D228), checked as
        // one on none: a Windows host's paths are its drives' (D207).
        let mut mp = if b.starts_with(crate::client_grants::RELAYED) {
            parse_raw_within(b)?
        } else {
            parse_raw(b, driver)?
        };
        if mp.kind == "bind" {
            relay(&mut mp, &run.client)?;
        }
        if bound.contains(&mp.destination) || tmpfs_dests.contains(&mp.destination) {
            return Err(format!("Duplicate mount point: {}", mp.destination));
        }
        let mut made = false;
        if mp.kind == "volume" {
            let (v, now) = named(&mp.name, &BTreeMap::new(), &BTreeMap::new())?;
            scoped_to(&v, &mp.destination, agent_volumes)?;
            made = now;
            mp.name = v.name.clone();
            mp.source = store.data(&v.name).to_string_lossy().into_owned();
            mp.driver = "local".into();
            if mp.mode.is_empty() {
                mp.mode = "z".into();
            }
        }
        bound.push(mp.destination.clone());
        set(points, mp, made);
    }
    for s in specs {
        // A client's: its bind is of its own directory, which this host need not have,
        // and whether this host has a path it names is none of the client's to learn.
        let mut mp = if run.client.is_empty() {
            parse_spec(s)?
        } else {
            parse_spec_within(s)?
        };
        if mp.kind == "bind" {
            relay(&mut mp, &run.client)?;
        }
        if bound.contains(&mp.destination) {
            let m = mounts::parse_mount(s, None)?;
            return Err(format!("Duplicate mount point: {}", m.target));
        }
        let mut made = false;
        if mp.kind == "volume" {
            let m = mounts::parse_mount(s, None)?;
            let (labels, options) = m
                .volume
                .as_ref()
                .map(|v| {
                    (
                        v.labels.clone(),
                        v.driver.as_ref().map(|d| d.1.clone()).unwrap_or_default(),
                    )
                })
                .unwrap_or_default();
            if let Some(k) = labels
                .keys()
                .find(|k| shards_agentfile::agentfile::reserved_label(k.as_bytes()))
            {
                return Err(reserved(k));
            }
            let (v, now) = named(&mp.name, &labels, &options)?;
            scoped_to(&v, &mp.destination, agent_volumes)?;
            made = now;
            mp.name = v.name.clone();
            mp.driver = "local".into();
            mp.source = store.data(&v.name).to_string_lossy().into_owned();
            if mp.mode.is_empty() {
                mp.mode = "z".into();
            }
        }
        set(points, mp, made);
    }
    // createContainerVolumesOS: each image or `-v DEST` volume not already mounted. An
    // Agentfile's image's own (`run.volumes` are a run's `-v DEST`) are its agents' and
    // harnesses' too (AGENTFILE_ARCH.md §4.5): a volume its Agentfile names is that named
    // volume, and one it scopes with `FOR` is not mounted in the run's own root. A run's
    // own mount at one of them stands in for it, given as the Agentfile grants it.
    let image_owned = image_volumes.len().saturating_sub(run.volumes.len());
    for (k, dest) in image_volumes.iter().enumerate() {
        let dest = clean(dest);
        // A Docker `VOLUME` over a domain's root, `/proc`, `/sys` or `/dev`, or at or under
        // its scratch, `/tmp`, stays the run's own root's: the domain's are its init's.
        let ours = k < image_owned
            && agent_volumes.is_some()
            && shards_agentfile::agentfile::domain_mount(dest.as_bytes()).is_none();
        let agent = agent_volumes
            .unwrap_or_default()
            .iter()
            .find(|v| v.paths.iter().any(|p| clean(p) == dest))
            .filter(|_| ours);
        if let Some((p, _)) = points.iter_mut().find(|(p, _)| p.destination == dest) {
            if ours && p.kind == "tmpfs" {
                return Err(cannot_stand_in("a tmpfs", &dest));
            }
            p.domains |= ours;
            continue;
        }
        if tmpfs_dests.iter().any(|t| clean(t) == dest) {
            if ours {
                return Err(cannot_stand_in("a tmpfs", &dest));
            }
            continue;
        }
        let name = agent.and_then(|a| a.name.as_deref()).unwrap_or("");
        let (v, now) = named(name, &BTreeMap::new(), &BTreeMap::new())?;
        if agent.is_some_and(|a| a.scoped) && !name.is_empty() {
            store.mark(&v.name, SCOPED)?;
        }
        let mp = MountPoint {
            kind: "volume".into(),
            name: v.name.clone(),
            source: store.data(&v.name).to_string_lossy().into_owned(),
            destination: dest,
            driver: "local".into(),
            rw: true,
            copy_data: true,
            domains: ours,
            domains_only: agent.is_some_and(|a| a.scoped),
            ..MountPoint::default()
        };
        set(points, mp, now);
    }
    Ok(())
}

/// A microVM's mount points, opened for a start: each shared directory, read-only or not,
/// and its one name where a file is bound alone; the guest's setup entry for each mount
/// point, by its destination; the named volumes among them, held from before they were
/// opened (D281), which its starter keeps with its run until the run ends; and the bound
/// each bounded one's share is served with, by its place in `dirs`.
#[derive(Debug, Default)]
pub struct Opened {
    pub dirs: Vec<(shards_ipc::fd::OwnedFd, bool, std::ffi::OsString)>,
    pub mounts: Vec<(String, Vec<u8>)>,
    pub held: Holds,
    pub bounds: Vec<Bounded>,
}

/// Opens `points` for a start, its first where `first`: a `-v` bind's missing source made,
/// as `-v` means it (volumemounts.MountPoint.Setup, createSource); a file shared through
/// its directory, limited to it; an image's files copied into a volume only as its
/// microVM is first started, where dockerd copies them as it is made (populateVolumes).
/// Each in the guest at `shards{i}`, `i` its place in `dirs`.
pub fn open(points: &[MountPoint], first: bool, store: &Store) -> Result<Opened, String> {
    open_for(points, first, store, &Access::default())
}

/// Whose run's mount points are opened (D228): a client's, what its owner granted it, by
/// which each directory of this machine's is judged once it is open; and where its own
/// directories' relays are taken from, by their tokens. None for this machine's own user's.
#[derive(Default)]
pub struct Access<'a> {
    pub client: Option<(&'a str, &'a crate::client_grants::Grants)>,
    /// The VM's end of the stream a client serves its directory on, by its relay's token;
    /// none where nothing is served, as for a visit, which mounts none of a client's.
    pub relay: Option<Relay<'a>>,
}

/// Where a run's start takes the VM's end of the stream a client serves a directory on, by
/// its relay's token (D228).
pub type Relay<'a> = &'a dyn Fn(&str) -> Result<shards_ipc::fd::OwnedFd, String>;

/// [`open`] of the mount points of a run `access` says whose.
pub fn open_for(
    points: &[MountPoint],
    first: bool,
    store: &Store,
    access: &Access<'_>,
) -> Result<Opened, String> {
    use shards_ipc::fd::OwnedFd;
    let mut opened = Opened::default();
    for p in points {
        let dest = &p.destination;
        // A client's own directory, served from it (D228): the stream its relay is, which
        // its share passes to the VM as it is.
        if !p.relay.is_empty() {
            let Some(take) = access.relay else {
                continue;
            };
            let fd = take(&p.relay)
                .map_err(|e| format!("error mounting \"{}\" to rootfs at \"{dest}\": {e}", p.source))?;
            let flags = if p.rw { "" } else { "ro" };
            let i = opened.dirs.len();
            let mut entry = format!("volume=shards{i}\0{dest}\0{flags}\0").into_bytes();
            entry.extend_from_slice(p.relay_only.as_bytes());
            opened.mounts.push((dest.clone(), entry));
            opened
                .dirs
                .push((fd, !p.rw, std::ffi::OsString::from(&p.relay_only)));
            continue;
        }
        if p.kind == "tmpfs" {
            let mut options = vec!["noexec", "nosuid", "nodev", "rprivate"];
            options.extend(p.tmpfs.split(',').filter(|o| !o.is_empty()));
            if !p.rw {
                options.push("ro");
            }
            let merged = crate::setup::merge_tmpfs_options(&options)?;
            opened.mounts.push((
                dest.clone(),
                format!("tmpfs={dest}\0{}", merged.join(",")).into_bytes(),
            ));
            continue;
        }
        // A named volume of the store, held for its run's life before its files are opened
        // (D281): no clearing removes them under the run, and one under way refuses its start.
        let volume = match p.kind.as_str() {
            "volume" => opened
                .held
                .found(store, &p.name)
                .map_err(|e| format!("error while mounting volume '{}': {e}", p.source))?,
            _ => None,
        };
        // Its bound (D281), held in one count with every other run's: a named volume's, which
        // a count file outlives as its lock file does.
        let bound = volume
            .as_ref()
            .map(Volume::bound)
            .transpose()
            .map_err(|e| format!("error while mounting volume '{}': {e}", p.source))?
            .flatten();
        let named = volume.as_ref().is_some_and(|v| !v.is_anonymous());
        if bound.is_some() && !named {
            return Err(format!(
                "error while mounting volume '{}': a bound (size) is held for a named volume alone",
                p.source
            ));
        }
        // A local volume made with options mounts what they say (the local driver's
        // mount): a host directory bound (`o=bind`, its `device`) is shared as a bind is. Its
        // size is its bound, whatever it mounts.
        let device;
        let source = match volume.map(|v| v.options) {
            Some(opts) if opts.keys().any(|k| k != "size") => {
                let bound = opts
                    .get("o")
                    .is_some_and(|o| o.split(',').any(|f| f == "bind" || f == "rbind"));
                if !bound {
                    return Err(format!(
                        "error while mounting volume '{}': failed to mount local volume: a volume of type {} is not supported yet: shards mounts host directories (o=bind) and its own volumes",
                        p.source,
                        shards_cmdline::go::quote(opts.get("type").map_or("", String::as_str))
                    ));
                }
                device = opts.get("device").cloned().unwrap_or_default();
                Path::new(&device)
            }
            _ => Path::new(&p.source),
        };
        // A client's: nothing of this machine's looked at, or made, beyond its grants
        // (D228); what is open is judged again below.
        let host_path = !p.cdi && (p.kind == "bind" || source != Path::new(&p.source));
        let shown = if p.kind == "bind" {
            format!("bind {}", p.source)
        } else {
            format!("volume {}", p.name)
        };
        if let Some((client, grants)) = access.client
            && host_path
        {
            crate::client_grants::refuse_path(client, grants, &shown, source, p.rw)?;
        }
        let meta = match std::fs::metadata(source) {
            Ok(meta) => meta,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && (p.create_source || (p.kind == "volume" && source == Path::new(&p.source))) =>
            {
                std::fs::create_dir_all(source)
                    .map_err(|e| format!("error while creating mount source path '{}': {e}", p.source))?;
                std::fs::metadata(source).map_err(|e| format!("{}: {e}", p.source))?
            }
            Err(e) => {
                return Err(format!(
                    "error mounting \"{}\" to rootfs at \"{dest}\": {e}",
                    p.source
                ));
            }
        };
        // A file shared through its directory, by its own name there: the file a link names,
        // as a bind mount takes the file its source's links lead to on the host (the
        // guest would follow a link it was given inside itself).
        let real;
        let (dir, only) = if meta.is_dir() {
            (source, std::ffi::OsString::new())
        } else {
            real = std::fs::canonicalize(source)
                .map_err(|e| format!("error mounting \"{}\" to rootfs at \"{dest}\": {e}", p.source))?;
            let parent = real
                .parent()
                .ok_or_else(|| format!("{}: no directory", p.source))?;
            let name = real.file_name().ok_or_else(|| format!("{}: no name", p.source))?;
            (parent, name.to_os_string())
        };
        if p.domains && !only.is_empty() {
            return Err(cannot_stand_in("a file", dest));
        }
        // A directory opens on Windows only for backup semantics (`platform::open_dir`).
        let file = shards_vmm::platform::open_dir(dir)
            .map_err(|e| format!("error mounting \"{}\" to rootfs at \"{dest}\": {e}", p.source))?;
        // A client's: a directory of this machine's only where its owner granted it, judged
        // by the directory open, which is what is shared (D228).
        if let Some((client, grants)) = access.client
            && host_path
        {
            crate::client_grants::refuse_opened(client, grants, &shown, &file, p.rw)?;
        }
        let fd: OwnedFd = file.into();
        let mut flags: Vec<&str> = p.flags.iter().map(String::as_str).collect();
        if !p.rw {
            flags.push("ro");
        }
        // Its files refused execution (`-v …:noexec`, D279): each mount of it `noexec`, the
        // run's own and each of its domains'. Read from its mode, which a run keeps.
        if p.mode.split(',').any(|o| o == "noexec") && !flags.contains(&"noexec") {
            flags.push("noexec");
        }
        if p.kind == "volume" && p.copy_data && first {
            flags.push("copy");
        }
        // The image's, given to its agents and harnesses too (D109); one its Agentfile
        // scopes, to them alone, which the run's own root does not mount.
        if p.domains {
            flags.push("domains");
            // The store's own volume, which an Agentfile's `--chown` and `--chmod` set;
            // never a host directory, which only its owner changes.
            if p.kind == "volume" && source == Path::new(&p.source) {
                flags.push("own");
            }
        }
        // A named volume counted with its run (D281): its first holder walks it; a bounded
        // one's share is served with its bound and its count file.
        if named
            && let Some((limit, count)) = opened
                .held
                .count(store, &p.name, dir, None)
                .map_err(|e| format!("error while mounting volume '{}': {e}", p.source))?
        {
            if !only.is_empty() {
                return Err(format!(
                    "error while mounting volume '{}': a file alone is served no bound",
                    p.source
                ));
            }
            opened.bounds.push(Bounded {
                share: opened.dirs.len(),
                limit,
                count,
            });
        }
        let kind = if p.domains_only { "domain-volume" } else { "volume" };
        let i = opened.dirs.len();
        let mut entry = format!("{kind}=shards{i}\0{dest}\0{}\0", flags.join(",")).into_bytes();
        // Its bytes as Unix has them; on Windows WTF-8's, a name's UTF-8.
        entry.extend_from_slice(only.as_encoded_bytes());
        opened.mounts.push((dest.clone(), entry));
        opened.dirs.push((fd, !p.rw, only));
    }
    Ok(opened)
}

/// A joiner's setup (D119): each shared directory of its, `volume=shards{i}`, the volume
/// `names[i]` of its microVM's join share instead (`join-volume=`), its destination, flags
/// and one name as they were.
pub fn joined(setup: &mut [Vec<u8>], names: &[String]) -> Result<(), String> {
    for entry in setup.iter_mut() {
        let Some(rest) = entry.strip_prefix(b"volume=shards") else {
            continue;
        };
        let end = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or("a malformed shared directory")?;
        let name = std::str::from_utf8(rest.get(..end).unwrap_or_default())
            .ok()
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| names.get(i))
            .ok_or("a shared directory past those opened")?;
        let mut renamed = format!("join-volume={name}").into_bytes();
        renamed.extend_from_slice(rest.get(end..).unwrap_or_default());
        *entry = renamed;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::agentfile::AgentVolume;

    /// A joiner's shared directories are its microVM's join share's volumes, by the names
    /// given them, all else of each entry as it was; other entries untouched.
    #[test]
    fn a_joiners_shared_directories_are_its_join_shares_volumes() {
        let mut setup: Vec<Vec<u8>> = vec![
            b"volume=shards0\0/data\0copy\0".to_vec(),
            b"tmpfs=/t\0size=1m".to_vec(),
            b"volume=shards1\0/etc/x.conf\0ro\0x.conf".to_vec(),
        ];
        joined(&mut setup, &["a".into(), "b".into()]).unwrap();
        assert_eq!(
            setup,
            [
                b"join-volume=a\0/data\0copy\0".to_vec(),
                b"tmpfs=/t\0size=1m".to_vec(),
                b"join-volume=b\0/etc/x.conf\0ro\0x.conf".to_vec(),
            ]
        );
        // Each of a key the run protocol registers (D257), as the guest refuses any other.
        assert_eq!(shards_abi::run::setup::check(&setup), Ok(()));
        let mut past = vec![b"volume=shards2\0/d\0\0".to_vec()];
        assert!(joined(&mut past, &["a".into()]).is_err());
    }

    /// An Agentfile's image's volumes as a run registers and opens them (D109): named and
    /// scoped as its Agentfile says; a Docker `VOLUME` every domain's, but over a domain's
    /// own mounts; a run's own `-v` standing in, given as the Agentfile gives the volume;
    /// `--volumes-from` keeping no other image's grant; a tmpfs or a file refused where
    /// it would stand in, as neither is given across domains.
    // Host directories on Windows, a drive's path in `-v` among them, come with the
    // platform parity plan's step 6.
    #[cfg(unix)]
    #[test]
    fn an_agentfiles_volumes_are_registered_as_it_and_the_run_say() {
        let home_dir = shards_testdir::TempDir::new("agent-volumes").unwrap();
        let home = home_dir.join("agent-volumes");
        std::fs::create_dir_all(home.join("hostdir")).unwrap();
        std::fs::write(home.join("hostfile"), b"x").unwrap();
        let store = Store::new(&home);
        let agent = [
            AgentVolume {
                name: Some("data".into()),
                paths: vec!["/data".into()],
                scoped: true,
                scope: Default::default(),
            },
            AgentVolume {
                name: Some("shared".into()),
                paths: vec!["/shared".into()],
                scoped: false,
                scope: Default::default(),
            },
            AgentVolume {
                name: None,
                paths: vec!["/scratch".into()],
                scoped: true,
                scope: Default::default(),
            },
        ];
        let none = |_: &str| Ok(Vec::new());
        let run_with = |args: &[&str]| {
            let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            args.push("img".into());
            crate::cli::request::for_test(&args).unwrap()
        };
        let registered_as = |args: &[&str],
                             from: &dyn Fn(&str) -> Result<Vec<MountPoint>, String>,
                             agent: Option<&[AgentVolume]>| {
            let run = run_with(args);
            // The image's own, then the run's `-v DEST`, as the daemon lists them.
            let mut image: Vec<String> = [
                "/data", "/shared", "/scratch", "/cache", "/dev/shm", "/tmp", "/tmp/x",
            ]
            .map(String::from)
            .to_vec();
            image.extend(run.volumes.iter().cloned());
            register(&store, &run, &image, agent, from, "")
                .map(|p| p.into_iter().map(|(p, _)| p).collect::<Vec<_>>())
        };
        let registered = |args: &[&str], from: &dyn Fn(&str) -> Result<Vec<MountPoint>, String>| {
            registered_as(args, from, Some(&agent))
        };
        let at = |points: &[MountPoint], dest: &str| {
            points.iter().find(|p| p.destination == dest).cloned().unwrap()
        };
        let said = |p: &MountPoint| (p.domains, p.domains_only);

        let points = registered(&["-v", "/anon"], &none).unwrap();
        assert_eq!(
            (at(&points, "/data").name.as_str(), said(&at(&points, "/data"))),
            ("data", (true, true))
        );
        assert_eq!(
            (
                at(&points, "/shared").name.as_str(),
                said(&at(&points, "/shared"))
            ),
            ("shared", (true, false))
        );
        assert_eq!(said(&at(&points, "/scratch")), (true, true));
        assert_eq!(at(&points, "/scratch").name.len(), 64, "anonymous");
        assert_eq!(
            said(&at(&points, "/cache")),
            (true, false),
            "a Dockerfile's VOLUME is every domain's"
        );
        assert_eq!(
            said(&at(&points, "/dev/shm")),
            (false, false),
            "a domain's /dev is its init's"
        );
        for dest in ["/tmp", "/tmp/x"] {
            assert_eq!(
                said(&at(&points, dest)),
                (false, false),
                "{dest}: a domain's /tmp is its own scratch"
            );
        }
        assert_eq!(
            said(&at(&points, "/anon")),
            (false, false),
            "the run's own -v DEST"
        );
        // What the guest is told: scoped ones in the domains' alone.
        let opened = open(&points, true, &store).unwrap();
        let entry = |dest: &str| {
            let (_, e) = opened.mounts.iter().find(|(d, _)| d == dest).unwrap();
            String::from_utf8_lossy(e).into_owned()
        };
        let flags = |e: &str| {
            e.split('\0')
                .nth(2)
                .unwrap()
                .split(',')
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert!(entry("/data").starts_with("domain-volume="), "{}", entry("/data"));
        assert!(entry("/scratch").starts_with("domain-volume="));
        // Each of a key the run protocol registers (D257), the one chosen as they are opened
        // among them, as the guest refuses any other.
        let entries: Vec<Vec<u8>> = opened.mounts.iter().map(|(_, e)| e.clone()).collect();
        assert_eq!(shards_abi::run::setup::check(&entries), Ok(()));
        for dest in ["/shared", "/cache"] {
            assert!(entry(dest).starts_with("volume="), "{}", entry(dest));
        }
        // Each the store's own, which the Agentfile's --chown and --chmod set.
        for dest in ["/data", "/scratch", "/shared", "/cache"] {
            let f = flags(&entry(dest));
            assert!(
                f.iter().any(|f| f == "domains") && f.iter().any(|f| f == "own"),
                "{dest}: {f:?}"
            );
        }
        for dest in ["/dev/shm", "/tmp", "/tmp/x", "/anon"] {
            assert!(!entry(dest).contains("domains"), "{}", entry(dest));
        }

        // A run's own volume or directory stands in, given as the Agentfile gives it.
        let host = home.join("hostdir");
        let points = registered(
            &[
                "-v",
                "other:/data",
                "-v",
                &format!("{}:/scratch:ro", host.display()),
            ],
            &none,
        )
        .unwrap();
        assert_eq!(
            (at(&points, "/data").name.as_str(), said(&at(&points, "/data"))),
            ("other", (true, false))
        );
        assert_eq!(
            (
                at(&points, "/scratch").kind.as_str(),
                said(&at(&points, "/scratch"))
            ),
            ("bind", (true, false))
        );
        let opened = open(&points, true, &store).unwrap();
        let (_, scratch) = opened.mounts.iter().find(|(d, _)| d == "/scratch").unwrap();
        let f = flags(&String::from_utf8_lossy(scratch));
        // A host directory: given, read-only as bound, and never the image's to own.
        assert!(
            f.iter().any(|f| f == "domains") && f.iter().any(|f| f == "ro"),
            "{f:?}"
        );
        assert!(!f.iter().any(|f| f == "own"), "{f:?}");
        // A Docker VOLUME's stand-in is every domain's, as the VOLUME is.
        let points = registered(&["-v", "cachevol:/cache"], &none).unwrap();
        assert_eq!(
            (at(&points, "/cache").name.as_str(), said(&at(&points, "/cache"))),
            ("cachevol", (true, false))
        );

        // A tmpfs or a file cannot stand in; where no domain is given the volume, they may.
        for args in [
            &["--tmpfs", "/data"][..],
            &["--mount", "type=tmpfs,dst=/shared"],
            &["--tmpfs", "/cache:size=1m"],
        ] {
            let e = registered(args, &none).unwrap_err();
            assert!(e.starts_with("a tmpfs cannot stand in for /"), "{args:?}: {e}");
        }
        assert!(registered(&["--tmpfs", "/dev/shm"], &none).is_ok());
        assert!(registered(&["--tmpfs", "/tmp"], &none).is_ok());
        assert!(registered(&["--tmpfs", "/elsewhere"], &none).is_ok());
        let file = home.join("hostfile");
        let points = registered(&["-v", &format!("{}:/cache", file.display())], &none).unwrap();
        let e = open(&points, true, &store).err().unwrap();
        assert!(e.starts_with("a file cannot stand in for /cache"), "{e}");
        let points = registered(&["-v", &format!("{}:/elsewhere", file.display())], &none).unwrap();
        assert!(
            open(&points, true, &store).is_ok(),
            "a file anywhere else is the run's own"
        );

        // `--volumes-from`: mounted as Docker mounts it, for whom this image says; one its
        // microVM's image gives its agents and harnesses alone is theirs (D115).
        let theirs = |scoped: bool| {
            move |_: &str| {
                Ok(["/theirs", "/data"]
                    .map(|dest| MountPoint {
                        kind: "volume".into(),
                        name: format!("v{}", dest.len()),
                        destination: dest.into(),
                        driver: "local".into(),
                        rw: true,
                        domains: true,
                        domains_only: scoped && dest == "/theirs",
                        ..MountPoint::default()
                    })
                    .to_vec())
            }
        };
        let points = registered(&["--volumes-from", "c"], &theirs(false)).unwrap();
        assert_eq!(said(&at(&points, "/theirs")), (false, false));
        assert_eq!(said(&at(&points, "/data")), (true, false));
        for agent in [Some(&agent[..]), None] {
            let e = registered_as(&["--volumes-from", "c"], &theirs(true), agent).unwrap_err();
            assert_eq!(
                e,
                "cannot take the volumes of c: its image's Agentfile gives the volume at /theirs to its agents and harnesses alone"
            );
        }

        // A named volume an Agentfile scoped is marked so as its run mounts it, and no
        // other run mounts it by name but where its own image's Agentfile puts it.
        assert!(store.get("data").unwrap().labels.contains_key(SCOPED));
        assert!(!store.get("shared").unwrap().labels.contains_key(SCOPED));
        let refused =
            "cannot mount volume data at /x: an image's Agentfile gives it to its agents and harnesses alone";
        for (args, agent) in [
            (&["-v", "data:/x"][..], None),
            (&["-v", "data:/x"], Some(&agent[..])),
            (&["--mount", "src=data,dst=/x"], None),
        ] {
            let e = registered_as(args, &none, agent).unwrap_err();
            assert!(e.starts_with(refused), "{args:?}: {e}");
        }
        assert!(
            registered(&["-v", "data:/data"], &none).is_ok(),
            "its own stand-in"
        );
        assert!(registered_as(&["-v", "shared:/x"], &none, None).is_ok());
        let e = registered(
            &[
                "--mount",
                "src=v1,dst=/v1,volume-label=vnd.osi.agentfile.scoped=1",
            ],
            &none,
        )
        .unwrap_err();
        assert!(
            e.starts_with("label \"vnd.osi.agentfile.scoped\" is reserved"),
            "{e}"
        );
        assert!(store.get("v1").is_none(), "refused before it is made");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// moby's linux_parser_test.go cases, and what dockerd says of them: of a microVM's
    /// paths on every host, and of a Unix host's.
    #[test]
    fn binds_and_volumes_are_read() {
        binds_and_volumes_are_read_by(|raw, _| parse_raw_within(raw));
        if cfg!(unix) {
            binds_and_volumes_are_read_by(parse_raw);
        }
    }

    fn binds_and_volumes_are_read_by(parse_raw: impl Fn(&str, &str) -> Result<MountPoint, String>) {
        let ok = |raw: &str| parse_raw(raw, "").unwrap();
        let b = ok("/host:/ctr:ro");
        assert_eq!(
            (b.kind.as_str(), b.source.as_str(), b.destination.as_str(), b.rw),
            ("bind", "/host", "/ctr", false)
        );
        assert_eq!(b.propagation, "rprivate");
        let v = ok("name:/data");
        assert_eq!(
            (v.kind.as_str(), v.name.as_str(), v.copy_data),
            ("volume", "name", true)
        );
        let n = ok("name:/data:nocopy");
        assert!(!n.copy_data);
        // Past Docker's (D279): Podman's `noexec`, kept in its mode, as a run keeps it; `exec`.
        let x = ok("/host:/ctr:ro,noexec");
        assert_eq!((x.rw, x.mode.as_str()), (false, "ro,noexec"));
        assert_eq!(ok("/host:/ctr:exec").mode, "exec");
        let a = ok("/anon");
        assert_eq!((a.kind.as_str(), a.name.as_str()), ("volume", ""));
        for (raw, said) in [
            ("", "invalid volume specification: ''"),
            ("/foo:rw", "invalid volume specification: '/foo:rw'"),
            ("/a:/b:/c:/d", "invalid volume specification: '/a:/b:/c:/d'"),
            ("/a:/b:nope", "invalid mode: nope"),
            ("/a:/b:ro,rw", "invalid mode: ro,rw"),
            ("/a:/b:exec,noexec", "invalid mode: exec,noexec"),
            (
                "/a:/",
                "invalid volume specification: '/a:/': invalid mount config for type \"bind\": invalid specification: destination can't be '/'",
            ),
            (
                "/a:rel",
                "invalid volume specification: '/a:rel': invalid mount config for type \"bind\": invalid mount path: 'rel' mount path must be absolute",
            ),
        ] {
            assert_eq!(parse_raw(raw, ""), Err(said.to_string()), "{raw}");
        }
        assert_eq!(parse_volumes_from("web"), Ok(("web".into(), "rw".into())));
        assert_eq!(
            parse_volumes_from("web:shared"),
            Err("invalid mode: shared".into())
        );
        assert_eq!(
            Store::valid("a"),
            Err("volume name is too short, names should be at least two alphanumeric characters".into())
        );
    }

    /// A Windows host's `-v` sources from a drive's root, read on every host: a drive's
    /// colon is no part's end, and a path is cleaned, its case kept.
    #[test]
    fn a_windows_drives_paths_are_read() {
        for (raw, parts) in [
            (r"C:\Host\Dir:/ctr:ro", &[r"C:\Host\Dir", "/ctr", "ro"][..]),
            ("c:/a b/x:/ctr", &["c:/a b/x", "/ctr"]),
            (r"\\?\D:\src:/dst:rw", &[r"\\?\D:\src", "/dst", "rw"]),
            // A drive with nothing after it is none: `c` is a volume's name (too short).
            ("c:/ctr", &["c", "/ctr"]),
            ("name:/data", &["name", "/data"]),
            (r"C:\a:/b:/c:/d", &[r"C:\a", "/b", "/c", "/d"]),
            ("1:/x:/y", &["1", "/x", "/y"]),
        ] {
            assert_eq!(split_raw(raw, true), parts, "{raw}");
        }
        assert_eq!(split_raw(r"C:\Host:/ctr", false), ["C", r"\Host", "/ctr"]);
        for (path, len) in [
            (r"C:\", Some(3)),
            ("z:/", Some(3)),
            (r"\\?\C:\x", Some(7)),
            ("C:x", None),
            (r"\x", None),
            (r"\\srv\share", None),
            ("/x", None),
        ] {
            assert_eq!(drive_len(path), len, "{path}");
        }
        for (path, clean) in [
            (r"C:\", r"C:\"),
            ("C:/Users/x/../Y/./z/", r"C:\Users\Y\z"),
            (r"C:\..\..\a", r"C:\a"),
            (r"\\?\C:\a\\b", r"\\?\C:\a\b"),
        ] {
            assert_eq!(clean_drive(path), clean, "{path}");
        }
    }

    /// On Windows a `-v`'s and a `--mount`'s source of this host's is a drive's path, and
    /// one from no drive's root is refused, saying what one is.
    #[cfg(windows)]
    #[test]
    fn a_windows_hosts_binds_are_its_drives() {
        let b = parse_raw(r"C:\Host\..\Dir\:/ctr:ro,rshared", "").unwrap();
        assert_eq!(
            (
                b.kind.as_str(),
                b.source.as_str(),
                b.destination.as_str(),
                b.rw,
                b.propagation.as_str()
            ),
            ("bind", r"C:\Dir", "/ctr", false, "rshared")
        );
        let v = parse_raw("name:/data", "").unwrap();
        assert_eq!((v.kind.as_str(), v.name.as_str()), ("volume", "name"));
        for raw in ["/host:/ctr", r"\host:/ctr"] {
            let e = parse_raw(raw, "").unwrap_err();
            assert!(e.ends_with("mount path must be absolute: a path of this Windows host's is one from its drive's root (C:\\src)"), "{e}");
        }
        let dir = shards_testdir::TempDir::new("drives").unwrap();
        let m = parse_spec(&format!("type=bind,source={},target=/x", dir.path().display())).unwrap();
        assert_eq!(m.source, clean_drive(&dir.path().to_string_lossy()));
        // A microVM's own paths are Linux's here too.
        assert_eq!(parse_raw_within("/host:/ctr").unwrap().source, "/host");
    }

    /// D281: a start holds each named volume it mounts, once however often, from before its
    /// files are opened until what it opened drops with its run: a clearing is refused
    /// meanwhile, and a start while a clearing holds one is refused. An anonymous one, its run's
    /// own, is held by none, and no hold's file is made for it.
    #[test]
    fn a_starts_named_volumes_are_held_until_its_run_lets_go() {
        let dir = shards_testdir::TempDir::new("vol-holds").unwrap();
        let store = Store::new(dir.path());
        let none = BTreeMap::new();
        let anonymous = {
            let _held = lock();
            store.create("agent-a", &none, &none, "").unwrap();
            store.create("", &none, &none, "").unwrap().0.name
        };
        let point = |name: &str, dest: &str| MountPoint {
            kind: "volume".into(),
            name: name.into(),
            source: store.data(name).to_string_lossy().into_owned(),
            destination: dest.into(),
            rw: true,
            ..MountPoint::default()
        };
        let points = [
            point("agent-a", "/a"),
            point(&anonymous, "/b"),
            point("agent-a", "/c"),
        ];
        let opened = open(&points, true, &store).unwrap();
        assert_eq!(opened.dirs.len(), 3);
        assert!(opened.held.holds("agent-a"));
        assert!(!opened.held.holds(&anonymous));
        assert!(!dir.path().join(ROOT).join(format!(".{anonymous}.lock")).exists());
        assert!(
            store.alone("agent-a").unwrap().is_none(),
            "its clearing is refused"
        );
        drop(opened);
        let clearing = store.alone("agent-a").unwrap().unwrap();
        let refused = open(&points, true, &store).unwrap_err();
        assert!(refused.contains("volume agent-a is being cleared"), "{refused}");
        drop(clearing);
        assert!(open(&points, true, &store).unwrap().held.holds("agent-a"));
    }

    /// D281: a volume is removed where no run holds it. One a run holds is refused `InUse` at
    /// once, its files kept, and removed once let go of; asked again, `Unknown`. An anonymous one
    /// is removed with no lock file made for it; one a plug held by its name is refused while it
    /// holds it, as a named one is. A start while a removal holds one alone is refused.
    #[test]
    fn a_volume_is_removed_only_where_no_run_holds_it() {
        let dir = shards_testdir::TempDir::new("vol-unheld").unwrap();
        let store = Store::new(dir.path());
        let none = BTreeMap::new();
        let (anonymous, plugged) = {
            let _held = lock();
            store.create("agent-a", &none, &none, "").unwrap();
            let anonymous = store.create("", &none, &none, "").unwrap().0.name;
            (anonymous, store.create("", &none, &none, "").unwrap().0.name)
        };
        let lock_file = |name: &str| dir.path().join(ROOT).join(format!(".{name}.lock"));
        let mut run = Holds::default();
        run.take(&store, "agent-a").unwrap();
        run.take(&store, &plugged).unwrap();
        for name in ["agent-a", plugged.as_str()] {
            assert!(
                matches!(store.remove_unheld(name), Err((ClearCause::InUse, _))),
                "{name}"
            );
            assert!(store.data(name).exists(), "{name}");
        }
        assert_eq!(store.remove_unheld(&anonymous), Ok(()));
        assert!(!store.data(&anonymous).exists());
        assert!(!lock_file(&anonymous).exists(), "no lock file outlives it");
        drop(run);
        let removing = store.alone("agent-a").unwrap().unwrap();
        assert!(Holds::default().take(&store, "agent-a").is_err());
        drop(removing);
        for name in ["agent-a", plugged.as_str()] {
            assert_eq!(store.remove_unheld(name), Ok(()), "{name}");
            assert!(!store.data(name).exists(), "{name}");
        }
        assert!(store.list().is_empty());
        assert!(matches!(
            store.remove_unheld("agent-a"),
            Err((ClearCause::Unknown, _))
        ));
    }

    /// D281: a volume's `size` is its bound, read as the local driver reads it; none where it
    /// has none or one of no bytes; one the local driver cannot read is refused as it is.
    #[test]
    fn a_volumes_size_is_its_bound() {
        let with = |size: &str| BTreeMap::from([("size".to_string(), size.to_string())]);
        assert!(validate_options(&with("10G")).is_ok());
        assert_eq!(validate_options(&with("ten")).unwrap_err(), "invalid size: 'ten'");
        let v = |options: BTreeMap<String, String>| Volume {
            options,
            ..Volume::default()
        };
        assert_eq!(v(with("10G")).bound().unwrap(), Some(10 << 30));
        assert_eq!(v(with("4096")).bound().unwrap(), Some(4096));
        assert_eq!(v(with("0")).bound().unwrap(), None);
        assert_eq!(v(BTreeMap::new()).bound().unwrap(), None);
    }

    /// D281: a volume's first holder walks it and writes its count before any server serves
    /// it; a holder beside it takes that count as its servers left it, walking nothing; once
    /// none holds it, the next first holder walks it again, so that what a holder left behind
    /// lasts no longer. Its bound is set by a first holder that asks another, and refused to
    /// one that asks another while it is held.
    #[test]
    fn a_volumes_first_holder_counts_it_and_the_others_share_its_count() {
        use shards_vmm::devices::virtio::fs::bound;
        use std::sync::atomic::Ordering;
        let dir = shards_testdir::TempDir::new("vol-count").unwrap();
        let store = Store::new(dir.path());
        let sized = BTreeMap::from([("size".to_string(), "1M".to_string())]);
        {
            let _held = lock();
            store.create("agent-a", &BTreeMap::new(), &sized, "").unwrap();
        }
        let data = store.data("agent-a");
        std::fs::write(data.join("f"), [1u8; 10]).unwrap();
        let counted = || bound::load(&store.count_file("agent-a").unwrap()).unwrap();
        let mut first = Holds::default();
        first.take(&store, "agent-a").unwrap();
        let served = first.count(&store, "agent-a", &data, None).unwrap();
        assert_eq!(served, Some((1 << 20, store.counts("agent-a"))));
        let walked = counted();
        assert!(walked > 10, "a name and its length in whole units: {walked}");
        // What a server charges since stays: a second holder walks nothing.
        let server = bound::mapped(&store.count_file("agent-a").unwrap()).unwrap();
        server.fetch_add(7, Ordering::Relaxed);
        let mut second = Holds::default();
        second.take(&store, "agent-a").unwrap();
        assert_eq!(second.count(&store, "agent-a", &data, None).unwrap(), served);
        assert_eq!(counted(), walked + 7, "not walked again while held");
        let refused = second.count(&store, "agent-a", &data, Some(2 << 20)).unwrap_err();
        assert!(
            refused.contains("changes only while no run holds it"),
            "{refused}"
        );
        drop((first, second));
        // None holds it: its next first holder walks it, and sets the bound it asks.
        let mut next = Holds::default();
        next.take(&store, "agent-a").unwrap();
        let asked = next.count(&store, "agent-a", &data, Some(2 << 20)).unwrap();
        assert_eq!(asked.map(|(limit, _)| limit), Some(2 << 20));
        assert_eq!(counted(), walked, "walked again");
        assert_eq!(
            store.get("agent-a").unwrap().options["size"],
            (2u64 << 20).to_string()
        );
    }

    /// D281: a start's bounded named volume is served with its bound and its count file, at
    /// its place among the start's shares; one without a bound with none; an anonymous one's
    /// size is refused, as no count file is kept for it.
    #[test]
    fn a_starts_bounded_volume_is_served_with_its_bound() {
        let dir = shards_testdir::TempDir::new("vol-bounds").unwrap();
        let store = Store::new(dir.path());
        let none = BTreeMap::new();
        let sized = BTreeMap::from([("size".to_string(), "64k".to_string())]);
        let anonymous = {
            let _held = lock();
            store.create("plain", &none, &none, "").unwrap();
            store.create("sized", &none, &sized, "").unwrap();
            store.create("", &none, &sized, "").unwrap().0.name
        };
        let point = |name: &str, dest: &str| MountPoint {
            kind: "volume".into(),
            name: name.into(),
            source: store.data(name).to_string_lossy().into_owned(),
            destination: dest.into(),
            rw: true,
            ..MountPoint::default()
        };
        let opened = open(&[point("plain", "/a"), point("sized", "/b")], true, &store).unwrap();
        assert_eq!(
            opened.bounds,
            vec![Bounded {
                share: 1,
                limit: 64 << 10,
                count: dir.path().join(ROOT).join(".sized.charge"),
            }]
        );
        let refused = open(&[point(&anonymous, "/c")], true, &store).unwrap_err();
        assert!(
            refused.contains("a bound (size) is held for a named volume alone"),
            "{refused}"
        );
    }
}

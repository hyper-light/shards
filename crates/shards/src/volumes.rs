//! Volumes and bind mounts, as dockerd reads and keeps them (moby docker-v29.8.1
//! daemon/volume/mounts/linux_parser.go, validate.go, volume_copy.go; daemon/volumes.go
//! registerMountPoints; daemon/volume/local): `-v`'s binds and volumes, `--mount`'s
//! mounts, `--volumes-from`, and an image's `VOLUME`s, each a mount point; named and
//! anonymous volumes kept in the home, as the local driver keeps them under its root. A
//! microVM reaches each over virtio-fs (D38).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use shards_cmdline::mounts::{self, Mount, clean};

/// A mount point of a container, as dockerd keeps one (volumemounts.MountPoint).
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
}

fn invalid_spec(spec: &str) -> String {
    format!("invalid volume specification: '{spec}'")
}

fn invalid_mode(mode: &str) -> String {
    format!("invalid mode: {mode}")
}

const PROPAGATIONS: [&str; 6] = ["private", "rprivate", "slave", "rslave", "shared", "rshared"];

/// linuxValidMountMode: rw or ro, a label, a propagation, `nocopy` and a consistency,
/// each once at most.
fn valid_mode(mode: &str) -> bool {
    if mode.is_empty() {
        return true;
    }
    let mut counts = [0u32; 5];
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

/// validateMountConfigImpl, for what a mount point needs whatever its kind.
fn validate(m: &Mount, check_source: bool) -> Result<(), String> {
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
            if !m.source.replace('\\', "/").starts_with('/') {
                return fail(&format!(
                    "invalid mount path: '{}' mount path must be absolute",
                    m.source.replace('\\', "/")
                ));
            }
            let create = m.bind.as_ref().is_some_and(|b| b.create_mountpoint);
            if check_source && !create && std::fs::metadata(&m.source).is_err() {
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
fn mount_point(m: &Mount, check_source: bool) -> Result<MountPoint, String> {
    validate(m, check_source)?;
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
            mp.source = clean(&m.source.replace('\\', "/"));
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

/// ParseMountRaw: a `-v` bind or volume, `[SOURCE:]DEST[:MODE]`.
pub fn parse_raw(raw: &str, driver: &str) -> Result<MountPoint, String> {
    let parts: Vec<&str> = raw.splitn(4, ':').collect();
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
    m.kind = if m.source.starts_with('/') {
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
    let mut mp = mount_point(&m, false).map_err(|e| format!("{}: {e}", invalid_spec(raw)))?;
    mp.mode = mode.to_string();
    mp.create_source = mp.kind == "bind";
    Ok(mp)
}

/// ParseMountSpec: a `--mount`, as the client sent it.
pub fn parse_spec(sent: &str) -> Result<MountPoint, String> {
    let m = mounts::parse_mount(sent, None)?;
    let mut mp = mount_point(&m, true)?;
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
    /// Made for one container, by none's name.
    #[serde(rename = "Anonymous", default)]
    pub anonymous: bool,
}

const RESTRICTED: &str = "[a-zA-Z0-9][a-zA-Z0-9_.-]";

impl Store {
    pub fn new(home: &Path) -> Store {
        Store {
            root: home.join("volumes"),
        }
    }

    /// Where volume `name`'s files are.
    pub fn data(&self, name: &str) -> PathBuf {
        self.root.join(name).join("_data")
    }

    /// The local driver's validateName.
    fn valid(name: &str) -> Result<(), String> {
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
    /// whether it was made now. An empty name is an anonymous volume's, named at random.
    pub fn create(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
        options: &BTreeMap<String, String>,
    ) -> Result<(Volume, bool), String> {
        let (name, anonymous) = if name.is_empty() {
            let mut bytes = [0u8; 32];
            getrandom(&mut bytes)?;
            (bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(), true)
        } else {
            Store::valid(name)?;
            (name.to_string(), false)
        };
        if let Some(v) = self.get(&name) {
            return Ok((v, false));
        }
        let dir = self.root.join(&name);
        std::fs::create_dir_all(self.data(&name))
            .map_err(|e| format!("error while creating volume root path '{}': {e}", dir.display()))?;
        let v = Volume {
            name: name.clone(),
            labels: labels.clone(),
            options: options.clone(),
            created: crate::containers::now(),
            anonymous,
        };
        let text = serde_json::to_vec(&v).map_err(|e| e.to_string())?;
        durably(&dir, "opts.json", &text).map_err(|e| format!("volume {name}: {e}"))?;
        Ok((v, true))
    }

    pub fn get(&self, name: &str) -> Option<Volume> {
        let text = std::fs::read(self.root.join(name).join("opts.json")).ok()?;
        serde_json::from_slice(&text).ok()
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
}

/// Deletes `path` and all under it, its directories first made this user's to empty.
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
    use std::io::Read as _;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|e| format!("a volume's name: {e}"))
}

/// What a container mounts: registerMountPoints' order, `--volumes-from`'s first, then
/// `-v`'s, then `--mount`'s, then the image's `VOLUME`s and `-v DEST`'s not otherwise
/// mounted, each as an anonymous volume; volumes made as they are named. `from` gives
/// another container's mount points by name or ID.
pub fn register(
    store: &Store,
    run: &shards_ipc::Run,
    image_volumes: &[String],
    from: &dyn Fn(&str) -> Result<Vec<MountPoint>, String>,
) -> Result<Vec<(MountPoint, bool)>, String> {
    let mut points: Vec<(MountPoint, bool)> = Vec::new();
    match register_into(&mut points, store, run, image_volumes, from) {
        Ok(()) => Ok(points),
        // The anonymous volumes it made go with the container it could not make.
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

fn register_into(
    points: &mut Vec<(MountPoint, bool)>,
    store: &Store,
    run: &shards_ipc::Run,
    image_volumes: &[String],
    from: &dyn Fn(&str) -> Result<Vec<MountPoint>, String>,
) -> Result<(), String> {
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
            let cp = MountPoint {
                rw: m.rw && !mode.split(',').any(|o| o == "ro"),
                copy_data: false,
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
        let mut mp = parse_raw(b, driver)?;
        if bound.contains(&mp.destination) || tmpfs_dests.contains(&mp.destination) {
            return Err(format!("Duplicate mount point: {}", mp.destination));
        }
        let mut made = false;
        if mp.kind == "volume" {
            let (v, now) = store.create(&mp.name, &BTreeMap::new(), &BTreeMap::new())?;
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
        let mut mp = parse_spec(s)?;
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
            let (v, now) = store.create(&mp.name, &labels, &options)?;
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
    // createContainerVolumesOS: each image or `-v DEST` volume not already mounted.
    for dest in image_volumes {
        let dest = clean(dest);
        if points.iter().any(|(p, _)| p.destination == dest) || tmpfs_dests.iter().any(|t| clean(t) == dest) {
            continue;
        }
        let (v, now) = store.create("", &BTreeMap::new(), &BTreeMap::new())?;
        let mp = MountPoint {
            kind: "volume".into(),
            name: v.name.clone(),
            source: store.data(&v.name).to_string_lossy().into_owned(),
            destination: dest,
            driver: "local".into(),
            rw: true,
            copy_data: true,
            ..MountPoint::default()
        };
        set(points, mp, now);
    }
    Ok(())
}

/// A container's mount points, opened for a start: each shared directory, read-only or not,
/// and its one name where a file is bound alone; and the guest's setup entry for each mount
/// point, by its destination.
#[derive(Debug, Default)]
pub struct Opened {
    pub dirs: Vec<(std::os::fd::OwnedFd, bool, std::ffi::OsString)>,
    pub mounts: Vec<(String, Vec<u8>)>,
}

/// Opens `points` for a start, its first where `first`: a `-v` bind's missing source made,
/// as dockerd makes it (volumemounts.MountPoint.Setup, createSource); a file shared through
/// its directory, limited to it; an image's files copied into a volume only as its
/// container is first started, where dockerd copies them as it is made (populateVolumes).
/// Each in the guest at `shards{i}`, `i` its place in `dirs`.
pub fn open(points: &[MountPoint], first: bool) -> Result<Opened, String> {
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt as _;
    let mut opened = Opened::default();
    for p in points {
        let dest = &p.destination;
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
        let source = Path::new(&p.source);
        let meta = match std::fs::metadata(source) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && (p.create_source || p.kind == "volume") => {
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
        let (dir, only) = if meta.is_dir() {
            (source, std::ffi::OsString::new())
        } else {
            let parent = source
                .parent()
                .ok_or_else(|| format!("{}: no directory", p.source))?;
            let name = source
                .file_name()
                .ok_or_else(|| format!("{}: no name", p.source))?;
            (parent, name.to_os_string())
        };
        let fd: OwnedFd = std::fs::File::open(dir)
            .map_err(|e| format!("error mounting \"{}\" to rootfs at \"{dest}\": {e}", p.source))?
            .into();
        let mut flags: Vec<&str> = Vec::new();
        if !p.rw {
            flags.push("ro");
        }
        if p.kind == "volume" && p.copy_data && first {
            flags.push("copy");
        }
        let i = opened.dirs.len();
        let mut entry = format!("volume=shards{i}\0{dest}\0{}\0", flags.join(",")).into_bytes();
        entry.extend_from_slice(only.as_bytes());
        opened.mounts.push((dest.clone(), entry));
        opened.dirs.push((fd, !p.rw, only));
    }
    Ok(opened)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// moby's linux_parser_test.go cases, and what dockerd says of them.
    #[test]
    fn binds_and_volumes_read_as_dockerd_reads_them() {
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
        let a = ok("/anon");
        assert_eq!((a.kind.as_str(), a.name.as_str()), ("volume", ""));
        for (raw, said) in [
            ("", "invalid volume specification: ''"),
            ("/foo:rw", "invalid volume specification: '/foo:rw'"),
            ("/a:/b:/c:/d", "invalid volume specification: '/a:/b:/c:/d'"),
            ("/a:/b:nope", "invalid mode: nope"),
            ("/a:/b:ro,rw", "invalid mode: ro,rw"),
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
}

//! What a client of another machine's, which this machine's daemon enrolled (D180), may
//! reach of this machine itself (D215): none of its directories and none of its devices,
//! but those its owner grants that client by name. A client's own files are its own: a
//! bind it names is of its directory, served from it through its context ([`relayed`]);
//! named volumes are the daemon's, every client's to mount. Default deny, as shards'
//! networks are: a client holds no grant until the owner gives one
//! (`shards grant client NAME --dir PATH[:ro] --device NAME --gpus`).

use std::path::{Path, PathBuf};

/// The variable the daemon's listener names the client a command runs for by (D215), set
/// for each client's command and no other: what the command asks of the daemon carries it
/// ([`shards_ipc::Run::client`], [`shards_ipc::Command::client`]). A client cannot set it:
/// its request's variables are `frame::ENV`'s alone, and the listener sets it last.
pub(crate) const CLIENT_VAR: &str = "SHARDS_CLIENT";

/// The step by which a client serves a directory of its own for a run of its own through
/// its context (D215), a word no command line can carry: then its relay's token.
pub(crate) const BIND_STEP: &str = "\0bind";

/// What a bind step's stream says first, once its relay is kept for its run to take.
pub(crate) const RELAY_READY: u8 = 1;

/// The client this process's command runs for, empty for this machine's own user's.
pub(crate) fn client_here() -> String {
    std::env::var(CLIENT_VAR).unwrap_or_default()
}

/// A directory of this machine's its owner granted a client, and whether only to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dir {
    pub path: PathBuf,
    pub read_only: bool,
}

/// What a client may reach of this machine: directories, by their real paths, and devices,
/// by the names a run gives them (a CDI device's `VENDOR/CLASS=NAME`, a VFIO group's node,
/// a host's PCI function as `plug function` names it), and its GPUs (`--gpus`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Grants {
    pub dirs: Vec<Dir>,
    pub devices: Vec<String>,
    pub gpus: bool,
}

const GRANTS: &str = "shards grants 1";

impl Grants {
    pub(crate) fn text(&self) -> String {
        let mut text = format!("{GRANTS}\n");
        for d in &self.dirs {
            let mode = if d.read_only { "ro" } else { "rw" };
            text.push_str(&format!("dir {mode} {}\n", d.path.display()));
        }
        for d in &self.devices {
            text.push_str(&format!("device {d}\n"));
        }
        if self.gpus {
            text.push_str("gpus\n");
        }
        text
    }

    pub(crate) fn parse(text: &str) -> Option<Grants> {
        let mut lines = text.lines();
        if lines.next()? != GRANTS {
            return None;
        }
        let mut grants = Grants::default();
        for line in lines {
            match line.split_once(' ') {
                Some(("dir", rest)) => {
                    let (mode, path) = rest.split_once(' ')?;
                    let read_only = match mode {
                        "ro" => true,
                        "rw" => false,
                        _ => return None,
                    };
                    grants.dirs.push(Dir {
                        path: PathBuf::from(path),
                        read_only,
                    });
                }
                Some(("device", name)) => grants.devices.push(name.to_string()),
                None if line == "gpus" => grants.gpus = true,
                None if line.is_empty() => {}
                _ => return None,
            }
        }
        Some(grants)
    }

    /// Whether it grants nothing.
    pub(crate) fn is_empty(&self) -> bool {
        self.dirs.is_empty() && self.devices.is_empty() && !self.gpus
    }

    /// What `asked` adds to it, each grant once: a directory given again takes the mode
    /// given last.
    pub(crate) fn add(&mut self, asked: &Asked) -> Result<(), String> {
        for (given, read_only) in &asked.dirs {
            let path = real_dir(given)?;
            self.dirs.retain(|d| d.path != path);
            self.dirs.push(Dir {
                path,
                read_only: *read_only,
            });
        }
        for d in &asked.devices {
            if !self.devices.contains(d) {
                self.devices.push(d.clone());
            }
        }
        self.gpus |= asked.gpus;
        Ok(())
    }

    /// It without what `asked` names: a directory by its path as given or its real one.
    pub(crate) fn take(&mut self, asked: &Asked) {
        for (given, _) in &asked.dirs {
            let real = std::fs::canonicalize(given).ok();
            self.dirs
                .retain(|d| d.path != Path::new(given) && Some(&d.path) != real.as_ref());
        }
        self.devices.retain(|d| !asked.devices.contains(d));
        if asked.gpus {
            self.gpus = false;
        }
    }
}

/// A directory's real path, as a grant keeps it: its links followed, as a bind's source's
/// are, so that what is granted is the directory, whatever path names it.
fn real_dir(given: &str) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(given).map_err(|e| format!("--dir {given}: {e}"))?;
    if !path.is_dir() {
        return Err(format!("--dir {given}: not a directory"));
    }
    // A grant's record is a line a path: a path that holds one is no grant's.
    if path.as_os_str().to_string_lossy().contains('\n') {
        return Err(format!("--dir {given}: a path with a line break in it"));
    }
    Ok(path)
}

/// What an owner's command names to grant or take back: directories, each with whether it
/// is granted only to read (`PATH:ro`), devices, and the GPUs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Asked {
    pub dirs: Vec<(String, bool)>,
    pub devices: Vec<String>,
    pub gpus: bool,
}

impl Asked {
    pub(crate) fn is_empty(&self) -> bool {
        self.dirs.is_empty() && self.devices.is_empty() && !self.gpus
    }

    /// `word` at `rest[i]`, read where it is one of a grant's flags: how many words it took,
    /// none where it is no grant's.
    pub(crate) fn read(&mut self, rest: &[&str], i: usize) -> Result<usize, String> {
        let value = |flag: &str| {
            rest.get(i + 1)
                .copied()
                .ok_or_else(|| format!("flag needs an argument: {flag}"))
        };
        match rest.get(i).copied() {
            Some("--dir") => {
                let given = value("--dir")?;
                let (path, read_only) = match given.strip_suffix(":ro") {
                    Some(path) => (path, true),
                    None => (given.strip_suffix(":rw").unwrap_or(given), false),
                };
                if path.is_empty() {
                    return Err("--dir: a directory's path".into());
                }
                self.dirs.push((path.to_string(), read_only));
                Ok(2)
            }
            Some("--device") => {
                let name = value("--device")?;
                if name.is_empty() || name.contains('\n') {
                    return Err(format!("--device {name:?}: a device's name"));
                }
                self.devices.push(name.to_string());
                Ok(2)
            }
            Some("--gpus") => {
                self.gpus = true;
                Ok(1)
            }
            _ => Ok(0),
        }
    }
}

fn path_of(home: &Path, client: &str) -> PathBuf {
    home.join("grants").join(client)
}

/// What client `client` holds of this machine: nothing where its owner granted it nothing,
/// or where its record does not read as one.
pub(crate) fn load(home: &Path, client: &str) -> Grants {
    if !shards_remote::invite::valid_name(client) {
        return Grants::default();
    }
    std::fs::read_to_string(path_of(home, client))
        .ok()
        .and_then(|t| Grants::parse(&t))
        .unwrap_or_default()
}

/// Keeps `grants` as what client `client` holds, whole or not at all: none kept for none.
pub(crate) fn save(home: &Path, client: &str, grants: &Grants) -> Result<(), String> {
    if !shards_remote::invite::valid_name(client) {
        return Err(format!("no client {client:?}"));
    }
    let path = path_of(home, client);
    if grants.is_empty() {
        return remove(home, client);
    }
    let dir = home.join("grants");
    shards_vmm::platform::create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let unnamed = dir.join(format!("~{client}.{}", std::process::id()));
    let _ = std::fs::remove_file(&unnamed);
    let written = shards_ipc::create_private(&unnamed).and_then(|mut f| {
        use std::io::Write as _;
        f.write_all(grants.text().as_bytes())?;
        f.sync_all()
    });
    let kept = written.and_then(|()| std::fs::rename(&unnamed, &path));
    if kept.is_err() {
        let _ = std::fs::remove_file(&unnamed);
    }
    kept.map_err(|e| format!("{}: {e}", path.display()))
}

/// Takes every grant of client `client`'s back, as its enrollment ends.
pub(crate) fn remove(home: &Path, client: &str) -> Result<(), String> {
    let path = path_of(home, client);
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
        _ => Ok(()),
    }
}

/// How the owner grants a client more: the command to give, which a refusal says.
fn granting(client: &str, what: &str) -> String {
    format!("`shards grant client {client} {what}`")
}

/// Why client `client`, holding `grants`, may not have the run `run` asks for of this
/// machine's devices (D215): its GPUs (`--gpus`, an Engine API create's requests), a CDI
/// device, a VFIO group or VFIO's own nodes. A `--device` of any other path is the
/// microVM's own (D44), every run's to take.
pub(crate) fn refuse_devices(run: &shards_ipc::Run, client: &str, grants: &Grants) -> Result<(), String> {
    let wants_gpus = !run.gpus.is_empty() || !run.device_requests.is_empty();
    if wants_gpus && !grants.gpus {
        return Err(format!(
            "--gpus: this machine's GPUs are its owner's, and client {client} holds no grant of them: its owner grants them with {}",
            granting(client, "--gpus")
        ));
    }
    let hosts = run
        .devices
        .iter()
        .map(|d| d.split(':').next().unwrap_or_default())
        .filter(|path| is_machines_device(path));
    for name in run.cdi_devices.iter().map(String::as_str).chain(hosts) {
        if !grants.devices.iter().any(|g| same_device(g, name)) {
            return Err(format!(
                "--device {name}: a device of this machine's, its owner's, and client {client} holds no grant of it: its owner grants it with {}",
                granting(client, &format!("--device {name}"))
            ));
        }
    }
    Ok(())
}

/// Whether `--device PATH` names a device of the host's rather than the microVM's: VFIO's
/// nodes, by which a host's PCI functions are passed through (D154, D214).
fn is_machines_device(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    path == "/dev/iommu" || path == "/dev/vfio" || path.starts_with("/dev/vfio/")
}

/// Whether grant `granted` is of device `asked`: by its name, a path's slashes aside.
fn same_device(granted: &str, asked: &str) -> bool {
    granted.trim_end_matches('/') == asked.trim_end_matches('/')
}

/// Why client `client` may not have the host's PCI function `function` plugged into a
/// microVM (`shards plug function`, D214).
pub(crate) fn refuse_function(function: &str, client: &str, grants: &Grants) -> Result<(), String> {
    if grants.devices.iter().any(|g| same_device(g, function)) {
        return Ok(());
    }
    Err(format!(
        "plug function {function}: a device of this machine's, its owner's, and client {client} holds no grant of it: its owner grants it with {}",
        granting(client, &format!("--device {function}"))
    ))
}

/// The grant of `grants` that holds the directory at `real`, a real path, the innermost
/// where grants nest.
fn holding<'g>(grants: &'g Grants, real: &Path) -> Option<&'g Dir> {
    grants
        .dirs
        .iter()
        .filter(|d| real.starts_with(&d.path))
        .max_by_key(|d| d.path.components().count())
}

/// Why client `client`, holding `grants`, may not have the directory of this machine's
/// that `shown` names, at `real`, its real path, mounted (written to where `rw`).
fn refuse_dir(client: &str, grants: &Grants, shown: &str, real: &Path, rw: bool) -> Result<(), String> {
    match holding(grants, real) {
        Some(d) if rw && d.read_only => Err(format!(
            "{shown}: client {client} holds {} read-only: mount it read-only (ro), or its owner grants it with {}",
            d.path.display(),
            granting(client, &format!("--dir {}", d.path.display()))
        )),
        Some(_) => Ok(()),
        None => Err(format!(
            "{shown}: a directory of this machine's, {}, its owner's, and client {client} holds no grant of it: its owner grants it with {}",
            real.display(),
            granting(client, &format!("--dir {}", real.display()))
        )),
    }
}

/// [`refuse_dir`] of a path, as a volume of a directory is made or a run is checked before
/// anything is opened: its real path, links followed as a bind's are. A path not there is
/// judged where it would be made, by its nearest directory that is.
pub(crate) fn refuse_path(client: &str, grants: &Grants, shown: &str, path: &Path, rw: bool) -> Result<(), String> {
    let mut at = path.to_path_buf();
    let mut rest = Vec::new();
    let real = loop {
        match std::fs::canonicalize(&at) {
            Ok(real) => break real,
            Err(_) => {
                let (Some(parent), Some(name)) = (at.parent(), at.file_name()) else {
                    return refuse_dir(client, grants, shown, path, rw);
                };
                rest.push(name.to_os_string());
                at = parent.to_path_buf();
            }
        }
    };
    let real = rest.iter().rev().fold(real, |p, n| p.join(n));
    // A name of the rest that climbs is the path's to resolve when it is made: judged as
    // given, which no grant holds unless it is beneath one as written.
    if rest.iter().any(|n| n == "..") {
        return refuse_dir(client, grants, shown, path, rw);
    }
    refuse_dir(client, grants, shown, &real, rw)
}

/// [`refuse_dir`] of the directory `opened` has open, as a run's start shares it: its path
/// now, as the system names an open file (`F_GETPATH`, `/proc/self/fd`,
/// GetFinalPathNameByHandleW), so that what is judged is what is shared, whatever a path
/// named a moment before (D38's binds follow their links; no rename between a look and
/// the open reaches past the grant).
pub(crate) fn refuse_opened(
    client: &str,
    grants: &Grants,
    shown: &str,
    opened: &std::fs::File,
    rw: bool,
) -> Result<(), String> {
    let real = opened_path(opened).map_err(|e| format!("{shown}: where it is: {e}"))?;
    refuse_dir(client, grants, shown, &real, rw)
}

/// The path of the file `file` has open (fcntl(2) `F_GETPATH`).
#[cfg(target_os = "macos")]
fn opened_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStringExt as _;
    // F_GETPATH fills a buffer of MAXPATHLEN (sys/param.h: PATH_MAX) bytes.
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: fcntl(2) F_GETPATH on an open descriptor, into a buffer of the size it fills.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(len);
    Ok(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

/// The path of the file `file` has open, as proc(5) names it; one removed since is no
/// path's (proc(5) appends ` (deleted)`).
#[cfg(all(unix, not(target_os = "macos")))]
fn opened_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd as _;
    let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if path.as_os_str().to_string_lossy().ends_with(" (deleted)") {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "removed"));
    }
    Ok(path)
}

/// The path of the file `file` has open, links resolved, as [`real_dir`]'s
/// `canonicalize` names it (`\\?\C:\...`).
#[cfg(windows)]
fn opened_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    shards_vmm::platform::final_path(file)
}

/// The prefix of a bind's source a client's context gives for a directory of its own
/// (D215): `d` for a directory or `f` for a file bound alone, its relay's token, then its
/// path there as hex, so that no `:` of a Windows path splits `-v`'s fields. A NUL is in
/// no path a system opens, so a source of this prefix that some part of shards took for
/// this machine's path opens nothing.
pub(crate) const RELAYED: &str = "/\0client-bind/";

/// A client's bind of its own directory, or of a file alone, as it crosses (D215).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Relayed {
    /// The relay's token, by which the machine finds the stream it is served on.
    pub token: String,
    /// Its path on the client.
    pub here: String,
    /// A file bound alone, not a directory.
    pub file: bool,
}

impl Relayed {
    /// The source it is sent as.
    pub(crate) fn source(&self) -> String {
        let hex: String = self.here.bytes().map(|b| format!("{b:02x}")).collect();
        let kind = if self.file { 'f' } else { 'd' };
        format!("{RELAYED}{kind}{}/{hex}", self.token)
    }

    /// The one name a file bound alone is shared by in its directory: its path's last, as
    /// the client's system splits it. Empty for a directory.
    pub(crate) fn only(&self) -> String {
        if !self.file {
            return String::new();
        }
        self.here.rsplit(['/', '\\']).next().unwrap_or_default().to_string()
    }
}

/// The client's bind a source [`Relayed::source`] made, none for any other.
pub(crate) fn relay_of(source: &str) -> Option<Relayed> {
    let rest = source.strip_prefix(RELAYED)?;
    let file = match rest.as_bytes().first()? {
        b'd' => false,
        b'f' => true,
        _ => return None,
    };
    let (token, hex) = rest.get(1..)?.split_once('/')?;
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_hexdigit()) || hex.len() % 2 != 0 {
        return None;
    }
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some(Relayed {
        token: token.to_string(),
        here: String::from_utf8(bytes).ok()?,
        file,
    })
}

/// Why a bind whose source is `source` may not be had by a run of `client` (empty for this
/// machine's own user's): a client's bind is of its own directory, which it serves
/// ([`Relayed`]), and no path of this machine's; and a relay is a client's alone.
pub(crate) fn refuse_bind(client: &str, source: &str) -> Result<(), String> {
    match (client.is_empty(), relay_of(source).is_some() || source.starts_with(RELAYED)) {
        (true, true) => Err(
            "a bind of a client's directory, which only a client's command through its context names".into(),
        ),
        (false, false) => Err(format!(
            "{source}: client {client}'s bind is of its own directory, which its shards serves to the microVM through its context, and names none of this machine's: to mount a directory of this machine's, name a volume of it, which its owner grants with {}",
            granting(client, "--dir PATH")
        )),
        _ => Ok(()),
    }
}

/// Why a client's command `args`, as it came, may not run here (D215): a command that
/// would read or write this machine's files in its own process, which a client's shards
/// carries otherwise (D180: a build's context kept in step, a copy driven step by step, a
/// file it saves or loads its stdout or stdin, a run's `--env-file` its variables, its
/// `--cidfile` told it on a pipe, its seccomp profile its JSON) or not at all (a kernel
/// booted directly, a snapshot's directory). Only a client sending words its shards never
/// sends meets one.
pub(crate) fn refuse_words(args: &[String], cidfile_marker: &str) -> Option<String> {
    use shards_cmdline::commands::{CREATE, EXEC, RUN};
    use shards_cmdline::flags::{self, Outcome};
    let said = shards_cmdline::grammar::rewrite(args).unwrap_or_else(|| args.to_vec());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    let files = |what: &str| {
        Some(format!(
            "{what}: names this machine's files, which a client's command reaches none of: its own shards sends what is its own"
        ))
    };
    match refs.as_slice() {
        ["run", "--kernel", ..] | ["restore", ..] => return files("a kernel booted directly, or a snapshot resumed"),
        ["build" | "cp", ..]
        | ["image" | "builder" | "buildx", "build", ..]
        | ["agent" | "harness" | "mcp", "build" | "sign", ..]
        | ["container", "cp", ..]
        | ["buildx", "policy", ..] => return files(refs.first().copied().unwrap_or_default()),
        ["import", rest @ ..] | ["image", "import", rest @ ..] => {
            let file = rest.iter().find(|w| !w.starts_with('-'));
            if file.is_some_and(|f| *f != "-" && !f.contains("://")) {
                return files("import FILE");
            }
        }
        _ => {}
    }
    let (command, path, n) = match refs.as_slice() {
        ["run", ..] => (&RUN, "shards run", 1),
        ["container", "run", ..] => (&RUN, "shards container run", 2),
        ["create", ..] => (&CREATE, "shards create", 1),
        ["container", "create", ..] => (&CREATE, "shards container create", 2),
        ["exec", ..] => (&EXEC, "shards exec", 1),
        ["container", "exec", ..] => (&EXEC, "shards container exec", 2),
        _ => shards_cmdline::commands::find(&refs)?,
    };
    let rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    let Outcome::Run(parsed) = flags::parse(command, path, &rest, &|_, value| Ok(value.to_string())) else {
        return None;
    };
    for flag in ["env-file", "label-file", "iidfile", "metadata-file"] {
        if parsed.changed(flag) {
            return files(&format!("--{flag}"));
        }
    }
    for flag in ["output", "input"] {
        if parsed.changed(flag) && parsed.string(flag) != "-" {
            return files(&format!("--{flag}"));
        }
    }
    if parsed.changed("cidfile") && !parsed.string("cidfile").contains(cidfile_marker) {
        return files("--cidfile");
    }
    let profile_file = parsed.many("security-opt").iter().any(|o| {
        let profile = o.strip_prefix("seccomp=").or_else(|| o.strip_prefix("seccomp:"));
        profile.is_some_and(|p| p != "unconfined" && !p.trim_start().starts_with('{'))
    });
    if profile_file {
        return files("--security-opt seccomp=FILE");
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A record reads back as it was written, and one of another kind or a malformed line
    /// reads as none.
    #[test]
    fn grants_are_kept_as_they_were_given() {
        let grants = Grants {
            dirs: vec![
                Dir {
                    path: "/srv/data".into(),
                    read_only: false,
                },
                Dir {
                    path: "/srv/models with spaces".into(),
                    read_only: true,
                },
            ],
            devices: vec!["nvidia.com/gpu=0".into(), "/dev/vfio/12".into()],
            gpus: true,
        };
        assert_eq!(Grants::parse(&grants.text()), Some(grants));
        assert_eq!(Grants::parse("shards grants 2\n"), None);
        assert_eq!(Grants::parse("shards grants 1\ndir rx /a\n"), None);
        assert_eq!(Grants::parse("shards grants 1\nall\n"), None);
        assert!(Grants::parse("shards grants 1\n").unwrap().is_empty());
    }

    /// A client holds no device of the machine's but those granted it by name, and its GPUs
    /// only where granted; a `--device` of the microVM's own is every run's.
    #[test]
    fn a_clients_devices_are_those_granted_it() {
        let run = |f: &dyn Fn(&mut shards_ipc::Run)| {
            let mut run = shards_ipc::Run::default();
            f(&mut run);
            run
        };
        let none = Grants::default();
        let own = run(&|r| r.devices = vec!["/dev/fuse:/dev/fuse:rwm".into()]);
        assert!(refuse_devices(&own, "laptop", &none).is_ok());
        let gpus = run(&|r| r.gpus = vec!["all".into()]);
        let refused = refuse_devices(&gpus, "laptop", &none).unwrap_err();
        assert!(refused.contains("shards grant client laptop --gpus"), "{refused}");
        let engine_gpus = run(&|r| r.device_requests = vec![shards_ipc::DeviceRequest::default()]);
        assert!(refuse_devices(&engine_gpus, "laptop", &none).is_err());
        let granted = Grants {
            gpus: true,
            ..Grants::default()
        };
        assert!(refuse_devices(&gpus, "laptop", &granted).is_ok());
        let cdi = run(&|r| r.cdi_devices = vec!["nvidia.com/gpu=0".into()]);
        let refused = refuse_devices(&cdi, "laptop", &granted).unwrap_err();
        assert!(
            refused.contains("--device nvidia.com/gpu=0") && refused.contains("client laptop"),
            "{refused}"
        );
        let vfio = run(&|r| r.devices = vec!["/dev/vfio/12:/dev/vfio/12:rwm".into()]);
        assert!(refuse_devices(&vfio, "laptop", &granted).is_err());
        let both = Grants {
            devices: vec!["nvidia.com/gpu=0".into(), "/dev/vfio/12".into()],
            ..Grants::default()
        };
        assert!(refuse_devices(&cdi, "laptop", &both).is_ok());
        assert!(refuse_devices(&vfio, "laptop", &both).is_ok());
        let iommu = run(&|r| r.devices = vec!["/dev/iommu".into()]);
        assert!(refuse_devices(&iommu, "laptop", &both).is_err());
        assert!(refuse_function("0000:01:00.0", "laptop", &both).is_err());
    }

    /// A directory is a client's to mount where a grant holds it, the innermost grant
    /// deciding whether it may write; a path's links are followed before it is judged, so
    /// that a link inside a grant to a directory outside it is refused, and a path not
    /// there is judged by where it would be made.
    #[cfg(unix)]
    #[test]
    fn a_clients_directories_are_those_granted_it() {
        let at = shards_testdir::TempDir::new("grants-dirs").unwrap();
        let root = std::fs::canonicalize(at.path()).unwrap();
        for d in ["granted/inner", "granted/ro", "elsewhere"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("granted/out")).unwrap();
        let mut grants = Grants::default();
        let mut asked = Asked::default();
        let (granted, ro) = (
            root.join("granted").display().to_string(),
            format!("{}:ro", root.join("granted/ro").display()),
        );
        let words = ["--dir", granted.as_str(), "--dir", ro.as_str()];
        let mut i = 0;
        while i < words.len() {
            i += asked.read(&words, i).unwrap();
        }
        grants.add(&asked).unwrap();
        let judged = |p: &str, rw: bool| refuse_path("laptop", &grants, p, &root.join(p), rw);
        assert!(judged("granted/inner", true).is_ok());
        assert!(judged("granted/not-yet/made", true).is_ok());
        assert!(judged("granted/ro", false).is_ok());
        let ro = judged("granted/ro", true).unwrap_err();
        assert!(ro.contains("read-only"), "{ro}");
        let out = judged("granted/out", false).unwrap_err();
        assert!(out.contains("no grant") && out.contains("elsewhere"), "{out}");
        assert!(judged("elsewhere", false).is_err());
        assert!(judged("granted/../elsewhere/x", false).is_err());
        // What a start shares is judged by where it is open, whatever its path said.
        let opened = std::fs::File::open(root.join("granted/out")).unwrap();
        assert!(refuse_opened("laptop", &grants, "-v", &opened, false).is_err());
        let opened = std::fs::File::open(root.join("granted/inner")).unwrap();
        assert!(refuse_opened("laptop", &grants, "-v", &opened, true).is_ok());
        // Taken back, by the path it was granted by.
        grants.take(&Asked {
            dirs: vec![(root.join("granted").display().to_string(), false)],
            ..Asked::default()
        });
        let judged = |p: &str, rw: bool| refuse_path("laptop", &grants, p, &root.join(p), rw);
        assert!(judged("granted/inner", false).is_err());
        assert!(judged("granted/ro", false).is_ok());
    }

    /// A client's bind crosses as its relay's token and its own path, read back whole, a
    /// Windows path's `:` among it; a client's bind of any other source, and an owner's of
    /// a relay, are refused.
    #[test]
    fn a_clients_bind_is_its_own_directory() {
        let file = Relayed {
            token: "0a1b".into(),
            here: r"C:\Users\me\x.conf".into(),
            file: true,
        };
        let source = file.source();
        assert!(!source.contains(':'));
        assert_eq!(relay_of(&source), Some(file.clone()));
        assert_eq!(file.only(), "x.conf");
        let dir = Relayed {
            here: "/home/me/src".into(),
            file: false,
            ..file
        };
        assert_eq!(relay_of(&dir.source()), Some(dir.clone()));
        assert_eq!(dir.only(), "");
        assert_eq!(relay_of("/srv/data"), None);
        assert_eq!(relay_of(&format!("{RELAYED}dzz/00")), None);
        assert_eq!(relay_of(&format!("{RELAYED}x0a/00")), None);
        assert!(refuse_bind("laptop", &source).is_ok());
        assert!(refuse_bind("", "/srv/data").is_ok());
        let machines = refuse_bind("laptop", "/srv/data").unwrap_err();
        assert!(machines.contains("its own directory"), "{machines}");
        assert!(refuse_bind("", &source).is_err());
    }
}

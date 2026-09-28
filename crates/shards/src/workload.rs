//! Runs a command in a microVM booted into an image: the host half of the run protocol
//! (docs/design/architecture.md D16). The guest dials in once the image is mounted, and
//! shards sends the workload, relays its stdio, and returns its exit status as
//! `docker run` does.

// Where shards has no vsock yet (Windows), only the options are parsed.
#![cfg_attr(not(unix), allow(dead_code))]

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[cfg(unix)]
use shards_abi::run::kind;
use shards_abi::run::{self, Spec};

/// Docker's PATH for Linux containers (moby daemon/pkg/oci/defaults.go).
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// `docker run`'s status when it could not run the command at all.
pub const NOT_RUN: u8 = 125;

/// What `--`, `--env`, `--workdir`, `--user`, `--hostname` and `--interactive` asked for.
#[derive(Debug, Default)]
pub struct Options {
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub workdir: String,
    pub user: String,
    pub hostname: Option<String>,
    pub interactive: bool,
}

/// The workload `docker run` would start. Its environment is Docker's PATH and HOSTNAME,
/// then the variables given, which replace those two, unset them when given without a
/// value, and are otherwise appended (moby daemon/container/container.go,
/// CreateDaemonEnvironment; daemon/container/env.go, ReplaceOrAppendEnvValues). As in the
/// Docker CLI, a variable given without a value first takes shards' own, if shards has
/// one (docker/cli opts/env.go, ValidateEnv).
pub fn spec(o: &Options) -> Result<Spec, String> {
    spec_in(o, |name| std::env::var_os(name))
}

/// [`spec`], with `lookup` for shards' own environment.
fn spec_in(o: &Options, lookup: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Spec, String> {
    let hostname = match &o.hostname {
        Some(h) => h.clone(),
        None => {
            // Docker names a container's host after the first 12 hex digits of its ID.
            let mut id = [0u8; 6];
            shards_vmm::platform::fill_random(&mut id).map_err(|e| format!("random hostname: {e}"))?;
            id.iter().map(|b| format!("{b:02x}")).collect()
        }
    };
    let defaults = [format!("PATH={DEFAULT_PATH}"), format!("HOSTNAME={hostname}")];
    let mut env: Vec<Option<Vec<u8>>> = defaults.iter().map(|d| Some(d.clone().into_bytes())).collect();
    let default_at = |key: &[u8]| {
        defaults
            .iter()
            .position(|d| d.split_once('=').is_some_and(|(k, _)| k.as_bytes() == key))
    };
    for entry in &o.env {
        let entry: Vec<u8> = match entry.split_once('=') {
            Some(("", _)) => return Err(format!("invalid environment variable: {entry}")),
            Some(_) => entry.clone().into_bytes(),
            None if entry.is_empty() => return Err("invalid environment variable: ".into()),
            None => match lookup(entry) {
                Some(value) => [entry.as_bytes(), b"=", &os_bytes(&value)].concat(),
                None => entry.clone().into_bytes(),
            },
        };
        match entry.iter().position(|&b| b == b'=') {
            None => {
                if let Some(slot) = default_at(&entry).and_then(|i| env.get_mut(i)) {
                    *slot = None;
                }
            }
            Some(eq) => match default_at(entry.get(..eq).unwrap_or_default()).and_then(|i| env.get_mut(i)) {
                Some(slot) => *slot = Some(entry),
                None => env.push(Some(entry)),
            },
        }
    }
    let spec = Spec {
        argv: o.argv.iter().map(|a| a.clone().into_bytes()).collect(),
        env: env.into_iter().flatten().collect(),
        cwd: o.workdir.clone().into_bytes(),
        user: o.user.clone().into_bytes(),
        hostname: hostname.into_bytes(),
    };
    if spec.encode().len() > run::MAX_PAYLOAD as usize {
        return Err("the command and its environment are too large".into());
    }
    Ok(spec)
}

#[cfg(unix)]
/// A private directory for this VM's vsock sockets, removed on drop.
#[derive(Debug)]
pub struct SocketDir(PathBuf);

#[cfg(unix)]
impl SocketDir {
    /// A name nobody can predict, made 0700, and never one that already exists: another
    /// user of a shared /tmp can neither take it over nor block it.
    pub fn new() -> io::Result<SocketDir> {
        let mut nonce = [0u8; 8];
        shards_vmm::platform::fill_random(&mut nonce)?;
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("shards-{}-{nonce}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(SocketDir(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(unix)]
impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
/// The socket the guest's run connection arrives on, removed on drop.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
/// Listens where the VM's vsock device delivers guest connections to the run port.
pub fn listen(vsock: &Path) -> io::Result<Listener> {
    let mut path = vsock.as_os_str().to_owned();
    path.push(format!("_{}", run::PORT));
    let path = PathBuf::from(path);
    let listener = UnixListener::bind(&path)?;
    Ok(Listener { listener, path })
}

#[cfg(unix)]
/// Serves the guest: sends `spec`, relays stdio, and returns the workload's exit status.
pub fn serve(listener: &Listener, spec: &Spec, interactive: bool) -> Result<u8, String> {
    let (mut conn, _) = listener
        .listener
        .accept()
        .map_err(|e| format!("waiting for the guest: {e}"))?;
    let payload = spec.encode();
    send(&mut conn, kind::SPEC, &payload).map_err(|e| format!("sending the command: {e}"))?;
    if interactive {
        let mut input = conn.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || forward_stdin(&mut input))
            .map_err(|e| format!("stdin: {e}"))?;
    } else {
        send(&mut conn, kind::STDIN, &[]).map_err(|e| format!("closing stdin: {e}"))?;
    }
    let mut payload = Vec::new();
    loop {
        let mut h = [0u8; run::HEADER];
        conn.read_exact(&mut h)
            .map_err(|e| format!("the guest stopped before the command ended: {e}"))?;
        let (which, len) = run::parse_header(h).ok_or("the guest sent a malformed frame")?;
        payload.resize(len as usize, 0);
        conn.read_exact(&mut payload)
            .map_err(|e| format!("the guest stopped mid-frame: {e}"))?;
        match which {
            kind::STDOUT => {
                let mut out = io::stdout().lock();
                // A closed stdout drops output, as a closed pipe does for `docker run`.
                let _ = out.write_all(&payload).and_then(|()| out.flush());
            }
            kind::STDERR => {
                let mut err = io::stderr().lock();
                let _ = err.write_all(&payload).and_then(|()| err.flush());
            }
            kind::SYSTEM_ERR => {
                let _ = writeln!(io::stderr(), "shards: {}", String::from_utf8_lossy(&payload));
            }
            kind::EXIT => {
                let status: [u8; 4] = payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| "malformed exit status")?;
                return Ok(u8::try_from(u32::from_be_bytes(status)).unwrap_or(u8::MAX));
            }
            _ => return Err(format!("the guest sent an unknown frame kind {which}")),
        }
    }
}

#[cfg(unix)]
fn send(conn: &mut UnixStream, which: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too long"))?;
    conn.write_all(&run::header(which, len))?;
    conn.write_all(payload)
}

#[cfg(unix)]
/// Copies shards' stdin to the workload's, then closes it.
fn forward_stdin(conn: &mut UnixStream) {
    let mut stdin = io::stdin().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if send(conn, kind::STDIN, buf.get(..n).unwrap_or_default()).is_err() {
                    return;
                }
            }
        }
    }
    let _ = send(conn, kind::STDIN, &[]);
}

/// An environment value's bytes, as the OS holds them.
#[cfg(unix)]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    std::os::unix::ffi::OsStrExt::as_bytes(value).to_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The environment for `-e` entries `env`, with shards' own environment holding only
    /// `FROM_SHARDS=yes`.
    fn env_of(env: &[&str], hostname: &str) -> Vec<String> {
        let o = Options {
            argv: vec!["x".into()],
            env: env.iter().map(|s| (*s).to_string()).collect(),
            hostname: Some(hostname.into()),
            ..Options::default()
        };
        spec_in(&o, |name| (name == "FROM_SHARDS").then(|| "yes".into()))
            .unwrap()
            .env
            .into_iter()
            .map(|e| String::from_utf8(e).unwrap())
            .collect()
    }

    #[test]
    fn environments_compose_as_docker_composes_them() {
        assert_eq!(
            env_of(&[], "box"),
            [format!("PATH={DEFAULT_PATH}"), "HOSTNAME=box".into()]
        );
        // Duplicates are appended, for the guest to keep the last (runc's prepareEnv).
        assert_eq!(
            env_of(&["PATH=/bin", "A=1", "A=2"], "box"),
            ["PATH=/bin", "HOSTNAME=box", "A=1", "A=2"]
        );
        // A name alone takes shards' value; without one, it unsets PATH or HOSTNAME and is
        // otherwise dropped.
        assert_eq!(
            env_of(&["A=1", "HOSTNAME", "MISSING", "FROM_SHARDS"], "box"),
            [
                format!("PATH={DEFAULT_PATH}"),
                "A=1".into(),
                "FROM_SHARDS=yes".into()
            ]
        );
        assert!(
            spec(&Options {
                argv: vec!["x".into()],
                env: vec!["=v".into()],
                ..Options::default()
            })
            .is_err()
        );
    }

    #[test]
    fn default_hostnames_are_twelve_hex_digits() {
        let s = spec(&Options {
            argv: vec!["x".into()],
            ..Options::default()
        })
        .unwrap();
        assert_eq!(s.hostname.len(), 12);
        assert!(s.hostname.iter().all(u8::is_ascii_hexdigit));
    }
}

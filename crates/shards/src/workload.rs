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
use std::sync::{Arc, Mutex, PoisonError};

#[cfg(unix)]
use shards_abi::run::kind;
use shards_abi::run::{self, Spec};

/// Docker's PATH for Linux containers (moby daemon/pkg/oci/defaults.go).
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// `docker run`'s status when it could not run the command at all.
pub const NOT_RUN: u8 = 125;

/// How `docker run` reports what its daemon said kept the container from running, and
/// the status it exits with (docker/cli cli/command/container/run.go, withHelp and
/// toStatusError).
pub fn not_run(said: &str) -> (String, u8) {
    (
        format!("shards: Error response from daemon: {said}\n\nRun 'shards run --help' for more information"),
        shards_cmdline::commands::run_status(said),
    )
}

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

/// [`spec`] for a request whose client already gave `-e NAME` its value (run.rs): a name
/// without a value stays unset, whatever this process's environment holds.
pub fn spec_given(o: &Options) -> Result<Spec, String> {
    spec_in(o, |_| None)
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
/// Listens where the VM's vsock device delivers guest connections to host `port`.
pub fn listen(vsock: &Path, port: u32) -> io::Result<Listener> {
    let mut path = vsock.as_os_str().to_owned();
    path.push(format!("_{port}"));
    let path = PathBuf::from(path);
    let listener = UnixListener::bind(&path)?;
    Ok(Listener { listener, path })
}

/// Signals on their way to the workload: sent once the guest has dialed the signal port,
/// queued before then while the workload runs or is sure to, and otherwise not
/// forwarded.
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct Signals {
    conn: Option<UnixStream>,
    queued: Vec<u32>,
    /// The workload runs, or is sure to.
    running: bool,
}

#[cfg(unix)]
pub type ToGuest = Arc<Mutex<Signals>>;

#[cfg(unix)]
fn lock(to: &ToGuest) -> std::sync::MutexGuard<'_, Signals> {
    to.lock().unwrap_or_else(PoisonError::into_inner)
}

/// When a request arrived and when it was answered, on the VMM's clock (for
/// `SHARDS_TIMING`).
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct Timing {
    pub request_us: std::sync::OnceLock<u128>,
    pub answered_us: std::sync::OnceLock<u128>,
    /// The request asked for the timing line (a warm VM's client set `SHARDS_TIMING`).
    pub asked: std::sync::atomic::AtomicBool,
}

/// The command a served VM runs.
#[cfg(unix)]
pub enum Request<'a> {
    /// Known before the guest connects.
    Now { spec: Spec, interactive: bool },
    /// Asked for once the guest is connected and waiting: a warm VM's request.
    Later(&'a dyn Fn() -> Result<Asked<'a>, String>),
}

/// A request, as served: the command, whether it reads stdin, the container's log if its
/// output is kept, and what to call once the command runs.
#[cfg(unix)]
pub struct Asked<'a> {
    pub spec: Spec,
    pub interactive: bool,
    pub log: Option<fs::File>,
    pub started: Option<&'a (dyn Fn() + Sync)>,
}

/// How a served command ended: its exit status, and if it never ran, why not, in the
/// guest's words.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub status: u8,
    pub not_run: Option<String>,
}

/// Serves the guest: sends the command, relays stdio, and returns how the workload ended.
/// Signals go through `to`, on the connection the guest makes to `signals`.
#[cfg(unix)]
pub fn serve(
    listener: &Listener,
    signals: Listener,
    request: Request<'_>,
    to: &ToGuest,
    timing: &Timing,
) -> Result<Ended, String> {
    let (mut conn, _) = listener
        .listener
        .accept()
        .map_err(|e| format!("waiting for the guest: {e}"))?;
    let Asked {
        spec,
        interactive,
        log,
        started,
    } = match request {
        Request::Now { spec, interactive } => Asked {
            spec,
            interactive,
            log: None,
            started: None,
        },
        Request::Later(ask) => ask()?,
    };
    let _ = timing.request_us.set(shards_vmm::log::uptime_us());
    send(&mut conn, kind::SPEC, &spec.encode()).map_err(|e| format!("sending the command: {e}"))?;
    if interactive {
        let mut input = conn.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("stdin".into())
            .spawn(move || forward_stdin(&mut input))
            .map_err(|e| format!("stdin: {e}"))?;
    } else {
        send(&mut conn, kind::STDIN, &[]).map_err(|e| format!("closing stdin: {e}"))?;
    }
    lock(to).running = true;
    let signal_path = signals.path.clone();
    let accepting = to.clone();
    std::thread::Builder::new()
        .name("signal-conn".into())
        .spawn(move || {
            let Ok((mut c, _)) = signals.listener.accept() else {
                return;
            };
            let mut state = lock(&accepting);
            if !state.running {
                return;
            }
            let queued = std::mem::take(&mut state.queued);
            if queued
                .iter()
                .all(|sig| send(&mut c, kind::SIGNAL, &sig.to_be_bytes()).is_ok())
            {
                state.conn = Some(c);
            }
        })
        .map_err(|e| format!("signal connection: {e}"))?;
    let status = relay(&mut conn, timing, log.as_ref(), started);
    {
        let mut state = lock(to);
        *state = Signals::default();
    }
    // A connection still being awaited is woken, so its listener goes. The guest powers
    // off once the host closes the run connection: shutting it down closes every copy.
    let _ = UnixStream::connect(&signal_path);
    let _ = conn.shutdown(std::net::Shutdown::Both);
    status
}

/// Copies the guest's frames to shards' stdout and stderr until the exit status, and to
/// `log` if the container's output is kept. Why a command never ran is not its output,
/// and stays out of the log, as dockerd keeps a failed start out of `docker logs`.
#[cfg(unix)]
fn relay(
    conn: &mut UnixStream,
    timing: &Timing,
    log: Option<&fs::File>,
    started: Option<&(dyn Fn() + Sync)>,
) -> Result<Ended, String> {
    let mut payload = Vec::new();
    let mut not_run = None;
    // A full disk or a removed log costs the log, not the run.
    let keep = |stream: u8, bytes: &[u8]| {
        if let Some(mut file) = log {
            let _ = file.write_all(&log_record(stream, bytes));
        }
    };
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
                keep(LOG_STDOUT, &payload);
            }
            kind::STDERR => {
                let mut err = io::stderr().lock();
                let _ = err.write_all(&payload).and_then(|()| err.flush());
                keep(LOG_STDERR, &payload);
            }
            kind::STARTED => {
                if let Some(started) = started {
                    started();
                }
            }
            kind::SYSTEM_ERR => not_run = Some(String::from_utf8_lossy(&payload).into_owned()),
            kind::EXIT => {
                let _ = timing.answered_us.set(shards_vmm::log::uptime_us());
                let status: [u8; 4] = payload
                    .as_slice()
                    .try_into()
                    .map_err(|_| "malformed exit status")?;
                return Ok(Ended {
                    status: u8::try_from(u32::from_be_bytes(status)).unwrap_or(u8::MAX),
                    not_run,
                });
            }
            _ => return Err(format!("the guest sent an unknown frame kind {which}")),
        }
    }
}

/// A container's log is a sequence of records, each a stream byte ([`LOG_STDOUT`] or
/// [`LOG_STDERR`]), the time the output arrived in nanoseconds since the Unix epoch
/// (big-endian u64), the output's length (big-endian u32), then the output. Appends keep
/// the streams in the order they arrived, as `docker logs` shows them.
pub const LOG_STDOUT: u8 = 1;
pub const LOG_STDERR: u8 = 2;

/// One log record for `bytes` on `stream`, stamped now.
#[cfg(unix)]
fn log_record(stream: u8, bytes: &[u8]) -> Vec<u8> {
    let at = u64::try_from(crate::containers::now()).unwrap_or(u64::MAX);
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let mut record = Vec::with_capacity(13 + bytes.len());
    record.push(stream);
    record.extend_from_slice(&at.to_be_bytes());
    record.extend_from_slice(&len.to_be_bytes());
    record.extend_from_slice(bytes);
    record
}

#[cfg(unix)]
fn send(conn: &mut UnixStream, which: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too long"))?;
    conn.write_all(&run::header(which, len))?;
    conn.write_all(payload)
}

/// Copies shards' stdin to the workload's, then closes it.
#[cfg(unix)]
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

/// Forwards the signals shards receives to the workload, through `to`, even those shards
/// was started ignoring, as the Docker CLI does (shards_ipc::take_forwarded). It blocks
/// them in the calling thread, which every thread started later inherits, so call it
/// before the VM starts: then only the forwarder's `sigwait` receives them. A signal that
/// would end shards, arriving before the workload runs, ends shards as it would have,
/// unless it was ignored. With `reads_terminal`, the terminal's job control applies to
/// shards (shards_ipc::forwarded).
#[cfg(unix)]
pub fn forward_signals(to: ToGuest, reads_terminal: bool) -> Result<(), String> {
    let (set, ignored) =
        shards_ipc::take_forwarded(reads_terminal).map_err(|e| format!("taking signals: {e}"))?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                let mut sig = 0;
                // SAFETY: sigwait(3) on a valid set.
                if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                    return;
                }
                let Some(&(_, linux)) = shards_ipc::FORWARDED.iter().find(|(s, _)| *s == sig) else {
                    continue;
                };
                let forwarded = signal_guest(&to, linux);
                let ends = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM].contains(&sig);
                if !forwarded && ends && !ignored.contains(&sig) {
                    // SAFETY: the default action of a terminating signal, on this process.
                    unsafe {
                        libc::signal(sig, libc::SIG_DFL);
                        let mut only: libc::sigset_t = std::mem::zeroed();
                        libc::sigemptyset(&mut only);
                        libc::sigaddset(&mut only, sig);
                        libc::pthread_sigmask(libc::SIG_UNBLOCK, &only, std::ptr::null_mut());
                        libc::raise(sig);
                    }
                }
            }
        })
        .map_err(|e| format!("signal thread: {e}"))?;
    Ok(())
}

/// From here on the workload will run, so signals queue for it until the guest dials the
/// signal port rather than being refused: a warm VM calls it once it has taken a request.
#[cfg(unix)]
pub fn will_run(to: &ToGuest) {
    lock(to).running = true;
}

/// Sends Linux signal `linux` to the workload through `to`, or queues it until the guest
/// dials the signal port. Returns false before the workload runs, when there is no one to
/// send it to.
#[cfg(unix)]
pub fn signal_guest(to: &ToGuest, linux: u32) -> bool {
    let mut guard = lock(to);
    let state = &mut *guard;
    match (&mut state.conn, state.running) {
        (Some(conn), _) => {
            if send(conn, kind::SIGNAL, &linux.to_be_bytes()).is_err() {
                state.conn = None;
            }
            true
        }
        (None, true) => {
            state.queued.push(linux);
            true
        }
        (None, false) => false,
    }
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

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
use shards_abi::run::{self, Size, Spec, kind};

#[cfg(unix)]
use crate::spec::{INDEX_LINE, INDEX_START, INDEX_STDERR, LOG_HEAD, LOG_STDERR, LOG_STDOUT, now};

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

/// Signals and terminal sizes on their way to the workload, as frames: sent once the guest
/// has dialed the signal port, queued before then while the workload runs or is sure to,
/// and otherwise not sent.
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct Signals {
    conn: Option<UnixStream>,
    queued: Vec<(u8, Vec<u8>)>,
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
    pub log: Option<Logger>,
    pub started: Option<&'a (dyn Fn() + Sync)>,
}

/// How a served command ended: its exit status, and if it never ran, why not, in the
/// guest's words.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub status: u8,
    pub not_run: Option<String>,
    /// Bytes of its output its log could not keep.
    pub lost: u64,
}

/// Keeps a container's output (spec.rs, `LOG`): each record appended to its log, then
/// its entry to the log's index once the record is whole. A record that cannot be kept,
/// on a full disk or a log removed, costs the log, not the run: it is taken back, so
/// neither file holds any of it, and its bytes are counted as lost (audit A12).
#[cfg(unix)]
#[derive(Debug)]
pub struct Logger {
    log: fs::File,
    index: fs::File,
    /// The log's and the index's lengths, where a record that fails is cut back to.
    logged: u64,
    indexed: u64,
    lost: u64,
}

#[cfg(unix)]
impl Logger {
    pub fn new(log: fs::File, index: fs::File) -> io::Result<Logger> {
        let logged = log.metadata()?.len();
        let mut indexed = index.metadata()?.len();
        // A partial entry an earlier writer left is not one.
        if indexed % 8 != 0 {
            indexed -= indexed % 8;
            index.set_len(indexed)?;
        }
        Ok(Logger {
            log,
            index,
            logged,
            indexed,
            lost: 0,
        })
    }

    fn keep(&mut self, stream: u8, bytes: &[u8]) {
        // Nothing is no output: a record of it would tell no stream's last byte.
        if bytes.is_empty() {
            return;
        }
        let record = log_record(stream, bytes);
        let mut entry = self.logged & INDEX_START;
        if stream == LOG_STDERR {
            entry |= INDEX_STDERR;
        }
        if bytes.last() == Some(&b'\n') {
            entry |= INDEX_LINE;
        }
        let kept = (&self.log)
            .write_all(&record)
            .and_then(|()| (&self.index).write_all(&entry.to_be_bytes()));
        match kept {
            Ok(()) => {
                self.logged += record.len() as u64;
                self.indexed += 8;
            }
            Err(_) => {
                self.lost = self.lost.saturating_add(bytes.len() as u64);
                let _ = self.index.set_len(self.indexed);
                let _ = self.log.set_len(self.logged);
            }
        }
    }

    /// Bytes of output not kept.
    pub fn lost(&self) -> u64 {
        self.lost
    }
}

/// Serves the guest: sends the command, relays stdio, and returns how the workload ended.
/// Signals go through `to`, on the connection the guest makes to `signals`. The command
/// goes only to a guest whose init announced this build's contract, `guest_abi`
/// (shards_abi::control::ABI): an init built for another shards could misread it.
#[cfg(unix)]
pub fn serve(
    listener: &Listener,
    signals: Listener,
    request: Request<'_>,
    to: &ToGuest,
    timing: &Timing,
    guest_abi: &dyn Fn() -> Option<u64>,
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
    let abi = guest_abi();
    if abi != Some(shards_abi::IDENTITY) {
        let spoken = abi.map_or_else(|| "none".to_string(), |abi| format!("{abi:016x}"));
        return Err(format!(
            "the microVM's shards-init speaks another shards' protocol ({spoken}, where this \
             shards speaks {:016x}): boot the shards-init built with this shards",
            shards_abi::IDENTITY
        ));
    }
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
                .all(|(which, payload)| send(&mut c, *which, payload).is_ok())
            {
                state.conn = Some(c);
            }
        })
        .map_err(|e| format!("signal connection: {e}"))?;
    let mut log = log;
    let status = relay(&mut conn, timing, log.as_mut(), started);
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
    mut log: Option<&mut Logger>,
    started: Option<&(dyn Fn() + Sync)>,
) -> Result<Ended, String> {
    let mut payload = Vec::new();
    let mut not_run = None;
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
                if let Some(log) = log.as_deref_mut() {
                    log.keep(LOG_STDOUT, &payload);
                }
            }
            kind::STDERR => {
                let mut err = io::stderr().lock();
                let _ = err.write_all(&payload).and_then(|()| err.flush());
                if let Some(log) = log.as_deref_mut() {
                    log.keep(LOG_STDERR, &payload);
                }
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
                    lost: log.as_deref().map_or(0, Logger::lost),
                });
            }
            _ => return Err(format!("the guest sent an unknown frame kind {which}")),
        }
    }
}

/// One log record for `bytes` on `stream`, stamped now.
#[cfg(unix)]
fn log_record(stream: u8, bytes: &[u8]) -> Vec<u8> {
    let at = u64::try_from(now()).unwrap_or(u64::MAX);
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let mut record = Vec::with_capacity(LOG_HEAD as usize + bytes.len());
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
    to_guest(to, kind::SIGNAL, &linux.to_be_bytes())
}

/// Sizes the workload's terminal, if it has one, as [`signal_guest`] sends a signal.
#[cfg(unix)]
pub fn resize_guest(to: &ToGuest, size: Size) -> bool {
    to_guest(to, kind::RESIZE, &size.encode())
}

#[cfg(unix)]
fn to_guest(to: &ToGuest, which: u8, payload: &[u8]) -> bool {
    let mut guard = lock(to);
    let state = &mut *guard;
    match (&mut state.conn, state.running) {
        (Some(conn), _) => {
            if send(conn, which, payload).is_err() {
                state.conn = None;
            }
            true
        }
        (None, true) => {
            state.queued.push((which, payload.to_vec()));
            true
        }
        (None, false) => false,
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shards-logger-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn append(path: &Path) -> fs::File {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
    }

    /// A logger keeps each record whole and indexes it, with its stream and whether it
    /// ends a line; a record that cannot be kept, in either file, is taken back from both
    /// and counted as lost, and the next is kept whole after it (audit A12).
    #[test]
    fn a_logger_keeps_records_whole_or_counts_them_lost() {
        let dir = temp("keep");
        let (log, index) = (dir.join("log"), dir.join("log.idx"));
        let mut logger = Logger::new(append(&log), append(&index)).unwrap();
        logger.keep(LOG_STDOUT, b"a\n");
        logger.keep(LOG_STDERR, b"b");
        logger.keep(LOG_STDOUT, b"");
        assert_eq!(logger.lost(), 0);
        let (bytes, entries) = (fs::read(&log).unwrap(), fs::read(&index).unwrap());
        assert_eq!(bytes.len(), 2 * 13 + 3, "no record of nothing");
        let entry = |i: usize| u64::from_be_bytes(entries[i * 8..i * 8 + 8].try_into().unwrap());
        assert_eq!(entries.len(), 16);
        assert_eq!(entry(0), INDEX_LINE);
        assert_eq!(entry(1), 15 | INDEX_STDERR);

        // A log that cannot be written, then an index that cannot.
        for broken in ["log", "index"] {
            let (l, i) = if broken == "log" {
                (fs::File::open(&log).unwrap(), append(&index))
            } else {
                (append(&log), fs::File::open(&index).unwrap())
            };
            let mut logger = Logger::new(l, i).unwrap();
            logger.keep(LOG_STDOUT, b"lost\n");
            assert_eq!(logger.lost(), 5, "{broken}");
            assert_eq!(
                fs::read(&log).unwrap(),
                bytes,
                "{broken}: a record left in the log"
            );
            assert_eq!(fs::read(&index).unwrap(), entries, "{broken}: an entry left");
        }
        let mut logger = Logger::new(append(&log), append(&index)).unwrap();
        logger.keep(LOG_STDOUT, b"c\n");
        assert_eq!(fs::read(&index).unwrap().len(), 24);
        assert_eq!(
            u64::from_be_bytes(fs::read(&index).unwrap()[16..24].try_into().unwrap()),
            bytes.len() as u64 | INDEX_LINE,
            "kept whole after the loss"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

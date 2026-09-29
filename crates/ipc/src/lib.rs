//! How shards' processes talk: the CLI, the daemon and warm VMM processes hand each other
//! requests, stdio and connections as messages (`kind`), the run a client asks for
//! ([`Run`]), and, on Unix, the transport that carries them with open descriptors, and the
//! socket where they meet, in their shared [`home`] (`unix.rs`).
//!
//! The `shards` command links this crate and the standard library alone, so that it
//! starts fast (docs/research/platform-measurements.md M23).

use std::path::PathBuf;

/// shards' home: `SHARDS_HOME`, or `shards` in this user's data directory:
/// `~/Library/Application Support` on macOS (Apple's File System Programming Guide),
/// `$XDG_DATA_HOME` or else `~/.local/share` on other Unix systems (XDG Base Directory
/// Specification), `%LOCALAPPDATA%` on Windows (`FOLDERID_LocalAppData`).
pub fn home() -> Result<PathBuf, String> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(home) = var("SHARDS_HOME") {
        return Ok(PathBuf::from(home));
    }
    let data = if cfg!(windows) {
        var("LOCALAPPDATA").map(PathBuf::from)
    } else {
        let home = var("HOME").map(PathBuf::from);
        if cfg!(target_os = "macos") {
            home.map(|h| h.join("Library").join("Application Support"))
        } else {
            var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| home.map(|h| h.join(".local").join("share")))
        }
    };
    Ok(data.ok_or("no data directory: set SHARDS_HOME")?.join("shards"))
}

/// Message kinds between the daemon, warm VMs and clients.
pub mod kind {
    /// Warm VM → daemon: the guest is connected and waiting for its command.
    pub const READY: u8 = 1;
    /// Daemon → warm VM: [`RUN_INTERACTIVE`](super::RUN_INTERACTIVE) flags, then the
    /// command (`shards_abi::run::Spec`). Descriptors: the client's connection, then its
    /// stdin, stdout and stderr, then with [`RUN_LOG`](super::RUN_LOG) the container's
    /// log. With [`RUN_DETACHED`](super::RUN_DETACHED), only the command's stdin and the
    /// log.
    pub const RUN: u8 = 2;
    /// Warm VM → client: the command's exit status, one byte, then, if the client asked
    /// for `SHARDS_TIMING`, the VM's timing line for the client to print.
    pub const EXIT: u8 = 3;
    /// Client or daemon → warm VM: a signal for the command, its Linux number as a
    /// big-endian u32.
    pub const SIGNAL: u8 = 4;
    /// Client → daemon: a run ([`Run`](super::Run)). Descriptors: the client's stdin,
    /// stdout and stderr.
    pub const START: u8 = 5;
    /// Daemon → client: this daemon is not the binary the client would start; start that
    /// one and ask again.
    pub const RESTART: u8 = 6;
    /// Client → daemon: exit once the runs in hand are handed over.
    pub const STOP: u8 = 7;
    /// Warm VM → daemon: it has the run's descriptors, so the daemon may close its own.
    pub const TAKEN: u8 = 8;
    /// Warm VM → daemon: the command ended with this status (one byte); then, if the
    /// command never ran, why not. Sent before the client has the status, as `STARTED`
    /// is before the client has anything the command wrote: the daemon's commands, which
    /// first take what every run has sent, see what any client has seen.
    pub const DONE: u8 = 9;
    /// Warm VM → daemon: the command is executing: it started, where `DONE` without this
    /// means it never did.
    pub const STARTED: u8 = 10;
    /// Client → daemon: a container command (`ps`, `wait`, `rm`, ...) and its arguments,
    /// as a list of strings, for the daemon to run and answer with `OUT`, `ERR` and `END`.
    pub const CONTAINER: u8 = 11;
    /// Daemon → client: bytes for the client's stdout. A detached run's first is its
    /// container's ID.
    pub const OUT: u8 = 12;
    /// Daemon → client: bytes for the client's stderr.
    pub const ERR: u8 = 13;
    /// Daemon → client: the command's exit status, one byte, last.
    pub const END: u8 = 14;
    /// Client → warm VM: the size of the command's terminal, rows then columns, each a
    /// big-endian u16, as `docker run` resizes a TTY container's (docker/cli
    /// cli/command/container/tty.go).
    pub const RESIZE: u8 = 15;
}

/// A container command as the client asks for it (`kind::CONTAINER`): its name and the
/// words after it, which the client has checked; what of the client's locale shapes the
/// answer; and the daemon binary the client would start, as in [`Run`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Command {
    pub argv: Vec<String>,
    /// The client's locale is East Asian, where the Docker CLI counts ambiguous-width
    /// characters as two columns (shards_cmdline::width::east_asian).
    pub east_asian: bool,
    /// The client's clock, in nanoseconds since the epoch, and its time zone's offset east
    /// of UTC then, in seconds: the Docker client reads `logs --since` by them.
    pub now: i64,
    pub utc_offset: i32,
    pub daemon: Identity,
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::new();
        put_list(&mut w, &self.argv);
        w.push(u8::from(self.east_asian));
        w.extend_from_slice(&self.now.to_be_bytes());
        w.extend_from_slice(&self.utc_offset.to_be_bytes());
        put_identity(&mut w, &self.daemon);
        w
    }

    /// `None` for anything but a whole command.
    pub fn decode(bytes: &[u8]) -> Option<Command> {
        let mut r = Reader(bytes);
        let command = Command {
            argv: r.list()?,
            east_asian: r.flag()?,
            now: r.u64()? as i64,
            utc_offset: r.u32()? as i32,
            daemon: r.identity()?,
        };
        r.0.is_empty().then_some(command)
    }
}

fn put_identity(w: &mut Vec<u8>, d: &Identity) {
    for v in [d.dev, d.ino, d.size, d.mtime_s as u64, u64::from(d.mtime_ns)] {
        w.extend_from_slice(&v.to_be_bytes());
    }
}

/// A binary, as the daemon and its clients tell builds apart: its file's identity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_s: i64,
    pub mtime_ns: u32,
}

/// `--pull`, as `docker run` takes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Pull {
    #[default]
    Missing,
    Always,
    Never,
}

/// A run as the client asks for it (`kind::START`): its command line as parsed, plus what
/// only the client knows. Its environment gave `-e NAME` its value and SHARDS_KERNEL and
/// SHARDS_INIT, which its working directory made absolute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Run {
    pub image: String,
    /// `NAME=VALUE`, or `NAME` alone to leave NAME unset (docker/cli opts/env.go).
    pub env: Vec<String>,
    pub workdir: String,
    pub user: String,
    pub hostname: Option<String>,
    pub interactive: bool,
    /// `--entrypoint`: `None` when not given; empty when given as "".
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Vec<String>,
    pub pull: Pull,
    pub kernel: Option<String>,
    pub init: Option<String>,
    /// SHARDS_TIMING: the VMM's timing line goes to the client's stderr.
    pub timing: bool,
    /// `--name`.
    pub name: Option<String>,
    /// `-d`: the client gets the container's ID, and the run goes on without it.
    pub detach: bool,
    /// `--rm`: the container goes once it ends.
    pub remove: bool,
    /// `-t`: the command's stdio is a pseudo-terminal, of this many rows and columns
    /// (0 for either: the kernel's default).
    pub tty: Option<(u16, u16)>,
    /// The daemon binary this client would start.
    pub daemon: Identity,
}

impl Run {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::new();
        put_str(&mut w, &self.image);
        put_list(&mut w, &self.env);
        put_str(&mut w, &self.workdir);
        put_str(&mut w, &self.user);
        put_opt(&mut w, self.hostname.as_deref());
        w.push(u8::from(self.interactive));
        match &self.entrypoint {
            Some(list) => {
                w.push(1);
                put_list(&mut w, list);
            }
            None => w.push(0),
        }
        put_list(&mut w, &self.cmd);
        w.push(match self.pull {
            Pull::Missing => 0,
            Pull::Always => 1,
            Pull::Never => 2,
        });
        put_opt(&mut w, self.kernel.as_deref());
        put_opt(&mut w, self.init.as_deref());
        w.push(u8::from(self.timing));
        put_opt(&mut w, self.name.as_deref());
        w.push(u8::from(self.detach));
        w.push(u8::from(self.remove));
        match self.tty {
            Some((rows, cols)) => {
                w.push(1);
                w.extend_from_slice(&rows.to_be_bytes());
                w.extend_from_slice(&cols.to_be_bytes());
            }
            None => w.push(0),
        }
        put_identity(&mut w, &self.daemon);
        w
    }

    /// `None` for anything but a whole, well-formed request.
    pub fn decode(bytes: &[u8]) -> Option<Run> {
        let mut r = Reader(bytes);
        let run = Run {
            image: r.str()?,
            env: r.list()?,
            workdir: r.str()?,
            user: r.str()?,
            hostname: r.opt()?,
            interactive: r.flag()?,
            entrypoint: if r.flag()? { Some(r.list()?) } else { None },
            cmd: r.list()?,
            pull: match r.byte()? {
                0 => Pull::Missing,
                1 => Pull::Always,
                2 => Pull::Never,
                _ => return None,
            },
            kernel: r.opt()?,
            init: r.opt()?,
            timing: r.flag()?,
            name: r.opt()?,
            detach: r.flag()?,
            remove: r.flag()?,
            tty: if r.flag()? {
                let [a, b, c, d] = <[u8; 4]>::try_from(r.take(4)?).ok()?;
                Some((u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d])))
            } else {
                None
            },
            daemon: r.identity()?,
        };
        r.0.is_empty().then_some(run)
    }
}

fn put_str(w: &mut Vec<u8>, s: &str) {
    w.extend_from_slice(&u32::try_from(s.len()).unwrap_or(u32::MAX).to_be_bytes());
    w.extend_from_slice(s.as_bytes());
}

fn put_list(w: &mut Vec<u8>, list: &[String]) {
    w.extend_from_slice(&u32::try_from(list.len()).unwrap_or(u32::MAX).to_be_bytes());
    for s in list {
        put_str(w, s);
    }
}

fn put_opt(w: &mut Vec<u8>, s: Option<&str>) {
    match s {
        Some(s) => {
            w.push(1);
            put_str(w, s);
        }
        None => w.push(0),
    }
}

/// Reads what the `put_*` functions wrote, refusing anything past the end.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (head, rest) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = rest;
        Some(head)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1)?.first().copied()
    }

    fn flag(&mut self) -> Option<bool> {
        match self.byte()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn str(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }

    fn list(&mut self) -> Option<Vec<String>> {
        let n = self.u32()? as usize;
        // Each entry takes at least its 4-byte length: a count past that is a lie.
        if n > self.0.len() / 4 {
            return None;
        }
        (0..n).map(|_| self.str()).collect()
    }

    fn identity(&mut self) -> Option<Identity> {
        Some(Identity {
            dev: self.u64()?,
            ino: self.u64()?,
            size: self.u64()?,
            mtime_s: self.u64()? as i64,
            mtime_ns: u32::try_from(self.u64()?).ok()?,
        })
    }

    fn opt(&mut self) -> Option<Option<String>> {
        if self.flag()? {
            Some(Some(self.str()?))
        } else {
            Some(None)
        }
    }
}

/// A `kind::RUN` flag: the command reads the client's stdin (`-i`).
pub const RUN_INTERACTIVE: u8 = 1;
/// A `kind::RUN` flag: the warm VM writes its timing line to the client's stderr, as
/// `SHARDS_TIMING` asks.
pub const RUN_TIMING: u8 = 2;
/// A `kind::RUN` flag: the command's output also goes to the container's log.
pub const RUN_LOG: u8 = 4;
/// A `kind::RUN` flag: no client: the command's output goes only to the container's log
/// (`-d`).
pub const RUN_DETACHED: u8 = 8;

/// The largest payload a message may carry.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// The most descriptors a message may carry.
pub const MAX_FDS: usize = 8;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn runs_round_trip_and_nothing_else_decodes() {
        let run = Run {
            image: "alpine:3".into(),
            env: vec!["A=1".into(), "B".into()],
            workdir: "/w".into(),
            user: "app".into(),
            hostname: Some("h".into()),
            interactive: true,
            entrypoint: Some(Vec::new()),
            cmd: vec!["sh".into(), "-c".into(), "echo hi".into()],
            pull: Pull::Never,
            kernel: Some("/k".into()),
            init: None,
            timing: true,
            name: Some("web".into()),
            detach: true,
            remove: true,
            tty: Some((24, 300)),
            daemon: Identity {
                dev: 1,
                ino: 2,
                size: 3,
                mtime_s: -4,
                mtime_ns: 5,
            },
        };
        let bytes = run.encode();
        let identity = run.daemon;
        assert_eq!(Run::decode(&bytes), Some(run));
        for n in 0..bytes.len() {
            assert_eq!(Run::decode(&bytes[..n]), None, "a request cut at {n} decoded");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(Run::decode(&longer), None);
        assert_eq!(Run::decode(&Run::default().encode()), Some(Run::default()));
        let command = Command {
            argv: vec!["ps".into(), "-a".into(), String::new()],
            east_asian: true,
            now: -5,
            utc_offset: -18_000,
            daemon: identity,
        };
        let bytes = command.encode();
        assert_eq!(Command::decode(&bytes), Some(command));
        for n in 0..bytes.len() {
            assert_eq!(Command::decode(&bytes[..n]), None, "a command cut at {n} decoded");
        }
        // A list that claims more entries than bytes remain is refused, not allocated.
        let mut lying = Vec::new();
        put_str(&mut lying, "i");
        lying.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(Run::decode(&lying), None);
    }
}

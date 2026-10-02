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
///
/// It is absolute: a relative `SHARDS_HOME` or `HOME` is taken from this process's
/// working directory now, since shards' processes make the home their working directory
/// and would read it again from there (audit A17). The XDG specification has a relative
/// `XDG_DATA_HOME` ignored. Whoever starts another shards process passes it this one
/// ([`HOME`]).
pub fn home() -> Result<PathBuf, String> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let absolute = |p: PathBuf| std::path::absolute(&p).map_err(|e| format!("{}: {e}", p.display()));
    if let Some(home) = var(HOME) {
        return absolute(PathBuf::from(home));
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
                .filter(|p| p.is_absolute())
                .or_else(|| home.map(|h| h.join(".local").join("share")))
        }
    };
    absolute(data.ok_or("no data directory: set SHARDS_HOME")?.join("shards"))
}

/// The variable that names shards' home.
pub const HOME: &str = "SHARDS_HOME";

/// Message kinds between the daemon, warm VMs and clients.
pub mod kind {
    /// Warm VM → daemon: the guest is connected and waiting for its command.
    pub const READY: u8 = 1;
    /// Daemon → warm VM: [`RUN_INTERACTIVE`](super::RUN_INTERACTIVE) flags, the most
    /// bytes a segment of the container's log holds and the most segments it keeps (each
    /// a big-endian u64), then the command (`shards_abi::run::Spec`). Descriptors: the
    /// client's connection, then its stdin, stdout and stderr, then with
    /// [`RUN_LOG`](super::RUN_LOG) the container's directory, where its log is. With
    /// [`RUN_DETACHED`](super::RUN_DETACHED), only the command's stdin and the directory.
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
    /// Warm VM → daemon, before `DONE`: bytes of the command's output its container's log
    /// could not keep, as a big-endian u64 (audit A12).
    pub const LOST: u8 = 16;
    /// Warm VM → daemon, before `DONE`: the working set it recorded, for the daemon to
    /// write with its template, which no VM may write (D30), in as many of these as its
    /// size takes. Each is a flags byte ([`WORKING_SET_LAST`] on the last), a u8 length
    /// and the name of the generation it was recorded from, then the next part of the
    /// working set as the snapshot encodes it (vmm `snapshot::encode_working_set`).
    pub const WORKING_SET: u8 = 17;
    /// VM → its spawner, before it opens anything: the paths it needs and how, for App
    /// Sandbox to let it reach them (macOS; the shards crate's `grant`). Answered with a
    /// `GRANTED` per path, in order, or an `ERR` saying why not.
    pub const GRANT: u8 = 18;
    /// Spawner → VM: what was granted for one path: a descriptor, a bookmark, or neither
    /// for a file not there.
    pub const GRANTED: u8 = 19;
    /// VM → its spawner, once it runs: a connection to the vsock host port given as a
    /// big-endian u32, on the socket `<path>_<port>` beside the vsock path it was granted
    /// to listen at. Answered with a `GRANTED` carrying the connection, or an `ERR`.
    pub const DIAL: u8 = 20;
    /// Warm VM → daemon, as its run's log fills a segment: the next segment's sequence
    /// number, a big-endian u64, which the daemon makes in the run's container and removes
    /// the oldest past the retention. Answered with a `SEGMENT` carrying the new segment's
    /// log and index, in that order, or none where it could not be made. A VM reaches no
    /// container's directory itself (D30).
    pub const LOG_SEGMENT: u8 = 21;
    /// Daemon → warm VM: the answer to its `LOG_SEGMENT`.
    pub const SEGMENT: u8 = 22;
    /// Client → daemon: run a command in a running container ([`Exec`](super::Exec)),
    /// with the client's stdin, stdout and stderr; the connection itself is its client's.
    pub const EXEC: u8 = 23;
    /// Daemon → the container's VM: run a command beside its workload. Its number (a
    /// big-endian u64, which `EXEC_TAKEN` answers), a flags byte
    /// ([`EXEC_INTERACTIVE`](super::EXEC_INTERACTIVE), [`EXEC_DETACHED`](super::EXEC_DETACHED)),
    /// then its `abi::run::Spec`, with the client's connection, stdin, stdout and stderr.
    /// The VM answers the client as a run does: what kept it from starting on its stderr,
    /// then `EXIT` with the status.
    pub const EXEC_RUN: u8 = 24;
    /// Warm VM → daemon: it has the `EXEC_RUN` numbered so (a big-endian u64), and with
    /// it the client's connection, which the daemon held until now: XNU collects a socket
    /// in flight that no process holds (M24).
    pub const EXEC_TAKEN: u8 = 25;
    /// Daemon → a VM's network process: host sockets of published ports (`-p`), each for
    /// the guest port and protocol its payload's next three bytes name: a big-endian u16,
    /// then the IP protocol number (6 TCP, 17 UDP). Connections and datagrams they take
    /// become the guest's. Said back, empty, once they are taken.
    pub const PUBLISH: u8 = 26;
    /// A VM → its network process, as its run ends: close every published port's
    /// listening socket; and back, once they are closed, so that the run's end is told
    /// only when its ports are free.
    pub const UNPUBLISH: u8 = 27;
}

/// An `EXEC_RUN` flag: the command reads the client's stdin (`-i`).
pub const EXEC_INTERACTIVE: u8 = 1;
/// An `EXEC_RUN` flag: the client is answered once the command starts (`-d`), and its
/// output goes nowhere.
pub const EXEC_DETACHED: u8 = 2;

/// `shards exec` as the client asks for it: the container, the command, and what the
/// command line set; the client's terminal size for `-t`; and the daemon binary the client
/// would start, as in [`Run`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exec {
    pub container: String,
    pub cmd: Vec<String>,
    /// `-e`, each `NAME=VALUE`, or `NAME` alone to unset NAME.
    pub env: Vec<String>,
    /// `-u`, or empty for the container's user.
    pub user: String,
    /// `-w`, or empty for the container's working directory.
    pub workdir: String,
    pub interactive: bool,
    pub detach: bool,
    pub tty: Option<(u16, u16)>,
    pub daemon: Identity,
}

impl Exec {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::new();
        put_str(&mut w, &self.container);
        put_list(&mut w, &self.cmd);
        put_list(&mut w, &self.env);
        put_str(&mut w, &self.user);
        put_str(&mut w, &self.workdir);
        w.push(u8::from(self.interactive));
        w.push(u8::from(self.detach));
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
    pub fn decode(bytes: &[u8]) -> Option<Exec> {
        let mut r = Reader(bytes);
        let exec = Exec {
            container: r.str()?,
            cmd: r.list()?,
            env: r.list()?,
            user: r.str()?,
            workdir: r.str()?,
            interactive: r.flag()?,
            detach: r.flag()?,
            tty: if r.flag()? {
                let [a, b, c, d] = <[u8; 4]>::try_from(r.take(4)?).ok()?;
                Some((u16::from_be_bytes([a, b]), u16::from_be_bytes([c, d])))
            } else {
                None
            },
            daemon: r.identity()?,
        };
        r.0.is_empty().then_some(exec)
    }
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
    /// `--network`: the network mode, the first network named or `default`, and the
    /// endpoints asked of networks, as the CLI asks dockerd for them (HostConfig's
    /// NetworkMode and NetworkingConfig's EndpointsConfig, docker/cli parseNetworkOpts).
    pub network: String,
    pub endpoints: Vec<Endpoint>,
    /// `--stop-signal`, as given.
    pub stop_signal: Option<String>,
    /// `--stop-timeout`, in seconds, if given.
    pub stop_timeout: Option<i64>,
    /// The `--health-*` settings, or `--no-healthcheck`'s `NONE`, if any was given.
    pub health: Option<Health>,
    /// `-p`'s bindings, in the order given: each a container port and protocol, a host
    /// address (empty for every one) and a host port, range or none (empty).
    pub publish: Vec<Publish>,
    /// `-P`: every exposed port without a binding gets one to a port the host picks.
    pub publish_all: bool,
    /// The daemon binary this client would start.
    pub daemon: Identity,
}

/// A health check as a run sets it, or as an image's merged with a run's: its test
/// (`CMD ...`, `CMD-SHELL cmd`, `NONE`, or empty for the image's), then durations in
/// nanoseconds and the retries, each 0 for "not set" (moby HealthConfig).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Health {
    pub test: Vec<String>,
    pub interval: i64,
    pub timeout: i64,
    pub start_period: i64,
    pub start_interval: i64,
    pub retries: i64,
}

/// One `-p` binding, as the CLI read it (shards_cmdline::ports).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Publish {
    pub port: u16,
    pub proto: String,
    pub host_ip: String,
    pub host_port: String,
}

/// What a run asks of its endpoint on one network, as the CLI read it: addresses as
/// given (the daemon checks them), empty where not asked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub network: String,
    pub aliases: Vec<String>,
    pub ipv4: String,
    pub ipv6: String,
    pub link_local: Vec<String>,
    pub mac: String,
    /// Driver options, each key once.
    pub driver_opts: Vec<(String, String)>,
    pub gw_priority: i64,
}

impl Endpoint {
    fn encode(&self, w: &mut Vec<u8>) {
        put_str(w, &self.network);
        put_list(w, &self.aliases);
        put_str(w, &self.ipv4);
        put_str(w, &self.ipv6);
        put_list(w, &self.link_local);
        put_str(w, &self.mac);
        w.extend_from_slice(
            &u32::try_from(self.driver_opts.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        for (k, v) in &self.driver_opts {
            put_str(w, k);
            put_str(w, v);
        }
        w.extend_from_slice(&self.gw_priority.to_be_bytes());
    }

    fn decode(r: &mut Reader<'_>) -> Option<Endpoint> {
        Some(Endpoint {
            network: r.str()?,
            aliases: r.list()?,
            ipv4: r.str()?,
            ipv6: r.str()?,
            link_local: r.list()?,
            mac: r.str()?,
            driver_opts: {
                let n = r.u32()? as usize;
                // Each takes at least its two 4-byte lengths.
                if n > r.0.len() / 8 {
                    return None;
                }
                (0..n)
                    .map(|_| Some((r.str()?, r.str()?)))
                    .collect::<Option<_>>()?
            },
            gw_priority: i64::from_be_bytes(r.take(8)?.try_into().ok()?),
        })
    }
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
        put_str(&mut w, &self.network);
        w.extend_from_slice(
            &u32::try_from(self.endpoints.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        for e in &self.endpoints {
            e.encode(&mut w);
        }
        put_opt(&mut w, self.stop_signal.as_deref());
        match &self.health {
            Some(h) => {
                w.push(1);
                put_list(&mut w, &h.test);
                for n in [h.interval, h.timeout, h.start_period, h.start_interval, h.retries] {
                    w.extend_from_slice(&n.to_be_bytes());
                }
            }
            None => w.push(0),
        }
        match self.stop_timeout {
            Some(t) => {
                w.push(1);
                w.extend_from_slice(&t.to_be_bytes());
            }
            None => w.push(0),
        }
        w.extend_from_slice(
            &u32::try_from(self.publish.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        for p in &self.publish {
            w.extend_from_slice(&p.port.to_be_bytes());
            put_str(&mut w, &p.proto);
            put_str(&mut w, &p.host_ip);
            put_str(&mut w, &p.host_port);
        }
        w.push(u8::from(self.publish_all));
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
            network: r.str()?,
            endpoints: {
                let n = r.u32()? as usize;
                // Each takes at least 36 bytes: seven 4-byte lengths and its priority.
                if n > r.0.len() / 36 {
                    return None;
                }
                (0..n).map(|_| Endpoint::decode(&mut r)).collect::<Option<_>>()?
            },
            stop_signal: r.opt()?,
            health: if r.flag()? {
                let test = r.list()?;
                let mut n = || r.take(8).and_then(|b| b.try_into().ok()).map(i64::from_be_bytes);
                Some(Health {
                    test,
                    interval: n()?,
                    timeout: n()?,
                    start_period: n()?,
                    start_interval: n()?,
                    retries: n()?,
                })
            } else {
                None
            },
            stop_timeout: if r.flag()? {
                Some(i64::from_be_bytes(r.take(8)?.try_into().ok()?))
            } else {
                None
            },
            publish: {
                let n = r.u32()? as usize;
                // Each takes at least 14 bytes: its port and three 4-byte lengths.
                if n > r.0.len() / 14 {
                    return None;
                }
                (0..n)
                    .map(|_| {
                        Some(Publish {
                            port: u16::from_be_bytes(r.take(2)?.try_into().ok()?),
                            proto: r.str()?,
                            host_ip: r.str()?,
                            host_port: r.str()?,
                        })
                    })
                    .collect::<Option<_>>()?
            },
            publish_all: r.flag()?,
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
/// A `kind::RUN` flag: the run publishes ports, which close before its end is told
/// (`kind::UNPUBLISH`).
pub const RUN_PUBLISHED: u8 = 16;

/// The largest payload a message may carry.
pub const MAX_PAYLOAD: usize = 1 << 20;

/// `kind::WORKING_SET`'s flag on the last part.
pub const WORKING_SET_LAST: u8 = 1;

/// A working set recorded from generation `name`, as the `kind::WORKING_SET` messages
/// that carry it, each within [`MAX_PAYLOAD`]; none for a name longer than 255 bytes, which
/// no generation's is.
pub fn working_set_parts(name: &str, set: &[u8]) -> Vec<Vec<u8>> {
    let Ok(len) = u8::try_from(name.len()) else {
        return Vec::new();
    };
    let room = MAX_PAYLOAD - 2 - name.len();
    let chunks: Vec<&[u8]> = if set.is_empty() {
        vec![&[][..]]
    } else {
        set.chunks(room).collect()
    };
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut part = Vec::with_capacity(2 + name.len() + chunk.len());
            part.push(if i == last { WORKING_SET_LAST } else { 0 });
            part.push(len);
            part.extend_from_slice(name.as_bytes());
            part.extend_from_slice(chunk);
            part
        })
        .collect()
}

/// One `kind::WORKING_SET` message's flags, generation name and part.
pub fn working_set_part(payload: &[u8]) -> Option<(u8, &str, &[u8])> {
    let (&flags, rest) = payload.split_first()?;
    let (&len, rest) = rest.split_first()?;
    let (name, part) = rest.split_at_checked(usize::from(len))?;
    Some((flags, std::str::from_utf8(name).ok()?, part))
}
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

    /// A working set goes in parts each within a message's payload, and comes back whole,
    /// the last part alone flagged so; an empty one is one part, and a name too long for
    /// its length byte sends nothing.
    #[test]
    fn working_sets_cross_in_parts_and_come_back_whole() {
        let name = "g-0000000000000000000000ff-1f-0";
        for len in [
            0,
            1,
            MAX_PAYLOAD - 2 - name.len(),
            MAX_PAYLOAD,
            3 * MAX_PAYLOAD + 7,
        ] {
            let set: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let parts = working_set_parts(name, &set);
            let mut back = Vec::new();
            for (i, part) in parts.iter().enumerate() {
                assert!(part.len() <= MAX_PAYLOAD, "{len}: part {i}");
                let (flags, got, bytes) = working_set_part(part).unwrap();
                assert_eq!(got, name);
                assert_eq!(flags == WORKING_SET_LAST, i == parts.len() - 1, "{len}: part {i}");
                back.extend_from_slice(bytes);
            }
            assert_eq!(back, set, "{len}");
        }
        assert!(working_set_parts(&"g".repeat(256), b"x").is_empty());
        assert!(working_set_part(&[0, 9, b'a']).is_none(), "a name past its end");
    }

    #[test]
    fn execs_round_trip_and_nothing_else_decodes() {
        let exec = Exec {
            container: "web".into(),
            cmd: vec!["sh".into(), "-c".into(), "env".into()],
            env: vec!["A=1".into(), "B".into()],
            user: "1000".into(),
            workdir: "/w".into(),
            interactive: true,
            detach: false,
            tty: Some((24, 80)),
            daemon: Identity {
                dev: 1,
                ino: 2,
                size: 3,
                mtime_s: 4,
                mtime_ns: 5,
            },
        };
        let bytes = exec.encode();
        assert_eq!(Exec::decode(&bytes), Some(exec));
        for n in 0..bytes.len() {
            assert_eq!(Exec::decode(&bytes[..n]), None, "an exec cut at {n} decoded");
        }
        assert_eq!(Exec::decode(&Exec::default().encode()), Some(Exec::default()));
    }

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
            network: "bridge".into(),
            endpoints: vec![
                Endpoint {
                    network: "bridge".into(),
                    aliases: vec!["a".into()],
                    ipv4: "172.17.0.9".into(),
                    ipv6: "fd00::5".into(),
                    link_local: vec!["169.254.1.1".into(), "fe80::2".into()],
                    mac: "02:11:22:33:44:55".into(),
                    driver_opts: vec![("k".into(), "v".into()), ("l".into(), String::new())],
                    gw_priority: -7,
                },
                Endpoint::default(),
            ],
            stop_signal: Some("SIGUSR1".into()),
            stop_timeout: Some(-1),
            publish: vec![
                Publish {
                    port: 80,
                    proto: "tcp".into(),
                    host_ip: "127.0.0.1".into(),
                    host_port: "8000-8010".into(),
                },
                Publish::default(),
            ],
            publish_all: true,
            health: Some(Health {
                test: vec!["CMD-SHELL".into(), "true".into()],
                interval: 1,
                timeout: 2,
                start_period: 3,
                start_interval: 4,
                retries: -5,
            }),
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

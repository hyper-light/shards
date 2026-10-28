//! How shards' processes talk: the CLI, the daemon and warm VMM processes hand each other
//! requests, stdio and connections as messages (`kind`), the run a client asks for
//! ([`Run`]), and, on Unix, the transport that carries them with open descriptors, and the
//! socket where they meet, in their shared [`home`] (`unix.rs`).
//!
//! The `shards` command links this crate and the standard library alone, so that it
//! starts fast (docs/research/platform-measurements.md M23).

use std::path::{Path, PathBuf};

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

/// What a daemon that no longer serves keeps in its home while it ends its runs: its
/// process ID. A daemon or client that finds none serving waits for that one while it
/// lives, as long as its runs' stop timeouts make it, and then takes its place.
pub const STOPPING: &str = "daemon.stopping";

/// Whether the daemon that `STOPPING` in `home` names still lives, ending its runs.
pub fn exiting(home: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(home.join(STOPPING)) else {
        return false;
    };
    #[cfg(unix)]
    let alive = unix::alive;
    #[cfg(windows)]
    let alive = windows::alive;
    text.trim().parse::<u32>().is_ok_and(alive)
}

/// Where a daemon the client starts writes its messages.
pub fn log(home: &Path) -> PathBuf {
    home.join("daemon.log")
}

/// The first word of `shards cp`'s steps, which no command line has: what its client asks
/// the daemon one at a time (cli/cp.rs, daemon/commands.rs).
pub const COPY_STEP: &str = "\u{0}cp";

/// Message kinds between the daemon, warm VMs and clients.
pub mod kind {
    /// Warm VM → daemon: the guest is connected and waiting for its command.
    pub const READY: u8 = 1;
    /// Daemon → warm VM: [`RUN_INTERACTIVE`](super::RUN_INTERACTIVE) flags, the most
    /// bytes a segment of the container's log holds, the most segments it keeps, and the
    /// segment the run's log starts in (each a big-endian u64), then the command
    /// (`shards_abi::run::Spec`). Descriptors: the
    /// client's connection, then its stdin, stdout and stderr, then with
    /// [`RUN_LOG`](super::RUN_LOG) the container's directory, where its log is. With
    /// [`RUN_DETACHED`](super::RUN_DETACHED), only the command's stdin and the directory.
    /// Last, where the VM shares directories with its guest (D38), its connection to the
    /// run's share process, which sends a connection for each ([`SHARE_ENDS`]).
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
    /// VM → its spawner: it has the connection a `DIAL` was answered with, which the
    /// spawner may close, so that the far end sees the VM's close as the end; and a build
    /// → its daemon, of the connection to an SSH agent its `DIAL` was answered with.
    pub const TAKEN: u8 = 8;
    /// Warm VM → daemon: the command ended with this status (one byte); then, if the
    /// command never ran, why not. Sent before the client has the status, as `STARTED`
    /// is before the client has anything the command wrote: the daemon's commands, which
    /// first take what every run has sent, see what any client has seen.
    pub const DONE: u8 = 9;
    /// Warm VM → daemon: the command is executing: it started, where `DONE` without this
    /// means it never did. A joiner's (D119) carries its exec's id in its provider's guest
    /// (u32, big-endian), by which another joiner may join its PID namespace.
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
    /// to listen at. Answered with a `GRANTED` carrying the connection, or an `ERR`. And a
    /// build → its daemon, empty, on the pair it was given for one of its client's SSH
    /// agents: a connection to that agent, answered the same way.
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
    /// Daemon → client: the run's container is made. Until then, a signal that would end
    /// the client ends it, and with it the run, as one ends the Docker CLI's request until
    /// `ContainerCreate` returns (docker/cli cmd/docker/docker.go, notifyContext); from
    /// then on, the client passes signals on to the command. Its payload is the
    /// container's ID.
    pub const CREATED: u8 = 28;
    /// Daemon → client on a colour terminal: how a pull goes, one
    /// [`Progress`](super::Progress) a message, for the client to show as it likes, where
    /// a client that is not on a terminal gets `docker pull`'s lines.
    pub const PROGRESS: u8 = 29;
    /// Daemon → client on a colour terminal: what a command found, as a
    /// [`Sheet`](super::Sheet) of records, for the client to lay out as its own page,
    /// where a client that is not on a terminal gets `docker`'s text.
    pub const SHEET: u8 = 30;
    /// Warm VM → daemon: the `EXEC_RUN` numbered so (a big-endian u64) has ended, with
    /// the status after it (a byte), for its `exec_die` event.
    pub const EXEC_ENDED: u8 = 31;
    /// Warm VM → daemon, before `DONE`: the container's writable layer is whole where
    /// `RUN_LAYER_OUT` said to write it; with the disk it used in the guest (a big-endian
    /// u64), where the guest said.
    pub const LAYER_SAVED: u8 = 32;
    /// Warm VM → daemon, before DONE: the guest killed a process of the command for want
    /// of memory (shards_abi::run::kind::OOM).
    pub const OOM: u8 = 33;
    /// Share process → VM, once the VM has its run: a connection for each directory it
    /// shares, in its devices' order, [`MAX_FDS`](super::MAX_FDS) a message at most; the VM
    /// answers [`TAKEN`] once it has them all, before which the share process keeps its
    /// copies (shards_ipc's flush in flight, PM M24).
    pub const SHARE_ENDS: u8 = 34;
    /// Client → daemon: `shards attach` ([`Attach`]), with the client's stdin, stdout and
    /// stderr; the VM answers the client as it does an exec's.
    pub const ATTACH: u8 = 35;
    /// Daemon → warm VM: a client to attach to the running command, with its connection,
    /// stdin, stdout and stderr; the payload is its number, as an exec's, and its
    /// [`ATTACH_STDIN`] flag. The VM answers [`EXEC_TAKEN`] with the number.
    pub const ATTACH_RUN: u8 = 36;
    /// Daemon → network process: the guest's address on its network (D46), its prefix and
    /// gateway, each IPv4 address's four bytes and the prefix's one; answered with the same
    /// kind once taken.
    pub const NET_ADDRESS: u8 = 37;
    /// Daemon → network process: a peer on the guest's network, its guest's address's four
    /// bytes, with a stream socket to its network process; answered with the same kind once
    /// taken, as the daemon holds its copy until then (M24).
    pub const NET_PEER: u8 = 38;
    /// Daemon → network process: the names the guest's network answers to (D46), whole,
    /// each time its members change (shards_net::dns::Names::encode); answered with the
    /// same kind.
    pub const NET_NAMES: u8 = 39;
    /// Daemon → network process: the ports its guest may open flows to, its image's
    /// Agentfile's egress grants (D59, shards_net::Ports::encode); answered with the same
    /// kind once taken.
    pub const NET_POLICY: u8 = 40;
    /// Daemon → a running container's VM: a container joins its network (D119). Its number
    /// (a big-endian u64, which `EXEC_TAKEN` answers), with the VM's end of the joiner's
    /// own connection to the daemon, on which the joiner's run goes as a warm VM's does
    /// (`RUN`, `TAKEN`, `STARTED`, `DONE`, its log's segments), and its image's EROFS file,
    /// which the VM gives a range of its join disk.
    pub const JOIN: u8 = 41;
    /// Daemon → a running container's VM: its number, then its join share's connection to
    /// the share process that serves it (D119), as the first container joining its network
    /// brings volumes; answered `EXEC_TAKEN`, as a joiner's `JOIN` is.
    pub const JOIN_SHARE: u8 = 42;
    /// Daemon → a join share process: a joiner's link to it, on which its volumes come
    /// (`JOIN_VOLUME`), served until it closes as the joiner ends.
    pub const JOIN_LINK: u8 = 43;
    /// Daemon → a join share process, on a joiner's link: a volume to serve,
    /// `NAME\0MODE\0ONLY` (`ro` or `rw`, and the one name of it a file is bound by, or
    /// none), with its directory; answered `TAKEN`, or `ERR` and why.
    pub const JOIN_VOLUME: u8 = 44;
    /// Daemon → a VM's network process: the guest's MAC from its run on (6 bytes), which
    /// its frames come from and go to, as init gives eth0 it (D46); answered `NET_MAC`.
    pub const NET_MAC: u8 = 45;
    /// Daemon → a VM's network process: the guest's link-local addresses on its network
    /// (PM M175), each a length, 4 or 16, then its octets; answered `NET_LINK_LOCAL`.
    pub const NET_LINK_LOCAL: u8 = 46;
    /// A VM → its daemon, as they come: records of the processes its run's members started
    /// and ended (D123), as shards-init writes them, for the container's record.
    pub const PROCESS: u8 = 47;
    /// Daemon → a VM's network process: a port of the host's network (`--network host`,
    /// D124), its guest's bind made the host's: the daemon's number for it (a big-endian
    /// u32), then the guest port and protocol as `PUBLISH`'s, with its host socket. Not
    /// answered: the daemon holds its own copy of the socket until it lets the port go.
    pub const NET_PUBLISHED: u8 = 48;
    /// Daemon → a VM's network process: the port of the host's network numbered so (a
    /// big-endian u32) let go, as the guest's socket was; answered with the same kind once
    /// its sockets are closed, so that a bind of the port may wait for it.
    pub const NET_UNPUBLISHED: u8 = 49;
    /// A VM on the host's network → its daemon (D124): its guest's ask of the host, the
    /// guest's frame's kind (`shards_abi::run::kind::BIND`, `LISTEN` or `UNBIND`), then its
    /// payload.
    pub const HOST_NET: u8 = 50;
    /// Daemon → a VM on the host's network: the host's answer to its guest's `BIND` or
    /// `LISTEN` (`shards_abi::run::HostBound`), for its guest.
    pub const HOST_BOUND: u8 = 51;
    /// A VM on the host's network → its daemon, as a container in it ends, before its end
    /// is told: its ports of the host's let go (`shards_abi::run::HostBind::owner`, a
    /// big-endian u32); answered with the same kind and owner once the daemon's copies and
    /// the network process's are closed, so that the host may bind them again once the
    /// end is told, as dockerd's may.
    pub const HOST_RELEASE: u8 = 52;
    /// A builder → its VM process (`--attach`, D134): a file for the VM's join disk, its
    /// one descriptor, the guest to read it alone (payload 0) or to write it too (1).
    /// Answered with the same kind and where its range starts and its length (each a
    /// big-endian u64), or `DISK_REFUSED`.
    pub const DISK_ATTACH: u8 = 53;
    /// A builder → its VM process: the join disk's range that starts where it says (a
    /// big-endian u64) let go, its file closed. Answered with the same kind, or
    /// `DISK_REFUSED`.
    pub const DISK_DETACH: u8 = 54;
    /// A VM process → its builder: why it did not attach or detach what was asked.
    pub const DISK_REFUSED: u8 = 55;
    /// Daemon → a VM, on its link (D176): save the running microVM into a generation the
    /// daemon prepared (`shards_vmm::snapshot::prepare`). Payload: `SAVE_*` flags, then the
    /// generation's name, the snapshot's and the microVM's, each after a NUL but the first;
    /// descriptors its memory, its state, and the run's swap where the run has one.
    /// Answered with `SAVED`.
    pub const SAVE: u8 = 56;
    /// A VM → its daemon: the save asked for is written, an empty payload, or why it is
    /// not. A microVM that stops after its save stops once this is sent.
    pub const SAVED: u8 = 57;
    /// Daemon → a VM running a run: how its run stops where the daemon is gone, its stop
    /// signal (a big-endian u32, 0 for SIGKILL at once) and its grace (a big-endian u64 of
    /// milliseconds, `u64::MAX` for ever). A VM whose daemon's connection closes as its run
    /// goes on stops it so, as `shards stop daemon` would have: no run outlives its daemon
    /// unreachable.
    pub const STOP_ORDER: u8 = 58;
    /// A VM → its daemon, on its run's link: one end of a connection its command made to
    /// its own microVM's engine (`--use-api-socket`, D181), the run's engine token its first
    /// bytes, for the daemon to check and serve with its Engine API, scoped to that run's
    /// microVM.
    pub const ENGINE_CONN: u8 = 59;
    /// A VM → its daemon, on a container's own link its microVM's engine made (D181),
    /// before `STARTED`: the port each of its bindings was given in that microVM's
    /// command's network namespace, a big-endian u16 each, in their order.
    pub const PORTS: u8 = 60;
    /// A VM → its daemon, on a container's own link its microVM's engine made (D181),
    /// before `PORTS` and `STARTED`: its membership of that microVM's switch, as text, as
    /// init said it (`shards_abi::run::kind::MEMBER`).
    pub const MEMBER: u8 = 61;
    /// Daemon → client, a run's, an exec's or an attach's: its request is this daemon's to
    /// serve, which will not ask it to restart. A client of a slot fills its command's
    /// stdin from then on: what it wrote to the slot of a daemon that asked it to restart
    /// would be lost with that slot (D185).
    pub const HEARD: u8 = 62;
    /// Daemon → a running run's VM: the most memory its guest may plug past what it booted
    /// with, in bytes (8, big-endian), as `update -m` sets the run's limit (D148): before
    /// its guest's cgroup is written, so that what the guest reports it may plug is the
    /// new most (D149).
    pub const PLUG: u8 = 63;
    /// A VM → its daemon (Linux): a file its share's DAX window has begun to map (D189), for
    /// the daemon to watch for other processes cutting it short: the share's place among
    /// the VM's shares (4, big-endian), the node of its server whose file it is (8) and the
    /// file's size as the window mapped it (8), with the file's descriptor.
    pub const WATCH: u8 = 64;
    /// A VM → its daemon (Linux): a file its share's DAX window maps no more, by its share's
    /// place (4, big-endian) and its node (8), as `WATCH` named it.
    pub const UNWATCH: u8 = 65;
    /// Daemon → a VM (Linux): a file `WATCH` named was cut short by another process, by its
    /// share's place (4, big-endian) and its node (8): the window maps it as it is now,
    /// zeros where it has no pages, before its guest reads there (PM M273).
    pub const CUT: u8 = 66;
    /// Warm VM → daemon, in `LAYER_SAVED`'s place: the container's writable layer stays in
    /// its own file, an ext4 file system its guest's journal written in place (D190), said
    /// once nothing of the VM's machine writes it, the file the daemon's from then on;
    /// with the disk it used in the guest (a big-endian u64), where the guest said.
    pub const LAYER_KEPT: u8 = 67;
    /// Daemon → a running run's VM: the CPUs whose time its vCPUs may use of the host's
    /// together (8, big-endian), 0 for every one, as `update` changes the run's CPU limits
    /// (D191): before its guest's cgroup is written.
    pub const CPUS: u8 = 68;
    /// Warm VM → daemon, before DONE: the guest refused the command a process or thread
    /// past its `--pids-limit` (shards_abi::run::kind::PIDS), and what its cgroup was
    /// allowed and held (D193).
    pub const PIDS: u8 = 69;
    /// Client → daemon, or its run's VM: a signal that would end the client, SIGINT,
    /// SIGTERM, SIGHUP or SIGQUIT where it was not ignored as the client started, as
    /// `SIGNAL`'s payload. Before the client's run is made (its image pulled and its spec
    /// prepared), its daemon takes it as the client's end, answers `INTERRUPTED`, and makes
    /// no container, as a Docker CLI interrupted then ends; after, it is the command's, as
    /// `SIGNAL` is (D26).
    pub const INTERRUPT: u8 = 70;
    /// Daemon → client: the `INTERRUPT` it took before the client's run was made, its
    /// payload as it came: the client ends as that signal ends the Docker CLI before its
    /// container is made (D26).
    pub const INTERRUPTED: u8 = 71;
    /// VM → its spawner, on its grants link, where it writes in no directory of its own
    /// (Windows' AppContainer, D141): stage a snapshot generation in the directory it was
    /// granted to write in, named as `GRANT` names a path, after a byte that says what the
    /// generation keeps beside its memory and state (1 its writable layer, 2 its DAX
    /// windows' heads). Answered with a `GRANTED` carrying the handles in the VM of the
    /// generation's memory file, state file, layer head and layer file, and windows' heads
    /// file, those it keeps, each a little-endian u64, then the generation's name; or an
    /// `ERR` (D196).
    pub const STAGE: u8 = 72;
    /// VM → its spawner: the generation the payload names, staged and filled, is to be made
    /// durable and the one in use. Answered with an empty `GRANTED`, or an `ERR` (D196).
    pub const COMMIT: u8 = 73;
    /// Daemon → a running run's VM: its microVM paused where it is (1), every vCPU held out
    /// of its guest, or let go on (0), as `pause` and `unpause` ask; then the ask's number
    /// (8, big-endian). Answered with a `PAUSED`.
    pub const PAUSE: u8 = 74;
    /// Warm VM → daemon: the `PAUSE` its number (8, big-endian) names done, every vCPU held
    /// or let go (nothing after it), or why not.
    pub const PAUSED: u8 = 75;
    /// VM → its spawner, on its grants link, where it may ask its file system nothing of a
    /// file's ranges (Windows' AppContainer, which makes no FSCTL, D141): which of a file of
    /// the VM's hold data, the file's handle in the VM and the bytes looked at, each a
    /// little-endian u64. Answered with a `GRANTED` of the ranges, each its offset and its
    /// length, little-endian u64s; or an `ERR` (D196).
    pub const RANGES: u8 = 76;

    /// Whether a network process's control message of `kind` hands it sockets: published
    /// ports' (`PUBLISH`, `NET_PUBLISHED`) and a peer's link (`NET_PEER`). Beside the
    /// message on Unix; on Windows, where a socket is shared with a process by its protocol
    /// info, before its payload, however many (`with_sockets`).
    pub fn hands_sockets(kind: u8) -> bool {
        matches!(kind, PUBLISH | NET_PUBLISHED | NET_PEER)
    }
}

/// A `SAVE` flag: the microVM goes on running once saved, rather than stopping.
pub const SAVE_LEAVE_RUNNING: u8 = 1;

/// The status of a run its save stopped (D176), as a shell says of a job it stopped, 128
/// plus SIGTSTP's number on Linux (20): not ended, but stopped where it is, to be resumed.
pub const SAVED_STATUS: u8 = 148;

/// An `ATTACH_RUN` flag: the client's stdin goes to the command's.
pub const ATTACH_STDIN: u8 = 1;

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
    /// The client's stdin is a terminal, as `-it` needs: the daemon refuses one that is
    /// not once it has found the container, where the CLI refuses it after its inspect.
    pub stdin_terminal: bool,
    pub daemon: Identity,
    /// `--privileged`: every capability.
    pub privileged: bool,
    /// `-t` from a client that says what its terminal is: its terminal types, as a run's
    /// ([`Run::terms`]).
    pub terms: Vec<String>,
    /// Its ID, where it has one already: the Engine API's, made with the exec and said in
    /// its `exec_create`, as dockerd has it (moby daemon/exec.go); empty for `shards
    /// exec`'s, which the daemon makes as it starts it.
    pub id: String,
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
        w.push(u8::from(self.stdin_terminal));
        put_identity(&mut w, &self.daemon);
        w.push(u8::from(self.privileged));
        put_list(&mut w, &self.terms);
        put_str(&mut w, &self.id);
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
            stdin_terminal: r.flag()?,
            daemon: r.identity()?,
            privileged: r.flag()?,
            terms: r.list()?,
            id: r.str()?,
        };
        r.0.is_empty().then_some(exec)
    }
}

/// `shards attach` as the client asks for it: the container, whether its stdin goes to the
/// command's, and the daemon binary the client would start, as in [`Run`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attach {
    pub container: String,
    pub stdin: bool,
    pub daemon: Identity,
}

impl Attach {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::new();
        put_str(&mut w, &self.container);
        w.push(u8::from(self.stdin));
        put_identity(&mut w, &self.daemon);
        w
    }

    /// `None` for anything but a whole, well-formed request.
    pub fn decode(bytes: &[u8]) -> Option<Attach> {
        let mut r = Reader(bytes);
        let attach = Attach {
            container: r.str()?,
            stdin: r.flag()?,
            daemon: r.identity()?,
        };
        r.0.is_empty().then_some(attach)
    }
}

/// A container command as the client asks for it (`kind::CONTAINER`): its name and the
/// words after it, which the client has checked; what of the client's locale shapes the
/// answer; and the daemon binary the client would start, as in [`Run`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Command {
    pub argv: Vec<String>,
    /// The client's [`REGISTRY_ENV`]: a registry it reaches is reached with its
    /// credentials and certificates, as the Docker CLI sends its own with each request.
    pub registry_env: Vec<String>,
    /// The client's locale is East Asian, where the Docker CLI counts ambiguous-width
    /// characters as two columns (shards_cmdline::width::east_asian).
    pub east_asian: bool,
    /// The client's clock, in nanoseconds since the epoch, and its time zone's offset east
    /// of UTC then, in seconds: the Docker client reads `logs --since` by them.
    pub now: i64,
    pub utc_offset: i32,
    /// Its stdout is a terminal of `width` columns (0 when it says none), and colours are
    /// welcome there (no `NO_COLOR`): `images` lays its tree out by them.
    pub terminal: bool,
    pub width: u16,
    pub color: bool,
    pub daemon: Identity,
}

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::new();
        put_list(&mut w, &self.argv);
        put_list(&mut w, &self.registry_env);
        w.push(u8::from(self.east_asian));
        w.extend_from_slice(&self.now.to_be_bytes());
        w.extend_from_slice(&self.utc_offset.to_be_bytes());
        w.push(u8::from(self.terminal));
        w.extend_from_slice(&self.width.to_be_bytes());
        w.push(u8::from(self.color));
        put_identity(&mut w, &self.daemon);
        w
    }

    /// `None` for anything but a whole command.
    pub fn decode(bytes: &[u8]) -> Option<Command> {
        let mut r = Reader(bytes);
        let command = Command {
            argv: r.list()?,
            registry_env: r.list()?,
            east_asian: r.flag()?,
            now: r.u64()? as i64,
            utc_offset: r.u32()? as i32,
            terminal: r.flag()?,
            width: u16::from_be_bytes(r.take(2)?.try_into().ok()?),
            color: r.flag()?,
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

impl Identity {
    /// The build a daemon binary belongs to, as clients and the daemon tell builds apart:
    /// its file's identity, folded with that of the VM binary `SHARDS_VM_BINARY` names in
    /// its place, if it names one. The binaries it starts are otherwise carried inside it
    /// (shards' helpers.rs), so any of them rebuilt is this file changed; a daemon given
    /// another VM binary is another build, which a client with its own replaces.
    pub fn of_build(daemon: &Path) -> std::io::Result<Identity> {
        let d = Identity::of(daemon)?;
        let Some(vm) = std::env::var_os("SHARDS_VM_BINARY").filter(|v| !v.is_empty()) else {
            return Ok(d);
        };
        let v = Identity::of(Path::new(&vm))?;
        Ok(Identity {
            dev: d.dev ^ v.dev.rotate_left(17),
            ino: d.ino ^ v.ino.rotate_left(29),
            size: d.size ^ v.size.rotate_left(37),
            mtime_s: d.mtime_s ^ v.mtime_s.rotate_left(41),
            mtime_ns: d.mtime_ns ^ v.mtime_ns.rotate_left(13),
        })
    }
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
    /// The client's [`REGISTRY_ENV`], for the image's pull.
    pub registry_env: Vec<String>,
    /// `shards start`: the container, as given, to run again as it was made, its
    /// writable layer put back (D37); the rest of this request says only how its client
    /// attaches.
    pub again: Option<String>,
    /// `shards restore vm` (D176): with `again`, the container resumed from a snapshot of
    /// its own, by name; empty for its newest.
    pub resume: Option<String>,
    /// With `again`, the client says nothing of the start but what fails, as docker/cli's
    /// `start --checkpoint` says nothing (container/start.go).
    pub silent: bool,
    /// `shards create`: the container is made, and not started.
    pub create: bool,
    /// `shards restart`: with `again`, the container is stopped first if it runs, by
    /// `stop_signal` and `stop_timeout` where given, else as it stops.
    pub restart: bool,
    /// The daemon binary this client would start.
    pub daemon: Identity,
    /// `--label`s, after `--label-file`'s, each `KEY=VALUE` or `KEY`.
    pub labels: Vec<String>,
    /// `--expose`: ports and ranges, each `PORT[-PORT][/PROTO]`.
    pub expose: Vec<String>,
    /// `--add-host`, each `HOST:IP` (or `HOST:host-gateway`), as the CLI gives dockerd.
    pub add_hosts: Vec<String>,
    /// `--dns`, `--dns-search` and `--dns-option`.
    pub dns: Vec<String>,
    pub dns_search: Vec<String>,
    pub dns_options: Vec<String>,
    /// `--domainname`.
    pub domainname: String,
    /// `--cidfile`: the file its client writes the container's ID to, absolute.
    pub cidfile: String,
    /// `-q`: a pull says nothing.
    pub quiet: bool,
    /// `--platform`, as given (or DOCKER_DEFAULT_PLATFORM): the image's to run.
    pub platform: String,
    /// The resource flags, as docker/cli sends them.
    pub resources: Resources,
    /// `--read-only`.
    pub read_only: bool,
    /// `--tmpfs`, each `DEST[:OPTIONS]`.
    pub tmpfs: Vec<String>,
    /// `--shm-size` in bytes, or 0 for the default.
    pub shm_size: i64,
    /// `--ulimit`, each `NAME=SOFT[:HARD]`.
    pub ulimits: Vec<String>,
    /// `--sysctl`, each `KEY=VALUE`.
    pub sysctls: Vec<String>,
    /// `--cap-add` and `--cap-drop`, as docker/cli sends them (EffectiveCapAddCapDrop):
    /// `CAP_` names or `ALL`, each once, sorted, an added one not dropped.
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    /// `--group-add`, each a group's name or number.
    pub group_add: Vec<String>,
    /// `--oom-score-adj`.
    pub oom_score_adj: i64,
    /// `--privileged`.
    pub privileged: bool,
    /// `--security-opt`, as docker/cli sends them (HostConfig.SecurityOpt): a seccomp
    /// profile's file read in, `seccomp=` and its JSON compacted; `systempaths=` taken out.
    pub security_opt: Vec<String>,
    /// `--security-opt systempaths=unconfined`: no paths masked or made read-only
    /// (HostConfig.MaskedPaths and ReadonlyPaths empty).
    pub system_paths: bool,
    /// `-v`'s binds and named volumes (HostConfig.Binds), each `SOURCE:DEST[:MODE]`, a
    /// relative host path made absolute.
    pub binds: Vec<String>,
    /// `-v DEST`'s anonymous volumes (Config.Volumes).
    pub volumes: Vec<String>,
    /// `--mount`, each as shards_cmdline::mounts::encode writes it.
    pub mounts: Vec<String>,
    /// `--volumes-from`, each `CONTAINER[:MODE]`.
    pub volumes_from: Vec<String>,
    /// `--volume-driver`.
    pub volume_driver: String,
    /// `--restart`, as docker/cli sends it (ParseRestartPolicy): the policy's name, empty
    /// where `--restart ""` gave none, and its most retries.
    pub restart_policy: (String, i64),
    /// `--init` where given (HostConfig.Init, a `*bool`): with true, a reaper, docker-init's
    /// part, is PID 1 of the command's PID namespace (D115); else the command is.
    pub init_process: Option<bool>,
    /// `--pid` (HostConfig.PidMode): empty for a PID namespace of the command's own,
    /// `host` for the microVM's own (D115).
    pub pid: String,
    /// `--device`s, as docker/cli parses them (HostConfig.Devices): each
    /// `PATH_ON_HOST:PATH_IN_CONTAINER:PERMISSIONS`, the host being the VM.
    pub devices: Vec<String>,
    /// `--device-cgroup-rule`s (HostConfig.DeviceCgroupRules).
    pub device_cgroup_rules: Vec<String>,
    /// `--device`s that name CDI devices (HostConfig.DeviceRequests, driver `cdi`).
    pub cdi_devices: Vec<String>,
    /// `--gpus`, each as given (HostConfig.DeviceRequests, shards_cmdline::gpus): this
    /// Mac's GPU, where one asks for any (D144).
    pub gpus: Vec<String>,
    /// `--blkio-weight-device`s, each `PATH:WEIGHT` (HostConfig.BlkioWeightDevice).
    pub blkio_weight_device: Vec<String>,
    /// `--device-read-bps`, `--device-write-bps`, `--device-read-iops` and
    /// `--device-write-iops`, each `PATH:RATE`, bytes or operations a second.
    pub device_read_bps: Vec<String>,
    pub device_write_bps: Vec<String>,
    pub device_read_iops: Vec<String>,
    pub device_write_iops: Vec<String>,
    /// `--annotation`s, each `KEY=VALUE` or `KEY` as given (HostConfig.Annotations, a map
    /// whose last value of a key stands).
    pub annotations: Vec<String>,
    /// `--isolation` (HostConfig.Isolation), as given.
    pub isolation: String,
    /// `--runtime` (HostConfig.Runtime), as given: empty for the default.
    pub runtime: String,
    /// `--ipc` (HostConfig.IpcMode), as given (D156).
    pub ipc: String,
    /// `--uts` (HostConfig.UTSMode), as given: `host` for the host's hostname (D156).
    pub uts: String,
    /// `--userns` (HostConfig.UsernsMode), as given (D156).
    pub userns: String,
    /// `--cgroupns` (HostConfig.CgroupnsMode), as given (D156).
    pub cgroupns: String,
    /// `--umask`, where given (D156).
    pub umask: Option<u32>,
    /// `--storage-opt`s, each `KEY=VALUE` (HostConfig.StorageOpt, D156).
    pub storage_opt: Vec<String>,
    /// `-a`: the streams its client attaches, `stdin`, `stdout` and `stderr`; none given,
    /// stdout and stderr, and stdin with `-i` (docker/cli opts.go).
    pub attach: Vec<String>,
    /// `--log-driver`, as given; empty for shards' own (D161).
    pub log_driver: String,
    /// `--log-opt`s, each `KEY=VALUE` or `KEY`.
    pub log_opts: Vec<String>,
    /// The client's environment a log driver that ships to a cloud finds its credentials
    /// by, `NAME=VALUE` (awslogs': `AWS_*`, the home, `PATH` and the proxies, D168): as the
    /// client's shell would find them, not as the daemon's happened to be when it started.
    /// The daemon keeps it in memory alone, never in the request a container keeps.
    pub cloud_env: Vec<String>,
    /// The streams an Engine API create says its client attaches, as it says them (D170);
    /// `None` for a run a command line made, whose `attach`, `interactive` and `detach`
    /// say them.
    pub streams: Option<Streams>,
    /// The GPU requests an Engine API create gives (HostConfig.DeviceRequests but CDI's),
    /// as it gives them (D170); a command line's are `gpus`.
    pub device_requests: Vec<DeviceRequest>,
    /// `--use-api-socket`: its command reaches its own microVM's engine (D181).
    pub use_api_socket: bool,
    /// Made through a microVM's own engine (D181): that microVM's container ID, and the
    /// name the container has there, none for one its engine names.
    pub engine_made: Option<(String, String)>,
    /// Binds of its microVM's own files (D181), each `SRC\0DST\0ro|rw\0PROPAGATION`, SRC a
    /// path in the files of the command whose engine made it, as a bind names a path of its
    /// engine's host: none of the host's.
    pub own_binds: Vec<String>,
    /// Made through a microVM's own engine (D181): the first network of that engine's it is
    /// on, a member of the microVM's switch (`bridge`, its default); `none` for its loopback
    /// alone; none for the microVM's own namespace, its engine's host's (`--network host`).
    pub engine_net: String,
    /// Made through a microVM's own engine (D181): each network of that engine's it is on,
    /// the first [`engine_net`](Self::engine_net), the address its client asked for there
    /// (empty for one its microVM gives it) and its aliases there, as
    /// `NETWORK\0ADDR[\0ALIAS...]`.
    pub engine_nets: Vec<String>,
    /// `--cgroup-parent`: the slice of the user's service manager its microVM's processes
    /// are made in (D183), as given; empty for none.
    pub cgroup_parent: String,
    /// `-t` from a client that says what its terminal is: the terminal types it is, best
    /// first, of which the command is told the first its root describes where its
    /// environment names no TERM; empty where the client says none, as an Engine API client
    /// says none.
    pub terms: Vec<String>,
}

/// The streams a create says its client attaches (Config.AttachStdin, AttachStdout and
/// AttachStderr), and whether its stdin closes once the first client that attached it
/// leaves (StdinOnce).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Streams {
    pub stdin: bool,
    pub stdout: bool,
    pub stderr: bool,
    pub stdin_once: bool,
}

impl Streams {
    fn values(&self) -> Vec<String> {
        [self.stdin, self.stdout, self.stderr, self.stdin_once]
            .map(|on| if on { "1" } else { "0" }.to_string())
            .to_vec()
    }

    fn of(values: &[String]) -> Option<Streams> {
        let on = |i: usize| match values.get(i).map(String::as_str) {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        };
        if values.len() != 4 {
            return None;
        }
        Some(Streams {
            stdin: on(0)?,
            stdout: on(1)?,
            stderr: on(2)?,
            stdin_once: on(3)?,
        })
    }
}

/// A request for devices of a driver (moby's container.DeviceRequest): how many (-1 for
/// all), or which by ID; the capabilities they must have, any one set of them; and the
/// driver's options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceRequest {
    pub driver: String,
    pub count: i64,
    pub device_ids: Option<Vec<String>>,
    pub capabilities: Vec<Vec<String>>,
    /// Each key once, in order.
    pub options: Vec<(String, String)>,
}

impl DeviceRequest {
    /// As a named field's values: its driver and count; its IDs' number, `-` for none
    /// given, and each; its sets' number, and each set's number and capabilities; its
    /// options' number, and each key and value.
    fn put(&self, out: &mut Vec<String>) {
        out.push(self.driver.clone());
        out.push(self.count.to_string());
        match &self.device_ids {
            Some(ids) => {
                out.push(ids.len().to_string());
                out.extend(ids.iter().cloned());
            }
            None => out.push("-".into()),
        }
        out.push(self.capabilities.len().to_string());
        for set in &self.capabilities {
            out.push(set.len().to_string());
            out.extend(set.iter().cloned());
        }
        out.push(self.options.len().to_string());
        for (k, v) in &self.options {
            out.push(k.clone());
            out.push(v.clone());
        }
    }

    fn take(values: &mut std::slice::Iter<'_, String>) -> Option<DeviceRequest> {
        let mut next = || values.next().cloned();
        let number = |n: Option<String>| n?.parse::<usize>().ok();
        let driver = next()?;
        let count = next()?.parse().ok()?;
        let device_ids = match next()?.as_str() {
            "-" => None,
            n => {
                let n: usize = n.parse().ok()?;
                Some((0..n).map(|_| next()).collect::<Option<Vec<_>>>()?)
            }
        };
        let sets = number(next())?;
        let mut capabilities = Vec::new();
        for _ in 0..sets {
            let n = number(next())?;
            capabilities.push((0..n).map(|_| next()).collect::<Option<Vec<_>>>()?);
        }
        let n = number(next())?;
        let options = (0..n)
            .map(|_| Some((next()?, next()?)))
            .collect::<Option<Vec<_>>>()?;
        Some(DeviceRequest {
            driver,
            count,
            device_ids,
            capabilities,
            options,
        })
    }
}

/// `run`'s resource flags as docker/cli sends them (container.Resources): memory in
/// bytes, CPUs in billionths, the rest as given; zero, empty or `None` where not given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    /// `-m`.
    pub memory: i64,
    /// `--memory-reservation`.
    pub memory_reservation: i64,
    /// `--memory-swap`: memory and swap together, -1 for no limit on swap.
    pub memory_swap: i64,
    /// `--memory-swappiness`, unless -1.
    pub memory_swappiness: Option<i64>,
    /// `--oom-kill-disable`.
    pub oom_kill_disable: bool,
    /// `--cpus`.
    pub nano_cpus: i64,
    /// `-c`.
    pub cpu_shares: i64,
    /// `--cpu-period` and `--cpu-quota`, in microseconds.
    pub cpu_period: i64,
    pub cpu_quota: i64,
    /// `--cpuset-cpus` and `--cpuset-mems`.
    pub cpuset_cpus: String,
    pub cpuset_mems: String,
    /// `--pids-limit`.
    pub pids_limit: i64,
    /// `--blkio-weight`: 10 to 1000, 0 for none.
    pub blkio_weight: u16,
    /// `--cpu-rt-period` and `--cpu-rt-runtime`, in microseconds: its guest's real-time
    /// budget (D155).
    pub cpu_realtime_period: i64,
    pub cpu_realtime_runtime: i64,
    /// `--cpu-count`: its microVM's vCPUs, 0 for as many as its other limits need (D155).
    pub cpu_count: i64,
    /// `--cpu-percent`: the most of its host's CPU time its microVM may use, 1 to 100, 0
    /// for no such cap (D155).
    pub cpu_percent: i64,
    /// `--io-maxbandwidth` and `--io-maxiops`: the most bytes and requests a second of its
    /// microVM's I/O to the host's storage, its volumes' and disks', which its VMM keeps
    /// (D178); 0 for no limit.
    pub io_maximum_bandwidth: u64,
    pub io_maximum_iops: u64,
}

impl Resources {
    /// Each given value as `name=value`, for a request's extension section.
    fn encode(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut int = |name: &str, v: i64| {
            if v != 0 {
                out.push(format!("{name}={v}"));
            }
        };
        int("memory", self.memory);
        int("memory-reservation", self.memory_reservation);
        int("memory-swap", self.memory_swap);
        int("cpus", self.nano_cpus);
        int("cpu-shares", self.cpu_shares);
        int("cpu-period", self.cpu_period);
        int("cpu-quota", self.cpu_quota);
        int("pids-limit", self.pids_limit);
        int("blkio-weight", i64::from(self.blkio_weight));
        int("cpu-rt-period", self.cpu_realtime_period);
        int("cpu-rt-runtime", self.cpu_realtime_runtime);
        int("cpu-count", self.cpu_count);
        int("cpu-percent", self.cpu_percent);
        for (name, v) in [
            ("io-maxbandwidth", self.io_maximum_bandwidth),
            ("io-maxiops", self.io_maximum_iops),
        ] {
            if v != 0 {
                out.push(format!("{name}={v}"));
            }
        }
        if let Some(s) = self.memory_swappiness {
            out.push(format!("memory-swappiness={s}"));
        }
        if self.oom_kill_disable {
            out.push("oom-kill-disable=true".into());
        }
        for (name, v) in [
            ("cpuset-cpus", &self.cpuset_cpus),
            ("cpuset-mems", &self.cpuset_mems),
        ] {
            if !v.is_empty() {
                out.push(format!("{name}={v}"));
            }
        }
        out
    }

    /// What [`encode`](Resources::encode) wrote, or `None` for a value that does not
    /// read; a name it does not know is a later build's, and left.
    fn decode(values: &[String]) -> Option<Resources> {
        let mut r = Resources::default();
        for v in values {
            let (name, value) = v.split_once('=')?;
            let int = || value.parse::<i64>().ok();
            match name {
                "memory" => r.memory = int()?,
                "memory-reservation" => r.memory_reservation = int()?,
                "memory-swap" => r.memory_swap = int()?,
                "cpus" => r.nano_cpus = int()?,
                "cpu-shares" => r.cpu_shares = int()?,
                "cpu-period" => r.cpu_period = int()?,
                "cpu-quota" => r.cpu_quota = int()?,
                "pids-limit" => r.pids_limit = int()?,
                "blkio-weight" => r.blkio_weight = value.parse().ok()?,
                "cpu-rt-period" => r.cpu_realtime_period = int()?,
                "cpu-rt-runtime" => r.cpu_realtime_runtime = int()?,
                "cpu-count" => r.cpu_count = int()?,
                "cpu-percent" => r.cpu_percent = int()?,
                "io-maxbandwidth" => r.io_maximum_bandwidth = value.parse().ok()?,
                "io-maxiops" => r.io_maximum_iops = value.parse().ok()?,
                "memory-swappiness" => r.memory_swappiness = Some(int()?),
                "oom-kill-disable" => r.oom_kill_disable = value == "true",
                "cpuset-cpus" => r.cpuset_cpus = value.to_string(),
                "cpuset-mems" => r.cpuset_mems = value.to_string(),
                _ => {}
            }
        }
        Some(r)
    }
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

/// A request's text as an extension's values: none where it is empty.
fn one(text: &str) -> Vec<String> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![text.to_string()]
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
        put_list(&mut w, &self.registry_env);
        put_opt(&mut w, self.again.as_deref());
        w.push(u8::from(self.create));
        w.push(u8::from(self.restart));
        put_identity(&mut w, &self.daemon);
        // Then what later builds added, each by name, so that a request kept by an
        // earlier one (a container's, D37) still reads: what it lacks is empty.
        let more: Vec<(&str, Vec<String>)> = [
            ("labels", self.labels.clone()),
            ("expose", self.expose.clone()),
            ("add-hosts", self.add_hosts.clone()),
            ("dns", self.dns.clone()),
            ("dns-search", self.dns_search.clone()),
            ("dns-options", self.dns_options.clone()),
            (
                "domainname",
                if self.domainname.is_empty() {
                    Vec::new()
                } else {
                    vec![self.domainname.clone()]
                },
            ),
            (
                "cidfile",
                if self.cidfile.is_empty() {
                    Vec::new()
                } else {
                    vec![self.cidfile.clone()]
                },
            ),
            (
                "quiet",
                if self.quiet {
                    vec![String::new()]
                } else {
                    Vec::new()
                },
            ),
            (
                "platform",
                if self.platform.is_empty() {
                    Vec::new()
                } else {
                    vec![self.platform.clone()]
                },
            ),
            ("resources", self.resources.encode()),
            (
                "read-only",
                if self.read_only {
                    vec![String::new()]
                } else {
                    Vec::new()
                },
            ),
            ("tmpfs", self.tmpfs.clone()),
            (
                "shm-size",
                if self.shm_size == 0 {
                    Vec::new()
                } else {
                    vec![self.shm_size.to_string()]
                },
            ),
            ("ulimits", self.ulimits.clone()),
            ("sysctls", self.sysctls.clone()),
            ("cap-add", self.cap_add.clone()),
            ("cap-drop", self.cap_drop.clone()),
            ("group-add", self.group_add.clone()),
            (
                "oom-score-adj",
                if self.oom_score_adj == 0 {
                    Vec::new()
                } else {
                    vec![self.oom_score_adj.to_string()]
                },
            ),
            (
                "privileged",
                if self.privileged {
                    vec![String::new()]
                } else {
                    Vec::new()
                },
            ),
            ("security-opt", self.security_opt.clone()),
            (
                "system-paths",
                if self.system_paths {
                    vec![String::new()]
                } else {
                    Vec::new()
                },
            ),
            ("binds", self.binds.clone()),
            ("volumes", self.volumes.clone()),
            ("mounts", self.mounts.clone()),
            ("volumes-from", self.volumes_from.clone()),
            (
                "volume-driver",
                if self.volume_driver.is_empty() {
                    Vec::new()
                } else {
                    vec![self.volume_driver.clone()]
                },
            ),
            (
                "restart-policy",
                if self.restart_policy == (String::new(), 0) {
                    Vec::new()
                } else {
                    vec![self.restart_policy.0.clone(), self.restart_policy.1.to_string()]
                },
            ),
            ("devices", self.devices.clone()),
            ("device-cgroup-rules", self.device_cgroup_rules.clone()),
            ("cdi-devices", self.cdi_devices.clone()),
            ("gpus", self.gpus.clone()),
            ("blkio-weight-device", self.blkio_weight_device.clone()),
            ("device-read-bps", self.device_read_bps.clone()),
            ("device-write-bps", self.device_write_bps.clone()),
            ("device-read-iops", self.device_read_iops.clone()),
            ("device-write-iops", self.device_write_iops.clone()),
            (
                "init-process",
                self.init_process.map(|b| vec![b.to_string()]).unwrap_or_default(),
            ),
            (
                "pid",
                if self.pid.is_empty() {
                    Vec::new()
                } else {
                    vec![self.pid.clone()]
                },
            ),
            ("annotations", self.annotations.clone()),
            (
                "isolation",
                if self.isolation.is_empty() {
                    Vec::new()
                } else {
                    vec![self.isolation.clone()]
                },
            ),
            (
                "runtime",
                if self.runtime.is_empty() {
                    Vec::new()
                } else {
                    vec![self.runtime.clone()]
                },
            ),
            ("ipc", one(&self.ipc)),
            ("uts", one(&self.uts)),
            ("userns", one(&self.userns)),
            ("cgroupns", one(&self.cgroupns)),
            (
                "umask",
                self.umask.map(|u| vec![u.to_string()]).unwrap_or_default(),
            ),
            ("storage-opt", self.storage_opt.clone()),
            ("attach", self.attach.clone()),
            ("log-driver", one(&self.log_driver)),
            ("log-opts", self.log_opts.clone()),
            ("cloud-env", self.cloud_env.clone()),
            ("resume", self.resume.iter().cloned().collect()),
            (
                "silent",
                if self.silent {
                    vec!["true".to_string()]
                } else {
                    Vec::new()
                },
            ),
            ("streams", self.streams.map(|s| s.values()).unwrap_or_default()),
            (
                "use-api-socket",
                if self.use_api_socket {
                    vec![String::new()]
                } else {
                    Vec::new()
                },
            ),
            (
                "engine-made",
                self.engine_made
                    .as_ref()
                    .map(|(of, name)| vec![of.clone(), name.clone()])
                    .unwrap_or_default(),
            ),
            ("own-binds", self.own_binds.clone()),
            (
                "engine-net",
                if self.engine_net.is_empty() {
                    Vec::new()
                } else {
                    vec![self.engine_net.clone()]
                },
            ),
            ("engine-nets", self.engine_nets.clone()),
            ("cgroup-parent", one(&self.cgroup_parent)),
            ("terms", self.terms.clone()),
            ("device-requests", {
                let mut values = Vec::new();
                for r in &self.device_requests {
                    r.put(&mut values);
                }
                values
            }),
        ]
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect();
        w.extend_from_slice(&u32::try_from(more.len()).unwrap_or(0).to_be_bytes());
        for (name, values) in &more {
            put_str(&mut w, name);
            put_list(&mut w, values);
        }
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
            registry_env: r.list()?,
            again: r.opt()?,
            create: r.flag()?,
            restart: r.flag()?,
            daemon: r.identity()?,
            ..Run::default()
        };
        let mut run = run;
        if !r.0.is_empty() {
            let n = r.u32()? as usize;
            // Each takes at least 8 bytes: its name's length and its list's.
            if n > r.0.len() / 8 {
                return None;
            }
            for _ in 0..n {
                let name = r.str()?;
                let values = r.list()?;
                match name.as_str() {
                    "labels" => run.labels = values,
                    "expose" => run.expose = values,
                    "add-hosts" => run.add_hosts = values,
                    "dns" => run.dns = values,
                    "dns-search" => run.dns_search = values,
                    "dns-options" => run.dns_options = values,
                    "domainname" => run.domainname = values.into_iter().next().unwrap_or_default(),
                    "cidfile" => run.cidfile = values.into_iter().next().unwrap_or_default(),
                    "quiet" => run.quiet = true,
                    "platform" => run.platform = values.into_iter().next().unwrap_or_default(),
                    "resources" => run.resources = Resources::decode(&values)?,
                    "read-only" => run.read_only = true,
                    "tmpfs" => run.tmpfs = values,
                    "shm-size" => run.shm_size = values.first()?.parse().ok()?,
                    "ulimits" => run.ulimits = values,
                    "sysctls" => run.sysctls = values,
                    "cap-add" => run.cap_add = values,
                    "cap-drop" => run.cap_drop = values,
                    "group-add" => run.group_add = values,
                    "oom-score-adj" => run.oom_score_adj = values.first()?.parse().ok()?,
                    "privileged" => run.privileged = true,
                    "security-opt" => run.security_opt = values,
                    "system-paths" => run.system_paths = true,
                    "binds" => run.binds = values,
                    "volumes" => run.volumes = values,
                    "mounts" => run.mounts = values,
                    "volumes-from" => run.volumes_from = values,
                    "volume-driver" => run.volume_driver = values.into_iter().next().unwrap_or_default(),
                    "restart-policy" => {
                        let (name, max) = (values.first()?, values.get(1)?);
                        run.restart_policy = (name.clone(), max.parse().ok()?);
                    }
                    "devices" => run.devices = values,
                    "device-cgroup-rules" => run.device_cgroup_rules = values,
                    "cdi-devices" => run.cdi_devices = values,
                    "gpus" => run.gpus = values,
                    "blkio-weight-device" => run.blkio_weight_device = values,
                    "device-read-bps" => run.device_read_bps = values,
                    "device-write-bps" => run.device_write_bps = values,
                    "device-read-iops" => run.device_read_iops = values,
                    "device-write-iops" => run.device_write_iops = values,
                    "init-process" => run.init_process = Some(values.first()?.parse().ok()?),
                    "pid" => run.pid = values.into_iter().next().unwrap_or_default(),
                    "annotations" => run.annotations = values,
                    "isolation" => run.isolation = values.into_iter().next().unwrap_or_default(),
                    "runtime" => run.runtime = values.into_iter().next().unwrap_or_default(),
                    "ipc" => run.ipc = values.into_iter().next().unwrap_or_default(),
                    "uts" => run.uts = values.into_iter().next().unwrap_or_default(),
                    "userns" => run.userns = values.into_iter().next().unwrap_or_default(),
                    "cgroupns" => run.cgroupns = values.into_iter().next().unwrap_or_default(),
                    "umask" => run.umask = Some(values.first()?.parse().ok()?),
                    "storage-opt" => run.storage_opt = values,
                    "attach" => run.attach = values,
                    "log-driver" => run.log_driver = values.into_iter().next().unwrap_or_default(),
                    "log-opts" => run.log_opts = values,
                    "cloud-env" => run.cloud_env = values,
                    "resume" => run.resume = values.into_iter().next(),
                    "silent" => run.silent = values.first().is_some_and(|v| v == "true"),
                    "streams" => run.streams = Some(Streams::of(&values)?),
                    "use-api-socket" => run.use_api_socket = true,
                    "engine-made" => {
                        let mut v = values.into_iter();
                        run.engine_made = v.next().zip(v.next());
                    }
                    "own-binds" => run.own_binds = values,
                    "engine-net" => run.engine_net = values.into_iter().next().unwrap_or_default(),
                    "engine-nets" => run.engine_nets = values,
                    "cgroup-parent" => run.cgroup_parent = values.into_iter().next().unwrap_or_default(),
                    "terms" => run.terms = values,
                    "device-requests" => {
                        let mut rest = values.iter();
                        while rest.len() > 0 {
                            run.device_requests.push(DeviceRequest::take(&mut rest)?);
                        }
                    }
                    // One a later build added: not this one's to read.
                    _ => {}
                }
            }
        }
        r.0.is_empty().then_some(run)
    }
}

/// What a registry is reached by (shards_registry's credentials, certs and proxy): the
/// Docker CLI's config, credential helpers on `PATH`, the homes of `certs.d`, and the
/// proxies Go's net/http takes from the environment; and the local engine a made microVM
/// is published to, as the Docker CLI finds it (`DOCKER_HOST`, `DOCKER_CONTEXT`), or none
/// (`SHARDS_LOCAL_STORE=none`).
pub const REGISTRY_ENV: [&str; 18] = [
    "SHARDS_AUTH_CONFIG",
    "DOCKER_AUTH_CONFIG",
    "DOCKER_CONFIG",
    "HOME",
    "USERPROFILE",
    "PATH",
    "PROGRAMDATA",
    "XDG_CONFIG_HOME",
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "REQUEST_METHOD",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "SHARDS_LOCAL_STORE",
];

/// This process's [`REGISTRY_ENV`], as `NAME=VALUE`, those it has.
pub fn registry_env() -> Vec<String> {
    REGISTRY_ENV
        .iter()
        .filter_map(|name| {
            let value = std::env::var_os(name)?;
            Some(format!("{name}={}", value.to_str()?))
        })
        .collect()
}

/// `name`'s value in `env`, a list [`registry_env`] made.
pub fn env_value(env: &[String], name: &str) -> Option<String> {
    env.iter()
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('=').map(String::from))
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
/// A `kind::RUN` flag: a stopped container's writable layer comes with the run, read
/// from a descriptor after the log's, to put back before the command runs (D37).
pub const RUN_LAYER_IN: u8 = 32;
/// A `kind::RUN` flag: the container stays once it stops, and its writable layer goes to
/// a descriptor after those, told by `kind::LAYER_SAVED` once whole.
pub const RUN_LAYER_OUT: u8 = 64;
/// A `kind::RUN` flag: the run's swap comes with it (D159), a sparse file of what its
/// limits allow, after the layers' descriptors, for its microVM's join disk to take as a
/// range the guest writes and swaps to.
pub const RUN_SWAP: u8 = 128;

/// A `kind::RUN` request's head, before its command's encoded spec, as the daemon writes
/// it and a warm VM's run and a joiner's (D119) read it: its flags, its log's retention
/// (each segment's most bytes, how many segments it keeps), its log's first segment, the
/// most memory its guest may plug in bytes (D148), its log's queue (D162): none where
/// the run's output is kept as it comes, else the most bytes queued for its log; its I/O
/// limits (D178); and the CPUs its microVM is held to (D191).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunHead {
    pub flags: u8,
    pub log_size: u64,
    pub log_files: u64,
    pub first: u64,
    pub plug: u64,
    pub queue: Option<u64>,
    /// Its microVM's I/O limits (D178): the most bytes and requests a second of its I/O to
    /// the host's storage, 0 for none.
    pub io_bytes: u64,
    pub io_requests: u64,
    /// The CPUs whose time its microVM's vCPUs may use of the host's together (D191), 0
    /// for every one.
    pub cpus: u64,
}

/// The big-endian u64 `rest` begins with, `rest` then past it.
fn take_u64(rest: &mut &[u8]) -> Option<u64> {
    let (n, after) = rest.split_first_chunk::<8>()?;
    *rest = after;
    Some(u64::from_be_bytes(*n))
}

impl RunHead {
    /// Its length: the flags, four big-endian u64s, the queue's mode (0 blocking, 1
    /// non-blocking), the queue's, the I/O limits' two, then the CPUs held to.
    pub const LEN: usize = 1 + 4 * 8 + 1 + 8 + 2 * 8 + 8;

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(self.flags);
        for n in [self.log_size, self.log_files, self.first, self.plug] {
            out.extend(n.to_be_bytes());
        }
        out.push(u8::from(self.queue.is_some()));
        out.extend(self.queue.unwrap_or(0).to_be_bytes());
        out.extend(self.io_bytes.to_be_bytes());
        out.extend(self.io_requests.to_be_bytes());
        out.extend(self.cpus.to_be_bytes());
    }

    /// The head `payload` begins with, and what follows it; why not, where it holds none.
    pub fn decode(payload: &[u8]) -> Result<(RunHead, &[u8]), String> {
        const SHORT: &str = "a request shorter than its head";
        let (&flags, mut rest) = payload.split_first().ok_or(SHORT)?;
        let log_size = take_u64(&mut rest).ok_or(SHORT)?;
        let log_files = take_u64(&mut rest).ok_or(SHORT)?;
        let first = take_u64(&mut rest).ok_or(SHORT)?;
        let plug = take_u64(&mut rest).ok_or(SHORT)?;
        let (&mode, mut rest) = rest.split_first().ok_or(SHORT)?;
        let queued = take_u64(&mut rest).ok_or(SHORT)?;
        let queue = match mode {
            0 => None,
            1 => Some(queued),
            m => return Err(format!("a request whose log's mode is {m}")),
        };
        let io_bytes = take_u64(&mut rest).ok_or(SHORT)?;
        let io_requests = take_u64(&mut rest).ok_or(SHORT)?;
        let cpus = take_u64(&mut rest).ok_or(SHORT)?;
        let head = RunHead {
            flags,
            log_size,
            log_files,
            first,
            plug,
            queue,
            io_bytes,
            io_requests,
            cpus,
        };
        Ok((head, rest))
    }
}

/// The largest payload a message may carry.
pub const MAX_PAYLOAD: usize = 1 << 20;

/// `kind::WORKING_SET`'s flag on the last part.
pub const WORKING_SET_LAST: u8 = 1;

/// Passes `each`, in turn, the `kind::WORKING_SET` messages that carry the working set
/// `set` recorded from generation `name`, each within [`MAX_PAYLOAD`] and built in the
/// buffer of the last, so that a set is never copied whole; none for a name longer than
/// 255 bytes, which no generation's is. Stops at `each`'s first error, which it returns.
pub fn working_set_parts<E>(
    name: &str,
    set: &[u8],
    mut each: impl FnMut(&[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let Ok(len) = u8::try_from(name.len()) else {
        return Ok(());
    };
    let room = MAX_PAYLOAD - 2 - name.len();
    let mut part = Vec::with_capacity(2 + name.len() + set.len().min(room));
    let mut chunks = set.chunks(room).peekable();
    // An empty set is one empty part, the last.
    let mut chunk = chunks.next().unwrap_or_default();
    loop {
        let last = chunks.peek().is_none();
        part.clear();
        part.push(if last { WORKING_SET_LAST } else { 0 });
        part.push(len);
        part.extend_from_slice(name.as_bytes());
        part.extend_from_slice(chunk);
        each(&part)?;
        match chunks.next() {
            Some(next) => chunk = next,
            None => return Ok(()),
        }
    }
}

/// One `kind::WORKING_SET` message's flags, generation name and part.
pub fn working_set_part(payload: &[u8]) -> Option<(u8, &str, &[u8])> {
    let (&flags, rest) = payload.split_first()?;
    let (&len, rest) = rest.split_first()?;
    let (name, part) = rest.split_at_checked(usize::from(len))?;
    Some((flags, std::str::from_utf8(name).ok()?, part))
}
/// The most descriptors a message may carry: a run's ([`kind::RUN`]) at most, the client's
/// connection and stdio, the log's two files, the writable layer in and out, and the
/// share process's connection.
pub const MAX_FDS: usize = 9;

pub mod slots;
#[cfg(unix)]
mod spawner;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use spawner::{Group, end_group, group_ended, serve_spawner, spawn_in_group, start_group, start_spawner};
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
mod pipe;
#[cfg(windows)]
pub use pipe::{
    Arriving, Event, ExitWatch, Message, Pipe, Poller, Stream, await_any, await_streams, end, readable_raw,
    recv, recv_by, send,
};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// What carries shards' own messages between its processes: a Unix socket on Unix; on
/// Windows a pipe's end ([`pipe::Stream`]), which Unix's code takes as it takes the socket.
#[cfg(unix)]
pub type Stream = std::os::unix::net::UnixStream;

/// Descriptors, as shards' processes hand them on: a file descriptor on Unix, a handle on
/// Windows, named by Unix's names, so that code that hands them on is written once.
pub mod fd {
    #[cfg(unix)]
    pub use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
    #[cfg(windows)]
    pub use std::os::windows::io::{
        BorrowedHandle as BorrowedFd, FromRawHandle as FromRawFd, IntoRawHandle as IntoRawFd,
        OwnedHandle as OwnedFd, RawHandle as RawFd,
    };

    /// A handle's borrow, as Unix's `AsFd` gives a descriptor's.
    #[cfg(windows)]
    pub trait AsFd {
        fn as_fd(&self) -> BorrowedFd<'_>;
    }

    #[cfg(windows)]
    impl<T: std::os::windows::io::AsHandle + ?Sized> AsFd for T {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.as_handle()
        }
    }

    /// A handle's value, as Unix's `AsRawFd` gives a descriptor's.
    #[cfg(windows)]
    pub trait AsRawFd {
        fn as_raw_fd(&self) -> RawFd;
    }

    #[cfg(windows)]
    impl<T: std::os::windows::io::AsRawHandle + ?Sized> AsRawFd for T {
        fn as_raw_fd(&self) -> RawFd {
            self.as_raw_handle()
        }
    }
}

/// The signals shards names a process's end by: this host's own on Unix; on Windows, which
/// sends a process none but its end, Linux's numbers, which a child it ends takes 128 and
/// one of as its status (`Child::kill`), as a Unix child a signal ended does.
pub mod signal {
    #[cfg(unix)]
    pub use libc::{SIGCONT, SIGHUP, SIGINT, SIGKILL, SIGQUIT, SIGSTOP, SIGTERM};
    #[cfg(windows)]
    pub const SIGHUP: i32 = 1;
    #[cfg(windows)]
    pub const SIGINT: i32 = 2;
    #[cfg(windows)]
    pub const SIGQUIT: i32 = 3;
    #[cfg(windows)]
    pub const SIGKILL: i32 = 9;
    #[cfg(windows)]
    pub const SIGTERM: i32 = 15;
    #[cfg(windows)]
    pub const SIGCONT: i32 = 18;
    #[cfg(windows)]
    pub const SIGSTOP: i32 = 19;
}

/// `stream`'s end, to hand on in a message ([`send`]): borrowed on Unix; on Windows a handle
/// of its own, as a [`Stream`] lends none (its end closes as it is ended, whatever borrows
/// it), which the message duplicates for its receiver.
#[cfg(unix)]
pub fn handed(stream: &Stream) -> std::io::Result<std::os::fd::BorrowedFd<'_>> {
    use std::os::fd::AsFd as _;
    Ok(stream.as_fd())
}

/// `stream`'s end, to hand on in a message ([`send`]): see Unix's.
#[cfg(windows)]
pub fn handed(stream: &Stream) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    stream.share()
}

/// What one thread sets, once, for others to see, and a wait can wake on: the daemon's end
/// of its clients, which a slot's reader of a request waits on beside the request (D185).
/// A pipe written a byte never read, on Unix; an event, on Windows.
#[derive(Debug)]
pub struct Latch {
    #[cfg(unix)]
    pipe: (std::io::PipeReader, std::io::PipeWriter),
    #[cfg(windows)]
    event: Event,
}

impl Latch {
    pub fn new() -> std::io::Result<Latch> {
        Ok(Latch {
            #[cfg(unix)]
            pipe: std::io::pipe()?,
            #[cfg(windows)]
            event: Event::new()?,
        })
    }

    /// Another handle to it: set through either, it is set for both.
    pub fn try_clone(&self) -> std::io::Result<Latch> {
        Ok(Latch {
            #[cfg(unix)]
            pipe: (self.pipe.0.try_clone()?, self.pipe.1.try_clone()?),
            #[cfg(windows)]
            event: self.event.try_clone()?,
        })
    }

    /// Sets it, and it stays set.
    pub fn set(&self) {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            // A full pipe is set already.
            let _ = (&self.pipe.1).write_all(&[0]);
        }
        #[cfg(windows)]
        self.event.set();
    }

    /// What a wait wakes on once it is set: readable from then on.
    #[cfg(unix)]
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        self.pipe.0.as_fd()
    }

    /// What a wait wakes on once it is set.
    #[cfg(windows)]
    pub fn event(&self) -> &Event {
        &self.event
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    #[test]
    fn progress_reads_as_it_was_written() {
        let all = [
            Progress::Pulling {
                reference: "docker.io/library/alpine:3.22".into(),
                repository: "library/alpine".into(),
            },
            Progress::Layers(vec![("sha256:aa".into(), 3), ("sha256:bb".into(), 0)]),
            Progress::Layers(Vec::new()),
            Progress::Have("sha256:aa".into()),
            Progress::Bytes("sha256:aa".into(), u64::MAX),
            Progress::Verified("sha256:bb".into()),
            Progress::Building,
            Progress::Unpacking(7),
            Progress::Pushing {
                reference: "localhost:5055/a:1".into(),
                repository: "a".into(),
            },
            Progress::Mounted("sha256:aa".into(), "library/alpine".into()),
            Progress::Facts(vec![
                ("id".into(), "sha256:dd".into()),
                ("cmd".into(), "sh -c a=b".into()),
                ("empty".into(), String::new()),
            ]),
            Progress::Facts(Vec::new()),
            Progress::Done {
                digest: "sha256:cc".into(),
                unchanged: true,
                bootable: false,
            },
            Progress::Waiting(
                "Waiting for docker-credential-desktop to answer for https://index.docker.io/v1/ (credsStore in /h/.docker/config.json)".into(),
            ),
        ];
        for p in all {
            assert_eq!(Progress::decode(&p.encode()), Some(p));
        }
        // A value's tabs and line ends are spaces: a fact is one field.
        let said = Progress::Facts(vec![("cmd".into(), "a\tb\nc".into())]);
        assert_eq!(
            Progress::decode(&said.encode()),
            Some(Progress::Facts(vec![("cmd".into(), "a b c".into())]))
        );
        let said = Progress::Waiting("in\ta\nb".into());
        assert_eq!(
            Progress::decode(&said.encode()),
            Some(Progress::Waiting("in a b".into()))
        );
        // What a newer daemon may say, and what no daemon says, are left alone.
        for unknown in [&b"glimmer\tx"[..], b"bytes\tsha256:aa\tmany", b"\xff", b""] {
            assert_eq!(Progress::decode(unknown), None);
        }
    }

    #[test]
    fn a_sheet_reads_as_it_was_written() {
        let mut sheet = Sheet::new("images");
        sheet.record(&[("name", "node".into()), ("tag", "22=slim".into())]);
        sheet.record(&[]);
        sheet.record(&[("odd", "a\x1eb\x1fc".into())]);
        let back = Sheet::decode(&sheet.encode()).unwrap();
        assert_eq!(back.name, "images");
        assert_eq!(back.get(0, "tag"), Some("22=slim"));
        assert_eq!(back.records[1], Vec::new());
        assert_eq!(back.get(2, "odd"), Some("a b c"));
        assert_eq!(Sheet::decode(b"x"), Some(Sheet::new("x")));
    }

    #[test]
    fn every_message_kind_has_its_own_number() {
        let source = include_str!("lib.rs");
        let kinds = source
            .split("pub mod kind {")
            .nth(1)
            .and_then(|rest| rest.split("\n}").next())
            .unwrap();
        let mut seen = std::collections::HashMap::new();
        for line in kinds.lines() {
            if let Some((name, n)) = line
                .trim()
                .strip_prefix("pub const ")
                .and_then(|l| l.split_once(": u8 = "))
            {
                let n: u8 = n.trim_end_matches(';').parse().unwrap();
                assert_eq!(seen.insert(n, name.to_string()), None, "{name} shares {n}");
            }
        }
        assert!(seen.len() > 20, "{} kinds", seen.len());
    }

    use super::*;

    /// A run's head reads back as written, with what follows it; one cut short, or of a
    /// mode no build writes, is refused.
    #[test]
    fn a_run_head_reads_back_as_written() {
        for queue in [None, Some(0), Some(1_000_000)] {
            let head = RunHead {
                flags: RUN_LOG | RUN_DETACHED,
                log_size: 20 << 20,
                log_files: 5,
                first: 3,
                plug: 1 << 30,
                queue,
                io_bytes: 10 << 20,
                io_requests: 100,
                cpus: 2,
            };
            let mut bytes = Vec::new();
            head.encode_into(&mut bytes);
            assert_eq!(bytes.len(), RunHead::LEN);
            bytes.extend(b"spec");
            assert_eq!(RunHead::decode(&bytes), Ok((head, &b"spec"[..])));
            assert!(RunHead::decode(&bytes[..RunHead::LEN - 1]).is_err());
        }
        let mut bytes = Vec::new();
        RunHead::default().encode_into(&mut bytes);
        bytes[33] = 2;
        assert_eq!(
            RunHead::decode(&bytes),
            Err("a request whose log's mode is 2".to_string())
        );
    }

    /// A daemon is ending its runs while the process its file names lives: this one is,
    /// one that has gone is not, and neither is a file naming no single process.
    #[cfg(unix)]
    #[test]
    fn a_daemon_is_exiting_while_its_process_lives() {
        let home = shards_testdir::TempDir::new("ipc-exiting").unwrap();
        assert!(!exiting(&home), "no file");
        let gone = std::process::Command::new("true").spawn().unwrap();
        let gone_pid = gone.id();
        let _ = { gone }.wait();
        for (said, is) in [
            (std::process::id().to_string(), true),
            (gone_pid.to_string(), false),
            ("0".to_string(), false),
            ("-1".to_string(), false),
            ("daemon".to_string(), false),
        ] {
            std::fs::write(home.join(STOPPING), format!("{said}\n")).unwrap();
            assert_eq!(exiting(&home), is, "{said}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

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
            let parts = parts(name, &set);
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
        assert!(parts(&"g".repeat(256), b"x").is_empty());
        assert!(working_set_part(&[0, 9, b'a']).is_none(), "a name past its end");
        // The first refusal stops them, and is the answer.
        let mut taken = 0;
        let refused = working_set_parts(name, &[7; 3 * MAX_PAYLOAD], |_| {
            taken += 1;
            if taken == 2 { Err("refused") } else { Ok(()) }
        });
        assert_eq!((refused, taken), (Err("refused"), 2));
    }

    /// The parts of a working set, each copied out.
    fn parts(name: &str, set: &[u8]) -> Vec<Vec<u8>> {
        let mut parts = Vec::new();
        let _ = working_set_parts::<()>(name, set, |p| {
            parts.push(p.to_vec());
            Ok(())
        });
        parts
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
            stdin_terminal: true,
            privileged: true,
            terms: vec!["xterm-kitty".into()],
            id: "1f".repeat(32),
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
    fn attach_requests_round_trip() {
        let attach = Attach {
            container: "web".into(),
            stdin: true,
            daemon: Identity {
                dev: 1,
                ino: 2,
                size: 3,
                mtime_s: 4,
                mtime_ns: 5,
            },
        };
        let bytes = attach.encode();
        assert_eq!(Attach::decode(&bytes), Some(attach));
        for n in 0..bytes.len() {
            assert_eq!(Attach::decode(&bytes[..n]), None, "an attach cut at {n} decoded");
        }
    }

    /// A name's value is its own, not that of a name it begins.
    #[test]
    fn registry_env_values_are_found_by_their_whole_name() {
        let env = [
            "PATHS=/no".to_string(),
            "PATH=/bin".to_string(),
            "HOME=".to_string(),
        ];
        assert_eq!(env_value(&env, "PATH").as_deref(), Some("/bin"));
        assert_eq!(env_value(&env, "HOME").as_deref(), Some(""));
        assert_eq!(env_value(&env, "PAT"), None);
        assert_eq!(env_value(&env, "DOCKER_CONFIG"), None);
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
            registry_env: vec!["DOCKER_CONFIG=/d".into(), "PATH=/bin".into()],
            again: Some("web".into()),
            // Its newest snapshot (D176): asked for by no name, which still reads.
            resume: Some(String::new()),
            silent: true,
            create: true,
            restart: true,
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
            labels: vec!["a=1".into(), "b".into()],
            expose: vec!["80".into(), "7000-7002/udp".into()],
            add_hosts: vec!["db:10.0.0.2".into()],
            dns: vec!["1.1.1.1".into()],
            dns_search: vec!["example.com".into()],
            dns_options: vec!["ndots:2".into()],
            domainname: "example.org".into(),
            cidfile: "/tmp/cid".into(),
            quiet: true,
            platform: "linux/386".into(),
            resources: Resources {
                memory: 64 << 20,
                memory_reservation: 32 << 20,
                memory_swap: -1,
                memory_swappiness: Some(0),
                oom_kill_disable: true,
                nano_cpus: 1_500_000_000,
                cpu_shares: 512,
                cpu_period: 50_000,
                cpu_quota: 25_000,
                cpuset_cpus: "0-1".into(),
                cpuset_mems: "0".into(),
                pids_limit: -1,
                blkio_weight: 300,
                cpu_realtime_period: 1000,
                cpu_realtime_runtime: 950,
                cpu_count: 2,
                cpu_percent: 50,
                io_maximum_bandwidth: 10 << 20,
                io_maximum_iops: 100,
            },
            read_only: true,
            tmpfs: vec!["/run:size=1m".into(), "/tmp".into()],
            shm_size: 128 << 20,
            ulimits: vec!["nofile=1024:2048".into()],
            sysctls: vec!["net.core.somaxconn=1024".into()],
            cap_add: vec!["CAP_NET_ADMIN".into()],
            cap_drop: vec!["CAP_CHOWN".into()],
            group_add: vec!["audio".into()],
            oom_score_adj: -500,
            privileged: true,
            security_opt: vec![
                "no-new-privileges".into(),
                "seccomp={\"defaultAction\":\"SCMP_ACT_ALLOW\"}".into(),
            ],
            system_paths: true,
            binds: vec!["/h:/c:ro".into()],
            volumes: vec!["/anon".into()],
            mounts: vec!["type=volume,source=v,target=/v".into()],
            volumes_from: vec!["web:ro".into()],
            volume_driver: "local".into(),
            restart_policy: ("on-failure".into(), 3),
            devices: vec!["/dev/fuse:/dev/fuse:rwm".into()],
            device_cgroup_rules: vec!["c 1:3 r".into()],
            cdi_devices: vec!["vendor.com/gpu=0".into()],
            gpus: vec!["all".into(), "\"device=0\",capabilities=compute".into()],
            blkio_weight_device: vec!["/dev/pmem0:300".into()],
            device_read_bps: vec!["/dev/pmem0:1048576".into()],
            device_write_bps: vec!["/dev/loop0:1".into()],
            device_read_iops: vec!["/dev/loop0:2".into()],
            device_write_iops: vec!["/dev/loop0:3".into()],
            annotations: vec!["a=1".into(), "b".into()],
            isolation: "default".into(),
            runtime: "runc".into(),
            ipc: "shareable".into(),
            uts: "host".into(),
            userns: "host".into(),
            cgroupns: "private".into(),
            umask: Some(0o22),
            storage_opt: vec!["size=1G".into()],
            attach: vec!["stdout".into(), "stdin".into()],
            log_driver: "json-file".into(),
            log_opts: vec!["max-size=10m".into(), "mode".into()],
            cloud_env: vec!["AWS_PROFILE=p".into(), "HOME=/h".into()],
            streams: Some(Streams {
                stdin: false,
                stdout: true,
                stderr: false,
                stdin_once: true,
            }),
            device_requests: vec![
                DeviceRequest {
                    driver: "nvidia".into(),
                    count: -1,
                    device_ids: None,
                    capabilities: vec![vec!["gpu".into(), "utility".into()], Vec::new()],
                    options: vec![("a".into(), String::new()), ("b".into(), "c=d".into())],
                },
                DeviceRequest {
                    device_ids: Some(vec!["0".into(), String::new()]),
                    ..DeviceRequest::default()
                },
            ],
            init_process: Some(false),
            pid: "host".into(),
            use_api_socket: true,
            engine_made: Some(("prov".into(), String::new())),
            own_binds: vec!["/work\0/src\0ro\0rprivate".into()],
            engine_net: "app".into(),
            engine_nets: vec!["app\0\0web\0www".into(), "back\x0010.244.2.9".into()],
            cgroup_parent: "agents-ci.slice".into(),
            terms: vec!["xterm-ghostty".into(), "xterm-256color".into()],
        };
        let bytes = run.encode();
        let identity = run.daemon;
        assert_eq!(Run::decode(&bytes), Some(run.clone()));
        // A request an earlier build kept ends where the added fields begin: it reads,
        // without them. Cut anywhere else, nothing does.
        let earlier = Run {
            labels: Vec::new(),
            expose: Vec::new(),
            add_hosts: Vec::new(),
            dns: Vec::new(),
            dns_search: Vec::new(),
            dns_options: Vec::new(),
            domainname: String::new(),
            cidfile: String::new(),
            quiet: false,
            platform: String::new(),
            resources: Resources::default(),
            read_only: false,
            tmpfs: Vec::new(),
            shm_size: 0,
            ulimits: Vec::new(),
            sysctls: Vec::new(),
            cap_add: Vec::new(),
            cap_drop: Vec::new(),
            group_add: Vec::new(),
            oom_score_adj: 0,
            privileged: false,
            security_opt: Vec::new(),
            system_paths: false,
            binds: Vec::new(),
            volumes: Vec::new(),
            mounts: Vec::new(),
            volumes_from: Vec::new(),
            volume_driver: String::new(),
            restart_policy: Default::default(),
            devices: Vec::new(),
            device_cgroup_rules: Vec::new(),
            cdi_devices: Vec::new(),
            gpus: Vec::new(),
            blkio_weight_device: Vec::new(),
            device_read_bps: Vec::new(),
            device_write_bps: Vec::new(),
            device_read_iops: Vec::new(),
            device_write_iops: Vec::new(),
            init_process: None,
            pid: String::new(),
            annotations: Vec::new(),
            isolation: String::new(),
            runtime: String::new(),
            ipc: String::new(),
            uts: String::new(),
            userns: String::new(),
            cgroupns: String::new(),
            umask: None,
            storage_opt: Vec::new(),
            attach: Vec::new(),
            log_driver: String::new(),
            log_opts: Vec::new(),
            cloud_env: Vec::new(),
            resume: None,
            silent: false,
            streams: None,
            device_requests: Vec::new(),
            use_api_socket: false,
            engine_made: None,
            own_binds: Vec::new(),
            engine_net: String::new(),
            engine_nets: Vec::new(),
            cgroup_parent: String::new(),
            terms: Vec::new(),
            ..run.clone()
        };
        let boundary = earlier.encode().len() - 4;
        assert_eq!(Run::decode(&bytes[..boundary]), Some(earlier));
        for n in (0..bytes.len()).filter(|&n| n != boundary) {
            assert_eq!(Run::decode(&bytes[..n]), None, "a request cut at {n} decoded");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(Run::decode(&longer), None);
        assert_eq!(Run::decode(&Run::default().encode()), Some(Run::default()));
        let command = Command {
            argv: vec!["ps".into(), "-a".into(), String::new()],
            registry_env: vec!["HOME=/h".into()],
            east_asian: true,
            now: -5,
            utc_offset: -18_000,
            terminal: true,
            width: 132,
            color: true,
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

/// What a command found, for a client on a colour terminal to lay out ([`kind::SHEET`]):
/// the command's name and its records, each a list of named fields. Fields are
/// separated by US (0x1f), records by RS (0x1e); either in a value is a space.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sheet {
    pub name: String,
    pub records: Vec<Vec<(String, String)>>,
}

impl Sheet {
    pub fn new(name: &str) -> Sheet {
        Sheet {
            name: name.to_string(),
            records: Vec::new(),
        }
    }

    /// Adds a record of `fields`.
    pub fn record(&mut self, fields: &[(&str, String)]) {
        self.records.push(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
        );
    }

    pub fn encode(&self) -> Vec<u8> {
        let clean = |s: &str| -> String {
            s.chars()
                .map(|c| if c == '\x1e' || c == '\x1f' { ' ' } else { c })
                .collect()
        };
        let mut out = clean(&self.name);
        for record in &self.records {
            out.push('\x1e');
            let fields: Vec<String> = record
                .iter()
                .map(|(k, v)| format!("{}={}", clean(k).replace('=', " "), clean(v)))
                .collect();
            out.push_str(&fields.join("\x1f"));
        }
        out.into_bytes()
    }

    pub fn decode(payload: &[u8]) -> Option<Sheet> {
        let text = std::str::from_utf8(payload).ok()?;
        let mut parts = text.split('\x1e');
        let name = parts.next()?.to_string();
        let mut records = Vec::new();
        for record in parts {
            let mut fields = Vec::new();
            if !record.is_empty() {
                for field in record.split('\x1f') {
                    let (k, v) = field.split_once('=')?;
                    fields.push((k.to_string(), v.to_string()));
                }
            }
            records.push(fields);
        }
        Some(Sheet { name, records })
    }

    /// The first value of `name` in record `i`.
    pub fn get(&self, i: usize, name: &str) -> Option<&str> {
        self.records
            .get(i)?
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// How a pull goes, as the daemon tells a client on a terminal ([`kind::PROGRESS`]): a
/// line of tab-separated fields, digests in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// The reference being pulled, and its repository's path.
    Pulling { reference: String, repository: String },
    /// The reference being pushed, and its repository's path: what follows is a push's.
    Pushing { reference: String, repository: String },
    /// A layer the registry had another repository of mount, by digest, and that
    /// repository.
    Mounted(String, String),
    /// The image's layers, by digest and compressed size, in order.
    Layers(Vec<(String, u64)>),
    /// A layer already here.
    Have(String),
    /// So many bytes of a layer have arrived, in all.
    Bytes(String, u64),
    /// A layer arrived whole and matches its digest.
    Verified(String),
    /// The image is being made into a microVM's root filesystem.
    Building,
    /// The layer at this index is being unpacked into it.
    Unpacking(usize),
    /// What there is to know of the image pulled, by name: its ID, platform, sizes, where
    /// it is, what it runs. Sent before [`Progress::Done`]; a client shows the names it
    /// knows.
    Facts(Vec<(String, String)>),
    /// Pulled: what the reference resolved to, whether nothing was new, and whether the
    /// image is ready to boot here (an image of another platform is only stored).
    Done {
        digest: String,
        unchanged: bool,
        bootable: bool,
    },
    /// The pull waits for its registry's credential helper: what it waits on, as a line
    /// says it (D184).
    Waiting(String),
}

impl Progress {
    pub fn encode(&self) -> Vec<u8> {
        let text = match self {
            Progress::Pulling {
                reference,
                repository,
            } => format!("pulling\t{reference}\t{repository}"),
            Progress::Pushing {
                reference,
                repository,
            } => format!("pushing\t{reference}\t{repository}"),
            Progress::Mounted(d, from) => format!("mounted\t{d}\t{from}"),
            Progress::Layers(layers) => {
                let mut out = String::from("layers");
                for (digest, size) in layers {
                    out.push_str(&format!("\t{digest}={size}"));
                }
                out
            }
            Progress::Have(d) => format!("have\t{d}"),
            Progress::Bytes(d, n) => format!("bytes\t{d}\t{n}"),
            Progress::Verified(d) => format!("verified\t{d}"),
            Progress::Building => "building".to_string(),
            Progress::Unpacking(i) => format!("unpacking\t{i}"),
            Progress::Facts(facts) => {
                let mut out = String::from("facts");
                for (name, value) in facts {
                    // A field each: a tab or a line end in a value is a space.
                    let value: String = value
                        .chars()
                        .map(|c| {
                            if c == '\t' || c == '\n' || c == '\r' {
                                ' '
                            } else {
                                c
                            }
                        })
                        .collect();
                    out.push_str(&format!("\t{name}={value}"));
                }
                out
            }
            Progress::Done {
                digest,
                unchanged,
                bootable,
            } => {
                format!(
                    "done\t{digest}\t{}\t{}",
                    u8::from(*unchanged),
                    u8::from(*bootable)
                )
            }
            // One field: a tab or a line end in it is a space.
            Progress::Waiting(notice) => format!("waiting\t{}", notice.replace(['\t', '\n', '\r'], " ")),
        };
        text.into_bytes()
    }

    /// The event a message carries, or `None` for one this client does not know: a newer
    /// daemon may say more.
    pub fn decode(payload: &[u8]) -> Option<Progress> {
        let text = std::str::from_utf8(payload).ok()?;
        let mut fields = text.split('\t');
        let word = fields.next()?;
        let mut next = || fields.next().map(str::to_string);
        Some(match word {
            "pulling" => Progress::Pulling {
                reference: next()?,
                repository: next()?,
            },
            "pushing" => Progress::Pushing {
                reference: next()?,
                repository: next()?,
            },
            "mounted" => {
                let digest = next()?;
                Progress::Mounted(digest, next()?)
            }
            "layers" => {
                let mut layers = Vec::new();
                while let Some(field) = next() {
                    let (digest, size) = field.rsplit_once('=')?;
                    layers.push((digest.to_string(), size.parse().ok()?));
                }
                Progress::Layers(layers)
            }
            "have" => Progress::Have(next()?),
            "bytes" => {
                let digest = next()?;
                Progress::Bytes(digest, next()?.parse().ok()?)
            }
            "verified" => Progress::Verified(next()?),
            "building" => Progress::Building,
            "unpacking" => Progress::Unpacking(next()?.parse().ok()?),
            "facts" => {
                let mut facts = Vec::new();
                while let Some(field) = next() {
                    let (name, value) = field.split_once('=')?;
                    facts.push((name.to_string(), value.to_string()));
                }
                Progress::Facts(facts)
            }
            "done" => Progress::Done {
                digest: next()?,
                unchanged: next()? == "1",
                bootable: next()? == "1",
            },
            "waiting" => Progress::Waiting(next()?),
            _ => return None,
        })
    }
}

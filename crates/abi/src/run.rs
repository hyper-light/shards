//! The run protocol: how shards hands a workload to shards-init and gets back its output
//! and exit status (docs/design/architecture.md D16). The guest dials the host on
//! [`PORT`] once the image is mounted, and the one connection carries everything.
//!
//! Frames have the header of Docker's attach streams (moby api/pkg/stdcopy): a kind byte,
//! three zero bytes, and the payload's length as a big-endian u32. So output reaches the
//! host framed as Docker's Engine API sends it.

use alloc::vec::Vec;

/// The host port the guest dials for its workload.
pub const PORT: u32 = 1024;
/// The host port the guest dials, once the workload runs, for [`kind::SIGNAL`] frames.
/// They travel apart from the run connection, so that stdin a workload leaves unread
/// cannot hold them up, as Docker sends them apart from its attach stream.
pub const SIGNAL_PORT: u32 = 1025;
/// The host port the guest dials once for each command run beside the workload (`docker
/// exec`), after the host asks for one with [`kind::EXEC`]. The host takes every
/// connection to it, and keeps those whose [`kind::HELLO`] names a token it sent.
pub const EXEC_PORT: u32 = 1027;
/// The bytes of an exec's token.
pub const TOKEN: usize = 16;
pub const HEADER: usize = 8;
/// The largest payload either side accepts.
pub const MAX_PAYLOAD: u32 = 1 << 20;
/// What the guest reads of a stream at once, and so the most one of its output frames
/// carries.
pub const CHUNK: usize = 64 * 1024;
/// What either side keeps waiting for the other in each direction before it stops
/// reading more, so that backpressure reaches the writer: at most what one write of the
/// guest's sends, and what the host reads at once.
pub const BUFFERED: usize = 256 * 1024;

/// Frame kinds. The first four are stdcopy's streams.
pub mod kind {
    /// Host to guest: bytes for the workload's stdin. An empty frame closes it.
    pub const STDIN: u8 = 0;
    /// Guest to host: the workload's output.
    pub const STDOUT: u8 = 1;
    pub const STDERR: u8 = 2;
    /// Guest to host: why the workload did not start, before [`EXIT`].
    pub const SYSTEM_ERR: u8 = 3;
    /// Host to guest, first and once: the [`Spec`](super::Spec).
    pub const SPEC: u8 = 16;
    /// Guest to host, last: the exit status, a big-endian u32 as `docker run` reports
    /// it: the workload's code, 128 plus a fatal signal's number, or 125 to 127 when the
    /// command never ran.
    pub const EXIT: u8 = 17;
    /// Host to guest, on the [`SIGNAL_PORT`](super::SIGNAL_PORT) connection: a signal for
    /// the workload's main process, as a big-endian u32 in Linux's numbering.
    pub const SIGNAL: u8 = 18;
    /// Guest to host: the command is executing. It comes before any of its output; a
    /// command that could not start sends [`SYSTEM_ERR`] instead.
    pub const STARTED: u8 = 19;
    /// Host to guest, on the [`SIGNAL_PORT`](super::SIGNAL_PORT) connection: the size of
    /// a terminal workload's pty, as a [`Size`](super::Size) encodes it. The kernel sends
    /// the terminal's foreground process group SIGWINCH if it changed.
    pub const RESIZE: u8 = 20;
    /// Host to guest, on the signal connection: run a command beside the workload. Its
    /// token ([`TOKEN`](super::TOKEN) bytes), its id (a big-endian u32), then its
    /// [`Spec`](super::Spec). The guest dials [`EXEC_PORT`](super::EXEC_PORT) for it,
    /// sends [`HELLO`] with the token, then [`STARTED`] and its output, or
    /// [`SYSTEM_ERR`], then [`EXIT`]; the host sends its [`STDIN`] there.
    pub const EXEC: u8 = 21;
    /// Host to guest, on the signal connection: an exec's id, then a signal for it, as
    /// [`SIGNAL`] carries one.
    pub const EXEC_SIGNAL: u8 = 22;
    /// Host to guest, on the signal connection: an exec's id, then its terminal's size,
    /// as [`RESIZE`] carries one.
    pub const EXEC_RESIZE: u8 = 23;
    /// Guest to host, first on an exec's connection: its token.
    pub const HELLO: u8 = 24;
    /// Guest to host, on the workload's connection: an exec it was asked for has no
    /// connection to tell the host on: its id (a big-endian u32), then why, in words. The
    /// host fails that exec with them, rather than wait for a connection that will not
    /// come.
    pub const EXEC_FAILED: u8 = 25;
    /// Either way, on the workload's connection: a piece of a container's writable layer,
    /// a tar archive of its changes in the OCI image layer form (`.wh.` whiteouts); an
    /// empty frame ends it. Host to guest before [`SPEC`]: a stopped container's layer, to
    /// put over the image before the command runs again. Guest to host after [`SAVE`]: the
    /// layer as the container left it.
    pub const LAYER: u8 = 26;
    /// Host to guest, after [`EXIT`]: send the container's writable layer as [`LAYER`]
    /// frames, then wait to be powered off.
    pub const SAVE: u8 = 27;
    /// Guest to host, before [`EXIT`]: the kernel killed a process of the workload's
    /// cgroup for want of memory (its `memory.events` counts an `oom_kill`), as
    /// containerd tells dockerd of an OOM.
    pub const OOM: u8 = 28;
    /// Guest to host, after [`SAVE`] and before the [`LAYER`] frames: the disk its
    /// writable layer uses (a big-endian u64), counted as containerd's snapshots count
    /// it, for inspect's SizeRw once it has stopped.
    pub const USAGE: u8 = 29;
}

/// Why an exec did not start, as its [`kind::SYSTEM_ERR`] says first: the runtime could
/// not start it (`docker exec`'s "OCI runtime exec failed"), or the daemon refuses it
/// (an unknown user: "Error response from daemon").
pub mod exec_failed {
    pub const RUNTIME: u8 = 0;
    pub const DAEMON: u8 = 1;
}

pub fn header(kind: u8, len: u32) -> [u8; HEADER] {
    let [a, b, c, d] = len.to_be_bytes();
    [kind, 0, 0, 0, a, b, c, d]
}

/// A header's kind and payload length, or `None` if it is malformed or too long.
pub fn parse_header(h: [u8; HEADER]) -> Option<(u8, u32)> {
    let [kind, z0, z1, z2, a, b, c, d] = h;
    let len = u32::from_be_bytes([a, b, c, d]);
    (z0 == 0 && z1 == 0 && z2 == 0 && len <= MAX_PAYLOAD).then_some((kind, len))
}

/// What to run, after the host has applied the image's and the command line's settings.
/// Strings are bytes, as Linux takes them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spec {
    pub argv: Vec<Vec<u8>>,
    /// `KEY=value` entries.
    pub env: Vec<Vec<u8>>,
    /// The working directory, or empty for `/`.
    pub cwd: Vec<u8>,
    /// Docker's `--user`: `user` or `user:group`, each a name or a number; empty for root.
    pub user: Vec<u8>,
    pub hostname: Vec<u8>,
    /// Docker's `--tty`: the workload's stdio is a pty of this size, whose output reaches
    /// the host as [`kind::STDOUT`] alone; `None` for pipes.
    pub tty: Option<Size>,
    /// The bytes of the run's `/etc/resolv.conf`, for a run on a network: the host's
    /// resolvers as Docker gives a container on its bridge.
    pub resolv: Option<Vec<u8>>,
    /// Docker's `-i`: the workload reads the client's stdin, through a pipe. Without it,
    /// and without a terminal, its stdin is `/dev/null`, as Docker gives a container's
    /// and an exec's (measured: Docker 29.3.1, `readlink /proc/self/fd/0`).
    pub stdin: bool,
    /// Not a command but one of shards-init's own ([`builtin`]), answered on the exec's
    /// stdout as a command's output would be, with `argv` its arguments; 0 for a command.
    pub builtin: u8,
    /// Docker's `--add-host`: `/etc/hosts` lines after its defaults, each `IP\tNAME`.
    pub hosts: Vec<Vec<u8>>,
    /// Docker's `--domainname`: the NIS domain name, and the host's full name in
    /// `/etc/hosts`.
    pub domainname: Vec<u8>,
    /// The workload's resource limits, as its cgroup's interface files take them (Linux
    /// Documentation/admin-guide/cgroup-v2.rst): each `FILE=VALUE`, written in order.
    pub cgroup: Vec<Vec<u8>>,
    /// What the workload's namespaces are given before it starts, in order, each
    /// `KIND=VALUE`: `tmpfs=DEST\0OPTIONS` (moby's merged tmpfs options), `shm=BYTES`,
    /// `ulimit=NAME=SOFT:HARD`, `sysctl=KEY=VALUE`, and `readonly` alone.
    pub setup: Vec<Vec<u8>>,
}

/// A container's capabilities by default, by number (linux/capability.h; moby
/// daemon/pkg/oci/caps/defaults.go): CHOWN, DAC_OVERRIDE, FOWNER, FSETID, KILL, SETGID,
/// SETUID, SETPCAP, NET_BIND_SERVICE, NET_RAW, SYS_CHROOT, MKNOD, AUDIT_WRITE and SETFCAP:
/// CapBnd 00000000a80425fb.
pub const CAPS: [u32; 14] = [0, 1, 3, 4, 5, 6, 7, 8, 10, 13, 18, 27, 29, 31];

/// Capabilities' names, by number, to CAP_CHECKPOINT_RESTORE (40), the last of the
/// kernel shards boots (linux/capability.h).
pub const CAP_NAMES: [&str; 41] = [
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_LINUX_IMMUTABLE",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_ADMIN",
    "CAP_NET_RAW",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_SYS_MODULE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_CHROOT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_PACCT",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_NICE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_MKNOD",
    "CAP_LEASE",
    "CAP_AUDIT_WRITE",
    "CAP_AUDIT_CONTROL",
    "CAP_SETFCAP",
    "CAP_MAC_OVERRIDE",
    "CAP_MAC_ADMIN",
    "CAP_SYSLOG",
    "CAP_WAKE_ALARM",
    "CAP_BLOCK_SUSPEND",
    "CAP_AUDIT_READ",
    "CAP_PERFMON",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
];

/// What a run of an image whose Agentfile declares agents or harnesses keeps from its own
/// command, beside them (D115): the daemon refuses each as it reads the run, and shards-init
/// again as it starts the command, in the same words.
pub mod beside {
    use alloc::format;
    use alloc::string::String;

    /// The capabilities a command may not hold beside an image's domains, by number, and
    /// what each reaches of theirs: across the PID namespace of the command's own, which
    /// hides their processes and init's, the kernel and the microVM they share.
    pub const DOMAINS: [(u32, &str); 12] = [
        (16, "loads code into the kernel they share"),
        (17, "reaches the microVM's memory-mapped devices and I/O ports"),
        (
            19,
            "traces the processes init starts beside the command before they give up init's privileges",
        ),
        (
            20,
            "records the exit of every process of the microVM, theirs among them",
        ),
        (
            21,
            "mounts their volumes and devices and lifts the masks on /proc",
        ),
        (23, "schedules the command ahead of theirs in real time"),
        (25, "sets the clock they share"),
        (30, "has the kernel audit their system calls"),
        (34, "reads what the kernel logs of their processes"),
        (37, "reads the kernel's audit of their system calls"),
        (38, "observes every process of the kernel they share"),
        (39, "loads programs into the kernel they share"),
    ];

    /// Those a command may not hold where its domains' flows past the microVM cross its
    /// network namespace, init's, on their way to eth0 (D59).
    pub const UPLINK: [(u32, &str); 2] = [
        (
            12,
            "changes the network namespace their flows past the microVM cross",
        ),
        (
            13,
            "reads and writes the packets of their flows past the microVM, which cross the command's network namespace",
        ),
    ];

    /// The first capability of `caps` (a bit for each by number) a command may not hold
    /// beside an image's domains, with or without an `uplink`, and what it reaches.
    pub fn crossing(caps: u64, uplink: bool) -> Option<(u32, &'static str)> {
        let uplinked: &[(u32, &str)] = if uplink { &UPLINK } else { &[] };
        DOMAINS
            .iter()
            .chain(uplinked)
            .copied()
            .filter(|&(c, _)| c < 64 && caps & (1 << c) != 0)
            .min_by_key(|&(c, _)| c)
    }

    /// Whether `id`, a uid or gid, is one an image declaring `domains` agents and
    /// harnesses runs them or their in-VM servers as (D59, D60).
    pub fn taken(id: u32, domains: u32) -> bool {
        [crate::DOMAIN_FIRST_ID, crate::SERVER_FIRST_ID]
            .iter()
            .any(|&first| id.checked_sub(first).is_some_and(|n| n < domains))
    }

    const WHY: &str = "the image's Agentfile runs agents and harnesses beside the command";

    /// Refusing capability `name`, which reaches the domains as `what` says.
    pub fn capability(name: &str, what: &str) -> String {
        format!("cannot give the command {name}: {WHY}, and {name} {what}")
    }

    /// Refusing a privileged command, or exec (`what`).
    pub fn privileged(what: &str) -> String {
        format!(
            "cannot {what} privileged: {WHY}, and a privileged command has every capability and device, which reach them"
        )
    }

    /// Refusing `--security-opt systempaths=unconfined`.
    pub fn unmasked() -> String {
        format!(
            "cannot leave /proc unmasked: {WHY}, and /proc/sysrq-trigger signals their processes and /proc/sched_debug lists them"
        )
    }

    /// Saying that the command runs without capability `name`, which Docker gives by
    /// default, and which reaches the domains as `what` says.
    pub fn withheld(name: &str, what: &str) -> String {
        format!("{name} withheld from the command: {WHY}, and {name} {what}")
    }

    /// Refusing `--pid host`: the microVM's PID namespace, the parent of theirs.
    pub fn pid_host() -> String {
        format!(
            "cannot run the command in the microVM's PID namespace (--pid host): {WHY}, and that namespace holds their processes and init's, which the command would see and signal"
        )
    }

    /// The link-local /30 init's network namespace keeps for the link to its domains'
    /// switch, past which eth0 is (D59): its first address init's, its second the switch's.
    pub const UPLINK_NET: ([u8; 4], u8) = ([169, 254, 77, 0], 30);

    /// Whether IPv4 address `ip` lies in [`UPLINK_NET`].
    pub fn in_uplink(ip: [u8; 4]) -> bool {
        let (net, bits) = UPLINK_NET;
        let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
        u32::from_be_bytes(ip) & mask == u32::from_be_bytes(net) & mask
    }

    /// Refusing link-local address `ip` on eth0 where the domains' flows cross the
    /// command's namespace by [`UPLINK_NET`] (PM M175).
    pub fn link_local(ip: &str) -> String {
        format!(
            "cannot give eth0 link-local address {ip}: the image's agents reach past the microVM by 169.254.77.0/30, a link of init's that the address would lie in"
        )
    }

    /// Refusing sysctl `key` where the domains' flows cross the command's namespace.
    pub fn sysctl(key: &str) -> String {
        format!(
            "cannot set sysctl {key}: the image's agents reach past the microVM through the command's network namespace, which net.* sysctls change"
        )
    }

    /// Refusing `which` ("uid", "gid") `id`, one of an image's `domains`'s.
    pub fn id(which: &str, id: u32, domains: u32) -> String {
        let last = |first: u32| first.saturating_add(domains.saturating_sub(1));
        format!(
            "cannot run as {which} {id}: the image's Agentfile runs its agents and harnesses as {} to {}, and their in-VM servers as {} to {}, which no other process of the microVM takes",
            crate::DOMAIN_FIRST_ID,
            last(crate::DOMAIN_FIRST_ID),
            crate::SERVER_FIRST_ID,
            last(crate::SERVER_FIRST_ID)
        )
    }
}

/// What shards-init does itself for an exec ([`Spec::builtin`]).
pub mod builtin {
    /// The guest's processes as `/proc` has them, for `shards top` (init procs.rs).
    pub const PROCESSES: u8 = 1;
    /// What the container changed of its image's files, for `shards diff` (init
    /// changes.rs).
    pub const CHANGES: u8 = 2;
    /// The container's files as a tar archive, for `shards export` (init run.rs).
    pub const EXPORT: u8 = 3;
    /// The container's writable layer as an OCI layer, as it is now, for `shards
    /// commit` (init layer.rs): with `pause` its only argument, its processes stopped
    /// meanwhile.
    pub const LAYER: u8 = 4;
    /// `shards cp`'s (init copy.rs), each its path first: a path's stat as a JSON line;
    /// a tar archive of it; and the tar archive on stdin unpacked into it, then the
    /// container user's to own it all or empty, then `1` to let a directory and a file
    /// replace each other.
    pub const STAT: u8 = 5;
    pub const ARCHIVE: u8 = 6;
    pub const EXTRACT: u8 = 7;
    /// As a run's own command: nothing of the image's runs, and the container holds,
    /// its files there for the builtins above, until it is killed: the daemon's visit to
    /// a stopped container (D37).
    pub const HOLD: u8 = 8;
    /// Each argument a `FILE=VALUE` written to the workload's cgroup, as its limits are
    /// as it starts: `shards update` of a running container (moby daemon/update.go).
    pub const CGROUP: u8 = 9;
    /// The disk the writable layer uses, in decimal: inspect's and `ps -s`'s SizeRw.
    pub const SIZE: u8 = 10;
    /// The workload's use of its microVM, for `stats`: lines of `KEY VALUE`, each in
    /// decimal, as the cgroup's interface files and /proc say them (see shards-init).
    pub const STATS: u8 = 11;
    /// With `1` its only argument, the container's cgroup frozen, once the kernel says it
    /// is, as runc's cgroup v2 freezer freezes it; with `0`, thawed: `pause` and `unpause`
    /// of a container that shares its microVM (D119).
    pub const FREEZE: u8 = 12;
}

/// A terminal's size in character cells. Zero in either leaves the pty's size alone, as
/// runc leaves it (libcontainer/utils_linux.go, setupConsole).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

impl Size {
    /// Rows, then columns, each a big-endian u16.
    pub fn encode(self) -> [u8; 4] {
        let ([a, b], [c, d]) = (self.rows.to_be_bytes(), self.cols.to_be_bytes());
        [a, b, c, d]
    }

    pub fn decode(bytes: &[u8]) -> Option<Size> {
        let [a, b, c, d] = <[u8; 4]>::try_from(bytes).ok()?;
        Some(Size {
            rows: u16::from_be_bytes([a, b]),
            cols: u16::from_be_bytes([c, d]),
        })
    }
}

impl Spec {
    /// Lists are a big-endian u32 count, then their strings; each string is a big-endian
    /// u32 length, then its bytes. Then the optional sections, each a tag and what it
    /// holds, in order: a terminal, 1 and its size; resolv.conf, 2 and its bytes; stdin
    /// read, 3 alone; a built-in, 4 and its kind; hosts, 5 and a list; a domain name, 6
    /// and its bytes; cgroup limits, 7 and a list; namespace setup, 8 and a list.
    ///
    /// Callers see [`encoded_len`](Spec::encoded_len) within [`MAX_PAYLOAD`] first: past
    /// it, a length would not fit its u32.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// [`encode`](Spec::encode), appended to `out`, which grows once, by exactly the
    /// spec's length (audit D10).
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.reserve_exact(self.encoded_len().unwrap_or(0));
        for list in [&self.argv, &self.env] {
            put(out, list.len());
            for s in list {
                put_bytes(out, s);
            }
        }
        for s in [&self.cwd, &self.user, &self.hostname] {
            put_bytes(out, s);
        }
        if let Some(size) = self.tty {
            out.push(1);
            out.extend_from_slice(&size.encode());
        }
        if let Some(r) = &self.resolv {
            out.push(2);
            put_bytes(out, r);
        }
        if self.stdin {
            out.push(3);
        }
        if self.builtin != 0 {
            out.extend_from_slice(&[4, self.builtin]);
        }
        if !self.hosts.is_empty() {
            out.push(5);
            put(out, self.hosts.len());
            for h in &self.hosts {
                put_bytes(out, h);
            }
        }
        if !self.domainname.is_empty() {
            out.push(6);
            put_bytes(out, &self.domainname);
        }
        if !self.cgroup.is_empty() {
            out.push(7);
            put(out, self.cgroup.len());
            for c in &self.cgroup {
                put_bytes(out, c);
            }
        }
        if !self.setup.is_empty() {
            out.push(8);
            put(out, self.setup.len());
            for c in &self.setup {
                put_bytes(out, c);
            }
        }
    }

    /// The bytes [`encode`](Spec::encode) writes, or `None` past `usize`: measured without
    /// encoding, so a spec's size is checked before it is built (audit D10).
    pub fn encoded_len(&self) -> Option<usize> {
        let strings = |list: &[Vec<u8>]| {
            list.iter()
                .try_fold(4usize, |n, s| n.checked_add(4)?.checked_add(s.len()))
        };
        let mut n = strings(&self.argv)?.checked_add(strings(&self.env)?)?;
        for s in [&self.cwd, &self.user, &self.hostname] {
            n = n.checked_add(4)?.checked_add(s.len())?;
        }
        let resolv = match &self.resolv {
            Some(r) => r.len().checked_add(5)?,
            None => 0,
        };
        let hosts = if self.hosts.is_empty() {
            0
        } else {
            strings(&self.hosts)?.checked_add(1)?
        };
        let domain = if self.domainname.is_empty() {
            0
        } else {
            self.domainname.len().checked_add(5)?
        };
        let cgroup = if self.cgroup.is_empty() {
            0
        } else {
            strings(&self.cgroup)?.checked_add(1)?
        };
        let setup = if self.setup.is_empty() {
            0
        } else {
            strings(&self.setup)?.checked_add(1)?
        };
        n.checked_add(if self.tty.is_some() { 5 } else { 0 })?
            .checked_add(resolv)?
            .checked_add(usize::from(self.stdin))?
            .checked_add(if self.builtin != 0 { 2 } else { 0 })?
            .checked_add(hosts)?
            .checked_add(domain)?
            .checked_add(cgroup)?
            .checked_add(setup)
    }

    /// The spec in `bytes`, or `None` unless they hold exactly one.
    pub fn decode(bytes: &[u8]) -> Option<Spec> {
        let mut r = Cursor(bytes);
        let spec = Spec {
            argv: r.list()?,
            env: r.list()?,
            cwd: r.bytes()?,
            user: r.bytes()?,
            hostname: r.bytes()?,
            tty: None,
            resolv: None,
            stdin: false,
            builtin: 0,
            hosts: Vec::new(),
            domainname: Vec::new(),
            cgroup: Vec::new(),
            setup: Vec::new(),
        };
        let mut spec = spec;
        // Optional sections, each once, in order: 1 a terminal, 2 resolv.conf, 3 stdin, 4
        // a built-in, 5 hosts, 6 a domain name, 7 cgroup limits.
        let mut last = 0u8;
        while let Some(tag) = r.take(1).and_then(|t| t.first().copied()) {
            if tag <= last {
                return None;
            }
            last = tag;
            match tag {
                1 => spec.tty = Some(Size::decode(r.take(4)?)?),
                2 => spec.resolv = Some(r.bytes()?),
                3 => spec.stdin = true,
                4 => spec.builtin = r.take(1)?.first().copied().filter(|&b| b != 0)?,
                5 => spec.hosts = r.list().filter(|l| !l.is_empty())?,
                6 => spec.domainname = r.bytes().filter(|d| !d.is_empty())?,
                7 => spec.cgroup = r.list().filter(|l| !l.is_empty())?,
                8 => spec.setup = r.list().filter(|l| !l.is_empty())?,
                _ => return None,
            }
        }
        r.0.is_empty().then_some(spec)
    }
}

fn put(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, s: &[u8]) {
    put(out, s.len());
    out.extend_from_slice(s);
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (head, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(head)
    }

    fn len(&mut self) -> Option<usize> {
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        usize::try_from(u32::from_be_bytes(b)).ok()
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.len()?;
        self.take(n).map(<[u8]>::to_vec)
    }

    fn list(&mut self) -> Option<Vec<Vec<u8>>> {
        let n = self.len()?;
        // Each entry takes at least its 4-byte length: a count beyond that is a lie.
        if n > self.0.len() / 4 {
            return None;
        }
        // Sized once: collecting through Option would grow it from empty (audit D10).
        let mut list = Vec::with_capacity(n);
        for _ in 0..n {
            list.push(self.bytes()?);
        }
        Some(list)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use alloc::vec;

    use super::*;

    /// What a command may not hold beside an image's domains (D115): none of Docker's
    /// defaults, but CAP_NET_RAW where their flows cross its network namespace; each
    /// capability that reaches them across a PID namespace, and none that does not; the
    /// domains' IDs and their servers', and no other.
    #[test]
    fn a_command_beside_domains_holds_nothing_that_reaches_them() {
        use beside::{crossing, taken};
        let caps = |list: &[u32]| list.iter().fold(0u64, |m, &c| m | 1 << c);
        let defaults = caps(&CAPS);
        assert_eq!(crossing(defaults, false), None);
        assert_eq!(crossing(defaults, true).map(|(c, _)| c), Some(13));
        for c in [16, 17, 19, 20, 21, 23, 25, 30, 34, 37, 38, 39] {
            assert_eq!(crossing(caps(&[c]), false).map(|(c, _)| c), Some(c), "{c}");
        }
        // KILL, NET_ADMIN and NET_RAW without the uplink, SYS_BOOT, CHECKPOINT_RESTORE.
        assert_eq!(crossing(caps(&[5, 12, 13, 22, 40]), false), None);
        assert_eq!(crossing(caps(&[12]), true).map(|(c, _)| c), Some(12));
        // Every capability: the lowest that reaches them.
        assert_eq!(crossing(u64::MAX, false).map(|(c, _)| c), Some(16));
        assert_eq!(crossing(u64::MAX, true).map(|(c, _)| c), Some(12));
        for (id, domains, held) in [
            (crate::DOMAIN_FIRST_ID, 1, true),
            (crate::DOMAIN_FIRST_ID + 1, 1, false),
            (crate::DOMAIN_FIRST_ID + 1, 2, true),
            (crate::DOMAIN_FIRST_ID - 1, 2, false),
            (crate::SERVER_FIRST_ID, 1, true),
            (crate::SERVER_FIRST_ID + 2, 2, false),
            (crate::DOMAIN_FIRST_ID, 0, false),
            (0, 3, false),
            (u32::MAX, 1, false),
        ] {
            assert_eq!(taken(id, domains), held, "{id} of {domains}");
        }
        assert_eq!(
            beside::id("uid", 200_001, 2),
            "cannot run as uid 200001: the image's Agentfile runs its agents and harnesses as 200000 to 200001, and their in-VM servers as 4394304 to 4394305, which no other process of the microVM takes"
        );
    }

    #[test]
    fn specs_round_trip_and_bad_ones_are_refused() {
        let spec = Spec {
            argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"echo \xff".to_vec()],
            env: vec![b"PATH=/bin".to_vec(), b"EMPTY=".to_vec()],
            cwd: b"/work".to_vec(),
            user: b"app:staff".to_vec(),
            hostname: b"box".to_vec(),
            tty: Some(Size { rows: 24, cols: 300 }),
            resolv: Some(b"nameserver 192.168.1.1\n".to_vec()),
            stdin: true,
            builtin: builtin::CHANGES,
            hosts: vec![b"10.0.0.2\tdb".to_vec(), b"::1\tlocal6".to_vec()],
            domainname: b"example.org".to_vec(),
            cgroup: vec![b"memory.max=33554432".to_vec(), b"cpu.max=50000 100000".to_vec()],
            setup: vec![b"readonly".to_vec(), b"shm=1".to_vec()],
        };
        let bytes = spec.encode();
        assert_eq!(bytes.len(), spec.encoded_len().unwrap());
        assert_eq!(Spec::decode(&bytes), Some(spec.clone()));
        // Its length measured as written, whichever optional sections it has (review 1.20).
        for sections in 0..=255u8 {
            let some = Spec {
                tty: (sections & 1 != 0).then_some(Size { rows: 1, cols: 2 }),
                resolv: (sections & 2 != 0).then(|| b"nameserver 10.0.0.1\n".to_vec()),
                stdin: sections & 4 != 0,
                builtin: if sections & 8 != 0 { builtin::PROCESSES } else { 0 },
                hosts: if sections & 16 != 0 {
                    vec![b"1.2.3.4\tx".to_vec()]
                } else {
                    Vec::new()
                },
                domainname: if sections & 32 != 0 {
                    b"d".to_vec()
                } else {
                    Vec::new()
                },
                cgroup: if sections & 64 != 0 {
                    vec![b"pids.max=7".to_vec()]
                } else {
                    Vec::new()
                },
                setup: if sections & 128 != 0 {
                    vec![b"readonly".to_vec()]
                } else {
                    Vec::new()
                },
                ..spec.clone()
            };
            let written = some.encode();
            assert_eq!(written.len(), some.encoded_len().unwrap(), "{sections:04b}");
            assert_eq!(Spec::decode(&written), Some(some), "{sections:04b}");
        }
        let piped = Spec {
            hosts: Vec::new(),
            domainname: Vec::new(),
            cgroup: Vec::new(),
            setup: Vec::new(),
            tty: None,
            resolv: None,
            stdin: false,
            builtin: 0,
            ..spec.clone()
        };
        let without = piped.encode();
        assert_eq!(Spec::decode(&without), Some(piped.clone()));
        let tty_only = Spec {
            hosts: Vec::new(),
            domainname: Vec::new(),
            cgroup: Vec::new(),
            setup: Vec::new(),
            resolv: None,
            stdin: false,
            builtin: 0,
            ..spec.clone()
        };
        let with_tty = tty_only.encode();
        let closed = Spec {
            hosts: Vec::new(),
            domainname: Vec::new(),
            cgroup: Vec::new(),
            setup: Vec::new(),
            stdin: false,
            builtin: 0,
            ..spec.clone()
        };
        let without_stdin = closed.encode();
        let commanded = Spec {
            hosts: Vec::new(),
            domainname: Vec::new(),
            cgroup: Vec::new(),
            setup: Vec::new(),
            builtin: 0,
            ..spec.clone()
        };
        let without_builtin = commanded.encode();
        let hostless = Spec {
            hosts: Vec::new(),
            domainname: Vec::new(),
            cgroup: Vec::new(),
            setup: Vec::new(),
            ..spec.clone()
        };
        let without_hosts = hostless.encode();
        let domainless = Spec {
            domainname: Vec::new(),
            cgroup: Vec::new(),
            setup: Vec::new(),
            ..spec.clone()
        };
        let without_domain = domainless.encode();
        let unlimited = Spec {
            cgroup: Vec::new(),
            setup: Vec::new(),
            ..spec.clone()
        };
        let without_cgroup = unlimited.encode();
        let bare = Spec {
            setup: Vec::new(),
            ..spec.clone()
        };
        let without_setup = bare.encode();
        assert_eq!(Spec::decode(&Spec::default().encode()), Some(Spec::default()));
        let reading = Spec {
            stdin: true,
            ..Spec::default()
        };
        assert_eq!(Spec::decode(&reading.encode()), Some(reading));
        // Frames carry their length, so only a spec cut where a section ends reads as one:
        // without its optional sections, with its terminal alone, without stdin, without
        // its built-in, without its hosts, or without its domain name.
        for cut in 0..bytes.len() {
            let expected = if cut == without.len() {
                Some(piped.clone())
            } else if cut == with_tty.len() {
                Some(tty_only.clone())
            } else if cut == without_stdin.len() {
                Some(closed.clone())
            } else if cut == without_builtin.len() {
                Some(commanded.clone())
            } else if cut == without_hosts.len() {
                Some(hostless.clone())
            } else if cut == without_domain.len() {
                Some(domainless.clone())
            } else if cut == without_cgroup.len() {
                Some(unlimited.clone())
            } else if cut == without_setup.len() {
                Some(bare.clone())
            } else {
                None
            };
            assert_eq!(Spec::decode(&bytes[..cut]), expected, "cut at {cut}");
        }
        let mut long = bytes.clone();
        long.push(0);
        assert_eq!(Spec::decode(&long), None, "trailing bytes");
        assert_eq!(Spec::decode(&[0xff, 0xff, 0xff, 0xff]), None, "a huge count");
        let mut bad_tty = Spec::default().encode();
        bad_tty.push(2);
        assert_eq!(Spec::decode(&bad_tty), None, "not a terminal");
        let mut no_builtin = Spec::default().encode();
        no_builtin.extend([4, 0]);
        assert_eq!(Spec::decode(&no_builtin), None, "a built-in of no kind");
        let mut disordered = Spec::default().encode();
        disordered.extend([3, 1, 0, 24, 1, 44]);
        assert_eq!(Spec::decode(&disordered), None, "stdin before a terminal");
    }

    #[test]
    fn sizes_are_rows_then_columns() {
        let size = Size {
            rows: 0x0102,
            cols: 0x0304,
        };
        assert_eq!(size.encode(), [1, 2, 3, 4]);
        assert_eq!(Size::decode(&size.encode()), Some(size));
        assert_eq!(Size::decode(&[1, 2, 3]), None);
    }

    #[test]
    fn headers_are_stdcopy_headers() {
        assert_eq!(header(kind::STDERR, 0x0102_0304), [2, 0, 0, 0, 1, 2, 3, 4]);
        assert_eq!(parse_header([1, 0, 0, 0, 0, 0, 1, 0]), Some((kind::STDOUT, 256)));
        assert_eq!(parse_header([1, 0, 1, 0, 0, 0, 1, 0]), None);
        assert_eq!(parse_header(header(kind::STDOUT, MAX_PAYLOAD + 1)), None);
    }
}

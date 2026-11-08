//! The processes a run's members start and end, as the kernel's process events connector
//! says them (AGENTFILE_ARCH.md §9.9, architecture.md D123): each fork, exec and exit of a
//! process of the run's command and its execs, its agents and harnesses, and its joiners,
//! for the host to keep (`shards read processes`, `shards watch processes`). A record, not
//! a boundary: the confinement of §9.3 holds without it.
//!
//! A process is its member's from its first event there. A process a member's forks is
//! that member's, as the fork's event says without asking /proc, where a child that ended
//! at once is gone. Init starts each member's first processes, as its own children, in the
//! member's cgroup (`/workload`, `/domains/<kind>-<name>`, `/join-<id>`), which /proc names:
//! from its fork where it is born there (CLONE_INTO_CGROUP), from its exec where it moves
//! itself there before. Init reads the events waiting before it reaps a child, so that its
//! child's /proc is there to read.
//!
//! An exec's command line is the one the kernel sends with its event: shards' guest kernel
//! sends what the exec laid out, before the program runs (resources/kernel/patches/0002,
//! PM M176), so a process that ends at once has it too. Where a kernel sends none, it is
//! /proc/PID/cmdline as the event is read, else the process's name, else nothing, each
//! said as such.
//!
//! Each record is a line, its fields tab-separated: the time in nanoseconds since 1970,
//! the member (`command`, `agent NAME`, `harness NAME`, `joiner ID`), the process's PID in
//! the microVM, and the event with what it says: `fork` and the parent's PID; `exec` and
//! the command line, each argument quoted where it holds a space, a quote, a backslash or
//! a control character or is empty, then, where that is not all of it, a tab and `+N` for
//! the N bytes left out, `comm` where it is the process's name alone, `gone` where it is
//! nothing; `exit` and `code N` or `signal N`. Events the kernel could not queue are a
//! record of their own, `lost`, and a run whose record cannot be kept says why,
//! `unrecorded`. Threads are left out, and init's own processes.

use std::collections::HashMap;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// `NETLINK_CONNECTOR`, and the process connector's ids and request (include/uapi/linux/
/// connector.h, cn_proc.h).
const NETLINK_CONNECTOR: i32 = 11;
const CN_IDX_PROC: u32 = 1;
const CN_VAL_PROC: u32 = 1;
const PROC_CN_MCAST_LISTEN: u32 = 1;
const NLMSG_DONE: u16 = 3;
/// A message's `nlmsghdr` and `cn_msg`, before its `proc_event`.
const HEADERS: usize = 16 + 20;
/// A `proc_event`: its kind, CPU and time, then its data, as long as its longest kind's,
/// an exit's six words.
const EVENT: usize = 16 + 24;
/// `proc_event`'s `what`.
const PROC_EVENT_FORK: u32 = 0x1;
const PROC_EVENT_EXEC: u32 = 0x2;
const PROC_EVENT_EXIT: u32 = 0x8000_0000;
/// The most of a command line a record says: as much as shards' guest kernel sends with
/// an exec's event (resources/kernel/patches/0002, CN_PROC_EXEC_ARGS).
const COMMAND_MOST: usize = 4096;
/// The receive buffer asked for (SO_RCVBUFFORCE), past the kernel's default
/// (net.core.rmem_default, 208 KiB): what a burst of short-lived processes queues before
/// init reads it. Events past it are counted `lost`, never dropped unsaid.
const BUFFER: i32 = 4 << 20;

/// The process events connector, listened to.
pub struct Listener {
    sock: OwnedFd,
    record: Record,
}

impl Listener {
    /// Listens to the connector: from here on, every process event of the microVM.
    pub fn new() -> io::Result<Listener> {
        // SAFETY: socket(2) with constant arguments.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                NETLINK_CONNECTOR,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor just made, owned here alone.
        let sock = unsafe { OwnedFd::from_raw_fd(fd) };
        let size = BUFFER;
        // SAFETY: setsockopt(2) of an int of ours; init holds CAP_NET_ADMIN, which the
        // forced size takes.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUFFORCE,
                (&raw const size).cast(),
                size_of::<i32>() as libc::socklen_t,
            );
        }
        // SAFETY: zeroed is a valid sockaddr_nl.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = CN_IDX_PROC;
        // SAFETY: bind(2) of a sockaddr_nl of ours, its size given.
        if unsafe {
            libc::bind(
                fd,
                (&raw const addr).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let len = u32::try_from(HEADERS + 4).unwrap_or(u32::MAX);
        let mut ask = Vec::with_capacity(HEADERS + 4);
        ask.extend_from_slice(&len.to_ne_bytes());
        ask.extend_from_slice(&NLMSG_DONE.to_ne_bytes());
        ask.extend_from_slice(&0u16.to_ne_bytes());
        ask.extend_from_slice(&0u32.to_ne_bytes());
        ask.extend_from_slice(&std::process::id().to_ne_bytes());
        ask.extend_from_slice(&CN_IDX_PROC.to_ne_bytes());
        ask.extend_from_slice(&CN_VAL_PROC.to_ne_bytes());
        ask.extend_from_slice(&0u32.to_ne_bytes());
        ask.extend_from_slice(&0u32.to_ne_bytes());
        ask.extend_from_slice(&4u16.to_ne_bytes());
        ask.extend_from_slice(&0u16.to_ne_bytes());
        ask.extend_from_slice(&PROC_CN_MCAST_LISTEN.to_ne_bytes());
        // SAFETY: send(2) of a buffer of ours, its length given.
        if unsafe { libc::send(fd, ask.as_ptr().cast(), ask.len(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Listener {
            sock,
            record: Record {
                offset: clock(libc::CLOCK_REALTIME) - clock(libc::CLOCK_MONOTONIC),
                members: HashMap::new(),
                lost: false,
            },
        })
    }

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    /// Every event waiting, each a record in `out`.
    pub fn read(&mut self, out: &mut Vec<u8>) {
        // An event with the most of a command line the kernel sends, and room to spare.
        let mut buf = [0u8; 2 * COMMAND_MOST];
        loop {
            // SAFETY: recv(2) into a buffer of ours, its length given.
            let n = unsafe { libc::recv(self.fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::ENOBUFS) {
                    self.record.lost(clock(libc::CLOCK_REALTIME), out);
                    continue;
                }
                return;
            }
            let got = buf.get(..usize::try_from(n).unwrap_or(0)).unwrap_or_default();
            if let Some(event) = event_of(got) {
                self.record.event(event, out, &member_of);
            }
        }
    }

    /// Whether every process of a member's the record has seen start has its end in it, so
    /// that a run's record may end with the run, as its last processes' ends are read
    /// (none is pending past the kernel's queue but a loss, which the record says).
    pub fn settled(&self) -> bool {
        self.record.lost || self.record.members.is_empty()
    }
}

/// A record of `why` a run's processes could not be recorded.
pub fn unrecorded(why: &str) -> Vec<u8> {
    let now = clock(libc::CLOCK_REALTIME);
    format!("{now}\t-\t0\tunrecorded\t{}\n", quote(why)).into_bytes()
}

/// The `proc_event` a message holds, with what the kernel sent past it: as long as its
/// `cn_msg` says, without the padding to four bytes that netlink puts after it (nlmsg_put).
fn event_of(message: &[u8]) -> Option<&[u8]> {
    // `cn_msg`'s `len`, after its id, seq and ack.
    let len = message
        .get(16 + 16..16 + 18)?
        .try_into()
        .ok()
        .map(u16::from_ne_bytes)?;
    message.get(HEADERS..HEADERS + usize::from(len))
}

/// The record the events make.
struct Record {
    /// CLOCK_REALTIME less CLOCK_MONOTONIC, whose time the kernel stamps each event with,
    /// in nanoseconds, as it began listening.
    offset: i128,
    /// Each living process's member, by its PID.
    members: HashMap<u32, String>,
    /// Whether events were lost: then a process's end may never come.
    lost: bool,
}

impl Record {
    fn lost(&mut self, now: i128, out: &mut Vec<u8>) {
        self.lost = true;
        out.extend_from_slice(format!("{now}\t-\t0\tlost\t\n").as_bytes());
    }

    /// One `proc_event`, `e`: its kind, the CPU, its time (8 bytes at 8), then its data at
    /// 16, and what the kernel sends past it; `cgroup` the member a process's cgroup names.
    fn event(&mut self, e: &[u8], out: &mut Vec<u8>, cgroup: &dyn Fn(u32) -> Option<String>) {
        let word = |at: usize| {
            e.get(at..at + 4)
                .and_then(|b| b.try_into().ok())
                .map(u32::from_ne_bytes)
        };
        let stamp = e
            .get(8..16)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_ne_bytes);
        let (Some(what), Some(stamp)) = (word(0), stamp) else {
            return;
        };
        let time = self.offset + i128::from(stamp);
        let mut record = |member: &str, pid: u32, event: &str, says: &str| {
            out.extend_from_slice(format!("{time}\t{member}\t{pid}\t{event}\t{says}\n").as_bytes());
        };
        match what {
            PROC_EVENT_FORK => {
                // parent_pid, parent_tgid, child_pid, child_tgid.
                let (Some(parent), Some(pid), Some(tgid)) = (word(20), word(24), word(28)) else {
                    return;
                };
                if pid != tgid {
                    return;
                }
                // A member's child is its member's; any other's, init's, its cgroup's.
                let member = match self.members.get(&parent) {
                    Some(m) => Some(m.clone()),
                    None => cgroup(pid),
                };
                match member {
                    Some(member) => {
                        record(&member, pid, "fork", &parent.to_string());
                        self.members.insert(pid, member);
                    }
                    None => {
                        self.members.remove(&pid);
                    }
                }
            }
            PROC_EVENT_EXEC => {
                // process_pid, process_tgid.
                let (Some(pid), Some(tgid)) = (word(16), word(20)) else {
                    return;
                };
                if pid != tgid {
                    return;
                }
                let Some(member) = self.members.get(&pid).cloned().or_else(|| cgroup(pid)) else {
                    return;
                };
                let says = match e.get(EVENT..) {
                    Some(sent) if !sent.is_empty() => sent_command_line(sent),
                    _ => read_command_line(pid),
                };
                record(&member, pid, "exec", &says);
                self.members.insert(pid, member);
            }
            PROC_EVENT_EXIT => {
                // process_pid, process_tgid, exit_code (a wait status), exit_signal.
                let (Some(pid), Some(tgid), Some(status)) = (word(16), word(20), word(24)) else {
                    return;
                };
                if pid != tgid {
                    return;
                }
                let Some(member) = self.members.remove(&pid).or_else(|| cgroup(pid)) else {
                    return;
                };
                let says = match status & 0x7f {
                    0 => format!("code {}", (status >> 8) & 0xff),
                    signal => format!("signal {signal}"),
                };
                record(&member, pid, "exit", &says);
            }
            _ => {}
        }
    }
}

/// `clock`'s time in nanoseconds.
fn clock(clock: libc::clockid_t) -> i128 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime(2) into a timespec of ours.
    unsafe { libc::clock_gettime(clock, &raw mut t) };
    i128::from(t.tv_sec) * 1_000_000_000 + i128::from(t.tv_nsec)
}

/// The member process `pid` belongs to, by its cgroup: the run's command and its execs
/// (`/workload`), an agent or harness (`/domains/<kind>-<name>`), a joiner (`/join-<id>`);
/// none of init's own, or of one gone.
fn member_of(pid: u32) -> Option<String> {
    let mut text = String::new();
    std::fs::File::open(format!("/proc/{pid}/cgroup"))
        .and_then(|mut f| f.read_to_string(&mut text))
        .ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    member_by_cgroup(path)
}

/// [`member_of`] of a cgroup's path.
fn member_by_cgroup(path: &str) -> Option<String> {
    // Every member is under the run's cgroup (D148); init's own are not.
    let path = path.strip_prefix(crate::run::RUN_PATH)?;
    if path == "/workload" || path.starts_with("/workload/") {
        return Some("command".into());
    }
    if let Some(domain) = path.strip_prefix("/domains/") {
        let (kind, name) = domain.split('/').next()?.split_once('-')?;
        return matches!(kind, "agent" | "harness").then(|| format!("{kind} {name}"));
    }
    let id = path.strip_prefix("/join-")?.split('/').next()?;
    Some(format!("joiner {id}"))
}

/// The command line the kernel sent past an exec's event: the arguments' whole length, a
/// native-endian u32, then as much of them as it sent.
fn sent_command_line(sent: &[u8]) -> String {
    let Some((whole, args)) = sent.split_first_chunk::<4>() else {
        return quoted(&[], 0);
    };
    let whole = usize::try_from(u32::from_ne_bytes(*whole)).unwrap_or(usize::MAX);
    quoted(args, whole.saturating_sub(args.len()))
}

/// Process `pid`'s command line, where the kernel sent none, as /proc has it as its event is
/// read: none once the process has ended, its name then while it is not yet reaped.
fn read_command_line(pid: u32) -> String {
    let mut raw = Vec::new();
    let mut left = 0;
    if let Ok(mut f) = std::fs::File::open(format!("/proc/{pid}/cmdline")) {
        let _ = (&mut f).take(COMMAND_MOST as u64).read_to_end(&mut raw);
        left = io::copy(&mut f, &mut io::sink()).unwrap_or(0);
    }
    if !raw.is_empty() {
        return quoted(&raw, usize::try_from(left).unwrap_or(usize::MAX));
    }
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) => format!("{}\tcomm", quote(comm.trim_end_matches('\n'))),
        Err(_) => "\tgone".into(),
    }
}

/// A command line's arguments, as `/proc/PID/cmdline` gives them, each NUL-terminated, as
/// a record says them, with the `left` bytes past them that it leaves out.
fn quoted(raw: &[u8], left: usize) -> String {
    let mut said = if raw.is_empty() {
        String::new()
    } else {
        raw.strip_suffix(b"\0")
            .unwrap_or(raw)
            .split(|&b| b == 0)
            .map(|a| quote(&String::from_utf8_lossy(a)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    if left > 0 {
        said.push_str(&format!("\t+{left}"));
    }
    said
}

/// An argument as a record says it: quoted where it holds a space, a quote, a backslash
/// or a control character, or is empty (Rust's `{:?}`, as Go's `%q` writes one).
fn quote(a: &str) -> String {
    if !a.is_empty()
        && a.chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && !matches!(c, '"' | '\'' | '\\'))
    {
        a.to_string()
    } else {
        format!("{a:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Members by cgroup: the run's command and its execs, an agent or a harness, a
    /// joiner; none of init's own.
    #[test]
    fn members_are_their_cgroups() {
        for (path, member) in [
            ("/run/workload", Some("command")),
            ("/run/workload/sub", Some("command")),
            ("/run/domains/agent-web", Some("agent web")),
            ("/run/domains/harness-ci-main", Some("harness ci-main")),
            ("/run/domains/other-x", None),
            ("/run/join-7", Some("joiner 7")),
            ("/workload", None),
            ("/run", None),
            ("/", None),
            ("/init", None),
        ] {
            assert_eq!(member_by_cgroup(path).as_deref(), member, "{path}");
        }
    }

    /// A command line as a record says it: arguments that a tab or a line would split, or
    /// a shell would read otherwise, quoted, and empty ones; what it leaves out counted.
    #[test]
    fn command_lines_are_quoted_where_they_must_be() {
        assert_eq!(quoted(b"node\0server.js\0", 0), "node server.js");
        assert_eq!(quoted(b"sh\0-c\0echo a\tb\nc\0", 0), r#"sh -c "echo a\tb\nc""#);
        assert_eq!(quoted(b"say\0it's\0", 0), r#"say "it's""#);
        assert_eq!(quoted(b"sh\0-c\0\0", 0), r#"sh -c """#);
        assert_eq!(quoted(b"\0", 0), r#""""#);
        assert_eq!(quoted(b"", 0), "");
        assert_eq!(quoted(b"echo\0aaa", 4096), "echo aaa\t+4096");
    }

    /// A message's event, as long as its `cn_msg` says, the padding after it left out.
    #[test]
    fn an_event_is_as_long_as_its_message_says() {
        let mut message = vec![0u8; HEADERS];
        message[32..34].copy_from_slice(&54u16.to_ne_bytes());
        message.extend([7u8; 54]);
        message.extend([0u8; 2]);
        assert_eq!(event_of(&message), Some(&[7u8; 54][..]));
        assert_eq!(event_of(&message[..HEADERS + 10]), None, "a message cut short");
    }

    /// A `proc_event` of kind `what` at time `stamp`, its data `words`.
    fn event(what: u32, stamp: u64, words: &[u32]) -> Vec<u8> {
        let mut e = Vec::new();
        e.extend(what.to_ne_bytes());
        e.extend(0u32.to_ne_bytes());
        e.extend(stamp.to_ne_bytes());
        for w in words {
            e.extend(w.to_ne_bytes());
        }
        e.resize(EVENT, 0);
        e
    }

    /// The events of a member's processes, recorded: a child of a member's is its member's
    /// without its cgroup asked, which a child gone at once no longer has; an exec says
    /// the command line its event carries, and how much of it the event left out; an end
    /// says its code or signal, and leaves nothing pending. A process init starts is its
    /// cgroup's, from its fork where it is born there and from its exec where it moves
    /// there first; init's own are left out.
    #[test]
    fn a_members_processes_are_recorded_from_their_events() {
        let mut record = Record {
            offset: 0,
            members: HashMap::new(),
            lost: false,
        };
        let mut out = Vec::new();
        // Init's children: 10, born in the workload's cgroup, 20 moved into a joiner's
        // after its fork and before its exec, 30 its own; the children of 10's, none with
        // a cgroup to read.
        let moved = std::cell::Cell::new(false);
        let cgroup = |pid: u32| match pid {
            10 => Some("command".to_string()),
            20 if moved.get() => Some("joiner 2".to_string()),
            _ => None,
        };
        let exec = |stamp: u64, pid: u32, args: &[u8], whole: u32| {
            let mut e = event(PROC_EVENT_EXEC, stamp, &[pid, pid]);
            e.extend(whole.to_ne_bytes());
            e.extend(args);
            e
        };
        let exit = |stamp: u64, pid: u32, status: u32| event(PROC_EVENT_EXIT, stamp, &[pid, pid, status, 17]);
        for (i, e) in [
            event(PROC_EVENT_FORK, 1, &[1, 1, 10, 10]),
            exec(2, 10, b"sh\0-c\0for i in 1 2; do true; done\0", 34),
            event(PROC_EVENT_FORK, 3, &[10, 10, 11, 11]),
            exec(4, 11, b"/bin/true\0", 10),
            exit(5, 11, 0),
            // A thread of 10's, left out.
            event(PROC_EVENT_FORK, 6, &[10, 10, 12, 10]),
            // A joiner's process, its fork before its move.
            event(PROC_EVENT_FORK, 7, &[1, 1, 20, 20]),
            exec(8, 20, b"cat\0/etc/hostna", 18),
            exit(9, 20, 9),
            // Init's own.
            event(PROC_EVENT_FORK, 10, &[1, 1, 30, 30]),
            exec(11, 30, b"tar\0", 4),
            exit(12, 30, 0),
            exit(13, 10, 3 << 8),
        ]
        .iter()
        .enumerate()
        {
            // 20 moves once its fork is read.
            moved.set(i > 6);
            record.event(e, &mut out, &cgroup);
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            [
                "1\tcommand\t10\tfork\t1",
                "2\tcommand\t10\texec\tsh -c \"for i in 1 2; do true; done\"",
                "3\tcommand\t11\tfork\t10",
                "4\tcommand\t11\texec\t/bin/true",
                "5\tcommand\t11\texit\tcode 0",
                "8\tjoiner 2\t20\texec\tcat /etc/hostna\t+3",
                "9\tjoiner 2\t20\texit\tsignal 9",
                "13\tcommand\t10\texit\tcode 3",
                "",
            ]
            .join("\n")
        );
        assert!(record.members.is_empty(), "{:?}", record.members);
    }
}

//! The processes a run's members start and end, as the kernel's process events connector
//! says them (AGENTFILE_ARCH.md §9.9, architecture.md D123): each fork, exec and exit of a
//! process of the run's command and its execs, its agents and harnesses, and its joiners,
//! with the member its cgroup says it belongs to, for the host to keep (`shards read
//! processes`, `shards watch processes`). A record, not a boundary: the confinement of
//! §9.3 holds without it.
//!
//! Each record is a line, its fields tab-separated: the time in nanoseconds since 1970,
//! the member (`command`, `agent NAME`, `harness NAME`, `joiner ID`), the process's PID in
//! the microVM, and the event with what it says: `fork` and the parent's PID, `exec` and
//! the command line, each argument quoted where it holds what a line or a tab would split,
//! `exit` and `code N` or `signal N`. Events the kernel could not queue are a record of
//! their own, `lost`. Threads are left out, and init's own processes.

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
/// `proc_event`'s `what`.
const PROC_EVENT_FORK: u32 = 0x1;
const PROC_EVENT_EXEC: u32 = 0x2;
const PROC_EVENT_EXIT: u32 = 0x8000_0000;
/// The most of a command line a record says.
const COMMAND_MOST: usize = 4096;
/// The receive buffer asked for (SO_RCVBUFFORCE), past the kernel's default
/// (net.core.rmem_default, 208 KiB): what a burst of short-lived processes queues before
/// init reads it. Events past it are counted `lost`, never dropped unsaid.
const BUFFER: i32 = 4 << 20;

/// The process events connector, listened to.
pub struct Listener {
    sock: OwnedFd,
    /// CLOCK_REALTIME less CLOCK_MONOTONIC, whose time the kernel stamps each event with,
    /// in nanoseconds, as it began listening.
    offset: i128,
    /// Each process's member, from its fork or exec, for its exit, when /proc may no
    /// longer say it.
    members: HashMap<u32, String>,
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
            offset: clock(libc::CLOCK_REALTIME) - clock(libc::CLOCK_MONOTONIC),
            members: HashMap::new(),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    /// Every event waiting, each a record in `out`.
    pub fn read(&mut self, out: &mut Vec<u8>) {
        let mut buf = [0u8; 1024];
        loop {
            // SAFETY: recv(2) into a buffer of ours, its length given.
            let n = unsafe { libc::recv(self.fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::ENOBUFS) {
                    let now = clock(libc::CLOCK_REALTIME);
                    out.extend_from_slice(format!("{now}\t-\t0\tlost\t\n").as_bytes());
                    continue;
                }
                return;
            }
            let got = buf.get(..usize::try_from(n).unwrap_or(0)).unwrap_or_default();
            if let Some(event) = got.get(HEADERS..) {
                self.event(event, out);
            }
        }
    }

    /// One `proc_event`: its kind, the CPU, its time (8 bytes at 8), then its data at 16.
    fn event(&mut self, e: &[u8], out: &mut Vec<u8>) {
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
                let Some(member) = member_of(pid) else {
                    return;
                };
                record(&member, pid, "fork", &parent.to_string());
                self.members.insert(pid, member);
            }
            PROC_EVENT_EXEC => {
                // process_pid, process_tgid.
                let (Some(pid), Some(tgid)) = (word(16), word(20)) else {
                    return;
                };
                if pid != tgid {
                    return;
                }
                let Some(member) = member_of(pid).or_else(|| self.members.get(&pid).cloned()) else {
                    return;
                };
                record(&member, pid, "exec", &command_line(pid));
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
                let Some(member) = self.members.remove(&pid).or_else(|| member_of(pid)) else {
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

/// Process `pid`'s command line, each argument quoted where it holds a space, a quote, a
/// backslash or a control character (Rust's `{:?}`, as Go's `%q` writes one); none of a
/// process gone before it could be read.
fn command_line(pid: u32) -> String {
    let mut raw = Vec::new();
    let _ = std::fs::File::open(format!("/proc/{pid}/cmdline"))
        .and_then(|f| f.take(COMMAND_MOST as u64).read_to_end(&mut raw));
    quoted(&raw)
}

/// [`command_line`] of `/proc/PID/cmdline`'s bytes.
fn quoted(raw: &[u8]) -> String {
    raw.split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| {
            let a = String::from_utf8_lossy(a);
            if a.chars()
                .all(|c| !c.is_whitespace() && !c.is_control() && !matches!(c, '"' | '\'' | '\\'))
            {
                a.into_owned()
            } else {
                format!("{a:?}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Members by cgroup: the run's command and its execs, an agent or a harness, a
    /// joiner; none of init's own.
    #[test]
    fn members_are_their_cgroups() {
        for (path, member) in [
            ("/workload", Some("command")),
            ("/workload/sub", Some("command")),
            ("/domains/agent-web", Some("agent web")),
            ("/domains/harness-ci-main", Some("harness ci-main")),
            ("/domains/other-x", None),
            ("/join-7", Some("joiner 7")),
            ("/", None),
            ("/init", None),
        ] {
            assert_eq!(member_by_cgroup(path).as_deref(), member, "{path}");
        }
    }

    /// A command line as a record says it: arguments that a tab or a line would split, or
    /// a shell would read otherwise, quoted.
    #[test]
    fn command_lines_are_quoted_where_they_must_be() {
        assert_eq!(quoted(b"node\0server.js\0"), "node server.js");
        assert_eq!(quoted(b"sh\0-c\0echo a\tb\nc\0"), r#"sh -c "echo a\tb\nc""#);
        assert_eq!(quoted(b"say\0it's\0"), r#"say "it's""#);
        assert_eq!(quoted(b""), "");
    }
}

//! The network process's confinement on Linux (D31): Landlock
//! (https://docs.kernel.org/userspace-api/landlock.html), as the VM process has it (D30).
//! The network process parses every frame a guest and every packet the Internet sends it,
//! so it holds nothing it does not use: no file, no TCP port of its own, no signal or
//! abstract Unix socket past itself, and TCP connections only where its policy reaches.
//!
//! Two layers, as a policy comes in two steps: [`at_start`] before the process reads
//! anything a guest sends, and [`to_ports`] when an Agentfile's egress grants come
//! (`NET_POLICY`, sent once, before the run). Landlock layers only narrow: what the first
//! leaves open the second may close, never the reverse.
//!
//! It fails closed, as the VM process does (PM M66): a kernel whose Landlock is older than
//! ABI v5, which the VM process needs too, starts no network process.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// include/uapi/linux/landlock.h
const CREATE_RULESET_VERSION: u32 = 1;
/// The oldest ABI shards confines its processes under: v5 (Linux 6.10), as the VM
/// process's (shards/src/confine.rs); its TCP rules are v4's.
const MIN_ABI: i64 = 5;
const RULE_NET_PORT: libc::c_long = 2;
const REFER: u64 = 1 << 13;
const TRUNCATE: u64 = 1 << 14;
const IOCTL_DEV: u64 = 1 << 15;
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct NetPort {
    allowed_access: u64,
    port: u64,
}

/// The kernel's Landlock ABI, at least [`MIN_ABI`].
fn abi() -> io::Result<i64> {
    // SAFETY: landlock_create_ruleset(2) with no attribute asks only the ABI's version.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if abi < 0 {
        let e = io::Error::last_os_error();
        return Err(io::Error::other(match e.raw_os_error() {
            Some(libc::ENOSYS | libc::EOPNOTSUPP) => format!(
                "this kernel has no Landlock ({e}): shards confines its network process with it, \
                 and needs Linux 6.10 or later with Landlock enabled (lsm=...,landlock)"
            ),
            _ => format!("Landlock's ABI: {e}"),
        }));
    }
    if abi < MIN_ABI {
        return Err(io::Error::other(format!(
            "this kernel's Landlock is ABI v{abi}: shards needs v{MIN_ABI} (Linux 6.10) or later"
        )));
    }
    Ok(abi)
}

/// A ruleset handling `fs`, `net` and, where the ABI knows them (v6), `scoped`.
fn ruleset(abi: i64, fs: u64, net: u64, scoped: u64) -> io::Result<OwnedFd> {
    let attr = RulesetAttr {
        handled_access_fs: fs,
        handled_access_net: net,
        scoped: if abi >= 6 { scoped } else { 0 },
    };
    // The attribute's size as this ABI knows it: a v5 kernel refuses a longer one.
    let size = if abi >= 6 {
        std::mem::size_of::<RulesetAttr>()
    } else {
        16
    };
    // SAFETY: landlock_create_ruleset(2) reads `size` bytes of `attr`.
    let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &raw const attr, size, 0u32) };
    if fd < 0 {
        return Err(io::Error::other(format!(
            "Landlock's ruleset: {}",
            io::Error::last_os_error()
        )));
    }
    let fd =
        i32::try_from(fd).map_err(|_| io::Error::other("Landlock's ruleset: a descriptor out of range"))?;
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// This process, every thread of it, under `ruleset` from here on.
fn restrict(ruleset: &OwnedFd) -> io::Result<()> {
    // SAFETY: prctl(2) sets a flag of this process's; Landlock needs it, as no process
    // under it may gain privileges by exec.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::other(format!(
            "no_new_privs: {}",
            io::Error::last_os_error()
        )));
    }
    // LANDLOCK_RESTRICT_SELF_TSYNC (Linux 6.18) would take every thread; the network
    // process has none yet when it is confined, and each it makes inherits the domain.
    // SAFETY: landlock_restrict_self(2) of a ruleset descriptor of ours.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) } != 0 {
        return Err(io::Error::other(format!(
            "Landlock: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Confines the network process before it serves its guest: every filesystem right
/// Landlock's ABI v5 knows handled and none allowed, so no file is opened, made, read or
/// written; no TCP port bound (the ports it serves come bound, from the daemon); signals
/// and abstract Unix sockets kept to itself (v6). `no_tcp`, under a build's proxy (D110),
/// where every flow goes to the proxy's Unix socket: no TCP connection at all.
pub fn at_start(no_tcp: bool) -> io::Result<()> {
    let abi = abi()?;
    // v1's rights, EXECUTE through MAKE_SYM, then REFER (v2), TRUNCATE (v3) and IOCTL_DEV
    // (v5); v6 and v7 add none.
    let fs = ((1u64 << 13) - 1) | REFER | TRUNCATE | IOCTL_DEV;
    let net = NET_BIND_TCP | if no_tcp { NET_CONNECT_TCP } else { 0 };
    let ruleset = ruleset(abi, fs, net, SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL)?;
    restrict(&ruleset)
}

/// Narrows the network process's TCP connections to `ports`, an Agentfile's egress grants
/// (D59) as `NET_POLICY` brings them: what its flows may reach past the microVM, and its
/// resolvers' port where names may be resolved. Its own code holds each flow to its
/// address too; this holds the process, should that code be taken over.
pub fn to_ports(ports: &[u16]) -> io::Result<()> {
    let abi = abi()?;
    let ruleset = ruleset(abi, 0, NET_CONNECT_TCP, 0)?;
    let mut seen = std::collections::BTreeSet::new();
    for &port in ports {
        if !seen.insert(port) {
            continue;
        }
        let rule = NetPort {
            allowed_access: NET_CONNECT_TCP,
            port: u64::from(port),
        };
        // SAFETY: landlock_add_rule(2) reads the rule.
        let r = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                RULE_NET_PORT,
                &raw const rule,
                0u32,
            )
        };
        if r != 0 {
            return Err(io::Error::other(format!(
                "Landlock's rule for TCP port {port}: {}",
                io::Error::last_os_error()
            )));
        }
    }
    restrict(&ruleset)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::io::ErrorKind;
    use std::net::{TcpListener, TcpStream};
    use std::process::Command;

    /// Runs `test` of this module in a process of its own, as Landlock holds a process for
    /// good: the test binary again, told by `SHARDS_CONFINE_CHILD` to confine itself.
    fn in_child(test: &str, env: &[(&str, String)]) {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            &format!("confine::tests::{test}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SHARDS_CONFINE_CHILD", "1");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{test}: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn child() -> bool {
        std::env::var_os("SHARDS_CONFINE_CHILD").is_some()
    }

    /// Whether this host confines as shards needs: a test returns where it cannot, as VM
    /// tests do where the host has no hypervisor (a container whose seccomp profile refuses
    /// Landlock's calls, a kernel before 6.10).
    fn cannot_confine() -> bool {
        match super::abi() {
            Ok(_) => false,
            Err(e) => {
                eprintln!("SKIP: {e}");
                true
            }
        }
    }

    /// At start: no file opened, no TCP port bound; a TCP connection still made, as a
    /// guest's flow is, except under a build's proxy.
    #[test]
    fn at_start_leaves_no_file_and_no_port_of_its_own() {
        if cannot_confine() {
            return;
        }
        if !child() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port().to_string();
            in_child(
                "at_start_leaves_no_file_and_no_port_of_its_own",
                &[("PORT", port.clone()), ("NO_TCP", String::new())],
            );
            in_child(
                "at_start_leaves_no_file_and_no_port_of_its_own",
                &[("PORT", port), ("NO_TCP", "1".into())],
            );
            return;
        }
        let no_tcp = std::env::var("NO_TCP").is_ok_and(|v| !v.is_empty());
        let port: u16 = std::env::var("PORT").unwrap().parse().unwrap();
        super::at_start(no_tcp).unwrap();
        let opened = std::fs::File::open("/proc/self/status").map(|_| ());
        assert_eq!(opened.unwrap_err().kind(), ErrorKind::PermissionDenied);
        let bound = TcpListener::bind("127.0.0.1:0").map(|_| ());
        assert_eq!(bound.unwrap_err().kind(), ErrorKind::PermissionDenied);
        let connected = TcpStream::connect(("127.0.0.1", port)).map(|_| ());
        if no_tcp {
            assert_eq!(connected.unwrap_err().kind(), ErrorKind::PermissionDenied);
        } else {
            connected.unwrap();
        }
    }

    /// A policy's ports: a connection to one granted made, to any other refused.
    #[test]
    fn to_ports_reaches_the_granted_ports_alone() {
        if cannot_confine() {
            return;
        }
        if !child() {
            let granted = TcpListener::bind("127.0.0.1:0").unwrap();
            let other = TcpListener::bind("127.0.0.1:0").unwrap();
            in_child(
                "to_ports_reaches_the_granted_ports_alone",
                &[
                    ("GRANTED", granted.local_addr().unwrap().port().to_string()),
                    ("OTHER", other.local_addr().unwrap().port().to_string()),
                ],
            );
            return;
        }
        let granted: u16 = std::env::var("GRANTED").unwrap().parse().unwrap();
        let other: u16 = std::env::var("OTHER").unwrap().parse().unwrap();
        super::at_start(false).unwrap();
        super::to_ports(&[granted, granted]).unwrap();
        TcpStream::connect(("127.0.0.1", granted)).unwrap();
        let refused = TcpStream::connect(("127.0.0.1", other)).map(|_| ());
        assert_eq!(refused.unwrap_err().kind(), ErrorKind::PermissionDenied);
    }
}

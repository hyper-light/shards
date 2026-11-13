//! A command run in one of the run's members (the harness plan's step 6, G24; D243): born
//! in the member's PID namespace and into its cgroup at once (clone3(2) `CLONE_INTO_CGROUP`,
//! so the cgroup's `pids.max` holds it as it is born, which a move into `cgroup.procs` would
//! not), it enters the namespaces of the member's command (one setns(2) of a pidfd, Linux
//! 5.8), then is confined as that command was, by the same values
//! ([`crate::domains::Confinement`]) through the same steps ([`crate::domains::confine`]): the
//! member's uid and gid and no other group, no capability, `no_new_privs`, its Landlock
//! rules and its seccomp filter, its signals a new process's. crun's exec joins a container
//! the same way (src/libcrun/linux.c, `try_setns_with_pidfd` and `CLONE_INTO_CGROUP`).
//!
//! Its environment is the member's as it started, the exec's variables over it; its working
//! directory the exec's, else the member's; its program found on that environment's `PATH`
//! as the member's own was. It holds no descriptor of init's but its stdio: pipes init
//! relays as an exec's, or a terminal whose master init holds. The terminal is init's own
//! devpts', handed over as a descriptor (TIOCGPTPEER): a member's `/dev` holds no `pts`.
//!
//! Landlock makes the command a domain of its own, a sibling of its member's (a ruleset is
//! entered, never joined: landlock(7)), so the scopes the member is held to hold between
//! them too: it signals, and reaches abstract Unix sockets of, its own processes alone.

use std::ffi::CString;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use shards_abi::run::members::Refusal;
use shards_abi::run::{self, Size};

use crate::domains::{Confinement, Started};
use crate::run::{NotBorn, step};

/// A command started in a member: its pid, init's child, and init's ends of its stdio.
/// With a terminal, `stdout` is its master and `stdin` a duplicate of it, and there is no
/// `stderr`.
#[derive(Debug)]
pub struct Entered {
    pub pid: libc::pid_t,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
    pub tty: bool,
}

/// Why no command started in the member.
#[derive(Debug)]
pub enum Failed {
    /// Refused, by type, with the words for a person.
    Refused(Refusal, String),
    /// Its child could not run it: the step that failed ([`step`]), its errno, and the
    /// candidate it was trying, by its index in `tried`.
    Step { which: u8, errno: i32, tried: Vec<u8> },
}

/// What the child uses, made before it is: after clone3 it calls the kernel alone.
struct Prepared<'a> {
    actor: RawFd,
    stdio: [RawFd; 3],
    tty: bool,
    err: RawFd,
    cwd: CString,
    candidates: Vec<CString>,
    explicit: bool,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
    confinement: &'a Confinement,
}

/// Runs `spec`'s command in member `member` of `members`.
pub fn start(spec: &run::Spec, member: u32, members: &[Started]) -> Result<Entered, Failed> {
    let refused = |r: Refusal, words: String| Failed::Refused(r, words);
    let m = members.iter().find(|m| m.member == member).ok_or_else(|| {
        refused(
            Refusal::Unknown,
            format!("member {member} did not start in this run"),
        )
    })?;
    widening(spec).map_err(|what| {
        refused(
            Refusal::Widening,
            format!("{}: a command run in it runs as it does, so {what}", m.label),
        )
    })?;
    let argv0 = spec
        .argv
        .first()
        .ok_or_else(|| refused(Refusal::Widening, "no command given".into()))?;
    // A process of the member's command's mount namespace, held before it is checked to be
    // the member's still (hotplug.rs): one that ended since fails setns(2) below.
    let actor = crate::hotplug::member_process(&m.cgroup)
        .map_err(|e| refused(Refusal::Ended, format!("{}: {e}", m.label)))?;
    let c = &m.confinement;
    let env = over(&c.env, &spec.env).map_err(|e| {
        refused(
            Refusal::Widening,
            format!("{}: {:?}: {}", m.label, String::from_utf8_lossy(&e), Unsettable),
        )
    })?;
    let (explicit, candidates) = crate::run::candidates(argv0, &env);
    let nul = |what: &str| refused(Refusal::Widening, format!("{what} holds a NUL byte"));
    let cstrings = |list: &[Vec<u8>], what: &str| {
        list.iter()
            .map(|b| CString::new(b.as_slice()).map_err(|_| nul(what)))
            .collect::<Result<Vec<_>, _>>()
    };
    let candidates = cstrings(&candidates, "PATH")?;
    let args = cstrings(&spec.argv, "an argument")?;
    let mut vars = cstrings(&env, "the environment")?;
    let cwd = if spec.cwd.is_empty() {
        c.workdir.clone()
    } else {
        CString::new(spec.cwd.as_slice()).map_err(|_| nul("the working directory"))?
    };
    let (stdio, ours) = match spec.tty {
        Some(size) => {
            let (master, peer) = terminal(size, c.id).map_err(|e| {
                refused(
                    Refusal::Joining(errno(&e)),
                    format!("{}: its terminal: {e}", m.label),
                )
            })?;
            if let Some(term) = term(&actor, &spec.terms, &env) {
                vars.push(CString::new(term).map_err(|_| nul("TERM"))?);
            }
            (Stdio::Terminal(peer), Ours::Terminal(master))
        }
        None => pipes(spec.stdin, c.id).map_err(|e| {
            refused(
                Refusal::Joining(errno(&e)),
                format!("{}: its stdio: {e}", m.label),
            )
        })?,
    };
    let (err_r, err_w) = pipe().map_err(|e| refused(Refusal::Joining(errno(&e)), format!("a pipe: {e}")))?;
    let prepared = Prepared {
        actor: actor.as_raw_fd(),
        stdio: stdio.fds(),
        tty: spec.tty.is_some(),
        err: err_w.as_raw_fd(),
        cwd,
        explicit,
        argv: args
            .iter()
            .map(|a| a.as_ptr())
            .chain([std::ptr::null()])
            .collect(),
        envp: vars
            .iter()
            .map(|a| a.as_ptr())
            .chain([std::ptr::null()])
            .collect(),
        candidates,
        confinement: c,
    };
    let group = m.lifecycle.group();
    let born = crate::run::born_in(Some(&actor), || {
        let mut args = crate::domains::CloneArgs {
            flags: crate::domains::CLONE_INTO_CGROUP,
            exit_signal: libc::SIGCHLD as u64,
            cgroup: group.as_raw_fd() as u64,
            ..Default::default()
        };
        // SAFETY: clone3(2) as fork(2), with a clone_args of the size given; the child
        // calls only the kernel, on what `prepared` holds, and execs or exits.
        unsafe {
            libc::syscall(
                libc::SYS_clone3,
                &raw mut args,
                std::mem::size_of::<crate::domains::CloneArgs>(),
            )
        }
    });
    let pid = match born {
        Ok(0) => child(&prepared),
        Ok(pid) => pid,
        // The member's command ended since it was held: its pidfd names no process.
        Err(NotBorn::Entering(e)) if e.raw_os_error() == Some(libc::ESRCH) => {
            return Err(refused(Refusal::Ended, format!("{}: it has ended", m.label)));
        }
        Err(NotBorn::Entering(e)) => {
            return Err(refused(
                Refusal::Joining(errno(&e)),
                format!("{}: its PID namespace: {e}", m.label),
            ));
        }
        // Its cgroup's pids.max is reached (cgroup-v2.rst, "PID"; clone(2), EAGAIN).
        Err(NotBorn::Making(e)) if e.raw_os_error() == Some(libc::EAGAIN) => {
            return Err(refused(
                Refusal::Processes,
                format!("{}: it runs as many processes as it may", m.label),
            ));
        }
        // A PID namespace whose first process has ended takes no other (pid_namespaces(7),
        // ENOMEM): the member has ended.
        Err(NotBorn::Making(e)) if e.raw_os_error() == Some(libc::ENOMEM) => {
            return Err(refused(Refusal::Ended, format!("{}: it has ended", m.label)));
        }
        Err(NotBorn::Making(e)) => {
            return Err(refused(
                Refusal::Joining(errno(&e)),
                format!("{}: starting in it: {e}", m.label),
            ));
        }
    };
    drop((err_w, stdio, actor));
    // The error channel closes as the command execs: a report on it means it never ran.
    let mut report = Vec::new();
    let _ = std::fs::File::from(err_r).read_to_end(&mut report);
    if let [which, e0, e1, e2, e3, c0, c1, c2, c3] = report[..] {
        // SAFETY: waitpid(2) of init's own child, which exits after reporting.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        let errno = i32::from_be_bytes([e0, e1, e2, e3]);
        let at = u32::from_be_bytes([c0, c1, c2, c3]);
        return Err(match which {
            step::JOIN | step::TTY => refused(
                Refusal::Joining(errno),
                format!(
                    "{}: {}: {}",
                    m.label,
                    if which == step::JOIN {
                        "entering its namespaces"
                    } else {
                        "making its terminal the session's"
                    },
                    io::Error::from_raw_os_error(errno)
                ),
            ),
            step::CONFINE => {
                let n = u8::try_from(at).unwrap_or(0);
                let what = crate::domains::Confining::numbered(n)
                    .map_or("confining it".into(), |c| c.what().to_string_lossy().into_owned());
                refused(
                    Refusal::Confining(n, errno),
                    format!("{}: {what}: {}", m.label, io::Error::from_raw_os_error(errno)),
                )
            }
            _ => Failed::Step {
                which,
                errno,
                tried: usize::try_from(at)
                    .ok()
                    .and_then(|i| prepared.candidates.get(i))
                    .map_or_else(|| argv0.clone(), |c| c.as_bytes().to_vec()),
            },
        });
    }
    let (stdin, stdout, stderr) = match ours {
        Ours::Terminal(master) => {
            let input = master.try_clone().map_err(|e| {
                // SAFETY: kill(2) and waitpid(2) of init's own child, not yet waited for.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, std::ptr::null_mut(), 0);
                }
                refused(Refusal::Joining(errno(&e)), format!("its terminal: {e}"))
            })?;
            (Some(input), Some(master), None)
        }
        Ours::Pipes {
            stdin,
            stdout,
            stderr,
        } => (stdin, Some(stdout), Some(stderr)),
    };
    Ok(Entered {
        pid,
        stdin,
        stdout,
        stderr,
        tty: spec.tty.is_some(),
    })
}

/// Why a variable of an exec's is not laid over its member's: it holds a NUL, or names no
/// variable, or names SHARDS_DOMAIN, which init sets, the member's own name for its domain.
struct Unsettable;

impl std::fmt::Display for Unsettable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no NAME=value or NAME without NUL, or SHARDS_DOMAIN, which init sets")
    }
}

/// The member's variables `own` with an exec's `given` over them, in order, as moby lays an
/// exec's over its container's (daemon/container/env.go, ReplaceOrAppendEnvValues): each
/// replaces the variable of its name, a name alone unsets it, and the rest are appended. The
/// first refused, as [`Unsettable`].
fn over(own: &[CString], given: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, Vec<u8>> {
    let mut env: Vec<Vec<u8>> = own.iter().map(|e| e.as_bytes().to_vec()).collect();
    let key = |e: &[u8]| -> Vec<u8> { e.split(|&b| b == b'=').next().unwrap_or_default().to_vec() };
    for e in given {
        let name = key(e);
        if name.is_empty() || e.contains(&0) || name == run::members::DOMAIN_VARIABLE {
            return Err(e.clone());
        }
        env.retain(|have| key(have) != name);
        if e.contains(&b'=') {
            env.push(e.clone());
        }
    }
    Ok(env)
}

/// What `spec` asks that a member's command has not, by name: a user, capabilities, or
/// any other setup entry than the one naming the member; none for none.
fn widening(spec: &run::Spec) -> Result<(), &'static str> {
    if !spec.user.is_empty() {
        return Err("it takes no user of its own");
    }
    for e in &spec.setup {
        if e.starts_with(b"caps=") {
            return Err("it takes no capabilities");
        }
        if !e.starts_with(run::members::EXEC_KEY) {
            return Err("it takes no setup of its own");
        }
    }
    Ok(())
}

/// The child's stdio: the terminal's peer, or its ends of three pipes (stdin perhaps
/// `/dev/null`).
enum Stdio {
    Terminal(OwnedFd),
    Pipes([OwnedFd; 3]),
}

impl Stdio {
    fn fds(&self) -> [RawFd; 3] {
        match self {
            Stdio::Terminal(peer) => [peer.as_raw_fd(); 3],
            Stdio::Pipes(fds) => fds.each_ref().map(AsRawFd::as_raw_fd),
        }
    }
}

/// Init's ends of the child's stdio.
enum Ours {
    Terminal(OwnedFd),
    Pipes {
        stdin: Option<OwnedFd>,
        stdout: OwnedFd,
        stderr: OwnedFd,
    },
}

/// A terminal of `size` (none for a size of 0), from init's devpts: its master, and its
/// peer, opened through the master (TIOCGPTPEER, Linux 4.13) so that no path is looked up
/// in a mount namespace other than the master's, and owned by uid `id`, as runc gives a
/// command its stdio (libcontainer/init_linux.go, fixStdioPermissions).
fn terminal(size: Size, id: u32) -> io::Result<(OwnedFd, OwnedFd)> {
    // SAFETY: posix_openpt(3) returns a new descriptor, or -1.
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let master = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: unlockpt(3) of our master.
    if unsafe { libc::unlockpt(fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if size.rows != 0 && size.cols != 0 {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads one winsize.
        if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: TIOCGPTPEER opens the master's peer with these flags, a new descriptor.
    let peer = unsafe {
        libc::ioctl(
            fd,
            libc::TIOCGPTPEER,
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if peer < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor the ioctl just made, owned here alone.
    let peer = unsafe { OwnedFd::from_raw_fd(peer) };
    // SAFETY: fchown(2) of our peer; gid -1 keeps devpts's.
    if unsafe { libc::fchown(peer.as_raw_fd(), id, u32::MAX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, peer))
}

/// Pipes for the child's stdio, its ends owned by uid `id` as [`terminal`]'s peer is: its
/// stdin a pipe where it reads its client's (`-i`), else `/dev/null`, as Docker gives an
/// exec's (D160).
fn pipes(stdin: bool, id: u32) -> io::Result<(Stdio, Ours)> {
    let (out_r, out_w) = pipe()?;
    let (err_r, err_w) = pipe()?;
    let (input, ours_in) = if stdin {
        let (r, w) = pipe()?;
        own(&r, id)?;
        (r, Some(w))
    } else {
        // The microVM's own node, never given to the member.
        (OwnedFd::from(std::fs::File::open("/dev/null")?), None)
    };
    own(&out_w, id)?;
    own(&err_w, id)?;
    Ok((
        Stdio::Pipes([input, out_w, err_w]),
        Ours::Pipes {
            stdin: ours_in,
            stdout: out_r,
            stderr: err_r,
        },
    ))
}

/// The pipe `end` belongs to given to uid `id`, so that the command, as that uid, can open
/// its stdio again by path (`/dev/stdout`, through /proc): a pipe's two ends are one inode,
/// made init's.
fn own(end: &OwnedFd, id: u32) -> io::Result<()> {
    // SAFETY: fchown(2) of our own pipe's end; gid -1 leaves the group.
    if unsafe { libc::fchown(end.as_raw_fd(), id, u32::MAX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2(2) writes both descriptors into this array, owned below.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let [r, w] = fds;
    // SAFETY: pipe2 just made both descriptors, which nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) })
}

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// The TERM the command is told, where its environment names none: the first of `terms`
/// its member's root describes (terms.rs), that root seen through the member's command's
/// process (`/proc/PID/root`, proc(5)).
fn term(actor: &OwnedFd, terms: &[Vec<u8>], env: &[Vec<u8>]) -> Option<Vec<u8>> {
    if terms.is_empty() || env.iter().any(|e| e.starts_with(b"TERM=")) {
        return None;
    }
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", actor.as_raw_fd())).ok()?;
    let pid: u32 = info
        .lines()
        .find_map(|l| l.strip_prefix("Pid:"))
        .and_then(|p| p.trim().parse().ok())?;
    crate::terms::term(std::path::Path::new(&format!("/proc/{pid}/root")), terms, env)
}

/// The child: in the member's PID namespace and cgroup, it enters the member's command's
/// other namespaces, takes its stdio and session, and runs its command confined as that
/// command is.
fn child(p: &Prepared<'_>) -> ! {
    // SAFETY: system calls on what `p` holds, in a single-threaded child that execs or
    // exits; `report` exits.
    unsafe {
        let fail = |which: u8, at: usize| crate::run::report(p.err, which, at);
        let all = libc::CLONE_NEWNS
            | libc::CLONE_NEWNET
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWCGROUP;
        if libc::setns(p.actor, all) != 0 {
            fail(step::JOIN, 0);
        }
        libc::setsid();
        if p.tty && libc::ioctl(p.stdio[0], libc::TIOCSCTTY, 0) != 0 {
            fail(step::TTY, 0);
        }
        for (i, &fd) in p.stdio.iter().enumerate() {
            if libc::dup2(fd, i as libc::c_int) < 0 {
                fail(step::EXEC, 0);
            }
        }
        // Its error channel next above its stdio, then no other descriptor of init's.
        let err = libc::STDERR_FILENO + 1;
        let high = libc::fcntl(p.err, libc::F_DUPFD_CLOEXEC, err + 1);
        if high < 0
            || libc::dup3(high, err, libc::O_CLOEXEC) < 0
            || libc::syscall(libc::SYS_close_range, (err + 1) as u32, u32::MAX, 0u32) != 0
        {
            fail(step::EXEC, 0);
        }
        let fail = |which: u8, at: usize| crate::run::report(err, which, at);
        if let Err(failed) = crate::domains::confine(p.confinement) {
            fail(step::CONFINE, usize::from(failed as u8));
        }
        // As the member: where it may not go, neither may its command.
        if libc::chdir(p.cwd.as_ptr()) != 0 {
            fail(step::CHDIR, 0);
        }
        for (i, path) in p.candidates.iter().enumerate() {
            // Go's findExecutable: a file that is not a directory, executable by us.
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(path.as_ptr(), &mut st) != 0 {
                if p.explicit {
                    fail(step::STAT, i);
                }
                continue;
            }
            if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                if p.explicit {
                    *libc::__errno_location() = libc::EISDIR;
                    fail(step::ACCESS, i);
                }
                continue;
            }
            if libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) != 0 {
                if p.explicit {
                    fail(step::ACCESS, i);
                }
                continue;
            }
            libc::execve(path.as_ptr(), p.argv.as_ptr(), p.envp.as_ptr());
            fail(step::EXEC, i);
        }
        fail(step::NOT_IN_PATH, 0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn own() -> Vec<CString> {
        [
            "PATH=/agents/main:/usr/bin",
            "HOME=/tmp",
            "SHARDS_DOMAIN=agent main",
            "TOKEN=a=b",
        ]
        .iter()
        .map(|e| CString::new(*e).unwrap())
        .collect()
    }

    /// An exec's variables replace the member's of their names, a name alone unsets one,
    /// the rest are appended, in order: the last of a name given twice holds.
    #[test]
    fn an_execs_variables_lie_over_its_members() {
        let given: Vec<Vec<u8>> = ["HOME=/work", "TOKEN", "NEW=1", "NEW=2", "GONE"]
            .iter()
            .map(|e| e.as_bytes().to_vec())
            .collect();
        let got = over(&own(), &given).unwrap();
        let got: Vec<&str> = got.iter().map(|e| std::str::from_utf8(e).unwrap()).collect();
        assert_eq!(
            got,
            [
                "PATH=/agents/main:/usr/bin",
                "SHARDS_DOMAIN=agent main",
                "HOME=/work",
                "NEW=2"
            ]
        );
    }

    /// No exec renames its member's domain, nor gives a variable that names none or holds
    /// a NUL: refused whole, by the entry.
    #[test]
    fn an_exec_cannot_set_what_init_sets_or_nothing() {
        for bad in [
            &b"SHARDS_DOMAIN=agent other"[..],
            b"SHARDS_DOMAIN",
            b"=x",
            b"",
            b"A=\0",
        ] {
            assert_eq!(over(&own(), &[bad.to_vec()]), Err(bad.to_vec()), "{bad:?}");
        }
    }

    /// A stdio end is given to the member's uid, or init is told why not: fchown(2) to
    /// another uid is refused to all but root (EPERM), and `own` says so.
    #[test]
    fn a_stdio_end_given_to_another_uid_says_whether_it_was() {
        let (end, _other_end) = pipe().unwrap();
        // SAFETY: geteuid(2) reads this process's effective uid.
        let euid = unsafe { libc::geteuid() };
        let owned = super::own(&end, euid.wrapping_add(1));
        if euid == 0 {
            assert!(owned.is_ok(), "{owned:?}");
        } else {
            assert_eq!(owned.map_err(|e| e.raw_os_error()), Err(Some(libc::EPERM)));
        }
    }

    /// A command's terminal is a pty of the size asked, its peer a terminal given to the
    /// member's uid, or init is told why not, as a stdio end is.
    #[test]
    fn a_terminal_is_of_its_size_and_its_peer_the_members() {
        // SAFETY: geteuid(2) reads this process's effective uid.
        let euid = unsafe { libc::geteuid() };
        let (master, peer) = terminal(Size { rows: 24, cols: 80 }, euid).unwrap();
        // SAFETY: a winsize of zeroes, which TIOCGWINSZ fills.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ writes one winsize; isatty(3) of our own descriptor.
        unsafe {
            assert_eq!(libc::ioctl(master.as_raw_fd(), libc::TIOCGWINSZ, &raw mut ws), 0);
            assert_eq!(libc::isatty(peer.as_raw_fd()), 1);
        }
        assert_eq!((ws.ws_row, ws.ws_col), (24, 80));
        let other = terminal(Size::default(), euid.wrapping_add(1)).map(drop);
        if euid == 0 {
            assert!(other.is_ok(), "{other:?}");
        } else {
            assert_eq!(other.map_err(|e| e.raw_os_error()), Err(Some(libc::EPERM)));
        }
    }

    /// A member's command takes no user, capabilities or setup of its own: the entry naming
    /// its member is all it may carry.
    #[test]
    fn a_member_command_takes_nothing_its_member_has_not() {
        let spec = |user: &[u8], setup: &[&[u8]]| run::Spec {
            argv: vec![b"sh".to_vec()],
            user: user.to_vec(),
            setup: setup.iter().map(|e| e.to_vec()).collect(),
            ..run::Spec::default()
        };
        let named = run::members::exec_in(1);
        assert_eq!(widening(&spec(b"", &[&named])), Ok(()));
        assert!(widening(&spec(b"0", &[&named])).is_err());
        assert!(widening(&spec(b"", &[&named, b"caps=1"])).is_err());
        assert!(widening(&spec(b"", &[&named, b"in-join=3"])).is_err());
        assert!(widening(&spec(b"", &[&named, b"tmpfs=/x\0"])).is_err());
    }
}

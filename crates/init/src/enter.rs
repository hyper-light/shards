//! A command run in one of the run's members (the harness plan's step 6, G24; D243): born
//! of its member's starter ([`crate::starter`], confine-10), a process of the member's own,
//! so that it is everything a process of the member is: its PID namespace, cgroup (whose
//! `pids.max` holds it as it is born), namespaces, uid and gid and no other group, no
//! capability, `no_new_privs`, its seccomp filter and its very Landlock domain, which a
//! ruleset entered afresh never is (landlock(7): a domain is inherited, never joined), so
//! that it signals, and reaches the abstract Unix sockets of, the member's own processes as
//! they do one another's.
//!
//! Its environment is the member's as it started, the exec's variables over it; its working
//! directory the exec's, else the member's; its program found on that environment's `PATH`
//! as the member's own was, in the member's view. It holds no descriptor of init's but its
//! stdio: pipes init relays as an exec's, or a terminal whose master init holds. The
//! terminal is init's own devpts', handed over as a descriptor (TIOCGPTPEER): a member's
//! `/dev` holds no `pts`.
//!
//! Init asks the starter and waits for none of it: its answer, its exec and its end come in
//! init's poll loop (run.rs), as a process of the member's may stop the starter or the
//! command before it execs, as it may any of its own.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use shards_abi::run::members::Refusal;
use shards_abi::run::{self, Size};

use crate::domains::Started;
use crate::run::step;

/// A command asked of its member's starter: what init holds of it until it execs.
#[derive(Debug)]
pub struct Asked {
    /// The member it runs in.
    pub member: u32,
    /// Its member's name, for its refusals' words.
    label: String,
    /// Init's ends of its stdio.
    ours: Ours,
    /// Its error channel's read end, which closes as it execs ([`run::report`]'s words
    /// where it does not).
    pub err: OwnedFd,
    /// Where its program was looked for, for a failure's words.
    tried: Vec<Vec<u8>>,
    /// Its program as asked, and its working directory, for a failure's words.
    pub argv0: Vec<u8>,
    pub cwd: Vec<u8>,
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

/// Asks the starter of member `member` of `members` for `spec`'s command, exec `id`: what
/// init holds of it until its starter answers and it execs ([`Asked::born`],
/// [`Asked::launched`]).
pub fn start(spec: &run::Spec, member: u32, members: &[Started], id: u32) -> Result<Asked, Failed> {
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
    // A process of the member's command, held before it is checked to be the member's still
    // (hotplug.rs): its root, for the terminal types it describes.
    let actor = crate::hotplug::member_process(&m.cgroup)
        .map_err(|e| refused(Refusal::Ended, format!("{}: {e}", m.label)))?;
    let starter = m.starter.as_ref().ok_or_else(|| gone(&m.label))?;
    let c = &m.confinement;
    let env = over(&c.env, &spec.env).map_err(|e| refused(Refusal::Widening, format!("{}: {e}", m.label)))?;
    let (explicit, candidates) = crate::run::candidates(argv0, &env);
    let nul = |what: &str| refused(Refusal::Widening, format!("{what} holds a NUL byte"));
    let cstrings = |list: &[Vec<u8>], what: &str| {
        list.iter()
            .map(|b| CString::new(b.as_slice()).map_err(|_| nul(what)))
            .collect::<Result<Vec<_>, _>>()
    };
    let paths = cstrings(&candidates, "PATH")?;
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
    let asked = crate::starter::Command {
        id,
        candidates: &paths,
        explicit,
        argv: &args,
        envp: &vars,
        cwd: &cwd,
        stdio: stdio.fds(),
        err: err_w.as_raw_fd(),
        tty: spec.tty.is_some(),
    };
    starter.start(&asked).map_err(|e| match e {
        crate::starter::NotBorn::Gone => gone(&m.label),
        crate::starter::NotBorn::Spec(e) => refused(
            Refusal::Joining(errno(&e)),
            format!("{}: its command's spec: {e}", m.label),
        ),
    })?;
    // Its ends are the starter's now: the error channel closes as the command execs.
    drop((err_w, stdio, actor));
    Ok(Asked {
        member,
        label: m.label.clone(),
        ours,
        err: err_r,
        tried: candidates,
        argv0: argv0.clone(),
        cwd: spec.cwd.clone(),
    })
}

/// Its starter has gone, or never was: a process of the member's ended it, or its fork was
/// refused as the member started.
fn gone(label: &str) -> Failed {
    Failed::Refused(
        Refusal::NoStarter,
        format!("{label}: its starter, which starts what is run in it, has gone"),
    )
}

impl Asked {
    /// Its starter forked none: `errno`, its fork's, at its member's `pids.max` (clone(2)
    /// EAGAIN, cgroup-v2.rst "PID"), or in a PID namespace whose first process has ended
    /// (pid_namespaces(7), ENOMEM).
    pub fn not_born(&self, errno: i32) -> Failed {
        match errno {
            libc::EAGAIN => Failed::Refused(
                Refusal::Processes,
                format!("{}: it runs as many processes as it may", self.label),
            ),
            libc::ENOMEM => Failed::Refused(Refusal::Ended, format!("{}: it has ended", self.label)),
            _ => Failed::Refused(
                Refusal::Joining(errno),
                format!(
                    "{}: starting in it: {}",
                    self.label,
                    io::Error::from_raw_os_error(errno)
                ),
            ),
        }
    }

    /// Its starter went before it answered.
    pub fn gone(&self) -> Failed {
        gone(&self.label)
    }

    /// What its error channel said, `report`, once it closed: nothing as it execs, else why
    /// it did not ([`run::report`]'s step, errno and candidate).
    pub fn launched(&self, report: &[u8]) -> Result<(), Failed> {
        let [which, e0, e1, e2, e3, c0, c1, c2, c3] = report[..] else {
            return Ok(());
        };
        let errno = i32::from_be_bytes([e0, e1, e2, e3]);
        let at = u32::from_be_bytes([c0, c1, c2, c3]);
        Err(match which {
            step::TTY => Failed::Refused(
                Refusal::Joining(errno),
                format!(
                    "{}: making its terminal the session's: {}",
                    self.label,
                    io::Error::from_raw_os_error(errno)
                ),
            ),
            _ => Failed::Step {
                which,
                errno,
                tried: usize::try_from(at)
                    .ok()
                    .and_then(|i| self.tried.get(i))
                    .map_or_else(|| self.argv0.clone(), Clone::clone),
            },
        })
    }

    /// Init's ends of its stdio, for an exec that runs: its stdin, stdout and stderr; with a
    /// terminal, its master as stdout and a duplicate of it as stdin, and no stderr.
    pub fn stdio(self) -> io::Result<(Option<OwnedFd>, Option<OwnedFd>, Option<OwnedFd>)> {
        match self.ours {
            Ours::Terminal(master) => Ok((Some(master.try_clone()?), Some(master), None)),
            Ours::Pipes {
                stdin,
                stdout,
                stderr,
            } => Ok((stdin, Some(stdout), Some(stderr))),
        }
    }
}

/// Why a variable of an exec's is not laid over its member's: it holds a NUL, or names no
/// variable, or names SHARDS_DOMAIN, which init sets, the member's own name for its domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unsettable {
    EmptyName,
    Nul,
    Reserved,
}

impl std::fmt::Display for Unsettable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EmptyName => "an environment variable names no key",
            Self::Nul => "an environment variable holds a NUL",
            Self::Reserved => "SHARDS_DOMAIN, which init sets",
        })
    }
}

/// The member's variables `own` with an exec's `given` over them, in order, as moby lays an
/// exec's over its container's (daemon/container/env.go, ReplaceOrAppendEnvValues): each
/// replaces the variable of its name, a name alone unsets it, and the rest are appended. The
/// first refused, as [`Unsettable`].
fn over(own: &[CString], given: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, Unsettable> {
    let mut env: Vec<Vec<u8>> = own.iter().map(|e| e.as_bytes().to_vec()).collect();
    let key = |e: &[u8]| -> Vec<u8> { e.split(|&b| b == b'=').next().unwrap_or_default().to_vec() };
    for e in given {
        let name = key(e);
        if name.is_empty() {
            return Err(Unsettable::EmptyName);
        }
        if e.contains(&0) {
            return Err(Unsettable::Nul);
        }
        if name == run::members::DOMAIN_VARIABLE {
            return Err(Unsettable::Reserved);
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
#[derive(Debug)]
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
pub(crate) fn terminal(size: Size, id: u32) -> io::Result<(OwnedFd, OwnedFd)> {
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
        for (bad, reason) in [
            (&b"SHARDS_DOMAIN=agent other"[..], Unsettable::Reserved),
            (b"SHARDS_DOMAIN", Unsettable::Reserved),
            (b"=x", Unsettable::EmptyName),
            (b"", Unsettable::EmptyName),
            (b"A=\0", Unsettable::Nul),
        ] {
            assert_eq!(over(&own(), &[bad.to_vec()]), Err(reason));
        }
    }

    #[test]
    fn rejected_environment_values_never_enter_diagnostics() {
        let sentinel = "public-credential-sentinel";
        for bad in [
            format!("API_KEY={sentinel}\0suffix"),
            format!("={sentinel}"),
            format!("SHARDS_DOMAIN={sentinel}"),
        ] {
            let error = over(&own(), &[bad.into_bytes()]).unwrap_err();
            assert!(!error.to_string().contains(sentinel));
            assert!(!format!("{error:?}").contains(sentinel));
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

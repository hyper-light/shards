//! The daemon's listener for other machines' clients (D180): the clients this home enrolled
//! run shards commands here whole, each as this machine's own shards, started by the spawner
//! as every child of the daemon is (PM M158), and each said in the daemon's log; a client not
//! enrolled may only join, by an invitation this home made.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;

use shards_remote::frame::Request;
use shards_remote::server::{self, Host, Running};
use shards_remote::{Error, identity, invite};

/// This machine, as its daemon's clients reach it.
pub(super) struct Machine {
    pub home: PathBuf,
    pub exe: PathBuf,
}

/// Why a client may not run `args` here: a context's own commands, the daemon's, and what the
/// shards binary runs only as a part of itself, are the machine's owner's.
fn owners(args: &[String]) -> Option<String> {
    let said = shards_cmdline::grammar::rewrite(args).unwrap_or_else(|| args.to_vec());
    let first = said.first().map(String::as_str).unwrap_or_default();
    matches!(
        first,
        "context"
            | "client"
            | "invitation"
            | "daemon"
            | "spawner"
            | "grants"
            | "share"
            | "frontend"
            | "guest"
    )
    .then(|| format!("{first}: this machine's owner's to run, not a client's"))
}

impl Host for Machine {
    fn enrolled(&self, spki: &[u8]) -> Option<String> {
        crate::context::clients(&self.home)
            .into_iter()
            .find(|c| c.key == spki)
            .map(|c| c.name)
    }

    fn join(&self, name: &str, proof: &[u8], exported: &[u8; 32], spki: &[u8]) -> Result<(), Error> {
        let refused = || Error::new(server::NOT_INVITED);
        let (invitation, created) = crate::context::invitation(&self.home, name).ok_or_else(refused)?;
        if crate::context::lapsed(created) || !invite::verify(&invitation.secret, exported, proof) {
            return Err(refused());
        }
        if self.enrolled(spki).is_some() {
            return Err(refused());
        }
        // Redeemed first: of two joins at once, one enrolls.
        crate::context::redeem(&self.home, name).map_err(|_| refused())?;
        crate::context::enroll(&self.home, name, spki).map_err(|_| refused())?;
        let key = identity::hex(&identity::fingerprint(spki));
        super::log(format!(
            "client {name} joined, its key {}",
            key.get(..12).unwrap_or_default()
        ));
        Ok(())
    }

    fn start(&self, client: &str, request: Request) -> Result<Running, Error> {
        // A client removed since it connected runs nothing more.
        if !crate::context::clients(&self.home)
            .iter()
            .any(|c| c.name == client)
        {
            return Err(Error::new(server::NOT_ENROLLED));
        }
        let args = request
            .args
            .into_iter()
            .map(|a| String::from_utf8(a).map_err(|_| Error::new("an argument that is not UTF-8")))
            .collect::<Result<Vec<String>, Error>>()?;
        if let Some(why) = owners(&args) {
            return Err(Error::new(why));
        }
        super::log(format!("client {client}: shards {}", args.join(" ")));
        let pipe = || std::io::pipe().map_err(|e| Error::new(format!("a pipe for the command: {e}")));
        let tty = |e: std::io::Error| Error::new(format!("the command's terminal: {e}"));
        // The client's terminal: the command's stdin, stdout and stderr that the client's
        // terminal is are a terminal of its own here, as sshd gives a session's command one
        // (OpenSSH session.c, do_exec_pty); the rest are pipes.
        let terminal = request.terminal;
        let pty = match terminal {
            Some(t) => Some(open_pty(t.rows, t.cols).map_err(tty)?),
            None => None,
        };
        let on = |fd: usize| terminal.is_some_and(|t| [t.stdin, t.stdout, t.stderr].get(fd) == Some(&true));
        let (child_in, stdin): (OwnedFd, Box<dyn Write + Send>) = match &pty {
            Some((controller, peer)) if on(0) => (
                peer.try_clone().map_err(tty)?,
                Box::new(File::from(controller.try_clone().map_err(tty)?)),
            ),
            _ => {
                let (r, w) = pipe()?;
                (r.into(), Box::new(w))
            }
        };
        let (child_out, stdout): (OwnedFd, Box<dyn Read + Send>) = match &pty {
            Some((controller, peer)) if on(1) => (
                peer.try_clone().map_err(tty)?,
                Box::new(File::from(controller.try_clone().map_err(tty)?)),
            ),
            _ => {
                let (r, w) = pipe()?;
                (w.into(), Box::new(r))
            }
        };
        let (child_err, stderr): (OwnedFd, Box<dyn Read + Send>) = match &pty {
            // What the terminal shows is read once: as stdout's, where stdout is on it too.
            Some((_, peer)) if on(2) && on(1) => (peer.try_clone().map_err(tty)?, Box::new(std::io::empty())),
            Some((controller, peer)) if on(2) => (
                peer.try_clone().map_err(tty)?,
                Box::new(File::from(controller.try_clone().map_err(tty)?)),
            ),
            _ => {
                let (r, w) = pipe()?;
                (w.into(), Box::new(r))
            }
        };
        let env: Vec<(String, OsString)> = request
            .env
            .into_iter()
            .filter_map(|(k, v)| String::from_utf8(v).ok().map(|v| (k, OsString::from(v))))
            .collect();
        // This machine's own shards, whatever context its owner's commands use, and of the
        // daemon's environment only what shards is set by ([`kept`]).
        let daemons: Vec<(String, OsString)> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v)))
            .filter(|(k, _)| kept(k))
            .collect();
        let mut set: Vec<(&str, &OsStr)> = daemons.iter().map(|(k, v)| (k.as_str(), v.as_os_str())).collect();
        set.extend([
            ("SHARDS_HOME", self.home.as_os_str()),
            ("SHARDS_CONTEXT", OsStr::new(crate::context::DEFAULT)),
        ]);
        set.extend(env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())));
        if pty.is_some() {
            set.push((crate::context::TAKE_TERMINAL, OsStr::new("1")));
        }
        let argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let fds = [
            (child_in.as_fd(), 0),
            (child_out.as_fd(), 1),
            (child_err.as_fd(), 2),
        ];
        // A session of its own, out of reach of what a terminal of the daemon's sends its
        // group, whose controlling terminal is its own where it has one.
        let child = shards_ipc::spawn_in(&self.exe, &argv, &fds, true, &set)
            .map_err(|e| Error::new(format!("starting the command: {e}")))?;
        // The child holds its ends, and the terminal's peer: once it has gone, reading the
        // controller ends.
        drop((child_in, child_out, child_err));
        let controller = pty.map(|(controller, _peer)| controller);
        // Waited for on one thread, hung up and signalled from others: kill refuses a child
        // reaped.
        let child = Arc::new(child);
        let (hung, signalled) = (Arc::clone(&child), Arc::clone(&child));
        let resize = controller.map(|controller| -> Box<dyn Fn(u16, u16) + Send> {
            Box::new(move |rows, cols| {
                let ws = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                // SAFETY: TIOCSWINSZ reads one winsize, on the terminal's controller, ours.
                // The kernel signals the terminal's foreground, the command's group
                // (SIGWINCH).
                unsafe { libc::ioctl(controller.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
            })
        });
        Ok(Running {
            stdin,
            stdout,
            stderr,
            wait: Box::new(move || child.wait().unwrap_or(1)),
            hangup: Box::new(move || {
                let _ = hung.kill(libc::SIGHUP);
            }),
            resize,
            // What the client received, as this system numbers it; one it has not goes
            // nowhere.
            signal: Box::new(move |linux| {
                if let Some(sig) = shards_ipc::local_signal(linux) {
                    let _ = signalled.kill(sig);
                }
            }),
        })
    }

    fn invited(&self) -> usize {
        crate::context::open_invitations(&self.home)
    }
}

/// Whether the daemon's variable `name` goes to a client's command: shards' own settings of
/// this machine, and where its programs, user and temporary files are, and its network's
/// proxies, as dockerd has its own (D180). No credential of its owner's: none of `HOME`,
/// `DOCKER_CONFIG` or `XDG_CONFIG_HOME`, under which a registry's login, a cloud's
/// credentials and SSH keys are kept, and with no config of its own a pull asks its
/// registry with what its client sent alone (shards_registry::credentials, lookup); nor the
/// settings that are the client's, which its request carries (frame::ENV), nor shards'
/// tests' credentials.
fn kept(name: &str) -> bool {
    if shards_remote::frame::ENV.contains(&name) {
        return false;
    }
    matches!(
        name,
        "PATH"
            | "USER"
            | "LOGNAME"
            | "TMPDIR"
            | "XDG_CACHE_HOME"
            | "HTTP_PROXY"
            | "http_proxy"
            | "HTTPS_PROXY"
            | "https_proxy"
            | "NO_PROXY"
            | "no_proxy"
    ) || (name.starts_with("SHARDS_") && !name.starts_with("SHARDS_TEST_"))
}

/// A pseudo-terminal of `rows` by `cols`: its controller, and the peer a command's stdio is,
/// both close-on-exec. openpty(3) opens them without; no child takes them meanwhile, as a
/// daemon's children are its spawner's (PM M158), which holds only what it is sent.
fn open_pty(rows: u16, cols: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    let (mut controller, mut peer) = (-1, -1);
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty(3) fills both descriptors and reads `ws`.
    let opened = unsafe {
        libc::openpty(
            &mut controller,
            &mut peer,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            (&raw const ws).cast_mut(),
        )
    };
    if opened != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the two descriptors openpty(3) just made, which nothing else owns.
    let (controller, peer) = unsafe { (OwnedFd::from_raw_fd(controller), OwnedFd::from_raw_fd(peer)) };
    for fd in [&controller, &peer] {
        // SAFETY: fcntl(2) on our own descriptor.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((controller, peer))
}

/// Serves other machines' clients on `at`, on a thread of its own for the daemon's life: once
/// it is bound, its address is in the home (`listening`), where `shards invite client` finds
/// it, and in the daemon's log. An address it cannot bind is an error, before the daemon
/// says it serves.
pub(super) fn listen(
    home: PathBuf,
    exe: PathBuf,
    at: std::net::SocketAddr,
) -> Result<std::net::SocketAddr, String> {
    let identity = crate::context::identity(&home)?;
    let (tx, rx) = std::sync::mpsc::channel::<Result<std::net::SocketAddr, String>>();
    let ended = tx.clone();
    let machine = Machine {
        home: home.clone(),
        exe,
    };
    std::thread::Builder::new()
        .name("remote".into())
        .spawn(move || {
            let served = server::serve(&identity, at, machine, |bound, _stopper| {
                // The daemon ends with its process, the listener with it: it is not stopped.
                let _ = tx.send(Ok(bound));
            });
            if let Err(e) = served {
                super::log(format!("other machines' clients on {at}: {e}"));
                let _ = ended.send(Err(e.to_string()));
            }
        })
        .map_err(|e| format!("a thread for other machines' clients: {e}"))?;
    let bound = rx
        .recv()
        .map_err(|_| format!("listening on {at}: the listener ended before it bound"))?
        .map_err(|e| format!("listening on {at}: {e}"))?;
    let path = home.join(crate::context::LISTENING);
    std::fs::write(&path, format!("{bound}\n")).map_err(|e| format!("{}: {e}", path.display()))?;
    let key = identity::hex(&crate::context::identity(&home)?.fingerprint());
    super::log(format!(
        "serving the machines it enrolled on {bound} (QUIC), its key {}",
        key.get(..12).unwrap_or_default()
    ));
    Ok(bound)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A client's command takes shards' settings of the daemon's environment, and what it
    /// needs to act as this machine's shards; none of the owner's other variables, nor its
    /// tests' credentials.
    #[test]
    fn a_clients_command_takes_shards_settings_alone() {
        for name in ["PATH", "TMPDIR", "HTTPS_PROXY", "SHARDS_KERNEL", "SHARDS_LOG"] {
            assert!(kept(name), "{name}");
        }
        for name in [
            "HOME",
            "DOCKER_CONFIG",
            "DOCKER_AUTH_CONFIG",
            "SHARDS_AUTH_CONFIG",
            "XDG_CONFIG_HOME",
            "AWS_SECRET_ACCESS_KEY",
            "COSIGN_PASSWORD",
            "SSH_AUTH_SOCK",
            "SHARDS_TEST_S3_SECRET",
            "SHARDS_DEFAULT_PLATFORM",
            "LD_PRELOAD",
        ] {
            assert!(!kept(name), "{name}");
        }
    }

    /// A client runs shards' commands, not the machine's owner's.
    #[test]
    fn a_client_may_not_run_its_hosts_own_commands() {
        let args = |s: &str| s.split(' ').map(str::to_string).collect::<Vec<_>>();
        assert!(owners(&args("ps -a")).is_none());
        assert!(owners(&args("run --rm alpine echo hi")).is_none());
        for owner in [
            "context ls",
            "list contexts",
            "invite client --name x --output y",
            "list clients",
            "stop daemon",
            "daemon stop",
            "configure guest --kernel k --init i",
            "spawner",
        ] {
            assert!(owners(&args(owner)).is_some(), "{owner}");
        }
    }
}

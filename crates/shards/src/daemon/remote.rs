//! The daemon's listener for other machines' clients (D180): the clients this home enrolled
//! run shards commands here whole, each as this machine's own shards, started by the spawner
//! as every child of the daemon is (PM M158), and each said in the daemon's log; a client not
//! enrolled may only join, by an invitation this home made.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::PathBuf;
use std::sync::Arc;

use shards_ipc::fd::{AsFd, OwnedFd};

use shards_remote::frame::Request;
use shards_remote::server::{self, Host, Running};
use shards_remote::{Error, identity, invite};

/// This machine, as its daemon's clients reach it.
pub(super) struct Machine {
    pub home: PathBuf,
    pub exe: PathBuf,
    /// The connections builds' steps made to the SSH agents their clients name, by client and
    /// token, until the client claims each (`agent_step`).
    pub agents: Arc<std::sync::Mutex<Agents>>,
}

/// The connections to agents a client may claim, by client and token ([`Machine::agents`]).
type Agents = std::collections::HashMap<(String, String), shards_ipc::Stream>;

/// `n` random bytes, as hex.
fn token(n: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; n];
    shards_net::entropy(&mut bytes).map_err(|e| format!("a token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
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
        // A client this machine enrolled already, whose context was made again: the
        // invitation is redeemed, that no other key takes it, and the client joins as it was
        // enrolled, under the name it has, its key's one.
        if let Some(enrolled) = self.enrolled(spki) {
            crate::context::redeem(&self.home, name).map_err(|_| refused())?;
            super::log(format!("client {enrolled} joined again, by invitation {name}"));
            return Ok(());
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
        if args.first().map(String::as_str) == Some(super::COPY_STEP) {
            let shown: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
            super::log(format!("client {client}: copy step {}", shown.join(" ")));
            return self.copy_step(args);
        }
        if args.first().map(String::as_str) == Some(crate::context::ENGINE_STEP) {
            return self.engine_step();
        }
        if args.first().map(String::as_str) == Some(crate::synced::SYNC_STEP) {
            let shown: Vec<String> = args.iter().skip(1).map(|a| a.replace('\0', "")).collect();
            super::log(format!("client {client}: context step {}", shown.join(" ")));
            return self.sync_step(client, args, request.env, request.terminal);
        }
        if args.first().map(String::as_str) == Some(crate::agent::SIGN_STEP) {
            let shown: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
            super::log(format!("client {client}: signature step {}", shown.join(" ")));
            return self.sign_step(args);
        }
        // A run's or create's `--cidfile` of the client's (`context::CIDFILE`), the client's
        // file, which the run records: the command writes the ID to a pipe instead
        // (`cli::request::CidFile`), which goes to the client as the ID of what it made.
        let mut tells = false;
        let args: Vec<String> = args
            .into_iter()
            .map(|a| match a.split_once(crate::context::CIDFILE) {
                Some((flag, path)) => {
                    tells = true;
                    format!("{flag}{path}")
                }
                None => a,
            })
            .collect();
        super::log(format!("client {client}: shards {}", args.join(" ")));
        let pipe = || std::io::pipe().map_err(|e| Error::new(format!("a pipe for the command: {e}")));
        let (made, made_w) = match tells {
            true => {
                let (r, w) = pipe()?;
                (Some(r), Some(w))
            }
            false => (None, None),
        };
        let env = self.command_env(request.env);
        command(&self.exe, &args, &env, request.terminal, made, made_w)
    }

    fn invited(&self) -> usize {
        crate::context::open_invitations(&self.home)
    }
}

/// A client's command `args` started on Unix, `env` its environment, as [`Machine::start`]
/// starts one: on the client's terminal, where it has one, a pseudo-terminal of its own here
/// for those of its stdin, stdout and stderr that the client's terminal is; and `made_w` the
/// pipe its microVM's ID is told on, where the client asked for it.
#[cfg(unix)]
fn command(
    exe: &std::path::Path,
    args: &[String],
    env: &[(String, OsString)],
    terminal: Option<shards_remote::frame::Terminal>,
    made: Option<std::io::PipeReader>,
    made_w: Option<std::io::PipeWriter>,
) -> Result<Running, Error> {
    let pipe = || std::io::pipe().map_err(|e| Error::new(format!("a pipe for the command: {e}")));
    let tty = |e: std::io::Error| Error::new(format!("the command's terminal: {e}"));
    // The client's terminal: the command's stdin, stdout and stderr that the client's
    // terminal is are a terminal of its own here, as sshd gives a session's command one
    // (OpenSSH session.c, do_exec_pty); the rest are pipes.
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
    // What the terminal shows is read once, whole ([`Kept`]): as stdout's, where stdout is
    // on it, else as stderr's.
    let (shown, kept) = match &pty {
        Some((controller, peer)) if on(1) || on(2) => {
            let (reader, kept) = kept_terminal(controller, peer).map_err(tty)?;
            (Some(reader), Some(kept))
        }
        _ => (None, None),
    };
    let (out_shown, err_shown) = if on(1) { (shown, None) } else { (None, shown) };
    let (child_out, stdout): (OwnedFd, Box<dyn Read + Send>) = match (&pty, out_shown) {
        (Some((_, peer)), Some(reader)) => (peer.try_clone().map_err(tty)?, Box::new(reader)),
        _ => {
            let (r, w) = pipe()?;
            (w.into(), Box::new(r))
        }
    };
    let (child_err, stderr): (OwnedFd, Box<dyn Read + Send>) = match (&pty, err_shown) {
        (Some((_, peer)), _) if on(2) && on(1) => {
            (peer.try_clone().map_err(tty)?, Box::new(std::io::empty()))
        }
        (Some((_, peer)), Some(reader)) => (peer.try_clone().map_err(tty)?, Box::new(reader)),
        _ => {
            let (r, w) = pipe()?;
            (w.into(), Box::new(r))
        }
    };
    let mut set: Vec<(&str, &OsStr)> = env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())).collect();
    if pty.is_some() {
        set.push((crate::context::TAKE_TERMINAL, OsStr::new("1")));
    }
    let argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let mut fds = vec![
        (child_in.as_fd(), 0),
        (child_out.as_fd(), 1),
        (child_err.as_fd(), 2),
    ];
    let descriptor = crate::context::CIDFILE_DESCRIPTOR.to_string();
    if let Some(w) = &made_w {
        fds.push((w.as_fd(), crate::context::CIDFILE_DESCRIPTOR));
        set.push((crate::context::CIDFILE_FD, OsStr::new(&descriptor)));
    }
    // A session of its own, out of reach of what a terminal of the daemon's sends its
    // group, whose controlling terminal is its own where it has one.
    let child = shards_ipc::spawn_in(exe, &argv, &fds, true, &set)
        .map_err(|e| Error::new(format!("starting the command: {e}")))?;
    // The child holds its ends, and the terminal's peer: once it has gone, reading the
    // controller ends.
    drop((child_in, child_out, child_err, made_w));
    let controller = pty.map(|(controller, _peer)| controller);
    // Waited for on one thread, hung up and signalled from others: kill refuses a child
    // reaped.
    let child = Arc::new(child);
    let (hung, signalled) = (Arc::clone(&child), Arc::clone(&child));
    let abandoned = kept.clone();
    let resize = controller.map(resizer);
    Ok(Running {
        stdin,
        stdout,
        stderr,
        wait: Box::new(move || {
            let status = child.wait().unwrap_or(1);
            if let Some(kept) = &kept {
                kept.end();
            }
            status
        }),
        hangup: Box::new(move || {
            let _ = hung.kill(libc::SIGHUP);
            if let Some(kept) = &abandoned {
                kept.abandon();
            }
        }),
        resize,
        // What the client received, as this system numbers it; one it has not goes
        // nowhere.
        signal: Box::new(move |linux| {
            if let Some(sig) = shards_ipc::local_signal(linux) {
                let _ = signalled.kill(sig);
            }
        }),
        made: made.map(|r| Box::new(r) as Box<dyn Read + Send>),
    })
}

/// A pseudoconsole a client's command or build is made in (ConPTY), with its input's writer
/// where nothing else writes it, as a console whose input closed reads nothing more: made on
/// one thread, resized from another, and closed from a third, each under its lock for that
/// alone. Closed on a thread of its own, as before Windows 11 24H2 the close waits for what
/// it shows to be read, which its reader, the client's, does.
#[cfg(windows)]
#[derive(Clone)]
struct Console(Arc<std::sync::Mutex<Option<Held>>>);

/// What a [`Console`] holds until it is closed: the pseudoconsole, and its input's writer.
#[cfg(windows)]
type Held = (shards_ipc::PseudoConsole, Option<std::io::PipeWriter>);

#[cfg(windows)]
impl Console {
    fn new(console: shards_ipc::PseudoConsole, typed: Option<std::io::PipeWriter>) -> Console {
        Console(Arc::new(std::sync::Mutex::new(Some((console, typed)))))
    }

    /// `exe` started in it, as `shards_ipc::spawn_in_console` starts one.
    fn spawn(
        &self,
        exe: &std::path::Path,
        args: &[&OsStr],
        fds: &[(shards_ipc::fd::BorrowedFd<'_>, i32)],
        env: &[(&str, &OsStr)],
    ) -> std::io::Result<shards_ipc::Child> {
        match super::lock(&self.0).as_ref() {
            Some((console, _)) => shards_ipc::spawn_in_console(exe, args, fds, env, console),
            None => Err(std::io::Error::other("its console is closed")),
        }
    }

    /// What sizes it, as its client's terminal is sized.
    fn resizer(&self) -> Box<dyn Fn(u16, u16) + Send> {
        let held = self.clone();
        Box::new(move |rows, cols| {
            if let Some((console, _)) = super::lock(&held.0).as_ref() {
                let _ = console.resize(rows, cols);
            }
        })
    }

    /// Its build has ended: closed, what it shows read to its end by its client.
    fn end(&self) {
        self.close();
    }

    /// Its client has gone: closed, what it shows read by no one.
    fn abandon(&self) {
        self.close();
    }

    /// Closes it, once its command has ended, or its client gone.
    fn close(&self) {
        if let Some(held) = super::lock(&self.0).take() {
            let _ = std::thread::Builder::new()
                .name("console-close".into())
                .spawn(move || drop(held));
        }
    }
}

/// [`command`] on Windows: on the client's terminal, where it has one, a pseudoconsole of its
/// own here (ConPTY), as Windows' OpenSSH hosts a session's command, of the client's size
/// and sized with it, which those of its stdin, stdout and stderr that the client's terminal
/// is are opened in ([`crate::context::TAKE_TERMINAL`]); the rest pipes. What the client's
/// signals and its hang-up do to a Unix command, they do to this one through a pipe of its
/// own ([`crate::context::SIGNAL_FD`]): Windows sends a process no signal.
#[cfg(windows)]
fn command(
    exe: &std::path::Path,
    args: &[String],
    env: &[(String, OsString)],
    terminal: Option<shards_remote::frame::Terminal>,
    made: Option<std::io::PipeReader>,
    made_w: Option<std::io::PipeWriter>,
) -> Result<Running, Error> {
    let pipe = || std::io::pipe().map_err(|e| Error::new(format!("a pipe for the command: {e}")));
    let tty = |e: std::io::Error| Error::new(format!("the command's terminal: {e}"));
    let on = |fd: usize| terminal.is_some_and(|t| [t.stdin, t.stdout, t.stderr].get(fd) == Some(&true));
    let (console, mut typed, shown) = match terminal {
        Some(t) => {
            let (console, typed, shown) = shards_ipc::PseudoConsole::new(t.rows, t.cols).map_err(tty)?;
            (Some(console), Some(typed), Some(shown))
        }
        None => (None, None, None),
    };
    let (child_in, stdin): (Option<OwnedFd>, Box<dyn Write + Send>) = match typed.take() {
        Some(keys) if on(0) => (None, Box::new(keys)),
        unused => {
            // Kept with the console, which a closed input would leave reading nothing.
            typed = unused;
            let (r, w) = pipe()?;
            (Some(r.into()), Box::new(w))
        }
    };
    // What the console shows: as stdout's, where stdout is on it, else as stderr's; read
    // to its end where neither is, as a console whose screen is not read stops its writer.
    let (mut out_shown, mut err_shown) = if on(1) { (shown, None) } else { (None, shown) };
    if !on(1)
        && !on(2)
        && let Some(mut unread) = err_shown.take()
    {
        std::thread::Builder::new()
            .name("console-drain".into())
            .spawn(move || {
                let _ = std::io::copy(&mut unread, &mut std::io::sink());
            })
            .map_err(|e| Error::new(format!("a thread for the command's console: {e}")))?;
    }
    let (child_out, stdout): (Option<OwnedFd>, Box<dyn Read + Send>) = match out_shown.take() {
        Some(screen) => (None, Box::new(screen)),
        None => {
            let (r, w) = pipe()?;
            (Some(w.into()), Box::new(r))
        }
    };
    let (child_err, stderr): (Option<OwnedFd>, Box<dyn Read + Send>) = match err_shown.take() {
        _ if on(2) && on(1) => (None, Box::new(std::io::empty())),
        Some(screen) => (None, Box::new(screen)),
        None => {
            let (r, w) = pipe()?;
            (Some(w.into()), Box::new(r))
        }
    };
    let (signals_r, signals_w) = pipe()?;
    let mut set: Vec<(&str, &OsStr)> = env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())).collect();
    let streams: String = (0..3u8)
        .filter(|&fd| on(usize::from(fd)))
        .map(|fd| char::from(b'0' + fd))
        .collect();
    if console.is_some() {
        set.push((crate::context::TAKE_TERMINAL, OsStr::new(&streams)));
    }
    let argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let mut fds: Vec<(shards_ipc::fd::BorrowedFd<'_>, i32)> =
        [(&child_in, 0), (&child_out, 1), (&child_err, 2)]
            .into_iter()
            .filter_map(|(handle, n)| handle.as_ref().map(|h| (h.as_fd(), n)))
            .collect();
    let cidfile = crate::context::CIDFILE_DESCRIPTOR.to_string();
    if let Some(w) = &made_w {
        fds.push((w.as_fd(), crate::context::CIDFILE_DESCRIPTOR));
        set.push((crate::context::CIDFILE_FD, OsStr::new(&cidfile)));
    }
    let at = crate::context::signal_descriptor(fds.iter().map(|&(_, n)| n));
    let signals = at.to_string();
    fds.push((signals_r.as_fd(), at));
    set.push((crate::context::SIGNAL_FD, OsStr::new(&signals)));
    let console = console.map(|console| Console::new(console, typed));
    let started = match &console {
        Some(console) => console.spawn(exe, &argv, &fds, &set),
        None => shards_ipc::spawn_in(exe, &argv, &fds, true, &set),
    };
    let child = started.map_err(|e| Error::new(format!("starting the command: {e}")))?;
    drop((child_in, child_out, child_err, made_w, signals_r));
    let resize = console.as_ref().map(Console::resizer);
    let signals = Arc::new(std::sync::Mutex::new(Some(signals_w)));
    let hung = Arc::clone(&signals);
    Ok(Running {
        stdin,
        stdout,
        stderr,
        wait: Box::new(move || {
            let status = child.wait().unwrap_or(1);
            if let Some(console) = &console {
                console.close();
            }
            status
        }),
        // The pipe's end, as a terminal gone ends a Unix command by SIGHUP.
        hangup: Box::new(move || drop(super::lock(&hung).take())),
        resize,
        signal: Box::new(move |linux| {
            if let Some(pipe) = super::lock(&signals).as_mut() {
                let _ = pipe.write_all(&linux.to_le_bytes());
            }
        }),
        made: made.map(|r| Box::new(r) as Box<dyn Read + Send>),
    })
}

impl Machine {
    /// A connection to this machine's Engine API (D170), a client's Docker tools' through its
    /// context (D180 part 4): what the stream brings, to the API's socket, and the API's
    /// answers back, as a client of it on this machine; the stream's end of input the
    /// connection's, so that a client's half-close (an attach's end of stdin) reaches it.
    fn engine_step(&self) -> Result<Running, Error> {
        let reached = |e: std::io::Error| Error::new(format!("this machine's Engine API: {e}"));
        let conn = engine_api().map_err(reached)?;
        let (asking, hung) = (
            conn.try_clone().map_err(reached)?,
            conn.try_clone().map_err(reached)?,
        );
        Ok(Running {
            stdin: Box::new(HalfClosed(asking)),
            stdout: Box::new(conn),
            stderr: Box::new(std::io::empty()),
            wait: Box::new(|| 0),
            hangup: Box::new(move || {
                let _ = hung.shutdown(std::net::Shutdown::Both);
            }),
            resize: None,
            signal: Box::new(|_| {}),
            made: None,
        })
    }

    /// A client's command's environment: this machine's own shards, whatever context its
    /// owner's commands use, of the daemon's environment only what shards is set by
    /// ([`kept`]), and the client's own, which its request carries.
    fn command_env(&self, given: Vec<(String, Vec<u8>)>) -> Vec<(String, OsString)> {
        let mut env: Vec<(String, OsString)> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v)))
            .filter(|(k, _)| kept(k))
            .collect();
        env.extend([
            ("SHARDS_HOME".to_string(), self.home.clone().into_os_string()),
            (
                "SHARDS_CONTEXT".to_string(),
                OsString::from(crate::context::DEFAULT),
            ),
        ]);
        // The daemon's cache, where a build's microVMs find the VM process it writes out:
        // named outright, as no `HOME` of the owner's is given (XDG's falls back to it).
        if let Ok(cache) = crate::helpers::cache() {
            env.push(("SHARDS_CACHE".to_string(), cache.into_os_string()));
        }
        // The client's own paths are its step's to say (`sync_step`), no command's.
        env.extend(
            given
                .into_iter()
                .filter(|(k, _)| k != crate::synced::SHOWN_AS)
                .filter_map(|(k, v)| String::from_utf8(v).ok().map(|v| (k, OsString::from(v)))),
        );
        env
    }

    /// A step of a signature a client makes of an artifact here (agent.rs, `sign_step`), on a
    /// thread of its own: the client's bundle its stdin, what it says its stdout.
    fn sign_step(&self, args: Vec<String>) -> Result<Running, Error> {
        let pipe = || std::io::pipe().map_err(|e| Error::new(format!("a pipe for the signature: {e}")));
        let (mut in_r, in_w) = pipe()?;
        let (out_r, mut out_w) = pipe()?;
        let (err_r, mut err_w) = pipe()?;
        let (done, status) = std::sync::mpsc::channel();
        let home = self.home.clone();
        std::thread::Builder::new()
            .name("context-sign".into())
            .spawn(move || {
                let status = match crate::agent::sign_step(&home, &args, &mut in_r, &mut out_w) {
                    Ok(()) => 0,
                    Err(e) => {
                        let _ = writeln!(err_w, "ERROR: {e}");
                        1
                    }
                };
                drop((in_r, out_w, err_w));
                let _ = done.send(status);
            })
            .map_err(|e| Error::new(format!("a thread for the signature: {e}")))?;
        Ok(Running {
            stdin: Box::new(in_w),
            stdout: Box::new(out_r),
            stderr: Box::new(err_r),
            wait: Box::new(move || status.recv().unwrap_or(1)),
            hangup: Box::new(|| {}),
            resize: None,
            signal: Box::new(|_| {}),
            made: None,
        })
    }

    /// A step's connection to an SSH agent its client's build names, which the client claims
    /// by its token: what the step asks the client's to answer, there, the end of each's
    /// writing the other's end of reading.
    fn agent_step(&self, client: &str, token: &str) -> Result<Running, Error> {
        let taken = self
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(client.to_string(), token.to_string()));
        let conn = taken.ok_or_else(|| Error::new(format!("{token}: no step's connection to an agent")))?;
        let reached = |e: std::io::Error| Error::new(format!("a step's connection to an agent: {e}"));
        let (asking, hung) = (
            conn.try_clone().map_err(reached)?,
            conn.try_clone().map_err(reached)?,
        );
        Ok(Running {
            stdin: Box::new(HalfClosed(asking)),
            stdout: Box::new(conn),
            stderr: Box::new(std::io::empty()),
            wait: Box::new(|| 0),
            hangup: Box::new(move || {
                let _ = hung.shutdown(std::net::Shutdown::Both);
            }),
            resize: None,
            signal: Box::new(|_| {}),
            made: None,
        })
    }

    /// A step of a client's build context (crate::synced, D180 part 3c), kept in a copy of
    /// the client's own under this home, one step at a time under the copy's lock:
    /// `manifest`, what the copy holds; `build`, the build file's directory and the layer of
    /// what changed taken from the stream, then the build, of the copy, run as a client's
    /// command runs, its output and status the step's.
    fn sync_step(
        &self,
        client: &str,
        args: Vec<String>,
        env: Vec<(String, Vec<u8>)>,
        terminal: Option<shards_remote::frame::Terminal>,
    ) -> Result<Running, Error> {
        let step = args.get(1).cloned().unwrap_or_default();
        let key = args.get(2).cloned().unwrap_or_default();
        if step == "agent" {
            return self.agent_step(client, args.get(3).map(String::as_str).unwrap_or_default());
        }
        let copy = crate::synced::copy_of(&self.home, client, &key).map_err(Error::new)?;
        let copies = self.home.join("synced").join(client);
        // The client's own paths, which what the command prints says in place of its copy's.
        let (shown, env): (Vec<_>, Vec<_>) = env.into_iter().partition(|(k, _)| k == crate::synced::SHOWN_AS);
        let mut pairs = Vec::new();
        if let Some((_, shown)) = shown.first() {
            let mut theirs = shown.split(|b| *b == 0);
            for (ours, theirs) in [
                (copy.join("context"), theirs.next()),
                (copy.join("dockerfile"), theirs.next()),
            ] {
                if let Some(theirs) = theirs.filter(|t| !t.is_empty()) {
                    pairs.push((ours.into_os_string().into_encoded_bytes(), theirs.to_vec()));
                }
            }
            // Each tree the build names, by its key: its copy here, and the client's path.
            while let (Some(key), Some(theirs)) = (theirs.next(), theirs.next()) {
                let tree = std::str::from_utf8(key)
                    .ok()
                    .and_then(|key| crate::synced::copy_of(&self.home, client, key).ok());
                if let Some(tree) = tree.filter(|_| !theirs.is_empty()) {
                    pairs.push((
                        tree.join("context").into_os_string().into_encoded_bytes(),
                        theirs.to_vec(),
                    ));
                }
            }
        }
        let pipe = || std::io::pipe().map_err(|e| Error::new(format!("a pipe for the build: {e}")));
        let (mut in_r, in_w) = pipe()?;
        let (out_r, mut out_w) = pipe()?;
        // The client's terminal, where its stderr is one: a build's stderr is a terminal of
        // its own here, of the client's size and sized with it, as a command's is
        // ([`Machine::start`]), on which the build draws the display it draws on one
        // (build/live.rs). Its stdin and stdout are the step's streams, pipes.
        let tty = |e: std::io::Error| Error::new(format!("the build's terminal: {e}"));
        let on_terminal = terminal.filter(|t| t.stderr && step == "build");
        #[cfg(unix)]
        let pty = match on_terminal {
            Some(t) => Some(open_pty(t.rows, t.cols).map_err(tty)?),
            None => None,
        };
        #[cfg(unix)]
        let (err_r, mut err_w, kept): (Box<dyn Read + Send>, File, _) = match &pty {
            Some((controller, peer)) => {
                let (reader, kept) = kept_terminal(controller, peer).map_err(tty)?;
                (
                    Box::new(reader),
                    File::from(peer.try_clone().map_err(tty)?),
                    Some(kept),
                )
            }
            None => {
                let (r, w) = pipe()?;
                (Box::new(r), File::from(OwnedFd::from(w)), None)
            }
        };
        #[cfg(unix)]
        let resize = pty.map(|(controller, _peer)| resizer(controller));
        // On Windows the build is made in a pseudoconsole of its own (ConPTY), what it shows
        // read first; then what the daemon itself says of the build, on a pipe of its own, as
        // no process writes into a console it is not in.
        #[cfg(windows)]
        let (err_r, mut err_w, kept): (Box<dyn Read + Send>, File, Option<Console>) = {
            let (errors_r, errors_w) = pipe()?;
            let errors_w = File::from(OwnedFd::from(errors_w));
            match on_terminal {
                Some(t) => {
                    let (console, typed, screen) =
                        shards_ipc::PseudoConsole::new(t.rows, t.cols).map_err(tty)?;
                    let console = Console::new(console, Some(typed));
                    (Box::new(screen.chain(errors_r)), errors_w, Some(console))
                }
                None => (Box::new(errors_r), errors_w, None),
            }
        };
        #[cfg(windows)]
        let resize = kept.as_ref().map(Console::resizer);
        let (exe, env) = (self.exe.clone(), self.command_env(env));
        let (done, status) = std::sync::mpsc::channel();
        let building = Arc::new(Building::default());
        let ending = kept.clone();
        let registry = Arc::clone(&self.agents);
        let ours = Arc::clone(&building);
        let whose = client.to_string();
        std::thread::Builder::new()
            .name("context-step".into())
            .spawn(move || {
                let ran = (|| -> Result<i32, String> {
                    std::fs::create_dir_all(&copy).map_err(|e| format!("{}: {e}", copy.display()))?;
                    let lock = std::fs::File::create(copy.join(".lock"))
                        .and_then(|f| f.lock().map(|()| f))
                        .map_err(|e| format!("{}: {e}", copy.display()))?;
                    let (context, dockerfile) = (copy.join("context"), copy.join("dockerfile"));
                    let status = match step.as_str() {
                        "manifest" => {
                            let manifest = crate::synced::manifest_of(&context)?;
                            out_w
                                .write_all(&crate::synced::encode(&manifest))
                                .map_err(|e| e.to_string())?;
                            0
                        }
                        // Another tree the client's build names, brought in step before it.
                        "put" => {
                            let sent = crate::synced::receive_tree(&mut in_r, &context)?;
                            super::log(format!("client {whose}: tree {key}: {sent} bytes of files came"));
                            0
                        }
                        "build" => {
                            let words = args.get(3..).unwrap_or_default();
                            // As many secrets as the build's words name, and no more.
                            let named = words.iter().filter(|a| a.contains(crate::synced::SECRET)).count();
                            let (sent, secrets) =
                                crate::synced::receive(&mut in_r, &dockerfile, &context, named)?;
                            super::log(format!(
                                "client {whose}: context {key}: {sent} bytes of files came"
                            ));
                            // The build's outputs of the client's, in a directory of their own,
                            // which goes once they are sent.
                            let outputs = copy.join("out");
                            let _ = std::fs::remove_dir_all(&outputs);
                            std::fs::create_dir_all(&outputs)
                                .map_err(|e| format!("{}: {e}", outputs.display()))?;
                            // The SSH agents the build names: for each, a pair the build asks on
                            // for a connection to it (build::ssh::asked_of), its descriptors
                            // after its secrets', told it by a variable no client can send
                            // (frame::ENV), no path naming it for any other process to dial;
                            // each connection a step asks for is a pair of its own, one end the
                            // build's, the other held for the client to claim and relay to its
                            // own agent of that id.
                            let first_agent = crate::synced::SECRET_FD
                                .saturating_add(i32::try_from(secrets.len()).unwrap_or(i32::MAX));
                            let mut agent_places = std::collections::BTreeMap::new();
                            let mut asking = Vec::new();
                            let mut given = Vec::new();
                            for word in words {
                                let Some((_, id)) = word.split_once(crate::synced::AGENT_SOCKET) else {
                                    continue;
                                };
                                if agent_places.contains_key(id) {
                                    continue;
                                }
                                let (ours, theirs) = shards_ipc::Stream::pair()
                                    .map_err(|e| format!("a pair for the build's agent: {e}"))?;
                                let fd = first_agent
                                    .saturating_add(i32::try_from(given.len()).unwrap_or(i32::MAX));
                                agent_places.insert(id.to_string(), PathBuf::from(format!("/dev/fd/{fd}")));
                                asking.push((id.to_string(), ours));
                                given.push((theirs, fd));
                            }
                            let mut env = env.clone();
                            if !given.is_empty() {
                                let told: Vec<String> = given.iter().map(|(_, fd)| fd.to_string()).collect();
                                env.push((
                                    crate::build::ssh::AGENT_FDS.to_string(),
                                    OsString::from(told.join(",")),
                                ));
                            }
                            let argv: Vec<OsString> = words
                                .iter()
                                .map(|a| {
                                    let places = crate::synced::Places {
                                        context: &context,
                                        dockerfile: &dockerfile,
                                        out: &outputs,
                                        copies: &copies,
                                        agents: &agent_places,
                                    };
                                    crate::synced::placed(a, &places)
                                })
                                .collect();
                            // Each secret on a pipe of its own, which the build reads once, as
                            // its file: written in the order the build reads them, by one
                            // thread, each pipe closed once its secret is in it.
                            let mut readers = Vec::new();
                            let mut writers = Vec::new();
                            for (_, secret) in secrets {
                                let (r, w) =
                                    std::io::pipe().map_err(|e| format!("a pipe for a secret: {e}"))?;
                                readers.push(r);
                                writers.push((w, secret));
                            }
                            if !writers.is_empty() {
                                std::thread::Builder::new()
                                    .name("context-secrets".into())
                                    .spawn(move || {
                                        for (mut w, secret) in writers {
                                            let _ = w.write_all(secret.bytes());
                                        }
                                    })
                                    .map_err(|e| format!("a thread for the build's secrets: {e}"))?;
                            }
                            let ends = given
                                .iter()
                                .map(|(s, fd)| agent_end(s).map(|end| (end, *fd)))
                                .collect::<std::io::Result<Vec<_>>>()
                                .map_err(|e| format!("the build's agents: {e}"))?;
                            let fds: Vec<_> = readers
                                .iter()
                                .zip(crate::synced::SECRET_FD..)
                                .map(|(r, fd)| (r.as_fd(), fd))
                                .chain(ends.iter().map(|(end, fd)| (end.as_fd(), *fd)))
                                .collect();
                            // Its stdout framed as it comes (synced::SAID); then, once it is
                            // through, its outputs.
                            let (said_r, said_w) =
                                std::io::pipe().map_err(|e| format!("a pipe for the build's output: {e}"))?;
                            let (stop_r, stop_w) =
                                std::io::pipe().map_err(|e| format!("a pipe for the build's agents: {e}"))?;
                            let mut claimable = Vec::new();
                            let framed = crate::synced::Framed::new(&mut out_w);
                            let status = std::thread::scope(|scope| -> Result<i32, String> {
                                let framed = &framed;
                                let saying = std::thread::Builder::new()
                                    .name("context-said".into())
                                    .spawn_scoped(scope, move || crate::synced::say(said_r, framed))
                                    .map_err(|e| format!("a thread for the build's output: {e}"))?;
                                let (asking, stop_r, registry, whose) = (&asking, &stop_r, &registry, &whose);
                                #[cfg(unix)]
                                let err = BuildErr::Fd(err_w.as_fd());
                                #[cfg(windows)]
                                let err = match &ending {
                                    Some(console) => BuildErr::Console(console),
                                    None => BuildErr::Fd(err_w.as_fd()),
                                };
                                // Its agents served once it is made: on Windows each pair's end
                                // gives it what comes on it, the connections it asks for.
                                let mut serving = None;
                                let ran = ours.run(&exe, &argv, &env, &said_w, err, &fds, |child| {
                                    #[cfg(windows)]
                                    for (_, ours) in asking {
                                        let giver =
                                            child.giver().map_err(|e| format!("the build's agents: {e}"))?;
                                        ours.gives_to(giver.into_handle());
                                    }
                                    #[cfg(unix)]
                                    let _ = child;
                                    let thread = std::thread::Builder::new()
                                        .name("context-agents".into())
                                        .spawn_scoped(scope, move || {
                                            serve_agents(asking, stop_r, framed, whose, registry)
                                        })
                                        .map_err(|e| format!("a thread for the build's agents: {e}"))?;
                                    serving = Some(thread);
                                    Ok(())
                                });
                                drop((said_w, stop_w));
                                if let Some(serving) = serving {
                                    claimable = serving
                                        .join()
                                        .map_err(|_| "the build's agents' thread ended".to_string())?;
                                }
                                let said = saying
                                    .join()
                                    .map_err(|_| "the build's output's thread ended".to_string())?;
                                let status = ran?;
                                said.map_err(|e| format!("the build's output: {e}"))?;
                                Ok(status)
                            })?;
                            let out = framed.into_inner();
                            // The connections no client claimed go with the build.
                            {
                                let mut held =
                                    registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                for t in claimable {
                                    held.remove(&(whose.clone(), t));
                                }
                            }
                            if status == 0 {
                                crate::synced::send_outputs(&outputs, out)?;
                            }
                            let _ = std::fs::remove_dir_all(&outputs);
                            status
                        }
                        other => return Err(format!("{other:?}: no such step of a context")),
                    };
                    drop(lock);
                    Ok(status)
                })();
                let status = ran.unwrap_or_else(|e| {
                    let _ = writeln!(err_w, "shards: {e}");
                    1
                });
                drop((in_r, out_w, err_w));
                if let Some(kept) = &ending {
                    kept.end();
                }
                let _ = done.send(status);
            })
            .map_err(|e| Error::new(format!("a thread for the build: {e}")))?;
        let (hung, signalled) = (Arc::clone(&building), building);
        let abandoned = kept.clone();
        Ok(Running {
            stdin: Box::new(in_w),
            stdout: Box::new(out_r),
            stderr: Box::new(crate::synced::Shown::new(err_r, pairs)),
            wait: Box::new(move || status.recv().unwrap_or(1)),
            hangup: Box::new(move || {
                hung.end(shards_ipc::signal::SIGHUP);
                if let Some(kept) = &abandoned {
                    kept.abandon();
                }
            }),
            resize,
            signal: Box::new(move |linux| {
                if let Some(sig) = shards_ipc::local_signal(linux) {
                    signalled.end(sig);
                }
            }),
            made: None,
        })
    }

    /// A step of a client's copy (`shards cp`, cli/cp.rs), which the client drives as this
    /// machine's own CLI drives one (D180): this daemon's to take, asked as the CLI asks it,
    /// the stream's pipes the descriptors the CLI would give, an archive's to write and an
    /// extract's to read; what it says, and its status, the command's.
    fn copy_step(&self, args: Vec<String>) -> Result<Running, Error> {
        let identity = shards_ipc::Identity::of_build(&self.exe)
            .map_err(|e| Error::new(format!("{}: {e}", self.exe.display())))?;
        let pipe = || std::io::pipe().map_err(|e| Error::new(format!("a pipe for the copy: {e}")));
        let (in_r, in_w) = pipe()?;
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        let (done, status) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("copy-step".into())
            .spawn(move || {
                let (code, out, err) = {
                    let fds: Vec<crate::cli::Stream<'_>> = match args.get(1).map(String::as_str) {
                        Some("archive") => vec![crate::cli::Stream::To(out_w.as_fd())],
                        Some("extract") => vec![crate::cli::Stream::From(in_r.as_fd())],
                        _ => Vec::new(),
                    };
                    ask_self(args, identity, &fds).unwrap_or_else(|e| (1, Vec::new(), e.into_bytes()))
                };
                drop(in_r);
                let (mut out_w, mut err_w) = (out_w, err_w);
                let _ = out_w.write_all(&out);
                let _ = err_w.write_all(&err);
                drop((out_w, err_w));
                let _ = done.send(i32::from(code));
            })
            .map_err(|e| Error::new(format!("a thread for the copy: {e}")))?;
        Ok(Running {
            stdin: Box::new(in_w),
            stdout: Box::new(out_r),
            stderr: Box::new(err_r),
            wait: Box::new(move || status.recv().unwrap_or(1)),
            // The step ends with its pipes: an archive read no more, an extract's input
            // ended.
            hangup: Box::new(|| {}),
            resize: None,
            signal: Box::new(|_| {}),
            made: None,
        })
    }
}

/// A connection to this machine's Engine API: its socket, relative to the home, the
/// daemon's working directory; on Windows the named pipe the home's file names.
fn engine_api() -> std::io::Result<shards_ipc::Stream> {
    #[cfg(unix)]
    return std::os::unix::net::UnixStream::connect(super::engine::SOCKET);
    #[cfg(windows)]
    return shards_ipc::Stream::connect(std::fs::read_to_string(super::engine::SOCKET)?.trim());
}

/// A connection's writing half, closed as it drops: what its reader then reads is the end.
struct HalfClosed(shards_ipc::Stream);

impl Write for HalfClosed {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Drop for HalfClosed {
    fn drop(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Write);
    }
}

/// A build a context step runs: its command once it has started, and whether its client
/// left before then, which ends it as it starts; on Windows, the pipe its command takes its
/// client's signals from ([`crate::context::SIGNAL_FD`]), as Windows sends a process none.
#[derive(Default)]
struct Building {
    child: std::sync::OnceLock<shards_ipc::Child>,
    hung: std::sync::atomic::AtomicBool,
    #[cfg(windows)]
    signals: std::sync::Mutex<Option<std::io::PipeWriter>>,
}

/// Where a build's stderr goes: a descriptor of its own, or on Windows the pseudoconsole it
/// is made in, its stderr opened there ([`crate::context::TAKE_TERMINAL`]).
#[derive(Clone, Copy)]
enum BuildErr<'a> {
    Fd(shards_ipc::fd::BorrowedFd<'a>),
    #[cfg(windows)]
    Console(&'a Console),
}

impl Building {
    /// `argv` run as a client's command runs, in a session of its own, writing to `out` and
    /// `err`, given `more` too; `started` told of it once it is made, before it is waited
    /// for (on Windows, what hands it the connections its agents' pairs bring). Its status.
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        exe: &std::path::Path,
        argv: &[OsString],
        env: &[(String, OsString)],
        out: &impl AsFd,
        err: BuildErr<'_>,
        more: &[(shards_ipc::fd::BorrowedFd<'_>, i32)],
        started: impl FnOnce(&shards_ipc::Child) -> Result<(), String>,
    ) -> Result<i32, String> {
        let (input, _end) = std::io::pipe().map_err(|e| format!("a pipe for the build: {e}"))?;
        let args: Vec<&OsStr> = argv.iter().map(OsString::as_os_str).collect();
        #[cfg_attr(unix, allow(unused_mut))]
        let mut set: Vec<(&str, &OsStr)> = env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())).collect();
        let mut fds = vec![(input.as_fd(), 0), (out.as_fd(), 1)];
        match err {
            BuildErr::Fd(err) => fds.push((err, 2)),
            #[cfg(windows)]
            BuildErr::Console(_) => {}
        }
        fds.extend_from_slice(more);
        #[cfg(windows)]
        let (signals_r, at) = {
            let (r, w) = std::io::pipe().map_err(|e| format!("a pipe for the build's signals: {e}"))?;
            *super::lock(&self.signals) = Some(w);
            (r, crate::context::signal_descriptor(fds.iter().map(|&(_, n)| n)))
        };
        #[cfg(windows)]
        let number = at.to_string();
        #[cfg(windows)]
        {
            fds.push((signals_r.as_fd(), at));
            set.push((crate::context::SIGNAL_FD, OsStr::new(&number)));
            if matches!(err, BuildErr::Console(_)) {
                set.push((crate::context::TAKE_TERMINAL, OsStr::new("2")));
            }
        }
        if self.hung.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(129);
        }
        #[cfg(unix)]
        let child = shards_ipc::spawn_in(exe, &args, &fds, true, &set);
        #[cfg(windows)]
        let child = match err {
            BuildErr::Console(console) => console.spawn(exe, &args, &fds, &set),
            BuildErr::Fd(_) => shards_ipc::spawn_in(exe, &args, &fds, true, &set),
        };
        let child = child.map_err(|e| format!("starting the build: {e}"))?;
        drop(input);
        #[cfg(windows)]
        drop(signals_r);
        let child = self.child.get_or_init(|| child);
        if let Err(e) = started(child) {
            let _ = child.kill(shards_ipc::signal::SIGKILL);
            let _ = child.wait();
            return Err(e);
        }
        // A hang-up between the start and the line above found no command to end.
        if self.hung.load(std::sync::atomic::Ordering::SeqCst) {
            self.end(shards_ipc::signal::SIGHUP);
        }
        child.wait().map_err(|e| format!("the build: {e}"))
    }

    /// Ends the build with `sig`, or, not started yet, as it starts: on Windows told it on
    /// its signals' pipe, its hang-up that pipe's end.
    fn end(&self, sig: i32) {
        if sig == shards_ipc::signal::SIGHUP {
            self.hung.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        #[cfg(unix)]
        if let Some(child) = self.child.get() {
            let _ = child.kill(sig);
        }
        #[cfg(windows)]
        {
            let mut signals = super::lock(&self.signals);
            if sig == shards_ipc::signal::SIGHUP {
                drop(signals.take());
            } else if let (Some(pipe), Ok(linux)) = (signals.as_mut(), u32::try_from(sig)) {
                let _ = pipe.write_all(&linux.to_le_bytes());
            }
        }
    }
}

/// Answers what a build asks of its agents' pairs `asking`, until `stop` ends: each `DIAL`
/// a pair made for it, one end sent the build (`GRANTED`), the other held for `client` to
/// claim by a token, which `framed` tells it, with the agent's id. Each end sent is kept
/// until the build says it has it (`TAKEN`), as XNU's collector flushes a socket in flight
/// that no process holds (M24); then let go, so that the step's close is the end the
/// client's relay sees. The tokens told.
#[cfg(unix)]
fn serve_agents<W: Write>(
    asking: &[(String, shards_ipc::Stream)],
    stop: &std::io::PipeReader,
    framed: &crate::synced::Framed<W>,
    client: &str,
    registry: &std::sync::Mutex<Agents>,
) -> Vec<String> {
    use shards_ipc::kind;
    let mut told = Vec::new();
    // Each pair's: whether its build still asks on it, and the ends sent it not yet taken.
    let mut pairs: Vec<(bool, std::collections::VecDeque<shards_ipc::Stream>)> = asking
        .iter()
        .map(|_| (true, std::collections::VecDeque::new()))
        .collect();
    loop {
        // A pair whose build has gone is looked at no more (poll(2) passes over a negative
        // descriptor).
        let mut polled: Vec<libc::pollfd> = std::iter::once(stop.as_raw_fd())
            .chain(
                asking
                    .iter()
                    .zip(&pairs)
                    .map(|((_, s), (open, _))| if *open { s.as_raw_fd() } else { -1 }),
            )
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let n = libc::nfds_t::try_from(polled.len()).unwrap_or(libc::nfds_t::MAX);
        // SAFETY: poll(2) on pollfds of descriptors the caller holds open, `n` of them.
        if unsafe { libc::poll(polled.as_mut_ptr(), n, -1) } < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return told;
        }
        if polled.first().is_some_and(|p| p.revents != 0) {
            return told;
        }
        for (((id, socket), p), (open, sent)) in
            asking.iter().zip(polled.iter().skip(1)).zip(pairs.iter_mut())
        {
            if p.revents == 0 {
                continue;
            }
            match shards_ipc::recv(socket) {
                Ok(Some(m)) if m.kind == kind::TAKEN => {
                    sent.pop_front();
                }
                Ok(Some(m)) if m.kind == kind::DIAL => {
                    if let Some((t, theirs)) = grant(socket, id, framed, client, registry) {
                        told.push(t);
                        sent.push_back(theirs);
                    }
                }
                // The build ended, or asked what no build asks: no more of this pair.
                _ => {
                    *open = false;
                    sent.clear();
                }
            }
        }
    }
}

/// [`serve_agents`] on Windows, which has no poll(2) of pipes: each pair answered on a thread
/// of its own, what comes on it read as it comes, until the build's end closes it or `stop`
/// ends, when each pair's reads are ended, which ends its thread's.
#[cfg(windows)]
fn serve_agents<W: Write + Send>(
    asking: &[(String, shards_ipc::Stream)],
    stop: &std::io::PipeReader,
    framed: &crate::synced::Framed<W>,
    client: &str,
    registry: &std::sync::Mutex<Agents>,
) -> Vec<String> {
    use shards_ipc::kind;
    std::thread::scope(|scope| {
        let answering: Vec<_> = asking
            .iter()
            .filter_map(|(id, socket)| {
                std::thread::Builder::new()
                    .name("context-agent".into())
                    .spawn_scoped(scope, move || {
                        let (mut told, mut sent) = (Vec::new(), std::collections::VecDeque::new());
                        loop {
                            match shards_ipc::recv(socket) {
                                Ok(Some(m)) if m.kind == kind::TAKEN => {
                                    sent.pop_front();
                                }
                                Ok(Some(m)) if m.kind == kind::DIAL => {
                                    if let Some((t, theirs)) = grant(socket, id, framed, client, registry) {
                                        told.push(t);
                                        sent.push_back(theirs);
                                    }
                                }
                                // The build ended, or asked what no build asks.
                                _ => return told,
                            }
                        }
                    })
                    .ok()
            })
            .collect();
        // Its end the build's: the pairs' reads end, and each thread with them.
        let mut ended = stop;
        let _ = ended.read(&mut [0u8; 1]);
        for (_, socket) in asking {
            let _ = socket.shutdown(std::net::Shutdown::Read);
        }
        answering
            .into_iter()
            .flat_map(|thread| thread.join().unwrap_or_default())
            .collect()
    })
}

/// Answers a build's `DIAL` on `socket`, its agent `id`'s pair: a pair made for it, one end
/// sent the build (`GRANTED`), the other held for `client` to claim by a token, which
/// `framed` tells it, with the agent's id. The token, and the end sent, which is kept until
/// the build says it has it (`TAKEN`), as XNU's collector flushes a socket in flight that no
/// process holds (M24); none where it could make or send none, the build told why.
fn grant<W: Write>(
    socket: &shards_ipc::Stream,
    id: &str,
    framed: &crate::synced::Framed<W>,
    client: &str,
    registry: &std::sync::Mutex<Agents>,
) -> Option<(String, shards_ipc::Stream)> {
    use shards_ipc::kind;
    let made = shards_ipc::Stream::pair()
        .map_err(|e| format!("a connection to the agent: {e}"))
        .and_then(|pair| token(16).map(|t| (pair, t)))
        .and_then(|((conn, theirs), t)| {
            agent_end(&theirs)
                .map(|end| (conn, theirs, end, t))
                .map_err(|e| format!("a connection to the agent: {e}"))
        });
    let (conn, theirs, end, t) = match made {
        Ok(made) => made,
        Err(why) => {
            let _ = shards_ipc::send(socket, kind::ERR, why.as_bytes(), &[]);
            return None;
        }
    };
    shards_ipc::send(socket, kind::GRANTED, &[], &[end.as_fd()]).ok()?;
    registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((client.to_string(), t.clone()), conn);
    let _ = framed.frame(crate::synced::AGENT, format!("{id}\0{t}").as_bytes());
    Some((t, theirs))
}

/// What a child is given of `pair`'s end, by number, or sent of it: its descriptor on Unix;
/// on Windows another handle to its pipe's end, which the child takes as given
/// (`shards_ipc::Stream::given`).
fn agent_end(pair: &shards_ipc::Stream) -> std::io::Result<OwnedFd> {
    #[cfg(unix)]
    return pair.as_fd().try_clone_to_owned();
    #[cfg(windows)]
    return pair.share();
}

/// `argv` asked of this daemon as its CLI asks it, with `fds` the streams its request stands
/// for: through a slot of its home, the daemon's working directory (cli/client.rs
/// `ask_here`); its status, and what it said on stdout and stderr.
fn ask_self(
    argv: Vec<String>,
    daemon: shards_ipc::Identity,
    fds: &[crate::cli::Stream<'_>],
) -> Result<(u8, Vec<u8>, Vec<u8>), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    let command = shards_ipc::Command {
        argv,
        now,
        utc_offset: crate::cli::utc_offset(),
        daemon,
        ..shards_ipc::Command::default()
    };
    crate::cli::ask_here(&command, fds)
}

/// Whether the daemon's variable `name` goes to a client's command: shards' own settings of
/// this machine, and where its programs, user and temporary files are, and its network's
/// proxies (D180). No credential of its owner's: none of `HOME`,
/// `DOCKER_CONFIG` or `XDG_CONFIG_HOME`, under which a registry's login, a cloud's
/// credentials and SSH keys are kept, and with no config of its own a pull asks its
/// registry with what its client sent alone (shards_registry::credentials, lookup); nor the
/// settings that are the client's, which its request carries (frame::ENV), nor shards'
/// tests' credentials.
#[cfg(unix)]
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

/// [`kept`] on Windows, whose variables' names are a name whatever their case (`Path`):
/// its own names for the same, `USERNAME` and the temporary directory's `TEMP` and `TMP`,
/// and `PROGRAMDATA`, under which the machine's own registry certificates are kept
/// (certs.d), no user's; never `USERPROFILE`, the home under which the owner's registry
/// logins are kept (`.docker`).
#[cfg(windows)]
fn kept(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if shards_remote::frame::ENV
        .iter()
        .any(|e| e.eq_ignore_ascii_case(name))
    {
        return false;
    }
    matches!(
        upper.as_str(),
        "PATH" | "USERNAME" | "TEMP" | "TMP" | "PROGRAMDATA" | "HTTP_PROXY" | "HTTPS_PROXY" | "NO_PROXY"
    ) || (upper.starts_with("SHARDS_") && !upper.starts_with("SHARDS_TEST_"))
}

#[cfg(unix)]
/// What sizes the terminal `controller` controls, as its client's is sized.
fn resizer(controller: OwnedFd) -> Box<dyn Fn(u16, u16) + Send> {
    Box::new(move |rows, cols| {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads one winsize, on the terminal's controller, ours. The
        // kernel signals the terminal's foreground, the command's group (SIGWINCH).
        unsafe { libc::ioctl(controller.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    })
}

#[cfg(unix)]
/// A writer of a terminal of a client's command's own, kept until the command has ended and
/// nothing written to the terminal is unread, so that the command's close of it is never its
/// last: at a terminal's last close XNU waits 60 ticks for what is unread to be read
/// (bsd/kern/tty_dev.c, ptsclose's t_timeout; tty.c, ttylclose and ttywait), then
/// discards it, and the controller reads nothing of a terminal no longer open (tty_dev.c,
/// ptcread). A client that read slowly lost its command's last output, and the command
/// waited 0.6 s to end (PM M246). Linux keeps what is unread for the controller
/// (drivers/tty/n_tty.c, n_tty_read).
struct Kept {
    writer: std::sync::Mutex<Option<File>>,
    ended: std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
impl Kept {
    /// The command has ended: the writer goes now, where nothing is unread, else once its
    /// reader ([`Controller`]) has read it.
    fn end(&self) {
        self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
        self.close_if_read();
    }

    fn close_if_read(&self) {
        let mut writer = super::lock(&self.writer);
        if writer.as_ref().is_some_and(|w| unread(w) == 0) {
            *writer = None;
        }
    }

    /// The command's client is gone: what it wrote goes unread, now, and so does the writer,
    /// so that nothing waits for a reader that will not come. While a writer is kept, a
    /// command's end waits for what its controlling terminal holds to be read
    /// (bsd/kern/kern_exit.c, proc_exit's ttywait), which with no reader is never (PM M246).
    fn abandon(&self) {
        if let Some(writer) = super::lock(&self.writer).take() {
            // SAFETY: tcflush(3) of the terminal `writer` writes to, ours.
            unsafe { libc::tcflush(writer.as_raw_fd(), libc::TCOFLUSH) };
        }
    }
}

#[cfg(unix)]
/// How many bytes written to the terminal `writer` writes to are unread (TIOCOUTQ); none
/// where it cannot say.
fn unread(writer: &File) -> usize {
    let mut n: libc::c_int = 0;
    // SAFETY: ioctl(2) TIOCOUTQ writes one int, into `n`.
    if unsafe { libc::ioctl(writer.as_raw_fd(), libc::TIOCOUTQ, &raw mut n) } != 0 {
        return 0;
    }
    usize::try_from(n).unwrap_or(0)
}

#[cfg(unix)]
/// A terminal's controller, read to its end: once its command has ended and it has read
/// all, its [`Kept`] writer goes; once the last writer has, Linux's read says EIO, after all
/// that came before (drivers/tty/n_tty.c, n_tty_read, TTY_OTHER_CLOSED), which is the end.
struct Controller {
    file: File,
    kept: Arc<Kept>,
}

#[cfg(unix)]
impl Read for Controller {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = match self.file.read(buf) {
            Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
            read => read?,
        };
        if self.kept.ended.load(std::sync::atomic::Ordering::SeqCst) {
            self.kept.close_if_read();
        }
        Ok(n)
    }
}

#[cfg(unix)]
/// The reader of the terminal `controller` controls, and the [`Kept`] writer of its `peer`
/// it shares with its command's waiter.
fn kept_terminal(controller: &OwnedFd, peer: &OwnedFd) -> std::io::Result<(Controller, Arc<Kept>)> {
    let kept = Arc::new(Kept {
        writer: std::sync::Mutex::new(Some(File::from(peer.try_clone()?))),
        ended: std::sync::atomic::AtomicBool::new(false),
    });
    let reader = Controller {
        file: File::from(controller.try_clone()?),
        kept: Arc::clone(&kept),
    };
    Ok((reader, kept))
}

#[cfg(unix)]
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
        agents: Arc::default(),
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

    #[cfg(unix)]
    /// What a command writes to its terminal is read whole however late its reader comes,
    /// later than XNU waits at a terminal's last close (PM M246), and through what shows a
    /// client's paths, a tail that might have begun one among it; and a terminal its command
    /// wrote nothing more to ends as the command does.
    #[test]
    fn what_a_command_writes_to_its_terminal_is_read_however_late() {
        // All `reader` reads to its end, which must come.
        fn read_all(mut reader: impl Read + Send + 'static) -> Vec<u8> {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut read = Vec::new();
                let _ = tx.send(reader.read_to_end(&mut read).map(|_| read));
            });
            rx.recv_timeout(std::time::Duration::from_secs(30))
                .expect("the terminal's end")
                .unwrap()
        }
        let (controller, peer) = open_pty(24, 80).unwrap();
        let (reader, kept) = kept_terminal(&controller, &peer).unwrap();
        drop(controller);
        let mut command = File::from(peer);
        command.write_all(b"done /").unwrap();
        drop(command);
        kept.end();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let shown = crate::synced::Shown::new(reader, vec![(b"/m/copy".to_vec(), b"/here".to_vec())]);
        assert_eq!(read_all(shown), b"done /");

        let (controller, peer) = open_pty(24, 80).unwrap();
        let (reader, kept) = kept_terminal(&controller, &peer).unwrap();
        drop((controller, peer));
        kept.end();
        assert!(read_all(reader).is_empty());

        // Its client gone, the terminal ends, and what it wrote is let go. XNU holds what a
        // terminal's command wrote in the terminal's own queue until its controller reads
        // it, which the abandon flushes (PM M246), so none of it is read; Linux hands it to
        // the controller as it is written (drivers/tty/pty.c, pty_write), the flush reaching
        // only what the line discipline has yet to take, so some or all may be read.
        let (controller, peer) = open_pty(24, 80).unwrap();
        let (reader, kept) = kept_terminal(&controller, &peer).unwrap();
        drop(controller);
        let mut command = File::from(peer);
        command.write_all(b"never read").unwrap();
        drop(command);
        kept.abandon();
        let read = read_all(reader);
        #[cfg(target_os = "macos")]
        assert!(read.is_empty(), "{read:?}");
        #[cfg(not(target_os = "macos"))]
        assert!(b"never read".starts_with(&read), "{read:?}");
    }

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

    #[cfg(unix)]
    /// A build's step's connection to its agent ends, for the client's relay, with the
    /// step's close once the build has it: the daemon lets its own end go at `TAKEN`, where
    /// keeping it would have held each relay open until the build ended.
    #[test]
    fn an_agent_connection_ends_with_its_step_once_the_build_has_it() {
        use shards_ipc::kind;
        use std::os::unix::net::UnixStream;
        let (ours, build) = UnixStream::pair().unwrap();
        let (stop_r, stop_w) = std::io::pipe().unwrap();
        let framed = crate::synced::Framed::new(Vec::new());
        let registry = std::sync::Mutex::new(std::collections::HashMap::new());
        let asking = vec![("default".to_string(), ours)];
        let told = std::thread::scope(|scope| {
            let serving = scope.spawn(|| serve_agents(&asking, &stop_r, &framed, "client", &registry));
            shards_ipc::send(&build, kind::DIAL, &[], &[]).unwrap();
            let answer = shards_ipc::recv(&build).unwrap().unwrap();
            assert_eq!(answer.kind, kind::GRANTED);
            let mut step = UnixStream::from(answer.fds.into_iter().next().unwrap());
            // The end held for the client, once the daemon has it, and its timeout set
            // while both ends are open: XNU refuses an option of a socket its peer has
            // closed (EINVAL, bsd/kern/uipc_socket.c sosetoptlock).
            let started = std::time::Instant::now();
            let mut conn = loop {
                let held = registry.lock().unwrap().drain().next();
                if let Some(((client, _), conn)) = held {
                    assert_eq!(client, "client");
                    break conn;
                }
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(10),
                    "no connection held"
                );
                std::thread::yield_now();
            };
            conn.set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            shards_ipc::send(&build, kind::TAKEN, &[], &[]).unwrap();
            step.write_all(b"request").unwrap();
            drop(step);
            let mut got = Vec::new();
            conn.read_to_end(&mut got).unwrap();
            assert_eq!(got, b"request");
            drop(stop_w);
            serving.join().unwrap()
        });
        assert_eq!(told.len(), 1);
        let said = framed.into_inner();
        let token = told.first().unwrap();
        let tells = format!("default\0{token}");
        assert!(
            said.windows(tells.len()).any(|w| w == tells.as_bytes()),
            "the client is told the agent's id and the token"
        );
    }
}

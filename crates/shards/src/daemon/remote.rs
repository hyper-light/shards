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
            return self.sync_step(client, args, request.env);
        }
        if args.first().map(String::as_str) == Some(crate::agent::SIGN_STEP) {
            let shown: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
            super::log(format!("client {client}: signature step {}", shown.join(" ")));
            return self.sign_step(args);
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
        let env = self.command_env(request.env);
        let mut set: Vec<(&str, &OsStr)> = env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())).collect();
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

impl Machine {
    /// A connection to this machine's Engine API (D170), a client's Docker tools' through its
    /// context (D180 part 4): what the stream brings, to the API's socket, and the API's
    /// answers back, as a client of it on this machine; the stream's end of input the
    /// connection's, so that a client's half-close (an attach's end of stdin) reaches it.
    fn engine_step(&self) -> Result<Running, Error> {
        let reached = |e: std::io::Error| Error::new(format!("this machine's Engine API: {e}"));
        let conn = std::os::unix::net::UnixStream::connect(super::engine::SOCKET).map_err(reached)?;
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
    ) -> Result<Running, Error> {
        let step = args.get(1).cloned().unwrap_or_default();
        let key = args.get(2).cloned().unwrap_or_default();
        let copy = crate::synced::copy_of(&self.home, client, &key).map_err(Error::new)?;
        // The client's own paths, which what the command prints says in place of its copy's.
        let (shown, env): (Vec<_>, Vec<_>) = env.into_iter().partition(|(k, _)| k == crate::synced::SHOWN_AS);
        let mut pairs = Vec::new();
        if let Some((_, shown)) = shown.first() {
            let mut theirs = shown.splitn(2, |b| *b == 0);
            for (ours, theirs) in [
                (copy.join("context"), theirs.next()),
                (copy.join("dockerfile"), theirs.next()),
            ] {
                if let Some(theirs) = theirs.filter(|t| !t.is_empty()) {
                    pairs.push((ours.into_os_string().into_encoded_bytes(), theirs.to_vec()));
                }
            }
        }
        let pipe = || std::io::pipe().map_err(|e| Error::new(format!("a pipe for the build: {e}")));
        let (mut in_r, in_w) = pipe()?;
        let (out_r, mut out_w) = pipe()?;
        let (err_r, mut err_w) = pipe()?;
        let (done, status) = std::sync::mpsc::channel();
        let building = Arc::new(Building::default());
        let (exe, env) = (self.exe.clone(), self.command_env(env));
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
                        "build" => {
                            let sent = crate::synced::receive(&mut in_r, &dockerfile, &context)?;
                            super::log(format!(
                                "client {whose}: context {key}: {sent} bytes of files came"
                            ));
                            let argv: Vec<OsString> = args
                                .get(3..)
                                .unwrap_or_default()
                                .iter()
                                .map(|a| crate::synced::placed(a, &context, &dockerfile))
                                .collect();
                            ours.run(&exe, &argv, &env, &out_w, &err_w)?
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
                let _ = done.send(status);
            })
            .map_err(|e| Error::new(format!("a thread for the build: {e}")))?;
        let (hung, signalled) = (Arc::clone(&building), building);
        Ok(Running {
            stdin: Box::new(in_w),
            stdout: Box::new(out_r),
            stderr: Box::new(crate::synced::Shown::new(err_r, pairs)),
            wait: Box::new(move || status.recv().unwrap_or(1)),
            hangup: Box::new(move || hung.end(libc::SIGHUP)),
            resize: None,
            signal: Box::new(move |linux| {
                if let Some(sig) = shards_ipc::local_signal(linux) {
                    signalled.end(sig);
                }
            }),
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
                    let fds: Vec<std::os::fd::BorrowedFd<'_>> = match args.get(1).map(String::as_str) {
                        Some("archive") => vec![out_w.as_fd()],
                        Some("extract") => vec![in_r.as_fd()],
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
        })
    }
}

/// A connection's writing half, closed as it drops: what its reader then reads is the end.
struct HalfClosed(std::os::unix::net::UnixStream);

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
/// left before then, which ends it as it starts.
#[derive(Default)]
struct Building {
    child: std::sync::OnceLock<shards_ipc::Child>,
    hung: std::sync::atomic::AtomicBool,
}

impl Building {
    /// `argv` run as a client's command runs, in a session of its own, writing to `out` and
    /// `err`; its status.
    fn run(
        &self,
        exe: &std::path::Path,
        argv: &[OsString],
        env: &[(String, OsString)],
        out: &impl AsFd,
        err: &impl AsFd,
    ) -> Result<i32, String> {
        let (input, _end) = std::io::pipe().map_err(|e| format!("a pipe for the build: {e}"))?;
        let args: Vec<&OsStr> = argv.iter().map(OsString::as_os_str).collect();
        let set: Vec<(&str, &OsStr)> = env.iter().map(|(k, v)| (k.as_str(), v.as_os_str())).collect();
        let fds = [(input.as_fd(), 0), (out.as_fd(), 1), (err.as_fd(), 2)];
        if self.hung.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(129);
        }
        let child = shards_ipc::spawn_in(exe, &args, &fds, true, &set)
            .map_err(|e| format!("starting the build: {e}"))?;
        drop(input);
        let child = self.child.get_or_init(|| child);
        // A hang-up between the start and the line above found no command to end.
        if self.hung.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = child.kill(libc::SIGHUP);
        }
        child.wait().map_err(|e| format!("the build: {e}"))
    }

    /// Ends the build with `sig`, or, not started yet, as it starts.
    fn end(&self, sig: libc::c_int) {
        if sig == libc::SIGHUP {
            self.hung.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(child) = self.child.get() {
            let _ = child.kill(sig);
        }
    }
}

/// `argv` asked of this daemon as its CLI asks it (cli/client.rs `ask`), with `fds`: over its
/// socket, which the daemon's working directory, its home, holds; its status, and what it
/// said on stdout and stderr.
fn ask_self(
    argv: Vec<String>,
    daemon: shards_ipc::Identity,
    fds: &[std::os::fd::BorrowedFd<'_>],
) -> Result<(u8, Vec<u8>, Vec<u8>), String> {
    use shards_ipc::kind;
    let conn = std::os::unix::net::UnixStream::connect(shards_ipc::SOCKET)
        .map_err(|e| format!("this daemon's socket: {e}"))?;
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
    shards_ipc::send(&conn, kind::CONTAINER, &command.encode(), fds)
        .map_err(|e| format!("asking the daemon: {e}"))?;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    loop {
        match shards_ipc::recv(&conn) {
            Ok(Some(m)) if m.kind == kind::OUT => out.extend(m.payload),
            Ok(Some(m)) if m.kind == kind::ERR => err.extend(m.payload),
            Ok(Some(m)) if m.kind == kind::END => {
                return Ok((m.payload.first().copied().unwrap_or(1), out, err));
            }
            Ok(Some(m)) if m.kind == kind::RESTART => return Err("the daemon is shutting down".into()),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return Err("the daemon ended before it answered".into()),
        }
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

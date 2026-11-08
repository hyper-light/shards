//! A client's binds through its context (D215): `-v DIR:PATH` and `--mount type=bind` on
//! this machine, for a run on another, name this machine's directories, as a path names the
//! files where its user typed it. Each is served from here, by a share server of this
//! process's own over its own QUIC connection to the machine, whose run's share passes the
//! stream to its VM as it is: what the guest reads and writes are this machine's files, as
//! a bind's are its host's. The machine is told each as its relay's token
//! (`client_grants::Relayed`) and opens nothing of its own for it.

use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use shards_remote::client::{self, Controls};
use shards_remote::frame::Request;
use shards_remote::identity::Identity;

use crate::client_grants::Relayed;

/// A directory of this client's, or a file alone, a run there binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Served {
    pub relay: Relayed,
    pub read_only: bool,
}

/// A command's words as they are sent, what of this client's they bind, and whether its
/// binds are served apart from it ([`here_binds`]).
pub(crate) type Bound = (Vec<String>, Vec<Served>, bool);

/// A token a relay goes by: 128 random bits, as hex.
fn token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    shards_net::entropy(&mut bytes).map_err(|e| format!("a bind's token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `words` with each bind of a run's or create's sent as the relay that serves it from here,
/// and what is served, and whether the command leaves its microVM to run on without it
/// (`run -d`, `create`), so that the binds are served apart from it; or why not, in the
/// run's words, and whether the command is a run. A command that is no run or create, or
/// that shards' parser refuses, goes as it is.
pub(crate) fn here_binds(words: Vec<String>) -> Result<Bound, (bool, String)> {
    use shards_cmdline::commands::{CREATE, RUN};
    let said = shards_cmdline::grammar::rewrite(&words).unwrap_or_else(|| words.clone());
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    let (command, path, n) = match refs.as_slice() {
        ["run", ..] => (&RUN, "shards run", 1),
        ["container", "run", ..] => (&RUN, "shards container run", 2),
        ["create", ..] => (&CREATE, "shards create", 1),
        ["container", "create", ..] => (&CREATE, "shards container create", 2),
        _ => return Ok((words, Vec::new(), false)),
    };
    let run = std::ptr::eq(command, &RUN);
    let head = said.get(..n).map(<[String]>::to_vec).unwrap_or_default();
    let mut rest = said.get(n..).map(<[String]>::to_vec).unwrap_or_default();
    let shards_cmdline::flags::Outcome::Run(parsed) =
        shards_cmdline::flags::parse(command, path, &rest, &|_, value| Ok(value.to_string()))
    else {
        return Ok((words, Vec::new(), false));
    };
    let cwd = std::env::current_dir().map_err(|e| (run, format!("the working directory: {e}")))?;
    let mut served = Vec::new();
    for &(name, at, offset) in &parsed.positions {
        if name != "volume" && name != "mount" {
            continue;
        }
        let Some(word) = rest.get(at) else {
            continue;
        };
        let (flag, value) = (
            word.get(..offset).unwrap_or_default().to_string(),
            word.get(offset..).unwrap_or_default().to_string(),
        );
        let sent = match name {
            "volume" => volume_here(&value, &cwd, &mut served),
            _ => mount_here(&value, &cwd, &mut served),
        }
        .map_err(|e| (run, e))?;
        if let (Some(sent), Some(word)) = (sent, rest.get_mut(at)) {
            *word = format!("{flag}{sent}");
        }
    }
    let apart = !run || parsed.bool("detach");
    Ok((head.into_iter().chain(rest).collect(), served, apart))
}

/// A `-v` value as it is sent, where it binds a path of this client's: its source the relay
/// that serves it. A missing directory is made, as `-v` makes its host's (createSource).
fn volume_here(value: &str, cwd: &Path, served: &mut Vec<Served>) -> Result<Option<String>, String> {
    let Ok(spec) = shards_cmdline::mounts::parse_volume(value) else {
        return Ok(None);
    };
    if spec.kind != "bind" || spec.source.is_empty() {
        return Ok(None);
    }
    let Some(after) = value.strip_prefix(spec.source.as_str()) else {
        return Ok(None);
    };
    let here = cwd.join(&spec.source);
    if std::fs::symlink_metadata(&here).is_err() {
        std::fs::create_dir_all(&here)
            .map_err(|e| format!("error while creating mount source path '{}': {e}", here.display()))?;
    }
    let read_only = after
        .rsplit(':')
        .next()
        .is_some_and(|mode| after.matches(':').count() >= 2 && mode.split(',').any(|o| o == "ro"));
    let relay = relayed(&here)?;
    let source = relay.source();
    served.push(Served { relay, read_only });
    Ok(Some(format!("{source}{after}")))
}

/// A `--mount` value as it is sent, where it binds a path of this client's: its source the
/// relay that serves it, as docker/cli would have its daemon refuse one not there.
fn mount_here(value: &str, cwd: &Path, served: &mut Vec<Served>) -> Result<Option<String>, String> {
    let Ok(mut m) = shards_cmdline::mounts::parse_mount(value, Some(cwd)) else {
        return Ok(None);
    };
    if m.kind != "bind" || m.source.is_empty() {
        return Ok(None);
    }
    let here = cwd.join(&m.source);
    let creates = m.bind.as_ref().is_some_and(|b| b.create_mountpoint);
    if std::fs::symlink_metadata(&here).is_err() {
        if !creates {
            return Err(format!(
                "invalid mount config for type \"bind\": bind source path does not exist: {}",
                here.display()
            ));
        }
        std::fs::create_dir_all(&here)
            .map_err(|e| format!("error while creating mount source path '{}': {e}", here.display()))?;
    }
    let relay = relayed(&here)?;
    m.source = relay.source();
    served.push(Served {
        relay,
        read_only: m.read_only,
    });
    Ok(Some(shards_cmdline::mounts::encode(&m)))
}

/// The relay that serves `here`, a path of this client's: a directory, or a file alone, as
/// the file its links lead to, as a bind mount takes it.
fn relayed(here: &Path) -> Result<Relayed, String> {
    let meta = std::fs::metadata(here).map_err(|e| format!("{}: {e}", here.display()))?;
    let here = if meta.is_dir() {
        here.to_path_buf()
    } else {
        std::fs::canonicalize(here).map_err(|e| format!("{}: {e}", here.display()))?
    };
    Ok(Relayed {
        token: token()?,
        here: here
            .to_str()
            .ok_or_else(|| {
                format!(
                    "{}: a path that is not UTF-8 does not cross to another machine",
                    here.display()
                )
            })?
            .to_string(),
        file: !meta.is_dir(),
    })
}

/// The share server of `served`: of its directory, or of a file's directory limited to it,
/// telling its guest on the machine to map nothing of it (`Server::serves_afar`).
fn server_of(
    served: &Served,
    budget: u64,
) -> Result<shards_vmm::devices::virtio::fs::server::Server, String> {
    let here = PathBuf::from(&served.relay.here);
    let (dir, only) = if served.relay.file {
        let parent = here
            .parent()
            .ok_or_else(|| format!("{}: no directory", here.display()))?;
        let name = std::ffi::CString::new(served.relay.only())
            .map_err(|_| format!("{}: its name", here.display()))?;
        (parent.to_path_buf(), Some(name))
    } else {
        (here.clone(), None)
    };
    let opened = shards_vmm::platform::open_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let server = shards_vmm::devices::virtio::fs::server::Server::with_budget(
        opened.into(),
        served.read_only,
        only,
        budget,
    )
    .map_err(|e| format!("{}: {e}", here.display()))?;
    server.serves_afar();
    Ok(server)
}

/// The binds a command serves while it runs, each on a thread of its own, until each relay
/// ends: as the run that took it ends, or as [`Relays::hang_up`] lets the rest go.
pub(crate) struct Relays {
    controls: Vec<Controls>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Relays {
    /// Serves `served` for the run there that names their tokens, once each relay is kept on
    /// `remote`, the machine: or why one is not.
    pub(crate) fn start(home: &Path, remote: client::Remote, served: &[Served]) -> Result<Relays, String> {
        // As far as its limit may go, as a share process raises its own.
        let _ = shards_vmm::platform::raise_descriptor_limit();
        let limits = shards_vmm::devices::virtio::fs::server::Limits::now()
            .ok_or("this process's limit on descriptors is unknown")?;
        let budget = limits.budget(u64::try_from(served.len()).unwrap_or(u64::MAX));
        let mut relays = Relays {
            controls: Vec::new(),
            threads: Vec::new(),
        };
        let mut waits = Vec::new();
        for s in served {
            let server = server_of(s, budget)?;
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
            let (steer_tx, steer_rx) = std::sync::mpsc::channel::<Controls>();
            let (token, here) = (s.relay.token.clone(), s.relay.here.clone());
            let identity = crate::context::identity(home)?;
            let thread = std::thread::Builder::new()
                .name("context-bind".into())
                .spawn(move || serve(&identity, remote, &token, &here, &server, ready_tx, steer_tx))
                .map_err(|e| format!("a thread for {}: {e}", s.relay.here))?;
            relays.threads.push(thread);
            waits.push((ready_rx, steer_rx));
        }
        for (ready, steer) in waits {
            match ready.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    relays.hang_up();
                    return Err(e);
                }
                Err(_) => {
                    relays.hang_up();
                    return Err("a bind's relay ended before it was kept".into());
                }
            }
            if let Ok(c) = steer.recv() {
                relays.controls.push(c);
            }
        }
        Ok(relays)
    }

    /// Lets every relay go, and waits for its thread: a run that never took one leaves it
    /// kept there no longer.
    pub(crate) fn hang_up(&mut self) {
        for c in self.controls.drain(..) {
            let _ = c.hang_up();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }

    /// Waits for every relay to end, as the runs that took them do.
    pub(crate) fn wait(mut self) {
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Relays {
    /// What its command had no more of is let go, as the command ends.
    fn drop(&mut self) {
        self.hang_up();
    }
}

/// Serves directory `here` as `token` on `remote`'s relay with `server` until it ends:
/// `ready` told once the machine keeps it, or why not; `steer` given what hangs it up.
fn serve(
    identity: &Identity,
    remote: client::Remote,
    token: &str,
    here: &str,
    server: &shards_vmm::devices::virtio::fs::server::Server,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
    steer: std::sync::mpsc::Sender<Controls>,
) {
    let (Ok((mut asked, asking)), Ok((answers, answering))) = (io::pipe(), io::pipe()) else {
        let _ = ready.send(Err(format!("{here}: pipes for its relay")));
        return;
    };
    let request = Request {
        args: [crate::client_grants::BIND_STEP, token]
            .iter()
            .map(|w| w.as_bytes().to_vec())
            .collect(),
        env: Vec::new(),
        terminal: None,
    };
    let told = ready.clone();
    std::thread::scope(|scope| {
        let answered = std::thread::Builder::new()
            .name("context-bind-answers".into())
            .spawn_scoped(scope, move || {
                let mut first = [0u8; 1];
                match asked.read_exact(&mut first) {
                    Ok(()) if first[0] == crate::client_grants::RELAY_READY => {
                        let _ = told.send(Ok(()));
                    }
                    _ => return,
                }
                let _ = shards_vmm::devices::virtio::fs::answer_afar(server, asked, answering);
            });
        let (err_tx, err_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let ran = client::exec(
            identity,
            remote,
            request,
            Some(Box::new(answers)),
            asking,
            Said(err_tx),
            |controls| {
                let _ = steer.send(controls);
            },
        );
        let said: Vec<u8> = err_rx.try_iter().flatten().collect();
        let why = match ran {
            Ok(Ok(_)) => String::from_utf8_lossy(&said).trim().to_string(),
            Ok(Err(refused)) => refused,
            Err(e) => e.to_string(),
        };
        let _ = ready.send(Err(format!("serving {here}: {why}")));
        if let Ok(answered) = answered {
            let _ = answered.join();
        }
    });
}

/// What a relay's step says on stderr, taken whole once it is through.
struct Said(std::sync::mpsc::Sender<Vec<u8>>);

impl io::Write for Said {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let _ = self.0.send(bytes.to_vec());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The word `shards` runs [`serve_apart`] by: not for people to run (`cli::INTERNAL`).
pub(crate) const SERVE_APART: &str = "bind-server";

/// Starts a process of this binary's that serves `served` for a microVM of `context`'s
/// that runs on without its command (`run -d`, `create`), until each relay ends as the run
/// that took it does, and waits until it says each is kept there, or why not.
pub(crate) fn start_apart(context: &str, served: &[Served]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
    let mut args: Vec<String> = vec![SERVE_APART.into(), context.into()];
    for s in served {
        args.push(s.relay.source());
        args.push(if s.read_only { "ro" } else { "rw" }.into());
    }
    let (mut said, says) = io::pipe().map_err(|e| format!("a pipe for the binds' server: {e}"))?;
    let argv: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
    {
        use shards_ipc::fd::AsFd as _;
        let fds = [(says.as_fd(), 1)];
        // Left to run on its own: it ends as its relays do.
        let _ = shards_ipc::spawn(&exe, &argv, &fds, true)
            .map_err(|e| format!("starting the binds' server: {e}"))?;
    }
    drop(says);
    // Its first line: it holds its stdout open as long as it serves.
    let mut answer = String::new();
    io::BufRead::read_line(&mut io::BufReader::new(&mut said), &mut answer)
        .map_err(|e| format!("the binds' server: {e}"))?;
    match answer.trim_end() {
        "ok" => Ok(()),
        "" => Err("the binds' server ended before it served them".into()),
        why => Err(why.to_string()),
    }
}

/// `shards bind-server CONTEXT (SOURCE MODE)...`: [`start_apart`]'s process, which says `ok`
/// on a line of its stdout once every relay is kept there, or why not, and serves until each
/// relay ends.
pub(crate) fn serve_apart(args: impl Iterator<Item = std::ffi::OsString>) -> std::process::ExitCode {
    use std::io::Write as _;
    let args: Vec<String> = args.filter_map(|a| a.into_string().ok()).collect();
    let mut out = Some(io::stdout());
    let fail = |out: &mut Option<io::Stdout>, why: String| {
        if let Some(mut o) = out.take() {
            let _ = writeln!(o, "{}", why.replace('\n', " "));
        }
        std::process::ExitCode::FAILURE
    };
    let Some((context, rest)) = args.split_first() else {
        return fail(&mut out, "bind-server: its context".into());
    };
    let mut served = Vec::new();
    for pair in rest.chunks(2) {
        let (Some(source), Some(mode)) = (pair.first(), pair.get(1)) else {
            return fail(&mut out, "bind-server: each bind takes a mode".into());
        };
        let Some(relay) = crate::client_grants::relay_of(source) else {
            return fail(&mut out, format!("bind-server: {source:?} is no bind's"));
        };
        served.push(Served {
            relay,
            read_only: mode == "ro",
        });
    }
    let started = shards_ipc::home().and_then(|home| {
        let context =
            crate::context::load(&home, context)?.ok_or_else(|| format!("no context {context:?}"))?;
        Relays::start(&home, context.remote(), &served)
    });
    match started {
        Ok(relays) => {
            if let Some(mut o) = out.take() {
                let _ = writeln!(o, "ok");
                let _ = o.flush();
            }
            // Its stdout closed for good: what it was told on is its starter's alone.
            relays.wait();
            std::process::ExitCode::SUCCESS
        }
        Err(e) => fail(&mut out, e),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A run's binds of this client's paths are sent as relays, a missing `-v` directory
    /// made and a missing `--mount` source refused as docker/cli's daemon refuses it; its
    /// volumes, anonymous volumes and tmpfs go as they are; and a detached run or a create
    /// serves its binds apart from the command.
    #[cfg(unix)]
    #[test]
    fn a_runs_binds_are_served_from_the_client() {
        let at = shards_testdir::TempDir::new("client-binds").unwrap();
        let dir = at.join("src");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(at.join("x.conf"), b"x").unwrap();
        let w = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let missing = at.join("made");
        let (sent, served, apart) = here_binds(w(&[
            "run",
            "-v",
            &format!("{}:/src:ro", dir.display()),
            "-v",
            "data:/data",
            "-v",
            "/anon",
            &format!("--volume={}:/made", missing.display()),
            "--mount",
            &format!(
                "type=bind,src={},dst=/x.conf,readonly",
                at.join("x.conf").display()
            ),
            "--tmpfs",
            "/t",
            "alpine",
        ]))
        .unwrap();
        assert!(!apart);
        assert_eq!(served.len(), 3);
        assert!(missing.is_dir(), "-v makes its source");
        assert!(served[0].read_only && !served[0].relay.file);
        assert_eq!(served[0].relay.here, dir.to_str().unwrap());
        assert!(!served[1].read_only);
        assert!(served[2].read_only && served[2].relay.file);
        assert_eq!(sent[2], format!("{}:/src:ro", served[0].relay.source()));
        assert_eq!(&sent[3..7], ["-v", "data:/data", "-v", "/anon"].map(String::from));
        assert_eq!(sent[7], format!("--volume={}:/made", served[1].relay.source()));
        assert!(sent[9].contains(&served[2].relay.source()), "{}", sent[9]);
        assert!(!sent.iter().any(|w| w.contains(at.to_str().unwrap())));
        let (_, _, apart) = here_binds(w(&[
            "run",
            "-d",
            "-v",
            &format!("{}:/src", dir.display()),
            "alpine",
        ]))
        .unwrap();
        assert!(apart);
        let (_, _, apart) =
            here_binds(w(&["create", "-v", &format!("{}:/src", dir.display()), "alpine"])).unwrap();
        assert!(apart);
        let refused = here_binds(w(&[
            "run",
            "--mount",
            &format!("type=bind,src={},dst=/m", at.join("none").display()),
            "alpine",
        ]))
        .unwrap_err();
        assert!(
            refused.0 && refused.1.contains("bind source path does not exist"),
            "{refused:?}"
        );
        let (sent, served, _) = here_binds(w(&["ps", "-a"])).unwrap();
        assert!(served.is_empty() && sent == w(&["ps", "-a"]));
    }
}

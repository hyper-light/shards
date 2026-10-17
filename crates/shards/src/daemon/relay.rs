//! Docker's tools through a context (D180 part 4): each context this home joined has an
//! engine socket of its own in the home (`e/KEY.sock`, KEY its name hashed, so that a path
//! of any name fits a socket address), which this daemon serves while it runs, polled and
//! drained with its own
//! sockets. A connection to it is relayed whole to the Engine API (D170) of the context's
//! machine, a QUIC stream each, as `DOCKER_HOST` reaches a daemon: what a client of
//! Docker's API asks there, it asks of that machine.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Instant;

use super::{Daemon, Disk, Threads, lock, log};

/// A context's engine socket, as it is served.
struct Relay {
    listener: UnixListener,
    context: crate::context::Context,
    path: PathBuf,
    /// The socket file bound, by device and inode: one removed, or another put at its
    /// path, is this listener's no more.
    file: (u64, u64),
}

impl Relay {
    fn bind(context: crate::context::Context) -> io::Result<Relay> {
        let path = crate::context::engine_socket(&context.name);
        if let Some(dir) = path.parent() {
            shards_vmm::platform::create_private_dir(dir)?;
        }
        // In place of one a daemon before this one left, which no process serves.
        let listener = super::listening_at(&path)?;
        let bound = std::fs::symlink_metadata(&path)?;
        Ok(Relay {
            listener,
            context,
            path,
            file: (bound.dev(), bound.ino()),
        })
    }

    /// Whether the socket file at its path is still the one it bound.
    fn bound(&self) -> bool {
        std::fs::symlink_metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.file)
    }

    /// Removes its socket file, where it is still its own.
    fn unlink(&self) {
        if self.bound() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The engine sockets this daemon serves, by context, and the contexts whose socket it
/// could not bind, said once until it can.
#[derive(Default)]
pub(super) struct Relays {
    served: BTreeMap<String, Relay>,
    unbound: BTreeSet<String>,
}

impl Relays {
    pub(super) fn is_empty(&self) -> bool {
        self.served.is_empty()
    }

    /// The listeners' descriptors, for poll(2).
    pub(super) fn fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.served.values().map(|r| r.listener.as_raw_fd())
    }
}

/// What the machine's answer did on the local connection.
enum Said {
    /// Its first bytes were written.
    Answered,
    /// The client had gone.
    HungUp,
}

/// The local connection's writing half, which says when it has answered, and when its
/// client has gone.
struct Answering {
    conn: UnixStream,
    answered: bool,
    said: mpsc::Sender<Said>,
}

impl io::Write for Answering {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.conn.write(bytes);
        match &written {
            Ok(_) if !self.answered => {
                self.answered = true;
                let _ = self.said.send(Said::Answered);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected
                ) =>
            {
                let _ = self.said.send(Said::HungUp);
            }
            _ => {}
        }
        written
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.flush()
    }
}

impl<D: Disk> Daemon<D> {
    /// Serves the engine socket of each context this home joined and lets go of those of
    /// contexts removed, where the contexts may have changed (`listed`); and binds again a
    /// socket someone removed, or that a context's removal took before it was made again.
    pub(super) fn relay_contexts(&self, relays: &mut Relays, listed: bool) {
        if listed {
            match crate::context::list(&self.home) {
                Ok(contexts) => {
                    let names: BTreeSet<&str> = contexts.iter().map(|c| c.name.as_str()).collect();
                    relays.served.retain(|name, relay| {
                        let joined = names.contains(name.as_str());
                        if !joined {
                            relay.unlink();
                        }
                        joined
                    });
                    relays.unbound.retain(|name| names.contains(name.as_str()));
                    for context in contexts {
                        match relays.served.get_mut(&context.name) {
                            Some(relay) => relay.context = context,
                            None => self.serve_relay(relays, context),
                        }
                    }
                }
                Err(e) => log(format!("the contexts this home joined: {e}")),
            }
        }
        let lost: Vec<String> = relays
            .served
            .iter()
            .filter(|(_, relay)| !relay.bound())
            .map(|(name, _)| name.clone())
            .collect();
        for name in lost {
            if let Some(relay) = relays.served.remove(&name) {
                log(format!(
                    "context {name}: {} was removed; listening there again",
                    self.home.join(&relay.path).display()
                ));
                self.serve_relay(relays, relay.context);
            }
        }
    }

    fn serve_relay(&self, relays: &mut Relays, context: crate::context::Context) {
        let name = context.name.clone();
        match Relay::bind(context) {
            Ok(relay) => {
                relays.unbound.remove(&name);
                relays.served.insert(name, relay);
            }
            Err(e) => {
                if relays.unbound.insert(name.clone()) {
                    let path = crate::context::engine_socket(&name);
                    log(format!(
                        "context {name}: {}: {e}; its engine is not served",
                        self.home.join(path).display()
                    ));
                }
            }
        }
    }

    /// Relays each client waiting at a context's engine socket: false where one could not be
    /// accepted now (out of descriptors), which the listener waits a while for.
    pub(super) fn relay_waiting<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, relays: &Relays) -> bool {
        let mut ready = true;
        for relay in relays.served.values() {
            loop {
                match relay.listener.accept() {
                    Ok((conn, _)) => self.relay(threads, conn, relay.context.clone()),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log(format!(
                            "context {}: accepting its engine's client: {e}",
                            relay.context.name
                        ));
                        ready = false;
                        break;
                    }
                }
            }
        }
        ready
    }

    /// As the daemon closes: no context's socket takes a client from here on, and those that
    /// reached one already are relayed, as those at its own sockets are served.
    pub(super) fn close_relays<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, relays: &mut Relays) {
        if relays.served.is_empty() {
            return;
        }
        for relay in relays.served.values() {
            relay.unlink();
        }
        let _ = self.relay_waiting(threads, relays);
        relays.served.clear();
    }

    /// Relays `conn` to `context`'s machine's Engine API: what its client writes, as a
    /// command's stdin, and the machine's answer, as its stdout. Only this daemon's own
    /// user's clients, as its own Engine API serves only them; ended with the API's clients
    /// as the daemon stops.
    fn relay<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: UnixStream,
        context: crate::context::Context,
    ) {
        let name = context.name.clone();
        // SAFETY: geteuid(2) cannot fail.
        let ours = unsafe { libc::geteuid() };
        match shards_ipc::peer_uid(&conn) {
            Ok(uid) if uid == ours => {}
            Ok(uid) => {
                log(format!("context {name}: refused an engine client of user {uid}"));
                return;
            }
            Err(e) => {
                log(format!("context {name}: an engine client's credentials: {e}"));
                return;
            }
        }
        // Accepted from a listener that does not block, it would not either on macOS
        // (accept(2): it takes the listener's O_NONBLOCK).
        let cloned = conn
            .set_nonblocking(false)
            .and_then(|()| Ok((conn.try_clone()?, conn.try_clone()?, conn.try_clone()?)));
        let (read, write, held) = match cloned {
            Ok(cloned) => cloned,
            Err(e) => {
                log(format!("context {name}: an engine client's connection: {e}"));
                return;
            }
        };
        let identity = match crate::context::identity(&self.home) {
            Ok(identity) => identity,
            Err(e) => {
                log(format!("context {name}: {e}"));
                return;
            }
        };
        let number = self.engine.hold(held);
        let done = move || {
            self.engine.let_go(number);
            *lock(&self.last) = Instant::now();
            self.wake_listener();
        };
        let spawned = std::thread::Builder::new()
            .name("relay".into())
            .spawn_scoped(threads, move || {
                let request = shards_remote::frame::Request {
                    args: vec![crate::context::ENGINE_STEP.as_bytes().to_vec()],
                    env: Vec::new(),
                    terminal: None,
                };
                let (said_tx, said) = mpsc::channel();
                let answering = Answering {
                    conn: write,
                    answered: false,
                    said: said_tx,
                };
                // Hung up as the daemon stops, which ends its client's connection here.
                let steered = |controls| self.engine.hold_relayed(number, controls, &self.stopping);
                let ran = shards_remote::client::exec(
                    &identity,
                    context.remote(),
                    request,
                    Some(Box::new(read)),
                    answering,
                    io::sink(),
                    steered,
                );
                let said: Vec<Said> = said.try_iter().collect();
                let why = match ran {
                    Ok(Ok(_)) => None,
                    Ok(Err(refused)) => Some(refused),
                    Err(e) => Some(e.to_string()),
                };
                match why {
                    // A client gone, or ended as the daemon stops, is nobody's to tell.
                    Some(_) if self.stopping.load(Ordering::SeqCst) => {}
                    Some(_) if said.iter().any(|s| matches!(s, Said::HungUp)) => {}
                    // Asked and not answered: told why, as an answer of the API's.
                    Some(why) if said.is_empty() => {
                        let why = format!("context {}: {why}", context.name);
                        if let Err(e) = super::engine::unanswered(&conn, &why) {
                            log(format!("{why}; telling its client: {e}"));
                        }
                    }
                    Some(why) => log(format!("context {}: its engine: {why}", context.name)),
                    None => {}
                }
                // Its client reads the end, and the thread reading it ends.
                shards_ipc::end(&conn);
                done();
            });
        if let Err(e) = spawned {
            done();
            log(format!("context {name}: a thread for an engine client: {e}"));
        }
    }
}

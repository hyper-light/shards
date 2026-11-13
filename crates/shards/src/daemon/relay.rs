//! Docker's tools through a context (D180 part 4): each context this home joined has an
//! engine socket of its own in the home (`e/KEY.sock`, KEY its name hashed, so that a path
//! of any name fits a socket address), which this daemon serves while it runs, polled and
//! drained with its own
//! sockets. A connection to it is relayed whole to the Engine API (D170) of the context's
//! machine, a QUIC stream each, as `DOCKER_HOST` reaches a daemon: what a client of
//! Docker's API asks there, it asks of that machine.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd as _, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Instant;

use shards_ipc::Stream as UnixStream;

use super::{ApiListener, Daemon, Disk, Threads, lock, log};

/// A context's engine socket, as it is served: a listener as the Engine API's is
/// (`ApiListener`), on Windows a named pipe the file at its path names, whose clients its
/// [`Taker`] takes.
struct Relay {
    #[cfg(unix)]
    listener: ApiListener,
    #[cfg(windows)]
    _taker: Taker,
    path: PathBuf,
    /// The socket file bound, by device and inode: one removed, or another put at its
    /// path, is this listener's no more.
    file: (u64, u64),
}

impl Relay {
    /// Context `name`'s socket, bound as its record in `home` is now; none where the record
    /// is gone. Bound under the contexts' lock, which a removal takes the record and then
    /// the socket under (context.rs, `remove`): a socket is never bound after a removal
    /// returned, as one bound for a record read before it would be.
    fn bind(home: &Path, name: &str, #[cfg(windows)] came: Came) -> io::Result<Option<Relay>> {
        let (_locked, Some(context)) = crate::context::joined(home, name)? else {
            return Ok(None);
        };
        let path = crate::context::engine_socket(&context.name);
        if let Some(dir) = path.parent() {
            shards_vmm::platform::create_private_dir(dir)?;
        }
        // In place of one a daemon before this one left, which no process serves.
        let listener = super::listening_at(&path)?;
        let file = crate::os::path_id(&path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "its socket, bound, is gone"))?;
        Ok(Some(Relay {
            #[cfg(windows)]
            _taker: Taker::start(listener, context.name.clone(), came)?,
            #[cfg(unix)]
            listener,
            path,
            file,
        }))
    }

    /// Whether the socket file at its path is still the one it bound.
    fn bound(&self) -> bool {
        crate::os::path_id(&self.path) == Some(self.file)
    }

    /// Removes its socket file, where it is still its own.
    fn unlink(&self) {
        if self.bound() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The engine sockets this daemon serves, by context, and the contexts whose socket it
/// could not bind, said once until it can; on Windows, the clients its takers took, by
/// context, for the daemon to relay.
#[cfg_attr(unix, derive(Default))]
pub(super) struct Relays {
    served: BTreeMap<String, Relay>,
    unbound: BTreeSet<String>,
    #[cfg(windows)]
    came: (mpsc::Sender<Taken>, mpsc::Receiver<Taken>),
}

/// A client a context's taker took on Windows ([`Taker`]): the context's name, and its
/// connection.
#[cfg(windows)]
type Taken = (String, UnixStream);

#[cfg(windows)]
impl Default for Relays {
    fn default() -> Relays {
        Relays {
            served: BTreeMap::new(),
            unbound: BTreeSet::new(),
            came: mpsc::channel(),
        }
    }
}

impl Relays {
    pub(super) fn is_empty(&self) -> bool {
        self.served.is_empty()
    }

    /// The listeners' descriptors, for poll(2).
    #[cfg(unix)]
    pub(super) fn fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.served.values().map(|r| r.listener.as_raw_fd())
    }
}

/// Where a context's [`Taker`] hands each client it takes on Windows, by the context's
/// name, and what it wakes the daemon by.
#[cfg(windows)]
struct Came {
    to: mpsc::Sender<Taken>,
    wake: shards_ipc::Event,
}

/// The thread that takes the clients of a context's engine pipe on Windows, each handed to
/// the daemon ([`Came`]) as it comes: the daemon waits on no context's listener, as
/// WaitForMultipleObjects waits on 64 handles at most and a home joins as many contexts as
/// it is told. Stopped as this drops, its listener with it.
#[cfg(windows)]
struct Taker {
    stop: shards_ipc::Event,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl Taker {
    fn start(listener: ApiListener, name: String, came: Came) -> io::Result<Taker> {
        use windows_sys::Win32::Foundation::WAIT_FAILED;
        use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects, WaitForSingleObject};
        let stop = shards_ipc::Event::new()?;
        let stopped = stop.try_clone()?;
        let thread = std::thread::Builder::new()
            .name("relay-taker".into())
            .spawn(move || {
                let waited = [listener.event(), stopped.raw()];
                loop {
                    // SAFETY: two events this thread holds, the listener's and its stop.
                    if unsafe { WaitForMultipleObjects(2, waited.as_ptr(), 0, INFINITE) } == WAIT_FAILED
                        || stopped.is_set()
                    {
                        return;
                    }
                    loop {
                        match listener.accept() {
                            Ok(conn) => {
                                if came.to.send((name.clone(), conn)).is_err() {
                                    return;
                                }
                                came.wake.set();
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            // Out of handles, say, the client is not taken and its listener's
                            // event stays set: refused, its instance let go of and waiting for
                            // the next (`Listener::refuse`, which makes no handle), as the
                            // daemon's own listener refuses one (super::refuse_api).
                            Err(e) => {
                                log(format!(
                                    "context {name}: accepting its engine's client: {e}; refused"
                                ));
                                if let Err(e) = listener.refuse() {
                                    log(format!(
                                        "context {name}: refusing its engine's client: {e}; no more taken"
                                    ));
                                    // SAFETY: its stop event, which this thread holds.
                                    unsafe { WaitForSingleObject(stopped.raw(), INFINITE) };
                                    return;
                                }
                            }
                        }
                    }
                }
            })?;
        Ok(Taker {
            stop,
            thread: Some(thread),
        })
    }
}

#[cfg(windows)]
impl Drop for Taker {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
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
                        if !relays.served.contains_key(&context.name) {
                            self.serve_relay(relays, context.name);
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
            if let Some(relay) = relays.served.remove(&name)
                && self.serve_relay(relays, name.clone())
            {
                log(format!(
                    "context {name}: {} was removed; listening there again",
                    self.home.join(&relay.path).display()
                ));
            }
        }
    }

    /// Serves context `name`'s engine socket, as its record is now ([`Relay::bind`]):
    /// whether it does. Not where the record is gone: removed since a listing read it, or
    /// since its socket was bound, whose removal went with it.
    fn serve_relay(&self, relays: &mut Relays, name: String) -> bool {
        #[cfg(windows)]
        let came = match self.listener_wake.event().try_clone() {
            Ok(wake) => Came {
                to: relays.came.0.clone(),
                wake,
            },
            Err(e) => {
                log(format!("context {name}: its engine is not served: {e}"));
                return false;
            }
        };
        match Relay::bind(
            &self.home,
            &name,
            #[cfg(windows)]
            came,
        ) {
            Ok(Some(relay)) => {
                relays.unbound.remove(&name);
                relays.served.insert(name, relay);
                true
            }
            Ok(None) => {
                relays.unbound.remove(&name);
                false
            }
            Err(e) => {
                if relays.unbound.insert(name.clone()) {
                    let path = crate::context::engine_socket(&name);
                    log(format!(
                        "context {name}: {}: {e}; its engine is not served",
                        self.home.join(path).display()
                    ));
                }
                false
            }
        }
    }

    /// Relays each client waiting at a context's engine pipe: on Windows each context's taker
    /// took its clients, relayed as the context they came to is now, where it is still
    /// served.
    #[cfg(windows)]
    pub(super) fn relay_waiting<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        relays: &Relays,
        _spare: &mut super::Reserve,
    ) -> bool {
        while let Ok((name, conn)) = relays.came.1.try_recv() {
            if relays.served.contains_key(&name) {
                self.relay(threads, conn, name);
            }
        }
        true
    }

    /// Relays each client waiting at a context's engine socket, of the listeners poll(2) said
    /// have clients. One no descriptor is left for is refused with `spare`, as the Engine
    /// API's is (`super::refuse_api`): false where even that could not be done, so that the
    /// listener waits on them no more until it is woken.
    #[cfg(unix)]
    pub(super) fn relay_waiting<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        relays: &Relays,
        spare: &mut super::Reserve,
    ) -> bool {
        for (name, relay) in &relays.served {
            loop {
                match super::accept_api(&relay.listener) {
                    Ok(conn) => self.relay(threads, conn, name.clone()),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        log(format!(
                            "context {name}: accepting its engine's client: {e}; refused"
                        ));
                        if !super::refuse_api(&relay.listener, spare) {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }

    /// As the daemon closes: no context's socket takes a client from here on, and those that
    /// reached one already are relayed, as those at its own sockets are served.
    pub(super) fn close_relays<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        relays: &mut Relays,
        spare: &mut super::Reserve,
    ) {
        if relays.served.is_empty() {
            return;
        }
        for relay in relays.served.values() {
            relay.unlink();
        }
        let _ = self.relay_waiting(threads, relays, spare);
        relays.served.clear();
    }

    /// Relays `conn` to context `name`'s machine's Engine API: what its client writes, as a
    /// command's stdin, and the machine's answer, as its stdout. Only this daemon's own
    /// user's clients, as its own Engine API serves only them; ended with the API's clients
    /// as the daemon stops.
    fn relay<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, conn: UnixStream, name: String) {
        // On Windows a relay's listener is a named pipe whose DACL admits this user's
        // processes alone (ipc pipe.rs `attributes`), as the Engine API's is.
        // SAFETY: geteuid(2) cannot fail.
        #[cfg(unix)]
        let ours = unsafe { libc::geteuid() };
        #[cfg(unix)]
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
        // Admitted as its context's record is now, read under the contexts' lock, and
        // refused, closed, where a removal took it (context.rs, `remove`): nothing reaches
        // the machine through a context once its removal returned, not a client that came
        // to its socket before it went, nor, on Windows, one that came to the pipe it names,
        // which listens on until the daemon next looks at the contexts.
        let context = match crate::context::joined(&self.home, &name) {
            Ok((_, Some(context))) => context,
            Ok((_, None)) => {
                log(format!("context {name}: removed; refused an engine client"));
                return;
            }
            Err(e) => {
                log(format!("context {name}: an engine client's context: {e}"));
                return;
            }
        };
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
                // Its handshake and waits start from what the home knows of the machine's path,
                // and what the connection saw is kept for the next (D180). A machine never
                // reached is waited for with no deadline of shards' own, each probe logged, and
                // the relay hung up from its start as the daemon stops, which ends its client's
                // connection here.
                let before = crate::context::known(&self.home, &context);
                let told = |unanswered: shards_remote::client::Unanswered| {
                    log(format!(
                        "context {}: no answer yet from {}: {} probes over {:.1?}",
                        context.name, context.address, unanswered.probes, unanswered.waited
                    ));
                    true
                };
                let held = |hang_up| self.engine.hold_relayed(number, hang_up, &self.stopping);
                let (ran, seen) = shards_remote::client::exec_with(
                    &identity,
                    context.remote(before),
                    request,
                    Some(Box::new(read)),
                    answering,
                    io::sink(),
                    |_| {},
                    |_| {},
                    told,
                    held,
                );
                if let Err(e) = crate::context::keep(&self.home, &context, before, &seen) {
                    log(format!("context {}: {e}", context.name));
                }
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

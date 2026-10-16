//! Docker's tools through a context (D180 part 4): each context this home joined has an
//! engine socket of its own in the home (`e/KEY.sock`, KEY its name hashed, as Docker's
//! own context store names a context's directory, so that a path of any name fits a socket
//! address), which this daemon serves while it runs. A connection to it is relayed whole to
//! the Engine API (D170) of the context's machine, a QUIC stream each, as `DOCKER_HOST`
//! reaches a daemon: what a client of Docker's API asks there, it asks of that machine.

use std::collections::BTreeSet;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::{Daemon, Disk, Threads, lock, log};

/// The contexts whose engine sockets this daemon serves, by name.
#[derive(Default)]
pub(super) struct Relays(Mutex<BTreeSet<String>>);

/// The local connection's writing half: closed as the machine's answer ends, so that its
/// client reads the end.
struct Answering(UnixStream);

impl io::Write for Answering {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Write);
    }
}

impl<D: Disk> Daemon<D> {
    /// Serves the engine socket of each context this home joined that it does not serve yet,
    /// a thread each that takes its connections.
    pub(super) fn relay_contexts<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let Ok(contexts) = crate::context::list(&self.home) else {
            return;
        };
        for context in contexts {
            if !lock(&self.relays.0).insert(context.name.clone()) {
                continue;
            }
            let path = crate::context::engine_socket(&context.name);
            let bound = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| {
                    let _ = std::fs::remove_file(&path);
                    UnixListener::bind(&path)
                });
            let listener = match bound {
                Ok(listener) => listener,
                Err(e) => {
                    log(format!(
                        "context {}: {}: {e}; its engine is not relayed",
                        context.name,
                        self.home.join(&path).display()
                    ));
                    continue;
                }
            };
            let name = context.name.clone();
            let spawned = std::thread::Builder::new()
                .name("relay-listen".into())
                .spawn_scoped(threads, move || self.relay_listen(threads, &listener, &name));
            if let Err(e) = spawned {
                log(format!("context {}: a thread for its engine: {e}", context.name));
                lock(&self.relays.0).remove(&context.name);
            }
        }
    }

    /// Takes the connections to context `name`'s engine socket, each relayed on a thread of
    /// its own, until the context is gone: its removal connects here once, so that this
    /// thread sees it and lets go of its socket.
    fn relay_listen<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, listener: &UnixListener, name: &str) {
        loop {
            match listener.accept() {
                Ok((conn, _)) => {
                    let Ok(Some(context)) = crate::context::load(&self.home, name) else {
                        break;
                    };
                    self.relay(threads, conn, context);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    log(format!("context {name}: accepting its engine's client: {e}"));
                    break;
                }
            }
        }
        lock(&self.relays.0).remove(name);
    }

    /// Relays `conn` to `context`'s machine's Engine API: what its client writes, as a
    /// command's stdin, and the machine's answer, as its stdout. Only this daemon's own
    /// user's clients, as its own Engine API serves only them.
    fn relay<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: UnixStream,
        context: crate::context::Context,
    ) {
        // SAFETY: geteuid(2) cannot fail.
        let ours = unsafe { libc::geteuid() };
        match shards_ipc::peer_uid(&conn) {
            Ok(uid) if uid == ours => {}
            Ok(uid) => {
                log(format!(
                    "context {}: refused an engine client of user {uid}",
                    context.name
                ));
                return;
            }
            Err(e) => {
                log(format!(
                    "context {}: an engine client's credentials: {e}",
                    context.name
                ));
                return;
            }
        }
        let identity = match crate::context::identity(&self.home) {
            Ok(identity) => identity,
            Err(e) => {
                log(format!("context {}: {e}", context.name));
                return;
            }
        };
        let (read, write) = match conn.try_clone() {
            Ok(read) => (read, Answering(conn)),
            Err(e) => {
                log(format!(
                    "context {}: an engine client's connection: {e}",
                    context.name
                ));
                return;
            }
        };
        let name = context.name.clone();
        self.engine.open.fetch_add(1, Ordering::SeqCst);
        let done = move || {
            self.engine.open.fetch_sub(1, Ordering::SeqCst);
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
                let ran = shards_remote::client::exec(
                    &identity,
                    context.remote(),
                    request,
                    Some(Box::new(read)),
                    write,
                    io::sink(),
                    |_| {},
                );
                match ran {
                    Ok(Ok(_)) => {}
                    Ok(Err(refused)) => log(format!("context {}: its engine: {refused}", context.name)),
                    Err(e) => log(format!("context {}: its engine: {e}", context.name)),
                }
                done();
            });
        if let Err(e) = spawned {
            log(format!("context {name}: a thread for an engine client: {e}"));
            self.engine.open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

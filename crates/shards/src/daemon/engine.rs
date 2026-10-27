//! The Engine API (D170): what programs that make sandboxes ask of a container engine,
//! answered with microVMs, on a socket file in the home (`engine.sock`, which they reach
//! as `DOCKER_HOST=unix://HOME/engine.sock`), never a port.
//!
//! Each call is answered with what the daemon answers shards' CLI for the same thing: the
//! request the CLI makes, put on a socket pair of the engine's and taken as any client's
//! is (`take`), or the very function that answers the CLI. A caller of the API so gets
//! what the CLI gets, its checks and refusals among them. Requests are read as Go's server
//! reads them (build/proxy/head.rs), as dockerd's is Go's.

mod build;
mod containers;
mod create;
mod disk;
mod exec;
mod files;
mod http;
mod images;
mod networks;
mod own_networks;
mod own_volumes;
mod scope;
mod volumes;
mod watch;

use std::collections::HashMap;
use std::io::{self, BufReader, PipeReader, Read as _, Write as _};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use serde_json::json;
use shards_ipc::Stream as UnixStream;
use shards_ipc::fd::{AsFd as _, BorrowedFd};
use shards_ipc::{Run, kind};

use super::{Daemon, Threads, lock, log};
use crate::build::proxy::head::{self, Body, Head};
use crate::containers::Disk;
use http::Answer;
pub(super) use own_networks::own_id;
pub(super) use scope::Scope;

/// The socket, relative to the home, the daemon's working directory: a socket address fits
/// it whatever the home's path (`sun_path`).
pub(super) const SOCKET: &str = "engine.sock";
/// The API served, and the oldest answered (cli/version.rs).
pub(super) const VERSION: &str = crate::cli::version::ENGINE_API.0;
pub(super) const MIN_VERSION: &str = crate::cli::version::ENGINE_API.1;
/// The most of a request's body the engine reads: a run is at most one message's payload
/// (shards_ipc::MAX_PAYLOAD), and JSON's escapes take at most six bytes for each of its
/// bytes, its names and spaces more.
const BODY_MOST: u64 = 8 * shards_ipc::MAX_PAYLOAD as u64;

/// What the engine keeps between calls.
#[derive(Default)]
pub(super) struct Engine {
    /// API connections open: the daemon is not idle while one is.
    pub(super) open: AtomicUsize,
    /// Each open connection, by number, which a daemon that stops ends.
    conns: Mutex<HashMap<u64, UnixStream>>,
    /// What hangs up each connection relayed to a context's machine (relay.rs), by its
    /// number, which a daemon that stops hangs up: ending its client's connection here
    /// half-closes it there, and a stream there may outlast that.
    relayed: Mutex<HashMap<u64, shards_remote::client::Controls>>,
    next: AtomicU64,
    /// Attaches waiting for their container's start, by its ID: its start's output goes
    /// to them (`docker run` attaches before it starts).
    waiting: Mutex<HashMap<String, Vec<containers::Waiting>>>,
    /// Exec instances made, by ID, until their container goes.
    execs: Mutex<HashMap<String, exec::Instance>>,
    /// The connections terminals are resized through while a client is attached to them,
    /// by the ID of the container or exec whose they are.
    terminals: Mutex<HashMap<String, UnixStream>>,
    /// Runs' stdins their attached client detached from, kept open until the run ends: a
    /// detach leaves a command's stdin as it was.
    stdins: Mutex<HashMap<String, std::io::PipeWriter>>,
    /// Connections runs' commands made to their own microVMs' engines (D181), by the run's
    /// ID, for the listener, which has the threads to serve them, to take.
    arrived: Mutex<Vec<(String, UnixStream)>>,
    /// How many connections each microVM's own engine holds open (D181), by its run's ID.
    scoped: Mutex<HashMap<String, usize>>,
}

impl Engine {
    /// Holds `conn`, an API client's, which a daemon that stops ends (`end_engine`), and
    /// counts it open: the number it is let go of by.
    pub(super) fn hold(&self, conn: UnixStream) -> u64 {
        let number = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.conns).insert(number, conn);
        self.open.fetch_add(1, Ordering::SeqCst);
        number
    }

    /// Keeps `conn`, a connection run `provider`'s command made to its own microVM's engine
    /// (D181), for the listener to take ([`Engine::take_arrived`]).
    pub(super) fn arrive(&self, provider: &str, conn: UnixStream) {
        lock(&self.arrived).push((provider.to_string(), conn));
    }

    /// The connections runs' commands made to their own microVMs' engines since the last
    /// look.
    pub(super) fn take_arrived(&self) -> Vec<(String, UnixStream)> {
        std::mem::take(&mut *lock(&self.arrived))
    }

    /// Admits one more connection of microVM `provider`'s own engine (D181), or says why
    /// not. Each holds a thread of the daemon's, as a client in hand does, and is one
    /// (`busy`): of the `most` clients the daemon's threads hold, a microVM's engine holds
    /// an equal share alone, among the host's own clients and each microVM whose engine
    /// holds any, so that none takes the host's or another's.
    fn admit(&self, provider: &str, most: usize, busy: &AtomicUsize) -> Result<(), String> {
        let mut held = lock(&self.scoped);
        let others = held.iter().filter(|(p, n)| *p != provider && **n > 0).count();
        let share = share(most, others);
        let mine = held.get(provider).copied().unwrap_or(0);
        if mine >= share || busy.load(Ordering::SeqCst) >= most {
            return Err(format!(
                "this microVM's engine holds {mine} connections, its share of what the daemon's threads hold, which {} microVMs' engines and their host's own clients share: close one",
                others + 1
            ));
        }
        *held.entry(provider.to_string()).or_default() += 1;
        busy.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Lets go of a connection [`admit`](Self::admit) took.
    fn release(&self, provider: &str, busy: &AtomicUsize) {
        let mut held = lock(&self.scoped);
        if let Some(n) = held.get_mut(provider) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                held.remove(provider);
            }
        }
        busy.fetch_sub(1, Ordering::SeqCst);
    }

    /// Lets go of connection `number`, which has ended.
    pub(super) fn let_go(&self, number: u64) {
        lock(&self.conns).remove(&number);
        lock(&self.relayed).remove(&number);
        self.open.fetch_sub(1, Ordering::SeqCst);
    }

    /// Holds what hangs up relayed connection `number`, for a daemon that stops to end it;
    /// hung up now where it is stopping already. `stopping` is read under the lock
    /// [`end_engine`](Daemon::end_engine) hangs them up under, and stored before it: each
    /// connection is hung up by one or the other.
    pub(super) fn hold_relayed(
        &self,
        number: u64,
        controls: shards_remote::client::Controls,
        stopping: &AtomicBool,
    ) {
        let mut relayed = lock(&self.relayed);
        if stopping.load(Ordering::SeqCst) {
            let _ = controls.hang_up();
        } else {
            relayed.insert(number, controls);
        }
    }
}

/// A call as the engine reads it.
pub(super) struct Call<'h> {
    head: &'h Head,
    /// The API version it asks for: its path's, else the latest.
    version: String,
    /// Its path's segments, decoded, its version's taken off.
    path: Vec<String>,
    query: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Call<'_> {
    /// Query parameter `key`'s first value.
    fn query(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Query parameter `key`'s values, each as given, in order.
    fn all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.query
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Query parameter `key` as a flag, as dockerd reads one (httputils.BoolValue):
    /// anything but empty, `0`, `no`, `false` and `none`.
    fn flag(&self, key: &str) -> bool {
        let v = self.query(key).unwrap_or_default().trim().to_ascii_lowercase();
        !matches!(v.as_str(), "" | "0" | "no" | "false" | "none")
    }
}

/// Whether API version `a` is `b` or later, compared as moby compares them
/// (api/types/versions): each dot-separated part as a number.
pub(super) fn at_least(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    true
}

/// `s` percent-decoded, `+` a space where `plus` (a query's, as url.ParseQuery reads it);
/// `None` where an escape is malformed or the result is not UTF-8.
fn unescape(s: &str, plus: bool) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                let text = std::str::from_utf8(hex).ok()?;
                out.push(u8::from_str_radix(text, 16).ok()?);
                i += 3;
            }
            b'+' if plus => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// A query's parameters, as url.ParseQuery reads them: `&`-separated, each `KEY=VALUE`
/// or `KEY`, decoded; one that cannot be is left out, as Go leaves it.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty() && !p.contains(';'))
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((unescape(k, true)?, unescape(v, true)?))
        })
        .collect()
}

/// The status the API answers an error with, by what it says, as dockerd's error kinds
/// give each (errdefs; api/server/httpstatus): the first of [`STATUSES`] it says, else
/// `otherwise`.
fn status_of(said: &str, otherwise: u16) -> u16 {
    STATUSES
        .iter()
        .find(|(words, _)| said.contains(words))
        .map_or(otherwise, |&(_, status)| status)
}

/// Words of shards' refusals, and the status of the error kind dockerd gives the same
/// refusal (moby docker-v29.4.0).
const STATUSES: &[(&str, u16)] = &[
    // NotFound.
    ("No such ", 404),
    ("Could not find the file", 404),
    ("no network is named", 404),
    // A reference a registry has not, or will not show (daemon/containerd/image_pull.go:
    // containerd's NotFound kept, and "pull access denied for" made one).
    (": not found", 404),
    ("pull access denied for", 404),
    // Forbidden: PutContainerArchive's own description (dockerd's checkWritablePath says
    // InvalidParameter); a predefined network, one with members, one others take their
    // settings from (daemon/network.go deleteNetwork; libnetwork's ActiveEndpointsError,
    // "configuration network is in use").
    ("marked read-only", 403),
    ("is one of shards' own networks", 403),
    ("runs are on it", 403),
    ("networks take their settings from it", 403),
    // Forbidden: `scratch` pulled (postImagesCreate, validateRepoName).
    ("names no image to pull", 403),
    // InvalidParameter: a term more than one network answers to (daemon/network.go).
    ("networks by name", 400),
    ("networks by their IDs' beginning", 400),
    // Conflict: a name or volume in use, a run's state, a running microVM removed
    // without force (daemon/delete.go), an image a microVM was made of or that more
    // names refer to (daemon/containerd/image_delete.go).
    ("in use", 409),
    ("is not running", 409),
    ("is paused", 409),
    ("is not paused", 409),
    ("is restarting", 409),
    ("it is running", 409),
    ("cannot remove image", 409),
    // Unavailable.
    ("shutting down", 503),
];

/// What the daemon said, as the API says it: its text without the CLI's prefixes and its
/// last newline.
fn plain(said: &str) -> String {
    let said = said.trim_end_matches('\n');
    let said = said.strip_prefix("shards: ").unwrap_or(said);
    said.strip_prefix("Error response from daemon: ")
        .unwrap_or(said)
        .to_string()
}

/// An error the daemon said, answered with the status its words give it.
fn refused(said: &str, otherwise: u16) -> Answer {
    let message = plain(said);
    Answer::error(status_of(&message, otherwise), message)
}

/// A call whose body is taken as it comes, and what takes it.
enum Upload {
    /// `PUT /containers/{id}/archive`: files copied into microVM `id`.
    Files(String),
    /// `POST /images/load`: an archive of images.
    Images,
    /// `POST /images/create?fromSrc=-`: a root filesystem.
    Root,
    /// `POST /build`: a build's context.
    Build,
}

/// How a call was answered.
enum Done {
    /// Whole, by this answer, still to be written.
    Answer(Answer),
    /// On the connection, which may serve another call.
    Keep,
    /// On the connection, which ends now: a stream until it closed, or one taken over.
    Close,
}

/// What the daemon answered a run asked of it as the CLI asks one.
struct RunAnswer {
    /// Its status, `EXIT`'s.
    status: u8,
    /// Its container, once made (`CREATED`).
    id: Option<String>,
    /// What it said on the run's stderr: warnings, or why it did not run.
    said: String,
}

impl<D: Disk> Daemon<D> {
    /// Takes an API client's connection: this user's processes alone, as `take` takes the
    /// CLI's, served on a thread of its own until it closes; or, with `scope`, one a run's
    /// command made to its own microVM's engine, which its VM handed over (D181), served
    /// scoped to that microVM.
    pub(super) fn take_engine<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: UnixStream,
        scope: Option<Scope>,
    ) {
        // On Windows the Engine API takes no host client yet: its named pipe, whose DACL
        // admits this user's processes alone, comes with the plan's step 8. Nor does a
        // microVM's own engine come, whose run is refused as it is made (network.rs
        // `check`).
        #[cfg(windows)]
        if scope.is_none() {
            log("refused an API client: the Engine API is not served on Windows yet".into());
            return;
        }
        // SAFETY: geteuid(2) cannot fail.
        #[cfg(unix)]
        let ours = unsafe { libc::geteuid() };
        #[cfg(unix)]
        match shards_ipc::peer_uid(&conn) {
            _ if scope.is_some() => {}
            Ok(uid) if uid == ours => {}
            Ok(uid) => {
                log(format!("refused an API client of user {uid}"));
                return;
            }
            Err(e) => {
                log(format!("an API client's credentials: {e}"));
                return;
            }
        }
        if let Err(e) = conn.set_nonblocking(false) {
            log(format!("an API client's connection: {e}"));
            return;
        }
        let provider = scope.as_ref().map(|s| s.provider.clone());
        if let Some(p) = &provider
            && let Err(why) = self.engine.admit(p, self.max_clients, &self.busy)
        {
            if let Err(e) = answered_busy(&conn, &why) {
                log(format!("container {p}: refusing an engine connection: {e}"));
            }
            return;
        }
        let release = |provider: &Option<String>| {
            if let Some(p) = provider {
                self.engine.release(p, &self.busy);
            }
        };
        let number = match conn.try_clone() {
            Ok(held) => self.engine.hold(held),
            Err(e) => {
                release(&provider);
                log(format!("an API client's connection: {e}"));
                self.wake_listener();
                return;
            }
        };
        let done = move |provider: &Option<String>| {
            self.engine.let_go(number);
            release(provider);
            *lock(&self.last) = Instant::now();
            self.wake_listener();
        };
        let theirs = provider.clone();
        let spawned = std::thread::Builder::new()
            .name("engine".into())
            .spawn_scoped(threads, move || {
                self.engine_connection(threads, &conn, scope.as_ref());
                done(&theirs);
            });
        if let Err(e) = spawned {
            done(&provider);
            log(format!("an API client's thread: {e}"));
        }
    }

    /// Ends every API connection, those it relays to a context's machine too (relay.rs): the
    /// daemon stops.
    pub(super) fn end_engine(&self) {
        for conn in lock(&self.engine.conns).values() {
            shards_ipc::end(conn);
        }
        for controls in lock(&self.engine.relayed).values() {
            let _ = controls.hang_up();
        }
    }

    /// Serves the calls on `conn` one after another, as long as it is kept open.
    fn engine_connection<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        scope: Option<&Scope>,
    ) {
        let mut reader = BufReader::new(shards_ipc::Polled(conn));
        let mut after_post = false;
        loop {
            let head = match head::read(&mut reader, true, after_post) {
                Ok(head) => head,
                Err(fault) => {
                    if let Some(answer) = fault.answer() {
                        let mut out = conn;
                        let _ = out.write_all(&answer);
                    }
                    return;
                }
            };
            // A call is the daemon's work, as a run is.
            *lock(&self.last) = Instant::now();
            after_post = head.method == "POST";
            if !self.engine_call(threads, conn, &head, &mut reader, scope) || head.wants_close() {
                return;
            }
        }
    }

    /// Answers one call; whether its connection may serve another.
    fn engine_call<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        head: &Head,
        reader: &mut BufReader<shards_ipc::Polled<&UnixStream>>,
        scope: Option<&Scope>,
    ) -> bool {
        let (target, query) = head.target.split_once('?').unwrap_or((&head.target, ""));
        // Its version, where its path gives one (moby's router: `/v{version:[0-9.]+}`).
        let (version, path) = match target.strip_prefix("/v").and_then(|rest| rest.split_once('/')) {
            Some((v, rest)) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.') => {
                (v.to_string(), format!("/{rest}"))
            }
            _ => (VERSION.to_string(), target.to_string()),
        };
        let framing = match head.framing() {
            Ok(f) => f,
            Err(_) => return false,
        };
        // A call refused before its body is read: answered first, then the rest of its
        // body read and dropped ([`answered_first`]).
        let refuse = |reader: &mut BufReader<shards_ipc::Polled<&UnixStream>>, answer: Answer| {
            answered_first(conn, head, answer, &mut Body::new(reader, framing))
        };
        let Some(decoded) = unescape(&path, false) else {
            return refuse(reader, Answer::error(400, format!("{path}: a malformed path")));
        };
        if !at_least(&version, MIN_VERSION) || !at_least(VERSION, &version) {
            return refuse(
                reader,
                Answer::error(
                    400,
                    format!("API version {version}: shards serves {MIN_VERSION} to {VERSION}"),
                ),
            );
        }
        let mut call = Call {
            head,
            version,
            path: decoded
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            query: parse_query(query),
            body: Vec::new(),
        };
        let path: Vec<&str> = call.path.iter().map(String::as_str).collect();
        // A microVM's own engine (D181) answers what it does not serve before reading on.
        if scope.is_some() && !scope::serves(&head.method, &path) {
            return refuse(reader, scope::unserved(&head.method, &head.target));
        }
        // An upload is taken as it comes, never whole: files copied into a microVM, an
        // archive of images loaded, a root filesystem imported.
        let upload = match (head.method.as_str(), path.as_slice()) {
            ("PUT", ["containers", id, "archive"]) => Some(Upload::Files((*id).to_string())),
            ("POST", ["images", "load"]) => Some(Upload::Images),
            ("POST", ["images", "create"]) if call.query("fromSrc") == Some("-") => Some(Upload::Root),
            ("POST", ["build"]) => Some(Upload::Build),
            _ => None,
        };
        if let Some(upload) = upload {
            let mut body = Body::new(reader, framing);
            let done = match upload {
                Upload::Files(id) => self.engine_extract(threads, &id, &call, &mut body),
                Upload::Images => self.engine_load(threads, conn, &call, &mut body),
                Upload::Root => self.engine_import(threads, conn, &call, Some(&mut body)),
                Upload::Build => self.engine_build(conn, &call, &mut body),
            };
            return match done {
                Done::Answer(answer) => answered_first(conn, head, answer, &mut body),
                Done::Keep => body.ended() || drained(&mut body),
                Done::Close => false,
            };
        }
        // Every other call's body is JSON, or nothing: read whole, as much as a run holds.
        let mut body = Body::new(reader, framing);
        let read = (&mut body).take(BODY_MOST + 1).read_to_end(&mut call.body);
        if read.is_err() {
            return false;
        }
        if call.body.len() as u64 > BODY_MOST {
            let answer = Answer::error(
                413,
                format!("a call's body is at most {BODY_MOST} bytes, which hold the largest run"),
            );
            return answered_first(conn, head, answer, &mut body);
        }
        let done = match scope {
            Some(scope) => self.route_scoped(threads, conn, &call, scope),
            None => self.route(threads, conn, &call),
        };
        match done {
            Done::Answer(answer) => answer.write(conn, head, true).is_ok(),
            Done::Keep => true,
            Done::Close => false,
        }
    }

    /// The answer to `call`, made on `conn` itself where it streams.
    fn route<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, conn: &UnixStream, call: &Call<'_>) -> Done {
        let path: Vec<&str> = call.path.iter().map(String::as_str).collect();
        let method = call.head.method.as_str();
        let answer = match (method, path.as_slice()) {
            ("GET" | "HEAD", ["_ping"]) => ping(method),
            ("GET", ["version"]) => version(),
            ("GET", ["info"]) => {
                let info = crate::daemon::info::server(&self.facts(), "system.Info").value();
                Answer::document(200, crate::daemon::inspect_doc::json(&info))
            }
            ("POST", ["containers", "create"]) => self.engine_create(threads, call),
            ("GET", ["containers", "json"]) => self.engine_list(call),
            ("POST", ["containers", "prune"]) => self.engine_prune(call),
            ("GET", ["containers", id, "json"]) => self.engine_inspect(id, call),
            ("POST", ["containers", id, "start"]) => self.engine_start(threads, id, call),
            // A microVM's snapshots, Docker's checkpoints (D176), served as an experimental
            // daemon serves them (moby router/checkpoint).
            ("POST", ["containers", id, "checkpoints"]) => self.engine_checkpoint_create(id, call),
            ("GET", ["containers", id, "checkpoints"]) => self.engine_checkpoints(id, call),
            ("DELETE", ["containers", id, "checkpoints", checkpoint]) => {
                self.engine_checkpoint_remove(id, checkpoint, call)
            }
            ("POST", ["containers", id, "stop"]) => self.engine_stop(threads, id, call, false),
            ("POST", ["containers", id, "kill"]) => self.engine_stop(threads, id, call, true),
            ("DELETE", ["containers", id]) => self.engine_remove(threads, id, call),
            ("POST", ["containers", id, "wait"]) => return self.engine_wait(conn, id, call),
            ("POST", ["containers", id, "attach"]) => return self.engine_attach(threads, conn, id, call),
            ("GET", ["containers", id, "logs"]) => return self.engine_logs(threads, conn, id, call),
            ("POST", ["containers", id, "resize"]) => self.engine_resize(id, call),
            ("GET" | "HEAD", ["containers", id, "archive"]) => {
                return self.engine_archive(threads, conn, id, call);
            }
            ("GET", ["containers", id, "export"]) => return self.engine_export(threads, conn, id),
            ("GET", ["containers", id, "changes"]) => self.engine_changes(threads, id),
            ("GET", ["containers", id, "top"]) => self.engine_top(id, call),
            ("GET", ["containers", id, "stats"]) => return self.engine_stats(conn, id, call),
            ("POST", ["containers", id, "pause"]) => self.engine_pause(threads, id, true),
            ("POST", ["containers", id, "unpause"]) => self.engine_pause(threads, id, false),
            ("POST", ["containers", id, "restart"]) => self.engine_restart(threads, id, call),
            ("POST", ["containers", id, "rename"]) => self.engine_rename(threads, id, call),
            ("POST", ["containers", id, "update"]) => self.engine_update(threads, id, call),
            ("GET", ["events"]) => return self.engine_events(threads, conn, call),
            ("GET", ["volumes"]) => self.engine_volumes(call),
            ("POST", ["volumes", "create"]) => self.engine_volume_create(threads, call),
            ("POST", ["volumes", "prune"]) => self.engine_volume_prune(call),
            ("GET", ["volumes", name]) => self.engine_volume(name),
            ("DELETE", ["volumes", name]) => self.engine_volume_remove(threads, name, call),
            ("GET", ["networks"]) => self.engine_networks(call),
            ("POST", ["networks", "create"]) => self.engine_network_create(threads, call),
            ("POST", ["networks", "prune"]) => self.engine_network_prune(call),
            ("GET", ["networks", term]) => self.engine_network(term),
            ("DELETE", ["networks", term]) => self.engine_network_remove(threads, term),
            ("POST", ["networks", term, "connect"]) => self.engine_network_join(threads, term, call, true),
            ("POST", ["networks", term, "disconnect"]) => {
                self.engine_network_join(threads, term, call, false)
            }
            ("POST", ["exec", id, "resize"]) => self.engine_resize(id, call),
            ("POST", ["containers", id, "exec"]) => self.engine_exec_create(id, call),
            ("POST", ["exec", id, "start"]) => return self.engine_exec_start(threads, conn, id, call),
            ("GET", ["exec", id, "json"]) => self.engine_exec_inspect(id),
            ("GET", ["images", "json"]) => self.engine_images(call),
            ("GET", ["images", "search"]) => self.engine_search(call),
            ("POST", ["images", "create"]) => return self.engine_pull(threads, conn, call),
            ("POST", ["images", "prune"]) => self.engine_image_prune(call),
            ("GET", ["images", "get"]) => {
                let names: Vec<String> = call.all("names").map(str::to_string).collect();
                return self.engine_save(threads, conn, &names, call);
            }
            ("GET", ["images", name @ .., "get"]) if !name.is_empty() => {
                return self.engine_save(threads, conn, &[name.join("/")], call);
            }
            ("POST", ["images", name @ .., "push"]) if !name.is_empty() => {
                return self.engine_push(threads, conn, &name.join("/"), call);
            }
            ("POST", ["commit"]) => self.engine_commit(threads, call),
            ("POST", ["auth"]) => self.engine_auth(call),
            ("POST", ["images", name @ .., "tag"]) if !name.is_empty() => {
                self.engine_image_tag(threads, &name.join("/"), call)
            }
            ("GET", ["images", name @ .., "history"]) if !name.is_empty() => {
                self.engine_image_history(threads, &name.join("/"), call)
            }
            ("DELETE", ["images", name @ ..]) if !name.is_empty() => {
                self.engine_image_remove(threads, &name.join("/"), call)
            }
            ("GET", ["system", "df"]) => self.engine_df(call),
            ("GET", ["images", name @ .., "json"]) if !name.is_empty() => {
                self.engine_image_inspect(&name.join("/"))
            }
            _ => Answer::error(
                404,
                format!(
                    "{method} {}: no call of the Engine API that shards serves",
                    call.head.target
                ),
            ),
        };
        Done::Answer(answer)
    }

    /// The requests the engine puts to the daemon are a client's: in the daemon's own
    /// environment, as a CLI started where the daemon was would ask (its kernel, its init,
    /// its registries' and clouds' credentials), of this daemon's build.
    fn as_client(&self, run: &mut Run) -> Result<(), String> {
        let path = |name: &str| -> Result<Option<String>, String> {
            match std::env::var_os(name).filter(|v| !v.is_empty()) {
                None => Ok(None),
                Some(v) if std::path::Path::new(&v).is_absolute() => v
                    .into_string()
                    .map(Some)
                    .map_err(|v| format!("{name}: {v:?} is not UTF-8")),
                Some(v) => Err(format!(
                    "{name}: {v:?}: the daemon's is absolute, as no directory of a client's makes it one"
                )),
            }
        };
        run.kernel = path("SHARDS_KERNEL")?;
        run.init = path("SHARDS_INIT")?;
        if run.kernel.is_some() != run.init.is_some() {
            return Err("SHARDS_KERNEL and SHARDS_INIT go together".into());
        }
        run.registry_env = shards_ipc::registry_env();
        run.cloud_env = crate::cloud_environment();
        run.daemon = self.identity;
        Ok(())
    }

    /// Puts `run` to the daemon as the CLI puts one, on a socket pair of the engine's, with
    /// `stdio`, the run's stdin, stdout and stderr: the connection its answer comes on.
    fn put_run<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        run: &Run,
        stdio: [BorrowedFd<'_>; 3],
    ) -> Result<UnixStream, String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a socket pair: {e}"))?;
        self.take(threads, theirs);
        shards_ipc::send(&ours, kind::START, &run.encode(), &stdio)
            .map_err(|e| format!("asking the daemon: {e}"))?;
        Ok(ours)
    }

    /// Puts container command `argv` to the daemon as the CLI puts it (`shards ARGV`), and
    /// takes its answer whole: its status, and what it said on stdout and on stderr.
    fn ask_command<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        argv: &[&str],
    ) -> Result<(u8, Vec<u8>, Vec<u8>), String> {
        self.ask_command_with(threads, argv, &[])
    }

    /// [`ask_command`](Self::ask_command), the command given `body` as the file it reads,
    /// as the CLI gives one (`-i`, a copy's archive, an import's `-`): the body into it as
    /// it comes, never staged, and the daemon's answer heard meanwhile, so that a command
    /// that refuses stops reading and its answer still comes.
    fn ask_with_body<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        argv: &[&str],
        body: &mut dyn io::Read,
    ) -> Result<(u8, Vec<u8>, Vec<u8>), String> {
        let (from, mut into) = io::pipe().map_err(|e| format!("a pipe: {e}"))?;
        let ipc = self.command_conn_with(threads, argv, shards_ipc::registry_env(), &[from.as_fd()])?;
        drop(from);
        std::thread::scope(|s| {
            let answer = std::thread::Builder::new()
                .name("engine-upload".into())
                .spawn_scoped(s, || {
                    let (mut out, mut err) = (Vec::new(), Vec::new());
                    loop {
                        match shards_ipc::recv(&ipc) {
                            Ok(Some(m)) if m.kind == kind::OUT => out.extend(m.payload),
                            Ok(Some(m)) if m.kind == kind::ERR => err.extend(m.payload),
                            Ok(Some(m)) if m.kind == kind::END => {
                                return Ok((m.payload.first().copied().unwrap_or(1), out, err));
                            }
                            Ok(Some(m)) if m.kind == kind::RESTART => {
                                return Err("the daemon is shutting down".to_string());
                            }
                            Ok(Some(_)) => {}
                            Ok(None) | Err(_) => {
                                return Err("the daemon ended before it answered".to_string());
                            }
                        }
                    }
                })
                .map_err(|e| format!("its answer's thread: {e}"))?;
            let _ = io::copy(body, &mut into);
            drop(into);
            answer
                .join()
                .unwrap_or_else(|_| Err("its answer was lost".to_string()))
        })
    }

    /// [`ask_command`](Self::ask_command), the command given `files` as the CLI gives
    /// its own (what it reads or writes, as `-o` and `-i` give them).
    fn ask_command_with<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        argv: &[&str],
        files: &[BorrowedFd<'_>],
    ) -> Result<(u8, Vec<u8>, Vec<u8>), String> {
        let conn = self.command_conn_with(threads, argv, shards_ipc::registry_env(), files)?;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        loop {
            match shards_ipc::recv(&conn) {
                Ok(Some(m)) if m.kind == kind::OUT => out.extend(m.payload),
                Ok(Some(m)) if m.kind == kind::ERR => err.extend(m.payload),
                Ok(Some(m)) if m.kind == kind::END => {
                    return Ok((m.payload.first().copied().unwrap_or(1), out, err));
                }
                Ok(Some(m)) if m.kind == kind::RESTART => {
                    return Err("the daemon is shutting down".into());
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return Err("the daemon ended before it answered".into()),
            }
        }
    }

    /// The connection on which the daemon answers container command `argv`, put as the CLI
    /// puts it.
    fn command_conn<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        argv: &[&str],
    ) -> Result<UnixStream, String> {
        self.command_conn_with(threads, argv, shards_ipc::registry_env(), &[])
    }

    /// [`command_conn`](Self::command_conn), its registries reached with `registry_env`,
    /// and given `files`.
    fn command_conn_with<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        argv: &[&str],
        registry_env: Vec<String>,
        files: &[BorrowedFd<'_>],
    ) -> Result<UnixStream, String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let command = shards_ipc::Command {
            argv: argv.iter().map(|a| (*a).to_string()).collect(),
            registry_env,
            now,
            // The daemon's own zone, which it has from the client that started it.
            utc_offset: crate::cli::utc_offset(),
            daemon: self.identity,
            ..shards_ipc::Command::default()
        };
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("a socket pair: {e}"))?;
        self.take(threads, theirs);
        shards_ipc::send(&ours, kind::CONTAINER, &command.encode(), files)
            .map_err(|e| format!("asking the daemon: {e}"))?;
        Ok(ours)
    }
}

/// Waits until the daemon has a message on `ipc` for a stream it answers, or `client` goes
/// first, or sends something, which a client that waits for a stream sends none of: whether
/// it went. A stream ends with its client, as moby's ends with its request (net/http cancels
/// a request's context as its connection's read ends, server.go `backgroundRead`), and not
/// at its next message, which a quiet microVM's log, or a host where nothing happens, may
/// never send.
#[cfg(windows)]
fn gone_first(ipc: &UnixStream, client: &UnixStream) -> bool {
    shards_ipc::await_streams(&[ipc, client], None).is_ok_and(|ready| ready.get(1).copied().unwrap_or(false))
}

#[cfg(unix)]
fn gone_first(ipc: &UnixStream, client: &UnixStream) -> bool {
    use std::os::fd::AsRawFd as _;
    let pollfd = |fd: &UnixStream| libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut polled = [pollfd(ipc), pollfd(client)];
    // SAFETY: poll(2) on two pollfds of descriptors the caller holds open.
    while unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } < 0 {
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return false;
        }
    }
    let [_, client] = polled;
    client.revents != 0
}

/// Answers a client whose call a context's machine did not answer (relay.rs) with `why`, as
/// the API says an error, its connection closed after: a gateway's 502 (RFC 9110 §15.6.3),
/// which Docker's clients say as `Error response from daemon: WHY`.
pub(super) fn unanswered(conn: &UnixStream, why: &str) -> io::Result<()> {
    // Its call is not read: answered as one that has content would be.
    let head = Head {
        method: "GET".into(),
        target: String::new(),
        major: 1,
        minor: 1,
        fields: Vec::new(),
    };
    Answer::error(502, why).write(conn, &head, false)
}

/// A microVM's own engine's share of the `most` clients the daemon's threads hold (D181),
/// where `others` other microVMs' engines hold connections: the host's own clients, each of
/// theirs and its own, equally; one at the least.
fn share(most: usize, others: usize) -> usize {
    (most / others.saturating_add(2)).max(1)
}

/// Answers a connection the daemon has no thread for now with `why`, unread, closed after:
/// a 503 (RFC 9110 §15.6.4), which Docker's clients say as `Error response from daemon:
/// WHY`.
fn answered_busy(conn: &UnixStream, why: &str) -> io::Result<()> {
    let head = Head {
        method: "GET".into(),
        target: String::new(),
        major: 1,
        minor: 1,
        fields: Vec::new(),
    };
    Answer::error(503, why).write(conn, &head, false)
}

/// `_ping`: that the engine serves, and its API's version, which clients negotiate by; and
/// that it has experimental features, as docker/cli asks before it offers checkpoints,
/// which shards' are a microVM's snapshots (D176).
fn ping(method: &str) -> Answer {
    let mut answer = Answer::text(200, if method == "HEAD" { "" } else { "OK" });
    answer.fields = vec![
        ("Cache-Control", "no-cache, no-store, must-revalidate".into()),
        ("Docker-Experimental", "true".into()),
        ("Pragma", "no-cache".into()),
    ];
    if method == "HEAD" {
        answer.kind = "text/plain; charset=utf-8";
    }
    answer
}

/// `version`: shards' server half, as `shards version` gives it (cli/version.rs), the
/// platform its containers are: Linux, its guests' kernel's, which clients go by as they
/// go by a server's.
fn version() -> Answer {
    let (_, mut server) = crate::cli::version::info();
    let guest = server
        .components
        .iter()
        .find(|c| c.name == "Guest kernel")
        .map(|c| c.version.clone())
        .unwrap_or_default();
    server.os = "linux".into();
    if let Some(engine) = server.components.first_mut() {
        engine.details.insert("Os".into(), "linux".into());
        engine.details.insert("KernelVersion".into(), guest);
    }
    let components: Vec<serde_json::Value> = server
        .components
        .iter()
        .map(|c| json!({"Name": c.name, "Version": c.version, "Details": c.details}))
        .collect();
    let engine = server.components.first();
    let detail = |key: &str| {
        engine
            .and_then(|c| c.details.get(key))
            .cloned()
            .unwrap_or_default()
    };
    Answer::json(
        200,
        &json!({
            "Platform": {"Name": server.platform},
            "Components": components,
            "Version": server.version,
            "ApiVersion": VERSION,
            "MinAPIVersion": MIN_VERSION,
            "GitCommit": detail("GitCommit"),
            "GoVersion": detail("GoVersion"),
            "Os": server.os,
            "Arch": server.arch,
            "KernelVersion": detail("KernelVersion"),
            // Where clients before docker/cli 29 read it, the Engine's details being where
            // later ones do; dockerd says it in both (daemon/info.go, SystemVersion).
            "Experimental": detail("Experimental") == "true",
            "BuildTime": detail("BuildTime"),
        }),
    )
}

/// The answer to a run put with nothing to read or write (a create, a detached start):
/// its messages until it is done, and what it said on its stderr, whose other end is
/// `said`.
fn run_answer(conn: &UnixStream, said: PipeReader) -> RunAnswer {
    std::thread::scope(|s| {
        let reader = std::thread::Builder::new()
            .name("engine-said".into())
            .spawn_scoped(s, move || {
                let mut text = Vec::new();
                let _ = (&said).read_to_end(&mut text);
                String::from_utf8_lossy(&text).into_owned()
            });
        let mut answer = RunAnswer {
            status: 1,
            id: None,
            said: String::new(),
        };
        loop {
            match shards_ipc::recv(conn) {
                Ok(Some(m)) if m.kind == kind::CREATED => {
                    answer.id = Some(String::from_utf8_lossy(&m.payload).into_owned());
                }
                Ok(Some(m)) if m.kind == kind::EXIT => {
                    answer.status = m.payload.first().copied().unwrap_or(1);
                    break;
                }
                Ok(Some(m)) if m.kind == kind::RESTART => {
                    answer.said = "the daemon is shutting down\n".into();
                    break;
                }
                Ok(Some(_)) => {}
                // A detached run's connection is its VM's from its start, and ends there.
                Ok(None) | Err(_) => {
                    answer.status = u8::from(answer.id.is_none());
                    break;
                }
            }
        }
        if let Ok(reader) = reader {
            answer.said.push_str(&reader.join().unwrap_or_default());
        }
        answer
    })
}

/// Whether `conn` may serve another call, once `answer`, given before the call's `body` was
/// read whole, is written as one whose connection is kept and the rest of the body read and
/// dropped ([`drained`]). So a client still sending a body never finds its connection gone,
/// and reads its answer whenever it does: a server that answers before it has read a
/// request may keep reading it (RFC 9110 §10.1.1). Go's server, dockerd's, drops 256 KiB of
/// such a body and closes past that (net/http server.go, maxPostHandlerReadBytes), and a
/// client still sending a larger one, a build's context refused, can lose its answer to a
/// broken pipe.
fn answered_first<R: io::BufRead>(
    conn: &UnixStream,
    head: &Head,
    answer: Answer,
    body: &mut Body<'_, R>,
) -> bool {
    answer.write(conn, head, true).is_ok() && drained(body)
}

/// Whether the rest of `body` was read to its end, and dropped.
fn drained<R: io::BufRead>(body: &mut Body<'_, R>) -> bool {
    io::copy(body, &mut io::sink()).is_ok() && body.ended()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_part_by_part() {
        assert!(at_least("1.56", "1.40"));
        assert!(at_least("1.40", "1.40"));
        assert!(!at_least("1.39", "1.40"));
        assert!(at_least("1.100", "1.56"));
        assert!(!at_least("1.9", "1.40"));
    }

    #[test]
    fn queries_are_read_as_go_reads_them() {
        assert_eq!(
            parse_query("all=1&filters=%7B%22label%22%3A%5B%22a%3Db%22%5D%7D&x=a+b&bad=%zz&semi;x=1"),
            [
                ("all".into(), "1".into()),
                ("filters".into(), r#"{"label":["a=b"]}"#.into()),
                ("x".into(), "a b".into())
            ]
        );
    }

    /// A microVM's own engine holds its share of the daemon's clients alone (D181): the
    /// host's own and each engine holding any, equally; a microVM over its share is refused
    /// until one of its own goes, and none is refused where the daemon has none to give.
    #[test]
    fn a_microvms_engine_holds_its_share_of_the_daemons_clients() {
        assert_eq!(
            (share(12, 0), share(12, 1), share(12, 2), share(1, 5)),
            (6, 4, 3, 1)
        );
        let engine = Engine::default();
        let busy = AtomicUsize::new(0);
        for _ in 0..6 {
            engine.admit("a", 12, &busy).unwrap();
        }
        let refused = engine.admit("a", 12, &busy).unwrap_err();
        assert!(
            refused.starts_with("this microVM's engine holds 6 connections"),
            "{refused}"
        );
        // Another's share, while the first holds its own: a third each.
        for _ in 0..4 {
            engine.admit("b", 12, &busy).unwrap();
        }
        assert!(engine.admit("b", 12, &busy).is_err());
        assert!(engine.admit("a", 12, &busy).is_err());
        engine.release("a", &busy);
        assert_eq!(busy.load(Ordering::SeqCst), 9);
        // Every client the daemon holds in hand: none for anyone.
        busy.store(12, Ordering::SeqCst);
        assert!(engine.admit("c", 12, &busy).is_err());
    }

    #[test]
    fn errors_are_answered_with_their_status() {
        assert_eq!(status_of("No such container: x", 500), 404);
        assert_eq!(status_of("No such image: alpine:latest", 400), 404);
        assert_eq!(
            status_of("--name a: the name is in use by microVM 0123", 400),
            409
        );
        assert_eq!(status_of("the daemon is shutting down", 500), 503);
        assert_eq!(status_of("anything else", 400), 400);
        assert_eq!(
            plain("shards: Error response from daemon: No such image: x\n"),
            "No such image: x"
        );
    }
}

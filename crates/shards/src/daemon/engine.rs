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
mod volumes;
mod watch;

use std::collections::HashMap;
use std::io::{self, BufReader, PipeReader, Read as _, Write as _};
use std::os::fd::{AsFd as _, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use serde_json::json;
use shards_ipc::{Run, kind};

use super::{Daemon, Threads, lock, log};
use crate::build::proxy::head::{self, Body, Head};
use crate::containers::Disk;
use http::Answer;

/// The socket, relative to the home, the daemon's working directory, as `daemon.sock` is.
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
    /// CLI's, served on a thread of its own until it closes.
    pub(super) fn take_engine<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, conn: UnixStream) {
        // SAFETY: geteuid(2) cannot fail.
        let ours = unsafe { libc::geteuid() };
        match shards_ipc::peer_uid(&conn) {
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
        let number = self.engine.next.fetch_add(1, Ordering::Relaxed);
        match conn.try_clone() {
            Ok(held) => {
                lock(&self.engine.conns).insert(number, held);
            }
            Err(e) => {
                log(format!("an API client's connection: {e}"));
                return;
            }
        }
        self.engine.open.fetch_add(1, Ordering::SeqCst);
        let done = move || {
            lock(&self.engine.conns).remove(&number);
            self.engine.open.fetch_sub(1, Ordering::SeqCst);
            *lock(&self.last) = Instant::now();
            self.wake_listener();
        };
        let spawned = std::thread::Builder::new()
            .name("engine".into())
            .spawn_scoped(threads, move || {
                self.engine_connection(threads, &conn);
                done();
            });
        if let Err(e) = spawned {
            done();
            log(format!("an API client's thread: {e}"));
        }
    }

    /// Ends every API connection: the daemon stops.
    pub(super) fn end_engine(&self) {
        for conn in lock(&self.engine.conns).values() {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Serves the calls on `conn` one after another, as long as it is kept open.
    fn engine_connection<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, conn: &UnixStream) {
        let Ok(reading) = conn.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(reading);
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
            if !self.engine_call(threads, conn, &head, &mut reader) || head.wants_close() {
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
        reader: &mut BufReader<UnixStream>,
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
        // A call refused before its body is read: the rest of its body read and dropped,
        // as much as Go's server drops to keep a connection, past which it closes.
        let refuse = |reader: &mut BufReader<UnixStream>, answer: Answer| {
            let mut body = Body::new(reader, framing);
            let dropped = io::copy(&mut (&mut body).take(head::DISCARD), &mut io::sink());
            let whole = dropped.is_ok() && body.ended();
            answer.write(conn, head, whole).is_ok() && whole
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
        // An upload is taken as it comes, never whole: files copied into a microVM, an
        // archive of images loaded, a root filesystem imported.
        let path: Vec<&str> = call.path.iter().map(String::as_str).collect();
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
            let whole = body.ended();
            return match done {
                Done::Answer(answer) => answer.write(conn, head, whole).is_ok() && whole,
                Done::Keep => whole,
                Done::Close => false,
            };
        }
        // Every other call's body is JSON, or nothing: read whole, as much as a run holds.
        let read = Body::new(reader, framing)
            .take(BODY_MOST + 1)
            .read_to_end(&mut call.body);
        if read.is_err() {
            return false;
        }
        if call.body.len() as u64 > BODY_MOST {
            let answer = Answer::error(
                413,
                format!("a call's body is at most {BODY_MOST} bytes, which hold the largest run"),
            );
            let _ = answer.write(conn, head, false);
            return false;
        }
        match self.route(threads, conn, &call) {
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
            ("POST", ["containers", id, "start"]) => self.engine_start(threads, id),
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
    fn ask_with_body<'s, 'e, R: io::BufRead>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        argv: &[&str],
        body: &mut Body<'_, R>,
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

/// `_ping`: that the engine serves, and its API's version, which clients negotiate by.
fn ping(method: &str) -> Answer {
    let mut answer = Answer::text(200, if method == "HEAD" { "" } else { "OK" });
    answer.fields = vec![
        ("Cache-Control", "no-cache, no-store, must-revalidate".into()),
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

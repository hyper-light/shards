//! Containers' calls (D170): each container a microVM, made, started, attached, waited
//! for, stopped and removed as shards' CLI asks for each.

use std::fs::File;
use std::io::{self, PipeReader, PipeWriter, Read as _, Write as _};
use std::os::fd::{AsFd as _, AsRawFd as _};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use shards_cmdline::flags::{self, Outcome};
use shards_cmdline::term::{self, EscapeProxy};
use shards_ipc::{Run, kind};

use super::http::{self, Answer, MULTIPLEXED, RAW, Stream, frame};
use super::{Call, Done, RunAnswer, create, plain, refused, status_of};
use crate::containers::{Container, Disk, State as Life};
use crate::daemon::{Daemon, REQUEST, Threads, lock, log};

/// A checkpoint directory of a client's own: shards keeps a microVM's snapshots with it,
/// and dockerd restores from none other (moby daemon/start.go, "custom checkpointdir is
/// not supported").
const CHECKPOINT_DIR: &str =
    "a checkpoint directory of your own is not supported: shards keeps a microVM's checkpoints with it";

/// An attach waiting for its container's start, which `docker run` asks for before it
/// starts the container: the start's output goes to it.
pub(in crate::daemon) struct Waiting {
    /// Its number, by which it is taken out where it stops waiting.
    number: u64,
    /// The streams it asks for: stdin, stdout, stderr.
    streams: [bool; 3],
    /// Its connection, which the run's output is written to, and which the start ends
    /// once the run has.
    to: UnixStream,
    /// Where the start sends it the run's stdin, for it to fill from its client.
    stdin: Option<mpsc::Sender<PipeWriter>>,
}

/// How long an attach waiting for its container's start waits before it looks whether
/// the container is still there.
const LOOK_AGAIN: Duration = Duration::from_secs(1);

/// The connections a run's output goes to, each with the streams it asked for; and
/// whether the output is a terminal's, raw, or multiplexed.
pub(super) struct Outputs {
    pub(super) to: Mutex<Vec<(UnixStream, [bool; 3])>>,
    pub(super) tty: bool,
}

impl Outputs {
    /// Writes `bytes` of `stream` (1 stdout, 2 stderr) to each connection that asked for
    /// it, a frame each where the output is not a terminal's; one that fails, goes.
    fn write(&self, stream: u8, bytes: &[u8]) {
        let framed;
        let out: &[u8] = if self.tty {
            bytes
        } else {
            framed = frame(stream, bytes);
            &framed
        };
        let mut to = self.to.lock().unwrap_or_else(PoisonError::into_inner);
        to.retain(|(conn, asked)| {
            !asked.get(usize::from(stream)).copied().unwrap_or(false) || (&mut &*conn).write_all(out).is_ok()
        });
    }

    /// Copies `from` to each connection as `stream`, until it ends; or, unless `drain`,
    /// until no connection asks for it, which closes it, and its writer stops.
    pub(super) fn copy(&self, stream: u8, mut from: PipeReader, drain: bool) {
        let mut buf = vec![0u8; 32 << 10];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => self.write(stream, buf.get(..n).unwrap_or_default()),
            }
            let asked = |(_, streams): &(UnixStream, [bool; 3])| {
                streams.get(usize::from(stream)).copied().unwrap_or(false)
            };
            if !drain
                && !self
                    .to
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .any(asked)
            {
                return;
            }
        }
    }

    /// Ends every connection: the stream is over.
    pub(super) fn end(&self) {
        for (conn, _) in self.to.lock().unwrap_or_else(PoisonError::into_inner).iter() {
            shards_ipc::end(conn);
        }
    }
}

/// Copies a client's input on `conn` to a command's stdin, `to`, until the client closes
/// it; with `keys`, a terminal's detach keys, until it types them, after which nothing
/// more of it is read (moby's copyEscapable). Whether it detached.
pub(super) fn input(conn: &UnixStream, to: &mut PipeWriter, keys: Option<&[u8]>) -> bool {
    let mut proxy = keys.map(EscapeProxy::new);
    let mut buf = [0u8; 4096];
    loop {
        let n = match shards_ipc::read(conn, &mut buf) {
            Ok(0) | Err(_) => return false,
            Ok(n) => n,
        };
        let read = buf.get(..n).unwrap_or_default();
        let (bytes, detached) = match proxy.as_mut().map(|p| p.read(read)) {
            None => (read.to_vec(), false),
            Some(term::Read::Input(bytes)) => (bytes, false),
            Some(term::Read::Detach(bytes)) => (bytes, true),
        };
        if to.write_all(&bytes).is_err() {
            return false;
        }
        if detached {
            return true;
        }
    }
}

/// The detach keys a call asks for (`detachKeys`), else the default, ctrl-p ctrl-q.
pub(super) fn detach_keys(given: Option<&str>) -> Result<Vec<u8>, String> {
    match given.filter(|k| !k.is_empty()) {
        Some(keys) => term::to_bytes(keys).map_err(|e| format!("detachKeys {keys}: {e}")),
        None => Ok(term::DETACH_KEYS.to_vec()),
    }
}

/// Whether `client` has gone, or sent something more, which a client that waits sends
/// none of: it is readable.
fn hung_up(client: &UnixStream) -> bool {
    let mut polled = libc::pollfd {
        fd: client.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll(2) on one pollfd of a descriptor this function's caller holds open.
    unsafe { libc::poll(&raw mut polled, 1, 0) > 0 }
}

/// The warnings a create's daemon said, as the API gives them: each line, its `WARNING: `
/// taken off.
pub(super) fn warnings(said: &str) -> Vec<String> {
    said.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.strip_prefix("WARNING: ").unwrap_or(l).to_string())
        .collect()
}

/// A list's `filters`, as moby reads them (filters.FromJSON): an object of each key's
/// values, given as an object of values to `true`, or as a list.
pub(super) fn filters(given: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let Some(given) = given.filter(|g| !g.is_empty()) else {
        return Ok(Vec::new());
    };
    let parsed: Value = serde_json::from_str(given).map_err(|e| format!("invalid filter: {e}"))?;
    let mut out = Vec::new();
    for (key, values) in parsed.as_object().into_iter().flatten() {
        match values {
            Value::Object(set) => out.extend(
                set.iter()
                    .filter(|(_, on)| on.as_bool().unwrap_or(false))
                    .map(|(v, _)| (key.clone(), v.clone())),
            ),
            Value::Array(list) => out.extend(
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|v| (key.clone(), v.to_string())),
            ),
            _ => return Err(format!("invalid filter: {key}'s values are a list or an object")),
        }
    }
    Ok(out)
}

impl<D: Disk> Daemon<D> {
    /// The request container `id` was made by, as it keeps it (D37).
    pub(super) fn made_by(&self, id: &str) -> Option<Run> {
        let dir = lock(&self.containers).dir(id);
        Run::decode(&std::fs::read(dir.join(REQUEST)).ok()?)
    }

    /// The command of image `image` as the store keeps it, for a create's empty
    /// entrypoint (create.rs).
    pub(super) fn image_cmd<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        image: &str,
    ) -> Result<Option<Vec<String>>, String> {
        let (status, out, err) = self.ask_command(
            threads,
            &["image", "inspect", "--format", "{{json .Config.Cmd}}", image],
        )?;
        if status != 0 {
            return Err(plain(&String::from_utf8_lossy(&err)));
        }
        serde_json::from_slice(&out).map_err(|e| format!("image {image}'s command: {e}"))
    }

    /// `POST /containers/create`: the run the body asks for, made as `shards create` makes
    /// it.
    pub(super) fn engine_create<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, call: &Call<'_>) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("a create's body: {e}")),
        };
        let image = body.get("Image").and_then(Value::as_str).unwrap_or_default();
        let made = create::run_of(
            &body,
            call.query("name").filter(|n| !n.is_empty()),
            call.query("platform").filter(|p| !p.is_empty()),
            || self.image_cmd(threads, image),
        );
        let mut run = match made {
            Ok(run) => run,
            Err(e) => return Answer::error(status_of(&e, 400), e),
        };
        if let Err(e) = self.as_client(&mut run) {
            return Answer::error(500, e);
        }
        match self.run_detached(threads, &run) {
            Ok(RunAnswer {
                status: 0,
                id: Some(id),
                said,
            }) => Answer::json(201, &json!({ "Id": id, "Warnings": warnings(&said) })),
            Ok(answer) => refused(&answer.said, 400),
            Err(e) => Answer::error(500, e),
        }
    }

    /// Puts `run` to the daemon with nothing to read and nothing to write to: a create,
    /// or a detached start. What it says on stderr is its answer's.
    pub(super) fn run_detached<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        run: &Run,
    ) -> Result<RunAnswer, String> {
        let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        let (said, said_by) = io::pipe().map_err(|e| format!("a pipe: {e}"))?;
        let conn = self.put_run(threads, run, [null.as_fd(), null.as_fd(), said_by.as_fd()])?;
        drop(said_by);
        Ok(super::run_answer(&conn, said))
    }

    /// `GET /containers/json`: the containers `ps` lists, its flags the query's, each as
    /// the API sums one up (container.Summary).
    pub(super) fn engine_list(&self, call: &Call<'_>) -> Answer {
        let mut argv: Vec<String> = Vec::new();
        if call.flag("all") {
            argv.push("--all".into());
        }
        if let Some(limit) = call
            .query("limit")
            .and_then(|l| l.parse::<i64>().ok())
            .filter(|l| *l > 0)
        {
            argv.extend(["--last".into(), limit.to_string()]);
        }
        match filters(call.query("filters")) {
            Ok(f) => {
                for (k, v) in f {
                    argv.extend(["--filter".into(), format!("{k}={v}")]);
                }
            }
            Err(e) => return Answer::error(400, e),
        }
        let parsed = match flags::parse(&shards_cmdline::commands::PS, "shards ps", &argv, &flags::value) {
            Outcome::Run(parsed) => parsed,
            Outcome::Fail { text, .. } => return Answer::error(400, plain(&text)),
            Outcome::Help { .. } => return Answer::error(400, "a list's query asks for help"),
        };
        self.settle();
        let listing = match self.ps_listing(&parsed) {
            Ok(listing) => listing,
            Err(e) => return refused(&e, 400),
        };
        let sized = call.flag("size");
        let summaries: Vec<Value> = listing
            .containers
            .iter()
            .map(|c| self.summary(c, sized))
            .collect();
        Answer::json(200, &Value::Array(summaries))
    }

    /// Container `c` as a list sums it up (moby container.Summary): its ports while it
    /// runs, its networks and mounts as inspect gives them.
    pub(super) fn summary(&self, c: &Container, sized: bool) -> Value {
        let doc: Value =
            serde_json::from_str(&crate::daemon::inspect_doc::json(&self.container_value(c, sized)))
                .unwrap_or_default();
        let at = |path: &str| doc.pointer(path).cloned().unwrap_or(Value::Null);
        let paused = lock(&self.paused).contains(&c.id);
        let removing = lock(&self.removing).contains(&c.id);
        let health = lock(&self.health).get(&c.id).map(|h| h.status);
        let status = crate::daemon::commands::status(c, crate::containers::now(), health, removing, paused);
        let ports: Vec<Value> = if c.state == Life::Running {
            c.ports
                .iter()
                .map(|p| {
                    let mut port = json!({ "PrivatePort": p.private, "Type": p.proto });
                    if let (Some(ip), Some(o)) = (p.ip, port.as_object_mut()) {
                        o.insert("IP".into(), json!(ip.to_string()));
                        o.insert("PublicPort".into(), json!(p.public));
                    }
                    port
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut summary = json!({
            "Id": c.id,
            "Names": [format!("/{}", c.name)],
            "Image": c.image,
            "ImageID": c.image_id.clone().unwrap_or_default(),
            "Command": crate::daemon::commands::command_line(&c.command),
            "Created": i64::try_from(c.created / 1_000_000_000).unwrap_or(0),
            "Ports": ports,
            "Labels": c.labels,
            "State": at("/State/Status"),
            "Status": status,
            "HostConfig": { "NetworkMode": at("/HostConfig/NetworkMode") },
            "NetworkSettings": { "Networks": at("/NetworkSettings/Networks") },
            "Mounts": at("/Mounts"),
        });
        if let Some(o) = summary.as_object_mut() {
            if !c.annotations.is_empty()
                && let Some(host) = o.get_mut("HostConfig").and_then(Value::as_object_mut)
            {
                host.insert("Annotations".into(), json!(c.annotations));
            }
            if let Some(h) = doc.pointer("/State/Health") {
                o.insert(
                    "Health".into(),
                    json!({ "Status": h.get("Status"), "FailingStreak": h.get("FailingStreak") }),
                );
            }
            if sized {
                o.insert("SizeRw".into(), at("/SizeRw"));
                o.insert("SizeRootFs".into(), at("/SizeRootFs"));
            }
        }
        summary
    }

    /// `GET /containers/{id}/json`: its InspectResponse, as `inspect` gives it.
    pub(super) fn engine_inspect(&self, given: &str, call: &Call<'_>) -> Answer {
        self.settle();
        let found = self.resolve(given).and_then(|id| {
            lock(&self.containers)
                .get(&id)
                .cloned()
                .ok_or_else(|| format!("No such container: {given}"))
        });
        match found {
            Ok(c) => Answer::document(
                200,
                crate::daemon::inspect_doc::json(&self.container_value(&c, call.flag("size"))),
            ),
            Err(e) => refused(&e, 500),
        }
    }

    /// `POST /containers/{id}/start`: started as `shards start` starts it; attached where
    /// attaches wait for its start, its output theirs. One running already is not
    /// modified (304).
    pub(super) fn engine_start<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        given: &str,
        call: &Call<'_>,
    ) -> Answer {
        // A checkpoint, a microVM's snapshot (D176), checked as dockerd checks it, in its
        // order (moby daemon/start.go): its name, the container, its state, and only then
        // a directory of the client's, which dockerd refuses too.
        let checkpoint = call
            .query("checkpoint")
            .filter(|c| !c.is_empty())
            .map(str::to_string);
        if let Some(asked) = &checkpoint
            && !super::super::snapshots::valid_checkpoint(asked)
        {
            return Answer::error(400, super::super::snapshots::invalid_checkpoint(asked));
        }
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return refused(&e, 500),
        };
        if lock(&self.paused).contains(&id) {
            return Answer::error(409, "cannot start a paused container, try unpause instead");
        }
        if self.running(&id) {
            return Answer::text(304, "");
        }
        if call.query("checkpoint-dir").is_some_and(|d| !d.is_empty()) {
            return Answer::error(403, "custom checkpointdir is not supported");
        }
        let waiting = lock(&self.engine.waiting).remove(&id).unwrap_or_default();
        let result = if waiting.is_empty() {
            let mut run = Run {
                again: Some(id),
                detach: true,
                resume: checkpoint,
                ..Run::default()
            };
            match self.as_client(&mut run) {
                Ok(()) => self.run_detached(threads, &run),
                Err(e) => Err(e),
            }
        } else {
            self.start_attached(threads, &id, waiting, checkpoint)
        };
        match result {
            Ok(RunAnswer { status: 0, .. }) => Answer::text(204, ""),
            Ok(answer) => refused(&answer.said, 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /containers/{id}/checkpoints`: `save vm` (D176), its body moby's
    /// checkpoint.CreateRequest; its errors dockerd's (CheckpointCreate).
    pub(super) fn engine_checkpoint_create(&self, given: &str, call: &Call<'_>) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("a checkpoint's body: {e}")),
        };
        let field = |name: &str| body.get(name);
        if field("CheckpointDir")
            .and_then(Value::as_str)
            .is_some_and(|d| !d.is_empty())
        {
            return Answer::error(400, CHECKPOINT_DIR);
        }
        let asked = field("CheckpointID").and_then(Value::as_str).unwrap_or_default();
        let leave_running = !field("Exit").and_then(Value::as_bool).unwrap_or(false);
        match self.save_snapshot(given, Some(asked), leave_running) {
            Ok(_) => Answer::text(201, ""),
            Err(e) => Answer::error(e.status(), e.checkpoint_refusal(given, asked)),
        }
    }

    /// `GET /containers/{id}/checkpoints`: its snapshots' names, as moby's
    /// checkpoint.Summary list.
    pub(super) fn engine_checkpoints(&self, given: &str, call: &Call<'_>) -> Answer {
        if call.query("dir").is_some_and(|d| !d.is_empty()) {
            return Answer::error(400, CHECKPOINT_DIR);
        }
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return refused(&e, 500),
        };
        let names: Vec<Value> = self
            .snapshot_names(&id)
            .into_iter()
            .map(|name| json!({ "Name": name }))
            .collect();
        Answer::json(200, &Value::Array(names))
    }

    /// `DELETE /containers/{id}/checkpoints/{checkpoint}`: `remove snapshot` (D176).
    pub(super) fn engine_checkpoint_remove(&self, given: &str, checkpoint: &str, call: &Call<'_>) -> Answer {
        if call.query("dir").is_some_and(|d| !d.is_empty()) {
            return Answer::error(400, CHECKPOINT_DIR);
        }
        match self.remove_checkpoint(given, checkpoint) {
            Ok(()) => Answer::text(204, ""),
            Err((status, e)) => Answer::error(status, e),
        }
    }

    /// Starts container `id` attached as `shards start -a` starts one, its output to the
    /// attaches `waiting`, and its stdin from the first that asked for it, where the
    /// container reads one. Answers once it runs, or did not start; its output goes on
    /// meanwhile on a thread of its own, until the run ends.
    fn start_attached<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: &str,
        waiting: Vec<Waiting>,
        resume: Option<String>,
    ) -> Result<RunAnswer, String> {
        let made = self.made_by(id).unwrap_or_default();
        let reads = made.interactive && waiting.iter().any(|w| w.streams[0]);
        let (out, out_by) = io::pipe().map_err(|e| format!("a pipe: {e}"))?;
        let (err, err_by) = io::pipe().map_err(|e| format!("a pipe: {e}"))?;
        let (stdin, stdin_by) = io::pipe().map_err(|e| format!("a pipe: {e}"))?;
        let null = File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        let mut run = Run {
            again: Some(id.to_string()),
            resume,
            interactive: reads,
            tty: made.tty,
            ..Run::default()
        };
        self.as_client(&mut run)?;
        // Registered before its run can end: its end waits for its output to reach them.
        let relaying = self.relaying(id);
        let stdin_fd = if reads { stdin.as_fd() } else { null.as_fd() };
        let conn = self.put_run(threads, &run, [stdin_fd, out_by.as_fd(), err_by.as_fd()])?;
        drop((out_by, err_by, stdin));
        let outputs = Outputs {
            to: Mutex::new(
                waiting
                    .iter()
                    .filter_map(|w| Some((w.to.try_clone().ok()?, w.streams)))
                    .collect(),
            ),
            tty: made.tty.is_some(),
        };
        // Made, or not: what the daemon said before is the stream's, or the failure's.
        loop {
            match shards_ipc::recv(&conn) {
                Ok(Some(m)) if m.kind == kind::CREATED => break,
                Ok(Some(m)) if m.kind == kind::EXIT => {
                    let mut said = String::new();
                    let _ = (&err).read_to_string(&mut said);
                    outputs.end();
                    return Ok(RunAnswer {
                        status: m.payload.first().copied().unwrap_or(1),
                        id: None,
                        said,
                    });
                }
                Ok(Some(m)) if m.kind == kind::RESTART => {
                    outputs.end();
                    return Err("the daemon is shutting down".into());
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    outputs.end();
                    return Err("the daemon ended the start before it was made".into());
                }
            }
        }
        // Its stdin, to the first attach that asked for one.
        if reads && let Some(to) = waiting.iter().find_map(|w| w.stdin.as_ref()) {
            let _ = to.send(stdin_by);
        }
        // Its terminal, resized through its connection while it runs.
        if made.tty.is_some()
            && let Ok(held) = conn.try_clone()
        {
            lock(&self.engine.terminals).insert(id.to_string(), held);
        }
        let ended = id.to_string();
        let spawned = std::thread::Builder::new()
            .name("engine-output".into())
            .spawn_scoped(threads, move || {
                std::thread::scope(|s| {
                    let outputs = &outputs;
                    let (copying, copies) = mpsc::channel::<()>();
                    // All its output is read, though every attach goes: its command writes
                    // on, and its log has it.
                    for (stream, from, name) in [(1, out, "engine-stdout"), (2, err, "engine-stderr")] {
                        let copying = copying.clone();
                        let _ = std::thread::Builder::new()
                            .name(name.into())
                            .spawn_scoped(s, move || {
                                outputs.copy(stream, from, true);
                                drop(copying);
                            });
                    }
                    drop(copying);
                    // Its end, which the VM says once the command has ended.
                    while let Ok(Some(m)) = shards_ipc::recv(&conn) {
                        if m.kind == kind::EXIT {
                            break;
                        }
                    }
                    // Told once its attaches have what it wrote (`Relaying`).
                    super::copied(&copies);
                    drop(relaying);
                });
                outputs.end();
                lock(&self.engine.terminals).remove(&ended);
                lock(&self.engine.stdins).remove(&ended);
            });
        if let Err(e) = spawned {
            log(format!("a run's output to its attaches: {e}"));
        }
        Ok(RunAnswer {
            status: 0,
            id: Some(id.to_string()),
            said: String::new(),
        })
    }

    /// `POST /containers/{id}/stop` and `kill`: as `shards stop` and `kill` ask, by the
    /// query's signal and time. One that does not run is not modified by a stop (304).
    pub(super) fn engine_stop<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        given: &str,
        call: &Call<'_>,
        kill: bool,
    ) -> Answer {
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return refused(&e, 500),
        };
        if !kill && !self.running(&id) {
            return Answer::text(304, "");
        }
        let mut argv = vec![if kill { "kill" } else { "stop" }];
        if let Some(signal) = call.query("signal").filter(|s| !s.is_empty()) {
            argv.extend(["--signal", signal]);
        }
        if !kill && let Some(t) = call.query("t").filter(|t| !t.is_empty()) {
            argv.extend(["--timeout", t]);
        }
        argv.push(&id);
        match self.ask_command(threads, &argv) {
            Ok((0, _, _)) => Answer::text(204, ""),
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `DELETE /containers/{id}`: removed as `shards rm` removes it, `force` and `v` its
    /// flags.
    pub(super) fn engine_remove<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        given: &str,
        call: &Call<'_>,
    ) -> Answer {
        if call.flag("link") {
            return Answer::error(
                400,
                "link: a run reaches another by its network's names; there are no legacy links to remove",
            );
        }
        let mut argv = vec!["rm"];
        if call.flag("force") {
            argv.push("--force");
        }
        if call.flag("v") {
            argv.push("--volumes");
        }
        argv.push(given);
        match self.ask_command(threads, &argv) {
            Ok((0, _, _)) => Answer::text(204, ""),
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /containers/{id}/wait`: its status once it is not running (`not-running`,
    /// at once if it is not), once it next ends (`next-exit`, though it has yet to start),
    /// or once it is removed (`removed`). Its waiter is registered before its head is
    /// written, as dockerd's is: a client starts the container once it has the head
    /// (`docker run`), and an end that comes then is heard.
    pub(super) fn engine_wait(&self, conn: &UnixStream, given: &str, call: &Call<'_>) -> Done {
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return Done::Answer(refused(&e, 500)),
        };
        let condition = match call.query("condition").unwrap_or_default() {
            "" | "not-running" => "not-running",
            c @ ("next-exit" | "removed") => c,
            other => {
                return Done::Answer(Answer::error(
                    400,
                    format!("invalid condition: {other:?}: not-running, next-exit or removed"),
                ));
            }
        };
        let registry = lock(&self.containers);
        let Some((state, recorded)) = registry.made(&id).map(|c| (c.state, c.exit_code)) else {
            return Done::Answer(Answer::error(404, format!("No such container: {given}")));
        };
        let waiter = match condition {
            "not-running" => self.register_waiter(registry, &id, false),
            "next-exit" => self.register_waiter(registry, &id, true),
            // `removed`: its end first where it runs or has yet to start.
            _ if matches!(state, Life::Running | Life::Created) => self.register_waiter(registry, &id, true),
            _ => {
                drop(registry);
                Some(Err(recorded.unwrap_or(0)))
            }
        };
        let Some(waiter) = waiter else {
            return Done::Answer(Answer::error(
                500,
                format!("container {id}: its end cannot be waited for"),
            ));
        };
        let Ok(mut stream) = Stream::start(conn, call.head, 200, "application/json") else {
            if let Ok((number, _)) = &waiter {
                self.forget_waiter(&id, *number);
            }
            return Done::Close;
        };
        let code = match waiter {
            Err(code) => Some(code),
            Ok(waiter) => self.await_waiter(&id, waiter, None, Some(conn)),
        };
        let Some(code) = code else {
            return Done::Close;
        };
        if condition == "removed" && self.await_removal(&id, conn).is_none() {
            return Done::Close;
        }
        match stream
            .json(&json!({ "StatusCode": code }))
            .and_then(|()| stream.end())
        {
            Ok(()) => Done::Keep,
            Err(_) => Done::Close,
        }
    }

    /// Waits for container `id` to be removed: `None` once `client` goes first.
    fn await_removal(&self, id: &str, client: &UnixStream) -> Option<()> {
        let mut registry = lock(&self.containers);
        while registry.made(id).is_some() {
            if hung_up(client) {
                return None;
            }
            registry = self
                .arrived
                .wait_timeout(registry, Duration::from_millis(100))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Some(())
    }

    /// Takes the attach numbered `number`, waiting for container `id`'s start, out of
    /// those waiting: whether it was still there, which no start had taken.
    fn unwait(&self, id: &str, number: u64) -> bool {
        let mut waiting = lock(&self.engine.waiting);
        let Some(list) = waiting.get_mut(id) else {
            return false;
        };
        let before = list.len();
        list.retain(|w| w.number != number);
        let taken = list.len() < before;
        if list.is_empty() {
            waiting.remove(id);
        }
        taken
    }

    /// `POST /containers/{id}/attach`: the connection taken over for the container's
    /// streams. A container yet to start is attached as it starts (its start's output
    /// goes here); a running one, as `shards attach` attaches.
    pub(super) fn engine_attach<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        given: &str,
        call: &Call<'_>,
    ) -> Done {
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return Done::Answer(refused(&e, 500)),
        };
        let streams = [call.flag("stdin"), call.flag("stdout"), call.flag("stderr")];
        if !streams.iter().any(|s| *s) {
            return Done::Answer(Answer::error(400, "an attach asks for stdin, stdout or stderr"));
        }
        let made = self.made_by(&id).unwrap_or_default();
        let state = lock(&self.containers).get(&id).map(|c| c.state);
        let running = self.running(&id);
        if !running && state != Some(Life::Created) {
            return Done::Answer(Answer::error(
                409,
                format!("container {id} is not running: start it to attach to it"),
            ));
        }
        if call.flag("logs") {
            return Done::Answer(Answer::error(
                400,
                "logs: an attach replays no output; ask for its logs (GET /containers/{id}/logs)",
            ));
        }
        let keys = match detach_keys(call.query("detachKeys")) {
            Ok(keys) => keys,
            Err(e) => return Done::Answer(Answer::error(400, e)),
        };
        let tty = made.tty.is_some();
        let running_attach = |conn: &UnixStream| {
            if http::hijack(conn, call.head, &call.version, !tty).is_ok() {
                self.attach_running(threads, conn, &id, streams, &made, &keys);
            }
            Done::Close
        };
        if running {
            return running_attach(conn);
        }
        let Ok(to) = conn.try_clone() else {
            return Done::Close;
        };
        // Waiting for its start, which writes its output here and ends the connection: its
        // head written, and the attach registered, under the lock its start takes, so that
        // the start finds it once its client has the head, and writes after the head.
        let (stdin_to, stdin_at) = mpsc::channel();
        let number = self
            .engine
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        {
            let mut waiting = lock(&self.engine.waiting);
            if self.running(&id) {
                drop(waiting);
                return running_attach(conn);
            }
            if http::hijack(conn, call.head, &call.version, !tty).is_err() {
                return Done::Close;
            }
            waiting.entry(id.clone()).or_default().push(Waiting {
                number,
                streams,
                to,
                stdin: streams[0].then_some(stdin_to),
            });
        }
        let stdin = loop {
            match stdin_at.recv_timeout(LOOK_AGAIN) {
                Ok(stdin) => break Some(stdin),
                // Started without its stdin, or its stdin another's.
                Err(RecvTimeoutError::Disconnected) => break None,
                Err(RecvTimeoutError::Timeout) => {
                    // Removed before it started: so is its attach.
                    if lock(&self.containers).made(&id).is_none() {
                        self.unwait(&id, number);
                        return Done::Close;
                    }
                    // Started by a client that takes no attach (shards' CLI): attached as
                    // a running one is, where no start took it meanwhile.
                    if self.running(&id) && self.unwait(&id, number) {
                        self.attach_running(threads, conn, &id, streams, &made, &keys);
                        return Done::Close;
                    }
                }
            }
        };
        match stdin {
            // Its client's input, to the command's stdin until the client closes it, which
            // closes the command's; or until it detaches, which keeps it open for later.
            Some(mut stdin) => {
                if input(conn, &mut stdin, tty.then_some(&keys)) {
                    shards_ipc::end(conn);
                    lock(&self.engine.stdins).insert(id, stdin);
                }
            }
            // What its client sends is not read: it ends as the start's output does.
            None => {
                let mut sink = [0u8; 4096];
                while matches!(shards_ipc::read(conn, &mut sink), Ok(n) if n > 0) {}
            }
        }
        Done::Close
    }

    /// `POST /containers/{id}/resize` and `/exec/{id}/resize`: the terminal of what a
    /// client attached to, `h` rows by `w` columns, resized as `shards attach` resizes it.
    pub(super) fn engine_resize(&self, key: &str, call: &Call<'_>) -> Answer {
        let size = |k: &str| call.query(k).and_then(|v| v.parse::<u16>().ok());
        let (Some(rows), Some(cols)) = (size("h"), size("w")) else {
            return Answer::error(
                400,
                "a resize gives its rows (h) and columns (w), each 0 to 65535",
            );
        };
        let key = self.resolve(key).unwrap_or_else(|_| key.to_string());
        let terminals = lock(&self.engine.terminals);
        let Some(conn) = terminals.get(&key) else {
            return Answer::error(
                409,
                format!("{key}: no client is attached to a terminal of it, which is what a resize resizes"),
            );
        };
        let [a, b] = rows.to_be_bytes();
        let [c, d] = cols.to_be_bytes();
        match shards_ipc::send(conn, kind::RESIZE, &[a, b, c, d], &[]) {
            Ok(()) => Answer::text(200, ""),
            Err(e) => Answer::error(409, format!("{key}: its terminal has ended: {e}")),
        }
    }

    /// Attaches `conn` to running container `id`, made by `made`, as `shards attach`
    /// attaches: its output from here on, and its stdin where both ask for one, until its
    /// command ends, or its client detaches by `keys` from its terminal.
    fn attach_running<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        id: &str,
        streams: [bool; 3],
        made: &Run,
        keys: &[u8],
    ) {
        let (tty, reads) = (made.tty.is_some(), made.interactive);
        let pipes = (|| -> io::Result<_> { Ok((io::pipe()?, io::pipe()?, io::pipe()?)) })();
        let Ok(((out, out_by), (err, err_by), (stdin, mut stdin_by))) = pipes else {
            return;
        };
        let request = shards_ipc::Attach {
            container: id.to_string(),
            stdin: streams[0] && reads,
            daemon: self.identity,
        };
        // Registered before it attaches: the run's end waits for its output to reach it.
        let relaying = self.relaying(id);
        let Ok((ours, theirs)) = UnixStream::pair() else {
            return;
        };
        self.take(threads, theirs);
        if shards_ipc::send(
            &ours,
            kind::ATTACH,
            &request.encode(),
            &[stdin.as_fd(), out_by.as_fd(), err_by.as_fd()],
        )
        .is_err()
        {
            return;
        }
        drop((out_by, err_by, stdin));
        let Ok(to) = conn.try_clone() else {
            return;
        };
        let outputs = Outputs {
            to: Mutex::new(vec![(to, streams)]),
            tty,
        };
        if tty && let Ok(held) = ours.try_clone() {
            lock(&self.engine.terminals).insert(id.to_string(), held);
        }
        std::thread::scope(|s| {
            let outputs = &outputs;
            let (copying, copies) = mpsc::channel::<()>();
            // Its streams until it goes: then the VM's attach ends as their pipes close.
            for (stream, from, name) in [(1, out, "engine-stdout"), (2, err, "engine-stderr")] {
                let copying = copying.clone();
                let _ = std::thread::Builder::new()
                    .name(name.into())
                    .spawn_scoped(s, move || {
                        outputs.copy(stream, from, false);
                        drop(copying);
                    });
            }
            drop(copying);
            if request.stdin {
                let _ = std::thread::Builder::new()
                    .name("engine-stdin".into())
                    .spawn_scoped(s, move || {
                        if input(conn, &mut stdin_by, tty.then_some(keys)) {
                            outputs.end();
                        }
                    });
            }
            while let Ok(Some(m)) = shards_ipc::recv(&ours) {
                if m.kind == kind::EXIT {
                    break;
                }
            }
            // Its run's end told once it has what the run wrote (`Relaying`).
            super::copied(&copies);
            drop(relaying);
            outputs.end();
        });
        lock(&self.engine.terminals).remove(id);
    }

    /// `POST /containers/{id}/pause` and `unpause`: `shards pause vm` and `unpause vm`.
    /// The whole microVM frozen where it is, its kernel with it, until it is let run on.
    pub(super) fn engine_pause<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        given: &str,
        freeze: bool,
    ) -> Answer {
        match self.ask_command(threads, &[if freeze { "pause" } else { "unpause" }, given]) {
            Ok((0, _, _)) => Answer::text(204, ""),
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /containers/{id}/restart`: `shards restart vm`. Stopped, by `signal` and `t`
    /// where given, then started again, its files as it left them.
    pub(super) fn engine_restart<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        given: &str,
        call: &Call<'_>,
    ) -> Answer {
        let timeout = match call.query("t").filter(|t| !t.is_empty()).map(str::parse::<i64>) {
            None => None,
            Some(Ok(t)) => Some(t),
            Some(Err(_)) => return Answer::error(400, "t: a restart's grace is a whole number of seconds"),
        };
        let mut run = Run {
            again: Some(given.to_string()),
            restart: true,
            detach: true,
            stop_signal: call.query("signal").filter(|s| !s.is_empty()).map(str::to_string),
            stop_timeout: timeout,
            ..Run::default()
        };
        if let Err(e) = self.as_client(&mut run) {
            return Answer::error(500, e);
        }
        match self.run_detached(threads, &run) {
            Ok(RunAnswer { status: 0, .. }) => Answer::text(204, ""),
            Ok(answer) => refused(&answer.said, 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /containers/{id}/rename?name=`: `shards rename vm`.
    pub(super) fn engine_rename<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        given: &str,
        call: &Call<'_>,
    ) -> Answer {
        let Some(name) = call.query("name").filter(|n| !n.is_empty()) else {
            return Answer::error(400, "a rename gives the microVM's new name (name)");
        };
        match self.ask_command(threads, &["rename", given, name]) {
            Ok((0, _, _)) => Answer::text(204, ""),
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 500),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /containers/{id}/update`: `shards update vm`. Its limits and restart policy
    /// changed, checked as at its making, and written into its guest's cgroup at once.
    /// What a running microVM's update cannot change, asked, is refused, each named.
    pub(super) fn engine_update<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        given: &str,
        call: &Call<'_>,
    ) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("an update's body: {e}")),
        };
        let asked = |v: &Value| match v {
            Value::Null => false,
            Value::Number(n) => n.as_f64() != Some(0.0),
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
            Value::Bool(b) => *b,
        };
        const NUMBERS: [(&str, &str); 8] = [
            ("Memory", "--memory"),
            ("MemoryReservation", "--memory-reservation"),
            ("MemorySwap", "--memory-swap"),
            ("CpuShares", "--cpu-shares"),
            ("CpuPeriod", "--cpu-period"),
            ("CpuQuota", "--cpu-quota"),
            ("PidsLimit", "--pids-limit"),
            ("BlkioWeight", "--blkio-weight"),
        ];
        const TEXTS: [(&str, &str); 2] = [("CpusetCpus", "--cpuset-cpus"), ("CpusetMems", "--cpuset-mems")];
        let mut argv: Vec<String> = vec!["update".into()];
        let mut unheld = Vec::new();
        for (key, value) in body.as_object().into_iter().flatten() {
            if !asked(value) {
                continue;
            }
            if let Some((_, flag)) = NUMBERS.iter().find(|(k, _)| k == key) {
                match value.as_i64() {
                    Some(n) => argv.extend([(*flag).to_string(), n.to_string()]),
                    None => return Answer::error(400, format!("{key}: a whole number")),
                }
            } else if let Some((_, flag)) = TEXTS.iter().find(|(k, _)| k == key) {
                argv.extend([
                    (*flag).to_string(),
                    value.as_str().unwrap_or_default().to_string(),
                ]);
            } else if key == "NanoCpus" {
                let Some(n) = value.as_i64().filter(|n| *n > 0) else {
                    return Answer::error(400, "NanoCpus: a positive number of billionths of a CPU");
                };
                argv.extend([
                    "--cpus".into(),
                    format!("{}.{:09}", n / 1_000_000_000, n % 1_000_000_000),
                ]);
            } else if key == "RestartPolicy" {
                let name = value.get("Name").and_then(Value::as_str).unwrap_or_default();
                if !name.is_empty() {
                    let count = value
                        .get("MaximumRetryCount")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let policy = if count > 0 {
                        format!("{name}:{count}")
                    } else {
                        name.to_string()
                    };
                    argv.extend(["--restart".into(), policy]);
                }
            } else {
                unheld.push(key.clone());
            }
        }
        if !unheld.is_empty() {
            return Answer::error(
                400,
                format!(
                    "{}: a running microVM's update changes its memory, CPUs, PIDs, block I/O weight and restart policy; these are fixed as it is made",
                    unheld.join(", ")
                ),
            );
        }
        argv.push(given.to_string());
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        match self.ask_command(threads, &argv) {
            Ok((0, out, _)) => {
                let warnings: Vec<String> = String::from_utf8_lossy(&out)
                    .lines()
                    .skip(1)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                Answer::json(200, &json!({ "Warnings": warnings }))
            }
            Ok((_, _, err)) => refused(&String::from_utf8_lossy(&err), 400),
            Err(e) => Answer::error(500, e),
        }
    }

    /// `POST /containers/prune`: `shards prune vm`. The stopped microVMs the filters keep
    /// removed: their IDs, and the bytes reclaimed, counted, not rounded for a reader.
    pub(super) fn engine_prune(&self, call: &Call<'_>) -> Answer {
        let given = match filters(call.query("filters")) {
            Ok(f) => f.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
            Err(e) => return Answer::error(400, e),
        };
        let now = i128::try_from(crate::containers::now()).unwrap_or(i128::MAX);
        match self.prune_microvms(&given, now, i64::from(crate::cli::utc_offset())) {
            Ok((removed, bytes)) => {
                let ids: Vec<&str> = removed.iter().map(|(id, _)| id.as_str()).collect();
                Answer::json(200, &json!({ "ContainersDeleted": ids, "SpaceReclaimed": bytes }))
            }
            Err(e) => Answer::error(400, plain(&e)),
        }
    }

    /// `GET /containers/{id}/logs`: its log as `shards logs` reads it, by the query's
    /// flags, each piece a frame of its stream, or raw for a terminal's.
    pub(super) fn engine_logs<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        given: &str,
        call: &Call<'_>,
    ) -> Done {
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return Done::Answer(refused(&e, 500)),
        };
        let (stdout, stderr) = (call.flag("stdout"), call.flag("stderr"));
        if !stdout && !stderr {
            return Done::Answer(Answer::error(400, "logs: ask for stdout, stderr or both"));
        }
        let mut argv = vec!["logs"];
        for (key, flag) in [
            ("follow", "--follow"),
            ("timestamps", "--timestamps"),
            ("details", "--details"),
        ] {
            if call.flag(key) {
                argv.push(flag);
            }
        }
        // A time of 0 is none, as the client sends it when not given.
        for (key, flag) in [("since", "--since"), ("until", "--until")] {
            if let Some(v) = call.query(key).filter(|v| !v.is_empty() && *v != "0") {
                argv.extend([flag, v]);
            }
        }
        if let Some(v) = call.query("tail").filter(|v| !v.is_empty()) {
            argv.extend(["--tail", v]);
        }
        argv.push(&id);
        let tty = self.made_by(&id).is_some_and(|r| r.tty.is_some());
        let ipc = match self.command_conn(threads, &argv) {
            Ok(ipc) => ipc,
            Err(e) => return Done::Answer(Answer::error(500, e)),
        };
        let kind = if !tty && super::at_least(&call.version, "1.42") {
            MULTIPLEXED
        } else {
            RAW
        };
        let Ok(mut stream) = Stream::start(conn, call.head, 200, kind) else {
            return Done::Close;
        };
        loop {
            if super::gone_first(&ipc, conn) {
                return Done::Close;
            }
            let sent = match shards_ipc::recv(&ipc) {
                Ok(Some(m)) if m.kind == kind::OUT && stdout => {
                    stream.send(&if tty { m.payload } else { frame(1, &m.payload) })
                }
                Ok(Some(m)) if m.kind == kind::ERR && stderr => {
                    stream.send(&if tty { m.payload } else { frame(2, &m.payload) })
                }
                Ok(Some(m)) if m.kind == kind::END => break,
                Ok(Some(_)) => Ok(()),
                Ok(None) | Err(_) => return Done::Close,
            };
            if sent.is_err() {
                return Done::Close;
            }
        }
        match stream.end() {
            Ok(()) => Done::Keep,
            Err(_) => Done::Close,
        }
    }
}

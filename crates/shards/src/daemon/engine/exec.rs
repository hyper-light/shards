//! Execs' calls (D170): a command made to run in a running container, then started as
//! `shards exec` runs one, and its end kept for its client to read.

use std::fs::File;
use std::io::{self, Read as _};
use std::sync::Mutex;

use serde_json::{Value, json};
use shards_ipc::Stream as UnixStream;
use shards_ipc::fd::AsFd as _;
use shards_ipc::kind;

use super::containers::{Outputs, detach_keys, input};
use super::http::{self, Answer};
use super::{Call, Done, refused};
use crate::containers::Disk;
use crate::daemon::{Daemon, Threads, lock};

/// An exec made: what it runs, where, and how it ended.
pub(in crate::daemon) struct Instance {
    /// Its container's ID.
    container: String,
    request: shards_ipc::Exec,
    /// The streams it attaches: stdin, stdout, stderr.
    streams: [bool; 3],
    /// A terminal's detach keys.
    keys: Vec<u8>,
    running: bool,
    exit_code: Option<i64>,
}

fn words(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn flag(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// A terminal's rows and columns, as ConsoleSize gives them.
fn console(v: &Value) -> (u16, u16) {
    let size = |i: usize| {
        v.get("ConsoleSize")
            .and_then(|c| c.get(i))
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .unwrap_or(0)
    };
    (size(0), size(1))
}

impl<D: Disk> Daemon<D> {
    /// The container exec `exec_id` runs in, where there is such an exec.
    pub(super) fn exec_container(&self, exec_id: &str) -> Option<String> {
        lock(&self.engine.execs).get(exec_id).map(|i| i.container.clone())
    }

    /// `POST /containers/{id}/exec`: an exec made, in a container that runs, to start
    /// later.
    pub(super) fn engine_exec_create(&self, given: &str, call: &Call<'_>) -> Answer {
        let body: Value = match serde_json::from_slice(&call.body) {
            Ok(body) => body,
            Err(e) => return Answer::error(400, format!("an exec's body: {e}")),
        };
        let id = match self.resolve(given) {
            Ok(id) => id,
            Err(e) => return refused(&e, 500),
        };
        if !self.running(&id) {
            return Answer::error(
                409,
                format!("container {id} is not running: start it to run a command in it"),
            );
        }
        let cmd = words(&body, "Cmd");
        if cmd.is_empty() {
            return Answer::error(400, "an exec names the command it runs (Cmd)");
        }
        let streams = [
            flag(&body, "AttachStdin"),
            flag(&body, "AttachStdout"),
            flag(&body, "AttachStderr"),
        ];
        let tty = flag(&body, "Tty");
        let keys = match detach_keys(body.get("DetachKeys").and_then(Value::as_str)) {
            Ok(keys) => keys,
            Err(e) => return Answer::error(400, e),
        };
        let mut request = shards_ipc::Exec {
            container: id.clone(),
            cmd,
            env: words(&body, "Env"),
            user: text(&body, "User"),
            workdir: text(&body, "WorkingDir"),
            interactive: streams[0],
            tty: tty.then(|| console(&body)),
            // An API's client has its own terminal, which the daemon has no word on.
            stdin_terminal: true,
            privileged: flag(&body, "Privileged"),
            daemon: self.identity,
            ..shards_ipc::Exec::default()
        };
        // What it ends keeps no exec of a container that has gone.
        self.forget_execs();
        let exec_id = match crate::containers::new_id() {
            Ok(exec_id) => exec_id,
            Err(e) => return Answer::error(500, format!("an exec's ID: {e}")),
        };
        // Said as it is made, with the ID its client has, which its start and end are said
        // with too (moby daemon/exec.go, ContainerExecCreate).
        self.exec_event(&id, &request.cmd, "exec_create", &exec_id);
        request.id = exec_id.clone();
        lock(&self.engine.execs).insert(
            exec_id.clone(),
            Instance {
                container: id,
                request,
                streams,
                keys,
                running: false,
                exit_code: None,
            },
        );
        Answer::json(201, &json!({ "Id": exec_id }))
    }

    /// `POST /exec/{id}/start`: run as `shards exec` runs it, detached, or on `conn`
    /// taken over: its output, multiplexed unless a terminal's, and its stdin where it
    /// attaches one. Its end is kept before its stream ends, for its client to ask.
    pub(super) fn engine_exec_start<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        conn: &UnixStream,
        exec_id: &str,
        call: &Call<'_>,
    ) -> Done {
        let body: Value = if call.body.is_empty() {
            json!({})
        } else {
            match serde_json::from_slice(&call.body) {
                Ok(body) => body,
                Err(e) => return Done::Answer(Answer::error(400, format!("an exec start's body: {e}"))),
            }
        };
        let detach = flag(&body, "Detach");
        let (mut request, streams, keys) = {
            let mut execs = lock(&self.engine.execs);
            let Some(instance) = execs.get_mut(exec_id) else {
                return Done::Answer(Answer::error(404, format!("No such exec instance: {exec_id}")));
            };
            if instance.running || instance.exit_code.is_some() {
                return Done::Answer(Answer::error(409, format!("exec {exec_id} has run already")));
            }
            if !self.running(&instance.container) {
                return Done::Answer(Answer::error(
                    409,
                    format!("container {} is not running", instance.container),
                ));
            }
            instance.running = true;
            (instance.request.clone(), instance.streams, instance.keys.clone())
        };
        if flag(&body, "Tty") && request.tty.is_none() {
            request.tty = Some(console(&body));
        }
        request.detach = detach;
        let tty = request.tty.is_some();
        let exec_name = exec_id.to_string();
        let ended = |code: Option<i64>| {
            if let Some(instance) = lock(&self.engine.execs).get_mut(exec_id) {
                instance.running = false;
                instance.exit_code = Some(code.unwrap_or(-1));
            }
        };
        let pipes =
            (|| -> io::Result<_> { Ok((io::pipe()?, io::pipe()?, io::pipe()?, File::open("/dev/null")?)) })();
        let Ok(((out, out_by), (err, err_by), (stdin, mut stdin_by), null)) = pipes else {
            ended(None);
            return Done::Answer(Answer::error(500, "an exec's pipes"));
        };
        let Ok((ours, theirs)) = UnixStream::pair() else {
            ended(None);
            return Done::Answer(Answer::error(500, "an exec's socket pair"));
        };
        // Registered before it can end: its `exec_die` waits for its output to reach its
        // client (engine `Relaying`), which a detached exec has none of.
        let relaying = (!detach).then(|| self.relaying(exec_id));
        self.take(threads, theirs);
        let reads = request.interactive && !detach;
        let fds = if detach {
            [null.as_fd(), null.as_fd(), err_by.as_fd()]
        } else {
            [
                if reads { stdin.as_fd() } else { null.as_fd() },
                out_by.as_fd(),
                err_by.as_fd(),
            ]
        };
        if let Err(e) = shards_ipc::send(&ours, kind::EXEC, &request.encode(), &fds) {
            ended(None);
            return Done::Answer(Answer::error(500, format!("asking the daemon: {e}")));
        }
        drop((out_by, err_by, stdin, null));
        if detach {
            // Its answer once the command is under way, or why it is not.
            let mut status = None;
            while let Ok(Some(m)) = shards_ipc::recv(&ours) {
                if m.kind == kind::EXIT {
                    status = m.payload.first().copied();
                    break;
                }
            }
            let mut said = String::new();
            let _ = (&err).read_to_string(&mut said);
            if status != Some(0) {
                ended(status.map(i64::from));
                return Done::Answer(refused(&said, 500));
            }
            // Started: its end, which the VM says after, is kept as it comes.
            let followed = std::thread::Builder::new()
                .name("engine-exec".into())
                .spawn_scoped(threads, move || {
                    let mut code = None;
                    while let Ok(Some(m)) = shards_ipc::recv(&ours) {
                        if m.kind == kind::EXIT {
                            code = m.payload.first().copied().map(i64::from);
                            break;
                        }
                    }
                    if let Some(instance) = lock(&self.engine.execs).get_mut(&exec_name) {
                        instance.running = false;
                        instance.exit_code = Some(code.unwrap_or(-1));
                    }
                });
            if let Err(e) = followed {
                crate::daemon::log(format!("following exec {exec_id}: {e}"));
            }
            return Done::Answer(Answer::text(200, ""));
        }
        if http::hijack(conn, call.head, &call.version, !tty).is_err() {
            return Done::Close;
        }
        let Ok(to) = conn.try_clone() else {
            return Done::Close;
        };
        let outputs = Outputs {
            to: Mutex::new(vec![(to, streams)]),
            tty,
        };
        if tty && let Ok(held) = ours.try_clone() {
            lock(&self.engine.terminals).insert(exec_id.to_string(), held);
        }
        let code = std::thread::scope(|s| {
            let outputs = &outputs;
            let (copying, copies) = std::sync::mpsc::channel::<()>();
            let copied =
                [(1, out, "engine-stdout"), (2, err, "engine-stderr")].map(|(stream, from, name)| {
                    let copying = copying.clone();
                    std::thread::Builder::new()
                        .name(name.into())
                        .spawn_scoped(s, move || {
                            outputs.copy(stream, from, true);
                            drop(copying);
                        })
                });
            drop(copying);
            if reads {
                let keys = &keys;
                let _ = std::thread::Builder::new()
                    .name("engine-stdin".into())
                    .spawn_scoped(s, move || {
                        if input(conn, &mut stdin_by, tty.then_some(keys.as_slice())) {
                            outputs.end();
                        }
                    });
            }
            let mut code = None;
            while let Ok(Some(m)) = shards_ipc::recv(&ours) {
                if m.kind == kind::EXIT {
                    code = m.payload.first().copied().map(i64::from);
                    break;
                }
            }
            // Its `exec_die` told once its client has what it wrote (`Relaying`).
            super::copied(&copies);
            drop(relaying);
            // All its output, then its end kept, before its client hears the stream end.
            for copy in copied.into_iter().flatten() {
                let _ = copy.join();
            }
            ended(code);
            outputs.end();
            code
        });
        let _ = code;
        lock(&self.engine.terminals).remove(exec_id);
        Done::Close
    }

    /// `GET /exec/{id}/json`: whether it runs, how it ended, and what it runs.
    pub(super) fn engine_exec_inspect(&self, exec_id: &str) -> Answer {
        let execs = lock(&self.engine.execs);
        let Some(instance) = execs.get(exec_id) else {
            return Answer::error(404, format!("No such exec instance: {exec_id}"));
        };
        let r = &instance.request;
        let (entrypoint, arguments) = r
            .cmd
            .split_first()
            .map_or((String::new(), &[][..]), |(e, a)| (e.clone(), a));
        Answer::json(
            200,
            &json!({
                "ID": exec_id,
                "Running": instance.running,
                "ExitCode": instance.exit_code,
                "ProcessConfig": {
                    "tty": r.tty.is_some(),
                    "entrypoint": entrypoint,
                    "arguments": arguments,
                    "privileged": r.privileged,
                    "user": r.user,
                },
                "OpenStdin": instance.streams[0],
                "OpenStderr": instance.streams[2],
                "OpenStdout": instance.streams[1],
                "CanRemove": !instance.running,
                "ContainerID": instance.container,
                "DetachKeys": "",
                "Pid": 0,
            }),
        )
    }

    /// Forgets the execs of containers that are gone.
    fn forget_execs(&self) {
        let registry = lock(&self.containers);
        lock(&self.engine.execs).retain(|_, i| registry.made(&i.container).is_some());
    }
}

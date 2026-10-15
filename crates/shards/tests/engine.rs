//! The Engine API (D170), end to end on real microVMs: what programs that make sandboxes
//! ask of a container engine, asked of `engine.sock` in a home as they ask it, raw, and by
//! the docker CLI where this host has one (its `DOCKER_HOST` the socket, its config an
//! empty directory, so that it reaches nothing else). Real VMs, booted from SHARDS_KERNEL
//! and SHARDS_INIT, which the daemon has from the client that started it; the image comes
//! from a loopback registry (tests/common, `served`), and its entrypoint is the test guest.

#![cfg(unix)]
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

mod common;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use common::{Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

/// `shards ARGS` in `home`, whose runs boot the test kernel and init.
fn shards_in(home: &Path, args: &[&str]) -> Run {
    let (kernel, init) = (kernel(), guest_init());
    let env: [(&str, &OsStr); 3] = [
        ("SHARDS_HOME", home.as_os_str()),
        ("SHARDS_KERNEL", kernel.as_os_str()),
        ("SHARDS_INIT", init.as_os_str()),
    ];
    run_shards_env(&[], args, &env, TIMEOUT)
}

/// A home whose daemon runs, the test image pulled into it: `shards pull` starts the
/// daemon, which keeps the test's kernel and init from it.
fn home(name: &str) -> Option<(TempDir, String)> {
    if cannot_run_vms() {
        return None;
    }
    let (image, _) = served();
    let home = TempDir::new(name);
    let pulled = shards_in(&home, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{}", pulled.stderr);
    Some((home, image))
}

/// An answer: its status, its fields (names in lower case), its content.
#[derive(Debug)]
struct Answered {
    status: u16,
    fields: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answered {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A client of the API on one connection, its calls one after another.
struct Api {
    conn: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Api {
    fn new(home: &Path) -> Api {
        let conn = UnixStream::connect(home.join("engine.sock")).unwrap();
        conn.set_read_timeout(Some(TIMEOUT)).unwrap();
        let reader = BufReader::new(conn.try_clone().unwrap());
        Api { conn, reader }
    }

    fn send(&mut self, method: &str, path: &str, body: Option<&Value>, fields: &[(&str, &str)]) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut out = format!("{method} {path} HTTP/1.1\r\nHost: api\r\n");
        for (k, v) in fields {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        if !body.is_empty() {
            out.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            ));
        }
        out.push_str("\r\n");
        out.push_str(&body);
        self.conn.write_all(out.as_bytes()).unwrap();
    }

    /// An answer's status and fields.
    fn head(&mut self) -> (u16, Vec<(String, String)>) {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let status = line
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("a status line: {line:?}"));
        let mut fields = Vec::new();
        loop {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                return (status, fields);
            }
            let (k, v) = line.split_once(':').unwrap();
            fields.push((k.to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    /// An answer whole: its content by its length, or its chunks.
    fn answer(&mut self, method: &str) -> Answered {
        let (status, fields) = self.head();
        let get = |k: &str| fields.iter().find(|(f, _)| f == k).map(|(_, v)| v.clone());
        let mut body = Vec::new();
        if get("transfer-encoding").as_deref() == Some("chunked") {
            loop {
                let mut size = String::new();
                self.reader.read_line(&mut size).unwrap();
                let n = usize::from_str_radix(size.trim(), 16).unwrap();
                let mut chunk = vec![0u8; n + 2];
                self.reader.read_exact(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
        } else if let Some(n) = get("content-length").filter(|_| method != "HEAD") {
            body = vec![0u8; n.parse().unwrap()];
            self.reader.read_exact(&mut body).unwrap();
        }
        Answered { status, fields, body }
    }

    fn call(&mut self, method: &str, path: &str, body: Option<Value>) -> Answered {
        self.send(method, path, body.as_ref(), &[]);
        self.answer(method)
    }

    /// Asks for the connection to be taken over for a stream (`Connection: Upgrade`): its
    /// status and its content's type, once the engine has written them.
    fn upgrade(&mut self, path: &str, body: Option<Value>) -> (u16, String) {
        self.send(
            "POST",
            path,
            body.as_ref(),
            &[("Connection", "Upgrade"), ("Upgrade", "tcp")],
        );
        let (status, fields) = self.head();
        let kind = fields
            .iter()
            .find(|(k, _)| k == "content-type")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        (status, kind)
    }

    /// All a stream sends until it ends.
    fn rest(mut self) -> Vec<u8> {
        let mut rest = Vec::new();
        self.reader.read_to_end(&mut rest).unwrap();
        rest
    }

    /// [`upgrade`](Self::upgrade), and all the stream sends.
    fn upgraded(mut self, path: &str, body: Option<Value>) -> (u16, String, Vec<u8>) {
        let (status, kind) = self.upgrade(path, body);
        (status, kind, self.rest())
    }
}

/// A multiplexed stream's stdout and stderr (stdcopy's frames).
fn demux(mut bytes: &[u8]) -> (String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    while bytes.len() >= 8 {
        let n = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let payload = &bytes[8..8 + n];
        match bytes[0] {
            1 => out.extend_from_slice(payload),
            2 => err.extend_from_slice(payload),
            other => panic!("a frame of stream {other}"),
        }
        bytes = &bytes[8 + n..];
    }
    assert!(bytes.is_empty(), "a frame cut short: {bytes:?}");
    (
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    )
}

/// The engine answers on its socket file in the home: `_ping`, and `version` with the
/// API's versions, which paths name or leave to the latest; one too old or too new, and a
/// call it does not serve, are refused in shards' words.
#[test]
fn the_engine_answers_on_a_socket_file_in_the_home() {
    let Some((home, _)) = home("eng-ping") else {
        return;
    };
    let mut api = Api::new(&home);
    let ping = api.call("GET", "/_ping", None);
    assert_eq!((ping.status, ping.text().as_str()), (200, "OK"), "{ping:?}");
    assert_eq!(ping.field("api-version"), Some("1.56"));
    assert_eq!(ping.field("ostype"), Some("linux"));
    let head = api.call("HEAD", "/_ping", None);
    assert_eq!((head.status, head.body.len()), (200, 0), "{head:?}");
    for path in ["/version", "/v1.56/version", "/v1.40/version"] {
        let v = api.call("GET", path, None);
        assert_eq!(v.status, 200, "{path}: {v:?}");
        let v = v.json();
        assert_eq!(
            (&v["ApiVersion"], &v["MinAPIVersion"], &v["Os"]),
            (&json!("1.56"), &json!("1.40"), &json!("linux")),
            "{v}"
        );
        assert_eq!(v["Components"][0]["Name"], "Engine", "{v}");
    }
    for path in ["/v1.39/version", "/v1.57/version"] {
        let refused = api.call("GET", path, None);
        assert_eq!(refused.status, 400, "{path}: {refused:?}");
        assert!(
            refused.json()["message"]
                .as_str()
                .unwrap()
                .contains("shards serves 1.40 to 1.56"),
            "{refused:?}"
        );
    }
    let unknown = api.call("GET", "/swarm", None);
    assert_eq!(unknown.status, 404, "{unknown:?}");
}

/// A container made from a create's body is a microVM: inspected as made, started, its
/// status waited for, its streams in its log as frames of each, listed, and removed.
#[test]
fn a_container_is_made_started_waited_for_logged_and_removed() {
    let Some((home, image)) = home("eng-life") else {
        return;
    };
    let mut api = Api::new(&home);
    let made = api.call(
        "POST",
        "/v1.56/containers/create?name=life",
        Some(json!({"Image": image, "Cmd": ["streams", "to-out", "to-err"], "Labels": {"k": "v"}})),
    );
    assert_eq!(made.status, 201, "{made:?}: {}", made.text());
    let id = made.json()["Id"].as_str().unwrap().to_string();
    assert_eq!(made.json()["Warnings"], json!([]), "{}", made.text());
    let inspected = api.call("GET", "/containers/life/json", None);
    assert_eq!(inspected.status, 200, "{}", inspected.text());
    let doc = inspected.json();
    assert_eq!(
        (
            &doc["Id"],
            &doc["Name"],
            &doc["State"]["Status"],
            &doc["Config"]["Labels"]["k"]
        ),
        (&json!(id), &json!("/life"), &json!("created"), &json!("v")),
        "{doc}"
    );
    let taken = api.call(
        "POST",
        "/containers/create?name=life",
        Some(json!({"Image": image})),
    );
    assert_eq!(taken.status, 409, "{}", taken.text());
    // An image the store lacks: not found, which its client pulls on.
    let (unpulled, _) = served();
    let missing = api.call("POST", "/containers/create", Some(json!({"Image": unpulled})));
    assert_eq!(missing.status, 404, "{}", missing.text());
    assert_eq!(
        missing.json()["message"],
        json!(format!("No such image: {unpulled}"))
    );
    let started = api.call("POST", &format!("/containers/{id}/start"), None);
    assert_eq!(started.status, 204, "{}", started.text());
    let waited = api.call("POST", "/containers/life/wait", None);
    assert_eq!(
        (waited.status, waited.json()),
        (200, json!({"StatusCode": 0})),
        "{waited:?}"
    );
    let logs = api.call("GET", "/containers/life/logs?stdout=1&stderr=1", None);
    assert_eq!(logs.status, 200, "{}", logs.text());
    assert_eq!(
        logs.field("content-type"),
        Some("application/vnd.docker.multiplexed-stream")
    );
    assert_eq!(demux(&logs.body), ("to-out\n".into(), "to-err\n".into()));
    let only_err = api.call("GET", "/containers/life/logs?stderr=1", None);
    assert_eq!(demux(&only_err.body), (String::new(), "to-err\n".into()));
    let listed = api.call("GET", "/containers/json?all=1", None);
    assert_eq!(listed.status, 200, "{}", listed.text());
    let list = listed.json();
    let ours = list
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["Id"] == json!(id))
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(
        (&ours["Names"], &ours["State"], &ours["Labels"]["k"]),
        (&json!(["/life"]), &json!("exited"), &json!("v")),
        "{ours}"
    );
    assert!(
        ours["Status"].as_str().unwrap().starts_with("Exited (0)"),
        "{ours}"
    );
    let running = api.call("GET", "/containers/json", None);
    assert!(!running.text().contains(&id), "{}", running.text());
    let removed = api.call("DELETE", &format!("/containers/{id}"), None);
    assert_eq!(removed.status, 204, "{}", removed.text());
    let gone = api.call("GET", &format!("/containers/{id}/json"), None);
    assert_eq!(gone.status, 404, "{}", gone.text());
}

/// `docker run`'s calls: a create, an attach to it before it starts, a wait for its next
/// end (or, with `--rm`, its removal), then its start: the attach has its output from its
/// first byte, each stream's frames, and the wait its status; one removed as it ends is
/// gone once the wait is answered.
#[test]
fn an_attach_before_a_start_has_its_output_from_the_first_byte() {
    let Some((home, image)) = home("eng-attach") else {
        return;
    };
    let mut api = Api::new(&home);
    let made = api.call(
        "POST",
        "/containers/create",
        Some(json!({
            "Image": image,
            "Cmd": ["streams", "first-out", "first-err"],
            "AttachStdout": true,
            "AttachStderr": true,
        })),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    let id = made.json()["Id"].as_str().unwrap().to_string();
    // Each is in place once its head has come, as `docker run` takes it to be: it starts
    // the container then.
    let mut attacher = Api::new(&home);
    let (status, kind) = attacher.upgrade(
        &format!("/v1.56/containers/{id}/attach?stream=1&stdout=1&stderr=1"),
        None,
    );
    assert_eq!(
        (status, kind.as_str()),
        (101, "application/vnd.docker.multiplexed-stream")
    );
    let mut waiter = Api::new(&home);
    waiter.send(
        "POST",
        &format!("/containers/{id}/wait?condition=next-exit"),
        None,
        &[],
    );
    assert_eq!(waiter.head().0, 200);
    let started = api.call("POST", &format!("/containers/{id}/start"), None);
    assert_eq!(started.status, 204, "{}", started.text());
    assert_eq!(
        demux(&attacher.rest()),
        ("first-out\n".into(), "first-err\n".into())
    );
    let mut chunk = String::new();
    waiter.reader.read_line(&mut chunk).unwrap();
    let mut code = String::new();
    waiter.reader.read_line(&mut code).unwrap();
    assert_eq!(code.trim(), r#"{"StatusCode":0}"#);
    // `--rm`: its status once it is gone.
    let made = api.call(
        "POST",
        "/containers/create",
        Some(json!({"Image": image, "Cmd": ["exit", "3"], "HostConfig": {"AutoRemove": true}})),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    let id = made.json()["Id"].as_str().unwrap().to_string();
    let mut waiter = Api::new(&home);
    waiter.send(
        "POST",
        &format!("/containers/{id}/wait?condition=removed"),
        None,
        &[],
    );
    assert_eq!(waiter.head().0, 200);
    let started = api.call("POST", &format!("/containers/{id}/start"), None);
    assert_eq!(started.status, 204, "{}", started.text());
    let mut chunk = String::new();
    waiter.reader.read_line(&mut chunk).unwrap();
    let mut code = String::new();
    waiter.reader.read_line(&mut code).unwrap();
    assert_eq!(code.trim(), r#"{"StatusCode":3}"#);
    let gone = api.call("GET", &format!("/containers/{id}/json"), None);
    assert_eq!(gone.status, 404, "{}", gone.text());
}

/// A terminal's output comes raw, CRLF and all; a client's input reaches the command's
/// stdin through an attach before the start, and ends it as the client closes it; a
/// client that types its terminal's detach keys leaves, its container running; a
/// terminal a client started attached is resized while its run lives, and one started
/// detached, which no client holds, is not.
#[test]
fn attaches_carry_terminals_stdin_and_detaches() {
    let Some((home, image)) = home("eng-tty") else {
        return;
    };
    let mut api = Api::new(&home);
    // Its attach is in place once its head has come: then it is started.
    let start = |api: &mut Api, id: &str| {
        let started = api.call("POST", &format!("/containers/{id}/start"), None);
        assert_eq!(started.status, 204, "{}", started.text());
    };
    // A terminal's: raw, its two streams one.
    let made = api.call(
        "POST",
        "/containers/create",
        Some(json!({"Image": image, "Tty": true, "Cmd": ["streams", "tty-out", "tty-err"], "AttachStdout": true})),
    );
    let id = made.json()["Id"].as_str().unwrap().to_string();
    let mut attacher = Api::new(&home);
    let (status, kind) = attacher.upgrade(
        &format!("/containers/{id}/attach?stream=1&stdout=1&stderr=1"),
        None,
    );
    assert_eq!(
        (status, kind.as_str()),
        (101, "application/vnd.docker.raw-stream")
    );
    start(&mut api, &id);
    assert_eq!(
        String::from_utf8_lossy(&attacher.rest()),
        "tty-out\r\ntty-err\r\n"
    );
    // Its stdin, from the client, until it closes it.
    let made = api.call(
        "POST",
        "/containers/create",
        Some(json!({"Image": image, "OpenStdin": true, "StdinOnce": true, "AttachStdin": true, "Cmd": ["cat"]})),
    );
    let id = made.json()["Id"].as_str().unwrap().to_string();
    let mut client = Api::new(&home);
    client.send(
        "POST",
        &format!("/containers/{id}/attach?stream=1&stdin=1&stdout=1"),
        None,
        &[("Connection", "Upgrade"), ("Upgrade", "tcp")],
    );
    assert_eq!(client.head().0, 101);
    client.conn.write_all(b"typed in\n").unwrap();
    client.conn.shutdown(std::net::Shutdown::Write).unwrap();
    start(&mut api, &id);
    let mut echoed = Vec::new();
    client.reader.read_to_end(&mut echoed).unwrap();
    assert_eq!(demux(&echoed), ("typed in\n".into(), String::new()));
    let waited = api.call("POST", &format!("/containers/{id}/wait"), None);
    assert_eq!(waited.json(), json!({"StatusCode": 0}));
    // Detached by its terminal's keys: the container runs on, and is attached again.
    let made = api.call(
        "POST",
        "/containers/create?name=kept",
        Some(json!({"Image": image, "Tty": true, "OpenStdin": true, "AttachStdin": true, "Cmd": ["cat"]})),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    let mut client = Api::new(&home);
    client.send(
        "POST",
        "/containers/kept/attach?stream=1&stdin=1&stdout=1&detachKeys=ctrl-a,a",
        None,
        &[("Connection", "Upgrade"), ("Upgrade", "tcp")],
    );
    assert_eq!(client.head().0, 101);
    start(&mut api, "kept");
    let resized = api.call("POST", "/containers/kept/resize?h=40&w=100", None);
    assert_eq!(resized.status, 200, "{}", resized.text());
    client.conn.write_all(b"hi\n").unwrap();
    let mut line = String::new();
    client.reader.read_line(&mut line).unwrap();
    assert_eq!(line, "hi\r\n");
    client.conn.write_all(&[1, b'a']).unwrap();
    let mut rest = Vec::new();
    client.reader.read_to_end(&mut rest).unwrap();
    let inspected = api.call("GET", "/containers/kept/json", None);
    assert_eq!(
        inspected.json()["State"]["Running"],
        json!(true),
        "{}",
        inspected.text()
    );
    let resized = api.call("POST", "/containers/kept/resize?h=41&w=101", None);
    assert_eq!(resized.status, 200, "{}", resized.text());
    let removed = api.call("DELETE", "/containers/kept?force=1", None);
    assert_eq!(removed.status, 204, "{}", removed.text());
    let made = api.call(
        "POST",
        "/containers/create?name=alone",
        Some(json!({"Image": image, "Tty": true, "Cmd": ["sleep"]})),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    let started = api.call("POST", "/containers/alone/start", None);
    assert_eq!(started.status, 204, "{}", started.text());
    let unresized = api.call("POST", "/containers/alone/resize?h=40&w=100", None);
    assert_eq!(unresized.status, 409, "{}", unresized.text());
    let removed = api.call("DELETE", "/containers/alone?force=1", None);
    assert_eq!(removed.status, 204, "{}", removed.text());
}

/// A list takes its filters as the API gives them, and `info` says what shards holds and
/// the platform its containers are.
#[test]
fn lists_filter_and_info_counts() {
    let Some((home, image)) = home("eng-list") else {
        return;
    };
    let mut api = Api::new(&home);
    for (name, label) in [("la", "one"), ("lb", "two")] {
        let made = api.call(
            "POST",
            &format!("/containers/create?name={name}"),
            Some(json!({"Image": image, "Labels": {"set": label}})),
        );
        assert_eq!(made.status, 201, "{}", made.text());
    }
    let names = |answered: Answered| -> Vec<String> {
        answered
            .json()
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["Names"][0].as_str().unwrap().to_string())
            .collect()
    };
    let filtered = api.call(
        "GET",
        "/containers/json?all=1&filters=%7B%22label%22%3A%7B%22set%3Dtwo%22%3Atrue%7D%7D",
        None,
    );
    assert_eq!(names(filtered), ["/lb"]);
    let listed = api.call(
        "GET",
        "/containers/json?all=true&filters=%7B%22name%22%3A%5B%22la%22%5D%7D",
        None,
    );
    assert_eq!(names(listed), ["/la"]);
    let bad = api.call(
        "GET",
        "/containers/json?filters=%7B%22nonsense%22%3A%5B%22x%22%5D%7D",
        None,
    );
    assert_eq!(bad.status, 400, "{}", bad.text());
    let info = api.call("GET", "/info", None);
    assert_eq!(info.status, 200, "{}", info.text());
    let info = info.json();
    assert_eq!(
        (&info["OSType"], &info["Containers"], &info["ContainersStopped"]),
        (&json!("linux"), &json!(2), &json!(2)),
        "{info}"
    );
    assert!(info.get("ClientInfo").is_none(), "{info}");
}

/// A command run in a running container by exec: made, started on the connection taken
/// over, its streams as frames, its status kept for its client to read; the container
/// then killed and removed.
#[test]
fn a_command_runs_in_a_running_container_by_exec() {
    let Some((home, image)) = home("eng-exec") else {
        return;
    };
    let mut api = Api::new(&home);
    let made = api.call(
        "POST",
        "/containers/create?name=held",
        Some(json!({"Image": image, "Cmd": ["sleep"]})),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    let refused = api.call(
        "POST",
        "/containers/held/exec",
        Some(json!({"Cmd": ["exit", "1"]})),
    );
    assert_eq!(refused.status, 409, "{}", refused.text());
    let started = api.call("POST", "/containers/held/start", None);
    assert_eq!(started.status, 204, "{}", started.text());
    let again = api.call("POST", "/containers/held/start", None);
    assert_eq!(again.status, 304, "{}", again.text());
    for (cmd, out, err, code) in [
        (
            json!(["/bin/testguest", "streams", "exec-out", "exec-err"]),
            "exec-out\n",
            "exec-err\n",
            0,
        ),
        (json!(["/bin/testguest", "exit", "5"]), "", "", 5),
    ] {
        let exec = api.call(
            "POST",
            "/containers/held/exec",
            Some(json!({"Cmd": cmd, "AttachStdout": true, "AttachStderr": true})),
        );
        assert_eq!(exec.status, 201, "{}", exec.text());
        let exec_id = exec.json()["Id"].as_str().unwrap().to_string();
        let (status, _, streamed) = Api::new(&home).upgraded(
            &format!("/exec/{exec_id}/start"),
            Some(json!({"Detach": false, "Tty": false})),
        );
        assert_eq!(status, 101);
        assert_eq!(demux(&streamed), (out.to_string(), err.to_string()));
        let inspected = api.call("GET", &format!("/exec/{exec_id}/json"), None);
        let doc = inspected.json();
        assert_eq!(
            (&doc["Running"], &doc["ExitCode"]),
            (&json!(false), &json!(code)),
            "{doc}"
        );
    }
    // Detached: answered once it starts, its end kept as it comes.
    let exec = api.call(
        "POST",
        "/containers/held/exec",
        Some(json!({"Cmd": ["/bin/testguest", "exit", "6"]})),
    );
    let exec_id = exec.json()["Id"].as_str().unwrap().to_string();
    let started = api.call(
        "POST",
        &format!("/exec/{exec_id}/start"),
        Some(json!({"Detach": true})),
    );
    assert_eq!(started.status, 200, "{}", started.text());
    let mut ended = json!(null);
    for _ in 0..200 {
        ended = api.call("GET", &format!("/exec/{exec_id}/json"), None).json();
        if ended["Running"] == json!(false) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        (&ended["Running"], &ended["ExitCode"]),
        (&json!(false), &json!(6)),
        "{ended}"
    );
    let missing = api.call(
        "POST",
        "/containers/held/exec",
        Some(json!({"Cmd": ["/no/such/command"]})),
    );
    let exec_id = missing.json()["Id"].as_str().unwrap().to_string();
    let refused = api.call(
        "POST",
        &format!("/exec/{exec_id}/start"),
        Some(json!({"Detach": true})),
    );
    assert_eq!(refused.status, 500, "{}", refused.text());
    assert!(refused.text().contains("/no/such/command"), "{}", refused.text());
    let unknown = api.call("GET", "/exec/nothing/json", None);
    assert_eq!(unknown.status, 404, "{}", unknown.text());
    let killed = api.call("POST", "/containers/held/kill", None);
    assert_eq!(killed.status, 204, "{}", killed.text());
    let waited = api.call("POST", "/containers/held/wait", None);
    assert_eq!(waited.json()["StatusCode"], json!(137), "{}", waited.text());
    let removed = api.call("DELETE", "/containers/held?force=1", None);
    assert_eq!(removed.status, 204, "{}", removed.text());
}

/// Images as the API lists, inspects and pulls them: the pull a stream of JSON messages,
/// its failure the last of them.
#[test]
fn images_are_listed_inspected_and_pulled() {
    let Some((home, image)) = home("eng-images") else {
        return;
    };
    let mut api = Api::new(&home);
    let listed = api.call("GET", "/images/json", None);
    assert_eq!(listed.status, 200, "{}", listed.text());
    assert!(
        listed
            .json()
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["RepoTags"] == json!([image])),
        "{}",
        listed.text()
    );
    let inspected = api.call("GET", &format!("/images/{image}/json"), None);
    assert_eq!(inspected.status, 200, "{}", inspected.text());
    assert_eq!(
        inspected.json()["RepoTags"],
        json!([image]),
        "{}",
        inspected.text()
    );
    let missing = api.call("GET", "/images/nothing:here/json", None);
    assert_eq!(missing.status, 404, "{}", missing.text());
    let (name, tag) = image.rsplit_once(':').unwrap();
    let pulled = api.call(
        "POST",
        &format!("/images/create?fromImage={name}&tag={tag}"),
        None,
    );
    assert_eq!(pulled.status, 200, "{}", pulled.text());
    let messages: Vec<Value> = pulled
        .text()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        !messages.is_empty() && messages.iter().all(|m| m.get("error").is_none()),
        "{messages:?}"
    );
    let failed = api.call("POST", "/images/create?fromImage=127.0.0.1:1/none&tag=x", None);
    let last: Value = serde_json::from_str(failed.text().lines().last().unwrap()).unwrap();
    assert!(last["error"].is_string(), "{}", failed.text());
}

/// The docker CLI runs microVMs through the engine, as it runs Docker's containers: `run`
/// attached with its streams and status, `--rm`, `-d`, `exec`, `logs`, `ps`, `inspect`
/// and `rm -f`.
#[test]
fn the_docker_cli_runs_microvms_through_the_engine() {
    let Ok(docker) = which("docker") else {
        eprintln!("SKIP: no docker CLI on this host");
        return;
    };
    let Some((home, image)) = home("eng-cli") else {
        return;
    };
    let config = home.join("docker-config");
    std::fs::create_dir_all(&config).unwrap();
    let host = format!("unix://{}", home.join("engine.sock").display());
    let docker = |args: &[&str]| {
        let out = std::process::Command::new(&docker)
            .args(args)
            .env("DOCKER_HOST", &host)
            .env("DOCKER_CONFIG", &config)
            .env_remove("DOCKER_CONTEXT")
            .env("DOCKER_CLI_HINTS", "false")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (status, out, err) = docker(&["version", "--format", "{{.Server.APIVersion}} {{.Server.Os}}"]);
    assert_eq!(status, Some(0), "{out}{err}");
    assert!(out.trim().ends_with(" linux"), "{out}");
    let (status, out, err) = docker(&["run", "--rm", &image, "streams", "cli-out", "cli-err"]);
    assert_eq!(
        (status, out.as_str(), err.as_str()),
        (Some(0), "cli-out\n", "cli-err\n")
    );
    let (status, _, err) = docker(&["run", "--rm", &image, "exit", "7"]);
    assert_eq!(status, Some(7), "{err}");
    // One the home has not: pulled through the engine as the create finds none.
    let (unpulled, _) = served();
    let (status, out, err) = docker(&["run", "--rm", &unpulled, "streams", "pulled", "then"]);
    assert_eq!((status, out.as_str()), (Some(0), "pulled\n"), "{err}");
    assert!(
        err.starts_with(&format!("Unable to find image '{unpulled}' locally\n")),
        "{err}"
    );
    assert!(err.ends_with("then\n"), "{err}");
    let (status, out, err) = docker(&["run", "-d", "--name", "cli-held", &image, "sleep"]);
    assert_eq!(status, Some(0), "{err}");
    let id = out.trim().to_string();
    assert_eq!(id.len(), 64, "{out}");
    let (status, out, err) = docker(&[
        "exec",
        "cli-held",
        "/bin/testguest",
        "streams",
        "in-out",
        "in-err",
    ]);
    assert_eq!(
        (status, out.as_str(), err.as_str()),
        (Some(0), "in-out\n", "in-err\n")
    );
    let (status, _, _) = docker(&["exec", "cli-held", "/bin/testguest", "exit", "4"]);
    assert_eq!(status, Some(4));
    let (status, out, err) = docker(&["logs", "cli-held"]);
    assert_eq!((status, out.as_str()), (Some(0), "ready\n"), "{err}");
    let (status, out, err) = docker(&["ps", "--format", "{{.Names}} {{.State}}"]);
    assert_eq!((status, out.trim()), (Some(0), "cli-held running"), "{err}");
    let (status, out, err) = docker(&[
        "inspect",
        "--format",
        "{{.State.Running}} {{.Config.Image}}",
        "cli-held",
    ]);
    assert_eq!(
        (status, out.trim()),
        (Some(0), format!("true {image}").as_str()),
        "{err}"
    );
    let (status, _, err) = docker(&["rm", "-f", "cli-held"]);
    assert_eq!(status, Some(0), "{err}");
    let (_, out, _) = docker(&["ps", "-aq"]);
    assert_eq!(out.trim(), "");
}

/// `name` on PATH.
fn which(name: &str) -> Result<std::path::PathBuf, ()> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .ok_or(())
}

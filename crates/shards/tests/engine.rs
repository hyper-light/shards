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

use base64::Engine as _;
use common::{Run, TempDir, cannot_run_vms, guest_init, kernel, run_shards_env, served, tar};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);
/// A client's call past this is taken for hung, and fails its test with what it said
/// (`common::output_within`).
const CLIENT: Duration = Duration::from_secs(600);

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

/// A home whose daemon boots the guest it records (`shards guest use`), as a user's does,
/// so that runs save and restore their images' templates; with the test image pulled.
fn home_with_guest(name: &str) -> Option<(TempDir, String)> {
    if cannot_run_vms() {
        return None;
    }
    let (image, _) = served();
    let home = TempDir::new(name);
    let env = [("SHARDS_HOME", home.as_os_str())];
    let (kernel, init) = (kernel(), guest_init());
    let args = [
        "use".as_ref(),
        "--kernel".as_ref(),
        kernel.as_os_str(),
        "--init".as_ref(),
        init.as_os_str(),
    ];
    let recorded = run_shards_env(&["guest"], &args, &env, TIMEOUT);
    assert_eq!(recorded.status, Some(0), "{}", recorded.stderr);
    let pulled = run_shards_env(&["pull"], &["-q", image.as_str()], &env, TIMEOUT);
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

    /// A call whose body is `bytes` of `kind`, and its answer.
    fn upload(&mut self, method: &str, path: &str, kind: &str, bytes: &[u8]) -> Answered {
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: api\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n",
            bytes.len()
        );
        self.conn.write_all(head.as_bytes()).unwrap();
        self.conn.write_all(bytes).unwrap();
        self.answer(method)
    }

    /// A call whose answer runs until the connection closes: its status, fields and all it
    /// sends.
    fn downloaded(mut self, path: &str) -> Answered {
        self.send("GET", path, None, &[]);
        let (status, fields) = self.head();
        let mut body = Vec::new();
        self.reader.read_to_end(&mut body).unwrap();
        Answered { status, fields, body }
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

/// A tar archive's entries: each name and its contents (none for a directory).
fn untar(mut bytes: &[u8]) -> Vec<(String, Option<Vec<u8>>)> {
    let mut entries = Vec::new();
    while bytes.len() >= 512 && bytes[..512].iter().any(|&b| b != 0) {
        let header = &bytes[..512];
        let field = |r: std::ops::Range<usize>| {
            let raw = &header[r];
            String::from_utf8_lossy(&raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())])
                .into_owned()
        };
        let prefix = field(345..500);
        let name = match (prefix.is_empty(), field(0..100)) {
            (true, name) => name,
            (false, name) => format!("{prefix}/{name}"),
        };
        let size = usize::from_str_radix(field(124..136).trim(), 8).unwrap_or(0);
        let data = &bytes[512..512 + size];
        let kind = header[156];
        entries.push((name, (kind == b'0' || kind == 0).then(|| data.to_vec())));
        bytes = &bytes[512 + size.div_ceil(512) * 512..];
    }
    entries
}

/// Files go into a running microVM and out of a stopped one by `shards copy vm`, each
/// path's stat said, packed and unpacked by its own init; what it changed, what runs in
/// it and all its files are read as `shards diff vm`, `top vm` and `export vm` read them;
/// a path not there, and a microVM whose files take no writes, are refused with why.
#[test]
fn files_go_in_and_out_of_a_microvm_by_copy() {
    let Some((home, image)) = home("eng-files") else {
        return;
    };
    let mut api = Api::new(&home);
    for (name, extra) in [("files", json!({})), ("sealed", json!({"ReadonlyRootfs": true}))] {
        let made = api.call(
            "POST",
            &format!("/containers/create?name={name}"),
            Some(json!({"Image": image, "Cmd": ["sleep"], "HostConfig": extra})),
        );
        assert_eq!(made.status, 201, "{}", made.text());
        let started = api.call("POST", &format!("/containers/{name}/start"), None);
        assert_eq!(started.status, 204, "{}", started.text());
    }
    // In: an archive unpacked into /tmp.
    let archive = tar(&[
        ("into", 0o755, 0, None),
        ("into/note.txt", 0o644, 0, Some(b"from the host\n")),
    ]);
    let put = api.upload(
        "PUT",
        "/containers/files/archive?path=/tmp",
        "application/x-tar",
        &archive,
    );
    assert_eq!(put.status, 200, "{}", put.text());
    // Its stat, as the guest has it.
    let stat = api.call("HEAD", "/containers/files/archive?path=/tmp/into/note.txt", None);
    assert_eq!(stat.status, 200, "{stat:?}");
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(stat.field("x-docker-container-path-stat").unwrap())
        .unwrap();
    let stat: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(
        (&stat["name"], &stat["size"]),
        (&json!("note.txt"), &json!(14)),
        "{stat}"
    );
    // Out: the directory as an archive, packed by the guest.
    let out = Api::new(&home).downloaded("/containers/files/archive?path=/tmp/into");
    assert_eq!(
        (out.status, out.field("content-type")),
        (200, Some("application/x-tar")),
        "{out:?}"
    );
    let entries = untar(&out.body);
    assert!(
        entries.contains(&("into/note.txt".to_string(), Some(b"from the host\n".to_vec()))),
        "{entries:?}"
    );
    // Refused with why: a path not there, a microVM whose files take no writes.
    let missing = api.call("HEAD", "/containers/files/archive?path=/no/such", None);
    assert_eq!(missing.status, 404, "{missing:?}");
    let sealed = api.upload(
        "PUT",
        "/containers/sealed/archive?path=/tmp",
        "application/x-tar",
        &archive,
    );
    assert_eq!(sealed.status, 403, "{}", sealed.text());
    assert!(sealed.text().contains("read-only"), "{}", sealed.text());
    // What it changed, and what runs in it.
    let changes = api.call("GET", "/containers/files/changes", None);
    assert_eq!(changes.status, 200, "{}", changes.text());
    let changes = changes.json();
    assert!(
        changes
            .as_array()
            .unwrap()
            .contains(&json!({"Path": "/tmp/into/note.txt", "Kind": 1})),
        "{changes}"
    );
    let top = api.call("GET", "/containers/files/top", None);
    assert_eq!(top.status, 200, "{}", top.text());
    let top = top.json();
    let titles = top["Titles"].as_array().unwrap();
    let cmd = titles
        .iter()
        .position(|t| t == "CMD")
        .unwrap_or_else(|| panic!("{top}"));
    assert!(
        top["Processes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p[cmd].as_str().unwrap().contains("testguest sleep")),
        "{top}"
    );
    // All its files.
    let exported = Api::new(&home).downloaded("/containers/files/export");
    assert_eq!(exported.status, 200);
    let names: Vec<String> = untar(&exported.body).into_iter().map(|(n, _)| n).collect();
    assert!(
        names
            .iter()
            .any(|n| n.trim_start_matches("./") == "bin/testguest"),
        "{names:?}"
    );
    assert!(
        names
            .iter()
            .any(|n| n.trim_start_matches("./") == "tmp/into/note.txt"),
        "{names:?}"
    );
    // A stopped one's files, read as they were left.
    let killed = api.call("POST", "/containers/files/kill", None);
    assert_eq!(killed.status, 204, "{}", killed.text());
    api.call("POST", "/containers/files/wait", None);
    let out = Api::new(&home).downloaded("/containers/files/archive?path=/tmp/into/note.txt");
    assert_eq!(
        untar(&out.body),
        [("note.txt".to_string(), Some(b"from the host\n".to_vec()))]
    );
    for name in ["files", "sealed"] {
        let removed = api.call("DELETE", &format!("/containers/{name}?force=1"), None);
        assert_eq!(removed.status, 204, "{}", removed.text());
    }
}

/// What a microVM costs, sampled as `shards stats vm` samples it: a client's own
/// percentage of two samples sane, and beside the guest's view what the microVM costs the
/// host. Frozen and thawed, started again, named again, its limits changed (what cannot
/// be, refused by name), each said as an event; stopped ones pruned, the bytes counted.
#[test]
fn microvms_are_watched_and_managed() {
    let Some((home, image)) = home("eng-manage") else {
        return;
    };
    let mut api = Api::new(&home);
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let made = api.call(
        "POST",
        "/containers/create?name=live",
        Some(json!({"Image": image, "Cmd": ["sleep"]})),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    let id = made.json()["Id"].as_str().unwrap().to_string();
    assert_eq!(api.call("POST", "/containers/live/start", None).status, 204);
    // Two samples a second apart, as a client asks for one.
    let stats = api.call("GET", "/containers/live/stats?stream=false", None);
    assert_eq!(stats.status, 200, "{}", stats.text());
    let s = stats.json();
    let online = s["cpu_stats"]["online_cpus"].as_u64().unwrap();
    assert!(online > 0, "{s}");
    let delta = |k: &str| {
        s["cpu_stats"][k].as_u64().unwrap_or_default() as f64
            - s["precpu_stats"][k].as_u64().unwrap_or_default() as f64
    };
    let used = |k: &str| s[k]["cpu_usage"]["total_usage"].as_u64().unwrap() as f64;
    let percent =
        (used("cpu_stats") - used("precpu_stats")) / delta("system_cpu_usage") * online as f64 * 100.0;
    assert!(
        (0.0..=100.0 * online as f64).contains(&percent),
        "{percent} of {s}"
    );
    assert!(s["memory_stats"]["usage"].as_u64().unwrap() > 0, "{s}");
    assert!(s["microvm"]["footprint"].as_u64().unwrap() > 0, "{s}");
    let one = api
        .call("GET", "/containers/live/stats?stream=false&one-shot=true", None)
        .json();
    assert!(one["precpu_stats"].get("system_cpu_usage").is_none(), "{one}");
    // Frozen whole, and thawed.
    assert_eq!(api.call("POST", "/containers/live/pause", None).status, 204);
    let paused = api.call("GET", "/containers/live/json", None).json();
    assert_eq!(paused["State"]["Paused"], json!(true), "{paused}");
    assert_eq!(api.call("POST", "/containers/live/pause", None).status, 409);
    // Not started, nor answered as started already, as dockerd answers it.
    let start = api.call("POST", "/containers/live/start", None);
    assert_eq!(
        (start.status, start.json()["message"].clone()),
        (409, json!("cannot start a paused container, try unpause instead"))
    );
    assert_eq!(api.call("POST", "/containers/live/unpause", None).status, 204);
    assert_eq!(api.call("POST", "/containers/live/unpause", None).status, 409);
    // Started again, and named again.
    let started_at = |api: &mut Api, name: &str| {
        api.call("GET", &format!("/containers/{name}/json"), None).json()["State"]["StartedAt"].clone()
    };
    let first = started_at(&mut api, "live");
    let restarted = api.call("POST", "/containers/live/restart?t=1", None);
    assert_eq!(restarted.status, 204, "{}", restarted.text());
    let after = api.call("GET", "/containers/live/json", None).json();
    assert_eq!(after["State"]["Running"], json!(true), "{after}");
    assert_ne!(after["State"]["StartedAt"], first, "{after}");
    let renamed = api.call("POST", "/containers/live/rename?name=renamed", None);
    assert_eq!(renamed.status, 204, "{}", renamed.text());
    assert_eq!(api.call("GET", "/containers/renamed/json", None).status, 200);
    // Its limits, changed in its guest at once; what cannot be, refused by name.
    let updated = api.call(
        "POST",
        "/containers/renamed/update",
        Some(json!({"CpuShares": 512})),
    );
    assert_eq!(updated.status, 200, "{}", updated.text());
    let host = api.call("GET", "/containers/renamed/json", None).json();
    assert_eq!(host["HostConfig"]["CpuShares"], json!(512), "{host}");
    let unheld = api.call(
        "POST",
        "/containers/renamed/update",
        Some(json!({"Devices": [{"PathOnHost": "/dev/fuse"}]})),
    );
    assert_eq!(unheld.status, 400, "{}", unheld.text());
    assert!(unheld.text().contains("Devices"), "{}", unheld.text());
    // Each said as an event, in the order it came.
    let until = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 1;
    let events = api.call("GET", &format!("/events?since={since}&until={until}"), None);
    assert_eq!(events.status, 200, "{}", events.text());
    let actions: Vec<String> = events
        .text()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["Actor"]["ID"] == json!(id))
        .map(|e| e["Action"].as_str().unwrap().to_string())
        .collect();
    for action in [
        "create", "start", "pause", "unpause", "restart", "rename", "update",
    ] {
        assert!(actions.iter().any(|a| a == action), "{action} in {actions:?}");
    }
    let refused = api.call("GET", "/events?since=later", None);
    assert_eq!(refused.status, 400, "{}", refused.text());
    // Stopped, then pruned, its bytes counted.
    assert_eq!(api.call("POST", "/containers/renamed/kill", None).status, 204);
    api.call("POST", "/containers/renamed/wait", None);
    let pruned = api.call("POST", "/containers/prune", None);
    assert_eq!(pruned.status, 200, "{}", pruned.text());
    let pruned = pruned.json();
    assert_eq!(pruned["ContainersDeleted"], json!([id]), "{pruned}");
    assert!(pruned["SpaceReclaimed"].as_u64().is_some(), "{pruned}");
    // The prune said as an event too, with what it reclaimed, as dockerd says it.
    let until = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 1;
    let events = api.call(
        "GET",
        &format!("/events?since={since}&until={until}&filters=%7B%22event%22%3A%5B%22prune%22%5D%7D"),
        None,
    );
    let prunes: Vec<Value> = events
        .text()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(prunes.len(), 1, "{}", events.text());
    assert_eq!(
        (&prunes[0]["Type"], &prunes[0]["Actor"]["Attributes"]["reclaimed"]),
        (&json!("container"), &json!(pruned["SpaceReclaimed"].to_string())),
        "{}",
        events.text()
    );
}

/// A volume's data outlives the microVM that wrote it: made, listed, inspected; files
/// copied into it through a microVM that mounts it, a running one whose root takes no
/// writes, and a stopped one, whose copy reaches the volume as its command will see it,
/// not its root; read by another after; its removal refused while mounted, done after;
/// and an unused one pruned, its bytes counted.
#[test]
fn a_volumes_data_outlives_the_microvm_that_wrote_it() {
    let Some((home, image)) = home("eng-volume") else {
        return;
    };
    let mut api = Api::new(&home);
    let made = api.call(
        "POST",
        "/volumes/create",
        Some(json!({"Name": "work", "Labels": {"k": "v"}})),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    let doc = made.json();
    assert_eq!(
        (&doc["Name"], &doc["Driver"], &doc["Labels"]["k"]),
        (&json!("work"), &json!("local"), &json!("v")),
        "{doc}"
    );
    let listed = api.call("GET", "/volumes", None).json();
    assert!(
        listed["Volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["Name"] == "work"),
        "{listed}"
    );
    assert_eq!(api.call("GET", "/volumes/work", None).status, 200);
    assert_eq!(api.call("GET", "/volumes/nothing", None).status, 404);
    let mounting = |api: &mut Api, name: &str, host: Value| {
        let made = api.call(
            "POST",
            &format!("/containers/create?name={name}"),
            Some(json!({"Image": image, "Cmd": ["sleep"], "HostConfig": host})),
        );
        assert_eq!(made.status, 201, "{}", made.text());
    };
    let put = |api: &mut Api, name: &str, path: &str, file: &str| {
        let archive = tar(&[(file, 0o644, 0, Some(b"kept\n"))]);
        api.upload(
            "PUT",
            &format!("/containers/{name}/archive?path={path}"),
            "application/x-tar",
            &archive,
        )
    };
    // A running microVM whose root takes no writes: its writable volume takes files.
    mounting(
        &mut api,
        "writer",
        json!({"Binds": ["work:/data"], "ReadonlyRootfs": true}),
    );
    assert_eq!(api.call("POST", "/containers/writer/start", None).status, 204);
    let into_volume = put(&mut api, "writer", "/data", "running.txt");
    assert_eq!(into_volume.status, 200, "{}", into_volume.text());
    let into_root = put(&mut api, "writer", "/", "root.txt");
    assert_eq!(into_root.status, 403, "{}", into_root.text());
    assert!(
        into_root.text().contains("container rootfs is marked read-only"),
        "{}",
        into_root.text()
    );
    assert_eq!(api.call("DELETE", "/containers/writer?force=1", None).status, 204);
    // A stopped one: what is copied in is in the volume, read out of it, and not in its
    // root's changes.
    mounting(&mut api, "reader", json!({"Binds": ["work:/data"]}));
    let stopped = put(&mut api, "reader", "/data", "stopped.txt");
    assert_eq!(stopped.status, 200, "{}", stopped.text());
    for file in ["running.txt", "stopped.txt"] {
        let read = Api::new(&home).downloaded(&format!("/containers/reader/archive?path=/data/{file}"));
        assert_eq!(read.status, 200, "{file}: {read:?}");
        assert_eq!(untar(&read.body), [(file.to_string(), Some(b"kept\n".to_vec()))]);
    }
    let changes = api.call("GET", "/containers/reader/changes", None).text();
    assert!(!changes.contains(".txt"), "{changes}");
    // Started, its command finds both where it mounts the volume.
    assert_eq!(api.call("POST", "/containers/reader/start", None).status, 204);
    let found = api.call(
        "POST",
        "/containers/reader/exec",
        Some(json!({"Cmd": ["/bin/testguest", "stat", "/data/running.txt", "/data/stopped.txt"], "AttachStdout": true})),
    );
    assert_eq!(found.status, 201, "{}", found.text());
    let exec = found.json()["Id"].as_str().unwrap().to_string();
    let (status, _, streamed) = Api::new(&home).upgraded(
        &format!("/exec/{exec}/start"),
        Some(json!({"Detach": false, "Tty": false})),
    );
    assert_eq!(status, 101);
    let (out, _) = demux(&streamed);
    assert_eq!(out.matches("= kept\\n").count(), 2, "{out}");
    let in_use = api.call("DELETE", "/volumes/work", None);
    assert_eq!(in_use.status, 409, "{}", in_use.text());
    assert_eq!(api.call("DELETE", "/containers/reader?force=1", None).status, 204);
    assert_eq!(api.call("DELETE", "/volumes/work", None).status, 204);
    // Unused, pruned with `all`.
    assert_eq!(
        api.call("POST", "/volumes/create", Some(json!({"Name": "spare"})))
            .status,
        201
    );
    let pruned = api.call(
        "POST",
        "/volumes/prune?filters=%7B%22all%22%3A%5B%22true%22%5D%7D",
        None,
    );
    assert_eq!(pruned.status, 200, "{}", pruned.text());
    let pruned = pruned.json();
    assert_eq!(pruned["VolumesDeleted"], json!(["spare"]), "{pruned}");
    assert!(pruned["SpaceReclaimed"].as_u64().is_some(), "{pruned}");
}

/// The output of microVM `name`'s command so far, both streams, as its log has it.
fn output_of(api: &mut Api, name: &str) -> String {
    let logs = api.call("GET", &format!("/containers/{name}/logs?stdout=1&stderr=1"), None);
    let (out, err) = demux(&logs.body);
    out + &err
}

/// MicroVMs on one network reach each other by name and nothing else: made through the
/// API, its members told how it is closed; a microVM off it reaches none of them; one
/// not yet started joins and leaves it, a running one keeps the network it started on;
/// a network with members stays, one without goes, and an unused one is pruned.
#[test]
fn microvms_on_a_network_reach_each_other_and_nothing_else() {
    let Some((home, image)) = home("eng-net") else {
        return;
    };
    let mut api = Api::new(&home);
    let made = api.call("POST", "/networks/create", Some(json!({"Name": "team"})));
    assert_eq!(made.status, 201, "{}", made.text());
    assert!(
        made.json()["Warning"]
            .as_str()
            .unwrap()
            .contains("closed to all but its members"),
        "{}",
        made.text()
    );
    let internal = api.call(
        "POST",
        "/networks/create",
        Some(json!({"Name": "inner", "Internal": true})),
    );
    assert_eq!(internal.json()["Warning"], json!(""), "{}", internal.text());
    let listed = api.call("GET", "/networks", None).json();
    let team = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["Name"] == "team")
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(team.get("Containers").is_none(), "{team}");
    let inspected = api.call("GET", "/networks/team", None);
    assert_eq!(inspected.status, 200, "{}", inspected.text());
    assert_eq!(api.call("GET", "/networks/nowhere", None).status, 404);
    let on = |api: &mut Api, name: &str, network: &str, cmd: Value| {
        let made = api.call(
            "POST",
            &format!("/containers/create?name={name}"),
            Some(json!({"Image": image, "Cmd": cmd, "HostConfig": {"NetworkMode": network}})),
        );
        assert_eq!(made.status, 201, "{}", made.text());
        assert_eq!(
            api.call("POST", &format!("/containers/{name}/start"), None)
                .status,
            204
        );
    };
    // Two connections: the asker's, and none of the outsider's, so it runs on.
    on(&mut api, "srv", "team", json!(["serve", "7000", "2"]));
    let ready = (0..400).any(|_| {
        output_of(&mut api, "srv").contains("ready") || {
            std::thread::sleep(Duration::from_millis(25));
            false
        }
    });
    assert!(ready, "{}", output_of(&mut api, "srv"));
    // A member, by name; one off it, not at all.
    on(&mut api, "asker", "team", json!(["ask", "srv:7000"]));
    assert_eq!(
        api.call("POST", "/containers/asker/wait", None).json()["StatusCode"],
        json!(0)
    );
    assert!(
        output_of(&mut api, "asker").starts_with("ask from "),
        "{}",
        output_of(&mut api, "asker")
    );
    on(&mut api, "outsider", "bridge", json!(["ask", "srv:7000"]));
    assert_eq!(
        api.call("POST", "/containers/outsider/wait", None).json()["StatusCode"],
        json!(1)
    );
    assert!(
        output_of(&mut api, "outsider").starts_with("ask error"),
        "{}",
        output_of(&mut api, "outsider")
    );
    // One not yet started joins it, and leaves it; a running one keeps the one it has.
    let late = api.call(
        "POST",
        "/containers/create?name=late",
        Some(json!({"Image": image, "Cmd": ["sleep"]})),
    );
    assert_eq!(late.status, 201, "{}", late.text());
    let joined = api.call(
        "POST",
        "/networks/team/connect",
        Some(json!({"Container": "late", "EndpointConfig": {"Aliases": ["tardy"]}})),
    );
    assert_eq!(joined.status, 200, "{}", joined.text());
    let doc = api.call("GET", "/containers/late/json", None).json();
    assert!(doc["NetworkSettings"]["Networks"].get("team").is_some(), "{doc}");
    let left = api.call(
        "POST",
        "/networks/team/disconnect",
        Some(json!({"Container": "late"})),
    );
    assert_eq!(left.status, 200, "{}", left.text());
    let running = api.call(
        "POST",
        "/networks/inner/connect",
        Some(json!({"Container": "srv"})),
    );
    assert_eq!(running.status, 400, "{}", running.text());
    // A network with members stays; without, it goes; an unused one is pruned.
    let kept = api.call("DELETE", "/networks/team", None);
    assert!(kept.status >= 400, "{}", kept.text());
    for name in ["srv", "asker", "outsider", "late"] {
        assert_eq!(
            api.call("DELETE", &format!("/containers/{name}?force=1"), None)
                .status,
            204
        );
    }
    assert_eq!(api.call("DELETE", "/networks/team", None).status, 204);
    let pruned = api.call("POST", "/networks/prune", None);
    assert_eq!(pruned.status, 200, "{}", pruned.text());
    assert_eq!(
        pruned.json()["NetworksDeleted"],
        json!(["inner"]),
        "{}",
        pruned.text()
    );
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
        // Experimental, for its checkpoints (D176), as each docker/cli reads it.
        assert_eq!(
            (&v["Experimental"], &v["Components"][0]["Details"]["Experimental"]),
            (&json!(true), &json!("true")),
            "{v}"
        );
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
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
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
    // Its events carry its labels, as dockerd's do: what a client that follows its own
    // filters them by (Compose follows its project's).
    let until = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 1;
    let events = api.call(
        "GET",
        &format!("/events?since={since}&until={until}&filters=%7B%22label%22%3A%5B%22k%3Dv%22%5D%7D"),
        None,
    );
    assert_eq!(events.status, 200, "{}", events.text());
    let said: Vec<(String, Value)> = events
        .text()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .map(|e| {
            (
                e["Action"].as_str().unwrap().to_string(),
                e["Actor"]["Attributes"].clone(),
            )
        })
        .collect();
    let actions: Vec<&str> = said.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        ["create", "start", "die", "destroy"],
        "{}",
        events.text()
    );
    for (action, attributes) in &said {
        assert_eq!(
            (&attributes["k"], &attributes["name"]),
            (&json!("v"), &json!("life")),
            "{action}: {attributes}"
        );
    }
    assert_eq!(said[2].1["exitCode"], json!("0"));
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

/// Images as the API lists, inspects and pulls them: the pull a stream of JSON messages; a
/// pull that fails before it says anything answered by its status.
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
    // One that fails before it says anything is answered by its status, as dockerd answers
    // one whose stream it has not flushed, which clients read as an error of the call.
    let failed = api.call("POST", "/images/create?fromImage=127.0.0.1:1/none&tag=x", None);
    assert_eq!(failed.status, 500, "{}", failed.text());
    // A tag the registry has not: not found, as clients read a pull of nothing.
    let missing = api.call(
        "POST",
        &format!("/images/create?fromImage={name}&tag=nothere"),
        None,
    );
    assert_eq!(missing.status, 404, "{}", missing.text());
    assert!(
        failed.json()["message"]
            .as_str()
            .unwrap()
            .contains("failed to resolve reference"),
        "{}",
        failed.text()
    );
}

/// Images named, traced and removed as the API asks: a tag made by its repo and tag, one
/// that names no repository refused; its history newest first, the image's ID and names
/// on the newest; a name removed, the image itself refused while a microVM was made of
/// it; what shards takes on disk, each kind's usage, the fields of clients before 1.52
/// but with `verbose`, and its microVMs' templates, which go with the image they were
/// made of; and unused images pruned, their bytes counted.
#[test]
fn images_are_tagged_traced_removed_and_pruned() {
    let Some((home, image)) = home_with_guest("eng-imgs") else {
        return;
    };
    let mut api = Api::new(&home);
    let id = api.call("GET", &format!("/images/{image}/json"), None).json()["Id"]
        .as_str()
        .unwrap()
        .to_string();
    let tagged = api.call(
        "POST",
        &format!("/images/{image}/tag?repo=shp/named&tag=v1"),
        None,
    );
    assert_eq!(tagged.status, 201, "{}", tagged.text());
    let named = api.call("GET", "/images/shp/named:v1/json", None).json();
    assert_eq!(named["Id"], json!(id), "{named}");
    let unnamed = api.call("POST", &format!("/images/{image}/tag"), None);
    assert_eq!(unnamed.status, 400, "{}", unnamed.text());
    let missing = api.call("POST", "/images/nothing:here/tag?repo=x", None);
    assert_eq!(missing.status, 404, "{}", missing.text());
    // A history: the newest step the image's, with its names; the one before `<missing>`,
    // its layer's size.
    let (_, blobs) = common::test_image_with(Some(b"history"));
    let config = String::from_utf8(blobs[0].clone()).unwrap().replacen(
        '{',
        r#"{"history":[{"created":"2026-01-02T03:04:05Z","created_by":"ADD layer","comment":"first"},{"created":"2026-01-02T03:04:06Z","created_by":"ENV x=1","empty_layer":true}],"#,
        1,
    );
    let archive = tar(&[
        ("cfg.json", 0o644, 0, Some(config.as_bytes())),
        ("abc", 0o755, 0, None),
        ("abc/layer.tar", 0o644, 0, Some(blobs[1].as_slice())),
        (
            "manifest.json",
            0o644,
            0,
            Some(br#"[{"Config":"cfg.json","RepoTags":["shp/hist:1"],"Layers":["abc/layer.tar"]}]"#),
        ),
    ]);
    let at = home.join("hist.tar");
    std::fs::write(&at, archive).unwrap();
    let loaded = shards_in(&home, &["load", "-i", at.to_str().unwrap()]);
    assert_eq!(loaded.status, Some(0), "{loaded}");
    let hist = api.call("GET", "/images/shp/hist:1/json", None).json()["Id"].clone();
    let history = api.call("GET", "/images/shp/hist:1/history", None);
    assert_eq!(history.status, 200, "{}", history.text());
    let steps = history.json();
    assert_eq!(
        (
            &steps[0]["Id"],
            &steps[0]["Tags"],
            &steps[0]["CreatedBy"],
            &steps[0]["Created"],
            &steps[0]["Size"]
        ),
        (
            &hist,
            &json!(["shp/hist:1"]),
            &json!("ENV x=1"),
            &json!(1_767_323_046),
            &json!(0)
        ),
        "{steps}"
    );
    assert_eq!(
        (
            &steps[1]["Id"],
            &steps[1]["Tags"],
            &steps[1]["Comment"],
            &steps[1]["Created"]
        ),
        (
            &json!("<missing>"),
            &Value::Null,
            &json!("first"),
            &json!(1_767_323_045)
        ),
        "{steps}"
    );
    assert!(steps[1]["Size"].as_i64().unwrap() > 0, "{steps}");
    assert_eq!(steps.as_array().unwrap().len(), 2, "{steps}");
    let platform = api.call(
        "GET",
        "/images/shp/hist:1/history?platform=%7B%22os%22%3A%22linux%22%7D",
        None,
    );
    assert_eq!(platform.status, 400, "{}", platform.text());
    assert!(platform.text().contains("--platform"), "{}", platform.text());
    // A run of it, which a template is made of where this host saves them.
    let made = api.call(
        "POST",
        "/containers/create?name=user",
        Some(json!({"Image": "shp/named:v1", "Cmd": ["exit", "0"]})),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    assert_eq!(api.call("POST", "/containers/user/start", None).status, 204);
    assert_eq!(
        api.call("POST", "/containers/user/wait", None).json()["StatusCode"],
        json!(0)
    );
    let saved = std::fs::read_dir(home.join("templates"))
        .map(|d| d.flatten().filter(|e| e.path().is_dir()).count())
        .unwrap_or(0);
    // The image is refused while a microVM was made of it; a name of it goes.
    let refused = api.call("DELETE", &format!("/images/{id}"), None);
    assert_eq!(refused.status, 409, "{}", refused.text());
    let untagged = api.call("DELETE", "/images/shp/named:v1", None);
    assert_eq!(untagged.status, 200, "{}", untagged.text());
    assert_eq!(untagged.json(), json!([{"Untagged": "shp/named:v1"}]));
    // What shards takes on disk.
    let df = api.call("GET", "/system/df", None);
    assert_eq!(df.status, 200, "{}", df.text());
    let df = df.json();
    assert_eq!(df["ImageUsage"]["TotalCount"], json!(2), "{df}");
    assert_eq!(df["ContainerUsage"]["TotalCount"], json!(1), "{df}");
    assert!(
        df["Images"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["Id"] == json!(id)),
        "{df}"
    );
    assert_eq!(df["Containers"][0]["Names"], json!(["/user"]), "{df}");
    assert!(df["LayersSize"].as_i64().unwrap() > 0, "{df}");
    // Where this build has snapshots, the run saved its image's template.
    assert_eq!(saved > 0, shards_vmm::vm::SNAPSHOTS, "{saved} templates");
    match saved {
        0 => assert_eq!(df["TemplateUsage"], json!({}), "{df}"),
        n => assert_eq!(df["TemplateUsage"]["TotalCount"], json!(n), "{df}"),
    }
    let verbose = api.call("GET", "/system/df?verbose=1", None).json();
    assert!(
        verbose.get("Images").is_none() && verbose.get("LayersSize").is_none(),
        "{verbose}"
    );
    assert_eq!(
        verbose["ImageUsage"]["Items"].as_array().unwrap().len(),
        2,
        "{verbose}"
    );
    let typed = api.call("GET", "/system/df?type=volume", None).json();
    let mut kinds: Vec<&String> = typed.as_object().unwrap().keys().collect();
    kinds.sort();
    assert_eq!(kinds, ["VolumeUsage", "Volumes"], "{typed}");
    assert_eq!(api.call("GET", "/system/df?type=disks", None).status, 400);
    // Pruned: nothing dangling, then every image no microVM was made of, its bytes
    // counted; the templates made of it go after.
    let none = api.call("POST", "/images/prune", None).json();
    assert_eq!(none, json!({"ImagesDeleted": [], "SpaceReclaimed": 0}), "{none}");
    assert_eq!(api.call("DELETE", "/containers/user", None).status, 204);
    let pruned = api.call(
        "POST",
        "/images/prune?filters=%7B%22dangling%22%3A%5B%22false%22%5D%7D",
        None,
    );
    assert_eq!(pruned.status, 200, "{}", pruned.text());
    let pruned = pruned.json();
    let deleted = pruned["ImagesDeleted"].as_array().unwrap();
    assert!(deleted.contains(&json!({"Deleted": id})), "{pruned}");
    assert!(pruned["SpaceReclaimed"].as_i64().unwrap() > 0, "{pruned}");
    let gone = (0..200).any(|_| {
        api.call("GET", "/system/df", None).json()["TemplateUsage"] == json!({}) || {
            std::thread::sleep(Duration::from_millis(50));
            false
        }
    });
    assert!(gone, "{}", api.call("GET", "/system/df", None).text());
}

/// Images pushed, saved, loaded and imported as the API asks. A push goes with its
/// client's credentials alone, each layer said by its ID, its digest an `aux` message,
/// and our platform's manifest pushed alone, where the index was not all here, the `aux`
/// the docker CLI tells of it by; a tag not here said in its stream. An image saved
/// is an archive written as it is made, loaded back as it comes; one not here is 404. A
/// root filesystem is imported from the body, named, with its message and changes; a
/// source that is neither http nor the body is refused, and `scratch` is no image to pull.
#[test]
fn images_are_pushed_saved_loaded_and_imported() {
    if cannot_run_vms() {
        return;
    }
    let (index, blobs) = common::test_index();
    let (from, _) = common::registry(index.clone(), blobs.clone());
    let source = format!("127.0.0.1:{from}/test/image:v1");
    let home = TempDir::new("eng-push");
    let pulled = shards_in(&home, &["pull", "-q", &source]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("pusher:secret")
    );
    let (port, repos) = common::writable_registry_requiring(Some(basic));
    let repo = format!("127.0.0.1:{port}/team/app");
    let mut api = Api::new(&home);
    // A login checked with the registry: taken, no identity token where its registry gave
    // none; refused, 401, with the registry's word, from where it answered.
    let login = |password: &str| json!({"username": "pusher", "password": password, "serveraddress": format!("127.0.0.1:{port}")});
    let taken = api.call("POST", "/auth", Some(login("secret")));
    assert_eq!(
        (taken.status, taken.json()),
        (200, json!({"Status": "Login Succeeded"})),
        "{}",
        taken.text()
    );
    let refused = api.call("POST", "/auth", Some(login("wrong")));
    assert_eq!(
        (refused.status, refused.json()),
        (
            401,
            json!({"message": format!("http://127.0.0.1:{port}/v2/: unauthorized: the registry refused the credentials")})
        ),
        "{}",
        refused.text()
    );
    let tagged = api.call("POST", &format!("/images/{source}/tag?repo={repo}&tag=1"), None);
    assert_eq!(tagged.status, 201, "{}", tagged.text());
    let lines = |a: &Answered| -> Vec<Value> {
        a.text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    // Without its client's credentials, the registry refuses it.
    let refused = api.call("POST", &format!("/images/{repo}/push?tag=1"), None);
    assert_eq!(refused.status, 200, "{}", refused.text());
    assert!(
        lines(&refused).last().unwrap()["error"].is_string(),
        "{}",
        refused.text()
    );
    // With them, it goes.
    let auth = base64::engine::general_purpose::URL_SAFE.encode(
        json!({"username": "pusher", "password": "secret", "serveraddress": format!("127.0.0.1:{port}")})
            .to_string(),
    );
    api.send(
        "POST",
        &format!("/images/{repo}/push?tag=1"),
        None,
        &[("X-Registry-Auth", &auth)],
    );
    let pushed = api.answer("POST");
    assert_eq!(pushed.status, 200, "{}", pushed.text());
    let (manifest, layer) = (&blobs[2], &blobs[1]);
    let short = common::sha256_digest(layer);
    let short = short.trim_start_matches("sha256:").get(..12).unwrap().to_string();
    let digest = common::sha256_digest(manifest);
    assert_eq!(
        lines(&pushed),
        [
            json!({"status": format!("The push refers to repository [{repo}]")}),
            json!({"status": "Pushed", "progressDetail": {}, "id": short}),
            json!({"status": format!("1: digest: {digest} size: {}", manifest.len())}),
            json!({"progressDetail": {}, "aux": {"Tag": "1", "Digest": digest, "Size": manifest.len()}}),
            json!({"progressDetail": {}, "aux": {
                "manifestPushedInsteadOfIndex": true,
                "originalIndex": {"mediaType": "application/vnd.oci.image.index.v1+json", "digest": common::sha256_digest(&index), "size": 0},
                "selectedManifest": {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": digest, "size": 0},
            }}),
        ],
        "{}",
        pushed.text()
    );
    assert_eq!(repos.lock().unwrap().manifests["team/app"]["1"].1, *manifest);
    // Found by a search of the registry's catalog, with the client's credentials; and
    // refused without them, or for what dockerd refuses.
    api.send(
        "GET",
        &format!("/images/search?term=127.0.0.1:{port}/team&limit=5"),
        None,
        &[("X-Registry-Auth", &auth)],
    );
    let found = api.answer("GET");
    assert_eq!(
        (found.status, found.json()),
        (
            200,
            json!([{"description": "", "is_automated": false, "is_official": false, "name": repo, "star_count": 0}])
        ),
        "{}",
        found.text()
    );
    let unauthorized = api.call("GET", &format!("/images/search?term=127.0.0.1:{port}/team"), None);
    assert_eq!(unauthorized.status, 500, "{}", unauthorized.text());
    assert!(
        unauthorized.text().contains("authorization failed"),
        "{}",
        unauthorized.text()
    );
    for (query, said) in [
        ("term=x&limit=-1", "invalid limit specified"),
        ("term=x&limit=101", "limit 101 is outside the range of [1, 100]"),
        (
            "term=x&filters=%7B%22nope%22%3A%5B%221%22%5D%7D",
            "invalid filter 'nope'",
        ),
    ] {
        let refused = api.call("GET", &format!("/images/search?{query}"), None);
        assert_eq!(
            (refused.status, refused.json()),
            (400, json!({"message": said})),
            "{query}"
        );
    }
    // A tag not here: said after the repository, as dockerd says it.
    let missing = api.call("POST", &format!("/images/{repo}/push?tag=nope"), None);
    assert_eq!(missing.status, 200, "{}", missing.text());
    assert!(
        lines(&missing).last().unwrap()["error"]
            .as_str()
            .unwrap()
            .starts_with("tag does not exist"),
        "{}",
        missing.text()
    );
    // Saved as it is made, removed, and loaded back as it comes.
    let saved = Api::new(&home).downloaded(&format!("/images/{repo}:1/get"));
    assert_eq!(
        (saved.status, saved.field("content-type")),
        (200, Some("application/x-tar")),
        "{saved:?}"
    );
    let names: Vec<String> = untar(&saved.body).into_iter().map(|(n, _)| n).collect();
    assert!(
        names.iter().any(|n| n == "index.json") && names.iter().any(|n| n == "manifest.json"),
        "{names:?}"
    );
    assert_eq!(api.call("GET", "/images/nothing:here/get", None).status, 404);
    assert_eq!(api.call("DELETE", &format!("/images/{repo}:1"), None).status, 200);
    let loaded = api.upload("POST", "/images/load", "application/x-tar", &saved.body);
    assert_eq!(loaded.status, 200, "{}", loaded.text());
    assert_eq!(
        lines(&loaded),
        [json!({"stream": format!("Loaded image: {repo}:1\n")})],
        "{}",
        loaded.text()
    );
    assert_eq!(
        api.call("GET", &format!("/images/{repo}:1/json"), None).status,
        200
    );
    // A root filesystem imported from the body: named, its message and change kept.
    let imported = api.upload(
        "POST",
        "/images/create?fromSrc=-&repo=shp/imported&tag=1&message=from%20the%20api&changes=CMD%20%5B%22sleep%22%5D",
        "application/x-tar",
        layer,
    );
    assert_eq!(imported.status, 200, "{}", imported.text());
    let id = lines(&imported).last().unwrap()["status"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(id.starts_with("sha256:"), "{}", imported.text());
    let doc = api.call("GET", "/images/shp/imported:1/json", None).json();
    assert_eq!(
        (&doc["Id"], &doc["Config"]["Cmd"]),
        (&json!(id), &json!(["sleep"])),
        "{doc}"
    );
    let history = api.call("GET", "/images/shp/imported:1/history", None).json();
    assert_eq!(history[0]["Comment"], json!("from the api"), "{history}");
    let elsewhere = api.call("POST", "/images/create?fromSrc=ftp://example.com/root.tar", None);
    assert_eq!(elsewhere.status, 400, "{}", elsewhere.text());
    let scratch = api.call("POST", "/images/create?fromImage=scratch&tag=latest", None);
    assert_eq!(scratch.status, 403, "{}", scratch.text());
}

/// A microVM committed as the API asks: named, its author, comment and Dockerfile changes
/// kept, its files as it left them; an empty config is none, another refused.
#[test]
fn a_microvm_is_committed_as_an_image() {
    let Some((home, image)) = home("eng-commit") else {
        return;
    };
    let mut api = Api::new(&home);
    let made = api.call(
        "POST",
        "/containers/create?name=worked",
        Some(json!({"Image": image, "Cmd": ["fs", "write:/tmp/made=1"]})),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    assert_eq!(api.call("POST", "/containers/worked/start", None).status, 204);
    assert_eq!(
        api.call("POST", "/containers/worked/wait", None).json()["StatusCode"],
        json!(0)
    );
    let committed = api.call(
        "POST",
        "/commit?container=worked&repo=shp/worked&tag=1&author=api&comment=its%20work&changes=ENV%20K%3Dv",
        Some(json!({})),
    );
    assert_eq!(committed.status, 201, "{}", committed.text());
    let id = committed.json()["Id"].as_str().unwrap().to_string();
    let doc = api.call("GET", "/images/shp/worked:1/json", None).json();
    assert_eq!((&doc["Id"], &doc["Author"]), (&json!(id), &json!("api")), "{doc}");
    assert!(
        doc["Config"]["Env"].as_array().unwrap().contains(&json!("K=v")),
        "{doc}"
    );
    let history = api.call("GET", "/images/shp/worked:1/history", None).json();
    assert_eq!(history[0]["Comment"], json!("its work"), "{history}");
    // Its files as it left them, in a microVM of the new image.
    let read = api.call(
        "POST",
        "/containers/create?name=reread",
        Some(json!({"Image": "shp/worked:1", "Cmd": ["stat", "/tmp/made"]})),
    );
    assert_eq!(read.status, 201, "{}", read.text());
    assert_eq!(api.call("POST", "/containers/reread/start", None).status, 204);
    assert_eq!(
        api.call("POST", "/containers/reread/wait", None).json()["StatusCode"],
        json!(0)
    );
    assert!(
        output_of(&mut api, "reread").contains("= 1"),
        "{}",
        output_of(&mut api, "reread")
    );
    let configured = api.call(
        "POST",
        "/commit?container=worked&repo=shp/worked&tag=2",
        Some(json!({"Cmd": ["other"]})),
    );
    assert_eq!(configured.status, 400, "{}", configured.text());
    assert!(configured.text().contains("changes"), "{}", configured.text());
    let nothing = api.call("POST", "/commit?container=nothing", None);
    assert_eq!(nothing.status, 404, "{}", nothing.text());
}

/// Images built as the API asks, from a context in the call's body: each line of the build
/// a `stream` message, then its image's ID (`aux`) and the lines clients find it by; its
/// tags, build arguments and labels kept, its RUN step run in a microVM; with `q`, the ID
/// alone. A build that fails says why last; one through a BuildKit session is refused.
#[test]
fn images_are_built_from_a_context_in_the_body() {
    let Some((home, image)) = home("eng-build") else {
        return;
    };
    let mut api = Api::new(&home);
    let lines = |a: &Answered| -> Vec<Value> {
        a.text()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let dockerfile = format!(
        "FROM {image}\nARG GREETING\nCOPY note.txt /note.txt\nRUN [\"/bin/testguest\", \"fs\", \"write:/tmp/made=1\"]\nLABEL k=v\nENV SAID=$GREETING\n"
    );
    let context = tar(&[
        ("Dockerfile", 0o644, 0, Some(dockerfile.as_bytes())),
        ("note.txt", 0o644, 0, Some(b"from the context\n")),
    ]);
    let built = api.upload(
        "POST",
        "/build?t=shp/built:1&buildargs=%7B%22GREETING%22%3A%22hi%22%7D&labels=%7B%22made%22%3A%22api%22%7D",
        "application/x-tar",
        &context,
    );
    assert_eq!(built.status, 200, "{}", built.text());
    let said = lines(&built);
    assert!(said.iter().all(|m| m.get("error").is_none()), "{}", built.text());
    let id = said
        .iter()
        .find_map(|m| m["aux"]["ID"].as_str())
        .unwrap_or_else(|| panic!("{}", built.text()))
        .to_string();
    let short = id.trim_start_matches("sha256:").get(..12).unwrap().to_string();
    for wanted in [
        format!("Successfully built {short}\n"),
        "Successfully tagged shp/built:1\n".to_string(),
    ] {
        assert!(
            said.contains(&json!({ "stream": wanted })),
            "{wanted}: {}",
            built.text()
        );
    }
    // Squashed, as dockerd's experimental build squashes (D177): the build's layers past
    // its base's as one, named by the merge's history.
    let squashed = api.upload(
        "POST",
        "/build?t=shp/squashed:1&squash=1&buildargs=%7B%22GREETING%22%3A%22hi%22%7D",
        "application/x-tar",
        &context,
    );
    assert_eq!(squashed.status, 200, "{}", squashed.text());
    assert!(
        lines(&squashed).iter().all(|m| m.get("error").is_none()),
        "{}",
        squashed.text()
    );
    let mut layers = |name: &str| -> usize {
        let doc = api.call("GET", &format!("/images/{name}/json"), None).json();
        doc["RootFS"]["Layers"].as_array().map_or(0, Vec::len)
    };
    let (base, plain, one) = (layers(&image), layers("shp/built:1"), layers("shp/squashed:1"));
    assert_eq!((plain, one), (base + 2, base + 1));
    let history = api.call("GET", "/images/shp/squashed:1/history", None).json();
    assert!(
        history[0]["Comment"]
            .as_str()
            .is_some_and(|c| c.starts_with("merge sha256:")),
        "{history}"
    );
    let doc = api.call("GET", "/images/shp/built:1/json", None).json();
    assert_eq!(
        (
            &doc["Id"],
            &doc["Config"]["Labels"]["made"],
            &doc["Config"]["Labels"]["k"]
        ),
        (&json!(id), &json!("api"), &json!("v")),
        "{doc}"
    );
    assert!(
        doc["Config"]["Env"]
            .as_array()
            .unwrap()
            .contains(&json!("SAID=hi")),
        "{doc}"
    );
    // Its files: the context's, and what its RUN step made in its microVM.
    let made = api.call(
        "POST",
        "/containers/create?name=from-build",
        Some(json!({"Image": "shp/built:1", "Cmd": ["stat", "/note.txt", "/tmp/made"]})),
    );
    assert_eq!(made.status, 201, "{}", made.text());
    assert_eq!(api.call("POST", "/containers/from-build/start", None).status, 204);
    assert_eq!(
        api.call("POST", "/containers/from-build/wait", None).json()["StatusCode"],
        json!(0)
    );
    let out = output_of(&mut api, "from-build");
    assert!(
        out.contains("= from the context\\n") && out.contains("= 1"),
        "{out}"
    );
    // `q`: the ID alone.
    let quiet = api.upload("POST", "/build?q=1&t=shp/built:2", "application/x-tar", &context);
    assert_eq!(quiet.status, 200, "{}", quiet.text());
    let quiet = lines(&quiet);
    assert_eq!(quiet.len(), 1, "{quiet:?}");
    assert!(
        quiet[0]["stream"].as_str().unwrap().starts_with("sha256:"),
        "{quiet:?}"
    );
    // A RUN that fails: why, the stream's last message.
    let failing = format!("FROM {image}\nRUN [\"/bin/testguest\", \"exit\", \"3\"]\n");
    let failed = api.upload(
        "POST",
        "/build",
        "application/x-tar",
        &tar(&[("Dockerfile", 0o644, 0, Some(failing.as_bytes()))]),
    );
    assert_eq!(failed.status, 200, "{}", failed.text());
    let last = lines(&failed).pop().unwrap();
    assert!(
        last["error"].as_str().unwrap().contains("exit code: 3"),
        "{}",
        failed.text()
    );
    // A context sent through a BuildKit session: refused, with why.
    let session = api.upload(
        "POST",
        "/build?version=2&session=s1",
        "application/x-tar",
        &context,
    );
    assert_eq!(session.status, 400, "{}", session.text());
    assert!(session.text().contains("session"), "{}", session.text());
}

/// A call answered before its body is read is answered as it comes, and the rest of its
/// body read, so its client reads the answer at whatever pace it sends and keeps its
/// connection; each body larger than Go's server would read (256 KiB). On one connection:
/// a build's context sent through a session, refused by the build, and a call of a version
/// not served, refused before it is routed, each answer read before any of the body is
/// sent; a call's body past what a call may carry, answered once that much is read; and a
/// call after them.
#[test]
fn a_call_answered_first_keeps_its_connection() {
    let Some((home, _)) = home("eng-early") else {
        return;
    };
    let mut api = Api::new(&home);
    let head = |target: &str, kind: &str, len: usize| {
        format!(
            "POST {target} HTTP/1.1\r\nHost: api\r\nContent-Type: {kind}\r\nContent-Length: {len}\r\n\r\n"
        )
    };
    let body = vec![0_u8; 4 << 20];
    for (target, says) in [
        ("/build?version=2&session=s1", "session"),
        ("/v1.0/build", "API version 1.0"),
    ] {
        api.conn
            .write_all(head(target, "application/x-tar", body.len()).as_bytes())
            .unwrap();
        let refused = api.answer("POST");
        assert_eq!(refused.status, 400, "{target}: {}", refused.text());
        assert!(refused.text().contains(says), "{target}: {}", refused.text());
        assert_ne!(refused.field("connection"), Some("close"), "{target}");
        api.conn.write_all(&body).unwrap();
    }
    let past = vec![b' '; 9 << 20];
    api.conn
        .write_all(head("/containers/create", "application/json", past.len()).as_bytes())
        .unwrap();
    api.conn.write_all(&past).unwrap();
    let refused = api.answer("POST");
    assert_eq!(refused.status, 413, "{}", refused.text());
    assert_eq!(api.call("GET", "/_ping", None).status, 200);
}

/// The docker CLI checkpoints a microVM through the engine and restores it, as it does an
/// experimental dockerd's container (D176): `checkpoint create`, `ls`, `rm` and `start
/// --checkpoint`, the engine saying it is experimental as its ping does.
#[test]
fn the_docker_cli_checkpoints_and_restores_microvms() {
    let Ok(docker) = which("docker") else {
        eprintln!("SKIP: no docker CLI on this host");
        return;
    };
    let Some((home, image)) = home("eng-cp") else {
        return;
    };
    if !shards_vmm::vm::SNAPSHOTS {
        return;
    }
    let config = home.join("docker-config");
    std::fs::create_dir_all(&config).unwrap();
    // No credential helper on PATH: what the CLI keeps stays in the test's config.
    let no_helpers = home.join("path");
    std::fs::create_dir_all(&no_helpers).unwrap();
    let host = format!("unix://{}", home.join("engine.sock").display());
    let docker = |args: &[&str]| {
        let out = common::output_within(
            std::process::Command::new(&docker)
                .args(args)
                .env("DOCKER_HOST", &host)
                .env("DOCKER_CONFIG", &config)
                .env("PATH", &no_helpers)
                .env_remove("DOCKER_CONTEXT")
                .env("DOCKER_CLI_HINTS", "false")
                .stdin(std::process::Stdio::null()),
            CLIENT,
        );
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (status, out, err) = docker(&["version", "--format", "{{.Server.Experimental}}"]);
    assert_eq!((status, out.as_str()), (Some(0), "true\n"), "{err}");
    let (status, _, err) = docker(&["run", "-d", "--name", "cp", &image, "counter", "20"]);
    assert_eq!(status, Some(0), "{err}");
    let counts = || -> Vec<u64> {
        let (_, out, _) = docker(&["logs", "cp"]);
        out.lines().filter_map(|l| l.parse().ok()).collect()
    };
    let deadline = std::time::Instant::now() + CLIENT;
    while counts().len() < 10 {
        assert!(std::time::Instant::now() < deadline, "no counts");
        std::thread::sleep(Duration::from_millis(50));
    }
    let (status, out, err) = docker(&["checkpoint", "create", "cp", "first"]);
    assert_eq!((status, out.as_str()), (Some(0), "first\n"), "{err}");
    let n = counts().len();
    let (_, out, _) = docker(&["checkpoint", "ls", "cp"]);
    assert_eq!(out, "CHECKPOINT NAME\nfirst\n");
    let (status, out, err) = docker(&["start", "--checkpoint", "first", "cp"]);
    assert_eq!((status, out.as_str()), (Some(0), ""), "{err}");
    while counts().len() < n + 5 {
        assert!(
            std::time::Instant::now() < deadline,
            "no counts after the restore"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(counts()[n], n as u64 + 1);
    let (status, _, err) = docker(&["checkpoint", "create", "cp", "first"]);
    assert_eq!(status, Some(1));
    assert!(
        err.contains("checkpoint with name first already exists for container cp"),
        "{err}"
    );
    let (status, out, err) = docker(&["checkpoint", "rm", "cp", "first"]);
    assert_eq!((status, out.as_str()), (Some(0), ""), "{err}");
    let (_, out, _) = docker(&["checkpoint", "ls", "cp"]);
    assert_eq!(out, "CHECKPOINT NAME\n");
    let (status, _, _) = docker(&["rm", "-f", "cp"]);
    assert_eq!(status, Some(0));
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
    // An empty directory for PATH, where the CLI finds no credential helper: what its
    // login keeps goes in the test's config, never in this host's keychain.
    let no_helpers = home.join("path");
    std::fs::create_dir_all(&no_helpers).unwrap();
    let host = format!("unix://{}", home.join("engine.sock").display());
    let docker_with = |args: &[&str], set: &[(&str, &str)]| {
        let out = common::output_within(
            std::process::Command::new(&docker)
                .args(args)
                .env("DOCKER_HOST", &host)
                .env("DOCKER_CONFIG", &config)
                .env("PATH", &no_helpers)
                .env_remove("DOCKER_CONTEXT")
                .env("DOCKER_CLI_HINTS", "false")
                .envs(set.iter().copied())
                .stdin(std::process::Stdio::null()),
            CLIENT,
        );
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let docker = |args: &[&str]| docker_with(args, &[]);
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
    // Files in and out, what changed, what runs, all its files.
    let local = home.join("local.txt");
    std::fs::write(&local, "through the cli\n").unwrap();
    let (status, _, err) = docker(&["cp", local.to_str().unwrap(), "cli-held:/tmp/"]);
    assert_eq!(status, Some(0), "{err}");
    let back = home.join("back");
    std::fs::create_dir_all(&back).unwrap();
    let (status, _, err) = docker(&["cp", "cli-held:/tmp/local.txt", back.to_str().unwrap()]);
    assert_eq!(status, Some(0), "{err}");
    assert_eq!(
        std::fs::read_to_string(back.join("local.txt")).unwrap(),
        "through the cli\n"
    );
    let (status, out, err) = docker(&["diff", "cli-held"]);
    assert_eq!(status, Some(0), "{err}");
    assert!(out.lines().any(|l| l == "A /tmp/local.txt"), "{out}");
    let (status, out, err) = docker(&["top", "cli-held"]);
    assert_eq!(status, Some(0), "{err}");
    assert!(out.contains("testguest sleep"), "{out}");
    let exported = home.join("held.tar");
    let (status, _, err) = docker(&["export", "-o", exported.to_str().unwrap(), "cli-held"]);
    assert_eq!(status, Some(0), "{err}");
    let names: Vec<String> = untar(&std::fs::read(&exported).unwrap())
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert!(
        names
            .iter()
            .any(|n| n.trim_start_matches("./") == "tmp/local.txt"),
        "{names:?}"
    );
    // Watched and managed.
    let (status, out, err) = docker(&[
        "stats",
        "--no-stream",
        "--format",
        "{{.Name}} {{.CPUPerc}}",
        "cli-held",
    ]);
    assert_eq!(status, Some(0), "{err}");
    assert!(
        out.starts_with("cli-held ") && out.trim_end().ends_with('%'),
        "{out}"
    );
    for verb in ["pause", "unpause"] {
        let (status, _, err) = docker(&[verb, "cli-held"]);
        assert_eq!(status, Some(0), "{verb}: {err}");
    }
    let (status, _, err) = docker(&["restart", "-t", "1", "cli-held"]);
    assert_eq!(status, Some(0), "{err}");
    let (status, _, err) = docker(&["update", "--cpu-shares", "512", "cli-held"]);
    assert_eq!(status, Some(0), "{err}");
    let (status, out, err) = docker(&[
        "inspect",
        "--format",
        "{{.HostConfig.CpuShares}} {{.State.Running}}",
        "cli-held",
    ]);
    assert_eq!((status, out.trim()), (Some(0), "512 true"), "{err}");
    let (status, _, err) = docker(&["rename", "cli-held", "cli-renamed"]);
    assert_eq!(status, Some(0), "{err}");
    let (status, _, err) = docker(&["rm", "-f", "cli-renamed"]);
    assert_eq!(status, Some(0), "{err}");
    let (_, out, _) = docker(&["ps", "-aq"]);
    assert_eq!(out.trim(), "");
    // Stopped ones pruned.
    let (status, _, err) = docker(&["create", "--name", "cli-stopped", &image]);
    assert_eq!(status, Some(0), "{err}");
    let (status, out, err) = docker(&["container", "prune", "-f"]);
    assert_eq!(status, Some(0), "{err}");
    assert!(out.starts_with("Deleted Containers:\n"), "{out}");
    let (_, out, _) = docker(&["ps", "-aq"]);
    assert_eq!(out.trim(), "");
    // Images named, traced, removed and pruned; what shards takes on disk.
    let (status, _, err) = docker(&["tag", &image, "shp/cli:1"]);
    assert_eq!(status, Some(0), "{err}");
    let (status, out, err) = docker(&["history", "-q", "shp/cli:1"]);
    assert_eq!(status, Some(0), "{err}");
    let (_, id, _) = docker(&["image", "inspect", "--format", "{{.Id}}", "shp/cli:1"]);
    let short = id
        .trim()
        .trim_start_matches("sha256:")
        .get(..12)
        .unwrap_or_default()
        .to_string();
    assert!(out.lines().all(|l| l == short || l == "<missing>"), "{out}");
    let (status, out, err) = docker(&["rmi", "shp/cli:1"]);
    assert_eq!(
        (status, out.as_str()),
        (Some(0), "Untagged: shp/cli:1\n"),
        "{err}"
    );
    let (status, out, err) = docker(&["system", "df", "--format", "{{.Type}} {{.TotalCount}}"]);
    assert_eq!(status, Some(0), "{err}");
    let images: usize = out
        .lines()
        .find_map(|l| l.strip_prefix("Images "))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(images >= 1, "{out}");
    let (status, out, err) = docker(&["image", "prune", "-f"]);
    assert_eq!(
        (status, out.as_str()),
        (Some(0), "Total reclaimed space: 0B\n"),
        "{err}"
    );
    // A microVM committed; images saved, loaded, imported and pushed.
    let (status, _, err) = docker(&["run", "--name", "cli-work", &image, "fs", "write:/tmp/w=1"]);
    assert_eq!(status, Some(0), "{err}");
    let (status, out, err) = docker(&["commit", "cli-work", "shp/cli-work:1"]);
    assert_eq!(status, Some(0), "{err}");
    assert!(out.trim().starts_with("sha256:"), "{out}");
    let (status, _, err) = docker(&["rm", "cli-work"]);
    assert_eq!(status, Some(0), "{err}");
    let saved = home.join("cli-saved.tar");
    let (status, _, err) = docker(&["save", "-o", saved.to_str().unwrap(), "shp/cli-work:1"]);
    assert_eq!(status, Some(0), "{err}");
    let (status, _, err) = docker(&["rmi", "shp/cli-work:1"]);
    assert_eq!(status, Some(0), "{err}");
    let (status, out, err) = docker(&["load", "-i", saved.to_str().unwrap()]);
    assert_eq!(
        (status, out.as_str()),
        (Some(0), "Loaded image: shp/cli-work:1\n"),
        "{err}"
    );
    let (_, blobs) = common::test_image_with(None);
    let root = home.join("cli-root.tar");
    std::fs::write(&root, &blobs[1]).unwrap();
    let (status, out, err) = docker(&["import", root.to_str().unwrap(), "shp/cli-root:1"]);
    assert_eq!(status, Some(0), "{err}");
    assert!(out.trim().starts_with("sha256:"), "{out}");
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode("pusher:secret")
    );
    let (port, repos) = common::writable_registry_requiring(Some(basic));
    let pushed = format!("127.0.0.1:{port}/team/cli:1");
    let (status, _, err) = docker(&["tag", "shp/cli-work:1", &pushed]);
    assert_eq!(status, Some(0), "{err}");
    // Logged in through the engine, which checks the credentials with the registry; the
    // CLI keeps them, and its push sends them.
    let registry = format!("127.0.0.1:{port}");
    let (status, _, err) = docker(&["login", "-u", "pusher", "-p", "wrong", &registry]);
    assert_eq!(status, Some(1), "{err}");
    assert!(err.contains("the registry refused the credentials"), "{err}");
    let (status, out, err) = docker(&["login", "-u", "pusher", "-p", "secret", &registry]);
    assert_eq!((status, out.as_str()), (Some(0), "Login Succeeded\n"), "{err}");
    let (status, out, err) = docker(&["push", &pushed]);
    assert_eq!(status, Some(0), "{out}{err}");
    assert!(out.contains("1: digest: sha256:"), "{out}");
    assert!(repos.lock().unwrap().manifests["team/cli"].contains_key("1"));
    // Searched through the engine, the CLI's credentials for the registry with it.
    let (status, out, err) = docker(&["search", "--format", "{{.Name}}", &format!("{registry}/team")]);
    assert_eq!(
        (status, out.as_str()),
        (Some(0), format!("{registry}/team/cli\n").as_str()),
        "{err}"
    );
    // Built by the CLI's builder of the API's first version, which sends its context in
    // the call's body (DOCKER_BUILDKIT=0; its config has no buildx).
    let context = home.join("cli-context");
    std::fs::create_dir_all(&context).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        format!("FROM {image}\nRUN [\"/bin/testguest\", \"fs\", \"write:/tmp/c=1\"]\n"),
    )
    .unwrap();
    let (status, out, err) = docker_with(
        &["build", "-t", "shp/cli-built:1", context.to_str().unwrap()],
        &[("DOCKER_BUILDKIT", "0")],
    );
    assert_eq!(status, Some(0), "{out}{err}");
    assert!(out.contains("Successfully tagged shp/cli-built:1"), "{out}");
    let (status, _, err) = docker(&["image", "inspect", "shp/cli-built:1"]);
    assert_eq!(status, Some(0), "{err}");
    // Squashed by the CLI's own --squash, which it offers an experimental engine (D177):
    // two steps' layers as one.
    std::fs::write(
        context.join("Dockerfile.two"),
        format!(
            "FROM {image}\nRUN [\"/bin/testguest\", \"fs\", \"write:/tmp/c=1\"]\nRUN [\"/bin/testguest\", \"fs\", \"write:/tmp/d=2\"]\n"
        ),
    )
    .unwrap();
    let two = context.join("Dockerfile.two");
    let (status, out, err) = docker_with(
        &[
            "build",
            "--squash",
            "-f",
            two.to_str().unwrap(),
            "-t",
            "shp/cli-squashed:1",
            context.to_str().unwrap(),
        ],
        &[("DOCKER_BUILDKIT", "0")],
    );
    assert_eq!(status, Some(0), "{out}{err}");
    let layers = |name: &str| -> usize {
        let (status, out, err) = docker(&["image", "inspect", "-f", "{{len .RootFS.Layers}}", name]);
        assert_eq!(status, Some(0), "{err}");
        out.trim().parse().unwrap()
    };
    assert_eq!(layers("shp/cli-squashed:1"), layers(&image) + 1);
    let (status, out, err) = docker(&[
        "history",
        "--no-trunc",
        "--format",
        "{{.Comment}}",
        "shp/cli-squashed:1",
    ]);
    assert_eq!(status, Some(0), "{err}");
    assert!(out.starts_with("merge sha256:"), "{out}");
}

/// A run's command reaches its own microVM's engine (`--use-api-socket`, D181): at
/// `DOCKER_HOST`, a socket in its own files, through which it makes, starts, waits for,
/// reads, lists and removes containers in its own microVM, each a joiner of it, named
/// there as the command named it and on the host after its microVM. It sees none of the
/// host's other microVMs, itself among them; what would reach past its microVM, and what
/// the engine does not serve, are refused with why. What its engine made ends as its
/// command does, and goes with its microVM.
#[test]
fn a_runs_command_reaches_its_own_microvms_engine() {
    let Some((home, image)) = home("eng-own") else {
        return;
    };
    let shards = |args: &[&str]| shards_in(&home, args);
    for (name, options) in [("other", &[][..]), ("prov", &["--use-api-socket"][..])] {
        let mut args = vec!["run", "-d", "--name", name];
        args.extend_from_slice(options);
        args.extend([image.as_str(), "sleep"]);
        let ran = shards(&args);
        assert_eq!(ran.status, Some(0), "{ran}");
    }
    // One call, by the command's own client in its microVM: its status line and body.
    let api = |method: &str, path: &str, body: &str| -> (String, String) {
        let out = shards(&["exec", "prov", "/bin/testguest", "api", method, path, body]);
        assert_eq!(out.status, Some(0), "{out}");
        let (status, body) = out.stdout.split_once('\n').unwrap_or((&out.stdout, ""));
        (status.to_string(), body.to_string())
    };
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let out = shards(&["exec", "prov", "/bin/testguest", "api", "GET", "/_ping", ""]);
        if out.status == Some(0) && out.stdout == "api HTTP/1.1 200 OK\nOK" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "its engine never answered: {out}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (status, info) = api("GET", "/info", "");
    assert_eq!(status, "api HTTP/1.1 200 OK");
    let info: Value = serde_json::from_str(&info).unwrap();
    let hostname = shards(&["inspect", "-f", "{{.Config.Hostname}}", "prov"]).stdout;
    assert_eq!(
        (
            &info["Containers"],
            &info["Images"],
            &info["Name"],
            &info["DockerRootDir"]
        ),
        (&json!(0), &json!(1), &json!(hostname.trim()), &json!("")),
        "{info}"
    );
    let made = |name: &str, cmd: &[&str]| -> String {
        let body = json!({ "Image": image, "Cmd": cmd }).to_string();
        let (status, said) = api("POST", &format!("/containers/create?name={name}"), &body);
        assert_eq!(status, "api HTTP/1.1 201 Created", "{said}");
        serde_json::from_str::<Value>(&said).unwrap()["Id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let id = made("inner", &["streams", "out", "err"]);
    assert_eq!(
        api("POST", "/containers/inner/start", "").0,
        "api HTTP/1.1 204 No Content"
    );
    let (status, waited) = api("POST", "/containers/inner/wait", "");
    assert_eq!(status, "api HTTP/1.1 200 OK");
    assert_eq!(
        serde_json::from_str::<Value>(&waited).unwrap()["StatusCode"],
        json!(0),
        "{waited}"
    );
    assert_eq!(
        api("GET", "/containers/inner/logs?stdout=1&stderr=1", ""),
        ("api HTTP/1.1 200 OK".into(), "out\nerr\n".into())
    );
    // By the name it gave, alone among the host's microVMs; on the host, after its own.
    let (_, listed) = api("GET", "/containers/json?all=1", "");
    let listed: Value = serde_json::from_str(&listed).unwrap();
    let names: Vec<&Value> = listed.as_array().unwrap().iter().map(|c| &c["Names"]).collect();
    assert_eq!(names, [&json!(["/inner"])], "{listed}");
    let short: String = id.chars().take(12).collect();
    let (_, inspected) = api("GET", &format!("/containers/{short}/json"), "");
    assert_eq!(
        serde_json::from_str::<Value>(&inspected).unwrap()["Name"],
        json!("/inner")
    );
    let host_names = || shards(&["ps", "-a", "--format", "{{.Names}}"]).stdout;
    assert!(
        host_names().lines().any(|n| n == "prov.inner"),
        "{}",
        host_names()
    );
    for other in ["other", "prov"] {
        assert_eq!(
            api("GET", &format!("/containers/{other}/json"), ""),
            (
                "api HTTP/1.1 404 Not Found".into(),
                format!("{{\"message\":\"No such container: {other}\"}}\n")
            )
        );
    }
    // What would reach past its microVM, what is not served, and a login from inside.
    let body = json!({ "Image": image, "HostConfig": { "Binds": ["/etc:/x"] } }).to_string();
    assert_eq!(
        api("POST", "/containers/create", &body),
        (
            "api HTTP/1.1 403 Forbidden".into(),
            "{\"message\":\"-v /etc:/x: a container made by a microVM's own engine is in that microVM, and reaches nothing past it\"}\n".into()
        )
    );
    let (status, _) = api("GET", "/images/json", "");
    assert_eq!(status, "api HTTP/1.1 501 Not Implemented");
    let (status, _) = api("POST", "/auth", "{}");
    assert_eq!(status, "api HTTP/1.1 403 Forbidden");
    assert_eq!(
        api("DELETE", "/containers/inner", "").0,
        "api HTTP/1.1 204 No Content"
    );
    assert!(
        !host_names().lines().any(|n| n == "prov.inner"),
        "{}",
        host_names()
    );
    // One left running ends as its microVM's command does; it goes with its microVM.
    made("held", &["sleep"]);
    assert_eq!(
        api("POST", "/containers/held/start", "").0,
        "api HTTP/1.1 204 No Content"
    );
    let state = |name: &str| shards(&["inspect", "-f", "{{.State.Status}}", name]).stdout;
    assert_eq!(state("prov.held"), "running\n");
    let killed = shards(&["kill", "prov"]);
    assert_eq!(killed.status, Some(0), "{killed}");
    let deadline = std::time::Instant::now() + TIMEOUT;
    while state("prov.held") != "exited\n" || state("prov") != "exited\n" {
        assert!(
            std::time::Instant::now() < deadline,
            "prov.held: {}; prov: {}",
            state("prov.held"),
            state("prov")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let removed = shards(&["rm", "prov"]);
    assert_eq!(removed.status, Some(0), "{removed}");
    assert_eq!(host_names(), "other\n");
    let removed = shards(&["rm", "-f", "other"]);
    assert_eq!(removed.status, Some(0), "{removed}");
}

/// The docker CLI's own `--use-api-socket`, asked of shards' engine: the microVM's own
/// engine at `DOCKER_HOST` in place of the CLI's engine's socket, and the credentials the
/// CLI copies in refused, which it warns of: they never reach the microVM.
#[test]
fn the_docker_clis_use_api_socket_is_the_microvms_own_engine() {
    let Ok(docker) = which("docker") else {
        eprintln!("SKIP: no docker CLI on this host");
        return;
    };
    let Some((home, image)) = home("eng-cli-own") else {
        return;
    };
    // The test's own config, a registry's credentials in it that the CLI means to copy in,
    // and no credential helper on PATH.
    let config = home.join("docker-config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.json"),
        r#"{"auths":{"registry.invalid":{"auth":"dXNlcjpzZWNyZXQ="}}}"#,
    )
    .unwrap();
    let no_helpers = home.join("path");
    std::fs::create_dir_all(&no_helpers).unwrap();
    let host = format!("unix://{}", home.join("engine.sock").display());
    let docker = |args: &[&str]| {
        let out = common::output_within(
            std::process::Command::new(&docker)
                .args(args)
                .env("DOCKER_HOST", &host)
                .env("DOCKER_CONFIG", &config)
                .env("PATH", &no_helpers)
                .env_remove("DOCKER_CONTEXT")
                .env("DOCKER_CLI_HINTS", "false")
                .env("DOCKER_CLI_EXPERIMENTAL", "enabled")
                .stdin(std::process::Stdio::null()),
            CLIENT,
        );
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let (status, out, err) = docker(&[
        "run",
        "--rm",
        "--use-api-socket",
        &image,
        "api",
        "GET",
        "/_ping",
        "",
    ]);
    assert_eq!(
        (status, out.as_str()),
        (Some(0), "api HTTP/1.1 200 OK\nOK"),
        "{err}"
    );
    assert!(
        err.contains("WARNING: injecting docker config.json into container failed: copying config.json into container failed: Error response from daemon: your registries' credentials stay outside the microVM"),
        "{err}"
    );
    let (status, out, err) = docker(&[
        "run",
        "--rm",
        "--use-api-socket",
        &image,
        "stat",
        "/run/secrets/docker/config.json",
        "/var/run/docker.sock",
    ]);
    assert_eq!(status, Some(1), "{err}");
    assert!(
        out.lines().all(|l| l.contains(" missing ")),
        "nothing of the CLI's is in the microVM: {out}"
    );
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

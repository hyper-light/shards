//! Making shards' microVMs visible in the machine's local image store: Docker's, found as
//! the Docker CLI finds its engine, where there is one. A microVM goes there as an OCI
//! artifact, its EROFS disk the one layer (shards_image::save::microvm), named
//! `shards.local/` and the image's familiar name, so that it stands beside Docker's own
//! images and replaces none of them; `docker images` lists it and `docker run` will not
//! run it (Docker 29.3.1, measured 2026-10-04). Where no store answers, nothing is made
//! (shards' own local registry is to come).
//!
//! The engine is found as docker/cli finds it (cli/command/cli.go, resolveDockerEndpoint;
//! cli/context/store): `DOCKER_HOST`, else `DOCKER_CONTEXT`'s endpoint, else the config's
//! `currentContext`'s, else `/var/run/docker.sock`. Only a Unix socket is spoken to.
//!
//! A pull or `rmi` only queues its request: each name's go in order on a thread of the
//! name's, and names do not wait on each other, however long the engine takes. No upload
//! is cut off part way, by a deadline or otherwise: Docker 29.3.1 keeps the content of one
//! cut off locked, and later loads of it fail or never end (PM M118); an engine stuck on
//! one image's content so holds up that image alone. A microVM the engine holds already,
//! by its manifest's digest, is not sent again.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use shards_image::reference::{Digest, Reference};

/// Where a microVM is named in the local store: `shards.local/` and the familiar name.
pub fn name(reference: &str) -> String {
    let familiar = Reference::parse_normalized(reference)
        .map(|r| r.tag_name_only().familiar())
        .unwrap_or_else(|_| reference.to_string());
    format!("shards.local/{familiar}")
}

/// The local engine's socket, if one is named and there.
pub fn engine(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let unix = |host: &str| host.strip_prefix("unix://").map(PathBuf::from);
    if let Some(host) = env("DOCKER_HOST").filter(|h| !h.is_empty()) {
        return unix(&host).filter(|p| p.exists());
    }
    let home = env("HOME").map(PathBuf::from);
    let config = env("DOCKER_CONFIG")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".docker")));
    let context = env("DOCKER_CONTEXT").filter(|c| !c.is_empty()).or_else(|| {
        let text = std::fs::read(config.as_ref()?.join("config.json")).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&text).ok()?;
        v.get("currentContext")?.as_str().map(str::to_string)
    });
    if let Some(context) = context.filter(|c| c != "default")
        && let Some(config) = &config
    {
        use sha2::{Digest as _, Sha256};
        let id: String = Sha256::digest(context.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let meta = std::fs::read(config.join("contexts/meta").join(id).join("meta.json")).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&meta).ok()?;
        let host = v.pointer("/Endpoints/docker/Host")?.as_str()?;
        return unix(host).filter(|p| p.exists());
    }
    Some(PathBuf::from("/var/run/docker.sock")).filter(|p| p.exists())
}

/// What the publisher is asked to do, in the engine at `socket`.
pub enum Job {
    /// [`publish`] these.
    Publish {
        socket: PathBuf,
        reference: String,
        source: Digest,
        platform: String,
        disk: PathBuf,
        config: Vec<u8>,
    },
    /// [`unpublish`] this name.
    Unpublish { socket: PathBuf, reference: String },
}

impl Job {
    fn reference(&self) -> &str {
        match self {
            Job::Publish { reference, .. } | Job::Unpublish { reference, .. } => reference,
        }
    }

    fn run(self) -> Result<(), String> {
        match self {
            Job::Publish {
                socket,
                reference,
                source,
                platform,
                disk,
                config,
            } => publish(&socket, &reference, &source, &platform, &disk, &config).map(drop),
            Job::Unpublish { socket, reference } => unpublish(&socket, &reference),
        }
    }
}

/// Each name's lane: the job waiting behind the one in flight, if any. A name has a lane
/// while a job of it is in flight.
static LANES: Mutex<BTreeMap<String, Option<Job>>> = Mutex::new(BTreeMap::new());

fn lanes() -> std::sync::MutexGuard<'static, BTreeMap<String, Option<Job>>> {
    LANES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Hands `job` to its name's lane: run at once on a thread of its own, or after the job of
/// that name in flight, in place of any already waiting there (the last word on a name is
/// the one that counts). Names do not wait on each other: an engine stuck on one image's
/// content holds up that image alone. Errors go to the daemon's log, where its stderr goes.
pub fn queue(job: Job) {
    let name = job.reference().to_string();
    {
        let mut lanes = lanes();
        if let Some(waiting) = lanes.get_mut(&name) {
            *waiting = Some(job);
            return;
        }
        lanes.insert(name.clone(), None);
    }
    let lane = name.clone();
    let spawned = std::thread::Builder::new()
        .name("shards-publish".into())
        .spawn(move || {
            let mut next = Some(job);
            while let Some(job) = next {
                if let Err(e) = job.run() {
                    let _ = writeln!(std::io::stderr(), "shards daemon {}: {e}", std::process::id());
                }
                let mut lanes = lanes();
                next = lanes.get_mut(&lane).and_then(Option::take);
                if next.is_none() {
                    lanes.remove(&lane);
                }
            }
        });
    if let Err(e) = spawned {
        lanes().remove(&name);
        let _ = writeln!(
            std::io::stderr(),
            "shards daemon {}: publishing {name}: {e}",
            std::process::id()
        );
    }
}

/// The ID the engine at `socket` gives image `named`, if it has one by that name.
fn engine_id(socket: &Path, named: &str) -> Result<Option<String>, String> {
    let stream = connect(socket)?;
    (&stream)
        .write_all(
            format!(
                "GET /images/{}/json HTTP/1.1\r\nHost: docker\r\nUser-Agent: shards\r\nConnection: close\r\n\r\n",
                escaped(named)
            )
            .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
    let mut answer = Vec::new();
    Read::read_to_end(&mut (&stream).take(1 << 20), &mut answer).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&answer);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    match head.split(' ').nth(1).unwrap_or_default() {
        "200" => {
            // Chunked or not, the one JSON object is what lies between its braces.
            let json = body.find('{').and_then(|at| body.get(at..=body.rfind('}')?));
            let id = json
                .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
                .and_then(|v| v.get("Id")?.as_str().map(str::to_string));
            Ok(id)
        }
        "404" => Ok(None),
        code => Err(format!("the local image store answered {code} for {named}")),
    }
}

/// `name` as one segment of an Engine API path.
fn escaped(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// A connection to the engine at `socket`.
fn connect(socket: &Path) -> Result<UnixStream, String> {
    UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))
}

/// Bytes written as HTTP/1.1 chunks.
struct Chunked<W: Write>(W);

impl<W: Write> Write for Chunked<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        write!(self.0, "{:x}\r\n", buf.len())?;
        self.0.write_all(buf)?;
        self.0.write_all(b"\r\n")?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// Puts the microVM whose disk is `disk`, made from image `source` of `reference`, for
/// `platform`, in the engine at `socket` (the Engine API's `POST /images/load`).
pub fn publish(
    socket: &Path,
    reference: &str,
    source: &Digest,
    platform: &str,
    disk: &Path,
    config: &[u8],
) -> Result<String, String> {
    let named = name(reference);
    let layout =
        shards_image::save::MicroVm::of(&named, disk, config, source, platform).map_err(|e| e.to_string())?;
    // One the engine holds already is not sent again: a load sends the whole disk.
    if engine_id(socket, &named)?.as_deref() == Some(layout.id()) {
        return Ok(named);
    }
    let stream = connect(socket)?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, &stream);
    out.write_all(
        b"POST /images/load?quiet=1 HTTP/1.1\r\nHost: docker\r\nUser-Agent: shards\r\nContent-Type: application/x-tar\r\nTransfer-Encoding: chunked\r\n\r\n",
    )
    .map_err(|e| e.to_string())?;
    let mut chunked = Chunked(out);
    layout.write(&mut chunked).map_err(|e| e.to_string())?;
    let mut out = chunked.0;
    out.write_all(b"0\r\n\r\n")
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())?;
    drop(out);
    let mut reader = BufReader::new(&stream);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| e.to_string())?;
    let code = status.split(' ').nth(1).unwrap_or_default();
    if code != "200" {
        // What the engine said, for the log.
        let mut rest = String::new();
        let _ = Read::read_to_string(&mut (&mut reader).take(4096), &mut rest);
        return Err(format!(
            "the local image store refused {named}: {} {}",
            status.trim(),
            rest.trim()
        ));
    }
    // The answer streams JSON lines; a failure to load is said in one of them.
    let mut body = String::new();
    let _ = Read::read_to_string(&mut (&mut reader).take(1 << 16), &mut body);
    if let Some(error) = body.split("\"error\":").nth(1) {
        return Err(format!("the local image store refused {named}: {}", error.trim()));
    }
    Ok(named)
}

/// Removes the microVM named for `reference` from the engine at `socket`, if it is there.
pub fn unpublish(socket: &Path, reference: &str) -> Result<(), String> {
    let named = name(reference);
    let stream = connect(socket)?;
    let path = escaped(&named);
    (&stream)
        .write_all(format!("DELETE /images/{path} HTTP/1.1\r\nHost: docker\r\nUser-Agent: shards\r\nConnection: close\r\n\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut status = String::new();
    BufReader::new(&stream)
        .read_line(&mut status)
        .map_err(|e| e.to_string())?;
    match status.split(' ').nth(1).unwrap_or_default() {
        "200" | "404" => Ok(()),
        _ => Err(format!(
            "the local image store would not remove {named}: {}",
            status.trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine on a socket of its own that answers each request with the next of
    /// `answers`, and hands back the request lines it was sent.
    fn fake_engine(answers: Vec<String>) -> (PathBuf, std::thread::JoinHandle<Vec<String>>) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = PathBuf::from(format!("/tmp/shards-ls-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let served = std::thread::spawn(move || {
            let mut asked = Vec::new();
            for answer in answers {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                asked.push(line.trim().to_string());
                // The headers, then a chunked body to its last chunk.
                let mut chunked = false;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                    chunked |= header.to_ascii_lowercase().contains("transfer-encoding: chunked");
                }
                while chunked {
                    let mut size = String::new();
                    reader.read_line(&mut size).unwrap();
                    let n = usize::from_str_radix(size.trim(), 16).unwrap();
                    let mut chunk = vec![0; n + 2];
                    Read::read_exact(&mut reader, &mut chunk).unwrap();
                    chunked = n > 0;
                }
                (&stream).write_all(answer.as_bytes()).unwrap();
            }
            asked
        });
        (socket, served)
    }

    fn disk() -> (PathBuf, Digest) {
        let dir = std::env::temp_dir().join(format!("shards-ls-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.erofs");
        std::fs::write(&disk, vec![7u8; 8192]).unwrap();
        let source = Digest::parse(&format!("sha256:{}", "ab".repeat(32))).unwrap();
        (disk, source)
    }

    /// A microVM the engine holds by its manifest's digest is not sent again; one it
    /// does not is loaded.
    #[test]
    fn a_microvm_the_engine_holds_is_not_sent_again() {
        let (disk, source) = disk();
        let config = br#"{"architecture":"arm64","os":"linux"}"#;
        let named = name("held:1");
        let id = shards_image::save::MicroVm::of(&named, &disk, config, &source, "linux/arm64").unwrap();
        let body = format!(r#"{{"Id":"{}"}}"#, id.id());
        let held = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let (socket, served) = fake_engine(vec![held]);
        assert_eq!(
            publish(&socket, "held:1", &source, "linux/arm64", &disk, config).unwrap(),
            named
        );
        assert_eq!(
            served.join().unwrap(),
            ["GET /images/shards.local%2Fheld%3A1/json HTTP/1.1"]
        );
        let (socket, served) = fake_engine(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".into(),
        ]);
        publish(&socket, "held:1", &source, "linux/arm64", &disk, config).unwrap();
        assert_eq!(
            served.join().unwrap(),
            [
                "GET /images/shards.local%2Fheld%3A1/json HTTP/1.1",
                "POST /images/load?quiet=1 HTTP/1.1"
            ]
        );
        let _ = std::fs::remove_file(socket);
    }

    /// A name's jobs go in order, those queued behind one in flight collapsed to the last;
    /// another name's do not wait behind them.
    #[test]
    fn a_names_jobs_go_in_order_and_names_do_not_wait() {
        let gone = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_string();
        // The first removal is held until the others are queued.
        let (slow, slow_served) = fake_engine(vec![gone.clone(), gone.clone()]);
        let gate = slow.with_extension("gate");
        let _ = std::fs::remove_file(&gate);
        let gate_listener = std::os::unix::net::UnixListener::bind(&gate).unwrap();
        let (asked, heard) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let held = std::thread::spawn(move || {
            let (stream, _) = gate_listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            asked.send(line).unwrap();
            // Held, unanswered, until the test lets it go.
            let _ = released.recv();
        });
        // A lane is busy while its first job is in flight: here, one that waits on the gate.
        queue(Job::Unpublish {
            socket: gate.clone(),
            reference: "lane:1".into(),
        });
        queue(Job::Unpublish {
            socket: slow.clone(),
            reference: "lane:1".into(),
        });
        queue(Job::Unpublish {
            socket: slow.clone(),
            reference: "lane:1".into(),
        });
        // Another name goes at once, while lane:1 waits.
        let (fast, fast_served) = fake_engine(vec![gone]);
        queue(Job::Unpublish {
            socket: fast.clone(),
            reference: "other:1".into(),
        });
        assert_eq!(fast_served.join().unwrap().len(), 1);
        let first = heard.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(
            first.starts_with("DELETE /images/shards.local%2Flane%3A1 "),
            "{first}"
        );
        assert!(
            lanes().get("lane:1").is_some_and(Option::is_some),
            "lane:1 waits behind its first job"
        );
        release.send(()).unwrap();
        held.join().unwrap();
        // The gate answered nothing: its job failed, and the one job left of the two ran.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while lanes().contains_key("lane:1") {
            assert!(std::time::Instant::now() < deadline, "lane:1 never emptied");
            std::thread::yield_now();
        }
        // One of the two answers was asked for: the engine still waits on the second.
        let probe = UnixStream::connect(&slow).unwrap();
        (&probe).write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(
            slow_served.join().unwrap()[0],
            "DELETE /images/shards.local%2Flane%3A1 HTTP/1.1"
        );
        for s in [slow, gate, fast] {
            let _ = std::fs::remove_file(s);
        }
    }

    #[test]
    fn a_microvm_is_named_beside_its_image() {
        assert_eq!(
            name("docker.io/library/ubuntu:latest"),
            "shards.local/ubuntu:latest"
        );
        assert_eq!(name("ghcr.io/a/b:1"), "shards.local/ghcr.io/a/b:1");
        assert_eq!(name("alpine"), "shards.local/alpine:latest");
    }

    #[test]
    fn the_engine_is_found_as_the_cli_finds_it() {
        let dir = std::env::temp_dir().join(format!("shards-engine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sock = dir.join("docker.sock");
        std::fs::create_dir_all(
            dir.join(
                ".docker/contexts/meta/fe9c6bd7a66301f49ca9b6a70b217107cd1284598bfc254700c989b916da791e",
            ),
        )
        .unwrap_or(());
        std::fs::write(&sock, b"").unwrap_or(());
        std::fs::write(
            dir.join(".docker/config.json"),
            br#"{"currentContext":"desktop-linux"}"#,
        )
        .unwrap_or(());
        let meta = format!(
            r#"{{"Endpoints":{{"docker":{{"Host":"unix://{}"}}}}}}"#,
            sock.display()
        );
        std::fs::write(
            dir.join(".docker/contexts/meta/fe9c6bd7a66301f49ca9b6a70b217107cd1284598bfc254700c989b916da791e/meta.json"),
            meta,
        )
        .unwrap_or(());
        let home = dir.display().to_string();
        let env = |k: &str| (k == "HOME").then(|| home.clone());
        assert_eq!(engine(&env), Some(sock.clone()));
        let host = format!("unix://{}", sock.display());
        let env = |k: &str| (k == "DOCKER_HOST").then(|| host.clone());
        assert_eq!(engine(&env), Some(sock));
        let env = |k: &str| (k == "DOCKER_HOST").then(|| "tcp://1.2.3.4:2375".to_string());
        assert_eq!(engine(&env), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

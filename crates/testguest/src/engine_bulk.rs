//! Bulk bytes through a run's own engine (D181, D230), from its command, as the E2E tests
//! and the measurement harness of the engine ring ask (crates/shards/tests/engine_ring.rs):
//!
//! - `bulk-emit N`: N bytes of [`pattern`] on stdout, as a joiner whose log the engine keeps.
//! - `bulk-sum`: its stdin's length and FNV-1a, `sum N HASH`, once it ends.
//! - `api-bulk logs CONTAINER N`: the container's log through `$DOCKER_HOST`, checked to be N
//!   bytes of the pattern (engine to command): `bulk logs N NANOS`.
//! - `api-bulk stdin IMAGE N`: a container of IMAGE running `bulk-sum`, attached with its
//!   stdin, given N bytes of the pattern and its stdin's end, its sum checked (command to
//!   engine): `bulk stdin N NANOS`.
//! - `api-bulk ping M [GAP_US]`: M pings on one kept connection, back to back or GAP_US
//!   microseconds apart (each side asleep between them), `ping M P50 P90 P99 MAX` in
//!   nanoseconds.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

/// Byte `i` of the stream every test sends.
pub fn pattern(i: u64) -> u8 {
    (i as u8).wrapping_mul(31).wrapping_add(7)
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

fn fnv(hash: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(hash, |h, &b| (h ^ u64::from(b)).wrapping_mul(FNV_PRIME))
}

/// The pattern's first `n` bytes' FNV-1a.
pub fn pattern_hash(n: u64) -> u64 {
    let mut h = FNV_OFFSET;
    let mut buf = vec![0u8; 64 << 10];
    let mut at = 0u64;
    while at < n {
        let len = (n - at).min(buf.len() as u64) as usize;
        for (i, b) in buf.iter_mut().take(len).enumerate() {
            *b = pattern(at + i as u64);
        }
        h = fnv(h, buf.get(..len).unwrap_or_default());
        at += len as u64;
    }
    h
}

pub fn emit(n: u64) -> i32 {
    let mut out = io::stdout().lock();
    let mut buf = vec![0u8; 64 << 10];
    let mut at = 0u64;
    while at < n {
        let len = (n - at).min(buf.len() as u64) as usize;
        for (i, b) in buf.iter_mut().take(len).enumerate() {
            *b = pattern(at + i as u64);
        }
        if out.write_all(buf.get(..len).unwrap_or_default()).is_err() {
            return 1;
        }
        at += len as u64;
    }
    i32::from(out.flush().is_err())
}

pub fn sum() -> i32 {
    let mut stdin = io::stdin().lock();
    let mut buf = vec![0u8; 64 << 10];
    let (mut n, mut h) = (0u64, FNV_OFFSET);
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => {
                h = fnv(h, buf.get(..k).unwrap_or_default());
                n += k as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return 1,
        }
    }
    let _ = writeln!(io::stdout(), "sum {n} {h:016x}");
    0
}

pub fn api_bulk(args: &[String]) -> i32 {
    let arg = |i: usize| args.get(i).map_or("", String::as_str);
    let said = match arg(0) {
        "logs" => logs(arg(1), arg(2).parse().unwrap_or(0)).map(|ns| format!("bulk logs {} {ns}", arg(2))),
        "stdin" => stdin(arg(1), arg(2).parse().unwrap_or(0)).map(|ns| format!("bulk stdin {} {ns}", arg(2))),
        "ping" => ping(arg(1).parse().unwrap_or(1), arg(2).parse().unwrap_or(0)),
        other => Err(format!("no api-bulk {other:?}")),
    };
    match said {
        Ok(line) => {
            let _ = writeln!(io::stdout(), "{line}");
            0
        }
        Err(e) => {
            let _ = writeln!(io::stdout(), "api-bulk error {e}");
            1
        }
    }
}

fn connect() -> Result<UnixStream, String> {
    let host = std::env::var("DOCKER_HOST").unwrap_or_default();
    let socket = host
        .strip_prefix("unix://")
        .ok_or_else(|| format!("DOCKER_HOST {host:?} names no socket"))?;
    UnixStream::connect(socket).map_err(|e| format!("{socket}: {e}"))
}

fn ask(conn: &mut UnixStream, method: &str, path: &str, body: &str, extra: &str) -> Result<(), String> {
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: api\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}",
        body.len()
    );
    conn.write_all(request.as_bytes())
        .map_err(|e| format!("{method} {path}: {e}"))
}

/// An answer's status line and fields, its head read a byte at a time so that nothing of
/// its body is taken.
fn head(conn: &mut UnixStream) -> Result<(String, Vec<(String, String)>), String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match conn.read(&mut byte) {
            Ok(1) => head.extend_from_slice(&byte),
            other => return Err(format!("an answer's head: {other:?}")),
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default().to_string();
    let fields = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase()))
        .collect();
    Ok((status, fields))
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// A body read as it comes, chunked or not, to its end, each piece to `take`.
fn body(
    conn: &mut UnixStream,
    fields: &[(String, String)],
    take: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut buf = vec![0u8; 256 << 10];
    if field(fields, "transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        loop {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while !line.ends_with(b"\r\n") {
                conn.read_exact(&mut byte)
                    .map_err(|e| format!("a chunk's size: {e}"))?;
                line.extend_from_slice(&byte);
            }
            let text = String::from_utf8_lossy(&line);
            let size = usize::from_str_radix(text.trim().split(';').next().unwrap_or_default(), 16)
                .map_err(|e| format!("a chunk's size {text:?}: {e}"))?;
            let mut left = size;
            while left > 0 {
                let want = left.min(buf.len());
                let piece = buf.get_mut(..want).ok_or("a chunk's piece")?;
                conn.read_exact(piece).map_err(|e| format!("a chunk: {e}"))?;
                take(piece)?;
                left -= want;
            }
            let mut crlf = [0u8; 2];
            conn.read_exact(&mut crlf)
                .map_err(|e| format!("a chunk's end: {e}"))?;
            if size == 0 {
                return Ok(());
            }
        }
    }
    let mut left = field(fields, "content-length").and_then(|n| n.parse::<u64>().ok());
    loop {
        let want = left.map_or(buf.len(), |l| (l.min(buf.len() as u64)) as usize);
        if want == 0 {
            return Ok(());
        }
        let piece = buf.get_mut(..want).ok_or("a piece")?;
        match conn.read(piece) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                take(piece.get(..n).ok_or("a piece")?)?;
                left = left.map(|l| l - n as u64);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("a body: {e}")),
        }
    }
}

/// Demultiplexes stdcopy frames (moby api/pkg/stdcopy) as their bytes come: stdout's to
/// `out`, the rest dropped.
struct Frames {
    header: Vec<u8>,
    left: u64,
    stdout: bool,
}

impl Frames {
    fn new() -> Frames {
        Frames {
            header: Vec::with_capacity(8),
            left: 0,
            stdout: false,
        }
    }

    fn feed(
        &mut self,
        mut bytes: &[u8],
        out: &mut dyn FnMut(&[u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        while !bytes.is_empty() {
            if self.left == 0 {
                let (one, rest) = bytes.split_first().ok_or("a frame")?;
                self.header.push(*one);
                bytes = rest;
                if let Ok([kind, _, _, _, a, b, c, d]) = <[u8; 8]>::try_from(self.header.as_slice()) {
                    self.left = u64::from(u32::from_be_bytes([a, b, c, d]));
                    self.stdout = kind == 1;
                    self.header.clear();
                }
                continue;
            }
            let n = (self.left.min(bytes.len() as u64)) as usize;
            let (now, rest) = bytes.split_at_checked(n).ok_or("a frame's bytes")?;
            if self.stdout {
                out(now)?;
            }
            self.left -= n as u64;
            bytes = rest;
        }
        Ok(())
    }
}

fn logs(container: &str, n: u64) -> Result<u128, String> {
    let mut conn = connect()?;
    let started = Instant::now();
    ask(
        &mut conn,
        "GET",
        &format!("/containers/{container}/logs?stdout=1"),
        "",
        "Connection: close\r\n",
    )?;
    let (status, fields) = head(&mut conn)?;
    if !status.contains(" 200 ") {
        return Err(format!("logs: {status}"));
    }
    let mut frames = Frames::new();
    let mut at = 0u64;
    let mut check = |bytes: &[u8]| -> Result<(), String> {
        for b in bytes {
            if *b != pattern(at) {
                return Err(format!("byte {at} of the log is {b}, not {}", pattern(at)));
            }
            at += 1;
        }
        Ok(())
    };
    let multiplexed = !field(&fields, "content-type").is_some_and(|v| v.contains("raw-stream"));
    body(&mut conn, &fields, &mut |piece| {
        if multiplexed {
            frames.feed(piece, &mut check)
        } else {
            check(piece)
        }
    })?;
    let took = started.elapsed().as_nanos();
    if at != n {
        return Err(format!("the log had {at} bytes, not {n}"));
    }
    Ok(took)
}

fn stdin(image: &str, n: u64) -> Result<u128, String> {
    let mut calls = connect()?;
    let create = format!(
        r#"{{"Image":"{image}","Cmd":["/bin/testguest","bulk-sum"],"OpenStdin":true,"StdinOnce":true,"AttachStdin":true,"AttachStdout":true,"AttachStderr":true}}"#
    );
    ask(&mut calls, "POST", "/containers/create", &create, "")?;
    let (status, fields) = head(&mut calls)?;
    let mut said = Vec::new();
    body(&mut calls, &fields, &mut |p| {
        said.extend_from_slice(p);
        Ok(())
    })?;
    let said = String::from_utf8_lossy(&said).into_owned();
    let id = said
        .split("\"Id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .ok_or_else(|| format!("create: {status} {said}"))?
        .to_string();
    let mut attached = connect()?;
    ask(
        &mut attached,
        "POST",
        &format!("/containers/{id}/attach?stream=1&stdin=1&stdout=1&stderr=1"),
        "",
        "Connection: Upgrade\r\nUpgrade: tcp\r\n",
    )?;
    let (status, _) = head(&mut attached)?;
    if !status.contains(" 101 ") && !status.contains(" 200 ") {
        return Err(format!("attach: {status}"));
    }
    ask(&mut calls, "POST", &format!("/containers/{id}/start"), "", "")?;
    let (status, fields) = head(&mut calls)?;
    body(&mut calls, &fields, &mut |_| Ok(()))?;
    if !status.contains(" 204 ") {
        return Err(format!("start: {status}"));
    }
    let reader = attached.try_clone().map_err(|e| format!("the attach: {e}"))?;
    let started = Instant::now();
    let output = std::thread::Builder::new()
        .name("bulk-out".into())
        .spawn(move || {
            let mut reader = reader;
            let mut streamed = Vec::new();
            let _ = reader.read_to_end(&mut streamed);
            let mut out = Vec::new();
            let mut frames = Frames::new();
            let _ = frames.feed(&streamed, &mut |p| {
                out.extend_from_slice(p);
                Ok(())
            });
            out
        })
        .map_err(|e| format!("a thread: {e}"))?;
    let mut buf = vec![0u8; 256 << 10];
    let mut at = 0u64;
    while at < n {
        let len = (n - at).min(buf.len() as u64) as usize;
        for (i, b) in buf.iter_mut().take(len).enumerate() {
            *b = pattern(at + i as u64);
        }
        attached
            .write_all(buf.get(..len).unwrap_or_default())
            .map_err(|e| format!("stdin at {at}: {e}"))?;
        at += len as u64;
    }
    // Its stdin's end: the client's half-close, as the docker CLI's CloseWrite.
    attached
        .shutdown(std::net::Shutdown::Write)
        .map_err(|e| format!("stdin's end: {e}"))?;
    let out = output.join().map_err(|_| "the reader failed".to_string())?;
    let took = started.elapsed().as_nanos();
    let out = String::from_utf8_lossy(&out).into_owned();
    let want = format!("sum {n} {:016x}", pattern_hash(n));
    if out.trim() != want {
        return Err(format!("the container said {:?}, not {want:?}", out.trim()));
    }
    Ok(took)
}

fn ping(m: usize, gap_us: u64) -> Result<String, String> {
    let mut conn = connect()?;
    let mut took = Vec::with_capacity(m);
    for _ in 0..m {
        std::thread::sleep(std::time::Duration::from_micros(gap_us));
        let t = Instant::now();
        ask(&mut conn, "GET", "/_ping", "", "")?;
        let (status, fields) = head(&mut conn)?;
        body(&mut conn, &fields, &mut |_| Ok(()))?;
        if !status.contains(" 200 ") {
            return Err(format!("ping: {status}"));
        }
        took.push(t.elapsed().as_nanos());
    }
    took.sort_unstable();
    let at = |q: usize| {
        took.get((took.len().saturating_sub(1)) * q / 100)
            .copied()
            .unwrap_or(0)
    };
    Ok(format!("ping {m} {} {} {} {}", at(50), at(90), at(99), at(100)))
}

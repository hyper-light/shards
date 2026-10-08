//! HTTP sources (`ADD URL`), fetched as BuildKit's http source fetches them through Go's
//! client (moby/buildkit dockerfile/1.27.1 source/http/httpsource.go; net/http), and held
//! to Docker Desktop's BuildKit v0.28 by measurement:
//! - one GET, asking for gzip and undoing it, as Go's transport does;
//! - the URL's userinfo sent as Basic credentials to the URL itself, never to where it
//!   redirects;
//! - redirects followed anywhere, at most 10;
//! - a source without a checksum is fetched while BuildKit works out its cache key, and
//!   refused for a status under 200 or from 400 up; one with a checksum is fetched when
//!   its snapshot is made, whatever the status, and refused if its SHA-256 differs;
//! - its mtime is the response's `Last-Modified` as Go's http.ParseTime reads it, else
//!   the epoch.
//!
//! The bytes go to a file as they arrive, hashed on the way, never held whole, within the
//! limits an image pull holds to (audit A10). Every source of a build is fetched at once,
//! as BuildKit's solver fetches them, each on a thread of its own.

use std::cell::Cell;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use sha2::Digest as _;
use shards_build::archive::over_budget;
use shards_image::reference::{Algorithm, Digest};
use shards_image::store::{Limits, Room};
use shards_registry::http::{Cancel, Client, Redirects, Request};
use shards_registry::url::Url;

/// A fetched file: where its bytes are, how many, and its mtime.
#[derive(Debug)]
pub struct Download {
    pub path: PathBuf,
    pub size: u64,
    /// What it hashed to, by the checksum's algorithm, or SHA-256 (BuildKit's pin).
    pub digest: Digest,
    /// Its Last-Modified, as `http.ParseTime` reads it, if it has one Go can read.
    pub last_modified: Option<(i64, u32)>,
}

/// Why a fetch failed, and when BuildKit fails it: while it works out the source's cache
/// key, which fetches a source without a checksum, or while it makes the snapshot.
#[derive(Debug)]
pub enum Failure {
    CacheKey(String),
    Snapshot(String),
}

/// A source to fetch, and what to fetch it with.
#[derive(Clone)]
struct Job {
    config: std::sync::Arc<shards_registry::tls::ClientConfig>,
    cancel: Cancel,
    url: String,
    checksum: Option<String>,
    path: PathBuf,
    limits: Limits,
}

impl Job {
    fn run(self) -> Result<Download, Failure> {
        let config = self.config;
        // As BuildKit's http source reaches a URL: through the proxies the environment names.
        let client = Client::new(
            Box::new(move |_| Ok(config.clone())),
            &format!("shards/{}", env!("CARGO_PKG_VERSION")),
        )
        .cancelled_by(self.cancel)
        .with_proxies(shards_registry::proxy::Proxies::from_env(&|k| {
            std::env::var(k).ok()
        }));
        fetch(
            &client,
            &self.url,
            self.checksum.as_deref(),
            self.path,
            &self.limits,
        )
    }
}

/// What a fetch ended with, and the operation whose source it was.
type Fetched = (usize, Result<Download, Failure>);

/// A build's HTTP sources, each being fetched on a thread of its own. Dropped, it cancels
/// the fetches still running and waits for their threads to end.
pub struct Downloads {
    cancel: Cancel,
    threads: Vec<JoinHandle<()>>,
    done: mpsc::Receiver<Fetched>,
    /// What has ended and is not yet taken, in the order it ended.
    ended: Vec<Fetched>,
}

impl Downloads {
    /// Starts fetching each source, `(op, url, checksum)`, into `dir`, each within
    /// `limits`.
    pub fn start(
        sources: Vec<(usize, String, Option<String>)>,
        dir: &Path,
        limits: Limits,
    ) -> Result<Downloads, String> {
        let (tx, done) = mpsc::channel();
        let mut downloads = Downloads {
            cancel: Cancel::new(),
            threads: Vec::new(),
            done,
            ended: Vec::new(),
        };
        if sources.is_empty() {
            return Ok(downloads);
        }
        // The platform's roots, as Go's default transport trusts: no registry's certs.d.
        let config = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
        for (op, url, checksum) in sources {
            let job = Job {
                config: config.clone(),
                cancel: downloads.cancel.clone(),
                url,
                checksum,
                path: dir.join(op.to_string()),
                limits,
            };
            let spawned = std::thread::Builder::new().name("shards-download".into()).spawn({
                let (job, tx) = (job.clone(), tx.clone());
                move || {
                    let _ = tx.send((op, job.run()));
                }
            });
            match spawned {
                Ok(thread) => downloads.threads.push(thread),
                // No thread to spare: this one fetches it now.
                Err(_) => downloads.ended.push((op, job.run())),
            }
        }
        Ok(downloads)
    }

    /// The first fetch to have failed, if one has. BuildKit's solver fails a build on the
    /// first failure, stopping the steps running; shards, which runs one step at a time,
    /// fails it before its next step, or while it waits for a download.
    pub fn failed(&mut self) -> Option<(usize, Failure)> {
        while let Ok(fetched) = self.done.try_recv() {
            self.ended.push(fetched);
        }
        let at = self.ended.iter().position(|(_, r)| r.is_err())?;
        match self.ended.remove(at) {
            (op, Err(failure)) => Some((op, failure)),
            (_, Ok(_)) => None,
        }
    }

    /// Operation `op`'s download, once fetched, or the first fetch to fail before it.
    pub fn take(&mut self, op: usize) -> Result<Download, (usize, Failure)> {
        loop {
            if let Some(failed) = self.failed() {
                return Err(failed);
            }
            if let Some(at) = self.ended.iter().position(|(o, _)| *o == op) {
                return self.ended.remove(at).1.map_err(|f| (op, f));
            }
            match self.done.recv() {
                Ok(fetched) => self.ended.push(fetched),
                Err(_) => {
                    let why = format!("operation {op}: no download of its source");
                    return Err((op, Failure::Snapshot(why)));
                }
            }
        }
    }
}

impl Drop for Downloads {
    fn drop(&mut self) {
        self.cancel.cancel();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// Fetches `url` into `path`, within `limits`; `checksum` is the source's
/// `http.checksum`.
pub fn fetch(
    client: &Client,
    url: &str,
    checksum: Option<&str>,
    path: PathBuf,
    limits: &Limits,
) -> Result<Download, Failure> {
    let fail = |e: String| match checksum {
        Some(_) => Failure::Snapshot(e),
        None => Failure::CacheKey(e),
    };
    let (target, authorization) = userinfo(url).map_err(fail)?;
    let parsed = Url::parse(&as_go_sends(&target)).map_err(|e| fail(e.to_string()))?;
    // Credentials from the userinfo go with the first request alone (net/http
    // Client.send), so never to where it redirects.
    let first = Cell::new(true);
    let authorize = |_: &Url| {
        Ok(if first.replace(false) {
            authorization.clone()
        } else {
            None
        })
    };
    let request = Request {
        method: "GET",
        url: &parsed,
        headers: &[("Accept-Encoding", "gzip")],
        body: &[],
        file: None,
    };
    let mut response = client
        .follow(&request, &authorize, Redirects::Anywhere)
        .map_err(|e| fail(format!("Get {:?}: {e}", shown(url))))?;
    if checksum.is_none() {
        if !(200..400).contains(&response.status) {
            return Err(fail(format!("invalid response status {}", response.status)));
        }
        // BuildKit asks with If-None-Match only for what its cache holds, so the ETag of a
        // 304 it did not ask for matches nothing (resolveMetadataRef).
        if response.status == 304 {
            let etag = response.header("etag").unwrap_or_default();
            return Err(fail(format!(
                "invalid not-modified ETag: {}",
                etag.strip_prefix("W/").unwrap_or(etag)
            )));
        }
    }
    let last_modified = response.header("last-modified").and_then(parse_time);
    let gzipped = response
        .header("content-encoding")
        .is_some_and(|e| e.eq_ignore_ascii_case("gzip"));
    let dir = path.parent().unwrap_or(&path);
    let mut room = Room::new(dir, limits).map_err(|e| fail(e.to_string()))?;
    let mut file = File::create(&path).map_err(|e| fail(format!("{}: {e}", path.display())))?;
    let mut to = Bounded {
        file: &mut file,
        room: &mut room,
        written: 0,
        limit: limits.bytes,
    };
    // Hashed as the checksum names: BuildKit hashes with SHA-256 whatever it names, so
    // that no SHA-384 or SHA-512 checksum matches.
    let algorithm = checksum
        .and_then(|c| Digest::parse(c).ok())
        .map_or(Algorithm::Sha256, |c| c.algorithm());
    let (size, got) = if gzipped {
        save(
            shards_image::store::gunzip(BufReader::new(&mut response)),
            &mut to,
            algorithm,
        )
    } else {
        save(&mut response, &mut to, algorithm)
    }
    .map_err(|e| fail(e.to_string()))?;
    if let Some(want) = checksum
        && got.to_string() != want
    {
        return Err(Failure::Snapshot(format!("digest mismatch {got}: {want}")));
    }
    Ok(Download {
        path,
        size,
        digest: got,
        last_modified,
    })
}

/// Fetches `url` into `path` now, on this thread, within `limits`; `checksum` is the
/// source's `http.checksum`.
pub fn fetch_now(
    url: &str,
    checksum: Option<&str>,
    path: PathBuf,
    limits: &Limits,
) -> Result<Download, String> {
    // The platform's roots, as Go's default transport trusts: no registry's certs.d.
    let config = shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?;
    let job = Job {
        config,
        cancel: Cancel::new(),
        url: url.to_string(),
        checksum: checksum.map(String::from),
        path,
        limits: *limits,
    };
    job.run().map_err(|f| match f {
        Failure::CacheKey(e) | Failure::Snapshot(e) => e,
    })
}

/// A download's file, written within the bytes ADD may write and the room its
/// filesystem has.
struct Bounded<'a> {
    file: &'a mut File,
    room: &'a mut Room,
    written: u64,
    limit: u64,
}

impl Bounded<'_> {
    fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.written = self.written.saturating_add(chunk.len() as u64);
        if self.written > self.limit {
            return Err(io::Error::other(over_budget(self.limit)));
        }
        self.file.write_all(chunk)?;
        self.room.wrote(chunk.len())
    }
}

/// A hash by one of the algorithms a digest may name.
enum Hasher {
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
}

impl Hasher {
    fn new(algorithm: Algorithm) -> Hasher {
        match algorithm {
            Algorithm::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
            Algorithm::Sha384 => Hasher::Sha384(sha2::Sha384::new()),
            Algorithm::Sha512 => Hasher::Sha512(sha2::Sha512::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Hasher::Sha256(h) => h.update(data),
            Hasher::Sha384(h) => h.update(data),
            Hasher::Sha512(h) => h.update(data),
        }
    }

    fn finish(self) -> Digest {
        match self {
            Hasher::Sha256(h) => Digest::from_hash(Algorithm::Sha256, &h.finalize()),
            Hasher::Sha384(h) => Digest::from_hash(Algorithm::Sha384, &h.finalize()),
            Hasher::Sha512(h) => Digest::from_hash(Algorithm::Sha512, &h.finalize()),
        }
    }
}

/// Copies `body` to `file`: its length and its hash by `algorithm`.
fn save(mut body: impl Read, file: &mut Bounded<'_>, algorithm: Algorithm) -> io::Result<(u64, Digest)> {
    let mut hasher = Hasher::new(algorithm);
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let chunk = buf.get(..n).unwrap_or_default();
        hasher.update(chunk);
        file.write(chunk)?;
        size = size.saturating_add(chunk.len() as u64);
    }
    Ok((size, hasher.finish()))
}

/// The name a download's file takes: BuildKit's pathutil.SafeFileName
/// (source/util/pathutil), the base of `name` trimmed of spaces, or `download` for one
/// that is empty, `.`, `..` or holds a control character. Bytes not UTF-8 are neither.
pub fn safe_file_name(name: &[u8]) -> Vec<u8> {
    // Go's range over a string: a char, or one byte not UTF-8 (U+FFFD).
    let mut units = Vec::new();
    let mut i = 0;
    while let Some(&b) = name.get(i) {
        let width = match b {
            0xf0..=0xf7 => 4,
            0xe0..=0xef => 3,
            0xc0..=0xdf => 2,
            _ => 1,
        };
        let c = name
            .get(i..i + width)
            .and_then(|s| std::str::from_utf8(s).ok())
            .and_then(|s| s.chars().next());
        let width = if c.is_some() { width } else { 1 };
        units.push((c, i, width));
        i += width;
    }
    let space = |u: &&(Option<char>, usize, usize)| u.0.is_some_and(char::is_whitespace);
    let start = units.iter().find(|u| !space(u)).map_or(name.len(), |u| u.1);
    let end = units
        .iter()
        .rev()
        .find(|u| !space(u))
        .map_or(start, |u| u.1 + u.2);
    let trimmed = name.get(start..end.max(start)).unwrap_or_default();
    // filepath.Base: what follows the last slash, trailing slashes dropped.
    let stripped = match trimmed.iter().rposition(|&b| b != b'/') {
        Some(last) => trimmed.get(..=last).unwrap_or_default(),
        None if trimmed.is_empty() => b".",
        None => b"/",
    };
    let base = match stripped.iter().rposition(|&b| b == b'/') {
        Some(slash) if stripped.len() > 1 => stripped.get(slash + 1..).unwrap_or_default(),
        _ => stripped,
    };
    let control = units
        .iter()
        .filter(|u| u.1 >= start && u.1 < end)
        .any(|u| u.0.is_some_and(char::is_control));
    if base.is_empty() || base == b"." || base == b".." || control {
        return b"download".to_vec();
    }
    base.to_vec()
}

/// The authority of `url`: after `scheme://`, up to its path, query or fragment.
fn authority(url: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    Some((scheme, authority, tail))
}

/// `url` without its userinfo, and the Basic credentials Go's client makes of it, the
/// user and password percent-decoded (net/url parseAuthority, validUserinfo, unescape).
pub(super) fn userinfo(url: &str) -> Result<(String, Option<String>), String> {
    let Some((scheme, authority, tail)) = authority(url) else {
        return Ok((url.to_string(), None));
    };
    let Some((info, host)) = authority.rsplit_once('@') else {
        return Ok((url.to_string(), None));
    };
    let invalid = |why: &str| format!("parse {:?}: {why}", shown(url));
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b"-._:~!$&'()*+,;=%@".contains(&b);
    if !info.bytes().all(allowed) {
        return Err(invalid("net/url: invalid userinfo"));
    }
    let (user, password) = info.split_once(':').unwrap_or((info, ""));
    let decode = |s: &str| -> Result<Vec<u8>, String> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            if c != b'%' {
                out.push(c);
                i += 1;
                continue;
            }
            let hex = |j: usize| b.get(j).and_then(|&d| char::from(d).to_digit(16));
            match (hex(i + 1), hex(i + 2)) {
                (Some(h), Some(l)) => out.push(u8::try_from(h * 16 + l).unwrap_or(0)),
                _ => {
                    let bad = s.get(i..(i + 3).min(s.len())).unwrap_or("%");
                    return Err(invalid(&format!("invalid URL escape {bad:?}")));
                }
            }
            i += 3;
        }
        Ok(out)
    };
    let basic = shards_registry::auth::basic(&decode(user)?, &decode(password)?);
    Ok((format!("{scheme}://{host}{tail}"), Some(basic)))
}

/// `url` as Go's net/url writes it back (URL.String()), and so as its client asks for
/// it: what Go takes in a path and RFC 3986 does not (a space, `|`, `{`) escaped, so that
/// the strict parser takes it too.
fn as_go_sends(url: &str) -> String {
    match shards_dockerfile::url::parse(url.as_bytes()) {
        Ok(u) => String::from_utf8_lossy(&u.string()).into_owned(),
        Err(_) => url.to_string(),
    }
}

/// `url` for an error, its password, if any, as Go's client shows one: `***`.
pub(super) fn shown(url: &str) -> String {
    let Some((scheme, authority, tail)) = authority(url) else {
        return url.to_string();
    };
    match authority.rsplit_once('@') {
        Some((info, host)) => match info.split_once(':') {
            Some((user, _)) => format!("{scheme}://{user}:***@{host}{tail}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// A part of a Go time layout, of those http.ParseTime's three use.
#[derive(Clone, Copy)]
enum Part {
    Text(&'static str),
    WeekDay,
    LongWeekDay,
    ZeroDay,
    UnderDay,
    Month,
    LongYear,
    Year,
    Hour,
    ZeroMinute,
    ZeroSecond,
    Zone,
}

/// http.ParseTime's layouts, in its order: http.TimeFormat
/// ("Mon, 02 Jan 2006 15:04:05 GMT"), time.RFC850 ("Monday, 02-Jan-06 15:04:05 MST") and
/// time.ANSIC ("Mon Jan _2 15:04:05 2006").
const LAYOUTS: [&[Part]; 3] = {
    use Part::*;
    [
        &[
            WeekDay,
            Text(", "),
            ZeroDay,
            Text(" "),
            Month,
            Text(" "),
            LongYear,
            Text(" "),
            Hour,
            Text(":"),
            ZeroMinute,
            Text(":"),
            ZeroSecond,
            Text(" GMT"),
        ],
        &[
            LongWeekDay,
            Text(", "),
            ZeroDay,
            Text("-"),
            Month,
            Text("-"),
            Year,
            Text(" "),
            Hour,
            Text(":"),
            ZeroMinute,
            Text(":"),
            ZeroSecond,
            Text(" "),
            Zone,
        ],
        &[
            WeekDay,
            Text(" "),
            Month,
            Text(" "),
            UnderDay,
            Text(" "),
            Hour,
            Text(":"),
            ZeroMinute,
            Text(":"),
            ZeroSecond,
            Text(" "),
            LongYear,
        ],
    ]
};

const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// An HTTP date as Go's http.ParseTime reads one (go1.25 net/http/header.go ParseTime,
/// time/format.go parse), in Unix seconds and nanoseconds. A zone other than GMT reads
/// as UTC, as Go reads a name its local zone does not have.
pub fn parse_time(value: &str) -> Option<(i64, u32)> {
    LAYOUTS
        .iter()
        .find_map(|layout| parse_layout(layout, value.as_bytes()))
}

fn parse_layout(layout: &[Part], mut v: &[u8]) -> Option<(i64, u32)> {
    // time.format's skip: a space in the layout takes any run of spaces in the value.
    fn skip<'a>(mut v: &'a [u8], text: &str) -> Option<&'a [u8]> {
        let mut t = text.as_bytes();
        while let Some((&c, rest)) = t.split_first() {
            if c == b' ' {
                if v.first().is_some_and(|&b| b != b' ') {
                    return None;
                }
                t = rest;
                while t.first() == Some(&b' ') {
                    t = t.get(1..)?;
                }
                while v.first() == Some(&b' ') {
                    v = v.get(1..)?;
                }
                continue;
            }
            if v.first() != Some(&c) {
                return None;
            }
            (t, v) = (rest, v.get(1..)?);
        }
        Some(v)
    }
    // The index of the name `v` starts with, matched without regard to case.
    fn lookup<'a>(names: &[&str], len: Option<usize>, v: &'a [u8]) -> Option<(usize, &'a [u8])> {
        names.iter().enumerate().find_map(|(i, name)| {
            let name = name.get(..len.unwrap_or(name.len()))?.as_bytes();
            let head = v.get(..name.len())?;
            head.eq_ignore_ascii_case(name)
                .then(|| (i, v.get(name.len()..).unwrap_or_default()))
        })
    }
    // One or two digits; exactly two if `fixed`.
    fn num(v: &[u8], fixed: bool) -> Option<(u32, &[u8])> {
        let d = |i: usize| {
            v.get(i)
                .filter(|b| b.is_ascii_digit())
                .map(|b| u32::from(b - b'0'))
        };
        match (d(0)?, d(1)) {
            (a, Some(b)) => Some((a * 10 + b, v.get(2..)?)),
            (_, None) if fixed => None,
            (a, None) => Some((a, v.get(1..)?)),
        }
    }
    let (mut year, mut month, mut day, mut hour, mut minute, mut second, mut nanos) =
        (0i64, 0u32, 0u32, 0, 0, 0, 0u32);
    for part in layout {
        match *part {
            Part::Text(t) => v = skip(v, t)?,
            Part::WeekDay => v = lookup(&DAYS, Some(3), v)?.1,
            Part::LongWeekDay => v = lookup(&DAYS, None, v)?.1,
            Part::Month => {
                let (i, rest) = lookup(&MONTHS, None, v)?;
                month = u32::try_from(i).ok()? + 1;
                v = rest;
            }
            Part::ZeroDay | Part::UnderDay => {
                if matches!(part, Part::UnderDay) && v.first() == Some(&b' ') {
                    v = v.get(1..)?;
                }
                (day, v) = num(v, matches!(part, Part::ZeroDay))?;
            }
            Part::LongYear => {
                let digits = v.get(..4).filter(|d| d.iter().all(u8::is_ascii_digit))?;
                year = std::str::from_utf8(digits).ok()?.parse().ok()?;
                v = v.get(4..)?;
            }
            Part::Year => {
                // atoi of two bytes: a sign may lead ("+5" is 2005, as Go reads it).
                let two = std::str::from_utf8(v.get(..2)?).ok()?;
                let digits = two.strip_prefix(['+', '-']).unwrap_or(two);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                let y: i64 = digits.parse().ok()?;
                let y = if two.starts_with('-') { -y } else { y };
                year = if y >= 69 { y + 1900 } else { y + 2000 };
                v = v.get(2..)?;
            }
            Part::Hour => {
                (hour, v) = num(v, false)?;
                if hour >= 24 {
                    return None;
                }
            }
            Part::ZeroMinute => {
                (minute, v) = num(v, true)?;
                if minute >= 60 {
                    return None;
                }
            }
            Part::ZeroSecond => {
                (second, v) = num(v, true)?;
                if second >= 60 {
                    return None;
                }
                // A fraction the layout does not name is read all the same: its first
                // nine digits.
                if v.first().is_some_and(|&b| b == b'.' || b == b',')
                    && v.get(1).is_some_and(u8::is_ascii_digit)
                {
                    let n = 1 + v.get(1..)?.iter().take_while(|b| b.is_ascii_digit()).count();
                    let digits = v.get(1..n.min(10))?;
                    nanos = digits.iter().fold(0u32, |acc, d| acc * 10 + u32::from(d - b'0'))
                        * 10u32.pow(u32::try_from(10 - n.min(10)).ok()?);
                    v = v.get(n..)?;
                }
            }
            Part::Zone => v = v.get(zone(v)?..)?,
        }
    }
    if !v.is_empty() || day < 1 || day > days_in(month, year) {
        return None;
    }
    let t = shards_dockerfile::go::Time {
        year,
        month,
        day,
        hour,
        minute,
        second,
        nanosecond: nanos,
        offset: 0,
    };
    Some(t.unix())
}

/// How long the zone `v` starts with is, as time.format's stdTZ reads one: `UTC`, or
/// what parseTimeZone takes.
fn zone(v: &[u8]) -> Option<usize> {
    if v.starts_with(b"UTC") {
        return Some(3);
    }
    if v.len() < 3 {
        return None;
    }
    if v.starts_with(b"ChST") || v.starts_with(b"MeST") {
        return Some(4);
    }
    // A signed hour offset of at most 23 after GMT or alone; GMT alone is 3 long.
    let offset = |s: &[u8]| -> usize {
        let Some((&sign, rest)) = s.split_first() else {
            return 0;
        };
        if sign != b'+' && sign != b'-' {
            return 0;
        }
        let n = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        let hours = rest
            .get(..n)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| d.parse::<u64>().ok());
        match hours {
            Some(h) if n > 0 && h <= 23 => 1 + n,
            _ => 0,
        }
    };
    if v.starts_with(b"GMT") {
        return Some(3 + offset(v.get(3..).unwrap_or_default()));
    }
    if v.first().is_some_and(|&b| b == b'+' || b == b'-') {
        return Some(offset(v)).filter(|&n| n > 0);
    }
    let upper = v.iter().take(6).take_while(|b| b.is_ascii_uppercase()).count();
    match upper {
        3 => Some(3),
        4 if v.get(3) == Some(&b'T') || v.starts_with(b"WITA") => Some(4),
        5 if v.get(4) == Some(&b'T') => Some(5),
        _ => None,
    }
}

/// The days of month `month` (1 to 12) of `year`.
fn days_in(month: u32, year: i64) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1..=12 => 31,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A URL is asked for as Go asks for it, what it escapes in a path escaped (measured,
    /// go1.27.1: url.Parse(s).String()), and the strict parser takes that.
    #[test]
    fn urls_are_asked_for_as_go_asks_for_them() {
        for (url, go) in [
            ("http://h/a b|c{d}^`", "http://h/a%20b%7Cc%7Bd%7D%5E%60"),
            ("http://h/caf\u{e9}", "http://h/caf%C3%A9"),
            ("http://h:8080/x%20y/z?q=1", "http://h:8080/x%20y/z?q=1"),
        ] {
            assert_eq!(as_go_sends(url), go);
            assert!(Url::parse(go).is_ok(), "{go}");
        }
    }

    /// What Go's http.ParseTime makes of each date, as BuildKit v0.28 gave it to a file
    /// in Docker Desktop (the mtimes measured, the second unchanged by the fraction).
    #[test]
    fn dates_read_as_go_reads_them() {
        let nov6 = Some((784_111_777, 0));
        for (date, want) in [
            ("Sun, 06 Nov 1994 08:49:37 GMT", nov6),
            ("Sunday, 06-Nov-94 08:49:37 GMT", nov6),
            ("Sun Nov  6 08:49:37 1994", nov6),
            ("sun, 06 nov 1994 08:49:37 GMT", nov6),
            ("Sun,  06 Nov 1994 08:49:37 GMT", nov6),
            ("Sunday, 06-Nov-94 08:49:37 PST", nov6),
            (
                "Sun, 06 Nov 1994 08:49:37.25 GMT",
                Some((784_111_777, 250_000_000)),
            ),
            (
                "Sun, 06 Nov 1994 08:49:37,1234567891 GMT",
                Some((784_111_777, 123_456_789)),
            ),
            ("Sun, 06 Nov 1994 8:49:37 GMT", nov6),
            ("Thu, 29 Feb 1996 00:00:00 GMT", Some((825_552_000, 0))),
            ("Thu, 01 Jan 1970 00:00:00 GMT", Some((0, 0))),
            ("Tuesday, 01-Jan-68 00:00:00 GMT", Some((3_092_601_600, 0))),
            ("Sunday, 06-Nov-94 08:49:37 GMT+5", nov6),
            ("Sunday, 06-Nov-94 08:49:37 UTC", nov6),
            // Refused, so the epoch: what Go refuses.
            ("Sun, 31 Feb 1994 08:49:37 GMT", None),
            ("Wed, 29 Feb 1995 08:49:37 GMT", None),
            ("Sun, 06 Nov 1994 08:49:60 GMT", None),
            ("Sun, 06 Nov 1994 24:00:00 GMT", None),
            ("Sun, 06 Nov 1994 08:9:37 GMT", None),
            ("Sun, 06 Nov 1994 08:49:37 UTC", None),
            ("Sun, 06 Nov 1994 08:49:37 GMT ", None),
            ("Xyz, 06 Nov 1994 08:49:37 GMT", None),
            ("Sunday, 06-Nov-94 08:49:37 pst", None),
            ("yesterday", None),
            ("", None),
        ] {
            assert_eq!(parse_time(date), want, "{date:?}");
        }
    }

    /// Credentials as Go's client sends them: percent-decoded, a missing password empty,
    /// the last `@` ending them; refused as net/url refuses them.
    /// pathutil.SafeFileName's names (its tests, and the cases between).
    #[test]
    fn file_names_are_made_safe_as_buildkit_makes_them() {
        for (name, want) in [
            (&b"file.txt"[..], &b"file.txt"[..]),
            (b"  file.txt \t", b"file.txt"),
            (b"a/b/c", b"c"),
            (b"a/b/", b"b"),
            (b"", b"download"),
            (b"   ", b"download"),
            (b".", b"download"),
            (b"..", b"download"),
            (b"a\x00b", b"download"),
            (b"a\nb", b"download"),
            (b"a\xc2\x85", b"a"),
            (b"a\xc2\x9fb", b"download"),
            (b"\xffa", b"\xffa"),
            (b"caf\xc3\xa9", b"caf\xc3\xa9"),
            (b"/", b"/"),
        ] {
            assert_eq!(safe_file_name(name), want, "{:?}", String::from_utf8_lossy(name));
        }
    }

    #[test]
    fn userinfo_becomes_basic_credentials() {
        assert_eq!(
            userinfo("http://us%65r:pa%40ss@h:1/x?q@r").unwrap(),
            (
                "http://h:1/x?q@r".to_string(),
                Some("Basic dXNlcjpwYUBzcw==".to_string())
            )
        );
        assert_eq!(
            userinfo("http://justuser@h/x").unwrap(),
            ("http://h/x".to_string(), Some("Basic anVzdHVzZXI6".to_string()))
        );
        assert_eq!(userinfo("https://u:p@ss@[::1]:8/").unwrap().0, "https://[::1]:8/");
        assert_eq!(
            userinfo("http://h/a@b").unwrap(),
            ("http://h/a@b".to_string(), None)
        );
        assert_eq!(
            userinfo("http://u:s%zz@h/").unwrap_err(),
            r#"parse "http://u:***@h/": invalid URL escape "%zz""#
        );
        assert_eq!(
            userinfo("http://u s:p@h/").unwrap_err(),
            r#"parse "http://u s:***@h/": net/url: invalid userinfo"#
        );
        assert_eq!(shown("https://u:secret@h/x"), "https://u:***@h/x");
        assert_eq!(shown("https://u@h/x"), "https://u@h/x");
    }
}

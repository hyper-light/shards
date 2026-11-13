//! #74 step 10 part B (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 33; D242): what the
//! credential injector costs, measured in-process as step 3's tests drive it (inject.rs), with
//! no VM. README.md beside this file says how to run it and what it reports.
//!
//! - `request`: a request carrying a credential's placeholder through the injector against
//!   the same request with no grant, which the network process carries as it came, on a new
//!   connection (`connect.*`) and on a kept-alive one (`request.*`), interleaved.
//! - `ca`: the per-VM CA, each sample a fresh process as a VM's network process is: its key,
//!   the CA, its upstream verifier, a leaf minted first and second, and a process's first
//!   connection through the injector against its second, beside a run with no grant's.
//! - `gzip`: a compressible body sent gzip-coded against identity-coded, as Claude Code sends
//!   it when `NODE_EXTRA_CA_CERTS` names the run's CA (finding f70), through the injector.
//!
//! The guest's connection is carried as tcp.rs carries it: the guest's TLS bytes given while
//! its window (`TO_HOST`, less what the injector holds) has room, the host's read while the
//! queue to the guest is under `TO_GUEST`, the host's socket with Nagle off as the network
//! process's (sys.rs). Nothing here sleeps: a turn that moves nothing blocks on the host's
//! socket, or yields while its socket is full.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::print_stdout
)]

use std::fmt::Write as _;
use std::io::{self, BufRead, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, ClientConnection, ServerConfig, ServerConnection};

use super::{Flow, Queues, Run, Said, Upstream};
use crate::ring::Ring;
use crate::tcp::{TO_GUEST, TO_HOST};
use crate::{Credentials, Grant, ca, lease, signin};

/// The granted host, its credential, the placeholder a guest holds (kiln's broker's shape:
/// `kiln-placeholder-` and 256 bits in hex) and the value filled in. Names, not tunings.
const GRANTED: &str = "api.example.test";
const REFERENCE: &str = "anthropic/claude-max";
const PLACEHOLDER: &str = "kiln-placeholder-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const VALUE: &[u8] = b"sk-ant-api03-not-a-real-value-0123456789";

/// The quantile whose stability ends sampling, and the confidence of its interval: the p99
/// is the tail the person's rule holds first-class, and 95% the conventional two-sided level
/// of a distribution-free quantile interval (Le Boudec, Performance Evaluation of Computer
/// and Communication Systems, 2010, §2.4.3 and Theorem 2.1).
const QUANTILE: f64 = 0.99;
const CONFIDENCE: f64 = 0.95;

/// The most a TLS record carries on the wire: 2^14 bytes of plaintext, 256 of expansion and
/// its 5-byte header (RFC 8446 §5.2), given to the guest's TLS a record at a time.
const RECORD: usize = (1 << 14) + 256 + 5;

/// The largest request the Messages API takes: 128 MB ("Request size limits",
/// https://platform.claude.com/docs/en/api/overview, read 2026-11-13). `gzip`'s default
/// sweep doubles from one TLS record's plaintext (2^14, RFC 8446 §5.1) to the largest power
/// of two within it.
const MESSAGES_MOST: usize = 128_000_000;
const SWEEP_LEAST: usize = 1 << 14;

/// The token a flow's lease resumes it by; one flow waits at a time.
const WAITER: lease::Waiter = 1;

/// The child process's line, and the test it runs.
const CHILD_LINE: &str = "S33AB-CHILD ";
const CHILD_TEST: &str = "inject::ab::child";

/// Do: measure what README.md lists, as `S33_AB_*` say. Expect: a header naming the host, its
/// OS, the revision and the load, then n, p50, p90, p99 and max per arm, each p99 with its 95%
/// interval, and the raw samples under `S33_AB_OUT` where it is set.
#[test]
#[ignore = "a measurement ROOT runs (README.md), not a test"]
fn injector_ab() {
    let config = Config::from_env();
    let mut report = Report::new(&config);
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for _ in 0..config.load {
            scope.spawn(|| {
                let mut x = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    x = std::hint::black_box(x.wrapping_add(1));
                }
            });
        }
        // Ends the load as a measurement panics too, so that the scope's join returns.
        let _stop = Stopping(&stop);
        if config.wants("request") {
            requests(&config, &mut report);
        }
        if config.wants("ca") {
            ca_costs(&config, &mut report);
        }
        if config.wants("gzip") {
            codings(&config, &mut report);
        }
    });
    report.finish();
}

struct Stopping<'a>(&'a AtomicBool);

impl Drop for Stopping<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// What the run is asked, from `S33_AB_*` (README.md).
struct Config {
    only: Option<Vec<String>>,
    fixed: Option<usize>,
    budget: Option<f64>,
    load: usize,
    load_said: String,
    sizes: Option<Vec<usize>>,
    body: Option<PathBuf>,
    level: flate2::Compression,
    out: Option<PathBuf>,
}

impl Config {
    fn from_env() -> Config {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let number =
            |name: &str| var(name).map(|v| v.parse::<usize>().unwrap_or_else(|e| panic!("{name}={v}: {e}")));
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let load_said = var("S33_AB_LOAD").unwrap_or_else(|| "0".into());
        let load = match load_said.as_str() {
            "cpus" => cpus,
            n => n.parse().unwrap_or_else(|e| panic!("S33_AB_LOAD={n}: {e}")),
        };
        Config {
            only: var("S33_AB_ONLY").map(|v| v.split(',').map(str::to_string).collect()),
            fixed: number("S33_AB_N").inspect(|&n| assert!(n > 0, "S33_AB_N=0")),
            budget: var("S33_AB_BUDGET_S")
                .map(|v| v.parse().unwrap_or_else(|e| panic!("S33_AB_BUDGET_S={v}: {e}"))),
            load,
            load_said,
            sizes: var("S33_AB_SIZES").map(|v| v.split(',').map(|s| s.trim().parse().unwrap()).collect()),
            body: var("S33_AB_BODY").map(PathBuf::from),
            level: number("S33_AB_GZIP_LEVEL").map_or_else(flate2::Compression::default, |l| {
                flate2::Compression::new(u32::try_from(l).unwrap())
            }),
            out: var("S33_AB_OUT").map(PathBuf::from),
        }
    }

    fn wants(&self, measure: &str) -> bool {
        self.only.as_ref().is_none_or(|o| o.iter().any(|m| m == measure))
    }
}

// --- What the host and run are ---

fn command(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn revision() -> String {
    let dir = env!("CARGO_MANIFEST_DIR");
    let Some(head) = command("git", &["-C", dir, "rev-parse", "HEAD"]) else {
        return "unknown (no git)".into();
    };
    match command(
        "git",
        &["-C", dir, "status", "--porcelain", "--untracked-files=no"],
    ) {
        Some(s) if s.is_empty() => head,
        Some(_) => format!("{head} with uncommitted changes"),
        None => format!("{head} (status unread)"),
    }
}

fn os() -> String {
    let base = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    #[cfg(target_os = "macos")]
    let more = command("sw_vers", &["-productVersion"])
        .map(|v| format!("macOS {v}, {}", command("uname", &["-r"]).unwrap_or_default()));
    #[cfg(all(unix, not(target_os = "macos")))]
    let more = command("uname", &["-sr"]);
    #[cfg(windows)]
    let more = command("cmd", &["/C", "ver"]);
    match more {
        Some(m) => format!("{base} ({m})"),
        None => base,
    }
}

/// The host's 1, 5 and 15 minute load averages, where its OS keeps them.
fn loadavg() -> String {
    #[cfg(unix)]
    {
        let mut avg = [0f64; 3];
        // SAFETY: `avg` holds the three values asked for.
        let n = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
        if n == 3 {
            return format!("{:.2} {:.2} {:.2}", avg[0], avg[1], avg[2]);
        }
        "unread".into()
    }
    #[cfg(not(unix))]
    {
        "none kept by this OS".into()
    }
}

// --- Statistics ---

/// The nearest-rank `q` quantile of sorted `x`: its ⌈qn⌉th value.
fn quantile(x: &[u64], q: f64) -> u64 {
    let rank = ((q * x.len() as f64).ceil() as usize).clamp(1, x.len());
    x[rank - 1]
}

/// The distribution-free interval of the `q` quantile of `n` samples at `CONFIDENCE`, as the
/// 1-based order statistics (j, k) with P(X(j) <= ξq < X(k)) >= CONFIDENCE, each tail held to
/// half the rest (Le Boudec 2010, Theorem 2.1); none where n is too few for one.
fn interval(n: usize, q: f64) -> Option<(usize, usize)> {
    let tail = (1.0 - CONFIDENCE) / 2.0;
    let (lq, lp) = (q.ln(), (1.0 - q).ln());
    // cdf[i] = P(Binomial(n, q) <= i), summed in log space so that no term overflows.
    let (mut ln_choose, mut sum) = (0f64, 0f64);
    let mut cdf = Vec::with_capacity(n + 1);
    for i in 0..=n {
        if i > 0 {
            ln_choose += ((n - i + 1) as f64).ln() - (i as f64).ln();
        }
        sum += (ln_choose + i as f64 * lq + (n - i) as f64 * lp).exp();
        cdf.push(sum);
    }
    let j = (1..=n).filter(|&j| cdf[j - 1] <= tail).max()?;
    let k = (1..=n).find(|&k| cdf[k - 1] >= 1.0 - tail)?;
    Some((j, k))
}

/// The fewest samples whose p99 has an interval at all (368 at 95%).
fn least_n() -> usize {
    (1..).find(|&n| interval(n, QUANTILE).is_some()).unwrap()
}

/// Why sampling stopped (R04).
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// Each series' p99 fell within its interval of the round before, n doubled since.
    Stable { rounds: usize },
    /// `S33_AB_N` samples were asked.
    Fixed,
    /// `S33_AB_BUDGET_S` ran out first: the p99s were still moving.
    Budget { rounds: usize },
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stop::Stable { rounds } => write!(f, "stable after {rounds} rounds"),
            Stop::Fixed => f.write_str("at S33_AB_N"),
            Stop::Budget { rounds } => write!(
                f,
                "by S33_AB_BUDGET_S after {rounds} rounds, its p99s still moving"
            ),
        }
    }
}

/// Samples `series` series by `pair` (its index, then where each series' samples go) until
/// each one's p99 is stable: from the fewest samples a p99's interval needs, n doubles until
/// every series' p99 lies within the interval its previous round gave (or `S33_AB_N`, or the
/// budget, ends it).
fn sample(
    config: &Config,
    series: usize,
    mut pair: impl FnMut(usize, &mut [Vec<u64>]),
) -> (Vec<Vec<u64>>, Stop) {
    let mut samples = vec![Vec::new(); series];
    let started = Instant::now();
    let mut target = config.fixed.unwrap_or_else(least_n);
    let mut previous: Option<Vec<(u64, u64)>> = None;
    let (mut i, mut rounds) = (0, 0);
    loop {
        while samples[0].len() < target {
            pair(i, &mut samples);
            i += 1;
            if config.budget.is_some_and(|b| started.elapsed().as_secs_f64() > b) {
                return (samples, Stop::Budget { rounds });
            }
        }
        rounds += 1;
        if config.fixed.is_some() {
            return (samples, Stop::Fixed);
        }
        let now: Vec<(u64, u64, u64)> = samples
            .iter()
            .map(|s| {
                let mut s = s.clone();
                s.sort_unstable();
                let (j, k) = interval(s.len(), QUANTILE).unwrap();
                (quantile(&s, QUANTILE), s[j - 1], s[k - 1])
            })
            .collect();
        if let Some(previous) = &previous
            && now
                .iter()
                .zip(previous)
                .all(|(&(p99, _, _), &(lo, hi))| lo <= p99 && p99 <= hi)
        {
            return (samples, Stop::Stable { rounds });
        }
        previous = Some(now.iter().map(|&(_, lo, hi)| (lo, hi)).collect());
        target *= 2;
    }
}

// --- The report ---

struct Report {
    text: String,
    out: Option<PathBuf>,
}

impl Report {
    fn new(config: &Config) -> Report {
        let mut text = String::new();
        let host = command("hostname", &[]).unwrap_or_else(|| "unknown".into());
        let _ = writeln!(text, "s33-injector-ab (#74 step 10 part B, D242)");
        let _ = writeln!(text, "host {host}; OS {}; revision {}", os(), revision());
        let _ = writeln!(
            text,
            "cpus {}; load threads {} (S33_AB_LOAD={}); started at Unix time {}",
            std::thread::available_parallelism().map_or(1, usize::from),
            config.load,
            config.load_said,
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs())
        );
        if cfg!(debug_assertions) {
            let _ = writeln!(
                text,
                "NOT A MEASUREMENT: a build with debug assertions (run it with --release)"
            );
        }
        let _ = writeln!(
            text,
            "n: {} (S33_AB_N) else from {} doubling until each p99 is stable (README.md); times in µs",
            config.fixed.map_or("none".into(), |n| n.to_string()),
            least_n()
        );
        if let Some(out) = &config.out {
            std::fs::create_dir_all(out).unwrap();
        }
        print!("{text}");
        Report {
            text,
            out: config.out.clone(),
        }
    }

    fn line(&mut self, line: String) {
        println!("{line}");
        self.text.push_str(&line);
        self.text.push('\n');
    }

    /// One measure's series: n, p50, p90, p99, max and the p99's interval each, in µs where
    /// they are times and in bytes where they are sizes (a name ending `.wire` or `.bytes`).
    /// `key` names its raw files.
    fn series(
        &mut self,
        measure: &str,
        key: &str,
        names: &[String],
        samples: &[Vec<u64>],
        stop: Stop,
        load: (String, String),
    ) {
        self.line(format!(
            "{measure}: stopped {stop}; load average {} at its start, {} at its end",
            load.0, load.1
        ));
        for (name, s) in names.iter().zip(samples) {
            let mut sorted = s.clone();
            sorted.sort_unstable();
            let bytes = name.ends_with(".wire") || name.ends_with(".bytes");
            let show = |v: u64| {
                if bytes {
                    v.to_string()
                } else {
                    format!("{:.1}", v as f64 / 1e3)
                }
            };
            let ci = interval(sorted.len(), QUANTILE).map_or("none (too few)".into(), |(j, k)| {
                format!("[{}, {}]", show(sorted[j - 1]), show(sorted[k - 1]))
            });
            self.line(format!(
                "  {name:<28} n {:>7}  p50 {:>12}  p90 {:>12}  p99 {:>12}  max {:>12}  p99 95% {ci}",
                sorted.len(),
                show(quantile(&sorted, 0.50)),
                show(quantile(&sorted, 0.90)),
                show(quantile(&sorted, QUANTILE)),
                show(sorted.last().copied().unwrap_or(0)),
            ));
            if let Some(out) = &self.out {
                let raw: String = s.iter().map(|v| format!("{v}\n")).collect();
                std::fs::write(
                    out.join(format!("{key}.{name}.{}", if bytes { "bytes" } else { "ns" })),
                    raw,
                )
                .unwrap();
            }
        }
    }

    fn finish(self) {
        if let Some(out) = &self.out {
            std::fs::write(out.join("summary.txt"), &self.text).unwrap();
        }
    }
}

fn nanos(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_nanos()).unwrap()
}

// --- The host, the guest and the run ---

/// A test root, which the host's certificate chains to and the injector's upstream trusts
/// beside the host's own roots, as step 3's tests give it; a day's validity either side of now.
fn root() -> CertifiedIssuer<'static, KeyPair> {
    let mut p = params(&[]);
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    p.distinguished_name
        .push(DnType::CommonName, "s33-injector-ab root");
    CertifiedIssuer::self_signed(p, KeyPair::generate().unwrap()).unwrap()
}

fn params(names: &[&str]) -> CertificateParams {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let day = 86_400;
    let mut p = CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap();
    p.not_before = time::OffsetDateTime::from_unix_timestamp(now - day).unwrap();
    p.not_after = time::OffsetDateTime::from_unix_timestamp(now + day).unwrap();
    p
}

/// The host's TLS: rustls's defaults (tickets sent, as a server keeping sessions does), a
/// leaf for the granted host.
fn host_config(root: &CertifiedIssuer<'static, KeyPair>) -> Arc<ServerConfig> {
    let mut p = params(&[GRANTED]);
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let key = KeyPair::generate().unwrap();
    let cert = p.signed_by(&key, root).unwrap();
    Arc::new(
        ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .unwrap(),
    )
}

/// The host: an HTTPS server answering each request with what it had of it, `filled`,
/// `placeholder` or `none` by its head's credential, and how many bytes its body held, a gzip
/// body inflated first as an API's server inflates it. It closes where the request asks.
fn host(config: Arc<ServerConfig>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for tcp in listener.incoming() {
            let Ok(tcp) = tcp else { return };
            let config = config.clone();
            std::thread::spawn(move || answer(config, tcp));
        }
    });
    addr
}

fn answer(config: Arc<ServerConfig>, tcp: TcpStream) {
    let _ = tcp.set_nodelay(true);
    let Ok(conn) = ServerConnection::new(config) else {
        return;
    };
    let mut tls = io::BufReader::new(rustls::StreamOwned::new(conn, tcp));
    loop {
        let mut head = Vec::new();
        loop {
            let start = head.len();
            match tls.read_until(b'\n', &mut head) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if head[start..] == b"\r\n"[..] {
                break;
            }
        }
        let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
        let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(name)).map(str::trim);
        let len: u64 = field("content-length:").map_or(0, |v| v.parse().unwrap());
        let gzip = field("content-encoding:") == Some("gzip");
        let close = field("connection:") == Some("close");
        let had = if holds(&head, VALUE) {
            "filled"
        } else if holds(&head, PLACEHOLDER.as_bytes()) {
            "placeholder"
        } else {
            "none"
        };
        let mut body = (&mut tls).take(len);
        let read = if gzip {
            io::copy(&mut flate2::read::GzDecoder::new(&mut body), &mut io::sink())
        } else {
            io::copy(&mut body, &mut io::sink())
        };
        let Ok(read) = read else { return };
        if io::copy(&mut body, &mut io::sink()).is_err() {
            return;
        }
        let said = format!("{had} {read}");
        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{said}", said.len());
        let tls = tls.get_mut();
        if tls
            .write_all(response.as_bytes())
            .and_then(|()| tls.flush())
            .is_err()
        {
            return;
        }
        if close {
            tls.conn.send_close_notify();
            let _ = tls.flush();
            return;
        }
    }
}

fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A guest's TLS client trusting `root` alone: rustls's defaults, its sessions resumed where
/// its server allows (the host does; the injector, by design, does not: R05).
fn guest_config(root: CertificateDer<'static>) -> Arc<ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(root).unwrap();
    Arc::new(
        ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// A run's credentials as its network process holds them: the one grant toward the granted
/// host, its CA, its leases, and `upstream`, its TLS to the host.
fn credentials(ca: ca::Ca, upstream: Upstream) -> Credentials {
    Credentials {
        grants: vec![Grant {
            reference: REFERENCE.into(),
            toward: vec![GRANTED.into()],
            placeholder: PLACEHOLDER.into(),
        }],
        ca,
        leases: lease::Leases::new(vec![REFERENCE.into()]),
        upstream,
        sign_ins: signin::SignIns::new(Vec::new()),
    }
}

/// A run's CA, for the granted host.
fn run_ca() -> ca::Ca {
    ca::Ca::new(ca::Key::generate().unwrap(), &[GRANTED]).unwrap()
}

/// Answers each lease asked with the value, held for the whole run (a life of the most a lease
/// may say): the lease's round trip is the run host's, not the injector's. Whether any was.
fn lease_all(credentials: &mut Credentials) -> bool {
    let asks = credentials.leases.asks();
    for ask in &asks {
        let (id, reference) = lease::decode_ask(ask).unwrap();
        assert_eq!(reference, REFERENCE);
        let answer = lease::encode_answer(id, Ok((NonZeroU64::MAX, VALUE))).unwrap();
        assert!(credentials.leases.answered(&answer, Instant::now()));
    }
    if asks.is_empty() {
        return false;
    }
    let _ = credentials.leases.resumed();
    true
}

/// An HTTP/1.1 request to the granted host with the credential's placeholder, `body` its
/// body, gzip-coded where said, closing its connection where said.
fn request(body: &[u8], gzip: bool, close: bool) -> Vec<u8> {
    let mut r = format!(
        "POST /v1/messages HTTP/1.1\r\nHost: {GRANTED}\r\nx-api-key: {PLACEHOLDER}\r\ncontent-type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    )
    .into_bytes();
    if gzip {
        r.extend_from_slice(b"Content-Encoding: gzip\r\n");
    }
    if close {
        r.extend_from_slice(b"Connection: close\r\n");
    }
    r.extend_from_slice(b"\r\n");
    r.extend_from_slice(body);
    r
}

/// The small request of `request` and `ca`: a Messages API call's shape.
const MESSAGE: &[u8] =
    br#"{"model":"claude","max_tokens":1024,"messages":[{"role":"user","content":"hello"}]}"#;

/// A Messages API body of `size` bytes (at least its frame's), its text this crate's own
/// sources: text of the kind a coding agent sends, each repeat further apart than deflate's
/// 32 KiB window (RFC 1951 §2), so that gzip finds no more in it than in the text itself.
fn messages_body(corpus: &str, size: usize) -> Vec<u8> {
    let (open, close) = (
        &br#"{"model":"claude","max_tokens":1024,"stream":true,"messages":[{"role":"user","content":""#[..],
        &br#""}]}"#[..],
    );
    let mut body = open.to_vec();
    let want = size.saturating_sub(close.len());
    let mut buf = [0u8; 4];
    'fill: loop {
        for c in corpus.chars() {
            let escaped: &[u8] = match c {
                '"' => b"\\\"",
                '\\' => b"\\\\",
                '\n' => b"\\n",
                '\t' => b"\\t",
                c if (c as u32) < 0x20 => b" ",
                c => c.encode_utf8(&mut buf).as_bytes(),
            };
            if body.len() + escaped.len() > want {
                break 'fill;
            }
            body.extend_from_slice(escaped);
        }
    }
    body.extend_from_slice(close);
    body
}

fn corpus() -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap())
        .collect()
}

fn gzip(body: &[u8], level: flate2::Compression) -> Vec<u8> {
    let mut z = flate2::write::GzEncoder::new(Vec::new(), level);
    z.write_all(body).unwrap();
    z.finish().unwrap()
}

/// The response the host gave, where `got` holds it whole: its body.
fn answered(got: &[u8]) -> Option<&[u8]> {
    let end = got.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map_or(0, |v| v.trim().parse().unwrap());
    let body = got.get(end + 4..end + 4 + len)?;
    assert!(head.starts_with("http/1.1 200 "), "{head}");
    Some(body)
}

/// Asserts that the host had what the arm sent: the value filled in through the injector,
/// the placeholder as it came without, and the body whole.
fn check(got: &[u8], injected: bool, len: usize) {
    let had = if injected { "filled" } else { "placeholder" };
    let body = answered(got).unwrap();
    assert_eq!(String::from_utf8_lossy(body), format!("{had} {len}"));
    assert!(!holds(got, VALUE), "the value reached the guest");
}

/// What every turn reuses: the guest's TLS bytes and the host's, as read.
struct Scratch {
    out: Vec<u8>,
    read: Vec<u8>,
}

impl Scratch {
    fn new() -> Scratch {
        Scratch {
            out: Vec::new(),
            read: vec![0u8; TO_GUEST],
        }
    }
}

/// At most so many bytes, written.
struct Limited<'a>(&'a mut Vec<u8>, usize);

impl Write for Limited<'_> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = b.len().min(self.1.saturating_sub(self.0.len()));
        if n == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.0.extend_from_slice(&b[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A guest's connection to the host, carried as tcp.rs carries it: through the injector where
/// the run grants the host, as it came where it has no grant.
struct Conn {
    guest: ClientConnection,
    flow: Option<Flow>,
    to_host: Ring,
    to_guest: Ring,
    sock: TcpStream,
    host_ended: bool,
    got: Vec<u8>,
    /// The bytes the host's socket took: what the requests cost on the wire.
    wire: u64,
}

impl Conn {
    fn open(config: &Arc<ClientConfig>, to: SocketAddr, injected: bool) -> Conn {
        let mut guest =
            ClientConnection::new(config.clone(), ServerName::try_from(GRANTED).unwrap()).unwrap();
        guest.set_buffer_limit(None);
        let sock = TcpStream::connect(to).unwrap();
        sock.set_nodelay(true).unwrap();
        sock.set_nonblocking(true).unwrap();
        Conn {
            guest,
            flow: injected.then(Flow::new),
            to_host: Ring::new(),
            to_guest: Ring::new(),
            sock,
            host_ended: false,
            got: Vec::new(),
            wire: 0,
        }
    }

    /// The flow's step with the run, as tcp.rs takes it, its audit formatted as the network
    /// process formats it for the run host.
    fn step(
        &mut self,
        credentials: Option<&mut Credentials>,
        f: impl FnOnce(&mut Flow, &mut Run<'_>, &mut Queues<'_>) -> Said,
    ) {
        let Some(flow) = self.flow.as_mut() else { return };
        let credentials = credentials.expect("an injected flow's run has credentials");
        let mut run = Run {
            credentials,
            waiter: WAITER,
            now: Instant::now(),
            audit: Vec::new(),
            refused: Vec::new(),
        };
        let said = f(
            flow,
            &mut run,
            &mut Queues {
                to_host: &mut self.to_host,
                to_guest: &mut self.to_guest,
            },
        );
        for record in &run.audit {
            let _ = writeln!(io::sink(), "{record}");
        }
        match said {
            Said::Going | Said::Close => {}
            Said::Pass(_) => panic!("the granted host's flow was passed"),
            Said::Reset => panic!("the injector reset the flow"),
        }
    }

    /// One turn of each side; whether anything moved.
    fn turn(&mut self, mut credentials: Option<&mut Credentials>, s: &mut Scratch) -> bool {
        let mut moved = false;
        let held = self.flow.as_ref().map_or(0, Flow::held);
        let room = TO_HOST.saturating_sub(self.to_host.len() + held);
        s.out.clear();
        while self.guest.wants_write() && s.out.len() < room {
            if !matches!(self.guest.write_tls(&mut Limited(&mut s.out, room)), Ok(n) if n > 0) {
                break;
            }
        }
        if !s.out.is_empty() {
            moved = true;
            let bytes = std::mem::take(&mut s.out);
            if self.flow.is_some() {
                self.step(credentials.as_deref_mut(), |f, run, q| {
                    f.from_guest(&bytes, run, q)
                });
            } else {
                self.to_host.extend(&bytes).unwrap();
            }
            s.out = bytes;
        }
        if !self.to_host.is_empty() {
            let (a, _) = self.to_host.as_slices();
            match (&self.sock).write(a) {
                Ok(n) if n > 0 => {
                    self.to_host.drain(n);
                    self.wire += n as u64;
                    moved = true;
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("the host's socket: {e}"),
            }
        }
        if !self.host_ended && self.to_guest.len() < TO_GUEST {
            let room = TO_GUEST - self.to_guest.len();
            match (&self.sock).read(&mut s.read[..room]) {
                Ok(0) => {
                    self.host_ended = true;
                    moved = true;
                    self.step(credentials, |f, run, q| f.host_ended(run, q));
                }
                Ok(n) => {
                    moved = true;
                    let got = &s.read[..n];
                    if self.flow.is_some() {
                        self.step(credentials, |f, run, q| f.from_host(got, run, q));
                    } else {
                        self.to_guest.extend(got).unwrap();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("the host's socket: {e}"),
            }
        }
        while !self.to_guest.is_empty() {
            let (a, _) = self.to_guest.as_slices();
            let piece = &a[..a.len().min(RECORD)];
            let n = self.guest.read_tls(&mut &piece[..]).unwrap();
            self.to_guest.drain(n);
            self.guest
                .process_new_packets()
                .unwrap_or_else(|e| panic!("the guest's TLS: {e}"));
            let _ = self.guest.reader().read_to_end(&mut self.got);
            moved = true;
            if n == 0 {
                break;
            }
        }
        moved
    }

    /// Nothing moved: the host is what comes next, so wait on its socket (a yield while the
    /// socket is full, as a write's readiness has no portable wait in std).
    fn wait(&mut self) {
        assert!(!self.host_ended, "the host ended its side before answering");
        if !self.to_host.is_empty() {
            std::thread::yield_now();
            return;
        }
        self.sock.set_nonblocking(false).unwrap();
        let peeked = self.sock.peek(&mut [0u8; 1]);
        self.sock.set_nonblocking(true).unwrap();
        peeked.unwrap();
    }

    /// Sends `request` and turns until its whole response is in `got`: `credentials` the
    /// run's where it has a grant, none where it has none.
    fn exchange(&mut self, request: &[u8], mut credentials: Option<&mut Credentials>, s: &mut Scratch) {
        self.got.clear();
        self.guest.writer().write_all(request).unwrap();
        while answered(&self.got).is_none() {
            let moved = self.turn(credentials.as_deref_mut(), s);
            if credentials.as_deref_mut().is_some_and(lease_all) {
                self.step(credentials.as_deref_mut(), |f, run, q| f.resume(run, q));
            } else if !moved && answered(&self.got).is_none() {
                self.wait();
            }
        }
    }
}

/// The two arms' order in pair `i`, alternated so that neither always goes first.
fn order(i: usize) -> [usize; 2] {
    if i.is_multiple_of(2) { [0, 1] } else { [1, 0] }
}

// --- 1: the injector against direct egress ---

fn requests(config: &Config, report: &mut Report) {
    let root = root();
    let addr = host(host_config(&root));
    let upstream = Upstream::new(&shards_verify::Platform::read(), vec![root.der().clone()]).unwrap();
    let mut credentials = credentials(run_ca(), upstream);
    let configs = [
        guest_config(credentials.ca.der().to_vec().into()),
        guest_config(root.der().clone()),
    ];
    let mut s = Scratch::new();
    let (keep, close) = (request(MESSAGE, false, false), request(MESSAGE, false, true));
    // Each arm's first exchange of the process is `ca`'s to measure: not recorded here, as is
    // each kept connection's first, which asks the lease.
    for (arm, config) in configs.iter().enumerate() {
        let mut c = Conn::open(config, addr, arm == 0);
        c.exchange(&close, (arm == 0).then_some(&mut credentials), &mut s);
        check(&c.got, arm == 0, MESSAGE.len());
    }
    let mut kept = [
        Conn::open(&configs[0], addr, true),
        Conn::open(&configs[1], addr, false),
    ];
    for (arm, c) in kept.iter_mut().enumerate() {
        c.exchange(&keep, (arm == 0).then_some(&mut credentials), &mut s);
        check(&c.got, arm == 0, MESSAGE.len());
    }
    let names: Vec<String> = [
        "connect.inject",
        "connect.direct",
        "request.inject",
        "request.direct",
    ]
    .map(String::from)
    .into();
    let load = loadavg();
    let (samples, stop) = sample(config, names.len(), |i, series| {
        for arm in order(i) {
            let t = Instant::now();
            let mut c = Conn::open(&configs[arm], addr, arm == 0);
            c.exchange(&close, (arm == 0).then_some(&mut credentials), &mut s);
            series[arm].push(nanos(t));
            check(&c.got, arm == 0, MESSAGE.len());
        }
        for arm in order(i) {
            let t = Instant::now();
            kept[arm].exchange(&keep, (arm == 0).then_some(&mut credentials), &mut s);
            series[2 + arm].push(nanos(t));
            check(&kept[arm].got, arm == 0, MESSAGE.len());
        }
    });
    report.series("request", "request", &names, &samples, stop, (load, loadavg()));
}

// --- 2: the per-VM CA, each sample a fresh process ---

/// The child processes' kinds, and the series each reports, in its line's order.
const KINDS: [(&str, &[&str]); 3] = [
    ("leaf", &["key", "ca", "first", "second"]),
    ("inject", &["key", "ca", "upstream", "first", "second"]),
    ("direct", &["first", "second"]),
];

fn ca_costs(config: &Config, report: &mut Report) {
    let root = root();
    let addr = host(host_config(&root));
    let root_hex: String = root.der().iter().map(|b| format!("{b:02x}")).collect();
    let names: Vec<String> = KINDS
        .iter()
        .flat_map(|(kind, series)| series.iter().map(move |s| format!("{kind}.{s}")))
        .collect();
    let load = loadavg();
    let (samples, stop) = sample(config, names.len(), |i, series| {
        for k in 0..KINDS.len() {
            let (kind, _) = KINDS[(i + k) % KINDS.len()];
            let out = Command::new(std::env::current_exe().unwrap())
                .args([
                    CHILD_TEST,
                    "--exact",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("S33_AB_CHILD", kind)
                .env("S33_AB_ADDR", addr.to_string())
                .env("S33_AB_ROOT", &root_hex)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success(),
                "the {kind} child failed: {text}{}",
                String::from_utf8_lossy(&out.stderr)
            );
            // libtest writes the test's name on the line the child's own begins.
            let line = text
                .lines()
                .find_map(|l| l.split_once(CHILD_LINE).map(|(_, said)| said))
                .unwrap_or_else(|| panic!("the {kind} child said nothing: {text}"));
            for pair in line.split_whitespace() {
                let (name, ns) = pair.split_once('=').unwrap();
                let at = names.iter().position(|n| *n == format!("{kind}.{name}")).unwrap();
                series[at].push(ns.parse().unwrap());
            }
        }
    });
    report.series("ca", "ca", &names, &samples, stop, (load, loadavg()));
}

/// A child's measurement: a VM's network process's first moments, in a fresh process. Its
/// CA's key comes first, as `Prepared::new` makes it before anything else draws randomness.
#[test]
#[ignore = "run by injector_ab, in a process of its own"]
fn child() {
    let Ok(kind) = std::env::var("S33_AB_CHILD") else {
        return;
    };
    let addr: SocketAddr = std::env::var("S33_AB_ADDR").unwrap().parse().unwrap();
    let hex = std::env::var("S33_AB_ROOT").unwrap();
    let der: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap())
        .collect();
    let root = CertificateDer::from(der);
    let mut said = String::new();
    let mut s = Scratch::new();
    let close = request(MESSAGE, false, true);
    match kind.as_str() {
        "leaf" => {
            let mut key = None;
            timed("key", &mut said, &mut || key = Some(ca::Key::generate().unwrap()));
            let mut ca = None;
            timed("ca", &mut said, &mut || {
                ca = Some(ca::Ca::new(key.take().unwrap(), &[GRANTED]).unwrap())
            });
            let ca = ca.unwrap();
            timed("first", &mut said, &mut || drop(ca.leaf(GRANTED).unwrap()));
            timed("second", &mut said, &mut || drop(ca.leaf(GRANTED).unwrap()));
        }
        "inject" => {
            let mut key = None;
            timed("key", &mut said, &mut || key = Some(ca::Key::generate().unwrap()));
            let mut made = None;
            timed("ca", &mut said, &mut || {
                made = Some(ca::Ca::new(key.take().unwrap(), &[GRANTED]).unwrap())
            });
            let mut upstream = None;
            timed("upstream", &mut said, &mut || {
                upstream = Some(Upstream::new(&shards_verify::Platform::read(), vec![root.clone()]).unwrap())
            });
            let mut credentials = credentials(made.unwrap(), upstream.unwrap());
            let config = guest_config(credentials.ca.der().to_vec().into());
            for name in ["first", "second"] {
                let mut c = None;
                timed(name, &mut said, &mut || {
                    let mut conn = Conn::open(&config, addr, true);
                    conn.exchange(&close, Some(&mut credentials), &mut s);
                    c = Some(conn);
                });
                check(&c.unwrap().got, true, MESSAGE.len());
            }
        }
        "direct" => {
            // A run with no grant makes no key and has no credentials.
            let config = guest_config(root.clone());
            for name in ["first", "second"] {
                let mut c = None;
                timed(name, &mut said, &mut || {
                    let mut conn = Conn::open(&config, addr, false);
                    conn.exchange(&close, None, &mut s);
                    c = Some(conn);
                });
                check(&c.unwrap().got, false, MESSAGE.len());
            }
        }
        other => panic!("S33_AB_CHILD={other}"),
    }
    println!("{CHILD_LINE}{}", said.trim_end());
}

/// Runs `f`, adding its time to `said` as `name=ns`.
fn timed(name: &str, said: &mut String, f: &mut dyn FnMut()) {
    let t = Instant::now();
    f();
    let _ = write!(said, "{name}={} ", nanos(t));
}

// --- 3: gzip against identity coding (f70) ---

fn codings(config: &Config, report: &mut Report) {
    let root = root();
    let addr = host(host_config(&root));
    let upstream = Upstream::new(&shards_verify::Platform::read(), vec![root.der().clone()]).unwrap();
    let mut credentials = credentials(run_ca(), upstream);
    let guest = guest_config(credentials.ca.der().to_vec().into());
    let mut s = Scratch::new();
    let bodies: Vec<(String, Vec<u8>)> = match &config.body {
        Some(file) => vec![("file".into(), std::fs::read(file).unwrap())],
        None => {
            let corpus = corpus();
            let sizes = config.sizes.clone().unwrap_or_else(|| {
                std::iter::successors(Some(SWEEP_LEAST), |s| s.checked_mul(2))
                    .take_while(|&s| s <= MESSAGES_MOST)
                    .collect()
            });
            sizes
                .iter()
                .map(|&n| (n.to_string(), messages_body(&corpus, n)))
                .collect()
        }
    };
    report.line(format!(
        "gzip: level {} (S33_AB_GZIP_LEVEL; zlib's default 6 where unset)",
        config.level.level()
    ));
    for (label, body) in &bodies {
        let mut kept = [Conn::open(&guest, addr, true), Conn::open(&guest, addr, true)];
        for c in &mut kept {
            c.exchange(&request(MESSAGE, false, false), Some(&mut credentials), &mut s);
        }
        let names: Vec<String> = ["gzip", "identity", "gzip.wire", "identity.wire"]
            .map(String::from)
            .into();
        let load = loadavg();
        let compressed = gzip(body, config.level).len();
        let (samples, stop) = sample(config, names.len(), |i, series| {
            for arm in order(i) {
                let before = kept[arm].wire;
                let t = Instant::now();
                let r = if arm == 0 {
                    request(&gzip(body, config.level), true, false)
                } else {
                    request(body, false, false)
                };
                kept[arm].exchange(&r, Some(&mut credentials), &mut s);
                series[arm].push(nanos(t));
                series[2 + arm].push(kept[arm].wire - before);
                check(&kept[arm].got, true, body.len());
            }
        });
        report.series(
            &format!(
                "gzip body {label} ({} bytes, {compressed} gzip-coded)",
                body.len()
            ),
            &format!("gzip-{label}"),
            &names,
            &samples,
            stop,
            (load, loadavg()),
        );
    }
}

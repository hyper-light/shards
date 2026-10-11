//! One instance of the in-VM server: the agent or harness it serves reaches it by a
//! socket only that domain sees, `/run/shards/server.sock`. Two layers say who calls:
//!
//! - that socket, which init made visible to its domain alone: an identity the kernel
//!   makes, which no agent can forge;
//! - mutual TLS 1.3 over it, with a CA this instance makes as it starts: no key is shared
//!   with another agent's instance, and init holds none. The agent's certificate and key
//!   lie beside the socket, the key readable by the agent's group alone, and a connection
//!   must present that certificate.
//!
//! Started by init as `shards-server LABEL DIR GID READY FILTER PEERS`, already in namespaces
//! and a uid of its own with no capability (init's `domains`): it makes its files in DIR,
//! listens, says so on descriptor READY, then confines itself with the seccomp filter it
//! reads from descriptor FILTER, before it reads anything an agent sends.
//!
//! It speaks MCP over streamable HTTP (modelcontextprotocol.io, "Transports"): JSON
//! answers to JSON-RPC requests posted to `/mcp`.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

#[path = "../../init/src/json.rs"]
#[allow(dead_code)]
mod json;
use json::Value;

/// The name its certificate is for, which a client asks for.
pub const NAME: &str = "shards";
/// The MCP revisions it answers to, newest first (modelcontextprotocol.io, "Versioning").
const REVISIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];
/// The most of a request it holds: its head, and its body.
const HEAD_MAX: usize = 16 * 1024;
const BODY_MAX: usize = 1 << 20;

/// An instance, made: its caller, the socket it listens on, and its TLS.
pub struct Prepared {
    /// `agent main`: what it says its caller is.
    label: String,
    listener: UnixListener,
    /// The certificate issued to its caller, which a connection must present.
    cert: CertificateDer<'static>,
    config: Arc<rustls::ServerConfig>,
    /// The MCP servers offered its caller (§12 answer 5): a JSON array.
    offered: String,
}

pub fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards-server: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[OsString]) -> Result<(), String> {
    let arg = |i: usize, what: &str| {
        args.get(i).and_then(|a| a.to_str()).ok_or(format!(
            "usage: shards-server LABEL DIR GID READY FILTER (no {what})"
        ))
    };
    let label = arg(0, "LABEL")?;
    let dir = arg(1, "DIR")?;
    let gid: u32 = arg(2, "GID")?.parse().map_err(|_| "GID is no number")?;
    let fd = |i: usize, what: &str| -> Result<OwnedFd, String> {
        let n: RawFd = arg(i, what)?
            .parse()
            .map_err(|_| format!("{what} is no descriptor"))?;
        // SAFETY: init gives this process the descriptor named, for it alone.
        Ok(unsafe { OwnedFd::from_raw_fd(n) })
    };
    let (ready, filter) = (fd(3, "READY")?, fd(4, "FILTER")?);
    let mut peers = channels(args.get(5).and_then(|a| a.to_str()).unwrap_or_default())?;
    let mut p = prepare(dir, label, gid)?;
    // The MCP servers offered its caller, as init found them: a JSON array.
    p.offered = args
        .get(6)
        .and_then(|a| a.to_str())
        .filter(|a| json::parse(a.as_bytes()).is_ok())
        .unwrap_or("[]")
        .to_string();
    let program = read_filter(filter)?;
    let mut ready = std::fs::File::from(ready);
    ready
        .write_all(b"r")
        .map_err(|e| format!("saying it listens: {e}"))?;
    drop(ready);
    confine(&program)?;
    serve(&p, &mut peers);
    Ok(())
}

/// A channel to another domain's instance, which init made for what the Agentfile grants
/// (D60): its label, whether this caller may send it requests and answers its, and what is
/// in flight. Each message is one sequenced packet: a kind (1 a request, 2 an answer), an
/// ID of the asker's, 8 bytes big-endian, and its text.
pub struct Peer {
    label: String,
    fd: OwnedFd,
    send: bool,
    answer: bool,
    /// What was read from it and not yet taken: one at a time, so that what it sends past
    /// that waits in the kernel's buffer, whose bound is the kernel's.
    held: Option<Message>,
    /// Requests sent it, awaiting its answers: an answer to anything else is dropped.
    sent: Vec<u64>,
    /// The last ID given a request sent it.
    last: u64,
    /// Its requests taken and not yet answered: an answer to anything else is refused.
    asked: Vec<u64>,
    gone: bool,
}

/// A message read from a peer.
struct Message {
    request: bool,
    id: u64,
    text: String,
}

const REQUEST: u8 = 1;
const ANSWER: u8 = 2;

/// The channels init gave it: one a line, `FD\tLABEL\tSEND\tANSWER`.
fn channels(spec: &str) -> Result<Vec<Peer>, String> {
    let mut out = Vec::new();
    for line in spec.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        let [fd, label, send, answer] = f.as_slice() else {
            return Err(format!("a channel it cannot read: {line:?}"));
        };
        let fd: RawFd = fd
            .parse()
            .map_err(|_| format!("a channel's descriptor: {fd:?}"))?;
        // SAFETY: init gives this process the descriptor named, for it alone.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        out.push(Peer {
            label: label.to_string(),
            fd,
            send: *send == "true",
            answer: *answer == "true",
            held: None,
            sent: Vec::new(),
            last: 0,
            asked: Vec::new(),
            gone: false,
        });
    }
    Ok(out)
}

/// Reads what `peer` sent, if nothing of its is held: a request only where this caller
/// answers its, an answer only to a request sent it; anything else is dropped, whatever
/// the other instance is made to send.
fn take(peer: &mut Peer) {
    if peer.held.is_some() || peer.gone {
        return;
    }
    let mut buf = vec![0u8; 9 + BODY_MAX];
    // SAFETY: recv(2) into a buffer it owns.
    let n = unsafe {
        libc::recv(
            peer.fd.as_raw_fd(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if n == 0 {
        peer.gone = true;
        return;
    }
    let Some(frame) = usize::try_from(n).ok().and_then(|n| buf.get(..n)) else {
        return;
    };
    let Some((&kind, rest)) = frame.split_first() else {
        return;
    };
    let Some((id, text)) = rest.split_first_chunk::<8>() else {
        return;
    };
    let id = u64::from_be_bytes(*id);
    let request = match kind {
        REQUEST if peer.answer => true,
        ANSWER if peer.sent.contains(&id) => {
            peer.sent.retain(|s| *s != id);
            false
        }
        _ => return,
    };
    peer.held = Some(Message {
        request,
        id,
        text: String::from_utf8_lossy(text).into_owned(),
    });
}

/// Sends `peer` a message of `kind`; false where its buffer is full.
fn put(peer: &Peer, kind: u8, id: u64, text: &str) -> bool {
    let frame = [&[kind][..], &id.to_be_bytes(), text.as_bytes()].concat();
    // SAFETY: send(2) of a buffer it owns, on a socket of its own.
    let n = unsafe {
        libc::send(
            peer.fd.as_raw_fd(),
            frame.as_ptr().cast(),
            frame.len(),
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    usize::try_from(n).is_ok_and(|n| n == frame.len())
}

/// The seccomp filter init passed: its flags, 4 bytes little-endian, then its
/// `struct sock_filter`s (include/uapi/linux/filter.h), as init's setup carries it.
fn read_filter(from: OwnedFd) -> Result<(u32, Vec<libc::sock_filter>), String> {
    let mut bytes = Vec::new();
    std::fs::File::from(from)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("its seccomp filter: {e}"))?;
    let (flags, bytes) = bytes
        .split_first_chunk::<4>()
        .ok_or("its seccomp filter: no flags")?;
    let (program, rest) = bytes.as_chunks::<FILTER_LEN>();
    if program.is_empty() || !rest.is_empty() {
        return Err("its seccomp filter: no whole program".to_string());
    }
    Ok((
        u32::from_le_bytes(*flags),
        program
            .iter()
            .map(|&[c0, c1, jt, jf, k0, k1, k2, k3]| libc::sock_filter {
                code: u16::from_ne_bytes([c0, c1]),
                jt,
                jf,
                k: u32::from_ne_bytes([k0, k1, k2, k3]),
            })
            .collect(),
    ))
}

/// A `struct sock_filter`'s size: a u16, two u8s and a u32.
const FILTER_LEN: usize = 8;
const _: () = assert!(std::mem::size_of::<libc::sock_filter>() == FILTER_LEN);

/// Loads `program` with `flags`; `no_new_privs` init set before the exec.
fn confine((flags, program): &(u32, Vec<libc::sock_filter>)) -> Result<(), String> {
    let prog = libc::sock_fprog {
        len: u16::try_from(program.len()).map_err(|_| "its seccomp filter is too long")?,
        filter: program.as_ptr().cast_mut(),
    };
    // SAFETY: seccomp(2) with a sock_fprog over a program that outlives the call.
    if unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            *flags,
            &raw const prog,
        )
    } != 0
    {
        return Err(format!("its seccomp filter: {}", io::Error::last_os_error()));
    }
    Ok(())
}

fn err(what: &str) -> impl Fn(rcgen::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Makes its CA, its own certificate, and its caller's certificate and key in `dir`
/// (`ca.pem`, `cert.pem`, `key.pem`, readable by group `gid`, its caller's, alone for the
/// key), and listens on `dir/server.sock`, which that group may connect to.
pub fn prepare(dir: &str, label: &str, gid: u32) -> Result<Prepared, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let new_key = || KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(err("a key"));
    let mut ca = CertificateParams::new(Vec::<String>::new()).map_err(err("its CA"))?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca.distinguished_name
        .push(DnType::CommonName, format!("shards server of {label}"));
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    let ca_key = new_key()?;
    let ca_cert = ca.self_signed(&ca_key).map_err(err("its CA"))?;
    let issuer = Issuer::new(ca, ca_key);
    let leaf = |names: Vec<String>, cn: &str, usage: ExtendedKeyUsagePurpose| {
        let mut p = CertificateParams::new(names).map_err(err("a certificate"))?;
        p.distinguished_name.push(DnType::CommonName, cn);
        p.extended_key_usages = vec![usage];
        let key = new_key()?;
        let cert = p.signed_by(&key, &issuer).map_err(err("a certificate"))?;
        Ok::<_, String>((cert, key))
    };
    let (server, server_key) = leaf(vec![NAME.to_string()], NAME, ExtendedKeyUsagePurpose::ServerAuth)?;
    let (cert, key) = leaf(Vec::new(), label, ExtendedKeyUsagePurpose::ClientAuth)?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(ca_cert.der().clone())
        .map_err(|e| format!("its CA: {e}"))?;
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|e| format!("its client verifier: {e}"))?;
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("TLS 1.3: {e}"))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![server.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
        )
        .map_err(|e| format!("its certificate: {e}"))?;
    let fail = |what: &str, e: io::Error| format!("{dir}/{what}: {e}");
    let write = |name: &str, text: &str, mode: u32| -> Result<(), String> {
        let path = format!("{dir}/{name}");
        std::fs::write(&path, text).map_err(|e| fail(name, e))?;
        std::os::unix::fs::chown(&path, None, Some(gid)).map_err(|e| fail(name, e))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).map_err(|e| fail(name, e))
    };
    write("ca.pem", &ca_cert.pem(), 0o444)?;
    write("cert.pem", &cert.pem(), 0o444)?;
    write("key.pem", &key.serialize_pem(), 0o440)?;
    let sock = format!("{dir}/server.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| fail("server.sock", e))?;
    std::os::unix::fs::chown(&sock, None, Some(gid)).map_err(|e| fail("server.sock", e))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o660))
        .map_err(|e| fail("server.sock", e))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| fail("server.sock", e))?;
    Ok(Prepared {
        label: label.to_string(),
        listener,
        cert: cert.der().clone(),
        config: Arc::new(config),
        offered: "[]".into(),
    })
}

/// A connection, and what it has sent.
struct Conn {
    stream: UnixStream,
    tls: rustls::ServerConnection,
    checked: bool,
    plain: Vec<u8>,
    closing: bool,
}

/// How many connections it holds at once: what this process may open
/// (`RLIMIT_NOFILE`), less its standard descriptors and its listener.
fn room() -> usize {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit(2) into a struct it owns.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) } != 0 {
        return 0;
    }
    usize::try_from(lim.rlim_cur)
        .unwrap_or(usize::MAX)
        .saturating_sub(4)
}

pub fn serve(p: &Prepared, peers: &mut [Peer]) {
    let room = room();
    let mut conns: Vec<Conn> = Vec::new();
    let mut set: Vec<libc::pollfd> = Vec::new();
    loop {
        set.clear();
        set.push(libc::pollfd {
            fd: p.listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for peer in peers.iter() {
            set.push(libc::pollfd {
                fd: peer.fd.as_raw_fd(),
                events: if peer.held.is_none() && !peer.gone {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            });
        }
        for c in &conns {
            let out = if c.tls.wants_write() { libc::POLLOUT } else { 0 };
            set.push(libc::pollfd {
                fd: c.stream.as_raw_fd(),
                events: libc::POLLIN | out,
                revents: 0,
            });
        }
        // SAFETY: poll(2) of pollfds of ours.
        if unsafe { libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, -1) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return;
        }
        while let Ok((stream, _)) = p.listener.accept() {
            if conns.len() >= room || stream.set_nonblocking(true).is_err() {
                continue;
            }
            if let Ok(tls) = rustls::ServerConnection::new(p.config.clone()) {
                conns.push(Conn {
                    stream,
                    tls,
                    checked: false,
                    plain: Vec::new(),
                    closing: false,
                });
            }
        }
        for peer in peers.iter_mut() {
            take(peer);
        }
        for c in &mut conns {
            step(p, peers, c);
        }
        conns.retain(|c| !(c.closing && !c.tls.wants_write()));
    }
}

/// Reads what a connection sent, answers each whole request, and writes what is due.
fn step(p: &Prepared, peers: &mut [Peer], c: &mut Conn) {
    loop {
        match c.tls.read_tls(&mut c.stream) {
            Ok(0) => {
                c.closing = true;
                break;
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => {
                c.closing = true;
                break;
            }
        }
        if c.tls.process_new_packets().is_err() {
            c.closing = true;
            break;
        }
    }
    // The certificate presented must be the one issued for this socket's caller.
    if !c.checked && !c.tls.is_handshaking() {
        let presented = c.tls.peer_certificates().and_then(<[_]>::first);
        if presented != Some(&p.cert) {
            c.closing = true;
            c.tls.send_close_notify();
        }
        c.checked = true;
    }
    if c.checked && !c.closing {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match c.tls.reader().read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => c.plain.extend_from_slice(buf.get(..n).unwrap_or_default()),
            }
            if c.plain.len() > HEAD_MAX + BODY_MAX {
                c.closing = true;
                break;
            }
        }
        while let Some((answer, used, close)) = request(p, peers, &c.plain) {
            c.plain.drain(..used.min(c.plain.len()));
            let _ = c.tls.writer().write_all(&answer);
            if close {
                c.closing = true;
                c.tls.send_close_notify();
                break;
            }
        }
    }
    while c.tls.wants_write() {
        match c.tls.write_tls(&mut c.stream) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => {
                c.closing = true;
                return;
            }
        }
    }
}

/// The first whole request in `plain`, answered: the answer, the bytes it took, and
/// whether the connection then closes. None until one is whole.
fn request(p: &Prepared, peers: &mut [Peer], plain: &[u8]) -> Option<(Vec<u8>, usize, bool)> {
    let end = plain.windows(4).position(|w| w == b"\r\n\r\n");
    let Some(end) = end else {
        return (plain.len() > HEAD_MAX).then(|| (status(431, "header too large"), plain.len(), true));
    };
    if end > HEAD_MAX {
        return Some((status(431, "header too large"), plain.len(), true));
    }
    let head = String::from_utf8_lossy(plain.get(..end)?);
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path) = (first.next()?, first.next()?);
    let mut length = 0usize;
    let mut close = false;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => match v.trim().parse() {
                Ok(n) => length = n,
                Err(_) => return Some((status(400, "bad content-length"), plain.len(), true)),
            },
            "connection" => close = v.trim().eq_ignore_ascii_case("close"),
            _ => {}
        }
    }
    if length > BODY_MAX {
        return Some((status(413, "body too large"), plain.len(), true));
    }
    let used = end + 4 + length;
    let body = plain.get(end + 4..used)?;
    if path != "/mcp" {
        return Some((status(404, "not found"), used, close));
    }
    if method != "POST" {
        return Some((status(405, "method not allowed"), used, close));
    }
    let answer = match json::parse(body) {
        Ok(msg) => match rpc(p, peers, &msg) {
            Some(result) => http(200, "application/json", result.as_bytes()),
            // A notification is accepted, and answered with nothing.
            None => http(202, "", b""),
        },
        Err(_) => http(
            400,
            "application/json",
            error(&Value::Null, -32700, "parse error").as_bytes(),
        ),
    };
    Some((answer, used, close))
}

/// A tool's result: its one text item, a JSON value, and whether it is an error the caller
/// may act on (MCP's `isError`).
fn text(value: &str, is_error: bool) -> String {
    format!(
        r#"{{"content":[{{"type":"text","text":{}}}],"isError":{is_error}}}"#,
        quote(value)
    )
}

/// What it offers: who the caller is, those it may message, and messaging them.
const TOOLS: &str = r#"{"tools":[
{"name":"whoami","description":"Who this server knows the caller as: the agent or harness whose socket it called on.","inputSchema":{"type":"object","properties":{}}},
{"name":"mcp","description":"The MCP servers offered the caller: a remote one by its URL, one spoken to over stdio by its directory and the command that runs it, which the caller runs itself, in its own confinement.","inputSchema":{"type":"object","properties":{}}},
{"name":"peers","description":"The agents and harnesses the caller may message, as its Agentfile grants: whether it may send each requests (send), and whether it answers theirs (answer).","inputSchema":{"type":"object","properties":{}}},
{"name":"send","description":"Sends a peer the caller may send requests a request; its ID, which the peer's answer carries.","inputSchema":{"type":"object","properties":{"to":{"type":"string"},"text":{"type":"string"}},"required":["to","text"]}},
{"name":"receive","description":"What peers have sent the caller and it has not taken: their requests, and their answers to its own.","inputSchema":{"type":"object","properties":{}}},
{"name":"answer","description":"Answers a request a peer sent the caller, by its ID.","inputSchema":{"type":"object","properties":{"to":{"type":"string"},"id":{"type":"integer"},"text":{"type":"string"}},"required":["to","id","text"]}}
]}"#;

/// A JSON-RPC message's answer; None for a notification.
fn rpc(p: &Prepared, peers: &mut [Peer], msg: &Value) -> Option<String> {
    let id = msg.get("id")?;
    let result = match msg.get("method").and_then(Value::str) {
        Some("initialize") => {
            let asked = msg
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::str);
            let version = REVISIONS
                .iter()
                .find(|r| Some(**r) == asked)
                .unwrap_or(&REVISIONS[0]);
            format!(
                r#"{{"protocolVersion":{},"capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"shards","version":{}}}}}"#,
                quote(version),
                quote(env!("CARGO_PKG_VERSION"))
            )
        }
        Some("ping") => "{}".to_string(),
        Some("tools/list") => TOOLS.to_string(),
        Some("tools/call") => {
            let params = msg.get("params");
            let args = params.and_then(|p| p.get("arguments"));
            let arg = |k: &str| args.and_then(|a| a.get(k));
            match params.and_then(|p| p.get("name")).and_then(Value::str) {
                Some("whoami") => text(&p.label, false),
                Some("mcp") => text(&p.offered, false),
                Some("peers") => {
                    let list: Vec<String> = peers
                        .iter()
                        .map(|q| {
                            format!(
                                r#"{{"name":{},"send":{},"answer":{}}}"#,
                                quote(&q.label),
                                q.send,
                                q.answer
                            )
                        })
                        .collect();
                    text(&format!("[{}]", list.join(",")), false)
                }
                Some("send") => {
                    let to = arg("to").and_then(Value::str).unwrap_or_default();
                    let body = arg("text").and_then(Value::str).unwrap_or_default();
                    match peers.iter_mut().find(|q| q.label == to) {
                        Some(q) if q.send => {
                            let next = q.last.wrapping_add(1);
                            if put(q, REQUEST, next, body) {
                                q.last = next;
                                q.sent.push(next);
                                text(&format!(r#"{{"id":{next}}}"#), false)
                            } else {
                                text(
                                    &format!("{to} has not taken what was sent it; send again later"),
                                    true,
                                )
                            }
                        }
                        _ => text(&format!("{} may not send {to} requests", p.label), true),
                    }
                }
                Some("receive") => {
                    let mut got = Vec::new();
                    for q in peers.iter_mut() {
                        if let Some(m) = q.held.take() {
                            if m.request {
                                q.asked.push(m.id);
                            }
                            got.push(format!(
                                r#"{{"from":{},"kind":"{}","id":{},"text":{}}}"#,
                                quote(&q.label),
                                if m.request { "request" } else { "answer" },
                                m.id,
                                quote(&m.text)
                            ));
                        }
                    }
                    text(&format!("[{}]", got.join(",")), false)
                }
                Some("answer") => {
                    let to = arg("to").and_then(Value::str).unwrap_or_default();
                    let to_id = arg("id").and_then(Value::u64);
                    let body = arg("text").and_then(Value::str).unwrap_or_default();
                    match (peers.iter_mut().find(|q| q.label == to), to_id) {
                        (Some(q), Some(n)) if q.asked.contains(&n) => {
                            if put(q, ANSWER, n, body) {
                                q.asked.retain(|a| *a != n);
                                text("{}", false)
                            } else {
                                text(
                                    &format!("{to} has not taken what was sent it; answer again later"),
                                    true,
                                )
                            }
                        }
                        _ => text(
                            &format!("{} has no request of {to}'s to answer by that ID", p.label),
                            true,
                        ),
                    }
                }
                _ => return Some(error(id, -32602, "no such tool")),
            }
        }
        _ => return Some(error(id, -32601, "no such method")),
    };
    Some(format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{result}}}"#,
        write(id)
    ))
}

fn error(id: &Value, code: i32, message: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":{code},"message":{}}}}}"#,
        write(id),
        quote(message)
    )
}

/// An ID as it came: a string, a number, or null.
fn write(v: &Value) -> String {
    match v {
        Value::String(s) => quote(s),
        Value::Number(n) => n.clone(),
        _ => "null".to_string(),
    }
}

/// `s` as a JSON string (RFC 8259 §7).
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn http(code: u16, kind: &str, body: &[u8]) -> Vec<u8> {
    let reason = match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        _ => "",
    };
    let mut head = format!("HTTP/1.1 {code} {reason}\r\nContent-Length: {}\r\n", body.len());
    if !kind.is_empty() {
        head.push_str(&format!("Content-Type: {kind}\r\n"));
    }
    head.push_str("\r\n");
    [head.as_bytes(), body].concat()
}

fn status(code: u16, why: &str) -> Vec<u8> {
    http(code, "text/plain", why.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An instance answers only the certificate it issued its caller: another instance's
    /// is refused, and so is none.
    #[test]
    fn an_instance_answers_its_own_caller_alone() {
        use rustls::pki_types::ServerName;
        use rustls::pki_types::pem::PemObject as _;
        let base_dir = shards_testdir::TempDir::new("server").unwrap();
        let base = base_dir.join("server");
        // SAFETY: getgid(2).
        let gid = unsafe { libc::getgid() };
        let dirs: Vec<String> = ["x", "y"]
            .iter()
            .map(|n| {
                let d = base.join(n);
                std::fs::create_dir_all(&d).unwrap();
                d.to_str().unwrap().to_string()
            })
            .collect();
        for (dir, label) in dirs.iter().zip(["agent x", "agent y"]) {
            let p = prepare(dir, label, gid).unwrap();
            std::thread::spawn(move || serve(&p, &mut Vec::new()));
        }
        let ask = |socket: &str, creds: Option<&str>| -> Result<String, String> {
            let ca = CertificateDer::from_pem_file(format!("{socket}/ca.pem")).map_err(|e| e.to_string())?;
            let mut roots = rustls::RootCertStore::empty();
            roots.add(ca).map_err(|e| e.to_string())?;
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let builder = rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots);
            let config = match creds {
                Some(d) => {
                    let cert =
                        CertificateDer::from_pem_file(format!("{d}/cert.pem")).map_err(|e| e.to_string())?;
                    let key =
                        PrivateKeyDer::from_pem_file(format!("{d}/key.pem")).map_err(|e| e.to_string())?;
                    builder
                        .with_client_auth_cert(vec![cert], key)
                        .map_err(|e| e.to_string())?
                }
                None => builder.with_no_client_auth(),
            };
            let name = ServerName::try_from(NAME).map_err(|e| e.to_string())?;
            let tls = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| e.to_string())?;
            let sock = UnixStream::connect(format!("{socket}/server.sock")).map_err(|e| e.to_string())?;
            sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .map_err(|e| e.to_string())?;
            let mut s = rustls::StreamOwned::new(tls, sock);
            let body = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"whoami"}}"#;
            let req = format!(
                "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
            let mut got = Vec::new();
            s.read_to_end(&mut got).map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&got).into_owned();
            text.split_once(r#""text":""#)
                .and_then(|(_, r)| r.split_once('"'))
                .map(|(t, _)| t.to_string())
                .ok_or(format!("no answer: {text:?}"))
        };
        let (x, y) = (dirs[0].as_str(), dirs[1].as_str());
        assert_eq!(ask(x, Some(x)), Ok("agent x".to_string()));
        assert_eq!(ask(y, Some(y)), Ok("agent y".to_string()));
        assert!(
            ask(x, Some(y)).is_err(),
            "y's certificate was answered by x's instance"
        );
        assert!(ask(x, None).is_err(), "no certificate was answered");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What another instance sends is taken only as the grant says, whatever it is made to
    /// send: a request only from a peer this one answers, an answer only to a request it
    /// sent.
    #[test]
    fn a_peer_is_heard_only_as_granted() {
        let pair = || {
            let mut fds = [0 as RawFd; 2];
            // SAFETY: socketpair(2) into a two-element array.
            let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, fds.as_mut_ptr()) };
            assert_eq!(rc, 0);
            // SAFETY: both were just made, and are owned here alone.
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
        };
        let peer = |fd: OwnedFd, answer: bool| Peer {
            label: "agent o".into(),
            fd,
            send: true,
            answer,
            held: None,
            sent: Vec::new(),
            last: 0,
            asked: Vec::new(),
            gone: false,
        };
        let other = |fd: &OwnedFd| peer(fd.try_clone().unwrap(), true);
        // A request from one it does not answer: dropped.
        let (mine, theirs) = pair();
        let mut p = peer(mine, false);
        assert!(put(&other(&theirs), REQUEST, 1, "do this"));
        take(&mut p);
        assert!(p.held.is_none());
        // From one it answers: held.
        let (mine, theirs) = pair();
        let mut p = peer(mine, true);
        assert!(put(&other(&theirs), REQUEST, 1, "do this"));
        take(&mut p);
        assert!(p.held.as_ref().is_some_and(|m| m.request && m.id == 1));
        // An answer to no request sent: dropped; to one sent, held, and no longer awaited.
        let (mine, theirs) = pair();
        let mut p = peer(mine, false);
        assert!(put(&other(&theirs), ANSWER, 7, "unasked"));
        take(&mut p);
        assert!(p.held.is_none());
        p.sent.push(7);
        assert!(put(&other(&theirs), ANSWER, 7, "asked"));
        take(&mut p);
        assert!(p.held.as_ref().is_some_and(|m| !m.request && m.text == "asked"));
        assert!(p.sent.is_empty());
    }

    #[test]
    fn strings_are_quoted_as_json_writes_them() {
        assert_eq!(quote("a\"b\\c\n"), r#""a\"b\\c\u000a""#);
    }

    /// A head past its bound, a body past its bound, and a length that is no number each
    /// end the connection with an answer.
    #[test]
    fn requests_past_their_bounds_are_refused() {
        let p = prepare_for_test();
        let long = vec![b'a'; HEAD_MAX + 1];
        assert!(
            request(&p, &mut [], &long).is_some_and(|(a, _, close)| a.starts_with(b"HTTP/1.1 431") && close)
        );
        let big = format!("POST /mcp HTTP/1.1\r\nContent-Length: {}\r\n\r\n", BODY_MAX + 1);
        assert!(
            request(&p, &mut [], big.as_bytes())
                .is_some_and(|(a, _, close)| a.starts_with(b"HTTP/1.1 413") && close)
        );
        let bad = b"POST /mcp HTTP/1.1\r\nContent-Length: x\r\n\r\n";
        assert!(
            request(&p, &mut [], bad).is_some_and(|(a, _, close)| a.starts_with(b"HTTP/1.1 400") && close)
        );
    }

    fn prepare_for_test() -> Prepared {
        let d = shards_testdir::TempDir::new("server-bounds").unwrap();
        // SAFETY: getgid(2).
        prepare(d.to_str().unwrap(), "agent t", unsafe { libc::getgid() }).unwrap()
    }
}

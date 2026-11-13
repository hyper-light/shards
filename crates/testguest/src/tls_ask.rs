//! `tls-ask ADDR SNI [-- PROTO METHOD PATH [-s SNI] [-H NAME:VALUE]... [-d BODY]]...`: requests
//! to `ADDR` over TLS for server name `SNI` (or the step's `-s`), trusting `SSL_CERT_FILE` (else
//! the system bundle), as a harness's own client makes them toward a credential-granted host
//! (D242, plan step 33's steps 7 and 8). `PROTO` is `h2`, each request a stream of its server
//! name's one connection, in turn, or `http/1.1`, each request a connection of its own.
//!
//! In a value or a body, `{env:NAME}` is variable `NAME` (a placeholder its orchestrator
//! staged), and `{access_token}` and `{refresh_token}` are that member of the last answer's JSON
//! body (a sign-in's placeholders, as its CLI keeps them). Each request's answer is one line on
//! stdout: `answer STATUS echo=X-ECHO body=BODY`, or `answer reset CODE` (its stream reset), or
//! `answer error WHY`.

use std::io::{self, Read as _, Write as _};
use std::net::TcpStream;
use std::sync::Arc;

#[allow(dead_code)]
#[path = "../../net/src/hpack.rs"]
mod hpack;

use hpack::{Decoded, Decoder, Encoder, Field, Indexing};

type Tls = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

/// HTTP/2's frame types and flags this end reads or writes (RFC 9113 §6).
const DATA: u8 = 0x0;
const HEADERS: u8 = 0x1;
const RST_STREAM: u8 = 0x3;
const SETTINGS: u8 = 0x4;
const PING: u8 = 0x6;
const GOAWAY: u8 = 0x7;
const WINDOW_UPDATE: u8 = 0x8;
const CONTINUATION: u8 = 0x9;
const END_STREAM: u8 = 0x1;
const ACK: u8 = 0x1;
const END_HEADERS: u8 = 0x4;
const PADDED: u8 = 0x8;
const PRIORITY: u8 = 0x20;
/// The client's connection preface (§3.4).
const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
/// SETTINGS_HEADER_TABLE_SIZE and SETTINGS_MAX_FRAME_SIZE as the protocol starts them (§6.5.2).
const TABLE: usize = 4096;
const FRAME: usize = 16_384;

/// One request, as its step names it.
struct Step {
    h2: bool,
    sni: Option<String>,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

/// An answer: its status, its `x-echo` field, its body; or why there is none.
enum Answer {
    Got {
        status: String,
        echo: String,
        body: String,
    },
    Reset(u32),
    Failed(String),
}

pub fn main(args: &[String]) -> i32 {
    let (Some(addr), Some(sni)) = (args.first(), args.get(1)) else {
        let _ = writeln!(io::stderr(), "tls-ask ADDR SNI [-- STEP]...");
        return 2;
    };
    let steps = match steps(args.get(2..).unwrap_or_default()) {
        Ok(steps) => steps,
        Err(why) => {
            let _ = writeln!(io::stderr(), "tls-ask: {why}");
            return 2;
        }
    };
    let mut last = String::new();
    let mut h2: Vec<(String, H2)> = Vec::new();
    for step in &steps {
        let sni = step.sni.as_ref().unwrap_or(sni);
        let answer = match config(if step.h2 { "h2" } else { "http/1.1" }) {
            Err(why) => Answer::Failed(why),
            Ok(config) if !step.h2 => http1(addr, sni, &config, step, &last),
            Ok(config) => {
                if !h2.iter().any(|(name, _)| name == sni) {
                    match H2::open(addr, sni, &config) {
                        Ok(c) => h2.push((sni.clone(), c)),
                        Err(why) => {
                            let _ = writeln!(io::stdout(), "answer error {why}");
                            continue;
                        }
                    }
                }
                match h2.iter_mut().find(|(name, _)| name == sni) {
                    Some((_, c)) => c.ask(sni, step, &last),
                    None => Answer::Failed("no connection".into()),
                }
            }
        };
        let line = match &answer {
            Answer::Got { status, echo, body } => format!("answer {status} echo={echo} body={body}"),
            Answer::Reset(code) => format!("answer reset {code}"),
            Answer::Failed(why) => format!("answer error {why}"),
        };
        let _ = writeln!(io::stdout(), "{}", line.replace(['\r', '\n'], " "));
        let _ = io::stdout().flush();
        if let Answer::Got { body, .. } = answer {
            last = body;
        }
    }
    0
}

fn steps(args: &[String]) -> Result<Vec<Step>, String> {
    let mut out = Vec::new();
    for words in args.split(|a| a == "--").filter(|w| !w.is_empty()) {
        let (Some(proto), Some(method), Some(path)) = (words.first(), words.get(1), words.get(2)) else {
            return Err("a step is PROTO METHOD PATH".into());
        };
        if proto != "h2" && proto != "http/1.1" {
            return Err(format!("no protocol {proto}"));
        }
        let mut step = Step {
            h2: proto == "h2",
            sni: None,
            method: method.clone(),
            path: path.clone(),
            headers: Vec::new(),
            body: String::new(),
        };
        let mut rest = words.get(3..).unwrap_or_default().iter();
        while let Some(flag) = rest.next() {
            let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "-H" => {
                    let (name, v) = value.split_once(':').ok_or("-H NAME:VALUE")?;
                    step.headers
                        .push((name.to_ascii_lowercase(), v.trim_start().to_string()));
                }
                "-d" => step.body.clone_from(value),
                "-s" => step.sni = Some(value.clone()),
                other => return Err(format!("no flag {other}")),
            }
        }
        out.push(step);
    }
    Ok(out)
}

/// `text` with its `{env:NAME}`, `{access_token}` and `{refresh_token}` filled in.
fn fill(text: &str, last: &str) -> String {
    let mut out = text.to_string();
    for member in ["access_token", "refresh_token"] {
        let key = format!("{{{member}}}");
        if out.contains(&key) {
            out = out.replace(&key, &member_of(last, member));
        }
    }
    while let Some(at) = out.find("{env:") {
        let Some(len) = out.get(at..).and_then(|s| s.find('}')) else {
            break;
        };
        let name = out.get(at + 5..at + len).unwrap_or_default().to_string();
        let value = std::env::var(&name).unwrap_or_default();
        out.replace_range(at..=at + len, &value);
    }
    out
}

/// A JSON body's string member `name`, as a token response writes one (RFC 6749 §5.1).
fn member_of(body: &str, name: &str) -> String {
    let key = format!("\"{name}\"");
    body.find(&key)
        .and_then(|at| body.get(at + key.len()..))
        .and_then(|s| s.split_once('"'))
        .and_then(|(_, s)| s.split_once('"'))
        .map(|(v, _)| v.to_string())
        .unwrap_or_default()
}

fn config(proto: &str) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls::pki_types::pem::PemObject as _;
    let bundle =
        std::env::var("SSL_CERT_FILE").unwrap_or_else(|_| "/etc/ssl/certs/ca-certificates.crt".into());
    let mut roots = rustls::RootCertStore::empty();
    for c in
        rustls::pki_types::CertificateDer::pem_file_iter(&bundle).map_err(|e| format!("{bundle}: {e}"))?
    {
        let _ = roots.add(c.map_err(|e| format!("{bundle}: {e}"))?);
    }
    let mut config =
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| format!("tls: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = vec![proto.as_bytes().to_vec()];
    Ok(Arc::new(config))
}

fn connect(addr: &str, sni: &str, config: &Arc<rustls::ClientConfig>) -> Result<Tls, String> {
    let sock = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let name = rustls::pki_types::ServerName::try_from(sni.to_string()).map_err(|e| format!("name: {e}"))?;
    let conn = rustls::ClientConnection::new(config.clone(), name).map_err(|e| format!("tls: {e}"))?;
    Ok(rustls::StreamOwned::new(conn, sock))
}

fn http1(addr: &str, sni: &str, config: &Arc<rustls::ClientConfig>, step: &Step, last: &str) -> Answer {
    let mut tls = match connect(addr, sni, config) {
        Ok(t) => t,
        Err(why) => return Answer::Failed(why),
    };
    let body = fill(&step.body, last);
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {sni}\r\nConnection: close\r\n",
        step.method, step.path
    );
    for (name, value) in &step.headers {
        head.push_str(&format!("{name}: {}\r\n", fill(value, last)));
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    if let Err(e) = tls.write_all(head.as_bytes()) {
        return Answer::Failed(format!("write: {e}"));
    }
    let mut got = Vec::new();
    match tls.read_to_end(&mut got) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !got.is_empty() => {}
        Err(e) => return Answer::Failed(format!("read: {e}")),
    }
    let text = String::from_utf8_lossy(&got).into_owned();
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return Answer::Failed("no response head".into());
    };
    let status = head.split(' ').nth(1).unwrap_or_default().to_string();
    let echo = head
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(n, _)| n.eq_ignore_ascii_case("x-echo"))
        })
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    Answer::Got {
        status,
        echo,
        body: body.to_string(),
    }
}

/// An HTTP/2 connection's client end: its encoder and decoder, and the next stream's id.
struct H2 {
    tls: Tls,
    encoder: Encoder,
    decoder: Decoder,
    next: u32,
}

impl H2 {
    fn open(addr: &str, sni: &str, config: &Arc<rustls::ClientConfig>) -> Result<H2, String> {
        let mut tls = connect(addr, sni, config)?;
        let mut first = PREFACE.to_vec();
        frame(&mut first, SETTINGS, 0, 0, &[]);
        tls.write_all(&first).map_err(|e| format!("preface: {e}"))?;
        if tls.conn.alpn_protocol() != Some(b"h2") {
            return Err("the server chose no h2".into());
        }
        Ok(H2 {
            tls,
            encoder: Encoder::new(TABLE, TABLE),
            decoder: Decoder::new(TABLE),
            next: 1,
        })
    }

    fn ask(&mut self, sni: &str, step: &Step, last: &str) -> Answer {
        match self.exchange(sni, step, last) {
            Ok(a) => a,
            Err(why) => Answer::Failed(why),
        }
    }

    fn exchange(&mut self, sni: &str, step: &Step, last: &str) -> Result<Answer, String> {
        let id = self.next;
        self.next = self.next.checked_add(2).ok_or("no stream id left")?;
        let body = fill(&step.body, last);
        let mut fields = vec![
            Field::new(b":method", step.method.as_bytes(), Indexing::Incremental),
            Field::new(b":scheme", b"https", Indexing::Incremental),
            Field::new(b":authority", sni.as_bytes(), Indexing::Incremental),
            Field::new(b":path", step.path.as_bytes(), Indexing::Incremental),
        ];
        for (name, value) in &step.headers {
            fields.push(Field::new(
                name.as_bytes(),
                fill(value, last).as_bytes(),
                Indexing::Without,
            ));
        }
        let mut block = Vec::new();
        self.encoder.encode(&fields, &mut block);
        let mut out = Vec::new();
        let mut chunks = block.chunks(FRAME).peekable();
        let mut kind = HEADERS;
        while let Some(chunk) = chunks.next() {
            let mut flags = if chunks.peek().is_none() { END_HEADERS } else { 0 };
            if kind == HEADERS && body.is_empty() {
                flags |= END_STREAM;
            }
            frame(&mut out, kind, flags, id, chunk);
            kind = CONTINUATION;
        }
        let mut pieces = body.as_bytes().chunks(FRAME).peekable();
        while let Some(piece) = pieces.next() {
            frame(
                &mut out,
                DATA,
                if pieces.peek().is_none() { END_STREAM } else { 0 },
                id,
                piece,
            );
        }
        self.tls.write_all(&out).map_err(|e| format!("write: {e}"))?;
        self.answer(id)
    }

    /// Stream `id`'s answer, read frame by frame; the connection's own frames answered as
    /// they come.
    fn answer(&mut self, id: u32) -> Result<Answer, String> {
        let (mut status, mut echo, mut body) = (String::new(), String::new(), Vec::new());
        let mut block = Vec::new();
        loop {
            let (kind, flags, stream, payload) = self.read()?;
            match kind {
                SETTINGS if flags & ACK == 0 => {
                    let mut ack = Vec::new();
                    frame(&mut ack, SETTINGS, ACK, 0, &[]);
                    self.tls.write_all(&ack).map_err(|e| format!("write: {e}"))?;
                }
                PING if flags & ACK == 0 => {
                    let mut pong = Vec::new();
                    frame(&mut pong, PING, ACK, 0, &payload);
                    self.tls.write_all(&pong).map_err(|e| format!("write: {e}"))?;
                }
                GOAWAY => return Err(format!("goaway {:?}", payload.get(4..8))),
                RST_STREAM if stream == id => {
                    let code = payload
                        .get(..4)
                        .and_then(|b| b.try_into().ok())
                        .map_or(0, u32::from_be_bytes);
                    return Ok(Answer::Reset(code));
                }
                HEADERS | CONTINUATION if stream == id => {
                    let fragment = if kind == HEADERS {
                        unpad(&payload, flags, true)?
                    } else {
                        payload.as_slice()
                    };
                    block.extend_from_slice(fragment);
                    if flags & END_HEADERS != 0 {
                        let Ok(Decoded::Fields(fields)) = self.decoder.decode(&block, usize::MAX) else {
                            return Err("a header block this end cannot decode".into());
                        };
                        block.clear();
                        for f in fields {
                            let value = String::from_utf8_lossy(&f.value).into_owned();
                            match f.name.as_slice() {
                                b":status" => status = value,
                                b"x-echo" => echo = value,
                                _ => {}
                            }
                        }
                    }
                    if flags & END_STREAM != 0 {
                        break;
                    }
                }
                DATA if stream == id => {
                    let data = unpad(&payload, flags, false)?;
                    body.extend_from_slice(data);
                    let credit = u32::try_from(payload.len()).map_err(|_| "a frame past u32")?;
                    if credit > 0 {
                        let mut w = Vec::new();
                        frame(&mut w, WINDOW_UPDATE, 0, 0, &credit.to_be_bytes());
                        frame(&mut w, WINDOW_UPDATE, 0, id, &credit.to_be_bytes());
                        self.tls.write_all(&w).map_err(|e| format!("write: {e}"))?;
                    }
                    if flags & END_STREAM != 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        Ok(Answer::Got {
            status,
            echo,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    fn read(&mut self) -> Result<(u8, u8, u32, Vec<u8>), String> {
        let mut head = [0u8; 9];
        self.tls.read_exact(&mut head).map_err(|e| format!("read: {e}"))?;
        let [l0, l1, l2, kind, flags, s0, s1, s2, s3] = head;
        let len = usize::from(l0) << 16 | usize::from(l1) << 8 | usize::from(l2);
        let stream = u32::from_be_bytes([s0, s1, s2, s3]) & 0x7fff_ffff;
        let mut payload = vec![0u8; len];
        self.tls
            .read_exact(&mut payload)
            .map_err(|e| format!("read: {e}"))?;
        Ok((kind, flags, stream, payload))
    }
}

/// A frame's payload without its padding (and a HEADERS frame's priority, §6.2).
fn unpad(payload: &[u8], flags: u8, headers: bool) -> Result<&[u8], String> {
    let mut p = payload;
    let mut pad = 0usize;
    if flags & PADDED != 0 {
        let (&n, rest) = p.split_first().ok_or("a padded frame with no pad length")?;
        pad = usize::from(n);
        p = rest;
    }
    if headers && flags & PRIORITY != 0 {
        p = p.get(5..).ok_or("a HEADERS frame cut in its priority")?;
    }
    p.get(..p.len().checked_sub(pad).ok_or("padding past its frame")?)
        .ok_or_else(|| "padding past its frame".into())
}

fn frame(out: &mut Vec<u8>, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
    let len = payload.len().to_be_bytes();
    out.extend_from_slice(len.get(len.len().saturating_sub(3)..).unwrap_or_default());
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
}

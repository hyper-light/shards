//! The proxy, served in-process to a client on its socket, its requests going to servers
//! of the tests' own (one of them HTTPS, under a CA these tests trust upstream), its
//! questions answered as a build's thread answers them: `/denied` refused.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use super::*;
use std::net::{TcpListener, TcpStream};

/// A directory of its own, removed with the proxy: named short, as the proxy's socket in it
/// must be (sockaddr_un), whatever its test.
fn scratch(_: &str) -> shards_testdir::TempDir {
    shards_testdir::TempDir::new("proxy").unwrap()
}

/// A server answering each request with what `answer` makes of its head (method, target,
/// and fields, lowercased), for each connection on a thread of its own. It says what it was
/// asked on `asked`.
fn upstream(
    tls: Option<Arc<rustls::ServerConfig>>,
    answer: fn(&str, &str) -> Vec<u8>,
) -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (said, asked) = mpsc::channel();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { return };
            let (tls, said) = (tls.clone(), said.clone());
            std::thread::spawn(move || {
                let stream: Box<dyn ReadWrite> = match tls {
                    Some(c) => Box::new(rustls::StreamOwned::new(
                        rustls::ServerConnection::new(c).unwrap(),
                        conn,
                    )),
                    None => Box::new(conn),
                };
                let mut r = BufReader::new(stream);
                loop {
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        head.push_str(&line);
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; length];
                    r.read_exact(&mut body).unwrap();
                    let first = head.lines().next().unwrap_or_default().to_string();
                    let mut parts = first.split(' ');
                    let (method, target) = (
                        parts.next().unwrap().to_string(),
                        parts.next().unwrap().to_string(),
                    );
                    let close = head.to_ascii_lowercase().contains("\r\nconnection: close\r\n");
                    let _ = said.send(format!("{head}{}", String::from_utf8_lossy(&body)));
                    let reply = answer(&method, &target);
                    if r.get_mut().write_all(&reply).is_err() {
                        return;
                    }
                    let _ = r.get_mut().flush();
                    // `#cut`: the connection ends bare, no TLS close_notify sent.
                    if reply.ends_with(b"#cut") {
                        return;
                    }
                    // `#close`, or a client that asked to close: answered and let go.
                    if reply.ends_with(b"#close") || close {
                        r.get_mut().end();
                        return;
                    }
                }
            });
        }
    });
    (port, asked)
}

/// A server's connection, plain or TLS, and how it ends: TLS's with its close_notify, as
/// Go's `tls.Conn.Close` ends one.
trait ReadWrite: Read + Write + Send {
    fn end(&mut self) {}
}
impl ReadWrite for TcpStream {}
impl ReadWrite for rustls::StreamOwned<rustls::ServerConnection, TcpStream> {
    fn end(&mut self) {
        self.conn.send_close_notify();
        let _ = self.flush();
    }
}

/// The tests' upstream: `/hello` a short body, `/redirect` to it, `/chunked` a chunked
/// one, `/big` one of 5000 bytes with no length (to the connection's end), `/missing`
/// 404, a part asked for 206; anything else 200 empty.
fn answer(method: &str, target: &str) -> Vec<u8> {
    let _ = method;
    match target {
        "/hello" | "/denied" => b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nContent-Type: text/plain\r\nX-B: 2\r\nX-A: 1\r\n\r\nhello\n".to_vec(),
        "/redirect" => b"HTTP/1.1 302 Found\r\nLocation: /hello\r\nContent-Length: 0\r\n\r\n".to_vec(),
        "/chunked" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nDate: Mon, 02 Jan 2006 15:04:05 GMT\r\n\r\n3\r\nabc\r\n0\r\n\r\n".to_vec(),
        "/big" => [&b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n"[..], &[b'x'; 5000], b"#close"].concat(),
        "/missing" => b"HTTP/1.1 404 Not Found\r\nContent-Length: 4\r\n\r\ngone".to_vec(),
        "/part" => b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\n\r\nhe".to_vec(),
        "/bare" => b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabc#cut".to_vec(),
        _ => b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
    }
}

/// What the build's thread does: answers each question as `allow` says of its URL, until
/// `done`.
fn answering(q: &Questions, done: &AtomicBool, checked: &Mutex<Vec<String>>, allow: fn(&str) -> bool) {
    while !done.load(Ordering::SeqCst) {
        q.answer(&mut |method, url| {
            checked.lock().unwrap().push(format!("{method} {url}"));
            allow(url)
        });
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Runs `client` against a proxy, answering its questions, `/denied` refused; returns what
/// it returned, the checks put to the policies, and the capture. Its requests may reach the
/// loopback, where the tests' servers are, unless `host_kept` (as a build's proxy is made).
fn with_proxy<T: Send>(
    name: &str,
    roots: Option<rustls::pki_types::CertificateDer<'static>>,
    host_kept: bool,
    client: impl FnOnce(&Path, &[u8]) -> T + Send,
) -> (T, Vec<String>, Capture) {
    with_proxy_deciding(name, roots, host_kept, |url| !url.contains("/denied"), client)
}

/// [`with_proxy`], each question answered as `allow` says of its URL.
fn with_proxy_deciding<T: Send>(
    name: &str,
    roots: Option<rustls::pki_types::CertificateDer<'static>>,
    host_kept: bool,
    allow: fn(&str) -> bool,
    client: impl FnOnce(&Path, &[u8]) -> T + Send,
) -> (T, Vec<String>, Capture) {
    let scratch = scratch(name);
    let mut proxy = Proxy::new(scratch.to_path_buf(), &shards_image::store::Limits::none()).unwrap();
    if let Some(root) = roots {
        proxy.tls = shards_registry::tls::client_config(vec![root], None).unwrap();
    }
    if !host_kept {
        proxy.reach = |_| true;
    }
    serving(&proxy, allow, client)
}

/// Runs `client` against `proxy`, each question answered as `allow` says of its URL.
fn serving<T: Send>(
    proxy: &Proxy,
    allow: fn(&str) -> bool,
    client: impl FnOnce(&Path, &[u8]) -> T + Send,
) -> (T, Vec<String>, Capture) {
    let (session, questions) = Session::begin(proxy, shards_registry::proxy::Proxies::default()).unwrap();
    let (stop, stopped) = UnixStream::pair().unwrap();
    let done = AtomicBool::new(false);
    let checked = Mutex::new(Vec::new());
    let out = std::thread::scope(|scope| {
        let (session, stopped) = (&session, &stopped);
        std::thread::Builder::new()
            .spawn_scoped(scope, move || session.serve(scope, stopped))
            .unwrap();
        let (done, checked) = (&done, &checked);
        std::thread::Builder::new()
            .spawn_scoped(scope, move || answering(&questions, done, checked, allow))
            .unwrap();
        // A client that fails still ends the session, so its test fails rather than waits
        // on the session's threads for ever.
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client(proxy.socket(), proxy.ca())
        }));
        done.store(true, Ordering::SeqCst);
        drop(stop);
        session.end();
        out
    });
    let out = out.unwrap_or_else(|p| std::panic::resume_unwind(p));
    (out, checked.into_inner().unwrap(), session.capture())
}

/// Sends `raw` on a new connection and reads until the proxy closes it or `until` comes.
fn exchange(socket: &Path, raw: &[u8], until: Option<&[u8]>) -> Vec<u8> {
    let mut c = UnixStream::connect(socket).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(raw).unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if until.is_some_and(|u| got.windows(u.len()).any(|w| w == u)) {
            return got;
        }
        match c.read(&mut buf) {
            Ok(0) | Err(_) => return got,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
}

/// Its `Date` field's value said as `DATE`.
fn dated(s: &[u8]) -> String {
    let s = String::from_utf8_lossy(s).into_owned();
    s.lines()
        .map(|l| {
            if l.starts_with("Date: ") && !l.contains("2006") {
                "Date: DATE"
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn plain_requests_are_checked_and_passed_on_as_buildkit_passes_them() {
    let (port, asked) = upstream(None, answer);
    let base = format!("http://127.0.0.1:{port}");
    let ((hello, origin, refused, chunked, big), checked, capture) = with_proxy(
        "plain",
        None,
        false,
        |socket, _| {
            let hello = exchange(
            socket,
            format!("GET {base}/hello HTTP/1.1\r\nHost: x\r\nUser-Agent: t/1\r\nAccept-Encoding: gzip\r\nProxy-Authorization: p\r\nConnection: close\r\n\r\n").as_bytes(),
            None,
        );
            let origin = exchange(
                socket,
                format!("GET /hello HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
                None,
            );
            let refused = exchange(
                socket,
                format!("GET {base}/denied HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(),
                Some(b"Forbidden\n"),
            );
            let chunked = exchange(
                socket,
                format!("GET {base}/chunked HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
                None,
            );
            let big = exchange(
                socket,
                format!("GET {base}/big HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
                None,
            );
            (hello, origin, refused, chunked, big)
        },
    );
    // Passed on: the response as it was, its fields sorted, a Date added, closing.
    assert_eq!(
        dated(&hello),
        "HTTP/1.1 200 OK\nContent-Length: 6\nContent-Type: text/plain\nX-A: 1\nX-B: 2\nDate: DATE\nConnection: close\n\nhello"
    );
    assert_eq!(dated(&origin), dated(&hello));
    // Refused as BuildKit refuses: http.Error's 403, the connection kept.
    assert_eq!(
        dated(&refused),
        "HTTP/1.1 403 Forbidden\nContent-Type: text/plain; charset=utf-8\nX-Content-Type-Options: nosniff\nDate: DATE\nContent-Length: 10\n\nForbidden"
    );
    // A short chunked body has its length, as Go's server gives one it holds whole, and
    // what it is, sniffed, where the upstream said nothing.
    assert_eq!(
        dated(&chunked),
        "HTTP/1.1 200 OK\nDate: Mon, 02 Jan 2006 15:04:05 GMT\nContent-Length: 3\nContent-Type: text/plain; charset=utf-8\nConnection: close\n\nabc"
    );
    // A long one of no length goes chunked.
    let big = String::from_utf8_lossy(&big);
    assert!(
        big.contains("Transfer-Encoding: chunked\r\n") && big.ends_with("0\r\n\r\n"),
        "{big}"
    );
    // Upstream: the client's own User-Agent; no Accept-Encoding, no proxy credentials.
    let first = asked.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        first.starts_with("GET /hello HTTP/1.1\r\nHost: 127.0.0.1:"),
        "{first}"
    );
    assert!(first.contains("\r\nUser-Agent: t/1\r\n"), "{first}");
    assert!(
        !first.to_ascii_lowercase().contains("accept-encoding") && !first.contains("Proxy-Authorization"),
        "{first}"
    );
    assert_eq!(
        checked,
        [
            format!("GET {base}/hello"),
            format!("GET {base}/hello"),
            format!("GET {base}/denied"),
            format!("GET {base}/chunked"),
            format!("GET {base}/big"),
        ]
    );
    // The refused request is in no record.
    assert_eq!(
        String::from_utf8(capture.summary()).unwrap(),
        format!(
            "proxy network requests:\n- GET {base}/hello -> 200\n- GET {base}/hello -> 200\n- GET {base}/chunked -> 200\n- GET {base}/big -> 200\n"
        )
    );
    let hello_digest = digest_of({
        let mut h = sha2::Sha256::new();
        h.update(b"hello\n");
        h
    });
    assert_eq!(
        capture.materials.first().map(|m| m.digest.as_str()),
        Some(hello_digest.as_str())
    );
    assert_eq!(capture.materials.len(), 4);
}

#[test]
fn requests_that_are_no_material_say_why() {
    let (port, _asked) = upstream(None, answer);
    let base = format!("http://127.0.0.1:{port}");
    let (_, _, capture) = with_proxy("incomplete", None, false, |socket, _| {
        let close = "Connection: close\r\n";
        exchange(
            socket,
            format!("POST {base}/hello HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\n{close}\r\nx").as_bytes(),
            None,
        );
        exchange(
            socket,
            format!("GET {base}/hello HTTP/1.1\r\nHost: x\r\nRange: bytes=0-1\r\n{close}\r\n").as_bytes(),
            None,
        );
        exchange(
            socket,
            format!("GET {base}/part HTTP/1.1\r\nHost: x\r\n{close}\r\n").as_bytes(),
            None,
        );
        exchange(
            socket,
            format!("GET {base}/missing HTTP/1.1\r\nHost: x\r\n{close}\r\n").as_bytes(),
            None,
        );
        exchange(
            socket,
            format!("GET {base}/redirect HTTP/1.1\r\nHost: x\r\n{close}\r\n").as_bytes(),
            None,
        );
        exchange(
            socket,
            format!("HEAD {base}/hello HTTP/1.1\r\nHost: x\r\n{close}\r\n").as_bytes(),
            None,
        );
        // Nothing listens at port 1 on the loopback.
        exchange(
            socket,
            b"GET http://127.0.0.1:1/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
            None,
        );
    });
    let reasons: Vec<(String, &str)> = capture
        .incomplete
        .iter()
        .map(|i| (format!("{} {}", i.method, i.url.replace(&base, "")), i.reason))
        .collect();
    assert_eq!(
        reasons,
        [
            ("POST /hello".to_string(), "method_not_materializable"),
            ("GET /hello".to_string(), "partial_response"),
            ("GET /part".to_string(), "partial_response"),
            ("GET /missing".to_string(), "unsuccessful_response"),
            ("HEAD /hello".to_string(), "method_not_materializable"),
            ("GET http://127.0.0.1:1/".to_string(), "upstream_error"),
        ]
    );
    let redirect = capture
        .requests
        .iter()
        .find(|r| r.url.ends_with("/redirect"))
        .unwrap();
    assert_eq!(
        (redirect.status, redirect.redirect.as_str()),
        (302, format!("{base}/hello").as_str())
    );
}

/// The host's own addresses are no step's to reach: a request there is a 502, before any
/// connection is made, and an upstream error in the step's record.
#[test]
fn the_loopback_is_never_reached_through_the_proxy() {
    let (port, asked) = upstream(None, answer);
    let ((answered, ipv6), _, capture) = with_proxy("loopback", None, true, |socket, _| {
        let close = "Connection: close\r\n";
        (
            exchange(
                socket,
                format!("GET http://127.0.0.1:{port}/hello HTTP/1.1\r\nHost: x\r\n{close}\r\n").as_bytes(),
                None,
            ),
            exchange(
                socket,
                format!("GET http://[::1]:{port}/ HTTP/1.1\r\nHost: x\r\n{close}\r\n").as_bytes(),
                None,
            ),
        )
    });
    let answered = String::from_utf8_lossy(&answered);
    assert!(answered.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{answered}");
    assert!(
        answered.contains("no address a build step may reach"),
        "{answered}"
    );
    assert!(String::from_utf8_lossy(&ipv6).starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
    assert!(
        asked.recv_timeout(Duration::from_millis(200)).is_err(),
        "the loopback was reached"
    );
    assert_eq!(capture.incomplete.len(), 2);
    assert!(capture.incomplete.iter().all(|i| i.reason == "upstream_error"));
    assert!(!reachable("127.0.0.1".parse().unwrap()));
    assert!(!reachable("::1".parse().unwrap()));
    assert!(!reachable("::ffff:127.0.0.1".parse().unwrap()));
    assert!(!reachable("169.254.169.254".parse().unwrap()));
    assert!(!reachable("fe80::1".parse().unwrap()));
    assert!(!reachable("0.0.0.0".parse().unwrap()));
    assert!(!reachable("224.0.0.1".parse().unwrap()));
    assert!(!reachable("255.255.255.255".parse().unwrap()));
    assert!(!reachable("::".parse().unwrap()));
    assert!(!reachable("ff02::1".parse().unwrap()));
    assert!(!reachable("::127.0.0.1".parse().unwrap()));
    assert!(reachable("10.0.0.1".parse().unwrap()));
    assert!(reachable("93.184.215.14".parse().unwrap()));
    assert!(reachable(
        "2606:2800:21f:cb07:6820:80da:af6b:8b2c".parse().unwrap()
    ));
}

/// An HTTPS server's TLS under a CA of its own, for 127.0.0.1, and that CA.
fn https_server() -> (
    Arc<rustls::ServerConfig>,
    rustls::pki_types::CertificateDer<'static>,
) {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(ca, ca_key);
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let config =
        rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
            )
            .unwrap();
    (Arc::new(config), ca_cert.der().clone())
}

/// A client's TLS over a tunnel the proxy opened, trusting the proxy's CA alone.
fn tunnel_client(
    socket: &Path,
    ca: &[u8],
    authority: &str,
) -> rustls::StreamOwned<rustls::ClientConnection, UnixStream> {
    let mut c = UnixStream::connect(socket).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    c.write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .unwrap();
    let mut established = [0u8; 39];
    c.read_exact(&mut established).unwrap();
    assert_eq!(&established, b"HTTP/1.1 200 Connection Established\r\n\r\n");
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls::pki_types::pem::PemObject::pem_slice_iter(ca) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config =
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let host = strip_port(authority).to_string();
    let name = rustls::pki_types::ServerName::try_from(host).unwrap();
    rustls::StreamOwned::new(rustls::ClientConnection::new(Arc::new(config), name).unwrap(), c)
}

/// Reads a response off a tunnel: its head and as much body as its length says.
fn response(tls: &mut impl Read) -> String {
    let mut r = BufReader::new(tls);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return head;
        }
        head.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    // A chunked body is read whole, to its last chunk, so none of it is left for the next
    // response, however its records came.
    if head.contains("\r\nTransfer-Encoding: chunked\r\n") {
        let mut body = String::new();
        loop {
            let mut size = String::new();
            r.read_line(&mut size).unwrap();
            body.push_str(&size);
            let n = usize::from_str_radix(size.trim_end(), 16).unwrap();
            let mut chunk = vec![0u8; n + 2];
            r.read_exact(&mut chunk).unwrap();
            body.push_str(&String::from_utf8_lossy(&chunk));
            if n == 0 {
                return head + &body;
            }
        }
    }
    let length = head
        .lines()
        .find_map(|l| {
            l.strip_prefix("Content-Length: ")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    r.read_exact(&mut body).unwrap();
    head + &String::from_utf8_lossy(&body)
}

/// HTTPS through CONNECT: the step's client trusts the build's CA for the tunnel's host;
/// each request in the tunnel is checked as `https://` and the tunnel's host, its port
/// kept, and passed on over TLS the host trusts; a refusal is BuildKit's exact 403, and
/// ends the tunnel.
#[test]
fn tunnels_are_checked_request_by_request() {
    let (tls, root) = https_server();
    let (port, asked) = upstream(Some(tls), answer);
    let authority = format!("127.0.0.1:{port}");
    let ((first, second, refused), checked, capture) =
        with_proxy("tunnel", Some(root), false, |socket, ca| {
            let mut tls = tunnel_client(socket, ca, &authority);
            tls.write_all(format!("GET /hello HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
                .unwrap();
            let first = response(&mut tls);
            // A Host other than the tunnel's goes nowhere else: the tunnel's host is asked.
            tls.write_all(b"GET /chunked HTTP/1.1\r\nHost: elsewhere.example\r\n\r\n")
                .unwrap();
            let second = response(&mut tls);
            tls.write_all(format!("GET /denied HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
                .unwrap();
            let mut refused = Vec::new();
            let _ = tls.read_to_end(&mut refused);
            (first, second, refused)
        });
    assert!(
        first.starts_with("HTTP/1.1 200 OK\r\nContent-Length: 6\r\n"),
        "{first}"
    );
    assert!(first.ends_with("\r\n\r\nhello\n"), "{first}");
    assert!(
        second.starts_with("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n"),
        "{second}"
    );
    assert_eq!(
        String::from_utf8_lossy(&refused),
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 10\r\nConnection: close\r\n\r\nForbidden\n"
    );
    assert_eq!(
        checked,
        [
            format!("GET https://{authority}/hello"),
            format!("GET https://{authority}/chunked"),
            format!("GET https://{authority}/denied"),
        ]
    );
    let upstream_saw = asked.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        upstream_saw.starts_with("GET /hello HTTP/1.1\r\n"),
        "{upstream_saw}"
    );
    let second_saw = asked.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        second_saw.contains(&format!("\r\nHost: {authority}\r\n")),
        "{second_saw}"
    );
    assert_eq!(
        String::from_utf8(capture.summary()).unwrap(),
        format!(
            "proxy network requests:\n- GET https://{authority}/hello -> 200\n- GET https://{authority}/chunked -> 200\n"
        )
    );
}

/// A body of no length that an upstream's TLS ends without its close_notify may be cut
/// short, and nothing tells: passed on as it came, it is no material (`body_read_failed`),
/// as RFC 9112 §9.8 has a client treat it, where BuildKit's Go client takes the bare end
/// for the body's (D110's recorded difference).
#[test]
fn a_tls_body_ended_bare_is_no_material() {
    let (tls, root) = https_server();
    let (port, _) = upstream(Some(tls), answer);
    let authority = format!("127.0.0.1:{port}");
    let (got, _, capture) = with_proxy("bare", Some(root), false, |socket, ca| {
        let mut tls = tunnel_client(socket, ca, &authority);
        tls.write_all(format!("GET /bare HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
            .unwrap();
        let mut got = Vec::new();
        let _ = tls.read_to_end(&mut got);
        got
    });
    assert_eq!(
        String::from_utf8_lossy(&got),
        "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nabc#cut"
    );
    assert!(capture.materials.is_empty(), "{capture:?}");
    assert_eq!(
        capture.incomplete.first().map(|i| i.reason),
        Some("body_read_failed")
    );
}

/// Reads `c` until `end` comes, or it closes or times out.
fn read_until(c: &mut UnixStream, end: &[u8]) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while !got.ends_with(end) {
        match c.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    got
}

/// A request's head is waited for so long alone (BuildKit's ReadHeaderTimeout, 30 s; here
/// 300 ms), the connection then closed unanswered, as Go's server closes it; the wait
/// between one request and the next is not bounded, only each head from its first byte.
#[test]
fn heads_are_waited_for_so_long_alone() {
    let scratch = scratch("head-time");
    let mut proxy = Proxy::new(scratch.to_path_buf(), &shards_image::store::Limits::none()).unwrap();
    proxy.head_time = Duration::from_millis(300);
    let ((cut, waited, first, second), _, _) = serving(
        &proxy,
        |_| false,
        |socket, _| {
            let mut c = UnixStream::connect(socket).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let t = Instant::now();
            c.write_all(b"GET http://x/ HTTP/1.1\r\nHost: x\r\n").unwrap();
            let mut cut = Vec::new();
            let _ = c.read_to_end(&mut cut);
            let waited = t.elapsed();
            let mut k = UnixStream::connect(socket).unwrap();
            k.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let request = b"GET http://x/ HTTP/1.1\r\nHost: x\r\n\r\n";
            k.write_all(request).unwrap();
            let first = read_until(&mut k, b"Forbidden\n");
            std::thread::sleep(Duration::from_millis(700));
            // The next head in two writes: its time is its own, from its first byte.
            let (start, rest) = request.split_at(10);
            k.write_all(start).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            k.write_all(rest).unwrap();
            let second = read_until(&mut k, b"Forbidden\n");
            (cut, waited, first, second)
        },
    );
    assert!(cut.is_empty(), "{}", String::from_utf8_lossy(&cut));
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_secs(5),
        "{waited:?}"
    );
    assert!(
        first.ends_with(b"Forbidden\n"),
        "{}",
        String::from_utf8_lossy(&first)
    );
    assert!(
        second.ends_with(b"Forbidden\n"),
        "{}",
        String::from_utf8_lossy(&second)
    );
}

/// A step holds so many connections open at once (512; here 2): another waits, unserved,
/// until one of them ends.
#[test]
fn a_step_holds_so_many_connections_at_once() {
    let scratch = scratch("connections");
    let mut proxy = Proxy::new(scratch.to_path_buf(), &shards_image::store::Limits::none()).unwrap();
    proxy.connections = 2;
    let ((before, after), _, _) = serving(
        &proxy,
        |_| false,
        |socket, _| {
            let held: Vec<UnixStream> = (0..2).map(|_| UnixStream::connect(socket).unwrap()).collect();
            // Both taken before the third comes.
            std::thread::sleep(Duration::from_millis(300));
            let mut third = UnixStream::connect(socket).unwrap();
            third
                .write_all(b"GET http://x/ HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            third.set_read_timeout(Some(Duration::from_millis(700))).unwrap();
            let mut buf = [0u8; 64];
            let before = third.read(&mut buf).is_err();
            drop(held);
            third.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            (before, read_until(&mut third, b"Forbidden\n"))
        },
    );
    assert!(before, "a third connection was served while two were held");
    assert!(
        after.ends_with(b"Forbidden\n"),
        "{}",
        String::from_utf8_lossy(&after)
    );
}

/// A request whose question no one answers is refused: none asked where the build's thread
/// has stopped answering, nor one it lets go unanswered (the proxy's own 403, never a
/// request upstream).
#[test]
fn requests_no_one_answers_are_refused() {
    let scratch = scratch("unanswered");
    let proxy = Proxy::new(scratch.to_path_buf(), &shards_image::store::Limits::none()).unwrap();
    let raw = b"GET http://127.0.0.1:9/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    for dropped_before in [true, false] {
        let (session, questions) =
            Session::begin(&proxy, shards_registry::proxy::Proxies::default()).unwrap();
        let (stop, stopped) = UnixStream::pair().unwrap();
        let got = std::thread::scope(|scope| {
            let (session, stopped) = (&session, &stopped);
            std::thread::Builder::new()
                .spawn_scoped(scope, move || session.serve(scope, stopped))
                .unwrap();
            let questions = if dropped_before {
                drop(questions);
                None
            } else {
                Some(questions)
            };
            // The question comes, and is let go unanswered.
            let dropper = std::thread::Builder::new()
                .spawn_scoped(scope, move || {
                    if let Some(q) = questions {
                        use std::os::fd::AsRawFd as _;
                        let mut p = libc::pollfd {
                            fd: q.woken.as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        // SAFETY: poll(2) on one descriptor of ours.
                        unsafe { libc::poll(&mut p, 1, 10_000) };
                        drop(q);
                    }
                })
                .unwrap();
            let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                exchange(proxy.socket(), raw, None)
            }));
            let _ = dropper.join();
            drop(stop);
            session.end();
            got.unwrap_or_else(|p| std::panic::resume_unwind(p))
        });
        assert!(
            String::from_utf8_lossy(&got).starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "{dropped_before}: {}",
            String::from_utf8_lossy(&got)
        );
    }
}

/// An HTTP/1.0 client kept alive is let go after a body of no length, which only the
/// connection's end can end, as Go's server lets it go: its client sees the end at once.
#[test]
fn an_http10_body_of_no_length_ends_its_connection() {
    let (port, _) = upstream(None, answer);
    let (ended, _, _) = with_proxy("http10-unframed", None, false, |socket, _| {
        let mut c = UnixStream::connect(socket).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        c.write_all(
            format!("GET http://127.0.0.1:{port}/big HTTP/1.0\r\nConnection: keep-alive\r\n\r\n").as_bytes(),
        )
        .unwrap();
        let mut got = Vec::new();
        let ended = c.read_to_end(&mut got).is_ok();
        (ended, got)
    });
    let (ended, got) = ended;
    let got = String::from_utf8_lossy(&got);
    assert!(ended, "the connection was kept: {got}");
    assert!(
        got.starts_with("HTTP/1.0 200 OK\r\n") && got.ends_with("#close"),
        "{got}"
    );
}

/// A response's hop-by-hop fields go, each of RFC 9110 §7.6.1's and each its `Connection`
/// names, whatever their case; the rest stay, sorted (D110's recorded difference).
#[test]
fn a_responses_hop_fields_go() {
    let fields: Vec<(String, String)> = [
        ("x-kept", "2"),
        ("keep-alive", "timeout=5"),
        ("Proxy-Authenticate", "Basic"),
        ("Upgrade", "h2c"),
        ("TE", "trailers"),
        ("Trailer", "X-T"),
        ("Proxy-Connection", "keep-alive"),
        ("Transfer-Encoding", "chunked"),
        ("connection", "close, x-Named"),
        ("X-Named", "1"),
        ("A-Kept", "1"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(
        passed_on(&fields, |_| true),
        [
            ("A-Kept".to_string(), "1".to_string()),
            ("X-Kept".to_string(), "2".to_string())
        ]
    );
}

/// A request whose body cannot be read is a 502, recorded as an upstream error, as Go's
/// transport fails to send such a body on and BuildKit's handler answers it (the text is
/// the client's own, D110).
#[test]
fn a_body_that_cannot_be_read_is_a_502() {
    let (got, _, capture) = with_proxy("bad-body", None, false, |socket, _| {
        exchange_closing(
            socket,
            b"POST http://127.0.0.1:9/f HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhello\r\n0\r\n\r\n",
        )
    });
    let got = String::from_utf8_lossy(&got);
    assert!(got.starts_with("HTTP/1.1 502 Bad Gateway\r\n"), "{got}");
    assert_eq!(capture.requests.first().map(|r| r.status), Some(502));
    assert_eq!(
        capture.incomplete.first().map(|i| i.reason),
        Some("upstream_error")
    );
}

/// A request's body is held no further than the room the build's host keeps
/// (SHARDS_KEEP_FREE): past it, the request is refused (413) and its file is gone, so a
/// step never fills the host's disk through the proxy.
#[test]
fn request_bodies_stop_short_of_the_room_kept() {
    static LOOKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let limits = shards_image::store::Limits {
        keep_free: 1,
        // Room as the proxy starts; none when it looks again, 64 MiB on.
        available: |_| {
            Ok(if LOOKS.fetch_add(1, Ordering::SeqCst) == 0 {
                u64::MAX / 2
            } else {
                0
            })
        },
        ..shards_image::store::Limits::none()
    };
    let scratch = scratch("room");
    let dir = scratch.to_path_buf();
    let mut proxy = Proxy::new(dir.clone(), &limits).unwrap();
    proxy.reach = |_| true;
    let (got, checked, _) = serving(
        &proxy,
        |_| true,
        |socket, _| {
            let mut raw = format!(
                "POST http://127.0.0.1:9/upload HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
                65u64 << 20
            )
            .into_bytes();
            raw.resize(raw.len() + (65 << 20), b'u');
            // The proxy says nothing until it has held 64 MiB, which a busy host takes its
            // time over.
            exchange_closing_within(socket, &raw, Duration::from_secs(600))
        },
    );
    assert_eq!(checked, ["POST http://127.0.0.1:9/upload"]);
    let got = String::from_utf8_lossy(&got);
    assert!(
        got.starts_with("HTTP/1.1 413 Request Entity Too Large\r\n")
            && got.ends_with("\r\nConnection: close\r\n\r\nrequest body larger than the build's host has room for (SHARDS_KEEP_FREE)\n"),
        "{got}"
    );
    assert_eq!(LOOKS.load(Ordering::SeqCst), 2);
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["proxy"]);
}

/// An upstream whose certificate the host does not trust is a 502 in the tunnel, never
/// passed on as trusted.
#[test]
fn upstream_tls_is_never_weakened() {
    let (tls, _) = https_server();
    let (port, asked) = upstream(Some(tls), answer);
    let authority = format!("127.0.0.1:{port}");
    let (answered, _, capture) = with_proxy("untrusted", None, false, |socket, ca| {
        let mut tls = tunnel_client(socket, ca, &authority);
        tls.write_all(format!("GET /hello HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
            .unwrap();
        let mut got = Vec::new();
        let _ = tls.read_to_end(&mut got);
        got
    });
    let answered = String::from_utf8_lossy(&answered);
    assert!(
        answered.starts_with("HTTP/1.1 502 Bad Gateway\r\nContent-Length: "),
        "{answered}"
    );
    assert!(answered.contains("\r\nConnection: close\r\n\r\n"), "{answered}");
    assert!(
        asked.recv_timeout(Duration::from_millis(200)).is_err(),
        "a request went to an untrusted server"
    );
    assert_eq!(
        capture.incomplete.first().map(|i| i.reason),
        Some("upstream_error")
    );
}

/// What BuildKit v0.33.0's proxy made of the oracle's cases (scripts/proxy/generate).
fn oracle() -> serde_json::Value {
    serde_json::from_str(include_str!("../testdata/proxy-oracle.json")).unwrap()
}

/// A case's bytes as the oracle says them: text, base64, or a prefix, one byte many
/// times, and a suffix.
fn oracle_bytes(v: &serde_json::Value) -> Vec<u8> {
    use base64::Engine as _;
    if let Some(s) = v.as_str() {
        return s.as_bytes().to_vec();
    }
    if let Some(b) = v.get("base64") {
        return base64::engine::general_purpose::STANDARD
            .decode(b.as_str().unwrap())
            .unwrap();
    }
    let mut out = v["prefix"].as_str().unwrap().as_bytes().to_vec();
    let fill = v["fill"].as_str().unwrap().as_bytes()[0];
    out.extend(std::iter::repeat_n(
        fill,
        usize::try_from(v["times"].as_u64().unwrap()).unwrap(),
    ));
    out.extend_from_slice(v["suffix"].as_str().unwrap().as_bytes());
    out
}

/// Each `Date` field of an HTTP date said as `DATE`, as the oracle says it.
fn go_dated(b: &[u8]) -> String {
    let mut out = Vec::with_capacity(b.len());
    let mut rest = b;
    loop {
        let (line, next) = match rest.windows(2).position(|w| w == b"\r\n") {
            Some(at) => (&rest[..at], Some(&rest[at + 2..])),
            None => (rest, None),
        };
        // "Date: " and http.TimeFormat's 29 bytes, `Mon, 02 Jan 2006 15:04:05 GMT`.
        if line.starts_with(b"Date: ") && line.len() == 35 && line.ends_with(b" GMT") {
            out.extend_from_slice(b"Date: DATE");
        } else {
            out.extend_from_slice(line);
        }
        match next {
            Some(n) => {
                out.extend_from_slice(b"\r\n");
                rest = n;
            }
            None => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Writes `raw` on a new connection and ends its writing, reading until the proxy closes.
fn exchange_closing(socket: &Path, raw: &[u8]) -> Vec<u8> {
    exchange_closing_within(socket, raw, Duration::from_secs(20))
}

/// [`exchange_closing`], each read waiting `wait` at most.
fn exchange_closing_within(socket: &Path, raw: &[u8], wait: Duration) -> Vec<u8> {
    let c = UnixStream::connect(socket).unwrap();
    c.set_read_timeout(Some(wait)).unwrap();
    let w = c.try_clone().unwrap();
    std::thread::scope(|s| {
        s.spawn(move || {
            let _ = (&w).write_all(raw);
            let _ = w.shutdown(std::net::Shutdown::Write);
        });
        let mut got = Vec::new();
        let _ = (&c).read_to_end(&mut got);
        got
    })
}

/// The URLs the policies were asked, each check's method left out.
fn urls_of(checked: &[String]) -> Vec<String> {
    checked
        .iter()
        .map(|c| c.split_once(' ').map_or(c.as_str(), |(_, u)| u).to_string())
        .collect()
}

/// Each raw request, sent to the proxy and every one refused, is read, asked of the
/// policies and answered as BuildKit's handler on Go's server reads, asks and answers it,
/// byte for byte, but where D110 records shards' difference: a field value that is not
/// UTF-8 (obs-text) is refused, as shards' client writes fields as text.
#[test]
fn requests_are_read_as_buildkits_proxy_reads_them() {
    let oracle = oracle();
    let mut failed = Vec::new();
    for (i, case) in oracle["plain"].as_array().unwrap().iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let (got, checked, _) = with_proxy_deciding(
            &format!("oracle-plain-{i}"),
            None,
            true,
            |_| false,
            |socket, _| exchange_closing(socket, &oracle_bytes(&case["request"])),
        );
        let asked = urls_of(&checked);
        if name == "a value not UTF-8" {
            assert_eq!(
                (go_dated(&got).as_str(), asked.len()),
                (
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n400 Bad Request",
                    0
                )
            );
            continue;
        }
        let want: Vec<String> = case["checked"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u.as_str().unwrap().to_string())
            .collect();
        let response = go_dated(&oracle_bytes(&case["response"]));
        if asked != want || go_dated(&got) != response {
            failed.push(format!(
                "{name}:\n  asked {asked:?}\n  want  {want:?}\n  got  {:?}\n  want {response:?}",
                go_dated(&got)
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// Each CONNECT is answered, and each request in its tunnel asked of the policies (every
/// one refused) and answered, as BuildKit's proxy does, byte for byte.
/// Sends `connect`, reads the proxy's answer to it, and where it established the tunnel,
/// writes `raw` in TLS to `server_name` (trusting `ca`), ends its writing (close_notify),
/// and reads until the proxy closes: the answer, whether the handshake was made, what came
/// in the tunnel, and whether the tunnel ended with its close_notify.
fn tunnel_exchange(
    socket: &Path,
    ca: &[u8],
    connect: &[u8],
    server_name: &str,
    raw: &[u8],
) -> (Vec<u8>, bool, Vec<u8>, bool) {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls::pki_types::pem::PemObject::pem_slice_iter(ca) {
        roots.add(cert.unwrap()).unwrap();
    }
    let config = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let mut c = UnixStream::connect(socket).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    c.write_all(connect).unwrap();
    let mut established = Vec::new();
    let mut b = [0u8; 1];
    while !established.ends_with(b"\r\n\r\n") && c.read(&mut b).unwrap_or(0) == 1 {
        established.push(b[0]);
    }
    if !established.starts_with(b"HTTP/1.1 200 ") {
        let _ = c.read_to_end(&mut established);
        return (established, false, Vec::new(), false);
    }
    let name = rustls::pki_types::ServerName::try_from(server_name.to_string()).unwrap();
    let mut tls = rustls::StreamOwned::new(rustls::ClientConnection::new(config, name).unwrap(), c);
    let mut response = Vec::new();
    let handshake = tls.conn.complete_io(&mut tls.sock).is_ok();
    let mut ended = false;
    if handshake {
        let _ = tls.write_all(raw);
        tls.conn.send_close_notify();
        let _ = tls.flush();
        ended = tls.read_to_end(&mut response).is_ok();
    }
    (established, handshake, response, ended)
}

#[test]
fn tunnels_are_read_as_buildkits_proxy_reads_them() {
    let oracle = oracle();
    let mut failed = Vec::new();
    for (i, case) in oracle["tunnels"].as_array().unwrap().iter().enumerate() {
        let name = case["name"].as_str().unwrap();
        let ((established, handshake, response, ended), checked, _) = with_proxy_deciding(
            &format!("oracle-tunnel-{i}"),
            None,
            true,
            |_| false,
            |socket, ca| {
                tunnel_exchange(
                    socket,
                    ca,
                    case["connect"].as_str().unwrap().as_bytes(),
                    case["server_name"].as_str().unwrap(),
                    &oracle_bytes(&case["request"]),
                )
            },
        );
        let asked = urls_of(&checked);
        let want: Vec<String> = case["checked"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u.as_str().unwrap().to_string())
            .collect();
        let want_response = go_dated(&oracle_bytes(&case["response"]));
        // Every tunnel made ends with its TLS's close_notify, as Go's tls.Conn.Close ends
        // BuildKit's.
        if go_dated(&established) != case["established"].as_str().unwrap()
            || handshake != case["handshake"].as_bool().unwrap()
            || ended != handshake
            || asked != want
            || go_dated(&response) != want_response
        {
            failed.push(format!(
                "{name}:\n  established {:?} handshake {handshake} ended {ended}\n  asked {asked:?}\n  want  {want:?}\n  got  {:?}\n  want {want_response:?}",
                go_dated(&established),
                go_dated(&response)
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The oracle's upstream: what it answers, raw, by the path asked.
static ORACLE_UPSTREAM: std::sync::OnceLock<HashMap<String, Vec<u8>>> = std::sync::OnceLock::new();

fn oracle_answer(_method: &str, target: &str) -> Vec<u8> {
    let table = ORACLE_UPSTREAM.get_or_init(|| {
        oracle()["upstream"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(path, raw)| (path.clone(), oracle_bytes(raw)))
            .collect()
    });
    let path = target.split('?').next().unwrap_or_default();
    table
        .get(path)
        .cloned()
        .unwrap_or_else(|| b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec())
}

/// What a case's capture came to, as the oracle says it: requests, materials by URL, and
/// incomplete requests, each address said as `{upstream}` or `{upstream_tls}`.
fn capture_as_oracle(c: &Capture, n: &dyn Fn(&str) -> String) -> serde_json::Value {
    let mut materials = c.materials();
    materials.sort_by(|a, b| a.url.cmp(&b.url));
    serde_json::json!({
        "requests": c.requests.iter().map(|r| serde_json::json!({
            "method": r.method, "url": n(&r.url), "redirect": n(&r.redirect), "status": r.status,
        })).collect::<Vec<_>>(),
        "materials": materials.iter().map(|m| serde_json::json!({"url": n(&m.url), "digest": m.digest})).collect::<Vec<_>>(),
        "incomplete": c.incomplete.iter().map(|i| serde_json::json!({
            "method": i.method, "url": n(&i.url), "reason": i.reason,
        })).collect::<Vec<_>>(),
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Each chunked body said as one chunk: where a body is cut into chunks follows each
/// proxy's reads of its upstream (Go's too: after its first 512 bytes, each read), which
/// carries nothing a client may rely on (RFC 9112 §7.1); D110 records it.
fn one_chunk(b: &str) -> String {
    const TE: &[u8] = b"Transfer-Encoding: chunked\r\n";
    let b = b.as_bytes();
    let mut out = Vec::new();
    let mut rest = b;
    while let Some(te) = find(rest, TE) {
        let Some(end) = find(&rest[te..], b"\r\n\r\n").map(|e| te + e + 4) else {
            break;
        };
        out.extend_from_slice(&rest[..end]);
        rest = &rest[end..];
        let mut data = Vec::new();
        while let Some(eol) = find(rest, b"\r\n") {
            let Ok(n) = usize::from_str_radix(std::str::from_utf8(&rest[..eol]).unwrap_or("x"), 16) else {
                break;
            };
            if n == 0 {
                rest = &rest[(eol + 4).min(rest.len())..];
                break;
            }
            data.extend_from_slice(&rest[eol + 2..eol + 2 + n]);
            rest = &rest[(eol + 4 + n).min(rest.len())..];
        }
        out.extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\r\n0\r\n\r\n");
    }
    out.extend_from_slice(rest);
    String::from_utf8_lossy(&out).into_owned()
}

/// What shards' proxy answers where D110 records a difference from BuildKit's, by case.
fn passed_differently(kind: &str, name: &str) -> Option<&'static str> {
    match (kind, name) {
        // The hop's own fields go, as RFC 9110 §7.6.1 has a proxy remove them: Connection
        // and what it names (X-Hop), Keep-Alive, Proxy-Authenticate, Upgrade.
        ("passed", "hop-by-hop fields") => {
            Some("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\nDate: DATE\r\n\r\nok")
        }
        ("passed_tunnels", "hop-by-hop fields") => {
            Some("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\nok")
        }
        _ => None,
    }
}

/// Requests the policies let go (all but `/denied`), plain and in a tunnel, to upstreams
/// answering as the oracle's did: what the client read, what the policies were asked, and
/// what the proxy recorded of each, as BuildKit's proxy on Go's server passes them on and
/// records them, byte for byte, but for chunk boundaries and what D110 records.
#[test]
fn requests_are_passed_on_as_buildkits_proxy_passes_them() {
    let oracle = oracle();
    let (port, _) = upstream(None, oracle_answer);
    let (tls, root) = https_server();
    let (sport, _) = upstream(Some(tls), oracle_answer);
    let (plain, secure) = (format!("127.0.0.1:{port}"), format!("127.0.0.1:{sport}"));
    let n = |s: &str| s.replace(&secure, "{upstream_tls}").replace(&plain, "{upstream}");
    let fill = |s: &str| s.replace("{upstream_tls}", &secure).replace("{upstream}", &plain);
    let mut failed = Vec::new();
    for (kind, cases) in [
        ("passed", &oracle["passed"]),
        ("passed_tunnels", &oracle["passed_tunnels"]),
    ] {
        for (i, case) in cases.as_array().unwrap().iter().enumerate() {
            let name = case["name"].as_str().unwrap();
            let raw = fill(case["request"].as_str().unwrap());
            let (got, checked, capture) = with_proxy_deciding(
                &format!("oracle-{kind}-{i}"),
                Some(root.clone()),
                false,
                |url| !url.contains("/denied"),
                |socket, ca| {
                    if kind == "passed" {
                        exchange_closing(socket, raw.as_bytes())
                    } else {
                        let connect = format!("CONNECT {secure} HTTP/1.1\r\nHost: {secure}\r\n\r\n");
                        let (_, handshake, response, ended) =
                            tunnel_exchange(socket, ca, connect.as_bytes(), "127.0.0.1", raw.as_bytes());
                        assert!(handshake && ended, "{name}: handshake {handshake}, ended {ended}");
                        response
                    }
                },
            );
            let asked: Vec<String> = urls_of(&checked).iter().map(|u| n(u)).collect();
            let want: Vec<String> = case["checked"]
                .as_array()
                .unwrap()
                .iter()
                .map(|u| u.as_str().unwrap().to_string())
                .collect();
            let mut response = one_chunk(&n(&go_dated(&got)));
            let mut want_response = match passed_differently(kind, name) {
                Some(r) => r.to_string(),
                None => one_chunk(&go_dated(&oracle_bytes(&case["response"]))),
            };
            // A 502's text is each client's own error (D110): its status line alone is held.
            if want_response.starts_with("HTTP/1.1 502 ") {
                let line = |r: &str| r.split("\r\n").next().unwrap_or_default().to_string();
                (response, want_response) = (line(&response), line(&want_response));
            }
            let recorded = capture_as_oracle(&capture, &n);
            if asked != want || response != want_response || recorded != case["capture"] {
                failed.push(format!(
                    "{kind} {name}:\n  asked {asked:?}\n  want  {want:?}\n  got  {response:?}\n  want {want_response:?}\n  recorded {recorded}\n  want     {}",
                    case["capture"]
                ));
            }
        }
    }
    assert!(
        failed.is_empty(),
        "{} of them:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// The build's thread for a measurement: each question answered as it comes, woken as the
/// builder is (`poll` on the session's socket, as `Builder::wait_frame` waits), by a real
/// policy (`allow if input.http`, evaluated as a build evaluates one) where `policy`, else
/// at once; until `done`.
/// One request a measurement makes, each time it is called.
type Measured<'a> = Box<dyn Fn() + 'a>;

fn answering_as_a_build(q: Questions, done: &AtomicBool, policy: bool) {
    use super::super::super::policy;
    use std::os::fd::AsRawFd as _;
    struct Quiet;
    impl policy::Log for Quiet {
        fn line(&self, _: &str) {}
        fn fetch(&self, _: &str, _: &str, _: Option<&str>) -> Result<Vec<u8>, String> {
            Err("no fetch".into())
        }
    }
    struct Nothing;
    impl policy::Resolve for Nothing {
        fn resolve(&self, s: &policy::Source, _: &policy::MetaRequest) -> Result<policy::Meta, String> {
            Err(format!("{}: no metadata", s.identifier))
        }
    }
    let dir = scratch("policy");
    let policies = policy::Policies::configure(policy::Setup {
        default: policy::Opt {
            files: vec![policy::FileSpec {
                filename: "Dockerfile.rego".into(),
                optional: true,
                data: Some(
                    b"package docker\n\ndefault allow := false\n\nallow if input.http\n\ndecision := {\"allow\": allow}\n"
                        .to_vec(),
                ),
            }],
            context_dir: Some(dir.to_path_buf()),
            ..policy::Opt::default()
        },
        configs: &[],
        env: policy::Env::default(),
        cwd: dir.to_path_buf(),
        default_platform: shards_dockerfile::platform::Platform::new("linux", "arm64"),
        debug: false,
        default_policy: false,
        remote: None,
    })
    .unwrap()
    .unwrap();
    while !done.load(Ordering::SeqCst) {
        let mut p = libc::pollfd {
            fd: q.woken.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll(2) on one descriptor of ours, 10 ms at most, to see `done`.
        unsafe { libc::poll(&mut p, 1, 10) };
        q.answer(&mut |_, url| {
            !policy
                || matches!(
                    policies.evaluate(&policy::Source::new(url), None, &Nothing, &Quiet),
                    Ok(None)
                )
        });
    }
}

/// PM M150: what a request through the proxy costs against the same request straight to
/// its server, each on a connection of its own, as a step's `curl` makes them: plain HTTP,
/// its question answered at once and by a real policy; HTTPS, straight with TLS against
/// through CONNECT (the tunnel's TLS, then the proxy's own to the server). Each kind in
/// turn, so a busy host weighs on all alike. Microseconds, client side.
#[test]
#[ignore = "a measurement: docs/research/measurements/build-proxy/run.sh"]
fn request_costs() {
    let n = 1000;
    let quantiles = |mut took: Vec<u128>| {
        took.sort_unstable();
        let q = |p: usize| took[(took.len() * p / 100).min(took.len() - 1)];
        format!(
            "p50 {} p90 {} p99 {} max {}",
            q(50),
            q(90),
            q(99),
            took[took.len() - 1]
        )
    };
    let (port, _) = upstream(None, answer);
    let (tls, root) = https_server();
    let (sport, _) = upstream(Some(tls), answer);
    let read_all = |s: &mut dyn Read| {
        let mut got = Vec::new();
        let _ = s.read_to_end(&mut got);
        assert!(got.ends_with(b"hello\n"), "{}", String::from_utf8_lossy(&got));
    };
    let direct = || {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(
            format!("GET /hello HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
        read_all(&mut c);
    };
    let roots = {
        let mut r = rustls::RootCertStore::empty();
        r.add(root.clone()).unwrap();
        Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(r)
            .with_no_client_auth(),
        )
    };
    let direct_tls = || {
        let c = TcpStream::connect(("127.0.0.1", sport)).unwrap();
        let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
        let mut tls =
            rustls::StreamOwned::new(rustls::ClientConnection::new(roots.clone(), name).unwrap(), c);
        tls.write_all(
            format!("GET /hello HTTP/1.1\r\nHost: 127.0.0.1:{sport}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
        read_all(&mut tls);
    };
    let mut timings: Vec<(&str, Vec<u128>)> = Vec::new();
    for policy in [false, true] {
        let scratch = scratch("measure");
        let mut proxy = Proxy::new(scratch.to_path_buf(), &shards_image::store::Limits::none()).unwrap();
        proxy.tls = shards_registry::tls::client_config(vec![root.clone()], None).unwrap();
        proxy.reach = |_| true;
        let (session, questions) =
            Session::begin(&proxy, shards_registry::proxy::Proxies::default()).unwrap();
        let (stop, stopped) = UnixStream::pair().unwrap();
        let done = AtomicBool::new(false);
        let socket = proxy.socket().to_path_buf();
        let ca = proxy.ca().to_vec();
        let mut kinds: Vec<(&str, Measured<'_>)> = Vec::new();
        if !policy {
            kinds.push(("plain, straight", Box::new(direct)));
            kinds.push(("HTTPS, straight", Box::new(direct_tls)));
        }
        let plain_name = if policy {
            "plain, through the proxy, a policy checking it"
        } else {
            "plain, through the proxy"
        };
        let s = socket.clone();
        kinds.push((
            plain_name,
            Box::new(move || {
                let mut c = UnixStream::connect(&s).unwrap();
                c.write_all(
                    format!(
                        "GET http://127.0.0.1:{port}/hello HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
                read_all(&mut c);
            }),
        ));
        if !policy {
            let authority = format!("127.0.0.1:{sport}");
            kinds.push((
                "HTTPS, through the proxy's tunnel",
                Box::new(move || {
                    let mut tls = tunnel_client(&socket, &ca, &authority);
                    tls.write_all(
                        format!("GET /hello HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                            .as_bytes(),
                    )
                    .unwrap();
                    read_all(&mut tls);
                }),
            ));
        }
        std::thread::scope(|scope| {
            let (session, stopped, done) = (&session, &stopped, &done);
            std::thread::Builder::new()
                .spawn_scoped(scope, move || session.serve(scope, stopped))
                .unwrap();
            std::thread::Builder::new()
                .spawn_scoped(scope, move || answering_as_a_build(questions, done, policy))
                .unwrap();
            let mut took: Vec<Vec<u128>> = vec![Vec::with_capacity(n); kinds.len()];
            for _ in 0..n {
                for (k, (_, f)) in kinds.iter().enumerate() {
                    let t = Instant::now();
                    f();
                    took[k].push(t.elapsed().as_micros());
                }
            }
            for ((name, _), t) in kinds.iter().zip(took) {
                timings.push((name, t));
            }
            done.store(true, Ordering::SeqCst);
            drop(stop);
            session.end();
        });
    }
    for (name, t) in timings {
        println!("{name}: n={n} {}", quantiles(t));
    }
}

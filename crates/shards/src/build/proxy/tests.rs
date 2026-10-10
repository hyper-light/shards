//! The proxy, served in-process to a client on its socket, its requests going to servers
//! of the tests' own (one of them HTTPS, under a CA these tests trust upstream), its
//! questions answered as a build's thread answers them: `/denied` refused.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use super::*;
use std::net::TcpListener;

/// A directory of its own, removed with the proxy.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shards-proxy-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
                    let _ = said.send(format!("{head}{}", String::from_utf8_lossy(&body)));
                    let reply = answer(&method, &target);
                    if r.get_mut().write_all(&reply).is_err() || reply.ends_with(b"#close") {
                        return;
                    }
                    let _ = r.get_mut().flush();
                }
            });
        }
    });
    (port, asked)
}

trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

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
        _ => b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
    }
}

/// What the build's thread does: answers each question, `/denied` refused, until `done`.
fn answering(q: &Questions, done: &AtomicBool, checked: &Mutex<Vec<String>>) {
    while !done.load(Ordering::SeqCst) {
        q.answer(&mut |method, url| {
            checked.lock().unwrap().push(format!("{method} {url}"));
            !url.contains("/denied")
        });
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Runs `client` against a proxy, answering its questions; returns what it returned, the
/// checks put to the policies, and the capture. Its requests may reach the loopback, where
/// the tests' servers are, unless `host_kept` (as a build's proxy is made).
fn with_proxy<T: Send>(
    name: &str,
    roots: Option<rustls::pki_types::CertificateDer<'static>>,
    host_kept: bool,
    client: impl FnOnce(&Path, &[u8]) -> T + Send,
) -> (T, Vec<String>, Capture) {
    let mut proxy = Proxy::new(scratch(name), 1 << 20).unwrap();
    if let Some(root) = roots {
        proxy.tls = shards_registry::tls::client_config(vec![root], None).unwrap();
    }
    if !host_kept {
        proxy.reach = |_| true;
    }
    let (session, questions) = Session::begin(&proxy, shards_registry::proxy::Proxies::default()).unwrap();
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
            .spawn_scoped(scope, move || answering(&questions, done, checked))
            .unwrap();
        let out = client(proxy.socket(), proxy.ca());
        done.store(true, Ordering::SeqCst);
        drop(stop);
        session.end();
        out
    });
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
    // A short chunked body has its length, as Go's server gives one it holds whole.
    assert_eq!(
        dated(&chunked),
        "HTTP/1.1 200 OK\nDate: Mon, 02 Jan 2006 15:04:05 GMT\nContent-Length: 3\nConnection: close\n\nabc"
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

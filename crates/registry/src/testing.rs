//! Test registries: a CA made for each test, and loopback servers that answer with
//! scripted bytes.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

/// A day's validity from an hour ago: macOS refuses server certificates valid for over
/// 825 days, even under a custom root.
fn params(sans: Vec<String>) -> CertificateParams {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let mut p = CertificateParams::new(sans).unwrap();
    p.not_before = time::OffsetDateTime::from_unix_timestamp(now - 3600).unwrap();
    p.not_after = time::OffsetDateTime::from_unix_timestamp(now + 86400).unwrap();
    p
}

/// A registry CA, and the server configuration of a host it certifies as `localhost`.
pub(crate) fn registry(
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> (CertificateDer<'static>, Arc<ServerConfig>) {
    registry_named("localhost", versions)
}

/// [`registry`], certifying the host as `name`.
pub(crate) fn registry_named(
    name: &str,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> (CertificateDer<'static>, Arc<ServerConfig>) {
    let mut ca = params(Vec::new());
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    ca.distinguished_name
        .push(DnType::CommonName, "shards test registry CA");
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
    let mut leaf = params(vec![name.into()]);
    leaf.distinguished_name.push(DnType::CommonName, name);
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let key = KeyPair::generate().unwrap();
    let cert = leaf.signed_by(&key, &ca).unwrap();
    let server = ServerConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_protocol_versions(versions)
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .unwrap();
    (ca.der().clone(), Arc::new(server))
}

/// A CA, a client certificate it issued (chain and key, as PEM), and the configuration
/// of a `localhost` server that demands a certificate from that CA.
pub(crate) fn demanding_registry() -> (CertificateDer<'static>, String, String, Arc<ServerConfig>) {
    let mut ca = params(Vec::new());
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    ca.distinguished_name.push(DnType::CommonName, "shards test CA");
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
    let issue = |sans: Vec<String>, usage: ExtendedKeyUsagePurpose| {
        let mut leaf = params(sans);
        leaf.extended_key_usages = vec![usage];
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let key = KeyPair::generate().unwrap();
        (leaf.signed_by(&key, &ca).unwrap(), key)
    };
    let (server_cert, server_key) = issue(vec!["localhost".into()], ExtendedKeyUsagePurpose::ServerAuth);
    let (client_cert, client_key) = issue(Vec::new(), ExtendedKeyUsagePurpose::ClientAuth);
    let provider = Arc::new(aws_lc_rs::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let verifier =
        rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .unwrap();
    let server = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![server_cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
        )
        .unwrap();
    (
        ca.der().clone(),
        client_cert.pem(),
        client_key.serialize_pem(),
        Arc::new(server),
    )
}

/// What the server does with a connection after a scripted response.
#[derive(Debug, Clone, Copy)]
pub(crate) enum After {
    Keep,
    Close,
}

trait Io: Read + Write {}
impl<T: Read + Write> Io for T {}

/// A connection a server holds: its socket, to shut down, and its thread.
type Held = (TcpStream, std::thread::JoinHandle<()>);

/// A scripted server: its port, the connections it accepted, and every request it read.
/// Dropped, it stops: its threads end and are joined, its connections closed.
pub(crate) struct Server {
    pub port: u16,
    accepted: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    /// Set as the server goes; its accept loop then returns.
    stopping: Arc<AtomicBool>,
    /// The connections in hand, to shut down as the server goes, and their threads.
    conns: Arc<Mutex<Vec<Held>>>,
    acceptor: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Wakes the accept loop, which then sees it is stopping.
        drop(TcpStream::connect(("127.0.0.1", self.port)));
        if let Some(acceptor) = self.acceptor.take() {
            let _ = acceptor.join();
        }
        let conns = std::mem::take(
            &mut *self
                .conns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for (tcp, thread) in conns {
            let _ = tcp.shutdown(Shutdown::Both);
            let _ = thread.join();
        }
    }
}

impl Server {
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Every request it has read, head and body.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// A request as a test server read it.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A loopback server answering each request with the next scripted response.
pub(crate) fn serve(tls: Option<Arc<ServerConfig>>, script: Vec<(Vec<u8>, After)>) -> Server {
    let script = Mutex::new(script.into_iter());
    route(tls, move |_| script.lock().unwrap().next())
}

/// A loopback server answering each request with what `answer` makes of it, each
/// connection on its own thread. `None` closes the connection unanswered.
pub(crate) fn route(
    tls: Option<Arc<ServerConfig>>,
    answer: impl Fn(&Seen) -> Option<(Vec<u8>, After)> + Send + Sync + 'static,
) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stopping = Arc::new(AtomicBool::new(false));
    let conns = Arc::new(Mutex::new(Vec::new()));
    let (count, seen, stop, held) = (
        accepted.clone(),
        requests.clone(),
        stopping.clone(),
        conns.clone(),
    );
    let answer = Arc::new(answer);
    let acceptor = std::thread::spawn(move || {
        for tcp in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let Ok(tcp) = tcp else { return };
            count.fetch_add(1, Ordering::SeqCst);
            let Ok(ours) = tcp.try_clone() else { return };
            let (tls, seen, answer) = (tls.clone(), seen.clone(), answer.clone());
            let Ok(closer) = tcp.try_clone() else { return };
            let thread = std::thread::spawn(move || {
                // However it ends, the connection closes with it: the server's clone,
                // kept to shut it down, would otherwise hold it open.
                struct Closes(TcpStream);
                impl Drop for Closes {
                    fn drop(&mut self) {
                        let _ = self.0.shutdown(Shutdown::Both);
                    }
                }
                let _closes = Closes(closer);
                let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
                // A plain server asked for TLS answers as Go's net/http answers a request
                // it cannot read, as a registry does (server.go, publicErr).
                let mut first = [0u8; 1];
                if tls.is_none() && tcp.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
                    let _ = (&tcp).write_all(GO_BAD_REQUEST);
                    return;
                }
                let mut io: Box<dyn Io> = match &tls {
                    Some(config) => Box::new(StreamOwned::new(
                        ServerConnection::new(config.clone()).unwrap(),
                        tcp,
                    )),
                    None => Box::new(tcp),
                };
                while let Ok(request) = read_request(&mut io) {
                    let text = String::from_utf8_lossy(&request).into_owned();
                    seen.lock().unwrap().push(text.clone());
                    let Some((response, after)) = answer(&parse(&text)) else {
                        return;
                    };
                    let _ = io.write_all(&response).and_then(|()| io.flush());
                    if let After::Close = after {
                        return;
                    }
                }
            });
            held.lock().unwrap().push((ours, thread));
        }
    });
    Server {
        port,
        accepted,
        requests,
        stopping,
        conns,
        acceptor: Some(acceptor),
    }
}

/// What Go's net/http writes to a connection whose request it cannot read.
pub(crate) const GO_BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n400 Bad Request";

fn parse(text: &str) -> Seen {
    let head = text.split("\r\n\r\n").next().unwrap_or_default();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_string();
    let target = first.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.to_string(), v.trim().to_string()))
        .collect();
    Seen {
        method,
        target,
        headers,
    }
}

/// Reads one request: its head, then as many body bytes as its Content-Length says.
fn read_request(io: &mut Box<dyn Io>) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if io.read(&mut byte)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let length = text
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map(|v| v.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    io.read_exact(&mut body)?;
    head.extend(body);
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server dropped is gone: its accept loop has returned and its listener closed, so
    /// its port refuses connections, and a connection it held is closed.
    #[test]
    fn a_dropped_server_leaves_nothing_running() {
        let server = serve(
            None,
            vec![(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
                After::Keep,
            )],
        );
        let port = server.port;
        let mut held = TcpStream::connect(("127.0.0.1", port)).unwrap();
        held.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let mut answer = [0u8; 38];
        held.read_exact(&mut answer).unwrap();
        drop(server);
        assert_eq!(
            TcpStream::connect(("127.0.0.1", port))
                .map_err(|e| e.kind())
                .err(),
            Some(io::ErrorKind::ConnectionRefused)
        );
        held.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        assert_eq!(
            held.read(&mut [0u8; 1]).unwrap(),
            0,
            "the connection it held is closed"
        );
    }
}

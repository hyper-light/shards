//! Test registries: a CA made for each test, and loopback servers that answer with
//! scripted bytes.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    let mut ca = params(Vec::new());
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    ca.distinguished_name
        .push(DnType::CommonName, "shards test registry CA");
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().unwrap()).unwrap();
    let mut leaf = params(vec!["localhost".into()]);
    leaf.distinguished_name.push(DnType::CommonName, "localhost");
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

/// What the server does with a connection after a scripted response.
#[derive(Debug, Clone, Copy)]
pub(crate) enum After {
    Keep,
    Close,
}

trait Io: Read + Write {}
impl<T: Read + Write> Io for T {}

/// A scripted server: its port, the connections it accepted, and every request it read.
pub(crate) struct Server {
    pub port: u16,
    accepted: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
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

/// A loopback server answering each request with the next scripted response.
pub(crate) fn serve(tls: Option<Arc<ServerConfig>>, script: Vec<(Vec<u8>, After)>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (count, seen) = (accepted.clone(), requests.clone());
    std::thread::spawn(move || {
        let mut script = script.into_iter();
        'accept: for tcp in listener.incoming() {
            let Ok(tcp) = tcp else { return };
            count.fetch_add(1, Ordering::SeqCst);
            let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
            let mut io: Box<dyn Io> = match &tls {
                Some(config) => Box::new(StreamOwned::new(
                    ServerConnection::new(config.clone()).unwrap(),
                    tcp,
                )),
                None => Box::new(tcp),
            };
            loop {
                match read_request(&mut io) {
                    Ok(request) => seen
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&request).into_owned()),
                    Err(_) => continue 'accept,
                }
                let Some((response, after)) = script.next() else {
                    return;
                };
                let _ = io.write_all(&response).and_then(|()| io.flush());
                if let After::Close = after {
                    continue 'accept;
                }
            }
        }
    });
    Server {
        port,
        accepted,
        requests,
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

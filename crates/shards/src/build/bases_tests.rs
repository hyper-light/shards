//! Where a build's base comes from when the store already holds it (D78): from the store,
//! no registry asked. BuildKit takes a digest its content store holds from there too (v0.28.1
//! util/imageutil/config.go `Config`, util/pull/pull.go `tryLocalResolve`), and a tag its
//! image store holds where buildx's docker driver asks it to (`image-resolve-mode=local`,
//! buildx v0.37.1 build/opt.go). Each registry here is a stand-in on loopback that counts the
//! requests it answers.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::io::{BufRead as _, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A registry on loopback serving `docs` (manifests and indexes), the first also by the
/// tag `v1`, and `blobs`, each by its digest, under any repository; and how many requests
/// it has answered.
struct StandIn {
    port: u16,
    asked: Arc<AtomicUsize>,
}

impl StandIn {
    fn new(docs: Vec<Vec<u8>>, blobs: Vec<Vec<u8>>) -> StandIn {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked = Arc::new(AtomicUsize::new(0));
        let (served, counted) = (Arc::new((docs, blobs)), asked.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let (served, counted) = (served.clone(), counted.clone());
                std::thread::spawn(move || answer(stream, &served.0, &served.1, &counted));
            }
        });
        StandIn { port, asked }
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

/// Answers one connection's requests, each counted, as a registry of plain HTTP on loopback
/// does. A TLS hello, which a client sends a loopback port first (shards_registry's
/// `Registry::at`), is answered as Go's net/http answers a request it cannot read
/// (server.go, publicErr), which sends the client to plain HTTP.
fn answer(stream: TcpStream, docs: &[Vec<u8>], blobs: &[Vec<u8>], counted: &AtomicUsize) {
    let mut first = [0u8; 1];
    if stream.peek(&mut first).is_ok_and(|n| n == 1) && first[0] == 0x16 {
        counted.fetch_add(1, Ordering::SeqCst);
        let _ = (&stream).write_all(
            b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n400 Bad Request",
        );
        return;
    }
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut out = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut header = String::new();
        while reader.read_line(&mut header).unwrap_or(0) > 2 {
            header.clear();
        }
        counted.fetch_add(1, Ordering::SeqCst);
        let mut parts = line.split(' ');
        let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let named = |b: &&Vec<u8>, d: &str| sha256(b).to_string() == d;
        let found = if path == "/v2/" {
            Some((b"{}".to_vec(), "application/json".to_string()))
        } else if let Some((_, r)) = path.split_once("/manifests/") {
            docs.iter()
                .enumerate()
                .find(|(i, d)| (*i == 0 && r == "v1") || named(d, r))
                .map(|(_, d)| (d.clone(), media_type(d)))
        } else if let Some((_, d)) = path.split_once("/blobs/") {
            blobs
                .iter()
                .chain(docs)
                .find(|b| named(b, d))
                .map(|b| (b.clone(), "application/octet-stream".to_string()))
        } else {
            None
        };
        let response = match found {
            Some((body, kind)) => {
                let mut r = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nDocker-Content-Digest: {}\r\nContent-Length: {}\r\n\r\n",
                    sha256(&body),
                    body.len()
                )
                .into_bytes();
                if method != "HEAD" {
                    r.extend(body);
                }
                r
            }
            None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        };
        if out.write_all(&response).is_err() {
            return;
        }
    }
}

/// A document's own media type.
fn media_type(doc: &[u8]) -> String {
    let parsed: serde_json::Value = serde_json::from_slice(doc).unwrap();
    parsed["mediaType"].as_str().unwrap().to_string()
}

/// An image of this host's platform behind an index: the index, its manifest, then the
/// config and the one layer, a file of `text`.
fn image(text: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<Vec<u8>>) {
    let mut tar = shards_image::tar::writer::Writer::new(Vec::new());
    tar.header(&shards_image::tar::writer::Header {
        name: b"held.txt".to_vec(),
        typeflag: b'0',
        mode: 0o644,
        size: i64::try_from(text.len()).unwrap(),
        ..Default::default()
    })
    .unwrap();
    tar.write(text).unwrap();
    let layer = tar.finish().unwrap();
    let arch = show(&host_platform().architecture);
    let config = format!(
        r#"{{"architecture":"{arch}","os":"linux","config":{{}},"rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
        sha256(&layer)
    )
    .into_bytes();
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
        sha256(&config),
        config.len(),
        sha256(&layer),
        layer.len()
    )
    .into_bytes();
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{},"platform":{{"architecture":"{arch}","os":"linux"}}}}]}}"#,
        sha256(&manifest),
        manifest.len()
    )
    .into_bytes();
    (index, manifest, vec![config, layer])
}

/// Pulls `reference` into `home`'s store as a build pulls its base, with none of this
/// host's docker config, certificates or proxies.
fn pull(home: &Path, reference: &str) {
    let reference = Reference::parse(reference).unwrap();
    crate::pull::fetch(
        home,
        &reference,
        &image_platform::guest(),
        &|_: Event<'_>| {},
        &|_: &str| {},
        None,
        &|_: &str| None,
        false,
    )
    .unwrap();
}

struct Quiet;

impl policy::Log for Quiet {
    fn line(&self, _: &str) {}

    fn fetch(&self, _: &str, _: &str, _: Option<&str>) -> Result<Vec<u8>, String> {
        Err("no fetch".into())
    }
}

/// A build's bases over `store`, as `shards build` without `--pull` resolves them.
fn bases<'a>(
    home: &'a Path,
    store: &'a Store,
    progress: &'a RefCell<Progress>,
    secrets: &'a BTreeMap<String, buildflags::SecretBytes>,
    agents: &'a Agents,
) -> Bases<'a> {
    Bases {
        home,
        store,
        pull: false,
        progress,
        resolved: RefCell::new(BTreeMap::new()),
        layouts: BTreeMap::new(),
        artifacts: RefCell::new(BTreeMap::new()),
        pulled: RefCell::new(std::collections::BTreeSet::new()),
        secrets,
        agents,
        answered: RefCell::new(BTreeMap::new()),
        steps: RefCell::new(BTreeMap::new()),
        epochs: RefCell::new(BTreeMap::new()),
        policies: None,
        policy_log: &Quiet,
        refused: RefCell::new(None),
        local_dockerfile: false,
    }
}

/// A base pinned by a digest the store holds, under any name, is resolved from the store
/// to the digest asked, and no registry is asked: neither the one its name names nor the
/// one it was pulled from (cluster 13: test homes that pulled their pinned base each run
/// spent Docker Hub's anonymous pulls for the whole LAN). A tag the store holds is taken
/// from the store too. What is asked of a registry the store does not hold goes through
/// this host's docker config, which no test reads, so this test asks none.
#[test]
fn a_base_the_store_holds_is_resolved_without_asking_a_registry() {
    let home = shards_testdir::TempDir::new("bases-held").unwrap();
    let (index, manifest, blobs) = image(b"held\n");
    let pinned = sha256(&index).to_string();
    let source = StandIn::new(vec![index, manifest], blobs);
    pull(&home, &format!("127.0.0.1:{}/test/image@{pinned}", source.port));
    let pulled = source.asked();
    assert!(pulled > 0, "the pull asked its registry");

    // Another registry, which counts what it is asked and holds nothing.
    let elsewhere = StandIn::new(Vec::new(), Vec::new());
    let store = crate::pull::store(&home).unwrap();
    let progress = RefCell::new(Progress::quiet());
    let (secrets, agents) = (BTreeMap::new(), Agents::new());
    let bases = bases(&home, &store, &progress, &secrets, &agents);
    let name = format!("127.0.0.1:{}/library/other:1@{pinned}", elsewhere.port);
    let resolved = bases
        .resolve(name.as_bytes(), None, b"[internal] load metadata")
        .unwrap_or_else(|e| panic!("{name}: {}", show(&e)));
    assert_eq!(resolved.digest.as_deref(), Some(pinned.as_bytes()));
    assert_eq!(
        show(&resolved.reference),
        Reference::parse(&name).unwrap().to_string()
    );
    assert_eq!(
        (elsewhere.asked(), source.asked()),
        (0, pulled),
        "a registry was asked for a base the store holds"
    );

    // A tag, pulled once: taken from the store after.
    let tag = format!("127.0.0.1:{}/test/image:v1", source.port);
    pull(&home, &tag);
    let tagged = source.asked();
    let found = bases
        .resolve(tag.as_bytes(), None, b"[internal] load metadata")
        .unwrap_or_else(|e| panic!("{tag}: {}", show(&e)));
    assert_eq!(found.digest.as_deref(), Some(pinned.as_bytes()));
    assert_eq!(
        (elsewhere.asked(), source.asked()),
        (0, tagged),
        "a registry was asked for a tag the store holds"
    );
}

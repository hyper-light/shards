//! Pushing images to OCI registries, as containerd's PushContent walks an image and its
//! docker pusher sends it (containerd v2 core/remotes/handlers.go, PushContent;
//! core/remotes/docker/pusher.go): what each manifest names before the manifest, every
//! child manifest of an index before the index, each blob only if the repository lacks
//! it, mounted from another repository of the registry where one holds it, else uploaded
//! with a POST and one PUT. The target goes last, by its tag.
//!
//! A manifest's blobs go up together, as many at once as pay over the path (crate::climb,
//! D226, PM M488): a blob's upload is one connection's, its bytes in order, so what more
//! uploads at once bring is the link one leaves idle, its round trips (a HEAD, a POST and
//! a PUT a blob) and a connection's own ceiling, and where that stops paying differs from
//! link to link: to a registry on the same host sixteen carried 7.3 times what one did,
//! to one across a LAN one already carried what the link does, and eight less than four.
//! What a push converged to is learned for the next over the same path (crate::links).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use shards_image::oci::{self, Descriptor, Document};
use shards_image::reference::Digest;
use shards_image::store::{Held, Store};

use crate::climb::{Climb, Window, cores};
use crate::links::{self, Learned, Link, Record};
use crate::registry::Registry;
use crate::{Error, ErrorKind};

/// What happened to a layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layer {
    Pushed,
    Exists,
    Mounted(String),
}

/// A blob or document the store lacks: containerd's words, and its kind, so that the
/// caller can push less.
fn missing(d: &str) -> Error {
    Error::of(ErrorKind::Missing, format!("content digest {d}: not found"))
}

/// Pushes what `target` describes, from `store`, to `registry`, `target` itself by
/// `tag` if given, else by its digest. Blobs are mounted from `from`, a repository of the
/// same registry, when given. `report` hears each layer's fate.
pub fn push(
    registry: &Registry,
    store: &Store,
    target: &Descriptor,
    tag: Option<&str>,
    from: Option<&str>,
    report: &(dyn Fn(&Digest, Layer) + Sync),
) -> Result<(), Error> {
    // As dockerd reports a push's failure (daemon/containerd/image_push.go).
    pushed(registry, store, target, tag, from, report).map_err(Error::into_reported)
}

fn pushed(
    registry: &Registry,
    store: &Store,
    target: &Descriptor,
    tag: Option<&str>,
    from: Option<&str>,
    report: &(dyn Fn(&Digest, Layer) + Sync),
) -> Result<(), Error> {
    registry.keep_in(store);
    let bytes = document(store, target)?;
    if let Document::Index(index) = oci::parse_document(&bytes, &target.media_type)? {
        // Every child first: an index whose content is not all here is not pushed.
        for child in &index.manifests {
            match store.held(child, oci::MAX_MANIFEST)? {
                Held::Whole(_) => {}
                Held::Invalid(why) => return Err(Error::new(why)),
                Held::Missing | Held::Changed(_) => return Err(missing(&child.digest)),
            }
        }
        for child in &index.manifests {
            push_manifest(registry, store, child, None, from, report)?;
        }
        if registry.exists(target, true, tag)? {
            return Ok(());
        }
        return put(registry, target, tag, &bytes).map(|_| ());
    }
    push_manifest(registry, store, target, tag, from, report).map(|_| ())
}

/// One manifest by its digest, after what it names, and the `OCI-Subject` the registry
/// answered its PUT with: none where it said none, or where the repository had it
/// already, which was then not put.
pub(crate) fn push_manifest_answered(
    registry: &Registry,
    store: &Store,
    desc: &Descriptor,
) -> Result<Option<String>, Error> {
    push_manifest(registry, store, desc, None, None, &|_, _| {})
}

/// The bytes of a stored manifest or index, whole.
fn document(store: &Store, desc: &Descriptor) -> Result<Vec<u8>, Error> {
    match store.held(desc, oci::MAX_MANIFEST)? {
        Held::Whole(b) => Ok(b),
        Held::Invalid(why) => Err(Error::new(why)),
        Held::Missing | Held::Changed(_) => Err(missing(&desc.digest)),
    }
}

/// A manifest: its config and layers, then it, by `tag` or its digest, unless the
/// repository has it already; the `OCI-Subject` its PUT was answered with, if any.
fn push_manifest(
    registry: &Registry,
    store: &Store,
    desc: &Descriptor,
    tag: Option<&str>,
    from: Option<&str>,
    report: &(dyn Fn(&Digest, Layer) + Sync),
) -> Result<Option<String>, Error> {
    let bytes = document(store, desc)?;
    let Document::Manifest(manifest) = oci::parse_document(&bytes, &desc.media_type)? else {
        return Err(Error::new(format!("{}: an index inside an index", desc.digest)));
    };
    for blob in std::iter::once(&manifest.config).chain(&manifest.layers) {
        if !store.has(&blob.digest()?) {
            return Err(missing(&blob.digest));
        }
    }
    // The config and the layers together; the layers' fates are what is told.
    let blobs: Vec<&Descriptor> = std::iter::once(&manifest.config)
        .chain(&manifest.layers)
        .collect();
    let record = registry.record(store, links::Direction::Push);
    upload_all(
        &blobs,
        &|desc, link| blob(registry, store, desc, from, link),
        report,
        Some(&record),
    )?;
    if registry.exists(desc, true, tag)? {
        return Ok(None);
    }
    put(registry, desc, tag, &bytes)
}

/// A manifest or index put by `tag`, or by its digest; the `OCI-Subject` it was answered
/// with, if any.
fn put(
    registry: &Registry,
    desc: &Descriptor,
    tag: Option<&str>,
    bytes: &[u8],
) -> Result<Option<String>, Error> {
    registry.put_manifest_subject(tag.unwrap_or(&desc.digest), &desc.media_type, bytes)
}

/// Uploads `blobs`, each by `up`, which counts its bytes into the [`Link`] it is given
/// as they go, as many at once as pay, from what `record` learned of the path (D226); each
/// fate but the first blob's (a manifest's config) told to `report`. What the uploads
/// converged to is kept in `record` for the next push over the path. The first failure,
/// once the uploads under way have ended.
fn upload_all(
    blobs: &[&Descriptor],
    up: &(dyn Fn(&Descriptor, &Link) -> Result<Layer, Error> + Sync),
    report: &(dyn Fn(&Digest, Layer) + Sync),
    record: Option<&Record>,
) -> Result<(), Error> {
    let learned = record.and_then(Record::read);
    let link = Link::new(learned.as_ref());
    // The mean blob: what one upload holds in flight.
    let unit = blobs.iter().filter_map(|b| b.size().ok()).sum::<u64>() as f64 / blobs.len().max(1) as f64;
    let mut climb = Climb::new(
        learned.as_ref().and_then(Learned::climb),
        blobs.len().min(cores()),
    );
    if let Some(answer) = link.answer() {
        climb.answered(answer);
    }
    let first = climb.target();
    link.set_count(first);
    let uploads = Uploads {
        blobs,
        work: Mutex::new(Work {
            next: 0,
            busy: 0,
            left: blobs.len(),
        }),
        changed: Condvar::new(),
        target: AtomicUsize::new(first),
        link: &link,
        failed: Mutex::new(None),
    };
    let one = |n: usize| uploads.work_on(n, |desc| up(desc, &link), report);
    std::thread::scope(|scope| {
        let mut started = 0;
        let mut start = |to: usize| {
            while started < to.min(blobs.len()) {
                let n = started;
                let one = &one;
                if std::thread::Builder::new()
                    .name("shards-push".into())
                    .spawn_scoped(scope, move || one(n))
                    .is_err()
                {
                    break;
                }
                started += 1;
            }
            started
        };
        // Without a thread to spare, this one does the work.
        if start(first) == 0 {
            one(0);
            return;
        }
        uploads.adapt(&mut climb, unit, &mut start);
    });
    let failed = uploads
        .failed
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(record) = record {
        let sent = link.sent().load(Ordering::Relaxed);
        // This push's own: added to what the record holds as it is kept, others' since
        // it was read included (Learned::merge).
        let (count, rate) = climb.learned().unwrap_or_default();
        let own = Learned {
            count,
            rate,
            answer_us: link
                .answer()
                .map(|a| u64::try_from(a.as_micros()).unwrap_or(u64::MAX)),
            bytes: sent,
            failures: link.failures(),
        };
        // What is learned only helps the next push: one not kept costs this one nothing.
        if sent > 0 {
            let _ = record.add(&own);
        }
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A manifest's blobs going up, as many at once as `target` says.
struct Uploads<'a> {
    blobs: &'a [&'a Descriptor],
    work: Mutex<Work>,
    /// Told as an upload ends, or one fails.
    changed: Condvar,
    /// How many uploads are to go at once: an upload numbered past it takes no more.
    target: AtomicUsize,
    /// The path as the uploads find it: their bytes, failures and answers.
    link: &'a Link,
    failed: Mutex<Option<Error>>,
}

struct Work {
    /// The next blob no upload has taken.
    next: usize,
    /// Uploads under way.
    busy: usize,
    /// Blobs not yet up.
    left: usize,
}

impl Uploads<'_> {
    fn work(&self) -> MutexGuard<'_, Work> {
        lock(&self.work)
    }

    fn stopped(&self) -> bool {
        lock(&self.failed).is_some()
    }

    /// Upload `n`'s work: the blobs no other has taken, each by `up` while it is within the
    /// target (waiting while it is past it), its fate told to `report` but the config's
    /// (the first).
    fn work_on(
        &self,
        n: usize,
        up: impl Fn(&Descriptor) -> Result<Layer, Error>,
        report: &(dyn Fn(&Digest, Layer) + Sync),
    ) {
        loop {
            let i = {
                let mut work = self.work();
                loop {
                    if self.stopped() || work.next >= self.blobs.len() {
                        return;
                    }
                    // Past the count: kept for when it climbs again.
                    if n < self.target.load(Ordering::Relaxed) {
                        break;
                    }
                    work = self.changed.wait(work).unwrap_or_else(PoisonError::into_inner);
                }
                let i = work.next;
                work.next += 1;
                work.busy += 1;
                i
            };
            let done = self
                .blobs
                .get(i)
                .ok_or_else(|| Error::new("no such blob"))
                .and_then(|desc| up(desc).and_then(|fate| Ok((desc.digest()?, fate))));
            match done {
                Ok((digest, fate)) if i > 0 => report(&digest, fate),
                Ok(_) => {}
                Err(e) => {
                    lock(&self.failed).get_or_insert(e);
                }
            }
            let mut work = self.work();
            work.busy = work.busy.saturating_sub(1);
            work.left = work.left.saturating_sub(1);
            drop(work);
            self.changed.notify_all();
        }
    }

    /// Measures what the uploads carry together, by `climb`, until every blob is up or one
    /// has failed; `unit` is the bytes one upload holds, and `start` starts uploads up to a
    /// count, and says how many have started.
    fn adapt(&self, climb: &mut Climb, unit: f64, start: &mut impl FnMut(usize) -> usize) {
        let sent = self.link.sent();
        let mut window = Window::new(Instant::now(), sent.load(Ordering::Relaxed));
        let mut work = self.work();
        loop {
            if work.left == 0 || self.stopped() {
                return;
            }
            // Kept for the rest of the push: woken only as an upload ends or fails.
            if climb.settled() {
                work = self.changed.wait(work).unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            if let Some(answer) = self.link.answer() {
                climb.answered(answer);
            }
            let wait = window.due(climb.answer());
            if !wait.is_zero() {
                work = self
                    .changed
                    .wait_timeout(work, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
                continue;
            }
            let Some(rate) = window.sample(Instant::now(), sent.load(Ordering::Relaxed)) else {
                continue;
            };
            let target = self.target.load(Ordering::Relaxed);
            // Measured only while every upload has a blob: with fewer blobs left than
            // uploads, more would show nothing.
            let Some(to) = climb.window(rate, work.busy >= target, unit) else {
                continue;
            };
            self.target.store(to, Ordering::Relaxed);
            self.link.set_count(to);
            self.changed.notify_all();
            if to > target {
                drop(work);
                start(to);
                work = self.work();
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One blob: there already, mounted, or uploaded over `link`.
fn blob(
    registry: &Registry,
    store: &Store,
    desc: &Descriptor,
    from: Option<&str>,
    link: &Link,
) -> Result<Layer, Error> {
    let asked = Instant::now();
    if registry.exists(desc, false, None)? {
        return Ok(Layer::Exists);
    }
    link.answered(asked.elapsed());
    let digest = desc.digest()?;
    let file =
        std::fs::File::open(store.blob_path(&digest)).map_err(|e| Error::new(format!("{digest}: {e}")))?;
    Ok(match registry.upload(desc, &file, from, Some(link))? {
        true => Layer::Mounted(from.unwrap_or_default().to_string()),
        false => Layer::Pushed,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::auth::Credentials;
    use crate::testing::{After, route};
    use sha2::{Digest as _, Sha256};
    use shards_image::reference::{Algorithm, Reference};
    use std::time::Duration;

    fn described(media_type: &str, bytes: &[u8]) -> Descriptor {
        Descriptor {
            media_type: media_type.into(),
            digest: Digest::from_hash(Algorithm::Sha256, &Sha256::digest(bytes)).to_string(),
            size: i64::try_from(bytes.len()).unwrap(),
            platform: None,
            annotations: Default::default(),
        }
    }

    /// An index goes up after its manifests, each by its digest, the index by its tag;
    /// what the registry has no copy of is uploaded first.
    #[test]
    fn an_index_is_pushed_after_its_manifests() {
        let dir = shards_testdir::TempDir::new("push-index").unwrap();
        let store = Store::open(&dir).unwrap();
        let put = |bytes: &[u8], media_type: &str| {
            let desc = described(media_type, bytes);
            store
                .ingest(&desc.digest().unwrap(), bytes.len() as u64, &mut &bytes[..])
                .unwrap();
            desc
        };
        let layer = put(b"layer bytes", "application/vnd.oci.image.layer.v1.tar");
        let manifest = |arch: &str| {
            let config = put(
                format!(
                    r#"{{"architecture":"{arch}","os":"linux","rootfs":{{"type":"layers","diff_ids":[]}}}}"#
                )
                .as_bytes(),
                "application/vnd.oci.image.config.v1+json",
            );
            let bytes = format!(
                r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"{}","digest":"{}","size":{}}},"layers":[{{"mediaType":"{}","digest":"{}","size":{}}}]}}"#,
                oci::media::OCI_MANIFEST,
                config.media_type,
                config.digest,
                config.size,
                layer.media_type,
                layer.digest,
                layer.size
            );
            put(bytes.as_bytes(), oci::media::OCI_MANIFEST)
        };
        let (arm, amd) = (manifest("arm64"), manifest("amd64"));
        let index = put(
            format!(
                r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[{{"mediaType":"{}","digest":"{}","size":{}}},{{"mediaType":"{}","digest":"{}","size":{}}}]}}"#,
                oci::media::OCI_INDEX,
                arm.media_type,
                arm.digest,
                arm.size,
                amd.media_type,
                amd.digest,
                amd.size
            )
            .as_bytes(),
            oci::media::OCI_INDEX,
        );
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let held = seen.clone();
        let server = route(None, move |req| {
            held.lock()
                .unwrap()
                .push(format!("{} {}", req.method, req.target));
            let reply: &[u8] = match req.method.as_str() {
                "HEAD" => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                "POST" => b"HTTP/1.1 202 Accepted\r\nLocation: /v2/team/app/blobs/uploads/u\r\nContent-Length: 0\r\n\r\n",
                _ => b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
            };
            Some((reply.to_vec(), After::Keep))
        });
        let reference = Reference::parse(&format!("127.0.0.1:{}/team/app:v1", server.port)).unwrap();
        let http = crate::testing::client();
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        push(
            &registry,
            &store,
            &index,
            Some("v1"),
            None,
            &|_: &Digest, _: Layer| {},
        )
        .unwrap();
        let requests = seen.lock().unwrap().clone();
        let put_at = |target: String| {
            requests
                .iter()
                .position(|r| *r == format!("PUT /v2/team/app/manifests/{target}"))
                .unwrap_or_else(|| panic!("no PUT of {target}: {requests:?}"))
        };
        let index_at = put_at("v1".into());
        assert!(put_at(arm.digest.clone()) < index_at, "{requests:?}");
        assert!(put_at(amd.digest.clone()) < index_at, "{requests:?}");
        assert_eq!(index_at, requests.len() - 1, "the index last: {requests:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What is not here to push is `Missing`, and a registry's 404 an unexpected status:
    /// a caller that pushes less where content is missing here does not take a
    /// registry's 404 for that.
    #[test]
    fn missing_content_and_a_registrys_404_are_told_apart() {
        let dir = shards_testdir::TempDir::new("push-kinds").unwrap();
        let store = Store::open(&dir).unwrap();
        let server = route(None, |_| {
            Some((
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                After::Keep,
            ))
        });
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let http = crate::testing::client();
        let registry = Registry::new(http, &reference, Credentials::Anonymous).unwrap();
        let [config, manifest] = layerless();
        let quiet = |_: &Digest, _: Layer| {};
        // Nothing of it here.
        let missing = push(&registry, &store, &manifest.0, Some("v1"), None, &quiet).unwrap_err();
        assert_eq!(missing.kind(), ErrorKind::Missing, "{missing}");
        // All of it here, and the registry answering 404.
        for (desc, bytes) in [&config, &manifest] {
            store
                .ingest(&desc.digest().unwrap(), bytes.len() as u64, &mut &bytes[..])
                .unwrap();
        }
        let manifest_desc = manifest.0;
        let refused = push(&registry, &store, &manifest_desc, Some("v1"), None, &quiet).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Other, "{refused}");
        assert!(refused.to_string().ends_with(": 404 Not Found"), "{refused}");
        assert!(
            refused
                .to_string()
                .starts_with("unknown: unexpected status from POST request to http://127.0.0.1:"),
            "{refused}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An image of a config and a manifest, and no layers: their descriptors and bytes.
    fn layerless() -> [(Descriptor, Vec<u8>); 2] {
        let config = br#"{"architecture":"arm64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
        let config_type = "application/vnd.oci.image.config.v1+json";
        let config_desc = described(config_type, config);
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"{}","digest":"{}","size":{}}},"layers":[]}}"#,
            oci::media::OCI_MANIFEST,
            config_type,
            config_desc.digest,
            config.len()
        )
        .into_bytes();
        [
            (config_desc, config.to_vec()),
            (described(oci::media::OCI_MANIFEST, &manifest), manifest),
        ]
    }

    /// A push refused its authorization as Docker 29.3.1 reports it against registry:2
    /// with basic authentication, measured: without credentials, its upload refused in
    /// containerd's words; with wrong ones, its first check of what is there.
    #[test]
    fn refused_authorization_is_reported() {
        let dir = shards_testdir::TempDir::new("push-refused").unwrap();
        let store = Store::open(&dir).unwrap();
        let [config, manifest] = layerless();
        for (desc, bytes) in [&config, &manifest] {
            store
                .ingest(&desc.digest().unwrap(), bytes.len() as u64, &mut &bytes[..])
                .unwrap();
        }
        let server = route(None, |_| {
            Some((
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"r\"\r\nContent-Length: 0\r\n\r\n"
                    .to_vec(),
                After::Keep,
            ))
        });
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", server.port)).unwrap();
        let pushed = |credentials, from| {
            let http = crate::testing::client();
            let registry = Registry::new(http, &reference, credentials).unwrap();
            push(&registry, &store, &manifest.0, Some("v1"), from, &|_, _| {}).unwrap_err()
        };
        // A mount refused so goes on as an upload, which is then refused.
        for from in [None, Some("other/repo")] {
            let none = pushed(Credentials::Anonymous, from);
            assert_eq!(
                none.to_string(),
                "push access denied, repository does not exist or may require authorization: \
                 authorization failed: no basic auth credentials",
                "{from:?}"
            );
            assert_eq!(none.kind(), ErrorKind::Unauthorized);
        }
        let wrong = pushed(
            Credentials::Password {
                username: "u".into(),
                password: "wrong".into(),
            },
            None,
        );
        assert_eq!(
            wrong.to_string(),
            format!(
                "unknown: unexpected status from HEAD request to http://127.0.0.1:{}/v2/test/image/blobs/{}: \
                 401 Unauthorized",
                server.port, config.0.digest
            )
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `blobs` uploads through [`upload_all`], learned in `record`, each of ten steps of
    /// a MiB, each step 10 ms and answered in as long: 100 MiB a second each, whatever the
    /// others do. How many uploads were under way as each began, in order.
    fn uploaded(blobs: usize, record: &Record) -> Vec<usize> {
        let descs: Vec<Descriptor> = (0..blobs)
            .map(|i| described("application/octet-stream", format!("blob {i}").as_bytes()))
            .collect();
        let refs: Vec<&Descriptor> = descs.iter().collect();
        let now = AtomicUsize::new(0);
        let began = Mutex::new(Vec::new());
        let up = |_: &Descriptor, link: &Link| {
            let at_once = now.fetch_add(1, Ordering::SeqCst) + 1;
            lock(&began).push(at_once);
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(10));
                link.sent().fetch_add(1 << 20, Ordering::Relaxed);
                link.answered(Duration::from_millis(10));
            }
            now.fetch_sub(1, Ordering::SeqCst);
            Ok(Layer::Pushed)
        };
        upload_all(&refs, &up, &|_: &Digest, _: Layer| {}, Some(record)).unwrap();
        began.into_inner().unwrap()
    }

    /// A push over a path never seen starts with one upload and doubles while that pays,
    /// to the host's cores at most; what it converged to is kept, and the next push over
    /// the path starts there.
    #[test]
    fn uploads_climb_from_one_and_the_next_push_starts_where_they_got() {
        let dir = shards_testdir::TempDir::new("push-climb").unwrap();
        let record = Record::of(&dir, links::Direction::Push, "127.0.0.1", 1, None);
        let began = uploaded(120, &record);
        assert_eq!(began[0], 1, "a new path starts at one: {began:?}");
        let most = began.iter().copied().max().unwrap();
        assert!(
            most >= 4.min(cores()),
            "uploads that each carry their own did not climb: {began:?}"
        );
        assert!(most <= cores(), "past the host's cores: {began:?}");
        let learned = record.read().expect("the path learned");
        assert!(learned.count >= 2.min(cores()), "{learned:?}");
        assert_eq!(learned.bytes, (120 * 10) << 20);
        let began = uploaded(learned.count * 4, &record);
        let at_first = began.iter().take(learned.count).copied().max().unwrap();
        assert_eq!(
            at_first, learned.count,
            "the next push did not start where the last got: {began:?}"
        );
    }
}

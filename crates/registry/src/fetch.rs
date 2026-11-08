//! Fetching a pull's layers over several connections at once, splitting the large ones
//! into ranges (PM M114).
//!
//! One connection to Docker Hub's CDN carried 49 to 74 MB/s where four carried 94 to
//! 102 MB/s, and a 6 GB image's 2.8 GB layer, fetched whole over one, took most of its
//! pull. So layers are fetched in order, each from where an earlier attempt left it, and
//! a connection with nothing left to start takes the second half of the largest range
//! still coming: the work is split as it goes, only where it pays.
//! - A range is split only when its half would take longer to arrive than a request
//!   takes to answer: a split costs a request, and its owner's connection.
//! - Ranges of a blob after the first are asked of where the registry redirected the
//!   first, as long as that answers: Docker Hub's redirect cost 100 ms a request more
//!   than asking its CDN directly (p50 151 against 52 ms).
//! - The connections climb as crate::climb has them (D226): from where the path was learned
//!   at, or from one, doubling while a doubling carries more than the path's noise, to the
//!   host's cores at most. A layer that is not compressed is fetched whole, over one
//!   connection, so that the registry may compress it for the transfer.
//!
//! containerd fetches each layer whole over one connection, three layers at a time.

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use shards_image::oci::{Descriptor, Manifest};
use shards_image::reference::Digest;
use shards_image::store::{Limits, Ranged, Store};

use crate::climb::{Climb, WINDOW, cores};
use crate::links::Learned;
use crate::pull::Event;
use crate::registry::{CHUNK, Registry, Trouble, not_fetched, short, stalled};
use crate::url::Url;
use crate::{Error, ErrorKind};

/// A layer of the pull.
struct Layer<'a> {
    desc: &'a Descriptor,
    digest: Digest,
    /// Its download, while ranges of it are coming.
    ranged: RwLock<Option<Ranged>>,
    /// Ranges of it not yet in.
    open: AtomicUsize,
    /// Whether the registry answered a range of it with that range: only then is it split.
    splits: AtomicBool,
    /// Where the registry redirected a range of it, to ask for the rest.
    redirected: Mutex<Option<Url>>,
}

/// What one connection is fetching: bytes `pos..end` of layer `layer`, `pos` advanced
/// before the bytes are written, and `end` brought in by a split.
#[derive(Clone, Copy, Default)]
struct Span {
    layer: usize,
    pos: u64,
    end: u64,
}

/// What the connections share.
struct Flows<'a> {
    registry: &'a Registry,
    store: &'a Store,
    limits: &'a Limits,
    report: &'a (dyn Fn(Event<'_>) + Sync),
    here: &'a (dyn Fn(usize) + Sync),
    /// Whether the layers are fetched again though stored (`pull --no-cache`).
    fresh: bool,
    layers: Vec<Layer<'a>>,
    /// Each connection's range, by its number.
    spans: Vec<Mutex<Span>>,
    /// The next layer no connection has started, and the connections working.
    work: Mutex<Work>,
    /// Told when a range may be split, one is done, or the pull ends.
    changed: Condvar,
    /// How many connections are to work.
    target: AtomicUsize,
    /// Bytes arrived, for measuring.
    bytes: AtomicU64,
    started: Instant,
    /// How long the last request took to answer, in µs.
    answer: AtomicU64,
    failed: Mutex<Option<Error>>,
}

#[derive(Default)]
struct Work {
    next: usize,
    /// Connections fetching something.
    busy: usize,
    /// Layers not yet stored.
    left: usize,
}

/// Fetches the layers of `manifest` that are not in `store`, telling `here` each one's
/// index as it is stored.
pub(crate) fn layers(
    registry: &Registry,
    store: &Store,
    manifest: &Manifest,
    limits: &Limits,
    report: &(dyn Fn(Event<'_>) + Sync),
    here: &(dyn Fn(usize) + Sync),
    fresh: bool,
) -> Result<(), Error> {
    registry.keep_in(store);
    let mut layers = Vec::with_capacity(manifest.layers.len());
    for desc in &manifest.layers {
        layers.push(Layer {
            desc,
            digest: desc.digest()?,
            ranged: RwLock::new(None),
            open: AtomicUsize::new(0),
            splits: AtomicBool::new(false),
            redirected: Mutex::new(None),
        });
    }
    let flows = Flows {
        registry,
        store,
        limits,
        report,
        here,
        fresh,
        work: Mutex::new(Work {
            left: layers.len(),
            ..Work::default()
        }),
        layers,
        spans: (0..cores()).map(|_| Mutex::new(Span::default())).collect(),
        changed: Condvar::new(),
        target: AtomicUsize::new(1),
        bytes: AtomicU64::new(0),
        started: Instant::now(),
        answer: AtomicU64::new(0),
        failed: Mutex::new(None),
    };
    let record = registry.record(store, crate::links::Direction::Pull);
    let learned = record.read();
    std::thread::scope(|scope| {
        let mut started = 0;
        let start = |to: usize, started: &mut usize| {
            while *started < to {
                let n = *started;
                let flows = &flows;
                let spawned = std::thread::Builder::new()
                    .name("shards-pull".into())
                    .spawn_scoped(scope, move || flows.flow(n));
                if spawned.is_err() {
                    break;
                }
                *started += 1;
            }
        };
        // As many as there are layers, or more where a layer may be split between them.
        let most = if flows.layers.iter().any(|l| compressed(&l.desc.media_type)) {
            flows.spans.len()
        } else {
            flows.spans.len().min(flows.layers.len())
        };
        let mut climb = Climb::new(learned.as_ref().and_then(Learned::climb), most);
        if let Some(us) = learned.as_ref().and_then(|l| l.answer_us) {
            climb.answered(Duration::from_micros(us));
        }
        flows.target.store(climb.target(), Ordering::Relaxed);
        start(climb.target(), &mut started);
        if started == 0 {
            // No thread to spare: fetched here, one connection.
            flows.flow(0);
            return;
        }
        flows.adapt(&mut climb, |to| start(to, &mut started));
        if let Some((count, rate)) = climb.learned() {
            let kept = Learned {
                count,
                rate,
                answer_us: climb
                    .answer()
                    .map(|a| u64::try_from(a.as_micros()).unwrap_or(u64::MAX)),
                ..Learned::default()
            };
            // What is learned only helps the next pull: one not kept costs this one nothing.
            let _ = record.write(&kept);
        }
    });
    match flows.failed.into_inner().unwrap_or_else(PoisonError::into_inner) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

impl<'a> Flows<'a> {
    fn work(&self) -> MutexGuard<'_, Work> {
        self.work.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stopped(&self) -> bool {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    fn fail(&self, e: Error) {
        self.failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(e);
        self.changed.notify_all();
    }

    /// Measures what the connections bring, doubling them while that pays, until the
    /// layers are in; `start` starts connections up to a count.
    fn adapt(&self, climb: &mut Climb, mut start: impl FnMut(usize)) {
        // The mean layer: what one connection holds in flight.
        let unit = self.layers.iter().filter_map(|l| l.desc.size().ok()).sum::<u64>() as f64
            / self.layers.len().max(1) as f64;
        let mut since = (Instant::now(), self.bytes.load(Ordering::Relaxed));
        let mut work = self.work();
        loop {
            if work.left == 0 || self.stopped() {
                return;
            }
            let wait = WINDOW.saturating_sub(since.0.elapsed());
            if !wait.is_zero() {
                work = self
                    .changed
                    .wait_timeout(work, wait)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
                continue;
            }
            let now = (Instant::now(), self.bytes.load(Ordering::Relaxed));
            let rate = (now.1 - since.1) as f64 / now.0.duration_since(since.0).as_secs_f64();
            since = now;
            let target = self.target.load(Ordering::Relaxed);
            let answer = self.answer.load(Ordering::Relaxed);
            if answer > 0 {
                climb.answered(Duration::from_micros(answer));
            }
            // Measured only while every connection has work.
            let Some(to) = climb.window(rate, work.busy >= target, unit) else {
                continue;
            };
            self.target.store(to, Ordering::Relaxed);
            self.changed.notify_all();
            if to > target {
                drop(work);
                start(to);
                work = self.work();
            }
        }
    }

    /// One connection's work, connection `n`: layers not started, then the halves of
    /// ranges of others.
    fn flow(&self, n: usize) {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let Some(span) = self.take(n) else {
                return;
            };
            let done = match span {
                Took::Whole(i) => self.whole(i),
                Took::Range => self.range(n, &mut buf),
            };
            if let Err(e) = done {
                self.fail(e);
                return;
            }
        }
    }

    /// What connection `n` is to fetch next, waiting until there is something, or `None`
    /// once there is nothing more for it.
    fn take(&self, n: usize) -> Option<Took> {
        let mut work = self.work();
        loop {
            if self.stopped() || work.left == 0 {
                self.changed.notify_all();
                return None;
            }
            // Past the count: kept for when it climbs again.
            if n >= self.target.load(Ordering::Relaxed) {
                work = self.changed.wait(work).unwrap_or_else(PoisonError::into_inner);
                continue;
            }
            if let Some(layer) = self.layers.get(work.next) {
                let i = work.next;
                work.next += 1;
                work.busy += 1;
                drop(work);
                match self.start(i, layer, n) {
                    Ok(Some(took)) => return Some(took),
                    Ok(None) => {
                        work = self.work();
                        work.busy = work.busy.saturating_sub(1);
                        continue;
                    }
                    Err(e) => {
                        self.fail(e);
                        return None;
                    }
                }
            }
            if let Some(span) = self.split(work.busy) {
                if let Some(slot) = self.spans.get(n) {
                    *lock(slot) = span;
                }
                work.busy += 1;
                return Some(Took::Range);
            }
            work = self.changed.wait(work).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Starts layer `i` on connection `n`: `None` if it is here.
    fn start(&self, i: usize, layer: &Layer<'_>, n: usize) -> Result<Option<Took>, Error> {
        if !self.fresh && self.store.has(&layer.digest) {
            (self.report)(Event::Present(&layer.digest));
            self.stored(i);
            return Ok(None);
        }
        if !compressed(&layer.desc.media_type) {
            return Ok(Some(Took::Whole(i)));
        }
        let size = layer.desc.size()?;
        let ranged = if self.fresh {
            self.store
                .download_ranged_again(&layer.digest, size, self.limits)
                .map(Some)
        } else {
            self.store.download_ranged(&layer.digest, size, self.limits)
        };
        let Some(ranged) = ranged.map_err(|e| Error::new(e.to_string()))? else {
            // Another process stored it meanwhile.
            (self.report)(Event::Layer(&layer.digest));
            self.stored(i);
            return Ok(None);
        };
        let have = ranged.have();
        *layer.ranged.write().unwrap_or_else(PoisonError::into_inner) = Some(ranged);
        layer.open.store(1, Ordering::Release);
        if let Some(slot) = self.spans.get(n) {
            *lock(slot) = Span {
                layer: i,
                pos: have,
                end: size,
            };
        }
        Ok(Some(Took::Range))
    }

    /// The second half of the range with the most left of any whose registry answers
    /// ranges, if fetching that half alone would take longer than a request does.
    fn split(&self, busy: usize) -> Option<Span> {
        let elapsed = self.started.elapsed().as_secs_f64();
        let busy = busy.max(1);
        let each = self.bytes.load(Ordering::Relaxed) as f64 / elapsed.max(f64::MIN_POSITIVE) / busy as f64;
        let answer = Duration::from_micros(self.answer.load(Ordering::Relaxed)).as_secs_f64();
        let least = (each * answer) as u64;
        let mut best: Option<(&Mutex<Span>, u64)> = None;
        for slot in &self.spans {
            let span = *lock(slot);
            let left = span.end.saturating_sub(span.pos);
            let splits = self
                .layers
                .get(span.layer)
                .is_some_and(|l| l.splits.load(Ordering::Acquire));
            if splits && left / 2 > least.max(CHUNK as u64) && best.is_none_or(|(_, b)| left > b) {
                best = Some((slot, left));
            }
        }
        let (slot, _) = best?;
        let mut span = lock(slot);
        let left = span.end.saturating_sub(span.pos);
        if left / 2 <= least.max(CHUNK as u64) {
            return None;
        }
        let mid = span.pos + left / 2;
        let taken = Span {
            layer: span.layer,
            pos: mid,
            end: span.end,
        };
        span.end = mid;
        // Counted before the owner can finish: the layer is not whole until both are.
        self.layers.get(span.layer)?.open.fetch_add(1, Ordering::AcqRel);
        Some(taken)
    }

    /// Fetches layer `i` whole, over one connection.
    fn whole(&self, i: usize) -> Result<(), Error> {
        let layer = self.layers.get(i).ok_or_else(|| Error::new("no such layer"))?;
        let digest = &layer.digest;
        let fetch = if self.fresh {
            Registry::fetch_blob_again
        } else {
            Registry::fetch_blob
        };
        fetch(self.registry, self.store, layer.desc, self.limits, &|n| {
            self.bytes.fetch_add(n, Ordering::Relaxed);
            (self.report)(Event::Progress(digest, n));
        })?;
        (self.report)(Event::Layer(digest));
        self.stored(i);
        self.idle();
        Ok(())
    }

    /// Fetches connection `n`'s range, as far as its end, which a split may bring in.
    fn range(&self, n: usize, buf: &mut [u8]) -> Result<(), Error> {
        let slot = self
            .spans
            .get(n)
            .ok_or_else(|| Error::new("no such connection"))?;
        let i = lock(slot).layer;
        let layer = self.layers.get(i).ok_or_else(|| Error::new("no such layer"))?;
        let held = layer.ranged.read().unwrap_or_else(PoisonError::into_inner);
        let ranged = held
            .as_ref()
            .ok_or_else(|| Error::new("a range of a layer not started"))?;
        let blob = self.registry.blob_url(&layer.digest)?;
        let mut trouble = Trouble::default();
        loop {
            let Span { pos, end, .. } = *lock(slot);
            if pos >= end {
                break;
            }
            let redirected = lock(&layer.redirected).clone();
            let url = redirected.as_ref().unwrap_or(&blob);
            let asked = Instant::now();
            let mut response = self.registry.get_range(url, &layer.desc.media_type, pos, end)?;
            let mut cut = false;
            let whole = response.status == 200;
            match response.status {
                206 => {
                    if redirected.is_none() && response.url() != &blob {
                        *lock(&layer.redirected) = Some(response.url().clone());
                    }
                    self.answer
                        .store(asked.elapsed().as_micros() as u64, Ordering::Relaxed);
                    if !layer.splits.swap(true, Ordering::AcqRel) {
                        self.changed.notify_all();
                    }
                }
                200 => {
                    // The whole blob, ranges not taken: what comes before the range is
                    // passed over.
                    if !response
                        .header("content-encoding")
                        .is_none_or(|e| e.eq_ignore_ascii_case("identity"))
                    {
                        return Err(Error::new(format!(
                            "{}: a range of it was sent encoded for the transfer",
                            layer.digest
                        )));
                    }
                    match std::io::copy(&mut (&mut response).take(pos), &mut std::io::sink()) {
                        Ok(p) if p == pos => {}
                        // Ended as its framing said, before the range began: the blob is
                        // shorter than its descriptor, which no asking again changes.
                        Ok(p) => return Err(short(&layer.digest, p, layer.desc.size)),
                        Err(_) => cut = true,
                    }
                }
                _ if redirected.is_some() => {
                    // Where it was redirected answers no more (a signed URL expired): asked
                    // of the registry again.
                    *lock(&layer.redirected) = None;
                    continue;
                }
                _ => return Err(not_fetched(response, url).context(&layer.digest)),
            }
            // A whole blob cut before the range began brought none of it.
            if !cut {
                let before = lock(slot).pos;
                let copied = self.copy(&mut response, slot, ranged, &layer.digest, buf);
                let ended = match copied {
                    Ok(()) => true,
                    Err(e) if e.kind() == ErrorKind::Transient => false,
                    Err(e) => return Err(e.context(&layer.digest)),
                };
                let Span { pos: now, end, .. } = *lock(slot);
                // An answer that ended as its framing said is the registry's own, not a
                // passing trouble: the whole blob, ended short of the range, is shorter
                // than its descriptor; a range that brought nothing will bring nothing.
                if ended && now < end && (whole || now == before) {
                    return Err(short(&layer.digest, now, layer.desc.size));
                }
                if now > before || now >= end {
                    self.registry.over(&mut trouble, url);
                    continue;
                }
            }
            drop(response);
            // It brought nothing: asked again while that lasts its path's budget (D222).
            match trouble.wait(asked, self.registry.budget(url), None) {
                Some(wait) => self.registry.pause(wait)?,
                None => return Err(stalled(&layer.digest, trouble.lasted())),
            }
        }
        drop(held);
        if layer.open.fetch_sub(1, Ordering::AcqRel) == 1 {
            let ranged = layer
                .ranged
                .write()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(ranged) = ranged {
                ranged.commit().map_err(|e| Error::new(e.to_string()))?;
            }
            (self.report)(Event::Layer(&layer.digest));
            self.stored(i);
        }
        self.idle();
        Ok(())
    }

    /// Copies a response into connection `slot`'s range, up to its end as it is when each
    /// piece arrives.
    fn copy(
        &self,
        response: &mut dyn Read,
        slot: &Mutex<Span>,
        ranged: &Ranged,
        digest: &Digest,
        buf: &mut [u8],
    ) -> Result<(), Error> {
        loop {
            let n = match response.read(buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            let (at, take) = {
                let mut span = lock(slot);
                let take = (n as u64).min(span.end.saturating_sub(span.pos));
                let at = span.pos;
                span.pos += take;
                (at, take)
            };
            ranged
                .write_at(at, buf.get(..take as usize).unwrap_or_default())
                .map_err(|e| Error::new(e.to_string()))?;
            self.bytes.fetch_add(take, Ordering::Relaxed);
            (self.report)(Event::Progress(digest, take));
            let ended = {
                let span = lock(slot);
                span.pos >= span.end
            };
            if take < n as u64 || ended {
                // The rest is another connection's, or there is no more.
                return Ok(());
            }
        }
    }

    /// Layer `i` is stored.
    fn stored(&self, i: usize) {
        (self.here)(i);
        let mut work = self.work();
        work.left = work.left.saturating_sub(1);
        drop(work);
        self.changed.notify_all();
    }

    /// A connection is done with what it took.
    fn idle(&self) {
        let mut work = self.work();
        work.busy = work.busy.saturating_sub(1);
        drop(work);
        self.changed.notify_all();
    }
}

/// What a connection took.
enum Took {
    /// A layer, whole.
    Whole(usize),
    /// The range now in its span.
    Range,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a layer of `media_type` is compressed: gzip or zstd, which no registry
/// compresses again for the transfer.
fn compressed(media_type: &str) -> bool {
    media_type.ends_with("gzip") || media_type.ends_with("zstd")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::auth::Credentials;
    use crate::http::Client;
    use crate::testing::{After, Seen, Server, route};
    use sha2::{Digest as _, Sha256};
    use shards_image::reference::{Algorithm, Reference};

    fn http(status: &str, fields: &[(&str, String)], body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status}\r\n");
        for (n, v) in fields {
            out.push_str(&format!("{n}: {v}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let mut out = out.into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn digest(bytes: &[u8]) -> Digest {
        Digest::from_hash(Algorithm::Sha256, &Sha256::digest(bytes))
    }

    /// How a fake CDN answers.
    #[derive(Clone, Copy)]
    struct Cdn {
        /// Whether it serves ranges, or the whole blob to every request.
        ranges: bool,
        /// Whether each URL the registry redirects to serves one request only, as a
        /// signed URL that expires would.
        once: bool,
    }

    /// A registry that redirects each blob request to a CDN, numbering each redirect.
    struct Fake {
        registry: Server,
        cdn: Server,
    }

    fn fake(blobs: Vec<Vec<u8>>, cdn: Cdn) -> Fake {
        fake_stuck(blobs, cdn, None)
    }

    /// [`fake`], whose CDN holds the first request for blob `stuck` unanswered for 5 s.
    fn fake_stuck(blobs: Vec<Vec<u8>>, cdn: Cdn, stuck: Option<Digest>) -> Fake {
        let by_digest: std::collections::HashMap<String, Vec<u8>> =
            blobs.into_iter().map(|b| (digest(&b).to_string(), b)).collect();
        let used = Mutex::new(std::collections::HashSet::new());
        let held = AtomicBool::new(false);
        let served = route(None, move |req: &Seen| {
            let (path, query) = req.target.split_once('?').unwrap_or((&req.target, ""));
            let name = path.strip_prefix("/cdn/")?;
            let bytes = by_digest.get(name)?;
            if stuck.as_ref().is_some_and(|d| d.to_string() == name) && !held.swap(true, Ordering::SeqCst) {
                std::thread::sleep(Duration::from_secs(5));
            }
            if cdn.once && !used.lock().unwrap().insert(query.to_string()) {
                return Some((http("403 Forbidden", &[], b"expired"), After::Keep));
            }
            // Slow enough to answer that the other connections are waiting for work.
            std::thread::sleep(Duration::from_millis(20));
            let range = req.header("range").filter(|_| cdn.ranges).and_then(|r| {
                let (from, last) = r.strip_prefix("bytes=")?.split_once('-')?;
                Some((from.parse::<usize>().ok()?, last.parse::<usize>().ok()?))
            });
            Some(match range {
                // Past the blob's end: none of it (RFC 9110 §15.5.17).
                Some((from, _)) if from >= bytes.len() => (
                    http(
                        "416 Range Not Satisfiable",
                        &[("Content-Range", format!("bytes */{}", bytes.len()))],
                        b"",
                    ),
                    After::Keep,
                ),
                Some((from, last)) => {
                    let last = last.min(bytes.len() - 1);
                    let range = format!("bytes {from}-{last}/{}", bytes.len());
                    (
                        http(
                            "206 Partial Content",
                            &[("Content-Range", range)],
                            &bytes[from..=last],
                        ),
                        After::Keep,
                    )
                }
                None => (http("200 OK", &[], bytes), After::Keep),
            })
        });
        let port = served.port;
        let redirects = AtomicUsize::new(0);
        let registry = route(None, move |req: &Seen| {
            let digest = req.target.strip_prefix("/v2/test/image/blobs/")?;
            let n = redirects.fetch_add(1, Ordering::SeqCst);
            let location = format!("http://127.0.0.1:{port}/cdn/{digest}?signed={n}");
            Some((
                http("307 Temporary Redirect", &[("Location", location)], b""),
                After::Keep,
            ))
        });
        Fake {
            registry,
            cdn: served,
        }
    }

    fn manifest(layers: &[(&str, &[u8])]) -> Manifest {
        let layers: Vec<String> = layers
            .iter()
            .map(|(media_type, b)| {
                format!(
                    r#"{{"mediaType":"{media_type}","digest":"{}","size":{}}}"#,
                    digest(b),
                    b.len()
                )
            })
            .collect();
        serde_json::from_str(&format!(
            r#"{{"schemaVersion":2,"config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":2}},"layers":[{}]}}"#,
            digest(b"{}"),
            layers.join(",")
        ))
        .unwrap()
    }

    fn registry(fake: &Fake) -> Registry {
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
        let client = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        );
        Registry::new(client, &reference, Credentials::Anonymous).unwrap()
    }

    /// A path to `fake`'s registry learned at `count` connections, so that a pull over it
    /// starts with that many.
    fn learned_at(store: &Store, fake: &Fake, count: usize) {
        let mut rate = crate::climb::Spread::default();
        rate.add(1e6);
        rate.add(1e6);
        let learned = Learned {
            count,
            rate,
            ..Learned::default()
        };
        let from = Some(std::net::IpAddr::from([127, 0, 0, 1]));
        crate::links::Record::of(
            store.root(),
            crate::links::Direction::Pull,
            "127.0.0.1",
            fake.registry.port,
            from,
        )
        .write(&learned)
        .unwrap();
    }

    fn store(name: &str) -> (Store, shards_testdir::TempDir) {
        let root = shards_testdir::TempDir::new(&format!("fetch-{name}")).unwrap();
        (Store::open(&root).unwrap(), root)
    }

    const GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

    /// Bytes no two ranges of which are alike.
    fn blob(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) ^ seed).to_le_bytes()[1])
            .collect()
    }

    fn ranges_asked(server: &Server, digest: &Digest) -> Vec<String> {
        server
            .requests()
            .into_iter()
            .filter(|r| r.starts_with(&format!("GET /cdn/{digest}")))
            .filter_map(|r| {
                r.lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    .map(|l| l.to_string())
            })
            .collect()
    }

    #[test]
    fn a_large_layer_is_split_between_connections_and_asked_of_its_cdn() {
        let big = blob(24 << 20, 1);
        let small = blob(1000, 2);
        let fake = fake(
            vec![big.clone(), small.clone()],
            Cdn {
                ranges: true,
                once: false,
            },
        );
        let (store, root) = store("split");
        learned_at(&store, &fake, 4);
        let here = Mutex::new(Vec::new());
        let arrived = AtomicU64::new(0);
        let report = |e: Event<'_>| {
            if let Event::Progress(_, n) = e {
                arrived.fetch_add(n, Ordering::Relaxed);
            }
        };
        layers(
            &registry(&fake),
            &store,
            &manifest(&[(GZIP, &big), (GZIP, &small)]),
            &Limits::none(),
            &report,
            &|i| here.lock().unwrap().push(i),
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(store.blob_path(&digest(&big))).unwrap(), big);
        assert_eq!(std::fs::read(store.blob_path(&digest(&small))).unwrap(), small);
        let mut here = here.into_inner().unwrap();
        here.sort_unstable();
        assert_eq!(here, [0, 1]);
        // Every byte arrived once.
        assert_eq!(arrived.load(Ordering::Relaxed), (big.len() + small.len()) as u64);
        let asked = ranges_asked(&fake.cdn, &digest(&big));
        assert!(asked.len() > 1, "split: {asked:?}");
        // The ranges after the first were asked of the CDN directly.
        let redirected = fake
            .registry
            .requests()
            .iter()
            .filter(|r| r.starts_with(&format!("GET /v2/test/image/blobs/{}", digest(&big))))
            .count();
        assert_eq!(redirected, 1);
        assert_eq!(std::fs::read_dir(root.join("ingest")).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_fresh_fetch_fetches_what_is_stored_again() {
        let big = blob(2 << 20, 7);
        let fake = fake(
            vec![big.clone()],
            Cdn {
                ranges: true,
                once: false,
            },
        );
        let (store, root) = store("fresh");
        let fetch = |fresh| {
            layers(
                &registry(&fake),
                &store,
                &manifest(&[(GZIP, &big)]),
                &Limits::none(),
                &|_| {},
                &|_| {},
                fresh,
            )
            .unwrap()
        };
        fetch(false);
        let first = fake.cdn.requests().len();
        // Stored: not asked for again, unless fresh.
        fetch(false);
        assert_eq!(fake.cdn.requests().len(), first);
        fetch(true);
        assert!(fake.cdn.requests().len() > first);
        assert_eq!(std::fs::read(store.blob_path(&digest(&big))).unwrap(), big);
        assert_eq!(std::fs::read_dir(root.join("ingest")).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_cdn_that_ignores_ranges_serves_each_layer_whole_once() {
        let big = blob(8 << 20, 3);
        let fake = fake(
            vec![big.clone()],
            Cdn {
                ranges: false,
                once: false,
            },
        );
        let (store, root) = store("whole");
        layers(
            &registry(&fake),
            &store,
            &manifest(&[(GZIP, &big)]),
            &Limits::none(),
            &|_| {},
            &|_| {},
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(store.blob_path(&digest(&big))).unwrap(), big);
        assert_eq!(fake.cdn.requests().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stuck item is isolated: a layer whose first request its CDN holds unanswered
    /// keeps no other layer waiting, and is itself asked again once its answer is overdue,
    /// and fetched (D222).
    #[test]
    fn a_stuck_layer_keeps_no_other_waiting() {
        // Each its own: a seed changes no byte `blob` keeps, a length does.
        let stuck = blob(1 << 20, 6);
        let others: Vec<Vec<u8>> = (1..4).map(|n| blob((1 << 20) + n, 6)).collect();
        let mut all = vec![stuck.clone()];
        all.extend(others.iter().cloned());
        let fake = fake_stuck(
            all.clone(),
            Cdn {
                ranges: true,
                once: false,
            },
            Some(digest(&stuck)),
        );
        let reference = Reference::parse(&format!("127.0.0.1:{}/test/image:v1", fake.registry.port)).unwrap();
        // An answer's head overdue after 1 s, where the CDN holds the stuck one 5 s.
        let client = Client::new(
            Box::new(|_| crate::tls::client_config(Vec::new(), None)),
            "shards-test",
        )
        .with_patience(crate::http::Patience {
            connect: Duration::from_secs(30),
            head: Some(Duration::from_secs(1)),
            stall: Some(Duration::from_secs(1)),
        });
        let registry = Registry::new(client, &reference, Credentials::Anonymous).unwrap();
        let (store, root) = store("stuck");
        // A path learned at four connections: a new one starts at one (D226), and isolates
        // a stuck layer only from its first doubling on.
        learned_at(&store, &fake, 4);
        let t0 = Instant::now();
        let here = Mutex::new(Vec::new());
        let layered: Vec<(&str, &[u8])> = all.iter().map(|b| (GZIP, &b[..])).collect();
        layers(
            &registry,
            &store,
            &manifest(&layered),
            &Limits::none(),
            &|_| {},
            &|i| here.lock().unwrap().push((i, t0.elapsed())),
            false,
        )
        .unwrap();
        for b in &all {
            assert_eq!(&std::fs::read(store.blob_path(&digest(b))).unwrap(), b);
        }
        let here = here.into_inner().unwrap();
        // The others, stored while the stuck one waited on its overdue answer.
        let when = |i: usize| here.iter().find(|(n, _)| *n == i).unwrap().1;
        for i in 1..all.len() {
            assert!(when(i) < Duration::from_secs(1), "layer {i}: {here:?}");
        }
        // The stuck one, asked again past its overdue answer, not after the CDN's 5 s.
        assert!(when(0) >= Duration::from_secs(1), "{here:?}");
        assert!(when(0) < Duration::from_secs(4), "{here:?}");
        assert!(ranges_asked(&fake.cdn, &digest(&stuck)).len() >= 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A blob that ends, its answers whole, short of what its descriptor says fails at
    /// once, saying so: the registry's own answer, not a trouble to ride out (D222), which
    /// asked again forever where the CDN sends the whole blob each time.
    #[test]
    fn a_blob_shorter_than_its_descriptor_fails_at_once() {
        for ranges in [false, true] {
            let short = blob(1 << 20, 5);
            let fake = fake(vec![short.clone()], Cdn { ranges, once: false });
            let (store, root) = store(if ranges { "short-r" } else { "short-w" });
            let mut manifest = manifest(&[(GZIP, &short)]);
            manifest.layers[0].size = (2 << 20) as i64;
            let t0 = Instant::now();
            let e = layers(
                &registry(&fake),
                &store,
                &manifest,
                &Limits::none(),
                &|_| {},
                &|_| {},
                false,
            )
            .unwrap_err();
            // Without ranges, the whole blob ended short; with them, the range past its
            // end is refused (416).
            let said = if ranges {
                "416"
            } else {
                "the registry's blob ends at"
            };
            assert!(e.to_string().contains(said), "{ranges}: {e}");
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "{ranges}: {:?}",
                t0.elapsed()
            );
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn an_expired_redirect_is_asked_of_the_registry_again() {
        let big = blob(24 << 20, 4);
        let fake = fake(
            vec![big.clone()],
            Cdn {
                ranges: true,
                once: true,
            },
        );
        let (store, root) = store("expired");
        learned_at(&store, &fake, 4);
        layers(
            &registry(&fake),
            &store,
            &manifest(&[(GZIP, &big)]),
            &Limits::none(),
            &|_| {},
            &|_| {},
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(store.blob_path(&digest(&big))).unwrap(), big);
        assert!(
            fake.registry.requests().len() > 1,
            "asked again: {:?}",
            fake.cdn.requests().len()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_layer_goes_on_from_what_an_earlier_pull_left() {
        let big = blob(4 << 20, 5);
        let fake = fake(
            vec![big.clone()],
            Cdn {
                ranges: true,
                once: false,
            },
        );
        let (store, root) = store("resume");
        let d = digest(&big);
        let mut left = store
            .download(&d, big.len() as u64, &Limits::none())
            .unwrap()
            .unwrap();
        left.write(&big[..1_000_000]).unwrap();
        drop(left);
        layers(
            &registry(&fake),
            &store,
            &manifest(&[(GZIP, &big)]),
            &Limits::none(),
            &|_| {},
            &|_| {},
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(store.blob_path(&d)).unwrap(), big);
        let asked = ranges_asked(&fake.cdn, &d);
        assert_eq!(
            asked[0].to_ascii_lowercase(),
            format!("range: bytes=1000000-{}", big.len() - 1)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_uncompressed_layer_is_fetched_whole() {
        let tar = blob(100_000, 6);
        let fake = fake(
            vec![tar.clone()],
            Cdn {
                ranges: true,
                once: false,
            },
        );
        let (store, root) = store("tar");
        let media_type = "application/vnd.oci.image.layer.v1.tar";
        layers(
            &registry(&fake),
            &store,
            &manifest(&[(media_type, &tar)]),
            &Limits::none(),
            &|_| {},
            &|_| {},
            false,
        )
        .unwrap();
        assert_eq!(std::fs::read(store.blob_path(&digest(&tar))).unwrap(), tar);
        let asked = fake.cdn.requests();
        assert_eq!(asked.len(), 1);
        let asked = asked[0].to_ascii_lowercase();
        assert!(
            !asked.contains("\r\nrange:") && asked.contains("accept-encoding: zstd"),
            "{asked}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

//! The splunk driver's sending (D167): its events (logging/splunk.rs) posted to an HTTP
//! Event Collector, as moby's splunk driver posts them: in batches of 1,000, or as many as
//! `SPLUNK_LOGGING_DRIVER_POST_MESSAGES_BATCH_SIZE` in the daemon's environment says, with
//! `Authorization: Splunk TOKEN`, gzipped where asked, each post given 30 s. A batch is
//! posted once full, and whenever the shipper has caught up with its log (Driver::flush),
//! where moby's waits for its 5 s tick. It is delivered once HEC answers 200; with indexer
//! acknowledgment, once HEC says it is indexed, its ack asked for on the driver's channel,
//! and one HEC has not acknowledged within 5 minutes is posted again, as Splunk's
//! documentation asks of a client ("About HTTP Event Collector Indexer Acknowledgment").
//! None is ever dropped: moby's drops what it cannot post once 10,000 wait, and posts each
//! batch on a channel of its own, whose acks no one can ask for.

use std::io::{self, Read as _, Write as _};
use std::time::{Duration, Instant};

use shards_registry::http::{Client, Patience, Request};
use shards_registry::url::Url;

use super::{Driver, Message, Wait};
use crate::logging::splunk::{self, Config, From};

/// The events of a batch, unless the daemon's environment says (moby's
/// defaultPostMessagesBatchSize).
const BATCH: usize = 1000;
/// How long a post waits on HEC (moby's batchSendTimeout).
const POST: Duration = Duration::from_secs(30);
/// How long a batch posted waits for HEC to say it is indexed before it is posted again
/// (Splunk's documentation's example).
const UNACKNOWLEDGED: Duration = Duration::from_secs(5 * 60);
/// The most of an answer's body a refusal quotes (moby's maxResponseSize).
const QUOTED: usize = 1024;

pub(super) struct Splunk {
    config: Config,
    from: From,
    client: Client,
    events: Url,
    acks: Url,
    /// Indexer acknowledgment's channel: a GUID of the driver's own.
    channel: String,
    batch_size: usize,
    /// Events made, not yet posted, oldest first.
    batch: Vec<Vec<u8>>,
    /// Batches posted that HEC has not yet said are indexed: each its ack's ID, when it
    /// was posted, and its events.
    waiting: Vec<(u64, Instant, Vec<Vec<u8>>)>,
    /// How long a batch waits for its ack before it is posted again: [`UNACKNOWLEDGED`].
    unacknowledged: Duration,
}

impl Splunk {
    pub(super) fn new(config: Config, from: From) -> Result<Splunk, String> {
        let tls = shards_registry::tls::client_config(config.roots()?, None).map_err(|e| e.to_string())?;
        let mut client = Client::new(
            Box::new(move |_| Ok(tls.clone())),
            &format!("shards/{}", env!("CARGO_PKG_VERSION")),
        )
        .with_proxies(shards_registry::proxy::Proxies::from_env(&|k| {
            std::env::var(k).ok()
        }))
        .with_patience(Patience {
            head: Some(POST),
            stall: Some(POST),
        });
        if let Some(name) = &config.caname {
            client = client.with_server_name(name);
        }
        let channel = guid().map_err(|e| e.to_string())?;
        let events = Url::parse(&format!("{}{}", config.base, splunk::EVENTS)).map_err(|e| e.to_string())?;
        let acks = Url::parse(&format!("{}{}?channel={channel}", config.base, splunk::ACKS))
            .map_err(|e| e.to_string())?;
        let batch_size = std::env::var("SPLUNK_LOGGING_DRIVER_POST_MESSAGES_BATCH_SIZE")
            .ok()
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(BATCH);
        Ok(Splunk {
            config,
            from,
            client,
            events,
            acks,
            channel,
            batch_size,
            batch: Vec::new(),
            waiting: Vec::new(),
            unacknowledged: UNACKNOWLEDGED,
        })
    }

    /// Posts the batch, in posts of `batch_size` events, the oldest first.
    fn post(&mut self) -> io::Result<()> {
        while !self.batch.is_empty() {
            let n = self.batch.len().min(self.batch_size);
            let body = self.batch.get(..n).unwrap_or_default().concat();
            let body = match self.config.gzip {
                Some(level) => {
                    let level = flate2::Compression::new(u32::try_from(level).unwrap_or(6));
                    let mut e = flate2::write::GzEncoder::new(Vec::new(), level);
                    e.write_all(&body)?;
                    e.finish()?
                }
                None => body,
            };
            let answer = self.request(&self.events.clone(), &body, self.config.gzip.is_some(), 64)?;
            let events: Vec<Vec<u8>> = self.batch.drain(..n).collect();
            // With indexer acknowledgment, HEC names the post's ack; without, its 200 is
            // its word.
            if self.config.index_ack
                && let Some(id) = ack_id(&answer)
            {
                self.waiting.push((id, Instant::now(), events));
            }
        }
        Ok(())
    }

    /// Asks HEC which of the batches waiting it has indexed, and lets them go; those it
    /// has not acknowledged within [`UNACKNOWLEDGED`] go back to be posted again.
    fn ask_acks(&mut self) -> io::Result<()> {
        if self.waiting.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = self.waiting.iter().map(|(id, _, _)| id.to_string()).collect();
        let body = format!("{{\"acks\":[{}]}}", ids.join(","));
        // Its answer names each ack asked for once: `"ID":true,`.
        let most = ids.iter().map(|id| id.len() + 8).sum::<usize>() + 64;
        let answer = self.request(&self.acks.clone(), body.as_bytes(), false, most)?;
        let indexed = acked(&answer);
        self.waiting.retain(|(id, _, _)| !indexed.contains(id));
        let now = Instant::now();
        let (overdue, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|(_, at, _)| now.saturating_duration_since(*at) >= self.unacknowledged);
        self.waiting = kept;
        let mut again: Vec<Vec<u8>> = overdue.into_iter().flat_map(|(_, _, events)| events).collect();
        again.append(&mut self.batch);
        self.batch = again;
        Ok(())
    }

    /// Posts `body` to `url`, as moby's driver posts a batch: its token, gzip's encoding
    /// where `gzipped`, and with indexer acknowledgment its channel. Its answer's body, at
    /// most `keep` bytes of it, where HEC answers 200; else why not, its answer quoted.
    fn request(&self, url: &Url, body: &[u8], gzipped: bool, keep: usize) -> io::Result<Vec<u8>> {
        let auth = format!("Splunk {}", self.config.token);
        let mut headers = vec![("Authorization", auth.as_str())];
        if gzipped {
            headers.push(("Content-Encoding", "gzip"));
        }
        if self.config.index_ack {
            headers.push(("X-Splunk-Request-Channel", self.channel.as_str()));
        }
        let mut answer = self
            .client
            .send(&Request {
                method: "POST",
                url,
                headers: &headers,
                body,
                file: None,
            })
            .map_err(|e| io::Error::other(e.to_string()))?;
        let mut text = Vec::new();
        (&mut answer)
            .take(keep.max(QUOTED) as u64)
            .read_to_end(&mut text)?;
        // The rest read and let go, so that the connection serves the next post.
        io::copy(&mut answer, &mut io::sink())?;
        if answer.status != 200 {
            text.truncate(QUOTED);
            return Err(io::Error::other(format!(
                "splunk: {} - {}",
                answer.status_text(),
                String::from_utf8_lossy(&text)
            )));
        }
        Ok(text)
    }
}

impl Driver for Splunk {
    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        // A full batch goes first: a message is taken only where there is room for it.
        if self.batch.len() >= self.batch_size {
            self.post()?;
        }
        if let Some(event) = splunk::event(&self.config, &self.from, m.stream, m.at, m.line) {
            self.batch.push(event);
        }
        Ok(())
    }

    fn flush(&mut self, _wait: &Wait<'_>) -> io::Result<()> {
        self.post()?;
        self.ask_acks()?;
        // Those posted again as their acks are overdue.
        self.post()
    }

    fn undelivered(&mut self) -> usize {
        self.batch.len()
            + self
                .waiting
                .iter()
                .map(|(_, _, events)| events.len())
                .sum::<usize>()
    }

    fn close(&mut self, wait: &Wait<'_>) -> io::Result<()> {
        self.flush(wait)?;
        match self.waiting.len() {
            0 => Ok(()),
            n => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("{n} posts HEC has not yet said are indexed"),
            )),
        }
    }
}

/// A random GUID (RFC 9562's version 4), as indexer acknowledgment's channel is one.
fn guid() -> io::Result<String> {
    let mut b = [0u8; 16];
    shards_vmm::platform::fill_random(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        hex.get(..8).unwrap_or_default(),
        hex.get(8..12).unwrap_or_default(),
        hex.get(12..16).unwrap_or_default(),
        hex.get(16..20).unwrap_or_default(),
        hex.get(20..).unwrap_or_default()
    ))
}

/// The ack HEC names in its answer to a post, `ackId` (or, as its documentation spells
/// it, `ackID`), a number or a string of one.
fn ack_id(answer: &[u8]) -> Option<u64> {
    use shards_image::json::Value;
    let parsed = shards_image::json::parse(answer).ok()?;
    let Value::Object(fields) = &parsed else {
        return None;
    };
    fields.iter().find_map(|(k, v)| match (k.as_slice(), v) {
        (b"ackId" | b"ackID", Value::Number(n) | Value::String(n, _)) => {
            std::str::from_utf8(n).ok()?.parse().ok()
        }
        _ => None,
    })
}

/// The acks HEC says are indexed, of its answer to a query: `{"acks":{"ID":true,…}}`.
fn acked(answer: &[u8]) -> Vec<u64> {
    use shards_image::json::Value;
    let Ok(parsed) = shards_image::json::parse(answer) else {
        return Vec::new();
    };
    let Value::Object(fields) = &parsed else {
        return Vec::new();
    };
    fields
        .iter()
        .filter_map(|(k, v)| match (k.as_slice(), v) {
            (b"acks", Value::Object(acks)) => Some(acks),
            _ => None,
        })
        .flatten()
        .filter(|(_, v)| matches!(v, Value::Bool(true)))
        .filter_map(|(k, _)| std::str::from_utf8(k).ok()?.parse().ok())
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{BufRead as _, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;

    /// A request a fake HEC was sent.
    #[derive(Debug)]
    struct Seen {
        method: String,
        target: String,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    /// Serves the requests of connections to `listener` in turn, each answered as `answer`
    /// says and sent on `seen`, until a request whose method is QUIT.
    fn serve(listener: &TcpListener, answer: &dyn Fn(&Seen) -> (u16, String), seen: &mpsc::Sender<Seen>) {
        for conn in listener.incoming() {
            let mut reader = BufReader::new(conn.unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let mut parts = line.split_whitespace();
                let (method, target) = (
                    parts.next().unwrap().to_string(),
                    parts.next().unwrap().to_string(),
                );
                if method == "QUIT" {
                    return;
                }
                let mut headers = BTreeMap::new();
                loop {
                    let mut field = String::new();
                    reader.read_line(&mut field).unwrap();
                    let field = field.trim_end();
                    if field.is_empty() {
                        break;
                    }
                    let (k, v) = field.split_once(':').unwrap();
                    headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
                }
                let len: usize = headers.get("content-length").map_or(0, |l| l.parse().unwrap());
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).unwrap();
                let request = Seen {
                    method,
                    target,
                    headers,
                    body,
                };
                let (status, text) = answer(&request);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n{text}",
                    text.len()
                );
                reader.get_mut().write_all(reply.as_bytes()).unwrap();
                seen.send(request).unwrap();
            }
        }
    }

    /// Ends `serve` on its port as it drops, a test's assertion failing too: declared
    /// before the driver, which drops first, closing the connection the fake serves.
    struct Quit(u16);

    impl Drop for Quit {
        fn drop(&mut self) {
            if let Ok(mut conn) = TcpStream::connect(("127.0.0.1", self.0)) {
                let _ = conn.write_all(b"QUIT / HTTP/1.1\r\n\r\n");
            }
        }
    }

    fn splunk(port: u16, opts: &[(&str, &str)]) -> Splunk {
        let mut all: BTreeMap<String, String> = [
            ("splunk-url".to_string(), format!("http://127.0.0.1:{port}")),
            ("splunk-token".to_string(), "t".to_string()),
        ]
        .into();
        for (k, v) in opts {
            all.insert((*k).to_string(), (*v).to_string());
        }
        let from = From {
            host: "h".into(),
            tag: None,
            attrs: BTreeMap::new(),
        };
        Splunk::new(crate::logging::splunk::config(&all).unwrap(), from).unwrap()
    }

    fn message(line: &[u8]) -> Message<'_> {
        Message {
            stream: crate::spec::LOG_STDOUT,
            at: 1_791_260_014_360_799_172,
            line,
            part: None,
        }
    }

    /// The lines of a post's events.
    fn lines(body: &[u8]) -> Vec<String> {
        let text = String::from_utf8(body.to_vec()).unwrap();
        serde_json::Deserializer::from_str(&text)
            .into_iter::<serde_json::Value>()
            .map(|e| e.unwrap()["event"]["line"].as_str().unwrap().to_string())
            .collect()
    }

    /// Posts a batch once full and, as its shipper catches up, what it holds; takes a
    /// message only where there is room for it, so that one HEC refuses is given again,
    /// none lost and none twice; gzips where asked.
    #[test]
    fn batches_go_full_or_as_the_log_is_caught_up_with() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen, posts) = mpsc::channel();
        let refuse = std::sync::atomic::AtomicBool::new(false);
        let answer = |_: &Seen| {
            if refuse.load(std::sync::atomic::Ordering::Relaxed) {
                (503, r#"{"text":"Server is busy","code":9}"#.to_string())
            } else {
                (200, r#"{"text":"Success","code":0}"#.to_string())
            }
        };
        std::thread::scope(|scope| {
            scope.spawn(|| serve(&listener, &answer, &seen));
            let _quit = Quit(port);
            let (_stop, stopped) = UnixStream::pair().unwrap();
            let wait = Wait(&stopped);
            let mut hec = splunk(port, &[("splunk-gzip", "true")]);
            hec.batch_size = 2;
            for line in [b"a", b"b"] {
                hec.send(&message(line), &wait).unwrap();
            }
            // Full: refused, the third is not taken.
            refuse.store(true, std::sync::atomic::Ordering::Relaxed);
            let refused = hec.send(&message(b"c"), &wait).unwrap_err();
            assert!(refused.to_string().contains("503"), "{refused}");
            assert_eq!(hec.undelivered(), 2);
            refuse.store(false, std::sync::atomic::Ordering::Relaxed);
            hec.send(&message(b"c"), &wait).unwrap();
            hec.flush(&wait).unwrap();
            assert_eq!(hec.undelivered(), 0);
        });
        let posts: Vec<Seen> = posts.try_iter().collect();
        let mut got = Vec::new();
        for post in &posts {
            assert_eq!(
                (post.method.as_str(), post.target.as_str()),
                ("POST", splunk::EVENTS)
            );
            assert_eq!(post.headers["authorization"], "Splunk t");
            assert_eq!(post.headers["content-encoding"], "gzip");
            let mut body = Vec::new();
            flate2::read::GzDecoder::new(post.body.as_slice())
                .read_to_end(&mut body)
                .unwrap();
            got.push(lines(&body));
        }
        // The refused post, then the batch again, then what was left.
        assert_eq!(got, [vec!["a", "b"], vec!["a", "b"], vec!["c"]]);
    }

    /// With indexer acknowledgment, a post is delivered once HEC says it is indexed, its
    /// ack asked for on the driver's channel, and closing waits for it; one not acked in
    /// time is posted again.
    #[test]
    fn indexed_is_delivered() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen, requests) = mpsc::channel();
        let indexed = std::sync::atomic::AtomicBool::new(false);
        let posted = std::sync::atomic::AtomicU64::new(0);
        let answer = |r: &Seen| {
            if r.target.starts_with(splunk::ACKS) {
                let yes = indexed.load(std::sync::atomic::Ordering::Relaxed);
                let ids: Vec<u64> = serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["acks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap())
                    .collect();
                let acks: Vec<String> = ids.iter().map(|id| format!("\"{id}\":{yes}")).collect();
                (200, format!("{{\"acks\":{{{}}}}}", acks.join(",")))
            } else {
                let id = posted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                (200, format!("{{\"text\":\"Success\",\"code\":0,\"ackId\":{id}}}"))
            }
        };
        std::thread::scope(|scope| {
            scope.spawn(|| serve(&listener, &answer, &seen));
            let _quit = Quit(port);
            let (_stop, stopped) = UnixStream::pair().unwrap();
            let wait = Wait(&stopped);
            let mut hec = splunk(port, &[("splunk-index-acknowledgment", "true")]);
            hec.send(&message(b"a"), &wait).unwrap();
            hec.flush(&wait).unwrap();
            assert_eq!(hec.undelivered(), 1, "posted, not yet indexed");
            let waiting = hec.close(&wait).unwrap_err();
            assert_eq!(waiting.kind(), io::ErrorKind::WouldBlock);
            // Not acked in time: posted again.
            hec.unacknowledged = Duration::ZERO;
            hec.flush(&wait).unwrap();
            assert_eq!(hec.undelivered(), 1);
            indexed.store(true, std::sync::atomic::Ordering::Relaxed);
            hec.close(&wait).unwrap();
            assert_eq!(hec.undelivered(), 0);
        });
        let requests: Vec<Seen> = requests.try_iter().collect();
        let channel = &requests[0].headers["x-splunk-request-channel"];
        assert_eq!(channel.len(), 36, "a GUID: {channel}");
        let shape: Vec<(String, String)> = requests
            .iter()
            .map(|r| {
                assert_eq!(&r.headers["x-splunk-request-channel"], channel, "one channel");
                (r.method.clone(), r.target.clone())
            })
            .collect();
        let acks = format!("{}?channel={channel}", splunk::ACKS);
        let post = || ("POST".to_string(), splunk::EVENTS.to_string());
        let ask = || ("POST".to_string(), acks.clone());
        assert_eq!(shape, [post(), ask(), ask(), ask(), post(), ask()]);
        assert_eq!(lines(&requests[4].body), ["a"], "posted again");
    }
}

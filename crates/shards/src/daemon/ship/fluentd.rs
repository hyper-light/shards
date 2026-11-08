//! The fluentd driver's sending (D165): its events (logging/fluentd.rs) over TCP, TLS or a
//! unix socket, a write each, as fluent-logger-golang sends them unbuffered (fluent.go,
//! write): with `fluentd-request-ack`, the server's ack read back for each, its chunk's ID
//! matched; a failure tried again after `fluentd-retry-wait`, half as long again each time
//! up to a minute, `fluentd-max-retries` times; then the shipper's own waits, where
//! fluent-logger-golang gives the event up. Without acks, an event is held until its
//! receiver's TCP has it, and written again on the next connection should one end first
//! (stream.rs), where fluent-logger-golang's are lost.
//!
//! The forward protocol states no bound on an event, so one is held to what its
//! destination has shown it takes (learned.rs, D163): Fluent Bit's documented
//! `buffer_max_size` at first, then what it refuses. With acks, an event written whole on
//! two connections in a row and acked on neither, longer than the destination has taken,
//! is refused, and the shipper cuts it shorter (`ErrorKind::FileTooLarge`): Fluentd's
//! `in_forward` drops an event past its `chunk_size_limit` unacked, Fluent Bit ends the
//! connection. Without acks, an event its receiver ends connections on (stream.rs
//! `suspect`) is learned so and sent again cut, as a run's last is before its connection
//! ends (stream.rs `close`).
//!
//! An event cut again keeps its line's identity. The forward protocol names none of an
//! event's own: its only ID is `chunk`'s, "an ID of a set of events" that a client sends
//! to have it acked, the server then bound to ack it ("Server MUST respond `ack` when the
//! `chunk` option is sent by client"; Forward Protocol Specification v1, Option and
//! Response), so without acks there is none. A line's parts are joined by their records'
//! own fields, moby's `partial_id`, `partial_ordinal` and `partial_last`: fluent-plugin-concat
//! gathers a line's parts by its tag and `partial_id`, sorts them by `partial_ordinal`
//! and joins them at the one whose `partial_last` is "true" (filter_concat.rb,
//! fluent-plugins-nursery b8583840, `process_partial_metadata`, `buffered_lines`). So
//! the pieces of an event that was a line's part go under the line's `partial_id`, in its
//! place: they take its ordinal and those after it, each of the line's parts after them
//! goes as many places later (those held written again so, the shipper's still to come
//! numbered so), and the last piece is the line's last where the event was. The parts its
//! receiver had before it stand, and the line its receiver joins is the line. An event that
//! was a whole line goes as the parts of a line of its own ID: none of it was had under any.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;

use super::stream::{self, Stream};
use super::{Driver, Message, Wait};
use crate::logging::fluentd::{self, Config, From, To};

/// The longest wait between tries (fluent-logger-golang's defaultMaxRetryWait).
const MOST_WAIT: Duration = Duration::from_secs(60);
/// The longest string an ack's map may hold: its key and its ID, 24 bytes of base64.
const ACK_MOST: usize = 1024;
/// The most of an event's bytes a receiver of Fluentd's forward protocol takes unless set
/// otherwise, where a destination's own bound starts (learned.rs): Fluent Bit's
/// `buffer_max_size`, "the maximum buffer memory size used to receive a Forward message",
/// 6144000 by default, past which a payload is "rejected and the connection is closed"
/// (Fluent Bit's documentation, pipeline/inputs/forward.md, fluent-bit-docs b6e1c922).
/// Fluentd's own `in_forward` bounds none by default (`chunk_size_limit`, nil: fluentd
/// v1.19.0 lib/fluent/plugin/in_forward.rb).
const FORWARD_MOST: usize = 6_144_000;

pub(super) struct Fluentd {
    config: Config,
    from: From,
    stream: Stream,
    /// The most of a line one event carries: [`FORWARD_MOST`] less the rest of its event.
    part: usize,
    /// What its destination has shown it takes of an event.
    learned: super::learned::Learned,
    /// Without acks, the line of each event the stream holds, its part's place, and its
    /// event's length, to cut it again.
    held: VecDeque<Held>,
    /// The lines one of whose parts was cut again, their last part not yet sent: each one's
    /// ID, and how many places later than the shipper numbers them its parts go.
    later: Vec<(String, u32)>,
}

/// An event a stream holds: its stream, time, line, part's place, and length.
type Held = (u8, u64, Vec<u8>, Option<(String, u32, bool)>, usize);

impl Fluentd {
    /// The driver `config` says, from what `home` keeps of its destination.
    pub(super) fn new(config: Config, from: From, home: &Path) -> Result<Fluentd, String> {
        let address = match &config.to {
            To::Tcp(a) | To::Tls(a) => a.clone(),
            To::Unix(path) => path.display().to_string(),
        };
        let learned = super::learned::Learned::open(home, "fluentd", &address, FORWARD_MOST);
        let to = match &config.to {
            To::Tcp(address) => stream::To::Tcp(address.clone()),
            // The platform's roots, as fluent-logger-golang's tls.Dial verifies.
            To::Tls(address) => stream::To::Tls(
                address.clone(),
                shards_registry::tls::client_config(Vec::new(), None).map_err(|e| e.to_string())?,
            ),
            To::Unix(path) => stream::To::Unix(path.clone()),
        };
        let stream = Stream::new(to, [config.write_timeout, config.read_timeout]);
        // With acks, its acks say what its receiver took; without, its connections' ends are
        // asked by a message of nothing (stream.rs `Asking`).
        let stream = if config.ack {
            stream.answered()
        } else {
            stream.probing(fluentd::nothing(&from))
        };
        let part = part(&config, &from);
        Ok(Fluentd {
            config,
            from,
            stream,
            part,
            learned,
            held: VecDeque::new(),
            later: Vec::new(),
        })
    }

    /// The length of `m`'s event, its chunk's ID at its length where acks are asked for,
    /// its part in the place it goes.
    fn event_len(&self, m: &Message<'_>) -> usize {
        let chunk = self.config.ack.then(|| "A".repeat(24));
        fluentd::event(
            &self.config,
            &self.from,
            m.stream,
            m.at,
            m.line,
            self.placed(m.part),
            chunk.as_deref(),
        )
        .len()
    }

    /// Without acks, follows what the destination shows (as gelf.rs `follow`): the events
    /// the stream no longer holds it has; one it points at as refused is learned so, and
    /// its line put back cut, in its line's place (this module's words).
    fn follow(&mut self) -> io::Result<()> {
        if self.config.ack {
            return Ok(());
        }
        while self.held.len() > self.stream.held() {
            if let Some((.., len)) = self.held.pop_front() {
                self.learned.taken(len);
            }
        }
        let Some(i) = self.stream.suspect(self.learned.took()) else {
            return Ok(());
        };
        let Some((st, at, line, part, len)) = self.held.get(i).cloned() else {
            return Ok(());
        };
        self.learned.refused(len);
        let (config, from, most) = (&self.config, &self.from, self.learned.most());
        // Its line's ID, its place and whether it was the last; a whole line's own.
        let (id, first, last) = match &part {
            Some((id, ordinal, last)) => (id.clone(), *ordinal, *last),
            None => (super::partial_id()?, 1, true),
        };
        let event = |l: &[u8], ordinal: u32, last: bool| {
            fluentd::event(
                config,
                from,
                st,
                at,
                l,
                Some(fluentd::Part {
                    id: &id,
                    ordinal,
                    last,
                }),
                None,
            )
        };
        let pieces = super::halves(&line, &|l| event(l, u32::MAX, false).len() <= most);
        let count = pieces.len();
        let mut events = Vec::with_capacity(count);
        let mut entries = Vec::with_capacity(count);
        let mut ordinal = first;
        for (k, l) in pieces.into_iter().enumerate() {
            let last = last && k + 1 == count;
            let e = event(&l, ordinal, last);
            entries.push((st, at, l, Some((id.clone(), ordinal, last)), e.len()));
            events.push(e);
            ordinal = ordinal.saturating_add(1);
        }
        // How many places later the line's parts after it go.
        let later = u32::try_from(count.saturating_sub(1)).unwrap_or(u32::MAX);
        self.stream.replace(i, events);
        let rest = self.held.split_off(i);
        self.held.extend(entries);
        self.held.extend(rest.into_iter().skip(1));
        if part.is_none() || later == 0 {
            return Ok(());
        }
        let mut rewritten = Vec::new();
        let mut ended = last;
        for (k, (st, at, l, part, len)) in self.held.iter_mut().enumerate().skip(i + count) {
            let Some((pid, ordinal, last)) = part else {
                continue;
            };
            if *pid != id {
                continue;
            }
            *ordinal = ordinal.saturating_add(later);
            ended |= *last;
            let e = fluentd::event(
                &self.config,
                &self.from,
                *st,
                *at,
                l,
                Some(fluentd::Part {
                    id: pid,
                    ordinal: *ordinal,
                    last: *last,
                }),
                None,
            );
            *len = e.len();
            rewritten.push((k, e));
        }
        self.stream.rewrite(rewritten);
        // The shipper's parts of it still to come.
        if !ended {
            match self.later.iter_mut().find(|(pid, _)| *pid == id) {
                Some((_, n)) => *n = n.saturating_add(later),
                None => self.later.push((id, later)),
            }
        }
        Ok(())
    }

    /// `part` placed as this driver has its line's parts go ([`Fluentd::later`]).
    fn placed<'p>(&self, part: Option<fluentd::Part<'p>>) -> Option<fluentd::Part<'p>> {
        part.map(|p| {
            let later = self
                .later
                .iter()
                .find(|(id, _)| id == p.id)
                .map_or(0, |(_, n)| *n);
            fluentd::Part {
                ordinal: p.ordinal.saturating_add(later),
                ..p
            }
        })
    }

    /// Writes `event`, and where `chunk` is given, reads its ack; `written` says whether
    /// it was written whole.
    fn try_send(
        &mut self,
        event: &[u8],
        chunk: Option<&str>,
        written: &mut bool,
        wait: &Wait<'_>,
    ) -> io::Result<()> {
        self.stream.send(event, wait)?;
        *written = true;
        if let Some(chunk) = chunk {
            let ack = read_ack(&mut self.stream)?;
            if ack != chunk.as_bytes() {
                return Err(io::Error::other(format!(
                    "an ack of {} where {chunk} was sent",
                    String::from_utf8_lossy(&ack)
                )));
            }
            // Its receiver has it, and so all before it.
            self.stream.let_go();
        }
        Ok(())
    }
}

/// The most of a line one event carries: what [`FORWARD_MOST`] leaves past the rest of the
/// longest event this run's lines make: a part's place at its widest (its ID, the largest
/// ordinal, not its last), its time at its widest, its chunk's ID where acks are asked for,
/// and the line's string header at its widest (MessagePack's str 32, 5 bytes, where an
/// empty one takes 1).
fn part(config: &Config, from: &From) -> usize {
    let id = "f".repeat(64);
    let chunk = config.ack.then(|| "A".repeat(24));
    let rest = fluentd::event(
        config,
        from,
        crate::spec::LOG_STDERR,
        u64::MAX,
        b"",
        Some(fluentd::Part {
            id: &id,
            ordinal: u32::MAX,
            last: false,
        }),
        chunk.as_deref(),
    )
    .len();
    FORWARD_MOST.saturating_sub(rest + 4).max(1)
}

/// An event's chunk ID, as fluent-logger-golang's getUniqueID makes one: its time's
/// seconds and a random number, each eight bytes little-endian, in base64.
fn unique_id(at: u64) -> io::Result<String> {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(
        &i64::try_from(at / 1_000_000_000)
            .unwrap_or(i64::MAX)
            .to_le_bytes(),
    );
    shards_vmm::platform::fill_random(&mut bytes[8..])?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// The ack a server sends for a chunk: a MessagePack map whose `ack` names it.
fn read_ack(conn: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut byte = [0u8; 1];
    conn.read_exact(&mut byte)?;
    let len = match byte[0] {
        b @ 0x80..=0x8f => usize::from(b & 0x0f),
        0xde => usize::from(u16::from_be_bytes(take(conn)?)),
        0xdf => usize::try_from(u32::from_be_bytes(take(conn)?)).unwrap_or(usize::MAX),
        b => return Err(io::Error::other(format!("an ack that is no map: {b:#04x}"))),
    };
    let mut ack = None;
    for _ in 0..len {
        let key = read_string(conn)?;
        let value = read_string(conn)?;
        if key == b"ack" {
            ack = Some(value);
        }
    }
    ack.ok_or_else(|| io::Error::other("an ack without its ID"))
}

fn take<const N: usize>(conn: &mut impl Read) -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    conn.read_exact(&mut b)?;
    Ok(b)
}

/// A MessagePack string, or bytes, no longer than [`ACK_MOST`].
fn read_string(conn: &mut impl Read) -> io::Result<Vec<u8>> {
    let [b] = take::<1>(conn)?;
    let len = match b {
        0xa0..=0xbf => usize::from(b & 0x1f),
        0xd9 | 0xc4 => usize::from(take::<1>(conn)?[0]),
        0xda | 0xc5 => usize::from(u16::from_be_bytes(take(conn)?)),
        0xdb | 0xc6 => usize::try_from(u32::from_be_bytes(take(conn)?)).unwrap_or(usize::MAX),
        b => {
            return Err(io::Error::other(format!(
                "an ack's member that is no string: {b:#04x}"
            )));
        }
    };
    if len > ACK_MOST {
        return Err(io::Error::other(format!("an ack's string of {len} bytes")));
    }
    let mut s = vec![0u8; len];
    conn.read_exact(&mut s)?;
    Ok(s)
}

impl Driver for Fluentd {
    fn part(&self) -> usize {
        self.part
    }

    /// Within what its destination has shown it takes (learned.rs).
    fn fits(&mut self, m: &Message<'_>) -> io::Result<bool> {
        Ok(self.event_len(m) <= self.learned.most())
    }

    fn send(&mut self, m: &Message<'_>, wait: &Wait<'_>) -> io::Result<()> {
        let chunk = if self.config.ack {
            Some(unique_id(m.at)?)
        } else {
            None
        };
        let mut pause = self.config.retry_wait;
        let mut tries = 0u32;
        // Tries in a row on which the event went first on its connection, whole, and was not
        // acked.
        let mut unacked = 0u32;
        loop {
            // What the destination showed since, followed first: a part of this line cut
            // again places this one later.
            self.follow()?;
            let placed = self.placed(m.part);
            let event = fluentd::event(
                &self.config,
                &self.from,
                m.stream,
                m.at,
                m.line,
                placed,
                chunk.as_deref(),
            );
            let mut written = false;
            let Err(e) = self.try_send(&event, chunk.as_deref(), &mut written, wait) else {
                if chunk.is_some() {
                    self.learned.taken(event.len());
                } else {
                    let part = placed.map(|p| (p.id.to_string(), p.ordinal, p.last));
                    self.held
                        .push_back((m.stream, m.at, m.line.to_vec(), part, event.len()));
                }
                if let Some(p) = placed.filter(|p| p.last) {
                    self.later.retain(|(id, _)| id != p.id);
                }
                return Ok(());
            };
            if chunk.is_some() && written {
                // An ack missing on a connection its receiver may have ended after its last
                // (in_forward's `deny_keepalive` closes one once its ack is written) says
                // nothing of the event: it goes again at once, first on one of its own.
                if !self.stream.first_on_its_connection() {
                    self.stream.drop_connection();
                    self.stream.let_go();
                    continue;
                }
                unacked += 1;
                // Each a connection of its own, the event its first: one never acked,
                // longer than any the destination took, is past its bound.
                if unacked >= 2 && event.len() > self.learned.took() && m.line.len() > 1 {
                    self.learned.refused(event.len());
                    self.stream.drop_connection();
                    self.stream.let_go();
                    return Err(io::Error::new(
                        io::ErrorKind::FileTooLarge,
                        format!(
                            "an event of {} bytes its receiver did not ack on two connections: {e}",
                            event.len()
                        ),
                    ));
                }
            } else {
                unacked = 0;
            }
            // An ack not read leaves its connection's state unknown; the event, held, is
            // this loop's to send again.
            if chunk.is_some() {
                self.stream.drop_connection();
                self.stream.let_go();
            }
            if tries >= self.config.max_retries {
                return Err(e);
            }
            tries += 1;
            if wait.pause(pause) {
                return Err(super::stops());
            }
            pause = pause.mul_f64(1.5).min(MOST_WAIT);
        }
    }

    fn undelivered(&mut self) -> usize {
        let n = self.stream.undelivered();
        if self.follow().is_err() {
            return n;
        }
        self.stream.held()
    }

    fn close(&mut self, wait: &Wait<'_>) -> io::Result<()> {
        self.follow()?;
        self.stream.close(wait)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;

    /// Do: with acks, send to a receiver of the forward protocol that acks each event of at
    /// most 2,000 bytes and ends its connection, unacked, on reading more, as Fluent Bit's
    /// input ends one past its `buffer_max_size`: a short line, then one of 5,000 bytes,
    /// then its halves. Expect the short one acked; the long one, written on two
    /// connections and acked on neither, refused as too long (`FileTooLarge`), the shipper's
    /// to cut, the destination's bound learned below it so that it no longer fits; its
    /// halves then acked.
    #[test]
    fn an_event_acked_on_no_connection_is_refused_as_too_long() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            const CHUNK: &[u8] = b"\xa5chunk\xb8";
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut held: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                while let Ok(n) = conn.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    held.extend_from_slice(&buf[..n]);
                    if held.len() > 2000 {
                        break;
                    }
                    let Some(at) = held.windows(CHUNK.len()).position(|w| w == CHUNK) else {
                        continue;
                    };
                    let Some(id) = held.get(at + CHUNK.len()..at + CHUNK.len() + 24) else {
                        continue;
                    };
                    let mut ack = vec![0x81, 0xa3, b'a', b'c', b'k', 0xb8];
                    ack.extend_from_slice(id);
                    held.clear();
                    if conn.write_all(&ack).is_err() {
                        break;
                    }
                }
            }
        });
        let home = shards_testdir::TempDir::new("fluentd-ack").unwrap();
        let config = Config {
            to: To::Tcp(address),
            sub_second: false,
            ack: true,
            retry_wait: Duration::from_millis(10),
            max_retries: 3,
            write_timeout: Some(Duration::from_secs(10)),
            read_timeout: Some(Duration::from_secs(10)),
        };
        let from = From {
            tag: "t".into(),
            id: "0123".into(),
            name: "/n".into(),
            extra: std::collections::BTreeMap::new(),
        };
        let mut f = Fluentd::new(config, from, &home).unwrap();
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        let wait = Wait(&stopped);
        fn m(line: &[u8]) -> Message<'_> {
            Message {
                stream: crate::spec::LOG_STDOUT,
                at: 1_000_000_000,
                line,
                part: None,
            }
        }
        f.send(&m(b"short"), &wait).unwrap();
        let long = vec![b'x'; 5000];
        let e = f.send(&m(&long), &wait).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::FileTooLarge, "{e}");
        assert!(!f.fits(&m(&long)).unwrap());
        for half in long.chunks(2500) {
            assert!(f.fits(&m(&half[..1500])).unwrap());
            f.send(&m(&half[..1500]), &wait).unwrap();
        }
    }

    /// Where the MessagePack value at `b[at]` ends, of the forms an event holds; none where
    /// `b` holds only its beginning.
    fn skip(b: &[u8], at: usize) -> Option<usize> {
        let t = *b.get(at)?;
        let be = |from: usize, n: usize| -> Option<usize> {
            Some(
                b.get(from..from + n)?
                    .iter()
                    .fold(0usize, |v, &x| v << 8 | usize::from(x)),
            )
        };
        let next = at + 1;
        let (body, items) = match t {
            0x00..=0x7f | 0xc0 | 0xc2 | 0xc3 => (next, 0),
            0x80..=0x8f => (next, 2 * usize::from(t & 0x0f)),
            0x90..=0x9f => (next, usize::from(t & 0x0f)),
            0xa0..=0xbf => return Some(next + usize::from(t & 0x1f)).filter(|&e| e <= b.len()),
            0xc4 | 0xd9 => return Some(next + 1 + be(next, 1)?).filter(|&e| e <= b.len()),
            0xc5 | 0xda => return Some(next + 2 + be(next, 2)?).filter(|&e| e <= b.len()),
            0xc6 | 0xdb => return Some(next + 4 + be(next, 4)?).filter(|&e| e <= b.len()),
            0xcc | 0xd0 => return Some(next + 1).filter(|&e| e <= b.len()),
            0xcd | 0xd1 => return Some(next + 2).filter(|&e| e <= b.len()),
            0xce | 0xd2 => return Some(next + 4).filter(|&e| e <= b.len()),
            0xcf | 0xd3 => return Some(next + 8).filter(|&e| e <= b.len()),
            0xd7 => return Some(next + 9).filter(|&e| e <= b.len()),
            0xdc => (next + 2, be(next, 2)?),
            0xdd => (next + 4, be(next, 4)?),
            0xde => (next + 2, 2 * be(next, 2)?),
            0xdf => (next + 4, 2 * be(next, 4)?),
            t => panic!("a form {t:#x} no event holds"),
        };
        let mut end = body;
        for _ in 0..items {
            end = skip(b, end)?;
        }
        Some(end)
    }

    /// How a test's receiver is reached.
    #[derive(Clone, Copy, Debug)]
    enum Via {
        Tcp,
        Tls,
        Unix,
    }

    /// How a test's receiver ends connections.
    #[derive(Clone, Copy, Debug)]
    enum Ends {
        /// As Fluent Bit's forward input does past its `buffer_max_size`: on reading an
        /// event longer than this.
        Past(usize),
        /// As Fluentd's in_forward does with `deny_keepalive`: after every message it
        /// handles, once its ack, where one is asked, is written (in_forward.rb
        /// `handle_connection`).
        AfterEach,
    }

    /// A forward receiver for the tests, ending connections as `ends` says (over TLS with
    /// close_notify, its TCP's writing then shut): each event it takes sent on `got`, and
    /// its `chunk` acked where one is asked. A driver that sends to it from `home`, with
    /// acks where `ack`.
    fn receiver(
        via: Via,
        home: &Path,
        ends: Ends,
        ack: bool,
    ) -> (Fluentd, std::sync::mpsc::Receiver<Vec<u8>>) {
        fn serve<C: Read + std::io::Write>(
            mut conn: C,
            ends: Ends,
            got: &std::sync::mpsc::Sender<Vec<u8>>,
            end: impl FnOnce(C),
        ) -> bool {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match conn.read(&mut chunk) {
                    Ok(0) | Err(_) => return true,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
                while let Some(len) = skip(&buf, 0) {
                    let event: Vec<u8> = buf.drain(..len).collect();
                    if let Ends::Past(most) = ends
                        && event.len() > most
                    {
                        end(conn);
                        return true;
                    }
                    let mut at = 0;
                    let chunk = crate::logging::fluentd::tests::unpack(&event, &mut at)[3]["chunk"]
                        .as_str()
                        .map(str::to_string);
                    if got.send(event).is_err() {
                        return false;
                    }
                    if let Some(chunk) = chunk {
                        let mut ack = vec![0x81, 0xa3, b'a', b'c', b'k', 0xb8];
                        ack.extend_from_slice(chunk.as_bytes());
                        if conn.write_all(&ack).and_then(|()| conn.flush()).is_err() {
                            return true;
                        }
                    }
                    if let Ends::AfterEach = ends {
                        end(conn);
                        return true;
                    }
                }
            }
        }
        let (got, events) = std::sync::mpsc::channel();
        let (to, tls) = match via {
            Via::Tcp | Via::Tls => {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let port = listener.local_addr().unwrap().port();
                let tls = matches!(via, Via::Tls).then(super::super::stream::tests::tls);
                let server = tls.as_ref().map(|(server, _)| server.clone());
                std::thread::spawn(move || {
                    let mut kept = Vec::new();
                    for conn in listener.incoming() {
                        let Ok(conn) = conn else { return };
                        let going = match &server {
                            None => serve(conn, ends, &got, drop),
                            Some(server) => {
                                let tls = rustls::StreamOwned::new(
                                    rustls::ServerConnection::new(server.clone()).unwrap(),
                                    conn,
                                );
                                serve(tls, ends, &got, |mut tls| {
                                    tls.conn.send_close_notify();
                                    let _ = std::io::Write::flush(&mut tls);
                                    let _ = tls.sock.shutdown(std::net::Shutdown::Write);
                                    kept.push(tls);
                                })
                            }
                        };
                        if !going {
                            return;
                        }
                    }
                });
                // TLS's by the name its certificate has.
                let host = if tls.is_some() { "localhost" } else { "127.0.0.1" };
                (To::Tcp(format!("{host}:{port}")), tls)
            }
            Via::Unix => {
                let path = home.join("f.sock");
                let listener = shards_vmm::platform::UnixListener::bind(&path).unwrap();
                std::thread::spawn(move || {
                    loop {
                        let Ok((conn, _)) = listener.accept() else { return };
                        if !serve(conn, ends, &got, drop) {
                            return;
                        }
                    }
                });
                (To::Unix(path), None)
            }
        };
        let config = Config {
            to: to.clone(),
            sub_second: false,
            ack,
            retry_wait: Duration::from_millis(10),
            max_retries: 3,
            write_timeout: Some(Duration::from_secs(10)),
            read_timeout: Some(Duration::from_secs(10)),
        };
        let from = From {
            tag: "t".into(),
            id: "0123".into(),
            name: "/n".into(),
            extra: std::collections::BTreeMap::new(),
        };
        let mut f = Fluentd::new(config, from, home).unwrap();
        if let (Some((_, client)), To::Tcp(address)) = (tls, to) {
            let stream = Stream::new(stream::To::Tls(address, client), [None; 2]);
            f.stream = if ack {
                stream.answered()
            } else {
                stream.probing(fluentd::nothing(&f.from))
            };
        }
        (f, events)
    }

    /// The line `events` make of the parts of ID `id`, as a receiver that joins a line's
    /// parts by their places (fluent-plugin-concat's `buffered_lines`) makes it; each
    /// place's part checked to be one line, the same wherever it came again, the places
    /// one after another from 1, and the last part the last place's alone.
    fn joined(events: &[serde_json::Value], id: &str) -> String {
        let mut parts: std::collections::BTreeMap<u32, (String, bool)> = Default::default();
        for e in events {
            let record = &e[2];
            if record["partial_id"] != id {
                continue;
            }
            let ordinal: u32 = record["partial_ordinal"].as_str().unwrap().parse().unwrap();
            let part = (
                record["log"].as_str().unwrap().to_string(),
                record["partial_last"] == "true",
            );
            let had = parts.entry(ordinal).or_insert_with(|| part.clone());
            assert_eq!(*had, part, "place {ordinal} came as two parts");
        }
        let places: Vec<u32> = parts.keys().copied().collect();
        assert_eq!(
            places,
            (1..=places.len() as u32).collect::<Vec<_>>(),
            "places of {id}"
        );
        let lasts: Vec<u32> = parts
            .iter()
            .filter(|(_, (_, last))| *last)
            .map(|(o, _)| *o)
            .collect();
        assert_eq!(lasts, vec![places.len() as u32], "the last of {id}");
        parts.into_values().map(|(log, _)| log).collect()
    }

    /// The events received, decoded, but messages of nothing.
    fn decoded(events: &std::sync::mpsc::Receiver<Vec<u8>>, most: usize) -> Vec<serde_json::Value> {
        events
            .try_iter()
            .map(|e| {
                assert!(e.len() <= most, "{}", e.len());
                let mut at = 0;
                let v = crate::logging::fluentd::tests::unpack(&e, &mut at);
                assert_eq!(at, e.len());
                v
            })
            .filter(|v| !v[1].is_array())
            .collect()
    }

    /// Do: without acks, over TCP, TLS and a unix socket, to a receiver that ends its
    /// connection on any event of more than 2,000 bytes, send a line's three parts as a
    /// shipper does, its second of 3,000 bytes, its third the run's last; then close the
    /// driver as its shipper does, again while it says its receiver ended the connection
    /// with events held. Expect, before the close ends, every event within 2,000 bytes, and
    /// the line rebuilt whole from its parts by their places under its one ID: the second
    /// cut into pieces in its place, the third later by as many, still the last.
    #[test]
    fn a_part_refused_goes_again_in_its_lines_place() {
        for via in [Via::Tcp, Via::Tls, Via::Unix] {
            let home = shards_testdir::TempDir::new("fw").unwrap();
            let (mut f, events) = receiver(via, &home, Ends::Past(2000), false);
            let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
            let wait = Wait(&stopped);
            let id = "a".repeat(64);
            let long = "b".repeat(3000);
            let parts = [("first", 1, false), (long.as_str(), 2, false), ("third", 3, true)];
            for (line, ordinal, last) in parts {
                f.send(
                    &Message {
                        stream: crate::spec::LOG_STDOUT,
                        at: 1_000_000_000,
                        line: line.as_bytes(),
                        part: Some(fluentd::Part {
                            id: &id,
                            ordinal,
                            last,
                        }),
                    },
                    &wait,
                )
                .unwrap();
            }
            super::super::gelf::tests::close_until_ended(&mut || f.close(&wait));
            // The receiver's thread has sent all it read: the connection ended only once it
            // had read all.
            let events = decoded(&events, 2000);
            assert_eq!(joined(&events, &id), format!("first{long}third"), "{via:?}");
            assert!(f.learned.most() < 3000, "{via:?}");
        }
    }

    /// Do: without acks, over TCP, to the same receiver, send a line's first part and a
    /// second of 3,000 bytes, and follow the driver until the long part is shown refused;
    /// follow it, then send the line's third and last part, and close. Expect the long part
    /// cut in its place, the third, which the shipper numbers 3, placed after the pieces:
    /// the line rebuilt whole.
    #[test]
    fn a_lines_parts_still_to_come_go_after_a_part_cut_again() {
        let home = shards_testdir::TempDir::new("fw").unwrap();
        let (mut f, events) = receiver(Via::Tcp, &home, Ends::Past(2000), false);
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        let wait = Wait(&stopped);
        let id: &'static str = String::leak("c".repeat(64));
        let long: &'static str = String::leak("d".repeat(3000));
        let m = |line: &'static str, ordinal, last| Message {
            stream: crate::spec::LOG_STDOUT,
            at: 1_000_000_000,
            line: line.as_bytes(),
            part: Some(fluentd::Part { id, ordinal, last }),
        };
        f.send(&m("first", 1, false), &wait).unwrap();
        f.send(&m(long, 2, false), &wait).unwrap();
        let started = std::time::Instant::now();
        while f.stream.suspect(f.learned.took()).is_none() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "nothing shown refused"
            );
            std::thread::sleep(Duration::from_millis(5));
            f.stream.undelivered();
        }
        f.follow().unwrap();
        assert!(f.later.iter().any(|(i, n)| i == id && *n > 0), "{:?}", f.later);
        f.send(&m("third", 3, true), &wait).unwrap();
        assert!(f.later.is_empty(), "a line's places kept past its last part");
        super::super::gelf::tests::close_until_ended(&mut || f.close(&wait));
        let events = decoded(&events, 2000);
        assert_eq!(joined(&events, id), format!("first{long}third"));
    }

    /// Do: over TCP, TLS and a unix socket, with acks and without, to a receiver that ends
    /// its connection after every message it takes, as in_forward with `deny_keepalive`
    /// does (its ack written first, where one is asked): send whole lines and a line in
    /// three parts, one of them 3,000 bytes, as a shipper does, then close. Expect nothing
    /// lost and nothing cut: every line received, each part as it was sent, its places as
    /// the shipper gave them; nothing learned refused; and, without acks, the stream found
    /// to have a receiver that ends a connection after each message.
    #[test]
    fn a_receiver_that_ends_a_connection_after_each_message_loses_and_cuts_nothing() {
        for (via, ack) in [
            (Via::Tcp, false),
            (Via::Tls, false),
            (Via::Unix, false),
            (Via::Tcp, true),
        ] {
            let home = shards_testdir::TempDir::new("fw").unwrap();
            let (mut f, events) = receiver(via, &home, Ends::AfterEach, ack);
            let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
            let wait = Wait(&stopped);
            let id = "e".repeat(64);
            let long = "f".repeat(3000);
            let whole = |line: &str| (line.to_string(), None);
            let sent: Vec<(String, Option<(u32, bool)>)> = vec![
                whole("one"),
                whole("two"),
                ("first".into(), Some((1, false))),
                (long.clone(), Some((2, false))),
                ("third".into(), Some((3, true))),
                whole("three"),
            ];
            for (line, part) in &sent {
                f.send(
                    &Message {
                        stream: crate::spec::LOG_STDOUT,
                        at: 1_000_000_000,
                        line: line.as_bytes(),
                        part: part.map(|(ordinal, last)| fluentd::Part {
                            id: &id,
                            ordinal,
                            last,
                        }),
                    },
                    &wait,
                )
                .unwrap();
            }
            super::super::gelf::tests::close_until_ended(&mut || f.close(&wait));
            let events = decoded(&events, usize::MAX);
            let got: std::collections::BTreeSet<(String, Option<(u32, bool)>)> = events
                .iter()
                .map(|e| {
                    let r = &e[2];
                    let part = r["partial_ordinal"]
                        .as_str()
                        .map(|o| (o.parse().unwrap(), r["partial_last"] == "true"));
                    (r["log"].as_str().unwrap().to_string(), part)
                })
                .collect();
            let want: std::collections::BTreeSet<_> = sent.iter().cloned().collect();
            assert_eq!(got, want, "{via:?} ack {ack}");
            assert_eq!(
                f.learned.most(),
                FORWARD_MOST,
                "{via:?} ack {ack}: learned refused"
            );
            if !ack {
                assert!(
                    f.stream.closes(),
                    "{via:?}: its ends not found to follow each message"
                );
            }
        }
    }
}

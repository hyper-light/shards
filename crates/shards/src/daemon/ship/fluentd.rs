//! The fluentd driver's sending (D165): its events (logging/fluentd.rs) over TCP, TLS or a
//! unix socket, a write each, as fluent-logger-golang sends them unbuffered (fluent.go,
//! write): with `fluentd-request-ack`, the server's ack read back for each, its chunk's ID
//! matched; a failure tried again after `fluentd-retry-wait`, half as long again each time
//! up to a minute, `fluentd-max-retries` times; then the shipper's own waits, where
//! fluent-logger-golang gives the event up. Without acks, an event is held until its
//! receiver's TCP has it, and written again on the next connection should one end first
//! (stream.rs), where fluent-logger-golang's are lost.

use std::io::{self, Read};
use std::time::Duration;

use base64::Engine as _;

use super::stream::{self, Stream};
use super::{Driver, Message, Wait};
use crate::logging::fluentd::{self, Config, From, To};

/// The longest wait between tries (fluent-logger-golang's defaultMaxRetryWait).
const MOST_WAIT: Duration = Duration::from_secs(60);
/// The longest string an ack's map may hold: its key and its ID, 24 bytes of base64.
const ACK_MOST: usize = 1024;
/// The most of an event's bytes a receiver of Fluentd's forward protocol takes: Fluent Bit's
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
}

impl Fluentd {
    pub(super) fn new(config: Config, from: From) -> Result<Fluentd, String> {
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
        let part = part(&config, &from);
        Ok(Fluentd {
            config,
            from,
            stream,
            part,
        })
    }

    /// Writes `event`, and where `chunk` is given, reads its ack.
    fn try_send(&mut self, event: &[u8], chunk: Option<&str>) -> io::Result<()> {
        self.stream.send(event)?;
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

    fn send(&mut self, m: &Message<'_>, wait: &Wait<'_>) -> io::Result<()> {
        let chunk = if self.config.ack {
            Some(unique_id(m.at)?)
        } else {
            None
        };
        let event = fluentd::event(
            &self.config,
            &self.from,
            m.stream,
            m.at,
            m.line,
            m.part,
            chunk.as_deref(),
        );
        let mut pause = self.config.retry_wait;
        let mut tries = 0u32;
        loop {
            match self.try_send(&event, chunk.as_deref()) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    // An ack not read leaves its connection's state unknown; the event,
                    // held, is this loop's to send again.
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
        }
    }

    fn undelivered(&mut self) -> usize {
        self.stream.undelivered()
    }

    fn close(&mut self, wait: &Wait<'_>) -> io::Result<()> {
        self.stream.close(wait)
    }
}

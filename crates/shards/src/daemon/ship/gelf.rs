//! The gelf driver's sending (D164): its messages (logging/gelf.rs) over UDP, compressed
//! and in chunks, a datagram each, or over TCP, each followed by a NUL, as go-gelf sends
//! them (udpwriter.go, tcpwriter.go). Over TCP a write that fails is tried again on a new
//! connection, `gelf-tcp-max-reconnect` times, `gelf-tcp-reconnect-delay` seconds apart,
//! as go-gelf tries it; then the shipper's own waits take over, where go-gelf drops the
//! message. And a message is held until its receiver has it, and written again on the
//! next connection should one end first (stream.rs), where go-gelf's are lost. Over TCP,
//! where GELF states no bound, a message is held to what its destination has shown it
//! takes (learned.rs): Graylog's documented 2 MiB at first, then what it refuses by ending
//! the connection a message came on (stream.rs `suspect`), that message sent again cut
//! (D163). GELF's messages carry no part's place, so a message cut so is its pieces.

use std::collections::VecDeque;
use std::io;
use std::net::UdpSocket;
use std::path::Path;
use std::time::Duration;

use super::stream::{self, Stream};
use super::{Driver, Message, Wait};
use crate::logging::gelf::{self, Config, From, To};

pub(super) struct Gelf {
    config: Config,
    from: From,
    udp: Option<UdpSocket>,
    /// Over TCP: its stream, what its destination has shown it takes, and the line of each
    /// message the stream holds, and its message's length, to cut it again.
    tcp: Option<Stream>,
    learned: Option<super::learned::Learned>,
    held: VecDeque<(u8, u64, Vec<u8>, usize)>,
}

impl Gelf {
    /// The driver `config` says; over TCP, what `home` keeps of its destination.
    pub(super) fn new(config: Config, from: From, home: &Path) -> Gelf {
        let (tcp, learned) = match &config.to {
            To::Tcp(address) => (
                Some(Stream::new(stream::To::Tcp(address.clone()), [None; 2])),
                Some(super::learned::Learned::open(
                    home,
                    "gelf",
                    address,
                    gelf::TCP_MOST,
                )),
            ),
            To::Udp(_) => (None, None),
        };
        Gelf {
            config,
            from,
            udp: None,
            tcp,
            learned,
            held: VecDeque::new(),
        }
    }

    /// Follows what the destination shows over TCP: the messages its stream no longer holds
    /// it has, their lengths taken; one it points at as refused (stream.rs `suspect`) is
    /// learned so, and its line put back on the stream cut in pieces each within what is
    /// learned (ship.rs `halves`).
    fn follow(&mut self) {
        let (Some(stream), Some(learned)) = (&mut self.tcp, &mut self.learned) else {
            return;
        };
        while self.held.len() > stream.held() {
            if let Some((_, _, _, len)) = self.held.pop_front() {
                learned.taken(len);
            }
        }
        let Some(i) = stream.suspect(learned.took()) else {
            return;
        };
        let Some((st, at, line, len)) = self.held.get(i).cloned() else {
            return;
        };
        learned.refused(len);
        let most = learned.most();
        let from = &self.from;
        let pieces = super::halves(&line, &|l| gelf::message(from, st, at, l).len() <= most);
        let messages: Vec<(u8, u64, Vec<u8>, usize)> = pieces
            .into_iter()
            .map(|l| {
                let len = gelf::message(from, st, at, &l).len();
                (st, at, l, len)
            })
            .collect();
        stream.replace(
            i,
            messages
                .iter()
                .map(|(st, at, l, _)| {
                    let mut framed = gelf::message(from, *st, *at, l);
                    framed.push(0);
                    framed
                })
                .collect(),
        );
        let rest = self.held.split_off(i);
        self.held.extend(messages);
        self.held.extend(rest.into_iter().skip(1));
    }

    fn send_udp(&mut self, address: &str, message: &[u8]) -> io::Result<()> {
        let data = gelf::compress(self.config.compression, self.config.level, message)?;
        let mut id = [0u8; 8];
        shards_vmm::platform::fill_random(&mut id)?;
        let chunks = gelf::chunks(&data, id).ok_or_else(|| {
            io::Error::other(format!(
                "a message of {} bytes compressed, past the {} chunks GELF takes",
                data.len(),
                gelf::CHUNKS
            ))
        })?;
        if self.udp.is_none() {
            self.udp = Some(super::sockets::udp(address)?);
        }
        let Some(socket) = &self.udp else {
            return Err(io::Error::other("no socket"));
        };
        for chunk in &chunks {
            if let Err(e) = super::sockets::send_datagram(socket, chunk) {
                self.udp = None;
                return Err(e);
            }
        }
        Ok(())
    }

    fn send_tcp(
        &mut self,
        address: &str,
        m: &Message<'_>,
        message: &[u8],
        wait: &Wait<'_>,
    ) -> io::Result<()> {
        let mut framed = message.to_vec();
        framed.push(0);
        let mut tries = 0;
        loop {
            self.follow();
            let Some(stream) = &mut self.tcp else {
                return Err(io::Error::from(io::ErrorKind::NotConnected));
            };
            let Err(e) = stream.send(&framed) else {
                self.held
                    .push_back((m.stream, m.at, m.line.to_vec(), message.len()));
                return Ok(());
            };
            tries += 1;
            if tries > self.config.reconnects {
                return Err(io::Error::other(format!(
                    "{address}: no connection took the message in {tries} tries: {e}"
                )));
            }
            if wait.pause(Duration::from_secs(u64::from(self.config.delay))) {
                return Err(super::stops());
            }
        }
    }
}

impl Driver for Gelf {
    /// What its input takes of a message (logging/gelf.rs `Config::part`).
    fn part(&self) -> usize {
        self.config.part(&self.from)
    }

    /// Over TCP within what its destination has shown it takes (learned.rs); over UDP
    /// within GELF's 128 chunks.
    fn written(&mut self, m: &Message<'_>) -> io::Result<Option<(usize, usize)>> {
        let (written, bound) = self
            .config
            .written(&gelf::message(&self.from, m.stream, m.at, m.line))?;
        Ok(Some((written, self.learned.as_ref().map_or(bound, |l| l.most()))))
    }

    fn send(&mut self, m: &Message<'_>, wait: &Wait<'_>) -> io::Result<()> {
        // moby's gelfLogger sends no empty line.
        if m.line.is_empty() {
            return Ok(());
        }
        let message = gelf::message(&self.from, m.stream, m.at, m.line);
        match self.config.to.clone() {
            To::Udp(address) => self.send_udp(&address, &message),
            To::Tcp(address) => self.send_tcp(&address, m, &message, wait),
        }
    }

    fn undelivered(&mut self) -> usize {
        let n = self.tcp.as_mut().map_or(0, Stream::undelivered);
        self.follow();
        self.tcp.as_ref().map_or(n, Stream::held)
    }

    fn close(&mut self, wait: &Wait<'_>) -> io::Result<()> {
        self.follow();
        let closed = self.tcp.as_mut().map_or(Ok(()), |s| s.close(wait));
        self.follow();
        closed
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(super) mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Do: send over TCP, to a receiver that ends its connection on any GELF message of
    /// more than 2,000 bytes as Graylog's input ends one past its `max_message_size`, a
    /// short line, one of 6,000 bytes, then short lines, as a shipper sends them; then open
    /// a driver of the same destination from the same home. Expect the long line received
    /// whole, in messages each within 2,000 bytes; every message received within them; the
    /// destination's bound learned below the long line's message from its ends alone, kept
    /// in the home.
    #[test]
    fn a_message_its_receiver_ends_connections_on_is_sent_again_cut() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (got, frames) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let mut read = std::io::BufReader::new(conn);
                loop {
                    let mut frame = Vec::new();
                    match std::io::BufRead::read_until(&mut read, 0, &mut frame) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    frame.pop();
                    if frame.len() > 2000 {
                        break;
                    }
                    if got.send(frame).is_err() {
                        return;
                    }
                }
            }
        });
        let home = shards_testdir::TempDir::new("gelf-tcp").unwrap();
        let config = Config {
            to: To::Tcp(address.clone()),
            compression: gelf::Compression::None,
            level: 1,
            reconnects: 3,
            delay: 0,
        };
        let from = From {
            host: "h".into(),
            extra: String::new(),
        };
        let mut g = Gelf::new(config.clone(), from.clone(), &home);
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
        let long: Vec<u8> = (0..6000u32).map(|i| b'a' + (i % 26) as u8).collect();
        g.send(&m(b"first"), &wait).unwrap();
        g.send(&m(&long), &wait).unwrap();
        let started = std::time::Instant::now();
        let mut lines: Vec<String> = Vec::new();
        let mut i = 0;
        while !lines.concat().contains(std::str::from_utf8(&long).unwrap()) {
            assert!(started.elapsed() < Duration::from_secs(30), "{lines:?}");
            std::thread::sleep(Duration::from_millis(20));
            g.undelivered();
            g.send(&m(format!("tick{i}").as_bytes()), &wait).unwrap();
            i += 1;
            for f in frames.try_iter() {
                assert!(f.len() <= 2000, "{}", f.len());
                let v: serde_json::Value = serde_json::from_slice(&f).unwrap();
                let line = v["short_message"].as_str().unwrap().to_string();
                if line.starts_with("tick") || line == "first" {
                    continue;
                }
                lines.push(line);
            }
        }
        g.close(&wait).unwrap();
        let most = g.learned.as_ref().unwrap().most();
        let refused = gelf::message(&from, crate::spec::LOG_STDOUT, 1_000_000_000, &long).len();
        assert!(most < refused, "{most} {refused}");
        let again = Gelf::new(config, from, &home);
        assert_eq!(again.learned.unwrap().most(), most);
    }

    /// Closes a driver by `close` as its shipper does, again where its receiver ended the connection with
    /// messages held (`ConnectionAborted`), until it ends.
    pub(in crate::daemon::ship) fn close_until_ended(close: &mut dyn FnMut() -> io::Result<()>) {
        for _ in 0..64 {
            match close() {
                Ok(()) => return,
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {}
                Err(e) => panic!("{e}"),
            }
        }
        panic!("the connection never ended with all delivered");
    }

    /// Do: over TCP, to a receiver that ends its connection on any GELF message of more
    /// than 2,000 bytes, send a short line and one of 6,000 bytes, the run's last, and close
    /// the driver as its shipper does, the close tried again while it says its receiver
    /// ended the connection with messages held. Expect no line lost: the long one received
    /// whole, in messages each within 2,000 bytes, before the close ends; the destination's
    /// bound learned below the long line's message.
    #[test]
    fn a_runs_last_message_refused_at_close_is_sent_again_cut() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let (got, frames) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { return };
                let mut read = std::io::BufReader::new(conn);
                loop {
                    let mut frame = Vec::new();
                    match std::io::BufRead::read_until(&mut read, 0, &mut frame) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    frame.pop();
                    if frame.len() > 2000 {
                        break;
                    }
                    if got.send(frame).is_err() {
                        return;
                    }
                }
            }
        });
        let home = shards_testdir::TempDir::new("gelf-close").unwrap();
        let config = Config {
            to: To::Tcp(address),
            compression: gelf::Compression::None,
            level: 1,
            reconnects: 3,
            delay: 0,
        };
        let from = From {
            host: "h".into(),
            extra: String::new(),
        };
        let mut g = Gelf::new(config, from.clone(), &home);
        let (_stop, stopped) = shards_ipc::Stream::pair().unwrap();
        let wait = Wait(&stopped);
        let m = |line: &'static [u8]| Message {
            stream: crate::spec::LOG_STDOUT,
            at: 1_000_000_000,
            line,
            part: None,
        };
        let long: &'static [u8] = Vec::leak((0..6000u32).map(|i| b'a' + (i % 26) as u8).collect());
        g.send(&m(b"first"), &wait).unwrap();
        g.send(&m(long), &wait).unwrap();
        close_until_ended(&mut || g.close(&wait));
        let mut lines = String::new();
        for f in frames.try_iter() {
            assert!(f.len() <= 2000, "{}", f.len());
            let v: serde_json::Value = serde_json::from_slice(&f).unwrap();
            let line = v["short_message"].as_str().unwrap();
            if line != "first" {
                lines.push_str(line);
            }
        }
        assert!(lines.contains(std::str::from_utf8(long).unwrap()), "{lines}");
        let refused = gelf::message(&from, crate::spec::LOG_STDOUT, 1_000_000_000, long).len();
        assert!(g.learned.as_ref().unwrap().most() < refused);
    }
}

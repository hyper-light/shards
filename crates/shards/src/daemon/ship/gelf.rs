//! The gelf driver's sending (D164): its messages (logging/gelf.rs) over UDP, compressed
//! and in chunks, a datagram each, or over TCP, each followed by a NUL, as go-gelf sends
//! them (udpwriter.go, tcpwriter.go). Over TCP a write that fails is tried again on a new
//! connection, `gelf-tcp-max-reconnect` times, `gelf-tcp-reconnect-delay` seconds apart,
//! as go-gelf tries it; then the shipper's own waits take over, where go-gelf drops the
//! message. And a message is held until its receiver has it, and written again on the
//! next connection should one end first (stream.rs), where go-gelf's are lost.

use std::io;
use std::net::UdpSocket;
use std::time::Duration;

use super::stream::{self, Stream};
use super::{Driver, Message, Wait};
use crate::logging::gelf::{self, Config, From, To};

pub(super) struct Gelf {
    config: Config,
    from: From,
    udp: Option<UdpSocket>,
    /// Over TCP: its stream.
    tcp: Option<Stream>,
}

impl Gelf {
    pub(super) fn new(config: Config, from: From) -> Gelf {
        let tcp = match &config.to {
            To::Tcp(address) => Some(Stream::new(stream::To::Tcp(address.clone()), [None; 2])),
            To::Udp(_) => None,
        };
        Gelf {
            config,
            from,
            udp: None,
            tcp,
        }
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

    fn send_tcp(&mut self, address: &str, message: &[u8], wait: &Wait<'_>) -> io::Result<()> {
        let Some(stream) = &mut self.tcp else {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        };
        let mut framed = message.to_vec();
        framed.push(0);
        let mut tries = 0;
        loop {
            let Err(e) = stream.send(&framed) else {
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
    fn send(&mut self, m: &Message<'_>, wait: &Wait<'_>) -> io::Result<()> {
        // moby's gelfLogger sends no empty line.
        if m.line.is_empty() {
            return Ok(());
        }
        let message = gelf::message(&self.from, m.stream, m.at, m.line);
        match self.config.to.clone() {
            To::Udp(address) => self.send_udp(&address, &message),
            To::Tcp(address) => self.send_tcp(&address, &message, wait),
        }
    }

    fn undelivered(&mut self) -> usize {
        self.tcp.as_mut().map_or(0, Stream::undelivered)
    }

    fn close(&mut self, wait: &Wait<'_>) -> io::Result<()> {
        self.tcp.as_mut().map_or(Ok(()), |s| s.close(wait))
    }
}

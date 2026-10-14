//! The gelf driver's sending (D164): its messages (logging/gelf.rs) over UDP, compressed
//! and in chunks, a datagram each, or over TCP, each followed by a NUL, as go-gelf sends
//! them (udpwriter.go, tcpwriter.go). Over TCP a write that fails is tried again on a new
//! connection, `gelf-tcp-max-reconnect` times, `gelf-tcp-reconnect-delay` seconds apart,
//! as go-gelf tries it; then the shipper's own waits take over, where go-gelf drops the
//! message.

use std::io::{self, Write as _};
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

use super::{Driver, Message};
use crate::logging::gelf::{self, Config, From, To};

pub(super) struct Gelf {
    config: Config,
    from: From,
    udp: Option<UdpSocket>,
    tcp: Option<TcpStream>,
}

impl Gelf {
    pub(super) fn new(config: Config, from: From) -> Gelf {
        Gelf {
            config,
            from,
            udp: None,
            tcp: None,
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

    fn send_tcp(&mut self, address: &str, message: &[u8], wait: &dyn Fn(Duration) -> bool) -> io::Result<()> {
        let mut framed = message.to_vec();
        framed.push(0);
        let mut tries = 0;
        loop {
            if let Some(conn) = &mut self.tcp
                && conn.write_all(&framed).is_ok()
            {
                return Ok(());
            }
            self.tcp = None;
            if tries > self.config.reconnects {
                return Err(io::Error::other(format!(
                    "{address}: no connection took the message in {} tries",
                    tries
                )));
            }
            if tries > 0 && wait(Duration::from_secs(u64::from(self.config.delay))) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "the daemon stops"));
            }
            tries += 1;
            if let Ok(conn) = TcpStream::connect(address) {
                self.tcp = Some(conn);
            }
        }
    }
}

impl Driver for Gelf {
    fn send(&mut self, m: &Message<'_>, wait: &dyn Fn(Duration) -> bool) -> io::Result<()> {
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
}

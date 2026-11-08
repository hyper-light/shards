//! git's own transport (gitprotocol-pack.adoc, "Git Transport"): a TCP connection to the
//! daemon, port 9418 unless the URL says another, opened with the service and path asked
//! for, the host, and `version=2` as an extra parameter (gitprotocol-v2.adoc, "Git
//! Transport"). Each request takes a connection of its own, as smart HTTP's do: the
//! advertisement read, the command sent, its answer read to the end.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::pktline::{self, Packet};
use crate::remote::Transport;

/// git's port.
pub const PORT: u16 = 9418;

/// A daemon at `host:port` serving the repository at `path`.
#[derive(Debug)]
pub struct Daemon {
    pub host: String,
    pub port: u16,
    pub path: String,
    /// How long a connection may take to open, and a read to come.
    pub patience: Duration,
}

impl Daemon {
    /// The daemon a `git://HOST[:PORT]/PATH` URL names.
    pub fn of_url(url: &str, patience: Duration) -> Result<Daemon, String> {
        let rest = url.strip_prefix("git://").ok_or("not a git:// URL")?;
        let (authority, path) = rest.split_at(rest.find('/').ok_or("a git:// URL without a path")?);
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (
                h.trim_matches(['[', ']']).to_string(),
                p.parse().map_err(|_| format!("a bad port in {url}"))?,
            ),
            _ => (authority.trim_matches(['[', ']']).to_string(), PORT),
        };
        if host.is_empty() {
            return Err(format!("{url}: no host"));
        }
        Ok(Daemon {
            host,
            port,
            path: path.to_string(),
            patience,
        })
    }

    /// A connection that has asked for upload-pack, version 2.
    fn open(&self) -> Result<TcpStream, String> {
        let at = format!("{}:{}", self.host, self.port);
        let addrs = std::net::ToSocketAddrs::to_socket_addrs(&at).map_err(|e| format!("{at}: {e}"))?;
        let mut last = format!("{at}: no address");
        for a in addrs {
            match TcpStream::connect_timeout(&a, self.patience) {
                Ok(mut s) => {
                    s.set_read_timeout(Some(self.patience))
                        .map_err(|e| e.to_string())?;
                    let host = if self.port == PORT {
                        self.host.clone()
                    } else {
                        at.clone()
                    };
                    let line = format!("git-upload-pack {}\0host={host}\0\0version=2\0", self.path);
                    s.write_all(&pktline::data(line.as_bytes()).map_err(|e| e.to_string())?)
                        .map_err(|e| format!("{at}: {e}"))?;
                    return Ok(s);
                }
                Err(e) => last = format!("{at}: {e}"),
            }
        }
        Err(last)
    }
}

impl Transport for Daemon {
    fn advertise(&self) -> Result<Box<dyn Read + '_>, String> {
        Ok(Box::new(self.open()?))
    }

    fn command(&self, body: &[u8]) -> Result<Box<dyn Read + '_>, String> {
        let mut s = self.open()?;
        // The advertisement comes first, to its flush.
        loop {
            match pktline::read(&mut s).map_err(|e| format!("the daemon's advertisement: {e}"))? {
                Some(Packet::Flush) => break,
                Some(Packet::Data(line)) if line.starts_with(b"ERR ") => {
                    return Err(format!(
                        "remote error: {}",
                        String::from_utf8_lossy(pktline::text(line.get(4..).unwrap_or_default()))
                    ));
                }
                Some(_) => {}
                None => return Err("the daemon hung up".into()),
            }
        }
        s.write_all(body).map_err(|e| e.to_string())?;
        Ok(Box::new(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemons_are_found_as_their_urls_name_them() {
        let d = Daemon::of_url("git://h/r.git", Duration::from_secs(1)).unwrap();
        assert_eq!((d.host.as_str(), d.port, d.path.as_str()), ("h", 9418, "/r.git"));
        let d = Daemon::of_url("git://h:9000/a/b", Duration::from_secs(1)).unwrap();
        assert_eq!((d.port, d.path.as_str()), (9000, "/a/b"));
        let d = Daemon::of_url("git://[::1]:9000/x", Duration::from_secs(1)).unwrap();
        assert_eq!((d.host.as_str(), d.port), ("::1", 9000));
        assert!(Daemon::of_url("git://h", Duration::from_secs(1)).is_err());
        assert!(Daemon::of_url("https://h/x", Duration::from_secs(1)).is_err());
    }
}

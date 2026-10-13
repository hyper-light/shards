//! Packet lines (gitprotocol-common.adoc, "pkt-line Format"): four hexadecimal digits of
//! length, the length counting themselves, then the payload; `0000` is a flush, `0001` a
//! delimiter and `0002` the end of a response (protocol v2).

use std::io::{self, Read};

/// The longest packet: 65520 bytes, its length included (LARGE_PACKET_MAX).
pub const MAX: usize = 65520;

/// A packet as read.
#[derive(Debug, PartialEq, Eq)]
pub enum Packet {
    Data(Vec<u8>),
    Flush,
    Delimiter,
    ResponseEnd,
}

/// `payload` as one packet.
pub fn data(payload: &[u8]) -> io::Result<Vec<u8>> {
    let len = payload
        .len()
        .checked_add(4)
        .filter(|n| *n <= MAX)
        .ok_or_else(|| io::Error::other("a packet line too long"))?;
    let mut out = format!("{len:04x}").into_bytes();
    out.extend_from_slice(payload);
    Ok(out)
}

pub const FLUSH: &[u8] = b"0000";
pub const DELIMITER: &[u8] = b"0001";

/// The next packet of `r`; `None` where it ends between packets.
pub fn read(r: &mut impl Read) -> io::Result<Option<Packet>> {
    let mut head = [0u8; 4];
    let mut got = 0;
    while got < head.len() {
        // A read a signal interrupted is read again, as `read_exact` does: a socket with a
        // receive timeout is never restarted after a handler runs (signal(7)).
        match r.read(head.get_mut(got..).unwrap_or_default()) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let len = std::str::from_utf8(&head)
        .ok()
        .and_then(|h| usize::from_str_radix(h, 16).ok())
        .ok_or_else(|| {
            io::Error::other(format!(
                "a bad packet line length {:?}",
                String::from_utf8_lossy(&head)
            ))
        })?;
    match len {
        0 => Ok(Some(Packet::Flush)),
        1 => Ok(Some(Packet::Delimiter)),
        2 => Ok(Some(Packet::ResponseEnd)),
        3 => Err(io::Error::other("a bad packet line length 0003")),
        n if n > MAX => Err(io::Error::other(format!(
            "a packet line of {n} bytes, past {MAX}"
        ))),
        n => {
            let mut payload = vec![0u8; n - 4];
            r.read_exact(&mut payload)?;
            Ok(Some(Packet::Data(payload)))
        }
    }
}

/// `payload` without the one newline that ends a text packet.
pub fn text(payload: &[u8]) -> &[u8] {
    payload.strip_suffix(b"\n").unwrap_or(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_are_framed_as_git_frames_them() {
        // gitprotocol-common.adoc's own examples.
        assert_eq!(data(b"a\n").unwrap(), b"0006a\n");
        assert_eq!(data(b"a").unwrap(), b"0005a");
        assert_eq!(data(b"foobar\n").unwrap(), b"000bfoobar\n");
        assert_eq!(data(b"").unwrap(), b"0004");
        let mut wire: &[u8] = b"0006a\n000100000002";
        assert_eq!(read(&mut wire).unwrap(), Some(Packet::Data(b"a\n".to_vec())));
        assert_eq!(read(&mut wire).unwrap(), Some(Packet::Delimiter));
        assert_eq!(read(&mut wire).unwrap(), Some(Packet::Flush));
        assert_eq!(read(&mut wire).unwrap(), Some(Packet::ResponseEnd));
        assert_eq!(read(&mut wire).unwrap(), None);
        assert!(read(&mut &b"0003"[..]).is_err());
        assert!(read(&mut &b"zzzz"[..]).is_err());
        assert!(read(&mut &b"0009ab"[..]).is_err());
        assert!(data(&[0; MAX]).is_err());
        assert_eq!(text(b"ok\n"), b"ok");
    }

    /// A read a signal interrupted is read again, a packet's length as its payload is: a
    /// socket with a receive timeout is interrupted by any handler that runs.
    #[test]
    fn an_interrupted_read_is_read_again() {
        struct Interrupted<'a> {
            wire: &'a [u8],
            each: bool,
        }
        impl Read for Interrupted<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.each = !self.each;
                if self.each {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                // A byte at a time, so that a length is read in parts.
                let n = buf.len().min(1);
                self.wire.read(buf.get_mut(..n).unwrap_or_default())
            }
        }
        let mut wire = Interrupted {
            wire: b"0006a\n0000",
            each: false,
        };
        assert_eq!(read(&mut wire).unwrap(), Some(Packet::Data(b"a\n".to_vec())));
        assert_eq!(read(&mut wire).unwrap(), Some(Packet::Flush));
        assert_eq!(read(&mut wire).unwrap(), None);
    }
}

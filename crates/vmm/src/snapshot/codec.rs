//! Little-endian binary encoding for snapshot state. Snapshot files come from disk, so
//! decoding treats them as untrusted: every read is bounds-checked, every length capped,
//! and a malformed file yields an error, never a panic or an unbounded allocation.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed snapshot state: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

pub type Result<T> = std::result::Result<T, DecodeError>;

#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u128(&mut self, v: u128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    /// A length-prefixed byte string.
    pub fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }

    /// A length-prefixed sequence.
    pub fn seq<T>(&mut self, items: &[T], mut each: impl FnMut(&mut Writer, &T)) {
        self.u32(items.len() as u32);
        for item in items {
            each(self, item);
        }
    }
}

#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf }
    }

    fn take<const N: usize>(&mut self, what: &str) -> Result<[u8; N]> {
        let (head, rest) = self
            .buf
            .split_first_chunk::<N>()
            .ok_or_else(|| DecodeError(format!("truncated {what}")))?;
        self.buf = rest;
        Ok(*head)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(u8::from_le_bytes(self.take("u8")?))
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take("u16")?))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take("u32")?))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take("u64")?))
    }

    pub fn u128(&mut self) -> Result<u128> {
        Ok(u128::from_le_bytes(self.take("u128")?))
    }

    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(DecodeError(format!("bool {v}"))),
        }
    }

    /// A length-prefixed byte string of at most `max` bytes.
    pub fn bytes(&mut self, max: usize) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        if len > max || len > self.buf.len() {
            return Err(DecodeError(format!("byte string of {len} (limit {max})")));
        }
        let (head, rest) = self.buf.split_at(len);
        self.buf = rest;
        Ok(head)
    }

    /// A length-prefixed sequence of at most `max` items.
    pub fn seq<T>(
        &mut self,
        max: usize,
        mut each: impl FnMut(&mut Reader<'a>) -> Result<T>,
    ) -> Result<Vec<T>> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(DecodeError(format!("sequence of {len} (limit {max})")));
        }
        // Capacity comes from the checked length, and each item consumes input, so a
        // hostile count cannot allocate more than `max` items.
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            out.push(each(self)?);
        }
        Ok(out)
    }

    /// Fails unless every byte was consumed.
    pub fn finish(self) -> Result<()> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(DecodeError(format!("{} trailing bytes", self.buf.len())))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_type() {
        let mut w = Writer::default();
        w.u8(1);
        w.u16(0x0203);
        w.u32(0x0405_0607);
        w.u64(u64::MAX);
        w.u128(u128::MAX - 1);
        w.bool(true);
        w.bytes(b"abc");
        w.seq(&[7u32, 8, 9], |w, &v| w.u32(v));
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 1);
        assert_eq!(r.u16().unwrap(), 0x0203);
        assert_eq!(r.u32().unwrap(), 0x0405_0607);
        assert_eq!(r.u64().unwrap(), u64::MAX);
        assert_eq!(r.u128().unwrap(), u128::MAX - 1);
        assert!(r.bool().unwrap());
        assert_eq!(r.bytes(16).unwrap(), b"abc");
        assert_eq!(r.seq(8, |r| r.u32()).unwrap(), vec![7, 8, 9]);
        r.finish().unwrap();
    }

    #[test]
    fn rejects_malformed_input() {
        // Truncation at every length of a valid encoding fails cleanly.
        let mut w = Writer::default();
        w.u64(1);
        w.bytes(b"payload");
        let bytes = w.into_bytes();
        for cut in 0..bytes.len() {
            let mut r = Reader::new(&bytes[..cut]);
            let ok = r.u64().and_then(|_| r.bytes(64).map(|_| ()));
            assert!(ok.is_err(), "cut at {cut}");
        }
        // Lengths past the limit or the input, invalid bools and trailing bytes.
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0x7f]).bytes(usize::MAX).is_err());
        assert!(Reader::new(&[9, 0, 0, 0]).bytes(4).is_err());
        assert!(
            Reader::new(&[0xff, 0xff, 0xff, 0xff])
                .seq(16, |r| r.u8())
                .is_err()
        );
        assert!(Reader::new(&[2]).bool().is_err());
        assert!(Reader::new(&[0]).finish().is_err());
    }
}

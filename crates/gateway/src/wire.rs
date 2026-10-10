//! Protobuf's wire format (protobuf.dev, "Encoding"), as the gateway's messages use it:
//! a reader of a message's fields, each bounded by the message, and a writer that writes
//! fields in their numbers' order, proto3's zero values left out, as protobuf-go writes
//! them (its deterministic marshal orders map entries by key too).

/// What a message could not be read as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protobuf: {}", self.0)
    }
}

/// A field's value, as its wire type holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

impl<'a> Value<'a> {
    pub fn varint(self) -> Result<u64, Error> {
        match self {
            Value::Varint(v) => Ok(v),
            _ => Err(Error("a varint field of another wire type".into())),
        }
    }

    pub fn bytes(self) -> Result<&'a [u8], Error> {
        match self {
            Value::Bytes(b) => Ok(b),
            _ => Err(Error("a length-delimited field of another wire type".into())),
        }
    }

    /// A string field: UTF-8, as protobuf-go requires of proto3's strings.
    pub fn string(self) -> Result<String, Error> {
        String::from_utf8(self.bytes()?.to_vec())
            .map_err(|_| Error("a string field that is not UTF-8".into()))
    }
}

/// A varint at the start of `buf`, and the bytes it took: at most ten, the tenth adding
/// no more than the 64th bit.
pub fn varint(buf: &[u8]) -> Result<(u64, usize), Error> {
    let mut v = 0u64;
    for (i, &b) in buf.iter().enumerate().take(10) {
        if i == 9 && b > 1 {
            return Err(Error("a varint past 64 bits".into()));
        }
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    Err(Error("a varint past its message".into()))
}

/// A message's fields in the order written.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let Some(head) = self.buf.get(..n) else {
            return Err(Error("a field past its message".into()));
        };
        self.buf = self.buf.get(n..).unwrap_or_default();
        Ok(head)
    }
}

impl<'a> Iterator for Reader<'a> {
    type Item = Result<(u32, Value<'a>), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        let field = (|| {
            let (tag, n) = varint(self.buf)?;
            self.take(n)?;
            let number = u32::try_from(tag >> 3)
                .ok()
                .filter(|&n| n != 0 && n < 1 << 29)
                .ok_or_else(|| Error("a field number out of range".into()))?;
            let value = match tag & 7 {
                0 => {
                    let (v, n) = varint(self.buf)?;
                    self.take(n)?;
                    Value::Varint(v)
                }
                1 => {
                    let b = self.take(8)?;
                    Value::Fixed64(u64::from_le_bytes(
                        b.try_into().map_err(|_| Error("fixed64".into()))?,
                    ))
                }
                2 => {
                    let (len, n) = varint(self.buf)?;
                    self.take(n)?;
                    let len = usize::try_from(len).map_err(|_| Error("a length out of range".into()))?;
                    Value::Bytes(self.take(len)?)
                }
                5 => {
                    let b = self.take(4)?;
                    Value::Fixed32(u32::from_le_bytes(
                        b.try_into().map_err(|_| Error("fixed32".into()))?,
                    ))
                }
                _ => return Err(Error("a group or unknown wire type".into())),
            };
            Ok((number, value))
        })();
        if field.is_err() {
            // Nothing past an error is read.
            self.buf = &[];
        }
        Some(field)
    }
}

/// A message being written.
#[derive(Debug, Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn tag(&mut self, field: u32, wire: u8) {
        self.varint(u64::from(field) << 3 | u64::from(wire));
    }

    /// A uint32, uint64, enum or bool field, left out at zero.
    pub fn uint(&mut self, field: u32, v: u64) {
        if v != 0 {
            self.tag(field, 0);
            self.varint(v);
        }
    }

    /// An int32 field: sign-extended to 64 bits, as protobuf writes it.
    pub fn int32(&mut self, field: u32, v: i32) {
        self.uint(field, i64::from(v) as u64);
    }

    /// An int64 field: two's complement, ten bytes when negative.
    pub fn int64(&mut self, field: u32, v: i64) {
        self.uint(field, v as u64);
    }

    pub fn bool(&mut self, field: u32, v: bool) {
        self.uint(field, u64::from(v));
    }

    /// A bytes or string field, left out when empty.
    pub fn bytes(&mut self, field: u32, v: &[u8]) {
        if !v.is_empty() {
            self.message(field, v);
        }
    }

    pub fn string(&mut self, field: u32, v: &str) {
        self.bytes(field, v.as_bytes());
    }

    /// A message field, written however empty (a set message), or a repeated bytes
    /// field's element, which is written however empty too.
    pub fn message(&mut self, field: u32, v: &[u8]) {
        self.tag(field, 2);
        self.varint(v.len() as u64);
        self.0.extend_from_slice(v);
    }

    /// A map<string, V> field's entries, by their keys' order: each its key (field 1)
    /// and its value (field 2), the value as `value` writes it into the entry.
    pub fn map<'k, V>(
        &mut self,
        field: u32,
        entries: impl IntoIterator<Item = (&'k str, V)>,
        value: impl Fn(&mut Writer, V),
    ) {
        let mut sorted: Vec<(&str, V)> = entries.into_iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in sorted {
            let mut e = Writer::default();
            e.string(1, k);
            value(&mut e, v);
            self.message(field, &e.0);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn fields_read_back_as_written() {
        let mut w = Writer::default();
        w.uint(1, 150);
        w.int64(2, -1);
        w.string(3, "testing");
        w.message(4, &[]);
        w.bool(5, false);
        w.map(6, [("b", 2u64), ("a", 1u64)], |e, v| e.uint(2, v));
        // protobuf.dev's own examples: 150 as 08 96 01, "testing" as 1a 07 74 65 …
        assert_eq!(&w.0[..3], &[0x08, 0x96, 0x01]);
        let read: Vec<(u32, Value<'_>)> = Reader::new(&w.0).map(Result::unwrap).collect();
        assert_eq!(read[0], (1, Value::Varint(150)));
        assert_eq!(read[1], (2, Value::Varint(u64::MAX)));
        assert_eq!(read[2], (3, Value::Bytes(b"testing")));
        assert_eq!(read[3], (4, Value::Bytes(b"")));
        // The map's entries, "a" first.
        assert_eq!(read[4], (6, Value::Bytes(&[0x0a, 0x01, b'a', 0x10, 0x01])));
        assert_eq!(read.len(), 6);
    }

    #[test]
    fn what_no_message_holds_is_refused() {
        for (bytes, why) in [
            (&[0x08][..], "a varint past its message"),
            (
                &[0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02],
                "a varint past 64 bits",
            ),
            (&[0x0a, 0x05, 0x01], "a field past its message"),
            (&[0x0b], "a group or unknown wire type"),
            (&[0x00, 0x00], "a field number out of range"),
            (&[0x09, 0x01, 0x02], "a field past its message"),
        ] {
            let mut r = Reader::new(bytes);
            assert_eq!(r.next(), Some(Err(Error(why.into()))), "{bytes:?}");
            assert_eq!(r.next(), None);
        }
    }
}

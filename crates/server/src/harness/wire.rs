use crate::server::json::Value;
use std::io::{self, Write};

pub(super) struct Count(pub usize);
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or(io::ErrorKind::InvalidData)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct Fixed<'a>(pub &'a mut Vec<u8>);
impl Write for Fixed<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let length = self
            .0
            .len()
            .checked_add(bytes.len())
            .ok_or(io::ErrorKind::InvalidData)?;
        if length > self.0.capacity() {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// RFC 8259 §7 escapes, streamed without a second retained copy of private text.
pub(super) fn quoted(out: &mut impl Write, value: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    let mut start = 0;
    for (at, byte) in value.bytes().enumerate() {
        let escaped: &[u8] = match byte {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            0..=0x1f => b"",
            _ => continue,
        };
        out.write_all(
            value
                .as_bytes()
                .get(start..at)
                .ok_or(io::ErrorKind::InvalidData)?,
        )?;
        if byte < 0x20 {
            const HEX: &[u8] = b"0123456789abcdef";
            let high = HEX
                .get(usize::from(byte >> 4))
                .ok_or(io::ErrorKind::InvalidData)?;
            let low = HEX
                .get(usize::from(byte & 0xf))
                .ok_or(io::ErrorKind::InvalidData)?;
            out.write_all(&[b'\\', b'u', b'0', b'0', *high, *low])?;
        } else {
            out.write_all(escaped)?;
        }
        start = at.checked_add(1).ok_or(io::ErrorKind::InvalidData)?;
    }
    out.write_all(value.as_bytes().get(start..).ok_or(io::ErrorKind::InvalidData)?)?;
    out.write_all(b"\"")
}

pub(super) fn id(out: &mut impl Write, value: &Value) -> io::Result<()> {
    match value {
        Value::String(text) => quoted(out, text),
        Value::Number(number) if number.parse::<i64>().is_ok() => out.write_all(number.as_bytes()),
        _ => Err(io::ErrorKind::InvalidData.into()),
    }
}

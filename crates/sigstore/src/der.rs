//! DER as Go's cryptobyte reads it (x/crypto/cryptobyte asn1.go): single-octet tags,
//! definite lengths in their shortest form, integers minimally encoded.

/// Tags as cryptobyte names them.
pub const BOOLEAN: u8 = 0x01;
pub const INTEGER: u8 = 0x02;
pub const BIT_STRING: u8 = 0x03;
pub const OCTET_STRING: u8 = 0x04;
pub const NULL: u8 = 0x05;
pub const OID: u8 = 0x06;
pub const ENUMERATED: u8 = 0x0a;
pub const UTF8_STRING: u8 = 0x0c;
pub const SEQUENCE: u8 = 0x30;
pub const SET: u8 = 0x31;
pub const PRINTABLE_STRING: u8 = 0x13;
pub const T61_STRING: u8 = 0x14;
pub const IA5_STRING: u8 = 0x16;
pub const UTC_TIME: u8 = 0x17;
pub const GENERALIZED_TIME: u8 = 0x18;
pub const BMP_STRING: u8 = 0x1e;

/// A context-specific constructed tag `[n]`.
pub const fn explicit(n: u8) -> u8 {
    0xa0 | n
}

/// A context-specific primitive tag `[n]`.
pub const fn implicit(n: u8) -> u8 {
    0x80 | n
}

/// A cursor over DER, as cryptobyte.String.
#[derive(Debug, Clone, Copy)]
pub struct Der<'a>(pub &'a [u8]);

impl<'a> Der<'a> {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The next element, whole (header and contents), and its tag.
    pub fn any_element(&mut self) -> Option<(u8, &'a [u8], &'a [u8])> {
        let b = self.0;
        let (&tag, rest) = b.split_first()?;
        let (&len_byte, _) = rest.split_first()?;
        if tag & 0x1f == 0x1f {
            return None;
        }
        let (header, len) = if len_byte & 0x80 == 0 {
            (2usize, usize::from(len_byte))
        } else {
            let n = usize::from(len_byte & 0x7f);
            if n == 0 || n > 4 {
                return None;
            }
            let bytes = b.get(2..2 + n)?;
            let len = bytes.iter().fold(0u32, |a, &x| (a << 8) | u32::from(x));
            if len < 128 || (len >> ((n - 1) * 8)) == 0 {
                return None;
            }
            (2 + n, usize::try_from(len).ok()?)
        };
        let total = header.checked_add(len)?;
        let whole = b.get(..total)?;
        let contents = whole.get(header..)?;
        self.0 = b.get(total..)?;
        Some((tag, whole, contents))
    }

    /// The next element's contents, if its tag is `tag` (ReadASN1).
    pub fn read(&mut self, tag: u8) -> Option<Der<'a>> {
        let mut peek = *self;
        let (t, _, contents) = peek.any_element()?;
        if t != tag {
            return None;
        }
        *self = peek;
        Some(Der(contents))
    }

    /// The next element whole, if its tag is `tag` (ReadASN1Element).
    pub fn read_element(&mut self, tag: u8) -> Option<&'a [u8]> {
        let mut peek = *self;
        let (t, whole, _) = peek.any_element()?;
        if t != tag {
            return None;
        }
        *self = peek;
        Some(whole)
    }

    /// The next element's contents if its tag is `tag`, or none and nothing read
    /// (ReadOptionalASN1).
    pub fn optional(&mut self, tag: u8) -> Option<Option<Der<'a>>> {
        if self.peek(tag) {
            self.read(tag).map(Some)
        } else {
            Some(None)
        }
    }

    /// Whether the next element's tag is `tag` (PeekASN1Tag).
    pub fn peek(&self, tag: u8) -> bool {
        self.0.first() == Some(&tag)
    }

    /// An INTEGER's octets, minimal: its magnitude if non-negative (readASN1Bytes).
    pub fn unsigned_bytes(&mut self) -> Option<&'a [u8]> {
        let mut peek = *self;
        let b = peek.read(INTEGER)?.0;
        if !minimal_integer(b) || b.first()? & 0x80 != 0 {
            return None;
        }
        *self = peek;
        let i = b
            .iter()
            .position(|x| *x != 0)
            .unwrap_or(b.len().saturating_sub(1));
        b.get(i..)
    }

    /// An INTEGER as a signed two's complement value's octets, minimal.
    pub fn integer_raw(&mut self) -> Option<&'a [u8]> {
        let mut peek = *self;
        let b = peek.read(INTEGER)?.0;
        if !minimal_integer(b) {
            return None;
        }
        *self = peek;
        Some(b)
    }

    /// An INTEGER that fits an int64.
    pub fn int64(&mut self) -> Option<i64> {
        let mut peek = *self;
        let b = peek.read(INTEGER)?.0;
        let v = signed64(b)?;
        *self = peek;
        Some(v)
    }

    /// A BOOLEAN, DER: 0x00 or 0xff.
    pub fn boolean(&mut self) -> Option<bool> {
        let mut peek = *self;
        let v = match peek.read(BOOLEAN)?.0 {
            [0x00] => false,
            [0xff] => true,
            _ => return None,
        };
        *self = peek;
        Some(v)
    }

    /// An OBJECT IDENTIFIER's encoded octets, as ReadASN1ObjectIdentifier reads them:
    /// each arc minimal and below 2³¹.
    pub fn oid(&mut self) -> Option<&'a [u8]> {
        let mut peek = *self;
        let b = peek.read(OID)?.0;
        oid_arcs(b)?;
        *self = peek;
        Some(b)
    }

    /// An INTEGER that fits a uint64, non-negative (readASN1Uint64).
    pub fn uint64(&mut self) -> Option<u64> {
        let mut peek = *self;
        let b = peek.read(INTEGER)?.0;
        if !minimal_integer(b)
            || b.first()? & 0x80 != 0
            || b.len() > 9
            || (b.len() == 9 && b.first() != Some(&0))
        {
            return None;
        }
        *self = peek;
        Some(b.iter().fold(0u64, |a, &x| (a << 8) | u64::from(x)))
    }

    /// An integer under `tag` that fits an int64 (ReadASN1Int64WithTag).
    pub fn int64_with_tag(&mut self, tag: u8) -> Option<i64> {
        let mut peek = *self;
        let b = peek.read(tag)?.0;
        let v = signed64(b)?;
        *self = peek;
        Some(v)
    }

    /// The next element under `tag`, skipped where present (SkipOptionalASN1).
    pub fn skip_optional(&mut self, tag: u8) -> Option<()> {
        if self.peek(tag) {
            self.read(tag)?;
        }
        Some(())
    }

    /// A BIT STRING's octets and its unused bits (ReadASN1BitString).
    pub fn bit_string(&mut self) -> Option<(&'a [u8], u8)> {
        let mut peek = *self;
        let b = peek.read(BIT_STRING)?.0;
        let (&unused, rest) = b.split_first()?;
        if unused > 7 || (rest.is_empty() && unused != 0) {
            return None;
        }
        if let Some(&last) = rest.last()
            && last & ((1u8 << unused) - 1) != 0
        {
            return None;
        }
        *self = peek;
        Some((rest, unused))
    }
}

/// checkASN1Integer.
fn minimal_integer(b: &[u8]) -> bool {
    match b {
        [] => false,
        [_] => true,
        [0x00, second, ..] => second & 0x80 != 0,
        [0xff, second, ..] => second & 0x80 == 0,
        _ => true,
    }
}

/// An integer's octets as an int64: minimal, at most eight (asn1Signed).
fn signed64(b: &[u8]) -> Option<i64> {
    if !minimal_integer(b) || b.len() > 8 {
        return None;
    }
    let mut v: i64 = if b.first()? & 0x80 != 0 { -1 } else { 0 };
    for &x in b {
        v = (v << 8) | i64::from(x);
    }
    Some(v)
}

/// An OID's arcs as cryptobyte's ReadASN1ObjectIdentifier reads them (readBase128Int):
/// non-empty, each arc at most five octets, below 2³¹, without a leading 0x80.
pub fn oid_arcs(b: &[u8]) -> Option<Vec<u32>> {
    if b.is_empty() {
        return None;
    }
    let mut rest = b;
    let mut arcs = Vec::new();
    while !rest.is_empty() {
        let mut v: u32 = 0;
        let mut done = false;
        for i in 0..5 {
            if v >= 1 << (31 - 7) {
                return None;
            }
            let (&x, tail) = rest.split_first()?;
            rest = tail;
            if i == 0 && x == 0x80 {
                return None;
            }
            v = (v << 7) | u32::from(x & 0x7f);
            if x & 0x80 == 0 {
                done = true;
                break;
            }
        }
        if !done {
            return None;
        }
        if arcs.is_empty() {
            if v < 80 {
                arcs.push(v / 40);
                arcs.push(v % 40);
            } else {
                arcs.push(2);
                arcs.push(v - 80);
            }
        } else {
            arcs.push(v);
        }
    }
    Some(arcs)
}

/// An OID's dotted text, as encoding/asn1's ObjectIdentifier.String writes it, of octets
/// `oid_arcs` reads.
pub fn oid_text(b: &[u8]) -> String {
    oid_arcs(b)
        .unwrap_or_default()
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// An element of `tag` around `contents`, its length in DER's shortest form.
pub fn tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = contents.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes: Vec<u8> = len.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(contents);
    out
}

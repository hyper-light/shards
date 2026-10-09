//! Go 1.26's encoding/asn1 Unmarshal (asn1.go), for the structures Go and the libraries
//! sigstore-go uses read with it rather than cryptobyte: a field at a time, as
//! `parseField` reads one into a Go value of a kind, with its tags (explicit, implicit,
//! optional, defaults) and its leniency (extra octets after a SEQUENCE's last field,
//! and after the value as a whole, are left alone).

use crate::gotime;
use crate::time::Time;

/// encoding/asn1's errors, in its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asn1Error(pub String);

impl std::fmt::Display for Asn1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn syntax(s: &str) -> Asn1Error {
    Asn1Error(format!("asn1: syntax error: {s}"))
}

fn structural(s: &str) -> Asn1Error {
    Asn1Error(format!("asn1: structure error: {s}"))
}

pub const CLASS_UNIVERSAL: u8 = 0;
pub const CLASS_APPLICATION: u8 = 1;
pub const CLASS_CONTEXT: u8 = 2;
pub const CLASS_PRIVATE: u8 = 3;

pub const TAG_BOOLEAN: u32 = 1;
pub const TAG_INTEGER: u32 = 2;
pub const TAG_BIT_STRING: u32 = 3;
pub const TAG_OCTET_STRING: u32 = 4;
pub const TAG_OID: u32 = 6;
pub const TAG_ENUM: u32 = 10;
pub const TAG_UTF8_STRING: u32 = 12;
pub const TAG_SEQUENCE: u32 = 16;
pub const TAG_SET: u32 = 17;
pub const TAG_NUMERIC_STRING: u32 = 18;
pub const TAG_PRINTABLE_STRING: u32 = 19;
pub const TAG_T61_STRING: u32 = 20;
pub const TAG_IA5_STRING: u32 = 22;
pub const TAG_UTC_TIME: u32 = 23;
pub const TAG_GENERALIZED_TIME: u32 = 24;
pub const TAG_GENERAL_STRING: u32 = 27;
pub const TAG_BMP_STRING: u32 = 30;

/// A tag and length (tagAndLength).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tl {
    pub class: u8,
    pub tag: u32,
    pub length: usize,
    pub compound: bool,
}

/// parseBase128Int: an arc or a tag number, below 2³¹.
fn base128(b: &[u8], mut offset: usize) -> Result<(u32, usize), Asn1Error> {
    let mut ret: i64 = 0;
    let mut shifted = 0;
    while let Some(&x) = b.get(offset) {
        if shifted == 5 {
            return Err(structural("base 128 integer too large"));
        }
        ret <<= 7;
        if shifted == 0 && x == 0x80 {
            return Err(syntax("integer is not minimally encoded"));
        }
        ret |= i64::from(x & 0x7f);
        offset += 1;
        if x & 0x80 == 0 {
            if ret > i64::from(i32::MAX) {
                return Err(structural("base 128 integer too large"));
            }
            return Ok((u32::try_from(ret).unwrap_or(0), offset));
        }
        shifted += 1;
    }
    Err(syntax("truncated base 128 integer"))
}

/// parseTagAndLength.
pub fn tag_and_length(b: &[u8], offset: usize) -> Result<(Tl, usize), Asn1Error> {
    let Some(&first) = b.get(offset) else {
        return Err(Asn1Error("asn1: internal error in parseTagAndLength".into()));
    };
    let mut offset = offset + 1;
    let mut ret = Tl {
        class: first >> 6,
        compound: first & 0x20 == 0x20,
        tag: u32::from(first & 0x1f),
        length: 0,
    };
    if ret.tag == 0x1f {
        let (t, o) = base128(b, offset)?;
        ret.tag = t;
        offset = o;
        if ret.tag < 0x1f {
            return Err(syntax("non-minimal tag"));
        }
    }
    let Some(&lb) = b.get(offset) else {
        return Err(syntax("truncated tag or length"));
    };
    offset += 1;
    if lb & 0x80 == 0 {
        ret.length = usize::from(lb & 0x7f);
    } else {
        let n = usize::from(lb & 0x7f);
        if n == 0 {
            return Err(syntax("indefinite length found (not DER)"));
        }
        let mut length: usize = 0;
        for _ in 0..n {
            let Some(&x) = b.get(offset) else {
                return Err(syntax("truncated tag or length"));
            };
            offset += 1;
            if length >= 1 << 23 {
                return Err(structural("length too large"));
            }
            length = (length << 8) | usize::from(x);
            if length == 0 {
                return Err(structural("superfluous leading zeros in length"));
            }
        }
        if length < 0x80 {
            return Err(structural("non-minimal length"));
        }
        ret.length = length;
    }
    Ok((ret, offset))
}

/// The Go kinds of value a field is read into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// asn1.RawValue: any element.
    Raw,
    Oid,
    BitString,
    /// time.Time: UTCTime by default, GeneralizedTime where the element is one.
    Time,
    BigInt,
    Int64,
    Int32,
    Enumerated,
    Bool,
    Flag,
    /// []byte: an OCTET STRING.
    Bytes,
    /// string: PrintableString by default, any string type the element is.
    Str,
    /// A struct: a SEQUENCE, read field by field by the caller.
    Struct,
    /// A slice of another kind: a SEQUENCE OF (`set` makes it a SET OF).
    SeqOf(&'static Kind),
    /// A slice type whose name ends in SET: a SET OF.
    SetOf(&'static Kind),
    /// An empty interface (any): any element, read by its universal tag.
    Any,
}

/// A field's tags and options (fieldParameters).
#[derive(Debug, Clone, Copy, Default)]
pub struct Params {
    pub optional: bool,
    pub explicit: bool,
    pub application: bool,
    pub private: bool,
    pub default: Option<i64>,
    pub tag: Option<u32>,
    pub string_type: u32,
    pub time_type: u32,
    pub set: bool,
    pub omit_empty: bool,
    /// The Go type's name (reflect.Type.Name), which Go's mismatch error prints.
    pub type_name: &'static str,
}

impl Params {
    pub fn tagged(tag: u32) -> Params {
        Params {
            tag: Some(tag),
            ..Params::default()
        }
    }

    pub fn explicit(tag: u32) -> Params {
        Params {
            explicit: true,
            tag: Some(tag),
            ..Params::default()
        }
    }

    pub fn optional(mut self) -> Params {
        self.optional = true;
        self
    }

    pub fn with_default(mut self, v: i64) -> Params {
        self.default = Some(v);
        self
    }

    /// The field's Go type, by name.
    pub fn named(mut self, name: &'static str) -> Params {
        self.type_name = name;
        self
    }

    /// The parameters as Go's %+v prints fieldParameters. Go prints its two pointers'
    /// addresses, which no run reproduces; they print here as `(ptr)` (D105).
    fn go_print(&self) -> String {
        let ptr = |set: bool| if set { "(ptr)" } else { "<nil>" };
        format!(
            "{{optional:{} explicit:{} application:{} private:{} defaultValue:{} tag:{} stringType:{} timeType:{} set:{} omitEmpty:{}}}",
            self.optional,
            self.explicit,
            self.application,
            self.private,
            ptr(self.default.is_some()),
            ptr(self.tag.is_some()),
            self.string_type,
            self.time_type,
            self.set,
            self.omit_empty
        )
    }
}

/// What a field was read as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<'a> {
    Raw {
        class: u8,
        tag: u32,
        compound: bool,
        bytes: &'a [u8],
        full: &'a [u8],
    },
    Oid(Vec<u32>),
    BitString {
        bytes: &'a [u8],
        bit_length: usize,
    },
    Time(Time),
    /// An INTEGER's two's complement octets, minimal.
    BigInt(&'a [u8]),
    Int(i64),
    Bool(bool),
    Bytes(&'a [u8]),
    Str(String),
    /// A SEQUENCE's contents, and the element whole (RawContent).
    Struct {
        inner: &'a [u8],
        full: &'a [u8],
    },
    List(Vec<Value<'a>>),
}

/// getUniversalType: (matchAny, tag, compound).
fn universal(kind: Kind, params: &Params) -> (bool, u32, bool) {
    match kind {
        Kind::Raw => (true, 0, false),
        Kind::Oid => (false, TAG_OID, false),
        Kind::BitString => (false, TAG_BIT_STRING, false),
        Kind::Time => (false, TAG_UTC_TIME, false),
        Kind::Enumerated => (false, TAG_ENUM, false),
        Kind::Flag | Kind::Bool => (false, TAG_BOOLEAN, false),
        Kind::BigInt | Kind::Int64 | Kind::Int32 => (false, TAG_INTEGER, false),
        Kind::Struct => (false, TAG_SEQUENCE, true),
        Kind::Bytes => (false, TAG_OCTET_STRING, false),
        Kind::SeqOf(_) => (false, if params.set { TAG_SET } else { TAG_SEQUENCE }, true),
        Kind::SetOf(_) => (false, TAG_SET, true),
        Kind::Any => (true, 0, false),
        Kind::Str => (false, TAG_PRINTABLE_STRING, false),
    }
}

fn check_integer(b: &[u8]) -> Result<(), Asn1Error> {
    match b {
        [] => Err(structural("empty integer")),
        [_] => Ok(()),
        [0, s, ..] if s & 0x80 == 0 => Err(structural("integer not minimally-encoded")),
        [0xff, s, ..] if s & 0x80 == 0x80 => Err(structural("integer not minimally-encoded")),
        _ => Ok(()),
    }
}

/// parseInt64.
pub fn int64(b: &[u8]) -> Result<i64, Asn1Error> {
    check_integer(b)?;
    if b.len() > 8 {
        return Err(structural("integer too large"));
    }
    let mut v: i64 = if b.first().is_some_and(|x| x & 0x80 != 0) {
        -1
    } else {
        0
    };
    for &x in b {
        v = (v << 8) | i64::from(x);
    }
    Ok(v)
}

fn int32(b: &[u8]) -> Result<i64, Asn1Error> {
    check_integer(b)?;
    let v = int64(b)?;
    if i64::from(v as i32) != v {
        return Err(structural("integer too large"));
    }
    Ok(v)
}

fn bool_of(b: &[u8]) -> Result<bool, Asn1Error> {
    match b {
        [0] => Ok(false),
        [0xff] => Ok(true),
        _ => Err(syntax("invalid boolean")),
    }
}

fn bit_string(b: &[u8]) -> Result<Value<'_>, Asn1Error> {
    let Some((&pad, rest)) = b.split_first() else {
        return Err(syntax("zero length BIT STRING"));
    };
    let last = b.last().copied().unwrap_or(0);
    if pad > 7 || (b.len() == 1 && pad > 0) || last & ((1u8 << pad.min(7)) - 1) != 0 {
        return Err(syntax("invalid padding bits in BIT STRING"));
    }
    Ok(Value::BitString {
        bytes: rest,
        bit_length: rest.len() * 8 - usize::from(pad),
    })
}

/// BitString.RightAlign.
pub fn right_align(bytes: &[u8], bit_length: usize) -> Vec<u8> {
    let shift = 8 - (bit_length % 8);
    if shift == 8 || bytes.is_empty() {
        return bytes.to_vec();
    }
    let mut a = vec![0u8; bytes.len()];
    if let (Some(first), Some(&b0)) = (a.first_mut(), bytes.first()) {
        *first = b0 >> shift;
    }
    for i in 1..bytes.len() {
        let prev = bytes.get(i - 1).copied().unwrap_or(0);
        let cur = bytes.get(i).copied().unwrap_or(0);
        if let Some(slot) = a.get_mut(i) {
            *slot = (prev << (8 - shift)) | (cur >> shift);
        }
    }
    a
}

/// parseObjectIdentifier.
pub fn oid(b: &[u8]) -> Result<Vec<u32>, Asn1Error> {
    if b.is_empty() {
        return Err(syntax("zero length OBJECT IDENTIFIER"));
    }
    let (v, mut offset) = base128(b, 0)?;
    let mut s = if v < 80 {
        vec![v / 40, v % 40]
    } else {
        vec![2, v - 80]
    };
    while offset < b.len() {
        let (v, o) = base128(b, offset)?;
        s.push(v);
        offset = o;
    }
    Ok(s)
}

fn printable(b: u8, asterisk: bool, ampersand: bool) -> bool {
    b.is_ascii_alphanumeric()
        || (b'\''..=b')').contains(&b)
        || (b'+'..=b'/').contains(&b)
        || matches!(b, b' ' | b':' | b'=' | b'?')
        || (asterisk && b == b'*')
        || (ampersand && b == b'&')
}

fn string_of(tag: u32, b: &[u8]) -> Result<String, Asn1Error> {
    match tag {
        TAG_PRINTABLE_STRING => {
            if b.iter().all(|&c| printable(c, true, true)) {
                Ok(String::from_utf8_lossy(b).into_owned())
            } else {
                Err(syntax("PrintableString contains invalid character"))
            }
        }
        TAG_NUMERIC_STRING => {
            if b.iter().all(|&c| c.is_ascii_digit() || c == b' ') {
                Ok(String::from_utf8_lossy(b).into_owned())
            } else {
                Err(syntax("NumericString contains invalid character"))
            }
        }
        TAG_IA5_STRING => {
            if b.is_ascii() {
                Ok(String::from_utf8_lossy(b).into_owned())
            } else {
                Err(syntax("IA5String contains invalid character"))
            }
        }
        TAG_T61_STRING | TAG_GENERAL_STRING => Ok(b.iter().map(|&c| char::from(c)).collect()),
        TAG_UTF8_STRING => {
            String::from_utf8(b.to_vec()).map_err(|_| Asn1Error("asn1: invalid UTF-8 string".into()))
        }
        TAG_BMP_STRING => bmp(b).map_err(|e| Asn1Error(e.into())),
        t => Err(syntax(&format!("internal error: unknown string type {t}"))),
    }
}

/// parseBMPString.
pub fn bmp(b: &[u8]) -> Result<String, &'static str> {
    if !b.len().is_multiple_of(2) {
        return Err("invalid BMPString");
    }
    let mut v = b;
    if v.len() >= 2 && v.ends_with(&[0, 0]) {
        v = v.get(..v.len() - 2).unwrap_or_default();
    }
    let mut units = Vec::with_capacity(v.len() / 2);
    for c in v.chunks(2) {
        let p = u16::from_be_bytes([c.first().copied().unwrap_or(0), c.get(1).copied().unwrap_or(0)]);
        if p == 0xfffe || p == 0xffff || (0xfdd0..=0xfdef).contains(&p) || (0xd800..=0xdfff).contains(&p) {
            return Err("invalid BMPString");
        }
        units.push(p);
    }
    Ok(String::from_utf16_lossy(&units))
}

/// parseUTCTime.
fn utc_time(b: &[u8]) -> Result<Time, Asn1Error> {
    // The minute layout first; the round trip against whichever layout parsed.
    let layout = match gotime::parse("0601021504Z0700", b) {
        Ok(_) => "0601021504Z0700",
        Err(_) => "060102150405Z0700",
    };
    let t = gotime::parse_exact(layout, b).map_err(Asn1Error)?;
    Ok(if gotime::year(&t) >= 2050 {
        gotime::add_years(&t, -100)
    } else {
        t
    })
}

/// parseGeneralizedTime.
fn generalized_time(b: &[u8]) -> Result<Time, Asn1Error> {
    gotime::parse_exact("20060102150405.999999999Z0700", b).map_err(Asn1Error)
}

/// A field `kind` at `offset` of `bytes`, as parseField reads it: the value (None where
/// an optional field is absent; its default, if any, is the caller's), and the offset
/// after it.
pub fn field<'a>(
    bytes: &'a [u8],
    offset: usize,
    kind: Kind,
    params: &Params,
) -> Result<(Option<Value<'a>>, usize), Asn1Error> {
    let init = offset;
    if offset == bytes.len() {
        if params.optional {
            return Ok((None, offset));
        }
        return Err(syntax("sequence truncated"));
    }
    if kind == Kind::Any {
        return any(bytes, offset);
    }
    let (mut t, mut offset) = tag_and_length(bytes, offset)?;
    if params.explicit {
        let expected = if params.application {
            CLASS_APPLICATION
        } else {
            CLASS_CONTEXT
        };
        if offset == bytes.len() {
            return Err(structural("explicit tag has no child"));
        }
        if t.class == expected && Some(t.tag) == params.tag && (t.length == 0 || t.compound) {
            if kind == Kind::Raw {
                // A RawValue takes the explicit element itself.
            } else if t.length > 0 {
                let (inner, o) = tag_and_length(bytes, offset)?;
                t = inner;
                offset = o;
            } else if kind == Kind::Flag {
                return Ok((Some(Value::Bool(true)), offset));
            } else {
                return Err(structural("zero length explicit tag was not an asn1.Flag"));
            }
        } else if params.optional {
            return Ok((None, init));
        } else {
            return Err(structural("explicitly tagged member didn't match"));
        }
    }
    let (match_any, mut universal_tag, compound) = universal(kind, params);
    if universal_tag == TAG_PRINTABLE_STRING && kind == Kind::Str {
        if t.class == CLASS_UNIVERSAL {
            if matches!(
                t.tag,
                TAG_IA5_STRING
                    | TAG_GENERAL_STRING
                    | TAG_T61_STRING
                    | TAG_UTF8_STRING
                    | TAG_NUMERIC_STRING
                    | TAG_BMP_STRING
            ) {
                universal_tag = t.tag;
            }
        } else if params.string_type != 0 {
            universal_tag = params.string_type;
        }
    }
    if universal_tag == TAG_UTC_TIME {
        if t.class == CLASS_UNIVERSAL {
            if t.tag == TAG_GENERALIZED_TIME {
                universal_tag = t.tag;
            }
        } else if params.time_type != 0 {
            universal_tag = params.time_type;
        }
    }
    if params.set {
        universal_tag = TAG_SET;
    }
    let mut match_any_class_and_tag = match_any;
    let mut expected_class = CLASS_UNIVERSAL;
    let mut expected_tag = universal_tag;
    if !params.explicit
        && let Some(tag) = params.tag
    {
        expected_class = if params.application {
            CLASS_APPLICATION
        } else if params.private {
            CLASS_PRIVATE
        } else {
            CLASS_CONTEXT
        };
        expected_tag = tag;
        match_any_class_and_tag = false;
    }
    if (!match_any_class_and_tag && (t.class != expected_class || t.tag != expected_tag))
        || (!match_any && t.compound != compound)
    {
        if params.optional {
            return Ok((None, init));
        }
        // Go prints the field's parameters and type here (%+v), pointers among them; the
        // words before them are its.
        return Err(structural(&format!(
            "tags don't match ({expected_tag} vs {{class:{} tag:{} length:{} isCompound:{}}}) {} {} @{offset}",
            t.class,
            t.tag,
            t.length,
            t.compound,
            params.go_print(),
            params.type_name
        )));
    }
    let end = offset
        .checked_add(t.length)
        .filter(|e| *e <= bytes.len())
        .ok_or_else(|| syntax("data truncated"))?;
    let inner = bytes.get(offset..end).unwrap_or_default();
    let full = bytes.get(init..end).unwrap_or_default();
    let value = match kind {
        Kind::Raw => Value::Raw {
            class: t.class,
            tag: t.tag,
            compound: t.compound,
            bytes: inner,
            full,
        },
        Kind::Oid => Value::Oid(oid(inner)?),
        Kind::BitString => bit_string(inner)?,
        Kind::Time => Value::Time(if universal_tag == TAG_UTC_TIME {
            utc_time(inner)?
        } else {
            generalized_time(inner)?
        }),
        Kind::Enumerated => Value::Int(int32(inner)?),
        Kind::Flag => Value::Bool(true),
        Kind::BigInt => {
            check_integer(inner)?;
            Value::BigInt(inner)
        }
        Kind::Bool => Value::Bool(bool_of(inner)?),
        Kind::Int64 => Value::Int(int64(inner)?),
        Kind::Int32 => Value::Int(int32(inner)?),
        Kind::Struct => Value::Struct { inner, full },
        Kind::Bytes => Value::Bytes(inner),
        Kind::SeqOf(elem) | Kind::SetOf(elem) => Value::List(sequence_of(inner, *elem)?),
        Kind::Any => return any(bytes, init),
        Kind::Str => Value::Str(string_of(universal_tag, inner)?),
    };
    Ok((Some(value), end))
}

/// An element read into an empty interface: its universal primitives parsed by their
/// tags (their errors Go's), anything else left unread.
fn any(bytes: &[u8], offset: usize) -> Result<(Option<Value<'_>>, usize), Asn1Error> {
    let (t, o) = tag_and_length(bytes, offset)?;
    let end = o
        .checked_add(t.length)
        .filter(|e| *e <= bytes.len())
        .ok_or_else(|| syntax("data truncated"))?;
    let inner = bytes.get(o..end).unwrap_or_default();
    if !t.compound && t.class == CLASS_UNIVERSAL {
        match t.tag {
            TAG_BOOLEAN => {
                bool_of(inner)?;
            }
            TAG_PRINTABLE_STRING | TAG_NUMERIC_STRING | TAG_IA5_STRING | TAG_T61_STRING | TAG_UTF8_STRING
            | TAG_BMP_STRING => {
                string_of(t.tag, inner)?;
            }
            TAG_INTEGER => {
                int64(inner)?;
            }
            TAG_BIT_STRING => {
                bit_string(inner)?;
            }
            TAG_OID => {
                oid(inner)?;
            }
            TAG_UTC_TIME => {
                utc_time(inner)?;
            }
            TAG_GENERALIZED_TIME => {
                generalized_time(inner)?;
            }
            _ => {}
        }
    }
    Ok((
        Some(Value::Raw {
            class: t.class,
            tag: t.tag,
            compound: t.compound,
            bytes: inner,
            full: bytes.get(offset..end).unwrap_or_default(),
        }),
        end,
    ))
}

/// parseSequenceOf.
fn sequence_of(bytes: &[u8], elem: Kind) -> Result<Vec<Value<'_>>, Asn1Error> {
    let (match_any, expected, compound) = universal(elem, &Params::default());
    let mut n = 0;
    let mut offset = 0;
    while offset < bytes.len() {
        let (mut t, o) = tag_and_length(bytes, offset)?;
        match t.tag {
            TAG_IA5_STRING | TAG_GENERAL_STRING | TAG_T61_STRING | TAG_UTF8_STRING | TAG_NUMERIC_STRING
            | TAG_BMP_STRING => {
                t.tag = TAG_PRINTABLE_STRING;
            }
            TAG_GENERALIZED_TIME | TAG_UTC_TIME => t.tag = TAG_UTC_TIME,
            _ => {}
        }
        if !match_any && (t.class != CLASS_UNIVERSAL || t.compound != compound || t.tag != expected) {
            return Err(structural("sequence tag mismatch"));
        }
        offset = o
            .checked_add(t.length)
            .filter(|e| *e <= bytes.len())
            .ok_or_else(|| syntax("truncated sequence"))?;
        n += 1;
    }
    let mut out = Vec::with_capacity(n);
    let mut offset = 0;
    for _ in 0..n {
        let (v, o) = field(bytes, offset, elem, &Params::default())?;
        if let Some(v) = v {
            out.push(v);
        }
        offset = o;
    }
    Ok(out)
}

/// A SEQUENCE's fields, read one after another as parseField reads a struct's.
#[derive(Debug, Clone, Copy)]
pub struct Fields<'a> {
    pub bytes: &'a [u8],
    pub offset: usize,
}

impl<'a> Fields<'a> {
    pub fn new(bytes: &'a [u8]) -> Fields<'a> {
        Fields { bytes, offset: 0 }
    }

    pub fn next(&mut self, kind: Kind, params: &Params) -> Result<Option<Value<'a>>, Asn1Error> {
        let (v, o) = field(self.bytes, self.offset, kind, params)?;
        self.offset = o;
        Ok(v)
    }
}

/// Unmarshal into a value of `kind`: the value and the octets after it.
pub fn unmarshal<'a>(b: &'a [u8], kind: Kind, params: &Params) -> Result<(Value<'a>, &'a [u8]), Asn1Error> {
    let (v, o) = field(b, 0, kind, params)?;
    let v = v.ok_or_else(|| syntax("sequence truncated"))?;
    Ok((v, b.get(o..).unwrap_or_default()))
}

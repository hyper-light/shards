//! HPACK (RFC 7541), HTTP/2's field compression, for the credential injector's HTTP/2 (D242,
//! step 7). Compression is stateful and each endpoint has a decoder and an encoder of its own
//! (RFC 9113 §4.3.1), so an intermediary decodes what one hop's peer sent with that hop's
//! decoder and encodes what it sends on the other hop with that hop's encoder: an index into one
//! table never stands for anything in the other. A field keeps how its sender sent it, and one
//! sent never indexed is never indexed by any hop after (§7.1.3).

use std::collections::VecDeque;
use std::fmt;
use std::sync::LazyLock;

use zeroize::Zeroizing;

/// The octets an entry costs a table beyond its name's and value's (§4.1), as a field line
/// costs a header list (RFC 9113 §6.5.2).
const ENTRY: usize = 32;

/// How a field was sent (§6.2), which the next hop keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indexing {
    /// Added to its sender's table (§6.2.1), or sent as an index into one (§6.1).
    Incremental,
    /// Sent without being added (§6.2.2).
    Without,
    /// Never to be added to a table, by this hop or any after it (§6.2.3, §7.1.3).
    Never,
}

/// A field line as a block carries it: its name, its value and how it was sent. Its value never
/// prints, as it may be a placeholder (a capability, D242) or a credential.
#[derive(Clone, PartialEq, Eq)]
pub struct Field {
    pub name: Vec<u8>,
    pub value: Zeroizing<Vec<u8>>,
    pub indexing: Indexing,
}

impl Field {
    pub fn new(name: &[u8], value: &[u8], indexing: Indexing) -> Field {
        Field {
            name: name.to_vec(),
            value: Zeroizing::new(value.to_vec()),
            indexing,
        }
    }

    /// Its size as a table counts an entry (§4.1) and a header list a line (RFC 9113 §6.5.2).
    pub fn size(&self) -> usize {
        size(&self.name, &self.value)
    }
}

impl fmt::Debug for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Field")
            .field("name", &String::from_utf8_lossy(&self.name))
            .field("value", &format_args!("{} bytes", self.value.len()))
            .field("indexing", &self.indexing)
            .finish()
    }
}

fn size(name: &[u8], value: &[u8]) -> usize {
    name.len().saturating_add(value.len()).saturating_add(ENTRY)
}

/// Why a block is not HPACK this side can decode: in HTTP/2 each is a connection error of type
/// COMPRESSION_ERROR (RFC 9113 §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The block ends inside a representation.
    Truncated,
    /// An integer past what a `usize` holds (§5.1).
    Integer,
    /// An index of no entry, or zero (§2.3.3, §6.1).
    Index,
    /// A Huffman-coded string holding EOS, or padded with more than 7 bits or with bits other
    /// than EOS's first (§5.2).
    Huffman,
    /// A dynamic table size update past this side's limit, after a field, or missing at the
    /// start of the first block after the limit was lowered under the table (§4.2, §6.3).
    SizeUpdate,
}

/// A decoded block: its fields, or none of them kept where they add up past what its caller
/// holds, the block decoded whole all the same, as a field block "MUST be processed to ensure a
/// consistent connection state" (RFC 9113 §10.5.1).
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    Fields(Vec<Field>),
    TooLarge,
}

/// A dynamic table (§2.3.2, §4): its newest entry first, its size as §4.1 counts it, and the
/// most it may hold. Its values are wiped as they are evicted.
#[derive(Default)]
struct Table {
    entries: VecDeque<(Vec<u8>, Zeroizing<Vec<u8>>)>,
    size: usize,
    max: usize,
}

impl fmt::Debug for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Table")
            .field("entries", &self.entries.len())
            .field("size", &self.size)
            .field("max", &self.max)
            .finish()
    }
}

impl Table {
    fn new(max: usize) -> Table {
        Table {
            max,
            ..Table::default()
        }
    }

    /// The entry at `index` of the index space the static and dynamic tables share (§2.3.3).
    fn get(&self, index: usize) -> Option<(&[u8], &[u8])> {
        match index.checked_sub(1)? {
            i if i < STATIC.len() => STATIC.get(i).map(|&(n, v)| (n, v)),
            i => self
                .entries
                .get(i - STATIC.len())
                .map(|(n, v)| (n.as_slice(), v.as_slice())),
        }
    }

    /// Adds an entry, evicting the oldest until it fits; one larger than the table empties it
    /// and is not added (§4.4).
    fn insert(&mut self, name: Vec<u8>, value: Zeroizing<Vec<u8>>) {
        let cost = size(&name, &value);
        self.evict(self.max.saturating_sub(cost));
        if cost <= self.max {
            self.size = self.size.saturating_add(cost);
            self.entries.push_front((name, value));
        } else {
            self.entries.clear();
            self.size = 0;
        }
    }

    fn resize(&mut self, max: usize) {
        self.max = max;
        self.evict(max);
    }

    /// Evicts the oldest entries until the table holds `room` or less (§4.3).
    fn evict(&mut self, room: usize) {
        while self.size > room {
            let Some((name, value)) = self.entries.pop_back() else {
                self.size = 0;
                return;
            };
            self.size = self.size.saturating_sub(size(&name, &value));
        }
    }

    /// The lowest index of an entry holding `name` and, where `whole`, `value` too; else of one
    /// holding `name` alone. With the whole entry's index first.
    fn find(&self, name: &[u8], value: &[u8], whole: bool) -> Option<(usize, bool)> {
        let all = STATIC
            .iter()
            .map(|&(n, v)| (n, v))
            .chain(self.entries.iter().map(|(n, v)| (n.as_slice(), v.as_slice())));
        let mut named = None;
        for (i, (n, v)) in all.enumerate() {
            if n != name {
                continue;
            }
            if whole && v == value {
                return Some((i.saturating_add(1), true));
            }
            named = named.or(Some((i.saturating_add(1), false)));
        }
        named
    }
}

/// A hop's decoder: the table its peer's encoder keeps, and the most this side lets it hold,
/// this side's SETTINGS_HEADER_TABLE_SIZE (RFC 9113 §6.5.2).
#[derive(Debug)]
pub struct Decoder {
    table: Table,
    limit: usize,
}

impl Decoder {
    /// With this side's SETTINGS_HEADER_TABLE_SIZE as its peer knows it: the protocol's 4,096
    /// until this side says another (RFC 9113 §6.5.2).
    pub fn new(limit: usize) -> Decoder {
        Decoder {
            table: Table::new(limit),
            limit,
        }
    }

    /// This side's SETTINGS_HEADER_TABLE_SIZE once its peer has acknowledged it (RFC 9113
    /// §6.5.3). Lowered under the table's size, the peer's next block must begin by saying so.
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }

    /// `block`'s fields, decoded whole; none kept where they add up past `most` octets as RFC
    /// 9113 §6.5.2 counts a header list.
    pub fn decode(&mut self, block: &[u8], most: usize) -> Result<Decoded, Error> {
        let mut at = 0;
        let mut fields = Vec::new();
        let mut total = 0usize;
        let mut over = false;
        let mut first = true;
        while let Some(&b) = block.get(at) {
            if b & 0xe0 == 0x20 {
                // A dynamic table size update, only where a block begins (§4.2, §6.3).
                let max = integer(block, &mut at, 5)?;
                if !first || max > self.limit {
                    return Err(Error::SizeUpdate);
                }
                self.table.resize(max);
                continue;
            }
            if first && self.table.max > self.limit {
                return Err(Error::SizeUpdate);
            }
            first = false;
            let field = if b & 0x80 != 0 {
                let index = integer(block, &mut at, 7)?;
                let (name, value) = self.table.get(index).ok_or(Error::Index)?;
                total = total.saturating_add(size(name, value));
                (total <= most && !over).then(|| Field::new(name, value, Indexing::Incremental))
            } else {
                let (prefix, indexing) = match b & 0xf0 {
                    0x00 => (4, Indexing::Without),
                    0x10 => (4, Indexing::Never),
                    _ => (6, Indexing::Incremental),
                };
                let index = integer(block, &mut at, prefix)?;
                let name = match index {
                    0 => std::mem::take(&mut *string(block, &mut at)?),
                    i => self.table.get(i).ok_or(Error::Index)?.0.to_vec(),
                };
                let value = string(block, &mut at)?;
                total = total.saturating_add(size(&name, &value));
                let field = Field {
                    name,
                    value,
                    indexing,
                };
                if indexing == Indexing::Incremental {
                    self.table.insert(field.name.clone(), field.value.clone());
                }
                (total <= most && !over).then_some(field)
            };
            match field {
                Some(field) => fields.push(field),
                None => {
                    over = true;
                    fields.clear();
                }
            }
        }
        Ok(if over {
            Decoded::TooLarge
        } else {
            Decoded::Fields(fields)
        })
    }
}

/// A hop's encoder: the table this side keeps for its peer's decoder, as large as the peer's
/// SETTINGS_HEADER_TABLE_SIZE lets it be up to `most`, what this side lets the other hop's peer
/// make its own (so that a hop's table holds no more than the other's), and the sizes it has to
/// say at its next block's start (§4.2).
#[derive(Debug)]
pub struct Encoder {
    table: Table,
    most: usize,
    /// The least size since its last block, then the last, where the peer changed its limit.
    sizes: Option<(usize, usize)>,
}

impl Encoder {
    /// Under its peer's SETTINGS_HEADER_TABLE_SIZE as the protocol starts it, `limit` (RFC 9113
    /// §6.5.2: 4,096), and no more than `most`.
    pub fn new(limit: usize, most: usize) -> Encoder {
        let size = limit.min(most);
        Encoder {
            table: Table::new(size),
            most,
            sizes: (size < limit).then_some((size, size)),
        }
    }

    /// The peer's SETTINGS_HEADER_TABLE_SIZE as it changes: the table follows it, up to
    /// `most`, and says so at the next block's start, the least size first where it shrank in
    /// between (§4.2).
    pub fn set_limit(&mut self, limit: usize) {
        let size = limit.min(self.most);
        let least = self.sizes.map_or(size, |(least, _)| least.min(size));
        self.sizes = Some((least, size));
    }

    /// `fields` as one block onto `out`, each as its sender sent it: a field sent never indexed
    /// (and each `never` names) written never indexed, its value as its octets, so that its
    /// length says nothing of its characters (Huffman's codes differ in length, Appendix B); any
    /// other value Huffman-coded where that is no longer, as RFC 7541's own examples are (C.4, C.6).
    pub fn encode(&mut self, fields: &[Field], out: &mut Vec<u8>) {
        if let Some((least, last)) = self.sizes.take() {
            for size in [least, last] {
                put_integer(out, 0x20, 5, size);
                self.table.resize(size);
                if least == last {
                    break;
                }
            }
        }
        for field in fields {
            self.field(field, out);
        }
    }

    fn field(&mut self, field: &Field, out: &mut Vec<u8>) {
        let mut indexing = field.indexing;
        if indexing == Indexing::Incremental && field.size() > self.table.max {
            indexing = Indexing::Without;
        }
        let found = self
            .table
            .find(&field.name, &field.value, indexing != Indexing::Never);
        if let Some((index, true)) = found {
            put_integer(out, 0x80, 7, index);
            return;
        }
        let (pattern, prefix) = match indexing {
            Indexing::Incremental => (0x40, 6),
            Indexing::Without => (0x00, 4),
            Indexing::Never => (0x10, 4),
        };
        put_integer(out, pattern, prefix, found.map_or(0, |(index, _)| index));
        if found.is_none() {
            put_string(out, &field.name, true);
        }
        put_string(out, &field.value, indexing != Indexing::Never);
        if indexing == Indexing::Incremental {
            self.table.insert(field.name.clone(), field.value.clone());
        }
    }
}

/// An integer of an `n`-bit prefix (§5.1), from `at`.
fn integer(block: &[u8], at: &mut usize, n: u32) -> Result<usize, Error> {
    let mask = (1u8 << n).wrapping_sub(1);
    let first = *block.get(*at).ok_or(Error::Truncated)? & mask;
    *at = at.saturating_add(1);
    if first < mask {
        return Ok(usize::from(first));
    }
    let mut value = u64::from(first);
    let mut shift = 0u32;
    loop {
        let b = *block.get(*at).ok_or(Error::Truncated)?;
        *at = at.saturating_add(1);
        let low = u64::from(b & 0x7f);
        let add = low
            .checked_shl(shift)
            .filter(|a| a.checked_shr(shift) == Some(low))
            .ok_or(Error::Integer)?;
        value = value.checked_add(add).ok_or(Error::Integer)?;
        if b & 0x80 == 0 {
            return usize::try_from(value).map_err(|_| Error::Integer);
        }
        shift = shift.saturating_add(7);
    }
}

fn put_integer(out: &mut Vec<u8>, pattern: u8, n: u32, value: usize) {
    let mask = (1u8 << n).wrapping_sub(1);
    if value < usize::from(mask) {
        out.push(pattern | value as u8);
        return;
    }
    out.push(pattern | mask);
    let mut rest = value - usize::from(mask);
    while rest >= 0x80 {
        out.push((rest & 0x7f) as u8 | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

/// A string literal (§5.2), from `at`, wiped as it drops.
fn string(block: &[u8], at: &mut usize) -> Result<Zeroizing<Vec<u8>>, Error> {
    let huffman = block.get(*at).ok_or(Error::Truncated)? & 0x80 != 0;
    let len = integer(block, at, 7)?;
    let end = at.checked_add(len).ok_or(Error::Truncated)?;
    let raw = block.get(*at..end).ok_or(Error::Truncated)?;
    *at = end;
    if huffman {
        unhuffman(raw)
    } else {
        Ok(Zeroizing::new(raw.to_vec()))
    }
}

fn put_string(out: &mut Vec<u8>, s: &[u8], huffman: bool) {
    let bits: usize = s
        .iter()
        .map(|&b| HUFFMAN.get(usize::from(b)).map_or(0, |&(_, n)| usize::from(n)))
        .sum();
    let coded = bits.div_ceil(8);
    if huffman && coded <= s.len() {
        put_integer(out, 0x80, 7, coded);
        let (mut acc, mut n) = (0u64, 0u32);
        for &b in s {
            let Some(&(code, len)) = HUFFMAN.get(usize::from(b)) else {
                continue;
            };
            acc = (acc << len) | u64::from(code);
            n += u32::from(len);
            while n >= 8 {
                n -= 8;
                out.push((acc >> n) as u8);
            }
            acc &= (1u64 << n) - 1;
        }
        if n > 0 {
            // Padded with EOS's first bits, all ones (§5.2).
            out.push(((acc << (8 - n)) | ((1u64 << (8 - n)) - 1)) as u8);
        }
    } else {
        put_integer(out, 0x00, 7, s.len());
        out.extend_from_slice(s);
    }
}

/// Appendix B's codes by length: for each length, its codes in order, each with its symbol.
static CODES: LazyLock<Vec<Vec<(u32, u16)>>> = LazyLock::new(|| {
    let longest = HUFFMAN.iter().map(|&(_, n)| usize::from(n)).max().unwrap_or(0);
    let mut by_length = vec![Vec::new(); longest.saturating_add(1)];
    for (symbol, &(code, n)) in (0u16..).zip(HUFFMAN.iter()) {
        if let Some(codes) = by_length.get_mut(usize::from(n)) {
            codes.push((code, symbol));
        }
    }
    for codes in &mut by_length {
        codes.sort_unstable();
    }
    by_length
});

/// A Huffman-coded string decoded (§5.2), wiped as it drops.
fn unhuffman(raw: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    let mut out = Zeroizing::new(Vec::with_capacity(raw.len()));
    let (mut code, mut len) = (0u32, 0usize);
    for &byte in raw {
        for bit in (0..8).rev() {
            code = (code << 1) | u32::from((byte >> bit) & 1);
            len += 1;
            let codes = CODES.get(len).ok_or(Error::Huffman)?;
            if let Ok(i) = codes.binary_search_by_key(&code, |&(c, _)| c) {
                let &(_, symbol) = codes.get(i).ok_or(Error::Huffman)?;
                // EOS, the one symbol past a byte, is no string's (§5.2).
                out.push(u8::try_from(symbol).map_err(|_| Error::Huffman)?);
                (code, len) = (0, 0);
            }
        }
    }
    // What is left is padding: fewer than 8 bits, each a one, EOS's first (§5.2).
    if len > 7 || code != (1u32 << len) - 1 {
        return Err(Error::Huffman);
    }
    Ok(out)
}

// RFC 7541's Appendix A (Table 1) and Appendix B, as docs/research/s33-veil-creds/scripts/
// step7-hpack-tables.py reads them from the RFC's text; the tests below hold them to it.

/// The static table (Appendix A): index 1 is the first entry.
pub(crate) const STATIC: [(&[u8], &[u8]); 61] = [
    (b":authority", b""),
    (b":method", b"GET"),
    (b":method", b"POST"),
    (b":path", b"/"),
    (b":path", b"/index.html"),
    (b":scheme", b"http"),
    (b":scheme", b"https"),
    (b":status", b"200"),
    (b":status", b"204"),
    (b":status", b"206"),
    (b":status", b"304"),
    (b":status", b"400"),
    (b":status", b"404"),
    (b":status", b"500"),
    (b"accept-charset", b""),
    (b"accept-encoding", b"gzip, deflate"),
    (b"accept-language", b""),
    (b"accept-ranges", b""),
    (b"accept", b""),
    (b"access-control-allow-origin", b""),
    (b"age", b""),
    (b"allow", b""),
    (b"authorization", b""),
    (b"cache-control", b""),
    (b"content-disposition", b""),
    (b"content-encoding", b""),
    (b"content-language", b""),
    (b"content-length", b""),
    (b"content-location", b""),
    (b"content-range", b""),
    (b"content-type", b""),
    (b"cookie", b""),
    (b"date", b""),
    (b"etag", b""),
    (b"expect", b""),
    (b"expires", b""),
    (b"from", b""),
    (b"host", b""),
    (b"if-match", b""),
    (b"if-modified-since", b""),
    (b"if-none-match", b""),
    (b"if-range", b""),
    (b"if-unmodified-since", b""),
    (b"last-modified", b""),
    (b"link", b""),
    (b"location", b""),
    (b"max-forwards", b""),
    (b"proxy-authenticate", b""),
    (b"proxy-authorization", b""),
    (b"range", b""),
    (b"referer", b""),
    (b"refresh", b""),
    (b"retry-after", b""),
    (b"server", b""),
    (b"set-cookie", b""),
    (b"strict-transport-security", b""),
    (b"transfer-encoding", b""),
    (b"user-agent", b""),
    (b"vary", b""),
    (b"via", b""),
    (b"www-authenticate", b""),
];

/// The Huffman code (Appendix B): each symbol's code, aligned to its least significant bit,
/// and its length in bits; the last is EOS.
pub(crate) const HUFFMAN: [(u32, u8); 257] = [
    (0x1ff8, 13),
    (0x7fffd8, 23),
    (0xfffffe2, 28),
    (0xfffffe3, 28),
    (0xfffffe4, 28),
    (0xfffffe5, 28),
    (0xfffffe6, 28),
    (0xfffffe7, 28),
    (0xfffffe8, 28),
    (0xffffea, 24),
    (0x3ffffffc, 30),
    (0xfffffe9, 28),
    (0xfffffea, 28),
    (0x3ffffffd, 30),
    (0xfffffeb, 28),
    (0xfffffec, 28),
    (0xfffffed, 28),
    (0xfffffee, 28),
    (0xfffffef, 28),
    (0xffffff0, 28),
    (0xffffff1, 28),
    (0xffffff2, 28),
    (0x3ffffffe, 30),
    (0xffffff3, 28),
    (0xffffff4, 28),
    (0xffffff5, 28),
    (0xffffff6, 28),
    (0xffffff7, 28),
    (0xffffff8, 28),
    (0xffffff9, 28),
    (0xffffffa, 28),
    (0xffffffb, 28),
    (0x14, 6),
    (0x3f8, 10),
    (0x3f9, 10),
    (0xffa, 12),
    (0x1ff9, 13),
    (0x15, 6),
    (0xf8, 8),
    (0x7fa, 11),
    (0x3fa, 10),
    (0x3fb, 10),
    (0xf9, 8),
    (0x7fb, 11),
    (0xfa, 8),
    (0x16, 6),
    (0x17, 6),
    (0x18, 6),
    (0x0, 5),
    (0x1, 5),
    (0x2, 5),
    (0x19, 6),
    (0x1a, 6),
    (0x1b, 6),
    (0x1c, 6),
    (0x1d, 6),
    (0x1e, 6),
    (0x1f, 6),
    (0x5c, 7),
    (0xfb, 8),
    (0x7ffc, 15),
    (0x20, 6),
    (0xffb, 12),
    (0x3fc, 10),
    (0x1ffa, 13),
    (0x21, 6),
    (0x5d, 7),
    (0x5e, 7),
    (0x5f, 7),
    (0x60, 7),
    (0x61, 7),
    (0x62, 7),
    (0x63, 7),
    (0x64, 7),
    (0x65, 7),
    (0x66, 7),
    (0x67, 7),
    (0x68, 7),
    (0x69, 7),
    (0x6a, 7),
    (0x6b, 7),
    (0x6c, 7),
    (0x6d, 7),
    (0x6e, 7),
    (0x6f, 7),
    (0x70, 7),
    (0x71, 7),
    (0x72, 7),
    (0xfc, 8),
    (0x73, 7),
    (0xfd, 8),
    (0x1ffb, 13),
    (0x7fff0, 19),
    (0x1ffc, 13),
    (0x3ffc, 14),
    (0x22, 6),
    (0x7ffd, 15),
    (0x3, 5),
    (0x23, 6),
    (0x4, 5),
    (0x24, 6),
    (0x5, 5),
    (0x25, 6),
    (0x26, 6),
    (0x27, 6),
    (0x6, 5),
    (0x74, 7),
    (0x75, 7),
    (0x28, 6),
    (0x29, 6),
    (0x2a, 6),
    (0x7, 5),
    (0x2b, 6),
    (0x76, 7),
    (0x2c, 6),
    (0x8, 5),
    (0x9, 5),
    (0x2d, 6),
    (0x77, 7),
    (0x78, 7),
    (0x79, 7),
    (0x7a, 7),
    (0x7b, 7),
    (0x7ffe, 15),
    (0x7fc, 11),
    (0x3ffd, 14),
    (0x1ffd, 13),
    (0xffffffc, 28),
    (0xfffe6, 20),
    (0x3fffd2, 22),
    (0xfffe7, 20),
    (0xfffe8, 20),
    (0x3fffd3, 22),
    (0x3fffd4, 22),
    (0x3fffd5, 22),
    (0x7fffd9, 23),
    (0x3fffd6, 22),
    (0x7fffda, 23),
    (0x7fffdb, 23),
    (0x7fffdc, 23),
    (0x7fffdd, 23),
    (0x7fffde, 23),
    (0xffffeb, 24),
    (0x7fffdf, 23),
    (0xffffec, 24),
    (0xffffed, 24),
    (0x3fffd7, 22),
    (0x7fffe0, 23),
    (0xffffee, 24),
    (0x7fffe1, 23),
    (0x7fffe2, 23),
    (0x7fffe3, 23),
    (0x7fffe4, 23),
    (0x1fffdc, 21),
    (0x3fffd8, 22),
    (0x7fffe5, 23),
    (0x3fffd9, 22),
    (0x7fffe6, 23),
    (0x7fffe7, 23),
    (0xffffef, 24),
    (0x3fffda, 22),
    (0x1fffdd, 21),
    (0xfffe9, 20),
    (0x3fffdb, 22),
    (0x3fffdc, 22),
    (0x7fffe8, 23),
    (0x7fffe9, 23),
    (0x1fffde, 21),
    (0x7fffea, 23),
    (0x3fffdd, 22),
    (0x3fffde, 22),
    (0xfffff0, 24),
    (0x1fffdf, 21),
    (0x3fffdf, 22),
    (0x7fffeb, 23),
    (0x7fffec, 23),
    (0x1fffe0, 21),
    (0x1fffe1, 21),
    (0x3fffe0, 22),
    (0x1fffe2, 21),
    (0x7fffed, 23),
    (0x3fffe1, 22),
    (0x7fffee, 23),
    (0x7fffef, 23),
    (0xfffea, 20),
    (0x3fffe2, 22),
    (0x3fffe3, 22),
    (0x3fffe4, 22),
    (0x7ffff0, 23),
    (0x3fffe5, 22),
    (0x3fffe6, 22),
    (0x7ffff1, 23),
    (0x3ffffe0, 26),
    (0x3ffffe1, 26),
    (0xfffeb, 20),
    (0x7fff1, 19),
    (0x3fffe7, 22),
    (0x7ffff2, 23),
    (0x3fffe8, 22),
    (0x1ffffec, 25),
    (0x3ffffe2, 26),
    (0x3ffffe3, 26),
    (0x3ffffe4, 26),
    (0x7ffffde, 27),
    (0x7ffffdf, 27),
    (0x3ffffe5, 26),
    (0xfffff1, 24),
    (0x1ffffed, 25),
    (0x7fff2, 19),
    (0x1fffe3, 21),
    (0x3ffffe6, 26),
    (0x7ffffe0, 27),
    (0x7ffffe1, 27),
    (0x3ffffe7, 26),
    (0x7ffffe2, 27),
    (0xfffff2, 24),
    (0x1fffe4, 21),
    (0x1fffe5, 21),
    (0x3ffffe8, 26),
    (0x3ffffe9, 26),
    (0xffffffd, 28),
    (0x7ffffe3, 27),
    (0x7ffffe4, 27),
    (0x7ffffe5, 27),
    (0xfffec, 20),
    (0xfffff3, 24),
    (0xfffed, 20),
    (0x1fffe6, 21),
    (0x3fffe9, 22),
    (0x1fffe7, 21),
    (0x1fffe8, 21),
    (0x7ffff3, 23),
    (0x3fffea, 22),
    (0x3fffeb, 22),
    (0x1ffffee, 25),
    (0x1ffffef, 25),
    (0xfffff4, 24),
    (0xfffff5, 24),
    (0x3ffffea, 26),
    (0x7ffff4, 23),
    (0x3ffffeb, 26),
    (0x7ffffe6, 27),
    (0x3ffffec, 26),
    (0x3ffffed, 26),
    (0x7ffffe7, 27),
    (0x7ffffe8, 27),
    (0x7ffffe9, 27),
    (0x7ffffea, 27),
    (0x7ffffeb, 27),
    (0xffffffe, 28),
    (0x7ffffec, 27),
    (0x7ffffed, 27),
    (0x7ffffee, 27),
    (0x7ffffef, 27),
    (0x7fffff0, 27),
    (0x3ffffee, 26),
    (0x3fffffff, 30),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7541 as committed with step 33's evidence, read without its page breaks.
    const RFC: &str =
        include_str!("../../../docs/research/s33-veil-creds/evidence/standards/rfc/rfc7541.txt");

    fn lines() -> Vec<&'static str> {
        RFC.lines()
            .filter(|l| {
                !l.starts_with("Peon & Ruellan") && !l.trim_start_matches('\x0c').starts_with("RFC 7541")
            })
            .collect()
    }

    /// The lines from `heading` to the next heading.
    fn section(heading: &str) -> Vec<&'static str> {
        let all = lines();
        let start = all.iter().position(|l| l.starts_with(heading)).unwrap();
        all[start + 1..]
            .iter()
            .take_while(|l| {
                !["C.", "Appendix", "Acknowledgments"]
                    .iter()
                    .any(|h| l.starts_with(h))
            })
            .copied()
            .collect()
    }

    type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

    /// An Appendix C example: its block, and the fields it decodes to.
    fn example(heading: &str) -> (Vec<u8>, Pairs) {
        let s = section(heading);
        let at = s
            .iter()
            .position(|l| l.trim() == "Hex dump of encoded data:")
            .unwrap();
        let mut block = Vec::new();
        for l in &s[at + 1..] {
            let t = l.trim();
            if t.starts_with("Decoding process:") {
                break;
            }
            let Some((groups, _)) = t.split_once('|') else {
                continue;
            };
            for g in groups.split_whitespace() {
                for pair in g.as_bytes().chunks(2) {
                    block.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
                }
            }
        }
        let at = s.iter().position(|l| l.trim() == "Decoded header list:").unwrap();
        let fields = s[at + 1..]
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| {
                // A pseudo-header field's name begins with its own colon.
                let (colon, rest) = l.strip_prefix(':').map_or(("", l), |r| (":", r));
                let (name, value) = rest.split_once(": ").unwrap();
                ([colon, name].concat().into_bytes(), value.as_bytes().to_vec())
            })
            .collect();
        (block, fields)
    }

    fn pairs(fields: &[Field]) -> Pairs {
        fields
            .iter()
            .map(|f| (f.name.clone(), f.value.to_vec()))
            .collect()
    }

    fn decoded(d: Result<Decoded, Error>) -> Vec<Field> {
        match d {
            Ok(Decoded::Fields(f)) => f,
            other => panic!("not decoded: {other:?}"),
        }
    }

    #[test]
    fn the_tables_are_rfc_7541s() {
        let a = section("Appendix A.  Static Table Definition");
        let rows: Pairs = a
            .iter()
            .filter_map(|l| {
                let cells: Vec<&str> = l.trim().split('|').map(str::trim).collect();
                (cells.len() == 5 && cells[1].parse::<usize>().is_ok())
                    .then(|| (cells[2].as_bytes().to_vec(), cells[3].as_bytes().to_vec()))
            })
            .collect();
        let ours: Pairs = STATIC.iter().map(|(n, v)| (n.to_vec(), v.to_vec())).collect();
        assert_eq!(rows, ours);
        let b = section("Appendix B.  Huffman Code");
        let codes: Vec<(usize, u32, u8)> = b
            .iter()
            .filter(|l| l.contains(")  |") && l.trim_end().ends_with(']'))
            .map(|l| {
                let (left, _) = l.split_once(")  |").unwrap();
                let symbol = left.rsplit_once('(').unwrap().1.trim().parse().unwrap();
                let (before, len) = l.rsplit_once('[').unwrap();
                let len = len.trim_end_matches(']').trim().parse().unwrap();
                let hex = u32::from_str_radix(before.split_whitespace().last().unwrap(), 16).unwrap();
                (symbol, hex, len)
            })
            .collect();
        assert_eq!(codes.len(), 257);
        for (symbol, code, len) in codes {
            assert_eq!(HUFFMAN[symbol], (code, len), "symbol {symbol}");
        }
    }

    #[test]
    fn integers_are_rfc_7541s_examples() {
        for (value, n, bytes) in [(10, 5, &[0x0a][..]), (1337, 5, &[0x1f, 0x9a, 0x0a][..])] {
            let mut out = Vec::new();
            put_integer(&mut out, 0, n, value);
            assert_eq!(out, bytes);
            assert_eq!(integer(bytes, &mut 0, n), Ok(value));
        }
    }

    #[test]
    fn rfc_7541s_examples_decode_and_encode_as_written() {
        // C.2: one block each, each into a table of its own; how each field was sent is kept.
        for (heading, indexing) in [
            ("C.2.1.", Indexing::Incremental),
            ("C.2.2.", Indexing::Without),
            ("C.2.3.", Indexing::Never),
            ("C.2.4.", Indexing::Incremental),
        ] {
            let (block, fields) = example(heading);
            let got = decoded(Decoder::new(4096).decode(&block, usize::MAX));
            assert_eq!(pairs(&got), fields, "{heading}");
            assert!(got.iter().all(|f| f.indexing == indexing), "{heading}");
        }
        // C.3 to C.6: three blocks each on one connection; C.5 and C.6 under a table of 256.
        for (section, limit, encoded) in [
            ("C.3", 4096, false),
            ("C.4", 4096, true),
            ("C.5", 256, false),
            ("C.6", 256, true),
        ] {
            let mut decoder = Decoder::new(limit);
            let mut encoder = Encoder::new(limit, limit);
            for n in 1..=3 {
                let (block, fields) = example(&format!("{section}.{n}."));
                let got = decoded(decoder.decode(&block, usize::MAX));
                assert_eq!(pairs(&got), fields, "{section}.{n}");
                if encoded {
                    // The examples with Huffman coding are what this encoder writes, byte for byte.
                    let mut out = Vec::new();
                    encoder.encode(&got, &mut out);
                    assert_eq!(out, block, "{section}.{n}");
                }
            }
        }
    }

    #[test]
    fn a_field_sent_never_indexed_stays_never_indexed_and_out_of_every_table() {
        let (block, _) = example("C.2.3.");
        let got = decoded(Decoder::new(4096).decode(&block, usize::MAX));
        let mut encoder = Encoder::new(4096, 4096);
        let (mut first, mut second) = (Vec::new(), Vec::new());
        encoder.encode(&got, &mut first);
        encoder.encode(&got, &mut second);
        assert_eq!(first[0] & 0xf0, 0x10, "a never-indexed literal");
        assert_eq!(first, second, "never added to the table, never sent as an index");
        assert_eq!(encoder.table.entries.len(), 0);
        // Its value written as its octets, not Huffman-coded.
        assert!(holds(&first, b"secret"));
    }

    fn holds(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn malformed_blocks_are_refused() {
        let refused = |block: &[u8], limit: usize| Decoder::new(limit).decode(block, usize::MAX).unwrap_err();
        assert_eq!(refused(&[0x80], 4096), Error::Index, "index zero");
        assert_eq!(refused(&[0xbe], 4096), Error::Index, "an index past the table");
        assert_eq!(refused(&[0x40, 0x85, b'a'], 4096), Error::Truncated);
        assert_eq!(refused(&[0x41], 4096), Error::Truncated);
        let mut long = vec![0xff];
        long.extend([0xff; 12]);
        assert_eq!(refused(&long, 4096), Error::Integer, "an integer past 64 bits");
        // "a" is 00011 (5 bits); EOS's 30 ones; padding of more than 7 bits; padding not ones.
        assert_eq!(
            refused(&[0x00, 0x81, 0x18, 0x01, b'v'], 4096),
            Error::Huffman,
            "padding of zeros"
        );
        assert_eq!(
            refused(&[0x00, 0x82, 0x1f, 0xff, 0x01, b'v'], 4096),
            Error::Huffman,
            "padding of 11 bits"
        );
        assert_eq!(
            refused(&[0x00, 0x84, 0xff, 0xff, 0xff, 0xfc, 0x01, b'v'], 4096),
            Error::Huffman,
            "EOS"
        );
        assert_eq!(
            refused(&[0x82, 0x3f, 0xe1, 0x1f], 4096),
            Error::SizeUpdate,
            "a size update after a field"
        );
        assert_eq!(
            refused(&[0x3f, 0xe2, 0x1f], 4096),
            Error::SizeUpdate,
            "a size update past the limit"
        );
        let mut lowered = Decoder::new(4096);
        lowered.set_limit(100);
        assert_eq!(
            lowered.decode(&[0x82], usize::MAX),
            Err(Error::SizeUpdate),
            "a lowered limit unsaid"
        );
        assert!(matches!(
            lowered.decode(&[0x3f, 0x45, 0x82], usize::MAX),
            Ok(Decoded::Fields(_))
        ));
    }

    #[test]
    fn a_block_past_its_bound_is_decoded_whole_and_its_fields_dropped() {
        let mut decoder = Decoder::new(4096);
        let mut block = Vec::new();
        Encoder::new(4096, 4096).encode(
            &[Field::new(b"x-big", &[b'v'; 200], Indexing::Incremental)],
            &mut block,
        );
        assert_eq!(decoder.decode(&block, 100), Ok(Decoded::TooLarge));
        // Its entry was added all the same: the next block's index 62 is it.
        let got = decoded(decoder.decode(&[0xbe], usize::MAX));
        assert_eq!(got[0].name, b"x-big");
    }

    #[test]
    fn a_fields_value_never_prints() {
        let field = Field::new(b"authorization", b"Bearer kiln-placeholder-0123", Indexing::Never);
        assert!(!format!("{field:?}").contains("kiln-placeholder"));
    }
}

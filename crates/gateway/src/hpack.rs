//! HPACK (RFC 7541) as an HTTP/2 client needs it. The decoder reads every representation
//! a peer may send: indexed fields, literals with incremental indexing, without indexing
//! and never indexed, dynamic table size updates at a block's start, and Huffman strings,
//! each bounded. The encoder writes literals without indexing (§6.2.2), which change no
//! table, the static table naming what it holds.

use std::collections::VecDeque;
use std::sync::OnceLock;

use crate::hpack_tables::{HUFFMAN, STATIC};

/// A header field: its name and value.
pub type Field = (Vec<u8>, Vec<u8>);

/// What a header block could not be read as: a COMPRESSION_ERROR (RFC 7540 §4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hpack: {}", self.0)
    }
}

fn bad<T>(why: &str) -> Result<T, Error> {
    Err(Error(why.to_string()))
}

/// What a field takes of a table (§4.1): its name, its value, and 32.
fn entry_size(name: &[u8], value: &[u8]) -> usize {
    name.len().saturating_add(value.len()).saturating_add(32)
}

/// A decoder of one direction's header blocks.
#[derive(Debug)]
pub struct Decoder {
    /// The dynamic table, its newest entry first (§2.3.3).
    table: VecDeque<Field>,
    size: usize,
    /// The table's size now, as the peer's last update set it.
    max: usize,
    /// The size this side allows (SETTINGS_HEADER_TABLE_SIZE): no update may pass it.
    allowed: usize,
    /// The most a block's fields may come to, each counted as a table counts it.
    list_max: usize,
}

impl Decoder {
    /// A decoder whose table may hold `table_size` bytes, and whose blocks may hold
    /// `list_max` bytes of fields.
    pub fn new(table_size: usize, list_max: usize) -> Decoder {
        Decoder {
            table: VecDeque::new(),
            size: 0,
            max: table_size,
            allowed: table_size,
            list_max,
        }
    }

    /// The fields of one whole header block.
    pub fn decode(&mut self, mut block: &[u8]) -> Result<Vec<Field>, Error> {
        let mut out = Vec::new();
        let mut listed = 0usize;
        let mut first = true;
        while let Some(&b) = block.first() {
            if b & 0x80 != 0 {
                let index = integer(&mut block, 7)?;
                let (name, value) = self.field(index)?;
                out.push((name.to_vec(), value.to_vec()));
            } else if b & 0xe0 == 0x20 {
                // §4.2: an update comes first in a block, or not at all.
                if !first {
                    return bad("a dynamic table size update after a field");
                }
                let size = integer(&mut block, 5)?;
                if size > self.allowed {
                    return bad("a dynamic table size update past the size allowed");
                }
                self.max = size;
                self.evict(0);
                continue;
            } else {
                let (prefix, indexing) = if b & 0xc0 == 0x40 { (6, true) } else { (4, false) };
                let index = integer(&mut block, prefix)?;
                let name = if index == 0 {
                    string(&mut block)?
                } else {
                    self.field(index)?.0.to_vec()
                };
                let value = string(&mut block)?;
                if indexing {
                    self.insert(name.clone(), value.clone());
                }
                out.push((name, value));
            }
            first = false;
            if let Some((name, value)) = out.last() {
                listed = listed.saturating_add(entry_size(name, value));
                if listed > self.list_max {
                    return bad("a header list larger than allowed");
                }
            }
        }
        Ok(out)
    }

    /// The field at `index`: the static table's from 1, then the dynamic table's.
    fn field(&self, index: usize) -> Result<(&[u8], &[u8]), Error> {
        if index == 0 {
            return bad("index 0");
        }
        if let Some((n, v)) = STATIC.get(index - 1) {
            return Ok((n.as_bytes(), v.as_bytes()));
        }
        match self.table.get(index - 1 - STATIC.len()) {
            Some((n, v)) => Ok((n, v)),
            None => bad("an index past the tables"),
        }
    }

    /// §4.4: a new entry, older ones evicted to make room; one larger than the table
    /// empties it.
    fn insert(&mut self, name: Vec<u8>, value: Vec<u8>) {
        let size = entry_size(&name, &value);
        if size > self.max {
            self.table.clear();
            self.size = 0;
            return;
        }
        self.evict(size);
        self.size += size;
        self.table.push_front((name, value));
    }

    /// Evicts the oldest entries until `room` more bytes fit.
    fn evict(&mut self, room: usize) {
        while self.size.saturating_add(room) > self.max {
            match self.table.pop_back() {
                Some((n, v)) => self.size = self.size.saturating_sub(entry_size(&n, &v)),
                None => {
                    self.size = 0;
                    break;
                }
            }
        }
    }
}

/// §5.1: an integer of an `n`-bit prefix, the rest of its first byte left as it was.
fn integer(block: &mut &[u8], n: u32) -> Result<usize, Error> {
    let Some((&first, rest)) = block.split_first() else {
        return bad("an integer past the block");
    };
    *block = rest;
    let mask = (1u32 << n) - 1;
    let mut value = (u32::from(first) & mask) as usize;
    if value < mask as usize {
        return Ok(value);
    }
    let mut shift = 0u32;
    loop {
        let Some((&b, rest)) = block.split_first() else {
            return bad("an integer past the block");
        };
        *block = rest;
        // Past 28 bits nothing a peer may send fits: refused, not wrapped.
        if shift > 21 {
            return bad("an integer too large");
        }
        value = value
            .checked_add(((b & 0x7f) as usize) << shift)
            .ok_or_else(|| Error("an integer too large".into()))?;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(value);
        }
    }
}

/// §5.2: a string literal, Huffman-coded or not.
fn string(block: &mut &[u8]) -> Result<Vec<u8>, Error> {
    let huffman = block.first().is_some_and(|b| b & 0x80 != 0);
    let len = integer(block, 7)?;
    let Some(raw) = block.get(..len) else {
        return bad("a string past the block");
    };
    *block = block.get(len..).unwrap_or_default();
    if huffman {
        huffman_decode(raw)
    } else {
        Ok(raw.to_vec())
    }
}

/// The code as a trie, built once: each node's two children, a leaf holding a symbol.
#[derive(Clone, Copy)]
enum Node {
    Inner([u16; 2]),
    Leaf(u16),
}

fn trie() -> &'static [Node] {
    static TRIE: OnceLock<Vec<Node>> = OnceLock::new();
    TRIE.get_or_init(|| {
        let mut nodes = vec![Node::Inner([0, 0])];
        for (symbol, &(code, len)) in HUFFMAN.iter().enumerate() {
            let mut at = 0usize;
            for i in (0..len).rev() {
                let bit = ((code >> i) & 1) as usize;
                let last = i == 0;
                let next = match nodes.get(at) {
                    Some(Node::Inner(children)) => children.get(bit).copied().unwrap_or(0),
                    _ => 0,
                };
                if next != 0 {
                    at = usize::from(next);
                    continue;
                }
                let made = u16::try_from(nodes.len()).unwrap_or(u16::MAX);
                nodes.push(if last {
                    Node::Leaf(u16::try_from(symbol).unwrap_or(u16::MAX))
                } else {
                    Node::Inner([0, 0])
                });
                if let Some(Node::Inner(children)) = nodes.get_mut(at)
                    && let Some(c) = children.get_mut(bit)
                {
                    *c = made;
                }
                at = usize::from(made);
            }
        }
        nodes
    })
}

/// §5.2: Huffman-coded bytes, the code's last partial byte padded with ones, fewer than
/// eight of them; EOS never coded.
pub fn huffman_decode(raw: &[u8]) -> Result<Vec<u8>, Error> {
    let trie = trie();
    let mut out = Vec::with_capacity(raw.len() * 8 / 5);
    let mut at = 0usize;
    // Bits read since the last symbol, and whether all were ones.
    let mut pending = 0u32;
    let mut ones = true;
    for &byte in raw {
        for i in (0..8).rev() {
            let bit = usize::from((byte >> i) & 1);
            pending += 1;
            ones &= bit == 1;
            let next = match trie.get(at) {
                Some(Node::Inner(children)) => children.get(bit).copied().unwrap_or(0),
                _ => 0,
            };
            if next == 0 {
                return bad("a Huffman code no symbol has");
            }
            match trie.get(usize::from(next)) {
                Some(Node::Leaf(256)) => return bad("EOS in a Huffman string"),
                Some(Node::Leaf(symbol)) => {
                    out.push(u8::try_from(*symbol).unwrap_or(0));
                    at = 0;
                    pending = 0;
                    ones = true;
                }
                Some(Node::Inner(_)) => at = usize::from(next),
                None => return bad("a Huffman code no symbol has"),
            }
        }
    }
    if pending > 7 || !ones {
        return bad("Huffman padding that is not EOS's first bits");
    }
    Ok(out)
}

/// §5.1: `value` in an `n`-bit prefix whose other bits are `flags`.
fn put_integer(out: &mut Vec<u8>, flags: u8, n: u32, mut value: usize) {
    let mask = (1usize << n) - 1;
    if value < mask {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | mask as u8);
    value -= mask;
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A header block of `fields`, each a literal without indexing (§6.2.2), its name the
/// static table's where it holds one, every string as it is.
pub fn encode(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(name, value) in fields {
        match STATIC.iter().position(|(n, _)| *n == name) {
            Some(i) => put_integer(&mut out, 0x00, 4, i + 1),
            None => {
                out.push(0x00);
                put_integer(&mut out, 0x00, 7, name.len());
                out.extend_from_slice(name.as_bytes());
            }
        }
        put_integer(&mut out, 0x00, 7, value.len());
        out.extend_from_slice(value.as_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s
            .chars()
            .filter_map(|c| c.to_digit(16))
            .map(|d| d as u8)
            .collect();
        digits.chunks(2).map(|p| p[0] << 4 | p[1]).collect()
    }

    fn fields(f: &[(&str, &str)]) -> Vec<Field> {
        f.iter()
            .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect()
    }

    /// RFC 7541 Appendix C.3 and C.4: three requests, without and with Huffman coding,
    /// each read by one decoder, the table as the RFC says after each.
    #[test]
    fn the_rfcs_requests_decode_as_it_says() {
        for blocks in [
            [
                "8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d",
                "8286 84be 5808 6e6f 2d63 6163 6865",
                "8287 85bf 400a 6375 7374 6f6d 2d6b 6579 0c63 7573 746f 6d2d 7661 6c75 65",
            ],
            [
                "8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff",
                "8286 84be 5886 a8eb 1064 9cbf",
                "8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf",
            ],
        ] {
            let mut d = Decoder::new(4096, 1 << 20);
            assert_eq!(
                d.decode(&hex(blocks[0])).unwrap(),
                fields(&[
                    (":method", "GET"),
                    (":scheme", "http"),
                    (":path", "/"),
                    (":authority", "www.example.com")
                ])
            );
            assert_eq!(d.size, 57);
            assert_eq!(
                d.decode(&hex(blocks[1])).unwrap(),
                fields(&[
                    (":method", "GET"),
                    (":scheme", "http"),
                    (":path", "/"),
                    (":authority", "www.example.com"),
                    ("cache-control", "no-cache")
                ])
            );
            assert_eq!(d.size, 110);
            assert_eq!(
                d.decode(&hex(blocks[2])).unwrap(),
                fields(&[
                    (":method", "GET"),
                    (":scheme", "https"),
                    (":path", "/index.html"),
                    (":authority", "www.example.com"),
                    ("custom-key", "custom-value")
                ])
            );
            assert_eq!(d.size, 164);
            assert_eq!(d.table[0], (b"custom-key".to_vec(), b"custom-value".to_vec()));
        }
    }

    /// RFC 7541 Appendix C.5: responses through a 256-byte table, entries evicted as
    /// the RFC evicts them.
    #[test]
    fn the_rfcs_responses_evict_as_it_says() {
        let mut d = Decoder::new(256, 1 << 20);
        d.decode(&hex(
            "4803 3330 3258 0770 7269 7661 7465 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133 \
             2032 303a 3133 3a32 3120 474d 546e 1768 7474 7073 3a2f 2f77 7777 2e65 7861 6d70 \
             6c65 2e63 6f6d",
        ))
        .unwrap();
        assert_eq!(d.size, 222);
        let second = d.decode(&hex("4803 3330 37c1 c0bf")).unwrap();
        assert_eq!(second[0], (b":status".to_vec(), b"307".to_vec()));
        assert_eq!(d.size, 222);
        assert_eq!(d.table.len(), 4);
        let third = d
            .decode(&hex(
                "88c1 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133 2032 303a 3133 3a32 3220 474d \
                 54c0 5a04 677a 6970 7738 666f 6f3d 4153 444a 4b48 514b 425a 584f 5157 454f 5049 \
                 5541 5851 5745 4f49 553b 206d 6178 2d61 6765 3d33 3630 303b 2076 6572 7369 6f6e \
                 3d31",
            ))
            .unwrap();
        assert_eq!(third[0], (b":status".to_vec(), b"200".to_vec()));
        assert_eq!(third.last().unwrap().0, b"set-cookie".to_vec());
        assert_eq!(d.size, 215);
        assert_eq!(d.table.len(), 3);
    }

    #[test]
    fn every_symbol_reads_back() {
        // Each byte, Huffman-coded by the table and padded with ones, decodes to itself.
        for symbol in 0..=255u8 {
            let (code, len) = HUFFMAN[usize::from(symbol)];
            let total = u32::from(len).div_ceil(8) * 8;
            let padded =
                (u64::from(code) << (total - u32::from(len))) | ((1u64 << (total - u32::from(len))) - 1);
            let bytes: Vec<u8> = (0..total / 8).rev().map(|i| (padded >> (i * 8)) as u8).collect();
            assert_eq!(huffman_decode(&bytes).unwrap(), vec![symbol], "{symbol}");
        }
    }

    #[test]
    fn what_rfc_7541_forbids_is_refused() {
        let mut d = Decoder::new(4096, 1 << 20);
        for (block, why) in [
            ("80", "index 0"),
            ("be", "an index past the tables"),
            ("8220", "a dynamic table size update after a field"),
            // 31 + 98 + (31 << 7) = 4097.
            ("3fe21f", "a dynamic table size update past the size allowed"),
            ("0f", "an integer past the block"),
            ("ffffffffffff7f", "an integer too large"),
            ("0005616263", "a string past the block"),
            // EOS's 30 ones, coded.
            ("0084ffffffff", "EOS in a Huffman string"),
            // "a" (00011) then 3 bits of zeros: padding that is not EOS's.
            ("00811800", "Huffman padding that is not EOS's first bits"),
            // Eight ones of padding after "a"'s five bits: a whole byte more than allowed.
            ("00821fff", "Huffman padding that is not EOS's first bits"),
        ] {
            assert_eq!(d.decode(&hex(block)), Err(Error(why.into())), "{block}");
        }
        let mut small = Decoder::new(4096, 40);
        assert_eq!(
            small.decode(&hex("8286")),
            Err(Error("a header list larger than allowed".into()))
        );
    }

    /// What this side writes reads back, through the static table where it names one.
    #[test]
    fn written_fields_read_back() {
        let fields = [
            (":method", "POST"),
            (":path", "/moby.buildkit.v1.frontend.LLBBridge/Ping"),
            ("te", "trailers"),
            ("content-type", "application/grpc"),
            ("x-long", &"y".repeat(300)),
        ];
        let block = encode(&fields);
        let read = Decoder::new(4096, 1 << 20).decode(&block).unwrap();
        assert_eq!(read.len(), fields.len());
        for ((n, v), (rn, rv)) in fields.iter().zip(&read) {
            assert_eq!((n.as_bytes(), v.as_bytes()), (rn.as_slice(), rv.as_slice()));
        }
        // `:method` by the static table's name (index 2), not a literal name.
        assert_eq!(&block[..2], &[0x02, 0x04]);
    }
}

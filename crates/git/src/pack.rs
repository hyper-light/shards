//! Packfiles (gitformat-pack.adoc): a header, each object's type and size and its zlib
//! stream, whole objects or deltas against another by its offset (`OBJ_OFS_DELTA`) or its
//! name (`OBJ_REF_DELTA`), and the SHA-1 of all of it at the end. Every object is named
//! from its resolved data, as index-pack names them, so that a pack can only claim
//! objects it holds.

use std::collections::HashMap;
use std::io::Read as _;

use crate::Oid;
use crate::object::Kind;

/// A pack's entry, as its header says.
#[derive(Debug, Clone, Copy)]
enum Base {
    Whole(Kind),
    /// A delta against the entry at that offset.
    Offset(usize),
    /// A delta against the object of that name.
    Name(Oid),
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    base: Base,
    /// Where its zlib stream starts.
    data: usize,
    /// Its data's size once inflated (a delta's: the delta's own).
    size: usize,
}

/// A pack's objects, by name.
#[derive(Debug)]
pub struct Pack {
    bytes: Vec<u8>,
    entries: HashMap<usize, Entry>,
    names: HashMap<Oid, usize>,
    /// The largest object it may hold, inflated.
    largest: usize,
}

/// How many deltas deep a chain may go: git's pack.depth defaults to 50 and allows at
/// most 4095 (builtin/pack-objects.c); past that a pack is refused.
const MAX_DEPTH: usize = 4095;

impl Pack {
    /// Reads `bytes`, a whole pack, naming every object; objects past `largest` bytes,
    /// inflated, are refused, as is a pack whose trailer is not its SHA-1.
    pub fn read(bytes: Vec<u8>, largest: usize) -> Result<Pack, String> {
        let body_len = bytes.len().checked_sub(20).ok_or("a pack cut short")?;
        let (body, trailer) = bytes.split_at(body_len);
        {
            use sha1_checked::Digest as _;
            let mut hasher = sha1_checked::Sha1::new();
            hasher.update(body);
            match hasher.try_finalize() {
                sha1_checked::CollisionResult::Ok(d) if d.as_slice() == trailer => {}
                sha1_checked::CollisionResult::Ok(_) => {
                    return Err("a pack whose checksum does not match".into());
                }
                _ => return Err("a pack shaped as a SHA-1 collision".into()),
            }
        }
        if body.get(..4) != Some(b"PACK") {
            return Err("not a pack".into());
        }
        let version = be32(body, 4)?;
        if version != 2 && version != 3 {
            return Err(format!("a pack of version {version}"));
        }
        let count = usize::try_from(be32(body, 8)?).map_err(|_| "a pack of too many objects")?;
        let mut entries = HashMap::new();
        let mut order = Vec::new();
        let mut at = 12usize;
        for _ in 0..count {
            let start = at;
            let first = *body.get(at).ok_or("a pack cut short")?;
            at += 1;
            let kind = (first >> 4) & 7;
            let mut size = usize::from(first & 0x0f);
            let mut shift = 4u32;
            let mut byte = first;
            while byte & 0x80 != 0 {
                byte = *body.get(at).ok_or("a pack cut short")?;
                at += 1;
                let bits = usize::from(byte & 0x7f)
                    .checked_shl(shift)
                    .filter(|_| shift < 57)
                    .ok_or("an object's size too large")?;
                size |= bits;
                shift += 7;
            }
            if size > largest {
                return Err(format!("an object of {size} bytes, past {largest}"));
            }
            let base = match kind {
                6 => {
                    let mut c = *body.get(at).ok_or("a pack cut short")?;
                    at += 1;
                    let mut off = usize::from(c & 0x7f);
                    while c & 0x80 != 0 {
                        c = *body.get(at).ok_or("a pack cut short")?;
                        at += 1;
                        off = off
                            .checked_add(1)
                            .and_then(|o| o.checked_mul(128))
                            .map(|o| o | usize::from(c & 0x7f))
                            .ok_or("a delta's offset too large")?;
                    }
                    Base::Offset(
                        start
                            .checked_sub(off)
                            .filter(|_| off > 0)
                            .ok_or("a delta before its pack")?,
                    )
                }
                7 => {
                    let name: [u8; 20] = body
                        .get(at..at + 20)
                        .and_then(|b| b.try_into().ok())
                        .ok_or("a pack cut short")?;
                    at += 20;
                    Base::Name(Oid(name))
                }
                n => Base::Whole(Kind::of_pack(n).ok_or_else(|| format!("an object of type {n}"))?),
            };
            let consumed = inflate(body.get(at..).unwrap_or_default(), size, None)?.1;
            entries.insert(start, Entry { base, data: at, size });
            order.push(start);
            at += consumed;
        }
        if at != body.len() {
            return Err("a pack with bytes past its objects".into());
        }
        let mut pack = Pack {
            bytes,
            entries,
            names: HashMap::new(),
            largest,
        };
        // Named until no more can be: a delta naming its base waits for that base's name,
        // which may be another delta's, later in the pack.
        let mut pending = order;
        loop {
            let before = pending.len();
            let mut failed = None;
            let mut waiting = Vec::new();
            for offset in pending {
                match pack.at(offset, 0) {
                    Ok((kind, data)) => {
                        let oid = Oid::of(kind, &data).ok_or("an object shaped as a SHA-1 collision")?;
                        pack.names.insert(oid, offset);
                    }
                    Err(e) => {
                        failed.get_or_insert(e);
                        waiting.push(offset);
                    }
                }
            }
            match failed {
                None => return Ok(pack),
                Some(e) if waiting.len() == before => return Err(e),
                Some(_) => pending = waiting,
            }
        }
    }

    /// The object named `oid`.
    pub fn get(&self, oid: &Oid) -> Result<(Kind, Vec<u8>), String> {
        let offset = *self
            .names
            .get(oid)
            .ok_or_else(|| format!("object {} is not in the pack", oid.hex()))?;
        self.at(offset, 0)
    }

    /// Whether the pack holds `oid`.
    pub fn has(&self, oid: &Oid) -> bool {
        self.names.contains_key(oid)
    }

    /// The pack as it came, its trailer included.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Each object's name and where its entry lies in the pack, from its header to the
    /// next's (an index's offset and CRC32 are of these bytes).
    pub fn spans(&self) -> Vec<(Oid, usize, usize)> {
        let mut starts: Vec<usize> = self.entries.keys().copied().collect();
        starts.sort_unstable();
        let end_of_all = self.bytes.len().saturating_sub(20);
        self.names
            .iter()
            .map(|(oid, &at)| {
                let next = starts.partition_point(|&s| s <= at);
                (*oid, at, starts.get(next).copied().unwrap_or(end_of_all))
            })
            .collect()
    }

    /// How many objects it holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The object at `offset`, its deltas applied.
    fn at(&self, offset: usize, depth: usize) -> Result<(Kind, Vec<u8>), String> {
        if depth > MAX_DEPTH {
            return Err("a delta chain too deep".into());
        }
        let entry = self
            .entries
            .get(&offset)
            .ok_or("a delta against no object of the pack")?;
        let body = self.bytes.get(entry.data..).unwrap_or_default();
        let (data, _) = inflate(body, entry.size, Some(entry.size))?;
        let base_offset = match entry.base {
            Base::Whole(kind) => return Ok((kind, data)),
            Base::Offset(o) => o,
            Base::Name(oid) => *self
                .names
                .get(&oid)
                .ok_or_else(|| format!("a delta against {}, which the pack does not hold", oid.hex()))?,
        };
        let (kind, base) = self.at(base_offset, depth + 1)?;
        Ok((kind, apply(&base, &data, self.largest)?))
    }
}

fn be32(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| "a pack cut short".to_string())
}

/// Inflates the zlib stream at the start of `input`, which must hold `size` bytes: its
/// data (kept when `keep` says so) and how many bytes of `input` it took.
fn inflate(input: &[u8], size: usize, keep: Option<usize>) -> Result<(Vec<u8>, usize), String> {
    let mut z = flate2::bufread::ZlibDecoder::new(input);
    let mut out = Vec::with_capacity(keep.unwrap_or(0).min(1 << 20));
    let mut limited = z
        .by_ref()
        .take(u64::try_from(size).unwrap_or(u64::MAX).saturating_add(1));
    let got = match keep {
        Some(_) => limited.read_to_end(&mut out).map(|n| n as u64),
        None => std::io::copy(&mut limited, &mut std::io::sink()),
    }
    .map_err(|e| format!("an object's data: {e}"))?;
    if usize::try_from(got).ok() != Some(size) {
        return Err(format!("an object of {got} bytes, where its header says {size}"));
    }
    // The stream must end with the object: its checksum read, nothing left over.
    let mut rest = [0u8; 1];
    if z.read(&mut rest).map_err(|e| format!("an object's data: {e}"))? != 0 {
        return Err("an object longer than its header says".into());
    }
    let consumed = usize::try_from(z.total_in()).map_err(|_| "an object too large")?;
    Ok((out, consumed))
}

/// `delta` applied to `base` (gitformat-pack.adoc, "Deltified representation").
pub fn apply(base: &[u8], delta: &[u8], largest: usize) -> Result<Vec<u8>, String> {
    let mut at = 0usize;
    let mut varint = || -> Result<usize, String> {
        let mut n = 0usize;
        let mut shift = 0u32;
        loop {
            let b = *delta.get(at).ok_or("a delta cut short")?;
            at += 1;
            n |= usize::from(b & 0x7f)
                .checked_shl(shift)
                .filter(|_| shift < 57)
                .ok_or("a delta's size too large")?;
            shift += 7;
            if b & 0x80 == 0 {
                return Ok(n);
            }
        }
    };
    let base_size = varint()?;
    let out_size = varint()?;
    if base_size != base.len() {
        return Err("a delta against a base of another size".into());
    }
    if out_size > largest {
        return Err(format!("an object of {out_size} bytes, past {largest}"));
    }
    let mut out = Vec::with_capacity(out_size);
    while let Some(&op) = delta.get(at) {
        at += 1;
        if op & 0x80 != 0 {
            let mut field = |bits: u8, bytes: u32| -> Result<usize, String> {
                let mut n = 0usize;
                for i in 0..bytes {
                    if bits & (1 << i) != 0 {
                        n |= usize::from(*delta.get(at).ok_or("a delta cut short")?) << (8 * i);
                        at += 1;
                    }
                }
                Ok(n)
            };
            let offset = field(op & 0x0f, 4)?;
            let size = match field((op >> 4) & 0x07, 3)? {
                0 => 0x10000,
                n => n,
            };
            let end = offset.checked_add(size).ok_or("a delta's copy past its base")?;
            out.extend_from_slice(base.get(offset..end).ok_or("a delta's copy past its base")?);
        } else if op != 0 {
            let n = usize::from(op);
            out.extend_from_slice(delta.get(at..at + n).ok_or("a delta cut short")?);
            at += n;
        } else {
            return Err("a delta's reserved instruction".into());
        }
        if out.len() > out_size {
            return Err("a delta past its result's size".into());
        }
    }
    if out.len() != out_size {
        return Err("a delta short of its result's size".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(data).unwrap();
        z.finish().unwrap()
    }

    fn header(kind: u8, mut size: usize) -> Vec<u8> {
        let mut out = vec![(kind << 4) | (size & 0x0f) as u8];
        size >>= 4;
        while size > 0 {
            *out.last_mut().unwrap() |= 0x80;
            out.push((size & 0x7f) as u8);
            size >>= 7;
        }
        out
    }

    fn finish(objects: u32, body: Vec<u8>) -> Vec<u8> {
        use sha1_checked::Digest as _;
        let mut pack = b"PACK".to_vec();
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&objects.to_be_bytes());
        pack.extend_from_slice(&body);
        let sum = sha1_checked::Sha1::digest(&pack);
        pack.extend_from_slice(&sum);
        pack
    }

    /// A pack of a blob, an offset delta against it and a name delta against that, as
    /// gitformat-pack.adoc lays them out; each object named from its resolved data.
    #[test]
    fn packs_are_read_with_their_deltas() {
        let base = b"hello, world\n".repeat(20);
        let mut body = Vec::new();
        let base_at = 12;
        body.extend(header(3, base.len()));
        body.extend(zlib(&base));
        // Copy the first 13 bytes, then insert "again\n".
        // Its base's 260 bytes as a varint, then the result's 19.
        let delta = [&[0x84, 0x02, 19, 0x90, 13, 6][..], b"again\n"].concat();
        let ofs_at = 12 + body.len();
        body.extend(header(6, delta.len()));
        body.push((ofs_at - base_at) as u8);
        body.extend(zlib(&delta));
        let second = b"hello, world\nagain\n";
        let second_oid = Oid::of(Kind::Blob, second).unwrap();
        // Copy all 19 bytes, then insert "!".
        let delta2 = [&[19u8, 20, 0x90, 19, 1][..], b"!"].concat();
        body.extend(header(7, delta2.len()));
        body.extend(second_oid.0);
        body.extend(zlib(&delta2));
        let pack = Pack::read(finish(3, body.clone()), 1 << 20).unwrap();
        assert_eq!(pack.len(), 3);
        assert_eq!(
            pack.get(&Oid::of(Kind::Blob, &base).unwrap()).unwrap(),
            (Kind::Blob, base.clone())
        );
        assert_eq!(pack.get(&second_oid).unwrap().1, second);
        let third = Oid::of(Kind::Blob, b"hello, world\nagain\n!").unwrap();
        assert_eq!(pack.get(&third).unwrap().1, b"hello, world\nagain\n!");

        // A trailer that is not the pack's SHA-1, and an object larger than allowed.
        let mut bad = finish(3, body.clone());
        *bad.last_mut().unwrap() ^= 1;
        assert!(Pack::read(bad, 1 << 20).is_err());
        assert!(Pack::read(finish(3, body), 100).is_err());
    }

    #[test]
    fn deltas_are_checked_against_their_bases() {
        assert_eq!(apply(b"abc", &[3, 3, 0x91, 0, 3], 99).unwrap(), b"abc");
        // A size 0 copy is 0x10000 bytes, past this base.
        assert!(apply(b"abc", &[3, 3, 0x80], 99).is_err());
        assert!(apply(b"abc", &[4, 3, 0x91, 0, 3], 99).is_err());
        assert!(apply(b"abc", &[3, 3, 0], 99).is_err());
        assert!(apply(b"abc", &[3, 4, 0x91, 0, 3], 99).is_err());
    }
}

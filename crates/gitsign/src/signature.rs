//! Signature packets (RFC 9580 §5.2) as go-crypto's Signature.parse reads them: versions
//! 4 and 6 (5 refused, as go-crypto is built without its `v5` tag), the algorithms and
//! hashes it takes, each subpacket it knows checked as it checks them, the hash suffix a
//! verification hashes, and the signature's values.

use crate::{Contents, Error};

/// A public-key algorithm go-crypto signs with (PublicKeyAlgorithm).
pub const RSA: u8 = 1;
pub const RSA_SIGN_ONLY: u8 = 3;
pub const DSA: u8 = 17;
pub const ECDSA: u8 = 19;
pub const EDDSA: u8 = 22;
pub const ED25519: u8 = 27;
pub const ED448: u8 = 28;

/// A hash go-crypto knows, by its OpenPGP id (algorithm.HashById, and SHA-1 for v4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha3_256,
    Sha3_512,
}

impl Hash {
    fn of(id: u8, sha1: bool) -> Option<Hash> {
        Some(match id {
            2 if sha1 => Hash::Sha1,
            8 => Hash::Sha256,
            9 => Hash::Sha384,
            10 => Hash::Sha512,
            11 => Hash::Sha224,
            12 => Hash::Sha3_256,
            14 => Hash::Sha3_512,
            _ => return None,
        })
    }

    pub fn id(self) -> u8 {
        match self {
            Hash::Sha1 => 2,
            Hash::Sha256 => 8,
            Hash::Sha384 => 9,
            Hash::Sha512 => 10,
            Hash::Sha224 => 11,
            Hash::Sha3_256 => 12,
            Hash::Sha3_512 => 14,
        }
    }

    /// SaltLengthForHash: a v6 signature's salt.
    fn salt_len(self) -> Result<usize, Error> {
        match self {
            Hash::Sha256 | Hash::Sha224 | Hash::Sha3_256 => Ok(16),
            Hash::Sha384 => Ok(24),
            Hash::Sha512 | Hash::Sha3_512 => Ok(32),
            Hash::Sha1 => Err(Error::Unsupported(
                "hash function not supported for V6 signatures".into(),
            )),
        }
    }
}

/// A notation (§5.2.3.24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notation {
    pub human_readable: bool,
    pub name: String,
    pub value: Vec<u8>,
    pub critical: bool,
}

/// The signature's values, by algorithm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Values {
    /// An MPI: its bit length as written, and its bytes.
    Rsa(Mpi),
    Dsa(Mpi, Mpi),
    Ecdsa(Mpi, Mpi),
    EdDsa(Mpi, Mpi),
    /// Ed25519's or Ed448's native signature.
    Native(Vec<u8>),
}

/// An MPI as encoding.MPI reads one: its declared bit length and the bytes it spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mpi {
    pub bits: u16,
    pub bytes: Vec<u8>,
}

/// Where a signature's fields are read from, in order: its packet's contents (the
/// outermost signature), or the bytes of the subpacket that embeds it, read in place.
pub(crate) trait Source {
    type Bytes: AsRef<[u8]>;
    /// `n` bytes, or UnexpectedEof where fewer are left.
    fn bytes(&mut self, n: usize) -> Result<Self::Bytes, Error>;
}

impl Source for Contents<'_, '_> {
    type Bytes = Vec<u8>;
    fn bytes(&mut self, n: usize) -> Result<Vec<u8>, Error> {
        self.read_full(n)
    }
}

/// An embedded signature's bytes, as the bytes.Buffer go-crypto reads them from: a read
/// past their end is UnexpectedEof, as the packet's own reader gives it.
struct Slice<'d> {
    data: &'d [u8],
    at: usize,
}

impl<'d> Source for Slice<'d> {
    type Bytes = &'d [u8];
    fn bytes(&mut self, n: usize) -> Result<&'d [u8], Error> {
        let rest = self.data.get(self.at..).unwrap_or_default();
        match rest.get(..n) {
            Some(b) => {
                self.at += n;
                Ok(b)
            }
            None => {
                self.at = self.data.len();
                Err(Error::UnexpectedEof)
            }
        }
    }
}

pub fn read_mpi(r: &mut Contents<'_, '_>) -> Result<Mpi, Error> {
    mpi_of(r)
}

fn mpi_of(r: &mut impl Source) -> Result<Mpi, Error> {
    let len = r.bytes(2)?;
    let len = len.as_ref();
    let bits = u16::from_be_bytes([
        len.first().copied().unwrap_or(0),
        len.get(1).copied().unwrap_or(0),
    ]);
    let bytes = r.bytes(usize::from(bits).div_ceil(8))?.as_ref().to_vec();
    Ok(Mpi { bits, bytes })
}

/// A signature packet, as parsed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Signature {
    pub version: u8,
    pub sig_type: u8,
    pub pubkey_algo: u8,
    pub hash: Option<Hash>,
    /// version, type, algorithms, hashed subpackets and the trailer, as hashed after the
    /// signed data.
    pub hash_suffix: Vec<u8>,
    pub hash_tag: [u8; 2],
    pub salt: Vec<u8>,
    pub creation_time: Option<u32>,
    pub sig_lifetime: Option<u32>,
    pub key_lifetime: Option<u32>,
    pub issuer_key_id: Option<u64>,
    pub issuer_fingerprint: Option<Vec<u8>>,
    pub flags_valid: bool,
    pub flags: u8,
    pub is_primary_id: Option<bool>,
    pub revocation_reason: Option<(u8, String)>,
    pub notations: Vec<Notation>,
    pub embedded: Option<Box<Signature>>,
    pub values: Option<Values>,
}

/// SigTypePrimaryKeyBinding.
const PRIMARY_KEY_BINDING: u8 = 0x19;

/// What a subpacket area holds next: the rest after one subpacket, or an embedded
/// signature's bytes and the rest after them.
enum Next<'d> {
    Rest(&'d [u8]),
    Embedded(&'d [u8], &'d [u8]),
}

fn multiple_embedded() -> Error {
    Error::Structural("Cannot have multiple embedded signatures".into())
}

/// An embedded signature, a cross-signature, is a primary key's binding.
fn cross_signature(sig: &Signature) -> Result<(), Error> {
    if sig.sig_type != PRIMARY_KEY_BINDING {
        return Err(Error::Structural(format!(
            "cross-signature has unexpected type {}",
            sig.sig_type
        )));
    }
    Ok(())
}

/// An embedded signature, read from `root` (go-crypto's bytes.Buffer over the
/// subpacket), with the signatures it embeds in turn, however deep they go: read with a
/// stack of frames of its own, where go-crypto recurses on a stack that grows to 1 GiB,
/// and each in place, where go-crypto copies each level's areas, so the read is linear
/// in the input. Only the first is kept: a cross-signature's own embedded signature is
/// checked as it is read, and nothing verifies it, so no chain is kept to clone or drop.
fn parse_embedded(root: &[u8]) -> Result<Signature, Error> {
    /// A signature being read: its fields so far, its bytes, what is left of the area
    /// it is in and which, and whether it embedded one already.
    struct Frame<'d> {
        sig: Signature,
        src: Slice<'d>,
        area: &'d [u8],
        hashed: bool,
        embeds: bool,
    }
    fn start(data: &[u8], kept: bool) -> Result<Frame<'_>, Error> {
        let mut src = Slice { data, at: 0 };
        let (mut sig, hashed) = Signature::begin(&mut src)?;
        if kept {
            sig.build_hash_suffix(hashed);
        }
        Ok(Frame {
            sig,
            src,
            area: hashed,
            hashed: true,
            embeds: false,
        })
    }
    let mut stack = vec![start(root, true)?];
    loop {
        let Some(f) = stack.last_mut() else {
            return Err(Error::Structural("embedded signature read past its end".into()));
        };
        // The frame on top read until it embeds one, or ends.
        let child = loop {
            if f.area.is_empty() {
                f.sig.require_creation_time()?;
                if f.hashed {
                    f.area = f.sig.unhashed(&mut f.src)?;
                    f.hashed = false;
                    continue;
                }
                f.sig.finish(&mut f.src)?;
                break None;
            }
            match f.sig.subpacket(f.area, f.hashed)? {
                Next::Rest(rest) => f.area = rest,
                Next::Embedded(sub, rest) => {
                    if f.embeds {
                        return Err(multiple_embedded());
                    }
                    f.area = rest;
                    break Some(sub);
                }
            }
        };
        if let Some(sub) = child {
            stack.push(start(sub, false)?);
            continue;
        }
        let Some(done) = stack.pop() else {
            return Err(Error::Structural("embedded signature read past its end".into()));
        };
        let Some(parent) = stack.last_mut() else {
            return Ok(done.sig);
        };
        cross_signature(&done.sig)?;
        parent.embeds = true;
    }
}

impl Signature {
    /// Signature.parse.
    pub fn parse(r: &mut Contents<'_, '_>) -> Result<Signature, Error> {
        let (mut sig, hashed) = Signature::begin(r)?;
        sig.build_hash_suffix(&hashed);
        sig.area(&hashed, true)?;
        let unhashed = sig.unhashed(r)?;
        sig.area(&unhashed, false)?;
        sig.finish(r)?;
        Ok(sig)
    }

    /// The version, the head and the hashed area.
    fn begin<S: Source>(r: &mut S) -> Result<(Signature, S::Bytes), Error> {
        let mut sig = Signature::default();
        let first = r.bytes(1)?;
        sig.version = first.as_ref().first().copied().unwrap_or(0);
        match sig.version {
            4 | 6 => {}
            5 => {
                return Err(Error::Unsupported(
                    "support for parsing v5 entities is disabled; build with `-tags v5` if needed".into(),
                ));
            }
            v => return Err(Error::Unsupported(format!("signature packet version {v}"))),
        }
        let head = r.bytes(if sig.version == 6 { 7 } else { 5 })?;
        let head = head.as_ref();
        let at = |i: usize| head.get(i).copied().unwrap_or(0);
        sig.sig_type = at(0);
        sig.pubkey_algo = at(1);
        if ![RSA, RSA_SIGN_ONLY, DSA, ECDSA, EDDSA, ED25519, ED448].contains(&sig.pubkey_algo) {
            return Err(Error::Unsupported(format!(
                "public key algorithm {}",
                sig.pubkey_algo
            )));
        }
        let hash = Hash::of(at(2), sig.version < 5)
            .ok_or_else(|| Error::Unsupported(format!("hash function {}", at(2))))?;
        sig.hash = Some(hash);
        let hashed_len = if sig.version == 6 {
            u32::from_be_bytes([at(3), at(4), at(5), at(6)]) as usize
        } else {
            usize::from(u16::from_be_bytes([at(3), at(4)]))
        };
        let hashed = r.bytes(hashed_len)?;
        Ok((sig, hashed))
    }

    /// The unhashed area.
    fn unhashed<S: Source>(&self, r: &mut S) -> Result<S::Bytes, Error> {
        let len = if self.version == 6 {
            let b = r.bytes(4)?;
            let b = b.as_ref();
            u32::from_be_bytes([
                b.first().copied().unwrap_or(0),
                b.get(1).copied().unwrap_or(0),
                b.get(2).copied().unwrap_or(0),
                b.get(3).copied().unwrap_or(0),
            ]) as usize
        } else {
            let b = r.bytes(2)?;
            let b = b.as_ref();
            usize::from(u16::from_be_bytes([
                b.first().copied().unwrap_or(0),
                b.get(1).copied().unwrap_or(0),
            ]))
        };
        r.bytes(len)
    }

    /// The hash tag, a v6 signature's salt, and the signature's values.
    fn finish<S: Source>(&mut self, r: &mut S) -> Result<(), Error> {
        let tag = r.bytes(2)?;
        let tag = tag.as_ref();
        self.hash_tag = [
            tag.first().copied().unwrap_or(0),
            tag.get(1).copied().unwrap_or(0),
        ];
        if self.version == 6 {
            let n = r.bytes(1)?;
            let expected = self.hash.map_or(Ok(0), Hash::salt_len)?;
            if usize::from(n.as_ref().first().copied().unwrap_or(0)) != expected {
                return Err(Error::Structural(
                    "unexpected salt size for the given hash algorithm".into(),
                ));
            }
            self.salt = r.bytes(expected)?.as_ref().to_vec();
        }
        self.values = Some(match self.pubkey_algo {
            RSA | RSA_SIGN_ONLY => Values::Rsa(mpi_of(r)?),
            DSA => Values::Dsa(mpi_of(r)?, mpi_of(r)?),
            ECDSA => Values::Ecdsa(mpi_of(r)?, mpi_of(r)?),
            EDDSA => Values::EdDsa(mpi_of(r)?, mpi_of(r)?),
            ED25519 => Values::Native(r.bytes(64)?.as_ref().to_vec()),
            _ => Values::Native(r.bytes(114)?.as_ref().to_vec()),
        });
        Ok(())
    }

    /// The outermost signature's area (parseSignatureSubpackets): each subpacket, an
    /// embedded signature read whole (parse_embedded), then a creation time required.
    fn area(&mut self, mut data: &[u8], hashed: bool) -> Result<(), Error> {
        while !data.is_empty() {
            data = match self.subpacket(data, hashed)? {
                Next::Rest(rest) => rest,
                Next::Embedded(sub, rest) => {
                    if self.embedded.is_some() {
                        return Err(multiple_embedded());
                    }
                    let embedded = parse_embedded(sub)?;
                    cross_signature(&embedded)?;
                    self.embedded = Some(Box::new(embedded));
                    rest
                }
            };
        }
        self.require_creation_time()
    }

    fn require_creation_time(&self) -> Result<(), Error> {
        if self.creation_time.is_none() {
            return Err(Error::Structural("no creation time in signature".into()));
        }
        Ok(())
    }

    /// buildHashSuffix.
    fn build_hash_suffix(&mut self, hashed: &[u8]) {
        let id = self.hash.map_or(0, Hash::id);
        let mut out = vec![self.version, self.sig_type, self.pubkey_algo, id];
        let len = hashed.len();
        if self.version == 6 {
            out.extend_from_slice(&(len as u32).to_be_bytes());
        } else {
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        let prefix = out.len();
        out.extend_from_slice(hashed);
        let l = (prefix + hashed.len()) as u64;
        out.push(self.version);
        out.push(0xff);
        out.extend_from_slice(&(l as u32).to_be_bytes());
        self.hash_suffix = out;
    }

    /// parseSignatureSubpacket: one subpacket, and what follows it; an embedded
    /// signature's bytes, for the caller to read.
    fn subpacket<'d>(&mut self, data: &'d [u8], hashed: bool) -> Result<Next<'d>, Error> {
        let truncated = || Error::Structural("signature subpacket truncated".into());
        let structural = |s: &str| Error::Structural(s.into());
        let b = |i: usize| data.get(i).copied().unwrap_or(0);
        let (length, rest) = match b(0) {
            0..=191 => (usize::from(b(0)), data.get(1..).unwrap_or_default()),
            192..=254 => {
                if data.len() < 2 {
                    return Err(truncated());
                }
                (
                    (usize::from(b(0) - 192) << 8) + usize::from(b(1)) + 192,
                    data.get(2..).unwrap_or_default(),
                )
            }
            255 => {
                if data.len() < 5 {
                    return Err(truncated());
                }
                (
                    u32::from_be_bytes([b(1), b(2), b(3), b(4)]) as usize,
                    data.get(5..).unwrap_or_default(),
                )
            }
        };
        if length > rest.len() {
            return Err(truncated());
        }
        let (sub, after) = rest.split_at(length);
        let Some((&kind_byte, sub)) = sub.split_first() else {
            return Err(structural("zero length signature subpacket"));
        };
        let kind = kind_byte & 0x7f;
        let critical = kind_byte & 0x80 != 0;
        if !hashed && kind != 16 && kind != 33 && kind != 32 {
            return Ok(Next::Rest(after));
        }
        let u32_of = |s: &[u8]| {
            u32::from_be_bytes([
                s.first().copied().unwrap_or(0),
                s.get(1).copied().unwrap_or(0),
                s.get(2).copied().unwrap_or(0),
                s.get(3).copied().unwrap_or(0),
            ])
        };
        let u64_of = |s: &[u8]| {
            let mut v = 0u64;
            for x in s.iter().take(8) {
                v = (v << 8) | u64::from(*x);
            }
            v
        };
        match kind {
            2 => {
                if sub.len() != 4 {
                    return Err(structural("signature creation time not four bytes"));
                }
                self.creation_time = Some(u32_of(sub));
            }
            3 => {
                if sub.len() != 4 {
                    return Err(structural("expiration subpacket with bad length"));
                }
                self.sig_lifetime = Some(u32_of(sub));
            }
            4 => {
                if sub.first() == Some(&0) {
                    return Err(Error::Unsupported(
                        "signature with non-exportable certification".into(),
                    ));
                }
            }
            5 => {
                if sub.len() != 2 {
                    return Err(structural("trust subpacket with bad length"));
                }
            }
            6 => {
                if sub.is_empty() {
                    return Err(structural("regexp subpacket with bad length"));
                }
                if sub.last() != Some(&0) {
                    return Err(structural("expected regular expression to be null-terminated"));
                }
            }
            9 => {
                if sub.len() != 4 {
                    return Err(structural("key expiration subpacket with bad length"));
                }
                self.key_lifetime = Some(u32_of(sub));
            }
            16 => {
                if self.version > 4 && hashed {
                    return Err(structural("issuer subpacket found in v6 key"));
                }
                if sub.len() != 8 {
                    return Err(structural("issuer subpacket with bad length"));
                }
                if self.version <= 4 {
                    self.issuer_key_id = Some(u64_of(sub));
                }
            }
            20 => {
                if sub.len() < 8 {
                    return Err(structural("notation data subpacket with bad length"));
                }
                let name_len = usize::from(u16::from_be_bytes([
                    sub.get(4).copied().unwrap_or(0),
                    sub.get(5).copied().unwrap_or(0),
                ]));
                let value_len = usize::from(u16::from_be_bytes([
                    sub.get(6).copied().unwrap_or(0),
                    sub.get(7).copied().unwrap_or(0),
                ]));
                if sub.len() != name_len + value_len + 8 {
                    return Err(structural("notation data subpacket with bad length"));
                }
                self.notations.push(Notation {
                    human_readable: sub.first().is_some_and(|f| f & 0x80 != 0),
                    name: String::from_utf8_lossy(sub.get(8..8 + name_len).unwrap_or_default()).into_owned(),
                    value: sub.get(8 + name_len..).unwrap_or_default().to_vec(),
                    critical,
                });
            }
            25 => {
                if sub.len() != 1 {
                    return Err(structural("primary user id subpacket with bad length"));
                }
                self.is_primary_id = Some(sub.first().is_some_and(|&x| x > 0));
            }
            27 => {
                self.flags_valid = true;
                if let Some(&f) = sub.first() {
                    self.flags = f;
                }
            }
            29 => {
                let Some((&code, text)) = sub.split_first() else {
                    return Err(structural("empty revocation reason subpacket"));
                };
                self.revocation_reason = Some((code, String::from_utf8_lossy(text).into_owned()));
            }
            // Read by the caller, with the signatures it embeds (parse_embedded).
            32 => return Ok(Next::Embedded(sub, after)),
            33 => {
                let Some((&v, fp)) = sub.split_first() else {
                    return Err(structural("empty issuer fingerprint subpacket"));
                };
                if (v >= 5 && fp.len() != 32) || (v < 5 && fp.len() != 20) {
                    return Err(structural("bad fingerprint length"));
                }
                self.issuer_fingerprint = Some(fp.to_vec());
                self.issuer_key_id = Some(if v >= 5 {
                    u64_of(fp.get(..8).unwrap_or_default())
                } else {
                    u64_of(fp.get(12..20).unwrap_or_default())
                });
            }
            35 => {
                let Some((&v, fp)) = sub.split_first() else {
                    return Err(structural("invalid intended recipient fingerpring length"));
                };
                if (v >= 5 && fp.len() != 32) || (v < 5 && fp.len() != 20) {
                    return Err(structural("invalid fingerprint length"));
                }
            }
            39 => {
                if sub.len() % 2 != 0 {
                    return Err(structural("invalid aead cipher suite length"));
                }
            }
            11 | 21 | 22 | 23 | 24 | 26 | 28 | 30 => {}
            _ => {
                if critical {
                    return Err(Error::Unsupported(format!(
                        "unknown critical signature subpacket type {kind}"
                    )));
                }
            }
        }
        Ok(Next::Rest(after))
    }
}

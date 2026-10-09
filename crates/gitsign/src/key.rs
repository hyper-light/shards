//! Public-key packets (RFC 9580 §5.5.2) as go-crypto's PublicKey.parse reads them: v4
//! and v6 (v5 refused, as go-crypto is built without its `v5` tag), each algorithm's
//! fields and checks, the key re-serialized for hashing as go-crypto serializes it, and
//! its fingerprint and key ID. Secret-key packets as PrivateKey.parse reads their form
//! (§5.5.3), keeping only the public key they carry (D103).

use zeroize::Zeroizing;

use crate::signature::Mpi;
use crate::{Contents, Error};

/// A curve go-crypto knows, by its OID (internal/ecc curve_info.go).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    P256,
    P384,
    P521,
    Secp256k1,
    Curve25519,
    Curve448,
    Ed25519Legacy,
    Ed448Legacy,
    BrainpoolP256,
    BrainpoolP384,
    BrainpoolP512,
}

impl Curve {
    fn of(oid: &[u8]) -> Option<Curve> {
        Some(match oid {
            [0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07] => Curve::P256,
            [0x2B, 0x81, 0x04, 0x00, 0x22] => Curve::P384,
            [0x2B, 0x81, 0x04, 0x00, 0x23] => Curve::P521,
            [0x2B, 0x81, 0x04, 0x00, 0x0A] => Curve::Secp256k1,
            [0x2B, 0x06, 0x01, 0x04, 0x01, 0x97, 0x55, 0x01, 0x05, 0x01] => Curve::Curve25519,
            [0x2B, 0x65, 0x6F] => Curve::Curve448,
            [0x2B, 0x06, 0x01, 0x04, 0x01, 0xDA, 0x47, 0x0F, 0x01] => Curve::Ed25519Legacy,
            [0x2B, 0x65, 0x71] => Curve::Ed448Legacy,
            [0x2B, 0x24, 0x03, 0x03, 0x02, 0x08, 0x01, 0x01, 0x07] => Curve::BrainpoolP256,
            [0x2B, 0x24, 0x03, 0x03, 0x02, 0x08, 0x01, 0x01, 0x0B] => Curve::BrainpoolP384,
            [0x2B, 0x24, 0x03, 0x03, 0x02, 0x08, 0x01, 0x01, 0x0D] => Curve::BrainpoolP512,
            _ => return None,
        })
    }

    /// A Weierstrass curve's (ECDSA's and ECDH's generic curves).
    fn generic(self) -> bool {
        matches!(
            self,
            Curve::P256
                | Curve::P384
                | Curve::P521
                | Curve::Secp256k1
                | Curve::BrainpoolP256
                | Curve::BrainpoolP384
                | Curve::BrainpoolP512
        )
    }
}

/// A public key's own fields, by algorithm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Material {
    Rsa {
        n: Mpi,
        e: Mpi,
    },
    Dsa {
        p: Mpi,
        q: Mpi,
        g: Mpi,
        y: Mpi,
    },
    ElGamal {
        p: Mpi,
        g: Mpi,
        y: Mpi,
    },
    Ecdsa {
        curve: Curve,
        oid: Vec<u8>,
        point: Mpi,
    },
    Ecdh {
        curve: Curve,
        oid: Vec<u8>,
        point: Mpi,
        kdf: Vec<u8>,
    },
    EdDsa {
        curve: Curve,
        oid: Vec<u8>,
        point: Mpi,
    },
    /// X25519's, X448's, Ed25519's or Ed448's native point.
    Native(Vec<u8>),
}

/// A public key packet, as parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    pub version: u8,
    pub created: u32,
    pub algo: u8,
    pub material: Material,
    pub fingerprint: Vec<u8>,
    pub key_id: u64,
    pub is_subkey: bool,
    /// Read from a secret-key packet.
    pub secret: bool,
}

pub const RSA: u8 = 1;
pub const RSA_ENCRYPT_ONLY: u8 = 2;
pub const RSA_SIGN_ONLY: u8 = 3;
pub const ELGAMAL: u8 = 16;
pub const DSA: u8 = 17;
pub const ECDH: u8 = 18;
pub const ECDSA: u8 = 19;
pub const EDDSA: u8 = 22;
pub const X25519: u8 = 25;
pub const X448: u8 = 26;
pub const ED25519: u8 = 27;
pub const ED448: u8 = 28;

fn mpi(r: &mut Contents<'_, '_>) -> Result<Mpi, Error> {
    crate::signature::read_mpi(r)
}

/// encoding.MPI's ReadFrom over a secret key's octets: its length and its octets, a short
/// read UnexpectedEof.
fn secret_mpi(data: &mut &[u8]) -> Result<(), Error> {
    let (len, rest) = data.split_first_chunk::<2>().ok_or(Error::UnexpectedEof)?;
    let n = usize::from(u16::from_be_bytes(*len)).div_ceil(8);
    *data = rest.get(n..).ok_or(Error::UnexpectedEof)?;
    Ok(())
}

/// OpaqueSubpackets of a user attribute: each subpacket's length within the packet.
pub fn user_attribute(mut b: &[u8]) -> Result<(), Error> {
    let truncated = || Error::Structural("subpacket truncated".into());
    while let Some(&first) = b.first() {
        let (header, len) = match first {
            0..=191 => (1usize, u32::from(first)),
            192..=254 => {
                let second = *b.get(1).ok_or_else(truncated)?;
                if b.len() < 3 {
                    return Err(truncated());
                }
                (2, (u32::from(first - 192) << 8) + u32::from(second) + 192)
            }
            255 => {
                let n = b.get(1..5).ok_or_else(truncated)?;
                if b.len() < 6 {
                    return Err(truncated());
                }
                (
                    5,
                    u32::from_be_bytes([
                        n.first().copied().unwrap_or(0),
                        n.get(1).copied().unwrap_or(0),
                        n.get(2).copied().unwrap_or(0),
                        n.get(3).copied().unwrap_or(0),
                    ]),
                )
            }
        };
        if b.len() < header + 1 {
            return Err(truncated());
        }
        let rest = b.get(header..).unwrap_or_default();
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len > rest.len() || len == 0 {
            return Err(truncated());
        }
        b = rest.get(len..).unwrap_or_default();
    }
    Ok(())
}

/// encoding.OID's ReadFrom: a length octet, neither 0 nor 0xff, then that many octets.
fn oid(r: &mut Contents<'_, '_>) -> Result<Vec<u8>, Error> {
    let n = r.read_full(1)?;
    let n = n.first().copied().unwrap_or(0);
    if n == 0 || n == 0xff {
        return Err(Error::Unsupported("reserved for future extensions".into()));
    }
    r.read_full(usize::from(n))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl PublicKey {
    /// PublicKey.parse.
    pub fn parse(r: &mut Contents<'_, '_>, is_subkey: bool) -> Result<PublicKey, Error> {
        let head = r.read_full(6)?;
        let at = |i: usize| head.get(i).copied().unwrap_or(0);
        let version = at(0);
        match version {
            4 | 6 => {}
            5 => {
                return Err(Error::Unsupported(
                    "support for parsing v5 entities is disabled; build with `-tags v5` if needed".into(),
                ));
            }
            v => return Err(Error::Unsupported(format!("public key version {v}"))),
        }
        if version >= 5 {
            r.read_full(4)?;
        }
        let created = u32::from_be_bytes([at(1), at(2), at(3), at(4)]);
        let algo = at(5);
        let material = match algo {
            RSA | RSA_ENCRYPT_ONLY | RSA_SIGN_ONLY => {
                let n = mpi(r)?;
                let e = mpi(r)?;
                if e.bytes.len() > 3 {
                    return Err(Error::Unsupported("large public exponent".into()));
                }
                Material::Rsa { n, e }
            }
            DSA => Material::Dsa {
                p: mpi(r)?,
                q: mpi(r)?,
                g: mpi(r)?,
                y: mpi(r)?,
            },
            ELGAMAL => Material::ElGamal {
                p: mpi(r)?,
                g: mpi(r)?,
                y: mpi(r)?,
            },
            ECDSA => {
                let oid = oid(r)?;
                let curve = Curve::of(&oid)
                    .ok_or_else(|| Error::Unsupported(format!("unknown oid: {}", hex(&oid))))?;
                let point = mpi(r)?;
                if !curve.generic() {
                    return Err(Error::Unsupported(format!("unsupported oid: {}", hex(&oid))));
                }
                if !crate::arith::on_curve(curve, &point.bytes) {
                    return Err(Error::Other("ecdsa: failed to parse EC point".into()));
                }
                Material::Ecdsa { curve, oid, point }
            }
            ECDH => {
                let oid = oid(r)?;
                let curve = Curve::of(&oid)
                    .ok_or_else(|| Error::Unsupported(format!("unknown oid: {}", hex(&oid))))?;
                if version == 6 && curve == Curve::Curve25519 {
                    return Err(Error::Structural(
                        "cannot read v6 key with deprecated OID: Curve25519Legacy".into(),
                    ));
                }
                let point = mpi(r)?;
                let kdf = oid_bytes(r)?;
                if !(curve.generic() || matches!(curve, Curve::Curve25519 | Curve::Curve448)) {
                    return Err(Error::Unsupported(format!("unsupported oid: {}", hex(&oid))));
                }
                if kdf.len() < 3 {
                    return Err(Error::Unsupported(format!(
                        "unsupported ECDH KDF length: {}",
                        kdf.len()
                    )));
                }
                let k = |i: usize| kdf.get(i).copied().unwrap_or(0);
                if k(0) != 0x01 {
                    return Err(Error::Unsupported(format!(
                        "unsupported KDF reserved field: {}",
                        k(0)
                    )));
                }
                if !matches!(k(1), 8 | 9 | 10 | 11 | 12 | 14) {
                    return Err(Error::Unsupported(format!("unsupported ECDH KDF hash: {}", k(1))));
                }
                // algorithm.CipherById: TripleDES, CAST5, AES-128/192/256.
                if !matches!(k(2), 2 | 3 | 7 | 8 | 9) {
                    return Err(Error::Unsupported(format!(
                        "unsupported ECDH KDF cipher: {}",
                        k(2)
                    )));
                }
                // UnmarshalBytePoint: a generic curve's point unchecked; X25519's and X448's
                // prefixed and of their size.
                let ok = match curve {
                    Curve::Curve25519 => point.bytes.len() == 33,
                    Curve::Curve448 => point.bytes.len() == 57,
                    _ => true,
                };
                if !ok {
                    return Err(Error::Other("ecdh: failed to parse EC point".into()));
                }
                Material::Ecdh {
                    curve,
                    oid,
                    point,
                    kdf,
                }
            }
            EDDSA => {
                if version == 6 {
                    return Err(Error::Structural(
                        "cannot generate v6 key with deprecated algorithm: EdDSALegacy".into(),
                    ));
                }
                let oid = oid(r)?;
                let curve = Curve::of(&oid)
                    .ok_or_else(|| Error::Unsupported(format!("unknown oid: {}", hex(&oid))))?;
                if !matches!(curve, Curve::Ed25519Legacy | Curve::Ed448Legacy) {
                    return Err(Error::Unsupported(format!("unsupported oid: {}", hex(&oid))));
                }
                let point = mpi(r)?;
                let Some(&flag) = point.bytes.first() else {
                    return Err(Error::Structural("empty EdDSA public key".into()));
                };
                if flag != 0x40 {
                    return Err(Error::Unsupported(format!(
                        "unsupported EdDSA compression: {flag}"
                    )));
                }
                let size = if curve == Curve::Ed25519Legacy { 33 } else { 58 };
                if point.bytes.len() != size {
                    return Err(Error::Other("eddsa: failed to parse EC point".into()));
                }
                Material::EdDsa { curve, oid, point }
            }
            X25519 => Material::Native(r.read_full(32)?),
            X448 => Material::Native(r.read_full(56)?),
            ED25519 => Material::Native(r.read_full(32)?),
            ED448 => Material::Native(r.read_full(57)?),
            a => return Err(Error::Unsupported(format!("public key type: {a}"))),
        };
        let mut pk = PublicKey {
            version,
            created,
            algo,
            material,
            fingerprint: Vec::new(),
            key_id: 0,
            is_subkey,
            secret: false,
        };
        pk.set_fingerprint();
        Ok(pk)
    }

    /// PrivateKey.parse: the public key, then the secret part's form: its S2K usage, its
    /// cipher, AEAD mode and S2K parameters where encrypted, its IV, and where not
    /// encrypted its checksum (v4) and each algorithm's secret fields. The secret octets
    /// are zeroed once read; nothing is computed with them (D103). A GNU dummy key ends
    /// the parse where go-crypto ends it, the rest of its packet unread.
    pub fn parse_secret(r: &mut Contents<'_, '_>, is_subkey: bool) -> Result<PublicKey, Error> {
        let mut pk = PublicKey::parse(r, is_subkey)?;
        pk.secret = true;
        let v6 = pk.version == 6;
        let byte = |r: &mut Contents<'_, '_>| -> Result<u8, Error> {
            Ok(r.read_full(1)?.first().copied().unwrap_or(0))
        };
        let s2k_type = byte(r)?;
        if v6 && s2k_type != 0 {
            byte(r)?;
        }
        let mut encrypted = false;
        let mut iv_size = 0;
        match s2k_type {
            0 => {}
            253..=255 => {
                if v6 && s2k_type == 255 {
                    return Err(Error::Structural(format!(
                        "wrong s2k identifier for version {}",
                        pk.version
                    )));
                }
                let cipher = byte(r)?;
                let (key_size, block) = match cipher {
                    2 => (24, 8),
                    3 => (16, 8),
                    7 => (16, 16),
                    8 => (24, 16),
                    9 => (32, 16),
                    _ => (0, 0),
                };
                if cipher != 0 && key_size == 0 {
                    return Err(Error::Unsupported(
                        "unsupported cipher function in private key".into(),
                    ));
                }
                iv_size = block;
                if s2k_type == 253 {
                    let aead = byte(r)?;
                    iv_size = match aead {
                        1 => 16,
                        2 => 15,
                        3 => 12,
                        _ => return Err(Error::Unsupported("unsupported aead mode in private key".into())),
                    };
                }
                if v6 {
                    byte(r)?;
                }
                // s2k.ParseIntoParams (each read io.ReadFull's: EOF where none is
                // left), then Params.Function.
                let first = |b: Vec<u8>| b.first().copied().unwrap_or(0);
                let mode = first(r.read_full_io(1)?);
                let hash_id = match mode {
                    0 => Some(first(r.read_full_io(1)?)),
                    1 => Some(first(r.read_full_io(9)?)),
                    3 => Some(first(r.read_full_io(10)?)),
                    4 => {
                        let p = r.read_full_io(19)?;
                        let at = |i: usize| p.get(i).copied().unwrap_or(0);
                        let (passes, parallelism, memory) = (at(16), at(17), at(18));
                        if parallelism == 0 {
                            return Err(Error::Structural(
                                "invalid argon2 params: parallelism is 0".into(),
                            ));
                        }
                        if passes == 0 {
                            return Err(Error::Structural("invalid argon2 params: iterations is 0".into()));
                        }
                        if memory > 31 || (1u64 << memory) < 8 * u64::from(parallelism) {
                            return Err(Error::Structural(
                                "invalid argon2 params: memory is out of bounds".into(),
                            ));
                        }
                        None
                    }
                    101 => {
                        let g = r.read_full_io(5)?;
                        if g.get(1..5) != Some(&b"GNU\x01"[..]) {
                            return Err(Error::Unsupported("GNU S2K extension".into()));
                        }
                        // A dummy: the parse ends here.
                        return Ok(pk);
                    }
                    _ => return Err(Error::Unsupported("S2K function".into())),
                };
                if mode == 4 && s2k_type != 253 {
                    return Err(Error::Structural(
                        "using Argon2 S2K without AEAD is not allowed".into(),
                    ));
                }
                if mode == 0 && v6 {
                    return Err(Error::Structural(
                        "using Simple S2K with version 6 keys is not allowed".into(),
                    ));
                }
                if let Some(id) = hash_id
                    && !matches!(id, 2 | 8 | 9 | 10 | 11 | 12 | 14)
                {
                    return Err(Error::Unsupported(format!("hash for S2K function: {id}")));
                }
                encrypted = true;
            }
            _ => {
                return Err(Error::Unsupported(
                    "deprecated s2k function in private key".into(),
                ));
            }
        }
        if encrypted {
            if iv_size == 0 {
                return Err(Error::Unsupported("unsupported cipher in private key: 0".into()));
            }
            r.read_full(iv_size)?;
        }
        let data = Zeroizing::new(r.read_all()?);
        if encrypted {
            return Ok(pk);
        }
        if data.len() < 2 {
            return Err(Error::Structural("truncated private key data".into()));
        }
        let mut fields: &[u8] = &data;
        if !v6 {
            let (body, sum) = data.split_at(data.len() - 2);
            let total = body.iter().fold(0u16, |a, b| a.wrapping_add(u16::from(*b)));
            if sum != total.to_be_bytes() {
                return Err(Error::Structural("private key checksum failure".into()));
            }
            fields = body;
        }
        match pk.algo {
            RSA | RSA_SIGN_ONLY | RSA_ENCRYPT_ONLY => {
                for _ in 0..3 {
                    secret_mpi(&mut fields)?;
                }
            }
            DSA | ELGAMAL | ECDSA | ECDH | EDDSA => secret_mpi(&mut fields)?,
            X25519 | X448 | ED25519 | ED448 => {
                let (size, name) = match pk.algo {
                    X25519 => (32, "x25519"),
                    X448 => (56, "x448"),
                    ED25519 => (32, "ed25519"),
                    _ => (57, "ed448"),
                };
                if fields.len() != size {
                    return Err(Error::Structural(format!("wrong {name} key size")));
                }
            }
            _ => return Err(Error::Structural("unknown private key type".into())),
        }
        Ok(pk)
    }

    /// algorithmSpecificByteCount.
    fn material_len(&self) -> usize {
        let m = |x: &Mpi| 2 + x.bytes.len();
        match &self.material {
            Material::Rsa { n, e } => m(n) + m(e),
            Material::Dsa { p, q, g, y } => m(p) + m(q) + m(g) + m(y),
            Material::ElGamal { p, g, y } => m(p) + m(g) + m(y),
            Material::Ecdsa { oid, point, .. } | Material::EdDsa { oid, point, .. } => {
                1 + oid.len() + m(point)
            }
            Material::Ecdh { oid, point, kdf, .. } => 1 + oid.len() + m(point) + 1 + kdf.len(),
            Material::Native(p) => p.len(),
        }
    }

    /// serializeWithoutHeaders.
    fn body(&self, out: &mut Vec<u8>) {
        out.push(self.version);
        out.extend_from_slice(&self.created.to_be_bytes());
        out.push(self.algo);
        if self.version >= 5 {
            out.extend_from_slice(&(self.material_len() as u32).to_be_bytes());
        }
        let mpi = |out: &mut Vec<u8>, x: &Mpi| {
            out.extend_from_slice(&x.bits.to_be_bytes());
            out.extend_from_slice(&x.bytes);
        };
        let oid = |out: &mut Vec<u8>, o: &[u8]| {
            out.push(o.len() as u8);
            out.extend_from_slice(o);
        };
        match &self.material {
            Material::Rsa { n, e } => {
                mpi(out, n);
                mpi(out, e);
            }
            Material::Dsa { p, q, g, y } => {
                for x in [p, q, g, y] {
                    mpi(out, x);
                }
            }
            Material::ElGamal { p, g, y } => {
                for x in [p, g, y] {
                    mpi(out, x);
                }
            }
            Material::Ecdsa { oid: o, point, .. } | Material::EdDsa { oid: o, point, .. } => {
                oid(out, o);
                mpi(out, point);
            }
            Material::Ecdh {
                oid: o, point, kdf, ..
            } => {
                oid(out, o);
                mpi(out, point);
                oid(out, kdf);
            }
            Material::Native(p) => out.extend_from_slice(p),
        }
    }

    /// SerializeForHash: the prefix (0x99 and a two-octet length, or v6's 0x9b and a
    /// four-octet one) and the body, as a signature over a key hashes it.
    pub fn serialize_for_hash(&self, out: &mut Vec<u8>) {
        let len = self.material_len() + 6 + if self.version >= 5 { 4 } else { 0 };
        if self.version >= 5 {
            out.push(0x95 + self.version);
            out.extend_from_slice(&(len as u32).to_be_bytes());
        } else {
            out.push(0x99);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        self.body(out);
    }

    /// setFingerprintAndKeyId: SHA-256 of a v6 key, its first eight octets the ID; SHA-1
    /// of a v4 key, its last eight.
    fn set_fingerprint(&mut self) {
        let mut data = Vec::new();
        self.serialize_for_hash(&mut data);
        let take = |b: &[u8]| {
            let mut v = 0u64;
            for x in b.iter().take(8) {
                v = (v << 8) | u64::from(*x);
            }
            v
        };
        if self.version >= 5 {
            let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &data);
            self.fingerprint = d.as_ref().to_vec();
            self.key_id = take(self.fingerprint.get(..8).unwrap_or_default());
        } else {
            let d = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY, &data);
            self.fingerprint = d.as_ref().to_vec();
            self.key_id = take(self.fingerprint.get(12..20).unwrap_or_default());
        }
    }

    /// CanSign.
    pub fn can_sign(&self) -> bool {
        !matches!(self.algo, RSA_ENCRYPT_ONLY | ELGAMAL | ECDH)
    }
}

/// An ECDH KDF's parameters, read as an OID is (a length octet, then its octets).
fn oid_bytes(r: &mut Contents<'_, '_>) -> Result<Vec<u8>, Error> {
    oid(r)
}

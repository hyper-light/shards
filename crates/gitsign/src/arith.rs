//! The verifications AWS-LC has no algorithm for, over public values only (keys,
//! signatures and digests), so none needs constant time: RSA PKCS #1 v1.5 as Go's
//! crypto/rsa checks it (the encoded message rebuilt and compared, RFC 8017 §8.2.2), for
//! every hash Go names; DSA as crypto/dsa; ECDSA on the brainpool curves as Go's generic
//! ECDSA over go-crypto's brainpool curves; and Ed448 as cloudflare/circl (RFC 8032
//! §5.2.7).

use num_bigint::BigUint;

use crate::key::Curve;
use crate::signature::Hash;

/// elliptic.Unmarshal's check of `data` as a point of a Weierstrass `curve`:
/// uncompressed, of the curve's size, each coordinate below the prime, on the curve. The
/// NIST curves and secp256k1 through AWS-LC; brainpool's as go-crypto's rcurve checks.
pub fn on_curve(curve: Curve, data: &[u8]) -> bool {
    use aws_lc_rs::signature::{
        ECDSA_P256_SHA256_FIXED, ECDSA_P256K1_SHA256_FIXED, ECDSA_P384_SHA384_FIXED, ECDSA_P521_SHA512_FIXED,
        ParsedPublicKey,
    };
    let aws = match curve {
        Curve::P256 => &ECDSA_P256_SHA256_FIXED,
        Curve::P384 => &ECDSA_P384_SHA384_FIXED,
        Curve::P521 => &ECDSA_P521_SHA512_FIXED,
        Curve::Secp256k1 => &ECDSA_P256K1_SHA256_FIXED,
        _ => {
            return brainpool(curve).is_some_and(|c| c.point(data).is_some());
        }
    };
    let size = match curve {
        Curve::P384 => 48,
        Curve::P521 => 66,
        _ => 32,
    };
    data.len() == 1 + 2 * size && data.first() == Some(&4) && ParsedPublicKey::new(aws, data).is_ok()
}

/// A brainpool curve's parameters.
pub fn brainpool(curve: Curve) -> Option<&'static Brainpool> {
    match curve {
        Curve::BrainpoolP256 => Some(&BRAINPOOL_P256),
        Curve::BrainpoolP384 => Some(&BRAINPOOL_P384),
        Curve::BrainpoolP512 => Some(&BRAINPOOL_P512),
        _ => None,
    }
}

fn big(hex: &str) -> BigUint {
    BigUint::parse_bytes(hex.as_bytes(), 16).unwrap_or_default()
}

/// x mod m, or None for a zero modulus.
fn modulo(x: &BigUint, m: &BigUint) -> Option<BigUint> {
    (m.bits() > 0).then(|| x % m)
}

/// The DigestInfo prefix of each hash (Go's fips140/rsa hashPrefixes).
fn digest_info(hash: Hash) -> &'static [u8] {
    match hash {
        Hash::Sha1 => &[
            0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14,
        ],
        Hash::Sha224 => &[
            0x30, 0x2d, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x04, 0x05,
            0x00, 0x04, 0x1c,
        ],
        Hash::Sha256 => &[
            0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
            0x00, 0x04, 0x20,
        ],
        Hash::Sha384 => &[
            0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05,
            0x00, 0x04, 0x30,
        ],
        Hash::Sha512 => &[
            0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05,
            0x00, 0x04, 0x40,
        ],
        Hash::Sha3_256 => &[
            0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x08, 0x05,
            0x00, 0x04, 0x20,
        ],
        Hash::Sha3_512 => &[
            0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x0a, 0x05,
            0x00, 0x04, 0x40,
        ],
    }
}

/// Why Go's crypto/rsa refuses a public key before it verifies with it, in its words.
pub fn rsa_key_error(n: &[u8], e: &[u8]) -> Option<String> {
    let n = BigUint::from_bytes_be(n);
    if n.bits() < 1024 {
        return Some(format!(
            "crypto/rsa: {}-bit keys are insecure (see https://go.dev/pkg/crypto/rsa#hdr-Minimum_key_size)",
            n.bits()
        ));
    }
    if !n.bit(0) {
        return Some("crypto/rsa: public modulus is even".into());
    }
    let e = BigUint::from_bytes_be(e);
    if e < BigUint::from(2u8) {
        return Some("crypto/rsa: public exponent too small or negative".into());
    }
    if !e.bit(0) {
        return Some("crypto/rsa: public exponent is even".into());
    }
    None
}

/// rsa.VerifyPKCS1v15 of `digest` by (n, e): the signature, of the modulus's length (as
/// go-crypto pads it), below n, raised to e, equal octet for octet to the encoded
/// message EMSA-PKCS1-v1_5 makes of the digest.
pub fn rsa_pkcs1_verify(n: &[u8], e: &[u8], hash: Hash, digest: &[u8], sig: &[u8]) -> bool {
    if rsa_key_error(n, e).is_some() {
        return false;
    }
    let n = BigUint::from_bytes_be(n);
    let e = BigUint::from_bytes_be(e);
    let k = usize::try_from(n.bits().div_ceil(8)).unwrap_or(usize::MAX);
    if sig.len() != k {
        return false;
    }
    let s = BigUint::from_bytes_be(sig);
    if s >= n {
        return false;
    }
    let m = s.modpow(&e, &n).to_bytes_be();
    let Some(pad) = k.checked_sub(m.len()) else {
        return false;
    };
    let mut em = vec![0u8; pad];
    em.extend(m);
    let prefix = digest_info(hash);
    let t = prefix.len() + digest.len();
    let Some(ps) = k.checked_sub(t + 3).filter(|ps| *ps >= 8) else {
        return false;
    };
    let mut want = Vec::with_capacity(k);
    want.extend_from_slice(&[0x00, 0x01]);
    want.extend(std::iter::repeat_n(0xff, ps));
    want.push(0x00);
    want.extend_from_slice(prefix);
    want.extend_from_slice(digest);
    em == want
}

/// rsa.VerifyPSS of `digest` by (n, e) (RFC 8017 §9.1.2, as Go's emsaPSSVerify): the salt
/// `salt` octets long, or of the length the encoding shows (PSSSaltLengthAuto) where None.
pub fn rsa_pss_verify(
    n: &[u8],
    e: &[u8],
    hash: Hash,
    digest: &[u8],
    sig: &[u8],
    salt: Option<usize>,
) -> bool {
    use aws_lc_rs::digest as d;
    if rsa_key_error(n, e).is_some() {
        return false;
    }
    let (alg, h_len): (&'static d::Algorithm, usize) = match hash {
        Hash::Sha1 => (&d::SHA1_FOR_LEGACY_USE_ONLY, 20),
        Hash::Sha256 => (&d::SHA256, 32),
        Hash::Sha384 => (&d::SHA384, 48),
        Hash::Sha512 => (&d::SHA512, 64),
        _ => return false,
    };
    let n = BigUint::from_bytes_be(n);
    let e = BigUint::from_bytes_be(e);
    let bits = n.bits();
    let k = usize::try_from(bits.div_ceil(8)).unwrap_or(usize::MAX);
    if sig.len() != k {
        return false;
    }
    let s = BigUint::from_bytes_be(sig);
    if s >= n {
        return false;
    }
    let m = s.modpow(&e, &n).to_bytes_be();
    let Some(lead) = k.checked_sub(m.len()) else {
        return false;
    };
    let mut full = vec![0u8; lead];
    full.extend(m);
    let em_bits = usize::try_from(bits - 1).unwrap_or(usize::MAX);
    let em_len = em_bits.div_ceil(8);
    // The modulus's octets beyond emLen: one at most, zero.
    let em = match k.checked_sub(em_len) {
        Some(0) => full.as_slice(),
        Some(1) if full.first() == Some(&0) => full.get(1..).unwrap_or_default(),
        _ => return false,
    };
    let min = h_len + salt.unwrap_or(0) + 2 - usize::from(salt.is_none());
    if digest.len() != h_len || em_len < min || em.last() != Some(&0xbc) {
        return false;
    }
    let db_len = em_len - h_len - 1;
    let (masked, rest) = em.split_at(db_len);
    let Some(h) = rest.get(..h_len) else {
        return false;
    };
    let top = 8 * em_len - em_bits;
    let bit_mask = 0xffu8 >> top;
    if masked.first().is_some_and(|b| b & !bit_mask != 0) {
        return false;
    }
    let mut mask = Vec::with_capacity(db_len + h_len);
    let mut counter = 0u32;
    while mask.len() < db_len {
        let mut ctx = d::Context::new(alg);
        ctx.update(h);
        ctx.update(&counter.to_be_bytes());
        mask.extend_from_slice(ctx.finish().as_ref());
        counter += 1;
    }
    let mut db: Vec<u8> = masked.iter().zip(&mask).map(|(a, b)| a ^ b).collect();
    if let Some(first) = db.first_mut() {
        *first &= bit_mask;
    }
    let s_len = match salt {
        Some(s) => s,
        None => match db.iter().position(|x| *x == 1) {
            Some(ps) => db.len() - ps - 1,
            None => return false,
        },
    };
    let Some(ps_len) = em_len.checked_sub(h_len + s_len + 2) else {
        return false;
    };
    if db.get(..ps_len).is_none_or(|z| z.iter().any(|x| *x != 0)) || db.get(ps_len) != Some(&1) {
        return false;
    }
    let Some(salt) = db.get(db.len() - s_len..) else {
        return false;
    };
    let mut ctx = d::Context::new(alg);
    ctx.update(&[0u8; 8]);
    ctx.update(digest);
    ctx.update(salt);
    ctx.finish().as_ref() == h
}

/// dsa.Verify of `digest` (already cut to the subgroup's size, as go-crypto cuts it).
pub fn dsa_verify(p: &[u8], q: &[u8], g: &[u8], y: &[u8], digest: &[u8], r: &[u8], s: &[u8]) -> bool {
    let (p, q, g, y) = (
        BigUint::from_bytes_be(p),
        BigUint::from_bytes_be(q),
        BigUint::from_bytes_be(g),
        BigUint::from_bytes_be(y),
    );
    let (r, s) = (BigUint::from_bytes_be(r), BigUint::from_bytes_be(s));
    if p.bits() == 0 {
        return false;
    }
    let zero = BigUint::ZERO;
    if r == zero || r >= q || s == zero || s >= q {
        return false;
    }
    let Some(w) = s.modinv(&q) else {
        return false;
    };
    if q.bits() % 8 != 0 {
        return false;
    }
    let z = BigUint::from_bytes_be(digest);
    let u1 = (&z * &w) % &q;
    let u2 = (&r * &w) % &q;
    let v = (g.modpow(&u1, &p) * y.modpow(&u2, &p)) % &p;
    v % &q == r
}

/// A prime field's arithmetic.
struct Field<'a> {
    p: &'a BigUint,
}

impl Field<'_> {
    fn add(&self, a: &BigUint, b: &BigUint) -> BigUint {
        (a + b) % self.p
    }
    fn sub(&self, a: &BigUint, b: &BigUint) -> BigUint {
        ((a + self.p) - (b % self.p)) % self.p
    }
    fn mul(&self, a: &BigUint, b: &BigUint) -> BigUint {
        (a * b) % self.p
    }
    fn small(&self, k: u32, a: &BigUint) -> BigUint {
        (BigUint::from(k) * a) % self.p
    }
    /// a⁻¹ by Fermat: p is prime.
    fn inv(&self, a: &BigUint) -> BigUint {
        a.modpow(&(self.p - BigUint::from(2u8)), self.p)
    }
}

/// A point of a short Weierstrass curve with a = -3, in Jacobian coordinates; None the
/// point at infinity.
type Jacobian = Option<(BigUint, BigUint, BigUint)>;

/// dbl-2001-b (a = -3).
fn jdouble(f: &Field<'_>, pt: &Jacobian) -> Jacobian {
    let (x, y, z) = pt.as_ref()?;
    if y.bits() == 0 {
        return None;
    }
    let delta = f.mul(z, z);
    let gamma = f.mul(y, y);
    let beta = f.mul(x, &gamma);
    let alpha = f.small(3, &f.mul(&f.sub(x, &delta), &f.add(x, &delta)));
    let x3 = f.sub(&f.mul(&alpha, &alpha), &f.small(8, &beta));
    let yz = f.add(y, z);
    let z3 = f.sub(&f.sub(&f.mul(&yz, &yz), &gamma), &delta);
    let g2 = f.mul(&gamma, &gamma);
    let y3 = f.sub(&f.mul(&alpha, &f.sub(&f.small(4, &beta), &x3)), &f.small(8, &g2));
    Some((x3, y3, z3))
}

/// add-2007-bl, with the cases it leaves out: a point at infinity, equal points, and a
/// point and its negation.
fn jadd(f: &Field<'_>, a: &Jacobian, b: &Jacobian) -> Jacobian {
    let Some((x1, y1, z1)) = a else {
        return b.clone();
    };
    let Some((x2, y2, z2)) = b else {
        return a.clone();
    };
    let z1z1 = f.mul(z1, z1);
    let z2z2 = f.mul(z2, z2);
    let u1 = f.mul(x1, &z2z2);
    let u2 = f.mul(x2, &z1z1);
    let s1 = f.mul(&f.mul(y1, z2), &z2z2);
    let s2 = f.mul(&f.mul(y2, z1), &z1z1);
    if u1 == u2 {
        return if s1 == s2 { jdouble(f, a) } else { None };
    }
    let h = f.sub(&u2, &u1);
    let i = f.small(4, &f.mul(&h, &h));
    let j = f.mul(&h, &i);
    let r = f.small(2, &f.sub(&s2, &s1));
    let v = f.mul(&u1, &i);
    let x3 = f.sub(&f.sub(&f.mul(&r, &r), &j), &f.small(2, &v));
    let y3 = f.sub(&f.mul(&r, &f.sub(&v, &x3)), &f.small(2, &f.mul(&s1, &j)));
    let zz = f.add(z1, z2);
    let z3 = f.mul(&f.sub(&f.sub(&f.mul(&zz, &zz), &z1z1), &z2z2), &h);
    Some((x3, y3, z3))
}

fn jmul(f: &Field<'_>, k: &BigUint, pt: &Jacobian) -> Jacobian {
    let mut acc: Jacobian = None;
    for i in (0..k.bits()).rev() {
        acc = jdouble(f, &acc);
        if k.bit(i) {
            acc = jadd(f, &acc, pt);
        }
    }
    acc
}

/// A brainpool r1 curve, computed on the twisted t1 curve it maps to (z), as go-crypto's
/// rcurve computes: t1's prime, order, b and generator (go-crypto v1.4.1
/// brainpool/brainpool.go).
#[derive(Debug)]
pub struct Brainpool {
    p: &'static str,
    n: &'static str,
    b: &'static str,
    gx: &'static str,
    gy: &'static str,
    z: &'static str,
    pub bytes: usize,
}

pub const BRAINPOOL_P256: Brainpool = Brainpool {
    p: "A9FB57DBA1EEA9BC3E660A909D838D726E3BF623D52620282013481D1F6E5377",
    n: "A9FB57DBA1EEA9BC3E660A909D838D718C397AA3B561A6F7901E0E82974856A7",
    b: "662C61C430D84EA4FE66A7733D0B76B7BF93EBC4AF2F49256AE58101FEE92B04",
    gx: "A3E8EB3CC1CFE7B7732213B23A656149AFA142C47AAFBC2B79A191562E1305F4",
    gy: "2D996C823439C56D7F7B22E14644417E69BCB6DE39D027001DABE8F35B25C9BE",
    z: "3E2D4BD9597B58639AE7AA669CAB9837CF5CF20A2C852D10F655668DFC150EF0",
    bytes: 32,
};

pub const BRAINPOOL_P384: Brainpool = Brainpool {
    p: "8CB91E82A3386D280F5D6F7E50E641DF152F7109ED5456B412B1DA197FB71123ACD3A729901D1A71874700133107EC53",
    n: "8CB91E82A3386D280F5D6F7E50E641DF152F7109ED5456B31F166E6CAC0425A7CF3AB6AF6B7FC3103B883202E9046565",
    b: "7F519EADA7BDA81BD826DBA647910F8C4B9346ED8CCDC64E4B1ABD11756DCE1D2074AA263B88805CED70355A33B471EE",
    gx: "18DE98B02DB9A306F2AFCD7235F72A819B80AB12EBD653172476FECD462AABFFC4FF191B946A5F54D8D0AA2F418808CC",
    gy: "25AB056962D30651A114AFD2755AD336747F93475B7A1FCA3B88F2B6A208CCFE469408584DC2B2912675BF5B9E582928",
    z: "41DFE8DD399331F7166A66076734A89CD0D2BCDB7D068E44E1F378F41ECBAE97D2D63DBC87BCCDDCCC5DA39E8589291C",
    bytes: 48,
};

pub const BRAINPOOL_P512: Brainpool = Brainpool {
    p: "AADD9DB8DBE9C48B3FD4E6AE33C9FC07CB308DB3B3C9D20ED6639CCA703308717D4D9B009BC66842AECDA12AE6A380E62881FF2F2D82C68528AA6056583A48F3",
    n: "AADD9DB8DBE9C48B3FD4E6AE33C9FC07CB308DB3B3C9D20ED6639CCA70330870553E5C414CA92619418661197FAC10471DB1D381085DDADDB58796829CA90069",
    b: "7CBBBCF9441CFAB76E1890E46884EAE321F70C0BCB4981527897504BEC3E36A62BCDFA2304976540F6450085F2DAE145C22553B465763689180EA2571867423E",
    gx: "640ECE5C12788717B9C1BA06CBC2A6FEBA85842458C56DDE9DB1758D39C0313D82BA51735CDB3EA499AA77A7D6943A64F7A3F25FE26F06B51BAA2696FA9035DA",
    gy: "5B534BD595F5AF0FA2C892376C84ACE1BB4E3019B71634C01131159CAE03CEE9D9932184BEEF216BD71DF2DADF86A627306ECFF96DBB8BACE198B61E00F8B332",
    z: "12EE58E6764838B69782136F0F2D3BA06E27695716054092E60A80BEDB212B64E585D90BCE13761F85C3F1D2A64E3BE8FEA2220F01EBA5EEB0F35DBD29D922AB",
    bytes: 64,
};

/// NIST P-224, a short Weierstrass curve with a = -3 as brainpool's twists are, computed
/// untwisted (z = 1), for the keys crypto/ecdsa verifies on it: SP 800-186 §3.2.1.2's
/// parameters, as Go 1.26's crypto/elliptic (nistec.go) carries them.
pub const NIST_P224: Brainpool = Brainpool {
    p: "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF000000000000000000000001",
    n: "FFFFFFFFFFFFFFFFFFFFFFFFFFFF16A2E0B8F03E13DD29455C5C2A3D",
    b: "B4050A850C04B3ABF54132565044B0B7D7BFD8BA270B39432355FFB4",
    gx: "B70E0CBD6BB4BF7F321390B94A03C1D356C21122343280D6115C1D21",
    gy: "BD376388B5F723FB4C22DFE6CD4375A05A07476444D5819985007E34",
    z: "01",
    bytes: 28,
};

/// NIST P-521, likewise (a = -3, untwisted), for digests longer than AWS-LC takes.
pub const NIST_P521: Brainpool = Brainpool {
    p: "1FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
    n: "1FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFA51868783BF2F966B7FCC0148F709A5D03BB5C9B8899C47AEBB6FB71E91386409",
    b: "51953EB9618E1C9A1F929A21A0B68540EEA2DA725B99B315F3B8B489918EF109E156193951EC7E937B1652C0BD3BB1BF073573DF883D2C34F1EF451FD46B503F00",
    gx: "C6858E06B70404E9CD9E3ECB662395B4429C648139053FB521F828AF606B4D3DBAA14B5E77EFE75928FE1DC127A2FFA8DE3348B3C1856A429BF97E7E31C2E5BD66",
    gy: "11839296A789A3BC0045C8A5FB42C7D1BD998F54449579B446817AFBD17273E662C97EE72995EF42640C550B9013FAD0761353C7086A272C24088BE94769FD16650",
    z: "01",
    bytes: 66,
};

impl Brainpool {
    /// elliptic.Unmarshal: an uncompressed point of the curve's size, on the curve.
    pub fn point(&self, data: &[u8]) -> Option<(BigUint, BigUint)> {
        if data.len() != 1 + 2 * self.bytes || data.first() != Some(&4) {
            return None;
        }
        let x = BigUint::from_bytes_be(data.get(1..1 + self.bytes)?);
        let y = BigUint::from_bytes_be(data.get(1 + self.bytes..)?);
        self.on_curve(&x, &y).then_some((x, y))
    }

    /// rcurve.IsOnCurve of an uncompressed point's coordinates, below the prime.
    pub fn on_curve(&self, x: &BigUint, y: &BigUint) -> bool {
        let p = big(self.p);
        if *x >= p || *y >= p {
            return false;
        }
        let f = Field { p: &p };
        let (tx, ty) = self.to_twisted(&f, x, y);
        let lhs = f.mul(&ty, &ty);
        let rhs = f.add(
            &f.sub(&f.mul(&f.mul(&tx, &tx), &tx), &f.small(3, &tx)),
            &big(self.b),
        );
        lhs == rhs
    }

    fn to_twisted(&self, f: &Field<'_>, x: &BigUint, y: &BigUint) -> (BigUint, BigUint) {
        let z = big(self.z);
        let z2 = f.mul(&z, &z);
        let z3 = f.mul(&z2, &z);
        (f.mul(x, &z2), f.mul(y, &z3))
    }

    /// ecdsa.Verify (verifyLegacy) of `digest` by the point (x, y): r and s in [1, n),
    /// the digest cut to n's bits (hashToInt), and x of u1·G + u2·Q, mapped back from
    /// the twist, equal to r mod n.
    pub fn verify(&self, x: &BigUint, y: &BigUint, digest: &[u8], r: &[u8], s: &[u8]) -> bool {
        let p = big(self.p);
        let n = big(self.n);
        let f = Field { p: &p };
        let (r, s) = (BigUint::from_bytes_be(r), BigUint::from_bytes_be(s));
        let zero = BigUint::ZERO;
        if r == zero || s == zero || r >= n || s >= n {
            return false;
        }
        let order_bits = n.bits();
        let order_bytes = usize::try_from(order_bits.div_ceil(8)).unwrap_or(usize::MAX);
        let cut = digest.get(..order_bytes.min(digest.len())).unwrap_or_default();
        let mut e = BigUint::from_bytes_be(cut);
        let excess = (cut.len() as u64 * 8).saturating_sub(order_bits);
        if excess > 0 {
            e >>= excess;
        }
        let Some(w) = s.modinv(&n) else {
            return false;
        };
        let u1 = (&e * &w) % &n;
        let u2 = (&r * &w) % &n;
        let one = BigUint::from(1u8);
        let g: Jacobian = Some((big(self.gx), big(self.gy), one.clone()));
        let (tx, ty) = self.to_twisted(&f, x, y);
        let q: Jacobian = Some((tx, ty, one));
        let sum = jadd(&f, &jmul(&f, &u1, &g), &jmul(&f, &u2, &q));
        let Some((jx, _, jz)) = sum else {
            return false;
        };
        // Affine x on the twist, then back: x / z² (Jacobian), then / z_map².
        let zi = f.inv(&jz);
        let ax = f.mul(&jx, &f.mul(&zi, &zi));
        let zm = big(self.z);
        let zm2 = f.mul(&zm, &zm);
        let xr = f.mul(&ax, &f.inv(&zm2));
        modulo(&xr, &n).is_some_and(|v| v == r)
    }
}

/// Ed448 (RFC 8032 §5.2), its point arithmetic in projective coordinates (§5.2.4).
mod ed448 {
    use num_bigint::BigUint;
    use sha3::digest::{ExtendableOutput, Update, XofReader};

    use super::{Field, big};

    const P: &str = "fffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    const L: &str = "3fffffffffffffffffffffffffffffffffffffffffffffffffffffff7cca23e9c44edb49aed63690216cc2728dc58f552378c292ab5844f3";
    const BX: &str = "4f1970c66bed0ded221d15a622bf36da9e146570470f1767ea6de324a3d3a46412ae1af72ab66511433b80e18b00938e2626a82bc70cc05e";
    const BY: &str = "693f46716eb6bc248876203756c9c7624bea73736ca3984087789c1e05a0c2d73ad3ff1ce67c39c4fdbd132c4ed7c8ad9808795bf230fa14";

    type Point = (BigUint, BigUint, BigUint);

    fn d(f: &Field<'_>) -> BigUint {
        // -39081 mod p.
        f.sub(&BigUint::ZERO, &BigUint::from(39081u32))
    }

    fn add(f: &Field<'_>, (x1, y1, z1): &Point, (x2, y2, z2): &Point) -> Point {
        let a = f.mul(z1, z2);
        let b = f.mul(&a, &a);
        let c = f.mul(x1, x2);
        let dd = f.mul(y1, y2);
        let e = f.mul(&f.mul(&d(f), &c), &dd);
        let ff = f.sub(&b, &e);
        let g = f.add(&b, &e);
        let h = f.mul(&f.add(x1, y1), &f.add(x2, y2));
        let x3 = f.mul(&f.mul(&a, &ff), &f.sub(&f.sub(&h, &c), &dd));
        let y3 = f.mul(&f.mul(&a, &g), &f.sub(&dd, &c));
        let z3 = f.mul(&ff, &g);
        (x3, y3, z3)
    }

    fn double(f: &Field<'_>, (x1, y1, z1): &Point) -> Point {
        let s = f.add(x1, y1);
        let b = f.mul(&s, &s);
        let c = f.mul(x1, x1);
        let dd = f.mul(y1, y1);
        let e = f.add(&c, &dd);
        let h = f.mul(z1, z1);
        let j = f.sub(&e, &f.small(2, &h));
        let x3 = f.mul(&f.sub(&b, &e), &j);
        let y3 = f.mul(&e, &f.sub(&c, &dd));
        let z3 = f.mul(&e, &j);
        (x3, y3, z3)
    }

    fn mul(f: &Field<'_>, k: &BigUint, pt: &Point) -> Point {
        let mut acc: Point = (BigUint::ZERO, BigUint::from(1u8), BigUint::from(1u8));
        for i in (0..k.bits()).rev() {
            acc = double(f, &acc);
            if k.bit(i) {
                acc = add(f, &acc, pt);
            }
        }
        acc
    }

    fn le(b: &[u8]) -> BigUint {
        BigUint::from_bytes_le(b)
    }

    /// goldilocks.FromBytes: y below p, the sign of x from the last octet's top bit (its
    /// other bits unread, as circl leaves them), and x the root of (y² - 1)/(dy² - 1).
    fn decode(f: &Field<'_>, enc: &[u8]) -> Option<Point> {
        if enc.len() != 57 {
            return None;
        }
        let y = le(enc.get(..56)?);
        if &y >= f.p {
            return None;
        }
        let sign = enc.get(56)? >> 7;
        let one = BigUint::from(1u8);
        let y2 = f.mul(&y, &y);
        let u = f.sub(&y2, &one);
        let v = f.sub(&f.mul(&d(f), &y2), &one);
        // x = u³v(u⁵v³)^((p-3)/4) (§5.2.3), a root where v·x² = u.
        let u3 = f.mul(&f.mul(&u, &u), &u);
        let u5 = f.mul(&f.mul(&u3, &u), &u);
        let v3 = f.mul(&f.mul(&v, &v), &v);
        let exp = (f.p - BigUint::from(3u8)) >> 2;
        let mut x = f.mul(&f.mul(&u3, &v), &f.mul(&u5, &v3).modpow(&exp, f.p));
        if f.mul(&v, &f.mul(&x, &x)) != u {
            return None;
        }
        if x.bits() == 0 && sign == 1 {
            return None;
        }
        if u8::from(x.bit(0)) != sign {
            x = f.sub(&BigUint::ZERO, &x);
        }
        Some((x, y, one))
    }

    fn encode(f: &Field<'_>, (x, y, z): &Point) -> Vec<u8> {
        let zi = f.inv(z);
        let (ax, ay) = (f.mul(x, &zi), f.mul(y, &zi));
        let mut out = ay.to_bytes_le();
        out.resize(57, 0);
        if ax.bit(0)
            && let Some(last) = out.last_mut()
        {
            *last |= 0x80;
        }
        out
    }

    /// p, L and B as the module holds them.
    #[cfg(test)]
    pub fn constants() -> [BigUint; 4] {
        [big(P), big(L), big(BX), big(BY)]
    }

    /// ed448.Verify(public, message, signature, ""): S below L, then [S]B - [k]A, encoded,
    /// equal to R's octets.
    pub fn verify(public: &[u8], message: &[u8], sig: &[u8]) -> bool {
        if public.len() != 57 || sig.len() != 114 {
            return false;
        }
        let p = big(P);
        let l = big(L);
        let f = Field { p: &p };
        let (Some(r), Some(s)) = (sig.get(..57), sig.get(57..)) else {
            return false;
        };
        let s = le(s);
        if s >= l {
            return false;
        }
        let Some(a) = decode(&f, public) else {
            return false;
        };
        let mut h = sha3::Shake256::default();
        // dom4(0, ""): "SigEd448", the flag, the context's length.
        h.update(b"SigEd448\x00\x00");
        h.update(r);
        h.update(public);
        h.update(message);
        let mut k = [0u8; 114];
        h.finalize_xof().read(&mut k);
        let k = le(&k) % &l;
        let neg_a = (f.sub(&BigUint::ZERO, &a.0), a.1, a.2);
        let one = BigUint::from(1u8);
        let b = (big(BX), big(BY), one);
        let sum = add(&f, &mul(&f, &s, &b), &mul(&f, &k, &neg_a));
        encode(&f, &sum) == r
    }
}

pub use ed448::verify as ed448_verify;

#[cfg(test)]
pub(crate) use ed448::constants as ed448_constants;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::string_slice)]

    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // RFC 8032 §5.2's parameters, from their definitions: p = 2^448 - 2^224 - 1,
    // L = 2^446 - 13818066809895115352007386748515426880336692474882178609894547503885,
    // and B on the curve x² + y² = 1 + d·x²·y², d = -39081.
    #[test]
    fn nist_generators_are_on_their_curves_and_of_their_orders() {
        for c in [NIST_P224, NIST_P521] {
            let (gx, gy) = (big(c.gx), big(c.gy));
            assert!(c.on_curve(&gx, &gy));
            let p = big(c.p);
            let f = Field { p: &p };
            let g: Jacobian = Some((gx, gy, BigUint::from(1u8)));
            assert!(jmul(&f, &big(c.n), &g).is_none());
        }
    }

    #[test]
    fn ed448_constants_are_rfc_8032s() {
        let [p, l, x, y] = ed448_constants();
        let two = BigUint::from(2u8);
        assert_eq!(p, two.pow(448) - two.pow(224) - 1u8);
        let c = BigUint::parse_bytes(
            b"13818066809895115352007386748515426880336692474882178609894547503885",
            10,
        )
        .unwrap();
        assert_eq!(l, two.pow(446) - c);
        let d = &p - BigUint::from(39081u32);
        let (x2, y2) = ((&x * &x) % &p, (&y * &y) % &p);
        assert_eq!((&x2 + &y2) % &p, (1u8 + d * x2 * y2) % &p);
        // RFC 8032's B, given in decimal.
        assert_eq!(
            x,
            BigUint::parse_bytes(b"224580040295924300187604334099896036246789641632564134246125461686950415467406032909029192869357953282578032075146446173674602635247710", 10).unwrap()
        );
        assert_eq!(
            y,
            BigUint::parse_bytes(b"298819210078481492676017930443930673437544040154080242095928241372331506189835876003536878655418784733982303233503462500531545062832660", 10).unwrap()
        );
    }

    // RFC 8032 §7.4: "-----Blank", "1 octet", and "11 octets".
    #[test]
    fn ed448_verifies_rfc_8032s_vectors() {
        let cases = [
            (
                "5fd7449b59b461fd2ce787ec616ad46a1da1342485a70e1f8a0ea75d80e96778edf124769b46c7061bd6783df1e50f6cd1fa1abeafe8256180",
                "",
                "533a37f6bbe457251f023c0d88f976ae2dfb504a843e34d2074fd823d41a591f2b233f034f628281f2fd7a22ddd47d7828c59bd0a21bfd3980ff0d2028d4b18a9df63e006c5d1c2d345b925d8dc00b4104852db99ac5c7cdda8530a113a0f4dbb61149f05a7363268c71d95808ff2e652600",
            ),
            (
                "43ba28f430cdff456ae531545f7ecd0ac834a55d9358c0372bfa0c6c6798c0866aea01eb00742802b8438ea4cb82169c235160627b4c3a9480",
                "03",
                "26b8f91727bd62897af15e41eb43c377efb9c610d48f2335cb0bd0087810f4352541b143c4b981b7e18f62de8ccdf633fc1bf037ab7cd779805e0dbcc0aae1cbcee1afb2e027df36bc04dcecbf154336c19f0af7e0a6472905e799f1953d2a0ff3348ab21aa4adafd1d234441cf807c03a00",
            ),
            (
                "dcea9e78f35a1bf3499a831b10b86c90aac01cd84b67a0109b55a36e9328b1e365fce161d71ce7131a543ea4cb5f7e9f1d8b00696447001400",
                "0c3e544074ec63b0265e0c",
                "1f0a8888ce25e8d458a21130879b840a9089d999aaba039eaf3e3afa090a09d389dba82c4ff2ae8ac5cdfb7c55e94d5d961a29fe0109941e00b8dbdeea6d3b051068df7254c0cdc129cbe62db2dc957dbb47b51fd3f213fb8698f064774250a5028961c9bf8ffd973fe5d5c206492b140e00",
            ),
        ];
        for (public, message, sig) in cases {
            let (public, message, sig) = (hex(public), hex(message), hex(sig));
            assert!(ed448_verify(&public, &message, &sig));
            let mut bad = sig.clone();
            bad[3] ^= 1;
            assert!(!ed448_verify(&public, &message, &bad));
            let mut other = message.clone();
            other.push(0);
            assert!(!ed448_verify(&public, &other, &sig));
        }
    }

    #[test]
    fn brainpool_generators_are_on_their_curves() {
        for (c, gx, gy) in [
            (
                &BRAINPOOL_P256,
                "8BD2AEB9CB7E57CB2C4B482FFC81B7AFB9DE27E1E3BD23C23A4453BD9ACE3262",
                "547EF835C3DAC4FD97F8461A14611DC9C27745132DED8E545C1D54C72F046997",
            ),
            (
                &BRAINPOOL_P384,
                "1D1C64F068CF45FFA2A63A81B7C13F6B8847A3E77EF14FE3DB7FCAFE0CBD10E8E826E03436D646AAEF87B2E247D4AF1E",
                "8ABE1D7520F9C2A45CB1EB8E95CFD55262B70B29FEEC5864E19C054FF99129280E4646217791811142820341263C5315",
            ),
            (
                &BRAINPOOL_P512,
                "81AEE4BDD82ED9645A21322E9C4C6A9385ED9F70B5D916C1B43B62EEF4D0098EFF3B1F78E2D0D48D50D1687B93B97D5F7C6D5047406A5E688B352209BCB9F822",
                "7DDE385D566332ECC0EABFA9CF7822FDF209F70024A57B1AA000C55B881F8111B2DCDE494A5F485E5BCA4BD88A2763AED1CA2B2FA8F0540678CD1E0F3AD80892",
            ),
        ] {
            let (x, y) = (big(gx), big(gy));
            assert!(c.on_curve(&x, &y));
            assert!(!c.on_curve(&x, &(&y + 1u8)));
            let mut point = vec![4u8];
            point.extend(hex(gx));
            point.extend(hex(gy));
            assert!(c.point(&point).is_some());
            let last = point.len() - 1;
            point[last] ^= 1;
            assert!(c.point(&point).is_none());
        }
    }
}

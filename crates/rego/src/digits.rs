//! An integer's decimal digits, however many: made once from its 64-bit limbs, holding
//! the digits and a working set bounded before it starts ([`need`]; D98, PM M537).
//!
//! num-bigint 0.4.8's `to_str_radix` divides by powers of ten and conquers, its
//! Burnikel-Ziegler division cloning its operands at each level of its recursion; Go's
//! `nat.utoa` (natconv.go) divides likewise. Here the digits come the other way, by Brent
//! and Zimmermann's FastIntegerInput (Modern Computer Arithmetic, 2010, §1.7.2, algorithm
//! 1.25) computing in base 10^19 on the base-2^64 limbs: an aligned block of 2^(k+1)
//! limbs is its upper half times T[k] = 2^(64·2^k) plus its lower half, each
//! T[k+1] = T[k]², from T[0] = 2^64. A product with an operand of at most [`SCHOOL`]
//! limbs is schoolbook's; past it, a cyclic convolution by number-theoretic transforms
//! modulo three primes (Pollard, "The fast Fourier transform in a finite field", Math.
//! Comp. 25, 1971), the residues joined by Garner's algorithm (MCA §2.7). A block whose
//! limbs are all zero or all ones is 0 or T[k] − 1, made with no product: the integers
//! OPA writes this long are a float's mantissa times a power of two (floor, ceil, round,
//! format_int, bits.lsh) or a few of them joined (bits.and/or/xor/negate, rem), whose
//! zero limbs cost the products that shift the mantissa, not a division each.

use std::f64::consts::LOG10_2;

use num_bigint::BigUint;

use crate::number::Error;

/// 10^19, the largest power of ten below 2^64: the base of the decimal limbs.
const BASE: u64 = 10_000_000_000_000_000_000;
/// The digits of a decimal limb.
const LIMB_DIGITS: usize = 19;
/// 2^64 in base 10^19: T[0].
const T0: [u64; 2] = [8_446_744_073_709_551_616, 1];
/// Möller and Granlund's reciprocal of BASE, ⌊(2^128 − 1) / BASE⌋ − 2^64, for dividing
/// two limbs by BASE with two products ("Improved division by invariant integers", IEEE
/// Trans. Computers 60(2), 2011, algorithm 4); BASE is normalized (≥ 2^63).
const BASE_INV: u64 = (u128::MAX / BASE as u128 - (1u128 << 64)) as u64;

/// Products with an operand of at most this many limbs are schoolbook's: past it the
/// transforms measured cheaper at every size, s by s limbs and s by 64·s, on arm64 macOS
/// (from 73) and x86_64 Linux (from 164) (PM M537, `digits_crossover.rs`'s
/// `schoolbook_against_transforms`). The transforms' cost steps where their points
/// double, so they win in windows before that (112 to 128 on a Ryzen 9950X3D2).
const SCHOOL: usize = 163;

/// (u1·2^64 + u0) / BASE and its remainder, for u1 < BASE.
fn div_base(u1: u64, u0: u64) -> (u64, u64) {
    let q = (u128::from(BASE_INV) * u128::from(u1)).wrapping_add((u128::from(u1) << 64) | u128::from(u0));
    let mut q1 = ((q >> 64) as u64).wrapping_add(1);
    let mut r = u0.wrapping_sub(q1.wrapping_mul(BASE));
    if r > q as u64 {
        q1 = q1.wrapping_sub(1);
        r = r.wrapping_add(BASE);
    }
    if r >= BASE {
        q1 = q1.wrapping_add(1);
        r -= BASE;
    }
    (q1, r)
}

fn refused<T>(n: usize) -> Error {
    Error::Refused {
        bytes: u64::try_from(n)
            .unwrap_or(u64::MAX)
            .saturating_mul(size_of::<T>() as u64),
    }
}

/// An empty vector able to take `n` items without growing, or the allocator's refusal.
fn room<T>(n: usize) -> Result<Vec<T>, Error> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| refused::<T>(n))?;
    Ok(v)
}

/// A prime of the transforms, with Montgomery's constants for R = 2^64.
#[derive(Debug, Clone, Copy)]
struct Prime {
    p: u64,
    /// p^-1 mod 2^64.
    inv: u64,
    /// 2^128 mod p: Montgomery's form of 2^64.
    r2: u64,
    /// A generator of the multiplicative group mod p.
    g: u64,
}

const fn prime(p: u64, g: u64) -> Prime {
    // Newton's iteration doubles the low bits of p^-1 that are right, from the one bit
    // an odd p's has.
    let mut inv: u64 = 1;
    let mut i = 0;
    while i < 6 {
        inv = inv.wrapping_mul(2u64.wrapping_sub(p.wrapping_mul(inv)));
        i += 1;
    }
    let r = ((1u128 << 64) % p as u128) as u64;
    let r2 = ((r as u128 * r as u128) % p as u128) as u64;
    Prime { p, inv, r2, g }
}

/// b^e mod m.
const fn pow_mod(b: u64, mut e: u64, m: u64) -> u64 {
    let m = m as u128;
    let (mut r, mut b) = (1u128, b as u128 % m);
    while e > 0 {
        if e & 1 == 1 {
            r = r * b % m;
        }
        b = b * b % m;
        e >>= 1;
    }
    r as u64
}

/// a^-1 mod the prime m, in Montgomery's form mod m.
const fn inv_mont(a: u64, m: u64) -> u64 {
    let inv = pow_mod(a, m - 2, m);
    (((inv as u128) << 64) % m as u128) as u64
}

/// The three largest primes below 2^62 of the form c·2^32 + 1, each with a generator of
/// its group (checked by the tests): transforms of up to 2^32 points; four residues sum
/// below 2^64; and their product, past 2^185, exceeds every coefficient of a convolution
/// of 2^32 limbs, below 2^32·(10^19 − 1)^2 < 2^159.
const PRIMES: [Prime; 3] = [
    prime(0x3fff_ffee_0000_0001, 3),
    prime(0x3fff_ffb4_0000_0001, 19),
    prime(0x3fff_ffa0_0000_0001, 3),
];
const P0: u64 = PRIMES[0].p;
const P1: u64 = PRIMES[1].p;
const P2: u64 = PRIMES[2].p;
/// Garner's constants: P0^-1 mod P1, P0^-1 mod P2 and P1^-1 mod P2, in Montgomery's form.
const C01: u64 = inv_mont(P0, P1);
const C02: u64 = inv_mont(P0, P2);
const C12: u64 = inv_mont(P1, P2);
const P01: u128 = P0 as u128 * P1 as u128;
/// The most points a transform takes: 2^32 divides p − 1 for each prime.
const MAX_POINTS: usize = 1 << 32;

impl Prime {
    /// t·2^-64 mod p, for t < p·2^64: Montgomery's REDC, subtracting m·p, whose low word
    /// is t's, so that the difference of the high words is exact.
    fn redc(self, t: u128) -> u64 {
        let m = (t as u64).wrapping_mul(self.inv);
        let mp = u128::from(m) * u128::from(self.p);
        let (r, borrow) = ((t >> 64) as u64).overflowing_sub((mp >> 64) as u64);
        if borrow { r.wrapping_add(self.p) } else { r }
    }

    /// a·b·2^-64 mod p, for a·b < p·2^64.
    fn mul(self, a: u64, b: u64) -> u64 {
        self.redc(u128::from(a) * u128::from(b))
    }

    fn add(self, a: u64, b: u64) -> u64 {
        let s = a + b;
        if s >= self.p { s - self.p } else { s }
    }

    fn sub(self, a: u64, b: u64) -> u64 {
        if a >= b { a - b } else { a + self.p - b }
    }

    /// a·2^64 mod p, Montgomery's form of any a below 2^64 (a·r2 < 2^64·p).
    fn mont(self, a: u64) -> u64 {
        self.mul(a, self.r2)
    }

    /// b^e for b in Montgomery's form, in it.
    fn pow(self, mut b: u64, mut e: u64) -> u64 {
        let mut r = self.mont(1);
        while e > 0 {
            if e & 1 == 1 {
                r = self.mul(r, b);
            }
            b = self.mul(b, b);
            e >>= 1;
        }
        r
    }

    /// w^i for i < n/2 into `tw`, w a primitive n-th root of unity (n a power of two up
    /// to MAX_POINTS, so dividing p − 1), in Montgomery's form.
    fn twiddles(self, n: u64, tw: &mut [u64]) {
        let w = self.pow(self.mont(self.g), (self.p - 1) / n.max(1));
        let mut x = self.mont(1);
        for t in tw.iter_mut() {
            *t = x;
            x = self.mul(x, w);
        }
    }

    /// The transform of `a` (Gentleman and Sande's decimation in frequency), its points
    /// left in bit-reversed order.
    fn forward(self, a: &mut [u64], tw: &[u64]) {
        let mut half = a.len() / 2;
        let mut stride = 1;
        while half > 0 {
            for block in a.chunks_exact_mut(2 * half) {
                let (lo, hi) = block.split_at_mut(half);
                for ((u, v), &w) in lo.iter_mut().zip(hi.iter_mut()).zip(tw.iter().step_by(stride)) {
                    let (x, y) = (*u, *v);
                    *u = self.add(x, y);
                    *v = self.mul(self.sub(x, y), w);
                }
            }
            half /= 2;
            stride *= 2;
        }
    }

    /// The inverse of [`Prime::forward`] (Cooley and Tukey's decimation in time), times
    /// the transform's length, from bit-reversed order to natural. A span of 2h points
    /// takes w^-j = −w^(n/2 − j·n/2h): the forward twiddles, backwards and negated.
    fn inverse(self, a: &mut [u64], tw: &[u64]) {
        let n = a.len();
        let one = self.mont(1);
        let mut half = 1;
        while half < n {
            let stride = n / (2 * half);
            for block in a.chunks_exact_mut(2 * half) {
                let (lo, hi) = block.split_at_mut(half);
                let ws = std::iter::once(one).chain(
                    tw.iter()
                        .step_by(stride)
                        .skip(1)
                        .take(half - 1)
                        .rev()
                        .map(|&w| self.p - w),
                );
                for ((u, v), w) in lo.iter_mut().zip(hi.iter_mut()).zip(ws) {
                    let t = self.mul(*v, w);
                    let x = *u;
                    *u = self.add(x, t);
                    *v = self.sub(x, t);
                }
            }
            half *= 2;
        }
    }

    /// The cyclic convolution of a and b (of a with itself where b is None) modulo p, in
    /// `fa`'s points, natural form, with `fb` for b's points and `tw` (half as many) for
    /// the twiddles.
    fn residues(self, a: &[u64], b: Option<&[u64]>, fa: &mut [u64], fb: &mut [u64], tw: &mut [u64]) {
        let n = u64::try_from(fa.len()).unwrap_or(u64::MAX);
        self.twiddles(n, tw);
        let load = |x: &[u64], into: &mut [u64]| {
            let mut points = into.iter_mut();
            for (&l, p) in x.iter().zip(points.by_ref()) {
                *p = self.mont(l);
            }
            points.for_each(|p| *p = 0);
        };
        load(a, fa);
        self.forward(fa, tw);
        match b {
            Some(b) => {
                load(b, fb);
                self.forward(fb, tw);
                for (x, &y) in fa.iter_mut().zip(fb.iter()) {
                    *x = self.mul(*x, y);
                }
            }
            None => {
                for x in fa.iter_mut() {
                    *x = self.mul(*x, *x);
                }
            }
        }
        self.inverse(fa, tw);
        // 1/n = p − (p − 1)/n, n a power of two dividing p − 1: its Montgomery product
        // divides by n and leaves Montgomery's form.
        let scale = self.p - (self.p - 1) / n.max(1);
        for x in fa.iter_mut() {
            *x = self.mul(*x, scale);
        }
    }
}

/// A convolution coefficient from its residues, as three 64-bit words, least first:
/// x = r0 + P0·t1 + P0·P1·t2 (Garner), below P0·P1·P2.
fn garner(r0: u64, r1: u64, r2: u64) -> [u64; 3] {
    let [_, q1, q2] = PRIMES;
    // r0 < P0 < 2·P1 < 2·P2, and t1 < P1 < 2·P2: one subtraction reduces each.
    let t1 = q1.mul(q1.sub(r1, if r0 >= P1 { r0 - P1 } else { r0 }), C01);
    let u = q2.mul(q2.sub(r2, if r0 >= P2 { r0 - P2 } else { r0 }), C02);
    let t2 = q2.mul(q2.sub(u, if t1 >= P2 { t1 - P2 } else { t1 }), C12);
    let s = u128::from(P0) * u128::from(t1) + u128::from(r0);
    let a = u128::from(P01 as u64) * u128::from(t2);
    let b = u128::from((P01 >> 64) as u64) * u128::from(t2);
    let lo = u128::from(a as u64) + u128::from(s as u64);
    let mid = (a >> 64) + u128::from(b as u64) + (s >> 64) + (lo >> 64);
    let top = (b >> 64) + (mid >> 64);
    [lo as u64, mid as u64, top as u64]
}

/// Base-10^19 limbs from coefficients of three words, each at its limb, the carries
/// taken up: `len` limbs at most, the top zeros trimmed.
fn normalize(coefficients: impl Iterator<Item = [u64; 3]>, len: usize) -> Result<Vec<u64>, Error> {
    let mut out = room(len)?;
    // The carry stays below 2^187 / 10^19 < 2^124: two words.
    let mut carry: u128 = 0;
    for [x0, x1, x2] in coefficients {
        let lo = u128::from(x0) + u128::from(carry as u64);
        let mid = u128::from(x1) + (carry >> 64) + (lo >> 64);
        // Below 2^60 < BASE, as each coefficient is below 2^186.
        let top = x2.wrapping_add((mid >> 64) as u64);
        let (q1, r) = div_base(top, mid as u64);
        let (q0, limb) = div_base(r, lo as u64);
        out.push(limb);
        carry = (u128::from(q1) << 64) | u128::from(q0);
    }
    while carry > 0 {
        out.push((carry % u128::from(BASE)) as u64);
        carry /= u128::from(BASE);
    }
    trim(&mut out);
    Ok(out)
}

fn trim(v: &mut Vec<u64>) {
    while v.last() == Some(&0) {
        v.pop();
    }
}

/// a·b row by row, a the shorter: each cell below BASE² + BASE, so each carry below BASE.
fn school(a: &[u64], b: &[u64]) -> Result<Vec<u64>, Error> {
    let len = a.len() + b.len();
    let mut out = room(len)?;
    out.resize(len, 0);
    for (j, &y) in b.iter().enumerate() {
        let Some(row) = out.get_mut(j..) else {
            continue;
        };
        let mut carry = 0u64;
        let mut cells = row.iter_mut();
        // a first: zip takes from its first iterator first, and none past a's end.
        for (&x, cell) in a.iter().zip(cells.by_ref()) {
            let t = u128::from(*cell) + u128::from(x) * u128::from(y) + u128::from(carry);
            let (q, r) = div_base((t >> 64) as u64, t as u64);
            *cell = r;
            carry = q;
        }
        // No row before this one reached this cell.
        if let Some(cell) = cells.next() {
            *cell = carry;
        }
    }
    trim(&mut out);
    Ok(out)
}

/// The transforms' working words, made once at the size the largest of a conversion's
/// products asks and lent to each: a product's own buffers, given back and made again
/// larger at each level, stayed resident in macOS's allocator besides those in use, past
/// what the conversion needs at once (PM M537).
#[derive(Debug, Default)]
struct Scratch {
    words: Vec<u64>,
    /// The points of the conversion's largest transform, which the words are made for.
    most: usize,
}

impl Scratch {
    /// Words for a transform of n points ([`words_for`]).
    fn lend(&mut self, n: usize) -> Result<&mut [u64], Error> {
        let want = words_for(n);
        if self.words.len() < want {
            let len = want.max(words_for(self.most));
            self.words = Vec::new();
            self.words
                .try_reserve_exact(len)
                .map_err(|_| refused::<u64>(len))?;
            self.words.resize(len, 0);
        }
        self.words
            .get_mut(..want)
            .ok_or(Error::Go("number too long for its transforms"))
    }
}

/// A transform's words for n points: three residues and the second operand's, and half as
/// many for the twiddles.
fn words_for(n: usize) -> usize {
    n.saturating_mul(4).saturating_add(n / 2)
}

/// a·b (a² where b is None) by transforms: a convolution of their lengths' sum less one
/// points, rounded up to a power of two.
fn convolve(a: &[u64], b: Option<&[u64]>, scratch: &mut Scratch) -> Result<Vec<u64>, Error> {
    let len = a.len() + b.map_or(a.len(), <[u64]>::len);
    let coefficients = len.saturating_sub(1);
    let too_long = Error::Go("number too long for its transforms");
    let n = coefficients
        .checked_next_power_of_two()
        .filter(|&n| n <= MAX_POINTS)
        .ok_or(too_long.clone())?;
    let words = scratch.lend(n)?;
    let (r0, rest) = words.split_at_mut_checked(n).ok_or(too_long.clone())?;
    let (r1, rest) = rest.split_at_mut_checked(n).ok_or(too_long.clone())?;
    let (r2, rest) = rest.split_at_mut_checked(n).ok_or(too_long.clone())?;
    let (fb, tw) = rest.split_at_mut_checked(n).ok_or(too_long)?;
    let [q0, q1, q2] = PRIMES;
    q0.residues(a, b, r0, fb, tw);
    q1.residues(a, b, r1, fb, tw);
    q2.residues(a, b, r2, fb, tw);
    normalize(
        r0.iter()
            .zip(r1.iter())
            .zip(r2.iter())
            .take(coefficients)
            .map(|((&x, &y), &z)| garner(x, y, z)),
        len,
    )
}

/// a·b in base 10^19.
fn mul(a: &[u64], b: &[u64], scratch: &mut Scratch) -> Result<Vec<u64>, Error> {
    let (s, l) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if s.is_empty() {
        return Ok(Vec::new());
    }
    if s.len() <= SCHOOL {
        return school(s, l);
    }
    by_transforms(s, l, scratch)
}

/// s·l by transforms, s not longer than l, s not empty: by pieces of s's length where a
/// piece's transform is at most half the whole's (the product's limbs and a piece's
/// transforms then hold less than the whole's transforms would, [`need`]'s 4.5·N).
fn by_transforms(s: &[u64], l: &[u64], scratch: &mut Scratch) -> Result<Vec<u64>, Error> {
    let whole = (s.len() + l.len() - 1).checked_next_power_of_two();
    let piece = (2 * s.len() - 1).checked_next_power_of_two();
    if piece.zip(whole).is_none_or(|(p, w)| p > w / 2) {
        return convolve(s, Some(l), scratch);
    }
    let mut out = room(s.len() + l.len())?;
    for (i, piece) in l.chunks(s.len()).enumerate() {
        let p = convolve(s, Some(piece), scratch)?;
        add_at(&mut out, &p, i * s.len())?;
    }
    trim(&mut out);
    Ok(out)
}

fn square(a: &[u64], scratch: &mut Scratch) -> Result<Vec<u64>, Error> {
    if a.len() <= SCHOOL {
        return school(a, a);
    }
    convolve(a, None, scratch)
}

/// acc += b·BASE^at, growing acc only within its capacity where it can.
fn add_at(acc: &mut Vec<u64>, b: &[u64], at: usize) -> Result<(), Error> {
    let end = at + b.len();
    if acc.len() < end {
        if acc.capacity() < end {
            acc.try_reserve_exact(end - acc.len())
                .map_err(|_| refused::<u64>(end))?;
        }
        acc.resize(end, 0);
    }
    let mut carry = false;
    if let Some(tail) = acc.get_mut(at..) {
        let mut rest = tail.iter_mut();
        for (&y, x) in b.iter().zip(rest.by_ref()) {
            // Two limbs' sum may pass 2^64: x is compared with what y leaves below BASE.
            let c = u64::from(carry);
            let left = BASE - y - c;
            carry = *x >= left;
            *x = if carry { *x - left } else { *x + y + c };
        }
        for x in rest {
            if !carry {
                break;
            }
            carry = *x == BASE - 1;
            *x = if carry { 0 } else { *x + 1 };
        }
    }
    if carry {
        acc.push(1);
    }
    Ok(())
}

/// v − 1, v not zero.
fn less_one(mut v: Vec<u64>) -> Vec<u64> {
    for x in v.iter_mut() {
        if *x > 0 {
            *x -= 1;
            break;
        }
        *x = BASE - 1;
    }
    trim(&mut v);
    v
}

/// An aligned block of 2^k limbs.
#[derive(Debug)]
enum Block {
    Zero,
    /// Every bit one: 2^(64·2^k) − 1.
    Ones,
    /// Its value in base 10^19, least significant limb first, no top zero limbs.
    Value(Vec<u64>),
}

/// FastIntegerInput's state, the limbs taken least significant first.
#[derive(Debug, Default)]
struct Converter {
    /// Blocks not yet joined, of 2^k limbs each, k decreasing from the first (the lowest
    /// limbs): the set bits of `taken`, as a binary counter's.
    stack: Vec<(u32, Block)>,
    /// The limbs taken.
    taken: u64,
    /// T[k] in base 10^19, as far as made; None where let go.
    powers: Vec<Option<Vec<u64>>>,
    scratch: Scratch,
}

impl Converter {
    /// T[k], made by squaring the largest made.
    fn power(&mut self, k: u32) -> Result<&[u64], Error> {
        let k = usize::try_from(k).map_err(|_| Error::Go("number too long"))?;
        while self.powers.len() <= k {
            let next = match self.powers.last() {
                None => T0.to_vec(),
                Some(Some(t)) => square(t, &mut self.scratch)?,
                Some(None) => return Err(Error::Go("a power of two let go")),
            };
            self.powers.push(Some(next));
        }
        match self.powers.get(k) {
            Some(Some(t)) => Ok(t),
            _ => Err(Error::Go("a power of two let go")),
        }
    }

    /// x·T[k], T[k] made.
    fn times_power(&mut self, x: &[u64], k: u32) -> Result<Vec<u64>, Error> {
        let at = usize::try_from(k).map_err(|_| Error::Go("number too long"))?;
        match self.powers.get(at) {
            Some(Some(t)) => mul(x, t, &mut self.scratch),
            _ => Err(Error::Go("a power of two let go")),
        }
    }

    /// Lets go of every power below T[k] but the largest made, from which the rest square.
    fn let_go_below(&mut self, k: u32) {
        let keep = self.powers.len().saturating_sub(1);
        let below = usize::try_from(k).unwrap_or(usize::MAX).min(keep);
        for t in self.powers.iter_mut().take(below) {
            *t = None;
        }
    }

    /// A block's value.
    fn value(&mut self, k: u32, block: Block) -> Result<Vec<u64>, Error> {
        match block {
            Block::Zero => Ok(Vec::new()),
            Block::Value(v) => Ok(v),
            Block::Ones => {
                let t = self.power(k)?;
                let mut v = room(t.len())?;
                v.extend_from_slice(t);
                Ok(less_one(v))
            }
        }
    }

    /// The block of 2^(k+1) limbs whose halves are `upper` and `lower`. The product is at
    /// least as long as `lower`'s value, below T[k], and their sum fits it: no growth.
    fn join(&mut self, k: u32, upper: Block, lower: Block) -> Result<Block, Error> {
        Ok(match (upper, lower) {
            (Block::Zero, Block::Zero) => Block::Zero,
            (Block::Ones, Block::Ones) => Block::Ones,
            (Block::Zero, lower) => Block::Value(self.value(k, lower)?),
            (upper, lower) => {
                let upper = self.value(k, upper)?;
                self.power(k)?;
                let mut v = self.times_power(&upper, k)?;
                drop(upper);
                let lower = self.value(k, lower)?;
                add_at(&mut v, &lower, 0)?;
                Block::Value(v)
            }
        })
    }

    /// Takes a block of 2^k limbs, aligned at the limbs taken.
    fn push(&mut self, mut k: u32, mut block: Block) -> Result<(), Error> {
        let size = 1u64.checked_shl(k).ok_or(Error::Go("number too long"))?;
        self.taken = self.taken.checked_add(size).ok_or(Error::Go("number too long"))?;
        while let Some(&(top, _)) = self.stack.last()
            && top == k
        {
            let Some((_, lower)) = self.stack.pop() else {
                break;
            };
            block = self.join(k, block, lower)?;
            k += 1;
        }
        self.stack.push((k, block));
        Ok(())
    }

    /// Takes `count` limbs all zero, or all ones: as the largest aligned blocks they fill.
    fn uniform(&mut self, ones: bool, mut count: u64) -> Result<(), Error> {
        while count > 0 {
            let k = (63 - count.leading_zeros()).min(self.taken.trailing_zeros());
            self.push(k, if ones { Block::Ones } else { Block::Zero })?;
            count -= 1 << k;
        }
        Ok(())
    }

    fn limb(&mut self, x: u64) -> Result<(), Error> {
        match x {
            0 => self.uniform(false, 1),
            u64::MAX => self.uniform(true, 1),
            x if x < BASE => self.push(0, Block::Value(vec![x])),
            x => self.push(0, Block::Value(vec![x - BASE, 1])),
        }
    }

    /// Takes limbs, least significant first, runs of zeros and of ones as blocks.
    fn limbs(&mut self, limbs: impl Iterator<Item = u64>) -> Result<(), Error> {
        let (mut run, mut of) = (0u64, 0u64);
        for x in limbs {
            if run > 0 && x == of {
                run += 1;
                continue;
            }
            if run > 0 {
                self.uniform(of == u64::MAX, run)?;
                run = 0;
            }
            if x == 0 || x == u64::MAX {
                (run, of) = (1, x);
            } else {
                self.limb(x)?;
            }
        }
        if run > 0 {
            self.uniform(of == u64::MAX, run)?;
        }
        Ok(())
    }

    /// The integer of the limbs taken, in base 10^19: the blocks joined from the highest,
    /// the part above times T[k] plus each block of 2^k limbs below it.
    fn finish(mut self) -> Result<Vec<u64>, Error> {
        let mut acc: Option<Vec<u64>> = None;
        while let Some((k, block)) = self.stack.pop() {
            // No block of a lower level is left to join.
            self.let_go_below(k);
            acc = Some(match acc {
                None => self.value(k, block)?,
                Some(upper) => {
                    self.power(k)?;
                    let mut v = self.times_power(&upper, k)?;
                    drop(upper);
                    self.let_go_below(k);
                    let lower = self.value(k, block)?;
                    add_at(&mut v, &lower, 0)?;
                    v
                }
            });
        }
        Ok(acc.unwrap_or_default())
    }
}

/// The most limbs of base 10^19 a value of `bits` bits takes: ⌊bits·log10 2⌋ + 1
/// digits at most, rounded up to limbs, and one for f64's rounding of the product.
fn limbs_of(bits: u64) -> u64 {
    (bits as f64 * LOG10_2 / LIMB_DIGITS as f64) as u64 + 2
}

/// What a conversion of an integer of `bits` bits allocates at most, in words: the
/// powers T[j] made (T[0] 2 words, T[j] as many as T[j − 1] squared has limbs, twice its
/// own), the largest of them, the integer's value in limbs (the blocks on the stack and
/// a product's operand are disjoint parts of it: `limbs_of(bits)`, and a limb of rounding
/// and one of capacity a block), and the points of its largest transform (the power of
/// two at least a product's length less one, a product of parts being a part).
#[derive(Debug, Clone, Copy)]
struct Sizes {
    powers: u64,
    largest: u64,
    values: u64,
    points: u64,
}

fn sizes(bits: u64) -> Sizes {
    let n = bits.div_ceil(64).max(1);
    let top = 63 - n.leading_zeros();
    let (mut powers, mut largest) = (2u64, 2u64);
    for j in 1..=top {
        let below = 64u64.checked_shl(j - 1).map_or(u64::MAX, |b| b.saturating_add(1));
        largest = limbs_of(below).saturating_mul(2);
        powers = powers.saturating_add(largest);
    }
    let values = limbs_of(bits).saturating_add(2 * (u64::from(top) + 1));
    let points = values
        .saturating_add(1)
        .checked_next_power_of_two()
        .unwrap_or(u64::MAX);
    Sizes {
        powers,
        largest,
        values,
        points,
    }
}

/// The most bytes writing an integer of `bits` bits holds at its peak, its text with
/// it: an upper bound over the integers of that size, each part as the converter
/// allocates it (vectors' capacities), the integer's own limbs aside (its caller holds
/// them already). While converting, words of:
///
/// - the powers ([`Sizes`]: every one made, as if none were let go);
/// - the values: the stack's blocks and a product's operand; the product made from them,
///   as long; and as much again for what the allocator keeps of the results a chain of
///   products gave back, each at least twice the one before, so all together less than
///   the last;
/// - a copy of the largest power, for a block of ones;
/// - the transforms' scratch ([`Scratch`]), made once for the largest: 4.5 words a
///   point.
///
/// Then, on top of those (an allocator may keep what the conversion gave back while the
/// text is made: macOS's did, PM M537), the text, its digits (19 a limb, from the last
/// product's limbs) and a sign, and its caller's copy (`Rc<str>` from a String copies).
pub fn need(bits: u64) -> u64 {
    let z = sizes(bits);
    let scratch =
        u64::try_from(words_for(usize::try_from(z.points).unwrap_or(usize::MAX))).unwrap_or(u64::MAX);
    let words = z
        .powers
        .saturating_add(z.values.saturating_mul(3))
        .saturating_add(z.largest)
        .saturating_add(scratch);
    words
        .saturating_mul(8)
        .saturating_add(most(bits).saturating_mul(2))
}

/// The most bytes the text of an integer of `bits` bits takes: its decimal limbs' digits
/// at most ([`limbs_of`]), and a sign.
pub fn most(bits: u64) -> u64 {
    limbs_of(bits)
        .saturating_mul(LIMB_DIGITS as u64)
        .saturating_add(1)
}

/// The digits of `mant`·2^shift, after a `-` for `neg`, into `out`; refused where writing
/// them would hold more than `holdable` bytes ([`need`]).
pub fn write(out: &mut String, neg: bool, mant: &BigUint, shift: u64, holdable: u64) -> Result<(), Error> {
    let bits = mant.bits().saturating_add(shift);
    let need = need(bits);
    if need > holdable {
        return Err(Error::Unholdable { bits, need, holdable });
    }
    text(out, neg, &limbs_of_shifted(mant, shift)?)
}

/// mant·2^shift in base 10^19 by the converter, mant's limbs shifted as they go, never
/// made whole.
fn limbs_of_shifted(mant: &BigUint, shift: u64) -> Result<Vec<u64>, Error> {
    let mut c = Converter::default();
    let bits = mant.bits().saturating_add(shift);
    c.scratch.most = usize::try_from(sizes(bits).points).unwrap_or(usize::MAX);
    if mant.bits() > 0 {
        c.uniform(false, shift / 64)?;
        let s = (shift % 64) as u32;
        if s == 0 {
            c.limbs(mant.iter_u64_digits())?;
        } else {
            let mut carry = 0u64;
            let shifted = mant.iter_u64_digits().map(|x| {
                let y = (x << s) | carry;
                carry = x >> (64 - s);
                y
            });
            c.limbs(shifted)?;
            if carry != 0 {
                c.limb(carry)?;
            }
        }
    }
    c.finish()
}

/// The digits of base-10^19 limbs (least first, no top zero limbs), after a `-` for
/// `neg`, into `out`, grown once to their length.
fn text(out: &mut String, neg: bool, limbs: &[u64]) -> Result<(), Error> {
    let Some((&top, rest)) = limbs.split_last() else {
        out.push_str(if neg { "-0" } else { "0" });
        return Ok(());
    };
    let mut digits = [b'0'; LIMB_DIGITS];
    let lead = limb_digits(top, &mut digits);
    let len = (LIMB_DIGITS - lead) + rest.len() * LIMB_DIGITS + usize::from(neg);
    out.try_reserve_exact(len).map_err(|_| refused::<u8>(len))?;
    if neg {
        out.push('-');
    }
    out.extend(digits.iter().skip(lead).map(|&d| char::from(d)));
    for &x in rest.iter().rev() {
        limb_digits(x, &mut digits);
        out.extend(digits.iter().map(|&d| char::from(d)));
    }
    Ok(())
}

/// A limb's 19 digits, zero-padded, and how many of them lead as zeros (18 for 0).
fn limb_digits(mut x: u64, out: &mut [u8; LIMB_DIGITS]) -> usize {
    for d in out.iter_mut().rev() {
        *d = b'0' + (x % 10) as u8;
        x /= 10;
    }
    out.iter()
        .take(LIMB_DIGITS - 1)
        .take_while(|&&d| d == b'0')
        .count()
}

#[cfg(test)]
#[path = "../../../docs/research/measurements/rego-bignum/digits_crossover.rs"]
mod crossover;

#[cfg(test)]
mod tests {
    use super::*;

    /// splitmix64: draws repeat run to run.
    pub(super) struct Rng(pub(super) u64);

    impl Rng {
        pub(super) fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
    }

    /// The converter's digits, as `write` makes them.
    fn text(x: &BigUint, shift: u64) -> String {
        let mut out = String::new();
        super::text(&mut out, false, &limbs_of_shifted(x, shift).unwrap()).unwrap();
        out
    }

    #[test]
    fn primes_are_ntt_primes() {
        for q in PRIMES {
            assert!(q.p < 1 << 62);
            assert_eq!((q.p - 1) % (1 << 32), 0);
            // A generator: g^((p − 1)/f) ≠ 1 for each prime f of p − 1.
            let mut m = q.p - 1;
            let mut f = 2;
            let mut factors = Vec::new();
            while f * f <= m {
                if m % f == 0 {
                    factors.push(f);
                    while m % f == 0 {
                        m /= f;
                    }
                }
                f += 1;
            }
            if m > 1 {
                factors.push(m);
            }
            for f in factors {
                assert_ne!(pow_mod(q.g, (q.p - 1) / f, q.p), 1, "{} {f}", q.p);
            }
            // Prime: a strong probable prime to the first twelve primes' bases, which
            // decide every number below 3.3·10^24 (Sorenson and Webster 2017).
            let (mut d, mut s) = (q.p - 1, 0);
            while d % 2 == 0 {
                d /= 2;
                s += 1;
            }
            for a in [2u64, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37] {
                let mut x = pow_mod(a, d, q.p);
                let mut witness = x != 1 && x != q.p - 1;
                for _ in 1..s {
                    if !witness {
                        break;
                    }
                    x = ((u128::from(x) * u128::from(x)) % u128::from(q.p)) as u64;
                    witness = x != q.p - 1;
                }
                assert!(!witness, "{} is composite, witness {a}", q.p);
            }
            assert_eq!(q.inv.wrapping_mul(q.p), 1);
        }
    }

    #[test]
    fn division_by_the_base_is_exact() {
        let mut r = Rng(9);
        let mut cases: Vec<(u64, u64)> = (0..100_000).map(|_| (r.next() % BASE, r.next())).collect();
        cases.extend([
            (0, 0),
            (0, BASE - 1),
            (0, BASE),
            (0, u64::MAX),
            (BASE - 1, u64::MAX),
            (BASE - 1, 0),
        ]);
        for (u1, u0) in cases {
            let x = (u128::from(u1) << 64) | u128::from(u0);
            let want = ((x / u128::from(BASE)) as u64, (x % u128::from(BASE)) as u64);
            assert_eq!(div_base(u1, u0), want, "{x}");
        }
    }

    /// A random integer of `n` limbs in base 10^19.
    fn decimal(r: &mut Rng, n: usize) -> Vec<u64> {
        let mut v: Vec<u64> = (0..n).map(|_| r.next() % BASE).collect();
        if let Some(t) = v.last_mut() {
            *t = (*t).max(1);
        }
        v
    }

    fn to_big(v: &[u64]) -> BigUint {
        v.iter().rev().fold(BigUint::default(), |acc, &x| acc * BASE + x)
    }

    #[test]
    fn products_are_exact() {
        let mut r = Rng(3);
        for (la, lb) in [
            (1, 1),
            (3, 5),
            (24, 24),
            (25, 25),
            (25, 60),
            (33, 31),
            (100, 7),
            (64, 300),
            (500, 501),
            (1000, 3000),
        ] {
            let (a, b) = (decimal(&mut r, la), decimal(&mut r, lb));
            let want = to_big(&a) * to_big(&b);
            let mut z = Scratch::default();
            assert_eq!(to_big(&mul(&a, &b, &mut z).unwrap()), want, "{la}×{lb}");
            assert_eq!(to_big(&school(&a, &b).unwrap()), want, "{la}×{lb} by rows");
            assert_eq!(
                to_big(&convolve(&a, Some(&b), &mut z).unwrap()),
                want,
                "{la}×{lb} by transforms"
            );
            let sq = to_big(&a) * to_big(&a);
            assert_eq!(to_big(&square(&a, &mut z).unwrap()), sq, "{la}²");
            assert_eq!(
                to_big(&convolve(&a, None, &mut z).unwrap()),
                sq,
                "{la}² by transforms"
            );
        }
        // Every limb at its most: the largest coefficients the transforms meet.
        let a = vec![BASE - 1; 4096];
        let want = to_big(&a) * to_big(&a);
        assert_eq!(
            to_big(&convolve(&a, Some(&a), &mut Scratch::default()).unwrap()),
            want
        );
        assert_eq!(to_big(&school(&a, &a).unwrap()), want);
    }

    fn from_limbs(limbs: &[u64]) -> BigUint {
        BigUint::from_slice(
            &limbs
                .iter()
                .flat_map(|&x| [x as u32, (x >> 32) as u32])
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn integers_are_written_as_num_bigint_writes_them() {
        let mut r = Rng(5);
        let mut cases: Vec<(BigUint, u64)> = vec![
            (BigUint::default(), 0),
            (BigUint::from(1u32), 0),
            (BigUint::from(BASE - 1), 0),
            (BigUint::from(BASE), 0),
            (BigUint::from(u64::MAX), 0),
            (BigUint::from(1u32), 64),
            (BigUint::from(1u32), 6400),
            (BigUint::from(3u32), 100_000),
            (BigUint::from(u64::MAX), 77_777),
        ];
        for n in [1usize, 2, 3, 7, 8, 9, 31, 32, 33, 100, 255, 256, 257, 1000, 4097] {
            let limbs: Vec<u64> = (0..n).map(|_| r.next()).collect();
            cases.push((from_limbs(&limbs), 0));
            // Few nonzero limbs: a mantissa shifted, and two far apart.
            let m = BigUint::from(r.next() | 1 << 63);
            let s = r.next() % 300_000;
            cases.push((m.clone(), s));
            cases.push(((&m << s) + BigUint::from(r.next()), 0));
            // Runs of ones: 2^k − 1, and a mantissa times it.
            cases.push(((BigUint::from(1u32) << (s + 64)) - 1u32, 0));
            cases.push((((&m << (s + 64)) - 1u32) << 17, 3));
            // Limbs of ones among others.
            let mut mixed = limbs.clone();
            for x in mixed.iter_mut().step_by(3) {
                *x = u64::MAX;
            }
            cases.push((from_limbs(&mixed), 0));
        }
        for (x, shift) in &cases {
            let want = (x << *shift).to_str_radix(10);
            assert_eq!(text(x, *shift), want, "{} bits, shift {shift}", x.bits());
        }
    }

    #[test]
    fn a_number_past_what_the_evaluation_holds_is_refused() {
        let mut out = String::new();
        let x = BigUint::from(1u32);
        let e = write(&mut out, false, &x, 1 << 31, 1 << 30).unwrap_err();
        let Error::Unholdable { bits, need, holdable } = e else {
            panic!("{e:?}");
        };
        assert_eq!((bits, holdable), ((1 << 31) + 1, 1 << 30));
        assert!(need > holdable);
        assert!(out.is_empty());
        // At its need, written.
        let x = BigUint::from(12345u32);
        write(&mut out, true, &x, 1000, need_of(&x, 1000)).unwrap();
        assert_eq!(out, format!("-{}", (x << 1000u32).to_str_radix(10)));
    }

    fn need_of(x: &BigUint, shift: u64) -> u64 {
        need(x.bits() + shift)
    }
}

//! Go's `math/big.Float`, as far as OPA's numbers use it (go1.26 `float.go`,
//! `floatconv.go`, `ftoa.go`, `decimal.go`): OPA reads a number's text with
//! `SetString` at 64 bits, computes with `Add`, `Sub`, `Mul` and `Quo`, and writes the
//! result with `Text('g' | 'f', -1)` (topdown/builtins `NumberToFloat`, `FloatToNumber`).
//!
//! A finite value is `mant × 2^exp`, its mantissa at most `prec` bits. Go keeps the
//! mantissa normalized in words, which changes no result: each operation rounds the
//! exact result (or, for `Quo`, a quotient with a sticky bit) to `prec` bits, to
//! nearest, ties to even, as Go's `round` does. `SetString` is ported as Go does it,
//! powers of five at `prec + 64` bits included, so it rounds as Go rounds, twice where
//! Go does.

use std::cmp::Ordering;

use num_bigint::{BigInt, BigUint, Sign};

/// Why a number could not be read or computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Go's `MinExp` and `MaxExp`, bounds of `0.mant × 2^exp`.
const MIN_EXP: i64 = i32::MIN as i64;
const MAX_EXP: i64 = i32::MAX as i64;

/// A text Go's `SetString` refuses: its exponent is past what Go reads.
const OUT_OF_RANGE: Error = Error("exponent out of range");

/// A value Go rounds to ±Inf (`setExpAndRound`). OPA makes no infinities of its own,
/// so it is an error here; OPA's parser calls such a literal too big.
pub const OVERFLOW: Error = Error("exponent overflow");

/// OPA's bound on a number: a literal whose `MantExp` is past ±10^5 is refused (ast/parser.go
/// `parseNumber`, v1.14.1) because writing a big.Float's text costs more than linear time in
/// its exponent (golang/go#11068). Builtins here write no number text, and make no exact
/// integer, past it either (D98): OPA would write it, in time growing with the exponent's
/// value, from an exponent of a few digits.
pub const MAX_EXPONENT: i64 = 100_000;

/// What a number past `MAX_EXPONENT` is, in OPA's parser's words.
pub const TOO_BIG: Error = Error("number too big");

/// A `big.Float` that is zero or finite (OPA makes no infinities: it refuses division
/// by zero first, and what overflows Go's exponent is an error here).
#[derive(Debug, Clone)]
pub struct Float {
    prec: u32,
    neg: bool,
    /// Zero when the value is ±0.
    mant: BigUint,
    exp: i64,
}

fn bits(m: &BigUint) -> i64 {
    i64::try_from(m.bits()).unwrap_or(i64::MAX)
}

/// `m × 2^exp` as a mantissa at exponent `at`, and whether bits below `at` were dropped.
fn at_exponent(m: &BigUint, exp: i64, at: i64) -> (BigUint, bool) {
    let gap = exp.abs_diff(at);
    if exp >= at {
        return (m << gap, false);
    }
    let dropped = m.trailing_zeros().is_some_and(|tz| tz < gap);
    (m >> gap, dropped)
}

impl Float {
    fn zero(prec: u32, neg: bool) -> Float {
        Float {
            prec,
            neg,
            mant: BigUint::default(),
            exp: 0,
        }
    }

    pub fn is_zero(&self) -> bool {
        self.mant.bits() == 0
    }

    pub fn signbit(&self) -> bool {
        self.neg
    }

    /// `x.MantExp(nil)`: Go's exponent of `0.mant × 2^exp`, 0 for zero.
    pub fn go_exponent(&self) -> i64 {
        if self.is_zero() { 0 } else { self.go_exp() }
    }

    /// Go's exponent of `0.mant × 2^exp`.
    fn go_exp(&self) -> i64 {
        self.exp.saturating_add(bits(&self.mant))
    }

    /// Go's `round`, after `setExpAndRound`'s range checks.
    fn round(&mut self, sbit: bool) -> Result<(), Error> {
        if self.is_zero() {
            return Ok(());
        }
        let e = self.go_exp();
        if e < MIN_EXP {
            self.mant = BigUint::default();
            self.exp = 0;
            return Ok(());
        }
        if e > MAX_EXP {
            return Err(OVERFLOW);
        }
        let have = self.mant.bits();
        let prec = u64::from(self.prec);
        if have > prec {
            let cut = have - prec;
            let r = cut - 1;
            let rbit = self.mant.bit(r);
            let sticky = sbit || self.mant.trailing_zeros().is_some_and(|tz| tz < r);
            self.mant >>= cut;
            self.exp = self
                .exp
                .checked_add(i64::try_from(cut).map_err(|_| OVERFLOW)?)
                .ok_or(OVERFLOW)?;
            if rbit && (sticky || self.mant.bit(0)) {
                self.mant += 1u32;
                if self.mant.bits() > prec {
                    self.mant >>= 1u32;
                    self.exp = self.exp.checked_add(1).ok_or(OVERFLOW)?;
                }
            }
            if self.go_exp() > MAX_EXP {
                return Err(OVERFLOW);
            }
        }
        Ok(())
    }

    fn rounded(mut self, sbit: bool) -> Result<Float, Error> {
        self.round(sbit)?;
        Ok(self)
    }

    /// `new(Float).SetUint64(x)`, or with `prec`.
    pub fn from_u64(x: u64, prec: u32) -> Result<Float, Error> {
        let prec = if prec == 0 { 64 } else { prec };
        Float {
            prec,
            neg: false,
            mant: BigUint::from(x),
            exp: 0,
        }
        .rounded(false)
    }

    /// `big.NewFloat(x)` for the halves and ones OPA uses: 53 bits.
    pub fn from_f64_exact(neg: bool, mant: u64, exp: i64) -> Float {
        Float {
            prec: 53,
            neg,
            mant: BigUint::from(mant),
            exp,
        }
    }

    /// `new(Float).SetInt(x)`: the precision is the larger of x's bits and 64.
    pub fn from_int(x: &BigInt) -> Result<Float, Error> {
        let (sign, mag) = x.clone().into_parts();
        let prec = u32::try_from(mag.bits()).unwrap_or(u32::MAX).max(64);
        Float {
            prec,
            neg: sign == Sign::Minus,
            mant: mag,
            exp: 0,
        }
        .rounded(false)
    }

    /// `new(Float).SetString(s)`: Go's `Parse(s, 0)` at 64 bits.
    pub fn parse(s: &str) -> Result<Float, Error> {
        let (neg, mant, e2, e5) = exact_parts(s)?;
        let prec = 64;
        if mant.bits() == 0 {
            return Ok(Float::zero(prec, neg));
        }
        let mut z = Float {
            prec,
            neg,
            mant,
            exp: e2,
        };
        let ge = z.go_exp();
        if !(MIN_EXP..=MAX_EXP).contains(&ge) {
            return Err(OUT_OF_RANGE);
        }
        if e5 == 0 {
            z.round(false)?;
            return Ok(z);
        }
        let p = match pow5(e5.unsigned_abs(), prec + 64) {
            Ok(p) => p,
            // Go's 5^|e5| past its exponents is +Inf, and z over it is zero.
            Err(_) if e5 < 0 => return Ok(Float::zero(prec, neg)),
            Err(e) => return Err(e),
        };
        if e5 < 0 {
            Float::quo(&z, &p, prec)
        } else {
            Float::mul(&z, &p, prec)
        }
    }

    /// The precision a fresh `new(big.Float).Op(x, y)` takes.
    pub fn max_prec(x: &Float, y: &Float) -> u32 {
        x.prec.max(y.prec)
    }

    /// Go's `ucmp`: compares magnitudes.
    fn ucmp(x: &Float, y: &Float) -> Ordering {
        let (ex, ey) = (x.go_exp(), y.go_exp());
        if ex != ey {
            return ex.cmp(&ey);
        }
        // Same leading exponent: align and compare the mantissas.
        let (bx, by) = (x.mant.bits(), y.mant.bits());
        match bx.cmp(&by) {
            Ordering::Equal => x.mant.cmp(&y.mant),
            Ordering::Less => (&x.mant << (by - bx)).cmp(&y.mant),
            Ordering::Greater => x.mant.cmp(&(&y.mant << (bx - by))),
        }
    }

    /// Two nonzero magnitudes as mantissas at one exponent, for their sum or difference
    /// rounded to `prec` bits, and whether bits below that exponent were dropped. Go
    /// aligns to the lower exponent however far apart the two are (`uadd`, `usub`), a
    /// shift as long as the gap. The larger's mantissa (at most `prec` bits) is kept
    /// whole; the other's bits more than `prec + 2` below the larger's top are dropped
    /// for a sticky bit. That rounds as Go does: the sum is at least 2^(top - 1), and the
    /// difference with a smaller one that far below at least 2^(top - 2), so the round
    /// bit is at or above the exponent, and what is dropped is only ever below it.
    fn aligned(x: &Float, y: &Float, prec: u32) -> (BigUint, BigUint, i64, bool) {
        let top = x.go_exp().max(y.go_exp());
        let floor = top.saturating_sub(i64::from(prec)).saturating_sub(2);
        let at = x.exp.min(y.exp).max(floor);
        let (a, a_dropped) = at_exponent(&x.mant, x.exp, at);
        let (b, b_dropped) = at_exponent(&y.mant, y.exp, at);
        (a, b, at, a_dropped || b_dropped)
    }

    fn uadd(x: &Float, y: &Float, prec: u32, neg: bool) -> Result<Float, Error> {
        let (a, b, exp, dropped) = Float::aligned(x, y, prec);
        Float {
            prec,
            neg,
            mant: a + b,
            exp,
        }
        .rounded(dropped)
    }

    /// `|x| - |y|`, with `|x| > |y|`.
    fn usub(x: &Float, y: &Float, prec: u32, neg: bool) -> Result<Float, Error> {
        let (a, b, exp, dropped) = Float::aligned(x, y, prec);
        if a <= b {
            return Ok(Float::zero(prec, false));
        }
        // What y dropped is a fraction below the exponent: the difference is one less
        // there, and a fraction more.
        let d = a - b;
        let mant = if dropped { d - 1u32 } else { d };
        Float { prec, neg, mant, exp }.rounded(dropped)
    }

    /// `z.Set(x)` for a z of precision `prec`.
    fn set(x: &Float, prec: u32) -> Result<Float, Error> {
        Float { prec, ..x.clone() }.rounded(false)
    }

    /// `z.Add(x, y)` for a z of precision `prec`.
    pub fn add(x: &Float, y: &Float, prec: u32) -> Result<Float, Error> {
        if !x.is_zero() && !y.is_zero() {
            if x.neg == y.neg {
                return Float::uadd(x, y, prec, x.neg);
            }
            return if Float::ucmp(x, y) == Ordering::Greater {
                Float::usub(x, y, prec, x.neg)
            } else {
                Float::usub(y, x, prec, !x.neg)
            };
        }
        if x.is_zero() && y.is_zero() {
            return Ok(Float::zero(prec, x.neg && y.neg));
        }
        if y.is_zero() {
            Float::set(x, prec)
        } else {
            Float::set(y, prec)
        }
    }

    /// `z.Sub(x, y)` for a z of precision `prec`.
    pub fn sub(x: &Float, y: &Float, prec: u32) -> Result<Float, Error> {
        if !x.is_zero() && !y.is_zero() {
            if x.neg != y.neg {
                return Float::uadd(x, y, prec, x.neg);
            }
            return if Float::ucmp(x, y) == Ordering::Greater {
                Float::usub(x, y, prec, x.neg)
            } else {
                Float::usub(y, x, prec, !x.neg)
            };
        }
        if x.is_zero() && y.is_zero() {
            return Ok(Float::zero(prec, x.neg && !y.neg));
        }
        if y.is_zero() {
            return Float::set(x, prec);
        }
        let mut z = Float::set(y, prec)?;
        z.neg = !z.neg;
        Ok(z)
    }

    /// `z.Mul(x, y)` for a z of precision `prec`.
    pub fn mul(x: &Float, y: &Float, prec: u32) -> Result<Float, Error> {
        let neg = x.neg != y.neg;
        if x.is_zero() || y.is_zero() {
            return Ok(Float::zero(prec, neg));
        }
        let exp = x.exp.checked_add(y.exp).ok_or(OVERFLOW)?;
        Float {
            prec,
            neg,
            mant: &x.mant * &y.mant,
            exp,
        }
        .rounded(false)
    }

    /// `z.Quo(x, y)` for a z of precision `prec`; y is not zero (OPA checks first).
    pub fn quo(x: &Float, y: &Float, prec: u32) -> Result<Float, Error> {
        let neg = x.neg != y.neg;
        if y.is_zero() {
            return Err(Error("division by zero"));
        }
        if x.is_zero() {
            return Ok(Float::zero(prec, neg));
        }
        // A quotient of at least prec + 2 bits, the remainder as the sticky bit.
        let want = u64::from(prec) + 2;
        let (bx, by) = (x.mant.bits(), y.mant.bits());
        let shift = (want + by).saturating_sub(bx);
        let num = &x.mant << shift;
        let q = &num / &y.mant;
        let sticky = (&num % &y.mant).bits() != 0;
        let shift = i64::try_from(shift).map_err(|_| OVERFLOW)?;
        let exp = x
            .exp
            .checked_sub(y.exp)
            .and_then(|e| e.checked_sub(shift))
            .ok_or(OVERFLOW)?;
        Float {
            prec,
            neg,
            mant: q,
            exp,
        }
        .rounded(sticky)
    }

    /// `x.Abs(x)`.
    pub fn abs(mut self) -> Float {
        self.neg = false;
        self
    }

    /// `x.Cmp(y)`.
    pub fn compare(x: &Float, y: &Float) -> Ordering {
        let ord = |f: &Float| -> i32 {
            if f.is_zero() {
                0
            } else if f.neg {
                -1
            } else {
                1
            }
        };
        let (mx, my) = (ord(x), ord(y));
        match mx.cmp(&my) {
            Ordering::Equal => match mx {
                -1 => Float::ucmp(y, x),
                1 => Float::ucmp(x, y),
                _ => Ordering::Equal,
            },
            o => o,
        }
    }

    /// Go's `MinPrec`: the bits the mantissa needs.
    fn min_prec(&self) -> i64 {
        let tz = self.mant.trailing_zeros().unwrap_or(0);
        bits(&self.mant).saturating_sub(i64::try_from(tz).unwrap_or(0))
    }

    /// `x.IsInt()`.
    pub fn is_int(&self) -> bool {
        if self.is_zero() {
            return true;
        }
        let e = self.go_exp();
        if e <= 0 {
            return false;
        }
        i64::from(self.prec) <= e || self.min_prec() <= e
    }

    /// `x.Int(nil)`: truncated toward zero, and whether that is exact; past
    /// `MAX_EXPONENT`, refused, the integer as long as its exponent's value.
    pub fn int(&self) -> Result<(BigInt, bool), Error> {
        if self.is_zero() {
            return Ok((BigInt::default(), true));
        }
        if self.go_exp() > MAX_EXPONENT {
            return Err(TOO_BIG);
        }
        let exact = self.is_int();
        let mag = if self.exp >= 0 {
            &self.mant << u64::try_from(self.exp).unwrap_or(0)
        } else {
            &self.mant >> self.exp.unsigned_abs()
        };
        let sign = if self.neg { Sign::Minus } else { Sign::Plus };
        Ok((BigInt::from_biguint(sign, mag), exact))
    }

    /// `x.Text(format, -1)` for `'f'` and `'g'`: the shortest decimal that reads back as
    /// x; past `MAX_EXPONENT` either way, refused: Go's way, ported here, writes x's exact
    /// decimal first, as many digits as the exponent's value.
    pub fn text(&self, format: u8) -> Result<String, Error> {
        if !self.is_zero() && !(-MAX_EXPONENT..=MAX_EXPONENT).contains(&self.go_exp()) {
            return Err(TOO_BIG);
        }
        let mut out = String::new();
        if self.neg {
            out.push('-');
        }
        let mut d = Decimal::init(&self.mant, self.exp);
        round_shortest(&mut d, self);
        let len = i64::try_from(d.mant.len()).unwrap_or(i64::MAX);
        match format {
            b'f' => fmt_f(&mut out, (len - d.exp).max(0), &d),
            _ => {
                let prec = len;
                let exp = d.exp - 1;
                if !(-4..6).contains(&exp) {
                    fmt_e(&mut out, prec.min(len) - 1, &d);
                } else {
                    let p = if prec > d.exp { len } else { prec };
                    fmt_f(&mut out, (p - d.exp).max(0), &d);
                }
            }
        }
        Ok(out)
    }
}

/// The exact value of a number's text as Go's scanner reads it (base prefixes, `_`
/// separators, `e` and `p` exponents): its sign, mantissa, and the powers of two and
/// five it is multiplied by.
pub fn exact_parts(s: &str) -> Result<(bool, BigUint, i64, i64), Error> {
    let mut r = Reader {
        b: s.as_bytes(),
        at: 0,
    };
    let neg = match r.peek() {
        Some(b'-') => {
            r.at += 1;
            true
        }
        Some(b'+') => {
            r.at += 1;
            false
        }
        _ => false,
    };
    let (mant, base, fcount) = scan_mantissa(&mut r)?;
    let (exp, ebase) = scan_exponent(&mut r)?;
    if r.peek().is_some() {
        return Err(Error("expected end of string"));
    }
    let mut e2: i64 = 0;
    let mut e5: i64 = 0;
    if fcount < 0 {
        match base {
            10 => {
                e5 = fcount;
                e2 = fcount;
            }
            2 => e2 = fcount,
            8 => e2 = fcount.checked_mul(3).ok_or(OUT_OF_RANGE)?,
            _ => e2 = fcount.checked_mul(4).ok_or(OUT_OF_RANGE)?,
        }
    }
    if ebase == 10 {
        e5 = e5.checked_add(exp).ok_or(OUT_OF_RANGE)?;
    }
    e2 = e2.checked_add(exp).ok_or(OUT_OF_RANGE)?;
    Ok((neg, mant, e2, e5))
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }
    fn next(&mut self) -> Option<u8> {
        let c = self.peek();
        if c.is_some() {
            self.at += 1;
        }
        c
    }
}

/// `nat.scan(r, 0, true)`: the mantissa's digits, its base, and the count of digits
/// after the radix point as a negative number (0 when there is none).
fn scan_mantissa(r: &mut Reader<'_>) -> Result<(BigUint, u32, i64), Error> {
    let mut prev = b'.';
    let mut inval_sep = false;
    let mut b: u32 = 10;
    let mut count: i64 = 0;
    let mut frac_ok = true;
    let mut ch = r.next();
    if ch == Some(b'0') {
        prev = b'0';
        count = 1;
        ch = r.next();
        let prefix = match ch {
            Some(b'b' | b'B') => Some(2),
            Some(b'o' | b'O') => Some(8),
            Some(b'x' | b'X') => Some(16),
            _ => None,
        };
        if let Some(base) = prefix {
            b = base;
            count = 0;
            ch = r.next();
        }
    }
    let mut z = BigUint::default();
    let mut dp: i64 = -1;
    while let Some(c) = ch {
        if c == b'.' && frac_ok {
            frac_ok = false;
            if prev == b'_' {
                inval_sep = true;
            }
            prev = b'.';
            dp = count;
        } else if c == b'_' {
            if prev != b'0' {
                inval_sep = true;
            }
            prev = b'_';
        } else {
            let d1 = match c {
                b'0'..=b'9' => u32::from(c - b'0'),
                b'a'..=b'z' => u32::from(c - b'a') + 10,
                b'A'..=b'Z' => u32::from(c - b'A') + 10,
                _ => u32::MAX,
            };
            if d1 >= b {
                r.at -= 1;
                break;
            }
            prev = b'0';
            count += 1;
            z = z * b + d1;
        }
        ch = r.next();
    }
    if inval_sep || prev == b'_' {
        return Err(Error("'_' must separate successive digits"));
    }
    if count == 0 {
        return Err(Error("number has no digits"));
    }
    let fcount = if dp >= 0 { dp - count } else { 0 };
    Ok((z, b, fcount))
}

/// `scanExponent(r, true, true)`: the exponent and its base (10 for `e`, 2 for `p`).
fn scan_exponent(r: &mut Reader<'_>) -> Result<(i64, u32), Error> {
    let base = match r.next() {
        None => return Ok((0, 10)),
        Some(b'e' | b'E') => 10,
        Some(b'p' | b'P') => 2,
        Some(_) => {
            r.at -= 1;
            return Ok((0, 10));
        }
    };
    let mut digits = String::new();
    match r.peek() {
        Some(b'-') => {
            digits.push('-');
            r.at += 1;
        }
        Some(b'+') => r.at += 1,
        _ => {}
    }
    let mut prev = b'.';
    let mut inval_sep = false;
    let mut has = false;
    while let Some(c) = r.next() {
        if c.is_ascii_digit() {
            digits.push(char::from(c));
            prev = b'0';
            has = true;
        } else if c == b'_' {
            if prev != b'0' {
                inval_sep = true;
            }
            prev = b'_';
        } else {
            r.at -= 1;
            break;
        }
    }
    if !has {
        return Err(Error("number has no digits"));
    }
    let exp = digits.parse::<i64>().map_err(|_| OUT_OF_RANGE)?;
    if inval_sep || prev == b'_' {
        return Err(Error("'_' must separate successive digits"));
    }
    Ok((exp, base))
}

const POW5: [u64; 28] = [
    1,
    5,
    25,
    125,
    625,
    3125,
    15625,
    78125,
    390625,
    1953125,
    9765625,
    48828125,
    244140625,
    1220703125,
    6103515625,
    30517578125,
    152587890625,
    762939453125,
    3814697265625,
    19073486328125,
    95367431640625,
    476837158203125,
    2384185791015625,
    11920928955078125,
    59604644775390625,
    298023223876953125,
    1490116119384765625,
    7450580596923828125,
];

/// Go's `pow5` on a Float of precision `prec`. Go squares `f` once more after its last
/// use, which past its exponents only makes it +Inf, unread; that squaring is left out
/// here, where it would be an error.
fn pow5(n: u64, prec: u32) -> Result<Float, Error> {
    let m = (POW5.len() - 1) as u64;
    if n <= m {
        let v = POW5.get(usize::try_from(n).unwrap_or(0)).copied().unwrap_or(1);
        return Float::from_u64(v, prec);
    }
    let mut z = Float::from_u64(POW5.last().copied().unwrap_or(1), prec)?;
    let mut n = n - m;
    let fprec = prec + 64;
    let mut f = Float::from_u64(5, fprec)?;
    while n > 0 {
        if n & 1 != 0 {
            z = Float::mul(&z, &f, prec)?;
        }
        n >>= 1;
        if n > 0 {
            f = Float::mul(&f, &f, fprec)?;
        }
    }
    Ok(z)
}

/// Go's `decimal`: digits, big-endian, the radix point `exp` digits from the left.
#[derive(Debug, Clone, Default)]
struct Decimal {
    mant: Vec<u8>,
    exp: i64,
}

impl Decimal {
    /// The exact decimal of `m × 2^shift`, trailing zeros trimmed.
    fn init(m: &BigUint, shift: i64) -> Decimal {
        if m.bits() == 0 {
            return Decimal::default();
        }
        let (digits, frac) = if shift >= 0 {
            ((m << shift.unsigned_abs()).to_str_radix(10), 0i64)
        } else {
            let k = shift.unsigned_abs();
            let five = BigUint::from(5u32).pow(u32::try_from(k).unwrap_or(u32::MAX));
            ((m * five).to_str_radix(10), i64::try_from(k).unwrap_or(i64::MAX))
        };
        let mut mant = digits.into_bytes();
        let exp = i64::try_from(mant.len()).unwrap_or(i64::MAX) - frac;
        while mant.last() == Some(&b'0') {
            mant.pop();
        }
        let mut d = Decimal { mant, exp };
        d.trim();
        d
    }

    fn at(&self, i: i64) -> u8 {
        usize::try_from(i)
            .ok()
            .and_then(|i| self.mant.get(i))
            .copied()
            .unwrap_or(b'0')
    }

    fn len(&self) -> i64 {
        i64::try_from(self.mant.len()).unwrap_or(i64::MAX)
    }

    fn should_round_up(&self, n: usize) -> bool {
        let Some(&c) = self.mant.get(n) else { return false };
        if c == b'5' && n + 1 == self.mant.len() {
            return n > 0 && self.mant.get(n - 1).is_some_and(|p| (p - b'0') & 1 != 0);
        }
        c >= b'5'
    }

    fn round(&mut self, n: usize) {
        if n >= self.mant.len() {
            return;
        }
        if self.should_round_up(n) {
            self.round_up(n)
        } else {
            self.round_down(n)
        }
    }

    fn round_up(&mut self, n: usize) {
        if n >= self.mant.len() {
            return;
        }
        let mut n = n;
        while n > 0 && self.mant.get(n - 1).is_some_and(|&c| c >= b'9') {
            n -= 1;
        }
        if n == 0 {
            self.mant.truncate(1);
            if let Some(c) = self.mant.first_mut() {
                *c = b'1';
            }
            self.exp += 1;
            return;
        }
        if let Some(c) = self.mant.get_mut(n - 1) {
            *c += 1;
        }
        self.mant.truncate(n);
    }

    fn round_down(&mut self, n: usize) {
        if n >= self.mant.len() {
            return;
        }
        self.mant.truncate(n);
        self.trim();
    }

    fn trim(&mut self) {
        while self.mant.last() == Some(&b'0') {
            self.mant.pop();
        }
        if self.mant.is_empty() {
            self.exp = 0;
        }
    }
}

/// Go's `roundShortest`.
fn round_shortest(d: &mut Decimal, x: &Float) {
    if d.mant.is_empty() {
        return;
    }
    // x = mant × 2^exp with mant of prec + 1 bits: its lsb is half an ulp.
    let have = bits(&x.mant);
    let s = have - (i64::from(x.prec) + 1);
    let mant = if s < 0 {
        &x.mant << s.unsigned_abs()
    } else {
        &x.mant >> s.unsigned_abs()
    };
    let exp = x.exp + s;
    let lower = Decimal::init(&(&mant - 1u32), exp);
    let upper = Decimal::init(&(&mant + 1u32), exp);
    let inclusive = !mant.bit(1);
    let mut i = 0usize;
    while let Some(&m) = d.mant.get(i) {
        let ii = i64::try_from(i).unwrap_or(i64::MAX);
        let l = lower.at(ii);
        let u = upper.at(ii);
        let okdown = l != m || inclusive && ii + 1 == lower.len();
        let okup = m != u && (inclusive || m + 1 < u || ii + 1 < upper.len());
        match (okdown, okup) {
            (true, true) => {
                d.round(i + 1);
                return;
            }
            (true, false) => {
                d.round_down(i + 1);
                return;
            }
            (false, true) => {
                d.round_up(i + 1);
                return;
            }
            (false, false) => {}
        }
        i += 1;
    }
}

/// Go's `fmtE` (with `'e'`).
fn fmt_e(out: &mut String, prec: i64, d: &Decimal) {
    out.push(char::from(d.mant.first().copied().unwrap_or(b'0')));
    if prec > 0 {
        out.push('.');
        let mut i: i64 = 1;
        let m = d.len().min(prec + 1);
        while i < m {
            out.push(char::from(d.at(i)));
            i += 1;
        }
        while i <= prec {
            out.push('0');
            i += 1;
        }
    }
    out.push('e');
    let mut exp = if d.mant.is_empty() { 0 } else { d.exp - 1 };
    if exp < 0 {
        out.push('-');
        exp = -exp;
    } else {
        out.push('+');
    }
    if exp < 10 {
        out.push('0');
    }
    out.push_str(&exp.to_string());
}

/// Go's `fmtF`.
fn fmt_f(out: &mut String, prec: i64, d: &Decimal) {
    if d.exp > 0 {
        let m = d.len().min(d.exp);
        let mut i = 0;
        while i < m {
            out.push(char::from(d.at(i)));
            i += 1;
        }
        while i < d.exp {
            out.push('0');
            i += 1;
        }
    } else {
        out.push('0');
    }
    if prec > 0 {
        out.push('.');
        for i in 0..prec {
            out.push(char::from(d.at(d.exp + i)));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use num_bigint::BigUint;

    use super::{Float, MAX_EXPONENT, OUT_OF_RANGE, OVERFLOW, TOO_BIG};

    /// Go's alignment, as it was here: the lower exponent, however far.
    fn aligned_whole(x: &Float, y: &Float) -> (BigUint, BigUint, i64) {
        let at = x.exp.min(y.exp);
        let up = |f: &Float| &f.mant << u64::try_from(f.exp - at).unwrap();
        (up(x), up(y), at)
    }

    /// `x + y`, or `x - y` with `sub`, as Go makes it: exact, then rounded once.
    fn go_add(x: &Float, y: &Float, prec: u32, sub: bool) -> Float {
        let y_neg = y.neg != sub;
        let (a, b, exp) = aligned_whole(x, y);
        let (neg, mant) = if x.neg == y_neg {
            (x.neg, a + b)
        } else if a > b {
            (x.neg, a - b)
        } else if b > a {
            (y_neg, b - a)
        } else {
            return Float::zero(prec, false);
        };
        Float { prec, neg, mant, exp }.rounded(false).unwrap()
    }

    /// xorshift64*: the same cases on every run.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
        }

        /// A mantissa of at most `prec` bits: all ones, a power of two, one past it, or
        /// random bits.
        fn mantissa(&mut self, prec: u32) -> BigUint {
            let bits = 1 + self.below(u64::from(prec));
            let top = BigUint::from(1u32) << (bits - 1);
            match self.below(5) {
                0 => (&top << 1u32) - 1u32,
                1 => top,
                2 => top + 1u32,
                _ => (0..bits).fold(BigUint::default(), |m, _| (m << 1u32) + self.below(2)) | top,
            }
        }

        fn float(&mut self, prec: u32, exp: i64) -> Float {
            let (neg, mant) = (self.below(2) == 0, self.mantissa(prec));
            Float { prec, neg, mant, exp }.rounded(false).unwrap()
        }
    }

    /// Sums and differences with the smaller's bits dropped below the larger's
    /// precision round as the exact ones do: ties, carries, borrows, gaps on both sides
    /// of where the dropping starts.
    #[test]
    fn sums_and_differences_round_as_the_exact_ones() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut checked, mut dropping) = (0, 0);
        for _ in 0..40000 {
            let prec = [53, 64, 100, 200][usize::try_from(rng.below(4)).unwrap()];
            let x = rng.float(prec, 0);
            let gap = i64::try_from(rng.below(u64::from(prec) * 3)).unwrap() - i64::from(prec);
            let y = rng.float(prec, -gap);
            dropping += usize::from(Float::aligned(&x, &y, prec).3);
            for (a, b) in [(&x, &y), (&y, &x)] {
                for sub in [false, true] {
                    let got = if sub {
                        Float::sub(a, b, prec)
                    } else {
                        Float::add(a, b, prec)
                    }
                    .unwrap();
                    let want = go_add(a, b, prec, sub);
                    let op = if sub { "-" } else { "+" };
                    assert!(
                        Float::compare(&got, &want) == Ordering::Equal && got.neg == want.neg,
                        "{a:?} {op} {b:?}: {got:?}, Go {want:?}"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 160000);
        assert!(
            (5000..35000).contains(&dropping),
            "{dropping} of 40000 pairs dropped bits"
        );
    }

    /// Exponents 1.2 billion apart align in the precision's bits, not the gap's.
    #[test]
    fn alignment_is_bounded_by_the_precision_not_the_gap() {
        let (x, y) = (
            Float::parse("1e600000000").unwrap(),
            Float::parse("1e-600000000").unwrap(),
        );
        let (a, b, _, dropped) = Float::aligned(&x, &y, 64);
        assert!(
            a.bits() <= 64 + 2 && b.bits() <= 64 + 2 && dropped,
            "{} {}",
            a.bits(),
            b.bits()
        );
        for z in [Float::add(&x, &y, 64).unwrap(), Float::sub(&x, &y, 64).unwrap()] {
            assert_eq!(Float::compare(&z, &x), Ordering::Equal);
        }
    }

    /// As Go 1.26.1's SetString reads these (MantExp, ±Inf, ±0, refused).
    #[test]
    fn huge_exponents_read_as_go_reads_them() {
        // 5^(6e8) is finite; Go squares its pow5's factor once more past MaxExp, unread.
        assert_eq!(Float::parse("1e600000000").unwrap().go_exponent(), 1993156857);
        assert_eq!(Float::parse("1e-600000000").unwrap().go_exponent(), -1993156856);
        // Go's +Inf.
        assert_eq!(Float::parse("1e700000000").unwrap_err(), OVERFLOW);
        // Go's 5^(1e9) is +Inf, and 1 over it -0 here.
        let z = Float::parse("-1e-1000000000").unwrap();
        assert!(z.is_zero() && z.signbit());
        let z = Float::parse("1e-700000000").unwrap();
        assert!(z.is_zero() && !z.signbit());
        // Past MaxExp before any scaling: Go's SetString refuses it.
        assert_eq!(Float::parse("1e3000000000").unwrap_err(), OUT_OF_RANGE);
    }

    /// Texts and integers end where OPA's parser does, MantExp ±1e5, both inclusive.
    #[test]
    fn texts_and_integers_end_at_opas_bound() {
        let f = |s: &str| Float::parse(s).unwrap();
        for s in ["0x1p99999", "-0x1p99999", "0x1p-100001"] {
            assert!(
                f(s).go_exponent().abs() == MAX_EXPONENT && f(s).text(b'g').is_ok(),
                "{s}"
            );
        }
        for s in ["0x1p100000", "0x1p-100002"] {
            assert_eq!(f(s).text(b'g'), Err(TOO_BIG), "{s}");
        }
        assert_eq!(f("0x1p99999").int().unwrap().0.bits(), 100000);
        assert_eq!(f("0x1p100000").int().unwrap_err(), TOO_BIG);
        assert_eq!(f("0x1p-100002").int().unwrap(), (Default::default(), false));
        assert_eq!(f("0e99999").text(b'f').unwrap(), "0");
    }
}

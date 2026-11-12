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

use std::borrow::Cow;
use std::cmp::Ordering;
use std::f64::consts::{LOG2_10, LOG10_2};

use num_bigint::{BigInt, BigUint, Sign};

/// Why a number could not be read, computed or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// What Go's big.Float refuses, in its words, or what this port refuses where Go
    /// would make ±Inf.
    Go(&'static str),
    /// Writing a number of `bits` bits would hold `need` bytes, past the `holdable` the
    /// evaluation can (D98's number-text bound).
    Unholdable { bits: u64, need: u64, holdable: u64 },
    /// The allocator refused `bytes` that the bound had allowed.
    Refused { bytes: u64 },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Go(s) => f.write_str(s),
            Error::Unholdable {
                bits,
                need,
                holdable,
            } => write!(
                f,
                "number too large to write: the text of a {bits}-bit number takes {need} bytes \
                 to make, more than the {holdable} bytes this evaluation can hold"
            ),
            Error::Refused { bytes } => write!(f, "number too large to write: {bytes} bytes refused"),
        }
    }
}

/// Go's `MinExp` and `MaxExp`, bounds of `0.mant × 2^exp`.
const MIN_EXP: i64 = i32::MIN as i64;
const MAX_EXP: i64 = i32::MAX as i64;

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
            return Err(Error::Go("exponent overflow"));
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
                .checked_add(i64::try_from(cut).map_err(|_| Error::Go("exponent overflow"))?)
                .ok_or(Error::Go("exponent overflow"))?;
            if rbit && (sticky || self.mant.bit(0)) {
                self.mant += 1u32;
                if self.mant.bits() > prec {
                    self.mant >>= 1u32;
                    self.exp = self.exp.checked_add(1).ok_or(Error::Go("exponent overflow"))?;
                }
            }
            if self.go_exp() > MAX_EXP {
                return Err(Error::Go("exponent overflow"));
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
            return Err(Error::Go("exponent overflow"));
        }
        if e5 == 0 {
            z.round(false)?;
            return Ok(z);
        }
        match (pow5(e5.unsigned_abs(), prec + 64)?, e5 < 0) {
            (Some(p), true) => Float::quo(&z, &p, prec),
            (Some(p), false) => Float::mul(&z, &p, prec),
            // Go's x / +Inf is ±0, of x's sign.
            (None, true) => Ok(Float::zero(prec, neg)),
            // x · +Inf is ±Inf in Go, which this port refuses.
            (None, false) => Err(Error::Go("exponent overflow")),
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

    /// The exact sum of two magnitudes, aligned to the lower exponent.
    fn aligned(x: &Float, y: &Float) -> (BigUint, BigUint, i64) {
        match x.exp.cmp(&y.exp) {
            Ordering::Less => {
                let shift = u64::try_from(y.exp - x.exp).unwrap_or(0);
                (x.mant.clone(), &y.mant << shift, x.exp)
            }
            Ordering::Greater => {
                let shift = u64::try_from(x.exp - y.exp).unwrap_or(0);
                (&x.mant << shift, y.mant.clone(), y.exp)
            }
            Ordering::Equal => (x.mant.clone(), y.mant.clone(), x.exp),
        }
    }

    fn uadd(x: &Float, y: &Float, prec: u32, neg: bool) -> Result<Float, Error> {
        let (a, b, exp) = Float::aligned(x, y);
        Float {
            prec,
            neg,
            mant: a + b,
            exp,
        }
        .rounded(false)
    }

    /// `|x| - |y|`, with `|x| > |y|`.
    fn usub(x: &Float, y: &Float, prec: u32, neg: bool) -> Result<Float, Error> {
        let (a, b, exp) = Float::aligned(x, y);
        if a <= b {
            return Ok(Float::zero(prec, false));
        }
        Float {
            prec,
            neg,
            mant: a - b,
            exp,
        }
        .rounded(false)
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
        let exp = x.exp.checked_add(y.exp).ok_or(Error::Go("exponent overflow"))?;
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
            return Err(Error::Go("division by zero"));
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
        let shift = i64::try_from(shift).map_err(|_| Error::Go("exponent overflow"))?;
        let exp = x
            .exp
            .checked_sub(y.exp)
            .and_then(|e| e.checked_sub(shift))
            .ok_or(Error::Go("exponent overflow"))?;
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

    /// `x.Int(nil)`: truncated toward zero, and whether that is exact.
    pub fn int(&self) -> (BigInt, bool) {
        if self.is_zero() {
            return (BigInt::default(), true);
        }
        let exact = self.is_int();
        let mag = if self.exp >= 0 {
            &self.mant << u64::try_from(self.exp).unwrap_or(0)
        } else {
            &self.mant >> self.exp.unsigned_abs()
        };
        let sign = if self.neg { Sign::Minus } else { Sign::Plus };
        (BigInt::from_biguint(sign, mag), exact)
    }

    /// x's precision.
    pub fn precision(&self) -> u32 {
        self.prec
    }

    /// x's mantissa: x is ±mant·2^(go_exponent − mant's bits).
    pub fn mantissa(&self) -> &BigUint {
        &self.mant
    }

    /// x's magnitude truncated toward zero, as m·2^shift: x.Int(nil)'s, never made whole.
    pub fn truncated(&self) -> (Cow<'_, BigUint>, u64) {
        match u64::try_from(self.exp) {
            Ok(shift) => (Cow::Borrowed(&self.mant), shift),
            Err(_) => (Cow::Owned(&self.mant >> self.exp.unsigned_abs()), 0),
        }
    }

    /// `x.Int(nil).String()`: x truncated toward zero, every digit (format_int's and
    /// units.parse_bytes' answers, and floor's, ceil's and round's of an integer past its
    /// halves); refused where writing it would hold more than `holdable` bytes.
    pub fn int_text(&self, holdable: u64) -> Result<String, Error> {
        let (mant, shift) = self.truncated();
        let mut out = String::new();
        crate::digits::write(&mut out, self.neg && mant.bits() > 0, &mant, shift, holdable)?;
        Ok(out)
    }

    /// `x.Text(format, -1)` for `'f'` and `'g'`: the shortest decimal that reads back as x;
    /// refused where writing it would hold more than `holdable` bytes.
    ///
    /// An integer whose precision holds it whole (an ulp of at most 1) is its own shortest
    /// decimal: its neighbours half an ulp away are within one half of it, where no other
    /// integer is, and no decimal of as few digits but an integer. Its digits are made
    /// once, by `digits` (Go makes it and both neighbours whole: three conversions, as
    /// long as the answer each).
    pub fn text(&self, format: u8, holdable: u64) -> Result<String, Error> {
        if self.is_int() && !self.is_zero() && self.go_exp() <= i64::from(self.prec) {
            if format == b'f' {
                return self.int_text(holdable);
            }
            // Its digits, then Go's %g of them, both held with the text's copy.
            let (mant, shift) = self.truncated();
            let bits = mant.bits().saturating_add(shift);
            let need = crate::digits::need(bits).saturating_add(crate::digits::most(bits));
            if need > holdable {
                return Err(Error::Unholdable {
                    bits,
                    need,
                    holdable,
                });
            }
            let mut digits = String::new();
            crate::digits::write(&mut digits, false, &mant, shift, holdable)?;
            let mut mant = digits.into_bytes();
            let exp = len_i64(mant.len());
            while mant.last() == Some(&b'0') {
                mant.pop();
            }
            let sig = len_i64(mant.len());
            return self.written(&Decimal { mant, exp, sig }, format, holdable);
        }
        self.written(&self.shortest(), format, holdable)
    }

    /// Go's fmtF and fmtE of d, as Text(format, -1) picks them, into a string grown once,
    /// where it and the caller's copy of it fit in `holdable`.
    fn written(&self, d: &Decimal, format: u8, holdable: u64) -> Result<String, Error> {
        let len = d.len();
        // %g of the shortest decimal takes %e for an exponent below −4 or past 5 (Go's
        // eprec of 6 for the shortest).
        let exponent = format != b'f' && !(-4..6).contains(&(d.exp - 1));
        let prec = if exponent { len - 1 } else { (len - d.exp).max(0) };
        let frac = if prec > 0 { prec.saturating_add(1) } else { 0 };
        let whole = if exponent { 1 } else { d.exp.max(1) };
        // The exponent's sign and digits, at most 2 + 19.
        let tail = if exponent { 21 } else { 0 };
        let size = whole
            .saturating_add(frac)
            .saturating_add(tail)
            .saturating_add(i64::from(self.neg));
        let size = u64::try_from(size).unwrap_or(u64::MAX);
        let need = size.saturating_mul(2);
        if need > holdable {
            return Err(Error::Unholdable {
                bits: u64::try_from(self.go_exp().max(0)).unwrap_or(0),
                need,
                holdable,
            });
        }
        let mut out = String::new();
        let reserve = usize::try_from(size).map_err(|_| Error::Refused { bytes: size })?;
        out.try_reserve_exact(reserve)
            .map_err(|_| Error::Refused { bytes: size })?;
        if self.neg {
            out.push('-');
        }
        if exponent {
            fmt_e(&mut out, prec, d);
        } else {
            fmt_f(&mut out, prec, d);
        }
        Ok(out)
    }

    /// Go's `d.init(x.mant, exp)` and `roundShortest(&d, x)`: the shortest decimal that
    /// reads back as x. Go computes x's exact decimal and its neighbours' half an ulp away
    /// whole; where they are long, only the digits roundShortest reads of them are
    /// computed (`leading`).
    fn shortest(&self) -> Decimal {
        if self.is_zero() {
            return Decimal::default();
        }
        let (m, e) = self.halves();
        let read = digits_read(self.prec);
        // m × 2^e is x unless halves cut bits off, as no rounded float's mantissa is longer
        // than its precision.
        if bits(&self.mant) <= i64::from(self.prec) + 1
            && decimal_len(&m, e) >= LEADING_FROM * read as f64
            && let Some(d) = shortest_leading(&m, e, read)
        {
            return d;
        }
        self.shortest_exact(&m, e)
    }

    /// x = mant × 2^exp as m × 2^e, m of prec + 1 bits: its lsb is half an ulp. m is not
    /// zero where x is not, as a shift right keeps prec + 1 bits.
    fn halves(&self) -> (BigUint, i64) {
        let s = bits(&self.mant) - (i64::from(self.prec) + 1);
        let m = if s < 0 {
            &self.mant << s.unsigned_abs()
        } else {
            &self.mant >> s.unsigned_abs()
        };
        (m, self.exp.saturating_add(s))
    }

    /// Go's way: x's exact decimal and its neighbours', whole.
    fn shortest_exact(&self, m: &BigUint, e: i64) -> Decimal {
        let mut d = Decimal::init(&self.mant, self.exp);
        let lower = Decimal::init(&(m - 1u32), e);
        let upper = Decimal::init(&(m + 1u32), e);
        round_shortest(&mut d, &lower, &upper, !m.bit(1));
        d
    }
}

/// The shortest decimal of x = m × 2^e from the leading digits of its decimal and of its
/// neighbours' (`leading`), which `digits_read`'s settle. The neighbours are possible
/// outputs only for an even mantissa (ties to even).
fn shortest_leading(m: &BigUint, e: i64, read: i64) -> Option<Decimal> {
    let [lower, mut d, upper] = leading([&(m - 1u32), m, &(m + 1u32)], e, read)?;
    round_shortest(&mut d, &lower, &upper, !m.bit(1)).then_some(d)
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
        return Err(Error::Go("expected end of string"));
    }
    let overflow = Error::Go("exponent overflow");
    let mut e2: i64 = 0;
    let mut e5: i64 = 0;
    if fcount < 0 {
        match base {
            10 => {
                e5 = fcount;
                e2 = fcount;
            }
            2 => e2 = fcount,
            8 => e2 = fcount.checked_mul(3).ok_or(overflow.clone())?,
            _ => e2 = fcount.checked_mul(4).ok_or(overflow.clone())?,
        }
    }
    if ebase == 10 {
        e5 = e5.checked_add(exp).ok_or(overflow.clone())?;
    }
    e2 = e2.checked_add(exp).ok_or(overflow)?;
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
        return Err(Error::Go("'_' must separate successive digits"));
    }
    if count == 0 {
        return Err(Error::Go("number has no digits"));
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
        return Err(Error::Go("number has no digits"));
    }
    let exp = digits
        .parse::<i64>()
        .map_err(|_| Error::Go("exponent out of range"))?;
    if inval_sep || prev == b'_' {
        return Err(Error::Go("'_' must separate successive digits"));
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

/// Go's `pow5` on a Float of precision `prec`; None where Go's is +Inf: a product past
/// MaxExp is +Inf in Go (setExpAndRound), and so is every product by it. Go squares its
/// factor once more after the last bit it uses; that square passing MaxExp changes
/// nothing, as Go's +Inf there is never used.
fn pow5(n: u64, prec: u32) -> Result<Option<Float>, Error> {
    let m = (POW5.len() - 1) as u64;
    if n <= m {
        let v = POW5.get(usize::try_from(n).unwrap_or(0)).copied().unwrap_or(1);
        return Float::from_u64(v, prec).map(Some);
    }
    let mut z = Some(Float::from_u64(POW5.last().copied().unwrap_or(1), prec)?);
    let mut n = n - m;
    let fprec = prec + 64;
    let mut f = Some(Float::from_u64(5, fprec)?);
    // Float::mul's only error is its exponent's overflow: Go's +Inf.
    while n > 0 {
        if n & 1 != 0 {
            z = z.zip(f.as_ref()).and_then(|(z, f)| Float::mul(&z, f, prec).ok());
        }
        f = f.and_then(|f| Float::mul(&f, &f, fprec).ok());
        n >>= 1;
    }
    Ok(z)
}

/// Go's `decimal`: digits, big-endian, the radix point `exp` digits from the left, `sig`
/// significant digits in all. `mant` holds them all, or (from `leading`) the first of them
/// only, past which no digit is known.
#[derive(Debug, Clone, Default)]
struct Decimal {
    mant: Vec<u8>,
    exp: i64,
    sig: i64,
}

fn len_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
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
        let mant = digits.into_bytes();
        let exp = len_i64(mant.len()) - frac;
        let mut d = Decimal { mant, exp, sig: 0 };
        d.trim();
        d
    }

    /// Go's `at`: the digit at `i`, '0' outside the significant ones; None where it is not
    /// known.
    fn at(&self, i: i64) -> Option<u8> {
        if i < 0 || i >= self.sig {
            return Some(b'0');
        }
        usize::try_from(i).ok().and_then(|i| self.mant.get(i)).copied()
    }

    /// The digit at `i` of a decimal whose digits are all known.
    fn digit(&self, i: i64) -> u8 {
        self.at(i).unwrap_or(b'0')
    }

    fn len(&self) -> i64 {
        self.sig
    }

    /// Go's `shouldRoundUp`; None where it reads a digit not known.
    fn should_round_up(&self, n: i64) -> Option<bool> {
        let c = self.at(n)?;
        if c == b'5' && n + 1 == self.sig {
            return Some(n > 0 && (self.at(n - 1)? - b'0') & 1 != 0);
        }
        Some(c >= b'5')
    }

    /// Go's `round`, `roundUp` and `roundDown` to `n` digits; false where they would read
    /// a digit not known.
    fn round(&mut self, n: i64) -> bool {
        if n < 0 || n >= self.sig {
            return true;
        }
        match self.should_round_up(n) {
            Some(true) => self.round_up(n),
            Some(false) => self.round_down(n),
            None => false,
        }
    }

    fn round_up(&mut self, n: i64) -> bool {
        if n < 0 || n >= self.sig {
            return true;
        }
        let Some(mut n) = usize::try_from(n).ok().filter(|&n| n <= self.mant.len()) else {
            return false;
        };
        while n > 0 && self.mant.get(n - 1).is_some_and(|&c| c >= b'9') {
            n -= 1;
        }
        if n == 0 {
            self.mant.clear();
            self.mant.push(b'1');
            self.exp = self.exp.saturating_add(1);
        } else {
            if let Some(c) = self.mant.get_mut(n - 1) {
                *c += 1;
            }
            self.mant.truncate(n);
        }
        self.sig = len_i64(self.mant.len());
        true
    }

    fn round_down(&mut self, n: i64) -> bool {
        if n < 0 || n >= self.sig {
            return true;
        }
        let Some(n) = usize::try_from(n).ok().filter(|&n| n <= self.mant.len()) else {
            return false;
        };
        self.mant.truncate(n);
        self.trim();
        true
    }

    /// Go's `trim`, of a decimal whose digits are all known.
    fn trim(&mut self) {
        while self.mant.last() == Some(&b'0') {
            self.mant.pop();
        }
        self.sig = len_i64(self.mant.len());
        if self.mant.is_empty() {
            self.exp = 0;
        }
    }
}

/// Go's `roundShortest`: rounds d, x's decimal, to the shortest decimal between x's
/// neighbours', `lower` and `upper`, comparing their digits index by index, as Go does,
/// wherever their points are. False where it would read a digit not known (never of
/// `leading`'s: `digits_read`).
fn round_shortest(d: &mut Decimal, lower: &Decimal, upper: &Decimal, inclusive: bool) -> bool {
    let mut i = 0i64;
    while i < d.sig {
        let (Some(m), Some(l), Some(u)) = (d.at(i), lower.at(i), upper.at(i)) else {
            return false;
        };
        // Down (truncating) where lower has a different digit, or lower is inclusive and
        // is what rounding down makes; up where upper has a different digit and is
        // inclusive or more than what rounding up makes.
        let okdown = l != m || inclusive && i + 1 == lower.sig;
        let okup = m != u && (inclusive || m + 1 < u || i + 1 < upper.sig);
        match (okdown, okup) {
            (true, true) => return d.round(i + 1),
            (true, false) => return d.round_down(i + 1),
            (false, true) => return d.round_up(i + 1),
            (false, false) => i += 1,
        }
    }
    true
}

const LOG10_5: f64 = 1.0 - LOG10_2;
const LOG2_5: f64 = LOG2_10 - 1.0;

/// Where x's exact decimal has at least this many times the digits roundShortest reads
/// (`digits_read`), computing only those is the cheaper way: the least ratio past which the
/// leading digits' p50 was below Go's way at every size measured, either sign of exponent
/// (PM M524's crossover; the leading digits cost about the same at every size, Go's way
/// grows with the decimal). Below it both ways give the same digits; past it, at the
/// sizes CI's fuzzer reached, Go's way costs minutes and the leading digits microseconds.
const LEADING_FROM: f64 = 8.26;

/// How many digits of x's decimal, and of its neighbours', roundShortest reads at most, for
/// a mantissa of `prec` bits: ⌊(prec + 1)·log10 2⌋ + 3, and one more for the product's
/// rounding in f64.
///
/// x's decimal is d = M·P and lower's d − P, P half an ulp, 2^prec ≤ M < 2^(prec+1). Where
/// they have one exponent X and agree in their first k digits, both lie in one interval of
/// 10^(X−k), so P < 10^(X−k); and P = d/M > 10^(X−1)/2^(prec+1). So 10^(k−1) < 2^(prec+1),
/// and k ≤ ⌊(prec + 1)·log10 2⌋ + 1 (the product is irrational). Where lower's exponent is
/// less, its first digit is 9 and d's 1. So the walk stops by that index, and rounding there
/// reads d's next digit.
fn digits_read(prec: u32) -> i64 {
    ((f64::from(prec) + 1.0) * LOG10_2) as i64 + 4
}

/// log2(n), n not zero, from its top 64 bits.
fn log2(n: &BigUint) -> f64 {
    let b = n.bits();
    let top = if b > 64 { n >> (b - 64) } else { n.clone() };
    let t = top.iter_u64_digits().next().unwrap_or(0) as f64;
    t.log2() + b.saturating_sub(64) as f64
}

/// The digits of n × 2^e's exact decimal (n × 2^a × 5^b's, `leading`), within one: for
/// exponents of Go's range, f64's error is far below one digit, and it is only an
/// estimate where it leans on being within one.
fn decimal_len(n: &BigUint, e: i64) -> f64 {
    let (a, b) = if e >= 0 {
        (e as f64, 0.0)
    } else {
        (0.0, -(e as f64))
    };
    ((log2(n) + a) * LOG10_2 + b * LOG10_5).floor() + 1.0
}

/// What `Decimal::init` makes of n × 2^e for each n of `ns`, but only its first `k` digits,
/// each exact, with its exact length and exponent; None where `k` digits come too near the
/// decimal's end, or where the bounds below do not settle before they are exact.
///
/// n × 2^e is V / 10^b for the integer V = n × 2^a × 5^b ((a, b) is (e, 0), or (0, −e)):
/// its digits are V's, with its point b from V's right. V's first digits are
/// q = ⌊V / 10^j⌋, j leaving more than k of them: V / 10^j = n × 2^(a−j) × 5^(b−j). That
/// power of five is bounded from below and from above at p bits (`pow5_bounds`), and where
/// both bounds of V / 10^j have one floor, q is it. They differ only where V / 10^j lies
/// within the bounds' gap of an integer, and it is no integer: j is more than n's bits, so
/// more than V's trailing zeros (at most n's twos, or its fives). So p doubles until the
/// floors agree, as they do once the bounds are exact.
fn leading(ns: [&BigUint; 3], e: i64, k: i64) -> Option<[Decimal; 3]> {
    let (a, b) = if e >= 0 { (e, 0) } else { (0, e.checked_neg()?) };
    let [_, mid, _] = ns;
    let nbits = ns.iter().map(|n| bits(n)).max()?;
    // V's digits are the middle n's within one, and the others' within one of it (they
    // differ by a part in 2^prec): j leaves each k to k + 4 of them.
    let j = (decimal_len(mid, e) as i64).checked_sub(k)?.checked_sub(2)?;
    if j <= nbits {
        return None;
    }
    let c = b.checked_sub(j)?;
    let t = a.checked_sub(j)?;
    let power = c.unsigned_abs();
    // q's bits, one a squaring for the error each doubles, and a word, past which the
    // bounds' floors differ for one q in 2^63; and the size at which the bounds are exact.
    let mut p = ((k + 4) as f64 * LOG2_10) as u64 + u64::from(u64::BITS - power.leading_zeros()) + 64;
    let exact = (power as f64 * LOG2_5) as u64 + 2;
    let qs = loop {
        let (lo, hi) = pow5_bounds(power, p)?;
        let (under, over) = if c >= 0 { (&lo, &hi) } else { (&hi, &lo) };
        let mut qs = Vec::with_capacity(ns.len());
        for n in ns {
            let q = scaled(n, t, under, c >= 0)?;
            if q != scaled(n, t, over, c >= 0)? {
                break;
            }
            qs.push(q);
        }
        if qs.len() == ns.len() {
            break qs;
        }
        if p > exact {
            return None;
        }
        p = p.checked_mul(2)?;
    };
    let mut out: [Decimal; 3] = Default::default();
    for ((n, q), d) in ns.into_iter().zip(qs).zip(out.iter_mut()) {
        let mut mant = q.to_str_radix(10).into_bytes();
        let got = len_i64(mant.len());
        if got < k {
            return None;
        }
        let digits = j.checked_add(got)?;
        mant.truncate(usize::try_from(k).ok()?);
        *d = Decimal {
            mant,
            exp: digits.checked_sub(b)?,
            sig: digits.checked_sub(decimal_zeros(n, a, b)?)?,
        };
    }
    Some(out)
}

/// The trailing zeros of the decimal of n × 2^a × 5^b, n not zero: the fewer of its twos
/// and its fives.
fn decimal_zeros(n: &BigUint, a: i64, b: i64) -> Option<i64> {
    let twos = i64::try_from(n.trailing_zeros()?).ok()?.checked_add(a)?;
    let mut fives = b;
    let mut rest = n.clone();
    while fives < twos && (&rest % 5u32).bits() == 0 {
        rest /= 5u32;
        fives += 1;
    }
    Some(twos.min(fives))
}

/// A bound m × 2^e.
type Bound = (BigUint, i64);

/// 5^n from below and from above, each m × 2^e with m of about p bits: squarings and
/// products by 5, each cut to p bits, rounding down for the one and up for the other.
/// Exact where 5^n has p bits or fewer.
fn pow5_bounds(n: u64, p: u64) -> Option<(Bound, Bound)> {
    let mut lo: Bound = (BigUint::from(1u32), 0);
    let mut hi = lo.clone();
    for i in (0..u64::BITS - n.leading_zeros()).rev() {
        lo = cut(&lo.0 * &lo.0, lo.1.checked_mul(2)?, p, false);
        hi = cut(&hi.0 * &hi.0, hi.1.checked_mul(2)?, p, true);
        if (n >> i) & 1 == 1 {
            lo = cut(lo.0 * 5u32, lo.1, p, false);
            hi = cut(hi.0 * 5u32, hi.1, p, true);
        }
    }
    Some((lo, hi))
}

/// m × 2^e cut to p bits, rounding down or up.
fn cut(m: BigUint, e: i64, p: u64, up: bool) -> Bound {
    let have = m.bits();
    if have <= p {
        return (m, e);
    }
    let s = have - p;
    let dropped = m.trailing_zeros().is_some_and(|tz| tz < s);
    let mut kept = m >> s;
    if up && dropped {
        kept += 1u32;
    }
    (kept, e.saturating_add(i64::try_from(s).unwrap_or(i64::MAX)))
}

/// ⌊n × 2^t × f⌋, or ⌊n × 2^t / f⌋ for `mul` false, f = m × 2^e not zero.
fn scaled(n: &BigUint, t: i64, (m, e): &Bound, mul: bool) -> Option<BigUint> {
    if mul {
        let s = t.checked_add(*e)?;
        let x = n * m;
        return Some(if s >= 0 {
            x << s.unsigned_abs()
        } else {
            x >> s.unsigned_abs()
        });
    }
    let s = t.checked_sub(*e)?;
    Some(if s >= 0 {
        (n << s.unsigned_abs()) / m
    } else {
        n / (m << s.unsigned_abs())
    })
}

/// Go's `fmtE` (with `'e'`).
fn fmt_e(out: &mut String, prec: i64, d: &Decimal) {
    out.push(char::from(d.digit(0)));
    if prec > 0 {
        out.push('.');
        let mut i: i64 = 1;
        let m = d.len().min(prec + 1);
        while i < m {
            out.push(char::from(d.digit(i)));
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
        out.extend((0..m).map(|i| char::from(d.digit(i))));
        out.extend(std::iter::repeat_n('0', usize::try_from(d.exp - m).unwrap_or(0)));
    } else {
        out.push('0');
    }
    if prec > 0 {
        out.push('.');
        out.extend((0..prec).map(|i| char::from(d.digit(d.exp + i))));
    }
}

#[cfg(test)]
#[path = "../../../docs/research/measurements/rego-bignum/crossover.rs"]
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

        /// A 64-bit float's mantissa and an exponent whose size is spread evenly in
        /// log2 between 2^lo and 2^hi, either sign.
        pub(super) fn float(&mut self, lo: u32, hi: u32) -> Float {
            let mant = BigUint::from(self.next() | 1 << 63);
            let span = f64::from(hi - lo) * (self.next() >> 11) as f64 / (1u64 << 53) as f64;
            let size = 2f64.powf(f64::from(lo) + span) as i64;
            let exp = if self.next() & 1 == 0 { size } else { -size };
            Float {
                prec: 64,
                neg: false,
                mant,
                exp,
            }
        }
    }

    /// Floats at the edges of a decade (where x's neighbours have another exponent, or
    /// rounding carries through every digit), and the fuzzer's number.
    fn edges() -> Vec<Float> {
        let mut out = Vec::new();
        for d in [90, 91, 120, 300, 301, 1000, 4931, 4932, 9999] {
            for s in [
                "1e",
                "1e-",
                "9.999999999999999999e",
                "9.9999999999999999999e-",
                "1.0000000000000000001e",
                "1.000000000000000000001e-",
                "5e",
                "5e-",
                "2.5e",
                "1.5e-",
                "8.5e",
                "9.5e-",
            ] {
                if let Ok(f) = Float::parse(&format!("{s}{d}")) {
                    out.push(f);
                }
            }
        }
        out
    }

    #[test]
    fn leading_digits_are_the_exact_decimals() {
        let mut r = Rng(1);
        let read = digits_read(64);
        let mut checked = 0;
        for _ in 0..1500 {
            let x = r.float(6, 15);
            let (m, e) = x.halves();
            let ns = [&(&m - 1u32), &m, &(&m + 1u32)];
            let Some(got) = leading(ns, e, read) else {
                // Only where the whole decimal is short.
                assert!(decimal_len(&m, e) <= (read + 2 + 66) as f64, "{m} {e}");
                continue;
            };
            for (n, g) in ns.iter().zip(&got) {
                let want = Decimal::init(n, e);
                assert_eq!((g.exp, g.sig), (want.exp, want.sig), "{n} × 2^{e}");
                assert_eq!(g.mant, want.mant[..g.mant.len()], "{n} × 2^{e}");
                assert_eq!(g.mant.len() as i64, read);
            }
            checked += 1;
        }
        assert!(checked > 1200, "{checked}");
    }

    #[test]
    fn shortest_from_leading_digits_is_go_s() {
        let mut r = Rng(2);
        let read = digits_read(64);
        let mut floats: Vec<Float> = (0..1500).map(|_| r.float(6, 15)).collect();
        floats.extend(edges());
        let mut led = 0;
        for x in &floats {
            let (m, e) = x.halves();
            let want = x.shortest_exact(&m, e);
            if let Some(got) = shortest_leading(&m, e, read) {
                assert_eq!(
                    (&got.mant, got.exp, got.sig),
                    (&want.mant, want.exp, want.sig),
                    "{m} × 2^{e}"
                );
                led += 1;
            }
            let got = x.shortest();
            assert_eq!((&got.mant, got.exp, got.sig), (&want.mant, want.exp, want.sig));
        }
        assert!(led > 1200, "{led}");
    }

    #[test]
    fn round_shortest_refuses_digits_it_does_not_hold() {
        // A float whose neighbours share its first 19 digits, given prefixes of two: the
        // walk reaches past them, and refuses rather than guess.
        let x = Float::parse("1.2345678901234567891e5000").unwrap();
        let (m, e) = x.halves();
        let [lower, mut d, upper] = leading([&(&m - 1u32), &m, &(&m + 1u32)], e, 2).unwrap();
        assert!(!round_shortest(&mut d, &lower, &upper, !m.bit(1)));
        // With digits_read's, it decides, as the exact decimals do.
        let got = shortest_leading(&m, e, digits_read(64)).unwrap();
        let want = x.shortest_exact(&m, e);
        assert_eq!((got.mant, got.exp, got.sig), (want.mant, want.exp, want.sig));
        // The fuzzer's number: its lower neighbour is 7999…, it 8000…: the walk stops at
        // once, at "8".
        let x = Float::parse("80000000000e00029174680").unwrap();
        let (m, e) = x.halves();
        let got = shortest_leading(&m, e, digits_read(64)).unwrap();
        assert_eq!((got.mant, got.exp, got.sig), (b"8".to_vec(), 29_174_691, 1));
    }

    #[test]
    fn bounds_enclose_the_power_of_five() {
        for n in [0u64, 1, 2, 27, 28, 29, 100, 1000, 12345] {
            let exact = BigUint::from(5u32).pow(u32::try_from(n).unwrap());
            for p in [4u64, 8, 64, 128, 4096, 40_000] {
                let (lo, hi) = pow5_bounds(n, p).unwrap();
                // m × 2^e against 5^n, both scaled to integers.
                let pair = |(m, e): &Bound| {
                    if *e >= 0 {
                        (m << e.unsigned_abs(), exact.clone())
                    } else {
                        (m.clone(), &exact << e.unsigned_abs())
                    }
                };
                let (l, x) = pair(&lo);
                assert!(l <= x, "5^{n} at {p} bits: the lower bound is above it");
                let (h, x) = pair(&hi);
                assert!(h >= x, "5^{n} at {p} bits: the upper bound is below it");
                if exact.bits() <= p {
                    assert_eq!((&lo.0, lo.1, &hi.0, hi.1), (&exact, 0, &exact, 0));
                }
            }
        }
    }

    #[test]
    fn scaled_floors_hold_the_exact_floor() {
        let mut r = Rng(4);
        for _ in 0..2000 {
            let n = BigUint::from(r.next() | 1 << 63);
            let c = r.next() % 300;
            let t = (r.next() % 1000) as i64 - 500;
            let p = 8 + r.next() % 120;
            let five = BigUint::from(5u32).pow(u32::try_from(c).unwrap());
            let (lo, hi) = pow5_bounds(c, p).unwrap();
            // ⌊n × 2^t × 5^c⌋ and ⌊n × 2^t / 5^c⌋, exactly.
            let (mul, div) = if t >= 0 {
                ((&n * &five) << t.unsigned_abs(), (&n << t.unsigned_abs()) / &five)
            } else {
                ((&n * &five) >> t.unsigned_abs(), &n / (&five << t.unsigned_abs()))
            };
            let at = |f: &Bound, m: bool| scaled(&n, t, f, m).unwrap();
            assert!(
                at(&lo, true) <= mul && mul <= at(&hi, true),
                "{n} 2^{t} 5^{c} at {p}"
            );
            assert!(
                at(&hi, false) <= div && div <= at(&lo, false),
                "{n} 2^{t} / 5^{c} at {p}"
            );
        }
    }

    #[test]
    fn the_fuzzers_number_is_written_whole_from_its_first_digits() {
        let x = Float::parse("80000000000e00029174680").unwrap();
        let (m, e) = x.halves();
        assert!(decimal_len(&m, e) >= LEADING_FROM * digits_read(64) as f64);
        let text = x.text(b'f', u64::MAX).unwrap();
        assert_eq!(text.len(), 29_174_691);
        assert!(text.starts_with('8'));
        assert!(text.bytes().skip(1).all(|b| b == b'0'));
        // Grown once, to its length.
        assert_eq!(text.capacity(), text.len());
    }
}

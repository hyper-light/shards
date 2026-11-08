//! Numbers: topdown/arithmetic.go, aggregates.go's sum and product, bits.go,
//! numbers.go and casts.go's to_number.

use num_bigint::{BigInt, Sign};

use super::super::{BuiltinError, Context, arg, number_operand};
use super::{Out, diff, element_err, elements, gorand, ok};
use crate::number::{self, Float};
use crate::value::{Number, Value};

fn num_err(e: number::Error) -> BuiltinError {
    BuiltinError::Other(e.to_string())
}

/// builtins.NumberToFloat.
fn float(n: &Number) -> Result<Float, BuiltinError> {
    n.to_float().map_err(num_err)
}

/// builtins.FloatToNumber.
fn number(f: &Float) -> Value {
    Value::Number(Number::from_float(f))
}

/// `n.Int()` when inSmallIntRange.
fn small(n: &Number) -> Option<i64> {
    n.as_i64().filter(|x| -1000 < *x && *x < 1000)
}

/// builtins.NumberToInt.
fn to_int(n: &Number) -> Option<BigInt> {
    let (i, exact) = n.to_float().ok()?.int();
    exact.then_some(i)
}

/// big.Int's Int64: the low 64 bits of the magnitude, signed.
fn low_i64(i: &BigInt) -> i64 {
    let low = i.magnitude().iter_u64_digits().next().unwrap_or(0) as i64;
    if i.sign() == Sign::Minus {
        low.wrapping_neg()
    } else {
        low
    }
}

/// big.Int's Uint64: the low 64 bits of the magnitude.
fn low_u64(i: &BigInt) -> u64 {
    i.magnitude().iter_u64_digits().next().unwrap_or(0)
}

fn int_value(i: &BigInt) -> Value {
    Value::Number(Number(i.to_string().into()))
}

/// 0.5 and 1.0 as big.NewFloat makes them.
fn half() -> Float {
    Float::from_f64_exact(false, 1, -1)
}

fn one() -> Float {
    Float::from_f64_exact(false, 1, 0)
}

fn arity1(args: &[Value], f: fn(Float) -> Result<Float, BuiltinError>) -> Out {
    let n = number_operand(arg(args, 0)?, 1)?;
    ok(number(&f(float(n)?)?))
}

pub(super) fn abs(_: &mut Context, args: &[Value]) -> Out {
    arity1(args, |a| Ok(a.abs()))
}

pub(super) fn round(_: &mut Context, args: &[Value]) -> Out {
    arity1(args, |a| {
        let h = half();
        let prec = Float::max_prec(&a, &h);
        let r = if a.signbit() {
            Float::sub(&a, &h, prec)
        } else {
            Float::add(&a, &h, prec)
        }
        .map_err(num_err)?;
        Float::from_int(&r.int().0).map_err(num_err)
    })
}

pub(super) fn ceil(_: &mut Context, args: &[Value]) -> Out {
    arity1(args, |a| {
        let f = Float::from_int(&a.int().0).map_err(num_err)?;
        if f.signbit() || Float::compare(&a, &f).is_eq() {
            return Ok(f);
        }
        let o = one();
        Float::add(&f, &o, Float::max_prec(&f, &o)).map_err(num_err)
    })
}

pub(super) fn floor(_: &mut Context, args: &[Value]) -> Out {
    arity1(args, |a| {
        let f = Float::from_int(&a.int().0).map_err(num_err)?;
        if !f.signbit() || Float::compare(&a, &f).is_eq() {
            return Ok(f);
        }
        let o = one();
        Float::sub(&f, &o, Float::max_prec(&f, &o)).map_err(num_err)
    })
}

type FloatOp = fn(&Float, &Float, u32) -> Result<Float, number::Error>;

/// plus and mul: small integers directly, else big.Float.
fn small_or_float(args: &[Value], small_op: fn(i64, i64) -> i64, op: FloatOp) -> Out {
    let n1 = number_operand(arg(args, 0)?, 1)?;
    let n2 = number_operand(arg(args, 1)?, 2)?;
    if let (Some(x), Some(y)) = (small(n1), small(n2)) {
        return ok(Value::int(small_op(x, y)));
    }
    let (a, b) = (float(n1)?, float(n2)?);
    ok(number(&op(&a, &b, Float::max_prec(&a, &b)).map_err(num_err)?))
}

pub(super) fn plus(_: &mut Context, args: &[Value]) -> Out {
    small_or_float(args, |x, y| x + y, Float::add)
}

pub(super) fn mul(_: &mut Context, args: &[Value]) -> Out {
    small_or_float(args, |x, y| x * y, Float::mul)
}

pub(super) fn minus(_: &mut Context, args: &[Value]) -> Out {
    let (a, b) = (arg(args, 0)?, arg(args, 1)?);
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => small_or_float(args, |x, y| x - y, Float::sub),
        (Value::Set(s1), Value::Set(s2)) => ok(Value::set(diff(s1, s2))),
        (Value::Set(_), Value::Number(_)) => Err(BuiltinError::operand_type(2, b, &["set"])),
        (Value::Number(_) | Value::Set(_), _) => Err(BuiltinError::operand_type(2, b, &["number"])),
        _ => Err(BuiltinError::operand_type(1, a, &["number", "set"])),
    }
}

pub(super) fn div(_: &mut Context, args: &[Value]) -> Out {
    let n1 = number_operand(arg(args, 0)?, 1)?;
    let n2 = number_operand(arg(args, 1)?, 2)?;
    let (a, b) = (float(n1)?, float(n2)?);
    if b.is_zero() {
        return Err(BuiltinError::Other("divide by zero".into()));
    }
    ok(number(
        &Float::quo(&a, &b, Float::max_prec(&a, &b)).map_err(num_err)?,
    ))
}

pub(super) fn rem(_: &mut Context, args: &[Value]) -> Out {
    let (a, b) = (arg(args, 0)?, arg(args, 1)?);
    let (n1, n2) = match (a, b) {
        (Value::Number(x), Value::Number(y)) => (x, y),
        (Value::Number(_), _) => return Err(BuiltinError::operand_type(2, b, &["number"])),
        _ => return Err(BuiltinError::operand_type(1, a, &["number"])),
    };
    if let (Some(x), Some(y)) = (small(n1), small(n2)) {
        if y == 0 {
            return Err(BuiltinError::Other("modulo by zero".into()));
        }
        return ok(Value::int(x % y));
    }
    let (Some(x), Some(y)) = (to_int(n1), to_int(n2)) else {
        return Err(BuiltinError::Other("modulo on floating-point number".into()));
    };
    if low_i64(&y) == 0 {
        return Err(BuiltinError::Other("modulo by zero".into()));
    }
    ok(int_value(&(x % y)))
}

/// sum and product: big.Float over the elements, from `start`.
fn fold(args: &[Value], start: Float, op: FloatOp) -> Result<Float, BuiltinError> {
    let a = arg(args, 0)?;
    let Some(items) = elements(a) else {
        return Err(BuiltinError::operand_type(1, a, &["set", "array"]));
    };
    let mut acc = start;
    for x in items {
        let Value::Number(n) = x else {
            return Err(element_err(1, a, x, "number"));
        };
        let f = float(n)?;
        acc = op(&acc, &f, Float::max_prec(&acc, &f)).map_err(num_err)?;
    }
    Ok(acc)
}

pub(super) fn sum(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    // The fast path: every element an integer, summed as Go's int, wrapping.
    if let Some(items) = elements(a) {
        let mut total = 0i64;
        let all_ints = items.iter().all(|x| match x {
            Value::Number(n) => n.as_i64().map(|i| total = total.wrapping_add(i)).is_some(),
            _ => false,
        });
        if all_ints {
            return ok(Value::int(total));
        }
    }
    ok(number(&fold(
        args,
        Float::from_f64_exact(false, 0, 0),
        Float::add,
    )?))
}

pub(super) fn product(_: &mut Context, args: &[Value]) -> Out {
    ok(number(&fold(args, one(), Float::mul)?))
}

/// builtins.BigIntOperand.
fn big_int_operand(v: &Value, pos: usize) -> Result<BigInt, BuiltinError> {
    let Value::Number(n) = v else {
        return Err(BuiltinError::operand_type(pos, v, &["integer"]));
    };
    to_int(n)
        .ok_or_else(|| BuiltinError::operand(pos, "must be integer number but got floating-point number"))
}

fn bits1(args: &[Value], f: fn(BigInt) -> BigInt) -> Out {
    let i = big_int_operand(arg(args, 0)?, 1)?;
    ok(int_value(&f(i)))
}

fn bits2(args: &[Value], f: fn(BigInt, BigInt) -> Result<BigInt, BuiltinError>) -> Out {
    let a = big_int_operand(arg(args, 0)?, 1)?;
    let b = big_int_operand(arg(args, 1)?, 2)?;
    ok(int_value(&f(a, b)?))
}

pub(super) fn bits_and(_: &mut Context, args: &[Value]) -> Out {
    bits2(args, |a, b| Ok(a & b))
}

pub(super) fn bits_or(_: &mut Context, args: &[Value]) -> Out {
    bits2(args, |a, b| Ok(a | b))
}

pub(super) fn bits_xor(_: &mut Context, args: &[Value]) -> Out {
    bits2(args, |a, b| Ok(a ^ b))
}

pub(super) fn bits_negate(_: &mut Context, args: &[Value]) -> Out {
    bits1(args, |a| !a)
}

/// The shift a negative count refuses, as bits.go words it.
fn shift_count(b: &BigInt) -> Result<u64, BuiltinError> {
    if b.sign() == Sign::Minus {
        return Err(BuiltinError::operand(
            2,
            "must be an unsigned integer number but got a negative integer",
        ));
    }
    Ok(low_u64(b))
}

/// The most bits a left shift may make. Go allocates whatever the shift asks and
/// fails when memory runs out; this refuses first, a result past it being one no
/// policy can hold.
const MAX_SHIFT: u64 = 1 << 32;

pub(super) fn bits_lsh(_: &mut Context, args: &[Value]) -> Out {
    bits2(args, |a, b| {
        let shift = shift_count(&b)?;
        if a.sign() == Sign::NoSign {
            return Ok(a);
        }
        if shift > MAX_SHIFT {
            return Err(BuiltinError::Other("shift count too large".into()));
        }
        Ok(a << shift)
    })
}

pub(super) fn bits_rsh(_: &mut Context, args: &[Value]) -> Out {
    bits2(args, |a, b| {
        let shift = shift_count(&b)?;
        if a.bits() <= shift {
            // Everything shifted out: Go's Rsh rounds toward negative infinity.
            return Ok(if a.sign() == Sign::Minus {
                BigInt::from(-1)
            } else {
                BigInt::default()
            });
        }
        Ok(a >> shift)
    })
}

/// ast.HasInternedIntNumberTerm.
fn interned(i: i64) -> bool {
    (-1..=512).contains(&i)
}

/// numbers.go's canGenerateCheapRange: both bounds integers OPA interns.
fn cheap(args: &[Value], at: usize) -> Option<i64> {
    match args.get(at)? {
        Value::Number(n) => n.as_i64().filter(|i| interned(*i)),
        _ => None,
    }
}

fn cheap_range(x: i64, y: i64, step: i64) -> Value {
    let mut out = Vec::new();
    let mut i = x;
    if x <= y {
        while i <= y {
            out.push(Value::int(i));
            i += step;
        }
    } else {
        while i >= y {
            out.push(Value::int(i));
            i -= step;
        }
    }
    Value::array(out)
}

fn big_range(x: BigInt, y: &BigInt, step: &BigInt) -> Value {
    let mut out = Vec::new();
    let mut i = x;
    if i > *y {
        while i >= *y {
            out.push(int_value(&i));
            i -= step;
        }
    } else {
        while i <= *y {
            out.push(int_value(&i));
            i += step;
        }
    }
    Value::array(out)
}

pub(super) fn range(_: &mut Context, args: &[Value]) -> Out {
    if let (Some(x), Some(y)) = (cheap(args, 0), cheap(args, 1)) {
        return ok(cheap_range(x, y, 1));
    }
    let x = big_int_operand(arg(args, 0)?, 1)?;
    let y = big_int_operand(arg(args, 1)?, 2)?;
    ok(big_range(x, &y, &BigInt::from(1)))
}

const STEP_ERR: &str = "numbers.range_step: step must be a positive integer";

pub(super) fn range_step(_: &mut Context, args: &[Value]) -> Out {
    if let (Some(x), Some(y), Some(step)) = (cheap(args, 0), cheap(args, 1), cheap(args, 2)) {
        if step <= 0 {
            return Err(BuiltinError::Other(STEP_ERR.into()));
        }
        return ok(cheap_range(x, y, step));
    }
    let x = big_int_operand(arg(args, 0)?, 1)?;
    let y = big_int_operand(arg(args, 1)?, 2)?;
    let step = big_int_operand(arg(args, 2)?, 3)?;
    if step.sign() != Sign::Plus {
        return Err(BuiltinError::Other(STEP_ERR.into()));
    }
    ok(big_range(x, &y, &step))
}

/// io.ReadFull of `n` bytes from the query's seed, its errors in Go's words.
pub(super) fn read_seed(ctx: &mut Context, n: usize) -> Result<Vec<u8>, BuiltinError> {
    // A seed read from the system (OPA's default, rand.Reader) is filled as it is read.
    if let Some(fill) = ctx.fill {
        let short = n.saturating_sub(ctx.seed.len().saturating_sub(ctx.seed_at));
        if short > 0 {
            let mut more = vec![0u8; short];
            fill(&mut more).map_err(BuiltinError::Other)?;
            ctx.seed.extend_from_slice(&more);
        }
    }
    let rest = ctx.seed.get(ctx.seed_at..).unwrap_or_default();
    let got: Vec<u8> = rest.iter().take(n).copied().collect();
    ctx.seed_at = ctx.seed_at.saturating_add(got.len());
    if got.len() == n {
        Ok(got)
    } else if got.is_empty() {
        Err(BuiltinError::Other("EOF".into()))
    } else {
        Err(BuiltinError::Other("unexpected EOF".into()))
    }
}

pub(super) fn rand_intn(ctx: &mut Context, args: &[Value]) -> Out {
    super::super::string_operand(arg(args, 0)?, 1)?;
    let n = super::super::int_operand(arg(args, 1)?, 2)?;
    if n == 0 {
        return ok(Value::int(0));
    }
    // Go's -n leaves the least int64 negative, and Intn then panics.
    let n = n
        .checked_abs()
        .ok_or_else(|| BuiltinError::Other("invalid argument to Intn".into()))?;
    // BuiltinContext.Rand: a generator seeded from the next 8 bytes, big-endian.
    let bytes = read_seed(ctx, 8)?;
    let mut seed = [0u8; 8];
    for (d, s) in seed.iter_mut().zip(&bytes) {
        *d = *s;
    }
    let mut r = gorand::Rand::new(i64::from_be_bytes(seed));
    ok(Value::int(r.intn(n)))
}

pub(super) fn to_number(_: &mut Context, args: &[Value]) -> Out {
    let a = arg(args, 0)?;
    match a {
        Value::Null => ok(Value::int(0)),
        Value::Bool(b) => ok(Value::int(i64::from(*b))),
        Value::Number(_) => ok(a.clone()),
        Value::String(s) => {
            // strings.ToLower maps each rune alone: U+0130 to a plain i.
            let lower: String = s
                .trim_start_matches(['+', '-'])
                .chars()
                .map(|c| {
                    if c == '\u{130}' {
                        'i'
                    } else {
                        c.to_lowercase().next().unwrap_or(c)
                    }
                })
                .collect();
            if lower == "inf" || lower == "infinity" || lower == "nan" {
                return Err(BuiltinError::operand_type(1, a, &["valid number string"]));
            }
            if let Err(why) = parse_float(s) {
                let mut q = String::new();
                crate::goquote::quote(&mut q, s);
                return Err(BuiltinError::Other(format!(
                    "strconv.ParseFloat: parsing {q}: {why}"
                )));
            }
            ok(Value::Number(Number(s.clone())))
        }
        _ => Err(BuiltinError::operand_type(
            1,
            a,
            &["null", "boolean", "number", "string"],
        )),
    }
}

/// strconv.ParseFloat(s, 64)'s verdict: Go's readFloat for the syntax, then whether
/// the value overflows a float64.
fn parse_float(s: &str) -> Result<(), &'static str> {
    const SYNTAX: &str = "invalid syntax";
    const RANGE: &str = "value out of range";
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut i = 0;
    if matches!(at(i), b'+' | b'-') {
        i += 1;
    }
    let hex = i + 2 < b.len() && at(i) == b'0' && at(i + 1).eq_ignore_ascii_case(&b'x');
    if hex {
        i += 2;
    }
    let (mut sawdot, mut sawdigits, mut underscores) = (false, false, false);
    while i < b.len() {
        let c = at(i);
        match c {
            b'_' => underscores = true,
            b'.' if sawdot => break,
            b'.' => sawdot = true,
            b'0'..=b'9' => sawdigits = true,
            _ if hex && c.is_ascii_hexdigit() => sawdigits = true,
            _ => break,
        }
        i += 1;
    }
    if !sawdigits {
        return Err(SYNTAX);
    }
    let exp_char = if hex { b'p' } else { b'e' };
    if i < b.len() && at(i).to_ascii_lowercase() == exp_char {
        i += 1;
        if matches!(at(i), b'+' | b'-') {
            i += 1;
        }
        if !at(i).is_ascii_digit() {
            return Err(SYNTAX);
        }
        while at(i).is_ascii_digit() || at(i) == b'_' {
            underscores |= at(i) == b'_';
            i += 1;
        }
    } else if hex {
        return Err(SYNTAX);
    }
    if underscores && !underscore_ok(b.get(..i).unwrap_or_default()) {
        return Err(SYNTAX);
    }
    if i != b.len() {
        return Err(SYNTAX);
    }
    let finite = if hex {
        hex_fits(s)
    } else {
        let plain: String = s.chars().filter(|c| *c != '_').collect();
        plain.parse::<f64>().is_ok_and(f64::is_finite)
    };
    if finite { Ok(()) } else { Err(RANGE) }
}

/// strconv's underscoreOK.
fn underscore_ok(s: &[u8]) -> bool {
    let s = match s.first() {
        Some(b'+' | b'-') => s.get(1..).unwrap_or_default(),
        _ => s,
    };
    let mut saw = b'^';
    let mut i = 0;
    let mut hex = false;
    if s.len() >= 2
        && s.first() == Some(&b'0')
        && matches!(s.get(1).map(u8::to_ascii_lowercase), Some(b'b' | b'o' | b'x'))
    {
        i = 2;
        saw = b'0';
        hex = s.get(1).map(u8::to_ascii_lowercase) == Some(b'x');
    }
    while let Some(&c) = s.get(i) {
        i += 1;
        if c.is_ascii_digit() || hex && c.is_ascii_hexdigit() {
            saw = b'0';
            continue;
        }
        if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
            continue;
        }
        if saw == b'_' {
            return false;
        }
        saw = b'!';
    }
    saw != b'_'
}

/// Whether a hexadecimal float's value, rounded to 53 bits, is below 2^1024.
fn hex_fits(s: &str) -> bool {
    let Ok((_, mant, e2, _)) = number::exact_parts(s) else {
        return false;
    };
    let bits = i64::try_from(mant.bits()).unwrap_or(i64::MAX);
    if bits == 0 {
        return true;
    }
    let mut top = bits.saturating_add(e2);
    if bits > 53 {
        // Rounds up to the next power of two when the 53 bits kept are all ones and
        // what is dropped is at least half.
        let cut = u64::try_from(bits - 53).unwrap_or(0);
        let kept = &mant >> cut;
        let all_ones = kept.count_ones() == 53;
        let half = mant.bit(cut - 1);
        let sticky = mant.trailing_zeros().is_some_and(|tz| tz < cut - 1);
        if all_ones && half && (sticky || kept.bit(0)) {
            top = top.saturating_add(1);
        }
    }
    top <= 1024
}

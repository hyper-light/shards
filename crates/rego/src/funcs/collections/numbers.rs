//! Numbers: topdown/arithmetic.go, aggregates.go's sum and product, bits.go,
//! numbers.go and casts.go's to_number.

use num_bigint::{BigInt, BigUint, Sign};

use super::super::{BuiltinError, Context, arg, number_operand};
use super::{Out, diff, element_err, elements, gorand, ok};
use crate::digits;
use crate::number::{self, Float};
use crate::value::{Number, Value};

fn num_err(e: number::Error) -> BuiltinError {
    BuiltinError::Other(e.to_string())
}

/// builtins.NumberToFloat.
fn float(n: &Number) -> Result<Float, BuiltinError> {
    n.to_float().map_err(num_err)
}

/// builtins.FloatToNumber, where its text fits what the evaluation can hold.
fn number(ctx: &Context, f: &Float) -> Out {
    ok(Value::Number(
        Number::from_float(f, ctx.holdable).map_err(num_err)?,
    ))
}

/// `n.Int()` when inSmallIntRange.
fn small(n: &Number) -> Option<i64> {
    n.as_i64().filter(|x| -1000 < *x && *x < 1000)
}

/// builtins.NumberToInt: `None` where Go's is inexact (or OPA panics: Go cannot read
/// the number); refused where its limbs would pass `holdable`.
fn to_int(n: &Number, holdable: u64) -> Result<Option<BigInt>, BuiltinError> {
    let Ok(f) = n.to_float() else {
        return Ok(None);
    };
    let (i, exact) = f.int(holdable).map_err(num_err)?;
    Ok(exact.then_some(i))
}

/// big.Int's Uint64: the low 64 bits of the magnitude.
fn low_u64(i: &BigInt) -> u64 {
    i.magnitude().iter_u64_digits().next().unwrap_or(0)
}

/// The digits of m·2^shift, where writing them, beside the `held` bytes the builtin
/// holds already, fits what the evaluation can hold; a refusal names the evaluation's
/// bound, and the held bytes with the need.
fn written(ctx: &Context, neg: bool, m: &BigUint, shift: u64, held: u64) -> Result<Value, BuiltinError> {
    let mut out = String::new();
    digits::write(&mut out, neg, m, shift, ctx.holdable.saturating_sub(held)).map_err(|e| {
        num_err(match e {
            number::Error::Unholdable { bits, need, .. } => number::Error::Unholdable {
                bits,
                need: need.saturating_add(held),
                holdable: ctx.holdable,
            },
            e => e,
        })
    })?;
    Ok(Value::Number(Number(out.into())))
}

/// An integer's text, `i.String()`, as [`written`].
fn int_value(ctx: &Context, i: &BigInt, held: u64) -> Result<Value, BuiltinError> {
    written(ctx, i.sign() == Sign::Minus, i.magnitude(), 0, held)
}

/// The bits of a number operand's integer, from its float, before the integer is made
/// (0 for what is no number: its operand error comes later).
fn int_bits(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n
            .to_float()
            .map_or(0, |f| u64::try_from(f.go_exponent().max(0)).unwrap_or(0)),
        _ => 0,
    }
}

/// The bytes integers of these bits take as num-bigint's limbs.
fn limb_bytes(bits: &[u64]) -> u64 {
    bits.iter()
        .map(|b| b.div_ceil(64).saturating_mul(8))
        .fold(0, u64::saturating_add)
}

/// Refuses, before its operands are made, an integer answer of at most `bits` bits
/// whose text, beside the integers of `made` bits the builtin makes on the way, would
/// take more than the evaluation can hold.
fn holds(ctx: &Context, bits: u64, made: &[u64]) -> Result<(), BuiltinError> {
    let need = digits::need(bits).saturating_add(limb_bytes(made));
    if need > ctx.holdable {
        return Err(num_err(number::Error::Unholdable {
            bits,
            need,
            holdable: ctx.holdable,
        }));
    }
    Ok(())
}

/// 0.5 and 1.0 as big.NewFloat makes them.
fn half() -> Float {
    Float::from_f64_exact(false, 1, -1)
}

fn one() -> Float {
    Float::from_f64_exact(false, 1, 0)
}

/// `new(big.Float).SetInt(x.Int(nil))`, for an x below 2^64: [`whole`] answers those past
/// their halves.
fn int_float(x: &Float) -> Result<Float, BuiltinError> {
    Float::from_int(&x.int(u64::MAX).map_err(num_err)?.0).map_err(num_err)
}

fn arity1(ctx: &Context, args: &[Value], f: fn(Float) -> Result<Float, BuiltinError>) -> Out {
    let n = number_operand(arg(args, 0)?, 1)?;
    number(ctx, &f(float(n)?)?)
}

pub(super) fn abs(ctx: &mut Context, args: &[Value]) -> Out {
    arity1(ctx, args, |a| Ok(a.abs()))
}

/// round, ceil and floor of a float whose ulp is 2 or more: an integer, which each
/// answers whole, as `new(big.Float).SetInt` of its Int writes it (half of 1 is under
/// half its ulp, so round's sum rounds back to it); None for other floats.
fn whole(ctx: &Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let a = float(number_operand(arg(args, 0)?, 1)?)?;
    if a.go_exponent() <= i64::from(a.precision()) {
        return Ok(None);
    }
    let text = a.int_text(ctx.holdable).map_err(num_err)?;
    Ok(Some(Value::Number(Number(text.into()))))
}

pub(super) fn round(ctx: &mut Context, args: &[Value]) -> Out {
    if let Some(v) = whole(ctx, args)? {
        return ok(v);
    }
    arity1(ctx, args, |a| {
        let h = half();
        let prec = Float::max_prec(&a, &h);
        let r = if a.signbit() {
            Float::sub(&a, &h, prec)
        } else {
            Float::add(&a, &h, prec)
        }
        .map_err(num_err)?;
        int_float(&r)
    })
}

pub(super) fn ceil(ctx: &mut Context, args: &[Value]) -> Out {
    if let Some(v) = whole(ctx, args)? {
        return ok(v);
    }
    arity1(ctx, args, |a| {
        let f = int_float(&a)?;
        if f.signbit() || Float::compare(&a, &f).is_eq() {
            return Ok(f);
        }
        let o = one();
        Float::add(&f, &o, Float::max_prec(&f, &o)).map_err(num_err)
    })
}

pub(super) fn floor(ctx: &mut Context, args: &[Value]) -> Out {
    if let Some(v) = whole(ctx, args)? {
        return ok(v);
    }
    arity1(ctx, args, |a| {
        let f = int_float(&a)?;
        if !f.signbit() || Float::compare(&a, &f).is_eq() {
            return Ok(f);
        }
        let o = one();
        Float::sub(&f, &o, Float::max_prec(&f, &o)).map_err(num_err)
    })
}

type FloatOp = fn(&Float, &Float, u32) -> Result<Float, number::Error>;

/// plus and mul: small integers directly, else big.Float.
fn small_or_float(ctx: &Context, args: &[Value], small_op: fn(i64, i64) -> i64, op: FloatOp) -> Out {
    let n1 = number_operand(arg(args, 0)?, 1)?;
    let n2 = number_operand(arg(args, 1)?, 2)?;
    if let (Some(x), Some(y)) = (small(n1), small(n2)) {
        return ok(Value::int(small_op(x, y)));
    }
    let (a, b) = (float(n1)?, float(n2)?);
    number(ctx, &op(&a, &b, Float::max_prec(&a, &b)).map_err(num_err)?)
}

pub(super) fn plus(ctx: &mut Context, args: &[Value]) -> Out {
    small_or_float(ctx, args, |x, y| x + y, Float::add)
}

pub(super) fn mul(ctx: &mut Context, args: &[Value]) -> Out {
    small_or_float(ctx, args, |x, y| x * y, Float::mul)
}

pub(super) fn minus(ctx: &mut Context, args: &[Value]) -> Out {
    let (a, b) = (arg(args, 0)?, arg(args, 1)?);
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => small_or_float(ctx, args, |x, y| x - y, Float::sub),
        (Value::Set(s1), Value::Set(s2)) => ok(Value::set(diff(s1, s2))),
        (Value::Set(_), Value::Number(_)) => Err(BuiltinError::operand_type(2, b, &["set"])),
        (Value::Number(_) | Value::Set(_), _) => Err(BuiltinError::operand_type(2, b, &["number"])),
        _ => Err(BuiltinError::operand_type(1, a, &["number", "set"])),
    }
}

pub(super) fn div(ctx: &mut Context, args: &[Value]) -> Out {
    let n1 = number_operand(arg(args, 0)?, 1)?;
    let n2 = number_operand(arg(args, 1)?, 2)?;
    let (a, b) = (float(n1)?, float(n2)?);
    if b.is_zero() {
        return Err(BuiltinError::Other("divide by zero".into()));
    }
    number(
        ctx,
        &Float::quo(&a, &b, Float::max_prec(&a, &b)).map_err(num_err)?,
    )
}

/// x % y of integers from floats, as big.Int's Rem (truncated, of x's sign), y's low 64
/// bits not zero, so y below 2^(64 + its precision): x's mantissa and its power of two
/// each reduced mod y, so that x is never made whole.
fn float_rem(x: &Float, y: &Float) -> BigInt {
    let (ym, yshift) = y.truncated();
    let y = ym.into_owned() << yshift;
    let (xm, xshift) = x.truncated();
    let r = (xm.as_ref() % &y) * BigUint::from(2u32).modpow(&BigUint::from(xshift), &y) % &y;
    let sign = if x.signbit() { Sign::Minus } else { Sign::Plus };
    BigInt::from_biguint(sign, r)
}

pub(super) fn rem(ctx: &mut Context, args: &[Value]) -> Out {
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
    let integer = |n: &Number| n.to_float().ok().filter(Float::is_int);
    let (Some(x), Some(y)) = (integer(n1), integer(n2)) else {
        return Err(BuiltinError::Other("modulo on floating-point number".into()));
    };
    // big.Int's Int64 of y, its low 64 bits, as OPA asks of a divisor.
    let (ym, yshift) = y.truncated();
    let low = ym
        .iter_u64_digits()
        .next()
        .unwrap_or(0)
        .checked_shl(u32::try_from(yshift).unwrap_or(u32::MAX));
    if low.is_none_or(|l| l == 0) {
        return Err(BuiltinError::Other("modulo by zero".into()));
    }
    ok(int_value(ctx, &float_rem(&x, &y), 0)?)
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

pub(super) fn sum(ctx: &mut Context, args: &[Value]) -> Out {
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
    number(ctx, &fold(args, Float::from_f64_exact(false, 0, 0), Float::add)?)
}

pub(super) fn product(ctx: &mut Context, args: &[Value]) -> Out {
    number(ctx, &fold(args, one(), Float::mul)?)
}

/// builtins.BigIntOperand, made where it fits `holdable`.
fn big_int_operand(v: &Value, pos: usize, holdable: u64) -> Result<BigInt, BuiltinError> {
    let Value::Number(n) = v else {
        return Err(BuiltinError::operand_type(pos, v, &["integer"]));
    };
    to_int(n, holdable)?
        .ok_or_else(|| BuiltinError::operand(pos, "must be integer number but got floating-point number"))
}

/// bits.negate: ~a, a bit longer than a at most; num-bigint's Not works in a's limbs.
fn bits1(ctx: &Context, args: &[Value], f: fn(BigInt) -> BigInt) -> Out {
    let v = arg(args, 0)?;
    let bits = int_bits(v);
    holds(ctx, bits.saturating_add(1), &[bits])?;
    let i = big_int_operand(v, 1, ctx.holdable)?;
    ok(int_value(ctx, &f(i), 0)?)
}

/// and, or and xor: a bit longer than the longer operand at most, made in the first's
/// limbs, the second held beside them.
fn bits2(ctx: &Context, args: &[Value], f: fn(BigInt, BigInt) -> BigInt) -> Out {
    let (va, vb) = (arg(args, 0)?, arg(args, 1)?);
    let (ba, bb) = (int_bits(va), int_bits(vb));
    holds(ctx, ba.max(bb).saturating_add(1), &[ba, bb])?;
    let a = big_int_operand(va, 1, ctx.holdable)?;
    let b = big_int_operand(vb, 2, ctx.holdable)?;
    ok(int_value(ctx, &f(a, b), 0)?)
}

pub(super) fn bits_and(ctx: &mut Context, args: &[Value]) -> Out {
    bits2(ctx, args, |a, b| a & b)
}

pub(super) fn bits_or(ctx: &mut Context, args: &[Value]) -> Out {
    bits2(ctx, args, |a, b| a | b)
}

pub(super) fn bits_xor(ctx: &mut Context, args: &[Value]) -> Out {
    bits2(ctx, args, |a, b| a ^ b)
}

pub(super) fn bits_negate(ctx: &mut Context, args: &[Value]) -> Out {
    bits1(ctx, args, |a| !a)
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

/// The operands, made where they fit what the evaluation can hold, and the shift.
fn shifted(ctx: &Context, args: &[Value]) -> Result<(BigInt, u64), BuiltinError> {
    let (va, vb) = (arg(args, 0)?, arg(args, 1)?);
    let (ba, bb) = (int_bits(va), int_bits(vb));
    holds(ctx, 0, &[ba, bb])?;
    let a = big_int_operand(va, 1, ctx.holdable)?;
    let b = big_int_operand(vb, 2, ctx.holdable)?;
    Ok((a, shift_count(&b)?))
}

/// a << s, written from a and s, never made: Go makes it whole, whatever the shift
/// asks, and fails when memory runs out; the text's bound refuses first.
pub(super) fn bits_lsh(ctx: &mut Context, args: &[Value]) -> Out {
    let (a, shift) = shifted(ctx, args)?;
    let shift = if a.sign() == Sign::NoSign { 0 } else { shift };
    let held = limb_bytes(&[a.bits()]);
    ok(written(ctx, a.sign() == Sign::Minus, a.magnitude(), shift, held)?)
}

pub(super) fn bits_rsh(ctx: &mut Context, args: &[Value]) -> Out {
    let (a, shift) = shifted(ctx, args)?;
    let r = if a.bits() <= shift {
        // Everything shifted out: Go's Rsh rounds toward negative infinity.
        if a.sign() == Sign::Minus {
            BigInt::from(-1)
        } else {
            BigInt::default()
        }
    } else {
        &a >> shift
    };
    ok(int_value(ctx, &r, limb_bytes(&[a.bits()]))?)
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

/// Every integer from x to y, `step` apart, each written where it fits beside the
/// bounds and the step, which the range holds as it goes.
fn big_range(ctx: &Context, x: BigInt, y: &BigInt, step: &BigInt) -> Out {
    let held = limb_bytes(&[x.bits(), y.bits(), step.bits()]);
    let mut out = Vec::new();
    let mut i = x;
    if i > *y {
        while i >= *y {
            out.push(int_value(ctx, &i, held)?);
            i -= step;
        }
    } else {
        while i <= *y {
            out.push(int_value(ctx, &i, held)?);
            i += step;
        }
    }
    ok(Value::array(out))
}

/// The bounds of a range (and its step), made where they fit what the evaluation can
/// hold beside an element as long as the longer bound.
fn range_operands(ctx: &Context, args: &[Value]) -> Result<(BigInt, BigInt), BuiltinError> {
    let (vx, vy) = (arg(args, 0)?, arg(args, 1)?);
    let (bx, by) = (int_bits(vx), int_bits(vy));
    let bs = args.get(2).map_or(0, int_bits);
    holds(ctx, bx.max(by), &[bx, by, bs])?;
    Ok((
        big_int_operand(vx, 1, ctx.holdable)?,
        big_int_operand(vy, 2, ctx.holdable)?,
    ))
}

pub(super) fn range(ctx: &mut Context, args: &[Value]) -> Out {
    if let (Some(x), Some(y)) = (cheap(args, 0), cheap(args, 1)) {
        return ok(cheap_range(x, y, 1));
    }
    let (x, y) = range_operands(ctx, args)?;
    big_range(ctx, x, &y, &BigInt::from(1))
}

const STEP_ERR: &str = "numbers.range_step: step must be a positive integer";

pub(super) fn range_step(ctx: &mut Context, args: &[Value]) -> Out {
    if let (Some(x), Some(y), Some(step)) = (cheap(args, 0), cheap(args, 1), cheap(args, 2)) {
        if step <= 0 {
            return Err(BuiltinError::Other(STEP_ERR.into()));
        }
        return ok(cheap_range(x, y, step));
    }
    let (x, y) = range_operands(ctx, args)?;
    let step = big_int_operand(arg(args, 2)?, 3, ctx.holdable)?;
    if step.sign() != Sign::Plus {
        return Err(BuiltinError::Other(STEP_ERR.into()));
    }
    big_range(ctx, x, &y, &step)
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

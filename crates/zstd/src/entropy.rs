//! `compress.ShannonEntropyBits` (compressible.go), which the best encoder weighs a
//! block's literals by, with Go's own pure-Go `math.Log2`, `Log` and `Frexp` (log10.go,
//! log.go, frexp.go), so that every bit of it is Go's.

/// `frexp`: `x` as a fraction in [½, 1) and a power of two.
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || x.is_infinite() || x.is_nan() {
        return (x, 0);
    }
    let (x, mut exp) = if x.abs() < f64::MIN_POSITIVE {
        (x * (1u64 << 52) as f64, -52)
    } else {
        (x, 0)
    };
    let mut b = x.to_bits();
    exp += ((b >> 52) & 0x7ff) as i32 - 1023 + 1;
    b &= !(0x7ff << 52);
    b |= 1022 << 52;
    (f64::from_bits(b), exp)
}

/// `log`: FreeBSD's `e_log.c`, as Go ports it; its constants as Go writes them, each
/// the same f64.
#[allow(clippy::excessive_precision)]
fn log(x: f64) -> f64 {
    const LN2_HI: f64 = 6.931_471_803_691_238_164_90e-01;
    const LN2_LO: f64 = 1.908_214_929_270_587_700_02e-10;
    const L1: f64 = 6.666_666_666_666_735_130e-01;
    const L2: f64 = 3.999_999_999_940_941_908e-01;
    const L3: f64 = 2.857_142_874_366_239_149e-01;
    const L4: f64 = 2.222_219_843_214_978_396e-01;
    const L5: f64 = 1.818_357_216_161_805_012e-01;
    const L6: f64 = 1.531_383_769_920_937_332e-01;
    const L7: f64 = 1.479_819_860_511_658_591e-01;
    if x.is_nan() || x == f64::INFINITY {
        return x;
    }
    if x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    let (mut f1, mut ki) = frexp(x);
    if f1 < std::f64::consts::SQRT_2 / 2.0 {
        f1 *= 2.0;
        ki -= 1;
    }
    let f = f1 - 1.0;
    let k = f64::from(ki);
    let s = f / (2.0 + f);
    let s2 = s * s;
    let s4 = s2 * s2;
    let t1 = s2 * (L1 + s4 * (L3 + s4 * (L5 + s4 * L7)));
    let t2 = s4 * (L2 + s4 * (L4 + s4 * L6));
    let r = t1 + t2;
    let hfsq = 0.5 * f * f;
    k * LN2_HI - ((hfsq - (s * (hfsq + r) + k * LN2_LO)) - f)
}

/// `log2`.
fn log2(x: f64) -> f64 {
    let (frac, exp) = frexp(x);
    if frac == 0.5 {
        return f64::from(exp - 1);
    }
    log(frac) * (1.0 / std::f64::consts::LN_2) + f64::from(exp)
}

/// `ShannonEntropyBits`: the bits `b` takes at least, coded by its bytes' frequencies.
pub fn shannon_entropy_bits(b: &[u8]) -> i64 {
    if b.is_empty() {
        return 0;
    }
    let mut hist = [0u64; 256];
    for &c in b {
        if let Some(h) = hist.get_mut(usize::from(c)) {
            *h += 1;
        }
    }
    let mut shannon = 0.0f64;
    let inv_total = 1.0 / b.len() as f64;
    for &v in &hist {
        if v > 0 {
            let n = v as f64;
            shannon += (-log2(n * inv_total) * n).ceil();
        }
    }
    shannon.ceil() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log2_is_exact_at_powers_and_close_elsewhere() {
        for e in -20..20 {
            assert_eq!(log2(2f64.powi(e)), f64::from(e));
        }
        for x in [0.3, 0.7, 0.123_456, 0.999] {
            assert!((log2(x) - x.log2()).abs() < 1e-15, "{x}");
        }
    }
}

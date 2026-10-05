//! The values of `run`'s resource flags as docker/cli v29.8.1 reads and prints them
//! (opts/opts.go): MemBytes and MemSwapBytes through go-units' RAMInBytes and BytesSize,
//! NanoCPUs through math/big's Rat.

use num_bigint::BigUint;
use num_integer::Integer as _;
use num_traits::Zero as _;

use crate::go;

/// go-units v0.5.0 RAMInBytes (size.go parseSize, binary units): a number, then an
/// optional `k`, `m`, `g`, `t` or `p` with an optional `b` or `ib`, case aside, or `b`
/// alone; a space may stand between. Go converts the float to int64 as arm64 does,
/// saturating, and NaN to 0.
pub fn ram_in_bytes(size: &str) -> Result<i64, String> {
    let invalid = || format!("invalid size: '{size}'");
    let sep = size
        .rfind(|c: char| c.is_ascii_digit() || c == '.' || c == ' ')
        .ok_or_else(invalid)?;
    // The separator is ASCII, so both sides are whole characters.
    let end = if size.as_bytes().get(sep) == Some(&b' ') {
        sep
    } else {
        sep + 1
    };
    let (num, sfx) = (
        size.get(..end).ok_or_else(invalid)?,
        size.get(sep + 1..).ok_or_else(invalid)?,
    );
    let value = go::parse_float(num).map_err(|e| e.to_string())?;
    if value < 0.0 {
        return Err(invalid());
    }
    #[allow(clippy::cast_possible_truncation)]
    let int = |v: f64| v as i64;
    if sfx.is_empty() {
        return Ok(int(value));
    }
    let bad = || format!("invalid suffix: '{}'", sfx.to_lowercase());
    if sfx.len() > 3 {
        return Err(format!("invalid suffix: '{sfx}'"));
    }
    let lower = sfx.to_lowercase();
    let mut chars = lower.bytes();
    let first = chars.next().ok_or_else(bad)?;
    if first == b'b' {
        return if lower.len() > 1 {
            Err(bad())
        } else {
            Ok(int(value))
        };
    }
    let mul: f64 = match first {
        b'k' => 1024.0,
        b'm' => 1_048_576.0,
        b'g' => 1_073_741_824.0,
        b't' => 1_099_511_627_776.0,
        b'p' => 1_125_899_906_842_624.0,
        _ => return Err(bad()),
    };
    match lower.len() {
        2 if !lower.ends_with('b') => return Err(bad()),
        3 if lower.get(1..) != Some("ib") => return Err(bad()),
        _ => {}
    }
    Ok(int(value * mul))
}

/// MemBytes.String: go-units' BytesSize, or `0` for none (so pflag hides the default).
pub fn mem_bytes_string(n: i64) -> String {
    if n == 0 {
        return "0".into();
    }
    #[allow(clippy::cast_precision_loss)]
    crate::format::units::bytes_size(n as f64)
}

/// A reader of bytes as Go's io.ByteScanner over a string.
struct Scan<'a> {
    s: &'a [u8],
    i: usize,
}

impl Scan<'_> {
    fn read(&mut self) -> Option<u8> {
        let c = *self.s.get(self.i)?;
        self.i += 1;
        Some(c)
    }

    fn unread(&mut self) {
        self.i = self.i.saturating_sub(1);
    }

    fn done(&mut self) -> bool {
        self.read().is_none()
    }
}

/// math/big's scanSign: none at the end of the input.
fn scan_sign(r: &mut Scan<'_>) -> Option<bool> {
    match r.read()? {
        b'-' => Some(true),
        b'+' => Some(false),
        _ => {
            r.unread();
            Some(false)
        }
    }
}

/// math/big's nat.scan with base 0: the number, its base, and its digit count, or with
/// `frac_ok` the negated count of digits after a point.
fn nat_scan(r: &mut Scan<'_>, frac_ok: bool) -> Option<(BigUint, u32, i64)> {
    let mut frac_ok = frac_ok;
    let mut prev = b'.';
    let mut inval_sep = false;
    let mut ch = r.read();
    let (mut b, mut prefix) = (10u32, 0u8);
    let mut count: i64 = 0;
    if ch == Some(b'0') {
        prev = b'0';
        count = 1;
        ch = r.read();
        if let Some(c) = ch {
            match c {
                b'b' | b'B' => (b, prefix) = (2, b'b'),
                b'o' | b'O' => (b, prefix) = (8, b'o'),
                b'x' | b'X' => (b, prefix) = (16, b'x'),
                _ if !frac_ok => (b, prefix) = (8, b'0'),
                _ => {}
            }
            if prefix != 0 {
                count = 0;
                if prefix != b'0' {
                    ch = r.read();
                }
            }
        }
    }
    let mut z = BigUint::zero();
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
            let d = match c {
                b'0'..=b'9' => u32::from(c - b'0'),
                b'a'..=b'z' => u32::from(c - b'a') + 10,
                b'A'..=b'Z' => u32::from(c - b'A') + 10,
                _ => u32::MAX,
            };
            if d >= b {
                r.unread();
                break;
            }
            prev = b'0';
            count += 1;
            z = z * b + d;
        }
        ch = r.read();
    }
    if inval_sep || prev == b'_' {
        return None;
    }
    if count == 0 {
        // Only the octal prefix "0": decimal 0.
        return (prefix == b'0').then(|| (BigUint::zero(), 10, 1));
    }
    if dp >= 0 {
        count = dp - count;
    }
    Some((z, b, count))
}

/// math/big's scanExponent with a base-2 `p` and separators allowed: the exponent and
/// its base, 10 without one.
fn scan_exponent(r: &mut Scan<'_>) -> Option<(i64, u32)> {
    let Some(ch) = r.read() else {
        return Some((0, 10));
    };
    let base = match ch {
        b'e' | b'E' => 10,
        b'p' | b'P' => 2,
        _ => {
            r.unread();
            return Some((0, 10));
        }
    };
    let mut digits = String::new();
    let mut ch = r.read();
    if let Some(c @ (b'+' | b'-')) = ch {
        if c == b'-' {
            digits.push('-');
        }
        ch = r.read();
    }
    let (mut prev, mut inval_sep, mut has) = (b'.', false, false);
    while let Some(c) = ch {
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
            r.unread();
            break;
        }
        ch = r.read();
    }
    if !has {
        return None;
    }
    let exp = digits.parse::<i64>().ok()?;
    (!inval_sep && prev != b'_').then_some((exp, base))
}

/// Rat.SetString (ratconv.go, Go 1.26): a fraction `a/b` of integers in any base Go
/// reads, or a number with a point and an `e` or `p` exponent. The sign, and the
/// numerator and denominator in lowest terms.
fn rat(s: &str) -> Option<(bool, BigUint, BigUint)> {
    if s.is_empty() {
        return None;
    }
    let (neg, num, den) = if let Some((a, b)) = s.split_once('/') {
        let mut r = Scan {
            s: a.as_bytes(),
            i: 0,
        };
        let neg = scan_sign(&mut r)?;
        let (num, _, _) = nat_scan(&mut r, false)?;
        let mut d = Scan {
            s: b.as_bytes(),
            i: 0,
        };
        let (den, _, _) = nat_scan(&mut d, false)?;
        if !r.done() || !d.done() || den.is_zero() {
            return None;
        }
        (neg, num, den)
    } else {
        let mut r = Scan {
            s: s.as_bytes(),
            i: 0,
        };
        let neg = scan_sign(&mut r)?;
        let (m, base, fcount) = nat_scan(&mut r, true)?;
        let (exp, ebase) = scan_exponent(&mut r)?;
        if !r.done() {
            return None;
        }
        if m.is_zero() {
            return Some((false, m, BigUint::from(1u32)));
        }
        let (mut exp2, mut exp5) = (0i64, 0i64);
        if fcount < 0 {
            match base {
                10 => (exp2, exp5) = (fcount, fcount),
                2 => exp2 = fcount,
                8 => exp2 = fcount.wrapping_mul(3),
                _ => exp2 = fcount.wrapping_mul(4),
            }
        }
        if ebase == 10 {
            exp5 = exp5.wrapping_add(exp);
        }
        exp2 = exp2.wrapping_add(exp);
        let (mut num, mut den) = (m, BigUint::from(1u32));
        if exp5 != 0 {
            let n = exp5.checked_abs()?;
            if n > 1_000_000 {
                return None;
            }
            let pow5 = BigUint::from(5u32).pow(u32::try_from(n).ok()?);
            if exp5 > 0 {
                num *= pow5;
            } else {
                den = pow5;
            }
        }
        if !(-10_000_000..=10_000_000).contains(&exp2) {
            return None;
        }
        let shift = usize::try_from(exp2.unsigned_abs()).ok()?;
        if exp2 > 0 {
            num <<= shift;
        } else {
            den <<= shift;
        }
        (neg, num, den)
    };
    let g = num.gcd(&den);
    let (num, den) = if g.is_zero() {
        (num, den)
    } else {
        (num / &g, den / &g)
    };
    let neg = neg && !num.is_zero();
    Some((neg, num, den))
}

/// docker/cli's ParseCPUs: `value` as a rational number of CPUs, in billionths, which
/// must be whole; the int64 of Go's Int.Int64, the low 64 bits.
pub fn parse_cpus(value: &str) -> Result<i64, String> {
    let (neg, num, den) =
        rat(value).ok_or_else(|| format!("failed to parse {value} as a rational number"))?;
    let (nano, rest) = (num * 1_000_000_000u64).div_rem(&den);
    if !rest.is_zero() {
        return Err("value is too precise".into());
    }
    let low = nano.iter_u64_digits().next().unwrap_or(0);
    #[allow(clippy::cast_possible_wrap)]
    let v = low as i64;
    Ok(if neg { v.wrapping_neg() } else { v })
}

/// NanoCPUs.String: nothing for none, else the CPUs with three decimals, the last
/// rounded half away from zero (Rat.FloatString(3)).
pub fn nano_cpus_string(n: i64) -> String {
    if n == 0 {
        return String::new();
    }
    let abs = n.unsigned_abs();
    let (mut whole, frac) = (abs / 1_000_000_000, abs % 1_000_000_000);
    let mut milli = frac / 1_000_000;
    if (frac % 1_000_000) * 2 >= 1_000_000 {
        milli += 1;
        if milli == 1000 {
            whole += 1;
            milli = 0;
        }
    }
    let sign = if n < 0 { "-" } else { "" };
    format!("{sign}{whole}.{milli:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against go-units v0.5.0's size_test.go and what RAMInBytes makes of the rest.
    #[test]
    fn sizes_read_as_go_units_reads_them() {
        for (s, n) in [
            ("32", 32),
            ("32b", 32),
            ("32B", 32),
            ("32k", 32 * 1024),
            ("32K", 32 * 1024),
            ("32kb", 32 * 1024),
            ("32Kb", 32 * 1024),
            ("32Mb", 32 * 1024 * 1024),
            ("32Gb", 32 * 1024 * 1024 * 1024),
            ("32Tb", 32 * 1024 * 1024 * 1024 * 1024),
            ("32Pb", 32 * 1024 * 1024 * 1024 * 1024 * 1024),
            ("32PB", 32 * 1024 * 1024 * 1024 * 1024 * 1024),
            ("32P", 32 * 1024 * 1024 * 1024 * 1024 * 1024),
            ("32.3", 32),
            ("32.3 mb", 33_869_004),
            ("32.3 MiB", 33_869_004),
            ("0.3 MB", 314_572),
            ("1.5g", 1_610_612_736),
        ] {
            assert_eq!(ram_in_bytes(s), Ok(n), "{s}");
        }
        for (s, e) in [
            ("", "invalid size: ''"),
            ("hello", "invalid size: 'hello'"),
            ("-32", "invalid size: '-32'"),
            ("32 mbmb", "invalid suffix: 'mbmb'"),
            ("32m b", "strconv.ParseFloat: parsing \"32m\": invalid syntax"),
            ("32bm", "invalid suffix: 'bm'"),
            ("32mib", "32mib"),
        ] {
            if e == "32mib" {
                assert_eq!(ram_in_bytes(s), Ok(32 * 1024 * 1024));
            } else {
                assert_eq!(ram_in_bytes(s), Err(e.to_string()), "{s}");
            }
        }
        assert_eq!(
            ram_in_bytes(" 32 "),
            Err("strconv.ParseFloat: parsing \" 32\": invalid syntax".into())
        );
        assert_eq!(mem_bytes_string(536_870_912), "512MiB");
        assert_eq!(mem_bytes_string(-1), "-1B");
        assert_eq!(mem_bytes_string(0), "0");
    }

    #[test]
    fn cpus_read_as_math_big_reads_them() {
        for (s, n) in [
            ("1", 1_000_000_000),
            ("0.5", 500_000_000),
            ("1.5", 1_500_000_000),
            ("1/3", 0),
            ("3/2", 1_500_000_000),
            ("0x10", 16_000_000_000),
            ("1e-9", 1),
            ("2.5e1", 25_000_000_000),
            ("0b1.1", 1_500_000_000),
            ("1p1", 2_000_000_000),
            ("-1", -1_000_000_000),
            ("0", 0),
            ("1_0", 10_000_000_000),
            ("010/2", 4_000_000_000),
        ] {
            if s == "1/3" {
                assert_eq!(parse_cpus(s), Err("value is too precise".into()));
            } else {
                assert_eq!(parse_cpus(s), Ok(n), "{s}");
            }
        }
        for s in ["", "x", "1/0", "1/-2", "1.2.3", "1e", "_1", "1_", "0x", "1/"] {
            assert_eq!(
                parse_cpus(s),
                Err(format!("failed to parse {s} as a rational number")),
                "{s}"
            );
        }
        assert_eq!(nano_cpus_string(1_500_000_000), "1.500");
        assert_eq!(nano_cpus_string(1_999_500_000), "2.000");
        assert_eq!(nano_cpus_string(-500_000), "-0.001");
        assert_eq!(nano_cpus_string(0), "");
    }
}

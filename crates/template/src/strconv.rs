//! Go's strconv, as far as templates need it: quoting (quote.go), unquoting string and
//! character literals, number literals (atoi.go, atof.go) and float formatting
//! (internal/strconv/ftoa.go).

use std::fmt::Write as _;

/// `strconv.IsPrint`: letters, marks, numbers, punctuation, symbols and the ASCII space.
/// Rust's standard library has no general-category tables, so beyond ASCII this treats
/// controls, white space other than U+0020, private use and the format characters
/// listed in Unicode 15's Cf category as unprintable, and everything else as printable;
/// unassigned code points read as printable here where Go says they are not.
pub(crate) fn is_print(c: char) -> bool {
    let u = u32::from(c);
    if u < 0x80 {
        return (0x20..0x7f).contains(&u);
    }
    if c.is_control() || c.is_whitespace() {
        return false;
    }
    !matches!(u,
        0xad
        | 0x600..=0x605
        | 0x61c
        | 0x6dd
        | 0x70f
        | 0x890..=0x891
        | 0x8e2
        | 0x180e
        | 0x200b..=0x200f
        | 0x202a..=0x202e
        | 0x2060..=0x2064
        | 0x2066..=0x206f
        | 0xe000..=0xf8ff
        | 0xfeff
        | 0xfff9..=0xfffb
        | 0x110bd
        | 0x110cd
        | 0x13430..=0x1343f
        | 0x1bca0..=0x1bca3
        | 0x1d173..=0x1d17a
        | 0xe0001
        | 0xe0020..=0xe007f
        | 0xf0000..=0x10ffff)
}

const LOWERHEX: &[u8; 16] = b"0123456789abcdef";

fn hex_digit(n: u32) -> char {
    char::from(LOWERHEX.get((n & 0xf) as usize).copied().unwrap_or(b'0'))
}

/// quote.go's appendEscapedRune.
fn escaped_rune(out: &mut String, r: char, quote: char, ascii_only: bool) {
    if r == quote || r == '\\' {
        out.push('\\');
        out.push(r);
        return;
    }
    if ascii_only {
        if r.is_ascii() && is_print(r) {
            out.push(r);
            return;
        }
    } else if is_print(r) {
        out.push(r);
        return;
    }
    let u = u32::from(r);
    match r {
        '\x07' => out.push_str("\\a"),
        '\x08' => out.push_str("\\b"),
        '\x0c' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\x0b' => out.push_str("\\v"),
        _ if u < 0x20 || u == 0x7f => {
            out.push_str("\\x");
            out.push(hex_digit(u >> 4));
            out.push(hex_digit(u));
        }
        _ if u < 0x10000 => {
            out.push_str("\\u");
            for shift in [12, 8, 4, 0] {
                out.push(hex_digit(u >> shift));
            }
        }
        _ => {
            out.push_str("\\U");
            for shift in [28, 24, 20, 16, 12, 8, 4, 0] {
                out.push(hex_digit(u >> shift));
            }
        }
    }
}

/// `strconv.Quote`, or `strconv.QuoteToASCII` when `ascii_only`.
pub(crate) fn quote_with(s: &str, ascii_only: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        escaped_rune(&mut out, c, '"', ascii_only);
    }
    out.push('"');
    out
}

/// `strconv.Quote`.
pub(crate) fn quote(s: &str) -> String {
    quote_with(s, false)
}

/// `strconv.QuoteRune`, or `QuoteRuneToASCII` when `ascii_only`.
pub(crate) fn quote_rune(r: char, ascii_only: bool) -> String {
    let mut out = String::from("'");
    escaped_rune(&mut out, r, '\'', ascii_only);
    out.push('\'');
    out
}

/// `strconv.CanBackquote`.
pub(crate) fn can_backquote(s: &str) -> bool {
    s.chars().all(|c| {
        if c.len_utf8() > 1 {
            return c != '\u{feff}';
        }
        !((c < ' ' && c != '\t') || c == '`' || c == '\x7f')
    })
}

fn unhex(b: u8) -> Option<u32> {
    char::from(b).to_digit(16)
}

/// `strconv.UnquoteChar`: the value, whether it is a multibyte character rather than a
/// single byte, and the bytes consumed. Errors are Go's `ErrSyntax`.
pub(crate) fn unquote_char(s: &str, quote: u8) -> Result<(u32, bool, usize), ()> {
    let bytes = s.as_bytes();
    let c = *bytes.first().ok_or(())?;
    if c == quote && (quote == b'\'' || quote == b'"') {
        return Err(());
    }
    if c >= 0x80 {
        let r = s.chars().next().ok_or(())?;
        return Ok((u32::from(r), true, r.len_utf8()));
    }
    if c != b'\\' {
        return Ok((u32::from(c), false, 1));
    }
    let c = *bytes.get(1).ok_or(())?;
    let rest = bytes.get(2..).unwrap_or(&[]);
    let simple = |v: u8| Ok((u32::from(v), false, 2));
    match c {
        b'a' => simple(7),
        b'b' => simple(8),
        b'f' => simple(12),
        b'n' => simple(b'\n'),
        b'r' => simple(b'\r'),
        b't' => simple(b'\t'),
        b'v' => simple(11),
        b'x' | b'u' | b'U' => {
            let n = match c {
                b'x' => 2,
                b'u' => 4,
                _ => 8,
            };
            let digits = rest.get(..n).ok_or(())?;
            let mut v: u32 = 0;
            for &d in digits {
                v = (v << 4) | unhex(d).ok_or(())?;
            }
            if c == b'x' {
                return Ok((v, false, 2 + n));
            }
            if char::from_u32(v).is_none() {
                return Err(());
            }
            Ok((v, true, 2 + n))
        }
        b'0'..=b'7' => {
            let mut v = u32::from(c - b'0');
            let digits = rest.get(..2).ok_or(())?;
            for &d in digits {
                if !(b'0'..=b'7').contains(&d) {
                    return Err(());
                }
                v = (v << 3) | u32::from(d - b'0');
            }
            if v > 255 {
                return Err(());
            }
            Ok((v, false, 4))
        }
        b'\\' => simple(b'\\'),
        b'\'' | b'"' => {
            if c != quote {
                return Err(());
            }
            simple(c)
        }
        _ => Err(()),
    }
}

/// `strconv.Unquote`. A `\x` escape of a byte that is not UTF-8 by itself reads as
/// U+FFFD, since Rust strings are UTF-8.
pub(crate) fn unquote(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return Err(());
    }
    let quote = *bytes.first().ok_or(())?;
    let rest = s.get(1..).ok_or(())?;
    let end = rest.find(char::from(quote)).ok_or(())? + 2;
    match quote {
        b'`' => {
            if end != s.len() {
                return Err(());
            }
            let inner = s.get(1..end - 1).ok_or(())?;
            Ok(inner.replace('\r', ""))
        }
        b'"' | b'\'' => {
            let mut buf: Vec<u8> = Vec::new();
            let mut i = 1;
            loop {
                let tail = s.get(i..).ok_or(())?;
                match tail.as_bytes().first() {
                    None => return Err(()),
                    Some(&b) if b == quote => break,
                    Some(&b'\n') => return Err(()),
                    Some(_) => {}
                }
                let (r, multibyte, n) = unquote_char(tail, quote)?;
                i += n;
                if r < 0x80 || !multibyte {
                    buf.push(u8::try_from(r).map_err(|_| ())?);
                } else {
                    let c = char::from_u32(r).ok_or(())?;
                    let mut tmp = [0u8; 4];
                    buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
                }
                if quote == b'\'' {
                    break;
                }
            }
            if s.as_bytes().get(i) != Some(&quote) || i + 1 != s.len() {
                return Err(());
            }
            Ok(String::from_utf8_lossy(&buf).into_owned())
        }
        _ => Err(()),
    }
}

/// atoi.go's underscoreOK: underscores only between digits, as Go literals allow.
fn underscore_ok(s: &str) -> bool {
    let mut b = s.as_bytes();
    if let Some((b'-' | b'+', rest)) = b.split_first() {
        b = rest;
    }
    let mut saw = b'^';
    let mut i = 0;
    let mut hex = false;
    if b.len() >= 2
        && b.first() == Some(&b'0')
        && matches!(b.get(1).map(u8::to_ascii_lowercase), Some(b'b' | b'o' | b'x'))
    {
        i = 2;
        saw = b'0';
        hex = b.get(1).map(u8::to_ascii_lowercase) == Some(b'x');
    }
    while let Some(&c) = b.get(i) {
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

/// `strconv.ParseUint(s, 0, 64)`.
pub(crate) fn parse_uint(s0: &str) -> Option<u64> {
    let mut s = s0.as_bytes();
    if s.is_empty() {
        return None;
    }
    let mut base: u64 = 10;
    if s.first() == Some(&b'0') {
        let prefix = s.get(1).map(u8::to_ascii_lowercase);
        if s.len() >= 3 && prefix == Some(b'b') {
            base = 2;
            s = s.get(2..)?;
        } else if s.len() >= 3 && prefix == Some(b'o') {
            base = 8;
            s = s.get(2..)?;
        } else if s.len() >= 3 && prefix == Some(b'x') {
            base = 16;
            s = s.get(2..)?;
        } else {
            base = 8;
            s = s.get(1..)?;
        }
    }
    let mut underscores = false;
    let mut n: u64 = 0;
    for &c in s {
        let d = match c {
            b'_' => {
                underscores = true;
                continue;
            }
            b'0'..=b'9' => u64::from(c - b'0'),
            _ if c.to_ascii_lowercase().is_ascii_lowercase() => u64::from(c.to_ascii_lowercase() - b'a') + 10,
            _ => return None,
        };
        if d >= base {
            return None;
        }
        n = n.checked_mul(base)?.checked_add(d)?;
    }
    if underscores && !underscore_ok(s0) {
        return None;
    }
    Some(n)
}

/// `strconv.ParseInt(s, 0, 64)`.
pub(crate) fn parse_int(s: &str) -> Option<i64> {
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, s.get(1..)?),
        Some(b'+') => (false, s.get(1..)?),
        _ => (false, s),
    };
    let un = parse_uint(digits)?;
    // ParseInt's underscore check sees the sign too; parse_uint saw the digits alone.
    if digits.contains('_') && !underscore_ok(s) {
        return None;
    }
    const CUTOFF: u64 = 1 << 63;
    if !neg && un >= CUTOFF || neg && un > CUTOFF {
        return None;
    }
    if neg {
        Some(0i64.wrapping_sub_unsigned(un))
    } else {
        i64::try_from(un).ok()
    }
}

/// `strconv.ParseFloat(s, 64)` for the literals the template lexer passes it: decimal
/// and hexadecimal (with a `p` exponent) mantissas, underscores between digits.
pub(crate) fn parse_float(s: &str) -> Option<f64> {
    if s.contains('_') && !underscore_ok(s) {
        return None;
    }
    let clean: String = s.chars().filter(|&c| c != '_').collect();
    let (neg, body) = match clean.as_bytes().first() {
        Some(b'-') => (true, clean.get(1..)?),
        Some(b'+') => (false, clean.get(1..)?),
        _ => (false, clean.as_str()),
    };
    let lower = body.to_ascii_lowercase();
    let v = if let Some(hex) = lower.strip_prefix("0x") {
        hex_float(hex)?
    } else {
        if !body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            return None;
        }
        body.parse::<f64>().ok()?
    };
    if v.is_infinite() {
        return None;
    }
    Some(if neg { -v } else { v })
}

/// A hexadecimal float's mantissa and binary exponent (`1.8p3`).
fn hex_float(s: &str) -> Option<f64> {
    let (mant, exp) = s.split_once('p')?;
    let exp: i32 = exp.parse().ok()?;
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let mut v = 0f64;
    for c in int.chars().chain(frac.chars()) {
        v = v * 16.0 + f64::from(c.to_digit(16)?);
    }
    let shift = exp.checked_sub(i32::try_from(frac.len()).ok()?.checked_mul(4)?)?;
    Some(v * 2f64.powi(shift))
}

/// Decimal digits (no leading or trailing zeros) and the decimal point's position:
/// the value is 0.DIGITS × 10^dp. Zero has no digits.
struct Decimal {
    d: Vec<u8>,
    dp: i64,
}

impl Decimal {
    fn nd(&self) -> i64 {
        i64::try_from(self.d.len()).unwrap_or(i64::MAX)
    }

    fn digit(&self, i: i64) -> Option<u8> {
        usize::try_from(i).ok().and_then(|i| self.d.get(i).copied())
    }

    /// From Rust's `{:e}` form, `D.DDDDe±X`.
    fn from_exp(s: &str) -> Decimal {
        let (mant, exp) = s.split_once('e').unwrap_or((s, "0"));
        let exp: i64 = exp.parse().unwrap_or(0);
        let mut d: Vec<u8> = mant.bytes().filter(u8::is_ascii_digit).collect();
        let mut dp = exp + 1;
        while d.first() == Some(&b'0') {
            d.remove(0);
            dp -= 1;
        }
        while d.last() == Some(&b'0') {
            d.pop();
        }
        if d.is_empty() {
            dp = 0;
        }
        Decimal { d, dp }
    }

    /// From Rust's `{:.N}` form, `III.FFF`.
    fn from_fixed(s: &str) -> Decimal {
        let (int, frac) = s.split_once('.').unwrap_or((s, ""));
        let mut d: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
        let mut dp = i64::try_from(int.len()).unwrap_or(0);
        while d.first() == Some(&b'0') {
            d.remove(0);
            dp -= 1;
        }
        while d.last() == Some(&b'0') {
            d.pop();
        }
        if d.is_empty() {
            dp = 0;
        }
        Decimal { d, dp }
    }
}

/// ftoa.go's fmtE.
fn fmt_e(out: &mut String, neg: bool, d: &Decimal, prec: i64, fmt: u8) {
    if neg {
        out.push('-');
    }
    out.push(char::from(d.digit(0).unwrap_or(b'0')));
    if prec > 0 {
        out.push('.');
        let mut i = 1;
        let m = d.nd().min(prec + 1);
        while i < m {
            out.push(char::from(d.digit(i).unwrap_or(b'0')));
            i += 1;
        }
        while i <= prec {
            out.push('0');
            i += 1;
        }
    }
    out.push(char::from(fmt));
    let mut exp = d.dp - 1;
    if d.d.is_empty() {
        exp = 0;
    }
    if exp < 0 {
        out.push('-');
        exp = -exp;
    } else {
        out.push('+');
    }
    if exp < 10 {
        out.push('0');
    }
    let _ = write!(out, "{exp}");
}

/// ftoa.go's fmtF.
fn fmt_f(out: &mut String, neg: bool, d: &Decimal, prec: i64) {
    if neg {
        out.push('-');
    }
    if d.dp > 0 {
        let m = d.nd().min(d.dp);
        for i in 0..m {
            out.push(char::from(d.digit(i).unwrap_or(b'0')));
        }
        for _ in m..d.dp {
            out.push('0');
        }
    } else {
        out.push('0');
    }
    if prec > 0 {
        out.push('.');
        for i in 0..prec {
            out.push(char::from(d.digit(d.dp + i).unwrap_or(b'0')));
        }
    }
}

/// ftoa.go's formatDigits.
fn format_digits(out: &mut String, shortest: bool, neg: bool, d: &Decimal, prec: i64, fmt: u8) {
    match fmt {
        b'e' | b'E' => fmt_e(out, neg, d, prec, fmt),
        b'f' => fmt_f(out, neg, d, prec),
        _ => {
            let mut prec = prec;
            let mut eprec = prec;
            if eprec > d.nd() && d.nd() >= d.dp {
                eprec = d.nd();
            }
            if shortest {
                eprec = 6;
            }
            let exp = d.dp - 1;
            if exp < -4 || exp >= eprec {
                if prec > d.nd() {
                    prec = d.nd();
                }
                fmt_e(out, neg, d, prec - 1, fmt + b'e' - b'g');
                return;
            }
            if prec > d.dp {
                prec = d.nd();
            }
            fmt_f(out, neg, d, (prec - d.dp).max(0));
        }
    }
}

/// `strconv.FormatFloat(v, fmt, prec, 64)` for the formats b, e, E, f, g and G.
pub(crate) fn format_float(v: f64, fmt: u8, prec: i64) -> String {
    let mut out = String::new();
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Inf" } else { "+Inf" }.into();
    }
    let neg = v.is_sign_negative();
    let a = v.abs();
    if fmt == b'b' {
        let bits = v.to_bits();
        let mut exp = i64::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
        let mut mant = bits & ((1u64 << 52) - 1);
        if exp == 0 {
            exp += 1;
        } else {
            mant |= 1u64 << 52;
        }
        exp += -1023 - 52;
        if neg {
            out.push('-');
        }
        let _ = write!(out, "{mant}p");
        if exp >= 0 {
            out.push('+');
        }
        let _ = write!(out, "{exp}");
        return out;
    }
    if a == 0.0 {
        let d = Decimal { d: Vec::new(), dp: 0 };
        format_digits(&mut out, prec < 0, neg, &d, prec, fmt);
        return out;
    }
    if prec < 0 {
        let d = Decimal::from_exp(&format!("{a:e}"));
        let prec = match fmt {
            b'e' | b'E' => (d.nd() - 1).max(0),
            b'f' => (d.nd() - d.dp).max(0),
            _ => d.nd(),
        };
        format_digits(&mut out, true, neg, &d, prec, fmt);
        return out;
    }
    let (d, prec) = match fmt {
        b'f' => {
            let p = usize::try_from(prec).unwrap_or(0);
            (Decimal::from_fixed(&format!("{a:.p$}")), prec)
        }
        b'e' | b'E' => {
            let p = usize::try_from(prec).unwrap_or(0);
            (Decimal::from_exp(&format!("{a:.p$e}")), prec)
        }
        _ => {
            let prec = prec.max(1);
            let p = usize::try_from(prec - 1).unwrap_or(0);
            (Decimal::from_exp(&format!("{a:.p$e}")), prec)
        }
    };
    format_digits(&mut out, false, neg, &d, prec, fmt);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_format_as_go_formats_them() {
        assert_eq!(format_float(1e6, b'g', -1), "1e+06");
        assert_eq!(format_float(123456.0, b'g', -1), "123456");
        assert_eq!(format_float(0.0, b'g', -1), "0");
        assert_eq!(format_float(1.5, b'g', 3), "1.5");
        assert_eq!(format_float(0.000012, b'g', -1), "1.2e-05");
        assert_eq!(format_float(1.23456, b'f', 2), "1.23");
        assert_eq!(format_float(0.001, b'f', 2), "0.00");
        assert_eq!(format_float(1.0, b'e', 6), "1.000000e+00");
        assert_eq!(format_float(1e21, b'f', -1), "1000000000000000000000");
    }

    #[test]
    fn literals_unquote_as_go_unquotes_them() {
        assert_eq!(unquote(r#""a\tb\x41é""#), Ok("a\tbAé".into()));
        assert_eq!(unquote("`a\\n`"), Ok("a\\n".into()));
        assert_eq!(unquote(r#""a"#), Err(()));
        assert_eq!(quote("a\"\n\u{1}é"), r#""a\"\n\x01é""#);
        assert_eq!(parse_int("-0x10"), Some(-16));
        assert_eq!(parse_uint("1_000"), Some(1000));
        assert_eq!(parse_uint("_1"), None);
        assert_eq!(parse_float("0x1.8p1"), Some(3.0));
    }
}

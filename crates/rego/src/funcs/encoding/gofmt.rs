//! Go's strconv and fmt as the encoding builtins' texts need them: FormatFloat's
//! shortest `'e'`, `'f'` and `'g'` (go1.26 strconv/ftoa.go), encoding/json's float
//! form, Quote over bytes that may not be UTF-8, and Go's reading of such bytes as a
//! string (each byte of a bad sequence one U+FFFD, as utf8.DecodeRune reads them).

use crate::goquote;

/// The shortest decimal digits that read back as `f` (|f|, finite, non-zero), and the
/// decimal point's place: f = 0.d1d2… × 10^dp.
fn shortest(f: f64, bits32: bool) -> (Vec<u8>, i64) {
    let s = if bits32 {
        // Truncation to f32 is the conversion Go's FormatFloat makes for bitSize 32.
        #[allow(clippy::cast_possible_truncation)]
        let x = f.abs() as f32;
        format!("{x:e}")
    } else {
        format!("{:e}", f.abs())
    };
    let (mant, exp) = s.split_once('e').unwrap_or((s.as_str(), "0"));
    let digits: Vec<u8> = mant.bytes().filter(u8::is_ascii_digit).collect();
    let exp: i64 = exp.parse().unwrap_or(0);
    (digits, exp + 1)
}

fn fmt_e(out: &mut String, digits: &[u8], dp: i64, prec: i64, zero: bool) {
    out.push(char::from(if zero { b'0' } else { digits.first().copied().unwrap_or(b'0') }));
    if prec > 0 {
        out.push('.');
        let mut i = 1usize;
        let n = usize::try_from(prec).unwrap_or(0) + 1;
        while i < n {
            out.push(char::from(digits.get(i).copied().unwrap_or(b'0')));
            i += 1;
        }
    }
    out.push('e');
    let mut exp = if zero { 0 } else { dp - 1 };
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

fn fmt_f(out: &mut String, digits: &[u8], dp: i64, prec: i64) {
    if dp > 0 {
        let mut m = 0i64;
        while m < dp {
            let d = usize::try_from(m).ok().and_then(|i| digits.get(i)).copied().unwrap_or(b'0');
            out.push(char::from(d));
            m += 1;
        }
    } else {
        out.push('0');
    }
    if prec > 0 {
        out.push('.');
        for i in 1..=prec {
            let j = dp + i - 1;
            let d = if j < 0 {
                b'0'
            } else {
                usize::try_from(j).ok().and_then(|j| digits.get(j)).copied().unwrap_or(b'0')
            };
            out.push(char::from(d));
        }
    }
}

/// strconv.FormatFloat(f, fmt, -1, bits) for `'e'`, `'f'` and `'g'`.
pub fn format_float(f: f64, fmt: u8, bits32: bool) -> String {
    let f = if bits32 {
        #[allow(clippy::cast_possible_truncation)]
        let x = f as f32;
        f64::from(x)
    } else {
        f
    };
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf" } else { "-Inf" }.to_string();
    }
    let mut out = String::new();
    if f.is_sign_negative() {
        out.push('-');
    }
    let zero = f == 0.0;
    let (digits, dp) = if zero { (Vec::new(), 0) } else { shortest(f, bits32) };
    let nd = i64::try_from(digits.len()).unwrap_or(0);
    match fmt {
        b'e' => fmt_e(&mut out, &digits, dp, (nd - 1).max(0), zero),
        b'f' => fmt_f(&mut out, &digits, dp, (nd - dp).max(0)),
        _ => {
            let exp = dp - 1;
            if exp < -4 || exp >= 6 {
                fmt_e(&mut out, &digits, dp, nd - 1, zero);
            } else {
                fmt_f(&mut out, &digits, dp, (nd - dp).max(0));
            }
        }
    }
    out
}

/// encoding/json's float64 text (floatEncoder), for a finite f.
pub fn json_float(f: f64) -> String {
    let abs = f.abs();
    let fmt = if abs != 0.0 && (abs < 1e-6 || abs >= 1e21) { b'e' } else { b'f' };
    let mut s = format_float(f, fmt, false);
    if fmt == b'e' {
        // e-09 to e-9
        let b = s.as_bytes();
        let n = b.len();
        if n >= 4 && b.get(n - 4) == Some(&b'e') && b.get(n - 3) == Some(&b'-') && b.get(n - 2) == Some(&b'0') {
            let last = b.get(n - 1).copied().unwrap_or(b'0');
            s.truncate(n - 2);
            s.push(char::from(last));
        }
    }
    s
}

/// Go's reading of bytes as a string: each byte of a bad UTF-8 sequence one U+FFFD.
pub fn lossy(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let (c, w) = decode_rune(b.get(i..).unwrap_or_default());
        out.push(c.unwrap_or('\u{FFFD}'));
        i += w.max(1);
    }
    out
}

/// utf8.DecodeRune: the rune and its width, None for a bad sequence (width 1).
pub fn decode_rune(b: &[u8]) -> (Option<char>, usize) {
    let n = match b.first() {
        None => return (None, 0),
        Some(&c) if c < 0x80 => return (Some(char::from(c)), 1),
        Some(&c) if (0xC2..=0xDF).contains(&c) => 2,
        Some(&c) if (0xE0..=0xEF).contains(&c) => 3,
        Some(&c) if (0xF0..=0xF4).contains(&c) => 4,
        Some(_) => return (None, 1),
    };
    match b.get(..n).map(std::str::from_utf8) {
        Some(Ok(s)) => (s.chars().next(), n),
        _ => (None, 1),
    }
}

/// strconv.Quote over bytes that may not be UTF-8.
pub fn quote_bytes(b: &[u8]) -> String {
    let mut out = String::from("\"");
    let mut i = 0;
    while i < b.len() {
        let (c, w) = decode_rune(b.get(i..).unwrap_or_default());
        match c {
            Some(c) => {
                let mut tmp = String::new();
                goquote::quote(&mut tmp, c.encode_utf8(&mut [0; 4]));
                // Drop the quotes around the one character.
                let inner = tmp.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(&tmp);
                out.push_str(inner);
            }
            None => out.push_str(&format!("\\x{:02x}", b.get(i).copied().unwrap_or(0))),
        }
        i += w.max(1);
    }
    out.push('"');
    out
}

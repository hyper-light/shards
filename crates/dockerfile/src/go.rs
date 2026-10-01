//! Go's handling of strings, which BuildKit's parser and lexer inherit: strings are bytes,
//! decoded to runes as `utf8.DecodeRune` decodes them (an invalid byte is U+FFFD, one byte
//! wide), runes written back as `utf8.AppendRune` writes them, Unicode classes as Go
//! 1.26's `unicode` package has them (`tables`), and `strconv.Quote`.

use std::fmt::Write as _;

use crate::tables;

/// `b` from `at` on, empty past its end.
pub(crate) fn tail(b: &[u8], at: usize) -> &[u8] {
    b.get(at..).unwrap_or_default()
}

/// `b` up to `end`, all of it if shorter.
pub(crate) fn head(b: &[u8], end: usize) -> &[u8] {
    b.get(..end).unwrap_or(b)
}

/// `b` from `start` to `end`, empty where that is not within it.
pub(crate) fn span(b: &[u8], start: usize, end: usize) -> &[u8] {
    b.get(start..end).unwrap_or_default()
}

/// `utf8.RuneError`, what an invalid byte decodes to.
pub(crate) const RUNE_ERROR: u32 = 0xFFFD;

/// The rune at the start of `b`, and its width: `utf8.DecodeRune`. An invalid or
/// incomplete sequence is U+FFFD one byte wide; nothing is U+FFFD zero wide.
pub(crate) fn decode(b: &[u8]) -> (u32, usize) {
    let Some(&lead) = b.first() else {
        return (RUNE_ERROR, 0);
    };
    if lead < 0x80 {
        return (u32::from(lead), 1);
    }
    let width = match lead {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return (RUNE_ERROR, 1),
    };
    // Go and Rust validate alike: no overlong forms, no surrogates, nothing past U+10FFFF.
    match b.get(..width).and_then(|s| std::str::from_utf8(s).ok()) {
        Some(s) => (s.chars().next().map_or(RUNE_ERROR, u32::from), width),
        None => (RUNE_ERROR, 1),
    }
}

/// The rune at the end of `b`, and its width: `utf8.DecodeLastRune`.
pub(crate) fn decode_last(b: &[u8]) -> (u32, usize) {
    let end = b.len();
    let Some(&last) = b.last() else {
        return (RUNE_ERROR, 0);
    };
    if last < 0x80 {
        return (u32::from(last), 1);
    }
    // Back to the nearest byte that can start a rune, no further than four back.
    let lim = end.saturating_sub(4);
    let mut start = end - 1;
    while start > lim {
        start -= 1;
        if b.get(start).is_some_and(|&c| c & 0xC0 != 0x80) {
            break;
        }
    }
    let (r, w) = decode(tail(b, start));
    if start + w != end {
        return (RUNE_ERROR, 1);
    }
    (r, w)
}

/// The runes of `b` in order, each with its width.
pub(crate) fn runes(b: &[u8]) -> impl Iterator<Item = (u32, usize)> + '_ {
    let mut at = 0;
    std::iter::from_fn(move || {
        let rest = b.get(at..).filter(|r| !r.is_empty())?;
        let (r, w) = decode(rest);
        at += w;
        Some((r, w))
    })
}

/// Appends `r` as UTF-8: `utf8.AppendRune`, which writes U+FFFD for what is no rune.
pub(crate) fn push(out: &mut Vec<u8>, r: u32) {
    let c = char::from_u32(r).unwrap_or('\u{FFFD}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn in_table(table: &[(u32, u32)], r: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < r {
                std::cmp::Ordering::Less
            } else if lo > r {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `unicode.IsSpace`.
pub(crate) fn is_space(r: u32) -> bool {
    in_table(tables::SPACE, r)
}

/// `unicode.IsLetter`.
pub(crate) fn is_letter(r: u32) -> bool {
    in_table(tables::LETTER, r)
}

/// `unicode.IsDigit`.
pub(crate) fn is_digit(r: u32) -> bool {
    in_table(tables::DIGIT, r)
}

/// `unicode.ToLower`.
fn lower(r: u32) -> u32 {
    if r < 0x80 {
        return u32::from((r as u8).to_ascii_lowercase());
    }
    tables::LOWER
        .binary_search_by_key(&r, |&(from, _)| from)
        .ok()
        .and_then(|at| tables::LOWER.get(at))
        .map_or(r, |&(_, to)| to)
}

/// `strings.ToLower`: ASCII lowered byte by byte, and otherwise rune by rune, an invalid
/// byte becoming U+FFFD.
pub(crate) fn to_lower(b: &[u8]) -> Vec<u8> {
    if b.is_ascii() {
        return b.to_ascii_lowercase();
    }
    let mut out = Vec::with_capacity(b.len());
    for (r, _) in runes(b) {
        push(&mut out, lower(r));
    }
    out
}

/// `unicode.ToUpper`.
fn upper(r: u32) -> u32 {
    if r < 0x80 {
        return u32::from((r as u8).to_ascii_uppercase());
    }
    tables::UPPER
        .binary_search_by_key(&r, |&(from, _)| from)
        .ok()
        .and_then(|at| tables::UPPER.get(at))
        .map_or(r, |&(_, to)| to)
}

/// `strings.ToUpper`, as [`to_lower`] lowers.
pub(crate) fn to_upper(b: &[u8]) -> Vec<u8> {
    if b.is_ascii() {
        return b.to_ascii_uppercase();
    }
    let mut out = Vec::with_capacity(b.len());
    for (r, _) in runes(b) {
        push(&mut out, upper(r));
    }
    out
}

/// `strconv.ParseBool`'s accepted words.
pub(crate) fn parse_bool(b: &[u8]) -> Option<bool> {
    match b {
        b"1" | b"t" | b"T" | b"TRUE" | b"true" | b"True" => Some(true),
        b"0" | b"f" | b"F" | b"FALSE" | b"false" | b"False" => Some(false),
        _ => None,
    }
}

/// `strconv.ParseInt(s, 10, 32)`, and its error as Go words it.
pub(crate) fn parse_int32(s: &[u8]) -> Result<i64, Vec<u8>> {
    let fail = |why: &str| {
        let mut m = b"strconv.ParseInt: parsing ".to_vec();
        m.extend_from_slice(quote(s).as_bytes());
        m.extend_from_slice(b": ");
        m.extend_from_slice(why.as_bytes());
        m
    };
    let (neg, digits) = match s.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(fail("invalid syntax"));
    }
    let mut n: u64 = 0;
    let mut over = false;
    for &d in digits {
        match n.checked_mul(10).and_then(|v| v.checked_add(u64::from(d - b'0'))) {
            Some(v) => n = v,
            None => over = true,
        }
    }
    let limit: u64 = if neg { 1 << 31 } else { (1 << 31) - 1 };
    if over || n > limit {
        return Err(fail("value out of range"));
    }
    let n = i64::try_from(n).map_err(|_| fail("value out of range"))?;
    Ok(if neg { -n } else { n })
}

/// `time`'s own quoting in its errors: non-ASCII and control bytes as `\xNN`, but a
/// U+FFFD that ends the string written as its first byte alone (Go's bound is one short).
pub(crate) fn time_quote(s: &[u8]) -> Vec<u8> {
    let mut out = vec![b'"'];
    let mut i = 0;
    while i < s.len() {
        let (c, w) = decode(tail(s, i));
        if !(0x20..0x80).contains(&c) {
            let width = if c == RUNE_ERROR {
                if i + 2 < s.len() && span(s, i, i + 3) == "\u{FFFD}".as_bytes() {
                    3
                } else {
                    1
                }
            } else {
                w
            };
            for &b in span(s, i, i + width) {
                out.extend_from_slice(format!("\\x{b:02x}").as_bytes());
            }
        } else {
            if c == u32::from('"') || c == u32::from('\\') {
                out.push(b'\\');
            }
            out.push(c as u8);
        }
        i += w;
    }
    out.push(b'"');
    out
}

/// `time.ParseDuration`, in nanoseconds, with its errors.
pub(crate) fn parse_duration(orig: &[u8]) -> Result<i64, Vec<u8>> {
    let fail = |what: &[u8]| {
        let mut m = b"time: ".to_vec();
        m.extend_from_slice(what);
        m.push(b' ');
        m.extend_from_slice(&time_quote(orig));
        m
    };
    let invalid = || fail(b"invalid duration");
    let mut s = orig;
    let mut neg = false;
    if let Some((&c, rest)) = s.split_first()
        && (c == b'-' || c == b'+')
    {
        neg = c == b'-';
        s = rest;
    }
    if s == b"0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }
    const LIMIT: u64 = 1 << 63;
    let mut d: u64 = 0;
    while !s.is_empty() {
        if !s.first().is_some_and(|&c| c == b'.' || c.is_ascii_digit()) {
            return Err(invalid());
        }
        // Leading integer.
        let mut v: u64 = 0;
        let mut i = 0;
        while let Some(&c) = s.get(i).filter(|c| c.is_ascii_digit()) {
            if v > LIMIT / 10 {
                return Err(invalid());
            }
            v = v * 10 + u64::from(c - b'0');
            if v > LIMIT {
                return Err(invalid());
            }
            i += 1;
        }
        let pre = i > 0;
        s = tail(s, i);
        let (mut f, mut scale, mut post) = (0u64, 1f64, false);
        if s.first() == Some(&b'.') {
            s = tail(s, 1);
            let mut i = 0;
            let mut overflow = false;
            while let Some(&c) = s.get(i).filter(|c| c.is_ascii_digit()) {
                i += 1;
                if overflow {
                    continue;
                }
                if f > (LIMIT - 1) / 10 {
                    overflow = true;
                    continue;
                }
                let y = f * 10 + u64::from(c - b'0');
                if y > LIMIT {
                    overflow = true;
                    continue;
                }
                f = y;
                scale *= 10.0;
            }
            post = i > 0;
            s = tail(s, i);
        }
        if !pre && !post {
            return Err(invalid());
        }
        let i = s
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if i == 0 {
            return Err(fail(b"missing unit in duration"));
        }
        let unit_text = head(s, i);
        s = tail(s, i);
        let unit: u64 = match unit_text {
            b"ns" => 1,
            b"us" => 1_000,
            b"\xc2\xb5s" | b"\xce\xbcs" => 1_000,
            b"ms" => 1_000_000,
            b"s" => 1_000_000_000,
            b"m" => 60_000_000_000,
            b"h" => 3_600_000_000_000,
            _ => {
                let mut what = b"unknown unit ".to_vec();
                what.extend_from_slice(&time_quote(unit_text));
                what.extend_from_slice(b" in duration");
                return Err(fail(&what));
            }
        };
        if v > LIMIT / unit {
            return Err(invalid());
        }
        v *= unit;
        if f > 0 {
            // Go's own float arithmetic: the same rounding.
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > LIMIT {
                return Err(invalid());
            }
        }
        d += v;
        if d > LIMIT {
            return Err(invalid());
        }
    }
    if neg {
        return Ok((d as i64).wrapping_neg());
    }
    if d > LIMIT - 1 {
        return Err(invalid());
    }
    Ok(d as i64)
}

/// Levenshtein distance over runes (agext/levenshtein's, all costs 1).
pub(crate) fn levenshtein(a: &[u8], b: &[u8]) -> usize {
    let a: Vec<u32> = runes(a).map(|(r, _)| r).collect();
    let b: Vec<u32> = runes(b).map(|(r, _)| r).collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        if let Some(c) = cur.first_mut() {
            *c = i + 1;
        }
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev
                .get(j)
                .copied()
                .unwrap_or(usize::MAX)
                .saturating_add(usize::from(ca != cb));
            let del = prev.get(j + 1).copied().unwrap_or(usize::MAX).saturating_add(1);
            let ins = cur.get(j).copied().unwrap_or(usize::MAX).saturating_add(1);
            if let Some(c) = cur.get_mut(j + 1) {
                *c = sub.min(del).min(ins);
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev.last().copied().unwrap_or(0)
}

/// `strconv.IsPrint`.
fn is_print(r: u32) -> bool {
    if r < 0x80 {
        return (0x20..0x7f).contains(&r);
    }
    !in_table(tables::NOT_PRINT, r)
}

/// `bytes.TrimLeftFunc(b, unicode.IsSpace)`.
pub(crate) fn trim_left_space(b: &[u8]) -> &[u8] {
    let mut at = 0;
    while at < b.len() {
        let (r, w) = decode(tail(b, at));
        if !is_space(r) {
            break;
        }
        at += w;
    }
    tail(b, at)
}

/// `bytes.TrimRightFunc(b, unicode.IsSpace)`.
pub(crate) fn trim_right_space(b: &[u8]) -> &[u8] {
    let mut end = b.len();
    while end > 0 {
        let (r, w) = decode_last(head(b, end));
        if !is_space(r) {
            break;
        }
        end -= w;
    }
    head(b, end)
}

/// `strings.TrimSpace`.
pub(crate) fn trim_space(b: &[u8]) -> &[u8] {
    trim_right_space(trim_left_space(b))
}

/// `path.Clean`: the shortest path naming what `p` names, lexically: one slash between
/// elements, no `.` elements, `..` eating the element before it, and none at the root.
pub(crate) fn clean(p: &[u8]) -> Vec<u8> {
    if p.is_empty() {
        return b".".to_vec();
    }
    let rooted = p.first() == Some(&b'/');
    let mut out: Vec<u8> = Vec::with_capacity(p.len());
    if rooted {
        out.push(b'/');
    }
    // Where `..` stops: after the root, or after the `..` elements that lead.
    let mut dotdot = out.len();
    for element in p.split(|&b| b == b'/') {
        match element {
            b"" | b"." => {}
            b".." => {
                if out.len() > dotdot {
                    // Back to the slash before the last element, or its start.
                    let mut end = out.len() - 1;
                    while end > dotdot && out.get(end) != Some(&b'/') {
                        end -= 1;
                    }
                    out.truncate(end);
                } else if !rooted {
                    if !out.is_empty() {
                        out.push(b'/');
                    }
                    out.extend_from_slice(b"..");
                    dotdot = out.len();
                }
            }
            _ => {
                if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                    out.push(b'/');
                }
                out.extend_from_slice(element);
            }
        }
    }
    if out.is_empty() {
        out.push(b'.');
    }
    out
}

/// `path.Join`: the non-empty elements joined by slashes, cleaned; empty if all are.
pub(crate) fn join(elements: &[&[u8]]) -> Vec<u8> {
    let parts: Vec<&[u8]> = elements.iter().copied().filter(|e| !e.is_empty()).collect();
    if parts.is_empty() {
        return Vec::new();
    }
    clean(&parts.join(&b'/'))
}

/// `strings.EqualFold(s, name)` for an ASCII `name`, as Go's `encoding/json` matches a
/// key to a field: the only runes beyond ASCII whose case-folding orbit holds an ASCII
/// letter are U+212A KELVIN SIGN (with `k`) and U+017F LATIN SMALL LETTER LONG S (with
/// `s`) (Unicode's CaseFolding.txt; Go's `unicode.SimpleFold`).
pub(crate) fn equal_fold_ascii(s: &[u8], name: &[u8]) -> bool {
    let mut runes = runes(s).map(|(r, _)| r);
    for &c in name {
        let Some(r) = runes.next() else {
            return false;
        };
        let folds = match r {
            0x212A => c.eq_ignore_ascii_case(&b'k'),
            0x17F => c.eq_ignore_ascii_case(&b's'),
            _ => u8::try_from(r).is_ok_and(|b| b.is_ascii() && b.eq_ignore_ascii_case(&c)),
        };
        if !folds {
            return false;
        }
    }
    runes.next().is_none()
}

/// A time as Go's `time.Parse(time.RFC3339, ...)` reads it: its wall clock where it was
/// written, and that place's offset from UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Time {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub nanosecond: u32,
    /// Seconds east of UTC.
    pub offset: i32,
}

const RFC3339: &[u8] = b"2006-01-02T15:04:05Z07:00";

fn is_leap(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn days_in(month: u32, year: u32) -> u32 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `getnum`: one or two digits, two if `fixed`.
fn getnum(s: &[u8], fixed: bool) -> Option<(u32, &[u8])> {
    let d = |i: usize| {
        s.get(i)
            .filter(|c| c.is_ascii_digit())
            .map(|&c| u32::from(c - b'0'))
    };
    let first = d(0)?;
    match d(1) {
        Some(second) => Some((first * 10 + second, tail(s, 2))),
        None if fixed => None,
        None => Some((first, tail(s, 1))),
    }
}

/// `time.Parse(time.RFC3339, value)`, which `Time.UnmarshalJSON` uses: more lenient than
/// RFC 3339 (a one-digit hour, a comma before the fraction, offsets to 24:60), and failing
/// with its error's text.
pub fn parse_rfc3339(value: &[u8]) -> Result<Time, Vec<u8>> {
    let quoted = |b: &[u8]| time_quote(b);
    let cannot = |rest: &[u8], elem: &[u8]| -> Vec<u8> {
        [
            b"parsing time ".as_slice(),
            &quoted(value),
            b" as ",
            &quoted(RFC3339),
            b": cannot parse ",
            &quoted(rest),
            b" as ",
            &quoted(elem),
        ]
        .concat()
    };
    let range = |what: &str| -> Vec<u8> {
        [
            b"parsing time ".as_slice(),
            &quoted(value),
            b": ",
            what.as_bytes(),
            b" out of range",
        ]
        .concat()
    };
    let skip = |rest: &'_ [u8], lit: u8| -> Option<usize> { (rest.first() == Some(&lit)).then_some(1) };

    let mut rest = value;
    // 2006
    let year = match rest.get(..4) {
        Some(p) if p.iter().all(u8::is_ascii_digit) => {
            p.iter().fold(0u32, |y, &c| y * 10 + u32::from(c - b'0'))
        }
        _ => return Err(cannot(rest, b"2006")),
    };
    rest = tail(rest, 4);
    // -01
    rest = tail(rest, skip(rest, b'-').ok_or_else(|| cannot(rest, b"-"))?);
    let (month, r) = getnum(rest, true).ok_or_else(|| cannot(rest, b"01"))?;
    if !(1..=12).contains(&month) {
        return Err(range("month"));
    }
    rest = r;
    // -02
    rest = tail(rest, skip(rest, b'-').ok_or_else(|| cannot(rest, b"-"))?);
    let (day, r) = getnum(rest, true).ok_or_else(|| cannot(rest, b"02"))?;
    rest = r;
    // T15
    rest = tail(rest, skip(rest, b'T').ok_or_else(|| cannot(rest, b"T"))?);
    let (hour, r) = getnum(rest, false).ok_or_else(|| cannot(rest, b"15"))?;
    if hour >= 24 {
        return Err(range("hour"));
    }
    rest = r;
    // :04
    rest = tail(rest, skip(rest, b':').ok_or_else(|| cannot(rest, b":"))?);
    let (minute, r) = getnum(rest, true).ok_or_else(|| cannot(rest, b"04"))?;
    if minute >= 60 {
        return Err(range("minute"));
    }
    rest = r;
    // :05, and any fraction after it
    rest = tail(rest, skip(rest, b':').ok_or_else(|| cannot(rest, b":"))?);
    let (second, r) = getnum(rest, true).ok_or_else(|| cannot(rest, b"05"))?;
    if second >= 60 {
        return Err(range("second"));
    }
    rest = r;
    let mut nanosecond = 0;
    if matches!(rest.first(), Some(b'.' | b',')) && rest.get(1).is_some_and(u8::is_ascii_digit) {
        let n = 1 + tail(rest, 1).iter().take_while(|c| c.is_ascii_digit()).count();
        // parseNanoseconds: at most nine digits count.
        let digits = span(rest, 1, n.min(10));
        let mut ns = digits.iter().fold(0u32, |v, &c| v * 10 + u32::from(c - b'0'));
        for _ in digits.len()..9 {
            ns *= 10;
        }
        nanosecond = ns;
        rest = tail(rest, n);
    }
    // Z07:00
    let offset = if rest.first() == Some(&b'Z') {
        rest = tail(rest, 1);
        0
    } else {
        let zone = rest;
        if rest.len() < 6 || rest.get(3) != Some(&b':') {
            return Err(cannot(zone, b"Z07:00"));
        }
        // Go parses the hour, then the minute if the hour parsed; reports a value out of
        // range before a malformed one, the minute's range over the hour's.
        let two = |b: &[u8]| getnum(b, true).map(|(n, _)| n);
        let hr = two(span(rest, 1, 3));
        let mm = hr.and_then(|_| two(span(rest, 4, 6)));
        if mm.is_some_and(|m| m > 60) {
            return Err(range("time zone offset minute"));
        }
        if hr.is_some_and(|h| h > 24) {
            return Err(range("time zone offset hour"));
        }
        let (Some(hr), Some(mm)) = (hr, mm) else {
            return Err(cannot(zone, b"Z07:00"));
        };
        let sign = match rest.first() {
            Some(b'+') => 1,
            Some(b'-') => -1,
            _ => return Err(cannot(zone, b"Z07:00")),
        };
        rest = tail(rest, 6);
        sign * i32::try_from((hr * 60 + mm) * 60).unwrap_or_default()
    };
    if !rest.is_empty() {
        return Err([
            b"parsing time ".as_slice(),
            &quoted(value),
            b": extra text: ",
            &quoted(rest),
        ]
        .concat());
    }
    if day < 1 || day > days_in(month, year) {
        return Err(range("day"));
    }
    Ok(Time {
        year,
        month,
        day,
        hour,
        minute,
        second,
        nanosecond,
        offset,
    })
}

impl Time {
    /// `Time.MarshalJSON`'s text without its quotes: RFC 3339 with the fraction's trailing
    /// zeros dropped, and `Z` for UTC. Fails where Go's does, for an offset of 24 hours or
    /// more.
    pub fn rfc3339_nano(&self) -> Result<String, Vec<u8>> {
        // The wall clock in its own zone is the one parsed: Go adds the offset back.
        let mut out = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        if self.nanosecond != 0 {
            let frac = format!("{:09}", self.nanosecond);
            out.push('.');
            out.push_str(frac.trim_end_matches('0'));
        }
        if self.offset == 0 {
            out.push('Z');
            return Ok(out);
        }
        let zone = self.offset / 60;
        let (sign, zone) = if zone < 0 { ('-', -zone) } else { ('+', zone) };
        if zone / 60 >= 24 {
            return Err(b"Time.MarshalJSON: timezone hour outside of range [0,23]".to_vec());
        }
        let _ = write!(out, "{sign}{:02}:{:02}", zone / 60, zone % 60);
        Ok(out)
    }
}

/// `b` in double quotes, escaped as `strconv.Quote` escapes it: an invalid byte as `\xNN`.
pub fn quote(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len() + 2);
    out.push('"');
    let mut at = 0;
    while at < b.len() {
        let (r, w) = decode(tail(b, at));
        if r == RUNE_ERROR && w == 1 {
            let _ = write!(out, "\\x{:02x}", b.get(at).copied().unwrap_or_default());
            at += 1;
            continue;
        }
        at += w;
        escape(&mut out, r);
    }
    out.push('"');
    out
}

/// Go's `appendEscapedRune` for a double-quoted string.
fn escape(out: &mut String, r: u32) {
    if r == u32::from('"') || r == u32::from('\\') {
        out.push('\\');
        out.push(char::from_u32(r).unwrap_or('\u{FFFD}'));
        return;
    }
    if is_print(r) {
        out.push(char::from_u32(r).unwrap_or('\u{FFFD}'));
        return;
    }
    match r {
        0x07 => out.push_str("\\a"),
        0x08 => out.push_str("\\b"),
        0x0c => out.push_str("\\f"),
        0x0a => out.push_str("\\n"),
        0x0d => out.push_str("\\r"),
        0x09 => out.push_str("\\t"),
        0x0b => out.push_str("\\v"),
        _ if r < 0x20 || r == 0x7f => {
            let _ = write!(out, "\\x{r:02x}");
        }
        _ if r < 0x10000 => {
            let _ = write!(out, "\\u{r:04x}");
        }
        _ => {
            let _ = write!(out, "\\U{r:08x}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Decoding as Go decodes: invalid and incomplete sequences one byte at a time.
    #[test]
    fn runes_decode_as_go_decodes_them() {
        assert_eq!(decode(b"a"), (0x61, 1));
        assert_eq!(decode("é".as_bytes()), (0xE9, 2));
        assert_eq!(decode(b"\xff"), (RUNE_ERROR, 1));
        assert_eq!(decode(b"\xc3"), (RUNE_ERROR, 1), "incomplete");
        assert_eq!(decode(b"\xed\xa0\x80"), (RUNE_ERROR, 1), "a surrogate");
        assert_eq!(decode(b"\xc0\x80"), (RUNE_ERROR, 1), "overlong");
        assert_eq!(decode("\u{FFFD}".as_bytes()), (RUNE_ERROR, 3));
        assert_eq!(decode(b""), (RUNE_ERROR, 0));
        assert_eq!(decode_last("aé".as_bytes()), (0xE9, 2));
        assert_eq!(decode_last(b"a\xff"), (RUNE_ERROR, 1));
        assert_eq!(decode_last(b"\xc3\xa9\xa9"), (RUNE_ERROR, 1));
        assert_eq!(decode_last("\u{FFFD}".as_bytes()), (RUNE_ERROR, 3));
    }

    /// Quoting as `strconv.Quote` quotes.
    #[test]
    fn quoting_is_strconvs() {
        assert_eq!(quote(b"a\"b\\c\n\x01\x7f\xff"), r#""a\"b\\c\n\x01\x7f\xff""#);
        assert_eq!(quote("é\u{a0}\u{2028}😀".as_bytes()), "\"é\\u00a0\\u2028😀\"");
        assert_eq!(trim_space(" \u{85}a b\u{a0}\t".as_bytes()), b"a b");
    }

    /// Paths clean and join as Go's `path` package does them.
    #[test]
    fn paths_are_gos() {
        for (p, want) in [
            ("", "."),
            ("/", "/"),
            ("a/b/../c", "a/c"),
            ("/../a", "/a"),
            ("../../a/..", "../.."),
            ("a/../..", ".."),
            ("//a//b/./", "/a/b"),
            ("./", "."),
            ("a/b/c/../../..", "."),
            ("/a/..", "/"),
            ("../a/../b", "../b"),
        ] {
            assert_eq!(clean(p.as_bytes()), want.as_bytes(), "{p}");
        }
        assert_eq!(join(&[b"/", b"a", b"", b"../b/"]), b"/b");
        assert_eq!(join(&[b"", b""]), b"");
        assert!(equal_fold_ascii("\u{212A}ey".as_bytes(), b"KEY"));
        assert!(equal_fold_ascii("\u{17F}".as_bytes(), b"s"));
        assert!(!equal_fold_ascii(b"ke", b"key"));
        assert!(!equal_fold_ascii("\u{130}".as_bytes(), b"i"));
    }
    /// Times parse and print as Go 1.26's `Time.UnmarshalJSON` and `MarshalJSON` do; the
    /// answers are Go's. tests/oracle.rs holds the rest, as image configs' times.
    #[test]
    fn times_parse_and_print_as_gos() {
        enum Want {
            Ok(&'static [u8]),
            Err(&'static [u8]),
            MErr(&'static [u8]),
        }
        use Want::{Err, MErr, Ok};
        let cases: &[(&[u8], Want)] = &[
            (
                b"2024-02-30T00:00:00Z",
                Err(b"parsing time \"2024-02-30T00:00:00Z\": day out of range"),
            ),
            (
                b"2024-01-02T03:04:05+01:61",
                Err(b"parsing time \"2024-01-02T03:04:05+01:61\": time zone offset minute out of range"),
            ),
            (b"2024-01-02T03:04:05Z", Ok(b"2024-01-02T03:04:05Z")),
            (b"2024-01-02T03:04:05.120000000Z", Ok(b"2024-01-02T03:04:05.12Z")),
            (b"2024-01-02T3:04:05Z", Ok(b"2024-01-02T03:04:05Z")),
            (b"2024-01-02T03:04:05,5+01:30", Ok(b"2024-01-02T03:04:05.5+01:30")),
            (b"2024-01-02T03:04:05-00:00", Ok(b"2024-01-02T03:04:05Z")),
            (
                b"2024-01-02T03:04:05+24:00",
                MErr(b"Time.MarshalJSON: timezone hour outside of range [0,23]"),
            ),
        ];
        for (input, want) in cases {
            let got = parse_rfc3339(input);
            let shown = String::from_utf8_lossy(input);
            match want {
                Ok(text) => assert_eq!(got.unwrap().rfc3339_nano().unwrap().as_bytes(), *text, "{shown}"),
                MErr(text) => assert_eq!(got.unwrap().rfc3339_nano().unwrap_err(), *text, "{shown}"),
                Err(text) => assert_eq!(
                    String::from_utf8_lossy(&got.unwrap_err()),
                    String::from_utf8_lossy(text),
                    "{shown}"
                ),
            }
        }
    }
}

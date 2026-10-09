//! Go 1.26's time.Parse and Time.Format (time/format.go) for the layouts ASN.1 times are
//! read with: encoding/asn1's and cryptobyte's UTCTime (`0601021504Z0700`,
//! `060102150405Z0700`) and GeneralizedTime (`20060102150405Z0700`, and
//! `20060102150405.999999999Z0700` with a fraction), each element read as `parse` reads
//! it and each failure in Go's words.

use crate::time::{Time, days_from_civil, days_in};

/// The layout elements these layouts use (nextStdChunk's codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Std {
    Year,
    LongYear,
    ZeroMonth,
    ZeroDay,
    Hour,
    ZeroMinute,
    ZeroSecond,
    FracSecond9,
    Iso8601Tz,
}

/// A layout as its elements, each with the layout text it is.
fn chunks(layout: &str) -> Option<Vec<(Std, &'static str)>> {
    Some(match layout {
        "0601021504Z0700" => vec![
            (Std::Year, "06"),
            (Std::ZeroMonth, "01"),
            (Std::ZeroDay, "02"),
            (Std::Hour, "15"),
            (Std::ZeroMinute, "04"),
            (Std::Iso8601Tz, "Z0700"),
        ],
        "060102150405Z0700" => vec![
            (Std::Year, "06"),
            (Std::ZeroMonth, "01"),
            (Std::ZeroDay, "02"),
            (Std::Hour, "15"),
            (Std::ZeroMinute, "04"),
            (Std::ZeroSecond, "05"),
            (Std::Iso8601Tz, "Z0700"),
        ],
        "20060102150405Z0700" => vec![
            (Std::LongYear, "2006"),
            (Std::ZeroMonth, "01"),
            (Std::ZeroDay, "02"),
            (Std::Hour, "15"),
            (Std::ZeroMinute, "04"),
            (Std::ZeroSecond, "05"),
            (Std::Iso8601Tz, "Z0700"),
        ],
        "20060102150405.999999999Z0700" => vec![
            (Std::LongYear, "2006"),
            (Std::ZeroMonth, "01"),
            (Std::ZeroDay, "02"),
            (Std::Hour, "15"),
            (Std::ZeroMinute, "04"),
            (Std::ZeroSecond, "05"),
            (Std::FracSecond9, ".999999999"),
            (Std::Iso8601Tz, "Z0700"),
        ],
        _ => return None,
    })
}

/// time's quote: printable ASCII as is, `"` and `\` escaped, every other byte of a rune
/// as `\xNN`.
pub fn quote(s: &[u8]) -> String {
    let mut out = String::from("\"");
    let mut i = 0;
    while let Some(&b) = s.get(i) {
        if !(b' '..0x80).contains(&b) {
            let width = if b >= 0x80 {
                utf8_width(s.get(i..).unwrap_or_default())
            } else {
                1
            };
            for j in 0..width {
                if let Some(x) = s.get(i + j) {
                    out.push_str(&format!("\\x{x:02x}"));
                }
            }
            i += width;
        } else {
            if b == b'"' || b == b'\\' {
                out.push('\\');
            }
            out.push(char::from(b));
            i += 1;
        }
    }
    out.push('"');
    out
}

/// The octets of the rune at the start of `s`, 1 where it is not valid UTF-8 (Go's
/// range over a string yields RuneError, one octet wide). time's quote widens a
/// RuneError to 3 where the octets are U+FFFD itself.
fn utf8_width(s: &[u8]) -> usize {
    for w in 2..=4 {
        if let Some(head) = s.get(..w)
            && std::str::from_utf8(head).is_ok()
        {
            return w;
        }
    }
    1
}

/// A parse failure, as ParseError prints.
fn parse_error(layout: &str, value: &[u8], layout_elem: &str, value_elem: &[u8], message: &str) -> String {
    if message.is_empty() {
        format!(
            "parsing time {} as {}: cannot parse {} as {}",
            quote(value),
            quote(layout.as_bytes()),
            quote(value_elem),
            quote(layout_elem.as_bytes())
        )
    } else {
        format!("parsing time {}{message}", quote(value))
    }
}

fn digit(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(u8::is_ascii_digit)
}

/// getnum.
fn getnum(s: &[u8], fixed: bool) -> Option<(i64, &[u8])> {
    let a = *s.first().filter(|c| c.is_ascii_digit())?;
    if !digit(s, 1) {
        if fixed {
            return None;
        }
        return Some((i64::from(a - b'0'), s.get(1..).unwrap_or_default()));
    }
    let b = *s.get(1)?;
    Some((
        i64::from(a - b'0') * 10 + i64::from(b - b'0'),
        s.get(2..).unwrap_or_default(),
    ))
}

/// atoi: an optional sign, then digits to the end.
fn atoi(s: &[u8]) -> Option<i64> {
    let (neg, digits) = match s.first() {
        Some(b'-') => (true, s.get(1..).unwrap_or_default()),
        Some(b'+') => (false, s.get(1..).unwrap_or_default()),
        _ => (false, s),
    };
    let mut x: u64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        if x > (1u64 << 63) / 10 {
            return None;
        }
        x = x * 10 + u64::from(c - b'0');
        if x > 1u64 << 63 {
            return None;
        }
    }
    let x = i64::try_from(x).unwrap_or(i64::MIN);
    Some(if neg { x.wrapping_neg() } else { x })
}

/// parseNanoseconds of `value`'s first `n` octets (a separator, then digits).
fn nanoseconds(value: &[u8], n: usize) -> Result<i64, Option<&'static str>> {
    if !matches!(value.first(), Some(b'.' | b',')) {
        return Err(None);
    }
    let n = n.min(10);
    let ns = atoi(value.get(1..n).unwrap_or_default()).ok_or(None)?;
    if ns < 0 {
        return Err(Some("fractional second"));
    }
    Ok(ns * 10i64.pow(u32::try_from(10 - n).unwrap_or(0)))
}

/// time.Parse(layout, value), the zone fixed by the value (none of these layouts names
/// one).
pub fn parse(layout: &str, value: &[u8]) -> Result<Time, String> {
    let Some(elems) = chunks(layout) else {
        return Err(format!("unsupported layout {layout}"));
    };
    let (mut year, mut month, mut day, mut hour, mut min, mut sec, mut nsec) =
        (0i64, -1i64, -1i64, 0i64, 0i64, 0i64, 0i64);
    let mut offset: i64 = 0;
    let mut v = value;
    for (k, &(std, stdstr)) in elems.iter().enumerate() {
        let hold = v;
        let mut range: Option<&str> = None;
        let mut bad = false;
        match std {
            Std::Year => match v.get(..2) {
                Some(p) => {
                    v = v.get(2..).unwrap_or_default();
                    match atoi(p) {
                        Some(y) => year = if y >= 69 { y + 1900 } else { y + 2000 },
                        None => bad = true,
                    }
                }
                None => bad = true,
            },
            Std::LongYear => match v.get(..4) {
                Some(p) if digit(v, 0) => {
                    v = v.get(4..).unwrap_or_default();
                    match atoi(p) {
                        Some(y) => year = y,
                        None => bad = true,
                    }
                }
                _ => bad = true,
            },
            Std::ZeroMonth => match getnum(v, true) {
                Some((m, rest)) => {
                    month = m;
                    v = rest;
                    if !(1..=12).contains(&m) {
                        range = Some("month");
                    }
                }
                None => bad = true,
            },
            Std::ZeroDay => match getnum(v, true) {
                Some((d, rest)) => {
                    day = d;
                    v = rest;
                }
                None => bad = true,
            },
            Std::Hour => match getnum(v, false) {
                Some((h, rest)) => {
                    hour = h;
                    v = rest;
                    if !(0..24).contains(&h) {
                        range = Some("hour");
                    }
                }
                None => bad = true,
            },
            Std::ZeroMinute => match getnum(v, true) {
                Some((m, rest)) => {
                    min = m;
                    v = rest;
                    if !(0..60).contains(&m) {
                        range = Some("minute");
                    }
                }
                None => bad = true,
            },
            Std::ZeroSecond => match getnum(v, true) {
                Some((s, rest)) => {
                    sec = s;
                    v = rest;
                    if !(0..60).contains(&s) {
                        range = Some("second");
                    } else if v.len() >= 2 && matches!(v.first(), Some(b'.' | b',')) && digit(v, 1) {
                        let next_frac = elems.get(k + 1).is_some_and(|(s, _)| *s == Std::FracSecond9);
                        if !next_frac {
                            let mut n = 2;
                            while digit(v, n) {
                                n += 1;
                            }
                            match nanoseconds(v, n) {
                                Ok(ns) => nsec = ns,
                                Err(Some(r)) => range = Some(r),
                                Err(None) => bad = true,
                            }
                            v = v.get(n..).unwrap_or_default();
                        }
                    }
                }
                None => bad = true,
            },
            Std::FracSecond9 => {
                if v.len() >= 2 && matches!(v.first(), Some(b'.' | b',')) && digit(v, 1) {
                    let mut i = 0;
                    while digit(v, i + 1) {
                        i += 1;
                    }
                    match nanoseconds(v, 1 + i) {
                        Ok(ns) => nsec = ns,
                        Err(Some(r)) => range = Some(r),
                        Err(None) => bad = true,
                    }
                    v = v.get(1 + i..).unwrap_or_default();
                }
            }
            Std::Iso8601Tz => {
                if v.first() == Some(&b'Z') {
                    v = v.get(1..).unwrap_or_default();
                    offset = 0;
                } else if v.len() < 5 {
                    bad = true;
                } else {
                    let sign = v.first().copied();
                    let hh = v.get(1..3).unwrap_or_default();
                    let mm = v.get(3..5).unwrap_or_default();
                    v = v.get(5..).unwrap_or_default();
                    let hr = getnum(hh, true).map(|(x, _)| x);
                    let mi = hr.and_then(|_| getnum(mm, true).map(|(x, _)| x));
                    let (hr, mi) = (hr.unwrap_or(0), mi.unwrap_or(0));
                    if getnum(hh, true).is_none() || getnum(mm, true).is_none() {
                        bad = true;
                    }
                    if hr > 24 {
                        range = Some("time zone offset hour");
                    }
                    if mi > 60 {
                        range = Some("time zone offset minute");
                    }
                    offset = (hr * 60 + mi) * 60;
                    match sign {
                        Some(b'+') => {}
                        Some(b'-') => offset = -offset,
                        _ => bad = true,
                    }
                }
            }
        }
        if let Some(r) = range {
            return Err(parse_error(
                layout,
                value,
                stdstr,
                v,
                &format!(": {r} out of range"),
            ));
        }
        if bad {
            return Err(parse_error(layout, value, stdstr, hold, ""));
        }
    }
    if !v.is_empty() {
        return Err(parse_error(
            layout,
            value,
            "",
            v,
            &format!(": extra text: {}", quote(v)),
        ));
    }
    if month < 0 {
        month = 1;
    }
    if day < 0 {
        day = 1;
    }
    let m = u32::try_from(month).unwrap_or(1);
    if day < 1 || day > i64::from(days_in(m, year)) {
        return Err(parse_error(layout, value, "", v, ": day out of range"));
    }
    let d = u32::try_from(day).unwrap_or(1);
    let secs = days_from_civil(year, m, d) * 86_400 + hour * 3600 + min * 60 + sec - offset;
    Ok(Time {
        secs,
        nanos: u32::try_from(nsec).unwrap_or(0),
        offset: i32::try_from(offset).unwrap_or(0),
    })
}

/// t.Format(layout) for these layouts.
pub fn format(layout: &str, t: &Time) -> String {
    let local = t.secs + i64::from(t.offset);
    let days = local.div_euclid(86_400);
    let rem = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let mut out = String::new();
    for (std, _) in chunks(layout).unwrap_or_default() {
        match std {
            Std::Year => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            Std::LongYear => out.push_str(&format!("{y:04}")),
            Std::ZeroMonth => out.push_str(&format!("{m:02}")),
            Std::ZeroDay => out.push_str(&format!("{d:02}")),
            Std::Hour => out.push_str(&format!("{:02}", rem / 3600)),
            Std::ZeroMinute => out.push_str(&format!("{:02}", rem / 60 % 60)),
            Std::ZeroSecond => out.push_str(&format!("{:02}", rem % 60)),
            Std::FracSecond9 => {
                if t.nanos != 0 {
                    let f = format!("{:09}", t.nanos);
                    out.push('.');
                    out.push_str(f.trim_end_matches('0'));
                }
            }
            Std::Iso8601Tz => {
                if t.offset == 0 {
                    out.push('Z');
                } else {
                    let o = t.offset.unsigned_abs() / 60;
                    out.push(if t.offset < 0 { '-' } else { '+' });
                    out.push_str(&format!("{:02}{:02}", o / 60, o % 60));
                }
            }
        }
    }
    out
}

/// A day count from 1970 as a civil date (Hinnant's civil_from_days).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

/// Parse, then the round trip Go makes: the time must print back as given.
pub fn parse_exact(layout: &str, value: &[u8]) -> Result<Time, String> {
    let t = parse(layout, value)?;
    let back = format(layout, &t);
    if back.as_bytes() != value {
        return Err(format!(
            "asn1: time did not serialize back to the original value and may be invalid: given {}, but serialized as {}",
            shards_dockerfile::go::quote(value),
            shards_dockerfile::go::quote(back.as_bytes())
        ));
    }
    Ok(t)
}

/// t.AddDate(years, 0, 0), as Date normalizes it (Feb 29 of a common year becoming
/// Mar 1), in t's zone.
pub fn add_years(t: &Time, years: i64) -> Time {
    let local = t.secs + i64::from(t.offset);
    let days = local.div_euclid(86_400);
    let rem = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let ny = y + years;
    let (m2, d2) = if m == 2 && d == 29 && days_in(2, ny) == 28 {
        (3, 1)
    } else {
        (m, d)
    };
    Time {
        secs: days_from_civil(ny, m2, d2) * 86_400 + rem - i64::from(t.offset),
        nanos: t.nanos,
        offset: t.offset,
    }
}

/// The year of t in its zone.
pub fn year(t: &Time) -> i64 {
    civil_from_days((t.secs + i64::from(t.offset)).div_euclid(86_400)).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse_and_fail_as_go_s_do() {
        let t = parse_exact("20060102150405.999999999Z0700", b"20260325134132.5Z").unwrap();
        assert_eq!((t.secs, t.nanos), (1_774_446_092, 500_000_000));
        assert!(parse_exact("20060102150405.999999999Z0700", b"20260325134132.50Z").is_err());
        assert_eq!(
            parse("0601021504Z0700", b"2603251341Q").unwrap_err(),
            "parsing time \"2603251341Q\" as \"0601021504Z0700\": cannot parse \"Q\" as \"Z0700\""
        );
        assert_eq!(
            parse("060102150405Z0700", b"261325134132Z").unwrap_err(),
            "parsing time \"261325134132Z\": month out of range"
        );
        assert_eq!(
            parse("060102150405Z0700", b"260230134132Z").unwrap_err(),
            "parsing time \"260230134132Z\": day out of range"
        );
        assert_eq!(
            parse("060102150405Z0700", b"260325134132Zx").unwrap_err(),
            "parsing time \"260325134132Zx\": extra text: \"x\""
        );
    }
}

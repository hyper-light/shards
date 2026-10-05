//! Go's `time` formats, as the Docker client reads `logs --since` and `--until` (moby
//! client/internal/timestamp/timestamp.go, GetTimestamp, with Go 1.26.1's
//! src/time/format.go, ParseDuration and parse), and as dockerd reads what the client
//! sends it (moby daemon/internal/timestamp/timestamp.go, ParseUnixTimestamp).

use crate::go;

const NS: i128 = 1_000_000_000;

/// `time.ParseDuration`: nanoseconds, or `None` where Go returns an error, which
/// GetTimestamp never shows.
pub fn parse_duration(s: &str) -> Option<i64> {
    duration(s).ok()
}

/// `time.ParseDuration`: nanoseconds, or Go's error, as pflag shows it for a duration
/// flag.
pub fn duration(orig: &str) -> Result<i64, String> {
    let invalid = || format!("time: invalid duration {}", quote(orig));
    let mut s = orig.as_bytes();
    let mut negative = false;
    if let Some((&c @ (b'-' | b'+'), rest)) = s.split_first() {
        negative = c == b'-';
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
        let first = *s.first().ok_or_else(invalid)?;
        if !(first == b'.' || first.is_ascii_digit()) {
            return Err(invalid());
        }
        let before = s.len();
        let (v, rest) = leading_int(s).ok_or_else(invalid)?;
        s = rest;
        let pre = before != s.len();
        let (mut f, mut scale, mut post) = (0u64, 1f64, false);
        if let Some((b'.', rest)) = s.split_first() {
            s = rest;
            let before = s.len();
            (f, scale, s) = leading_fraction(s);
            post = before != s.len();
        }
        if !pre && !post {
            return Err(invalid());
        }
        let unit_len = s
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if unit_len == 0 {
            return Err(format!("time: missing unit in duration {}", quote(orig)));
        }
        let (unit, rest) = s.split_at(unit_len);
        s = rest;
        let unit: u64 = match unit {
            b"ns" => 1,
            // U+00B5 and U+03BC: the micro sign and the Greek mu.
            b"us" | b"\xc2\xb5s" | b"\xce\xbcs" => 1_000,
            b"ms" => 1_000_000,
            b"s" => 1_000_000_000,
            b"m" => 60_000_000_000,
            b"h" => 3_600_000_000_000,
            _ => {
                return Err(format!(
                    "time: unknown unit {} in duration {}",
                    quote(&String::from_utf8_lossy(unit)),
                    quote(orig)
                ));
            }
        };
        if v > LIMIT / unit {
            return Err(invalid());
        }
        let mut v = v * unit;
        if f > 0 {
            // As Go computes it, in float64.
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let fraction = (f as f64 * (unit as f64 / scale)) as u64;
            v = v.checked_add(fraction).ok_or_else(invalid)?;
            if v > LIMIT {
                return Err(invalid());
            }
        }
        d = d.checked_add(v).ok_or_else(invalid)?;
        if d > LIMIT {
            return Err(invalid());
        }
    }
    if negative {
        return Ok(0i64.wrapping_sub_unsigned(d));
    }
    i64::try_from(d).map_err(|_| invalid())
}

/// The `time` package's own `quote`: in double quotes, `"` and `\` escaped, and every
/// byte of a character below a space or past ASCII as `\xNN`.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c < ' ' || !c.is_ascii() {
            let mut bytes = [0u8; 4];
            for b in c.encode_utf8(&mut bytes).bytes() {
                out.push_str(&format!("\\x{b:02x}"));
            }
        } else {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out.push('"');
    out
}

/// `time.Duration.String`: `72h3m0.5s`, and below a second `1.2ms`, `3µs`, `5ns`; `0s`.
pub fn format_duration(d: i64) -> String {
    // The digits of `v` with `prec` of them after a point, less trailing zeros, the point
    // too if none are left (fmtFrac, fmtInt).
    fn frac(v: u64, prec: u32) -> (u64, String) {
        let p = 10u64.pow(prec);
        let digits = format!("{:0width$}", v % p, width = prec as usize);
        let digits = digits.trim_end_matches('0');
        (
            v / p,
            if digits.is_empty() {
                String::new()
            } else {
                format!(".{digits}")
            },
        )
    }
    if d == 0 {
        return "0s".into();
    }
    let u = d.unsigned_abs();
    let text = if u < 1_000_000_000 {
        let (prec, unit) = match u {
            0..1_000 => (0, "ns"),
            1_000..1_000_000 => (3, "µs"),
            _ => (6, "ms"),
        };
        let (int, fraction) = frac(u, prec);
        format!("{int}{fraction}{unit}")
    } else {
        let (secs, fraction) = frac(u, 9);
        let (s, m) = (secs % 60, secs / 60);
        match (m / 60, m % 60) {
            (0, 0) => format!("{s}{fraction}s"),
            (0, m) => format!("{m}m{s}{fraction}s"),
            (h, m) => format!("{h}h{m}m{s}{fraction}s"),
        }
    };
    if d < 0 { format!("-{text}") } else { text }
}

/// Go's `leadingInt`: the digits at the start of `s`, or `None` past 1<<63.
fn leading_int(s: &[u8]) -> Option<(u64, &[u8])> {
    let digits = s.iter().take_while(|c| c.is_ascii_digit()).count();
    let mut x: u64 = 0;
    for &c in s.get(..digits)? {
        if x > (1 << 63) / 10 {
            return None;
        }
        x = x * 10 + u64::from(c - b'0');
        if x > 1 << 63 {
            return None;
        }
    }
    Some((x, s.get(digits..)?))
}

/// Go's `leadingFraction`: the digits at the start of `s` as far as they fit, the power
/// of ten they scale by, and the rest.
fn leading_fraction(s: &[u8]) -> (u64, f64, &[u8]) {
    let digits = s.iter().take_while(|c| c.is_ascii_digit()).count();
    let (mut x, mut scale, mut overflow) = (0u64, 1f64, false);
    for &c in s.get(..digits).unwrap_or_default() {
        if overflow {
            continue;
        }
        if x > ((1 << 63) - 1) / 10 {
            overflow = true;
            continue;
        }
        let y = x * 10 + u64::from(c - b'0');
        if y > 1 << 63 {
            overflow = true;
            continue;
        }
        x = y;
        scale *= 10.0;
    }
    (x, scale, s.get(digits..).unwrap_or_default())
}

/// A layout element Go's `nextStdChunk` finds in the layouts GetTimestamp uses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Std {
    LongYear,
    ZeroMonth,
    ZeroDay,
    Hour,
    ZeroMinute,
    ZeroSecond,
    FracSecond9,
    ColonTz,
}

/// A layout: its text, and its elements, each after the literal text before it.
struct Layout {
    text: &'static str,
    chunks: &'static [(&'static str, Std, &'static str)],
}

macro_rules! layout {
    ($text:literal, $($prefix:literal $std:ident $stdstr:literal),+) => {
        Layout { text: $text, chunks: &[$(($prefix, Std::$std, $stdstr)),+] }
    };
}

const DATE_LOCAL: Layout = layout!("2006-01-02", "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02");
const DATE_ZONE: Layout = layout!(
    "2006-01-02Z07:00",
    "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02", "" ColonTz "Z07:00"
);
const HOUR_LOCAL: Layout = layout!(
    "2006-01-02T15",
    "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02", "T" Hour "15"
);
const HOUR_ZONE: Layout = layout!(
    "2006-01-02T15Z07:00",
    "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02", "T" Hour "15", "" ColonTz "Z07:00"
);
const MINUTE_LOCAL: Layout = layout!(
    "2006-01-02T15:04",
    "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02", "T" Hour "15", ":" ZeroMinute "04"
);
const MINUTE_ZONE: Layout = layout!(
    "2006-01-02T15:04Z07:00",
    "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02", "T" Hour "15", ":" ZeroMinute "04",
    "" ColonTz "Z07:00"
);
const RFC3339_LOCAL: Layout = layout!(
    "2006-01-02T15:04:05",
    "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02", "T" Hour "15", ":" ZeroMinute "04",
    ":" ZeroSecond "05"
);
const RFC3339: Layout = layout!(
    "2006-01-02T15:04:05Z07:00",
    "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02", "T" Hour "15", ":" ZeroMinute "04",
    ":" ZeroSecond "05", "" ColonTz "Z07:00"
);
const RFC3339_NANO_LOCAL: Layout = layout!(
    "2006-01-02T15:04:05.999999999",
    "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02", "T" Hour "15", ":" ZeroMinute "04",
    ":" ZeroSecond "05", "" FracSecond9 ".999999999"
);
const RFC3339_NANO: Layout = layout!(
    "2006-01-02T15:04:05.999999999Z07:00",
    "" LongYear "2006", "-" ZeroMonth "01", "-" ZeroDay "02", "T" Hour "15", ":" ZeroMinute "04",
    ":" ZeroSecond "05", "" FracSecond9 ".999999999", "" ColonTz "Z07:00"
);

/// Go's `time.quote`: double quotes, `"` and `\` escaped, and every byte of anything
/// but printable ASCII as `\xNN`.
fn time_quote(s: &[u8]) -> String {
    let mut out = String::from('"');
    // Walk the bytes as Go ranges over a string: invalid bytes are one each.
    let mut i = 0;
    while let Some(rest) = s.get(i..).filter(|r| !r.is_empty()) {
        let width = match std::str::from_utf8(rest.get(..rest.len().min(4)).unwrap_or_default()) {
            Ok(t) => t.chars().next().map_or(1, char::len_utf8),
            Err(e) if e.valid_up_to() > 0 => {
                std::str::from_utf8(rest.get(..e.valid_up_to()).unwrap_or_default())
                    .ok()
                    .and_then(|t| t.chars().next())
                    .map_or(1, char::len_utf8)
            }
            Err(_) => 1,
        };
        let bytes = rest.get(..width).unwrap_or_default();
        match bytes {
            [c] if (b' '..0x80).contains(c) => {
                if *c == b'"' || *c == b'\\' {
                    out.push('\\');
                }
                out.push(char::from(*c));
            }
            _ => {
                for b in bytes {
                    out.push_str(&format!("\\x{b:02x}"));
                }
            }
        }
        i += width;
    }
    out.push('"');
    out
}

/// Go's `ParseError` text.
fn parse_error(layout: &Layout, value: &[u8], layout_elem: &str, value_elem: &[u8], message: &str) -> String {
    if message.is_empty() {
        format!(
            "parsing time {} as {}: cannot parse {} as {}",
            time_quote(value),
            time_quote(layout.text.as_bytes()),
            time_quote(value_elem),
            time_quote(layout_elem.as_bytes())
        )
    } else {
        format!("parsing time {}{message}", time_quote(value))
    }
}

/// Go's `getnum`: one or two digits (two if `fixed`), and the rest.
fn getnum(s: &[u8], fixed: bool) -> Option<(i64, &[u8])> {
    match s {
        [a, b, rest @ ..] if a.is_ascii_digit() && b.is_ascii_digit() => {
            Some((i64::from(a - b'0') * 10 + i64::from(b - b'0'), rest))
        }
        [a, rest @ ..] if a.is_ascii_digit() && !fixed => Some((i64::from(a - b'0'), rest)),
        _ => None,
    }
}

/// Go's `parseNanoseconds`: the fraction in `value[1..n]` as nanoseconds, at most nine
/// digits counting.
fn parse_nanoseconds(value: &[u8], n: usize) -> Option<i64> {
    if !matches!(value.first(), Some(b'.' | b',')) {
        return None;
    }
    let n = n.min(10);
    let digits = value.get(1..n)?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut ns: i64 = 0;
    for &c in digits {
        ns = ns * 10 + i64::from(c - b'0');
    }
    for _ in 0..10 - n {
        ns *= 10;
    }
    Some(ns)
}

/// Days from 1970-01-01 to `y`-`m`-`d` in the proleptic Gregorian calendar (Howard
/// Hinnant, "chrono-Compatible Low-Level Date Algorithms", days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in(month: i64, year: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Go's `time.Parse`/`ParseInLocation` of `value` by `layout`, as nanoseconds since the
/// epoch; a value without a zone is in the zone `offset` seconds east of UTC.
fn parse(layout: &Layout, avalue: &[u8], offset: i64) -> Result<i128, String> {
    let (mut year, mut month, mut day) = (0i64, -1i64, -1i64);
    let (mut hour, mut min, mut sec, mut nsec) = (0i64, 0i64, 0i64, 0i64);
    let mut zone: Option<i64> = None;
    let mut value = avalue;
    for (i, &(prefix, std, stdstr)) in layout.chunks.iter().enumerate() {
        // Go's skip: the literal text, byte for byte.
        for &p in prefix.as_bytes() {
            match value.split_first() {
                Some((&c, rest)) if c == p => value = rest,
                _ => return Err(parse_error(layout, avalue, prefix, value, "")),
            }
        }
        let hold = value;
        let mut range: Option<&str> = None;
        let mut bad = false;
        match std {
            Std::LongYear => match value.get(..4) {
                Some(p) if p.first().is_some_and(u8::is_ascii_digit) && p.iter().all(u8::is_ascii_digit) => {
                    year = p.iter().fold(0, |y, &c| y * 10 + i64::from(c - b'0'));
                    value = value.get(4..).unwrap_or_default();
                }
                _ => bad = true,
            },
            Std::ZeroMonth => match getnum(value, true) {
                Some((m, rest)) => {
                    (month, value) = (m, rest);
                    if !(1..=12).contains(&m) {
                        range = Some("month");
                    }
                }
                None => bad = true,
            },
            Std::ZeroDay => match getnum(value, true) {
                Some((d, rest)) => (day, value) = (d, rest),
                None => bad = true,
            },
            Std::Hour => match getnum(value, false) {
                Some((h, rest)) => {
                    (hour, value) = (h, rest);
                    if h >= 24 {
                        range = Some("hour");
                    }
                }
                None => bad = true,
            },
            Std::ZeroMinute => match getnum(value, true) {
                Some((m, rest)) => {
                    (min, value) = (m, rest);
                    if m >= 60 {
                        range = Some("minute");
                    }
                }
                None => bad = true,
            },
            Std::ZeroSecond => match getnum(value, true) {
                Some((s, rest)) => {
                    (sec, value) = (s, rest);
                    if s >= 60 {
                        range = Some("second");
                    } else if matches!(value.first(), Some(b'.' | b','))
                        && value.get(1).is_some_and(u8::is_ascii_digit)
                        && layout.chunks.get(i + 1).map(|c| c.1) != Some(Std::FracSecond9)
                    {
                        // A fraction the layout has no place for is taken all the same.
                        let n = 2 + value.iter().skip(2).take_while(|c| c.is_ascii_digit()).count();
                        match parse_nanoseconds(value, n) {
                            Some(ns) => nsec = ns,
                            None => bad = true,
                        }
                        value = value.get(n..).unwrap_or_default();
                    }
                }
                None => bad = true,
            },
            Std::FracSecond9 => {
                if matches!(value.first(), Some(b'.' | b',')) && value.get(1).is_some_and(u8::is_ascii_digit)
                {
                    let n = 1 + value.iter().skip(1).take_while(|c| c.is_ascii_digit()).count();
                    match parse_nanoseconds(value, n) {
                        Some(ns) => nsec = ns,
                        None => bad = true,
                    }
                    value = value.get(n..).unwrap_or_default();
                }
            }
            Std::ColonTz => {
                if let Some((b'Z', rest)) = value.split_first() {
                    value = rest;
                    zone = Some(0);
                } else if value.len() < 6 || value.get(3) != Some(&b':') {
                    bad = true;
                } else {
                    let (sign, hh, mm) = (value.first(), value.get(1..3), value.get(4..6));
                    value = value.get(6..).unwrap_or_default();
                    let hr = hh.and_then(|h| getnum(h, true)).map(|(n, _)| n);
                    let mn = hr.and(mm.and_then(|m| getnum(m, true)).map(|(n, _)| n));
                    let (h, m) = (hr.unwrap_or(0), mn.unwrap_or(0));
                    bad = hr.is_none() || mn.is_none();
                    if h > 24 {
                        range = Some("time zone offset hour");
                    }
                    if m > 60 {
                        range = Some("time zone offset minute");
                    }
                    let east = (h * 60 + m) * 60;
                    match sign {
                        Some(b'+') => zone = Some(east),
                        Some(b'-') => zone = Some(-east),
                        _ => bad = true,
                    }
                }
            }
        }
        if let Some(what) = range {
            return Err(parse_error(
                layout,
                avalue,
                stdstr,
                value,
                &format!(": {what} out of range"),
            ));
        }
        if bad {
            return Err(parse_error(layout, avalue, stdstr, hold, ""));
        }
    }
    if !value.is_empty() {
        return Err(parse_error(
            layout,
            avalue,
            "",
            value,
            &format!(": extra text: {}", time_quote(value)),
        ));
    }
    if day < 1 || day > days_in(month, year) {
        return Err(parse_error(layout, avalue, "", value, ": day out of range"));
    }
    let east = zone.unwrap_or(offset);
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + min * 60 + sec - east;
    Ok(i128::from(seconds) * NS + i128::from(nsec))
}

/// Go's `strconv.ParseInt(s, 10, 64)`.
fn parse_int10(s: &str) -> Result<i64, go::NumError> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(go::NumError::syntax("ParseInt", s));
    }
    s.strip_prefix('+')
        .unwrap_or(s)
        .parse::<i64>()
        .map_err(|_| go::NumError::range("ParseInt", s))
}

/// What the Docker client sends dockerd for `logs --since` or `--until` `value`, at
/// `now` (nanoseconds since the epoch) in a zone `offset` seconds east of UTC
/// (GetTimestamp): a duration back from now as whole seconds, a time as
/// `SECONDS.NANOSECONDS`, and a Unix timestamp as given. The error is the client's.
pub fn get_timestamp(value: &str, now: i128, offset: i64) -> Result<String, String> {
    if value != "0"
        && let Some(d) = parse_duration(value)
    {
        // Go negates the duration as an int64, wrapping.
        return Ok((now + i128::from(d.wrapping_neg())).div_euclid(NS).to_string());
    }
    let (layout, in_location) = layout_for(value);
    match parse(layout, value.as_bytes(), if in_location { offset } else { 0 }) {
        Ok(ns) => Ok(format!("{}.{:09}", ns.div_euclid(NS), ns.rem_euclid(NS))),
        // With a `-`, it was probably meant for a time.
        Err(e) if value.contains('-') => Err(e),
        Err(_) => {
            let (s, n) = match value.split_once('.') {
                Some((s, n)) => (s, Some(n)),
                None => (value, None),
            };
            if parse_int10(s).is_err() || n.is_some_and(|n| parse_int10(n).is_err()) {
                return Err(format!(
                    "failed to parse value as time or duration: {}",
                    go::quote(value)
                ));
            }
            Ok(value.to_string())
        }
    }
}

/// The layout GetTimestamp and dockerd's Parse read `value` by, and whether in the
/// local zone: none of `zZ+` and not three `-`s.
fn layout_for(value: &str) -> (&'static Layout, bool) {
    let in_location = !value.contains(['z', 'Z', '+']) && value.matches('-').count() != 3;
    let layout = if value.contains('.') {
        if in_location {
            &RFC3339_NANO_LOCAL
        } else {
            &RFC3339_NANO
        }
    } else if value.contains('T') {
        let mut colons = value.matches(':').count();
        // A `+` or `-` offset brings a colon of its own.
        if !in_location && !value.contains(['z', 'Z']) && colons > 0 {
            colons -= 1;
        }
        match (in_location, colons) {
            (true, 0) => &HOUR_LOCAL,
            (true, 1) => &MINUTE_LOCAL,
            (true, _) => &RFC3339_LOCAL,
            (false, 0) => &HOUR_ZONE,
            (false, 1) => &MINUTE_ZONE,
            (false, _) => &RFC3339,
        }
    } else if in_location {
        &DATE_LOCAL
    } else {
        &DATE_ZONE
    };
    (layout, in_location)
}

/// What dockerd makes of a filter's time `value` at `now` (nanoseconds since the epoch)
/// in a zone `offset` seconds east of UTC (moby daemon/internal/timestamp, Parse): a
/// duration back from now, a time, or a Unix timestamp; nanoseconds since the epoch.
pub fn parse_timestamp(value: &str, now: i128, offset: i64) -> Result<i128, String> {
    if value.trim().is_empty() {
        return Err("failed to parse value as time or duration: value is empty".into());
    }
    if value != "0"
        && let Some(d) = parse_duration(value)
    {
        return Ok(now + i128::from(d.wrapping_neg()));
    }
    let (layout, in_location) = layout_for(value);
    match parse(layout, value.as_bytes(), if in_location { offset } else { 0 }) {
        Ok(ns) => Ok(ns),
        Err(e) if value.contains('-') => Err(e),
        Err(_) => unix_ns(value).map_err(|e| format!("failed to parse value as time or duration: {e}")),
    }
}

/// A Unix timestamp as dockerd reads `since` and `until` (ParseUnixTimestamp): seconds,
/// and perhaps a fraction of up to 20 digits, of which 9 count. `None` for an empty one.
pub fn parse_unix_timestamp(value: &str) -> Result<Option<i128>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    unix_ns(value)
        .map(Some)
        .map_err(|why| format!("invalid timestamp {}: {why}", go::quote(value)))
}

/// parseTimestamp: seconds, and perhaps a fraction of up to 20 digits, of which 9 count;
/// in nanoseconds.
fn unix_ns(value: &str) -> Result<i128, String> {
    let (s, n) = match value.split_once('.') {
        Some((s, n)) => (s, Some(n)),
        None => (value, None),
    };
    let seconds = parse_int10(s).map_err(|e| format!("invalid seconds {}: {}", go::quote(s), e.reason()))?;
    let seconds = i128::from(seconds) * NS;
    let Some(n) = n.filter(|n| !matches!(*n, "" | "000000000" | "0")) else {
        return Ok(seconds);
    };
    if n.len() > 20 {
        return Err(format!(
            "invalid nanoseconds: length {} exceeds maximum 20",
            n.len()
        ));
    }
    if let Some((at, c)) = n.bytes().enumerate().find(|(_, c)| !c.is_ascii_digit()) {
        return Err(format!(
            "invalid nanoseconds: invalid character {} at position {at}",
            go::quote_rune(char::from(c))
        ));
    }
    let nine: i128 = n
        .bytes()
        .chain(std::iter::repeat(b'0'))
        .take(9)
        .fold(0, |ns, c| ns * 10 + i128::from(c - b'0'));
    Ok(seconds + nine)
}

//! Go's archive/tar field encodings (go1.26.1 src/archive/tar/strconv.go) and the
//! strconv.ParseInt they lean on.

use super::{ERR_HEADER, Time};
use crate::error::Error;

/// isASCII: every byte below 0x80 and none NUL. Go walks runes, but a rune of several
/// bytes, or a byte that starts none, is all at or past 0x80, so bytes say the same.
pub(crate) fn is_ascii(s: &[u8]) -> bool {
    s.iter().all(|&c| c < 0x80 && c != 0)
}

/// toASCII: what is not ASCII, or is NUL, dropped.
pub(crate) fn to_ascii(s: &[u8]) -> Vec<u8> {
    s.iter().copied().filter(|&c| c < 0x80 && c != 0).collect()
}

/// parseString: up to the first NUL.
pub(crate) fn parse_string(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(i) => b.get(..i).unwrap_or_default(),
        None => b,
    }
}

/// formatString: a short value NUL-terminated, a long one cut, and a cut one's trailing
/// slash replaced by NUL. Reports whether it fit.
pub(crate) fn format_string(b: &mut [u8], s: &[u8]) -> bool {
    let fits = s.len() <= b.len();
    for (dst, &src) in b.iter_mut().zip(s) {
        *dst = src;
    }
    if let Some(end) = b.get_mut(s.len()) {
        *end = 0;
    }
    if !fits && b.last() == Some(&b'/') {
        let cut = s.get(..b.len().saturating_sub(1)).unwrap_or_default();
        let keep = cut.iter().rposition(|&c| c != b'/').map_or(0, |i| i + 1);
        if let Some(end) = b.get_mut(keep) {
            *end = 0;
        }
    }
    fits
}

/// fitsInBase256.
pub(crate) fn fits_base256(n: usize, x: i64) -> bool {
    if n >= 9 {
        return true;
    }
    let bits = (n as u32).saturating_sub(1) * 8;
    x >= -(1i64 << bits) && x < (1i64 << bits)
}

/// fitsInOctal.
pub(crate) fn fits_octal(n: usize, x: i64) -> bool {
    if x < 0 {
        return false;
    }
    if n >= 22 {
        return true;
    }
    let bits = (n as u32).saturating_sub(1) * 3;
    x < (1i64 << bits)
}

/// formatOctal: zero-padded, leaving room for a NUL; 0 when it does not fit. Reports
/// whether it fit.
pub(crate) fn format_octal(b: &mut [u8], x: i64) -> bool {
    let fits = fits_octal(b.len(), x);
    let x = if fits { x } else { 0 };
    let mut s = format!("{x:o}").into_bytes();
    let n = b.len().saturating_sub(s.len() + 1);
    if n > 0 {
        let mut padded = vec![b'0'; n];
        padded.append(&mut s);
        s = padded;
    }
    format_string(b, &s);
    fits
}

/// parseNumeric: base-256 when the top bit is set, else octal. None is Go's ErrHeader.
pub(crate) fn parse_numeric(b: &[u8]) -> Option<i64> {
    match b.first() {
        Some(&first) if first & 0x80 != 0 => {
            let inv: u8 = if first & 0x40 != 0 { 0xff } else { 0 };
            let mut x: u64 = 0;
            for (i, &c) in b.iter().enumerate() {
                let mut c = c ^ inv;
                if i == 0 {
                    c &= 0x7f;
                }
                if x >> 56 > 0 {
                    return None;
                }
                x = (x << 8) | u64::from(c);
            }
            if x >> 63 > 0 {
                return None;
            }
            let x = x as i64;
            Some(if inv == 0xff { !x } else { x })
        }
        _ => parse_octal(b),
    }
}

/// parseOctal: spaces and NULs trimmed, then octal digits.
pub(crate) fn parse_octal(b: &[u8]) -> Option<i64> {
    let start = b.iter().position(|&c| c != b' ' && c != 0);
    let end = b.iter().rposition(|&c| c != b' ' && c != 0);
    let (Some(start), Some(end)) = (start, end) else {
        return Some(0);
    };
    let digits = parse_string(b.get(start..=end).unwrap_or_default());
    // strconv.ParseUint(s, 8, 64): an empty string, a sign or a non-octal digit fails;
    // so does a value past 64 bits. Go keeps the clamped value; the caller only sees
    // ErrHeader.
    if digits.is_empty() {
        return None;
    }
    let mut x: u64 = 0;
    for &c in digits {
        if !(b'0'..=b'7').contains(&c) {
            return None;
        }
        x = x.checked_mul(8)?.checked_add(u64::from(c - b'0'))?;
    }
    Some(x as i64)
}

/// strconv.ParseInt(s, 10, 64).
pub(crate) fn parse_int(s: &[u8]) -> Option<i64> {
    let (neg, digits) = match s.first() {
        Some(b'+') => (false, s.get(1..).unwrap_or_default()),
        Some(b'-') => (true, s.get(1..).unwrap_or_default()),
        _ => (false, s),
    };
    if digits.is_empty() {
        return None;
    }
    let mut x: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        let d = i64::from(c - b'0');
        x = x.checked_mul(10)?;
        x = if neg { x.checked_sub(d)? } else { x.checked_add(d)? };
    }
    Some(x)
}

/// parsePAXTime: seconds, then up to nine digits of fraction (more are checked and
/// dropped).
pub(crate) fn parse_pax_time(s: &[u8]) -> Result<Time, Error> {
    let (ss, sn) = match s.iter().position(|&c| c == b'.') {
        Some(i) => (s.get(..i).unwrap_or_default(), s.get(i + 1..).unwrap_or_default()),
        None => (s, &[][..]),
    };
    let secs = parse_int(ss).ok_or_else(|| Error::new(crate::Kind::Header, ERR_HEADER))?;
    if sn.is_empty() {
        return Ok(Time::unix(secs, 0));
    }
    let mut nanos: i64 = 0;
    for (i, &c) in sn.iter().enumerate() {
        if !c.is_ascii_digit() {
            return Err(Error::new(crate::Kind::Header, ERR_HEADER));
        }
        if i < 9 {
            nanos = nanos * 10 + i64::from(c - b'0');
        }
    }
    for _ in sn.len()..9 {
        nanos *= 10;
    }
    if ss.first() == Some(&b'-') {
        return Ok(Time::unix(secs, -nanos));
    }
    Ok(Time::unix(secs, nanos))
}

/// formatPAXTime: seconds, with the fraction's trailing zeros trimmed.
pub(crate) fn format_pax_time(t: Time) -> Vec<u8> {
    let (mut secs, mut nsecs) = (t.sec, i64::from(t.nsec));
    if nsecs == 0 {
        return secs.to_string().into_bytes();
    }
    let mut sign = "";
    if secs < 0 {
        sign = "-";
        secs = -(secs + 1);
        nsecs = -(nsecs - 1_000_000_000);
    }
    let s = format!("{sign}{secs}.{nsecs:09}");
    s.trim_end_matches('0').as_bytes().to_vec()
}

/// parsePAXRecord: `size key=value\n`. Returns the key, the value and what follows, or
/// None for ErrHeader.
pub(crate) fn parse_pax_record(s: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let sp = s.iter().position(|&c| c == b' ')?;
    let n_str = s.get(..sp)?;
    let rest = s.get(sp + 1..)?;
    let n = parse_int(n_str)?;
    if n < 5 || n > s.len() as i64 {
        return None;
    }
    let n = n - (n_str.len() as i64 + 1);
    if n <= 0 {
        return None;
    }
    let n = usize::try_from(n).ok()?;
    let rec = rest.get(..n - 1)?;
    if rest.get(n - 1..n)? != b"\n" {
        return None;
    }
    let rem = rest.get(n..)?;
    let eq = rec.iter().position(|&c| c == b'=')?;
    let (k, v) = (rec.get(..eq)?, rec.get(eq + 1..)?);
    if !valid_pax_record(k, v) {
        return None;
    }
    Some((k, v, rem))
}

/// formatPAXRecord: the size counts itself.
pub(crate) fn format_pax_record(k: &[u8], v: &[u8]) -> Option<Vec<u8>> {
    if !valid_pax_record(k, v) {
        return None;
    }
    let mut size = k.len() + v.len() + 3;
    size += size.to_string().len();
    let record = |size: usize| [size.to_string().as_bytes(), b" ", k, b"=", v, b"\n"].concat();
    let mut r = record(size);
    if r.len() != size {
        r = record(r.len());
    }
    Some(r)
}

/// validPAXRecord.
pub(crate) fn valid_pax_record(k: &[u8], v: &[u8]) -> bool {
    if k.is_empty() || k.contains(&b'=') {
        return false;
    }
    match k {
        b"path" | b"linkpath" | b"uname" | b"gname" => !v.contains(&0),
        _ => !k.contains(&0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cases from go1.26.1 src/archive/tar/strconv_test.go (TestParseNumeric,
    // TestFormatNumeric, TestParsePAXTime, TestFormatPAXTime, TestParsePAXRecord).
    #[test]
    fn numbers_as_go() {
        let cases: &[(&[u8], Option<i64>)] = &[
            (b"", Some(0)),
            (b"\x80", Some(0)),
            (b"\x80\x00", Some(0)),
            (b"\xbf", Some((1 << 6) - 1)),
            (b"\xbf\xff\xff", Some((1 << 22) - 1)),
            (b"\xff", Some(-1)),
            (b"\xff\xff\xff", Some(-1)),
            (b"\xc0", Some(-(1 << 6))),
            (b"\xc0\x00\x00", Some(-(1 << 22))),
            (b"\x87\x76\xa2\x22\xeb\x8a\x72\x61", Some(537795476381659745)),
            (
                b"\x80\x00\x00\x00\x07\x76\xa2\x22\xeb\x8a\x72\x61",
                Some(537795476381659745),
            ),
            (b"\xf7\x76\xa2\x22\xeb\x8a\x72\x61", Some(-615126028225187231)),
            (
                b"\xff\xff\xff\xff\xf7\x76\xa2\x22\xeb\x8a\x72\x61",
                Some(-615126028225187231),
            ),
            (b"\x80\x7f\xff\xff\xff\xff\xff\xff\xff", Some(i64::MAX)),
            (b"\x80\x80\x00\x00\x00\x00\x00\x00\x00", None),
            (b"\xff\x80\x00\x00\x00\x00\x00\x00\x00", Some(i64::MIN)),
            (b"\xff\x7f\xff\xff\xff\xff\xff\xff\xff", None),
            (b"\xf5\xec\xd1\xc7\x7e\x5f\x26\x48\x81\x9f\x8f\x9b", None),
            (b"0000000\x00", Some(0)),
            (b" \x0000000\x00", Some(0)),
            (b" \x0000003\x00", Some(3)),
            (b"00000000227\x00", Some(0o227)),
            (b"032033\x00 ", Some(0o32033)),
            (b"320330\x00 ", Some(0o320330)),
            (b"0000660\x00 ", Some(0o660)),
            (b"\x00 0000660\x00 ", Some(0o660)),
            (b"0123456789abcdef", None),
            (b"0123456789\x00abcdef", None),
            (b"01234567\x0089abcdef", Some(342391)),
            (b"0123\x7e\x5f\x264123", None),
            (b"1000000000000000000000", Some(i64::MIN)),
        ];
        for (input, want) in cases {
            assert_eq!(parse_numeric(input), *want, "{input:?}");
        }
        let mut b = [0u8; 8];
        assert!(format_octal(&mut b, 0o755));
        assert_eq!(&b, b"0000755\x00");
        assert!(!format_octal(&mut b, 0o10000000));
        assert_eq!(&b, b"0000000\x00");
    }

    #[test]
    fn pax_times_as_go() {
        let t = |s: &str| parse_pax_time(s.as_bytes()).ok().map(|t| (t.sec, t.nsec));
        assert_eq!(t("1350244992.023960108"), Some((1350244992, 23960108)));
        assert_eq!(t("1350244992.02396010"), Some((1350244992, 23960100)));
        assert_eq!(t("1350244992.3"), Some((1350244992, 300000000)));
        assert_eq!(t("1350244992"), Some((1350244992, 0)));
        assert_eq!(t("-1.000000001"), Some((-2, 999999999)));
        assert_eq!(t("-1.001000"), Some((-2, 999000000)));
        assert_eq!(t("-1"), Some((-1, 0)));
        assert_eq!(t("-1.999999999"), Some((-2, 1)));
        assert_eq!(t("-1.9999999999"), Some((-2, 1)));
        assert_eq!(t("0.000000001"), Some((0, 1)));
        assert_eq!(t("1.0000000009"), Some((1, 0)));
        assert_eq!(t("1350244992.0239601089"), Some((1350244992, 23960108)));
        assert_eq!(t("-1350244992.0239601089"), Some((-1350244993, 976039892)));
        assert_eq!(t(""), None);
        assert_eq!(t("0"), Some((0, 0)));
        assert_eq!(t("1."), Some((1, 0)));
        assert_eq!(t("0.0"), Some((0, 0)));
        assert_eq!(t(".5"), None);
        assert_eq!(t("-1.3"), Some((-2, 700000000)));
        assert_eq!(t("1.2.3"), None);
        assert_eq!(t("1e3"), None);
        let f = |s, n| String::from_utf8(format_pax_time(Time::unix(s, n))).unwrap();
        assert_eq!(f(1350244992, 23960108), "1350244992.023960108");
        assert_eq!(f(1350244992, 0), "1350244992");
        assert_eq!(f(-1, -1), "-1.000000001");
        assert_eq!(f(-1, -999999999), "-1.999999999");
        assert_eq!(f(-1, -300000000), "-1.3");
        assert_eq!(f(0, 1), "0.000000001");
    }

    #[test]
    fn pax_records_as_go() {
        type Rec<'a> = Option<(&'a [u8], &'a [u8], &'a [u8])>;
        let cases: &[(&[u8], Rec)] = &[
            (b"6 k=v\n\n", Some((b"k", b"v", b"\n"))),
            (b"19 path=/etc/hosts\n", Some((b"path", b"/etc/hosts", b""))),
            (b"9 foo=ba\n", Some((b"foo", b"ba", b""))),
            (b"11 foo=bar\n\x00", Some((b"foo", b"bar", b"\x00"))),
            (b"18 foo=b=\nar=\n==\x00\n", Some((b"foo", b"b=\nar=\n==\x00", b""))),
            (b"27 foo=hello9 foo=ba\nworld\n", Some((b"foo", b"hello9 foo=ba\nworld", b""))),
            ("27 ☺☻☹=日a本b語ç\nmeow mix".as_bytes(), Some(("☺☻☹".as_bytes(), "日a本b語ç".as_bytes(), b"meow mix"))),
            (b"17 \x00hello=\x00world\n", None),
            (b"1 k=1\n", None),
            (b"6 k~1\n", None),
            (b"6_k=1\n", None),
            (b"6 k=1 ", None),
            (b"632 k=1\n", None),
            (b"16 longkeyname=hahaha\n", None),
            (b"3 somelongkey=\n", None),
            (b"50 tooshort=\n", None),
            (b"0000000000000000000000000000000030 mtime=1432668921.098285006\n30 ctime=2147483649.15163319", None),
            (b"06 k=v\n", None),
            (b"00006 k=v\n", None),
            (b"000006 k=v\n", None),
            (b"000000 k=v\n", None),
            (b"0 k=v\n", None),
            (b"+0000005 x=\n", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_pax_record(input), *want, "{input:?}");
        }
        assert_eq!(format_pax_record(b"k", b"v").unwrap(), b"6 k=v\n");
        assert_eq!(
            format_pax_record(b"path", b"/etc/hosts").unwrap(),
            b"19 path=/etc/hosts\n"
        );
        let v = vec![b'a'; 91];
        assert_eq!(format_pax_record(b"path", &v).unwrap().len(), 101);
    }
}

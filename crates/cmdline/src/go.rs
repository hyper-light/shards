//! Go's `strconv` formats, in which the Docker CLI reads flag values and prints them back
//! (Go 1.26.1: src/strconv/quote.go, src/internal/strconv/atoi.go and atob.go).

use std::fmt::{self, Write as _};

use crate::tables::{LINUX_ERRNO, NOT_PRINT};
use crate::width::within;

/// Whether Go's `strconv.IsPrint` holds for `c`: a letter, mark, number, punctuation or
/// symbol, or the ASCII space.
pub fn is_print(c: char) -> bool {
    let r = u32::from(c);
    if r < 0x80 {
        return (0x20..0x7f).contains(&r);
    }
    !within(NOT_PRINT, r)
}

/// `s` in double quotes, escaped as Go's `strconv.Quote` escapes it.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        escape(&mut out, c, '"');
    }
    out.push('"');
    out
}

/// `c` in single quotes, escaped as Go's `strconv.QuoteRune` escapes it.
pub fn quote_rune(c: char) -> String {
    let mut out = String::from('\'');
    escape(&mut out, c, '\'');
    out.push('\'');
    out
}

/// Go's `appendEscapedRune`, for UTF-8 text: the quote and the backslash escaped, the
/// printable as it is, the rest as `\a`…`\v`, `\xNN`, `\uNNNN` or `\UNNNNNNNN`.
fn escape(out: &mut String, c: char, quote: char) {
    if c == quote || c == '\\' {
        out.push('\\');
        out.push(c);
        return;
    }
    if is_print(c) {
        out.push(c);
        return;
    }
    let r = u32::from(c);
    match c {
        '\u{7}' => out.push_str("\\a"),
        '\u{8}' => out.push_str("\\b"),
        '\u{c}' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{b}' => out.push_str("\\v"),
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

/// Linux's error `errno` in Go's words (`syscall.Errno.Error`), in which runc, and so
/// dockerd, report what kept a command from starting; `errno N` for one Go has no words
/// for on this architecture.
pub fn linux_error(errno: i32) -> String {
    LINUX_ERRNO
        .iter()
        .find(|&&(n, _, everywhere)| n == errno && (everywhere || cfg!(target_arch = "aarch64")))
        .map_or_else(|| format!("errno {errno}"), |&(_, text, _)| text.to_string())
}

/// Text Go's `strconv` could not read as a number or a boolean, worded as its `NumError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumError {
    func: &'static str,
    text: String,
    range: bool,
}

impl NumError {
    /// Text `func` could not read at all.
    pub fn syntax(func: &'static str, text: &str) -> NumError {
        NumError {
            func,
            text: text.to_string(),
            range: false,
        }
    }

    /// A number `func` read that does not fit.
    pub fn range(func: &'static str, text: &str) -> NumError {
        NumError {
            func,
            text: text.to_string(),
            range: true,
        }
    }

    /// Why, as Go's `ErrSyntax` and `ErrRange` say it.
    pub fn reason(&self) -> &'static str {
        if self.range {
            "value out of range"
        } else {
            "invalid syntax"
        }
    }
}

impl fmt::Display for NumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "strconv.{}: parsing {}: {}",
            self.func,
            quote(&self.text),
            self.reason()
        )
    }
}

/// `s` read as `strconv.ParseInt(s, 0, 64)` reads it, as pflag reads an `int` flag: an
/// optional sign, then decimal digits, or `0b`, `0o` or `0x` and the base's digits, or `0`
/// and octal digits, with `_` allowed between digits.
pub fn parse_int(s: &str) -> Result<i64, NumError> {
    let fail = |range| NumError {
        func: "ParseInt",
        text: s.to_string(),
        range,
    };
    let (negative, digits) = match s.as_bytes().first() {
        None => return Err(fail(false)),
        Some(b'+') => (false, s.get(1..).unwrap_or_default()),
        Some(b'-') => (true, s.get(1..).unwrap_or_default()),
        Some(_) => (false, s),
    };
    // An unsigned overflow still reads as out of range once the sign is back.
    let magnitude = match parse_uint(digits) {
        Ok(n) => n,
        Err(true) => u64::MAX,
        Err(false) => return Err(fail(false)),
    };
    const CUTOFF: u64 = 1 << 63;
    if !negative && magnitude >= CUTOFF || negative && magnitude > CUTOFF {
        return Err(fail(true));
    }
    Ok(if negative {
        0i64.wrapping_sub_unsigned(magnitude)
    } else {
        i64::try_from(magnitude).map_err(|_| fail(true))?
    })
}

/// Go's `ParseUint(s, 0, 64)`: the number, or whether it failed by overflow (`true`)
/// rather than syntax.
fn parse_uint(s: &str) -> Result<u64, bool> {
    let bytes = s.as_bytes();
    let Some(&first) = bytes.first() else {
        return Err(false);
    };
    let lower = |c: u8| c | (b'x' - b'X');
    let (base, digits) = match (first, bytes.get(1).map(|&c| lower(c))) {
        (b'0', Some(b'b')) if bytes.len() >= 3 => (2u64, bytes.get(2..)),
        (b'0', Some(b'o')) if bytes.len() >= 3 => (8, bytes.get(2..)),
        (b'0', Some(b'x')) if bytes.len() >= 3 => (16, bytes.get(2..)),
        (b'0', _) => (8, bytes.get(1..)),
        _ => (10, Some(bytes)),
    };
    let cutoff = u64::MAX / base + 1;
    let mut underscores = false;
    let mut n: u64 = 0;
    for &c in digits.unwrap_or_default() {
        let d = match c {
            b'_' => {
                underscores = true;
                continue;
            }
            b'0'..=b'9' => c - b'0',
            _ if lower(c).is_ascii_lowercase() => lower(c) - b'a' + 10,
            _ => return Err(false),
        };
        if u64::from(d) >= base {
            return Err(false);
        }
        if n >= cutoff {
            return Err(true);
        }
        n = (n * base).checked_add(u64::from(d)).ok_or(true)?;
    }
    if underscores && !underscores_ok(s) {
        return Err(false);
    }
    Ok(n)
}

/// Go's `underscoreOK`: each `_` stands between digits, or between a base prefix and a
/// digit.
fn underscores_ok(s: &str) -> bool {
    let lower = |c: u8| c | (b'x' - b'X');
    let mut bytes = s.as_bytes();
    if let Some((b'-' | b'+', rest)) = bytes.split_first() {
        bytes = rest;
    }
    // What came last: '^' the start, '0' a digit or base prefix, '_', or '!' else.
    let mut saw = '^';
    let mut hex = false;
    if let [b'0', prefix, rest @ ..] = bytes
        && matches!(lower(*prefix), b'b' | b'o' | b'x')
    {
        saw = '0';
        hex = lower(*prefix) == b'x';
        bytes = rest;
    }
    for &c in bytes {
        if c.is_ascii_digit() || hex && (b'a'..=b'f').contains(&lower(c)) {
            saw = '0';
        } else if c == b'_' {
            if saw != '0' {
                return false;
            }
            saw = '_';
        } else {
            if saw == '_' {
                return false;
            }
            saw = '!';
        }
    }
    saw != '_'
}

/// `s` read as `strconv.ParseBool` reads it, as pflag reads a `bool` flag's value.
pub fn parse_bool(s: &str) -> Result<bool, NumError> {
    match s {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Ok(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Ok(false),
        _ => Err(NumError {
            func: "ParseBool",
            text: s.to_string(),
            range: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_escape_what_go_escapes() {
        assert_eq!(
            quote("a\"b\\c\n\t\u{7}\u{1b}\u{7f}"),
            "\"a\\\"b\\\\c\\n\\t\\a\\x1b\\x7f\""
        );
        // No-break space, right-to-left override, a private-use rune, one past the BMP.
        assert_eq!(
            quote("\u{a0}\u{202e}\u{e000}\u{e0001}"),
            "\"\\u00a0\\u202e\\ue000\\U000e0001\""
        );
        assert_eq!(quote("日本語 é \u{301}"), "\"日本語 é \u{301}\"");
        assert_eq!(quote_rune('\''), "'\\''");
        assert_eq!(quote_rune('"'), "'\"'");
        assert_eq!(quote_rune('Ã'), "'Ã'");
    }

    #[test]
    fn integers_read_as_go_reads_them() {
        for (s, n) in [
            ("0", 0),
            ("-1", -1),
            ("+7", 7),
            ("010", 8),
            ("0x1F", 31),
            ("0X_1f", 31),
            ("0b101", 5),
            ("0o17", 15),
            ("1_000", 1000),
            ("9223372036854775807", i64::MAX),
            ("-9223372036854775808", i64::MIN),
        ] {
            assert_eq!(parse_int(s), Ok(n), "{s}");
        }
        for s in [
            "", "+", "-", "x", "08", "0x", "1__0", "_1", "1_", "--1", "+-1", "1.0", " 1",
        ] {
            let e = parse_int(s).unwrap_err();
            assert_eq!(
                e.to_string(),
                format!("strconv.ParseInt: parsing {}: invalid syntax", quote(s))
            );
        }
        for s in [
            "9223372036854775808",
            "-9223372036854775809",
            "99999999999999999999x",
        ] {
            let e = parse_int(s).unwrap_err();
            assert_eq!(
                e.to_string(),
                format!("strconv.ParseInt: parsing {}: value out of range", quote(s))
            );
        }
    }

    #[test]
    fn linux_errors_read_as_go_words_them() {
        assert_eq!(linux_error(2), "no such file or directory");
        assert_eq!(linux_error(13), "permission denied");
        assert_eq!(linux_error(21), "is a directory");
        assert_eq!(linux_error(40), "too many levels of symbolic links");
        assert_eq!(linux_error(0), "errno 0");
        assert_eq!(linux_error(9999), "errno 9999");
    }

    #[test]
    fn booleans_read_as_go_reads_them() {
        assert_eq!(parse_bool("True"), Ok(true));
        assert_eq!(parse_bool("0"), Ok(false));
        assert_eq!(
            parse_bool("yes").unwrap_err().to_string(),
            "strconv.ParseBool: parsing \"yes\": invalid syntax"
        );
    }
}

//! OPA's strings builtins (topdown/strings.go and template.go): Go's strings package
//! as they call it, fmt's Sprintf for `sprintf` (print.go, format.go, with strconv's
//! float formatting and math/big's `Int.Format`), and Go's text/template, through
//! shards-template, for `strings.render_template`.

use std::fmt::Write as _;

use num_bigint::{BigInt, Sign};

use super::Builtin;
use super::{BuiltinError, Context, arg, int_operand, number_operand, string_operand};
use crate::goquote;
use crate::value::Value;

pub fn lookup(name: &str) -> Option<Builtin> {
    Some(match name {
        "concat" => concat,
        "contains" => contains,
        "endswith" => endswith,
        "format_int" => format_int,
        "indexof" => indexof,
        "indexof_n" => indexof_n,
        "lower" => lower,
        "replace" => replace,
        "split" => split,
        "sprintf" => sprintf,
        "startswith" => startswith,
        "strings.any_prefix_match" => any_prefix_match,
        "strings.any_suffix_match" => any_suffix_match,
        "strings.count" => count,
        "strings.replace_n" => replace_n,
        "strings.reverse" => reverse,
        "substring" => substring,
        "trim" => trim,
        "trim_left" => trim_left,
        "trim_prefix" => trim_prefix,
        "trim_right" => trim_right,
        "trim_space" => trim_space,
        "trim_suffix" => trim_suffix,
        "upper" => upper,
        "strings.render_template" => render_template,
        _ => return None,
    })
}

type Out = Result<Option<Value>, BuiltinError>;

fn ok_str(s: impl Into<std::rc::Rc<str>>) -> Out {
    Ok(Some(Value::string(s)))
}

fn ok_int(i: usize) -> Out {
    Ok(Some(Value::int(i64::try_from(i).unwrap_or(i64::MAX))))
}

/// The first two operands, as strings.
fn two_strings(args: &[Value]) -> Result<(&str, &str), BuiltinError> {
    let a = string_operand(arg(args, 0)?, 1)?;
    let b = string_operand(arg(args, 1)?, 2)?;
    Ok((a, b))
}

/// builtins.NewOperandElementErr with one expected type.
fn element_err(pos: usize, composite: &Value, got: &Value, expected: &str) -> BuiltinError {
    let tpe = composite.type_name();
    BuiltinError::operand(
        pos,
        format!(
            "must be {tpe} of {expected}s but got {tpe} containing {}",
            got.type_name()
        ),
    )
}

/// builtins.StringSliceOperand.
fn string_slice_operand(v: &Value, pos: usize) -> Result<Vec<&str>, BuiltinError> {
    let items: Box<dyn Iterator<Item = &Value>> = match v {
        Value::Array(a) => Box::new(a.iter()),
        Value::Set(s) => Box::new(s.iter()),
        _ => return Err(BuiltinError::operand_type(pos, v, &["array", "set"])),
    };
    items
        .map(|x| x.as_str().ok_or_else(|| element_err(pos, v, x, "string")))
        .collect()
}

/// The strings of a string, set or array operand (any_prefix_match's switch).
fn strings_of(v: &Value, pos: usize) -> Result<Vec<&str>, BuiltinError> {
    match v {
        Value::String(s) => Ok(vec![s]),
        Value::Array(_) | Value::Set(_) => string_slice_operand(v, pos),
        _ => Err(BuiltinError::operand_type(pos, v, &["string", "set", "array"])),
    }
}

/// anyStartsWithAny: whether any string starts with any prefix (go-patricia's
/// MatchSubtree over a trie of the strings).
fn any_starts_with_any(strs: &[String], prefixes: &[String]) -> bool {
    strs.iter()
        .any(|s| prefixes.iter().any(|p| s.as_bytes().starts_with(p.as_bytes())))
}

fn any_prefix_match(_: &mut Context, args: &[Value]) -> Out {
    let strs = strings_of(arg(args, 0)?, 1)?;
    let prefixes = strings_of(arg(args, 1)?, 2)?;
    let strs: Vec<String> = strs.into_iter().map(str::to_owned).collect();
    let prefixes: Vec<String> = prefixes.into_iter().map(str::to_owned).collect();
    Ok(Some(Value::Bool(any_starts_with_any(&strs, &prefixes))))
}

fn any_suffix_match(_: &mut Context, args: &[Value]) -> Out {
    let strs = strings_of(arg(args, 0)?, 1)?;
    let suffixes = strings_of(arg(args, 1)?, 2)?;
    let strs: Vec<String> = strs.into_iter().map(reverse_string).collect();
    let suffixes: Vec<String> = suffixes.into_iter().map(reverse_string).collect();
    Ok(Some(Value::Bool(any_starts_with_any(&strs, &suffixes))))
}

fn format_int(_: &mut Context, args: &[Value]) -> Out {
    let input = number_operand(arg(args, 0)?, 1)?;
    let base = number_operand(arg(args, 1)?, 2)?;
    // OPA compares the base's text.
    let radix = match base.text() {
        "2" => 2,
        "8" => 8,
        "10" => {
            if let Some(i) = input.as_i64() {
                return ok_str(i.to_string());
            }
            10
        }
        "16" => 16,
        _ => return Err(BuiltinError::operand(2, "must be one of {2, 8, 10, 16}")),
    };
    let f = input.to_float().map_err(|e| BuiltinError::Other(e.to_string()))?;
    let (i, _) = f.int();
    ok_str(i.to_str_radix(radix))
}

fn concat(_: &mut Context, args: &[Value]) -> Out {
    let join = string_operand(arg(args, 0)?, 1)?;
    let coll = arg(args, 1)?;
    let items: Vec<&Value> = match coll {
        Value::Array(a) => a.iter().collect(),
        Value::Set(s) => s.iter().collect(),
        _ => return Err(BuiltinError::operand_type(2, coll, &["set", "array"])),
    };
    let mut parts = Vec::with_capacity(items.len());
    for x in items {
        parts.push(x.as_str().ok_or_else(|| element_err(2, coll, x, "string"))?);
    }
    ok_str(parts.join(join))
}

/// indexof's and indexof_n's search, by runes: every start where `search` matches.
fn rune_matches(base: &str, search: &str) -> Vec<usize> {
    let base: Vec<char> = base.chars().collect();
    let search: Vec<char> = search.chars().collect();
    let mut out = Vec::new();
    if search.is_empty() {
        return out;
    }
    for i in 0..base.len() {
        match base.get(i..i + search.len()) {
            Some(w) if w == search.as_slice() => out.push(i),
            Some(_) => {}
            None => break,
        }
    }
    out
}

fn indexof(_: &mut Context, args: &[Value]) -> Out {
    let (base, search) = two_strings(args)?;
    if search.is_empty() {
        return Err(BuiltinError::Other("empty search character".into()));
    }
    match rune_matches(base, search).first() {
        Some(&i) => ok_int(i),
        None => Ok(Some(Value::int(-1))),
    }
}

fn indexof_n(_: &mut Context, args: &[Value]) -> Out {
    let (base, search) = two_strings(args)?;
    if search.is_empty() {
        return Err(BuiltinError::Other("empty search character".into()));
    }
    let found = rune_matches(base, search)
        .into_iter()
        .map(|i| Value::int(i64::try_from(i).unwrap_or(i64::MAX)))
        .collect();
    Ok(Some(Value::array(found)))
}

fn substring(_: &mut Context, args: &[Value]) -> Out {
    let base = string_operand(arg(args, 0)?, 1)?;
    let start = int_operand(arg(args, 1)?, 2)?;
    let length = int_operand(arg(args, 2)?, 3)?;
    if start < 0 {
        return Err(BuiltinError::Other("negative offset".into()));
    }
    let runes: Vec<char> = base.chars().collect();
    let len = i64::try_from(runes.len()).unwrap_or(i64::MAX);
    if start >= len {
        return ok_str("");
    }
    let upto = if length < 0 {
        len
    } else {
        len.min(start.saturating_add(length))
    };
    let from = usize::try_from(start).unwrap_or(0);
    let to = usize::try_from(upto).unwrap_or(0);
    let s: String = runes.get(from..to).unwrap_or_default().iter().collect();
    ok_str(s)
}

fn contains(_: &mut Context, args: &[Value]) -> Out {
    let (s, sub) = two_strings(args)?;
    Ok(Some(Value::Bool(s.contains(sub))))
}

/// strings.Count: non-overlapping instances, or the runes plus one for "".
fn count(_: &mut Context, args: &[Value]) -> Out {
    let (s, sub) = two_strings(args)?;
    if sub.is_empty() {
        return ok_int(s.chars().count() + 1);
    }
    ok_int(s.matches(sub).count())
}

fn startswith(_: &mut Context, args: &[Value]) -> Out {
    let (s, p) = two_strings(args)?;
    Ok(Some(Value::Bool(s.starts_with(p))))
}

fn endswith(_: &mut Context, args: &[Value]) -> Out {
    let (s, p) = two_strings(args)?;
    Ok(Some(Value::Bool(s.ends_with(p))))
}

/// unicode.ToLower: the simple case mapping (Rust's full mapping differs only where
/// it is more than one character, U+0130 alone for lower case).
fn go_to_lower(c: char) -> char {
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) => l,
        _ if c == '\u{130}' => 'i',
        _ => c,
    }
}

/// unicode.ToUpper: the simple case mapping. Where Rust's full mapping is more than one
/// character, UnicodeData's simple mapping is the letter itself, but for the Greek
/// letters with ypogegrammeni, which map to their title-case forms.
fn go_to_upper(c: char) -> char {
    let mut it = c.to_uppercase();
    match (it.next(), it.next()) {
        (Some(u), None) => u,
        _ => {
            let u = u32::from(c);
            let mapped = match u {
                0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7 => u + 8,
                0x1FB3 | 0x1FC3 | 0x1FF3 => u + 9,
                _ => u,
            };
            char::from_u32(mapped).unwrap_or(c)
        }
    }
}

fn lower(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok_str(s.chars().map(go_to_lower).collect::<String>())
}

fn upper(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok_str(s.chars().map(go_to_upper).collect::<String>())
}

fn split(_: &mut Context, args: &[Value]) -> Out {
    let (text, delim) = two_strings(args)?;
    let parts: Vec<Value> = if delim.is_empty() {
        // strings.SplitSeq explodes into runes.
        text.chars().map(|c| Value::string(c.to_string())).collect()
    } else {
        text.split(delim).map(Value::string).collect()
    };
    Ok(Some(Value::array(parts)))
}

/// strings.NewReplacer(oldnew...).WriteString, as its generic replacer runs (the
/// specialized ones answer the same): at each byte, the first pair whose old string
/// starts there is replaced, an empty one not twice at one place. An empty old string
/// matches between the bytes of a character too; Go then writes the character's
/// bytes apart, which reads back here as U+FFFD for each, as encoding/json writes them.
fn go_replace(s: &str, pairs: &[(&str, &str)]) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let (mut i, mut last, mut prev_empty) = (0, 0, false);
    while i <= b.len() {
        let rest = b.get(i..).unwrap_or_default();
        let found = pairs
            .iter()
            .find(|(old, _)| !(prev_empty && old.is_empty()) && rest.starts_with(old.as_bytes()));
        prev_empty = matches!(found, Some((old, _)) if old.is_empty());
        if let Some((old, new)) = found {
            out.extend_from_slice(b.get(last..i).unwrap_or_default());
            out.extend_from_slice(new.as_bytes());
            i += old.len();
            last = i;
            continue;
        }
        i += 1;
    }
    if last < b.len() {
        out.extend_from_slice(b.get(last..).unwrap_or_default());
    }
    go_bytes_to_string(&out)
}

/// Bytes as a string, each byte that starts no valid UTF-8 character read as U+FFFD
/// (utf8.DecodeRune's size-1 errors, as encoding/json writes them).
fn go_bytes_to_string(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let mut taken = false;
        for n in 1..=4 {
            if let Some(Ok(c)) = b.get(i..i + n).map(std::str::from_utf8) {
                out.push_str(c);
                i += n;
                taken = true;
                break;
            }
        }
        if !taken {
            out.push('\u{FFFD}');
            i += 1;
        }
    }
    out
}

fn replace(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    let old = string_operand(arg(args, 1)?, 2)?;
    let new = string_operand(arg(args, 2)?, 3)?;
    ok_str(go_replace(s, &[(old, new)]))
}

fn replace_n(_: &mut Context, args: &[Value]) -> Out {
    let patterns = arg(args, 0)?;
    let Value::Object(patterns) = patterns else {
        return Err(BuiltinError::operand_type(1, patterns, &["object"]));
    };
    let s = string_operand(arg(args, 1)?, 2)?;
    let mut pairs = Vec::with_capacity(patterns.len());
    for (k, v) in patterns.iter() {
        let Some(k) = k.as_str() else {
            return Err(BuiltinError::operand(1, "non-string key found in pattern object"));
        };
        let Some(v) = v.as_str() else {
            return Err(BuiltinError::operand(
                1,
                "non-string value found in pattern object",
            ));
        };
        pairs.push((k, v));
    }
    ok_str(go_replace(s, &pairs))
}

fn trim(_: &mut Context, args: &[Value]) -> Out {
    let (s, cut) = two_strings(args)?;
    ok_str(s.trim_matches(|c| cut.contains(c)))
}

fn trim_left(_: &mut Context, args: &[Value]) -> Out {
    let (s, cut) = two_strings(args)?;
    ok_str(s.trim_start_matches(|c| cut.contains(c)))
}

fn trim_right(_: &mut Context, args: &[Value]) -> Out {
    let (s, cut) = two_strings(args)?;
    ok_str(s.trim_end_matches(|c| cut.contains(c)))
}

fn trim_prefix(_: &mut Context, args: &[Value]) -> Out {
    let (s, p) = two_strings(args)?;
    ok_str(s.strip_prefix(p).unwrap_or(s))
}

fn trim_suffix(_: &mut Context, args: &[Value]) -> Out {
    let (s, p) = two_strings(args)?;
    ok_str(s.strip_suffix(p).unwrap_or(s))
}

/// strings.TrimSpace: unicode.IsSpace is the White_Space property, as Rust's is.
fn trim_space(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok_str(s.trim_matches(char::is_whitespace))
}

fn reverse_string(s: &str) -> String {
    s.chars().rev().collect()
}

fn reverse(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok_str(reverse_string(s))
}

/// A value as OPA prints it (`Value.String()`): strings quoted as Go quotes them,
/// objects and sets in their order.
fn opa_text(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(n.text()),
        Value::String(s) => goquote::quote(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                opa_text(out, x);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                opa_text(out, k);
                out.push_str(": ");
                opa_text(out, x);
            }
            out.push('}');
        }
        Value::Set(s) => {
            if s.is_empty() {
                out.push_str("set()");
                return;
            }
            out.push('{');
            for (i, x) in s.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                opa_text(out, x);
            }
            out.push('}');
        }
    }
}

fn sprintf(_: &mut Context, args: &[Value]) -> Out {
    let format = string_operand(arg(args, 0)?, 1)?;
    let arr = arg(args, 1)?;
    let Value::Array(items) = arr else {
        return Err(BuiltinError::operand_type(2, arr, &["array"]));
    };
    let goargs: Vec<GoArg> = items
        .iter()
        .map(|v| match v {
            Value::Number(n) => {
                let text = n.text();
                if let Some(i) = n.as_i64() {
                    GoArg::Int(i)
                } else if let Some(b) = big_int_base10(text) {
                    GoArg::Big(b)
                } else if let Some(f) = parse_f64(text) {
                    GoArg::Float(f)
                } else {
                    GoArg::Str(text.to_string())
                }
            }
            Value::String(s) => GoArg::Str(s.to_string()),
            _ => {
                let mut s = String::new();
                opa_text(&mut s, v);
                GoArg::Str(s)
            }
        })
        .collect();
    let mut p = Printer::default();
    p.do_printf(format, &goargs);
    ok_str(p.buf)
}

/// `new(big.Int).SetString(s, 10)`: an optional sign and decimal digits.
fn big_int_base10(s: &str) -> Option<BigInt> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.strip_prefix('+').unwrap_or(s).parse().ok()
}

/// `json.Number(s).Float64()`: strconv.ParseFloat, out of range an error.
fn parse_f64(s: &str) -> Option<f64> {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if body.is_empty()
        || !body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return None;
    }
    s.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// An operand as builtinSprintf hands it to fmt.
#[derive(Debug)]
enum GoArg {
    Int(i64),
    Big(BigInt),
    Float(f64),
    Str(String),
}

impl GoArg {
    fn type_name(&self) -> &'static str {
        match self {
            GoArg::Int(_) => "int",
            GoArg::Big(_) => "*big.Int",
            GoArg::Float(_) => "float64",
            GoArg::Str(_) => "string",
        }
    }
}

const LDIGITS: &[u8; 17] = b"0123456789abcdefx";
const UDIGITS: &[u8; 17] = b"0123456789ABCDEFX";

#[derive(Default, Clone, Copy)]
struct Flags {
    wid_present: bool,
    prec_present: bool,
    minus: bool,
    plus: bool,
    sharp: bool,
    space: bool,
    zero: bool,
    plus_v: bool,
    sharp_v: bool,
}

/// print.go's pp with format.go's fmt.
#[derive(Default)]
struct Printer {
    buf: String,
    f: Flags,
    wid: i64,
    prec: i64,
    reordered: bool,
    good_arg_num: bool,
    erroring: bool,
}

fn digit(digits: &[u8; 17], i: u64) -> char {
    char::from(
        usize::try_from(i)
            .ok()
            .and_then(|i| digits.get(i))
            .copied()
            .unwrap_or(b'?'),
    )
}

/// print.go's tooLarge.
fn too_large(x: i64) -> bool {
    !(-1_000_000..=1_000_000).contains(&x)
}

fn len_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

impl Printer {
    fn clear_flags(&mut self) {
        self.f = Flags::default();
        self.wid = 0;
        self.prec = 0;
    }

    fn write_padding(&mut self, n: i64) {
        if n <= 0 {
            return;
        }
        let pad = if self.f.zero && !self.f.minus { '0' } else { ' ' };
        for _ in 0..n {
            self.buf.push(pad);
        }
    }

    fn pad(&mut self, s: &str) {
        if !self.f.wid_present || self.wid == 0 {
            self.buf.push_str(s);
            return;
        }
        let width = self.wid - len_i64(s.chars().count());
        if !self.f.minus {
            self.write_padding(width);
            self.buf.push_str(s);
        } else {
            self.buf.push_str(s);
            self.write_padding(width);
        }
    }

    fn fmt_boolean(&mut self, v: bool) {
        self.pad(if v { "true" } else { "false" });
    }

    fn fmt_unicode(&mut self, u: u64) {
        let mut prec: i64 = 4;
        if self.f.prec_present && self.prec > 4 {
            prec = self.prec;
        }
        let mut rev: Vec<char> = Vec::new();
        let mut v = u;
        while v >= 16 {
            rev.push(digit(UDIGITS, v & 0xf));
            prec -= 1;
            v >>= 4;
        }
        rev.push(digit(UDIGITS, v));
        prec -= 1;
        while prec > 0 {
            rev.push('0');
            prec -= 1;
        }
        let mut s = String::from("U+");
        s.extend(rev.into_iter().rev());
        if self.f.sharp
            && let Some(c) = u32::try_from(u).ok().and_then(char::from_u32)
            && goquote::is_print(u32::from(c))
        {
            s.push_str(" '");
            s.push(c);
            s.push('\'');
        }
        let old = self.f.zero;
        self.f.zero = false;
        self.pad(&s);
        self.f.zero = old;
    }

    fn fmt_integer(&mut self, u: u64, base: u64, signed: bool, verb: char, digits: &[u8; 17]) {
        let negative = signed && (u as i64) < 0;
        let mut u = if negative { u.wrapping_neg() } else { u };
        let mut prec: i64 = 0;
        if self.f.prec_present {
            prec = self.prec;
            if prec == 0 && u == 0 {
                let old = self.f.zero;
                self.f.zero = false;
                self.write_padding(self.wid);
                self.f.zero = old;
                return;
            }
        } else if self.f.zero && !self.f.minus && self.f.wid_present {
            prec = self.wid;
            if negative || self.f.plus || self.f.space {
                prec -= 1;
            }
        }
        let mut rev: Vec<char> = Vec::new();
        while u >= base {
            rev.push(digit(digits, u % base));
            u /= base;
        }
        rev.push(digit(digits, u));
        while len_i64(rev.len()) < prec {
            rev.push('0');
        }
        if self.f.sharp {
            match base {
                2 => rev.extend(['b', '0']),
                8 => {
                    if rev.last() != Some(&'0') {
                        rev.push('0');
                    }
                }
                16 => rev.extend([digit(digits, 16), '0']),
                _ => {}
            }
        }
        if verb == 'O' {
            rev.extend(['o', '0']);
        }
        if negative {
            rev.push('-');
        } else if self.f.plus {
            rev.push('+');
        } else if self.f.space {
            rev.push(' ');
        }
        let s: String = rev.into_iter().rev().collect();
        let old = self.f.zero;
        self.f.zero = false;
        self.pad(&s);
        self.f.zero = old;
    }

    fn truncate<'s>(&self, s: &'s str) -> &'s str {
        if self.f.prec_present {
            let n = usize::try_from(self.prec).unwrap_or(0);
            if let Some((i, _)) = s.char_indices().nth(n) {
                return s.get(..i).unwrap_or(s);
            }
        }
        s
    }

    fn fmt_s(&mut self, s: &str) {
        let s = self.truncate(s);
        self.pad(s);
    }

    /// format.go's fmtSbx for strings.
    fn fmt_sx(&mut self, s: &str, digits: &[u8; 17]) {
        let b = s.as_bytes();
        let mut length = len_i64(b.len());
        if self.f.prec_present && self.prec < length {
            length = self.prec;
        }
        let mut width = 2 * length;
        if width > 0 {
            if self.f.space {
                if self.f.sharp {
                    width *= 2;
                }
                width += length - 1;
            } else if self.f.sharp {
                width += 2;
            }
        } else {
            if self.f.wid_present {
                self.write_padding(self.wid);
            }
            return;
        }
        if self.f.wid_present && self.wid > width && !self.f.minus {
            self.write_padding(self.wid - width);
        }
        if self.f.sharp {
            self.buf.push('0');
            self.buf.push(digit(digits, 16));
        }
        for (i, &c) in b.iter().take(usize::try_from(length).unwrap_or(0)).enumerate() {
            if self.f.space && i > 0 {
                self.buf.push(' ');
                if self.f.sharp {
                    self.buf.push('0');
                    self.buf.push(digit(digits, 16));
                }
            }
            self.buf.push(digit(digits, u64::from(c >> 4)));
            self.buf.push(digit(digits, u64::from(c & 0xf)));
        }
        if self.f.wid_present && self.wid > width && self.f.minus {
            self.write_padding(self.wid - width);
        }
    }

    fn fmt_q(&mut self, s: &str) {
        let s = self.truncate(s);
        if self.f.sharp && can_backquote(s) {
            self.pad(&format!("`{s}`"));
            return;
        }
        let mut q = String::new();
        quote_with(&mut q, s, '"', self.f.plus);
        self.pad(&q);
    }

    fn rune(c: u64) -> char {
        u32::try_from(c)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('\u{fffd}')
    }

    fn fmt_c(&mut self, c: u64) {
        let mut tmp = [0u8; 4];
        let s = Printer::rune(c).encode_utf8(&mut tmp).to_owned();
        self.pad(&s);
    }

    fn fmt_qc(&mut self, c: u64) {
        let mut tmp = [0u8; 4];
        let r = Printer::rune(c).encode_utf8(&mut tmp).to_owned();
        let mut q = String::new();
        quote_with(&mut q, &r, '\'', self.f.plus);
        self.pad(&q);
    }

    /// format.go's fmtFloat.
    fn fmt_float_digits(&mut self, v: f64, verb: u8, prec: i64) {
        let prec = if self.f.prec_present { self.prec } else { prec };
        let s = format_float(v, verb, prec);
        let mut num: Vec<u8> = Vec::with_capacity(s.len() + 1);
        if !(s.starts_with('-') || s.starts_with('+')) {
            num.push(b'+');
        }
        num.extend_from_slice(s.as_bytes());
        if self.f.space
            && !self.f.plus
            && let Some(first @ b'+') = num.first_mut()
        {
            *first = b' ';
        }
        if matches!(num.get(1), Some(b'I' | b'N')) {
            let old = self.f.zero;
            self.f.zero = false;
            if num.get(1) == Some(&b'N') && !self.f.space && !self.f.plus {
                num.remove(0);
            }
            self.pad(&String::from_utf8_lossy(&num));
            self.f.zero = old;
            return;
        }
        if self.f.sharp && verb != b'b' {
            let mut digits: i64 = 0;
            if matches!(verb, b'v' | b'g' | b'G' | b'x') {
                digits = prec;
                if digits == -1 {
                    digits = 6;
                }
            }
            let mut tail: Vec<u8> = Vec::new();
            let mut has_point = false;
            let mut saw_nonzero = false;
            let mut i = 1;
            while let Some(&c) = num.get(i) {
                let exponent = match c {
                    b'p' | b'P' => true,
                    b'e' | b'E' => verb != b'x' && verb != b'X',
                    _ => false,
                };
                if exponent {
                    tail.extend(num.drain(i..));
                    break;
                }
                if c == b'.' {
                    has_point = true;
                } else {
                    if c != b'0' {
                        saw_nonzero = true;
                    }
                    if saw_nonzero {
                        digits -= 1;
                    }
                }
                i += 1;
            }
            if !has_point {
                if num.len() == 2 && num.get(1) == Some(&b'0') {
                    digits -= 1;
                }
                num.push(b'.');
            }
            while digits > 0 {
                num.push(b'0');
                digits -= 1;
            }
            num.extend(tail);
        }
        let len = len_i64(num.len());
        if self.f.plus || num.first() != Some(&b'+') {
            if self.f.zero && !self.f.minus && self.f.wid_present && self.wid > len {
                if let Some(&sign) = num.first() {
                    self.buf.push(char::from(sign));
                }
                self.write_padding(self.wid - len);
                self.buf
                    .push_str(&String::from_utf8_lossy(num.get(1..).unwrap_or(&[])));
                return;
            }
            self.pad(&String::from_utf8_lossy(&num));
            return;
        }
        self.pad(&String::from_utf8_lossy(num.get(1..).unwrap_or(&[])));
    }

    fn fmt_int(&mut self, a: &GoArg, u: u64, signed: bool, verb: char) {
        match verb {
            'v' => {
                if self.f.sharp_v && !signed {
                    self.fmt_0x64(u, true);
                } else {
                    self.fmt_integer(u, 10, signed, verb, LDIGITS);
                }
            }
            'd' => self.fmt_integer(u, 10, signed, verb, LDIGITS),
            'b' => self.fmt_integer(u, 2, signed, verb, LDIGITS),
            'o' | 'O' => self.fmt_integer(u, 8, signed, verb, LDIGITS),
            'x' => self.fmt_integer(u, 16, signed, verb, LDIGITS),
            'X' => self.fmt_integer(u, 16, signed, verb, UDIGITS),
            'c' => self.fmt_c(u),
            'q' => self.fmt_qc(u),
            'U' => self.fmt_unicode(u),
            _ => self.bad_verb(verb, a),
        }
    }

    fn fmt_0x64(&mut self, u: u64, leading0x: bool) {
        let sharp = self.f.sharp;
        self.f.sharp = leading0x;
        self.fmt_integer(u, 16, false, 'v', LDIGITS);
        self.f.sharp = sharp;
    }

    fn fmt_float(&mut self, a: &GoArg, f: f64, verb: char) {
        match verb {
            'v' => self.fmt_float_digits(f, b'g', -1),
            'b' | 'g' | 'G' | 'x' | 'X' => self.fmt_float_digits(f, verb as u8, -1),
            'f' | 'e' | 'E' => self.fmt_float_digits(f, verb as u8, 6),
            'F' => self.fmt_float_digits(f, b'f', 6),
            _ => self.bad_verb(verb, a),
        }
    }

    fn fmt_string(&mut self, a: &GoArg, s: &str, verb: char) {
        match verb {
            'v' => {
                if self.f.sharp_v {
                    self.fmt_q(s);
                } else {
                    self.fmt_s(s);
                }
            }
            's' => self.fmt_s(s),
            'x' => self.fmt_sx(s, LDIGITS),
            'X' => self.fmt_sx(s, UDIGITS),
            'q' => self.fmt_q(s),
            _ => self.bad_verb(verb, a),
        }
    }

    fn bad_verb(&mut self, verb: char, a: &GoArg) {
        self.erroring = true;
        self.buf.push_str("%!");
        self.buf.push(verb);
        self.buf.push('(');
        self.buf.push_str(a.type_name());
        self.buf.push('=');
        self.print_arg(a, 'v');
        self.buf.push(')');
        self.erroring = false;
    }

    fn print_arg(&mut self, a: &GoArg, verb: char) {
        if verb == 'T' {
            self.fmt_s(a.type_name());
            return;
        }
        match a {
            GoArg::Int(i) => {
                if verb == 'p' {
                    self.bad_verb(verb, a);
                } else {
                    self.fmt_int(a, *i as u64, true, verb);
                }
            }
            GoArg::Float(f) => {
                if verb == 'p' {
                    self.bad_verb(verb, a);
                } else {
                    self.fmt_float(a, *f, verb);
                }
            }
            GoArg::Str(s) => {
                if verb == 'p' {
                    self.bad_verb(verb, a);
                } else {
                    self.fmt_string(a, s, verb);
                }
            }
            GoArg::Big(b) => {
                // handleMethods: big.Int is a fmt.Formatter, but while erroring and
                // for %w, where reflection prints the struct behind the pointer. Its
                // address (%p) Go prints as such; here it prints as reflection would.
                if self.erroring {
                    self.big_struct(b);
                } else if verb == 'w' || verb == 'p' {
                    self.bad_verb(verb, a);
                } else {
                    self.big_format(b, verb);
                }
            }
        }
    }

    /// printValue of a *big.Int through reflection: `&{neg abs}`.
    fn big_struct(&mut self, b: &BigInt) {
        let names = self.f.plus_v || self.f.sharp_v;
        self.buf.push('&');
        if self.f.sharp_v {
            self.buf.push_str("big.Int");
        }
        self.buf.push('{');
        if names {
            self.buf.push_str("neg:");
        }
        self.fmt_boolean(b.sign() == Sign::Minus);
        if self.f.sharp_v {
            self.buf.push_str(", ");
        } else {
            self.buf.push(' ');
        }
        if names {
            self.buf.push_str("abs:");
        }
        let words = b.magnitude().to_u64_digits();
        if self.f.sharp_v {
            self.buf.push_str("big.nat{");
        } else {
            self.buf.push('[');
        }
        for (i, w) in words.into_iter().enumerate() {
            if i > 0 {
                if self.f.sharp_v {
                    self.buf.push_str(", ");
                } else {
                    self.buf.push(' ');
                }
            }
            if self.f.sharp_v {
                self.fmt_0x64(w, true);
            } else {
                self.fmt_integer(w, 10, false, 'v', LDIGITS);
            }
        }
        self.buf.push(if self.f.sharp_v { '}' } else { ']' });
        self.buf.push('}');
    }

    /// math/big's `(*Int).Format`.
    fn big_format(&mut self, x: &BigInt, ch: char) {
        let base = match ch {
            'b' => 2,
            'o' | 'O' => 8,
            'd' | 's' | 'v' => 10,
            'x' | 'X' => 16,
            _ => {
                let _ = write!(self.buf, "%!{ch}(big.Int={x})");
                return;
            }
        };
        let plus = self.f.plus || self.f.plus_v;
        let sharp = self.f.sharp || self.f.sharp_v;
        let sign = if x.sign() == Sign::Minus {
            "-"
        } else if plus {
            "+"
        } else if self.f.space {
            " "
        } else {
            ""
        };
        let mut prefix = "";
        if sharp {
            prefix = match ch {
                'b' => "0b",
                'o' => "0",
                'x' => "0x",
                'X' => "0X",
                _ => "",
            };
        }
        if ch == 'O' {
            prefix = "0o";
        }
        let mut digits = x.magnitude().to_str_radix(base);
        if ch == 'X' {
            digits = digits.to_ascii_uppercase();
        }
        let (mut left, mut zeros, mut right) = (0i64, 0i64, 0i64);
        let dl = len_i64(digits.len());
        if self.f.prec_present {
            if dl < self.prec {
                zeros = self.prec - dl;
            } else if digits == "0" && self.prec == 0 {
                return;
            }
        }
        let length = len_i64(sign.len() + prefix.len()) + zeros + dl;
        if self.f.wid_present && length < self.wid {
            let d = self.wid - length;
            if self.f.minus {
                right = d;
            } else if self.f.zero && !self.f.prec_present {
                zeros = d;
            } else {
                left = d;
            }
        }
        for _ in 0..left {
            self.buf.push(' ');
        }
        self.buf.push_str(sign);
        self.buf.push_str(prefix);
        for _ in 0..zeros {
            self.buf.push('0');
        }
        self.buf.push_str(&digits);
        for _ in 0..right {
            self.buf.push(' ');
        }
    }

    /// print.go's argNumber.
    fn arg_number(
        &mut self,
        arg_num: usize,
        format: &[u8],
        i: usize,
        num_args: usize,
    ) -> (usize, usize, bool) {
        if format.get(i) != Some(&b'[') {
            return (arg_num, i, false);
        }
        self.reordered = true;
        let (index, wid, ok) = parse_arg_number(format.get(i..).unwrap_or(&[]));
        if ok && index >= 0 && usize::try_from(index).is_ok_and(|x| x < num_args) {
            return (usize::try_from(index).unwrap_or(0), i + wid, true);
        }
        self.good_arg_num = false;
        (arg_num, i + wid, ok)
    }

    /// The verb's print, with the %v and %w flags moved as doPrintf moves them.
    fn print_verb(&mut self, a: &GoArg, verb: char) {
        if verb == 'v' || verb == 'w' {
            self.f.sharp_v = self.f.sharp;
            self.f.sharp = false;
            self.f.plus_v = self.f.plus;
            self.f.plus = false;
        }
        self.print_arg(a, verb);
    }

    /// print.go's doPrintf.
    fn do_printf(&mut self, format: &str, a: &[GoArg]) {
        let fb = format.as_bytes();
        let end = fb.len();
        let mut arg_num: usize = 0;
        let mut after_index;
        self.reordered = false;
        let mut i = 0;
        'format: while i < end {
            self.good_arg_num = true;
            let lasti = i;
            while i < end && fb.get(i) != Some(&b'%') {
                i += 1;
            }
            if i > lasti {
                self.buf.push_str(format.get(lasti..i).unwrap_or(""));
            }
            if i >= end {
                break;
            }
            i += 1;
            self.clear_flags();
            while let Some(&c) = fb.get(i) {
                match c {
                    b'#' => self.f.sharp = true,
                    b'0' => self.f.zero = true,
                    b'+' => self.f.plus = true,
                    b'-' => self.f.minus = true,
                    b' ' => self.f.space = true,
                    _ => {
                        if c.is_ascii_lowercase() && arg_num < a.len() {
                            if let Some(x) = a.get(arg_num) {
                                self.print_verb(x, char::from(c));
                            }
                            arg_num += 1;
                            i += 1;
                            continue 'format;
                        }
                        break;
                    }
                }
                i += 1;
            }
            (arg_num, i, after_index) = self.arg_number(arg_num, fb, i, a.len());
            if fb.get(i) == Some(&b'*') {
                i += 1;
                let (n, ok, next) = int_from_arg(a, arg_num);
                self.wid = n;
                self.f.wid_present = ok;
                arg_num = next;
                if !ok {
                    self.buf.push_str("%!(BADWIDTH)");
                }
                if self.wid < 0 {
                    self.wid = -self.wid;
                    self.f.minus = true;
                    self.f.zero = false;
                }
                after_index = false;
            } else {
                let (n, ok, next) = parse_num(fb, i, end);
                self.wid = n;
                self.f.wid_present = ok;
                i = next;
                if after_index && self.f.wid_present {
                    self.good_arg_num = false;
                }
            }
            if i + 1 < end && fb.get(i) == Some(&b'.') {
                i += 1;
                if after_index {
                    self.good_arg_num = false;
                }
                (arg_num, i, after_index) = self.arg_number(arg_num, fb, i, a.len());
                if fb.get(i) == Some(&b'*') {
                    i += 1;
                    let (n, ok, next) = int_from_arg(a, arg_num);
                    self.prec = n;
                    self.f.prec_present = ok;
                    arg_num = next;
                    if self.prec < 0 {
                        self.prec = 0;
                        self.f.prec_present = false;
                    }
                    if !self.f.prec_present {
                        self.buf.push_str("%!(BADPREC)");
                    }
                    after_index = false;
                } else {
                    let (n, ok, next) = parse_num(fb, i, end);
                    self.prec = n;
                    self.f.prec_present = ok;
                    i = next;
                    if !self.f.prec_present {
                        self.prec = 0;
                        self.f.prec_present = true;
                    }
                }
            }
            if !after_index {
                (arg_num, i, _) = self.arg_number(arg_num, fb, i, a.len());
            }
            if i >= end {
                self.buf.push_str("%!(NOVERB)");
                break;
            }
            let verb = format
                .get(i..)
                .and_then(|s| s.chars().next())
                .unwrap_or('\u{fffd}');
            i += verb.len_utf8();
            match verb {
                '%' => self.buf.push('%'),
                _ if !self.good_arg_num => {
                    self.buf.push_str("%!");
                    self.buf.push(verb);
                    self.buf.push_str("(BADINDEX)");
                }
                _ if arg_num >= a.len() => {
                    self.buf.push_str("%!");
                    self.buf.push(verb);
                    self.buf.push_str("(MISSING)");
                }
                _ => {
                    if let Some(x) = a.get(arg_num) {
                        self.print_verb(x, verb);
                    }
                    arg_num += 1;
                }
            }
        }
        if !self.reordered && arg_num < a.len() {
            self.clear_flags();
            self.buf.push_str("%!(EXTRA ");
            for (i, x) in a.iter().skip(arg_num).enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.buf.push_str(x.type_name());
                self.buf.push('=');
                self.print_arg(x, 'v');
            }
            self.buf.push(')');
        }
    }
}

/// print.go's parsenum.
fn parse_num(s: &[u8], start: usize, end: usize) -> (i64, bool, usize) {
    if start >= end {
        return (0, false, end);
    }
    let mut num: i64 = 0;
    let mut isnum = false;
    let mut i = start;
    while i < end {
        let Some(&c) = s.get(i) else { break };
        if !c.is_ascii_digit() {
            break;
        }
        if too_large(num) {
            return (0, false, end);
        }
        num = num * 10 + i64::from(c - b'0');
        isnum = true;
        i += 1;
    }
    (num, isnum, i)
}

/// print.go's parseArgNumber.
fn parse_arg_number(format: &[u8]) -> (i64, usize, bool) {
    if format.len() < 3 {
        return (0, 1, false);
    }
    for i in 1..format.len() {
        if format.get(i) == Some(&b']') {
            let (width, ok, newi) = parse_num(format, 1, i);
            if !ok || newi != i {
                return (0, i + 1, false);
            }
            return (width - 1, i + 1, true);
        }
    }
    (0, 1, false)
}

/// print.go's intFromArg: only an int is one.
fn int_from_arg(a: &[GoArg], arg_num: usize) -> (i64, bool, usize) {
    let Some(x) = a.get(arg_num) else {
        return (0, false, arg_num);
    };
    let (mut num, mut is_int) = match x {
        GoArg::Int(i) => (*i, true),
        _ => (0, false),
    };
    if too_large(num) {
        num = 0;
        is_int = false;
    }
    (num, is_int, arg_num + 1)
}

const LOWERHEX: &[u8; 16] = b"0123456789abcdef";

fn hex_digit(n: u32) -> char {
    char::from(LOWERHEX.get((n & 0xf) as usize).copied().unwrap_or(b'0'))
}

/// strconv's appendQuotedWith: Quote, QuoteToASCII, QuoteRune, QuoteRuneToASCII.
fn quote_with(out: &mut String, s: &str, quote: char, ascii_only: bool) {
    out.push(quote);
    for r in s.chars() {
        let u = u32::from(r);
        if r == quote || r == '\\' {
            out.push('\\');
            out.push(r);
            continue;
        }
        if ascii_only {
            if r.is_ascii() && goquote::is_print(u) {
                out.push(r);
                continue;
            }
        } else if goquote::is_print(u) {
            out.push(r);
            continue;
        }
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
    out.push(quote);
}

/// strconv.CanBackquote.
fn can_backquote(s: &str) -> bool {
    s.chars().all(|c| {
        if c.len_utf8() > 1 {
            return c != '\u{feff}';
        }
        !((c < ' ' && c != '\t') || c == '`' || c == '\x7f')
    })
}

/// Decimal digits (no leading or trailing zeros) and the decimal point's position:
/// the value is 0.DIGITS × 10^dp. Zero has no digits.
struct Decimal {
    d: Vec<u8>,
    dp: i64,
}

impl Decimal {
    fn nd(&self) -> i64 {
        len_i64(self.d.len())
    }

    fn digit(&self, i: i64) -> Option<u8> {
        usize::try_from(i).ok().and_then(|i| self.d.get(i).copied())
    }

    fn trimmed(mut d: Vec<u8>, mut dp: i64) -> Decimal {
        let lead = d.iter().take_while(|&&c| c == b'0').count();
        d.drain(..lead);
        dp -= len_i64(lead);
        while d.last() == Some(&b'0') {
            d.pop();
        }
        if d.is_empty() {
            dp = 0;
        }
        Decimal { d, dp }
    }

    /// From Rust's `{:e}` form, `D.DDDDe±X`, which is exact (or shortest) as Go's is.
    fn from_exp(s: &str) -> Decimal {
        let (mant, exp) = s.split_once('e').unwrap_or((s, "0"));
        let exp: i64 = exp.parse().unwrap_or(0);
        let d: Vec<u8> = mant.bytes().filter(u8::is_ascii_digit).collect();
        Decimal::trimmed(d, exp + 1)
    }

    /// From Rust's `{:.N}` form, `III.FFF`.
    fn from_fixed(s: &str) -> Decimal {
        let (int, frac) = s.split_once('.').unwrap_or((s, ""));
        let d: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
        Decimal::trimmed(d, len_i64(int.len()))
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

/// ftoa.go's fmtX: %x and %X, a hexadecimal mantissa and binary exponent.
fn fmt_x(out: &mut String, prec: i64, fmt: u8, neg: bool, mut mant: u64, mut exp: i64) {
    const MANTBITS: u32 = 52;
    if mant == 0 {
        exp = 0;
    }
    mant <<= 60 - MANTBITS;
    while mant != 0 && mant & (1 << 60) == 0 {
        mant <<= 1;
        exp -= 1;
    }
    if (0..15).contains(&prec) {
        let shift = u32::try_from(prec * 4).unwrap_or(0);
        let extra = (mant << shift) & ((1 << 60) - 1);
        mant >>= 60 - shift;
        if extra | (mant & 1) > 1 << 59 {
            mant += 1;
        }
        mant <<= 60 - shift;
        if mant & (1 << 61) != 0 {
            mant >>= 1;
            exp += 1;
        }
    }
    let upper = fmt == b'X';
    let hex = |n: u64| -> char {
        let c = hex_digit(u32::try_from(n & 15).unwrap_or(0));
        if upper { c.to_ascii_uppercase() } else { c }
    };
    if neg {
        out.push('-');
    }
    out.push('0');
    out.push(char::from(fmt));
    out.push(if (mant >> 60) & 1 == 1 { '1' } else { '0' });
    mant <<= 4;
    if prec < 0 && mant != 0 {
        out.push('.');
        while mant != 0 {
            out.push(hex(mant >> 60));
            mant <<= 4;
        }
    } else if prec > 0 {
        out.push('.');
        for _ in 0..prec {
            out.push(hex(mant >> 60));
            mant <<= 4;
        }
    }
    out.push(if upper { 'P' } else { 'p' });
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

/// The shortest digits of `a` (positive, finite) as Go's Dragonbox picks them: Rust's
/// shortest digits are Go's but where `a` lies exactly halfway between two shortest
/// candidates, where Go takes the one whose last digit is even (ftoadbox.go) and Rust
/// may not.
fn shortest_even(a: f64, d: Decimal) -> Decimal {
    let Some(&last) = d.d.last() else { return d };
    if last % 2 == 0 {
        return d;
    }
    // The exact value, 0.EXACT × 10^dp (an f64's expansion ends within 1100 digits).
    let exact = Decimal::from_exp(&format!("{a:.1100e}"));
    let to_int = |digits: &[u8]| -> BigInt {
        std::str::from_utf8(digits)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_default()
    };
    // Both as integers × 10^c.
    let (ed, dd) = (exact.dp - exact.nd(), d.dp - d.nd());
    let c = ed.min(dd);
    let pow10 = |n: i64| -> BigInt { BigInt::from(10u32).pow(u32::try_from(n).unwrap_or(0)) };
    let ie = to_int(&exact.d) * pow10(ed - c);
    let id = to_int(&d.d) * pow10(dd - c);
    let unit = pow10(dd - c);
    let diff = &ie - &id;
    let twice: BigInt = &diff * 2;
    if twice.magnitude() != unit.magnitude() {
        return d;
    }
    let alt: BigInt = if diff.sign() == Sign::Minus {
        to_int(&d.d) - 1u32
    } else {
        to_int(&d.d) + 1u32
    };
    let text = alt.to_string();
    if format!("{text}e{dd}").parse::<f64>().ok() != Some(a) {
        return d;
    }
    let n = len_i64(text.len());
    Decimal::trimmed(text.into_bytes(), dd + n)
}

/// `strconv.FormatFloat(v, fmt, prec, 64)` for the formats b, e, E, f, g, G, x and X.
fn format_float(v: f64, fmt: u8, prec: i64) -> String {
    let mut out = String::new();
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Inf" } else { "+Inf" }.into();
    }
    let neg = v.is_sign_negative();
    let a = v.abs();
    if matches!(fmt, b'b' | b'x' | b'X') {
        let bits = v.to_bits();
        let mut exp = i64::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
        let mut mant = bits & ((1u64 << 52) - 1);
        if exp == 0 {
            exp += 1;
        } else {
            mant |= 1u64 << 52;
        }
        exp += -1023;
        if fmt != b'b' {
            fmt_x(&mut out, prec, fmt, neg, mant, exp);
            return out;
        }
        exp -= 52;
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
        let d = shortest_even(a, Decimal::from_exp(&format!("{a:e}")));
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

/// The variables of `strings.render_template` as OPA hands them to text/template:
/// `ast.As` decodes the object's text (`Value.String()`) as JSON into a
/// `map[string]any`, numbers as `json.Number` (a string here). What that text is not
/// JSON for fails with encoding/json's syntax error: a set, a key that is not a string,
/// and an escape strconv.Quote writes that JSON has not (`\a`, `\v`, `\x`, `\U`).
fn template_value(v: &Value) -> Result<shards_template::Value, String> {
    use shards_template::Value as T;
    let invalid = |c: char, ctx: &str| format!("invalid character '{c}' {ctx}");
    let first_char = |v: &Value| -> char {
        let mut s = String::new();
        opa_text(&mut s, v);
        s.chars().next().unwrap_or(' ')
    };
    Ok(match v {
        Value::Null => T::Nil,
        Value::Bool(b) => T::Bool(*b),
        Value::Number(n) => T::String(n.text().to_string()),
        Value::String(s) => {
            json_escapes_ok(s)?;
            T::String(s.to_string())
        }
        Value::Array(a) => T::list(a.iter().map(template_value).collect::<Result<_, _>>()?),
        Value::Object(o) => {
            let mut m = std::collections::BTreeMap::new();
            for (k, x) in o.iter() {
                let Value::String(k) = k else {
                    return Err(invalid(
                        first_char(k),
                        "looking for beginning of object key string",
                    ));
                };
                json_escapes_ok(k)?;
                m.insert(k.to_string(), template_value(x)?);
            }
            T::map(m)
        }
        Value::Set(s) => match s.iter().next() {
            None => return Err(invalid('s', "looking for beginning of value")),
            Some(Value::String(first)) => {
                json_escapes_ok(first)?;
                let next = if s.len() > 1 { ',' } else { '}' };
                return Err(invalid(next, "after object key"));
            }
            Some(first) => {
                return Err(invalid(
                    first_char(first),
                    "looking for beginning of object key string",
                ));
            }
        },
    })
}

/// Whether strconv.Quote's text of `s` reads as a JSON string, encoding/json's error
/// where it does not.
fn json_escapes_ok(s: &str) -> Result<(), String> {
    for r in s.chars() {
        let u = u32::from(r);
        if r == '"' || r == '\\' || goquote::is_print(u) {
            continue;
        }
        let bad = match r {
            '\x07' => 'a',
            '\x0b' => 'v',
            '\x08' | '\x0c' | '\n' | '\r' | '\t' => continue,
            _ if u < 0x20 || u == 0x7f => 'x',
            _ if u < 0x10000 => continue,
            _ => 'U',
        };
        return Err(format!("invalid character '{bad}' in string escape code"));
    }
    Ok(())
}

fn render_template(_: &mut Context, args: &[Value]) -> Out {
    let text = string_operand(arg(args, 0)?, 1)?;
    let vars = arg(args, 1)?;
    if !matches!(vars, Value::Object(_)) {
        return Err(BuiltinError::operand_type(2, vars, &["object"]));
    }
    let data = template_value(vars).map_err(BuiltinError::Other)?;
    let tmpl = shards_template::Template::parse("template", text).map_err(BuiltinError::Other)?;
    let out = tmpl.execute(&data).map_err(BuiltinError::Other)?;
    ok_str(out.replace("<no value>", "<undefined>"))
}

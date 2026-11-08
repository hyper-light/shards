//! Texts: topdown/semver.go (with OPA's internal/semver), uuid.go (with OPA's
//! internal/uuid and google/uuid v1.6.0's Parse) and template_string.go.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use super::super::{BuiltinError, Context, arg, string_operand};
use super::{Out, array_operand, numbers, ok};
use crate::value::Value;

/// internal/semver's Version.
struct Version<'a> {
    major: i64,
    minor: i64,
    patch: i64,
    pre: &'a str,
}

/// strings.Cut at a byte.
fn cut(s: &str, sep: char) -> (&str, &str) {
    s.split_once(sep).unwrap_or((s, ""))
}

/// `^[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*$`.
fn identifiers_ok(s: &str) -> bool {
    s.split('.')
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// strconv.ParseInt(s, 10, 64).
fn parse_int(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok()
}

/// internal/semver's Parse.
fn parse(version: &str) -> Option<Version<'_>> {
    let version = version.strip_prefix('v').unwrap_or(version);
    let (version, meta) = cut(version, '+');
    if !meta.is_empty() && !identifiers_ok(meta) {
        return None;
    }
    let (version, pre) = cut(version, '-');
    if !pre.is_empty() && !identifiers_ok(pre) {
        return None;
    }
    if version.matches('.').count() != 2 {
        return None;
    }
    let (major, after) = cut(version, '.');
    let (minor, patch) = cut(after, '.');
    Some(Version {
        major: parse_int(major)?,
        minor: parse_int(minor)?,
        patch: parse_int(patch)?,
        pre,
    })
}

/// strconv.Atoi of decimal digits: out of range clamps to the largest int.
fn atoi(s: &str) -> i64 {
    s.parse::<i64>().unwrap_or(i64::MAX)
}

fn is_all_decimals(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Version.Compare.
fn compare(v: &Version<'_>, o: &Version<'_>) -> Ordering {
    let core = (v.major, v.minor, v.patch).cmp(&(o.major, o.minor, o.patch));
    if core != Ordering::Equal || v.pre == o.pre {
        return core;
    }
    if v.pre.is_empty() {
        return Ordering::Greater;
    }
    if o.pre.is_empty() {
        return Ordering::Less;
    }
    let (mut a, mut after_a) = cut(v.pre, '.');
    let (mut b, mut after_b) = cut(o.pre, '.');
    loop {
        if a.is_empty() && !b.is_empty() {
            return Ordering::Less;
        }
        if !a.is_empty() && b.is_empty() {
            return Ordering::Greater;
        }
        let (a_int, b_int) = (is_all_decimals(a), is_all_decimals(b));
        match (a_int, b_int) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            (true, true) => match atoi(a).cmp(&atoi(b)) {
                Ordering::Equal => {}
                o => return o,
            },
            (false, false) => match a.as_bytes().cmp(b.as_bytes()) {
                Ordering::Equal => {}
                o => return o,
            },
        }
        if !after_a.is_empty() && after_b.is_empty() {
            return Ordering::Greater;
        }
        if after_a.is_empty() && !after_b.is_empty() {
            return Ordering::Less;
        }
        // Different texts whose identifiers all compare equal ("1" and "01"): OPA
        // loops on them forever; they compare equal here.
        if after_a.is_empty() && after_b.is_empty() {
            return Ordering::Equal;
        }
        (a, after_a) = cut(after_a, '.');
        (b, after_b) = cut(after_b, '.');
    }
}

pub(super) fn semver_compare(_: &mut Context, args: &[Value]) -> Out {
    let a = string_operand(arg(args, 0)?, 1)?;
    let b = string_operand(arg(args, 1)?, 2)?;
    // `%s` of an ast.String writes it quoted.
    let invalid = |pos: usize, s: &str| {
        let mut q = String::new();
        crate::goquote::quote(&mut q, s);
        BuiltinError::Other(format!("operand {pos}: string {q} is not a valid SemVer"))
    };
    let va = parse(a).ok_or_else(|| invalid(1, a))?;
    let vb = parse(b).ok_or_else(|| invalid(2, b))?;
    ok(Value::int(match compare(&va, &vb) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }))
}

pub(super) fn semver_is_valid(_: &mut Context, args: &[Value]) -> Out {
    let valid = match arg(args, 0)? {
        Value::String(s) => parse(s).is_some(),
        _ => false,
    };
    ok(Value::Bool(valid))
}

pub(super) fn uuid_rfc4122(ctx: &mut Context, args: &[Value]) -> Out {
    arg(args, 0)?;
    let mut bs = numbers::read_seed(ctx, 16)?;
    if let Some(b) = bs.get_mut(8) {
        *b = *b & !0xc0 | 0x80;
    }
    if let Some(b) = bs.get_mut(6) {
        *b = *b & !0xf0 | 0x40;
    }
    let hex = |r: std::ops::Range<usize>| -> String {
        bs.get(r)
            .unwrap_or_default()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    ok(Value::string(format!(
        "{}-{}-{}-{}-{}",
        hex(0..4),
        hex(4..6),
        hex(6..8),
        hex(8..10),
        hex(10..16)
    )))
}

fn xtob(hi: u8, lo: u8) -> Option<u8> {
    let d = |c: u8| (c as char).to_digit(16).and_then(|d| u8::try_from(d).ok());
    Some(d(hi)? << 4 | d(lo)?)
}

/// google/uuid's Parse.
fn uuid_bytes(s: &str) -> Option<[u8; 16]> {
    let mut s = s.as_bytes();
    let mut uuid = [0u8; 16];
    match s.len() {
        36 => {}
        45 => {
            if !s.get(..9)?.eq_ignore_ascii_case(b"urn:uuid:") {
                return None;
            }
            s = s.get(9..)?;
        }
        38 => s = s.get(1..)?,
        32 => {
            for (i, b) in uuid.iter_mut().enumerate() {
                *b = xtob(*s.get(i * 2)?, *s.get(i * 2 + 1)?)?;
            }
            return Some(uuid);
        }
        _ => return None,
    }
    if [8, 13, 18, 23].iter().any(|&i| s.get(i) != Some(&b'-')) {
        return None;
    }
    let at = [0, 2, 4, 6, 9, 11, 14, 16, 19, 21, 24, 26, 28, 30, 32, 34];
    for (b, x) in uuid.iter_mut().zip(at) {
        *b = xtob(*s.get(x)?, *s.get(x + 1)?)?;
    }
    Some(uuid)
}

/// 100s of nanoseconds between 15 October 1582 and the Unix epoch.
const G1582NS100: i64 = (2440587 - 2299160) * 86400 * 10_000_000;

pub(super) fn uuid_parse(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    let Some(u) = uuid_bytes(s) else {
        return Ok(None);
    };
    let b = |i: usize| u.get(i).copied().unwrap_or(0);
    let be16 = |i: usize| u16::from_be_bytes([b(i), b(i + 1)]);
    let be32 = |i: usize| u32::from_be_bytes([b(i), b(i + 1), b(i + 2), b(i + 3)]);
    let version = b(6) >> 4;
    let variant = match b(8) {
        x if x & 0xc0 == 0x80 => "RFC4122",
        x if x & 0xe0 == 0xc0 => "Microsoft",
        x if x & 0xe0 == 0xe0 => "Future",
        _ => "Reserved",
    };
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        m.insert(Value::string(k), v);
    };
    put("version", Value::int(i64::from(version)));
    put("variant", Value::string(variant));
    if version == 1 || version == 2 {
        // Time's forward-compatible form, then UnixTime and OPA's nanoUnix.
        let t = i64::from(be32(0)) | i64::from(be16(4)) << 32 | i64::from(be16(6) & 0xfff) << 48;
        let sec = t.wrapping_sub(G1582NS100);
        let nsec = (sec % 10_000_000) * 100;
        let sec = sec / 10_000_000;
        put(
            "time",
            Value::int(sec.wrapping_mul(1_000_000_000).wrapping_add(nsec)),
        );
        let node: Vec<String> = (10..16).map(|i| format!("{:02x}", b(i))).collect();
        put("nodeid", Value::string(node.join("-")));
        let mac = match b(10) {
            x if x & 0b11 == 0b11 => "local:multicast",
            x if x & 0b01 == 0b01 => "global:multicast",
            x if x & 0b10 == 0b10 => "local:unicast",
            _ => "global:unicast",
        };
        put("macvariables", Value::string(mac));
        put("clocksequence", Value::int(i64::from(be16(8) & 0x3fff)));
        if version == 2 {
            put("id", Value::int(i64::from(be32(0))));
            let domain = match b(9) {
                0 => "Person".to_string(),
                1 => "Group".to_string(),
                2 => "Org".to_string(),
                d => format!("Domain{d}"),
            };
            put("domain", Value::string(domain));
        }
    }
    ok(Value::object(m))
}

/// A value as OPA writes it in policy (ast's AppendText).
fn write_text(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(n.text()),
        Value::String(s) => crate::goquote::quote(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_text(out, x);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_text(out, k);
                out.push_str(": ");
                write_text(out, x);
            }
            out.push('}');
        }
        Value::Set(s) if s.is_empty() => out.push_str("set()"),
        Value::Set(s) => {
            out.push('{');
            for (i, x) in s.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_text(out, x);
            }
            out.push('}');
        }
    }
}

/// A part as builtinPrintCrossProductOperands writes it: strings raw, others as
/// policy text.
fn part(v: &Value) -> String {
    match v {
        Value::String(s) => s.to_string(),
        v => {
            let mut s = String::new();
            write_text(&mut s, v);
            s
        }
    }
}

/// builtinPrintCrossProductOperands, its one output the result and a second a
/// conflict.
struct Cross<'a> {
    operands: &'a [Value],
    buf: Vec<String>,
    outputs: usize,
    result: String,
}

impl Cross<'_> {
    fn walk(&mut self, i: usize) -> Result<(), BuiltinError> {
        let Some(operand) = self.operands.get(i) else {
            self.outputs += 1;
            if self.outputs > 1 {
                return Err(BuiltinError::Halt(
                    "eval_conflict_error: template-strings must not produce multiple outputs".into(),
                ));
            }
            self.result = self.buf.concat();
            return Ok(());
        };
        match operand {
            Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => self.with(part(operand), i),
            Value::Set(s) if s.is_empty() => self.with("<undefined>".into(), i),
            Value::Set(s) => {
                for x in s.iter() {
                    self.with(part(x), i)?;
                }
                Ok(())
            }
            v => Err(BuiltinError::Halt(format!(
                "eval_internal_error: illegal argument type: {}",
                v.type_name()
            ))),
        }
    }

    fn with(&mut self, text: String, i: usize) -> Result<(), BuiltinError> {
        self.buf.push(text);
        let r = self.walk(i + 1);
        self.buf.pop();
        r
    }
}

pub(super) fn template_string(_: &mut Context, args: &[Value]) -> Out {
    let operands = array_operand(arg(args, 0)?, 1)?;
    let mut c = Cross {
        operands,
        buf: Vec::with_capacity(operands.len()),
        outputs: 0,
        result: String::new(),
    };
    c.walk(0)?;
    ok(Value::string(c.result))
}

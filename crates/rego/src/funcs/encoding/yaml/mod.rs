//! YAML as OPA reads and writes it: sigs.k8s.io/yaml v1.6.0 over go.yaml.in/yaml/v2
//! v2.4.2 (go-yaml v2, libyaml's C ported to Go), ported here as there.
//!
//! `yaml.unmarshal` is `yaml.YAMLToJSON`: go-yaml decodes the first document into Go
//! values (YAML 1.1 resolution: yes/no/on/off, octal, base-60 left as strings,
//! timestamps kept as their text, merges, aliases), sigs.k8s.io turns map keys into
//! strings, and encoding/json writes the result (which NaN and infinities fail), read
//! back as OPA reads JSON. `yaml.marshal` is `yaml.Marshal`: the value's JSON read by
//! go-yaml, then emitted by go-yaml's encoder and libyaml's emitter.

mod emitter;
mod parser;
mod scanner;

use std::collections::{BTreeMap, HashMap, HashSet};

use super::gofmt;
use crate::value::{Number, Value};
use parser::{EventType, Parser};
use scanner::YamlError;

/// Where a decoded value sits in its [`Arena`].
pub type Id = usize;

/// A Go value as go-yaml decodes into `interface{}`, its elements by [`Id`].
#[derive(Debug, Clone)]
pub enum GoVal {
    Nil,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(Vec<u8>),
    Seq(Vec<Id>),
    Map(GoMap),
}

/// A `map[interface{}]interface{}`: entries in insertion order, keys unique by Go's ==.
#[derive(Debug, Clone, Default)]
pub struct GoMap {
    pub entries: Vec<(Id, Id)>,
    index: HashMap<Vec<u8>, usize>,
}

impl GoMap {
    /// Sets k (whose identity is `key_id`) to v, as SetMapIndex does.
    fn set(&mut self, key_id: Option<Vec<u8>>, k: Id, v: Id) {
        match key_id {
            Some(id) => {
                if let Some(&i) = self.index.get(&id) {
                    if let Some(slot) = self.entries.get_mut(i) {
                        slot.1 = v;
                    }
                } else {
                    self.index.insert(id, self.entries.len());
                    self.entries.push((k, v));
                }
            }
            None => self.entries.push((k, v)),
        }
    }
}

/// A scalar key's identity under Go's == on interfaces; None for NaN, never equal.
fn key_id(k: &GoVal) -> Option<Vec<u8>> {
    let mut id = Vec::new();
    match k {
        GoVal::Nil => id.push(0),
        GoVal::Bool(b) => id.extend_from_slice(&[1, u8::from(*b)]),
        GoVal::Int(i) => {
            id.push(2);
            id.extend_from_slice(&i.to_be_bytes());
        }
        GoVal::Uint(u) => {
            id.push(3);
            id.extend_from_slice(&u.to_be_bytes());
        }
        GoVal::Float(f) => {
            if f.is_nan() {
                return None;
            }
            id.push(4);
            let f = if *f == 0.0 { 0.0f64 } else { *f };
            id.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        GoVal::Str(s) => {
            id.push(5);
            id.extend_from_slice(s);
        }
        GoVal::Seq(_) | GoVal::Map(_) => return None,
    }
    Some(id)
}

/// Decoded values, one slot each, so that no walk over them (or their drop) recurses
/// as deep as the document nests.
#[derive(Debug, Default)]
pub struct Arena {
    vals: Vec<GoVal>,
}

static NIL: GoVal = GoVal::Nil;

impl Arena {
    fn add(&mut self, v: GoVal) -> Id {
        self.vals.push(v);
        self.vals.len() - 1
    }

    pub fn get(&self, id: Id) -> &GoVal {
        self.vals.get(id).unwrap_or(&NIL)
    }
}

// ---- tags (resolve.go) ----

const LONG_TAG_PREFIX: &str = "tag:yaml.org,2002:";
const NULL_TAG: &str = "tag:yaml.org,2002:null";
const BOOL_TAG: &str = "tag:yaml.org,2002:bool";
const STR_TAG: &str = "tag:yaml.org,2002:str";
const INT_TAG: &str = "tag:yaml.org,2002:int";
const FLOAT_TAG: &str = "tag:yaml.org,2002:float";
const TIMESTAMP_TAG: &str = "tag:yaml.org,2002:timestamp";
const BINARY_TAG: &str = "tag:yaml.org,2002:binary";
const MERGE_TAG: &str = "tag:yaml.org,2002:merge";

fn short_tag(tag: &str) -> String {
    match tag.strip_prefix(LONG_TAG_PREFIX) {
        Some(rest) => format!("!!{rest}"),
        None => tag.to_string(),
    }
}

fn resolvable_tag(tag: &str) -> bool {
    matches!(
        tag,
        "" | STR_TAG | BOOL_TAG | INT_TAG | FLOAT_TAG | NULL_TAG | TIMESTAMP_TAG
    )
}

/// A resolved scalar: a Go value, or a timestamp (kept as its text in interface{}).
#[derive(Debug, Clone)]
enum Resolved {
    Val(GoVal),
    Timestamp,
}

fn resolve_map(s: &[u8]) -> Option<(&'static str, GoVal)> {
    Some(match s {
        b"y" | b"Y" | b"yes" | b"Yes" | b"YES" | b"true" | b"True" | b"TRUE" | b"on" | b"On" | b"ON" => {
            (BOOL_TAG, GoVal::Bool(true))
        }
        b"n" | b"N" | b"no" | b"No" | b"NO" | b"false" | b"False" | b"FALSE" | b"off" | b"Off" | b"OFF" => {
            (BOOL_TAG, GoVal::Bool(false))
        }
        b"" | b"~" | b"null" | b"Null" | b"NULL" => (NULL_TAG, GoVal::Nil),
        b".nan" | b".NaN" | b".NAN" => (FLOAT_TAG, GoVal::Float(f64::NAN)),
        b".inf" | b".Inf" | b".INF" | b"+.inf" | b"+.Inf" | b"+.INF" => {
            (FLOAT_TAG, GoVal::Float(f64::INFINITY))
        }
        b"-.inf" | b"-.Inf" | b"-.INF" => (FLOAT_TAG, GoVal::Float(f64::NEG_INFINITY)),
        b"<<" => (MERGE_TAG, GoVal::Str(b"<<".to_vec())),
        _ => return None,
    })
}

fn resolve_hint(c: u8) -> u8 {
    match c {
        b'+' | b'-' => b'S',
        b'0'..=b'9' => b'D',
        b'y' | b'Y' | b'n' | b'N' | b't' | b'T' | b'f' | b'F' | b'o' | b'O' | b'~' => b'M',
        b'.' => b'.',
        _ => 0,
    }
}

/// yamlStyleFloat: `^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$`.
fn yaml_style_float(s: &[u8]) -> bool {
    let mut i = 0;
    let at = |i: usize| s.get(i).copied();
    if matches!(at(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let digits = |mut i: usize| {
        let st = i;
        while at(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        (i, i - st)
    };
    if at(i) == Some(b'.') {
        let (j, n) = digits(i + 1);
        if n == 0 {
            return false;
        }
        i = j;
    } else {
        let (j, n) = digits(i);
        if n == 0 {
            return false;
        }
        i = j;
        if at(i) == Some(b'.') {
            i = digits(i + 1).0;
        }
    }
    if matches!(at(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(at(i), Some(b'-' | b'+')) {
            i += 1;
        }
        let (j, n) = digits(i);
        if n == 0 {
            return false;
        }
        i = j;
    }
    i == s.len()
}

/// strconv.ParseInt(s, base, 64), base 0 reading Go's prefixes and underscores.
fn parse_int(s: &[u8], base: u32) -> Option<i64> {
    let (neg, body) = match s.first() {
        Some(b'-') => (true, s.get(1..).unwrap_or_default()),
        Some(b'+') => (false, s.get(1..).unwrap_or_default()),
        _ => (false, s),
    };
    let u = parse_uint(body, base)?;
    if neg {
        if u > 1u64 << 63 {
            return None;
        }
        Some(0i64.wrapping_sub_unsigned(u))
    } else {
        i64::try_from(u).ok()
    }
}

/// strconv.ParseUint(s, base, 64).
fn parse_uint(s: &[u8], base: u32) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut base = base;
    let mut s = s;
    let base0 = base == 0;
    if base == 0 {
        base = 10;
        if s.first() == Some(&b'0') {
            match s.get(1).map(u8::to_ascii_lowercase) {
                Some(b'b') if s.len() >= 3 => {
                    base = 2;
                    s = s.get(2..).unwrap_or_default();
                }
                Some(b'o') if s.len() >= 3 => {
                    base = 8;
                    s = s.get(2..).unwrap_or_default();
                }
                Some(b'x') if s.len() >= 3 => {
                    base = 16;
                    s = s.get(2..).unwrap_or_default();
                }
                _ => {
                    base = 8;
                    s = s.get(1..).unwrap_or_default();
                }
            }
        }
    }
    let mut n: u64 = 0;
    for &c in s {
        let d = match c {
            b'_' if base0 => continue,
            b'0'..=b'9' => u32::from(c - b'0'),
            b'a'..=b'z' => u32::from(c - b'a') + 10,
            b'A'..=b'Z' => u32::from(c - b'A') + 10,
            _ => return None,
        };
        if d >= base {
            return None;
        }
        n = n.checked_mul(u64::from(base))?.checked_add(u64::from(d))?;
    }
    Some(n)
}

/// strconv's underscoreOK: underscores only between digits (a base prefix counts).
fn underscore_ok(s: &[u8]) -> bool {
    let mut saw = b'^';
    let mut i = 0;
    let mut s = s;
    if matches!(s.first(), Some(b'-' | b'+')) {
        s = s.get(1..).unwrap_or_default();
    }
    let mut hex = false;
    if s.len() >= 2 && s.first() == Some(&b'0') {
        let c = s.get(1).map(u8::to_ascii_lowercase);
        if matches!(c, Some(b'b' | b'o' | b'x')) {
            i = 2;
            saw = b'0';
            hex = c == Some(b'x');
        }
    }
    while let Some(&c) = s.get(i) {
        let lc = c.to_ascii_lowercase();
        if c.is_ascii_digit() || (hex && (b'a'..=b'f').contains(&lc)) {
            saw = b'0';
        } else if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
        } else {
            if saw == b'_' {
                return false;
            }
            saw = b'!';
        }
        i += 1;
    }
    saw != b'_'
}

/// strconv.ParseInt(s, 0, 64) with Go's underscore rule.
fn parse_int0(s: &[u8]) -> Option<i64> {
    if s.contains(&b'_') && !underscore_ok(s) {
        return None;
    }
    parse_int(s, 0)
}

/// strconv.ParseUint(s, 0, 64) with Go's underscore rule.
fn parse_uint0(s: &[u8]) -> Option<u64> {
    if s.contains(&b'_') && !underscore_ok(s) {
        return None;
    }
    parse_uint(s, 0)
}

/// strconv.ParseFloat(s, 64) for decimal texts (Go's syntax, underscores included); None
/// on a syntax error or overflow. Hex floats and inf/nan words are not reached here.
fn parse_float(s: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(s).ok()?;
    let lower = text.to_ascii_lowercase();
    let body = lower.trim_start_matches(['+', '-']);
    if body.len() + 1 < lower.len() {
        return None;
    }
    if matches!(body, "inf" | "infinity") {
        return Some(if lower.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    if body == "nan" {
        return Some(f64::NAN);
    }
    if body.starts_with("0x") {
        return None;
    }
    // Decimal: digits, one '.', digits, optional exponent; underscores per underscoreOK.
    let b = body.as_bytes();
    let mut i = 0;
    let mut saw_digits = false;
    let mut saw_dot = false;
    while let Some(&c) = b.get(i) {
        if c.is_ascii_digit() || c == b'_' {
            saw_digits |= c != b'_';
        } else if c == b'.' && !saw_dot {
            saw_dot = true;
        } else {
            break;
        }
        i += 1;
    }
    if !saw_digits {
        return None;
    }
    if matches!(b.get(i), Some(b'e')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !b.get(i).is_some_and(u8::is_ascii_digit) {
            return None;
        }
        while b.get(i).is_some_and(|c| c.is_ascii_digit() || *c == b'_') {
            i += 1;
        }
    }
    if i != b.len() {
        return None;
    }
    if text.contains('_') && !underscore_ok(text.as_bytes()) {
        return None;
    }
    let clean: String = text.chars().filter(|&c| c != '_').collect();
    let f: f64 = clean.parse().ok()?;
    if f.is_infinite() { None } else { Some(f) }
}

/// resolve(tag, in): the tag a scalar resolves to and its value, or go-yaml's error.
fn resolve(tag: &str, input: &[u8]) -> Result<(String, Resolved), String> {
    if !resolvable_tag(tag) {
        return Ok((tag.to_string(), Resolved::Val(GoVal::Str(input.to_vec()))));
    }
    let (rtag, out) = resolve_inner(tag, input);
    match tag {
        "" | STR_TAG | BINARY_TAG => return Ok((rtag.to_string(), out)),
        t if t == rtag => return Ok((rtag.to_string(), out)),
        FLOAT_TAG if rtag == INT_TAG => {
            if let Resolved::Val(GoVal::Int(i)) = out {
                #[allow(clippy::cast_precision_loss)]
                let f = i as f64;
                return Ok((FLOAT_TAG.to_string(), Resolved::Val(GoVal::Float(f))));
            }
        }
        _ => {}
    }
    Err(format!(
        "cannot decode {} `{}` as a {}",
        short_tag(rtag),
        gofmt::lossy(input),
        short_tag(tag)
    ))
}

fn resolve_inner(tag: &str, input: &[u8]) -> (&'static str, Resolved) {
    let str_out = || (STR_TAG, Resolved::Val(GoVal::Str(input.to_vec())));
    let hint = match input.first() {
        None => b'N',
        Some(&c) => resolve_hint(c),
    };
    if hint == 0 || tag == STR_TAG || tag == BINARY_TAG {
        return str_out();
    }
    if let Some((t, v)) = resolve_map(input) {
        return (t, Resolved::Val(v));
    }
    match hint {
        b'.' => {
            if let Some(f) = parse_float(input) {
                return (FLOAT_TAG, Resolved::Val(GoVal::Float(f)));
            }
        }
        b'D' | b'S' => {
            if (tag.is_empty() || tag == TIMESTAMP_TAG) && parse_timestamp(input) {
                return (TIMESTAMP_TAG, Resolved::Timestamp);
            }
            let plain: Vec<u8> = input.iter().copied().filter(|&c| c != b'_').collect();
            if let Some(i) = parse_int0(&plain) {
                return (INT_TAG, Resolved::Val(GoVal::Int(i)));
            }
            if let Some(u) = parse_uint0(&plain) {
                return (INT_TAG, Resolved::Val(GoVal::Uint(u)));
            }
            if yaml_style_float(&plain)
                && let Some(f) = parse_float(&plain)
            {
                return (FLOAT_TAG, Resolved::Val(GoVal::Float(f)));
            }
            if let Some(rest) = plain.strip_prefix(b"0b") {
                if let Some(i) = parse_int(rest, 2) {
                    return (INT_TAG, Resolved::Val(GoVal::Int(i)));
                }
                if let Some(u) = parse_uint(rest, 2) {
                    return (INT_TAG, Resolved::Val(GoVal::Uint(u)));
                }
            } else if let Some(rest) = plain.strip_prefix(b"-0b") {
                let mut neg = b"-".to_vec();
                neg.extend_from_slice(rest);
                if let Some(i) = parse_int(&neg, 2) {
                    return (INT_TAG, Resolved::Val(GoVal::Int(i)));
                }
            }
        }
        _ => {}
    }
    str_out()
}

// ---- timestamps (time.Parse of resolve.go's four layouts) ----

#[derive(Clone, Copy)]
enum Tok {
    Lit(u8),
    Space,
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Frac,
    Zone,
}

fn parse_timestamp(s: &[u8]) -> bool {
    let mut i = 0;
    while s.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    if i != 4 || i == s.len() || s.get(i) != Some(&b'-') {
        return false;
    }
    use Tok::*;
    let date = [Year, Lit(b'-'), Month, Lit(b'-'), Day];
    let time = [Hour, Lit(b':'), Minute, Lit(b':'), Second, Frac];
    let mut layouts: Vec<Vec<Tok>> = Vec::new();
    for sep in [Lit(b'T'), Lit(b't')] {
        let mut l = date.to_vec();
        l.push(sep);
        l.extend_from_slice(&time);
        l.push(Zone);
        layouts.push(l);
    }
    let mut l = date.to_vec();
    l.push(Space);
    l.extend_from_slice(&time);
    layouts.push(l);
    layouts.push(date.to_vec());
    layouts.iter().any(|l| time_parse(l, s))
}

/// getnum: one or two digits.
fn getnum(v: &[u8]) -> Option<(i64, &[u8])> {
    let d0 = v.first().filter(|c| c.is_ascii_digit())?;
    match v.get(1).filter(|c| c.is_ascii_digit()) {
        None => Some((i64::from(d0 - b'0'), v.get(1..).unwrap_or_default())),
        Some(d1) => Some((
            i64::from(d0 - b'0') * 10 + i64::from(d1 - b'0'),
            v.get(2..).unwrap_or_default(),
        )),
    }
}

fn days_in(month: i64, year: i64) -> i64 {
    match month {
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn time_parse(layout: &[Tok], value: &[u8]) -> bool {
    let mut v = value;
    let (mut year, mut month, mut day) = (0i64, 0i64, 0i64);
    for tok in layout {
        match *tok {
            Tok::Lit(c) => {
                if v.first() != Some(&c) {
                    return false;
                }
                v = v.get(1..).unwrap_or_default();
            }
            Tok::Space => {
                if v.first().is_some_and(|&c| c != b' ') {
                    return false;
                }
                while v.first() == Some(&b' ') {
                    v = v.get(1..).unwrap_or_default();
                }
            }
            Tok::Year => {
                let Some(p) = v.get(..4) else { return false };
                let Some(y) = std::str::from_utf8(p).ok().and_then(atoi) else {
                    return false;
                };
                year = y;
                v = v.get(4..).unwrap_or_default();
            }
            Tok::Month | Tok::Day | Tok::Hour | Tok::Minute | Tok::Second => {
                let Some((n, rest)) = getnum(v) else { return false };
                v = rest;
                let ok = match *tok {
                    Tok::Month => {
                        month = n;
                        (1..=12).contains(&n)
                    }
                    Tok::Day => {
                        day = n;
                        true
                    }
                    Tok::Hour => n < 24,
                    _ => n < 60,
                };
                if !ok {
                    return false;
                }
            }
            Tok::Frac => {
                let ok = v.len() >= 2
                    && matches!(v.first(), Some(b'.' | b','))
                    && v.get(1).is_some_and(u8::is_ascii_digit);
                if ok {
                    let mut i = 0;
                    while v.get(i + 1).is_some_and(u8::is_ascii_digit) {
                        i += 1;
                    }
                    v = v.get(1 + i..).unwrap_or_default();
                }
            }
            Tok::Zone => {
                if v.first() == Some(&b'Z') {
                    v = v.get(1..).unwrap_or_default();
                    continue;
                }
                if v.len() < 6 || v.get(3) != Some(&b':') {
                    return false;
                }
                let hh = v.get(1..3).unwrap_or_default();
                let mm = v.get(4..6).unwrap_or_default();
                let fixed = |s: &[u8]| -> Option<i64> {
                    let (n, rest) = getnum(s)?;
                    if rest.len() + 2 != s.len() { None } else { Some(n) }
                };
                let (Some(hr), Some(mi)) = (fixed(hh), fixed(mm)) else {
                    return false;
                };
                if hr > 24 || mi > 60 || !matches!(v.first(), Some(b'+' | b'-')) {
                    return false;
                }
                v = v.get(6..).unwrap_or_default();
            }
        }
    }
    if !v.is_empty() {
        return false;
    }
    day >= 1 && day <= days_in(month, year)
}

/// time's atoi: an optional sign and digits.
fn atoi(s: &str) -> Option<i64> {
    let (neg, d) = match s.as_bytes().first() {
        Some(b'-') => (true, s.get(1..)?),
        Some(b'+') => (false, s.get(1..)?),
        _ => (false, s),
    };
    if d.is_empty() || !d.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: i64 = d.parse().ok()?;
    Some(if neg { -n } else { n })
}

// ---- the node tree (decode.go's parser) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Document,
    Mapping,
    Sequence,
    Scalar,
    Alias,
}

#[derive(Debug)]
struct Node {
    kind: Kind,
    tag: String,
    value: Vec<u8>,
    implicit: bool,
    children: Vec<usize>,
    alias: Option<usize>,
}

/// An error as go-yaml returns it: `yaml: ` and its text.
fn yaml_err(e: &YamlError) -> String {
    format!("yaml: {}", e.text())
}

struct TreeBuilder<'a> {
    parser: Parser<'a>,
    event: Option<parser::Event>,
    nodes: Vec<Node>,
    anchors: HashMap<Vec<u8>, usize>,
    done_init: bool,
}

impl TreeBuilder<'_> {
    fn next(&mut self) -> Result<&parser::Event, String> {
        if self.event.is_none() {
            let e = self.parser.parse().map_err(|e| yaml_err(&e))?;
            self.event = Some(e);
        }
        self.event.as_ref().ok_or_else(|| "yaml: no event".to_string())
    }

    fn peek(&mut self) -> Result<EventType, String> {
        Ok(self.next()?.typ)
    }

    fn expect(&mut self, want: EventType) -> Result<parser::Event, String> {
        let typ = self.peek()?;
        if typ == EventType::StreamEnd {
            return Err("yaml: attempted to go past the end of stream; corrupted value?".to_string());
        }
        if typ != want {
            return Err(format!(
                "yaml: expected {} event but got {}",
                want.name(),
                typ.name()
            ));
        }
        self.event.take().ok_or_else(|| "yaml: no event".to_string())
    }

    fn add(&mut self, kind: Kind) -> usize {
        self.nodes.push(Node {
            kind,
            tag: String::new(),
            value: Vec::new(),
            implicit: false,
            children: Vec::new(),
            alias: None,
        });
        self.nodes.len() - 1
    }

    /// p.parse(), with the nesting kept on a stack of its own rather than the call
    /// stack: a node, None at the stream's end.
    fn parse(&mut self) -> Result<Option<usize>, String> {
        if !self.done_init {
            self.expect(EventType::StreamStart)?;
            self.done_init = true;
        }
        // Open collections (node, closing event, children) and documents (node).
        enum Open {
            Coll(usize, EventType, Vec<usize>),
            Doc(usize),
        }
        let mut stack: Vec<Open> = Vec::new();
        loop {
            let mut done: Option<Option<usize>> = None;
            if let Some(Open::Coll(_, end, _)) = stack.last()
                && self.peek()? == *end
            {
                let end = *end;
                self.expect(end)?;
                if let Some(Open::Coll(n, _, children)) = stack.pop() {
                    if let Some(node) = self.nodes.get_mut(n) {
                        node.children = children;
                    }
                    done = Some(Some(n));
                }
            }
            if done.is_none() {
                match self.peek()? {
                    EventType::Scalar => {
                        let e = self.expect(EventType::Scalar)?;
                        let n = self.add(Kind::Scalar);
                        if let Some(node) = self.nodes.get_mut(n) {
                            node.value = e.value;
                            node.tag = String::from_utf8_lossy(&e.tag).into_owned();
                            node.implicit = e.implicit;
                        }
                        if let Some(a) = e.anchor {
                            self.anchors.insert(a, n);
                        }
                        done = Some(Some(n));
                    }
                    EventType::Alias => {
                        let e = self.expect(EventType::Alias)?;
                        let name = e.anchor.unwrap_or_default();
                        let Some(&target) = self.anchors.get(&name) else {
                            return Err(format!(
                                "yaml: unknown anchor '{}' referenced",
                                gofmt::lossy(&name)
                            ));
                        };
                        let n = self.add(Kind::Alias);
                        if let Some(node) = self.nodes.get_mut(n) {
                            node.value = name;
                            node.alias = Some(target);
                        }
                        done = Some(Some(n));
                    }
                    t @ (EventType::MappingStart | EventType::SequenceStart) => {
                        let (end, kind) = if t == EventType::MappingStart {
                            (EventType::MappingEnd, Kind::Mapping)
                        } else {
                            (EventType::SequenceEnd, Kind::Sequence)
                        };
                        let n = self.add(kind);
                        let e = self.expect(t)?;
                        if let Some(a) = e.anchor {
                            self.anchors.insert(a, n);
                        }
                        stack.push(Open::Coll(n, end, Vec::new()));
                        continue;
                    }
                    EventType::DocumentStart => {
                        let n = self.add(Kind::Document);
                        self.anchors.clear();
                        self.expect(EventType::DocumentStart)?;
                        stack.push(Open::Doc(n));
                        continue;
                    }
                    EventType::StreamEnd => done = Some(None),
                    other => {
                        return Err(format!(
                            "yaml: attempted to parse unknown event: {}",
                            other.name()
                        ));
                    }
                }
            }
            // Hand the finished node to what is open.
            let mut finished = done.flatten();
            loop {
                match stack.last_mut() {
                    None => return Ok(finished),
                    Some(Open::Coll(_, _, children)) => {
                        children.extend(finished);
                        break;
                    }
                    Some(Open::Doc(n)) => {
                        let n = *n;
                        self.expect(EventType::DocumentEnd)?;
                        if let Some(node) = self.nodes.get_mut(n) {
                            node.children = finished.into_iter().collect();
                        }
                        stack.pop();
                        finished = Some(n);
                    }
                }
            }
        }
    }
}

// ---- decoding into interface{} (decode.go's decoder) ----

struct Decoder<'a> {
    nodes: &'a [Node],
    arena: Arena,
    aliases: HashSet<usize>,
    decode_count: i64,
    alias_count: i64,
    alias_depth: i64,
}

fn allowed_alias_ratio(decode_count: i64) -> f64 {
    const LOW: i64 = 400_000;
    const HIGH: i64 = 4_000_000;
    if decode_count <= LOW {
        0.99
    } else if decode_count >= HIGH {
        0.10
    } else {
        #[allow(clippy::cast_precision_loss)]
        let r = (decode_count - LOW) as f64 / (HIGH - LOW) as f64;
        0.99 - 0.89 * r
    }
}

const WANT_MAP: &str = "yaml: map merge requires map or sequence of maps as the value";

/// The decoder's steps, run from a stack in the order go-yaml's recursion runs them.
enum Task {
    /// d.unmarshal(n, interface{}): its value is pushed onto the results.
    Value(usize),
    /// d.unmarshal(n, map): a merge into the map.
    IntoMap(usize, Id),
    /// The end of an alias's expansion.
    EndAlias(usize),
    /// A sequence's elements from `next` on.
    SeqItems { node: usize, next: usize, seq: Id },
    /// Append the last result to a sequence.
    SeqAppend(Id),
    /// A mapping's key/value pairs from pair `next` on.
    MapPairs { node: usize, next: usize, map: Id },
    /// The last result is a key: check it, then decode its value.
    MapKey { value: usize, map: Id },
    /// The last result is the value of `key`.
    MapInsert { map: Id, key: Id },
    /// A merged sequence's maps, the `next` last first.
    MergeSeq { node: usize, next: usize, map: Id },
    /// A finished collection as the result.
    Done(Id),
}

impl Decoder<'_> {
    fn node(&self, id: usize) -> Result<&Node, String> {
        self.nodes.get(id).ok_or_else(|| "yaml: bad node".to_string())
    }

    fn kind_of(&self, id: Option<usize>) -> Option<Kind> {
        id.and_then(|a| self.nodes.get(a)).map(|t| t.kind)
    }

    fn count(&mut self) -> Result<(), String> {
        self.decode_count += 1;
        if self.alias_depth > 0 {
            self.alias_count += 1;
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = self.alias_count as f64 / self.decode_count as f64;
        if self.alias_count > 100
            && self.decode_count > 1000
            && ratio > allowed_alias_ratio(self.decode_count)
        {
            return Err("yaml: document contains excessive aliasing".to_string());
        }
        Ok(())
    }

    fn enter_alias(&mut self, id: usize) -> Result<usize, String> {
        let n = self.node(id)?;
        if self.aliases.contains(&id) {
            return Err(format!(
                "yaml: anchor '{}' value contains itself",
                gofmt::lossy(&n.value)
            ));
        }
        let target = n.alias.ok_or_else(|| "yaml: bad alias".to_string())?;
        self.aliases.insert(id);
        self.alias_depth += 1;
        Ok(target)
    }

    fn is_merge(&self, id: usize) -> bool {
        self.nodes
            .get(id)
            .is_some_and(|n| n.kind == Kind::Scalar && n.value == b"<<" && (n.implicit || n.tag == MERGE_TAG))
    }

    /// d.unmarshal(root, interface{}).
    fn run(&mut self, root: usize) -> Result<Id, String> {
        let mut tasks = vec![Task::Value(root)];
        let mut results: Vec<Id> = Vec::new();
        let nil = self.arena.add(GoVal::Nil);
        while let Some(t) = tasks.pop() {
            match t {
                Task::Value(n) => {
                    self.count()?;
                    let node = self.node(n)?;
                    match node.kind {
                        Kind::Document => match node.children.first() {
                            Some(&c) if node.children.len() == 1 => tasks.push(Task::Value(c)),
                            _ => results.push(nil),
                        },
                        Kind::Alias => {
                            let target = self.enter_alias(n)?;
                            tasks.push(Task::EndAlias(n));
                            tasks.push(Task::Value(target));
                        }
                        Kind::Scalar => {
                            let v = self.scalar(n)?;
                            results.push(self.arena.add(v));
                        }
                        Kind::Mapping => {
                            let m = self.arena.add(GoVal::Map(GoMap::default()));
                            tasks.push(Task::Done(m));
                            tasks.push(Task::MapPairs {
                                node: n,
                                next: 0,
                                map: m,
                            });
                        }
                        Kind::Sequence => {
                            let s = self.arena.add(GoVal::Seq(Vec::new()));
                            tasks.push(Task::Done(s));
                            tasks.push(Task::SeqItems {
                                node: n,
                                next: 0,
                                seq: s,
                            });
                        }
                    }
                }
                Task::IntoMap(n, m) => {
                    self.count()?;
                    let node = self.node(n)?;
                    match node.kind {
                        Kind::Alias => {
                            let target = self.enter_alias(n)?;
                            tasks.push(Task::EndAlias(n));
                            tasks.push(Task::IntoMap(target, m));
                        }
                        Kind::Mapping => tasks.push(Task::MapPairs {
                            node: n,
                            next: 0,
                            map: m,
                        }),
                        Kind::Document => {
                            if let Some(&c) = node.children.first() {
                                tasks.push(Task::IntoMap(c, m));
                            }
                        }
                        Kind::Scalar | Kind::Sequence => {}
                    }
                }
                Task::EndAlias(n) => {
                    self.alias_depth -= 1;
                    self.aliases.remove(&n);
                }
                Task::SeqItems { node, next, seq } => {
                    if let Some(&c) = self.node(node)?.children.get(next) {
                        tasks.push(Task::SeqItems {
                            node,
                            next: next + 1,
                            seq,
                        });
                        tasks.push(Task::SeqAppend(seq));
                        tasks.push(Task::Value(c));
                    }
                }
                Task::SeqAppend(seq) => {
                    let v = results.pop().unwrap_or(nil);
                    if let Some(GoVal::Seq(items)) = self.arena.vals.get_mut(seq) {
                        items.push(v);
                    }
                }
                Task::MapPairs { node, next, map } => {
                    let children = &self.node(node)?.children;
                    let (Some(&k), Some(&v)) = (children.get(2 * next), children.get(2 * next + 1)) else {
                        continue;
                    };
                    tasks.push(Task::MapPairs {
                        node,
                        next: next + 1,
                        map,
                    });
                    if self.is_merge(k) {
                        self.merge(v, map, &mut tasks)?;
                    } else {
                        tasks.push(Task::MapKey { value: v, map });
                        tasks.push(Task::Value(k));
                    }
                }
                Task::MapKey { value, map } => {
                    let key = results.pop().unwrap_or(nil);
                    if matches!(self.arena.get(key), GoVal::Map(_) | GoVal::Seq(_)) {
                        return Err(format!(
                            "yaml: invalid map key: {}",
                            sharp_v(&self.arena, key, true)
                        ));
                    }
                    tasks.push(Task::MapInsert { map, key });
                    tasks.push(Task::Value(value));
                }
                Task::MapInsert { map, key } => {
                    let v = results.pop().unwrap_or(nil);
                    let id = key_id(self.arena.get(key));
                    if let Some(GoVal::Map(m)) = self.arena.vals.get_mut(map) {
                        m.set(id, key, v);
                    }
                }
                Task::MergeSeq { node, next, map } => {
                    let Some(j) = next.checked_sub(1) else { continue };
                    let Some(&c) = self.node(node)?.children.get(j) else {
                        continue;
                    };
                    let cn = self.node(c)?;
                    if cn.kind == Kind::Alias {
                        if self.kind_of(cn.alias).is_some_and(|k| k != Kind::Mapping) {
                            return Err(WANT_MAP.to_string());
                        }
                    } else if cn.kind != Kind::Mapping {
                        return Err(WANT_MAP.to_string());
                    }
                    tasks.push(Task::MergeSeq { node, next: j, map });
                    tasks.push(Task::IntoMap(c, map));
                }
                Task::Done(id) => results.push(id),
            }
        }
        Ok(results.pop().unwrap_or(nil))
    }

    /// d.merge(n, out).
    fn merge(&mut self, id: usize, map: Id, tasks: &mut Vec<Task>) -> Result<(), String> {
        let n = self.node(id)?;
        match n.kind {
            Kind::Mapping => tasks.push(Task::IntoMap(id, map)),
            Kind::Alias => {
                if self.kind_of(n.alias).is_some_and(|k| k != Kind::Mapping) {
                    return Err(WANT_MAP.to_string());
                }
                tasks.push(Task::IntoMap(id, map));
            }
            Kind::Sequence => tasks.push(Task::MergeSeq {
                node: id,
                next: n.children.len(),
                map,
            }),
            _ => return Err(WANT_MAP.to_string()),
        }
        Ok(())
    }

    #[inline(never)]
    fn scalar(&mut self, id: usize) -> Result<GoVal, String> {
        let n = self.node(id)?;
        let resolved = if n.tag.is_empty() && !n.implicit {
            Resolved::Val(GoVal::Str(n.value.clone()))
        } else {
            let (tag, mut resolved) = resolve(&n.tag, &n.value).map_err(|e| format!("yaml: {e}"))?;
            if tag == BINARY_TAG {
                let Resolved::Val(GoVal::Str(s)) = &resolved else {
                    return Err("yaml: !!binary value contains invalid base64 data".to_string());
                };
                let data = super::base64_std_decode(s)
                    .map_err(|_| "yaml: !!binary value contains invalid base64 data".to_string())?;
                resolved = Resolved::Val(GoVal::Str(data));
            }
            resolved
        };
        Ok(match resolved {
            Resolved::Timestamp => GoVal::Str(n.value.clone()),
            Resolved::Val(v) => v,
        })
    }
}

/// yaml.Unmarshal of `input` into an interface{}: the first document's value (nil when
/// there is none) and the arena holding it.
pub fn decode(input: &[u8]) -> Result<(Arena, Id), String> {
    let input = if input.is_empty() { b"\n".as_slice() } else { input };
    let mut tb = TreeBuilder {
        parser: Parser::new(input),
        event: None,
        nodes: Vec::new(),
        anchors: HashMap::new(),
        done_init: false,
    };
    let Some(root) = tb.parse()? else {
        let mut arena = Arena::default();
        let nil = arena.add(GoVal::Nil);
        return Ok((arena, nil));
    };
    let mut d = Decoder {
        nodes: &tb.nodes,
        arena: Arena::default(),
        aliases: HashSet::new(),
        decode_count: 0,
        alias_count: 0,
        alias_depth: 0,
    };
    let root = d.run(root)?;
    Ok((d.arena, root))
}

// ---- fmt's %#v of the decoded values (for go-yaml's and sigs.k8s.io's errors) ----

fn sharp_v(a: &Arena, id: Id, top: bool) -> String {
    enum P {
        Text(&'static str),
        Val(Id, bool),
    }
    let mut out = String::new();
    let mut stack = vec![P::Val(id, top)];
    while let Some(p) = stack.pop() {
        let (id, top) = match p {
            P::Text(t) => {
                out.push_str(t);
                continue;
            }
            P::Val(id, top) => (id, top),
        };
        match a.get(id) {
            GoVal::Nil => out.push_str(if top { "<nil>" } else { "interface {}(nil)" }),
            GoVal::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            GoVal::Int(i) => out.push_str(&i.to_string()),
            GoVal::Uint(u) => out.push_str(&format!("0x{u:x}")),
            GoVal::Float(f) => out.push_str(&gofmt::format_float(*f, b'g', false)),
            GoVal::Str(s) => out.push_str(&gofmt::quote_bytes(s)),
            GoVal::Seq(items) => {
                out.push_str("[]interface {}{");
                stack.push(P::Text("}"));
                for (i, &it) in items.iter().enumerate().rev() {
                    stack.push(P::Val(it, false));
                    if i > 0 {
                        stack.push(P::Text(", "));
                    }
                }
            }
            GoVal::Map(m) => {
                let mut entries = m.entries.clone();
                entries.sort_by(|x, y| fmtsort(a.get(x.0), a.get(y.0)));
                out.push_str("map[interface {}]interface {}{");
                stack.push(P::Text("}"));
                for (i, &(k, v)) in entries.iter().enumerate().rev() {
                    stack.push(P::Val(v, false));
                    stack.push(P::Text(":"));
                    stack.push(P::Val(k, false));
                    if i > 0 {
                        stack.push(P::Text(", "));
                    }
                }
            }
        }
    }
    out
}

/// internal/fmtsort's order for keys of one type (keys of mixed types: by type).
fn fmtsort(a: &GoVal, b: &GoVal) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rank = |v: &GoVal| match v {
        GoVal::Nil => 0,
        GoVal::Bool(_) => 1,
        GoVal::Int(_) => 2,
        GoVal::Uint(_) => 3,
        GoVal::Float(_) => 4,
        GoVal::Str(_) => 5,
        GoVal::Seq(_) => 6,
        GoVal::Map(_) => 7,
    };
    match (a, b) {
        (GoVal::Bool(x), GoVal::Bool(y)) => x.cmp(y),
        (GoVal::Int(x), GoVal::Int(y)) => x.cmp(y),
        (GoVal::Uint(x), GoVal::Uint(y)) => x.cmp(y),
        (GoVal::Float(x), GoVal::Float(y)) => {
            if x.is_nan() {
                if y.is_nan() {
                    Ordering::Equal
                } else {
                    Ordering::Less
                }
            } else if y.is_nan() {
                Ordering::Greater
            } else {
                x.partial_cmp(y).unwrap_or(Ordering::Equal)
            }
        }
        (GoVal::Str(x), GoVal::Str(y)) => x.cmp(y),
        _ => rank(a).cmp(&rank(b)),
    }
}

// ---- sigs.k8s.io/yaml's YAMLToJSON ----

fn go_type(v: &GoVal) -> &'static str {
    match v {
        GoVal::Nil => "%!s(<nil>)",
        GoVal::Bool(_) => "bool",
        GoVal::Int(_) => "int",
        GoVal::Uint(_) => "uint64",
        GoVal::Float(_) => "float64",
        GoVal::Str(_) => "string",
        GoVal::Seq(_) => "[]interface {}",
        GoVal::Map(_) => "map[interface {}]interface {}",
    }
}

/// convertToJSONableObject's key string, or its error for keys it cannot convert.
fn json_key(a: &Arena, k: Id, v: Id) -> Result<Vec<u8>, String> {
    let key = a.get(k);
    Ok(match key {
        GoVal::Str(s) => s.clone(),
        GoVal::Int(i) => i.to_string().into_bytes(),
        GoVal::Float(f) => {
            let s = gofmt::format_float(*f, b'g', true);
            match s.as_str() {
                "+Inf" => ".inf".into(),
                "-Inf" => "-.inf".into(),
                "NaN" => ".nan".into(),
                _ => s.into_bytes(),
            }
        }
        GoVal::Bool(b) => b.to_string().into_bytes(),
        _ => {
            return Err(format!(
                "unsupported map key of type: {}, key: {}, value: {}",
                go_type(key),
                sharp_v(a, k, true),
                sharp_v(a, v, true)
            ));
        }
    })
}

/// convertToJSONableObject's checks: every map key, each before its value's.
fn check_keys(a: &Arena, root: Id) -> Result<(), String> {
    enum T {
        Val(Id),
        Key(Id, Id),
    }
    let mut stack = vec![T::Val(root)];
    while let Some(t) = stack.pop() {
        match t {
            T::Key(k, v) => {
                json_key(a, k, v)?;
            }
            T::Val(id) => match a.get(id) {
                GoVal::Seq(items) => stack.extend(items.iter().rev().map(|&i| T::Val(i))),
                GoVal::Map(m) => {
                    for &(k, v) in m.entries.iter().rev() {
                        stack.push(T::Val(v));
                        stack.push(T::Key(k, v));
                    }
                }
                _ => {}
            },
        }
    }
    Ok(())
}

/// encoding/json's float64, read back as a number; its error for NaN and infinities.
fn json_number(f: f64) -> Result<Value, String> {
    if !f.is_finite() {
        return Err(format!(
            "json: unsupported value: {}",
            gofmt::format_float(f, b'g', false)
        ));
    }
    Ok(Value::Number(Number(gofmt::json_float(f).into())))
}

/// encoding/json's marshal of the converted value (map keys in byte order, the first
/// NaN or infinity met failing it), read back as OPA reads JSON.
fn to_value(a: &Arena, root: Id) -> Result<Value, String> {
    enum Frame<'a> {
        Seq(&'a [Id], usize, Vec<Value>),
        Map(Vec<(Vec<u8>, Id)>, usize, BTreeMap<Value, Value>),
    }
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut next = Some(root);
    loop {
        let mut done: Option<Value> = None;
        if let Some(id) = next.take() {
            match a.get(id) {
                GoVal::Seq(items) => stack.push(Frame::Seq(items, 0, Vec::with_capacity(items.len()))),
                GoVal::Map(m) => {
                    let mut sorted = BTreeMap::new();
                    for &(k, v) in &m.entries {
                        sorted.insert(json_key(a, k, v)?, v);
                    }
                    stack.push(Frame::Map(sorted.into_iter().collect(), 0, BTreeMap::new()));
                }
                GoVal::Nil => done = Some(Value::Null),
                GoVal::Bool(b) => done = Some(Value::Bool(*b)),
                GoVal::Int(i) => done = Some(Value::Number(Number(i.to_string().into()))),
                GoVal::Uint(u) => done = Some(Value::Number(Number(u.to_string().into()))),
                GoVal::Float(f) => done = Some(json_number(*f)?),
                GoVal::Str(s) => done = Some(Value::string(gofmt::lossy(s))),
            }
        }
        loop {
            let Some(top) = stack.last_mut() else {
                return done.ok_or_else(|| "json: no value".to_string());
            };
            match top {
                Frame::Seq(items, i, acc) => {
                    if let Some(v) = done.take() {
                        acc.push(v);
                    }
                    if let Some(&c) = items.get(*i) {
                        *i += 1;
                        next = Some(c);
                        break;
                    }
                }
                Frame::Map(entries, i, acc) => {
                    if let Some(v) = done.take()
                        && let Some((k, _)) = i.checked_sub(1).and_then(|j| entries.get(j))
                    {
                        acc.insert(Value::string(gofmt::lossy(k)), v);
                    }
                    if let Some(&(_, c)) = entries.get(*i) {
                        *i += 1;
                        next = Some(c);
                        break;
                    }
                }
            }
            done = Some(match stack.pop() {
                Some(Frame::Seq(_, _, acc)) => Value::array(acc),
                Some(Frame::Map(_, _, acc)) => Value::object(acc),
                None => Value::Null,
            });
        }
    }
}

/// yaml.YAMLToJSON, read back as OPA reads JSON (`yaml.unmarshal`).
pub fn yaml_to_value(input: &[u8]) -> Result<Value, String> {
    let (arena, root) = decode(input)?;
    check_keys(&arena, root)?;
    to_value(&arena, root)
}

/// yaml.Marshal of a value's JSON (`yaml.marshal`).
pub fn marshal(json: &str) -> Result<String, String> {
    let (arena, root) = decode(json.as_bytes())?;
    emitter::encode(&arena, root)
}

/// Whether a string resolves to other than !!str (or reads as a base-60 float), so that
/// go-yaml quotes it.
pub fn needs_quotes(s: &[u8]) -> bool {
    let tag = match resolve("", s) {
        Ok((tag, _)) => tag,
        Err(_) => return true,
    };
    tag != STR_TAG || is_base60_float(s)
}

/// isBase60Float: `^[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+(?:\.[0-9_]*)?$`.
fn is_base60_float(s: &[u8]) -> bool {
    let Some(&c) = s.first() else { return false };
    if !(c == b'+' || c == b'-' || c.is_ascii_digit()) || !s.contains(&b':') {
        return false;
    }
    let at = |i: usize| s.get(i).copied();
    let mut i = 0;
    if matches!(at(i), Some(b'+' | b'-')) {
        i += 1;
    }
    if !at(i).is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    i += 1;
    while at(i).is_some_and(|c| c.is_ascii_digit() || c == b'_') {
        i += 1;
    }
    let mut groups = 0;
    while at(i) == Some(b':') {
        i += 1;
        // [0-5]?[0-9]: one digit, or a 0-5 then a digit.
        match (at(i), at(i + 1)) {
            (Some(a), Some(b)) if (b'0'..=b'5').contains(&a) && b.is_ascii_digit() => i += 2,
            (Some(a), _) if a.is_ascii_digit() => i += 1,
            _ => return false,
        }
        groups += 1;
    }
    if groups == 0 {
        return false;
    }
    if at(i) == Some(b'.') {
        i += 1;
        while at(i).is_some_and(|c| c.is_ascii_digit() || c == b'_') {
            i += 1;
        }
    }
    i == s.len()
}

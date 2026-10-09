//! OPA's regex and glob builtins (topdown/regex.go, regex_template.go, glob.go):
//! Go's regexp (its syntax, `syntax`, and the matching, split, find and replace loops
//! of regexp.go), gobwas/glob v0.2.3 (`glob`) and yashtewari/glob-intersection v0.2.0
//! (`gintersect`), the versions OPA v1.14.1 pins.

mod gintersect;
mod glob;
mod syntax;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::{Builtin, BuiltinError, Context, arg, int_operand, string_operand};
use crate::value::Value;

pub fn lookup(name: &str) -> Option<Builtin> {
    Some(match name {
        "re_match" | "regex.match" => regex_match,
        "regex.find_all_string_submatch_n" => find_all_string_submatch_n,
        "regex.find_n" => find_n,
        "regex.globs_match" => globs_match,
        "regex.is_valid" => is_valid,
        "regex.replace" => replace,
        "regex.split" => split,
        "regex.template_match" => template_match,
        "glob.match" => glob_match,
        "glob.quote_meta" => glob_quote_meta,
        _ => return None,
    })
}

/// A compiled Go regexp.
#[derive(Debug)]
struct Regexp {
    re: regex::Regex,
    /// regexp.SubexpNames: index 0 is the whole match.
    names: Vec<String>,
    /// Whether the expression is empty (`len(re.expr) == 0`).
    empty_expr: bool,
}

/// What the `regex` crate needs to hold any pattern Go accepts: Go limits a
/// program to 3.3M instructions and a tree to 1000 levels.
const SIZE_LIMIT: usize = 1 << 30;
const NEST_LIMIT: u32 = 10_000;

/// regexp.Compile.
fn compile(pattern: &str) -> Result<Regexp, String> {
    let parsed = syntax::parse(pattern)?;
    let re = regex::RegexBuilder::new(&parsed.pattern())
        .size_limit(SIZE_LIMIT)
        .nest_limit(NEST_LIMIT)
        .build()
        .map_err(|e| format!("error compiling regexp: {e}"))?;
    Ok(Regexp {
        re,
        names: parsed.names,
        empty_expr: pattern.is_empty(),
    })
}

const CACHE_MAX: usize = 100;

thread_local! {
    /// OPA's regexpCache: compiled patterns by their text, shared by regex.match and
    /// the others with regex.template_match, and at most 100 of them, one dropped
    /// to make room.
    static CACHE: RefCell<HashMap<String, Rc<Regexp>>> = RefCell::new(HashMap::new());
}

fn cached(pattern: &str) -> Option<Rc<Regexp>> {
    CACHE.with(|c| c.try_borrow().ok().and_then(|c| c.get(pattern).cloned()))
}

fn cache(pattern: &str, re: &Rc<Regexp>, evict: bool) {
    CACHE.with(|c| {
        if let Ok(mut c) = c.try_borrow_mut() {
            if evict
                && c.len() >= CACHE_MAX
                && let Some(k) = c.keys().next().cloned()
            {
                c.remove(&k);
            }
            c.insert(pattern.to_string(), Rc::clone(re));
        }
    });
}

/// getRegexp.
fn get_regexp(pattern: &str) -> Result<Rc<Regexp>, BuiltinError> {
    if let Some(re) = cached(pattern) {
        return Ok(re);
    }
    let re = Rc::new(compile(pattern).map_err(BuiltinError::Other)?);
    cache(pattern, &re, true);
    Ok(re)
}

fn strings(v: Vec<&str>) -> Value {
    Value::array(v.into_iter().map(Value::string).collect())
}

fn is_valid(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let valid = match arg(args, 0)?.as_str() {
        Some(s) => syntax::parse(s).is_ok(),
        None => false,
    };
    Ok(Some(Value::Bool(valid)))
}

fn regex_match(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let re = get_regexp(pattern)?;
    Ok(Some(Value::Bool(re.re.is_match(s))))
}

/// The width of the rune at `pos`, 0 at the end (utf8.DecodeRuneInString).
fn width_at(s: &str, pos: usize) -> usize {
    s.get(pos..)
        .and_then(|t| t.chars().next())
        .map_or(0, char::len_utf8)
}

/// regexp's allMatches: the matches' group positions, at most n of them, an empty
/// match right after a match left out.
fn all_matches(re: &Regexp, s: &str, n: i64) -> Vec<Vec<Option<(usize, usize)>>> {
    let n = if n < 0 {
        i64::try_from(s.len()).unwrap_or(i64::MAX).saturating_add(1)
    } else {
        n
    };
    let mut out = Vec::new();
    let (mut pos, mut i, mut prev_end): (usize, i64, Option<usize>) = (0, 0, None);
    while i < n && pos <= s.len() {
        let Some(caps) = re.re.captures_at(s, pos) else {
            break;
        };
        let Some(m) = caps.get(0) else { break };
        let mut accept = true;
        if m.end() == pos {
            if Some(m.start()) == prev_end {
                accept = false;
            }
            let width = width_at(s, pos);
            pos = if width > 0 { pos + width } else { s.len() + 1 };
        } else {
            pos = m.end();
        }
        prev_end = Some(m.end());
        if accept {
            out.push(caps.iter().map(|g| g.map(|g| (g.start(), g.end()))).collect());
            i += 1;
        }
    }
    out
}

fn slice(s: &str, g: Option<(usize, usize)>) -> &str {
    g.and_then(|(a, b)| s.get(a..b)).unwrap_or("")
}

fn find_n(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let n = int_operand(arg(args, 2)?, 3)?;
    let re = get_regexp(pattern)?;
    let found = all_matches(&re, s, n);
    Ok(Some(strings(
        found
            .iter()
            .map(|m| slice(s, m.first().copied().flatten()))
            .collect(),
    )))
}

fn find_all_string_submatch_n(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let n = int_operand(arg(args, 2)?, 3)?;
    let re = get_regexp(pattern)?;
    let found = all_matches(&re, s, n);
    Ok(Some(Value::array(
        found
            .iter()
            .map(|m| strings(m.iter().map(|g| slice(s, *g)).collect()))
            .collect(),
    )))
}

fn split(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let re = get_regexp(pattern)?;
    // regexp.Split(s, -1).
    if !re.empty_expr && s.is_empty() {
        return Ok(Some(strings(vec![""])));
    }
    let mut out = Vec::new();
    let (mut beg, mut end) = (0, 0);
    for m in all_matches(&re, s, -1) {
        let Some(Some((start, stop))) = m.first().copied() else {
            continue;
        };
        end = start;
        if stop != 0 {
            out.push(s.get(beg..end).unwrap_or(""));
        }
        beg = stop;
    }
    if end != s.len() {
        out.push(s.get(beg..).unwrap_or(""));
    }
    Ok(Some(strings(out)))
}

fn replace(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let base = string_operand(arg(args, 0)?, 1)?;
    let pattern = string_operand(arg(args, 1)?, 2)?;
    let value = string_operand(arg(args, 2)?, 3)?;
    let re = get_regexp(pattern)?;
    let res = replace_all_string(&re, base, value);
    if res == base {
        return Ok(Some(arg(args, 0)?.clone()));
    }
    Ok(Some(Value::string(res)))
}

/// regexp.ReplaceAllString: replaceAll with expand.
fn replace_all_string(re: &Regexp, src: &str, repl: &str) -> String {
    let mut buf = String::new();
    let (mut last_end, mut search) = (0, 0);
    while search <= src.len() {
        let Some(caps) = re.re.captures_at(src, search) else {
            break;
        };
        let Some(m) = caps.get(0) else { break };
        buf.push_str(src.get(last_end..m.start()).unwrap_or(""));
        if m.end() > last_end || m.start() == 0 {
            let groups: Vec<Option<(usize, usize)>> =
                caps.iter().map(|g| g.map(|g| (g.start(), g.end()))).collect();
            expand(&mut buf, re, repl, src, &groups);
        }
        last_end = m.end();
        let width = width_at(src, search);
        if search + width > m.end() {
            search += width;
        } else if search + 1 > m.end() {
            search += 1;
        } else {
            search = m.end();
        }
    }
    buf.push_str(src.get(last_end..).unwrap_or(""));
    buf
}

/// Whether Go's unicode.IsLetter or unicode.IsDigit holds for c.
fn letter_or_digit(c: char) -> bool {
    use std::sync::OnceLock;
    static RE: OnceLock<Option<regex::Regex>> = OnceLock::new();
    if c.is_ascii() {
        return c.is_ascii_alphanumeric();
    }
    let mut buf = [0u8; 4];
    RE.get_or_init(|| regex::Regex::new(r"\A[\p{L}\p{Nd}]\z").ok())
        .as_ref()
        .is_some_and(|re| re.is_match(c.encode_utf8(&mut buf)))
}

/// regexp's extract: a `$name`, `${name}` or `$1` at the start of `s`, as the name,
/// its number (if it is one) and what follows.
fn extract(s: &str) -> Option<(&str, Option<usize>, &str)> {
    let (brace, body) = match s.strip_prefix('{') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let end = body
        .char_indices()
        .find(|&(_, c)| !(c == '_' || letter_or_digit(c)))
        .map_or(body.len(), |(i, _)| i);
    if end == 0 {
        return None;
    }
    let name = body.get(..end)?;
    let mut rest = body.get(end..)?;
    if brace {
        rest = rest.strip_prefix('}')?;
    }
    let mut num: Option<usize> = Some(0);
    for d in name.bytes() {
        num = match num {
            Some(n) if d.is_ascii_digit() && n < 100_000_000 => Some(n * 10 + usize::from(d - b'0')),
            _ => None,
        };
        if num.is_none() {
            break;
        }
    }
    if name.len() > 1 && name.starts_with('0') {
        num = None;
    }
    Some((name, num, rest))
}

/// regexp's expand: the template with `$` references to the match's groups.
fn expand(dst: &mut String, re: &Regexp, template: &str, src: &str, groups: &[Option<(usize, usize)>]) {
    let mut t = template;
    while let Some((before, after)) = t.split_once('$') {
        dst.push_str(before);
        t = after;
        if let Some(rest) = t.strip_prefix('$') {
            dst.push('$');
            t = rest;
            continue;
        }
        let Some((name, num, rest)) = extract(t) else {
            dst.push('$');
            continue;
        };
        t = rest;
        match num {
            Some(n) => {
                if let Some(Some(g)) = groups.get(n) {
                    dst.push_str(slice(src, Some(*g)));
                }
            }
            None => {
                for (i, n) in re.names.iter().enumerate() {
                    if n == name
                        && let Some(Some(g)) = groups.get(i)
                    {
                        dst.push_str(slice(src, Some(*g)));
                        break;
                    }
                }
            }
        }
    }
    dst.push_str(t);
}

fn globs_match(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let a = string_operand(arg(args, 0)?, 1)?;
    let b = string_operand(arg(args, 1)?, 2)?;
    let ne = gintersect::non_empty(a, b).map_err(BuiltinError::Other)?;
    Ok(Some(Value::Bool(ne)))
}

/// regexp.QuoteMeta.
fn quote_meta(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// delimiterIndices: the byte spans of the outermost delimited parts.
fn delimiter_indices(s: &str, start: u8, end: u8) -> Result<Vec<(usize, usize)>, String> {
    let unbalanced = || {
        let mut q = String::new();
        crate::goquote::quote(&mut q, s);
        format!("unbalanced braces in {q}")
    };
    let (mut level, mut idx) = (0i64, 0);
    let mut out = Vec::new();
    for (i, &c) in s.as_bytes().iter().enumerate() {
        if c == start {
            level += 1;
            if level == 1 {
                idx = i;
            }
        } else if c == end {
            level -= 1;
            if level == 0 {
                out.push((idx, i + 1));
            } else if level < 0 {
                return Err(unbalanced());
            }
        }
    }
    if level != 0 {
        return Err(unbalanced());
    }
    Ok(out)
}

/// compileRegexTemplate.
fn compile_template(tpl: &str, start: u8, end: u8) -> Result<Regexp, String> {
    let idxs = delimiter_indices(tpl, start, end)?;
    let mut pattern = String::from("^");
    let mut last = 0;
    for &(a, b) in &idxs {
        let raw = tpl.get(last..a).unwrap_or("");
        last = b;
        let patt = tpl.get(a + 1..b.saturating_sub(1)).unwrap_or("");
        pattern.push_str(&quote_meta(raw));
        pattern.push('(');
        pattern.push_str(patt);
        pattern.push(')');
        compile(&format!("^{patt}$"))?;
    }
    pattern.push_str(&quote_meta(tpl.get(last..).unwrap_or("")));
    pattern.push('$');
    compile(&pattern)
}

fn template_match(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let start = string_operand(arg(args, 2)?, 3)?;
    let end = string_operand(arg(args, 3)?, 4)?;
    let (&[ds], end_len) = (start.as_bytes(), end.len()) else {
        return Err(BuiltinError::Other(format!(
            "start delimiter has to be exactly one character long but is {} long",
            start.len()
        )));
    };
    let &[de] = end.as_bytes() else {
        // OPA reports the start delimiter's length here.
        let _ = end_len;
        return Err(BuiltinError::Other(format!(
            "end delimiter has to be exactly one character long but is {} long",
            start.len()
        )));
    };
    // getRegexpTemplate: the cache is keyed by the pattern alone, without the
    // delimiters, and shared with getRegexp.
    let re = match cached(pattern) {
        Some(re) => re,
        None => {
            let re = Rc::new(compile_template(pattern, ds, de).map_err(BuiltinError::Other)?);
            cache(pattern, &re, false);
            re
        }
    };
    Ok(Some(Value::Bool(re.re.is_match(s))))
}

fn glob_match(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let delims = arg(args, 1)?;
    let delimiters: Vec<char> = match delims {
        Value::Null => Vec::new(),
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a.iter() {
                let Some(s) = v.as_str() else {
                    return Err(BuiltinError::operand(
                        2,
                        format!(
                            "must be array of strings but got array containing {}",
                            v.type_name()
                        ),
                    ));
                };
                let mut cs = s.chars();
                match (cs.next(), cs.next()) {
                    (Some(c), None) => out.push(c),
                    _ => {
                        return Err(BuiltinError::operand(
                            2,
                            "must be array of runes but got array containing string",
                        ));
                    }
                }
            }
            if out.is_empty() { vec!['.'] } else { out }
        }
        v => return Err(BuiltinError::operand_type(2, v, &["array", "null"])),
    };
    let s = string_operand(arg(args, 2)?, 3)?;
    let g = glob::compile(pattern, &delimiters).map_err(BuiltinError::Other)?;
    let m = g.matches(s.as_bytes()).map_err(BuiltinError::Other)?;
    Ok(Some(Value::Bool(m)))
}

fn glob_quote_meta(_: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    Ok(Some(Value::string(glob::quote_meta(pattern))))
}

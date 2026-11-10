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

/// OPA's `regexCacheMaxSize` (OPA v1.14.1 v1/topdown/regex.go:20).
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
fn get_regexp(_ctx: &mut Context, pattern: &str) -> Result<Rc<Regexp>, BuiltinError> {
    #[cfg(test)]
    if let Some(result) = _ctx
        .regex_experiment
        .compiled(experiment::Kind::Ordinary, pattern)
    {
        return result;
    }
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

fn regex_match(ctx: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let re = get_regexp(ctx, pattern)?;
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

fn find_n(ctx: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let n = int_operand(arg(args, 2)?, 3)?;
    let re = get_regexp(ctx, pattern)?;
    let found = all_matches(&re, s, n);
    Ok(Some(strings(
        found
            .iter()
            .map(|m| slice(s, m.first().copied().flatten()))
            .collect(),
    )))
}

fn find_all_string_submatch_n(ctx: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let n = int_operand(arg(args, 2)?, 3)?;
    let re = get_regexp(ctx, pattern)?;
    let found = all_matches(&re, s, n);
    Ok(Some(Value::array(
        found
            .iter()
            .map(|m| strings(m.iter().map(|g| slice(s, *g)).collect()))
            .collect(),
    )))
}

fn split(ctx: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let re = get_regexp(ctx, pattern)?;
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

fn replace(ctx: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let base = string_operand(arg(args, 0)?, 1)?;
    let pattern = string_operand(arg(args, 1)?, 2)?;
    let value = string_operand(arg(args, 2)?, 3)?;
    let re = get_regexp(ctx, pattern)?;
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

fn template_match(_ctx: &mut Context, args: &[Value]) -> Result<Option<Value>, BuiltinError> {
    let pattern = string_operand(arg(args, 0)?, 1)?;
    let s = string_operand(arg(args, 1)?, 2)?;
    let start = string_operand(arg(args, 2)?, 3)?;
    let end = string_operand(arg(args, 3)?, 4)?;
    let &[ds] = start.as_bytes() else {
        return Err(BuiltinError::Other(format!(
            "start delimiter has to be exactly one character long but is {} long",
            start.len()
        )));
    };
    // OPA reports the start delimiter's length for the end delimiter too.
    let &[de] = end.as_bytes() else {
        return Err(BuiltinError::Other(format!(
            "end delimiter has to be exactly one character long but is {} long",
            start.len()
        )));
    };
    #[cfg(test)]
    if let Some(result) = _ctx
        .regex_experiment
        .compiled(experiment::Kind::Template(ds, de), pattern)
    {
        let re = result?;
        return Ok(Some(Value::Bool(re.re.is_match(s))));
    }
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

/// Test-only candidate: a program owns a fixed set of compilation identities taken
/// from its AST. Dynamic misses compile without inserting any retained history.
#[cfg(test)]
pub(crate) mod experiment {
    use super::*;
    use crate::ast::{Body, Expr, ExprTerms, Head, Module, Rule, TemplatePart, Term, TermValue};
    use std::collections::BTreeMap;
    use std::rc::Weak;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub(crate) enum Kind {
        Ordinary,
        Template(u8, u8),
    }
    #[derive(Debug)]
    enum State {
        Uncompiled,
        Ready(Rc<Regexp>),
        Failed(BuiltinError),
    }
    #[derive(Debug, Default)]
    pub(crate) struct Pool {
        entries: RefCell<BTreeMap<Kind, BTreeMap<Rc<str>, State>>>,
    }
    #[derive(Debug, Default)]
    pub(crate) enum Owner {
        #[default]
        Current,
        Uncached,
        Program(Rc<Pool>),
    }
    #[derive(Debug, Default)]
    pub(crate) enum Binding {
        #[default]
        Current,
        Uncached,
        Program(Weak<Pool>),
    }
    impl Owner {
        pub(crate) fn binding(&self) -> Binding {
            match self {
                Self::Current => Binding::Current,
                Self::Uncached => Binding::Uncached,
                Self::Program(p) => Binding::Program(Rc::downgrade(p)),
            }
        }
    }
    fn fresh(kind: Kind, pattern: &str) -> Result<Rc<Regexp>, BuiltinError> {
        match kind {
            Kind::Ordinary => compile(pattern),
            Kind::Template(start, end) => compile_template(pattern, start, end),
        }
        .map(Rc::new)
        .map_err(BuiltinError::Other)
    }
    impl Binding {
        pub(super) fn compiled(&self, kind: Kind, pattern: &str) -> Option<Result<Rc<Regexp>, BuiltinError>> {
            match self {
                Self::Current => None,
                Self::Uncached => Some(fresh(kind, pattern)),
                Self::Program(owner) => Some(match owner.upgrade() {
                    Some(pool) => pool.compiled(kind, pattern),
                    None => Err(BuiltinError::Other("experimental compilation owner ended".into())),
                }),
            }
        }
    }
    impl Pool {
        fn compiled(&self, kind: Kind, pattern: &str) -> Result<Rc<Regexp>, BuiltinError> {
            let mut entries = self
                .entries
                .try_borrow_mut()
                .map_err(|_| BuiltinError::Other("experimental compilation pool is borrowed".into()))?;
            let Some(state) = entries
                .get_mut(&kind)
                .and_then(|patterns| patterns.get_mut(pattern))
            else {
                return fresh(kind, pattern);
            };
            if matches!(state, State::Uncompiled) {
                *state = match fresh(kind, pattern) {
                    Ok(re) => State::Ready(re),
                    Err(error) => State::Failed(error),
                };
            }
            match state {
                State::Ready(re) => Ok(Rc::clone(re)),
                State::Failed(error) => Err(error.clone()),
                State::Uncompiled => Err(BuiltinError::Other(
                    "experimental compilation did not transition".into(),
                )),
            }
        }
        pub(crate) fn counts(&self) -> (usize, usize, usize) {
            let entries = self.entries.borrow();
            let mut count = (0, 0, 0);
            for state in entries.values().flat_map(BTreeMap::values) {
                count.0 += 1;
                match state {
                    State::Ready(_) => count.1 += 1,
                    State::Failed(_) => count.2 += 1,
                    State::Uncompiled => {}
                }
            }
            count
        }
        pub(crate) fn from_modules<'a>(modules: impl Iterator<Item = &'a Module>) -> Rc<Self> {
            enum Node<'a> {
                Rule(&'a Rule),
                Head(&'a Head),
                Body(&'a Body),
                Expr(&'a Expr),
                Term(&'a Term),
            }
            let mut work = Vec::new();
            for module in modules {
                for rule in &module.rules {
                    work.push(Node::Rule(rule));
                }
            }
            let mut entries: BTreeMap<Kind, BTreeMap<Rc<str>, State>> = BTreeMap::new();
            let mut call = |terms: &[Term]| {
                let Some(op) = terms.first().and_then(Term::as_ref) else {
                    return;
                };
                let name = crate::compile::text_of_ref(op);
                let (kind, at) = match name.as_str() {
                    "regex.template_match" => {
                        let (Some(TermValue::String(start)), Some(TermValue::String(end))) =
                            (terms.get(3).map(|t| &t.value), terms.get(4).map(|t| &t.value))
                        else {
                            return;
                        };
                        let (&[start], &[end]) = (start.as_bytes(), end.as_bytes()) else {
                            return;
                        };
                        (Kind::Template(start, end), 1)
                    }
                    "regex.replace" => (Kind::Ordinary, 2),
                    "regex.match"
                    | "re_match"
                    | "regex.find_n"
                    | "regex.find_all_string_submatch_n"
                    | "regex.split" => (Kind::Ordinary, 1),
                    _ => return,
                };
                if let Some(TermValue::String(pattern)) = terms.get(at).map(|t| &t.value) {
                    entries
                        .entry(kind)
                        .or_default()
                        .entry(Rc::clone(pattern))
                        .or_insert(State::Uncompiled);
                }
            };
            while let Some(node) = work.pop() {
                match node {
                    Node::Rule(rule) => {
                        for branch in rule.branches() {
                            work.push(Node::Head(&branch.head));
                            work.push(Node::Body(&branch.body));
                        }
                    }
                    Node::Head(head) => {
                        for term in &head.reference {
                            work.push(Node::Term(term));
                        }
                        for term in &head.args {
                            work.push(Node::Term(term));
                        }
                        if let Some(term) = &head.key {
                            work.push(Node::Term(term));
                        }
                        if let Some(term) = &head.value {
                            work.push(Node::Term(term));
                        }
                    }
                    Node::Body(body) => work.extend(body.iter().map(Node::Expr)),
                    Node::Expr(expr) => {
                        for with in &expr.with {
                            work.push(Node::Term(&with.target));
                            work.push(Node::Term(&with.value));
                        }
                        match &expr.terms {
                            ExprTerms::Term(term) => work.push(Node::Term(term)),
                            ExprTerms::Call(terms) => {
                                call(terms);
                                work.extend(terms.iter().map(Node::Term));
                            }
                            ExprTerms::Some(decl) => work.extend(decl.symbols.iter().map(Node::Term)),
                            ExprTerms::Every(every) => {
                                if let Some(key) = &every.key {
                                    work.push(Node::Term(key));
                                }
                                work.push(Node::Term(&every.value));
                                work.push(Node::Term(&every.domain));
                                work.push(Node::Body(&every.body));
                            }
                        }
                    }
                    Node::Term(term) => match &term.value {
                        TermValue::Call(terms) => {
                            call(terms);
                            work.extend(terms.iter().map(Node::Term));
                        }
                        TermValue::Ref(terms) | TermValue::Array(terms) | TermValue::Set(terms) => {
                            work.extend(terms.iter().map(Node::Term))
                        }
                        TermValue::Object(pairs) => {
                            for (key, value) in pairs {
                                work.push(Node::Term(key));
                                work.push(Node::Term(value));
                            }
                        }
                        TermValue::ArrayCompr(term, body) | TermValue::SetCompr(term, body) => {
                            work.push(Node::Term(term));
                            work.push(Node::Body(body));
                        }
                        TermValue::ObjectCompr(key, value, body) => {
                            work.push(Node::Term(key));
                            work.push(Node::Term(value));
                            work.push(Node::Body(body));
                        }
                        TermValue::TemplateString { parts, .. } => {
                            for part in parts {
                                match part {
                                    TemplatePart::Term(term) => work.push(Node::Term(term)),
                                    TemplatePart::Expr(expr) => work.push(Node::Expr(expr)),
                                }
                            }
                        }
                        TermValue::Null
                        | TermValue::Bool(_)
                        | TermValue::Number(_)
                        | TermValue::String(_)
                        | TermValue::Var(_) => {}
                    },
                }
            }
            Rc::new(Self {
                entries: RefCell::new(entries),
            })
        }
    }
    pub(crate) fn clear_current() {
        CACHE.with(|cache| cache.borrow_mut().clear());
    }
    pub(crate) fn current_entries() -> usize {
        CACHE.with(|cache| cache.borrow().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Diagnostic only: no production cache behavior is changed. The exact same
    /// synthetic calls run against pinned OPA in measurements/rego-cache/run.sh.
    #[test]
    #[ignore = "explicit cache semantics/retention measurement"]
    fn rego_cache_probe() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../docs/research/measurements/rego-cache/cases.json"
        ))
        .unwrap();
        let strict = std::env::var("SHARDS_REGO_CACHE_STRICT").as_deref() == Ok("1");
        let mut pure = true;
        let count = || CACHE.with(|c| c.borrow().len());
        let clear = || CACHE.with(|c| c.borrow_mut().clear());
        let call = |name: &str, args: &[Value]| {
            let mut context = Context::default();
            let result = lookup(name).unwrap()(&mut context, args).unwrap().unwrap();
            drop(context);
            match result {
                Value::Bool(answer) => answer,
                other => panic!("non-bool: {other:?}"),
            }
        };
        for case in cases.as_array().unwrap() {
            clear();
            let mut answers = Vec::new();
            let mut retained = Vec::new();
            for instruction in case["calls"].as_array().unwrap() {
                let args: Vec<_> = instruction["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| Value::string(a.as_str().unwrap()))
                    .collect();
                answers.push(call(instruction["name"].as_str().unwrap(), &args));
                retained.push(count());
            }
            let expected: Vec<_> = case["expected_pure"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_bool().unwrap())
                .collect();
            pure &= answers == expected;
            println!(
                "REGOCACHE_CASE {}",
                serde_json::json!({"implementation":"shards", "name":case["name"], "answers":answers, "expected_pure":expected, "entries_after_context_drop":retained})
            );
        }
        for kind in ["ordinary", "template"] {
            for multiple in [1, 2, 4] {
                clear();
                let n = CACHE_MAX * multiple;
                let mut last = String::new();
                for i in 0..n {
                    last = format!("cache-probe-{i}");
                    let mut args = vec![Value::string(last.as_str()), Value::string(last.as_str())];
                    if kind == "template" {
                        args.extend([Value::string("<"), Value::string(">")]);
                    }
                    assert!(call(
                        if kind == "template" {
                            "regex.template_match"
                        } else {
                            "regex.match"
                        },
                        &args
                    ));
                }
                let re = cached(&last).unwrap();
                let object = Rc::downgrade(&re);
                drop(re);
                assert!(
                    object.upgrade().is_some(),
                    "cache did not retain compiled object after context drop"
                );
                let before_extra = count();
                let extra = [
                    Value::string("cache-probe-extra"),
                    Value::string("cache-probe-extra"),
                ];
                assert!(call("regex.match", &extra));
                let after_extra = count();
                println!(
                    "REGOCACHE_RETAIN {}",
                    serde_json::json!({"implementation":"shards", "kind":kind, "calls":n, "declared_max":CACHE_MAX, "entries_after_context_drop":before_extra, "entries_after_ordinary_extra":after_extra})
                );
                clear();
                assert!(
                    object.upgrade().is_none(),
                    "compiled object survived removal of its only owner"
                );
            }
        }
        clear();
        let held = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            let (sent, received) = std::sync::mpsc::channel();
            for _ in 0..2 {
                let held = &held;
                let sent = sent.clone();
                scope.spawn(move || {
                    for i in 0..CACHE_MAX {
                        let pattern = format!("cache-probe-thread-{i}");
                        let args = [
                            Value::string(pattern.as_str()),
                            Value::string(pattern.as_str()),
                            Value::string("<"),
                            Value::string(">"),
                        ];
                        let mut context = Context::default();
                        assert_eq!(template_match(&mut context, &args), Ok(Some(Value::Bool(true))));
                    }
                    sent.send(CACHE.with(|c| c.borrow().len())).unwrap();
                    held.wait();
                });
            }
            let first = received.recv().unwrap();
            let second = received.recv().unwrap();
            println!(
                "REGOCACHE_THREADS {}",
                serde_json::json!({"implementation":"shards","threads":2,"entries_each":[first,second],"simultaneous_total":first+second,"same_patterns":true})
            );
            held.wait();
        });
        assert_eq!(count(), 0);
        assert!(
            !strict || pure,
            "regex results depend on prior compilation kind or delimiter choice"
        );
    }

    fn glob(pattern: &str, s: &str) -> Result<Option<Value>, BuiltinError> {
        let args = [Value::string(pattern), Value::array(Vec::new()), Value::string(s)];
        glob_match(&mut Context::default(), &args)
    }

    #[test]
    fn glob_fails_where_go_panics() {
        // gobwas/glob slices out of range on these, which ends OPA's process.
        assert!(glob("a{}", "a").is_err());
        assert!(glob("b{b{", "bb").is_err());
        assert_eq!(glob("a{}b", "ab"), Ok(Some(Value::Bool(false))));
    }
}

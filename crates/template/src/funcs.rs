//! The functions templates call: text/template's builtins (funcs.go) and the ones the
//! Docker CLI adds (docker/cli v29.8.1 templates/templates.go, basicFunctions and
//! HeaderFunctions).

use std::borrow::Cow;

use crate::fmt::{sprint, sprint_printable, sprintf, sprintln};
use crate::reflect::{Key, Param, R, child, indirect, indirect_interface, is_true};
use crate::strconv;
use crate::value::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Func {
    And,
    Or,
    Not,
    Call,
    Html,
    Js,
    UrlQuery,
    Print,
    Printf,
    Println,
    Index,
    Slice,
    Len,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Json,
    Split,
    Join,
    Title,
    Lower,
    Upper,
    Pad,
    Truncate,
    /// HeaderFunctions' json, title, lower and upper: the string unchanged.
    HeaderText,
    /// HeaderFunctions' split and join: the first string unchanged.
    HeaderPair,
    HeaderTruncate,
}

/// A function's parameters, the last of them variadic when `variadic` is set.
pub(crate) struct Sig {
    pub params: &'static [Param],
    pub variadic: Option<Param>,
}

const fn sig(params: &'static [Param], variadic: Option<Param>) -> Sig {
    Sig { params, variadic }
}

const BUILTINS: [(&str, Func); 19] = [
    ("and", Func::And),
    ("call", Func::Call),
    ("html", Func::Html),
    ("index", Func::Index),
    ("slice", Func::Slice),
    ("js", Func::Js),
    ("len", Func::Len),
    ("not", Func::Not),
    ("or", Func::Or),
    ("print", Func::Print),
    ("printf", Func::Printf),
    ("println", Func::Println),
    ("urlquery", Func::UrlQuery),
    ("eq", Func::Eq),
    ("ge", Func::Ge),
    ("gt", Func::Gt),
    ("le", Func::Le),
    ("lt", Func::Lt),
    ("ne", Func::Ne),
];

const DOCKER: [(&str, Func); 8] = [
    ("json", Func::Json),
    ("split", Func::Split),
    ("join", Func::Join),
    ("title", Func::Title),
    ("lower", Func::Lower),
    ("upper", Func::Upper),
    ("pad", Func::Pad),
    ("truncate", Func::Truncate),
];

/// Whether a template may name the function (parse.go's hasFunction).
pub(crate) fn defined(name: &str) -> bool {
    DOCKER.iter().chain(BUILTINS.iter()).any(|(n, _)| *n == name)
}

/// funcs.go's findFunction: the template's functions first (Docker's, or its header
/// variants once `Funcs(HeaderFunctions)` replaced them), then the builtins.
pub(crate) fn find(name: &str, header: bool) -> Option<(Func, bool)> {
    if let Some((_, f)) = DOCKER.iter().find(|(n, _)| *n == name) {
        let f = match (header, f) {
            (true, Func::Json | Func::Title | Func::Lower | Func::Upper) => Func::HeaderText,
            (true, Func::Split | Func::Join) => Func::HeaderPair,
            (true, Func::Truncate) => Func::HeaderTruncate,
            _ => *f,
        };
        return Some((f, false));
    }
    BUILTINS.iter().find(|(n, _)| *n == name).map(|(_, f)| (*f, true))
}

use Param as P;

impl Func {
    pub(crate) fn sig(self) -> Sig {
        match self {
            Func::And | Func::Or | Func::Call | Func::Index | Func::Slice | Func::Eq => {
                sig(&[P::Value], Some(P::Value))
            }
            Func::Not | Func::Len => sig(&[P::Value], None),
            Func::Html | Func::Js | Func::UrlQuery | Func::Print | Func::Println => sig(&[], Some(P::Any)),
            Func::Printf => sig(&[P::String], Some(P::Any)),
            Func::Ne | Func::Lt | Func::Le | Func::Gt | Func::Ge => sig(&[P::Value, P::Value], None),
            Func::Json => sig(&[P::Any], None),
            Func::Split | Func::HeaderPair => sig(&[P::String, P::String], None),
            Func::Join => sig(&[P::Any, P::String], None),
            Func::Title | Func::Lower | Func::Upper | Func::HeaderText => sig(&[P::String], None),
            Func::Pad => sig(&[P::String, P::Int, P::Int], None),
            Func::Truncate | Func::HeaderTruncate => sig(&[P::String, P::Int], None),
        }
    }

    /// Calls the function on arguments evaluated and checked against its parameters.
    /// `and`, `or` and `call` are exec.go's to run, not these.
    pub(crate) fn call<'d>(self, mut args: Vec<R<'d>>) -> Result<R<'d>, String> {
        let s = |s: String| Ok(R::owned(Value::String(s)));
        let b = |b: bool| Ok(R::owned(Value::Bool(b)));
        match self {
            Func::And | Func::Or | Func::Call => Err("unreachable".into()),
            Func::Not => b(!truth(args.into_iter().next().unwrap_or(R::Invalid))),
            Func::Html => s(html(&eval_args(&args))),
            Func::Js => s(js(&eval_args(&args))),
            Func::UrlQuery => s(query_escape(&eval_args(&args))),
            Func::Print => s(sprint(&anys(&args))),
            Func::Println => s(sprintln(&anys(&args))),
            Func::Printf => {
                let vals = anys(&args);
                let (format, rest) = vals.split_first().ok_or("printf: no format")?;
                let format = match format {
                    Value::String(f) => f.as_str(),
                    _ => "",
                };
                s(sprintf(format, rest))
            }
            Func::Index => {
                let item = take(&mut args, 0);
                index(item, args.into_iter().skip(1).collect())
            }
            Func::Slice => {
                let item = take(&mut args, 0);
                slice(item, args.into_iter().skip(1).collect())
            }
            Func::Len => length(take(&mut args, 0)).map(|n| R::owned(Value::Int(n))),
            Func::Eq => {
                let first = take(&mut args, 0);
                b(eq(first, args.into_iter().skip(1).collect())?)
            }
            Func::Ne => {
                let (x, y) = (take(&mut args, 0), take(&mut args, 1));
                b(!eq(x, vec![y])?)
            }
            Func::Lt => b(lt(take(&mut args, 0), take(&mut args, 1))?),
            Func::Le => {
                let (x, y) = (take(&mut args, 0), take(&mut args, 1));
                b(le(x, y)?)
            }
            Func::Gt => {
                let (x, y) = (take(&mut args, 0), take(&mut args, 1));
                b(!le(x, y)?)
            }
            Func::Ge => b(!lt(take(&mut args, 0), take(&mut args, 1))?),
            Func::Json => {
                let mut out = String::new();
                take(&mut args, 0).to_any().json(&mut out)?;
                s(out)
            }
            Func::Split => {
                let (x, sep) = (string(&args, 0), string(&args, 1));
                Ok(R::owned(Value::strings(split(&x, &sep))))
            }
            Func::Join => join(&take(&mut args, 0), &string(&args, 1)).and_then(s),
            Func::Title => s(title(&string(&args, 0))),
            Func::Lower => s(map_chars(&string(&args, 0), true)),
            Func::Upper => s(map_chars(&string(&args, 0), false)),
            Func::Pad => {
                let x = string(&args, 0);
                let (pre, post) = (int(&args, 1), int(&args, 2));
                if x.is_empty() {
                    return s(x);
                }
                let pre = usize::try_from(pre).map_err(|_| NEGATIVE_REPEAT)?;
                let post = usize::try_from(post).map_err(|_| NEGATIVE_REPEAT)?;
                s(format!("{}{x}{}", " ".repeat(pre), " ".repeat(post)))
            }
            Func::Truncate => {
                let x = string(&args, 0);
                let n = int(&args, 1);
                if i64::try_from(x.len()).unwrap_or(i64::MAX) < n {
                    return s(x);
                }
                let n = usize::try_from(n)
                    .map_err(|_| format!("runtime error: slice bounds out of range [:{n}]"))?;
                s(String::from_utf8_lossy(x.as_bytes().get(..n).unwrap_or(&[])).into_owned())
            }
            Func::HeaderText | Func::HeaderPair | Func::HeaderTruncate => s(string(&args, 0)),
        }
    }
}

/// strings.Repeat's panic, which text/template's safeCall turns into an error.
const NEGATIVE_REPEAT: &str = "strings: negative Repeat count";

fn take<'d>(args: &mut [R<'d>], i: usize) -> R<'d> {
    args.get_mut(i)
        .map_or(R::Invalid, |a| std::mem::replace(a, R::Invalid))
}

fn string(args: &[R<'_>], i: usize) -> String {
    match args.get(i).and_then(R::value) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn int(args: &[R<'_>], i: usize) -> i64 {
    match args.get(i).and_then(R::value) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    }
}

/// Arguments as a function taking `...any` receives them.
fn anys(args: &[R<'_>]) -> Vec<Value> {
    args.iter().map(R::to_any).collect()
}

/// funcs.go's truth.
pub(crate) fn truth(r: R<'_>) -> bool {
    is_true(&indirect_interface(r))
}

/// funcs.go's evalArgs: one string as it is, else the arguments as Sprint prints them,
/// a nil one as `<no value>`.
fn eval_args(args: &[R<'_>]) -> String {
    let vals = anys(args);
    if let [Value::String(s)] = vals.as_slice() {
        return s.clone();
    }
    let vals: Vec<Value> = vals
        .into_iter()
        .map(|v| match v {
            Value::Nil => Value::String("<no value>".into()),
            v => v,
        })
        .collect();
    sprint_printable(&vals)
}

/// funcs.go's HTMLEscapeString.
fn html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\0' => out.push('\u{fffd}'),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// funcs.go's JSEscapeString.
fn js(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '<' => out.push_str("\\u003C"),
            '>' => out.push_str("\\u003E"),
            '&' => out.push_str("\\u0026"),
            '=' => out.push_str("\\u003D"),
            _ if c < ' ' => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            _ if !c.is_ascii() && !strconv::is_print(c) => {
                out.push_str(&format!("\\u{:04X}", u32::from(c)));
            }
            _ => out.push(c),
        }
    }
    out
}

/// net/url's QueryEscape.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The type of what a value points to, as funcs.go's indirect leaves it.
fn pointee(v: &Value) -> String {
    let name = v.type_name();
    match name.strip_prefix('*') {
        Some(elem) => elem.to_owned(),
        None => name,
    }
}

/// funcs.go's indexArg.
fn index_arg(index: &R<'_>, cap: usize) -> Result<usize, String> {
    let x = match index {
        R::Invalid => return Err("cannot index slice/array with nil".into()),
        R::Plain(v) => match v.as_ref() {
            Value::Int(i) => *i,
            Value::Uint(u) => *u as i64,
            _ => {
                return Err(format!(
                    "cannot index slice/array with type {}",
                    index.type_name()
                ));
            }
        },
        R::Iface(_) => {
            return Err(format!(
                "cannot index slice/array with type {}",
                index.type_name()
            ));
        }
    };
    match usize::try_from(x) {
        Ok(i) if i <= cap => Ok(i),
        _ => Err(format!("index out of range: {x}")),
    }
}

/// funcs.go's index.
fn index<'d>(item: R<'d>, indexes: Vec<R<'d>>) -> Result<R<'d>, String> {
    let mut item = indirect_interface(item);
    if matches!(item, R::Invalid) {
        return Err("index of untyped nil".into());
    }
    for idx in indexes {
        let idx = indirect_interface(idx);
        let (it, is_nil) = indirect(item);
        if is_nil {
            return Err("index of nil pointer".into());
        }
        let R::Plain(v) = it else {
            return Err("index of untyped nil".into());
        };
        item = match v.as_ref() {
            Value::List(kind, items) => {
                let x = index_arg(&idx, items.len())?;
                let e = child(&v, Key::Index(x)).ok_or("reflect: slice index out of range")?;
                R::elem(*kind, e)
            }
            Value::String(s) => {
                let x = index_arg(&idx, s.len())?;
                let byte = s.as_bytes().get(x).ok_or("reflect: string index out of range")?;
                R::owned(Value::Uint(u64::from(*byte)))
            }
            Value::Map(kind, _) => {
                let key = match &idx {
                    R::Invalid => return Err("value is nil; should be of type string".into()),
                    R::Plain(k) => match k.as_ref() {
                        Value::String(k) => k.clone(),
                        _ => return Err(format!("value has type {}; should be string", idx.type_name())),
                    },
                    R::Iface(_) => {
                        return Err(format!("value has type {}; should be string", idx.type_name()));
                    }
                };
                let kind = *kind;
                match child(&v, Key::Name(&key)) {
                    Some(e) => R::elem(kind, e),
                    None => R::elem(kind, Cow::Owned(kind.zero())),
                }
            }
            _ => return Err(format!("can't index item of type {}", pointee(&v))),
        };
    }
    Ok(item)
}

/// funcs.go's slice. A list's capacity is its length.
fn slice<'d>(item: R<'d>, indexes: Vec<R<'d>>) -> Result<R<'d>, String> {
    let item = indirect_interface(item);
    let R::Plain(v) = item else {
        return Err("slice of untyped nil".into());
    };
    if indexes.len() > 3 {
        return Err(format!("too many slice indexes: {}", indexes.len()));
    }
    let len = match v.as_ref() {
        Value::String(s) => {
            if indexes.len() == 3 {
                return Err("cannot 3-index slice a string".into());
            }
            s.len()
        }
        Value::List(_, items) => items.len(),
        _ => return Err(format!("can't slice item of type {}", pointee(&v))),
    };
    let mut idx = [0, len, 0];
    for (i, index) in indexes.iter().enumerate() {
        let x = index_arg(index, len)?;
        if let Some(slot) = idx.get_mut(i) {
            *slot = x;
        }
    }
    let [i, j, k] = idx;
    if i > j {
        return Err(format!("invalid slice index: {i} > {j}"));
    }
    if indexes.len() == 3 && j > k {
        return Err(format!("invalid slice index: {j} > {k}"));
    }
    Ok(R::owned(match v.as_ref() {
        Value::String(s) => {
            Value::String(String::from_utf8_lossy(s.as_bytes().get(i..j).unwrap_or(&[])).into_owned())
        }
        Value::List(kind, items) => Value::List(*kind, items.get(i..j).unwrap_or(&[]).to_vec()),
        _ => Value::Nil,
    }))
}

/// funcs.go's length.
fn length(item: R<'_>) -> Result<i64, String> {
    let (item, is_nil) = indirect(item);
    if is_nil {
        return Err("len of nil pointer".into());
    }
    let n = match &item {
        R::Invalid => return Err("reflect: call of reflect.Value.Type on zero Value".into()),
        R::Plain(v) | R::Iface(v) => match v.as_ref() {
            Value::String(s) => s.len(),
            Value::List(_, l) => l.len(),
            Value::Map(_, m) => m.len(),
            _ => return Err(format!("len of type {}", pointee(v))),
        },
    };
    Ok(i64::try_from(n).unwrap_or(i64::MAX))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Basic {
    Invalid,
    Bool,
    Int,
    Float,
    String,
    Uint,
}

/// funcs.go's basicKind: the kind, and whether it is one of the basic ones.
fn basic_kind(r: &R<'_>) -> (Basic, bool) {
    match r.value() {
        Some(Value::Bool(_)) => (Basic::Bool, true),
        Some(Value::Int(_)) => (Basic::Int, true),
        Some(Value::Uint(_)) => (Basic::Uint, true),
        Some(Value::Float(_)) => (Basic::Float, true),
        Some(Value::String(_)) => (Basic::String, true),
        _ => (Basic::Invalid, false),
    }
}

/// The reflect kind of a non-basic value, as canCompare compares them.
fn reflect_kind(r: &R<'_>) -> u8 {
    match r.value() {
        None => 0,
        Some(Value::List(..)) => 1,
        Some(Value::Map(..)) => 2,
        Some(_) => 3,
    }
}

const BAD_COMPARISON: &str = "invalid type for comparison";

/// funcs.go's eq.
fn eq(arg1: R<'_>, args: Vec<R<'_>>) -> Result<bool, String> {
    let arg1 = indirect_interface(arg1);
    if args.is_empty() {
        return Err("missing argument for comparison".into());
    }
    let (k1, _) = basic_kind(&arg1);
    let v1 = arg1.value();
    for arg in args {
        let arg = indirect_interface(arg);
        let (k2, _) = basic_kind(&arg);
        let v2 = arg.value();
        let truth = if k1 != k2 {
            match (v1, v2) {
                (Some(Value::Int(i)), Some(Value::Uint(u))) | (Some(Value::Uint(u)), Some(Value::Int(i))) => {
                    *i >= 0 && *i as u64 == *u
                }
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "incompatible types for comparison: {} and {}",
                        arg1.type_name(),
                        arg.type_name()
                    ));
                }
                _ => false,
            }
        } else {
            match (v1, v2) {
                (Some(Value::Bool(a)), Some(Value::Bool(b))) => a == b,
                (Some(Value::Float(a)), Some(Value::Float(b))) => a == b,
                (Some(Value::Int(a)), Some(Value::Int(b))) => a == b,
                (Some(Value::Uint(a)), Some(Value::Uint(b))) => a == b,
                (Some(Value::String(a)), Some(Value::String(b))) => a == b,
                _ => {
                    let (r1, r2) = (reflect_kind(&arg1), reflect_kind(&arg));
                    if r1 != r2 && r1 != 0 && r2 != 0 {
                        return Err(format!(
                            "non-comparable types {}: {}, {}: {}",
                            show(v1, "%s"),
                            arg1.type_name(),
                            arg.type_name(),
                            show(v2, "%v")
                        ));
                    }
                    match (v1, v2) {
                        (None, None) => true,
                        (None, _) | (_, None) => false,
                        (Some(a), Some(b)) => match (a, b) {
                            (Value::Object(a), Value::Object(b)) => std::rc::Rc::ptr_eq(a, b),
                            _ => {
                                return Err(format!(
                                    "non-comparable type {}: {}",
                                    show(v2, "%s"),
                                    arg.type_name()
                                ));
                            }
                        },
                    }
                }
            }
        };
        if truth {
            return Ok(true);
        }
    }
    Ok(false)
}

fn show(v: Option<&Value>, verb: &str) -> String {
    sprintf(verb, &[v.cloned().unwrap_or(Value::Nil)])
}

/// funcs.go's lt.
fn lt(arg1: R<'_>, arg2: R<'_>) -> Result<bool, String> {
    let arg1 = indirect_interface(arg1);
    let arg2 = indirect_interface(arg2);
    let (k1, ok1) = basic_kind(&arg1);
    if !ok1 {
        return Err(BAD_COMPARISON.into());
    }
    let (k2, ok2) = basic_kind(&arg2);
    if !ok2 {
        return Err(BAD_COMPARISON.into());
    }
    Ok(match (arg1.value(), arg2.value()) {
        (Some(Value::Int(i)), Some(Value::Uint(u))) => *i < 0 || (*i as u64) < *u,
        (Some(Value::Uint(u)), Some(Value::Int(i))) => *i >= 0 && *u < *i as u64,
        _ if k1 != k2 => {
            return Err(format!(
                "incompatible types for comparison: {} and {}",
                arg1.type_name(),
                arg2.type_name()
            ));
        }
        (Some(Value::Float(a)), Some(Value::Float(b))) => a < b,
        (Some(Value::Int(a)), Some(Value::Int(b))) => a < b,
        (Some(Value::Uint(a)), Some(Value::Uint(b))) => a < b,
        (Some(Value::String(a)), Some(Value::String(b))) => a < b,
        _ => return Err(BAD_COMPARISON.into()),
    })
}

/// funcs.go's le.
fn le(arg1: R<'_>, arg2: R<'_>) -> Result<bool, String> {
    if lt(arg1.clone(), arg2.clone())? {
        return Ok(true);
    }
    eq(arg1, vec![arg2])
}

/// strings.Split.
fn split(s: &str, sep: &str) -> Vec<String> {
    if sep.is_empty() {
        return s.chars().map(String::from).collect();
    }
    s.split(sep).map(str::to_owned).collect()
}

/// templates.go's joinElements.
fn join(elems: &R<'_>, sep: &str) -> Result<String, String> {
    let Some(v) = elems.value() else {
        return Ok(String::new());
    };
    match v {
        Value::List(_, items) => Ok(items
            .iter()
            .map(|e| sprint(std::slice::from_ref(e)))
            .collect::<Vec<_>>()
            .join(sep)),
        Value::Map(_, m) => {
            let mut out: Vec<String> = m.values().map(|e| sprint(std::slice::from_ref(e))).collect();
            out.sort();
            Ok(out.join(sep))
        }
        _ => Err(format!("expected slice, got {}", v.type_name())),
    }
}

/// strings.Title's isSeparator.
fn is_separator(c: char) -> bool {
    if c.is_ascii() {
        return !(c.is_ascii_alphanumeric() || c == '_');
    }
    if c.is_alphabetic() || c.is_numeric() {
        return false;
    }
    c.is_whitespace()
}

/// strings.Title.
fn title(s: &str) -> String {
    let mut prev = ' ';
    s.chars()
        .map(|c| {
            let out = if is_separator(prev) { title_case(c) } else { c };
            prev = c;
            out
        })
        .collect()
}

/// unicode's one-rune case mapping, as strings.ToLower and ToUpper apply it: where
/// Unicode maps a rune to several, Go keeps the rune (or, for U+0130, maps it to 'i').
fn simple_case(c: char, lower: bool) -> char {
    if !lower && let Some(u) = greek_upper(c) {
        return u;
    }
    let mut it: Box<dyn Iterator<Item = char>> = if lower {
        Box::new(c.to_lowercase())
    } else {
        Box::new(c.to_uppercase())
    };
    match (it.next(), it.next()) {
        (Some(m), None) => m,
        (Some(m), Some(_)) if lower => m,
        _ => c,
    }
}

/// Runes whose one-rune upper and title cases std's full mappings do not give: Greek with
/// ypogegrammeni (UnicodeData.txt's simple mappings).
fn greek_upper(c: char) -> Option<char> {
    let u = u32::from(c);
    let up = match u {
        0x1f80..=0x1f87 | 0x1f90..=0x1f97 | 0x1fa0..=0x1fa7 => u + 8,
        0x1fb3 | 0x1fc3 | 0x1ff3 => u + 9,
        _ => return None,
    };
    char::from_u32(up)
}

/// unicode.ToTitle: the upper case, but for the Latin digraphs, whose title case is
/// their own (UnicodeData.txt).
fn title_case(c: char) -> char {
    match c {
        'Ǆ' | 'ǅ' | 'ǆ' => 'ǅ',
        'Ǉ' | 'ǈ' | 'ǉ' => 'ǈ',
        'Ǌ' | 'ǋ' | 'ǌ' => 'ǋ',
        'Ǳ' | 'ǲ' | 'ǳ' => 'ǲ',
        _ => simple_case(c, false),
    }
}

fn map_chars(s: &str, lower: bool) -> String {
    s.chars().map(|c| simple_case(c, lower)).collect()
}

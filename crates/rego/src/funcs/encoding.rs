//! OPA's encoding builtins (topdown/encoding.go, json.go, parse_bytes.go,
//! parse_units.go): base64, hex, URL queries, JSON and YAML, and units, with Go's
//! standard library and OPA's dependencies (sigs.k8s.io/yaml, go-yaml v2) ported where
//! their results or error texts show.

mod gofmt;
mod gojson;
mod patch;
mod yaml;

use std::collections::BTreeMap;

use num_bigint::{BigInt, BigUint, Sign};

use super::{Builtin, BuiltinError, Context, arg, string_operand};
use crate::goquote;
use crate::number::{self, Float};
use crate::value::{self, Number, Value};

pub fn lookup(name: &str) -> Option<Builtin> {
    Some(match name {
        "base64.decode" => base64_decode,
        "base64.encode" => base64_encode,
        "base64.is_valid" => base64_is_valid,
        "base64url.decode" => base64url_decode,
        "base64url.encode" => base64url_encode,
        "base64url.encode_no_pad" => base64url_encode_no_pad,
        "hex.decode" => hex_decode,
        "hex.encode" => hex_encode,
        "json.filter" => json_filter,
        "json.is_valid" => json_is_valid,
        "json.marshal" => json_marshal,
        "json.marshal_with_options" => json_marshal_with_options,
        "json.patch" => json_patch,
        "json.remove" => json_remove,
        "json.unmarshal" => json_unmarshal,
        "yaml.is_valid" => yaml_is_valid,
        "yaml.marshal" => yaml_marshal,
        "yaml.unmarshal" => yaml_unmarshal,
        "urlquery.decode" => urlquery_decode,
        "urlquery.decode_object" => urlquery_decode_object,
        "urlquery.encode" => urlquery_encode,
        "urlquery.encode_object" => urlquery_encode_object,
        "units.parse" => units_parse,
        "units.parse_bytes" => units_parse_bytes,
        _ => return None,
    })
}

type Out = Result<Option<Value>, BuiltinError>;

fn ok(v: Value) -> Out {
    Ok(Some(v))
}

fn other(msg: impl Into<String>) -> BuiltinError {
    BuiltinError::Other(msg.into())
}

/// A Rego value as OPA prints it (`Term.String()`).
fn rego_string(v: &Value) -> String {
    let mut out = String::new();
    write_rego(&mut out, v);
    out
}

fn write_rego(out: &mut String, v: &Value) {
    enum P<'a> {
        Text(&'static str),
        Val(&'a Value),
    }
    let mut stack = vec![P::Val(v)];
    while let Some(p) = stack.pop() {
        let v = match p {
            P::Text(t) => {
                out.push_str(t);
                continue;
            }
            P::Val(v) => v,
        };
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(n.text()),
            Value::String(s) => goquote::quote(out, s),
            Value::Array(a) => {
                out.push('[');
                stack.push(P::Text("]"));
                for (i, x) in a.iter().enumerate().rev() {
                    stack.push(P::Val(x));
                    if i > 0 {
                        stack.push(P::Text(", "));
                    }
                }
            }
            Value::Object(m) => {
                out.push('{');
                stack.push(P::Text("}"));
                for (i, (k, x)) in m.iter().enumerate().rev() {
                    stack.push(P::Val(x));
                    stack.push(P::Text(": "));
                    stack.push(P::Val(k));
                    if i > 0 {
                        stack.push(P::Text(", "));
                    }
                }
            }
            Value::Set(s) => {
                if s.is_empty() {
                    out.push_str("set()");
                    continue;
                }
                out.push('{');
                stack.push(P::Text("}"));
                for (i, x) in s.iter().enumerate().rev() {
                    stack.push(P::Val(x));
                    if i > 0 {
                        stack.push(P::Text(", "));
                    }
                }
            }
        }
    }
}

/// builtins.ObjectOperand.
fn object_operand(v: &Value, pos: usize) -> Result<&BTreeMap<Value, Value>, BuiltinError> {
    match v {
        Value::Object(m) => Ok(m),
        _ => Err(BuiltinError::operand_type(pos, v, &["object"])),
    }
}

/// builtins.ArrayOperand.
fn array_operand(v: &Value, pos: usize) -> Result<&[Value], BuiltinError> {
    match v {
        Value::Array(a) => Ok(a),
        _ => Err(BuiltinError::operand_type(pos, v, &["array"])),
    }
}

/// ast.JSON then json.Marshal.
fn marshal(v: &Value) -> Result<String, BuiltinError> {
    value::to_json(v).map_err(|e| other(e.0))
}

// ---- base64 (encoding/base64) ----

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64_encode(src: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let sym = |i: u32| {
        char::from(
            alphabet
                .get(usize::try_from(i & 0x3F).unwrap_or(0))
                .copied()
                .unwrap_or(b'A'),
        )
    };
    let mut out = String::with_capacity(src.len().div_ceil(3) * 4);
    for chunk in src.chunks(3) {
        let b0 = u32::from(chunk.first().copied().unwrap_or(0));
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let v = (b0 << 16) | (b1 << 8) | b2;
        out.push(sym(v >> 18));
        out.push(sym(v >> 12));
        if chunk.len() > 1 {
            out.push(sym(v >> 6));
        } else if pad {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(sym(v));
        } else if pad {
            out.push('=');
        }
    }
    out
}

/// Encoding.DecodeString (padded, not strict): the bytes, or the CorruptInputError's
/// offset.
fn b64_decode(src: &[u8], alphabet: &[u8; 64]) -> Result<Vec<u8>, usize> {
    let decode = |c: u8| {
        alphabet
            .iter()
            .position(|&a| a == c)
            .and_then(|p| u32::try_from(p).ok())
    };
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    let mut si = 0usize;
    while si < src.len() {
        // decodeQuantum
        let mut dbuf = [0u32; 4];
        let mut dlen = 4;
        let mut j = 0usize;
        let mut err: Option<usize> = None;
        while j < 4 {
            if src.len() == si {
                if j == 0 {
                    return Ok(out);
                }
                return Err(si - j);
            }
            let input = src.get(si).copied().unwrap_or(0);
            si += 1;
            if let Some(v) = decode(input) {
                if let Some(slot) = dbuf.get_mut(j) {
                    *slot = v;
                }
                j += 1;
                continue;
            }
            if input == b'\n' || input == b'\r' {
                continue;
            }
            if input != b'=' {
                return Err(si - 1);
            }
            match j {
                0 | 1 => return Err(si - 1),
                2 => {
                    while si < src.len() && matches!(src.get(si), Some(b'\n' | b'\r')) {
                        si += 1;
                    }
                    if si == src.len() {
                        return Err(src.len());
                    }
                    if src.get(si) != Some(&b'=') {
                        return Err(si - 1);
                    }
                    si += 1;
                }
                _ => {}
            }
            while si < src.len() && matches!(src.get(si), Some(b'\n' | b'\r')) {
                si += 1;
            }
            if si < src.len() {
                err = Some(si);
            }
            dlen = j;
            break;
        }
        let val = (dbuf[0] << 18) | (dbuf[1] << 12) | (dbuf[2] << 6) | dbuf[3];
        let bytes = [(val >> 16) as u8, (val >> 8) as u8, val as u8];
        out.extend_from_slice(bytes.get(..dlen.saturating_sub(1)).unwrap_or_default());
        if let Some(e) = err {
            return Err(e);
        }
    }
    Ok(out)
}

/// base64.StdEncoding.DecodeString, for go-yaml's !!binary.
fn base64_std_decode(src: &[u8]) -> Result<Vec<u8>, usize> {
    b64_decode(src, STD)
}

fn corrupt(at: usize) -> BuiltinError {
    other(format!("illegal base64 data at input byte {at}"))
}

fn base64_encode(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok(Value::string(b64_encode(s.as_bytes(), STD, true)))
}

fn base64_decode(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    let b = b64_decode(s.as_bytes(), STD).map_err(corrupt)?;
    ok(Value::string(gofmt::lossy(&b)))
}

fn base64_is_valid(_: &mut Context, args: &[Value]) -> Out {
    let Some(s) = arg(args, 0)?.as_str() else {
        return ok(Value::Bool(false));
    };
    ok(Value::Bool(b64_decode(s.as_bytes(), STD).is_ok()))
}

fn base64url_encode(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok(Value::string(b64_encode(s.as_bytes(), URL, true)))
}

fn base64url_encode_no_pad(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok(Value::string(b64_encode(s.as_bytes(), URL, false)))
}

fn base64url_decode(_: &mut Context, args: &[Value]) -> Out {
    let mut s = string_operand(arg(args, 0)?, 1)?.to_string();
    if !s.ends_with('=') {
        match s.len() % 4 {
            0 => {}
            2 => s.push_str("=="),
            3 => s.push('='),
            _ => return Err(other(format!("illegal base64url string: {s}"))),
        }
    }
    let b = b64_decode(s.as_bytes(), URL).map_err(corrupt)?;
    ok(Value::string(gofmt::lossy(&b)))
}

// ---- hex (encoding/hex) ----

fn hex_encode(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        out.push_str(&format!("{b:02x}"));
    }
    ok(Value::string(out))
}

/// InvalidByteError: `%#U` of the byte as a rune.
fn invalid_byte(b: u8) -> BuiltinError {
    let r = u32::from(b);
    let mut msg = format!("encoding/hex: invalid byte: U+{r:04X}");
    if goquote::is_print(r) {
        msg.push_str(&format!(" '{}'", char::from(b)));
    }
    other(msg)
}

fn hex_decode(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?.as_bytes();
    let val = |c: u8| char::from(c).to_digit(16).and_then(|d| u8::try_from(d).ok());
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut j = 1;
    while j < s.len() {
        let p = s.get(j - 1).copied().unwrap_or(0);
        let q = s.get(j).copied().unwrap_or(0);
        let a = val(p).ok_or_else(|| invalid_byte(p))?;
        let b = val(q).ok_or_else(|| invalid_byte(q))?;
        out.push((a << 4) | b);
        j += 2;
    }
    if s.len() % 2 == 1 {
        let p = s.get(j - 1).copied().unwrap_or(0);
        if val(p).is_none() {
            return Err(invalid_byte(p));
        }
        return Err(other("encoding/hex: odd length hex string"));
    }
    ok(Value::string(gofmt::lossy(&out)))
}

// ---- URL queries (net/url) ----

fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// url.QueryUnescape: the bytes, or the EscapeError's text.
fn query_unescape(s: &str) -> Result<Vec<u8>, String> {
    let b = s.as_bytes();
    let hex = |c: Option<&u8>| c.and_then(|c| char::from(*c).to_digit(16));
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b.get(i) {
            Some(b'%') => {
                let (Some(h), Some(l)) = (hex(b.get(i + 1)), hex(b.get(i + 2))) else {
                    let rest = b.get(i..).unwrap_or_default();
                    let rest = rest.get(..3).unwrap_or(rest);
                    return Err(format!("invalid URL escape {}", gofmt::quote_bytes(rest)));
                };
                out.push(u8::try_from((h << 4) | l).unwrap_or(0));
                i += 3;
            }
            Some(b'+') => {
                out.push(b' ');
                i += 1;
            }
            Some(&c) => {
                out.push(c);
                i += 1;
            }
            None => break,
        }
    }
    Ok(out)
}

fn urlquery_encode(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok(Value::string(query_escape(s)))
}

fn urlquery_decode(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    let b = query_unescape(s).map_err(other)?;
    ok(Value::string(gofmt::lossy(&b)))
}

fn urlquery_encode_object(_: &mut Context, args: &[Value]) -> Out {
    let v = arg(args, 0)?;
    // ast.JSON: what has no JSON fails first.
    value::to_json(v).map_err(|e| other(e.0))?;
    let Value::Object(m) = v else {
        return Err(BuiltinError::operand_type(1, v, &["object"]));
    };
    let err = || BuiltinError::operand(1, "values must be string, array[string], or set[string]");
    let mut query: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (k, x) in m.iter() {
        let Value::String(k) = k else { return Err(err()) };
        match x {
            Value::String(s) => {
                query.insert(k, vec![s]);
            }
            Value::Array(_) | Value::Set(_) => {
                let items: Vec<&Value> = match x {
                    Value::Array(a) => a.iter().collect(),
                    Value::Set(s) => s.iter().collect(),
                    _ => Vec::new(),
                };
                for item in items {
                    let Value::String(s) = item else { return Err(err()) };
                    query.entry(k).or_default().push(s);
                }
            }
            _ => return Err(err()),
        }
    }
    // Values.Encode: keys sorted by their bytes.
    let mut keys: Vec<&str> = query.keys().copied().collect();
    keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut out = String::new();
    for k in keys {
        let ek = query_escape(k);
        for v in query.get(k).map(Vec::as_slice).unwrap_or_default() {
            if !out.is_empty() {
                out.push('&');
            }
            out.push_str(&ek);
            out.push('=');
            out.push_str(&query_escape(v));
        }
    }
    ok(Value::string(out))
}

fn urlquery_decode_object(_: &mut Context, args: &[Value]) -> Out {
    let query = string_operand(arg(args, 0)?, 1)?;
    if query.matches('&').count() + 1 > 10000 {
        return Err(other("number of URL query parameters exceeded limit"));
    }
    let mut m: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
    let mut err: Option<String> = None;
    let mut rest = query;
    while !rest.is_empty() {
        let (key, after) = rest.split_once('&').unwrap_or((rest, ""));
        rest = after;
        if key.contains(';') {
            err = Some("invalid semicolon separator in query".to_string());
            continue;
        }
        if key.is_empty() {
            continue;
        }
        let (k, v) = key.split_once('=').unwrap_or((key, ""));
        let k = match query_unescape(k) {
            Ok(k) => k,
            Err(e) => {
                err.get_or_insert(e);
                continue;
            }
        };
        let v = match query_unescape(v) {
            Ok(v) => v,
            Err(e) => {
                err.get_or_insert(e);
                continue;
            }
        };
        m.entry(k).or_default().push(v);
    }
    if let Some(e) = err {
        return Err(other(e));
    }
    let mut out = BTreeMap::new();
    for (k, vs) in m {
        let arr = vs.iter().map(|v| Value::string(gofmt::lossy(v))).collect();
        out.insert(Value::string(gofmt::lossy(&k)), Value::array(arr));
    }
    ok(Value::object(out))
}

// ---- JSON ----

fn json_marshal(_: &mut Context, args: &[Value]) -> Out {
    ok(Value::string(marshal(arg(args, 0)?)?))
}

fn json_marshal_with_options(_: &mut Context, args: &[Value]) -> Out {
    let json = marshal(arg(args, 0)?)?;
    let opts = object_operand(arg(args, 1)?, 2)?;
    let mut indent = "\t".to_string();
    let mut prefix = String::new();
    let mut implicit = false;
    let mut explicit_set = false;
    let mut pretty = false;
    for (idx, (k, val)) in opts.iter().enumerate() {
        let Value::String(key) = k else {
            return Err(BuiltinError::operand(
                2,
                format!(
                    "failed to stringify key {} at index {idx}: operand {idx} must be string but got {}",
                    rego_string(k),
                    k.type_name()
                ),
            ));
        };
        match &**key {
            "prefix" | "indent" => {
                let Value::String(s) = val else {
                    return Err(BuiltinError::operand(
                        2,
                        format!(
                            "key {} failed cast to string: operand {idx} must be string but got {}",
                            rego_string(k),
                            val.type_name()
                        ),
                    ));
                };
                if &**key == "prefix" {
                    prefix = s.to_string();
                } else {
                    indent = s.to_string();
                }
                implicit = true;
            }
            "pretty" => {
                explicit_set = true;
                let Value::Bool(b) = val else {
                    return Err(BuiltinError::operand(
                        2,
                        format!("key {} failed cast to bool", rego_string(k)),
                    ));
                };
                pretty = *b;
            }
            _ => {
                return Err(BuiltinError::operand(
                    2,
                    format!("object contained unknown key {}", rego_string(k)),
                ));
            }
        }
    }
    if !explicit_set {
        pretty = implicit;
    }
    if pretty {
        let s = gojson::indent(&json, &prefix, &indent);
        return ok(Value::string(format!("{prefix}{s}")));
    }
    ok(Value::string(json))
}

fn json_unmarshal(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok(gojson::unmarshal(s.as_bytes()).map_err(other)?)
}

fn json_is_valid(_: &mut Context, args: &[Value]) -> Out {
    let Some(s) = arg(args, 0)?.as_str() else {
        return ok(Value::Bool(false));
    };
    ok(Value::Bool(gojson::check_valid(s.as_bytes()).is_ok()))
}

fn json_filter(_: &mut Context, args: &[Value]) -> Out {
    let obj = arg(args, 0)?;
    object_operand(obj, 1)?;
    let paths = patch::json_paths(arg(args, 1)?)?;
    let filter = patch::PathNode::Obj(patch::paths_to_object(&paths));
    ok(patch::filter(obj, &filter))
}

fn json_remove(_: &mut Context, args: &[Value]) -> Out {
    let obj = arg(args, 0)?;
    object_operand(obj, 1)?;
    let paths = patch::json_paths(arg(args, 1)?)?;
    let b = patch::PathNode::Obj(patch::paths_to_object(&paths));
    Ok(patch::remove(obj, Some(&b)))
}

fn json_patch(_: &mut Context, args: &[Value]) -> Out {
    let source = arg(args, 0)?;
    let ops = array_operand(arg(args, 1)?, 2)?;
    match patch::apply_patches(source, ops) {
        Ok(v) => Ok(v),
        Err(patch::PatchError::Plain(e)) => Err(other(e)),
        Err(patch::PatchError::Builtin(e)) => Err(e),
    }
}

// ---- YAML ----

fn yaml_marshal(_: &mut Context, args: &[Value]) -> Out {
    let json = marshal(arg(args, 0)?)?;
    ok(Value::string(yaml::marshal(&json).map_err(other)?))
}

fn yaml_unmarshal(_: &mut Context, args: &[Value]) -> Out {
    let s = string_operand(arg(args, 0)?, 1)?;
    ok(yaml::yaml_to_value(s.as_bytes()).map_err(other)?)
}

fn yaml_is_valid(_: &mut Context, args: &[Value]) -> Out {
    let Some(s) = arg(args, 0)?.as_str() else {
        return ok(Value::Bool(false));
    };
    ok(Value::Bool(yaml::yaml_to_value(s.as_bytes()).is_ok()))
}

// ---- units ----

/// extractNumAndUnit: the leading number (digits, '.', signs and exponents) and the rest.
fn extract_num_and_unit(s: &str) -> (&str, &str) {
    let b = s.as_bytes();
    let is_num = |c: u8| c.is_ascii_digit() || c == b'.';
    let mut first_non_num: Option<usize> = None;
    let mut idx = 0;
    while idx < b.len() {
        let r = b.get(idx).copied().unwrap_or(0);
        if !is_num(r) && r != b'e' && r != b'E' && r != b'+' && r != b'-' {
            first_non_num = Some(idx);
            break;
        }
        if r == b'e' || r == b'E' {
            let next = b.get(idx + 1).copied();
            if idx == b.len() - 1 || !next.is_some_and(|c| c.is_ascii_digit() || c == b'+' || c == b'-') {
                first_non_num = Some(idx);
                break;
            }
            if matches!(next, Some(b'+' | b'-')) {
                idx += 1;
            }
        }
        idx += 1;
    }
    match first_non_num {
        None => (s, ""),
        Some(0) => ("", s),
        Some(i) => (s.get(..i).unwrap_or(""), s.get(i..).unwrap_or("")),
    }
}

/// strings.ToLower.
fn go_lower(s: &str) -> String {
    s.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

fn units_parse_bytes(_: &mut Context, args: &[Value]) -> Out {
    let raw = string_operand(arg(args, 0)?, 1)?;
    let fail = |m: &str| other(format!("units.parse_bytes: {m}"));
    let s = go_lower(raw).replace('"', "");
    if s.contains(' ') {
        return Err(fail("spaces not allowed in resource strings"));
    }
    let (num, unit) = extract_num_and_unit(&s);
    if num.is_empty() {
        return Err(fail("no byte amount provided"));
    }
    let m: u64 = match unit {
        "" => 1,
        "kb" | "k" => 1000,
        "kib" | "ki" => 1 << 10,
        "mb" | "m" => 1_000_000,
        "mib" | "mi" => 1 << 20,
        "gb" | "g" => 1_000_000_000,
        "gib" | "gi" => 1 << 30,
        "tb" | "t" => 1_000_000_000_000,
        "tib" | "ti" => 1 << 40,
        "pb" | "p" => 1_000_000_000_000_000,
        "pib" | "pi" => 1 << 50,
        "eb" | "e" => 1_000_000_000_000_000_000,
        "eib" | "ei" => 1 << 60,
        _ => return Err(fail(&format!("byte unit {unit} not recognized"))),
    };
    // big.Float.SetString: Go's own syntax errors (and an exponent out of range
    // before scaling) fail; a product past Go's exponents is ±Inf, whose Int is 0.
    let conv = || fail("could not parse byte amount to a number");
    let (_, mant, e2, _) = number::exact_parts(num).map_err(|_| conv())?;
    let bits = i64::try_from(mant.bits()).unwrap_or(i64::MAX);
    let exp2 = bits.saturating_add(e2);
    if mant.bits() != 0 && !(i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&exp2) {
        return Err(conv());
    }
    let total = Float::parse(num)
        .and_then(|f| {
            let m = Float::from_u64(m, 64)?;
            Float::mul(&f, &m, 64)
        })
        .map(|p| p.int().0)
        .unwrap_or_default();
    ok(Value::Number(Number(total.to_string().into())))
}

/// An exact rational: sign, numerator, denominator (never zero).
struct Rat {
    neg: bool,
    num: BigUint,
    den: BigUint,
}

fn gcd(a: &BigUint, b: &BigUint) -> BigUint {
    let (mut a, mut b) = (a.clone(), b.clone());
    while b.bits() != 0 {
        let r = &a % &b;
        a = b;
        b = r;
    }
    a
}

impl Rat {
    fn norm(mut self) -> Rat {
        if self.num.bits() == 0 {
            return Rat {
                neg: false,
                num: BigUint::default(),
                den: BigUint::from(1u32),
            };
        }
        let g = gcd(&self.num, &self.den);
        if g != BigUint::from(1u32) {
            self.num = &self.num / &g;
            self.den = &self.den / &g;
        }
        self
    }

    /// big.Rat.SetString for a decimal text, with Go's exponent limits.
    fn parse(s: &str) -> Option<Rat> {
        let (neg, mant, e2, e5) = number::exact_parts(s).ok()?;
        if mant.bits() == 0 {
            return Some(Rat {
                neg: false,
                num: BigUint::default(),
                den: BigUint::from(1u32),
            });
        }
        let mut num = mant;
        let mut den = BigUint::from(1u32);
        if e5 != 0 {
            let n = e5.unsigned_abs();
            if n > 1_000_000 {
                return None;
            }
            let p = BigUint::from(5u32).pow(u32::try_from(n).ok()?);
            if e5 > 0 {
                num *= p;
            } else {
                den = p;
            }
        }
        if !(-10_000_000..=10_000_000).contains(&e2) {
            return None;
        }
        if e2 > 0 {
            num <<= e2.unsigned_abs();
        } else if e2 < 0 {
            den <<= e2.unsigned_abs();
        }
        Some(Rat { neg, num, den }.norm())
    }

    /// big.Rat.SetFloat64 of a finite, positive f.
    fn from_f64(f: f64) -> Rat {
        let bits = f.to_bits();
        let frac = bits & ((1u64 << 52) - 1);
        let exp = i64::try_from((bits >> 52) & 0x7FF).unwrap_or(0);
        let (mant, e) = if exp == 0 {
            (frac, -1074)
        } else {
            (frac | (1u64 << 52), exp - 1075)
        };
        let mut num = BigUint::from(mant);
        let mut den = BigUint::from(1u32);
        if e >= 0 {
            num <<= e.unsigned_abs();
        } else {
            den <<= e.unsigned_abs();
        }
        Rat { neg: false, num, den }.norm()
    }

    fn mul(&self, o: &Rat) -> Rat {
        Rat {
            neg: self.neg != o.neg,
            num: &self.num * &o.num,
            den: &self.den * &o.den,
        }
        .norm()
    }

    fn is_int(&self) -> bool {
        self.den == BigUint::from(1u32)
    }

    fn int_text(&self) -> String {
        let sign = if self.neg { Sign::Minus } else { Sign::Plus };
        BigInt::from_biguint(sign, self.num.clone()).to_string()
    }

    /// FloatString(prec) of a non-integer.
    fn float_string(&self, prec: u32) -> String {
        let mut q = &self.num / &self.den;
        let r = &self.num % &self.den;
        let p = BigUint::from(10u32).pow(prec);
        let r = r * &p;
        let mut r1 = &r / &self.den;
        let r2 = &r % &self.den;
        let r2 = &r2 + &r2;
        if self.den <= r2 {
            r1 += 1u32;
            if r1 >= p {
                q += 1u32;
                r1 -= &p;
            }
        }
        let mut out = String::new();
        if self.neg {
            out.push('-');
        }
        out.push_str(&q.to_string());
        if prec > 0 {
            out.push('.');
            let rs = r1.to_string();
            for _ in rs.len()..usize::try_from(prec).unwrap_or(0) {
                out.push('0');
            }
            out.push_str(&rs);
        }
        out
    }
}

fn units_parse(_: &mut Context, args: &[Value]) -> Out {
    let raw = string_operand(arg(args, 0)?, 1)?;
    let fail = |m: &str| other(format!("units.parse: {m}"));
    let s = raw.replace('"', "");
    if s.contains(' ') {
        return Err(fail("spaces not allowed in resource strings"));
    }
    let (num, unit) = extract_num_and_unit(&s);
    if num.is_empty() {
        return Err(fail("no amount provided"));
    }
    // Lowercase after the first byte, to tell 'm' from 'M'.
    let unit_bytes: Vec<u8> = if unit.len() > 1 {
        let b = unit.as_bytes();
        let mut v = b.get(..1).unwrap_or_default().to_vec();
        v.extend_from_slice(go_lower(&gofmt::lossy(b.get(1..).unwrap_or_default())).as_bytes());
        v
    } else {
        unit.as_bytes().to_vec()
    };
    let x = match unit_bytes.as_slice() {
        b"m" => Rat::from_f64(0.001),
        b"" => Rat::from_f64(1.0),
        b"k" | b"K" => Rat::from_f64(1e3),
        b"ki" | b"Ki" => Rat::from_f64(1024.0),
        b"M" => Rat::from_f64(1e6),
        b"mi" | b"Mi" => Rat::from_f64(1048576.0),
        b"g" | b"G" => Rat::from_f64(1e9),
        b"gi" | b"Gi" => Rat::from_f64(1073741824.0),
        b"t" | b"T" => Rat::from_f64(1e12),
        b"ti" | b"Ti" => Rat::from_f64(1099511627776.0),
        b"p" | b"P" => Rat::from_f64(1e15),
        b"pi" | b"Pi" => Rat::from_f64(1125899906842624.0),
        b"e" | b"E" => Rat::from_f64(1e18),
        b"ei" | b"Ei" => Rat::from_f64(1152921504606846976.0),
        other_unit => {
            return Err(fail(&format!("unit {} not recognized", gofmt::lossy(other_unit))));
        }
    };
    let n = Rat::parse(num).ok_or_else(|| fail("could not parse amount to a number"))?;
    let r = n.mul(&x);
    if r.is_int() {
        return ok(Value::Number(Number(r.int_text().into())));
    }
    ok(Value::Number(Number(r.float_string(10).into())))
}

#[cfg(test)]
mod tests {
    /// go-yaml nests flow collections and indents 10000 deep (scannerc.go's
    /// max_flow_level, max_indents). Decoding and writing YAML that deep runs on a
    /// spawned thread's default 2 MiB stack, in debug builds too: the nesting is kept
    /// on stacks of its own. (A `Value` that deep is the crate's own to build and drop.)
    #[test]
    fn yaml_nests_as_deep_as_go_yaml_on_a_small_stack() {
        let run = || {
            let doc = format!("{}{}", "[".repeat(10000), "]".repeat(10000));
            assert!(super::yaml::decode(doc.as_bytes()).is_ok());
            // Block sequences in block sequences, the innermost empty.
            let want = format!("{}[]\n", "- ".repeat(9999));
            assert_eq!(super::yaml::marshal(&doc).unwrap(), want);
            let deep = format!("{}x", "- ".repeat(10001));
            assert_eq!(
                super::yaml::yaml_to_value(deep.as_bytes()).unwrap_err(),
                "yaml: exceeded max depth of 10000"
            );
        };
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(run)
            .unwrap()
            .join()
            .unwrap();
    }
}

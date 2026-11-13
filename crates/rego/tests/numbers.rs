//! number.rs against what Go's big.Float and OPA make of testdata/numbers.json
//! (testdata/numbers-oracle.json, written by scripts/rego/generate).

#![allow(clippy::unwrap_used, clippy::panic)]

use shards_rego::digits;
use shards_rego::funcs::{self, BuiltinError, Context};
use shards_rego::number::{self, Error, Float};
use shards_rego::value::{self, Value};

fn text(f: &Float) -> String {
    f.text(if f.is_int() { b'f' } else { b'g' }, u64::MAX).unwrap()
}

fn oracle() -> serde_json::Result<serde_json::Value> {
    serde_json::from_str(include_str!("../testdata/numbers-oracle.json"))
}

/// What the oracle keeps of a text too long to keep whole: its length, its first and last
/// 64 bytes, and its SHA-256.
fn digest(t: &str) -> serde_json::Value {
    use sha2::{Digest as _, Sha256};
    let sha: String = Sha256::digest(t.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    serde_json::json!({
        "len": t.len(),
        "head": t.get(..t.len().min(64)).unwrap_or_default(),
        "tail": t.get(t.len().saturating_sub(64)..).unwrap_or_default(),
        "sha256": sha,
    })
}

fn kept(c: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"len": c["len"], "head": c["head"], "tail": c["tail"], "sha256": c["sha256"]})
}

#[test]
fn long_numbers_write_as_opa_writes_them() {
    let oracle = oracle().unwrap();
    let mut failed = Vec::new();
    for c in oracle["texts"].as_array().unwrap() {
        let s = c["s"].as_str().unwrap();
        let got = digest(&text(&Float::parse(s).unwrap()));
        if got != kept(c) {
            failed.push(format!("{s}: {got}, OPA {}", kept(c)));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn abs_of_the_fuzzers_number_is_opa_s() {
    // fuzz/fuzz_targets/rego-builtins.rs's oom-4b431d4a9980bfc277352f46ea1c9d70669ec749:
    // `abs`, then its operands' JSON, `[\r80000000000e00029174680]\t`; what OPA's `abs`
    // makes of it, evaluated by topdown.
    let oracle = oracle().unwrap();
    for c in oracle["abs"].as_array().unwrap() {
        let s = c["s"].as_str().unwrap();
        let operands = value::from_json(&format!("[\r{s}]\t")).unwrap();
        let Value::Array(args) = &operands else {
            panic!("{s}: no array");
        };
        let abs = funcs::lookup("abs").unwrap();
        let v = abs(&mut Context::default(), args).unwrap().unwrap();
        assert_eq!(digest(&value::to_json(&v).unwrap()), kept(c), "abs({s})");
    }
}

#[test]
fn numbers_read_and_write_as_go_reads_and_writes_them() {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/numbers-oracle.json")).unwrap();
    let mut failed = Vec::new();
    for n in oracle["numbers"].as_array().unwrap() {
        let s = n["s"].as_str().unwrap();
        let got = Float::parse(s);
        if !n["ok"].as_bool().unwrap() {
            if got.is_ok() {
                failed.push(format!("{s:?}: read, Go refuses it"));
            }
            continue;
        }
        let want = |k: &str| n.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        // Go's ±Inf is an error here (number::OVERFLOW): OPA makes no infinities of its own.
        if want("text").ends_with("Inf") {
            if !matches!(got, Err(ref e) if *e == number::OVERFLOW) {
                failed.push(format!("{s:?}: {got:?}, Go reads it as {}", want("text")));
            }
            continue;
        }
        let Ok(f) = got else {
            failed.push(format!("{s:?}: refused, Go reads it"));
            continue;
        };
        let flag = |k: &str| n.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let (int, exact) = f.int(u64::MAX).unwrap();
        let got = (text(&f), f.is_int(), int.to_string(), exact, f.signbit());
        let wanted = (
            want("text"),
            flag("is_int"),
            want("int"),
            flag("exact"),
            flag("neg"),
        );
        if got != wanted {
            failed.push(format!("{s:?}: {got:?}, Go {wanted:?}"));
        }
    }
    for p in oracle["pairs"].as_array().unwrap() {
        let (a, b) = (p["a"].as_str().unwrap(), p["b"].as_str().unwrap());
        let (x, y) = (Float::parse(a).unwrap(), Float::parse(b).unwrap());
        let prec = Float::max_prec(&x, &y);
        let ops = [
            ("add", Float::add(&x, &y, prec)),
            ("sub", Float::sub(&x, &y, prec)),
            ("mul", Float::mul(&x, &y, prec)),
        ];
        for (op, r) in ops {
            let got = text(&r.unwrap());
            if got != p[op].as_str().unwrap() {
                failed.push(format!("{a} {op} {b}: {got}, Go {}", p[op]));
            }
        }
        if let Some(q) = p.get("quo").and_then(|v| v.as_str()) {
            let got = text(&Float::quo(&x, &y, prec).unwrap());
            if got != q {
                failed.push(format!("{a} quo {b}: {got}, Go {q}"));
            }
        }
        let compare = shards_rego::value::number_compare(a, b) as i64;
        if compare != p["compare"].as_i64().unwrap() {
            failed.push(format!(
                "NumberCompare({a}, {b}): {compare}, OPA {}",
                p["compare"]
            ));
        }
        let cmp = Float::compare(&x, &y) as i64;
        if cmp != p["cmp"].as_i64().unwrap() {
            failed.push(format!("{a} cmp {b}: {cmp}, Go {}", p["cmp"]));
        }
    }
    for q in oracle["quote"].as_array().unwrap() {
        let (s, want) = (q[0].as_str().unwrap(), q[1].as_str().unwrap());
        let mut got = String::new();
        shards_rego::goquote::quote(&mut got, s);
        if got != want {
            failed.push(format!("strconv.Quote({s:?}): {got}, Go {want}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn every_opa_builtin_is_read() {
    let r = shards_rego::builtins::registry();
    assert_eq!(r.len(), 203);
    assert_eq!(r.values().filter(|b| b.allowed).count(), 144);
}

/// Go's `Text('p', 0)`: the mantissa in hexadecimal, normalized to at least a half and
/// below one, its trailing zeros cut, and the binary exponent (ftoa.go's fmtP).
fn text_p(f: &Float) -> String {
    let sign = if f.signbit() { "-" } else { "" };
    if f.is_zero() {
        return format!("{sign}0");
    }
    let m = f.mantissa();
    let hex = (m << ((4 - m.bits() % 4) % 4)).to_str_radix(16);
    let e = f.go_exponent();
    let plus = if e >= 0 { "+" } else { "" };
    format!("{sign}0x.{}p{plus}{e}", hex.trim_end_matches('0'))
}

#[test]
fn numbers_read_as_go_reads_them_to_its_exponents_ends() {
    // Go reads 1e536870939 through 1e646456992 as finite, and 1e-x past them as 0, its
    // pow5 squaring once past the last bit it uses; this port refused them all before.
    let oracle = oracle().unwrap();
    let mut failed = Vec::new();
    for r in oracle["reads"].as_array().unwrap() {
        let s = r["s"].as_str().unwrap();
        let got = Float::parse(s);
        let ok = r["ok"].as_bool().unwrap();
        let inf = r.get("inf").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let wrong = match (ok, inf, &got) {
            (false, _, Ok(f)) => Some(format!("read as {}, Go refuses it", text_p(f))),
            (false, _, Err(_)) => None,
            // Go's ±Inf, which this port refuses (D98's kept differences).
            (true, true, Err(Error::Go("exponent overflow"))) => None,
            (true, true, _) => Some(format!("{got:?}, Go's ±Inf")),
            (true, false, Err(e)) => Some(format!("refused ({e}), Go reads it")),
            (true, false, Ok(f)) => {
                let want = r["p"].as_str().unwrap();
                (text_p(f) != want).then(|| format!("{}, Go {want}", text_p(f)))
            }
        };
        if let Some(w) = wrong {
            failed.push(format!("{s}: {w}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// A builtin's answer as the oracle keeps it: its value's text (a number's, a string's
/// unquoted, an array's numbers' with a comma between) as a digest, or its error's text
/// as topdown's error for the builtin says it.
fn answer(name: &str, got: &Result<Option<Value>, BuiltinError>) -> serde_json::Value {
    match got {
        Ok(Some(Value::Number(n))) => digest(n.text()),
        Ok(Some(Value::String(s))) => digest(s),
        Ok(Some(Value::Array(a))) if a.iter().all(|v| matches!(v, Value::Number(_))) => {
            let texts: Vec<&str> = a
                .iter()
                .filter_map(|v| match v {
                    Value::Number(n) => Some(n.text()),
                    _ => None,
                })
                .collect();
            digest(&texts.join(","))
        }
        Ok(Some(v)) => serde_json::json!({ "other": format!("{v:?}") }),
        Ok(None) => serde_json::json!({ "error": "undefined" }),
        Err(BuiltinError::Operand(m)) => {
            serde_json::json!({ "error": format!("eval_type_error: {name}: {m}") })
        }
        Err(BuiltinError::Other(m)) => {
            serde_json::json!({ "error": format!("eval_builtin_error: {name}: {m}") })
        }
        Err(BuiltinError::Halt(m)) => serde_json::json!({ "error": m }),
    }
}

fn call(name: &str, args: &str, ctx: &mut Context) -> Result<Option<Value>, BuiltinError> {
    let operands = value::from_json(args).unwrap();
    let Value::Array(args) = &operands else {
        return Err(BuiltinError::Other(format!("{name}({args}): no array")));
    };
    funcs::lookup(name).unwrap()(ctx, args)
}

#[test]
fn builtins_answer_long_numbers_as_opa_does() {
    // floor, ceil, round, format_int, bits, rem and units of numbers whose answers run
    // to millions of digits, the fuzzer's among them: OPA's own answers, byte for byte.
    let oracle = oracle().unwrap();
    let mut failed = Vec::new();
    for c in oracle["calls"].as_array().unwrap() {
        let (name, args) = (c["name"].as_str().unwrap(), c["args"].as_str().unwrap());
        let got = answer(name, &call(name, args, &mut Context::default()));
        let want = match c.get("error") {
            Some(e) => serde_json::json!({ "error": e }),
            None => kept(c),
        };
        if got != want {
            failed.push(format!("{name}{args}: {got}, OPA {want}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn a_text_past_what_the_evaluation_holds_is_refused_by_name() {
    // The fuzzer's number, as fuzz/fuzz_targets/rego-builtins.rs read it: each of floor,
    // ceil, round and format_int answers every digit of its 96,916,226-bit integer,
    // abs its shortest decimal, 29,174,691 digits. With the room the bound asks, OPA's
    // answer; with a byte less, refused, naming the size and the limit.
    let oracle = oracle().unwrap();
    let want = |name: &str| {
        let fz = "[80000000000e00029174680]";
        oracle["calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name && c["args"].as_str().unwrap().starts_with(fz.trim_end_matches(']')))
            .map(kept)
            .unwrap()
    };
    let bits = 96_916_226;
    let whole = digits::need(bits);
    let shortest = 2 * 29_174_691;
    for (name, args, need) in [
        ("floor", "[80000000000e00029174680]", whole),
        ("round", "[80000000000e00029174680]", whole),
        ("format_int", "[80000000000e00029174680, 10]", whole),
        ("abs", "[80000000000e00029174680]", shortest),
    ] {
        let mut room = Context {
            holdable: need,
            ..Context::default()
        };
        let got = answer(name, &call(name, args, &mut room));
        let wanted = if name == "abs" {
            kept(&oracle["abs"][0])
        } else {
            want(name)
        };
        assert_eq!(got, wanted, "{name} with its need's room");
        let mut tight = Context {
            holdable: need - 1,
            ..Context::default()
        };
        let e = call(name, args, &mut tight).unwrap_err();
        assert_eq!(
            e.message(),
            format!(
                "number too large to write: the text of a {bits}-bit number takes {need} bytes \
                 to make, more than the {} bytes this evaluation can hold",
                need - 1
            ),
            "{name}"
        );
    }
    // ceil of its negation, the same digits after a sign.
    let mut tight = Context {
        holdable: whole - 1,
        ..Context::default()
    };
    assert!(call("ceil", "[-80000000000e00029174680]", &mut tight).is_err());
    // A shift past what any host holds is refused before anything is made.
    let mut ctx = Context {
        holdable: 1 << 40,
        ..Context::default()
    };
    let e = call("bits.lsh", "[1, 1e30]", &mut ctx).unwrap_err();
    assert!(
        e.message().starts_with("number too large to write"),
        "{}",
        e.message()
    );
    let e = call(
        "bits.or",
        "[1e646456992, 1]",
        &mut Context {
            holdable: 1 << 30,
            ..Context::default()
        },
    )
    .unwrap_err();
    assert!(
        e.message().starts_with("number too large to write"),
        "{}",
        e.message()
    );
}

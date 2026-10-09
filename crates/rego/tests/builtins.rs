//! Each ported builtin against what OPA's own function returns for the calls in
//! testdata/calls-*.json (testdata/calls-*-oracle.json, written by
//! scripts/rego/generate): its result, undefined, or its error's text. Values are JSON
//! with sets as {"$set": [...]} and objects keyed by other than strings as
//! {"$object": [[k, v], ...]}.

use std::collections::{BTreeMap, BTreeSet};

use shards_rego::funcs::{self, BuiltinError, Context};
use shards_rego::value::{self, Value};

fn decode(v: Value) -> Value {
    match v {
        Value::Array(a) => Value::array(a.iter().cloned().map(decode).collect()),
        Value::Object(o) => {
            if o.len() == 1 {
                if let Some(Value::Array(items)) = o.get(&Value::string("$set")) {
                    return Value::set(items.iter().cloned().map(decode).collect::<BTreeSet<_>>());
                }
                if let Some(Value::Array(pairs)) = o.get(&Value::string("$object")) {
                    let mut m = BTreeMap::new();
                    for p in pairs.iter() {
                        if let Value::Array(kv) = p
                            && let [k, v] = kv.as_slice()
                        {
                            m.insert(decode(k.clone()), decode(v.clone()));
                        }
                    }
                    return Value::object(m);
                }
            }
            Value::object(
                o.iter()
                    .map(|(k, v)| (decode(k.clone()), decode(v.clone())))
                    .collect(),
            )
        }
        v => v,
    }
}

/// As the oracle's encode writes a value.
fn encode(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(n.text()),
        Value::String(s) => value::write_json_string(out, s),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode(out, x);
            }
            out.push(']');
        }
        Value::Set(s) => {
            out.push_str("{\"$set\":[");
            for (i, x) in s.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                encode(out, x);
            }
            out.push_str("]}");
        }
        Value::Object(o) => {
            if o.keys().all(|k| matches!(k, Value::String(_))) {
                out.push('{');
                for (i, (k, x)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    encode(out, k);
                    out.push(':');
                    encode(out, x);
                }
                out.push('}');
            } else {
                out.push_str("{\"$object\":[");
                for (i, (k, x)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('[');
                    encode(out, k);
                    out.push(',');
                    encode(out, x);
                    out.push(']');
                }
                out.push_str("]}");
            }
        }
    }
}

fn error_text(name: &str, e: &BuiltinError) -> String {
    match e {
        BuiltinError::Operand(m) => format!("p.rego:1: eval_type_error: {name}: {m}"),
        BuiltinError::Other(m) => format!("p.rego:1: eval_builtin_error: {name}: {m}"),
        BuiltinError::Halt(m) => m.clone(),
    }
}

#[test]
fn builtins_answer_as_opas_answer() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_str().unwrap();
            n.starts_with("calls-") && n.ends_with("-oracle.json")
        })
        .collect();
    files.sort();
    let (mut failed, mut missing, mut total) = (Vec::new(), BTreeSet::new(), 0);
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let Value::Array(calls) = value::from_json(&text).unwrap() else {
            panic!()
        };
        for c in calls.iter() {
            total += 1;
            let name = c
                .get(&Value::string("name"))
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            let Some(f) = funcs::lookup(&name) else {
                missing.insert(name);
                continue;
            };
            let Value::Array(args) = c.get(&Value::string("args")).unwrap().clone() else {
                panic!()
            };
            let args: Vec<Value> = args.iter().cloned().map(decode).collect();
            let mut ctx = Context {
                time_ns: 1_700_000_000_000_000_000,
                seed: vec![0; 1024],
                ..Context::default()
            };
            let got = match f(&mut ctx, &args) {
                Ok(Some(v)) => {
                    let mut s = String::new();
                    encode(&mut s, &v);
                    format!("result {s}")
                }
                Ok(None) => "undefined".to_string(),
                Err(e) => format!("error {}", error_text(&name, &e)),
            };
            let want = if let Some(r) = c.get(&Value::string("result")) {
                let mut s = String::new();
                encode(&mut s, &decode(r.clone()));
                format!("result {s}")
            } else if let Some(e) = c.get(&Value::string("error")) {
                format!("error {}", e.as_str().unwrap())
            } else {
                "undefined".to_string()
            };
            if got != want {
                let mut a = String::new();
                encode(&mut a, &Value::array(args));
                failed.push(format!("{name}{a}:\n  got  {got}\n  OPA  {want}"));
            }
        }
    }
    assert!(
        failed.is_empty() && missing.is_empty(),
        "{} of {total} calls differ, builtins not ported: {missing:?}\n{}",
        failed.len(),
        failed.join("\n")
    );
}

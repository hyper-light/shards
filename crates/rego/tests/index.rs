//! The rule index against OPA's (testdata/index-oracle.json, written by
//! scripts/rego/generate): for each case of testdata/index.json, its modules beside
//! buildx's builtins.rego compiled, and for each lookup the index at its path, the rules
//! Lookup finds with a resolver over the lookup's documents, and AllRules.
//!
//! OPA's index ranges over Go maps, whose order is random per run; the oracle records
//! every distinct answer OPA gave over 100 compilers, and the index's answer must be one
//! of them (where OPA gave one answer, that one).

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use shards_rego::ast::{RuleKind, Term, TermValue};
use shards_rego::compile::{Compiler, Function, RuleNode};
use shards_rego::index::{IndexResult, Resolved};
use shards_rego::parser::parse_module;
use shards_rego::types::Type;
use shards_rego::value::{Number, Value, from_json};

fn host() -> Vec<Function> {
    let s = || Type::String;
    let a = || Type::Any(Vec::new());
    let f = |args: Vec<Type>, result: Type| Type::Function {
        args,
        result: Some(Box::new(result)),
        variadic: None,
    };
    vec![
        Function {
            name: "load_json".into(),
            decl: f(vec![s()], a()),
        },
        Function {
            name: "verify_git_signature".into(),
            decl: f(vec![a(), s()], Type::Boolean),
        },
        Function {
            name: "verify_http_pgp_signature".into(),
            decl: f(vec![a(), s(), s()], Type::Boolean),
        },
        Function {
            name: "pin_image".into(),
            decl: f(vec![a(), s()], Type::Boolean),
        },
        Function {
            name: "artifact_attestation".into(),
            decl: f(vec![a(), s()], a()),
        },
        Function {
            name: "github_attestation".into(),
            decl: f(vec![a(), s()], a()),
        },
    ]
}

/// A dotted path (`data.docker.allow`) as a ref.
fn path(s: &str) -> Vec<Term> {
    let mut parts = s.split('.');
    let mut out = vec![Term::var(parts.next().unwrap(), None)];
    out.extend(parts.map(|p| Term::string(p, None)));
    out
}

fn name(id: &RuleNode) -> String {
    if id.2 == 0 {
        format!("{}#{}", id.0, id.1)
    } else {
        format!("{}#{}.{}", id.0, id.1, id.2)
    }
}

fn answer(r: &IndexResult) -> Json {
    let mut elses = Vec::new();
    for rule in &r.rules {
        if let Some(es) = r.else_.get(rule) {
            elses.push(json!({"rule": name(rule), "else": es.iter().map(name).collect::<Vec<_>>()}));
        }
    }
    let mut out = json!({
        "rules": r.rules.iter().map(name).collect::<Vec<_>>(),
        "else": elses,
        "kind": match r.kind { RuleKind::SingleValue => "single", RuleKind::MultiValue => "multi" },
        "early_exit": r.early_exit,
        "only_ground_refs": r.only_ground_refs,
    });
    if let Some(d) = &r.default {
        out["default"] = json!(name(d));
    }
    out
}

fn key(t: &Term) -> Option<Value> {
    match &t.value {
        TermValue::Null => Some(Value::Null),
        TermValue::Bool(b) => Some(Value::Bool(*b)),
        TermValue::Number(n) => Some(Value::Number(Number(n.text().into()))),
        TermValue::String(s) => Some(Value::String(s.clone())),
        _ => None,
    }
}

/// Value.Find: undefined where the path leaves the document.
fn find(doc: &Value, path: &[Term]) -> Option<Value> {
    let mut v = doc;
    for t in path {
        v = v.get(&key(t)?)?;
    }
    Some(v.clone())
}

/// The oracle's resolver (indexResolver).
fn resolve(l: &Json, r: &[Term]) -> Result<Resolved<Value>, String> {
    for u in l["unknowns"].as_array().into_iter().flatten() {
        let u = path(u.as_str().unwrap());
        if u.len() <= r.len() && u.iter().zip(r).all(|(a, b)| a.equal(b)) {
            return Ok(Resolved::Unknown);
        }
    }
    let doc = |k: &str| l.get(k).map(|d| from_json(&d.to_string()).unwrap());
    match r.first().and_then(Term::as_var) {
        Some("args") => {
            let args = l["args"].as_array().cloned().unwrap_or_default();
            let i = match r.get(1).map(|t| &t.value) {
                Some(TermValue::Number(n)) => n.as_i64().and_then(|i| usize::try_from(i).ok()),
                _ => None,
            };
            match i.and_then(|i| args.get(i)) {
                Some(a) => Ok(Resolved::Value(from_json(&a.to_string()).unwrap())),
                None => Ok(Resolved::Unknown),
            }
        }
        Some(root @ ("input" | "data")) => {
            let Some(d) = doc(root) else {
                return Ok(Resolved::Undefined);
            };
            Ok(find(&d, &r[1..]).map_or(Resolved::Undefined, Resolved::Value))
        }
        _ => Err("illegal ref".into()),
    }
}

#[test]
fn lookups_find_the_rules_opa_finds() {
    let cases: Json = serde_json::from_str(include_str!("../testdata/index.json")).unwrap();
    let oracle: Json = serde_json::from_str(include_str!("../testdata/index-oracle.json")).unwrap();
    let (mut failed, mut total) = (Vec::new(), 0);
    for (c, want) in cases.as_array().unwrap().iter().zip(oracle.as_array().unwrap()) {
        let cname = c["name"].as_str().unwrap();
        assert_eq!(cname, want["name"].as_str().unwrap());
        assert!(want.get("error").is_none(), "{cname}: OPA: {}", want["error"]);
        let mut modules = BTreeMap::new();
        let defaults = include_str!("../src/buildx_defaults.rego");
        modules.insert(
            "builtin/buildx_defaults.rego".to_string(),
            parse_module("builtin/buildx_defaults.rego", defaults).unwrap(),
        );
        for m in c["modules"].as_array().unwrap() {
            let file = m[0].as_str().unwrap();
            modules.insert(
                file.to_string(),
                parse_module(file, m[1].as_str().unwrap()).unwrap(),
            );
        }
        let mut comp = Compiler::new(modules, host(), true);
        comp.compile();
        assert!(comp.errors.is_empty(), "{cname}: {:?}", comp.errors);
        for (i, (l, w)) in c["lookups"]
            .as_array()
            .unwrap()
            .iter()
            .zip(want["lookups"].as_array().unwrap())
            .enumerate()
        {
            total += 1;
            let p = l["path"].as_str().unwrap();
            let index = comp.rule_index(&path(p));
            let mut got = json!({"path": p, "index": index.is_some()});
            if let Some(index) = index {
                match index.lookup(&mut |r: &[Term]| resolve(l, r)) {
                    Ok(res) => got["lookup"] = answer(&res),
                    Err(e) => got["error"] = json!(e),
                }
                got["all"] = answer(&index.all_rules());
            }
            let seen = |k: &str| -> bool {
                match (got.get(k), w.get(k)) {
                    (None, None) => true,
                    (Some(g), Some(Json::Array(outcomes))) => outcomes.contains(g),
                    _ => false,
                }
            };
            if got["index"] != w["index"]
                || got.get("error") != w.get("error")
                || !seen("lookup")
                || !seen("all")
            {
                failed.push(format!("--- {cname} lookup {i} ({p})\ngot: {got}\nOPA: {w}"));
            }
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {total} lookups differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

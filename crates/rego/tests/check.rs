//! The type checker against what OPA's makes of testdata/check.json's modules beside
//! buildx's builtins.rego (testdata/check-oracle.json, written by scripts/rego/generate):
//! the compile errors, or none, and for each body whether PassesTypeCheck passes it.
//!
//! OPA's checker walks Go maps (the rule graph's nodes, a type node's children), so a
//! case can have several answers: the oracle compiles each case 20 times and keeps every
//! distinct answer, and a case matches when the answer here is one of them.

use std::collections::BTreeMap;

use shards_rego::compile::{Compiler, Function};
use shards_rego::parser::{Statement, parse_module, parse_statements};
use shards_rego::types::Type;

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

#[test]
fn types_check_as_opa_checks_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/check.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/check-oracle.json")).unwrap();
    let cases = cases.as_array().unwrap();
    let oracle = oracle.as_array().unwrap();
    assert_eq!(cases.len(), oracle.len());
    let mut failed = Vec::new();
    for (c, want) in cases.iter().zip(oracle) {
        let name = c["name"].as_str().unwrap();
        assert_eq!(name, want["name"].as_str().unwrap());
        let mut srcs = vec![(
            "builtin/buildx_defaults.rego".to_string(),
            include_str!("../src/buildx_defaults.rego").to_string(),
        )];
        for m in c["modules"].as_array().unwrap() {
            srcs.push((
                m[0].as_str().unwrap().to_string(),
                m[1].as_str().unwrap().to_string(),
            ));
        }
        let mut modules = BTreeMap::new();
        for (file, src) in &srcs {
            modules.insert(file.clone(), parse_module(file, src).unwrap());
        }
        let mut comp = Compiler::new(modules, host(), true);
        comp.compile();
        let mut got = serde_json::Map::new();
        if comp.errors.is_empty() {
            let mut passes = Vec::new();
            for b in c.get("bodies").and_then(|b| b.as_array()).into_iter().flatten() {
                let stmts = parse_statements("", b.as_str().unwrap()).unwrap();
                let [Statement::Body(body)] = stmts.as_slice() else {
                    panic!("{name}: {b} is not a body")
                };
                passes.push(serde_json::Value::Bool(comp.passes_type_check(body)));
            }
            if !passes.is_empty() {
                got.insert("passes".into(), passes.into());
            }
        } else {
            let text = match comp.errors.as_slice() {
                [e] => format!("1 error occurred: {e}"),
                es => format!(
                    "{} errors occurred:\n{}",
                    es.len(),
                    es.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
                ),
            };
            got.insert("error".into(), text.into());
        }
        let got = serde_json::Value::Object(got);
        let answers = want["answers"].as_array().unwrap();
        if !answers.contains(&got) {
            failed.push(format!(
                "--- {name}\ngot:\n{}\nOPA:\n{}",
                show(&got),
                answers.iter().map(show).collect::<Vec<_>>().join("\n-- or\n")
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} differ:\n{}",
        failed.len(),
        cases.len(),
        failed.join("\n")
    );
}

fn show(v: &serde_json::Value) -> String {
    match v.get("error") {
        Some(e) => e.as_str().unwrap().to_string(),
        None => format!(
            "ok {}",
            v.get("passes").map(ToString::to_string).unwrap_or_default()
        ),
    }
}

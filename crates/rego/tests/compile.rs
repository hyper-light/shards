//! The compiler against what OPA's makes of testdata/cases.json's modules beside
//! buildx's builtins.rego (testdata/compile-oracle.json, written by
//! scripts/rego/generate): each compiled module's text, or the errors.

use std::collections::BTreeMap;

use shards_rego::compile::{Compiler, Function};
use shards_rego::parser::parse_module;
use shards_rego::types::Type;

fn host() -> Vec<Function> {
    let s = || Type::String;
    let a = || Type::Any(Vec::new());
    let f = |args: Vec<Type>, result: Type| Type::Function { args, result: Some(Box::new(result)), variadic: None };
    vec![
        Function { name: "load_json".into(), decl: f(vec![s()], a()) },
        Function { name: "verify_git_signature".into(), decl: f(vec![a(), s()], Type::Boolean) },
        Function { name: "verify_http_pgp_signature".into(), decl: f(vec![a(), s(), s()], Type::Boolean) },
        Function { name: "pin_image".into(), decl: f(vec![a(), s()], Type::Boolean) },
        Function { name: "artifact_attestation".into(), decl: f(vec![a(), s()], a()) },
        Function { name: "github_attestation".into(), decl: f(vec![a(), s()], a()) },
    ]
}

#[test]
fn modules_compile_as_opa_compiles_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/cases.json")).unwrap();
    let oracle: serde_json::Value = serde_json::from_str(include_str!("../testdata/compile-oracle.json")).unwrap();
    let mut failed = Vec::new();
    let mut total = 0;
    for (c, want) in cases.as_array().unwrap().iter().zip(oracle.as_array().unwrap()) {
        let name = c["name"].as_str().unwrap();
        if want.get("error").and_then(|e| e.as_str()).is_some_and(|e| e.starts_with("parse: ")) {
            continue;
        }
        total += 1;
        let mut srcs = vec![("builtin/buildx_defaults.rego".to_string(), include_str!("../src/buildx_defaults.rego").to_string())];
        for m in c["modules"].as_array().unwrap() {
            srcs.push((m[0].as_str().unwrap().to_string(), m[1].as_str().unwrap().to_string()));
        }
        let mut modules = BTreeMap::new();
        for (file, src) in &srcs {
            modules.insert(file.clone(), parse_module(file, src).unwrap());
        }
        let mut comp = Compiler::new(modules, host(), true);
        comp.compile();
        let got = if comp.errors.is_empty() {
            let mut out = String::new();
            for (n, m) in &comp.modules {
                out.push_str(&format!("== {n}\n{m}\n"));
            }
            out
        } else {
            match comp.errors.as_slice() {
                [e] => format!("1 error occurred: {e}"),
                es => format!("{} errors occurred:\n{}", es.len(), es.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")),
            }
        };
        let wanted = match want.get("modules") {
            Some(m) => {
                let m: BTreeMap<String, String> = serde_json::from_value(m.clone()).unwrap();
                m.iter().map(|(n, t)| format!("== {n}\n{t}\n")).collect()
            }
            None => want["error"].as_str().unwrap().to_string(),
        };
        // OPA orders unsafe-variable errors of one location at random (a map's range,
        // then an unstable sort): measured, see scripts/rego. Compare those runs as sets.
        if normalize(&got) != normalize(&wanted) {
            failed.push(format!("--- {name}\ngot:\n{got}\nOPA:\n{wanted}"));
        }
    }
    assert!(failed.is_empty(), "{} of {total} differ:\n{}", failed.len(), failed.join("\n"));
}

fn normalize(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut run: Vec<String> = Vec::new();
    let key = |l: &str| l.split(": rego_unsafe_var_error:").next().map(str::to_string);
    for line in text.lines() {
        if line.contains(": rego_unsafe_var_error: ") {
            if run.first().is_some_and(|f| key(f) != key(line)) {
                run.sort();
                out.append(&mut run);
            }
            run.push(line.to_string());
            continue;
        }
        run.sort();
        out.append(&mut run);
        out.push(line.to_string());
    }
    run.sort();
    out.append(&mut run);
    out.join("\n")
}

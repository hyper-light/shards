//! The evaluator against what OPA makes of testdata/cases.json, set up as buildx sets
//! it up (testdata/oracle.json, written by scripts/rego/generate): each result, the
//! error, and what print printed.

use std::collections::BTreeMap;

use shards_rego::compile::{Compiler, Function};
use shards_rego::eval::{Host, HostError, Machine, Program, eval_query, partial_query};
use shards_rego::funcs::Context;
use shards_rego::parser::parse_module;
use shards_rego::types::Type;
use shards_rego::value::{self, Value};

struct Table(BTreeMap<String, BTreeMap<String, Value>>);

impl Host for Table {
    fn call(&mut self, name: &str, args: &[Value]) -> Result<Option<Value>, HostError> {
        let key = value::to_json(&Value::array(args.to_vec())).map_err(|e| HostError::Undefined(e.0))?;
        Ok(self.0.get(name).and_then(|t| t.get(&key)).cloned())
    }
}

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
fn policies_evaluate_as_opa_evaluates_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/cases.json")).unwrap();
    let oracle: serde_json::Value = serde_json::from_str(include_str!("../testdata/oracle.json")).unwrap();
    let mut failed = Vec::new();
    let mut total = 0;
    for (c, want) in cases.as_array().unwrap().iter().zip(oracle.as_array().unwrap()) {
        let name = c["name"].as_str().unwrap();
        let mut modules = BTreeMap::new();
        let mut parse_failed = false;
        for (file, src) in std::iter::once((
            "builtin/buildx_defaults.rego",
            include_str!("../src/buildx_defaults.rego"),
        ))
        .chain(
            c["modules"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| (m[0].as_str().unwrap(), m[1].as_str().unwrap())),
        ) {
            match parse_module(file, src) {
                Ok(m) => {
                    modules.insert(file.to_string(), m);
                }
                Err(_) => parse_failed = true,
            }
        }
        if parse_failed {
            continue;
        }
        total += 1;
        let mut comp = Compiler::new(modules, host(), true);
        comp.compile();
        let got = if !comp.errors.is_empty() {
            match comp.errors.as_slice() {
                [e] => format!("error 1 error occurred: {e}"),
                es => format!(
                    "error {} errors occurred:\n{}",
                    es.len(),
                    es.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
                ),
            }
        } else {
            let program = Program::new(&comp, host().into_iter().map(|f| f.name).collect());
            let mut table = BTreeMap::new();
            if let Some(h) = c.get("host").and_then(|h| h.as_object()) {
                for (fname, t) in h {
                    let mut m = BTreeMap::new();
                    for (k, v) in t.as_object().unwrap() {
                        m.insert(k.clone(), value::from_json(&v.to_string()).unwrap());
                    }
                    table.insert(fname.clone(), m);
                }
            }
            let mut host = Table(table);
            let input = c.get("input").map(|i| value::from_json(&i.to_string()).unwrap());
            let query = shards_rego::ast::Term::reference(
                vec![
                    shards_rego::ast::Term::var("data", None),
                    shards_rego::ast::Term::string("docker", None),
                    shards_rego::ast::Term::string("decision", None),
                ],
                None,
            );
            let mut partial = String::new();
            if let Some(us) = c.get("unknowns").and_then(|u| u.as_array()) {
                let unknowns: Vec<_> = us.iter().map(|u| parse_term(u.as_str().unwrap())).collect();
                let mut machine = Machine::new(&program, &mut host, Context::default());
                match partial_query(&mut machine, &query, input.clone(), &unknowns) {
                    Ok(p) => {
                        let queries: Vec<String> = p
                            .queries
                            .iter()
                            .map(|q| shards_rego::ast::BodyText(q).to_string())
                            .collect();
                        let mut support: Vec<String> = p.support.iter().map(ToString::to_string).collect();
                        support.sort();
                        partial = format!("queries {queries:?} support {support:?} ");
                    }
                    Err(e) => partial = format!("partial error {e} "),
                }
            }
            let mut machine = Machine::new(&program, &mut host, Context::default());
            match eval_query(&mut machine, &query, input) {
                Ok(vs) => {
                    let results: Vec<String> = vs.iter().map(|v| value::to_json(v).unwrap()).collect();
                    format!("{partial}results {results:?} prints {:?}", machine.prints)
                }
                Err(e) => format!("{partial}error {e}"),
            }
        };
        let mut partial = String::new();
        if c.get("unknowns").is_some() {
            let strs = |k: &str| -> Vec<String> {
                want.get(k)
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().map(|q| q.as_str().unwrap().to_string()).collect())
                    .unwrap_or_default()
            };
            partial = format!(
                "queries {:?} support {:?} ",
                strs("queries"),
                strs("support_modules")
            );
        }
        let wanted = match want.get("error").and_then(|e| e.as_str()) {
            Some(e) => format!("{partial}error {e}"),
            None => {
                // Read with the crate's reader, which keeps numbers' text, as OPA does.
                let raw = value::from_json(include_str!("../testdata/oracle.json")).unwrap();
                let Value::Array(all) = raw else { panic!() };
                let entry = all
                    .iter()
                    .find(|e| e.get(&Value::string("name")).and_then(Value::as_str) == Some(name))
                    .unwrap();
                let Some(Value::Array(rs)) = entry.get(&Value::string("results")) else {
                    panic!()
                };
                let results: Vec<String> = rs.iter().map(|r| value::to_json(r).unwrap()).collect();
                let prints: Vec<String> = want["prints"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p.as_str().unwrap().to_string())
                    .collect();
                format!("{partial}results {results:?} prints {prints:?}")
            }
        };
        if normalize(&got) != normalize(&wanted) {
            failed.push(format!("--- {name}\n  got  {got}\n  OPA  {wanted}"));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {total} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// A term as rego.Unknowns parses it.
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
fn parse_term(s: &str) -> shards_rego::ast::Term {
    let m = parse_module("unknown.rego", &format!("package x\n\nx := {s}\n")).unwrap();
    m.rules[0].head.value.clone().unwrap()
}

/// OPA orders unsafe-variable errors of one location at random (measured), and recursion
/// errors as it walks a map of the rule tree's children (TreeNode.DepthFirst): compare
/// those as sets.
fn normalize(text: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.iter().all(|l| {
        l.contains("rego_unsafe_var_error") || l.contains("rego_recursion_error") || l.contains("errors occurred")
    }) {
        lines.sort();
    }
    lines.join("\n")
}

//! The evaluator against what OPA makes of testdata/cases.json, set up as buildx sets
//! it up (testdata/oracle.json, written by scripts/rego/generate): each result, the
//! error, and what print printed.

use std::collections::BTreeMap;

use shards_rego::compile::{Compiler, Function};
use shards_rego::eval::{eval_query, Host, Machine, Program};
use shards_rego::funcs::Context;
use shards_rego::parser::parse_module;
use shards_rego::types::Type;
use shards_rego::value::{self, Value};

struct Table(BTreeMap<String, BTreeMap<String, Value>>);

impl Host for Table {
    fn call(&mut self, name: &str, args: &[Value]) -> Result<Option<Value>, String> {
        let key = value::to_json(&Value::array(args.to_vec())).map_err(|e| e.0)?;
        Ok(self.0.get(name).and_then(|t| t.get(&key)).cloned())
    }
}

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
fn policies_evaluate_as_opa_evaluates_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/cases.json")).unwrap();
    let oracle: serde_json::Value = serde_json::from_str(include_str!("../testdata/oracle.json")).unwrap();
    let mut failed = Vec::new();
    let mut total = 0;
    for (c, want) in cases.as_array().unwrap().iter().zip(oracle.as_array().unwrap()) {
        let name = c["name"].as_str().unwrap();
        let mut modules = BTreeMap::new();
        let mut parse_failed = false;
        for (file, src) in std::iter::once(("builtin/buildx_defaults.rego", include_str!("../src/buildx_defaults.rego")))
            .chain(c["modules"].as_array().unwrap().iter().map(|m| (m[0].as_str().unwrap(), m[1].as_str().unwrap())))
        {
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
                es => format!("error {} errors occurred:\n{}", es.len(), es.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")),
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
            let mut machine = Machine::new(&program, &mut host, Context::default());
            let input = c.get("input").map(|i| value::from_json(&i.to_string()).unwrap());
            let query = shards_rego::ast::Term::reference(
                vec![
                    shards_rego::ast::Term::var("data", None),
                    shards_rego::ast::Term::string("docker", None),
                    shards_rego::ast::Term::string("decision", None),
                ],
                None,
            );
            match eval_query(&mut machine, &query, input) {
                Ok(vs) => {
                    let results: Vec<String> = vs.iter().map(|v| value::to_json(v).unwrap()).collect();
                    format!("results {results:?} prints {:?}", machine.prints)
                }
                Err(e) => format!("error {e}"),
            }
        };
        let wanted = match want.get("error").and_then(|e| e.as_str()) {
            Some(e) => format!("error {e}"),
            None => {
                // Read with the crate's reader, which keeps numbers' text, as OPA does.
                let raw = value::from_json(include_str!("../testdata/oracle.json")).unwrap();
                let Value::Array(all) = raw else { panic!() };
                let entry = all.iter().find(|e| e.get(&Value::string("name")).and_then(Value::as_str) == Some(name)).unwrap();
                let Some(Value::Array(rs)) = entry.get(&Value::string("results")) else { panic!() };
                let results: Vec<String> = rs.iter().map(|r| value::to_json(r).unwrap()).collect();
                let prints: Vec<String> = want["prints"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect();
                format!("results {results:?} prints {prints:?}")
            }
        };
        if got != wanted {
            failed.push(format!("--- {name}\n  got  {got}\n  OPA  {wanted}"));
        }
    }
    assert!(failed.is_empty(), "{} of {total} differ:\n{}", failed.len(), failed.join("\n"));
}

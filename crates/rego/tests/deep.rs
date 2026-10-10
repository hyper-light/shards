//! Values deeper than any document reads: JSON and YAML nest at most 10000 levels, but
//! json.patch puts a value inside another at any depth, and OPA v1.14.1 builds, compares,
//! writes and drops what it makes, on Go's stack, which grows. shards does the same on
//! the policy thread's fixed stack (123 MiB, M126): what walks a value walks it without
//! a frame a level.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use shards_rego::ast::Term;
use shards_rego::compile::{Compiler, Function};
use shards_rego::eval::{Host, HostError, Machine, Program, eval_query};
use shards_rego::funcs::Context;
use shards_rego::parser::parse_module;
use shards_rego::types::Type;
use shards_rego::value::{self, Value};

/// crates/shards' `policy::STACK`.
const POLICY_STACK: usize = 123 << 20;

struct NoHost;

impl Host for NoHost {
    fn call(&mut self, _: &str, _: &[Value]) -> Result<Option<Value>, HostError> {
        Ok(None)
    }
}

fn host() -> Vec<Function> {
    let s = || Type::String;
    let a = || Type::Any(Vec::new());
    let f = |name: &str, args: Vec<Type>, result: Type| Function {
        name: name.into(),
        decl: Type::Function {
            args,
            result: Some(Box::new(result)),
            variadic: None,
        },
    };
    vec![
        f("load_json", vec![s()], a()),
        f("verify_git_signature", vec![a(), s()], Type::Boolean),
        f("verify_http_pgp_signature", vec![a(), s(), s()], Type::Boolean),
        f("pin_image", vec![a(), s()], Type::Boolean),
        f("artifact_attestation", vec![a(), s()], a()),
        f("github_attestation", vec![a(), s()], a()),
    ]
}

/// `data.docker.decision`'s results as JSON, or the error, on the policy thread.
fn decide(src: String) -> Result<Vec<String>, String> {
    std::thread::Builder::new()
        .stack_size(POLICY_STACK)
        .spawn(move || {
            let mut modules = BTreeMap::new();
            modules.insert(
                "policy.rego".to_string(),
                parse_module("policy.rego", &src).map_err(|e| e.to_string())?,
            );
            let mut comp = Compiler::new(modules, host(), true);
            comp.compile();
            if let Some(e) = comp.errors.first() {
                return Err(e.to_string());
            }
            let program = Program::new(&comp, host().into_iter().map(|f| f.name).collect());
            let mut host = NoHost;
            let mut m = Machine::new(&program, &mut host, Context::default());
            let query = Term::reference(
                vec![
                    Term::var("data", None),
                    Term::string("docker", None),
                    Term::string("decision", None),
                ],
                None,
            );
            let results = eval_query(&mut m, &query, None).map_err(|e| e.to_string())?;
            Ok(results.iter().map(|v| value::to_json(v).unwrap()).collect())
        })
        .unwrap()
        .join()
        .unwrap()
}

/// `steps` patches, each adding a 9999-deep array at the innermost point of the last.
fn patched(steps: usize) -> String {
    format!(
        r#"package docker

rep(s, n) := concat("", [s | some i in numbers.range(0, n); i < n])

base := json.unmarshal(concat("", [rep("[", 9999), rep("]", 9999)]))

step := rep("/0", 9999)

first := rep("/0", 9998)

deep := json.patch(base, [op |
	some i in numbers.range(0, {last})
	op := {{"op": "add", "path": concat("", [first, concat("", [step | some j in numbers.range(0, i); j < i]), "/-"]), "value": base}}
])

decision := {{"n": count(json.marshal(deep)), "eq": deep == deep}}
"#,
        last = steps - 1
    )
}

/// A value a million levels deep is made a term, the term a value again, and both are
/// dropped, on a thread of 256 KiB: a stack too small for a frame a level.
#[test]
fn a_value_a_million_levels_deep_converts_and_drops_on_a_small_stack() {
    std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(|| {
            let mut v = Value::array(Vec::new());
            for _ in 0..1_000_000 {
                v = Value::array(vec![v]);
            }
            let t = shards_rego::eval::to_term(&v);
            let back = shards_rego::eval::to_value(&t).unwrap();
            drop(v);
            drop(t);
            let mut depth = 0;
            let mut at = &back;
            while let Value::Array(a) = at {
                depth += 1;
                match a.first() {
                    Some(x) => at = x,
                    None => break,
                }
            }
            assert_eq!(depth, 1_000_001);
            drop(back);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// 50 patches make a value 500000 levels deep: OPA answers as here, in 1.8 s and
/// 974 MiB (measured on an M5 Max); 10 patches, 100000 levels, in 0.28 s.
#[test]
fn values_patched_deeper_than_json_nests_compare_and_write_as_opa() {
    if cfg!(debug_assertions) {
        eprintln!("SKIP: the policy stack holds release builds");
        return;
    }
    assert_eq!(decide(patched(10)).unwrap(), [r#"{"eq":true,"n":219978}"#]);
    assert_eq!(decide(patched(50)).unwrap(), [r#"{"eq":true,"n":1019898}"#]);
}

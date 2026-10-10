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
    decide_on(POLICY_STACK, src)
}

/// `n` rules, each wrapping the one before in an array.
fn wrapped(n: usize) -> String {
    let mut s = String::from("package docker\n\nr0 := 1\n\n");
    for i in 1..=n {
        s.push_str(&format!("r{i} := [r{}]\n\n", i - 1));
    }
    s.push_str(&format!("decision := count(json.marshal(r{n}))\n"));
    s
}

/// Each rule's value is the one before, wrapped: the evaluation goes as deep as the
/// rules do. OPA answers 2000 such rules [4001] (1.1 s), 10000 [20001] (17 s, 5.4 GB):
/// its stack grows to 1 GB; a Rust thread's does not, so past what the policy thread
/// holds the evaluation ends with an error, never overflowing the stack.
#[test]
fn rules_nested_as_deep_as_the_stack_holds_answer_as_opa_and_no_deeper() {
    if cfg!(debug_assertions) {
        eprintln!("SKIP: the policy stack holds release builds");
        return;
    }
    assert_eq!(decide(wrapped(2000)).unwrap(), ["4001"]);
    let e = decide_on(16 << 20, wrapped(2000)).unwrap_err();
    assert!(
        e.ends_with("eval_internal_error: policy evaluation nests deeper than its thread's stack"),
        "{e}"
    );
}

/// `every` statements nested `n` deep, each using a variable of its own, then `also`.
fn nested_every(n: usize, also: &str) -> String {
    format!(
        "package docker\n\ndecision if {{\n\t{}true{}\n}}\n",
        format!("every x in [1] {{ x == 1; {also}").repeat(n),
        " }".repeat(n)
    )
}

/// OPA v1.14.1 compiles `every` statements nested 24 deep in 11 minutes, twice as long
/// with each level, and they answer true (16, 20 and 24 deep, measured). shards takes
/// one level's work and memory for each: 4000 deep, each printing, answer true, and
/// 99990 deep, as deep as OPA's parser takes them, end in the error of an evaluation
/// deeper than its stack. Before, the print and template rewrites and the safety check
/// copied, at each level, what was safe at it or what was within it, and the
/// evaluation each level's body for each of its runs: 4000 deep compiled in 28.6 s,
/// with a print call at each level in minutes, and evaluated in 31 GB (on an M5 Max).
#[test]
fn every_statements_nested_deep_answer_in_time() {
    if cfg!(debug_assertions) {
        eprintln!("SKIP: the policy stack holds release builds");
        return;
    }
    let t0 = std::time::Instant::now();
    assert_eq!(decide(nested_every(4000, "print(x); ")).unwrap(), ["true"]);
    let e = decide(nested_every(99990, "")).unwrap_err();
    assert!(
        e.ends_with("eval_internal_error: policy evaluation nests deeper than its thread's stack"),
        "{e}"
    );
    // Measured: 4000 deep 0.9 s, 99990 deep 4.3 s, on a busy host; the bound leaves room.
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(60),
        "{:?}",
        t0.elapsed()
    );
}

fn decide_on(stack: usize, src: String) -> Result<Vec<String>, String> {
    std::thread::Builder::new()
        .stack_size(stack)
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
            // Built apart, compared member by member; written; read for variables.
            let mut w = Value::array(Vec::new());
            for _ in 0..1_000_000 {
                w = Value::array(vec![w]);
            }
            assert_eq!(v, w);
            assert!(v < Value::array(vec![w.clone(), Value::Null]));
            let u = shards_rego::eval::to_term(&w);
            assert!(t.equal(&u));
            assert!(t.is_ground() && t.is_value());
            assert_eq!(value::to_json(&v).unwrap().len(), 2_000_002);
            drop((u, w));
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

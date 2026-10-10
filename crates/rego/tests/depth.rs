//! How deep a policy may nest, against OPA v1.14.1 (testdata/depth-oracle.json, written
//! by `scripts/rego/generate depth` from testdata/depth.json): for each shape, the
//! deepest policy OPA's parser takes parses, and one level deeper fails as OPA's parser
//! fails; for the shapes to check, the deepest runs as OPA runs it. All on a thread with
//! the policy thread's stack (123 MiB, M126), which no policy may overflow: an overflow
//! aborts the process, this test's binary with it.
//!
//! OPA's whole check of the deepest object and set took it 3 and 19 minutes, and more
//! than 50 GB: depth.json keeps the answers it gave (`answer`, measured once with
//! oracle_test.go's `run`) rather than the generator running it again. The other
//! shapes not checked whole take OPA too long at their deepest: its type checker is
//! exponential in nested comprehensions (10 levels 0.19 s, 12 levels 2.3 s, 14 levels
//! 29 s, measured on an M5 Max), builtin and function calls nested 9999 deep took it 28
//! and 13 minutes, and a chain of 99993 `+` did not end in 10.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
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

/// The shape's policy nested `n` deep (scripts/rego/oracle_test.go's DepthShape.text).
fn text(shape: &Json, n: usize) -> String {
    let field = |k: &str| shape.get(k).and_then(Json::as_str).unwrap_or_default();
    let mut s = field("head").to_string();
    if field("sep").is_empty() {
        s.push_str(&field("open").repeat(n));
        s.push_str(field("inner"));
        s.push_str(&field("close").repeat(n));
    } else {
        for _ in 0..n {
            s.push_str(field("inner"));
            s.push_str(field("sep"));
        }
        s.push_str(field("inner"));
    }
    s.push_str(field("tail"));
    s
}

/// A text as the oracle keeps it (oracle_test.go's longText): whole up to 300 bytes,
/// else its length, FNV-1a 64-bit hash and first 300 bytes.
fn long_text(s: &str) -> Json {
    if s.len() <= 300 {
        return s.into();
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    json!({"len": s.len(), "fnv64a": format!("{h:016x}"), "head": s.get(..300).unwrap()})
}

/// What the whole check makes of a policy, as the eval oracle words it: its results as
/// one JSON array, or its error.
fn check(src: &str) -> (&'static str, String) {
    let mut modules = BTreeMap::new();
    for (file, text) in [
        (
            "builtin/buildx_defaults.rego",
            include_str!("../src/buildx_defaults.rego"),
        ),
        ("policy.rego", src),
    ] {
        match parse_module(file, text) {
            Ok(m) => {
                modules.insert(file.to_string(), m);
            }
            Err(e) => return ("error", e.to_string()),
        }
    }
    let mut comp = Compiler::new(modules, host(), true);
    comp.compile();
    match comp.errors.as_slice() {
        [] => {}
        [e] => return ("error", format!("1 error occurred: {e}")),
        es => {
            let lines: Vec<String> = es.iter().map(ToString::to_string).collect();
            return (
                "error",
                format!("{} errors occurred:\n{}", es.len(), lines.join("\n")),
            );
        }
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
    match eval_query(&mut m, &query, None) {
        Ok(vs) => {
            let results: Vec<String> = vs.iter().map(|v| value::to_json(v).unwrap()).collect();
            ("results", format!("[{}]", results.join(",")))
        }
        Err(e) => ("error", e.to_string()),
    }
}

#[test]
fn policies_nest_as_deep_as_opa_takes_them_and_no_deeper() {
    if cfg!(debug_assertions) {
        // M126 measured the policy thread's stack on release builds.
        eprintln!("SKIP: the policy stack holds release builds");
        return;
    }
    let shapes: Json = serde_json::from_str(include_str!("../testdata/depth.json")).unwrap();
    let oracle: Json = serde_json::from_str(include_str!("../testdata/depth-oracle.json")).unwrap();
    let failed = std::thread::Builder::new()
        .stack_size(POLICY_STACK)
        .spawn(move || {
            let mut failed = Vec::new();
            for (shape, want) in shapes.as_array().unwrap().iter().zip(oracle.as_array().unwrap()) {
                let name = shape["name"].as_str().unwrap();
                assert_eq!(want["name"], name);
                let max = usize::try_from(want["max"].as_u64().unwrap()).unwrap();
                let refused = match parse_module("policy.rego", &text(shape, max + 1)) {
                    Ok(_) => json!("parsed"),
                    Err(e) => long_text(&e.to_string()),
                };
                if refused != want["refused"] {
                    failed.push(format!(
                        "{name} at {}: refused\n  got {refused}\n  OPA {}",
                        max + 1,
                        want["refused"]
                    ));
                }
                // OPA's answer at the deepest: its oracle's, or the one measured once.
                let answer = match shape.get("answer") {
                    Some(a) => json!({"results": a}),
                    None if shape.get("check") == Some(&json!(true)) => want.clone(),
                    None => {
                        if let Err(e) = parse_module("policy.rego", &text(shape, max)) {
                            failed.push(format!("{name} at {max}: refused, as OPA does not: {e}"));
                        }
                        continue;
                    }
                };
                let want = &answer;
                let t0 = std::time::Instant::now();
                let (kind, got) = check(&text(shape, max));
                // Measured: under 0.1 s each on an M5 Max, the deepest set's type taking
                // 41 s while each level copied the type of the levels within it (OPA: 19
                // minutes); the bound leaves room for a busy host.
                if t0.elapsed() > std::time::Duration::from_secs(30) {
                    failed.push(format!("{name} at {max}: checked in {:?}", t0.elapsed()));
                }
                if long_text(&got) != want[kind] {
                    let other = if kind == "results" { "error" } else { "results" };
                    failed.push(format!(
                        "{name} at {max}: {kind}\n  got {}\n  OPA {}{}",
                        long_text(&got),
                        want[kind],
                        want.get(other)
                            .map(|o| format!(" ({other} {o})"))
                            .unwrap_or_default()
                    ));
                }
            }
            failed
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

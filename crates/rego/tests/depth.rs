//! How deep a policy may nest, against OPA v1.14.1 (testdata/depth-oracle.json, written
//! by `scripts/rego/generate depth` from testdata/depth.json): for each shape, the
//! deepest policy OPA's parser takes parses, and one level deeper fails as OPA's parser
//! fails; for the shapes to check, the deepest runs as OPA runs it. All on a thread with
//! the policy thread's stack (123 MiB, M126), which no policy may overflow: an overflow
//! aborts the process, this test's binary with it.
//!
//! OPA's whole check of the deepest object and set took it 3 and 19 minutes, and more
//! than 50 GB: depth.json keeps the answers it gave (`answer`, measured once with
//! oracle_test.go's `run`) rather than the generator running it again. Nested `every`
//! OPA answers only shallower: 1000 and 5000 deep it answered as depth.json's `answer`
//! (in 5.5 s and 337 MiB, and in 10 minutes 52 s and 6.7 GB, copying each statement
//! whole at each level), 20000 deep it gave no answer in an hour, and at its deepest
//! its memory passed 48 GiB in 64 s. The other
//! shapes not checked whole take OPA too long at their deepest: its type checker is
//! exponential in nested comprehensions (10 levels 0.19 s, 12 levels 2.3 s, 14 levels
//! 29 s, measured on an M5 Max), builtin and function calls nested 9999 deep took it 28
//! and 13 minutes, and a chain of 99993 `+` did not end in 10.
//!
//! A shape checked at its deepest is held to grow no faster than linearly in its depth,
//! on the host it runs on ([`grows_linearly`], D98), and every shape's times print on
//! every run, past the harness's capture, for comparing hosts.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::Write as _;
use std::time::{Duration, Instant};

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

/// How many times shallower than a shape's deepest policy the one its growth is read from
/// is (D98): work linear in the depth takes at most this many times as long at the
/// deepest, work quadratic in it about its square. A timing error moves the exponent read
/// over it by its log over log 16, half what it moved over a factor of 4.
const DEPTH_FACTOR: u32 = 16;

/// How many times each depth is checked, the two interleaved, its least taken (D98): noise
/// only adds time (Chen and Revels, "Robust benchmarking in noisy environments", 2016), so
/// a depth's estimate moves only where all its runs were slowed, and a host slowed through
/// both depths slows them alike.
const RUNS: usize = 3;

/// Whether `deep`, a shape's check at its deepest, took no longer than linear growth
/// allows from `shallow`, its check [`DEPTH_FACTOR`] times shallower on the same host:
/// a ratio of at most DEPTH_FACTOR^(3/2), the bound as far, in ratio, from linear growth
/// (DEPTH_FACTOR) as from quadratic (its square).
fn grows_linearly(deep: Duration, shallow: Duration) -> bool {
    deep.as_secs_f64() <= shallow.as_secs_f64() * f64::from(DEPTH_FACTOR).powf(1.5)
}

/// Says a line on the test's own stderr, which the harness does not capture, so that a
/// run that passes shows it too.
fn say(line: &str) {
    let _ = writeln!(std::io::stderr(), "depth: {line}");
}

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
        decl: Type::function(args, result),
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
        ("builtin/defaults.rego", include_str!("../src/defaults.rego")),
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
                    Some(a @ Json::Object(_)) => a.clone(),
                    Some(a) => json!({"results": a}),
                    None if shape.get("check") == Some(&json!(true)) => want.clone(),
                    None => {
                        let t0 = Instant::now();
                        let parsed = parse_module("policy.rego", &text(shape, max));
                        say(&format!("{name} at {max}: parsed in {:?}", t0.elapsed()));
                        if let Err(e) = parsed {
                            failed.push(format!("{name} at {max}: refused, as OPA does not: {e}"));
                        }
                        continue;
                    }
                };
                let want = &answer;
                // Its growth: the deepest against one DEPTH_FACTOR times shallower, each
                // checked RUNS times on this host, interleaved, its least taken.
                let timed = |n: usize| {
                    let t0 = Instant::now();
                    let checked = check(&text(shape, n));
                    (t0.elapsed(), checked)
                };
                let shallow = max / DEPTH_FACTOR as usize;
                let (mut smalls, mut deeps, mut first) = (Vec::new(), Vec::new(), None);
                for _ in 0..RUNS {
                    smalls.push(timed(shallow).0);
                    let (t, checked) = timed(max);
                    deeps.push(t);
                    if first.is_none() {
                        first = Some(checked);
                    }
                }
                let (kind, got) = first.unwrap();
                let small = *smalls.iter().min().unwrap();
                let deep = *deeps.iter().min().unwrap();
                let ratio = deep.as_secs_f64() / small.as_secs_f64();
                let growth = format!(
                    "{name} at {max}: {deep:?} (least of {deeps:?}); at {shallow}: {small:?} \
                     (least of {smalls:?}); ratio {ratio:.2}, exponent {:.2} (1 linear, \
                     2 quadratic, past 1.5 refused)",
                    ratio.ln() / f64::from(DEPTH_FACTOR).ln()
                );
                say(&growth);
                if !grows_linearly(deep, small) {
                    failed.push(format!("grew faster than linearly: {growth}"));
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

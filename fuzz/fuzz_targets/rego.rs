//! Build policies as a Dockerfile's `.rego`, `--policy` or a remote context sends them
//! (shards_rego, D98, D101, D111): parsed, compiled with buildx's functions, and
//! evaluated for `data.docker.decision`, partially and whole, as the policy layer runs
//! them, on a thread of the policy layer's stack: no panic, no overflow. The input is
//! the policy's text, then a NUL and the input's JSON.
#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use shards_rego::ast::Term;
use shards_rego::compile::{Compiler, Function};
use shards_rego::eval::{Host, HostError, Machine, Program, eval_query, partial_query};
use shards_rego::funcs::Context;
use shards_rego::parser::parse_module;
use shards_rego::types::Type;
use shards_rego::value::{self, Value};

/// crates/shards' `policy::STACK`.
const POLICY_STACK: usize = 123 << 20;

/// Answers every function as a host that has nothing to say.
struct Undefined;

impl Host for Undefined {
    fn call(&mut self, _: &str, _: &[Value]) -> Result<Option<Value>, HostError> {
        Ok(None)
    }
}

/// crates/shards' `policy::functions`.
fn functions() -> Vec<Function> {
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
        f("verify_image_signature", vec![a(), s()], Type::Boolean),
    ]
}

fn reference(root: &str, path: &[&str]) -> Term {
    let mut parts = vec![Term::var(root, None)];
    parts.extend(path.iter().map(|p| Term::string(p, None)));
    Term::reference(parts, None)
}

fn run(src: &str, input: Option<&str>) {
    let input = input.and_then(|i| value::from_json(i).ok());
    let Ok(module) = parse_module("policy.rego", src) else {
        return;
    };
    let mut modules = BTreeMap::new();
    modules.insert("policy.rego".to_string(), module);
    let mut comp = Compiler::new(modules, functions(), true);
    comp.compile();
    if !comp.errors.is_empty() {
        return;
    }
    let program = Program::new(&comp, functions().into_iter().map(|f| f.name).collect());
    let query = reference("data", &["docker", "decision"]);
    let unknowns = [
        reference("input", &["image", "labels"]),
        reference("input", &["git", "commit"]),
        reference("input", &["http", "checksum"]),
    ];
    let mut host = Undefined;
    let mut m = Machine::new(&program, &mut host, Context::default());
    let _ = partial_query(&mut m, &query, input.clone(), &unknowns);
    drop(m);
    let mut m = Machine::new(&program, &mut host, Context::default());
    if let Ok(results) = eval_query(&mut m, &query, input) {
        for v in &results {
            let _ = value::to_json(v);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let (src, input) = match data.iter().position(|&b| b == 0) {
        Some(at) => (data.get(..at).unwrap_or_default(), data.get(at + 1..)),
        None => (data, None),
    };
    let Ok(src) = std::str::from_utf8(src) else {
        return;
    };
    let input = input
        .and_then(|i| std::str::from_utf8(i).ok())
        .map(str::to_string);
    let src = src.to_string();
    // A panic on the policy thread is the finding: join hands it back to be raised here.
    if let Ok(Err(e)) = std::thread::Builder::new()
        .stack_size(POLICY_STACK)
        .spawn(move || run(&src, input.as_deref()))
        .map(std::thread::JoinHandle::join)
    {
        std::panic::resume_unwind(e);
    }
});

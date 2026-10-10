//! What one policy check costs, as shards' policy layer runs one (D101) and buildx runs
//! one per source it checks: buildx's builtins module and the policy parsed, compiled
//! with buildx's functions, and `data.docker.decision` evaluated (or partially evaluated
//! over the unknowns buildx passes before a source's metadata is resolved), on the policy
//! thread's stack (M126). For each case of `benches/policies/cases.json`, `--n` times
//! after a warm-up: each phase's time, the whole check's, and evaluation alone on a
//! program compiled once; then the process's peak resident memory. OPA v1.14.1's own
//! numbers for the same cases: `scripts/rego/bench-opa`.
//!
//! `cargo bench -p shards-rego --bench policy [-- --n 200 --cases FILE --only NAME]`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use shards_rego::ast::Term;
use shards_rego::compile::{Compiler, Function};
use shards_rego::eval::{Host, HostError, Machine, Program, eval_query, partial_query};
use shards_rego::types::Type;
use shards_rego::value::Value;

struct NoHost;

impl Host for NoHost {
    fn call(&mut self, _: &str, _: &[Value]) -> Result<Option<Value>, HostError> {
        Ok(None)
    }
}

/// buildx's functions (policy/funcs.go), as shards declares them.
fn functions() -> Vec<Function> {
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

fn ref_term(path: &str) -> Term {
    let parts = path
        .split('.')
        .enumerate()
        .map(|(i, seg)| {
            if i == 0 {
                Term::var(seg, None)
            } else {
                Term::string(seg, None)
            }
        })
        .collect();
    Term::reference(parts, None)
}

struct Case {
    name: String,
    file: String,
    src: String,
    input: Option<String>,
    unknowns: Option<Vec<String>>,
}

fn compile(case: &Case) -> (Program, f64, f64) {
    let t0 = Instant::now();
    let mut modules = BTreeMap::new();
    for (file, text) in [
        (
            "builtin/buildx_defaults.rego",
            include_str!("../src/buildx_defaults.rego"),
        ),
        (case.file.as_str(), case.src.as_str()),
    ] {
        modules.insert(
            file.to_string(),
            shards_rego::parser::parse_module(file, text).unwrap(),
        );
    }
    let parsed = t0.elapsed();
    let mut comp = Compiler::new(modules, functions(), true);
    comp.compile();
    assert!(comp.errors.is_empty(), "{}: {:?}", case.name, comp.errors);
    let program = Program::new(&comp, functions().into_iter().map(|f| f.name).collect());
    let compiled = t0.elapsed();
    (
        program,
        parsed.as_secs_f64() * 1e6,
        (compiled - parsed).as_secs_f64() * 1e6,
    )
}

/// One evaluation, its time in µs.
fn evaluate(case: &Case, program: &Program) -> f64 {
    let input = case
        .input
        .as_deref()
        .map(|t| shards_rego::value::from_json(t).unwrap());
    let t0 = Instant::now();
    let mut host = NoHost;
    let ctx = shards_rego::funcs::Context {
        time_ns: 1_700_000_000_000_000_000,
        seed: vec![7; 64],
        ..Default::default()
    };
    let mut m = Machine::new(program, &mut host, ctx);
    let q = ref_term("data.docker.decision");
    match &case.unknowns {
        None => {
            let r = eval_query(&mut m, &q, input).unwrap();
            assert_eq!(r.len(), 1, "{}", case.name);
        }
        Some(u) => {
            let terms: Vec<Term> = u.iter().map(|x| ref_term(x)).collect();
            partial_query(&mut m, &q, input, &terms).unwrap();
        }
    }
    drop(m);
    t0.elapsed().as_secs_f64() * 1e6
}

fn percentile(v: &[f64], p: f64) -> f64 {
    let rank = ((p / 100.0) * v.len() as f64).ceil().max(1.0) as usize;
    v[rank.min(v.len()) - 1]
}

fn stats(name: &str, mut v: Vec<f64>) -> (String, String) {
    v.sort_by(f64::total_cmp);
    let (p50, p90, p99, max) = (
        percentile(&v, 50.0),
        percentile(&v, 90.0),
        percentile(&v, 99.0),
        v[v.len() - 1],
    );
    (
        format!(
            "{name:<34} {:>6} {p50:>10.1} {p90:>10.1} {p99:>10.1} {max:>10.1}  us",
            v.len()
        ),
        format!(
            "\"{name}\":{{\"unit\":\"us\",\"n\":{},\"p50\":{p50:.1},\"p90\":{p90:.1},\"p99\":{p99:.1},\"max\":{max:.1}}}",
            v.len()
        ),
    )
}

fn output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn stamp() -> (String, String, String, String) {
    let (host, os, load) = if cfg!(target_os = "macos") {
        (
            format!(
                "{} ({})",
                output("sysctl", &["-n", "machdep.cpu.brand_string"]),
                output("sysctl", &["-n", "hw.model"])
            ),
            format!(
                "macOS {} ({})",
                output("sw_vers", &["-productVersion"]),
                output("sw_vers", &["-buildVersion"])
            ),
            output("sysctl", &["-n", "vm.loadavg"])
                .trim_matches(|c: char| c == '{' || c == '}' || c.is_whitespace())
                .to_string(),
        )
    } else {
        (
            std::fs::read_to_string("/proc/cpuinfo")
                .ok()
                .and_then(|c| {
                    c.lines()
                        .find(|l| l.starts_with("model name"))
                        .and_then(|l| l.split(':').nth(1))
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_else(|| "unknown".into()),
            format!("{} {}", output("uname", &["-s"]), output("uname", &["-r"])),
            std::fs::read_to_string("/proc/loadavg")
                .ok()
                .map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| "unknown".into()),
        )
    };
    let dir = env!("CARGO_MANIFEST_DIR");
    let mut rev = output("git", &["-C", dir, "rev-parse", "--short", "HEAD"]);
    if Command::new("git")
        .args(["-C", dir, "status", "--porcelain", "--untracked-files=no"])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty())
    {
        rev.push_str("-dirty");
    }
    (host, os, rev, load)
}

/// The process's peak resident memory in MiB (getrusage(2)); none off Unix.
#[cfg(unix)]
fn peak_rss_mib() -> Option<u64> {
    // SAFETY: getrusage(2) into a zeroed struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut ru) };
    let max = u64::try_from(ru.ru_maxrss).ok()?;
    Some(if cfg!(target_os = "macos") {
        max >> 20
    } else {
        max >> 10
    })
}

#[cfg(not(unix))]
fn peak_rss_mib() -> Option<u64> {
    None
}

fn read_cases(path: &Path, only: Option<&str>) -> Vec<Case> {
    let list: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let base = path.parent().unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .filter(|c| only.is_none_or(|o| c["name"].as_str().unwrap().contains(o)))
        .map(|c| Case {
            name: c["name"].as_str().unwrap().to_string(),
            file: c["file"].as_str().unwrap().to_string(),
            src: std::fs::read_to_string(base.join(c["file"].as_str().unwrap())).unwrap(),
            input: c
                .get("input")
                .and_then(|i| i.as_str())
                .map(|i| std::fs::read_to_string(base.join(i)).unwrap()),
            unknowns: c
                .get("unknowns")
                .and_then(|u| u.as_array())
                .map(|u| u.iter().map(|x| x.as_str().unwrap().to_string()).collect()),
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let n: usize = flag("--n").map_or(200, |v| v.parse().unwrap());
    let cases_path: PathBuf = flag("--cases").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/policies/cases.json"),
        PathBuf::from,
    );
    let only = flag("--only");
    let rows = std::thread::Builder::new()
        .name("policy".into())
        .stack_size(123 << 20)
        .spawn(move || {
            let mut rows = Vec::new();
            for case in read_cases(&cases_path, only.as_deref()) {
                // Warm-up: the registry of builtins and the allocator's first pages.
                let (program, _, _) = compile(&case);
                evaluate(&case, &program);
                let (mut whole, mut parse, mut comp, mut eval) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                for _ in 0..n {
                    let t0 = Instant::now();
                    let (program, p, c) = compile(&case);
                    let e = evaluate(&case, &program);
                    drop(program);
                    whole.push(t0.elapsed().as_secs_f64() * 1e6);
                    parse.push(p);
                    comp.push(c);
                    eval.push(e);
                }
                let mut alone = Vec::new();
                for _ in 0..n {
                    alone.push(evaluate(&case, &program));
                }
                rows.push(stats(&format!("{}/whole", case.name), whole));
                rows.push(stats(&format!("{}/parse", case.name), parse));
                rows.push(stats(&format!("{}/compile", case.name), comp));
                rows.push(stats(&format!("{}/eval", case.name), eval));
                rows.push(stats(&format!("{}/eval-prepared", case.name), alone));
            }
            rows
        })
        .unwrap()
        .join()
        .unwrap();
    let (host, os, rev, load) = stamp();
    println!("policy: n={n}\nhost: {host}\nos: {os}\nrevision: {rev}\nload (1, 5, 15 min): {load}\n");
    println!(
        "{:<34} {:>6} {:>10} {:>10} {:>10} {:>10}",
        "case/phase", "n", "p50", "p90", "p99", "max"
    );
    for (row, _) in &rows {
        println!("{row}");
    }
    let peak = peak_rss_mib().map_or_else(|| "null".to_string(), |p| p.to_string());
    println!("peak resident: {peak} MiB");
    let json: Vec<&str> = rows.iter().map(|(_, j)| j.as_str()).collect();
    println!(
        "\nshards-bench {{\"bench\":\"rego-policy\",\"n\":\"{n}\",\"host\":\"{host}\",\"os\":\"{os}\",\"revision\":\"{rev}\",\"load\":\"{load}\",\"peak_rss_mib\":{peak},{}}}",
        json.join(",")
    );
}

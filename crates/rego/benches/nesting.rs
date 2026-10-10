//! What deeply nested policies cost (M160): a shape of `benches/nesting.json` at a depth,
//! parsed, compiled with buildx's functions and `data.docker.decision` evaluated, on the
//! policy thread's stack (M126), `--n` times after a warm-up, whatever the outcome (OPA
//! answers some shapes only with an error): each phase's time, the whole check's, the
//! outcome, then the process's peak resident memory. One shape and depth a process, so
//! that the peak is the case's. OPA v1.14.1's numbers for the same shapes:
//! `docs/research/measurements/rego-nesting/run.sh`.
//!
//! A shape nests `open` `depth` times around `inner`, closed by as many `close`, between
//! `head` and `tail`; or repeats `item` for i from 1 to `depth` (`{i}` the index, `{j}`
//! the one before), `{n}` in `tail` the depth.
//!
//! `cargo bench -p shards-rego --bench nesting -- --shape NAME --depth N [--n 5]`

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::time::Instant;

mod common;

use common::{NoHost, functions, peak_rss_mib, ref_term, stamp, stats};

use shards_rego::compile::Compiler;
use shards_rego::eval::{Machine, Program, eval_query};

/// The shape's policy, `depth` deep.
fn policy(shape: &serde_json::Value, depth: usize) -> String {
    let field = |k: &str| {
        shape
            .get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    };
    let mut s = field("head").to_string();
    if let Some(item) = shape.get("item").and_then(serde_json::Value::as_str) {
        for i in 1..=depth {
            s.push_str(
                &item
                    .replace("{i}", &i.to_string())
                    .replace("{j}", &(i - 1).to_string()),
            );
        }
    } else {
        s.push_str(&field("open").repeat(depth));
        s.push_str(field("inner"));
        s.push_str(&field("close").repeat(depth));
    }
    s.push_str(&field("tail").replace("{n}", &depth.to_string()));
    s
}

/// One check: each phase's time in µs, and its outcome.
fn check(src: &str) -> ([f64; 3], String) {
    let t0 = Instant::now();
    let mut modules = BTreeMap::new();
    modules.insert(
        "builtin/buildx_defaults.rego".to_string(),
        shards_rego::parser::parse_module(
            "builtin/buildx_defaults.rego",
            include_str!("../src/buildx_defaults.rego"),
        )
        .unwrap(),
    );
    let parsed = shards_rego::parser::parse_module("policy.rego", src);
    let t_parse = t0.elapsed();
    let us = |d: std::time::Duration| d.as_secs_f64() * 1e6;
    let m = match parsed {
        Ok(m) => m,
        Err(e) => {
            return (
                [us(t_parse), 0.0, 0.0],
                format!("parse error ({} bytes)", e.to_string().len()),
            );
        }
    };
    modules.insert("policy.rego".to_string(), m);
    let mut comp = Compiler::new(modules, functions(), true);
    comp.compile();
    let t_compile = t0.elapsed();
    if let Some(e) = comp.errors.first() {
        let outcome = format!("{} errors, the first {e}", comp.errors.len());
        return ([us(t_parse), us(t_compile - t_parse), 0.0], outcome);
    }
    let program = Program::new(&comp, functions().into_iter().map(|f| f.name).collect());
    let mut host = NoHost;
    let mut m = Machine::new(&program, &mut host, shards_rego::funcs::Context::default());
    let r = eval_query(&mut m, &ref_term("data.docker.decision"), None);
    drop(m);
    let t_eval = t0.elapsed();
    let outcome = match r {
        Ok(vs) => format!(
            "results [{}]",
            vs.iter()
                .map(|v| shards_rego::value::to_json(v).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(",")
        ),
        Err(e) => format!("error {e}"),
    };
    drop(program);
    drop(comp);
    (
        [us(t_parse), us(t_compile - t_parse), us(t_eval - t_compile)],
        outcome,
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let n: usize = flag("--n").map_or(5, |v| v.parse().unwrap());
    let name = flag("--shape").expect("--shape NAME");
    let depth: usize = flag("--depth").expect("--depth N").parse().unwrap();
    let shapes: serde_json::Value = serde_json::from_str(include_str!("nesting.json")).unwrap();
    let shape = shapes
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name.as_str())
        .expect("a shape of benches/nesting.json")
        .clone();
    let src = policy(&shape, depth);
    let case = format!("{name}-{depth}");
    let (rows, outcome) = std::thread::Builder::new()
        .name("policy".into())
        .stack_size(123 << 20)
        .spawn(move || {
            // Warm-up: the registry of builtins and the allocator's first pages.
            let (_, outcome) = check(&src);
            let (mut parse, mut compile, mut eval, mut whole) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for _ in 0..n {
                let ([p, c, e], _) = check(&src);
                parse.push(p);
                compile.push(c);
                eval.push(e);
                whole.push(p + c + e);
            }
            let rows = vec![
                stats(&format!("{case}/whole"), whole),
                stats(&format!("{case}/parse"), parse),
                stats(&format!("{case}/compile"), compile),
                stats(&format!("{case}/eval"), eval),
            ];
            (rows, outcome)
        })
        .unwrap()
        .join()
        .unwrap();
    let (host, os, rev, load) = stamp();
    println!("nesting: n={n}\nhost: {host}\nos: {os}\nrevision: {rev}\nload (1, 5, 15 min): {load}\n");
    println!(
        "{:<34} {:>6} {:>10} {:>10} {:>10} {:>10}",
        "case/phase", "n", "p50", "p90", "p99", "max"
    );
    for (row, _) in &rows {
        println!("{row}");
    }
    let outcome = if outcome.len() > 300 {
        format!(
            "{}... ({} bytes)",
            outcome.get(..300).unwrap_or_default(),
            outcome.len()
        )
    } else {
        outcome
    };
    println!("outcome: {outcome}");
    let peak = peak_rss_mib().map_or_else(|| "null".to_string(), |p| p.to_string());
    println!("peak resident: {peak} MiB");
    let json: Vec<&str> = rows.iter().map(|(_, j)| j.as_str()).collect();
    println!(
        "\nshards-bench {{\"bench\":\"rego-nesting\",\"n\":\"{n}\",\"host\":\"{host}\",\"os\":\"{os}\",\"revision\":\"{rev}\",\"load\":\"{load}\",\"outcome\":{},\"peak_rss_mib\":{peak},{}}}",
        serde_json::Value::String(outcome),
        json.join(",")
    );
}

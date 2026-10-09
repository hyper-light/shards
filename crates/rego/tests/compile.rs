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
fn modules_compile_as_opa_compiles_them() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../testdata/cases.json")).unwrap();
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("../testdata/compile-oracle.json")).unwrap();
    let mut failed = Vec::new();
    let mut total = 0;
    for (c, want) in cases.as_array().unwrap().iter().zip(oracle.as_array().unwrap()) {
        let name = c["name"].as_str().unwrap();
        if want
            .get("error")
            .and_then(|e| e.as_str())
            .is_some_and(|e| e.starts_with("parse: "))
        {
            continue;
        }
        total += 1;
        let mut srcs = vec![(
            "builtin/buildx_defaults.rego".to_string(),
            include_str!("../src/buildx_defaults.rego").to_string(),
        )];
        for m in c["modules"].as_array().unwrap() {
            srcs.push((
                m[0].as_str().unwrap().to_string(),
                m[1].as_str().unwrap().to_string(),
            ));
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
                es => format!(
                    "{} errors occurred:\n{}",
                    es.len(),
                    es.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
                ),
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
        // then an unstable sort): measured, see scripts/rego. It orders recursion errors
        // as it ranges over the rule tree's map of children (TreeNode.DepthFirst). Compare
        // those runs as sets.
        if normalize(&got) != normalize(&wanted) {
            failed.push(format!("--- {name}\ngot:\n{got}\nOPA:\n{wanted}"));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {total} differ:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// A policy of `n` rules, each reading the one before it and the input, as a large
/// organization's policy reads its helpers.
fn chain(n: usize) -> String {
    let mut s = String::from("package docker\n\ndefault allow := false\n\nr0 if input.image\n\n");
    for i in 1..n {
        s.push_str(&format!(
            "r{i} if {{\n\tr{}\n\tinput.image.repo != \"blocked-{i}\"\n\tsome x in object.get(input.image, \"labels\", {{}})\n\tx != \"deny-{i}\"\n}}\n\n",
            i - 1
        ));
    }
    s.push_str(&format!("allow if r{}\n\ndecision := {{\"allow\": allow}}\n", n - 1));
    s
}

/// The compiler's work grows as OPA's does with the rules a policy has, not with their
/// square or cube: 2000 rules in a chain compile within a bound that leaves room for a
/// busy host (OPA v1.14.1 compiles 1600 in 0.27 s on an M5 Max; before the dependency
/// graph was built once, shards took 3.5 s for 400 and minutes for 2000).
#[test]
fn a_long_chain_of_rules_compiles_in_time() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(123 << 20)
        .spawn(move || {
            let mut modules = BTreeMap::new();
            modules.insert(
                "builtin/buildx_defaults.rego".to_string(),
                parse_module("builtin/buildx_defaults.rego", include_str!("../src/buildx_defaults.rego")).unwrap(),
            );
            modules.insert("chain.rego".to_string(), parse_module("chain.rego", &chain(2000)).unwrap());
            let mut comp = Compiler::new(modules, host(), true);
            comp.compile();
            let _ = tx.send(comp.errors.len());
        })
        .unwrap();
    let errors = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("2000 rules did not compile within 10 s");
    assert_eq!(errors, 0);
}

/// Layers of rules, each rule reading the whole layer below it: few rules, many edges.
fn layers(width: usize, depth: usize) -> String {
    let mut s = String::from("package docker\n\n");
    for i in 0..width {
        s.push_str(&format!("l0.r{i} := {i}\n"));
    }
    for k in 1..depth {
        for i in 0..width {
            s.push_str(&format!("l{k}.r{i} if {{ some x in l{}; x != {i} }}\n", k - 1));
        }
    }
    s.push_str(&format!("decision := {{\"allow\": count(l{}) > 0}}\n", depth - 1));
    s
}

/// The recursion check searches only the rules on a cycle: OPA searches from every rule
/// (checkRecursion, one DFSPath each), which on these 6000 rules of 2.75 million edges
/// walks the edges below every rule again, billions of steps.
#[test]
fn a_wide_policy_without_cycles_compiles_in_time() {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(123 << 20)
        .spawn(move || {
            let mut modules = BTreeMap::new();
            modules.insert(
                "builtin/buildx_defaults.rego".to_string(),
                parse_module("builtin/buildx_defaults.rego", include_str!("../src/buildx_defaults.rego")).unwrap(),
            );
            modules.insert("layers.rego".to_string(), parse_module("layers.rego", &layers(500, 12)).unwrap());
            let mut comp = Compiler::new(modules, host(), true);
            comp.compile();
            let _ = tx.send(comp.errors.iter().map(ToString::to_string).collect::<Vec<_>>());
        })
        .unwrap();
    let errors = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("6000 rules did not compile within 10 s");
    assert_eq!(errors, Vec::<String>::new());
}

fn normalize(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut run: Vec<String> = Vec::new();
    // The run a line's error is ordered within: unsafe-variable errors by location,
    // recursion errors all together.
    let key = |l: &str| -> Option<String> {
        if l.contains(": rego_recursion_error: ") {
            return Some(String::new());
        }
        l.split_once(": rego_unsafe_var_error: ").map(|(at, _)| at.to_string())
    };
    for line in text.lines() {
        if key(line).is_some() {
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

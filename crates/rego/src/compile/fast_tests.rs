//! [`localvars::Fast`] held to the rewrite it stands for: OPA's, each walk walked again
//! (`localvars::EXACT`). Policies made at random of `every` statements, comprehensions,
//! declarations, uses of names declared after them, `with`, templates and errors, each
//! compiled both ways: the same modules, the same errors, the same names in place of the
//! generated ones the modules hold.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use super::localvars::EXACT;
use super::{Compiler, Function, vars};
use crate::parser::parse_module;
use crate::types::Type;

/// xorshift64*: the cases the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

struct Gen {
    rng: Rng,
    fresh: usize,
}

impl Gen {
    fn name(&mut self, p: &str) -> String {
        self.fresh += 1;
        format!("{p}{}", self.fresh)
    }

    /// A name to use: one in scope, else one declared later (`w0`..`w2`) or a free one.
    fn use_of(&mut self, scope: &[String]) -> String {
        if !scope.is_empty() && !self.rng.one_in(4) {
            return scope[self.rng.below(scope.len() as u64) as usize].clone();
        }
        format!("w{}", self.rng.below(3))
    }

    fn term(&mut self, depth: usize, scope: &[String]) -> String {
        match self.rng.below(if depth == 0 { 3 } else { 6 }) {
            0 => format!("{}", self.rng.below(3)),
            1 | 2 => self.use_of(scope),
            3 => format!("[{}]", self.term(depth - 1, scope)),
            4 => format!("{{\"k\": {}}}", self.term(depth - 1, scope)),
            _ => self.compr(depth - 1, scope),
        }
    }

    fn compr(&mut self, depth: usize, scope: &[String]) -> String {
        let t = self.name("t");
        let mut inner = scope.to_vec();
        let first = format!("{t} := {}", self.term(0, scope));
        inner.push(t.clone());
        let body = self.body(depth, &inner, Some(first));
        match self.rng.below(3) {
            0 => format!("[{t} | {body}]"),
            1 => format!("{{{t} | {body}}}"),
            _ => format!("{{{t}: 1 | {body}}}"),
        }
    }

    fn body(&mut self, depth: usize, scope: &[String], first: Option<String>) -> String {
        let mut scope = scope.to_vec();
        let mut out: Vec<String> = first.into_iter().collect();
        let n = 1 + self.rng.below(3);
        for _ in 0..n {
            let pick = if depth == 0 {
                self.rng.below(4)
            } else {
                self.rng.below(13)
            };
            match pick {
                0 => {
                    let x = self.name("x");
                    out.push(format!("{x} := {}", self.term(depth.min(1), &scope)));
                    scope.push(x);
                }
                1 => {
                    let u = self.use_of(&scope);
                    out.push(format!("{u} == {}", self.term(0, &scope)));
                }
                2 => {
                    let s = self.name("s");
                    out.push(format!("some {s} in [1, 2]"));
                    scope.push(s);
                }
                3 => {
                    let u = self.use_of(&scope);
                    let x = self.name("x");
                    out.push(format!("{x} := $\"{{{u}}}\""));
                    scope.push(x);
                }
                4..=8 => {
                    let v = self.name("v");
                    let mut inner = scope.clone();
                    let head = match self.rng.below(4) {
                        0 => format!("_, {v}"),
                        1 => {
                            let k = self.name("k");
                            inner.push(k.clone());
                            format!("{k}, {v}")
                        }
                        _ => v.clone(),
                    };
                    inner.push(v.clone());
                    let domain = match self.rng.below(4) {
                        0 => "[1, 2]".to_string(),
                        1 => format!("[{}]", self.use_of(&scope)),
                        2 => self.compr(depth - 1, &scope),
                        _ => "input.xs".to_string(),
                    };
                    let use_v = format!("{v} == {v}");
                    let body = self.body(depth - 1, &inner, Some(use_v));
                    out.push(format!("every {head} in {domain} {{ {body} }}"));
                    // A name the statement may have used, declared after it.
                    let w = format!("w{}", self.rng.below(3));
                    if self.rng.one_in(2) && !scope.contains(&w) && !out.iter().any(|e| e.contains(&w)) {
                        out.push(format!("{w} := 1"));
                        scope.push(w);
                    }
                }
                9 | 10 => {
                    let c = self.compr(depth - 1, &scope);
                    out.push(format!("count({c}) >= 0"));
                }
                _ => {
                    let c = self.compr(depth - 1, &scope);
                    let with = if self.rng.one_in(2) {
                        " with input as {\"xs\": [1]}"
                    } else {
                        ""
                    };
                    let refd = if self.rng.one_in(3) {
                        format!("; {}.a.b == 1", self.use_of(&scope))
                    } else {
                        String::new()
                    };
                    out.push(format!("count({c}) >= 0{with}{refd}"));
                }
            }
        }
        out.join("; ")
    }

    fn policy(&mut self, depth: usize) -> String {
        let mut s = String::from("package docker\n\n");
        let body = self.body(depth, &[], None);
        s.push_str(&format!("decision if {{ {body} }}\n\n"));
        if self.rng.one_in(2) {
            let body = self.body(depth.saturating_sub(1), &["a".to_string()], Some("a == a".into()));
            s.push_str(&format!("f(a) := 1 if {{ {body} }}\n\n"));
        }
        if self.rng.one_in(3) {
            let c = self.compr(depth.saturating_sub(1), &[]);
            s.push_str(&format!("p := {c}\n"));
        }
        s
    }
}

/// buildx's host functions, as the policy layer declares them.
fn host() -> Vec<Function> {
    let f = |name: &str, args: Vec<Type>, result: Type| Function {
        name: name.into(),
        decl: Type::function(args, result),
    };
    let (s, a) = (|| Type::String, || Type::Any(Vec::new()));
    vec![
        f("load_json", vec![s()], a()),
        f("verify_git_signature", vec![a(), s()], Type::Boolean),
        f("verify_http_pgp_signature", vec![a(), s(), s()], Type::Boolean),
        f("pin_image", vec![a(), s()], Type::Boolean),
        f("artifact_attestation", vec![a(), s()], a()),
        f("github_attestation", vec![a(), s()], a()),
    ]
}

/// The modules' text, else the errors, and what each generated name the modules hold
/// stands for (RewrittenVars, which error texts read).
fn compiled(src: &str, exact: bool) -> String {
    EXACT.with(|e| e.set(exact));
    let mut modules = BTreeMap::new();
    modules.insert(
        "builtin/defaults.rego".to_string(),
        parse_module("builtin/defaults.rego", include_str!("../defaults.rego")).unwrap(),
    );
    let Ok(m) = parse_module("policy.rego", src) else {
        EXACT.with(|e| e.set(false));
        return "parse error".into();
    };
    modules.insert("policy.rego".to_string(), m);
    let mut comp = Compiler::new(modules, host(), true);
    comp.compile();
    EXACT.with(|e| e.set(false));
    let mut out = String::new();
    for e in &comp.errors {
        out.push_str(&format!("{e}\n"));
    }
    let mut names = vars::VarVisitor::default();
    for (n, m) in &comp.modules {
        out.push_str(&format!("== {n}\n{m}\n"));
        for r in &m.rules {
            names.rule(r);
        }
    }
    let mut held: Vec<_> = names
        .vars
        .iter()
        .filter_map(|v| comp.rewritten.get(v).map(|w| format!("{v} -> {w}")))
        .collect();
    held.sort();
    out.push_str(&held.join("\n"));
    out
}

fn same_both_ways(src: &str) {
    let fast = compiled(src, false);
    let exact = compiled(src, true);
    assert!(
        fast == exact,
        "policy:\n{src}\nfast:\n{fast}\nOPA's way:\n{exact}"
    );
}

#[test]
fn random_policies_rewrite_as_opas_walks_again() {
    let mut g = Gen {
        rng: Rng(0x9E37_79B9_7F4A_7C15),
        fresh: 0,
    };
    for i in 0..4000 {
        let depth = 1 + (i % 4);
        let src = g.policy(depth);
        same_both_ways(&src);
    }
}

/// M510's shapes, as deep as OPA's way still answers in a test's time.
#[test]
fn nested_shapes_rewrite_as_opas_walks_again() {
    let nest = |open: &str, inner: &str, close: &str, n: usize| {
        format!(
            "package docker\n\ndecision if {{\n\t{}{inner}{}\n}}\n",
            open.repeat(n),
            close.repeat(n)
        )
    };
    let alternating = (
        "every x in [1] { x == 1; count([z | z := x; ",
        "true",
        "]) == 1 }",
    );
    let after = ("every x in [1] { x == 1; ", "true", "; y := 1; y == 1 }");
    let uses_after = (
        "every x in [1] { x == 1; count([z | z := y; ",
        "true",
        "]) == 1; y := 1 }",
    );
    let keyed = (
        "every k, x in [1] { k == 0; [z | z := x; every _, q in [k] { q == k; ",
        "true",
        " }] }",
    );
    for (open, inner, close) in [alternating, after, uses_after, keyed] {
        for n in 1..=9 {
            same_both_ways(&nest(open, inner, close, n));
        }
    }
}

/// The oracle's corpus (testdata/cases.json), each policy compiled both ways.
#[test]
fn the_corpus_rewrites_as_opas_walks_again() {
    let cases: serde_json::Value = serde_json::from_str(include_str!("../../testdata/cases.json")).unwrap();
    for c in cases.as_array().unwrap() {
        for m in c["modules"].as_array().unwrap() {
            same_both_ways(m[1].as_str().unwrap());
        }
    }
}

/// The generator skips the numbers the modules' names take, as OPA's loop does, and
/// says when it is asked past what OPA's counter numbers.
#[test]
fn the_generator_numbers_as_opas_and_stops_at_its_int() {
    use super::localvars::LocalVarGen;
    let taken: vars::VarSet = [
        "__local0__",
        "__local2__",
        "__local3__",
        "__local7__",
        "__local05__",
        "x",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let mut g = LocalVarGen::new(taken, "");
    let names: Vec<String> = (0..6).map(|_| g.generate().to_string()).collect();
    assert_eq!(
        names,
        [
            "__local1__",
            "__local4__",
            "__local5__",
            "__local6__",
            "__local8__",
            "__local9__",
        ]
    );
    assert_eq!(&*g.name_at(1), "__local4__");
    let mut g = LocalVarGen::new(vars::VarSet::new(), "");
    g.advance(i64::MAX.unsigned_abs());
    assert!(!g.overflowed());
    assert_eq!(g.calls(), i64::MAX.unsigned_abs());
    g.generate();
    assert!(
        !g.overflowed(),
        "the last name OPA's counter numbers: __local{}__",
        i64::MAX
    );
    g.generate();
    assert!(g.overflowed());
    g.rewind(0);
    assert!(!g.overflowed());
    g.advance(u64::MAX);
    assert!(g.overflowed());
}

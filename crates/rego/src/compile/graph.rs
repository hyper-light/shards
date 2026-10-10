//! The rules' dependency graph (OPA's `Graph`, setGraph): every rule and else branch,
//! numbered in module order, and the rules each may depend on, found once.
//!
//! A ref depends on the rules it refers to (`refers_to`): the rules of the same root
//! whose refs agree with it wherever both have a term and neither is a variable. For a
//! rule ref of strings alone, an index by the ref's leading strings finds them without a
//! scan of every rule; a rule ref with other terms is matched term by term. A branch's
//! dependencies are in the order a scan finds them: by ref in the rule's walk order, then
//! by module name and rule.
//!
//! A branch depends on all of a rule's branches at once, so the graph keeps an edge to
//! the rule, which leads on to each of its branches, as OPA's edges lead to each branch:
//! two rules of many else branches, one reading the other, make as many edges as they
//! have branches, not the product. Every walk over the graph keeps its own stack: a
//! cycle or a chain may be longer than a thread has stack for frames.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use crate::ast::{Module, Rule, Term, TermValue};

use super::{RuleNode, refers_to, rule_ref, safety};

pub(crate) struct DepGraph {
    /// Every rule and else branch: (module, rule, else depth).
    pub nodes: Vec<RuleNode>,
    /// Each branch's dependencies, as rule numbers.
    deps: Vec<Vec<usize>>,
    /// Each rule, by number: its first branch's node and its branches' count.
    groups: Vec<(usize, usize)>,
    /// Each module's rules' numbers.
    rules: Vec<Vec<usize>>,
    /// Each module's place, by name.
    module_at: HashMap<String, usize>,
}

/// Every rule and else branch, in the graph's node order, with its package's path.
pub(crate) fn branches(modules: &BTreeMap<String, Module>) -> Vec<(&[Term], &Rule)> {
    let mut out = Vec::new();
    for m in modules.values() {
        for rule in &m.rules {
            out.extend(rule.branches().map(|r| (m.package.path.as_slice(), r)));
        }
    }
    out
}

/// A ref's string parts after its root, up to its first other term, and whether they
/// are all of it.
fn string_prefix(r: &[Term]) -> (Vec<Rc<str>>, bool) {
    let mut parts = Vec::with_capacity(r.len());
    for t in r.iter().skip(1) {
        match &t.value {
            TermValue::String(s) => parts.push(s.clone()),
            _ => return (parts, false),
        }
    }
    (parts, true)
}

/// The refs of a rule's head and body, in walk order (setGraph's `WalkRefs`).
fn refs_of(rule: &Rule) -> Vec<Vec<Term>> {
    let mut refs: Vec<Vec<Term>> = Vec::new();
    let mut collect = |t: &Term| -> bool {
        if let TermValue::Ref(r) = &t.value {
            refs.push(r.to_vec());
        }
        false
    };
    for t in rule
        .head
        .args
        .iter()
        .chain(rule.head.key.iter())
        .chain(rule.head.value.iter())
    {
        safety::walk_terms(t, &mut collect);
    }
    for e in &rule.body {
        safety::walk_terms_expr(e, &mut collect);
    }
    refs
}

impl DepGraph {
    pub fn new(modules: &BTreeMap<String, Module>) -> DepGraph {
        let mut nodes = Vec::new();
        let mut groups = Vec::new();
        let mut rules = Vec::with_capacity(modules.len());
        let mut module_at = HashMap::with_capacity(modules.len());
        for (mi, (name, m)) in modules.iter().enumerate() {
            module_at.insert(name.clone(), mi);
            let mut per = Vec::with_capacity(m.rules.len());
            for (i, rule) in m.rules.iter().enumerate() {
                let start = nodes.len();
                nodes.extend(
                    rule.branches()
                        .enumerate()
                        .map(|(depth, _)| (name.clone(), i, depth)),
                );
                per.push(groups.len());
                groups.push((start, nodes.len() - start));
            }
            rules.push(per);
        }
        // Every rule's ref, by rule number, and the index of those of strings alone: by
        // their whole ref, and by each shorter prefix of it.
        let refs: Vec<Vec<Term>> = modules
            .values()
            .flat_map(|m| m.rules.iter().map(|rule| rule_ref(&m.package.path, rule)))
            .collect();
        let mut whole: HashMap<Vec<Rc<str>>, Vec<usize>> = HashMap::new();
        let mut under: HashMap<Vec<Rc<str>>, Vec<usize>> = HashMap::new();
        // The rules whose refs are not of strings alone: matched term by term.
        let mut other: Vec<usize> = Vec::new();
        for (g, r) in refs.iter().enumerate() {
            match string_prefix(r) {
                (parts, true) if r.first().and_then(Term::as_var) == Some("data") => {
                    for k in 0..parts.len() {
                        under
                            .entry(parts.get(..k).unwrap_or_default().to_vec())
                            .or_default()
                            .push(g);
                    }
                    whole.entry(parts).or_default().push(g);
                }
                _ => other.push(g),
            }
        }
        let mut deps = Vec::with_capacity(nodes.len());
        let mut found: Vec<usize> = Vec::new();
        for (_, x) in branches(modules) {
            let mut out = Vec::new();
            for dep in refs_of(x) {
                if dep.first().and_then(Term::as_var) != Some("data") {
                    continue;
                }
                found.clear();
                // The ref's strings up to its first other term: a rule ref of strings
                // alone no longer than them agrees with it where it equals their prefix;
                // a longer one, where it extends them and agrees with the ref's later
                // terms too.
                let (parts, exact) = string_prefix(&dep);
                for k in 0..=parts.len() {
                    if let Some(list) = parts.get(..k).and_then(|p| whole.get(p)) {
                        found.extend_from_slice(list);
                    }
                }
                if let Some(list) = under.get(&parts) {
                    for &g in list {
                        if exact || refs.get(g).is_some_and(|rr| refers_to(&dep, rr)) {
                            found.push(g);
                        }
                    }
                }
                for &g in &other {
                    if refs.get(g).is_some_and(|rr| refers_to(&dep, rr)) {
                        found.push(g);
                    }
                }
                // Rule numbers follow module name and rule, the order a scan finds them.
                found.sort_unstable();
                found.dedup();
                out.extend_from_slice(&found);
            }
            deps.push(out);
        }
        DepGraph {
            nodes,
            deps,
            groups,
            rules,
            module_at,
        }
    }

    /// The number of the node `n` names.
    pub fn id(&self, n: &RuleNode) -> Option<usize> {
        let mi = *self.module_at.get(&n.0)?;
        let g = *self.rules.get(mi)?.get(n.1)?;
        let &(start, count) = self.groups.get(g)?;
        (n.2 < count).then_some(start + n.2)
    }

    /// The branches a branch depends on, in order (OPA's edges).
    #[cfg(test)]
    pub fn edges_of(&self, b: usize) -> Vec<usize> {
        let mut out = Vec::new();
        for &g in self.deps.get(b).map(Vec::as_slice).unwrap_or_default() {
            if let Some(&(start, count)) = self.groups.get(g) {
                out.extend(start..start + count);
            }
        }
        out
    }

    /// The `e`th successor of vertex `v`: vertices are the branches, then the rules; a
    /// branch leads to the rules it depends on, a rule to its branches.
    fn successor(&self, v: usize, e: usize) -> Option<usize> {
        let b = self.nodes.len();
        match v.checked_sub(b) {
            None => self.deps.get(v)?.get(e).map(|g| b + g),
            Some(g) => {
                let &(start, count) = self.groups.get(g)?;
                (e < count).then_some(start + e)
            }
        }
    }

    fn vertices(&self) -> usize {
        self.nodes.len() + self.groups.len()
    }

    /// The branches on a cycle (Tarjan's algorithm, without recursion): those in a
    /// strongly connected component of more than one vertex. A branch that depends on
    /// its own rule forms one with that rule.
    pub fn cyclic(&self) -> Vec<bool> {
        let n = self.vertices();
        let mut index = vec![usize::MAX; n];
        let mut low = vec![0usize; n];
        let mut on_stack = vec![false; n];
        let mut stack: Vec<usize> = Vec::new();
        let mut out = vec![false; self.nodes.len()];
        let mut next = 0usize;
        // The DFS's frames: a vertex and the next of its successors to follow.
        let mut frames: Vec<(usize, usize)> = Vec::new();
        for root in 0..n {
            if index.get(root).is_some_and(|&i| i != usize::MAX) {
                continue;
            }
            frames.push((root, 0));
            while let Some(&mut (v, ref mut e)) = frames.last_mut() {
                if *e == 0 && index.get(v).is_some_and(|&i| i == usize::MAX) {
                    if let (Some(iv), Some(lv), Some(sv)) =
                        (index.get_mut(v), low.get_mut(v), on_stack.get_mut(v))
                    {
                        *iv = next;
                        *lv = next;
                        *sv = true;
                    }
                    next += 1;
                    stack.push(v);
                }
                if let Some(w) = self.successor(v, *e) {
                    *e += 1;
                    if index.get(w).is_some_and(|&i| i == usize::MAX) {
                        frames.push((w, 0));
                    } else if on_stack.get(w).copied().unwrap_or(false) {
                        let iw = index.get(w).copied().unwrap_or(usize::MAX);
                        if let Some(lv) = low.get_mut(v) {
                            *lv = (*lv).min(iw);
                        }
                    }
                    continue;
                }
                frames.pop();
                if let Some(&(parent, _)) = frames.last() {
                    let lv = low.get(v).copied().unwrap_or(usize::MAX);
                    if let Some(lp) = low.get_mut(parent) {
                        *lp = (*lp).min(lv);
                    }
                }
                if low.get(v) == index.get(v) {
                    let mut members = Vec::new();
                    while let Some(w) = stack.pop() {
                        if let Some(s) = on_stack.get_mut(w) {
                            *s = false;
                        }
                        members.push(w);
                        if w == v {
                            break;
                        }
                    }
                    if members.len() > 1 {
                        for w in members {
                            if let Some(o) = out.get_mut(w) {
                                *o = true;
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// util.dfsRecursive from branch `z` back to it, without recursion: the branches on
    /// the way, last first, `z` at both ends; empty if there is none.
    pub fn path_back(&self, z: usize) -> Vec<usize> {
        let mut visited = vec![false; self.vertices()];
        if let Some(v) = visited.get_mut(z) {
            *v = true;
        }
        let mut frames: Vec<(usize, usize)> = vec![(z, 0)];
        while let Some(&mut (u, ref mut e)) = frames.last_mut() {
            let Some(v) = self.successor(u, *e) else {
                frames.pop();
                continue;
            };
            *e += 1;
            if v == z {
                let mut path = vec![z];
                path.extend(frames.iter().rev().map(|&(u, _)| u));
                path.retain(|&u| u < self.nodes.len());
                return path;
            }
            if let Some(seen) = visited.get_mut(v)
                && !*seen
            {
                *seen = true;
                frames.push((v, 0));
            }
        }
        Vec::new()
    }

    /// Graph.Sort: every branch after the branches it depends on, from each branch in
    /// module order, depth first, without recursion.
    pub fn sorted(&self) -> Vec<usize> {
        let b = self.nodes.len();
        let mut marked = vec![false; self.vertices()];
        let mut out = Vec::with_capacity(b);
        let mut frames: Vec<(usize, usize)> = Vec::new();
        for root in 0..b {
            if marked.get(root).copied().unwrap_or(true) {
                continue;
            }
            if let Some(m) = marked.get_mut(root) {
                *m = true;
            }
            frames.push((root, 0));
            while let Some(&mut (v, ref mut e)) = frames.last_mut() {
                if let Some(w) = self.successor(v, *e) {
                    *e += 1;
                    if let Some(m) = marked.get_mut(w)
                        && !*m
                    {
                        *m = true;
                        frames.push((w, 0));
                    }
                    continue;
                }
                frames.pop();
                if v < b {
                    out.push(v);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::DepGraph;
    use crate::ast::Module;
    use crate::compile::{Compiler, Function, RuleNode};
    use crate::parser::parse_module;
    use crate::types::Type;

    /// buildx's host functions, as the policy layer declares them.
    fn host() -> Vec<Function> {
        let f = |name: &str, args: Vec<Type>, result: Type| Function {
            name: name.into(),
            decl: Type::Function {
                args,
                result: Some(Box::new(result)),
                variadic: None,
            },
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

    /// A chain of rules, each reading the one before it, and rules of every ref shape:
    /// refs with variables and numbers, rule refs with variables, else branches, functions.
    const SHAPES: &str = r#"package docker

default allow := false

r0 if input.image

r1 if { r0; input.image.repo != "x" }

r2 if { data.docker.r1; data.other[_] }

p[x] := 1 if some x in ["a", "b"]

p.q.r := 2

s contains v if { some v in data.docker.p; data.docker.p.q }

t := [1] if { data.docker.u } else := [2] if { data.docker.s[_] } else := [3]

u if data.docker.p["a"]

f(x) := y if { y := x; data.docker.t[0] }

allow if { r2; f(1) }

decision := {"allow": allow}
"#;

    /// Another package, which a ref of its root alone reads whole.
    const OTHER: &str = "package other\n\na := 1\n\nb contains 2\n";

    /// Refs whose strings stop short of a longer rule ref, then disagree with it or not:
    /// the type checker refuses some, so only the scan's equivalence reads them.
    const ODD: &str = r#"package odd

a if data.docker.p[1]

b if { some k; data.docker.p[k].z }

c if { some k; data.docker.p[k].r }

d if data.docker.p.q[0]
"#;

    /// Every corpus case's modules beside buildx's builtins, and the shapes above.
    fn module_sets() -> Vec<BTreeMap<String, Module>> {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../testdata/cases.json")).unwrap();
        let mut sets = Vec::new();
        let builtins = (
            "builtin/buildx_defaults.rego",
            include_str!("../buildx_defaults.rego"),
        );
        let mut srcs: Vec<Vec<(String, String)>> = vec![vec![
            (builtins.0.into(), builtins.1.into()),
            ("shapes.rego".into(), SHAPES.into()),
            ("other.rego".into(), OTHER.into()),
            ("odd.rego".into(), ODD.into()),
        ]];
        for c in cases.as_array().unwrap() {
            let mut set = vec![(builtins.0.to_string(), builtins.1.to_string())];
            for m in c["modules"].as_array().unwrap() {
                set.push((m[0].as_str().unwrap().into(), m[1].as_str().unwrap().into()));
            }
            srcs.push(set);
        }
        for set in srcs {
            let Ok(modules) = set
                .iter()
                .map(|(n, s)| parse_module(n, s).map(|m| (n.clone(), m)))
                .collect::<Result<BTreeMap<_, _>, _>>()
            else {
                continue;
            };
            let mut comp = Compiler::new(modules.clone(), host(), true);
            comp.compile();
            sets.push(modules);
            sets.push(comp.modules);
        }
        sets
    }

    #[test]
    fn the_index_finds_what_a_scan_of_every_rule_finds() {
        let sets = module_sets();
        assert!(sets.len() > 100, "{} module sets", sets.len());
        let mut edges = 0;
        for modules in sets {
            let comp = Compiler::new(modules, host(), true);
            let graph = DepGraph::new(&comp.modules);
            for (at, node) in graph.nodes.iter().enumerate() {
                let rule = comp.rule_node(node).unwrap();
                let want = comp.dependencies(rule);
                let got: Vec<RuleNode> = graph
                    .edges_of(at)
                    .iter()
                    .map(|&i| graph.nodes[i].clone())
                    .collect();
                assert_eq!(got, want, "{node:?}");
                edges += got.len();
            }
        }
        assert!(edges > 0, "no edges");
    }

    /// The edges of each ref shape, as OPA's graph has them.
    #[test]
    fn each_ref_shape_names_the_rules_it_may_read() {
        let modules: BTreeMap<String, Module> = [("shapes.rego", SHAPES), ("other.rego", OTHER)]
            .into_iter()
            .map(|(n, s)| (n.to_string(), parse_module(n, s).unwrap()))
            .collect();
        let mut comp = Compiler::new(modules, host(), true);
        comp.compile();
        assert!(comp.errors.is_empty(), "{:?}", comp.errors);
        let graph = DepGraph::new(&comp.modules);
        let name = |at: usize| {
            let (m, i, d) = &graph.nodes[at];
            let rule = &comp.modules[m].rules[*i];
            format!(
                "{}{}",
                crate::compile::text_of_ref(&rule.head.ref_path()),
                if *d > 0 {
                    format!("/else{d}")
                } else {
                    String::new()
                }
            )
        };
        let deps = |rule: &str| -> Vec<String> {
            let mut out: Vec<String> = (0..graph.nodes.len())
                .filter(|&at| name(at) == rule)
                .flat_map(|at| graph.edges_of(at).into_iter().map(name))
                .collect();
            out.sort();
            out.dedup();
            out
        };
        assert_eq!(deps("r1"), ["r0"]);
        // data.other[_] reads every rule of the package.
        assert_eq!(deps("r2"), ["a", "b", "r1"]);
        assert_eq!(deps("s"), ["p.q.r", "p[__local2__]"]);
        assert_eq!(deps("t"), ["u"]);
        assert_eq!(deps("t/else1"), ["s"]);
        assert_eq!(deps("t/else2"), Vec::<String>::new());
        assert_eq!(deps("u"), ["p[__local2__]"]);
        assert_eq!(deps("f"), ["t", "t/else1", "t/else2"]);
        assert_eq!(deps("allow"), ["f", "r2"]);
        assert_eq!(deps("decision"), ["allow"]);
    }

    /// A branch's edge leads to the rule it reads, which leads on to each branch: two
    /// rules of 1000 branches, one reading the other, keep 1000 edges for OPA's million.
    #[test]
    fn edges_lead_to_rules_then_branches() {
        let mut src = String::from("package p\n\na := 0 if input.x");
        for i in 1..1000 {
            src.push_str(&format!(" else := {i} if input.x"));
        }
        src.push_str("\n\nb := 0 if data.p.a");
        for i in 1..1000 {
            src.push_str(&format!(" else := {i} if data.p.a"));
        }
        src.push('\n');
        let modules: BTreeMap<String, Module> =
            [("p.rego".to_string(), parse_module("p.rego", &src).unwrap())].into();
        let graph = DepGraph::new(&modules);
        assert_eq!(graph.deps.iter().map(Vec::len).sum::<usize>(), 1000);
        assert_eq!(
            (0..graph.nodes.len())
                .map(|b| graph.edges_of(b).len())
                .sum::<usize>(),
            1_000_000
        );
    }
}

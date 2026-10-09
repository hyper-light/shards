//! OPA's rule index (ast/index.go, v1.14.1): for the rules at a path, a trie over the
//! refs their bodies compare with constants (`input.x == "a"`, a function's arguments,
//! `glob.match` patterns), which a lookup walks with the refs' values to find the rules
//! that may match, in OPA's order, with their else chains, the default rule, and
//! whether evaluation may stop at the first value. Built per path as the compiler's
//! buildRuleIndices builds them. Held to OPA by `tests/index.rs`, against the lookups
//! `scripts/rego/generate` records.
//!
//! Where OPA ranges over a Go map (refindices' frequency map and a trie node's scalars,
//! both HasherMaps over a map from OPA's hashes to chains; the rule tree's children),
//! the order is random per range: Go's small maps (eight entries or fewer) range over
//! their slots, filled in insertion order, from a random one. This index ranges from
//! the first slot, one of the orders OPA takes, with OPA's hashes (xxhash) and chains.
//! The oracle records every outcome OPA gave over many runs.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use crate::ast::{Expr, ExprTerms, Module, Rule, RuleKind, Term, TermValue};
use crate::compile::{Compiler, RuleNode, rule_ref};
use crate::value::{Value, number_compare};

/// What a lookup's resolver says of a ref (ast.ValueResolver): its value, undefined, or
/// unknown (OPA's UnknownValueErr, for refs in partial evaluation's save set).
#[derive(Debug, Clone)]
pub enum Resolved<V> {
    Value(V),
    Undefined,
    Unknown,
}

/// A lookup's resolver: each indexed ref to what it is.
pub type Resolver<'r, V, E> = dyn FnMut(&[Term]) -> Result<Resolved<V>, E> + 'r;

/// A scalar as the index compares it.
#[derive(Debug, Clone)]
pub enum Scalar<'a> {
    Null,
    Bool(bool),
    /// A number's text.
    Number(&'a str),
    String(Cow<'a, str>),
}

/// How the index sees a resolved value: a scalar, an array (its elements' scalars,
/// None for the others), or anything else.
#[derive(Debug, Clone)]
pub enum View<'a> {
    Scalar(Scalar<'a>),
    Array(Vec<Option<Scalar<'a>>>),
    Other,
}

/// A value a resolver hands the index.
pub trait IndexValue {
    fn view(&self) -> View<'_>;
}

fn term_scalar(t: &Term) -> Option<Scalar<'_>> {
    match &t.value {
        TermValue::Null => Some(Scalar::Null),
        TermValue::Bool(b) => Some(Scalar::Bool(*b)),
        TermValue::Number(n) => Some(Scalar::Number(n.text())),
        TermValue::String(s) => Some(Scalar::String(Cow::Borrowed(s))),
        _ => None,
    }
}

impl IndexValue for Term {
    fn view(&self) -> View<'_> {
        match &self.value {
            TermValue::Array(a) => View::Array(a.iter().map(term_scalar).collect()),
            _ => term_scalar(self).map_or(View::Other, View::Scalar),
        }
    }
}

fn value_scalar(v: &Value) -> Option<Scalar<'_>> {
    match v {
        Value::Null => Some(Scalar::Null),
        Value::Bool(b) => Some(Scalar::Bool(*b)),
        Value::Number(n) => Some(Scalar::Number(n.text())),
        Value::String(s) => Some(Scalar::String(Cow::Borrowed(s))),
        _ => None,
    }
}

impl IndexValue for Value {
    fn view(&self) -> View<'_> {
        match self {
            Value::Array(a) => View::Array(a.iter().map(value_scalar).collect()),
            _ => value_scalar(self).map_or(View::Other, View::Scalar),
        }
    }
}

/// A trie node's scalar key.
#[derive(Debug, Clone)]
enum Key {
    Null,
    Bool(bool),
    Number(Rc<str>),
    String(Rc<str>),
}

const P1: u64 = 11400714785074694791;
const P2: u64 = 14029467366897019727;
const P3: u64 = 1609587929392839161;
const P4: u64 = 9650029242287828579;
const P5: u64 = 2870177450012600261;

fn xx_round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn xx_merge(acc: u64, val: u64) -> u64 {
    (acc ^ xx_round(0, val)).wrapping_mul(P1).wrapping_add(P4)
}

fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    for (x, y) in a.iter_mut().zip(b) {
        *x = *y;
    }
    u64::from_le_bytes(a)
}

/// xxhash.Sum64 (cespare/xxhash v2), of which OPA's strings and vars hash.
fn xxhash64(b: &[u8]) -> u64 {
    let n = b.len() as u64;
    let mut h;
    let mut rest = b;
    if b.len() >= 32 {
        let (mut v1, mut v2, mut v3, mut v4) = (P1.wrapping_add(P2), P2, 0u64, 0u64.wrapping_sub(P1));
        while let Some((stripe, tail)) = rest.split_at_checked(32) {
            let (words, _) = stripe.as_chunks::<8>();
            let w = |i: usize| words.get(i).map_or(0, |x| u64::from_le_bytes(*x));
            v1 = xx_round(v1, w(0));
            v2 = xx_round(v2, w(1));
            v3 = xx_round(v3, w(2));
            v4 = xx_round(v4, w(3));
            rest = tail;
        }
        h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        for v in [v1, v2, v3, v4] {
            h = xx_merge(h, v);
        }
    } else {
        h = P5;
    }
    h = h.wrapping_add(n);
    while let Some((w, tail)) = rest.split_at_checked(8) {
        h = (h ^ xx_round(0, le64(w)))
            .rotate_left(27)
            .wrapping_mul(P1)
            .wrapping_add(P4);
        rest = tail;
    }
    if let Some((w, tail)) = rest.split_at_checked(4) {
        h = (h ^ le64(w).wrapping_mul(P1))
            .rotate_left(23)
            .wrapping_mul(P2)
            .wrapping_add(P3);
        rest = tail;
    }
    for &c in rest {
        h = (h ^ u64::from(c).wrapping_mul(P5))
            .rotate_left(11)
            .wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

/// Number.Hash: a short integer's value, else its float's (int(f), which saturates on
/// arm64, as Rust's conversion does), else its text's.
fn number_hash(s: &str) -> i64 {
    if s.len() < 4
        && let Ok(i) = s.parse::<i64>()
    {
        return i;
    }
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => f as i64,
        _ => xxhash64(s.as_bytes()) as i64,
    }
}

/// Value.Hash of a scalar.
fn scalar_hash(s: &Scalar<'_>) -> i64 {
    match s {
        Scalar::Null | Scalar::Bool(false) => 0,
        Scalar::Bool(true) => 1,
        Scalar::Number(n) => number_hash(n),
        Scalar::String(s) => xxhash64(s.as_bytes()) as i64,
    }
}

/// Value.Hash of a ref's part (termSliceHash sums them); composite parts beyond
/// arrays, which ground refs a rule index keeps do not have, hash as 0.
fn term_hash(t: &Term) -> i64 {
    match &t.value {
        TermValue::Var(v) => xxhash64(v.as_bytes()) as i64,
        TermValue::Ref(r) | TermValue::Array(r) => ref_hash(r),
        _ => term_scalar(t).map_or(0, |s| scalar_hash(&s)),
    }
}

fn ref_hash(r: &[Term]) -> i64 {
    r.iter().fold(0i64, |h, t| h.wrapping_add(term_hash(t)))
}

impl Key {
    fn of(t: &Term) -> Option<Key> {
        match &t.value {
            TermValue::Null => Some(Key::Null),
            TermValue::Bool(b) => Some(Key::Bool(*b)),
            TermValue::Number(n) => Some(Key::Number(n.text().into())),
            TermValue::String(s) => Some(Key::String(s.clone())),
            _ => None,
        }
    }

    fn scalar(&self) -> Scalar<'_> {
        match self {
            Key::Null => Scalar::Null,
            Key::Bool(b) => Scalar::Bool(*b),
            Key::Number(n) => Scalar::Number(n),
            Key::String(s) => Scalar::String(Cow::Borrowed(s)),
        }
    }

    /// ValueEqual.
    fn matches(&self, s: &Scalar<'_>) -> bool {
        match (self, s) {
            (Key::Null, Scalar::Null) => true,
            (Key::Bool(a), Scalar::Bool(b)) => a == b,
            (Key::Number(a), Scalar::Number(b)) => number_compare(a, b) == std::cmp::Ordering::Equal,
            (Key::String(a), Scalar::String(b)) => **a == **b,
            _ => false,
        }
    }
}

/// OPA's HasherMap as Go ranges over it: a Go map from hashes to chains, each chain
/// newest first. Small Go maps range over their slots, filled in insertion order,
/// from a random one; this takes the first.
#[derive(Debug, Clone)]
struct HasherMap<T> {
    buckets: Vec<(i64, Vec<T>)>,
}

impl<T> Default for HasherMap<T> {
    fn default() -> Self {
        HasherMap { buckets: Vec::new() }
    }
}

impl<T> HasherMap<T> {
    fn get(&self, hash: i64, eq: impl Fn(&T) -> bool) -> Option<&T> {
        self.buckets
            .iter()
            .find(|(h, _)| *h == hash)?
            .1
            .iter()
            .find(|x| eq(x))
    }

    fn get_mut(&mut self, hash: i64, eq: impl Fn(&T) -> bool) -> Option<&mut T> {
        self.buckets
            .iter_mut()
            .find(|(h, _)| *h == hash)?
            .1
            .iter_mut()
            .find(|x| eq(x))
    }

    /// Put of a key not there.
    fn put(&mut self, hash: i64, x: T) {
        match self.buckets.iter_mut().find(|(h, _)| *h == hash) {
            Some((_, chain)) => chain.insert(0, x),
            None => self.buckets.push((hash, vec![x])),
        }
    }

    fn iter(&self) -> impl Iterator<Item = &T> {
        self.buckets.iter().flat_map(|(_, chain)| chain.iter())
    }
}

/// A rule the index holds: where it is, and what its head says.
#[derive(Debug, Clone)]
struct Entry {
    id: RuleNode,
    value: Option<Term>,
    complete: bool,
}

#[derive(Debug, Default, Clone)]
struct TrieNode {
    r: Vec<Term>,
    /// The glob delimiters whose mappers apply here (valueMapper.Key).
    mappers: Vec<String>,
    next: Option<usize>,
    any: Option<usize>,
    undefined: Option<usize>,
    scalars: HasherMap<(Key, usize)>,
    array: Option<usize>,
    /// (insertion order, priority, entry).
    rules: Vec<(usize, usize, usize)>,
    value: Option<Term>,
    multiple: bool,
}

/// The rule index of one path (baseDocEqIndex).
#[derive(Debug, Clone)]
pub struct RuleIndex {
    entries: Vec<Entry>,
    nodes: Vec<TrieNode>,
    default_rule: Option<RuleNode>,
    kind: RuleKind,
    only_ground_refs: bool,
}

/// A lookup's answer (ast.IndexResult).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexResult {
    pub rules: Vec<RuleNode>,
    /// Each rule's else chain, for the rules that have one.
    pub else_: BTreeMap<RuleNode, Vec<RuleNode>>,
    pub default: Option<RuleNode>,
    pub kind: RuleKind,
    pub early_exit: bool,
    pub only_ground_refs: bool,
}

impl IndexResult {
    /// IndexResult.Empty.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.default.is_none()
    }
}

/// trieTraversalResult.
#[derive(Default)]
struct Traversal {
    unordered: HashMap<usize, Vec<(usize, usize)>>,
    ordering: Vec<usize>,
    exist: Option<Term>,
    multiple: bool,
}

const ANY: &str = "__any__";
const GLOB_WILDCARD: &str = "$globwildcard";

/// OPA's IsGround (comprehensions ground when their terms and bodies are).
fn ground(t: &Term) -> bool {
    match &t.value {
        TermValue::Null | TermValue::Bool(_) | TermValue::Number(_) | TermValue::String(_) => true,
        TermValue::Var(_) | TermValue::TemplateString { .. } => false,
        TermValue::Ref(r) => r.iter().skip(1).all(ground),
        TermValue::Array(a) | TermValue::Set(a) | TermValue::Call(a) => a.iter().all(ground),
        TermValue::Object(o) => o.iter().all(|(k, v)| ground(k) && ground(v)),
        TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => ground(x) && b.iter().all(expr_ground),
        TermValue::ObjectCompr(k, v, b) => ground(k) && ground(v) && b.iter().all(expr_ground),
    }
}

/// Expr.IsGround.
fn expr_ground(e: &Expr) -> bool {
    match &e.terms {
        ExprTerms::Call(c) => c.iter().skip(1).all(ground),
        ExprTerms::Term(t) => ground(t),
        ExprTerms::Some(_) | ExprTerms::Every(_) => true,
    }
}

/// IsConstant.
fn constant(t: &Term) -> bool {
    match &t.value {
        TermValue::Null | TermValue::Bool(_) | TermValue::Number(_) | TermValue::String(_) => true,
        TermValue::Array(a) | TermValue::Set(a) => a.iter().all(constant),
        TermValue::Object(o) => o.iter().all(|(k, v)| constant(k) && constant(v)),
        _ => false,
    }
}

/// Head.DocKind() == CompleteDoc: no key, and no dynamic part in the ref (HasDynamicRef).
fn complete_doc(rule: &Rule) -> bool {
    if rule.head.key.is_some() {
        return false;
    }
    let r = rule.head.ref_path();
    let dynamic = match r.first().map(|t| &t.value) {
        Some(TermValue::Call(_)) => Some(0),
        _ => r.iter().skip(1).position(|t| !constant(t)).map(|p| p + 1),
    };
    !matches!(dynamic, Some(p) if p > 0 && p < r.len())
}

/// Whether a term is the ref `parts` names (a variable, then strings).
fn is_ref(t: &Term, parts: &[&str]) -> bool {
    let Some(r) = t.as_ref() else { return false };
    r.len() == parts.len()
        && r.iter().zip(parts).enumerate().all(|(i, (x, p))| match &x.value {
            TermValue::Var(v) if i == 0 => &**v == *p,
            TermValue::String(s) if i > 0 => &**s == *p,
            _ => false,
        })
}

fn operator(e: &Expr) -> Option<&Term> {
    match &e.terms {
        ExprTerms::Call(c) => c.first(),
        _ => None,
    }
}

/// skipIndexing: rules that call these are not indexed.
fn skips_indexing(e: &Expr) -> bool {
    operator(e).is_some_and(|op| is_ref(op, &["internal", "print"]) || is_ref(op, &["internal", "test_case"]))
}

/// refindex.
#[derive(Debug, Clone)]
struct RefIndex {
    r: Vec<Term>,
    value: Term,
    mapper: Option<String>,
}

/// The rule tree as OPA's buildRuleIndices walks it (TreeNode): keys, rules, children
/// in the order they were added.
#[derive(Debug, Default)]
struct TreeNode {
    key: String,
    values: Vec<(String, usize)>,
    children: Vec<usize>,
}

#[derive(Debug)]
struct Tree {
    nodes: Vec<TreeNode>,
}

impl Tree {
    /// TreeNode.Child.
    fn child(&self, n: usize, t: &Term) -> Option<usize> {
        if matches!(t.value, TermValue::Ref(_) | TermValue::Call(_)) {
            return None;
        }
        let k = t.to_string();
        let node = self.nodes.get(n)?;
        node.children
            .iter()
            .copied()
            .find(|&c| self.nodes.get(c).is_some_and(|x| x.key == k))
    }

    /// TreeNode.add.
    fn add(&mut self, path: &[Term], rule: Option<(String, usize)>) {
        let mut n = 0;
        for t in path {
            n = match self.child(n, t) {
                Some(c) => c,
                None => {
                    let c = self.nodes.len();
                    self.nodes.push(TreeNode {
                        key: t.to_string(),
                        ..TreeNode::default()
                    });
                    if let Some(p) = self.nodes.get_mut(n) {
                        p.children.push(c);
                    }
                    c
                }
            };
        }
        if let (Some(r), Some(node)) = (rule, self.nodes.get_mut(n)) {
            node.values.push(r);
        }
    }

    /// isVirtual over a ref's ground prefix.
    fn is_virtual(&self, r: &[Term]) -> bool {
        let mut n = 0;
        for t in crate::compile::ground_prefix(r).iter() {
            let Some(c) = self.child(n, t) else { return false };
            if self.nodes.get(c).is_some_and(|x| !x.values.is_empty()) {
                return true;
            }
            n = c;
        }
        true
    }

    /// TreeNode.DepthFirst from a node, collecting every node's rules.
    fn collect(&self, n: usize, out: &mut Vec<(String, usize)>) {
        if let Some(node) = self.nodes.get(n) {
            out.extend(node.values.iter().cloned());
            for &c in &node.children {
                self.collect(c, out);
            }
        }
    }
}

/// NewModuleTree then NewRuleTree: modules by name within a package, packages in the
/// order they first appear, each package's modules before its subpackages'.
fn rule_tree(modules: &BTreeMap<String, Module>) -> Tree {
    // The module tree: (key, module names, children).
    let mut mtree: Vec<(String, Vec<&str>, Vec<usize>)> = vec![(String::new(), Vec::new(), Vec::new())];
    for (name, m) in modules {
        let mut n = 0;
        for t in &m.package.path {
            let k = t.to_string();
            let found = mtree.get(n).and_then(|x| {
                x.2.iter()
                    .copied()
                    .find(|&c| mtree.get(c).is_some_and(|y| y.0 == k))
            });
            n = match found {
                Some(c) => c,
                None => {
                    let c = mtree.len();
                    mtree.push((k, Vec::new(), Vec::new()));
                    if let Some(p) = mtree.get_mut(n) {
                        p.2.push(c);
                    }
                    c
                }
            };
        }
        if let Some(x) = mtree.get_mut(n) {
            x.1.push(name);
        }
    }
    let mut order: Vec<&str> = Vec::new();
    let mut stack = vec![0];
    while let Some(n) = stack.pop() {
        if let Some(x) = mtree.get(n) {
            order.extend(x.1.iter().copied());
            stack.extend(x.2.iter().rev().copied());
        }
    }
    let mut tree = Tree {
        nodes: vec![TreeNode::default()],
    };
    for name in order {
        let Some(m) = modules.get(name) else { continue };
        if m.rules.is_empty() {
            tree.add(&m.package.path, None);
        }
        for (i, r) in m.rules.iter().enumerate() {
            tree.add(
                &crate::compile::ground_prefix(&rule_ref(&m.package.path, r)),
                Some((name.to_string(), i)),
            );
        }
    }
    tree
}

/// buildRuleIndices: an index for each rule tree node with rules, a node whose rules'
/// refs are not ground taking in all the rules below it, keyed by the node's path.
pub fn build_rule_indices(c: &mut Compiler) {
    let tree = rule_tree(&c.modules);
    let mut out = BTreeMap::new();
    let mut path = Vec::new();
    walk(c, &tree, 0, &mut path, &mut out);
    c.indices = out;
}

fn walk(
    c: &Compiler,
    tree: &Tree,
    n: usize,
    path: &mut Vec<String>,
    out: &mut BTreeMap<Vec<String>, RuleIndex>,
) {
    let Some(node) = tree.nodes.get(n) else { return };
    if n != 0 {
        path.push(node.key.clone());
    }
    let mut stop = false;
    if !node.values.is_empty() {
        let mut rules = node.values.clone();
        let non_ground = rules
            .iter()
            .filter_map(|id| c.rule(id))
            .any(|r| !r.head.ref_path().iter().skip(1).all(ground));
        if non_ground {
            for &ch in &node.children {
                tree.collect(ch, &mut rules);
            }
        }
        if let Some(index) = RuleIndex::build(c, &rules, &|r| tree.is_virtual(r)) {
            out.insert(path.clone(), index);
        }
        stop = non_ground;
    }
    if !stop {
        for &ch in &node.children {
            walk(c, tree, ch, path, out);
        }
    }
    if n != 0 {
        path.pop();
    }
}

/// refindices.
struct RefIndices<'a> {
    is_virtual: &'a dyn Fn(&[Term]) -> bool,
    rules: HashMap<usize, Vec<RefIndex>>,
    /// Each ref and how often rules index it (Go ranges over it as HasherMap shows).
    frequency: HasherMap<(Vec<Term>, i64)>,
}

fn ref_equal(a: &[Term], b: &[Term]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.equal(y))
}

/// FunctionArgRootDocument[i].
fn arg_ref(i: usize) -> Vec<Term> {
    vec![
        Term::var("args", None),
        Term::new(
            TermValue::Number(crate::value::Number(i.to_string().into())),
            None,
        ),
    ]
}

/// indexValue.
fn index_value(b: &Term) -> Option<Term> {
    match &b.value {
        TermValue::Null
        | TermValue::Bool(_)
        | TermValue::Number(_)
        | TermValue::String(_)
        | TermValue::Var(_) => Some(b.clone()),
        TermValue::Array(a) => a
            .iter()
            .all(|x| {
                matches!(
                    x.value,
                    TermValue::Null
                        | TermValue::Bool(_)
                        | TermValue::Number(_)
                        | TermValue::String(_)
                        | TermValue::Var(_)
                )
            })
            .then(|| b.clone()),
        _ => None,
    }
}

/// splitStringEscaped: s split at bytes that are delimiter runes, unless escaped by a
/// reverse solidus. (Go compares each byte as the rune of the same number.)
fn split_escaped(s: &str, delim: &str) -> Vec<String> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let (mut last, mut escaped) = (0, false);
    for (curr, &c) in b.iter().enumerate() {
        if c == b'\\' || escaped {
            escaped = !escaped;
            continue;
        }
        if delim.chars().any(|d| d as u32 == u32::from(c)) {
            out.push(String::from_utf8_lossy(b.get(last..curr).unwrap_or_default()).into_owned());
            last = curr + 1;
        }
    }
    out.push(String::from_utf8_lossy(b.get(last..).unwrap_or_default()).into_owned());
    out
}

/// globDelimiterToString.
fn glob_delimiter(t: &Term) -> Option<String> {
    let TermValue::Array(a) = &t.value else {
        return None;
    };
    if a.is_empty() {
        return Some(".".into());
    }
    a.iter()
        .map(|x| x.as_string())
        .collect::<Option<Vec<_>>>()
        .map(|v| v.concat())
}

/// globPatternToArray.
fn glob_pattern(t: &Term, delim: &str) -> Option<Term> {
    let s = t.as_string()?;
    let mut arr = Vec::new();
    for part in split_escaped(s, delim) {
        if part == "*" {
            arr.push(Term::var(GLOB_WILDCARD, None));
            continue;
        }
        let mut escaped = false;
        for c in part.chars() {
            if c == '\\' {
                escaped = !escaped;
                continue;
            }
            if !escaped && matches!(c, '[' | '?' | '{' | '*') {
                return None;
            }
            escaped = false;
        }
        arr.push(Term::string(&part, None));
    }
    Some(Term::new(TermValue::Array(arr), None))
}

impl RefIndices<'_> {
    /// eqOperandsToRefAndValue.
    fn eq_operands(&self, args: &[Term], a: &Term, b: &Term) -> Option<RefIndex> {
        match &a.value {
            TermValue::Var(_) => {
                for (i, arg) in args.iter().enumerate() {
                    if crate::compare::term_compare(arg, a) == std::cmp::Ordering::Equal
                        && let Some(value) = index_value(b)
                    {
                        return Some(RefIndex {
                            r: arg_ref(i),
                            value,
                            mapper: None,
                        });
                    }
                }
                None
            }
            TermValue::Ref(v) => {
                let root = v.first().and_then(Term::as_var);
                if !matches!(root, Some("data" | "input")) {
                    return None;
                }
                if (self.is_virtual)(v) {
                    return None;
                }
                if v.iter().any(|t| matches!(t.value, TermValue::Ref(_))) || !v.iter().skip(1).all(ground) {
                    return None;
                }
                index_value(b).map(|value| RefIndex {
                    r: v.clone(),
                    value,
                    mapper: None,
                })
            }
            _ => None,
        }
    }

    /// refindices.updateEq.
    fn update_eq(&mut self, rule: usize, args: &[Term], a: &Term, b: &Term) {
        if let Some(idx) = self.eq_operands(args, a, b) {
            self.insert(rule, idx);
        } else if let Some(idx) = self.eq_operands(args, b, a) {
            self.insert(rule, idx);
        }
    }

    /// refindices.updateGlobMatch.
    fn update_glob_match(&mut self, rule: usize, args: &[Term], e: &Expr) {
        let (Some(pattern), Some(delim), Some(matched)) = (e.operand(0), e.operand(1), e.operand(2)) else {
            return;
        };
        let Some(delim) = glob_delimiter(delim) else {
            return;
        };
        let Some(arr) = glob_pattern(pattern, &delim) else {
            return;
        };
        let Some(mv) = matched.as_var() else { return };
        let mut r = None;
        for other in self.rules.get(&rule).into_iter().flatten() {
            if other.value.as_var() == Some(mv) {
                r = Some(other.r.clone());
            }
        }
        if r.is_none() {
            for (j, arg) in args.iter().enumerate() {
                if arg.equal(matched) {
                    r = Some(arg_ref(j));
                }
            }
        }
        if let Some(r) = r {
            self.insert(
                rule,
                RefIndex {
                    r,
                    value: arr,
                    mapper: Some(delim),
                },
            );
        }
    }

    /// refindices.Update.
    fn update(&mut self, rule: usize, args: &[Term], e: &Expr) {
        if !e.with.is_empty() || e.negated {
            return;
        }
        let op = operator(e);
        if op.is_none()
            && let ExprTerms::Term(t) = &e.terms
            && matches!(t.value, TermValue::Ref(_))
        {
            self.update_eq(rule, args, t, &Term::var(ANY, None));
        }
        let Some(op) = op else { return };
        let operands = match &e.terms {
            ExprTerms::Call(c) => c.len().saturating_sub(1),
            _ => 0,
        };
        if is_ref(op, &["eq"]) || (is_ref(op, &["equal"]) && operands == 2) {
            if let (Some(a), Some(b)) = (e.operand(0), e.operand(1)) {
                self.update_eq(rule, args, a, b);
            }
        } else if is_ref(op, &["glob", "match"]) && operands == 3 {
            self.update_glob_match(rule, args, e);
        }
    }

    /// refindices.insert.
    fn insert(&mut self, rule: usize, index: RefIndex) {
        let hash = ref_hash(&index.r);
        match self.frequency.get_mut(hash, |(r, _)| ref_equal(r, &index.r)) {
            Some((_, n)) => *n = n.saturating_add(1),
            None => self.frequency.put(hash, (index.r.clone(), 1)),
        }
        let list = self.rules.entry(rule).or_default();
        match list.iter_mut().find(|x| ref_equal(&x.r, &index.r)) {
            Some(slot) => *slot = index,
            None => list.push(index),
        }
    }

    fn index(&self, rule: usize, r: &[Term]) -> Option<&RefIndex> {
        self.rules.get(&rule)?.iter().find(|x| ref_equal(&x.r, r))
    }

    /// refindices.Sorted: sort.Slice over the frequency map's refs, more frequent
    /// first, then by their heads' locations. OPA's less function reads each count at
    /// the position the ref started from, not where the sort moved it; for the slices
    /// a rule index has (twelve refs or fewer), sort.Slice is an insertion sort, run
    /// here as Go runs it, from the order the map ranges in.
    fn sorted(&self) -> Vec<Vec<Term>> {
        let counts: Vec<i64> = self.frequency.iter().map(|(_, n)| *n).collect();
        let mut sorted: Vec<Vec<Term>> = self.frequency.iter().map(|(r, _)| r.clone()).collect();
        let less = |sorted: &[Vec<Term>], a: usize, b: usize| -> bool {
            let (ca, cb) = (
                counts.get(a).copied().unwrap_or(0),
                counts.get(b).copied().unwrap_or(0),
            );
            if ca > cb {
                return true;
            }
            if cb > ca {
                return false;
            }
            let la = sorted.get(a).and_then(|r| r.first()).and_then(|t| t.loc.as_ref());
            let lb = sorted.get(b).and_then(|r| r.first()).and_then(|t| t.loc.as_ref());
            loc_compare(la, lb) == std::cmp::Ordering::Less
        };
        for i in 1..sorted.len() {
            let mut j = i;
            while j > 0 && less(&sorted, j, j - 1) {
                sorted.swap(j, j - 1);
                j -= 1;
            }
        }
        sorted
    }
}

/// Location.Compare: a missing location after any other.
fn loc_compare(a: Option<&crate::ast::Location>, b: Option<&crate::ast::Location>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x
            .file
            .cmp(&y.file)
            .then(x.row.cmp(&y.row))
            .then(x.col.cmp(&y.col)),
    }
}

/// A rule and its else chain, with each one's depth (WalkRules on a rule).
fn chain(rule: &Rule) -> Vec<&Rule> {
    let mut out = Vec::new();
    let mut r = Some(rule);
    while let Some(x) = r {
        out.push(x);
        r = x.else_.as_deref();
    }
    out
}

impl RuleIndex {
    /// baseDocEqIndex.Build over the rules (module, index) of one tree node; None when
    /// there are none.
    fn build(
        c: &Compiler,
        ids: &[(String, usize)],
        is_virtual: &dyn Fn(&[Term]) -> bool,
    ) -> Option<RuleIndex> {
        let rules: Vec<(&(String, usize), &Rule)> =
            ids.iter().filter_map(|id| c.rule(id).map(|r| (id, r))).collect();
        let (_, first) = rules.first()?;
        let mut index = RuleIndex {
            entries: Vec::new(),
            nodes: vec![TrieNode::default()],
            default_rule: None,
            kind: first.head.kind(),
            only_ground_refs: true,
        };
        let mut indices = RefIndices {
            is_virtual,
            rules: HashMap::new(),
            frequency: HasherMap::default(),
        };
        // The walked rules' entries, per top-level rule, None for default rules.
        let mut walked: Vec<Vec<Option<usize>>> = Vec::new();
        for ((module, i), rule) in &rules {
            let mut entries = Vec::new();
            for (depth, r) in chain(rule).into_iter().enumerate() {
                let id = (module.clone(), *i, depth);
                if r.default {
                    index.default_rule = Some(id);
                    entries.push(None);
                    continue;
                }
                if index.only_ground_refs {
                    index.only_ground_refs = r.head.ref_path().iter().skip(1).all(ground);
                }
                let e = index.entries.len();
                index.entries.push(Entry {
                    id,
                    value: r.head.value.clone(),
                    complete: complete_doc(r),
                });
                entries.push(Some(e));
                if !r.body.iter().any(skips_indexing) {
                    for expr in &r.body {
                        indices.update(e, &r.head.args, expr);
                    }
                }
            }
            walked.push(entries);
        }
        let sorted = indices.sorted();
        for (idx, entries) in walked.iter().enumerate() {
            for (prio, &e) in entries.iter().flatten().enumerate() {
                let mut node = 0;
                if indices.rules.get(&e).is_some_and(|l| !l.is_empty()) {
                    for r in &sorted {
                        let ri = indices.index(e, r);
                        node = index.insert(
                            node,
                            r,
                            ri.map(|x| &x.value),
                            ri.and_then(|x| x.mapper.as_deref()),
                        );
                    }
                }
                index.append(node, idx, prio, e);
            }
        }
        Some(index)
    }

    fn new_node(&mut self) -> usize {
        self.nodes.push(TrieNode::default());
        self.nodes.len() - 1
    }

    fn node(&self, n: usize) -> Option<&TrieNode> {
        self.nodes.get(n)
    }

    /// trieNode.Insert.
    fn insert(&mut self, node: usize, r: &[Term], value: Option<&Term>, mapper: Option<&str>) -> usize {
        let next = match self.node(node).and_then(|x| x.next) {
            Some(n) => n,
            None => {
                let n = self.new_node();
                if let Some(x) = self.nodes.get_mut(n) {
                    x.r = r.to_vec();
                }
                if let Some(x) = self.nodes.get_mut(node) {
                    x.next = Some(n);
                }
                n
            }
        };
        if let Some(m) = mapper
            && let Some(x) = self.nodes.get_mut(next)
            && !x.mappers.iter().any(|k| k == m)
        {
            x.mappers.push(m.to_string());
        }
        self.insert_value(next, value)
    }

    fn child(&mut self, node: usize, get: fn(&mut TrieNode) -> &mut Option<usize>) -> usize {
        if let Some(c) = self.nodes.get_mut(node).and_then(|x| *get(x)) {
            return c;
        }
        let c = self.new_node();
        if let Some(x) = self.nodes.get_mut(node) {
            *get(x) = Some(c);
        }
        c
    }

    fn scalar_child(&mut self, node: usize, key: Key) -> usize {
        let hash = scalar_hash(&key.scalar());
        let s = key.scalar();
        if let Some(c) = self
            .node(node)
            .and_then(|x| x.scalars.get(hash, |(k, _)| k.matches(&s)))
            .map(|(_, c)| *c)
        {
            return c;
        }
        let c = self.new_node();
        if let Some(x) = self.nodes.get_mut(node) {
            x.scalars.put(hash, (key, c));
        }
        c
    }

    /// trieNode.insertValue.
    fn insert_value(&mut self, node: usize, value: Option<&Term>) -> usize {
        let Some(value) = value else {
            return self.child(node, |x| &mut x.undefined);
        };
        match &value.value {
            TermValue::Var(_) => self.child(node, |x| &mut x.any),
            TermValue::Array(a) => {
                let arr = self.child(node, |x| &mut x.array);
                self.insert_array(arr, a)
            }
            _ => match Key::of(value) {
                Some(k) => self.scalar_child(node, k),
                // indexValue admits nothing else.
                None => node,
            },
        }
    }

    /// trieNode.insertArray.
    fn insert_array(&mut self, node: usize, arr: &[Term]) -> usize {
        let Some((head, rest)) = arr.split_first() else {
            return node;
        };
        let c = match &head.value {
            TermValue::Var(_) => self.child(node, |x| &mut x.any),
            _ => match Key::of(head) {
                Some(k) => self.scalar_child(node, k),
                None => return node,
            },
        };
        self.insert_array(c, rest)
    }

    /// trieNode.append.
    fn append(&mut self, node: usize, idx: usize, prio: usize, e: usize) {
        let Some(entry) = self.entries.get(e) else { return };
        let (value, complete) = (entry.value.clone(), entry.complete);
        let Some(x) = self.nodes.get_mut(node) else { return };
        x.rules.push((idx, prio, e));
        if let (Some(nv), Some(v)) = (&x.value, &value)
            && !nv.equal(v)
        {
            x.multiple = true;
        }
        if x.value.is_none() && complete {
            x.value = value;
        }
    }

    /// trieTraversalResult.Add.
    fn add(&self, n: usize, tr: &mut Traversal) {
        let Some(x) = self.node(n) else { return };
        for &(idx, prio, e) in &x.rules {
            let list = tr.unordered.entry(idx).or_default();
            if list.is_empty() {
                tr.ordering.push(idx);
            }
            list.push((prio, e));
        }
        if x.multiple {
            tr.multiple = true;
        }
        let Some(v) = &x.value else { return };
        if tr.multiple {
            return;
        }
        if ground(v) && tr.exist.is_none() || tr.exist.as_ref().is_some_and(|ex| ex.equal(v)) {
            tr.exist = Some(v.clone());
            return;
        }
        tr.multiple = true;
    }

    /// trieNode.Traverse.
    fn traverse_node<V: IndexValue, E>(
        &self,
        n: Option<usize>,
        res: &mut Resolver<'_, V, E>,
        tr: &mut Traversal,
    ) -> Result<(), E> {
        let Some(n) = n else { return Ok(()) };
        self.add(n, tr);
        self.traverse(self.node(n).and_then(|x| x.next), res, tr)
    }

    /// trieNode.traverse: a ref node, by its ref's value.
    fn traverse<V: IndexValue, E>(
        &self,
        n: Option<usize>,
        res: &mut Resolver<'_, V, E>,
        tr: &mut Traversal,
    ) -> Result<(), E> {
        let Some(x) = n.and_then(|n| self.node(n)) else {
            return Ok(());
        };
        let v = match res(&x.r)? {
            Resolved::Unknown => return self.traverse_unknown(n, res, tr),
            Resolved::Undefined => None,
            Resolved::Value(v) => Some(v),
        };
        self.traverse_node(x.undefined, res, tr)?;
        let Some(v) = v else { return Ok(()) };
        self.traverse_node(x.any, res, tr)?;
        let view = v.view();
        self.traverse_value(x, res, tr, &view)?;
        for delim in &x.mappers {
            if let View::Scalar(Scalar::String(s)) = &view {
                let mapped = View::Array(
                    split_escaped(s, delim)
                        .into_iter()
                        .map(|p| Some(Scalar::String(Cow::Owned(p))))
                        .collect(),
                );
                self.traverse_value(x, res, tr, &mapped)?;
            }
        }
        Ok(())
    }

    /// trieNode.traverseValue.
    fn traverse_value<V: IndexValue, E>(
        &self,
        x: &TrieNode,
        res: &mut Resolver<'_, V, E>,
        tr: &mut Traversal,
        view: &View<'_>,
    ) -> Result<(), E> {
        match view {
            View::Array(a) => self.traverse_array(x.array, res, tr, a),
            View::Scalar(s) => match x.scalars.get(scalar_hash(s), |(k, _)| k.matches(s)) {
                Some((_, c)) => self.traverse_node(Some(*c), res, tr),
                None => Ok(()),
            },
            View::Other => Ok(()),
        }
    }

    /// trieNode.traverseArray.
    fn traverse_array<V: IndexValue, E>(
        &self,
        n: Option<usize>,
        res: &mut Resolver<'_, V, E>,
        tr: &mut Traversal,
        arr: &[Option<Scalar<'_>>],
    ) -> Result<(), E> {
        let Some(x) = n.and_then(|n| self.node(n)) else {
            return Ok(());
        };
        let Some((head, rest)) = arr.split_first() else {
            return self.traverse_node(n, res, tr);
        };
        self.traverse_array(x.any, res, tr, rest)?;
        let Some(head) = head else { return Ok(()) };
        let child = x
            .scalars
            .get(scalar_hash(head), |(k, _)| k.matches(head))
            .map(|(_, c)| *c);
        self.traverse_array(child, res, tr, rest)
    }

    /// trieNode.traverseUnknown. (OPA drops a scalar child's error, stopping there.)
    fn traverse_unknown<V: IndexValue, E>(
        &self,
        n: Option<usize>,
        res: &mut Resolver<'_, V, E>,
        tr: &mut Traversal,
    ) -> Result<(), E> {
        let Some(x) = n.and_then(|n| self.node(n)) else {
            return Ok(());
        };
        self.traverse_node(n, res, tr)?;
        self.traverse_unknown(x.undefined, res, tr)?;
        self.traverse_unknown(x.any, res, tr)?;
        self.traverse_unknown(x.array, res, tr)?;
        for (_, c) in x.scalars.iter() {
            if self.traverse_unknown(Some(*c), res, tr).is_err() {
                break;
            }
        }
        Ok(())
    }

    /// The result's rules from a traversal: each top-level rule's walked rules by
    /// priority, the first the rule, the rest its else chain.
    fn result(&self, mut tr: Traversal) -> (IndexResult, bool) {
        let mut out = IndexResult {
            rules: Vec::new(),
            else_: BTreeMap::new(),
            default: self.default_rule.clone(),
            kind: self.kind,
            early_exit: false,
            only_ground_refs: self.only_ground_refs,
        };
        let mut roots = Vec::new();
        for pos in &tr.ordering {
            let Some(nodes) = tr.unordered.get_mut(pos) else {
                continue;
            };
            nodes.sort_by_key(|(prio, _)| *prio);
            let ids: Vec<RuleNode> = nodes
                .iter()
                .filter_map(|(_, e)| self.entries.get(*e).map(|x| x.id.clone()))
                .collect();
            let Some((root, rest)) = ids.split_first() else {
                continue;
            };
            out.rules.push(root.clone());
            if let Some((_, e)) = nodes.first() {
                roots.push(*e);
            }
            if !rest.is_empty() {
                out.else_.insert(root.clone(), rest.to_vec());
            }
        }
        if !tr.multiple {
            // Lookup: a rule that is not a complete document, or two values, end it.
            let mut last: Option<&Term> = None;
            for e in &roots {
                let Some(entry) = self.entries.get(*e) else {
                    continue;
                };
                if !entry.complete {
                    tr.multiple = true;
                    break;
                }
                if let Some(v) = &entry.value {
                    if last.is_some_and(|l| !l.equal(v)) {
                        tr.multiple = true;
                        break;
                    }
                    last = Some(v);
                }
            }
        }
        (out, tr.multiple)
    }

    /// baseDocEqIndex.Lookup: the rules that may match the values the resolver gives
    /// the indexed refs.
    pub fn lookup<V: IndexValue, E>(&self, resolver: &mut Resolver<'_, V, E>) -> Result<IndexResult, E> {
        let mut tr = Traversal::default();
        self.traverse_node(Some(0), resolver, &mut tr)?;
        let (mut out, multiple) = self.result(tr);
        out.early_exit = !multiple;
        Ok(out)
    }

    /// baseDocEqIndex.AllRules: every rule, as with indexing off.
    pub fn all_rules(&self) -> IndexResult {
        let mut tr = Traversal::default();
        self.walk_all(0, &mut tr);
        let multiple = tr.multiple;
        let (mut out, _) = self.result(Traversal { multiple: true, ..tr });
        out.early_exit = !multiple;
        out
    }

    /// trieNode.Do with the ruleWalker.
    fn walk_all(&self, n: usize, tr: &mut Traversal) {
        let Some(x) = self.node(n) else { return };
        self.add(n, tr);
        for c in [x.any, x.undefined].into_iter().flatten() {
            self.walk_all(c, tr);
        }
        for (_, c) in x.scalars.iter() {
            self.walk_all(*c, tr);
        }
        for c in [x.array, x.next].into_iter().flatten() {
            self.walk_all(c, tr);
        }
    }
}

impl Compiler {
    /// Compiler.RuleIndex: the index of the rules at exactly this path.
    pub fn rule_index(&self, path: &[Term]) -> Option<&RuleIndex> {
        if path
            .iter()
            .any(|t| matches!(t.value, TermValue::Ref(_) | TermValue::Call(_)))
        {
            return None;
        }
        self.indices.get(&crate::compile::ref_key(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What OPA v1.14.1's String.Hash and Number.Hash give, on arm64 (measured).
    #[test]
    fn hashes_are_opas() {
        for (s, h) in [
            ("", -1205034819632174695i64),
            ("a", -3292477735350538661),
            ("abc", 4952883123889572249),
            ("input", 327217269536945793),
            ("Nobody inspects the spammish repetition", -302119147016844303),
            ("0123456789abcdef0123456789abcdef0123456789", -6385663626540185060),
        ] {
            assert_eq!(xxhash64(s.as_bytes()) as i64, h, "{s:?}");
        }
        for (n, h) in [
            ("1", 1i64),
            ("1.0", 1),
            ("100", 100),
            ("1e2", 100),
            ("-1", -1),
            ("1e400", -3991343253117230208),
            ("12345678901234567890123", 9223372036854775807),
            ("0.5", 0),
            ("1e300", 9223372036854775807),
        ] {
            assert_eq!(number_hash(n), h, "{n}");
        }
    }
}

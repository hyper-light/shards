//! OPA's evaluator (topdown/eval.go, v1.14.1): queries over compiled modules, by
//! unification with continuations, as OPA's topdown evaluates them: bindings per query,
//! virtual documents from rules with OPA's caches, early exit, conflicts and errors in
//! OPA's words.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{Body, Expr, ExprTerms, Location, Module, Rule, RuleKind, Term, TermValue};
use crate::compare::term_compare;
use crate::compile::{ground_prefix, rule_ref, Compiler};
use crate::funcs::{self, BuiltinError};
use crate::value::{Number, Value};

/// An evaluation error: OPA's topdown.Error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalError {
    pub code: &'static str,
    pub message: String,
    pub loc: Option<Location>,
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.loc {
            Some(l) if !l.file.is_empty() => write!(f, "{}:{}: {}: {}", l.file, l.row, self.code, self.message),
            Some(l) => write!(f, "{}:{}: {}: {}", l.row, l.col, self.code, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

pub const CONFLICT_ERR: &str = "eval_conflict_error";
pub const TYPE_ERR: &str = "eval_type_error";
pub const BUILTIN_ERR: &str = "eval_builtin_error";
pub const INTERNAL_ERR: &str = "eval_internal_error";
pub const WITH_MERGE_ERR: &str = "eval_with_merge_error";

/// What unwinds an evaluation: an error, or OPA's early exit (`deferred` when it passed
/// a query that must go on enumerating), each wrapping the one it came from.
#[derive(Debug, Clone)]
enum Flow {
    Err(EvalError),
    Early { deferred: bool, prev: Option<Box<Flow>> },
}

type R = Result<(), Flow>;

fn err(code: &'static str, loc: Option<Location>, message: impl Into<String>) -> Flow {
    Flow::Err(EvalError { code, message: message.into(), loc })
}

/// suppressEarlyExit: one level of early exit unwrapped.
fn suppress(r: R) -> R {
    match r {
        Err(Flow::Early { prev, .. }) => match prev {
            Some(p) => Err(*p),
            None => Ok(()),
        },
        r => r,
    }
}

fn is_deferred(r: &R) -> bool {
    matches!(r, Err(Flow::Early { deferred: true, .. }))
}

/// A function the host answers: buildx's own, in policies.
pub trait Host {
    fn call(&mut self, name: &str, args: &[Value]) -> Result<Option<Value>, String>;
}

/// A rule, its path (Rule.Ref) and where it is.
#[derive(Debug)]
pub struct RuleRec {
    pub rule: Rule,
    pub path: Vec<Term>,
    /// The rule's else chain, in order.
    pub elses: Vec<Rc<RuleRec>>,
}

/// A node of the rule tree: children by key, sorted as OPA sorts them.
#[derive(Debug, Default)]
pub struct TreeNode {
    pub children: Vec<(Term, TreeNode)>,
    pub values: Vec<Rc<RuleRec>>,
}

impl TreeNode {
    fn child(&self, k: &Term) -> Option<&TreeNode> {
        if matches!(k.value, TermValue::Ref(_) | TermValue::Call(_)) {
            return None;
        }
        self.children.iter().find(|(key, _)| key.equal(k)).map(|(_, n)| n)
    }

    fn find(&self, r: &[Term]) -> Option<&TreeNode> {
        let mut n = self;
        for t in r {
            n = n.child(t)?;
        }
        Some(n)
    }
}

/// Compiled modules, ready to evaluate.
#[derive(Debug)]
pub struct Program {
    pub root: TreeNode,
    host_names: Vec<String>,
}

impl Program {
    pub fn new(c: &Compiler, host_names: Vec<String>) -> Program {
        let mut root = TreeNode::default();
        for m in c.modules.values() {
            add_module(&mut root, m);
        }
        sort_tree(&mut root);
        Program { root, host_names }
    }

    fn rules_at(&self, r: &[Term]) -> Vec<Rc<RuleRec>> {
        self.root.find(r).map(|n| n.values.clone()).unwrap_or_default()
    }
}

fn add_module(root: &mut TreeNode, m: &Module) {
    for rule in &m.rules {
        let path = rule_ref(&m.package.path, rule);
        let mut elses = Vec::new();
        let mut e = rule.else_.as_deref();
        while let Some(x) = e {
            let mut r = x.clone();
            r.else_ = None;
            elses.push(Rc::new(RuleRec { rule: r, path: path.clone(), elses: Vec::new() }));
            e = x.else_.as_deref();
        }
        let mut r = rule.clone();
        r.else_ = None;
        let rec = Rc::new(RuleRec { rule: r, path: path.clone(), elses });
        let mut node = &mut *root;
        for t in ground_prefix(&path) {
            let pos = node.children.iter().position(|(k, _)| k.equal(&t));
            let i = match pos {
                Some(i) => i,
                None => {
                    node.children.push((t.clone(), TreeNode::default()));
                    node.children.len() - 1
                }
            };
            node = &mut node.children[i].1;
        }
        node.values.push(rec);
    }
}

fn sort_tree(n: &mut TreeNode) {
    n.children.sort_by(|a, b| term_compare(&a.0, &b.0));
    for (_, c) in n.children.iter_mut() {
        sort_tree(c);
    }
}

/// Bindings of one query: variables to terms and the bindings those terms are in.
#[derive(Debug, Default)]
struct Bindings {
    values: HashMap<Rc<str>, (Term, usize)>,
}

/// A query being evaluated: OPA's `eval`, its mutable parts in the Machine.
#[derive(Debug, Clone)]
struct Frame {
    query: Rc<Body>,
    index: usize,
    b: usize,
    qid: u64,
    find_one: bool,
    input: Option<Rc<Term>>,
    data: Option<Rc<Term>>,
}

/// The rule index's answer (ast.IndexResult).
#[derive(Debug, Default)]
struct IndexResult {
    rules: Vec<Rc<RuleRec>>,
    elses: HashMap<usize, Vec<Rc<RuleRec>>>,
    default: Option<Rc<RuleRec>>,
    kind_multi: bool,
    early_exit: bool,
    only_ground_refs: bool,
}

impl IndexResult {
    fn empty(&self) -> bool {
        self.rules.is_empty() && self.default.is_none()
    }
}

/// What every query of one evaluation shares.
pub struct Machine<'p> {
    p: &'p Program,
    bindings: Vec<Bindings>,
    next_qid: u64,
    /// The virtual-document cache, a scope per `with`: Some(None) caches "undefined".
    vcache: Vec<HashMap<String, Option<Term>>>,
    /// Function and builtin replacements, a scope per `with`.
    mocks: Vec<Vec<(String, Term)>>,
    /// The documents `with` replaced, a scope per `with` (targetStack).
    targets: Vec<Vec<Vec<Term>>>,
    pub prints: Vec<String>,
    pub ctx: funcs::Context,
    host: &'p mut dyn Host,
    genvar: u64,
}

impl std::fmt::Debug for Machine<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Machine")
    }
}

type K<'a> = &'a mut dyn FnMut(&mut Machine<'_>) -> R;
type I<'a> = &'a mut dyn FnMut(&mut Machine<'_>, &Frame) -> R;

/// Converts a ground term to a value.
pub fn to_value(t: &Term) -> Option<Value> {
    Some(match &t.value {
        TermValue::Null => Value::Null,
        TermValue::Bool(b) => Value::Bool(*b),
        TermValue::Number(n) => Value::Number(n.clone()),
        TermValue::String(s) => Value::String(s.clone()),
        TermValue::Array(a) => Value::array(a.iter().map(to_value).collect::<Option<Vec<_>>>()?),
        TermValue::Set(s) => Value::set(s.iter().map(to_value).collect::<Option<_>>()?),
        TermValue::Object(o) => Value::object(o.iter().map(|(k, v)| Some((to_value(k)?, to_value(v)?))).collect::<Option<_>>()?),
        _ => return None,
    })
}

/// A value as a term.
pub fn to_term(v: &Value) -> Term {
    let value = match v {
        Value::Null => TermValue::Null,
        Value::Bool(b) => TermValue::Bool(*b),
        Value::Number(n) => TermValue::Number(n.clone()),
        Value::String(s) => TermValue::String(s.clone()),
        Value::Array(a) => TermValue::Array(a.iter().map(to_term).collect()),
        Value::Set(s) => TermValue::Set(s.iter().map(to_term).collect()),
        Value::Object(o) => TermValue::Object(o.iter().map(|(k, v)| (to_term(k), to_term(v))).collect()),
    };
    Term::new(value, None)
}

fn var_term(name: &str) -> Term {
    Term::var(name, None)
}

fn is_var(t: &Term) -> bool {
    matches!(t.value, TermValue::Var(_))
}

fn ref_of(r: &[Term]) -> Term {
    Term::reference(r.to_vec(), None)
}

fn is_ground(t: &Term) -> bool {
    t.is_ground()
}

/// The text a virtual-cache key is kept under.
fn key_text(r: &[Term]) -> String {
    ref_of(r).to_string()
}

/// Builds a set of terms, repeats kept once.
fn set_of(items: Vec<Term>) -> Term {
    crate::ast::set_term(items, None)
}

impl<'p> Machine<'p> {
    pub fn new(p: &'p Program, host: &'p mut dyn Host, ctx: funcs::Context) -> Machine<'p> {
        Machine {
            p,
            bindings: Vec::new(),
            next_qid: 0,
            vcache: vec![HashMap::new()],
            mocks: vec![Vec::new()],
            targets: Vec::new(),
            prints: Vec::new(),
            ctx,
            host,
            genvar: 0,
        }
    }

    fn new_bindings(&mut self) -> usize {
        self.bindings.push(Bindings::default());
        self.bindings.len() - 1
    }

    fn qid(&mut self) -> u64 {
        self.next_qid += 1;
        self.next_qid
    }

    /// bindings.apply: a variable's value, followed through the bindings it is in.
    fn apply(&self, t: &Term, b: usize) -> (Term, usize) {
        let mut t = t.clone();
        let mut b = b;
        loop {
            let TermValue::Var(v) = &t.value else { return (t, b) };
            match self.bindings.get(b).and_then(|x| x.values.get(v)) {
                Some((nt, nb)) => {
                    let (nt, nb) = (nt.clone(), *nb);
                    t = nt;
                    b = nb;
                }
                None => return (t, b),
            }
        }
    }

    /// bindings.Plug: every bound variable replaced by its value.
    fn plug(&self, t: &Term, b: usize) -> Term {
        match &t.value {
            TermValue::Var(_) => {
                let (nt, nb) = self.apply(t, b);
                if is_var(&nt) {
                    return nt;
                }
                self.plug(&nt, nb)
            }
            TermValue::Array(a) => {
                if is_ground(t) {
                    return t.clone();
                }
                Term::new(TermValue::Array(a.iter().map(|x| self.plug(x, b)).collect()), t.loc.clone())
            }
            TermValue::Object(o) => {
                if is_ground(t) {
                    return t.clone();
                }
                let pairs = o.iter().map(|(k, v)| (self.plug(k, b), self.plug(v, b))).collect();
                crate::ast::object_term(pairs, t.loc.clone())
            }
            TermValue::Set(s) => {
                if is_ground(t) {
                    return t.clone();
                }
                crate::ast::set_term(s.iter().map(|x| self.plug(x, b)).collect(), t.loc.clone())
            }
            TermValue::Ref(r) => Term::new(TermValue::Ref(r.iter().map(|x| self.plug(x, b)).collect()), t.loc.clone()),
            _ => t.clone(),
        }
    }

    fn bind(&mut self, a: &Term, value: &Term, vb: usize, b: usize) -> Option<Rc<str>> {
        let TermValue::Var(v) = &a.value else { return None };
        let slot = self.bindings.get_mut(b)?;
        slot.values.insert(v.clone(), (value.clone(), vb));
        Some(v.clone())
    }

    fn unbind(&mut self, v: Option<Rc<str>>, b: usize) {
        if let (Some(v), Some(slot)) = (v, self.bindings.get_mut(b)) {
            slot.values.remove(&v);
        }
    }

    fn vcache_get(&self, key: &str) -> Option<&Option<Term>> {
        self.vcache.last().and_then(|c| c.get(key))
    }

    fn vcache_put(&mut self, key: String, v: Option<Term>) {
        if let Some(c) = self.vcache.last_mut() {
            c.insert(key, v);
        }
    }

    fn genvar(&mut self, suffix: &str) -> Term {
        self.genvar += 1;
        var_term(&format!("__{suffix}{}__", self.genvar))
    }
}

/// Evaluates `query`'s value: each result of `x = query`, as rego.Eval captures it.
pub fn eval_query(m: &mut Machine<'_>, query: &Term, input: Option<Value>) -> Result<Vec<Value>, EvalError> {
    let capture = var_term("__localq0__");
    let loc = query.loc.clone();
    let expr = Expr::new(ExprTerms::Call(vec![crate::compile::localvars::op("eq"), query.clone(), capture.clone()]), loc);
    let b = m.new_bindings();
    let qid = m.qid();
    let f = Frame {
        query: Rc::new(vec![expr]),
        index: 0,
        b,
        qid,
        find_one: false,
        input: input.as_ref().map(|v| Rc::new(to_term(v))),
        data: None,
    };
    let mut out = Vec::new();
    let r = eval_expr(m, &f, &mut |m, f| {
        let v = m.plug(&capture, f.b);
        if let Some(v) = to_value(&v) {
            out.push(v);
        }
        Ok(())
    });
    match suppress_all(r) {
        Ok(()) => Ok(out),
        Err(e) => Err(e),
    }
}

fn suppress_all(r: R) -> Result<(), EvalError> {
    let mut r = r;
    loop {
        match r {
            Ok(()) => return Ok(()),
            Err(Flow::Err(e)) => return Err(e),
            Err(Flow::Early { prev: None, .. }) => return Ok(()),
            Err(Flow::Early { prev: Some(p), .. }) => r = Err(*p),
        }
    }
}

fn eval_expr(m: &mut Machine<'_>, f: &Frame, iter: I<'_>) -> R {
    if f.index >= f.query.len() {
        if let Err(e) = iter(m, f) {
            return match e {
                Flow::Early { .. } => Err(Flow::Early { deferred: !f.find_one, prev: Some(Box::new(e)) }),
                e => Err(e),
            };
        }
        if f.find_one {
            return Err(Flow::Early { deferred: false, prev: None });
        }
        return Ok(());
    }
    let Some(expr) = f.query.get(f.index) else { return Ok(()) };
    if !expr.with.is_empty() {
        return eval_with(m, f, iter);
    }
    eval_step(m, f, &mut |m, f| next(m, f, iter))
}

fn next(m: &mut Machine<'_>, f: &Frame, iter: I<'_>) -> R {
    let mut g = f.clone();
    g.index += 1;
    eval_expr(m, &g, iter)
}

fn current(f: &Frame) -> Option<&Expr> {
    f.query.get(f.index)
}

fn eval_step(m: &mut Machine<'_>, f: &Frame, iter: I<'_>) -> R {
    let Some(expr) = current(f).cloned() else { return Ok(()) };
    if expr.negated {
        return eval_not(m, f, iter);
    }
    match &expr.terms {
        ExprTerms::Call(terms) => {
            if expr.is_equality() && terms.len() == 3 {
                let (Some(a), Some(b)) = (terms.get(1), terms.get(2)) else { return Ok(()) };
                unify(m, f, a, b, f.b, f.b, &mut |m| iter(m, f))
            } else {
                eval_call(m, f, terms, &mut |m| iter(m, f))
            }
        }
        ExprTerms::Term(t) => {
            let rterm = var_term(&format!("__term_{}_{}", f.qid, f.index));
            unify(m, f, t, &rterm, f.b, f.b, &mut |m| {
                let v = m.plug(&rterm, f.b);
                if matches!(v.value, TermValue::Bool(false)) {
                    return Ok(());
                }
                iter(m, f)
            })
        }
        ExprTerms::Every(ev) => eval_every(m, f, ev, iter),
        ExprTerms::Some(_) => Ok(()),
    }
}

fn closure(m: &mut Machine<'_>, f: &Frame, body: Body) -> Frame {
    Frame { query: Rc::new(body), index: 0, b: f.b, qid: m.qid(), find_one: false, input: f.input.clone(), data: f.data.clone() }
}

fn child(m: &mut Machine<'_>, f: &Frame, body: Body) -> Frame {
    let b = m.new_bindings();
    Frame { query: Rc::new(body), index: 0, b, qid: m.qid(), find_one: false, input: f.input.clone(), data: f.data.clone() }
}

fn eval_not(m: &mut Machine<'_>, f: &Frame, iter: I<'_>) -> R {
    let Some(expr) = current(f) else { return Ok(()) };
    let mut neg = expr.clone();
    neg.negated = false;
    neg.with.clear();
    neg.index = 0;
    let c = closure(m, f, vec![neg]);
    let mut defined = false;
    eval_expr(m, &c, &mut |_, _| {
        defined = true;
        Ok(())
    })?;
    if !defined {
        return iter(m, f);
    }
    Ok(())
}

fn eval_every(m: &mut Machine<'_>, f: &Frame, ev: &crate::ast::Every, iter: I<'_>) -> R {
    let pd = m.plug(&ev.domain, f.b);
    if !matches!(pd.value, TermValue::Array(_) | TermValue::Object(_) | TermValue::Set(_) | TermValue::Var(_) | TermValue::Ref(_)) {
        return Ok(());
    }
    let key = ev.key.clone().unwrap_or_else(|| var_term("$_"));
    let loc = ev.domain.loc.clone();
    let mut r = match &ev.domain.value {
        TermValue::Ref(r) => r.clone(),
        _ => vec![ev.domain.clone()],
    };
    r.push(key);
    let generator = Expr::new(
        ExprTerms::Call(vec![crate::compile::localvars::op("eq"), Term::reference(r, loc.clone()), ev.value.clone()]),
        loc,
    );
    let domain = closure(m, f, vec![generator]);
    let mut all = true;
    let body = ev.body.clone();
    eval_expr(m, &domain, &mut |m, child_f| {
        if !all {
            return Ok(());
        }
        let mut bf = closure(m, child_f, body.clone());
        bf.find_one = true;
        let mut done = false;
        let r = eval_expr(m, &bf, &mut |_, _| {
            done = true;
            Ok(())
        });
        if !done {
            all = false;
        }
        suppress(r)
    })?;
    if all {
        return iter(m, f);
    }
    Ok(())
}

fn eval_with(m: &mut Machine<'_>, f: &Frame, iter: I<'_>) -> R {
    let Some(expr) = current(f).cloned() else { return Ok(()) };
    let mut input_pairs: Vec<(Vec<Term>, Term)> = Vec::new();
    let mut data_pairs: Vec<(Vec<Term>, Term)> = Vec::new();
    let mut mocks: Vec<(String, Term)> = Vec::new();
    let mut targets: Vec<Vec<Term>> = Vec::new();
    for w in &expr.with {
        let plugged = m.plug(&w.value, f.b);
        let target = w.target.as_ref().map(<[Term]>::to_vec).unwrap_or_default();
        let name = crate::compile::text_of_ref(&target);
        let is_fn = !m.p.rules_at(&target).is_empty() && m.p.rules_at(&target).iter().any(|r| !r.rule.head.args.is_empty());
        if is_fn {
            mocks.push((name, plugged));
        } else if target.first().and_then(Term::as_var) == Some("input") {
            input_pairs.push((target.clone(), plugged));
        } else if target.first().and_then(Term::as_var) == Some("data") {
            data_pairs.push((target.clone(), plugged));
        } else if crate::compile::allowed(&name).is_some() || m.p.host_names.contains(&name) {
            mocks.push((name, plugged));
            continue;
        }
        targets.push(target);
    }
    let mut g = f.clone();
    if !input_pairs.is_empty() {
        g.input = Some(Rc::new(merge_with(f.input.as_deref(), &input_pairs).ok_or_else(|| {
            err(CONFLICT_ERR, expr.loc.clone(), "conflicting values for input")
        })?));
    }
    if !data_pairs.is_empty() {
        g.data = Some(Rc::new(merge_with(f.data.as_deref(), &data_pairs).ok_or_else(|| {
            err(CONFLICT_ERR, expr.loc.clone(), "conflicting values for data")
        })?));
    }
    let push = |m: &mut Machine<'_>| {
        m.vcache.push(HashMap::new());
        m.mocks.push(mocks.clone());
        m.targets.push(targets.clone());
    };
    let pop = |m: &mut Machine<'_>| {
        m.vcache.pop();
        m.mocks.pop();
        m.targets.pop();
    };
    push(m);
    let r = eval_step(m, &g, &mut |m, _| {
        pop(m);
        let r = next(m, f, iter);
        push(m);
        r
    });
    pop(m);
    r
}

/// mergeTermWithValues: the document with each target replaced.
fn merge_with(base: Option<&Term>, pairs: &[(Vec<Term>, Term)]) -> Option<Term> {
    let mut doc = base.cloned();
    for (target, value) in pairs {
        let path = target.get(1..).unwrap_or_default();
        if path.is_empty() {
            doc = Some(value.clone());
            continue;
        }
        let root = doc.take().unwrap_or_else(|| crate::ast::object_term(Vec::new(), None));
        doc = Some(set_path(root, path, value.clone())?);
    }
    doc
}

fn set_path(doc: Term, path: &[Term], value: Term) -> Option<Term> {
    let Some((first, rest)) = path.split_first() else { return Some(value) };
    let TermValue::Object(mut o) = doc.value else {
        return set_path(crate::ast::object_term(Vec::new(), None), path, value);
    };
    let existing = o.iter().position(|(k, _)| k.equal(first));
    let inner = match existing {
        Some(i) => o.remove(i).1,
        None => crate::ast::object_term(Vec::new(), None),
    };
    let new = if rest.is_empty() { value } else { set_path(inner, rest, value)? };
    o.push((first.clone(), new));
    Some(crate::ast::object_term(o, doc.loc))
}

/// biunify.
fn unify(m: &mut Machine<'_>, f: &Frame, a: &Term, b: &Term, b1: usize, b2: usize, k: K<'_>) -> R {
    let (a, b1) = m.apply(a, b1);
    let (b, b2) = m.apply(b, b2);
    use TermValue as V;
    match (&a.value, &b.value) {
        (V::Var(_) | V::Ref(_) | V::ArrayCompr(..) | V::SetCompr(..) | V::ObjectCompr(..), _) => unify_values(m, f, &a, &b, b1, b2, k),
        (V::Null, V::Var(_) | V::Null | V::Ref(_))
        | (V::Bool(_), V::Var(_) | V::Bool(_) | V::Ref(_))
        | (V::Number(_), V::Var(_) | V::Number(_) | V::Ref(_))
        | (V::String(_), V::Var(_) | V::String(_) | V::Ref(_)) => unify_values(m, f, &a, &b, b1, b2, k),
        (V::Array(_), V::Var(_) | V::Ref(_) | V::ArrayCompr(..)) => unify_values(m, f, &a, &b, b1, b2, k),
        (V::Array(x), V::Array(y)) => {
            if x.len() == y.len() {
                unify_slices(m, f, x, y, b1, b2, 0, k)
            } else {
                Ok(())
            }
        }
        (V::Object(_), V::Var(_) | V::Ref(_) | V::ObjectCompr(..)) => unify_values(m, f, &a, &b, b1, b2, k),
        (V::Object(x), V::Object(y)) => {
            if x.len() != y.len() {
                return Ok(());
            }
            let x: Vec<(Term, Term)> = x.iter().map(|(kk, v)| (if is_ground(kk) { kk.clone() } else { m.plug(kk, b1) }, v.clone())).collect();
            let y: Vec<(Term, Term)> = y.iter().map(|(kk, v)| (if is_ground(kk) { kk.clone() } else { m.plug(kk, b2) }, v.clone())).collect();
            let mut keys = x.clone();
            keys.sort_by(|p, q| term_compare(&p.0, &q.0));
            unify_objects(m, f, &keys, &y, b1, b2, 0, k)
        }
        (V::Set(_), _) => unify_values(m, f, &a, &b, b1, b2, k),
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn unify_slices(m: &mut Machine<'_>, f: &Frame, a: &[Term], b: &[Term], b1: usize, b2: usize, i: usize, k: K<'_>) -> R {
    let (Some(x), Some(y)) = (a.get(i), b.get(i)) else { return k(m) };
    unify(m, f, x, y, b1, b2, &mut |m| unify_slices(m, f, a, b, b1, b2, i + 1, k))
}

#[allow(clippy::too_many_arguments)]
fn unify_objects(m: &mut Machine<'_>, f: &Frame, a: &[(Term, Term)], b: &[(Term, Term)], b1: usize, b2: usize, i: usize, k: K<'_>) -> R {
    let Some((key, av)) = a.get(i) else { return k(m) };
    let Some((_, bv)) = b.iter().find(|(bk, _)| bk.equal(key)) else { return Ok(()) };
    unify(m, f, av, bv, b1, b2, &mut |m| unify_objects(m, f, a, b, b1, b2, i + 1, k))
}

fn is_compr(t: &Term) -> bool {
    matches!(t.value, TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..))
}

/// biunifyValues.
fn unify_values(m: &mut Machine<'_>, f: &Frame, a: &Term, b: &Term, b1: usize, b2: usize, k: K<'_>) -> R {
    if matches!(a.value, TermValue::Ref(_)) {
        return unify_ref(m, f, a, b, b1, b2, k);
    }
    if matches!(b.value, TermValue::Ref(_)) {
        return unify_ref(m, f, b, a, b2, b1, k);
    }
    if is_compr(a) {
        return unify_comprehension(m, f, a, b, b1, b2, k);
    } else if is_compr(b) {
        return unify_comprehension(m, f, b, a, b2, b1, k);
    }
    match (is_var(a), is_var(b)) {
        (true, true) => {
            if b1 == b2 && a.equal(b) {
                return k(m);
            }
            let u = m.bind(a, b, b2, b1);
            let r = k(m);
            m.unbind(u, b1);
            r
        }
        (true, false) => {
            let u = m.bind(a, b, b2, b1);
            let r = k(m);
            m.unbind(u, b1);
            r
        }
        (false, true) => {
            let u = m.bind(b, a, b1, b2);
            let r = k(m);
            m.unbind(u, b2);
            r
        }
        (false, false) => {
            let (pa, pb) = if matches!(a.value, TermValue::Set(_)) { (m.plug(a, b1), m.plug(b, b2)) } else { (a.clone(), b.clone()) };
            if pa.equal(&pb) { k(m) } else { Ok(()) }
        }
    }
}

/// biunifyRef.
fn unify_ref(m: &mut Machine<'_>, f: &Frame, a: &Term, b: &Term, b1: usize, b2: usize, k: K<'_>) -> R {
    let Some(r) = a.as_ref() else { return Ok(()) };
    let head = r.first().and_then(Term::as_var);
    if head == Some("data") {
        let plugged: Vec<Term> = r.to_vec();
        let node = r.first().and_then(|h| m.p.root.child(h)).map(|n| n as *const TreeNode);
        return eval_tree(m, f, r, 1, plugged, b1, b, b2, node, k);
    }
    let (term, tb) = if head == Some("input") {
        match &f.input {
            Some(i) => ((**i).clone(), b1),
            None => return Ok(()),
        }
    } else {
        let Some(h) = r.first() else { return Ok(()) };
        let (t, tb) = m.apply(h, b1);
        if is_var(&t) && t.equal(h) {
            return Ok(());
        }
        (t, tb)
    };
    eval_term(m, f, r, 1, b1, &term, tb, b, b2, k)
}

/// evalTerm: the rest of a ref, into a value.
#[allow(clippy::too_many_arguments)]
fn eval_term(m: &mut Machine<'_>, f: &Frame, r: &[Term], pos: usize, b: usize, term: &Term, tb: usize, rterm: &Term, rb: usize, k: K<'_>) -> R {
    if pos == r.len() {
        return unify(m, f, term, rterm, tb, rb, k);
    }
    let Some(part) = r.get(pos) else { return Ok(()) };
    let plugged = m.plug(part, b);
    if is_ground(&plugged) {
        let Some((t, nb)) = term_get(m, term, tb, &plugged) else { return Ok(()) };
        return eval_term(m, f, r, pos + 1, b, &t, nb, rterm, rb, k);
    }
    let mut deferred: Option<Flow> = None;
    let handle = |r: R, deferred: &mut Option<Flow>| -> R {
        if is_deferred(&r) {
            if deferred.is_none() {
                *deferred = r.err();
            }
            return Ok(());
        }
        r
    };
    match &term.value {
        TermValue::Array(a) => {
            for i in 0..a.len() {
                let idx = Term::new(TermValue::Number(Number::from_i64(i as i64)), None);
                let (bv, bb) = m.apply(part, b);
                if is_var(&bv) {
                    let u = m.bind(&bv, &idx, bb, bb);
                    let res = term_get(m, term, tb, &idx);
                    let r1 = match res {
                        Some((t, nb)) => eval_term(m, f, r, pos + 1, b, &t, nb, rterm, rb, k),
                        None => Ok(()),
                    };
                    m.unbind(u, bb);
                    handle(r1, &mut deferred)?;
                }
            }
        }
        TermValue::Object(o) => {
            let mut keys: Vec<Term> = o.iter().map(|(kk, _)| kk.clone()).collect();
            keys.sort_by(term_compare);
            for key in keys {
                let r1 = unify(m, f, &key, part, tb, b, &mut |m| {
                    let pk = m.plug(&key, tb);
                    match term_get(m, term, tb, &pk) {
                        Some((t, nb)) => eval_term(m, f, r, pos + 1, b, &t, nb, rterm, rb, k),
                        None => Ok(()),
                    }
                });
                handle(r1, &mut deferred)?;
            }
        }
        TermValue::Set(s) => {
            let mut items = s.clone();
            items.sort_by(term_compare);
            for elem in items {
                let r1 = unify(m, f, &elem, part, tb, b, &mut |m| {
                    let pe = m.plug(&elem, tb);
                    match term_get(m, term, tb, &pe) {
                        Some((t, nb)) => eval_term(m, f, r, pos + 1, b, &t, nb, rterm, rb, k),
                        None => Ok(()),
                    }
                });
                handle(r1, &mut deferred)?;
            }
        }
        _ => {}
    }
    match deferred {
        Some(d) => Err(d),
        None => Ok(()),
    }
}

/// evalTerm.get: a member of a collection, by a ground key.
fn term_get(m: &Machine<'_>, term: &Term, tb: usize, key: &Term) -> Option<(Term, usize)> {
    match &term.value {
        TermValue::Set(s) => {
            for e in s {
                if m.plug(e, tb).equal(key) {
                    return Some(m.apply(key, tb));
                }
            }
            None
        }
        TermValue::Object(o) => {
            for (kk, v) in o {
                if m.plug(kk, tb).equal(key) {
                    return Some(m.apply(v, tb));
                }
            }
            None
        }
        TermValue::Array(a) => {
            let TermValue::Number(n) = &key.value else { return None };
            let i = n.as_i64().and_then(|i| usize::try_from(i).ok())?;
            a.get(i).map(|t| m.apply(t, tb))
        }
        _ => None,
    }
}

/// evalTree: a ref into data: the rule tree, then the base document.
#[allow(clippy::too_many_arguments)]
fn eval_tree(m: &mut Machine<'_>, f: &Frame, r: &[Term], pos: usize, plugged: Vec<Term>, b: usize, rterm: &Term, rb: usize, node: Option<*const TreeNode>, k: K<'_>) -> R {
    if pos == r.len() {
        let v = tree_extent(m, f, &plugged, node)?;
        let Some(v) = v else { return Ok(()) };
        let tmp = m.new_bindings();
        return unify(m, f, rterm, &v, rb, tmp, k);
    }
    let Some(part) = r.get(pos) else { return Ok(()) };
    let p = m.plug(part, b);
    if is_ground(&p) {
        return tree_next(m, f, r, pos, plugged, b, rterm, rb, node, p, k);
    }
    // enumerate: keys of the base document, then the rule tree's children.
    let base = resolve_base(f, plugged.get(..pos).unwrap_or_default());
    let mut deferred: Option<Flow> = None;
    if let Some(doc) = base {
        let keys: Vec<Term> = match &doc.value {
            TermValue::Array(a) => (0..a.len()).map(|i| Term::new(TermValue::Number(Number::from_i64(i as i64)), None)).collect(),
            TermValue::Object(o) => {
                let mut ks: Vec<Term> = o.iter().map(|(kk, _)| kk.clone()).collect();
                ks.sort_by(term_compare);
                ks
            }
            TermValue::Set(s) => {
                let mut ks = s.clone();
                ks.sort_by(term_compare);
                ks
            }
            _ => Vec::new(),
        };
        for key in keys {
            let pl = plugged.clone();
            let r1 = unify(m, f, &key, part, b, b, &mut |m| tree_next(m, f, r, pos, pl.clone(), b, rterm, rb, node, key.clone(), k));
            if is_deferred(&r1) {
                if deferred.is_none() {
                    deferred = r1.err();
                }
            } else {
                r1?;
            }
        }
    }
    if let Some(d) = deferred {
        return Err(d);
    }
    let Some(n) = node else { return Ok(()) };
    // SAFETY-free: nodes live in the Program, which outlives the Machine.
    let keys: Vec<Term> = node_ref(m, n).children.iter().map(|(kk, _)| kk.clone()).collect();
    for key in keys {
        let pl = plugged.clone();
        unify(m, f, &key, part, b, b, &mut |m| tree_next(m, f, r, pos, pl.clone(), b, rterm, rb, node, key.clone(), k))?;
    }
    Ok(())
}

/// The tree node a pointer names: the Program's nodes outlive every evaluation of it.
fn node_ref<'a>(m: &'a Machine<'_>, n: *const TreeNode) -> &'a TreeNode {
    find_node(&m.p.root, n).unwrap_or(&m.p.root)
}

fn find_node(root: &TreeNode, n: *const TreeNode) -> Option<&TreeNode> {
    if std::ptr::eq(root, n) {
        return Some(root);
    }
    root.children.iter().find_map(|(_, c)| find_node(c, n))
}

#[allow(clippy::too_many_arguments)]
fn tree_next(m: &mut Machine<'_>, f: &Frame, r: &[Term], pos: usize, mut plugged: Vec<Term>, b: usize, rterm: &Term, rb: usize, node: Option<*const TreeNode>, p: Term, k: K<'_>) -> R {
    if let Some(slot) = plugged.get_mut(pos) {
        *slot = p.clone();
    }
    let mut child_node = None;
    let path = plugged.get(..=pos).unwrap_or_default();
    let replaced = m.targets.iter().flatten().any(|t| t.len() <= path.len() && t.iter().zip(path).all(|(a, b)| a.equal(b)));
    if replaced {
        return eval_tree(m, f, r, pos + 1, plugged, b, rterm, rb, None, k);
    }
    if let Some(n) = node {
        if let Some(c) = node_ref(m, n).child(&p) {
            if !c.values.is_empty() {
                return eval_virtual(m, f, r, plugged, pos, b, rterm, rb, k);
            }
            child_node = Some(c as *const TreeNode);
        }
    }
    eval_tree(m, f, r, pos + 1, plugged, b, rterm, rb, child_node, k)
}

/// The base document at a path (only what `with data...` put there).
fn resolve_base(f: &Frame, path: &[Term]) -> Option<Term> {
    let mut doc = (*f.data.clone()?).clone();
    for t in path.iter().skip(1) {
        let TermValue::Object(o) = &doc.value else { return None };
        doc = o.iter().find(|(kk, _)| kk.equal(t))?.1.clone();
    }
    Some(doc)
}

/// evalTree.extent: the whole document at a path, rules' values included.
fn tree_extent(m: &mut Machine<'_>, f: &Frame, plugged: &[Term], node: Option<*const TreeNode>) -> Result<Option<Term>, Flow> {
    let base = resolve_base(f, plugged);
    let virtual_doc = match node {
        Some(n) => leaves(m, f, plugged.to_vec(), n)?,
        None => None,
    };
    Ok(match (base, virtual_doc) {
        (b, None) => b,
        (None, Some(v)) => Some(v),
        (Some(b), Some(v)) => Some(merge_docs(&b, &v).ok_or_else(|| err(WITH_MERGE_ERR, plugged.first().and_then(|t| t.loc.clone()), "real and replacement data could not be merged"))?),
    })
}

fn merge_docs(a: &Term, b: &Term) -> Option<Term> {
    match (&a.value, &b.value) {
        (TermValue::Object(x), TermValue::Object(y)) => {
            let mut out = x.clone();
            for (k, v) in y {
                match out.iter().position(|(ok, _)| ok.equal(k)) {
                    Some(i) => {
                        let merged = merge_docs(&out[i].1, v)?;
                        out[i].1 = merged;
                    }
                    None => out.push((k.clone(), v.clone())),
                }
            }
            Some(crate::ast::object_term(out, None))
        }
        _ => None,
    }
}

fn leaves(m: &mut Machine<'_>, f: &Frame, plugged: Vec<Term>, n: *const TreeNode) -> Result<Option<Term>, Flow> {
    let children: Vec<(Term, bool, *const TreeNode)> =
        node_ref(m, n).children.iter().map(|(k, c)| (k.clone(), !c.values.is_empty(), c as *const TreeNode)).collect();
    let mut result: Vec<(Term, Term)> = Vec::new();
    for (key, has_values, c) in children {
        let mut path = plugged.clone();
        path.push(key.clone());
        let save = if has_values {
            let rterm = m.genvar("leaf");
            let mut out = None;
            let tmp = m.new_bindings();
            let rf = Frame { b: tmp, ..f.clone() };
            unify(m, &rf, &ref_of(&path), &rterm, tmp, tmp, &mut |m| {
                out = Some(m.plug(&rterm, tmp));
                Ok(())
            })?;
            out
        } else {
            leaves(m, f, path, c)?
        };
        if let Some(v) = save {
            result.push((key, v));
        }
    }
    Ok(Some(crate::ast::object_term(result, None)))
}

/// getRules: the rules at a path, as OPA's index answers when it indexes nothing.
fn get_rules(m: &Machine<'_>, path: &[Term]) -> IndexResult {
    let roots = m.p.rules_at(path);
    let mut ir = IndexResult { only_ground_refs: true, ..IndexResult::default() };
    let Some(first) = roots.first() else { return ir };
    ir.kind_multi = first.rule.head.kind() == RuleKind::MultiValue;
    let mut multiple = false;
    let mut last_value: Option<Term> = None;
    for r in &roots {
        let chain: Vec<&Rc<RuleRec>> = std::iter::once(r).chain(r.elses.iter()).collect();
        for x in &chain {
            if !x.rule.head.reference.iter().skip(1).all(Term::is_ground) {
                ir.only_ground_refs = false;
            }
        }
        if r.rule.default {
            ir.default = Some(r.clone());
            continue;
        }
        ir.elses.insert(ir.rules.len(), r.elses.clone());
        ir.rules.push(r.clone());
    }
    for r in &ir.rules {
        let complete = r.rule.head.kind() == RuleKind::SingleValue && r.rule.head.args.is_empty() && r.rule.head.reference.iter().skip(1).all(Term::is_ground);
        if !complete && r.rule.head.key.is_some() {
            multiple = true;
            break;
        }
        if let Some(v) = &r.rule.head.value {
            if last_value.as_ref().is_some_and(|l| !l.equal(v)) {
                multiple = true;
                break;
            }
            last_value = Some(v.clone());
        }
    }
    ir.early_exit = !multiple;
    ir
}

/// evalVirtual.
#[allow(clippy::too_many_arguments)]
fn eval_virtual(m: &mut Machine<'_>, f: &Frame, r: &[Term], plugged: Vec<Term>, pos: usize, b: usize, rterm: &Term, rb: usize, k: K<'_>) -> R {
    let ir = get_rules(m, plugged.get(..=pos).unwrap_or_default());
    if ir.kind_multi {
        let empty = if ir.only_ground_refs { set_of(Vec::new()) } else { crate::ast::object_term(Vec::new(), None) };
        return eval_partial(m, f, r, plugged, pos, b, rterm, rb, &ir, empty, k);
    }
    if ir.only_ground_refs {
        return eval_complete(m, f, r, plugged, pos, b, rterm, rb, &ir, k);
    }
    eval_partial(m, f, r, plugged, pos, b, rterm, rb, &ir, crate::ast::object_term(Vec::new(), None), k)
}

/// evalVirtualComplete.
#[allow(clippy::too_many_arguments)]
fn eval_complete(m: &mut Machine<'_>, f: &Frame, r: &[Term], plugged: Vec<Term>, pos: usize, b: usize, rterm: &Term, rb: usize, ir: &IndexResult, k: K<'_>) -> R {
    if ir.empty() {
        return Ok(());
    }
    if ir.rules.first().is_some_and(|x| !x.rule.head.args.is_empty()) || ir.default.as_ref().is_some_and(|d| !d.rule.head.args.is_empty()) {
        return Ok(());
    }
    let key = key_text(plugged.get(..=pos).unwrap_or_default());
    match m.vcache_get(&key).cloned() {
        Some(None) => return Ok(()),
        Some(Some(cached)) => {
            let tmp = m.new_bindings();
            return eval_term(m, f, r, pos + 1, b, &cached, tmp, rterm, rb, k);
        }
        None => {}
    }
    let find_one = ir.early_exit;
    let mut prev: Option<Term> = None;
    let mut deferred: Option<Flow> = None;
    let res = (|| -> R {
        for (i, rule) in ir.rules.iter().enumerate() {
            let (next_v, r1) = eval_value_rule(m, f, r, pos, b, rterm, rb, rule, &mut prev, find_one, &key, k);
            if is_deferred(&r1) {
                if deferred.is_none() {
                    deferred = r1.err();
                }
            } else {
                r1?;
            }
            let mut next_v = next_v;
            if next_v.is_none() {
                for erule in ir.elses.get(&i).map(Vec::as_slice).unwrap_or_default() {
                    let (nv, r1) = eval_value_rule(m, f, r, pos, b, rterm, rb, erule, &mut prev, find_one, &key, k);
                    if is_deferred(&r1) {
                        if deferred.is_none() {
                            deferred = r1.err();
                        }
                    } else {
                        r1?;
                    }
                    if nv.is_some() {
                        next_v = nv;
                        break;
                    }
                }
            }
            if next_v.is_some() {
                prev = next_v;
            }
        }
        if let Some(d) = &ir.default
            && prev.is_none()
        {
            let (_, r1) = eval_value_rule(m, f, r, pos, b, rterm, rb, d, &mut prev, find_one, &key, k);
            return r1;
        }
        if prev.is_none() {
            m.vcache_put(key.clone(), None);
        }
        match deferred.take() {
            Some(d) => Err(d),
            None => Ok(()),
        }
    })();
    suppress(res)
}

#[allow(clippy::too_many_arguments)]
fn eval_value_rule(
    m: &mut Machine<'_>,
    f: &Frame,
    r: &[Term],
    pos: usize,
    b: usize,
    rterm: &Term,
    rb: usize,
    rule: &Rc<RuleRec>,
    prev: &mut Option<Term>,
    find_one: bool,
    key: &str,
    k: K<'_>,
) -> (Option<Term>, R) {
    let mut cf = child(m, f, rule.rule.body.clone());
    cf.find_one = find_one;
    let mut result: Option<Term> = None;
    let value = rule.rule.head.value.clone().unwrap_or_else(|| Term::boolean(true, None));
    let loc = rule.rule.loc.clone();
    let res = eval_expr(m, &cf, &mut |m, cf| {
        let v = m.plug(&value, cf.b);
        result = Some(v.clone());
        if let Some(p) = prev.as_ref() {
            if term_compare(&v, p) != std::cmp::Ordering::Equal {
                return Err(err(CONFLICT_ERR, loc.clone(), "complete rules must not produce multiple outputs"));
            }
            return Ok(());
        }
        *prev = Some(v.clone());
        m.vcache_put(key.to_string(), Some(v.clone()));
        let (t, tb) = m.apply(&value, cf.b);
        eval_term(m, f, r, pos + 1, b, &t, tb, rterm, rb, k)
    });
    (result, res)
}

/// evalVirtualPartial (without partial evaluation).
#[allow(clippy::too_many_arguments)]
fn eval_partial(m: &mut Machine<'_>, f: &Frame, r: &[Term], plugged: Vec<Term>, pos: usize, b: usize, rterm: &Term, rb: usize, ir: &IndexResult, empty: Term, k: K<'_>) -> R {
    if r.len() == pos + 1 {
        let key = key_text(plugged.get(..=pos).unwrap_or_default());
        if let Some(Some(cached)) = m.vcache_get(&key).cloned() {
            let tmp = m.new_bindings();
            return unify(m, f, &cached, rterm, tmp, rb, k);
        }
        let result = all_rules(m, f, pos, ir, empty)?;
        m.vcache_put(key, Some(result.clone()));
        let tmp = m.new_bindings();
        return unify(m, f, &result, rterm, tmp, rb, k);
    }
    if ir.empty() {
        return Ok(());
    }
    // evalCache: the full extent when the next part is a variable.
    let full_key = key_text(plugged.get(..=pos).unwrap_or_default());
    if let Some(Some(cached)) = m.vcache_get(&full_key).cloned() {
        let tmp = m.new_bindings();
        return eval_term(m, f, r, pos + 1, b, &cached, tmp, rterm, rb, k);
    }
    let nextp = r.get(pos + 1).map(|t| m.plug(t, b));
    if nextp.as_ref().is_some_and(is_var) {
        let result = all_rules(m, f, pos, ir, empty)?;
        m.vcache_put(full_key, Some(result.clone()));
        let tmp = m.new_bindings();
        return eval_term(m, f, r, pos + 1, b, &result, tmp, rterm, rb, k);
    }
    let mut result = empty;
    let mut visited: Vec<Vec<Term>> = Vec::new();
    for rule in &ir.rules {
        result = one_rule_pre_unify(m, f, r, pos, b, rule, result, &mut visited)?;
    }
    let tmp = m.new_bindings();
    eval_term(m, f, r, pos + 1, b, &result, tmp, rterm, rb, k)
}

fn all_rules(m: &mut Machine<'_>, f: &Frame, pos: usize, ir: &IndexResult, empty: Term) -> Result<Term, Flow> {
    let mut result = empty;
    let mut visited: Vec<Vec<Term>> = Vec::new();
    for rule in &ir.rules {
        let cf = child(m, f, rule.rule.body.clone());
        let mut res = Some(result);
        eval_expr(m, &cf, &mut |m, cf| {
            let cur = res.take().unwrap_or_else(|| set_of(Vec::new()));
            let (next_v, _) = reduce(m, pos, rule, cf.b, cur, &mut visited)?;
            res = Some(next_v);
            Ok(())
        })?;
        result = res.unwrap_or_else(|| set_of(Vec::new()));
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn one_rule_pre_unify(m: &mut Machine<'_>, f: &Frame, r: &[Term], pos: usize, b: usize, rule: &Rc<RuleRec>, result: Term, visited: &mut Vec<Vec<Term>>) -> Result<Term, Flow> {
    let cf = child(m, f, rule.rule.body.clone());
    let mut res = Some(result);
    unify_rule_head(m, f, pos + 1, r, rule, b, cf.b, &mut |m| {
        eval_expr(m, &cf, &mut |m, cf| {
            let cur = res.take().unwrap_or_else(|| set_of(Vec::new()));
            let (next_v, _) = reduce(m, pos, rule, cf.b, cur, visited)?;
            res = Some(next_v);
            Ok(())
        })
    })?;
    Ok(res.unwrap_or_else(|| set_of(Vec::new())))
}

/// biunifyRuleHead.
#[allow(clippy::too_many_arguments)]
fn unify_rule_head(m: &mut Machine<'_>, f: &Frame, pos: usize, r: &[Term], rule: &Rc<RuleRec>, rb: usize, cb: usize, k: K<'_>) -> R {
    let path = rule.path.clone();
    unify_dynamic_ref(m, f, pos, r, &path, rb, cb, &mut |m, p| {
        if rule.rule.head.kind() == RuleKind::MultiValue && p < r.len() && path.len() <= r.len() {
            let head_key = rule.rule.head.key.clone().or_else(|| rule.rule.head.reference.last().cloned()).unwrap_or_else(|| var_term("_"));
            let Some(rp) = r.get(p) else { return k(m) };
            return unify(m, f, rp, &head_key, rb, cb, k);
        }
        k(m)
    })
}

#[allow(clippy::too_many_arguments)]
fn unify_dynamic_ref(m: &mut Machine<'_>, f: &Frame, pos: usize, a: &[Term], b: &[Term], b1: usize, b2: usize, k: &mut dyn FnMut(&mut Machine<'_>, usize) -> R) -> R {
    let (Some(x), Some(y)) = (a.get(pos), b.get(pos)) else { return k(m, pos) };
    unify(m, f, x, y, b1, b2, &mut |m| unify_dynamic_ref(m, f, pos + 1, a, b, b1, b2, k))
}

/// evalVirtualPartial.reduce: one rule's result added to the document.
fn reduce(m: &Machine<'_>, pos: usize, rule: &Rc<RuleRec>, b: usize, result: Term, visited: &mut Vec<Vec<Term>>) -> Result<(Term, bool), Flow> {
    let head = &rule.rule.head;
    let loc = head.loc.clone();
    match result.value {
        TermValue::Set(mut s) => {
            let key = m.plug(head.key.as_ref().unwrap_or(&Term::boolean(true, None)), b);
            let exists = s.iter().any(|x| x.equal(&key));
            if !exists {
                s.push(key);
            }
            Ok((Term::new(TermValue::Set(s), None), exists))
        }
        TermValue::Object(o) => {
            let full = &rule.path;
            let collision: Vec<Term> = full.get(pos + 1..).unwrap_or_default().iter().map(|t| m.plug(t, b)).collect();
            if visited.iter().any(|c| c.len() < collision.len() && collision.iter().zip(c).all(|(x, y)| x.equal(y))) {
                return Err(err(CONFLICT_ERR, loc, "object keys must be unique"));
            }
            visited.push(collision);
            let obj_path: Vec<Term> = full.get(pos + 1..full.len().saturating_sub(1)).unwrap_or_default().iter().map(|t| m.plug(t, b)).collect();
            let leaf_key = full.last().map(|t| m.plug(t, b)).unwrap_or_else(|| Term::boolean(true, None));
            let leaf = if head.kind() == RuleKind::SingleValue {
                Leaf::Value(m.plug(head.value.as_ref().unwrap_or(&Term::boolean(true, None)), b))
            } else {
                Leaf::SetMember(m.plug(head.key.as_ref().unwrap_or(&Term::boolean(true, None)), b))
            };
            let mut exists = false;
            let new = insert_nested(Term::new(TermValue::Object(o), None), &obj_path, &leaf_key, &leaf, &mut exists, &loc)?;
            Ok((new, exists))
        }
        _ => Ok((result, false)),
    }
}

enum Leaf {
    Value(Term),
    SetMember(Term),
}

fn insert_nested(obj: Term, path: &[Term], leaf_key: &Term, leaf: &Leaf, exists: &mut bool, loc: &Option<Location>) -> Result<Term, Flow> {
    let TermValue::Object(mut o) = obj.value else {
        return Err(err(CONFLICT_ERR, loc.clone(), "object keys must be unique"));
    };
    if let Some((first, rest)) = path.split_first() {
        let pos = o.iter().position(|(k, _)| k.equal(first));
        let inner = match pos {
            Some(i) => {
                let v = o.remove(i).1;
                if !matches!(v.value, TermValue::Object(_)) {
                    return Err(err(CONFLICT_ERR, loc.clone(), "object keys must be unique"));
                }
                v
            }
            None => crate::ast::object_term(Vec::new(), None),
        };
        let new = insert_nested(inner, rest, leaf_key, leaf, exists, loc)?;
        match pos {
            Some(i) => o.insert(i, (first.clone(), new)),
            None => o.push((first.clone(), new)),
        }
        return Ok(Term::new(TermValue::Object(o), None));
    }
    let pos = o.iter().position(|(k, _)| k.equal(leaf_key));
    match leaf {
        Leaf::Value(v) => match pos {
            Some(i) => {
                if !o[i].1.equal(v) {
                    return Err(err(CONFLICT_ERR, loc.clone(), "object keys must be unique"));
                }
                *exists = true;
            }
            None => o.push((leaf_key.clone(), v.clone())),
        },
        Leaf::SetMember(v) => match pos {
            Some(i) => {
                let TermValue::Set(s) = &mut o[i].1.value else {
                    return Err(err(CONFLICT_ERR, loc.clone(), "object keys must be unique"));
                };
                *exists = s.iter().any(|x| x.equal(v));
                if !*exists {
                    s.push(v.clone());
                }
            }
            None => o.push((leaf_key.clone(), set_of(vec![v.clone()]))),
        },
    }
    Ok(Term::new(TermValue::Object(o), None))
}

/// Comprehensions: their values, then unified.
fn unify_comprehension(m: &mut Machine<'_>, f: &Frame, a: &Term, b: &Term, b1: usize, b2: usize, k: K<'_>) -> R {
    let value = match &a.value {
        TermValue::ArrayCompr(head, body) => {
            let c = closure(m, f, body.clone());
            let c = Frame { b: b1, ..c };
            let mut out = Vec::new();
            eval_expr(m, &c, &mut |m, cf| {
                out.push(m.plug(head, cf.b));
                Ok(())
            })?;
            Term::new(TermValue::Array(out), None)
        }
        TermValue::SetCompr(head, body) => {
            let c = closure(m, f, body.clone());
            let c = Frame { b: b1, ..c };
            let mut out: Vec<Term> = Vec::new();
            eval_expr(m, &c, &mut |m, cf| {
                let v = m.plug(head, cf.b);
                if !out.iter().any(|x| x.equal(&v)) {
                    out.push(v);
                }
                Ok(())
            })?;
            Term::new(TermValue::Set(out), None)
        }
        TermValue::ObjectCompr(kk, vv, body) => {
            let c = closure(m, f, body.clone());
            let c = Frame { b: b1, ..c };
            let mut out: Vec<(Term, Term)> = Vec::new();
            let loc = kk.loc.clone();
            eval_expr(m, &c, &mut |m, cf| {
                let key = m.plug(kk, cf.b);
                let val = m.plug(vv, cf.b);
                match out.iter().find(|(x, _)| x.equal(&key)) {
                    Some((_, existing)) if !existing.equal(&val) => {
                        return Err(err(CONFLICT_ERR, loc.clone(), "object keys must be unique"));
                    }
                    Some(_) => {}
                    None => out.push((key, val)),
                }
                Ok(())
            })?;
            Term::new(TermValue::Object(out), None)
        }
        _ => return Ok(()),
    };
    let tmp = m.new_bindings();
    unify(m, f, &value, b, tmp, b2, k)
}

/// evalCall.
fn eval_call(m: &mut Machine<'_>, f: &Frame, terms: &[Term], k: K<'_>) -> R {
    let Some(op) = terms.first().and_then(Term::as_ref) else { return Ok(()) };
    let name = crate::compile::text_of_ref(op);
    let loc = current(f).and_then(|e| e.loc.clone());
    // A `with` replacement of the function or builtin.
    let mock = m.mocks.iter().rev().flat_map(|s| s.iter()).find(|(n, _)| *n == name).map(|(_, v)| v.clone());
    if let Some(mv) = mock {
        if let Some(r) = mv.as_ref().map(<[Term]>::to_vec) {
            let target = crate::compile::text_of_ref(&r);
            if target != name {
                let mut call = vec![ref_of(&r)];
                call.extend(terms.iter().skip(1).cloned());
                return eval_call(m, f, &call, k);
            }
        }
        let arity = arity_of(m, op);
        return match terms.len() {
            n if n == arity + 2 => {
                let Some(out) = terms.last() else { return Ok(()) };
                let tmp = m.new_bindings();
                unify(m, f, out, &mv, f.b, tmp, k)
            }
            _ => {
                if matches!(mv.value, TermValue::Bool(false)) {
                    Ok(())
                } else {
                    k(m)
                }
            }
        };
    }
    if op.first().and_then(Term::as_var) == Some("data") {
        let ir = get_rules(m, op);
        return eval_func(m, f, terms, &ir, k);
    }
    let operands: Vec<Term> = terms.iter().skip(1).map(|t| m.plug(t, f.b)).collect();
    let arity = arity_of(m, op);
    let has_output = operands.len() > arity;
    let ins = operands.get(..arity.min(operands.len())).unwrap_or_default();
    if name == "internal.print" {
        return eval_print(m, f, ins, loc, k);
    }
    let mut args = Vec::with_capacity(ins.len());
    for t in ins {
        match to_value(t) {
            Some(v) => args.push(v),
            None => return Ok(()),
        }
    }
    let result = if m.p.host_names.contains(&name) {
        m.host.call(&name, &args).map_err(|e| err(BUILTIN_ERR, loc.clone(), format!("{name}: {e}")))?
    } else {
        let Some(bf) = funcs::lookup(&name) else {
            return Err(err(INTERNAL_ERR, loc, format!("unsupported built-in: {name}")));
        };
        match bf(&mut m.ctx, &args) {
            Ok(v) => v,
            Err(e) => {
                let (code, msg) = match &e {
                    BuiltinError::Operand(msg) => (TYPE_ERR, format!("{name}: {msg}")),
                    BuiltinError::Other(msg) => (BUILTIN_ERR, format!("{name}: {msg}")),
                    BuiltinError::Halt(msg) => (BUILTIN_ERR, msg.clone()),
                };
                return Err(err(code, loc, msg));
            }
        }
    };
    let Some(v) = result else { return Ok(()) };
    let decl_void = matches!(crate::compile::allowed(&name).map(|b| &b.decl), Some(crate::types::Type::Function { result: None, .. }));
    if decl_void {
        return k(m);
    }
    if has_output {
        let Some(out) = terms.last() else { return Ok(()) };
        let tmp = m.new_bindings();
        return unify(m, f, out, &to_term(&v), f.b, tmp, k);
    }
    if matches!(v, Value::Bool(false)) {
        return Ok(());
    }
    k(m)
}

fn arity_of(m: &Machine<'_>, op: &[Term]) -> usize {
    let name = crate::compile::text_of_ref(op);
    if let Some(crate::types::Type::Function { args, .. }) = crate::compile::allowed(&name).map(|b| &b.decl) {
        return args.len();
    }
    if m.p.host_names.contains(&name) {
        return host_arity(&name);
    }
    m.p.rules_at(op).first().map_or(0, |r| r.rule.head.args.len())
}

/// The arities of buildx's functions (policy/funcs.go).
fn host_arity(name: &str) -> usize {
    match name {
        "load_json" => 1,
        "verify_http_pgp_signature" => 3,
        _ => 2,
    }
}

/// internal.print: each operand a set of what it printed, crossed.
fn eval_print(m: &mut Machine<'_>, _f: &Frame, ins: &[Term], loc: Option<Location>, k: K<'_>) -> R {
    let Some(TermValue::Array(ops)) = ins.first().map(|t| &t.value) else { return k(m) };
    let mut lines: Vec<Vec<String>> = vec![Vec::new()];
    for op in ops {
        let parts: Vec<String> = match &op.value {
            TermValue::String(s) => vec![s.to_string()],
            TermValue::Number(_) | TermValue::Bool(_) | TermValue::Null => vec![op.to_string()],
            TermValue::Set(s) if s.is_empty() => vec!["<undefined>".to_string()],
            TermValue::Set(s) => {
                let mut items = s.clone();
                items.sort_by(term_compare);
                items.iter().map(|x| match &x.value {
                    TermValue::String(s) => s.to_string(),
                    _ => x.to_string(),
                }).collect()
            }
            _ => return Err(err(INTERNAL_ERR, loc, format!("illegal argument type: {}", op.value_name()))),
        };
        let mut next = Vec::new();
        for l in &lines {
            for p in &parts {
                let mut l2 = l.clone();
                l2.push(p.clone());
                next.push(l2);
            }
        }
        lines = next;
    }
    let at = loc.unwrap_or_default();
    for l in lines {
        m.prints.push(at.format(&l.join(" ")));
    }
    k(m)
}

/// evalFunc (evalValue).
fn eval_func(m: &mut Machine<'_>, f: &Frame, terms: &[Term], ir: &IndexResult, k: K<'_>) -> R {
    if ir.empty() {
        return Ok(());
    }
    let arg_count = ir.rules.first().or(ir.default.as_ref()).map_or(0, |r| r.rule.head.args.len());
    // The cache, by the plugged operands.
    let plen = if terms.len() == arg_count + 2 { terms.len() - 1 } else { terms.len() };
    let key_terms: Vec<Term> = terms.iter().take(plen).map(|t| if is_ground(t) { t.clone() } else { m.plug(t, f.b) }).collect();
    let key = format!("fn:{}", Term::new(TermValue::Array(key_terms), None));
    if let Some(Some(cached)) = m.vcache_get(&key).cloned() {
        if arg_count == terms.len() - 1 {
            if matches!(cached.value, TermValue::Bool(false)) {
                return Ok(());
            }
            return k(m);
        }
        let Some(out) = terms.last() else { return Ok(()) };
        let tmp = m.new_bindings();
        return unify(m, f, out, &cached, f.b, tmp, k);
    }
    let find_one = ir.early_exit;
    let mut prev: Option<Term> = None;
    let mut outer: Option<Flow> = None;
    let res = (|| -> R {
        for (i, rule) in ir.rules.iter().enumerate() {
            let (mut next_v, r1) = func_one_rule(m, f, terms, rule, &key, &mut prev, find_one, k);
            if is_deferred(&r1) {
                outer.get_or_insert(r1.err().unwrap_or(Flow::Early { deferred: true, prev: None }));
            } else {
                r1?;
            }
            if next_v.is_none() {
                for erule in ir.elses.get(&i).map(Vec::as_slice).unwrap_or_default() {
                    let (nv, r1) = func_one_rule(m, f, terms, erule, &key, &mut prev, find_one, k);
                    if is_deferred(&r1) {
                        outer.get_or_insert(r1.err().unwrap_or(Flow::Early { deferred: true, prev: None }));
                    } else {
                        r1?;
                    }
                    if nv.is_some() {
                        next_v = nv;
                        break;
                    }
                }
            }
            if next_v.is_some() {
                prev = next_v;
            }
        }
        if let Some(d) = &ir.default
            && prev.is_none()
        {
            let (_, r1) = func_one_rule(m, f, terms, d, &key, &mut prev, find_one, k);
            return r1;
        }
        match outer.take() {
            Some(o) => Err(o),
            None => Ok(()),
        }
    })();
    suppress(res)
}

#[allow(clippy::too_many_arguments)]
fn func_one_rule(m: &mut Machine<'_>, f: &Frame, terms: &[Term], rule: &Rc<RuleRec>, key: &str, prev: &mut Option<Term>, find_one: bool, k: K<'_>) -> (Option<Term>, R) {
    let mut cf = child(m, f, rule.rule.body.clone());
    cf.find_one = find_one;
    let mut args: Vec<Term> = rule.rule.head.args.clone();
    let value = rule.rule.head.value.clone().unwrap_or_else(|| Term::boolean(true, None));
    if terms.len() - 1 == args.len() + 1 {
        args.push(value.clone());
    }
    let ins: Vec<Term> = terms.iter().skip(1).cloned().collect();
    let loc = rule.rule.loc.clone();
    let mut result: Option<Term> = None;
    let nargs = rule.rule.head.args.len();
    let res = unify_terms(m, f, &ins, &args, f.b, cf.b, 0, &mut |m| {
        eval_expr(m, &cf, &mut |m, cf| {
            let v = m.plug(&value, cf.b);
            result = Some(v.clone());
            m.vcache_put(key.to_string(), Some(v.clone()));
            if nargs == terms.len() - 1 && matches!(v.value, TermValue::Bool(false)) {
                if prev.as_ref().is_some_and(|p| !p.equal(&v)) {
                    return Err(err(CONFLICT_ERR, loc.clone(), "functions must not produce multiple outputs for same inputs"));
                }
                *prev = Some(v);
                return Ok(());
            }
            if let Some(p) = prev.as_ref() {
                if !p.equal(&v) {
                    return Err(err(CONFLICT_ERR, loc.clone(), "functions must not produce multiple outputs for same inputs"));
                }
                return Ok(());
            }
            *prev = Some(v);
            k(m)
        })
    });
    (result, res)
}

#[allow(clippy::too_many_arguments)]
fn unify_terms(m: &mut Machine<'_>, f: &Frame, a: &[Term], b: &[Term], b1: usize, b2: usize, i: usize, k: K<'_>) -> R {
    if a.len() != b.len() {
        return Ok(());
    }
    let (Some(x), Some(y)) = (a.get(i), b.get(i)) else { return k(m) };
    unify(m, f, x, y, b1, b2, &mut |m| unify_terms(m, f, a, b, b1, b2, i + 1, k))
}

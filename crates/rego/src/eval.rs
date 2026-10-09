//! OPA's evaluator (topdown/eval.go, save.go, bindings.go, query.go, v1.14.1): queries over
//! compiled modules, by unification with continuations, structured as topdown is: query
//! ids and bindings numbered as OPA numbers them, virtual documents from rules with OPA's
//! caches, early exit, conflicts and errors in OPA's words, and partial evaluation (save
//! set, save stack, support modules, inlining control) as `rego.Partial` runs it.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use crate::ast::{
    Body, Every, Expr, ExprTerms, Head, Location, Module, Package, Rule, RuleKind, Term, TermValue, With,
};
use crate::compare::{expr_compare, rule_compare, term_compare};
use crate::compile::vars::{self as cvars, Params, VarSet, VarVisitor};
use crate::compile::{Compiler, ground_prefix, rule_ref};
use crate::copyprop::{self, CopyPropagator, eq_expr, is_constant, ref_has_prefix};
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
        // A host's halt is its own error, as topdown.Halt returns the error it wraps.
        if self.code == HALT {
            return f.write_str(&self.message);
        }
        match &self.loc {
            Some(l) if !l.file.is_empty() => {
                write!(f, "{}:{}: {}: {}", l.file, l.row, self.code, self.message)
            }
            Some(l) => write!(f, "{}:{}: {}: {}", l.row, l.col, self.code, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

/// A host function's halt, which carries no code of its own.
pub const HALT: &str = "";
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
    Flow::Err(EvalError {
        code,
        message: message.into(),
        loc,
    })
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

/// deferredEarlyExitContainer.handleErr: keeps the first deferred early exit, passes
/// other errors.
fn handle_deferred(r: R, deferred: &mut Option<Flow>) -> R {
    if is_deferred(&r) {
        if deferred.is_none() {
            *deferred = r.err();
        }
        return Ok(());
    }
    r
}

/// Why a function the host answers failed: an error that leaves its call undefined, as
/// OPA records a builtin's (topdown evalBuiltin), or one that stops the query
/// (topdown.Halt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    Undefined(String),
    Halt(String),
}

/// A function the host answers: buildx's own, in policies.
pub trait Host {
    fn call(&mut self, name: &str, args: &[Value]) -> Result<Option<Value>, HostError>;

    /// A line `print` printed (PrintHook), in order with what the host's functions say;
    /// false leaves it to the machine's `prints`.
    fn print(&mut self, _line: &str) -> bool {
        false
    }
}

/// A rule, its path (Rule.Ref), its package's length and its else chain.
#[derive(Debug)]
pub struct RuleRec {
    pub rule: Rule,
    pub path: Vec<Term>,
    pub pkg_len: usize,
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

    /// DepthFirst over the node and its descendants, collecting their rules.
    fn all_values(&self, out: &mut Vec<Rc<RuleRec>>) {
        out.extend(self.values.iter().cloned());
        for (_, c) in &self.children {
            c.all_values(out);
        }
    }
}

/// Compiled modules, ready to evaluate.
#[derive(Debug)]
pub struct Program {
    pub root: TreeNode,
    host_names: Vec<String>,
    /// Comprehensions evaluated once and indexed by these variables
    /// (buildComprehensionIndices), by the comprehension's key.
    compr_index: HashMap<String, Vec<Term>>,
    /// The rule indices, by rule tree path (TreeNode.Index).
    indices: BTreeMap<Vec<String>, crate::index::RuleIndex>,
    /// Each rule and else branch, by the index's name for it.
    nodes: HashMap<crate::compile::RuleNode, Rc<RuleRec>>,
    /// The compiler's type environment, for the checks on saved bodies.
    type_env: crate::check::TypeEnv,
}

/// A comprehension's identity: where it is and what it says.
fn compr_key(t: &Term) -> String {
    match &t.loc {
        Some(l) => format!("{}:{}:{}:{t}", l.file, l.row, l.col),
        None => format!("-:{t}"),
    }
}

impl Program {
    pub fn new(c: &Compiler, host_names: Vec<String>) -> Program {
        let mut root = TreeNode::default();
        let mut nodes = HashMap::new();
        for (name, m) in &c.modules {
            add_module(&mut root, name, m, &mut nodes);
        }
        sort_tree(&mut root);
        let mut compr_index = HashMap::new();
        let arity = |r: &[Term]| c.arity(r);
        for m in c.modules.values() {
            for rule in &m.rules {
                let mut x = Some(rule);
                while let Some(r) = x {
                    let mut candidates: VarSet = ["data", "input"].iter().map(|v| Rc::from(*v)).collect();
                    let mut v = VarVisitor::default();
                    v.args(&r.head.args);
                    candidates.extend(v.vars);
                    build_compr_indices(&arity, &mut candidates, &r.body, &mut compr_index);
                    x = r.else_.as_deref();
                }
            }
        }
        Program {
            root,
            host_names,
            compr_index,
            indices: c.indices.clone(),
            nodes,
            type_env: c.type_env.clone(),
        }
    }

    fn rules_at(&self, r: &[Term]) -> Vec<Rc<RuleRec>> {
        self.root.find(r).map(|n| n.values.clone()).unwrap_or_default()
    }

    /// GetArity: a builtin's, a host function's or a function rule's arguments.
    pub fn arity(&self, r: &[Term]) -> Option<usize> {
        let name = crate::compile::text_of_ref(r);
        if let Some(crate::types::Type::Function { args, .. }) =
            crate::compile::allowed(&name).map(|b| &b.decl)
        {
            return Some(args.len());
        }
        if self.host_names.contains(&name) {
            return Some(host_arity(&name));
        }
        let first = self.rules_at(r);
        first.first().map(|x| x.rule.head.args.len())
    }

    /// isFunction over the type environment: a builtin, a host function or a function
    /// rule.
    fn is_function(&self, r: &[Term]) -> bool {
        let name = crate::compile::text_of_ref(r);
        crate::compile::allowed(&name).is_some()
            || self.host_names.contains(&name)
            || self.rules_at(r).iter().any(|x| !x.rule.head.args.is_empty())
    }

    /// GetRulesDynamicWithOpts: the rules a ref may refer to, variables matching any key.
    fn rules_dynamic(&self, r: &[Term]) -> Vec<Rc<RuleRec>> {
        let mut out = Vec::new();
        dynamic_walk(&self.root, r, 0, &mut out);
        let mut seen: Vec<*const RuleRec> = Vec::new();
        out.retain(|x| {
            let p = Rc::as_ptr(x);
            if seen.contains(&p) {
                return false;
            }
            seen.push(p);
            true
        });
        out
    }

    /// Compiler.PassesTypeCheck over a saved body.
    fn passes_type_check(&self, body: &Body) -> bool {
        let mut env = self.type_env.clone();
        crate::check::Checker::new(None)
            .check_body(&mut env, body)
            .is_empty()
    }
}

fn dynamic_walk(node: &TreeNode, r: &[Term], i: usize, out: &mut Vec<Rc<RuleRec>>) {
    let Some(t) = r.get(i) else {
        node.all_values(out);
        return;
    };
    if i == 0 || is_constant(t) {
        if let Some(c) = node.child(t) {
            out.extend(c.values.iter().cloned());
            dynamic_walk(c, r, i + 1, out);
        }
        return;
    }
    for (_, c) in &node.children {
        out.extend(c.values.iter().cloned());
        dynamic_walk(c, r, i + 1, out);
    }
}

fn add_module(
    root: &mut TreeNode,
    name: &str,
    m: &Module,
    nodes: &mut HashMap<crate::compile::RuleNode, Rc<RuleRec>>,
) {
    let pkg_len = m.package.path.len();
    for (ri, rule) in m.rules.iter().enumerate() {
        let path = rule_ref(&m.package.path, rule);
        let mut elses = Vec::new();
        let mut e = rule.else_.as_deref();
        while let Some(x) = e {
            let mut r = x.clone();
            r.else_ = None;
            let rec = Rc::new(RuleRec {
                rule: r,
                path: path.clone(),
                pkg_len,
                elses: Vec::new(),
            });
            nodes.insert((name.to_string(), ri, elses.len() + 1), rec.clone());
            elses.push(rec);
            e = x.else_.as_deref();
        }
        let mut r = rule.clone();
        r.else_ = None;
        let rec = Rc::new(RuleRec {
            rule: r,
            path: path.clone(),
            pkg_len,
            elses,
        });
        nodes.insert((name.to_string(), ri, 0), rec.clone());
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
            let Some(slot) = node.children.get_mut(i) else {
                return;
            };
            node = &mut slot.1;
        }
        node.values.push(rec);
    }
}

/// buildComprehensionIndices over a body and the bodies nested in it (WalkBodies order),
/// the candidates growing with each expression met.
fn build_compr_indices(
    arity: crate::compile::safety::Arity<'_>,
    candidates: &mut VarSet,
    body: &Body,
    out: &mut HashMap<String, Vec<Term>>,
) {
    for e in body {
        if let Some((t, keys)) = compr_index(arity, candidates, e) {
            out.insert(compr_key(&t), keys);
        }
        let p = Params {
            skip_closures: true,
            skip_ref_call_head: true,
            ..Params::default()
        };
        candidates.extend(cvars::expr_vars(e, p));
    }
    for e in body {
        for nested in nested_bodies(e) {
            build_compr_indices(arity, candidates, &nested, out);
        }
    }
}

/// The bodies directly nested in an expression's terms (comprehensions, every).
fn nested_bodies(e: &Expr) -> Vec<Body> {
    let mut out = Vec::new();
    if let ExprTerms::Every(ev) = &e.terms {
        out.push(ev.body.clone());
    }
    crate::compile::safety::walk_terms_expr(e, &mut |t: &Term| match &t.value {
        TermValue::ArrayCompr(_, b) | TermValue::SetCompr(_, b) | TermValue::ObjectCompr(_, _, b) => {
            out.push(b.to_vec());
            true
        }
        _ => false,
    });
    out
}

/// getComprehensionIndex.
fn compr_index(
    arity: crate::compile::safety::Arity<'_>,
    candidates: &VarSet,
    e: &Expr,
) -> Option<(Term, Vec<Term>)> {
    if !e.is_equality() || e.negated || !e.with.is_empty() {
        return None;
    }
    let (lhs, rhs) = (e.operand(0)?, e.operand(1)?);
    let term = if is_var(lhs) && is_compr(rhs) {
        rhs
    } else if is_var(rhs) && is_compr(lhs) {
        lhs
    } else {
        return None;
    };
    let body = match &term.value {
        TermValue::ArrayCompr(_, b) | TermValue::SetCompr(_, b) | TermValue::ObjectCompr(_, _, b) => b,
        _ => return None,
    };
    let reserved: VarSet = ["data", "input"].iter().map(|v| Rc::from(*v)).collect();
    let outputs = crate::compile::safety::output_vars_for_body(body, arity, &reserved);
    let all = cvars::body_vars(body, cvars::SAFETY);
    if all.iter().any(|v| !outputs.contains(v) && !reserved.contains(v)) {
        return None;
    }
    if compr_regression(candidates, body) || compr_nested_candidate(candidates, body) {
        return None;
    }
    let mut keys: Vec<Term> = candidates.intersection(&outputs).map(|v| var_term(v)).collect();
    if keys.is_empty() {
        return None;
    }
    keys.sort_by(term_compare);
    Some((term.clone(), keys))
}

/// The comprehension index's regression check: a candidate a ref would bind before
/// the body has seen it.
fn compr_regression(candidates: &VarSet, body: &Body) -> bool {
    let mut seen = VarSet::new();
    let mut worse = false;
    for e in body {
        crate::compile::safety::walk_terms_expr(e, &mut |t: &Term| {
            if worse {
                return true;
            }
            match &t.value {
                TermValue::Ref(r) => {
                    for x in r.iter().skip(1) {
                        if let TermValue::Var(v) = &x.value
                            && candidates.contains(v)
                            && !seen.contains(v)
                        {
                            worse = true;
                        }
                    }
                    false
                }
                TermValue::Var(v) => {
                    seen.insert(v.clone());
                    false
                }
                TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..) => true,
                _ => false,
            }
        });
    }
    worse
}

/// The nested-candidate check: a comprehension in the body over a candidate.
fn compr_nested_candidate(candidates: &VarSet, body: &Body) -> bool {
    let mut found = false;
    for e in body {
        crate::compile::safety::walk_terms_expr(e, &mut |t: &Term| {
            if found {
                return true;
            }
            if is_compr(t) {
                let mut v = VarVisitor::new(Params {
                    skip_ref_head: true,
                    ..Params::default()
                });
                v.term(t);
                found = v.vars.iter().any(|x| candidates.contains(x));
                return true;
            }
            false
        });
    }
    found
}

fn sort_tree(n: &mut TreeNode) {
    n.children.sort_by(|a, b| term_compare(&a.0, &b.0));
    for (_, c) in n.children.iter_mut() {
        sort_tree(c);
    }
}

/// OPA's bindingsArrayHashmap holds 16 bindings in an array before it becomes a map.
const MAX_LINEAR_SCAN: usize = 16;

/// The bindings of one query: variables to terms and the bindings those terms are in,
/// numbered by the query that made them.
#[derive(Debug, Default)]
struct Bindings {
    id: u64,
    values: Vec<(Rc<str>, Term, usize)>,
    /// Past MAX_LINEAR_SCAN, OPA keeps a Go map, iterated at random; these keep their
    /// order.
    map_mode: bool,
}

/// One frame of the save set: refs and variables (in the bindings they are in) that
/// partial evaluation must not evaluate.
#[derive(Debug, Clone)]
struct SaveSetElem {
    refs: Vec<Vec<Term>>,
    vars: Vec<Term>,
    b: Option<usize>,
}

/// An expression saved for the partial result, with the bindings to plug it with.
#[derive(Debug, Clone)]
struct SaveStackElem {
    expr: Expr,
    b1: Option<usize>,
    b2: Option<usize>,
}

/// A disableInliningFrame.
#[derive(Debug, Clone)]
struct DisableFrame {
    internal: bool,
    refs: Vec<Vec<Term>>,
    var: Option<Rc<str>>,
}

/// Partial evaluation's state: the save set and stack, the support modules made, and
/// what may be inlined.
#[derive(Debug)]
struct Partial {
    set: Vec<SaveSetElem>,
    stack: Vec<Vec<SaveStackElem>>,
    /// Support modules by their package's text.
    support: BTreeMap<String, Module>,
    namespace: Term,
    skip_namespace: bool,
}

/// A query being evaluated: OPA's `eval`, its shared parts in the Machine.
#[derive(Debug, Clone)]
struct Frame {
    query: Rc<Body>,
    index: usize,
    b: usize,
    qid: u64,
    find_one: bool,
    input: Option<Rc<Term>>,
    data: Option<Rc<Term>>,
    /// genvarid: the eval's counter, copied into its closures and children.
    genvarid: Rc<Cell<u64>>,
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

/// A virtual cache entry: the value, or undefined.
#[derive(Debug, Clone, Default)]
struct VEntry {
    value: Option<Term>,
    undefined: bool,
}

/// What every query of one evaluation shares.
pub struct Machine<'p> {
    p: &'p Program,
    bindings: Vec<Bindings>,
    next_qid: u64,
    /// The virtual-document cache, a scope per `with`.
    vcache: Vec<HashMap<String, VEntry>>,
    /// Function and builtin replacements (functionMocksStack): elements of frames.
    mocks: Vec<Vec<Vec<(String, Term)>>>,
    /// The documents `with` replaced, a scope per `with` (targetStack).
    targets: Vec<Vec<Vec<Term>>>,
    /// Indexed comprehensions' values by their keys, a scope per `with`.
    ccache: Vec<HashMap<String, HashMap<String, Term>>>,
    /// inliningControl.
    shallow: bool,
    disable: Vec<DisableFrame>,
    partial: Option<Partial>,
    /// The bindings of the query partial evaluation started from (e.caller.bindings).
    caller: usize,
    pub prints: Vec<String>,
    /// Builtins' errors: recorded, the call undefined, as OPA does without strict errors.
    pub builtin_errors: Vec<EvalError>,
    pub ctx: funcs::Context,
    host: &'p mut dyn Host,
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
        TermValue::Object(o) => Value::object(
            o.iter()
                .map(|(k, v)| Some((to_value(k)?, to_value(v)?)))
                .collect::<Option<_>>()?,
        ),
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

fn is_compr(t: &Term) -> bool {
    matches!(
        t.value,
        TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..)
    )
}

fn ref_of(r: &[Term]) -> Term {
    Term::reference(r.to_vec(), None)
}

fn int_term(i: usize) -> Term {
    Term::new(
        TermValue::Number(Number::from_i64(i64::try_from(i).unwrap_or(i64::MAX))),
        None,
    )
}

/// The virtual cache's key for a ref: each part's text.
fn key_text(r: &[Term]) -> String {
    let mut s = String::new();
    for (i, t) in r.iter().enumerate() {
        if i > 0 {
            s.push('\u{1f}');
        }
        s.push_str(&t.to_string());
    }
    s
}

/// Builds a set of terms, repeats kept once.
fn set_of(items: Vec<Term>) -> Term {
    crate::ast::set_term(items, None)
}

/// Expr.ComplementNoWith.
fn complement_no_with(e: &Expr) -> Expr {
    let mut c = e.clone();
    c.negated = !c.negated;
    c.with.clear();
    c
}

/// Expr.Complement.
fn complement(e: &Expr) -> Expr {
    let mut c = e.clone();
    c.negated = !c.negated;
    c
}

/// isOtherRef: a `with` target outside data and input.
fn is_other_ref(t: &Term) -> bool {
    let h = t.as_ref().and_then(|r| r.first()).and_then(Term::as_var);
    !matches!(h, Some("data") | Some("input"))
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
            ccache: vec![HashMap::new()],
            shallow: false,
            disable: Vec::new(),
            partial: None,
            caller: 0,
            prints: Vec::new(),
            builtin_errors: Vec::new(),
            ctx,
            host,
        }
    }

    fn new_bindings(&mut self, id: u64) -> usize {
        self.bindings.push(Bindings {
            id,
            ..Bindings::default()
        });
        self.bindings.len() - 1
    }

    /// queryIDFactory.Next.
    fn qid(&mut self) -> u64 {
        let q = self.next_qid;
        self.next_qid += 1;
        q
    }

    fn is_partial(&self) -> bool {
        self.partial.is_some()
    }

    fn bget(&self, b: usize, v: &str) -> Option<(&Term, usize)> {
        self.bindings
            .get(b)?
            .values
            .iter()
            .find(|(k, _, _)| &**k == v)
            .map(|(_, t, nb)| (t, *nb))
    }

    /// bindings.apply: a variable's value, followed through the bindings it is in; and
    /// whether it was bound.
    fn apply_flag(&self, t: &Term, b: usize) -> (Term, usize, bool) {
        let mut t = t.clone();
        let mut b = b;
        let mut bound = false;
        loop {
            let TermValue::Var(v) = &t.value else {
                return (t, b, bound);
            };
            match self.bget(b, v) {
                Some((nt, nb)) => {
                    let (nt, nb) = (nt.clone(), nb);
                    t = nt;
                    b = nb;
                    bound = true;
                }
                None => return (t, b, bound),
            }
        }
    }

    fn apply(&self, t: &Term, b: usize) -> (Term, usize) {
        let (t, b, _) = self.apply_flag(t, b);
        (t, b)
    }

    /// bindings.namespaceVar.
    fn namespace_var(&self, v: &Rc<str>, b: usize, caller: Option<usize>) -> Term {
        if let Some(c) = caller
            && c != b
            && !matches!(&**v, "data" | "input")
            && let Some(x) = self.bindings.get(b)
        {
            return var_term(&format!("{v}{}", x.id));
        }
        Term::var(v, None)
    }

    /// bindings.PlugNamespaced.
    fn plug_ns(&self, t: &Term, b: usize, caller: Option<usize>) -> Term {
        match &t.value {
            TermValue::Var(v) => {
                let (nt, nb, bound) = self.apply_flag(t, b);
                if bound {
                    return self.plug_ns(&nt, nb, caller);
                }
                let mut out = self.namespace_var(v, b, caller);
                out.loc = t.loc.clone();
                out
            }
            TermValue::Array(a) => {
                if t.is_ground() {
                    return t.clone();
                }
                Term::new(
                    TermValue::Array(a.iter().map(|x| self.plug_ns(x, b, caller)).collect()),
                    t.loc.clone(),
                )
            }
            TermValue::Object(o) => {
                if t.is_ground() {
                    return t.clone();
                }
                let pairs = o
                    .iter()
                    .map(|(k, v)| (self.plug_ns(k, b, caller), self.plug_ns(v, b, caller)))
                    .collect();
                crate::ast::object_term(pairs, t.loc.clone())
            }
            TermValue::Set(s) => {
                if t.is_ground() {
                    return t.clone();
                }
                crate::ast::set_term(
                    s.iter().map(|x| self.plug_ns(x, b, caller)).collect(),
                    t.loc.clone(),
                )
            }
            TermValue::Ref(r) => Term::new(
                TermValue::Ref(r.iter().map(|x| self.plug_ns(x, b, caller)).collect()),
                t.loc.clone(),
            ),
            _ => t.clone(),
        }
    }

    /// bindings.Plug.
    fn plug(&self, t: &Term, b: usize) -> Term {
        self.plug_ns(t, b, None)
    }

    /// Plugs with optional bindings (the save stack's nil bindings leave a term as is).
    fn plug_opt(&self, t: &Term, b: Option<usize>, caller: Option<usize>) -> Term {
        match b {
            Some(b) => self.plug_ns(t, b, caller),
            None => t.clone(),
        }
    }

    /// bindings.bind; the undo is the variable and the bindings.
    fn bind(&mut self, a: &Term, value: &Term, vb: usize, b: usize) -> Option<(Rc<str>, usize)> {
        let TermValue::Var(v) = &a.value else { return None };
        let slot = self.bindings.get_mut(b)?;
        if let Some(e) = slot.values.iter_mut().find(|(k, _, _)| k == v) {
            e.1 = value.clone();
            e.2 = vb;
        } else {
            slot.values.push((v.clone(), value.clone(), vb));
            if slot.values.len() > MAX_LINEAR_SCAN {
                slot.map_mode = true;
            }
        }
        Some((v.clone(), b))
    }

    /// undo.Undo: bindingsArrayHashmap.Delete, the last entry moved into the gap while
    /// the bindings are an array.
    fn unbind(&mut self, u: Option<(Rc<str>, usize)>) {
        let Some((v, b)) = u else { return };
        let Some(slot) = self.bindings.get_mut(b) else {
            return;
        };
        let Some(i) = slot.values.iter().position(|(k, _, _)| *k == v) else {
            return;
        };
        if slot.map_mode {
            slot.values.remove(i);
        } else {
            slot.values.swap_remove(i);
        }
    }

    /// bindings.Iter: each bound variable and its value, plugged and namespaced.
    fn iter_bindings(&self, b: usize, caller: Option<usize>) -> Vec<(Term, Term)> {
        let Some(x) = self.bindings.get(b) else {
            return Vec::new();
        };
        x.values
            .iter()
            .map(|(k, _, _)| (Term::var(k, None), self.plug_ns(&Term::var(k, None), b, caller)))
            .collect()
    }

    fn vcache_get(&self, key: &str) -> (Option<Term>, bool) {
        match self.vcache.last().and_then(|c| c.get(key)) {
            Some(e) if e.undefined => (None, true),
            Some(e) => (e.value.clone(), false),
            None => (None, false),
        }
    }

    fn vcache_put(&mut self, key: String, v: Option<Term>) {
        if let Some(c) = self.vcache.last_mut() {
            let e = c.entry(key).or_default();
            match v {
                Some(v) => e.value = Some(v),
                None => e.undefined = true,
            }
        }
    }

    /// functionMocksStack.Get.
    fn mock(&self, name: &str) -> Option<Term> {
        let current = self.mocks.last()?;
        current
            .iter()
            .rev()
            .find_map(|frame| frame.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()))
    }

    // ---- inliningControl

    fn disabled_ref(&self, r: &[Term], ignore_internal: bool) -> bool {
        self.disable.iter().any(|f| {
            (!f.internal || !ignore_internal)
                && f.refs
                    .iter()
                    .any(|o| ref_has_prefix(o, r) || ref_has_prefix(r, o))
        })
    }

    fn disabled_var(&self, v: &str, ignore_internal: bool) -> bool {
        self.disable
            .iter()
            .any(|f| (!f.internal || !ignore_internal) && f.var.as_deref() == Some(v))
    }

    fn disabled_term(&self, t: &Term, ignore_internal: bool) -> bool {
        match &t.value {
            TermValue::Ref(r) => self.disabled_ref(r, ignore_internal),
            TermValue::Var(v) => self.disabled_var(v, ignore_internal),
            _ => false,
        }
    }

    // ---- saveSet

    fn sse_contains_var(&self, e: &SaveSetElem, t: &Term, b: usize) -> bool {
        e.b == Some(b) && e.vars.iter().any(|v| v.equal(t))
    }

    fn sse_contains(&self, e: &SaveSetElem, t: &Term, b: Option<usize>) -> bool {
        match &t.value {
            TermValue::Var(_) => b.is_some_and(|b| self.sse_contains_var(e, t, b)),
            TermValue::Ref(other) => {
                if e.refs
                    .iter()
                    .any(|r| ref_has_prefix(r, other) || ref_has_prefix(other, r))
                {
                    return true;
                }
                match (other.first(), b) {
                    (Some(h), Some(b)) => self.sse_contains_var(e, h, b),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// saveSet.Contains.
    fn ss_contains(&self, t: &Term, b: Option<usize>) -> bool {
        let Some(p) = &self.partial else { return false };
        p.set.iter().rev().any(|e| self.sse_contains(e, t, b))
    }

    /// saveSet.ContainsRecursive.
    fn ss_contains_rec(&self, t: &Term, b: usize) -> bool {
        if self.partial.is_none() {
            return false;
        }
        let mut found = false;
        crate::compile::safety::walk_terms(t, &mut |x: &Term| {
            if found {
                return true;
            }
            if is_var(x) {
                let (x1, b1, bound) = self.apply_flag(x, b);
                if bound || b1 != b {
                    if self.ss_contains_rec(&x1, b1) {
                        found = true;
                    }
                } else if self.ss_contains(&x1, Some(b1)) {
                    found = true;
                }
            }
            found
        });
        found
    }

    /// saveSet.Vars.
    fn ss_vars(&self, caller: usize) -> VarSet {
        let mut out = VarSet::new();
        let Some(p) = &self.partial else { return out };
        for e in &p.set {
            for v in &e.vars {
                if let Some(b) = e.b
                    && let TermValue::Var(x) = self.plug_ns(v, b, Some(caller)).value
                {
                    out.insert(x);
                }
            }
        }
        out
    }

    fn ss_push(&mut self, ts: Vec<Term>, b: Option<usize>) {
        let mut refs = Vec::new();
        let mut vars = Vec::new();
        for t in ts {
            match t.value {
                TermValue::Var(_) => vars.push(t),
                TermValue::Ref(r) => refs.push(r.into_inner()),
                _ => {}
            }
        }
        if let Some(p) = self.partial.as_mut() {
            p.set.push(SaveSetElem { refs, vars, b });
        }
    }

    fn ss_pop(&mut self) {
        if let Some(p) = self.partial.as_mut() {
            p.set.pop();
        }
    }

    // ---- saveStack

    fn stack_push_query(&mut self, q: Vec<SaveStackElem>) {
        if let Some(p) = self.partial.as_mut() {
            p.stack.push(q);
        }
    }

    fn stack_pop_query(&mut self) -> Vec<SaveStackElem> {
        self.partial
            .as_mut()
            .and_then(|p| p.stack.pop())
            .unwrap_or_default()
    }

    fn stack_peek(&self) -> Vec<SaveStackElem> {
        self.partial
            .as_ref()
            .and_then(|p| p.stack.last().cloned())
            .unwrap_or_default()
    }

    fn stack_push(&mut self, expr: Expr, b1: Option<usize>, b2: Option<usize>) {
        if let Some(q) = self.partial.as_mut().and_then(|p| p.stack.last_mut()) {
            q.push(SaveStackElem { expr, b1, b2 });
        }
    }

    fn stack_pop(&mut self) {
        if let Some(q) = self.partial.as_mut().and_then(|p| p.stack.last_mut()) {
            q.pop();
        }
    }

    /// saveStackElem.Plug.
    fn elem_plug(&self, e: &SaveStackElem, caller: usize) -> Expr {
        if e.b1.is_none() && e.b2.is_none() {
            return e.expr.clone();
        }
        let mut x = e.expr.clone();
        let c = Some(caller);
        let eq = x.is_equality();
        match &mut x.terms {
            ExprTerms::Call(terms) => {
                if eq {
                    if let Some(t) = terms.get_mut(1) {
                        *t = self.plug_opt(t, e.b1, c);
                    }
                    if let Some(t) = terms.get_mut(2) {
                        *t = self.plug_opt(t, e.b2, c);
                    }
                } else {
                    for t in terms.iter_mut().skip(1) {
                        *t = self.plug_opt(t, e.b1, c);
                    }
                }
            }
            ExprTerms::Term(t) => {
                **t = self.plug_opt(t, e.b1, c);
            }
            _ => {}
        }
        for w in x.with.iter_mut() {
            w.value = self.plug_opt(&w.value, e.b1, c);
        }
        x
    }

    /// saveStackQuery.Plug.
    fn query_plug(&self, q: &[SaveStackElem], caller: usize) -> Body {
        if q.is_empty() {
            return vec![Expr::term(Term::boolean(true, None))];
        }
        q.iter()
            .enumerate()
            .map(|(i, e)| {
                let mut x = self.elem_plug(e, caller);
                x.index = i;
                x
            })
            .collect()
    }

    // ---- saveSupport

    fn support_exists(&self, path: &[Term]) -> bool {
        let Some(p) = &self.partial else { return false };
        let (pkg, rule) = split_package_and_rule(path);
        let Some(m) = p.support.get(&ref_text(&pkg)) else {
            return false;
        };
        if rule.len() == 1 {
            let name = rule.first().and_then(Term::as_var).unwrap_or_default();
            return m.rules.iter().any(|r| r.head.name.as_deref() == Some(name));
        }
        m.rules.iter().any(|r| ref_has_prefix(&r.head.ref_path(), &rule))
    }

    fn support_insert(&mut self, path: &[Term], rule: Rule) {
        let (pkg, _) = split_package_and_rule(path);
        self.support_insert_by_pkg(pkg, rule);
    }

    fn support_insert_by_pkg(&mut self, pkg: Vec<Term>, rule: Rule) {
        let Some(p) = self.partial.as_mut() else { return };
        let k = ref_text(&pkg);
        let m = p.support.entry(k).or_insert_with(|| Module {
            package: Package { path: pkg, loc: None },
            imports: Vec::new(),
            rules: Vec::new(),
        });
        m.rules.push(rule);
    }

    /// eval.namespaceRef.
    fn namespace_ref(&self, r: &[Term]) -> Vec<Term> {
        let Some(p) = &self.partial else { return r.to_vec() };
        if p.skip_namespace {
            return r.to_vec();
        }
        let mut out = r.to_vec();
        let at = 1.min(out.len());
        out.insert(at, p.namespace.clone());
        out
    }

    /// The type environment's check, as the compiler runs it on saved bodies.
    fn passes_type_check(&self, body: &Body) -> bool {
        self.p.passes_type_check(body)
    }

    /// saveRequired.
    fn save_required(&self, ignore_internal: bool, b: usize, node: Node<'_>, rec: bool) -> bool {
        let mut found = false;
        self.save_required_walk(ignore_internal, b, node, rec, &mut found);
        found
    }

    fn save_required_walk(&self, ii: bool, b: usize, node: Node<'_>, rec: bool, found: &mut bool) {
        if *found {
            return;
        }
        match node {
            Node::Expr(e) => {
                if !e.with.is_empty() || ignore_during_partial(e) {
                    *found = true;
                    return;
                }
                match &e.terms {
                    ExprTerms::Term(t) => self.save_required_walk(ii, b, Node::Term(t), rec, found),
                    ExprTerms::Call(c) => c
                        .iter()
                        .for_each(|t| self.save_required_walk(ii, b, Node::Term(t), rec, found)),
                    ExprTerms::Some(d) => d
                        .symbols
                        .iter()
                        .for_each(|t| self.save_required_walk(ii, b, Node::Term(t), rec, found)),
                    ExprTerms::Every(ev) => self.save_required_every(ii, b, ev, rec, found),
                }
                for w in &e.with {
                    self.save_required_walk(ii, b, Node::Term(&w.target), rec, found);
                    self.save_required_walk(ii, b, Node::Term(&w.value), rec, found);
                }
            }
            Node::Body(body) => body
                .iter()
                .for_each(|e| self.save_required_walk(ii, b, Node::Expr(e), rec, found)),
            Node::Rule(r) => {
                let h = &r.head;
                h.args
                    .iter()
                    .for_each(|t| self.save_required_walk(ii, b, Node::Term(t), rec, found));
                h.reference
                    .iter()
                    .for_each(|t| self.save_required_walk(ii, b, Node::Term(t), rec, found));
                if let Some(k) = &h.key {
                    self.save_required_walk(ii, b, Node::Term(k), rec, found);
                }
                if let Some(v) = &h.value {
                    self.save_required_walk(ii, b, Node::Term(v), rec, found);
                }
                self.save_required_walk(ii, b, Node::Body(&r.body), rec, found);
            }
            Node::Term(t) => {
                match &t.value {
                    TermValue::Var(_) => {
                        if !rec && self.ss_contains_rec(t, b) {
                            *found = true;
                            return;
                        }
                    }
                    TermValue::Ref(r) => {
                        if self.ss_contains(t, Some(b)) || self.disabled_ref(&constant_prefix(r), ii) {
                            *found = true;
                            return;
                        }
                        for rule in self.p.rules_dynamic(r) {
                            let chain: Vec<&RuleRec> = std::iter::once(&*rule)
                                .chain(rule.elses.iter().map(|x| &**x))
                                .collect();
                            for x in chain {
                                if self.save_required(ii, b, Node::Rule(&x.rule), true) {
                                    *found = true;
                                    return;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                match &t.value {
                    TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) | TermValue::Set(r) => r
                        .iter()
                        .for_each(|x| self.save_required_walk(ii, b, Node::Term(x), rec, found)),
                    TermValue::Object(o) => o.iter().for_each(|(k, v)| {
                        self.save_required_walk(ii, b, Node::Term(k), rec, found);
                        self.save_required_walk(ii, b, Node::Term(v), rec, found);
                    }),
                    TermValue::ArrayCompr(x, body) | TermValue::SetCompr(x, body) => {
                        self.save_required_walk(ii, b, Node::Term(x), rec, found);
                        self.save_required_walk(ii, b, Node::Body(body), rec, found);
                    }
                    TermValue::ObjectCompr(k, v, body) => {
                        self.save_required_walk(ii, b, Node::Term(k), rec, found);
                        self.save_required_walk(ii, b, Node::Term(v), rec, found);
                        self.save_required_walk(ii, b, Node::Body(body), rec, found);
                    }
                    TermValue::TemplateString { parts, .. } => {
                        for p in parts {
                            match p {
                                crate::ast::TemplatePart::Term(x) => {
                                    self.save_required_walk(ii, b, Node::Term(x), rec, found)
                                }
                                crate::ast::TemplatePart::Expr(e) => {
                                    self.save_required_walk(ii, b, Node::Expr(e), rec, found)
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn save_required_every(&self, ii: bool, b: usize, ev: &Every, rec: bool, found: &mut bool) {
        if let Some(k) = &ev.key {
            self.save_required_walk(ii, b, Node::Term(k), rec, found);
        }
        self.save_required_walk(ii, b, Node::Term(&ev.value), rec, found);
        self.save_required_walk(ii, b, Node::Term(&ev.domain), rec, found);
        self.save_required_walk(ii, b, Node::Body(&ev.body), rec, found);
    }

    /// eval.unknown.
    fn unknown(&self, node: Node<'_>, b: usize) -> bool {
        self.is_partial() && self.save_required(true, b, node, false)
    }

    /// eval.unknownRef.
    fn unknown_ref(&self, r: &[Term], b: usize) -> bool {
        self.is_partial() && self.save_required(true, b, Node::Term(&ref_of(r)), false)
    }

    /// updateSavedMocks: the `with`s a saved expression keeps (data and input targets).
    fn update_saved_mocks(&self, withs: &[With]) -> Vec<With> {
        withs
            .iter()
            .filter(|w| !is_other_ref(&w.target) && !w.target.as_ref().is_some_and(|r| self.p.is_function(r)))
            .cloned()
            .collect()
    }
}

/// What saveRequired walks.
#[derive(Clone, Copy)]
enum Node<'a> {
    Term(&'a Term),
    Expr(&'a Expr),
    Body(&'a [Expr]),
    Rule(&'a Rule),
}

/// ignoreExprDuringPartial: the calls partial evaluation leaves for later
/// (IgnoreDuringPartialEval, nondeterministic builtins).
fn ignore_during_partial(e: &Expr) -> bool {
    let ExprTerms::Call(c) = &e.terms else {
        return false;
    };
    let Some(name) = c.first().map(Term::to_string) else {
        return false;
    };
    let Some(b) = crate::builtins::registry().get(&name) else {
        return false;
    };
    matches!(
        name.as_str(),
        "rand.intn"
            | "uuid.rfc4122"
            | "io.jwt.decode_verify"
            | "io.jwt.encode_sign_raw"
            | "io.jwt.encode_sign"
            | "time.now_ns"
            | "http.send"
            | "opa.runtime"
            | "net.lookup_ip_addr"
    ) || b.nondeterministic
}

/// Ref.ConstantPrefix.
fn constant_prefix(r: &[Term]) -> Vec<Term> {
    if let Some(TermValue::Call(_)) = r.first().map(|t| &t.value) {
        return Vec::new();
    }
    match r.iter().skip(1).position(|t| !is_constant(t)) {
        Some(i) => r.get(..i + 1).unwrap_or_default().to_vec(),
        None => r.to_vec(),
    }
}

fn ref_text(r: &[Term]) -> String {
    crate::compile::text_of_ref(r)
}

/// splitPackageAndRule.
fn split_package_and_rule(path: &[Term]) -> (Vec<Term>, Vec<Term>) {
    let string_prefix = 1 + path
        .iter()
        .skip(1)
        .take_while(|t| matches!(t.value, TermValue::String(_)))
        .count();
    let mut start = 2;
    for i in 2..string_prefix {
        match path.get(i).and_then(Term::as_string) {
            Some(s) if crate::ast::is_var_compatible(s) => start = i,
            _ => break,
        }
    }
    let start = start.min(path.len());
    let pkg = path.get(..start).unwrap_or_default().to_vec();
    let mut rule = path.get(start..).unwrap_or_default().to_vec();
    if let Some(first) = rule.first_mut()
        && let TermValue::String(s) = &first.value
    {
        first.value = TermValue::Var(s.clone());
    }
    (pkg, rule)
}

/// Starts a query's evaluation: the root eval, query id 0, bindings 0.
fn root_frame(m: &mut Machine<'_>, query: Body, input: Option<Value>) -> Frame {
    let qid = m.qid();
    let b = m.new_bindings(qid);
    m.caller = b;
    Frame {
        query: Rc::new(query),
        index: 0,
        b,
        qid,
        find_one: false,
        input: input.as_ref().map(|v| Rc::new(to_term(v))),
        data: None,
        genvarid: Rc::new(Cell::new(0)),
    }
}

/// Evaluates `query`'s value: each result of `x = query`, as rego.Eval captures it.
pub fn eval_query(m: &mut Machine<'_>, query: &Term, input: Option<Value>) -> Result<Vec<Value>, EvalError> {
    let capture = var_term("__localq0__");
    let loc = query.loc.clone();
    let expr = Expr::new(
        ExprTerms::Call(vec![
            crate::compile::localvars::op("eq"),
            query.clone(),
            capture.clone(),
        ]),
        loc,
    );
    let f = root_frame(m, vec![expr], input);
    let mut out = Vec::new();
    let r = eval_expr(m, &f, &mut |m, f| {
        let v = m.plug(&capture, f.b);
        if let Some(v) = to_value(&v) {
            out.push(v);
        }
        Ok(())
    });
    suppress_all(r)?;
    Ok(out)
}

/// What partial evaluation made: the queries left and the support modules, sorted by
/// package (OPA lists them from a Go map, in no order).
#[derive(Debug, Default)]
pub struct PartialResult {
    pub queries: Vec<Body>,
    pub support: Vec<Module>,
}

/// Query.PartialRun of a ground ref, as rego.Partial runs it with buildx's options
/// (SkipPartialNamespace): `unknowns` are the refs it must not evaluate.
pub fn partial_query(
    m: &mut Machine<'_>,
    query: &Term,
    input: Option<Value>,
    unknowns: &[Term],
) -> Result<PartialResult, EvalError> {
    let expr = Expr::term(query.clone());
    let f = root_frame(m, vec![expr], input);
    m.partial = Some(Partial {
        set: Vec::new(),
        stack: vec![Vec::new()],
        support: BTreeMap::new(),
        namespace: Term::string("partial", None),
        skip_namespace: true,
    });
    m.ss_push(unknowns.to_vec(), Some(f.b));
    let mut live = VarSet::new();
    for t in unknowns {
        match &t.value {
            TermValue::Var(v) => {
                live.insert(v.clone());
            }
            TermValue::Ref(r) => {
                if let Some(v) = r.first().and_then(Term::as_var) {
                    live.insert(Rc::from(v));
                }
            }
            _ => {}
        }
    }
    let mut vis = VarVisitor::default();
    vis.term(query);
    live.extend(vis.vars.into_iter().filter(|v| !cvars::is_generated(v)));
    let p = m.p;
    let arity = |r: &[Term]| p.arity(r);
    let mut queries = Vec::new();
    let r = eval_expr(m, &f, &mut |m, f| {
        let mut body: Body = Vec::new();
        for e in m.stack_peek() {
            copyprop::append(&mut body, m.elem_plug(&e, f.b));
        }
        let mut exprs: Vec<Expr> = m
            .iter_bindings(f.b, Some(f.b))
            .into_iter()
            .map(|(a, b)| eq_expr(a, b))
            .collect();
        exprs.sort_by(expr_compare);
        for e in exprs {
            copyprop::append(&mut body, e);
        }
        if !m.passes_type_check(&body) {
            return Ok(());
        }
        let mut cp = CopyPropagator::new(live.clone(), false, &arity);
        queries.push(cp.apply(&body));
        Ok(())
    });
    let res = suppress_all(r);
    let mut support: Vec<Module> = m
        .partial
        .take()
        .map(|p| p.support.into_values().collect())
        .unwrap_or_default();
    for module in support.iter_mut() {
        module.rules.sort_by(rule_compare);
    }
    res?;
    Ok(PartialResult { queries, support })
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
                Flow::Early { .. } => Err(Flow::Early {
                    deferred: !f.find_one,
                    prev: Some(Box::new(e)),
                }),
                e => Err(e),
            };
        }
        if f.find_one && !m.is_partial() {
            return Err(Flow::Early {
                deferred: false,
                prev: None,
            });
        }
        return Ok(());
    }
    let Some(expr) = f.query.get(f.index) else {
        return Ok(());
    };
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

fn current_loc(f: &Frame) -> Option<Location> {
    current(f).and_then(|e| e.loc.clone())
}

fn eval_step(m: &mut Machine<'_>, f: &Frame, iter: I<'_>) -> R {
    let Some(expr) = current(f).cloned() else {
        return Ok(());
    };
    if expr.negated {
        return eval_not(m, f, iter);
    }
    match &expr.terms {
        ExprTerms::Call(terms) => {
            if expr.is_equality() && terms.len() == 3 {
                let (Some(a), Some(b)) = (terms.get(1), terms.get(2)) else {
                    return Ok(());
                };
                unify(m, f, a, b, f.b, f.b, &mut |m| iter(m, f))
            } else {
                eval_call(m, f, terms, &mut |m| iter(m, f))
            }
        }
        ExprTerms::Term(t) => {
            let rterm = var_term(&format!("$_term_{}_{}", f.qid, f.index));
            let partial = m.is_partial();
            if partial {
                m.disable.push(DisableFrame {
                    internal: true,
                    refs: Vec::new(),
                    var: rterm.as_var().map(Rc::from),
                });
            }
            let r = unify(m, f, t, &rterm, f.b, f.b, &mut |m| {
                if m.ss_contains(&rterm, Some(f.b)) {
                    return save_expr(m, f, Expr::term(rterm.clone()), f.b, &mut |m| iter(m, f));
                }
                let v = m.plug(&rterm, f.b);
                if matches!(v.value, TermValue::Bool(false)) {
                    return Ok(());
                }
                iter(m, f)
            });
            if partial {
                m.disable.pop();
            }
            r
        }
        ExprTerms::Every(ev) => eval_every(m, f, ev, &expr, iter),
        ExprTerms::Some(_) => Ok(()),
    }
}

/// eval.closure: the same bindings, a new query id.
fn closure(m: &mut Machine<'_>, f: &Frame, body: Body) -> Frame {
    Frame {
        query: Rc::new(body),
        index: 0,
        b: f.b,
        qid: m.qid(),
        find_one: false,
        input: f.input.clone(),
        data: f.data.clone(),
        genvarid: Rc::new(Cell::new(f.genvarid.get())),
    }
}

/// eval.child: new bindings, numbered by the new query id.
fn child(m: &mut Machine<'_>, f: &Frame, body: Body) -> Frame {
    let qid = m.qid();
    let b = m.new_bindings(qid);
    Frame {
        query: Rc::new(body),
        index: 0,
        b,
        qid,
        find_one: false,
        input: f.input.clone(),
        data: f.data.clone(),
        genvarid: Rc::new(Cell::new(f.genvarid.get())),
    }
}

fn eval_not(m: &mut Machine<'_>, f: &Frame, iter: I<'_>) -> R {
    let Some(expr) = current(f).cloned() else {
        return Ok(());
    };
    if m.unknown(Node::Expr(&expr), f.b) {
        return eval_not_partial(m, f, &expr, iter);
    }
    let c = closure(m, f, vec![complement_no_with(&expr)]);
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

/// evalNotPartial.
fn eval_not_partial(m: &mut Machine<'_>, f: &Frame, expr: &Expr, iter: I<'_>) -> R {
    let c = closure(m, f, vec![complement_no_with(expr)]);
    let caller = m.caller;
    let unknowns = m.ss_vars(caller);
    let p = m.p;
    let arity = |r: &[Term]| p.arity(r);
    let shallow = m.shallow;
    let mut saved: Vec<Body> = Vec::new();
    m.stack_push_query(Vec::new());
    let _ = eval_expr(m, &c, &mut |m, _| {
        let q = m.stack_peek();
        let mut plugged = m.query_plug(&q, caller);
        if !m.passes_type_check(&plugged) {
            return Ok(());
        }
        if !shallow {
            plugged = CopyPropagator::new(unknowns.clone(), true, &arity).apply(&plugged);
        }
        saved.push(plugged);
        Ok(())
    });
    m.stack_pop_query();
    if saved.is_empty() {
        return iter(m, f);
    }
    if !can_inline_negation(&unknowns, &saved) {
        return eval_not_partial_support(m, f, c.qid, expr, unknowns, saved, iter);
    }
    complemented_product(m, f, &saved, 0, &mut Vec::new(), iter)
}

fn complemented_product(
    m: &mut Machine<'_>,
    f: &Frame,
    queries: &[Body],
    idx: usize,
    curr: &mut Vec<Expr>,
    iter: I<'_>,
) -> R {
    let Some(q) = queries.get(idx) else {
        let exprs = curr.clone();
        return save_inlined_negated(m, f, exprs, &mut |m| iter(m, f));
    };
    for e in q {
        curr.push(complement(e));
        let r = complemented_product(m, f, queries, idx + 1, curr, iter);
        curr.pop();
        r?;
    }
    Ok(())
}

/// evalNotPartialSupport.
#[allow(clippy::too_many_arguments)]
fn eval_not_partial_support(
    m: &mut Machine<'_>,
    f: &Frame,
    negation_id: u64,
    expr: &Expr,
    unknowns: VarSet,
    queries: Vec<Body>,
    iter: I<'_>,
) -> R {
    let name = format!("__not{}_{}_{}__", f.qid, f.index, negation_id);
    let namespace = m
        .partial
        .as_ref()
        .map(|p| p.namespace.clone())
        .unwrap_or_else(|| Term::string("partial", None));
    let path = vec![var_term("data"), namespace, Term::string(&name, None)];
    let term = ref_of(&path);
    let mut body_vars = VarSet::new();
    for q in &queries {
        body_vars.extend(cvars::body_vars(q, Params::default()));
    }
    let mut args: Vec<Term> = unknowns.intersection(&body_vars).map(|v| var_term(v)).collect();
    args.sort_by(term_compare);
    let head = Head {
        args: args.clone(),
        value: Some(Term::boolean(true, None)),
        ..Head::var(&name, None)
    };
    for q in queries {
        m.support_insert(
            &path,
            Rule {
                default: false,
                head: head.clone(),
                body: q,
                else_: None,
                loc: None,
                generated_body: false,
            },
        );
    }
    let mut cpy = expr.clone();
    cpy.terms = if args.is_empty() {
        ExprTerms::Term(Box::new(term))
    } else {
        let mut t = vec![term];
        t.extend(args);
        ExprTerms::Call(t)
    };
    save_inlined_negated(m, f, vec![cpy], &mut |m| next(m, f, iter))
}

/// canInlineNegation.
fn can_inline_negation(safe: &VarSet, queries: &[Body]) -> bool {
    let mut size: usize = 1;
    for q in queries {
        size = size.saturating_mul(q.len());
        for e in q {
            if contains_nested_ref_or_call(e) {
                return false;
            }
            if !e.negated {
                let vars = cvars::expr_vars(
                    e,
                    Params {
                        skip_ref_call_head: true,
                        skip_closures: true,
                        ..Params::default()
                    },
                );
                if vars
                    .iter()
                    .any(|v| !safe.contains(v) && !matches!(&**v, "data" | "input"))
                {
                    return false;
                }
            }
        }
    }
    size <= 16
}

fn has_ref_or_call(t: &Term) -> bool {
    let mut found = false;
    crate::compile::safety::walk_terms(t, &mut |x: &Term| {
        if matches!(x.value, TermValue::Ref(_) | TermValue::Call(_)) {
            found = true;
        }
        found
    });
    found
}

/// containsNestedRefOrCall.
fn contains_nested_ref_or_call(e: &Expr) -> bool {
    let in_term = |t: &Term| match &t.value {
        TermValue::Ref(r) => r.iter().skip(1).any(has_ref_or_call),
        _ => has_ref_or_call(t),
    };
    match &e.terms {
        ExprTerms::Call(c) if e.is_equality() => c.iter().skip(1).any(in_term),
        ExprTerms::Call(c) => c.iter().skip(1).any(has_ref_or_call),
        ExprTerms::Term(t) => in_term(t),
        _ => false,
    }
}

fn eval_every(m: &mut Machine<'_>, f: &Frame, ev: &Every, expr: &Expr, iter: I<'_>) -> R {
    if m.is_partial() && (m.unknown(Node::Term(&ev.domain), f.b) || m.unknown(Node::Body(&ev.body), f.b)) {
        let plugged = plug_every(m, f, expr);
        return save_expr(m, f, plugged, f.b, &mut |m| iter(m, f));
    }
    let pd = m.plug(&ev.domain, f.b);
    if !matches!(
        pd.value,
        TermValue::Array(_) | TermValue::Object(_) | TermValue::Set(_)
    ) {
        return Ok(());
    }
    let key = ev.key.clone().unwrap_or_else(|| var_term("$_"));
    let loc = ev.domain.loc.clone();
    let mut r = match &ev.domain.value {
        TermValue::Ref(r) => r.to_vec(),
        _ => vec![ev.domain.clone()],
    };
    r.push(key);
    let generator = Expr::new(
        ExprTerms::Call(vec![
            crate::compile::localvars::op("eq"),
            Term::reference(r, loc.clone()),
            ev.value.clone(),
        ]),
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

/// evalEvery.plug: the `every` with its body's terms, key, value and domain plugged.
fn plug_every(m: &Machine<'_>, f: &Frame, expr: &Expr) -> Expr {
    let mut cpy = expr.clone();
    if let ExprTerms::Every(ev) = &mut cpy.terms {
        plug_every_in(m, f, ev);
    }
    cpy
}

fn plug_every_in(m: &Machine<'_>, f: &Frame, ev: &mut Every) {
    let c = Some(m.caller);
    for e in ev.body.iter_mut() {
        match &mut e.terms {
            ExprTerms::Term(t) => **t = m.plug_ns(t, f.b, c),
            ExprTerms::Call(ts) => {
                for t in ts.iter_mut().skip(1) {
                    *t = m.plug_ns(t, f.b, c);
                }
            }
            ExprTerms::Every(inner) => plug_every_in(m, f, inner),
            ExprTerms::Some(_) => {}
        }
    }
    if let Some(k) = &ev.key {
        ev.key = Some(m.plug_ns(k, f.b, c));
    }
    ev.value = m.plug_ns(&ev.value, f.b, c);
    ev.domain = m.plug_ns(&ev.domain, f.b, c);
}

fn eval_with(m: &mut Machine<'_>, f: &Frame, iter: I<'_>) -> R {
    let Some(expr) = current(f).cloned() else {
        return Ok(());
    };
    let mut disable: Vec<Vec<Term>> = Vec::new();
    if m.is_partial() {
        let mut disable_partial: Vec<Vec<Term>> = Vec::new();
        let add = |t: &Term, out: &mut Vec<Vec<Term>>| {
            crate::compile::safety::walk_terms(t, &mut |x: &Term| {
                if let TermValue::Ref(r) = &x.value {
                    out.push(ground_prefix(r));
                }
                false
            });
        };
        for w in &expr.with {
            let target_fn = w.target.as_ref().is_some_and(|r| m.p.is_function(r));
            if target_fn || is_other_ref(&w.target) {
                add(&w.value, &mut disable_partial);
                continue;
            }
            if m.ss_contains_rec(&w.value, f.b) {
                return save_expr_mark_unknowns(m, f, expr.clone(), f.b, &mut |m| next(m, f, iter));
            }
            add(&w.target, &mut disable_partial);
            add(&w.value, &mut disable_partial);
        }
        let mut no_with = expr.clone();
        no_with.with.clear();
        crate::compile::safety::walk_terms_expr(&no_with, &mut |x: &Term| {
            if let TermValue::Ref(r) = &x.value {
                disable_partial.push(ground_prefix(r));
            }
            false
        });
        disable = disable_partial;
    }
    let mut input_pairs: Vec<(Vec<Term>, Term)> = Vec::new();
    let mut data_pairs: Vec<(Vec<Term>, Term)> = Vec::new();
    let mut mocks: Vec<(String, Term)> = Vec::new();
    let mut targets: Vec<Vec<Term>> = Vec::new();
    for w in &expr.with {
        let plugged = m.plug(&w.value, f.b);
        let target = w.target.as_ref().map(<[Term]>::to_vec).unwrap_or_default();
        let name = crate::compile::text_of_ref(&target);
        let head = target.first().and_then(Term::as_var);
        if m.p.is_function(&target) {
            mocks.push((name, plugged));
        } else if head == Some("input") {
            input_pairs.push((target.clone(), plugged));
        } else if head == Some("data") {
            data_pairs.push((target.clone(), plugged));
        } else if crate::compile::allowed(&name).is_some() || m.p.host_names.contains(&name) {
            mocks.push((name, plugged));
            continue;
        }
        targets.push(target);
    }
    let mut g = f.clone();
    if !input_pairs.is_empty() {
        g.input = Some(Rc::new(merge_with(f.input.as_deref(), &input_pairs).ok_or_else(
            || err(CONFLICT_ERR, expr.loc.clone(), "conflicting values for input"),
        )?));
    }
    if !data_pairs.is_empty() {
        g.data = Some(Rc::new(merge_with(f.data.as_deref(), &data_pairs).ok_or_else(
            || err(CONFLICT_ERR, expr.loc.clone(), "conflicting values for data"),
        )?));
    }
    let push = |m: &mut Machine<'_>| {
        m.ccache.push(HashMap::new());
        m.vcache.push(HashMap::new());
        m.targets.push(targets.clone());
        m.disable.push(DisableFrame {
            internal: true,
            refs: disable.clone(),
            var: None,
        });
        if let Some(top) = m.mocks.last_mut() {
            top.push(mocks.clone());
        }
    };
    let pop = |m: &mut Machine<'_>| {
        m.disable.pop();
        m.targets.pop();
        m.vcache.pop();
        m.ccache.pop();
        if let Some(top) = m.mocks.last_mut() {
            top.pop();
        }
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
        let root = doc
            .take()
            .unwrap_or_else(|| crate::ast::object_term(Vec::new(), None));
        doc = Some(set_path(root, path, value.clone())?);
    }
    doc
}

fn set_path(doc: Term, path: &[Term], value: Term) -> Option<Term> {
    let Some((first, rest)) = path.split_first() else {
        return Some(value);
    };
    let TermValue::Object(mut o) = doc.value else {
        return set_path(crate::ast::object_term(Vec::new(), None), path, value);
    };
    let existing = o.iter().position(|(k, _)| k.equal(first));
    let inner = match existing {
        Some(i) => o.remove(i).1,
        None => crate::ast::object_term(Vec::new(), None),
    };
    let new = if rest.is_empty() {
        value
    } else {
        set_path(inner, rest, value)?
    };
    o.push((first.clone(), new));
    Some(crate::ast::object_term(o.into_inner(), doc.loc))
}

/// biunify.
fn unify(m: &mut Machine<'_>, f: &Frame, a: &Term, b: &Term, b1: usize, b2: usize, k: K<'_>) -> R {
    let (a, b1) = m.apply(a, b1);
    let (b, b2) = m.apply(b, b2);
    use TermValue as V;
    match (&a.value, &b.value) {
        (V::Var(_) | V::Ref(_) | V::ArrayCompr(..) | V::SetCompr(..) | V::ObjectCompr(..), _) => {
            unify_values(m, f, &a, &b, b1, b2, k)
        }
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
            let x: Vec<(Term, Term)> = x
                .iter()
                .map(|(kk, v)| {
                    (
                        if kk.is_ground() {
                            kk.clone()
                        } else {
                            m.plug(kk, b1)
                        },
                        v.clone(),
                    )
                })
                .collect();
            let y: Vec<(Term, Term)> = y
                .iter()
                .map(|(kk, v)| {
                    (
                        if kk.is_ground() {
                            kk.clone()
                        } else {
                            m.plug(kk, b2)
                        },
                        v.clone(),
                    )
                })
                .collect();
            let mut keys = x.clone();
            keys.sort_by(|p, q| term_compare(&p.0, &q.0));
            unify_objects(m, f, &keys, &y, b1, b2, 0, k)
        }
        (V::Set(_), _) => unify_values(m, f, &a, &b, b1, b2, k),
        _ => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn unify_slices(
    m: &mut Machine<'_>,
    f: &Frame,
    a: &[Term],
    b: &[Term],
    b1: usize,
    b2: usize,
    i: usize,
    k: K<'_>,
) -> R {
    let (Some(x), Some(y)) = (a.get(i), b.get(i)) else {
        return k(m);
    };
    unify(m, f, x, y, b1, b2, &mut |m| {
        unify_slices(m, f, a, b, b1, b2, i + 1, k)
    })
}

#[allow(clippy::too_many_arguments)]
fn unify_objects(
    m: &mut Machine<'_>,
    f: &Frame,
    a: &[(Term, Term)],
    b: &[(Term, Term)],
    b1: usize,
    b2: usize,
    i: usize,
    k: K<'_>,
) -> R {
    let Some((key, av)) = a.get(i) else { return k(m) };
    let Some((_, bv)) = b.iter().find(|(bk, _)| bk.equal(key)) else {
        return Ok(());
    };
    unify(m, f, av, bv, b1, b2, &mut |m| {
        unify_objects(m, f, a, b, b1, b2, i + 1, k)
    })
}

/// biunifyValues.
fn unify_values(m: &mut Machine<'_>, f: &Frame, a: &Term, b: &Term, b1: usize, b2: usize, k: K<'_>) -> R {
    let save_a = if matches!(a.value, TermValue::Set(_)) {
        m.ss_contains_rec(a, b1)
    } else {
        let s = m.ss_contains(a, Some(b1));
        if !s && matches!(a.value, TermValue::Ref(_)) {
            return unify_ref(m, f, a, b, b1, b2, k);
        }
        s
    };
    let save_b = if matches!(b.value, TermValue::Set(_)) {
        m.ss_contains_rec(b, b2)
    } else {
        let s = m.ss_contains(b, Some(b2));
        if !s && matches!(b.value, TermValue::Ref(_)) {
            return unify_ref(m, f, b, a, b2, b1, k);
        }
        s
    };
    if save_a || save_b {
        return save_unify(m, f, a.clone(), b.clone(), b1, b2, k);
    }
    if is_compr(a) {
        return unify_comprehension(m, f, a, b, b1, b2, false, k);
    } else if is_compr(b) {
        return unify_comprehension(m, f, b, a, b2, b1, true, k);
    }
    match (is_var(a), is_var(b)) {
        (true, true) => {
            if b1 == b2 && a.equal(b) {
                return k(m);
            }
            let u = m.bind(a, b, b2, b1);
            let r = k(m);
            m.unbind(u);
            r
        }
        (true, false) => {
            let u = m.bind(a, b, b2, b1);
            let r = k(m);
            m.unbind(u);
            r
        }
        (false, true) => {
            let u = m.bind(b, a, b1, b2);
            let r = k(m);
            m.unbind(u);
            r
        }
        (false, false) => {
            let (pa, pb) = if matches!(a.value, TermValue::Set(_)) {
                (m.plug(a, b1), m.plug(b, b2))
            } else {
                (a.clone(), b.clone())
            };
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
        let node = r
            .first()
            .and_then(|h| m.p.root.child(h))
            .map(|n| n as *const TreeNode);
        return eval_tree(m, f, r, 1, plugged, b1, b, b2, node, k);
    }
    let (term, tb) = if head == Some("input") {
        match &f.input {
            Some(i) => ((**i).clone(), b1),
            None => return Ok(()),
        }
    } else {
        let Some(h) = r.first() else { return Ok(()) };
        let (t, tb, bound) = m.apply_flag(h, b1);
        if !bound {
            return Ok(());
        }
        (t, tb)
    };
    eval_term(m, f, r, 1, b1, &term, tb, b, b2, k)
}

/// evalTerm: the rest of a ref, into a value.
#[allow(clippy::too_many_arguments)]
fn eval_term(
    m: &mut Machine<'_>,
    f: &Frame,
    r: &[Term],
    pos: usize,
    b: usize,
    term: &Term,
    tb: usize,
    rterm: &Term,
    rb: usize,
    k: K<'_>,
) -> R {
    if pos == r.len() {
        return unify(m, f, term, rterm, tb, rb, k);
    }
    if m.ss_contains(term, Some(tb)) {
        return eval_term_save(m, f, r, pos, b, term, tb, rterm, rb, k);
    }
    let Some(part) = r.get(pos) else { return Ok(()) };
    let plugged = m.plug(part, b);
    if plugged.is_ground() {
        let Some((t, nb)) = term_get(m, term, tb, &plugged) else {
            return Ok(());
        };
        return eval_term(m, f, r, pos + 1, b, &t, nb, rterm, rb, k);
    }
    let mut deferred: Option<Flow> = None;
    match &term.value {
        TermValue::Array(a) => {
            for i in 0..a.len() {
                let idx = int_term(i);
                if is_var(part) {
                    let (bv, bb) = m.apply(part, b);
                    let u = m.bind(&bv, &idx, bb, bb);
                    let r1 = match term_get(m, term, tb, &idx) {
                        Some((t, nb)) => eval_term(m, f, r, pos + 1, bb, &t, nb, rterm, rb, k),
                        None => Ok(()),
                    };
                    m.unbind(u);
                    handle_deferred(r1, &mut deferred)?;
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
                handle_deferred(r1, &mut deferred)?;
            }
        }
        TermValue::Set(s) => {
            let mut items = s.to_vec();
            items.sort_by(term_compare);
            for elem in items {
                let r1 = unify(m, f, &elem, part, tb, b, &mut |m| {
                    let pe = m.plug(&elem, tb);
                    match term_get(m, term, tb, &pe) {
                        Some((t, nb)) => eval_term(m, f, r, pos + 1, b, &t, nb, rterm, rb, k),
                        None => Ok(()),
                    }
                });
                handle_deferred(r1, &mut deferred)?;
            }
        }
        _ => {}
    }
    match deferred {
        Some(d) => Err(d),
        None => Ok(()),
    }
}

/// evalTerm.save: a saved term's rest as a ref on a fresh variable.
#[allow(clippy::too_many_arguments)]
fn eval_term_save(
    m: &mut Machine<'_>,
    f: &Frame,
    r: &[Term],
    pos: usize,
    b: usize,
    term: &Term,
    tb: usize,
    rterm: &Term,
    rb: usize,
    k: K<'_>,
) -> R {
    let id = f.genvarid.get();
    let v = var_term(&format!("$_ref_{id}"));
    f.genvarid.set(id + 1);
    unify(m, f, term, &v, tb, b, &mut |m| {
        let mut rr = vec![v.clone()];
        rr.extend(r.iter().skip(pos).cloned());
        unify(m, f, &ref_of(&rr), rterm, b, rb, k)
    })
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
            let TermValue::Number(n) = &key.value else {
                return None;
            };
            let i = n.as_i64().and_then(|i| usize::try_from(i).ok())?;
            a.get(i).map(|t| m.apply(t, tb))
        }
        _ => None,
    }
}

/// evalTree: a ref into data: the rule tree, then the base document.
#[allow(clippy::too_many_arguments)]
fn eval_tree(
    m: &mut Machine<'_>,
    f: &Frame,
    r: &[Term],
    pos: usize,
    plugged: Vec<Term>,
    b: usize,
    rterm: &Term,
    rb: usize,
    node: Option<*const TreeNode>,
    k: K<'_>,
) -> R {
    if pos == r.len() {
        if m.is_partial() && m.unknown_ref(&plugged, b) {
            return save_unify(m, f, ref_of(&plugged), rterm.clone(), b, rb, k);
        }
        let v = tree_extent(m, f, &plugged, node)?;
        let Some(v) = v else { return Ok(()) };
        return unify(m, f, rterm, &v, rb, b, k);
    }
    let Some(part) = r.get(pos) else { return Ok(()) };
    let p = m.plug(part, b);
    if p.is_ground() {
        return tree_next(m, f, r, pos, plugged, b, rterm, rb, node, p, k);
    }
    // enumerate: keys of the base document, then the rule tree's children.
    if m.disabled_ref(plugged.get(..pos).unwrap_or_default(), true) {
        return save_unify(m, f, ref_of(&plugged), rterm.clone(), b, rb, k);
    }
    let base = resolve(m, f, plugged.get(..pos).unwrap_or_default())?;
    let mut deferred: Option<Flow> = None;
    if let Some(doc) = base {
        let keys: Vec<Term> = match &doc.value {
            TermValue::Array(a) => (0..a.len()).map(int_term).collect(),
            TermValue::Object(o) => {
                let mut ks: Vec<Term> = o.iter().map(|(kk, _)| kk.clone()).collect();
                ks.sort_by(term_compare);
                ks
            }
            TermValue::Set(s) => {
                let mut ks = s.to_vec();
                ks.sort_by(term_compare);
                ks
            }
            _ => Vec::new(),
        };
        for key in keys {
            let pl = plugged.clone();
            let r1 = unify(m, f, &key, part, b, b, &mut |m| {
                tree_next(m, f, r, pos, pl.clone(), b, rterm, rb, node, key.clone(), k)
            });
            handle_deferred(r1, &mut deferred)?;
        }
    }
    if let Some(d) = deferred {
        return Err(d);
    }
    let Some(n) = node else { return Ok(()) };
    let keys: Vec<Term> = node_ref(m, n).children.iter().map(|(kk, _)| kk.clone()).collect();
    for key in keys {
        let pl = plugged.clone();
        unify(m, f, &key, part, b, b, &mut |m| {
            tree_next(m, f, r, pos, pl.clone(), b, rterm, rb, node, key.clone(), k)
        })?;
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
fn tree_next(
    m: &mut Machine<'_>,
    f: &Frame,
    r: &[Term],
    pos: usize,
    mut plugged: Vec<Term>,
    b: usize,
    rterm: &Term,
    rb: usize,
    node: Option<*const TreeNode>,
    p: Term,
    k: K<'_>,
) -> R {
    if let Some(slot) = plugged.get_mut(pos) {
        *slot = p.clone();
    }
    let mut child_node = None;
    let path = plugged.get(..=pos).unwrap_or_default();
    let replaced = m.targets.iter().flatten().any(|t| ref_has_prefix(path, t));
    if !replaced
        && let Some(n) = node
        && let Some(c) = node_ref(m, n).child(&p)
    {
        if !c.values.is_empty() {
            return eval_virtual(m, f, r, plugged, pos, b, rterm, rb, k);
        }
        child_node = Some(c as *const TreeNode);
    }
    eval_tree(m, f, r, pos + 1, plugged, b, rterm, rb, child_node, k)
}

/// eval.Resolve for a data ref: what `with data...` put there, and the store's empty
/// root (buildx loads no data).
fn resolve(m: &Machine<'_>, f: &Frame, path: &[Term]) -> Result<Option<Term>, Flow> {
    let mut rep = f.data.as_ref().map(|d| (**d).clone());
    for t in path.iter().skip(1) {
        let Some(doc) = rep else { break };
        rep = match &doc.value {
            TermValue::Object(o) => o.iter().find(|(kk, _)| kk.equal(t)).map(|(_, v)| v.clone()),
            _ => None,
        };
    }
    if m.targets.iter().flatten().any(|t| ref_has_prefix(path, t)) {
        return Ok(rep);
    }
    if path.iter().skip(1).any(|t| {
        !matches!(
            t.value,
            TermValue::Null | TermValue::Bool(_) | TermValue::Number(_) | TermValue::String(_)
        )
    }) {
        return Ok(rep);
    }
    if path.len() == 1 {
        let store = crate::ast::object_term(Vec::new(), None);
        return match rep {
            None => Ok(Some(store)),
            Some(a) => merge_values(&a, &store).map(Some).ok_or_else(|| {
                err(
                    CONFLICT_ERR,
                    path.first().and_then(|t| t.loc.clone()),
                    "real and replacement data could not be merged",
                )
            }),
        };
    }
    Ok(rep)
}

/// evalTree.extent: the whole document at a path, rules' values included.
fn tree_extent(
    m: &mut Machine<'_>,
    f: &Frame,
    plugged: &[Term],
    node: Option<*const TreeNode>,
) -> Result<Option<Term>, Flow> {
    let base = resolve(m, f, plugged)?;
    let virtual_doc = match node {
        Some(n) => leaves(m, f, plugged.to_vec(), n)?,
        None => None,
    };
    let loc = plugged.first().and_then(|t| t.loc.clone());
    Ok(match (base, virtual_doc) {
        (None, None) => None,
        (Some(b), None) => Some(b),
        (None, Some(v)) => Some(v),
        (Some(b), Some(v)) => Some(merge_values(&b, &v).ok_or_else(|| {
            err(
                WITH_MERGE_ERR,
                loc,
                "real and replacement data could not be merged",
            )
        })?),
    })
}

/// merge: objects merged deeply, else the first.
fn merge_values(a: &Term, b: &Term) -> Option<Term> {
    match (&a.value, &b.value) {
        (TermValue::Object(x), TermValue::Object(y)) => {
            let mut out: Vec<(Term, Term)> = Vec::new();
            for (k, v) in x {
                match y.iter().find(|(yk, _)| yk.equal(k)) {
                    None => out.push((k.clone(), v.clone())),
                    Some((_, v2)) => match (&v.value, &v2.value) {
                        (TermValue::Object(_), TermValue::Object(_)) => {
                            out.push((k.clone(), merge_values(v, v2)?))
                        }
                        _ => out.push((k.clone(), v.clone())),
                    },
                }
            }
            for (k, v) in y {
                if !x.iter().any(|(xk, _)| xk.equal(k)) {
                    out.push((k.clone(), v.clone()));
                }
            }
            Some(crate::ast::object_term(out, None))
        }
        _ => Some(a.clone()),
    }
}

fn leaves(
    m: &mut Machine<'_>,
    f: &Frame,
    plugged: Vec<Term>,
    n: *const TreeNode,
) -> Result<Option<Term>, Flow> {
    let children: Vec<(Term, bool, *const TreeNode)> = node_ref(m, n)
        .children
        .iter()
        .map(|(k, c)| (k.clone(), !c.values.is_empty(), c as *const TreeNode))
        .collect();
    let mut result: Vec<(Term, Term)> = Vec::new();
    for (key, has_values, c) in children {
        let mut path = plugged.clone();
        path.push(key.clone());
        let save = if has_values {
            let rterm = var_term("$_leaf");
            let mut out = None;
            unify(m, f, &ref_of(&path), &rterm, f.b, f.b, &mut |m| {
                out = Some(m.plug(&rterm, f.b));
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

/// getRules: the rule index's answer at a path, its resolver evalResolver's.
fn get_rules(m: &Machine<'_>, f: &Frame, path: &[Term], args: Option<&[Term]>) -> Result<IndexResult, Flow> {
    let Some(index) = m.p.indices.get(&crate::compile::ref_key(path)) else {
        return Ok(IndexResult::default());
    };
    let mut resolver =
        |r: &[Term]| -> Result<crate::index::Resolved<Term>, Flow> { resolve_for_index(m, f, args, r) };
    let found = index.lookup(&mut resolver)?;
    let rec = |n: &crate::compile::RuleNode| m.p.nodes.get(n).cloned();
    let mut ir = IndexResult {
        default: found.default.as_ref().and_then(rec),
        kind_multi: found.kind == RuleKind::MultiValue,
        early_exit: found.early_exit,
        only_ground_refs: found.only_ground_refs,
        ..IndexResult::default()
    };
    for n in &found.rules {
        let Some(r) = rec(n) else { continue };
        if let Some(es) = found.else_.get(n) {
            ir.elses
                .insert(ir.rules.len(), es.iter().filter_map(rec).collect());
        }
        ir.rules.push(r);
    }
    Ok(ir)
}

/// evalResolver.Resolve: what a ref the index looks at is.
fn resolve_for_index(
    m: &Machine<'_>,
    f: &Frame,
    args: Option<&[Term]>,
    r: &[Term],
) -> Result<crate::index::Resolved<Term>, Flow> {
    use crate::index::Resolved;
    if m.disabled_ref(r, true) || m.ss_contains(&ref_of(r), None) {
        return Ok(Resolved::Unknown);
    }
    match r.first().and_then(Term::as_var) {
        Some("args") => {
            let i = r.get(1).and_then(|t| match &t.value {
                TermValue::Number(n) => n.as_i64().and_then(|i| usize::try_from(i).ok()),
                _ => None,
            });
            match i.and_then(|i| args.and_then(|a| a.get(i))) {
                Some(a) => Ok(Resolved::Value(m.plug_ns(a, f.b, Some(m.caller)))),
                None => Ok(Resolved::Unknown),
            }
        }
        Some("input") => match &f.input {
            Some(i) => {
                Ok(find_in(i, r.get(1..).unwrap_or_default()).map_or(Resolved::Undefined, Resolved::Value))
            }
            None => Ok(Resolved::Undefined),
        },
        Some("data") => Ok(resolve(m, f, r)?.map_or(Resolved::Undefined, Resolved::Value)),
        _ => Err(err(INTERNAL_ERR, None, "illegal ref")),
    }
}

/// evalVirtual.
#[allow(clippy::too_many_arguments)]
fn eval_virtual(
    m: &mut Machine<'_>,
    f: &Frame,
    r: &[Term],
    plugged: Vec<Term>,
    pos: usize,
    b: usize,
    rterm: &Term,
    rb: usize,
    k: K<'_>,
) -> R {
    let ir = get_rules(m, f, plugged.get(..=pos).unwrap_or_default(), None)?;
    if !ir.elses.is_empty() && m.unknown_ref(r, b) {
        return save_unify(m, f, ref_of(r), rterm.clone(), b, rb, k);
    }
    let v = Virtual {
        r,
        plugged,
        pos,
        b,
        rterm,
        rb,
        ir: &ir,
    };
    if ir.kind_multi {
        let empty = if ir.only_ground_refs {
            set_of(Vec::new())
        } else {
            crate::ast::object_term(Vec::new(), None)
        };
        return eval_partial(m, f, &v, empty, k);
    }
    if ir.only_ground_refs {
        return eval_complete(m, f, &v, k);
    }
    eval_partial(m, f, &v, crate::ast::object_term(Vec::new(), None), k)
}

/// What evalVirtualComplete and evalVirtualPartial share.
struct Virtual<'a> {
    r: &'a [Term],
    plugged: Vec<Term>,
    pos: usize,
    b: usize,
    rterm: &'a Term,
    rb: usize,
    ir: &'a IndexResult,
}

impl Virtual<'_> {
    fn path(&self) -> &[Term] {
        self.plugged.get(..=self.pos).unwrap_or_default()
    }
}

/// evalVirtualComplete.
fn eval_complete(m: &mut Machine<'_>, f: &Frame, v: &Virtual<'_>, k: K<'_>) -> R {
    let ir = v.ir;
    if ir.empty() {
        return Ok(());
    }
    if ir.rules.first().is_some_and(|x| !x.rule.head.args.is_empty())
        || ir.default.as_ref().is_some_and(|d| !d.rule.head.args.is_empty())
    {
        return Ok(());
    }
    if !m.unknown_ref(v.r, v.b) {
        return complete_value(m, f, v, ir.early_exit, k);
    }
    let mut generate_support = false;
    if let Some(d) = &ir.default {
        let dv = d
            .rule
            .head
            .value
            .clone()
            .unwrap_or_else(|| Term::boolean(true, None));
        if !(matches!(dv.value, TermValue::Bool(false)) && m.disabled_term(v.rterm, false)) {
            let rt = m.plug(v.rterm, v.rb);
            generate_support = !is_constant(&rt) || dv.equal(&rt);
        }
    }
    if generate_support || m.shallow || m.disabled_ref(v.path(), false) {
        return complete_support(m, f, v, k);
    }
    // partialEval: each rule's value, inlined.
    for rule in &ir.rules {
        let cf = child(m, f, rule.rule.body.clone());
        let value = rule
            .rule
            .head
            .value
            .clone()
            .unwrap_or_else(|| Term::boolean(true, None));
        eval_expr(m, &cf, &mut |m, cf| {
            let (t, tb) = m.apply(&value, cf.b);
            eval_term(m, f, v.r, v.pos + 1, v.b, &t, tb, v.rterm, v.rb, k)
        })?;
    }
    Ok(())
}

/// evalVirtualComplete.evalValue.
fn complete_value(m: &mut Machine<'_>, f: &Frame, v: &Virtual<'_>, find_one: bool, k: K<'_>) -> R {
    let ir = v.ir;
    let key = key_text(v.path());
    match m.vcache_get(&key) {
        (_, true) => return Ok(()),
        (Some(cached), _) => return eval_term(m, f, v.r, v.pos + 1, v.b, &cached, v.b, v.rterm, v.rb, k),
        (None, false) => {}
    }
    let mut prev: Option<Term> = None;
    let mut deferred: Option<Flow> = None;
    let res = (|| -> R {
        for (i, rule) in ir.rules.iter().enumerate() {
            let (next_v, r1) = complete_value_rule(m, f, v, rule, &mut prev, find_one, &key, k);
            handle_deferred(r1, &mut deferred)?;
            let mut next_v = next_v;
            if next_v.is_none() {
                for erule in ir.elses.get(&i).map(Vec::as_slice).unwrap_or_default() {
                    let (nv, r1) = complete_value_rule(m, f, v, erule, &mut prev, find_one, &key, k);
                    handle_deferred(r1, &mut deferred)?;
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
            let (_, r1) = complete_value_rule(m, f, v, d, &mut prev, find_one, &key, k);
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

/// evalVirtualComplete.evalValueRule.
#[allow(clippy::too_many_arguments)]
fn complete_value_rule(
    m: &mut Machine<'_>,
    f: &Frame,
    v: &Virtual<'_>,
    rule: &Rc<RuleRec>,
    prev: &mut Option<Term>,
    find_one: bool,
    key: &str,
    k: K<'_>,
) -> (Option<Term>, R) {
    let mut cf = child(m, f, rule.rule.body.clone());
    cf.find_one = find_one;
    let mut result: Option<Term> = None;
    let value = rule
        .rule
        .head
        .value
        .clone()
        .unwrap_or_else(|| Term::boolean(true, None));
    let loc = rule.rule.loc.clone();
    let res = eval_expr(m, &cf, &mut |m, cf| {
        let x = m.plug(&value, cf.b);
        result = Some(x.clone());
        if let Some(p) = prev.as_ref() {
            if term_compare(&x, p) != std::cmp::Ordering::Equal {
                return Err(err(
                    CONFLICT_ERR,
                    loc.clone(),
                    "complete rules must not produce multiple outputs",
                ));
            }
            return Ok(());
        }
        *prev = Some(x.clone());
        m.vcache_put(key.to_string(), Some(x));
        let (t, tb) = m.apply(&value, cf.b);
        eval_term(m, f, v.r, v.pos + 1, v.b, &t, tb, v.rterm, v.rb, k)
    });
    (result, res)
}

/// evalVirtualComplete.partialEvalSupport.
fn complete_support(m: &mut Machine<'_>, f: &Frame, v: &Virtual<'_>, k: K<'_>) -> R {
    let original = v.path().to_vec();
    let ns_path = m.namespace_ref(&original);
    let term = ref_of(&m.namespace_ref(v.r));
    let mut defined = m.support_exists(&ns_path);
    if !defined {
        let rules: Vec<Rc<RuleRec>> =
            v.ir.rules
                .iter()
                .cloned()
                .chain(v.ir.default.iter().cloned())
                .collect();
        for rule in rules {
            let mut rule_ref: Vec<Term> = original.get(rule.pkg_len..).unwrap_or_default().to_vec();
            if let Some(first) = rule_ref.first_mut()
                && let TermValue::String(s) = &first.value
            {
                first.value = TermValue::Var(s.clone());
            }
            let pkg = ns_path
                .get(..ns_path.len().saturating_sub(rule_ref.len()))
                .unwrap_or_default()
                .to_vec();
            if complete_support_rule(m, f, &rule, pkg, rule_ref)? {
                defined = true;
            }
        }
    }
    if !defined {
        return Ok(());
    }
    save_unify(m, f, term, v.rterm.clone(), v.b, v.rb, k)
}

/// evalVirtualComplete.partialEvalSupportRule.
fn complete_support_rule(
    m: &mut Machine<'_>,
    f: &Frame,
    rule: &Rc<RuleRec>,
    pkg: Vec<Term>,
    rule_ref: Vec<Term>,
) -> Result<bool, Flow> {
    let cf = child(m, f, rule.rule.body.clone());
    m.stack_push_query(Vec::new());
    let caller = m.caller;
    let p = m.p;
    let arity = |r: &[Term]| p.arity(r);
    let mut defined = false;
    let value = rule
        .rule
        .head
        .value
        .clone()
        .unwrap_or_else(|| Term::boolean(true, None));
    let r = eval_expr(m, &cf, &mut |m, cf| {
        defined = true;
        let current = m.stack_pop_query();
        let mut plugged = m.query_plug(&current, caller);
        if m.passes_type_check(&plugged) {
            let head = Head::reference(rule_ref.clone(), Some(m.plug_ns(&value, cf.b, Some(caller))));
            if !m.shallow {
                let live = head_vars(&head);
                plugged = CopyPropagator::new(live, true, &arity).apply(&plugged);
            }
            m.support_insert_by_pkg(
                pkg.clone(),
                Rule {
                    default: rule.rule.default,
                    head,
                    body: plugged,
                    else_: None,
                    loc: None,
                    generated_body: false,
                },
            );
        }
        m.stack_push_query(current);
        Ok(())
    });
    m.stack_pop_query();
    r?;
    Ok(defined)
}

/// Head.Vars: the variables of the head's args, key, value and ref.
fn head_vars(h: &Head) -> VarSet {
    let mut v = VarVisitor::default();
    v.args(&h.args);
    if let Some(k) = &h.key {
        v.term(k);
    }
    if let Some(x) = &h.value {
        v.term(x);
    }
    for t in h.reference.iter().skip(1) {
        v.term(t);
    }
    v.vars
}

/// maxRefLength.
fn max_ref_length(rules: &[Rc<RuleRec>], ceil: usize) -> usize {
    let mut l = 0;
    for r in rules {
        let mut rl = r.path.len();
        if r.rule.head.kind() == RuleKind::MultiValue {
            rl += 1;
        }
        if rl >= ceil {
            return ceil;
        } else if rl > l {
            l = rl;
        }
    }
    l
}

/// evalVirtualPartial.
fn eval_partial(m: &mut Machine<'_>, f: &Frame, v: &Virtual<'_>, empty: Term, k: K<'_>) -> R {
    let unknown = m.unknown(Node::Term(&ref_of(v.r.get(..=v.pos).unwrap_or_default())), v.b);
    if v.r.len() == v.pos + 1 {
        if unknown {
            return partial_support(m, f, v, &empty, k);
        }
        return partial_all_rules(m, f, v, empty, k);
    }
    if (unknown && m.shallow) || m.disabled_ref(v.r.get(..=v.pos).unwrap_or_default(), false) {
        return partial_support(m, f, v, &empty, k);
    }
    partial_each_rule(m, f, v, empty, unknown, k)
}

/// evalVirtualPartial.evalAllRules.
fn partial_all_rules(m: &mut Machine<'_>, f: &Frame, v: &Virtual<'_>, empty: Term, k: K<'_>) -> R {
    let key = key_text(v.path());
    if let (Some(cached), _) = m.vcache_get(&key) {
        return unify(m, f, &cached, v.rterm, v.b, v.rb, k);
    }
    let result = all_rules_no_cache(m, f, v, empty)?;
    m.vcache_put(key, Some(result.clone()));
    unify(m, f, &result, v.rterm, v.b, v.rb, k)
}

fn all_rules_no_cache(m: &mut Machine<'_>, f: &Frame, v: &Virtual<'_>, empty: Term) -> Result<Term, Flow> {
    let mut result = empty;
    let mut visited: Vec<Vec<Term>> = Vec::new();
    for rule in &v.ir.rules {
        let cf = child(m, f, rule.rule.body.clone());
        let mut res = Some(result);
        eval_expr(m, &cf, &mut |m, cf| {
            let cur = res.take().unwrap_or_else(|| set_of(Vec::new()));
            let (next_v, _) = reduce(m, v.pos, rule, cf.b, cur, &mut visited)?;
            res = Some(next_v);
            Ok(())
        })?;
        result = res.unwrap_or_else(|| set_of(Vec::new()));
    }
    Ok(result)
}

/// The key of evalVirtualPartial's cache hint, scopes as vcKeyScope writes them.
enum HintPart {
    Term(Term),
    Scope(Vec<Term>),
}

fn hint_key(parts: &[HintPart]) -> String {
    let mut s = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            s.push('\u{1f}');
        }
        match p {
            HintPart::Term(t) => s.push_str(&t.to_string()),
            HintPart::Scope(r) => {
                let items: Vec<String> = r
                    .iter()
                    .map(|t| if is_var(t) { "_".to_string() } else { t.to_string() })
                    .collect();
                s.push('<');
                s.push_str(&items.join(","));
                s.push('>');
            }
        }
    }
    s
}

/// evalVirtualPartial.evalEachRule.
fn partial_each_rule(
    m: &mut Machine<'_>,
    f: &Frame,
    v: &Virtual<'_>,
    empty: Term,
    unknown: bool,
    k: K<'_>,
) -> R {
    let ir = v.ir;
    if ir.empty() {
        return Ok(());
    }
    if m.is_partial() {
        let mx = max_ref_length(&ir.rules, v.r.len());
        let suffix = v.r.get(v.pos + 1..mx.max(v.pos + 1)).unwrap_or_default();
        if m.unknown(Node::Term(&ref_of(suffix)), v.b) {
            for rule in &ir.rules {
                partial_one_rule_post_unify(m, f, v, rule, k)?;
            }
            return Ok(());
        }
    }
    // evalCache.
    let mut hint: Option<(Vec<HintPart>, bool)> = None;
    if !m.unknown(Node::Term(&ref_of(v.r.get(..=v.pos).unwrap_or_default())), v.b) {
        if let (Some(cached), _) = m.vcache_get(&key_text(v.path())) {
            return eval_term(m, f, v.r, v.pos + 1, v.b, &cached, v.b, v.rterm, v.rb, k);
        }
        let nextp = v.r.get(v.pos + 1).map(|t| m.plug(t, v.b));
        if nextp.as_ref().is_some_and(is_var) {
            let result = all_rules_no_cache(m, f, v, empty)?;
            m.vcache_put(key_text(v.path()), Some(result.clone()));
            return eval_term(m, f, v.r, v.pos + 1, v.b, &result, v.b, v.rterm, v.rb, k);
        }
        let mx = max_ref_length(&ir.rules, v.r.len());
        let mut parts: Vec<HintPart> = Vec::new();
        let mut scoping = false;
        let mut end = 0;
        for i in v.pos + 1..mx {
            let Some(t) = v.r.get(i) else { break };
            let p = m.plug(t, v.b);
            if p.is_ground() && !scoping {
                end = i;
                parts = v
                    .plugged
                    .get(..i)
                    .unwrap_or_default()
                    .iter()
                    .cloned()
                    .map(HintPart::Term)
                    .collect();
                parts.push(HintPart::Term(p));
            } else {
                scoping = true;
                if parts.is_empty() {
                    break;
                }
                match parts.last_mut() {
                    Some(HintPart::Scope(s)) => s.push(p),
                    _ => parts.push(HintPart::Scope(vec![p])),
                }
            }
            if let (Some(cached), _) = m.vcache_get(&hint_key(&parts)) {
                return eval_term(m, f, v.r, end + 1, v.b, &cached, v.b, v.rterm, v.rb, k);
            }
        }
        if let Some(HintPart::Scope(s)) = parts.last_mut() {
            while s.last().is_some_and(is_var) {
                s.pop();
            }
            if s.is_empty() {
                parts.pop();
            }
        }
        if !parts.is_empty() {
            hint = Some((parts, false));
        }
    }
    let mut result = empty;
    let mut visited: Vec<Vec<Term>> = Vec::new();
    for rule in &ir.rules {
        result = partial_one_rule_pre_unify(m, f, v, rule, result, unknown, &mut visited, k)?;
    }
    if let Some((parts, _)) = hint {
        let without_scope: Vec<Term> = parts
            .iter()
            .filter_map(|p| match p {
                HintPart::Term(t) => Some(t.clone()),
                HintPart::Scope(_) => None,
            })
            .collect();
        let rest = without_scope.get(v.pos + 1..).unwrap_or_default();
        if let Some(found) = find_in(&result, rest) {
            m.vcache_put(hint_key(&parts), Some(found));
        }
    }
    if !unknown {
        return eval_term(m, f, v.r, v.pos + 1, v.b, &result, v.b, v.rterm, v.rb, k);
    }
    Ok(())
}

/// Value.Find: the value at a path of keys.
fn find_in(t: &Term, path: &[Term]) -> Option<Term> {
    let Some((first, rest)) = path.split_first() else {
        return Some(t.clone());
    };
    let next = match &t.value {
        TermValue::Object(o) => o.iter().find(|(k, _)| k.equal(first)).map(|(_, v)| v.clone()),
        TermValue::Set(s) => s.iter().find(|x| x.equal(first)).cloned(),
        TermValue::Array(a) => match &first.value {
            TermValue::Number(n) => n
                .as_i64()
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| a.get(i).cloned()),
            _ => None,
        },
        _ => None,
    }?;
    find_in(&next, rest)
}

/// wrapInObjects.
fn wrap_in_objects(leaf: Term, r: &[Term]) -> Term {
    let Some((key, rest)) = r.split_first() else {
        return leaf;
    };
    crate::ast::object_term(vec![(key.clone(), wrap_in_objects(leaf, rest))], None)
}

/// evalVirtualPartial.evalOneRulePreUnify.
#[allow(clippy::too_many_arguments)]
fn partial_one_rule_pre_unify(
    m: &mut Machine<'_>,
    f: &Frame,
    v: &Virtual<'_>,
    rule: &Rc<RuleRec>,
    result: Term,
    unknown: bool,
    visited: &mut Vec<Vec<Term>>,
    k: K<'_>,
) -> Result<Term, Flow> {
    let cf = child(m, f, rule.rule.body.clone());
    let head_key = rule
        .rule
        .head
        .key
        .clone()
        .or_else(|| rule.rule.head.reference.last().cloned())
        .unwrap_or_else(|| var_term("_"));
    let mut res = Some(result);
    unify_rule_head(m, f, v.pos + 1, v.r, rule, v.b, cf.b, &mut |m, _| {
        eval_expr(m, &cf, &mut |m, cf| {
            let term = rule.rule.head.value.clone().unwrap_or_else(|| head_key.clone());
            if unknown {
                let (t, tb) = m.apply(&term, cf.b);
                let t = if rule.rule.head.kind() == RuleKind::MultiValue {
                    set_of(vec![t])
                } else {
                    t
                };
                let obj_ref = rule.path.get(v.pos + 1..).unwrap_or_default();
                let t = wrap_in_objects(t, obj_ref);
                eval_term(m, f, v.r, v.pos + 1, v.b, &t, tb, v.rterm, v.rb, k)
            } else {
                let cur = res.take().unwrap_or_else(|| set_of(Vec::new()));
                let (next_v, _) = reduce(m, v.pos, rule, cf.b, cur, visited)?;
                res = Some(next_v);
                Ok(())
            }
        })
    })?;
    Ok(res.unwrap_or_else(|| set_of(Vec::new())))
}

/// evalVirtualPartial.evalOneRulePostUnify.
fn partial_one_rule_post_unify(
    m: &mut Machine<'_>,
    f: &Frame,
    v: &Virtual<'_>,
    rule: &Rc<RuleRec>,
    k: K<'_>,
) -> R {
    let cf = child(m, f, rule.rule.body.clone());
    eval_expr(m, &cf, &mut |m, cf| {
        let cb = cf.b;
        unify_rule_head(m, f, v.pos + 1, v.r, rule, v.b, cb, &mut |m, _| {
            let term = rule
                .rule
                .head
                .value
                .clone()
                .or_else(|| rule.rule.head.key.clone())
                .unwrap_or_else(|| Term::boolean(true, None));
            let (t, tb) = m.apply(&term, cb);
            let t = if rule.rule.head.kind() == RuleKind::MultiValue {
                set_of(vec![t])
            } else {
                t
            };
            let obj_ref = rule.path.get(v.pos + 1..).unwrap_or_default();
            let t = wrap_in_objects(t, obj_ref);
            eval_term(m, f, v.r, v.pos + 1, v.b, &t, tb, v.rterm, v.rb, k)
        })
    })
}

/// evalVirtualPartial.partialEvalSupport.
fn partial_support(m: &mut Machine<'_>, f: &Frame, v: &Virtual<'_>, empty: &Term, k: K<'_>) -> R {
    let path = m.namespace_ref(v.path());
    let mut term = ref_of(&m.namespace_ref(v.r));
    let mut defined = m.support_exists(&path);
    if !defined {
        for rule in &v.ir.rules {
            if partial_support_rule(m, f, rule)? {
                defined = true;
            }
        }
    }
    if !defined {
        if v.r.len() != v.pos + 1 {
            return Ok(());
        }
        term = empty.clone();
    }
    save_unify(m, f, term, v.rterm.clone(), v.b, v.rb, k)
}

/// evalVirtualPartial.partialEvalSupportRule.
fn partial_support_rule(m: &mut Machine<'_>, f: &Frame, rule: &Rc<RuleRec>) -> Result<bool, Flow> {
    let cf = child(m, f, rule.rule.body.clone());
    m.stack_push_query(Vec::new());
    let caller = m.caller;
    let p = m.p;
    let arity = |r: &[Term]| p.arity(r);
    let mut defined = false;
    let r = eval_expr(m, &cf, &mut |m, cf| {
        defined = true;
        let current = m.stack_pop_query();
        let mut plugged = m.query_plug(&current, caller);
        if m.passes_type_check(&plugged) {
            let h = &rule.rule.head;
            let value = h.value.as_ref().map(|x| m.plug_ns(x, cf.b, Some(caller)));
            let mut path = m.namespace_ref(&rule.path);
            for t in path.iter_mut().skip(1) {
                *t = m.plug_ns(t, cf.b, Some(caller));
            }
            let (pkg, rule_ref) = split_package_and_rule(&path);
            let mut head = Head::reference(rule_ref.clone(), value);
            if let Some(key) = &h.key
                && h.kind() == RuleKind::MultiValue
            {
                head.key = Some(m.plug_ns(key, cf.b, Some(caller)));
            }
            if h.kind() == RuleKind::SingleValue && rule_ref.len() == 2 {
                head.key = rule_ref.last().cloned();
            }
            if head.name.is_none()
                && (rule_ref.len() == 1 || (rule_ref.len() == 2 && h.kind() == RuleKind::SingleValue))
            {
                head.name = rule_ref.first().and_then(Term::as_var).map(Rc::from);
            }
            if !m.shallow {
                let live = head_vars(&head);
                plugged = CopyPropagator::new(live, true, &arity).apply(&plugged);
            }
            m.support_insert_by_pkg(
                pkg,
                Rule {
                    default: rule.rule.default,
                    head,
                    body: plugged,
                    else_: None,
                    loc: None,
                    generated_body: false,
                },
            );
        }
        m.stack_push_query(current);
        Ok(())
    });
    m.stack_pop_query();
    r?;
    Ok(defined)
}

/// biunifyRuleHead.
#[allow(clippy::too_many_arguments)]
fn unify_rule_head(
    m: &mut Machine<'_>,
    f: &Frame,
    pos: usize,
    r: &[Term],
    rule: &Rc<RuleRec>,
    rb: usize,
    cb: usize,
    k: &mut dyn FnMut(&mut Machine<'_>, usize) -> R,
) -> R {
    let path = rule.path.clone();
    unify_dynamic_ref(m, f, pos, r, &path, rb, cb, &mut |m, p| {
        if rule.rule.head.kind() == RuleKind::MultiValue && p < r.len() && path.len() <= r.len() {
            let head_key = rule
                .rule
                .head
                .key
                .clone()
                .or_else(|| rule.rule.head.reference.last().cloned())
                .unwrap_or_else(|| var_term("_"));
            let Some(rp) = r.get(p) else { return k(m, p) };
            return unify(m, f, rp, &head_key, rb, cb, &mut |m| k(m, p + 1));
        }
        k(m, p)
    })
}

#[allow(clippy::too_many_arguments)]
fn unify_dynamic_ref(
    m: &mut Machine<'_>,
    f: &Frame,
    pos: usize,
    a: &[Term],
    b: &[Term],
    b1: usize,
    b2: usize,
    k: &mut dyn FnMut(&mut Machine<'_>, usize) -> R,
) -> R {
    let (Some(x), Some(y)) = (a.get(pos), b.get(pos)) else {
        return k(m, pos);
    };
    unify(m, f, x, y, b1, b2, &mut |m| {
        unify_dynamic_ref(m, f, pos + 1, a, b, b1, b2, k)
    })
}

/// evalVirtualPartial.reduce: one rule's result added to the document.
fn reduce(
    m: &Machine<'_>,
    pos: usize,
    rule: &Rc<RuleRec>,
    b: usize,
    result: Term,
    visited: &mut Vec<Vec<Term>>,
) -> Result<(Term, bool), Flow> {
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
            let collision: Vec<Term> = full
                .get(pos + 1..)
                .unwrap_or_default()
                .iter()
                .map(|t| m.plug(t, b))
                .collect();
            if visited
                .iter()
                .any(|c| ref_has_prefix(&collision, c) && !(c.len() == collision.len()))
            {
                return Err(err(CONFLICT_ERR, loc, "object keys must be unique"));
            }
            visited.push(collision);
            let obj_path: Vec<Term> = full
                .get(pos + 1..full.len().saturating_sub(1))
                .unwrap_or_default()
                .iter()
                .map(|t| m.plug(t, b))
                .collect();
            let leaf_key = full
                .last()
                .map(|t| m.plug(t, b))
                .unwrap_or_else(|| Term::boolean(true, None));
            let leaf = if head.kind() == RuleKind::SingleValue {
                Leaf::Value(m.plug(head.value.as_ref().unwrap_or(&Term::boolean(true, None)), b))
            } else {
                Leaf::SetMember(m.plug(head.key.as_ref().unwrap_or(&Term::boolean(true, None)), b))
            };
            let mut exists = false;
            let new = insert_nested(
                Term::new(TermValue::Object(o), None),
                &obj_path,
                &leaf_key,
                &leaf,
                &mut exists,
                &loc,
            )?;
            Ok((new, exists))
        }
        _ => Ok((result, false)),
    }
}

enum Leaf {
    Value(Term),
    SetMember(Term),
}

fn insert_nested(
    obj: Term,
    path: &[Term],
    leaf_key: &Term,
    leaf: &Leaf,
    exists: &mut bool,
    loc: &Option<Location>,
) -> Result<Term, Flow> {
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
        Leaf::Value(v) => match pos.and_then(|i| o.get(i)) {
            Some((_, cur)) => {
                if !cur.equal(v) {
                    return Err(err(CONFLICT_ERR, loc.clone(), "object keys must be unique"));
                }
                *exists = true;
            }
            None => o.push((leaf_key.clone(), v.clone())),
        },
        Leaf::SetMember(v) => match pos.and_then(|i| o.get_mut(i)) {
            Some((_, cur)) => {
                let TermValue::Set(s) = &mut cur.value else {
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

/// buildComprehensionCache: an indexed comprehension's values for the current keys,
/// its body evaluated once for every key.
fn compr_cached(m: &mut Machine<'_>, f: &Frame, a: &Term) -> Result<Option<Term>, Flow> {
    let ck = compr_key(a);
    let Some(keys) = m.p.compr_index.get(&ck).cloned() else {
        return Ok(None);
    };
    if m.ccache.last().is_none_or(|c| !c.contains_key(&ck)) {
        let body = match &a.value {
            TermValue::ArrayCompr(_, b) | TermValue::SetCompr(_, b) | TermValue::ObjectCompr(_, _, b) => {
                b.to_vec()
            }
            _ => return Ok(None),
        };
        let cf = child(m, f, body);
        let mut groups: HashMap<String, Term> = HashMap::new();
        eval_expr(m, &cf, &mut |m, cf| {
            let kv: Vec<Term> = keys.iter().map(|x| m.plug(x, cf.b)).collect();
            let gk = Term::new(TermValue::Array(kv.into()), None).to_string();
            let entry = groups.remove(&gk);
            let next = match &a.value {
                TermValue::ArrayCompr(h, _) => {
                    let v = m.plug(h, cf.b);
                    let mut items = match entry.map(|t| t.value) {
                        Some(TermValue::Array(xs)) => xs.into_inner(),
                        _ => Vec::new(),
                    };
                    items.push(v);
                    Term::new(TermValue::Array(items.into()), None)
                }
                TermValue::SetCompr(h, _) => {
                    let v = m.plug(h, cf.b);
                    let mut items = match entry.map(|t| t.value) {
                        Some(TermValue::Set(xs)) => xs.into_inner(),
                        _ => Vec::new(),
                    };
                    if !items.iter().any(|x| x.equal(&v)) {
                        items.push(v);
                    }
                    Term::new(TermValue::Set(items.into()), None)
                }
                TermValue::ObjectCompr(kk, vv, _) => {
                    let key = m.plug(kk, cf.b);
                    let val = m.plug(vv, cf.b);
                    let mut items = match entry.map(|t| t.value) {
                        Some(TermValue::Object(xs)) => xs.into_inner(),
                        _ => Vec::new(),
                    };
                    match items.iter_mut().find(|(x, _)| x.equal(&key)) {
                        Some(slot) => slot.1 = val,
                        None => items.push((key, val)),
                    }
                    Term::new(TermValue::Object(items.into()), None)
                }
                _ => return Ok(()),
            };
            groups.insert(gk, next);
            Ok(())
        })?;
        if let Some(c) = m.ccache.last_mut() {
            c.insert(ck.clone(), groups);
        }
    }
    let kv: Vec<Term> = keys.iter().map(|x| m.plug(x, f.b)).collect();
    let gk = Term::new(TermValue::Array(kv.into()), None).to_string();
    Ok(m.ccache
        .last()
        .and_then(|c| c.get(&ck))
        .and_then(|g| g.get(&gk))
        .cloned())
}

/// biunifyComprehension.
#[allow(clippy::too_many_arguments)]
fn unify_comprehension(
    m: &mut Machine<'_>,
    f: &Frame,
    a: &Term,
    b: &Term,
    b1: usize,
    b2: usize,
    swap: bool,
    k: K<'_>,
) -> R {
    if m.unknown(Node::Term(a), b1) {
        let ca = amend_comprehension(m, a, b1);
        let cb = if is_compr(b) {
            amend_comprehension(m, b, b2)
        } else {
            b.clone()
        };
        return if !swap {
            save_unify(m, f, ca, cb, b1, b2, k)
        } else {
            save_unify(m, f, cb, ca, b2, b1, k)
        };
    }
    if let Some(v) = compr_cached(m, f, a)? {
        return unify(m, f, &v, b, b1, b2, k);
    }
    let value = match &a.value {
        TermValue::ArrayCompr(head, body) => {
            let c = closure(m, f, body.to_vec());
            let c = Frame { b: b1, ..c };
            let mut out = Vec::new();
            eval_expr(m, &c, &mut |m, cf| {
                out.push(m.plug(head, cf.b));
                Ok(())
            })?;
            Term::new(TermValue::Array(out.into()), None)
        }
        TermValue::SetCompr(head, body) => {
            let c = closure(m, f, body.to_vec());
            let c = Frame { b: b1, ..c };
            let mut out: Vec<Term> = Vec::new();
            eval_expr(m, &c, &mut |m, cf| {
                let v = m.plug(head, cf.b);
                if !out.iter().any(|x| x.equal(&v)) {
                    out.push(v);
                }
                Ok(())
            })?;
            Term::new(TermValue::Set(out.into()), None)
        }
        TermValue::ObjectCompr(kk, vv, body) => {
            let c = closure(m, f, body.to_vec());
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
            Term::new(TermValue::Object(out.into()), None)
        }
        _ => return Ok(()),
    };
    unify(m, f, &value, b, b1, b2, k)
}

/// amendComprehension: a comprehension with its bound variables as equalities in its
/// body, its variables namespaced.
fn amend_comprehension(m: &Machine<'_>, a: &Term, b1: usize) -> Term {
    let mut cpy = a.clone();
    let vars = cvars::term_vars(a);
    let caller = m.caller;
    let eqs: Vec<Expr> = m
        .iter_bindings(b1, Some(caller))
        .into_iter()
        .filter(|(k, _)| k.as_var().is_some_and(|v| vars.contains(v)))
        .map(|(k, v)| eq_expr(k, v))
        .collect();
    if let TermValue::ArrayCompr(_, body)
    | TermValue::SetCompr(_, body)
    | TermValue::ObjectCompr(_, _, body) = &mut cpy.value
    {
        for e in eqs {
            copyprop::append(body, e);
        }
    }
    namespace_comprehension(m, &mut cpy, b1, caller);
    cpy
}

/// bindings.Namespace over a comprehension: its variables renamed for these bindings.
fn namespace_comprehension(m: &Machine<'_>, t: &mut Term, b: usize, caller: usize) {
    match &mut t.value {
        TermValue::ArrayCompr(x, body) | TermValue::SetCompr(x, body) => {
            **x = namespace_term(m, x, b, caller);
            namespace_body(m, body, b, caller);
        }
        TermValue::ObjectCompr(k, v, body) => {
            **k = namespace_term(m, k, b, caller);
            **v = namespace_term(m, v, b, caller);
            namespace_body(m, body, b, caller);
        }
        _ => {}
    }
}

fn namespace_body(m: &Machine<'_>, body: &mut Body, b: usize, caller: usize) {
    for e in body.iter_mut() {
        match &mut e.terms {
            ExprTerms::Call(terms) => {
                for t in terms.iter_mut().skip(1) {
                    *t = namespace_term(m, t, b, caller);
                }
            }
            ExprTerms::Term(t) => **t = namespace_term(m, t, b, caller),
            _ => {}
        }
        for w in e.with.iter_mut() {
            w.target = namespace_term(m, &w.target, b, caller);
            w.value = namespace_term(m, &w.value, b, caller);
        }
        let mut nested: Vec<&mut Term> = Vec::new();
        match &mut e.terms {
            ExprTerms::Call(terms) => nested.extend(terms.iter_mut()),
            ExprTerms::Term(t) => nested.push(t),
            _ => {}
        }
        for t in nested {
            namespace_nested(m, t, b, caller);
        }
    }
}

/// The comprehensions inside a term, namespaced as the visitor reaches them.
fn namespace_nested(m: &Machine<'_>, t: &mut Term, b: usize, caller: usize) {
    if is_compr(t) {
        namespace_comprehension(m, t, b, caller);
        return;
    }
    match &mut t.value {
        TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) | TermValue::Set(r) => {
            r.iter_mut().for_each(|x| namespace_nested(m, x, b, caller))
        }
        TermValue::Object(o) => o.iter_mut().for_each(|(k, v)| {
            namespace_nested(m, k, b, caller);
            namespace_nested(m, v, b, caller);
        }),
        _ => {}
    }
}

/// namespacingVisitor.namespaceTerm.
fn namespace_term(m: &Machine<'_>, t: &Term, b: usize, caller: usize) -> Term {
    match &t.value {
        TermValue::Var(v) => {
            let mut out = m.namespace_var(v, b, Some(caller));
            out.loc = t.loc.clone();
            out
        }
        TermValue::Array(a) => {
            if t.is_ground() {
                return t.clone();
            }
            Term::new(
                TermValue::Array(a.iter().map(|x| namespace_term(m, x, b, caller)).collect()),
                t.loc.clone(),
            )
        }
        TermValue::Object(o) => {
            if t.is_ground() {
                return t.clone();
            }
            crate::ast::object_term(
                o.iter()
                    .map(|(k, v)| (namespace_term(m, k, b, caller), namespace_term(m, v, b, caller)))
                    .collect(),
                t.loc.clone(),
            )
        }
        TermValue::Set(s) => {
            if t.is_ground() {
                return t.clone();
            }
            crate::ast::set_term(
                s.iter().map(|x| namespace_term(m, x, b, caller)).collect(),
                t.loc.clone(),
            )
        }
        TermValue::Ref(r) => Term::new(
            TermValue::Ref(r.iter().map(|x| namespace_term(m, x, b, caller)).collect()),
            t.loc.clone(),
        ),
        _ => t.clone(),
    }
}

/// updateFromQuery: a saved expression takes the current expression's `with`s and
/// location.
fn update_from_query(m: &Machine<'_>, f: &Frame, e: &mut Expr) {
    let withs = current(f).map(|x| x.with.clone()).unwrap_or_default();
    e.with = m.update_saved_mocks(&withs);
    e.loc = current_loc(f);
}

/// saveExpr.
fn save_expr(m: &mut Machine<'_>, f: &Frame, mut e: Expr, b: usize, k: K<'_>) -> R {
    update_from_query(m, f, &mut e);
    m.stack_push(e, Some(b), Some(b));
    let r = k(m);
    m.stack_pop();
    r
}

/// saveExprMarkUnknowns.
fn save_expr_mark_unknowns(m: &mut Machine<'_>, f: &Frame, mut e: Expr, b: usize, k: K<'_>) -> R {
    update_from_query(m, f, &mut e);
    let decl_args = decl_args_len(m, f, &e);
    let pairs = save_pairs_from_expr(m, decl_args, &e, b);
    let pops = pairs.len();
    for (t, pb) in pairs {
        m.ss_push(vec![t], Some(pb));
    }
    m.stack_push(e, Some(b), Some(b));
    let r = k(m);
    m.stack_pop();
    for _ in 0..pops {
        m.ss_pop();
    }
    r
}

/// saveUnify.
fn save_unify(m: &mut Machine<'_>, f: &Frame, a: Term, b: Term, b1: usize, b2: usize, k: K<'_>) -> R {
    let mut e = eq_expr(a.clone(), b.clone());
    update_from_query(m, f, &mut e);
    let mut pairs = save_pairs_from_term(m, &a, b1);
    pairs.extend(save_pairs_from_term(m, &b, b2));
    let pops = pairs.len();
    for (t, pb) in pairs {
        m.ss_push(vec![t], Some(pb));
    }
    m.stack_push(e, Some(b1), Some(b2));
    let r = k(m);
    m.stack_pop();
    for _ in 0..pops {
        m.ss_pop();
    }
    r
}

/// saveCall.
fn save_call(m: &mut Machine<'_>, f: &Frame, decl_args: usize, terms: &[Term], k: K<'_>) -> R {
    let mut e = Expr::new(ExprTerms::Call(terms.to_vec()), None);
    update_from_query(m, f, &mut e);
    let mut pops = 0;
    if decl_args + 2 == terms.len()
        && let Some(out) = terms.last()
    {
        let pairs = save_pairs_from_term(m, out, f.b);
        pops = pairs.len();
        for (t, pb) in pairs {
            m.ss_push(vec![t], Some(pb));
        }
    }
    m.stack_push(e, Some(f.b), None);
    let r = k(m);
    m.stack_pop();
    for _ in 0..pops {
        m.ss_pop();
    }
    r
}

/// saveInlinedNegatedExprs.
fn save_inlined_negated(m: &mut Machine<'_>, f: &Frame, exprs: Vec<Expr>, k: K<'_>) -> R {
    let caller = m.caller;
    let withs: Vec<With> = current(f)
        .map(|x| x.with.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|mut w| {
            w.value = m.plug_ns(&w.value, f.b, Some(caller));
            w
        })
        .collect();
    let n = exprs.len();
    for mut e in exprs {
        e.with = m.update_saved_mocks(&withs);
        m.stack_push(e, None, None);
    }
    let r = k(m);
    for _ in 0..n {
        m.stack_pop();
    }
    r
}

/// getDeclArgsLen.
fn decl_args_len(m: &Machine<'_>, f: &Frame, e: &Expr) -> Option<usize> {
    let ExprTerms::Call(c) = &e.terms else { return None };
    let op = c.first()?.as_ref()?;
    let name = crate::compile::text_of_ref(op);
    if let Some(crate::types::Type::Function { args, .. }) = crate::compile::allowed(&name).map(|b| &b.decl) {
        return Some(args.len());
    }
    if m.p.host_names.contains(&name) {
        return Some(host_arity(&name));
    }
    let ir = get_rules(m, f, op, None).ok()?;
    ir.rules.first().map(|r| r.rule.head.args.len())
}

/// getSavePairsFromExpr.
fn save_pairs_from_expr(m: &Machine<'_>, decl_args: Option<usize>, e: &Expr, b: usize) -> Vec<(Term, usize)> {
    match &e.terms {
        ExprTerms::Term(t) => save_pairs_from_term(m, t, b),
        ExprTerms::Call(terms) => {
            if e.is_equality() {
                let mut out = Vec::new();
                if let Some(t) = terms.get(1) {
                    out.extend(save_pairs_from_term(m, t, b));
                }
                if let Some(t) = terms.get(2) {
                    out.extend(save_pairs_from_term(m, t, b));
                }
                return out;
            }
            if decl_args.is_some_and(|n| n + 2 == terms.len())
                && let Some(t) = terms.last()
            {
                return save_pairs_from_term(m, t, b);
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// getSavePairsFromTerm.
fn save_pairs_from_term(m: &Machine<'_>, x: &Term, b: usize) -> Vec<(Term, usize)> {
    if is_var(x) {
        return vec![(x.clone(), b)];
    }
    let mut vis = VarVisitor::new(Params {
        skip_closures: true,
        skip_ref_head: true,
        ..Params::default()
    });
    vis.term(x);
    let mut out = Vec::new();
    for v in vis.vars {
        let (y, next) = m.apply(&var_term(&v), b);
        out.extend(save_pairs_from_term(m, &y, next));
    }
    out
}

/// evalCall.
fn eval_call(m: &mut Machine<'_>, f: &Frame, terms: &[Term], k: K<'_>) -> R {
    let Some(op) = terms.first().and_then(Term::as_ref) else {
        return Ok(());
    };
    let name = crate::compile::text_of_ref(op);
    let loc = current_loc(f);
    let mock = m.mock(&name);
    if let Some(mv) = &mock
        && let Some(r) = mv.as_ref()
        && m.p.is_function(r)
    {
        let mut call = vec![ref_of(r)];
        call.extend(terms.iter().skip(1).cloned());
        m.mocks.push(Vec::new());
        let r = eval_call(m, f, &call, &mut |m| {
            let saved = m.mocks.pop();
            let r = k(m);
            if let Some(s) = saved {
                m.mocks.push(s);
            }
            r
        });
        m.mocks.pop();
        return r;
    }
    if op.first().and_then(Term::as_var) == Some("data") {
        if let Some(mv) = mock {
            let arity = m.p.arity(op).unwrap_or(0);
            return eval_call_value(m, f, arity, terms, &mv, k);
        }
        let args = if m.is_partial() { None } else { terms.get(1..) };
        let ir = get_rules(m, f, op, args)?;
        return eval_func(m, f, terms, &ir, k);
    }
    let decl_arity = if crate::compile::allowed(&name).is_some() || m.p.host_names.contains(&name) {
        m.p.arity(op).unwrap_or(0)
    } else {
        return Err(err(INTERNAL_ERR, loc, format!("unsupported built-in: {name}")));
    };
    if let Some(mv) = mock {
        return eval_call_value(m, f, decl_arity, terms, &mv, k);
    }
    if let Some(e) = current(f).cloned()
        && m.unknown(Node::Expr(&e), f.b)
    {
        return save_call(m, f, decl_arity, terms, k);
    }
    let operands: Vec<Term> = terms.iter().skip(1).map(|t| m.plug(t, f.b)).collect();
    let has_output = operands.len() > decl_arity;
    let ins = operands.get(..decl_arity.min(operands.len())).unwrap_or_default();
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
    // A builtin's error makes its call undefined and is recorded; a halt stops the query
    // (evalBuiltin.eval).
    let result = if m.p.host_names.contains(&name) {
        match m.host.call(&name, &args) {
            Ok(v) => v,
            Err(HostError::Halt(msg)) => return Err(err(HALT, loc, msg)),
            Err(HostError::Undefined(e)) => {
                m.builtin_errors.push(EvalError {
                    code: BUILTIN_ERR,
                    message: format!("{name}: {e}"),
                    loc,
                });
                return Ok(());
            }
        }
    } else {
        let Some(bf) = funcs::lookup(&name) else {
            return Err(err(INTERNAL_ERR, loc, format!("unsupported built-in: {name}")));
        };
        match bf(&mut m.ctx, &args) {
            Ok(v) => v,
            Err(BuiltinError::Halt(msg)) => return Err(err(INTERNAL_ERR, loc, msg)),
            Err(e) => {
                let (code, msg) = match &e {
                    BuiltinError::Operand(msg) => (TYPE_ERR, format!("{name}: {msg}")),
                    BuiltinError::Other(msg) | BuiltinError::Halt(msg) => {
                        (BUILTIN_ERR, format!("{name}: {msg}"))
                    }
                };
                m.builtin_errors.push(EvalError {
                    code,
                    message: msg,
                    loc,
                });
                return Ok(());
            }
        }
    };
    let Some(v) = result else { return Ok(()) };
    let decl_void = matches!(
        crate::compile::allowed(&name).map(|b| &b.decl),
        Some(crate::types::Type::Function { result: None, .. })
    );
    if decl_void {
        return k(m);
    }
    if has_output {
        let Some(out) = terms.last() else { return Ok(()) };
        return unify(m, f, out, &to_term(&v), f.b, f.b, k);
    }
    if matches!(v, Value::Bool(false)) {
        return Ok(());
    }
    k(m)
}

/// evalCallValue.
fn eval_call_value(m: &mut Machine<'_>, f: &Frame, arity: usize, terms: &[Term], mock: &Term, k: K<'_>) -> R {
    if terms.len() == arity + 2 {
        let Some(out) = terms.last() else { return Ok(()) };
        return unify(m, f, out, mock, f.b, f.b, k);
    }
    if matches!(mock.value, TermValue::Bool(false)) {
        return Ok(());
    }
    k(m)
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
    let Some(TermValue::Array(ops)) = ins.first().map(|t| &t.value) else {
        return k(m);
    };
    let mut lines: Vec<Vec<String>> = vec![Vec::new()];
    for op in ops {
        let parts: Vec<String> = match &op.value {
            TermValue::String(s) => vec![s.to_string()],
            TermValue::Number(_) | TermValue::Bool(_) | TermValue::Null => vec![op.to_string()],
            TermValue::Set(s) if s.is_empty() => vec!["<undefined>".to_string()],
            TermValue::Set(s) => {
                let mut items = s.clone();
                items.sort_by(term_compare);
                items
                    .iter()
                    .map(|x| match &x.value {
                        TermValue::String(s) => s.to_string(),
                        _ => x.to_string(),
                    })
                    .collect()
            }
            _ => {
                return Err(err(
                    INTERNAL_ERR,
                    loc,
                    format!("illegal argument type: {}", op.value_name()),
                ));
            }
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
        let line = at.format(&l.join(" "));
        if !m.host.print(&line) {
            m.prints.push(line);
        }
    }
    k(m)
}

/// evalFunc.
fn eval_func(m: &mut Machine<'_>, f: &Frame, terms: &[Term], ir: &IndexResult, k: K<'_>) -> R {
    if ir.empty() {
        return Ok(());
    }
    let arg_count = ir
        .rules
        .first()
        .or(ir.default.as_ref())
        .map_or(0, |r| r.rule.head.args.len());
    if m.is_partial() {
        if !ir.elses.is_empty()
            && let Some(e) = current(f).cloned()
            && m.unknown(Node::Expr(&e), f.b)
        {
            return save_call(m, f, arg_count, terms, k);
        }
        let mut must_support = false;
        if let Some(d) = &ir.default {
            if d.rule.head.args.len() + 1 == terms.len() {
                if d.rule
                    .head
                    .value
                    .as_ref()
                    .is_none_or(|v| !matches!(v.value, TermValue::Bool(false)))
                {
                    must_support = true;
                }
            } else {
                must_support = true;
            }
        }
        let op = terms.first().and_then(Term::as_ref).unwrap_or_default().to_vec();
        if must_support || m.shallow || m.disabled_ref(&op, false) {
            let mut unknown = m.unknown_ref(&op, f.b);
            for i in 1..=arg_count {
                if unknown {
                    break;
                }
                if let Some(t) = terms.get(i) {
                    unknown = m.unknown(Node::Term(t), f.b);
                }
            }
            if unknown {
                return func_support(m, f, terms, ir, arg_count, k);
            }
        }
    }
    func_value(m, f, terms, ir, arg_count, ir.early_exit, k)
}

/// evalFunc.evalValue.
fn func_value(
    m: &mut Machine<'_>,
    f: &Frame,
    terms: &[Term],
    ir: &IndexResult,
    arg_count: usize,
    find_one: bool,
    k: K<'_>,
) -> R {
    let mut key: Option<String> = None;
    if !m.is_partial() {
        let plen = if terms.len() == arg_count + 2 {
            terms.len() - 1
        } else {
            terms.len()
        };
        let key_terms: Vec<Term> = terms
            .iter()
            .take(plen)
            .map(|t| if t.is_ground() { t.clone() } else { m.plug(t, f.b) })
            .collect();
        let kt = format!("fn:{}", key_text(&key_terms));
        if let (Some(cached), _) = m.vcache_get(&kt) {
            if arg_count == terms.len() - 1 {
                if matches!(cached.value, TermValue::Bool(false)) {
                    return Ok(());
                }
                return k(m);
            }
            let Some(out) = terms.last() else { return Ok(()) };
            return unify(m, f, out, &cached, f.b, f.b, k);
        }
        key = Some(kt);
    }
    let mut prev: Option<Term> = None;
    let mut outer: Option<Flow> = None;
    let res = (|| -> R {
        for (i, rule) in ir.rules.iter().enumerate() {
            let (mut next_v, r1) = func_one_rule(m, f, terms, rule, key.as_deref(), &mut prev, find_one, k);
            handle_deferred(r1, &mut outer)?;
            if next_v.is_none() {
                for erule in ir.elses.get(&i).map(Vec::as_slice).unwrap_or_default() {
                    let (nv, r1) = func_one_rule(m, f, terms, erule, key.as_deref(), &mut prev, find_one, k);
                    handle_deferred(r1, &mut outer)?;
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
            let (_, r1) = func_one_rule(m, f, terms, d, key.as_deref(), &mut prev, find_one, k);
            return r1;
        }
        match outer.take() {
            Some(o) => Err(o),
            None => Ok(()),
        }
    })();
    suppress(res)
}

/// evalFunc.evalOneRule.
#[allow(clippy::too_many_arguments)]
fn func_one_rule(
    m: &mut Machine<'_>,
    f: &Frame,
    terms: &[Term],
    rule: &Rc<RuleRec>,
    key: Option<&str>,
    prev: &mut Option<Term>,
    find_one: bool,
    k: K<'_>,
) -> (Option<Term>, R) {
    let mut cf = child(m, f, rule.rule.body.clone());
    cf.find_one = find_one;
    let mut args: Vec<Term> = rule.rule.head.args.clone();
    let value = rule
        .rule
        .head
        .value
        .clone()
        .unwrap_or_else(|| Term::boolean(true, None));
    if terms.len() - 1 == args.len() + 1 {
        args.push(value.clone());
    }
    let ins: Vec<Term> = terms.iter().skip(1).cloned().collect();
    let loc = rule.rule.loc.clone();
    let mut result: Option<Term> = None;
    let nargs = rule.rule.head.args.len();
    let res = unify_terms(m, f, &ins, &args, f.b, cf.b, 0, &mut |m| {
        eval_expr(m, &cf, &mut |m, cf| {
            if nargs == terms.len() - 1 && m.ss_contains(&value, Some(cf.b)) {
                return save_expr(m, f, Expr::term(value.clone()), cf.b, k);
            }
            let v = m.plug(&value, cf.b);
            result = Some(v.clone());
            if let Some(kt) = key {
                m.vcache_put(kt.to_string(), Some(v.clone()));
            }
            if nargs == terms.len() - 1 && matches!(v.value, TermValue::Bool(false)) {
                if prev.as_ref().is_some_and(|p| !p.equal(&v)) {
                    return Err(err(
                        CONFLICT_ERR,
                        loc.clone(),
                        "functions must not produce multiple outputs for same inputs",
                    ));
                }
                *prev = Some(v);
                return Ok(());
            }
            if !m.is_partial()
                && let Some(p) = prev.as_ref()
            {
                if !p.equal(&v) {
                    return Err(err(
                        CONFLICT_ERR,
                        loc.clone(),
                        "functions must not produce multiple outputs for same inputs",
                    ));
                }
                return Ok(());
            }
            *prev = Some(v);
            k(m)
        })
    });
    (result, res)
}

/// evalFunc.partialEvalSupport.
fn func_support(
    m: &mut Machine<'_>,
    f: &Frame,
    terms: &[Term],
    ir: &IndexResult,
    decl_args: usize,
    k: K<'_>,
) -> R {
    let op = terms.first().and_then(Term::as_ref).unwrap_or_default().to_vec();
    let path = m.namespace_ref(&op);
    if !m.support_exists(&path) {
        let rules: Vec<Rc<RuleRec>> = ir
            .rules
            .iter()
            .cloned()
            .chain(ir.default.iter().cloned())
            .collect();
        for rule in rules {
            func_support_rule(m, f, &rule, &path)?;
        }
    }
    if !m.support_exists(&path) {
        return Ok(());
    }
    let mut call = vec![ref_of(&path)];
    call.extend(terms.iter().skip(1).cloned());
    save_call(m, f, decl_args, &call, k)
}

/// evalFunc.partialEvalSupportRule.
fn func_support_rule(m: &mut Machine<'_>, f: &Frame, rule: &Rc<RuleRec>, path: &[Term]) -> R {
    let cf = child(m, f, rule.rule.body.clone());
    m.stack_push_query(Vec::new());
    let mut args: Vec<Term> = Vec::new();
    for a in &rule.rule.head.args {
        crate::compile::safety::walk_terms(a, &mut |t: &Term| {
            if let TermValue::Var(v) = &t.value {
                args.push(var_term(v));
            }
            false
        });
    }
    m.ss_push(args, Some(cf.b));
    let caller = m.caller;
    let r = eval_expr(m, &cf, &mut |m, cf| {
        let current = m.stack_pop_query();
        let plugged = m.query_plug(&current, caller);
        if m.passes_type_check(&plugged) {
            let h = &rule.rule.head;
            let head = Head {
                name: h.name.clone(),
                reference: h.reference.clone(),
                value: h.value.as_ref().map(|v| m.plug_ns(v, cf.b, Some(caller))),
                args: h.args.iter().map(|a| m.plug_ns(a, cf.b, Some(caller))).collect(),
                ..Head::default()
            };
            m.support_insert(
                path,
                Rule {
                    default: rule.rule.default,
                    head,
                    body: plugged,
                    else_: None,
                    loc: None,
                    generated_body: false,
                },
            );
        }
        m.stack_push_query(current);
        Ok(())
    });
    m.ss_pop();
    m.stack_pop_query();
    r
}

#[allow(clippy::too_many_arguments)]
fn unify_terms(
    m: &mut Machine<'_>,
    f: &Frame,
    a: &[Term],
    b: &[Term],
    b1: usize,
    b2: usize,
    i: usize,
    k: K<'_>,
) -> R {
    if a.len() != b.len() {
        return Ok(());
    }
    let (Some(x), Some(y)) = (a.get(i), b.get(i)) else {
        return k(m);
    };
    unify(m, f, x, y, b1, b2, &mut |m| {
        unify_terms(m, f, a, b, b1, b2, i + 1, k)
    })
}

//! OPA's copy propagation (topdown/copypropagation, v1.14.1): the queries partial
//! evaluation saves, with the variables that only copy others or name refs replaced by
//! what they copy, as OPA rewrites them.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{Body, Expr, ExprTerms, Term, TermValue};
use crate::compare::term_compare;
use crate::compile::safety::{Arity, output_vars_for_body, output_vars_for_expr, walk_terms};
use crate::compile::transform::{self, Transformer};
use crate::compile::vars::{self, Params, SAFETY, Var, VarSet, VarVisitor};

/// ast.IsConstant: no variables, refs, closures or calls anywhere.
pub fn is_constant(t: &Term) -> bool {
    match &t.value {
        TermValue::Null | TermValue::Bool(_) | TermValue::Number(_) | TermValue::String(_) => true,
        TermValue::Var(_)
        | TermValue::Ref(_)
        | TermValue::ArrayCompr(..)
        | TermValue::SetCompr(..)
        | TermValue::ObjectCompr(..)
        | TermValue::Call(_)
        | TermValue::TemplateString { .. } => false,
        TermValue::Array(a) | TermValue::Set(a) => a.iter().all(is_constant),
        TermValue::Object(o) => o.iter().all(|(k, v)| is_constant(k) && is_constant(v)),
    }
}

/// Ref.HasPrefix.
pub fn ref_has_prefix(r: &[Term], prefix: &[Term]) -> bool {
    prefix.len() <= r.len() && r.iter().zip(prefix).all(|(a, b)| a.equal(b))
}

/// Equality.Expr(a, b).
pub fn eq_expr(a: Term, b: Term) -> Expr {
    Expr::new(
        ExprTerms::Call(vec![crate::compile::localvars::op("eq"), a, b]),
        None,
    )
}

/// Body.Append: the expression's index is its place.
pub fn append(body: &mut Body, mut e: Expr) {
    e.index = body.len();
    body.push(e);
}

#[derive(Debug, Clone)]
struct Root {
    key: Var,
    constant: Option<Term>,
}

impl Root {
    fn value(&self) -> TermValue {
        match &self.constant {
            Some(c) => c.value.clone(),
            None => TermValue::Var(self.key.clone()),
        }
    }
}

/// The union-find over variables (unionfind.go), ranked to keep live variables roots.
#[derive(Debug, Default)]
struct UnionFind {
    roots: HashMap<Var, Root>,
    parents: HashMap<Var, Var>,
}

impl UnionFind {
    fn find(&self, v: &Var) -> Option<Var> {
        let mut v = v.clone();
        loop {
            let parent = self.parents.get(&v)?;
            if *parent == v {
                return self.roots.contains_key(&v).then_some(v);
            }
            v = parent.clone();
        }
    }

    fn root(&self, v: &Var) -> Option<&Root> {
        self.find(v).and_then(|k| self.roots.get(&k))
    }

    fn make_set(&mut self, v: &Var) -> Var {
        if let Some(r) = self.find(v) {
            return r;
        }
        self.parents.insert(v.clone(), v.clone());
        self.roots.insert(
            v.clone(),
            Root {
                key: v.clone(),
                constant: None,
            },
        );
        v.clone()
    }

    fn merge(&mut self, live: &VarSet, a: &Var, b: &Var) -> bool {
        let r1 = self.make_set(a);
        let r2 = self.make_set(b);
        if r1 == r2 {
            return true;
        }
        let (r1, r2) = if live.contains(&r1) { (r1, r2) } else { (r2, r1) };
        self.parents.insert(r2.clone(), r1.clone());
        let Some(gone) = self.roots.remove(&r2) else {
            return true;
        };
        let Some(keep) = self.roots.get_mut(&r1) else {
            return true;
        };
        match (&keep.constant, &gone.constant) {
            (Some(x), Some(y)) if !x.equal(y) => return false,
            (None, _) => keep.constant = gone.constant,
            _ => {}
        }
        true
    }
}

/// makeDisjointSets.
fn disjoint_sets(live: &VarSet, query: &Body) -> Option<UnionFind> {
    let mut uf = UnionFind::default();
    for e in query {
        if !e.is_equality() || e.negated || !e.with.is_empty() {
            continue;
        }
        let (Some(a), Some(b)) = (e.operand(0), e.operand(1)) else {
            continue;
        };
        match (&a.value, &b.value) {
            (TermValue::Var(x), TermValue::Var(y)) => {
                if !uf.merge(live, x, y) {
                    return None;
                }
            }
            (TermValue::Var(x), _) if is_constant(b) => set_constant(&mut uf, x, b)?,
            (_, TermValue::Var(y)) if is_constant(a) => set_constant(&mut uf, y, a)?,
            _ => {}
        }
    }
    Some(uf)
}

fn set_constant(uf: &mut UnionFind, v: &Var, c: &Term) -> Option<()> {
    let r = uf.make_set(v);
    let root = uf.roots.get_mut(&r)?;
    if let Some(existing) = &root.constant
        && !existing.equal(c)
    {
        return None;
    }
    root.constant = Some(c.clone());
    Some(())
}

/// The bindings copy propagation removed: variable to value, a ValueMap.
type Removed = Vec<(Term, TermValue)>;

fn removed_get<'a>(r: &'a Removed, k: &TermValue) -> Option<&'a TermValue> {
    let key = Term::new(k.clone(), None);
    r.iter().find(|(x, _)| x.equal(&key)).map(|(_, v)| v)
}

fn removed_put(r: &mut Removed, k: TermValue, v: TermValue) {
    let key = Term::new(k, None);
    match r.iter_mut().find(|(x, _)| x.equal(&key)) {
        Some(slot) => slot.1 = v,
        None => r.push((key, v)),
    }
}

fn value_ground(v: &TermValue) -> bool {
    Term::new(v.clone(), None).is_ground()
}

struct Plug<'a> {
    removed: &'a Removed,
    uf: &'a UnionFind,
    negated: bool,
}

impl Plug<'_> {
    /// plugBindingsVar.
    fn var(&self, v: &Var) -> TermValue {
        let result = match self.uf.root(v) {
            Some(r) => r.value(),
            None => TermValue::Var(v.clone()),
        };
        let TermValue::Var(rv) = &result else {
            return result;
        };
        let Some(b) = removed_get(self.removed, &TermValue::Var(rv.clone())) else {
            return result;
        };
        if self.negated && !value_ground(b) {
            return result;
        }
        if let TermValue::Ref(r) = b {
            let mut vis = VarVisitor::new(Params {
                skip_ref_head: true,
                ..Params::default()
            });
            vis.reference(r);
            if vis.vars.contains(rv) {
                return result;
            }
        }
        b.clone()
    }

    /// plugBindingsRef.
    fn reference(&self, r: &[Term]) -> Vec<Term> {
        let mut v = r.to_vec();
        if let Some(head) = v.first_mut()
            && let TermValue::Var(h) = &head.value
            && let Some(root) = self.uf.root(h)
        {
            head.value = root.value();
        }
        let Some(head) = v.first() else { return v };
        if let Some(b) = removed_get(self.removed, &head.value)
            && (!self.negated || value_ground(b))
        {
            let mut base = match b {
                TermValue::Ref(x) => x.to_vec(),
                other => vec![Term::new(other.clone(), None)],
            };
            base.extend(v.iter().skip(1).cloned());
            return base;
        }
        v
    }
}

impl Transformer for Plug<'_> {
    fn term(&mut self, t: &mut Term) {
        let next = match &t.value {
            TermValue::Var(v) => Some(self.var(v)),
            TermValue::Ref(r) => Some(TermValue::Ref(self.reference(r).into())),
            _ => None,
        };
        if let Some(n) = next {
            t.value = n;
        }
    }
}

/// isNoop.
fn is_noop(e: &Expr) -> bool {
    match &e.terms {
        ExprTerms::Term(t) => is_constant(t) && !matches!(t.value, TermValue::Bool(false)),
        ExprTerms::Call(_) => match (e.operator_name().as_deref(), e.operand(0), e.operand(1)) {
            (Some("equal"), Some(a), Some(b)) => a.equal(b),
            _ => false,
        },
        _ => false,
    }
}

/// containedIn: a value anywhere in a body, closures' bodies and `every` bodies aside.
fn contained_in(value: &TermValue, body: &Body) -> bool {
    let target = Term::new(value.clone(), None);
    let mut stop = false;
    let mut f = |x: &Term| -> bool {
        if stop {
            return true;
        }
        if matches!(
            x.value,
            TermValue::ArrayCompr(..) | TermValue::SetCompr(..) | TermValue::ObjectCompr(..)
        ) {
            return true;
        }
        let hit = match (&x.value, value) {
            (TermValue::Ref(r), TermValue::Ref(p)) => ref_has_prefix(r, p),
            _ => x.equal(&target),
        };
        if hit {
            stop = true;
        }
        stop
    };
    for e in body {
        match &e.terms {
            ExprTerms::Every(ev) => {
                if let Some(k) = &ev.key {
                    walk_terms(k, &mut f);
                }
                walk_terms(&ev.value, &mut f);
                walk_terms(&ev.domain, &mut f);
            }
            ExprTerms::Term(t) => walk_terms(t, &mut f),
            ExprTerms::Call(c) => c.iter().for_each(|t| walk_terms(t, &mut f)),
            ExprTerms::Some(d) => d.symbols.iter().for_each(|t| walk_terms(t, &mut f)),
        }
        for w in &e.with {
            walk_terms(&w.target, &mut f);
            walk_terms(&w.value, &mut f);
        }
    }
    stop
}

/// CopyPropagator.
#[allow(missing_debug_implementations)]
pub struct CopyPropagator<'a> {
    live: VarSet,
    ensure_non_empty: bool,
    arity: Arity<'a>,
    next: usize,
}

impl<'a> CopyPropagator<'a> {
    pub fn new(live: VarSet, ensure_non_empty: bool, arity: Arity<'a>) -> CopyPropagator<'a> {
        CopyPropagator {
            live,
            ensure_non_empty,
            arity,
            next: 0,
        }
    }

    fn generate(&mut self) -> Var {
        let v: Var = Rc::from(format!("__localcp{}__", self.next));
        self.next += 1;
        v
    }

    fn live_ref(&self, a: &Term) -> bool {
        match &a.value {
            TermValue::Ref(r) => r
                .first()
                .and_then(Term::as_var)
                .is_some_and(|h| self.live.contains(h)),
            _ => false,
        }
    }

    /// updateBindingsEqAsymmetric.
    fn eq_asym(&self, a: &Term, b: &Term) -> Option<(Var, TermValue)> {
        let TermValue::Var(k) = &a.value else { return None };
        if self.live.contains(k) {
            return None;
        }
        matches!(b.value, TermValue::Ref(_) | TermValue::Call(_)).then(|| (k.clone(), b.value.clone()))
    }

    /// updateBindings: whether the expression stays.
    fn update(&mut self, removed: &mut Removed, headvars: &VarSet, negated: bool, e: &Expr) -> bool {
        if negated || !e.with.is_empty() {
            return true;
        }
        if e.is_equality() {
            let (Some(a), Some(b)) = (e.operand(0), e.operand(1)) else {
                return !is_noop(e);
            };
            if a.equal(b) {
                if self.live_ref(a) {
                    let k = self.generate();
                    removed_put(removed, TermValue::Var(k), a.value.clone());
                }
                return false;
            }
            if let Some((k, v)) = self.eq_asym(a, b).or_else(|| self.eq_asym(b, a)) {
                removed_put(removed, TermValue::Var(k), v);
                return false;
            }
        } else if let ExprTerms::Call(terms) = &e.terms
            && let Some(op) = terms.first().and_then(Term::as_ref)
            && (self.arity)(op).is_some_and(|n| n + 2 == terms.len())
            && let Some(out) = terms.last()
            && let TermValue::Var(k) = &out.value
            && !self.live.contains(k)
            && !headvars.contains(k)
        {
            let call = terms.get(..terms.len() - 1).unwrap_or_default().to_vec();
            removed_put(removed, TermValue::Var(k.clone()), TermValue::Call(call.into()));
            return false;
        }
        !is_noop(e)
    }

    /// Apply.
    pub fn apply(&mut self, query: &Body) -> Body {
        let Some(mut uf) = disjoint_sets(&self.live, query) else {
            return query.clone();
        };
        let mut headvars = VarSet::new();
        for e in query {
            walk_refs_expr(e, &mut |r: &[Term]| {
                if let Some(TermValue::Var(v)) = r.first().map(|t| &t.value) {
                    match uf.find(v) {
                        Some(k) => {
                            if let Some(root) = uf.roots.get_mut(&k) {
                                root.constant = None;
                            }
                            headvars.insert(k);
                        }
                        None => {
                            headvars.insert(v.clone());
                        }
                    }
                }
            });
        }
        let mut removed: Removed = Vec::new();
        let mut result: Body = Vec::new();
        for e in query {
            let mut x = e.clone();
            {
                let mut p = Plug {
                    removed: &removed,
                    uf: &uf,
                    negated: e.negated,
                };
                transform::expr(&mut p, &mut x);
            }
            if self.update(&mut removed, &headvars, e.negated, &x) {
                append(&mut result, x);
            }
        }
        let mut sorted: Vec<&Var> = self.live.iter().collect();
        sorted.sort();
        for v in sorted {
            let Some(root) = uf.root(v) else { continue };
            let vt = Term::var(v, None);
            if let Some(c) = &root.constant {
                append(&mut result, eq_expr(vt, c.clone()));
            } else if let Some(b) = removed_get(&removed, &TermValue::Var(root.key.clone())) {
                append(&mut result, eq_expr(vt, Term::new(b.clone(), None)));
            } else if root.key != *v {
                append(&mut result, eq_expr(vt, Term::var(&root.key, None)));
            }
        }
        let arity = self.arity;
        let mut safe: VarSet = ["data", "input"].iter().map(|v| Rc::from(*v)).collect();
        safe.extend(self.live.iter().cloned());
        let out = output_vars_for_body(&result, arity, &safe);
        safe.extend(out);
        let mut unsafe_vars: VarSet = vars::body_vars(&result, SAFETY)
            .difference(&safe)
            .cloned()
            .collect();
        let mut bindings = removed.clone();
        bindings.sort_by(|a, b| term_compare(&b.0, &a.0));
        for (k, v) in bindings {
            let eq = eq_expr(k, Term::new(v.clone(), None));
            let outputs = output_vars_for_expr(&eq, arity, &safe);
            let before = unsafe_vars.len();
            let remaining: VarSet = unsafe_vars.difference(&outputs).cloned().collect();
            let provides = remaining.len() < before;
            if provides {
                unsafe_vars = remaining;
            }
            let safe_var_ref = match &v {
                TermValue::Ref(r) if r.len() == 1 => {
                    r.first().and_then(Term::as_var).is_some_and(|h| safe.contains(h))
                }
                _ => false,
            };
            if provides || (!safe_var_ref && !contained_in(&v, &result)) {
                append(&mut result, eq);
                safe.extend(outputs);
            }
        }
        if !unsafe_vars.is_empty() {
            return query.clone();
        }
        if self.ensure_non_empty && result.is_empty() {
            append(&mut result, Expr::term(Term::boolean(true, None)));
        }
        result
    }
}

/// WalkRefs over an expression: every ref, closures and nested refs included.
pub fn walk_refs_expr(e: &Expr, f: &mut dyn FnMut(&[Term])) {
    let mut g = |t: &Term| -> bool {
        if let TermValue::Ref(r) = &t.value {
            f(r);
        }
        false
    };
    crate::compile::safety::walk_terms_expr(e, &mut g);
}

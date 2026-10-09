//! OPA's `VarVisitor` (ast/visit.go): the variables of a node, with the parameters that
//! skip parts of it, walked in OPA's order (object and set members sorted).

use std::collections::BTreeSet;
use std::rc::Rc;

use crate::ast::{Body, Expr, ExprTerms, Head, Rule, TemplatePart, Term, TermValue, With};
use crate::compare::term_compare;

pub type Var = Rc<str>;
pub type VarSet = BTreeSet<Var>;

/// OPA's VarVisitorParams.
#[derive(Debug, Clone, Copy, Default)]
pub struct Params {
    pub skip_ref_head: bool,
    pub skip_ref_call_head: bool,
    pub skip_object_keys: bool,
    pub skip_closures: bool,
    pub skip_with_target: bool,
    pub skip_sets: bool,
    pub skip_template_strings: bool,
}

/// SafetyCheckVisitorParams.
pub const SAFETY: Params = Params {
    skip_ref_head: false,
    skip_ref_call_head: true,
    skip_object_keys: false,
    skip_closures: true,
    skip_with_target: false,
    skip_sets: false,
    skip_template_strings: false,
};

/// The parameters outputVarsForExprCall and Unify walk with.
pub const OUTPUT: Params = Params {
    skip_ref_head: true,
    skip_ref_call_head: false,
    skip_object_keys: true,
    skip_closures: true,
    skip_with_target: false,
    skip_sets: true,
    skip_template_strings: false,
};

/// Object pairs in OPA's iteration order (sorted keys).
pub fn sorted_pairs(o: &[(Term, Term)]) -> Vec<&(Term, Term)> {
    let mut v: Vec<&(Term, Term)> = o.iter().collect();
    v.sort_by(|a, b| term_compare(&a.0, &b.0));
    v
}

/// Set members in OPA's iteration order.
pub fn sorted_items(s: &[Term]) -> Vec<&Term> {
    let mut v: Vec<&Term> = s.iter().collect();
    v.sort_by(|a, b| term_compare(a, b));
    v
}

#[derive(Debug, Default)]
pub struct VarVisitor {
    pub params: Params,
    pub vars: VarSet,
}

impl VarVisitor {
    pub fn new(params: Params) -> VarVisitor {
        VarVisitor {
            params,
            vars: VarSet::new(),
        }
    }

    pub fn add(&mut self, v: &Var) {
        self.vars.insert(v.clone());
    }

    pub fn term(&mut self, t: &Term) {
        self.value(&t.value);
    }

    pub fn value(&mut self, v: &TermValue) {
        let p = self.params;
        match v {
            TermValue::Object(o) if p.skip_object_keys => {
                for (_, val) in sorted_pairs(o) {
                    self.term(val);
                }
                return;
            }
            TermValue::Ref(r) if p.skip_ref_head => {
                for t in r.iter().skip(1) {
                    self.term(t);
                }
                return;
            }
            TermValue::ArrayCompr(..)
            | TermValue::SetCompr(..)
            | TermValue::ObjectCompr(..)
            | TermValue::TemplateString { .. }
                if p.skip_closures =>
            {
                return;
            }
            TermValue::Set(_) if p.skip_sets => return,
            TermValue::Call(c) if p.skip_ref_call_head => {
                if let Some(op) = c.first().and_then(Term::as_ref) {
                    for t in op.iter().skip(1) {
                        self.term(t);
                    }
                }
                for t in c.iter().skip(1) {
                    self.term(t);
                }
                return;
            }
            TermValue::TemplateString { .. } if p.skip_template_strings => return,
            TermValue::Var(x) => {
                self.add(x);
                return;
            }
            _ => {}
        }
        match v {
            TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) => {
                r.iter().for_each(|t| self.term(t))
            }
            TermValue::Object(o) => {
                for (k, val) in sorted_pairs(o) {
                    self.term(k);
                    self.term(val);
                }
            }
            TermValue::Set(s) => sorted_items(s).into_iter().for_each(|t| self.term(t)),
            TermValue::ArrayCompr(t, b) | TermValue::SetCompr(t, b) => {
                self.term(t);
                self.body(b);
            }
            TermValue::ObjectCompr(k, val, b) => {
                self.term(k);
                self.term(val);
                self.body(b);
            }
            TermValue::TemplateString { parts, .. } => {
                for part in parts {
                    match part {
                        TemplatePart::Term(t) => self.term(t),
                        TemplatePart::Expr(e) => self.expr(e),
                    }
                }
            }
            _ => {}
        }
    }

    pub fn body(&mut self, b: &[Expr]) {
        b.iter().for_each(|e| self.expr(e));
    }

    pub fn expr(&mut self, e: &Expr) {
        let p = self.params;
        if p.skip_closures
            && let ExprTerms::Every(ev) = &e.terms
        {
            self.term(&ev.domain);
            return;
        }
        if p.skip_ref_call_head
            && let ExprTerms::Call(terms) = &e.terms
        {
            if let Some(op) = terms.first().and_then(Term::as_ref) {
                for t in op.iter().skip(1) {
                    self.term(t);
                }
            }
            for t in terms.iter().skip(1) {
                self.term(t);
            }
            for w in &e.with {
                self.with(w);
            }
            return;
        }
        match &e.terms {
            ExprTerms::Term(t) => self.term(t),
            ExprTerms::Some(d) => d.symbols.iter().for_each(|t| self.term(t)),
            ExprTerms::Every(ev) => {
                if let Some(k) = &ev.key {
                    self.term(k);
                }
                self.term(&ev.value);
                self.term(&ev.domain);
                self.body(&ev.body);
            }
            ExprTerms::Call(c) => c.iter().for_each(|t| self.term(t)),
        }
        for w in &e.with {
            self.with(w);
        }
    }

    pub fn with(&mut self, w: &With) {
        let p = self.params;
        if p.skip_with_target {
            self.term(&w.value);
            return;
        }
        if p.skip_ref_call_head {
            if let Some(r) = w.target.as_ref() {
                for t in r.iter().skip(1) {
                    self.term(t);
                }
            }
            if let Some(r) = w.value.as_ref() {
                for t in r.iter().skip(1) {
                    self.term(t);
                }
            } else {
                self.term(&w.value);
            }
            return;
        }
        self.term(&w.target);
        self.term(&w.value);
    }

    pub fn args(&mut self, args: &[Term]) {
        args.iter().for_each(|t| self.term(t));
    }

    pub fn reference(&mut self, r: &[Term]) {
        let skip = usize::from(self.params.skip_ref_head);
        for t in r.iter().skip(skip) {
            self.term(t);
        }
    }

    pub fn head(&mut self, h: &Head) {
        if !h.reference.is_empty() {
            self.reference(&h.reference);
        } else {
            if let Some(n) = &h.name {
                self.add(n);
            }
            if let Some(k) = &h.key {
                self.term(k);
            }
        }
        self.args(&h.args);
        if let Some(v) = &h.value {
            self.term(v);
        }
    }

    pub fn rule(&mut self, r: &Rule) {
        self.head(&r.head);
        self.body(&r.body);
        if let Some(e) = &r.else_ {
            self.rule(e);
        }
    }
}

/// Term.Vars(): every variable of a term.
pub fn term_vars(t: &Term) -> VarSet {
    let mut v = VarVisitor::default();
    v.term(t);
    v.vars
}

/// Body.Vars(params).
pub fn body_vars(b: &Body, params: Params) -> VarSet {
    let mut v = VarVisitor::new(params);
    v.body(b);
    v.vars
}

/// Expr.Vars(params).
pub fn expr_vars(e: &Expr, params: Params) -> VarSet {
    let mut v = VarVisitor::new(params);
    v.expr(e);
    v.vars
}

/// Var.IsGenerated.
pub fn is_generated(v: &str) -> bool {
    v.starts_with("__local")
}

/// Var.IsWildcard.
pub fn is_wildcard(v: &str) -> bool {
    v.starts_with('$')
}

//! OPA's Transform (ast/transform.go): a pre-order walk that may replace a node before
//! descending into it, in OPA's child order (heads before bodies, object and set members
//! sorted), and the read-only walks over expressions.

use super::vars::{sorted_items, sorted_pairs};
use crate::ast::{Body, Expr, ExprTerms, Rule, TemplatePart, Term, TermValue};

/// What a transform does at bodies and terms, before their children.
pub trait Transformer {
    fn body(&mut self, _b: &mut Body) {}
    fn term(&mut self, _t: &mut Term) {}
}

pub fn rule<T: Transformer + ?Sized>(t: &mut T, r: &mut Rule) {
    for x in r.head.reference.iter_mut() {
        term(t, x);
    }
    for x in r.head.args.iter_mut() {
        term(t, x);
    }
    if let Some(k) = r.head.key.as_mut() {
        term(t, k);
    }
    if let Some(v) = r.head.value.as_mut() {
        term(t, v);
    }
    body(t, &mut r.body);
    if let Some(e) = r.else_.as_mut() {
        rule(t, e);
    }
}

pub fn body<T: Transformer + ?Sized>(t: &mut T, b: &mut Body) {
    t.body(b);
    for e in b.iter_mut() {
        expr(t, e);
    }
}

pub fn expr<T: Transformer + ?Sized>(t: &mut T, e: &mut Expr) {
    match &mut e.terms {
        ExprTerms::Some(d) => d.symbols.iter_mut().for_each(|x| term(t, x)),
        ExprTerms::Call(c) => c.iter_mut().for_each(|x| term(t, x)),
        ExprTerms::Term(x) => term(t, x),
        ExprTerms::Every(ev) => {
            if let Some(k) = ev.key.as_mut() {
                term(t, k);
            }
            term(t, &mut ev.value);
            term(t, &mut ev.domain);
            body(t, &mut ev.body);
        }
    }
    for w in e.with.iter_mut() {
        term(t, &mut w.target);
        term(t, &mut w.value);
    }
}

pub fn term<T: Transformer + ?Sized>(t: &mut T, x: &mut Term) {
    t.term(x);
    match &mut x.value {
        TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) => r.iter_mut().for_each(|y| term(t, y)),
        TermValue::Object(o) => {
            let mut pairs: Vec<(Term, Term)> = sorted_pairs(o).into_iter().cloned().collect();
            for (k, v) in pairs.iter_mut() {
                term(t, k);
                term(t, v);
            }
            *o = pairs;
        }
        TermValue::Set(s) => {
            let mut items: Vec<Term> = sorted_items(s).into_iter().cloned().collect();
            items.iter_mut().for_each(|y| term(t, y));
            *s = items;
        }
        TermValue::ArrayCompr(y, b) | TermValue::SetCompr(y, b) => {
            term(t, y);
            body(t, b);
        }
        TermValue::ObjectCompr(k, v, b) => {
            term(t, k);
            term(t, v);
            body(t, b);
        }
        TermValue::TemplateString { parts, .. } => {
            for p in parts.iter_mut() {
                if let TemplatePart::Expr(e) = p {
                    expr(t, e);
                }
            }
        }
        _ => {}
    }
}

struct Terms<'a>(&'a mut dyn FnMut(&mut Term));

impl Transformer for Terms<'_> {
    fn term(&mut self, t: &mut Term) {
        (self.0)(t);
    }
}

/// Every term under a term, closures included, each before its children.
pub fn terms_mut(t: &mut Term, f: &mut dyn FnMut(&mut Term)) {
    term(&mut Terms(f), t);
}

/// Every expression under an expression (itself first), closures included.
pub fn exprs(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    match &e.terms {
        ExprTerms::Term(t) => exprs_in_term(t, f),
        ExprTerms::Call(c) => c.iter().for_each(|t| exprs_in_term(t, f)),
        ExprTerms::Some(d) => d.symbols.iter().for_each(|t| exprs_in_term(t, f)),
        ExprTerms::Every(ev) => {
            if let Some(k) = &ev.key {
                exprs_in_term(k, f);
            }
            exprs_in_term(&ev.value, f);
            exprs_in_term(&ev.domain, f);
            ev.body.iter().for_each(|x| exprs(x, f));
        }
    }
    for w in &e.with {
        exprs_in_term(&w.target, f);
        exprs_in_term(&w.value, f);
    }
}

/// Every expression in the closures under a term.
pub fn exprs_in_term(t: &Term, f: &mut dyn FnMut(&Expr)) {
    match &t.value {
        TermValue::Ref(r) | TermValue::Array(r) | TermValue::Call(r) | TermValue::Set(r) => {
            r.iter().for_each(|x| exprs_in_term(x, f))
        }
        TermValue::Object(o) => o.iter().for_each(|(k, v)| {
            exprs_in_term(k, f);
            exprs_in_term(v, f);
        }),
        TermValue::ArrayCompr(x, b) | TermValue::SetCompr(x, b) => {
            exprs_in_term(x, f);
            b.iter().for_each(|e| exprs(e, f));
        }
        TermValue::ObjectCompr(k, v, b) => {
            exprs_in_term(k, f);
            exprs_in_term(v, f);
            b.iter().for_each(|e| exprs(e, f));
        }
        TermValue::TemplateString { parts, .. } => {
            for p in parts {
                if let TemplatePart::Expr(e) = p {
                    exprs(e, f);
                }
            }
        }
        _ => {}
    }
}

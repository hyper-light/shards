//! OPA's `Compare` (ast/compare.go) over syntax terms: kinds in `sortOrder`, numbers by
//! `NumberCompare`, collections element by element, objects and sets by sorted members.

use std::cmp::Ordering;

use crate::ast::{Body, Expr, ExprTerms, Head, Rule, Shared, Term, TermValue, With};
use crate::value::number_compare;

/// ast.sortOrder.
fn sort_order(v: &TermValue) -> u8 {
    match v {
        TermValue::Null => 0,
        TermValue::Bool(_) => 1,
        TermValue::Number(_) => 2,
        TermValue::String(_) => 3,
        TermValue::TemplateString { .. } => 4,
        TermValue::Var(_) => 5,
        TermValue::Ref(_) => 6,
        TermValue::Array(_) => 7,
        TermValue::Object(_) => 8,
        TermValue::Set(_) => 9,
        TermValue::ArrayCompr(..) => 10,
        TermValue::ObjectCompr(..) => 11,
        TermValue::SetCompr(..) => 12,
        TermValue::Call(_) => 13,
    }
}

fn slice_compare(a: &[Term], b: &[Term]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = term_compare(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

fn sorted(items: &[Term]) -> Vec<Term> {
    let mut v = items.to_vec();
    v.sort_by(term_compare);
    v
}

/// Compare: a pair of members at a time (a term made of a value nests deeper than a
/// thread has stack for a frame a level), collections that are one and the same at once.
pub fn term_compare(a: &Term, b: &Term) -> Ordering {
    enum Step {
        Pair(Term, Term),
        Len(usize, usize),
    }
    // Scalars, the most compared, without the walk's stack.
    match (&a.value, &b.value) {
        (TermValue::Bool(x), TermValue::Bool(y)) => return x.cmp(y),
        (TermValue::Number(x), TermValue::Number(y)) => return number_compare(x.text(), y.text()),
        (TermValue::String(x), TermValue::String(y)) | (TermValue::Var(x), TermValue::Var(y)) => {
            return x.as_bytes().cmp(y.as_bytes());
        }
        _ => {}
    }
    let mut todo = vec![Step::Pair(a.clone(), b.clone())];
    while let Some(step) = todo.pop() {
        let (a, b) = match step {
            Step::Pair(a, b) => (a, b),
            Step::Len(x, y) => match x.cmp(&y) {
                Ordering::Equal => continue,
                o => return o,
            },
        };
        let (oa, ob) = (sort_order(&a.value), sort_order(&b.value));
        if oa != ob {
            return oa.cmp(&ob);
        }
        let mut pairs = |xs: Vec<Term>, ys: Vec<Term>| {
            todo.push(Step::Len(xs.len(), ys.len()));
            todo.extend(xs.into_iter().zip(ys).rev().map(|(p, q)| Step::Pair(p, q)));
        };
        let o = match (&a.value, &b.value) {
            (TermValue::Null, TermValue::Null) => Ordering::Equal,
            (TermValue::Bool(x), TermValue::Bool(y)) => x.cmp(y),
            (TermValue::Number(x), TermValue::Number(y)) => number_compare(x.text(), y.text()),
            (TermValue::String(x), TermValue::String(y)) | (TermValue::Var(x), TermValue::Var(y)) => {
                x.as_bytes().cmp(y.as_bytes())
            }
            (TermValue::Ref(x), TermValue::Ref(y))
            | (TermValue::Array(x), TermValue::Array(y))
            | (TermValue::Call(x), TermValue::Call(y)) => {
                if !Shared::ptr_eq(x, y) {
                    pairs(x.to_vec(), y.to_vec());
                }
                continue;
            }
            (TermValue::Set(x), TermValue::Set(y)) => {
                if !Shared::ptr_eq(x, y) {
                    pairs(sorted(x), sorted(y));
                }
                continue;
            }
            (TermValue::Object(x), TermValue::Object(y)) => {
                if !Shared::ptr_eq(x, y) {
                    let by_key = |o: &[(Term, Term)]| {
                        let mut kv = o.to_vec();
                        kv.sort_by(|p, q| term_compare(&p.0, &q.0));
                        kv.into_iter().flat_map(|(k, v)| [k, v]).collect::<Vec<Term>>()
                    };
                    let (xs, ys) = (by_key(x), by_key(y));
                    // Members of the shorter object decide before the lengths do.
                    let lens = (x.len(), y.len());
                    todo.push(Step::Len(lens.0, lens.1));
                    todo.extend(xs.into_iter().zip(ys).rev().map(|(p, q)| Step::Pair(p, q)));
                }
                continue;
            }
            (TermValue::ArrayCompr(x, xb), TermValue::ArrayCompr(y, yb))
            | (TermValue::SetCompr(x, xb), TermValue::SetCompr(y, yb)) => {
                term_compare(x, y).then_with(|| body_compare(xb, yb))
            }
            (TermValue::ObjectCompr(xk, xv, xb), TermValue::ObjectCompr(yk, yv, yb)) => term_compare(xk, yk)
                .then_with(|| term_compare(xv, yv))
                .then_with(|| body_compare(xb, yb)),
            // Template strings: OPA compares their parts; their text orders them the same way.
            _ => a.to_string().cmp(&b.to_string()),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Compare over optional terms: nil first.
fn opt_compare(a: Option<&Term>, b: Option<&Term>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => term_compare(x, y),
    }
}

/// Expr.sortOrder.
fn expr_order(e: &Expr) -> u8 {
    match e.terms {
        ExprTerms::Some(_) => 0,
        ExprTerms::Term(_) => 1,
        ExprTerms::Call(_) => 2,
        ExprTerms::Every(_) => 3,
    }
}

/// Expr.Compare.
pub fn expr_compare(a: &Expr, b: &Expr) -> Ordering {
    let c = expr_order(a)
        .cmp(&expr_order(b))
        .then(a.index.cmp(&b.index))
        .then(a.negated.cmp(&b.negated));
    if c != Ordering::Equal {
        return c;
    }
    let c = match (&a.terms, &b.terms) {
        (ExprTerms::Term(x), ExprTerms::Term(y)) => term_compare(x, y),
        (ExprTerms::Call(x), ExprTerms::Call(y)) => slice_compare(x, y),
        (ExprTerms::Some(x), ExprTerms::Some(y)) => slice_compare(&x.symbols, &y.symbols),
        (ExprTerms::Every(x), ExprTerms::Every(y)) => opt_compare(x.key.as_ref(), y.key.as_ref())
            .then_with(|| term_compare(&x.value, &y.value))
            .then_with(|| term_compare(&x.domain, &y.domain))
            .then_with(|| body_compare(&x.body, &y.body)),
        _ => Ordering::Equal,
    };
    c.then_with(|| with_slice_compare(&a.with, &b.with))
}

fn with_slice_compare(a: &[With], b: &[With]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = term_compare(&x.target, &y.target).then_with(|| term_compare(&x.value, &y.value));
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// Body.Compare.
pub fn body_compare(a: &Body, b: &Body) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = expr_compare(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    a.len().cmp(&b.len())
}

/// Head.Compare.
fn head_compare(a: &Head, b: &Head) -> Ordering {
    b.assign
        .cmp(&a.assign)
        .then_with(|| slice_compare(&a.args, &b.args))
        .then_with(|| slice_compare(&a.reference, &b.reference))
        .then_with(|| {
            a.name
                .as_deref()
                .unwrap_or_default()
                .as_bytes()
                .cmp(b.name.as_deref().unwrap_or_default().as_bytes())
        })
        .then_with(|| opt_compare(a.key.as_ref(), b.key.as_ref()))
        .then_with(|| opt_compare(a.value.as_ref(), b.value.as_ref()))
}

/// Rule.Compare: branch by branch down both else chains (OPA recurses down them).
pub fn rule_compare(a: &Rule, b: &Rule) -> Ordering {
    let (mut a, mut b) = (a, b);
    loop {
        let o = head_compare(&a.head, &b.head)
            .then(a.default.cmp(&b.default))
            .then_with(|| body_compare(&a.body, &b.body));
        if o != Ordering::Equal {
            return o;
        }
        match (&a.else_, &b.else_) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => (a, b) = (x, y),
        }
    }
}

//! OPA's `Compare` (ast/compare.go) over syntax terms: kinds in `sortOrder`, numbers by
//! `NumberCompare`, collections element by element, objects and sets by sorted members.

use std::cmp::Ordering;

use crate::ast::{Term, TermValue};
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

pub fn term_compare(a: &Term, b: &Term) -> Ordering {
    let (oa, ob) = (sort_order(&a.value), sort_order(&b.value));
    if oa != ob {
        return oa.cmp(&ob);
    }
    match (&a.value, &b.value) {
        (TermValue::Null, TermValue::Null) => Ordering::Equal,
        (TermValue::Bool(x), TermValue::Bool(y)) => x.cmp(y),
        (TermValue::Number(x), TermValue::Number(y)) => number_compare(x.text(), y.text()),
        (TermValue::String(x), TermValue::String(y)) | (TermValue::Var(x), TermValue::Var(y)) => {
            x.as_bytes().cmp(y.as_bytes())
        }
        (TermValue::Ref(x), TermValue::Ref(y))
        | (TermValue::Array(x), TermValue::Array(y))
        | (TermValue::Call(x), TermValue::Call(y)) => slice_compare(x, y),
        (TermValue::Set(x), TermValue::Set(y)) => slice_compare(&sorted(x), &sorted(y)),
        (TermValue::Object(x), TermValue::Object(y)) => {
            let mut xs: Vec<&(Term, Term)> = x.iter().collect();
            let mut ys: Vec<&(Term, Term)> = y.iter().collect();
            xs.sort_by(|p, q| term_compare(&p.0, &q.0));
            ys.sort_by(|p, q| term_compare(&p.0, &q.0));
            for (p, q) in xs.iter().zip(&ys) {
                let c = term_compare(&p.0, &q.0);
                if c != Ordering::Equal {
                    return c;
                }
                let c = term_compare(&p.1, &q.1);
                if c != Ordering::Equal {
                    return c;
                }
            }
            xs.len().cmp(&ys.len())
        }
        // Comprehensions and template strings: OPA compares their terms and bodies;
        // their text orders them the same way for equal kinds and is total.
        _ => a.to_string().cmp(&b.to_string()),
    }
}
